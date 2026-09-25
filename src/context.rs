use crate::{aahl, arith};

const MAX_SYMS: usize = 4096;
const O2_BUCKETS: usize = 1024;
const O2_MIN_SEEN: u64 = 2;
const O1_CONTEXTS: usize = arith::V4_NC;
const SENTINEL: u16 = (O1_CONTEXTS - 1) as u16;
const CONTEXT_MAGIC: [u8; 2] = [0xA0, b'X'];
const EMPTY_BLOCK: [u8; 6] = [0, 0, 0, 0, 0, 0];
const MAX_TOKENS: usize = 1 << 30;

struct AdaptiveModel {
    counts: Vec<u64>,
    fenwick: arith::Fenwick,
    total: u64,
}

impl AdaptiveModel {
    fn new(n: usize) -> Self {
        let mut fenwick = arith::Fenwick::zeros(n);
        for i in 0..n {
            fenwick.add(i, 1);
        }
        Self {
            counts: vec![1; n],
            fenwick,
            total: n as u64,
        }
    }

    fn interval(&self, symbol: usize) -> (u64, u64, u64) {
        (self.fenwick.prefix(symbol), self.counts[symbol], self.total)
    }

    fn update(&mut self, symbol: usize) {
        self.counts[symbol] += 1;
        self.fenwick.add(symbol, 1);
        self.total += 1;
    }
}

struct ContextModel {
    n: usize,
    order1: Vec<AdaptiveModel>,
    order2: Vec<Option<AdaptiveModel>>,
    seen: Vec<u64>,
    wins1: Vec<u64>,
    wins2: Vec<u64>,
    context: usize,
    previous: u16,
    previous2: u16,
}

impl ContextModel {
    fn new(n: usize) -> Self {
        let order1 = (0..O1_CONTEXTS).map(|_| AdaptiveModel::new(n)).collect();
        let order2 = (0..O2_BUCKETS).map(|_| None).collect();
        Self {
            n,
            order1,
            order2,
            seen: vec![0; O2_BUCKETS],
            wins1: vec![0; O2_BUCKETS],
            wins2: vec![0; O2_BUCKETS],
            context: O1_CONTEXTS - 1,
            previous: SENTINEL,
            previous2: SENTINEL,
        }
    }

