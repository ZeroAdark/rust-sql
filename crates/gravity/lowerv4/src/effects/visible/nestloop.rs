//! The effects home's visible half for the nest family's rerun strategies: may the user see what a nest inner's re-open
//! skips, repeats or reorders?
//! - A union's arms (from rerun/union_arms.rs): an arm that raises nothing and calls no user or VOLATILE function shows
//!   neither whether PostgreSQL ran it nor how often (`quiet_arm`, `quiet_arm_in`; their expressions `quiet_in`; a
//!   re-open's pruning steps `quiet_prune`; a hash build over the union `quiet_build`), and an arm drained into a root
//!   INSERT may raise but must not act (`acts`).
//! - A union a parameter-free nest fills once (from rerun/union_arms.rs, S2k): its fill ahead of PostgreSQL's first rescan
//!   shows nothing when no arm, nor anything feeding one, raises or calls (`kept_quiet`).
//! - A nest inner's hash probe (from rerun/inner_probe.rs): its prefix, build side and bitmap keys are read through
//!   `quiet_ops`, `probe_quiet` and `bitmap_quiet`.
//! - A computed key's re-open (from rerun/params_key.rs): whether evaluating a key may raise or call a VOLATILE function
//!   (`loud`, the home's `Fx::loud`; a node by itself: `Fx::loud_here`) and whether an eager SubPlan step may (`loud_sub`).
//! The strategies keep their call lines; rerun/union_arms.rs re-exports its five tests under its own path.
use gravity_decompose::effects;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::append::{AppendDesc, PruneDesc};
use gravity_decompose::{Op, PipelineNode, Source};
use gravity_program::ArenaRef;
use types_nodes::Node;

use super::material::quiet_nest;
use crate::effects::raise_free::Reading;
use crate::graph::Gx;

/// D3782 (S2k): no arm of a once-filled union, nor anything feeding one, can raise or call a user or VOLATILE function
/// (build_class `side` Safe), so its fill ahead of PostgreSQL's first rescan, or under an outer that hands on no row,
/// shows nothing.
pub(crate) fn kept_quiet(gx: &Gx<'_, '_>, feeds: &[usize]) -> bool {
    use crate::ops::hashjoin::build_class::{side, Side};
    feeds.iter().all(|&f| side(gx, f) == Side::Safe)
}

/// `n` acts by the effects home's reader (a VOLATILE call or one the catalog cannot answer, a SubPlan, a node the walk
/// does not know), or holds a domain coercion (its CHECK may call anything: fail closed).
pub(crate) fn acts(gx: &Gx<'_, '_>, arena: ArenaRef, n: Node<'_>) -> bool {
    let fx = gx.fx(n);
    fx.domain_cast() || fx.acts_plain(&effects::Cx::without_stmt(gx.facts, arena))
}

/// Arm `p` (its ops end in the Feed) raises nothing and calls no user or VOLATILE function: a scan, Result or VALUES source
/// under filters, gates and projections of quiet expressions.
pub(crate) fn quiet_arm(gx: &Gx<'_, '_>, p: &PipelineNode<'_>) -> bool {
    quiet_arm_in(gx, p, false, false)
}

/// `quiet_arm`; `exec`: an executor parameter a nest or per-row SubPlan binds (`PipelineGraph::params`) reads quietly too
/// (the owner wrote it before the re-open), and an index key's operator is a builtin one; a bitmap heap arm passes here
/// only (a pruned nest inner's bitmap leaves), by the index arm's rule over its leaves' keys. `nests`: a nest in the arm
/// passes when it is quiet (`quiet_nest`: one heap or index scan inner, its parameters and join qual quiet; e.g. UNION ALL
/// arms each joined to another table by index, per outer row).
pub(crate) fn quiet_arm_in(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, exec: bool, nests: bool) -> bool {
    quiet_arm_by(gx, p, exec, nests, &|_| false)
}

/// D3614 (rerun/union_arms.rs, S2q's refill of a union a rerun set prunes at every run): PARAM_EXEC `id` is a bound word
/// (a nest's or a per-row SubPlan's, `PipelineGraph::params`) or an unseen InitPlan's output (pgrc.rs `init_unseen`:
/// running it at the set's run, where PostgreSQL may never prune or read it, shows nothing).
pub(crate) fn refill_bound(gx: &Gx<'_, '_>, id: i32) -> bool {
    gx.graph.params.iter().any(|p| p.0 == id) || super::pgrc::init_unseen(gx, id)
}

