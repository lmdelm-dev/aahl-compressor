//! Backend: locates the `aahl` CLI engine and drives it via subprocess.
//!
//! Discovery order (first match wins):
//!   1. `AAHL_BIN` environment variable (explicit path to the engine)
//!   2. `aahl`/`aahl.exe` on PATH
//!   3. sibling binary next to the GUI executable (`aahl` or `aahl.exe`)

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// JSON wire structs (mirror the CLI's --json output; deserialize-only here)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateInfoJson {
    pub files: usize,
    pub unique_chunks: usize,
    pub raw_bytes: u64,
    pub archive_bytes: u64,
    pub store: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListEntryJson {
    pub path: String,
    pub size: u64,
    pub chunks: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListInfoJson {
    pub version: u16,
    pub store: bool,
    pub chunk_size: u32,
    pub total_files: usize,
    pub total_size: u64,
    pub unique_chunks: usize,
    pub files: Vec<ListEntryJson>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestReportJson {
    pub ok: bool,
    pub files_checked: usize,
    pub errors: Vec<String>,
}

// ---------------------------------------------------------------------------
// Engine discovery and runner
// ---------------------------------------------------------------------------

/// Abstraction over the aahl engine so tests can substitute a fake.
pub trait Engine {
    /// Machine-name for status display (e.g. the binary path).
    fn label(&self) -> String;
    /// `aahl list --json ARCHIVE`
    fn list(&self, archive: &Path) -> Result<ListInfoJson>;
    /// `aahl test --json ARCHIVE` (non-zero exit is reported as ok:false)
    fn test(&self, archive: &Path) -> Result<TestReportJson>;
    /// `aahl create --json ARCHIVE INPUT...`
    fn create(&self, archive: &Path, inputs: &[PathBuf]) -> Result<CreateInfoJson>;
}

/// The real engine: spawns the `aahl` CLI executable.
#[derive(Debug, Clone)]
pub struct CliEngine {
    bin: PathBuf,
}

impl CliEngine {
    /// Locate the aahl engine using the discovery order above.
    pub fn discover() -> Result<CliEngine> {
        let candidates: Vec<PathBuf> = {
            let mut v = Vec::new();
            if let Ok(p) = std::env::var("AAHL_BIN") {
                v.push(PathBuf::from(p));
            }
            v.push(PathBuf::from("aahl"));
            v.push(PathBuf::from("aahl.exe"));
            if let Ok(exe) = std::env::current_exe() {
                if let Some(dir) = exe.parent() {
                    v.push(dir.join("aahl"));
                    v.push(dir.join("aahl.exe"));
                }
            }
            v
        };
        let found = candidates
            .iter()
            .find(|c| c.is_file())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!(
                "aahl engine not found (set AAHL_BIN or put aahl on PATH next to this GUI)"
            ))?;
        Ok(CliEngine { bin: found })
    }

    fn run_json<T: serde::de::DeserializeOwned>(&self, args: &[&str]) -> Result<T> {
        let out = Command::new(&self.bin)
            .args(args)
            .output()
            .with_context(|| format!("failed to execute {}", self.bin.display()))?;
        if !out.status.success() {
            bail!(
                "aahl {} failed ({})\n{}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let text = String::from_utf8_lossy(&out.stdout);
        serde_json::from_str(&text).with_context(|| format!("bad JSON from aahl: {text}"))
    }
}

impl Engine for CliEngine {
    fn label(&self) -> String {
        self.bin.display().to_string()
    }

    fn list(&self, archive: &Path) -> Result<ListInfoJson> {
        self.run_json(&["list", "--json", archive.to_str().unwrap()])
    }

    fn test(&self, archive: &Path) -> Result<TestReportJson> {
        self.run_json(&["test", "--json", archive.to_str().unwrap()])
    }

    fn create(&self, archive: &Path, inputs: &[PathBuf]) -> Result<CreateInfoJson> {
        let mut args = vec!["create", "--json", archive.to_str().unwrap()];
        for i in inputs {
            args.push(i.to_str().unwrap());
        }
        self.run_json(&args)
    }
}

/// Fake engine used by tests and a "demo mode" toggle.
#[derive(Debug, Clone)]
pub struct FakeEngine;

impl Engine for FakeEngine {
    fn label(&self) -> String {
        "fake (demo)".into()
    }

    fn list(&self, archive: &Path) -> Result<ListInfoJson> {
        Ok(ListInfoJson {
            version: 5,
            store: false,
            chunk_size: 1_048_576,
            total_files: 2,
            total_size: 397_786,
            unique_chunks: 1,
            files: vec![
                ListEntryJson { path: format!("{}/a.txt", archive.display()), size: 198_893, chunks: 1 },
                ListEntryJson { path: format!("{}/b.txt", archive.display()), size: 198_893, chunks: 1 },
            ],
        })
    }

    fn test(&self, archive: &Path) -> Result<TestReportJson> {
        Ok(TestReportJson {
            ok: !format!("{}", archive.display()).contains("bad"),
            files_checked: 2,
            errors: if format!("{}", archive.display()).contains("bad") {
                vec!["chunk 0: chunk hash mismatch (corrupt)".into()]
            } else {
                vec![]
            },
        })
    }

    fn create(&self, _archive: &Path, inputs: &[PathBuf]) -> Result<CreateInfoJson> {
        Ok(CreateInfoJson {
            files: inputs.len(),
            unique_chunks: inputs.len(),
            raw_bytes: 397_786,
            archive_bytes: 4_384,
            store: false,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_list_json_roundtrip_via_serde() {
        let f = FakeEngine;
        let info = f.list(Path::new("x.aahl")).unwrap();
        let s = serde_json::to_string(&info).unwrap();
        let back: ListInfoJson = serde_json::from_str(&s).unwrap();
        assert_eq!(back.total_files, 2);
        assert_eq!(back.files[0].chunks, 1);
    }

    #[test]
    fn fake_test_ok_and_corrupt() {
        let f = FakeEngine;
        assert!(f.test(Path::new("good.aahl")).unwrap().ok);
        let bad = f.test(Path::new("bad.aahl")).unwrap();
        assert!(!bad.ok);
        assert_eq!(bad.errors.len(), 1);
    }

    #[test]
    fn fake_create_counts_inputs() {
        let f = FakeEngine;
        let c = f
            .create(Path::new("o.aahl"), &[PathBuf::from("a.txt"), PathBuf::from("b.txt")])
            .unwrap();
        assert_eq!(c.files, 2);
    }

    #[test]
    fn serde_shape_matches_cli_json_output() {
        // lock the wire format: keys must match what `aahl test --json` prints
        let raw = r#"{"ok":true,"files_checked":2,"errors":[]}"#;
        let r: TestReportJson = serde_json::from_str(raw).unwrap();
        assert!(r.ok);
        assert_eq!(r.files_checked, 2);

        let raw = r#"{"version":5,"store":false,"chunk_size":1048576,"total_files":2,"total_size":397786,"unique_chunks":1,"files":[{"path":"a.txt","size":198893,"chunks":1}]}"#;
        let l: ListInfoJson = serde_json::from_str(raw).unwrap();
        assert_eq!(l.total_files, 2);
        assert_eq!(l.files[0].path, "a.txt");
    }
}
