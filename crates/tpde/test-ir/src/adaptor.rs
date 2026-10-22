// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
use crate::{Op, TestIR, Value, ValueType};
use core::{iter::FusedIterator, marker::PhantomData, ops::Range, slice};
use tpde_base::ValLocalIdx;
use tpde_core::ir_adaptor::{HighestValueIndexProvider, IrAdaptor, PhiRef, ProvidesHighestValue};

macro_rules! ir_refs {
    ($($name:ident),+ $(,)?) => {$(
        #[repr(transparent)]
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub u32);

        impl From<u32> for $name {
            #[inline]
            fn from(value: u32) -> Self { Self(value) }
        }

        impl From<$name> for u32 {
            #[inline]
            fn from(value: $name) -> Self { value.0 }
        }

        mcx::forget_safe_nodrop!($name);
        const _: () = assert!(core::mem::size_of::<$name>() == 4);
    )+};
}
ir_refs!(IRValueRef, IRInstRef, IRBlockRef, IRFuncRef);

impl From<IRInstRef> for IRValueRef {
    #[inline]
    fn from(inst: IRInstRef) -> Self {
        Self(inst.0)
    }
}

#[derive(Clone)]
pub struct RefRange<R> {
    indices: Range<u32>,
    marker: PhantomData<R>,
}

impl<R> RefRange<R> {
    fn new(begin: u32, end: u32) -> Self {
        Self {
            indices: begin..end,
            marker: PhantomData,
        }
    }
}

impl<R: From<u32>> Iterator for RefRange<R> {
    type Item = R;

    #[inline]
    fn next(&mut self) -> Option<R> {
        self.indices.next().map(R::from)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.indices.size_hint()
    }
}

impl<R: From<u32>> DoubleEndedIterator for RefRange<R> {
    #[inline]
    fn next_back(&mut self) -> Option<R> {
        self.indices.next_back().map(R::from)
    }
}

impl<R: From<u32>> ExactSizeIterator for RefRange<R> {}
impl<R: From<u32>> FusedIterator for RefRange<R> {}

#[derive(Clone)]
pub struct RefSlice<'a, R> {
    indices: slice::Iter<'a, u32>,
    marker: PhantomData<R>,
}

impl<'a, R> RefSlice<'a, R> {
    fn new(indices: &'a [u32]) -> Self {
        Self {
            indices: indices.iter(),
            marker: PhantomData,
        }
    }
}

impl<R: From<u32>> Iterator for RefSlice<'_, R> {
    type Item = R;

    #[inline]
    fn next(&mut self) -> Option<R> {
        self.indices.next().copied().map(R::from)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.indices.size_hint()
    }
}

impl<R: From<u32>> DoubleEndedIterator for RefSlice<'_, R> {
    #[inline]
    fn next_back(&mut self) -> Option<R> {
        self.indices.next_back().copied().map(R::from)
    }
}

impl<R: From<u32>> ExactSizeIterator for RefSlice<'_, R> {}
impl<R: From<u32>> FusedIterator for RefSlice<'_, R> {}

pub struct StaticAllocas<'a> {
    values: &'a [Value],
    indices: Range<u32>,
}

impl Iterator for StaticAllocas<'_> {
    type Item = IRValueRef;

    #[inline]
    fn next(&mut self) -> Option<IRValueRef> {
        for index in self.indices.by_ref() {
            if self.values[index as usize].op == Op::Alloca {
                return Some(IRValueRef(index));
            }
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, self.indices.size_hint().1)
    }
}

impl FusedIterator for StaticAllocas<'_> {}

#[derive(Clone, Copy)]
pub struct Phi<'a> {
    values: &'a [u32],
    blocks: &'a [u32],
}

