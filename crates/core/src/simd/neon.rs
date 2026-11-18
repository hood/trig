//! NEON kernels. NEON is baseline on aarch64, so no runtime detection.
use core::arch::aarch64::*;

/// For a 4-bit lane mask, byte shuffle that packs the selected u32 lanes to the front.
const COMPRESS: [[u8; 16]; 16] = {
    let mut t = [[0xFFu8; 16]; 16];
    let mut m = 0;
    while m < 16 {
        let mut k = 0;
        let mut lane = 0;
        while lane < 4 {
            if m & (1 << lane) != 0 {
                let mut b = 0;
                while b < 4 {
                    t[m][k * 4 + b] = (lane * 4 + b) as u8;
                    b += 1;
                }
                k += 1;
            }
            lane += 1;
        }
        m += 1;
    }
    t
};

/// Stream VByte: for a control byte, the shuffle that spreads 1-4 byte values into u32 lanes.
const SVB_SHUF: [[u8; 16]; 256] = {
    let mut t = [[0xFFu8; 16]; 256];
    let mut c = 0;
    while c < 256 {
        let mut off = 0u8;
        let mut lane = 0;
        while lane < 4 {
            let len = ((c >> (lane * 2)) & 3) + 1;
            let mut b = 0;
            while b < len {
                t[c][lane * 4 + b] = off;
                off += 1;
                b += 1;
            }
            lane += 1;
        }
        c += 1;
    }
    t
};

pub const SVB_LEN: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut c = 0;
    while c < 256 {
        t[c] = ((c & 3) + ((c >> 2) & 3) + ((c >> 4) & 3) + ((c >> 6) & 3) + 4) as u8;
        c += 1;
    }
    t
};

/// Sorted-set intersection, 4x4 all-pairs block compare (Schlegel et al. / Lemire).
pub fn intersect(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    let na = a.len() & !3;
    let nb = b.len() & !3;
    out.reserve(a.len().min(b.len()) + 4);
    let (mut i, mut j, mut k) = (0usize, 0usize, 0usize);
    unsafe {
        let dst = out.as_mut_ptr().add(out.len());
        let weights = vld1q_u32([1u32, 2, 4, 8].as_ptr());
        while i < na && j < nb {
            let va = vld1q_u32(a.as_ptr().add(i));
            let vb = vld1q_u32(b.as_ptr().add(j));
            let m = vorrq_u32(
                vorrq_u32(vceqq_u32(va, vb), vceqq_u32(va, vextq_u32::<1>(vb, vb))),
                vorrq_u32(vceqq_u32(va, vextq_u32::<2>(vb, vb)), vceqq_u32(va, vextq_u32::<3>(vb, vb))),
            );
            let mask = vaddvq_u32(vandq_u32(m, weights)) as usize;
            let packed = vqtbl1q_u8(vreinterpretq_u8_u32(va), vld1q_u8(COMPRESS[mask].as_ptr()));
            vst1q_u8(dst.add(k) as *mut u8, packed);
            k += mask.count_ones() as usize;
            let amax = *a.get_unchecked(i + 3);
            let bmax = *b.get_unchecked(j + 3);
            if amax <= bmax {
                i += 4;
            }
            if bmax <= amax {
                j += 4;
            }
        }
        out.set_len(out.len() + k);
    }
    super::scalar::intersect(&a[i..], &b[j..], out);
}

pub fn count_byte(hay: &[u8], needle: u8) -> usize {
    let mut total = 0usize;
    let chunks = hay.chunks_exact(64);
    let rem = chunks.remainder();
    unsafe {
        let n = vdupq_n_u8(needle);
        let mut acc = vdupq_n_u8(0);
        let mut iters = 0;
        for c in chunks {
            let p = c.as_ptr();
            // ceq yields 0xFF (-1) per hit, so subtracting counts up.
            acc = vsubq_u8(acc, vceqq_u8(vld1q_u8(p), n));
            acc = vsubq_u8(acc, vceqq_u8(vld1q_u8(p.add(16)), n));
            acc = vsubq_u8(acc, vceqq_u8(vld1q_u8(p.add(32)), n));
            acc = vsubq_u8(acc, vceqq_u8(vld1q_u8(p.add(48)), n));
            iters += 1;
            if iters == 63 {
                total += vaddlvq_u8(acc) as usize;
                acc = vdupq_n_u8(0);
                iters = 0;
            }
        }
        total += vaddlvq_u8(acc) as usize;
    }
    total + super::scalar::count_byte(rem, needle)
}

pub fn ascii_lower_into(src: &[u8], dst: &mut Vec<u8>) {
    dst.clear();
    dst.reserve(src.len());
    let chunks = src.chunks_exact(16);
    let rem = chunks.remainder();
    unsafe {
        let mut d = dst.as_mut_ptr();
        let a = vdupq_n_u8(b'A');
        let z = vdupq_n_u8(b'Z');
        let bit = vdupq_n_u8(0x20);
        for c in chunks {
            let v = vld1q_u8(c.as_ptr());
            let upper = vandq_u8(vcgeq_u8(v, a), vcleq_u8(v, z));
            vst1q_u8(d, vorrq_u8(v, vandq_u8(upper, bit)));
            d = d.add(16);
        }
        dst.set_len(src.len() - rem.len());
    }
    dst.extend(rem.iter().map(u8::to_ascii_lowercase));
}

/// Stream VByte decode + delta restore. Full groups whose 16-byte load stays in bounds go
/// through NEON (tbl shuffle + 2-step prefix sum), the tail goes scalar.
pub fn svb_decode_delta(ctrl: &[u8], data: &[u8], n: usize, out: &mut Vec<u32>) {
    let groups = n / 4;
    out.reserve(n + 4);
    let (mut g, mut pos) = (0usize, 0usize);
    let mut prev = 0u32;
    unsafe {
        let dst = out.as_mut_ptr().add(out.len());
        let zero = vdupq_n_u32(0);
        let mut carry = vdupq_n_u32(0);
        while g < groups && pos + 16 <= data.len() {
            let c = *ctrl.get_unchecked(g) as usize;
            let raw = vld1q_u8(data.as_ptr().add(pos));
            let mut x = vreinterpretq_u32_u8(vqtbl1q_u8(raw, vld1q_u8(SVB_SHUF[c].as_ptr())));
            x = vaddq_u32(x, vextq_u32::<3>(zero, x));
            x = vaddq_u32(x, vextq_u32::<2>(zero, x));
            x = vaddq_u32(x, carry);
            vst1q_u32(dst.add(g * 4), x);
            carry = vdupq_laneq_u32::<3>(x);
            pos += SVB_LEN[c] as usize;
            g += 1;
        }
        if g > 0 {
            prev = vgetq_lane_u32::<0>(carry);
        }
        out.set_len(out.len() + g * 4);
    }
    let done = g * 4;
    // Scalar tail starts at a group boundary, so control bytes stay aligned.
    super::scalar::svb_decode_delta(&ctrl[g..], &data[pos..], n - done, prev, out);
}
