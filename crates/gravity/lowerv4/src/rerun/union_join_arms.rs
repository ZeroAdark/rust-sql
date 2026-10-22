//! A settled union's arm that is a hash join of quiet scans (a partitionwise join's child; W3-D1486-NONFAM row 1, a helper
//! of nest/union_arms.rs, whose `quiet_arm` rejects such an arm by its probe op alone). The arm's own pipeline and every
//! build it probes (recursively) read a heap scan, a Result or a VALUES list under filters, gates, projections, hash build
//! keys and inner / left / semi / anti probes, and every expression on the way is quiet (union_arms.rs `quiet_in`, the
//! existing rule: no raise, no user or VOLATILE call), so PostgreSQL's runs of it and its builds (read to the end into the
//! union's hash, or skipped for an empty outer) show nothing; the eager fill runs them all, in arm order. `arms` decides
//! D1486 over every arm first; then a union with a joined arm refuses by GUARD-G1's name when a probe's hash support is not
//! built in (guard/user_fn.rs `USER_FN`, D2113). The NO-WIDEN names left at their root fixes (D2178 / D2179, D2180, D2197
//! and D2213; D2194 / D2147 / D2201 at the lazy-build root fix,
//! with D2325 fixed). Row 2 (W3-D1486-TAIL, V4R-574): a bitmap heap scan arm of quiet keys and recheck (`bitmap`)
//! passes too (two one-index bitmap scans over one index serve since D2207's fix: each rechecks with its own qual, found
//! by its node id). Registers nothing. Owner M8-NEST.
use gravity_abi::breaker::BreakerKind;
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::catalog::expr_type::type_of;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::scan::BitmapPlan;
use gravity_decompose::graph::IndexKey;
use gravity_decompose::{Op, Source};
use gravity_program::LifeId;
use types_nodes::jointype::JoinType;
use types_nodes::Node;
use mcx::{CollectIn, GrowIn, PgVec};

use crate::frame::iface;
use crate::graph::Gx;
use crate::effects::{support, Key, KeyUse, USER_FN};
use crate::admit::refusal::Reason;

/// How deep a build may itself probe (a three-way partitionwise join's child is two).
const DEPTH: u8 = 4;

/// The feeds of the settled union whose emit is pipeline `emit` (its Union op reads `slot`), in arm order: the pipelines
/// feeding `slot` after the slot's previous emit (a slot is reused after its life).
pub(crate) fn feeds(gx: &Gx<'_, '_>, emit: usize, slot: u8) -> Vec<usize> {
    let ps = gx.pipelines();
    let reads = |q: usize| crate::rerun::union_arms::union_of(&ps[q]).is_some_and(|u| u.0 == slot);
    let from = (0..emit).rev().find(|&q| reads(q)).map_or(0, |q| q + 1);
    (from..emit).filter(|&q| crate::rerun::union_arms::feed_of(&ps[q]) == Some(slot)).collect() // alloc: Program
}

/// D3897 (build_class.rs `feed`, D3893's `arms_at`): every feed of the settled union whose last arm is pipeline `last`
/// (the life a record reader's `life_of` names: its emit, or a nest or SubPlan inner embedded in a later pipeline), in arm
/// order: the pipelines up to `last` whose Feed writes `slot`, back to the slot's previous reader (a slot is reused after
/// its life; a record read twice keeps its arms before the first reader, so this counts from the life, not the reader).
pub(crate) fn arms_upto<'g>(gx: &Gx<'g, '_>, last: usize, slot: u8) -> PgVec<'g, usize> {
    let ps = gx.pipelines();
    let feeds = |q: usize| crate::rerun::union_arms::feed_of(&ps[q]) == Some(slot);
    let reads = |q: usize| {
        !feeds(q) && (ps[q].desc.consumes.contains(&slot) || matches!(ps[q].source, gravity_decompose::Source::Breaker { slot: s } if s == slot) || crate::rerun::union_arms::union_of(&ps[q]).is_some_and(|u| u.0 == slot))
    };
    let from = (0..last).rev().find(|&q| reads(q)).map_or(0, |q| q + 1);
    (from..=last).filter(|&q| feeds(q)).collect_in(gx.mcx)
}

impl Gx<'_, '_> {
    /// Every feed of the settled union pipeline `p` emits (none when `p` is no union's emit): join/build_class.rs `feed`
    /// reads them all, not only the emit's nearest producer, so a union's side is every arm's.
    pub(crate) fn union_feeds(&self, p: usize) -> Vec<usize> {
        crate::rerun::union_arms::union_of(&self.pipelines()[p]).map_or_else(Vec::new, |u| feeds(self, p, u.0))
    }
}

