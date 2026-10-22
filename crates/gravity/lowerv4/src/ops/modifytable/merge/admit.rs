//! Kind `write.merge`'s admission (split from write/merge.rs: each file under its 300-line budget). Owner
//! M4-MERGE. `admit` holds every refusal by name of write/merge.rs's module doc and returns the target relation when the
//! MERGE serves (and D776's target producer when the planner hashed the target); `fires` asks whether a trigger on one of MERGE's commands fires; `shape` / `refuse_kind` are the two
//! refusal forms both files use.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::ops::shared::srf::SrfPlace;
use gravity_decompose::ops::modifytable::trig_facts::{cmd_bits, TrigExt};
use gravity_decompose::ops::shared::modifytable::{MergeDesc, Member, WriteDesc};
use gravity_decompose::{Am, Op, Source};
use gravity_program::Cmd;
use types_nodes::jointype::JoinType;
use types_nodes::plannodes::RowMarkType;
use types_nodes::primnodes::{MergeMatchKind, INNER_VAR};
use types_nodes::{Node, NodeTag};
use mcx::{CollectIn, PgVec};

use crate::face::end::{ChecksAt, RowOpRoad};
use crate::face::kinds::KindId;
use crate::face::strategy::WRITE_CHECKS;
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy, Variant};

use crate::ops::modifytable::strategies::checks_ir::Checks;

pub(crate) fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// M7 B1 (I47): a MERGE whose join hands on NOT MATCHED rows in a form not served (a LEFT join whose nullable
/// side is not the one its NOT MATCHED kind names, several, a SEMI / ANTI probe). W6 serves BY TARGET, V4R-495 BY SOURCE.
const NOT_MATCHED: &str = "a MERGE whose join hands on NOT MATCHED rows (write.merge serves the matched rows only)";

/// RIGHT-MERGE: the RIGHT merge join's outer the target's own scan beside a MATCHED action, the target without
/// its junk ctid (merge/lower.rs tells the inner fill's NOT MATCHED rows, which have no position, by its NULL).
const RIGHT_OWN: &str = "a MERGE over a RIGHT merge join whose outer is the target's own scan, beside a MATCHED action (the inner fill's rows have no position)";

fn refuse_kind(node: i32, k: KindId) -> ! {
    crate::not_built!(node, Variant::Kind(k))
}

/// A trigger on one of MERGE's commands (statement triggers fire even at zero rows), or unknown facts.
pub(crate) fn fires(t: TrigExt) -> bool {
    !t.known || (t.before_row | t.after_row | t.instead_row | t.statement | t.transition) & cmd_bits(Cmd::Merge) != 0
}

