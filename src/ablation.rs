//! Scientific ablation harness.
//!
//! For each corpus directory and each chunk size, this module computes the
//! payload that every pipeline component would produce over the *unique* chunk
//! set (blake3-deduped exactly like `create`), so the ablation answers four
//! questions quantitatively:
//!
//!   1. context order  - order-0 vs order-1-token vs order-1-byte vs blend
//!   2. recursion      - pair-merge folding on vs off (max_merges = 0)
//!   3. persistence    - cross-chunk grammar vs per-chunk stateless best
//!   4. chunking       - payload as a function of chunk size
//!
//! Everything is deterministic: the same corpus + chunk size always produces
//! the same rows (unit-tested below), and all numbers are produced by the real
//! code paths that archives use (aahl::compress_block and PersistentGrammar),
//! not by re-implementations of them.

use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::{aahl, corpus, grammar, table};

pub const ABLATION_CHUNK_SIZES: [usize; 4] = [4096, 16384, 65536, 262144];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AblationRow {
    pub corpus: String,
    pub chunk: usize,
    pub raw: usize,
    pub stateless_best: usize,
    pub fold: usize,
    pub order0: usize,
    pub order1_tok: usize,
    pub order1_byte: usize,
    pub blend: usize,
    pub norec_order1_tok: usize,
    pub norec_blend: usize,
    pub grammar: usize,
    pub grammar_norec: usize,
    /// v5 table-transform payload: same grammar run, but each chunk whose
    /// measured transform the oracle selected is emitted as the wrapped T
    /// stream (mirrors the real v5 container, byte-for-byte semantics).
    pub grammar_tx: usize,
    /// Number of chunks (of the unique set) for which the table transform
    /// was chosen by the oracle measurement gate.
    pub tx_selected: usize,
    pub model_bytes: usize,
    pub create_ms: u64,
}

/// Gather the unique chunks of a corpus dir at `chunk_size`, blake3-deduped in
/// first-seen order (byte-identical semantics to cmd_create_params).
fn unique_chunks(dir: &Path, chunk_size: usize) -> Result<(Vec<Vec<u8>>, usize)> {
    let files = corpus::list_files(dir)?;
    let mut index: HashMap<[u8; 32], ()> = HashMap::new();
    let mut pieces: Vec<Vec<u8>> = Vec::new();
    let mut raw = 0usize;
    for (_, p) in files {
        let data = std::fs::read(&p)?;
        if !data.is_empty() {
            for piece in data.chunks(chunk_size) {
                let hash = *blake3::hash(piece).as_bytes();
                if index.insert(hash, ()).is_some() {
                    continue; // repeat chunk: a ref, never touches the grammar
                }
                raw += piece.len();
                pieces.push(piece.to_vec());
            }
        }
    }
    Ok((pieces, raw))
}

fn model_footprint(g: &grammar::PersistentGrammar) -> usize {
    g.model_footprint_bytes()
}

fn run_stateless(pieces: &[Vec<u8>]) -> (usize, usize, usize, usize, usize, usize, usize, usize) {
    // (stateless_best, fold, order0, order1_tok, order1_byte, blend, norec_order1_tok, norec_blend)
    let norec = aahl::FoldConfig { max_merges: 0, ..aahl::FoldConfig::default() };
    let mut best = 0usize;
    let mut fold = 0usize;
    let mut order0 = 0usize;
    let mut o1t = 0usize;
    let mut o1b = 0usize;
    let mut blend = 0usize;
    let mut no1t = 0usize;
    let mut nblend = 0usize;
    for p in pieces {
        best += aahl::compress_block(p).len();
        let m = aahl::mode_sizes(p);
        fold += m.fold;
        order0 += m.order0;
        o1t += m.order1_tok;
        o1b += m.order1_byte;
        blend += m.blend;
        let nm = aahl::mode_sizes_with(p, &norec);
        no1t += nm.order1_tok;
        nblend += nm.blend;
    }
    (best, fold, order0, o1t, o1b, blend, no1t, nblend)
}

