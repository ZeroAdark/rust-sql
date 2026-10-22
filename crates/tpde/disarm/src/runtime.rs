// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

macro_rules! register {
    ($name:ident) => {
        #[repr(transparent)]
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct $name(u8);

        impl $name {
            #[inline]
            pub const fn new(value: u8) -> Option<Self> {
                if value <= 31 {
                    Some(Self(value))
                } else {
                    None
                }
            }

            #[inline]
            pub const fn wrapping(value: u32) -> Self {
                Self((value & 31) as u8)
            }

            #[inline]
            pub const fn value(self) -> u8 {
                self.0
            }
        }
    };
}

register!(GReg);
register!(GRegZr);
register!(GRegSp);
register!(VReg);

impl From<GReg> for GRegZr {
    #[inline]
    fn from(value: GReg) -> Self {
        Self(value.0)
    }
}

impl From<GReg> for GRegSp {
    #[inline]
    fn from(value: GReg) -> Self {
        Self(value.0)
    }
}

pub const DA_ZR: GRegZr = GRegZr(31);
pub const DA_SP: GRegSp = GRegSp(31);
pub type Cond = u32;
pub type PrfOp = u32;
pub type Da64Cond = Cond;
pub type Da64PrfOp = PrfOp;
pub const DA_EQ: Cond = 0;
pub const DA_NE: Cond = 1;
pub const DA_CS: Cond = 2;
pub const DA_HS: Cond = DA_CS;
pub const DA_CC: Cond = 3;
pub const DA_LO: Cond = DA_CC;
pub const DA_MI: Cond = 4;
pub const DA_PL: Cond = 5;
pub const DA_VS: Cond = 6;
pub const DA_VC: Cond = 7;
pub const DA_HI: Cond = 8;
pub const DA_LS: Cond = 9;
pub const DA_GE: Cond = 10;
pub const DA_LT: Cond = 11;
pub const DA_GT: Cond = 12;
pub const DA_LE: Cond = 13;
pub const DA_AL: Cond = 14;
pub const DA_NV: Cond = 15;
pub const DA_PRF_PLDL1KEEP: PrfOp = 0;
pub const DA_PRF_PLDL1STRM: PrfOp = 1;
pub const DA_PRF_PLDL2KEEP: PrfOp = 2;
pub const DA_PRF_PLDL2STRM: PrfOp = 3;
pub const DA_PRF_PLDL3KEEP: PrfOp = 4;
pub const DA_PRF_PLDL3STRM: PrfOp = 5;
pub const DA_PRF_PLIL1KEEP: PrfOp = 8;
pub const DA_PRF_PLIL1STRM: PrfOp = 9;
pub const DA_PRF_PLIL2KEEP: PrfOp = 10;
pub const DA_PRF_PLIL2STRM: PrfOp = 11;
pub const DA_PRF_PLIL3KEEP: PrfOp = 12;
pub const DA_PRF_PLIL3STRM: PrfOp = 13;
pub const DA_PRF_PSTL1KEEP: PrfOp = 16;
pub const DA_PRF_PSTL1STRM: PrfOp = 17;
pub const DA_PRF_PSTL2KEEP: PrfOp = 18;
pub const DA_PRF_PSTL2STRM: PrfOp = 19;
pub const DA_PRF_PSTL3KEEP: PrfOp = 20;
pub const DA_PRF_PSTL3STRM: PrfOp = 21;

#[inline]
pub const fn da_sext(imm: i64, bits: u32) -> i64 {
    assert!(bits > 0 && bits <= 64);
    if bits == 64 {
        return imm;
    }
    let mask = (1u64 << bits) - 1;
    let sign = 1u64 << (bits - 1);
    (((imm as u64 & mask) ^ sign).wrapping_sub(sign)) as i64
}

#[inline]
pub const fn da_immadd(value: i64) -> u32 {
    let (inst, uval) = if value < 0 {
        (0x4000_0000, (value as u64).wrapping_neg())
    } else {
        (0, value as u64)
    };
    if uval & 0xfff == uval {
        return inst | ((uval << 10) as u32);
    }
    if uval & 0xfff000 == uval {
        return inst | (1 << 22) | ((uval >> 2) as u32);
    }
    u32::MAX
}

