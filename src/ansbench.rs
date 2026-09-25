//! `ans-bench` harness: experimental tANS/FSE backend (tans.rs) measured
//! against the shipping arithmetic coder (arith.rs) on identical symbol
//! streams, with an honest complete cost (frequency table + framing +
//! payload). This module is measurement-only; it never touches the archive
//! format, and `--recut` is the only code path that re-runs the default
//! create machinery (read-only, to obtain the real emitted inner blocks).
//!
//! Modes:
//!   tans-lit   raw bytes            -> tANS literal (fresh table, alphabet 256)
//!   arith-lit  raw bytes            -> order-0 adaptive range coder ('A' framing)
//!   tans-tok   folded token stream  -> tANS over grammar tokens
//!   arith-tok  folded token stream  -> order-0 adaptive range coder ('A' framing)
//!   tans-recut the default-path inner block bytes (real emitted blocks) -> tANS
//!
//! Costs are complete. The shared 'A' container pays 2 tag + 4 num_rules +
//! 4*rules on both sides. arith pays 4 num_tokens + 4 blob_len (its adaptive
//! order-0 model serializes no table, so table_bits = 0 for arith rows). tANS
//! pays its self-contained block: 1 log2 + 2 num_used + 4*U (sym,freq) pairs
//! + 4 num_tokens + 4 blob_len + blob. recut is compared against the real
//! inner block bytes the default path emits (before the v5 table wrap, which
//! is container structure, not entropy).
//!
//! Determinism: fold_with / arith / tANS are all deterministic; the harness
//! re-encodes each run and asserts byte-identical outputs.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};

use crate::{aahl, arith, grammar, table, tans};

#[derive(Clone, Debug)]
pub struct Options {
    pub runs: usize,
    pub recut: bool,
    pub runs_tsv: PathBuf,
    pub summary_tsv: PathBuf,
    pub chunk_size: usize,
    pub table_log2: u32,
    pub no_tokens: bool,
}

#[derive(Clone, Debug)]
struct Row {
    input: String,
    chunk: u64,
    run: usize,
    mode: String,
    bytes: usize,
    alph_size: usize,
    table_bits: u64,
    payload_bits: u64,
    total_bits: u64,
    total_bytes: usize,
    arith_bytes: usize,
    ratio: f64,
    encode_us: u64,
    decode_us: u64,
    ok: bool,
}

impl Row {
    fn to_tsv(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.6}\t{}\t{}\t{}",
            self.input,
            self.chunk,
            self.run,
            self.mode,
            self.bytes,
            self.alph_size,
            self.table_bits,
            self.payload_bits,
            self.total_bits,
            self.total_bytes,
            self.arith_bytes,
            self.ratio,
            self.encode_us,
            self.decode_us,
            if self.ok { 1 } else { 0 }
        )
    }
}

fn median_us(samples: &[u64]) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut v = samples.to_vec();
    v.sort_unstable();
    v[v.len() / 2]
}

fn write_tsv(path: &Path, header: &str, rows: &[String]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("make dir {}", dir.display()))?;
    }
    let mut out = String::with_capacity(rows.len() * 96 + 256);
    out.push_str(header);
    out.push('\n');
    for r in rows {
        out.push_str(r);
        out.push('\n');
    }
    fs::write(path, out).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-mode measurement
// ---------------------------------------------------------------------------

/// Measure tANS encode+decode on a token stream. `frame_bytes` is the shared
/// container framing paid by this tANS block (2+4+4R for the A-style frame of
/// the token modes, 6 for the literal mode, 0 for standalone recut blocks).
/// Returns (per-run rows, and length of one block as a String for stamping).
fn measure_tans_tokens(
    tokens: &[u16],
    n: usize,
    log2: u32,
    runs: usize,
    bytes: usize,
    frame_bytes: usize,
) -> Result<(Vec<Row>, String)> {
    let mut rows = Vec::with_capacity(runs);
    let mut last_block: Option<Vec<u8>> = None;
    for run in 0..runs {
        let t0 = Instant::now();
        let enc = tans::encode_tokens(tokens, n, log2).map_err(|e| anyhow::anyhow!("tans encode: {e}"))?;
        let t1 = Instant::now();
        let dec = tans::decode_tokens(&enc.block, tokens.len(), n).map_err(|e| anyhow::anyhow!("tans decode: {e}"))?;
        let t2 = Instant::now();
        let mut ok = dec == tokens;
        if let Some(prev) = &last_block {
            if *prev != enc.block {
                ok = false; // determinism violation between runs
            }
        } else {
            last_block = Some(enc.block.clone());
        }
        // The complete cost of this block must decompose exactly as the codec
        // claims: table header + 8-byte framing + payload. Enforces the
        // mission's honest accounting at runtime, not just in tests.
        let body = enc.table_bytes + enc.framing_bytes + enc.payload_bytes;
        if enc.block.len() != body {
            return Err(anyhow::anyhow!(
                "tans: block {} B != table {}+framing {}+payload {} (log2={})",
                enc.block.len(), enc.table_bytes, enc.framing_bytes, enc.payload_bytes, enc.eff_log2
            ));
        }
        let total_bytes = frame_bytes + enc.block.len();
        rows.push(Row {
            input: String::new(),
            chunk: 0,
            run,
            mode: String::new(),
            bytes,
            alph_size: enc.alphabet_size,
            table_bits: enc.table_bytes as u64 * 8,
            payload_bits: enc.payload_bits,
            total_bits: total_bytes as u64 * 8,
            total_bytes,
            arith_bytes: 0,
            ratio: 0.0,
            encode_us: (t1 - t0).as_micros() as u64,
            decode_us: (t2 - t1).as_micros() as u64,
            ok,
        });
    }
    let blen = last_block.map(|b| b.len()).unwrap_or(0);
    Ok((rows, blen.to_string()))
}

