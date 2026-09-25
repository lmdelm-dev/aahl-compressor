//! Grammar-dictionary training (`aahl train`) and the `.aahld` dictionary file
//! (STEP 4 of V6). A dictionary pre-seeds the persistent grammar's rule table
//! before chunk 0, so neither side ever retransmits those definitions. Rules
//! are pure grammar (rule id -> body of tokens/literals), never LZ matches:
//! the encoder finds reuse through the existing phrase-trie longest-match
//! pass, and the decoder unfolds ids through the seeded table.
//!
//! File format (`AAHD`, version 1, little-endian, no framing beyond length):
//!   offset 0:  magic "AAHD"             [4B]
//!   offset 4:  version u16 = 1          [2B]
//!   offset 6:  hash_alg u8 = 0 (blake3) [1B]
//!   offset 7:  rule_count u32           [4B]
//!   offset 11: sample_hash blake3[32]   [32B] blake3 over every training byte
//!   offset 43: reserved u16 = 0         [2B]
//!   offset 45: rule_hash blake3[32]     [32B] blake3 over the rules block [77..]
//!   offset 77: rules: (l u16, r u16) * rule_count
//! total size = 77 + 4 * rule_count.
//!
//! The archive records `rule_hash` + `rule_count` (RECORD_DICT / STORE mode 1)
//! and the decoder fails closed unless the supplied dict reproduces both.
//!
//! Training: each kept chunk (byte entropy < 7.9, mirroring the compressor's
//! fast path) is folded with the incremental BPE engine against a GLOBAL pair
//! map, so rules deduplicate across chunks and receive stable ids in creation
//! order. A rule's id is 256 + position, and every rule references only
//! earlier ids, so any prefix of the table is a closed grammar. Benefits are
//! measured at invention: benefit = rep * (expand_len - 1) - 4, where the -4
//! is the 4-byte storage cost of the rule in the dict file.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::aahl::{byte_entropy, bpe_engine, FoldConfig};
use crate::corpus;

/// Maximum seed rules: grammar rule ids are 256 + index and MAX_SYMS caps the
/// alphabet at 4096.
pub const MAX_RULES: usize = 3840;
const MAGIC: &[u8; 4] = b"AAHD";
const FORMAT_VERSION: u16 = 1;
const HASH_ALG: u8 = 0;
const HEADER_LEN: usize = 77;
/// Entropy threshold for skipping chunks during training (mirror of the
/// compressor's fast path: high-entropy chunks barely fold).
const ENTR_BAD: f64 = 7.9;
/// Training fold depth per chunk. Higher than the archive fold (1024) so the
/// dict can own rules the archive will then reuse cheaply.
const TRAIN_MERGES: usize = 4096;
/// Expansion-length saturation for benefit accounting.
const EXP_CAP: u64 = 1 << 24;

/// Training knobs.
#[derive(Debug, Clone)]
pub struct TrainOptions {
    /// Chunk size for splitting the training stream (must be 4KiB..1MiB).
    pub chunk_size: usize,
    /// Hard cap on dict rules (clamped to MAX_RULES). Applied as a prefix,
    /// keeping the table a closed grammar.
    pub max_rules: usize,
    /// Drop rules whose measured benefit is below this. 0 = keep all trained.
    /// Applied as a prefix trim (first failing index onward), a conservative
    /// filter because BPE creation order is benefit-ordered within a chunk
    /// and near-ordered across chunks.
    pub min_benefit: i64,
}

impl Default for TrainOptions {
    fn default() -> Self {
        Self { chunk_size: 65536, max_rules: MAX_RULES, min_benefit: 1 }
    }
}

/// Loaded dictionary plus the validation results that bound it to an archive.
#[derive(Debug, Clone)]
pub struct Dict {
    /// (l, r) rule definitions; id = 256 + position.
    pub rules: Vec<(u16, u16)>,
    /// blake3 over the serialized rules block (matches the file and the
    /// archive's RECORD_DICT / STORE mode-1 marker).
    pub rule_hash: [u8; 32],
    /// blake3 over every training byte (provenance; not required by decode).
    pub sample_hash: [u8; 32],
}

/// blake3 over the rules serialized as little-endian (l u16, r u16) pairs,
/// i.e. exactly the bytes stored at offset 77 of the dict file.
pub fn rule_hash_of(rules: &[(u16, u16)]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    for &(l, r) in rules {
        h.update(&l.to_le_bytes());
        h.update(&r.to_le_bytes());
    }
    *h.finalize().as_bytes()
}

