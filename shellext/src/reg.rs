//! Per-user registry registration for the AAHL shell extension.
//!
//! Everything lives under `HKCU\Software\Classes` (and `HKCU\Software\AAHL`),
//! so both registration and unregistration work without elevation and never
//! require a reboot or an Explorer restart to take effect.

use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
};

/// CLSID of the AAHL context menu extension.
pub const CLSID: &str = "{FF2729B8-37AC-4416-B770-EF81D8E5F168}";

/// HKCU key recording the install directory.
pub const AAHL_KEY: &str = "Software\\AAHL";

/// Win32 error → COM HRESULT mapping (HRESULT_FROM_WIN32).
pub fn hresult_from_win32(code: u32) -> i32 {
    (0x8007_0000u32 | (code & 0xFFFF)) as i32
}

/// Write a REG_SZ value under `subkey` of `parent` (creating the key if
/// needed). `name == None` addresses the key's (default) value.
fn set_string(parent: HKEY, subkey: &str, name: Option<&str>, value: &str) -> Result<(), u32> {
    let subkey = crate::tool::wide(subkey);
    let name_wide = name.map(crate::tool::wide);
    let value_data = crate::tool::wide_bytes(value);

    let mut key: HKEY = unsafe { std::mem::zeroed() };
    let created = unsafe {
        RegCreateKeyExW(
            parent,
            PCWSTR::from_raw(subkey.as_ptr()),
            None,           // reserved
            PCWSTR::null(), // class
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None, // security attributes
            &mut key,
            None, // disposition
        )
    };
    if created.0 != 0 {
        return Err(created.0 as u32);
    }

    let written = match &name_wide {
        Some(name) => unsafe {
            RegSetValueExW(
                key,
                PCWSTR::from_raw(name.as_ptr()),
                None,
                REG_SZ,
                Some(&value_data),
            )
        },
        None => unsafe {
            RegSetValueExW(key, PCWSTR::null(), None, REG_SZ, Some(&value_data))
        },
    };
    let _ = unsafe { RegCloseKey(key) };
    if written.0 != 0 {
        Err(written.0 as u32)
    } else {
        Ok(())
    }
}

/// Delete `subkey` and everything below it. Missing keys (errors 2 and 3)
/// count as success so unregistration is idempotent.
fn delete_tree(parent: HKEY, subkey: &str) -> Result<(), u32> {
    let subkey = crate::tool::wide(subkey);
    let rc = unsafe { RegDeleteTreeW(parent, PCWSTR::from_raw(subkey.as_ptr())) };
    match rc.0 as u32 {
        0 | 2 | 3 => Ok(()),
        other => Err(other),
    }
}

/// Register the extension for the current user. `dll_path` is the full path
/// to `aahl_shellext.dll`.
pub fn register(dll_path: &Path) -> Result<(), u32> {
    let clsid = format!("Software\\Classes\\CLSID\\{CLSID}");
    set_string(
        HKEY_CURRENT_USER,
        &clsid,
        None,
        "AAHL Context Menu Shell Extension",
    )?;

    let inproc = format!("{clsid}\\InprocServer32");
    set_string(HKEY_CURRENT_USER, &inproc, None, &dll_path.to_string_lossy())?;
    set_string(HKEY_CURRENT_USER, &inproc, Some("ThreadingModel"), "Apartment")?;

    let file_handler = "Software\\Classes\\*\\shellex\\ContextMenuHandlers\\AAHL";
    set_string(HKEY_CURRENT_USER, file_handler, None, CLSID)?;

    let directory_handler = "Software\\Classes\\Directory\\shellex\\ContextMenuHandlers\\AAHL";
    set_string(HKEY_CURRENT_USER, directory_handler, None, CLSID)?;

    if let Some(dir) = dll_path.parent() {
        set_string(HKEY_CURRENT_USER, AAHL_KEY, None, &dir.to_string_lossy())?;
    }
    Ok(())
}

/// Remove the extension's HKCU entries. Symmetric with [`register`].
pub fn unregister() -> Result<(), u32> {
    let clsid = format!("Software\\Classes\\CLSID\\{CLSID}");
    delete_tree(HKEY_CURRENT_USER, &clsid)?;
    delete_tree(
        HKEY_CURRENT_USER,
        "Software\\Classes\\*\\shellex\\ContextMenuHandlers\\AAHL",
    )?;
    delete_tree(
        HKEY_CURRENT_USER,
        "Software\\Classes\\Directory\\shellex\\ContextMenuHandlers\\AAHL",
    )?;
    delete_tree(HKEY_CURRENT_USER, AAHL_KEY)?;
    Ok(())
}