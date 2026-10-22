// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause
#![allow(non_snake_case)]

use crate::EncodeError;

pub const FE_JMPL: u32 = 0x8;
pub const FE_ADDR32: u32 = 0x10;
pub const FE_SEG_MASK: u32 = 0x7;
pub const FE_RC_MASK: u32 = 0x60;
pub const FE_RC_RN: u32 = 0x00;
pub const FE_RC_RD: u32 = 0x20;
pub const FE_RC_RU: u32 = 0x40;
pub const FE_RC_RZ: u32 = 0x60;

pub type FeCond = u32;
pub const FE_CC_O: FeCond = 0x00000;
pub const FE_CC_NO: FeCond = 0x10000;
pub const FE_CC_C: FeCond = 0x20000;
pub const FE_CC_B: FeCond = FE_CC_C;
pub const FE_CC_NAE: FeCond = FE_CC_C;
pub const FE_CC_NC: FeCond = 0x30000;
pub const FE_CC_AE: FeCond = FE_CC_NC;
pub const FE_CC_NB: FeCond = FE_CC_NC;
pub const FE_CC_Z: FeCond = 0x40000;
pub const FE_CC_E: FeCond = FE_CC_Z;
pub const FE_CC_NZ: FeCond = 0x50000;
pub const FE_CC_NE: FeCond = FE_CC_NZ;
pub const FE_CC_BE: FeCond = 0x60000;
pub const FE_CC_NA: FeCond = FE_CC_BE;
pub const FE_CC_A: FeCond = 0x70000;
pub const FE_CC_NBE: FeCond = FE_CC_A;
pub const FE_CC_S: FeCond = 0x80000;
pub const FE_CC_NS: FeCond = 0x90000;
pub const FE_CC_P: FeCond = 0xa0000;
pub const FE_CC_PE: FeCond = FE_CC_P;
pub const FE_CC_NP: FeCond = 0xb0000;
pub const FE_CC_PO: FeCond = FE_CC_NP;
pub const FE_CC_L: FeCond = 0xc0000;
pub const FE_CC_NGE: FeCond = FE_CC_L;
pub const FE_CC_GE: FeCond = 0xd0000;
pub const FE_CC_NL: FeCond = FE_CC_GE;
pub const FE_CC_LE: FeCond = 0xe0000;
pub const FE_CC_NG: FeCond = FE_CC_LE;
pub const FE_CC_G: FeCond = 0xf0000;
pub const FE_CC_NLE: FeCond = FE_CC_G;
pub const FE_CC_MASK: FeCond = 0xf0000;

macro_rules! register {
    ($name:ident, $constructor:ident, $min:literal, $max:literal) => {
        #[repr(transparent)]
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub struct $name {
            pub(crate) idx: u8,
        }

        impl $name {
            #[inline]
            pub const fn new(idx: u8) -> Result<Self, EncodeError> {
                if idx >= $min && idx <= $max {
                    Ok(Self { idx })
                } else {
                    Err(EncodeError::InvalidOperand)
                }
            }

            #[inline]
            pub const fn index(self) -> u8 {
                self.idx
            }
        }

        #[inline]
        pub const fn $constructor(idx: u8) -> Result<$name, EncodeError> {
            $name::new(idx)
        }
    };
}

register!(FeRegGP, FE_GP, 0, 15);
register!(FeRegGPH, FE_GPH, 4, 7);
register!(FeRegSREG, FE_SREG, 0, 5);
register!(FeRegST, FE_ST, 0, 7);
register!(FeRegMM, FE_MM, 0, 7);
register!(FeRegXMM, FE_XMM, 0, 31);
register!(FeRegMASK, FE_K, 0, 7);
register!(FeRegTMM, FE_TMM, 0, 7);
register!(FeRegCR, FE_CR, 0, 15);
register!(FeRegDR, FE_DR, 0, 15);