/// Measure the adaptive order-0 range coder on a token stream. `frame_bytes`
/// is the full 'A' framing (14 + 4R, or 14 for literals). arith rows are their
/// own reference (ratio 1.0).
fn measure_arith_tokens(
    tokens: &[u16],
    n: usize,
    runs: usize,
    bytes: usize,
    frame_bytes: usize,
) -> Result<(Vec<Row>, String)> {
    let mut rows = Vec::with_capacity(runs);
    let mut last_blob: Option<Vec<u8>> = None;
    for run in 0..runs {
        let t0 = Instant::now();
        let blob = arith::encode_tokens(tokens, n);
        let t1 = Instant::now();
        let mut out = Vec::with_capacity(tokens.len());
        let dec_res = arith::decode_tokens(&blob, n, tokens.len(), &mut out);
        let t2 = Instant::now();
        let mut ok = dec_res.is_ok() && out == tokens;
        if let Some(prev) = &last_blob {
            if *prev != blob {
                ok = false;
            }
        } else {
            last_blob = Some(blob.clone());
        }
        let total_bytes = frame_bytes + blob.len();
        rows.push(Row {
            input: String::new(),
            chunk: 0,
            run,
            mode: String::new(),
            bytes,
            alph_size: 0, // adaptive order-0: no serialized alphabet
            table_bits: 0,
            payload_bits: blob.len() as u64 * 8,
            total_bits: total_bytes as u64 * 8,
            total_bytes,
            arith_bytes: total_bytes,
            ratio: 1.0,
            encode_us: (t1 - t0).as_micros() as u64,
            decode_us: (t2 - t1).as_micros() as u64,
            ok,
        });
    }
    let blen = last_blob.map(|b| b.len()).unwrap_or(0);
    Ok((rows, blen.to_string()))
}

/// Measure tANS over raw bytes (`frame_bytes` for the literal/recut contexts).
fn measure_tans_bytes(
    raw: &[u8],
    log2: u32,
    runs: usize,
    frame_bytes: usize,
) -> Result<(Vec<Row>, String)> {
    let mut rows = Vec::with_capacity(runs);
    let mut last_block: Option<Vec<u8>> = None;
    for run in 0..runs {
        let t0 = Instant::now();
        let enc = tans::encode_bytes(raw, log2).map_err(|e| anyhow::anyhow!("tans bytes: {e}"))?;
        let t1 = Instant::now();
        let dec = tans::decode_bytes(&enc.block, raw.len()).map_err(|e| anyhow::anyhow!("tans bytes decode: {e}"))?;
        let t2 = Instant::now();
        let mut ok = dec == raw;
        if let Some(prev) = &last_block {
            if *prev != enc.block {
                ok = false;
            }
        } else {
            last_block = Some(enc.block.clone());
        }
        let total_bytes = frame_bytes + enc.block.len();
        rows.push(Row {
            input: String::new(),
            chunk: 0,
            run,
            mode: String::new(),
            bytes: raw.len(),
            alph_size: enc.alphabet_size,
            table_bits: enc.table_bytes as u64 * 8,
            payload_bits: enc.payload_bits,
            total_bits: total_bytes as u64 * 8,
            total_bytes,
            arith_bytes: 0,
            ratio: 0.0,
            encode_us: (t1 - t0).as_micros() as u64,
            decode_us: (t2 - t1).as_micros() as u64,
            ok,
        });
    }
    let blen = last_block.map(|b| b.len()).unwrap_or(0);
    Ok((rows, blen.to_string()))
}

// ---------------------------------------------------------------------------
// Chunk-level drivers
// ---------------------------------------------------------------------------