pub const fn da_immlogical(mut value: u64, is64: u32) -> u32 {
    if is64 == 0 {
        value |= value << 32;
    }
    if value == 0 || value == u64::MAX {
        return u32::MAX;
    }
    let clz = value.leading_zeros();
    let iclz = (!value).leading_zeros();
    let ctz = value.trailing_zeros();
    let ictz = (!value).trailing_zeros();
    let mut count = value.count_ones();
    let mut shift = 64;
    let mut imms = 0x1780 | (count - 1);
    let mut elog = 0;
    while elog < 6 {
        if clz + ctz == shift - count {
            return (((if ctz != 0 { shift - ctz } else { 0 }) << 6) | (imms & 0x103f)) << 10;
        }
        if iclz + ictz == count {
            return ((iclz << 6) | (imms & 0x103f)) << 10;
        }
        elog += 1;
        count >>= 1;
        shift >>= 1;
        imms >>= 1;
        let mask = (1u64 << shift) - 1;
        if value & mask != (value >> shift & mask) {
            return u32::MAX;
        }
    }
    u32::MAX
}

#[inline]
pub const fn da_immfmov32(value: f32) -> u32 {
    let bits = value.to_bits();
    if bits & 0x7ffff == 0 && ((bits >> 25 & 0x3f).wrapping_sub(0x1f)) <= 1 {
        (bits >> 19 & 0x7f) | (bits >> 24 & 0x80)
    } else {
        u32::MAX
    }
}

#[inline]
pub const fn da_immfmov64(value: f64) -> u32 {
    let bits = value.to_bits();
    if bits & 0xffffffffffff == 0 && ((bits >> 54 & 0x1ff).wrapping_sub(0xff)) <= 1 {
        ((bits >> 48 & 0x7f) | (bits >> 56 & 0x80)) as u32
    } else {
        u32::MAX
    }
}

#[inline]
const fn movi_encode(imm8: u64, op: bool, cmode: u32) -> u32 {
    (if op { 0x2000_0000 } else { 0 })
        | (cmode << 12)
        | (((imm8 << 11) & 0x70000) as u32)
        | (((imm8 << 5) & 0x3e0) as u32)
}

