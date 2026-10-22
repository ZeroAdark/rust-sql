// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use crate::generated::{da64_classify_impl, da64_decode_operands, DecoderHelpers};
use crate::types::*;

#[inline]
pub fn da64_classify(inst: u32) -> u32 {
    da64_classify_impl(inst)
}

pub fn da64_decode(inst: u32, ddi: &mut Instruction) {
    ddi.ops = [Operand::NONE; 5];
    let mnem = da64_classify(inst);
    ddi.mnem = mnem as u16;
    da64_decode_operands::<Helpers>(inst, mnem, ddi);
}

#[inline]
pub fn decode(inst: u32) -> Instruction {
    let mut result = Instruction::default();
    da64_decode(inst, &mut result);
    result
}

#[inline]
fn op(kind: u8, reg: u32, data: u16) -> Operand {
    Operand {
        kind,
        reg: reg as u8,
        data,
    }
}

#[inline]
fn bytes(lo: u32, hi: u32) -> u16 {
    u16::from_le_bytes([lo as u8, hi as u8])
}

fn immlogical(sf: u32, n: u32, immr: u32, imms: u32) -> u64 {
    if n == 0 && imms == 0x3f {
        return 0;
    }
    let len = 31 - ((n << 6) | (!imms & 0x3f)).leading_zeros();
    let levels = (1 << len) - 1;
    let s = imms & levels;
    let r = immr & levels;
    let esize = 1 << len;
    // Upstream shifts by 64 for the reserved all-ones element; define its mask.
    let mut welem = if s == 63 {
        u64::MAX
    } else {
        (1u64 << (s + 1)) - 1
    };
    if r != 0 {
        welem = (welem >> r) | (welem << (esize - r));
    }
    if esize < 64 {
        welem &= (1u64 << esize) - 1;
    }
    let mut wmask = 0;
    let mut i = 0;
    while i < if sf == 0 { 32 } else { 64 } {
        wmask |= welem << i;
        i += esize;
    }
    wmask
}

struct Helpers;

#[allow(non_snake_case)]
impl DecoderHelpers for Helpers {
    type State = Instruction;
    type Op = Operand;

    #[inline]
    fn set_operand(ddi: &mut Instruction, index: usize, operand: Operand) {
        ddi.ops[index] = operand;
    }

    #[inline]
    fn sext(imm: u64, bits: u32) -> u64 {
        let sign = 1u64 << (bits - 1);
        if imm & sign != 0 {
            (imm ^ sign).wrapping_sub(sign)
        } else {
            imm
        }
    }

    #[inline]
    fn clz(v: u32, sz: u32) -> u32 {
        if v == 0 {
            sz
        } else {
            v.leading_zeros().wrapping_add(sz).wrapping_sub(32)
        }
    }

    #[inline]
    fn ctz(v: u32) -> u32 {
        v.trailing_zeros()
    }

    #[inline]
    fn OPreggp(idx: u32, sf: bool) -> Operand {
        op(DA_OP_REGGP, idx, u16::from(sf))
    }

    #[inline]
    fn OPreggpinc(idx: u32) -> Operand {
        op(DA_OP_REGGPINC, idx, 1)
    }

    #[inline]
    fn OPreggpsp(idx: u32, sf: bool) -> Operand {
        op(
            if idx != 31 { DA_OP_REGGP } else { DA_OP_REGSP },
            idx,
            u16::from(sf),
        )
    }

    #[inline]
    fn OPreggpmaysp(maysp: bool, idx: u32, sf: bool) -> Operand {
        op(
            if idx < 31 || !maysp {
                DA_OP_REGGP
            } else {
                DA_OP_REGSP
            },
            idx,
            u16::from(sf),
        )
    }

    #[inline]
    fn OPreggpprf(isprf: bool, idx: u32, sf: bool) -> Operand {
        op(
            if isprf { DA_OP_PRFOP } else { DA_OP_REGGP },
            idx,
            u16::from(sf),
        )
    }

    #[inline]
    fn OPreggpext(idx: u32, sf: bool, ext: u32, shift: u32) -> Operand {
        op(
            DA_OP_REGGPEXT,
            idx,
            bytes(u32::from(sf) | ((ext & 127) << 1), shift),
        )
    }

    #[inline]
    fn OPregfp(idx: u32, size: u32) -> Operand {
        op(DA_OP_REGFP, idx, bytes(size, 0))
    }

    #[inline]
    fn OPregvec(idx: u32, esize: u32, q: bool) -> Operand {
        op(
            DA_OP_REGVEC,
            idx,
            bytes((esize << 1).wrapping_add(u32::from(q)), 0),
        )
    }

    #[inline]
    fn OPregvidx(idx: u32, esize: u32, elem: u32) -> Operand {
        op(DA_OP_REGVIDX, idx, bytes(esize, elem))
    }

    #[inline]
    fn OPregvtbl(idx: u32, esize: u32, q: bool, cnt: u32) -> Operand {
        op(
            DA_OP_REGVTBL,
            idx,
            bytes((esize << 1).wrapping_add(u32::from(q)), cnt),
        )
    }

