//! The plain fold's drain (kind `fold.plain`'s source; split from plain.rs for D345, V4R-434, main 09-28). Hooks /
//! levers: -. Budget 100 lines. Owner M1-FOLD.
//!
//! One row over the plain end's merged record: every aggregate's final into a finals area the drain pipeline's HAVING
//! and projection read `Aggref`s from, no row over an empty input when every key was dropped (D345's `empty_no_row`),
//! and the group's kept pinned columns (fold/kept.rs) as the row's columns.
use gravity_abi::breaker::{RecordLayout, LAYOUT_IMPLICIT_COUNT};
use gravity_decompose::ops::shared::agg::{AggDecl, NONE};
use gravity_expr::emit::{AggCell, CellValid};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntBinOp, Type, Value};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};

use crate::face::fold::{Acc, FoldClass};
use crate::face::kinds::KindId;
use crate::face::source::{SourceMech, WinConds};
use crate::frame::env::{AggRow, Env};
use crate::frame::frame::{SourceMaker, Win};
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::admit::refusal::Reason;

use crate::ops::agg::trans::hosted::{arena_word, hosted, hosted_slot};
use crate::ops::agg::trans::slots::{layout_of, record_at};
use crate::ops::shared::slots::{slot_ok, slot_valid};

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::FoldPlain, &SourceMaker(make_drain))], ..crate::ops::NONE };

/// The drain: one row over the merged record, every aggregate's final into a finals area (aggno `i` at byte `16 i`, its
/// validity at `16 i + 8`), which the drain pipeline's HAVING and projection read `Aggref`s from (`Env::aggs`).
struct Drain<'g, 'p> {
    folds: PgVec<'g, (&'g AggDecl<'p>, &'static dyn FoldClass, i32, CellValid)>,
    layout: RecordLayout,
    n: usize,
    /// Every key the planner dropped (a GroupAggregate over no key, D345): no row when no row was folded.
    empty_no_row: bool,
    arena: Option<u16>,
    /// D345: the kept pinned columns, read back as the row's columns (fold/kept.rs).
    kept: PgVec<'g, (crate::ops::agg::trans::kept::Kept, gravity_program::rep::ExprOut)>,
    at: u64, // the consumed region of the record: 0, or an embedded inner's among its owner's (D1339 row D)
    /// D1520 (aggtrans T7b): the ordered-set direct arguments that read no column, compiled (`direct_exprs`).
    direct: PgVec<'g, (i16, usize, crate::frame::px::ExId, crate::cell::Cell)>,
    /// D1371: the folds (indices into `folds`) whose host sort and transitions run ahead of every final, in PG's transno
    /// order (effects `folds_ahead`).
    ahead: PgVec<'g, usize>,
}

fn make_drain<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let node = gx.pipelines()[k].node_id;
    let Some(g) = gx.drained_group(k) else { crate::refuse!(node, Reason::Bug { payload: "a plain drain without its group" }) };
    if g.aggs.iter().any(|a| a.slot == NONE && !hosted(a)) {
        // a DISTINCT / ordered aggregate: its kind's end (keyed.distinct_grouped, runs.group) owns it, and the end refused
        // first; reached past that end's diag placeholder only (D542: the mode read slot NONE, abi breaker.rs's panic)
        crate::refuse!(node, Reason::Shape { why: crate::admit::refusal::ShapeWhy("a plain drain over a slotless aggregate (its kind's end refused)") })
    }
    let layout = layout_of(px, g);
    let slot = |a: &AggDecl<'_>| if hosted(a) { hosted_slot(&layout, g, a) } else { a.slot };
    let folds = g.aggs.iter().map(|a| (a, crate::ops::agg::trans::class_of::class_in(&node, a, true, px.mcx()), layout.slot_off(slot(a) as usize) as i32, slot_valid(&layout, slot(a)))).collect_in(px.mcx());
    let n = g.aggs.iter().map(|a| a.aggno as usize + 1).max().unwrap_or(0);
    let kept = crate::ops::agg::trans::kept::drain_of(px, k, g, &layout, node);
    let arena = (g.aggs.iter().any(|a| hosted(a)) || !kept.is_empty()).then(|| arena_word(px, k, g));
    let at = region(px, k, node); // D1339 row D
    let direct = direct_exprs(px, g, node);
    let ahead = crate::effects::visible::group::folds_ahead(gx, g);
    mcx::pg_box_dyn!(px.mcx(), Drain { folds, layout, n, empty_no_row: g.empty_no_row, arena, kept, at, direct, ahead } => dyn SourceMech + 'g)
}

