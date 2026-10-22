// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use crate::raw::Encode2Runtime;
pub use crate::types::*;

pub struct Runtime;

const VEX_WPP_SHIFT: u32 = 8;
const VEX_WPP_MASK: u32 = 0x83 << VEX_WPP_SHIFT;
const VEX_MMM_SHIFT: u32 = 10;
const VEX_MMM_MASK: u32 = 0x1f << VEX_MMM_SHIFT;
const DOWNGRADE_VEX: u32 = 1 << 24;
const DOWNGRADE_FLIPW: u32 = 1 << 25;

#[inline]
fn enc_mem_common(buf: &mut [u8], ripoff: usize, mem: FeMem, reg: u32, disp8scale: i32) -> usize {
    let reg = reg & 7;
    let mut sib = 0x20;
    let mut withsib = false;
    let mut dispsz = 0;
    let mut off = mem.off;
    let mut mode = 0;
    if mem.idx.idx < 0x80 {
        let scale = u32::from(mem.scale);
        if scale.wrapping_sub(1) >= 8 || scale & scale.wrapping_sub(1) != 0 {
            return 0;
        }
        let scale = u32::from(scale & 0xa != 0) | (u32::from(scale & 0xc != 0) << 1);
        sib = scale << 6 | (u32::from(mem.idx.idx) & 7) << 3;
        withsib = true;
    } else if mem.scale != 0 {
        return 0;
    }
    if mem.base.idx >= 0x20 {
        if mem.base.idx >= FE_NOREG.idx {
            buf[0] = (reg << 3 | 4) as u8;
            buf[1] = (sib | 5) as u8;
            Runtime::enc_imm(&mut buf[2..], i64::from(off), 4);
            return ripoff + 6;
        }
        if mem.base.idx == FE_IP.idx {
            if withsib {
                return 0;
            }
            buf[0] = (reg << 3 | 5) as u8;
            let displacement = (off as u32).wrapping_sub(ripoff as u32).wrapping_sub(5);
            Runtime::enc_imm(&mut buf[1..], i64::from(displacement), 4);
            return ripoff + 5;
        }
        return 0;
    }
    let rm = u32::from(mem.base.idx) & 7;
    if off != 0 {
        if disp8scale == 0 {
            let short = i32::from(off as i8) == off;
            mode = if short { 0x40 } else { 0x80 };
            dispsz = if short { 1 } else { 4 };
        } else if (1..32).contains(&disp8scale) {
            if off as u32 & ((1_u32 << disp8scale) - 1) == 0
                && Runtime::op_imm_n(i64::from(off >> disp8scale), 1)
            {
                off >>= disp8scale;
                mode = 0x40;
                dispsz = 1;
            } else {
                mode = 0x80;
                dispsz = 4;
            }
        } else {
            return 0;
        }
    } else if rm == 5 {
        dispsz = 1;
        mode = 0x40;
    }
    if withsib || rm == 4 {
        buf[0] = (mode | reg << 3 | 4) as u8;
        buf[1] = (sib | rm) as u8;
        Runtime::enc_imm(&mut buf[2..], i64::from(off), 4);
        ripoff + 2 + dispsz
    } else {
        buf[0] = (mode | reg << 3 | rm) as u8;
        Runtime::enc_imm(&mut buf[1..], i64::from(off), 4);
        ripoff + 1 + dispsz
    }
}

#[inline]
fn enc_mem_vsib(
    buf: &mut [u8],
    ripoff: usize,
    mem: FeMemV,
    reg: u32,
    _forcesib: u32,
    disp8scale: i32,
) -> usize {
    let mem = FeMem {
        flags: 0,
        base: mem.base,
        scale: mem.scale,
        idx: FeRegGP { idx: mem.idx.idx },
        off: mem.off,
    };
    enc_mem_common(buf, ripoff, mem, reg, disp8scale)
}

