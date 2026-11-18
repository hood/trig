//! Posting list codec: `[n: u32 LE][ctrl: ceil(n/4)][data]`, Stream VByte over deltas.
use crate::simd;

pub fn encode(ids: &[u32]) -> Vec<u8> {
    let n = ids.len();
    let cl = n.div_ceil(4);
    let mut out = Vec::with_capacity(4 + cl + n * 2);
    out.extend_from_slice(&(n as u32).to_le_bytes());
    out.resize(4 + cl, 0);
    let mut prev = 0u32;
    for (i, &id) in ids.iter().enumerate() {
        let d = id.wrapping_sub(prev);
        prev = id;
        let len = if d < 1 << 8 {
            1
        } else if d < 1 << 16 {
            2
        } else if d < 1 << 24 {
            3
        } else {
            4
        };
        out[4 + i / 4] |= ((len - 1) as u8) << ((i % 4) * 2);
        out.extend_from_slice(&d.to_le_bytes()[..len]);
    }
    out
}

pub fn len(buf: &[u8]) -> usize {
    if buf.len() < 4 { 0 } else { u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize }
}

pub fn decode_into(buf: &[u8], out: &mut Vec<u32>) {
    let n = len(buf);
    if n == 0 {
        return;
    }
    let cl = n.div_ceil(4);
    simd::svb_decode_delta(&buf[4..4 + cl], &buf[4 + cl..], n, out);
}

pub fn decode(buf: &[u8]) -> Vec<u32> {
    let mut v = Vec::with_capacity(len(buf));
    decode_into(buf, &mut v);
    v
}

/// Union of sorted lists. Bulk indexing hands out disjoint, increasing id ranges per
/// chunk, so the common case is a plain concat once ordered by first id.
pub fn merge_lists(mut lists: Vec<Vec<u32>>) -> Vec<u32> {
    lists.retain(|l| !l.is_empty());
    lists.sort_unstable_by_key(|l| l[0]);
    let disjoint = lists.windows(2).all(|w| w[0].last() < w[1].first());
    let total = lists.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(total);
    for l in &lists {
        out.extend_from_slice(l);
    }
    if !disjoint {
        out.sort_unstable();
        out.dedup();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut x = 0x2545F4914F6CDD1Du64;
        for n in [0usize, 1, 3, 4, 5, 7, 8, 9, 31, 32, 33, 1000, 50_000] {
            for spread in [1u64, 300, 70_000, 20_000_000] {
                let mut v = Vec::with_capacity(n);
                let mut cur = 0u32;
                for _ in 0..n {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    cur = cur.saturating_add(1 + (x % spread) as u32);
                    v.push(cur);
                }
                v.dedup();
                let enc = encode(&v);
                assert_eq!(decode(&enc), v, "n={n} spread={spread}");
                let mut tail = vec![];
                simd::scalar::svb_decode_delta(&enc[4..4 + v.len().div_ceil(4)], &enc[4 + v.len().div_ceil(4)..], v.len(), 0, &mut tail);
                assert_eq!(tail, v);
            }
        }
    }

    #[test]
    fn merge_overlapping_and_disjoint() {
        assert_eq!(merge_lists(vec![vec![5, 6], vec![1, 2], vec![]]), vec![1, 2, 5, 6]);
        assert_eq!(merge_lists(vec![vec![1, 5, 9], vec![2, 5, 10]]), vec![1, 2, 5, 9, 10]);
    }
}
