//! CLI-level dictionary smoke and regression coverage. Probes the real v6
//! dictionary seam end to end: train -> create --dict -> extract --dict ->
//! test --dict, plus every fail-closed branch (no --dict, wrong archive,
//! plain archive with --dict, corrupt dict file). Shells out to the built
//! release binary so argument framing, exit codes, and stderr all get tested.
#![cfg_attr(not(feature = "test"), allow(unused))]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn exe_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if cfg!(debug_assertions) {
        p.push("target/debug/aahl.exe");
    } else {
        p.push("target/release/aahl.exe");
    }
    p
}

fn scratch(tag: &str) -> PathBuf {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("aahl_dict_{}_{}", tag, nanos));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write_fixture(dir: &Path, name: &str, hash1: &[u8], hash2: &[u8]) -> PathBuf {
    // deterministic, high-entropy-ish but compressible enough to exercise dict rules
    let mut out = Vec::new();
    for round in 0..64u32 {
        for b in hash1 {
            let v = (*b as u32).wrapping_add(round.wrapping_mul(17));
            out.push((v & 0xFF) as u8);
        }
    }
    let mut bb = hash2.to_vec();
    bb.extend_from_slice(&hash1);
    for round in 0..128u32 {
        for b in &bb {
            let v = (*b as u32).wrapping_add(round.wrapping_mul(31));
            out.push((v & 0xFF) as u8);
        }
    }
    let p = dir.join(name);
    std::fs::write(&p, &out).unwrap();
    p
}

fn run(args: &[&str]) -> (i32, String) {
    let out = Command::new(exe_path())
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {:?}: {}", args, e));
    let code = out.status.code().unwrap_or(-1);
    let mut msg = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.stderr.is_empty() {
        msg.push_str("\\nSTDERR:\\n");
        msg.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    (code, msg)
}

fn sha256(p: &Path) -> String {
    use std::io::Read;
    let data = std::fs::read(p).unwrap();
    // no sha2 dep guaranteed; use blake3 of the file as the identity proxy
    let h = blake3::hash(&data);
    format!("{:x}", h)
}

fn roundtrip_ok() {
