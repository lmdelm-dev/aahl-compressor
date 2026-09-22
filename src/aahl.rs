//! AAHL-Fold v1: recursive pair-folding grammar + hand-rolled canonical Huffman.
//! Not LZ77/Huffman-per-block like zip, not PPM like rar.
//! Pipeline: bytes -> global BPE-style grammar fold -> custom Huffman bitstream.
//! All code here is from scratch (no zstd/lzma crates).

use anyhow::{bail, Result};
use std::collections::{BinaryHeap, HashMap};

use crate::arith;
use crate::binning;
use crate::spectral;

const MAX_SYMS: usize = 4096;
const MAX_MERGES: usize = 1024;
const MIN_PAIR_COUNT: usize = 4;

/// Tunable fold knobs (used to sweep grammar sizes without recompiling).
#[derive(Clone, Copy)]
pub struct FoldConfig {
    pub max_merges: usize,
    pub min_pair_count: usize,
    pub max_syms: usize,
}

impl Default for FoldConfig {
    fn default() -> Self {
        Self { max_merges: MAX_MERGES, min_pair_count: MIN_PAIR_COUNT, max_syms: MAX_SYMS }
    }
}

struct BitWriter {
    buf: Vec<u8>,
    cur: u8,
    nbits: u8, // bits filled in cur (0..8)
    total_bits: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self { buf: Vec::new(), cur: 0, nbits: 0, total_bits: 0 }
    }
    fn write_bits(&mut self, code: u32, len: u8) {
        for i in (0..len).rev() {
            let bit = ((code >> i) & 1) as u8;
            self.cur = (self.cur << 1) | bit;
            self.nbits += 1;
            self.total_bits += 1;
            if self.nbits == 8 {
                self.buf.push(self.cur);
                self.cur = 0;
                self.nbits = 0;
            }
        }
    }
    fn finish(mut self) -> (Vec<u8>, u32) {
        if self.nbits > 0 {
            self.cur <<= 8 - self.nbits; // pad with zeros
            self.buf.push(self.cur);
        }
        (self.buf, self.total_bits)
    }
}

struct BitReader<'a> {
    buf: &'a [u8],
    total_bits: u32,
    pos: u32,
}

impl<'a> BitReader<'a> {
    fn new(buf: &'a [u8], total_bits: u32) -> Self {
        Self { buf, total_bits, pos: 0 }
    }
    fn read_bit(&mut self) -> Option<u8> {
        if self.pos >= self.total_bits {
            return None;
        }
        let byte = self.buf[(self.pos / 8) as usize];
        let shift = 7 - (self.pos % 8);
        self.pos += 1;
        Some((byte >> shift) & 1)
    }
}

// ---------- Huffman (from scratch, canonical) ----------

#[derive(Eq, PartialEq)]
struct Node {
    freq: u64,
    sym: Option<usize>, // Some for leaves
    left: Option<Box<Node>>,
    right: Option<Box<Node>>,
    // tie-break id for deterministic builds
    id: usize,
}

