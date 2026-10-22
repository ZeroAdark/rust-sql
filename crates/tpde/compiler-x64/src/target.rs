// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{CompilerX64, X64CompilerState};
use tpde_assembler::{AssemblerElf, SymRef};
use tpde_compiler::{
    target::{CompilerOutput, CompilerTarget, TargetMachine},
    CompilerAdaptor, RegallocEvent, ValuePartRef,
};
use tpde_core::calling_convention::CallingConventionAssigner;
use tpde_core::compiler::{ValRefSpecialStruct, ValueParts};
use types_error::PgResult;

pub trait X64Frontend<A: CompilerAdaptor>: Sized {
    type Special: ValRefSpecialStruct;
    type Parts: ValueParts;
    const DEFAULT_VAR_REF_HANDLING: bool = true;
    fn val_parts(&self, adaptor: &A, value: A::ValueRef) -> Self::Parts;
    fn val_ref_special(&self, adaptor: &A, value: A::ValueRef) -> Option<Self::Special>;
    fn cur_func_may_emit_calls(&self, adaptor: &A) -> bool;
    fn cur_personality_func(&self, adaptor: &A) -> SymRef;
    fn define_func_idx(&mut self, adaptor: &A, function: A::FuncRef, index: u32);
    fn reset(&mut self) {}
    fn try_force_fixed_assignment(&self, _adaptor: &A, _value: A::ValueRef) -> bool {
        false
    }
    fn allow_fixed_assignment(&self, _adaptor: &A, _value: A::ValueRef) -> bool {
        true
    }
    /// See `CompilerTarget::PHI_COALESCING` / `WEIGHTED_REGALLOC` /
    /// `PINNED_ASSIGNMENTS` / `value_weight`.
    const PHI_COALESCING: bool = false;
    const WEIGHTED_REGALLOC: bool = false;
    const PINNED_ASSIGNMENTS: [u32; 2] = [0, 0];
    fn value_weight(&self, _adaptor: &A, _value: A::ValueRef) -> Option<u32> {
        None
    }
    /// See `CompilerTarget::regalloc_event`.
    fn regalloc_event(&self, _adaptor: &A, _event: RegallocEvent, _block: A::BlockRef) {}
    fn before_function<'m, O: CompilerOutput<'m>>(
        &mut self,
        _adaptor: &mut A,
        _output: &mut O,
        _function: A::FuncRef,
        _index: u32,
        _symbol: SymRef,
    ) -> PgResult<bool> {
        Ok(true)
    }
    fn after_function<'m, O: CompilerOutput<'m>>(
        &mut self,
        _adaptor: &mut A,
        _output: &mut O,
        _function: A::FuncRef,
        _index: u32,
        success: bool,
    ) -> PgResult<bool> {
        Ok(success)
    }
    fn hook_post_func_sym_init<'m>(
        &mut self,
        _adaptor: &mut A,
        _assembler: &mut AssemblerElf<'m>,
        _symbols: &[SymRef],
    ) -> PgResult<bool> {
        Ok(true)
    }
    fn analysis_start(&mut self, _adaptor: &A) {}
    fn analysis_end(&mut self, _adaptor: &A) {}
    fn special_part<'s, 'm, M>(
        machine: &'s M,
        special: &'s Self::Special,
        part: u32,
    ) -> ValuePartRef<'s, M, 2>
    where
        A: 's,
        M: TargetMachine<'m, A, 2>,
        M::Target: X64CompilerState<Frontend = Self>
            + CompilerTarget<A, 2, Special = Self::Special, Jump = crate::Jump>;
    /// `prologue_assign_arg` (upstream `LLVMCompilerX64::prologue_assign_arg`
    /// overrides the generic one for `x86_fp80` arguments); the default is the
    /// generic single-part assignment.
    fn prologue_assign_arg<'m, M, C>(
        machine: &M,
        assigner: &mut C,
        index: u32,
        value: &A::ValueRef,
    ) -> PgResult<()>
    where
        M: TargetMachine<'m, A, 2>,
        M::Target: X64CompilerState<Frontend = Self>
            + CompilerTarget<A, 2, Special = Self::Special, Jump = crate::Jump>,
        C: CallingConventionAssigner,
    {
        tpde_compiler::calls::prologue_assign_arg(machine, assigner, index, value, 1, false)
    }
    fn compile_inst<'m, M>(
        machine: &M,
        inst: A::InstRef,
        remaining: impl Iterator<Item = A::InstRef> + Clone,
    ) -> PgResult<bool>
    where
        M: TargetMachine<'m, A, 2>,
        M::Target: X64CompilerState<Frontend = Self>
            + CompilerTarget<A, 2, Special = Self::Special, Jump = crate::Jump>;
    fn setup_var_ref_assignments<'m, M>(_machine: &M) -> PgResult<()>
    where
        M: TargetMachine<'m, A, 2>,
        M::Target: X64CompilerState<Frontend = Self>
            + CompilerTarget<A, 2, Special = Self::Special, Jump = crate::Jump>,
    {
        Err(crate::invalid(
            "X64 frontend has no custom variable-reference setup",
        ))
    }
    fn load_address_of_var_reference<'m, M>(
        _machine: &M,
        _destination: tpde_core::Reg,
        _local: tpde_base::ValLocalIdx,
        _part: u32,
    ) -> PgResult<()>
    where
        M: TargetMachine<'m, A, 2>,
        M::Target: X64CompilerState<Frontend = Self>
            + CompilerTarget<A, 2, Special = Self::Special, Jump = crate::Jump>,
    {
        Err(crate::invalid(
            "X64 frontend has no custom variable references",
        ))
    }
}

