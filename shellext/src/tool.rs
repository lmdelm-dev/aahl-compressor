//! Cross-cutting helpers: wide strings, module discovery, process spawning
//! and user-facing reporting for the AAHL shell extension.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// CREATE_NO_WINDOW: the launched process gets no console window.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// MB_ICONERROR (0x10) — message box error glyph.
pub const MB_ICONERROR: u32 = 0x0000_0010;
/// MB_ICONINFORMATION (0x40) — message box info glyph.
pub const MB_ICONINFORMATION: u32 = 0x0000_0040;

/// NUL-terminated UTF-16 encoding of `text`.
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// NUL-terminated UTF-16LE byte encoding of `text` (for REG_SZ values).
pub fn wide_bytes(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() * 2 + 2);
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out.extend_from_slice(&[0, 0]);
    out
}

/// Directory containing the module the address `ptr` lies in.
pub fn module_dir_from_address(ptr: usize) -> Option<PathBuf> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleExW};

    const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x0000_0004;
    const GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT: u32 = 0x0000_0002;

    unsafe {
        let mut module: HMODULE = std::mem::zeroed();
        if GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR::from_raw(ptr as *const u16),
            &mut module,
        )
        .is_err()
        {
            return None;
        }
        let mut buffer = vec![0u16; 4096];
        let written = GetModuleFileNameW(Some(module), &mut buffer);
        if written == 0 {
            return None;
        }
        let name = String::from_utf16(&buffer[..written as usize]).ok()?;
        Some(PathBuf::from(name))
    }
}

/// Directory containing this DLL (resolved once from the
/// [`crate::DllGetClassObject`] export address).
pub fn dll_dir() -> Option<PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        module_dir_from_address(crate::DllGetClassObject as *const () as usize)
            .and_then(|name| name.parent().map(|dir| dir.to_path_buf()))
    })
    .clone()
}

/// Directory previously recorded under `HKCU\Software\AAHL` by the installer
/// or by `DllRegisterServer`.
pub fn installed_dir() -> Option<PathBuf> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::WIN32_ERROR;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
    };

    unsafe {
        let mut key: HKEY = std::mem::zeroed();
        let subkey = wide("Software\\AAHL");
        let opened = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(subkey.as_ptr()),
            None, // ulOptions reserved
            KEY_READ,
            &mut key,
        );
        if opened != WIN32_ERROR(0) {
            return None;
        }

        let mut size: u32 = 0;
        let queried = RegQueryValueExW(key, PCWSTR::null(), None, None, None, Some(&mut size));
        if queried != WIN32_ERROR(0) {
            let _ = RegCloseKey(key);
            return None;
        }
        // Size is in bytes; round up to a whole number of UTF-16 units and
        // leave room for the terminator.
        let mut size = (size + 1) & !1;
        let mut buffer = vec![0u16; (size as usize / 2) + 1];
        let queried = RegQueryValueExW(
            key,
            PCWSTR::null(),
            None,
            None,
            Some(buffer.as_mut_ptr() as *mut u8),
            Some(&mut size),
        );
        let _ = RegCloseKey(key);
        if queried != WIN32_ERROR(0) {
            return None;
        }
        let text = String::from_utf16(&buffer[..size as usize / 2]).ok()?;
        let text = text.trim_end_matches('\0').trim();
        if text.is_empty() {
            None
        } else {
            Some(PathBuf::from(text))
        }
    }
}

/// Candidate locations of the AAHL executables.
#[derive(Debug, Clone, Default)]
pub struct Binaries {
    pub cli: Option<PathBuf>,
    pub gui: Option<PathBuf>,
}

/// Resolve the AAHL executables: `AAHL_BIN` env var, next to this DLL, then
/// the HKCU install directory.
pub fn discover() -> Binaries {
    let mut binaries = Binaries::default();
    if let Ok(base) = std::env::var("AAHL_BIN") {
        let base = PathBuf::from(base);
        binaries.cli = Some(base.join("aahl.exe"));
        binaries.gui = Some(base.join("aahl-gui.exe"));
    } else if let Some(dir) = dll_dir() {
        binaries.cli = Some(dir.join("aahl.exe"));
        binaries.gui = Some(dir.join("aahl-gui.exe"));
    } else if let Some(dir) = installed_dir() {
        binaries.cli = Some(dir.join("aahl.exe"));
        binaries.gui = Some(dir.join("aahl-gui.exe"));
    }
    binaries
}

/// Spawn the AAHL CLI hidden (no console) and hand back the child for the
/// caller to wait on.
fn spawn_impl(program: &Path, args: &[String], cwd: &Path) -> std::io::Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .creation_flags(CREATE_NO_WINDOW);
    command.spawn()
}

/// Launch `program` detached (no visible console, no wait).
pub fn spawn_detached(program: &Path, args: &[String], cwd: &Path) -> bool {
    spawn_impl(program, args, cwd).is_ok()
}

/// One-line description of a launch for error reports.
pub fn cli_report(program: &Path, args: &[String], cwd: &Path) -> String {
    let mut report = format!("Program: {}\n", program.display());
    report.push_str(&format!("Args:    {}\n", args.join(" ")));
    report.push_str(&format!("CWD:     {}", cwd.display()));
    report
}

/// Show a simple message box headed "AAHL". `icon` is one of
/// [`MB_ICONERROR`] or [`MB_ICONINFORMATION`]; the button is always MB_OK.
pub fn report_box(message: &str, icon: u32) {
    use windows::core::{w, PCWSTR};
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MESSAGEBOX_STYLE};

    let text = wide(message);
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR::from_raw(text.as_ptr()),
            PCWSTR::from_raw(w!("AAHL").as_ptr()),
            MESSAGEBOX_STYLE(icon),
        );
    }
}

