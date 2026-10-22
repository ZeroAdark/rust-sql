// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use crate::{generated, Instruction, Operand, *};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatError {
    MalformedOperand,
    OutputTooSmall,
}

struct Buffer {
    bytes: [u8; 128],
    len: usize,
}

impl Buffer {
    fn append(&mut self, bytes: &[u8]) -> Result<(), FormatError> {
        let end = self.len + bytes.len();
        self.bytes
            .get_mut(self.len..end)
            .ok_or(FormatError::MalformedOperand)?
            .copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }

    fn byte(&mut self, byte: u8) -> Result<(), FormatError> {
        self.append(&[byte])
    }

    fn decimal(&mut self, value: u16) -> Result<(), FormatError> {
        if value > 64 {
            return Err(FormatError::MalformedOperand);
        }
        if value >= 10 {
            self.byte(b'0' + (value / 10) as u8)?;
        }
        self.byte(b'0' + (value % 10) as u8)
    }

    fn hex(&mut self, value: u64) -> Result<(), FormatError> {
        let digits = (64 - (value | 1).leading_zeros()).div_ceil(4) as usize;
        let end = self.len + digits;
        let dst = self
            .bytes
            .get_mut(self.len..end)
            .ok_or(FormatError::MalformedOperand)?;
        let mut remaining = value;
        for byte in dst.iter_mut().rev() {
            *byte = b"0123456789abcdef"[(remaining & 15) as usize];
            remaining >>= 4;
        }
        self.len = end;
        Ok(())
    }

    fn signed_hex(&mut self, value: i16, skip: usize) -> Result<(), FormatError> {
        self.append(if value < 0 {
            &b"], #-0x"[skip..]
        } else {
            &b"], #0x"[skip..]
        })?;
        self.hex(u64::from(value.unsigned_abs()))
    }

    fn reg(&mut self, prefix: u8, index: u8) -> Result<(), FormatError> {
        if index > 31 {
            return Err(FormatError::MalformedOperand);
        }
        self.byte(prefix)?;
        self.decimal(u16::from(index))
    }

    fn gp(&mut self, sf: bool, index: u8, sp: bool) -> Result<(), FormatError> {
        if index == 31 {
            self.append(match (sf, sp) {
                (false, false) => b"wzr",
                (true, false) => b"xzr",
                (false, true) => b"wsp",
                (true, true) => b"sp",
            })
        } else {
            self.reg(if sf { b'x' } else { b'w' }, index)
        }
    }

    fn arrangement(&mut self, arrangement: u8) -> Result<(), FormatError> {
        let entry = match arrangement {
            0 => &b".8b"[..],
            1 => b".16b",
            2 => b".4h",
            3 => b".8h",
            4 => b".2s",
            5 => b".4s",
            6 => b".1d",
            7 => b".2d",
            8 => b".2h",
            9 => b".1q",
            _ => return Err(FormatError::MalformedOperand),
        };
        self.append(entry)
    }