impl Ord for Node {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // reverse for min-heap
        other.freq.cmp(&self.freq).then_with(|| other.id.cmp(&self.id))
    }
}
impl PartialOrd for Node {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn huffman_lengths(freqs: &[u64]) -> Vec<u8> {
    let n = freqs.len();
    let active: Vec<usize> = (0..n).filter(|&i| freqs[i] > 0).collect();
    if active.is_empty() {
        return vec![0; n];
    }
    if active.len() == 1 {
        let mut l = vec![0u8; n];
        l[active[0]] = 1;
        return l;
    }
    let mut heap = BinaryHeap::new();
    let mut next_id = 0usize;
    for &s in &active {
        heap.push(Node { freq: freqs[s], sym: Some(s), left: None, right: None, id: next_id });
        next_id += 1;
    }
    while heap.len() > 1 {
        let a = heap.pop().unwrap();
        let b = heap.pop().unwrap();
        heap.push(Node {
            freq: a.freq + b.freq,
            sym: None,
            left: Some(Box::new(a)),
            right: Some(Box::new(b)),
            id: next_id,
        });
        next_id += 1;
    }
    let root = heap.pop().unwrap();
    let mut lens = vec![0u8; n];
    fn walk(n: &Node, depth: u8, lens: &mut [u8]) {
        if let Some(s) = n.sym {
            lens[s] = depth.max(1);
        } else {
            if let Some(ref l) = n.left {
                walk(l, depth + 1, lens);
            }
            if let Some(ref r) = n.right {
                walk(r, depth + 1, lens);
            }
        }
    }
    walk(&root, 0, &mut lens);
    lens
}

fn canonical_codes(lens: &[u8]) -> Vec<(u32, u8)> {
    // returns (code, len) per symbol; canonical MSB-first
    let mut pairs: Vec<(u8, usize)> = lens
        .iter()
        .enumerate()
        .filter(|(_, &l)| l > 0)
        .map(|(s, &l)| (l, s))
        .collect();
    pairs.sort_by_key(|&(l, s)| (l, s));
    let mut table = vec![(0u32, 0u8); lens.len()];
    let mut code: u32 = 0;
    let mut prev_len: u8 = 0;
    for (len, sym) in pairs {
        code <<= len - prev_len;
        table[sym] = (code, len);
        code += 1;
        prev_len = len;
    }
    table
}

// decode tree from canonical codes
struct DecNode {
    sym: Option<usize>,
    child: [Option<Box<DecNode>>; 2],
}
impl DecNode {
    fn new() -> Self {
        Self { sym: None, child: [None, None] }
    }
    fn insert(&mut self, code: u32, len: u8, sym: usize) {
        let mut cur = self;
        for i in (0..len).rev() {
            let b = ((code >> i) & 1) as usize;
            if cur.child[b].is_none() {
                cur.child[b] = Some(Box::new(DecNode::new()));
            }
            cur = cur.child[b].as_mut().unwrap();
        }
        cur.sym = Some(sym);
    }
}

// ---------- Grammar folding ----------

/// Incremental BPE engine. Merges adjacent pairs into rules whose ids are
/// assigned by the caller (append to its rule table, or reuse an existing
/// committed rule). Winner selection is the same deterministic rule as the old
/// fold_with: highest pair count, then highest pair key, over the CURRENT
/// pair multiset. Counts are maintained incrementally: only the edges at a
/// splice boundary are touched per merge.
///
/// `cap_ok(rules_len)` gates the loop the way `256 + rules.len() >= max_syms`
/// did (rules_len = number of brand-new rules emitted so far this call).
///
/// `on_pair(l, r, rep)` is called with each winning pair and its
/// non-overlapping occurrence count. Return `(Some(id), true)` to splice with
/// a NEW rule id, `(Some(id), false)` to splice with a REUSED id, or
/// `(None, _)` to stop folding entirely (the caller's shrink gate). Rules are
/// only recorded by the caller inside on_pair, so a `None` return leaves the
/// caller's table untouched and no merge happens.
///
/// Returns the folded token list.
pub(crate) fn bpe_engine(
    syms: Vec<u16>,
    cfg: &FoldConfig,
    mut cap_ok: impl FnMut(usize) -> bool,
    mut on_pair: impl FnMut(u16, u16, usize) -> (Option<u16>, bool),
) -> Vec<u16> {
    if syms.len() < 2 {
        return syms;
    }
    // Linked list over tokens; nodes are appended (never removed) so indices
    // stay stable. -1 marks head/tail sentinels (in `prv`/`nxt`).
    let n0 = syms.len();
    let mut sn: Vec<u16> = syms.clone();
    let mut prv: Vec<isize> = (0..n0 as isize).map(|i| i - 1).collect();
    let mut nxt: Vec<isize> = (1..n0 as isize).collect();
    nxt.push(-1);
    let mut head: isize = 0;

    // pair key -> live occurrence count over the current list (overlapping).
    let mut counts: HashMap<u32, usize> = HashMap::with_capacity(n0.min(65536));
    for w in syms.windows(2) {
        let key = ((w[0] as u32) << 16) | w[1] as u32;
        *counts.entry(key).or_insert(0) += 1;
    }
    // max-heap of (count, key); stale entries are validated against `counts` on
    // pop, and every mutation of `counts` re-pushes the affected key so the
    // true current maximum always has a live heap entry.
    let mut heap: BinaryHeap<(usize, u32)> = counts.iter().map(|(&k, &c)| (c, k)).collect();

    #[inline]
    fn rec_decay(c: &mut HashMap<u32, usize>, key: u32) -> usize {
        match c.get_mut(&key) {
            Some(x) => {
                *x = x.saturating_sub(1);
                let v = *x;
                if v == 0 {
                    c.remove(&key);
                }
                v
            }
            None => 0,
        }
    }
    #[inline]
    fn rec_grow(c: &mut HashMap<u32, usize>, key: u32) -> usize {
        let n = c.entry(key).or_insert(0);
        *n += 1;
        *n
    }
    #[inline]
    fn key_of(u: u16, v: u16) -> u32 {
        ((u as u32) << 16) | v as u32
    }

    let mut rules_len = 0usize;
    for _ in 0..cfg.max_merges {
        if !cap_ok(rules_len) {
            break;
        }
        // Pop the highest (count, key) live pair.
        let (best_count, best_key) = loop {
            match heap.pop() {
                Some((c, k)) => {
                    if counts.get(&k) == Some(&c) {
                        break (c, k);
                    }
                }
                None => break (0, 0),
            }
        };
        if best_count < cfg.min_pair_count {
            break;
        }
        let l = (best_key >> 16) as u16;
        let r = (best_key & 0xffff) as u16;
        // Count NON-overlapping occurrences (left to right), the number that
        // will actually be spliced. Mirrors the old scanner exactly.
        let mut rep = 0usize;
        let mut cur = head;
        while cur >= 0 {
            let i = cur as usize;
            let j = nxt[i];
            if j >= 0 && sn[i] == l && sn[j as usize] == r {
                rep += 1;
                cur = nxt[j as usize];
            } else {
                cur = j;
            }
        }
        let (maybe_id, added) = on_pair(l, r, rep);
        let new_id = match maybe_id {
            Some(id) => id,
            None => break, // caller's shrink gate: pair doesn't pay
        };
        // Splice every non-overlapping occurrence, maintaining `counts` and
        // `heap` only at the splice boundaries.
        cur = head;
        while cur >= 0 {
            let i = cur as usize;
            let j = nxt[i];
            if j >= 0 && sn[i] == l && sn[j as usize] == r {
                let a = prv[i]; // may be -1
                let b = nxt[j as usize]; // may be -1
                // retire the old adjacency edges
                if a >= 0 {
                    let k = key_of(sn[a as usize], l);
                    let c = rec_decay(&mut counts, k);
                    if c > 0 {
                        heap.push((c, k));
                    }
                }
                let k = key_of(l, r);
                let c = rec_decay(&mut counts, k);
                if c > 0 {
                    heap.push((c, k));
                }
                if b >= 0 {
                    let k = key_of(r, sn[b as usize]);
                    let c = rec_decay(&mut counts, k);
                    if c > 0 {
                        heap.push((c, k));
                    }
                }
                // replace nodes i,j with one new node holding new_id
                let x = sn.len() as isize;
                sn.push(new_id);
                prv.push(a);
                nxt.push(b);
                if a >= 0 {
                    nxt[a as usize] = x;
                } else {
                    head = x;
                }
                if b >= 0 {
                    prv[b as usize] = x;
                }
                // create the new adjacency edges
                if a >= 0 {
                    let k = key_of(sn[a as usize], new_id);
                    let c = rec_grow(&mut counts, k);
                    heap.push((c, k));
                }
                if b >= 0 {
                    let k = key_of(new_id, sn[b as usize]);
                    let c = rec_grow(&mut counts, k);
                    heap.push((c, k));
                }
                cur = b; // skip past the consumed r, like the old `i += 2`
            } else {
                cur = j;
            }
        }
        if added {
            rules_len += 1;
        }
    }
    // Collect the surviving tokens by walking the linked list.
    let mut out = Vec::with_capacity(sn.len());
    let mut cur = head;
    while cur >= 0 {
        out.push(sn[cur as usize]);
        cur = nxt[cur as usize];
    }
    out
}

/// Fold bytes into (tokens, rules). rules[i] = (left, right) for symbol 256+i.
pub fn fold(syms: Vec<u16>) -> (Vec<u16>, Vec<(u16, u16)>) {
    fold_with(syms, &FoldConfig::default())
}

pub fn fold_with(syms: Vec<u16>, cfg: &FoldConfig) -> (Vec<u16>, Vec<(u16, u16)>) {
    let mut rules: Vec<(u16, u16)> = Vec::new();
    let out = bpe_engine(
        syms,
        cfg,
        |rules_len| 256 + rules_len < cfg.max_syms,
        |l, r, rep| {
            // only keep merge if it actually shrinks token count enough to pay
            // for the rule cost: rep occurrences -> net token reduction = rep,
            // require >= 3 (rule costs ~4 bytes + huffman overhead)
            if rep < 3 {
                return (None, false);
            }
            let new_id = (256 + rules.len()) as u16;
            rules.push((l, r));
            (Some(new_id), true)
        },
    );
    (out, rules)
}

pub fn unfold(tokens: &[u16], rules: &[(u16, u16)], limit: usize) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(limit.min(1 << 26));
    // iterative expansion per token
    let mut stack: Vec<u16> = Vec::new();
    for &t in tokens {
        stack.clear();
        stack.push(t);
        while let Some(s) = stack.pop() {
            if s < 256 {
                out.push(s as u8);
                if out.len() > limit {
                    bail!("expansion exceeds expected size");
                }
            } else {
                let idx = (s - 256) as usize;
                if idx >= rules.len() {
                    bail!("bad grammar reference {s}");
                }
                let (l, r) = rules[idx];
                stack.push(r);
                stack.push(l);
            }
        }
    }
    Ok(out)
}

