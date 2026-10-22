// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{
    emission::{gp, xmm},
    emit, encoding_error, invalid, CCAssignerSysV, X64CompilerState, CC_INFO,
};
use core::marker::PhantomData;
use tpde_assembler::elf;
use tpde_compiler::{
    calls::{CallArg, CallBuilderBase, CallBuilderHooks, CallTarget},
    target::{CompilerOutput, TargetMachine},
    CompilerAdaptor, ScratchReg, ValuePartRef, ValueRef,
};
use tpde_core::{
    calling_convention::{CCAssigner, CCAssignment, CallingConventionAssigner},
    Reg, RegBank,
};
use tpde_fadec::*;
use tpde_util::{tpde_assert, tpde_assert_eq};
use types_error::PgResult;

pub struct CallBuilder<'m, A> {
    stack_adjust_off: u32,
    marker: PhantomData<fn() -> (&'m (), A)>,
}
impl<'m, A> Default for CallBuilder<'m, A> {
    fn default() -> Self {
        Self {
            stack_adjust_off: 0,
            marker: PhantomData,
        }
    }
}
impl<'m, A: CompilerAdaptor> CallBuilder<'m, A> {
    pub fn set_stack_used<M: TargetMachine<'m, A, 2>>(&mut self, machine: &M) -> PgResult<()>
    where
        M::Target: X64CompilerState,
    {
        if machine.frame_has_dynamic_alloca() && self.stack_adjust_off == 0 {
            self.stack_adjust_off = u32::try_from(machine.with_output(|output| output.offset()))
                .map_err(|_| invalid("X64 call stack patch offset overflow"))?;
            emit(machine, |output| fe64_SUB64ri(output, 0, FE_SP, 0x100))?;
            tpde_assert_eq!(
                machine.with_output(|output| output.offset()),
                self.stack_adjust_off as usize + 7
            );
        }
        Ok(())
    }
}
impl<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>> CallBuilderHooks<M, 2>
    for CallBuilder<'m, A>
