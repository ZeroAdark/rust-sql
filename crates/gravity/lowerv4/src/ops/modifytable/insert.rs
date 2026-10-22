//! Kind `write.insert` (plan §3.2; R12; cards/M4.md L4 `write/insert.rs`): PostgreSQL's ExecInsert for every row its
//! pipeline brings, over any source the other kinds serve: a Result (every cell a bind-time word), a VALUES list (the
//! literal table, write/literals.rs), a SELECT over a scan, a probe, a join or a drained breaker. Owner M4-INSERT.
//!
//! The write op is a step (face/end.rs `RowOpMech`, write/rowop_step.rs): per row the target's cells are the entries of
//! the projection the write reads (the ModifyTable child's target list: one non-junk entry per attribute in attno order,
//! a dropped column's NULL, a STORED generated column's placeholder), each cell's datum through its Rep's accessor, stored
//! into the write site with `LocFrom::None`; then ONE host call, `write_row(ctx, site, WRITE_INSERT)`, into the write host
//! the driver binds from the frozen write plan. The host does the rest as nodeModifyTable does: RowExclusiveLock,
//! CheckValidResultRel, ExecComputeStoredGenerated, ExecConstraints and ExecWithCheckOptions in PG's order (road
//! write.checks, write/checks_ir.rs: `ChecksAt::Host`), `heap_insert` under the statement's command id and
//! ExecInsertIndexTuples with its unique checks. `WRITE_DONE` goes on to the end; the end counts it (the command tag is
//! the driver's `add_rows`); a host error stops at the row, with the rows written before it counted (ENTRY §E3). An
//! INSERT under a constant-false gate never reaches its step: the statement still binds its write host (the program's
//! write plan) and inserts nothing, as PG.
//!
//! Refused by name, each by the kind (or road) that builds it (R-A):
//! - a target with a trigger that fires on INSERT (row, statement, transition capture, and the RI check triggers of a
//!   referencing table), on the target or on the partitioned root: `write.transition` (admission's props, and the
//!   step's hand-off to that kind's make, M4-TRIG); a view target (its INSTEAD OF trigger is the write) is the same kind;
//! - ON CONFLICT: `write.onconflict` (its plan; DO UPDATE's arm over the conflict site); row marks: `write.lock` (RETURNING is served: `done` is write.returning's
//!   continuation, the end its sink);
//! - a routed INSERT (a partitioned target: write.route's `Route::plan`, M4-ROUTE) or several result relations: `write.route`;
//! - a foreign or non-heap target: the AM refusal.
//! Leader-only (R12-L17): `claim` refuses a parallel-eligible pipeline (write/rowop.rs).
use gravity_abi::program::WRITE_INSERT;
use gravity_decompose::ops::modifytable::trig_facts::{TrigExt, TRIG_INSERT};
use gravity_decompose::catalog::typefacts::{self, TypeFacts};
use gravity_decompose::ops::shared::modifytable::{Member, WriteDesc};
use gravity_decompose::{Am, Op};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, Value};
use gravity_program::{ArenaRef, Cmd, WritePlan};
use mcx::{GrowIn, PgBox, PgVec};

use crate::face::end::{ChecksAt, EndMech, LocFrom, RowOpDecl, RowOpMech, RowOpRow};
use crate::face::kinds::KindId;
use crate::face::strategy::{WRITE_CHECKS, WRITE_ROUTE};
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{EndMaker, Next, StepMaker};
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy, Variant};
use crate::ops::Table;

use crate::ops::modifytable::face::RetMech;
use crate::ops::modifytable::strategies::checks_ir::Checks;

/// `write.insert` has two faces: the write op in the chain (the row op as a step) and the pipeline's end.
pub(crate) const ROWS: Table = Table { kinds: &[(KindId::WriteInsert, &StepMaker(make_step)), (KindId::WriteInsert, &EndMaker(make_end))], ..crate::ops::NONE };

