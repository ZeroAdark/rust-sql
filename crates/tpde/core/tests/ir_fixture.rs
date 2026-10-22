// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::ir_adaptor::*;
use core::{cell::Cell, fmt, marker::PhantomData};
use std::{format, vec, vec::Vec};
use tpde_base::ValLocalIdx;

pub trait FixtureRef: Copy + PartialEq + fmt::Display {
    const INVALID: Self;
    fn from_id(id: u32) -> Self;
    fn id(self) -> u32;
}

impl FixtureRef for u32 {
    const INVALID: Self = u32::MAX;
    fn from_id(id: u32) -> Self {
        id
    }
    fn id(self) -> u32 {
        self
    }
}
impl FixtureRef for u64 {
    const INVALID: Self = u64::MAX;
    fn from_id(id: u32) -> Self {
        u64::from(id)
    }
    fn id(self) -> u32 {
        u32::try_from(self).unwrap()
    }
}

#[derive(Clone, Copy, PartialEq)]
pub struct LargeRef(pub [u64; 2]);
impl fmt::Display for LargeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0[0].fmt(f)
    }
}
impl FixtureRef for LargeRef {
    const INVALID: Self = Self([u64::MAX; 2]);
    fn from_id(id: u32) -> Self {
        Self([u64::from(id), 0])
    }
    fn id(self) -> u32 {
        self.0[0].try_into().unwrap()
    }
}

pub struct RefFormat<R>(char, R);
impl<R: fmt::Display> fmt::Display for RefFormat<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.0, self.1)
    }
}

pub struct RefIter<'a, R> {
    source: core::slice::Iter<'a, u32>,
    marker: PhantomData<R>,
}
impl<'a, R> RefIter<'a, R> {
    fn new(values: &'a [u32]) -> Self {
        Self {
            source: values.iter(),
            marker: PhantomData,
        }
    }
}
impl<R: FixtureRef> Iterator for RefIter<'_, R> {
    type Item = R;
    fn next(&mut self) -> Option<R> {
        self.source.next().copied().map(R::from_id)
    }
}

pub struct Phi<'a, V, B> {
    values: &'a [u32; 2],
    blocks: &'a [u32; 2],
    marker: PhantomData<(V, B)>,
}
impl<V: FixtureRef, B: FixtureRef> PhiRef<V, B> for Phi<'_, V, B> {
    fn incoming_count(&self) -> u32 {
        self.values.len() as u32
    }
    fn incoming_val_for_slot(&self, slot: u32) -> V {
        V::from_id(self.values[slot as usize])
    }
    fn incoming_block_for_slot(&self, slot: u32) -> B {
        B::from_id(self.blocks[slot as usize])
    }
    fn incoming_val_for_block(&self, block: B) -> V {
        self.incoming_val_for_slot(self.blocks.iter().position(|b| *b == block.id()).unwrap() as u32)
    }
}

pub struct Fixture<'ir, V = u32, B = u32, F = u32, H = ProvidesHighestValue, I = u32> {
    pub names: [&'ir [u8]; 2],
    pub aux: [Cell<u32>; 2],
    pub aux2: [Cell<u32>; 2],
    pub started: u32,
    pub ended: u32,
    pub switches: u32,
    pub resets: u32,
    pub current: u32,
    pub function_ids: &'ir [u32],
    pub args: [u32; 2],
    pub blocks: [u32; 2],
    pub incoming_values: [u32; 2],
    marker: PhantomData<(V, B, F, H, I)>,
}
impl<V, B, F, H, I> Fixture<'_, V, B, F, H, I> {
    pub fn new() -> Self {
        Self {
            names: [b"main", b"external"],
            aux: [Cell::new(0), Cell::new(0)],
            aux2: [Cell::new(0), Cell::new(0)],
            started: 0,
            ended: 0,
            switches: 0,
            resets: 0,
            current: u32::MAX,
            function_ids: &[0, 1],
            args: [10, 11],
            blocks: [1, 2],
            incoming_values: [10, 12],
            marker: PhantomData,
        }
    }
}
impl<V, B, F, I> HighestValueIndexProvider for Fixture<'_, V, B, F, ProvidesHighestValue, I> {
    fn cur_highest_val_idx(&self) -> u32 {
        15
    }
}
impl<'ir, V: FixtureRef, B: FixtureRef, F: FixtureRef, H, I: FixtureRef> IrAdaptor
    for Fixture<'ir, V, B, F, H, I>