struct Runner {
    rows: Vec<Row>,
    warnings: Vec<String>,
    any_fail: bool,
    arith_sizes_match: bool,
}

impl Runner {
    fn record(&mut self, row: Row) {
        if !row.ok {
            self.any_fail = true;
        }
        self.rows.push(row);
    }
}

/// Measure a single chunk across the literal and token modes.
fn measure_chunk(
    r: &mut Runner,
    input: &str,
    chunk_idx: u64,
    raw: &[u8],
    opts: &Options,
    fold_cache: &mut Option<(Vec<u16>, Vec<(u16, u16)>)>,
) -> Result<()> {
    let l2 = opts.table_log2;

    // ---- literal modes ----
    let bytes_u16: Vec<u16> = raw.iter().map(|&b| b as u16).collect();
    let (arith_rows, arith_lit_blob) =
        measure_arith_tokens(&bytes_u16, 256, opts.runs, raw.len(), 14)?;
    let arith_lit_complete = 14 + arith_lit_blob.parse::<usize>().unwrap_or(0);
    let (mut tans_rows, _tblen) = measure_tans_bytes(raw, l2, opts.runs, 6)?;
    for mut row in tans_rows.drain(..) {
        row.arith_bytes = arith_lit_complete;
        row.ratio = row.total_bytes as f64 / arith_lit_complete as f64;
        row.input = input.to_string();
        row.chunk = chunk_idx;
        row.mode = "tans-lit".to_string();
        r.record(row);
    }
    for mut row in arith_rows.into_iter() {
        row.input = input.to_string();
        row.chunk = chunk_idx;
        row.mode = "arith-lit".to_string();
        r.record(row);
    }

    // ---- token modes ----
    if !opts.no_tokens {
        if fold_cache.is_none() {
            let initial: Vec<u16> = raw.iter().map(|&b| b as u16).collect();
            *fold_cache = Some(aahl::fold_with(initial, &aahl::FoldConfig::default()));
        }
        let (tokens, rules) = fold_cache.as_ref().unwrap();
        let nr = rules.len();
        let n = 256 + nr;

        // arith-tok first: its complete size is the reference for tans-tok.
        let (arith_rows, arith_blob) =
            measure_arith_tokens(tokens, n, opts.runs, raw.len(), 14 + 4 * nr)?;
        let arith_tok_complete = 14 + 4 * nr + arith_blob.parse::<usize>().unwrap_or(0);

        // Cross-check against the 'A' sizing helper used by the other tools.
        let (nr2, nt2, asize) = aahl::arith_sizes(raw, &aahl::FoldConfig::default());
        if nr2 != nr || nt2 != tokens.len() || asize != arith_tok_complete {
            r.warnings.push(format!(
                "{} chunk {}: arith_sizes mismatch (rules {} vs {}, tokens {} vs {}, A {} vs {})",
                input, chunk_idx, nr2, nr, nt2, tokens.len(), asize, arith_tok_complete
            ));
            r.arith_sizes_match = false;
        }

        let (mut tans_rows, _tblen2) =
            measure_tans_tokens(tokens, n, l2, opts.runs, raw.len(), 6 + 4 * nr)?;
        for mut row in tans_rows.drain(..) {
            row.arith_bytes = arith_tok_complete;
            row.ratio = row.total_bytes as f64 / arith_tok_complete as f64;
            row.input = input.to_string();
            row.chunk = chunk_idx;
            row.mode = "tans-tok".to_string();
            r.record(row);
        }
        for mut row in arith_rows.into_iter() {
            row.input = input.to_string();
            row.chunk = chunk_idx;
            row.mode = "arith-tok".to_string();
            r.record(row);
        }
    }
    Ok(())
}

