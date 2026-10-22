//! D1292: a full Sort over a key of a collatable type with no collation (decompose `effects::sort_raises`: a UNION ALL
//! column over conflicting implicit collations, ordered). PG's first ExecSort call begins the tuplesort, whose varlena.c
//! `varstr_sortsupport` raises 42P22 before the Sort reads a row; the producer's open (`RunEnd::open`, run before its
//! source opens) raises it through `m6_raise`. Served only where that open is the program's first act (the effects
//! home's `raising`): otherwise refused by decompose's D1292 name. W8-EQUIVCLASS-SORTOP's root form (D2206) the same
//! way: a key whose btree family has no order proc raises XX000 there (tuplesort_begin_heap, `m11_raise_no_proc`).
use gravity_decompose::ops::shared::sort::SortDesc;
use gravity_decompose::ops::sort;
use gravity_decompose::{Op, Source};
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, IntCond, Type, Value};
use gravity_kernels::host_abi::m11_raise_no_proc;
use gravity_kernels::host_abi::v4::m6_raise as h6;
use gravity_program::{ArenaRef, Pipeline, SiteDesc};
use mcx::PgBox;

use crate::face::end::{EndMech, SinkKind};
use crate::face::source::{DecodeOrder, GranuleIdent, Locator, Packed, ReadPlan, SourceMech, VerdictKind};
use crate::frame::env::Env;
use crate::frame::frame::{Mask, Win, WordWin};
use crate::frame::open::OpenWords;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::graph::Gx;
use crate::effects::visible::sort::{first_act, raising};

/// Pipeline `k` and its node, once `raising` has judged its Sort (runs/sort.rs `make_end`, before any form's dispatch).
pub(crate) fn checked<'g, 'p>(gx: &Gx<'g, 'p>, k: usize) -> (&'g gravity_decompose::PipelineNode<'p>, i32) {
    let p = &gx.pipelines()[k];
    if let Some(Op::Sort(d)) = p.ops.iter().find(|o| matches!(o, Op::Sort(_))) {
        raising(gx, k, d);
    }
    (p, p.node_id)
}

/// What a Sort raises at its first call: D1292's 42P22, or XX000 for a key with no order proc (its type and family).
#[derive(Clone, Copy)]
enum What {
    Coll,
    NoProc(u32, u32),
}

/// What pipeline `k`'s Sort `d` raises at its start, served (`raising`; sort_noproc.rs `checked` served the root form).
fn what(gx: &Gx<'_, '_>, k: usize, d: &SortDesc<'_>) -> Option<What> {
    if raising(gx, k, d) {
        return Some(What::Coll);
    }
    let (t, f) = sort::no_proc(d)?;
    first_act(gx, k).then_some(What::NoProc(t, f))
}

/// Pipeline `k`'s producer `end`, whose open raises first when its Sort raises.
pub(crate) fn wrap<'g>(gx: &Gx<'g, '_>, k: usize, end: PgBox<'g, dyn EndMech + 'g>) -> PgBox<'g, dyn EndMech + 'g> {
    match gx.pipelines()[k].ops.iter().find_map(|o| if let Op::Sort(d) = o { what(gx, k, d) } else { None }) {
        Some(w) => mcx::pg_box_dyn!(gx.mcx, Raise(end, w) => dyn EndMech + 'g),
        None => end,
    }
}

/// Pipeline `k`'s `end` (frame/mechs.rs `make_end`, every end), whose open raises first when it feeds the breaker a
/// raising Sort's pipeline reads (`raising`: the feeders run before that pipeline).
pub(crate) fn wrap_feeder<'g>(gx: &Gx<'g, '_>, k: usize, end: PgBox<'g, dyn EndMech + 'g>) -> PgBox<'g, dyn EndMech + 'g> {
    match sort_over_feeder(gx, k).and_then(|(j, d)| what(gx, j, d)) {
        Some(w) => mcx::pg_box_dyn!(gx.mcx, Raise(end, w) => dyn EndMech + 'g),
        None => end,
    }
}

