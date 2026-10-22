//! Rule B's window arm (material item 9, nest.spool's home; a helper of rerun/union_arms.rs `still`, which hands it a pipeline whose source is
//! a breaker; gen ma-0129: `INSERT .. SELECT .. FROM r1 UNION ALL SELECT .., count(*) OVER () .. FROM tag`). PostgreSQL's
//! WindowAgg reads its partition's rows into its buffer before it hands the first one on, a Sort its whole input before its
//! first row, and under rule B PostgreSQL reads every arm to its end; so an arm that is a window over a still chain shows
//! nothing the eager fill does not. The chain: each breaker's one producer is read by its consumer alone and is rule B's
//! itself (`fill`: union_arms.rs's source and op test over the ops it runs); a window's partition buffer (decompose
//! window.rs `map`: a Materialize) is read by a pipeline whose first op is the Window; a full sort (its op ends the
//! pipeline that fills it) compares by builtin operators of their default family. Nothing acts: each window function (its aggregate's support functions
//! included: the effects walk's `agg`), argument, FILTER, frame offset and run condition, and the partition and peer
//! comparisons (a builtin operator that is not polymorphic: an array, record or range one runs the element's). The fills
//! are earlier pipelines, so they run ahead of the arms before this one; they act nowhere, so the only thing that can show
//! is which error a statement reports that PostgreSQL fails too (DECISIONS.md 4 (a)). Registers nothing.
use gravity_decompose::ops::shared::sort::{EqKey, SortDesc, SortForm};
use gravity_decompose::ops::window::WindowDesc;
use gravity_decompose::ops::shared::window::FrameBound;
use gravity_decompose::{Op, PipelineNode};
use gravity_program::LifeId;
use mcx::{CollectIn, PgVec};
use types_nodes::Node;

use crate::graph::Gx;

/// Pipeline `p` (a feed: its ops but the Feed, `body`; or a fill: all of them) reading breaker `slot` is rule B's: the
/// slot's one producer is read by `p` alone and is itself rule B's (`fill`: union_arms.rs's test over a pipeline and the
/// ops it runs), and the breaker is a window's partition buffer whose Window is `p`'s first op, or a full sort (a window's
/// order) whose keys compare by builtin operators of their default family; then filters, gates and projections.
pub(crate) fn still(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, slot: u8, body: &[Op<'_>], calm: &dyn Fn(Node<'_>) -> bool, fill: &dyn Fn(&PipelineNode<'_>, &[Op<'_>]) -> bool) -> bool {
    let ps = gx.pipelines();
    let n = gx.graph.pipelines.len();
    let Some(LifeId(q)) = p.desc.consumes_from.first().copied().filter(|_| p.desc.consumes[..] == [slot]) else { return false };
    let q = q as usize;
    if q >= n {
        return false;
    }
    // the fill's rows go to `p` alone
    let readers = (0..n).filter(|&c| ps[c].desc.consumes.iter().any(|&s| gx.life_of(c, s) == Some(LifeId(q as u16)))).count();
    let qops = &ps[q].ops[..];
    let (fill_ops, ops) = match (body.split_first(), qops.split_last()) {
        // the Window first: the slot is its partition buffer (decompose window.rs `map` closes the Materialize the Window's
        // pipeline reads)
        (Some((Op::Window(d), rest)), _) if window(d, calm) => (qops, rest),
        // a full sort: its op ends the pipeline that fills the slot
        (_, Some((Op::Sort(s), before))) if sorted(s) => (before, body),
        _ => return false,
    };
    readers == 1
        && fill(&ps[q], fill_ops)
        && ops.iter().all(|op| match op {
            Op::Filter(x) | Op::Gate(x) => calm(*x),
            Op::Project(l) => l.iter().all(calm),
            _ => false,
        })
}

/// A builtin, non-polymorphic operator (an array, record or range one runs the element's support functions).
fn builtin(opno: u32) -> bool {
    gravity_decompose::effects::builtin_oper(opno).is_some_and(|o| !o.poly)
}

/// Sort `s` reads its whole input and compares only through builtin operators of their default family (`still`).
fn sorted(s: &SortDesc<'_>) -> bool {
    matches!(s.form, SortForm::Runs) && s.keys.iter().all(|k| !k.own_family && builtin(k.opno))
}

/// Window `d` acts nowhere (`still`).
fn window(d: &WindowDesc<'_>, calm: &dyn Fn(Node<'_>) -> bool) -> bool {
    let bound = |b: FrameBound<'_>| match b {
        FrameBound::Preceding(x) | FrameBound::Following(x) => calm(x),
        _ => true,
    };
    let key = |k: &EqKey| builtin(k.opno);
    !d.volatile_args
        && !d.volatile_proj
        && d.partition.iter().chain(d.order.iter()).all(key)
        && bound(d.frame.start)
        && bound(d.frame.end)
        && d.functions.iter().all(|f| calm(f.node) && bound(f.cursor.start) && bound(f.cursor.end))
        && d.run_condition.iter().all(calm)
}