use crate::{
    calling_convention::{CCAssignerSysV, CC_INFO},
    emission, frame, Jump,
};
use tpde_assembler::Architecture;
use tpde_base::ValLocalIdx;
use tpde_compiler::GenericValuePart;
use tpde_core::{calling_convention::CCAssignment, Reg, RegBank};
use tpde_function_writer::Label;

impl<A: CompilerAdaptor, F: X64Frontend<A>> CompilerTarget<A, 2> for CompilerX64<'_, F> {
    type Special = F::Special;
    type Parts = F::Parts;
    type Jump = Jump;
    type Writer = tpde_function_writer_x64::X64Target;
    type CCPolicy = CCAssignerSysV;
    const ARCHITECTURE: Architecture = Architecture::X86_64;
    const FRAME_INDEXING_NEGATIVE: bool = true;
    const PLATFORM_POINTER_SIZE: u32 = 8;
    const GP_BANK: RegBank = RegBank::new(0);
    const FIXED_ASSIGNMENTS: [u32; 2] = crate::NUM_FIXED_ASSIGNMENTS;
    const MAX_STATIC_ALLOCA_SIZE: u32 = crate::MAX_STATIC_ALLOCA_SIZE;
    const DEFAULT_VAR_REF_HANDLING: bool = F::DEFAULT_VAR_REF_HANDLING;
    fn reset(&mut self) {
        self.clear_returns();
        self.state.sym_tls_get_addr = SymRef::default();
        self.frontend.reset();
    }
    fn cc_info(&self, _adaptor: &A) -> tpde_core::calling_convention::CCInfo {
        CC_INFO
    }
    fn new_cc_policy(&self, _adaptor: &A) -> Self::CCPolicy {
        CCAssignerSysV::new(false)
    }
    fn val_parts(&self, adaptor: &A, value: A::ValueRef) -> Self::Parts {
        self.frontend.val_parts(adaptor, value)
    }
    fn val_ref_special(&self, adaptor: &A, value: A::ValueRef) -> Option<Self::Special> {
        self.frontend.val_ref_special(adaptor, value)
    }
    fn cur_func_may_emit_calls(&self, adaptor: &A) -> bool {
        self.frontend.cur_func_may_emit_calls(adaptor)
    }
    fn cur_personality_func(&self, adaptor: &A) -> SymRef {
        self.frontend.cur_personality_func(adaptor)
    }
    fn define_func_idx(&mut self, adaptor: &A, function: A::FuncRef, index: u32) {
        self.frontend.define_func_idx(adaptor, function, index)
    }
    fn try_force_fixed_assignment(&self, adaptor: &A, value: A::ValueRef) -> bool {
        self.frontend.try_force_fixed_assignment(adaptor, value)
    }
    const PHI_COALESCING: bool = F::PHI_COALESCING;
    const WEIGHTED_REGALLOC: bool = F::WEIGHTED_REGALLOC;
    const PINNED_ASSIGNMENTS: [u32; 2] = F::PINNED_ASSIGNMENTS;
    fn value_weight(&self, adaptor: &A, value: A::ValueRef) -> Option<u32> {
        self.frontend.value_weight(adaptor, value)
    }
    fn regalloc_event(&self, adaptor: &A, event: RegallocEvent, block: A::BlockRef) {
        self.frontend.regalloc_event(adaptor, event, block)
    }
    fn pin_reg_mask<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        bank: RegBank,
        crosses_call: bool,
        crosses_cold_call: bool,
        value: A::ValueRef,
    ) -> u64 {
        // Cold-call save/restore is not enabled on x86-64 (the pinned
        // budget is three and the caller-saved relaxation has an open
        // amd64 differential failure, tpde-phi-coalescing.md): a cold call
        // counts as hot.
        let crosses_call = crosses_call || crosses_cold_call;
        if !F::PHI_COALESCING
            || !machine.with_target(|target| {
                target
                    .frontend
                    .allow_fixed_assignment(machine.call_adaptor(), value)
            })
        {
            return 0;
        }
        let (excluded, saved) = machine.with_target(|target| {
            let state = target.x64();
            (
                state.fixed_assignment_nonallocatable_mask,
                state.callee_saved_regs,
            )
        });
        let mut mask = tpde_core::RegisterFile::<2, 32>::bank_regs(bank) & !excluded;
        if crosses_call && !machine.call_is_leaf_function() {
            mask &= saved;
        }
        mask
    }
    fn hook_post_func_sym_init<'m>(
        &mut self,
        adaptor: &mut A,
        assembler: &mut AssemblerElf<'m>,
        symbols: &[SymRef],
    ) -> PgResult<bool> {
        self.frontend
            .hook_post_func_sym_init(adaptor, assembler, symbols)
    }
    fn before_function<'m, O: CompilerOutput<'m>>(
        &mut self,
        adaptor: &mut A,
        output: &mut O,
        function: A::FuncRef,
        index: u32,
        symbol: SymRef,
    ) -> PgResult<bool> {
        self.frontend
            .before_function(adaptor, output, function, index, symbol)
    }
    fn after_function<'m, O: CompilerOutput<'m>>(
        &mut self,
        adaptor: &mut A,
        output: &mut O,
        function: A::FuncRef,
        index: u32,
        success: bool,
    ) -> PgResult<bool> {
        self.frontend
            .after_function(adaptor, output, function, index, success)
    }
    fn analysis_start(&mut self, adaptor: &A) {
        self.frontend.analysis_start(adaptor)
    }
    fn analysis_end(&mut self, adaptor: &A) {
        self.frontend.analysis_end(adaptor)
    }
    fn unconditional_jump() -> Jump {
        Jump::jmp
    }
    fn invert_jump(jump: Jump) -> Jump {
        emission::invert_jump(jump)
    }
    fn special_part<'s, 'm, M: TargetMachine<'m, A, 2, Target = Self, Special = Self::Special>>(
        machine: &'s M,
        special: &'s Self::Special,
        part: u32,
    ) -> ValuePartRef<'s, M, 2>
    where
        A: 's,
    {
        F::special_part(machine, special, part)
    }
    fn select_fixed_assignment_reg<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        local: ValLocalIdx,
        part: u32,
        value: A::ValueRef,
    ) -> Reg {
        if machine.with_target(|target| {
            target
                .frontend
                .allow_fixed_assignment(machine.call_adaptor(), value)
        }) {
            frame::select_fixed_assignment_reg(machine, local, part)
        } else {
            Reg::make_invalid()
        }
    }
    fn start_func<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        index: u32,
    ) -> PgResult<()> {
        frame::start_func(machine, index)
    }
    fn finish_func<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        index: u32,
    ) -> PgResult<()> {
        frame::finish_func(machine, index)
    }
    fn prologue_begin<
        'm,
        M: TargetMachine<'m, A, 2, Target = Self>,
        C: CallingConventionAssigner,
    >(
        machine: &M,
        assigner: &mut C,
    ) -> PgResult<()> {
        frame::prologue_begin(machine, assigner)
    }
    fn prologue_assign_arg<
        'm,
        M: TargetMachine<'m, A, 2, Target = Self>,
        C: CallingConventionAssigner,
    >(
        machine: &M,
        assigner: &mut C,
        index: u32,
        value: &A::ValueRef,
    ) -> PgResult<()> {
        F::prologue_assign_arg(machine, assigner, index, value)
    }
    fn prologue_end<'m, M: TargetMachine<'m, A, 2, Target = Self>, C: CallingConventionAssigner>(
        machine: &M,
        assigner: &mut C,
    ) -> PgResult<()> {
        frame::prologue_end(machine, assigner)
    }
    fn prologue_assign_arg_part<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        mut part: ValuePartRef<'_, M, 2>,
        assignment: CCAssignment,
    ) -> PgResult<Option<i32>> {
        frame::prologue_assign_arg_part(machine, &mut part, assignment)
    }
    fn compile_inst<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        inst: A::InstRef,
        remaining: impl Iterator<Item = A::InstRef> + Clone,
    ) -> PgResult<bool> {
        F::compile_inst(machine, inst, remaining)
    }
    fn alloca_fixed<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        size: u32,
        alignment: u32,
        mut part: ValuePartRef<'_, M, 2>,
    ) -> PgResult<()> {
        emission::alloca_fixed(machine, u64::from(size), alignment, &mut part)
    }
    fn setup_var_ref_assignments<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
    ) -> PgResult<()> {
        F::setup_var_ref_assignments(machine)
    }
    fn load_address_of_stack_var<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        destination: Reg,
        local: ValLocalIdx,
        part: u32,
    ) -> PgResult<()> {
        emission::load_address_of_stack_var(machine, destination, local, part)
    }
    fn load_address_of_var_reference<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        destination: Reg,
        local: ValLocalIdx,
        part: u32,
    ) -> PgResult<()> {
        F::load_address_of_var_reference(machine, destination, local, part)
    }
    fn spill_reg<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        reg: Reg,
        offset: i32,
        size: u32,
    ) -> PgResult<()> {
        emission::spill_reg(machine, reg, offset, size)
    }
    fn load_from_stack<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        reg: Reg,
        offset: i32,
        size: u32,
    ) -> PgResult<()> {
        emission::load_from_stack(machine, reg, offset, size, false)
    }
    fn mov<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        destination: Reg,
        source: Reg,
        size: u32,
    ) -> PgResult<()> {
        emission::mov(machine, destination, source, size)
    }
    fn materialize_constant<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        words: &[u64],
        bank: RegBank,
        size: u32,
        reg: Reg,
    ) -> PgResult<()> {
        emission::materialize_constant(machine, words, bank, size, reg)
    }
    fn generate_raw_intext<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        destination: Reg,
        source: Reg,
        sign: bool,
        from: u32,
        to: u32,
    ) -> PgResult<()> {
        emission::generate_raw_intext(machine, destination, source, sign, from, to)
    }
    fn gval_expr_as_reg<'s, 'm, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &'s M,
        value: &mut GenericValuePart<'s, M, 2>,
    ) -> PgResult<Reg> {
        emission::gval_expr_as_reg(machine, value)
    }
    fn generate_raw_jump<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        jump: Jump,
        label: Label,
    ) -> PgResult<()> {
        emission::generate_raw_jump(machine, jump, label)
    }
    fn switch_emit_cmpeq<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        label: Label,
        cmp: Reg,
        tmp: Reg,
        value: u64,
        width32: bool,
    ) -> PgResult<()> {
        emission::switch_emit_cmpeq(machine, label, cmp, tmp, value, width32)
    }
    fn switch_emit_binary_step<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        equal: Label,
        greater: Label,
        cmp: Reg,
        tmp: Reg,
        value: u64,
        width32: bool,
    ) -> PgResult<()> {
        emission::switch_emit_binary_step(machine, equal, greater, cmp, tmp, value, width32)
    }
    fn switch_create_jump_table<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        default: Label,
        cmp: Reg,
        tmp: Reg,
        low: u64,
        high: u64,
        width32: bool,
        fill: impl FnOnce(&mut [Label]),
    ) -> PgResult<bool> {
        emission::switch_create_jump_table(machine, default, cmp, tmp, low, high, width32, fill)
    }
    fn gen_func_epilog<'m, M: TargetMachine<'m, A, 2, Target = Self>>(machine: &M) -> PgResult<()> {
        frame::gen_func_epilog(machine)
    }
}