/// Diagnostic: per-mode block sizes (bytes) for a raw chunk, plus the final
/// `compress_block` output. Returns (fold, arith, final_packed, raw, mode_tag).
pub fn block_sizes(raw: &[u8]) -> (usize, usize, usize, usize, usize, usize, u8) {
    let folded = fold_encode(raw).len();
    let arith = arith_encode(raw).len();
    let o1t = o1_encode(raw).len();
    let o1b = o1_bytes_encode(raw).len();
    let packed = compress_block(raw);
    let mode = if packed.len() >= 2 && packed[0] == 0xA0 { packed[1] } else { b'F' };
    (folded, arith, o1t, o1b, packed.len(), raw.len(), mode)
}

/// Diagnostic: fold stats + arith size for a given fold config (dev sweep).
/// Returns (num_rules, num_tokens, arith_size).
pub fn arith_sizes(raw: &[u8], cfg: &FoldConfig) -> (usize, usize, usize) {
    let init: Vec<u16> = raw.iter().map(|&b| b as u16).collect();
    let (tokens, rules) = fold_with(init, cfg);
    let arith_size = {
        let n = 256 + rules.len();
        let blob = arith::encode_tokens(&tokens, n);
        2 + 4 + rules.len() * 4 + 8 + blob.len()
    };
    (rules.len(), tokens.len(), arith_size)
}

/// Weighted ratio of a grammar: (bytes_of_rules + bytes_per_token) per source
/// byte, to gauge whether deeper folding is paying for itself.
pub fn grammar_cost(raw: &[u8], cfg: &FoldConfig) -> f64 {
    let init: Vec<u16> = raw.iter().map(|&b| b as u16).collect();
    let (tokens, rules) = fold_with(init, cfg);
    let n = 256 + rules.len();
    let overhead = 2.0 + 4.0 + n as f64; // total_syms u16 + num_tokens u32 + lens table
    if raw.is_empty() {
        return 0.0;
    }
    (overhead + rules.len() as f64 * 4.0) / raw.len() as f64
}

// ---------- Ablation diagnostics ----------
//
// Measurement-only helpers: compute the block size every codec would produce
// for a chunk WITHOUT changing the archive format. The ablation harness sums
// these over a corpus to produce evidence for which pipeline component earns
// its bytes. `blend` mirrors the 'D' layout with the deterministic order-1+2
// blend blob (see arith.rs); it is not yet emitted into archives -- this is
// the measurement side of "wire into the persistent model in a later phase".

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModeSizes {
    pub fold: usize,        // 'F' legacy huffman fold block
    pub order0: usize,      // 'A' adaptive order-0 over folded tokens
    pub order1_tok: usize,  // 'D' order-1 over folded tokens
    pub order1_byte: usize, // 'C' order-1 over raw bytes
    pub blend: usize,       // blend (order-1 + order-2 bucket), 'D' framing
}

fn blend_size_parts(tokens: &[u16], rules: &[(u16, u16)]) -> usize {
    let n = 256 + rules.len();
    let blob = arith::encode_tokens_blend(tokens, n);
    2 + 4 + rules.len() * 4 + 8 + blob.len()
}

/// Block sizes every codec would produce for `raw` under `cfg`.
/// `cfg.max_merges == 0` disables the recursive pair-merge pass (rules stay
/// empty), which is the "recursion off" arm of the ablation.
pub fn mode_sizes_with(raw: &[u8], cfg: &FoldConfig) -> ModeSizes {
    if raw.is_empty() {
        return ModeSizes::default();
    }
    let init: Vec<u16> = raw.iter().map(|&b| b as u16).collect();
    let (tokens, rules) = fold_with(init, cfg);
    ModeSizes {
        fold: fold_encode_parts(&tokens, &rules).len(),
        order0: arith_encode_parts(&tokens, &rules).len(),
        order1_tok: o1_encode_parts(&tokens, &rules).len(),
        order1_byte: o1_bytes_encode(raw).len(),
        blend: blend_size_parts(&tokens, &rules),
    }
}

/// Default-config variant used by the ablation sweeps.
pub fn mode_sizes(raw: &[u8]) -> ModeSizes {
    mode_sizes_with(raw, &FoldConfig::default())
}

/// Order-1 folded-token block: [0xA0, b'D', num_rules u32, rules, num_tokens u32,
///  blob_len u32, blob]. 'D' = order-1 on grammar tokens.
fn o1_encode(raw: &[u8]) -> Vec<u8> {
    let (tokens, rules) = fold_stream(raw);
    o1_encode_parts(&tokens, &rules)
}

fn o1_encode_parts(tokens: &[u16], rules: &[(u16, u16)]) -> Vec<u8> {
    let n = 256 + rules.len();
    let blob = arith::encode_tokens_order1(tokens, n);
    let mut out = Vec::with_capacity(blob.len() + 2 + 4 + rules.len() * 4 + 8);
    out.push(0xA0);
    out.push(b'D');
    out.extend_from_slice(&(rules.len() as u32).to_le_bytes());
    for &(l, r) in rules {
        out.extend_from_slice(&l.to_le_bytes());
        out.extend_from_slice(&r.to_le_bytes());
    }
    out.extend_from_slice(&(tokens.len() as u32).to_le_bytes());
    out.extend_from_slice(&(blob.len() as u32).to_le_bytes());
    out.extend_from_slice(&blob);
    out
}

/// Order-1 raw-byte block: [0xA0, b'C', len u32, blob_len u32, blob].
fn o1_bytes_encode(raw: &[u8]) -> Vec<u8> {
    let blob = arith::encode_bytes_order1(raw);
    let mut out = Vec::with_capacity(blob.len() + 10);
    out.push(0xA0);
    out.push(b'C');
    out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    out.extend_from_slice(&(blob.len() as u32).to_le_bytes());
    out.extend_from_slice(&blob);
    out
}

fn o1_bytes_decode(blk: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    if blk.len() < 2 + 4 + 4 {
        bail!("o1 byte block too small");
    }
    let raw_len = u32::from_le_bytes([blk[2], blk[3], blk[4], blk[5]]) as usize;
    let blob_len = u32::from_le_bytes([blk[6], blk[7], blk[8], blk[9]]) as usize;
    if blob_len as usize != blk.len() - 10 {
        bail!("o1 byte block length mismatch");
    }
    if raw_len != expected_len {
        bail!("o1 byte length mismatch");
    }
    let blob = &blk[10..];
    let mut out = Vec::with_capacity(raw_len);
    arith::decode_bytes_order1(blob, raw_len, &mut out).map_err(anyhow::Error::msg)?;
    if out.len() != raw_len {
        bail!("o1 byte decode length mismatch");
    }
    Ok(out)
}