impl PhiRef<IRValueRef, IRBlockRef> for Phi<'_> {
    #[inline]
    fn incoming_count(&self) -> u32 {
        self.values.len() as u32
    }

    #[inline]
    fn incoming_val_for_slot(&self, slot: u32) -> IRValueRef {
        IRValueRef(self.values[slot as usize])
    }

    #[inline]
    fn incoming_block_for_slot(&self, slot: u32) -> IRBlockRef {
        IRBlockRef(self.blocks[slot as usize])
    }

    fn incoming_val_for_block(&self, block: IRBlockRef) -> IRValueRef {
        self.blocks
            .iter()
            .position(|&incoming| incoming == block.0)
            .map_or(IRValueRef(u32::MAX), |slot| IRValueRef(self.values[slot]))
    }
}

pub struct TestIRAdaptor<'a, 'm> {
    pub ir: &'a TestIR<'m>,
    pub highest_local_val_idx: u32,
    pub cur_func: u32,
    static_alloca_limit: Option<u32>,
}

impl<'a, 'm> TestIRAdaptor<'a, 'm> {
    pub const TPDE_PROVIDES_HIGHEST_VAL_IDX: bool = true;

    pub fn new(ir: &'a TestIR<'m>) -> Self {
        Self {
            ir,
            highest_local_val_idx: 0,
            cur_func: u32::MAX,
            static_alloca_limit: None,
        }
    }

    /// Native compilation needs liveness for sizes above `max_size` or alignment above 16.
    /// `max_size` must equal the target's `MAX_STATIC_ALLOCA_SIZE`.
    /// The default adaptor ignores all allocas.
    pub fn with_static_alloca_limit(mut self, max_size: u32) -> Self {
        self.static_alloca_limit = Some(max_size);
        self
    }
}

impl HighestValueIndexProvider for TestIRAdaptor<'_, '_> {
    #[inline]
    fn cur_highest_val_idx(&self) -> u32 {
        self.highest_local_val_idx
    }
}

impl tpde_compiler::compiler_base::CompilerAdaptor for TestIRAdaptor<'_, '_> {
    fn compiler_insts(&self, block: Self::BlockRef) -> impl Iterator<Item = Self::InstRef> + Clone {
        self.block_insts(block).into_iter()
    }
}

impl IrAdaptor for TestIRAdaptor<'_, '_> {
    type ValueRef = IRValueRef;
    type InstRef = IRInstRef;
    type BlockRef = IRBlockRef;
    type FuncRef = IRFuncRef;
    type HighestValue = ProvidesHighestValue;

    const INVALID_VALUE_REF: IRValueRef = IRValueRef(u32::MAX);
    const INVALID_BLOCK_REF: IRBlockRef = IRBlockRef(u32::MAX);
    const INVALID_FUNC_REF: IRFuncRef = IRFuncRef(u32::MAX);
    const TPDE_LIVENESS_VISIT_ARGS: bool = true;

    type Funcs = RefRange<IRFuncRef>;
    type FuncsToCompile = RefRange<IRFuncRef>;
    type Args<'a>
        = RefRange<IRValueRef>
    where
        Self: 'a;
    type StaticAllocas<'a>
        = StaticAllocas<'a>
    where
        Self: 'a;
    type Blocks<'a>
        = RefRange<IRBlockRef>
    where
        Self: 'a;
    type Successors<'a>
        = RefSlice<'a, IRBlockRef>
    where
        Self: 'a;
    type Insts<'a>
        = RefRange<IRInstRef>
    where
        Self: 'a;
    type Phis<'a>
        = RefRange<IRValueRef>
    where
        Self: 'a;
    type Operands<'a>
        = RefSlice<'a, IRValueRef>
    where
        Self: 'a;
    type Results<'a>
        = core::option::IntoIter<IRValueRef>
    where
        Self: 'a;
    type Phi<'a>
        = Phi<'a>
    where
        Self: 'a;
    type BlockFormat<'a>
        = &'a str
    where
        Self: 'a;
    type ValueFormat<'a>
        = &'a str
    where
        Self: 'a;
    type InstFormat<'a>
        = &'a str
    where
        Self: 'a;

    #[inline]
    fn func_count(&self) -> u32 {
        self.ir.functions.len() as u32
    }