pub const FE_AX: FeRegGP = FeRegGP { idx: 0 };
pub const FE_CX: FeRegGP = FeRegGP { idx: 1 };
pub const FE_DX: FeRegGP = FeRegGP { idx: 2 };
pub const FE_BX: FeRegGP = FeRegGP { idx: 3 };
pub const FE_SP: FeRegGP = FeRegGP { idx: 4 };
pub const FE_BP: FeRegGP = FeRegGP { idx: 5 };
pub const FE_SI: FeRegGP = FeRegGP { idx: 6 };
pub const FE_DI: FeRegGP = FeRegGP { idx: 7 };
pub const FE_R8: FeRegGP = FeRegGP { idx: 8 };
pub const FE_R9: FeRegGP = FeRegGP { idx: 9 };
pub const FE_R10: FeRegGP = FeRegGP { idx: 10 };
pub const FE_R11: FeRegGP = FeRegGP { idx: 11 };
pub const FE_R12: FeRegGP = FeRegGP { idx: 12 };
pub const FE_R13: FeRegGP = FeRegGP { idx: 13 };
pub const FE_R14: FeRegGP = FeRegGP { idx: 14 };
pub const FE_R15: FeRegGP = FeRegGP { idx: 15 };
pub const FE_IP: FeRegGP = FeRegGP { idx: 0x20 };
pub const FE_NOREG: FeRegGP = FeRegGP { idx: 0x80 };
pub const FE_AH: FeRegGPH = FeRegGPH { idx: 4 };
pub const FE_CH: FeRegGPH = FeRegGPH { idx: 5 };
pub const FE_DH: FeRegGPH = FeRegGPH { idx: 6 };
pub const FE_BH: FeRegGPH = FeRegGPH { idx: 7 };
pub const FE_ES: FeRegSREG = FeRegSREG { idx: 0 };
pub const FE_CS: FeRegSREG = FeRegSREG { idx: 1 };
pub const FE_SS: FeRegSREG = FeRegSREG { idx: 2 };
pub const FE_DS: FeRegSREG = FeRegSREG { idx: 3 };
pub const FE_FS: FeRegSREG = FeRegSREG { idx: 4 };
pub const FE_GS: FeRegSREG = FeRegSREG { idx: 5 };
pub const FE_ST0: FeRegST = FeRegST { idx: 0 };
pub const FE_ST1: FeRegST = FeRegST { idx: 1 };
pub const FE_ST2: FeRegST = FeRegST { idx: 2 };
pub const FE_ST3: FeRegST = FeRegST { idx: 3 };
pub const FE_ST4: FeRegST = FeRegST { idx: 4 };
pub const FE_ST5: FeRegST = FeRegST { idx: 5 };
pub const FE_ST6: FeRegST = FeRegST { idx: 6 };
pub const FE_ST7: FeRegST = FeRegST { idx: 7 };
pub const FE_MM0: FeRegMM = FeRegMM { idx: 0 };
pub const FE_MM1: FeRegMM = FeRegMM { idx: 1 };
pub const FE_MM2: FeRegMM = FeRegMM { idx: 2 };
pub const FE_MM3: FeRegMM = FeRegMM { idx: 3 };
pub const FE_MM4: FeRegMM = FeRegMM { idx: 4 };
pub const FE_MM5: FeRegMM = FeRegMM { idx: 5 };
pub const FE_MM6: FeRegMM = FeRegMM { idx: 6 };
pub const FE_MM7: FeRegMM = FeRegMM { idx: 7 };
pub const FE_XMM0: FeRegXMM = FeRegXMM { idx: 0 };
pub const FE_XMM1: FeRegXMM = FeRegXMM { idx: 1 };
pub const FE_XMM2: FeRegXMM = FeRegXMM { idx: 2 };
pub const FE_XMM3: FeRegXMM = FeRegXMM { idx: 3 };
pub const FE_XMM4: FeRegXMM = FeRegXMM { idx: 4 };
pub const FE_XMM5: FeRegXMM = FeRegXMM { idx: 5 };
pub const FE_XMM6: FeRegXMM = FeRegXMM { idx: 6 };
pub const FE_XMM7: FeRegXMM = FeRegXMM { idx: 7 };
pub const FE_XMM8: FeRegXMM = FeRegXMM { idx: 8 };
pub const FE_XMM9: FeRegXMM = FeRegXMM { idx: 9 };
pub const FE_XMM10: FeRegXMM = FeRegXMM { idx: 10 };
pub const FE_XMM11: FeRegXMM = FeRegXMM { idx: 11 };
pub const FE_XMM12: FeRegXMM = FeRegXMM { idx: 12 };
pub const FE_XMM13: FeRegXMM = FeRegXMM { idx: 13 };
pub const FE_XMM14: FeRegXMM = FeRegXMM { idx: 14 };
pub const FE_XMM15: FeRegXMM = FeRegXMM { idx: 15 };
pub const FE_XMM16: FeRegXMM = FeRegXMM { idx: 16 };
pub const FE_XMM17: FeRegXMM = FeRegXMM { idx: 17 };
pub const FE_XMM18: FeRegXMM = FeRegXMM { idx: 18 };
pub const FE_XMM19: FeRegXMM = FeRegXMM { idx: 19 };
pub const FE_XMM20: FeRegXMM = FeRegXMM { idx: 20 };
pub const FE_XMM21: FeRegXMM = FeRegXMM { idx: 21 };
pub const FE_XMM22: FeRegXMM = FeRegXMM { idx: 22 };
pub const FE_XMM23: FeRegXMM = FeRegXMM { idx: 23 };
pub const FE_XMM24: FeRegXMM = FeRegXMM { idx: 24 };
pub const FE_XMM25: FeRegXMM = FeRegXMM { idx: 25 };
pub const FE_XMM26: FeRegXMM = FeRegXMM { idx: 26 };
pub const FE_XMM27: FeRegXMM = FeRegXMM { idx: 27 };
pub const FE_XMM28: FeRegXMM = FeRegXMM { idx: 28 };
pub const FE_XMM29: FeRegXMM = FeRegXMM { idx: 29 };
pub const FE_XMM30: FeRegXMM = FeRegXMM { idx: 30 };
pub const FE_XMM31: FeRegXMM = FeRegXMM { idx: 31 };
pub const FE_K0: FeRegMASK = FeRegMASK { idx: 0 };
pub const FE_K1: FeRegMASK = FeRegMASK { idx: 1 };
pub const FE_K2: FeRegMASK = FeRegMASK { idx: 2 };
pub const FE_K3: FeRegMASK = FeRegMASK { idx: 3 };
pub const FE_K4: FeRegMASK = FeRegMASK { idx: 4 };
pub const FE_K5: FeRegMASK = FeRegMASK { idx: 5 };
pub const FE_K6: FeRegMASK = FeRegMASK { idx: 6 };
pub const FE_K7: FeRegMASK = FeRegMASK { idx: 7 };
pub const FE_TMM0: FeRegTMM = FeRegTMM { idx: 0 };
pub const FE_TMM1: FeRegTMM = FeRegTMM { idx: 1 };
pub const FE_TMM2: FeRegTMM = FeRegTMM { idx: 2 };
pub const FE_TMM3: FeRegTMM = FeRegTMM { idx: 3 };
pub const FE_TMM4: FeRegTMM = FeRegTMM { idx: 4 };
pub const FE_TMM5: FeRegTMM = FeRegTMM { idx: 5 };
pub const FE_TMM6: FeRegTMM = FeRegTMM { idx: 6 };
pub const FE_TMM7: FeRegTMM = FeRegTMM { idx: 7 };