/// D1486's arms rule for a union PostgreSQL may not read to its end (union_arms.rs admit): every feed is `quiet_arm`,
/// `merged` (a MergeAppend of quiet arms, no probe), `joined` or `bitmap`, decided over all first (design review r1 F2: a
/// loud arm keeps D1486's name whatever else the statement holds). A union with a joined or bitmap arm then refuses by
/// USER_FN when a probe's hash support is not built in.
pub(crate) fn arms(gx: &Gx<'_, '_>, feeds: &[usize], node: i32) -> bool {
    let Some(joins) = arms_take(gx, feeds) else { return false };
    if !joins.iter().all(|&f| hashes(gx, f, 0)) {
        crate::refuse!(node, Reason::Shape { why: USER_FN });
    }
    true
}

/// `arms`' decision without its USER_FN refusal: the feeds that are neither `quiet_arm` nor `merged`, when each is `joined`
/// or `bitmap` (the rule takes the union), else none (union_arms.rs `arms_read` asks it).
pub(crate) fn arms_take<'g>(gx: &Gx<'g, '_>, feeds: &[usize]) -> Option<PgVec<'g, usize>> {
    let ps = gx.pipelines();
    let joins: PgVec<'g, usize> = feeds.iter().copied().filter(|&f| !crate::rerun::union_arms::quiet_arm(gx, &ps[f]) && !merged(gx, f)).collect_in(gx.mcx);
    joins.iter().all(|&f| joined(gx, f) || bitmap(gx, f)).then_some(joins)
}

/// Feed `f` is a quiet bitmap arm (row 2): a bitmap heap scan whose every leaf key has a builtin operator and a quiet value
/// and whose recheck is quiet, under quiet filters, gates and projections (union_arms.rs `quiet_arm_in`'s Index arm over the
/// tree's leaves, through its `quiet_in`). PostgreSQL's Hash reads it to its end or never; nothing it runs shows (a user
/// opclass's support function under a builtin operator is the deferred family: D2063).
pub(crate) fn bitmap(gx: &Gx<'_, '_>, f: usize) -> bool {
    let p = &gx.pipelines()[f];
    let quiet = |n: Node<'_>| crate::rerun::union_arms::quiet_in(gx, n, false);
    let Source::Bitmap { plan, recheck, .. } = &p.source else { return false };
    let Some((Op::Append(AppendDesc::Feed { .. }), body)) = p.ops.split_last() else { return false };
    let key = |k: &IndexKey<'_>| (k.opno == 0 || gravity_decompose::effects::builtin_oper(k.opno).is_some()) && quiet(k.value);
    leaves(plan, &key) && recheck.iter().all(quiet) && p.desc.consumes.is_empty() && body.iter().all(|op| match op {
        Op::Filter(n) | Op::Gate(n) => quiet(*n),
        Op::Project(l) => l.iter().all(quiet),
        _ => false,
    })
}

/// D1486 (material, foreign_key#1393 / #1394: ATTACH PARTITION's RI validation, a Merge Right Anti Join over an ordered Append
/// of MergeAppends of index only scans): feed `f` is a MergeAppend's reader (decompose map_merge_append: every child feeds
/// one Sort slot, the reader's first op the Merge) with no pruning, under quiet filters, gates and projections; each child
/// is a `quiet_arm` and each key a builtin scalar order (append/merge_admit.rs `builtin_keys`). PostgreSQL's merge reads
/// each child's first row at its first fetch and the rest as it merges, and may stop early; with nothing in a child or a
/// key compare able to raise or call, which rows it read shows nowhere, so the eager fill runs it whole, in arm order.
pub(crate) fn merged(gx: &Gx<'_, '_>, f: usize) -> bool {
    let ps = gx.pipelines();
    let p = &ps[f];
    let quiet = |n: Node<'_>| crate::rerun::union_arms::quiet_in(gx, n, false);
    let Some((Op::Append(AppendDesc::Feed { .. }), body)) = p.ops.split_last() else { return false };
    let Some((Op::Append(AppendDesc::Merge { prune: None, .. }), rest)) = body.split_first() else { return false };
    let Some(m) = crate::ops::append::merge_admit::Merge::of_reader(gx, f) else { return false };
    matches!(p.source, Source::Breaker { slot } if slot == m.slot)
        && rest.iter().all(|op| match op {
            Op::Filter(n) | Op::Gate(n) => quiet(*n),
            Op::Project(l) => l.iter().all(quiet),
            _ => false,
        })
        && m.arms.iter().all(|&a| crate::rerun::union_arms::quiet_arm(gx, &ps[a]))
        && crate::ops::append::merge_admit::builtin_keys(gx, &m)
}

