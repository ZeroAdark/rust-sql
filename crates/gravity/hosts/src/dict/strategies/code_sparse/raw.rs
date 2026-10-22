//! `code_sparse`'s unsafe steps (rule R6, the module's raw home): the NEON unpack of widths 1-16 and the wide walk's
//! unaligned word reads.
#![allow(unsafe_code)]

/// Codes from 0 of a frame of widths 1-16, 16 a step by NEON: each non-base row with its code to `f`, in row order; the
/// step's first code after the last full one.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub(super) fn neon(base: u32, wu: usize, bits: &[u8], values: usize, f: &mut impl FnMut(usize, u32)) -> usize {
    // SAFETY: NEON is baseline on aarch64.
    unsafe { if wu <= 8 { neon8(base, wu, bits, values, f) } else { neon16(base, wu, bits, values, f) } }
}

/// The 8 bytes of `bits` from byte `at` as a native word; the caller's `at + 8 <= bits.len()`.
#[inline(always)]
pub(super) fn word(bits: &[u8], at: usize) -> u64 {
    // SAFETY: the word's 8 bytes lie inside `bits` (the caller reads only codes whose whole window is readable).
    unsafe { (bits.as_ptr().add(at) as *const u64).read_unaligned() }
}

/// Widths 1-8: 16 codes per step, each code's low / high byte gathered by a table lookup, shifted and masked in a 16-bit
/// lane; the step's first code after the last full one.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn neon8(base: u32, wu: usize, bits: &[u8], values: usize, f: &mut impl FnMut(usize, u32)) -> usize {
    use core::arch::aarch64::*;
    let (mut lo, mut hi, mut sh) = ([0u8; 16], [0u8; 16], [0i16; 16]);
    for j in 0..16 {
        (lo[j], hi[j], sh[j]) = ((j * wu >> 3) as u8, (j * wu >> 3) as u8 + 1, -((j * wu & 7) as i16));
    }
    let (tlo, thi, s0, s1) = (vld1q_u8(lo.as_ptr()), vld1q_u8(hi.as_ptr()), vld1q_s16(sh.as_ptr()), vld1q_s16(sh.as_ptr().add(8)));
    let vmask = vdupq_n_u16(((1u32 << wu) - 1) as u16);
    let mut i = 0;
    while i + 16 <= values && i * wu / 8 + 16 <= bits.len() {
        let src = vld1q_u8(bits.as_ptr().add(i * wu / 8));
        let (b0, b1) = (vqtbl1q_u8(src, tlo), vqtbl1q_u8(src, thi));
        let c0 = vandq_u16(vshlq_u16(vreinterpretq_u16_u8(vzip1q_u8(b0, b1)), s0), vmask);
        let c1 = vandq_u16(vshlq_u16(vreinterpretq_u16_u8(vzip2q_u8(b0, b1)), s1), vmask);
        let codes = vcombine_u8(vmovn_u16(c0), vmovn_u16(c1));
        if vmaxvq_u8(codes) != 0 {
            let nib = vshrn_n_u16(vreinterpretq_u16_u8(vmvnq_u8(vceqzq_u8(codes))), 4);
            let mut m = vget_lane_u64(vreinterpret_u64_u8(nib), 0) & 0x1111_1111_1111_1111;
            let mut cb = [0u8; 16];
            vst1q_u8(cb.as_mut_ptr(), codes);
            while m != 0 {
                let j = (m.trailing_zeros() >> 2) as usize;
                f(i + j, base + cb[j] as u32);
                m &= m - 1;
            }
        }
        i += 16;
    }
    i
}

/// Widths 9-16: 8 codes per 32-bit-lane gather (3 bytes a code), two groups (16 codes) per zero test.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn neon16(base: u32, wu: usize, bits: &[u8], values: usize, f: &mut impl FnMut(usize, u32)) -> usize {
    use core::arch::aarch64::*;
    let (mut ix, mut sh) = ([[0xffu8; 16]; 2], [0i32; 8]);
    for j in 0..8 {
        for k in 0..3 {
            // bytes past the group's `wu` bytes are never part of a code (index 0xff reads 0)
            ix[j / 4][j % 4 * 4 + k] = if (j * wu >> 3) + k < wu { ((j * wu >> 3) + k) as u8 } else { 0xff };
        }
        sh[j] = -((j * wu & 7) as i32);
    }
    let (t0, t1, s0, s1) = (vld1q_u8(ix[0].as_ptr()), vld1q_u8(ix[1].as_ptr()), vld1q_s32(sh.as_ptr()), vld1q_s32(sh.as_ptr().add(4)));
    let vmask = vdupq_n_u32((1u32 << wu) - 1);
    let group = |src| vcombine_u16(vmovn_u32(vandq_u32(vshlq_u32(vreinterpretq_u32_u8(vqtbl1q_u8(src, t0)), s0), vmask)), vmovn_u32(vandq_u32(vshlq_u32(vreinterpretq_u32_u8(vqtbl1q_u8(src, t1)), s1), vmask)));
    let mut i = 0;
    while i + 16 <= values && i * wu / 8 + wu + 16 <= bits.len() {
        let q = bits.as_ptr().add(i * wu / 8);
        let (c0, c1) = (group(vld1q_u8(q)), group(vld1q_u8(q.add(wu))));
        if vmaxvq_u16(vorrq_u16(c0, c1)) != 0 {
            let ne = vcombine_u8(vmovn_u16(vtstq_u16(c0, c0)), vmovn_u16(vtstq_u16(c1, c1)));
            let mut m = vget_lane_u64(vreinterpret_u64_u8(vshrn_n_u16(vreinterpretq_u16_u8(ne), 4)), 0) & 0x1111_1111_1111_1111;
            let mut cb = [0u16; 16];
            vst1q_u16(cb.as_mut_ptr(), c0);
            vst1q_u16(cb.as_mut_ptr().add(8), c1);
            while m != 0 {
                let j = (m.trailing_zeros() >> 2) as usize;
                f(i + j, base + cb[j] as u32);
                m &= m - 1;
            }
        }
        i += 16;
    }
    i
}