/// One target attribute's cell: the projection entry that carries it and its type's facts.
struct Col {
    entry: usize,
    tf: TypeFacts,
}

struct InsertStep<'g> {
    op: PgBox<'g, dyn RowOpMech + 'g>,
    cols: PgVec<'g, Col>,
    node: i32,
    /// RETURNING: write.returning's continuation over the written row.
    ret: Option<PgBox<'g, dyn RetMech + 'g>>,
    /// ON CONFLICT DO UPDATE: write.onconflict's arm over the conflict site (L3b).
    arm: Option<PgBox<'g, dyn crate::ops::modifytable::onconflict::ConflictArm + 'g>>,
}

fn make_step<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Some(Op::Write(w)) = p.ops.get(j) else { crate::refuse!(node, Reason::Bug { payload: "write.insert's step over an op that is not the write" }) };
    let (relid, plan) = admit(gx, k, w, px.arena);
    // write.transition (M4-TRIG): the host fires a firing (or unknown) trigger set (hosts trigger*.rs, write_view.rs); the
    // trigger plan's make declares it (ProtoKind::Transition) and refuses by name what the host does not fire.
    // NotBuilt(write.transition) until M4-TRIG registers it. Its step is the identity: made, never lowered.
    if fires(gx.facts.trig_ext(relid)) {
        drop(super::triggers::make_plan(px, k, j));
    }
    let cols = cols(gx, k, j, w.site.natts(), node);
    let arm = w.on_conflict.as_ref().is_some_and(|oc| oc.update).then(|| super::onconflict::arm(px, k, j));
    // M4-RET R4a: RETURNING OLD under DO UPDATE reads the existing row the arm replaced, which the host fetches after the
    // arm's update (the conflict site's successor); a plain INSERT's OLD is NULL (no fetch)
    let plan = if arm.is_some() && w.returning.is_some() && w.old_new.old { plan.with_old() } else { plan };
    // V4R-501: and its system columns (the existing row's; NULL for an inserted row)
    let plan = if crate::ops::modifytable::ret_sys::sys_old_plan(w) { plan.with_sys_old() } else { plan };
    // D774: RETURNING's system columns of the inserted row (the routed leaf's), handed back by the host
    let plan = match crate::ops::modifytable::ret_sys::sys_plan(w) {
        Some(header) => plan.with_sys(header),
        None => plan,
    };
    let decl = RowOpDecl { op: WRITE_INSERT, relid, natts: cols.len() as u16, plan, loc: LocFrom::None, conflict: arm.is_some() };
    let op = super::rowop_step::make_op(px, k, decl);
    let ret = w.returning.is_some().then(|| super::returning::make_ret(px, k, j));
    mcx::pg_box_dyn!(px.mcx(), InsertStep { op, cols, node, ret, arm } => dyn StepMech + 'g)
}

impl StepMech for InsertStep<'_> {
    fn prologue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _x: &Exits) {
        self.op.open(b, env);
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let t = b.bconst(true);
        let mcx = env.mcx;
        let mut cx = env.lx(b, self.node);
        let mut cells = mcx::reserved_in(self.cols.len(), mcx);
        for c in self.cols.iter() {
            let Some(cr) = row.cell(c.entry) else { crate::refuse!(self.node, Reason::Bug { payload: "an INSERT cell past the row" }) };
            let (d, null) = crate::rep::datum(&mut cx, cr, &c.tf);
            let valid = cx.b.int_bin(IntBinOp::Xor, null, t);
            cells.push_in((d, valid));
        }
        env.memo = cx.memo;
        let (op, ret, next) = (&*self.op, self.ret.as_deref(), core::cell::RefCell::new(next));
        // after the insert (`conflict` false) or DO UPDATE's update (true): RETURNING reads that site's written row
        let then = |b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, r: &RowVals, conflict: bool| {
            let n = &mut *next.borrow_mut();
            let Some(ret) = ret else { return n(b, env, r) };
            let written = op.cells(b, env, conflict);
            let old = conflict.then(|| op.old_cells(b, env));
            ret.lower_old(b, env, r, &written, old.as_deref().unwrap_or(&[]), &mut **n)
        };
        let mut on = |b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, r: &RowVals| match &self.arm {
            Some(arm) => arm.lower(b, env, r, &cells[..], op, &mut |b, env, r| then(b, env, r, true)),
            None => crate::refuse!(self.node, Reason::Bug { payload: "a conflict arm without DO UPDATE" }),
        };
        let conflict: Option<Next<'_, 'e>> = if self.arm.is_some() { Some(&mut on) } else { None };
        op.call(b, env, row, RowOpRow { cells: &cells[..], tid: None, rel: None }, &mut |b, env, r| then(b, env, r, false), conflict)
    }
}