/// Replicate the default create path's chunk compressor and recut the real
/// emitted inner blocks with tANS (reference = inner block byte count).
fn measure_recut(
    r: &mut Runner,
    input: &str,
    chunk_idx: u64,
    raw: &[u8],
    opts: &Options,
    grammar: &mut grammar::PersistentGrammar,
) -> Result<()> {
    let plan = table::prepare(raw, None).with_context(|| format!("table::prepare chunk {}", chunk_idx))?;
    let stream = match &plan {
        Some(p) => p.t_stream.clone(),
        None => raw.to_vec(),
    };
    let (inner, _gc) = grammar.compress(&stream);

    let (mut tans_rows, _blen) = measure_tans_bytes(&inner, opts.table_log2, opts.runs, 0)?;
    let inner_len = inner.len();
    for mut row in tans_rows.drain(..) {
        row.arith_bytes = inner_len;
        row.ratio = row.total_bytes as f64 / inner_len as f64;
        row.input = input.to_string();
        row.chunk = chunk_idx;
        row.mode = "tans-recut".to_string();
        r.record(row);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Aggregation + top-level
// ---------------------------------------------------------------------------

fn summarize(rows: &[Row]) -> Vec<String> {
    let mut map: BTreeMap<(String, String), (usize, u64, u64, u64, u64, Vec<u64>, Vec<u64>, bool)> =
        BTreeMap::new();
    for row in rows {
        let key = (row.input.clone(), row.mode.clone());
        let e = map.entry(key).or_insert((0, 0, 0, 0, 0, Vec::new(), Vec::new(), true));
        e.0 = e.0.max(row.alph_size);
        e.1 += row.table_bits;
        e.2 += row.payload_bits;
        e.3 += row.total_bytes as u64;
        e.4 += row.arith_bytes as u64;
        e.5.push(row.encode_us);
        e.6.push(row.decode_us);
        e.7 = e.7 && row.ok;
    }
    let mut lines: Vec<String> = Vec::new();
    for ((input, mode), (alpha, tbits, pbits, tbytes, abytes, es, ds, ok)) in map {
        let a = abytes.max(1);
        let delta = tbytes as i64 - abytes as i64;
        let pct = delta as f64 * 100.0 / a as f64;
        let enc_ms = median_us(&es) as f64 / 1000.0;
        let dec_ms = median_us(&ds) as f64 / 1000.0;
        lines.push(format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.4}\t{:.3}\t{:.3}\t{}",
            input, mode, alpha, tbits, pbits, tbytes, abytes, delta, pct, enc_ms, dec_ms, if ok { 1 } else { 0 }
        ));
    }
    lines
}

pub fn run_ans_bench(inputs: &[PathBuf], opts: &Options) -> Result<()> {
    if inputs.is_empty() {
        bail!("ans-bench: no inputs");
    }
    if opts.runs == 0 {
        bail!("ans-bench: --runs must be >= 1");
    }
    if opts.table_log2 < 7 || opts.table_log2 > 12 {
        bail!("ans-bench: --table-log2 must be 7..=12");
    }
    let mut runner = Runner {
        rows: Vec::new(),
        warnings: Vec::new(),
        any_fail: false,
        arith_sizes_match: true,
    };

    for input in inputs {
        let data = fs::read(input).with_context(|| format!("read {}", input.display()))?;
        let label = input
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| input.display().to_string());
        if data.is_empty() {
            eprintln!("ans-bench: {} (0 bytes, skipped)", label);
            continue;
        }
        let mut grammar = if opts.recut {
            Some(grammar::PersistentGrammar::with_lag_v4(
                aahl::FoldConfig::default(),
                64,
                16,
            ))
        } else {
            None
        };
        let mut chunk_idx = 0u64;
        for piece in data.chunks(opts.chunk_size) {
            // fold cache: valid for ONE chunk only (folded tokens + rules are a
            // pure function of the chunk bytes); must not leak across chunks.
            let mut fold_cache: Option<(Vec<u16>, Vec<(u16, u16)>)> = None;
            measure_chunk(&mut runner, &label, chunk_idx, piece, opts, &mut fold_cache)?;
            if let Some(g) = grammar.as_mut() {
                measure_recut(&mut runner, &label, chunk_idx, piece, opts, g)?;
            }
            chunk_idx += 1;
        }
    }

    let header = "input\tchunk\trun\tmode\tbytes\talph_size\ttable_bits\tpayload_bits\ttotal_bits\ttotal_bytes\tarith_bytes\tratio\tencode_us\tdecode_us\tok";
    let rows: Vec<String> = runner.rows.iter().map(|r| r.to_tsv()).collect();
    write_tsv(&opts.runs_tsv, header, &rows)?;

    let mut summary = vec!["input\tmode\talph size\ttable bits\tpayload bits\ttotal bytes\tarith_bytes\tdelta_bytes\tdelta_%\tencode_ms\tdecode_ms\tok".to_string()];
    summary.extend(summarize(&runner.rows));
    write_tsv(&opts.summary_tsv, &summary[0], &summary[1..])?;

    for w in &runner.warnings {
        eprintln!("ans-bench warning: {w}");
    }
    println!(
        "ans-bench: {} inputs, {} ts-rows ({} runs max), recut={}; arith_sizes cross-check {}",
        inputs.len(),
        runner.rows.len(),
        opts.runs,
        opts.recut,
        if runner.arith_sizes_match { "ok" } else { "MISMATCH" }
    );
    if runner.any_fail {
        bail!("ans-bench: one or more round-trip/determinism checks failed - see runs TSV");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_helpers_work() {
        assert_eq!(median_us(&[1, 5, 3]), 3);
        assert_eq!(median_us(&[]), 0);
    }
}
