//! Persistent cross-block grammar.
//!
//! The core problem this module attacks: `aahl::compress_block` is stateless —
//! each 64 KiB chunk is folded and coded in isolation, so a phrase defined in
//! chunk 1 is reinvented again in chunk 2. zip/7z win on code because their LZ
//! windows see whole-file matches; our grammar resets every chunk.
//!
//! Here we keep ONE grammar across an entire archive:
//!   - a persistent rule table `rules` (id = 256 + index), appended to per chunk;
//!   - a persistent order-1 adaptive model (`auth::PersistentTokenModel`);
//!   - an encoder-side phrase trie over rule byte-expansions, so raw bytes that
//!     already match an existing rule emit that rule's id (pure reuse, no LZ).
//!
//! Commit discipline (what keeps encode/decode in lockstep):
//!   - The encoder emits a 'G' block and *then* commits the rules/models it
//!     used. If the candidate block does not beat the stateless codec, the
//!     model rolls back and no rules are appended (the chunk goes out as a
//!     normal stateless block). The decoder advances its grammar ONLY when it
//!     reads a 'G' block; the defs it contains fully determine the delta.
//!   - Both sides therefore walk exactly the same chunk order and the same
//!     token streams; grow/append decisions are transmitted, never inferred.
//!
//! Garbage collection:
//!   - At each GC-interval boundary the encoder computes the live rule set
//!     (rules referenced by a token inside the window, plus the transitive
//!     closure of rule definitions reachable from them). If that set is
//!     strictly smaller than the table it serializes a GC record (survivor
//!     indices, ascending) and applies the remap locally; the container
//!     writes the record between DATA records and the decoder applies the
//!     same remap before the next chunk. Dead grammar history is thereby
//!     reclaimed in place — alphabet, token costs and memory shrink together.

use std::collections::HashMap;

use anyhow::{bail, Context, Result};

use crate::aahl::{FoldConfig, unfold};
use crate::arith::{Decoder, Encoder, PersistentTokenModel};

/// Rule expansions longer than this are never put in the phrase trie. Such
/// rules still work as grammar symbols (they decode fine) but the reuse pass
/// cannot match them against raw bytes, because only a truncated expansion is
/// known. Cap keeps trie memory + match cost bounded.
const MAX_PHRASE: usize = 256;

/// Hard ceiling on symbols: literals (256) + rules. Mirrors aahl::MAX_SYMS.
const MAX_SYMS: usize = 4096;

/// A G candidate may exceed the stateless block by up to this many bytes and
/// still be accepted, so the persistent grammar can bootstrap on the first
/// compressible chunk. Without the credit the very first G block loses to
/// stateless, gets rolled back, and the grammar never grows.
const INVEST_CREDIT: usize = 256;

/// 'G' block layout: [0xA0, b'G', num_new_rules u32, (l u16, r u16)*,
///  num_tokens u32, blob_len u32, blob bytes].
const G_TAG: (u8, u8) = (0xA0, b'G');

/// GC record body layout: flags u8 (reserved, 0) | num_survivors u32 |
/// (survivor u32)* in ascending pre-GC rule order. Body length = 5 + 4*n.
/// A GC record sits *between* DATA records (it is not a chunk); it compacts
/// the persistent grammar by dropping rule-history dead since the last GC
/// interval and renumbering the survivors 0..k'-1, so the model alphabet
/// (and every token cost under it) shrinks. The survivors are transmitted
/// explicitly — the decoder applies exactly the remap the encoder did.
const GC_KIND: u8 = 0x02;
const GC_BODY_HDR: usize = 1 + 4;
const DEFAULT_GC_INTERVAL: usize = 64;

struct TNode {
    kids: HashMap<u8, Box<TNode>>,
    rule: Option<u16>,
}

impl TNode {
    fn new() -> Self {
        Self { kids: HashMap::new(), rule: None }
    }
}

