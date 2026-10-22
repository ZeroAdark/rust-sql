// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use std::vec::Vec;

pub fn text(output: &str) -> Vec<u8> {
    let line = output
        .lines()
        .find_map(|line| line.strip_prefix("text "))
        .expect("text record");
    assert_eq!(line.len() % 2, 0);
    line.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let digit = |c: u8| match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                _ => panic!("invalid hex"),
            };
            digit(pair[0]) * 16 + digit(pair[1])
        })
        .collect()
}

fn immediate(code: &[u8], pc: &mut usize, bytes: usize) -> u64 {
    let mut out = 0;
    for index in 0..bytes {
        out |= (code[*pc + index] as u64) << (index * 8);
    }
    *pc += bytes;
    out
}
fn signed(code: &[u8], pc: &mut usize, bytes: usize) -> u64 {
    let value = immediate(code, pc, bytes);
    match bytes {
        1 => value as i8 as i64 as u64,
        2 => value as i16 as i64 as u64,
        4 => value as i32 as i64 as u64,
        8 => value,
        _ => panic!("invalid signed width"),
    }
}
fn operand(code: &[u8], pc: &mut usize, rex: u8, regs: &[u64; 16]) -> (usize, usize, u64, bool) {
    let modrm = immediate(code, pc, 1) as u8;
    let mode = modrm >> 6;
    let reg = usize::from((modrm >> 3) & 7) + usize::from(rex & 4 != 0) * 8;
    let rm = usize::from(modrm & 7) + usize::from(rex & 1 != 0) * 8;
    if mode == 3 {
        return (reg, rm, regs[rm], true);
    }
    let mut address = 0u64;
    if modrm & 7 == 4 {
        let sib = immediate(code, pc, 1) as u8;
        let base = usize::from(sib & 7) + usize::from(rex & 1 != 0) * 8;
        let index = usize::from((sib >> 3) & 7) + usize::from(rex & 2 != 0) * 8;
        if (sib >> 3) & 7 != 4 || rex & 2 != 0 {
            address = regs[index].wrapping_mul(1u64 << (sib >> 6));
        }
        if mode != 0 || sib & 7 != 5 {
            address = address.wrapping_add(regs[base]);
        } else {
            address = address.wrapping_add(signed(code, pc, 4));
        }
    } else if mode == 0 && modrm & 7 == 5 {
        panic!("RIP-relative memory not admitted by correction evaluator");
    } else {
        address = regs[rm];
    }
    if mode == 1 {
        address = address.wrapping_add(signed(code, pc, 1));
    }
    if mode == 2 {
        address = address.wrapping_add(signed(code, pc, 4));
    }
    (reg, rm, address, false)
}
pub fn evaluate(code: &[u8], mut regs: [u64; 16]) -> [u64; 16] {
    let mut pc = 0;
    while pc < code.len() {
        let mut rex = 0;
        let mut width = 32;
        if (0x40..=0x4f).contains(&code[pc]) {
            rex = code[pc];
            pc += 1;
            if rex & 8 != 0 {
                width = 64;
            }
        }
        let opcode = immediate(code, &mut pc, 1) as u8;
        let mask = if width == 32 {
            u32::MAX as u64
        } else {
            u64::MAX
        };
        match opcode {
            0xc3 => {
                assert_eq!(pc, code.len());
                return regs;
            }
            0x90 => {}
            0xb8..=0xbf => {
                let reg = usize::from(opcode - 0xb8) + usize::from(rex & 1 != 0) * 8;
                regs[reg] = immediate(code, &mut pc, if width == 64 { 8 } else { 4 });
            }
            0x8d => {
                let (reg, _, address, direct) = operand(code, &mut pc, rex, &regs);
                assert!(!direct);
                regs[reg] = address & mask;
            }
            0x89 | 0x8b | 0x21 | 0x23 | 0x09 | 0x0b | 0x01 | 0x03 | 0x31 | 0x33 => {
                let (reg, rm, value, direct) = operand(code, &mut pc, rex, &regs);
                assert!(direct);
                let (dst, src) = if opcode & 2 == 0 {
                    (rm, regs[reg])
                } else {
                    (reg, value)
                };
                regs[dst] = match opcode {
                    0x89 | 0x8b => src,
                    0x21 | 0x23 => regs[dst] & src,
                    0x09 | 0x0b => regs[dst] | src,
                    0x01 | 0x03 => regs[dst].wrapping_add(src),
                    0x31 | 0x33 => regs[dst] ^ src,
                    _ => unreachable!(),
                } & mask;
            }
            0xc7 | 0x81 | 0x83 => {
                let (group, rm, _, direct) = operand(code, &mut pc, rex, &regs);
                assert!(direct);
                let value = signed(code, &mut pc, if opcode == 0x83 { 1 } else { 4 });
                regs[rm] = if opcode == 0xc7 {
                    assert_eq!(group & 7, 0);
                    value
                } else {
                    match group & 7 {
                        0 => regs[rm].wrapping_add(value),
                        1 => regs[rm] | value,
                        4 => regs[rm] & value,
                        5 => regs[rm].wrapping_sub(value),
                        6 => regs[rm] ^ value,
                        _ => panic!("unsupported immediate operation"),
                    }
                } & mask;
            }
            0x05 | 0x25 => {
                let value = signed(code, &mut pc, 4);
                regs[0] = (if opcode == 0x05 {
                    regs[0].wrapping_add(value)
                } else {
                    regs[0] & value
                }) & mask;
            }
            0x69 | 0x6b => {
                let (reg, _, value, direct) = operand(code, &mut pc, rex, &regs);
                assert!(direct);
                let factor = signed(code, &mut pc, if opcode == 0x6b { 1 } else { 4 });
                regs[reg] = value.wrapping_mul(factor) & mask;
            }
            0xc1 | 0xd1 => {
                let (group, rm, _, direct) = operand(code, &mut pc, rex, &regs);
                assert!(direct);
                assert_eq!(group & 7, 4);
                let count = if opcode == 0xd1 {
                    1
                } else {
                    immediate(code, &mut pc, 1) as u32
                };
                regs[rm] = regs[rm].wrapping_shl(count & (width - 1)) & mask;
            }
            0x0f => {
                let second = immediate(code, &mut pc, 1) as u8;
                let (reg, rm, value, direct) = operand(code, &mut pc, rex, &regs);
                assert!(direct);
                match second {
                    0xb6 => regs[reg] = value & 255,
                    0xb7 => regs[reg] = value & 65535,
                    0xba => {
                        assert_eq!(reg & 7, 6);
                        let bit = immediate(code, &mut pc, 1) as u32;
                        regs[rm] &= !(1u64 << (bit & (width - 1)));
                    }
                    _ => panic!("unsupported two-byte opcode {second:02x}"),
                }
            }
            _ => panic!("unsupported correction opcode {opcode:02x} at {}", pc - 1),
        }
    }
    panic!("missing final RET")
}

