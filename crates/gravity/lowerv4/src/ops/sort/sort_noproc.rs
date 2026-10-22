//! W8-EQUIVCLASS-SORTOP (M11; MERGE R5): a full Sort over a key whose btree family has neither a
//! sortsupport nor an order proc for its type (decompose `SortKey::no_proc`: equivclass.sql's int8alias1 `<`). PG 18.6's
//! first ExecSort call raises XX000 "missing support function 1(T,T) in opfamily F" (tuplesort_begin_heap) before it reads
//! a row. PG plans such a Sort as a merge join's inner, so the raise sits at PG's own ExecMergeJoin states, the inner's
//! first fetch: INITIALIZE_INNER (merge/machine.rs) and ENDOUTER (merge/mergejoin.rs `flush`), each before a lazy inner's
//! wait, through the run reader's `first_fetch`. The Sort itself runs as today (its type's word order, never read). Served only where
//! v4 reaches those states exactly when PG does (`served`); every other form refuses by decompose's NO_ORDER_PROC. A served
//! form in a statement holding a CTE, a set operation or an InitPlan that may raise is served (v0.4: the last two were
//! refused for speed only).
//! Split from runs/sort.rs (its budget). Owner M11-MERGE.
use gravity_abi::breaker::BreakerKind;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::sort;
use gravity_decompose::ops::shared::sort::SortDesc;
use gravity_decompose::{Bound, Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntCond, Type, Value};
use gravity_kernels::host_abi::m11_raise_no_proc;
use gravity_program::LifeId;
use mcx::{GrowIn, PgBox, PgVec};
use types_nodes::jointype::JoinType;

use crate::face::runs::SlotRun;
use crate::frame::env::Env;
use crate::frame::iface::{end_breaker, holds_limit};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::graph::{EndKind, Gx};
use crate::admit::refusal::{Reason, ShapeWhy};

/// Pipeline `k`'s full Sort's `(T, F)` when a key has no order proc; refused by NO_ORDER_PROC unless `served`. Called by
/// runs/sort.rs `make_step`, the maker of every full Sort op whatever its end (a merge inner's, runs.sort's, a group's, a
/// MergeAppend arm's, a top over groups'), so no end maker checks it again. v0.4: a served form in a statement holding a
/// set operation (D2197) or an InitPlan that may raise (its plan-order recheck) is served; those forms were refused for
/// speed only, never for their answer.
pub(crate) fn checked(gx: &Gx<'_, '_>, k: usize) -> Option<(u32, u32)> {
    let p = &gx.pipelines()[k];
    let (np, d) = p.ops.iter().find_map(|o| if let Op::Sort(d) = o { sort::no_proc(d).map(|np| (np, d)) } else { None })?;
    if !served(gx, k, d) {
        crate::refuse!(p.node_id, Reason::Shape { why: ShapeWhy(sort::NO_ORDER_PROC) })
    }
    Some(np)
}

/// v0.4: the pipeline that begins pipeline `k`'s Sort at its own first call: `k`'s run is the source of one reader `q`
/// alone, and `q`'s ops each pull their input at their first call (decompose `sort::starts_raise`: a projection, a filter,
/// a Limit that is not of 0, a Unique's first-of-run, a Sort or a Materialize's spool of its own): PG's node above the Sort calls it the first time
/// `q`'s reader calls
/// `q`. `q` itself is no union arm.
pub(crate) fn puller(gx: &Gx<'_, '_>, k: usize) -> Option<usize> {
    let life = Some(LifeId(k as u16));
    let mut r = (0..gx.graph.pipelines.len()).filter(|&q| q != k && reads(gx, q, k));
    let (Some(q), None) = (r.next(), r.next()) else { return None };
    let p = &gx.pipelines()[q];
    let sourced = matches!(p.source, Source::Breaker { slot } if gx.life_of(q, slot) == life);
    // a Sort of its own pulls its whole input at its first call too (a Unique's Distinct sorted again)
    (sourced && p.union.is_none() && p.ops.iter().all(|o| sort::starts_raise(o) || matches!(o, Op::Sort(_) | Op::Join(JoinDesc::Spool)))).then_some(q)
}

