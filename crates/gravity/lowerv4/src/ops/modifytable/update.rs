//! Kind `write.update` (old row = the scan's row; L4; cards/M4.md; R12-L05; the ladder's `probe_write` UPDATE arm,
//! lower/src/write.rs at ladder-final). Owner M4-UPDATE.
//!
//! PostgreSQL's ExecUpdate for what M4's first UPDATE landing serves: an UPDATE of one heap relation over its own scan.
//! The new tuple is formed in the write site's cells, one per target attribute in attno order: a SET column's cell is the
//! projection's entry for it (the plan's `updateColnos` order, resnos 1..n), every other attribute's is the scan's own cell,
//! carried to the write op in the child's projection (decompose `update_carry`, carry.rs as for MERGE: `WriteDesc::old_cols`),
//! and a dropped attribute's is NULL. So the write never refetches the old row by ctid (rowexec's `ExecGetUpdateNewTuple`
//! over the refetched slot), and an unchanged by-reference cell passes through as the page's datum: a TOASTed value's
//! pointer is stored as it was read, never detoasted (heap_update's toaster keeps an unchanged external value).
//!
//! The row op is the M4 face (face/end.rs `RowOpMech`, write/rowop_step.rs): the position from the driving source's
//! locator, ONE `write_row(ctx, site, WRITE_UPDATE)`, and the status branch. The host (hosts/src/write.rs) does the rest as
//! ExecUpdate does: the partition constraint, WCO / RLS, `ExecConstraints` (NOT NULL, CHECK) and the stored generated
//! columns over the formed row, `heap_update` under the statement's command id with its own HOT decision, the index
//! inserts on a non-HOT update with the `indexUnchanged` hint (road `write.idxunchanged`, `IdxUnchanged` below; H-a), and
//! the TM_Result mapping: TM_SelfModified skipped, READ COMMITTED's TM_Updated locked and rechecked (EvalPlanQual resumes
//! past the row, the SET recomputed over the new version), REPEATABLE READ's 40001.
//!
//! Refused by name, each by the kind or road that builds it (R-A):
//! - RETURNING is served (M4-RET's continuation over the written row, the end its sink); a SubPlan in the write's own
//!   expressions refuses as `nest.subplan` (M8); rows that are not the target's own scan rows or its inner probes' (a
//!   locking or parent mark, IN over a subquery): `write.lock` (UPDATE ... FROM over inner joins serves: D655, marks.rs);
//!   an UPDATE through a partitioned root whose row may move that the move host cannot bind (`Route::moves`: a root
//!   with rules, unknown or unservable leaves): `write.route`; over a join it moves under the marks (V4R-323), with check
//!   options each destination checks the first target's list (V4R-329).
//!
//! An UPDATE through a partitioned root whose row may leave its leaf (`moving`: a partition key column, `Member::CrossPartition`,
//! H-e (c); a BEFORE ROW UPDATE trigger's NEW, D687; an ancestor's key, D820) is this kind's plan plus the moved rows' destinations
//! (`WritePlan::with_moves`): the host (`hosts/src/write_move.rs`) deletes a row that fails its leaf's partition
//! constraint and inserts it through the root, ExecCrossPartitionUpdate's order and EPQ.
//!
//! Several result relations (an inheritance tree, a partitioned table not pruned to one leaf: road `write.route`'s
//! `TargetsRoad`, as write/delete.rs): decompose streams the write into its Append's arms (V4R-214), one arm per target in
//! child order, and every arm writes through the one site write/rowop.rs shares. ModifyTable's child hands it the new values
//! of the SET columns alone (resnos 1..n, the plan's `updateColnos` order: the root's attnos ascending, rewriteTargetListIU's
//! order) and the row's position; ExecUpdate forms the target's new row over its old version (ExecGetUpdateNewTuple over the
//! row `table_tuple_fetch_row_version` fetched under SnapshotAny by the ctid). So the site carries the n SET cells, and the
//! host (hosts `MultiTargetHost`) fetches the old version from the target the position names, maps the root's SET attnos to
//! the target's by name (ri_RootToChildMap) and overlays the cells. The lowerer checks that every target's SET list, in the
//! root's numbering, is one strictly ascending list, else a Shape by name. A check option (WITH CHECK OPTION / RLS) over
//! several result relations is its target's own (withCheckOptionLists[i]): the host checks each target's row against its
//! list (write.checks). An inheritance tree's ONE result relation
//! (a child the others' exclusion left, the parent planned as its own child, a WHERE false dummy) is the single-target
//! write under the root (WritePlan::with_route, no leaves), as write/delete.rs: the root's statement triggers, never the leaf's.
//! UNION: when decompose cannot stream the write into the Append's arms (a check option's SubPlan, a
//! SubPlan over the Append, a data-modifying CTE beside it) the Append settles into a union of the targets' scans the
//! write's pipeline reads first (merge/route.rs `union_targets`), or that a nested loop's inner reads under another
//! relation's outer rows (update_built.rs `nest_union`): the same site and plan, each row's result relation its junk
//! tableoid and its position its junk ctid (write/update_built.rs `union_of` / `union_rel`); under RETURNING the first
//! target's list over the row's own cells mapped onto its attributes (lockrows UNION-RET, update_built.rs `ret_cells`).
//! - Handed to `write.transition` (M4-TRIG; NotBuilt by that name until it registers): a view target (its INSTEAD OF row
//!   triggers are the write: the kind's own end), and a trigger that fires on UPDATE (on the target or on the partitioned
//!   root it is pruned through; RI's triggers included; unknown facts count: its step is made for the trigger plan, the
//!   host fires them).
//! - a row beside another pipeline's product (an InitPlan / SubPlan's own pipeline): the row-op claim's Shape naming D652
//!   (D610's rest: the leader's EvalPlanQual reruns the stopped pipeline alone), write/rowop.rs.
//! - an index of the target outside its frozen `IndexPlan`s (a partial, expression, INCLUDE or deferrable index, an
//!   exclusion or non-btree index with one of those, an access method or uniqueness the host's maintenance does not plan)
//!   or one that is not valid / not ready: a Shape naming D624, per index. The host maintains the plan's indexes and no
//!   other (hosts index_maint), so a non-HOT update would miss that index's entries or skip its exclusion check.
//! - D838 (returning-019 / returning-014): the target hashed on a join's build side, write/update_built.rs.
use gravity_abi::program::WRITE_UPDATE;
use gravity_decompose::ops::modifytable::trig_facts::{TrigExt, TRIG_UPDATE};
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::ops::shared::modifytable::{IndexDesc, Member, WriteDesc};
use gravity_decompose::{Am, Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::IntBinOp;
use gravity_program::{ArenaRef, Cmd, IndexPlan, WritePlan};
use mcx::{GrowIn, PgBox, PgVec};

use crate::face::end::{Arm, ChecksAt, EndMech, LocFrom, RowOpDecl, RowOpMech, RowOpRoad, RowOpRow, SinkKind, TargetsRoad};
use crate::face::kinds::KindId;
use crate::face::strategy::{WRITE_CHECKS, WRITE_ROUTE};
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{EndMaker, Next, StepMaker};
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};