/// D3614 (lifetime/material.rs `spool_fill`'s D1107 refill): fill pipeline `k` is a settled union's emit, its Union, quiet
/// filters and projections and its Spool, whose feeds are quiet arms over bound words and unseen InitPlans' outputs
/// (`quiet_arm_by` with `refill_bound`) and whose run-time pruning is quiet over the same. The reader's F5 counts such a
/// read as one that may raise (decompose effects read.rs: an unbound PARAM_EXEC); for the set's refill it shows nothing.
pub(crate) fn refill_union_quiet(gx: &Gx<'_, '_>, k: usize) -> bool {
    let ps = gx.pipelines();
    let Some(p) = ps.get(k).filter(|_| k < gx.graph.pipelines.len()) else { return false };
    let Some((Op::Append(AppendDesc::Union { slot, prune, .. }), rest)) = p.ops.split_first() else { return false };
    let Some((Op::Join(JoinDesc::Spool), body)) = rest.split_last() else { return false };
    let q = |n: Node<'_>| gx.fx(n).quiet_in(&Reading::union_arms(&|id| refill_bound(gx, id)));
    let feeds = crate::rerun::union_join_arms::feeds(gx, k, *slot);
    !feeds.is_empty()
        && quiet_ops(body, &q)
        && feeds.iter().all(|&f| quiet_arm_by(gx, &ps[f], true, true, &|id| super::pgrc::init_unseen(gx, id)))
        && prune.as_ref().is_none_or(|d| quiet_prune_bound(gx, d, &|id| refill_bound(gx, id)))
}

/// D3614 (rerun/union_arms.rs `refills_whole`): a nest in a refilled union's emit `e`, INNER or LEFT, whose inner reads a
/// plan Material over another refilled union (`refill_union_quiet`), its parameters, join qual and inner ops quiet over
/// bound words and unseen InitPlans' outputs (`refill_bound`): PostgreSQL's Nested Loop reads every outer row and rescans
/// the Material per row; the set's run fills both records before the emit reads them, which shows nothing since neither
/// fill can. The Material's fill pipeline, for the caller's form test, else none.
pub(crate) fn refill_nest(gx: &Gx<'_, '_>, e: usize, op: &Op<'_>) -> Option<usize> {
    use types_nodes::jointype::JoinType;
    let Op::Join(JoinDesc::Nest { jointype, params, qual, inner, .. }) = op else { return None };
    let Source::Breaker { slot } = inner.source else { return None };
    let (ps, d) = (gx.pipelines(), &inner.desc);
    let life = d.consumes.iter().position(|&s| s == slot).and_then(|j| d.consumes_from.get(j).copied());
    let f = life.map(|l| l.0 as usize).or_else(|| (0..e).rev().find(|&x| ps[x].desc.produces() == Some(slot)))?;
    let q = |n: Node<'_>| gx.fx(n).quiet_in(&Reading::union_arms(&|id| refill_bound(gx, id)));
    (matches!(jointype, JoinType::JOIN_INNER | JoinType::JOIN_LEFT)
        && f < e
        && refill_union_quiet(gx, f)
        && d.consumes.iter().all(|&s| s == slot)
        && params.iter().all(|p| p.as_nest_loop_param().is_some_and(|p| q(p.paramval)))
        && qual.iter().all(q)
        && quiet_ops(&inner.ops, &q))
        .then_some(f)
}

/// `quiet_arm_in`, the PARAM_EXEC ids `more` names read as bound words too when `exec` (D3614: an unseen InitPlan's
/// output that a rerun set's run re-arms, pgrc.rs `init_unseen`: the arm's read may run it, which shows nothing).
pub(crate) fn quiet_arm_by(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, exec: bool, nests: bool, more: &dyn Fn(i32) -> bool) -> bool {
    let q = |n: Node<'_>| quiet_more(gx, n, exec, more);
    arm_by(gx, p, exec, nests, &q, &q)
}

