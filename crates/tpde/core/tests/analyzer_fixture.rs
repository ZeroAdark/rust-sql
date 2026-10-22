// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::ir_adaptor::*;
use core::{cell::Cell, marker::PhantomData};
use std::{string::String, vec, vec::Vec};
use tpde_base::ValLocalIdx;

#[derive(Clone, Default)]
pub struct Value {
    pub local_idx: u32,
    pub ignore: bool,
    pub incoming: Vec<(u32, u32)>,
}
#[derive(Clone, Default)]
pub struct Block {
    pub succs: Vec<u32>,
    pub insts: Vec<u32>,
    pub phis: Vec<u32>,
    pub info: Cell<u32>,
    pub info2: Cell<u32>,
}
#[derive(Clone, Default)]
pub struct Inst {
    pub operands: Vec<u32>,
    pub results: Vec<u32>,
    pub fused: bool,
}
#[derive(Clone)]
pub struct Program {
    pub name: String,
    pub entry: u32,
    pub highest: u32,
    pub args: Vec<u32>,
    pub values: Vec<Value>,
    pub blocks: Vec<Block>,
    pub insts: Vec<Inst>,
    pub epoch_override: Option<u16>,
    pub successor_live: Cell<usize>,
    pub successor_drops: Cell<usize>,
}
impl Program {
    pub fn new(name: &str, block_count: usize) -> Self {
        Self {
            name: name.into(),
            entry: 0,
            highest: 0,
            args: vec![0],
            values: vec![Value::default()],
            blocks: vec![Block::default(); block_count],
            insts: Vec::new(),
            epoch_override: None,
            successor_live: Cell::new(0),
            successor_drops: Cell::new(0),
        }
    }
    pub fn linear(count: usize) -> Self {
        let mut p = Self::new("linear", count);
        for b in 0..count {
            if b + 1 < count {
                p.blocks[b].succs.push(b as u32 + 1);
            }
            let result = p.value(false);
            p.instruction(b as u32, &[if b == 0 { 0 } else { result - 1 }], &[result]);
        }
        p
    }
    pub fn value(&mut self, ignore: bool) -> u32 {
        let id = self.values.len() as u32;
        self.values.push(Value {
            local_idx: id,
            ignore,
            incoming: Vec::new(),
        });
        self.highest = self.highest.max(id);
        id
    }
    pub fn instruction(&mut self, block: u32, operands: &[u32], results: &[u32]) -> u32 {
        let id = self.insts.len() as u32;
        self.insts.push(Inst {
            operands: operands.into(),
            results: results.into(),
            fused: false,
        });
        self.blocks[block as usize].insts.push(id);
        id
    }
}
pub type Refs<'a> = core::iter::Copied<core::slice::Iter<'a, u32>>;
pub struct Successors<'a> {
    refs: Refs<'a>,
    program: &'a Program,
}
impl Iterator for Successors<'_> {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        self.refs.next()
    }
}
impl Drop for Successors<'_> {
    fn drop(&mut self) {
        self.program
            .successor_live
            .set(self.program.successor_live.get() - 1);
        self.program
            .successor_drops
            .set(self.program.successor_drops.get() + 1);
    }
}
pub struct Phi<'a>(&'a [(u32, u32)]);
impl PhiRef<u32, u32> for Phi<'_> {
    fn incoming_count(&self) -> u32 {
        self.0.len() as u32
    }
    fn incoming_val_for_slot(&self, slot: u32) -> u32 {
        self.0[slot as usize].1
    }
    fn incoming_block_for_slot(&self, slot: u32) -> u32 {
        self.0[slot as usize].0
    }
    fn incoming_val_for_block(&self, block: u32) -> u32 {
        self.0.iter().find(|p| p.0 == block).unwrap().1
    }
}
pub struct Fixture<'a, H = ProvidesHighestValue, const VISIT: bool = true> {
    pub program: &'a Program,
    marker: PhantomData<H>,
}
impl<'a, H, const VISIT: bool> Fixture<'a, H, VISIT> {
    pub fn new(program: &'a Program) -> Self {
        Self {
            program,
            marker: PhantomData,
        }
    }
}
impl<H, const VISIT: bool> HighestValueIndexProvider for Fixture<'_, H, VISIT> {
    fn cur_highest_val_idx(&self) -> u32 {
        self.program.highest
    }
}
impl<H, const VISIT: bool> IrAdaptor for Fixture<'_, H, VISIT>
where
    H: HighestValuePolicy<Self>,
{
    type ValueRef = u32;
    type BlockRef = u32;
    type InstRef = u32;
    type FuncRef = u32;
    type HighestValue = H;
    const INVALID_VALUE_REF: u32 = u32::MAX;
    const INVALID_BLOCK_REF: u32 = u32::MAX;
    const INVALID_FUNC_REF: u32 = u32::MAX;
    const TPDE_LIVENESS_VISIT_ARGS: bool = VISIT;
    type Funcs = core::ops::Range<u32>;
    type FuncsToCompile = core::ops::Range<u32>;
    type Args<'a>
        = Refs<'a>
    where
        Self: 'a;
    type StaticAllocas<'a>
        = core::iter::Empty<u32>
    where
        Self: 'a;
    type Blocks<'a>
        = core::ops::Range<u32>
    where
        Self: 'a;
    type Successors<'a>
        = Successors<'a>
    where
        Self: 'a;
    type Insts<'a>
        = Refs<'a>
    where
        Self: 'a;
    type Phis<'a>
        = Refs<'a>
    where
        Self: 'a;
    type Operands<'a>
        = Refs<'a>
    where
        Self: 'a;
    type Results<'a>
        = Refs<'a>
    where
        Self: 'a;
    type Phi<'a>
        = Phi<'a>
    where
        Self: 'a;
    type BlockFormat<'a>
        = u32
    where
        Self: 'a;
    type ValueFormat<'a>
        = u32
    where
        Self: 'a;
    type InstFormat<'a>
        = u32
    where
        Self: 'a;
    fn func_count(&self) -> u32 {
        1
    }
    fn funcs(&self) -> Self::Funcs {
        0..1
    }
    fn funcs_to_compile(&self) -> Self::FuncsToCompile {
        0..1
    }
    fn func_link_name(&self, _: u32) -> &[u8] {
        b"fixture"
    }
    fn func_extern(&self, _: u32) -> bool {
        false
    }
    fn func_only_local(&self, _: u32) -> bool {
        true
    }
    fn func_has_weak_linkage(&self, _: u32) -> bool {
        false
    }
    fn cur_needs_unwind_info(&self) -> bool {
        false
    }
    fn cur_is_vararg(&self) -> bool {
        false
    }
    fn cur_args(&self) -> Self::Args<'_> {
        self.program.args.iter().copied()
    }
    fn cur_arg_is_byval(&self, _: u32) -> bool {
        false
    }
    fn cur_arg_byval_size(&self, _: u32) -> u32 {
        0
    }
    fn cur_arg_byval_align(&self, _: u32) -> u32 {
        1
    }
    fn cur_arg_is_sret(&self, _: u32) -> bool {
        false
    }
    fn cur_static_allocas(&self) -> Self::StaticAllocas<'_> {
        core::iter::empty()
    }
    fn cur_has_dynamic_alloca(&self) -> bool {
        false
    }
    fn cur_entry_block(&self) -> u32 {
        self.program.entry
    }
    fn cur_blocks(&self) -> Self::Blocks<'_> {
        0..self.program.blocks.len() as u32
    }
    fn block_succs(&self, b: u32) -> Self::Successors<'_> {
        self.program
            .successor_live
            .set(self.program.successor_live.get() + 1);
        Successors {
            refs: self.program.blocks[b as usize].succs.iter().copied(),
            program: self.program,
        }
    }
    fn block_insts(&self, b: u32) -> Self::Insts<'_> {
        self.program.blocks[b as usize].insts.iter().copied()
    }
    fn block_phis(&self, b: u32) -> Self::Phis<'_> {
        self.program.blocks[b as usize].phis.iter().copied()
    }
    fn block_info(&self, b: u32) -> u32 {
        self.program.blocks[b as usize].info.get()
    }
    fn block_set_info(&self, b: u32, v: u32) {
        self.program.blocks[b as usize].info.set(v)
    }
    fn block_info2(&self, b: u32) -> u32 {
        self.program.blocks[b as usize].info2.get()
    }
    fn block_set_info2(&self, b: u32, v: u32) {
        self.program.blocks[b as usize].info2.set(v)
    }
    fn block_fmt_ref(&self, b: u32) -> u32 {
        b
    }
    fn val_local_idx(&self, v: u32) -> ValLocalIdx {
        ValLocalIdx::new(self.program.values[v as usize].local_idx)
    }
    fn val_ignore_in_liveness_analysis(&self, v: u32) -> bool {
        self.program.values[v as usize].ignore
    }
    fn val_is_phi(&self, v: u32) -> bool {
        self.program.blocks.iter().any(|b| b.phis.contains(&v))
    }
    fn val_as_phi(&self, v: u32) -> Self::Phi<'_> {
        Phi(&self.program.values[v as usize].incoming)
    }
    fn val_alloca_size(&self, _: u32) -> u32 {
        0
    }
    fn val_alloca_align(&self, _: u32) -> u32 {
        1
    }
    fn value_fmt_ref(&self, v: u32) -> u32 {
        v
    }
    fn inst_operands(&self, i: u32) -> Self::Operands<'_> {
        self.program.insts[i as usize].operands.iter().copied()
    }
    fn inst_results(&self, i: u32) -> Self::Results<'_> {
        self.program.insts[i as usize].results.iter().copied()
    }
    fn inst_fused(&self, i: u32) -> bool {
        self.program.insts[i as usize].fused
    }
    fn inst_fmt_ref(&self, i: u32) -> u32 {
        i
    }
    fn start_compile(&mut self) {}
    fn end_compile(&mut self) {}
    fn switch_func(&mut self, f: u32) -> bool {
        f == 0
    }
    fn reset(&mut self) {}
}