pub const fn da_immsimdmovi(value: u64) -> u32 {
    if value == 0 || value == u64::MAX {
        return movi_encode(value & 0xff, false, 0xe);
    }
    if value & 0xffff_ffff == value >> 32 {
        if value & 0xffff != (value >> 16 & 0xffff) {
            let value32 = value as u32;
            let clz = value32.leading_zeros() >> 3;
            let iclz = (!value32).leading_zeros() >> 3;
            let ctz = value32.trailing_zeros() >> 3;
            let ictz = (!value32).trailing_zeros() >> 3;
            if clz + ctz >= 3 {
                return movi_encode(value >> (ctz * 8), false, ctz * 2);
            }
            if iclz + ictz >= 3 {
                return movi_encode(!value >> (ictz * 8), true, ictz * 2);
            }
            if clz + ictz >= 3 {
                return movi_encode(value >> (ictz * 8), false, 0xc + ictz - 1);
            }
            if iclz + ctz >= 3 {
                return movi_encode(!value >> (ctz * 8), true, 0xc + ctz - 1);
            }
        } else if value & 0xff != (value >> 8 & 0xff) {
            let low = value & 0xff;
            let high = value >> 8 & 0xff;
            if high == 0 {
                return movi_encode(low, false, 8);
            }
            if high == 0xff {
                return movi_encode(!low, true, 8);
            }
            if low == 0 {
                return movi_encode(high, false, 0xa);
            }
            if low == 0xff {
                return movi_encode(!high, true, 0xa);
            }
        } else {
            return movi_encode(value & 0xff, false, 0xe);
        }
    }
    let mut imm8 = 0;
    let mut i = 0;
    while i < 8 {
        let byte = value >> (8 * i) & 0xff;
        if byte == 0xff {
            imm8 |= 1 << i;
        } else if byte != 0 {
            return u32::MAX;
        }
        i += 1;
    }
    movi_encode(imm8, true, 0xe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_are_bounded_and_preserve_upstream_wrapping() {
        for value in 0..=255 {
            assert_eq!(
                GReg::new(value).map(GReg::value),
                (value <= 31).then_some(value)
            );
            assert_eq!(GReg::wrapping(value as u32).value(), value & 31);
            assert_eq!(
                GRegZr::from(GReg::wrapping(value as u32)).value(),
                value & 31
            );
            assert_eq!(
                GRegSp::from(GReg::wrapping(value as u32)).value(),
                value & 31
            );
        }
        assert_eq!(DA_ZR.value(), 31);
        assert_eq!(DA_SP.value(), 31);
        assert_eq!(DA_CS, DA_HS);
        assert_eq!(DA_CC, DA_LO);
    }

    #[test]
    fn immediate_helper_boundaries() {
        assert_eq!(da_immadd(0), 0);
        assert_eq!(da_immadd(4095), 4095 << 10);
        assert_eq!(da_immadd(-4096), 0x4040_0400);
        assert_eq!(da_immadd(4097), u32::MAX);
        assert_eq!(da_immadd(i64::MIN), u32::MAX);
        for bits in 1..64 {
            assert_eq!(da_sext(-1, bits), -1);
            assert_eq!(da_sext(0, bits), 0);
            assert_eq!(da_sext(1i64 << (bits - 1), bits), -(1i64 << (bits - 1)));
        }
        assert_eq!(da_sext(i64::MIN, 64), i64::MIN);
        assert_eq!(da_immlogical(0, 1), u32::MAX);
        assert_eq!(da_immlogical(u64::MAX, 1), u32::MAX);
        assert_ne!(da_immlogical(0xff00_ff00_ff00_ff00, 1), u32::MAX);
        assert_eq!(da_immlogical(0x1234_5678_9abc_def0, 1), u32::MAX);
    }

    #[test]
    fn floating_immediates_cover_all_encodings() {
        for imm in 0..256u32 {
            let bits = (imm & 0x80) << 24
                | if imm & 0x40 != 0 {
                    0x3e00_0000
                } else {
                    0x4000_0000
                }
                | (imm & 0x3f) << 19;
            let value = f32::from_bits(bits);
            assert_eq!(da_immfmov32(value), imm);
            assert_eq!(da_immfmov64(value as f64), imm);
            assert_eq!(da_immfmov32(f32::from_bits(bits | 1)), u32::MAX);
            assert_eq!(
                da_immfmov64(f64::from_bits((value as f64).to_bits() | 1)),
                u32::MAX
            );
        }
        for value in [0.0, -0.0, f32::INFINITY, f32::NEG_INFINITY, f32::NAN] {
            assert_eq!(da_immfmov32(value), u32::MAX);
            assert_eq!(da_immfmov64(value as f64), u32::MAX);
        }
    }

    #[test]
    fn simd_repetition_and_mask_paths() {
        for byte in 0..256u64 {
            assert_ne!(da_immsimdmovi(byte * 0x0101_0101_0101_0101), u32::MAX);
            let mut mask = 0;
            for i in 0..8 {
                if byte & (1 << i) != 0 {
                    mask |= 0xff << (i * 8);
                }
            }
            assert_ne!(da_immsimdmovi(mask), u32::MAX);
        }
        for value in [
            0x0012_0000_0012_0000,
            0xffff_ff12_ffff_ff12,
            0x0000_12ff_0000_12ff,
            0xffff_ed00_ffff_ed00,
            0x0012_0012_0012_0012,
            0xff12_ff12_ff12_ff12,
            0x1200_1200_1200_1200,
            0x12ff_12ff_12ff_12ff,
        ] {
            assert_ne!(da_immsimdmovi(value), u32::MAX);
        }
        assert_eq!(da_immsimdmovi(0x1234_5678_9abc_def0), u32::MAX);
    }
}