    #[inline]
    fn OPregvtblidx(idx: u32, esize: u32, elem: u32, cnt: u32) -> Operand {
        op(
            DA_OP_REGVTBLIDX,
            idx,
            bytes((esize & 15) | ((elem & 15) << 4), cnt),
        )
    }

    #[inline]
    fn OPmemuoff(idx: u32, off: u16) -> Operand {
        op(DA_OP_MEMUOFF, idx, off)
    }

    #[inline]
    fn OPmemsoff(idx: u32, off: i16) -> Operand {
        op(DA_OP_MEMSOFF, idx, off as u16)
    }

    #[inline]
    fn OPmemsoffpre(idx: u32, off: i16) -> Operand {
        op(DA_OP_MEMSOFFPRE, idx, off as u16)
    }

    #[inline]
    fn OPmemsoffpost(idx: u32, off: i16) -> Operand {
        op(DA_OP_MEMSOFFPOST, idx, off as u16)
    }

    #[inline]
    fn OPmemreg(idx: u32, offreg: u32, ext: u32, scale: bool, shift: u32) -> Operand {
        op(
            DA_OP_MEMREG,
            idx,
            bytes(
                u32::from(scale) | ((ext & 15) << 1) | ((shift & 7) << 5),
                offreg,
            ),
        )
    }

    #[inline]
    fn OPmemregsimdpost(idx: u32, offreg: u32, constoff: u32) -> Operand {
        if offreg == 31 {
            return op(DA_OP_MEMSOFFPOST, idx, constoff as u16);
        }
        op(
            DA_OP_MEMREGPOST,
            idx,
            bytes(u32::from(DA_EXT_UXTX) << 1, offreg),
        )
    }

    #[inline]
    fn OPmeminc(idx: u32) -> Operand {
        op(DA_OP_MEMINC, idx, 0)
    }

    #[inline]
    fn OPimmsmall(imm6: u32) -> Operand {
        op(DA_OP_IMMSMALL, 0, imm6 as u16)
    }

    #[inline]
    fn OPsimm(imm: i16) -> Operand {
        op(DA_OP_SIMM, 0, imm as u16)
    }

    #[inline]
    fn OPuimm(imm: u16) -> Operand {
        op(DA_OP_UIMM, 0, imm)
    }

    #[inline]
    fn OPuimmshift(imm: u16, msl: bool, shift: u32) -> Operand {
        op(DA_OP_UIMMSHIFT, u32::from(msl) | ((shift & 127) << 1), imm)
    }

    #[inline]
    fn OPreladdr(ddi: &mut Instruction, imm: i64) -> Operand {
        ddi.imm64 = imm as u64;
        op(DA_OP_RELADDR, 0, 0)
    }

    #[inline]
    fn OPimmlogical(ddi: &mut Instruction, sf: u32, n: u32, immr: u32, imms: u32) -> Operand {
        ddi.imm64 = immlogical(sf, n, immr, imms);
        op(DA_OP_IMMLARGE, 0, sf as u16)
    }

    #[inline]
    fn OPimmsimdmask(ddi: &mut Instruction, imm8: u8) -> Operand {
        let mut result = 0;
        for i in 0..8 {
            if imm8 & (1 << i) != 0 {
                result |= 0xffu64 << (i * 8);
            }
        }
        ddi.imm64 = result;
        op(DA_OP_IMMLARGE, 0, 1)
    }

    #[inline]
    fn OPimmfloatzero(ddi: &mut Instruction) -> Operand {
        ddi.imm64 &= 0xffff_ffff_0000_0000;
        op(DA_OP_IMMFLOAT, 0, 0x100)
    }

    #[inline]
    fn OPimmfloat(ddi: &mut Instruction, imm8: u8) -> Operand {
        let mut result = u32::from(imm8 & 0x80) << 24;
        result |= if imm8 & 0x40 != 0 {
            0x3e00_0000
        } else {
            0x4000_0000
        };
        result |= u32::from(imm8 & 0x3f) << 19;
        ddi.imm64 = (ddi.imm64 & 0xffff_ffff_0000_0000) | u64::from(result);
        op(DA_OP_IMMFLOAT, 0, u16::from(imm8))
    }

    #[inline]
    fn OPsysreg(reg: u32) -> Operand {
        op(DA_OP_SYSREG, 0, reg as u16)
    }

