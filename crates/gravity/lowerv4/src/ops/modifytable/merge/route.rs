//! W6-MERGE-ROUTE-MATCHED (M8-WRITE):
//! write.merge takes write.route's UPDATE / DELETE forms (write/update.rs's and write/delete.rs's, M4-ROUTE R2). A MERGE
//! through a partitioned or inheritance root whose actions update or delete, and never insert, is either
//! (a) ONE result relation under the root (a leaf the planner pruned to, an inheritance tree's one): the single-target
//!     write over its own scan, its plan naming the root (`WritePlan::with_route`, no leaves), so the host locks the root
//!     and fires the root's statement triggers for MERGE's actions (hosts write.rs `under_root`), never the leaf's; or
//! (b) SEVERAL result relations: decompose streams the write into its Append's arms (append.rs `streamed_write`, V4R-214),
//!     each arm its own target's rows (write.route's `TargetsRoad`), every arm writing through the one site write/rowop.rs
//!     shares. An UPDATE action's cells are the statement's updated columns in the root's attno order (`pick`: hosts
//!     `MultiTargetHost`'s `set`, updatedCols less dropped and generated columns): the action's SET value, else the carried
//!     old cell. The host forms each target's new row over the old version it fetches by the position (ExecMergeMatched's
//!     ri_oldTupleSlot, ExecGetUpdateNewTuple) and writes it through that target's MERGE host.
//! MOVE: a row that may leave its leaf (`moving`) moves as an UPDATE's does: the plan carries write.route's
//! destinations (`Route::moves`) and the row's host deletes it from its leaf and inserts it through the root (hosts
//! write_move.rs, ExecCrossPartitionUpdate). A delete that meets a newer version answers Recheck, and the MERGE's own
//! recheck reruns the whole action list over the newest version (ExecCrossPartitionUpdate leaves it to
//! ExecMergeMatched); a deleted one is NOT MATCHED (its NOT MATCHED actions run). Check options are the move host's.
//! Refused by name here (the census names each next row): TABLEOID (the target
//! rows of several result relations under a sort, a hash build or a nest inner: a locator of (tableoid, ctid); but a
//! hash build over the settled union of their scans serves as the union form, mergecmd TABLEOID-HASH `hashed_union`),
//! INSERT_BESIDE (a NOT MATCHED INSERT, routed through the root, beside the target host), RETURNING over several result
//! relations and NEST (rows of several result relations through a nested loop: D1933's recheck names one target's inner
//! scan, the design review's condition; a root pruned to one leaf is nest_write.rs's, mergecmd MOVE (c)). D3367: a root
//! pruned to no leaf (D829's MERGE form) is (a) over the planner's dummy Result: the root its own result relation, no row
//! reaching it and none moving, its statement triggers the host's (write.rs `pruned_root`). A LIMIT that may be zero or a
//! one-time gate elsewhere in the statement does not refuse: a data-modifying CTE runs to its end (D1688 / D1751, fixed).
//! ROUTED stays for
//! the INSERT-only MERGE with a partition key updated (V4R-498's form is the partitioned one) and for one result relation
//! whose rows are not its own scan's (under a nest, a sort or a hash build; but a nest's Material inner over its scan:
//! MOVE (b), and the one producer of its scan that `built_target` takes, D776's / D1249's form: MOVE (d)). mergecmd INHERIT-ROOT: the INSERT-only MERGE
//! through an inheritance root with one result relation inserts into the root itself (`inherit_root`); its RLS checks are
//! the first result relation's list mapped onto the root's attnos (`WritePlan.checks_of`, hosts write.rs) and RETURNING
//! reads the root's row at that relation's attnos (nm.rs `returned`); a root with indexes or triggers stays refused, each
//! by its name.
//! Helper file of write.merge: no rows of its own.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::modifytable::partwrite::root_attno;
use gravity_decompose::ops::modifytable::trig_facts::{TrigExt, TRIG_UPDATE};
use gravity_decompose::ops::shared::modifytable::{MergeDesc, Member, RouteDesc, Target, WriteDesc};
use gravity_decompose::{Am, Op, Source};
use gravity_ir::Value;
use gravity_program::{ArenaVec, Cmd, WritePlan};
use mcx::{CollectIn, GrowIn, Mcx, PgVec};
use types_nodes::jointype::JoinType;

use crate::face::end::TargetsRoad;
use crate::face::kinds::KindId;
use crate::face::strategy::WRITE_ROUTE;
use crate::frame::frame::StepMaker;
use crate::frame::px::Px;
use crate::graph::Gx;
use crate::admit::refusal::Reason;

use crate::ops::modifytable::merge::admit::{fires, shape};
use crate::ops::modifytable::merge::nm::insert_only;
use crate::ops::modifytable::strategies::route::Route;

