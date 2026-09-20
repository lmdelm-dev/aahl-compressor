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

use std::collections::HashMap;

use anyhow::{bail, Result};

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
}

impl Default for PersistentGrammar {
    fn default() -> Self {
        Self::new(FoldConfig::default())
    }
}

impl PersistentGrammar {
    pub fn new(cfg: FoldConfig) -> Self {
        Self {
            rules: Vec::new(),
            pair_index: HashMap::new(),
            trie: PhraseTrie::new(),
            model: PersistentTokenModel::new(256),
            cfg,
            rules_before: 0,
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
    pub fn compress(&mut self, raw: &[u8]) -> Vec<u8> {
        if raw.is_empty() {
            return crate::aahl::compress_block(raw);
        }
        // Same entropy fast-path as compress_block: near-random data cannot be
        // grammar-coded; skipping also avoids polluting the rule table.
        if crate::aahl::byte_entropy(raw) >= 7.9 {
            return crate::aahl::compress_block(raw);
        }
        let stateless = crate::aahl::compress_block(raw);
        let g = self.try_compress(raw);
        if let Some(g) = g {
            // Accept when it beats stateless outright, or when the grammar is
            // still young: paying a small per-block premium now lets later
            // chunks reuse invented rules (which is the whole point). Without
            // this the first grammar chunk always loses to stateless, is
            // rejected, its rules are rolled back, and the grammar can never
            // bootstrap.
            if g.len() < stateless.len() + INVEST_CREDIT {
                self.commit_new_rules();
                return g;
            }
        }
        self.reject_chunk();
        stateless
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

    fn try_compress(&mut self, raw: &[u8]) -> Option<Vec<u8>> {
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
        for &t in &tokens {
            debug_assert!((t as usize) < n);
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
        Some(out)
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

    fn multi_chunk_roundtrip(chunks: &[&[u8]]) {
        let mut enc = PersistentGrammar::new(FoldConfig::default());
        let mut dec = PersistentGrammar::new(FoldConfig::default());
        assert_eq!(enc.rules_len(), dec.rules_len());
        for raw in chunks {
            let blk = enc.compress(raw);
            let out = dec.decompress(&blk, raw.len()).expect("decode");
            assert_eq!(out, *raw, "chunk mismatch");
            assert_eq!(enc.rules_len(), dec.rules_len(), "grammars drifted");
            assert_eq!(enc.model.n(), dec.model.n(), "models drifted");
        }
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
            let blk = enc.compress(&noise);
            let out = dec.decompress(&blk, noise.len()).unwrap();
            assert_eq!(out, noise);
        }
        assert_eq!(enc.rules_len(), dec.rules_len());
    }
}