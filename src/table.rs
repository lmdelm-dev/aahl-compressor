//! Phase B: reversible, self-describing, per-slice columnar table transform (v5).
//!
//! A *grid slice* (a data chunk) is a byte region where every complete,
//! newline-terminated line between the first and the last newline splits on
//! one of `, ; \t |` into the same number of cells. When that structural
//! detector fires, this module rewrites the slice with a *pure, stateless*
//! transform:
//!
//!   - the partial head line (bytes up to and including the first newline) is
//!     stored verbatim, so mid-row chunk boundaries are exact,
//!   - every complete line between the first and last newline is split into
//!     cells and regrouped into contiguous per-column value streams (plus a
//!     per-row length stream for variable-width columns), so the persistent
//!     grammar sees columns, not rows,
//!   - the partial tail line (bytes after the last newline) is stored verbatim,
//!   - numeric columns that parse cleanly (canonical i64, or YYYY-MM-DD dates)
//!     are delta-coded **only when the measured delta bytes are strictly
//!     smaller** than storing the values verbatim (per-column, per-slice).
//!
//! The resulting transform stream `T` is wrapped in a self-describing block
//!
//! ```text
//!   [0xA0 b'T'] meta(version, kind, delimiters, geometry, per-column
//!                    descriptors, t_len) inner_block
//! ```
//!
//! where `inner_block` is any normal codec block (grammar 'G' or stateless)
//! produced by `compress_block` over `T`. On extraction the container reader
//! first decodes the inner block to `T`, then runs the exact inverse, then the
//! caller verifies the original blake3 + length, so every archive-level
//! integrity guarantee applies to transformed slices unchanged.
//!
//! Selection (B2) is the *measured* rule, not a heuristic: the wrapper plus the
//! real encoded size of `T` competes against the real stateless encoding of the
//! original slice, and the transform wins only when strictly smaller. Because
//! `prepare` is a pure function of the slice bytes (no grammar state, no wall
//! clock, no worker count), j1/j4/j8 archives are byte-identical.
//!
//! Bounds: every decode path validates against the slice's own declared
//! geometry and the container `unpacked_len`; hostile metadata can cause
//! errors but never panics and never allocates beyond the input-derived caps.
//! The transform is a *reversible* lossless rearrangement - no LZ-family
//! compression engine is introduced.

use anyhow::{bail, Result};
use std::collections::HashMap;

pub const T_TAG: [u8; 2] = [0xA0, b'T'];
pub const WRAP_VERSION: u8 = 1;
pub const KIND_GRID: u8 = 0;
/// Head/tail (partial line) blob cap: a mid-row edge longer than this declines
/// the transform (a table line this long would never columnarize well anyway).
pub const MAX_EDGE_BYTES: usize = 1 << 16;
pub const MAX_COLS: u16 = 4096;
/// Slices below this size never trigger the detector (a 64-byte "table" is a
/// toy; the margin also keeps the trial-encode oracle cheap per chunk).
pub const MIN_GRID_BYTES: usize = 64;
pub const DELIMS: [u8; 4] = [b',', b';', b'\t', b'|'];
/// Delta modes. 0 = store values verbatim. 1 = canonical i64 deltas rendered
/// back exactly. 2 = YYYY-MM-DD day numbers (timestamps) deltas.
pub const DMODE_NONE: u8 = 0;
pub const DMODE_INT: u8 = 1;
pub const DMODE_DATE: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColDesc {
    /// true: raw values are stored at a fixed width, no length stream.
    pub fixed: bool,
    /// fixed-width byte length (only meaningful when `fixed`).
    pub width: u32,
    /// DMODE_NONE / DMODE_INT / DMODE_DATE (delta coding active for the column).
    pub dmode: u8,
}

