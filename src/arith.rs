//! From-scratch carryless range coder (LZMA-style) + adaptive order-0 model.
//! Encodes a folded-token stream with per-symbol adaptive counts tracked
//! through a Fenwick tree, so probabilities converge to the true distribution
//! without storing a model table in the block.

const TOP: u64 = 1u64 << 24;
const MAX_SYMS: usize = 4096;

pub struct Encoder {
    low: u64,
    range: u64,
    cache: u8,
    cache_size: u64,
    out: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Self {
        Self { low: 0, range: 0xFFFF_FFFF, cache: 0, cache_size: 1, out: Vec::new() }
    }

    fn shift_low(&mut self) {
        if (self.low as u32) < 0xFF00_0000u32 || (self.low >> 32) != 0 {
            let mut temp = self.cache;
            let carry = (self.low >> 32) as u8;
            loop {
                self.out.push(temp.wrapping_add(carry));
                temp = 0xFF;
                self.cache_size -= 1;
                if self.cache_size == 0 {
                    break;
                }
            }
            self.cache = ((self.low as u32) >> 24) as u8;
        }
self.cache_size += 1;
        self.low = (((self.low as u32) << 8) & 0xFFFF_FFFF) as u64;
    }

    fn encode_step(&mut self, start: u64, size: u64, total: u64) {
        debug_assert!(total > 0 && size > 0 && start + size <= total);
        self.range /= total;
        self.low += start * self.range;
        self.range *= size;
        while self.range < TOP {
            self.range <<= 8;
            self.shift_low();
        }
    }

    pub fn flush(mut self) -> Vec<u8> {
        for _ in 0..5 {
            self.shift_low();
        }
        self.out
    }
}

pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
    code: u64,
    range: u64,
}

impl<'a> Decoder<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        let mut d = Self { buf, pos: 0, code: 0, range: 0xFFFF_FFFF };
        for _ in 0..5 {
            d.code = ((d.code << 8) | d.read_byte()) & 0xFFFF_FFFF;
        }
        d
    }

    fn read_byte(&mut self) -> u64 {
        let b = if self.pos < self.buf.len() { self.buf[self.pos] } else { 0 };
        self.pos += 1;
        b as u64
    }

    /// Divides range by total and returns the scaled code (threshold).
    pub fn threshold(&mut self, total: u64) -> u64 {
        self.range /= total;
        if self.range == 0 {
            // pathological overshoot; keep code in range via clamp
            0
        } else {
            self.code / self.range
        }
    }

    pub fn decode_step(&mut self, start: u64, size: u64, _total: u64) {
        self.code -= start * self.range;
        self.range *= size;
        while self.range < TOP {
            self.range <<= 8;
            self.code = ((self.code << 8) | self.read_byte()) & 0xFFFF_FFFF;
        }
    }

    pub fn consumed(&self) -> usize {
        self.pos
    }
}

// ---------- Fenwick prefix sums over adaptive counts ----------

pub struct Fenwick {
    n: usize,
    tree: Vec<u64>,
}

impl Fenwick {
    pub fn zeros(n: usize) -> Self {
        Self { n, tree: vec![0u64; n] }
    }

    /// Apply a signed delta to the count at `idx`. `delta` is `u64` so that
    /// negative adjustments can be expressed as wraparound (`u64::MAX == -1`);
    /// `wrapping_add` makes that contract hold in debug builds too, where plain
    /// `+=` would panic on overflow even though the value is logically valid.
    pub fn add(&mut self, idx: usize, delta: u64) {
        let mut i = idx + 1;
        while i <= self.n {
            self.tree[i - 1] = self.tree[i - 1].wrapping_add(delta);
            i += i & i.wrapping_neg();
        }
    }

    /// Sum of counts for indices in [0, k).
    pub fn prefix(&self, mut k: usize) -> u64 {
        if k > self.n {
            k = self.n;
        }
        let mut s = 0u64;
        let mut i = k;
        while i > 0 {
            s += self.tree[i - 1];
            i -= i & i.wrapping_neg();
        }
        s
    }

    /// Smallest 0-based index `s` such that prefix(s+1) > val.
    pub fn find_gt(&self, val: u64) -> usize {
        let mut bit = self.n.next_power_of_two() >> 1;
        let mut seen = 0u64;
        // walk from high bit down; keep idx such that seen + tree[...] <= val
        let mut cur = 0usize;
        while bit > 0 {
            let next = cur + bit;
            if next <= self.n {
                let tv = self.tree[next - 1];
                if seen + tv <= val {
                    seen += tv;
                    cur = next;
                }
            }
            bit >>= 1;
        }
        cur
    }
}

