//! write.lock's DELETE end (D882, M4-CLOSE B8): a DELETE whose rows are not its target's own scan rows, as
//! write/delete.rs hands it over. Owner M4-CLOSE. The target hashed on a join's build side under an emit that scans a
//! REFERENCE-marked relation (merge_admit.rs `built_target`: exactly one producer scans the target, D838's DELETE half)
//! is deleted at its junk ctid (write/update_built.rs `locate` / `junk_tid`): a recheck names that producer's scan site,
//! rerun by the host at the target's newest version, as ExecDelete's EvalPlanQual over the join. Under RETURNING
//! write.returning's step does the same row op (returning.rs `make_delete`) and its end is the sink. Anything else
//! refuses by name (merge_admit.rs `unbuilt`: M7's write.*).
use gravity_abi::program::WRITE_DELETE;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::modifytable::{Member, WriteDesc};
use gravity_decompose::Op;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::IntBinOp;
use mcx::{CollectIn, PgBox, PgVec};

use crate::face::end::{EndMech, RowOpDecl, RowOpMech, RowOpRow, SinkKind, TargetsRoad};
use crate::face::kinds::KindId;
use crate::face::strategy::WRITE_ROUTE;
use crate::frame::env::Env;
use crate::frame::frame::EndMaker;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::admit::refusal::Reason;
use crate::ops::Table;

use crate::ops::modifytable::update_built::{junk_tid, locate, union_of, union_rel, Tid, Union};

/// write.lock's end face (its step face is write/lockrows.rs's LockRows).
pub(crate) const ROWS: Table = Table { kinds: &[(KindId::WriteLock, &EndMaker(make_end))], ..crate::ops::NONE };

/// The DELETE row op at the hashed target's junk ctid.
struct BuiltEnd<'g> {
    op: PgBox<'g, dyn RowOpMech + 'g>,
    tid: Tid,
    node: i32,
    /// DELETE-UNION: each row's result relation by its junk tableoid (write/update_built.rs `union_rel`).
    union: Union<'g>,
    /// ROWC-UNION: the in-IR recheck in a data-modifying CTE (`union_end`).
    epq: Option<crate::rerun::epq::epq_ir::Epq<'g>>,
}