/// Byte-level phrase trie mapping rule expansions -> rule ids (encoder only).
/// `longest` returns the deepest (longest) rule match and the byte span it
/// consumes; greedy left-to-right scanning turns raw bytes into reuse tokens.
struct PhraseTrie {
    root: TNode,
}

impl PhraseTrie {
    fn new() -> Self {
        Self { root: TNode::new() }
    }

    fn insert(&mut self, bytes: &[u8], id: u16) {
        let mut cur = &mut self.root;
        for &b in bytes {
            cur = cur.kids.entry(b).or_insert_with(|| Box::new(TNode::new()));
        }
        cur.rule = Some(id);
    }

    /// Longest prefix of `bytes` that is a rule expansion. Returns
    /// `(rule, consumed)`; consumed == 0 means no match.
    fn longest(&self, bytes: &[u8]) -> (Option<u16>, usize) {
        let mut cur = &self.root;
        let mut best: Option<(u16, usize)> = None;
        let mut depth = 0usize;
        for &b in bytes {
            match cur.kids.get(&b) {
                Some(node) => {
                    cur = node;
                    depth += 1;
                    if let Some(r) = cur.rule {
                        best = Some((r, depth));
                    }
                }
                None => break,
            }
        }
        match best {
            Some((r, d)) => (Some(r), d),
            None => (None, 0),
        }
    }
}

/// One grammar owned by the whole archive. Instantiate once on each side
/// (create / extract) and feed chunks in archive order.
pub struct PersistentGrammar {
    rules: Vec<(u16, u16)>,
    pair_index: HashMap<u32, u16>,
    trie: PhraseTrie,
    model: PersistentTokenModel,
    cfg: FoldConfig,
    /// Rules committed before the current chunk attempt; rejection truncates
    /// back to this so staged (untransmitted) rules never drift the encoder.
    rules_before: usize,
    /// GC interval in emitted DATA chunks (from the v3 params). On interval
    /// boundaries the encoder recomputes the live rule set and emits a GC
    /// record when that set is strictly smaller than the table.
    gc_interval: usize,
    /// Monotonic per-chunk sequence; rules carry the seq of their last use.
    seq: u64,
    /// First chunk seq of the current GC window; rules last used at or after
    /// this are live.
    interval_start_seq: u64,
    /// Per-rule last-use sequence (rule index -> seq).
    last_use: Vec<u64>,
}

impl Default for PersistentGrammar {
    fn default() -> Self {
        Self::with_gc(FoldConfig::default(), DEFAULT_GC_INTERVAL)
    }
}

impl PersistentGrammar {
    pub fn new(cfg: FoldConfig) -> Self {
        Self::with_gc(cfg, DEFAULT_GC_INTERVAL)
    }

    /// Instantiate with a custom GC interval (chunks between GC attempts).
    pub fn with_gc(cfg: FoldConfig, gc_interval: usize) -> Self {
        Self {
            rules: Vec::new(),
            pair_index: HashMap::new(),
            trie: PhraseTrie::new(),
            model: PersistentTokenModel::new(256),
            cfg,
            rules_before: 0,
            gc_interval: gc_interval.max(1),
            seq: 0,
            interval_start_seq: 1,
            last_use: Vec::new(),
        }
    }

    /// Number of rules in the persistent table (k).
    pub fn rules_len(&self) -> usize {
        self.rules.len()
    }

    /// Byte expansion of rule at index `idx`. Returns None if the expansion is
    /// longer than `cap` (unknown / unbounded). Iterative, DAG-safe.
    fn rule_bytes(&self, idx: usize, cap: usize) -> Option<Vec<u8>> {
        if idx >= self.rules.len() {
            return None;
        }
        let mut out = Vec::with_capacity(cap.min(64));
        // stack of symbols to expand (literal or rule id)
        let mut stack: Vec<u16> = vec![(256 + idx) as u16];
        let mut exhausted = true;
        while let Some(s) = stack.pop() {
            if out.len() >= cap {
                exhausted = false; // stacked under cap; expansion longer than cap
                break;
            }
            if s < 256 {
                out.push(s as u8);
            } else {
                let ridx = (s - 256) as usize;
                if ridx >= self.rules.len() {
                    return None;
                }
                let (l, r) = self.rules[ridx];
                stack.push(r);
                stack.push(l);
            }
        }
        if !exhausted {
            return None;
        }
        Some(out)
    }

