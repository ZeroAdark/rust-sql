// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use crate::{common, decoder_runtime::append_format};
use std::fs;
use tpde_disarm::*;

#[derive(Default)]
struct Corpus {
    input: Vec<u8>,
    output: Vec<u8>,
    kinds: [usize; 27],
    cases: usize,
}

impl Corpus {
    fn add(&mut self, inst: Instruction, address: u64) {
        self.input.extend_from_slice(&inst.mnem.to_le_bytes());
        for op in inst.ops {
            self.input.push(op.kind);
            self.input.push(op.reg);
            self.input.extend_from_slice(&op.data.to_le_bytes());
        }
        self.input.extend_from_slice(&inst.imm64.to_le_bytes());
        self.input.extend_from_slice(&address.to_le_bytes());
        append_format(&mut self.output, &inst, Some(address));
        append_format(&mut self.output, &inst, None);
        self.cases += 1;
    }

    fn operand(&mut self, kind: u8, reg: u8, data: u16) {
        self.kinds[usize::from(kind)] += 1;
        self.add(
            Instruction {
                mnem: DA64I_UDF as u16,
                ops: [
                    Operand { kind, reg, data },
                    Operand::NONE,
                    Operand::NONE,
                    Operand::NONE,
                    Operand::NONE,
                ],
                imm64: 0xfedc_ba98_7654_3210,
            },
            0xffff_ffff_0000_0000,
        );
    }
}

