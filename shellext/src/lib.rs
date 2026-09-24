//! AAHL Windows shell extension.
//!
//! Exposes a COM class (CLSID {FF2729B8-37AC-4416-B770-EF81D8E5F168}) that
//! adds "Add to AAHL archive", extract and test commands to the Explorer
//! context menu. Registration is per-user (HKCU) and never requires
//! elevation.
//!
//! The crate is windows-only; on other platforms it compiles to an empty
//! library so the workspace keeps building on Linux.

#![cfg(windows)]

mod com;
pub mod menu;
pub mod reg;
pub mod tool;

use std::ffi::c_void;

use windows::core::{GUID, HRESULT, IUnknown, Interface};
use windows::Win32::Foundation::{E_NOINTERFACE, E_POINTER, S_FALSE, S_OK};
use windows::Win32::System::Com::IClassFactory;

/// CLSID for the AAHL shell extension.
pub const CLSID: GUID = GUID::from_values(
    0xFF2729B8,
    0x37AC,
    0x4416,
    [0xB7, 0x70, 0xEF, 0x81, 0xD8, 0xE5, 0xF1, 0x68],
);

/// Resolve a class object. The returned interface owns a reference the
/// caller must Release.
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if rclsid.is_null() || riid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    unsafe { *ppv = std::ptr::null_mut() };
    let rclsid = unsafe { &*rclsid };
    let riid = unsafe { &*riid };

    if *rclsid != CLSID {
        return E_NOINTERFACE;
    }
    // Return the interface pointer matching the requested riid. For
    // windows-implement objects the IUnknown and IClassFactory interface
    // pointers are distinct addresses, so handing out the wrong one would
    // make callers read the identity vtable as an IClassFactory vtable.
    if *riid == IClassFactory::IID {
        let factory: IClassFactory = com::AahlClassFactory.into();
        unsafe { *ppv = factory.into_raw() };
        S_OK
    } else if *riid == IUnknown::IID {
        let factory: IUnknown = com::AahlClassFactory.into();
        unsafe { *ppv = factory.into_raw() };
        S_OK
    } else {
        E_NOINTERFACE
    }
}

/// The extension keeps no cross-instance locks, so the DLL could unload;
/// S_FALSE however keeps it resident while any instance is outstanding,
/// which is what COM expects here.
#[no_mangle]
pub unsafe extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}

/// Register the extension in the current user's registry.
#[no_mangle]
pub unsafe extern "system" fn DllRegisterServer() -> HRESULT {
    match tool::dll_dir() {
        Some(dir) => match reg::register(&dir.join("aahl_shellext.dll")) {
            Ok(()) => S_OK,
            Err(code) => HRESULT(reg::hresult_from_win32(code)),
        },
        None => HRESULT(reg::hresult_from_win32(2)), // ERROR_FILE_NOT_FOUND
    }
}

/// Remove the extension from the current user's registry.
#[no_mangle]
pub unsafe extern "system" fn DllUnregisterServer() -> HRESULT {
    match reg::unregister() {
        Ok(()) => S_OK,
        Err(code) => HRESULT(reg::hresult_from_win32(code)),
    }
}