/// Every refusal by name of the module doc; the target relation when the MERGE serves, and (D776) the producer pipeline
/// that scans the target when the planner hashed it (the emit scans the REFERENCE-marked source).
pub(crate) fn admit(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>, m: &MergeDesc<'_>) -> (u32, Option<usize>) {
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    for mb in w.members.iter() {
        match mb {
            // D883: a whole-row reference to the target: the decomposer carries every live column (write/merge.rs `full`)
            Member::Merge | Member::Insert | Member::Update | Member::Delete | Member::Generated | Member::GeneratedVirtual | Member::WholeRow => {}
            Member::IndexUnique | Member::IndexPlain | Member::IndexDeferrable | Member::IndexExclusion | Member::IndexPartial | Member::IndexExpression | Member::IndexInclude | Member::IndexNonBtree => {}
            // D789: write.returning's continuation after each action (write/merge.rs MergeStep); its shapes refuse there
            Member::Returning | Member::ReturningOld | Member::ReturningNew | Member::MergeActionFn => {}
            // D1112 (nest.subplan B4's MERGE part): a SubPlan only MERGE actions read is lent lazily at its
            // action's read (subplan per_row.rs, decompose `attach`: conditional), so the action evaluates it per row as
            // ExecMergeMatched does; RETURNING's, a check option's or a hashed one keep B4 by name
            Member::WriteSubPlan if action_subs(p, w) => {}
            Member::WriteSubPlan => crate::refuse!(node, Reason::Shape { why: ShapeWhy("a SubPlan in a write's own expressions, a check, RETURNING, ON CONFLICT or a MERGE action (D1112: nest.subplan B4)") }),
            // R2 (D655): a joined source's REFERENCE mark (write/rowop.rs `claim`, marks.rs); a parent's is M7's (D1000)
            Member::WriteMarks => {}
            Member::ParentMark => crate::rerun::epq::marks::parent_mark(node, w.mark_descs),
            // V4R-353: a view target's INSTEAD OF row triggers are the write (write/merge_view.rs, hosts H-f)
            Member::TargetView => crate::ops::modifytable::merge::view::admit(node, w),
            Member::Route | Member::Targets | Member::CrossPartition => {} // D1082: merge/route.rs `admit` (below)
            Member::CheckView | Member::CheckRls => RowOpRoad::Checks.require(&node),
            Member::TargetForeign | Member::TargetOtherAm | Member::TargetAm | Member::DirectForeign => crate::refuse!(node, Reason::UnsupportedAm { relid: w.plan.target_rel }),
            Member::ConflictNothing | Member::ConflictUpdate => crate::refuse!(node, Reason::Bug { payload: "a MERGE carrying an ON CONFLICT member" }),
            // PG 18 never plans a locking mark on a MERGE (planner.c:2238)
            Member::LockingWriteMark | Member::MultiLock => crate::refuse!(node, Reason::Bug { payload: "a MERGE carrying a locking row mark" }),
        }
    }
    // the planner's dummy relation (its row never comes; D829's root pruned to no leaf among them: merge/route.rs)
    let dummy = crate::ops::shared::dummy::dummy(gx, k, p.ops.len());
    let at = crate::ops::modifytable::merge::route::admit(gx, k, (w, m), node, dummy); // W6-MERGE-ROUTE-MATCHED: write.route's forms, else by name
    // V4R-495: BY SOURCE plans target LEFT JOIN source, its join condition splitting MATCHED from BY SOURCE (merge.rs).
    // DO-NOTHING-JOIN: only an action with a command counts, as PostgreSQL 18.6's transform_MERGE_to_join counts
    // no CMD_NOTHING action: a BY SOURCE DO NOTHING plans the join without it (its rows never null-extend the source; a
    // recheck whose join condition fails runs the BY SOURCE list, DO NOTHING, as ExecMergeMatched does: lower.rs `split`)
    let bs = m.actions.iter().any(|a| a.kind == MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_SOURCE && a.cmd.is_some());
    let relid = w.plan.target_rel;
    let view = w.members.has(Member::TargetView);
    if !view && w.route.is_none() && gx.facts.rel(relid).is_some_and(|r| r.am != Am::Heap) {
        crate::refuse!(node, Reason::UnsupportedAm { relid });
    }
    if !view && m.actions.iter().any(|a| a.cmd == Some(Cmd::Update)) && (!w.plan.indexes().is_empty() || !w.targets[0].indexes.is_empty()) {
        RowOpRoad::IdxUnchanged.require(&node);
    }
    if crate::frame::iface::need_strategy::<Checks>(&node, WRITE_CHECKS).plan(gx, node, w) != ChecksAt::Host {
        crate::refuse!(node, Reason::Bug { payload: "an IR checks plan at M4's first landing" });
    }
    // the join a NOT MATCHED action plans (source LEFT JOIN target: W6's, the target hashed or sorted on the nullable side)
    // and every other non-inner form, by its kind
    // DO-NOTHING-JOIN: the NOT MATCHED actions merge/nm.rs `actions` lowers (an INSERT, or a DO NOTHING whose quals may
    // raise or call: a concurrent delete's fall-through runs them), each over the target's junk ctid; DO NOTHING alone
    // over quiet quals (D3470: nm.rs `idle`) plans no outer join for them (transform_MERGE_to_join) and lowers no arm
    let nm = !crate::ops::modifytable::merge::nm::idle(gx, m);
    let left = |o: &Op<'_>| matches!(o, Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_LEFT, .. } | JoinDesc::Merge { jointype: JoinType::JOIN_LEFT, .. }));
    let full = |o: &Op<'_>| matches!(o, Op::Join(JoinDesc::Merge { jointype: JoinType::JOIN_FULL, .. }));
    // RIGHT-MERGE (merge#468 group): the planner's commuted form, the target RIGHT JOIN the source as a merge join
    // (the target's Sort the outer, the nullable side; every row of the source, the inner, comes out, an unmatched one by
    // the inner fill with a NULL target), for NOT MATCHED [BY TARGET] actions only (BY SOURCE plans the target LEFT JOIN
    // the source). The nullable side is pipeline `k`'s own source (checked below with the LEFT forms: `lefts`)
    let right = |o: &Op<'_>| matches!(o, Op::Join(JoinDesc::Merge { jointype: JoinType::JOIN_RIGHT, .. }));
    for o in p.ops.iter().take_while(|o| !matches!(o, Op::Write(_))) {
        match o {
            Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_INNER, .. }) => {}
            // W6: the planner's source LEFT JOIN target of a NOT MATCHED [BY TARGET] action (the target the nullable side)
            Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_LEFT, .. }) if nm || bs => {}
            Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_LEFT | JoinType::JOIN_SEMI | JoinType::JOIN_ANTI, .. }) => shape(node, NOT_MATCHED),
            Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_FULL, .. }) => refuse_kind(node, KindId::JoinFull),
            Op::Join(JoinDesc::Probe { .. }) => refuse_kind(node, KindId::JoinRight),
            // a merge join by its jointype, as the probes (M8-WRITE W1): INNER on to the own-rows / chain checks below
            Op::Join(JoinDesc::Merge { jointype: JoinType::JOIN_INNER, .. }) => {}
            Op::Join(JoinDesc::Merge { jointype: JoinType::JOIN_LEFT, .. }) if nm || bs => {}
            Op::Join(JoinDesc::Merge { jointype: JoinType::JOIN_LEFT | JoinType::JOIN_SEMI | JoinType::JOIN_ANTI, .. }) => shape(node, NOT_MATCHED),
            // SORT-UNION part 2 (merge#364 group): NOT MATCHED beside NOT MATCHED BY SOURCE plans the source FULL
            // JOIN the target, the target the merge's inner (checked below with the LEFT forms: `fulls`)
            Op::Join(JoinDesc::Merge { jointype: JoinType::JOIN_FULL, .. }) if nm && bs => {}
            Op::Join(JoinDesc::Merge { jointype: JoinType::JOIN_FULL, .. }) => refuse_kind(node, KindId::JoinFull),
            Op::Join(JoinDesc::Merge { jointype: JoinType::JOIN_RIGHT, .. }) if nm && !bs => {}
            Op::Join(JoinDesc::Merge { .. }) => refuse_kind(node, KindId::JoinRight),
            Op::Join(JoinDesc::Nest { .. }) => {} // D1933: write/nest_write.rs admits it (below) or refuses by name
            _ => {}
        }
    }
    // the target's own scan rows, one to one or joined by inner probes (R2), or the target hashed (D776)
    let own = match &p.source {
        Source::Range { am, scanrelid, relid: r, .. } | Source::Index { am, scanrelid, relid: r, .. } | Source::Bitmap { am, scanrelid, relid: r, .. } | Source::Tid { am, scanrelid, relid: r, .. } => {
            *am == Am::Heap && (u32::from(*scanrelid), *r) == at // W6-MERGE-ROUTE-MATCHED: an arm's own target
        }
        // a Result: only the planner's dummy relation, whose row never comes
        Source::Const => dummy,
        _ => false,
    };
    // A4-MATEND (D2371; hashjoin/drain/lower.rs `merge_drained_target`): written in a spooled drain whose probe scans the
    // target's own heap, the rows are the Drain op's (the probe's joined rows, then the drained source rows); a view
    // target's too (`a2a_write`: its INSTEAD OF row triggers, no position)
    let drained = gx.merge_drained_target(k).is_some() || view && gx.a2a_write(k).is_some() || gx.merge_drained_arms(k).is_some() || gx.hashed_union_build(k).is_some(); // writesubplan (merge#368 group): or the drained arms'
    // D1933 hashed COPY: the ops before a COPY-marked subquery row's entry (`copy_entry`) are the subquery's own;
    // the recheck enters there with the stored row (copy_row.rs) and never runs them, as EvalPlanQual hands the
    // SubqueryScan its stored row, so they need no recheck form of their own. Not past a merge join: its row reads the
    // window the join opens (mergejoin/lower.rs), and the copy entry's call opens none (frame.rs `copy_entry`)
    let entry = own_entry(gx, k, w);
    // G2(c) part 2: the rows of a settled union of the targets' feeds (merge/route.rs `union_targets`), its Union op first
    let union = crate::ops::modifytable::merge::route::union_targets(gx, k, w);
    // D3476: an insert-only MERGE (nest_write.rs `positionless`: it locates and rechecks no target row) reads its source
    // as an INSERT .. SELECT does: a settled source union's Union op heading the emit, or a streamed one's arm marker
    let ins = crate::ops::modifytable::nest_write::positionless(w, m);
    let source = |i: usize, o: &Op<'_>| ins && (i == 0 && matches!(o, Op::Append(AppendDesc::Union { .. })) || crate::graph::is_arm(o));
    // D3477: or a WHEN MATCHED-only MERGE's flattened UNION ALL source settled, its Union op first, probing the hashed
    // target (update_built.rs `copied_settled`; merge/settled.rs its recheck)
    let settled = crate::ops::modifytable::update_built::copied_settled(gx, k, w).is_some();
    let chain = p.ops.iter().take_while(|o| !matches!(o, Op::Write(_))).enumerate().all(|(i, o)| i == 0 && (union.is_some() || settled) || source(i, o) || i < entry && subquery_own(o) || matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_)) || crate::rerun::epq::marks::write_joins(o) || (nm || bs) && left(o) || nm && !bs && right(o) || nm && bs && full(o) || w.targets.len() > 1 && crate::graph::is_arm(o) || drained && matches!(o, Op::Join(JoinDesc::Drain { .. })) || action_sub(o, w) || crate::rerun::epq::marks::join_qual_sub(o)); // W6-MERGE-ROUTE-MATCHED: an arm (merge/route.rs); D1112: an action's lazy SubPlan
    // a view target's rows are any chain's: no ctid, lock or recheck (write/merge_view.rs); D1933: or a nested loop's rows
    let routed = w.route.as_ref().and_then(|_| crate::ops::modifytable::merge::nm::routed_target(gx, k)); // V4R-498
    let (chain, built) = crate::ops::modifytable::nest_write::merge(gx, k, w, m, node).map_or_else(|| (chain, if !own && chain && !view { routed.or_else(|| union.as_ref().map(|f| f[0])).or_else(|| built_target(gx, k, w)) } else { None }), |b| (true, b));
    // mergecmd insert-only nest / D2247 (merge/lower.rs `positionless`): an insert-only MERGE over a nest, or whose write
    // runs in a spooled drain, reads no target position (a MATCHED row has no action, a NOT MATCHED one has none to
    // read), so it needs neither its own scan rows nor a built target
    let free = crate::ops::modifytable::merge::lower::positionless(gx, k, w, m);
    if !free && (!own && !view && built.is_none() || !chain) {
        unbuilt(gx, k, w, node); // D882: by name (M7's write.*)
    }
    // W6: the LEFT join's nullable side is the target's producer (its slot), so a NULL ctid is a source row with no target;
    // RIGHT-MERGE: a RIGHT merge join's is its outer, pipeline `k`'s breaker source (none: no producer's, `u8::MAX`)
    let outer = match p.source { Source::Breaker { slot } => slot, _ => u8::MAX };
    let slot = |o: &Op<'_>| match o { Op::Join(JoinDesc::Probe { slot, .. } | JoinDesc::Merge { slot, .. }) if left(o) => Some(*slot), _ if right(o) => Some(outer), _ => None };
    let lefts: PgVec<'_, u8> = p.ops.iter().take_while(|o| !matches!(o, Op::Write(_))).filter_map(slot).collect_in(gx.mcx);
    // V4R-495: BY SOURCE's LEFT join has the source on the nullable side (the target the outer: its own scan or sorted)
    // RIGHT-MERGE: or the RIGHT merge join's outer the target's own scan (the inner fill's rows are NULL in the junk ctid:
    // an insert-only MERGE, `free`, reads no position, merge/lower.rs `positionless`). mergecmd RIGHT_OWN (held-out
    // subq-0184): beside a MATCHED action, a matched row's position is the driving scan's (the join hands it on at the
    // outer's current row), and the junk ctid's NULL sends a fill row to the NOT MATCHED actions (merge/lower.rs `test`)
    let own_right = own && p.ops.iter().take_while(|o| !matches!(o, Op::Write(_))).any(right);
    if own_right && !free && w.targets[0].junk_col <= 0 {
        shape(node, RIGHT_OWN);
    }
    // INSERT-ONLY-SIDE: an insert-only MERGE with no built target (`free`: its source a COPY-marked row, a function,
    // VALUES or subquery scan, which `built_target` does not take) tells its matched rows by the target's junk ctid alone
    // (lower.rs: LocFrom::None), so the join's nullable side must be the target's lone producer
    let lone = if free && built.is_none() && !view { target_producer(gx, k, w) } else { None };
    let target_side = !lefts.is_empty() && (own_right || built.or(lone).is_some_and(|q| gx.pipelines()[q].desc.produces() == Some(lefts[0])));
    // mergecmd view NM: a view target's NOT MATCHED row is its NULL wholerow (ExecMerge's view arm), whichever side made it
    if !lefts.is_empty() && (lefts.len() > 1 || nm && !target_side && !view || bs && target_side) {
        shape(node, NOT_MATCHED);
    }
    // SORT-UNION part 2: the FULL merge join's inner is the target's producer (a NULL ctid a source row no target
    // row joined, a NULL source a target row no source row joined: the join condition fails, BY SOURCE), its only join
    let fulls: PgVec<'_, u8> = p.ops.iter().take_while(|o| !matches!(o, Op::Write(_))).filter_map(|o| match o { Op::Join(JoinDesc::Merge { slot, jointype: JoinType::JOIN_FULL, .. }) => Some(*slot), _ => None }).collect_in(gx.mcx);
    // FULL-OUTER (tables never analyzed): or its outer, pipeline `k`'s breaker source, the target's
    // Sort (the planner's commuted form, RIGHT-MERGE's side): a NULL ctid is a source row the inner fill hands on, a NULL
    // source a target row the outer fill hands on (BY SOURCE)
    let outer_full = outer != u8::MAX && built.is_some_and(|q| gx.pipelines()[q].desc.produces() == Some(outer));
    if !fulls.is_empty() && (fulls.len() > 1 || !lefts.is_empty() || view || !outer_full && !built.is_some_and(|q| gx.pipelines()[q].desc.produces() == Some(fulls[0]))) {
        shape(node, NOT_MATCHED);
    }
    // D1684 / D1685 guard the recheck's empty source position; a view target never rechecks (V4R-353: its INSTEAD OF
    // triggers are the write, no ctid and no mark read, as ExecMergeMatched's view arm runs no EvalPlanQual)
    if bs && !view {
        crate::ops::modifytable::merge::nm::keyed_source(gx, k, w, node);
    }
    // a NOT MATCHED INSERT over the target's own scan: no source row to insert (the planner's left join); DO NOTHING is none
    if own && !own_right && m.actions.iter().any(|a| a.kind == MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_TARGET && a.cmd.is_some()) {
        shape(node, "MERGE: a NOT MATCHED INSERT over the target's own scan (no source row to insert)");
    }
    // after the write only RETURNING's projection (write.returning checks its shape, D789)
    if w.returning.is_none() && p.ops.iter().skip_while(|o| !matches!(o, Op::Write(_))).nth(1).is_some() {
        crate::refuse!(node, Reason::Bug { payload: "a MERGE with an op after its write and no RETURNING" });
    }
    (relid, built)
}

