//! The nest family's lifetime decisions: what one re-open of a nest inner starts with (`Scope::NestOpen`). PostgreSQL's
//! ExecReScanNestLoop marks the inner's changed parameters, and every node under it resets its per-scan state when it
//! is next pulled (ExecReScan); here the inner's per-open frame words (its position, gate and LIMIT cursor, its steps'
//! counters and walk state) are declared at its re-open (`declare`) and stored at every re-open before anything reads
//! them (`reopen`). Its source's own resets (a kept table's match marks: hashjoin.rs `reset_at_reopen`) and its rerun
//! set's (`Scope::OwnerRun`, stored by rerun/sets.rs before each run) come from their own declarations; a union's
//! counters past its arm marker live for a union run (`OpenAt::Union`), not for the re-open. What the re-open runs
//! before its set does: its LIMIT's count when it may be 0 (`count_before_run`; where it cannot, a count of 0 refuses:
//! `zero_count`) and its set's InitPlans re-armed ahead of its gates (`rearm_before_gates`); the set then runs, before
//! the source opens. A set operation's two fills run again at every re-open only as the set's members (`setop_refilled`).
//! Which ops an inner may hold at all is decided here too (`reopens`, D1018): each one the re-open runs again from its
//! start, as PG's rescan of that node does; any other refuses by rerun/inner.rs INNER_BEYOND_FORMS.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::{Op, PipelineNode};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;
use gravity_program::ArenaRef;

use super::{Declared, Lifetimes, OpenAt, Place, RunResets, Scope};
use crate::frame::open::{Base, OpenWord, OpenWords};

/// A word inner `inner`'s re-open resets: one stored at every open, on the frame's words site.
pub(crate) fn per_open(w: &OpenWord) -> bool {
    w.base == Base::Words && w.scope == OpenAt::Open
}

/// Declares the `per_open` words of `words` at the re-open of instance `at` of inner `inner` (`Scope::NestOpen`: `at` the
/// identity its makers declared under, D3936) and captures them (`Lifetimes::nest_open`); `Err` names a word whose place
/// another declaration holds in another role or scope.
pub(crate) fn declare(lt: &mut Lifetimes, inner: u16, at: u16, words: &OpenWords) -> Result<RunResets, &'static str> {
    let scope = Scope::NestOpen { inner, at };
    for w in words.words.iter().filter(|w| per_open(w)) {
        lt.declare(Declared { what: w.what, scope, place: Place::Word { off: w.off, ty: w.ty, init: w.init } }).map_err(|_| w.what)?;
    }
    Ok(lt.nest_open(inner, at))
}

/// The re-open's words (`declare`'s capture) stored into frame words `words` (params area `params`): `false`, nothing
/// stored, when there is a word and no words site.
#[must_use]
pub(crate) fn reopen(b: &mut FunctionBuilder<'_>, resets: &RunResets, words: Option<Value>, params: Value) -> bool {
    resets.write(b, words, params)
}

/// D1903: inner `ip`'s re-open reads its LIMIT's count before its rerun set runs, when that count may be 0 (inner_limit.rs
/// `may_zero`): PostgreSQL's ExecLimit at a zero count never pulls its child (LIMIT_EMPTY), so the set's members never
/// run. Under a Memoize (`memo`) the hit road keeps the set's run first.
pub(crate) fn count_before_run(ip: &gravity_decompose::PipelineNode<'_>, set: bool, memo: bool) -> bool {
    set && !memo && crate::rerun::inner_limit::may_zero(ip)
}

/// D1903: the re-open runs the inner's rerun set before its steps see a count; PG's ExecLimit at 0 runs nothing (LIMIT_EMPTY).
pub(crate) const ZERO_LIMIT_OVER_SET: &str = "a nest inner's LIMIT 0 over its rerun set (PG runs nothing below a zero count: D1903)";
/// D1917: the re-open evaluates the inner's one-time gates first too; PG never runs one below the LIMIT (earlier in `ip.ops`).
pub(crate) const ZERO_LIMIT_OVER_GATE: &str = "a nest inner's LIMIT 0 over a one-time gate (PG runs nothing below a zero count: D1917)";

