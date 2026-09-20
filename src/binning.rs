//! Divide-and-bin structural grouping for AAHL.
//! Standard codecs compress bytes in linear order. This pass acts like a
//! sorting factory: it splits a chunk into fixed pieces, scores each piece by
//! structural signature (coarse byte histogram + entropy bucket), clusters
//! similar pieces into bins, and compresses each bin independently. The order
//! map restores the original layout losslessly on decode.

pub const PIECE_SIZE: usize = 2048;
pub const MAX_BINS: usize = 8;
/// L1 distance threshold on normalized 16-dim histograms for bin membership.
const JOIN_THRESHOLD: f64 = 0.45;

fn signature(piece: &[u8]) -> [f64; 16] {
    let mut h = [0u64; 16];
    for &b in piece {
        h[(b >> 4) as usize] += 1;
    }
    let n = piece.len().max(1) as f64;
    let mut s = [0.0f64; 16];
    for (i, &c) in h.iter().enumerate() {
        s[i] = c as f64 / n;
    }
    s
}

fn dist(a: &[f64; 16], b: &[f64; 16]) -> f64 {
    a.iter().zip(b.iter()).map(|(x, y)| (x - y).abs()).sum()
}

/// Greedy clustering: each piece joins the first bin whose centroid is close
/// enough, else opens a new bin (up to MAX_BINS). Returns bin index per piece
/// and per-bin centroids (for tests/transparency).
pub fn cluster(pieces: &[&[u8]]) -> Vec<usize> {
    let sigs: Vec<[f64; 16]> = pieces.iter().map(|p| signature(p)).collect();
    let mut centroids: Vec<[f64; 16]> = Vec::new();
    let mut counts: Vec<usize> = Vec::new();
    let mut assign = vec![0usize; pieces.len()];
    for (i, s) in sigs.iter().enumerate() {
        let mut best: Option<usize> = None;
        for (b, c) in centroids.iter().enumerate() {
            if dist(s, c) < JOIN_THRESHOLD {
                best = Some(b);
                break;
            }
        }
        let b = match best {
            Some(b) => b,
            None => {
                if centroids.len() < MAX_BINS {
                    centroids.push(*s);
                    counts.push(0);
                    centroids.len() - 1
                } else {
                    // bins full: join nearest
                    let mut nb = 0;
                    let mut nd = f64::MAX;
                    for (bi, c) in centroids.iter().enumerate() {
                        let d = dist(s, c);
                        if d < nd {
                            nd = d;
                            nb = bi;
                        }
                    }
                    nb
                }
            }
        };
        // running centroid update
        let n = counts[b] as f64;
        for k in 0..16 {
            centroids[b][k] = (centroids[b][k] * n + s[k]) / (n + 1.0);
        }
        counts[b] += 1;
        assign[i] = b;
    }
    assign
}

/// Split data into PIECE_SIZE pieces (last may be short).
/// Returns None when binning cannot help (fewer than 4 pieces).
pub fn plan(data: &[u8]) -> Option<(Vec<usize>, usize)> {
    if data.len() < PIECE_SIZE * 4 {
        return None;
    }
    let pieces: Vec<&[u8]> = data.chunks(PIECE_SIZE).collect();
    let assign = cluster(&pieces);
    let nbins = assign.iter().copied().max().unwrap_or(0) + 1;
    if nbins < 2 {
        return None; // homogeneous: binning adds only overhead
    }
    Some((assign, pieces.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn homogeneous_text_is_single_bin() {
        let data = vec![b'a'; 16 * 1024];
        let pieces: Vec<&[u8]> = data.chunks(PIECE_SIZE).collect();
        let a = cluster(&pieces);
        assert!(a.iter().all(|&b| b == a[0]));
    }

    #[test]
    fn interleaved_text_and_binary_split() {
        // alternating text pieces and high-byte pieces must not share one bin
        let mut data = Vec::new();
        for i in 0..8 {
            if i % 2 == 0 {
                data.extend(vec![b'x'; PIECE_SIZE]);
            } else {
                data.extend((0..PIECE_SIZE).map(|j| (j % 256) as u8));
            }
        }
        let pieces: Vec<&[u8]> = data.chunks(PIECE_SIZE).collect();
        let a = cluster(&pieces);
        let text_bin = a[0];
        assert!(a[2] == text_bin && a[4] == text_bin);
        assert!(a[1] != text_bin && a[3] != text_bin);
    }

    #[test]
    fn small_input_has_no_plan() {
        assert!(plan(&vec![1u8; 100]).is_none());
    }
}
