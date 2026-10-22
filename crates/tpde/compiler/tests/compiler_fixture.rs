// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#[path = "compiler_fixture/adaptor.rs"]
mod adaptor;
#[path = "compiler_fixture/guards.rs"]
mod guards;

use adaptor::{cc_info, Adaptor, Policy};
use mcx::{Mcx, MemoryContext};
use std::{cell::RefCell, fmt::Write, marker::PhantomData};
use tpde_assembler::{Architecture, AssemblerElf, SectionKind, SymRef};
use tpde_base::ValLocalIdx;
use tpde_compiler::{
    branches::{self, BranchMachine},
    calls::{
        CallArg, CallArgFlag, CallBuilderBase, CallBuilderHooks, CallMachine, CallTarget,
        RetBuilder,
    },
    compiler_base::with_compiler_base,
    frame_guard::FrameState,
    generic_value_part::GenericValuePart,
    guard_machine::GuardMachine,
    scratch_reg::ScratchReg,
    target::{CompilerOutput, CompilerTarget, TargetMachine},
    value_part_ref::ValuePartRef,
};
use tpde_core::{
    calling_convention::{CCAssigner, CCAssignment, CCInfo, CallingConventionAssigner},
    compiler::{ValRefSpecial, ValueParts},
    ir_adaptor::IrAdaptor,
    Reg, RegBank,
};
use tpde_function_writer::{
    FunctionWriter, FunctionWriterBase, FunctionWriterJumpTables, FunctionWriterTarget, Label,
    LabelFixupKind, ModuleWriter, TargetCieInfo,
};
use tpde_test_ir::{IRBlockRef, IRFuncRef, IRInstRef, IRValueRef, Op, TestIR};
use types_error::{PgError, PgResult};

fn record(log: &RefCell<String>, tag: &str, values: &[u64]) {
    let mut output = log.borrow_mut();
    output.push_str(tag);
    for value in values {
        write!(output, " {value}").unwrap();
    }
    output.push('\n');
}

struct Writer<'log> {
    log: &'log RefCell<String>,
}
impl FunctionWriterTarget for Writer<'_> {
    const CIE: TargetCieInfo<'static> = TargetCieInfo {
        instrs: &[0x0c, 31, 0],
        return_addr_register: 30,
        code_alignment_factor: 1,
        data_alignment_factor: 0x78,
    };
    fn handle_fixups(&mut self, base: &mut FunctionWriterBase<'_, '_, '_>) -> PgResult<()> {
        record(
            self.log,
            "writer.finish",
            &[
                base.offset() as u64,
                base.jump_table_count() as u64,
                base.label_count() as u64,
            ],
        );
        for index in 0..base.label_count() {
            let label = Label(index as u32);
            let offset = if base.label_is_pending(label)? {
                u32::MAX
            } else {
                base.label_offset(label)? + base.label_skew()
            };
            record(self.log, "writer.label", &[index as u64, u64::from(offset)]);
        }
        for index in 0..base.jump_table_count() {
            let table = base.jump_table_header(index)?;
            record(
                self.log,
                "table",
                &[
                    u64::from(table.size()),
                    u64::from(table.idx.id()),
                    u64::from(table.tmp.id()),
                ],
            );
            for label in base.jump_table_labels(index)? {
                record(self.log, "table.label", &[u64::from(label.0)]);
            }
        }
        Ok(())
    }
}
impl FunctionWriterJumpTables for Writer<'_> {
    fn create_jump_table(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        size: u32,
        index: Reg,
        temporary: Reg,
        _index_is_32: bool,
        fill: impl FnOnce(&mut [Label]),
    ) -> PgResult<()> {
        let mut table = base.alloc_jump_table(size, index, temporary)?;
        fill(table.labels_mut());
        Ok(())
    }
}

struct Parts {
    count: u32,
    bank: RegBank,
}
impl ValueParts for Parts {
    fn count(&self) -> u32 {
        self.count
    }
    fn size_bytes(&self, _: u32) -> u32 {
        8
    }
    fn reg_bank(&self, _: u32) -> RegBank {
        self.bank
    }
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Jump {
    Unconditional,
    Equal,
    Unequal,
}

struct RecordingTarget<'log, const NEG: bool> {
    log: &'log RefCell<String>,
    fixed: bool,
    tables: bool,
    stride: u64,
    emit_bytes: bool,
    guard_mode: bool,
    guard_finished: bool,
    guard_force_fixed: Option<Reg>,
    spill_scratch: bool,
    inside_spill: bool,
    function_begin: usize,
    fail_after: Option<usize>,
    emitted: usize,
}