pub(crate) fn make_end<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Some(w) = p.ops.iter().find_map(|o| if let Op::Write(w) = o { Some(w) } else { None }) else {
        crate::refuse!(node, Reason::Bug { payload: "write.lock's DELETE end without its write op" })
    };
    // DELETE-UNION: the rows of a settled union of several result relations' scans (`union_end`)
    if let Some(end) = union_end(px, k, w, node) {
        return end;
    }
    // as write/update.rs: the rows reach the write through filters, projections and inner probes, of one target
    // D1933B: or the target's scan the inner of one nested loop (write/nest_write.rs `dml`: its view index)
    let nest = crate::ops::modifytable::nest_write::dml(gx, k, w, node);
    // or the drained build rows of the target's own scan (hashjoin/drain/lower.rs `drained_build_target`: a RIGHT ANTI drain)
    let drained = gx.drained_build_target(k).is_some();
    // producerchain cluster A: or a COPY-marked row's own ops below its entry (merge/admit.rs `own_entry`)
    // D3501: or a LEFT hash probe whose build no target's scan feeds (update_built.rs `left_built`)
    let entry = crate::ops::modifytable::merge::admit::own_entry(gx, k, w);
    // D3477: or the rows of a flattened UNION ALL subquery's settled union, its Union op first, the target hashed
    // (write/update_built.rs `copied_settled`: EvalPlanQual scans the union's members again, whole)
    let settled = super::update_built::copied_settled(gx, k, w).is_some();
    let chain = nest.is_some() || p.ops.iter().take_while(|o| !matches!(o, Op::Write(_))).enumerate().all(|(i, o)| i == 0 && settled || i < entry && crate::ops::modifytable::merge::admit::subquery_own(o) || crate::ops::modifytable::nest_write::target_spool(gx, k, w, o) || matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::SubPlan(_)) || i == 0 && w.targets.len() == 1 && crate::ops::modifytable::merge::admit::reference_union(gx, k, w) || crate::rerun::epq::marks::write_joins(o) || super::update_built::left_built(gx, k, w, o) || drained && matches!(o, Op::Join(gravity_decompose::join::JoinDesc::Drain { .. })));
    let built = nest.or_else(|| if chain && w.targets.len() == 1 { crate::ops::modifytable::merge::admit::built_target(gx, k, w) } else { None });
    if built.is_none() {
        crate::ops::modifytable::merge::admit::unbuilt(gx, k, w, node);
    }
    if w.returning.is_some() {
        return super::returning::make_end(px, k);
    }
    // as write/delete.rs's own end: a root pruned to its one leaf writes under the root (WritePlan::with_route)
    let plan = match &w.route {
        Some(r) if w.members.has(Member::Route) || w.members.has(Member::Targets) => w.plan.copy_in(px.arena).with_route(px.arena, r.root_relid, &[]),
        _ => w.plan.copy_in(px.arena),
    };
    let (loc, tid) = locate(px, node, w, built);
    // D2739 NEST-INNER (subq-0323): a data-modifying CTE's target as a nest's inner rechecks in the IR (write/rowop_epq.rs
    // `nest_inner`), the site as wide as the target for the newest version
    let j = p.ops.iter().position(|o| matches!(o, Op::Write(_))).unwrap_or(0);
    let inner = if super::rowop_epq::in_ir(gx, k) { super::rowop_epq::nest_inner(gx, k, j, w).map(|x| x.1) } else { None };
    // D3870 BUILT-HELD: or the target hashed, probed by a COPY-marked CTE's rows (write/rowop_epq.rs `built_held`)
    let held = if inner.is_none() && super::rowop_epq::in_ir(gx, k) { super::rowop_epq::built_held(gx, k, j, w).map(|x| x.1) } else { None };
    let natts = if inner.is_some() || held.is_some() { super::rowop_epq::epq_width(w) as u16 } else { 0 };
    let decl = RowOpDecl { op: WRITE_DELETE, relid: w.plan.target_rel, natts, plan, loc, conflict: false };
    let op = super::rowop_step::make_op(px, k, decl);
    // D3838 ROWC-INIT: arming the InitPlans the CTE write's first recheck runs again (`cte_armed`)
    let epq = match (inner, held) {
        (Some(ni), _) => Some(crate::rerun::epq::epq_ir::epq_of_inner(px, (k, j), ni, w.plan.target_rel, node, crate::rerun::epq::epq_ir::ROWC_SYS)),
        (None, Some(f)) => Some(crate::rerun::epq::epq_ir::epq_of_built_held(px, f.build, (k, f.probe, j), node, w.plan.target_rel, crate::rerun::epq::epq_ir::ROWC_SYS)),
        (None, None) => None,
    }
    .map(|e| crate::rerun::epq::epq_ir::cte_armed(px, k, e));
    mcx::pg_box_dyn!(px.mcx(), BuiltEnd { op, tid, node, union: None, epq } => dyn EndMech + 'g)
}

