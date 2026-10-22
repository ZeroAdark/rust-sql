// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{TestIR, ValueType};
use core::fmt::{self, Write};

impl TestIR<'_> {
    pub fn print(&self, out: &mut impl Write) -> fmt::Result {
        writeln!(out, "Printing IR")?;
        for function in &*self.functions {
            let name = self.name(function.name);
            if function.declaration {
                writeln!(out, "Extern function {}", name)?;
            } else if function.local_only {
                writeln!(out, "Local function {}", name)?;
            } else {
                writeln!(out, "Function {}", name)?;
            }
            for arg in function.arg_begin_idx..function.arg_end_idx {
                let value = self.values[arg as usize];
                assert_eq!(value.value_type, ValueType::Arg);
                writeln!(out, "  Argument {}", self.name(value.name))?;
            }
            if function.declaration {
                continue;
            }
            for index in function.block_begin_idx..function.block_end_idx {
                let block = &self.blocks[index as usize];
                writeln!(out, "  Block {}", self.name(block.name))?;
                for succ in block.succ_begin_idx..block.succ_end_idx {
                    writeln!(
                        out,
                        "    Succ {}",
                        self.name(self.blocks[self.value_operands[succ as usize] as usize].name)
                    )?;
                }
                for index in block.inst_begin_idx..block.inst_end_idx {
                    let value = self.values[index as usize];
                    match value.value_type {
                        ValueType::Normal | ValueType::Terminator => {
                            let info = value.op.info();
                            writeln!(out, "    Value {} ({})", self.name(value.name), info.name)?;
                            if value.op == crate::Op::Call {
                                writeln!(
                                    out,
                                    "      Target {}",
                                    self.name(self.functions[value.call_func_idx as usize].name)
                                )?;
                            }
                            for op in value.op_begin_idx..value.op_end_idx {
                                let operand = self.value_operands[op as usize];
                                if op - value.op_begin_idx < value.op_count {
                                    writeln!(
                                        out,
                                        "      Op {}",
                                        self.name(self.values[operand as usize].name)
                                    )?;
                                } else if value.op_end_idx - op > info.imm_count {
                                    writeln!(
                                        out,
                                        "      Op ^{}",
                                        self.name(self.blocks[operand as usize].name)
                                    )?;
                                } else {
                                    writeln!(out, "      Op ${}", operand)?;
                                }
                            }
                        }
                        ValueType::Arg => panic!("argument must not be in instruction list"),
                        ValueType::Phi => {
                            writeln!(out, "    PHI {}", self.name(value.name))?;
                            for op in 0..value.op_count {
                                let block = self.value_operands
                                    [(value.op_begin_idx + value.op_count + op) as usize];
                                let incoming =
                                    self.value_operands[(value.op_begin_idx + op) as usize];
                                writeln!(
                                    out,
                                    "      {} from {}",
                                    self.name(self.values[incoming as usize].name),
                                    self.name(self.blocks[block as usize].name)
                                )?;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn dump_debug(&self, out: &mut impl Write, trace: bool) -> fmt::Result {
        writeln!(out, "Dumping IR")?;
        for (idx, function) in self.functions.iter().enumerate() {
            let name = self.name(function.name);
            if function.declaration {
                writeln!(out, "Extern function {}: {}", idx, name)?;
            } else if function.local_only {
                writeln!(out, "Local function {}: {}", idx, name)?;
            } else {
                writeln!(out, "Function {}: {}", idx, name)?;
            }
            if trace {
                writeln!(
                    out,
                    "  Arg {}->{}",
                    function.arg_begin_idx, function.arg_end_idx
                )?;
            }
            for arg in function.arg_begin_idx..function.arg_end_idx {
                let value = self.values[arg as usize];
                assert_eq!(value.value_type, ValueType::Arg);
                writeln!(out, "  Arg {}: {}", arg, self.name(value.name))?;
            }
            if function.declaration {
                continue;
            }
            if trace {
                writeln!(
                    out,
                    "  Block {}->{}",
                    function.block_begin_idx, function.block_end_idx
                )?;
            }
            for index in function.block_begin_idx..function.block_end_idx {
                let block = &self.blocks[index as usize];
                writeln!(out, "  Block {}: {}", index, self.name(block.name))?;
                if trace {
                    writeln!(
                        out,
                        "    Succ {}->{}",
                        block.succ_begin_idx, block.succ_end_idx
                    )?;
                }
                for succ in block.succ_begin_idx..block.succ_end_idx {
                    let index = self.value_operands[succ as usize];
                    writeln!(
                        out,
                        "    Succ {}: {}",
                        index,
                        self.name(self.blocks[index as usize].name)
                    )?;
                }
                if trace {
                    writeln!(
                        out,
                        "    Inst {}->{}",
                        block.inst_begin_idx, block.inst_end_idx
                    )?;
                }
                for index in block.inst_begin_idx..block.inst_end_idx {
                    let value = self.values[index as usize];
                    match value.value_type {
                        ValueType::Normal | ValueType::Terminator => {
                            writeln!(
                                out,
                                "    Value {} ({}): {}",
                                index,
                                value.op.info().name,
                                self.name(value.name)
                            )?;
                            for op in value.op_begin_idx..value.op_end_idx {
                                let operand = self.value_operands[op as usize];
                                if op - value.op_begin_idx < value.op_count {
                                    writeln!(
                                        out,
                                        "      Op {}: {}",
                                        operand,
                                        self.name(self.values[operand as usize].name)
                                    )?;
                                } else {
                                    writeln!(out, "      Op ${}", operand)?;
                                }
                            }
                        }
                        ValueType::Arg => panic!("argument must not be in instruction list"),
                        ValueType::Phi => {
                            writeln!(out, "    PHI {}", index)?;
                            for op in 0..value.op_count {
                                let block = self.value_operands
                                    [(value.op_begin_idx + value.op_count + op) as usize];
                                let incoming =
                                    self.value_operands[(value.op_begin_idx + op) as usize];
                                writeln!(
                                    out,
                                    "      {} from {}: {} from {}",
                                    incoming,
                                    block,
                                    self.name(self.values[incoming as usize].name),
                                    self.name(self.blocks[block as usize].name)
                                )?;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