impl<'log, const NEG: bool> RecordingTarget<'log, NEG> {
    fn new(log: &'log RefCell<String>) -> Self {
        Self {
            log,
            fixed: true,
            tables: true,
            stride: 1,
            emit_bytes: true,
            guard_mode: false,
            guard_finished: false,
            guard_force_fixed: None,
            spill_scratch: false,
            inside_spill: false,
            function_begin: 0,
            fail_after: None,
            emitted: 0,
        }
    }
    fn record<'m, 'a, 'ir: 'a, M>(machine: &M, tag: &str, values: &[u64])
    where
        M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>,
    {
        machine.with_target(|target| record(target.log, tag, values));
    }
    fn emit<'m, 'a, 'ir: 'a, M>(machine: &M, tag: &str, values: &[u64]) -> PgResult<()>
    where
        M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>,
    {
        let bytes = machine.with_target(|target| -> PgResult<bool> {
            if target.fail_after == Some(target.emitted) {
                return Err(PgError::error("injected target emission failure").into());
            }
            target.emitted += 1;
            record(target.log, tag, values);
            Ok(target.emit_bytes)
        })?;
        if bytes {
            machine.with_output(|output| output.write_bytes(&[0]))?;
        }
        Ok(())
    }
    fn snapshot<'m, 'a, 'ir: 'a, M>(machine: &M, tag: &str)
    where
        M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>,
    {
        let registers = machine.with_registers(|registers| {
            [
                registers.allocatable.get(),
                registers.used.get(),
                registers.clobbered.get(),
            ]
        });
        Self::record(
            machine,
            tag,
            &[
                u64::from(machine.frame_size()),
                u64::from(machine.frame_has_dynamic_alloca()),
                u64::from(machine.call_is_leaf_function()),
                u64::from(machine.frame_generated_call()),
                u64::from(machine.frame_used()),
                registers[0],
                registers[1],
                registers[2],
                u64::from(machine.fixed_assignment_count(RegBank::new(0))),
                u64::from(machine.fixed_assignment_count(RegBank::new(1))),
            ],
        );
        machine.with_registers(|registers| {
            for id in registers.used_regs() {
                let reg = Reg::new(id);
                Self::record(
                    machine,
                    "register",
                    &[
                        id,
                        u64::from(registers.reg_local_idx(reg).get()),
                        u64::from(registers.reg_part(reg)),
                        u64::from(registers.lock_counts[id as usize / 32][id as usize % 32].get()),
                    ],
                );
            }
        });
        for local in 0..machine.assignment_slot_count() {
            let local_idx = ValLocalIdx::new(local as u32);
            if !machine.assignment_exists(local_idx) {
                continue;
            }
            let count = machine.with_assignment(local_idx, |assignment| {
                Self::record(
                    machine,
                    "assignment",
                    &[
                        local as u64,
                        u64::from(assignment.part_count()),
                        u64::from(assignment.references_left()),
                        u64::from(assignment.pending_free()),
                        u64::from(assignment.delay_free()),
                        u64::from(assignment.variable_ref()),
                        u64::from(assignment.stack_variable()),
                        assignment.frame_off() as i64 as u64,
                        u64::from(assignment.max_part_size()),
                    ],
                );
                assignment.part_count()
            });
            for index in 0..count {
                machine.with_part(local_idx, index, |part| {
                    Self::record(
                        machine,
                        "part",
                        &[
                            local as u64,
                            u64::from(index),
                            u64::from(part.bank().id()),
                            u64::from(part.get_reg().id()),
                            u64::from(part.part_size()),
                            u64::from(part.fixed_assignment()),
                            u64::from(part.register_valid()),
                            u64::from(part.modified()),
                        ],
                    )
                });
            }
        }
    }
}