/// D1082 (M7 I48): what stays refused of a MERGE through a root (write/merge_admit.rs's name, kept for the census).
pub(crate) const ROUTED: &str = "a MERGE through a partitioned or inheritance root (write.route's MERGE form)";
pub(crate) const TABLEOID: &str = "a MERGE through a root whose target rows reach the write through a sort, a hash build or a nest inner over several result relations (the junk tableoid form)";
pub(crate) const INSERT_BESIDE: &str = "a MERGE through a root with a NOT MATCHED INSERT beside UPDATE / DELETE (the routed INSERT beside the target host)";
pub(crate) const RETURNING: &str = "a MERGE through a root with RETURNING over several result relations";
pub(crate) const NEST: &str = "a MERGE through a root over a nested loop's rows (write.route's nest form: D1933's recheck names one target's inner scan)";
pub(crate) const INHERIT_INDEXED: &str = "an INSERT-only MERGE through an inheritance root with indexes (the root's own INSERT maintains them)";
pub(crate) const INHERIT_TRIGGERS: &str = "an INSERT-only MERGE through an inheritance root with triggers (the root's own INSERT fires them)";
pub(crate) const INHERIT_CHECKS: &str = "an INSERT-only MERGE through an inheritance root with a view's check options (the root's are RLS only)";

/// write/merge_admit.rs's routed check: the result relation whose own scan rows reach pipeline `k`'s write `(rti, relid)`
/// (the arm's own target under (b); the write's own target otherwise), or a refusal by name.
pub(crate) fn admit(gx: &Gx<'_, '_>, k: usize, (w, m): (&WriteDesc<'_>, &MergeDesc<'_>), node: i32, dummy: bool) -> (u32, u32) {
    let own = (w.rti, w.plan.target_rel);
    let (Some(r), false) = (w.route.as_ref(), insert_only(m)) else {
        // unrouted, or V4R-498's INSERT-only form (member Route; one updating a key stays ROUTED); mergecmd INHERIT-ROOT
        if inherit_root(gx, w, m, node).is_some() {
            return own;
        }
        if w.targets.len() != 1 && w.route.is_none() || w.members.has(Member::Targets) || w.members.has(Member::CrossPartition) || w.route.is_some_and(|r| r.cross_partition) {
            shape(node, ROUTED);
        }
        return own;
    };
    // SORT-UNION (merge#364 group): beside the result relations' arms a merge join's inner Sort takes
    // (`sorted_arms`), a NOT MATCHED INSERT is the root's own routed INSERT (hosts MultiTargetHost's route leg,
    // ExecInitMerge's rootRelInfo); every other routed form keeps the name
    if m.actions.iter().any(|a| a.cmd == Some(Cmd::Insert)) && insert_beside(gx, k, w).is_none() {
        shape(node, INSERT_BESIDE);
    }
    // MOVE: a row that may leave its leaf moves (`plan` gives the write write.route's destinations, as an
    // UPDATE's: hosts write_move.rs, ExecCrossPartitionUpdate under ExecMergeMatched). Its check options are the move
    // host's, as an UPDATE's (V4R-329): each destination checks the first result relation's list with WCO_RLS_UPDATE_CHECK,
    // ExecInsert's kind for a MERGE UPDATE action's moved row; the source skips them (ExecUpdateAct's
    // partition_constraint_failed); ExecMergeMatched's USING check of the old version runs first (hosts `merge_check`)
    // G2(c) part 2: target rows out of a settled union of the targets' feeds (`union_targets`), each picked by its tableoid
    let union = union_targets(gx, k, w).is_some();
    // writesubplan (gen mg-0179): an arm of the (b) form projects its own result relation's list (ExecInitMerge gives
    // each its returningList, the first's mapped onto its attnos) over the row its host hands back (`b_arm`); beside
    // the routed INSERT, whose row is the root's read at the first relation's attnos (nm.rs `returned`), it stays refused
    if w.returning.is_some() && w.targets.len() > 1 && !union && !(b_arm(gx, k, w) && insert_beside(gx, k, w).is_none()) {
        shape(node, RETURNING);
    }
    // the design review's condition for D1933 (write/nest_write.rs admits a nest under ONE target's recheck): no routed
    // UPDATE / DELETE form of several result relations reaches the write through a nested loop. mergecmd MOVE (c): the
    // one result relation of a root pruned to a leaf (merge#377: a key pinned by a constant) is that leaf's own scan, so
    // its nest is nest_write.rs's as an unrouted target's (`merge`: the leaf's scan the outer of REFERENCE-marked inners,
    // or the leaf's scan the inner); every other form there by name
    let p = &gx.pipelines()[k];
    let wi = p.ops.iter().position(|o| matches!(o, Op::Write(_))).unwrap_or(p.ops.len());
    // (G2(c) part 2: over the union form, nest_write.rs `union_nest` admits the nest or refuses it by name)
    if p.ops[..wi].iter().any(|o| matches!(o, Op::Join(JoinDesc::Nest { .. }))) && !union && w.targets.len() != 1 {
        shape(node, NEST);
    }
    // write.transition's plan names the target's and the root's; every other result relation's here (merge.rs `fires`)
    for relid in relids(w, gx.mcx) {
        if let Some(why) = crate::ops::modifytable::triggers::refusal(gx.facts.trig_ext(relid), Cmd::Merge, false, false) {
            shape(node, why);
        }
    }
    let scans = |rti: u32, relid: u32| matches!(&p.source, Source::Range { am, scanrelid, relid: r, .. } | Source::Index { am, scanrelid, relid: r, .. } | Source::Bitmap { am, scanrelid, relid: r, .. } | Source::Tid { am, scanrelid, relid: r, .. }
        if *am == Am::Heap && u32::from(*scanrelid) == rti && *r == relid);
    if w.targets.len() == 1 {
        // (a): the one result relation's own scan rows, or the planner's dummy relation (its row never comes: D3367, D829's
        // root pruned to no leaf among them, `pruned`); mergecmd
        // MOVE (b): or that leaf's rows out of the Material a nest's inner reads (merge#377's plan of unanalyzed tables:
        // the source's scan the outer), as an unrouted target's (nest_write.rs `material` (b): the position the leaf's
        // junk ctid, the recheck rerunning the fill at its newest version)
        // writesubplan (updatable_views#961-963): or the drained form beside the routed INSERT (`insert_beside`); D3479:
        // its RETURNING too, the site as wide as the leaf's row and the root's (`ret_width`)
        // D3478: or that leaf's rows out of the one producer that scans it (merge/admit.rs `built_target`: a
        // hash build the emit's REFERENCE-marked source probes, D776, or a merge join's sorted input, D1249), as an
        // unrouted target's: the position the leaf's junk ctid, the recheck rerunning that producer at its newest version
        let built = || crate::ops::modifytable::merge::admit::built_target(gx, k, w).is_some();
        if !(scans(own.0, own.1) || matches!(p.source, Source::Const) && dummy || crate::ops::modifytable::nest_write::material_target(gx, k, w) || insert_beside(gx, k, w).is_some() || built()) {
            shape(node, ROUTED);
        }
        return own;
    }
    // G2(c) part 2: the union form's rows name their result relation by the junk tableoid (merge/lower.rs `Targets`)
    if union {
        return own;
    }
    // (b): an arm of the Append the write streams into, over its own target's scan
    let arm = p.ops[..wi].iter().any(crate::graph::is_arm);
    match w.targets.iter().find(|t| scans(t.rti, t.relid)) {
        Some(t) if arm => (t.rti, t.relid),
        _ => shape(node, TABLEOID),
    }
}