where
    H: HighestValuePolicy<Self>,
{
    type ValueRef = V;
    type InstRef = I;
    type BlockRef = B;
    type FuncRef = F;
    type HighestValue = H;
    const INVALID_VALUE_REF: V = V::INVALID;
    const INVALID_BLOCK_REF: B = B::INVALID;
    const INVALID_FUNC_REF: F = F::INVALID;
    const TPDE_LIVENESS_VISIT_ARGS: bool = true;
    type Funcs = RefIter<'ir, F>;
    type FuncsToCompile = RefIter<'ir, F>;
    type Args<'a>
        = RefIter<'a, V>
    where
        Self: 'a;
    type StaticAllocas<'a>
        = RefIter<'a, V>
    where
        Self: 'a;
    type Blocks<'a>
        = RefIter<'a, B>
    where
        Self: 'a;
    type Successors<'a>
        = RefIter<'a, B>
    where
        Self: 'a;
    type Insts<'a>
        = RefIter<'a, I>
    where
        Self: 'a;
    type Phis<'a>
        = RefIter<'a, V>
    where
        Self: 'a;
    type Operands<'a>
        = RefIter<'a, V>
    where
        Self: 'a;
    type Results<'a>
        = RefIter<'a, V>
    where
        Self: 'a;
    type Phi<'a>
        = Phi<'a, V, B>
    where
        Self: 'a;
    type BlockFormat<'a>
        = RefFormat<B>
    where
        Self: 'a;
    type ValueFormat<'a>
        = RefFormat<V>
    where
        Self: 'a;
    type InstFormat<'a>
        = RefFormat<I>
    where
        Self: 'a;
    fn func_count(&self) -> u32 {
        2
    }
    fn funcs(&self) -> Self::Funcs {
        RefIter::new(self.function_ids)
    }
    fn funcs_to_compile(&self) -> Self::FuncsToCompile {
        RefIter::new(&self.function_ids[..1])
    }
    fn func_link_name(&self, func: F) -> &[u8] {
        self.names[func.id() as usize]
    }
    fn func_extern(&self, func: F) -> bool {
        func.id() == 1
    }
    fn func_only_local(&self, func: F) -> bool {
        func.id() == 0
    }
    fn func_has_weak_linkage(&self, func: F) -> bool {
        func.id() == 1
    }
    fn cur_needs_unwind_info(&self) -> bool {
        true
    }
    fn cur_is_vararg(&self) -> bool {
        false
    }
    fn cur_args(&self) -> Self::Args<'_> {
        RefIter::new(&self.args)
    }
    fn cur_arg_is_byval(&self, index: u32) -> bool {
        index == 0
    }
    fn cur_arg_byval_size(&self, index: u32) -> u32 {
        if index == 0 {
            24
        } else {
            0
        }
    }
    fn cur_arg_byval_align(&self, index: u32) -> u32 {
        if index == 0 {
            8
        } else {
            0
        }
    }
    fn cur_arg_is_sret(&self, index: u32) -> bool {
        index == 1
    }
    fn cur_static_allocas(&self) -> Self::StaticAllocas<'_> {
        RefIter::new(&[15])
    }
    fn cur_has_dynamic_alloca(&self) -> bool {
        true
    }
    fn cur_entry_block(&self) -> B {
        B::from_id(1)
    }
    fn cur_blocks(&self) -> Self::Blocks<'_> {
        RefIter::new(&self.blocks)
    }
    fn block_succs(&self, block: B) -> Self::Successors<'_> {
        RefIter::new(if block.id() == 1 { &[2] } else { &[] })
    }
    fn block_insts(&self, block: B) -> Self::Insts<'_> {
        RefIter::new(if block.id() == 1 { &[20] } else { &[21] })
    }
    fn block_phis(&self, block: B) -> Self::Phis<'_> {
        RefIter::new(if block.id() == 1 { &[] } else { &[13] })
    }
    fn block_info(&self, block: B) -> u32 {
        self.aux[(block.id() - 1) as usize].get()
    }
    fn block_set_info(&self, block: B, info: u32) {
        self.aux[(block.id() - 1) as usize].set(info);
    }
    fn block_info2(&self, block: B) -> u32 {
        self.aux2[(block.id() - 1) as usize].get()
    }
    fn block_set_info2(&self, block: B, info: u32) {
        self.aux2[(block.id() - 1) as usize].set(info);
    }
    fn block_fmt_ref(&self, block: B) -> RefFormat<B> {
        RefFormat('b', block)
    }
    fn val_local_idx(&self, value: V) -> ValLocalIdx {
        ValLocalIdx::new(value.id() - 10)
    }
    fn val_ignore_in_liveness_analysis(&self, value: V) -> bool {
        value.id() == 15
    }
    fn val_is_phi(&self, value: V) -> bool {
        value.id() == 13
    }
    fn val_as_phi(&self, value: V) -> Self::Phi<'_> {
        assert_eq!(value.id(), 13);
        Phi {
            values: &self.incoming_values,
            blocks: &self.blocks,
            marker: PhantomData,
        }
    }
    fn val_alloca_size(&self, value: V) -> u32 {
        assert_eq!(value.id(), 15);
        64
    }
    fn val_alloca_align(&self, value: V) -> u32 {
        assert_eq!(value.id(), 15);
        16
    }
    fn value_fmt_ref(&self, value: V) -> RefFormat<V> {
        RefFormat('v', value)
    }
    fn inst_operands(&self, inst: I) -> Self::Operands<'_> {
        RefIter::new(if inst.id() == 20 { &[10, 11] } else { &[13] })
    }
    fn inst_results(&self, inst: I) -> Self::Results<'_> {
        RefIter::new(if inst.id() == 20 { &[12] } else { &[14] })
    }
    fn inst_fused(&self, inst: I) -> bool {
        inst.id() == 21
    }
    fn inst_fmt_ref(&self, inst: I) -> RefFormat<I> {
        RefFormat('i', inst)
    }
    fn start_compile(&mut self) {
        self.started += 1;
    }
    fn end_compile(&mut self) {
        self.ended += 1;
    }
    fn switch_func(&mut self, func: F) -> bool {
        self.switches += 1;
        if func.id() == 0 {
            self.current = 0;
            true
        } else {
            false
        }
    }
    fn reset(&mut self) {
        for aux in self.aux.iter().chain(&self.aux2) {
            aux.set(0);
        }
        self.current = u32::MAX;
        self.resets += 1;
    }
}