#[inline]
fn enc_evex_common(buf: &mut [u8], opcode: u32, base: u32, idx: u32, reg: u32, vvvv: u32) -> usize {
    buf[0] = 0x62;
    let mut byte1 = (opcode & VEX_MMM_MASK) >> VEX_MMM_SHIFT;
    if reg & 8 == 0 {
        byte1 |= 0x80;
    }
    if idx & 8 == 0 {
        byte1 |= 0x40;
    }
    if base & 8 == 0 {
        byte1 |= 0x20;
    }
    if reg & 16 == 0 {
        byte1 |= 0x10;
    }
    if base & 16 != 0 {
        byte1 |= 0x08;
    }
    buf[1] = byte1 as u8;
    let mut byte2 = (opcode & VEX_WPP_MASK) >> VEX_WPP_SHIFT;
    if idx & 16 == 0 {
        byte2 |= 0x04;
    }
    byte2 |= (!vvvv & 15) << 3;
    buf[2] = byte2 as u8;
    let mut byte3 = opcode & 0xff;
    if vvvv & 16 == 0 {
        byte3 |= 0x08;
    }
    buf[3] = byte3 as u8;
    buf[4] = (opcode >> 16) as u8;
    5
}

#[inline]
fn enc_evex_to_vex(opcode: u32) -> u32 {
    if opcode & DOWNGRADE_FLIPW != 0 {
        opcode ^ 0x8000
    } else {
        opcode
    }
}

impl Encode2Runtime for Runtime {
    #[inline]
    fn op_imm_n(imm: i64, size: usize) -> bool {
        match size {
            0 => imm == 0,
            1 => i64::from(imm as i8) == imm,
            2 => i64::from(imm as i16) == imm,
            3 => imm & 0xff_ffff == imm,
            4 => i64::from(imm as i32) == imm,
            8 => true,
            _ => false,
        }
    }

    #[cold]
    fn enc_seg67(buf: &mut [u8], flags: u32) -> usize {
        let mut index = 0;
        if flags & FE_SEG_MASK != 0 {
            buf[index] = (0x0065_643e_362e_2600_u64 >> (8 * (flags & FE_SEG_MASK))) as u8;
            index += 1;
        }
        if flags & FE_ADDR32 != 0 {
            buf[index] = 0x67;
            index += 1;
        }
        index
    }

    #[inline]
    fn enc_rex_mem(mem: FeMem, reg: u32) -> u32 {
        let value = reg
            | u32::from(mem.flags)
            | u32::from(mem.base.idx) << 8
            | u32::from(mem.idx.idx) << 24;
        let masked = value & 0x0800_0808;
        if masked == 0 {
            0
        } else {
            u32::from((masked.wrapping_mul(1 | 1 << 15 | 1 << 25) >> 26) as u8) + 0x40
        }
    }

    #[inline]
    fn enc_imm(buf: &mut [u8], imm: i64, size: usize) {
        buf[..size].copy_from_slice(&imm.to_le_bytes()[..size]);
    }

    #[inline]
    fn enc_mem(
        buf: &mut [u8],
        ripoff: usize,
        mut mem: FeMem,
        reg: u32,
        forcesib: u32,
        disp8scale: i32,
    ) -> usize {
        if mem.idx.idx == 4 {
            return 0;
        }
        if forcesib != 0 && mem.idx.idx == FE_NOREG.idx {
            mem.scale = 1;
            mem.idx = FeRegGP { idx: 4 };
        }
        enc_mem_common(buf, ripoff, mem, reg, disp8scale)
    }