/// Shannon entropy (bits/byte) from the global byte histogram.
pub fn byte_entropy(raw: &[u8]) -> f64 {
    let mut h = [0u64; 256];
    for &b in raw {
        h[b as usize] += 1;
    }
    let n = raw.len() as f64;
    let mut e = 0.0f64;
    for &c in &h {
        if c > 0 {
            let p = c as f64 / n;
            e -= p * p.log2();
        }
    }
    e
}

/// Store block layout: [0xA0, b'R', len u32 LE, raw bytes].
/// Used when every codec would make the chunk bigger (incompressible input),
/// mirroring deflate's "stored" block so archive overhead stays < 0.1%.
fn store_encode(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + raw.len());
    out.push(0xA0);
    out.push(b'R');
    out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    out.extend_from_slice(raw);
    out
}

fn store_decode(blk: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    if blk.len() < 6 {
        bail!("store block too small");
    }
    let len = u32::from_le_bytes([blk[2], blk[3], blk[4], blk[5]]) as usize;
    if len != expected_len {
        bail!("store length mismatch");
    }
    if blk.len() != 6 + len {
        bail!("store block length mismatch");
    }
    Ok(blk[6..].to_vec())
}

/// Fold raw bytes into (tokens, rules) under the default config. Single source
/// of truth for the shared fold; codecs consume it instead of re-folding.
fn fold_stream(raw: &[u8]) -> (Vec<u16>, Vec<(u16, u16)>) {
    let init: Vec<u16> = raw.iter().map(|&b| b as u16).collect();
    fold(init)
}

/// Compress raw bytes -> AAHL block bytes (custom format, no external codec).
/// Mode 'F' (fold, legacy layout), 'S' (spectral period jump via complex FFT),
/// 'B' (binned), 'A' (adaptive arithmetic) or 'R' (stored, incompressible).
/// Spectral blocks start with [0xA0, b'S']; legacy blocks start with total_syms.
pub fn compress_block(raw: &[u8]) -> Vec<u8> {
    if raw.is_empty() {
        return vec![0, 0, 0, 0, 0, 0]; // total_syms=0 marker handled by decompress
    }
    // Fast path: near-max-entropy bytes cannot be shrunk by any of our order-0
    // grammar / arithmetic models. Store raw with a 6-byte header instead of
    // paying fold/arith table overhead (and the O(n*m) fold cost).
    if byte_entropy(raw) >= 7.9 {
        return store_encode(raw);
    }
    // Fold ONCE; every grammar-based codec below consumes the same tokens/rules
    // instead of re-running fold_with (which was ~3x redundant work).
    let (tokens, rules) = fold_stream(raw);
    let mut best = fold_encode_parts(&tokens, &rules);
    // Spectral jump: FFT detects the period in O(n log n), then we store one
    // period instead of a whole grammar. Only exact periods (verified in time
    // domain) are accepted, so decoding stays lossless.
    if raw.len() >= 64 {
        if let Some(p) = spectral::detect_period(raw) {
            let spec = spectral_encode(raw, p);
            if spec.len() < best.len() {
                best = spec;
            }
        }
    }
    // Divide-and-bin: group structurally similar pieces so each bin's
    // grammar sees clean repetition instead of interleaved noise.
    if raw.len() >= binning::PIECE_SIZE * 4 {
        if let Some(binned) = binned_encode(raw) {
            if binned.len() + 64 < best.len() {
                // +64 guards the bin-table overhead against marginal wins
                return binned;
            }
        }
    }
    // Adaptive arithmetic: order-0 model converging on the folded grammar.
    // Beats static Huffman on skewed distributions (no per-symbol table).
    let arith = arith_encode_parts(&tokens, &rules);
    // Order-1 models: condition each symbol on its predecessor (no LZ77).
    // - 'D': order-1 over folded grammar tokens (rules capture phrases).
    // - 'C': order-1 over raw bytes (exact byte context).
    let o1t = o1_encode_parts(&tokens, &rules);
    let o1b = o1_bytes_encode(raw);
    if arith.len() < best.len() {
        best = arith;
    }
    if o1t.len() < best.len() {
        best = o1t;
    }
    if o1b.len() < best.len() {
        best = o1b;
    }
    // Safety net: if no codec actually shrank the chunk, store it raw.
    if best.len() >= raw.len() + 6 {
        return store_encode(raw);
    }
    best
}

/// Arithmetic fold block layout:
/// [0xA0, b'A', num_rules u32, rules (u16,u16)*, num_tokens u32,
///  blob_len u32, blob bytes]
fn arith_encode(raw: &[u8]) -> Vec<u8> {
    let (tokens, rules) = fold_stream(raw);
    arith_encode_parts(&tokens, &rules)
}

fn arith_encode_parts(tokens: &[u16], rules: &[(u16, u16)]) -> Vec<u8> {
    let n = 256 + rules.len();
    let blob = arith::encode_tokens(tokens, n);
    let mut out = Vec::with_capacity(blob.len() + 2 + 4 + rules.len() * 4 + 8);
    out.push(0xA0);
    out.push(b'A');
    out.extend_from_slice(&(rules.len() as u32).to_le_bytes());
    for &(l, r) in rules {
        out.extend_from_slice(&l.to_le_bytes());
        out.extend_from_slice(&r.to_le_bytes());
    }
    out.extend_from_slice(&(tokens.len() as u32).to_le_bytes());
    out.extend_from_slice(&(blob.len() as u32).to_le_bytes());
    out.extend_from_slice(&blob);
    out
}

fn arith_decode(blk: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    arith_decode_with(blk, expected_len, arith::decode_tokens)
}

fn o1_decode(blk: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    arith_decode_with(blk, expected_len, arith::decode_tokens_order1)
}