impl<'a, 'ir, 'log, const NEG: bool> CompilerTarget<Adaptor<'a, 'ir>, 2>
    for RecordingTarget<'log, NEG>
{
    type Special = ValRefSpecial<()>;
    type Parts = Parts;
    type Jump = Jump;
    type Writer = Writer<'log>;
    type CCPolicy = Policy;
    const ARCHITECTURE: Architecture = Architecture::Aarch64;
    const FRAME_INDEXING_NEGATIVE: bool = NEG;
    const PLATFORM_POINTER_SIZE: u32 = 8;
    const GP_BANK: RegBank = RegBank::new(0);
    const FIXED_ASSIGNMENTS: [u32; 2] = [4, 4];
    const MAX_STATIC_ALLOCA_SIZE: u32 = adaptor::MAX_STATIC_ALLOCA_SIZE;
    const DEFAULT_VAR_REF_HANDLING: bool = true;
    fn reset(&mut self) {
        self.emitted = 0;
        self.function_begin = 0;
        self.guard_finished = false;
        self.guard_force_fixed = None;
        self.inside_spill = false;
    }
    fn cc_info(&self, _: &Adaptor<'a, 'ir>) -> CCInfo {
        cc_info()
    }
    fn new_cc_policy(&self, _: &Adaptor<'a, 'ir>) -> Policy {
        Policy::new()
    }
    fn val_parts(&self, adaptor: &Adaptor<'a, 'ir>, value: IRValueRef) -> Parts {
        let name = adaptor
            .ir()
            .name(adaptor.ir().values[value.0 as usize].name);
        Parts {
            count: if name.starts_with("p2_") {
                2
            } else if name.starts_with("p3_") {
                3
            } else {
                1
            },
            bank: RegBank::new(u8::from(name.starts_with("v_"))),
        }
    }
    fn val_ref_special(&self, _: &Adaptor<'a, 'ir>, _: IRValueRef) -> Option<Self::Special> {
        None
    }
    fn cur_func_may_emit_calls(&self, adaptor: &Adaptor<'a, 'ir>) -> bool {
        adaptor.ir().functions[adaptor.current_function() as usize].has_call
    }
    fn cur_personality_func(&self, _: &Adaptor<'a, 'ir>) -> SymRef {
        SymRef::default()
    }
    fn define_func_idx(&mut self, _: &Adaptor<'a, 'ir>, function: IRFuncRef, index: u32) {
        if !self.guard_mode {
            record(
                self.log,
                "function.symbol",
                &[u64::from(function.0), u64::from(index)],
            );
        }
    }
    fn try_force_fixed_assignment(&self, adaptor: &Adaptor<'a, 'ir>, value: IRValueRef) -> bool {
        if self.guard_mode {
            self.guard_force_fixed.is_some()
        } else {
            adaptor.ir().values[value.0 as usize].force_fixed_assignment
        }
    }
    fn unconditional_jump() -> Jump {
        Jump::Unconditional
    }
    fn invert_jump(jump: Jump) -> Jump {
        match jump {
            Jump::Equal => Jump::Unequal,
            Jump::Unequal => Jump::Equal,
            Jump::Unconditional => panic!("cannot invert unconditional jump"),
        }
    }
    fn special_part<
        's,
        'm,
        M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self, Special = Self::Special>,
    >(
        _: &'s M,
        _: &'s Self::Special,
        _: u32,
    ) -> ValuePartRef<'s, M, 2>
    where
        Adaptor<'a, 'ir>: 's,
    {
        panic!("recording target admits no special values")
    }
    fn select_fixed_assignment_reg<'m, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>>(
        machine: &M,
        local: ValLocalIdx,
        part: u32,
        _: IRValueRef,
    ) -> Reg {
        let (guard, requested, enabled) = machine
            .with_target(|target| (target.guard_mode, target.guard_force_fixed, target.fixed));
        if guard {
            return requested.unwrap_or(Reg::make_invalid());
        }
        if !enabled {
            return Reg::make_invalid();
        }
        let bank = machine.with_part(local, part, |part| part.bank());
        machine.with_registers(|registers| {
            registers.find_first_free_excluding(bank, !0x0000_0f00_0000_0f00)
        })
    }
    fn start_func<'m, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>>(
        machine: &M,
        index: u32,
    ) -> PgResult<()> {
        machine.set_frame_size(16);
        let used = machine.frame_used();
        machine.set_frame_used(true);
        assert!(machine.frame_used());
        machine.set_frame_used(false);
        assert!(!machine.frame_used());
        machine.set_frame_used(used);
        if machine.with_target(|target| target.guard_mode) {
            machine.with_registers(|registers| {
                registers.reset();
                registers.allocatable.set(cc_info().allocatable_regs());
            });
            guards::run(machine)?;
            machine.with_target(|target| target.guard_finished = true);
            return Err(PgError::error("recording guard fixture completed").into());
        }
        let offset = machine.with_output(|output| output.offset());
        machine.with_target(|target| target.function_begin = offset);
        Self::emit(machine, "function.start", &[u64::from(index)])
    }
    fn finish_func<'m, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>>(
        machine: &M,
        index: u32,
    ) -> PgResult<()> {
        Self::snapshot(machine, "function.state");
        let begin = machine.with_target(|target| target.function_begin);
        let size = machine.with_output(|output| {
            let size = output.offset() - begin;
            let section = output.section();
            output.assembler_mut().sym_def(
                machine.function_symbol(index),
                section,
                begin as u64,
                size as u64,
            )?;
            Ok::<_, Box<PgError>>(size)
        })?;
        Self::record(machine, "function.end", &[u64::from(index), size as u64]);
        Ok(())
    }
    fn prologue_begin<
        'm,
        M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>,
        C: CallingConventionAssigner,
    >(
        machine: &M,
        _: &mut C,
    ) -> PgResult<()> {
        Self::emit(machine, "prologue.begin", &[])
    }
    fn prologue_end<
        'm,
        M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>,
        C: CallingConventionAssigner,
    >(
        machine: &M,
        assigner: &mut C,
    ) -> PgResult<()> {
        Self::emit(
            machine,
            "prologue.end",
            &[u64::from(assigner.get_stack_size())],
        )
    }
    fn prologue_assign_arg_part<'m, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>>(
        machine: &M,
        mut part: ValuePartRef<'_, M, 2>,
        assignment: CCAssignment,
    ) -> PgResult<Option<i32>> {
        Self::emit(
            machine,
            "prologue.part",
            &[
                u64::from(part.local_idx().get()),
                u64::from(part.part()),
                u64::from(assignment.reg.id()),
                u64::from(assignment.stack_off),
                u64::from(assignment.size),
                u64::from(assignment.byval()),
                u64::from(assignment.sret()),
            ],
        )?;
        if assignment.byval() {
            return Ok(Some((64 + assignment.stack_off) as i32));
        }
        if assignment.reg.valid() {
            part.set_value_reg(assignment.reg)?;
            machine.with_registers(|registers| {
                registers
                    .allocatable
                    .set(registers.allocatable.get() | 1u64 << assignment.reg.id())
            });
        } else {
            let reg = part.alloc_reg()?;
            Self::emit(
                machine,
                "prologue.load",
                &[
                    u64::from(reg.id()),
                    u64::from(assignment.stack_off),
                    u64::from(assignment.size),
                ],
            )?;
        }
        Ok(None)
    }
    fn compile_inst<'m, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>>(
        machine: &M,
        instruction: IRInstRef,
        _: impl Iterator<Item = IRInstRef> + Clone,
    ) -> PgResult<bool> {
        let ir = machine.adaptor().ir();
        let value = ir.values[instruction.0 as usize];
        let operand =
            |index: u32| IRValueRef(ir.value_operands[(value.op_begin_idx + index) as usize]);
        let block =
            |index: u32| IRBlockRef(ir.value_operands[(value.op_begin_idx + index) as usize]);
        Self::record(
            machine,
            "instruction",
            &[u64::from(instruction.0), value.op as u64],
        );
        match value.op {
            Op::Alloca => {}
            Op::Terminate | Op::Ret => {
                machine.with_cur_cc_assigner(|assigner| -> PgResult<()> {
                    let mut builder = RetBuilder::new(machine, assigner);
                    if value.op_count != 0 {
                        builder.add(&operand(0))?;
                    }
                    builder.ret()
                })?;
            }
            Op::Trap => {
                Self::emit(machine, "trap", &[])?;
                machine.release_regs_after_return();
            }
            Op::Br => branches::generate_uncond_branch(machine, block(0))?,
            Op::Condbr | Op::Tbz => {
                let owner = machine.value_ref(operand(0))?;
                let mut part = owner.part(0);
                let reg = part.load_to_reg()?;
                Self::emit(
                    machine,
                    "condition",
                    &[
                        u64::from(reg.id()),
                        if value.op == Op::Tbz {
                            u64::from(ir.value_operands[(value.op_begin_idx + 3) as usize])
                        } else {
                            64
                        },
                    ],
                )?;
                branches::generate_cond_branch(machine, Jump::Unequal, block(1), block(2))?;
            }
            Op::Jump => {
                let mut condition = ScratchReg::new(machine);
                condition.alloc_gp()?;
                let count = value.op_end_idx - value.op_begin_idx;
                assert!(count > 0);
                let stride = machine.with_target(|target| target.stride);
                assert!(stride != 0 && (count <= 1 || u64::from(count - 2) <= u64::MAX / stride));
                let cases: Vec<_> = (1..count)
                    .map(|index| (u64::from(index - 1) * stride, block(index)))
                    .collect();
                branches::generate_switch(machine, condition, 63, block(0), &cases)?;
            }
            Op::Call => {
                let info = cc_info();
                let mut assigner = CCAssigner::new(&info, Policy::new());
                let mut hooks = Hooks::<'a, 'ir, 'log, NEG>(PhantomData);
                let mut builder = CallBuilderBase::new(machine, &mut assigner, &mut hooks);
                let mut result = machine.result_ref(instruction.into())?;
                for index in 0..value.op_count {
                    let value = operand(index);
                    let name = ir.name(ir.values[value.0 as usize].name);
                    let mut argument = CallArg::new(value);
                    if name.starts_with("byval_") {
                        argument = CallArg::with_flags(value, CallArgFlag::Byval, 8, 24);
                    } else if name.starts_with("sret_") {
                        argument.flag = CallArgFlag::Sret;
                    } else if name.starts_with("sext_") {
                        argument.flag = CallArgFlag::Sext;
                        argument.ext_bits = 8;
                    } else if name.starts_with("zext_") {
                        argument.flag = CallArgFlag::Zext;
                        argument.ext_bits = 8;
                    } else if name.starts_with("split_") {
                        argument.flag = CallArgFlag::AllowSplit;
                    }
                    builder.add_arg(&argument)?;
                }
                builder.call(CallTarget::Symbol(
                    machine.function_symbol(value.call_func_idx),
                ))?;
                builder.add_ret(&mut result)?;
            }
            Op::Any | Op::Add | Op::Sub | Op::Condselect | Op::Zerofill => {
                let mut owners = Vec::with_capacity(value.op_count as usize);
                for index in 0..value.op_count {
                    owners.push(machine.value_ref(operand(index))?);
                }
                let mut parts = Vec::new();
                for (index, owner) in owners.iter().enumerate() {
                    let count = machine.with_target(|target| {
                        target
                            .val_parts(machine.adaptor(), operand(index as u32))
                            .count()
                    });
                    for part in 0..count {
                        let mut part = owner.part(part);
                        part.load_to_reg()?;
                        parts.push(part);
                    }
                }
                let result = machine.result_ref(instruction.into())?;
                let count = machine.with_target(|target| {
                    target
                        .val_parts(machine.adaptor(), instruction.into())
                        .count()
                });
                for index in 0..count {
                    let mut part = result.part(index);
                    let length = parts.len();
                    let reg = if length != 0
                        && parts[index as usize % length].has_reg()
                        && parts[index as usize % length].bank() == part.bank()
                    {
                        part.alloc_try_reuse(&mut parts[index as usize % length])?
                    } else {
                        part.alloc_reg()?
                    };
                    Self::emit(
                        machine,
                        "operation",
                        &[
                            value.op as u64,
                            u64::from(index),
                            u64::from(reg.id()),
                            length as u64,
                        ],
                    )?;
                    part.set_modified();
                }
                drop(result);
                while let Some(part) = parts.pop() {
                    drop(part);
                }
                drop(parts);
                for owner in owners.into_iter().rev() {
                    drop(owner);
                }
            }
            Op::None => return Ok(false),
        }
        Self::snapshot(machine, "instruction.state");
        Ok(true)
    }
    fn alloca_fixed<'m, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>>(
        machine: &M,
        size: u32,
        alignment: u32,
        mut part: ValuePartRef<'_, M, 2>,
    ) -> PgResult<()> {
        let reg = part.alloc_reg()?;
        Self::emit(
            machine,
            "dynamic.alloca",
            &[u64::from(size), u64::from(alignment), u64::from(reg.id())],
        )
    }
    fn setup_var_ref_assignments<'m, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>>(
        _: &M,
    ) -> PgResult<()> {
        panic!("recording target uses default variable reference policy")
    }
    fn load_address_of_stack_var<'m, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>>(
        machine: &M,
        reg: Reg,
        local: ValLocalIdx,
        part: u32,
    ) -> PgResult<()> {
        let offset = machine.with_part(local, part, |part| part.variable_stack_off());
        Self::emit(
            machine,
            "stack.address",
            &[u64::from(reg.id()), offset as i64 as u64],
        )
    }
    fn load_address_of_var_reference<
        'm,
        M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>,
    >(
        machine: &M,
        reg: Reg,
        local: ValLocalIdx,
        part: u32,
    ) -> PgResult<()> {
        let index = machine.with_part(local, part, |part| part.variable_ref_data());
        Self::emit(
            machine,
            "variable.address",
            &[u64::from(reg.id()), u64::from(index)],
        )
    }
    fn spill_reg<'m, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>>(
        machine: &M,
        reg: Reg,
        offset: i32,
        size: u32,
    ) -> PgResult<()> {
        let scratch = machine.with_target(|target| {
            let take = target.spill_scratch && !target.inside_spill && reg == Reg::new(7);
            if take {
                target.inside_spill = true;
            }
            take
        });
        if scratch {
            let result = (|| -> PgResult<()> {
                let mut scratch = ScratchReg::new(machine);
                scratch.alloc_specific(Reg::new(2))?;
                Self::emit(
                    machine,
                    "spill.scratch",
                    &[u64::from(scratch.cur_reg().id())],
                )
            })();
            machine.with_target(|target| target.inside_spill = false);
            result?;
        }
        Self::emit(
            machine,
            "spill",
            &[
                u64::from(reg.id()),
                u64::from(offset as u32),
                u64::from(size),
            ],
        )
    }
    fn load_from_stack<'m, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>>(
        machine: &M,
        reg: Reg,
        offset: i32,
        size: u32,
    ) -> PgResult<()> {
        Self::emit(
            machine,
            "reload",
            &[
                u64::from(reg.id()),
                u64::from(offset as u32),
                u64::from(size),
            ],
        )
    }
    fn mov<'m, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>>(
        machine: &M,
        destination: Reg,
        source: Reg,
        size: u32,
    ) -> PgResult<()> {
        Self::emit(
            machine,
            "move",
            &[
                u64::from(destination.id()),
                u64::from(source.id()),
                u64::from(size),
            ],
        )
    }
    fn materialize_constant<'m, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>>(
        machine: &M,
        words: &[u64],
        bank: RegBank,
        size: u32,
        reg: Reg,
    ) -> PgResult<()> {
        Self::emit(
            machine,
            "constant",
            &[u64::from(bank.id()), u64::from(size), u64::from(reg.id())],
        )?;
        for word in &words[..size.div_ceil(8) as usize] {
            Self::record(machine, "constant.word", &[*word]);
        }
        Ok(())
    }
    fn generate_raw_intext<'m, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>>(
        machine: &M,
        destination: Reg,
        source: Reg,
        sign: bool,
        from: u32,
        to: u32,
    ) -> PgResult<()> {
        Self::emit(
            machine,
            "extend",
            &[
                u64::from(destination.id()),
                u64::from(source.id()),
                u64::from(sign),
                u64::from(from),
                u64::from(to),
            ],
        )
    }
    fn gval_expr_as_reg<'s, 'm, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>>(
        machine: &'s M,
        value: &mut GenericValuePart<'s, M, 2>,
    ) -> PgResult<Reg> {
        let GenericValuePart::Expr(expr) = value else {
            panic!("generic value is not expression");
        };
        let base = if expr.has_base() {
            expr.base_reg()
        } else {
            Reg::make_invalid()
        };
        let index = if expr.has_index() {
            expr.index_reg()
        } else {
            Reg::make_invalid()
        };
        let (scale, disp) = (expr.scale, expr.disp);
        let mut scratch = ScratchReg::new(machine);
        let reg = scratch.alloc_gp()?;
        Self::emit(
            machine,
            "expression",
            &[
                u64::from(reg.id()),
                u64::from(base.id()),
                u64::from(index.id()),
                scale as u64,
                disp as u64,
            ],
        )?;
        *value = scratch.into();
        Ok(reg)
    }
    fn generate_raw_jump<'m, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>>(
        machine: &M,
        jump: Jump,
        label: Label,
    ) -> PgResult<()> {
        Self::emit(machine, "jump", &[jump as u64, u64::from(label.0)])
    }
    fn switch_emit_cmpeq<'m, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>>(
        machine: &M,
        label: Label,
        cmp: Reg,
        tmp: Reg,
        value: u64,
        width32: bool,
    ) -> PgResult<()> {
        Self::emit(
            machine,
            "switch.equal",
            &[
                u64::from(label.0),
                u64::from(cmp.id()),
                u64::from(tmp.id()),
                value,
                u64::from(width32),
            ],
        )
    }
    fn switch_emit_binary_step<'m, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>>(
        machine: &M,
        equal: Label,
        greater: Label,
        cmp: Reg,
        tmp: Reg,
        value: u64,
        width32: bool,
    ) -> PgResult<()> {
        Self::emit(
            machine,
            "switch.binary",
            &[
                u64::from(equal.0),
                u64::from(greater.0),
                u64::from(cmp.id()),
                u64::from(tmp.id()),
                value,
                u64::from(width32),
            ],
        )
    }
    fn switch_create_jump_table<'m, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>>(
        machine: &M,
        default: Label,
        cmp: Reg,
        tmp: Reg,
        low: u64,
        high: u64,
        width32: bool,
        fill: impl FnOnce(&mut [Label]),
    ) -> PgResult<bool> {
        let tables = machine.with_target(|target| target.tables);
        Self::emit(
            machine,
            "switch.table",
            &[
                u64::from(default.0),
                u64::from(cmp.id()),
                u64::from(tmp.id()),
                low,
                high,
                u64::from(width32),
                u64::from(tables),
            ],
        )?;
        if !tables {
            return Ok(false);
        }
        machine.with_output(|output| {
            output.jump_table((high - low + 1) as u32, cmp, tmp, width32, fill)
        })?;
        Ok(true)
    }
    fn gen_func_epilog<'m, M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = Self>>(
        machine: &M,
    ) -> PgResult<()> {
        Self::emit(machine, "epilogue", &[])
    }
}