/// D1112: op `o` is a SubPlan step only MERGE actions read, lent lazily at the action's read (decompose `attach`: conditional;
/// a nested form): it produces nothing the chain carries, so the recheck's rerun of the chain re-lends it over the new row.
pub(crate) fn action_sub(o: &Op<'_>, w: &WriteDesc<'_>) -> bool {
    crate::graph::lazy_nested_sub(o).is_some_and(|id| w.action_only(id))
}

/// D1112: every SubPlan step the write's own lists read is an action's lazy one (`action_sub`), or (writesubplan) a step
/// after the write of one only its RETURNING list reads (write.returning `merge_ret_steps`).
fn action_subs(p: &gravity_decompose::PipelineNode<'_>, w: &WriteDesc<'_>) -> bool {
    let wi = p.ops.iter().position(|o| matches!(o, Op::Write(_)));
    let ret = |i: usize| wi.is_some_and(|wi| i > wi && crate::ops::modifytable::returning::merge_ret_steps(p, wi, w));
    p.ops.iter().enumerate().all(|(i, o)| !matches!(o, Op::SubPlan(d) if w.reads_own(d.plan_id)) || action_sub(o, w) || ret(i))
}

/// D776's form: the emit scans a REFERENCE-marked source relation's heap and a producer scans the target's own heap (that
/// producer's pipeline, the one the recheck reruns at the host's newest version of the target row). D1249 (M8-WRITE W4):
/// or the emit drains a merge join's sorted outer, the target's producer or a REFERENCE-marked relation's scan (Filter /
/// Project / Sort only): the recheck reruns both sorted inputs, one row each, and the merge step over them.
/// A4-MATEND (D2371): or the emit is a spooled drain whose probe pipeline scans the target's own heap (hashjoin/drain/lower.rs
/// `merge_drained_target`), that probe pipeline the target's producer: the recheck reruns it at the host's newest version.
/// Or an UPDATE / DELETE whose rows are a RIGHT ANTI drain's build rows of the target's own scan (`drained_build_target`):
/// that build the target's producer.
pub(crate) fn built_target(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> Option<usize> {
    if let Some(q) = gx.merge_drained_target(k).or_else(|| gx.drained_build_target(k)) {
        return Some(q);
    }
    let (ps, heap) = (gx.pipelines(), |s: &Source<'_>| match s {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } => Some((u32::from(*scanrelid), *relid)),
        _ => None,
    });
    let reference = |q: usize| heap(&ps[q].source).is_some_and(|(rti, relid)| w.mark_descs.iter().any(|m| m.rti == rti && m.relid == relid && m.mark == RowMarkType::ROW_MARK_REFERENCE));
    let mut target = (0..ps.len()).filter(|&q| q != k && ps[q].desc.produces().is_some() && heap(&ps[q].source).is_some_and(|(rti, relid)| rti == w.rti && relid == w.plan.target_rel));
    // one producer scans the target (its build); none or several: `unbuilt`, by name
    let q = target.next()?;
    let sorted = |f: usize| ps[f].ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::Sort(_))) && (f == q || reference(f));
    let fed = match ps[k].source { Source::Breaker { slot } => (0..k).rev().find(|&f| ps[f].desc.produces() == Some(slot)).is_some_and(sorted), _ => false };
    let merged = ps[k].ops.iter().any(|o| matches!(o, Op::Join(JoinDesc::Merge { .. })));
    // D3689: or the emit reads the keyed groups of an aggregate over a REFERENCE-marked relation's scan (a semi join made
    // unique): the recheck reruns that scan over its stored row, the aggregate over it, and the target's producer
    let grouped = match ps[k].source {
        Source::Breaker { slot } => (0..k).rev().find(|&f| ps[f].desc.produces() == Some(slot)).is_some_and(|f| {
            let keyed = gx.graph.groups.iter().any(|g| g.producer == gravity_program::LifeId(f as u16) && g.slot == slot && !g.keys.is_empty() && g.set.is_none() && g.two_pass.is_none());
            keyed && reference(f) && ps[f].ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_)))
        }),
        _ => false,
    };
    // W5-C (I44): or the emit enters a COPY-marked subquery / function row itself (copy_row.rs: its stored row on recheck)
    let copied = gx.graph.record_rows.iter().any(|r| crate::rerun::epq::marks::copied(gx, r.scanrelid) && crate::rerun::epq::copy_row::boundary(gx, k, r).is_some());
    // D3477: or an UPDATE's / DELETE's emit reads a flattened UNION ALL subquery's settled union whose members the recheck
    // scans again (update_built.rs `copied_settled`)
    let settled = crate::ops::modifytable::update_built::copied_settled(gx, k, w).is_some();
    // D3497: or its own scan that union's one live member, scanned again whole on the recheck (`copied_one`)
    let one = crate::ops::modifytable::update_built::copied_one(gx, k, w);
    // D3870 BUILT-HELD: or a data-modifying CTE's write whose one probe joins the target's build to a COPY mark's rows,
    // entering it or a group that de-duplicates them (rowop_epq.rs `built_held`: its recheck in the IR)
    let j = ps[k].ops.iter().position(|o| matches!(o, Op::Write(_)));
    let held = || j.and_then(|j| crate::ops::modifytable::rowop_epq::built_held(gx, k, j, w)).is_some_and(|(_, f)| f.build.0 == q);
    (target.next().is_none() && (reference(k) || copied || settled || one || fed && merged || grouped || reference_union(gx, k, w) || held())).then_some(q)
}

