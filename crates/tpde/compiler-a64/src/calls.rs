// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{
    abi::{CCAssignerAAPCS, AAPCS_INFO},
    raw, A64CompilerState,
};
use core::marker::PhantomData;
use tpde_assembler::{elf, SymRef};
use tpde_compiler::{
    calls::{CallArg, CallBuilderBase, CallBuilderHooks, CallTarget},
    target::{CompilerOutput, TargetMachine},
    CompilerAdaptor, ScratchReg, ValuePartRef, ValueRef,
};
use tpde_core::{
    calling_convention::{CCAssigner, CCAssignment, CallingConventionAssigner},
    Reg, RegBank, RegisterFile,
};
use tpde_compiler::RegallocEvent;
use tpde_util::{misc::BitSetIterator, tpde_assert, tpde_assert_eq};
use types_error::PgResult;

/// Fixed (classic or pinned) values sitting in argument registers, parked
/// for the call sequence: `park_argument_registers` stores each to its slot
/// and releases the register (the value's part keeps its register and
/// `fixed` state; the register file forgets it), so the argument moves and
/// the result placement find the register free; `unpark_argument_registers`
/// takes the register back and reloads it. The target's masks admit
/// x2–x8 for a value that crosses calls only when the callee-saved and the
/// other caller-saved registers are all taken (`values.rs`,
/// `target_guard.rs`), so this runs for the loops that need more than
/// nineteen registers across a host call.
struct Parked {
    entries: [(Reg, tpde_base::ValLocalIdx, u32, bool); 9],
    count: usize,
}

fn park_argument_registers<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(compiler: &M) -> PgResult<Parked>
where
    M::Target: A64CompilerState,
{
    let mut parked = Parked { entries: [(Reg::make_invalid(), tpde_base::ValLocalIdx::new(0), 0, false); 9], count: 0 };
    let candidates = compiler.with_registers(|regs| regs.used.get())
        & AAPCS_INFO.arg_regs()
        & RegisterFile::<2, 32>::bank_regs(RegBank::new(0));
    for id in BitSetIterator::<false>::new(candidates) {
        let reg = Reg::new(id);
        let (fixed, local, part, locks) = compiler.with_registers(|regs| {
            (regs.is_fixed(reg), regs.reg_local_idx(reg), regs.reg_part(reg), regs.reg_lock_count(reg))
        });
        if !fixed || local == tpde_compiler::guard_machine::INVALID_VALUE {
            continue;
        }
        crate::ensure(locks == 1, "A64 call: a fixed argument register holds a locked reference")?;
        // On the stack for the call sequence: an argument that is this
        // value reloads from the slot into its argument register.
        let valid = compiler.with_part(local, part, |ap| ap.register_valid());
        if valid {
            compiler.call_spill_part(local, part)?;
            compiler.with_part(local, part, |ap| ap.set_register_valid(false));
        }
        compiler.with_registers(|regs| {
            regs.dec_lock_count_must_zero(reg, 1);
            regs.unmark_used(reg);
        });
        parked.entries[parked.count] = (reg, local, part, valid);
        parked.count += 1;
    }
    Ok(parked)
}

fn unpark_argument_registers<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(compiler: &M, parked: &Parked) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    for &(reg, local, part, valid) in &parked.entries[..parked.count] {
        // The value died in the call sequence (its last reference was an
        // argument): its assignment is gone, the register is free.
        if !compiler.assignment_exists(local) {
            continue;
        }
        if compiler.with_registers(|regs| regs.is_used(reg)) {
            compiler.evict_reg(reg)?;
        }
        compiler.with_registers(|regs| {
            regs.mark_used(reg, local, part);
            regs.inc_lock_count(reg);
            regs.mark_clobbered(reg);
        });
        if valid {
            compiler.reload_to_reg(reg, local, part)?;
            compiler.with_part(local, part, |ap| ap.set_register_valid(true));
            compiler.regalloc_event(RegallocEvent::CallRestore);
        }
    }
    Ok(())
}