    /// Encode `raw` with the persistent grammar. Returns a 'G' block when it
    /// beats the best stateless block (then the grammar is committed), or the
    /// stateless block otherwise (grammar untouched, matching the decoder).
    /// The second element is a serialized GC record to write *after* this
    /// chunk's DATA record when an interval boundary found dead rule history;
    /// it must be applied by the decoder before decoding the next chunk.
    pub fn compress(&mut self, raw: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
        self.seq += 1;
        let blk = if raw.is_empty() {
            crate::aahl::compress_block(raw)
        } else {
            // Same entropy fast-path as compress_block: near-random data cannot
            // be grammar-coded; skipping also avoids polluting the rule table.
            if crate::aahl::byte_entropy(raw) >= 7.9 {
                crate::aahl::compress_block(raw)
            } else {
                let stateless = crate::aahl::compress_block(raw);
                let g = self.try_compress(raw);
                if let Some((g, used)) = g {
                    // Accept when it beats stateless outright, or when the
                    // grammar is still young: paying a small per-block premium
                    // now lets later chunks reuse invented rules (which is the
                    // whole point). Without this the first grammar chunk always
                    // loses to stateless, is rejected, its rules are rolled
                    // back, and the grammar can never bootstrap.
                    if g.len() < stateless.len() + INVEST_CREDIT {
                        for idx in used {
                            if let Some(slot) = self.last_use.get_mut(idx) {
                                *slot = self.seq;
                            }
                        }
                        self.commit_new_rules();
                        g
                    } else {
                        self.reject_chunk();
                        stateless
                    }
                } else {
                    self.reject_chunk();
                    stateless
                }
            }
        };
        let gc = self.gc_attempt();
        (blk, gc)
    }

    /// At an interval boundary, if the live rule set is strictly smaller than
    /// the full table, serialize a GC record and apply it to the encoder state.
    /// Returns the record bytes to write after the just-emitted DATA record.
    fn gc_attempt(&mut self) -> Option<Vec<u8>> {
        let interval = self.gc_interval as u64;
        if self.seq.saturating_sub(self.interval_start_seq) + 1 < interval {
            return None;
        }
        // window = chunks in [interval_start_seq, seq]; rules emitted as a
        // token in that window are live, plus the transitive closure of rule
        // definitions reachable from them. The window advances exclusively, so
        // the chunk that closed the previous window is never counted twice.
        let window_from = self.interval_start_seq;
        self.interval_start_seq = self.seq + 1;
        let mut live: Vec<bool> = vec![false; self.rules.len()];
        for idx in 0..self.rules.len() {
            if self.last_use[idx] >= window_from {
                live[idx] = true;
            }
        }
        let survivors = self.live_closure(&mut live);
        if self.rules.is_empty() || survivors.len() >= self.rules.len() {
            return None;
        }
        let rec = Self::encode_gc_record(&survivors);
        match self.apply_gc(&survivors) {
            Ok(()) => Some(rec),
            Err(_) => None,
        }
    }

    /// Transitive closure over rule definitions, starting from the `live` bit
    /// set. Any symbol referenced as l/r by a live rule must survive too, or
    /// the decoder could not unfold it. Returns survivor rule indices sorted.
    fn live_closure(&self, live: &mut Vec<bool>) -> Vec<u32> {
        let mut stack: Vec<usize> = (0..self.rules.len()).filter(|&i| live[i]).collect();
        while let Some(i) = stack.pop() {
            let (l, r) = self.rules[i];
            for comp in [l, r] {
                if comp >= 256 {
                    let j = (comp - 256) as usize;
                    if j < self.rules.len() && !live[j] {
                        live[j] = true;
                        stack.push(j);
                    }
                }
            }
        }
        (0..self.rules.len()).filter(|&i| live[i]).map(|i| i as u32).collect()
    }

