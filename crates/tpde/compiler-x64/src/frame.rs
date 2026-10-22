// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{
    emission::{gp, xmm},
    emit, encoding_error, invalid, X64CompilerState,
};
use tpde_base::ValLocalIdx;
use tpde_compiler::{
    guard_machine::INVALID_VALUE,
    target::{CompilerOutput, TargetMachine},
    CompilerAdaptor, ValuePartRef,
};
use tpde_core::{
    calling_convention::{CCAssignment, CallingConventionAssigner},
    Reg, RegBank,
};
use tpde_fadec::*;
use tpde_util::dwarf;
use tpde_util::{tpde_assert, tpde_assert_eq};
use types_error::PgResult;

pub fn start_func<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    _index: u32,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    machine.with_target(|target| target.x64_mut().preserve_flags = false);
    Ok(())
}

pub fn prologue_begin<
    'm,
    A: CompilerAdaptor,
    M: TargetMachine<'m, A, 2>,
    C: CallingConventionAssigner,
>(
    machine: &M,
    assigner: &mut C,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let start = u32::try_from(machine.with_output(|output| output.offset()))
        .map_err(|_| invalid("X64 function offset overflow"))?;
    let csr = assigner.get_ccinfo().callee_saved_regs();
    tpde_assert_eq!(
        csr & !tpde_core::RegisterFile::<2, 32>::bank_regs(RegBank::new(0)),
        0,
        "non-GP callee-saved registers not implemented"
    );
    let low = (csr & 255).count_ones();
    let high = ((csr >> 8) & 255).count_ones();
    let reserved = low + 2 * high + 11;
    machine.set_frame_size((low + high) * 8);
    machine.with_target(|target| {
        target.clear_returns();
        let state = target.x64_mut();
        state.func_start_off = start;
        state.callee_saved_regs = csr;
        state.func_prologue_alloc = reserved;
        state.scalar_arg_count = u32::MAX;
        state.vec_arg_count = u32::MAX;
        state.max_callee_stack_arg_size = 0;
    });
    machine.with_output(|output| output.skip(reserved as usize))?;
    if machine.call_adaptor().cur_is_vararg() {
        machine.set_frame_used(true);
        let frame = machine
            .frame_size()
            .checked_add(6 * 8 + 8 * 16)
            .ok_or_else(|| invalid("X64 frame size overflow"))?;
        machine.set_frame_size(frame);
        machine.with_target(|target| target.x64_mut().reg_save_frame_off = frame);
        let mut offset =
            -(i32::try_from(frame).map_err(|_| invalid("X64 frame offset overflow"))?);
        for reg in [FE_DI, FE_SI, FE_DX, FE_CX, FE_R8, FE_R9] {
            let memory = FE_MEM(FE_BP, 0, FE_NOREG, offset).map_err(encoding_error)?;
            emit(machine, |out| fe64_MOV64mr(out, 0, memory, reg))?;
            offset += 8;
        }
        let skip = machine.with_output(|output| output.label_create())?;
        emit(machine, |out| fe64_TEST8rr(out, 0, FE_AX, FE_AX))?;
        crate::emission::generate_raw_jump(machine, crate::Jump::je, skip)?;
        for reg in 32..40 {
            let memory = FE_MEM(FE_BP, 0, FE_NOREG, offset).map_err(encoding_error)?;
            emit(machine, |out| {
                fe64_SSE_MOVDQUmr(out, 0, memory, xmm(Reg::new(reg)))
            })?;
            offset += 16;
        }
        machine.with_output(|output| output.label_place(skip))?;
    }
    Ok(())
}

