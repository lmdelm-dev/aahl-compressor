//! Spectral period detector built on from-scratch complex FFT.
//! Breaks the O(n*m) pair-scan chain: instead of counting every adjacent
//! pair link-by-link, jump straight to long-range periods via the frequency
//! domain (complex roots of unity), then verify losslessly in time domain.

use std::f64::consts::PI;

#[derive(Clone, Copy, Debug)]
pub struct Complex {
    pub re: f64,
    pub im: f64,
}

impl Complex {
    pub fn new(re: f64, im: f64) -> Self {
        Self { re, im }
    }
    pub fn norm2(self) -> f64 {
        self.re * self.re + self.im * self.im
    }
}

use std::ops::{Add, Mul, Sub};
impl Add for Complex {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        Self::new(self.re + o.re, self.im + o.im)
    }
}
impl Sub for Complex {
    type Output = Self;
    fn sub(self, o: Self) -> Self {
        Self::new(self.re - o.re, self.im - o.im)
    }
}
impl Mul for Complex {
    type Output = Self;
    fn mul(self, o: Self) -> Self {
        Self::new(
            self.re * o.re - self.im * o.im,
            self.re * o.im + self.im * o.re,
        )
    }
}

/// In-place iterative Cooley-Tukey FFT. `data.len()` must be a power of two.
/// If `invert` is true, computes the inverse FFT (scaled by 1/n).
pub fn fft(data: &mut [Complex], invert: bool) {
    let n = data.len();
    debug_assert!(n.is_power_of_two());
    // bit-reversal permutation
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            data.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = 2.0 * PI / (len as f64) * if invert { -1.0 } else { 1.0 };
        let wlen = Complex::new(ang.cos(), ang.sin());
        let half = len / 2;
        let mut i = 0;
        while i < n {
            let mut w = Complex::new(1.0, 0.0);
            for k in 0..half {
                let u = data[i + k];
                let v = data[i + k + half] * w;
                data[i + k] = u + v;
                data[i + k + half] = u - v;
                w = w * wlen;
            }
            i += len;
        }
        len <<= 1;
    }
    if invert {
        let inv = 1.0 / (n as f64);
        for c in data.iter_mut() {
            c.re *= inv;
            c.im *= inv;
        }
    }
}

fn next_pow2(mut n: usize) -> usize {
    if n <= 1 {
        return 1;
    }
    n -= 1;
    n |= n >> 1;
    n |= n >> 2;
    n |= n >> 4;
    n |= n >> 8;
    n |= n >> 16;
    n |= n >> 32;
    n + 1
}

fn is_exact_period(raw: &[u8], p: usize) -> bool {
    if p == 0 || p >= raw.len() {
        return false;
    }
    for (i, &b) in raw.iter().enumerate() {
        if b != raw[i % p] {
            return false;
        }
    }
    true
}

/// Detect an exact period using the complex frequency domain.
/// Returns the smallest exact period whose FFT bin dominates, or None.
/// Pure integer-period signals concentrate energy at bin k = N/p, so we rank
/// bins by power and verify candidates in time domain (lossless guarantee).
pub fn detect_period(raw: &[u8]) -> Option<usize> {
    const MIN_LEN: usize = 64;
    const MAX_PERIOD: usize = 4096;
    if raw.len() < MIN_LEN {
        return None;
    }
    let nfft = next_pow2(raw.len()).max(256);
    if nfft > 1 << 20 {
        return None; // cap FFT size for prototype
    }
    let mean: f64 = raw.iter().map(|&b| b as f64).sum::<f64>() / (raw.len() as f64);
    let mut spec: Vec<Complex> = (0..nfft)
        .map(|i| {
            if i < raw.len() {
                Complex::new(raw[i] as f64 - mean, 0.0)
            } else {
                Complex::new(0.0, 0.0)
            }
        })
        .collect();
    fft(&mut spec, false);
    // power spectrum for k = 1..nfft/2 (skip DC)
    let half = nfft / 2;
    let mut bins: Vec<(usize, f64)> = (1..half)
        .map(|k| (k, spec[k].norm2()))
        .collect();
    bins.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    let total: f64 = bins.iter().map(|(_, p)| p).sum();
    if total <= 1e-9 {
        return None;
    }
    // try top bins; threshold keeps us from chasing noise
    for &(k, pwr) in bins.iter().take(16) {
        if pwr < 0.02 * total {
            break;
        }
        // candidate period from bin frequency; check neighbors for leakage
        for dk in [0i64, -1, 1, -2, 2] {
            let kk = k as i64 + dk;
            if kk <= 0 {
                continue;
            }
            let p_cand = (nfft as f64 / kk as f64).round() as usize;
            if p_cand == 0 || p_cand > MAX_PERIOD || p_cand >= raw.len() {
                continue;
            }
            // exact periods also imply divisors may be exact; prefer smallest
            let mut p_try = p_cand;
            // reduce to smallest divisor that is still exact
            let mut divs: Vec<usize> = Vec::new();
            let mut d = 1usize;
            while d * d <= p_try {
                if p_try % d == 0 {
                    divs.push(d);
                    if d * d != p_try {
                        divs.push(p_try / d);
                    }
                }
                d += 1;
            }
            divs.sort_unstable();
            for d in divs {
                if d >= 1 && d <= MAX_PERIOD && is_exact_period(raw, d) {
                    p_try = d;
                    break;
                }
            }
            if is_exact_period(raw, p_try) {
                return Some(p_try);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_roundtrip_restores_signal() {
        let mut v = vec![
            Complex::new(1.0, 0.0),
            Complex::new(2.0, 0.0),
            Complex::new(3.0, 0.0),
            Complex::new(4.0, 0.0),
        ];
        let orig: Vec<(f64, f64)> = v.iter().map(|c| (c.re, c.im)).collect();
        fft(&mut v, false);
        fft(&mut v, true);
        for (c, (re, im)) in v.iter().zip(orig) {
            assert!((c.re - re).abs() < 1e-9);
            assert!((c.im - im).abs() < 1e-9);
        }
    }

    #[test]
    fn detects_exact_period_13() {
        let pat: Vec<u8> = b"hello world! ".to_vec();
        let mut raw = Vec::new();
        for _ in 0..200 {
            raw.extend_from_slice(&pat);
        }
        assert_eq!(detect_period(&raw), Some(13));
    }

    #[test]
    fn rejects_randomish_data() {
        let raw: Vec<u8> = (0..500).map(|i| ((i * 37 + 11) % 251) as u8).collect();
        assert_eq!(detect_period(&raw), None);
    }

    #[test]
    fn rejects_short_input() {
        assert_eq!(detect_period(b"abc"), None);
    }
}