/// The end of an INSERT without RETURNING: the rows the host wrote reach it and are counted (the pipeline's out), and
/// nothing is emitted.
struct InsertEnd;

impl EndMech for InsertEnd {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        let out = b.int_bin(IntBinOp::Add, row.carry.out, env.one);
        Carry { out, accs: mcx::to_vec_in(&row.carry.accs[..], env.mcx) }
    }
}

fn make_end<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let p = &px.gx.pipelines()[k];
    if !p.ops.iter().any(|o| matches!(o, Op::Write(w) if w.cmd == Cmd::Insert)) {
        crate::refuse!(p.node_id, Reason::Bug { payload: "write.insert's end over a pipeline that does not insert" });
    }
    if p.ops.iter().any(|o| matches!(o, Op::Write(w) if w.returning.is_some())) {
        return super::returning::make_end(px, k);
    }
    mcx::pg_box_dyn!(px.mcx(), InsertEnd => dyn EndMech + 'g)
}

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// The cells write op `j` reads: the projection before it, one non-junk entry per target attribute in attno order.
fn cols<'g>(gx: &Gx<'g, '_>, k: usize, j: usize, natts: usize, node: i32) -> PgVec<'g, Col> {
    let p = &gx.pipelines()[k];
    let Some(list) = p.ops[..j].iter().rev().find_map(|o| if let Op::Project(l) = o { Some(*l) } else { None }) else {
        shape(node, "an INSERT whose rows come without a projection")
    };
    let mut cols = mcx::reserved_in(natts, gx.mcx);
    for (i, n) in list.iter().enumerate() {
        let Some(te) = n.as_target_entry() else { shape(node, "an INSERT projection entry that is not a TargetEntry") };
        if te.resjunk {
            continue;
        }
        if te.resno as usize != cols.len() + 1 {
            shape(node, "an INSERT projection whose entries are not the target's attributes in order");
        }
        let typ = gravity_decompose::catalog::expr_type::type_of(te.expr).unwrap_or_else(|| gravity_decompose::catalog::expr_type::result_type(te.expr));
        let Some(tf) = typefacts::facts_of(gx.facts, typ) else { shape(node, "an INSERT cell type the catalog does not know") };
        cols.push_in(Col { entry: i, tf });
    }
    if cols.len() != natts {
        shape(node, "an INSERT row that is not the relation's width");
    }
    cols
}

fn refuse_kind(node: i32, k: KindId) -> ! {
    crate::not_built!(node, Variant::Kind(k))
}

/// IN-CONFLICT: every SubPlan of write `w`'s own lists is its ON CONFLICT DO UPDATE's (none in its checks or RETURNING).
fn conflict_only(w: &WriteDesc<'_>) -> bool {
    let sub = |n: types_nodes::Node<'_>| n.as_list().map_or_else(|| gravity_decompose::expr::has_subplan(n), |l| l.iter().any(gravity_decompose::expr::has_subplan));
    let ret = |r: Option<&types_nodes::NodeList<'_>>| r.is_some_and(|r| r.iter().any(sub));
    w.on_conflict.as_ref().is_some_and(|oc| oc.update) && w.merge.is_none() && !w.checks.iter().any(|c| sub(c.qual)) && !ret(w.returning) && !w.targets.iter().any(|t| ret(t.returning))
}