/// `quiet_arm_in` with the arm's filters, gates and projections read by `body` (D1483, rerun/union_arms.rs
/// `refills_whole`: under an emit that reads every arm whole, they may raise); the source's keys and recheck stay quiet.
pub(crate) fn arm_in(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, exec: bool, nests: bool, body: &dyn Fn(Node<'_>) -> bool) -> bool {
    arm_by(gx, p, exec, nests, &|n| quiet_in(gx, n, exec), body)
}

/// `quiet_in` with the PARAM_EXEC ids `more` names read as bound words too when `exec`.
fn quiet_more(gx: &Gx<'_, '_>, n: Node<'_>, exec: bool, more: &dyn Fn(i32) -> bool) -> bool {
    gx.fx(n).quiet_in(&Reading::union_arms(&|id| exec && (gx.graph.params.iter().any(|p| p.0 == id) || more(id))))
}

/// `arm_in` with the source's keys and recheck read by `q`.
fn arm_by(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, exec: bool, nests: bool, q: &dyn Fn(Node<'_>) -> bool, body: &dyn Fn(Node<'_>) -> bool) -> bool {
    let src = match &p.source {
        Source::Range { .. } | Source::Const => true,
        Source::Index { keys, recheck, .. } => keys.iter().all(|x| q(x.value) && (!exec || x.opno == 0 || gravity_decompose::effects::builtin_oper(x.opno).is_some())) && recheck.iter().all(q),
        Source::Bitmap { plan, recheck, .. } if exec => crate::rerun::union_join_arms::leaves(plan, &|x| q(x.value) && (x.opno == 0 || gravity_decompose::effects::builtin_oper(x.opno).is_some())) && recheck.iter().all(q),
        Source::Values { lists, .. } => lists.iter().all(|l| l.as_list().is_some_and(|l| l.iter().all(q))),
        _ => false,
    };
    let Some((Op::Append(AppendDesc::Feed { .. }), ops)) = p.ops.split_last() else { return false };
    src && ops.iter().all(|op| match op {
        Op::Filter(n) | Op::Gate(n) => body(*n),
        Op::Project(l) => l.iter().all(body),
        Op::Join(JoinDesc::Nest { params, qual, inner, .. }) if nests => quiet_nest(gx, None, params, qual, inner),
        // a constant row bound under a window that reads only the union's first rows (ops/limit `const_bound`)
        _ => crate::ops::limit::lower::const_bound(op),
    })
}

/// A hash build ending the settled union's emit `emit` (rerun/union_arms.rs, S2r / S2q) that shows nothing when PostgreSQL
/// skips it (an empty probe side) or builds it again: its keys are quiet and every probe of its table hashes through
/// built-in support (GUARD-G1 `support`).
pub(crate) fn quiet_build(gx: &Gx<'_, '_>, emit: usize, keys: &types_nodes::NodeList<'_>) -> bool {
    let ps = gx.pipelines();
    let inner = ps.inners().into_iter().enumerate().map(|(i, (_, p))| (ps.len() + i, p));
    let mut probes = (0..ps.len()).map(|q| (q, &ps[q])).chain(inner).flat_map(|(q, p)| p.ops.iter().filter_map(move |op| match op {
        Op::Join(JoinDesc::Probe { slot, clauses, .. }) if gx.life_of(q, *slot).is_some_and(|l| l.0 as usize == emit) => Some(*clauses),
        _ => None,
    }));
    let builtin = |clauses: &types_nodes::NodeList<'_>| clauses.iter().all(|c| gx.fx(c).hash_builtin());
    keys.iter().all(|n| quiet_in(gx, n, true)) && probes.next().is_some_and(builtin) && probes.all(builtin)
}

/// D2013: every call PostgreSQL's run-time pruning makes per re-open is quiet: each step's key expressions, and its support
/// functions builtin ones of a leaf type (not the array, record, range and multirange ones, which run the element type's).
pub(crate) fn quiet_prune(gx: &Gx<'_, '_>, d: &PruneDesc<'_>) -> bool {
    quiet_prune_by(d, &|e| quiet_in(gx, e, true))
}

/// `quiet_prune` with the PARAM_EXEC ids `bound` names read as bound words (D3297, rerun/spool/form.rs `union_pull`: the
/// raise-free walk's `union_arms` reading over fixed, unseen InitPlans' parameters).
pub(crate) fn quiet_prune_bound(gx: &Gx<'_, '_>, d: &PruneDesc<'_>, bound: &dyn Fn(i32) -> bool) -> bool {
    quiet_prune_by(d, &|e| gx.fx(e).quiet_in(&Reading::union_arms(bound)))
}

/// `quiet_prune` with the steps' key expressions read by `q`. A step's support function (a btree FUNCTION 1 or a hash
/// FUNCTION 2: partprune.c gen_prune_steps_from_opexps) is quiet when it is built in and no DISPATCH function of the one
/// table (`types_core::quiet::dispatch`): of those, only btarraycmp, btrecordcmp, range_cmp, multirange_cmp and the four
/// extended hashes take two arguments and return integer or bigint as CREATE OPERATOR CLASS requires of either
/// (opclasscmds.c assignProcTypes), and they run the element, field or subtype's functions.
fn quiet_prune_by(d: &PruneDesc<'_>, q: &dyn Fn(Node<'_>) -> bool) -> bool {
    let op = |s: Node<'_>| s.as_partition_prune_step_op().is_none_or(|o| o.exprs.iter().all(q) && o.cmpfns.iter().all(|f| gravity_decompose::effects::builtin_func(f).is_some() && types_core::quiet::dispatch(f).is_none()));
    d.info.prune_infos.iter().flat_map(|l| l.as_list().into_iter().flat_map(|l| l.iter())).all(|r| r.as_partitioned_rel_prune_info().is_some_and(|r| r.initial_pruning_steps.iter().chain(r.exec_pruning_steps.iter()).all(op)))
}

