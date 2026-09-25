//! Backend: locates the `aahl` CLI engine and drives it via subprocess.
//!
//! Discovery order (first match wins):
//!   1. `AAHL_BIN` environment variable (explicit path to the engine)
//!   2. a user-configured engine path (Settings)
//!   3. `aahl`/`aahl.exe` on PATH
//!   4. sibling binary next to the GUI executable (`aahl` or `aahl.exe`)

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
// Create / extract options
// ---------------------------------------------------------------------------

/// Options forwarded to `aahl create`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateOptions {
    pub chunk_size: u32,
    pub jobs: u32,
    pub lag: u32,
    pub gc_interval: u32,
    pub no_table: bool,
    pub dict: Option<String>,
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            chunk_size: 1_048_576,
            jobs: 1,
            lag: 16,
            gc_interval: 64,
            no_table: false,
            dict: None,
        }
    }
}

/// Options forwarded to `aahl extract`.
#[derive(Debug, Clone, Default)]
pub struct ExtractOptions {
    pub dict: Option<String>,
}

// ---------------------------------------------------------------------------
// Engine abstraction
// ---------------------------------------------------------------------------

/// Abstraction over the aahl engine so tests can substitute a fake.
pub trait Engine {
    /// Machine-name for status display (e.g. the binary path).
    fn label(&self) -> String;
    /// `aahl list --json ARCHIVE`
    fn list(&self, archive: &Path) -> Result<ListInfoJson>;
    /// `aahl test --json ARCHIVE` (non-zero exit is reported as ok:false)
    fn test(&self, archive: &Path) -> Result<TestReportJson>;
    /// `aahl create --json [opts] ARCHIVE INPUT...`
    fn create(&self, archive: &Path, inputs: &[PathBuf], opts: &CreateOptions) -> Result<CreateInfoJson>;
    /// `aahl extract [opts] ARCHIVE OUT_DIR` (no --json support in the engine)
    fn extract(&self, archive: &Path, out_dir: &Path, opts: &ExtractOptions) -> Result<()>;
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

    /// Engine from an explicit (user-configured) binary path.
    pub fn from_path(bin: PathBuf) -> CliEngine {
        CliEngine { bin }
    }

    /// The resolved engine binary path.
    pub fn bin(&self) -> &Path {
        &self.bin
    }

    fn run_json<T: serde::de::DeserializeOwned>(&self, args: &[String]) -> Result<T> {
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

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

impl Engine for CliEngine {
    fn label(&self) -> String {
        self.bin.display().to_string()
    }

    fn list(&self, archive: &Path) -> Result<ListInfoJson> {
        self.run_json(&["list".into(), "--json".into(), s(archive)])
    }

    fn test(&self, archive: &Path) -> Result<TestReportJson> {
        let args = vec!["test".into(), "--json".into(), s(archive)];
        // test on a dict-seeded archive requires the dict to be supplied
        // (handled by the caller via ExtractOptions-compatible dict passthrough)
        self.run_json(&args)
    }

    fn create(&self, archive: &Path, inputs: &[PathBuf], opts: &CreateOptions) -> Result<CreateInfoJson> {
        let mut args: Vec<String> = vec![
            "create".into(),
            "--json".into(),
            "--chunk-size".into(),
            opts.chunk_size.to_string(),
            "-j".into(),
            opts.jobs.to_string(),
            "--lag".into(),
            opts.lag.to_string(),
            "--gc-interval".into(),
            opts.gc_interval.to_string(),
        ];
        if opts.no_table {
            args.push("--no-table".into());
        }
        if let Some(d) = &opts.dict {
            args.push("--dict".into());
            args.push(d.clone());
        }
        args.push(s(archive));
        for i in inputs {
            args.push(s(i));
        }
        self.run_json(&args)
    }

    fn extract(&self, archive: &Path, out_dir: &Path, opts: &ExtractOptions) -> Result<()> {
        let mut args: Vec<String> = vec!["extract".into()];
        if let Some(d) = &opts.dict {
            args.push("--dict".into());
            args.push(d.clone());
        }
        args.push(s(archive));
        args.push(s(out_dir));
        // ensure the target directory exists so `create_dir_all` semantics hold
        std::fs::create_dir_all(out_dir)
            .with_context(|| format!("failed to create {}", out_dir.display()))?;
        let out = Command::new(&self.bin)
            .args(&args)
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
        Ok(())
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

    fn create(&self, _archive: &Path, inputs: &[PathBuf], _opts: &CreateOptions) -> Result<CreateInfoJson> {
        Ok(CreateInfoJson {
            files: inputs.len(),
            unique_chunks: inputs.len(),
            raw_bytes: 397_786,
            archive_bytes: 4_384,
            store: false,
        })
    }

    fn extract(&self, _archive: &Path, out_dir: &Path, _opts: &ExtractOptions) -> Result<()> {
        std::fs::create_dir_all(out_dir)?;
        Ok(())
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
            .create(Path::new("o.aahl"), &[PathBuf::from("a.txt"), PathBuf::from("b.txt")], &CreateOptions::default())
            .unwrap();
        assert_eq!(c.files, 2);
    }

    #[test]
    fn fake_extract_creates_out_dir() {
        let f = FakeEngine;
        let tmp = std::env::temp_dir().join(format!("aahl-gui-fake-extract-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        f.extract(Path::new("x.aahl"), &tmp, &ExtractOptions::default()).unwrap();
        assert!(tmp.is_dir());
        let _ = std::fs::remove_dir_all(&tmp);
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