use crate::ops::modifytable::face::RetMech;
use crate::ops::modifytable::strategies::checks_ir::Checks;
use crate::ops::modifytable::strategies::route::Route;
use crate::ops::modifytable::update_built::{junk_tid, locate, union_of, union_rel, Tid, Union};
use crate::ops::modifytable::update_cells::{cells_of, new_tuple, ret_width, set_cells, Src};

crate::strategies! {
    /// IndexUnchanged per index on non-HOT updates.
    WRITE_IDXUNCHANGED = 117, "write.idxunchanged", Update;
}

/// Two faces. Without RETURNING, as `write.delete`'s: the row-op end does the write and the write op in the chain is an
/// identity step (M1's end form, so no IR pin moves). With RETURNING the write op is the row op as a step: on Done its
/// continuation is write.returning's (`RetMech`) over the written row, and the end is write.returning's sink.
pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::WriteUpdate, &EndMaker(make_end)), (KindId::WriteUpdate, &StepMaker(make_step))], strategies: &[(crate::face::strategy::WRITE_IDXUNCHANGED, &IdxUnchanged)], ..crate::ops::NONE };

/// Road `write.idxunchanged`'s face (D235): PG's `index_unchanged_by_update` hint is the write host's (hosts
/// `index_maint.rs` `set_updated`, once per index per bind), so the program carries no bit; this row only builds the road
/// that `RowOpRoad::IdxUnchanged.require` asks for on a target with an index.
pub(crate) struct IdxUnchanged;

struct WriteStep;

impl StepMech for WriteStep {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        next(b, env, row)
    }
}

/// The row op as a step (UPDATE ... RETURNING): the cells, ONE host call, and on Done write.returning's continuation over
/// the written row (`RowOpMech::cells`: a BEFORE ROW trigger's row, the stored generated columns). After an EvalPlanQual
/// recheck the runner runs this same program over the new version, so RETURNING projects the version actually updated.
struct UpdateStep<'g> {
    op: PgBox<'g, dyn RowOpMech + 'g>,
    cells: PgVec<'g, Src>,
    tid: Tid,
    ret: PgBox<'g, dyn RetMech + 'g>,
    node: i32,
    /// D655 Row C (V4R-534): the op rechecks in the IR (a data-modifying CTE's): the recheck its `Arm::Epq` reruns.
    epq: Option<crate::rerun::epq::epq_ir::Epq<'g>>,
    /// UNION: each row's result relation by its junk tableoid (write/update_built.rs `union_rel`).
    union: Union<'g>,
    /// UNION-RET: the site's cells and the row's own relation's cells onto the first target's attributes.
    map: Option<(usize, PgVec<'g, (u32, PgVec<'g, Option<usize>>)>)>,
}