#[test]
fn every_formatter_operand_kind_and_presentation_boundary_matches_c() {
    let dir = common::prepare("manual-format");
    let references = common::compile_all(&dir, "format_reference.cpp");
    let mut corpus = Corpus::default();
    corpus.operand(DA_OP_NONE, 0, 0);
    for kind in [DA_OP_REGGP, DA_OP_REGGPINC, DA_OP_REGSP] {
        for reg in 0..32 {
            for sf in 0..2 {
                corpus.operand(kind, reg, sf);
            }
        }
    }
    for reg in [0, 9, 10, 30, 31] {
        for sf in 0..2 {
            for ext in 0..12 {
                for shift in [0, 1, 9, 10, 31, 32, 63, 64] {
                    corpus.operand(DA_OP_REGGPEXT, reg, sf | (ext << 1) | (shift << 8));
                }
            }
        }
        for size in 0..5 {
            corpus.operand(DA_OP_REGFP, reg, size);
        }
        for va in 0..10 {
            corpus.operand(DA_OP_REGVEC, reg, va);
            for count in 1..=4 {
                corpus.operand(DA_OP_REGVTBL, reg, va | (count << 8));
            }
        }
        for size in 0..8 {
            for elem in [0, 1, 9, 10, 15, 31, 63] {
                corpus.operand(DA_OP_REGVIDX, reg, size | (elem << 8));
            }
        }
        for size in 0..4 {
            for elem in [0, 1, 9, 10, 15] {
                for count in 1..=4 {
                    corpus.operand(DA_OP_REGVTBLIDX, reg, size | (elem << 4) | (count << 8));
                }
            }
        }
        for kind in [
            DA_OP_MEMUOFF,
            DA_OP_MEMSOFF,
            DA_OP_MEMSOFFPRE,
            DA_OP_MEMSOFFPOST,
        ] {
            for immediate in [0, 1, 15, 16, 255, 256, 0x7fff, 0x8000, 0xffff] {
                corpus.operand(kind, reg, immediate);
            }
        }
        for offreg in [0, 9, 10, 30, 31] {
            for ext in [2, 3, 6, 7] {
                for scale in 0..2 {
                    for shift in 0..5 {
                        corpus.operand(
                            DA_OP_MEMREG,
                            reg,
                            scale | (ext << 1) | (shift << 5) | (offreg << 8),
                        );
                    }
                }
            }
            corpus.operand(DA_OP_MEMREGPOST, reg, offreg << 8);
        }
        corpus.operand(DA_OP_MEMINC, reg, 0);
    }
    for condition in 0..16 {
        corpus.operand(DA_OP_COND, 0, condition);
    }
    for operation in 0..32 {
        corpus.operand(DA_OP_PRFOP, operation, 0);
    }
    for sysreg in 0..=u16::MAX {
        corpus.operand(DA_OP_SYSREG, 0, sysreg);
    }
    for immediate in 0..=64 {
        corpus.operand(DA_OP_IMMSMALL, 0, immediate);
    }
    for kind in [DA_OP_SIMM, DA_OP_UIMM] {
        for immediate in [0, 1, 9, 10, 15, 16, 255, 256, 0x7fff, 0x8000, 0xffff] {
            corpus.operand(kind, 0, immediate);
        }
    }
    for mask in 0..2 {
        for shift in 0..=64 {
            for immediate in [0, 1, 0x7fff, 0xffff] {
                corpus.operand(DA_OP_UIMMSHIFT, mask | (shift << 1), immediate);
            }
        }
    }
    for kind in [DA_OP_IMMLARGE, DA_OP_RELADDR] {
        corpus.operand(kind, 0, 0);
        for immediate in [0, 1, 15, 16, u64::MAX, 1 << 63] {
            for address in [0, 1, 0x1000, u64::MAX] {
                corpus.add(
                    Instruction {
                        mnem: DA64I_UDF as u16,
                        ops: [
                            Operand {
                                kind,
                                reg: 0,
                                data: 0,
                            },
                            Operand::NONE,
                            Operand::NONE,
                            Operand::NONE,
                            Operand::NONE,
                        ],
                        imm64: immediate,
                    },
                    address,
                );
            }
        }
    }
    for immediate in 0..=256 {
        corpus.operand(DA_OP_IMMFLOAT, 0, immediate);
    }
    for mnem in [0, DA64I_UDF as u16, u16::MAX] {
        for first_condition in [false, true] {
            let gp = Operand {
                kind: DA_OP_REGGP,
                reg: 31,
                data: 1,
            };
            let condition = Operand {
                kind: DA_OP_COND,
                reg: 0,
                data: 0,
            };
            let ops = if first_condition {
                [condition, gp, gp, gp, gp]
            } else {
                [gp, condition, gp, gp, gp]
            };
            corpus.add(
                Instruction {
                    mnem,
                    ops,
                    imm64: u64::MAX,
                },
                0,
            );
            corpus.add(
                Instruction {
                    mnem,
                    ops: [gp, Operand::NONE, condition, gp, gp],
                    imm64: 0,
                },
                0,
            );
        }
    }
    assert!(corpus.kinds.iter().all(|&count| count > 0));
    assert!(corpus.cases >= 69_000);
    assert_eq!(corpus.input.len(), corpus.cases * 38);
    fs::write(dir.join("format-input.bin"), &corpus.input).unwrap();
    fs::write(dir.join("format-rust.bin"), &corpus.output).unwrap();
    fs::write(
        dir.join("format-operand-kinds.tsv"),
        corpus
            .kinds
            .iter()
            .enumerate()
            .map(|(kind, count)| format!("{kind}\t{count}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let modes = common::compare_all(
        &dir,
        &references,
        &corpus.input,
        &corpus.output,
        "all handwritten formatter operand and punctuation branches",
    );
    let mut extension = [0; 128];
    let inst = Instruction {
        mnem: DA64I_UDF as u16,
        ops: [
            Operand {
                kind: DA_OP_REGVIDX,
                reg: 31,
                data: 64 << 8,
            },
            Operand::NONE,
            Operand::NONE,
            Operand::NONE,
            Operand::NONE,
        ],
        imm64: 0,
    };
    let written = da64_format(&inst, &mut extension).unwrap();
    assert_eq!(&extension[..written + 1], b"udf v31.b[64]\0");
    println!(
        "TPDE_DISARM_RUNTIME_FORMAT_OPERANDS_OK profile={} cases={} kinds=27 cpp_modes={modes}",
        tpde_disarm::GENERATION_PROFILE,
        corpus.cases
    );
}