    fn bucket(&self) -> usize {
        let a = (self.previous as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let b = (self.previous2 as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        let h = a ^ b.rotate_left(29) ^ (self.previous as u64).wrapping_mul(0x94D0_49BB_1331_11EB);
        (h as usize) & (O2_BUCKETS - 1)
    }

    fn use_order2(&self, bucket: usize) -> bool {
        self.seen[bucket] >= O2_MIN_SEEN && self.wins2[bucket] > self.wins1[bucket]
    }

    fn finish(&mut self, symbol: usize, bucket: usize) {
        let existing = self.order2[bucket].is_some();
        if existing {
            let model = self.order2[bucket].as_ref().expect("checked");
            let p1 = self.order1[self.context].counts[symbol] as u128 * model.total as u128;
            let p2 = model.counts[symbol] as u128 * self.order1[self.context].total as u128;
            if p2 > p1 {
                self.wins2[bucket] = self.wins2[bucket].saturating_add(1);
            } else {
                self.wins1[bucket] = self.wins1[bucket].saturating_add(1);
            }
            self.order2[bucket]
                .as_mut()
                .expect("checked")
                .update(symbol);
        } else {
            let mut model = AdaptiveModel::new(self.n);
            model.update(symbol);
            self.order2[bucket] = Some(model);
        }
        self.order1[self.context].update(symbol);
        self.seen[bucket] = self.seen[bucket].saturating_add(1);
        self.previous2 = self.previous;
        self.previous = symbol as u16;
        self.context = arith::v4_ctx(symbol, self.n);
    }

    fn encode_token(&mut self, symbol: u16, encoder: &mut arith::Encoder) {
        let s = symbol as usize;
        let bucket = self.bucket();
        let (start, size, total) = if self.use_order2(bucket) {
            self.order2[bucket]
                .as_ref()
                .expect("order2 selected")
                .interval(s)
        } else {
            self.order1[self.context].interval(s)
        };
        encoder.encode_step(start, size, total);
        self.finish(s, bucket);
    }

    fn decode_token(&mut self, decoder: &mut arith::Decoder) -> Result<u16, String> {
        let bucket = self.bucket();
        let symbol = if self.use_order2(bucket) {
            let model = self.order2[bucket].as_ref().expect("order2 selected");
            let threshold = decoder.threshold(model.total);
            if threshold >= model.total {
                return Err("context: order2 threshold out of range".into());
            }
            let symbol = model.fenwick.find_gt(threshold);
            if symbol >= self.n {
                return Err("context: order2 symbol out of range".into());
            }
            let (start, size, total) = model.interval(symbol);
            decoder.decode_step(start, size, total);
            symbol
        } else {
            let model = &self.order1[self.context];
            let threshold = decoder.threshold(model.total);
            if threshold >= model.total {
                return Err("context: order1 threshold out of range".into());
            }
            let symbol = model.fenwick.find_gt(threshold);
            if symbol >= self.n {
                return Err("context: order1 symbol out of range".into());
            }
            let (start, size, total) = model.interval(symbol);
            decoder.decode_step(start, size, total);
            symbol
        };
        self.finish(symbol, bucket);
        Ok(symbol as u16)
    }
}

fn validate_alphabet(n: usize) -> Result<(), String> {
    if !(2..=MAX_SYMS).contains(&n) {
        return Err(format!("context: alphabet size {n} out of range"));
    }
    Ok(())
}

pub fn encode_tokens(tokens: &[u16], n: usize) -> Result<Vec<u8>, String> {
    validate_alphabet(n)?;
    for &token in tokens {
        if token as usize >= n {
            return Err(format!("context: symbol {token} outside alphabet {n}"));
        }
    }
    if tokens.is_empty() {
        return Ok(Vec::new());
    }
    let mut model = ContextModel::new(n);
    let mut encoder = arith::Encoder::new();
    for &token in tokens {
        model.encode_token(token, &mut encoder);
    }
    Ok(encoder.flush())
}

pub fn decode_tokens(blob: &[u8], n: usize, num_tokens: usize) -> Result<Vec<u16>, String> {
    validate_alphabet(n)?;
    if num_tokens > MAX_TOKENS {
        return Err("context: token count too large".into());
    }
    if num_tokens == 0 {
        return Ok(Vec::new());
    }
    if blob.len() < 5 {
        return Err("context: arithmetic payload too short".into());
    }
    let mut decoder = arith::Decoder::new(blob);
    let mut model = ContextModel::new(n);
    let mut out = Vec::new();
    for _ in 0..num_tokens {
        out.push(model.decode_token(&mut decoder)?);
    }
    if decoder.consumed() > blob.len() {
        return Err("context: truncated arithmetic payload".into());
    }
    Ok(out)
}

fn checksum(bytes: &[u8]) -> u32 {
    let mut value = 0x811C_9DC5u32;
    for &byte in bytes {
        value ^= byte as u32;
        value = value.wrapping_mul(0x0100_0193);
    }
    value
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    if offset + 4 > bytes.len() {
        return Err("context: truncated length field".into());
    }
    Ok(u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]))
}

pub fn encode(raw: &[u8]) -> Result<Vec<u8>, String> {
    if raw.is_empty() {
        return Ok(EMPTY_BLOCK.to_vec());
    }
    if raw.len() > u32::MAX as usize {
        return Err("context: input too large".into());
    }
    let initial: Vec<u16> = raw.iter().map(|&byte| byte as u16).collect();
    let (tokens, rules) = aahl::fold(initial);
    let n = 256 + rules.len();
    validate_alphabet(n)?;
    let blob = encode_tokens(&tokens, n)?;
    let mut block = Vec::with_capacity(18 + rules.len() * 4 + blob.len());
    block.extend_from_slice(&CONTEXT_MAGIC);
    block.extend_from_slice(&(rules.len() as u32).to_le_bytes());
    for &(left, right) in &rules {
        block.extend_from_slice(&left.to_le_bytes());
        block.extend_from_slice(&right.to_le_bytes());
    }
    block.extend_from_slice(&(tokens.len() as u32).to_le_bytes());
    block.extend_from_slice(&(blob.len() as u32).to_le_bytes());
    block.extend_from_slice(&blob);
    let sum = checksum(&block);
    block.extend_from_slice(&sum.to_le_bytes());
    Ok(block)
}