fn make_step<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id);
    let Op::Write(w) = &p.ops[j] else { crate::refuse!(node, Reason::Bug { payload: "write.update's step over an op that is not the write" }) };
    // a view target's write is write.transition's (D667): under RETURNING its step, else its end (make_end hands it over);
    // without RETURNING the end writes (UpdateEnd)
    if w.members.has(Member::TargetView) && w.returning.is_some() {
        return crate::ops::modifytable::triggers::view_step(px, k, j);
    }
    if w.returning.is_none() {
        return mcx::pg_box_dyn!(px.mcx(), WriteStep => dyn StepMech + 'g);
    }
    let (op, cells, tid, union) = row_op(px, k, w);
    let epq = op.epq_in_ir().then(|| epq_of(px, (k, j), node, w, union.is_some()));
    let ret = super::returning::make_ret(px, k, j);
    let map = super::update_built::ret_map(gx, k, j, (w, &union, cells.len()), node);
    mcx::pg_box_dyn!(px.mcx(), UpdateStep { op, cells, tid, ret, node, epq, union, map } => dyn StepMech + 'g)
}

impl StepMech for UpdateStep<'_> {
    fn prologue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _x: &Exits) {
        self.op.open(b, env);
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let cells = new_tuple(&self.cells, self.node, b, env, row);
        let tid = junk_tid(&self.tid, self.node, b, env, row);
        let rel = union_rel(&self.union, self.node, b, env, row);
        let (op, ret) = (&*self.op, &*self.ret);
        let mut again = |b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, r: &RowVals| new_tuple(&self.cells, self.node, b, env, r);
        op.call_op(b, env, row, WRITE_UPDATE, RowOpRow { cells: &cells, tid, rel }, &mut |b, env, r| {
            let (mut written, mut old) = (op.cells(b, env, false), op.old_cells(b, env));
            if let Some(m) = &self.map {
                (written, old) = (super::update_built::ret_cells(b, env, r, (&self.union, self.node), m, written), super::update_built::ret_cells(b, env, r, (&self.union, self.node), m, old));
            }
            ret.lower_old(b, env, r, &written, &old, &mut *next)
        }, match &self.epq { Some(e) => Arm::Epq(e, &mut again), None => Arm::None })
    }

    fn wants_locator(&self) -> bool {
        true
    }
}

struct UpdateEnd<'g> {
    op: PgBox<'g, dyn RowOpMech + 'g>,
    cells: PgVec<'g, Src>,
    tid: Tid,
    node: i32,
    /// D652-S1: the op rechecks in the IR (write/rowop_epq.rs `subs_in_ir`): the recheck its `Arm::Epq` reruns.
    epq: Option<crate::rerun::epq::epq_ir::Epq<'g>>,
    /// UNION: each row's result relation by its junk tableoid (write/update_built.rs `union_rel`).
    union: Union<'g>,
}

fn make_end<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Some(w) = p.ops.iter().find_map(|o| if let Op::Write(w) = o { Some(w) } else { None }) else {
        crate::refuse!(node, Reason::Bug { payload: "a row-op end without its write op" })
    };
    if w.cmd != Cmd::Update {
        crate::refuse!(node, Reason::Bug { payload: "write.update's end over a write that is not an UPDATE" });
    }
    // RETURNING: the step (UpdateStep, or a view's write.transition step, D667) did the write; the end is write.returning's sink
    if w.returning.is_some() {
        return super::returning::make_end(px, k);
    }
    // a view target (member TargetView): its INSTEAD OF row triggers are the write (hosts write_view_rows.rs, H-d). write.transition
    // makes the end: the view's cells, then the wholerow OLD. NotBuilt(write.transition) by name until M4-TRIG registers it
    if w.members.has(Member::TargetView) {
        return super::triggers::make_view_end(px, k);
    }
    let (op, cells, tid, union) = row_op(px, k, w);
    let j = p.ops.iter().position(|o| matches!(o, Op::Write(_))).unwrap_or(0);
    let epq = op.epq_in_ir().then(|| epq_of(px, (k, j), node, w, union.is_some()));
    mcx::pg_box_dyn!(px.mcx(), UpdateEnd { op, cells, tid, node, epq, union } => dyn EndMech + 'g)
}

