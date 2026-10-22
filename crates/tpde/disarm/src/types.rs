// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

pub const DA_EXT_UXTB: u8 = 0;
pub const DA_EXT_UXTH: u8 = 1;
pub const DA_EXT_UXTW: u8 = 2;
pub const DA_EXT_UXTX: u8 = 3;
pub const DA_EXT_SXTB: u8 = 4;
pub const DA_EXT_SXTH: u8 = 5;
pub const DA_EXT_SXTW: u8 = 6;
pub const DA_EXT_SXTX: u8 = 7;
pub const DA_EXT_LSL: u8 = 8;
pub const DA_EXT_LSR: u8 = 9;
pub const DA_EXT_ASR: u8 = 10;
pub const DA_EXT_ROR: u8 = 11;

pub const DA_VA_8B: u8 = 0;
pub const DA_VA_16B: u8 = 1;
pub const DA_VA_4H: u8 = 2;
pub const DA_VA_8H: u8 = 3;
pub const DA_VA_2S: u8 = 4;
pub const DA_VA_4S: u8 = 5;
pub const DA_VA_1D: u8 = 6;
pub const DA_VA_2D: u8 = 7;
pub const DA_VA_2H: u8 = 8;
pub const DA_VA_1Q: u8 = 9;

pub const DA_OP_NONE: u8 = 0;
pub const DA_OP_REGGP: u8 = 1;
pub const DA_OP_REGGPINC: u8 = 2;
pub const DA_OP_REGGPEXT: u8 = 3;
pub const DA_OP_REGSP: u8 = 4;
pub const DA_OP_REGFP: u8 = 5;
pub const DA_OP_REGVEC: u8 = 6;
pub const DA_OP_REGVTBL: u8 = 7;
pub const DA_OP_REGVIDX: u8 = 8;
pub const DA_OP_REGVTBLIDX: u8 = 9;
pub const DA_OP_MEMUOFF: u8 = 10;
pub const DA_OP_MEMSOFF: u8 = 11;
pub const DA_OP_MEMSOFFPRE: u8 = 12;
pub const DA_OP_MEMSOFFPOST: u8 = 13;
pub const DA_OP_MEMREG: u8 = 14;
pub const DA_OP_MEMREGPOST: u8 = 15;
pub const DA_OP_MEMINC: u8 = 16;
pub const DA_OP_COND: u8 = 17;
pub const DA_OP_PRFOP: u8 = 18;
pub const DA_OP_SYSREG: u8 = 19;
pub const DA_OP_IMMSMALL: u8 = 20;
pub const DA_OP_SIMM: u8 = 21;
pub const DA_OP_UIMM: u8 = 22;
pub const DA_OP_UIMMSHIFT: u8 = 23;
pub const DA_OP_IMMLARGE: u8 = 24;
pub const DA_OP_RELADDR: u8 = 25;
pub const DA_OP_IMMFLOAT: u8 = 26;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct Operand {
    pub kind: u8,
    pub reg: u8,
    pub data: u16,
}

impl Operand {
    pub const NONE: Self = Self {
        kind: DA_OP_NONE,
        reg: 0,
        data: 0,
    };

    #[inline]
    pub const fn gp_sf(self) -> bool {
        self.data & 1 != 0
    }

    #[inline]
    pub const fn gp_ext(self) -> u8 {
        (self.data as u8) >> 1
    }

    #[inline]
    pub const fn gp_shift(self) -> u8 {
        (self.data >> 8) as u8
    }

    #[inline]
    pub const fn fp_size(self) -> u8 {
        self.data as u8
    }

    #[inline]
    pub const fn vector_arrangement(self) -> u8 {
        self.data as u8
    }

    #[inline]
    pub const fn element_size(self) -> u8 {
        self.data as u8
    }

    #[inline]
    pub const fn element_index(self) -> u8 {
        (self.data >> 8) as u8
    }

    #[inline]
    pub const fn table_count(self) -> u8 {
        (self.data >> 8) as u8
    }

    #[inline]
    pub const fn table_element_size(self) -> u8 {
        self.data as u8 & 15
    }

    #[inline]
    pub const fn table_element_index(self) -> u8 {
        (self.data as u8) >> 4
    }

    #[inline]
    pub const fn memory_scaled(self) -> bool {
        self.data & 1 != 0
    }

    #[inline]
    pub const fn memory_ext(self) -> u8 {
        (self.data as u8 >> 1) & 15
    }

    #[inline]
    pub const fn memory_shift(self) -> u8 {
        self.data as u8 >> 5
    }

    #[inline]
    pub const fn memory_offset_register(self) -> u8 {
        (self.data >> 8) as u8
    }

    #[inline]
    pub const fn signed_immediate(self) -> i16 {
        self.data as i16
    }

    #[inline]
    pub const fn condition(self) -> u8 {
        self.data as u8
    }

    #[inline]
    pub const fn immediate_mask(self) -> bool {
        self.reg & 1 != 0
    }

    #[inline]
    pub const fn immediate_shift(self) -> u8 {
        self.reg >> 1
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct Instruction {
    pub mnem: u16,
    pub ops: [Operand; 5],
    pub imm64: u64,
}

impl Instruction {
    #[inline]
    pub const fn float8(self) -> f32 {
        f32::from_bits(self.imm64 as u32)
    }
}

pub type Da64Op = Operand;
pub type Da64Inst = Instruction;
pub type Da64Ext = u32;
pub type Da64VectorArrangement = u32;
pub type Da64OpType = u32;

const _: () = {
    assert!(core::mem::size_of::<Operand>() == 4);
    assert!(core::mem::align_of::<Operand>() == 2);
    assert!(core::mem::offset_of!(Operand, kind) == 0);
    assert!(core::mem::offset_of!(Operand, reg) == 1);
    assert!(core::mem::offset_of!(Operand, data) == 2);
    assert!(core::mem::size_of::<Instruction>() == 32);
    assert!(core::mem::align_of::<Instruction>() == 8);
    assert!(core::mem::offset_of!(Instruction, mnem) == 0);
    assert!(core::mem::offset_of!(Instruction, ops) == 2);
    assert!(core::mem::offset_of!(Instruction, imm64) == 24);
    assert!(!core::mem::needs_drop::<Operand>());
    assert!(!core::mem::needs_drop::<Instruction>());
};
