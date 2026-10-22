// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use tpde_base::ValLocalIdx;
use tpde_core::{
    calling_convention::{CCAssignment, CCAssignmentPolicy, CCInfo},
    ir_adaptor::{HighestValueIndexProvider, IrAdaptor, ProvidesHighestValue},
    Reg,
};
use tpde_test_ir::{IRBlockRef, IRFuncRef, IRInstRef, IRValueRef, TestIR, TestIRAdaptor};

pub const MAX_STATIC_ALLOCA_SIZE: u32 = 256;

impl tpde_compiler::compiler_base::CompilerAdaptor for Adaptor<'_, '_> {
    fn compiler_insts(&self, block: Self::BlockRef) -> impl Iterator<Item = Self::InstRef> + Clone {
        self.block_insts(block).into_iter()
    }
}

pub struct Adaptor<'a, 'm> {
    pub inner: TestIRAdaptor<'a, 'm>,
}

impl<'a, 'm> Adaptor<'a, 'm> {
    pub fn new(ir: &'a TestIR<'m>) -> Self {
        Self {
            inner: TestIRAdaptor::new(ir),
        }
    }

    pub fn ir(&self) -> &'a TestIR<'m> {
        self.inner.ir
    }

    pub fn current_function(&self) -> u32 {
        self.inner.cur_func
    }

    fn argument_name(&self, index: u32) -> &str {
        let function = &self.inner.ir.functions[self.inner.cur_func as usize];
        let value = &self.inner.ir.values[(function.arg_begin_idx + index) as usize];
        self.inner.ir.name(value.name)
    }
}

impl HighestValueIndexProvider for Adaptor<'_, '_> {
    fn cur_highest_val_idx(&self) -> u32 {
        self.inner.cur_highest_val_idx()
    }
}

impl<'a, 'm> IrAdaptor for Adaptor<'a, 'm> {
    type ValueRef = IRValueRef;
    type InstRef = IRInstRef;
    type BlockRef = IRBlockRef;
    type FuncRef = IRFuncRef;
    type HighestValue = ProvidesHighestValue;

    const INVALID_VALUE_REF: IRValueRef = TestIRAdaptor::INVALID_VALUE_REF;
    const INVALID_BLOCK_REF: IRBlockRef = TestIRAdaptor::INVALID_BLOCK_REF;
    const INVALID_FUNC_REF: IRFuncRef = TestIRAdaptor::INVALID_FUNC_REF;
    const TPDE_LIVENESS_VISIT_ARGS: bool = TestIRAdaptor::TPDE_LIVENESS_VISIT_ARGS;

    type Funcs = <TestIRAdaptor<'a, 'm> as IrAdaptor>::Funcs;
    type FuncsToCompile = <TestIRAdaptor<'a, 'm> as IrAdaptor>::FuncsToCompile;
    type Args<'b>
        = <TestIRAdaptor<'a, 'm> as IrAdaptor>::Args<'b>
    where
        Self: 'b;
    type StaticAllocas<'b>
        = <TestIRAdaptor<'a, 'm> as IrAdaptor>::StaticAllocas<'b>
    where
        Self: 'b;
    type Blocks<'b>
        = <TestIRAdaptor<'a, 'm> as IrAdaptor>::Blocks<'b>
    where
        Self: 'b;
    type Successors<'b>
        = <TestIRAdaptor<'a, 'm> as IrAdaptor>::Successors<'b>
    where
        Self: 'b;
    type Insts<'b>
        = <TestIRAdaptor<'a, 'm> as IrAdaptor>::Insts<'b>
    where
        Self: 'b;
    type Phis<'b>
        = <TestIRAdaptor<'a, 'm> as IrAdaptor>::Phis<'b>
    where
        Self: 'b;
    type Operands<'b>
        = <TestIRAdaptor<'a, 'm> as IrAdaptor>::Operands<'b>
    where
        Self: 'b;
    type Results<'b>
        = <TestIRAdaptor<'a, 'm> as IrAdaptor>::Results<'b>
    where
        Self: 'b;
    type Phi<'b>
        = <TestIRAdaptor<'a, 'm> as IrAdaptor>::Phi<'b>
    where
        Self: 'b;
    type BlockFormat<'b>
        = <TestIRAdaptor<'a, 'm> as IrAdaptor>::BlockFormat<'b>
    where
        Self: 'b;
    type ValueFormat<'b>
        = <TestIRAdaptor<'a, 'm> as IrAdaptor>::ValueFormat<'b>
    where
        Self: 'b;
    type InstFormat<'b>
        = <TestIRAdaptor<'a, 'm> as IrAdaptor>::InstFormat<'b>
    where
        Self: 'b;