/// `n` raises nothing on any input and calls nothing with an effect: the raise-free walk's shared leaf set (raise_free.rs
/// `leaf` and `own_call`: columns, constants, extern parameters, COLLATE, ROW(), a row conversion, CASE, COALESCE, AND / OR /
/// NOT, IS [NOT] NULL / TRUE, ..), builtin calls the effects home calls quiet, an integer modulus or division by a constant
/// it cannot raise on (`quiet_args`, D567: `id % 9 = 0` in an arm, D1486; both keys hold the divisor's class, effects
/// `divisor_class`), and a list of such (an implicit AND).
pub(crate) fn quiet_in(gx: &Gx<'_, '_>, n: Node<'_>, exec: bool) -> bool {
    // the effects home's one raise-free walk under its `union_arms` reading; an implicit-AND list (a Result's one-time
    // filter: decompose `map_result`'s Gate holds resconstantqual whole) is quiet when every member is (each arm a Result
    // gated on the outer row over a bitmap walk, say)
    gx.fx(n).quiet_in(&Reading::union_arms(&|id| exec && gx.graph.params.iter().any(|p| p.0 == id)))
}

/// A bitmap heap scan whose index keys (every leaf of its AND / OR tree) and recheck quals are quiet (PG keeps
/// a one-batch Hash over it as over a heap or index scan; its bitmap is read before any row, as an index scan's keys are).
pub(crate) fn bitmap_quiet(plan: &gravity_decompose::ops::shared::scan::BitmapPlan<'_>, recheck: &types_nodes::list::NodeList<'_>, q: &dyn Fn(Node<'_>) -> bool) -> bool {
    crate::effects::bitmap_all(plan, &|_, keys, _| keys.iter().all(|k| q(k.value))) && recheck.iter().all(q)
}

/// A probe whose hash keys, hash clauses and join qual are quiet (the probe op itself raises nothing and calls nothing).
pub(crate) fn probe_quiet(op: &Op<'_>, q: &dyn Fn(Node<'_>) -> bool) -> bool {
    let Op::Join(JoinDesc::Probe { keys, clauses, qual, catalog: false, .. }) = op else { return false };
    keys.iter().chain(clauses.iter()).chain(qual.iter()).all(q)
}

pub(crate) fn quiet_ops(ops: &[Op<'_>], q: &dyn Fn(Node<'_>) -> bool) -> bool {
    ops.iter().all(|op| match op {
        Op::Filter(n) | Op::Gate(n) => q(*n),
        Op::Project(l) => l.iter().all(q),
        _ => false,
    })
}

/// Whether evaluating `n` may raise or call a VOLATILE function anywhere in it (the effects home's answer, `Fx::loud`: each
/// node read by itself, `Fx::loud_here`, a list's members; an unknown node counts).
pub(crate) fn loud(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, n: Node<'_>) -> bool {
    gx.fx(n).loud(p)
}

/// D1911: a SubPlan its step runs eagerly (not conditional: nest/subplan.rs `lazy`) that may raise or call a VOLATILE function.
pub(crate) fn loud_sub(d: &gravity_decompose::ops::shared::subplan::SubPlanDesc<'_>) -> bool {
    !d.conditional && crate::effects::fx::step_loud(d)
}