pub fn decode(block: &[u8], expected_len: usize) -> Result<Vec<u8>, String> {
    if block == EMPTY_BLOCK {
        if expected_len == 0 {
            return Ok(Vec::new());
        }
        return Err("context: empty block has nonzero expected length".into());
    }
    if expected_len == 0 {
        return Err("context: nonempty block has zero expected length".into());
    }
    if block.len() < 22 || block[0..2] != CONTEXT_MAGIC {
        return Err("context: invalid block header".into());
    }
    let body_len = block.len() - 4;
    let stored = read_u32(block, body_len)?;
    if checksum(&block[..body_len]) != stored {
        return Err("context: checksum mismatch".into());
    }
    let rule_count = read_u32(block, 2)? as usize;
    if rule_count > MAX_SYMS - 256 {
        return Err("context: rule count out of range".into());
    }
    let rules_start: usize = 6;
    let rules_end = rules_start
        .checked_add(
            rule_count
                .checked_mul(4)
                .ok_or("context: rule size overflow")?,
        )
        .ok_or("context: rule size overflow")?;
    if rules_end > body_len {
        return Err("context: truncated rule table".into());
    }
    let mut rules = Vec::with_capacity(rule_count);
    for i in 0..rule_count {
        let offset = rules_start + i * 4;
        let left = u16::from_le_bytes([block[offset], block[offset + 1]]);
        let right = u16::from_le_bytes([block[offset + 2], block[offset + 3]]);
        rules.push((left, right));
    }
    let token_count = read_u32(block, rules_end)? as usize;
    let blob_len = read_u32(block, rules_end + 4)? as usize;
    if token_count > MAX_TOKENS || token_count > expected_len {
        return Err("context: token count out of range".into());
    }
    let payload_start = rules_end + 8;
    let payload_end = payload_start
        .checked_add(blob_len)
        .ok_or("context: payload size overflow")?;
    if payload_end != body_len {
        return Err("context: payload length mismatch".into());
    }
    let n = 256 + rule_count;
    let tokens = decode_tokens(&block[payload_start..payload_end], n, token_count)?;
    let raw = aahl::unfold(&tokens, &rules, expected_len)
        .map_err(|error| format!("context: unfold: {error}"))?;
    if raw.len() != expected_len {
        return Err("context: decoded length mismatch".into());
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(raw: &[u8]) {
        let block = encode(raw).expect("encode");
        let got = decode(&block, raw.len()).expect("decode");
        assert_eq!(got, raw);
    }

    #[test]
    fn roundtrip_empty() {
        roundtrip(b"");
    }

    #[test]
    fn roundtrip_structured() {
        let mut raw = Vec::new();
        for i in 0..5000u32 {
            raw.extend_from_slice(
                format!("{i}: if (value == 1) {{ return value + 2; }}\n").as_bytes(),
            );
        }
        roundtrip(&raw);
    }

    #[test]
    fn deterministic_encoding() {
        let raw = b"deterministic context model";
        assert_eq!(encode(raw).unwrap(), encode(raw).unwrap());
    }

    #[test]
    fn order_two_context_beats_order_one() {
        let n = 300;
        let mut tokens = Vec::new();
        for i in 0..60_000u32 {
            let row = (i % 8) as u16;
            tokens.push(row);
            tokens.push(100);
            tokens.push(if row.is_multiple_of(2) { 1 } else { 2 });
            tokens.push(99);
        }
        let a = encode_tokens(&tokens, n).unwrap();
        let b = crate::arith::encode_tokens_order1(&tokens, n);
        assert!(a.len() < b.len(), "context {} order1 {}", a.len(), b.len());
        let got = decode_tokens(&a, n, tokens.len()).unwrap();
        assert_eq!(got, tokens);
    }

    #[test]
    fn rejects_truncation_and_corruption() {
        let raw = b"context block validation input";
        let block = encode(raw).unwrap();
        for cut in 0..block.len() {
            assert!(decode(&block[..cut], raw.len()).is_err(), "cut {cut}");
        }
        let mut bad = block;
        let i = bad.len() / 2;
        bad[i] ^= 0x80;
        assert!(decode(&bad, raw.len()).is_err());
    }
}
