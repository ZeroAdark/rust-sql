//! `IrAdaptor` for `gravity_ir::Program`.
//!
//! Function table: the program's functions first, then one extern function per
//! host function (`h<i>`), resolved by name to `HostFnDecl::addr` at map time.

use crate::view::{FnView, ImmRules, VInst};
use core::{iter::FusedIterator, ops::Range};
use gravity_ir::{Function, Program};
use mcx::{reserved_in, GrowIn, Mcx, PgVec};
use tpde_base::ValLocalIdx;
use tpde_core::ir_adaptor::{BlockCalls, HighestValueIndexProvider, IrAdaptor, PhiRef, ProvidesHighestValue};

macro_rules! refs {
    ($($name:ident),+) => {$(
        #[repr(transparent)]
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub u32);
        impl From<u32> for $name { #[inline] fn from(v: u32) -> Self { Self(v) } }
        mcx::forget_safe_nodrop!($name);
    )+};
}
refs!(GValue, GInst, GBlock, GFunc);

#[derive(Clone)]
pub struct RefRange<R> {
    indices: Range<u32>,
    marker: core::marker::PhantomData<R>,
}
impl<R> RefRange<R> {
    pub fn new(begin: u32, end: u32) -> Self {
        Self { indices: begin..end, marker: core::marker::PhantomData }
    }
}
impl<R: From<u32>> Iterator for RefRange<R> {
    type Item = R;
    #[inline]
    fn next(&mut self) -> Option<R> {
        self.indices.next().map(R::from)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.indices.size_hint()
    }
}
impl<R: From<u32>> ExactSizeIterator for RefRange<R> {}
impl<R: From<u32>> FusedIterator for RefRange<R> {}

pub struct RefSlice<'a, R> {
    indices: core::slice::Iter<'a, u32>,
    marker: core::marker::PhantomData<R>,
}
impl<'a, R> RefSlice<'a, R> {
    pub fn new(s: &'a [u32]) -> Self {
        Self { indices: s.iter(), marker: core::marker::PhantomData }
    }
}
impl<R: From<u32>> Iterator for RefSlice<'_, R> {
    type Item = R;
    #[inline]
    fn next(&mut self) -> Option<R> {
        self.indices.next().copied().map(R::from)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.indices.size_hint()
    }
}
impl<R: From<u32>> ExactSizeIterator for RefSlice<'_, R> {}
impl<R: From<u32>> FusedIterator for RefSlice<'_, R> {}

/// The link names of a program's functions (`f<i>`) and host functions
/// (`h<i>`): one buffer, one offset table (two allocations per compile,
/// not one per name), in the compile's view context. `parse` reads the
/// index back from a symbol name.
pub struct LinkNames<'p> {
    buf: PgVec<'p, u8>,
    off: PgVec<'p, u32>,
}