/// D1339 row D (review C1-C3): inner `k`'s record region. 0 (Row B's read, IR unchanged) when the slot is the first region of
/// the graph pipeline whose call runs `k`; past it, its index in the running call's declared list (`Px::consumed`: a Drain,
/// spooled drain or tail's), equal to `Pipes::consumed_at`'s (the list of the graph pipeline at the root of `k`'s owners) unless
/// that call declares its own list (a spooled drain's tail: gen rr-0079), no LAZY hashed SubPlan's slot before it
/// (frame/program.rs drops it later); else D1339, never 0.
/// D2600 (P1 CRASH, the D1337 widening of rerun#201): an inner whose owner is itself an inner (a SubPlan over a plain aggregate
/// in a nest's inner) took region 0 unconditionally, and region 0 of the running call is the owner's first breaker (the nest's
/// outer HashAggregate): record_consumed opened another breaker's region with the plain record's layout and the backend
/// aborted. Its record is region 0 only when the call's first region is its slot, else the region at its slot's index as for
/// a graph-owned inner: an inner-owned inner runs in its root graph pipeline's call too (gen subplan values.lateral, sp-0123:
/// a LATERAL VALUES list's second aggregate SubPlan; e2e subplan_inner_owned_record).
fn region(px: &Px<'_, '_>, k: usize, node: i32) -> u64 {
    let ps = px.gx.pipelines();
    let (gravity_decompose::Source::Breaker { slot }, Some(i)) = (&ps[k].source, k.checked_sub(ps.len())) else { return 0 };
    let (o, run, lazy) = (ps.inners()[i].0 .0, px.ident(k), gravity_program::protos::SUBHASH_LAZY);
    // D3745: the set's slot in the running call's life (`life_of`), as rerun/sets.rs `run_region` reads it: a slot number
    // another life reuses (a hashed set inside a CTE's pulled plan, the call's LATERAL aggregate later) names no set here
    let set_in = |p: u16, s: u8| px.gx.life_of(run, s).is_some_and(|l| l.0 == p);
    let before = |at: usize| px.protos.iter().any(|p| p.kind == gravity_program::protos::ProtoKind::SubHash && p.arg & lazy != 0 && ps.get(p.pipeline as usize).and_then(|q| q.desc.produces()).is_some_and(|s| set_in(p.pipeline, s) && px.consumed(run, s).is_some_and(|j| j < at)));
    let mut g = o; // the graph pipeline whose call runs `k` (an inner's owner may be an inner)
    while let Some(j) = g.checked_sub(ps.len()) {
        g = ps.inners()[j].0 .0;
    }
    if ps[g].desc.consumes.first() == Some(slot) {
        return 0;
    }
    // a pass's virtual id (frame/program.rs `pass_id`: a spill pair's or a two-pass's, its makers run before `Px::add_pass`
    // records it) names no pipeline, so `consumed(run, ..)` indexed the graph past its end and lowering panicked (gen sp-0111
    // under a spill pair); the pass's call's regions are not proven here: D1339's name
    if run >= ps.end() {
        crate::refuse!(node, Reason::Shape { why: crate::admit::refusal::ShapeWhy(crate::rerun::sets::D_RERUN_EMIT) })
    }
    match (px.consumed(run, *slot), ps.consumed_at(k, *slot)) {
        (Some(a), Some(b)) if a == b && !before(a) => a as u64,
        // D2743 (rnd-2894): past a LAZY hashed SubPlan's set, whose slot frame/program.rs drops from the call's list, the
        // running call's index, the lower (`Px::consumed` skips the set, D2325), as rerun/sets.rs `run_region` gives a sort run
        (Some(a), Some(b)) if a < b && px.tails.iter().all(|t| t.ident != run) => a as u64,
        // a call that declares its own list (a spooled drain's tail, its spool slot moved last) binds its regions in that
        // list's order: the record at its index there (rerun/sets.rs `run_region`, D2331's road for a sort run)
        (Some(_), Some(_)) if px.tails.iter().any(|t| t.ident == run) => match crate::rerun::sets::run_region(px, k, *slot) {
            Some(a) => a as u64,
            None => crate::refuse!(node, Reason::Shape { why: crate::admit::refusal::ShapeWhy(crate::rerun::sets::D_RERUN_EMIT) }),
        },
        _ => crate::refuse!(node, Reason::Shape { why: crate::admit::refusal::ShapeWhy(crate::rerun::sets::D_RERUN_EMIT) }),
    }
}