/// Every leaf key of bitmap tree `p` passes `ok`.
pub(crate) fn leaves(p: &BitmapPlan<'_>, ok: &dyn Fn(&IndexKey<'_>) -> bool) -> bool {
    crate::stack_check();
    match p {
        BitmapPlan::Index { keys, .. } => keys.iter().all(ok),
        BitmapPlan::And { plans, .. } | BitmapPlan::Or { plans, .. } => plans.iter().all(|q| leaves(q, ok)),
    }
}

/// Feed `f` (its ops end in the Feed) is a joined quiet arm: at least one probe, and `pipe` over its ops and builds.
pub(crate) fn joined(gx: &Gx<'_, '_>, f: usize) -> bool {
    let p = &gx.pipelines()[f];
    let Some((Op::Append(AppendDesc::Feed { .. }), body)) = p.ops.split_last() else { return false };
    if let Some((Op::Join(JoinDesc::Drain { matches, jointype }), rest)) = body.split_first() {
        return drain(gx, f, *matches, *jointype, rest);
    }
    body.iter().any(|op| matches!(op, Op::Join(JoinDesc::Probe { .. }))) && pipe(gx, f, body, 0)
}

/// D1483 (S2q, rerun/union_arms.rs `refills_whole`): feed `f` is a joined quiet arm a rerun set refills at every re-open
/// (a LATERAL or per-row SubPlan's `Append` arm that is a hash join, keyed by the outer row): `joined`'s form without a
/// draining arm, read with executor parameters (`pipe_by`), every probe hashing through built-in support (`hashes`).
pub(crate) fn joined_exec(gx: &Gx<'_, '_>, f: usize) -> bool {
    let p = &gx.pipelines()[f];
    let Some((Op::Append(AppendDesc::Feed { .. }), body)) = p.ops.split_last() else { return false };
    body.iter().any(|op| matches!(op, Op::Join(JoinDesc::Probe { marks: false, .. }))) && pipe_by(gx, f, body, 0, true) && hashes(gx, f, 0)
}

