// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use core::{
    fmt::Display,
    ops::{Deref, DerefMut},
};
use tpde_base::ValLocalIdx;

pub trait CanBeFormatted: Display {}
impl<T: Display + ?Sized> CanBeFormatted for T {}

pub trait IrIter<Value>: Iterator<Item = Value> {}
impl<Value, T: Iterator<Item = Value>> IrIter<Value> for T {}

pub trait IrRange<Value>: IntoIterator<Item = Value> {}
impl<Value, T: IntoIterator<Item = Value>> IrRange<Value> for T {}

pub trait PhiRef<Value, Block> {
    fn incoming_count(&self) -> u32;
    fn incoming_val_for_slot(&self, slot: u32) -> Value;
    fn incoming_block_for_slot(&self, slot: u32) -> Block;
    fn incoming_val_for_block(&self, block: Block) -> Value;
}

mod sealed {
    pub trait HighestValuePolicy {}
}

pub enum NoHighestValue {}
pub enum ProvidesHighestValue {}

impl sealed::HighestValuePolicy for NoHighestValue {}
impl sealed::HighestValuePolicy for ProvidesHighestValue {}

pub trait HighestValueIndexProvider {
    fn cur_highest_val_idx(&self) -> u32;
}

pub trait HighestValuePolicy<A: ?Sized>: sealed::HighestValuePolicy {
    const PROVIDED: bool;
    fn highest_value_index(adaptor: &A) -> Option<u32>;
}

impl<A: ?Sized> HighestValuePolicy<A> for NoHighestValue {
    const PROVIDED: bool = false;

    #[inline]
    fn highest_value_index(_: &A) -> Option<u32> {
        None
    }
}

impl<A: HighestValueIndexProvider + ?Sized> HighestValuePolicy<A> for ProvidesHighestValue {
    const PROVIDED: bool = true;

    #[inline]
    fn highest_value_index(adaptor: &A) -> Option<u32> {
        Some(adaptor.cur_highest_val_idx())
    }
}

/// The calls a block contains, by temperature (`IrAdaptor::block_calls`).
/// A hot call is one on the function's frequent path (once per row); a
/// cold one runs rarely relative to the values live around it (a page
/// fetch once per ~100 rows, a raise), so saving and restoring a register
/// around it is cheaper than keeping the value out of that register.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockCalls {
    pub hot: bool,
    pub cold: bool,
}

pub trait IrAdaptor: Sized {
    type ValueRef: PartialEq;
    type InstRef;
    type BlockRef: PartialEq;
    type FuncRef: PartialEq;
    type HighestValue: HighestValuePolicy<Self>;

    const INVALID_VALUE_REF: Self::ValueRef;
    const INVALID_BLOCK_REF: Self::BlockRef;
    const INVALID_FUNC_REF: Self::FuncRef;
    const TPDE_LIVENESS_VISIT_ARGS: bool;

    type Funcs: IrRange<Self::FuncRef>;
    type FuncsToCompile: IrRange<Self::FuncRef>;
    type Args<'a>: IrRange<Self::ValueRef>
    where
        Self: 'a;
    type StaticAllocas<'a>: IrRange<Self::ValueRef>
    where
        Self: 'a;
    type Blocks<'a>: IrRange<Self::BlockRef>
    where
        Self: 'a;
    type Successors<'a>: IrRange<Self::BlockRef>
    where
        Self: 'a;
    type Insts<'a>: IrRange<Self::InstRef>
    where
        Self: 'a;
    type Phis<'a>: IrRange<Self::ValueRef>
    where
        Self: 'a;
    type Operands<'a>: IrRange<Self::ValueRef>
    where
        Self: 'a;
    type Results<'a>: IrRange<Self::ValueRef>
    where
        Self: 'a;
    type Phi<'a>: PhiRef<Self::ValueRef, Self::BlockRef>
    where
        Self: 'a;
    type BlockFormat<'a>: CanBeFormatted
    where
        Self: 'a;
    type ValueFormat<'a>: CanBeFormatted
    where
        Self: 'a;
    type InstFormat<'a>: CanBeFormatted
    where
        Self: 'a;

