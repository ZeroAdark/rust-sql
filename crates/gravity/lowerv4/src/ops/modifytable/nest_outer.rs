//! D1933 (c) ('a write target without a single producer chain'): an UPDATE / DELETE over a
//! nested loop whose outer is the target's own heap scan and whose inner is a Material over one REFERENCE-marked
//! relation's heap scan. PostgreSQL 18.6 plans `UPDATE t ... FROM s WHERE <a qual no hash or merge join takes>` (DELETE
//! ... USING too) so: the target's scan outer, the join filter on the nested loop, s's scan and its own filters under a
//! Materialize. Its EvalPlanQual reruns that plan with the target's scan returning the locked newest version alone and
//! s's scan the row its junk ctid names (EvalPlanQualFetchRowMark under SnapshotAny: the version the join read), so the
//! Material holds that one row and the join filter runs again over the pair (no row: the write skips the target row).
//! Here the write rechecks in the IR (write/rowop_epq.rs `in_ir`, rerun/epq/epq_ir.rs `Redo::Nest`): on WRITE_RECHECK
//! the host puts the locked newest version in the write site's cells, the redo runs the target scan's recheck quals and
//! filters over it, joins it to the inner row the nest handed on (that row is s's version the join read: the Material
//! spooled it under the statement's snapshot) and runs the join quals, then the projection, and the op runs again at
//! that version (or the row is skipped). Not the runner's recheck (marks.rs): it would rerun the stopped pipeline on a
//! second node, whose Material reads the spool the statement's parameter words hold (rerun/spool/rec.rs: the record,
//! its count and the pulled child's end are per-execution words), so the inner would be the whole Material, not s's one
//! row (regress/isolation-overlay/specs/admit-d1933c-nest-outer.spec, s1k: a key moved off the joined row). Admitted
//! only where nothing else runs (no InitPlan but fixed, unseen ones: D3297 / D3632 `fixed_inits`, or D3714's that a
//! deferred fill runs first; no CTE, grouping but a fixed InitPlan's own (D3638), other producer or rerun set but the
//! unparameterized
//! inner's own; heap only), and where that recheck applies (known trigger facts, a BEFORE ROW trigger on the command
//! only on a write of one result relation, no system column past ctid and tableoid, no LIMIT that may be zero or one-time
//! gate: write/rowop_epq.rs's names), so no recheck defect nest_write.rs `deferred` names can apply. Owner admit.
//! D3486 (producerchain_mat_nests / producerchain_probe_nests, a BEFORE UPDATE ROW trigger on the target): PostgreSQL's
//! EvalPlanQual then runs in GetTupleForTrigger, which locks the newest version before any trigger runs, rechecks it
//! (the joins again over it), forms the new row over it and only then fires the triggers. The host's BEFORE ROW
//! prologue is that lock (hosts trigger_before.rs `lock_old`): a traversed lock answers RECHECK with no trigger run,
//! `Redo::Nest` rejoins the newest version and forms the cells, and the op runs again at its position, whose prologue
//! takes the held lock and fires the triggers over those cells, as D2812's data-modifying CTE write does. Several
//! result relations (producerchain ARM) keep the refusal, as rowop_epq.rs's.
//! D3364 (isolation epq-subq-subplan updjv): or beside per-row correlated SubPlan steps among the ops below the write
//! (`SET v = .. (SELECT .. FROM s2 WHERE s2.k = t.k)` over the same nest), none conditional: PostgreSQL's EvalPlanQual
//! re-runs such a SubPlan with the newest version's arguments under the statement's snapshot, and the redo evaluates
//! each step again in its place (rerun/epq/epq_ir.rs `Redo::Sub`, D652-S1's steps, in the same walk as `Redo::Nest`);
//! their rerun sets (a grouping inside one too) are the only other pipelines they add.
//! producerchain C (rules#364 / #366 / #368: a view rule's `UPDATE t ... WHERE t.k = OLD.k` over the view's LEFT join):
//! several such nests in a chain, INNER or LEFT, each over its own Material of one REFERENCE-marked relation's heap scan
//! or, beside such a nest or a LEFT one, over that relation's heap scan itself (unparameterized: its own quals read no
//! outer column, so the row it handed on is the version EvalPlanQual refetches and passes them again; an INNER chain of
//! heap scan inners alone stays write/nest_ref.rs's, the runner's recheck).
//! PostgreSQL 18.6 plans the rule's UPDATE as nested loops whose outer is the target's own scan and whose inners are
//! Materials (the view's relations, no join qual a hash or merge join takes, often none at all: each relation filtered
//! on the rule's key). EvalPlanQual refetches every marked relation's row at its junk ctid (a NULL one, a LEFT join's
//! unmatched inner, is no test tuple: that inner scan returns nothing, so the LEFT join NULL-extends again) and reruns
//! the joins over the newest version; a LEFT nest whose join qual fails over it NULL-extends the row, never skips it.
//! The redo does the same over the inner rows the nests handed on (epq_ir.rs `Redo::Nest`, nest.loop's
//! `Env::redo_nests`: a LEFT nest's NULL-extended row too).
//! VQ-WRITE (D3413, material_values_quiet B: `UPDATE t SET b = v.x FROM (VALUES ..) v(n, x) WHERE t.a > v.n`, DELETE .. USING
//! too): or a nest over a Material of a constant VALUES list (material V-QUIET's eager fill), the list COPY-marked.
//! PostgreSQL's EvalPlanQual hands the Values Scan its stored row (EvalPlanQualFetchRowMark's copy leg), which is the row
//! the Material held and the nest handed on, so the redo over that inner row is the same as over a REFERENCE one.
//! D3503 (`UPDATE t SET v = 3 WHERE id > ANY (SELECT nextval('q') - 8)`: Nested Loop Semi Join over a Materialize of the
//! Subquery Scan of a Result): or a Material of a COPY-marked subquery's Result row, the same stored-row recheck. D3619
//! (pg-boss resumeJobs: `UPDATE job .. WHERE name = $1 AND id IN (SELECT UNNEST($2::uuid[])) AND state = 'cancelled'`
//! over few rows, Nested Loop Semi Join over a Materialize of the Subquery Scan of a ProjectSet over a Result): or that
//! Result's set-returning target list's rows, each the subquery's row the nest may hand on; EvalPlanQual hands the
//! Subquery Scan the stored row of the one the join matched and never runs the ProjectSet again.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::modifytable::trig_facts::cmd_bits;
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::ops::shared::modifytable::{Member, WriteDesc};
use gravity_decompose::ops::shared::srf::SrfPlace;
use gravity_decompose::{Am, Op, Source};
use gravity_program::Cmd;
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::jointype::JoinType;
use types_nodes::plannodes::RowMarkType;

