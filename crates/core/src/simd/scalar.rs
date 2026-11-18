pub fn intersect(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (x, y) = (a[i], b[j]);
        if x == y {
            out.push(x);
            i += 1;
            j += 1;
        } else if x < y {
            i += 1;
        } else {
            j += 1;
        }
    }
}

/// Intersection for skewed sizes: walk the small list, gallop in the big one.
pub fn gallop(small: &[u32], big: &[u32], out: &mut Vec<u32>) {
    let mut lo = 0;
    for &x in small {
        let mut step = 1;
        let mut hi = lo;
        while hi < big.len() && big[hi] < x {
            lo = hi + 1;
            hi += step;
            step <<= 1;
        }
        // big[hi] >= x (or hi is past the end), so hi itself must be in the search window.
        let hi = (hi + 1).min(big.len());
        match big[lo..hi].binary_search(&x) {
            Ok(p) => {
                out.push(x);
                lo += p + 1;
            }
            Err(p) => lo += p,
        }
        if lo >= big.len() {
            break;
        }
    }
}

pub fn union(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.reserve(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (x, y) = (a[i], b[j]);
        if x == y {
            out.push(x);
            i += 1;
            j += 1;
        } else if x < y {
            out.push(x);
            i += 1;
        } else {
            out.push(y);
            j += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
}

pub fn count_byte(hay: &[u8], needle: u8) -> usize {
    hay.iter().filter(|&&b| b == needle).count()
}

pub fn ascii_lower_into(src: &[u8], dst: &mut Vec<u8>) {
    dst.clear();
    dst.extend(src.iter().map(u8::to_ascii_lowercase));
}

/// Stream VByte decode with delta (prefix sum) restore, appending `n` values to `out`.
/// Returns bytes of `data` consumed.
pub fn svb_decode_delta(ctrl: &[u8], data: &[u8], n: usize, mut prev: u32, out: &mut Vec<u32>) -> usize {
    let mut pos = 0;
    for i in 0..n {
        let code = (ctrl[i / 4] >> ((i % 4) * 2)) & 3;
        let len = code as usize + 1;
        let mut v = 0u32;
        for k in 0..len {
            v |= (data[pos + k] as u32) << (8 * k);
        }
        pos += len;
        prev = prev.wrapping_add(v);
        out.push(prev);
    }
    pos
}
