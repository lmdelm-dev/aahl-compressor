//! CLI-level dictionary smoke and regression coverage. Probes the real v6
//! dictionary seam end to end: train -> create --dict -> extract --dict ->
//! test --dict, plus every fail-closed branch (no --dict, wrong archive,
//! plain archive with --dict, corrupt dict file). Shells out to the built
//! release binary so argument framing, exit codes, and stderr all get tested.
#![cfg_attr(not(feature = "test"), allow(unused))]

use std::path::{Path, PathBuf};
use std::process::Command;
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
        msg.push_str("\nSTDERR:\n");
        msg.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    (code, msg)
}

fn file_hash(p: &Path) -> String {
    let data = std::fs::read(p).unwrap();
    format!("{}", blake3::hash(&data))
}

fn hash_of_dir(dir: &Path) -> String {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let mut acc = String::new();
    for n in names {
        acc.push_str(&n);
        acc.push_str(&file_hash(&dir.join(&n)));
    }
    acc
}

#[test]
fn roundtrip_ok() {
    let d = scratch("rt");
    let (a, b) = (
        write_fixture(&d, "a.bin", &[1, 7, 42, 200], &[255, 3, 9, 128]),
        write_fixture(&d, "b.bin", &[9, 8, 7, 6], &[5, 4, 3, 2]),
    );
    let dict = d.join("t.aahld");
    let arc = d.join("out.aahl");

    assert!(run(&["train", dict.to_str().unwrap(), a.to_str().unwrap(), b.to_str().unwrap()]).0 == 0,
        "train failed");
    assert!(dict.is_file(), "dict not written");

    let (c, out) = run(&[
        "create",
        arc.to_str().unwrap(),
        "--chunk-size", "65536",
        "--dict", dict.to_str().unwrap(),
        a.to_str().unwrap(), b.to_str().unwrap(),
    ]);
    assert!(c == 0, "create --dict failed: {}", out);
    assert!(arc.is_file(), "archive not written");

    // extract with dict -> must succeed and reproduce bytes
    let out_dir = d.join("out");
    let (c, msg) = run(&[
        "extract",
        arc.to_str().unwrap(),
        out_dir.to_str().unwrap(),
        "--dict", dict.to_str().unwrap(),
    ]);
    assert!(c == 0, "extract --dict failed: {}", msg);

    // structural equivalence: both inputs restored, archives/dicts excluded
    assert_eq!(file_hash(&out_dir.join("a.bin")), file_hash(&a));
    assert_eq!(file_hash(&out_dir.join("b.bin")), file_hash(&b));

    // test --dict -> ok
    let (c, msg) = run(&["test", arc.to_str().unwrap(), "--dict", dict.to_str().unwrap()]);
    assert!(c == 0, "test --dict failed: {}", msg);
}

#[test]
fn fails_closed_without_dict() {
    let d = scratch("nodict");
    let (a, b) = (
        write_fixture(&d, "a.bin", &[1, 7, 42, 200], &[255, 3, 9, 128]),
        write_fixture(&d, "b.bin", &[9, 8, 7, 6], &[5, 4, 3, 2]),
    );
    let dict = d.join("t.aahld");
    let arc = d.join("out.aahl");
    let _ = run(&["train", dict.to_str().unwrap(), a.to_str().unwrap(), b.to_str().unwrap()]);
    let (c, _) = run(&[
        "create",
        arc.to_str().unwrap(),
        "--dict", dict.to_str().unwrap(),
        a.to_str().unwrap(),
    ]);
    assert!(c == 0, "create --dict should succeed");

    // extract WITHOUT --dict must fail
    let out_dir = d.join("out");
    let (c, msg) = run(&["extract", arc.to_str().unwrap(), out_dir.to_str().unwrap()]);
    assert!(c != 0, "extract without --dict must fail, got: {}", msg);

    // test WITHOUT --dict must fail
    let (c, msg) = run(&["test", arc.to_str().unwrap()]);
    assert!(c != 0, "test without --dict must fail, got: {}", msg);
}

#[test]
fn fails_closed_with_wrong_dict() {
    let d = scratch("wrong");
    let a = write_fixture(&d, "a.bin", &[1, 7, 42, 200], &[255, 3, 9, 128]);
    let dict1 = d.join("d1.aahld");
    let dict2 = d.join("d2.aahld");
    let arc = d.join("out.aahl");

    let _ = run(&["train", dict1.to_str().unwrap(), a.to_str().unwrap()]);
    // train a second dict from different data so its rule hash differs
    let other = write_fixture(&d, "other.bin", &[0, 0, 0, 0], &[1, 1, 1, 1]);
    let _ = run(&["train", dict2.to_str().unwrap(), other.to_str().unwrap()]);

    let (c, _) = run(&[
        "create",
        arc.to_str().unwrap(),
        "--dict", dict1.to_str().unwrap(),
        a.to_str().unwrap(),
    ]);
    assert!(c == 0, "create dict1 should succeed");

    let (c, msg) = run(&[
        "test",
        arc.to_str().unwrap(),
        "--dict", dict2.to_str().unwrap(),
    ]);
    assert!(c != 0, "test with wrong dict must fail, got: {}", msg);
}