use crate::graph::Gx;

/// A heap range, index (not index-only) or bitmap scan source's (scanrelid, relid), as marks.rs `scan_of` takes them.
pub(super) fn heap(s: &Source<'_>) -> Option<(u32, u32)> {
    match s {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } => {
            Some((u32::from(*scanrelid), *relid))
        }
        _ => None,
    }
}

/// The pipeline before `k` whose `slot` pipeline `k` reads (its life, `Gx::life_of`): a later one may reuse the slot (a
/// data-modifying CTE's reader refills it, modifytable rowc-probe), and so may an earlier one (D3559: a sorted claim's
/// Sort before the CTE Scan's build of its store, a sort's slot reused for the build).
pub(crate) fn producer(gx: &Gx<'_, '_>, k: usize, slot: u8) -> Option<usize> {
    let q = gx.life_of(k, slot).map(|l| usize::from(l.0)).filter(|&q| q < k)?;
    (gx.pipelines()[q].desc.produces() == Some(slot)).then_some(q)
}

/// producerchain D: the INNER hash probe of `slot`'s build when it is one REFERENCE-markable relation's own heap scan under
/// filters, projections and per-row SubPlan steps (a key's, PostgreSQL's Hash evaluating it per inserted row), none
/// VOLATILE, its keys none either: (the build's pipeline, the relation's range table index, its oid). Its EvalPlanQual
/// hashes the build relation's row refetched at its junk ctid, the version the build read, so its key is the one the
/// matched record holds (the same snapshot, no VOLATILE call), and the redo matches the newest version against that record
/// (epq_ir.rs `Redo::Probe`). The build is `producer`'s.
pub(super) fn built(gx: &Gx<'_, '_>, k: usize, slot: u8) -> Option<(usize, u32, u32)> {
    let ps = gx.pipelines();
    let q = producer(gx, k, slot)?;
    let (rti, relid) = heap(&ps[q].source)?;
    let (Some(Op::Join(JoinDesc::Build { keys, .. })), rest) = ps[q].ops.split_last().map_or((None, &[][..]), |(l, r)| (Some(l), r)) else { return None };
    let quiet = rest.iter().all(|o| match o {
        Op::Filter(x) => !crate::effects::volatile(gx, *x),
        Op::Project(l) => !l.iter().any(|x| crate::effects::volatile(gx, x)),
        Op::SubPlan(d) => !d.effects().acts,
        _ => false,
    });
    (quiet && !keys.iter().any(|x| crate::effects::volatile(gx, x))).then_some((q, rti, relid))
}