    /// Serialize a GC record: kind | body_len | flags | num_survivors |
    /// survivor u32 list. Written into the archive between DATA records.
    fn encode_gc_record(survivors: &[u32]) -> Vec<u8> {
        let body_len = GC_BODY_HDR + 4 * survivors.len();
        let mut rec = Vec::with_capacity(5 + body_len);
        rec.push(GC_KIND);
        rec.extend_from_slice(&(body_len as u32).to_le_bytes());
        rec.push(0); // flags
        rec.extend_from_slice(&(survivors.len() as u32).to_le_bytes());
        for &s in survivors {
            rec.extend_from_slice(&s.to_le_bytes());
        }
        rec
    }

    /// Parse a GC record *body* (flags | num_survivors | survivors...).
    /// Validates framing, strict ascending order and bounds; returns the
    /// survivor rule indices. Used by the container reader so corrupt archive
    /// inputs fail cleanly instead of panicking.
    pub fn parse_gc_body(body: &[u8]) -> Result<Vec<u32>> {
        if body.len() < GC_BODY_HDR {
            bail!("GC record too small");
        }
        if body[0] != 0 {
            bail!("unknown GC flags {}", body[0]);
        }
        let n = u32::from_le_bytes([body[1], body[2], body[3], body[4]]) as usize;
        if body.len() != GC_BODY_HDR + 4 * n {
            bail!("GC record length mismatch");
        }
        let mut out = Vec::with_capacity(n);
        let mut prev: Option<u32> = None;
        for k in 0..n {
            let off = GC_BODY_HDR + 4 * k;
            let v = u32::from_le_bytes([
                body[off],
                body[off + 1],
                body[off + 2],
                body[off + 3],
            ]);
            if let Some(p) = prev {
                if v <= p {
                    bail!("GC survivor list not strictly ascending");
                }
            }
            prev = Some(v);
            out.push(v);
        }
        Ok(out)
    }

    /// Apply a GC remap: keep only `survivors` (rule indices, ascending),
    /// renumber them 0..k'-1, recycle the encoder indices and shrink the model
    /// alphabet to 256 + k'. Must run on encoder and decoder with the exact
    /// same survivor list at the exact same chunk boundary.
    pub fn apply_gc(&mut self, survivors: &[u32]) -> Result<()> {
        let old_len = self.rules.len();
        if survivors.len() > MAX_SYMS - 256 {
            bail!("too many survivors");
        }
        // old id -> new id for survivors (position in the ascending list)
        let mut remap: HashMap<u32, u16> = HashMap::with_capacity(survivors.len());
        let mut prev: Option<usize> = None;
        for (new_i, &si) in survivors.iter().enumerate() {
            let i = si as usize;
            if i >= old_len {
                bail!("GC survivor index out of range");
            }
            if let Some(p) = prev {
                if i <= p {
                    bail!("GC survivor list not strictly ascending");
                }
            }
            prev = Some(i);
            remap.insert((256 + i) as u32, (256 + new_i) as u16);
        }
        // rewrite the surviving definitions into the new id-space and renumber
        let mut kept: Vec<(u16, u16)> = Vec::with_capacity(survivors.len());
        let mut new_use: Vec<u64> = Vec::with_capacity(survivors.len());
        for &si in survivors {
            let (l, r) = self.rules[si as usize];
            let nl = if l >= 256 { *remap.get(&(l as u32)).context("dead component")? } else { l };
            let nr = if r >= 256 { *remap.get(&(r as u32)).context("dead component")? } else { r };
            kept.push((nl, nr));
            new_use.push(self.last_use.get(si as usize).copied().unwrap_or(0));
        }
        self.rules = kept;
        self.last_use = new_use;
        self.rules_before = self.rules.len();

        let n2 = 256 + self.rules.len();
        self.model.shrink_alphabet(n2);

        self.pair_index.clear();
        self.trie = PhraseTrie::new();
        for i in 0..self.rules.len() {
            let (l, r) = self.rules[i];
            let id = (256 + i) as u16;
            self.pair_index.insert(((l as u32) << 16) | r as u32, id);
            if let Some(bytes) = self.rule_bytes(i, MAX_PHRASE) {
                if bytes.len() >= 2 {
                    self.trie.insert(&bytes, id);
                }
            }
        }
        Ok(())
    }

