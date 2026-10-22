//! The DELETE row op, kind `write.delete` (plan §3.2; R12; the ladder's rung-10 DELETE, lower/src/write.rs `probe_write` at
//! lowerv2-final), served through the row-op end (write/rowop.rs). PostgreSQL's ExecDelete for what M1 serves: a DELETE of
//! one heap relation over its own scan, whose target fires no trigger. The write host does the rest exactly as
//! nodeModifyTable does: RowExclusiveLock on the target (and on the partitioned root a pruned DELETE goes through),
//! CheckValidResultRel's publication check, `heap_delete` under the statement's command id, the TM_Result mapping incl. the
//! READ COMMITTED EvalPlanQual recheck and the REPEATABLE READ serialization failure. A DELETE maintains no index (ExecDelete
//! opens none; decompose's write plan carries none), so every index kind of the target serves.
//!
//! Refused by name, each by the kind (or road) that builds it (R-A; `serve.tsv` places each at that milestone or later):
//! - a target with a trigger that fires on DELETE (row BEFORE / AFTER, INSTEAD OF, statement, transition tables, and RI's
//!   action triggers on a referenced table), on the target or on the partitioned root it is pruned through; unknown trigger
//!   facts count as triggers: `write.transition` (M4; props needs it at admission, and the end hands it to that kind's
//!   step, L0-e). A view target (its INSTEAD OF trigger is the write) is the same kind's end.
//! - RETURNING: handed to `write.returning` (L0-e): its step does the row op and projects, its end is the sink.
//! - row marks (EPQ over joined or locked relations: DELETE ... USING, IN over a subquery), and a write over more than its
//!   target's own scan: `write.lock`'s end (D882, write/delete_built.rs: the target hashed on a join's build side; the
//!   rest refused by name, M7's write.*).
//! - several result relations (inheritance, a partitioned target not pruned to one leaf): `write.route` (M4).
//! Rules on the target serve: the rewriter already applied them, and the extra rule queries are statements of their own.
use gravity_abi::program::WRITE_DELETE;
use gravity_decompose::ops::modifytable::trig_facts::{TrigExt, TRIG_DELETE};
use gravity_decompose::ops::shared::modifytable::{Member, WriteDesc};
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::{Am, Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, Value};
use gravity_program::{ArenaRef, Cmd, WritePlan};
use mcx::PgBox;

use crate::face::end::{Arm, EndMech, RowOpDecl, RowOpMech, RowOpRow, SinkKind, TargetsRoad};
use crate::face::kinds::KindId;
use crate::face::strategy::WRITE_ROUTE;
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{EndMaker, Next, StepMaker};
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::graph::Gx;
use crate::admit::refusal::Reason;

use crate::ops::modifytable::rowop as write_rowop;
use crate::ops::modifytable::rowop::RowOp;

/// `write.delete` has two faces: the row-op end (the pipeline's end) and the write op in the chain (an identity step:
/// the end does the write).
pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::WriteDelete, &EndMaker(make_end)), (KindId::WriteDelete, &StepMaker(make_step))], ..crate::ops::NONE };

struct WriteStep;

impl StepMech for WriteStep {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        next(b, env, row)
    }
}

const WRITE_SUBPLAN: &str = "a SubPlan in a write's own expressions, a check, RETURNING, ON CONFLICT or a MERGE action (D1112: nest.subplan B4)";

fn make_step<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    // L0-e: a DELETE with RETURNING is the row op as a step, then the projection (write.returning)
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    // D1112: a SubPlan in the RETURNING list is a step after this one (write.returning `proj_at`); a write SubPlan any other
    // list reads refuses first, as admit does below
    if matches!(p.ops.get(j), Some(Op::Write(w)) if w.members.has(Member::WriteSubPlan) && !super::returning::returning_only(w)) {
        crate::refuse!(p.node_id, Reason::Shape { why: crate::admit::refusal::ShapeWhy(WRITE_SUBPLAN) });
    }
    if let Some(Op::Write(w)) = p.ops.get(j).filter(|o| matches!(o, Op::Write(w) if w.returning.is_some())) {
        // D655 Row C: a data-modifying CTE's DELETE ends in nest.cte's store, never in make_end: its admit and trigger plan
        // are made here; a view's are not
        if gx.cte_write(k) && !w.members.has(Member::TargetView) {
            let (relid, _, _) = admit(gx, k, w, px.arena);
            trigger_plan(px, k, w, relid);
        }
        return super::returning::make_delete(px, k, j);
    }
    mcx::pg_box_dyn!(px.mcx(), WriteStep => dyn StepMech + 'g)
}