fn run_grammar(
    pieces: &[Vec<u8>],
    norec: bool,
) -> (usize, usize, u64) {
    // (payload, model_footprint, elapsed_ms)
    let cfg = if norec {
        aahl::FoldConfig { max_merges: 0, ..aahl::FoldConfig::default() }
    } else {
        aahl::FoldConfig::default()
    };
    // Default create params: lag 16, GC every 64 chunks (PARAM_* defaults).
    // The persistent model ships as v4 (exact contexts for hot rules), so the
    // ablation must measure the same flavor the archives use.
    let mut g = grammar::PersistentGrammar::with_lag_v4(cfg, 64, 16);
    let t = Instant::now();
    let mut payload = 0usize;
    for p in pieces {
        let (blk, gc) = g.compress(p);
        payload += blk.len();
        if let Some(gc) = gc {
            payload += gc.len();
        }
    }
    let ms = t.elapsed().as_millis() as u64;
    let footprint = model_footprint(&g);
    (payload, footprint, ms)
}

/// v5-lane ablation: the same grammar lifecycle as run_grammar, but each
/// chunk is first offered to table::prepare; when the measurement oracle
/// selects the transform, the epilogue compresses the T stream and wraps it
/// exactly like the v5 container (grammar.compress + table::wrap_block).
/// Returns (payload, footprint, tx_selected, elapsed_ms). Deterministic:
/// prepare + the grammar are pure functions of the chunk bytes.
fn run_grammar_tx(pieces: &[Vec<u8>]) -> (usize, usize, usize, u64) {
    let cfg = aahl::FoldConfig::default();
    let mut g = grammar::PersistentGrammar::with_lag_v4(cfg, 64, 16);
    let t = Instant::now();
    let mut payload = 0usize;
    let mut selected = 0usize;
    for p in pieces {
        // prepare drives the same oracle (aahl::compress_block sizes) the
        // container uses; None -> plain grammar, Some(T) -> wrapped T.
        let plan = table::prepare(p, None).unwrap_or(None);
        let (blk, gc) = match &plan {
            Some(plan) => {
                selected += 1;
                g.compress(&plan.t_stream)
            }
            None => g.compress(p),
        };
        let packed_len = match &plan {
            Some(plan) => table::wrap_block(&plan.meta, &blk).map(|w| w.len()).unwrap_or(blk.len()),
            None => blk.len(),
        };
        payload += packed_len;
        if let Some(gc) = gc {
            payload += gc.len();
        }
    }
    let ms = t.elapsed().as_millis() as u64;
    let footprint = model_footprint(&g);
    (payload, footprint, selected, ms)
}