    /// Decode `blk`. Non-'G' blocks are dispatched to the stateless decoder and
    /// never touch the grammar; 'G' blocks advance it.
    pub fn decompress(&mut self, blk: &[u8], expected_len: usize) -> Result<Vec<u8>> {
        if !(blk.len() >= 2 && blk[0] == G_TAG.0 && blk[1] == G_TAG.1) {
            return crate::aahl::decompress_block(blk, expected_len);
        }
        self.model.begin_chunk();
        let mut p = 2usize;
        if blk.len() < p + 4 {
            bail!("G block too small");
        }
        let num_new = u32::from_le_bytes([blk[p], blk[p + 1], blk[p + 2], blk[p + 3]]) as usize;
        p += 4;
        if num_new > MAX_SYMS - 256 {
            bail!("too many new rules");
        }
        if blk.len() < p + num_new * 4 + 8 {
            bail!("G block truncated (defs)");
        }
        let base = self.rules.len();
        for i in 0..num_new {
            let l = u16::from_le_bytes([blk[p], blk[p + 1]]);
            let r = u16::from_le_bytes([blk[p + 2], blk[p + 3]]);
            p += 4;
            // forward-reference check, same shape as arith_decode
            let new_id = 256 + base + i;
            if (l as usize) >= new_id || (r as usize) >= new_id {
                bail!(
                    "forward grammar reference: def[{i}] ({l},{r}) base={base} new_id={new_id} rules={}",
                    self.rules.len()
                );
            }
            self.rules.push((l, r));
        }
        let n = 256 + self.rules.len();
        self.model.grow(n);
        let num_tokens = u32::from_le_bytes([blk[p], blk[p + 1], blk[p + 2], blk[p + 3]]) as usize;
        p += 4;
        let blob_len = u32::from_le_bytes([blk[p], blk[p + 1], blk[p + 2], blk[p + 3]]) as usize;
        p += 4;
        if blk.len() < p + blob_len {
            bail!("G block truncated (blob)");
        }
        let blob = &blk[p..p + blob_len];
        let mut dec = Decoder::new(blob);
        let mut tokens: Vec<u16> = Vec::with_capacity(num_tokens);
        for _ in 0..num_tokens {
            tokens.push(self.model.decode_token(&mut dec).map_err(anyhow::Error::msg)?);
        }
        let raw = unfold(&tokens, &self.rules, expected_len)?;
        if raw.len() != expected_len {
            bail!("G length mismatch");
        }
        Ok(raw)
    }

    // ---- encoder internals -------------------------------------------------