impl ColDesc {
    fn flags(&self) -> u8 {
        debug_assert!(self.dmode <= 3);
        (if self.fixed { 1 } else { 0 }) | (self.dmode << 1)
    }
    fn from_flags(f: u8) -> Result<ColDesc> {
        if f & 0xF0 != 0 {
            bail!("unknown column descriptor flags 0x{f:02x}");
        }
        let dmode = (f >> 1) & 0x03;
        if dmode > DMODE_DATE {
            bail!("unknown delta mode {dmode}");
        }
        Ok(ColDesc { fixed: f & 1 != 0, width: 0, dmode })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrapMeta {
    pub row_delim: u8,
    pub col_delim: u8,
    pub n_rows: u32,
    pub n_cols: u16,
    /// length of the transform stream T (inner block decodes to exactly this)
    pub t_len: u32,
    pub head_len: u32,
    pub tail_len: u32,
    pub cols: Vec<ColDesc>,
}

impl WrapMeta {
    /// Serialized metadata byte length (the fixed 22-byte core + descriptors).
    pub fn meta_bytes(&self) -> usize {
        1 + 1 + 1 + 1 + 2 + 4 + 4 + 4 + 4 + self.cols.len()
            + self.cols.iter().filter(|c| c.fixed).count() * 4
    }
    /// Full wrapper prefix length: tag (2) + metadata.
    pub fn wrapper_len(&self) -> usize {
        2 + self.meta_bytes()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransformPlan {
    pub t_stream: Vec<u8>,
    pub meta: WrapMeta,
}

/// Measurement-gate counters (Phase B diagnostics behind `create --table-stats`).
/// Like ParStats, these never influence output bytes or determinism.
#[derive(Default, Clone)]
pub struct TableStats {
    pub chunks_total: u64,
    pub grids_found: u64,
    pub transforms_chosen: u64,
    pub raw_oracle_bytes: u64,
    pub t_side_bytes: u64,
    pub oracle_us: u64,
    pub delta_columns: u64,
    pub grid_rows: u64,
}

// ---------------------------------------------------------------------------
// varint / zigzag helpers (LEB128-style; bounded and deterministic)
// ---------------------------------------------------------------------------

fn uv_len(v: u64) -> usize {
    if v < (1 << 7) { 1 } else if v < (1 << 14) { 2 } else if v < (1 << 21) { 3 }
    else if v < (1 << 28) { 4 } else if v < (1 << 35) { 5 } else if v < (1 << 42) { 6 }
    else if v < (1 << 49) { 7 } else if v < (1 << 56) { 8 } else if v < (1 << 63) { 9 } else { 10 }
}

fn zz_len(v: i64) -> usize {
    uv_len(((v.wrapping_shl(1)) ^ (v >> 63)) as u64)
}

fn put_uvarint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn put_zigzag(out: &mut Vec<u8>, v: i64) {
    put_uvarint(out, ((v.wrapping_shl(1)) ^ (v >> 63)) as u64);
}

/// Read a uvarint within `[pos, end)`; `pos` advances past it. Never reads
/// past `end`; repeats limit 10 bytes, so a hostile block cannot loop.
fn read_uvarint(b: &[u8], pos: &mut usize, end: usize) -> Result<u64> {
    let mut v: u64 = 0;
    let mut shift: u32 = 0;
    for _ in 0..10 {
        if *pos >= end {
            bail!("varint truncated");
        }
        let byte = b[*pos];
        *pos += 1;
        v |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
    }
    bail!("varint too long");
}

fn read_zigzag(b: &[u8], pos: &mut usize, end: usize) -> Result<i64> {
    let u = read_uvarint(b, pos, end)?;
    Ok(((u >> 1) as i64) ^ -((u & 1) as i64))
}
// ---------------------------------------------------------------------------
// numeric / date eligibility (exact-roundtrip rules)
// ---------------------------------------------------------------------------

/// Canonical i64 parse: the cell must be a plain decimal integer whose
/// `to_string()` reproduces the cell bytes exactly ("007", "+1", "-0" decline).
fn parse_int(c: &[u8]) -> Option<i64> {
    let s = std::str::from_utf8(c).ok()?;
    let v: i64 = s.parse().ok()?;
    if v.to_string() == s { Some(v) } else { None }
}

/// Howard Hinnant civil <-> days-from-epoch conversions (no chrono dependency).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn render_date(z: i64) -> Option<[u8; 10]> {
    let (y, m, d) = civil_from_days(z);
    if !(1800..=2400).contains(&y) {
        return None;
    }
    let mut out = [b'0'; 10];
    let ds = format!("{y:04}-{m:02}-{d:02}");
    out.copy_from_slice(ds.as_bytes());
    Some(out)
}

fn parse_date(c: &[u8]) -> Option<i64> {
    if c.len() != 10 || c[4] != b'-' || c[7] != b'-' {
        return None;
    }
    let dg = |i: usize| -> Option<i64> {
        let b = c[i];
        if b.is_ascii_digit() { Some((b - b'0') as i64) } else { None }
    };
    let y = dg(0)? * 1000 + dg(1)? * 100 + dg(2)? * 10 + dg(3)?;
    let m = dg(5)? * 10 + dg(6)?;
    let d = dg(8)? * 10 + dg(9)?;
    if !(1800..=2400).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// All cells parse as canonical i64.
fn int_eligible(cells: &[&[u8]]) -> bool {
    cells.iter().all(|c| parse_int(c).is_some())
}

/// All cells parse as YYYY-MM-DD and render back byte-exactly.
fn date_eligible(cells: &[&[u8]]) -> bool {
    cells.iter().all(|c| match parse_date(c) {
        Some(z) => render_date(z).map(|r| r[..] == **c).unwrap_or(false),
        None => false,
    })
}

/// Per-row deltas for an eligible column: value 0 stays verbatim, thereafter
/// the difference from the previous row (wrapping i64 arithmetic, consistent
/// both ways). None when the mode is inapplicable.
fn delta_values(cells: &[&[u8]], dmode: u8) -> Option<Vec<i64>> {
    if cells.is_empty() || dmode == DMODE_NONE {
        return None;
    }
    let mut out = Vec::with_capacity(cells.len());
    let mut prev: i64 = 0;
    for (i, c) in cells.iter().enumerate() {
        let v = match dmode {
            DMODE_INT => parse_int(c)?,
            DMODE_DATE => parse_date(c)?,
            _ => return None,
        };
        let d = if i == 0 { v } else { v.wrapping_sub(prev) };
        prev = v;
        out.push(d);
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// per-column decision (used by both the transform builder and `aahl char`)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColDecision {
    pub fixed: bool,
    pub width: u32,
    pub dmode: u8,
    pub raw_profile: usize,
    pub delta_profile: usize,
}

/// Deterministic per-column choice. Independent of everything except the cell
/// bytes: raw verbatim storage versus delta coding, with fixed-width detection.
/// The smaller *real byte layout* wins (B3: measured, never a guess).
pub fn column_decision(cells: &[&[u8]]) -> ColDecision {
    let width = cells.first().map(|c| c.len()).unwrap_or(0);
    let fixed = cells.iter().all(|c| c.len() == width);
    let mut raw_profile = if fixed {
        0
    } else {
        cells.iter().map(|c| uv_len(c.len() as u64)).sum::<usize>()
    };
    raw_profile += cells.iter().map(|c| c.len()).sum::<usize>();

    let mut dmode = DMODE_NONE;
    if int_eligible(cells) {
        dmode = DMODE_INT;
    } else if date_eligible(cells) {
        dmode = DMODE_DATE;
    }
    let mut delta_profile = usize::MAX;
    if dmode != DMODE_NONE {
        if let Some(ds) = delta_values(cells, dmode) {
            let value_bytes: usize = ds.iter().map(|&d| zz_len(d)).sum();
            let length_bytes: usize = ds.iter().map(|&d| uv_len(zz_len(d) as u64)).sum();
            delta_profile = value_bytes + length_bytes;
        }
    }
    ColDecision { fixed, width: width as u32, dmode, raw_profile, delta_profile }
}
// ---------------------------------------------------------------------------
// structural detector + columnar builder (the "candidate")
// ---------------------------------------------------------------------------

/// Pure detector + transform builder. Returns None when the slice is not a
/// uniform grid (prose, binary, random, ragged), or when edge blobs exceed
/// caps. The oracle (real encoding of both candidates) lives in `prepare`.
fn candidate(raw: &[u8]) -> Result<Option<TransformPlan>> {
    if raw.len() < MIN_GRID_BYTES {
        return Ok(None);
    }
    let f = match raw.iter().position(|&c| c == b'\n') {
        Some(x) => x,
        None => return Ok(None),
    };
    let l = match raw.iter().rposition(|&c| c == b'\n') {
        Some(x) => x,
        None => return Ok(None),
    };
    if f == l {
        return Ok(None);
    }
    let head = &raw[..=f];
    let tail = &raw[l + 1..];
    if head.len() > MAX_EDGE_BYTES || tail.len() > MAX_EDGE_BYTES {
        return Ok(None);
    }
    let full = &raw[f + 1..l];
    if full.is_empty() {
        return Ok(None);
    }
    let lines: Vec<&[u8]> = full.split(|&c| c == b'\n').collect();
    if lines.is_empty() {
        return Ok(None);
    }

    // first delimiter (of `, ; \t |`) with a uniform per-line count in [1, MAX_COLS-1]
    let mut chosen: Option<(u8, u16)> = None;
    for &d in &DELIMS {
        let mut count: Option<u16> = None;
        let mut uniform = true;
        for ln in &lines {
            let c = ln.iter().filter(|&&b| b == d).count();
            if c == 0 || c > (u32::from(MAX_COLS) - 1) as usize {
                uniform = false;
                break;
            }
            match count {
                None => count = Some(c as u16),
                Some(prev) if prev == c as u16 => {}
                _ => {
                    uniform = false;
                    break;
                }
            }
        }
        if uniform {
            if let Some(c) = count {
                chosen = Some((d, c + 1));
                break;
            }
        }
    }
    let (col_delim, n_cols) = match chosen {
        Some(x) => x,
        None => return Ok(None),
    };
    if n_cols < 2 {
        return Ok(None);
    }
    let n_rows = lines.len() as u32;

    // split every full line into cells, column-major
    let mut columns: Vec<Vec<&[u8]>> = vec![Vec::with_capacity(lines.len()); n_cols as usize];
    for ln in &lines {
        let mut start = 0usize;
        let mut done = 0usize;
        for (i, &b) in ln.iter().enumerate() {
            if b == col_delim {
                if done >= n_cols as usize - 1 {
                    return Ok(None); // extra delimiter: decline defensively
                }
                columns[done].push(&ln[start..i]);
                done += 1;
                start = i + 1;
            }
        }
        if done + 1 != n_cols as usize {
            return Ok(None); // internal inconsistency: decline (cannot happen)
        }
        columns[done].push(&ln[start..]);
    }

    // assemble T: head + (per column: lengths if variable, then values) + tail
    let mut t: Vec<u8> = Vec::with_capacity(raw.len() + (n_rows as usize) * 4 + 16);
    t.extend_from_slice(head);
    let mut descs: Vec<ColDesc> = Vec::with_capacity(n_cols as usize);
    for col in &columns {
        let dec = column_decision(col);
        let use_delta = dec.dmode != DMODE_NONE && dec.delta_profile < dec.raw_profile;
        let fixed = dec.fixed && !use_delta;
        if fixed {
            for c in col.iter() {
                t.extend_from_slice(c);
            }
        } else if use_delta {
            let ds = delta_values(col, dec.dmode).expect("decided with a delta mode");
            for d in &ds {
                put_uvarint(&mut t, zz_len(*d) as u64);
            }
            for d in &ds {
                put_zigzag(&mut t, *d);
            }
        } else {
            for c in col.iter() {
                put_uvarint(&mut t, c.len() as u64);
            }
            for c in col.iter() {
                t.extend_from_slice(c);
            }
        }
        descs.push(ColDesc {
            fixed,
            width: if fixed { dec.width } else { 0 },
            dmode: if use_delta { dec.dmode } else { DMODE_NONE },
        });
    }
    t.extend_from_slice(tail);

    let meta = WrapMeta {
        row_delim: b'\n',
        col_delim,
        n_rows,
        n_cols,
        t_len: t.len() as u32,
        head_len: head.len() as u32,
        tail_len: tail.len() as u32,
        cols: descs,
    };
    Ok(Some(TransformPlan { t_stream: t, meta }))
}

// ---------------------------------------------------------------------------
// wrapper (de)serialization
// ---------------------------------------------------------------------------

pub fn wrap_block(meta: &WrapMeta, inner: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(meta.wrapper_len() + inner.len());
    out.extend_from_slice(&T_TAG);
    out.push(WRAP_VERSION);
    out.push(KIND_GRID);
    out.push(meta.row_delim);
    out.push(meta.col_delim);
    out.extend_from_slice(&meta.n_cols.to_le_bytes());
    out.extend_from_slice(&meta.n_rows.to_le_bytes());
    out.extend_from_slice(&meta.t_len.to_le_bytes());
    out.extend_from_slice(&meta.head_len.to_le_bytes());
    out.extend_from_slice(&meta.tail_len.to_le_bytes());
    for c in &meta.cols {
        out.push(c.flags());
        if c.fixed {
            out.extend_from_slice(&c.width.to_le_bytes());
        }
    }
    out.extend_from_slice(inner);
    Ok(out)
}

/// If `packed` is a table block, parse and return (meta, inner_offset);
/// otherwise Ok(None). All reads bounded; hostile bytes yield Err, never panic.
pub fn try_unwrap(packed: &[u8]) -> Result<Option<(WrapMeta, usize)>> {
    if packed.len() < 2 || packed[0] != T_TAG[0] || packed[1] != T_TAG[1] {
        return Ok(None);
    }
    let mut p = 2usize;
    let need = |p: usize, n: usize, len: usize| -> Result<()> {
        if p.checked_add(n).map(|e| e > len).unwrap_or(true) {
            bail!("table wrapper truncated");
        }
        Ok(())
    };
    need(p, 22, packed.len())?;
    let version = packed[p];
    p += 1;
    if version != WRAP_VERSION {
        bail!("unsupported table wrapper version {version}");
    }
    let kind = packed[p];
    p += 1;
    if kind != KIND_GRID {
        bail!("unsupported table kind {kind}");
    }
    let row_delim = packed[p];
    p += 1;
    let col_delim = packed[p];
    p += 1;
    let n_cols = u16::from_le_bytes(packed[p..p + 2].try_into().unwrap());
    p += 2;
    if !(2..=MAX_COLS).contains(&n_cols) {
        bail!("implausible table width {n_cols}");
    }
    let n_rows = u32::from_le_bytes(packed[p..p + 4].try_into().unwrap());
    p += 4;
    if n_rows == 0 {
        bail!("table with zero rows");
    }
    let t_len = u32::from_le_bytes(packed[p..p + 4].try_into().unwrap());
    p += 4;
    let head_len = u32::from_le_bytes(packed[p..p + 4].try_into().unwrap());
    p += 4;
    let tail_len = u32::from_le_bytes(packed[p..p + 4].try_into().unwrap());
    p += 4;
    if (t_len as u64) < (head_len as u64).saturating_add(tail_len as u64) {
        bail!("table stream shorter than its edges");
    }
    // per-column descriptors
    let mut cols = Vec::with_capacity(n_cols as usize);
    for _ in 0..n_cols {
        if p >= packed.len() {
            bail!("descriptor truncated");
        }
        let mut d = ColDesc::from_flags(packed[p])?;
        p += 1;
        if d.fixed {
            need(p, 4, packed.len())?;
            d.width = u32::from_le_bytes(packed[p..p + 4].try_into().unwrap());
            p += 4;
        }
        cols.push(d);
    }
    if p >= packed.len() {
        bail!("table block has no inner stream");
    }
    Ok(Some((WrapMeta { row_delim, col_delim, n_rows, n_cols, t_len, head_len, tail_len, cols }, p)))
}
// ---------------------------------------------------------------------------
// measured selection (B2) - the container-level entry point
// ---------------------------------------------------------------------------

/// Deterministic chunk preparation: `candidate` + oracle. Returns
/// Some(plan) when the wrapped-columnar stream encodes strictly smaller than
/// the best stateless encoding of the original slice; None otherwise (the
/// caller then feeds the original slice to the grammar exactly like v4).
/// `stats` is the measurement-only gate (never affects output bytes).
pub fn prepare(raw: &[u8], mut stats: Option<&mut TableStats>) -> Result<Option<TransformPlan>> {
    let t0 = std::time::Instant::now();
    if let Some(s) = stats.as_deref_mut() {
        s.chunks_total += 1;
    }
    let Some(plan) = candidate(raw)? else {
        return Ok(None);
    };
    let a = crate::aahl::compress_block(raw).len();
    let b = plan.meta.wrapper_len() + crate::aahl::compress_block(&plan.t_stream).len();
    let chosen = b < a;
    if let Some(s) = stats.as_deref_mut() {
        s.grids_found += 1;
        s.raw_oracle_bytes += a as u64;
        s.oracle_us += t0.elapsed().as_micros() as u64;
        if chosen {
            s.transforms_chosen += 1;
            s.t_side_bytes += b as u64;
            s.delta_columns += plan.meta.cols.iter().filter(|c| c.dmode != DMODE_NONE).count() as u64;
            s.grid_rows += plan.meta.n_rows as u64;
        }
    }
    Ok(if chosen { Some(plan) } else { None })
}

// ---------------------------------------------------------------------------
// exact inverse
// ---------------------------------------------------------------------------

/// Rebuild the original slice from a decoded transform stream. `expected_len`
/// is the container's `unpacked_len`; every allocation and loop is bounded by
/// `expected_len` and the T stream itself with checked arithmetic.
pub fn inverse(t: &[u8], meta: &WrapMeta, expected_len: usize) -> Result<Vec<u8>> {
    if t.len() as u32 != meta.t_len {
        bail!("transform stream length mismatch: {} != {}", t.len(), meta.t_len);
    }
    let n_cols = meta.n_cols as u64;
    let n_rows = meta.n_rows as u64;
    // geometry cap: each row emits at least n_cols bytes in the output.
    if n_rows.checked_mul(n_cols).map(|v| v > expected_len as u64).unwrap_or(true) {
        bail!("implausible table geometry");
    }
    let head_len = meta.head_len as u64;
    let tail_len = meta.tail_len as u64;
    let t_len = meta.t_len as u64;
    if head_len + tail_len > t_len {
        bail!("table edges overlap stream");
    }
    let col_start = head_len as usize;
    let col_end = (t_len - tail_len) as usize;
    if col_end < col_start {
        bail!("table edges overlap stream");
    }
    if meta.cols.len() != n_cols as usize {
        bail!("column count mismatch");
    }

    // Phase A: per column, record the value-region base (where its values
    // begin) and, for variable columns, the per-row value offsets/lengths.
    // All reads bounded by col_end.
    let mut p = col_start;
    let mut value_bases: Vec<usize> = Vec::with_capacity(n_cols as usize);
    let mut storage: Vec<Vec<u64>> = Vec::with_capacity(n_cols as usize);
    let mut lens: Vec<Vec<u32>> = Vec::with_capacity(n_cols as usize);
    for c in &meta.cols {
        if c.fixed {
            let sz = n_rows
                .checked_mul(c.width as u64)
                .ok_or_else(|| anyhow::anyhow!("width overflow"))?;
            if sz > (col_end - p) as u64 {
                bail!("fixed column overruns stream");
            }
            value_bases.push(p); // fixed: values begin at column start (no length stream)
            p += sz as usize;
            storage.push(Vec::new());
            lens.push(Vec::new());
        } else {
            let mut offs: Vec<u64> = Vec::with_capacity(n_rows as usize);
            let mut lens_c: Vec<u32> = Vec::with_capacity(n_rows as usize);
            let mut sum: u64 = 0;
            let mut prev: u64 = 0;
            for _ in 0..n_rows {
                let l = read_uvarint(t, &mut p, col_end)?;
                if l > u32::MAX as u64 {
                    bail!("column length too large");
                }
                offs.push(prev);
                prev += l;
                sum += l;
                lens_c.push(l as u32);
            }
            if sum > (col_end - p) as u64 {
                bail!("variable column overruns stream");
            }
            value_bases.push(p); // values begin after this column's length stream
            p += sum as usize;
            storage.push(offs);
            lens.push(lens_c);
        }
    }
    if p != col_end {
        bail!("columnar data length mismatch");
    }

    // Phase B: rebuild rows (row-major, exactly the original byte order).
    let mut out: Vec<u8> = Vec::with_capacity(expected_len.min(1 << 26));
    out.extend_from_slice(&t[..col_start]);
    let mut prev_vals: Vec<i64> = vec![0i64; n_cols as usize];
    for r in 0..n_rows as usize {
        for (ci, c) in meta.cols.iter().enumerate() {
            if ci > 0 {
                out.push(meta.col_delim);
            }
            let base = value_bases[ci];
            if c.dmode == DMODE_NONE {
                // raw value slice
                let (off, len) = if c.fixed {
                    (base + r * c.width as usize, c.width as usize)
                } else {
                    (base + storage[ci][r] as usize, lens[ci][r] as usize)
                };
                let end = off
                    .checked_add(len)
                    .ok_or_else(|| anyhow::anyhow!("cell overflow"))?;
                if end > t.len() {
                    bail!("cell overruns stream");
                }
                out.extend_from_slice(&t[off..end]);
            } else {
                // delta: decode the varint (relative to its own cell),
                // accumulate across rows, render byte-exactly.
                let (off, len) = (base + storage[ci][r] as usize, lens[ci][r] as usize);
                let end = off
                    .checked_add(len)
                    .ok_or_else(|| anyhow::anyhow!("delta cell overflow"))?;
                if end > t.len() {
                    bail!("delta cell overruns stream");
                }
                let mut pos = 0usize;
                let dv = read_zigzag(&t[off..end], &mut pos, len)?;
                if pos != len {
                    bail!("delta varint overran its cell");
                }
                let v = if r == 0 { dv } else { prev_vals[ci].wrapping_add(dv) };
                prev_vals[ci] = v;
                match c.dmode {
                    DMODE_INT => out.extend_from_slice(v.to_string().as_bytes()),
                    DMODE_DATE => {
                        let d = render_date(v)
                            .ok_or_else(|| anyhow::anyhow!("date out of range"))?;
                        out.extend_from_slice(&d);
                    }
                    _ => bail!("unknown delta mode"),
                }
            }
        }
        out.push(meta.row_delim);
    }
    out.extend_from_slice(&t[col_end..]);
    if out.len() != expected_len {
        bail!("table inverse produced {} bytes, expected {expected_len}", out.len());
    }
    Ok(out)
}
// ---------------------------------------------------------------------------
// Phase B0 characterization (`aahl char`)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct CharColumn {
    pub idx: usize,
    pub fixed: bool,
    pub width: u32,
    pub cardinality: usize,
    pub int_rows: u64,
    pub numeric_pct: f64,
    pub monotonic: bool,
    pub entropy: f64,
    pub delta_smaller: bool,
    pub raw_profile: u64,
    pub delta_profile: u64,
}

#[derive(Clone, Debug)]
pub struct GridChars {
    pub col_delim: u8,
    pub n_cols: u16,
    pub grid_lines: u64,
    pub grid_pct: f64,
    pub sample_rows: usize,
    pub columns: Vec<CharColumn>,
}

#[derive(Clone, Debug)]
pub struct CharReport {
    pub total_bytes: u64,
    pub total_lines: u64,
    pub jsonish_lines: u64,
    pub grid: Option<GridChars>,
}

fn shannon(data: &[u8]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let mut cnt = [0u64; 256];
    for &b in data {
        cnt[b as usize] += 1;
    }
    let n = data.len() as f64;
    cnt.iter().filter(|&&c| c > 0).fold(0.0, |h, &c| {
        let p = c as f64 / n;
        h - p * p.log2()
    })
}

/// Deterministic, sample-based characterization for `aahl char`. Splits into
/// lines, finds the delimiter with the largest uniform-count block, and (when
/// one dominates) computes per-column stats over a strided sample.
pub fn characterize(input: &[u8], max_rows: usize) -> CharReport {
    let total_bytes = input.len() as u64;
    let mut lines: Vec<&[u8]> = input.split(|&c| c == b'\n').collect();
    // `wc -l` semantics: a terminal newline must not add a phantom empty line.
    if lines.len() > 1 && lines.last() == Some(&&b""[..]) {
        lines.pop();
    }
    let total_lines = lines.len() as u64;
    let jsonish_lines = lines
        .iter()
        .filter(|ln| ln.contains(&b'{') && ln.contains(&b'"'))
        .count() as u64;

    // dominant uniform delimiter over all non-empty lines
    let mut best: Option<(u8, u16, u64)> = None;
    for &d in &DELIMS {
        let mut counts: HashMap<u16, u64> = HashMap::new();
        for ln in &lines {
            if ln.is_empty() {
                continue;
            }
            let c = ln.iter().filter(|&&b| b == d).count();
            if c == 0 || c > (u32::from(MAX_COLS) - 1) as usize {
                continue;
            }
            *counts.entry(c as u16).or_insert(0) += 1;
        }
        if let Some((&c, &n)) = counts.iter().max_by_key(|(_, &n)| n) {
            if best.as_ref().map(|(_, _, bn)| n > *bn).unwrap_or(true) {
                best = Some((d, c + 1, n));
            }
        }
    }
    let Some((col_delim, n_cols, grid_lines)) = best else {
        return CharReport { total_bytes, total_lines, jsonish_lines, grid: None };
    };
    let grid_pct = if total_lines > 0 { grid_lines as f64 / total_lines as f64 * 100.0 } else { 0.0 };

    // sample the matched lines deterministically
    let matched: Vec<&[u8]> = lines
        .iter()
        .copied()
        .filter(|ln| {
            !ln.is_empty()
                && {
                    let c = ln.iter().filter(|&&b| b == col_delim).count();
                    c == (n_cols - 1) as usize
                }
        })
        .collect();
    let stride = (matched.len() / max_rows.max(1)).max(1);
    let sample_rows = (matched.len() + stride - 1) / stride;
    let mut columns: Vec<Vec<&[u8]>> = vec![Vec::new(); n_cols as usize];
    for (i, ln) in matched.iter().enumerate() {
        if i % stride != 0 {
            continue;
        }
        let mut start = 0usize;
        let mut done = 0usize;
        for (j, &b) in ln.iter().enumerate() {
            if b == col_delim {
                columns[done].push(&ln[start..j]);
                done += 1;
                start = j + 1;
            }
        }
        columns[done].push(&ln[start..]);
    }
    let mut col_out = Vec::with_capacity(n_cols as usize);
    for (idx, cells) in columns.iter().enumerate() {
        let dec = column_decision(cells);
        let mut cat: Vec<&[u8]> = cells.to_vec();
        cat.sort_unstable();
        cat.dedup();
        let cardinality = cat.len();
        let int_rows = cells.iter().filter(|c| parse_int(c).is_some()).count() as u64;
        let numeric_pct = if cells.is_empty() { 0.0 } else { int_rows as f64 / cells.len() as f64 * 100.0 };
        let monotonic = int_eligible(cells)
            && cells
                .windows(2)
                .all(|w| match (parse_int(w[0]), parse_int(w[1])) {
                    (Some(a), Some(b)) => a <= b,
                    _ => true,
                });
        let all: Vec<u8> = cells.iter().flat_map(|c| c.iter().copied()).collect();
        let entropy = shannon(&all);
        let delta_smaller = dec.dmode != DMODE_NONE && dec.delta_profile < dec.raw_profile;
        col_out.push(CharColumn {
            idx,
            fixed: dec.fixed,
            width: dec.width,
            cardinality,
            int_rows,
            numeric_pct,
            monotonic,
            entropy,
            delta_smaller,
            raw_profile: dec.raw_profile as u64,
            delta_profile: if delta_smaller { dec.delta_profile as u64 } else { 0 },
        });
    }

    CharReport {
        total_bytes,
        total_lines,
        jsonish_lines,
        grid: Some(GridChars {
            col_delim,
            n_cols,
            grid_lines,
            grid_pct,
            sample_rows,
            columns: col_out,
        }),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::aahl::compress_block;

    fn csv_rows(rows: usize) -> Vec<u8> {
        let mut s = String::from("# csv table\n");
        for i in 0..rows {
            s.push_str(&format!(
                "{i},gadget-{i:05},gadget,{},2365.65,W{}\n",
                1 + (i % 500),
                92 + (i % 24)
            ));
        }
        s.into_bytes()
    }

    fn json_rows(rows: usize) -> Vec<u8> {
        let mut s = String::from("# json lines\n");
        for i in 0..rows {
            s.push_str(&format!(
                "{{\"id\":{i},\"sku\":\"gadget-{i:05}\",\"category\":\"gadget\",\"qty\":{},\"price\":2365.65,\"wh\":\"W{}\"}}\n",
                1 + (i % 500),
                92 + (i % 24)
            ));
        }
        s.into_bytes()
    }

    fn roundtrip_prepared(raw: &[u8], stats: bool) {
        let mut st = TableStats::default();
        let plan = match prepare(raw, if stats { Some(&mut st) } else { None }) {
            Ok(Some(p)) => p,
            Ok(None) => return, // declined: nothing to verify
            Err(e) => panic!("prepare failed: {e:#}"),
        };
        // exact inverse of the plain transform stream
        let back = inverse(&plan.t_stream, &plan.meta, raw.len()).unwrap();
        assert_eq!(back, raw, "inverse roundtrip mismatch");
        // full wrapper path through a real codec block
        let inner = compress_block(&plan.t_stream);
        let wrapped = wrap_block(&plan.meta, &inner).unwrap();
        let (m2, off) = try_unwrap(&wrapped).unwrap().expect("wrapper must unwrap");
        assert_eq!(m2, plan.meta);
        let t2 = crate::aahl::decompress_block(&wrapped[off..], m2.t_len as usize).unwrap();
        assert_eq!(t2, plan.t_stream);
        let back2 = inverse(&t2, &m2, raw.len()).unwrap();
        assert_eq!(back2, raw, "wrapper roundtrip mismatch");
    }

    #[test]
    fn csv_chunk_selected_and_roundtrips() {
        let raw = csv_rows(2000);
        assert!(raw.len() > MIN_GRID_BYTES);
        roundtrip_prepared(&raw, true);
    }

    #[test]
    fn json_lines_selected_and_roundtrips() {
        let raw = json_rows(2000);
        roundtrip_prepared(&raw, true);
    }

    #[test]
    fn partial_boundaries_roundtrip() {
        let full = csv_rows(4000);
        let mut windows: Vec<&[u8]> = Vec::new();
        let step = (full.len() / 5).max(1) + 1;
        for cut in (0..full.len()).step_by(step) {
            if cut + 512 <= full.len() {
                windows.push(&full[cut..cut + 512]);
            }
        }
        windows.push(&full[full.len() - 512..]);
        for w in windows {
            match prepare(w, None) {
                Ok(Some(p)) => {
                    let back = inverse(&p.t_stream, &p.meta, w.len()).unwrap();
                    assert_eq!(back, w, "mid-row chunk boundary mismatch");
                }
                Ok(None) => {}
                Err(e) => panic!("prepare failed: {e:#}"),
            }
        }
    }

    #[test]
    fn prose_and_random_decline() {
        let prose = b"the quick brown fox jumps over the lazy dog while the farmer watches\n".repeat(200);
        assert!(matches!(prepare(&prose, None), Ok(None)));
        let mut rand = Vec::new();
        let mut x = 0x12345678u32;
        for _ in 0..100_000 {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            rand.extend_from_slice(&x.to_le_bytes());
        }
        assert!(matches!(prepare(&rand, None), Ok(None)));
        assert!(matches!(prepare(b"", None), Ok(None)));
        assert!(matches!(prepare(b"a", None), Ok(None)));
    }

    #[test]
    fn ragged_grid_declines() {
        let mut raw = csv_rows(100);
        let idx = raw.windows(6).position(|w| w == b"gadget").unwrap();
        raw.insert(idx, b'X');
        raw.insert(idx, b',');
        assert!(matches!(prepare(&raw, None), Ok(None)));
    }

    #[test]
    fn delta_chosen_on_monotonic_int_column() {
        let raw = csv_rows(3000);
        let plan = prepare(&raw, None).unwrap().expect("csv selected");
        // column 0 = id (all canonical ints, monotonic) => INT delta chosen
        assert_eq!(plan.meta.cols[0].dmode, DMODE_INT, "id column must delta-code");
        // column 2 = category 'gadget' (fixed width) uses fixed raw storage
        let c2 = &plan.meta.cols[2];
        assert!(c2.fixed && c2.width == 6, "category must be fixed-width raw");
    }

    #[test]
    fn date_delta_chosen_and_roundtrips() {
        let mut s = String::new();
        for i in 0..3000u32 {
            let (yy, mm, dd) = (2026, 1 + (i / 28) % 12, 1 + (i % 28));
            s.push_str(&format!("{i},a,{yy:04}-{mm:02}-{dd:02}\n"));
        }
        let raw = s.into_bytes();
        let plan = prepare(&raw, None).unwrap().expect("grid selected");
        assert_eq!(plan.meta.cols[2].dmode, DMODE_DATE, "date column must delta-code");
        let back = inverse(&plan.t_stream, &plan.meta, raw.len()).unwrap();
        assert_eq!(back, raw);
    }

    #[test]
    fn prepare_is_deterministic() {
        let raw = csv_rows(1500);
        let a = prepare(&raw, None).unwrap();
        let b = prepare(&raw, None).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn hostile_wrappers_never_panic() {
        // garbage after the tag never panics and always errors
        for len in [0usize, 1, 2, 3, 5, 8, 22, 40, 100] {
            let mut bad = vec![0xA0, b'T'];
            bad.extend(std::iter::repeat(0xFFu8).take(len));
            let _ = try_unwrap(&bad);
        }
        // truncations of a real wrapper never panic
        let raw = csv_rows(300);
        let plan = prepare(&raw, None).unwrap().expect("selected");
        let inner = compress_block(&plan.t_stream);
        let wrapped = wrap_block(&plan.meta, &inner).unwrap();
        for cut in (0..wrapped.len()).step_by(3) {
            let _ = try_unwrap(&wrapped[..cut]);
            let _ = try_unwrap(&wrapped[cut..]);
        }
        // a valid wrapper whose inner block is corrupt must error, not decode
        let mut corrupt = inner.clone();
        corrupt[0] ^= 0xFF;
        let wrapped_bad = wrap_block(&plan.meta, &corrupt).unwrap();
        let (m3, off3) = try_unwrap(&wrapped_bad).unwrap().expect("wrapper must unwrap");
        assert!(
            crate::aahl::decompress_block(&wrapped_bad[off3..], m3.t_len as usize).is_err(),
            "corrupt inner block must fail to decode"
        );
    }

    #[test]
    fn inverse_rejects_bogus_geometry() {
        let raw = csv_rows(50);
        let plan = prepare(&raw, None).unwrap().expect("selected");
        let t = plan.t_stream.clone();
        // n_rows * n_cols far above expected_len -> geometry bail
        let mut m1 = plan.meta.clone();
        m1.n_rows = u32::MAX;
        assert!(inverse(&t, &m1, raw.len()).is_err());
        // t_len mismatch
        let mut m2 = plan.meta.clone();
        m2.t_len += 1;
        assert!(inverse(&t, &m2, raw.len()).is_err());
        // descriptor count mismatch
        let mut m3 = plan.meta.clone();
        m3.cols.pop();
        assert!(inverse(&t, &m3, raw.len()).is_err());
        // head + tail overlapping the t stream
        let mut m4 = plan.meta.clone();
        m4.head_len = m4.t_len;
        assert!(inverse(&t, &m4, raw.len()).is_err());
    }

    #[test]
    fn characterize_reports_grid_and_delta_columns() {
        let raw = csv_rows(400);
        let rep = characterize(&raw, 1000);
        let g = rep.grid.expect("characterize must find the grid");
        assert_eq!(g.n_cols, 6);
        assert_eq!(g.col_delim, b',');
        assert_eq!(rep.total_lines, 401);
        assert_eq!(g.grid_pct, 400.0 / 401.0 * 100.0);
        assert!(g.columns[0].delta_smaller, "id column delta must win in the report");
        assert!(g.columns[0].monotonic, "id column must be monotonic");
        assert_eq!(g.columns[0].numeric_pct, 100.0);
        assert!(g.columns[2].fixed, "category column must be fixed width");
        assert!(!g.columns[1].delta_smaller, "sku column is not delta-eligible");
    }

    #[test]
    fn characterize_json_half() {
        let raw = json_rows(100);
        let rep = characterize(&raw, 100);
        assert!(rep.jsonish_lines >= 99);
        let g = rep.grid.unwrap();
        assert_eq!(g.n_cols, 6);
        assert_eq!(g.grid_lines, 100);
    }

    #[test]
    fn characterize_reports_no_grid_for_prose() {
        let prose = b"some prose text without any delimiter structure at all\n".repeat(50);
        let rep = characterize(&prose, 100);
        assert!(rep.grid.is_none());
    }
}