/// v0.4: the pipelines pulled from pipeline `k` up: `puller` of `k`, then of each puller whose end PG's node reads at its
/// reader's first call (any breaker but a hash build or a top: a Materialize, a Unique's or an aggregate's, a Sort), in
/// order; at most one per pipeline of the graph.
fn pullers<'g>(gx: &Gx<'g, '_>, k: usize) -> PgVec<'g, usize> {
    let mut out = PgVec::new_in(gx.mcx);
    let mut cur = k;
    while let Some(q) = puller(gx, cur) {
        if out.contains(&q) || out.len() > gx.graph.pipelines.len() {
            break;
        }
        out.push_in(q);
        if matches!(end_breaker(gx, q), None | Some(BreakerKind::HashBuild | BreakerKind::TopK)) {
            break;
        }
        cur = q;
    }
    out
}

/// v0.4: pipeline `r` runs exactly when PG first calls its node: `r` is the root; or its end is a breaker whose PG node
/// reads its whole input at its own first call (an aggregate, a hashed DISTINCT, a Sort) and its one reader reads it as
/// its source by ops that pull at their first call, itself so driven; or `r` fills a hashed SubPlan's set, which a set
/// holding a no-order-proc raise builds LAZY at its first probe (ops/subplan/hashed.rs `no_proc_set`), as PG's
/// buildSubPlanHash runs at its first ExecHashSubPlan.
fn driven(gx: &Gx<'_, '_>, r: usize, depth: usize) -> bool {
    use gravity_decompose::ops::shared::subplan::SubForm;
    if r == gx.graph.root as usize {
        return true;
    }
    if depth > gx.graph.pipelines.len() {
        return false;
    }
    let p = &gx.pipelines()[r];
    if p.ops.iter().any(|o| matches!(o, Op::SubPlan(d) if matches!(d.form, SubForm::Build))) {
        return true;
    }
    matches!(end_breaker(gx, r), Some(BreakerKind::PlainAgg | BreakerKind::HashAgg | BreakerKind::Distinct | BreakerKind::Sort))
        && puller(gx, r).is_some_and(|q| driven(gx, q, depth + 1))
}

/// v0.4: whether pipeline `k` (a set's pipeline, or one feeding it) holds a Sort whose key has no order proc, or reads
/// (transitively, through the pipelines whose runs it consumes) one that does: its set builds LAZY at its first probe.
pub(crate) fn no_proc_set(gx: &Gx<'_, '_>, k: usize) -> bool {
    let mut seen = mcx::pg_vec![in gx.mcx; k];
    let mut at = 0;
    while let Some(&q) = seen.get(at) {
        at += 1;
        if gx.pipelines()[q].ops.iter().any(|o| matches!(o, Op::Sort(d) if sort::no_proc(d).is_some())) {
            return true;
        }
        for x in crate::effects::producers(gx, q) {
            if !seen.contains(&x) {
                seen.push_in(x);
            }
        }
    }
    false
}