/// Adaptive order-0 encoder over an alphabet of `n` symbols.
/// Returns the raw range-coded bytes.
pub fn encode_tokens(tokens: &[u16], n: usize) -> Vec<u8> {
    let mut enc = Encoder::new();
    let mut counts = vec![1u64; n];
    let mut fw = Fenwick::zeros(n);
    for i in 0..n {
        fw.add(i, 1);
    }
    let mut total: u64 = n as u64;
    for &t in tokens {
        let s = t as usize;
        let start = fw.prefix(s);
        let size = counts[s];
        enc.encode_step(start, size, total);
        counts[s] += 1;
        fw.add(s, 1);
        total += 1;
    }
    enc.flush()
}

/// Decode `num_tokens` symbols over alphabet `n` from `buf`.
pub fn decode_tokens(buf: &[u8], n: usize, num_tokens: usize, out: &mut Vec<u16>) -> Result<(), String> {
    if n == 0 || n > MAX_SYMS {
        return Err("bad alphabet size".into());
    }
    let mut dec = Decoder::new(buf);
    let mut counts = vec![1u64; n];
    let mut fw = Fenwick::zeros(n);
    for i in 0..n {
        fw.add(i, 1);
    }
    let mut total: u64 = n as u64;
    for _ in 0..num_tokens {
        let th = dec.threshold(total);
        if th >= total {
            return Err("threshold out of range".into());
        }
        let s = fw.find_gt(th);
        if s >= n {
            return Err("symbol out of range".into());
        }
        let start = fw.prefix(s);
        let size = counts[s];
        dec.decode_step(start, size, total);
        out.push(s as u16);
        counts[s] += 1;
        fw.add(s, 1);
        total += 1;
    }
    Ok(())
}

// ---------- Byte-level order-1 context model ----------
//
// Adaptive order-1 arithmetic coder over raw bytes (alphabet 256). The model
// keeps an independent adaptive count table for each previous-byte context,
// so code like `if (` or `def ` costs fractions of a bit without any LZ77 or
// sliding window. Escape is implicit: every context starts at count 1, so no
// symbol ever has zero probability; context 256 is a sentinel used only for
// the very first byte. Two independent lives here beats an order-0 model on
// structured text because each byte's cost is conditioned on the byte before.

const NBYTE: usize = 256;
const FIRST_CTX: usize = 256; // sentinel context for the leading byte

fn fenwick_ones(n: usize) -> Fenwick {
    let mut fw = Fenwick::zeros(n);
    for i in 0..n {
        fw.add(i, 1);
    }
    fw
}

pub fn encode_bytes_order1(raw: &[u8]) -> Vec<u8> {
    const NC: usize = NBYTE + 1;
    let mut enc = Encoder::new();
    let mut counts: Vec<Vec<u64>> = vec![vec![1u64; NBYTE]; NC];
    let mut fw: Vec<Fenwick> = (0..NC).map(|_| fenwick_ones(NBYTE)).collect();
    let mut total: Vec<u64> = vec![NBYTE as u64; NC];
    let mut ctx = FIRST_CTX;
    for &b in raw {
        let s = b as usize;
        let start = fw[ctx].prefix(s);
        let size = counts[ctx][s];
        enc.encode_step(start, size, total[ctx]);
        counts[ctx][s] += 1;
        fw[ctx].add(s, 1);
        total[ctx] += 1;
        ctx = s;
    }
    enc.flush()
}

pub fn decode_bytes_order1(
    buf: &[u8],
    len: usize,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    const NC: usize = NBYTE + 1;
    let mut dec = Decoder::new(buf);
    let mut counts: Vec<Vec<u64>> = vec![vec![1u64; NBYTE]; NC];
    let mut fw: Vec<Fenwick> = (0..NC).map(|_| fenwick_ones(NBYTE)).collect();
    let mut total: Vec<u64> = vec![NBYTE as u64; NC];
    let mut ctx = FIRST_CTX;
    for _ in 0..len {
        let t = total[ctx];
        let th = dec.threshold(t);
        if th >= t {
            return Err("threshold out of range".into());
        }
        let s = fw[ctx].find_gt(th);
        if s >= NBYTE {
            return Err("symbol out of range".into());
        }
        let start = fw[ctx].prefix(s);
        let size = counts[ctx][s];
        dec.decode_step(start, size, t);
        out.push(s as u8);
        counts[ctx][s] += 1;
        fw[ctx].add(s, 1);
        total[ctx] += 1;
        ctx = s as usize;
    }
    Ok(())
}