    #[inline]
    fn funcs(&self) -> Self::Funcs {
        RefRange::new(0, self.func_count())
    }

    #[inline]
    fn funcs_to_compile(&self) -> Self::FuncsToCompile {
        self.funcs()
    }

    fn func_link_name(&self, func: IRFuncRef) -> &[u8] {
        self.ir
            .name(self.ir.functions[func.0 as usize].name)
            .as_bytes()
    }

    #[inline]
    fn func_extern(&self, func: IRFuncRef) -> bool {
        self.ir.functions[func.0 as usize].declaration
    }

    #[inline]
    fn func_only_local(&self, func: IRFuncRef) -> bool {
        self.ir.functions[func.0 as usize].local_only
    }

    fn func_has_weak_linkage(&self, _: IRFuncRef) -> bool {
        false
    }

    fn cur_needs_unwind_info(&self) -> bool {
        false
    }

    fn cur_is_vararg(&self) -> bool {
        false
    }

    fn cur_args(&self) -> Self::Args<'_> {
        let func = &self.ir.functions[self.cur_func as usize];
        RefRange::new(func.arg_begin_idx, func.arg_end_idx)
    }

    fn cur_arg_is_byval(&self, _: u32) -> bool {
        false
    }

    fn cur_arg_byval_size(&self, _: u32) -> u32 {
        0
    }

    fn cur_arg_byval_align(&self, _: u32) -> u32 {
        0
    }

    fn cur_arg_is_sret(&self, _: u32) -> bool {
        false
    }

    fn cur_static_allocas(&self) -> Self::StaticAllocas<'_> {
        let func = &self.ir.functions[self.cur_func as usize];
        assert_ne!(func.block_begin_idx, func.block_end_idx);
        let block = &self.ir.blocks[func.block_begin_idx as usize];
        StaticAllocas {
            values: &self.ir.values,
            indices: block.inst_begin_idx..block.inst_end_idx,
        }
    }

    fn cur_has_dynamic_alloca(&self) -> bool {
        false
    }

    fn cur_entry_block(&self) -> IRBlockRef {
        let func = &self.ir.functions[self.cur_func as usize];
        assert_ne!(func.block_begin_idx, func.block_end_idx);
        IRBlockRef(func.block_begin_idx)
    }

    fn cur_blocks(&self) -> Self::Blocks<'_> {
        let func = &self.ir.functions[self.cur_func as usize];
        RefRange::new(func.block_begin_idx, func.block_end_idx)
    }

    fn block_succs(&self, block: IRBlockRef) -> Self::Successors<'_> {
        let block = &self.ir.blocks[block.0 as usize];
        RefSlice::new(
            &self.ir.value_operands[block.succ_begin_idx as usize..block.succ_end_idx as usize],
        )
    }

    fn block_insts(&self, block: IRBlockRef) -> Self::Insts<'_> {
        let block = &self.ir.blocks[block.0 as usize];
        RefRange::new(block.phi_end_idx, block.inst_end_idx)
    }

    fn block_phis(&self, block: IRBlockRef) -> Self::Phis<'_> {
        let block = &self.ir.blocks[block.0 as usize];
        RefRange::new(block.inst_begin_idx, block.phi_end_idx)
    }

    #[inline]
    fn block_info(&self, block: IRBlockRef) -> u32 {
        self.ir.blocks[block.0 as usize].block_info.get()
    }

    #[inline]
    fn block_set_info(&self, block: IRBlockRef, info: u32) {
        self.ir.blocks[block.0 as usize].block_info.set(info);
    }

    #[inline]
    fn block_info2(&self, block: IRBlockRef) -> u32 {
        self.ir.blocks[block.0 as usize].block_info2.get()
    }

    #[inline]
    fn block_set_info2(&self, block: IRBlockRef, info: u32) {
        self.ir.blocks[block.0 as usize].block_info2.set(info);
    }

    fn block_fmt_ref(&self, block: IRBlockRef) -> &str {
        self.ir.name(self.ir.blocks[block.0 as usize].name)
    }

    #[inline]
    fn val_local_idx(&self, value: IRValueRef) -> ValLocalIdx {
        let first = self.ir.functions[self.cur_func as usize].arg_begin_idx;
        assert!(value.0 >= first);
        ValLocalIdx::new(value.0 - first)
    }

    #[inline]
    fn val_ignore_in_liveness_analysis(&self, value: IRValueRef) -> bool {
        self.ir.values[value.0 as usize].op == Op::Alloca
            && self.static_alloca_limit.is_none_or(|max_size| {
                self.val_alloca_size(value) <= max_size && self.val_alloca_align(value) <= 16
            })
    }

    #[inline]
    fn val_is_phi(&self, value: IRValueRef) -> bool {
        self.ir.values[value.0 as usize].value_type == ValueType::Phi
    }

    fn val_as_phi(&self, value: IRValueRef) -> Self::Phi<'_> {
        let value = &self.ir.values[value.0 as usize];
        assert_eq!(value.value_type, ValueType::Phi);
        let begin = value.op_begin_idx as usize;
        let middle = begin + value.op_count as usize;
        let end = middle + value.op_count as usize;
        Phi {
            values: &self.ir.value_operands[begin..middle],
            blocks: &self.ir.value_operands[middle..end],
        }
    }

    fn val_alloca_size(&self, value: IRValueRef) -> u32 {
        let value = &self.ir.values[value.0 as usize];
        assert_eq!(value.op, Op::Alloca);
        self.ir.value_operands[value.op_begin_idx as usize]
    }

    fn val_alloca_align(&self, value: IRValueRef) -> u32 {
        let value = &self.ir.values[value.0 as usize];
        assert_eq!(value.op, Op::Alloca);
        self.ir.value_operands[value.op_begin_idx as usize + 1]
    }

    fn value_fmt_ref(&self, value: IRValueRef) -> &str {
        self.ir.name(self.ir.values[value.0 as usize].name)
    }

    fn inst_operands(&self, inst: IRInstRef) -> Self::Operands<'_> {
        let value = &self.ir.values[inst.0 as usize];
        let begin = value.op_begin_idx as usize;
        RefSlice::new(&self.ir.value_operands[begin..begin + value.op_count as usize])
    }

    #[inline]
    fn inst_results(&self, inst: IRInstRef) -> Self::Results<'_> {
        self.ir.values[inst.0 as usize]
            .op
            .info()
            .is_def
            .then_some(IRValueRef::from(inst))
            .into_iter()
    }

    fn inst_fused(&self, _: IRInstRef) -> bool {
        false
    }

    fn inst_fmt_ref(&self, inst: IRInstRef) -> &str {
        self.ir.name(self.ir.values[inst.0 as usize].name)
    }

    fn start_compile(&mut self) {}

    fn end_compile(&mut self) {}

    fn switch_func(&mut self, func: IRFuncRef) -> bool {
        self.cur_func = func.0;
        self.highest_local_val_idx = 0;
        let func = &self.ir.functions[self.cur_func as usize];
        assert_ne!(func.block_begin_idx, func.block_end_idx);
        self.highest_local_val_idx =
            self.ir.blocks[func.block_end_idx as usize - 1].inst_end_idx - func.arg_begin_idx;
        if self.highest_local_val_idx > 0 {
            self.highest_local_val_idx -= 1;
        }
        true
    }

    fn reset(&mut self) {
        self.highest_local_val_idx = 0;
    }
}

const _: () = {
    assert!(core::mem::size_of::<TestIRAdaptor<'static, 'static>>() <= 128);
    assert!(!core::mem::needs_drop::<TestIRAdaptor<'static, 'static>>());
    assert!(!core::mem::needs_drop::<Phi<'static>>());
    assert!(!core::mem::needs_drop::<StaticAllocas<'static>>());
    assert!(!core::mem::needs_drop::<RefRange<IRValueRef>>());
    assert!(!core::mem::needs_drop::<RefSlice<'static, IRValueRef>>());
};