    fn operand(&mut self, op: &Operand, imm64: u64, addr: u64) -> Result<(), FormatError> {
        let low = op.data as u8;
        let high = (op.data >> 8) as u8;
        match op.kind {
            DA_OP_REGGP | DA_OP_REGGPINC => {
                self.gp(low & 1 != 0, op.reg, false)?;
                if op.kind == DA_OP_REGGPINC {
                    self.byte(b'!')?;
                }
            }
            DA_OP_REGGPEXT => {
                self.gp(low & 1 != 0, op.reg, false)?;
                let extension = match low >> 1 {
                    0 => &b", uxtb"[..],
                    1 => b", uxth",
                    2 => b", uxtw",
                    3 => b", uxtx",
                    4 => b", sxtb",
                    5 => b", sxth",
                    6 => b", sxtw",
                    7 => b", sxtx",
                    8 => b", lsl",
                    9 => b", lsr",
                    10 => b", asr",
                    11 => b", ror",
                    _ => return Err(FormatError::MalformedOperand),
                };
                self.append(extension)?;
                self.append(b" #")?;
                self.decimal(u16::from(high))?;
            }
            DA_OP_REGSP => self.append(if low & 1 != 0 { b"sp" } else { b"wsp" })?,
            DA_OP_REGFP => {
                let prefix = *b"bhsdq"
                    .get(low as usize)
                    .ok_or(FormatError::MalformedOperand)?;
                self.reg(prefix, op.reg)?;
            }
            DA_OP_REGVEC => {
                self.reg(b'v', op.reg)?;
                self.arrangement(low)?;
            }
            DA_OP_REGVIDX => {
                self.reg(b'v', op.reg)?;
                self.append(match low {
                    0 => &b".b["[..],
                    1 => b".h[",
                    2 => b".s[",
                    3 => b".d[",
                    4 => b".q[",
                    5 => b".2b[",
                    6 => b".4b[",
                    7 => b".2h[",
                    _ => return Err(FormatError::MalformedOperand),
                })?;
                self.decimal(u16::from(high))?;
                self.byte(b']')?;
            }
            DA_OP_REGVTBL | DA_OP_REGVTBLIDX => {
                self.byte(b'{')?;
                self.reg(b'v', op.reg)?;
                let indexed = op.kind == DA_OP_REGVTBLIDX;
                let suffix = if indexed {
                    Some(match low & 15 {
                        0 => &b".b"[..],
                        1 => b".h",
                        2 => b".s",
                        3 => b".d",
                        _ => return Err(FormatError::MalformedOperand),
                    })
                } else {
                    None
                };
                if let Some(suffix) = suffix {
                    self.append(suffix)?;
                } else {
                    self.arrangement(low)?;
                }
                if high > 1 {
                    self.byte(b'-')?;
                    self.reg(b'v', op.reg.wrapping_add(high).wrapping_sub(1) & 31)?;
                    if let Some(suffix) = suffix {
                        self.append(suffix)?;
                    } else {
                        self.arrangement(low)?;
                    }
                }
                if indexed {
                    self.append(b" }[")?;
                    self.decimal(u16::from(low >> 4))?;
                    self.byte(b']')?;
                } else {
                    self.byte(b'}')?;
                }
            }
            DA_OP_MEMUOFF | DA_OP_MEMSOFF | DA_OP_MEMSOFFPRE | DA_OP_MEMSOFFPOST => {
                self.byte(b'[')?;
                self.gp(true, op.reg, true)?;
                match op.kind {
                    DA_OP_MEMUOFF => {
                        if op.data != 0 {
                            self.append(b", #0x")?;
                            self.hex(u64::from(op.data))?;
                        }
                        self.byte(b']')?;
                    }
                    DA_OP_MEMSOFF => {
                        if op.data != 0 {
                            self.signed_hex(op.data as i16, 1)?;
                        }
                        self.byte(b']')?;
                    }
                    DA_OP_MEMSOFFPRE => {
                        self.signed_hex(op.data as i16, 1)?;
                        self.append(b"]!")?;
                    }
                    _ => self.signed_hex(op.data as i16, 0)?,
                }
            }
            DA_OP_MEMREG => {
                self.byte(b'[')?;
                self.gp(true, op.reg, true)?;
                self.append(b", ")?;
                let extension = (low >> 1) & 15;
                self.gp(extension & 3 == 3, high, false)?;
                let compact = usize::from(
                    extension
                        .checked_sub(1)
                        .ok_or(FormatError::MalformedOperand)?
                        >> 1,
                );
                if low & 1 == 0 {
                    let table = b", uxtw]\0]\0      , sxtw]\0, sxtx]\0";
                    let start = compact * 8;
                    let count = if compact == 1 { 1 } else { 7 };
                    self.append(
                        table
                            .get(start..start + count)
                            .ok_or(FormatError::MalformedOperand)?,
                    )?;
                } else {
                    let table = concat!(
                        ", uxtw #0]\0, lsl #0]\0 , sxtw #0]\0, sxtx #0]\0",
                        ", uxtw #1]\0, lsl #1]\0 , sxtw #1]\0, sxtx #1]\0",
                        ", uxtw #2]\0, lsl #2]\0 , sxtw #2]\0, sxtx #2]\0",
                        ", uxtw #3]\0, lsl #3]\0 , sxtw #3]\0, sxtx #3]\0",
                        ", uxtw #4]\0, lsl #4]\0 , sxtw #4]\0, sxtx #4]\0",
                    )
                    .as_bytes();
                    let start = 11 * (compact + 4 * usize::from(low >> 5));
                    // The LSL entry includes an embedded NUL in C's ten-byte result.
                    self.append(
                        table
                            .get(start..start + 10)
                            .ok_or(FormatError::MalformedOperand)?,
                    )?;
                }
            }
            DA_OP_MEMREGPOST => {
                self.byte(b'[')?;
                self.gp(true, op.reg, true)?;
                self.append(b"], ")?;
                self.gp(true, high, false)?;
            }
            DA_OP_MEMINC => {
                self.byte(b'[')?;
                self.gp(true, op.reg, false)?;
                self.append(b"]!")?;
            }
            DA_OP_IMMSMALL => {
                self.byte(b'#')?;
                self.decimal(op.data)?;
            }
            DA_OP_IMMLARGE | DA_OP_RELADDR => {
                self.append(b"#0x")?;
                self.hex(if op.kind == DA_OP_RELADDR {
                    imm64.wrapping_add(addr)
                } else {
                    imm64
                })?;
            }
            DA_OP_SIMM => self.signed_hex(op.data as i16, 3)?,
            DA_OP_UIMM | DA_OP_UIMMSHIFT => {
                self.append(b"#0x")?;
                self.hex(u64::from(op.data))?;
                if op.kind == DA_OP_UIMMSHIFT {
                    self.append(if op.reg & 1 == 0 {
                        b", lsl #"
                    } else {
                        b", msl #"
                    })?;
                    self.decimal(u16::from(op.reg >> 1))?;
                }
            }
            DA_OP_IMMFLOAT => {
                let start = 12 * usize::from(op.data);
                let length = *FLOATS.get(start).ok_or(FormatError::MalformedOperand)?;
                self.append(
                    FLOATS
                        .get(start + 1..start + 1 + usize::from(length))
                        .ok_or(FormatError::MalformedOperand)?,
                )?;
            }
            DA_OP_COND => {
                let start = 2 * usize::from(low);
                self.append(
                    b"eqnecsccmiplvsvchilsgeltgtlealnv"
                        .get(start..start + 2)
                        .ok_or(FormatError::MalformedOperand)?,
                )?;
            }
            DA_OP_PRFOP => {
                let start = 10 * usize::from(op.reg);
                let length = *PREFETCH.get(start).ok_or(FormatError::MalformedOperand)?;
                self.append(
                    PREFETCH
                        .get(start + 1..start + 1 + usize::from(length))
                        .ok_or(FormatError::MalformedOperand)?,
                )?;
            }
            _ => {}
        }
        Ok(())
    }
}