/// Pipeline `k`'s run is read by the ROOT pipeline alone, through one merge join as its inner, so that merge's machine is
/// PG's call of the Sort: no other op or pipeline reads the run (a Materialize or nested merge, a hash build, a CTE, a
/// SubPlan, a MergeAppend arm, an aggregate, a window, a root Sort: PG may call those never or first), only Filter /
/// Project or a constant Limit after the merge, no Limit or gate before it, and for RIGHT / FULL / RIGHT ANTI any op
/// before it (ENDOUTER runs on the frame's flush path after an earlier op ends the call: D2141). A D1292 key would raise
/// 42P22 first.
/// equivclass#53: or the run's lone reader is a Materialize-ended pipeline `p` that reads it through one merge join by
/// the same rule, and `p`'s run is read by the root alone, through one merge join as its inner by that rule: PG calls
/// `p`'s merge only from the root merge's inner fetches (the Materialize pulls it), and `p`, whose machine raises
/// (markable.rs `no_proc_reader`), is an inner that may raise to the root merge: it waits in the root's lazy inner (D824)
/// or runs ahead under errorhold's licence (D1236), so `p`'s first inner fetch raises when PG's does. D2206 / the root
/// form: or the Sort begins as the program's first act (D1292's scope, effects `first_act`: the root reads its run, or a
/// WindowAgg's buffer of it), and its producers' open raises (sort_raise.rs).
fn served(gx: &Gx<'_, '_>, k: usize, d: &SortDesc<'_>) -> bool {
    let root = gx.graph.root as usize;
    let readers = |k: usize| (0..gx.graph.pipelines.len()).filter(move |&q| reads(gx, q, k));
    let materialized = |p: usize| matches!(gx.end_kind(p), EndKind::Breaker(BreakerKind::Materialize));
    let nested = || {
        let mut r = readers(k);
        let (Some(p), None) = (r.next(), r.next()) else { return false };
        p != root && materialized(p) && inner_of(gx, k, p) && readers(p).eq([root]) && inner_of(gx, p, root)
    };
    // v0.4: or `k`'s run is pulled by one pipeline `q` (`puller`) whose run the root reads alone through one merge join
    // as its inner: a Materialize or a Unique over the Sort as a merge's inner (a small work_mem, a semi join's inner made
    // unique): PG's merge calls `q`'s node at its first inner fetch, which calls the Sort; the root's reader of `q`'s run
    // raises there (`wrap`)
    // v0.4: the merge's reader need not be the root: one PG calls exactly when it is first called (`driven`: an aggregate
    // over the merge, a hashed SubPlan's set built at its first probe), and the chain of pullers above `k` (a Unique made
    // into a Distinct and a Sort again) counts as the Sort's call at its last puller's first fetch
    let merged = |q: usize| {
        let mut r = readers(q);
        matches!((r.next(), r.next()), (Some(x), None) if inner_of(gx, q, x) && driven(gx, x, 0))
    };
    let pulled = || pullers(gx, k).into_iter().any(merged);
    end_breaker(gx, k) == Some(BreakerKind::Sort)
        && !gravity_decompose::effects::sort_raises(gx.facts, d)
        && (merged(k) || nested() || pulled() || crate::effects::visible::sort::first_act(gx, k))
}

/// Whether pipeline `q` reads pipeline `k`'s run (a slot it consumes or its source, in `k`'s life).
fn reads(gx: &Gx<'_, '_>, q: usize, k: usize) -> bool {
    let src = if let Source::Breaker { slot } = gx.pipelines()[q].source { Some(slot) } else { None };
    gx.pipelines()[q].desc.consumes.iter().copied().chain(src).any(|s| gx.life_of(q, s) == Some(LifeId(k as u16)))
}