/// D1520 (aggtrans T7b): group `g`'s ordered-set direct arguments that are no constant but read no column (a Param bound
/// before the drain runs, a call over Params and constants) and do not act (the effects home), compiled for the drain's
/// one row (aggno, argument index, expression, its cell): finalize_aggregate evaluates them once per group, right before
/// the finalfn, as the drain does. One that reads a column (none is a key here) stays class.num_varlena's refusal.
fn direct_exprs<'g, 'p>(px: &mut Px<'g, 'p>, g: &gravity_decompose::ops::shared::agg::GroupDecl<'p>, node: i32) -> PgVec<'g, (i16, usize, crate::frame::px::ExId, crate::cell::Cell)> {
    use gravity_decompose::effects;
    let gx = px.gx;
    let cx = effects::Cx::without_stmt(gx.facts, px.arena);
    let mut out = PgVec::new_in(px.mcx());
    for a in g.aggs.iter().filter(|a| hosted(a)) {
        let Some(r) = a.node.as_aggref() else { continue };
        for (i, n) in r.aggdirectargs.iter().enumerate() {
            let columnless = gravity_decompose::expr::walk(n, &mut |x| x.as_var().is_none());
            if n.as_const().is_some() || !columnless || effects::of_expr(&cx, n, &effects::PLAIN).acts {
                continue;
            }
            let cell = crate::cell::Cell::of_expr(crate::step::project::out_of(gx, n, node), 0, 0, crate::cell::Nullable::Maybe);
            out.push_in((a.aggno, i, crate::step::expr::compile(px, n, None, node), cell));
        }
    }
    out
}

/// D1520: aggregate `a`'s direct arguments for its final when one is compiled (`direct_exprs`): each constant as the
/// host binds it (kernels `DIRECT_BOUND` / `DIRECT_NULL`), each compiled one's value over `row`; `None` when none is, or
/// one is neither (class.num_varlena's `consts` refuses it by name).
fn direct_args<'e>(d: &Drain<'_, '_>, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, node: i32, a: &AggDecl<'_>, row: &RowVals) -> Option<PgVec<'e, (Value, Value)>> {
    use gravity_kernels::ops::agg::trans::hosted::{DIRECT_BOUND, DIRECT_NULL};
    let r = a.node.as_aggref()?;
    let ex = |i: usize| d.direct.iter().find(|x| x.0 == a.aggno && x.1 == i);
    if !r.aggdirectargs.iter().enumerate().any(|(i, _)| ex(i).is_some()) || r.aggdirectargs.iter().enumerate().any(|(i, n)| n.as_const().is_none() && ex(i).is_none()) {
        return None;
    }
    let mut out = mcx::reserved_in(r.aggdirectargs.len(), env.mcx);
    for (i, n) in r.aggdirectargs.iter().enumerate() {
        match (n.as_const(), ex(i)) {
            (Some(c), _) => out.push_in((env.zero, b.iconst(Type::I64, (if c.constisnull { DIRECT_NULL } else { DIRECT_BOUND }) as i64))),
            (None, Some(x)) => {
                let c = crate::table::rec::ex_ref(b, env, node, row, x.2, &x.3);
                let Some(tf) = gravity_decompose::catalog::typefacts::row_of(c.cell.typid).copied() else { return None };
                out.push_in(crate::table::rec::direct_word(b, env, node, c, &tf));
            }
            (None, None) => return None,
        }
    }
    Some(out)
}