struct Hooks<'a, 'ir, 'log, const NEG: bool>(PhantomData<(&'a &'ir (), &'log ())>);
impl<'a, 'ir, 'log, 'm, const NEG: bool, M> CallBuilderHooks<M, 2> for Hooks<'a, 'ir, 'log, NEG>
where
    M: TargetMachine<'m, Adaptor<'a, 'ir>, 2, Target = RecordingTarget<'log, NEG>>,
{
    fn add_arg_byval<C: CallingConventionAssigner>(
        &mut self,
        machine: &M,
        _: &mut C,
        value: &mut ValuePartRef<'_, M, 2>,
        assignment: &mut CCAssignment,
    ) -> PgResult<()> {
        let reg = value.cur_reg_or_load()?;
        RecordingTarget::emit(
            machine,
            "call.byval",
            &[
                u64::from(reg.id()),
                u64::from(assignment.stack_off),
                u64::from(assignment.size),
                u64::from(assignment.align),
            ],
        )
    }
    fn add_arg_stack<C: CallingConventionAssigner>(
        &mut self,
        machine: &M,
        _: &mut C,
        value: &mut ValuePartRef<'_, M, 2>,
        assignment: &mut CCAssignment,
    ) -> PgResult<()> {
        let reg = value.cur_reg_or_load()?;
        RecordingTarget::emit(
            machine,
            "call.stack",
            &[
                u64::from(reg.id()),
                u64::from(assignment.stack_off),
                u64::from(assignment.size),
            ],
        )
    }
    fn call_impl<C: CallingConventionAssigner>(
        &mut self,
        machine: &M,
        _: &mut C,
        target: CallTarget<'_, M, 2>,
    ) -> PgResult<()> {
        match target {
            CallTarget::Symbol(symbol) => {
                RecordingTarget::emit(machine, "call.direct", &[u64::from(symbol.id())])
            }
            CallTarget::Value(mut part) => {
                let reg = part.cur_reg_or_load()?;
                RecordingTarget::emit(machine, "call.indirect", &[u64::from(reg.id())])?;
                part.reset();
                Ok(())
            }
        }
    }
}