/// G2(c) part 2 (mergecmd; material's G2(b) settles the union under rule B): pipeline `k`'s write `w` of several result
/// relations whose target rows come out of a settled union read first (`Union`, its pruning an initial one or none) of
/// one feed per result relation, each a heap range or index scan of that relation under filters and projections into
/// the union's slot. PostgreSQL's ModifyTable takes such a row's result relation from its junk tableoid
/// (ExecLookupResultRelByOid) and its position from the junk ctid; an EvalPlanQual recheck substitutes that relation's
/// scan with the newest version and leaves every other result relation's scan empty (EvalPlanQualStart's
/// relsubs_blocked). The feeds by target index (`targets` order), else `None`.
pub(crate) fn union_targets<'g>(gx: &Gx<'g, '_>, k: usize, w: &WriteDesc<'_>) -> Option<PgVec<'g, usize>> {
    let ps = gx.pipelines();
    // D2015 NEST-ARMS: or the result relations' arms of a union a nest's inner embeds (merge/nest_arms.rs)
    if let Some(arms) = crate::ops::modifytable::merge::nest_arms::arms(gx, k, w) {
        return Some(arms);
    }
    // SORT-UNION (merge#364 group): or the result relations' arms streamed into a merge join's inner Sort
    if let Some(arms) = sorted_arms(gx, k, w) {
        return Some(arms);
    }
    // writesubplan (merge#368 group): or the result relations' arms probing the hashed source, written in the drain's
    // spooled form (hashjoin/drain/lower.rs `merge_drained_arms`)
    if let Some(arms) = gx.merge_drained_arms(k) {
        return Some(arms);
    }
    // TABLEOID-HASH (merge#425): or the union a hash build reads, the write's pipeline probing it (`hashed_union`).
    // D3290: an UPDATE's or DELETE's probing pipeline may read a COPY-marked row first (a data-modifying CTE's CteScan,
    // copy_row.rs's entry): EvalPlanQual hands it the stored row and probes the rebuilt Hash with it
    let reads = |q: usize| ps[q].ops.first().is_some_and(|o| crate::graph::step_kind(o) == KindId::SrcAppend && !crate::graph::is_arm(o));
    let copied = || w.merge.is_none() && gx.graph.record_rows.iter().any(|r| crate::rerun::epq::marks::copied(gx, r.scanrelid) && crate::rerun::epq::copy_row::boundary(gx, k, r).is_some());
    // D3483 / D3673: or the union a drained write's hash build reads (hashjoin/drain/lower.rs `hashed_union_build`)
    let k = match (gx.hashed_union_build(k), &ps.get(k)?.source) {
        (Some(b), _) => b,
        (None, Source::Breaker { .. }) if reads(k) || !copied() => k,
        _ => hashed_union(gx, k)?,
    };
    crate::rerun::union_arms::target_feeds(gx, k, w)
}