pub fn prologue_assign_arg_part<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    value: &mut ValuePartRef<'_, M, 2>,
    assignment: CCAssignment,
) -> PgResult<Option<i32>>
where
    M::Target: X64CompilerState,
{
    if assignment.reg.valid() {
        value.set_value_reg(assignment.reg)?;
        machine.with_registers(|registers| {
            registers
                .allocatable
                .set(registers.allocatable.get() | 1u64 << assignment.reg.id())
        });
        return Ok(None);
    }
    if value.is_owned() {
        return Ok(None);
    }
    machine.set_frame_used(true);
    let offset = 0x10u32
        .checked_add(assignment.stack_off)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| invalid("X64 incoming argument offset overflow"))?;
    if assignment.byval() {
        return Ok(Some(offset));
    }
    let reuse =
        value.with_assignment(|part| part.assignment().part_count() == 1 && !part.register_valid());
    if reuse {
        value.with_assignment(|part| {
            part.set_stack_valid();
            part.assignment().set_frame_off(offset);
        });
    } else {
        let destination = value.alloc_reg()?;
        crate::emission::load_from_stack(machine, destination, offset, assignment.size, false)?;
    }
    Ok(None)
}

pub fn prologue_end<
    'm,
    A: CompilerAdaptor,
    M: TargetMachine<'m, A, 2>,
    C: CallingConventionAssigner,
>(
    machine: &M,
    assigner: &mut C,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    if machine.call_adaptor().cur_is_vararg() {
        let args = assigner.get_ccinfo().arg_regs();
        let (gp_count, xmm_count) = machine.with_registers(|registers| {
            let live = registers.allocatable.get() & args;
            (
                (live & tpde_core::RegisterFile::<2, 32>::bank_regs(RegBank::new(0))).count_ones(),
                (live & tpde_core::RegisterFile::<2, 32>::bank_regs(RegBank::new(1))).count_ones(),
            )
        });
        let stack = assigner
            .get_stack_size()
            .checked_add(0x10)
            .ok_or_else(|| invalid("X64 vararg stack offset overflow"))?;
        machine.with_target(|target| {
            let state = target.x64_mut();
            state.scalar_arg_count = gp_count;
            state.vec_arg_count = xmm_count;
            state.var_arg_stack_off = stack;
        });
    }
    Ok(())
}

pub fn gen_func_epilog<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let offset = u32::try_from(machine.with_output(|output| output.offset()))
        .map_err(|_| invalid("X64 return offset overflow"))?;
    machine.with_target(|target| target.push_return(offset))?;
    machine.with_output(|output| output.skip(5))
}