    #[inline]
    fn enc_vex_common(
        buf: &mut [u8],
        opcode: u32,
        base: u32,
        idx: u32,
        reg: u32,
        vvvv: u32,
    ) -> usize {
        if (base | idx | reg | vvvv) & 16 != 0 {
            return 0;
        }
        let vex3 = (base | idx) & 8 != 0 || opcode & 0xfc00 != 0x0400;
        let length;
        if vex3 {
            buf[0] = 0xc4;
            let mut byte1 = (opcode & VEX_MMM_MASK) >> VEX_MMM_SHIFT;
            if reg & 8 == 0 {
                byte1 |= 0x80;
            }
            if idx & 8 == 0 {
                byte1 |= 0x40;
            }
            if base & 8 == 0 {
                byte1 |= 0x20;
            }
            buf[1] = byte1 as u8;
            let mut byte2 = (opcode & VEX_WPP_MASK) >> VEX_WPP_SHIFT;
            if opcode & 0x20 != 0 {
                byte2 |= 4;
            }
            byte2 |= (vvvv ^ 15) << 3;
            buf[2] = byte2 as u8;
            length = 4;
        } else {
            buf[0] = 0xc5;
            let mut byte2 = opcode >> VEX_WPP_SHIFT & 3;
            if opcode & 0x20 != 0 {
                byte2 |= 4;
            }
            if reg & 8 == 0 {
                byte2 |= 0x80;
            }
            byte2 |= (vvvv ^ 15) << 3;
            buf[1] = byte2 as u8;
            length = 3;
        }
        buf[length - 1] = (opcode >> 16) as u8;
        length
    }

    #[inline]
    fn enc_vex_reg(buf: &mut [u8], opcode: u32, rm: u32, reg: u32, vvvv: u32) -> usize {
        let offset = Self::enc_vex_common(buf, opcode, rm, 0, reg, vvvv);
        buf[offset] = (0xc0 | reg << 3 & 0x38 | rm & 7) as u8;
        if offset != 0 { offset + 1 } else { 0 }
    }

    #[inline]
    fn enc_evex_reg(buf: &mut [u8], opcode: u32, rm: u32, reg: u32, vvvv: u32) -> usize {
        let offset = if (rm | reg | vvvv) & 16 == 0 && opcode & DOWNGRADE_VEX != 0 {
            Self::enc_vex_common(buf, enc_evex_to_vex(opcode), rm, 0, reg, vvvv)
        } else {
            enc_evex_common(buf, opcode, rm, 0, reg, vvvv)
        };
        buf[offset] = (0xc0 | reg << 3 & 0x38 | rm & 7) as u8;
        offset + 1
    }

    #[inline]
    fn enc_evex_xmm(buf: &mut [u8], opcode: u32, rm: u32, reg: u32, vvvv: u32) -> usize {
        let offset = if (rm | reg | vvvv) & 16 == 0 && opcode & DOWNGRADE_VEX != 0 {
            Self::enc_vex_common(buf, enc_evex_to_vex(opcode), rm, 0, reg, vvvv)
        } else {
            enc_evex_common(buf, opcode, rm & 15, rm >> 1, reg, vvvv)
        };
        buf[offset] = (0xc0 | reg << 3 & 0x38 | rm & 7) as u8;
        offset + 1
    }

    #[inline]
    fn enc_vex_mem(
        buf: &mut [u8],
        opcode: u32,
        rm: FeMem,
        reg: u32,
        vvvv: u32,
        ripoff: usize,
        forcesib: u32,
        disp8scale: i32,
    ) -> usize {
        let offset = Self::enc_vex_common(
            buf,
            opcode,
            u32::from(rm.base.idx),
            u32::from(rm.idx.idx),
            reg,
            vvvv,
        );
        let memoff = Self::enc_mem(
            &mut buf[offset..],
            ripoff + offset,
            rm,
            reg,
            forcesib,
            disp8scale,
        );
        if offset != 0 && memoff != 0 {
            memoff
        } else {
            0
        }
    }

    #[inline]
    fn enc_evex_mem(
        buf: &mut [u8],
        opcode: u32,
        rm: FeMem,
        reg: u32,
        vvvv: u32,
        ripoff: usize,
        forcesib: u32,
        mut disp8scale: i32,
    ) -> usize {
        let base = u32::from(rm.base.idx);
        let idx = u32::from(rm.idx.idx);
        let offset = if (base | idx | reg | vvvv) & 16 == 0 && opcode & DOWNGRADE_VEX != 0 {
            disp8scale = 0;
            Self::enc_vex_common(buf, enc_evex_to_vex(opcode), base, idx, reg, vvvv)
        } else {
            enc_evex_common(buf, opcode, base, idx, reg, vvvv)
        };
        let memoff = Self::enc_mem(
            &mut buf[offset..],
            ripoff + offset,
            rm,
            reg,
            forcesib,
            disp8scale,
        );
        if offset != 0 && memoff != 0 {
            memoff
        } else {
            0
        }
    }