fn frame(log: &RefCell<String>, mcx: Mcx<'_>, source: &str, negative: bool) {
    let mut frame = FrameState::new(mcx);
    frame.frame_size = 16;
    let mut words = source.split_whitespace();
    while let Some(command) = words.next() {
        match command {
            "alloc" => {
                let size: u32 = words.next().unwrap().parse().unwrap();
                let offset = frame.allocate(size, negative).unwrap();
                record(
                    log,
                    "alloc",
                    &[
                        u64::from(size),
                        offset as i64 as u64,
                        u64::from(frame.frame_size),
                    ],
                );
            }
            "free" => {
                let offset: i64 = words.next().unwrap().parse().unwrap();
                let size: u32 = words.next().unwrap().parse().unwrap();
                frame.free(offset as i32, size);
                record(
                    log,
                    "free",
                    &[u64::from(size), offset as u64, u64::from(frame.frame_size)],
                );
            }
            _ => panic!("unknown frame command"),
        }
    }
    for index in 0..5 {
        let list = frame.fixed_free_list(index);
        record(log, "freelist", &[index as u64, list.len() as u64]);
        for &offset in list {
            record(log, "free.slot", &[offset as i64 as u64]);
        }
    }
    let mut dynamic: Vec<_> = frame
        .dynamic_free_lists()
        .filter(|(_, list)| !list.is_empty())
        .collect();
    dynamic.sort_unstable_by_key(|(size, _)| *size);
    for (size, list) in dynamic {
        record(
            log,
            "dynamic.freelist",
            &[u64::from(size), list.len() as u64],
        );
        for &offset in list {
            record(log, "free.slot", &[offset as i64 as u64]);
        }
    }
}