/// D3403 REFUNION (D3379 isolation umbrella): emit `k` reads a settled union first (its Union op, unpruned) whose every
/// feed scans one REFERENCE-marked relation's own heap under filters and projections: the children of an inheritance or
/// partitioned parent the write joins (`UPDATE t .. FROM parent WHERE ..`, PostgreSQL's Hash Join probed by the Append of
/// the parent's children, the target hashed on its build side). `built_target`'s emit then reads that union as it would
/// the REFERENCE relation's own scan: the recheck reruns each feed over its child's stored row (rerun/epq/marks.rs: the
/// child's mark at its feed's scan site, gated on the family's tableoid column, as EvalPlanQualFetchRowMark fetches only
/// the row's own child), the union's emit over them, and the target's producer at the host's newest version.
pub(crate) fn reference_union(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> bool {
    use crate::rerun::union_arms::{feed_of, union_of, union_op};
    let ps = gx.pipelines();
    let Source::Breaker { slot } = ps[k].source else { return false };
    if !ps[k].ops.first().is_some_and(union_op) || union_of(&ps[k]).map(|u| u.0) != Some(slot) || feed_of(&ps[k]).is_some() {
        return false;
    }
    let feeds = crate::rerun::union_join_arms::feeds(gx, k, slot);
    let reference = |q: usize| {
        let p = &ps[q];
        let scan = match &p.source {
            Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } => Some((u32::from(*scanrelid), *relid)),
            _ => None,
        };
        let Some((last, body)) = p.ops.split_last() else { return false };
        scan.is_some_and(|(rti, relid)| rti != w.rti && w.mark_descs.iter().any(|m| m.rti == rti && m.relid == relid && m.mark == RowMarkType::ROW_MARK_REFERENCE))
            && feed_of(p) == Some(slot) && matches!(last, Op::Append(_)) && p.desc.consumes.is_empty() && body.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_)))
    };
    !feeds.is_empty() && feeds.iter().all(|&q| reference(q))
}