    fn func_count(&self) -> u32 {
        self.inner.func_count()
    }

    fn funcs(&self) -> Self::Funcs {
        self.inner.funcs()
    }

    fn funcs_to_compile(&self) -> Self::FuncsToCompile {
        self.inner.funcs_to_compile()
    }

    fn func_link_name(&self, func: IRFuncRef) -> &[u8] {
        self.inner.func_link_name(func)
    }

    fn func_extern(&self, func: IRFuncRef) -> bool {
        self.inner.func_extern(func)
    }

    fn func_only_local(&self, func: IRFuncRef) -> bool {
        self.inner.func_only_local(func)
    }

    fn func_has_weak_linkage(&self, func: IRFuncRef) -> bool {
        self.inner.func_has_weak_linkage(func)
    }

    fn cur_needs_unwind_info(&self) -> bool {
        self.inner.cur_needs_unwind_info()
    }

    fn cur_is_vararg(&self) -> bool {
        self.inner.cur_is_vararg()
    }

    fn cur_args(&self) -> Self::Args<'_> {
        self.inner.cur_args()
    }

    fn cur_arg_is_byval(&self, index: u32) -> bool {
        self.argument_name(index).starts_with("byval_")
    }

    fn cur_arg_byval_size(&self, _: u32) -> u32 {
        24
    }

    fn cur_arg_byval_align(&self, _: u32) -> u32 {
        8
    }

    fn cur_arg_is_sret(&self, index: u32) -> bool {
        self.argument_name(index).starts_with("sret_")
    }

    fn cur_static_allocas(&self) -> Self::StaticAllocas<'_> {
        self.inner.cur_static_allocas()
    }

    fn cur_has_dynamic_alloca(&self) -> bool {
        self.inner.cur_has_dynamic_alloca()
    }

    fn cur_entry_block(&self) -> IRBlockRef {
        self.inner.cur_entry_block()
    }

    fn cur_blocks(&self) -> Self::Blocks<'_> {
        self.inner.cur_blocks()
    }

    fn block_succs(&self, block: IRBlockRef) -> Self::Successors<'_> {
        self.inner.block_succs(block)
    }

    fn block_insts(&self, block: IRBlockRef) -> Self::Insts<'_> {
        self.inner.block_insts(block)
    }

    fn block_phis(&self, block: IRBlockRef) -> Self::Phis<'_> {
        self.inner.block_phis(block)
    }

    fn block_info(&self, block: IRBlockRef) -> u32 {
        self.inner.block_info(block)
    }

    fn block_set_info(&self, block: IRBlockRef, info: u32) {
        self.inner.block_set_info(block, info);
    }

    fn block_info2(&self, block: IRBlockRef) -> u32 {
        self.inner.block_info2(block)
    }

    fn block_set_info2(&self, block: IRBlockRef, info: u32) {
        self.inner.block_set_info2(block, info);
    }

    fn block_fmt_ref(&self, block: IRBlockRef) -> Self::BlockFormat<'_> {
        self.inner.block_fmt_ref(block)
    }

    fn val_local_idx(&self, value: IRValueRef) -> ValLocalIdx {
        self.inner.val_local_idx(value)
    }

    fn val_ignore_in_liveness_analysis(&self, value: IRValueRef) -> bool {
        self.inner.val_ignore_in_liveness_analysis(value)
            && self.val_alloca_size(value) <= MAX_STATIC_ALLOCA_SIZE
            && self.val_alloca_align(value) <= 16
    }

    fn val_is_phi(&self, value: IRValueRef) -> bool {
        self.inner.val_is_phi(value)
    }

    fn val_as_phi(&self, value: IRValueRef) -> Self::Phi<'_> {
        self.inner.val_as_phi(value)
    }

    fn val_alloca_size(&self, value: IRValueRef) -> u32 {
        self.inner.val_alloca_size(value)
    }

    fn val_alloca_align(&self, value: IRValueRef) -> u32 {
        self.inner.val_alloca_align(value)
    }

    fn value_fmt_ref(&self, value: IRValueRef) -> Self::ValueFormat<'_> {
        self.inner.value_fmt_ref(value)
    }

    fn inst_operands(&self, inst: IRInstRef) -> Self::Operands<'_> {
        self.inner.inst_operands(inst)
    }

    fn inst_results(&self, inst: IRInstRef) -> Self::Results<'_> {
        self.inner.inst_results(inst)
    }

    fn inst_fused(&self, inst: IRInstRef) -> bool {
        self.inner.inst_fused(inst)
    }

    fn inst_fmt_ref(&self, inst: IRInstRef) -> Self::InstFormat<'_> {
        self.inner.inst_fmt_ref(inst)
    }

    fn start_compile(&mut self) {
        self.inner.start_compile();
    }

    fn end_compile(&mut self) {
        self.inner.end_compile();
    }

    fn switch_func(&mut self, func: IRFuncRef) -> bool {
        self.inner.switch_func(func)
    }

    fn reset(&mut self) {
        self.inner.reset();
    }
}

