// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{
    calling_convention::CallingConventionAssigner,
    compiler_config::{
        check_config, CompilerConfig, CustomVariableReferences, DefaultVariableReferences,
        RegisterBanks, VariableReferencePolicy,
    },
    AssignmentPartRef, RegBank,
};
use tpde_assembler::SymRef;

pub trait ValueParts {
    fn count(&self) -> u32;
    fn size_bytes(&self, part: u32) -> u32;
    fn reg_bank(&self, part: u32) -> RegBank;
}

mod sealed {
    pub trait Special {}
    pub trait VariableRequirements<T: ?Sized, C> {}
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValRefSpecial<P: Copy> {
    pub mode: u8,
    pub payload: P,
}

impl<P: Copy> ValRefSpecial<P> {
    pub const fn new(mode: u8, payload: P) -> Self {
        Self { mode, payload }
    }
}

pub trait ValRefSpecialStruct: sealed::Special + Copy {
    fn mode(&self) -> u8;
    fn mode_mut(&mut self) -> &mut u8;
}

impl<P: Copy> sealed::Special for ValRefSpecial<P> {}

impl<P: Copy> ValRefSpecialStruct for ValRefSpecial<P> {
    fn mode(&self) -> u8 {
        self.mode
    }

    fn mode_mut(&mut self) -> &mut u8 {
        &mut self.mode
    }
}

pub trait CustomVariableReferenceCompiler<C: CompilerConfig> {
    fn setup_var_ref_assignments(&mut self);
    fn load_address_of_var_reference(
        &mut self,
        destination: C::AsmReg,
        part: AssignmentPartRef<'_, '_, '_>,
    );
}

pub trait VariableReferenceRequirements<T: ?Sized, C: CompilerConfig>:
    VariableReferencePolicy + sealed::VariableRequirements<T, C>
{
}

impl<T: ?Sized, C: CompilerConfig> sealed::VariableRequirements<T, C>
    for DefaultVariableReferences
{
}

impl<T: CustomVariableReferenceCompiler<C> + ?Sized, C: CompilerConfig>
    sealed::VariableRequirements<T, C> for CustomVariableReferences
{
}

impl<T: ?Sized, C: CompilerConfig> VariableReferenceRequirements<T, C>
    for DefaultVariableReferences
{
}

impl<T: CustomVariableReferenceCompiler<C> + ?Sized, C: CompilerConfig>
    VariableReferenceRequirements<T, C> for CustomVariableReferences
{
}

pub trait Compiler<C: CompilerConfig>: Sized
where
    C::VariableReferences: VariableReferenceRequirements<Self, C>,
{
    const NUM_FIXED_ASSIGNMENTS: <C::Banks as RegisterBanks>::FixedAssignments;

    type IRValueRef;
    type IRFuncRef;
    type IRInstRef;
    type InstRange;
    type GenericValuePart;
    type ValuePart;
    type Parts: ValueParts;
    type ValRefSpecial: ValRefSpecialStruct;
    type CCAssigner: CallingConventionAssigner;

    fn start_func(&mut self, func_idx: u32);
    fn prologue_assign_arg<A: CallingConventionAssigner>(
        &mut self,
        assigner: &mut A,
        index: u32,
        value: Self::IRValueRef,
    );
    fn finish_func(&mut self, func_idx: u32);
    fn spill_reg(&mut self, reg: C::AsmReg, frame_off: u32, size: u32);
    fn load_from_stack(&mut self, reg: C::AsmReg, frame_off: u32, size: u32);
    fn mov(&mut self, destination: C::AsmReg, source: C::AsmReg, size: u32);
    fn gval_expr_as_reg(&mut self, value: &mut Self::GenericValuePart) -> C::AsmReg;
    fn select_fixed_assignment_reg(
        &mut self,
        part: AssignmentPartRef<'_, '_, '_>,
        value: Self::IRValueRef,
    ) -> C::AsmReg;
    fn cur_func_may_emit_calls(&self) -> bool;
    fn cur_personality_func(&self) -> SymRef;
    fn cur_cc_assigner(&mut self) -> &mut Self::CCAssigner;
    #[doc = "Lend the selected state without cloning; restore it on return or unwind and reject nested acquisition."]
    fn with_cur_cc_assigner<R>(
        &mut self,
        operation: impl FnOnce(&mut Self, &mut Self::CCAssigner) -> R,
    ) -> R;
    fn try_force_fixed_assignment(&mut self, value: Self::IRValueRef) -> bool;
    fn val_parts(&self, value: Self::IRValueRef) -> Self::Parts;
    fn val_ref_special(&mut self, value: Self::IRValueRef) -> Option<Self::ValRefSpecial>;
    fn val_part_ref_special(
        &mut self,
        value: &mut Self::ValRefSpecial,
        part: u32,
    ) -> Self::ValuePart;
    fn define_func_idx(&mut self, function: Self::IRFuncRef, index: u32);
    fn compile_inst(&mut self, instruction: Self::IRInstRef, range: Self::InstRange) -> bool;
}

pub const fn check_compiler<C: CompilerConfig, T: Compiler<C>>()
where
    C::VariableReferences: VariableReferenceRequirements<T, C>,
{
    check_config::<C>();
    const {
        assert!(
            C::NUM_BANKS != 0,
            "compiler requires at least one register bank"
        )
    };
}