/// TABLEOID-HASH (merge#425): PostgreSQL 18.6 plans a MERGE of several result relations joined to a larger
/// source as a hash join, the source's scan probing a Hash over the Append of the result relations' scans; a row's
/// result relation is its junk tableoid as in the union form, and EvalPlanQual substitutes that relation's scan with the
/// newest version (the others empty), rebuilds the Hash over it and probes it with the source's refetched row (D3290: an
/// UPDATE's or DELETE's, eval-plan-qual complexpartupdate, with the row a COPY mark stores for its CteScan). Here the
/// union settles under the build (decompose append.rs `settle_arms`: no arm copies a Build), so pipeline `k` (no nest,
/// one INNER probe before its write) probes the build pipeline that reads the union first under filters and projections
/// into its Build: that pipeline, for `union_targets`'s checks; the recheck reruns the feeds (the row's own at the
/// newest version, merge/lower.rs `rel`) and then the build (marks.rs: a producer whose every slot a rerun producer makes).
fn hashed_union(gx: &Gx<'_, '_>, k: usize) -> Option<usize> {
    let ps = gx.pipelines();
    let pre = || ps[k].ops.iter().take_while(|o| !matches!(o, Op::Write(_)));
    if pre().any(|o| matches!(o, Op::Join(JoinDesc::Nest { .. } | JoinDesc::Merge { .. } | JoinDesc::Drain { .. }))) {
        return None;
    }
    let mut probes = pre().filter_map(|o| match o { Op::Join(JoinDesc::Probe { slot, jointype: JoinType::JOIN_INNER, .. }) => Some(*slot), _ => None });
    let (Some(slot), None) = (probes.next(), probes.next()) else { return None };
    if pre().any(|o| matches!(o, Op::Join(JoinDesc::Probe { jointype, .. }) if *jointype != JoinType::JOIN_INNER)) {
        return None;
    }
    let b = (0..ps.len()).find(|&q| q != k && ps[q].desc.produces() == Some(slot))?;
    let (last, body) = ps[b].ops.split_last()?;
    let simple = body.iter().skip(1).all(|o| matches!(o, Op::Filter(_) | Op::Project(_)));
    (matches!(last, Op::Join(JoinDesc::Build { .. })) && simple && matches!(ps[b].source, Source::Breaker { .. })).then_some(b)
}

/// SORT-UNION (merge#364 group): PostgreSQL 18.6 plans a MERGE of several result relations with a NOT MATCHED
/// action over tables never analyzed as a merge join, the source's Sort the outer and a Sort over the Append of the result
/// relations' scans the inner (Merge Left Join; Merge Full Join beside NOT MATCHED BY SOURCE). Decompose streams the
/// Append's arms into that Sort (each arm its relation's scan under filters and projections, then the Sort into the
/// merge's inner slot), so pipeline `k` (its one join that merge join, INNER / LEFT / FULL; no nest, probe or drain)
/// reads the target rows out of the arms' Sort: a row's result relation is its junk tableoid as in the union form, and
/// EvalPlanQual substitutes that relation's scan with the newest version (the others empty), re-sorts and merges it
/// with the source's refetched row (marks.rs D1249: the target arms and the source's sorted producer rerun). mergecmd
/// UNION-MULTIPAGE: or, an INNER merge join (MATCHED actions only), the arms' Sort the outer, pipeline `k`'s own source,
/// and the source's Sort the merge's slot (PostgreSQL's cheaper order there; the recheck reruns the same producers). The
/// arms by target index (`targets` order), else `None`.
fn sorted_arms<'g>(gx: &Gx<'g, '_>, k: usize, w: &WriteDesc<'_>) -> Option<PgVec<'g, usize>> {
    let ps = gx.pipelines();
    let Source::Breaker { slot: outer } = ps.get(k)?.source else { return None };
    if w.targets.len() < 2 || w.tableoid_col <= 0 {
        return None;
    }
    let pre = || ps[k].ops.iter().take_while(|o| !matches!(o, Op::Write(_)));
    if pre().any(|o| matches!(o, Op::Join(JoinDesc::Nest { .. } | JoinDesc::Probe { .. } | JoinDesc::Drain { .. }))) {
        return None;
    }
    let mut merges = pre().filter_map(|o| match o { Op::Join(JoinDesc::Merge { slot, jointype, .. }) => Some((*slot, *jointype)), _ => None });
    let (Some((slot, jt)), None) = (merges.next(), merges.next()) else { return None };
    if !matches!(jt, JoinType::JOIN_INNER | JoinType::JOIN_LEFT | JoinType::JOIN_FULL) {
        return None;
    }
    // the arms that sort into `slot`, by target index: each its relation's heap scan under filters and projections, one arm op
    let sorted_into = |slot: u8| -> Option<PgVec<'g, usize>> {
        let mut by = mcx::pg_vec![in gx.mcx; usize::MAX; w.targets.len()];
        for q in (0..ps.len()).filter(|&q| q != k && ps[q].desc.produces() == Some(slot)) {
            let p = &ps[q];
            let (rti, relid) = match &p.source {
                Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } => (u32::from(*scanrelid), *relid),
                _ => return None,
            };
            let (last, body) = p.ops.split_last()?;
            let arms = body.iter().filter(|o| crate::graph::is_arm(o)).count();
            if !matches!(last, Op::Sort(_)) || arms != 1 || !p.desc.consumes.is_empty() || !body.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_)) || crate::graph::is_arm(o)) {
                return None;
            }
            let i = w.targets.iter().position(|t| t.rti == rti && t.relid == relid)?;
            if by[i] != usize::MAX {
                return None;
            }
            by[i] = q;
        }
        by.iter().all(|&q| q != usize::MAX).then_some(by)
    };
    sorted_into(slot).or_else(|| if jt == JoinType::JOIN_INNER { sorted_into(outer) } else { None })
}