/// `copy_entry`, none past a merge join (a write's chain takes `subquery_own` ops below it); producerchain cluster A: an
/// UPDATE's / DELETE's chain too (update.rs, delete_built.rs).
pub(crate) fn own_entry(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> usize {
    let merged = gx.pipelines()[k].ops.iter().any(|o| matches!(o, Op::Join(JoinDesc::Merge { .. })));
    if merged { 0 } else { copy_entry(gx, k, w) }
}

/// INSERT-ONLY-SIDE: the one pipeline other than `k` that produces and scans the target's own heap (`built_target`'s
/// candidate), none when no pipeline or several do.
fn target_producer(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> Option<usize> {
    let ps = gx.pipelines();
    let heap = |s: &Source<'_>| matches!(s, Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } if u32::from(*scanrelid) == w.rti && *relid == w.plan.target_rel);
    let mut target = (0..ps.len()).filter(|&q| q != k && ps[q].desc.produces().is_some() && heap(&ps[q].source));
    let q = target.next()?;
    target.next().is_none().then_some(q)
}

/// D1933 hashed COPY: the op of pipeline `k` where the first COPY-marked non-target row it runs enters
/// (marks.rs `own_copy_at`: a subquery or function row's copy entry, its own VALUES scan's 0), else 0.
fn copy_entry(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> usize {
    let copy = |mk: &&gravity_decompose::ops::shared::modifytable::RowMark| mk.mark == RowMarkType::ROW_MARK_COPY && !w.targets.iter().any(|t| t.rti == mk.rti);
    w.mark_descs.iter().filter(copy).filter_map(|mk| crate::rerun::epq::marks::own_copy_at(gx, k, mk.rti)).min().unwrap_or(0)
}

/// An op below a copy entry the copied form takes: one that emits each row as it arrives (no end-of-input rows to flush
/// past a recheck's stop), a filter, gate, projection, LIMIT, DISTINCT ON's first-of-run, a WindowAgg's step (the row's
/// window functions evaluated over the partition buffer of the Materialize decompose closes under it, whose fill is a
/// producer the recheck skips; EvalPlanQual hands the SubqueryScan its stored row and never reruns the subquery) or an
/// inner probe (its build a producer the recheck skips: copy_row.rs `upstream`; never a LEFT one, which the NOT MATCHED
/// test reads). A LIMIT by its step kind: its presence only, the bound being the limit family's (C30). producerchain
/// cluster A: or a function row's own FROM step (D890: its entry is the op after it, so the recheck never calls the
/// function). writesubplan (gen setop_core-0157 / 0158): or a set operation's step, by its step kind (setop.setop
/// `Copies`, its emit's first op: each left group's row handed on its copy count times as the row arrives, a stop
/// inside the row resuming past the copies handed on, H32). D3767 (River's JobRescueMany over a large job table:
/// `UPDATE river_job .. FROM (SELECT unnest(..) AS id, ..) AS updated_job WHERE river_job.id = updated_job.id`, Nested
/// Loop (Subquery Scan -> Result -> ProjectSet -> Result; the job's index scan)): or a ProjectSet's step, whose set
/// loop hands each output row on as it comes and holds the opened sets across a stop inside the input row
/// (srf/project_set.rs, H17 c); the recheck enters past it.
pub(crate) fn subquery_own(o: &Op<'_>) -> bool {
    matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::FirstOfRun(_) | Op::Window(_) | Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_INNER, .. }))
        || matches!(o, Op::Srf(d) if matches!(d.place, SrfPlace::From { .. } | SrfPlace::TargetList))
        || matches!(crate::graph::step_kind(o), KindId::StepLimit | KindId::SetopSetop)
}