pub fn da64_format(ddi: &Instruction, out: &mut [u8]) -> Result<usize, FormatError> {
    da64_format_abs(ddi, 0, out)
}

pub fn da64_format_abs(ddi: &Instruction, addr: u64, out: &mut [u8]) -> Result<usize, FormatError> {
    let mut buffer = Buffer {
        bytes: [0; 128],
        len: 0,
    };
    let mut mnemonic_length = 0;
    if ddi.mnem != 0 {
        if let Some(&entry) = generated::DA64_DECSTRTAB.get(usize::from(ddi.mnem)) {
            let start = usize::from(entry & 0xfff);
            let length = usize::from(entry >> 12);
            mnemonic_length = length;
            buffer.append(
                generated::DA64_DECSTR
                    .as_bytes()
                    .get(start..start + length)
                    .ok_or(FormatError::MalformedOperand)?,
            )?;
        }
        for (index, op) in ddi.ops.iter().enumerate() {
            if op.kind == DA_OP_NONE {
                break;
            }
            if index != 0 && ddi.ops[0].kind != DA_OP_COND {
                buffer.append(b", ")?;
            } else if index != 0 || ddi.ops[0].kind != DA_OP_COND {
                buffer.byte(b' ')?;
            }
            buffer.operand(op, ddi.imm64, addr)?;
        }
    }
    let length = buffer.len;
    buffer.byte(0)?;
    let destination = out
        .get_mut(..buffer.len)
        .ok_or(FormatError::OutputTooSmall)?;
    if mnemonic_length > 12 {
        // The fixed twelve-byte mnemonic copy leaves this gap untouched.
        buffer.bytes[12..mnemonic_length].copy_from_slice(&destination[12..mnemonic_length]);
    }
    destination.copy_from_slice(&buffer.bytes[..buffer.len]);
    Ok(length)
}