/// `n` in decimal appended to `buf`.
fn push_decimal(buf: &mut PgVec<'_, u8>, mut n: usize) {
    let mut d = [0u8; 20];
    let mut k = d.len();
    loop {
        k -= 1;
        d[k] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    buf.extend_from_slice_in(&d[k..]);
}

impl<'p> LinkNames<'p> {
    pub fn new(funcs: usize, hosts: usize, mcx: Mcx<'p>) -> Self {
        let mut buf = reserved_in((funcs + hosts) * 4, mcx);
        let mut off = reserved_in(funcs + hosts + 1, mcx);
        for (kind, n) in [(b'f', funcs), (b'h', hosts)] {
            for i in 0..n {
                off.push_in(buf.len() as u32);
                buf.push_in(kind);
                push_decimal(&mut buf, i);
            }
        }
        off.push_in(buf.len() as u32);
        Self { buf, off }
    }
    pub fn len(&self) -> usize {
        self.off.len() - 1
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn get(&self, i: usize) -> &[u8] {
        &self.buf[self.off[i] as usize..self.off[i + 1] as usize]
    }
    /// `(is_host, index)` of a link name; `None` for any other symbol.
    pub fn parse(name: &[u8]) -> Option<(bool, usize)> {
        let (&kind, digits) = name.split_first()?;
        if digits.is_empty() || digits.len() > 9 {
            return None;
        }
        let mut n = 0usize;
        for &d in digits {
            if !d.is_ascii_digit() {
                return None;
            }
            n = n * 10 + (d - b'0') as usize;
        }
        match kind {
            b'f' => Some((false, n)),
            b'h' => Some((true, n)),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub struct GPhi<'a> {
    incoming: &'a [(u32, gravity_ir::Value)],
}
impl PhiRef<GValue, GBlock> for GPhi<'_> {
    fn incoming_count(&self) -> u32 {
        self.incoming.len() as u32
    }
    fn incoming_val_for_slot(&self, slot: u32) -> GValue {
        GValue(self.incoming[slot as usize].1 .0)
    }
    fn incoming_block_for_slot(&self, slot: u32) -> GBlock {
        GBlock(self.incoming[slot as usize].0)
    }
    fn incoming_val_for_block(&self, block: GBlock) -> GValue {
        self.incoming
            .iter()
            .find(|(b, _)| *b == block.0)
            .map_or(GValue(u32::MAX), |(_, v)| GValue(v.0))
    }
}

pub struct GravityAdaptor<'p> {
    pub program: &'p Program,
    pub cur: u32,
    pub view: FnView<'p>,
    /// The link names `f<i>` (functions) then `h<i>` (host functions), as
    /// one buffer with offsets: `link_name(i)`.
    pub link_names: LinkNames<'p>,
    /// The target's immediate-folding rules, shared by the view (which
    /// decides which constants are never materialized) and the frontend.
    pub rules: &'static dyn ImmRules,
    /// Nanoseconds spent in `FnView::build` over every `switch_func`
    /// (`crate::CompileTiming::view_ns`).
    pub view_ns: u64,
    /// The program's poll host (`gravity_ir::KERNEL_POLL`), whose cold call blocks the layout sinks (D3651).
    poll: Option<gravity_ir::HostFn>,
    /// The JIT map's recording (`crate::compile_tagged`), `None` for every other compile.
    pub jit: Option<JitRec<'p>>,
}

/// The ranges' tag before a function's first tagged instruction (its prologue) and after its body (its epilogue).
pub const JIT_PROLOGUE: u32 = u32::MAX;
pub const JIT_EPILOGUE: u32 = u32::MAX - 1;

/// The JIT map's recording of one compile (gravity_backend perfmap mode `tags`): the lowering's tags, the current
/// function's runs by (block, index), its tag changes at raw text offsets, and every finished function's ranges at their
/// final text offsets `(function, offset, tag)`. Reads offsets only: the code is the same.
/// Its lists live in the compile's view context (`mcx`), reset with the compile's scratch.
pub struct JitRec<'p> {
    tags: &'p gravity_ir::JitTags,
    mcx: Mcx<'p>,
    runs: PgVec<'p, gravity_ir::JitRun>,
    cur: core::cell::RefCell<PgVec<'p, (u32, u32)>>,
    last: core::cell::Cell<u32>,
    done: core::cell::RefCell<PgVec<'p, (u32, u32, u32)>>,
}

impl<'p> JitRec<'p> {
    pub fn new_in(tags: &'p gravity_ir::JitTags, mcx: Mcx<'p>) -> Self {
        let (cur, done) = (core::cell::RefCell::new(PgVec::new_in(mcx)), core::cell::RefCell::new(PgVec::new_in(mcx)));
        Self { tags, mcx, runs: PgVec::new_in(mcx), cur, last: core::cell::Cell::new(u32::MAX), done }
    }

    /// Every finished function's ranges `(function, raw text offset less the function's skew, tag)`, taken.
    pub fn take_done(&mut self) -> PgVec<'p, (u32, u32, u32)> {
        core::mem::replace(self.done.get_mut(), PgVec::new_in(self.mcx))
    }

    /// The runs of function `func`, by block then index.
    fn load(&mut self, func: u32) {
        let tags = self.tags;
        self.runs.clear();
        self.runs.extend_in(tags.runs().iter().copied().filter(|r| r.func == func));
        self.runs.sort_unstable_by_key(|r| (r.block, r.idx));
    }

    /// The tag of instruction `idx` of IR block `block` (its terminator: `insts.len()`); 0 (`frame`) when untagged.
    pub fn tag_at(&self, block: u32, idx: u32) -> u32 {
        let p = self.runs.partition_point(|r| (r.block, r.idx) <= (block, idx));
        match p.checked_sub(1).map(|i| self.runs[i]) {
            Some(r) if r.block == block => r.tag,
            _ => 0,
        }
    }

    /// The instruction about to be compiled at raw text offset `off`, with `tag`.
    #[inline]
    pub fn at(&self, off: usize, tag: u32) {
        if self.last.get() != tag {
            self.last.set(tag);
            self.cur.borrow_mut().push_in((off as u32, tag));
        }
    }
}

