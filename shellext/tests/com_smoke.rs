//! Smoke test for the aahl_shellext DLL.
//!
//! Loads the built DLL and exercises the four exports plus a COM
//! instantiation round-trip against the real `windows` types.
//!
//! The test is skipped (not failed) when the DLL is not present, and avoids
//! touching an already-registered CLSID so a machine-local install is never
//! disturbed. The registration test cleans up after itself.

#![cfg(windows)]

use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr;

use windows::core::{s, GUID, HRESULT, Interface, IUnknown, PCWSTR};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, RegCloseKey, RegOpenKeyExW,
};
use windows::Win32::System::Com::{IClassFactory, IDataObject};
use windows::Win32::UI::Shell::{CMINVOKECOMMANDINFO, GCS_VERBA, IContextMenu, IShellExtInit};
use windows::Win32::UI::WindowsAndMessaging::{CreatePopupMenu, DestroyMenu, GetMenuItemCount};

use aahl_shellext::reg;
use aahl_shellext::tool;

type DllGetClassObjectFn = unsafe extern "system" fn(
    *const GUID,
    *const GUID,
    *mut *mut c_void,
) -> i32;
type DllCanUnloadNowFn = unsafe extern "system" fn() -> i32;
type DllRegisterServerFn = unsafe extern "system" fn() -> i32;
type DllUnregisterServerFn = unsafe extern "system" fn() -> i32;