/// D3485 (a union's arm that is a Hash Right Join, read by a hash join's probe under a nest): feed `f` is a draining
/// hash join's arm (right, full, right-semi or right-anti: decompose join.rs `JOIN_FORMS` `drains`). Its source is the
/// hash table, its first op the Drain over the spool its probe pipeline closes, then quiet filters, gates and
/// projections; that probe pipeline passes `pipe` and its last op probes the same table. PostgreSQL hands on the entries
/// the probe left after the probe, as the Drain does, and nothing on the way raises or calls, so whether PostgreSQL ran
/// the arm shows nowhere.
fn drain(gx: &Gx<'_, '_>, f: usize, m: u8, jt: JoinType, rest: &[Op<'_>]) -> bool {
    let ps = gx.pipelines();
    let quiet = |n| crate::rerun::union_arms::quiet_in(gx, n, false);
    let Source::Breaker { slot: h } = ps[f].source else { return false };
    let Some(pm) = gx.life_of(f, m).map(|l| l.0 as usize).filter(|&pm| pm < f) else { return false };
    let spool = iface::breaker_of(&ps[pm].desc).is_some_and(|x| x.kind == BreakerKind::Materialize && x.slot == m);
    let probe = matches!(ps[pm].ops.last(), Some(Op::Join(JoinDesc::Probe { slot, jointype, marks: true, .. })) if *slot == h && *jointype == jt)
        && gx.life_of(pm, h).is_some()
        && gx.life_of(pm, h) == gx.life_of(f, h);
    drains(jt)
        && spool
        && probe
        && pipe(gx, pm, &ps[pm].ops, 0)
        && ps[f].desc.consumes.iter().all(|&s| s == h || s == m)
        && rest.iter().all(|op| match op {
            Op::Filter(n) | Op::Gate(n) => quiet(*n),
            Op::Project(l) => l.iter().all(quiet),
            _ => false,
        })
}

/// A join type whose inner entries are handed on after the probe (decompose join.rs `JOIN_FORMS`).
fn drains(jt: JoinType) -> bool {
    gravity_decompose::join::JOIN_FORMS.iter().any(|f| f.jt == jt && f.drains)
}

/// Pipeline `q` (its ops `ops`): a heap scan, a Result or quiet VALUES under quiet filters, gates, projections, build keys
/// and inner / left / semi / anti probes; each slot it reads is one of its probes' builds, a HashBuild pipeline of the
/// same form reading no other slot.
fn pipe(gx: &Gx<'_, '_>, q: usize, ops: &[Op<'_>], depth: u8) -> bool {
    pipe_by(gx, q, ops, depth, false)
}

/// `pipe`; `exec` (D1483, S2q's rerun refill, `joined_exec`): an executor parameter the nest or per-row SubPlan binds
/// reads quietly (rerun/union_arms.rs `quiet_in`), and an index walk or a bitmap heap scan (every leaf) keyed by quiet
/// values through built-in operators, its recheck quiet, is a source too.
fn pipe_by(gx: &Gx<'_, '_>, q: usize, ops: &[Op<'_>], depth: u8, exec: bool) -> bool {
    crate::stack_check();
    let ps = gx.pipelines();
    let quiet = |n: Node<'_>| crate::rerun::union_arms::quiet_in(gx, n, exec);
    let src = match &ps[q].source {
        Source::Range { .. } | Source::Const => true,
        Source::Values { lists, .. } => lists.iter().all(|l| l.as_list().is_some_and(|l| l.iter().all(quiet))),
        Source::Index { keys, recheck, .. } if exec => keys.iter().all(|x| quiet(x.value) && (x.opno == 0 || gravity_decompose::effects::builtin_oper(x.opno).is_some())) && recheck.iter().all(quiet),
        Source::Bitmap { plan, recheck, .. } if exec => leaves(plan, &|x| quiet(x.value) && (x.opno == 0 || gravity_decompose::effects::builtin_oper(x.opno).is_some())) && recheck.iter().all(quiet),
        _ => false,
    };
    let ops_ok = ops.iter().all(|op| match op {
        Op::Filter(n) | Op::Gate(n) => quiet(*n),
        Op::Project(l) => l.iter().all(quiet),
        Op::Join(JoinDesc::Build { keys, .. }) => keys.iter().all(quiet),
        // a draining probe (marks) ends the pipeline `drain` reads through its spool
        Op::Join(JoinDesc::Probe { jointype, keys, clauses, qual, marks, .. }) => {
            (if *marks { drains(*jointype) } else { matches!(jointype, JoinType::JOIN_INNER | JoinType::JOIN_LEFT | JoinType::JOIN_SEMI | JoinType::JOIN_ANTI) })
                && keys.iter().chain(clauses.iter()).chain(qual.iter()).all(quiet)
        }
        _ => false,
    });
    let probed = |s: u8| ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Probe { slot, .. }) if *slot == s));
    let build = |s: u8| match gx.life_of(q, s) {
        Some(LifeId(b)) if (b as usize) < q && depth < DEPTH => {
            let bp = &ps[b as usize];
            iface::breaker_of(&gx.pipelines()[b as usize].desc).is_some_and(|x| x.kind == BreakerKind::HashBuild && x.slot == s) && pipe_by(gx, b as usize, &bp.ops, depth + 1, exec)
        }
        _ => false,
    };
    src && ops_ok && ps[q].desc.consumes.iter().all(|&s| probed(s) && build(s))
}

/// Every probe in pipeline `q` and its builds hashes through built-in support only: GUARD-G1 `support`, KeyUse::Hash, over
/// the keys guard/user_fn.rs's HashJoin arm reads (design review r1 F3: the plan node is not in the graph; the probe's
/// `clauses` are its `hashclauses`, decompose join.rs, and PG's `hashoperators` are each clause's `opno`, createplan.c).
fn hashes(gx: &Gx<'_, '_>, q: usize, depth: u8) -> bool {
    crate::stack_check();
    let ps = gx.pipelines();
    ps[q].ops.iter().all(|op| {
        let Op::Join(JoinDesc::Probe { clauses, .. }) = op else { return true };
        let mut keys = PgVec::new_in(gx.mcx);
        let all = clauses.iter().all(|c| {
            let key = c.as_op_expr().filter(|o| o.args.len() == 2).and_then(|o| Some(Key { op: o.opno, lefttype: type_of(o.args.nth(0))?, righttype: type_of(o.args.nth(1))?, family: 0, coll: 0 }));
            key.map(|k| keys.push_in(k)).is_some()
        });
        all && support(gx, KeyUse::Hash, &keys[..], &[]).is_ok()
    }) && ps[q].desc.consumes.iter().all(|&s| match gx.life_of(q, s) {
        Some(LifeId(b)) if (b as usize) < q && depth < DEPTH => hashes(gx, b as usize, depth + 1),
        _ => false,
    })
}
