//! A TPDE-shaped view of one Gravity function.
//!
//! TPDE wants phis (one incoming value per predecessor block), a flat
//! instruction list per block with the terminator as the last instruction,
//! static allocas as values, and dense value numbers. Gravity has block
//! parameters bound by edge arguments, so this module:
//!
//! * turns every non-entry block parameter into a phi whose incoming values
//!   come from the edges targeting the block;
//! * splits edges when one source block has several edges to the same target
//!   (`condbr c (loop a) (loop b)`, or several switch cases), because a phi
//!   can name only one incoming value per predecessor;
//! * appends one pseudo value per stack slot, listed as a static alloca at the
//!   start of the entry block;
//! * propagates copies: a block parameter whose incoming values are all one
//!   value (a single-predecessor block's pass-through parameters, a join
//!   passed the same value from every arm) *is* that value. `rename` maps
//!   it, `val_local_idx` applies the map, and TPDE never sees the phi: no
//!   move on the edge, and the value's one register serves both names
//!   (SSA copy propagation is unconditionally valid; the register is not
//!   rewritten between the copy and its uses because every rewrite is an
//!   edge into the value's own block, which the copy's block dominates
//!   only from the far side).

use core::cell::Cell;
use core::hash::Hash;
use mcx::{pg_vec, reserved_in, GrowIn, Mcx, PgFxHashMap, PgVec};
use tpde_core::ir_adaptor::BlockCalls;
use gravity_ir::{Block, ConvOp, Edge, IntOvfOp, FloatCond, Function, IntBinOp, IntCond, Op, Program, Terminator, Type, Value};

/// One instruction in the flattened list.
#[derive(Clone, Copy, Debug)]
pub enum VInst {
    /// Pseudo instruction defining the stack slot value.
    Alloca { slot: u32 },
    /// `func.blocks[block].insts[idx]`.
    Op { block: u32, idx: u32 },
    /// The terminator of the original block.
    Term { block: u32 },
    /// The `Br` of a split block (`split` index into `FnView::splits`).
    SplitBr { split: u32 },
}

#[derive(Debug)]
pub struct VBlock<'s> {
    /// Original block, or `None` for a split block.
    pub orig: Option<u32>,
    /// Phi values: the original block's params (empty for entry and splits).
    pub phis: PgVec<'s, Value>,
    /// Range into `FnView::insts`.
    pub inst_begin: u32,
    pub inst_end: u32,
    /// Successor view-block indices, one per edge, in terminator edge order.
    pub succs: PgVec<'s, u32>,
    /// The same successors in layout order for TPDE's RPO: the likely / hot
    /// successor first, cold successors last (a successor listed last is
    /// visited first by the DFS, so it lands later in the layout).
    pub layout_succs: PgVec<'s, u32>,
    pub info: Cell<u32>,
    pub info2: Cell<u32>,
}

#[derive(Debug)]
pub struct Split {
    pub target: u32,
    /// The edge split: original block `from`'s `edge`-th (its arguments are the split block's).
    pub from: u32,
    pub edge: u32,
}

/// A compare fused into its block's `CondBr`: the compare instruction is
/// skipped and the terminator emits `cmp` + `b.cond` (or `cbz`/`cbnz`).
#[derive(Clone, Copy, Debug)]
pub struct FusedCmp {
    pub a: Value,
    /// Right operand, or `None` when it was a fused constant (`imm`).
    pub b: Option<Value>,
    pub imm: u64,
    pub icond: Option<IntCond>,
    pub fcond: Option<FloatCond>,
    pub is_f64: bool,
    /// A fused signed add/sub overflow predicate: `(op, width in bits)`;
    /// the branch is taken on overflow of `a op b`. `icond`/`fcond` are None.
    pub ovf: Option<(IntOvfOp, u32)>,
    /// `icmp a, sext(trunc(a))` folded to one compare against `a`'s own
    /// sign extension from this width (`cmp x, w, sxtw`). `b` is None.
    pub self_ext: Option<u32>,
    /// A second compare fused as an AArch64 conditional compare
    /// (`ImmRules::ccmp`, `fuse_compares`): the branch was on
    /// `iand`/`ior` of this compare and a second one, or of this compare
    /// and a `Bool` value (`Ccmp::icond` `Ne` against zero).
    pub second: Option<Ccmp>,
    /// A third condition chained the same way after the second (the branch
    /// was on a nested `iand (iand x y) z` / `ior (ior x y) z`).
    pub third: Option<Ccmp>,
}

/// The second compare of a fused `cmp; ccmp; b.cond` sequence.
#[derive(Clone, Copy, Debug)]
pub struct Ccmp {
    pub a: Value,
    /// Right operand, or `None` for the immediate `imm` (0..32).
    pub b: Option<Value>,
    pub imm: u32,
    pub icond: IntCond,
    /// The branch is on `ior` (else `iand`) of the two conditions.
    pub or: bool,
}

#[derive(Debug)]
pub struct Phi<'s> {
    /// (view predecessor block, incoming value), one entry per incoming edge.
    pub incoming: PgVec<'s, (u32, Value)>,
}

/// The passes of `FnView::build_in`, in order, for `FnView::pass_ns`.
pub const PASS_NAMES: [&str; 19] = ["blocks", "loop_bodies", "insts_edges", "layout_succs", "block_freqs", "consts", "propagate_copies", "forward_phis", "eliminate_dead", "fuse_compares", "fuse_immediates", "fuse_narrow_ops", "fuse_narrow_moves", "fuse_addresses", "fuse_stores", "fuse_flags", "pick_fixed", "compute_weights", "pick_phi_reuse"];

#[derive(Debug)]
pub struct FnView<'s> {
    /// The context every table lives in: the compile's view context (`crate::Scratch`).
    mcx: Mcx<'s>,
    /// Nanoseconds per pass of the last `build_in` (`PASS_NAMES` order),
    /// by the clock `crate::set_clock` installed; zero without one.
    pub pass_ns: [u64; 19],
    /// Per value (allocas after the real values): the operand slots and
    /// edge arguments naming it, exact at every moment (`set_operands`
    /// keeps it). The passes' use counts through the rename map and their
    /// definition tables are snapshots of it (`uses_snapshot`,
    /// `defs_snapshot`) in storage reused across builds.
    pub direct_uses: PgVec<'s, u32>,
    uses: PgVec<'s, u32>,
    def_inst: PgVec<'s, u32>,
    pub blocks: PgVec<'s, VBlock<'s>>,
    pub insts: PgVec<'s, VInst>,
    pub splits: PgVec<'s, Split>,
    /// Operands per instruction: `operands[ops_begin[i]..ops_end[i]]`. A
    /// fusion pass rewrites an instruction's range in place
    /// (`set_operands`: the new operands are appended, the old slice is
    /// left unreferenced), so the ranges are dense arrays and the lookup
    /// on TPDE's hottest path (`inst_operands`) is two loads.
    pub operands: PgVec<'s, u32>,
    pub ops_begin: PgVec<'s, u32>,
    pub ops_end: PgVec<'s, u32>,
    /// Result value per instruction, `u32::MAX` for none.
    pub results: PgVec<'s, u32>,
    /// Phi per value index (`None` for non-phi values).
    pub phis: PgVec<'s, Option<Phi<'s>>>,
    /// Number of real values (`func.values.len()`); allocas follow.
    pub num_values: u32,
    pub num_slots: u32,
    pub entry: u32,
    pub has_calls: bool,
    /// Instructions skipped by the compiler (their work moved elsewhere).
    pub fused: PgVec<'s, bool>,
    /// Per original block: the compare fused into its `CondBr`.
    pub term_fuse: PgVec<'s, Option<FusedCmp>>,
    /// Per original block: the edge a `CondBr` on a constant always takes
    /// (`const_terms`): the other edge is not an edge of the view.
    pub term_const: PgVec<'s, Option<usize>>,
    /// Values the frontend asks TPDE to keep in one register for life.
    pub force_fixed: PgVec<'s, bool>,
    /// Values defined outside every loop and never read in one: TPDE's own
    /// rule would fix them (live past their block) and hold a register
    /// through every loop for a value read once per call
    /// (`allow_fixed_assignment` refuses; `pick_fixed`).
    pub no_fixed: PgVec<'s, bool>,
    /// Expected executions per call of each original block under the
    /// view's frequency model (`block_freqs`): the weight of a regalloc
    /// event in that block (`crate::RegEvent`).
    pub block_freq: PgVec<'s, f32>,
    /// The natural loops (`loop_bodies`): per header the blocks of its
    /// body, computed once per function for `block_freqs` and `pick_fixed`.
    pub loops: PgVec<'s, (usize, Body<'s>)>,
    /// Predecessors per original block (`loop_bodies`, `block_freqs`).
    pub preds: PgVec<'s, PgVec<'s, u32>>,
    /// Per view block: the calls it contains (`IrAdaptor::block_calls`). A
    /// call is cold when its block is in `hints.cold_blocks` or its expected
    /// frequency per call (`block_freqs`) is below 1: the page fetch
    /// (`set_freq 0.01`), the done/flush blocks, raises. Per-row host calls
    /// (fmgr, detoast, `deform`, `stage_tuple`) sit in loop blocks and stay hot.
    pub block_calls: PgVec<'s, BlockCalls>,
    /// Copy propagation: the value each value stands for (identity for all
    /// but renamed block parameters), for `val_local_idx`.
    pub rename: PgVec<'s, u32>,
    /// Spill weight per value from the IR hints (`CompilerTarget::value_weight`):
    /// the sum over its references of the referencing block's expected
    /// frequency, times 64; `remat` values weigh a quarter.
    pub weight: PgVec<'s, u32>,
    /// Constant per value (by `Const` definition).
    pub const_of: PgVec<'s, Option<(Type, u64)>>,
    /// Narrow-integer results the lowerer proved fit their type (no
    /// canonicalization after the op).
    pub nowrap: PgVec<'s, bool>,
    /// Constants every use of which folds into an immediate (never
    /// materialized). The frontend folds an immediate only for these.
    pub const_fused: PgVec<'s, bool>,
    /// Per value: the phi it may be computed into (`CompilerAdaptor::phi_reuse`),
    /// or `u32::MAX`.
    pub phi_reuse: PgVec<'s, u32>,
    /// `icmp eq/ne a, sext(trunc(a))` producing a value: the width the
    /// compare sign-extends `a` from (`cmp x, w, sxtw`); the conversions
    /// are fused away and the instruction's operands are `[a]`.
    pub icmp_self_ext: VMap<'s, u32, u32>,
    /// `or/and/xor/add/sub a, (shl x k)`: the operand `x` and shift `k`
    /// folded into the operation's shifted-register form; the instruction's
    /// operands are `[other, x]`.
    pub shifted: VMap<'s, u32, (u32, u32, u32)>,
    /// `trunc (ushr x k)` to `w` bits with `k + w <= 64`: one signed
    /// bitfield extract `(x, k)` (`fuse_narrow_ops`); the operand is `[x]`.
    pub trunc_sbfx: VMap<'s, u32, (u32, u32)>,
    /// Narrow integer results (`add`/`sub`/`mul`/`and`/`or`/`xor`/`shl`)
    /// whose every use is a store's value (`fuse_narrow_moves`): the
    /// frontends skip the canonicalisation (a store reads the low bytes).
    pub no_canon: PgVec<'s, bool>,
    /// `ptr_add` instructions rewritten by `fuse_addresses`: the index
    /// value, scale and offset the frontend uses instead of the op's.
    pub ptr_add_override: VMap<'s, u32, (u32, u64, i64)>,
    /// Loads rewritten by `fuse_addresses`: `(base, index and shift, offset)`.
    /// With an index the load is the register-offset form and its operands
    /// are `[base, index]`; without, the load's own offset is replaced.
    pub load_override: VMap<'s, u32, (u32, Option<(u32, u32)>, i64)>,
    /// Stores paired by `fuse_stores`: the first store of two 8-byte
    /// stores to consecutive cells of one base, mapped to the second's
    /// value (`stp`); the second store is fused away. A value that is a
    /// fused zero constant is the zero register.
    pub store_pair: VMap<'s, u32, u32>,
    /// Stores rewritten by `fuse_addresses` to the register-offset form:
    /// `(base, index, shift)`; the operands are `[base, index, value]`.
    pub store_override: VMap<'s, u32, (u32, u32, u32)>,
    /// `Icmp`s whose flags were set by the instruction before them on the
    /// same operands (`fuse_flags`): the frontend emits `cset` alone and
    /// the operands are empty.
    pub icmp_flags: PgVec<'s, bool>,
    /// `Load`s whose only use was a `zext` to I64 (`fuse_narrow_moves`):
    /// the load zero-extends and the `zext` is renamed to it.
    pub load_zext: PgVec<'s, bool>,
    /// Zero stores widened by `fuse_stores`: two zero stores of one
    /// integer width to consecutive cells become one zero store of the
    /// next width (`strb wzr` x2 -> `strh wzr`, ...), keyed by the first.
    pub store_zero_wide: VMap<'s, u32, Type>,
    /// shapecheck: the fusions (`FUSION_NAMES` bits) taken in any function since the compile began; not cleared per
    /// function (`clear`), reset by `crate::compile_with`.
    pub fusions: u8,
    /// Per view block: TPDE's layout sinks it to the end of its innermost
    /// loop (`IrAdaptor::block_sink`, `mark_sinks`; D3651).
    pub sink: PgVec<'s, bool>,
}

/// shapecheck fire names of `FnView::fusions`' bits (REGISTRY CR2-19..25), in bit order.
pub const FUSION_NAMES: [&str; 7] = ["tpde.cmp_branch", "tpde.imm", "tpde.narrow_op", "tpde.narrow_move", "tpde.addr", "tpde.store_pair", "tpde.flag_reuse"];

/// `v` holding `n` copies of `x`, its capacity kept.
/// D1939: the representative of value `v` in a rename table read as a union-find (a value past the table, or one that names
/// itself, is its own), halving the path it walks. A pass storing it for every value leaves the table naming each value's
/// representative directly, the form the passes after a rename read in one step.
fn find_rep(rename: &mut [u32], v: usize) -> usize {
    let mut x = v;
    while x < rename.len() && rename[x] as usize != x {
        let p = rename[x] as usize;
        let gp = if p < rename.len() { rename[p] } else { p as u32 };
        rename[x] = gp;
        x = gp as usize;
    }
    x
}

/// D3698: a natural loop's body (`FnView::loops`): a bit set over the range of original blocks its members span, of a
/// function of `n` blocks. `body[b]` reads a block's bit as the dense table of the function's size did, and a pass that
/// wants the members walks the set bits (`ones`): a table per loop of every block (a function of a thousand loops and
/// twenty thousand blocks: twenty million entries to fill and to walk per pass) cost the square of a chain of loops.
#[derive(Debug)]
pub struct Body<'s> {
    n: usize,
    /// The first block of `words`' range, a multiple of 64.
    lo: usize,
    words: PgVec<'s, u64>,
}

impl<'s> Body<'s> {
    /// The body of blocks `members` (each below `n`; at least one), in `mcx`.
    pub fn of(n: usize, members: &[u32], mcx: Mcx<'s>) -> Body<'s> {
        let lo = members.iter().map(|&b| b as usize).min().unwrap_or(0) & !63;
        let hi = members.iter().map(|&b| b as usize + 1).max().unwrap_or(0);
        let mut words = filled_in(mcx, 0u64, hi.saturating_sub(lo).div_ceil(64));
        for &b in members {
            let b = b as usize;
            assert!(b < n, "block {b} past the function's {n} blocks");
            words[(b - lo) / 64] |= 1 << ((b - lo) % 64);
        }
        Body { n, lo, words }
    }

    /// Whether block `b` is in the body.
    pub fn contains(&self, b: usize) -> bool {
        assert!(b < self.n, "block {b} past the function's {} blocks", self.n);
        b >= self.lo && self.words.get((b - self.lo) / 64).is_some_and(|w| (w >> ((b - self.lo) % 64)) & 1 != 0)
    }

    /// The number of blocks in the body.
    pub fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// The body's blocks, ascending.
    pub fn ones(&self) -> impl Iterator<Item = usize> + '_ {
        let lo = self.lo;
        self.words.iter().enumerate().flat_map(move |(i, &w)| {
            let mut w = w;
            core::iter::from_fn(move || {
                (w != 0).then(|| {
                    let b = w.trailing_zeros() as usize;
                    w &= w - 1;
                    lo + i * 64 + b
                })
            })
        })
    }
}

impl core::ops::Index<usize> for Body<'_> {
    type Output = bool;
    fn index(&self, b: usize) -> &bool {
        if self.contains(b) { &true } else { &false }
    }
}

fn refill<T: Clone>(v: &mut PgVec<'_, T>, n: usize, x: T) {
    v.clear();
    v.resize_in(n, x);
}

/// Room for `n` more elements of `v`, or its context's ERROR: the growth `GrowIn` has no form for (a `reserve`, a
/// `resize_with`) goes through it first. The check inlines; the growth is one out-of-line call.
#[inline(always)]
pub(crate) fn room<T>(v: &mut PgVec<'_, T>, n: usize) {
    if v.capacity() - v.len() < n {
        grow(v, n)
    }
}

#[cold]
#[inline(never)]
fn grow<T>(v: &mut PgVec<'_, T>, n: usize) {
    if v.try_reserve(n).is_err() {
        mcx::raise(v.allocator().oom_boxed(n.saturating_mul(core::mem::size_of::<T>())));
    }
}

/// `GrowIn`'s push and extend for the view's per-instruction and per-value loops at `std::vec::Vec`'s speed: the room check
/// inlined at the site, the growth one out-of-line call (`grow`), then the elements written into the room
/// (`crate::raw::grow`). mcx's `push_in` is a call in fast-profile, and allocator-api2's `extend` / `extend_from_slice`
/// check the capacity and store the length per element.
trait GrowHot<T> {
    fn push_hot(&mut self, x: T);
    /// `extend` (the iterator's lower bound written in one step, any rest pushed).
    fn extend_hot<I: IntoIterator<Item = T>>(&mut self, it: I);
    /// `extend_from_slice` of a `Copy` slice: one copy.
    fn extend_copy(&mut self, s: &[T])
    where
        T: Copy;
}

impl<T> GrowHot<T> for PgVec<'_, T> {
    #[inline(always)]
    fn push_hot(&mut self, x: T) {
        room(self, 1);
        crate::raw::grow::push_in_room(self, x);
    }

    #[inline(always)]
    fn extend_hot<I: IntoIterator<Item = T>>(&mut self, it: I) {
        let mut it = it.into_iter();
        let n = it.size_hint().0;
        room(self, n);
        crate::raw::grow::fill_room(self, &mut it, n);
        for x in it {
            self.push_hot(x);
        }
    }

    #[inline(always)]
    fn extend_copy(&mut self, s: &[T])
    where
        T: Copy,
    {
        room(self, s.len());
        crate::raw::grow::copy_in_room(self, s);
    }
}

/// A `Copy` slice copied into `m` (`to_vec_in`, one copy).
#[inline(always)]
fn copied_in<'m, T: Copy>(s: &[T], m: Mcx<'m>) -> PgVec<'m, T> {
    let mut v = PgVec::new_in(m);
    v.extend_copy(s);
    v
}

/// `pg_vec![in m; x; n]` at `std::vec!`'s speed: the room taken with the check inlined (`room`), the `n` elements written
/// into it with one length store (`crate::raw::grow::fill_room`). mcx's `vec_elem_in` is a call per site in the cgu-1 and
/// release builds, and its `resize` checks the room a second time.
#[inline(always)]
fn filled_in<'m, T: Clone>(m: Mcx<'m>, x: T, n: usize) -> PgVec<'m, T> {
    let mut v = PgVec::new_in(m);
    room(&mut v, n);
    crate::raw::grow::fill_room(&mut v, &mut core::iter::repeat_n(x, n), n);
    v
}

/// mcx `CollectIn::collect_in` by `extend_hot`.
trait CollectHot: Iterator + Sized {
    #[inline(always)]
    fn collect_hot<'m>(self, m: Mcx<'m>) -> PgVec<'m, Self::Item> {
        let mut v = PgVec::new_in(m);
        v.extend_hot(self);
        v
    }
}

impl<I: Iterator> CollectHot for I {}

/// `v.resize_with(n, f)` with its room taken first (`room`).
fn resize_with_in<T>(v: &mut PgVec<'_, T>, n: usize, f: impl FnMut() -> T) {
    room(v, n.saturating_sub(v.len()));
    v.resize_with(n, f);
}

/// `m.insert(k, x)` with its room taken first, or the context's ERROR (hashbrown's own growth aborts).
fn put<K: Hash + Eq, V>(m: &mut PgFxHashMap<'_, K, V>, k: K, x: V) {
    if m.try_reserve(1).is_err() {
        mcx::raise(m.allocator().oom_boxed(core::mem::size_of::<(K, V)>()));
    }
    m.insert(k, x);
}