// ---------- Order-1 over a folded-token alphabet ----------
//
// Context = previous symbol. Symbols < 256 (literals) are their own context;
// symbols >= 256 (grammar rules) are hashed into RULE_CTX buckets so the model
// stays bounded (NC * alphabet) even for alphabets up to MAX_SYMS.

const RULE_CTX: usize = 8;
const TOKEN_NC: usize = 256 + RULE_CTX + 1; // literals + rule buckets + sentinel
const TOKEN_SENTINEL: usize = TOKEN_NC - 1;

fn rule_ctx(sym: usize, n: usize) -> usize {
    256 + (((sym - 256) * 0x9E37_79B9) as usize) % RULE_CTX
}

pub fn token_ctx(sym: usize, n: usize) -> usize {
    if sym < 256 { sym } else { rule_ctx(sym, n) }
}

pub fn encode_tokens_order1(tokens: &[u16], n: usize) -> Vec<u8> {
    let nc = TOKEN_NC;
    let mut enc = Encoder::new();
    let mut counts: Vec<Vec<u64>> = vec![vec![1u64; n]; nc];
    let mut fw: Vec<Fenwick> = (0..nc).map(|_| fenwick_ones(n)).collect();
    let mut total: Vec<u64> = vec![n as u64; nc];
    let mut ctx = TOKEN_SENTINEL;
    for &t in tokens {
        let s = t as usize;
        debug_assert!(s < n);
        let start = fw[ctx].prefix(s);
        let size = counts[ctx][s];
        enc.encode_step(start, size, total[ctx]);
        counts[ctx][s] += 1;
        fw[ctx].add(s, 1);
        total[ctx] += 1;
        ctx = token_ctx(s, n);
    }
    enc.flush()
}

pub fn decode_tokens_order1(
    buf: &[u8],
    n: usize,
    num_tokens: usize,
    out: &mut Vec<u16>,
) -> Result<(), String> {
    if n == 0 || n > MAX_SYMS {
        return Err("bad alphabet size".into());
    }
    let nc = TOKEN_NC;
    let mut dec = Decoder::new(buf);
    let mut counts: Vec<Vec<u64>> = vec![vec![1u64; n]; nc];
    let mut fw: Vec<Fenwick> = (0..nc).map(|_| fenwick_ones(n)).collect();
    let mut total: Vec<u64> = vec![n as u64; nc];
    let mut ctx = TOKEN_SENTINEL;
    for _ in 0..num_tokens {
        let t = total[ctx];
        let th = dec.threshold(t);
        if th >= t {
            return Err("threshold out of range".into());
        }
        let s = fw[ctx].find_gt(th);
        if s >= n {
            return Err("symbol out of range".into());
        }
        let start = fw[ctx].prefix(s);
        let size = counts[ctx][s];
        dec.decode_step(start, size, t);
        out.push(s as u16);
        counts[ctx][s] += 1;
        fw[ctx].add(s, 1);
        total[ctx] += 1;
ctx = token_ctx(s, n);
    }
    Ok(())
}

// ---------- Persistent order-1 token model (cross-block global grammar) ----------
//
// One instance is minted for the whole archive and carried across chunk
// boundaries, so both the rule table (the alphabet) and the adaptive counts
// keep their context from earlier chunks. New rules append to the alphabet as
// the grammar grows; every symbol starts at count 1 in every context (implicit
// escape), so a rule invented in chunk 1 that reappears in chunk 17 still has
// probability > 0 without any extra signalling.
//
// The encoder and decoder must walk exactly the same token streams in the same
// order, or the model drifts apart and decoding corrupts. The container
// guarantees this by feeding chunks to the persistent model in archive order
// (see grammar.rs).

pub struct PersistentTokenModel {
    n: usize,                 // current alphabet size (256 + accumulated rules)
    base_n: usize,            // alphabet size at last begin_chunk (for rollback)
    counts: Vec<Vec<u64>>,    // [TOKEN_NC][n] adaptive counts, grown as rules append
    fw: Vec<Fenwick>,         // [TOKEN_NC] Fenwick prefix trees over the alphabet
    total: Vec<u64>,          // [TOKEN_NC] total count observed per context
    ctx: usize,               // current context (previous token mapped to its ctx)
    // transaction log for rejecting a candidate chunk atomically
    log: Vec<(usize, usize, u64)>,
    start_ctx: usize,
}