impl SourceMech for Drain<'_, '_> {
    fn once(&self) -> bool {
        true
    }
    fn open(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        env.zero
    }
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _l_eof: Block) -> Win {
        let t = b.iconst(Type::I64, 1);
        // the record's sentinel counts every folded row: a row-count slot from 0, else the implicit count from the 1 abi's
        // Rec::open stores (its `Opened` validity: at least 2)
        let end = match self.empty_no_row {
            true => {
                let rec = record_at(b, env, &self.layout, Some(self.at));
                let o = self.layout.sentinel_off();
                let v = if self.layout.flags & LAYOUT_IMPLICIT_COUNT != 0 { CellValid::Opened(o) } else { CellValid::NonZeroAt(o) };
                let any = slot_ok(b, env, rec, v);
                b.conv(gravity_ir::ConvOp::Zext, Type::I64, any)
            }
            false => env.one,
        };
        Win { cursor: env.zero, end, facts: WinConds { nonulls: Some(t), nosel: Some(t), ..WinConds::default() } }
    }
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let rec = record_at(b, env, &self.layout, Some(self.at));
        let s = b.declare_stack_slot((16 * self.n.max(1)) as u32, 8);
        let area = b.stack_addr(s);
        let mut cells = mcx::pg_vec![in env.mcx; AggCell::count(0); self.n];
        let outer = match self.arena { Some(a) => env.agg_arena.replace(a), None => env.agg_arena }; // D1370 (D1264's rule): this Agg's arena for its finals, the caller's back after (an end above the drain folds through its own)
        let (node, none) = (env.node, RowVals { row: crate::cell::Row::new_in(env.mcx), vals: PgVec::new_in(env.mcx), cols: PgVec::new_in(env.mcx), carry: carry.copy_in(env.mcx), next, at: j }); // D1520: the direct arguments read no column
        // D1371: every host-sorted aggregate's sort and transitions before the first final, as finalize_aggregates
        for &i in self.ahead.iter() {
            let (a, class, off, valid) = self.folds[i];
            class.fold_ahead(b, env, a, Acc::Slot { base: rec, off, valid });
        }
        for &(a, class, off, valid) in self.folds.iter() {
            let (v, null) = match direct_args(self, b, env, node, a, &none) {
                Some(d) => class.finish_direct(b, env, a, Acc::Slot { base: rec, off, valid }, &d[..]),
                None => class.finish(b, env, a, Acc::Slot { base: rec, off, valid }),
            };
            let i = 16 * a.aggno as i32;
            b.store(area, i, v);
            let t = b.bconst(true);
            let ok = b.int_bin(IntBinOp::Xor, null, t);
            let ok = b.conv(gravity_ir::ConvOp::Zext, Type::I8, ok);
            b.store(area, i + 8, ok);
            cells[a.aggno as usize] = AggCell::word(i as u32, CellValid::Bit { off: i as u32 + 8, bit: 0 });
        }
        env.aggs = Some(AggRow { rec: area, cells });
        let r = crate::ops::agg::trans::kept::drain_row(b, env, rec, &self.kept, (carry, next, j));
        env.agg_arena = outer;
        r
    }
}