/// The in-IR recheck of UPDATE `w`, op `j` of pipeline `k` (write/rowop_epq.rs `in_ir`): the target's own (`epq_of_subs`),
/// or, under the union form (`union`: write/update_built.rs `union_of`), the row's own relation's feed over its newest
/// version and the ops above the union (modifytable ROWC-UNION-UPDATE, triggers#500: delete_built.rs `union_epq`, as
/// D2734's DELETE).
fn epq_of<'g>(px: &mut Px<'g, '_>, (k, j): (usize, usize), node: i32, w: &WriteDesc<'_>, union: bool) -> crate::rerun::epq::epq_ir::Epq<'g> {
    let gx = px.gx;
    // D3647 BUILT-COPY: or the target hashed, probed by a COPY-marked CTE's rows (write/rowop_epq.rs `built_copy`)
    if let Some((_, f)) = super::rowop_epq::built_copy(gx, k, j, w).filter(|_| !union) {
        let copy = w.mark_descs.iter().find(|m| m.rti != w.rti).map(|m| (m.ctid_col, m.rti));
        let Some(copy) = copy else { crate::refuse!(node, Reason::Bug { payload: "a BUILT-COPY write without its COPY mark" }) };
        let e = crate::rerun::epq::epq_ir::epq_of_build_copy(px, f.build, (k, f.entry, f.probe, j), copy, node, w.plan.target_rel, crate::rerun::epq::epq_ir::ROWC_SYS);
        return crate::rerun::epq::epq_ir::cte_armed(px, k, e); // D3838 ROWC-INIT
    }
    // D3824 DRAIN-COPY: or the target hashed under a RIGHT SEMI drain probed by a COPY-marked CTE's rows
    // (write/rowop_epq.rs `drain_copy`)
    if let Some((_, f)) = super::rowop_epq::drain_copy(gx, k, j, w).filter(|_| !union) {
        let Some(copy) = w.mark_descs.iter().find(|m| m.rti != w.rti).map(|m| (m.ctid_col, m.rti)) else { crate::refuse!(node, Reason::Bug { payload: "a DRAIN-COPY write without its COPY mark" }) };
        let e = crate::rerun::epq::epq_ir::epq_of_drain_copy(px, f.build, f.probe, (k, f.drain, j), copy, node, w.plan.target_rel, crate::rerun::epq::epq_ir::ROWC_SYS);
        return crate::rerun::epq::epq_ir::cte_armed(px, k, e); // D3838 ROWC-INIT
    }
    // D3870 BUILT-HELD: or the target hashed, probed by a COPY-marked CTE's rows a group de-duplicates
    // (write/rowop_epq.rs `built_held`): the build's redo joined to the probe row the probe held
    if let Some((_, f)) = super::rowop_epq::built_held(gx, k, j, w).filter(|_| !union) {
        let e = crate::rerun::epq::epq_ir::epq_of_built_held(px, f.build, (k, f.probe, j), node, w.plan.target_rel, crate::rerun::epq::epq_ir::ROWC_SYS);
        return crate::rerun::epq::epq_ir::cte_armed(px, k, e); // D3838 ROWC-INIT
    }
    // D3602: or the target a nest's inner (write/rowop_epq.rs `nest_inner`), as the DELETE's (delete_built.rs)
    if let Some((_, ni)) = super::rowop_epq::nest_inner(gx, k, j, w).filter(|_| !union) {
        let e = crate::rerun::epq::epq_ir::epq_of_inner(px, (k, j), ni, w.plan.target_rel, node, crate::rerun::epq::epq_ir::ROWC_SYS);
        return crate::rerun::epq::epq_ir::cte_armed(px, k, e); // D3838 ROWC-INIT
    }
    match super::delete_built::union_feeds(gx, k, w).filter(|_| union) {
        Some(feeds) => super::delete_built::union_epq(px, (k, j), w, &feeds, node),
        None => crate::rerun::epq::epq_ir::epq_of_subs(px, k, j, node, super::rowop_epq::epq_rel(gx, k, w), crate::rerun::epq::epq_ir::ROWC_SYS),
    }
}

/// The row op for UPDATE `w` of pipeline `k` (whichever face writes: the end, or the step under RETURNING), after every
/// refusal by name and write.transition's trigger hand-off; with the new tuple's cell sources (and the union form's facts).
fn row_op<'g>(px: &mut Px<'g, '_>, k: usize, w: &WriteDesc<'_>) -> (PgBox<'g, dyn RowOpMech + 'g>, PgVec<'g, Src>, Tid, Union<'g>) {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let (relid, routed, built) = admit(gx, k, w, px.arena);
    // write.transition (M4-TRIG): the host fires a firing (or unknown) trigger set (hosts trigger*.rs). The trigger plan's make
    // declares it (ProtoKind::Transition) and refuses by name what the host does not fire. NotBuilt(write.transition) until M4-TRIG
    // registers it. Its step is the identity, so it is made and not lowered.
    if fires(gx.facts.trig_ext(relid)) || w.route.is_some_and(|r| fires(gx.facts.trig_ext(r.root_relid))) {
        let j = p.ops.iter().position(|o| matches!(o, Op::Write(_))).unwrap_or(0);
        drop(super::triggers::make_plan(px, k, j));
    }
    // several result relations: the SET cells alone, under the plan write.route routed over every target
    if let Some((rti, plan)) = routed {
        let plan = moving(gx, node, w, plan, px.arena);
        let mut cells = set_cells(gx, k, w, rti);
        let plan = ret_width(gx, k, w, plan, &mut cells);
        // D652-S2: a recheck in the IR reads the newest version of the row's target from the site (write/rowop_epq.rs)
        if super::rowop_epq::in_ir(gx, k) {
            cells.extend_in((cells.len()..super::rowop_epq::epq_width(w)).map(|_| Src::Null));
        }
        // UNION: rows out of a settled union of the targets' feeds are no Append arm's, so write/rowop.rs
        // `claim` adds no leaves: every target's here (as merge/route.rs `plan`); the position is the junk ctid at the
        // first feed's site, each row's relation its junk tableoid (write/update_built.rs)
        let union = union_of(px, k, node, w);
        let (plan, (loc, tid)) = match &union {
            Some(_) => {
                let mut plan = plan;
                if let Some(r) = plan.route.as_mut() {
                    let leaves = crate::ops::modifytable::strategies::route::leaves(gx, w);
                    r.leaves = gravity_program::ArenaVec::with_capacity_in(leaves.len(), px.arena);
                    r.leaves.extend_from_slice(&leaves);
                }
                // UNION-RET: a row's tableoid its own relation's, the host's (ret_sys.rs `union_form`)
                let plan = match crate::ops::modifytable::ret_sys::sys_plan_at(gx, k, w) { Some(h) => plan.with_sys(h), None => plan };
                (plan, locate(px, node, w, built))
            }
            None => (plan, (LocFrom::Driving, None)),
        };
        let decl = RowOpDecl { op: WRITE_UPDATE, relid, natts: cells.len() as u16, plan, loc, conflict: false };
        return (super::rowop_step::make_op(px, k, decl), cells, tid, union);
    }
    let cells = cells_of(gx, k, w, relid);
    let natts = cells.len() as u16;
    // one leaf of a partitioned root pruned to it (D665's gap, as delete.rs), or an inheritance tree's one result relation
    // (as delete.rs, V4R-214): the frozen plan names the root (WritePlan::with_route, no leaves), so the host locks it and
    // fires its statement triggers (ExecInitModifyTable's rootRelation, fireBSTriggers / fireASTriggers on it, at zero rows
    // too), never the leaf's
    let plan = match &w.route {
        Some(r) if w.members.has(Member::Route) || w.members.has(Member::Targets) => w.plan.copy_in(px.arena).with_route(px.arena, r.root_relid, &[]),
        _ => w.plan.copy_in(px.arena),
    };
    // D829: a pruned root moves no row (its dummy Result yields none)
    let plan = if crate::ops::shared::dummy::pruned_root(gx, k, w) { plan } else { moving(gx, node, w, plan, px.arena) };
    // M4-RET R4a: RETURNING OLD reads the version each UPDATE replaced, which the host fetches (no fetch otherwise)
    let plan = if w.returning.is_some() && w.old_new.old { plan.with_old() } else { plan };
    let plan = crate::ops::modifytable::ret_sys::sys_update(w, plan); // D774: RETURNING's system columns, the host's
    let (loc, tid) = locate(px, node, w, built); // D838: a hashed target's junk ctid (write/update_built.rs)
    let decl = RowOpDecl { op: WRITE_UPDATE, relid, natts, plan, loc, conflict: false };
    (super::rowop_step::make_op(px, k, decl), cells, tid, None)
}