/// Where inner `ik`'s (`ip`) re-open cannot read a LIMIT's count before what runs below it, a count of 0 refuses by name:
/// D1903 (the set its re-open runs: rerun/sets.rs `owner_set`, D1337(H)'s probe road too), D1917 (a one-time gate below
/// the LIMIT). Its bounds otherwise any (a computed one, a negative constant or a bind-time word is read at each re-open:
/// step/limit.rs `late`, D1016 row 5 / D1759). D3378: a constant 0 is served as the computed road serves a zero: the
/// re-open reads it before the set runs (`count_before_run`, rerun/inner_limit.rs `may_zero`) and before the one-time gates
/// below it (nest/inner.rs D2228's top-down reads; limit/lower.rs `reopen` answers a constant shut), so the set's members,
/// the gates' quals and the source never run, as PG's ExecLimit at a count of 0 returns at LIMIT_EMPTY without pulling
/// its child (ExecReScanResult only re-arms the gate's check). Under a Memoize the hit road keeps the set's run first, so
/// a constant 0 there still refuses by name.
pub(crate) fn zero_count(gx: &crate::graph::Gx<'_, '_>, ik: usize, ip: &gravity_decompose::PipelineNode<'_>, node: i32) {
    use gravity_decompose::{graph::Bound, Op};
    let memo = crate::ops::memoize::lower::at(ip).is_some();
    for (i, op) in ip.ops.iter().enumerate() {
        let Some((count, _)) = crate::frame::iface::limit_bounds(op) else { continue };
        if matches!(count, Bound::Const(0)) && memo {
            let why = if crate::rerun::sets::owner_set(gx, ik).is_some() { ZERO_LIMIT_OVER_SET } else if ip.ops[..i].iter().any(|o| matches!(o, Op::Gate(_))) { ZERO_LIMIT_OVER_GATE } else { continue };
            crate::refuse!(node, crate::admit::refusal::Reason::Shape { why: crate::admit::refusal::ShapeWhy(why) });
        }
    }
}

/// D1018 (a set operation as a nest inner; PG 18.6 nodeSetOp.c): op `i` of inner `ik` is a hashed or sorted INTERSECT /
/// EXCEPT [ALL] emit (its first op, over its left group, the inner's source, with its right group consumed) whose two
/// group fills the re-open runs again: both are members of the inner's rerun set. ExecReScanSetOp rebuilds the table from
/// both children at a rescan that changes a param either reads, each re-read whole, and the set re-runs both fills at
/// every re-open in program order; the setop.setop protocol files PG's copy counts after the later of them (exec
/// ops/setop/proto.rs `at`, its after_pipeline hook runs in a re-run too) and its empty-left form skips the right fill as
/// PG never reads it. A fill outside the set (read once, where PG re-reads it at every rebuild; or filled before the other
/// side's re-run, so the counts would be filed early) is no form the re-open serves (rerun/inner.rs `INNER_BEYOND_FORMS`).
pub(crate) fn setop_refilled(gx: &crate::graph::Gx<'_, '_>, ik: usize, ip: &gravity_decompose::PipelineNode<'_>, i: usize) -> bool {
    use gravity_decompose::ops::shared::setop::SetDesc;
    let Some(gravity_decompose::Op::SetOp(SetDesc::Set { left, right, .. })) = ip.ops.get(i) else { return false };
    let set = crate::rerun::sets::owner_set(gx, ik).and_then(|(n, p)| gx.graph.reruns.iter().find(|r| r.node_id == n && r.plan_id == p));
    let member = |s: u8| gx.life_of(ik, s).is_some_and(|l| set.is_some_and(|r| r.pipelines.contains(&l.0)));
    i == 0 && member(*left) && member(*right)
}

/// D4101 (S3 rkeep ra-hx3/g7-setop-lat-exists): op `i` of inner `ik` (embedded by op `j` of pipeline `k`) is a set
/// operation's emit over two group fills no rerun set re-runs (the inner has no set: neither side reads a parameter the
/// owner sets), under a quiet owner (effects.rs `owner_quiet`). PostgreSQL 18.6's ExecReScanSetOp keeps a
/// hashed table no changed parameter reaches and re-reads it (a sorted one re-sorts the same rows). Here both fills run once
/// ahead of the owner, the setop.setop protocol files the copy counts once after the later of them, and the left group's
/// emit is gathered again at every re-open (rerun/inner_once.rs `kept_emit`, which the inner's source must also pass).
pub(crate) fn setop_kept(gx: &crate::graph::Gx<'_, '_>, (k, j): (usize, usize), ik: usize, ip: &gravity_decompose::PipelineNode<'_>, i: usize) -> bool {
    use gravity_decompose::ops::shared::setop::SetDesc;
    let Some(gravity_decompose::Op::SetOp(SetDesc::Set { left, right, .. })) = ip.ops.get(i) else { return false };
    let ps = gx.pipelines();
    let kept = |s: u8| gx.life_of(ik, s).is_some_and(|l| (l.0 as usize) < ps.len() && !crate::rerun::sets::in_owner_set(gx, l.0 as usize)) && ps.consumed_at(ik, s).is_some();
    i == 0
        && matches!(ip.source, gravity_decompose::Source::Breaker { slot } if slot == *left)
        && crate::rerun::sets::owner_set(gx, ik).is_none()
        && crate::effects::owner_quiet(gx, k, j)
        && kept(*left)
        && kept(*right)
}

