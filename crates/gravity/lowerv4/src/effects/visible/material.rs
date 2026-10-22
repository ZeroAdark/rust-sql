//! What a plan Material's fill or a nest's Materialize inner may show the user when it runs ahead of PostgreSQL's lazy
//! fill (nodeMaterial.c ExecMaterial reads its child one row per call): the material family's sibling file of
//! effects/visible.rs.
//!
//! nest.spool's quiet child (D1108 row 1, V4R-541): a plan Material whose child is made of constants only, so its eager
//! form (spool_form.rs) is PostgreSQL's Material whatever FACTS-1b's F1 / F5 say of it. Each source is a FROM function's
//! one row (`Source::Const`), a VALUES list of such expressions (`Source::Values`, material V-QUIET) or the spool of another
//! such Material; each op a FROM-clause loop of builtin series or unnest calls over constants, a filter or a projection
//! D449's rule passes (props merge.rs `ops_can_raise`: builtin leakproof calls over raise-free arguments, never a user
//! function), or an INNER nest without parameters over an inner of the same form whose join qual that rule passes.
//! generate_series over integers raises only on a zero step (its overflow ends the series) and unnest never raises, so
//! nothing in the child can raise; it reads no relation and no executor parameter (checked here on every filter,
//! projection, set-function target list and nest qual: spool_form.rs `exec_param`, never left to D449's rule), so every
//! rescan returns the same rows (the EPQ parameter an INSERT's plan adds to every scan's allParam is never read). With no
//! VOLATILE call in the reader nest's facts, filling it up front shows nothing PostgreSQL's lazy fill does not (an empty
//! outer, a SEMI or LIMIT stop, a cursor: the same rows; a STABLE call does not act, C1).
use gravity_abi::breaker::BreakerKind;
use gravity_decompose::effects::{self, Volatility};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::srf::{SrfFn, SrfKind, SrfPlace};
use gravity_decompose::{Op, PipelineNode, Source};
use types_nodes::jointype::JoinType;
use types_nodes::{Node, NodeList};

use crate::graph::{EndKind, Gx};
use crate::props::ops_can_raise;
use crate::rerun::spool::form::exec_param;

/// Whether fill pipeline `k` (ops ending in its Spool) is a quiet child.
pub(crate) fn quiet(gx: &Gx<'_, '_>, k: usize) -> bool {
    let p = &gx.pipelines()[k];
    match p.ops.split_last() {
        Some((Op::Join(JoinDesc::Spool), body)) => quiet_in(gx, p, body),
        _ => false,
    }
}

/// `p`'s source and `ops` (a nest inner's: all of its ops) are of the quiet form.
fn quiet_in(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, ops: &[Op<'_>]) -> bool {
    crate::stack_check(); // D3832
    let source = match p.source {
        Source::Const => true,
        // a VALUES list whose every expression D449's rule passes and reads no executor parameter (updatable_views
        // rw_view1's MERGE USING (VALUES ..): a write's plan puts its EvalPlanQual parameter in every scan's allParam, so
        // FACTS-1b's F1 never holds there and only this form keeps the Material)
        Source::Values { lists, .. } => lists.iter().all(|l| l.as_list().is_some_and(|l| !l.iter().any(exec_param) && !ops_can_raise(gx, &[Op::Project(l)]))),
        Source::Breaker { slot } => {
            let d = &p.desc;
            let life = d.consumes.iter().position(|&s| s == slot).and_then(|j| d.consumes_from.get(j).copied());
            let ps = gx.pipelines();
            // the producer's end is a Materialize breaker (a quiet producer writes no row, so its end kind is its declared end)
            life.map(|l| l.0 as usize).is_some_and(|q| q < ps.len() && matches!(gx.end_kind(q), EndKind::Breaker(BreakerKind::Materialize)) && quiet(gx, q))
        }
        _ => false,
    };
    source
        && ops.iter().all(|op| match op {
            Op::Srf(d) => matches!(d.place, SrfPlace::From { .. }) && d.functions.iter().all(series) && !d.tlist.iter().any(exec_param) && !ops_can_raise(gx, &[Op::Project(d.tlist)]),
            Op::Filter(q) => !exec_param(*q) && !ops_can_raise(gx, core::slice::from_ref(op)),
            Op::Project(l) => !l.iter().any(exec_param) && !ops_can_raise(gx, core::slice::from_ref(op)),
            Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, params, inner, qual, .. }) => {
                params.is_nil() && !qual.iter().any(exec_param) && !ops_can_raise(gx, &[Op::Project(*qual)]) && quiet_in(gx, inner, &inner.ops)
            }
            _ => false,
        })
}

/// A builtin series or unnest call over constants that raises on none: generate_series over int4 / int8 (a step a non-null
/// nonzero constant: the step's class both keys hold, lower key.rs `series_step`, never the literal), unnest(anyarray).
fn series(f: &SrfFn<'_>) -> bool {
    let consts = f.args.iter().all(|a| a.as_const().is_some());
    match f.kind {
        SrfKind::SeriesInt { step: false, .. } | SrfKind::Unnest => consts,
        SrfKind::SeriesInt { step: true, .. } => consts && gravity_lower::inputs::key::series_step(f.funcid, f.args) == Some(true),
        _ => false,
    }
}