/// `m.entry(k).or_insert(x)` with its room taken first, or the context's ERROR: one hash where a `get` and then a `put`
/// were two.
fn put_or_get<'a, K: Hash + Eq, V>(m: &'a mut PgFxHashMap<'_, K, V>, k: K, x: V) -> &'a mut V {
    if m.try_reserve(1).is_err() {
        mcx::raise(m.allocator().oom_boxed(core::mem::size_of::<(K, V)>()));
    }
    m.entry(k).or_insert(x)
}

/// An empty map in `mcx`.
fn map_in<'s, K, V>(mcx: Mcx<'s>) -> PgFxHashMap<'s, K, V> {
    PgFxHashMap::with_hasher_in(Default::default(), mcx)
}

/// One of the view's fusion maps (`FnView::shifted`, `store_pair`, ...): most functions fill none of them and every
/// instruction of the passes and the frontends looks into several, so a look into an empty one is one load and a branch
/// (no hash, no probe), as the ordered maps they replaced cost. Everything else is the map's own (`Deref`).
#[derive(Debug)]
pub struct VMap<'s, K, V>(PgFxHashMap<'s, K, V>);

impl<'s, K: Hash + Eq, V> VMap<'s, K, V> {
    fn new_in(mcx: Mcx<'s>) -> Self {
        VMap(map_in(mcx))
    }

    #[inline(always)]
    pub fn get(&self, k: &K) -> Option<&V> {
        if self.0.is_empty() { None } else { self.0.get(k) }
    }

    #[inline(always)]
    pub fn contains_key(&self, k: &K) -> bool {
        !self.0.is_empty() && self.0.contains_key(k)
    }

    #[inline(always)]
    pub fn get_mut(&mut self, k: &K) -> Option<&mut V> {
        if self.0.is_empty() { None } else { self.0.get_mut(k) }
    }

    #[inline(always)]
    pub fn remove(&mut self, k: &K) -> Option<V> {
        if self.0.is_empty() { None } else { self.0.remove(k) }
    }
}

impl<'s, K, V> core::ops::Deref for VMap<'s, K, V> {
    type Target = PgFxHashMap<'s, K, V>;

    fn deref(&self) -> &PgFxHashMap<'s, K, V> {
        &self.0
    }
}

impl<'s, K, V> core::ops::DerefMut for VMap<'s, K, V> {
    fn deref_mut(&mut self) -> &mut PgFxHashMap<'s, K, V> {
        &mut self.0
    }
}

/// Every edge of every function passes exactly its target block's
/// parameters. The view binds a phi's incoming values by zipping the
/// target's parameters with the edge's arguments (`bind_args`), so an
/// edge with fewer arguments leaves a phi without an incoming value on
/// that predecessor — no move on the edge, the phi's register holds
/// whatever it held — and one with more silently drops the extra: both
/// are IR structural inconsistencies (`gravity_ir::verify` rejects them
/// too), refused here so a release build reports them rather than
/// compiling them.
pub fn check_edge_arity(program: &Program) -> Result<(), alloc::string::String> {
    for f in &program.funcs {
        for (bi, b) in f.blocks.iter().enumerate() {
            for e in b.term.edges() {
                let Some(tb) = f.blocks.get(e.target.index()) else {
                    return Err(alloc::format!("fn {}: b{bi} branches to b{} which does not exist", f.name, e.target.0));
                };
                if tb.params.len() != e.args.len() {
                    return Err(alloc::format!("fn {}: edge b{bi} -> b{} has {} args, block has {} params", f.name, e.target.0, e.args.len(), tb.params.len()));
                }
            }
        }
    }
    Ok(())
}

impl<'s> FnView<'s> {
    /// An empty view whose tables live in `mcx`.
    pub fn new_in(mcx: Mcx<'s>) -> FnView<'s> {
        let v = || PgVec::new_in(mcx);
        FnView {
            mcx,
            pass_ns: [0; 19],
            direct_uses: v(),
            uses: v(),
            def_inst: v(),
            blocks: PgVec::new_in(mcx),
            insts: PgVec::new_in(mcx),
            splits: PgVec::new_in(mcx),
            operands: v(),
            ops_begin: v(),
            ops_end: v(),
            results: v(),
            phis: PgVec::new_in(mcx),
            num_values: 0,
            num_slots: 0,
            entry: 0,
            has_calls: false,
            fused: PgVec::new_in(mcx),
            term_fuse: PgVec::new_in(mcx),
            term_const: PgVec::new_in(mcx),
            force_fixed: PgVec::new_in(mcx),
            no_fixed: PgVec::new_in(mcx),
            block_freq: PgVec::new_in(mcx),
            loops: PgVec::new_in(mcx),
            preds: PgVec::new_in(mcx),
            block_calls: PgVec::new_in(mcx),
            rename: v(),
            weight: v(),
            const_of: PgVec::new_in(mcx),
            nowrap: PgVec::new_in(mcx),
            const_fused: PgVec::new_in(mcx),
            phi_reuse: v(),
            icmp_self_ext: VMap::new_in(mcx),
            shifted: VMap::new_in(mcx),
            trunc_sbfx: VMap::new_in(mcx),
            no_canon: PgVec::new_in(mcx),
            ptr_add_override: VMap::new_in(mcx),
            load_override: VMap::new_in(mcx),
            store_pair: VMap::new_in(mcx),
            store_override: VMap::new_in(mcx),
            icmp_flags: PgVec::new_in(mcx),
            load_zext: PgVec::new_in(mcx),
            store_zero_wide: VMap::new_in(mcx),
            fusions: 0,
            sink: PgVec::new_in(mcx),
        }
    }

    /// The context the view's tables live in.
    pub fn mcx(&self) -> Mcx<'s> {
        self.mcx
    }