/// D882 (V4R-362; M7's write.* items): a write whose rows are neither its target's own scan rows nor a
/// `built_target` form. A target a producer scans (a join's build side) under several result relations, or under a
/// COPY-marked row no other pipeline produces (a VALUES scan's, a subquery's or function's entry: the emit's own, whose
/// recheck would replay it from its copy, marks.rs D652), else no single producer chain.
pub(crate) fn unbuilt(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>, node: i32) -> ! {
    let ps = gx.pipelines();
    let target = |s: &Source<'_>| matches!(s, Source::Range { scanrelid, .. } | Source::Index { scanrelid, .. } | Source::Bitmap { scanrelid, .. } if w.targets.iter().any(|t| t.rti == u32::from(*scanrelid)));
    let built = (0..ps.len()).any(|q| q != k && ps[q].desc.produces().is_some() && target(&ps[q].source));
    let elsewhere = |rti: u32| (0..ps.len()).filter(|&q| q != k).any(|q| matches!(ps[q].source, Source::Values { scanrelid, .. } | Source::TableFunc { scanrelid, .. } if u32::from(scanrelid) == rti)
        || ps[q].ops.iter().any(|o| matches!(o, Op::Srf(d) if matches!(d.place, SrfPlace::From { scanrelid, .. } if u32::from(scanrelid) == rti)))
        || gx.graph.record_rows.iter().any(|r| r.scanrelid == rti && crate::rerun::epq::copy_row::boundary(gx, q, r).is_some()));
    match () {
        _ if built && w.targets.len() > 1 => shape(node, "a write of several result relations whose targets sit on a hash build side"),
        _ if built && w.mark_descs.iter().any(|m| m.mark == RowMarkType::ROW_MARK_COPY && !elsewhere(m.rti)) => shape(node, "a write target on a hash build side under a copied source"),
        _ => shape(node, "a write target without a single producer chain"),
    }
}