impl PersistentTokenModel {
    pub fn new(n0: usize) -> Self {
        assert!((256..=MAX_SYMS).contains(&n0));
        let nc = TOKEN_NC;
        // Fenwicks are allocated at full MAX_SYMS capacity so `grow` can later
        // add columns without reallocating index bounds; only [0, n0) holds 1s.
        let counts = vec![vec![1u64; n0]; nc];
        let mut fw: Vec<Fenwick> = (0..nc).map(|_| Fenwick::zeros(MAX_SYMS)).collect();
        for tree in fw.iter_mut() {
            for i in 0..n0 {
                tree.add(i, 1);
            }
        }
        let total = vec![n0 as u64; nc];
        Self { n: n0, base_n: n0, counts, fw, total, ctx: TOKEN_SENTINEL, log: Vec::new(), start_ctx: TOKEN_SENTINEL }
    }

    /// Alphabet size this model currently allocates counts for.
    pub fn n(&self) -> usize {
        self.n
    }

    /// Grow the alphabet to `n2` (>= current). New symbols receive the same
    /// count-1 prior in every context, so no probabilities go stale.
    pub fn grow(&mut self, n2: usize) {
        if n2 <= self.n {
            return;
        }
        assert!(n2 <= MAX_SYMS);
        for row in self.counts.iter_mut() {
            row.resize(n2, 1);
        }
        for (fw, tot) in self.fw.iter_mut().zip(self.total.iter_mut()) {
            for i in self.n..n2 {
                fw.add(i, 1);
            }
            *tot += (n2 - self.n) as u64;
        }
        self.n = n2;
    }

    /// Start of a new chunk: reset the context to the sentinel (first-token)
    /// and begin a fresh transaction log for this chunk's tokens.
    pub fn begin_chunk(&mut self) {
        self.start_ctx = TOKEN_SENTINEL;
        self.ctx = TOKEN_SENTINEL;
        self.base_n = self.n;
        self.log.clear();
    }

    /// Discard the state changes made since the last `begin_chunk`. After a
    /// rollback the model is exactly as it was before this chunk (including
    /// any `grow` calls), so a chunk stored raw leaves no footprint.
    pub fn rollback_chunk(&mut self) {
        while let Some((c, s, sz)) = self.log.pop() {
            debug_assert!(self.total[c] > 0);
            self.total[c] -= 1;
            self.counts[c][s] -= 1;
            self.fw[c].add(s, u64::MAX); // -1 modulo 2^64 restores the tree
            debug_assert!(self.counts[c][s] >= 1, "count underflow at ({c},{s})");
        }
        // undo any alphabet growth done during this chunk
        if self.n > self.base_n {
            for (fw, tot) in self.fw.iter_mut().zip(self.total.iter_mut()) {
                for i in self.base_n..self.n {
                    fw.add(i, u64::MAX); // remove the count-1 prior again
                }
                *tot -= (self.n - self.base_n) as u64;
            }
            for row in self.counts.iter_mut() {
                row.truncate(self.base_n);
            }
            self.n = self.base_n;
        }
        self.ctx = self.start_ctx;
    }

    /// Encode one token, updating this archive-wide model.
    pub fn encode_token(&mut self, s: usize, enc: &mut Encoder) {
        debug_assert!(s < self.n);
        let c = self.ctx;
        let start = self.fw[c].prefix(s);
        let size = self.counts[c][s];
        enc.encode_step(start, size, self.total[c]);
        self.log.push((c, s, size));
        self.counts[c][s] += 1;
        self.fw[c].add(s, 1);
        self.total[c] += 1;
        self.ctx = token_ctx(s, self.n);
    }