    pub fn build(mcx: Mcx<'s>, func: &Function, rules: &dyn ImmRules) -> FnView<'s> {
        FnView::build_in(FnView::new_in(mcx), func, rules)
    }

    /// Every table emptied, every capacity kept: the view of the next
    /// function (or the next compile, through `crate::Scratch`) reuses
    /// the allocations.
    pub fn clear(&mut self) {
        let FnView { mcx: _, pass_ns: _, direct_uses, uses, def_inst, blocks, insts, splits, operands, ops_begin, ops_end, results, phis, num_values, num_slots, entry, has_calls, fused, term_fuse, term_const, force_fixed, no_fixed, block_freq, loops, preds, block_calls, rename, weight, const_of, nowrap, const_fused, phi_reuse, icmp_self_ext, shifted, trunc_sbfx, no_canon, ptr_add_override, load_override, store_override, icmp_flags, load_zext, store_pair, store_zero_wide, fusions: _, sink } = self;
        direct_uses.clear();
        uses.clear();
        def_inst.clear();
        blocks.clear();
        insts.clear();
        splits.clear();
        operands.clear();
        ops_begin.clear();
        ops_end.clear();
        results.clear();
        phis.clear();
        *num_values = 0;
        *num_slots = 0;
        *entry = 0;
        *has_calls = false;
        fused.clear();
        term_fuse.clear();
        term_const.clear();
        force_fixed.clear();
        no_fixed.clear();
        block_freq.clear();
        loops.clear();
        preds.clear();
        block_calls.clear();
        rename.clear();
        weight.clear();
        const_of.clear();
        nowrap.clear();
        const_fused.clear();
        phi_reuse.clear();
        icmp_self_ext.clear();
        shifted.clear();
        trunc_sbfx.clear();
        no_canon.clear();
        ptr_add_override.clear();
        load_override.clear();
        store_override.clear();
        icmp_flags.clear();
        load_zext.clear();
        store_pair.clear();
        store_zero_wide.clear();
        sink.clear();
    }

    /// D3651: the blocks TPDE's layout may sink (`IrAdaptor::block_sink`): a cancel check's poll detour, the cold block
    /// that calls `poll` (`gravity_ir::KERNEL_POLL`'s host, D3406) and rejoins the check's continuation. Left where the
    /// RPO puts it (before that continuation) its bytes sit in the loop's straight line: the clear-byte path jumps around
    /// them and every block after them moves (a grouped count's row loop moved 12 bytes off its fetch alignment: +5.5 %
    /// cycles at equal instructions). Only a block TPDE can move safely is marked: not a loop header, its results read only inside it,
    /// its successors without parameters.
    pub fn mark_sinks(&mut self, func: &Function, poll: Option<gravity_ir::HostFn>) {
        let m = self.mcx;
        self.sink.clear();
        self.sink.resize_in(self.blocks.len(), false);
        let Some(poll) = poll else { return };
        let mut any = false;
        // D3698: the headers per block, from one walk of the hints and the loops (a scan of both per cold block)
        let mut headers = filled_in(m, false, func.blocks.len());
        for h in func.hints.loop_headers.iter().map(|h| h.index()).chain(self.loops.iter().map(|(h, _)| *h)) {
            if let Some(x) = headers.get_mut(h) {
                *x = true;
            }
        }
        for b in &func.hints.cold_blocks {
            let bi = b.index();
            let Some(block) = func.blocks.get(bi) else { continue };
            let calls_poll = block.insts.iter().any(|i| matches!(&i.op, Op::CallHost { callee, .. } if *callee == poll));
            let header = headers[bi];
            let plain_succs = block.term.edges().all(|e| e.args.is_empty() && func.blocks.get(e.target.index()).is_some_and(|t| t.params.is_empty()));
            if calls_poll && !header && plain_succs && block.params.is_empty() && bi < self.sink.len() {
                self.sink[bi] = true;
                any = true;
            }
        }
        if !any {
            return;
        }
        // A marked block's results are read in it alone (the layout puts it after the blocks that would read them).
        let mut def_block = filled_in(m, u32::MAX, func.values.len());
        for (bi, block) in func.blocks.iter().enumerate() {
            if self.sink.get(bi).copied().unwrap_or(false) {
                for i in &block.insts {
                    if let Some(r) = i.result {
                        def_block[r.index()] = bi as u32;
                    }
                }
            }
        }
        let mut ops: PgVec<Value> = PgVec::new_in(m);
        for (bi, block) in func.blocks.iter().enumerate() {
            ops.clear();
            for i in &block.insts {
                i.op.for_each_operand(|v| ops.push_hot(v));
            }
            block.term.for_each_operand(|v| ops.push_hot(v));
            for e in block.term.edges() {
                ops.extend_copy(&e.args);
            }
            for v in &ops {
                let d = def_block.get(v.index()).copied().unwrap_or(u32::MAX);
                if d != u32::MAX && d != bi as u32 {
                    self.sink[d as usize] = false;
                }
            }
        }
    }

    /// shapecheck: the `FUSION_NAMES` bits this function's fusion passes took (narrow moves are marked at their sites).
    fn fusion_bits(&self) -> u8 {
        let mut b = 0u8;
        b |= (self.term_fuse.iter().any(|t| t.is_some()) as u8) << 0;
        b |= (self.const_fused.iter().any(|&c| c) as u8) << 1;
        b |= ((!self.icmp_self_ext.is_empty() || !self.shifted.is_empty() || !self.trunc_sbfx.is_empty()) as u8) << 2;
        b |= ((!self.load_override.is_empty() || !self.ptr_add_override.is_empty() || !self.store_override.is_empty()) as u8) << 4;
        b |= ((!self.store_pair.is_empty() || !self.store_zero_wide.is_empty()) as u8) << 5;
        b |= (self.icmp_flags.iter().any(|&f| f) as u8) << 6;
        b
    }

    /// `build` into `view`'s storage (`rebuild`).
    pub fn build_in(mut view: FnView<'s>, func: &Function, rules: &dyn ImmRules) -> FnView<'s> {
        view.rebuild(func, rules);
        view
    }

    /// The view of `func` in this view's storage (`clear`ed first).
    pub fn rebuild(&mut self, func: &Function, rules: &dyn ImmRules) {
        let view = self;
        let m = view.mcx;
        let nb = func.blocks.len();
        let mut marks = [0u64; 20];
        marks[0] = crate::now_ns();
        view.clear();
        view.num_values = func.values.len() as u32;
        view.num_slots = func.stack_slots.len() as u32;
        view.entry = func.entry.0;
        resize_with_in(&mut view.phis, func.values.len(), || None);
        // Incoming edges per block: the phis' `incoming` lists are sized
        // once (a split edge adds one predecessor, counted the same way).
        let mut npred = filled_in(m, 0u32, nb);
        for b in &func.blocks {
            for e in b.term.edges() {
                npred[e.target.index()] += 1;
            }
        }
        for (bi, b) in func.blocks.iter().enumerate() {
            let is_entry = bi as u32 == func.entry.0;
            if !is_entry {
                for &p in &b.params {
                    view.phis[p.index()] = Some(Phi { incoming: reserved_in(npred[bi] as usize, m) });
                }
            }
            view.blocks.push_hot(VBlock {
                orig: Some(bi as u32),
                phis: if is_entry { PgVec::new_in(m) } else { copied_in(&b.params, m) },
                inst_begin: 0,
                inst_end: 0,
                succs: PgVec::new_in(m),
                layout_succs: PgVec::new_in(m),
                info: Cell::new(0),
                info2: Cell::new(0),
            });
        }
        marks[1] = pass_end();
        view.term_const = const_terms(func, m);
        // The natural loops first: an edge into a loop header from outside
        // its body is split below (a preheader).
        view.loop_bodies(func);
        marks[2] = pass_end();
        // Instructions and edges, splitting duplicate (source, target) pairs
        // and the entries into loops. Sized from the IR: one view
        // instruction per IR instruction plus one terminator per block plus
        // the allocas; the operand arena grows by the fusion passes'
        // rewrites only.
        let n_insts = func.blocks.iter().map(|b| b.insts.len() + 1).sum::<usize>() + func.stack_slots.len();
        room(&mut view.insts, n_insts);
        room(&mut view.results, n_insts);
        room(&mut view.ops_begin, n_insts);
        room(&mut view.ops_end, n_insts);
        room(&mut view.operands, n_insts * 2);
        let mut ops: PgVec<u32> = reserved_in(8, m);
        // D3698: the loops by header, for the entry-edge test below (a scan of every loop per edge)
        let of_header = loops_by_header(&view.loops, nb, m);
        for bi in 0..nb {
            if bi % 256 == 0 {
                crate::stop_point(); // D2909: a function of many blocks takes a cancel inside its view build too
            }
            let b = &func.blocks[bi];
            let begin = view.insts.len() as u32;
            if bi as u32 == func.entry.0 {
                for s in 0..func.stack_slots.len() {
                    view.push_inst(VInst::Alloca { slot: s as u32 }, &[], Some(view.num_values + s as u32));
                }
            }
            let mut block_calls = false;
            let folded_cmp = view.term_const[bi].and_then(|_| match &b.term {
                Terminator::CondBr { cond, .. } => b.insts.iter().rposition(|i| i.result == Some(*cond) && matches!(i.op, Op::Icmp { .. })),
                _ => None,
            });
            for (ii, inst) in b.insts.iter().enumerate() {
                if folded_cmp == Some(ii) {
                    continue;
                }
                op_operands_into(&inst.op, &mut ops);
                if matches!(inst.op, Op::Call { .. } | Op::CallHost { .. }) {
                    view.has_calls = true;
                    block_calls = true;
                }
                view.push_inst(VInst::Op { block: bi as u32, idx: ii as u32 }, &ops, inst.result.map(|v| v.0));
            }
            ops.clear();
            match &b.term {
                Terminator::Br(_) | Terminator::Unreachable => {}
                Terminator::CondBr { .. } if view.term_const[bi].is_some() => {}
                Terminator::CondBr { cond, .. } => ops.push_hot(cond.0),
                Terminator::Switch { value, .. } => ops.push_hot(value.0),
                Terminator::Return(vs) => ops.extend_hot(vs.iter().map(|v| v.0)),
            }
            view.push_inst(VInst::Term { block: bi as u32 }, &ops, None);
            let end = view.insts.len() as u32;
            view.blocks[bi].inst_begin = begin;
            view.blocks[bi].inst_end = end;
            view.block_calls.push_hot(BlockCalls { hot: block_calls, cold: false });

            let mut succs = reserved_in(b.term.edges().count(), m);
            for (ei, e) in b.term.edges().enumerate() {
                if view.term_const[bi].is_some_and(|t| t != ei) {
                    continue;
                }
                let dup = b.term.edges().enumerate().any(|(oi, o)| oi != ei && o.target == e.target);
                let target_has_params = !func.blocks[e.target.index()].params.is_empty();
                // A loop's entry edge gets its own block, laid out right
                // before the header: the header's phis are pinned at their
                // first compiled edge and hold their registers until the
                // header's block, so an entry edge compiled in the window
                // dispatch would hold them through every loop laid out
                // between (a two-key group-by: five loops of six parameters dispatched
                // from one region, 30 pinned registers wanted at once).
                let entry = of_header[e.target.index()].iter().any(|&li| !view.loops[li as usize].1[bi]);
                if target_has_params && (dup || entry) {
                    let sb = view.add_split(bi as u32, ei, e);
                    succs.push_hot(sb);
                } else {
                    view.bind_args(bi as u32, e);
                    succs.push_hot(e.target.0);
                }
            }
            view.blocks[bi].succs = succs;
        }
        view.build_direct_uses(func);
        marks[3] = pass_end();
        refill(&mut view.nowrap, func.values.len() + view.num_slots as usize, false);
        for v in &func.hints.nowrap {
            view.nowrap[v.index()] = true;
        }
        view.order_layout_succs(func);
        marks[4] = pass_end();
        let freq = view.block_freqs(func);
        view.block_freq.clear();
        view.block_freq.extend_copy(&freq);
        for bi in 0..nb {
            if view.block_calls[bi].hot && freq[bi] < 1.0 {
                view.block_calls[bi] = BlockCalls { hot: false, cold: true };
            }
        }
        view.block_calls.resize_in(view.blocks.len(), BlockCalls::default());
        marks[5] = pass_end();
        refill(&mut view.fused, view.insts.len(), false);
        resize_with_in(&mut view.term_fuse, nb, || None);
        refill(&mut view.const_of, func.values.len() + view.num_slots as usize, None);
        refill(&mut view.const_fused, func.values.len() + view.num_slots as usize, false);
        for b in &func.blocks {
            for i in &b.insts {
                if let (Some(r), Op::Const { ty, bits }) = (i.result, &i.op) {
                    view.const_of[r.index()] = Some((*ty, *bits));
                }
            }
        }
        marks[6] = pass_end();
        view.propagate_copies(func, rules);
        marks[7] = pass_end();
        view.forward_phis(func);
        view.reuse_values(func);
        marks[8] = pass_end();
        view.eliminate_dead(func);
        marks[9] = pass_end();
        view.fuse_compares(func, rules);
        marks[10] = pass_end();
        view.fuse_immediates(func, rules);
        marks[11] = pass_end();
        view.fuse_narrow_ops(func, rules);
        marks[12] = pass_end();
        view.fuse_narrow_moves(func);
        marks[13] = pass_end();
        view.fuse_addresses(func, rules);
        marks[14] = pass_end();
        view.fuse_stores(func, rules);
        marks[15] = pass_end();
        view.fuse_flags(func, rules);
        marks[16] = pass_end();
        view.fusions |= view.fusion_bits();
        view.pick_fixed(func);
        marks[17] = pass_end();
        view.compute_weights(func);
        marks[18] = pass_end();
        view.pick_phi_reuse(func);
        marks[19] = pass_end();
        for i in 0..19 {
            view.pass_ns[i] = marks[i + 1] - marks[i];
        }
    }

    /// Values that may be computed straight into a phi's register
    /// (`tpde-phi-coalescing.md` section 3, "computing a back-edge incoming
    /// into the phi's own register"): an instruction result `r` of block
    /// `X`, all of whose uses are in `X` and exactly one of which is an
    /// argument of `X`'s terminator to a phi `p` (on an unsplit edge), by
    /// an instruction that writes its result after its last operand read;
    /// `p` is not read in `X` after `r`'s definition, nor in any block
    /// reachable from `X` without passing through `p`'s block (where every
    /// edge rewrites `p`'s register), `X` itself included. The compiler
    /// applies the dynamic half (`p` sits in a fixed register when `r` is
    /// defined, `FunctionSession::shadow_phi`).
    fn pick_phi_reuse(&mut self, func: &Function) {
        let m = self.mcx;
        let nv = func.values.len();
        let total = nv + self.num_slots as usize;
        refill(&mut self.phi_reuse, total, u32::MAX);
        let def_inst = self.defs_snapshot(true);
        let rn = |v: usize| self.rename.get(v).map_or(v, |r| *r as usize);
        // Instruction (and terminator) uses outside the defining block, and
        // edge uses: (count, source block, target block, parameter index).
        let mut outside = filled_in(m, 0u32, total);
        for (bi, b) in self.blocks.iter().enumerate() {
            for i in b.inst_begin..b.inst_end {
                for &v in self.inst_operands(i) {
                    let v = rn(v as usize);
                    let d = def_inst[v];
                    let same = d != u32::MAX && matches!(self.insts[d as usize], VInst::Op { block, .. } if block as usize == bi);
                    if !same {
                        outside[v] += 1;
                    }
                }
            }
        }
        // Edge uses per value: the count, and the (target, parameter)
        // they all pass the value to — `u32::MAX` when they disagree or an
        // edge is split. The sources need not be one block: `j + 1` passed
        // to the header from both arms of the body is computed into the
        // header's register once the walk below finds every source in the
        // region (a source outside it would let the value be read after
        // an edge into the phi's block rewrote the register).
        let mut edge: PgVec<(u32, u32, u32)> = filled_in(m, (0, u32::MAX, u32::MAX), total);
        for (bi, b) in func.blocks.iter().enumerate() {
            for (ei, e) in b.term.edges().enumerate() {
                for (i, a) in e.args.iter().enumerate() {
                    let v = rn(a.index());
                    let split = self.blocks[bi].succs.get(ei).map_or(true, |&s| s != e.target.0);
                    let (n, t, pi) = edge[v];
                    edge[v] = if split || (n != 0 && (t, pi) != (e.target.0, i as u32)) { (n + 1, u32::MAX, u32::MAX) } else { (n + 1, e.target.0, i as u32) };
                }
            }
        }
        // Checked from state built once, not per candidate (quadratic in a
        // big loop): per `X` the last instruction reading each value (edge
        // arguments last); per block the candidate phis it reads; per
        // (X, target) their union over the region; per candidate its edge
        // sources (all at (target, i), so each must be `X` or in the region).
        let mut cands: PgVec<(u32, u32, u32, u32, u32, u32)> = PgVec::new_in(m);
        for r in 0..nv {
            if rn(r) != r || func.values[r].is_float() || outside[r] != 0 || edge[r].0 == 0 || edge[r].1 == u32::MAX {
                continue;
            }
            let d = def_inst[r];
            if d == u32::MAX {
                continue;
            }
            let VInst::Op { block, idx } = self.insts[d as usize] else { continue };
            let (_, target, i) = edge[r];
            let inst = &func.blocks[block as usize].insts[idx as usize];
            // Result written after the last operand read (one instruction,
            // or a sequence that reads its operands first). The remainders
            // and unsigned division write the result before reading the
            // operands again (`sdiv d; msub d, d, r, n`).
            if !matches!(inst.op, Op::IntBin { .. } | Op::Icmp { .. } | Op::Select { .. } | Op::Conv { .. } | Op::Load { .. } | Op::LoadZext { .. } | Op::PtrAdd { .. }) {
                continue;
            }
            if matches!(inst.op, Op::IntBin { op: IntBinOp::Srem | IntBinOp::Udiv | IntBinOp::Urem, .. }) {
                continue;
            }
            let Some(&p) = func.blocks[target as usize].params.get(i as usize) else { continue };
            // A forwarded parameter (`forward_phis`) is the successor's phi.
            let p = rn(p.index());
            if self.phis[p].is_none() || func.values[p].is_float() {
                continue;
            }
            cands.push_hot((block, target, r as u32, i, p as u32, d));
        }
        if !cands.is_empty() {
            cands.sort_unstable();
            let nb = self.blocks.len();
            let mut pidx = filled_in(m, u32::MAX, total);
            let mut np = 0usize;
            for c in &cands {
                if pidx[c.4 as usize] == u32::MAX {
                    pidx[c.4 as usize] = np as u32;
                    np += 1;
                }
            }
            // D3698: per candidate phi the view blocks reading it, (phi index, block) pairs sorted: a candidate is read in a
            // region when one of its readers was walked. A bit row of every candidate per block (blocks x phis / 64
            // words, OR-ed per walked block) was the square of a chain of loops.
            let mut reads: PgVec<(u32, u32)> = PgVec::new_in(m);
            for b in 0..nb {
                let mut mark = |v: usize| {
                    let k = pidx[rn(v)];
                    if k != u32::MAX {
                        reads.push_hot((k, b as u32));
                    }
                };
                for i in self.blocks[b].inst_begin..self.blocks[b].inst_end {
                    for &v in self.inst_operands(i) {
                        mark(v as usize);
                    }
                }
                match self.blocks[b].orig {
                    Some(o) => func.blocks[o as usize].term.edges().for_each(|e| e.args.iter().for_each(|a| mark(a.index()))),
                    None => {
                        let sp = &self.splits[match self.insts[self.blocks[b].inst_begin as usize] {
                            VInst::SplitBr { split } => split as usize,
                            _ => unreachable!(),
                        }];
                        let e = func.blocks[sp.from as usize].term.edges().nth(sp.edge as usize).expect("a split's edge");
                        e.args.iter().for_each(|a| mark(a.index()));
                    }
                }
            }
            reads.sort_unstable();
            reads.dedup();
            let mut reads_begin = filled_in(m, 0u32, np + 1);
            for &(k, _) in &reads {
                reads_begin[k as usize + 1] += 1;
            }
            for k in 0..np {
                reads_begin[k + 1] += reads_begin[k];
            }
            let mut is_cand = filled_in(m, false, nv);
            for c in &cands {
                is_cand[c.2 as usize] = true;
            }
            let mut sources: PgVec<(u32, u32)> = PgVec::new_in(m);
            for (bi, b) in func.blocks.iter().enumerate() {
                for e in b.term.edges() {
                    for a in e.args.iter() {
                        let v = rn(a.index());
                        if v < nv && is_cand[v] {
                            sources.push_hot((v as u32, bi as u32));
                        }
                    }
                }
            }
            sources.sort_unstable();
            let mut seen = filled_in(m, 0u32, nb);
            let mut in_x: PgVec<(u32, u32)> = filled_in(m, (0, 0), total);
            let mut stack: PgVec<u32> = PgVec::new_in(m);
            let (mut epoch, mut x_epoch, mut cur_x) = (0u32, 0u32, usize::MAX);
            let mut g = 0;
            while g < cands.len() {
                crate::stop_point(); // D2909: each candidate's region walk can cover the function (O(blocks) per walk)
                let (x, target) = (cands[g].0 as usize, cands[g].1);
                let mut end = g + 1;
                while end < cands.len() && (cands[end].0 as usize, cands[end].1) == (x, target) {
                    end += 1;
                }
                if x != cur_x {
                    (cur_x, x_epoch) = (x, x_epoch + 1);
                    for i in self.blocks[x].inst_begin..self.blocks[x].inst_end {
                        for &v in self.inst_operands(i) {
                            in_x[rn(v as usize)] = (x_epoch, i);
                        }
                    }
                    for e in func.blocks[x].term.edges() {
                        for a in e.args.iter() {
                            in_x[rn(a.index())] = (x_epoch, u32::MAX);
                        }
                    }
                }
                epoch += 1;
                stack.clear();
                stack.extend_copy(&self.blocks[x].succs);
                while let Some(b) = stack.pop() {
                    let b = b as usize;
                    if b == target as usize || seen[b] == epoch {
                        continue;
                    }
                    seen[b] = epoch;
                    stack.extend_copy(&self.blocks[b].succs);
                }
                // `p` read in the region, `X` itself included, or in `X` after
                // `r`'s definition: by an instruction, the terminator, or one
                // of its edges (a parallel copy `a <- b` reads `b`'s register
                // at the edge, after `s` would have taken it).
                if seen[x] != epoch {
                    for &(_, _, r, _, p, d) in &cands[g..end] {
                        let k = pidx[p as usize] as usize;
                        let in_region = reads[reads_begin[k] as usize..reads_begin[k + 1] as usize].iter().any(|&(_, b)| seen[b as usize] == epoch);
                        let after_d = in_x[p as usize].0 == x_epoch && in_x[p as usize].1 > d;
                        if in_region || after_d {
                            continue;
                        }
                        let lo = sources.partition_point(|s| s.0 < r);
                        let hi = sources.partition_point(|s| s.0 <= r);
                        let found = sources[lo..hi].iter().filter(|s| s.1 as usize == x || seen[s.1 as usize] == epoch).count() as u32;
                        if found == edge[r as usize].0 {
                            self.phi_reuse[r as usize] = p;
                        }
                    }
                }
                g = end;
            }
        }
        self.def_inst = def_inst;
    }

    /// Phi forwarding: a block `J` whose terminator is an unconditional
    /// `br H args` and whose parameter `P` is read only there, at position
    /// `i`, is a join that only passes `P` on to `H`'s parameter `Ph` — the
    /// lowerer's join before a loop's back edge (`out` and `n` through
    /// a filtered group-by's `b20`). The allocator would pin `P` to a register of its own
    /// and move it into `Ph`'s on the edge, once per row. Instead `P`
    /// becomes `Ph`: the edges into `J` move straight into `Ph`'s register
    /// (`J` lists `Ph` among its phis with those incomings; the edge that
    /// passed `Ph` itself moves nothing, and a value defined in the arm is
    /// computed into the register by `pick_phi_reuse`). Sound because `Ph`
    /// is not read in `J` (checked: instructions, the terminator, the other
    /// edge arguments), so the register is free to take the new value at
    /// `J`'s entry, and every reader of `Ph` before the edge into `J` is in
    /// the source block, before its terminator's moves.
    #[inline]
    fn forward_phis(&mut self, func: &Function) {
        let nv = func.values.len();
        let uses = self.uses_snapshot();
        let mut rename = core::mem::replace(&mut self.rename, PgVec::new_in(self.mcx));
        // D1939: the rename table is a union-find while phis are forwarded (each value's representative found by following it,
        // with path halving), and one pass at the end points every value at its representative: rewriting the whole table per
        // forwarded phi was phis x values (73% of a 4,000-compare filter's compile).
        let rn = |rename: &mut PgVec<u32>, v: usize| find_rep(rename, v);
        for j in 0..func.blocks.len() {
            if j == func.entry.index() || self.blocks[j].phis.is_empty() {
                continue;
            }
            let Terminator::Br(e) = &func.blocks[j].term else { continue };
            let h = e.target.index();
            if h == j {
                continue;
            }
            // Values `J` reads outside the forwarded edge argument.
            let mut k = 0;
            while k < self.blocks[j].phis.len() {
                let p = self.blocks[j].phis[k];
                k += 1;
                let pr = rn(&mut rename, p.index());
                if pr != p.index() || uses[pr] != 1 {
                    continue;
                }
                let Some(i) = e.args.iter().position(|a| rn(&mut rename, a.index()) == pr) else { continue };
                let Some(&ph) = func.blocks[h].params.get(i) else { continue };
                let ph = rn(&mut rename, ph.index());
                if ph >= nv || ph == pr || self.phis[ph].is_none() || func.values[ph] != func.values[pr] {
                    continue;
                }
                // `Ph` read in `J`: by an instruction, or by another edge argument.
                let b = &self.blocks[j];
                let read = (b.inst_begin..b.inst_end).any(|ii| self.inst_operands(ii).iter().any(|&v| rn(&mut rename, v as usize) == ph))
                    || e.args.iter().enumerate().any(|(m, a)| m != i && rn(&mut rename, a.index()) == ph)
                    || self.blocks[j].phis.iter().any(|q| rn(&mut rename, q.index()) == ph);
                if read {
                    continue;
                }
                // `P` stays `J`'s phi (its own incoming list: the analyzer
                // visits a phi's list once per listing block, and the
                // compiler looks the edge's incoming up by block); only the
                // assignment is `Ph`'s, through the rename map.
                rename[pr] = ph as u32;
            }
        }
        for v in 0..rename.len() {
            rename[v] = find_rep(&mut rename, v) as u32;
        }
        self.share_loop_phis(func, &mut rename);
        self.rename = rename;
        self.uses = uses;
    }

    /// Block-local value reuse: a pure instruction (int/float arithmetic,
    /// compare, conversion, select, `ptr_add`) or a load whose operands
    /// through the rename map equal an earlier one's in the same block
    /// with no store or call between them *is* that earlier value, and a
    /// load from a stack slot at an offset the block stored to last, with
    /// nothing in between that writes memory, *is* the stored value (the
    /// hashagg run test staging the group image through the frame and
    /// reading it straight back: a filtered five-key text group-by's 4 loads and 2 duplicate key-lane
    /// loads per row). Renames only; `eliminate_dead` drops the doubles.
    fn reuse_values(&mut self, func: &Function) {
        // D1939: the keys are ordered (the IR's fieldless enums as their discriminants) so the available values are a map:
        // a linear search per instruction made one big function's compile quadratic in its size.
        #[derive(PartialEq, Eq, Hash)]
        enum Key {
            Bin(u8, u32, u32),
            Cmp(u8, u32, u32),
            Conv(u8, u8, u32),
            Sel(u32, u32, u32),
            Load(bool, u8, u32, i32, bool),
            Ptr(u32, u32, u8, i32),
        }
        let m = self.mcx;
        let nv = func.values.len();
        let mut rename = core::mem::replace(&mut self.rename, PgVec::new_in(m));
        let rn = |rename: &PgVec<u32>, v: Value| rename.get(v.index()).map_or(v.index(), |r| *r as usize) as u32;
        let mut is_stack = filled_in(m, false, nv);
        for (ii, vi) in self.insts.iter().enumerate() {
            if let VInst::Op { block, idx } = *vi {
                if matches!(func.blocks[block as usize].insts[idx as usize].op, Op::StackAddr { .. }) && self.results[ii] != u32::MAX {
                    is_stack[self.results[ii] as usize] = true;
                }
            }
        }
        // the first value per key; loads apart (a store forgets them all, the rest stay)
        let mut avail: PgFxHashMap<Key, u32> = map_in(m);
        let mut loads: PgFxHashMap<Key, u32> = map_in(m);
        let mut stored: PgVec<(u32, i32, u32, Type)> = PgVec::new_in(m);
        let mut alias: PgVec<(u32, usize, usize)> = PgVec::new_in(m);
        for b in 0..self.blocks.len() {
            avail.clear();
            loads.clear();
            stored.clear();
            for ii in self.blocks[b].inst_begin..self.blocks[b].inst_end {
                if self.fused[ii as usize] {
                    continue;
                }
                let VInst::Op { block, idx } = self.insts[ii as usize] else { continue };
                let op = &func.blocks[block as usize].insts[idx as usize].op;
                let r = self.results[ii as usize];
                let key = match op {
                    Op::Store { ptr, offset, value, .. } => {
                        loads.clear();
                        let p = rn(&rename, *ptr);
                        if (p as usize) >= nv || is_stack[p as usize] {
                            let ty = func.values[value.index()];
                            let new = ty.bytes() as i32;
                            stored.retain(|(q, o, _, t)| *q != p || *o + t.bytes() as i32 <= *offset || *offset + new <= *o);
                            stored.push_hot((p, *offset, rn(&rename, *value), ty));
                        } else {
                            stored.clear();
                        }
                        continue;
                    }
                    Op::Const { .. } | Op::StackAddr { .. } | Op::Prefetch { .. } | Op::IntUn { .. } | Op::IntOvf { .. } | Op::Crc32c { .. } | Op::AddHi { .. } | Op::FloatBin { .. } | Op::FloatUn { .. } | Op::Fcmp { .. } => continue,
                    Op::IntBin { op, a, b } => Key::Bin(*op as u8, rn(&rename, *a), rn(&rename, *b)),
                    Op::Icmp { cond, a, b } => Key::Cmp(*cond as u8, rn(&rename, *a), rn(&rename, *b)),
                    Op::Conv { op, to, a } => Key::Conv(*op as u8, *to as u8, rn(&rename, *a)),
                    Op::Select { cond, a, b } => Key::Sel(rn(&rename, *cond), rn(&rename, *a), rn(&rename, *b)),
                    Op::PtrAdd { base, index, scale, offset } => Key::Ptr(rn(&rename, *base), rn(&rename, *index), *scale, *offset),
                    Op::Load { ty, ptr, offset, unaligned } | Op::LoadZext { ty, ptr, offset, unaligned } => {
                        let p = rn(&rename, *ptr);
                        let zext = matches!(op, Op::LoadZext { .. });
                        if !zext && r != u32::MAX {
                            if let Some(&(_, _, v, _)) = stored.iter().find(|(q, o, _, t)| *q == p && *o == *offset && *t == *ty) {
                                alias.push_hot((ii, r as usize, v as usize));
                                continue;
                            }
                        }
                        Key::Load(zext, *ty as u8, p, *offset, *unaligned)
                    }
                    _ => {
                        avail.clear();
                        loads.clear();
                        stored.clear();
                        continue;
                    }
                };
                if r == u32::MAX || (r as usize) >= nv || rename.get(r as usize).is_some_and(|x| *x != r) {
                    continue;
                }
                let map = if matches!(key, Key::Load(..)) { &mut loads } else { &mut avail };
                // one hash per key: the first result under it, or `r` put there
                let had = map.len();
                let first = *put_or_get(map, key, r);
                if map.len() == had && (first as usize) < nv && func.values[first as usize] == func.values[r as usize] {
                    rename[r as usize] = first;
                    alias.push_hot((ii, r as usize, first as usize));
                    continue;
                }
            }
        }
        // D1939: the aliases as a union-find over the rename table (`find_rep`), then one pass that points every value at its
        // representative. Rewriting the whole table per alias was aliases x values.
        for (ii, dup, to) in alias {
            let to = find_rep(&mut rename, to);
            if to == dup {
                continue;
            }
            rename[dup] = to as u32;
            self.fused[ii as usize] = true;
            self.clear_operands(ii);
            self.results[ii as usize] = u32::MAX;
        }
        for v in 0..rename.len() {
            rename[v] = find_rep(&mut rename, v) as u32;
        }
        self.rename = rename;
    }

    /// The sibling-loop rule: a phi `P` of an inner loop's header whose
    /// every entry edge passes the same phi `Q` of the enclosing loop's
    /// header, where `Q` is read nowhere but on such entries and the inner
    /// loop leaves only to `Q`'s header or out of `Q`'s loop, *is* `Q` —
    /// the chunk loop's run state handed into its per-bit and straight
    /// roads and back. One assignment carries the state around the nest:
    /// the roads' phis share the chunk header's pins instead of each
    /// taking their own (two roads' `out`, live to the fold's end, held two
    /// pins through the whole function and the second road got none), and
    /// the store at the road's exit / load at its entry per chunk go away.
    fn share_loop_phis(&self, func: &Function, rename: &mut PgVec<'s, u32>) {
        let m = self.mcx;
        let nv = func.values.len();
        let nb = func.blocks.len();
        let rn = |rename: &PgVec<u32>, v: usize| rename.get(v).map_or(v, |r| *r as usize);
        let mut param_block = filled_in(m, u32::MAX, nv);
        for (bi, b) in func.blocks.iter().enumerate() {
            for p in &b.params {
                param_block[p.index()] = bi as u32;
            }
        }
        let live_edge = |bi: usize, ei: usize| !self.term_const[bi].is_some_and(|t| t != ei);
        // Reads the compiler emits: operands of the view's instructions
        // and the arguments of live edges (a folded `condbr`'s dead arm
        // names values it never moves).
        let mut uses = filled_in(m, 0u32, nv);
        for b in &self.blocks {
            for i in b.inst_begin..b.inst_end {
                for &v in self.inst_operands(i) {
                    if (v as usize) < nv {
                        uses[rn(rename, v as usize)] += 1;
                    }
                }
            }
        }
        for (bi, b) in func.blocks.iter().enumerate() {
            for (ei, e) in b.term.edges().enumerate() {
                if live_edge(bi, ei) {
                    for a in &e.args {
                        uses[rn(rename, a.index())] += 1;
                    }
                }
            }
        }
        // D3553: each loop's live entry edges (into its header from a block outside its body), in block then edge order,
        // from one walk of the function's edges. Each loop walked every block for them, once per loop below and again per
        // loop for every candidate parameter (`entry_reads`): cubic in the loops, and a 60-table join chain's or a LEFT
        // JOIN probe pipeline's hundreds of loops spent seconds of the compile here.
        let of_header = loops_by_header(&self.loops, nb, m);
        let mut entries: PgVec<PgVec<&Edge>> = filled_in(m, PgVec::new_in(m), self.loops.len());
        // Per entry edge argument (after renaming), the edge's source block: `entry_reads` counts a value's in a range.
        let mut entry_src: PgVec<(u32, u32)> = PgVec::new_in(m);
        for (bi, b) in func.blocks.iter().enumerate() {
            if bi % 256 == 0 {
                crate::stop_point(); // D3552: a function of many blocks takes a cancel inside its walks
            }
            for (ei, e) in b.term.edges().enumerate() {
                if e.target.index() >= nb || !live_edge(bi, ei) {
                    continue;
                }
                for &li in &of_header[e.target.index()] {
                    if !self.loops[li as usize].1[bi] {
                        entries[li as usize].push_hot(e);
                        entry_src.extend_hot(e.args.iter().map(|a| (rn(rename, a.index()) as u32, bi as u32)));
                    }
                }
            }
        }
        entry_src.sort_unstable();
        // Reads of `q` that are an argument of an edge from a block of
        // `dbody` entering a loop header from outside that loop's body (a
        // phi initialised once per entry, after `q`'s own header ran).
        let entry_reads = |q: usize, dbody: &Body| -> u32 {
            let lo = entry_src.partition_point(|&(v, _)| (v as usize) < q);
            entry_src[lo..].iter().take_while(|&&(v, _)| v as usize == q).filter(|&&(_, bi)| dbody[bi as usize]).count() as u32
        };
        // D3698: per value, the original blocks reading it (operands of their instructions and the arguments of all their
        // edges, after renaming), so a candidate's "read inside the body" test reads its readers, not the whole body: a
        // walk of every body per loop was quadratic in a join chain's nested probe loops.
        let mut read_at: PgVec<(u32, u32)> = PgVec::new_in(m);
        for bi in 0..nb {
            for ii in self.blocks[bi].inst_begin..self.blocks[bi].inst_end {
                for &v in self.inst_operands(ii) {
                    if (v as usize) < nv {
                        read_at.push_hot((rn(rename, v as usize) as u32, bi as u32));
                    }
                }
            }
            for e in func.blocks[bi].term.edges() {
                for a in &e.args {
                    read_at.push_hot((rn(rename, a.index()) as u32, bi as u32));
                }
            }
        }
        read_at.sort_unstable();
        let read_in = |q: usize, body: &Body| {
            let lo = read_at.partition_point(|&(v, _)| (v as usize) < q);
            read_at[lo..].iter().take_while(|&&(v, _)| v as usize == q).any(|&(_, bi)| body[bi as usize])
        };
        let mut pairs: PgVec<(usize, usize)> = PgVec::new_in(m);
        for (li, (h, body)) in self.loops.iter().enumerate() {
            crate::stop_point(); // D3552: a loop's walks cover its body (a 9-way LEFT JOIN's probe pipeline holds hundreds of loops)
            let np = func.blocks[*h].params.len();
            if np == 0 || *h == func.entry.index() {
                continue;
            }
            // The entry edges' arguments, one value per parameter or none.
            let mut args: PgVec<Option<usize>> = filled_in(m, None, np);
            for e in &entries[li] {
                for (i, a) in e.args.iter().enumerate().take(np) {
                    let q = rn(rename, a.index());
                    args[i] = match args[i] {
                        Some(prev) if prev != q => Some(usize::MAX),
                        _ => Some(q),
                    };
                }
            }
            if entries[li].is_empty() {
                continue;
            }
            for (i, &p) in func.blocks[*h].params.iter().enumerate() {
                let p = p.index();
                let Some(q) = args[i] else { continue };
                if q == usize::MAX || q >= nv || q == p || rn(rename, p) != p || self.phis[p].is_none() || self.phis[q].is_none() || func.values[p] != func.values[q] || read_in(q, body) {
                    continue;
                }
                let d = param_block[q];
                if d == u32::MAX || body[d as usize] {
                    continue;
                }
                // `Q`'s block heads a loop around this one, left only through it.
                let Some(dbody) = of_header[d as usize].iter().map(|&dl| &self.loops[dl as usize].1).find(|db| db[*h]) else { continue };
                if uses[q] != entry_reads(q, dbody) {
                    continue;
                }
                let leaves_only_to_d = body.ones().all(|bi| {
                    func.blocks[bi].term.edges().enumerate().all(|(ei, e)| {
                        let t = e.target.index();
                        !live_edge(bi, ei) || body[t] || t == d as usize || !dbody[t]
                    })
                });
                if leaves_only_to_d {
                    pairs.push_hot((p, q));
                }
            }
        }
        // D3698: each pair renames `p` to `q`'s value and every value named `p` with it. The table is flat (every entry
        // names a root), so the second half is a forward of the value `p` (while `p` still names any entry: a root no
        // earlier pair forwarded), resolved for every entry once at the end; a rewrite of the whole table per pair was
        // pairs x values.
        if !pairs.is_empty() {
            let n = rename.len();
            let mut fwd: PgVec<u32> = (0..n as u32).collect_hot(m);
            let resolve = |fwd: &mut PgVec<u32>, v: usize| find_rep(fwd, v);
            for (p, q) in pairs {
                let q = resolve(&mut fwd, rn(rename, q));
                if q == p {
                    continue;
                }
                rename[p] = q as u32;
                if fwd[p] as usize == p {
                    fwd[p] = q as u32;
                }
            }
            for v in 0..n {
                rename[v] = resolve(&mut fwd, rename[v] as usize) as u32;
            }
        }
    }

    /// A block parameter whose incoming values (after renaming) are all one
    /// value other than itself is a copy of that value: drop the phi and
    /// map the parameter to the value. Iterates to a fixpoint for chains.
    fn propagate_copies(&mut self, func: &Function, rules: &dyn ImmRules) {
        let m = self.mcx;
        let nb = func.blocks.len();
        let total = func.values.len() + self.num_slots as usize;
        let mut rename: PgVec<u32> = (0..total as u32).collect_hot(m);
        let mut changed = true;
        while changed {
            changed = false;
            for b in 0..nb {
                let mut k = 0;
                while k < self.blocks[b].phis.len() {
                    let p = self.blocks[b].phis[k];
                    k += 1;
                    let Some(phi) = &self.phis[p.index()] else { continue };
                    let mut src: Option<u32> = None;
                    let mut ok = !phi.incoming.is_empty();
                    for &(_, a) in &phi.incoming {
                        let a = rename[a.index()];
                        if a == p.0 {
                            ok = false;
                            break;
                        }
                        match src {
                            None => src = Some(a),
                            Some(s) if s == a => {}
                            Some(_) => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if let (true, Some(s)) = (ok, src) {
                        rename[p.index()] = s;
                        self.phis[p.index()] = None;
                        self.blocks[b].phis.remove(k - 1);
                        k -= 1;
                        changed = true;
                    }
                }
            }
        }
        // Pointer/integer conversions are no-ops in the register convention
        // (every int and pointer is one 64-bit GP register), and so is the
        // sign extension of an integer (narrow integers are kept
        // sign-extended to 64 bits in registers, `canon`): the result is
        // its operand and the instruction is skipped. Not for a constant
        // operand: a constant all of whose uses fold is never materialized,
        // and the renamed result's uses would not fold. Not for a bool
        // (0/1, not sign-extended).
        for ii in 0..self.insts.len() {
            let VInst::Op { block, idx } = self.insts[ii] else { continue };
            let inst = &func.blocks[block as usize].insts[idx as usize];
            let (Some(r), Op::Conv { op, a, .. }) = (inst.result, &inst.op) else {
                continue;
            };
            let copy = match op {
                ConvOp::PtrToInt | ConvOp::IntToPtr => true,
                ConvOp::Sext => rules.sext_copy() && func.values[a.index()].is_int() && func.values[a.index()] != Type::Bool && func.values[r.index()].is_int(),
                _ => false,
            };
            if !copy || self.const_of[a.index()].is_some() {
                continue;
            }
            rename[r.index()] = rename[a.index()];
            self.fused[ii] = true;
            self.clear_operands(ii as u32);
            // No result either: the analyzer would count a definition of
            // the source.
            self.results[ii] = u32::MAX;
        }
        // A `Bool` is 0/1 in its register, so `zext` of one is a copy, and so
        // are `select c, 1, 0` and `icmp ne x, 0` of a boolean-valued `x` (a
        // `Bool`, a `zext` of one, or an `iand`/`ior`/`ixor` of two): the
        // register already holds the result. Not for a constant source.
        // Lets `fuse_compares` see `condbr (iand cmp1 cmp2)` through the
        // widening the IR spells out.
        let mut boolish: PgVec<bool> = func.values.iter().map(|t| *t == Type::Bool).collect_hot(m);
        boolish.resize_in(total, false);
        for ii in 0..self.insts.len() {
            let VInst::Op { block, idx } = self.insts[ii] else { continue };
            let inst = &func.blocks[block as usize].insts[idx as usize];
            let Some(r) = inst.result else { continue };
            let is_one = |v: Value| self.const_of[v.index()].is_some_and(|(t, bits)| const_canon(t, bits) == 1);
            let is_zero = |v: Value| self.const_of[v.index()].is_some_and(|(t, bits)| const_canon(t, bits) == 0);
            let src = match inst.op {
                Op::Conv { op: ConvOp::Zext, a, .. } if boolish[a.index()] && self.const_of[a.index()].is_none() => Some(a),
                Op::Select { cond, a, b } if boolish[cond.index()] && is_one(a) && is_zero(b) && self.const_of[cond.index()].is_none() => Some(cond),
                Op::Icmp { cond: IntCond::Ne, a, b } if boolish[a.index()] && is_zero(b) && self.const_of[a.index()].is_none() => Some(a),
                Op::IntBin { op: IntBinOp::And | IntBinOp::Or | IntBinOp::Xor, a, b } if boolish[a.index()] && boolish[b.index()] => {
                    boolish[r.index()] = true;
                    None
                }
                _ => None,
            };
            let Some(src) = src else { continue };
            boolish[r.index()] = true;
            rename[r.index()] = rename[src.index()];
            self.fused[ii] = true;
            self.clear_operands(ii as u32);
            self.results[ii] = u32::MAX;
        }
        // An identity operation is a copy too: `x + 0`, `0 + x`, `x | 0`,
        // `x ^ 0`, `x - 0`, `x << 0`, `x >> 0` (the lowerer's running
        // offsets start from a constant zero), `x * 1`, and a Bool negated
        // twice (the lowerer's three-valued logic spells `not` as `xor _,
        // true`). Same caveats: not for a constant source, and the types
        // agree by construction.
        let mut not_of = filled_in(m, u32::MAX, total);
        for (ii, vi) in self.insts.iter().enumerate() {
            if let VInst::Op { block, idx } = *vi {
                if let Some(r) = func.blocks[block as usize].insts[idx as usize].result {
                    not_of[r.index()] = ii as u32;
                }
            }
        }
        for ii in 0..self.insts.len() {
            let VInst::Op { block, idx } = self.insts[ii] else { continue };
            let inst = &func.blocks[block as usize].insts[idx as usize];
            let (Some(r), Op::IntBin { op, a, b }) = (inst.result, &inst.op) else {
                continue;
            };
            let zero = |v: Value| self.const_of[v.index()].is_some_and(|(t, bits)| const_canon(t, bits) == 0);
            let one = |v: Value| self.const_of[v.index()].is_some_and(|(t, bits)| const_canon(t, bits) == 1);
            // `not (not y)`: a Bool `xor _, true` of a Bool `xor y, true` is `y`
            let not = |v: Value| match not_of[v.index()] {
                u32::MAX => None,
                d => match self.insts[d as usize] {
                    VInst::Op { block, idx } => Some(&func.blocks[block as usize].insts[idx as usize].op),
                    _ => None,
                }
                .and_then(|op| match *op {
                    Op::IntBin { op: IntBinOp::Xor, a, b } if one(b) && self.const_of[a.index()].is_none() => Some(a),
                    Op::IntBin { op: IntBinOp::Xor, a, b } if one(a) && self.const_of[b.index()].is_none() => Some(b),
                    _ => None,
                }),
            };
            let bool_ty = func.values[r.index()] == Type::Bool;
            let src = match op {
                IntBinOp::Add | IntBinOp::Or | IntBinOp::Xor if zero(*b) => *a,
                IntBinOp::Add | IntBinOp::Or | IntBinOp::Xor if zero(*a) => *b,
                IntBinOp::Sub | IntBinOp::Shl | IntBinOp::Ushr | IntBinOp::Sshr if zero(*b) => *a,
                IntBinOp::Mul if !bool_ty && one(*b) => *a,
                IntBinOp::Mul if !bool_ty && one(*a) => *b,
                IntBinOp::Xor if bool_ty && one(*b) && not(*a).is_some() => not(*a).unwrap(),
                IntBinOp::Xor if bool_ty && one(*a) && not(*b).is_some() => not(*b).unwrap(),
                _ => continue,
            };
            if self.const_of[src.index()].is_some() || func.values[src.index()] != func.values[r.index()] {
                continue;
            }
            rename[r.index()] = rename[src.index()];
            self.fused[ii] = true;
            self.clear_operands(ii as u32);
            self.results[ii] = u32::MAX;
        }
        // A `ptr_add` whose constant index and offset sum to zero (`base + 0 * s + 0`: the lowerer's state pointer
        // spelled from a stack slot) is its base.
        for ii in 0..self.insts.len() {
            let VInst::Op { block, idx } = self.insts[ii] else { continue };
            let inst = &func.blocks[block as usize].insts[idx as usize];
            let (Some(r), Op::PtrAdd { base, index, scale, offset }) = (inst.result, &inst.op) else {
                continue;
            };
            let Some((t, bits)) = self.const_of[index.index()] else { continue };
            if self.const_of[base.index()].is_some() || const_canon(t, bits).wrapping_mul(*scale as i64).wrapping_add(*offset as i64) != 0 {
                continue;
            }
            rename[r.index()] = rename[base.index()];
            self.fused[ii] = true;
            self.clear_operands(ii as u32);
            self.results[ii] = u32::MAX;
        }
        // Chains resolve through the fixpoint above; make the map direct.
        for v in 0..total {
            let mut t = rename[v];
            while rename[t as usize] != t {
                t = rename[t as usize];
            }
            rename[v] = t;
        }
        self.rename = rename;
    }

    /// `direct_uses` from the instructions' current operand ranges and the
    /// edge arguments (built once; `set_operands` keeps it exact).
    fn build_direct_uses(&mut self, func: &Function) {
        refill(&mut self.direct_uses, self.num_values as usize + self.num_slots as usize, 0);
        for i in 0..self.insts.len() as u32 {
            for k in self.ops_begin[i as usize]..self.ops_end[i as usize] {
                self.direct_uses[self.operands[k as usize] as usize] += 1;
            }
        }
        for b in &func.blocks {
            for e in b.term.edges() {
                for a in &e.args {
                    self.direct_uses[a.index()] += 1;
                }
            }
        }
    }

    /// Use count per value through the rename map (a renamed value's uses
    /// are its source's), as of now: a pass takes it at its start and puts
    /// it back (`self.uses = uses`) so the storage is reused.
    fn uses_snapshot(&mut self) -> PgVec<'s, u32> {
        let mut uses = core::mem::replace(&mut self.uses, PgVec::new_in(self.mcx));
        refill(&mut uses, self.direct_uses.len(), 0);
        for (v, &n) in self.direct_uses.iter().enumerate() {
            uses[self.rename.get(v).map_or(v, |r| *r as usize)] += n;
        }
        uses
    }

    /// Defining instruction per value (`u32::MAX`: a parameter, or none),
    /// as of now; `live_only` leaves fused instructions out. Taken and
    /// put back like `uses_snapshot`.
    fn defs_snapshot(&mut self, live_only: bool) -> PgVec<'s, u32> {
        let mut def_inst = core::mem::replace(&mut self.def_inst, PgVec::new_in(self.mcx));
        refill(&mut def_inst, self.num_values as usize + self.num_slots as usize, u32::MAX);
        for (i, &r) in self.results.iter().enumerate() {
            if r != u32::MAX && !(live_only && self.fused[i]) {
                def_inst[r as usize] = i as u32;
            }
        }
        def_inst
    }

    /// Skip pure instructions whose result nothing uses: the lowerer builds
    /// constants and conversions it then folds away (a Datum constant
    /// truncated to its width, a null flag OR'd with `false`), and TPDE
    /// would materialize each of them. Iterates to a fixpoint so an operand
    /// that only fed a dead instruction goes too.
    fn eliminate_dead(&mut self, func: &Function) {
        loop {
            let uses = self.uses_snapshot();
            let mut dead = PgVec::new_in(self.mcx);
            for (ii, vi) in self.insts.iter().enumerate() {
                if self.fused[ii] {
                    continue;
                }
                let VInst::Op { block, idx } = *vi else { continue };
                let r = self.results[ii];
                if r == u32::MAX || uses[r as usize] != 0 {
                    continue;
                }
                let pure = matches!(
                    func.blocks[block as usize].insts[idx as usize].op,
                    Op::Const { .. } | Op::IntBin { .. } | Op::IntOvf { .. } | Op::IntUn { .. } | Op::Icmp { .. } | Op::FloatBin { .. } | Op::FloatUn { .. } | Op::Fcmp { .. } | Op::Conv { .. } | Op::Select { .. } | Op::Load { .. } | Op::LoadZext { .. } | Op::StackAddr { .. } | Op::PtrAdd { .. } | Op::Crc32c { .. } | Op::AddHi { .. }
                        | Op::VecLoad { .. } | Op::VecBin { .. } | Op::VecShift { .. } | Op::VecCmp { .. } | Op::VecSelect { .. } | Op::VecReduceAdd { .. } | Op::VecMovmask { .. } | Op::VecSplat { .. } | Op::VecExtract { .. } | Op::VecInsert { .. } | Op::VecWiden { .. } | Op::VecNarrow { .. }
                );
                if pure {
                    dead.push_hot(ii as u32);
                }
            }
            self.uses = uses;
            if dead.is_empty() {
                return;
            }
            for ii in dead {
                self.fused[ii as usize] = true;
                self.clear_operands(ii);
            }
        }
    }

    /// Fuse `Icmp`/`Fcmp` (and a small constant right operand) into the
    /// `CondBr` that is their only use, within the same block.
    fn fuse_compares(&mut self, func: &Function, rules: &dyn ImmRules) {
        let m = self.mcx;
        let uses = self.uses_snapshot();
        let def_inst = self.defs_snapshot(false);
        let rename = copied_in(&self.rename, m);
        let rn = |v: usize| rename.get(v).map_or(v, |r| *r as usize);
        // The instruction defining `v` in block `bi`, if `v` has one use.
        let single_def = |this: &Self, bi: usize, v: Value| -> Option<(u32, u32)> {
            if uses[v.index()] != 1 {
                return None;
            }
            let ci = def_inst[v.index()];
            if ci == u32::MAX {
                return None;
            }
            let VInst::Op { block, idx } = this.insts[ci as usize] else { return None };
            (block as usize == bi).then_some((ci, idx))
        };
        for (bi, b) in func.blocks.iter().enumerate() {
            let Terminator::CondBr { cond, .. } = &b.term else { continue };
            // The condition and the operands below may be copies
            // (`propagate_copies`): the fusion looks at what they stand for.
            let cond = Value(rn(cond.index()) as u32);
            let Some((ci, idx)) = single_def(self, bi, cond) else { continue };
            // `condbr (iand x y)` / `condbr (ior x y)` on a target with
            // conditional compares: `x` (or `y`) a compare of this block
            // used once becomes the flag-setting compare, the other side a
            // `ccmp` on the same flags (a second compare of this block used
            // once, or a `Bool` register tested against zero), and the
            // branch reads the flags once.
            if rules.ccmp() {
                if let Op::IntBin { op: bop @ (IntBinOp::And | IntBinOp::Or), a, b: bb } = b.insts[idx as usize].op {
                    let (a, bb) = (Value(rn(a.index()) as u32), Value(rn(bb.index()) as u32));
                    if func.values[a.index()] == Type::Bool && func.values[bb.index()] == Type::Bool {
                        let is_cmp = |this: &Self, v: Value| -> bool {
                            single_def(this, bi, v).is_some_and(|(_, i)| match b.insts[i as usize].op {
                                Op::Icmp { .. } | Op::Fcmp { .. } => true,
                                Op::IntOvf { op: IntOvfOp::SaddOvf | IntOvfOp::SsubOvf, a, .. } => matches!(func.values[a.index()], Type::I32 | Type::I64),
                                _ => false,
                            })
                        };
                        // A plain integer `icmp` (register right operand, or
                        // a constant used only here that fits `imm5`) can be
                        // the `ccmp`; prefer it as the second so the first
                        // keeps the richer forms (self-extension, overflow,
                        // float).
                        let ccmp_able = |this: &Self, v: Value| -> bool {
                            single_def(this, bi, v).is_some_and(|(_, i)| match b.insts[i as usize].op {
                                Op::Icmp { a, b: r, .. } => {
                                    func.values[a.index()] != Type::Bool
                                        && match this.const_of[r.index()] {
                                            Some((t, bits)) => rules.ccmp_imm(const_canon(t, bits)),
                                            None => true,
                                        }
                                }
                                _ => false,
                            })
                        };
                        // A nested `iand (iand x y) z` / `ior (ior x y) z` on
                        // either side (the inner one of this block, used once):
                        // three conditions, one compare and two `ccmp`s.
                        let inner = |this: &Self, v: Value| -> Option<(u32, Value, Value)> {
                            single_def(this, bi, v).and_then(|(ii, i)| match b.insts[i as usize].op {
                                Op::IntBin { op, a: x, b: y } if op == bop => {
                                    let (x, y) = (Value(rn(x.index()) as u32), Value(rn(y.index()) as u32));
                                    (func.values[x.index()] == Type::Bool && func.values[y.index()] == Type::Bool).then_some((ii, x, y))
                                }
                                _ => None,
                            })
                        };
                        let (inner_inst, leaves): (Option<u32>, PgVec<Value>) = match (inner(self, a), inner(self, bb)) {
                            (Some((ii, x, y)), _) => (Some(ii), pg_vec![in m; x, y, bb]),
                            (None, Some((ii, x, y))) => (Some(ii), pg_vec![in m; a, x, y]),
                            _ => (None, pg_vec![in m; a, bb]),
                        };
                        // The flag-setting compare: a compare leaf, preferring
                        // one the `ccmp` cannot take (self-extension, overflow,
                        // float); every other leaf a `ccmp`-able compare or a
                        // `Bool`.
                        let first_at = (0..leaves.len())
                            .find(|&k| is_cmp(self, leaves[k]) && !ccmp_able(self, leaves[k]))
                            .or_else(|| (0..leaves.len()).find(|&k| is_cmp(self, leaves[k])));
                        let pick = first_at.and_then(|k| {
                            let rest: PgVec<Value> = (0..leaves.len()).filter(|&j| j != k).map(|j| leaves[j]).collect_hot(m);
                            rest.iter().all(|&v| ccmp_able(self, v) || !is_cmp(self, v)).then_some((leaves[k], rest))
                        });
                        if let Some((first, rest)) = pick {
                            let (fi, fidx) = single_def(self, bi, first).unwrap();
                            let mut fuse = self.fuse_one_compare(func, rules, &uses, &def_inst, &rn, bi, fi, fidx);
                            let mut ops = pg_vec![in m; fuse.a.0];
                            if let Some(b) = fuse.b {
                                ops.push_hot(b.0);
                            }
                            for (n, &second) in rest.iter().enumerate() {
                                let sec = match single_def(self, bi, second) {
                                    Some((si, sidx)) if ccmp_able(self, second) => {
                                        let Op::Icmp { cond: c, a: sa, b: sb } = b.insts[sidx as usize].op else { unreachable!() };
                                        let mut cc = Ccmp { a: sa, b: Some(sb), imm: 0, icond: c, or: bop == IntBinOp::Or };
                                        if let Some((t, bits)) = self.const_of[sb.index()] {
                                            // `ccmp_able` checked the range; the
                                            // constant's fate is `fuse_immediates`'.
                                            cc.b = None;
                                            cc.imm = const_canon(t, bits) as u32;
                                        }
                                        self.fused[si as usize] = true;
                                        self.clear_operands(si);
                                        self.results[si as usize] = u32::MAX;
                                        cc
                                    }
                                    // Any Bool value: `ccmp y, #0, nzcv, cond; b.ne`.
                                    _ => Ccmp { a: second, b: None, imm: 0, icond: IntCond::Ne, or: bop == IntBinOp::Or },
                                };
                                if n == 0 {
                                    fuse.second = Some(sec);
                                } else {
                                    fuse.third = Some(sec);
                                }
                                ops.push_hot(sec.a.0);
                                if let Some(b) = sec.b {
                                    ops.push_hot(b.0);
                                }
                            }
                            for &ii in inner_inst.iter().chain(core::iter::once(&ci)) {
                                self.fused[ii as usize] = true;
                                self.clear_operands(ii);
                                self.results[ii as usize] = u32::MAX;
                            }
                            let ti = self.blocks[bi].inst_end - 1;
                            debug_assert!(matches!(self.insts[ti as usize], VInst::Term { .. }));
                            self.set_operands(ti, &ops);
                            self.term_fuse[bi] = Some(fuse);
                            continue;
                        }
                    }
                }
            }
            match b.insts[idx as usize].op {
                Op::Icmp { .. } | Op::Fcmp { .. } => {}
                Op::IntOvf { op: IntOvfOp::SaddOvf | IntOvfOp::SsubOvf, a, .. } if matches!(func.values[a.index()], Type::I32 | Type::I64) => {}
                _ => continue,
            }
            let fuse = self.fuse_one_compare(func, rules, &uses, &def_inst, &rn, bi, ci, idx);
            // The terminator now reads the compare operands (liveness).
            let ti = self.blocks[bi].inst_end - 1;
            debug_assert!(matches!(self.insts[ti as usize], VInst::Term { .. }));
            let mut ops = pg_vec![in m; fuse.a.0];
            if let Some(b) = fuse.b {
                ops.push_hot(b.0);
            }
            self.set_operands(ti, &ops);
            self.term_fuse[bi] = Some(fuse);
        }
        self.uses = uses;
        self.def_inst = def_inst;
    }

    /// Fuse the compare (`Icmp`, `Fcmp`, or a 32/64-bit signed add/sub
    /// overflow predicate) at instruction `ci` (`func.blocks[bi].insts[idx]`)
    /// into a `FusedCmp`: the instruction is skipped, a constant right
    /// operand folds into the immediate, and `icmp a, sext(trunc(a))` into
    /// the self-extension form. The caller wires the terminator.
    #[allow(clippy::too_many_arguments)]
    fn fuse_one_compare(&mut self, func: &Function, rules: &dyn ImmRules, uses: &[u32], def_inst: &[u32], rn: &dyn Fn(usize) -> usize, bi: usize, ci: u32, idx: u32) -> FusedCmp {
        let b = &func.blocks[bi];
        let inst = &b.insts[idx as usize];
        let (a, bv, icond, fcond, is_f64, ovf) = match inst.op {
            Op::Icmp { cond, a, b } => (a, b, Some(cond), None, false, None),
            Op::Fcmp { cond, a, b } => (a, b, None, Some(cond), func.values[a.index()] == Type::F64, None),
            // Signed add/sub overflow at 32/64 bits: the flag-setting op
            // feeds the branch directly (adds + b.vs / add + jo).
            Op::IntOvf { op: op @ (IntOvfOp::SaddOvf | IntOvfOp::SsubOvf), a, b } => (a, b, None, None, false, Some((op, func.values[a.index()].bytes() * 8))),
            _ => unreachable!("fuse_one_compare on a non-compare"),
        };
        // Fold a constant right operand into the compare immediate.
        let mut fuse = FusedCmp { a, b: Some(bv), imm: 0, icond, fcond, is_f64, ovf, self_ext: None, second: None, third: None };
        // `icmp a, sext(trunc(a))`: the narrow-overflow test. Fold the two
        // conversions into a sign-extending compare. With `sext_copy`
        // the `sext` was renamed to the truncation already, so the
        // compare's right operand is the `trunc` itself.
        if matches!(icond, Some(IntCond::Eq | IntCond::Ne)) && uses[rn(bv.index())] == 1 && func.values[a.index()] == Type::I64 {
            let bvr = rn(bv.index());
            let si = def_inst[bvr];
            // (sext instruction, trunc instruction)
            let mut pair: Option<(Option<u32>, u32)> = None;
            if si != u32::MAX {
                if let VInst::Op { block: sb, idx: sidx } = self.insts[si as usize] {
                    if sb as usize == bi {
                        match func.blocks[bi].insts[sidx as usize].op {
                            Op::Conv { op: ConvOp::Sext, to: Type::I64, a: narrow } => {
                                if uses[narrow.index()] == 1 {
                                    let ti = def_inst[narrow.index()];
                                    if ti != u32::MAX {
                                        if let VInst::Op { block: tb, idx: tidx } = self.insts[ti as usize] {
                                            if tb as usize == bi {
                                                if let Op::Conv { op: ConvOp::Trunc, to, a: src } = func.blocks[bi].insts[tidx as usize].op {
                                                    if rn(src.index()) == rn(a.index()) && matches!(to, Type::I8 | Type::I16 | Type::I32) {
                                                        pair = Some((Some(si), ti));
                                                        fuse.self_ext = Some(to.bytes() * 8);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Op::Conv { op: ConvOp::Trunc, to, a: src } => {
                                if rn(src.index()) == rn(a.index()) && matches!(to, Type::I8 | Type::I16 | Type::I32) {
                                    pair = Some((None, si));
                                    fuse.self_ext = Some(to.bytes() * 8);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            if let Some((si, ti)) = pair {
                fuse.b = None;
                for i in si.into_iter().chain(core::iter::once(ti)) {
                    self.fused[i as usize] = true;
                    self.clear_operands(i);
                    self.results[i as usize] = u32::MAX;
                }
            }
        }
        // The constant may sit in any block: it has no operands, so
        // folding it into this compare only removes its materialization.
        // Whatever the constant's other uses: the terminator then reads it
        // no more, and `fuse_immediates` decides whether it is ever
        // materialized (never, when every remaining use folds too).
        if icond.is_some() && fuse.b.is_some() {
            if let Some((ty, bits)) = self.const_of[bv.index()] {
                let v = const_canon(ty, bits);
                if rules.branch_cmp_imm(v) {
                    fuse.b = None;
                    fuse.imm = v as u64;
                }
            }
        }
        let _ = uses;
        self.fused[ci as usize] = true;
        self.clear_operands(ci);
        fuse
    }

    /// Fuse constants into immediate operands. A `Const` all of whose uses
    /// accept it as an immediate is never materialized.
    fn fuse_immediates(&mut self, func: &Function, rules: &dyn ImmRules) {
        let uses = self.uses_snapshot();
        let mut foldable = filled_in(self.mcx, 0u32, uses.len());
        for (ii, vi) in self.insts.iter().enumerate() {
            if self.fused[ii] {
                continue;
            }
            let VInst::Op { block, idx } = *vi else { continue };
            let inst = &func.blocks[block as usize].insts[idx as usize];
            if let Some(c) = imm_operand(func, &self.const_of, &inst.op, rules) {
                foldable[c.index()] += 1;
            }
        }
        // An edge argument that is a constant is materialized into the
        // phi's register (or slot) at the edge (`branches::move_to_phi`,
        // `IrAdaptor::val_const`), so it never keeps the constant alive.
        if rules.phi_const() {
            for b in &func.blocks {
                for e in b.term.edges() {
                    for a in &e.args {
                        if self.const_of[a.index()].is_some() {
                            foldable[a.index()] += 1;
                        }
                    }
                }
            }
        }
        for (ii, vi) in self.insts.iter().enumerate() {
            let VInst::Op { block, idx } = *vi else { continue };
            let inst = &func.blocks[block as usize].insts[idx as usize];
            if let (Some(r), Op::Const { .. }) = (inst.result, &inst.op) {
                if foldable[r.index()] == uses[r.index()] {
                    self.fused[ii] = true;
                    self.const_fused[r.index()] = true;
                }
            }
        }
        self.uses = uses;
    }

    /// Fold a narrow-overflow test and a shifted operand into one
    /// instruction (AArch64 forms; `ImmRules` says whether the target has
    /// them):
    ///
    /// * `icmp eq/ne a, trunc(a)` (the `sext` back to 64 bits is a copy,
    ///   renamed away above) is `cmp a, a, sxt{b,h,w}`: the truncation is
    ///   fused away and the compare's operands become `[a]`.
    /// * `or/and/xor/add/sub a, shl(x, k)` with a constant `k` is the
    ///   shifted-register form: the shift is fused away and the operands
    ///   become `[other, x]`. Only for 64-bit operations (a narrow shift's
    ///   result is canonicalized, which the fold would skip).
    fn fuse_narrow_ops(&mut self, func: &Function, rules: &dyn ImmRules) {
        if !rules.self_ext_cmp() && !rules.shifted_operands() && !rules.bitfield_extract() {
            return;
        }
        let m = self.mcx;
        let uses = self.uses_snapshot();
        let def_inst = self.defs_snapshot(true);
        let rn = |v: usize| self.rename.get(v).map_or(v, |r| *r as usize);
        let nv = func.values.len();
        let mut self_ext: PgVec<(u32, u32)> = PgVec::new_in(m);
        let mut shifted: PgVec<(u32, u32, u32, u32, u32)> = PgVec::new_in(m);
        let mut sbfx: PgVec<(u32, u32, u32)> = PgVec::new_in(m);
        let mut fuse_away: PgVec<u32> = PgVec::new_in(m);
        for ii in 0..self.insts.len() {
            if self.fused[ii] {
                continue;
            }
            let VInst::Op { block, idx } = self.insts[ii] else { continue };
            let inst = &func.blocks[block as usize].insts[idx as usize];
            match inst.op {
                Op::Icmp { cond: IntCond::Eq | IntCond::Ne, a, b } if rules.self_ext_cmp() => {
                    let (a, b) = (rn(a.index()), rn(b.index()));
                    if a >= nv || b >= nv || uses[b] != 1 || def_inst[b] == u32::MAX {
                        continue;
                    }
                    let db = def_inst[b];
                    let VInst::Op { block: tb, idx: tidx } = self.insts[db as usize] else { continue };
                    let Op::Conv { op: ConvOp::Trunc, to, a: src } = func.blocks[tb as usize].insts[tidx as usize].op else { continue };
                    if rn(src.index()) != a || !matches!(to, Type::I8 | Type::I16 | Type::I32) || !matches!(func.values[a], Type::I64) {
                        continue;
                    }
                    self_ext.push_hot((ii as u32, to.bytes() * 8));
                    fuse_away.push_hot(db);
                }
                Op::Conv { op: ConvOp::Trunc, to, a: s } if rules.bitfield_extract() => {
                    // `trunc (ushr x k)`: the field `x[k .. k + w)` sign-extended,
                    // one `sbfx` for the shift, the move and the extension.
                    let s = rn(s.index());
                    let w = to.bytes() * 8;
                    if s >= nv || w >= 64 || uses[s] != 1 || def_inst[s] == u32::MAX {
                        continue;
                    }
                    let ds = def_inst[s];
                    let VInst::Op { block: sb, idx: sidx } = self.insts[ds as usize] else { continue };
                    let Op::IntBin { op: IntBinOp::Ushr, a: x, b: k } = func.blocks[sb as usize].insts[sidx as usize].op else { continue };
                    let Some((kt, kb)) = self.const_of[k.index()] else { continue };
                    let k = const_canon(kt, kb);
                    if !(1..64).contains(&k) || k as u32 + w > 64 || func.values[x.index()] != Type::I64 {
                        continue;
                    }
                    sbfx.push_hot((ii as u32, rn(x.index()) as u32, k as u32));
                    fuse_away.push_hot(ds);
                }
                Op::IntBin { op: op @ (IntBinOp::Or | IntBinOp::And | IntBinOp::Xor | IntBinOp::Add | IntBinOp::Sub), a, b } if rules.shifted_operands() => {
                    if func.values[a.index()] != Type::I64 || imm_operand(func, &self.const_of, &inst.op, rules).is_some() {
                        continue;
                    }
                    let candidates: &[(Value, Value)] = if op == IntBinOp::Sub { &[(b, a)] } else { &[(b, a), (a, b)] };
                    for &(c, other) in candidates {
                        let c = rn(c.index());
                        if c >= nv || uses[c] != 1 || def_inst[c] == u32::MAX {
                            continue;
                        }
                        let dc = def_inst[c];
                        let VInst::Op { block: sb, idx: sidx } = self.insts[dc as usize] else { continue };
                        let Op::IntBin { op: IntBinOp::Shl, a: x, b: k } = func.blocks[sb as usize].insts[sidx as usize].op else { continue };
                        let Some((kt, kb)) = self.const_of[k.index()] else { continue };
                        let k = const_canon(kt, kb);
                        if !(1..64).contains(&k) || func.values[x.index()] != Type::I64 {
                            continue;
                        }
                        shifted.push_hot((ii as u32, rn(other.index()) as u32, rn(x.index()) as u32, k as u32, op as u32));
                        fuse_away.push_hot(dc);
                        break;
                    }
                }
                _ => {}
            }
        }
        for (ii, w) in self_ext {
            let a = self.inst_operands(ii)[0];
            self.set_operands(ii, &[a]);
            put(&mut self.icmp_self_ext, ii, w);
        }
        for (ii, other, x, k, _) in shifted {
            self.set_operands(ii, &[other, x]);
            put(&mut self.shifted, ii, (other, x, k));
        }
        for (ii, x, k) in sbfx {
            self.set_operands(ii, &[x]);
            put(&mut self.trunc_sbfx, ii, (x, k));
        }
        for d in fuse_away {
            self.fused[d as usize] = true;
            self.clear_operands(d);
            self.results[d as usize] = u32::MAX;
        }
        self.uses = uses;
        self.def_inst = def_inst;
    }

    /// Narrow moves that cost nothing in the register convention:
    ///
    /// * a `trunc` all of whose uses are the value operand of stores is a
    ///   copy of its source (a store reads the low bytes of the register;
    ///   the frontends size the store by the operand's own type), so the
    ///   `mov` + sign extension goes;
    /// * a `load` whose only use is a `zext` to I64 zero-extends itself
    ///   (`load_zext`; `ldr w` / `movzx`) and the `zext` is renamed to it.
    fn fuse_narrow_moves(&mut self, func: &Function) {
        let uses = self.uses_snapshot();
        let def_inst = self.defs_snapshot(true);
        let nv = func.values.len();
        let total = nv + self.num_slots as usize;
        refill(&mut self.load_zext, self.insts.len(), false);
        refill(&mut self.no_canon, self.insts.len(), false);
        // Uses of each value as a store's value operand.
        let mut store_uses = filled_in(self.mcx, 0u32, total);
        for ii in 0..self.insts.len() {
            if self.fused[ii] {
                continue;
            }
            let VInst::Op { block, idx } = self.insts[ii] else { continue };
            if let Op::Store { value, .. } = func.blocks[block as usize].insts[idx as usize].op {
                store_uses[self.rename[value.index()] as usize] += 1;
            }
        }
        for ii in 0..self.insts.len() {
            if self.fused[ii] {
                continue;
            }
            let VInst::Op { block, idx } = self.insts[ii] else { continue };
            let inst = &func.blocks[block as usize].insts[idx as usize];
            let Some(r) = inst.result else { continue };
            let r = r.index();
            match inst.op {
                Op::IntBin { op: IntBinOp::Add | IntBinOp::Sub | IntBinOp::Mul | IntBinOp::And | IntBinOp::Or | IntBinOp::Xor | IntBinOp::Shl, .. } => {
                    // A narrow result read only by stores: its low bytes are
                    // the same with or without the sign extension.
                    if matches!(func.values[r], Type::I8 | Type::I16 | Type::I32) && self.rename[r] as usize == r && uses[r] != 0 && store_uses[r] == uses[r] {
                        self.no_canon[ii] = true;
                    }
                }
                Op::Conv { op: ConvOp::Trunc, a, .. } => {
                    let a = self.rename[a.index()] as usize;
                    if self.rename[r] as usize != r || self.const_of[a].is_some() || uses[r] == 0 || store_uses[r] != uses[r] {
                        continue;
                    }
                    self.rename[r] = a as u32;
                    self.fusions |= 1 << 3;
                    self.fused[ii] = true;
                    self.clear_operands(ii as u32);
                    self.results[ii] = u32::MAX;
                }
                Op::Conv { op: ConvOp::Zext, to: Type::I64, a } => {
                    let a = self.rename[a.index()] as usize;
                    if a >= nv || self.rename[r] as usize != r || uses[a] != 1 || def_inst[a] == u32::MAX || !matches!(func.values[a], Type::I8 | Type::I16 | Type::I32) {
                        continue;
                    }
                    let li = def_inst[a];
                    let VInst::Op { block: lb, idx: lidx } = self.insts[li as usize] else { continue };
                    if !matches!(func.blocks[lb as usize].insts[lidx as usize].op, Op::Load { .. }) {
                        continue;
                    }
                    self.load_zext[li as usize] = true;
                    self.fusions |= 1 << 3;
                    self.rename[r] = a as u32;
                    self.fused[ii] = true;
                    self.clear_operands(ii as u32);
                    self.results[ii] = u32::MAX;
                }
                _ => {}
            }
        }
        // A value renamed earlier to a `trunc` or `zext` renamed here must
        // follow it: the map stays direct.
        for v in 0..total {
            let mut t = self.rename[v];
            while self.rename[t as usize] != t {
                t = self.rename[t as usize];
            }
            self.rename[v] = t;
        }
        self.uses = uses;
        self.def_inst = def_inst;
    }

    /// Store pairs (`ImmRules::store_pairs`, AArch64): two stores to
    /// consecutive cells of one base in one block, with only non-memory
    /// instructions between them (the second value defined before the
    /// first store, so the pair can issue at the first store's place):
    ///
    /// * two 8-byte (`I64` / `Ptr`) stores at `o` and `o + 8` (`o` a
    ///   multiple of 8 in the `stp` range) become one `stp`;
    /// * two zero stores of one integer width at `o` and `o + size` (`o`
    ///   a multiple of the doubled size) become one zero store of the
    ///   next width, repeatedly (four `strb wzr` -> `str wzr`).
    ///
    /// Aliasing: no memory instruction (load, store, call) sits between
    /// the two, so moving the second store to the first's place changes
    /// no observable order.
    fn fuse_stores(&mut self, func: &Function, rules: &dyn ImmRules) {
        if !rules.store_pairs() {
            return;
        }
        let rename = copied_in(&self.rename, self.mcx);
        let rn = |v: usize| rename.get(v).map_or(v, |r| *r as usize);
        let nv = func.values.len();
        let is_zero = |this: &Self, v: Value| -> bool { v.index() < nv && this.const_fused[v.index()] && this.const_of[v.index()].is_some_and(|(t, b)| !t.is_float() && const_canon(t, b) == 0) };
        let def_inst = self.defs_snapshot(false);
        for bi in 0..self.blocks.len() {
            let (begin, end) = (self.blocks[bi].inst_begin, self.blocks[bi].inst_end);
            // Effective (type, offset) of every live store of the block, as
            // widened so far.
            let mut changed = true;
            while changed {
                changed = false;
                let mut i = begin;
                while i < end {
                    if self.fused[i as usize] {
                        i += 1;
                        continue;
                    }
                    let VInst::Op { block, idx } = self.insts[i as usize] else {
                        i += 1;
                        continue;
                    };
                    let Op::Store { ptr, offset, value, unaligned: false } = func.blocks[block as usize].insts[idx as usize].op else {
                        i += 1;
                        continue;
                    };
                    if self.store_pair.contains_key(&i) || self.store_override.contains_key(&i) {
                        i += 1;
                        continue;
                    }
                    let ty = self.store_zero_wide.get(&i).copied().unwrap_or(func.values[value.index()]);
                    let size = ty.bytes() as i32;
                    let offset = offset as i64;
                    // The next store, past non-memory instructions only.
                    let mut j = i + 1;
                    let mut next = None;
                    while j < end {
                        if self.fused[j as usize] {
                            j += 1;
                            continue;
                        }
                        let VInst::Op { block: jb, idx: jidx } = self.insts[j as usize] else { break };
                        match func.blocks[jb as usize].insts[jidx as usize].op {
                            Op::Store { ptr: p2, offset: o2, value: v2, unaligned: false } => {
                                next = Some((p2, o2 as i64, v2));
                                break;
                            }
                            Op::Store { .. } | Op::Load { .. } | Op::LoadZext { .. } | Op::Call { .. } | Op::CallHost { .. } => break,
                            _ => j += 1,
                        }
                    }
                    let Some((p2, o2, v2)) = next else {
                        i += 1;
                        continue;
                    };
                    if rn(p2.index()) != rn(ptr.index()) || self.store_pair.contains_key(&j) || self.store_override.contains_key(&j) {
                        i += 1;
                        continue;
                    }
                    let ty2 = self.store_zero_wide.get(&j).copied().unwrap_or(func.values[v2.index()]);
                    if ty2 != ty || o2 != offset + size as i64 {
                        i += 1;
                        continue;
                    }
                    let z1 = is_zero(self, value);
                    let z2 = is_zero(self, v2);
                    let both_zero = z1 && z2 && ty.is_int();
                    let pairable = matches!(ty, Type::I64 | Type::Ptr) && offset % 8 == 0 && (-512..=504).contains(&offset);
                    // The second value must be available at the first store.
                    let v2r = rn(v2.index());
                    let v2_ready = z2 || v2r >= nv || def_inst[v2r] == u32::MAX || def_inst[v2r] < i;
                    if both_zero && size < 8 && offset % (2 * size as i64) == 0 {
                        let wide = match ty {
                            Type::I8 => Type::I16,
                            Type::I16 => Type::I32,
                            _ => Type::I64,
                        };
                        put(&mut self.store_zero_wide, i, wide);
                        self.store_zero_wide.remove(&j);
                        self.fused[j as usize] = true;
                        self.clear_operands(j);
                        changed = true;
                    } else if pairable && v2_ready {
                        put(&mut self.store_pair, i, rn(v2.index()) as u32);
                        self.fused[j as usize] = true;
                        self.clear_operands(j);
                        // The first store now reads the second value too.
                        let ops = [ptr.0, value.0, v2.0];
                        self.set_operands(i, &ops[..if z2 { 2 } else { 3 }]);
                        changed = true;
                    }
                    i += 1;
                }
            }
        }
        self.def_inst = def_inst;
    }

    /// Address folds (`ImmRules::address_folds`, AArch64):
    ///
    /// * a `shl x, k` (constant `k`) used only as `ptr_add` indexes with
    ///   power-of-two scales moves into their scales (`add d, b, x, lsl #s`
    ///   takes any shift); the shift is fused away;
    /// * a `ptr_add` used only by one integer load: with no offset on either
    ///   side and a scale of 1 or the load's size, the load becomes the
    ///   register-offset form (`ldr d, [base, index, lsl #s]`) and the
    ///   `ptr_add` is fused away; otherwise the `ptr_add`'s constant offset
    ///   moves into the load's immediate (`add d, b, i, lsl #s; ldr d, [d, #off]`).
    fn fuse_addresses(&mut self, func: &Function, rules: &dyn ImmRules) {
        if !rules.address_folds() {
            return;
        }
        let uses = self.uses_snapshot();
        let def_inst = self.defs_snapshot(true);
        let m = self.mcx;
        let nv = func.values.len();
        let rename = copied_in(&self.rename, m);
        let rn = |v: usize| rename.get(v).map_or(v, |r| *r as usize);
        // Instruction users per value (operands of unfused instructions),
        // as one CSR table: `users[users_begin[v]..users_begin[v + 1]]`.
        let total = nv + self.num_slots as usize;
        let mut users_begin = filled_in(m, 0u32, total + 1);
        for ii in 0..self.insts.len() {
            if self.fused[ii] {
                continue;
            }
            for &v in self.inst_operands(ii as u32) {
                users_begin[rn(v as usize) + 1] += 1;
            }
        }
        for v in 0..total {
            users_begin[v + 1] += users_begin[v];
        }
        let mut users = filled_in(m, 0u32, users_begin[total] as usize);
        {
            let mut fill = copied_in(&users_begin, m);
            for ii in 0..self.insts.len() {
                if self.fused[ii] {
                    continue;
                }
                for &v in self.inst_operands(ii as u32) {
                    let v = rn(v as usize);
                    users[fill[v] as usize] = ii as u32;
                    fill[v] += 1;
                }
            }
        }
        let op_of = |view: &FnView<'s>, ii: u32| -> Option<&Op> {
            match view.insts[ii as usize] {
                VInst::Op { block, idx } => Some(&func.blocks[block as usize].insts[idx as usize].op),
                _ => None,
            }
        };
        // (1) shifts into ptr_add scales.
        for ii in 0..self.insts.len() as u32 {
            if self.fused[ii as usize] {
                continue;
            }
            let Some(&Op::IntBin { op: IntBinOp::Shl, a: x, b: k }) = op_of(self, ii) else { continue };
            let s = self.results[ii as usize];
            if s == u32::MAX || func.values[s as usize] != Type::I64 || func.values[x.index()] != Type::I64 {
                continue;
            }
            let Some((kt, kb)) = self.const_of[k.index()] else { continue };
            let k = const_canon(kt, kb);
            if !(1..64).contains(&k) {
                continue;
            }
            let s = rn(s as usize);
            let us = &users[users_begin[s] as usize..users_begin[s + 1] as usize];
            if us.is_empty() || us.len() as u32 != uses[s] {
                continue;
            }
            let mut plan: PgVec<(u32, u64, i64)> = PgVec::new_in(m);
            let mut ok = true;
            for &u in us {
                let (index, scale, offset) = match self.ptr_add_override.get(&u) {
                    Some(&(i, sc, off)) => (i, sc, off),
                    None => match op_of(self, u) {
                        Some(&Op::PtrAdd { index, scale, offset, .. }) => (index.0, scale as u64, offset as i64),
                        _ => {
                            ok = false;
                            break;
                        }
                    },
                };
                if rn(index as usize) != s || !scale.is_power_of_two() || scale.trailing_zeros() as i64 + k >= 64 {
                    ok = false;
                    break;
                }
                // The base must not be the shift result either.
                if let Some(&Op::PtrAdd { base, .. }) = op_of(self, u) {
                    if rn(base.index()) == s {
                        ok = false;
                        break;
                    }
                }
                plan.push_hot((u, scale << k, offset));
            }
            if !ok {
                continue;
            }
            for (u, scale, offset) in plan {
                put(&mut self.ptr_add_override, u, (rn(x.index()) as u32, scale, offset));
                let base = self.inst_operands(u)[0];
                self.set_operands(u, &[base, rn(x.index()) as u32]);
            }
            self.fused[ii as usize] = true;
            self.clear_operands(ii);
            self.results[ii as usize] = u32::MAX;
        }
        // (2a) a ptr_add (no offset, scale 1 or the access size) used only
        // as the pointer of loads and stores at offset 0: every access takes
        // the register-offset form and the ptr_add goes.
        for pa in 0..self.insts.len() as u32 {
            if self.fused[pa as usize] {
                continue;
            }
            let (base, index, scale, off_p) = match self.ptr_add_override.get(&pa) {
                Some(&(i, sc, off)) => match op_of(self, pa) {
                    Some(&Op::PtrAdd { base, .. }) => (rn(base.index()) as u32, i, sc, off),
                    _ => continue,
                },
                None => match op_of(self, pa) {
                    Some(&Op::PtrAdd { base, index, scale, offset }) => (rn(base.index()) as u32, rn(index.index()) as u32, scale as u64, offset as i64),
                    _ => continue,
                },
            };
            let p = self.results[pa as usize];
            if p == u32::MAX || off_p != 0 || (index as usize) < nv && self.const_of[index as usize].is_some() {
                continue;
            }
            let p = rn(p as usize);
            let us = &users[users_begin[p] as usize..users_begin[p + 1] as usize];
            if us.is_empty() || us.len() as u32 != uses[p] {
                continue;
            }
            let mut ok = true;
            for &u in us {
                let size = match op_of(self, u) {
                    Some(&Op::Load { ty, ptr, offset, .. }) | Some(&Op::LoadZext { ty, ptr, offset, .. }) if rn(ptr.index()) == p && offset == 0 && self.load_override.get(&u).is_none() => ty.bytes(),
                    Some(&Op::Store { ptr, offset, value, unaligned: false }) if rn(ptr.index()) == p && offset == 0 && rn(value.index()) != p => {
                        self.store_zero_wide.get(&u).copied().unwrap_or(func.values[value.index()]).bytes()
                    }
                    _ => 0,
                };
                if size == 0 || !(scale == 1 || scale == size as u64) || size > 8 {
                    ok = false;
                    break;
                }
                if let Some(&Op::Load { ty, .. }) | Some(&Op::LoadZext { ty, .. }) = op_of(self, u) {
                    if ty.is_float() || ty.is_vector() {
                        ok = false;
                        break;
                    }
                }
                if let Some(&Op::Store { value, .. }) = op_of(self, u) {
                    if func.values[value.index()].is_float() || func.values[value.index()].is_vector() {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                continue;
            }
            for &u in us {
                match op_of(self, u) {
                    Some(&Op::Load { ty, .. }) | Some(&Op::LoadZext { ty, .. }) => {
                        let shift = if scale == 1 { 0 } else { ty.bytes().trailing_zeros() };
                        put(&mut self.load_override, u, (base, Some((index, shift)), 0));
                        self.set_operands(u, &[base, index]);
                    }
                    Some(&Op::Store { value, .. }) => {
                        let size = self.store_zero_wide.get(&u).copied().unwrap_or(func.values[value.index()]).bytes();
                        let shift = if scale == 1 { 0 } else { size.trailing_zeros() };
                        put(&mut self.store_override, u, (base, index, shift));
                        self.set_operands(u, &[base, index, value.0]);
                    }
                    _ => unreachable!(),
                }
            }
            self.fused[pa as usize] = true;
            self.clear_operands(pa);
            self.results[pa as usize] = u32::MAX;
            self.ptr_add_override.remove(&pa);
        }
        // (2) ptr_add into its only load.
        for ii in 0..self.insts.len() as u32 {
            if self.fused[ii as usize] {
                continue;
            }
            let (ptr, off_l, size, r) = match op_of(self, ii) {
                Some(&Op::Load { ty, ptr, offset, .. }) | Some(&Op::LoadZext { ty, ptr, offset, .. }) => (ptr, offset, ty.bytes(), self.results[ii as usize]),
                _ => continue,
            };
            if r == u32::MAX || !(func.values[r as usize].is_int() || func.values[r as usize] == Type::Ptr) {
                continue;
            }
            let p = rn(ptr.index());
            if p >= nv || uses[p] != 1 || def_inst[p] == u32::MAX {
                continue;
            }
            let pa = def_inst[p];
            let (base, index, scale, off_p) = match self.ptr_add_override.get(&pa) {
                Some(&(i, sc, off)) => match op_of(self, pa) {
                    Some(&Op::PtrAdd { base, .. }) => (rn(base.index()) as u32, i, sc, off),
                    _ => continue,
                },
                None => match op_of(self, pa) {
                    Some(&Op::PtrAdd { base, index, scale, offset }) => (rn(base.index()) as u32, rn(index.index()) as u32, scale as u64, offset as i64),
                    _ => continue,
                },
            };
            if (index as usize) < nv && self.const_of[index as usize].is_some() {
                // Constant index (materialized or not: the fold drops this
                // use): the whole address is `base + disp`; fold it into
                // the load's immediate when it fits, else leave the `add`.
                let (kt, kb) = self.const_of[index as usize].unwrap();
                let k = const_canon(kt, kb);
                let off = off_p + k.wrapping_mul(scale as i64) + off_l as i64;
                if off >= 0 && off as u64 % size as u64 == 0 && (off as u64 / size as u64) < 4096 && self.ptr_add_override.get(&pa).is_none() {
                    put(&mut self.load_override, ii, (base, None, off));
                    self.set_operands(ii, &[base]);
                    self.fused[pa as usize] = true;
                    self.clear_operands(pa);
                    self.results[pa as usize] = u32::MAX;
                }
                continue;
            }
            let off = off_p + off_l as i64;
            if off == 0 && (scale == 1 || scale == size as u64) {
                let shift = if scale == 1 { 0 } else { size.trailing_zeros() };
                put(&mut self.load_override, ii, (base, Some((index, shift)), 0));
                self.set_operands(ii, &[base, index]);
                self.fused[pa as usize] = true;
                self.clear_operands(pa);
                self.results[pa as usize] = u32::MAX;
                self.ptr_add_override.remove(&pa);
            } else if off_p != 0 && off >= 0 && off as u64 % size as u64 == 0 && (off as u64 / size as u64) < 4096 {
                put(&mut self.ptr_add_override, pa, (index, scale, 0));
                put(&mut self.load_override, ii, (p as u32, None, off));
            }
        }
        self.uses = uses;
        self.def_inst = def_inst;
    }

    /// Flag reuse (`ImmRules::flag_reuse`, AArch64): an `Icmp` producing a
    /// value whose operands are those of the flag-setting instruction before
    /// it in the block (a `umin`/`umax`/`smin`/`smax`, or another `Icmp`),
    /// with nothing between that writes the flags, keeps its `cset` alone
    /// (`icmp_flags`). What writes NZCV in the AArch64 frontend: compares,
    /// the min/max pair, `Select`, the overflow ops, calls; the allocator's
    /// own code (loads, stores, moves, constants) never does.
    fn fuse_flags(&mut self, func: &Function, rules: &dyn ImmRules) {
        refill(&mut self.icmp_flags, self.insts.len(), false);
        if !rules.flag_reuse() {
            return;
        }
        let rename = core::mem::replace(&mut self.rename, PgVec::new_in(self.mcx));
        let rn = |v: usize| rename.get(v).map_or(v, |r| *r as usize);
        for bi in 0..self.blocks.len() {
            let mut flags: Option<(usize, usize)> = None;
            for ii in self.blocks[bi].inst_begin..self.blocks[bi].inst_end {
                if self.fused[ii as usize] {
                    continue;
                }
                let VInst::Op { block, idx } = self.insts[ii as usize] else { continue };
                let op = &func.blocks[block as usize].insts[idx as usize].op;
                match *op {
                    Op::IntBin { op: IntBinOp::Smin | IntBinOp::Smax | IntBinOp::Umin | IntBinOp::Umax, a, b } => {
                        flags = Some((rn(a.index()), rn(b.index())));
                    }
                    Op::Icmp { a, b, .. } if func.values[a.index()] != Type::Bool && !func.values[a.index()].is_float() && self.icmp_self_ext.get(&ii).is_none() => {
                        let pair = (rn(a.index()), rn(b.index()));
                        if flags == Some(pair) {
                            self.icmp_flags[ii as usize] = true;
                            self.clear_operands(ii);
                        } else {
                            flags = Some(pair);
                        }
                    }
                    Op::Load { .. } | Op::LoadZext { .. } | Op::Store { .. } | Op::PtrAdd { .. } | Op::Conv { .. } | Op::Const { .. } | Op::StackAddr { .. } | Op::IntBin { .. } | Op::IntUn { .. } | Op::FloatBin { .. } | Op::FloatUn { .. } | Op::Prefetch { .. } | Op::Crc32c { .. } => {}
                    _ => flags = None,
                }
            }
        }
        self.rename = rename;
    }

    /// Order every block's successors for layout: likely edge (or the
    /// `then` edge of a `CondBr` by default) first, cold targets last.
    fn order_layout_succs(&mut self, func: &Function) {
        // D3698: the hints per block (a scan of each list per successor)
        let m = self.mcx;
        let (cold, likely) = hint_tables(func, m);
        let cold_at = |b: u32| cold.get(b as usize).copied().unwrap_or(false);
        let is_cold = |view: &FnView<'s>, vb: u32| -> bool {
            match view.blocks[vb as usize].orig {
                Some(o) => cold_at(o),
                None => cold_at(view.blocks[vb as usize].succs[0]),
            }
        };
        for bi in 0..self.blocks.len() {
            let succs = copied_in(&self.blocks[bi].succs, m);
            let mut ordered: PgVec<u32> = reserved_in(succs.len(), m);
            let orig = self.blocks[bi].orig;
            // Likely edge first.
            let likely_first = match orig.and_then(|o| likely.get(o as usize).copied().flatten()) {
                Some(true) => Some(0usize),
                Some(false) => Some(1usize),
                None => None,
            };
            if let Some(i) = likely_first {
                if i < succs.len() {
                    ordered.push_hot(succs[i]);
                }
            }
            for &s in &succs {
                if !ordered.contains(&s) && !is_cold(self, s) {
                    ordered.push_hot(s);
                }
            }
            for &s in &succs {
                if !ordered.contains(&s) {
                    ordered.push_hot(s);
                }
            }
            self.blocks[bi].layout_succs = ordered;
        }
    }

    fn clear_operands(&mut self, i: u32) {
        self.set_operands(i, &[]);
    }
    /// Replace instruction `i`'s operand list (appends to the arena; the old
    /// slice is left unreferenced).
    fn set_operands(&mut self, i: u32, ops: &[u32]) {
        for k in self.ops_begin[i as usize]..self.ops_end[i as usize] {
            self.direct_uses[self.operands[k as usize] as usize] -= 1;
        }
        for &v in ops {
            self.direct_uses[v as usize] += 1;
        }
        // The instruction's range moves to the arena end.
        let begin = self.operands.len() as u32;
        self.operands.extend_copy(ops);
        let end = self.operands.len() as u32;
        self.ops_begin[i as usize] = begin;
        self.ops_end[i as usize] = end;
    }

    /// Ask for fixed registers for the loop headers' parameters, the
    /// loop-invariant values read inside a loop, and the values read from
    /// the most blocks.
    fn pick_fixed(&mut self, func: &Function) {
        let m = self.mcx;
        let nv = func.values.len();
        let nb = func.blocks.len();
        let rn = |v: usize| self.rename.get(v).map_or(v, |r| *r as usize);
        let freq = copied_in(&self.block_freq, m);
        let bodies = core::mem::replace(&mut self.loops, PgVec::new_in(m));
        let mut in_loop = filled_in(m, false, nb);
        // D3698: per block its innermost loop (the smallest body holding it), from the same walk: the natural loops of
        // distinct headers nest or are disjoint, so a block's loops are the chain around its innermost one
        let sizes: PgVec<usize> = bodies.iter().map(|(_, body)| body.count()).collect_hot(m);
        let mut innermost: PgVec<u32> = filled_in(m, u32::MAX, nb);
        for (li, (_, body)) in bodies.iter().enumerate() {
            crate::stop_point(); // D3552: one walk of a body per loop
            for b in body.ones() {
                in_loop[b] = true;
                if innermost[b] == u32::MAX || sizes[li] < sizes[innermost[b] as usize] {
                    innermost[b] = li as u32;
                }
            }
        }
        // Per value: the number of distinct view blocks reading it (an
        // operand in the block, or an edge argument arriving into it), and
        // the expected frequency of its reads inside a loop (an edge
        // argument counts at the edge's source: an argument into a loop
        // header from outside the loop initializes the phi once per
        // entry; it is not read per iteration). Both are sets of (value,
        // block) pairs; walking the blocks in order with a per-value
        // marker of the last block counted makes each pair count once.
        // The arguments arriving into a block are gathered per target
        // first (CSR over the edges), so a block's operand reads and its
        // arriving arguments share one marker.
        let mut arriving_begin = filled_in(m, 0u32, nb + 1);
        for b in &func.blocks {
            for e in b.term.edges() {
                arriving_begin[e.target.index() + 1] += e.args.len() as u32;
            }
        }
        for i in 0..nb {
            arriving_begin[i + 1] += arriving_begin[i];
        }
        let mut arriving = filled_in(m, 0u32, arriving_begin[nb] as usize);
        {
            let mut fill = copied_in(&arriving_begin, m);
            for b in &func.blocks {
                for e in b.term.edges() {
                    let t = e.target.index();
                    for a in &e.args {
                        arriving[fill[t] as usize] = rn(a.index()) as u32;
                        fill[t] += 1;
                    }
                }
            }
        }
        // Per value: the loops (bits of `bodies`' index, the first 64)
        // with a block reading it.
        let mut used_loops = filled_in(m, 0u64, nv);
        let loops_of = |b: usize| -> u64 {
            let mut m = 0u64;
            for (li, (_, body)) in bodies.iter().enumerate().take(64) {
                if body[b] {
                    m |= 1 << li;
                }
            }
            m
        };
        let mut blocks_using = filled_in(m, 0u32, nv);
        let mut last_use = filled_in(m, 0u32, nv);
        let mut mark = filled_in(m, u32::MAX, nv);
        for (bi, b) in self.blocks.iter().enumerate() {
            let o = b.orig.unwrap_or(bi as u32);
            // A split block runs with its edge: the source's loops.
            let lm = b.orig.map_or(0, |o| loops_of(o as usize));
            for i in b.inst_begin..b.inst_end {
                for &v in self.inst_operands(i) {
                    if (v as usize) < nv {
                        let v = rn(v as usize);
                        last_use[v] = last_use[v].max(o);
                        used_loops[v] |= lm;
                        if mark[v] != bi as u32 {
                            mark[v] = bi as u32;
                            blocks_using[v] += 1;
                        }
                    }
                }
            }
            if let Some(orig) = b.orig {
                let o = orig as usize;
                for &v in &arriving[arriving_begin[o] as usize..arriving_begin[o + 1] as usize] {
                    last_use[v as usize] = last_use[v as usize].max(o as u32);
                    used_loops[v as usize] |= lm;
                    if mark[v as usize] != bi as u32 {
                        mark[v as usize] = bi as u32;
                        blocks_using[v as usize] += 1;
                    }
                }
            }
        }

        let mut hot = filled_in(m, 0.0f32, nv);
        let mut mark_hot = filled_in(m, u32::MAX, nv);
        for (bi, b) in self.blocks.iter().enumerate() {
            let Some(orig) = b.orig else { continue };
            let o = orig as usize;
            if !in_loop[o] {
                continue;
            }
            for i in b.inst_begin..b.inst_end {
                for &v in self.inst_operands(i) {
                    if (v as usize) < nv {
                        let v = rn(v as usize);
                        if mark_hot[v] != bi as u32 {
                            mark_hot[v] = bi as u32;
                            hot[v] += freq[o];
                        }
                    }
                }
            }
            for e in func.blocks[o].term.edges() {
                for a in &e.args {
                    let v = rn(a.index());
                    if mark_hot[v] != bi as u32 {
                        mark_hot[v] = bi as u32;
                        hot[v] += freq[o];
                    }
                }
            }
        }
        // Loop-header block parameters first: they are the loop-carried
        // state, and a phi not in a fixed register lives in a stack slot (a
        // store and a load per iteration). Coalescing at the anchor edge
        // pins them without the classic budget; the force is the fallback.
        let mut header_phi = filled_in(m, false, nv);
        for h in self.loop_headers(func) {
            for &p in &func.blocks[h].params {
                header_phi[p.index()] = true;
            }
        }
        // A phi of a block outside every loop (the exit join every loop
        // branches to, the return's parameter) is written once per loop
        // and read once after them; pinned at its first edge it would hold
        // a register through every later loop (the anchor is the first
        // compiled edge, the block comes after the loops in layout), so it
        // takes a slot instead.
        let mut slot_phi = filled_in(m, false, nv);
        if !bodies.is_empty() {
            // D3698: the headers per block (a scan of every loop per block)
            let mut heads_at = filled_in(m, false, nb);
            for (h, _) in &bodies {
                heads_at[*h] = true;
            }
            let is_header = |bi: usize| heads_at[bi];
            for (bi, b) in func.blocks.iter().enumerate() {
                if bi % 256 == 0 {
                    crate::stop_point(); // D3552: each block reads every loop
                }
                if bi == func.entry.index() || b.params.is_empty() || is_header(bi) {
                    continue;
                }
                // Likewise the join a loop's exit edge enters inside an
                // outer loop (the window loop's join after its row loops):
                // pinned at the first exit compiled, its phis would hold
                // registers through every loop laid out before the join.
                // D3698: a loop holds `p` and not `bi` exactly when `p`'s innermost loop does not hold `bi` (the loops
                // holding `p` are the chain around its innermost one; a scan of every loop per predecessor)
                let exit_join = self.preds[bi].iter().any(|&p| innermost[p as usize] != u32::MAX && !bodies[innermost[p as usize] as usize].1[bi]);
                if !in_loop[bi] || exit_join {
                    for &p in &b.params {
                        slot_phi[p.index()] = true;
                    }
                }
            }
            // A header phi no instruction reads — only passed along the edges
            // into an inner loop's header and back (the chunk loop's run
            // state around the per-bit loop) — is carried by the inner phis;
            // pinned it would take their register (nine pins: the chunk
            // loop's six left the per-bit loop's seven in the frame). A slot:
            // a store at the inner loop's exit, a load at its entry, per chunk.
            let mut inst_reads = filled_in(m, 0u32, nv);
            for b in &self.blocks {
                for i in b.inst_begin..b.inst_end {
                    for &v in self.inst_operands(i) {
                        if (v as usize) < nv {
                            inst_reads[rn(v as usize)] += 1;
                        }
                    }
                }
            }
            // D3698: the loops headed inside a body are found from its blocks (a scan of every loop per loop)
            let of_header = loops_by_header(&bodies, nb, m);
            for (h, body) in &bodies {
                crate::stop_point(); // D3552: each loop reads its body's headers
                let inner = body.ones().filter(|&h2| heads_at[h2]).any(|h2| {
                    of_header[h2].iter().any(|&l2| h2 != *h && !bodies[l2 as usize].1[*h] && !func.blocks[h2].params.is_empty())
                });
                if !inner {
                    continue;
                }
                for &p in &func.blocks[*h].params {
                    let v = p.index();
                    if rn(v) == v && inst_reads[v] == 0 {
                        slot_phi[v] = true;
                    }
                }
            }
        }
        // Then the loop-invariant values read inside a loop (the ctx
        // pointers loaded once per call: output arrays, capacity, scan
        // slots), by the expected frequency of their reads: a non-fixed
        // value is released at the loop header (a join) and reloaded from
        // its slot on every iteration. TPDE's own rule never fixes them
        // (`definitions_in_childs` reserves the budget for the loop's own
        // values), so they are nominated here, at most `INVARIANT_FIXED` of
        // them so the loop's values keep their share of the budget.
        let mut def_block = filled_in(m, u32::MAX, nv);
        for (ii, vi) in self.insts.iter().enumerate() {
            if let VInst::Op { block, .. } = *vi {
                let r = self.results[ii];
                if r != u32::MAX && (r as usize) < nv {
                    def_block[r as usize] = block;
                }
            }
        }
        let entry_param = |v: usize| func.blocks[func.entry.index()].params.iter().any(|p| p.index() == v);
        // D3698: the loop headers sorted, for `across` (a scan of every loop per value)
        let mut heads: PgVec<u32> = bodies.iter().map(|(h, _)| *h as u32).collect_hot(m);
        heads.sort_unstable();
        let mut invariant: PgVec<(u32, u32)> = PgVec::new_in(m);
        refill(&mut self.no_fixed, nv + self.num_slots as usize, false);
        for v in 0..nv {
            if func.values[v].is_float() || self.const_of[v].is_some() || rn(v) != v || header_phi[v] {
                continue;
            }
            let d = def_block[v];
            if d == u32::MAX && !entry_param(v) {
                continue;
            }
            // A value defined inside a loop is that loop's own business,
            // except the invariant of an inner loop: defined once per trip of
            // the outer loop (the window's plane pointers, bounds and record
            // faces, set up per window) and read at least ten times as often
            // inside the inner (row) loop. TPDE's own rule leaves it to a
            // slot there (`definitions_in_childs`), so it is reloaded on every
            // row: nominate it with the others, by the frequency of its reads
            // (V4R-153; a pgrc2 join-and-aggregate row loop, 22 such values per row loop).
            if d != u32::MAX && in_loop[d as usize] && hot[v] < 10.0 * freq[d as usize] {
                continue;
            }
            let hot = hot[v];
            if hot >= 1.0 {
                invariant.push_hot((hot.min(u32::MAX as f32 / 2.0) as u32, v as u32));
            }
            // A value live across a loop (defined before it, read inside
            // or after it) is nominated below or not fixed at all: TPDE's
            // own rule would otherwise hand the loop's registers to these
            // in definition order. A value whose range crosses no loop
            // keeps the rule (a fixed register is what carries it across
            // a join).
            let def = if d == u32::MAX { 0 } else { d };
            let across = used_loops[v] != 0 || heads.get(heads.partition_point(|&h| h <= def)).is_some_and(|&h| h <= last_use[v]);
            if !entry_param(v) && across {
                self.no_fixed[v] = true;
            }
        }
        for v in 0..nv {
            if slot_phi[v] && rn(v) == v {
                self.no_fixed[v] = true;
            }
        }
        // descending; the values are distinct, so the order is the stable sort's
        invariant.sort_unstable_by(|x, y| y.cmp(x));
        // Then the values read from many blocks. Everything else is left to
        // TPDE's own rule, which fixes a value live past its block while
        // the budget lasts: forcing a cold path's values would take the
        // registers from the hot path's.
        let mut cands: PgVec<(bool, usize, u32)> = (0..nv)
            .filter(|&v| (header_phi[v] || blocks_using[v] >= 4) && !func.values[v].is_float() && self.const_of[v].is_none())
            .map(|v| (header_phi[v], blocks_using[v] as usize, v as u32))
            .collect_hot(m);
        // descending; the values are distinct, so the order is the stable sort's
        cands.sort_unstable_by(|x, y| y.cmp(x));
        refill(&mut self.force_fixed, nv + self.num_slots as usize, false);
        // The row loop's invariants: the ctx and state pointers, the lane
        // pointer, the window's end, the record faces (nine in a one-key group-by), and the
        // inner loops' per-window invariants (V4R-153: fourteen; with nine
        // a pgrc2 join-and-aggregate row loop reloaded its plane pointers every row); the
        // frontend's `FIXED_ASSIGNMENTS` leaves a few for the loop's own
        // values that live past their block.
        const INVARIANT_FIXED: usize = 14;
        let mut forced = 0;
        for (_, v) in invariant.into_iter().take(INVARIANT_FIXED) {
            self.force_fixed[v as usize] = true;
            self.no_fixed[v as usize] = false;
            forced += 1;
        }
        for (phi, _, v) in cands {
            if self.force_fixed[v as usize] || slot_phi[v as usize] {
                continue;
            }
            if !phi {
                if forced >= 10 {
                    continue;
                }
                forced += 1;
            }
            self.force_fixed[v as usize] = true;
            self.no_fixed[v as usize] = false;
        }
        self.loops = bodies;
    }

    /// The natural loops: per header (`loop_headers`) the blocks of its
    /// body — a back edge is `p -> h` with `h` dominating `p`, and the body
    /// is `h` plus every block that reaches a back edge's source without
    /// passing `h`. Dominators by the iterative bit-set method over the
    /// original blocks (a few hundred at most; the transient tables are
    /// freed with the call). Block numbers say nothing here: the probes
    /// place a loop's entry blocks after the loop, and a range heuristic
    /// took those entries for back edges.
    fn loop_bodies(&mut self, func: &Function) {
        let m = self.mcx;
        let nb = func.blocks.len();
        self.preds.clear();
        resize_with_in(&mut self.preds, nb, || PgVec::new_in(m));
        for (bi, b) in func.blocks.iter().enumerate() {
            for (ei, e) in b.term.edges().enumerate() {
                if e.target.index() < nb && !self.term_const[bi].is_some_and(|t| t != ei) {
                    self.preds[e.target.index()].push_hot(bi as u32);
                }
            }
        }
        // D414: only blocks the entry reaches are predecessors. A block nothing branches to (the lowerer leaves such
        // blocks: 58-120 in the live count(DISTINCT), grouped-average and join row bodies) has the dominator set {itself}; intersected into
        // the live block it joins, it erased the loop header from every block past the join, the back edge vanished and
        // the whole row loop compiled as straight-line code run once (phis and invariants through stack slots, calls
        // counted cold). Unreachable code never runs, so dropping it from every predecessor list is exact.
        let entry = func.entry.index();
        let mut reach = filled_in(m, false, nb);
        let mut work = pg_vec![in m; entry];
        reach[entry] = true;
        while let Some(x) = work.pop() {
            for (ei, e) in func.blocks[x].term.edges().enumerate() {
                let t = e.target.index();
                if t < nb && !reach[t] && !self.term_const[x].is_some_and(|c| c != ei) {
                    reach[t] = true;
                    work.push_hot(t);
                }
            }
        }
        for ps in self.preds.iter_mut() {
            ps.retain(|p| reach[*p as usize]);
        }
        let preds = &self.preds;
        // D1939: the immediate dominators by Cooper, Harvey and Kennedy's iteration over the reverse postorder of the
        // reachable blocks, and `h` dominates `p` when `h` is on `p`'s idom chain. The bit-set fixpoint this replaces held
        // a set per block (nb x nb bits, nb^2 / 64 words per pass): a fifth of a function of 8,000 compares' compile.
        // Both answer the same relation; an unreachable block is dominated by itself alone, as before.
        let mut succs: PgVec<PgVec<u32>> = filled_in(m, PgVec::new_in(m), nb);
        for (b, ps) in preds.iter().enumerate() {
            for &p in ps {
                succs[p as usize].push_hot(b as u32);
            }
        }
        let mut rpo = filled_in(m, u32::MAX, nb);
        let mut order: PgVec<u32> = reserved_in(nb, m);
        {
            let mut seen = filled_in(m, false, nb);
            let mut st: PgVec<(u32, usize)> = pg_vec![in m; (entry as u32, 0)];
            seen[entry] = true;
            while let Some(&mut (b, ref mut i)) = st.last_mut() {
                if let Some(&s) = succs[b as usize].get(*i) {
                    *i += 1;
                    if !seen[s as usize] {
                        seen[s as usize] = true;
                        st.push_hot((s, 0));
                    }
                } else {
                    order.push_hot(b);
                    st.pop();
                }
            }
            order.reverse();
            for (n, &b) in order.iter().enumerate() {
                rpo[b as usize] = n as u32;
            }
        }
        let mut idom = filled_in(m, u32::MAX, nb);
        idom[entry] = entry as u32;
        let intersect = |idom: &[u32], mut a: u32, mut b: u32| {
            while a != b {
                while rpo[a as usize] > rpo[b as usize] {
                    a = idom[a as usize];
                }
                while rpo[b as usize] > rpo[a as usize] {
                    b = idom[b as usize];
                }
            }
            a
        };
        loop {
            let mut changed = false;
            for &b in order.iter().skip(1) {
                let mut new = u32::MAX;
                for &p in &preds[b as usize] {
                    if idom[p as usize] == u32::MAX {
                        continue;
                    }
                    new = if new == u32::MAX { p } else { intersect(&idom, p, new) };
                }
                if new != u32::MAX && idom[b as usize] != new {
                    idom[b as usize] = new;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        // D3698: `h` dominates `p` when `h` is on `p`'s idom chain: read from the dominator tree's preorder intervals
        // (`h` encloses `p`). A walk of the chain per test was the tree's depth: a chain of loops, each header testing its
        // entry edge's source, walked from every header to the function's entry.
        let mut kids: PgVec<PgVec<u32>> = filled_in(m, PgVec::new_in(m), nb);
        for &b in order.iter().skip(1) {
            if idom[b as usize] != u32::MAX {
                kids[idom[b as usize] as usize].push_hot(b);
            }
        }
        let mut pre = filled_in(m, u32::MAX, nb);
        let mut post = filled_in(m, 0u32, nb);
        {
            let mut clock = 0u32;
            let mut st: PgVec<(u32, usize)> = pg_vec![in m; (entry as u32, 0)];
            pre[entry] = clock;
            clock += 1;
            while let Some(&mut (b, ref mut i)) = st.last_mut() {
                if let Some(&k) = kids[b as usize].get(*i) {
                    *i += 1;
                    pre[k as usize] = clock;
                    clock += 1;
                    st.push_hot((k, 0));
                } else {
                    post[b as usize] = clock;
                    clock += 1;
                    st.pop();
                }
            }
        }
        let dominates = |h: usize, p: usize| {
            if idom[p] == u32::MAX {
                return h == p;
            }
            pre[h] != u32::MAX && pre[h] <= pre[p] && post[p] <= post[h]
        };
        let mut out = PgVec::new_in(m);
        let mut stack: PgVec<u32> = PgVec::new_in(m);
        let headers = self.loop_headers(func);
        // D3698: a body's blocks are marked with its loop's stamp in one table for every loop and gathered into a bit set
        // over their own range (`Body::of`), not a table of the function's size per header
        let mut mark = filled_in(m, 0u32, nb);
        let mut members: PgVec<u32> = PgVec::new_in(m);
        for (li, h) in headers.into_iter().enumerate() {
            crate::stop_point(); // D3552: one body per header
            let stamp = li as u32 + 1;
            members.clear();
            mark[h] = stamp;
            members.push_hot(h as u32);
            stack.clear();
            stack.extend_hot(preds[h].iter().copied().filter(|&p| dominates(h, p as usize)));
            while let Some(p) = stack.pop() {
                let p = p as usize;
                if mark[p] == stamp {
                    continue;
                }
                mark[p] = stamp;
                members.push_hot(p as u32);
                stack.extend_copy(&preds[p]);
            }
            out.push_hot((h, Body::of(nb, &members, m)));
        }
        self.loops = out;
    }

    /// Expected executions of each original block per call, from
    /// `hints.block_freq` (explicit), else the flow model below over the
    /// natural loops (`loop_bodies`).
    fn block_freqs(&self, func: &Function) -> PgVec<'s, f32> {
        let m = self.mcx;
        let nb = func.blocks.len();
        let bodies = &self.loops;
        // Blocks outside every loop run once per call. Inside a loop the
        // frequency flows from the header (100 per level: the header's
        // incoming from outside its range, times 100) along the forward
        // edges: an unconditional edge carries all of it, a conditional
        // branch halves it (the `likely` hint 9:1, an edge leaving the
        // loop's range 1:99 — the exit is taken once per loop), a switch
        // splits it evenly. A block deep in the loop's branch tree (the
        // run's end, the rare host call) so counts less than the row's
        // path, which is what `pick_fixed` and the weights need to prefer
        // the row's values without a profile. Block numbers inside a loop
        // need not be topological, so the flow is iterated to a fixpoint.
        let mut freq = filled_in(m, 1.0f32, nb);
        // Per block its innermost loop (the smallest body holding it), as
        // an index into `bodies`.
        let mut innermost: PgVec<Option<usize>> = filled_in(m, None, nb);
        // D3552: each body's size counted once (counted per block of each body, a function of thousands of loops took
        // seconds here between stop points)
        let sizes: PgVec<usize> = bodies.iter().map(|(_, body)| body.count()).collect_hot(m);
        for (li, (_, body)) in bodies.iter().enumerate() {
            crate::stop_point(); // D3552: a walk of a body per loop
            let size = sizes[li];
            for b in body.ones() {
                if innermost[b].is_none_or(|o| size < sizes[o]) {
                    innermost[b] = Some(li);
                }
            }
        }
        let in_range = |li: usize, b: usize| bodies[li].1[b];
        // D3698: the hints per block, from one walk of each list (a scan of both per conditional edge per round)
        let (cold, likely) = hint_tables(func, m);
        let is_cold = |b: usize| cold.get(b).copied().unwrap_or(false);
        let likely_of = |b: usize| likely.get(b).copied().flatten();
        // Edge weights per (source block, edge index).
        let weight = |bi: usize, ei: usize, target: usize| -> f32 {
            let b = &func.blocks[bi];
            let n = b.term.edges().count();
            match &b.term {
                Terminator::CondBr { .. } if self.term_const[bi].is_some() => f32::from(u8::from(self.term_const[bi] == Some(ei))),
                Terminator::CondBr { .. } => {
                    let leaves = |t: usize| innermost[bi].is_some_and(|h| !in_range(h, t)) || is_cold(t);
                    let other = b.term.edges().nth(1 - ei).map_or(target, |e| e.target.index());
                    match (leaves(target), leaves(other), likely_of(bi)) {
                        (true, false, _) => 0.01,
                        (false, true, _) => 0.99,
                        (_, _, Some(l)) => if (ei == 0) == l { 0.9 } else { 0.1 },
                        _ => 0.5,
                    }
                }
                _ => 1.0 / n.max(1) as f32,
            }
        };
        for _ in 0..8 {
            let mut changed = false;
            for b in 0..nb {
                let Some(li) = innermost[b] else { continue };
                let h = bodies[li].0;
                let mut sum = 0.0f32;
                for &pi in &self.preds[b] {
                    let pi = pi as usize;
                    for (ei, e) in func.blocks[pi].term.edges().enumerate() {
                        if e.target.index() != b {
                            continue;
                        }
                        // The header counts its entries (the back edges are
                        // the loop's own), a body block its in-range
                        // predecessors (a nested loop's exit carries 1/100
                        // of its scaled frequency: the outer's again).
                        let counts = if b == h { !in_range(li, pi) } else { in_range(li, pi) };
                        if counts {
                            sum += freq[pi] * weight(pi, ei, b);
                        }
                    }
                }
                let f = if b == h { sum * 100.0 } else { sum };
                let f = f.max(0.001);
                if (f - freq[b]).abs() > freq[b] * 1e-3 {
                    freq[b] = f;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for (b, f) in &func.hints.block_freq {
            if b.index() < nb {
                freq[b.index()] = *f;
            }
        }
        for b in &func.hints.cold_blocks {
            if b.index() < nb {
                freq[b.index()] = freq[b.index()].min(0.001);
            }
        }
        freq
    }

    /// Loop headers: the lowerer's `hints.loop_headers` when it gave any,
    /// else every block with an edge from a block created after it (the
    /// text-form corpus carries no hints; a lowerer that creates a join
    /// before its arms would make this heuristic take the join for a
    /// header, hence the hints).
    fn loop_headers(&self, func: &Function) -> PgVec<'s, usize> {
        let m = self.mcx;
        let nb = func.blocks.len();
        if !func.hints.loop_headers.is_empty() {
            return func.hints.loop_headers.iter().map(|h| h.index()).filter(|h| *h < nb).collect_hot(m);
        }
        let mut heads = filled_in(m, false, nb);
        for (bi, b) in func.blocks.iter().enumerate() {
            for e in b.term.edges() {
                if e.target.index() <= bi {
                    heads[e.target.index()] = true;
                }
            }
        }
        (0..nb).filter(|b| heads[*b]).collect_hot(m)
    }

    fn compute_weights(&mut self, func: &Function) {
        let m = self.mcx;
        let nv = func.values.len();
        let freq = copied_in(&self.block_freq, m);
        let mut w = filled_in(m, 0.0f32, nv);
        let rn = |v: usize| self.rename.get(v).map_or(v, |r| *r as usize);
        let mut ops: PgVec<u32> = reserved_in(8, m);
        for (bi, b) in func.blocks.iter().enumerate() {
            for &p in &b.params {
                w[rn(p.index())] += freq[bi];
            }
            for inst in &b.insts {
                if let Some(r) = inst.result {
                    w[r.index()] += freq[bi];
                }
                op_operands_into(&inst.op, &mut ops);
                for &v in &ops {
                    if (v as usize) < nv {
                        w[rn(v as usize)] += freq[bi];
                    }
                }
            }
            for (ei, e) in b.term.edges().enumerate() {
                if self.term_const[bi].is_some_and(|t| t != ei) {
                    continue;
                }
                for a in &e.args {
                    w[rn(a.index())] += freq[bi];
                }
            }
            match &b.term {
                Terminator::CondBr { .. } if self.term_const[bi].is_some() => {}
                Terminator::CondBr { cond, .. } => w[rn(cond.index())] += freq[bi],
                Terminator::Switch { value, .. } => w[rn(value.index())] += freq[bi],
                Terminator::Return(vs) => {
                    for v in vs {
                        w[rn(v.index())] += freq[bi];
                    }
                }
                _ => {}
            }
        }
        for v in &func.hints.remat {
            if v.index() < nv {
                w[v.index()] /= 4.0;
            }
        }
        self.weight.clear();
        self.weight.extend_hot(
            w.iter()
                .map(|x| ((x * 64.0).max(1.0)).min(u32::MAX as f32 / 2.0) as u32)
                .chain(core::iter::repeat(1).take(self.num_slots as usize)),
        );
    }

    #[inline(always)]
    fn push_inst(&mut self, inst: VInst, ops: &[u32], result: Option<u32>) {
        self.ops_begin.push_hot(self.operands.len() as u32);
        self.operands.extend_copy(ops);
        self.ops_end.push_hot(self.operands.len() as u32);
        self.insts.push_hot(inst);
        self.results.push_hot(result.unwrap_or(u32::MAX));
    }

    fn bind_args(&mut self, pred: u32, e: &Edge) {
        let t = e.target.index();
        // `check_edge_arity` rejects a mismatch before any view is built.
        debug_assert_eq!(self.blocks[t].phis.len(), e.args.len(), "edge to b{} of a function `compile` did not check", e.target.0);
        for (k, a) in e.args.iter().enumerate() {
            let p = self.blocks[t].phis[k];
            self.phis[p.index()].as_mut().unwrap().incoming.push_hot((pred, *a));
        }
    }

    fn add_split(&mut self, pred: u32, ei: usize, e: &Edge) -> u32 {
        let sb = self.blocks.len() as u32;
        let si = self.splits.len() as u32;
        let m = self.mcx;
        self.splits.push_hot(Split { target: e.target.0, from: pred, edge: ei as u32 });
        let begin = self.insts.len() as u32;
        self.push_inst(VInst::SplitBr { split: si }, &[], None);
        self.blocks.push_hot(VBlock {
            orig: None,
            phis: PgVec::new_in(m),
            inst_begin: begin,
            inst_end: begin + 1,
            succs: pg_vec![in m; e.target.0],
            layout_succs: pg_vec![in m; e.target.0],
            info: Cell::new(0),
            info2: Cell::new(0),
        });
        self.bind_args(sb, e);
        sb
    }

    #[inline]
    pub fn inst_operands(&self, i: u32) -> &[u32] {
        &self.operands[self.ops_begin[i as usize] as usize..self.ops_end[i as usize] as usize]
    }

    pub fn value_type(&self, func: &Function, v: u32) -> Type {
        if v < self.num_values {
            func.values[v as usize]
        } else {
            Type::Ptr
        }
    }
}

/// Canonical (sign-extended) value of a constant.
/// Per block, the edge a `CondBr` always takes: its condition a `Const`, or
/// an `Icmp` of this block over two `Const`s read by nothing else (the
/// lowerer's raise test on a lane whose error word is the constant zero:
/// `icmp ne 0 0` → the raise never taken). The compare and the other edge
/// leave the view: no compare, no branch, the dead block never laid out.
fn const_terms<'s>(func: &Function, m: Mcx<'s>) -> PgVec<'s, Option<usize>> {
    let nv = func.values.len();
    let mut consts: PgVec<Option<(Type, u64)>> = filled_in(m, None, nv);
    let mut uses = filled_in(m, 0u32, nv);
    let mut ops: PgVec<u32> = reserved_in(4, m);
    for b in &func.blocks {
        for i in &b.insts {
            if let (Some(r), Op::Const { ty, bits }) = (i.result, &i.op) {
                consts[r.index()] = Some((*ty, *bits));
            }
            op_operands_into(&i.op, &mut ops);
            for &v in &ops {
                uses[v as usize] += 1;
            }
        }
        for e in b.term.edges() {
            for a in &e.args {
                uses[a.index()] += 1;
            }
        }
        match &b.term {
            Terminator::CondBr { cond, .. } => uses[cond.index()] += 1,
            Terminator::Switch { value, .. } => uses[value.index()] += 1,
            Terminator::Return(vs) => vs.iter().for_each(|v| uses[v.index()] += 1),
            _ => {}
        }
    }
    let taken = |t: bool| Some(usize::from(!t));
    func.blocks
        .iter()
        .map(|b| {
            let Terminator::CondBr { cond, .. } = &b.term else { return None };
            if let Some((_, bits)) = consts[cond.index()] {
                return taken(bits & 1 != 0);
            }
            if uses[cond.index()] != 1 {
                return None;
            }
            let Op::Icmp { cond: c, a, b: rhs } = &b.insts.iter().rev().find(|i| i.result == Some(*cond))?.op else { return None };
            let (ta, x) = consts[a.index()]?;
            let (tb, y) = consts[rhs.index()]?;
            let (x, y) = (const_canon(ta, x), const_canon(tb, y));
            let (ux, uy) = (x as u64, y as u64);
            taken(match c {
                IntCond::Eq => x == y,
                IntCond::Ne => x != y,
                IntCond::Slt => x < y,
                IntCond::Sle => x <= y,
                IntCond::Sgt => x > y,
                IntCond::Sge => x >= y,
                IntCond::Ult => ux < uy,
                IntCond::Ule => ux <= uy,
                IntCond::Ugt => ux > uy,
                IntCond::Uge => ux >= uy,
            })
        })
        .collect_hot(m)
}

pub fn const_canon(ty: Type, bits: u64) -> i64 {
    match ty {
        Type::Bool => (bits & 1) as i64,
        Type::I8 => bits as u8 as i8 as i64,
        Type::I16 => bits as u16 as i16 as i64,
        Type::I32 => bits as u32 as i32 as i64,
        _ => bits as i64,
    }
}

/// Which constants a target folds into instruction immediates. The view
/// decides with these which `Const`s are never materialized, and the
/// frontend folds exactly the same ones (both go through `imm_operand`),
/// so a constant is never both skipped and needed. Each frontend supplies
/// its rules (`frontend::A64_IMM`, `frontend_x64::X64_IMM`).
pub trait ImmRules {
    /// `k` folds into an add/sub immediate (64-bit operand).
    fn add_imm(&self, k: i64) -> bool;
    /// `k` folds into an and/or/xor immediate (64-bit operand).
    fn logical_imm(&self, k: i64) -> bool;
    /// `k` folds into the compare immediate of an `Icmp` producing a value.
    fn cmp_imm(&self, k: i64) -> bool;
    /// `k` folds into the compare immediate of a compare fused into a
    /// `CondBr`.
    fn branch_cmp_imm(&self, k: i64) -> bool;
    /// A zero `Select` operand folds (the target has a zero register).
    fn select_zero(&self) -> bool;
    /// Narrow integers are kept sign-extended to 64 bits in registers, so
    /// a `sext` to a wider integer is a copy and renames to its operand
    /// (`propagate_copies`).
    fn sext_copy(&self) -> bool {
        false
    }
    /// The target compares a register with its own sign extension from a
    /// narrower width in one instruction (`cmp x, w, sxtw`), so
    /// `icmp eq/ne a, trunc(a)` producing a value folds (`fuse_narrow_ops`).
    fn self_ext_cmp(&self) -> bool {
        false
    }
    /// The target's logical and add/sub operations take a shifted
    /// register operand (`orr d, a, x, lsl #k`), so a `shl` by a constant
    /// used once folds into its user (`fuse_narrow_ops`).
    fn shifted_operands(&self) -> bool {
        false
    }
    /// The target has scaled register-offset loads and any shift on an
    /// `add`'s register operand (`fuse_addresses`).
    fn address_folds(&self) -> bool {
        false
    }
    /// The target has a conditional compare (`ccmp`): a branch on the
    /// `iand`/`ior` of two compares (or of a compare and a `Bool`) is one
    /// compare, one conditional compare and one branch (`fuse_compares`).
    fn ccmp(&self) -> bool {
        false
    }
    /// `k` folds into the conditional compare's immediate (`imm5`).
    fn ccmp_imm(&self, k: i64) -> bool {
        let _ = k;
        false
    }
    /// The target stores two registers to consecutive cells in one
    /// instruction (`stp`) and has a zero register (`fuse_stores`).
    fn store_pairs(&self) -> bool {
        false
    }
    /// A compare after a flag-setting instruction on the same operands
    /// reads the flags (`fuse_flags`).
    fn flag_reuse(&self) -> bool {
        false
    }
    /// The target extracts a signed bitfield in one instruction (`sbfx`):
    /// `trunc (ushr x k)` is one op (`fuse_narrow_ops`).
    fn bitfield_extract(&self) -> bool {
        false
    }
    /// A constant passed to a phi is materialized at the edge
    /// (`fuse_immediates` counts such uses as folded).
    fn phi_const(&self) -> bool {
        false
    }
}

/// Rules that fold nothing (targets without a frontend).
pub struct NoImm;
impl ImmRules for NoImm {
    fn add_imm(&self, _: i64) -> bool {
        false
    }
    fn logical_imm(&self, _: i64) -> bool {
        false
    }
    fn cmp_imm(&self, _: i64) -> bool {
        false
    }
    fn branch_cmp_imm(&self, _: i64) -> bool {
        false
    }
    fn select_zero(&self) -> bool {
        false
    }
}

/// The operand of `op` that the frontend folds into an immediate, if any.
/// The frontend must apply exactly the same rule.
pub fn imm_operand(func: &Function, consts: &[Option<(Type, u64)>], op: &Op, rules: &dyn ImmRules) -> Option<Value> {
    let c = |v: Value| consts.get(v.index()).copied().flatten().map(|(t, b)| const_canon(t, b));
    match *op {
        Op::IntBin { op, a, b } => {
            let ty = func.values[a.index()];
            if ty == Type::Bool {
                return None;
            }
            let fold = |k: i64, v: Value| match op {
                IntBinOp::Add | IntBinOp::Sub => rules.add_imm(k).then_some(v),
                IntBinOp::And | IntBinOp::Or | IntBinOp::Xor => rules.logical_imm(k).then_some(v),
                IntBinOp::Shl | IntBinOp::Ushr | IntBinOp::Sshr | IntBinOp::Rotl => (k >= 0 && k < ty.bytes() as i64 * 8).then_some(v),
                _ => None,
            };
            if let Some(k) = c(b) {
                return fold(k, b);
            }
            // A constant left operand of a commutative op folds the same
            // way (`k + x`: the lowerer's running offsets start from a
            // constant); the frontend takes the other operand as the
            // register source.
            if matches!(op, IntBinOp::Add | IntBinOp::And | IntBinOp::Or | IntBinOp::Xor) {
                if let Some(k) = c(a) {
                    return fold(k, a);
                }
            }
            None
        }
        Op::PtrAdd { index, scale, offset, .. } => {
            // A constant index is one immediate: `base + k * scale +
            // offset` (a zero total is a copy). `fuse_addresses` folds the
            // same sum into a load that is the pointer's only use.
            let k = c(index)?;
            let total = k.checked_mul(scale as i64)?.checked_add(offset as i64)?;
            (total == 0 || rules.add_imm(total)).then_some(index)
        }
        Op::Icmp { a, b, .. } => {
            let _ = a;
            let k = c(b)?;
            rules.cmp_imm(k).then_some(b)
        }
        Op::Store { value, .. } => {
            // A stored integer zero comes from the zero register.
            (rules.select_zero() && c(value) == Some(0) && !func.values[value.index()].is_float()).then_some(value)
        }
        Op::Select { cond, a, b } => {
            let _ = cond;
            if !rules.select_zero() {
                return None;
            }
            // Only a zero operand folds (the zero register); prefer `a`, else `b`.
            if c(a) == Some(0) && !func.values[a.index()].is_float() {
                Some(a)
            } else if c(b) == Some(0) && !func.values[b.index()].is_float() {
                Some(b)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The operand values of an instruction, in a fixed order, into a reused buffer (cleared first): no allocation per
/// instruction on the view's passes.
#[inline]
pub fn op_operands_into(op: &Op, out: &mut PgVec<'_, u32>) {
    out.clear();
    match op {
        Op::Const { .. } | Op::StackAddr { .. } => {}
        Op::IntBin { a, b, .. } | Op::IntOvf { a, b, .. } | Op::Icmp { a, b, .. } | Op::FloatBin { a, b, .. } | Op::Fcmp { a, b, .. } => {
            out.extend_copy(&[a.0, b.0]);
        }
        Op::IntUn { a, .. } | Op::FloatUn { a, .. } | Op::Conv { a, .. } => out.push_hot(a.0),
        Op::Select { cond, a, b } => out.extend_copy(&[cond.0, a.0, b.0]),
        Op::Load { ptr, .. } | Op::LoadZext { ptr, .. } => out.push_hot(ptr.0),
        Op::Store { ptr, value, .. } => out.extend_copy(&[ptr.0, value.0]),
        Op::PtrAdd { base, index, .. } => out.extend_copy(&[base.0, index.0]),
        Op::Prefetch { ptr, .. } => out.push_hot(ptr.0),
        Op::Crc32c { acc, data } => out.extend_copy(&[acc.0, data.0]),
        Op::AddHi { alo, blo, ahi, bhi } => out.extend_copy(&[alo.0, blo.0, ahi.0, bhi.0]),
        Op::Call { args, .. } | Op::CallHost { args, .. } => out.extend_hot(args.iter().map(|v| v.0)),
        // Vectors (simd.md).
        Op::VecLoad { ptr, .. } => out.push_hot(ptr.0),
        Op::VecStore { ptr, value, .. } => out.extend_copy(&[ptr.0, value.0]),
        Op::VecBin { a, b, .. } | Op::VecCmp { a, b, .. } | Op::VecNarrow { a, b } => out.extend_copy(&[a.0, b.0]),
        Op::VecShift { a, .. } | Op::VecReduceAdd { a } | Op::VecMovmask { a } | Op::VecSplat { a, .. } | Op::VecExtract { a, .. } | Op::VecWiden { a, .. } => out.push_hot(a.0),
        Op::VecSelect { mask, a, b } => out.extend_copy(&[mask.0, a.0, b.0]),
        Op::VecInsert { a, value, .. } => out.extend_copy(&[a.0, value.0]),
    }
}

#[allow(dead_code)]
pub fn block_of(b: Block) -> u32 {
    b.0
}

/// The end of a view pass (`FnView::build_in`'s marks): a stop point (D2909: a pending interrupt stops the compile between
/// passes), then the clock.
#[inline]
fn pass_end() -> u64 {
    crate::stop_point();
    crate::now_ns()
}

/// D3698: the cold-block and likely-edge hints per original block, from one walk of each list; a block's first `likely`
/// entry wins, as a `find` over the list does (function text may carry duplicates; the builder replaces).
fn hint_tables<'s>(func: &Function, m: Mcx<'s>) -> (PgVec<'s, bool>, PgVec<'s, Option<bool>>) {
    let nb = func.blocks.len();
    let mut cold = filled_in(m, false, nb);
    for c in &func.hints.cold_blocks {
        if let Some(x) = cold.get_mut(c.index()) {
            *x = true;
        }
    }
    let mut likely = filled_in(m, None, nb);
    for (b, t) in &func.hints.likely {
        if let Some(x) = likely.get_mut(b.index()).filter(|x| x.is_none()) {
            *x = Some(*t);
        }
    }
    (cold, likely)
}

/// D3698: per original block, the indexes into `loops` of the loops it heads, in `loops` order.
fn loops_by_header<'s>(loops: &[(usize, Body<'_>)], nb: usize, m: Mcx<'s>) -> PgVec<'s, PgVec<'s, u32>> {
    let mut of_header: PgVec<PgVec<u32>> = filled_in(m, PgVec::new_in(m), nb);
    for (li, (h, _)) in loops.iter().enumerate() {
        if let Some(l) = of_header.get_mut(*h) {
            l.push_hot(li as u32);
        }
    }
    of_header
}