/// SORT-UNION: the root `(relid, natts)` a NOT MATCHED INSERT beside the UPDATE / DELETE actions of the
/// `sorted_arms` form inserts into (ExecInitMerge's rootRelInfo: the root's layout and tuple routing, hosts
/// MultiTargetHost's route leg), else `None`.
pub(crate) fn insert_beside(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> Option<(u32, usize)> {
    // a partitioned root, or (writesubplan, merge#514) an inheritance root, a plain heap: its INSERT is the root's own
    // (ExecInitMerge's rootRelInfo over the root itself: no tuple routing, the root's row triggers fire, as merge.sql's
    // measurement tree's BEFORE INSERT trigger redirecting the row), hosts RouteHost's own-root leg
    let r = w.route.as_ref().filter(|r| gx.facts.rel(r.root_relid).is_some_and(|f| f.relkind == b'p' || f.relkind == b'r' && f.am == Am::Heap))?;
    let inserts = w.merge.as_ref().is_some_and(|m| !insert_only(m) && m.actions.iter().any(|a| a.cmd == Some(Cmd::Insert)));
    // writesubplan (updatable_views#961-963): or the one result relation's drained form (drain's `merge_drained_target`:
    // the leaf's own scan probes the hashed source, the NOT MATCHED rows drained after), MultiTargetHost over that leaf
    // mergecmd (held-out tw-part-022): or that leaf over a nested loop (nest_write.rs `merge`: W6's F1, a one-row Result
    // outer and the leaf's own scan the inner, LEFT; the position the leaf's junk ctid, its recheck the inner's site)
    let nest = gx.pipelines()[k].ops.iter().take_while(|o| !matches!(o, Op::Write(_))).any(|o| matches!(o, Op::Join(JoinDesc::Nest { .. })));
    let one = w.targets.len() == 1 && (gx.merge_drained_target(k).is_some() || nest);
    // writesubplan (merge#368 group): or the drained arms' union form (`union_targets`), as SORT-UNION's; writesubplan
    // D2015 NEST-ARMS: or the arms of a union a nest's inner embeds (merge/nest_arms.rs)
    // D3483: or the drained form over the result relations' hashed union (`hashed_union_build`)
    let arms = gx.merge_drained_arms(k).is_some() || crate::ops::modifytable::merge::nest_arms::arms(gx, k, w).is_some() || gx.hashed_union_build(k).is_some();
    (sorted_arms(gx, k, w).is_some() || one || arms).then_some(()).filter(|_| inserts).map(|_| (r.root_relid, r.root_natts as usize))
}

/// The relations whose triggers a routed MERGE's UPDATE / DELETE fires besides its first target (`transition`): the
/// root and every result relation; none for an unrouted or V4R-498 INSERT-only MERGE (route.rs admits its leaves).
fn relids<'g>(w: &WriteDesc<'_>, mcx: Mcx<'g>) -> PgVec<'g, u32> {
    let on = w.route.is_some() && w.merge.as_ref().is_some_and(|m| !insert_only(m));
    w.route.iter().map(|r| r.root_relid).chain(w.targets.iter().map(|t| t.relid)).filter(|_| on).collect_in(mcx)
}

/// merge.rs's write.transition: its plan refuses by name what the host does not fire (INSTEAD OF, unknown facts), over
/// the first target's triggers and, for write.route's UPDATE / DELETE forms, the root's and every result relation's.
pub(crate) fn transition(px: &mut Px<'_, '_>, k: usize, j: usize, relid: u32, w: &WriteDesc<'_>) {
    let (gx, node) = (px.gx, px.gx.pipelines()[k].node_id);
    if core::iter::once(relid).chain(relids(w, px.mcx())).any(|r| fires(gx.facts.trig_ext(r))) {
        drop((crate::frame::iface::kind_impl::<StepMaker>(&node, KindId::WriteTransition).0)(px, k, j));
    }
}

/// merge.rs's row op: its relation, cell count, plan and (b)'s `pick`. V4R-498's INSERT-only form routes its INSERTs
/// (write.route `Route::plan`); (a) names the root with no leaves; (b) is the arm's target under the plan write.route
/// routed over every target (`TargetsRoad`), its cells the updated columns (`pick`); unrouted, as given.
pub(crate) fn plan<'g>(px: &mut Px<'g, '_>, k: usize, (w, m): (&WriteDesc<'_>, &MergeDesc<'_>), (relid, natts): (u32, usize), plan: WritePlan) -> (u32, usize, WritePlan, Option<PgVec<'g, usize>>) {
    let (gx, node) = (px.gx, px.gx.pipelines()[k].node_id);
    // INHERIT-ROOT: the root's own INSERT (no route, no indexes: `inherit_root` refused a root with any)
    if let Some((root, root_natts)) = inherit_root(gx, w, m, node) {
        // its RLS checks are the first result relation's list, which the host maps onto the root's attnos (`checks_of`)
        let checks_of = if w.checks.is_empty() || w.targets[0].relid == root { 0 } else { w.targets[0].relid };
        return (root, root_natts, WritePlan { target_rel: root, indexes: ArenaVec::new_in(px.arena), route: None, checks_of, ..plan }, None);
    }
    if crate::ops::modifytable::merge::nm::routed(w).is_some() {
        return (relid, natts, crate::ops::modifytable::merge::nm::route_plan(gx, node, w, plan, px.arena), None);
    }
    let Some(r) = w.route.as_ref() else { return (relid, natts, plan, None) };
    // MOVE: a row that may leave its leaf gets write.route's destinations (`Route::moves`, as write/update.rs's
    // `moving`): the host deletes it from its leaf and inserts it through the root (hosts write_move.rs)
    // (D3367: a root pruned to no leaf moves no row, its dummy Result yields none, as write/update.rs's)
    let pruned = crate::ops::shared::dummy::pruned_root(gx, k, w);
    let moves = (!pruned && moving(gx, w, r)).then(|| crate::frame::iface::need_strategy::<Route>(&node, WRITE_ROUTE).moves(gx, node, w));
    let moved = |p: WritePlan| match moves.as_deref() {
        Some(m) => p.with_moves(px.arena, m),
        None => p,
    };
    if w.targets.len() == 1 {
        // writesubplan (updatable_views#961-963): beside the routed NOT MATCHED INSERT the plan names the one leaf, so the
        // host is MultiTargetHost (its route leg inserts through the root); the site is the root's row, the SET cells
        // (`pick`) first, as SORT-UNION's below. D3479: under RETURNING also as wide as the leaf's row, which the host hands
        // back for a MATCHED action (`ret_width`)
        if let Some((_, root_natts)) = insert_beside(gx, k, w) {
            let Some(pick) = pick(w, m, natts, px.mcx()) else { crate::refuse!(node, Reason::Bug { payload: "a routed MERGE's updated column the leaf does not map to the root, or a generated one" }) };
            let leaves = crate::ops::modifytable::strategies::route::leaves(gx, w);
            let checks_of = if w.checks.is_empty() { 0 } else { w.targets[0].relid };
            let routed = moved(plan.with_route(px.arena, r.root_relid, &leaves));
            let wide = ret_width(gx, k, w).unwrap_or(0);
            return (relid, root_natts.max(pick.len()).max(wide), WritePlan { checks_of, ..routed }, Some(pick));
        }
        return (relid, natts, moved(plan.with_route(px.arena, r.root_relid, &[])), None);
    }
    let (_, arm, routed) = (crate::frame::iface::need_strategy::<TargetsRoad>(&node, WRITE_ROUTE).0)(gx, k, w, px.arena);
    let mut routed = moved(routed);
    let Some(pick) = pick(w, m, natts, px.mcx()) else { crate::refuse!(node, Reason::Bug { payload: "a routed MERGE's updated column the first target does not map to the root, or a generated one" }) };
    // G2(c): the union form is no Append arm, so write/rowop.rs `claim` adds no leaves: the plan's are every
    // target's, for the multi-target host (hosts write_route.rs), here
    let union = union_targets(gx, k, w).is_some();
    if let (true, Some(r)) = (union, routed.route.as_mut()) {
        let leaves = crate::ops::modifytable::strategies::route::leaves(gx, w);
        r.leaves = ArenaVec::with_capacity_in(leaves.len(), px.arena);
        r.leaves.extend_from_slice(&leaves);
    }
    // G2(c), RETURNING under the union form: the site as wide as the widest result relation, whose first cells
    // the row's own relation's host fills with the deleted version (hosts MultiTargetHost write_pg, R5's DELETE form;
    // merge/lower.rs `per_leaf` maps them onto the first relation's attnos). writesubplan (gen mg-0178): an UPDATE's SET
    // cells come first (merge/lower.rs `pad`), and the host hands its new row back in its relation's layout (MultiTargetHost
    // `written`: the row it formed over the old version, ExecGetUpdateNewTuple's); beside the routed INSERT the site is
    // also as wide as the root's row, which the route leg hands back (merge/lower.rs `ret_at` reads it at the first result
    // relation's attnos), its check options the first result relation's (G2(d), as below)
    // The routed plan takes the statement plan's RETURNING fetches (OLD's replaced version, the written row's system
    // columns: merge/lower.rs `plan`), which write.route's plan (`TargetsRoad`, the decomposed one) does not carry.
    // (gen mg-0179: an arm of the (b) form likewise, the row its own relation's, which its RETURNING projects)
    if let Some(wide) = ret_width(gx, k, w) {
        let wide = wide.max(pick.len());
        let routed = WritePlan { old: plan.old, sys: plan.sys, sys_header: plan.sys_header, ..routed };
        if let Some((_, root_natts)) = insert_beside(gx, k, w) {
            let checks_of = if w.checks.is_empty() { 0 } else { w.targets[0].relid };
            return (arm, wide.max(root_natts), WritePlan { checks_of, ..routed }, Some(pick));
        }
        return (arm, wide, routed, Some(pick));
    }
    // SORT-UNION: beside the routed INSERT the site is the root's row (the INSERT's image); an UPDATE's SET cells
    // come first, the rest NULL (merge/lower.rs `image`; the host reads the statement's updated columns). Its check
    // options: the first result relation's list mapped onto each routed leaf (G2(d), nm.rs `route_plan`)
    if let Some((_, root_natts)) = insert_beside(gx, k, w) {
        let checks_of = if w.checks.is_empty() { 0 } else { w.targets[0].relid };
        return (arm, root_natts.max(pick.len()), WritePlan { checks_of, ..routed }, Some(pick));
    }
    (arm, pick.len(), routed, Some(pick))
}

