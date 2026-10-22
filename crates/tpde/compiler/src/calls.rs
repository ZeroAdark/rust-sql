// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use tpde_assembler::SymRef;
use tpde_base::ValLocalIdx;
use tpde_core::{
    calling_convention::{CCAssignment, CallingConventionAssigner},
    ir_adaptor::IrAdaptor,
    Reg, RegBank,
};
use tpde_util::{tpde_assert, tpde_assert_eq, tpde_assert_ne};
use types_error::PgResult;

use crate::{
    guard_machine::{GuardMachine, RegallocEvent},
    value_part_ref::ValuePartRef,
    value_ref::ValueRef,
};

#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CallArgFlag {
    #[default]
    None,
    Zext,
    Sext,
    Sret,
    Byval,
    AllowSplit,
    /// An `x86_fp80` argument: passed in memory as a 16-byte, 16-aligned
    /// x87-class slot (upstream `CCAssignment{.align = 16, .bank =
    /// RegBank(-2), .size = 16}` added part-wise by `LLVMCompilerBase`).
    X87,
}

/// Upstream `tpde::RegBank(-2)`: the x87 argument class (memory only).
pub const X87_BANK: RegBank = RegBank::new(u8::MAX - 1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallArg<V> {
    pub value: V,
    pub flag: CallArgFlag,
    pub byval_align: u8,
    pub ext_bits: u8,
    pub byval_size: u32,
}

impl<V> CallArg<V> {
    pub const fn new(value: V) -> Self {
        Self::with_flags(value, CallArgFlag::None, 1, 0)
    }

    pub const fn with_flags(value: V, flag: CallArgFlag, byval_align: u8, byval_size: u32) -> Self {
        Self {
            value,
            flag,
            byval_align,
            ext_bits: 0,
            byval_size,
        }
    }
}

pub trait CallMachine<const N: usize>: GuardMachine<N> {
    type CallAdaptor: IrAdaptor;
    const PLATFORM_POINTER_SIZE: u32;

    fn call_adaptor(&self) -> &Self::CallAdaptor;
    fn call_val_parts_count(&self, value: &<Self::CallAdaptor as IrAdaptor>::ValueRef) -> u32;
    fn call_val_idx(&self, value: &<Self::CallAdaptor as IrAdaptor>::ValueRef) -> ValLocalIdx;
    fn call_val_ref(
        &self,
        value: &<Self::CallAdaptor as IrAdaptor>::ValueRef,
    ) -> PgResult<ValueRef<'_, Self, N>>;
    fn call_result_ref(
        &self,
        value: &<Self::CallAdaptor as IrAdaptor>::ValueRef,
    ) -> PgResult<ValueRef<'_, Self, N>>;
    fn call_is_leaf_function(&self) -> bool;
    /// Store a value part's register to its spill slot, unconditionally
    /// (the register is the authoritative copy of a fixed value; no state
    /// change beyond `stack_valid`); the call sequence saves fixed
    /// caller-saved registers with it before a cold call and reloads them
    /// with `reload_to_reg` after.
    fn call_spill_part(&self, local: ValLocalIdx, part: u32) -> PgResult<()>;
    fn call_set_generated_call(&self);
    fn call_free_assignment_if_present(&self, local: ValLocalIdx);
    fn call_init_stack_variable(&self, local: ValLocalIdx, frame_off: i32) -> PgResult<()>;
    fn prologue_assign_arg_part(
        &self,
        value: ValuePartRef<'_, Self, N>,
        assignment: CCAssignment,
    ) -> PgResult<Option<i32>>;
    fn gen_func_epilog(&self) -> PgResult<()>;
    fn release_regs_after_return(&self);
}

pub enum CallTarget<'c, M: GuardMachine<N>, const N: usize> {
    Symbol(SymRef),
    Value(ValuePartRef<'c, M, N>),
}

pub trait CallBuilderHooks<M: CallMachine<N>, const N: usize> {
    fn add_arg_byval<A: CallingConventionAssigner>(
        &mut self,
        compiler: &M,
        assigner: &mut A,
        value: &mut ValuePartRef<'_, M, N>,
        assignment: &mut CCAssignment,
    ) -> PgResult<()>;
    fn add_arg_stack<A: CallingConventionAssigner>(
        &mut self,
        compiler: &M,
        assigner: &mut A,
        value: &mut ValuePartRef<'_, M, N>,
        assignment: &mut CCAssignment,
    ) -> PgResult<()>;
    fn call_impl<A: CallingConventionAssigner>(
        &mut self,
        compiler: &M,
        assigner: &mut A,
        target: CallTarget<'_, M, N>,
    ) -> PgResult<()>;
}

pub struct CallBuilderBase<
    'c,
    'a,
    M: CallMachine<N>,
    A: CallingConventionAssigner,
    H: CallBuilderHooks<M, N>,
    const N: usize,
> {
    compiler: &'c M,
    assigner: &'a mut A,
    hooks: &'a mut H,
    arg_regs: u64,
}

impl<
        'c,
        'a,
        M: CallMachine<N>,
        A: CallingConventionAssigner,
        H: CallBuilderHooks<M, N>,
        const N: usize,
    > CallBuilderBase<'c, 'a, M, A, H, N>
{
    pub fn new(compiler: &'c M, assigner: &'a mut A, hooks: &'a mut H) -> Self {
        Self {
            compiler,
            assigner,
            hooks,
            arg_regs: 0,
        }
    }

    pub fn argument_registers(&self) -> u64 {
        self.arg_regs
    }

    fn restore_argument_registers(&mut self) {
        self.compiler.with_registers(|registers| {
            tpde_assert_eq!(registers.allocatable.get() & self.arg_regs, 0);
            registers
                .allocatable
                .set(registers.allocatable.get() | self.arg_regs);
        });
        self.arg_regs = 0;
    }

    pub fn add_arg_part(
        &mut self,
        value: ValuePartRef<'_, M, N>,
        assignment: CCAssignment,
    ) -> PgResult<()> {
        let result = self.add_arg_part_impl(value, assignment);
        if result.is_err() {
            self.restore_argument_registers();
        }
        result
    }

    fn add_arg_part_impl(
        &mut self,
        mut value: ValuePartRef<'_, M, N>,
        mut assignment: CCAssignment,
    ) -> PgResult<()> {
        tpde_assert!(
            core::ptr::eq(self.compiler, value.machine()),
            "value belongs to another compiler"
        );
        if !assignment.byval() && assignment.bank == RegBank::default() {
            assignment.bank = value.bank();
            assignment.size = value.part_size();
        }
        self.assigner.assign_arg(&mut assignment);
        let needs_ext = assignment.int_ext != 0;
        let ext_sign = assignment.int_ext >> 7 != 0;
        let ext_bits = u32::from(assignment.int_ext & 0x3f);
        if assignment.byval() {
            self.hooks
                .add_arg_byval(self.compiler, self.assigner, &mut value, &mut assignment)?;
            value.reset();
        } else if assignment.reg.invalid() {
            if needs_ext {
                let mut extended = value.into_extended(ext_sign, ext_bits, 64)?;
                self.hooks.add_arg_stack(
                    self.compiler,
                    self.assigner,
                    &mut extended,
                    &mut assignment,
                )?;
                extended.reset();
            } else {
                self.hooks.add_arg_stack(
                    self.compiler,
                    self.assigner,
                    &mut value,
                    &mut assignment,
                )?;
                value.reset();
            }
        } else {
            move_to_abi_register(self.compiler, &mut value, assignment, assignment.size, true)?;
            value.reset();
            self.compiler.with_registers(|registers| {
                tpde_assert!(!registers.is_used(assignment.reg));
                registers.mark_clobbered(assignment.reg);
                registers
                    .allocatable
                    .set(registers.allocatable.get() & !(1u64 << assignment.reg.id()));
            });
            self.arg_regs |= 1u64 << assignment.reg.id();
        }
        Ok(())
    }

    pub fn add_arg_parts(
        &mut self,
        arg: &CallArg<<M::CallAdaptor as IrAdaptor>::ValueRef>,
        part_count: u32,
    ) -> PgResult<()> {
        let result = self.add_arg_parts_impl(arg, part_count);
        if result.is_err() {
            self.restore_argument_registers();
        }
        result
    }

    fn add_arg_parts_impl(
        &mut self,
        arg: &CallArg<<M::CallAdaptor as IrAdaptor>::ValueRef>,
        part_count: u32,
    ) -> PgResult<()> {
        let value = self.compiler.call_val_ref(&arg.value)?;
        if arg.flag == CallArgFlag::Byval {
            tpde_assert_eq!(part_count, 1);
            let mut assignment = CCAssignment::new();
            assignment.set_byval(true);
            assignment.align = arg.byval_align;
            assignment.size = arg.byval_size;
            return self.add_arg_part(value.part(0), assignment);
        }
        if arg.flag == CallArgFlag::X87 {
            tpde_assert_eq!(part_count, 1);
            let mut assignment = CCAssignment::new();
            assignment.align = 16;
            assignment.bank = X87_BANK;
            assignment.size = 16;
            return self.add_arg_part(value.part(0), assignment);
        }
        for part in 0..part_count {
            let mut assignment = CCAssignment::new();
            if matches!(arg.flag, CallArgFlag::Sext | CallArgFlag::Zext) {
                tpde_assert_ne!(arg.ext_bits, 0, "cannot extend zero-bit integer");
                assignment.int_ext = arg.ext_bits
                    | if arg.flag == CallArgFlag::Sext {
                        0x80
                    } else {
                        0
                    };
            }
            assignment.consecutive =
                consecutive_parts(part_count, part, arg.flag == CallArgFlag::AllowSplit);
            assignment.set_sret(arg.flag == CallArgFlag::Sret);
            assignment.align = if part == 0 { arg.byval_align } else { 1 };
            self.add_arg_part(value.part(part), assignment)?;
        }
        Ok(())
    }

    pub fn add_arg(
        &mut self,
        arg: &CallArg<<M::CallAdaptor as IrAdaptor>::ValueRef>,
    ) -> PgResult<()> {
        self.add_arg_parts(arg, self.compiler.call_val_parts_count(&arg.value))
    }

    pub fn call(&mut self, target: CallTarget<'_, M, N>) -> PgResult<()> {
        let result = self.call_inner(target);
        self.restore_argument_registers();
        result
    }

    fn call_inner(&mut self, target: CallTarget<'_, M, N>) -> PgResult<()> {
        tpde_assert!(
            !self.compiler.call_is_leaf_function(),
            "leaf function must not have calls"
        );
        self.compiler.call_set_generated_call();
        let mut skip_evict = self.arg_regs;
        if let CallTarget::Value(value) = &target {
            tpde_assert!(
                core::ptr::eq(self.compiler, value.machine()),
                "value belongs to another compiler"
            );
            if value.can_salvage() {
                let reg = value.cur_reg_unlocked();
                tpde_assert!(reg.valid());
                skip_evict |= 1u64 << reg.id();
            }
        }
        let clobbered = !self.assigner.get_ccinfo().callee_saved_regs();
        let mut evict = self
            .compiler
            .with_registers(|registers| registers.used.get() & clobbered & !skip_evict);
        // Fixed (classic or pinned) values in clobbered registers: the
        // target admitted a caller-saved register only for a value whose
        // range crosses no hot call, so this call is cold for it. Save the
        // register to the value's slot before the call and reload it after;
        // the value's assignment and the register file do not change. The
        // target's pin/fixed masks keep such values out of the argument and
        // return registers, so the argument moves and the result placement
        // never meet one.
        let mut saved = 0u64;
        while evict != 0 {
            let reg = Reg::new(u64::from(evict.trailing_zeros()));
            evict &= evict - 1;
            if self.compiler.with_registers(|registers| registers.is_fixed(reg)) {
                let (local, part) = self
                    .compiler
                    .with_registers(|registers| (registers.reg_local_idx(reg), registers.reg_part(reg)));
                self.compiler.call_spill_part(local, part)?;
                saved |= 1u64 << reg.id();
                continue;
            }
            self.compiler.evict_reg(reg)?;
            self.compiler
                .with_registers(|registers| registers.mark_clobbered(reg));
        }
        self.hooks.call_impl(self.compiler, self.assigner, target)?;
        while saved != 0 {
            let reg = Reg::new(u64::from(saved.trailing_zeros()));
            saved &= saved - 1;
            let (local, part) = self
                .compiler
                .with_registers(|registers| (registers.reg_local_idx(reg), registers.reg_part(reg)));
            self.compiler.reload_to_reg(reg, local, part)?;
            self.compiler.regalloc_event(RegallocEvent::CallRestore);
        }
        Ok(())
    }

    pub fn add_ret_part(
        &mut self,
        value: &mut ValuePartRef<'_, M, N>,
        mut assignment: CCAssignment,
    ) -> PgResult<()> {
        tpde_assert!(
            core::ptr::eq(self.compiler, value.machine()),
            "value belongs to another compiler"
        );
        assignment.bank = value.bank();
        assignment.size = value.part_size();
        self.assigner.assign_ret(&mut assignment);
        tpde_assert!(assignment.reg.valid(), "return value must be in register");
        let result = value.set_value_reg(assignment.reg);
        if result.is_err() {
            self.restore_argument_registers();
        }
        result
    }

    pub fn add_ret_part_owned(
        &mut self,
        mut value: ValuePartRef<'_, M, N>,
        assignment: CCAssignment,
    ) -> PgResult<()> {
        self.add_ret_part(&mut value, assignment)
    }

    pub fn add_ret(&mut self, value: &mut ValueRef<'_, M, N>) -> PgResult<()> {
        tpde_assert!(value.has_assignment());
        let count = value.with_assignment(|assignment| assignment.part_count());
        for part in 0..count {
            self.add_ret_part_owned(value.part(part), CCAssignment::new())?;
        }
        Ok(())
    }
}

impl<
        M: CallMachine<N>,
        A: CallingConventionAssigner,
        H: CallBuilderHooks<M, N>,
        const N: usize,
    > Drop for CallBuilderBase<'_, '_, M, A, H, N>
{
    fn drop(&mut self) {
        self.restore_argument_registers();
    }
}

fn consecutive_parts(count: u32, part: u32, allow_split: bool) -> u8 {
    if allow_split {
        0
    } else if count < 256 {
        (count - part - 1) as u8
    } else {
        255
    }
}

fn move_to_abi_register<M: CallMachine<N>, const N: usize>(
    compiler: &M,
    value: &mut ValuePartRef<'_, M, N>,
    assignment: CCAssignment,
    move_size: u32,
    extend_constant_directly: bool,
) -> PgResult<()> {
    let reg = assignment.reg;
    let needs_ext = assignment.int_ext != 0;
    let sign = assignment.int_ext >> 7 != 0;
    let bits = u32::from(assignment.int_ext & 0x3f);
    if value.is_in_reg(reg) {
        if value.can_salvage() {
            value.salvage();
        } else {
            compiler.evict_reg(reg)?;
        }
        if needs_ext {
            compiler.generate_raw_intext(reg, reg, sign, bits, 64)?;
        }
    } else {
        if compiler.with_registers(|registers| registers.is_used(reg)) {
            compiler.evict_reg(reg)?;
        }
        if value.can_salvage() {
            let source = value.salvage();
            if needs_ext {
                compiler.generate_raw_intext(reg, source, sign, bits, 64)?;
            } else {
                compiler.mov(reg, source, move_size)?;
            }
        } else if needs_ext && extend_constant_directly && value.is_const() {
            let word = value.const_data()[0];
            let extended = if sign {
                tpde_util::misc::sext(word, bits)? as u64
            } else {
                tpde_util::misc::zext(word, bits)?
            };
            compiler.materialize_constant(&[extended], assignment.bank, 8, reg)?;
        } else {
            value.reload_into_specific_fixed(reg, 0)?;
            if needs_ext {
                compiler.generate_raw_intext(reg, reg, sign, bits, 64)?;
            }
        }
    }
    Ok(())
}

pub struct RetBuilder<'c, 'a, M: CallMachine<N>, A: CallingConventionAssigner, const N: usize> {
    compiler: &'c M,
    assigner: &'a mut A,
    ret_regs: u64,
}

impl<'c, 'a, M: CallMachine<N>, A: CallingConventionAssigner, const N: usize>
    RetBuilder<'c, 'a, M, A, N>
{
    pub fn new(compiler: &'c M, assigner: &'a mut A) -> Self {
        assigner.reset();
        Self {
            compiler,
            assigner,
            ret_regs: 0,
        }
    }

    pub fn return_registers(&self) -> u64 {
        self.ret_regs
    }

    fn restore_return_registers(&mut self) {
        self.compiler.with_registers(|registers| {
            tpde_assert_eq!(registers.allocatable.get() & self.ret_regs, 0);
            registers
                .allocatable
                .set(registers.allocatable.get() | self.ret_regs);
        });
        self.ret_regs = 0;
    }

    pub fn add_part(
        &mut self,
        value: ValuePartRef<'_, M, N>,
        assignment: CCAssignment,
    ) -> PgResult<()> {
        let result = self.add_part_impl(value, assignment);
        if result.is_err() {
            self.restore_return_registers();
        }
        result
    }

    fn add_part_impl(
        &mut self,
        mut value: ValuePartRef<'_, M, N>,
        mut assignment: CCAssignment,
    ) -> PgResult<()> {
        tpde_assert!(
            core::ptr::eq(self.compiler, value.machine()),
            "value belongs to another compiler"
        );
        assignment.bank = value.bank();
        let size = value.part_size();
        assignment.size = size;
        self.assigner.assign_ret(&mut assignment);
        tpde_assert!(
            assignment.reg.valid(),
            "indirect return value must use sret argument"
        );
        move_to_abi_register(self.compiler, &mut value, assignment, size, false)?;
        value.reset();
        self.compiler.with_registers(|registers| {
            tpde_assert!(!registers.is_used(assignment.reg));
            registers.mark_clobbered(assignment.reg);
            registers
                .allocatable
                .set(registers.allocatable.get() & !(1u64 << assignment.reg.id()));
        });
        self.ret_regs |= 1u64 << assignment.reg.id();
        Ok(())
    }

    pub fn add(&mut self, value: &<M::CallAdaptor as IrAdaptor>::ValueRef) -> PgResult<()> {
        let result = self.add_value_impl(value);
        if result.is_err() {
            self.restore_return_registers();
        }
        result
    }

    fn add_value_impl(&mut self, value: &<M::CallAdaptor as IrAdaptor>::ValueRef) -> PgResult<()> {
        let count = self.compiler.call_val_parts_count(value);
        let reference = self.compiler.call_val_ref(value)?;
        for part in 0..count {
            self.add_part(reference.part(part), CCAssignment::new())?;
        }
        Ok(())
    }

    pub fn ret(&mut self) -> PgResult<()> {
        self.restore_return_registers();
        let result = self.compiler.gen_func_epilog();
        self.compiler.release_regs_after_return();
        result
    }
}

impl<M: CallMachine<N>, A: CallingConventionAssigner, const N: usize> Drop
    for RetBuilder<'_, '_, M, A, N>
{
    fn drop(&mut self) {
        self.restore_return_registers();
    }
}

pub fn prologue_assign_arg<M: CallMachine<N>, A: CallingConventionAssigner, const N: usize>(
    compiler: &M,
    assigner: &mut A,
    arg_idx: u32,
    arg: &<M::CallAdaptor as IrAdaptor>::ValueRef,
    align: u32,
    allow_split: bool,
) -> PgResult<()> {
    let mut value = compiler.call_result_ref(arg)?;
    let adaptor = compiler.call_adaptor();
    if adaptor.cur_arg_is_byval(arg_idx) {
        let mut assignment = CCAssignment::new();
        assignment.set_byval(true);
        assignment.align = adaptor.cur_arg_byval_align(arg_idx) as u8;
        assignment.size = adaptor.cur_arg_byval_size(arg_idx);
        assigner.assign_arg(&mut assignment);
        let frame_off = compiler.prologue_assign_arg_part(value.part(0), assignment)?;
        if let Some(frame_off) = frame_off {
            let local = compiler.call_val_idx(arg);
            value.reset();
            compiler.call_free_assignment_if_present(local);
            compiler.call_init_stack_variable(local, frame_off)?;
        }
        return Ok(());
    }
    let count = compiler.with_assignment(value.local_idx(), |assignment| assignment.part_count());
    if adaptor.cur_arg_is_sret(arg_idx) {
        tpde_assert_eq!(count, 1, "sret must be single-part");
        let part = value.part(0);
        let mut assignment = CCAssignment::new();
        assignment.set_sret(true);
        assignment.bank = part.bank();
        assignment.size = M::PLATFORM_POINTER_SIZE;
        assigner.assign_arg(&mut assignment);
        compiler.prologue_assign_arg_part(part, assignment)?;
        return Ok(());
    }
    for index in 0..count {
        let part = value.part(index);
        let mut assignment = CCAssignment::new();
        assignment.consecutive = consecutive_parts(count, index, allow_split);
        assignment.align = if index == 0 { align as u8 } else { 1 };
        assignment.bank = part.bank();
        assignment.size = part.part_size();
        assigner.assign_arg(&mut assignment);
        compiler.prologue_assign_arg_part(part, assignment)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/calls.rs"]
mod tests;