const PREFETCH: &[u8] = concat!(
    "\x09pldl1keep\x09pldl1strm\x09pldl2keep\x09pldl2strm",
    "\x09pldl3keep\x09pldl3strm\x02#6       \x02#7       ",
    "\x09plil1keep\x09plil1strm\x09plil2keep\x09plil2strm",
    "\x09plil3keep\x09plil3strm\x03#14      \x03#15      ",
    "\x09pstl1keep\x09pstl1strm\x09pstl2keep\x09pstl2strm",
    "\x09pstl3keep\x09pstl3strm\x03#22      \x03#23      ",
    "\x03#24      \x03#25      \x03#26      \x03#27      ",
    "\x03#28      \x03#29      \x03#30      \x03#31      ",
)
.as_bytes();

const FLOATS: &[u8] = concat!(
    "\x02#2         \x06#2.125     \x05#2.25      \x06#2.375     ",
    "\x04#2.5       \x06#2.625     \x05#2.75      \x06#2.875     ",
    "\x02#3         \x06#3.125     \x05#3.25      \x06#3.375     ",
    "\x04#3.5       \x06#3.625     \x05#3.75      \x06#3.875     ",
    "\x02#4         \x05#4.25      \x04#4.5       \x05#4.75      ",
    "\x02#5         \x05#5.25      \x04#5.5       \x05#5.75      ",
    "\x02#6         \x05#6.25      \x04#6.5       \x05#6.75      ",
    "\x02#7         \x05#7.25      \x04#7.5       \x05#7.75      ",
    "\x02#8         \x04#8.5       \x02#9         \x04#9.5       ",
    "\x03#10        \x05#10.5      \x03#11        \x05#11.5      ",
    "\x03#12        \x05#12.5      \x03#13        \x05#13.5      ",
    "\x03#14        \x05#14.5      \x03#15        \x05#15.5      ",
    "\x03#16        \x03#17        \x03#18        \x03#19        ",
    "\x03#20        \x03#21        \x03#22        \x03#23        ",
    "\x03#24        \x03#25        \x03#26        \x03#27        ",
    "\x03#28        \x03#29        \x03#30        \x03#31        ",
    "\x06#0.125     \x0a#0.1328125 \x09#0.140625  \x0a#0.1484375 ",
    "\x08#0.15625   \x0a#0.1640625 \x09#0.171875  \x0a#0.1796875 ",
    "\x07#0.1875    \x0a#0.1953125 \x09#0.203125  \x0a#0.2109375 ",
    "\x08#0.21875   \x0a#0.2265625 \x09#0.234375  \x0a#0.2421875 ",
    "\x05#0.25      \x09#0.265625  \x08#0.28125   \x09#0.296875  ",
    "\x07#0.3125    \x09#0.328125  \x08#0.34375   \x09#0.359375  ",
    "\x06#0.375     \x09#0.390625  \x08#0.40625   \x09#0.421875  ",
    "\x07#0.4375    \x09#0.453125  \x08#0.46875   \x09#0.484375  ",
    "\x04#0.5       \x08#0.53125   \x07#0.5625    \x08#0.59375   ",
    "\x06#0.625     \x08#0.65625   \x07#0.6875    \x08#0.71875   ",
    "\x05#0.75      \x08#0.78125   \x07#0.8125    \x08#0.84375   ",
    "\x06#0.875     \x08#0.90625   \x07#0.9375    \x08#0.96875   ",
    "\x02#1         \x07#1.0625    \x06#1.125     \x07#1.1875    ",
    "\x05#1.25      \x07#1.3125    \x06#1.375     \x07#1.4375    ",
    "\x04#1.5       \x07#1.5625    \x06#1.625     \x07#1.6875    ",
    "\x05#1.75      \x07#1.8125    \x06#1.875     \x07#1.9375    ",
    "\x03#-2        \x07#-2.125    \x06#-2.25     \x07#-2.375    ",
    "\x05#-2.5      \x07#-2.625    \x06#-2.75     \x07#-2.875    ",
    "\x03#-3        \x07#-3.125    \x06#-3.25     \x07#-3.375    ",
    "\x05#-3.5      \x07#-3.625    \x06#-3.75     \x07#-3.875    ",
    "\x03#-4        \x06#-4.25     \x05#-4.5      \x06#-4.75     ",
    "\x03#-5        \x06#-5.25     \x05#-5.5      \x06#-5.75     ",
    "\x03#-6        \x06#-6.25     \x05#-6.5      \x06#-6.75     ",
    "\x03#-7        \x06#-7.25     \x05#-7.5      \x06#-7.75     ",
    "\x03#-8        \x05#-8.5      \x03#-9        \x05#-9.5      ",
    "\x04#-10       \x06#-10.5     \x04#-11       \x06#-11.5     ",
    "\x04#-12       \x06#-12.5     \x04#-13       \x06#-13.5     ",
    "\x04#-14       \x06#-14.5     \x04#-15       \x06#-15.5     ",
    "\x04#-16       \x04#-17       \x04#-18       \x04#-19       ",
    "\x04#-20       \x04#-21       \x04#-22       \x04#-23       ",
    "\x04#-24       \x04#-25       \x04#-26       \x04#-27       ",
    "\x04#-28       \x04#-29       \x04#-30       \x04#-31       ",
    "\x07#-0.125    \x0b#-0.1328125\x0a#-0.140625 \x0b#-0.1484375",
    "\x09#-0.15625  \x0b#-0.1640625\x0a#-0.171875 \x0b#-0.1796875",
    "\x08#-0.1875   \x0b#-0.1953125\x0a#-0.203125 \x0b#-0.2109375",
    "\x09#-0.21875  \x0b#-0.2265625\x0a#-0.234375 \x0b#-0.2421875",
    "\x06#-0.25     \x0a#-0.265625 \x09#-0.28125  \x0a#-0.296875 ",
    "\x08#-0.3125   \x0a#-0.328125 \x09#-0.34375  \x0a#-0.359375 ",
    "\x07#-0.375    \x0a#-0.390625 \x09#-0.40625  \x0a#-0.421875 ",
    "\x08#-0.4375   \x0a#-0.453125 \x09#-0.46875  \x0a#-0.484375 ",
    "\x05#-0.5      \x09#-0.53125  \x08#-0.5625   \x09#-0.59375  ",
    "\x07#-0.625    \x09#-0.65625  \x08#-0.6875   \x09#-0.71875  ",
    "\x06#-0.75     \x09#-0.78125  \x08#-0.8125   \x09#-0.84375  ",
    "\x07#-0.875    \x09#-0.90625  \x08#-0.9375   \x09#-0.96875  ",
    "\x03#-1        \x08#-1.0625   \x07#-1.125    \x08#-1.1875   ",
    "\x06#-1.25     \x08#-1.3125   \x07#-1.375    \x08#-1.4375   ",
    "\x05#-1.5      \x08#-1.5625   \x07#-1.625    \x08#-1.6875   ",
    "\x06#-1.75     \x08#-1.8125   \x07#-1.875    \x08#-1.9375   ",
    "\x04#0.0       ",
)
.as_bytes();