pub fn finish_func<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    index: u32,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let cc = machine.with_cur_cc_assigner(|assigner| *assigner.get_ccinfo());
    let saved =
        machine.with_registers(|registers| registers.clobbered.get()) & cc.callee_saved_regs();
    let needs_frame = machine.frame_used()
        || machine.frame_generated_call()
        || machine.frame_has_dynamic_alloca()
        || saved != 0;
    let state = machine.with_target(|target| *target.x64());
    let had_returns = machine.with_target(|target| !target.return_offsets().is_empty());
    let personality = machine.personality_symbol()?;
    machine.with_output(|output| output.with_base(|base| base.eh_begin_fde(personality)))?;
    let mut prologue_size = 0usize;
    let mut saved_count = 0u32;
    let mut adjustment = 0u32;
    if needs_frame {
        let cfi_advance = machine.with_output(|output| {
            output.with_base(|base| -> PgResult<usize> {
                if had_returns {
                    base.eh_write_inst(dwarf::DW_CFA_remember_state)?;
                }
                base.eh_write_inst1(dwarf::DW_CFA_advance_loc, 1)?;
                base.eh_write_inst1(dwarf::DW_CFA_def_cfa_offset, 16)?;
                base.eh_write_inst2(dwarf::DW_CFA_offset, u64::from(dwarf::x64::DW_reg_rbp), 2)?;
                base.eh_write_inst1(dwarf::DW_CFA_advance_loc, 3)?;
                base.eh_write_inst1(
                    dwarf::DW_CFA_def_cfa_register,
                    u64::from(dwarf::x64::DW_reg_rbp),
                )?;
                let position = base.eh_size();
                if saved != 0 {
                    base.eh_write_inst1(dwarf::DW_CFA_advance_loc, 0)?;
                }
                Ok(position)
            })
        })?;
        let mut bytes = [0u8; 64];
        let mut output = Output::new(&mut bytes, state.func_start_off as usize);
        fe64_PUSHr(&mut output, 0, FE_BP).map_err(encoding_error)?;
        fe64_MOV64rr(&mut output, 0, FE_BP, FE_SP).map_err(encoding_error)?;
        const DWARF: [u8; 16] = [0, 2, 1, 3, 7, 6, 4, 5, 8, 9, 10, 11, 12, 13, 14, 15];
        let mut remaining = saved;
        while remaining != 0 {
            let reg = remaining.trailing_zeros();
            remaining &= remaining - 1;
            tpde_assert!(reg <= 15);
            fe64_PUSHr(&mut output, 0, gp(Reg::new(u64::from(reg)))).map_err(encoding_error)?;
            saved_count += 1;
            machine.with_output(|writer| {
                writer.with_base(|base| {
                    base.eh_write_inst2(
                        dwarf::DW_CFA_offset,
                        u64::from(DWARF[reg as usize]),
                        u64::from(saved_count + 2),
                    )
                })
            })?;
        }
        tpde_assert!(!machine.frame_has_dynamic_alloca() || state.max_callee_stack_arg_size == 0);
        let frame = machine
            .frame_size()
            .checked_add(state.max_callee_stack_arg_size)
            .and_then(|n| n.checked_add(15))
            .ok_or_else(|| invalid("X64 final frame size overflow"))?
            & !15;
        adjustment = frame
            .checked_sub(saved_count * 8)
            .ok_or_else(|| invalid("X64 saved-register frame underflow"))?;
        if machine.frame_generated_call()
            || machine.frame_has_dynamic_alloca()
            || adjustment > u32::from(cc.red_zone_size())
        {
            fe64_SUB64ri(
                &mut output,
                0,
                FE_SP,
                i32::try_from(adjustment)
                    .map_err(|_| invalid("X64 frame adjustment exceeds signed immediate"))?,
            )
            .map_err(encoding_error)?;
        } else {
            adjustment = 0;
        }
        prologue_size = output.len();
        tpde_assert!(prologue_size <= state.func_prologue_alloc as usize);
        machine.with_output(|writer| {
            writer.with_base(|base| -> PgResult<()> {
                base.data_mut()?
                    [state.func_start_off as usize..state.func_start_off as usize + prologue_size]
                    .copy_from_slice(output.bytes());
                if saved != 0 {
                    tpde_assert!(prologue_size < 0x44);
                    base.eh_data_mut()?[cfi_advance] =
                        dwarf::DW_CFA_advance_loc | (prologue_size as u8 - 4);
                }
                Ok(())
            })
        })?;
    }
    if had_returns {
        let offset = machine.with_output(|writer| writer.offset());
        let last = machine.with_target(|target| target.return_offsets().last().copied());
        if last.map(|value| value as usize + 5) == Some(offset) {
            machine.with_output(|writer| writer.with_base(|base| base.set_offset(offset - 5)))?;
            machine.with_target(|target| {
                target.pop_return();
            });
        }
        let epilogue = machine.with_output(|writer| writer.offset());
        let count = machine.with_target(|target| target.return_offsets().len());
        for i in 0..count {
            let position = machine.with_target(|target| target.return_offsets()[i]) as usize;
            let mut bytes = [0u8; 5];
            let mut output = Output::new(&mut bytes, position);
            fe64_JMP(&mut output, FE_JMPL, epilogue).map_err(encoding_error)?;
            machine.with_output(|writer| {
                writer.with_base(|base| -> PgResult<()> {
                    base.data_mut()?[position..position + 5].copy_from_slice(output.bytes());
                    Ok(())
                })
            })?;
        }
        machine.with_output(|writer| writer.ensure_space(prologue_size + 1))?;
        if needs_frame {
            if machine.frame_has_dynamic_alloca() {
                if saved_count == 0 {
                    emit(machine, |out| fe64_MOV64rr(out, 0, FE_SP, FE_BP))?;
                } else {
                    let memory = FE_MEM(FE_BP, 0, FE_NOREG, -(saved_count as i32 * 8))
                        .map_err(encoding_error)?;
                    emit(machine, |out| fe64_LEA64rm(out, 0, FE_SP, memory))?;
                }
            } else if adjustment != 0 {
                emit(machine, |out| {
                    fe64_ADD64ri(out, 0, FE_SP, adjustment as i32)
                })?;
            }
            tpde_assert!(u32::from(cc.red_zone_size()) >= saved_count * 8);
            let mut remaining = saved;
            while remaining != 0 {
                let reg = 63 - remaining.leading_zeros();
                remaining &= !(1u64 << reg);
                emit(machine, |out| {
                    fe64_POPr(out, 0, gp(Reg::new(u64::from(reg))))
                })?;
            }
            emit(machine, |out| fe64_POPr(out, 0, FE_BP))?;
            let advance = machine.with_output(|writer| writer.offset())
                - (state.func_start_off + state.func_prologue_alloc) as usize;
            machine.with_output(|writer| {
                writer.eh_advance(advance as u64)?;
                writer.with_base(|base| base.eh_write_inst(dwarf::DW_CFA_restore_state))
            })?;
        }
        emit(machine, |out| fe64_RET(out, 0))?;
    }
    let symbol = machine.function_symbol(index);
    machine.with_output(|writer| {
        writer.with_base(|base| {
            base.remove_prologue_bytes(
                state.func_start_off + prologue_size as u32,
                state.func_prologue_alloc - prologue_size as u32,
            )
        })?;
        let size = writer.offset() - state.func_start_off as usize;
        let section = writer.section();
        writer.assembler_mut().sym_def(
            symbol,
            section,
            u64::from(state.func_start_off),
            size as u64,
        )?;
        writer.with_base(|base| {
            base.eh_end_fde()?;
            base.except_encode_func()
        })
    })
}