#[inline]
pub const fn FE_SEG(seg: FeRegSREG) -> u32 {
    (seg.idx as u32 + 1) & FE_SEG_MASK
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FeRegGPLH {
    pub(crate) idx: u8,
}

impl FeRegGPLH {
    #[inline]
    pub const fn new(idx: u8) -> Result<Self, EncodeError> {
        if idx < 16 || (idx >= 36 && idx <= 39) {
            Ok(Self { idx })
        } else {
            Err(EncodeError::InvalidOperand)
        }
    }

    #[inline]
    pub const fn index(self) -> u8 {
        self.idx
    }
}

impl From<FeRegGP> for FeRegGPLH {
    #[inline]
    fn from(reg: FeRegGP) -> Self {
        Self { idx: reg.idx }
    }
}

impl From<FeRegGPH> for FeRegGPLH {
    #[inline]
    fn from(reg: FeRegGPH) -> Self {
        Self {
            idx: reg.idx | 0x20,
        }
    }
}

#[inline]
pub const fn FE_GPLH(idx: u8) -> Result<FeRegGPLH, EncodeError> {
    FeRegGPLH::new(idx)
}

#[inline]
pub fn FE_MAKE_GPLH(reg: impl Into<FeRegGPLH>) -> FeRegGPLH {
    reg.into()
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FeMem {
    pub(crate) flags: u8,
    pub(crate) base: FeRegGP,
    pub(crate) scale: u8,
    pub(crate) idx: FeRegGP,
    pub(crate) off: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FeMemV {
    pub(crate) flags: u8,
    pub(crate) base: FeRegGP,
    pub(crate) scale: u8,
    pub(crate) idx: FeRegXMM,
    pub(crate) off: i32,
}

impl FeMem {
    #[inline]
    pub const fn new(
        base: FeRegGP,
        scale: u8,
        idx: FeRegGP,
        off: i32,
    ) -> Result<Self, EncodeError> {
        if !(base.idx < 16 || base.idx == FE_IP.idx || base.idx == FE_NOREG.idx)
            || !(idx.idx < 16 && idx.idx != FE_SP.idx || idx.idx == FE_NOREG.idx)
            || if idx.idx == FE_NOREG.idx {
                scale != 0
            } else {
                !matches!(scale, 1 | 2 | 4 | 8)
            }
        {
            return Err(EncodeError::InvalidOperand);
        }
        Ok(Self {
            flags: 0,
            base,
            scale,
            idx,
            off,
        })
    }
}

impl FeMemV {
    #[inline]
    pub const fn new(
        base: FeRegGP,
        scale: u8,
        idx: FeRegXMM,
        off: i32,
    ) -> Result<Self, EncodeError> {
        if !(base.idx < 16 || base.idx == FE_NOREG.idx)
            || idx.idx >= 32
            || !matches!(scale, 1 | 2 | 4 | 8)
        {
            return Err(EncodeError::InvalidOperand);
        }
        Ok(Self {
            flags: 0,
            base,
            scale,
            idx,
            off,
        })
    }
}

macro_rules! memory_accessors {
    ($name:ident, $index:ident) => {
        impl $name {
            #[inline]
            pub const fn with_flags(mut self, flags: u8) -> Self {
                self.flags = flags;
                self
            }

            #[inline]
            pub const fn flags(self) -> u8 {
                self.flags
            }

            #[inline]
            pub const fn base(self) -> FeRegGP {
                self.base
            }

            #[inline]
            pub const fn scale(self) -> u8 {
                self.scale
            }

            #[inline]
            pub const fn index(self) -> $index {
                self.idx
            }

            #[inline]
            pub const fn offset(self) -> i32 {
                self.off
            }
        }
    };
}

memory_accessors!(FeMem, FeRegGP);
memory_accessors!(FeMemV, FeRegXMM);

#[inline]
pub const fn FE_MEM(
    base: FeRegGP,
    scale: u8,
    idx: FeRegGP,
    off: i32,
) -> Result<FeMem, EncodeError> {
    FeMem::new(base, scale, idx, off)
}

#[inline]
pub const fn FE_MEMV(
    base: FeRegGP,
    scale: u8,
    idx: FeRegXMM,
    off: i32,
) -> Result<FeMemV, EncodeError> {
    FeMemV::new(base, scale, idx, off)
}

const _: () = {
    assert!(core::mem::size_of::<FeMem>() == 8);
    assert!(core::mem::size_of::<FeMemV>() == 8);
    assert!(core::mem::align_of::<FeMem>() == 4);
    assert!(core::mem::align_of::<FeMemV>() == 4);
    assert!(!core::mem::needs_drop::<FeMem>());
    assert!(!core::mem::needs_drop::<FeMemV>());
};