/// Locate the built DLL: an explicit override or the workspace target dir.
fn dll_path() -> Option<PathBuf> {
    if let Ok(custom) = std::env::var("AAHL_SHELLEXT_DLL") {
        let path = PathBuf::from(custom);
        if path.is_file() {
            return Some(path);
        }
    }
    let manifest = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let base = PathBuf::from(manifest).join("../target");
    for profile in ["release", "debug"] {
        let candidate = base.join(profile).join("aahl_shellext.dll");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Load the DLL and resolve the four exports.
unsafe fn load_dll() -> (
    HMODULE,
    DllGetClassObjectFn,
    DllCanUnloadNowFn,
    DllRegisterServerFn,
    DllUnregisterServerFn,
) {
    let path = dll_path().expect("aahl_shellext.dll must be available to run this test");
    let path_wide = tool::wide(&path.to_string_lossy());
    let module = LoadLibraryW(PCWSTR::from_raw(path_wide.as_ptr()))
        .expect("LoadLibraryW(aahl_shellext.dll)");

    unsafe fn export<T: Copy>(module: HMODULE, name: windows::core::PCSTR) -> T {
        let proc = GetProcAddress(module, name);
        assert!(!proc.is_none(), "missing export {:?}", name);
        std::mem::transmute_copy(&proc)
    }
    let get = unsafe { export::<DllGetClassObjectFn>(module, s!("DllGetClassObject")) };
    let can_unload = unsafe { export::<DllCanUnloadNowFn>(module, s!("DllCanUnloadNow")) };
    let register_server = unsafe { export::<DllRegisterServerFn>(module, s!("DllRegisterServer")) };
    let unregister_server = unsafe { export::<DllUnregisterServerFn>(module, s!("DllUnregisterServer")) };
    (
        module, get, can_unload, register_server, unregister_server,
    )
}

#[test]
fn dll_loads_and_can_unload() {
    let (module, _get, can_unload, _register, _unregister) = unsafe { load_dll() };
    let result = unsafe { can_unload() };
    assert_eq!(result, 1, "DllCanUnloadNow should return S_FALSE, got {result}");
    unsafe {
        let _ = FreeLibrary(module);
    }
}

#[test]
fn class_object_and_instantiation() {
    let (module, get, _can_unload, _register, _unregister) = unsafe { load_dll() };
    unsafe {
        // Unknown CLSID → E_NOINTERFACE.
        let mut obj: *mut c_void = ptr::null_mut();
        let unknown = GUID::zeroed();
        let result = get(&unknown, &IClassFactory::IID, &mut obj);
        assert_eq!(result, 0x8000_4002u32 as i32, "bad CLSID: got 0x{result:08X}");
        assert!(obj.is_null());

        // Unknown riid for our CLSID → E_NOINTERFACE.
        let result = get(&aahl_shellext::CLSID, &GUID::zeroed(), &mut obj);
        assert_eq!(result, 0x8000_4002u32 as i32, "bad riid: got 0x{result:08X}");

        // Null ppv → E_POINTER.
        let result = get(&aahl_shellext::CLSID, &IClassFactory::IID, ptr::null_mut());
        assert_eq!(result, 0x8000_4003u32 as i32, "null ppv: got 0x{result:08X}");

        // Real class object.
        let mut factory_raw: *mut c_void = ptr::null_mut();
        let result = get(&aahl_shellext::CLSID, &IClassFactory::IID, &mut factory_raw);
        assert_eq!(result, 0, "DllGetClassObject: got 0x{result:08X}");
        assert!(!factory_raw.is_null());
        let factory = IClassFactory::from_raw(factory_raw);

        // IClassFactory reachable through IUnknown QI.
        let as_unknown = factory.cast::<IUnknown>().expect("QI for IUnknown");
        assert!(!as_unknown.as_raw().is_null());

        // Wrapper helper: same path, implicit QI handled by the helper.
        let via_wrapper = factory
            .CreateInstance::<_, IContextMenu>(None)
            .expect("CreateInstance via helper should succeed");
        assert!(!via_wrapper.as_raw().is_null());

        // CreateInstance with IContextMenu riid (raw vtable call so the
        // returned pointer is used exactly as produced, no implicit QI).
        let mut ext_raw: *mut c_void = ptr::null_mut();
        let hr = (factory.vtable().CreateInstance)(
            factory.as_raw(),
            ptr::null_mut(),
            &<IContextMenu as Interface>::IID,
            &mut ext_raw,
        );
        assert!(hr.is_ok(), "CreateInstance(IContextMenu): got 0x{:08X}", hr.0);
        assert!(!ext_raw.is_null());
        let context = IContextMenu::from_raw(ext_raw);

        // Cross-QI between the implemented interfaces.
        let shell = context.cast::<IShellExtInit>().expect("QI for IShellExtInit");
        let _ = shell.cast::<IUnknown>().expect("QI for IUnknown");

        // Aggregation must be refused.
        let mut agg_raw: *mut c_void = ptr::null_mut();
        let hr = (factory.vtable().CreateInstance)(
            factory.as_raw(),
            as_unknown.as_raw(),
            &<IUnknown as Interface>::IID,
            &mut agg_raw,
        );
        assert!(hr.is_err(), "aggregation should be refused, got 0x{:08X}", hr.0);

        // Without a drop selection the menu is empty.
        let hmenu = CreatePopupMenu().expect("CreatePopupMenu");
        let query = context.QueryContextMenu(hmenu, 0, 100, 200, 0);
        assert_eq!(query, HRESULT(0), "no-selection menu should return S_OK");
        assert_eq!(GetMenuItemCount(Some(hmenu)), 0);
        let _ = DestroyMenu(hmenu);

        // Initialize with a null data object is a no-op success.
        let initialized = shell.Initialize(None, None::<&IDataObject>, None);
        assert!(initialized.is_ok(), "Initialize(null) should succeed");

        // InvokeCommand with an empty selection is a no-op success.
        let info = CMINVOKECOMMANDINFO {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
            fMask: 0,
            hwnd: windows::Win32::Foundation::HWND(ptr::null_mut()),
            lpVerb: windows::core::PCSTR::null(),
            lpParameters: windows::core::PCSTR::null(),
            lpDirectory: windows::core::PCSTR::null(),
            nShow: 0,
            dwHotKey: 0,
            hIcon: windows::Win32::Foundation::HANDLE(ptr::null_mut()),
        };
        let invoked = context.InvokeCommand(&info);
        assert!(invoked.is_ok(), "InvokeCommand(no selection) should succeed");

        // GetCommandString with no verb is E_INVALIDARG (and must not crash).
        let mut name_buffer = [0u8; 64];
        let named = context.GetCommandString(
            0,
            GCS_VERBA,
            None,
            windows::core::PSTR(name_buffer.as_mut_ptr()),
            64,
        );
        assert!(named.is_err(), "GetCommandString with no verbs should fail");

        // Drop all interface values, then unload the DLL.
        drop(shell);
        drop(context);
        drop(as_unknown);
        drop(factory);
        drop(via_wrapper);
        let _ = FreeLibrary(module);
    }
}

/// `true` when `path` exists under `root`.
fn key_present(root: HKEY, path: &str) -> bool {
    let wide = tool::wide(path);
    let mut key: HKEY = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        RegOpenKeyExW(
            root,
            PCWSTR::from_raw(wide.as_ptr()),
            None,
            KEY_READ,
            &mut key,
        )
    };
    if rc.0 == 0 {
        unsafe {
            let _ = RegCloseKey(key);
        }
        true
    } else {
        false
    }
}

#[test]
fn register_and_unregister_roundtrip() {
    let Some(dll) = dll_path() else {
        eprintln!("skipping: aahl_shellext.dll not found");
        return;
    };
    let clsid_key = format!("Software\\Classes\\CLSID\\{}", reg::CLSID);
    if key_present(HKEY_CURRENT_USER, &clsid_key) {
        eprintln!("skipping: CLSID {} already registered", reg::CLSID);
        return;
    }

    let _ = reg::unregister(); // clear any partial residue from a prior run
    reg::register(&dll).expect("register should succeed");

    assert!(key_present(HKEY_CURRENT_USER, &clsid_key));
    assert!(key_present(
        HKEY_CURRENT_USER,
        &format!("{clsid_key}\\InprocServer32")
    ));
    assert!(key_present(
        HKEY_CURRENT_USER,
        "Software\\Classes\\*\\shellex\\ContextMenuHandlers\\AAHL"
    ));
    assert!(key_present(
        HKEY_CURRENT_USER,
        "Software\\Classes\\Directory\\shellex\\ContextMenuHandlers\\AAHL"
    ));
    assert!(key_present(HKEY_CURRENT_USER, reg::AAHL_KEY));

    reg::unregister().expect("unregister should succeed");
    assert!(!key_present(HKEY_CURRENT_USER, &clsid_key));
    assert!(!key_present(HKEY_CURRENT_USER, reg::AAHL_KEY));
}