    fn func_count(&self) -> u32;
    fn funcs(&self) -> Self::Funcs;
    fn funcs_to_compile(&self) -> Self::FuncsToCompile;
    fn func_link_name(&self, func: Self::FuncRef) -> &[u8];
    fn func_extern(&self, func: Self::FuncRef) -> bool;
    fn func_only_local(&self, func: Self::FuncRef) -> bool;
    fn func_has_weak_linkage(&self, func: Self::FuncRef) -> bool;
    fn cur_needs_unwind_info(&self) -> bool;
    fn cur_is_vararg(&self) -> bool;
    fn cur_args(&self) -> Self::Args<'_>;
    fn cur_arg_is_byval(&self, index: u32) -> bool;
    fn cur_arg_byval_size(&self, index: u32) -> u32;
    fn cur_arg_byval_align(&self, index: u32) -> u32;
    fn cur_arg_is_sret(&self, index: u32) -> bool;
    fn cur_static_allocas(&self) -> Self::StaticAllocas<'_>;
    fn cur_has_dynamic_alloca(&self) -> bool;
    fn cur_entry_block(&self) -> Self::BlockRef;
    fn cur_blocks(&self) -> Self::Blocks<'_>;
    fn block_succs(&self, block: Self::BlockRef) -> Self::Successors<'_>;
    fn block_insts(&self, block: Self::BlockRef) -> Self::Insts<'_>;
    fn block_phis(&self, block: Self::BlockRef) -> Self::Phis<'_>;
    fn block_info(&self, block: Self::BlockRef) -> u32;
    fn block_set_info(&self, block: Self::BlockRef, info: u32);
    fn block_info2(&self, block: Self::BlockRef) -> u32;
    fn block_set_info2(&self, block: Self::BlockRef, info: u32);
    fn block_fmt_ref(&self, block: Self::BlockRef) -> Self::BlockFormat<'_>;
    fn val_local_idx(&self, value: Self::ValueRef) -> ValLocalIdx;
    fn val_ignore_in_liveness_analysis(&self, value: Self::ValueRef) -> bool;
    fn val_is_phi(&self, value: Self::ValueRef) -> bool;
    fn val_as_phi(&self, value: Self::ValueRef) -> Self::Phi<'_>;
    fn val_alloca_size(&self, value: Self::ValueRef) -> u32;
    fn val_alloca_align(&self, value: Self::ValueRef) -> u32;
    fn value_fmt_ref(&self, value: Self::ValueRef) -> Self::ValueFormat<'_>;
    fn inst_operands(&self, inst: Self::InstRef) -> Self::Operands<'_>;
    fn inst_results(&self, inst: Self::InstRef) -> Self::Results<'_>;
    fn inst_fused(&self, inst: Self::InstRef) -> bool;
    fn inst_fmt_ref(&self, inst: Self::InstRef) -> Self::InstFormat<'_>;
    /// The calls a block contains, hot and cold. The analyzer marks values
    /// whose live range covers a block with a hot call
    /// (`LivenessInfo::crosses_call`) and, separately, one with a cold call
    /// (`LivenessInfo::crosses_cold_call`). A value crossing only cold
    /// calls may hold a caller-saved register: the call sequence saves and
    /// restores a fixed caller-saved register around a call instead of
    /// evicting it. The default assumes every block may call, hot.
    fn block_calls(&self, block: Self::BlockRef) -> BlockCalls {
        let _ = block;
        BlockCalls {
            hot: true,
            cold: false,
        }
    }
    /// `block` is a cold detour the layout may sink to the end of its
    /// innermost loop (D3651): rarely run, not a loop header, its results
    /// read only inside it, no phis at its successors. The RPO puts a
    /// detour before the block it rejoins, so left in place its bytes sit
    /// between the hot path's blocks (a taken branch around it, and every
    /// block after it shifted). The analyzer sinks it only inside a loop
    /// and extends the live range of each value live into its successors
    /// to cover it. Default: none.
    fn block_sink(&self, block: Self::BlockRef) -> bool {
        let _ = block;
        false
    }
    /// The frontend records its instructions' text offsets (a JIT map's):
    /// the compiler then reports each function's end (`func_compiled`).
    /// Default: no.
    fn records_offsets(&self) -> bool {
        false
    }
    /// The current function is finished: its body ended at text offset
    /// `body_end` (its epilogue follows), and the finish removed `skew`
    /// bytes ahead of every body offset (the unused prologue
    /// reservation), so a body offset recorded at `off` is now at
    /// `off - skew`. Called only when `records_offsets`.
    fn func_compiled(&self, body_end: usize, skew: u32) {
        let _ = (body_end, skew);
    }
    /// `value` is a constant the frontend never materializes as a value
    /// (every use folds): its bits, canonical for its type, for an edge
    /// that passes it to a phi (`branches::move_to_phi` materializes it
    /// straight into the phi's register or slot). Default: none.
    fn val_const(&self, value: Self::ValueRef) -> Option<u64> {
        let _ = value;
        None
    }
    /// The constant a materialized general-purpose value holds, by local
    /// index: such a value is rematerialized at a reload instead of read
    /// from its slot, and never stored (`spill_part`, `reload_to_reg`).
    /// Default: none.
    fn local_const(&self, local: ValLocalIdx) -> Option<u64> {
        let _ = local;
        None
    }
    fn start_compile(&mut self);
    fn end_compile(&mut self);
    fn switch_func(&mut self, func: Self::FuncRef) -> bool;
    fn reset(&mut self);
}

#[repr(transparent)]
pub struct Adaptor<A: IrAdaptor>(A);

impl<A: IrAdaptor> Adaptor<A> {
    pub const TPDE_PROVIDES_HIGHEST_VAL_IDX: bool = A::HighestValue::PROVIDED;

    #[inline]
    pub fn new(adaptor: A) -> Self {
        const {
            assert!(
                core::mem::size_of::<A::ValueRef>() <= 8,
                "IR value reference exceeds 8 bytes"
            );
            assert!(
                core::mem::size_of::<A::BlockRef>() <= 8,
                "IR block reference exceeds 8 bytes"
            );
            assert!(
                core::mem::size_of::<A::FuncRef>() <= 8,
                "IR function reference exceeds 8 bytes"
            );
        }
        Self(adaptor)
    }

    #[inline]
    pub fn highest_value_index(&self) -> Option<u32> {
        A::HighestValue::highest_value_index(&self.0)
    }

    #[inline]
    pub fn into_inner(self) -> A {
        self.0
    }
}

impl<A: IrAdaptor> Deref for Adaptor<A> {
    type Target = A;

    #[inline]
    fn deref(&self) -> &A {
        &self.0
    }
}

impl<A: IrAdaptor> DerefMut for Adaptor<A> {
    #[inline]
    fn deref_mut(&mut self) -> &mut A {
        &mut self.0
    }
}

#[cfg(test)]
#[path = "../tests/ir_fixture.rs"]
mod tests;