#[cfg(test)]
mod tests {
    use super::*;

    fn instruction(ops: &[Operand]) -> Instruction {
        let mut result = Instruction {
            mnem: u16::MAX,
            ops: [Operand::NONE; 5],
            imm64: u64::MAX,
        };
        result.ops[..ops.len()].copy_from_slice(ops);
        result
    }

    fn operand(kind: u8, reg: u8, data: u16) -> Operand {
        Operand { kind, reg, data }
    }

    fn check(inst: &Instruction, address: u64, expected: &[u8]) {
        let mut guarded = [0xa5; 132];
        assert_eq!(
            da64_format_abs(inst, address, &mut guarded[2..130]),
            Ok(expected.len() - 1)
        );
        assert_eq!(&guarded[2..2 + expected.len()], expected);
        assert_eq!(&guarded[..2], &[0xa5; 2]);
        assert!(guarded[2 + expected.len()..]
            .iter()
            .all(|&byte| byte == 0xa5));
    }

    #[test]
    fn every_operand_kind_preserves_its_text_shape() {
        let cases: &[(u8, u8, u16, &[u8])] = &[
            (DA_OP_NONE, 255, 65535, b"\0"),
            (DA_OP_REGGP, 31, 1, b" xzr\0"),
            (DA_OP_REGGPINC, 9, 0, b" w9!\0"),
            (DA_OP_REGGPEXT, 10, 0x4005, b" x10, uxtw #64\0"),
            (DA_OP_REGSP, 255, 1, b" sp\0"),
            (DA_OP_REGFP, 31, 4, b" q31\0"),
            (DA_OP_REGVEC, 10, 1, b" v10.16b\0"),
            (DA_OP_REGVTBL, 31, 0x401, b" {v31.16b-v2.16b}\0"),
            (DA_OP_REGVIDX, 4, 0xf07, b" v4.2h[15]\0"),
            (DA_OP_REGVTBLIDX, 31, 0x4f3, b" {v31.d-v2.d }[15]\0"),
            (DA_OP_MEMUOFF, 31, 65535, b" [sp, #0xffff]\0"),
            (DA_OP_MEMSOFF, 9, 0x8000, b" [x9, #-0x8000]\0"),
            (DA_OP_MEMSOFFPRE, 31, 0, b" [sp, #0x0]!\0"),
            (DA_OP_MEMSOFFPOST, 10, 65535, b" [x10], #-0x1\0"),
            (DA_OP_MEMREG, 31, 0x1f06, b" [sp, xzr]\0"),
            (DA_OP_MEMREGPOST, 31, 0x1f00, b" [sp], xzr\0"),
            (DA_OP_MEMINC, 31, 0, b" [xzr]!\0"),
            (DA_OP_COND, 255, 15, b"nv\0"),
            (DA_OP_PRFOP, 7, 65535, b" #7\0"),
            (DA_OP_SYSREG, 255, 65535, b" \0"),
            (DA_OP_IMMSMALL, 255, 64, b" #64\0"),
            (DA_OP_SIMM, 255, 0x8000, b" #-0x8000\0"),
            (DA_OP_UIMM, 255, 65535, b" #0xffff\0"),
            (DA_OP_UIMMSHIFT, 129, 65535, b" #0xffff, msl #64\0"),
            (DA_OP_IMMLARGE, 255, 65535, b" #0xffffffffffffffff\0"),
            (DA_OP_RELADDR, 255, 65535, b" #0x1\0"),
            (DA_OP_IMMFLOAT, 255, 256, b" #0.0\0"),
        ];
        for &(kind, reg, data, expected) in cases {
            check(&instruction(&[operand(kind, reg, data)]), 2, expected);
        }
    }

