pub mod scalar;
#[cfg(target_arch = "aarch64")]
pub mod neon;

#[cfg(target_arch = "aarch64")]
use neon as fast;
#[cfg(not(target_arch = "aarch64"))]
use scalar as fast;

/// Past this size ratio, galloping beats the block compare.
const GALLOP_RATIO: usize = 32;

pub fn intersect(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    let (small, big) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if small.is_empty() {
        return;
    }
    if big.len() / small.len() >= GALLOP_RATIO {
        scalar::gallop(small, big, out)
    } else {
        fast::intersect(small, big, out)
    }
}

pub fn union(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    scalar::union(a, b, out)
}

pub fn count_byte(hay: &[u8], needle: u8) -> usize {
    fast::count_byte(hay, needle)
}

pub fn ascii_lower_into(src: &[u8], dst: &mut Vec<u8>) {
    fast::ascii_lower_into(src, dst)
}

pub fn svb_decode_delta(ctrl: &[u8], data: &[u8], n: usize, out: &mut Vec<u32>) {
    #[cfg(target_arch = "aarch64")]
    neon::svb_decode_delta(ctrl, data, n, out);
    #[cfg(not(target_arch = "aarch64"))]
    scalar::svb_decode_delta(ctrl, data, n, 0, out);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn sorted_set(&mut self, n: usize, range: u32) -> Vec<u32> {
            let mut v: Vec<u32> = (0..n).map(|_| (self.next() % range as u64) as u32).collect();
            v.sort_unstable();
            v.dedup();
            v
        }
    }

    #[test]
    fn intersect_matches_scalar() {
        let mut r = Rng(0x9E3779B97F4A7C15);
        for round in 0..2000 {
            let na = (r.next() % 300) as usize;
            let nb = if round % 3 == 0 { na * 50 } else { (r.next() % 300) as usize };
            let range = 1 + (r.next() % 2000) as u32;
            let a = r.sorted_set(na, range);
            let b = r.sorted_set(nb, range);
            let mut want = vec![];
            scalar::intersect(&a, &b, &mut want);
            let mut got = vec![];
            intersect(&a, &b, &mut got);
            assert_eq!(got, want, "a={a:?} b={b:?}");
            #[cfg(target_arch = "aarch64")]
            {
                let mut got = vec![];
                neon::intersect(&a, &b, &mut got);
                assert_eq!(got, want);
            }
            let mut got = vec![];
            scalar::gallop(&a, &b, &mut got);
            assert_eq!(got, want);
        }
    }

    #[test]
    fn count_and_lower() {
        let mut r = Rng(42);
        for n in [0usize, 1, 15, 16, 63, 64, 65, 4031, 4032, 4033, 100_000] {
            let v: Vec<u8> = (0..n).map(|_| (r.next() % 96 + 10) as u8).collect();
            assert_eq!(count_byte(&v, b'\n'), scalar::count_byte(&v, b'\n'));
            assert_eq!(count_byte(&v, b'A'), scalar::count_byte(&v, b'A'));
            let (mut x, mut y) = (vec![], vec![]);
            ascii_lower_into(&v, &mut x);
            scalar::ascii_lower_into(&v, &mut y);
            assert_eq!(x, y);
        }
    }
}
