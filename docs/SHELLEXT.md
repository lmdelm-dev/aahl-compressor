# AAHL Windows Shell Extension

`shellext` is a native Windows Explorer context-menu extension (a COM Inproc
server, `aahl_shellext.dll`) that puts AAHL actions one right-click away.
Everything is per-user (`HKCU`), so there is no elevation, no admin prompt,
and no Explorer restart or reboot.

## Menu commands

| Verb             | Label                    | Selection          | Action |
|------------------|--------------------------|-------------------|--------|
| `aahladd`        | Add to AAHL archive      | any files         | `aahl-gui --create <files...>` (save dialog for the archive) |
| `aahlextracthere`| Extract here             | one `.aahl` archive | extract into the archive's folder |
| `aahlextractto`  | Extract to *\<stem\>*... | one `.aahl` archive | extract into `<folder>\<stem>\` |
| `aahltest`       | Test archive with AAHL   | one `.aahl` archive | integrity check, reports OK/errors |

Up to 4 files are offered at once (a larger batch is read but only the first
four are handled, to keep the menu fast). If an `.aahl` file is among the
selection, the extract/test verbs appear next to it. Progress is invisible
(CREATE_NO_WINDOW); completion or failure is reported with a message box.

## Install / uninstall

From the extracted release directory (contains `aahl.exe`, `aahl-gui.exe`,
`aahl_shellext.dll` and both `.ps1` scripts):

```powershell
powershell -ExecutionPolicy Bypass -File .\install-context-menu.ps1
powershell -ExecutionPolicy Bypass -File .\uninstall-context-menu.ps1
```

`install-context-menu.ps1` registers the CLSID under `HKCU\Software\Classes`,
points `InprocServer32` at the DLL and records the bin directory in
`HKCU\Software\AAHL`. It defaults `-BinDir` to the script's own folder; pass
`-BinDir <path>` to register a different layout. Uninstall removes exactly
those keys, does not touch the binaries, and is idempotent.

## What gets registered (HKCU)

```
Software\Classes\CLSID\{FF2729B8-37AC-4416-B770-EF81D8E5F168}
    (default)          = AAHL Context Menu Shell Extension
    \InprocServer32
        (default)      = <dir>\aahl_shellext.dll
        ThreadingModel = Apartment
Software\Classes\*\shellex\ContextMenuHandlers\AAHL          = CLSID
Software\Classes\Directory\shellex\ContextMenuHandlers\AAHL  = CLSID
Software\AAHL                                                = <dir>
```

## Runtime discovery

When the extension runs an action it finds the executables in this order:

1. `AAHL_BIN` environment variable (directory containing the binaries)
2. the DLL's own directory
3. `HKCU\Software\AAHL` (the install directory recorded at registration)

`Verb::Add` runs `aahl-gui --create <files...>`; the GUI asks for the archive
path with a save dialog, then drives the real `aahl.exe` engine
(`-BinDir`, PATH and sibling discovery apply inside the GUI as well).
Extract and test verbs call `aahl.exe extract/test` directly. The menu exits:
`0` success, `3` = the save dialog was cancelled (silent), anything else is
reported as a failure box.

## Threading and lifetimes

- `ThreadingModel = Apartment`: each Explorer thread gets its own instance
  (the class factory is not per-thread cached), eliminating locking.
- Every instance is a fresh object: selection state lives in the instance,
  not globally, so simultaneous icons/Explorer windows cannot cross-talk.
- `IShellExtInit::Initialize` receives a data object; the selected paths are
  extracted from it (HDROP format) with the ANSI representation kept as-is —
  paths that cannot round-trip through ANSI are skipped with a diagnostic
  instead of silently mangled.
- Actions run on a dedicated `aahl-action` thread; a panic inside it is
  caught and reported as a failure box rather than taking the shell down.
- `DllCanUnloadNow` returns `S_FALSE` (the DLL stays loaded while any
  extension instance is outstanding, as COM expects).

## Safety

- Registration writes are per-user, scoped to `HKCU`, and both
  scripts and the DLL's `DllRegisterServer`/`DllUnregisterServer` are
  symmetric: unregistration removes exactly what registration created.
- `CreateInstance` refuses aggregation (`CLASS_E_NOAGGREGATION`), so the
  extension can never be embedded by a hostile aggregator.
- Extraction targets are validated by the CLI; the extension only computes
  `<folder>` / `<folder>\<stem>` from the selected `.aahl` path.

## Verification

The crate ships `tests/com_smoke.rs` (runs with `cargo test -p aahl-shellext`):

- `DllGetClassObject` returns `E_POINTER` for null arguments and
  `E_NOINTERFACE` for unknown riids; the class factory is reachable both
  through `IClassFactory::IID` and through `IUnknown::IID`.
- The class factory is real: `IClassFactory::CreateInstance` (raw vtable and
  windows wrapper) yields an `IContextMenu`; cross-`QueryInterface` between
  `IContextMenu` and `IShellExtInit` works; aggregation is refused.
- With no drop-selection the menu is empty (`S_OK`, zero items);
  `Initialize(null)` is a no-op success; `InvokeCommand` with an empty
  selection is a no-op; `GetCommandString` with no verbs fails cleanly.
- `register_and_unregister_roundtrip` runs the real HKCU registration
  (skipped when the CLSID is already registered, so it never disturbs an
  installed copy).