impl EndMech for UpdateEnd<'_> {
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        self.op.open(b, env);
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        // the row reached the write (the scan node's EXPLAIN ANALYZE rows), whatever its status: counted as M1's DELETE end
        let out = b.int_bin(IntBinOp::Add, row.carry.out, env.one);
        let at = RowVals { row: row.row.clone(), vals: row.vals.clone(), cols: row.cols.clone(), carry: Carry { out, accs: mcx::to_vec_in(&row.carry.accs[..], env.mcx) }, next: row.next, at: row.at };
        let cells = new_tuple(&self.cells, self.node, b, env, &at);
        let tid = junk_tid(&self.tid, self.node, b, env, &at);
        let rel = union_rel(&self.union, self.node, b, env, &at);
        let Some(e) = &self.epq else { return self.op.call(b, env, &at, RowOpRow { cells: &cells, tid, rel }, &mut |_, env, r| r.carry.copy_in(env.mcx), None) };
        let mut again = |b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, r: &RowVals| new_tuple(&self.cells, self.node, b, env, r);
        self.op.call_op(b, env, &at, WRITE_UPDATE, RowOpRow { cells: &cells, tid, rel }, &mut |_, env, r| r.carry.copy_in(env.mcx), Arm::Epq(e, &mut again))
    }

    fn sink(&self) -> SinkKind {
        SinkKind::RowOp
    }
}