    #[test]
    fn condition_separators_empty_operands_and_lsl_nuls_are_preserved() {
        check(
            &instruction(&[
                operand(DA_OP_COND, 0, 2),
                operand(DA_OP_REGGP, 31, 1),
                operand(DA_OP_REGSP, 0, 0),
            ]),
            0,
            b"cs xzr wsp\0",
        );
        check(
            &instruction(&[
                operand(DA_OP_SYSREG, 0, 0),
                operand(255, 255, 65535),
                operand(DA_OP_REGGP, 0, 0),
            ]),
            0,
            b" , , w0\0",
        );
        check(
            &instruction(&[operand(DA_OP_MEMREG, 0, 0x167), operand(DA_OP_UIMM, 0, 42)]),
            0,
            b" [x0, x1, lsl #3]\0, #0x2a\0",
        );
        let mut unknown = instruction(&[operand(DA_OP_REGFP, 255, 65535)]);
        unknown.mnem = 0;
        check(&unknown, u64::MAX, b"\0");
        let mut buffer = [0xa5; 1];
        assert_eq!(
            da64_format(&unknown, &mut buffer[..0]),
            Err(FormatError::OutputTooSmall)
        );
        assert_eq!(buffer, [0xa5]);
        assert_eq!(da64_format(&unknown, &mut buffer), Ok(0));
        assert_eq!(buffer, [0]);
    }

    #[test]
    fn capacity_is_atomic_and_reusable_at_every_cut() {
        let inst = instruction(&[
            operand(DA_OP_REGVTBLIDX, 31, 0x4f3),
            operand(DA_OP_MEMREG, 31, 0x1f8f),
            operand(DA_OP_IMMLARGE, 0, 0),
        ]);
        let expected = b" {v31.d-v2.d }[15], [sp, xzr, sxtx #4], #0xffffffffffffffff\0";
        for capacity in 0..expected.len() {
            let mut output = [0x5a; 132];
            assert_eq!(
                da64_format(&inst, &mut output[2..2 + capacity]),
                Err(FormatError::OutputTooSmall)
            );
            assert_eq!(output, [0x5a; 132]);
            assert_eq!(
                da64_format(&inst, &mut output[2..2 + expected.len()]),
                Ok(expected.len() - 1)
            );
            assert_eq!(&output[2..2 + expected.len()], expected);
            assert_eq!(&output[..2], &[0x5a; 2]);
            assert!(output[2 + expected.len()..]
                .iter()
                .all(|&byte| byte == 0x5a));
        }
    }