    /// Decode one token using this archive-wide model.
    pub fn decode_token(&mut self, dec: &mut Decoder) -> Result<u16, String> {
        let c = self.ctx;
        let t = self.total[c];
        let th = dec.threshold(t);
        if th >= t {
            return Err("threshold out of range".into());
        }
        let s = self.fw[c].find_gt(th);
        if s >= self.n {
            return Err("symbol out of range".into());
        }
        let start = self.fw[c].prefix(s);
        let size = self.counts[c][s];
        dec.decode_step(start, size, t);
        self.counts[c][s] += 1;
        self.fw[c].add(s, 1);
        self.total[c] += 1;
        self.ctx = token_ctx(s, self.n);
        Ok(s as u16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(tokens: &[u16], n: usize) {
        let mut dec = Vec::new();
        let buf = encode_tokens(tokens, n);
        decode_tokens(&buf, n, tokens.len(), &mut dec).expect("decode");
        assert_eq!(dec, tokens);
    }

    #[test]
    fn empty_stream() {
        roundtrip(&[], 4);
    }

    #[test]
    fn single_symbol_heavy_frequency() {
        let tokens = vec![3u16; 500];
        roundtrip(&tokens, 8);
    }

    #[test]
    fn skewed_symbols_encode_below_1_bit_per_symbol() {
        // one dominant symbol should cost well under a byte
        let mut tokens = Vec::new();
        for _ in 0..4096 {
            tokens.push(7u16);
        }
        for _ in 0..16 {
            tokens.push(0u16);
        }
        let buf = encode_tokens(&tokens, 256);
        assert!(
            buf.len() < 400,
            "expected <400 bytes for 4096 skewed symbols, got {}",
            buf.len()
        );
    }

    #[test]
    fn fenwick_prefix_and_search_agree() {
        let mut fw = Fenwick::zeros(16);
        for i in 0..16 {
            fw.add(i, (i + 1) as u64);
        }
        assert_eq!(fw.prefix(0), 0);
        assert_eq!(fw.prefix(16), 136);
        assert_eq!(fw.prefix(5), 15);
        for val in 0..136 {
            let s = fw.find_gt(val);
            assert!(fw.prefix(s + 1) > (val as u64));
            if s > 0 {
                assert!(fw.prefix(s) <= (val as u64));
            }
        }
    }

#[test]
    fn decoder_rejects_bad_alphabet() {
        assert!(decode_tokens(&[0, 0, 0], 0, 1, &mut vec![]).is_err());
        assert!(decode_tokens(&[0, 0, 0], 5000, 1, &mut vec![]).is_err());
    }

    fn o1_roundtrip(raw: &[u8]) {
        let mut out = Vec::new();
        let buf = encode_bytes_order1(raw);
        decode_bytes_order1(&buf, raw.len(), &mut out).expect("o1 decode");
        assert_eq!(out, raw);
    }

    #[test]
    fn o1_empty_and_small_roundtrip() {
        o1_roundtrip(b"");
        o1_roundtrip(b"a");
        o1_roundtrip(b"abcdabcdabcd");
    }

    #[test]
    fn o1_single_byte_heavy_frequency() {
        let raw: Vec<u8> = vec![b'q'; 4096];
        o1_roundtrip(&raw);
        // a lone repeated byte has no order-1 structure to exploit, so o1 must
        // be no worse than the order-0 baseline on the same alphabet
        let o0 = encode_tokens(&raw.iter().map(|&b| b as u16).collect::<Vec<_>>(), 256).len();
        let o1 = encode_bytes_order1(&raw).len();
        assert!(o1 <= o0 + 2, "o1 ({}) should be <= o0 ({})+2", o1, o0);
    }

    #[test]
    fn o1_beats_order0_on_structured_text() {
        // code-like: `if (` and `return x;` patterns strongly condition next byte
        let mut raw = Vec::new();
        for _ in 0..2000 {
            raw.extend_from_slice(b"if (x == 1) { return y; } else if (z) { return 0; }\n");
        }
        let o0 = encode_tokens(&raw.iter().map(|&b| b as u16).collect::<Vec<_>>(), 256).len();
        let o1 = encode_bytes_order1(&raw).len();
        assert!(
            o1 < o0,
            "expected o1 ({}) < o0 ({}) on structured text",
            o1,
            o0
        );
    }

    fn o1t_roundtrip(tokens: &[u16], n: usize) {
        let mut out = Vec::new();
        let buf = encode_tokens_order1(tokens, n);
        decode_tokens_order1(&buf, n, tokens.len(), &mut out).expect("o1 token decode");
        assert_eq!(out, tokens);
    }

    #[test]
    fn o1_token_roundtrips() {
        o1t_roundtrip(&[], 256);
        o1t_roundtrip(&[0u16, 1, 2, 255], 256);
        let mut tok: Vec<u16> = Vec::new();
        for i in 0..5000 {
            tok.push((i % 10) as u16);
        }
        o1t_roundtrip(&tok, 256);
    }

    #[test]
    fn o1_token_beats_order0_with_rule_grammar() {
        // grammar tokens: frequent literal rule 300 followed by literal 5
        // (e.g., a merged "if " phrase) should encode cheaper in order-1
        let mut tok: Vec<u16> = Vec::new();
        for _ in 0..3000 {
            tok.push(300u16);
            tok.push(5u16);
        }
        let n = 301;
        let o0 = encode_tokens(&tok, n).len();
        let o1 = encode_tokens_order1(&tok, n).len();
        assert!(o1 < o0, "expected o1 token ({}) < o0 ({})", o1, o0);
    }

    #[test]
    fn o1_token_shares_bucketed_rule_contexts() {
        // rule symbols rotate through buckets deterministically, must roundtrip
        let n = 300;
        let mut tok: Vec<u16> = (256..300).collect();
        tok.extend((256..300).rev());
        o1t_roundtrip(&tok, n);
    }
}