pub struct CallBuilder<'m, A, M> {
    stack_adjust_off: Option<u32>,
    marker: PhantomData<(&'m (), fn() -> A, fn() -> M)>,
}
impl<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>> CallBuilder<'m, A, M>
where
    M::Target: A64CompilerState,
{
    pub fn new() -> Self {
        Self {
            stack_adjust_off: None,
            marker: PhantomData,
        }
    }
    fn set_stack_used(&mut self, compiler: &M) -> PgResult<()> {
        if self.stack_adjust_off.is_none() {
            compiler.with_output(|out| {
                out.ensure_space(16)?;
                self.stack_adjust_off = Some(
                    u32::try_from(out.offset())
                        .map_err(|_| crate::invalid("A64 call offset exceeds u32"))?,
                );
                out.skip(4)
            })?;
        }
        Ok(())
    }
}
impl<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>> Default for CallBuilder<'m, A, M>
where
    M::Target: A64CompilerState,
{
    fn default() -> Self {
        Self::new()
    }
}
impl<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>> CallBuilderHooks<M, 2>
    for CallBuilder<'m, A, M>
where
    M::Target: A64CompilerState,
{
    fn add_arg_byval<C: CallingConventionAssigner>(
        &mut self,
        compiler: &M,
        _: &mut C,
        value: &mut ValuePartRef<'_, M, 2>,
        assignment: &mut CCAssignment,
    ) -> PgResult<()> {
        let ptr = value.load_to_reg()?;
        let mut scratch = ScratchReg::new(compiler);
        let tmp = scratch.alloc_gp()?;
        self.set_stack_used(compiler)?;
        let mut off = 0;
        for width in [8, 4, 2, 1] {
            while assignment.size - off >= width {
                raw::load_off(compiler, tmp, ptr, off, width, false)?;
                raw::store_off(
                    compiler,
                    Reg::new(31),
                    assignment
                        .stack_off
                        .checked_add(off)
                        .ok_or_else(|| crate::invalid("A64 byval offset overflow"))?,
                    tmp,
                    width,
                )?;
                off += width;
            }
        }
        Ok(())
    }
    fn add_arg_stack<C: CallingConventionAssigner>(
        &mut self,
        compiler: &M,
        _: &mut C,
        value: &mut ValuePartRef<'_, M, 2>,
        assignment: &mut CCAssignment,
    ) -> PgResult<()> {
        self.set_stack_used(compiler)?;
        let reg = value.cur_reg_or_load()?;
        raw::store_off(
            compiler,
            Reg::new(31),
            assignment.stack_off,
            reg,
            assignment.size,
        )
    }
    fn call_impl<C: CallingConventionAssigner>(
        &mut self,
        compiler: &M,
        assigner: &mut C,
        target: CallTarget<'_, M, 2>,
    ) -> PgResult<()> {
        let sub = if let Some(off) = self.stack_adjust_off {
            let size = assigner.get_stack_size();
            let alignment = if size < 0x1000 { 16 } else { 4096 };
            let sub = size
                .checked_add(alignment - 1)
                .ok_or_else(|| crate::invalid("A64 call stack size overflow"))?
                & !(alignment - 1);
            let word =
                tpde_disarm::de64_SUBxi(tpde_disarm::DA_SP, tpde_disarm::DA_SP, i64::from(sub));
            crate::ensure(word != 0, "A64 call stack adjustment cannot be encoded")?;
            compiler.with_output(|out| {
                out.with_base(|base| {
                    base.data_mut()?
                        .get_mut(off as usize..off as usize + 4)
                        .ok_or_else(|| crate::invalid("A64 call stack patch outside text"))?
                        .copy_from_slice(&word.to_le_bytes());
                    Ok::<_, alloc::boxed::Box<types_error::PgError>>(())
                })
            })?;
            sub
        } else {
            tpde_assert_eq!(assigner.get_stack_size(), 0);
            0
        };
        let mut saved_fp = compiler.with_registers(|regs| {
            regs.used.get() & RegisterFile::<2, 32>::bank_regs(RegBank::new(1))
        }) & assigner.get_ccinfo().callee_saved_regs();
        while saved_fp != 0 {
            let reg = Reg::new(u64::from(saved_fp.trailing_zeros()));
            saved_fp &= saved_fp - 1;
            let (local, part) =
                compiler.with_registers(|regs| (regs.reg_local_idx(reg), regs.reg_part(reg)));
            if compiler.with_part(local, part, |ap| ap.part_size() > 8) {
                compiler.evict_part(local, part)?;
            }
        }
        match target {
            CallTarget::Symbol(sym) => {
                raw::emit(compiler, tpde_disarm::de64_BL(0))?;
                let off = compiler.with_output(|out| out.offset());
                compiler.reloc_text(sym, elf::R_AARCH64_CALL26, off as u64 - 4, 0)?;
            }
            CallTarget::Value(mut value) => {
                let reg = if value.can_salvage() {
                    value.salvage()
                } else {
                    let reg =
                        compiler.with_target(|target| target.a64_state().permanent_scratch_reg);
                    value.reload_into_specific_fixed(reg, 0)?;
                    reg
                };
                raw::emit(compiler, tpde_disarm::de64_BLR(raw::gp(reg)?.into()))?;
                value.reset();
            }
        }
        if self.stack_adjust_off.is_some() {
            raw::emit(
                compiler,
                tpde_disarm::de64_ADDxi(tpde_disarm::DA_SP, tpde_disarm::DA_SP, i64::from(sub)),
            )?;
        }
        Ok(())
    }
}
pub fn generate_call<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    compiler: &M,
    target: CallTarget<'_, M, 2>,
    arguments: &[CallArg<A::ValueRef>],
    result: Option<&mut ValueRef<'_, M, 2>>,
    _variable_args: bool,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    generate_call_iter(
        compiler,
        target,
        arguments.iter().copied(),
        result,
        _variable_args,
    )
}
pub fn generate_call_iter<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    compiler: &M,
    target: CallTarget<'_, M, 2>,
    arguments: impl IntoIterator<Item = CallArg<A::ValueRef>>,
    result: Option<&mut ValueRef<'_, M, 2>>,
    _variable_args: bool,
) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    let parked = park_argument_registers(compiler)?;
    let mut assigner = CCAssigner::new(&AAPCS_INFO, CCAssignerAAPCS::default());
    let mut hooks = CallBuilder::<'m, A, M>::new();
    let mut builder = CallBuilderBase::new(compiler, &mut assigner, &mut hooks);
    for arg in arguments {
        builder.add_arg(&arg)?;
    }
    builder.call(target)?;
    unpark_argument_registers(compiler, &parked)?;
    if let Some(result) = result {
        builder.add_ret(result)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TLSModel {
    GlobalDynamic,
    LocalDynamic,
    InitialExec,
    LocalExec,
}
pub fn tls_get_addr<'s, 'm, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    compiler: &'s M,
    sym: SymRef,
    _model: TLSModel,
) -> PgResult<ScratchReg<'s, M, 2>>
where
    M::Target: A64CompilerState,
{
    use tpde_disarm::*;
    tpde_assert!(!compiler.call_is_leaf_function());
    compiler.call_set_generated_call();
    let mut r0 = ScratchReg::new(compiler);
    r0.alloc_specific(Reg::new(0))?;
    let mut r1 = ScratchReg::new(compiler);
    r1.alloc_specific(Reg::new(1))?;
    if compiler.with_registers(|regs| regs.is_used(Reg::new(30))) {
        compiler.evict_reg(Reg::new(30))?;
    }
    compiler.with_output(|out| out.ensure_space(0x18))?;
    let operations = [
        (
            elf::R_AARCH64_TLSDESC_ADR_PAGE21,
            de64_ADRP(raw::gp(Reg::new(0))?.into(), 0, 0),
        ),
        (
            elf::R_AARCH64_TLSDESC_LD64_LO12,
            de64_LDRxu(
                raw::gp(Reg::new(1))?.into(),
                raw::gp(Reg::new(0))?.into(),
                0,
            ),
        ),
        (
            elf::R_AARCH64_TLSDESC_ADD_LO12,
            de64_ADDxi(
                raw::gp(Reg::new(0))?.into(),
                raw::gp(Reg::new(0))?.into(),
                0,
            ),
        ),
        (
            elf::R_AARCH64_TLSDESC_CALL,
            de64_BLR(raw::gp(Reg::new(1))?.into()),
        ),
    ];
    for (kind, word) in operations {
        let off = compiler.with_output(|out| out.offset());
        compiler.reloc_text(sym, kind, off as u64, 0)?;
        raw::emit(compiler, word)?;
    }
    raw::emit(compiler, de64_MRS(raw::gp(Reg::new(1))?.into(), 0xde82))?;
    raw::emit(
        compiler,
        de64_ADDx(
            raw::gp(Reg::new(0))?.into(),
            raw::gp(Reg::new(1))?.into(),
            raw::gp(Reg::new(0))?.into(),
        ),
    )?;
    Ok(r0)
}
