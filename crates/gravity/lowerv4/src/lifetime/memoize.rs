//! memoize's lifetime decisions, which the family asks here: whether a Memoize's cache may be purged between two opens of
//! its nest inner. PostgreSQL keeps one cache per execution across the nest's rescans and empties it only when a parameter
//! outside the cache keys changed (ExecReScanMemoize, nodeMemoize.c: `bms_nonempty_difference(outerPlan->chgParam,
//! node->keyparamids)` calls cache_purge_all). A lowered Memoize keeps a purge word only where a re-run can change such a
//! parameter (`rerun`); the owners arm it at every open of the inner's pipeline and the fill purges at the next lookup.
//! ops/memoize/lower.rs `make_parts` refuses by name, on these answers, the forms whose purges would not fall where
//! PostgreSQL's do.
use gravity_decompose::graph::ParamOwner;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;

use crate::graph::Gx;

/// Pipeline `k`, the nest inner a Memoize sits in, is re-opened by a re-run: an inner embedded in another op, or a member
/// of a rerun set. Only there can a parameter outside the keys change between two opens, so only there is a purge word kept.
pub(crate) fn rerun(gx: &Gx<'_, '_>, k: usize) -> bool {
    k >= gx.graph.pipelines.len() || gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&p| p as usize == k))
}

/// The Memoize's child reads a parameter of its own nest (plan node `nest`) outside the keys (`nonkey`): PostgreSQL then
/// purges at every rescan of the nest.
pub(crate) fn own_param(gx: &Gx<'_, '_>, nonkey: &[i32], nest: i32) -> bool {
    nonkey.iter().any(|p| gx.graph.params.iter().any(|(id, r)| id == p && r.owner == ParamOwner::Nest { node: nest }))
}

/// Whether the purge word, armed at every open of pipeline `k`, purges where PG's does. PG marks a param
/// changed at every row of its owner (ExecNestLoop, ExecScanSubPlan: an equal value too) and the memo purges at its next
/// rescan; `k` opens once per row of each re-opener (the op embedding inner `k`, the owner of every rerun set holding
/// it), so each must set a param of `nonkey` (the child's outside the keys). Params set further out change only where a
/// re-opener's row re-opens `k` too, so their purges fall on those opens.
pub(crate) fn purge_exact(gx: &Gx<'_, '_>, k: usize, nonkey: &[i32]) -> bool {
    let g = &gx.graph;
    let sets = |o: ParamOwner| g.params.iter().any(|(id, r)| r.owner == o && nonkey.contains(id));
    let embed = k.checked_sub(g.pipelines.len()).map(|i| {
        let ((ok, oj), _) = gx.pipelines().inners()[i];
        match gx.pipelines()[ok].ops.get(oj) {
            Some(Op::Join(JoinDesc::Nest { facts, .. })) => Some(ParamOwner::Nest { node: facts.node }),
            Some(Op::SubPlan(s)) => Some(ParamOwner::SubPlan { plan_id: s.plan_id }),
            _ => None,
        }
    });
    let sets_of = g.reruns.iter().filter(|r| r.pipelines.iter().any(|&p| p as usize == k));
    let owners = sets_of.map(|r| Some(if r.plan_id == 0 { ParamOwner::Nest { node: r.node_id } } else { ParamOwner::SubPlan { plan_id: r.plan_id } }));
    let mut all = embed.into_iter().chain(owners).peekable();
    all.peek().is_some() && all.all(|o| o.is_some_and(sets))
}

/// The inner's pipeline (`ops`) holds a one-time gate beside the Memoize at `m`.
pub(crate) fn gated(ops: &[Op<'_>], m: usize) -> bool {
    ops[..m].iter().chain(ops[m + 1..].iter()).any(|op| matches!(op, Op::Gate(_)))
}