    #[test]
    fn malformed_raw_fields_refuse_without_modifying_output() {
        let bad = [
            operand(DA_OP_REGGP, 32, 0),
            operand(DA_OP_REGGPINC, 255, 0),
            operand(DA_OP_REGGPEXT, 0, 24),
            operand(DA_OP_REGGPEXT, 0, 65 << 8),
            operand(DA_OP_REGFP, 0, 5),
            operand(DA_OP_REGFP, 32, 0),
            operand(DA_OP_REGVEC, 0, 10),
            operand(DA_OP_REGVIDX, 0, 8),
            operand(DA_OP_REGVIDX, 0, 65 << 8),
            operand(DA_OP_REGVTBL, 0, 10),
            operand(DA_OP_REGVTBLIDX, 0, 4),
            operand(DA_OP_MEMUOFF, 32, 0),
            operand(DA_OP_MEMREG, 0, 0),
            operand(DA_OP_MEMREG, 0, 18),
            operand(DA_OP_MEMREG, 0, 0xa5),
            operand(DA_OP_MEMREG, 0, 0x2004),
            operand(DA_OP_MEMREGPOST, 0, 0x2000),
            operand(DA_OP_MEMINC, 32, 0),
            operand(DA_OP_IMMSMALL, 0, 65),
            operand(DA_OP_UIMMSHIFT, 130, 0),
            operand(DA_OP_IMMFLOAT, 0, 257),
            operand(DA_OP_COND, 0, 16),
            operand(DA_OP_PRFOP, 32, 0),
        ];
        for op in bad {
            let inst = instruction(&[operand(DA_OP_REGGP, 9, 1), op]);
            let mut output = [0x5a; 132];
            assert_eq!(
                da64_format(&inst, &mut output[2..130]),
                Err(FormatError::MalformedOperand)
            );
            assert_eq!(output, [0x5a; 132]);
            let good = instruction(&[operand(DA_OP_MEMINC, 31, 0)]);
            assert_eq!(da64_format(&good, &mut output[2..130]), Ok(7));
            assert_eq!(&output[2..10], b" [xzr]!\0");
        }
    }

    #[test]
    fn float_boundaries_aliases_and_safe_extensions_are_stable() {
        for (value, expected) in [
            (0, &b" #2\0"[..]),
            (1, b" #2.125\0"),
            (63, b" #31\0"),
            (64, b" #0.125\0"),
            (65, b" #0.1328125\0"),
            (127, b" #1.9375\0"),
            (128, b" #-2\0"),
            (193, b" #-0.1328125\0"),
            (255, b" #-1.9375\0"),
            (256, b" #0.0\0"),
        ] {
            check(
                &instruction(&[operand(DA_OP_IMMFLOAT, 255, value)]),
                0,
                expected,
            );
        }
        check(
            &instruction(&[operand(DA_OP_MEMREG, 0, 0x102)]),
            0,
            b" [x0, w1, uxtw]\0",
        );
        check(
            &instruction(&[operand(DA_OP_MEMREG, 0, 0x113)]),
            0,
            b" [x0, w1, uxtw #1]\0",
        );
        check(
            &instruction(&[operand(DA_OP_REGVTBL, 31, 1)]),
            0,
            b" {v31.16b}\0",
        );
        check(
            &instruction(&[operand(DA_OP_REGVIDX, 0, 0x4000)]),
            0,
            b" v0.b[64]\0",
        );
    }

    #[test]
    fn real_mnemonic_and_absolute_wrapping_use_decoded_state() {
        let mut decoded = crate::decode(0x1400_0001);
        check(&decoded, 0, b"b #0x4\0");
        check(&decoded, u64::MAX - 2, b"b #0x1\0");
        let mut out = [0xa5; 128];
        assert_eq!(da64_format(&decoded, &mut out), Ok(6));
        assert_eq!(&out[..7], b"b #0x4\0");
        decoded.imm64 = 0;
        check(&decoded, 0, b"b #0x0\0");
    }
}