pub struct Correction {
    pub name: &'static str,
    pub input: &'static str,
    pub register: usize,
    pub expected: fn(&[u64; 16]) -> u64,
}
pub fn corrections() -> [Correction; 6] {
    [
        Correction {
            name: "no_base_scaled_disp",
            input: "init 0 0 0 1\nhold 7\nexpr 0 255 1 7 2 9\nrelease 7\nepilog\n",
            register: 0,
            expected: |r| r[7].wrapping_mul(2).wrapping_add(9),
        },
        Correction {
            name: "pure_displacement_zero",
            input: "init 0 0 0 1\nexpr 0 255 0 255 0 0\nepilog\n",
            register: 0,
            expected: |_| 0,
        },
        Correction {
            name: "pure_displacement_nonzero",
            input: "init 0 0 0 1\nexpr 0 255 0 255 0 12345678901\nepilog\n",
            register: 0,
            expected: |_| 12345678901,
        },
        Correction {
            name: "folded_zero_scale",
            input: "init 0 0 0 1\nhold 7\nexpr 1 7 1 7 18446744073709551615 0\nrelease 7\nepilog\n",
            register: 0,
            expected: |_| 0,
        },
        Correction {
            name: "overlapping_bitfield",
            input: "init 0 0 0 1\nhold 10\nbfi 10 10 4 8\nrelease 10\nepilog\n",
            register: 10,
            expected: |r| (r[10] & !(255 << 4)) | ((r[10] & 255) << 4),
        },
        Correction {
            name: "minimum_signed_scale",
            input:
                "init 0 0 0 1\nhold 7\nexpr 0 255 1 7 9223372036854775808 0\nrelease 7\nepilog\n",
            register: 0,
            expected: |r| r[7].wrapping_mul(1 << 63),
        },
    ]
}
pub fn initial(seed: u64) -> [u64; 16] {
    core::array::from_fn(|index| {
        seed.wrapping_mul(0x123456789ab)
            .wrapping_add(index as u64 * 31337 + 991)
    })
}