fn make_end<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Some(w) = p.ops.iter().find_map(|o| if let Op::Write(w) = o { Some(w) } else { None }) else {
        crate::refuse!(node, Reason::Bug { payload: "a row-op end without its write op" })
    };
    if w.cmd != Cmd::Delete {
        crate::refuse!(node, Reason::Bug { payload: "write.delete's end over a write that is not a DELETE" });
    }
    // L0-e: a view target (member TargetView): its INSTEAD OF row triggers are the write (hosts write_view_rows.rs, H-d).
    // write.transition makes the end (the view's cells, then the wholerow OLD); NotBuilt(write.transition) by name until
    // M4-TRIG registers it
    // D667: under RETURNING the view's row op is write.transition's step (write.returning's DELETE step hands it over); the
    // end is write.returning's sink
    if w.members.has(Member::TargetView) {
        return if w.returning.is_some() { super::returning::make_end(px, k) } else { super::triggers::make_view_end(px, k) };
    }
    let (relid, plan, own) = admit(gx, k, w, px.arena);
    // L0-e: a firing (or unknown) trigger set is write.transition's (M4-TRIG, hosts trigger*.rs): the trigger plan's make
    // declares it and refuses by name what the host does not fire; NotBuilt(write.transition) until M4-TRIG registers it.
    // Its step is the identity, so it is made and not lowered
    trigger_plan(px, k, w, relid);
    // D882: rows that are not the target's own scan rows (a join's, a subquery's): write.lock's end (write/delete_built.rs:
    // the target hashed on a join's build side, D838's DELETE half; else refused by name)
    if !own {
        return super::delete_built::make_end(px, k);
    }
    // L0-e: RETURNING's end is write.returning's (end.sink; its step did the row op and projects)
    if w.returning.is_some() {
        return super::returning::make_end(px, k);
    }
    // D652-S1 (write/rowop_epq.rs `subs_in_ir`): over SubPlan steps the recheck is the IR's, the row op the step's mech
    if super::rowop_epq::in_ir(gx, k) {
        let j = p.ops.iter().position(|o| matches!(o, Op::Write(_))).unwrap_or(0);
        let (loc, tid) = crate::ops::modifytable::update_built::locate(px, node, w, None);
        // the site is as wide as the target: WRITE_EPQ_ROW puts the newest version in its cells, which the redo reads
        // attribute by attribute (epq_ir.rs); a site of no cells left them unwritten and the redo read past it, so a
        // rechecked row's WHERE saw no newest version and the row stayed (PG deletes it when that version still passes)
        let natts = super::rowop_epq::epq_width(w) as u16;
        let op = super::rowop_step::make_op(px, k, RowOpDecl { op: WRITE_DELETE, relid, natts, plan, loc, conflict: false });
        let epq = crate::rerun::epq::epq_ir::epq_of_subs(px, k, j, node, relid, crate::rerun::epq::epq_ir::ROWC_SYS);
        return mcx::pg_box_dyn!(px.mcx(), DeleteEnd { op, tid, node, epq } => dyn EndMech + 'g);
    }
    write_rowop::end(px, k, RowOp { op: WRITE_DELETE, relid, natts: 0, plan })
}

/// D652-S1: the emit's DELETE that rechecks in the IR (write/rowop_epq.rs `subs_in_ir`): the row op's step mech, its
/// WRITE_RECHECK the kind's `Arm::Epq` (no cells: a DELETE writes none), the row counted as M1's end counts it.
struct DeleteEnd<'g> {
    op: PgBox<'g, dyn RowOpMech + 'g>,
    tid: crate::ops::modifytable::update_built::Tid,
    node: i32,
    epq: crate::rerun::epq::epq_ir::Epq<'g>,
}