#[test]
fn complete_api_preserves_shared_aux_and_reuse() {
    let mut ir = Adaptor::new(<Fixture<'_>>::new());
    assert!(Adaptor::<Fixture<'_>>::TPDE_PROVIDES_HIGHEST_VAL_IDX);
    assert_eq!(ir.highest_value_index(), Some(15));
    for turn in 1..=3 {
        ir.start_compile();
        assert_eq!(ir.func_count(), 2);
        assert_eq!(ir.funcs().collect::<Vec<_>>(), [0, 1]);
        assert_eq!(ir.funcs_to_compile().collect::<Vec<_>>(), [0]);
        assert!(!ir.switch_func(1));
        assert!(ir.switch_func(0));
        assert!(ir.cur_needs_unwind_info() && !ir.cur_is_vararg() && ir.cur_has_dynamic_alloca());
        assert_eq!(ir.cur_entry_block(), 1);
        for (idx, arg) in ir.cur_args().enumerate() {
            assert_eq!(arg, 10 + idx as u32);
            assert_eq!(ir.cur_arg_is_byval(idx as u32), idx == 0);
            assert_eq!(ir.cur_arg_is_sret(idx as u32), idx == 1);
            assert_eq!(
                ir.cur_arg_byval_size(idx as u32),
                if idx == 0 { 24 } else { 0 }
            );
            assert_eq!(
                ir.cur_arg_byval_align(idx as u32),
                if idx == 0 { 8 } else { 0 }
            );
        }
        for block in ir.cur_blocks() {
            assert_eq!(ir.block_info(block), 0);
            assert_eq!(ir.block_info2(block), 0);
            ir.block_set_info(block, block * 10);
            ir.block_set_info2(block, u32::MAX - block);
            assert_eq!(ir.block_info(block), block * 10);
            assert_eq!(ir.block_info2(block), u32::MAX - block);
            assert_eq!(format!("{}", ir.block_fmt_ref(block)), format!("b{block}"));
            assert_eq!(
                ir.block_succs(block).collect::<Vec<_>>(),
                if block == 1 { vec![2] } else { vec![] }
            );
            for inst in ir.block_insts(block) {
                assert_eq!(ir.inst_fused(inst), inst == 21);
                assert_eq!(format!("{}", ir.inst_fmt_ref(inst)), format!("i{inst}"));
                let operands = ir.inst_operands(inst).collect::<Vec<_>>();
                assert_eq!(operands, if inst == 20 { vec![10, 11] } else { vec![13] });
                assert_eq!(
                    ir.inst_results(inst).collect::<Vec<_>>(),
                    if inst == 20 { vec![12] } else { vec![14] }
                );
            }
            for value in ir.block_phis(block) {
                assert_eq!(value, 13);
                assert!(ir.val_is_phi(value));
                let phi = ir.val_as_phi(value);
                assert_eq!(phi.incoming_count(), 2);
                for slot in 0..phi.incoming_count() {
                    let block = phi.incoming_block_for_slot(slot);
                    assert_eq!(
                        phi.incoming_val_for_slot(slot),
                        phi.incoming_val_for_block(block)
                    );
                }
            }
        }
        for value in 10..=15 {
            assert_eq!(ir.val_local_idx(value).get(), value - 10);
            assert_eq!(ir.val_is_phi(value), value == 13);
            assert_eq!(ir.val_ignore_in_liveness_analysis(value), value == 15);
            assert_eq!(format!("{}", ir.value_fmt_ref(value)), format!("v{value}"));
        }
        for value in ir.cur_static_allocas() {
            assert_eq!(
                (ir.val_alloca_size(value), ir.val_alloca_align(value)),
                (64, 16)
            );
        }
        ir.end_compile();
        ir.reset();
        assert_eq!((ir.started, ir.ended, ir.resets), (turn, turn, turn));
        assert_eq!(ir.current, u32::MAX);
    }
}

#[test]
fn absent_highest_callback_and_eight_byte_references_are_admitted() {
    type Without = Fixture<'static, u64, u64, u64, NoHighestValue>;
    let mut ir = Adaptor::new(Without::new());
    assert!(!Adaptor::<Without>::TPDE_PROVIDES_HIGHEST_VAL_IDX);
    assert_eq!(ir.highest_value_index(), None);
    assert_eq!(Without::INVALID_VALUE_REF, u64::MAX);
    assert_eq!(Without::INVALID_BLOCK_REF, u64::MAX);
    assert_eq!(Without::INVALID_FUNC_REF, u64::MAX);
    assert!(ir.switch_func(0));
    assert_eq!(ir.cur_args().collect::<Vec<_>>(), [10, 11]);
    assert_eq!(core::mem::size_of_val(&ir), core::mem::size_of::<Without>());
    assert!(!core::mem::needs_drop::<Without>());
    let _ = ir.into_inner();
}

#[test]
fn linkage_names_preserve_non_utf8_and_embedded_null() {
    let names = [b"entry\xff\0".as_slice(), b"ext\0\xfe".as_slice()];
    let mut fixture = <Fixture<'_>>::new();
    fixture.names = names;
    let ir = Adaptor::new(fixture);
    assert_eq!(ir.func_link_name(0), names[0]);
    assert_eq!(ir.func_link_name(1), names[1]);
}

#[test]
fn module_ranges_borrow_external_ir_across_function_switches() {
    fn compile<A: IrAdaptor>(a: &mut Adaptor<A>) {
        for f in a.funcs_to_compile() {
            assert!(a.switch_func(f));
        }
    }
    let ids = [0, 1];
    let mut fixture = <Fixture<'_>>::new();
    fixture.function_ids = &ids;
    let mut a = Adaptor::new(fixture);
    let all = a.funcs();
    a.start_compile();
    compile(&mut a);
    a.end_compile();
    assert_eq!(all.collect::<Vec<_>>(), ids);
}