/// DELETE-UNION (gen mt-0338: `DELETE FROM pl USING src_p s WHERE pl.id = s.id - 28000 ..`, pl partitioned):
/// the DELETE twin of write/update.rs's union form (producerchain UNION). A DELETE of several result relations whose rows
/// come out of one settled union of their scans (`union_feeds`), its other ops filters, gates, projections, SubPlans and
/// inner probes: PostgreSQL's ModifyTable takes the row's result relation from its junk tableoid and its position from
/// its junk ctid, and EvalPlanQual substitutes that relation's scan with the newest version, every other result
/// relation's scan blocked. So the plan names every target (write.route's leaves, as update.rs adds them under the union
/// form), the position is the junk ctid at the first feed's site and the row's relation its junk tableoid (`union_rel`,
/// whose recheck code names that relation's feed site). D2734 ROWC-UNION (gen mt-0354): in a data-modifying CTE the recheck
/// is the IR's (write/rowop_epq.rs `union_rows`, epq_ir.rs `epq_of_union`), the site as wide as the widest target for
/// the newest version (`epq_width`). Under RETURNING write.returning's step does the same row op (returning.rs
/// `make_delete`), the multi-target host handing back the row's own relation's cells, which the first's list reads
/// mapped onto its attributes (`ret_map`).
fn union_end<'g>(px: &mut Px<'g, '_>, k: usize, w: &WriteDesc<'_>, node: i32) -> Option<PgBox<'g, dyn EndMech + 'g>> {
    let gx = px.gx;
    let feeds = union_feeds(gx, k, w)?;
    if w.returning.is_some() {
        return Some(super::returning::make_end(px, k));
    }
    let (relid, plan) = union_plan(px, k, w, node);
    let union = union_of(px, k, node, w);
    let (loc, tid) = locate(px, node, w, Some(feeds[0]));
    let epq = super::rowop_epq::in_ir(gx, k);
    let natts = if epq { super::rowop_epq::epq_width(w) as u16 } else { 0 };
    let decl = RowOpDecl { op: WRITE_DELETE, relid, natts, plan, loc, conflict: false };
    let op = super::rowop_step::make_op(px, k, decl);
    let j = gx.pipelines()[k].ops.iter().position(|o| matches!(o, Op::Write(_))).unwrap_or(0);
    let epq = epq.then(|| union_epq(px, (k, j), w, &feeds, node));
    Some(mcx::pg_box_dyn!(px.mcx(), BuiltEnd { op, tid, node, union, epq } => dyn EndMech + 'g))
}

/// DELETE-UNION: the feeds by target index (merge/route.rs `union_targets`: the union a hash build reads, the write's
/// pipeline probing it, or one read first; or the union a nested loop's inner reads, update_built.rs `nest_union`) when
/// pipeline `k`'s other ops before write `w` are filters, gates, projections, SubPlans and inner probes; else none.
/// D3379 (UNION-DRAINED): the nest's outer may be the settled union of a flattened UNION ALL subquery, its Union op
/// first (update_built.rs `copied_union`), as the UPDATE's.
pub(super) fn union_feeds<'g>(gx: &crate::graph::Gx<'g, '_>, k: usize, w: &WriteDesc<'_>) -> Option<PgVec<'g, usize>> {
    if w.targets.len() < 2 {
        return None;
    }
    let p = &gx.pipelines()[k];
    let first = crate::ops::modifytable::merge::route::union_targets(gx, k, w);
    let nested = if first.is_none() { crate::ops::modifytable::update_built::nest_union(gx, k, w) } else { None };
    let in_nest = |o: &Op<'_>| nested.is_some() && matches!(o, Op::Join(JoinDesc::Nest { .. }));
    let lead = nested.is_some() && crate::ops::modifytable::update_built::copied_union(gx, k, w);
    let chain = p.ops.iter().take_while(|o| !matches!(o, Op::Write(_))).enumerate().all(|(i, o)| i == 0 && (first.is_some() || lead) || in_nest(o) || matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::SubPlan(_)) || crate::rerun::epq::marks::write_joins(o));
    first.or(nested).filter(|_| chain)
}

/// ROWC-UNION: every result relation of write `w` laid out as the first (its attributes' types, dropped columns and root
/// columns by attno), so the first's RETURNING list reads any row's own cells as the multi-target host hands them back.
/// D2783: the root column too, as `ret_map` maps (a leaf whose same-typed columns sit in another order).
fn same_layout(gx: &crate::graph::Gx<'_, '_>, w: &WriteDesc<'_>) -> bool {
    use gravity_decompose::ops::modifytable::partwrite::root_attno;
    let layout = |t: &gravity_decompose::ops::shared::modifytable::Target<'_>| {
        let n = gx.facts.rel(t.relid)?.natts.max(0) as i16;
        Some((1..=n).map(|a| (gx.facts.att(t.relid, a).map(|f| f.typid), root_attno(t, a))).collect_in(gx.mcx))
    };
    let Some(first) = w.targets.first().and_then(layout) else { return false };
    w.targets.iter().all(|t| layout(t).as_ref() == Some(&first))
}

/// ROWC-UNION: under RETURNING over the union form, each result relation's cell index of every attribute of the first
/// (by root column: partwrite `root_attno`, the planner's map_partition_varattnos over returningLists, as merge/lower.rs
/// maps a MERGE's), or none when every relation is laid out as the first (`same_layout`: the cells as they come).
pub(super) fn ret_map<'g>(gx: &crate::graph::Gx<'g, '_>, w: &WriteDesc<'_>) -> Option<PgVec<'g, (u32, PgVec<'g, Option<usize>>)>> {
    use gravity_decompose::ops::modifytable::partwrite::root_attno;
    if same_layout(gx, w) {
        return None;
    }
    let t0 = &w.targets[0];
    let roots: PgVec<'_, i16> = (1..=t0.site.natts() as i16).map(|a| root_attno(t0, a)).collect_in(gx.mcx);
    Some(w.targets.iter().map(|t| (t.relid, roots.iter().map(|&r| (1..=t.site.natts() as i16).position(|a| r > 0 && root_attno(t, a) == r)).collect_in(gx.mcx))).collect_in(gx.mcx))
}