    #[inline]
    fn enc_vex_vsib(
        buf: &mut [u8],
        opcode: u32,
        rm: FeMemV,
        reg: u32,
        vvvv: u32,
        ripoff: usize,
        forcesib: u32,
        disp8scale: i32,
    ) -> usize {
        let offset = Self::enc_vex_common(
            buf,
            opcode,
            u32::from(rm.base.idx),
            u32::from(rm.idx.idx),
            reg,
            vvvv,
        );
        let memoff = enc_mem_vsib(
            &mut buf[offset..],
            ripoff + offset,
            rm,
            reg,
            forcesib,
            disp8scale,
        );
        if offset != 0 && memoff != 0 {
            memoff
        } else {
            0
        }
    }

    #[inline]
    fn enc_evex_vsib(
        buf: &mut [u8],
        opcode: u32,
        rm: FeMemV,
        reg: u32,
        _vvvv: u32,
        ripoff: usize,
        forcesib: u32,
        disp8scale: i32,
    ) -> usize {
        if opcode & 7 == 0 {
            return 0;
        }
        let idx = u32::from(rm.idx.idx);
        let offset = enc_evex_common(buf, opcode, u32::from(rm.base.idx), idx & 15, reg, idx & 16);
        let memoff = enc_mem_vsib(
            &mut buf[offset..],
            ripoff + offset,
            rm,
            reg,
            forcesib,
            disp8scale,
        );
        if offset != 0 && memoff != 0 {
            memoff
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immediate_width_boundaries() {
        for (size, low, high) in [
            (0, 0, 0),
            (1, -128, 127),
            (2, -32768, 32767),
            (3, 0, 0xff_ffff),
            (4, i32::MIN as i64, i32::MAX as i64),
        ] {
            assert!(Runtime::op_imm_n(low, size));
            assert!(Runtime::op_imm_n(high, size));
            assert!(!Runtime::op_imm_n(low - 1, size));
            assert!(!Runtime::op_imm_n(high + 1, size));
        }
        assert!(Runtime::op_imm_n(i64::MIN, 8));
        assert!(Runtime::op_imm_n(i64::MAX, 8));
        for size in [0, 1, 2, 3, 4, 8] {
            let mut buffer = [0xa5; 10];
            Runtime::enc_imm(&mut buffer[1..], 0x1234_5678_9abc_def0, size);
            assert_eq!(
                &buffer[1..size + 1],
                &0x1234_5678_9abc_def0_i64.to_le_bytes()[..size]
            );
            assert_eq!(buffer[0], 0xa5);
            assert_eq!(buffer[size + 1], 0xa5);
        }
    }

    #[test]
    fn memory_short_long_and_rip_displacements() {
        let mut buffer = [0; 32];
        let mem = FeMem::new(FE_BP, 0, FE_NOREG, 0).unwrap();
        assert_eq!(Runtime::enc_mem(&mut buffer, 2, mem, 1, 0, 0), 4);
        assert_eq!(&buffer[..2], &[0x4d, 0]);
        let mem = FeMem::new(FE_IP, 0, FE_NOREG, 100).unwrap();
        assert_eq!(Runtime::enc_mem(&mut buffer, 3, mem, 2, 0, 0), 8);
        assert_eq!(&buffer[..5], &[0x15, 92, 0, 0, 0]);
        let mem = FeMem::new(FE_NOREG, 4, FE_CX, -1).unwrap();
        assert_eq!(Runtime::enc_mem(&mut buffer, 2, mem, 0, 0, 0), 8);
        assert_eq!(&buffer[..6], &[4, 0x8d, 255, 255, 255, 255]);
        for (off, expected, compressed) in [
            (127, 2, 5),
            (128, 5, 2),
            (8128, 5, 2),
            (8192, 5, 5),
            (-8192, 5, 2),
            (-8256, 5, 5),
        ] {
            let mem = FeMem::new(FE_AX, 0, FE_NOREG, off).unwrap();
            assert_eq!(Runtime::enc_mem(&mut buffer, 0, mem, 0, 0, 0), expected);
            assert_eq!(Runtime::enc_mem(&mut buffer, 0, mem, 0, 0, 6), compressed);
        }
    }

    #[test]
    fn memory_refusals_and_forced_sib() {
        let mut buffer = [0; 32];
        let mut mem = FeMem::new(FE_AX, 1, FE_CX, 0).unwrap();
        for scale in [0, 3, 5, 7, 9, 255] {
            mem.scale = scale;
            assert_eq!(Runtime::enc_mem(&mut buffer, 0, mem, 0, 0, 0), 0);
        }
        mem.scale = 1;
        mem.idx = FE_SP;
        assert_eq!(Runtime::enc_mem(&mut buffer, 0, mem, 0, 0, 0), 0);
        mem.idx = FE_NOREG;
        mem.scale = 0;
        assert_eq!(Runtime::enc_mem(&mut buffer, 0, mem, 0, 1, 0), 2);
        assert_eq!(&buffer[..2], &[4, 0x20]);
        mem.base = FE_IP;
        assert_eq!(Runtime::enc_mem(&mut buffer, 0, mem, 0, 1, 0), 0);
    }

    #[test]
    fn vex_prefix_selection_and_register_refusal() {
        let mut buffer = [0; 32];
        assert_eq!(
            Runtime::enc_vex_common(&mut buffer, 0x580400, 0, 0, 0, 0),
            3
        );
        assert_eq!(&buffer[..3], &[0xc5, 0xf8, 0x58]);
        assert_eq!(
            Runtime::enc_vex_common(&mut buffer, 0x580400, 8, 0, 0, 0),
            4
        );
        assert_eq!(&buffer[..4], &[0xc4, 0xc1, 0x78, 0x58]);
        for operands in [[16, 0, 0, 0], [0, 16, 0, 0], [0, 0, 16, 0], [0, 0, 0, 16]] {
            assert_eq!(
                Runtime::enc_vex_common(
                    &mut buffer,
                    0x580400,
                    operands[0],
                    operands[1],
                    operands[2],
                    operands[3]
                ),
                0
            );
        }
    }

    #[test]
    fn evex_downgrade_and_extended_registers() {
        let mut buffer = [0; 32];
        assert_eq!(Runtime::enc_evex_xmm(&mut buffer, 0x0158_0400, 0, 0, 0), 4);
        assert_eq!(&buffer[..4], &[0xc5, 0xf8, 0x58, 0xc0]);
        assert_eq!(Runtime::enc_evex_xmm(&mut buffer, 0x0158_0400, 16, 0, 0), 6);
        assert_eq!(&buffer[..6], &[0x62, 0xb1, 0x7c, 8, 0x58, 0xc0]);
        assert_eq!(Runtime::enc_evex_xmm(&mut buffer, 0x0358_8400, 0, 0, 0), 4);
        assert_eq!(&buffer[..4], &[0xc5, 0xf8, 0x58, 0xc0]);
    }

    #[test]
    fn vsib_mask_index_and_compression() {
        let mut buffer = [0; 32];
        let mem = FeMemV::new(FE_AX, 4, FE_XMM31, 64).unwrap();
        assert_eq!(
            Runtime::enc_evex_vsib(&mut buffer, 0x900800, mem, 1, 0, 0, 0, 6),
            0
        );
        assert_eq!(
            Runtime::enc_evex_vsib(&mut buffer, 0x900801, mem, 1, 0, 0, 0, 6),
            8
        );
        assert_eq!(&buffer[..8], &[0x62, 0xb2, 0x7c, 1, 0x90, 0x4c, 0xb8, 1]);
        assert_eq!(
            Runtime::enc_vex_vsib(&mut buffer, 0x900800, mem, 1, 0, 0, 0, 0),
            0
        );
    }
}