fn execute<const NEG: bool>(
    mode: &str,
    source: &str,
    fixed: bool,
    stride: u64,
    tables: bool,
) -> String {
    let log = RefCell::new(String::new());
    record(&log, "tpde.compiler.state", &[1, u64::from(NEG)]);
    if mode == "sanitizer" {
        record(
            &log,
            "sanitizer",
            &[u64::from(tpde_util::asan::ADDRESS_SANITIZER_ACTIVE)],
        );
        return log.into_inner();
    }
    let context = MemoryContext::new_bump("compiler-fixture");
    if mode == "frame" {
        frame(&log, context.mcx(), source, NEG);
        return log.into_inner();
    }
    assert!(matches!(mode, "compile" | "guards"));
    let mut ir = TestIR::new(context.mcx());
    let input = if mode == "guards" {
        "fixture(%a, %b) {\nentry:\nret %a\n}\n"
    } else {
        source
    };
    if ir.parse_ir(input).is_err() {
        assert_ne!(mode, "guards");
        record(&log, "parse", &[0]);
        return log.into_inner();
    }
    if mode == "compile" {
        record(&log, "parse", &[1]);
    }
    let target = RecordingTarget::<NEG> {
        log: &log,
        fixed,
        tables,
        stride,
        emit_bytes: mode == "compile",
        guard_mode: mode == "guards",
        guard_finished: false,
        guard_force_fixed: None,
        spill_scratch: false,
        inside_spill: false,
        function_begin: 0,
        fail_after: None,
        emitted: 0,
    };
    with_compiler_base(
        context.mcx(),
        Adaptor::new(&ir),
        target,
        Writer { log: &log },
        Architecture::Aarch64,
        |mut compiler| {
            compiler.adaptor.start_compile();
            let result = compiler.compile();
            compiler.adaptor.end_compile();
            if mode == "guards" {
                assert!(
                    result.is_err() && compiler.target().guard_finished,
                    "guard fixture failed before completion: {result:?}"
                );
            } else {
                let success = result.unwrap();
                record(&log, "compile", &[u64::from(success)]);
                compiler.reset().unwrap();
                let counts = compiler.reset_state_counts();
                record(
                    &log,
                    "reset",
                    &[counts.0 as u64, counts.1 as u64, counts.2 as u64],
                );
                assert_eq!(counts, (0, 0, 0));
            }
        },
    )
    .unwrap();
    log.into_inner()
}

pub fn run(
    mode: &str,
    source: &str,
    negative: bool,
    fixed: bool,
    stride: u64,
    tables: bool,
) -> String {
    if negative {
        execute::<true>(mode, source, fixed, stride, tables)
    } else {
        execute::<false>(mode, source, fixed, stride, tables)
    }
}

#[test]
fn complete_compiler_runs_real_parser_analyzer_and_writer() {
    let output = run(
        "compile",
        "add(%a, %b) {\nentry:\n%r = add %a, %b\nret %r\n}\n",
        true,
        true,
        1,
        true,
    );
    assert!(output.contains("parse 1\n"));
    assert!(output.contains("operation 2 "));
    assert!(output.contains("writer.finish "));
    assert!(output.ends_with("compile 1\nreset 0 0 0\n"));
}