/// Every refusal by name of the module doc, and the checks' (write.checks); the target relation when the INSERT serves.
fn admit(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>, arena: ArenaRef) -> (u32, WritePlan) {
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    if w.cmd != Cmd::Insert {
        crate::refuse!(node, Reason::Bug { payload: "write.insert over a write that is not an INSERT" });
    }
    for m in w.members.iter() {
        match m {
            Member::Insert | Member::Generated | Member::GeneratedVirtual | Member::CheckView | Member::CheckRls => {}
            // the checks plan answers the index members (write/checks_ir.rs)
            Member::IndexUnique | Member::IndexPlain | Member::IndexDeferrable | Member::IndexExclusion | Member::IndexPartial | Member::IndexExpression | Member::IndexInclude | Member::IndexNonBtree => {}
            // RETURNING: write.returning's continuation (OLD / NEW reach here only once decompose binds them, and refuse there by name)
            Member::Returning | Member::ReturningOld | Member::ReturningNew => {}
            // W6-D1112: a check option's SubPlan is the host qual's read (write/rowop_sub.rs); any other keeps D1112
            Member::WriteSubPlan if crate::ops::modifytable::strategies::checks_ir::check_only(w) => {}
            // IN-CONFLICT: one only ON CONFLICT DO UPDATE reads is the conflict arm's read (nest.subplan lends it with no
            // row and refuses by name one it cannot, a hashed one)
            Member::WriteSubPlan if conflict_only(w) => {}
            // D1112: one only RETURNING reads is a step after the write (write.returning `proj_at`)
            Member::WriteSubPlan if super::returning::returning_only(w) => {}
            // D1112 (rxnested): both, each as it is alone (write.returning `returning_and_checks`)
            Member::WriteSubPlan if super::returning::returning_and_checks(w) => {}
            Member::WriteSubPlan => crate::refuse!(node, Reason::Shape { why: ShapeWhy("a SubPlan in a write's own expressions, a check, RETURNING, ON CONFLICT or a MERGE action (D1112: nest.subplan B4)") }),
            // ON CONFLICT: write.onconflict's plan below, and DO UPDATE's arm in make_step
            Member::ConflictNothing | Member::ConflictUpdate => {}
            // an INSERT's ModifyTable.rowMarks is NIL (preprocess_rowmarks; a SELECT's FOR UPDATE stays in its sub-query)
            Member::WriteMarks | Member::LockingWriteMark | Member::ParentMark | Member::MultiLock => crate::refuse!(node, Reason::Bug { payload: "an INSERT carrying row marks" }),
            // a view target: its INSTEAD OF INSERT trigger is the write (the host's ViewInsert takes the view's natts
            // cells); the write.transition gate in make_step covers it (an INSTEAD OF row trigger fires)
            Member::TargetView => {}
            // a routed INSERT (a partitioned target): write.route's plan below (M4-ROUTE); several targets are UPDATE / DELETE's
            Member::Route => {}
            Member::Targets | Member::CrossPartition => crate::refuse!(node, Reason::Bug { payload: "an INSERT of several result relations" }),
            // PGRCSERVE G2: a writable AM's target is written through its table AM (hosts `insert_via_am`)
            Member::TargetAm => {}
            Member::TargetForeign | Member::TargetOtherAm | Member::DirectForeign => crate::refuse!(node, Reason::UnsupportedAm { relid: w.plan.target_rel }),
            Member::Update | Member::Delete | Member::Merge | Member::WholeRow | Member::MergeActionFn => crate::refuse!(node, Reason::Bug { payload: "an INSERT carrying an UPDATE / DELETE / MERGE member" }),
        }
    }
    if !w.marks.is_nil() || !w.mark_descs.is_empty() {
        crate::refuse!(node, Reason::Bug { payload: "an INSERT carrying row marks" });
    }
    if w.route.is_none() && w.targets.len() != 1 {
        crate::refuse!(node, Reason::Bug { payload: "an INSERT of several result relations" });
    }
    let mut plan = w.plan.copy_in(arena);
    if w.on_conflict.is_some() {
        plan = super::onconflict::plan(gx, node, w, plan, arena);
    }
    if w.route.is_some() {
        // write.route sets the frozen plan's route (WritePlan::with_route) or refuses by name (M4-ROUTE R1 registered the
        // road), before any check of the root below
        plan = crate::frame::iface::need_strategy::<crate::ops::modifytable::strategies::route::Route>(&node, WRITE_ROUTE).plan(gx, node, w, plan, arena);
    }
    let relid = w.plan.target_rel;
    // a partitioned root has no storage (its facts read am none): write.route's plan checks every leaf's AM itself
    if w.route.is_none() && !w.members.has(Member::TargetView) && gx.facts.rel(relid).is_some_and(|r| r.am != Am::Heap && !gravity_decompose::ops::modifytable::writable_am(&r)) {
        crate::refuse!(node, Reason::UnsupportedAm { relid });
    }
    // after the write only RETURNING's projection (write.returning checks its shape)
    if w.returning.is_none() && p.ops.iter().skip_while(|o| !matches!(o, Op::Write(_))).nth(1).is_some() {
        crate::refuse!(node, Reason::Bug { payload: "an INSERT with an op after its write and no RETURNING" });
    }
    if crate::frame::iface::need_strategy::<Checks>(&node, WRITE_CHECKS).plan(gx, node, w) != ChecksAt::Host {
        crate::refuse!(node, Reason::Bug { payload: "an IR checks plan at M4's first landing" });
    }
    (relid, plan)
}

