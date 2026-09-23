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
            // pathological overshoot: a corrupt bitstream divided range down to
            // zero. Clamp to 1 so a subsequent decode_step cannot leave the
            // renorm loop stuck on 0 <<= 8 == 0. Clamping only fires on
            // corrupt input; valid streams keep their exact code path.
            self.range = 1;
            0
        } else {
            self.code / self.range
        }
    }

    pub fn decode_step(&mut self, start: u64, size: u64, _total: u64) {
        // Corrupt input can drive start*range above code (see threshold
        // overshoot clamp). Saturating keeps code >= 0 so the renorm
        // loop below still terminates; valid streams never saturate.
        self.code = self.code.saturating_sub(start * self.range);
        self.range = self.range.saturating_mul(size);
        if self.range == 0 {
            // corrupt input wrapped range to zero; renormalisation below would
            // stall on 0 <<= 8. Clamp so the loop terminates.
            self.range = 1;
        }
        let max_shifts = 8; // TOP = 1<<24; 1 << 8*3 already reaches it
        let mut shifts = 0;
        while self.range < TOP && shifts < max_shifts {
            self.range <<= 8;
            self.code = ((self.code << 8) | self.read_byte()) & 0xFFFF_FFFF;
            shifts += 1;
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

// ---------- Persistent model flavors (v3 vs v4) ----------
//
// V3 (legacy) hashes every rule symbol into RULE_CTX buckets. V4 gives the
// hottest rule ids (symbols 0..HOT_RULES above the literal range) an exact
// order-1 context row each and buckets only the cold tail. The mapping is a
// pure function of the symbol (never of the alphabet size), so encoder and
// decoder converge byte-identically in both flavors.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModelMode {
    V3,
    V4,
}

/// Rules (symbols 256..256+HOT_RULES) that get exact context rows in V4.
pub const HOT_RULES: usize = 192;

/// V4 context budget: literals + hot-rule rows + cold rule buckets + sentinel.
pub const V4_NC: usize = 256 + HOT_RULES + RULE_CTX + 1;

fn hot_ctx(sym: usize) -> usize {
    256 + (sym - 256)
}

fn cold_ctx(sym: usize) -> usize {
    // mirrors rule_ctx's multiplier so the cold tail keeps the same cyclic
    // permutation over the RULE_CTX buckets, offset past the hot rows
    256 + HOT_RULES + (((sym - 256 - HOT_RULES) * 0x9E37_79B9) as usize) % RULE_CTX
}

pub fn v4_ctx(sym: usize, n: usize) -> usize {
    if sym < 256 {
        sym
    } else if sym < 256 + HOT_RULES {
        hot_ctx(sym)
    } else {
        cold_ctx(sym)
    }
}

fn nc_for(mode: ModelMode) -> usize {
    match mode {
        ModelMode::V3 => TOKEN_NC,
        ModelMode::V4 => V4_NC,
    }
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
    mode: ModelMode,
}

impl PersistentTokenModel {
    /// Classic (v3) persistent model: rules rotate through RULE_CTX buckets.
    pub fn new(n0: usize) -> Self {
        Self::with_mode(n0, ModelMode::V3)
    }

    /// Persistent model in an explicit flavor: V3 = hashed rule buckets,
    /// V4 = exact contexts for the hottest rules plus buckets for the tail.
    pub fn with_mode(n0: usize, mode: ModelMode) -> Self {
        assert!((256..=MAX_SYMS).contains(&n0));
        let nc = nc_for(mode);
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
        Self {
            n: n0,
            base_n: n0,
            counts,
            fw,
            total,
            ctx: nc - 1,
            log: Vec::new(),
            start_ctx: nc - 1,
            mode,
        }
    }

    /// Map a symbol (0..n) to its order-1 context row in this model's flavor.
    fn ctx_of(&self, s: usize, n: usize) -> usize {
        match self.mode {
            ModelMode::V3 => token_ctx(s, n),
            ModelMode::V4 => v4_ctx(s, n),
        }
    }

    /// Context row used for the first token of a chunk (nothing seen yet).
    fn sentinel(&self) -> usize {
        self.counts.len() - 1
    }

    /// Number of adaptive contexts in the persistent model (literals + rule
    /// buckets + sentinel). Exposed for the ablation harness footprint math.
    pub fn n_contexts(&self) -> usize {
        self.counts.len()
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

    /// GC shrink: drop the dead grammar history by truncating the alphabet to
    /// `n2` (>= 256). Literal symbols (< 256) keep their accumulated counts
    /// everywhere; surviving rule symbols are *re-added* with the count-1 prior
    /// (frequency history is forgotten, the rule definitions are not â€” they
    /// live in grammar.rs). Every tree is rebuilt exactly from the counts rows
    /// so the encoder and decoder converge byte-identically: both sides run
    /// shrink_alphabet with the same n2 at the same chunk boundary.
    pub fn shrink_alphabet(&mut self, n2: usize) {
        assert!((256..=MAX_SYMS).contains(&n2));
        if n2 >= self.n {
            return;
        }
        for row in self.counts.iter_mut() {
            for i in 256..n2 {
                row[i] = 1;
            }
            row.truncate(n2);
        }
        let counts = &self.counts;
        for (c, (fw, tot)) in self.fw.iter_mut().zip(self.total.iter_mut()).enumerate() {
            let row = &counts[c];
            let mut nfw = Fenwick::zeros(MAX_SYMS);
            let mut sum = 0u64;
            for i in 0..n2 {
                nfw.add(i, row[i]);
            }
            for v in &row[..n2] {
                sum += v;
            }
            *fw = nfw;
            *tot = sum;
        }
        self.n = n2;
    }

    /// Start of a new chunk: reset the context to the sentinel (first-token)
    /// and begin a fresh transaction log for this chunk's tokens.
    pub fn begin_chunk(&mut self) {
        let s = self.sentinel();
        self.start_ctx = s;
        self.ctx = s;
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
        self.ctx = self.ctx_of(s, self.n);
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
        self.ctx = self.ctx_of(s as usize, self.n);
        Ok(s as u16)
    }
}

// ---------- Deterministic partial-order blending ----------
//
// Context mixing adds nothing to the *frame*; it only refines the probability
// model. This module implements a lightweight, deterministic blend of two
// contexts: the exact order-1 context (previous token, literals + rule
// buckets) and a partial order-2 "sparse" context (the previous two tokens
// hashed into O2_BUCKETS buckets). Each bucket learns independently *which*
// context predicts better via a win counter, so after a short warmup the
// better context wins per-bucket without any non-determinism: both sides walk
// the same token stream, so the counters converge identically. This maps
// directly from the research survey (PMC-style -- never reset models across
// blocks; blend high-order contexts with a fallback to lower-order evidence)
// while keeping the integer-only, byte-exact determinism contract.

const O2_BUCKETS: usize = 1024;
#[allow(dead_code)]
const O2_MIN_TOTAL: u64 = 4; // order-2 must have evidence before it may win

#[allow(dead_code)] // used by unit tests; wired into the persistent model in a later phase
fn o2_bucket(prev: u16, prev2: u16) -> usize {
    let h = (prev as u32) as u64 * 0x9E37_79B9
        ^ (prev2 as u32) as u64 * 0xBF58_476D;
    (h % O2_BUCKETS as u64) as usize
}

/// Encoder half of the deterministic blend model. Writes arithmetic-coded
/// bytes via `encode_step`, exactly like `encode_tokens_order1`, but orders
/// each token's probability from a mix of the order-1 context and the
/// order-2 bucket context.
#[allow(dead_code)] // unit-test only; wired into the persistent model in a later phase
pub struct BlendEncoder {
    n: usize,
    // order-1: full adaptive tables (TOKEN_NC contexts x alphabet)
    counts: Vec<Vec<u64>>,
    fw: Vec<Fenwick>,
    total: Vec<u64>,
    // order-2: per-bucket adaptive tables (lazily allocated)
    o2_counts: Vec<Vec<u64>>,
    o2_fw: Vec<Fenwick>,
    o2_total: Vec<u64>,
    o2_active: Vec<u64>, // 0 = no evidence, else current O2 total per bucket
    wins_o1: Vec<u64>,
    wins_o2: Vec<u64>,
    ctx: usize,
    prev: u16,
    prev2: u16,
}

#[allow(dead_code)] // unit-test only; wired into the persistent model in a later phase
impl BlendEncoder {
    pub fn new(n: usize) -> Self {
        assert!((2..=MAX_SYMS).contains(&n));
        let nc = TOKEN_NC;
        let counts = vec![vec![1u64; n]; nc];
        let fw: Vec<Fenwick> = (0..nc).map(|_| fenwick_ones(n)).collect();
        let total = vec![n as u64; nc];
        let o2_counts = vec![Vec::new(); O2_BUCKETS];
        let o2_fw = (0..O2_BUCKETS).map(|_| Fenwick::zeros(0)).collect();
        let o2_total = vec![0u64; O2_BUCKETS];
        let o2_active = vec![0u64; O2_BUCKETS];
        let wins_o1 = vec![0u64; O2_BUCKETS];
        let wins_o2 = vec![0u64; O2_BUCKETS];
        Self {
            n,
            counts,
            fw,
            total,
            o2_counts,
            o2_fw,
            o2_total,
            o2_active,
            wins_o1,
            wins_o2,
            ctx: TOKEN_SENTINEL,
            prev: TOKEN_SENTINEL as u16,
            prev2: TOKEN_SENTINEL as u16,
        }
    }

    /// Grow alphabets so the model is printable/decodable on both sides.
    pub fn grow(&mut self, n2: usize) {
        if n2 <= self.n {
            return;
        }
        // Rebuild each order-1 tree from its (resized) count row; new symbols
        // default to the count-1 prior so no probability ever goes stale.
        for (c, fw) in self.fw.iter_mut().enumerate() {
            self.counts[c].resize(n2, 1);
            let mut nw = Fenwick::zeros(MAX_SYMS);
            for i in 0..n2 {
                nw.add(i, self.counts[c][i]);
            }
            *fw = nw;
            self.total[c] += (n2 - self.n) as u64;
        }
        for (row, fw) in self.o2_counts.iter_mut().zip(self.o2_fw.iter_mut()) {
            if !row.is_empty() {
                row.resize(n2, 1);
                let mut nw = Fenwick::zeros(MAX_SYMS);
                for i in 0..n2 {
                    nw.add(i, row[i]);
                }
                *fw = nw;
            }
        }
        self.n = n2;
    }

    /// Encode one token against the current blended context.
    pub fn encode_token(&mut self, token: u16, enc: &mut Encoder) {
        let s = token as usize;
        debug_assert!(s < self.n);
        let c = self.ctx; // order-1 context

        // Probability from the order-1 table.
        let o1_start = self.fw[c].prefix(s);
        let o1_size = self.counts[c][s];
        let o1_total = self.total[c];

        // Order-2 candidate (if this bucket has evidence).
        let bucket = o2_bucket(self.prev, self.prev2);
        let o2_total = self.o2_total[bucket];

        // Deterministic per-bucket winner based on accumulated wins.
        let use_o2 = o2_total >= O2_MIN_TOTAL && self.wins_o2[bucket] > self.wins_o1[bucket];

        if use_o2 {
            let start = self.o2_fw[bucket].prefix(s);
            let size = self.o2_counts[bucket][s];
            enc.encode_step(start, size, o2_total);
        } else {
            enc.encode_step(o1_start, o1_size, o1_total);
        }

        // Update both models (order-2 bucket only once warm).
        self.counts[c][s] += 1;
        self.fw[c].add(s, 1);
        self.total[c] += 1;
        if self.o2_active[bucket] > 0 {
            self.o2_counts[bucket][s] += 1;
            self.o2_fw[bucket].add(s, 1);
            self.o2_total[bucket] += 1;
        }

        // Credit whichever model would have assigned the token the most
        // probability (integer-only comparison, order-independent: both sides
        // see identical counts, so the decision is identical).
        //
        // Compare p_o2(s)/total_o2 vs p_o1(s)/total_o1 by cross-multiplication,
        // evaluated at the post-update state on BOTH sides.
        if self.o2_active[bucket] > 0 {
            let p_o2 = self.o2_counts[bucket][s] * self.total[c];
            let p_o1 = self.counts[c][s] * self.o2_total[bucket];
            if p_o1 > p_o2 {
                self.wins_o1[bucket] = self.wins_o1[bucket].wrapping_add(1);
            } else {
                self.wins_o2[bucket] = self.wins_o2[bucket].wrapping_add(1);
            }
        } else {
            // Both cold: teach the bucket so it can start competing.
            // Lazily initialize the order-2 table for this bucket.
            if self.o2_active[bucket] == 0 {
                self.o2_counts[bucket] = vec![1u64; self.n];
                self.o2_fw[bucket] = fenwick_ones(self.n);
                self.o2_total[bucket] = self.n as u64;
                self.o2_active[bucket] = 1;
            }
            self.o2_counts[bucket][s] += 1;
            self.o2_fw[bucket].add(s, 1);
            self.o2_total[bucket] += 1;
            self.wins_o1[bucket] = self.wins_o1[bucket].wrapping_add(1);
        }

        self.prev2 = self.prev;
        self.prev = token;
        self.ctx = token_ctx(s, self.n);
    }
}

/// Encode a token stream with the deterministic blend model.
#[allow(dead_code)] // unit-test only; wired into the persistent model in a later phase
pub fn encode_tokens_blend(tokens: &[u16], n: usize) -> Vec<u8> {
    let mut model = BlendEncoder::new(n);
    let mut enc = Encoder::new();
    for &t in tokens {
        model.encode_token(t, &mut enc);
    }
    enc.flush()
}

/// Decode a token stream produced by `encode_tokens_blend`.
#[allow(dead_code)] // unit-test only; wired into the persistent model in a later phase
pub fn decode_tokens_blend(
    buf: &[u8],
    n: usize,
    num_tokens: usize,
    out: &mut Vec<u16>,
) -> Result<(), String> {
    if n == 0 || n > MAX_SYMS {
        return Err("bad alphabet size".into());
    }
    let mut model = BlendEncoder::new(n);
    let mut dec = Decoder::new(buf);
    for _ in 0..num_tokens {
        let c = model.ctx;
        let bucket = o2_bucket(model.prev, model.prev2);
        let o2_total = model.o2_total[bucket];
        let use_o2 = o2_total >= O2_MIN_TOTAL && model.wins_o2[bucket] > model.wins_o1[bucket];
        let s = if use_o2 {
            let t = o2_total;
            let th = dec.threshold(t);
            if th >= t {
                return Err("threshold out of range".into());
            }
            let s = model.o2_fw[bucket].find_gt(th);
            if s >= n {
                return Err("symbol out of range".into());
            }
            let start = model.o2_fw[bucket].prefix(s);
            let size = model.o2_counts[bucket][s];
            dec.decode_step(start, size, t);
            s
        } else {
            let t = model.total[c];
            let th = dec.threshold(t);
            if th >= t {
                return Err("threshold out of range".into());
            }
            let s = model.fw[c].find_gt(th);
            if s >= n {
                return Err("symbol out of range".into());
            }
            let start = model.fw[c].prefix(s);
            let size = model.counts[c][s];
            dec.decode_step(start, size, t);
            s
        };
        out.push(s as u16);
        // Mirror the encoder's model updates exactly.
        model.counts[c][s] += 1;
        model.fw[c].add(s, 1);
        model.total[c] += 1;
        if model.o2_active[bucket] > 0 {
            model.o2_counts[bucket][s] += 1;
            model.o2_fw[bucket].add(s, 1);
            model.o2_total[bucket] += 1;
        }
        if model.o2_active[bucket] > 0 {
            let p_o2 = model.o2_counts[bucket][s] * model.total[c];
            let p_o1 = model.counts[c][s] * model.o2_total[bucket];
            if p_o1 > p_o2 {
                model.wins_o1[bucket] = model.wins_o1[bucket].wrapping_add(1);
            } else {
                model.wins_o2[bucket] = model.wins_o2[bucket].wrapping_add(1);
            }
        } else {
            if model.o2_active[bucket] == 0 {
                model.o2_counts[bucket] = vec![1u64; n];
                model.o2_fw[bucket] = fenwick_ones(n);
                model.o2_total[bucket] = n as u64;
                model.o2_active[bucket] = 1;
            }
            model.o2_counts[bucket][s] += 1;
            model.o2_fw[bucket].add(s, 1);
            model.o2_total[bucket] += 1;
            model.wins_o1[bucket] = model.wins_o1[bucket].wrapping_add(1);
        }
        model.prev2 = model.prev;
        model.prev = s as u16;
        model.ctx = token_ctx(s, model.n);
    }
    Ok(())
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

    #[test]
    fn corrupt_overshoot_threshold_never_stalls_renorm() {
        // A corrupt bitstream can drive `range / total` to 0 inside
        // Decoder::threshold(). The decoder must not leave range==0 behind:
        // the following decode_step would then keep `range *= size` at 0 and
        // the renorm loop (`while range < TOP { range <<= 8; }`) would stall
        // forever on 0 <<= 8 == 0. Clamp range to >= 1 so renorm terminates.
        let buf = vec![0u8; 8];
        let mut dec = Decoder::new(&buf);
        let th = dec.threshold(u64::MAX); // range / huge total -> 0
        assert_eq!(th, 0, "overshoot must surface a zero threshold");
        assert!(dec.range >= 1, "range left at {} after overshoot", dec.range);
        // decode_step with any nonzero size must terminate the renorm loop
        dec.decode_step(1, 2, u64::MAX);
        assert!(dec.range >= TOP, "range never renormalised: {}", dec.range);
        assert!(dec.consumed() > 0, "decoder must have consumed input");
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
    fn rule_ctx_stays_in_bucket_range() {
        let n = MAX_SYMS;
        for sym in 256..MAX_SYMS {
            let c = rule_ctx(sym, n);
            assert!(c >= 256, "rule {sym} ctx {c} below literal range");
            assert!(c < TOKEN_NC, "rule {sym} ctx {c} past TOKEN_NC {TOKEN_NC}");
        }
    }

    #[test]
    fn rule_ctx_permutes_all_buckets_for_dense_rules() {
        let n = MAX_SYMS;
        let mut seen = std::collections::HashSet::new();
        for sym in 256..(256 + RULE_CTX) {
            seen.insert(rule_ctx(sym, n));
        }
        assert_eq!(seen.len(), RULE_CTX, "expected all {RULE_CTX} buckets used");
    }

    #[test]
    fn rule_ctx_uses_rule_id_not_alphabet_size() {
        let a = rule_ctx(300, 301);
        let b = rule_ctx(300, MAX_SYMS);
        assert_eq!(a, b, "rule context must not depend on alphabet size");
    }
    #[test]
    fn o1_token_shares_bucketed_rule_contexts() {
        // rule symbols rotate through buckets deterministically, must roundtrip
        let n = 300;
        let mut tok: Vec<u16> = (256..300).collect();
        tok.extend((256..300).rev());
        o1t_roundtrip(&tok, n);
    }

    fn blend_roundtrip(tokens: &[u16], n: usize) {
        let mut out = Vec::new();
        let buf = encode_tokens_blend(tokens, n);
        decode_tokens_blend(&buf, n, tokens.len(), &mut out).expect("blend decode");
        assert_eq!(out, tokens);
    }

    #[test]
    fn blend_empty_and_small_roundtrip() {
        blend_roundtrip(&[], 2);
        blend_roundtrip(&[0u16, 1, 0, 1], 2);
        blend_roundtrip(&[0u16, 255, 256, 1], 300);
    }

    #[test]
    fn blend_large_cyclic_roundtrip() {
        let n = 300;
        let mut tok: Vec<u16> = Vec::new();
        for i in 0..5000 {
            tok.push((i % 10) as u16);
        }
        for i in 0..2000 {
            tok.push(256 + (i % 44));
        }
        blend_roundtrip(&tok, n);
    }

    #[test]
    fn blend_is_deterministic() {
        let n = 300;
        let mut tok: Vec<u16> = Vec::new();
        for i in 0..3000 {
            tok.push((i * 7) % 300);
        }
        let a = encode_tokens_blend(&tok, n);
        let b = encode_tokens_blend(&tok, n);
        assert_eq!(a, b, "blend encode must be byte-deterministic");
    }

    #[test]
    fn blend_beats_order1_on_two_back_pattern() {
        // Stream where the next token depends on TWO tokens back: rows cycle
        // 0..8, each row is followed by a separator then a value decided by
        // the row (even -> 1, odd -> 2). Order-1 sees separator->{{1,2}}
        // (ambiguous), order-2 sees the (row, sep) pair and disambiguates.
        let n = 300;
        let mut tok: Vec<u16> = Vec::new();
        for i in 0..60_000u32 {
            let row = (i % 8) as u16;
            tok.push(row);
            tok.push(100u16);
            tok.push(if row % 2 == 0 { 1 } else { 2 });
            tok.push(99u16);
        }
        let o1 = encode_tokens_order1(&tok, n).len();
        let blend = encode_tokens_blend(&tok, n).len();
        assert!(
            blend < o1,
            "expected blend ({}) < order-1 ({}) on two-back structure",
            blend,
            o1
        );
    }

    #[test]
    fn blend_transitions_from_order1_to_order2_after_evidence() {
        // First only literals stream with NO long-range structure, then switch
        // to a two-back pattern; blend must stay correct (roundtrip) across
        // the transition, not just on uniform data.
        let n = 100;
        let mut tok: Vec<u16> = Vec::new();
        for i in 0..2000 {
            tok.push((i % 3) as u16);
        }
        for i in 0..20_000u32 {
            let row = (i % 4) as u16;
            tok.push(row);
            tok.push(50u16);
            tok.push(if row % 2 == 0 { 7 } else { 8 });
        }
        blend_roundtrip(&tok, n);
    }

    #[test]
    fn blend_no_worse_than_order1_on_skewed_data() {
        // Dominant single token: order-1 already near-optimal; blend must not
        // regress materially (the added order-2 model must not dominate).
        let n = 100;
        let mut tok: Vec<u16> = vec![42u16; 10_000];
        tok.push(3);
        let o1 = encode_tokens_order1(&tok, n).len();
        let blend = encode_tokens_blend(&tok, n).len();
        assert!(
            blend <= o1 + 4,
            "blend ({}) regressed past order-1 ({}) + 4",
            blend,
            o1
        );
    }
}

#[cfg(test)]
mod tests_v4_model {
    use super::*;

    fn encode_tokens_model(tokens: &[u16], n: usize, mode: ModelMode) -> Vec<u8> {
        let mut m = PersistentTokenModel::with_mode(n, mode);
        m.begin_chunk();
        let mut enc = Encoder::new();
        for &t in tokens {
            m.encode_token(t as usize, &mut enc);
        }
        enc.flush()
    }

    fn v4_roundtrip(tokens: &[u16], n: usize) {
        let mut out = Vec::new();
        let mut m = PersistentTokenModel::with_mode(n, ModelMode::V4);
        m.begin_chunk();
        let buf = encode_tokens_model(tokens, n, ModelMode::V4);
        let mut dec = Decoder::new(&buf);
        m.begin_chunk();
        for _ in 0..tokens.len() {
            m.decode_token(&mut dec).expect("v4 decode");
        }
        // decode consumed symbols must match via a fresh walk:
        let mut out2 = Vec::new();
        let mut m2 = PersistentTokenModel::with_mode(n, ModelMode::V4);
        m2.begin_chunk();
        let mut dec2 = Decoder::new(&buf);
        for _ in 0..tokens.len() {
            out2.push(m2.decode_token(&mut dec2).expect("v4 decode"));
        }
        assert_eq!(out2, tokens);
        out.extend_from_slice(&buf);
    }

    #[test]
    fn v4_context_budget_consts() {
        assert_eq!(TOKEN_NC, 265, "v3 budget must stay frozen");
        assert_eq!(V4_NC, 256 + HOT_RULES + RULE_CTX + 1);
        assert_eq!(V4_NC, 457);
        assert!(HOT_RULES >= RULE_CTX);
    }

    #[test]
    fn v4_context_mapping_bounds_and_exactness() {
        // hot rules get exact rows; literals are their own context
        for s in 256..(256 + HOT_RULES) {
            let c = v4_ctx(s, MAX_SYMS);
            assert_eq!(c, 256 + (s - 256), "hot rule {s} mapped to {c}");
        }
        for s in 0..256 {
            assert_eq!(v4_ctx(s, MAX_SYMS), s);
        }
        // cold rules stay inside the bucket tail, never colliding with hot rows
        for s in (256 + HOT_RULES)..2560 {
            let c = v4_ctx(s, MAX_SYMS);
            assert!(
                c >= 256 + HOT_RULES && c < 256 + HOT_RULES + RULE_CTX,
                "cold rule {s} ctx {c} outside bucket tail"
            );
        }
        // context depends only on the symbol, not the alphabet size
        assert_eq!(v4_ctx(300, 301), v4_ctx(300, MAX_SYMS));
        // the sentinel context is not reachable from any symbol
        assert_eq!(v4_ctx(MAX_SYMS - 1, MAX_SYMS).max(0) < V4_NC - 1, true);
    }

    #[test]
    fn v4_cold_rules_permute_all_buckets() {
        let mut seen = std::collections::HashSet::new();
        for s in (256 + HOT_RULES)..(256 + HOT_RULES + RULE_CTX) {
            seen.insert(v4_ctx(s, MAX_SYMS));
        }
        assert_eq!(seen.len(), RULE_CTX, "cold rules must use all {RULE_CTX} buckets");
    }

    #[test]
    fn v4_roundtrips_and_is_byte_deterministic() {
        v4_roundtrip(&[], 256);
        v4_roundtrip(&[0u16, 1, 2, 255], 256);
        let mut tok: Vec<u16> = Vec::new();
        for i in 0..3000u32 {
            tok.push(((i * 7) % 330) as u16);
        }
        v4_roundtrip(&tok, 330);
        let a = encode_tokens_model(&tok, 330, ModelMode::V4);
        let b = encode_tokens_model(&tok, 330, ModelMode::V4);
        assert_eq!(a, b, "v4 encode must be byte-deterministic");
    }

    #[test]
    fn v4_grow_shrink_rollback_are_consistent() {
        let mut m = PersistentTokenModel::with_mode(256, ModelMode::V4);
        m.begin_chunk();
        let mut enc = Encoder::new();
        m.grow(1000);
        for &t in &[260u16, 5u16, 261u16, 7u16, 999u16] {
            m.encode_token(t as usize, &mut enc);
        }
        let blob1 = enc.flush();
        m.rollback_chunk();
        // after rollback the model is pristine: same encode must give same bytes
        let mut m2 = PersistentTokenModel::with_mode(256, ModelMode::V4);
        m2.begin_chunk();
        m2.grow(1000);
        let mut enc2 = Encoder::new();
        for &t in &[260u16, 5u16, 261u16, 7u16, 999u16] {
            m2.encode_token(t as usize, &mut enc2);
        }
        assert_eq!(enc2.flush(), blob1, "rollback must restore exact model state");
        // GC shrink truncates and rebuilds identically on both sides
        let mut e = PersistentTokenModel::with_mode(300, ModelMode::V4);
        e.grow(330);
        e.shrink_alphabet(256);
        let mut d = PersistentTokenModel::with_mode(256, ModelMode::V4);
        let mut enc3 = Encoder::new();
        e.begin_chunk();
        for &t in &[5u16, 9u16, 250u16] {
            e.encode_token(t as usize, &mut enc3);
        }
        let buf = enc3.flush();
        d.begin_chunk();
        let mut dec = Decoder::new(&buf);
        let mut got = Vec::new();
        for _ in 0..3 {
            got.push(d.decode_token(&mut dec).expect("decode"));
        }
        assert_eq!(got, vec![5u16, 9u16, 250u16]);
    }

    #[test]
    fn v4_beats_v3_when_followers_differ_across_rules() {
        // rule ids 256..300 (all HOT). Under v3 the 44 rules collapse into 8
        // shared bucket rows (idx % 8), so a bucket mixes ~6 distinct follower
        // literals (r % 256). v4 gives each rule an exact row, so after warmup
        // the follower is predicted near-deterministically.
        let n = 300;
        let mut tok: Vec<u16> = Vec::new();
        for i in 0..30_000u32 {
            let r = 256 + ((i % 44) as u16);
            tok.push(r);
            tok.push((r % 256) as u16);
        }
        let v3 = encode_tokens_model(&tok, n, ModelMode::V3);
        let v4 = encode_tokens_model(&tok, n, ModelMode::V4);
        assert!(
            v4 < v3,
            "expected v4 ({}) < v3 ({}) when exact rule contexts disambiguate followers", v4.len(), v3.len()
        );
        // v4 must decode the same stream losslessly
        let mut out = Vec::new();
        let mut m = PersistentTokenModel::with_mode(n, ModelMode::V4);
        m.begin_chunk();
        let mut dec = Decoder::new(&v4);
        for _ in 0..tok.len() {
            out.push(m.decode_token(&mut dec).expect("v4 decode"));
        }
        assert_eq!(out, tok);
    }

    #[test]
    fn v4_context_count_is_bounded_for_footprint() {
        // 457 contexts x 4096 symbols x 8 bytes x 2 (counts + Fenwick) fits the
        // ablation footprint budget (~30 MiB).
        let m = PersistentTokenModel::with_mode(MAX_SYMS, ModelMode::V4);
        assert_eq!(m.n_contexts(), V4_NC);
        assert!(m.n_contexts() * MAX_SYMS * 8 * 2 < 64 * 1024 * 1024);
    }
}