/// writesubplan (gen mg-0178): RETURNING over the union form (`union_targets`): the widest result relation's attribute
/// count, the least the row op's site holds (each row's own relation's cells, an UPDATE's new row or a DELETE's deleted
/// version, the host hands back first). writesubplan (gen mg-0179): or of an arm of the (b) form (`b_arm`), whose host
/// hands back its own result relation's row. D2015 UNION-OUTER (held-out parts-0083): or a MERGE that rechecks in the IR
/// through its union outer's first arm (nest_arms.rs `first`), whose WRITE_EPQ_ROW puts the row's own result relation's
/// newest version in the site, in that relation's layout (write/rowop_epq.rs `epq_width`). D3479: or RETURNING over the
/// one leaf's form beside the routed INSERT (`insert_beside`), whose host hands back the leaf's own row for a MATCHED
/// action (MultiTargetHost `written`) and the root's for the INSERT (merge/lower.rs `ret_at` reads it at the leaf's
/// attnos). `None` for every other MERGE.
pub(crate) fn ret_width(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> Option<usize> {
    let one = || w.targets.len() == 1 && insert_beside(gx, k, w).is_some();
    let several = w.returning.is_some() && (union_targets(gx, k, w).is_some() || b_arm(gx, k, w) || one());
    let redo = || crate::ops::modifytable::merge::nest_arms::first(gx, k, w).is_some();
    (several || redo()).then(|| w.targets.iter().map(|t| t.site.natts()).max().unwrap_or(0))
}

/// writesubplan (gen mg-0179): pipeline `k`'s write of several result relations through a root is the (b) form's (`admit`):
/// an arm of the Append it streams into, over one result relation's own heap scan, no nested loop before it.
pub(crate) fn b_arm(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> bool {
    let p = &gx.pipelines()[k];
    let wi = p.ops.iter().position(|o| matches!(o, Op::Write(_))).unwrap_or(p.ops.len());
    let own = |t: &Target<'_>| matches!(&p.source, Source::Range { am, scanrelid, relid, .. } | Source::Index { am, scanrelid, relid, .. } | Source::Bitmap { am, scanrelid, relid, .. } | Source::Tid { am, scanrelid, relid, .. }
        if *am == Am::Heap && u32::from(*scanrelid) == t.rti && *relid == t.relid);
    w.targets.len() > 1 && w.route.is_some() && crate::ops::modifytable::merge::nm::routed(w).is_none()
        && p.ops[..wi].iter().any(crate::graph::is_arm)
        && !p.ops[..wi].iter().any(|o| matches!(o, Op::Join(JoinDesc::Nest { .. })))
        && w.targets.iter().any(own)
}

/// merge.rs's UPDATE image under (b): the cells `pick` names, in its order (the host's `set`); every cell otherwise.
pub(crate) fn picked<'e>(cells: PgVec<'e, (Value, Value)>, pick: Option<&[usize]>, mcx: Mcx<'e>) -> PgVec<'e, (Value, Value)> {
    match pick {
        Some(p) => p.iter().map(|&i| cells[i]).collect_in(mcx),
        None => cells,
    }
}

/// (b)'s cells: the statement's updated columns (every UPDATE action's SET columns: the rewriter's updateColnos, which
/// leave out generated columns, as hosts `MultiTargetHost` leaves them out of updatedCols) in the root's attno order,
/// each as its index among the first target's attributes (write/merge.rs's image); `None`: a column the first target
/// does not map to the root, or a generated one.
fn pick<'g>(w: &WriteDesc<'_>, m: &MergeDesc<'_>, natts: usize, mcx: Mcx<'g>) -> Option<PgVec<'g, usize>> {
    let t = &w.targets[0];
    let sets: PgVec<'_, PgVec<'_, i16>> = m.actions.iter().filter(|a| a.cmd == Some(Cmd::Update)).map(|a| a.set.iter().flat_map(|s| s.iter()).map(|x| x as i16).collect_in(mcx)).collect_in(mcx);
    let generated: PgVec<'_, i16> = t.generated.iter().map(|g| g.0).collect_in(mcx);
    cells_of(&sets[..], &|a| root_attno(t, a), &generated[..], natts, mcx)
}