pub fn select_fixed_assignment_reg<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    local: ValLocalIdx,
    part: u32,
) -> Reg
where
    M::Target: X64CompilerState,
{
    let bank = machine.with_part(local, part, |part| part.bank());
    tpde_assert!(bank.id() < 2);
    let unavailable =
        machine.with_target(|target| target.x64().fixed_assignment_nonallocatable_mask);
    let csr = machine.with_target(|target| target.x64().callee_saved_regs);
    // See the A64 counterpart: callee-saved only when the value's range
    // crosses a call. Cold calls count as hot here: the cold-call
    // save/restore is not enabled on x86-64 (`target.rs` `pin_reg_mask`).
    let live = machine.liveness(local);
    let crosses = live.crosses_call || live.crosses_cold_call;
    let possible = machine.with_registers(|registers| {
        let free = registers.allocatable.get()
            & !registers.used.get()
            & tpde_core::RegisterFile::<2, 32>::bank_regs(bank)
            & !unavailable;
        if !machine.call_is_leaf_function() && crosses {
            free & csr
        } else if free & !csr != 0 {
            free & !csr
        } else {
            free & csr
        }
    });
    if possible == 0 {
        return Reg::make_invalid();
    }
    let free = machine.with_registers(|registers| possible & !registers.used.get());
    if free != 0 {
        return Reg::new(u64::from(free.trailing_zeros()));
    }
    let mut remaining = possible;
    while remaining != 0 {
        let reg = Reg::new(u64::from(remaining.trailing_zeros()));
        remaining &= remaining - 1;
        let assignment = machine.with_registers(|registers| {
            if registers.is_fixed(reg) {
                None
            } else {
                Some((registers.reg_local_idx(reg), registers.reg_part(reg)))
            }
        });
        if let Some((local, part)) = assignment {
            if local != INVALID_VALUE && !machine.with_part(local, part, |part| part.modified()) {
                return reg;
            }
        }
    }
    Reg::make_invalid()
}