/// producerchain rules#445: the Material maker `q` as the emit of a settled, unpruned union whose feeds are heap scans
/// under filters and projections (an inheritance parent read with its children, as a rule's `UPDATE t1_1 ... FROM t1`
/// plans it: Materialize over the Append of t1 and t1_1): each feed's (pipeline, range table index, oid). The nest hands
/// on the row the union spooled, the version the join read, which EvalPlanQual's refetch of that member's junk ctid
/// returns (the other members' scans return nothing), so the redo over it is PostgreSQL's (module doc). `None` for any
/// other maker.
fn union_rels<'g>(gx: &Gx<'g, '_>, q: usize) -> Option<PgVec<'g, (usize, u32, u32)>> {
    use crate::rerun::union_arms::{feed_of, union_of};
    let ps = gx.pipelines();
    let Source::Breaker { slot: u } = ps[q].source else { return None };
    let (Some(Op::Join(JoinDesc::Spool)), Some((first, body))) = (ps[q].ops.last(), ps[q].ops[..ps[q].ops.len().saturating_sub(1)].split_first()) else { return None };
    let lead = crate::graph::step_kind(first) == crate::face::kinds::KindId::SrcAppend && !crate::graph::is_arm(first);
    // D3297: or a union pruned at run time over fixed, unseen InitPlans' parameters only (rerun/spool/form.rs `init_prune`;
    // D3714: or over one that may raise, `late_param`): the row the nest hands on came from a member that pruning kept,
    // whose refetch EvalPlanQual returns
    let prune = |o: &Op<'_>| match o {
        Op::Append(AppendDesc::Union { prune: Some(d), .. }) => d.exec && (crate::rerun::spool::form::init_prune(gx, q, d) || crate::rerun::spool::form::late_param(gx, q, d).is_some()),
        _ => true,
    };
    let union = matches!(union_of(&ps[q]), Some((s, prunes)) if s == u && (!prunes || ps[q].ops.iter().all(prune)));
    if !lead || !union || feed_of(&ps[q]).is_some() || !body.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) {
        return None;
    }
    let feeds = crate::rerun::union_join_arms::feeds(gx, q, u);
    if feeds.len() < 2 {
        return None;
    }
    // each feed one heap scan under filters and projections, its feed marker last (merge/route.rs `union_targets`'s reading)
    let mut out = PgVec::new_in(gx.mcx);
    for &f in feeds.iter() {
        let (last, rest) = ps[f].ops.split_last()?;
        let simple = feed_of(&ps[f]) == Some(u) && !matches!(last, Op::Filter(_) | Op::Project(_)) && ps[f].desc.consumes.is_empty() && rest.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_)));
        let (rti, relid) = heap(&ps[f].source).filter(|_| simple)?;
        out.push_in((f, rti, relid));
    }
    Some(out)
}

/// producerchain pj211 (regress partition_join#211: `DELETE FROM prt1_l WHERE EXISTS (SELECT 1 FROM int4_tbl, LATERAL
/// (SELECT int4_tbl.f1 FROM int8_tbl LIMIT 2) ss WHERE prt1_l.c IS NULL)`, PostgreSQL 18.6's Nested Loop Semi Join over
/// the targets' Append with a Materialize over a Nested Loop of int4_tbl and the Subquery Scan ss as its inner): the
/// Material maker `q`'s nests, each INNER, its join qual nothing VOLATILE, its inner the rows of a COPY-marked
/// subquery whose stored row is formed in that inner (marks.rs `own_copy_at`): the marks' range table indexes. Its
/// EvalPlanQual refetches the maker's own relation at its junk ctid (the version the Material spooled) and hands the
/// Subquery Scan the stored row, so the nest joins exactly the pair the Material holds, the one the write's nest
/// handed on: the redo over that row is PostgreSQL's (module doc). `None` for any other nest.
fn copy_nests<'g>(gx: &Gx<'g, '_>, w: &WriteDesc<'_>, q: usize) -> Option<PgVec<'g, u32>> {
    let ps = gx.pipelines();
    let mut rtis = PgVec::new_in(gx.mcx);
    for (j, o) in ps[q].ops.iter().enumerate() {
        let Op::Join(JoinDesc::Nest { jointype, qual, .. }) = o else { continue };
        let iq = ps.inner_of(q, j)?;
        if *jointype != JoinType::JOIN_INNER || qual.iter().any(|x| crate::effects::volatile(gx, x)) {
            return None;
        }
        let copy = |m: &&gravity_decompose::ops::shared::modifytable::RowMark| m.mark == RowMarkType::ROW_MARK_COPY && !m.parent && crate::rerun::epq::marks::own_copy_at(gx, iq, m.rti).is_some();
        let (Some(m), None) = ({ let mut c = w.mark_descs.iter().filter(copy); (c.next(), c.next()) }) else { return None };
        rtis.push_in(m.rti);
    }
    Some(rtis)
}