impl<'p> GravityAdaptor<'p> {
    /// With the view each function is built into (`FnView::rebuild`), its tables in the compile's view context
    /// (`crate::Scratch`).
    pub fn with_view(program: &'p Program, rules: &'static dyn ImmRules, view: FnView<'p>) -> Self {
        let link_names = LinkNames::new(program.funcs.len(), program.host_fns.len(), view.mcx());
        let poll = program.host_fns.iter().position(|h| h.name == gravity_ir::KERNEL_POLL).map(|i| gravity_ir::HostFn(i as u32));
        Self { program, cur: u32::MAX, view, link_names, rules, view_ns: 0, poll, jit: None }
    }

    /// The JIT map's tag of `inst` (an edge split's: its target block's first), `None` when not recording.
    #[inline]
    pub fn jit_tag(&self, inst: GInst) -> Option<u32> {
        let j = self.jit.as_ref()?;
        Some(match self.inst(inst) {
            VInst::Op { block, idx } => j.tag_at(block, idx),
            VInst::Term { block } => j.tag_at(block, self.func().blocks[block as usize].insts.len() as u32),
            VInst::SplitBr { split } => {
                let target = self.view.splits[split as usize].target;
                match self.view.blocks.get(target as usize).and_then(|b| b.orig) {
                    Some(b) => j.tag_at(b, 0),
                    None => j.last.get(),
                }
            }
            VInst::Alloca { .. } => j.last.get(),
        })
    }
    #[inline]
    pub fn func(&self) -> &'p Function {
        &self.program.funcs[self.cur as usize]
    }
    #[inline]
    pub fn inst(&self, i: GInst) -> VInst {
        self.view.insts[i.0 as usize]
    }
    /// Function-table index of a host function.
    pub fn host_func_index(&self, h: u32) -> u32 {
        self.program.funcs.len() as u32 + h
    }
}

impl HighestValueIndexProvider for GravityAdaptor<'_> {
    fn cur_highest_val_idx(&self) -> u32 {
        (self.view.num_values + self.view.num_slots).saturating_sub(1)
    }
}

impl tpde_compiler::compiler_base::CompilerAdaptor for GravityAdaptor<'_> {
    fn compiler_insts(&self, block: GBlock) -> impl Iterator<Item = GInst> + Clone {
        self.block_insts(block)
    }
    fn phi_reuse(&self, value: GValue) -> Option<GValue> {
        let phi = *self.view.phi_reuse.get(value.0 as usize)?;
        (phi != u32::MAX).then_some(GValue(phi))
    }
    /// A cache line: a function's code sits at the same offset in its lines whatever the host table (one 16-byte PLT
    /// slot a symbol ahead of the text) or the functions before it hold, so a program change moves no other code's
    /// placement (an IR-identical walk read 4 % slower when one host entry moved it 16 bytes).
    fn func_align(&self) -> u32 {
        64
    }
}