/// Every refusal by name of the module doc; the target relation when the UPDATE serves, and for a write of several result
/// relations the arm's own target's range table index with the plan write.route routed over every target.
fn admit(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>, a: ArenaRef) -> (u32, Option<(u32, WritePlan)>, Option<usize>) {
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let several = w.targets.len() > 1;
    for m in w.members.iter() {
        match m {
            Member::Update | Member::Route | Member::Generated | Member::GeneratedVirtual => {}
            // the index members the decomposer declares only once a consumer binds them (L0-r): the host maintains each
            Member::IndexUnique | Member::IndexPlain | Member::IndexDeferrable | Member::IndexExclusion | Member::IndexPartial | Member::IndexExpression | Member::IndexInclude | Member::IndexNonBtree => {}
            // RETURNING: write.returning's continuation (OLD / NEW reach here only once decompose binds them, and refuse there by name)
            Member::Returning | Member::ReturningOld | Member::ReturningNew => {}
            // W6-D1112: a check option's SubPlan is the host qual's read (write/rowop_sub.rs); any other keeps D1112
            Member::WriteSubPlan if crate::ops::modifytable::strategies::checks_ir::check_only(w) => {}
            // D1112: one only RETURNING reads is a step after the write (write.returning `proj_at`)
            Member::WriteSubPlan if super::returning::returning_only(w) => {}
            // D1112 (rxnested): both, each as it is alone (write.returning `returning_and_checks`)
            Member::WriteSubPlan if super::returning::returning_and_checks(w) => {}
            Member::WriteSubPlan => crate::refuse!(node, Reason::Shape { why: ShapeWhy("a SubPlan in a write's own expressions, a check, RETURNING, ON CONFLICT or a MERGE action (D1112: nest.subplan B4)") }),
            // D655: a joined relation's REFERENCE mark (write/rowop.rs `claim`, marks.rs); a parent's is M7's (D1000)
            Member::WriteMarks => {}
            Member::ParentMark => crate::rerun::epq::marks::parent_mark(node, w.mark_descs),
            Member::TargetView | Member::LockingWriteMark | Member::MultiLock => crate::refuse!(node, Reason::Bug { payload: "an UPDATE admit over a view (handed off before) or an unbound lock member" }),
            Member::Targets => {}
            // H-e (c): the moved row's destinations are write.route's (`Route::moves`, row_op)
            Member::CrossPartition => {}
            Member::CheckView | Member::CheckRls => RowOpRoad::Checks.require(&node),
            Member::TargetAm => {}
            Member::TargetForeign | Member::TargetOtherAm | Member::DirectForeign => crate::refuse!(node, Reason::UnsupportedAm { relid: w.plan.target_rel }),
            Member::Insert | Member::Delete | Member::Merge | Member::ConflictNothing | Member::ConflictUpdate | Member::WholeRow | Member::MergeActionFn => {
                crate::refuse!(node, Reason::Bug { payload: "an UPDATE carrying an INSERT / DELETE / MERGE member" })
            }
        }
    }
    if w.targets.is_empty() {
        crate::refuse!(node, Reason::Bug { payload: "an UPDATE without a result relation (PG keeps a pruned root's, D829)" });
    }
    // several result relations (write.route, M4-ROUTE R2): this pipeline is one arm of the Append decompose streamed the
    // write into (V4R-214), over its own target's rows; write.route names that target and the plan every arm shares
    let routed = several.then(|| (crate::frame::iface::need_strategy::<TargetsRoad>(&node, WRITE_ROUTE).0)(gx, k, w, a));
    let (rti, relid) = routed.as_ref().map_or((w.rti, w.plan.target_rel), |r| (r.0, r.1));
    // D829: a partitioned root pruned to no leaf is the result relation itself over the dummy Result: no storage, no row
    let pruned = crate::ops::shared::dummy::pruned_root(gx, k, w);
    if !pruned && gx.facts.rel(relid).is_some_and(|r| r.am != Am::Heap && !r.rows) {
        crate::refuse!(node, Reason::UnsupportedAm { relid });
    }
    // D624: the host maintains the frozen IndexPlans and nothing else; any other index of a target refuses by name (a
    // pruned root's are its partitioned indexes, which no row reaches)
    // D3681: these walk every target; an arm of a several-target write after the first that passed takes their answer
    // (memo.rs `targets_ok`), the same for every arm of the write
    let (key, at) = (crate::memo::key_of(&gx.graph.pipelines), w.targets.as_ptr() as usize);
    let kept = (several && !pruned).then(|| crate::memo::read(key, |m| m.targets_ok.iter().find(|x| x.0 == at).map(|x| x.1))).flatten().flatten();
    if let Some(indexed) = kept {
        if indexed {
            RowOpRoad::IdxUnchanged.require(&node);
        }
    } else {
        for t in w.targets.iter().filter(|_| !pruned) {
            if let Some(why) = t.indexes.iter().find_map(|d| unmaintained(gx, t.plan.indexes(), d)) {
                crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) });
            }
        }
        let indexed = !w.plan.indexes().is_empty() || !w.targets[0].indexes.is_empty() || w.targets.iter().any(|t| !t.plan.indexes().is_empty() || !t.indexes.is_empty());
        if indexed {
            RowOpRoad::IdxUnchanged.require(&node);
        }
        // write.checks (INSERT's road, Applies::InsertUpdate): WITH CHECK OPTION / RLS quals the host cannot serve, and the
        // indexes its maintenance refuses, by name; the checks run in the host op (ExecUpdate's order)
        if crate::frame::iface::need_strategy::<Checks>(&node, WRITE_CHECKS).plan(gx, node, w) != ChecksAt::Host {
            crate::refuse!(node, Reason::Bug { payload: "an IR checks plan at M4's first landing" });
        }
        if several && !pruned {
            crate::memo::write(key, |m| m.targets_ok.push((at, indexed))); // alloc: Program
        }
    }
    // the rows reaching the write are the target's own scan rows, one to one, or joined by inner probes (D655: the other
    // relations' REFERENCE marks rebuild the join at a recheck, marks.rs)
    let own = match &p.source {
        Source::Range { am, scanrelid, relid: r, .. } | Source::Index { am, scanrelid, relid: r, .. } | Source::Bitmap { am, scanrelid, relid: r, .. } | Source::Tid { am, scanrelid, relid: r, .. } => {
            *am == Am::Heap && *r == relid && u32::from(*scanrelid) == rti
        }
        // a Result: only under a constant-false one-time filter (the planner's dummy rel), whose row never comes
        Source::Const => crate::ops::shared::dummy::dummy(gx, k, p.ops.len()),
        _ => false,
    };
    let arm = |o: &Op<'_>| several && matches!(o, Op::Append(AppendDesc::Arm { .. }));
    // D1933B: or the target's scan the inner of one nested loop (write/nest_write.rs `dml`: its view index)
    let nest = if !own && !several { crate::ops::modifytable::nest_write::dml(gx, k, w, node) } else { None };
    // or the drained build rows of the target's own scan (hashjoin/drain/lower.rs `drained_build_target`: a RIGHT ANTI drain)
    let drained = gx.drained_build_target(k).is_some() || gx.hashed_union_build(k).is_some();
    // UNION: or the rows of a settled union of the targets' feeds, its Union op first (merge/route.rs
    // `union_targets`): each row's relation its junk tableoid, its position its junk ctid (write/update_built.rs). Under
    // RETURNING the multi-target host hands back the row's own relation's cells, read mapped onto the first relation's
    // attributes (lockrows UNION-RET, update_built.rs `ret_map`)
    let first = several.then(|| crate::ops::modifytable::merge::route::union_targets(gx, k, w).or_else(|| super::update_built::sorted_outer(gx, k, w))).flatten();
    // or the union a nest's inner reads, the nest over another relation's rows (write/update_built.rs `nest_union`), or
    // over a flattened UNION ALL subquery's settled union, its Union op first (`copied_union`, producerchain UNION-NEST)
    let nested = if several && first.is_none() { super::update_built::nest_union(gx, k, w) } else { None };
    let in_nest = |o: &Op<'_>| nested.is_some() && matches!(o, Op::Join(gravity_decompose::join::JoinDesc::Nest { .. }));
    // producerchain cluster A: or a COPY-marked row's own ops below its entry on this pipeline (merge/admit.rs `own_entry`)
    // D3501: or, over the hashed target, a LEFT hash probe whose build no target's scan feeds (update_built.rs `left_built`);
    // D3725: over the target's own scan too (River's JobSchedule: its scan, an INNER probe, then the LEFT one)
    let entry = crate::ops::modifytable::merge::admit::own_entry(gx, k, w);
    // D3792: or a nest over a plan Material of one merge or hash join whose write rechecks in the IR (rowop_epq.rs
    // `nest_in_ir`: River's JobSchedule with few due jobs)
    // D3477: or one target's rows over a flattened UNION ALL subquery's settled union, its Union op first, the target
    // hashed (`copied_settled`: EvalPlanQual scans the union's members again, whole)
    let settled = !several && super::update_built::copied_settled(gx, k, w).is_some();
    let chain = nest.is_some() || p.ops.iter().take_while(|o| !matches!(o, Op::Write(_))).enumerate().all(|(i, o)| i == 0 && (first.is_some() || nested.is_some() && super::update_built::copied_union(gx, k, w) || !several && crate::ops::modifytable::merge::admit::reference_union(gx, k, w) || settled) || i < entry && crate::ops::modifytable::merge::admit::subquery_own(o) || in_nest(o) || crate::ops::modifytable::nest_write::target_spool(gx, k, w, o) || matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::SubPlan(_)) || arm(o) || crate::rerun::epq::marks::write_joins(o) || super::update_built::left_built(gx, k, w, o) || super::rowop_epq::nest_in_ir(gx, k, i) || drained && matches!(o, Op::Join(gravity_decompose::join::JoinDesc::Drain { .. })));
    let union = first.or(nested);
    let projected = p.ops.iter().take_while(|o| !matches!(o, Op::Write(_))).any(|o| matches!(o, Op::Project(_)));
    // D1933 (c): or the target's own scan the outer of one nested loop over a Material (write/nest_outer.rs)
    // producerchain ARM: or one arm of several targets' streamed union over a nest copy (nest_outer.rs decides)
    let chain = chain || own && crate::ops::modifytable::nest_outer::admits(gx, k, w);
    // producerchain: or the outer of INNER nested loops over REFERENCE-marked relations' heap scans (write/nest_ref.rs)
    let chain = chain || !several && crate::ops::modifytable::nest_ref::chain(gx, k, w);
    // returning-019: or the target hashed by the one producer scanning it, the emit scanning a REFERENCE-marked relation
    let built = nest.or_else(|| if !own && chain && !several { crate::ops::modifytable::merge::admit::built_target(gx, k, w) } else { None }).or_else(|| union.as_ref().map(|f| f[0]));
    if !own && built.is_none() || !chain {
        crate::ops::modifytable::merge::admit::unbuilt(gx, k, w, node); // D882: by name (M7's write.*)
    }
    if !projected && !matches!(p.source, Source::Const) {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy("UPDATE: a child without its projection (the SET cells and the carried old row)") });
    }
    // after the write only RETURNING's projection (write.returning checks its shape)
    if w.returning.is_none() && p.ops.iter().skip_while(|o| !matches!(o, Op::Write(_))).nth(1).is_some() {
        crate::refuse!(node, Reason::Bug { payload: "an UPDATE with an op after its write and no RETURNING" });
    }
    (relid, routed.map(|r| (r.0, r.2)), built)
}

