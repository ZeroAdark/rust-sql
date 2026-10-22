// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![no_std]
#![doc = include_str!("../tests/ownership.md")]
extern crate alloc;
pub mod abi;
pub mod calls;
pub mod config;
pub mod prologue;
pub mod raw;
mod target_guard;
pub mod values;
pub use config::PlatformConfig;
pub const CPU_BASELINE: u32 = 0;
pub use abi::{CCAssignerAAPCS, AAPCS_INFO, FP_BANK, GP_BANK};
pub use raw::{AsmReg, Jump, JumpKind};
pub use target_guard::CompilerA64;
use tpde_assembler::{AssemblerElf, SymRef};
use tpde_base::ValLocalIdx;
use tpde_compiler::{
    target::{CompilerOutput, CompilerTarget, TargetMachine},
    CompilerAdaptor, RegallocEvent, ValuePartRef,
};
use tpde_core::{
    compiler::{ValRefSpecialStruct, ValueParts},
    Reg,
};
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};

#[derive(Clone, Copy, Debug)]
pub struct A64State {
    pub cpu_feats: u32,
    pub fixed_assignment_nonallocatable_mask: u64,
    pub callee_saved_regs: u64,
    pub func_start_off: u32,
    pub func_prologue_alloc: u32,
    pub func_arg_stack_add_off: u32,
    pub func_arg_stack_add_reg: Reg,
    pub permanent_scratch_reg: Reg,
    pub scalar_arg_count: u32,
    pub vec_arg_count: u32,
    pub reg_save_frame_off: u32,
}
impl Default for A64State {
    fn default() -> Self {
        Self {
            cpu_feats: 0,
            fixed_assignment_nonallocatable_mask: 3,
            callee_saved_regs: AAPCS_INFO.callee_saved_regs(),
            func_start_off: 0,
            func_prologue_alloc: 0,
            func_arg_stack_add_off: u32::MAX,
            func_arg_stack_add_reg: Reg::make_invalid(),
            permanent_scratch_reg: Reg::new(16),
            scalar_arg_count: u32::MAX,
            vec_arg_count: u32::MAX,
            reg_save_frame_off: 0,
        }
    }
}
pub trait A64CompilerState {
    type Frontend;
    fn a64_frontend(&self) -> &Self::Frontend;
    fn a64_frontend_mut(&mut self) -> &mut Self::Frontend;
    fn a64_state(&self) -> &A64State;
    fn a64_state_mut(&mut self) -> &mut A64State;
    fn a64_return_offsets(&self) -> &[u32];
    fn a64_clear_returns(&mut self);
    fn a64_push_return(&mut self, offset: u32) -> PgResult<()>;
    fn a64_pop_return(&mut self) -> Option<u32>;
}
pub trait A64Frontend<A: CompilerAdaptor>: Sized {
    type Special: ValRefSpecialStruct;
    type Parts: ValueParts;
    const DEFAULT_VAR_REF_HANDLING: bool = true;
    fn reset(&mut self) {}
    fn val_parts(&self, adaptor: &A, value: A::ValueRef) -> Self::Parts;
    fn val_ref_special(&self, adaptor: &A, value: A::ValueRef) -> Option<Self::Special>;
    fn cur_func_may_emit_calls(&self, adaptor: &A) -> bool;
    fn cur_personality_func(&self, adaptor: &A) -> SymRef;
    fn define_func_idx(&mut self, adaptor: &A, function: A::FuncRef, index: u32);
    fn hook_post_func_sym_init<'m>(
        &mut self,
        _: &mut A,
        _: &mut AssemblerElf<'m>,
        _: &[SymRef],
    ) -> PgResult<bool> {
        Ok(true)
    }
    fn before_function<'m, O: CompilerOutput<'m>>(
        &mut self,
        _: &mut A,
        _: &mut O,
        _: A::FuncRef,
        _: u32,
        _: SymRef,
    ) -> PgResult<bool> {
        Ok(true)
    }
    fn after_function<'m, O: CompilerOutput<'m>>(
        &mut self,
        _: &mut A,
        _: &mut O,
        _: A::FuncRef,
        _: u32,
        success: bool,
    ) -> PgResult<bool> {
        Ok(success)
    }
    fn analysis_start(&mut self, _: &A) {}
    fn analysis_end(&mut self, _: &A) {}
    fn try_force_fixed_assignment(&self, _: &A, _: A::ValueRef) -> bool {
        false
    }
    fn allow_fixed_assignment(&self, _: &A, _: A::ValueRef) -> bool {
        true
    }
    /// See `CompilerTarget::PHI_COALESCING` / `WEIGHTED_REGALLOC` /
    /// `PINNED_ASSIGNMENTS` / `value_weight`.
    const PHI_COALESCING: bool = false;
    const WEIGHTED_REGALLOC: bool = false;
    const PINNED_ASSIGNMENTS: [u32; 2] = [0, 0];
    /// Classic fixed-assignment budget per bank (upstream: 12 GP, 6 FP).
    /// A frontend that nominates loop-invariant values may raise it.
    const FIXED_ASSIGNMENTS: [u32; 2] = [12, 6];
    /// See `CompilerTarget::PHI_REUSE`.
    const PHI_REUSE: bool = false;
    fn value_weight(&self, _: &A, _: A::ValueRef) -> Option<u32> {
        None
    }
    /// See `CompilerTarget::regalloc_event`.
    fn regalloc_event(&self, _: &A, _: RegallocEvent, _: A::BlockRef) {}
    fn setup_var_ref_assignments<'m, M: TargetMachine<'m, A, 2>>(_: &M) -> PgResult<()>
    where
        M::Target: CompilerTarget<A, 2, Jump = Jump> + A64CompilerState<Frontend = Self>,
    {
        unreachable!("A64 uses default variable-reference handling")
    }
    fn load_address_of_var_reference<'m, M: TargetMachine<'m, A, 2>>(
        _: &M,
        _: Reg,
        _: ValLocalIdx,
        _: u32,
    ) -> PgResult<()>
    where
        M::Target: CompilerTarget<A, 2, Jump = Jump> + A64CompilerState<Frontend = Self>,
    {
        unreachable!("A64 uses default variable-reference handling")
    }
    fn compile_inst<'m, M: TargetMachine<'m, A, 2>>(
        machine: &M,
        inst: A::InstRef,
        remaining: impl Iterator<Item = A::InstRef> + Clone,
    ) -> PgResult<bool>
    where
        M::Target: CompilerTarget<A, 2, Jump = Jump> + A64CompilerState<Frontend = Self>;
    fn special_part<'s, 'm, M: TargetMachine<'m, A, 2>>(
        machine: &'s M,
        special: &'s Self::Special,
        part: u32,
    ) -> ValuePartRef<'s, M, 2>
    where
        A: 's,
        M::Target: CompilerTarget<A, 2, Special = Self::Special, Jump = Jump>
            + A64CompilerState<Frontend = Self>;
}
#[cold]
pub(crate) fn invalid(message: &'static str) -> alloc::boxed::Box<PgError> {
    PgError::error(message)
        .with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE)
        .into()
}
pub(crate) fn ensure(condition: bool, message: &'static str) -> PgResult<()> {
    if condition {
        Ok(())
    } else {
        Err(invalid(message))
    }
}
const _: () = assert!(core::mem::size_of::<A64State>() <= 128);
mcx::forget_safe_nodrop!(A64State);
#[cfg(all(test, miri))]
extern crate self as tpde_compiler_a64;
#[cfg(test)]
extern crate std;
#[cfg(all(test, miri))]
#[path = "../tests/platform_behaviors.rs"]
mod platform_behaviors;