    fn try_compress(&mut self, raw: &[u8]) -> Option<(Vec<u8>, Vec<usize>)> {
        self.model.begin_chunk();
        self.rules_before = self.rules.len();

        let tokens = self.reuse_pass(raw);
        if tokens.is_empty() {
            return None;
        }
        let tokens = self.invent(tokens);

        let n = 256 + self.rules.len();
        self.model.grow(n);

        let mut enc = Encoder::new();
        let mut used: Vec<usize> = Vec::new();
        for &t in &tokens {
            debug_assert!((t as usize) < n);
            if (t as usize) >= 256 {
                used.push((t - 256) as usize);
            }
            self.model.encode_token(t as usize, &mut enc);
        }
        let blob = enc.flush();

        // assemble block
        let num_new = self.rules.len() - self.rules_before;
        let mut out = Vec::with_capacity(blob.len() + 2 + 4 + num_new * 4 + 8);
        out.push(G_TAG.0);
        out.push(G_TAG.1);
        out.extend_from_slice(&(num_new as u32).to_le_bytes());
        for &(l, r) in &self.rules[self.rules_before..] {
            out.extend_from_slice(&l.to_le_bytes());
            out.extend_from_slice(&r.to_le_bytes());
        }
        out.extend_from_slice(&(tokens.len() as u32).to_le_bytes());
        out.extend_from_slice(&(blob.len() as u32).to_le_bytes());
        out.extend_from_slice(&blob);

        // must beat the store fallback or there is nothing to emit
        if out.len() >= raw.len() + 6 {
            return None;
        }
        Some((out, used))
    }

    /// Undo model + rule changes since the last `rules_before` snapshot. Rejection
    /// discards every staged (never-transmitted) rule so the encoder table stays
    /// exactly equal to the decoder's.
    fn reject_chunk(&mut self) {
        self.model.rollback_chunk();
        self.rules.truncate(self.rules_before);
    }

    /// Publish rules invented this chunk: once the 'G' block is accepted, the
    /// dedupe index and phrase trie are updated so later chunks see them.
    fn commit_new_rules(&mut self) {
        for i in self.rules_before..self.rules.len() {
            let (l, r) = self.rules[i];
            let id = (256 + i) as u16;
            self.pair_index.insert(((l as u32) << 16) | r as u32, id);
            if let Some(bytes) = self.rule_bytes(i, MAX_PHRASE) {
                if bytes.len() >= 2 {
                    self.trie.insert(&bytes, id);
                }
            }
        }
        // GC liveness bookkeeping: every committed rule gets a last-use slot
        // (0 = never referenced by a token yet).
        self.last_use.resize(self.rules.len(), 0);
    }

    /// Turn raw bytes into a token list: longest rule-expansion match emits the
    /// rule id; unmatched bytes stay literal. Grammar reuse, not LZ pointers.
    fn reuse_pass(&self, raw: &[u8]) -> Vec<u16> {
        let mut tokens = Vec::with_capacity(raw.len());
        let mut i = 0usize;
        while i < raw.len() {
            let (rule, consumed) = self.trie.longest(&raw[i..]);
            if consumed >= 2 {
                if let Some(r) = rule {
                    tokens.push(r);
                    i += consumed;
                    continue;
                }
            }
            tokens.push(raw[i] as u16);
            i += 1;
        }
        tokens
    }

