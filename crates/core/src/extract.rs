use crate::simd;

pub const MAX_FILE: u64 = 64 << 20;
const BINARY_SNIFF: usize = 8 << 10;

pub fn is_binary(data: &[u8]) -> bool {
    memchr::memchr(0, &data[..data.len().min(BINARY_SNIFF)]).is_some()
}

/// Unique, ASCII-case-folded trigrams of a buffer. Dedup through a 2^24-bit bitset that is
/// cleared lazily (only the words we touched), so the 2MB table costs nothing per file.
pub struct Extractor {
    bits: Vec<u64>,
    lower: Vec<u8>,
    out: Vec<u32>,
}

impl Default for Extractor {
    fn default() -> Self {
        Self::new()
    }
}

impl Extractor {
    pub fn new() -> Self {
        Extractor { bits: vec![0; (1 << 24) / 64], lower: Vec::new(), out: Vec::new() }
    }

    pub fn trigrams(&mut self, data: &[u8]) -> &[u32] {
        self.out.clear();
        if data.len() < 3 {
            return &self.out;
        }
        simd::ascii_lower_into(data, &mut self.lower);
        let l = &self.lower;
        let bits = &mut self.bits;
        let mut t = ((l[0] as u32) << 8) | l[1] as u32;
        for &b in &l[2..] {
            t = ((t << 8) | b as u32) & 0xFF_FFFF;
            let (w, m) = ((t >> 6) as usize, 1u64 << (t & 63));
            // SAFETY: t < 2^24 so w < bits.len().
            let word = unsafe { bits.get_unchecked_mut(w) };
            if *word & m == 0 {
                *word |= m;
                self.out.push(t);
            }
        }
        for &t in &self.out {
            bits[(t >> 6) as usize] = 0;
        }
        &self.out
    }
}

pub fn trigrams_of(s: &[u8]) -> impl Iterator<Item = u32> + '_ {
    s.windows(3).map(|w| ((w[0].to_ascii_lowercase() as u32) << 16) | ((w[1].to_ascii_lowercase() as u32) << 8) | w[2].to_ascii_lowercase() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_folded() {
        let mut e = Extractor::new();
        let mut got = e.trigrams(b"abcABCabc").to_vec();
        got.sort();
        let mut want: Vec<u32> = trigrams_of(b"abcabcabc").collect();
        want.sort();
        want.dedup();
        assert_eq!(got, want);
        // bitset is clean for the next file
        assert_eq!(e.trigrams(b"abc").len(), 1);
    }
}