#[test]
fn complete_compiler_guard_session_records_source_edge_cases() {
    let output = run("guards", "", true, true, 1, true);
    assert!(output.contains("part.default 255 0 0 0 0\n"));
    assert!(output.contains("specific.overwrite 7 132 0 0\n"));
    assert!(output.contains("specific.after.eviction "));
    assert!(output.contains("assigned.source 1 0 0 0 0\n"));
    assert!(!output.contains("prologue.begin"));
}

#[test]
fn emission_failure_abandons_output_and_reset_reuses_complete_compiler() {
    let context = MemoryContext::new_bump("compiler-failure-reuse");
    let mut ir = TestIR::new(context.mcx());
    ir.parse_ir("add(%a, %b) {\nentry:\n%r = add %a, %b\nret %r\n}\n")
        .unwrap();
    let log = RefCell::new(String::new());
    with_compiler_base(
        context.mcx(),
        Adaptor::new(&ir),
        RecordingTarget::<true>::new(&log),
        Writer { log: &log },
        Architecture::Aarch64,
        |mut compiler| {
            assert!(compiler.assembler().is_err());
            assert!(compiler.assembler_mut().is_err());
            compiler.adaptor.start_compile();
            assert!(compiler.compile().unwrap());
            compiler.adaptor.end_compile();
            assert!(compiler.assembler().is_ok());
            let text = {
                let output = compiler.assembler_mut().unwrap();
                let text = output.get_default_section(SectionKind::Text).unwrap();
                output.get_section_mut(text).unwrap().addr = 4096;
                text
            };
            assert_eq!(
                compiler
                    .assembler()
                    .unwrap()
                    .get_section(text)
                    .unwrap()
                    .addr,
                4096
            );
            let baseline = log.borrow().clone();
            let points = compiler.target().emitted;
            assert!(points >= 7);
            compiler.reset().unwrap();
            assert_eq!(compiler.target().emitted, 0);
            assert!(compiler.assembler_mut().is_err());
            for point in 0..points {
                log.borrow_mut().clear();
                compiler.target_mut().fail_after = Some(point);
                compiler.adaptor.start_compile();
                assert!(
                    compiler.compile().is_err(),
                    "missing injection point {point}"
                );
                compiler.adaptor.end_compile();
                assert!(compiler.assembler().is_err());
                assert!(compiler.assembler_mut().is_err());
                compiler.reset().unwrap();
                assert!(compiler.assembler_mut().is_err());
                assert_eq!(compiler.target().emitted, 0);
                assert_eq!(compiler.target().function_begin, 0);
                log.borrow_mut().clear();
                compiler.target_mut().fail_after = None;
                compiler.adaptor.start_compile();
                assert!(compiler.compile().unwrap(), "reuse after point {point}");
                compiler.adaptor.end_compile();
                assert_eq!(&*log.borrow(), &baseline, "state drift after point {point}");
                assert!(compiler.assembler().is_ok());
                assert!(compiler
                    .assembler_mut()
                    .unwrap()
                    .data_sections()
                    .all(|section| section.addr == 0));
                compiler.reset().unwrap();
            }
        },
    )
    .unwrap();
}

#[test]
fn compiler_budget_exhaustion_never_publishes_partial_assembler() {
    let ir_context = MemoryContext::new("compiler-budget-ir");
    let mut ir = TestIR::new(ir_context.mcx());
    ir.parse_ir("add(%a, %b) {\nentry:\n%r = add %a, %b\nret %r\n}\n")
        .unwrap();
    let mut failures = 0;
    let mut successes = 0;
    for budget in [64, 4096, 32768, 65536, 131072, 262144, 1048576] {
        let context = MemoryContext::new("compiler-budget").with_limit(budget);
        let log = RefCell::new(String::new());
        let result = with_compiler_base(
            context.mcx(),
            Adaptor::new(&ir),
            RecordingTarget::<true>::new(&log),
            Writer { log: &log },
            Architecture::Aarch64,
            |mut compiler| {
                compiler.adaptor.start_compile();
                let result = compiler.compile();
                compiler.adaptor.end_compile();
                if result.is_err() {
                    assert!(compiler.assembler().is_err());
                }
                result
            },
        )
        .and_then(core::convert::identity);
        match result {
            Ok(true) => successes += 1,
            Ok(false) => panic!("valid module refused"),
            Err(_) => failures += 1,
        }
        assert!(context.used() <= budget);
    }
    assert!(failures > 0 && successes > 0);
}

#[derive(Default)]
struct BridgeWriter {
    pending: bool,
}
impl FunctionWriterTarget for BridgeWriter {
    const CIE: TargetCieInfo<'static> = TargetCieInfo {
        code_alignment_factor: 4,
        ..Writer::CIE
    };
    fn handle_fixups(&mut self, _: &mut FunctionWriterBase<'_, '_, '_>) -> PgResult<()> {
        Ok(())
    }
    fn more_space(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        size: usize,
    ) -> PgResult<()> {
        let prefix = usize::from(self.pending) * 4;
        base.more_space(size + prefix)?;
        if self.pending {
            base.write_bytes_unchecked(&[0x7e; 4])?;
            self.pending = false;
        }
        Ok(())
    }
    fn ensure_space(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        size: usize,
    ) -> PgResult<()> {
        if size > 16384 {
            return Err(PgError::error("bridge target advancement exceeds 16 KiB").into());
        }
        base.ensure_space_with(self, size)
    }
    fn label_ref(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        label: Label,
        offset: u32,
        kind: LabelFixupKind,
    ) -> PgResult<()> {
        base.label_ref(label, offset, kind)?;
        self.pending = true;
        Ok(())
    }
    fn eh_advance(&mut self, base: &mut FunctionWriterBase<'_, '_, '_>, size: u64) -> PgResult<()> {
        base.eh_advance_raw(size / 4)
    }
}
impl FunctionWriterJumpTables for BridgeWriter {
    fn create_jump_table(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        size: u32,
        index: Reg,
        temporary: Reg,
        index_is_32: bool,
        fill: impl FnOnce(&mut [Label]),
    ) -> PgResult<()> {
        let table = base.jump_table_count();
        base.alloc_jump_table(size, index, temporary)?
            .set_misc(u8::from(index_is_32));
        self.ensure_space(base, 4)?;
        let offset = u32::try_from(base.offset()).unwrap();
        base.jump_table_mut(table)?.set_off(offset);
        base.skip_unchecked(4)?;
        fill(base.jump_table_mut(table)?.labels_mut());
        Ok(())
    }
}