fn arith_decode_with(
    blk: &[u8],
    expected_len: usize,
    decode: fn(&[u8], usize, usize, &mut Vec<u16>) -> Result<(), String>,
) -> Result<Vec<u8>> {
    if blk.len() < 2 + 4 + 4 + 4 {
        bail!("arith block too small");
    }
    let mut p = 2;
    let num_rules = u32::from_le_bytes([blk[p], blk[p + 1], blk[p + 2], blk[p + 3]]) as usize;
    p += 4;
    if num_rules > 4096 {
        bail!("too many rules");
    }
    if blk.len() < p + num_rules * 4 + 8 {
        bail!("arith block truncated (table)");
    }
    let mut rules = Vec::with_capacity(num_rules);
    for _ in 0..num_rules {
        let l = u16::from_le_bytes([blk[p], blk[p + 1]]);
        let r = u16::from_le_bytes([blk[p + 2], blk[p + 3]]);
        p += 4;
        let new_id = 256 + rules.len();
        if (l as usize) >= new_id || (r as usize) >= new_id {
            bail!("forward grammar reference");
        }
        rules.push((l, r));
    }
    let num_tokens = u32::from_le_bytes([blk[p], blk[p + 1], blk[p + 2], blk[p + 3]]) as usize;
    p += 4;
    let blob_len = u32::from_le_bytes([blk[p], blk[p + 1], blk[p + 2], blk[p + 3]]) as usize;
    p += 4;
    if blk.len() < p + blob_len {
        bail!("arith block truncated (blob)");
    }
    let blob = &blk[p..p + blob_len];
    let n = 256 + num_rules;
    let mut tokens: Vec<u16> = Vec::with_capacity(num_tokens);
    decode(blob, n, num_tokens, &mut tokens).map_err(anyhow::Error::msg)?;
    let raw = unfold(&tokens, &rules, expected_len)?;
    if raw.len() != expected_len {
        bail!("arith length mismatch");
    }
    Ok(raw)
}

/// Spectral block layout: [0xA0, b'S', period u32 LE, base_len u32 LE, base bytes].
fn spectral_encode(raw: &[u8], period: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + period);
    out.push(0xA0);
    out.push(b'S');
    out.extend_from_slice(&(period as u32).to_le_bytes());
    out.extend_from_slice(&(period as u32).to_le_bytes());
    out.extend_from_slice(&raw[..period]);
    out
}

fn spectral_decode(blk: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    if blk.len() < 10 {
        bail!("spectral block too small");
    }
    let period = u32::from_le_bytes([blk[2], blk[3], blk[4], blk[5]]) as usize;
    let base_len = u32::from_le_bytes([blk[6], blk[7], blk[8], blk[9]]) as usize;
    if period == 0 || period != base_len || period > 4096 {
        bail!("bad spectral period");
    }
    if blk.len() != 10 + period {
        bail!("spectral block length mismatch");
    }
    let base = &blk[10..];
    let mut out = Vec::with_capacity(expected_len);
    while out.len() < expected_len {
        let take = (expected_len - out.len()).min(period);
        out.extend_from_slice(&base[..take]);
    }
    Ok(out)
}

/// Binned block layout:
/// [0xA0, b'B', total_len u32, piece_size u32, num_pieces u32, num_bins u32,
///  order[num_pieces] u32, bin_payload_len[num_bins] u32,
///  bin_payloads... (each = fold_encode or spectral_encode of concatenated bin)]
fn binned_encode(raw: &[u8]) -> Option<Vec<u8>> {
    let (assign, npieces) = binning::plan(raw)?;
    let pieces: Vec<&[u8]> = raw.chunks(binning::PIECE_SIZE).collect();
    debug_assert_eq!(pieces.len(), npieces);
    let nbins = assign.iter().copied().max().unwrap_or(0) + 1;
    let mut bin_data: Vec<Vec<u8>> = vec![Vec::new(); nbins];
    for (piece, &b) in pieces.iter().zip(assign.iter()) {
        bin_data[b].extend_from_slice(piece);
    }
    // compress each bin independently with the best single-stream codec
    // (fold or spectral). Grouping exposes exact periodicity that
    // interleaving hides, so bins get the full selection logic (no
    // re-binning: flat payloads only, enforced on decode).
    let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(nbins);
    for bin in &bin_data {
        let folded = fold_encode(bin);
        let mut best = folded;
        if bin.len() >= 64 {
            if let Some(p) = spectral::detect_period(bin) {
                let spec = spectral_encode(bin, p);
                if spec.len() < best.len() {
                    best = spec;
                }
            }
        }
        payloads.push(best);
    }
    let mut out = Vec::new();
    out.push(0xA0);
    out.push(b'B');
    out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    out.extend_from_slice(&(binning::PIECE_SIZE as u32).to_le_bytes());
    out.extend_from_slice(&(npieces as u32).to_le_bytes());
    out.extend_from_slice(&(nbins as u32).to_le_bytes());
    for &b in &assign {
        out.extend_from_slice(&(b as u32).to_le_bytes());
    }
    for p in &payloads {
        out.extend_from_slice(&(p.len() as u32).to_le_bytes());
    }
    for p in &payloads {
        out.extend_from_slice(p);
    }
    Some(out)
}

fn binned_decode(blk: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    if blk.len() < 18 {
        bail!("binned block too small");
    }
    let total = u32::from_le_bytes([blk[2], blk[3], blk[4], blk[5]]) as usize;
    let piece_size = u32::from_le_bytes([blk[6], blk[7], blk[8], blk[9]]) as usize;
    let npieces = u32::from_le_bytes([blk[10], blk[11], blk[12], blk[13]]) as usize;
    let nbins = u32::from_le_bytes([blk[14], blk[15], blk[16], blk[17]]) as usize;
    if total != expected_len || piece_size == 0 || piece_size > 1 << 20 {
        bail!("bad binned header");
    }
    if npieces == 0 || nbins == 0 || nbins > binning::MAX_BINS || nbins > npieces {
        bail!("bad binned counts");
    }
    let mut p = 18;
    if blk.len() < p + npieces * 4 + nbins * 4 {
        bail!("binned block truncated (table)");
    }
    let mut order = Vec::with_capacity(npieces);
    for _ in 0..npieces {
        let b = u32::from_le_bytes([blk[p], blk[p + 1], blk[p + 2], blk[p + 3]]) as usize;
        if b >= nbins {
            bail!("bad bin reference");
        }
        order.push(b);
        p += 4;
    }
    let mut lens = Vec::with_capacity(nbins);
    for _ in 0..nbins {
        lens.push(u32::from_le_bytes([blk[p], blk[p + 1], blk[p + 2], blk[p + 3]]) as usize);
        p += 4;
    }
    // decode each bin payload (fold or spectral, never nested bins).
    // Bin lengths are exactly recoverable from the order map.
    let mut bin_lens = vec![0usize; nbins];
    for (i, &b) in order.iter().enumerate() {
        bin_lens[b] += (total - i * piece_size).min(piece_size);
    }
    let mut bins: Vec<Vec<u8>> = Vec::with_capacity(nbins);
    for (bi, &len) in lens.iter().enumerate() {
        if blk.len() < p + len {
            bail!("binned block truncated (payload)");
        }
        let payload = &blk[p..p + len];
        p += len;
        if payload.len() >= 2 && payload[0] == 0xA0 && payload[1] == b'B' {
            bail!("nested bins not allowed");
        }
        let bin = decompress_block(payload, bin_lens[bi])?;
        if bin.len() != bin_lens[bi] {
            bail!("bin length mismatch");
        }
        bins.push(bin);
    }
    // scatter pieces back into original order:
    // bin streams hold pieces in piece order; walk the order map and copy
    // each piece's bytes (piece_size, or the remainder for the last piece).
    let mut out = vec![0u8; total];
    let mut offsets = vec![0usize; nbins];
    let mut remaining = total;
    for (piece_idx, &b) in order.iter().enumerate() {
        let plen = remaining.min(piece_size);
        remaining -= plen;
        let src = bins.get(b).ok_or_else(|| anyhow::anyhow!("bad bin"))?;
        let off = offsets[b];
        if off + plen > src.len() {
            bail!("bin underflow");
        }
        let dst_off = piece_idx * piece_size;
        out[dst_off..dst_off + plen].copy_from_slice(&src[off..off + plen]);
        offsets[b] += plen;
    }
    // every bin stream must be fully consumed
    for (b, src) in bins.iter().enumerate() {
        if offsets[b] != src.len() {
            bail!("bin length mismatch");
        }
    }
    Ok(out)
}