impl EndMech for DeleteEnd<'_> {
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        self.op.open(b, env);
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        let out = b.int_bin(IntBinOp::Add, row.carry.out, env.one);
        let at = RowVals { row: row.row.clone(), vals: row.vals.clone(), cols: row.cols.clone(), carry: Carry { out, accs: mcx::to_vec_in(&row.carry.accs[..], env.mcx) }, next: row.next, at: row.at };
        let tid = crate::ops::modifytable::update_built::junk_tid(&self.tid, self.node, b, env, &at);
        let mut none = |_: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _: &RowVals| mcx::PgVec::<(Value, Value)>::new_in(env.mcx);
        self.op.call_op(b, env, &at, WRITE_DELETE, RowOpRow { cells: &[], tid, rel: None }, &mut |_, env, r| r.carry.copy_in(env.mcx), Arm::Epq(&self.epq, &mut none))
    }

    fn sink(&self) -> SinkKind {
        SinkKind::RowOp
    }
}

/// make_end's trigger plan (L0-e, its comment), also a data-modifying CTE's DELETE's (make_step).
fn trigger_plan(px: &mut Px<'_, '_>, k: usize, w: &WriteDesc<'_>, relid: u32) {
    let gx = px.gx;
    if fires(gx.facts.trig_ext(relid)) || w.route.is_some_and(|r| fires(gx.facts.trig_ext(r.root_relid))) {
        let j = gx.pipelines()[k].ops.iter().position(|o| matches!(o, Op::Write(_))).unwrap_or(0);
        drop(super::triggers::make_plan(px, k, j));
    }
}

/// Every refusal by name of the module doc; the target relation and the write plan when the DELETE serves.
fn admit(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>, a: ArenaRef) -> (u32, WritePlan, bool) {
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    for m in w.members.iter() {
        match m {
            Member::Delete | Member::Route | Member::Generated | Member::GeneratedVirtual => {}
            Member::IndexUnique | Member::IndexPlain | Member::IndexDeferrable | Member::IndexExclusion | Member::IndexPartial | Member::IndexExpression | Member::IndexInclude | Member::IndexNonBtree => {}
            // RETURNING: write.returning (L0-e; OLD / NEW reach here only once decompose binds them, and refuse there by name)
            Member::Returning | Member::ReturningOld | Member::ReturningNew => {}
            // D1112: one only RETURNING reads is a step after the write (write.returning `proj_at`)
            Member::WriteSubPlan if super::returning::returning_only(w) => {}
            Member::WriteSubPlan => crate::refuse!(node, Reason::Shape { why: crate::admit::refusal::ShapeWhy(WRITE_SUBPLAN) }),
            // D655: a joined relation's REFERENCE mark (write/rowop.rs `claim`, marks.rs); a parent's is M7's (D1000)
            Member::WriteMarks => {}
            Member::ParentMark => crate::rerun::epq::marks::parent_mark(node, w.mark_descs),
            Member::TargetView | Member::LockingWriteMark | Member::MultiLock => crate::refuse!(node, Reason::Bug { payload: "a DELETE admit over a view (handed off before) or an unbound lock member" }),
            Member::Targets => {}
            // a DELETE has no WITH CHECK OPTION (rewriteTargetView / get_row_security_policies add them to INSERT / UPDATE / MERGE)
            Member::CheckView | Member::CheckRls => crate::refuse!(node, Reason::Bug { payload: "a DELETE carrying a WITH CHECK OPTION" }),
            Member::TargetAm => {}
            Member::TargetForeign | Member::TargetOtherAm | Member::DirectForeign => crate::refuse!(node, Reason::UnsupportedAm { relid: w.plan.target_rel }),
            Member::Insert | Member::Update | Member::Merge | Member::ConflictNothing | Member::ConflictUpdate | Member::CrossPartition | Member::WholeRow | Member::MergeActionFn => {
                crate::refuse!(node, Reason::Bug { payload: "a DELETE carrying an INSERT / UPDATE / MERGE member" })
            }
        }
    }
    if !w.checks.is_empty() {
        crate::refuse!(node, Reason::Bug { payload: "a DELETE carrying a WITH CHECK OPTION" });
    }
    // several result relations (write.route, M4-ROUTE R2): this pipeline is one arm of the Append decompose streamed the
    // write into (V4R-214), over its own target's rows; write.route names that target and the plan every arm shares
    let several = w.targets.len() > 1;
    let (rti, relid, plan) = match several {
        true => (crate::frame::iface::need_strategy::<TargetsRoad>(&node, WRITE_ROUTE).0)(gx, k, w, a),
        // one leaf of a partitioned root pruned to it (D665), or an inheritance tree's one result relation (V4R-214): the
        // frozen plan names the root (WritePlan::with_route, no leaves), so the host locks it and fires its statement
        // triggers (ExecInitModifyTable's rootRelation, fireBSTriggers / fireASTriggers on it, at zero rows too), never the leaf's
        false => (w.rti, w.plan.target_rel, match &w.route {
            Some(r) if w.members.has(Member::Route) || w.members.has(Member::Targets) => w.plan.copy_in(a).with_route(a, r.root_relid, &[]),
            _ => w.plan.copy_in(a),
        }),
    };
    // D829: a partitioned root pruned to no leaf is the result relation itself over the dummy Result (dummy.rs pruned_root)
    if !crate::ops::shared::dummy::pruned_root(gx, k, w) && gx.facts.rel(relid).is_some_and(|r| r.am != Am::Heap && !r.rows) {
        crate::refuse!(node, Reason::UnsupportedAm { relid });
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
    let chain = p.ops.iter().take_while(|o| !matches!(o, Op::Write(_))).all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::SubPlan(_)) || arm(o) || crate::rerun::epq::marks::write_joins(o));
    // producerchain: or the outer of INNER nested loops over REFERENCE-marked relations' heap scans (write/nest_ref.rs)
    let chain = chain || !several && crate::ops::modifytable::nest_ref::chain(gx, k, w);
    // D882: otherwise write.lock's end (make_end); D1933 (c): or the outer of one nested loop over a Material (nest_outer.rs)
    // producerchain ARM: or one arm of several targets' streamed union over a nest copy (nest_outer.rs decides)
    let own = own && (chain || crate::ops::modifytable::nest_outer::admits(gx, k, w));
    // after the write only RETURNING's projection (write.returning checks its shape)
    if w.returning.is_none() && p.ops.iter().skip_while(|o| !matches!(o, Op::Write(_))).skip(1).next().is_some() {
        crate::refuse!(node, Reason::Bug { payload: "a DELETE with an op after its write and no RETURNING" });
    }
    (relid, plan, own)
}