/// decompose `sort_over_feeder` over the graph's pipelines; D3585: in the graph's lowering, from the readers kept once
/// (memo.rs), not a walk of every pipeline for each feeder (quadratic in an Append's arms).
fn sort_over_feeder<'g, 'p>(gx: &Gx<'g, 'p>, k: usize) -> Option<(usize, &'g SortDesc<'p>)> {
    let ps: &'g [gravity_decompose::PipelineNode<'p>] = &gx.graph.pipelines;
    let key = crate::memo::key_of(ps);
    let Some(kept) = crate::memo::read(key, |m| m.sort_readers.clone()) else { return sort::sort_over_feeder(ps, k) };
    let readers = kept.unwrap_or_else(|| {
        let sorts = |p: &gravity_decompose::PipelineNode<'_>| p.ops.iter().any(|o| matches!(o, Op::Sort(_)));
        let mut r: Vec<(u8, u32)> = ps.iter().enumerate().filter_map(|(j, p)| match p.source { Source::Breaker { slot } if sorts(p) => Some((slot, j as u32)), _ => None }).collect(); // alloc: Program
        r.sort_unstable();
        let r: std::rc::Rc<[(u8, u32)]> = r.into(); // alloc: Program
        crate::memo::write(key, |m| m.sort_readers = Some(r.clone()));
        r
    });
    let x = crate::frame::iface::end_breaker_of(gx, k)?;
    let at = readers.partition_point(|r| r.0 < x.slot);
    let j = readers[at..].iter().take_while(|r| r.0 == x.slot).map(|r| r.1 as usize).find(|&j| j != k)?;
    ps[j].ops.iter().find_map(|o| if let Op::Sort(d) = o { Some((j, d)) } else { None })
}

/// The producer whose open raises first; every other call is the producer's.
struct Raise<'g>(PgBox<'g, dyn EndMech + 'g>, What);

impl EndMech for Raise<'_> {
    fn state_bytes(&self) -> u32 { self.0.state_bytes() }
    fn open_words(&self, w: &mut OpenWords) { self.0.open_words(w) }
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        raise(b, env, self.1);
        self.0.open(b, env)
    }
    fn carried<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> mcx::PgVec<'e, Value> { self.0.carried(b, env) }
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, x: &Exits) -> Carry<'e> { self.0.lower(b, env, row, x) }
    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, c: &Carry) { self.0.window_end(b, env, c) }
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) { self.0.finish(b, env, x) }
    fn sink(&self) -> SinkKind { self.0.sink() }
    fn wants_locator(&self) -> bool { self.0.wants_locator() }
    fn whole(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, win: &Win, c: &Carry, l_wend: Block) -> bool { self.0.whole(b, env, win, c, l_wend) }
    fn block<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, w: &WordWin, base: Value, m: Mask, c: &Carry) -> Option<Carry<'e>> { self.0.block(b, env, w, base, m, c) }
    fn block_or_walk<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, w: &WordWin, base: Value, m: Mask, c: &Carry) -> Option<(Carry<'e>, Block)> { self.0.block_or_walk(b, env, w, base, m, c) }
    fn counts(&self, env: &Env) -> bool { self.0.counts(env) }
    fn takes_mult(&self) -> bool { self.0.takes_mult() }
    fn takes_set(&self) -> bool { self.0.takes_set() }
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize, d: gravity_program::Pipeline) -> gravity_program::Pipeline { self.0.pipeline_desc(px, k, d) }
}

/// Reader `k`'s source `src` (runs/sort.rs `make_emit`), whose open raises first when its producers' Sort raises: a
/// producer over an empty relation may never be called (no granule claims it), and PG raises at the begin all the same.
/// When a producer ran, its open raised first and the reader never opens.
pub(crate) fn wrap_emit<'g>(gx: &Gx<'g, '_>, k: usize, src: PgBox<'g, dyn SourceMech + 'g>) -> PgBox<'g, dyn SourceMech + 'g> {
    let Source::Breaker { slot } = gx.pipelines()[k].source else { return src };
    let producer = gx.life_of(k, slot).map(|l| l.0 as usize);
    match producer.and_then(|j| gx.pipelines()[j].ops.iter().find_map(|o| if let Op::Sort(d) = o { what(gx, j, d) } else { None })) {
        Some(w) => mcx::pg_box_dyn!(gx.mcx, RaiseEmit(src, w) => dyn SourceMech + 'g),
        None => src,
    }
}