/// Default archive path for the "Add" verb: `<parent of first file>\<stem>.aahl`,
/// or plain `<stem>.aahl` when the first file has no parent directory.
pub fn default_archive(files: &[String]) -> PathBuf {
    let first = files.first().map(|path| path.as_str()).unwrap_or("");
    let name = format!("{}.aahl", crate::menu::stem(first));
    let parent = Path::new(first)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    match parent {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

/// The archive a set of actions targets: the first selected `.aahl` file, or
/// the default archive computed by [`default_archive`].
pub fn archive_path(selection: &[String]) -> PathBuf {
    selection
        .iter()
        .find(|path| crate::menu::is_archive(path))
        .map(|path| PathBuf::from(path))
        .unwrap_or_else(|| default_archive(selection))
}

/// Target directory for extraction: the archive's parent folder, or that
/// folder joined with `stem` for "Extract to".
pub fn extract_target(archive: &Path, stem: Option<&str>) -> PathBuf {
    let parent = archive
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| parent.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    match stem {
        Some(stem) => parent.join(stem),
        None => parent,
    }
}

/// CLI arguments for the "Add" verb (aahl-gui --create on the selected files).
pub fn add_arguments(files: &[String]) -> Vec<String> {
    let mut args = vec!["--create".to_owned()];
    args.extend(files.iter().cloned());
    args
}

/// CLI arguments for extraction ("extract <archive> <dir>").
pub fn extract_arguments(archive: &Path, target: &Path) -> Vec<String> {
    vec![
        "extract".to_owned(),
        archive.to_string_lossy().into_owned(),
        target.to_string_lossy().into_owned(),
    ]
}

/// CLI arguments for testing ("test <archive>").
pub fn test_arguments(archive: &Path) -> Vec<String> {
    vec!["test".to_owned(), archive.to_string_lossy().into_owned()]
}

/// Run the selected CLI action in the background and report the outcome with
/// a message box. `description` is the human-readable action name.
pub fn run_action(program: Option<PathBuf>, args: Vec<String>, cwd: &Path, description: &str) {
    let Some(program) = program else {
        report_box(
            &format!(
                "AAHL tool not found.\n\nCannot {description}.\n\nInstall the AAHL binaries (aahl.exe, aahl-gui.exe) next to aahl_shellext.dll or set the AAHL_BIN environment variable."
            ),
            MB_ICONERROR,
        );
        return;
    };
    let cwd = cwd.to_path_buf();
    let description = description.to_owned();
    let _ = std::thread::Builder::new()
        .name("aahl-action".to_owned())
        .spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Option<i32> {
                let mut child = spawn_impl(&program, &args, &cwd).ok()?;
                child.wait().ok()?.code()
            }));
            match outcome {
                // 0: the action itself succeeded.
                Ok(Some(0)) => {
                    report_box(&format!("{description} completed successfully."), MB_ICONINFORMATION)
                }
                // 3: the GUI's save dialog was cancelled by the user.
                Ok(Some(3)) => {}
                _ => report_box(
                    &format!("{description} failed.\n\nCommand: {} {}", program.display(), args.join(" ")),
                    MB_ICONERROR,
                ),
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_encoding() {
        assert_eq!(wide("hi"), vec![b'h' as u16, b'i' as u16, 0]);
        assert_eq!(wide_bytes("hi"), vec![b'h', 0, b'i', 0, 0, 0]);
    }

    #[test]
    fn wide_bytes_roundtrip() {
        let hello = "héllo";
        let bytes = wide_bytes(hello);
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        let end = units.iter().position(|&unit| unit == 0).unwrap_or(units.len());
        assert_eq!(String::from_utf16(&units[..end]).unwrap(), hello);
    }

    #[test]
    fn default_archive_variants() {
        assert_eq!(
            default_archive(&["C:\\tmp\\b\\t.txt".to_owned()]),
            PathBuf::from("C:\\tmp\\b\\t.aahl")
        );
        assert_eq!(default_archive(&["t.txt".to_owned()]), PathBuf::from("t.aahl"));
        assert_eq!(default_archive(&["a".to_owned()]), PathBuf::from("a.aahl"));
        assert_eq!(default_archive(&[]), PathBuf::from("archive.aahl"));
    }

    #[test]
    fn archive_path_prefers_selected_archive() {
        let selection = vec!["c:\\x\\a.txt".to_owned(), "c:\\y\\b.aahl".to_owned()];
        assert_eq!(archive_path(&selection), PathBuf::from("c:\\y\\b.aahl"));
        assert_eq!(
            archive_path(&["c:\\x\\a.txt".to_owned()]),
            PathBuf::from("c:\\x\\a.aahl")
        );
    }

    #[test]
    fn extraction_targets() {
        let archive = PathBuf::from("c:\\x\\b.aahl");
        assert_eq!(extract_target(&archive, None), PathBuf::from("c:\\x"));
        assert_eq!(extract_target(&archive, Some("b")), PathBuf::from("c:\\x\\b"));
        let relative = PathBuf::from("b.aahl");
        assert_eq!(extract_target(&relative, Some("b")), PathBuf::from(".\\b"));
    }

    #[test]
    fn cli_argument_lists() {
        assert_eq!(
            add_arguments(&["a.txt".to_owned()]),
            vec!["--create".to_owned(), "a.txt".to_owned()]
        );
        assert_eq!(
            extract_arguments(&PathBuf::from("a.aahl"), &PathBuf::from("out")),
            vec!["extract".to_owned(), "a.aahl".to_owned(), "out".to_owned()]
        );
        assert_eq!(
            test_arguments(&PathBuf::from("a.aahl")),
            vec!["test".to_owned(), "a.aahl".to_owned()]
        );
    }
}