/// `served`'s rule for one reader: pipeline `r` reads `k`'s run through exactly one merge join as its inner, not as its
/// source, with no union, only Filter / Project after the merge (a Materialize-ended reader's own spool op last), and
/// before it no Limit or gate (RIGHT / FULL / RIGHT ANTI: any op, D2141).
fn inner_of(gx: &Gx<'_, '_>, k: usize, r: usize) -> bool {
    let life = Some(LifeId(k as u16));
    let p = &gx.pipelines()[r];
    let mut merges = p.ops.iter().enumerate().filter_map(|(j, op)| match op {
        Op::Join(JoinDesc::Merge { slot, jointype, .. }) if gx.life_of(r, *slot) == life => Some((j, *jointype)),
        _ => None,
    });
    let (Some((j, jt)), None) = (merges.next(), merges.next()) else { return false };
    let before = match jt {
        // v0.4: a RIGHT / FULL / RIGHT ANTI merge calls its inner whatever its outer gives: at INITIALIZE_INNER after an
        // outer row, else at ENDOUTER, which the frame's flush path runs after any earlier op that ends the call (a LIMIT,
        // a false gate, an inner merge or an empty build before it: D2141), as PG runs the merge over an empty outer
        JoinType::JOIN_RIGHT | JoinType::JOIN_RIGHT_ANTI | JoinType::JOIN_FULL => true,
        _ => !holds_limit(&p.ops[..j]) && !p.ops[..j].iter().any(|op| matches!(op, Op::Gate(_))),
    };
    // v0.4: or a Limit of constant bounds above the merge: PG's Limit calls the merge at its first call unless its count
    // is 0 (then never: no pipeline row reaches the merge either), and stops only after rows the merge returned, each
    // past its first inner call where one is made
    let above = |ops: &[Op<'_>]| ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_) | Op::Limit { count: Bound::Const(_) | Bound::All, offset: Bound::Const(_) | Bound::All, ties: None }));
    let after = match p.ops[j + 1..].split_last() {
        Some((Op::Join(JoinDesc::Spool), rest)) => plain(rest),
        // a hashed SubPlan's set fed by the merge (`driven`: built LAZY at its first probe): its Build passes rows on
        Some((Op::SubPlan(d), rest)) if matches!(d.form, gravity_decompose::ops::shared::subplan::SubForm::Build) => plain(rest),
        _ => above(&p.ops[j + 1..]),
    };
    let src = if let Source::Breaker { slot } = p.source { Some(slot) } else { None };
    src.is_none_or(|s| gx.life_of(r, s) != life) && p.union.is_none() && after && before
}

/// Ops that pass or drop rows and never end the pipeline.
fn plain(ops: &[Op<'_>]) -> bool {
    ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_)))
}

/// Merge join reader `run` of producer `q`'s run (runs/slot_run.rs `reader`): its first fetch raises when `q`'s Sort does.
pub(crate) fn wrap<'g>(gx: &Gx<'g, '_>, q: usize, run: PgBox<'g, dyn SlotRun + 'g>) -> PgBox<'g, dyn SlotRun + 'g> {
    // v0.4: or `q` pulls such a Sort's run (`puller`, `served`'s pulled form): the raise is the Sort's, at `q`'s first call
    let pulled = || (0..gx.graph.pipelines.len()).find(|&k| k != q && pullers(gx, k).contains(&q)).and_then(|k| checked(gx, k));
    match checked(gx, q).or_else(pulled) {
        Some(np) => mcx::pg_box_dyn!(gx.mcx, Raise(run, np) => dyn SlotRun + 'g),
        None => run,
    }
}

/// The reader whose first fetch raises; every other call is the reader's.
struct Raise<'g>(PgBox<'g, dyn SlotRun + 'g>, (u32, u32));

impl SlotRun for Raise<'_> {
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) -> Value {
        self.0.open(b, env, x)
    }
    fn fetch(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, j: Value) -> Value {
        self.0.fetch(b, env, j)
    }
    fn vals<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, p: Value, carry: Carry<'e>, next: Value, at: Value) -> RowVals<'e> {
        self.0.vals(b, env, p, carry, next, at)
    }
    fn mark(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, j: Value) {
        self.0.mark(b, env, j)
    }
    /// PG's raise: `m11_raise_no_proc` stashes it and the call leaves through `l_exit` with its status (always below zero).
    fn first_fetch(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        let h = b.declare_host_once("m11_raise_no_proc", &[Type::Ptr, Type::I64, Type::I64], Some(Type::I64), m11_raise_no_proc as *const () as usize, gravity_ir::HostFlags { may_error: true, ..Default::default() });
        let (typ, family) = self.1;
        let (t, f) = (b.iconst(Type::I64, typ as i64), b.iconst(Type::I64, family as i64));
        let st = b.call_host(h, &[env.ctx, t, f], Some(Type::I64)).expect("m11_raise_no_proc returns its status");
        let (bad, l_on) = (b.icmp(IntCond::Slt, st, env.zero), b.create_block(&[]));
        b.cond_br(bad, (env.x.l_exit, &[st]), (l_on, &[]));
        b.switch_to(l_on);
    }
}