/// D3297 (regress partition_prune#424: `UPDATE ab_a1 SET b = 3 FROM ab_a2 WHERE ab_a2.b = (SELECT 1)`, the InitPlan on the
/// ModifyTable, read by the Material's union pruning and its members' filters): the pipelines of the InitPlans beside
/// the write (graph.rs `Gx::with_producers`: each one's and the producers of the slots it reads) when every one is
/// a plain one (no CTE) whose parameters are fixed and unseen (rerun/spool/form.rs `init_fixed`: it acts nowhere, no
/// VOLATILE call, may not raise, no rerun set re-runs it; decompose refuses one with parameters of its own); `None`
/// otherwise. PostgreSQL's EvalPlanQualStart forces the InitPlans the subplan reads and copies the parent's values down
/// (ExecSetParamPlanMulti), and one inside the subplan tree runs again at the first recheck under the statement's
/// snapshot, which for such an InitPlan returns the same value: so the recheck sees the value the statement read, and the
/// rows the nests hand on passed their own filters at it. D3632: the redo's expressions read those parameters' words too
/// (a SET of `(SELECT 2)`, a filter over one), where rowop_epq.rs rowc_tail still refuses an InitPlan its forms read
/// (s2initr: a nextval InitPlan, VOLATILE, runs again). D3638: and every arm of a streamed union inside one (an aggregate
/// over a partitioned table's leaves), whose slot's life is its last arm's alone (graph.rs `run_arms`).
/// D3619: `own`, a data-modifying CTE's write pipeline, whose own CTE link is no InitPlan of this kind (the write is
/// that CTE's plan), and then no other InitPlan at all (rowop_epq.rs ROWC_INITPLAN's reason: a CTE's first recheck runs
/// one below its write again).
fn fixed_inits<'g>(gx: &Gx<'g, '_>, own: Option<usize>) -> Option<PgVec<'g, usize>> {
    use gravity_decompose::ops::shared::subplan::SubLink;
    let mut out = PgVec::new_in(gx.mcx);
    for i in gx.graph.init_plans.iter() {
        if own.is_some_and(|k| matches!(i.link, SubLink::Cte) && usize::from(i.pipeline) == k) {
            continue;
        }
        // D3714: or one that may raise whose value a deferred fill's pruning reads first (`init_at_fill`)
        let fixed = |id: i32| crate::rerun::spool::form::init_fixed(gx, id) || crate::rerun::spool::form::init_at_fill(gx, id);
        if own.is_some() || matches!(i.link, SubLink::Cte) || !i.params.iter().all(fixed) {
            return None;
        }
        out.push_in(usize::from(i.pipeline));
    }
    let mut at = 0;
    while let Some(&q) = out.get(at) {
        at += 1;
        for x in gx.run_arms(q).into_iter().chain(gx.with_producers([q])) {
            if !out.contains(&x) {
                out.push_in(x);
            }
        }
    }
    Some(out)
}

/// Whether pipeline index `x` is an inner embedded (at any depth) in graph pipeline `q`'s ops (pipes.rs `inners`).
fn within(ps: &crate::pipes::Pipes<'_>, q: usize, x: usize) -> bool {
    let inners = ps.inners();
    let mut at = x;
    while let Some(((owner, _), _)) = at.checked_sub(ps.len()).and_then(|i| inners.get(i)) {
        if *owner == q {
            return true;
        }
        at = *owner;
    }
    false
}