/// `pick`'s rule over the first target's SET attnos per UPDATE action, its attno -> root attno map (0: none) and its
/// generated attnos: the root attnos, unique and ascending, each as the index of the first target's attribute that maps
/// to it.
fn cells_of<'g, S: AsRef<[i16]>>(sets: &[S], root: &dyn Fn(i16) -> i16, generated: &[i16], natts: usize, mcx: Mcx<'g>) -> Option<PgVec<'g, usize>> {
    let mut r: PgVec<'_, i16> = PgVec::new_in(mcx);
    for &a in sets.iter().flat_map(|s| s.as_ref().iter()) {
        if generated.contains(&a) {
            return None;
        }
        r.push_in(root(a));
    }
    r.sort_unstable();
    r.dedup();
    let mut out = mcx::reserved_in(r.len(), mcx);
    for &x in r.iter() {
        out.push_in((1..=natts as i16).position(|a| x > 0 && root(a) == x)?);
    }
    Some(out)
}

/// INHERIT-ROOT (merge.sql's 'MERGE into inheritance root table'): an INSERT-only MERGE (`insert_only`)
/// through an inheritance root (`rootRelation` a plain heap table: decompose's member Targets) with one result relation.
/// PostgreSQL 18.6 inserts into the root itself (ExecInitMerge's rootRelInfo: no tuple routing, the root's own
/// constraints, a NO INHERIT one too), the result relation's rows only telling a matched source row from a not-matched
/// one (its ctid's NULL, as V4R-498's form reads it). `(root relid, root natts)`, or `None` for every other MERGE; a root
/// with indexes (or facts not readable now), triggers or a view's check options refuses by name.
fn inherit_root(gx: &Gx<'_, '_>, w: &WriteDesc<'_>, m: &MergeDesc<'_>, node: i32) -> Option<(u32, usize)> {
    let r = w.route.as_ref().filter(|r| insert_only(m) && !r.cross_partition && w.targets.len() == 1 && w.members.has(Member::Targets) && !w.members.has(Member::Route) && !w.members.has(Member::CrossPartition))?;
    gx.facts.rel(r.root_relid).filter(|f| f.relkind == b'r' && f.am == Am::Heap)?;
    let mut n = 0usize;
    if !gx.facts.indexes_all(r.root_relid, &mut |_| n += 1) || n > 0 {
        shape(node, INHERIT_INDEXED);
    }
    // the root's (its INSERT's row and statement triggers) and the result relation's (write.transition plans over it)
    // (their trigger lists: pg_class.relhastriggers outlives a dropped trigger, as merge.sql's does here)
    let any = |relid: u32| fires(gx.facts.trig_ext(relid));
    if any(r.root_relid) || any(w.targets[0].relid) {
        shape(node, INHERIT_TRIGGERS);
    }
    // its RLS checks (write.checks' kinds, one target) are the first result relation's list mapped onto the root's attnos
    // by name (ExecInitMerge's rootRelInfo->ri_WithCheckOptions; hosts write.rs `checks_onto`); a view's is no such root's
    if w.members.has(Member::CheckView) {
        shape(node, INHERIT_CHECKS);
    }
    // RETURNING reads the root's written row at the first result relation's attnos (nm.rs `returned`, its `to_root`)
    Some((r.root_relid, r.root_natts as usize))
}