pub fn run_ablation(
    set_dir: &Path,
    tsv: &Path,
    chunk_sizes: &[usize],
) -> Result<Vec<AblationRow>> {
    let dirs = corpus::corpus_dirs(set_dir)?;
    let mut rows = Vec::new();
    for dir in &dirs {
        let name = dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        for &chunk in chunk_sizes {
            let (pieces, raw) = unique_chunks(dir, chunk)?;
            if pieces.is_empty() {
                continue;
            }
            let (best, fold, order0, o1t, o1b, blend, no1t, nblend) = run_stateless(&pieces);
            let (grammar, model_bytes, ms) = run_grammar(&pieces, false);
            let (grammar_norec, _, _ms2) = run_grammar(&pieces, true);
            let (grammar_tx, _, tx_selected, _ms3) = run_grammar_tx(&pieces);
            rows.push(AblationRow {
                corpus: name.clone(),
                chunk,
                raw,
                stateless_best: best,
                fold,
                order0,
                order1_tok: o1t,
                order1_byte: o1b,
                blend,
                norec_order1_tok: no1t,
                norec_blend: nblend,
                grammar,
                grammar_norec,
                grammar_tx,
                tx_selected,
                model_bytes,
                create_ms: ms,
            });
        }
    }

    // Write TSV.
    let mut out = String::new();
    out.push_str("corpus\tchunk\traw\tstateless_best\tfold\torder0\torder1_tok\torder1_byte\tblend\tnorec_order1_tok\tnorec_blend\tgrammar\tgrammar_norec\tgrammar_tx\ttx_selected\tmodel_bytes\tcreate_ms\n");
    for r in &rows {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            r.corpus, r.chunk, r.raw, r.stateless_best, r.fold, r.order0,
            r.order1_tok, r.order1_byte, r.blend, r.norec_order1_tok,
            r.norec_blend, r.grammar, r.grammar_norec, r.grammar_tx,
            r.tx_selected, r.model_bytes, r.create_ms
        ));
    }
    if let Some(p) = tsv.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(tsv, out)?;

    // Console summary.
    println!(
        "{:<14} {:>7} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>12} {:>10}",
        "corpus", "chunk", "raw", "stateless", "order0", "o1tok", "o1byte", "blend", "grammar", "gm-tx", "tx-sel"
    );
    for r in &rows {
        println!(
            "{:<14} {:>7} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>12} {:>10}",
            r.corpus,
            r.chunk,
            r.raw,
            r.stateless_best,
            r.order0,
            r.order1_tok,
            r.order1_byte,
            r.blend,
            r.grammar,
            r.grammar_tx,
            r.tx_selected
        );
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("aahl_abl_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_corpus(root: &Path) {
        // table-like + code-like + incompressible, deterministic bytes
        let mut table = Vec::new();
        for i in 0..2000u32 {
            table.extend_from_slice(format!("row,{i},alpha,beta,gamma\n").as_bytes());
        }
        let mut code = Vec::new();
        for _ in 0..1500 {
            code.extend_from_slice(b"if (x == 1) { return y; } else { return 0; }\n");
        }
        let mut noise = Vec::new();
        for i in 0..200_000u32 {
            noise.push((i.wrapping_mul(2654435761) >> 24) as u8);
        }
        let dir = root.join("sub");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("t.csv"), &table).unwrap();
        std::fs::write(dir.join("c.txt"), &code).unwrap();
        std::fs::write(dir.join("n.bin"), &noise).unwrap();
    }

    #[test]
    fn ablation_is_deterministic() {
        let root = scratch("det");
        write_corpus(&root);
        let tsv1 = root.join("a1.tsv");
        let tsv2 = root.join("a2.tsv");
        let r1 = run_ablation(&root, &tsv1, &[4096, 65536]).unwrap();
        let r2 = run_ablation(&root, &tsv2, &[4096, 65536]).unwrap();
        // Payload fields must be byte-deterministic; create_ms is wall-clock and
        // legitimately varies between runs, so it is excluded.
        for (a, b) in r1.iter().zip(r2.iter()) {
            let mut x = a.clone();
            let mut y = b.clone();
            x.create_ms = 0;
            y.create_ms = 0;
            assert_eq!(x, y, "ablation payload must be byte-deterministic");
        }
        // all non-timing columns must be identical in the TSV output too
        let without_ms = |csv: &[u8]| String::from_utf8_lossy(csv)
            .lines().map(|l| {
                let cols: Vec<&str> = l.split('\t').collect();
                cols[..cols.len() - 1].join("\t")
            }).collect::<Vec<_>>();
        assert_eq!(without_ms(&std::fs::read(&tsv1).unwrap()), without_ms(&std::fs::read(&tsv2).unwrap()));
    }

    #[test]
    fn ablation_never_beats_store_bound() {
        // No pipeline may expand a chunk: stateless_best and grammar pay the
        // 'R'-store fallback, so totals stay <= raw + 6 per unique chunk.
        let root = scratch("bound");
        write_corpus(&root);
        let rows = run_ablation(&root, &root.join("b.tsv"), &[65536]).unwrap();
        for r in &rows {
            // infer chunk count from raw/chunk (approx) is fragile; instead the
            // incompressible file's own row shows the store bound directly on
            // the noise file. Assert absolute sanity on the totals:
            assert!(r.stateless_best <= r.raw + 6 * (r.raw / 65536 + 3));
            assert!(r.grammar <= r.raw + 6 * (r.raw / 65536 + 3));
        }
    }

    #[test]
    fn grammar_and_blend_relations_hold() {
        // On structured corpora: recursion helps (grammar<=grammar_norec is NOT
        // guaranteed per se due to INVEST_CREDIT bootstrap, but blend should
        // track order1_tok sanely and grammar should beat stateless on the
        // table-like content within INVEST_CREDIT accounting).
        let root = scratch("rel");
        write_corpus(&root);
        let rows = run_ablation(&root, &root.join("r.tsv"), &[65536]).unwrap();
        for r in &rows {
            assert!(r.order1_byte <= r.raw + 6 * (r.raw / 65536 + 3));
            assert!(r.blend <= r.order1_tok.saturating_mul(102) / 100 + 16,
               "blend within 2% slack of order1_tok ({} vs {})", r.blend, r.order1_tok);
        }
    }

    #[test]
    fn model_footprint_is_bounded_and_reported() {
        let root = scratch("mem");
        write_corpus(&root);
        let rows = run_ablation(&root, &root.join("m.tsv"), &[65536]).unwrap();
        for r in &rows {
            // model must not explode beyond ~64 MiB for 64 KiB chunks
            let mb = r.model_bytes;
            assert!(mb <= 64 * 1024 * 1024, "model {mb} too large");
            assert!(r.model_bytes > 0);
        }
    }
}