/// D2611: a gated re-open (a one-time gate, or `count_before_run`) re-arms its rerun set's InitPlans before the gates read
/// one (rerun/sets.rs `holds_init`), as PostgreSQL re-arms them at the rescan, ahead of the Result's first call; a failed
/// gate then skips the set's run and the next re-open still reads its own values.
pub(crate) fn rearm_before_gates(gx: &crate::graph::Gx<'_, '_>, set: Option<usize>, gated: bool) -> bool {
    gated && set.is_some_and(|s| crate::rerun::sets::holds_init(gx, s))
}

/// What a nest inner holds besides its ops, as rerun/inner.rs `make_through` reads it, for `reopens`.
pub(crate) struct InnerForm {
    /// The Memoize op the inner holds (nest.memo), unless it is read through (`through`).
    pub memo: Option<usize>,
    /// D3776: a Memoize read through (inner.rs `plain_memo`).
    pub through: Option<usize>,
    /// D1483: a MergeAppend whose arms the owner's rerun set runs again at every re-open.
    pub merge: bool,
    /// A union the re-open streams (S2b), the set refills (S2r), filled once and kept (S2k, D3782) or filled once ahead of
    /// the owner (D3773).
    pub arms: bool,
    /// Material W-INNER: a WindowAgg as the inner (D2346).
    pub win: bool,
    /// D3733: the inner is a SubPlan's.
    pub sub: bool,
}

/// D1018 (fail closed): every op of nest inner `ik` (`ip`, embedded by op `j` of pipeline `k`) is one its re-open runs
/// again from its start, as PostgreSQL's rescan runs that node again: filters, projections, a one-time gate, SubPlans, set
/// functions, nested nests (their own kinds); a LIMIT below any Memoize (row A, V4R-496); a RIGHT SEMI / ANTI drain over a
/// kept table first (join#212 / #213); a MergeAppend or a union first (`form`); a WindowAgg below any Memoize; a merge join
/// over a run filled once (psql#479 / #480); a probe of the inner's own (rerun/inner_probe.rs `ok`); a set operation's
/// refilled fill (`setop_refilled`); a streamed group (inner.rs `group_run`); an EXISTS's row lock (epq sub_lock.rs
/// `inner_ok`). Under a Memoize, its row-at-a-time ops only (a replayed row runs those past it). A keeping op or another
/// join holds rows across re-opens where PG's rescan would start again: refused by the caller.
pub(crate) fn reopens(gx: &crate::graph::Gx<'_, '_>, arena: ArenaRef, (k, j): (usize, usize), ik: usize, ip: &PipelineNode<'_>, form: &InnerForm) -> bool {
    let m = form.memo;
    ip.ops.iter().enumerate().all(|(i, op)| match m {
        _ if Some(i) == form.through => true,
        None if i == 0 && matches!(op, Op::Join(JoinDesc::Drain { .. })) => true,
        _ if crate::frame::iface::limit_bounds(op).is_some() => m.is_none_or(|m| i < m),
        _ if matches!(op, Op::Append(gravity_decompose::ops::shared::append::AppendDesc::Merge { .. })) => i == 0 && form.merge,
        // under a Memoize too: a miss streams the arms into the fill as PG's ExecMemoize pulls its Append, a hit opens none
        _ if matches!(op, Op::Append(_)) => i == 0 && form.arms,
        _ if matches!(op, Op::Window(_)) => form.win && m.is_none_or(|m| i < m),
        None if matches!(op, Op::Join(JoinDesc::Merge { .. })) => crate::ops::mergejoin::nest_inner::ok(gx, ik, i) || crate::ops::mergejoin::nest_refill::ok(gx, ik, i),
        Some(m) => i == m || crate::ops::memoize::lower::row_op(op, i > m),
        None => {
            matches!(op, Op::Filter(_) | Op::Project(_) | Op::Gate(_) | Op::SubPlan(_) | Op::Srf(_) | Op::Join(JoinDesc::Nest { .. }))
                || crate::rerun::inner_probe::ok(gx, arena, ik, ip, i)
                || setop_refilled(gx, ik, ip, i)
                || setop_kept(gx, (k, j), ik, ip, i)
                || crate::rerun::inner::group_run(ip, i, form.sub)
                || crate::rerun::epq::sub_lock::inner_ok(gx, (k, j), ik, i)
        }
    })
}