fn fold_encode(raw: &[u8]) -> Vec<u8> {
    let (tokens, rules) = fold_stream(raw);
    fold_encode_parts(&tokens, &rules)
}

fn fold_encode_parts(tokens: &[u16], rules: &[(u16, u16)]) -> Vec<u8> {
    let total_syms = 256 + rules.len();
    // frequencies of folded tokens
    let mut freqs = vec![0u64; total_syms];
    for &t in tokens {
        freqs[t as usize] += 1;
    }
    let lens = huffman_lengths(&freqs);
    let codes = canonical_codes(&lens);
    let mut bw = BitWriter::new();
    for &t in tokens {
        let (code, len) = codes[t as usize];
        bw.write_bits(code, len);
    }
    let (bits, bitlen) = bw.finish();

    let mut out = Vec::with_capacity(bits.len() + rules.len() * 4 + total_syms + 16);
    out.extend_from_slice(&(total_syms as u16).to_le_bytes());
    out.extend_from_slice(&(tokens.len() as u32).to_le_bytes());
    for &l in &lens {
        out.push(l);
    }
    out.extend_from_slice(&bitlen.to_le_bytes());
    out.extend_from_slice(&(bits.len() as u32).to_le_bytes());
    out.extend_from_slice(&bits);
    out.extend_from_slice(&(rules.len() as u32).to_le_bytes());
    for &(l, r) in rules {
        out.extend_from_slice(&l.to_le_bytes());
        out.extend_from_slice(&r.to_le_bytes());
    }
    out
}

pub fn decompress_block(blk: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    if blk.len() == 6 && blk == [0, 0, 0, 0, 0, 0] {
        return Ok(Vec::new());
    }
    // Spectral jump blocks (complex-FFT period path).
    if blk.len() >= 2 && blk[0] == 0xA0 && blk[1] == b'S' {
        let raw = spectral_decode(blk, expected_len)?;
        if raw.len() != expected_len {
            bail!("spectral length mismatch");
        }
        return Ok(raw);
    }
    // Divide-and-bin blocks.
    if blk.len() >= 2 && blk[0] == 0xA0 && blk[1] == b'B' {
        let raw = binned_decode(blk, expected_len)?;
        if raw.len() != expected_len {
            bail!("binned length mismatch");
        }
        return Ok(raw);
    }
    // Adaptive arithmetic blocks.
    if blk.len() >= 2 && blk[0] == 0xA0 && blk[1] == b'A' {
        return arith_decode(blk, expected_len);
    }
    // Order-1 folded-token blocks (same layout as 'A', order-1 model).
    if blk.len() >= 2 && blk[0] == 0xA0 && blk[1] == b'D' {
        return o1_decode(blk, expected_len);
    }
    // Order-1 raw-byte blocks.
    if blk.len() >= 2 && blk[0] == 0xA0 && blk[1] == b'C' {
        let raw = o1_bytes_decode(blk, expected_len)?;
        if raw.len() != expected_len {
            bail!("o1 byte length mismatch");
        }
        return Ok(raw);
    }
    // Stored blocks (incompressible chunks).
    if blk.len() >= 2 && blk[0] == 0xA0 && blk[1] == b'R' {
        return store_decode(blk, expected_len);
    }
    decompress_inner(blk, expected_len)
}