    /// Fold the token stream a bit more, inventing NEW rules that append to the
    /// persistent table. Deduplicates: if a pair is already a committed rule,
    /// that id is reused instead of inventing a twin. Deterministic like
    /// aahl::fold_with (count desc, key desc).
    fn invent(&mut self, syms: Vec<u16>) -> Vec<u16> {
        let cfg = self.cfg;
        let base = self.rules.len();
        let max_syms = cfg.max_syms;
        // split borrows: cap_ok only needs a copy, on_pair needs &mut rules and
        // &pair_index (disjoint fields)
        let rules = &mut self.rules;
        let pair_index = &self.pair_index;
        crate::aahl::bpe_engine(
            syms,
            &cfg,
            |rules_len| 256 + base + rules_len < max_syms,
            |l, r, rep| {
                // a reuse/merge must still shrink the token count by >= 1
                if rep < 1 {
                    return (None, false);
                }
                let key = ((l as u32) << 16) | r as u32;
                let (id, added) = match pair_index.get(&key) {
                    Some(&id) => (id, false),
                    None => {
                        let id = (256 + rules.len()) as u16;
                        rules.push((l, r));
                        (id, true)
                    }
                };
                (Some(id), added)
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_gc_body(body: &[u8]) -> Result<Vec<u32>> {
        PersistentGrammar::parse_gc_body(body)
    }

    fn roundtrip_chunks(enc: &mut PersistentGrammar, dec: &mut PersistentGrammar, chunks: &[&[u8]]) {
        for raw in chunks {
            let (blk, gc) = enc.compress(raw);
            let out = dec.decompress(&blk, raw.len()).expect("decode");
            assert_eq!(out, *raw, "chunk mismatch");
            // GC records are decoded AFTER the chunk that produced them and
            // BEFORE the next one — exactly the encoder boundary.
            if let Some(gc) = gc {
                let survivors = parse_gc_body(&gc[5..]).expect("parse gc");
                dec.apply_gc(&survivors).expect("apply gc");
            }
            assert_eq!(enc.rules_len(), dec.rules_len(), "grammars drifted");
            assert_eq!(enc.model.n(), dec.model.n(), "models drifted");
        }
    }

    fn multi_chunk_roundtrip(chunks: &[&[u8]]) {
        let mut enc = PersistentGrammar::new(FoldConfig::default());
        let mut dec = PersistentGrammar::new(FoldConfig::default());
        assert_eq!(enc.rules_len(), dec.rules_len());
        roundtrip_chunks(&mut enc, &mut dec, chunks);
    }

    #[test]
    fn cross_block_reuse_grows_grammar_and_stays_synced() {
        let idiom = b"fn process(&self) -> usize { unimplemented!() } ";
        let mut chunks: Vec<Vec<u8>> = Vec::new();
        for i in 0..8 {
            let mut c = Vec::new();
            for _ in 0..20 {
                c.extend_from_slice(idiom);
            }
            c.extend_from_slice(format!("// chunk {i}\n").as_bytes());
            c.extend_from_slice(b"".as_slice());
            chunks.push(c);
        }
        let refs: Vec<&[u8]> = chunks.iter().map(|c| c.as_slice()).collect();
        multi_chunk_roundtrip(&refs);
    }

    #[test]
    fn mixed_content_roundtrip() {
        let a: &[u8] = b"The quick brown fox jumps over the lazy dog. ";
        let b: &[u8] = b"fn main() { println!(\"hello\", 42); } ";
        let c: &[u8] = b"\x00\x01\x02\x03\x04\x05\x06\x07random\x08noise\xff";
        multi_chunk_roundtrip(&[a, b, c, a, b, c]);
    }

    #[test]
    fn incompressible_chunks_do_not_pollute_grammar() {
        let mut enc = PersistentGrammar::new(FoldConfig::default());
        let mut dec = PersistentGrammar::new(FoldConfig::default());
        let noise: Vec<u8> = (0..4096).map(|i| ((i * 131 + 17) % 251) as u8).collect();
        for _ in 0..3 {
            let (blk, _gc) = enc.compress(&noise);
            let out = dec.decompress(&blk, noise.len()).unwrap();
            assert_eq!(out, noise);
        }
        assert_eq!(enc.rules_len(), dec.rules_len());
    }

    fn idiom_a() -> Vec<u8> {
        b"fn process(&self) -> Vec<u8> { unimplemented!() } fn helper(a: u32) -> u32 { a.wrapping_mul(7) } "
            .repeat(40)
    }

    fn idiom_b() -> Vec<u8> {
        b"def transform(xs):\n    return [x * x + 1 for x in xs if x > 0]\nimport os, sys as _s\n"
            .repeat(40)
    }

    #[test]
    fn gc_records_fired_roundtrip_and_bounded() {
        let mut enc = PersistentGrammar::with_gc(FoldConfig::default(), 8);
        let mut dec = PersistentGrammar::with_gc(FoldConfig::default(), 8);
        let a = idiom_a();
        let b = idiom_b();
        let mut gcs = 0usize;
        let mut max_rules = 0usize;
        for i in 0..40 {
            let chunk = if i % 16 < 8 { &a } else { &b };
            let (blk, gc) = enc.compress(chunk);
            let out = dec.decompress(&blk, chunk.len()).expect("decode");
            assert_eq!(out, *chunk);
            if gc.is_some() {
                gcs += 1;
            }
            if let Some(gc) = gc {
                let survivors = parse_gc_body(&gc[5..]).expect("parse gc");
                dec.apply_gc(&survivors).expect("apply gc");
            }
            assert_eq!(enc.rules_len(), dec.rules_len(), "grammars drifted");
            assert_eq!(enc.model.n(), dec.model.n(), "models drifted");
            max_rules = max_rules.max(enc.rules_len());
        }
        assert!(gcs >= 1, "expected at least one GC record, got {gcs}");
        // after A dies to B, the grammar must have reclaimed the A history
        assert!(
            enc.rules_len() < max_rules,
            "GC should shrink rules: final {} < peak {max_rules}",
            enc.rules_len()
        );
    }

    #[test]
    fn gc_record_framing_and_validation() {
        // encode -> parse roundtrip
        let survivors = vec![0u32, 3, 7, 12];
        let rec = PersistentGrammar::encode_gc_record(&survivors);
        let body = &rec[5..];
        assert_eq!(parse_gc_body(body).unwrap(), survivors);
        // single survivor
        let rec = PersistentGrammar::encode_gc_record(&[0u32]);
        assert_eq!(parse_gc_body(&rec[5..]).unwrap(), vec![0]);
        // corrupted framing is rejected, never panics
        assert!(parse_gc_body(&[]).is_err()); // too small
        assert_eq!(parse_gc_body(&[0u8, 0, 0, 0, 0]).unwrap(), vec![]); // empty = full reset, valid
        assert!(parse_gc_body(&[0u8, 0, 0, 0, 1, 0, 0, 0]).is_err()); // len mismatch (8 != 9)
        assert!(parse_gc_body(&[1u8, 0, 0, 0, 0]).is_err()); // bad flags
        // non-ascending is rejected
        let mut bad = b"\x00\x02\x00\x00\x00\x10\x00\x00\x00\x00\x00\x00\x00".to_vec();
        assert!(parse_gc_body(&bad).is_err());
        bad = b"\x00\x02\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00".to_vec();
        assert!(parse_gc_body(&bad).is_err());
        // valid single survivor at a high rule id parses fine (LE: num=1, s0=256)
        assert_eq!(parse_gc_body(&[0u8, 1, 0, 0, 0, 0, 1, 0, 0]).unwrap(), vec![256]);
    }

    #[test]
    fn apply_gc_with_empty_survivors_resets_and_roundtrips() {
        let mut enc = PersistentGrammar::with_gc(FoldConfig::default(), 256);
        let mut dec = PersistentGrammar::with_gc(FoldConfig::default(), 256);
        let a = idiom_a();
        let mut late_chunks = 0;
        for i in 0..6 {
            let (blk, _gc) = enc.compress(&a);
            let out = dec.decompress(&blk, a.len()).unwrap();
            assert_eq!(out, a);
            late_chunks += 1;
            assert_eq!(enc.rules_len(), dec.rules_len());
        }
        assert!(late_chunks == 6);
        let peak = enc.rules_len();
        assert!(peak > 0, "expected invented rules");
        // fully reclaim: drop every rule, both sides, then keep compressing
        enc.apply_gc(&[]).unwrap();
        dec.apply_gc(&[]).unwrap();
        assert_eq!(enc.rules_len(), 0);
        assert_eq!(dec.rules_len(), 0);
        assert_eq!(enc.model.n(), dec.model.n());
        // the grammar reinvents from a clean slate and stays in lockstep
        let (blk, gc) = enc.compress(&a);
        assert!(gc.is_none(), "empty grammar should never emit a GC record");
        let out = dec.decompress(&blk, a.len()).unwrap();
        assert_eq!(out, a);
        assert_eq!(enc.rules_len(), dec.rules_len());
        assert_eq!(enc.model.n(), dec.model.n());
    }
}