#[test]
fn plain_archive_with_dict_fails() {
    let d = scratch("plain");
    let a = write_fixture(&d, "a.bin", &[1, 7, 42, 200], &[255, 3, 9, 128]);
    let dict = d.join("t.aahld");
    let arc = d.join("out.aahl");

    let _ = run(&["train", dict.to_str().unwrap(), a.to_str().unwrap()]);
    let (c, _) = run(&["create", arc.to_str().unwrap(), a.to_str().unwrap()]);
    assert!(c == 0, "plain create should succeed");

    // plain archive + --dict must fail (no dictionary expected)
    let (c, msg) = run(&["test", arc.to_str().unwrap(), "--dict", dict.to_str().unwrap()]);
    assert!(c != 0, "plain archive with --dict must fail, got: {}", msg);
}

#[test]
fn corrupt_dict_fails() {
    let d = scratch("corrupt");
    let a = write_fixture(&d, "a.bin", &[1, 7, 42, 200], &[255, 3, 9, 128]);
    let dict = d.join("t.aahld");
    let arc = d.join("out.aahl");

    let _ = run(&["train", dict.to_str().unwrap(), a.to_str().unwrap()]);
    let (c, _) = run(&[
        "create",
        arc.to_str().unwrap(),
        "--dict", dict.to_str().unwrap(),
        a.to_str().unwrap(),
    ]);
    assert!(c == 0, "create --dict should succeed");

    // Stomp the LAST byte: that is always inside the rules block (offset 77 ..),
    // the region `load` re-checks against the stored rule hash, so a corrupt
    // dict must fail closed regardless of fixture size.
    let data = std::fs::read(&dict).unwrap();
    let mut bad = data.clone();
    let last = bad.len() - 1;
    bad[last] ^= 0xFF;
    let bad_path = d.join("bad.aahld");
    std::fs::write(&bad_path, &bad).unwrap();

    let (c, msg) = run(&[
        "test",
        arc.to_str().unwrap(),
        "--dict", bad_path.to_str().unwrap(),
    ]);
    assert!(c != 0, "corrupt dict must fail, got: {}", msg);
}

#[test]
fn no_dict_roundtrip_byte_identical_to_baseline() {
    // Without --dict the create path must remain byte-identical: no dictionary
    // record may leak into the archive. Determinism: two plain creates must
    // produce the same bytes.
    let d = scratch("plainrt");
    let a = write_fixture(&d, "a.bin", &[1, 7, 42, 200], &[255, 3, 9, 128]);
    let arc1 = d.join("one.aahl");
    let arc2 = d.join("two.aahl");

    let (c, _) = run(&["create", arc1.to_str().unwrap(), "--chunk-size", "65536", a.to_str().unwrap()]);
    assert!(c == 0, "plain create 1 failed");
    let (c, _) = run(&["create", arc2.to_str().unwrap(), "--chunk-size", "65536", a.to_str().unwrap()]);
    assert!(c == 0, "plain create 2 failed");

    assert_eq!(file_hash(&arc1), file_hash(&arc2), "plain create must be deterministic");
}

#[test]
fn dict_create_is_deterministic() {
    let d = scratch("det");
    let a = write_fixture(&d, "a.bin", &[1, 7, 42, 200], &[255, 3, 9, 128]);
    let dict = d.join("t.aahld");
    let arc1 = d.join("one.aahl");
    let arc2 = d.join("two.aahl");

    let _ = run(&["train", dict.to_str().unwrap(), a.to_str().unwrap()]);
    let (c, _) = run(&[
        "create",
        arc1.to_str().unwrap(),
        "--dict", dict.to_str().unwrap(),
        a.to_str().unwrap(),
    ]);
    assert!(c == 0, "dict create 1 failed");
    let (c, _) = run(&[
        "create",
        arc2.to_str().unwrap(),
        "--dict", dict.to_str().unwrap(),
        a.to_str().unwrap(),
    ]);
    assert!(c == 0, "dict create 2 failed");

    assert_eq!(file_hash(&arc1), file_hash(&arc2), "dict create must be deterministic");
}

#[test]
fn dict_create_is_deterministic_across_jobs() {
    let d = scratch("detj");
    let a = write_fixture(&d, "a.bin", &[1, 7, 42, 200], &[255, 3, 9, 128]);
    let dict = d.join("t.aahld");
    let arc1 = d.join("one.aahl");
    let arc8 = d.join("eight.aahl");

    let _ = run(&["train", dict.to_str().unwrap(), a.to_str().unwrap()]);
    let (c, _) = run(&[
        "create",
        arc1.to_str().unwrap(),
        "--dict", dict.to_str().unwrap(),
        "-j", "1",
        a.to_str().unwrap(),
    ]);
    assert!(c == 0, "dict create j1 failed");
    let (c, _) = run(&[
        "create",
        arc8.to_str().unwrap(),
        "--dict", dict.to_str().unwrap(),
        "-j", "8",
        a.to_str().unwrap(),
    ]);
    assert!(c == 0, "dict create j8 failed");

    assert_eq!(file_hash(&arc1), file_hash(&arc8), "dict create must be byte-identical across -j");
}