/// `spool_fill`'s plain eager form: a reader's facts invariant and raise-free with no VOLATILE call (D1630's class when
/// the reader is two or more levels down under a word bound once at bind). A STABLE call fills up front only as F5's
/// per-call predicate takes it raise-free (D1637's eager sweep): it raises on no input, and STABLE code does not act.
pub(crate) fn calm_fill(invariant: bool, raise_free: bool, volatility: Volatility) -> bool {
    invariant && raise_free && volatility != Volatility::Volatile
}

/// D1108 row 1 (`spool_fill`): a reader with no VOLATILE call (C1, as `calm_fill`) over fill pipeline `k`'s child of
/// constants only (`quiet`), invariant and raise-free whatever F1 / F5 say.
pub(crate) fn quiet_fill(gx: &Gx<'_, '_>, k: usize, volatility: Volatility) -> bool {
    volatility != Volatility::Volatile && quiet(gx, k)
}

/// Rule A's nest between the union and its sort (psql's `\dd` describe: the union of catalog arms probing
/// pg_description by its index): a nest reads every outer row whatever its join type, so PostgreSQL still reads every arm
/// to its end, and its per-row work shows nothing between two arms' rows when its inner is one heap or index scan whose
/// keys, filters and projections are quiet (rerun/union_arms.rs `quiet_in`, the nest's own parameters bound) and its
/// parameters and join qual are quiet too. D1486 (window, heldout rnd-3175 / rnd-0098): PostgreSQL's Materialize over that
/// inner scan (a plan Material spool the inner reads at every re-open, `nest_fill` from pipeline `k`; an arm's nest,
/// union_arms.rs `quiet_arm_in`, passes none and takes no spool) shows nothing either: its one fill runs once, quietly.
pub(crate) fn quiet_nest(gx: &Gx<'_, '_>, k: Option<usize>, params: &NodeList<'_>, qual: &NodeList<'_>, inner: &PipelineNode<'_>) -> bool {
    use crate::rerun::union_arms::quiet_in;
    let q = |n: Node<'_>| quiet_in(gx, n, true);
    let src = match &inner.source {
        Source::Range { .. } => true,
        Source::Index { keys, recheck, .. } => keys.iter().all(|x| q(x.value) && (x.opno == 0 || effects::builtin_oper(x.opno).is_some())) && recheck.iter().all(q),
        // PostgreSQL's Materialize over the inner scan: the fill's keys, recheck and ops, each quiet
        Source::Breaker { slot } => k.and_then(|k| nest_fill(gx, k, inner, *slot)).is_some_and(|f| {
            let fq = |n: Node<'_>| quiet_in(gx, n, false);
            let keys = match &f.source {
                Source::Index { keys, recheck, .. } => keys.iter().all(|x| fq(x.value) && (x.opno == 0 || effects::builtin_oper(x.opno).is_some())) && recheck.iter().all(fq),
                _ => true,
            };
            keys && f.ops.split_last().is_some_and(|(_, body)| body.iter().all(|op| match op {
                Op::Filter(n) | Op::Gate(n) => fq(*n),
                Op::Project(l) => l.iter().all(fq),
                _ => false,
            }))
        }),
        _ => false,
    };
    let own = |s: &u8| matches!(inner.source, Source::Breaker { slot } if slot == *s);
    src && inner.desc.consumes.iter().all(own)
        && params.iter().all(|p| p.as_nest_loop_param().is_some_and(|p| q(p.paramval)))
        && qual.iter().all(q)
        && inner.ops.iter().all(|op| match op {
            Op::Filter(n) | Op::Gate(n) => q(*n),
            Op::Project(l) => l.iter().all(q),
            _ => false,
        })
}

/// `quiet_nest`'s Material inner: the inner of a nest in pipeline `k` reads a plan Material spool through `slot`; its
/// fill (the life the inner reads, `consumes_from`, else the slot's nearest producer before `k`) when that is one heap or
/// index scan, reading no other breaker, of filters, gates and projections ending in the spool (PostgreSQL's Materialize
/// over a plain scan, run once, at the first re-open). Was union_arms.rs `spool_fill`.
fn nest_fill<'g, 'p>(gx: &'g Gx<'_, 'p>, k: usize, inner: &PipelineNode<'_>, slot: u8) -> Option<&'g PipelineNode<'p>> {
    let (d, ps) = (&inner.desc, gx.pipelines());
    let life = d.consumes.iter().position(|&s| s == slot).and_then(|j| d.consumes_from.get(j).copied());
    let f = life.map(|l| l.0 as usize).or_else(|| (0..k).rev().find(|&q| ps[q].desc.produces() == Some(slot)))?;
    let fill = ps.get(f).filter(|_| f < k)?;
    let scan = matches!(fill.source, Source::Range { .. } | Source::Index { .. });
    let spool = matches!(fill.ops.split_last(), Some((Op::Join(JoinDesc::Spool), body)) if body.iter().all(|op| matches!(op, Op::Filter(_) | Op::Gate(_) | Op::Project(_))));
    (scan && spool && fill.desc.consumes.is_empty() && fill.desc.produces() == Some(slot)).then_some(fill)
}