/// Whether pipeline `k`'s UPDATE / DELETE `w` takes this form: then its rows are the target's own scan rows (the
/// caller's chain holds) and its write rechecks in the IR over the nests' inner rows (module doc).
pub(crate) fn admits(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> bool {
    let ps = gx.pipelines();
    let p = &ps[k];
    // producerchain ARM: or one arm of a several-target write's streamed union (inheritance or an unpruned partitioned
    // target), its source the arm's own target, its nest a copy over the plan Material every arm reads (decompose
    // arm_nests.rs). D3297 (#420 / #424): a row the SET moves to another partition too, as ROWC-UNION-UPDATE's IR recheck
    // takes one (rowop_epq.rs `union_rows`): ExecCrossPartitionUpdate's delete leg meeting a concurrent update runs
    // ExecUpdate's EvalPlanQual, then ExecGetUpdateNewTuple over its result and the update again (in place or a move); the
    // host's move answers Recheck on that leg (hosts write.rs `move_row`), the redo runs over the newest version and the op
    // runs again through the root (write_move.rs); a row another session moved is the lock's 40001, as PostgreSQL's
    let several = w.targets.len() > 1;
    if !matches!(w.cmd, Cmd::Update | Cmd::Delete) || !(several || w.targets.len() == 1 && w.route.is_none()) || w.members.has(Member::TargetView) {
        return false;
    }
    // D3681: only filters, gates, projections (and the arm's marker) before the write: no nest, so no LEFT one and no
    // Material maker (the test after the loop below), asked before the walk over every target
    if p.ops.iter().take_while(|o| !matches!(o, Op::Write(_))).all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_)) || several && crate::graph::is_arm(o)) {
        return false;
    }
    let target = |(rti, relid): (u32, u32)| if several { w.targets.iter().any(|t| t.rti == rti && t.relid == relid) } else { (rti, relid) == (w.rti, w.plan.target_rel) };
    if !heap(&p.source).is_some_and(target) {
        return false;
    }
    // filters, gates and projections beside INNER nests (producerchain C: or LEFT ones, several in a chain), each
    // unparameterized, its inner a Material's rows under filters and projections, or (beside such a nest or a LEFT one)
    // one heap scan's rows under filters and projections, as PostgreSQL 18.6 plans a relation it reads once
    let pre = || p.ops.iter().take_while(|o| !matches!(o, Op::Write(_)));
    // producerchain pj211: or a SEMI one over a Material (PostgreSQL's semi join hands on its first match, the junk ctid
    // and copy columns the write's marks read: EvalPlanQual's inner is that one row, as an INNER nest's)
    let nest = |o: &Op<'_>| matches!(o, Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER | JoinType::JOIN_LEFT | JoinType::JOIN_SEMI, params, .. }) if params.is_nil());
    let semi = |o: &Op<'_>| matches!(o, Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_SEMI, .. }));
    let left = pre().any(|o| matches!(o, Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_LEFT, .. })));
    // each nest's relation: (its Material's maker pipeline, none for a heap scan inner; the relation it scans)
    let mut rels: PgVec<'_, (Option<usize>, u32, u32)> = PgVec::new_in(gx.mcx);
    // producerchain D: the INNER hash probes' builds (`built`) and their key SubPlans' rerun members
    let mut builds: PgVec<'_, usize> = PgVec::new_in(gx.mcx);
    // producerchain rules#445: a Material's union feeds (`union_rels`)
    let mut arms: PgVec<'_, usize> = PgVec::new_in(gx.mcx);
    let mut unions: PgVec<'_, usize> = PgVec::new_in(gx.mcx);
    // producerchain pj211: the Material makers whose nests hand on a COPY-marked subquery's rows, with those marks' rtis
    let mut copiers: PgVec<'_, (usize, PgVec<'_, u32>)> = PgVec::new_in(gx.mcx);
    // D3364: the per-row SubPlan steps below the write (module doc), by plan id
    let mut subs: PgVec<'_, i32> = PgVec::new_in(gx.mcx);
    // VQ-WRITE (D3413): the Material makers over a constant VALUES list, with that list's range table index (its COPY mark)
    let mut vals: PgVec<'_, (usize, u32)> = PgVec::new_in(gx.mcx);
    // D3297: the pipelines of the fixed, unseen InitPlans (`fixed_inits`; D3632: the redo may read them). D3619: or a
    // data-modifying CTE's write (pg-boss resumeJobs: `WITH results AS (UPDATE job ..) SELECT COUNT(*) FROM results`),
    // its own CTE link no InitPlan of those, the pipelines that read its product after it (`after`) none of the
    // recheck's: PostgreSQL's EvalPlanQual for the CTE's ModifyTable reruns that CTE's plan alone
    let cte = gx.cte_write(k);
    let Some(inits) = fixed_inits(gx, cte.then_some(k)) else { return false };
    let mut after: PgVec<'_, usize> = PgVec::new_in(gx.mcx);
    if cte {
        let made = |q: usize| ps[q].desc.produces();
        let reads = |q: usize, from: usize| made(from).is_some_and(|s| ps[q].desc.consumes.contains(&s));
        while let Some(q) = (k + 1..ps.len()).find(|&q| !after.contains(&q) && (reads(q, k) || after.iter().any(|&a| reads(q, a)))) {
            after.push_in(q);
        }
    }
    for (j, o) in pre().enumerate() {
        if matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_)) || several && crate::graph::is_arm(o) {
            continue;
        }
        // D3364: a per-row step (its plan embedded in the step or a rerun set), never conditional nor a check option's
        // host-fed one; a table another pipeline builds for it (a hashed step's) is no maker's and refuses below
        // D3687: a check option's host-fed step (`graph::check_sub`), which the host runs per written row
        if crate::graph::check_sub(w, &p.ops, o) {
            continue;
        }
        if let Op::SubPlan(d) = o {
            if d.conditional || w.members.has(Member::WriteSubPlan) {
                return false;
            }
            subs.push_in(d.plan_id);
            continue;
        }
        if let Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_INNER, slot, keys, qual, .. }) = o {
            let Some((q, rti, relid)) = built(gx, k, *slot) else { return false };
            let sub = |l: &types_nodes::list::NodeList<'_>| l.iter().any(gravity_decompose::expr::has_subplan);
            if sub(keys) || sub(qual) || rels.iter().any(|x| x.1 == rti) {
                return false;
            }
            rels.push_in((None, rti, relid));
            builds.push_in(q);
            for d in ps[q].ops.iter().filter_map(|o| if let Op::SubPlan(d) = o { Some(d) } else { None }) {
                builds.extend_in(gx.graph.reruns.iter().filter(|r| r.plan_id == d.plan_id).flat_map(|r| r.pipelines.iter().map(|&x| usize::from(x))));
            }
            continue;
        }
        let Some(ik) = ps.inner_of(k, j).filter(|_| nest(o)) else { return false };
        if !ps[ik].ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) {
            return false;
        }
        let rel = match ps[ik].source {
            // the one pipeline that makes that Material (its spool op): a heap scan under filters and projections. D3640: a
            // fixed InitPlan's own breaker (its Aggregate's) may reuse the slot's number in a life that ends before: no maker
            Source::Breaker { slot } => {
                let mut made = (0..ps.len()).filter(|&q| q != k && ps[q].desc.produces() == Some(slot) && !inits.contains(&q));
                let (Some(q), None) = (made.next(), made.next()) else { return false };
                if let Some(feeds) = union_rels(gx, q) {
                    unions.push_in(q);
                    // producerchain rules#445: one relation per feed, none another nest's
                    for (f, rti, relid) in feeds {
                        if rels.iter().any(|x| x.1 == rti) {
                            return false;
                        }
                        rels.push_in((Some(q), rti, relid));
                        arms.push_in(f);
                    }
                    continue;
                }
                // producerchain pj211: or beside INNER nests over a COPY-marked subquery's rows (`copy_nests`)
                let nested = |o: &Op<'_>| matches!(o, Op::Join(JoinDesc::Nest { .. }));
                if ps[q].ops.iter().any(nested) {
                    let Some(rtis) = copy_nests(gx, w, q) else { return false };
                    copiers.push_in((q, rtis));
                }
                // D3619: a Result's set-returning target list (ProjectSet: `id IN (SELECT unnest($2::uuid[]))`) under the
                // Const source of D3503's COPY-marked subquery below
                let srf = |o: &Op<'_>| matches!(o, Op::Srf(d) if d.place == SrfPlace::TargetList) && matches!(ps[q].source, Source::Const);
                if !ps[q].ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_) | Op::Join(JoinDesc::Spool)) || nested(o) || srf(o)) {
                    return false;
                }
                // VQ-WRITE: or a VALUES list's rows (material V-QUIET's eager fill: no executor parameter, D449's quiet
                // expressions): EvalPlanQual hands the Values Scan its stored COPY row, the row the Material held and
                // the nest handed on, so the redo over that inner row is PostgreSQL's (module doc)
                match ps[q].source {
                    Source::Values { scanrelid, .. } if !ps[q].ops.iter().any(nested) && ps[q].desc.consumes.is_empty() => {
                        vals.push_in((q, u32::from(scanrelid)));
                        Some((Some(q), u32::from(scanrelid), 0))
                    }
                    // D3503: or a Result's one row (a VOLATILE call or a raise keeps it a subquery: the reader pulls it,
                    // rerun/spool/form.rs `result_child`), the row of a COPY-marked subquery whose record row the maker forms
                    // (marks.rs `own_copy_at`): EvalPlanQual hands that Subquery Scan its stored row, the one the Material
                    // holds and the nest handed on, and never runs the Result again
                    Source::Const if !ps[q].ops.iter().any(nested) && ps[q].desc.consumes.is_empty() => {
                        let copied = |m: &&gravity_decompose::ops::shared::modifytable::RowMark| m.mark == RowMarkType::ROW_MARK_COPY && !m.parent && crate::rerun::epq::marks::own_copy_at(gx, q, m.rti).is_some();
                        w.mark_descs.iter().find(copied).map(|m| {
                            vals.push_in((q, m.rti));
                            (Some(q), m.rti, 0)
                        })
                    }
                    ref s => heap(s).map(|(rti, relid)| (Some(q), rti, relid)),
                }
            }
            ref s => heap(s).map(|(rti, relid)| (None, rti, relid)),
        };
        let Some(rel) = rel.filter(|r| !rels.iter().any(|x| x.1 == r.1 || x.0.is_some() && x.0 == r.0)) else { return false };
        // producerchain pj211: a SEMI nest over a Material only
        if semi(o) && rel.0.is_none() {
            return false;
        }
        rels.push_in(rel);
    }
    // a Material or a LEFT nest (else an INNER chain of heap scan inners: write/nest_ref.rs's, the runner's recheck);
    // every other pipeline one of the makers, and the relations the write marks besides its target exactly the nests'
    // (REFERENCE: a plain relation's row, refetched at its junk ctid)
    let maker = |q: usize| rels.iter().any(|x| x.0 == Some(q));
    // producerchain ARM: a sibling arm (another target's rows, its own write and recheck), as rowop_epq.rs `subs_in_ir`
    let sibling = |q: usize| several && ps[q].ops.iter().any(crate::graph::is_arm) && ps[q].ops.iter().any(|o| matches!(o, Op::Write(_)));
    // D3364: a member of a SubPlan step's rerun set (the redo's `Redo::Sub` runs it again over the newest version)
    let member = |q: usize| gx.graph.reruns.iter().any(|r| subs.contains(&r.plan_id) && r.pipelines.iter().any(|&m| usize::from(m) == q));
    let other = |q: usize| maker(q) || builds.contains(&q) || arms.contains(&q) || sibling(q) || inits.contains(&q) || member(q) || after.contains(&q);
    if !(left || rels.iter().any(|x| x.0.is_some())) || (0..ps.len()).any(|q| q != k && !other(q)) {
        return false;
    }
    // producerchain rules#445: when a Material's union reads an inheritance parent's members, the parent's own mark
    // (isParent: PostgreSQL refetches its children's rows, never the parent's) and a member the planner excluded (its
    // mark stays, no scan reads it: EvalPlanQualFetchRowMark's tableoid test never matches it) are no nest's. D3640: a
    // parent's own mark is no nest's without a union too (a partitioned FROM relation pruned to one leaf at plan time,
    // whose Material reads that leaf alone): ExecInitModifyTable skips every isParent mark ('irrelevant at runtime')
    let inert = |m: &gravity_decompose::ops::shared::modifytable::RowMark| {
        m.parent || (several || !arms.is_empty()) && m.prti != m.rti && w.mark_descs.iter().any(|x| x.parent && x.rti == m.prti) && !rels.iter().any(|x| x.1 == m.rti)
    };
    // producerchain ARM (rules#431-#433: a view rule's UPDATE over an inheritance parent whose Material holds the view's
    // Subquery Scan over the union): or the COPY mark of the subquery whose rows a union's Material holds, its record row
    // formed in that maker (marks.rs `own_copy_at`): EvalPlanQual hands the Subquery Scan the stored row, the one the
    // nest handed on, and the union's members are no mark's
    let copy_of = |m: &gravity_decompose::ops::shared::modifytable::RowMark, q: usize| m.mark == RowMarkType::ROW_MARK_COPY && !m.parent && crate::rerun::epq::marks::own_copy_at(gx, q, m.rti).is_some();
    let copied = |q: usize| unions.contains(&q) && w.mark_descs.iter().any(|m| copy_of(m, q));
    // producerchain pj211: or the COPY mark a maker's nest hands on (`copy_nests`): EvalPlanQual's Subquery Scan returns
    // the stored row, the one the Material holds
    // VQ-WRITE: or a VALUES list's COPY mark, its row the one the Material holds (`vals`)
    let valued = |m: &gravity_decompose::ops::shared::modifytable::RowMark| m.mark == RowMarkType::ROW_MARK_COPY && !m.parent && vals.iter().any(|v| v.1 == m.rti);
    let copy = |m: &gravity_decompose::ops::shared::modifytable::RowMark| {
        unions.iter().any(|&q| copied(q) && copy_of(m, q)) || m.mark == RowMarkType::ROW_MARK_COPY && !m.parent && copiers.iter().any(|c| c.1.contains(&m.rti)) || valued(m)
    };
    if !vals.iter().all(|v| w.mark_descs.iter().any(|m| valued(m) && m.rti == v.1)) {
        return false;
    }
    let marks: PgVec<'_, _> = w.mark_descs.iter().filter(|m| !w.targets.iter().any(|t| t.rti == m.rti) && !inert(m) && !copy(m)).collect_in(gx.mcx);
    let held: PgVec<'_, _> = rels.iter().filter(|x| !x.0.is_some_and(copied) && !vals.iter().any(|v| x.0 == Some(v.0))).collect_in(gx.mcx);
    let marked = |m: &&gravity_decompose::ops::shared::modifytable::RowMark| m.mark == RowMarkType::ROW_MARK_REFERENCE && !m.parent && m.ctid_col > 0 && held.iter().any(|x| x.1 == m.rti && x.2 == m.relid);
    if marks.len() != held.len() || !marks.iter().all(marked) {
        return false;
    }
    // each nest's own rerun set (its Material's maker, PG's rescan of an unparameterized inner) are the only ones
    // (producerchain pj211: or a copier's nest inner, which EvalPlanQual never reruns: its row is the stored copy; D3364: or a
    // SubPlan step's own set, `member`)
    let under = |x: usize| x >= ps.len() && copiers.iter().any(|c| within(ps, c.0, x));
    let own_rerun = gx.graph.reruns.iter().all(|r| r.pipelines.iter().all(|&x| (other(usize::from(x)) && !after.contains(&usize::from(x))) || under(usize::from(x))));
    // D3364: a grouping (or streamed group) only inside a SubPlan step's rerun set (a correlated SubPlan over an aggregate,
    // re-run at each call and by the redo). D3638: or inside a fixed InitPlan's plan (`inits`: `SELECT max(v) FROM p`),
    // which runs once before the root; the redo reads its parameter's word, as EvalPlanQualStart copies the value down
    let grouped = gx.graph.groups.iter().chain(gx.graph.streams.iter()).any(|g| {
        let q = usize::from(g.producer.0);
        !member(q) && !inits.contains(&q) && !after.contains(&q)
    });
    if !own_rerun || grouped {
        return false;
    }
    // the IR recheck's own conditions (write/rowop_epq.rs `subs_in_ir`'s)
    let src = p.node_ops.iter().find(|n| n.first_op == 0).map_or(p.node_id, |n| n.node_id);
    let sys = gx.graph.scan_lanes.iter().any(|l| l.node_id == src && l.names().any(|n| n != "ctid" && n != "tableoid"));
    // every target's trigger facts known; a BEFORE ROW trigger of the command only on a write of one result relation
    // (D3486, module doc; producerchain ARM's several targets: none on any)
    let one = w.targets.len() <= 1 && w.route.is_none();
    let quiet = core::iter::once(w.plan.target_rel).chain(w.targets.iter().map(|t| t.relid)).map(|r| gx.facts.trig_ext(r)).all(|t| t.known && (one || t.before_row & cmd_bits(w.cmd) == 0));
    !sys && quiet
}