/// The reader whose open raises first; every other call is the reader's.
struct RaiseEmit<'g>(PgBox<'g, dyn SourceMech + 'g>, What);

impl SourceMech for RaiseEmit<'_> {
    fn declare(&mut self, pb: &mut ProgramBuilder) { self.0.declare(pb) }
    fn site_desc(&self, arena: ArenaRef) -> Option<SiteDesc> { self.0.site_desc(arena) }
    fn seal(&mut self, px: &mut Px<'_, '_>, k: usize, site: &mut SiteDesc) { self.0.seal(px, k, site) }
    fn in_order(&mut self) { self.0.in_order() }
    fn rereads_top(&self) -> bool { self.0.rereads_top() }
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline { self.0.pipeline_desc(px, k) }
    fn once(&self) -> bool { self.0.once() }
    fn open_words(&self, w: &mut OpenWords) { self.0.open_words(w) }
    fn read_plan(&self) -> ReadPlan { self.0.read_plan() }
    fn verdicts(&self) -> &'static [VerdictKind] { self.0.verdicts() }
    fn decode_order(&self) -> DecodeOrder { self.0.decode_order() }
    fn fill_late(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, win: &Win, at: Option<(Value, Mask)>) { self.0.fill_late(b, env, win, at) }
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        raise(b, env, self.1);
        self.0.open(b, env)
    }
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win { self.0.next_window(b, env, l_eof) }
    fn word_win<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, win: &Win) -> Option<WordWin<'e>> { self.0.word_win(b, env, win) }
    fn packed(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, win: &Win, attno: i16, base: Value) -> Option<Packed> { self.0.packed(b, env, win, attno, base) }
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> { self.0.row(b, env, win, j, carry, next) }
    fn granule_ident(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, win: &Win) -> Option<GranuleIdent> { self.0.granule_ident(b, env, win) }
    fn covers(&self) -> u64 { self.0.covers() }
    fn passed(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, win: &Win, row: &RowVals) { self.0.passed(b, env, win, row) }
    fn fills(&self) -> bool { self.0.fills() }
    fn fills_blocks(&self) -> bool { self.0.fills_blocks() }
    fn passed_block(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, win: &Win, base: Value, m: Mask) { self.0.passed_block(b, env, win, base, m) }
    fn locator(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) -> Option<Locator> { self.0.locator(b, env, row) }
    fn nominal_site(&self, px: &Px<'_, '_>) -> Option<usize> { self.0.nominal_site(px) }
    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, out: Value) { self.0.write_back(b, env, cur, out) }
    fn copy_entry(&self) -> Option<&crate::rerun::epq::copy_row::CopyEntry> { self.0.copy_entry() }
}

/// PG's raise at the Sort's start: `m6_raise` (42P22) or `m11_raise_no_proc` (XX000) stashes it and the open leaves
/// through `l_exit` with its status (always below zero: the producer's own open after it never runs).
fn raise(b: &mut FunctionBuilder<'_>, env: &Env, what: What) {
    let flags = gravity_ir::HostFlags { may_error: true, ..Default::default() };
    let st = match what {
        What::Coll => {
            let h = b.declare_host_once("m6_raise", &[Type::Ptr, Type::I64], Some(Type::I64), gravity_kernels::contract::entry1::<h6::M6Raise> as *const () as usize, flags);
            let code = b.iconst(Type::I64, h6::RAISE_INDETERMINATE_COLL as i64);
            b.call_host(h, &[env.ctx, code], Some(Type::I64)).expect("m6_raise returns its status")
        }
        What::NoProc(typ, family) => {
            let h = b.declare_host_once("m11_raise_no_proc", &[Type::Ptr, Type::I64, Type::I64], Some(Type::I64), m11_raise_no_proc as *const () as usize, flags);
            let (t, f) = (b.iconst(Type::I64, typ as i64), b.iconst(Type::I64, family as i64));
            b.call_host(h, &[env.ctx, t, f], Some(Type::I64)).expect("m11_raise_no_proc returns its status")
        }
    };
    let (bad, l_on) = (b.icmp(IntCond::Slt, st, env.zero), b.create_block(&[]));
    b.cond_br(bad, (env.x.l_exit, &[st]), (l_on, &[]));
    b.switch_to(l_on);
}