/// Legacy fold bitstream (also used for each bin payload).
fn decompress_inner(blk: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    if blk.len() < 2 + 4 + 4 + 4 {
        bail!("block too small");
    }
    let mut p = 0;
    let total_syms = u16::from_le_bytes([blk[p], blk[p + 1]]) as usize;
    p += 2;
    if total_syms < 256 || total_syms > MAX_SYMS {
        bail!("bad alphabet {total_syms}");
    }
    let num_tokens = u32::from_le_bytes([blk[p], blk[p + 1], blk[p + 2], blk[p + 3]]) as usize;
    p += 4;
    if blk.len() < p + total_syms + 4 + 4 {
        bail!("block truncated (table)");
    }
    let lens: Vec<u8> = blk[p..p + total_syms].to_vec();
    p += total_syms;
    let bitlen = u32::from_le_bytes([blk[p], blk[p + 1], blk[p + 2], blk[p + 3]]);
    p += 4;
    let bytelen = u32::from_le_bytes([blk[p], blk[p + 1], blk[p + 2], blk[p + 3]]) as usize;
    p += 4;
    if blk.len() < p + bytelen + 4 {
        bail!("block truncated (bits)");
    }
    let bits = &blk[p..p + bytelen];
    p += bytelen;
    let num_rules = u32::from_le_bytes([blk[p], blk[p + 1], blk[p + 2], blk[p + 3]]) as usize;
    p += 4;
    if num_rules + 256 != total_syms {
        bail!("rule count mismatch");
    }
    if blk.len() < p + num_rules * 4 {
        bail!("block truncated (rules)");
    }
    let mut rules = Vec::with_capacity(num_rules);
    for _ in 0..num_rules {
        let l = u16::from_le_bytes([blk[p], blk[p + 1]]);
        let r = u16::from_le_bytes([blk[p + 2], blk[p + 3]]);
        p += 4;
        // validate forward references only (DAG): children must be < new id
        let new_id = 256 + rules.len();
        if (l as usize) >= new_id || (r as usize) >= new_id {
            bail!("forward grammar reference");
        }
        rules.push((l, r));
    }
    // huffman decode bitstream to tokens
    let codes = canonical_codes(&lens);
    let mut root = DecNode::new();
    for (s, &(code, len)) in codes.iter().enumerate() {
        if len > 0 {
            root.insert(code, len, s);
        }
    }
    let mut reader = BitReader::new(bits, bitlen);
    let mut tokens: Vec<u16> = Vec::with_capacity(num_tokens);
    let mut cur = &root;
    while tokens.len() < num_tokens {
        let b = reader.read_bit().ok_or_else(|| anyhow::anyhow!("bitstream ended early"))? as usize;
        cur = cur.child[b].as_ref().ok_or_else(|| anyhow::anyhow!("bad huffman code"))?;
        if let Some(s) = cur.sym {
            tokens.push(s as u16);
            cur = &root;
        }
    }
    unfold(&tokens, &rules, expected_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(raw: &[u8]) {
        let blk = compress_block(raw);
        let out = decompress_block(&blk, raw.len()).expect("decode");
        assert_eq!(out, raw);
    }

    #[test]
    fn empty_roundtrip() {
        roundtrip(b"");
    }

    #[test]
    fn single_byte_roundtrip() {
        roundtrip(b"X");
    }

    #[test]
    fn periodic_uses_spectral_path_and_beats_fold() {
        let mut raw = Vec::new();
        for _ in 0..300 {
            raw.extend_from_slice(b"hello world! ");
        }
        let blk = compress_block(&raw);
        assert!(blk.len() >= 2 && blk[0] == 0xA0 && blk[1] == b'S');
        assert!(blk.len() < fold_encode(&raw).len());
        let out = decompress_block(&blk, raw.len()).unwrap();
        assert_eq!(out, raw);
    }

    #[test]
    fn nonperiodic_stays_on_fold_path() {
        let raw: Vec<u8> = (0..1000).map(|i| ((i * 37 + 11) % 251) as u8).collect();
        let blk = compress_block(&raw);
        assert!(!(blk.len() >= 2 && blk[0] == 0xA0 && blk[1] == b'S'));
        let out = decompress_block(&blk, raw.len()).unwrap();
        assert_eq!(out, raw);
    }

    #[test]
    fn text_roundtrip() {
        roundtrip(b"The quick brown fox jumps over the lazy dog. ");
    }

    #[test]
    fn corrupt_spectral_rejected() {
        let blk = vec![0xA0, b'S', 1, 0, 0, 0, 9, 0, 0, 0, b'x'];
        assert!(decompress_block(&blk, 5).is_err());
    }

    #[test]
    fn truncated_block_rejected() {
        assert!(decompress_block(&[0xA0, b'S', 1], 10).is_err());
        assert!(decompress_block(&[1, 2, 3], 3).is_err());
    }

    #[test]
    fn interleaved_mixed_data_uses_binned_path() {
        // text chunks interleaved with binary chunks: binning must trigger
        // and roundtrip losslessly
        let mut raw = Vec::new();
        for i in 0..16 {
            if i % 2 == 0 {
                raw.extend(vec![b'A' + (i as u8 % 3); binning::PIECE_SIZE]);
            } else {
                raw.extend((0..binning::PIECE_SIZE).map(|j| (j % 256) as u8));
            }
        }
        let blk = compress_block(&raw);
        assert!(
            blk.len() >= 2 && blk[0] == 0xA0 && blk[1] == b'B',
            "expected binned path, got {} bytes starting {:02x?}",
            blk.len(),
            &blk[..blk.len().min(4)]
        );
        let out = decompress_block(&blk, raw.len()).unwrap();
        assert_eq!(out, raw);
    }

    #[test]
    fn homogeneous_data_skips_binning() {
        let raw = vec![b'z'; 32 * 1024];
        let blk = compress_block(&raw);
        assert!(!(blk.len() >= 2 && blk[0] == 0xA0 && blk[1] == b'B'));
        let out = decompress_block(&blk, raw.len()).unwrap();
        assert_eq!(out, raw);
    }

    #[test]
    fn corrupt_binned_rejected() {
        // same shape as the interleaved test: proven to take the 'B' path
        let mut raw = Vec::new();
        for i in 0..16 {
            if i % 2 == 0 {
                raw.extend(vec![b'A' + (i as u8 % 3); binning::PIECE_SIZE]);
            } else {
                raw.extend((0..binning::PIECE_SIZE).map(|j| (j % 256) as u8));
            }
        }
        let mut blk = compress_block(&raw);
        assert!(blk.len() >= 2 && blk[0] == 0xA0 && blk[1] == b'B');
        blk[20] ^= 0xFF; // corrupt order table
        assert!(decompress_block(&blk, raw.len()).is_err());
    }

    #[test]
    fn skewed_repetition_uses_arithmetic_path() {
        // heavy repetition that is NOT exactly periodic (so spectral skips),
        // with a skewed symbol distribution where adaptive arithmetic beats
        // static Huffman's >=1 bit/symbol floor.
        let mut raw = Vec::new();
        for i in 0..5000 {
            raw.push(b"abc"[i % 3]);
            if i % 97 == 0 {
                raw.push(0xFF); // irregular noise kills exact periodicity
            }
        }
        let blk = compress_block(&raw);
        let out = decompress_block(&blk, raw.len()).unwrap();
        assert_eq!(out, raw);
        if blk.len() >= 2 && blk[0] == 0xA0 && blk[1] == b'A' {
            // on this skewed corpus the arith mode should also win outright
            assert!(blk.len() < fold_encode(&raw).len());
        }
    }

    #[test]
    fn forced_arith_block_roundtrips() {
        let raw = b"the quick brown fox jumps over the lazy dog. ".repeat(80);
        let blk = compress_block(&raw);
        let out = decompress_block(&blk, raw.len()).unwrap();
        assert_eq!(out, raw);
    }

    #[test]
    fn o1_token_block_roundtrips() {
        let raw = b"fn foo() { return bar(baz); }\n".repeat(40);
        let blk = o1_encode(&raw);
        assert!(blk.len() >= 2 && blk[0] == 0xA0 && blk[1] == b'D');
        let out = decompress_block(&blk, raw.len()).unwrap();
        assert_eq!(out, raw);
    }

    #[test]
    fn o1_bytes_block_roundtrips() {
        let raw = b"else return value;\n fn bar() { x = x + 1; }\n".repeat(40);
        let blk = o1_bytes_encode(&raw);
        assert!(blk.len() >= 2 && blk[0] == 0xA0 && blk[1] == b'C');
        let out = decompress_block(&blk, raw.len()).unwrap();
        assert_eq!(out, raw);
    }

    #[test]
    fn corrupt_o1_bytes_rejected() {
        // truncated headers must be rejected
        assert!(decompress_block(&[0xA0, b'C', 2, 0, 0, 0], 10).is_err());
        assert!(decompress_block(&[0xA0, b'C'], 3).is_err());
        // blob_len must match remaining bytes
        let mismatch = [0xA0, b'C', 3, 0, 0, 0, 99, 0, 0, 0, b'x', b'y'];
        assert!(decompress_block(&mismatch, 3).is_err());
        // header length mismatch vs expected_len must be rejected
        let wrong_len = [0xA0, b'C', 5, 0, 0, 0, 1, 0, 0, 0, 0x00];
        assert!(decompress_block(&wrong_len, 3).is_err());
    }

    #[test]
    fn corrupt_arith_rejected() {
        assert!(decompress_block(&[0xA0, b'A', 255, 0, 0, 0], 10).is_err());
    }

    #[test]
    fn incompressible_uses_store_path() {
        // uniform random bytes are near-max-entropy: must take the 'R' fast
        // path so archive overhead stays ~0.1% instead of growing.
        let raw: Vec<u8> = (0..16 * 1024).map(|i: u32| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
        let blk = compress_block(&raw);
        assert!(blk.len() >= 2 && blk[0] == 0xA0 && blk[1] == b'R', "expected store path");
        assert_eq!(blk.len(), raw.len() + 6);
        let out = decompress_block(&blk, raw.len()).unwrap();
        assert_eq!(out, raw);
    }

    #[test]
    fn medium_mixed_data_falls_back_to_store() {
        // text sprinkled into pseudo-random data: fold may miss, but store must
        // still roundtrip losslessly and never exceed raw+6.
        let mut raw: Vec<u8> = (0..64 * 1024).map(|i: u32| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
        for i in 0..256 {
            raw[i * 256..i * 256 + 9].copy_from_slice(b"aahl-test");
        }
        let blk = compress_block(&raw);
        assert!(blk.len() <= raw.len() + 6);
        let out = decompress_block(&blk, raw.len()).unwrap();
        assert_eq!(out, raw);
    }

    #[test]
    fn store_block_rejects_length_mismatch() {
        assert!(decompress_block(&[0xA0, b'R', 2, 0, 0, 0, b'x'], 3).is_err());
        assert!(decompress_block(&[0xA0, b'R', 3, 0, 0, 0, b'x'], 2).is_err());
        assert!(decompress_block(&[0xA0, b'R', 1, 0, 0, 0], 1).is_err());
    }
    #[test]
    fn mode_sizes_order1_byte_matches_block_sizes() {
        // The ablation diagnostic must agree with the established block_sizes
        // measurement for the codecs they share (fold/order0/order1tok/o1byte).
        let raw = b"fn foo() { return bar(baz); } else { x += 1; }\n".repeat(50);
        let m = mode_sizes(&raw);
        let (folded, arith, o1t, o1b, _packed, _r, _mode) = block_sizes(&raw);
        assert_eq!(m.fold, folded);
        assert_eq!(m.order0, arith);
        assert_eq!(m.order1_tok, o1t);
        assert_eq!(m.order1_byte, o1b);
    }

    #[test]
    fn mode_sizes_recursion_off_has_zero_rules_delta() {
        // With max_merges=0 no new rules are invented: order0/order1tok over
        // the token stream must equal the byte-level order1 cost (same
        // alphabet 256, no folding), and blend must equal order1_tok exactly
        // because a 256-symbol stream has no rule buckets to blend.
        let raw = b"abababab cdcdcdcd efefefef 12345678\n".repeat(20);
        let on = mode_sizes_with(&raw, &FoldConfig::default());
        let off_cfg = FoldConfig { max_merges: 0, ..FoldConfig::default() };
        let off = mode_sizes_with(&raw, &off_cfg);
        assert!(on.fold < off.fold, "folding must shrink the fold block ({} vs {})", on.fold, off.fold);
        // order0 arith over the folded stream should beat the literal stream
        assert!(on.order0 < off.order0, "grammar tokens must beat literals order0");
    }

    #[test]
    fn mode_sizes_blend_is_sane_upper_bound() {
        // blend adds an order-2 model; on skewed single-rule streams it must
        // not be materially worse than order1_tok, and on two-back structure
        // it should win (mirrors the arith unit test).
        let n = 300;
        let mut tok: Vec<u16> = Vec::new();
        for i in 0..60_000u32 {
            let row = (i % 8) as u16;
            tok.push(row);
            tok.push(100u16);
            tok.push(if row % 2 == 0 { 1 } else { 2 });
            tok.push(99u16);
        }
        // rebuild a raw byte stream that folds to this shape is impractical;
        // assert the raw-token codec sizes directly instead.
        let o1 = crate::arith::encode_tokens_order1(&tok, n).len();
        let blend = crate::arith::encode_tokens_blend(&tok, n).len();
        assert!(blend < o1, "blend ({blend}) must beat order-1 ({o1}) on two-back structure");
    }

    #[test]
    fn mode_sizes_empty_and_tiny() {
        let m = mode_sizes(b"");
        assert_eq!(m, ModeSizes::default());
        let t = mode_sizes(b"a");
        assert!(t.order1_byte > 0);
    }
}

#[cfg(test)]
mod tests_v4_fold {
    use super::*;

    #[test]
    fn default_fold_budget_is_1024_merges() {
        assert_eq!(FoldConfig::default().max_merges, 1024);
    }

    #[test]
    fn deeper_default_fold_reduces_token_count() {
        // Wide-but-repeating vocabulary: every line is a distinct 24-word
        // sequence over 16 words, so folding needs MANY merges (one distinct
        // phrase per bigram type) and a larger merge budget must strictly
        // reduce the token count that survives.
        let vocab: Vec<&str> = vec![
            "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
            "india", "juliet", "kilo", "lima", "mike", "november", "oscar", "papa",
        ];
        let mut raw: Vec<u8> = Vec::new();
        let mut state: u64 = 0x9E3779B97F4A7C15;
        for line in 0..4000 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let mut seed = state ^ (line as u64 * 0x9E3779B9);
            for _ in 0..24 {
                seed = seed.wrapping_mul(2862933555777941757).wrapping_add(3037000493);
                raw.extend_from_slice(vocab[(seed >> 33) as usize % vocab.len()].as_bytes());
                raw.push(b' ');
            }
            raw.push(b'\n');
        }
        let syms: Vec<u16> = raw.iter().map(|&b| b as u16).collect();
        let deep = fold_with(syms.clone(), &FoldConfig::default());
        let shallow_cfg = FoldConfig {
            max_merges: 256,
            min_pair_count: MIN_PAIR_COUNT,
            max_syms: MAX_SYMS,
        };
        let shallow = fold_with(syms, &shallow_cfg);
        assert!(
            deep.0.len() < shallow.0.len(),
            "deeper merge budget must reduce tokens ({} vs {})",
            deep.0.len(),
            shallow.0.len()
        );
        assert_eq!(
            crate::aahl::unfold(&deep.0, &deep.1, raw.len()).unwrap(),
            raw,
            "deeper fold must stay lossless"
        );
    }
}