/// H-e (c): an UPDATE through a partitioned root whose row may leave its leaf gets its destinations (write.route's
/// `Route::moves`): a row its new values put outside its leaf moves (hosts `write_move.rs`, ExecCrossPartitionUpdate);
/// any other plan as it is. PG's ExecUpdateAct checks a routed leaf's constraint whatever the planner marked: a row may
/// leave it by a partition key column (cross_partition), by a BEFORE ROW UPDATE trigger's NEW (D687), or by an ancestor's
/// key when the target root is itself a partition (D820: ExecFindPartition then names the root, as PG).
fn moving(gx: &Gx<'_, '_>, node: i32, w: &WriteDesc<'_>, plan: WritePlan, a: ArenaRef) -> WritePlan {
    let Some(r) = w.route.as_ref().filter(|_| w.members.has(Member::Route)) else { return plan };
    let root = gx.facts.rel_ext(r.root_relid);
    let trig = may_move(gx.facts.trig_ext(r.root_relid)) || w.leaves.iter().any(|l| l.has_triggers && may_move(l.trig));
    if !(r.cross_partition || trig || root.is_partition || !root.known) {
        return plan;
    }
    let moves = crate::frame::iface::need_strategy::<Route>(&node, WRITE_ROUTE).moves(gx, node, w);
    plan.with_moves(a, &moves)
}