where
    M::Target: X64CompilerState,
{
    fn add_arg_byval<C: CallingConventionAssigner>(
        &mut self,
        machine: &M,
        _assigner: &mut C,
        value: &mut ValuePartRef<'_, M, 2>,
        assignment: &mut CCAssignment,
    ) -> PgResult<()> {
        let pointer = gp(value.load_to_reg()?);
        let mut scratch = ScratchReg::new(machine);
        let temporary = gp(scratch.alloc_gp()?);
        let mut size = assignment.size;
        self.set_stack_used(machine)?;
        let mut offset = 0i32;
        let stack = i32::try_from(assignment.stack_off)
            .map_err(|_| invalid("X64 call argument stack offset overflow"))?;
        while size >= 8 {
            let source = FE_MEM(pointer, 0, FE_NOREG, offset).map_err(encoding_error)?;
            let destination = FE_MEM(
                FE_SP,
                0,
                FE_NOREG,
                stack
                    .checked_add(offset)
                    .ok_or_else(|| invalid("X64 byval stack offset overflow"))?,
            )
            .map_err(encoding_error)?;
            emit(machine, |out| fe64_MOV64rm(out, 0, temporary, source))?;
            emit(machine, |out| fe64_MOV64mr(out, 0, destination, temporary))?;
            offset = offset
                .checked_add(8)
                .ok_or_else(|| invalid("X64 byval size overflow"))?;
            size -= 8;
        }
        for width in [4, 2, 1] {
            if size >= width {
                let source = FE_MEM(pointer, 0, FE_NOREG, offset).map_err(encoding_error)?;
                let destination = FE_MEM(
                    FE_SP,
                    0,
                    FE_NOREG,
                    stack
                        .checked_add(offset)
                        .ok_or_else(|| invalid("X64 byval stack offset overflow"))?,
                )
                .map_err(encoding_error)?;
                match width {
                    4 => {
                        emit(machine, |out| fe64_MOV32rm(out, 0, temporary, source))?;
                        emit(machine, |out| fe64_MOV32mr(out, 0, destination, temporary))?;
                    }
                    2 => {
                        emit(machine, |out| fe64_MOVZXr32m16(out, 0, temporary, source))?;
                        emit(machine, |out| fe64_MOV16mr(out, 0, destination, temporary))?;
                    }
                    _ => {
                        emit(machine, |out| fe64_MOVZXr32m8(out, 0, temporary, source))?;
                        emit(machine, |out| fe64_MOV8mr(out, 0, destination, temporary))?;
                    }
                }
                offset += width as i32;
                size -= width;
            }
        }
        Ok(())
    }
    fn add_arg_stack<C: CallingConventionAssigner>(
        &mut self,
        machine: &M,
        _assigner: &mut C,
        value: &mut ValuePartRef<'_, M, 2>,
        assignment: &mut CCAssignment,
    ) -> PgResult<()> {
        self.set_stack_used(machine)?;
        let reg = if value.has_reg() {
            value.cur_reg()
        } else {
            value.load_to_reg()?
        };
        let memory = FE_MEM(
            FE_SP,
            0,
            FE_NOREG,
            i32::try_from(assignment.stack_off)
                .map_err(|_| invalid("X64 call stack argument overflow"))?,
        )
        .map_err(encoding_error)?;
        let bank = tpde_core::RegisterFile::<2, 32>::reg_bank(reg);
        if bank == RegBank::new(0) {
            let reg = gp(reg);
            match assignment.size {
                1 => emit(machine, |out| fe64_MOV8mr(out, 0, memory, reg)),
                2 => emit(machine, |out| fe64_MOV16mr(out, 0, memory, reg)),
                4 => emit(machine, |out| fe64_MOV32mr(out, 0, memory, reg)),
                8 => emit(machine, |out| fe64_MOV64mr(out, 0, memory, reg)),
                _ => Err(invalid("invalid X64 GP call argument size")),
            }
        } else {
            tpde_assert_eq!(bank, RegBank::new(1));
            let reg = xmm(reg);
            match assignment.size {
                4 => emit(machine, |out| fe64_SSE_MOVSSmr(out, 0, memory, reg)),
                8 => emit(machine, |out| fe64_SSE_MOVSDmr(out, 0, memory, reg)),
                16 => emit(machine, |out| fe64_SSE_MOVDQAmr(out, 0, memory, reg)),
                _ => Err(invalid("invalid X64 vector call argument size")),
            }
        }
    }
    fn call_impl<C: CallingConventionAssigner>(
        &mut self,
        machine: &M,
        assigner: &mut C,
        target: CallTarget<'_, M, 2>,
    ) -> PgResult<()> {
        if assigner.is_vararg() {
            if machine.with_registers(|registers| registers.is_used(Reg::new(0))) {
                machine.evict_reg(Reg::new(0))?;
            }
            let next = machine.with_registers(|registers| {
                registers.find_first_free_excluding(RegBank::new(1), 0)
            });
            let count = if next.valid() && next.id() - 32 < 8 {
                u32::from(next.id() - 32)
            } else {
                8
            };
            if count != 0 {
                emit(machine, |out| fe64_MOV32ri(out, 0, FE_AX, count as i32))?;
            } else {
                emit(machine, |out| fe64_XOR32rr(out, 0, FE_AX, FE_AX))?;
            }
        }
        let mut subtraction = 0u32;
        let size = assigner.get_stack_size();
        if self.stack_adjust_off != 0 {
            subtraction = size
                .checked_add(15)
                .ok_or_else(|| invalid("X64 outgoing frame size overflow"))?
                & !15;
            if subtraction > i32::MAX as u32 {
                return Err(invalid("X64 outgoing frame exceeds signed immediate"));
            }
            let position = self.stack_adjust_off as usize + 3;
            machine.with_output(|output| {
                output.with_base(|base| -> PgResult<()> {
                    base.data_mut()?[position..position + 4]
                        .copy_from_slice(&subtraction.to_le_bytes());
                    Ok(())
                })
            })?;
        } else {
            machine.with_target(|target| {
                let maximum = &mut target.x64_mut().max_callee_stack_arg_size;
                *maximum = (*maximum).max(size);
            });
        }
        match target {
            CallTarget::Symbol(symbol) => {
                let target = machine
                    .with_output(|output| output.offset())
                    .checked_add(5)
                    .ok_or_else(|| invalid("X64 call address overflow"))?;
                emit(machine, |out| fe64_CALL(out, 0, target))?;
                let offset = machine.with_output(|output| output.offset()) - 4;
                machine.reloc_text(symbol, elf::R_X86_64_PLT32, offset as u64, -4)?;
            }
            CallTarget::Value(mut value) => {
                if value.has_assignment()
                    && value.with_assignment(|assignment| !assignment.register_valid())
                {
                    let offset = value.with_assignment(|assignment| {
                        tpde_assert!(assignment.stack_valid());
                        assignment.frame_off()
                    });
                    let memory = FE_MEM(FE_BP, 0, FE_NOREG, offset).map_err(encoding_error)?;
                    emit(machine, |out| fe64_CALLm(out, 0, memory))?;
                } else if value.can_salvage() {
                    let reg = gp(value.salvage());
                    emit(machine, |out| fe64_CALLr(out, 0, reg))?;
                } else {
                    tpde_assert!(
                        !machine.with_registers(|registers| registers.is_used(Reg::new(10)))
                    );
                    let reg = gp(value.reload_into_specific_fixed(Reg::new(10), 0)?);
                    emit(machine, |out| fe64_CALLr(out, 0, reg))?;
                }
                value.reset();
            }
        }
        if self.stack_adjust_off != 0 {
            emit(machine, |out| {
                fe64_ADD64ri(out, 0, FE_SP, subtraction as i32)
            })?;
        }
        Ok(())
    }
}

pub fn generate_call<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    target: CallTarget<'_, M, 2>,
    arguments: &[CallArg<A::ValueRef>],
    result: Option<&mut ValueRef<'_, M, 2>>,
    variable_args: bool,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    generate_call_iter(
        machine,
        target,
        arguments.iter().copied(),
        result,
        variable_args,
    )
}

pub fn generate_call_iter<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    target: CallTarget<'_, M, 2>,
    arguments: impl IntoIterator<Item = CallArg<A::ValueRef>>,
    result: Option<&mut ValueRef<'_, M, 2>>,
    variable_args: bool,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let mut assigner = CCAssigner::new(&CC_INFO, CCAssignerSysV::new(variable_args));
    let mut hooks = CallBuilder::<'m, A>::default();
    let mut builder = CallBuilderBase::new(machine, &mut assigner, &mut hooks);
    for argument in arguments {
        builder.add_arg(&argument)?;
    }
    builder.call(target)?;
    if let Some(result) = result {
        builder.add_ret(result)?;
    }
    Ok(())
}