/// What an action reads that the row does not carry: its carried attributes, the child's own columns, the whole row if `full` (D883).
pub(crate) fn reads<A>(x: Node<'_>, rti: i32, atts: &[Option<A>], full: bool) -> Option<&'static str> {
    if x.node_tag() != NodeTag::T_Var {
        return None;
    }
    let v = x.as_var()?;
    if v.varlevelsup != 0 {
        Some("MERGE: an outer query's column in an action")
    } else if v.varno == INNER_VAR {
        None
    } else if v.varno != rti {
        Some("MERGE: a column of another relation in an action (a joined source: write.lock's EvalPlanQual over its marks)")
    } else if v.varattno < 0 || v.varattno == 0 && !full {
        Some("MERGE: a system column of the target, or its whole row where the child does not carry every column, in an action")
    } else if v.varattno > 0 && atts.get(v.varattno as usize - 1).map_or(true, |a| a.is_none()) {
        Some("MERGE: a target column the child does not carry")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use gravity_decompose::effects::Volatility;
    use gravity_decompose::ops::shared::modifytable::{MergeAction, Target, TargetKind};
    use gravity_decompose::ops::shared::subplan::{SubForm, SubLink, SubPlanDesc, SubSite};
    use gravity_decompose::{InsteadOf, MemberSet, PipelineNode};
    use gravity_program::{Arena, ArenaBox, ArenaVec, End, Pipeline, Reason, SiteDesc, WritePlan};
    use mcx::{Mcx, MemoryContext};
    use types_nodes::list::{IntList, NodeList};
    use types_nodes::primnodes::{SubPlan, TargetEntry};

    use super::*;

    /// mergecmd ACTION-CTE (main 20:19Z, FOR MAIN 42 condition 1): `action_sub` (drain/lower.rs `merge_drained_target`'s
    /// op between the Drain op and the write) admits only a MERGE action's own lazily lent SubPlan: one an action's SET
    /// list reads, nested and conditional. Not a SubPlan RETURNING (or a check) also reads, one no action reads, an eager
    /// one, nor any SubPlan of a write that is no MERGE.
    #[test]
    fn action_sub_is_a_merge_actions_own_lazy_subplan_only() {
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let cx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("action_sub"))); // alloc: Cold
        let m: Mcx<'static> = cx.mcx();
        let nil: &NodeList<'static> = Box::leak(Box::new(NodeList::nil()));
        let ints: &IntList<'static> = Box::leak(Box::new(IntList::nil()));
        // a target list reading SubPlan `id` (an UPDATE's SET value)
        let reads = |id: i32| -> &'static NodeList<'static> {
            let sp = Node::mk(m, SubPlan { plan_id: id, ..Default::default() }).expect("subplan");
            let mut tl = NodeList::nil();
            tl.lappend(m, Node::mk(m, TargetEntry { expr: sp, resno: 2, resname: None, ressortgroupref: 0, resorigtbl: 0, resorigcol: 0, resjunk: false }).expect("tle")).expect("tl");
            Box::leak(Box::new(tl))
        };
        let write = |cmd: Cmd, act: Option<i32>, ret: Option<i32>| {
            let mut targets = ArenaVec::new_in(a);
            let instead = InsteadOf { insert: false, update: false, delete: false, known: true };
            let plan = WritePlan::write(a, cmd, 16390, &[]);
            targets.push(Target { rti: 1, relid: 16390, kind: TargetKind::Heap, site: SiteDesc::none_in(a), plan, indexes: ArenaVec::new_in(a), junk_col: 1, wholerow_col: 0, instead, generated: ArenaVec::new_in(a), set: None, returning: None, merge: None, to_root: ArenaVec::new_in(a) });
            let plan = WritePlan::write(a, cmd, 16390, &[]);
            let (checks, leaves, mark_descs, old_cols) = (&[][..], &[][..], &[][..], &[][..]);
            let mut w = WriteDesc { cmd, rti: 1, site: SiteDesc::none_in(a), plan, set: None, returning: ret.map(reads), on_conflict: None, epq: 0, marks: nil, merge: None, targets: targets.leak(), tableoid_col: 0, checks, route: None, leaves, leaves_known: false, old_new: Default::default(), mark_descs, copy_arms: &[], members: MemberSet::default(), old_cols };
            if cmd == Cmd::Merge {
                let mut actions = ArenaVec::new_in(a);
                actions.push(MergeAction { kind: MergeMatchKind::MERGE_WHEN_MATCHED, cmd: Some(Cmd::Update), quals: None, tlist: act.map_or(nil, reads), set: None });
                w.merge = Some(MergeDesc { join_cond: None, actions, rowid_col: 1, target_cols: ArenaVec::new_in(a), returning_action: false });
            }
            w
        };
        let sub = |plan_id: i32, conditional: bool| {
            let inner = ArenaBox::new_in(PipelineNode::new(a, 9, Source::Range { site: SiteDesc::none_in(a), am: Am::Heap, scanrelid: 2, relid: 16400 }, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite)), a);
            Op::SubPlan(SubPlanDesc { plan_id, link: SubLink::Expr, test: None, args: nil, params: ints, sets: ints, ids: ints, site: SubSite::Row, conditional, volatility: Volatility::Immutable, raise_free: true, quiet_but_rows: true, invariant: false, unknown_eq_false: false, alternative: false, set_spool: false, stops: false, ahead: None, call: gravity_decompose::effects::found::Found::CLEAN, form: SubForm::Nested { inner } })
        };
        let merge = write(Cmd::Merge, Some(7), None);
        assert!(action_sub(&sub(7, true), &merge), "the UPDATE action's own lazy SubPlan");
        assert!(!action_sub(&sub(8, true), &merge), "a SubPlan no action reads");
        assert!(!action_sub(&sub(7, false), &merge), "an eager SubPlan (it runs for every row, not as the action reads it)");
        assert!(!action_sub(&sub(7, true), &write(Cmd::Merge, Some(7), Some(7))), "a SubPlan RETURNING also reads");
        assert!(!action_sub(&sub(7, true), &write(Cmd::Merge, None, Some(7))), "a SubPlan only RETURNING reads");
        assert!(!action_sub(&sub(7, true), &write(Cmd::Update, None, None)), "any SubPlan of a write that is no MERGE");
        assert!(!action_sub(&Op::Project(nil), &merge), "an op that is no SubPlan");
    }
}