/// N1: a row that may leave its leaf (write/update.rs `moving`): a partition key column updated, a BEFORE ROW UPDATE
/// trigger's NEW (D687), an ancestor's key (D820, the root itself a partition), or facts the source could not answer. An
/// inheritance tree routes no row.
fn moving(gx: &Gx<'_, '_>, w: &WriteDesc<'_>, r: &RouteDesc) -> bool {
    let may = |t: TrigExt| !t.known || t.before_row & TRIG_UPDATE != 0;
    let root = gx.facts.rel_ext(r.root_relid);
    w.members.has(Member::Route) && (r.cross_partition || !w.leaves_known || !root.known || root.is_partition || may(gx.facts.trig_ext(r.root_relid)) || w.leaves.iter().any(|l| l.has_triggers && may(l.trig)))
}

#[cfg(test)]
mod tests {
    /// `super::cells_of` over a test context, its answer copied out.
    fn cells_of(sets: &[Vec<i16>], root: &dyn Fn(i16) -> i16, generated: &[i16], natts: usize) -> Option<Vec<usize>> {
        super::cells_of(sets, root, generated, natts, mcx::MemoryContext::new("test").mcx()).map(|v| v.to_vec())
    }

    /// The cells are the root attnos every UPDATE action sets, unique and ascending, whatever each action's SET order
    /// (hosts MultiTargetHost's `set`: updatedCols in attno order), each at the first target's attribute for it.
    #[test]
    fn cells_are_the_updated_root_columns_ascending() {
        let id = |a: i16| a;
        assert_eq!(cells_of(&[vec![3, 1], vec![2, 3]], &id, &[], 4), Some(vec![0, 1, 2]));
        assert_eq!(cells_of(&[vec![4]], &id, &[], 4), Some(vec![3]));
        assert_eq!(cells_of(&[], &id, &[], 4), Some(vec![]));
    }

    /// A first target of another column order (an ATTACHed leaf) or with a dropped column: each root column's cell is
    /// the leaf attribute that maps to it, in the root's order.
    #[test]
    fn a_reordered_or_dropped_first_target_maps_by_root_attno() {
        let reordered = |a: i16| [3, 1, 2].get(a as usize - 1).copied().unwrap_or(0);
        assert_eq!(cells_of(&[vec![1], vec![2]], &reordered, &[], 3), Some(vec![1, 0]));
        let dropped = |a: i16| [1, 0, 2].get(a as usize - 1).copied().unwrap_or(0);
        assert_eq!(cells_of(&[vec![3, 1]], &dropped, &[], 3), Some(vec![0, 2]));
    }

    /// A generated column (the host leaves it out of its `set`) or a column with no root attno refuses.
    #[test]
    fn generated_or_unmapped_refuses() {
        let id = |a: i16| a;
        assert_eq!(cells_of(&[vec![2]], &id, &[2], 3), None);
        let none = |_: i16| 0;
        assert_eq!(cells_of(&[vec![1]], &none, &[], 3), None);
    }
}