/// A trigger that fires on INSERT (BEFORE / AFTER ROW, INSTEAD OF, the statement triggers even at zero rows, transition
/// capture; a referencing table's RI checks are AFTER ROW), or facts the source could not answer.
fn fires(t: TrigExt) -> bool {
    !t.known || (t.before_row | t.after_row | t.instead_row | t.statement | t.transition) & TRIG_INSERT != 0
}

#[cfg(test)]
mod tests {
    use super::fires;
    use gravity_decompose::ops::modifytable::trig_facts::{TrigExt, TRIG_DELETE, TRIG_INSERT, TRIG_UPDATE};

    /// Every trigger kind that fires on INSERT refuses, the RI check triggers of a referencing table included; a DELETE /
    /// UPDATE-only trigger (a referenced table's RI actions) does not; facts the source could not answer refuse.
    #[test]
    fn insert_triggers_refuse_by_command() {
        assert!(!fires(TrigExt::NONE));
        assert!(fires(TrigExt::UNKNOWN));
        let sets: [fn(&mut TrigExt); 5] = [|t| t.before_row = TRIG_INSERT, |t| t.after_row = TRIG_INSERT, |t| t.instead_row = TRIG_INSERT, |t| t.statement = TRIG_INSERT, |t| t.transition = TRIG_INSERT];
        for set in sets {
            let mut t = TrigExt::NONE;
            set(&mut t);
            assert!(fires(t));
        }
        let ri_check = TrigExt { after_row: TRIG_INSERT | TRIG_UPDATE, internal: TRIG_INSERT | TRIG_UPDATE, ..TrigExt::NONE };
        assert!(fires(ri_check));
        let ri_action = TrigExt { after_row: TRIG_DELETE | TRIG_UPDATE, internal: TRIG_DELETE | TRIG_UPDATE, ..TrigExt::NONE };
        assert!(!fires(ri_action));
    }
}