pub const fn cc_info() -> CCInfo {
    CCInfo::with_red_zone(
        0x0000_ffff_0000_ffff,
        0x0000_ff00_0000_ff00,
        0x0000_000f_0000_000f,
        32,
    )
}

#[derive(Default)]
pub struct Policy {
    next: [u32; 2],
    returns: [u32; 2],
    stack_size: u32,
}

impl Policy {
    pub const fn new() -> Self {
        Self {
            next: [0; 2],
            returns: [0; 2],
            stack_size: 0,
        }
    }
}

impl CCAssignmentPolicy for Policy {
    fn reset(&mut self) {
        *self = Self::new();
    }

    fn assign_arg(&mut self, assignment: &mut CCAssignment) {
        let bank = if assignment.byval() {
            0
        } else {
            assignment.bank.id() as usize
        };
        assert!(bank < 2);
        if assignment.sret() {
            assignment.reg = Reg::new(3);
            self.next[0] = 4;
        } else if !assignment.byval()
            && self.next[bank] + u32::from(assignment.consecutive) + 1 <= 4
        {
            assignment.reg = Reg::new(bank as u64 * 32 + u64::from(self.next[bank]));
            self.next[bank] += 1;
        } else {
            if !assignment.byval() {
                self.next[bank] = 4;
            }
            let align = u32::from(assignment.align).max(1);
            assert!(align.is_power_of_two());
            self.stack_size = self.stack_size.wrapping_add(align - 1) & !(align - 1);
            assignment.reg = Reg::make_invalid();
            assignment.stack_off = self.stack_size;
            self.stack_size = self.stack_size.wrapping_add(assignment.size);
        }
    }

    fn get_stack_size(&mut self) -> u32 {
        self.stack_size
    }

    fn assign_ret(&mut self, assignment: &mut CCAssignment) {
        let bank = assignment.bank.id() as usize;
        assert!(bank < 2 && self.returns[bank] < 4);
        assignment.reg = Reg::new(bank as u64 * 32 + u64::from(self.returns[bank]));
        self.returns[bank] += 1;
    }
}