/// D624: why the host would not maintain index `d` as ExecInsertIndexTuples does on a non-HOT update, or None: not valid
/// or not ready (hosts `index_maint::unmaintainable`), or outside the frozen `plans` (IndexMaint walks those alone, never
/// the relation's index list), named by the class that kept it out (`IndexDesc::plain`).
fn unmaintained(gx: &Gx<'_, '_>, plans: &[IndexPlan], d: &IndexDesc) -> Option<&'static str> {
    let e = gx.facts.index_ext(d.indexid);
    // D624: an index a concurrent CREATE / DROP / REINDEX CONCURRENTLY left not valid or not ready is outside the frozen
    // plans (get_relation_info skips it), so the host's IndexMaint takes the executor's own path (D765's generic:
    // ExecOpenIndices, ExecInsertIndexTuples maintaining a ready one and skipping a not-ready one), as PostgreSQL does
    if e.known && !(e.valid && e.ready) || plans.iter().any(|p| p.index_oid == d.indexid) {
        None
    } else if d.deferrable {
        Some("UPDATE: a deferrable index of the target outside the maintained index plans (D624)")
    } else if d.partial {
        Some("UPDATE: a partial index of the target outside the maintained index plans (D624)")
    } else if d.expression {
        Some("UPDATE: an expression index of the target outside the maintained index plans (D624)")
    } else if d.include {
        Some("UPDATE: an INCLUDE index of the target outside the maintained index plans (D624)")
    } else {
        Some("UPDATE: an index of the target whose access method or uniqueness the maintained index plans leave out (D624)")
    }
}

/// A trigger that fires on UPDATE (ExecUpdate's BEFORE / AFTER ROW, INSTEAD OF, the statement triggers even at zero rows,
/// transition capture; RI's check and action triggers are AFTER ROW), or facts the source could not answer.
fn fires(t: TrigExt) -> bool {
    !t.known || (t.before_row | t.after_row | t.instead_row | t.statement | t.transition) & TRIG_UPDATE != 0
}

/// A BEFORE ROW UPDATE trigger (its NEW may change the partition key), or facts the source could not answer.
fn may_move(t: TrigExt) -> bool {
    !t.known || t.before_row & TRIG_UPDATE != 0
}

#[cfg(test)]
mod tests {
    use super::{fires, may_move};
    use gravity_decompose::ops::modifytable::trig_facts::{TrigExt, TRIG_DELETE, TRIG_INSERT, TRIG_UPDATE};

    /// Through a partitioned root, only a BEFORE ROW UPDATE trigger (or unknown facts) may move the row
    /// (partition-key-update-1: the leaf's trigger sets NEW's key; PG moves the row, the host would raise 23514).
    #[test]
    fn before_row_update_may_move() {
        assert!(!may_move(TrigExt::NONE));
        assert!(may_move(TrigExt::UNKNOWN));
        assert!(may_move(TrigExt { before_row: TRIG_UPDATE, ..TrigExt::NONE }));
        assert!(!may_move(TrigExt { after_row: TRIG_UPDATE, statement: TRIG_UPDATE, ..TrigExt::NONE }));
        assert!(!may_move(TrigExt { before_row: TRIG_INSERT | TRIG_DELETE, ..TrigExt::NONE }));
    }

    /// Every trigger kind that fires on UPDATE refuses, RI's included (a referencing table's check triggers and a referenced
    /// table's action triggers both fire AFTER UPDATE); a DELETE / INSERT-only trigger does not (G4); unknown facts refuse.
    #[test]
    fn update_triggers_refuse_by_command() {
        assert!(!fires(TrigExt::NONE));
        assert!(fires(TrigExt::UNKNOWN));
        let sets: [fn(&mut TrigExt); 5] = [|t| t.before_row = TRIG_UPDATE, |t| t.after_row = TRIG_UPDATE, |t| t.instead_row = TRIG_UPDATE, |t| t.statement = TRIG_UPDATE, |t| t.transition = TRIG_UPDATE];
        for set in sets {
            let mut t = TrigExt::NONE;
            set(&mut t);
            assert!(fires(t));
        }
        let ri_check = TrigExt { after_row: TRIG_INSERT | TRIG_UPDATE, internal: TRIG_INSERT | TRIG_UPDATE, ..TrigExt::NONE };
        assert!(fires(ri_check));
        let delete_only = TrigExt { before_row: TRIG_DELETE, after_row: TRIG_DELETE | TRIG_INSERT, ..TrigExt::NONE };
        assert!(!fires(delete_only));
    }
}
