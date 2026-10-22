// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{A64CompilerState, A64Frontend, A64State, Jump, abi::*, prologue, raw, values};
use mcx::Mcx;
use tpde_assembler::{Architecture, AssemblerElf, SymRef};
use tpde_base::ValLocalIdx;
use tpde_compiler::{
    CompilerAdaptor, GenericValuePart, RegallocEvent, ValuePartRef,
    target::{CompilerOutput, CompilerTarget, TargetMachine},
};
use tpde_core::{
    Reg, RegBank,
    calling_convention::{CCAssignment, CCInfo, CallingConventionAssigner},
};
use tpde_function_writer::Label;
use tpde_function_writer_a64::A64Target;
use tpde_util::SmallVectorGuard;
use types_error::PgResult;

pub struct CompilerA64<'m, F> {
    pub frontend: F,
    state: A64State,
    return_offsets: SmallVectorGuard<'m, u32, 8>,
}
impl<'m, F> CompilerA64<'m, F> {
    pub fn new(mcx: Mcx<'m>, frontend: F, cpu_feats: u32) -> Self {
        Self {
            frontend,
            state: A64State {
                cpu_feats,
                ..A64State::default()
            },
            return_offsets: SmallVectorGuard::new(mcx),
        }
    }
    pub fn state(&self) -> &A64State {
        &self.state
    }
    pub fn has_cpu_feats(&self, features: u32) -> bool {
        self.state.cpu_feats & features == features
    }
}
impl<F> A64CompilerState for CompilerA64<'_, F> {
    type Frontend = F;
    fn a64_frontend(&self) -> &F {
        &self.frontend
    }
    fn a64_frontend_mut(&mut self) -> &mut F {
        &mut self.frontend
    }
    fn a64_state(&self) -> &A64State {
        &self.state
    }
    fn a64_state_mut(&mut self) -> &mut A64State {
        &mut self.state
    }
    fn a64_return_offsets(&self) -> &[u32] {
        &self.return_offsets
    }
    fn a64_clear_returns(&mut self) {
        self.return_offsets.clear();
    }
    fn a64_push_return(&mut self, offset: u32) -> PgResult<()> {
        self.return_offsets.push(offset)
    }
    fn a64_pop_return(&mut self) -> Option<u32> {
        self.return_offsets.pop()
    }
}
impl<'ctx, A: CompilerAdaptor, F: A64Frontend<A>> CompilerTarget<A, 2> for CompilerA64<'ctx, F> {
    type Special = F::Special;
    type Parts = F::Parts;
    type Jump = Jump;
    type Writer = A64Target<'ctx>;
    type CCPolicy = CCAssignerAAPCS;
    const ARCHITECTURE: Architecture = Architecture::Aarch64;
    const FRAME_INDEXING_NEGATIVE: bool = false;
    const PLATFORM_POINTER_SIZE: u32 = 8;
    const GP_BANK: RegBank = GP_BANK;
    const FIXED_ASSIGNMENTS: [u32; 2] = F::FIXED_ASSIGNMENTS;
    const MAX_STATIC_ALLOCA_SIZE: u32 = 0x10_0000;
    const DEFAULT_VAR_REF_HANDLING: bool = F::DEFAULT_VAR_REF_HANDLING;
    fn reset(&mut self) {
        let feats = self.state.cpu_feats;
        self.state = A64State {
            cpu_feats: feats,
            ..A64State::default()
        };
        self.return_offsets.clear();
        self.frontend.reset();
    }
    fn cc_info(&self, _: &A) -> CCInfo {
        AAPCS_INFO
    }
    fn new_cc_policy(&self, _: &A) -> Self::CCPolicy {
        CCAssignerAAPCS::default()
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
        self.frontend.define_func_idx(adaptor, function, index);
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
        self.frontend.analysis_start(adaptor);
    }
    fn analysis_end(&mut self, adaptor: &A) {
        self.frontend.analysis_end(adaptor);
    }
    fn try_force_fixed_assignment(&self, adaptor: &A, value: A::ValueRef) -> bool {
        self.frontend.try_force_fixed_assignment(adaptor, value)
    }
    const PHI_COALESCING: bool = F::PHI_COALESCING;
    const WEIGHTED_REGALLOC: bool = F::WEIGHTED_REGALLOC;
    const PINNED_ASSIGNMENTS: [u32; 2] = F::PINNED_ASSIGNMENTS;
    const PHI_REUSE: bool = F::PHI_REUSE;
    fn regalloc_event(&self, adaptor: &A, event: RegallocEvent, block: A::BlockRef) {
        self.frontend.regalloc_event(adaptor, event, block)
    }
    fn value_weight(&self, adaptor: &A, value: A::ValueRef) -> Option<u32> {
        self.frontend.value_weight(adaptor, value)
    }
    fn pin_reg_mask<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        bank: RegBank,
        crosses_call: bool,
        crosses_cold_call: bool,
        value: A::ValueRef,
    ) -> u64 {
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
            let state = target.a64_state();
            (
                state.fixed_assignment_nonallocatable_mask,
                state.callee_saved_regs,
            )
        });
        let mut mask = tpde_core::RegisterFile::<2, 32>::bank_regs(bank) & !excluded;
        if !machine.call_is_leaf_function() {
            if crosses_call {
                // A callee-saved register while one is free; else a
                // caller-saved register that no argument or result takes,
                // saved and restored around each call the value crosses
                // (`calls.rs::call_inner`: two memory operations per call,
                // never per iteration) — strictly better than the stack
                // slot the phi would otherwise get (`backend.md` §6.2 item 1).
                // Last, the argument registers but the return pair,
                // parked around each call (`calls.rs`).
                let free = machine.with_registers(|r| r.allocatable.get() & !r.used.get());
                let args = crate::abi::AAPCS_INFO.arg_regs();
                if mask & saved & free != 0 {
                    mask &= saved;
                } else if mask & !args & free != 0 {
                    mask &= saved | !args;
                } else {
                    mask &= !0b11;
                }
            } else if crosses_cold_call {
                // Saved and restored around the cold call (`calls.rs`), so
                // it must not be a register the call's arguments or result
                // take.
                mask &= !crate::abi::AAPCS_INFO.arg_regs();
            }
        }
        mask
    }
    fn unconditional_jump() -> Jump {
        Jump::default()
    }
    fn invert_jump(jump: Jump) -> Jump {
        raw::invert_jump(jump)
    }
    fn special_part<'s, 'm, M: TargetMachine<'m, A, 2, Target = Self>>(
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
        if !machine.with_target(|target| {
            target
                .frontend
                .allow_fixed_assignment(machine.call_adaptor(), value)
        }) {
            return Reg::make_invalid();
        }
        values::select_fixed_assignment_reg(machine, local, part, value)
    }
    fn start_func<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        _machine: &M,
        _index: u32,
    ) -> PgResult<()> {
        Ok(())
    }
    fn finish_func<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        index: u32,
    ) -> PgResult<()> {
        prologue::finish_func(machine, index)
    }
    fn prologue_begin<
        'm,
        M: TargetMachine<'m, A, 2, Target = Self>,
        C: CallingConventionAssigner,
    >(
        machine: &M,
        assigner: &mut C,
    ) -> PgResult<()> {
        prologue::prologue_begin(machine, assigner)
    }
    fn prologue_end<'m, M: TargetMachine<'m, A, 2, Target = Self>, C: CallingConventionAssigner>(
        machine: &M,
        assigner: &mut C,
    ) -> PgResult<()> {
        prologue::prologue_end(machine, assigner)
    }
    fn prologue_assign_arg_part<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        part: ValuePartRef<'_, M, 2>,
        assignment: CCAssignment,
    ) -> PgResult<Option<i32>> {
        prologue::prologue_assign_arg_part(machine, part, assignment)
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
        part: ValuePartRef<'_, M, 2>,
    ) -> PgResult<()> {
        values::alloca_fixed(machine, u64::from(size), alignment, part)
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
        values::load_address_of_stack_var(machine, destination, local, part)
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
        values::spill_reg(machine, reg, offset, size)
    }
    fn load_from_stack<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        reg: Reg,
        offset: i32,
        size: u32,
    ) -> PgResult<()> {
        values::load_from_stack(machine, reg, offset, size, false)
    }
    fn mov<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        destination: Reg,
        source: Reg,
        size: u32,
    ) -> PgResult<()> {
        raw::mov(machine, destination, source, size)
    }
    fn materialize_constant<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        words: &[u64],
        bank: RegBank,
        size: u32,
        reg: Reg,
    ) -> PgResult<()> {
        raw::materialize_constant(machine, words, bank, size, reg)
    }
    fn generate_raw_intext<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        destination: Reg,
        source: Reg,
        sign: bool,
        from: u32,
        to: u32,
    ) -> PgResult<()> {
        raw::generate_raw_intext(machine, destination, source, sign, from, to)
    }
    fn gval_expr_as_reg<'s, 'm, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &'s M,
        value: &mut GenericValuePart<'s, M, 2>,
    ) -> PgResult<Reg> {
        values::gval_expr_as_reg(machine, value)
    }
    fn generate_raw_jump<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        jump: Jump,
        label: Label,
    ) -> PgResult<()> {
        raw::generate_raw_jump(machine, jump, label)
    }
    fn switch_emit_cmpeq<'m, M: TargetMachine<'m, A, 2, Target = Self>>(
        machine: &M,
        label: Label,
        cmp: Reg,
        tmp: Reg,
        value: u64,
        width32: bool,
    ) -> PgResult<()> {
        raw::switch_emit_cmpeq(machine, label, cmp, tmp, value, width32)
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
        raw::switch_emit_binary_step(machine, equal, greater, cmp, tmp, value, width32)
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
        raw::switch_create_jump_table(machine, default, cmp, tmp, low, high, width32, fill)
    }
    fn gen_func_epilog<'m, M: TargetMachine<'m, A, 2, Target = Self>>(machine: &M) -> PgResult<()> {
        prologue::gen_func_epilog(machine)
    }
}