/// A trigger that fires on DELETE (ExecDelete's BEFORE / AFTER ROW, INSTEAD OF, the statement triggers even at zero
/// rows, transition capture; RI's action triggers are AFTER ROW), or facts the source could not answer.
fn fires(t: TrigExt) -> bool {
    !t.known || (t.before_row | t.after_row | t.instead_row | t.statement | t.transition) & TRIG_DELETE != 0
}

#[cfg(test)]
mod tests {
    use super::fires;
    use gravity_decompose::ops::modifytable::trig_facts::{TrigExt, TRIG_DELETE, TRIG_INSERT, TRIG_UPDATE};

    /// Every trigger kind that fires on DELETE refuses; an INSERT / UPDATE-only trigger (a referencing table's RI checks)
    /// does not; facts the source could not answer refuse.
    #[test]
    fn delete_triggers_refuse_by_command() {
        assert!(!fires(TrigExt::NONE));
        assert!(fires(TrigExt::UNKNOWN));
        let sets: [fn(&mut TrigExt); 5] = [|t| t.before_row = TRIG_DELETE, |t| t.after_row = TRIG_DELETE, |t| t.instead_row = TRIG_DELETE, |t| t.statement = TRIG_DELETE, |t| t.transition = TRIG_DELETE];
        for set in sets {
            let mut t = TrigExt::NONE;
            set(&mut t);
            assert!(fires(t));
        }
        let ri_check = TrigExt { after_row: TRIG_INSERT | TRIG_UPDATE, internal: TRIG_INSERT | TRIG_UPDATE, ..TrigExt::NONE };
        assert!(!fires(ri_check));
        let ri_action = TrigExt { after_row: TRIG_DELETE | TRIG_UPDATE, internal: TRIG_DELETE | TRIG_UPDATE, ..TrigExt::NONE };
        assert!(fires(ri_action));
    }
}