fn with_bridge<R>(
    operation: impl for<'w, 'a, 'm> FnOnce(&mut ModuleWriter<'w, 'a, 'm, BridgeWriter>) -> R,
) -> R {
    let context = MemoryContext::new("compiler-output-bridge");
    let mut assembler = AssemblerElf::new(context.mcx(), Architecture::Aarch64).unwrap();
    let section = assembler.get_default_section(SectionKind::Text).unwrap();
    let mut writer = FunctionWriter::new(context.mcx(), BridgeWriter::default());
    let mut module = writer.begin_module(&mut assembler).unwrap();
    module.switch_section(section).unwrap();
    CompilerOutput::begin_func(&mut module, 1, 4).unwrap();
    operation(&mut module)
}

#[test]
fn compiler_output_reservation_preserves_target_label_and_growth_effects() {
    with_bridge(|output| {
        let label = CompilerOutput::label_create(output).unwrap();
        CompilerOutput::write_bytes(output, &[0; 4]).unwrap();
        CompilerOutput::label_ref(output, label, 0, LabelFixupKind::AARCH64_TEST_BR).unwrap();
        output
            .with_base(|base| base.cap_reserved_end(base.offset()))
            .unwrap();
        CompilerOutput::ensure_space(output, 4).unwrap();
        assert_eq!(output.offset(), 8);
        CompilerOutput::skip(output, 4).unwrap();
        CompilerOutput::label_place(output, label).unwrap();
        output.with_base(|base| {
            assert_eq!(base.offset(), 12);
            assert_eq!(
                base.data().unwrap(),
                &[0, 0, 0, 0, 0x7e, 0x7e, 0x7e, 0x7e, 0, 0, 0, 0]
            );
            assert_eq!(base.label_offset(label).unwrap(), 12);
            assert_eq!(base.label_fixups()[0].label, label);
            assert_eq!(base.label_fixups()[0].kind, LabelFixupKind::AARCH64_TEST_BR);
        });
    });
}

#[test]
fn compiler_output_cfi_uses_target_instruction_width() {
    with_bridge(|output| {
        let begin = output.with_base(|base| base.eh_size());
        for bytes in [252, 256, 262140] {
            CompilerOutput::eh_advance(output, bytes).unwrap();
        }
        output.with_base(|base| {
            assert_eq!(
                &base.eh_data().unwrap()[begin..],
                &[0x7f, 2, 64, 3, 255, 255]
            );
        });
    });
}

#[test]
fn compiler_output_jump_tables_preserve_width_code_reservation_and_labels() {
    for index_is_32 in [false, true] {
        with_bridge(|output| {
            let first = CompilerOutput::label_create(output).unwrap();
            let second = CompilerOutput::label_create(output).unwrap();
            CompilerOutput::write_bytes(output, &[0; 4]).unwrap();
            CompilerOutput::label_ref(output, first, 0, LabelFixupKind::AARCH64_COND_BR).unwrap();
            output
                .with_base(|base| base.cap_reserved_end(base.offset()))
                .unwrap();
            let mut calls = 0;
            CompilerOutput::jump_table(
                output,
                3,
                Reg::new(2),
                Reg::new(3),
                index_is_32,
                |labels| {
                    calls += 1;
                    labels.copy_from_slice(&[second, first, second]);
                },
            )
            .unwrap();
            assert_eq!(calls, 1);
            assert_eq!(output.offset(), 12);
            CompilerOutput::label_place(output, first).unwrap();
            CompilerOutput::write_bytes(output, &[0; 4]).unwrap();
            CompilerOutput::label_place(output, second).unwrap();
            output.with_base(|base| {
                assert_eq!(base.jump_table_count(), 1);
                let table = base.jump_table_header(0).unwrap();
                assert_eq!(
                    (table.off, table.size(), table.idx, table.tmp, table.misc),
                    (8, 3, Reg::new(2), Reg::new(3), u8::from(index_is_32))
                );
                assert_eq!(base.jump_table_labels(0).unwrap(), &[second, first, second]);
                assert_eq!(
                    &base.data().unwrap()[4..12],
                    &[0x7e, 0x7e, 0x7e, 0x7e, 0, 0, 0, 0]
                );
            });
        });
    }
}

#[test]
fn compiler_output_rejected_advances_preserve_pending_fixups_and_allow_reuse() {
    with_bridge(|output| {
        let label = CompilerOutput::label_create(output).unwrap();
        CompilerOutput::write_bytes(output, &[0; 4]).unwrap();
        CompilerOutput::label_ref(output, label, 0, LabelFixupKind::AARCH64_TEST_BR).unwrap();
        output
            .with_base(|base| base.cap_reserved_end(base.offset()))
            .unwrap();
        assert!(CompilerOutput::ensure_space(output, 16385).is_err());
        assert!(CompilerOutput::skip(output, 16385).is_err());
        assert_eq!(output.offset(), 4);
        CompilerOutput::skip(output, 4).unwrap();
        CompilerOutput::label_place(output, label).unwrap();
        output.with_base(|base| {
            assert_eq!(base.offset(), 12);
            assert_eq!(&base.data().unwrap()[4..8], &[0x7e; 4]);
            assert_eq!(base.label_fixups().len(), 1);
        });
    });
}
