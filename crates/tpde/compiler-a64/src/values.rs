// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{raw, A64CompilerState, FP_BANK, GP_BANK};
use tpde_base::ValLocalIdx;
use tpde_compiler::{
    generic_value_part::{Expr, ExprReg},
    target::TargetMachine,
    CompilerAdaptor, GenericValuePart, ScratchReg, ValuePartRef,
};
use tpde_core::{Reg, RegisterFile};
use tpde_disarm::*;
use tpde_util::tpde_assert;
use types_error::PgResult;

pub fn val_spill_slot<'s, 'm, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &'s M,
    local: ValLocalIdx,
    part: u32,
) -> GenericValuePart<'s, M, 2>
where
    M::Target: A64CompilerState,
{
    let offset = machine.with_part(local, part, |ap| {
        tpde_assert!(ap.stack_valid() && !ap.variable_ref());
        ap.frame_off()
    });
    GenericValuePart::Expr(Expr::from_reg(Reg::new(29), i64::from(offset)))
}
pub fn spill_reg<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    reg: Reg,
    frame_off: i32,
    size: u32,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    crate::ensure(
        machine.frame_used()
            && size.is_power_of_two()
            && frame_off >= 0
            && (frame_off as u32) & (size - 1) == 0,
        "invalid A64 spill frame slot",
    )?;
    raw::store_off(machine, Reg::new(29), frame_off as u32, reg, size)
}
pub fn load_from_stack<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    dst: Reg,
    frame_off: i32,
    size: u32,
    sign: bool,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    crate::ensure(
        machine.frame_used()
            && size.is_power_of_two()
            && frame_off >= 0
            && (frame_off as u32) & (size - 1) == 0,
        "invalid A64 reload frame slot",
    )?;
    raw::load_off(machine, dst, Reg::new(29), frame_off as u32, size, sign)
}
pub fn load_address_of_stack_var<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    dst: Reg,
    local: ValLocalIdx,
    part: u32,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    let off = machine.with_part(local, part, |ap| ap.variable_stack_off());
    crate::ensure(
        machine.frame_used() && off >= 0,
        "invalid A64 stack variable",
    )?;
    let word = de64_ADDxi(raw::sp(dst)?, raw::sp(Reg::new(29))?, i64::from(off));
    if word != 0 {
        raw::emit(machine, word)
    } else {
        raw::materialize_constant(machine, &[off as u64], GP_BANK, 4, dst)?;
        raw::emit(
            machine,
            de64_ADDx_uxtw(raw::sp(dst)?, raw::sp(Reg::new(29))?, raw::zr(dst)?, 0),
        )
    }
}
pub fn gval_expr_as_reg<'s, 'm, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &'s M,
    value: &mut GenericValuePart<'s, M, 2>,
) -> PgResult<Reg>
where
    M::Target: A64CompilerState,
{
    let GenericValuePart::Expr(expr) = value else {
        return Err(crate::invalid("A64 expression value required"));
    };
    let mut scratch = ScratchReg::new(machine);
    let temp = machine.with_target(|target| target.a64_state().permanent_scratch_reg);
    match (expr.has_base(), expr.has_index()) {
        (false, false) => {
            let dst = scratch.alloc_gp()?;
            raw::materialize_constant(machine, &[expr.disp as u64], GP_BANK, 8, dst)?;
            expr.disp = 0;
        }
        (false, true) => {
            let index = expr.index_reg();
            if let ExprReg::Scratch(old) = &mut expr.index {
                scratch = old.take();
            } else {
                scratch.alloc_gp()?;
            }
            let dst = scratch.cur_reg();
            let scale = expr.scale as u64;
            if scale.is_power_of_two() {
                raw::emit(
                    machine,
                    de64_LSLxi(raw::zr(dst)?, raw::zr(index)?, scale.trailing_zeros()),
                )?;
            } else {
                raw::materialize_constant(machine, &[scale], GP_BANK, 8, temp)?;
                raw::emit(
                    machine,
                    de64_MULx(raw::zr(dst)?, raw::zr(index)?, raw::zr(temp)?),
                )?;
            }
        }
        (true, true) => {
            let base = expr.base_reg();
            let index = expr.index_reg();
            if let ExprReg::Scratch(old) = &mut expr.base {
                scratch = old.take();
            } else if let ExprReg::Scratch(old) = &mut expr.index {
                scratch = old.take();
            } else {
                scratch.alloc_gp()?;
            }
            let dst = scratch.cur_reg();
            let scale = expr.scale as u64;
            if scale.is_power_of_two() {
                raw::emit(
                    machine,
                    de64_ADDx_lsl(
                        raw::zr(dst)?,
                        raw::zr(base)?,
                        raw::zr(index)?,
                        scale.trailing_zeros(),
                    ),
                )?;
            } else {
                raw::materialize_constant(machine, &[scale], GP_BANK, 8, temp)?;
                raw::emit(
                    machine,
                    de64_MADDx(
                        raw::zr(dst)?,
                        raw::zr(index)?,
                        raw::zr(temp)?,
                        raw::zr(base)?,
                    ),
                )?;
            }
        }
        (true, false) => {
            let base = expr.base_reg();
            if let ExprReg::Scratch(old) = &mut expr.base {
                scratch = old.take();
            } else {
                scratch.alloc_gp()?;
            }
            let dst = scratch.cur_reg();
            let word = if expr.disp != 0 {
                de64_ADDxi(raw::sp(dst)?, raw::sp(base)?, expr.disp)
            } else {
                0
            };
            if word != 0 {
                raw::emit(machine, word)?;
                expr.disp = 0;
            } else if dst != base {
                raw::emit(machine, de64_MOVx(raw::zr(dst)?, raw::zr(base)?))?;
            }
        }
    }
    let dst = scratch.cur_reg();
    if expr.disp != 0 {
        let word = de64_ADDxi(raw::sp(dst)?, raw::sp(dst)?, expr.disp);
        if word != 0 {
            raw::emit(machine, word)?;
        } else {
            raw::materialize_constant(machine, &[expr.disp as u64], GP_BANK, 8, temp)?;
            raw::emit(
                machine,
                de64_ADDx(raw::zr(dst)?, raw::zr(dst)?, raw::zr(temp)?),
            )?;
        }
    }
    *value = GenericValuePart::Scratch(scratch);
    Ok(dst)
}
pub fn alloca_fixed<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    size: u64,
    align: u32,
    mut result: ValuePartRef<'_, M, 2>,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    crate::ensure(
        machine.frame_has_dynamic_alloca() && align.is_power_of_two(),
        "invalid A64 dynamic frame allocation",
    )?;
    let size = size
        .checked_add(15)
        .ok_or_else(|| crate::invalid("A64 alloca size overflow"))?
        & !15;
    let dst = result.alloc_reg()?;
    if size >= 0x10_0000 {
        let tmp = machine.with_target(|target| target.a64_state().permanent_scratch_reg);
        raw::materialize_constant(machine, &[size], GP_BANK, 8, tmp)?;
        raw::emit(
            machine,
            de64_SUBx_uxtx(raw::sp(dst)?, DA_SP, raw::zr(tmp)?, 0),
        )?;
    } else if size >= 0x1000 {
        raw::emit(
            machine,
            de64_SUBxi(raw::sp(dst)?, DA_SP, (size & 0xff_f000) as i64),
        )?;
        if size & 0xfff != 0 {
            raw::emit(
                machine,
                de64_SUBxi(raw::sp(dst)?, raw::sp(dst)?, (size & 0xfff) as i64),
            )?;
        }
    } else {
        raw::emit(
            machine,
            de64_SUBxi(raw::sp(dst)?, DA_SP, (size & 0xfff) as i64),
        )?;
    }
    if align > 16 {
        raw::emit(
            machine,
            de64_ANDxi(raw::sp(dst)?, raw::zr(dst)?, !(u64::from(align) - 1)),
        )?;
    }
    if size > 0 {
        raw::emit(machine, de64_MOV_SPx(DA_SP, raw::sp(dst)?))?;
    }
    Ok(())
}
pub fn alloca_dynamic<'s, 'm, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &'s M,
    elem_size: u64,
    mut count: ValuePartRef<'s, M, 2>,
    align: u32,
    mut result: ValuePartRef<'s, M, 2>,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    crate::ensure(
        machine.frame_has_dynamic_alloca() && align.is_power_of_two(),
        "invalid A64 variable frame allocation",
    )?;
    let size_reg = count.cur_reg_or_load()?;
    let dst = result.alloc_try_reuse(&mut count)?;
    if elem_size == 0 {
        // Upstream writes zero into SP for zero-sized elements; preserve the stack instead.
        raw::emit(machine, de64_MOV_SPx(raw::sp(dst)?, DA_SP))?;
        if align > 16 {
            raw::emit(
                machine,
                de64_ANDxi(raw::sp(dst)?, raw::zr(dst)?, !(u64::from(align) - 1)),
            )?;
        }
        return Ok(());
    } else if elem_size.is_power_of_two() {
        let shift = elem_size.trailing_zeros();
        if shift <= 4 {
            raw::emit(
                machine,
                de64_SUBx_uxtx(raw::sp(dst)?, DA_SP, raw::zr(size_reg)?, shift),
            )?;
        } else {
            raw::emit(
                machine,
                de64_LSLxi(raw::zr(dst)?, raw::zr(size_reg)?, shift),
            )?;
            raw::emit(
                machine,
                de64_SUBx_uxtx(raw::sp(dst)?, DA_SP, raw::zr(dst)?, 0),
            )?;
        }
    } else {
        let tmp = machine.with_target(|target| target.a64_state().permanent_scratch_reg);
        raw::materialize_constant(machine, &[elem_size], GP_BANK, 8, tmp)?;
        raw::emit(
            machine,
            de64_MULx(raw::zr(dst)?, raw::zr(size_reg)?, raw::zr(tmp)?),
        )?;
        raw::emit(
            machine,
            de64_SUBx_uxtx(raw::sp(dst)?, DA_SP, raw::zr(dst)?, 0),
        )?;
    }
    let align = align.max(16);
    if align > 16 || elem_size & u64::from(align - 1) != 0 {
        raw::emit(
            machine,
            de64_ANDxi(raw::sp(dst)?, raw::zr(dst)?, !(u64::from(align) - 1)),
        )?;
    }
    raw::emit(machine, de64_MOV_SPx(DA_SP, raw::sp(dst)?))
}
pub fn select_fixed_assignment_reg<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    local: ValLocalIdx,
    part: u32,
    _value: A::ValueRef,
) -> Reg
where
    M::Target: A64CompilerState,
{
    let (bank, size) = machine.with_part(local, part, |ap| (ap.bank(), ap.part_size()));
    let live = machine.liveness(local);
    // A 16-byte vector is fixed only while it crosses no call: the
    // callee-saved vector registers keep their low eight bytes only.
    if bank == FP_BANK && size > 8 && (live.crosses_call || live.crosses_cold_call) {
        return Reg::make_invalid();
    }
    tpde_assert!(bank.id() < 2);
    let excluded =
        machine.with_target(|target| target.a64_state().fixed_assignment_nonallocatable_mask);
    let mask = RegisterFile::<2, 32>::bank_regs(bank) & !excluded;
    let saved = machine.with_target(|target| target.a64_state().callee_saved_regs);
    let free = machine.with_registers(|regs| regs.allocatable.get() & !regs.used.get()) & mask;
    // A value whose live range crosses a hot call must sit in a callee-saved
    // register; one that crosses only cold calls may take a caller-saved
    // register that no call argument or result uses (the call sequence
    // saves and restores it, `calls.rs`); one that crosses none may take
    // any (the analyzer's `crosses_call` covers every block a call between
    // the definition and a use can be in). Upstream: callee-saved for every
    // non-leaf function.
    let leaf = machine.call_is_leaf_function();
    let possible = if !leaf && live.crosses_call {
        // Callee-saved while one is free; else caller-saved and not an
        // argument register, saved around each call (`target_guard.rs`
        // `pin_reg_mask`, the same rule).
        let args = crate::abi::AAPCS_INFO.arg_regs();
        let saved_free = free & saved;
        if saved_free != 0 {
            saved_free
        } else if free & !saved & !args != 0 {
            free & !saved & !args
        } else if live.first.0 > 0 {
            // An argument register but the return pair, parked around
            // each call (`calls.rs`); not for an entry-block value, whose
            // register may still hold an incoming argument.
            free & args & !0b11
        } else {
            0
        }
    } else {
        let mut caller = free & !saved;
        if !leaf && live.crosses_cold_call {
            caller &= !crate::abi::AAPCS_INFO.arg_regs();
        }
        if caller != 0 {
            caller
        } else {
            free & saved
        }
    };
    if possible == 0 {
        return Reg::make_invalid();
    }
    Reg::new(u64::from(possible.trailing_zeros()))
}
