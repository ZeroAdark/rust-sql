// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{
    branches::BranchMachine, calls::CallMachine, compiler_base::CompilerAdaptor,
    generic_value_part::GenericValuePart,
    guard_machine::{GuardMachine, RegallocEvent},
    value_part_ref::ValuePartRef, value_ref::ValueRef,
};
use tpde_assembler::{Architecture, AssemblerElf, SecRef, SymRef};
use tpde_base::ValLocalIdx;
use tpde_core::{
    calling_convention::{CCAssignment, CCAssignmentPolicy, CCInfo, CallingConventionAssigner},
    compiler::{ValRefSpecialStruct, ValueParts},
    Reg, RegBank,
};
use tpde_function_writer::{
    FunctionWriterBase, FunctionWriterJumpTables, Label, LabelFixupKind, ModuleWriter,
};
use types_error::PgResult;

/// ```compile_fail
/// use tpde_compiler::target::CompilerOutput;
/// use tpde_function_writer::{FunctionWriterTarget, ModuleWriter};
/// fn requires_output<'m>(_: &mut impl CompilerOutput<'m>) {}
/// fn without_jump_tables<'m, T: FunctionWriterTarget>(writer: &mut ModuleWriter<'_, '_, 'm, T>) {
///     requires_output(writer);
/// }
/// ```
pub trait CompilerOutput<'m> {
    fn with_base<R>(
        &mut self,
        operation: impl FnOnce(&mut FunctionWriterBase<'_, '_, 'm>) -> R,
    ) -> R;
    fn assembler(&self) -> &AssemblerElf<'m>;
    fn assembler_mut(&mut self) -> &mut AssemblerElf<'m>;
    fn begin_func(&mut self, alignment: u32, expected_size: u32) -> PgResult<()>;
    fn finish_func(&mut self) -> PgResult<()>;
    fn label_create(&mut self) -> PgResult<Label>;
    fn label_place(&mut self, label: Label) -> PgResult<()>;
    fn label_ref(&mut self, label: Label, offset: u32, kind: LabelFixupKind) -> PgResult<()>;
    fn eh_advance(&mut self, size: u64) -> PgResult<()>;
    fn align(&mut self, alignment: usize) -> PgResult<()>;
    fn ensure_space(&mut self, size: usize) -> PgResult<()>;
    fn skip(&mut self, size: usize) -> PgResult<()>;
    fn switch_section(&mut self, section: SecRef) -> PgResult<()>;
    fn flush(&mut self) -> PgResult<()>;
    fn offset(&self) -> usize;
    fn section(&self) -> SecRef;
    fn write_bytes(&mut self, bytes: &[u8]) -> PgResult<()>;
    fn jump_table(
        &mut self,
        size: u32,
        index: Reg,
        temporary: Reg,
        index_is_32: bool,
        fill: impl FnOnce(&mut [Label]),
    ) -> PgResult<()>;
}
impl<'m, T: FunctionWriterJumpTables> CompilerOutput<'m> for ModuleWriter<'_, '_, 'm, T> {
    fn with_base<R>(
        &mut self,
        operation: impl FnOnce(&mut FunctionWriterBase<'_, '_, 'm>) -> R,
    ) -> R {
        operation(&mut self.base)
    }
    fn assembler(&self) -> &AssemblerElf<'m> {
        self.base.assembler()
    }
    fn assembler_mut(&mut self) -> &mut AssemblerElf<'m> {
        self.base.assembler_mut()
    }
    fn begin_func(&mut self, alignment: u32, expected_size: u32) -> PgResult<()> {
        ModuleWriter::begin_func(self, alignment, expected_size)
    }
    fn finish_func(&mut self) -> PgResult<()> {
        ModuleWriter::finish_func(self)
    }
    fn label_create(&mut self) -> PgResult<Label> {
        self.base.label_create()
    }
    fn label_place(&mut self, label: Label) -> PgResult<()> {
        let offset = u32::try_from(self.base.offset()).expect("text offset exceeds upstream u32");
        self.base.label_place(label, offset)
    }
    fn label_ref(&mut self, label: Label, offset: u32, kind: LabelFixupKind) -> PgResult<()> {
        ModuleWriter::label_ref(self, label, offset, kind)
    }
    fn eh_advance(&mut self, size: u64) -> PgResult<()> {
        ModuleWriter::eh_advance(self, size)
    }
    fn align(&mut self, alignment: usize) -> PgResult<()> {
        ModuleWriter::align(self, alignment)
    }
    #[inline(always)]
    fn ensure_space(&mut self, size: usize) -> PgResult<()> {
        ModuleWriter::ensure_space(self, size)
    }
    fn skip(&mut self, size: usize) -> PgResult<()> {
        ModuleWriter::skip(self, size)
    }
    fn switch_section(&mut self, section: SecRef) -> PgResult<()> {
        self.base.switch_section(section)
    }
    fn flush(&mut self) -> PgResult<()> {
        self.base.flush()
    }
    #[inline(always)]
    fn offset(&self) -> usize {
        self.base.offset()
    }
    fn section(&self) -> SecRef {
        self.base.get_sec_ref()
    }
    #[inline(always)]
    fn write_bytes(&mut self, bytes: &[u8]) -> PgResult<()> {
        ModuleWriter::write_bytes(self, bytes)
    }
    fn jump_table(
        &mut self,
        size: u32,
        index: Reg,
        temporary: Reg,
        index_is_32: bool,
        fill: impl FnOnce(&mut [Label]),
    ) -> PgResult<()> {
        ModuleWriter::create_jump_table(self, size, index, temporary, index_is_32, fill)
    }
}