/// material W-INNER (gen window_core-0148 / 0150 / 0151 / 0153): a WindowAgg that a nest's or a nested SubPlan's inner reads
/// straight, its partition buffer filled by pipeline `q`, a member of that owner's rerun set, which runs whole at every
/// re-open (ops/window/rows.rs: each open restarts the object, D2346). PostgreSQL's WindowAgg reads its child at a rescan
/// only as far as the reader's rows need it (spool_tuples), and a reader may stop early (a LIMIT, a run condition, an
/// EXISTS / ANY or semi join's first match). The rows and errors are PostgreSQL's when nothing the fill evaluates can raise
/// or call a VOLATILE function (D449's rule over its filters and projections: leakproof builtins only) and its source is a
/// relation scan (a scan's recheck quals under the same rule) or a full sort's run (PostgreSQL's Sort reads its whole input
/// before its first row, so the sort's own input is read whole by both). Any other source refuses. The window functions'
/// arguments are evaluated per row as PostgreSQL reads them (rows.rs's marked and lazy cells), the run condition and qual
/// by the reader. D3373: a fill that may raise but calls no user or VOLATILE function runs whole there under the unchecked
/// licence (`inner_fill_ahead`); denied (a statement that acts, user code) it refuses by this name.
pub(crate) const W_INNER_LOUD: &str = "a WindowAgg a nest's or SubPlan's inner reads whose fill may raise or calls a VOLATILE function, or reads other than a relation scan or a full sort (PostgreSQL's spool reads its child only as far as the reader needs)";

/// The licence of fill `q`'s run ahead when what it evaluates is pure over the owner's given parameters: it shows
/// nothing (`effects::unobservable`).
pub(crate) fn inner_fill_ok(gx: &Gx<'_, '_>, q: usize) -> Option<gravity_decompose::effects::Licence> {
    // a nest's and a per-row SubPlan's own parameters are values its owner wrote before the set runs (graph.params, F6); an
    // InitPlan's still runs its plan at a first read
    let given: PgVec<'_, i32> = gx.graph.params.iter().map(|x| x.0).collect_in(gx.mcx);
    fill_by(gx, q, &|n| crate::props::pure_given(gx, n, &given[..])).then(|| gravity_decompose::effects::unobservable(gravity_decompose::effects::Effects::NONE)).flatten()
}

/// D3373: a fill `inner_fill_ok` refuses only because what it evaluates may raise: the same source and ops, none calling
/// a user or VOLATILE function or one the side class cannot see (hashjoin.rs `side_calls`, `hidden`). The nest asks the
/// unchecked licence for it (rerun/inner.rs, `Px::ahead_unchecked`, which tags the fill's scan): the whole fill at each
/// re-open reads rows PostgreSQL's spool may never reach (a LIMIT met, a run condition, a first match), so the rows are
/// the same and an error from such a row is the licence's tagged one (`DECISIONS.md` 4 (D) b).
/// That licence comes back.
pub(crate) fn inner_fill_ahead(px: &mut crate::frame::px::Px<'_, '_>, q: usize) -> Option<gravity_decompose::effects::Licence> {
    let gx = px.gx;
    let calls = |n: Node<'_>| crate::effects::visible::hashjoin::side_calls(gx, n) || crate::effects::visible::hashjoin::hidden(gx, n);
    if !fill_by(gx, q, &|n| !calls(n)) {
        return None;
    }
    px.ahead_unchecked(q, gravity_decompose::effects::Effects::RAISES)
}

/// `inner_fill_ok`'s shape of fill pipeline `q`: its filters, projections and recheck quals read by `calm`.
fn fill_by(gx: &Gx<'_, '_>, q: usize, calm: &dyn Fn(Node<'_>) -> bool) -> bool {
    use gravity_decompose::Source;
    let ps = gx.pipelines();
    let Some(p) = ps.get(q).filter(|_| q < ps.len()) else { return false };
    let src = match &p.source {
        Source::Range { .. } => true,
        Source::Index { recheck, .. } | Source::Bitmap { recheck, .. } => recheck.iter().all(calm),
        // a full sort's run (an incremental sort's is runs.incremental: it streams by presorted group, so it refuses)
        Source::Breaker { .. } => matches!(gx.source_kind(q), crate::face::kinds::KindId::RunsSort | crate::face::kinds::KindId::RunsTopkRows),
        _ => false,
    };
    // the ops up to the buffer's end: filters and projections only
    src && p.ops.iter().all(|op| match op {
        Op::Filter(x) => calm(*x),
        Op::Project(l) => l.iter().all(calm),
        _ => false,
    })
}