/// Serialize a rule table into the canonical dict file layout.
pub fn save(path: &Path, rules: &[(u16, u16)], sample_hash: [u8; 32]) -> Result<()> {
    if rules.len() > MAX_RULES {
        bail!("too many rules ({} > {MAX_RULES})", rules.len());
    }
    let count = rules.len() as u32;
    let rule_hash = rule_hash_of(rules);
    let mut out = Vec::with_capacity(HEADER_LEN + 4 * rules.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.push(HASH_ALG);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&sample_hash);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&rule_hash);
    for &(l, r) in rules {
        out.extend_from_slice(&l.to_le_bytes());
        out.extend_from_slice(&r.to_le_bytes());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(path, &out).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Load and fully validate a dict file (magic, version, algorithm, lengths,
/// and a recomputed rule hash over the rules block).
pub fn load(path: &Path) -> Result<Dict> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    if bytes.len() < HEADER_LEN {
        bail!("{}: dict file too small", path.display());
    }
    if &bytes[0..4] != MAGIC {
        bail!("{}: bad dict magic", path.display());
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != FORMAT_VERSION {
        bail!("{}: unsupported dict version {version}", path.display());
    }
    if bytes[6] != HASH_ALG {
        bail!("{}: unsupported dict hash_alg {}", path.display(), bytes[6]);
    }
    let n = u32::from_le_bytes([bytes[7], bytes[8], bytes[9], bytes[10]]) as usize;
    if n > MAX_RULES {
        bail!("{}: dict rule count {n} exceeds cap", path.display());
    }
    if bytes.len() != HEADER_LEN + 4 * n {
        bail!("{}: dict length mismatch", path.display());
    }
    let mut sample_hash = [0u8; 32];
    sample_hash.copy_from_slice(&bytes[11..43]);
    if bytes[43] != 0 || bytes[44] != 0 {
        bail!("{}: dict reserved bytes nonzero", path.display());
    }
    let mut stored_hash = [0u8; 32];
    stored_hash.copy_from_slice(&bytes[45..77]);
    let mut rules: Vec<(u16, u16)> = Vec::with_capacity(n);
    for pair in bytes[77..].chunks_exact(4) {
        let l = u16::from_le_bytes([pair[0], pair[1]]);
        let r = u16::from_le_bytes([pair[2], pair[3]]);
        rules.push((l, r));
    }
    let want = rule_hash_of(&rules);
    if stored_hash != want {
        bail!("{}: dict rule hash mismatch (corrupt)", path.display());
    }
    Ok(Dict { rules, rule_hash: stored_hash, sample_hash })
}

/// Training report: the final rule table plus diagnostics for the CLI and
/// tests (benefits come from the same deterministic pass that built the rules).
#[derive(Debug)]
pub struct TrainReport {
    pub rules: Vec<(u16, u16)>,
    /// measured benefit at creation per rule (parallel to `rules`)
    pub benefits: Vec<i64>,
    /// saturation-capped expanded length per rule (parallel to `rules`)
    pub expansions: Vec<u64>,
    pub sample_hash: [u8; 32],
    pub raw_bytes: u64,
    pub chunks_total: usize,
    pub chunks_kept: usize,
    /// rules dropped by the benefit filter (count, for reporting)
    pub trimmed_by_benefit: usize,
}

/// Train a dictionary from sample files and/or directories. Files are read in
/// a deterministic order (arc-name sorted), so the dict is byte-identical for
/// any input ordering of the same sample set.
pub fn train(inputs: &[PathBuf], opts: &TrainOptions) -> Result<TrainReport> {
    if !(4096..=1_048_576).contains(&opts.chunk_size) {
        bail!("--chunk-size must be 4KiB..1MiB");
    }
    if opts.max_rules > MAX_RULES {
        bail!("--max-rules must be <= {MAX_RULES}");
    }
    if opts.min_benefit < 0 {
        bail!("--min-benefit must be >= 0");
    }
    let files = collect_train_files(inputs)?;
    if files.is_empty() {
        bail!("no sample files found");
    }

    // Pass 1: hash every training byte (sample_hash) and split every file into
    // chunks, keeping only chunks the compressor would actually fold (mirror
    // of the fast path).
    let mut hasher = blake3::Hasher::new();
    let mut raw_bytes = 0u64;
    let mut kept: Vec<Vec<u8>> = Vec::new();
    let mut chunks_total = 0usize;
    for (_, disk) in &files {
        let data = fs::read(disk).with_context(|| format!("read {}", disk.display()))?;
        hasher.update(&data);
        raw_bytes += data.len() as u64;
        for piece in data.chunks(opts.chunk_size) {
            chunks_total += 1;
            if byte_entropy(piece) < ENTR_BAD {
                kept.push(piece.to_vec());
            }
        }
    }
    let sample_hash = *hasher.finalize().as_bytes();
    let chunks_kept = kept.len();

    // Pass 2: fold every kept chunk against one GLOBAL pair map. Rule ids are
    // assigned in creation order (256 + rules.len()), so a rule references
    // only earlier ids and any prefix is a closed grammar.
    let max_rules = opts.max_rules.min(MAX_RULES);
    let cfg = FoldConfig { max_merges: TRAIN_MERGES, min_pair_count: 2, max_syms: MAX_RULES + 256 };
    let mut rules: Vec<(u16, u16)> = Vec::with_capacity(1024);
    let mut benefits: Vec<i64> = Vec::new();
    let mut expansions: Vec<u64> = Vec::new();
    let mut pair: HashMap<u32, u16> = HashMap::with_capacity(4096);
    for chunk in &kept {
        if rules.len() >= max_rules {
            break;
        }
        let syms: Vec<u16> = chunk.iter().map(|&b| b as u16).collect();
        let _tokens = bpe_engine(
            syms,
            &cfg,
            |_| true, // rules-length gating happens in on_pair below
            |l, r, rep| {
                if rep < 2 || rules.len() >= max_rules {
                    return (None, false);
                }
                let key = ((l as u32) << 16) | r as u32;
                if let Some(&id) = pair.get(&key) {
                    // Already created in an earlier chunk: reuse, no new id.
                    return (Some(id), false);
                }
                let len_l = if l < 256 { 1 } else { expansions[(l - 256) as usize] };
                let len_r = if r < 256 { 1 } else { expansions[(r - 256) as usize] };
                let expand = len_l.saturating_add(len_r).min(EXP_CAP);
                let id = (256 + rules.len()) as u16;
                let benefit = (rep as i64) * (expand as i64 - 1) - 4;
                rules.push((l, r));
                expansions.push(expand);
                benefits.push(benefit);
                pair.insert(key, id);
                (Some(id), true)
            },
        );
    }

    // Cap as a prefix (dependency-closed), then trim the benefit tail.
    let mut trimmed_by_benefit = 0usize;
    rules.truncate(max_rules);
    expansions.truncate(max_rules);
    benefits.truncate(max_rules);
    if opts.min_benefit > 0 {
        let mut cut = rules.len();
        for (i, &b) in benefits.iter().enumerate() {
            if b < opts.min_benefit {
                cut = i;
                break;
            }
        }
        trimmed_by_benefit = rules.len() - cut;
        rules.truncate(cut);
        expansions.truncate(cut);
        benefits.truncate(cut);
    }

    Ok(TrainReport {
        rules,
        benefits,
        expansions,
        sample_hash,
        raw_bytes,
        chunks_total,
        chunks_kept,
        trimmed_by_benefit,
    })
}

/// Deterministic file collection: single files keep their file name; a
/// directory contributes its whole subtree under the directory's base name.
/// The result is sorted by arc name so the dict is independent of argument
/// order and readdir order.
fn collect_train_files(inputs: &[PathBuf]) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    for inp in inputs {
        if !inp.exists() {
            bail!("sample not found: {}", inp.display());
        }
        if inp.is_file() {
            let name = inp
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "sample".to_string());
            out.push((name, inp.clone()));
        } else {
            let root = inp
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            for (rel, p) in corpus::list_files(inp)? {
                let arc = if root.is_empty() { rel.clone() } else { format!("{root}/{rel}") };
                out.push((arc, p));
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("aahl_dict_{}_{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn prose() -> Vec<u8> {
        b"the quick brown fox jumps over the lazy dog while the farmer watches
the brown dog from the barn door and the quick fox runs back to the den
the the the the the the the the the the the the the the the the the the
quick quick quick quick quick quick quick quick quick quick quick quick
brown brown brown brown brown brown brown brown brown brown brown brown
dog dog dog dog dog dog dog dog dog dog dog dog dog dog dog dog dog dog"
            .repeat(12)
            .to_vec()
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tmp("rt");
        let p = dir.join("d.aahld");
        let rules = vec![(b't' as u16, b'h' as u16), (256, b'e' as u16), (257, 258)];
        let sh = [7u8; 32];
        save(&p, &rules, sh).unwrap();
        let d = load(&p).unwrap();
        assert_eq!(d.rules, rules);
        assert_eq!(d.sample_hash, sh);
        assert_eq!(d.rule_hash, rule_hash_of(&rules));
    }

    #[test]
    fn load_rejects_corruption() {
        let dir = tmp("corrupt");
        let rules = vec![(b't' as u16, b'h' as u16), (256, b'e' as u16)];
        let p = dir.join("c.aahld");
        save(&p, &rules, [0u8; 32]).unwrap();
        let mut bytes = fs::read(&p).unwrap();

        let mut bad = bytes.clone();
        bad[0] = b'X';
        fs::write(dir.join("magic.aahld"), &bad).unwrap();
        assert!(load(&dir.join("magic.aahld")).is_err());

        let mut bad = bytes.clone();
        bad[4] = 9;
        fs::write(dir.join("ver.aahld"), &bad).unwrap();
        assert!(load(&dir.join("ver.aahld")).is_err());

        let mut bad = bytes.clone();
        bad[6] = 1;
        fs::write(dir.join("alg.aahld"), &bad).unwrap();
        assert!(load(&dir.join("alg.aahld")).is_err());

        let mut bad = bytes.clone();
        bad[43] = 1;
        fs::write(dir.join("res.aahld"), &bad).unwrap();
        assert!(load(&dir.join("res.aahld")).is_err());

        // flip a byte inside the rules block: stored rule_hash no longer matches
        let n = bytes.len();
        bad = bytes.clone();
        bad[n - 1] ^= 0xFF;
        fs::write(dir.join("hash.aahld"), &bad).unwrap();
        assert!(load(&dir.join("hash.aahld")).is_err());

        // truncated file
        fs::write(dir.join("short.aahld"), &bytes[..20]).unwrap();
        assert!(load(&dir.join("short.aahld")).is_err());
    }

    #[test]
    fn train_is_deterministic() {
        let dir = tmp("det");
        let a = dir.join("a.bin");
        let b = dir.join("b.bin");
        fs::write(&a, &prose()).unwrap();
        fs::write(&b, &prose()[..5000]).unwrap();
        let opts = TrainOptions::default();
        let one = train(&[a.clone(), b.clone()], &opts).unwrap();
        // same set, different argument order and reversed
        let two = train(&[b.clone(), a.clone()], &opts).unwrap();
        assert_eq!(one.rules, two.rules);
        assert_eq!(one.benefits, two.benefits);
        assert_eq!(one.expansions, two.expansions);
        assert_eq!(one.sample_hash, two.sample_hash);
    }

    #[test]
    fn train_rules_are_closed_dag() {
        let dir = tmp("dag");
        let f = dir.join("p.bin");
        fs::write(&f, &prose()).unwrap();
        let rep = train(&[f], &TrainOptions::default()).unwrap();
        for (i, &(l, r)) in rep.rules.iter().enumerate() {
            let own = 256 + i;
            assert!((l as usize) < own, "rule {i} forward ref l={l}");
            assert!((r as usize) < own, "rule {i} forward ref r={r}");
        }
    }

    #[test]
    fn train_respects_max_rules_cap() {
        let dir = tmp("cap");
        let f = dir.join("p.bin");
        fs::write(&f, &prose()).unwrap();
        let opts = TrainOptions { max_rules: 40, ..TrainOptions::default() };
        let rep = train(&[f], &opts).unwrap();
        assert!(rep.rules.len() <= 40);
        // cap is a prefix of the uncapped result
        let full = train(&[dir.join("p.bin")], &TrainOptions::default()).unwrap();
        assert!(full.rules.starts_with(&rep.rules));
    }

    #[test]
    fn train_benefit_trim_is_prefix() {
        let dir = tmp("trim");
        let f = dir.join("p.bin");
        fs::write(&f, &prose()).unwrap();
        let all = train(&[f.clone()], &TrainOptions { min_benefit: 0, ..TrainOptions::default() }).unwrap();
        let kept = train(&[f], &TrainOptions { min_benefit: 2000, ..TrainOptions::default() }).unwrap();
        assert!(kept.rules.len() <= all.rules.len());
        assert!(all.rules.starts_with(&kept.rules));
        assert!(kept.trimmed_by_benefit > 0);
    }

    #[test]
    fn train_empty_samples_errors() {
        let dir = tmp("empty");
        let missing = dir.join("nope.bin");
        assert!(train(&[missing], &TrainOptions::default()).is_err());
        assert!(train(&[], &TrainOptions::default()).is_err());
    }
}