pub trait TargetMachine<'m, A: CompilerAdaptor, const N: usize>:
    GuardMachine<N, Special = <Self::Target as CompilerTarget<A, N>>::Special>
    + BranchMachine<N, Adaptor = A, Jump = <Self::Target as CompilerTarget<A, N>>::Jump>
    + CallMachine<N, CallAdaptor = A>
{
    type Target: CompilerTarget<A, N>;
    type Output: CompilerOutput<'m>;
    type Assigner: CallingConventionAssigner;
    fn with_cur_cc_assigner<R>(&self, operation: impl FnOnce(&mut Self::Assigner) -> R) -> R;
    fn with_target<R>(&self, operation: impl FnOnce(&mut Self::Target) -> R) -> R;
    fn with_output<R>(&self, operation: impl FnOnce(&mut Self::Output) -> R) -> R;
    fn frame_size(&self) -> u32;
    fn set_frame_size(&self, size: u32);
    fn frame_has_dynamic_alloca(&self) -> bool;
    fn frame_generated_call(&self) -> bool;
    fn frame_used(&self) -> bool;
    fn set_frame_used(&self, used: bool);
    fn function_symbol(&self, index: u32) -> SymRef;
    fn personality_symbol(&self) -> PgResult<SymRef>;
    fn init_variable_ref(&self, local: ValLocalIdx, data: u32) -> PgResult<()>;
    fn init_assignment(&self, value: A::ValueRef, local: ValLocalIdx) -> PgResult<()>;
    fn assignment_slot_count(&self) -> usize;
    fn fixed_assignment_count(&self, bank: RegBank) -> u32;
    fn result_ref_alias(
        &self,
        value: A::ValueRef,
        source: ValueRef<'_, Self, N>,
    ) -> PgResult<ValueRef<'_, Self, N>>;
    fn result_ref_stack_slot(
        &self,
        value: A::ValueRef,
        base_local: ValLocalIdx,
        base_part: u32,
        offset: i32,
    ) -> PgResult<ValueRef<'_, Self, N>>;
    fn with_value_ref_single<R>(
        &self,
        value: A::ValueRef,
        operation: impl FnOnce(&ValueRef<'_, Self, N>, ValuePartRef<'_, Self, N>) -> R,
    ) -> PgResult<R>;
    fn with_result_ref_single<R>(
        &self,
        value: A::ValueRef,
        operation: impl FnOnce(&ValueRef<'_, Self, N>, ValuePartRef<'_, Self, N>) -> R,
    ) -> PgResult<R>;
    fn evict_part(&self, local: ValLocalIdx, part: u32) -> PgResult<()>;
    fn reloc_text(&self, symbol: SymRef, kind: u32, offset: u64, addend: i64) -> PgResult<()>;
}