    #[inline]
    fn OPcond(cond: u32) -> Operand {
        op(DA_OP_COND, 0, bytes(cond, 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_bitfields_truncate_at_c_widths() {
        assert_eq!(Helpers::OPreggpsp(31, true).kind, DA_OP_REGSP);
        assert_eq!(Helpers::OPreggpsp(287, true).kind, DA_OP_REGGP);
        assert_eq!(Helpers::OPreggpsp(287, true).reg, 31);
        assert_eq!(Helpers::OPreggpmaysp(true, 32, false).kind, DA_OP_REGSP);
        assert_eq!(Helpers::OPreggpmaysp(false, 31, true).kind, DA_OP_REGGP);
        let gp = Helpers::OPreggpext(0x123, true, 0x185, 0x102);
        assert_eq!(
            gp,
            Operand {
                kind: DA_OP_REGGPEXT,
                reg: 0x23,
                data: 0x20b
            }
        );
        assert!(gp.gp_sf());
        assert_eq!(gp.gp_ext(), 5);
        assert_eq!(gp.gp_shift(), 2);
        let mem = Helpers::OPmemreg(3, 0x114, 0x23, true, 0xa);
        assert_eq!(mem.data, 0x1447);
        assert!(mem.memory_scaled());
        assert_eq!(mem.memory_ext(), 3);
        assert_eq!(mem.memory_shift(), 2);
        assert_eq!(mem.memory_offset_register(), 20);
        let table = Helpers::OPregvtblidx(5, 0x16, 0x2b, 0x103);
        assert_eq!(table.data, 0x3b6);
        assert_eq!(table.table_element_size(), 6);
        assert_eq!(table.table_element_index(), 11);
        assert_eq!(table.table_count(), 3);
        let shifted = Helpers::OPuimmshift(37, true, 0x185);
        assert_eq!(shifted.reg, 11);
        assert!(shifted.immediate_mask());
        assert_eq!(shifted.immediate_shift(), 5);
        assert_eq!(
            Helpers::OPmemregsimdpost(31, 31, 0x1_ffff).signed_immediate(),
            -1
        );
    }

    #[test]
    fn float_immediates_preserve_the_other_union_half() {
        let mut ddi = Instruction {
            imm64: 0x1234_5678_aaaa_aaaa,
            ..Instruction::default()
        };
        for (imm, expected) in [(0x70, 1.0f32), (0xf0, -1.0), (0, 2.0), (0x40, 0.125)] {
            let operand = Helpers::OPimmfloat(&mut ddi, imm);
            assert_eq!(operand.kind, DA_OP_IMMFLOAT);
            assert_eq!(operand.data, u16::from(imm));
            assert_eq!(ddi.float8(), expected);
            assert_eq!(ddi.imm64 >> 32, 0x1234_5678);
        }
        let operand = Helpers::OPimmfloatzero(&mut ddi);
        assert_eq!(operand.data, 0x100);
        assert_eq!(ddi.imm64, 0x1234_5678_0000_0000);
    }

    #[test]
    fn bit_immediates_extend_rotate_and_replicate() {
        assert_eq!(Helpers::sext(0x81, 8), (-127i64) as u64);
        assert_eq!(Helpers::sext(0x181, 8), 0x81);
        assert_eq!(Helpers::sext(0x107, 8), 0x107);
        assert_eq!(Helpers::clz(0, 16), 16);
        assert_eq!(Helpers::clz(1, 16), 15);
        assert_eq!(Helpers::clz(0x8000_0000, 16), u32::MAX - 15);
        assert_eq!(Helpers::ctz(0), 32);
        assert_eq!(immlogical(0, 0, 0, 0x3c), 0x5555_5555);
        assert_eq!(immlogical(1, 0, 1, 0x3c), 0xaaaa_aaaa_aaaa_aaaa);
        assert_eq!(immlogical(1, 1, 1, 0), 0x8000_0000_0000_0000);
        assert_eq!(immlogical(1, 0, 0, 0x3f), 0);
        assert_eq!(immlogical(1, 1, 0, 0x3f), u64::MAX);
        let mut ddi = Instruction::default();
        Helpers::OPimmsimdmask(&mut ddi, 0x81);
        assert_eq!(ddi.imm64, 0xff00_0000_0000_00ff);
    }

    #[test]
    fn complete_decode_clears_operands_and_reuses_immediate_storage() {
        let mut ddi = Instruction {
            mnem: u16::MAX,
            ops: [Operand {
                kind: 255,
                reg: 255,
                data: 65535,
            }; 5],
            imm64: 0x1234_5678_9abc_def0,
        };
        da64_decode(0xd503_201f, &mut ddi);
        assert_ne!(ddi.mnem, 0);
        assert_eq!(
            ddi.ops[0],
            Operand {
                kind: DA_OP_UIMM,
                reg: 0,
                data: 0
            }
        );
        assert_eq!(ddi.ops[1..], [Operand::NONE; 4]);
        assert_eq!(ddi.imm64, 0x1234_5678_9abc_def0);
        da64_decode(0x1400_0001, &mut ddi);
        assert_eq!(ddi.ops[0].kind, DA_OP_RELADDR);
        assert_eq!(ddi.imm64, 4);
        da64_decode(0xffff_ffff, &mut ddi);
        assert_eq!(ddi.mnem, 0);
        assert_eq!(ddi.ops, [Operand::NONE; 5]);
        assert_eq!(ddi.imm64, 4);
    }
}