/// ROWC-UNION: `cells` (`(datum, isnull)` in the row's own result relation's layout, by its junk tableoid in `at`) as the
/// first result relation's `natts` attributes (`ret_map`): per attribute a select over the relations, NULL where none
/// matches or that relation has no such column.
pub(super) fn per_leaf<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, at: &RowVals, (u, node): (&Union<'_>, i32), map: &[(u32, PgVec<'_, Option<usize>>)], cells: &[(gravity_ir::Value, gravity_ir::Value)]) -> PgVec<'e, (gravity_ir::Value, gravity_ir::Value)> {
    use gravity_ir::{IntCond, Type};
    let Some((col, of, _)) = u.as_ref() else { crate::refuse!(node, Reason::Bug { payload: "a union-form RETURNING map without the union" }) };
    let Some(cr) = at.col(*col) else { crate::refuse!(node, Reason::Bug { payload: "a union-fed DELETE's tableoid past the child's projection" }) };
    let mut cx = env.lx(b, node);
    let (rel, _) = crate::rep::datum(&mut cx, cr, of);
    env.memo = cx.memo;
    let mines: PgVec<'_, gravity_ir::Value> = map.iter().map(|(relid, _)| {
        let id = b.iconst(Type::I64, i64::from(*relid));
        b.icmp(IntCond::Eq, rel, id)
    }).collect_in(env.mcx);
    let natts = map.first().map_or(0, |m| m.1.len());
    let (zero, null) = (env.zero, b.bconst(true));
    (0..natts).map(|i| {
        let mut acc = (zero, null);
        for ((_, m), &mine) in map.iter().zip(mines.iter()) {
            let Some(c) = m.get(i).copied().flatten().and_then(|j| cells.get(j).copied()) else { continue };
            acc = (b.select(mine, c.0, acc.0), b.select(mine, c.1, acc.1));
        }
        acc
    }).collect_in(env.mcx)
}

/// DELETE-UNION: the relation the row op declares and its plan, naming every target (write.route's leaves).
pub(super) fn union_plan(px: &mut Px<'_, '_>, k: usize, w: &WriteDesc<'_>, node: i32) -> (u32, gravity_program::WritePlan) {
    let gx = px.gx;
    let (_, relid, mut plan) = (crate::frame::iface::need_strategy::<TargetsRoad>(&node, WRITE_ROUTE).0)(gx, k, w, px.arena);
    if let Some(r) = plan.route.as_mut() {
        let leaves = crate::ops::modifytable::strategies::route::leaves(gx, w);
        r.leaves = gravity_program::ArenaVec::with_capacity_in(leaves.len(), px.arena);
        r.leaves.extend_from_slice(&leaves);
    }
    (relid, plan)
}

/// ROWC-UNION: the in-IR recheck of write op `j` of pipeline `k` over its union's `feeds` (epq_ir.rs `epq_of_union`).
pub(super) fn union_epq<'g>(px: &mut Px<'g, '_>, (k, j): (usize, usize), w: &WriteDesc<'_>, feeds: &[usize], node: i32) -> crate::rerun::epq::epq_ir::Epq<'g> {
    let relids: PgVec<'_, u32> = w.targets.iter().map(|t| t.relid).collect_in(px.mcx());
    let Some(of) = gravity_decompose::catalog::typefacts::facts_of(px.gx.facts, 26) else { crate::refuse!(node, Reason::Bug { payload: "a union-fed write without the oid type" }) };
    crate::rerun::epq::epq_ir::epq_of_union(px, (k, j), feeds, &relids[..], (w.tableoid_col, of), node, crate::rerun::epq::epq_ir::ROWC_SYS)
}

impl EndMech for BuiltEnd<'_> {
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        self.op.open(b, env);
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        // the row reached the write (the scan node's EXPLAIN ANALYZE rows), whatever its status: counted as M1's DELETE end
        let out = b.int_bin(IntBinOp::Add, row.carry.out, env.one);
        let at = RowVals { row: row.row.clone(), vals: row.vals.clone(), cols: row.cols.clone(), carry: Carry { out, accs: mcx::to_vec_in(&row.carry.accs[..], env.mcx) }, next: row.next, at: row.at };
        let tid = junk_tid(&self.tid, self.node, b, env, &at);
        let rel = union_rel(&self.union, self.node, b, env, &at);
        let row = RowOpRow { cells: &[], tid, rel };
        match &self.epq {
            Some(e) => {
                let mut none = |_: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _: &RowVals| PgVec::<(gravity_ir::Value, gravity_ir::Value)>::new_in(env.mcx);
                self.op.call_op(b, env, &at, WRITE_DELETE, row, &mut |_, env, r| r.carry.copy_in(env.mcx), crate::face::end::Arm::Epq(e, &mut none))
            }
            None => self.op.call(b, env, &at, row, &mut |_, env, r| r.carry.copy_in(env.mcx), None),
        }
    }

    fn sink(&self) -> SinkKind {
        SinkKind::RowOp
    }
}