impl IrAdaptor for GravityAdaptor<'_> {
    type ValueRef = GValue;
    type InstRef = GInst;
    type BlockRef = GBlock;
    type FuncRef = GFunc;
    type HighestValue = ProvidesHighestValue;

    const INVALID_VALUE_REF: GValue = GValue(u32::MAX);
    const INVALID_BLOCK_REF: GBlock = GBlock(u32::MAX);
    const INVALID_FUNC_REF: GFunc = GFunc(u32::MAX);
    const TPDE_LIVENESS_VISIT_ARGS: bool = true;

    type Funcs = RefRange<GFunc>;
    type FuncsToCompile = RefRange<GFunc>;
    type Args<'a>
        = RefSlice<'a, GValue>
    where
        Self: 'a;
    type StaticAllocas<'a>
        = RefRange<GValue>
    where
        Self: 'a;
    type Blocks<'a>
        = RefRange<GBlock>
    where
        Self: 'a;
    type Successors<'a>
        = RefSlice<'a, GBlock>
    where
        Self: 'a;
    type Insts<'a>
        = RefRange<GInst>
    where
        Self: 'a;
    type Phis<'a>
        = core::iter::Map<core::slice::Iter<'a, gravity_ir::Value>, fn(&gravity_ir::Value) -> GValue>
    where
        Self: 'a;
    type Operands<'a>
        = RefSlice<'a, GValue>
    where
        Self: 'a;
    type Results<'a>
        = core::option::IntoIter<GValue>
    where
        Self: 'a;
    type Phi<'a>
        = GPhi<'a>
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
        self.link_names.len() as u32
    }
    fn funcs(&self) -> Self::Funcs {
        RefRange::new(0, self.func_count())
    }
    fn funcs_to_compile(&self) -> Self::FuncsToCompile {
        RefRange::new(0, self.program.funcs.len() as u32)
    }
    fn func_link_name(&self, func: GFunc) -> &[u8] {
        self.link_names.get(func.0 as usize)
    }
    fn func_extern(&self, func: GFunc) -> bool {
        func.0 as usize >= self.program.funcs.len()
    }
    fn func_only_local(&self, _: GFunc) -> bool {
        false
    }
    fn func_has_weak_linkage(&self, _: GFunc) -> bool {
        false
    }
    fn cur_needs_unwind_info(&self) -> bool {
        false
    }
    fn cur_is_vararg(&self) -> bool {
        false
    }
    fn cur_args(&self) -> Self::Args<'_> {
        // Entry block params are the function's arguments; `Value` is
        // `repr(transparent)`-shaped as a u32 tuple struct.
        let params = &self.func().blocks[self.func().entry.index()].params;
        RefSlice::new(crate::raw::values_u32(params))
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
        RefRange::new(self.view.num_values, self.view.num_values + self.view.num_slots)
    }
    fn cur_has_dynamic_alloca(&self) -> bool {
        false
    }
    fn cur_entry_block(&self) -> GBlock {
        GBlock(self.view.entry)
    }
    fn cur_blocks(&self) -> Self::Blocks<'_> {
        RefRange::new(0, self.view.blocks.len() as u32)
    }
    fn block_succs(&self, block: GBlock) -> Self::Successors<'_> {
        // Layout order (hot first, cold last): TPDE's RPO and its
        // fall-through decisions follow this order; the frontend uses
        // `succs` (terminator edge order) for branch targets.
        RefSlice::new(&self.view.blocks[block.0 as usize].layout_succs)
    }
    fn block_insts(&self, block: GBlock) -> Self::Insts<'_> {
        crate::stop_point(); // D2909: TPDE's liveness and encode walk each block's instructions here, a stop point per block
        let b = &self.view.blocks[block.0 as usize];
        RefRange::new(b.inst_begin, b.inst_end)
    }
    fn block_phis(&self, block: GBlock) -> Self::Phis<'_> {
        self.view.blocks[block.0 as usize].phis.iter().map(|v| GValue(v.0))
    }
    fn block_info(&self, block: GBlock) -> u32 {
        self.view.blocks[block.0 as usize].info.get()
    }
    fn block_set_info(&self, block: GBlock, info: u32) {
        self.view.blocks[block.0 as usize].info.set(info)
    }
    fn block_info2(&self, block: GBlock) -> u32 {
        self.view.blocks[block.0 as usize].info2.get()
    }
    fn block_set_info2(&self, block: GBlock, info: u32) {
        self.view.blocks[block.0 as usize].info2.set(info)
    }
    fn block_fmt_ref(&self, block: GBlock) -> u32 {
        block.0
    }
    fn val_local_idx(&self, value: GValue) -> ValLocalIdx {
        // Copy propagation: a renamed block parameter is its source value.
        ValLocalIdx::new(self.view.rename.get(value.0 as usize).copied().unwrap_or(value.0))
    }
    fn val_ignore_in_liveness_analysis(&self, value: GValue) -> bool {
        value.0 >= self.view.num_values
    }
    fn val_is_phi(&self, value: GValue) -> bool {
        (value.0 as usize) < self.view.phis.len() && self.view.phis[value.0 as usize].is_some()
    }
    fn val_as_phi(&self, value: GValue) -> GPhi<'_> {
        GPhi { incoming: &self.view.phis[value.0 as usize].as_ref().unwrap().incoming }
    }
    fn val_alloca_size(&self, value: GValue) -> u32 {
        self.func().stack_slots[(value.0 - self.view.num_values) as usize].bytes.max(1)
    }
    fn val_alloca_align(&self, value: GValue) -> u32 {
        self.func().stack_slots[(value.0 - self.view.num_values) as usize].align.max(1)
    }
    fn value_fmt_ref(&self, value: GValue) -> u32 {
        value.0
    }
    fn inst_operands(&self, inst: GInst) -> Self::Operands<'_> {
        RefSlice::new(self.view.inst_operands(inst.0))
    }
    fn inst_results(&self, inst: GInst) -> Self::Results<'_> {
        let r = self.view.results[inst.0 as usize];
        (r != u32::MAX).then_some(GValue(r)).into_iter()
    }
    fn inst_fused(&self, inst: GInst) -> bool {
        self.view.fused[inst.0 as usize]
    }
    fn block_calls(&self, block: GBlock) -> BlockCalls {
        self.view.block_calls[block.0 as usize]
    }
    fn records_offsets(&self) -> bool {
        self.jit.is_some()
    }
    fn func_compiled(&self, body_end: usize, skew: u32) {
        let Some(j) = &self.jit else { return };
        let mut done = j.done.borrow_mut();
        for &(off, tag) in j.cur.borrow().iter() {
            done.push_in((self.cur, off.saturating_sub(skew), tag));
        }
        done.push_in((self.cur, (body_end as u32).saturating_sub(skew), JIT_EPILOGUE));
        j.cur.borrow_mut().clear();
        j.last.set(u32::MAX);
    }
    fn block_sink(&self, block: GBlock) -> bool {
        self.view.sink.get(block.0 as usize).copied().unwrap_or(false)
    }
    fn val_const(&self, value: GValue) -> Option<u64> {
        let v = self.view.rename.get(value.0 as usize).copied().unwrap_or(value.0) as usize;
        if !self.view.const_fused.get(v).copied().unwrap_or(false) {
            return None;
        }
        self.view.const_of[v].map(|(t, b)| crate::view::const_canon(t, b) as u64)
    }
    fn local_const(&self, local: ValLocalIdx) -> Option<u64> {
        let v = local.get() as usize;
        match self.view.const_of.get(v).copied().flatten() {
            Some((t, b)) if !t.is_float() && !t.is_vector() => Some(crate::view::const_canon(t, b) as u64),
            _ => None,
        }
    }
    fn inst_fmt_ref(&self, inst: GInst) -> u32 {
        inst.0
    }
    fn start_compile(&mut self) {}
    fn end_compile(&mut self) {}
    fn switch_func(&mut self, func: GFunc) -> bool {
        if func.0 as usize >= self.program.funcs.len() {
            return false;
        }
        crate::stop_point(); // D2909: each function's compile starts at a stop point
        self.cur = func.0;
        if let Some(j) = &mut self.jit {
            j.load(func.0);
        }
        let t0 = crate::now_ns();
        let func = self.func();
        self.view.rebuild(func, self.rules);
        self.view.mark_sinks(func, self.poll);
        self.view_ns += crate::now_ns() - t0;
        true
    }
    fn reset(&mut self) {
        self.cur = u32::MAX;
        self.view.clear();
    }
}