/// D3749 (River's JobSchedule with few due jobs): fill pipeline `k`, or a pipeline feeding it, reads the store of a CTE
/// whose run locks rows, calls VOLATILE code or may raise (its InitPlan's facts, or its side as a build's is classed:
/// hashjoin probe.rs `pulls`' reading). A nest's facts leave out the CTEs its outer's first row ran (decompose rerun.rs
/// `first_row`: River's FOR UPDATE claim under the window CTE both sides read), so such a fill runs only at the nest's
/// first outer row (lazy.rs), once PostgreSQL's outer ran the CTE: in pipeline order it would run the CTE first, its locks
/// ahead of the outer's first row (V4R-512's order, an empty outer included).
pub(crate) fn pulls_loud_cte(gx: &Gx<'_, '_>, k: usize) -> bool {
    use crate::ops::hashjoin::build_class::{feed, side, Side};
    use gravity_decompose::ops::shared::subplan::SubLink;
    let ps = gx.pipelines();
    let src = |x: usize| if let Source::Breaker { slot } = ps[x].source { Some(slot) } else { None };
    let loud = |q: usize| {
        let cte = gx.graph.init_plans.iter().find(|i| i.pipeline as usize == q && matches!(i.link, SubLink::Cte));
        cte.is_some_and(|i| crate::effects::visible::hashjoin::init_loud(i.raise_free, i.volatility) || side(gx, q) != Side::Safe)
    };
    let reads = |x: usize| ps[x].desc.consumes.iter().copied().chain(src(x)).any(|s| gx.life_of(x, s).is_some_and(|l| loud(l.0 as usize)));
    k < ps.len() && core::iter::once(k).chain(feed(gx, k, ps[k].desc.consumes.iter().copied().chain(src(k)))).any(|x| x < ps.len() && reads(x))
}

/// D1108's held form (rerun/spool/form.rs `held_child`): the plan Material in `slot` that fill pipeline `k` or an inner it
/// embeds reads (errorhold_d1108_two_level's inner Materialize over e2l_d) shows nothing when made ahead of PostgreSQL's
/// lazy fill and returns the same rows at every read: its fill takes the pull or the eager form (`spool_fill`), its side
/// is Safe (build_class `side_by`: it and every pipeline feeding it raise nothing and call no user or VOLATILE function),
/// no rerun set refills any of them and none reads an executor parameter.
pub(crate) fn store_quiet(gx: &Gx<'_, '_>, k: usize, slot: u8) -> bool {
    use crate::ops::hashjoin::build_class::{feed, side_by, Side};
    let ps = gx.pipelines();
    let Some(f) = gx.life_of(k, slot).map(|l| l.0 as usize).filter(|&f| f < ps.len() && f != k) else { return false };
    let src = if let Source::Breaker { slot } = ps[f].source { Some(slot) } else { None };
    let feeders = feed(gx, f, ps[f].desc.consumes.iter().copied().chain(src));
    let refilled = |x: usize| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(x as u16)));
    crate::lifetime::material::spool_fill(gx, f).is_ok()
        && side_by(gx, f, usize::MAX, false) == Side::Safe
        && !core::iter::once(f).chain(feeders).any(|x| refilled(x) || crate::rerun::spool::form::reads_param(&ps[x]))
}

/// D3803 (rerun/spool/form.rs `first_run`): ops `ops` of a fill are per-row work that shows nothing, run one row at a
/// time as far as PostgreSQL's Material is read, each quiet (`effects::expr_quiet`; whether one reads a parameter from
/// outside the child is form.rs `reads_param`'s): filters, gates, projections, hash probes (not a catalog probe) of their
/// keys, clauses and join qual, a constant LIMIT / OFFSET, and a nest whose inner raises nothing and calls nothing
/// VOLATILE, its join qual and bound values quiet (`nest_step_quiet`: PostgreSQL rescans that inner per row with its own
/// parameters, an index lookup or a Memoize hit that shows nothing).
pub(crate) fn rows_quiet(gx: &Gx<'_, '_>, ops: &[Op<'_>]) -> bool {
    use gravity_decompose::graph::Bound;
    let quiet = |n: Node<'_>| gx.fx(n).quiet_calm();
    let fixed = |b: &Bound<'_>| !matches!(b, Bound::Expr(_));
    ops.iter().all(|op| match op {
        Op::Filter(n) | Op::Gate(n) => quiet(*n),
        Op::Project(l) => l.iter().all(quiet),
        Op::Join(JoinDesc::Probe { catalog: false, keys, clauses, qual, .. }) => keys.iter().chain(clauses.iter()).chain(qual.iter()).all(quiet),
        Op::Join(JoinDesc::Nest { .. }) => crate::effects::nest_step_quiet(op, &quiet, false),
        op => crate::frame::iface::demand(op).is_some_and(|d| !d.ties && fixed(d.count) && fixed(d.offset)),
    })
}