pub trait CompilerTarget<A: CompilerAdaptor, const N: usize>: Sized {
    type Special: ValRefSpecialStruct;
    type Parts: ValueParts;
    type Jump: Copy;
    type Writer: FunctionWriterJumpTables;
    type CCPolicy: CCAssignmentPolicy;
    const ARCHITECTURE: Architecture;
    const FRAME_INDEXING_NEGATIVE: bool;
    const PLATFORM_POINTER_SIZE: u32;
    const GP_BANK: RegBank;
    const FIXED_ASSIGNMENTS: [u32; N];
    const MAX_STATIC_ALLOCA_SIZE: u32;
    const DEFAULT_VAR_REF_HANDLING: bool;
    fn reset(&mut self) {}
    fn cc_info(&self, adaptor: &A) -> CCInfo;
    fn new_cc_policy(&self, adaptor: &A) -> Self::CCPolicy;
    fn val_parts(&self, adaptor: &A, value: A::ValueRef) -> Self::Parts;
    fn val_ref_special(&self, adaptor: &A, value: A::ValueRef) -> Option<Self::Special>;
    fn cur_func_may_emit_calls(&self, adaptor: &A) -> bool;
    fn cur_personality_func(&self, adaptor: &A) -> SymRef;
    fn define_func_idx(&mut self, adaptor: &A, function: A::FuncRef, index: u32);
    fn hook_post_func_sym_init<'m>(
        &mut self,
        adaptor: &mut A,
        assembler: &mut AssemblerElf<'m>,
        symbols: &[SymRef],
    ) -> PgResult<bool> {
        let _ = (adaptor, assembler, symbols);
        Ok(true)
    }
    fn before_function<'m, O: CompilerOutput<'m>>(
        &mut self,
        adaptor: &mut A,
        output: &mut O,
        function: A::FuncRef,
        index: u32,
        symbol: SymRef,
    ) -> PgResult<bool> {
        let _ = (adaptor, output, function, index, symbol);
        Ok(true)
    }
    fn after_function<'m, O: CompilerOutput<'m>>(
        &mut self,
        adaptor: &mut A,
        output: &mut O,
        function: A::FuncRef,
        index: u32,
        success: bool,
    ) -> PgResult<bool> {
        let _ = (adaptor, output, function, index);
        Ok(success)
    }
    fn analysis_start(&mut self, adaptor: &A) {
        let _ = adaptor;
    }
    fn analysis_end(&mut self, adaptor: &A) {
        let _ = adaptor;
    }
    fn try_force_fixed_assignment(&self, adaptor: &A, value: A::ValueRef) -> bool {
        let _ = (adaptor, value);
        false
    }
    /// Phi coalescing at the edge (`docs/design/gravity-simple/tpde-phi-coalescing.md`):
    /// the first compiled edge into a block pins each phi to the register
    /// its incoming value is in; later edges move into it.
    const PHI_COALESCING: bool = false;
    /// Static spill weights (`LivenessInfo::weight`, or `value_weight`)
    /// decide eviction order and fixed-assignment preference.
    const WEIGHTED_REGALLOC: bool = false;
    /// Budget of pinned (coalesced) phis per bank, separate from
    /// `FIXED_ASSIGNMENTS`.
    const PINNED_ASSIGNMENTS: [u32; N] = [0; N];
    /// A value defined in a block and passed by that block's terminator to
    /// a phi whose register is already fixed is computed into the phi's
    /// register when the phi has no other reference left but the edges
    /// from the current and later blocks (`CompilerAdaptor::phi_reuse`,
    /// `FunctionSession::shadow_phi`): the back-edge move disappears.
    const PHI_REUSE: bool = false;
    /// The compiler emitted code for a register-allocation event in
    /// `block` (`RegallocEvent`); a frontend that accounts frame traffic
    /// records it. Default: nothing.
    fn regalloc_event(&self, adaptor: &A, event: RegallocEvent, block: A::BlockRef) {
        let _ = (adaptor, event, block);
    }
    /// Frontend spill weight for a value; `None` uses the analyzer's.
    fn value_weight(&self, adaptor: &A, value: A::ValueRef) -> Option<u32> {
        let _ = (adaptor, value);
        None
    }
    /// Registers of `bank` a pinned phi may take: the callee-saved ones when
    /// the function calls and the phi's range crosses a hot call; any
    /// caller-saved register that is not an argument or return register
    /// when it crosses only cold calls (the call sequence saves and
    /// restores it, `calls.rs`); otherwise any allocatable register the
    /// target does not reserve.
    fn pin_reg_mask<'m, M: TargetMachine<'m, A, N, Target = Self>>(
        machine: &M,
        bank: RegBank,
        crosses_call: bool,
        crosses_cold_call: bool,
        value: A::ValueRef,
    ) -> u64 {
        let _ = (machine, bank, crosses_call, crosses_cold_call, value);
        0
    }
    fn unconditional_jump() -> Self::Jump;
    fn invert_jump(jump: Self::Jump) -> Self::Jump;
    fn special_part<'s, 'm, M: TargetMachine<'m, A, N, Target = Self, Special = Self::Special>>(
        machine: &'s M,
        special: &'s Self::Special,
        part: u32,
    ) -> ValuePartRef<'s, M, N>
    where
        A: 's;
    fn select_fixed_assignment_reg<'m, M: TargetMachine<'m, A, N, Target = Self>>(
        machine: &M,
        local: ValLocalIdx,
        part: u32,
        value: A::ValueRef,
    ) -> Reg;
    fn start_func<'m, M: TargetMachine<'m, A, N, Target = Self>>(
        machine: &M,
        index: u32,
    ) -> PgResult<()>;
    fn finish_func<'m, M: TargetMachine<'m, A, N, Target = Self>>(
        machine: &M,
        index: u32,
    ) -> PgResult<()>;
    fn prologue_begin<'m, M: TargetMachine<'m, A, N, Target = Self>, C: CallingConventionAssigner>(
        machine: &M,
        assigner: &mut C,
    ) -> PgResult<()>;
    fn prologue_assign_arg<
        'm,
        M: TargetMachine<'m, A, N, Target = Self>,
        C: CallingConventionAssigner,
    >(
        machine: &M,
        assigner: &mut C,
        index: u32,
        value: &A::ValueRef,
    ) -> PgResult<()> {
        crate::calls::prologue_assign_arg(machine, assigner, index, value, 1, false)
    }
    fn prologue_end<'m, M: TargetMachine<'m, A, N, Target = Self>, C: CallingConventionAssigner>(
        machine: &M,
        assigner: &mut C,
    ) -> PgResult<()>;
    fn prologue_assign_arg_part<'m, M: TargetMachine<'m, A, N, Target = Self>>(
        machine: &M,
        part: ValuePartRef<'_, M, N>,
        assignment: CCAssignment,
    ) -> PgResult<Option<i32>>;
    fn compile_inst<'m, M: TargetMachine<'m, A, N, Target = Self>>(
        machine: &M,
        inst: A::InstRef,
        remaining: impl Iterator<Item = A::InstRef> + Clone,
    ) -> PgResult<bool>;
    fn alloca_fixed<'m, M: TargetMachine<'m, A, N, Target = Self>>(
        machine: &M,
        size: u32,
        alignment: u32,
        part: ValuePartRef<'_, M, N>,
    ) -> PgResult<()>;
    fn setup_var_ref_assignments<'m, M: TargetMachine<'m, A, N, Target = Self>>(
        machine: &M,
    ) -> PgResult<()>;
    fn load_address_of_stack_var<'m, M: TargetMachine<'m, A, N, Target = Self>>(
        machine: &M,
        destination: Reg,
        local: ValLocalIdx,
        part: u32,
    ) -> PgResult<()>;
    fn load_address_of_var_reference<'m, M: TargetMachine<'m, A, N, Target = Self>>(
        machine: &M,
        destination: Reg,
        local: ValLocalIdx,
        part: u32,
    ) -> PgResult<()>;
    fn spill_reg<'m, M: TargetMachine<'m, A, N, Target = Self>>(
        machine: &M,
        reg: Reg,
        offset: i32,
        size: u32,
    ) -> PgResult<()>;
    fn load_from_stack<'m, M: TargetMachine<'m, A, N, Target = Self>>(
        machine: &M,
        reg: Reg,
        offset: i32,
        size: u32,
    ) -> PgResult<()>;
    fn mov<'m, M: TargetMachine<'m, A, N, Target = Self>>(
        machine: &M,
        destination: Reg,
        source: Reg,
        size: u32,
    ) -> PgResult<()>;
    fn materialize_constant<'m, M: TargetMachine<'m, A, N, Target = Self>>(
        machine: &M,
        words: &[u64],
        bank: RegBank,
        size: u32,
        reg: Reg,
    ) -> PgResult<()>;
    fn generate_raw_intext<'m, M: TargetMachine<'m, A, N, Target = Self>>(
        machine: &M,
        destination: Reg,
        source: Reg,
        sign: bool,
        from: u32,
        to: u32,
    ) -> PgResult<()>;
    fn gval_expr_as_reg<'s, 'm, M: TargetMachine<'m, A, N, Target = Self>>(
        machine: &'s M,
        value: &mut GenericValuePart<'s, M, N>,
    ) -> PgResult<Reg>;
    fn generate_raw_jump<'m, M: TargetMachine<'m, A, N, Target = Self>>(
        machine: &M,
        jump: Self::Jump,
        label: Label,
    ) -> PgResult<()>;
    fn switch_emit_cmpeq<'m, M: TargetMachine<'m, A, N, Target = Self>>(
        machine: &M,
        label: Label,
        cmp: Reg,
        tmp: Reg,
        value: u64,
        width32: bool,
    ) -> PgResult<()>;
    fn switch_emit_binary_step<'m, M: TargetMachine<'m, A, N, Target = Self>>(
        machine: &M,
        equal: Label,
        greater: Label,
        cmp: Reg,
        tmp: Reg,
        value: u64,
        width32: bool,
    ) -> PgResult<()>;
    fn switch_create_jump_table<'m, M: TargetMachine<'m, A, N, Target = Self>>(
        machine: &M,
        default: Label,
        cmp: Reg,
        tmp: Reg,
        low: u64,
        high: u64,
        width32: bool,
        fill: impl FnOnce(&mut [Label]),
    ) -> PgResult<bool>;
    fn gen_func_epilog<'m, M: TargetMachine<'m, A, N, Target = Self>>(machine: &M) -> PgResult<()>;
}
