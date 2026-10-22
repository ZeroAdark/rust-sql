//! Kind `write.transition` (cards/M4.md `write/triggers.rs`; M4-plan §3.6, Decision 8): a write whose target fires
//! triggers. The host fires every trigger as nodeModifyTable does (hosts `trigger.rs`, `trigger_before.rs`,
//! `trigger_stmt.rs`, `write_view.rs`, `write_view_rows.rs`): BEFORE ROW with the slot trip only when one exists (a NULL
//! makes the row Skipped), AFTER ROW queued by ctid, the statement triggers at bind / finish even at zero rows,
//! transition capture into the AFTER-trigger query's tuplestores. RI's checks and actions are AFTER ROW triggers (an FK
//! INSERT is this kind); their SPI statements are admitted on their own at the next re-entry level. The IR part:
//! - The trigger plan (`StepMaker`). The row-op kinds (write/insert.rs, update.rs, delete.rs) make it at their write op
//!   when a trigger on the command fires, or when the facts are unknown. Its make refuses by name what the host does not
//!   fire, and declares `ProtoKind::Transition`. Its step is the identity: the row op's `done` arm already carries the
//!   host's row (face/end.rs `RowOpMech::cells`, a BEFORE ROW trigger's row). So RETURNING reads the row as written. A
//!   checks plan never runs in IR ahead of a BEFORE ROW trigger (write/checks_ir.rs: PG checks the trigger's row).
//! - A view's UPDATE / DELETE (`EndMaker`; member TargetView). The view's INSTEAD OF row triggers are the write (hosts
//!   `write_view_rows.rs`, H-d, Decision 8). The site carries the view's natts cells, then the `wholerow` junk (OLD):
//!   UPDATE's SET columns from the child's projection, the rest NULL (NEW = OLD with the SET columns replaced is the
//!   host's). The row goes through ExecIR{Update,Delete}Triggers; there is no heap, no ctid, no EPQ. V4R-330: an outer
//!   view's WITH CHECK OPTION over the UPDATE (WCO_VIEW_CHECK) is the host's ExecUpdateEpilogue check over the row the
//!   INSTEAD OF trigger returned (hosts write.rs `view_write`); an RLS check option over a view target stays refused.
//! A transition-table scan (`Source::Named`, a trigger body's `SELECT .. FROM new_table`) is srf.function's (V4R-213).
//! Created by M4 (owner M4-TRIG).
use gravity_abi::program::{WRITE_DELETE, WRITE_UPDATE};
use gravity_decompose::ops::modifytable::trig_facts::TrigExt;
use gravity_decompose::catalog::typefacts::{self, TypeFacts};
use gravity_decompose::ops::shared::modifytable::{Member, TargetKind, WriteDesc};
use gravity_decompose::Op;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, Value};
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::Cmd;

use crate::face::end::{ChecksAt, EndMech, LocFrom, RowOpDecl, RowOpMech, RowOpRow, SinkKind};
use crate::ops::modifytable::face::RetMech;
use crate::face::kinds::KindId;
use crate::face::strategy::WRITE_CHECKS;
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{EndMaker, Next, StepMaker};
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy, Variant};
use crate::ops::Table;
use crate::rep;
use crate::ops::modifytable::strategies::checks_ir::Checks;
use types_nodes::parsenodes::WCOKind;
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};

pub(crate) const ROWS: Table = Table {
    kinds: &[(KindId::WriteTransition, &StepMaker(make_plan)), (KindId::WriteTransition, &EndMaker(make_view_end))],
    ..crate::ops::NONE
};

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// What the trigger plan does not serve for `cmd` over `t`, by name. `view`: the target is a view, whose INSTEAD OF row
/// trigger is the write. `conflict_update`: ON CONFLICT DO UPDATE, whose arm fires the UPDATE kinds too. An EXPLAIN ANALYZE
/// lowering is no different (D3381): its per-trigger lines (`ExplainPrintTriggers`) are the statement level's counts of
/// the calls the host makes (trigger crate `instr`, opened by exec's entry, which delegates the shapes it cannot report).
pub(crate) fn refusal(t: TrigExt, cmd: Cmd, view: bool, conflict_update: bool) -> Option<&'static str> {
    // unknown facts (the source could not answer): the host binds the relation's own descriptor and fires what it holds;
    // nothing the lowering emits reads the facts (a road that would, write.checks, takes them as firing)
    if !t.known {
        return None;
    }
    if let Some(what) = t.refusal_on(cmd, view) {
        return Some(what);
    }
    if conflict_update {
        if let Some(what) = t.refusal_on(Cmd::Update, false) {
            return Some(what);
        }
    }
    None
}

/// The trigger plan of pipeline `k`'s write op `j`: every trigger-firing relation of the write (the target, and the
/// partitioned root a routed or pruned write goes through) is one the host fires; declared once per program.
pub(crate) fn make_plan<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Some(Op::Write(w)) = p.ops.get(j) else { crate::refuse!(node, Reason::Bug { payload: "write.transition's plan over an op that is not the write" }) };
    let view = w.members.has(Member::TargetView);
    let conflict_update = w.on_conflict.as_ref().is_some_and(|c| c.update);
    let root = w.route.map(|r| r.root_relid);
    for relid in core::iter::once(w.plan.target_rel).chain(root) {
        let t = gx.facts.trig_ext(relid);
        if let Some(why) = refusal(t, w.cmd, view && relid == w.plan.target_rel, conflict_update) {
            shape(node, why);
        }
    }
    if !px.protos.iter().any(|q| q.kind == ProtoKind::Transition) {
        px.declare_proto(Proto::new(ProtoKind::Transition, k as u16));
    }
    mcx::pg_box_dyn!(px.mcx(), Identity => dyn StepMech + 'g)
}

/// The trigger plan's step: the host fires the triggers inside the row op's call.
struct Identity;

impl StepMech for Identity {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        next(b, env, row)
    }
}

/// One view cell: a SET column's projection entry (resno) with its type's facts, or NULL.
enum Cell {
    Col(i16, TypeFacts),
    Null,
}

/// A view's UPDATE / DELETE end: its natts cells, then the wholerow OLD, through the INSTEAD OF row triggers (H-d).
struct ViewEnd<'g> {
    op: PgBox<'g, dyn RowOpMech + 'g>,
    cells: PgVec<'g, Cell>,
    node: i32,
    mcx: Mcx<'g>,
}

pub(crate) fn make_view_end<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Some((j, w)) = p.ops.iter().enumerate().find_map(|(j, o)| if let Op::Write(w) = o { Some((j, w)) } else { None }) else {
        crate::refuse!(node, Reason::Bug { payload: "a row-op end without its write op" })
    };
    let op = match w.cmd {
        Cmd::Update => WRITE_UPDATE,
        Cmd::Delete => WRITE_DELETE,
        _ => crate::refuse!(node, Reason::Bug { payload: "write.transition's view end over a write that is not an UPDATE / DELETE" }),
    };
    mcx::pg_box_dyn!(px.mcx(), view_end(px, k, j, w, op, false) => dyn EndMech + 'g)
}

/// The view's row op (its natts cells, then the wholerow) after every refusal by name; `ret`: under RETURNING (D667),
/// whose OLD of an UPDATE is the view's old row the host hands back (WritePlan.old).
fn view_end<'g>(px: &mut Px<'g, '_>, k: usize, j: usize, w: &WriteDesc<'_>, op: u32, ret: bool) -> ViewEnd<'g> {
    let gx = px.gx;
    let node = gx.pipelines()[k].node_id;
    let relid = admit_view(gx, k, w, ret);
    let cells = view_cells(gx, k, j, w, relid);
    drop(make_plan(px, k, j));
    let plan = w.plan.copy_in(px.arena);
    let plan = if ret && w.cmd == Cmd::Update && w.old_new.old { plan.with_old() } else { plan };
    let decl = RowOpDecl { op, relid, natts: cells.len() as u16, plan, loc: LocFrom::None, conflict: false };
    let op = super::rowop_step::make_op(px, k, decl);
    ViewEnd { op, cells, node, mcx: px.mcx() }
}

/// D667: a view's UPDATE / DELETE .. RETURNING (write/update.rs's step, write.returning's DELETE step): the INSTEAD OF row
/// op as a step, then write.returning's continuation over the row the host hands back (hosts write_view_rows.rs
/// `written`: UPDATE's final NEW, DELETE's OLD) and, under RETURNING OLD, the view's old row (`old_row`). A skipped row
/// (a trigger returned NULL) returns nothing, as ExecUpdate / ExecDelete.
pub(crate) fn view_step<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Some(Op::Write(w)) = p.ops.get(j) else { crate::refuse!(node, Reason::Bug { payload: "write.transition's view step over an op that is not the write" }) };
    let op = match w.cmd {
        Cmd::Update => WRITE_UPDATE,
        Cmd::Delete => WRITE_DELETE,
        _ => crate::refuse!(node, Reason::Bug { payload: "write.transition's view step over a write that is not an UPDATE / DELETE" }),
    };
    let view = view_end(px, k, j, w, op, true);
    let ret = super::returning::make_ret(px, k, j);
    mcx::pg_box_dyn!(px.mcx(), ViewStep { view, ret, natts: w.site.natts() as usize } => dyn StepMech + 'g)
}

/// D667: the view's row op, then RETURNING over the view's natts cells (the site's wholerow cell is the child's).
struct ViewStep<'g> {
    view: ViewEnd<'g>,
    ret: PgBox<'g, dyn RetMech + 'g>,
    natts: usize,
}

impl StepMech for ViewStep<'_> {
    fn prologue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _x: &Exits) {
        self.view.op.open(b, env);
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let cells = self.view.cells(b, env, row);
        let (op, ret, n) = (&*self.view.op, &*self.ret, self.natts);
        op.call(b, env, row, RowOpRow { cells: &cells[..], tid: None, rel: None }, &mut |b, env, r| {
            let written = op.cells(b, env, false);
            let old = op.old_cells(b, env);
            if written.len() < n {
                crate::refuse!(self.view.node, Reason::Bug { payload: "a view row op whose site has fewer cells than the view's" });
            }
            ret.lower_old(b, env, r, &written[..n], &old[..old.len().min(n)], &mut *next)
        }, None)
    }
}

fn refuse_kind(node: i32, k: KindId) -> ! {
    crate::not_built!(node, Variant::Kind(k))
}

/// The view write's refusals by name; the view's relid when it serves. A view has no heap, no ctid, no index and no
/// EvalPlanQual recheck (ExecUpdate / ExecDelete call the INSTEAD OF triggers before any heap access), so the row marks
/// PG gives the view query's relations are never read.
fn admit_view(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>, ret: bool) -> u32 {
    let node = gx.pipelines()[k].node_id;
    for m in w.members.iter() {
        match m {
            Member::Update | Member::Delete | Member::TargetView | Member::WriteMarks => {}
            // D667: RETURNING (its OLD / NEW too) is served as the view step's continuation (`view_step`)
            Member::Returning | Member::ReturningOld | Member::ReturningNew if ret => {}
            Member::WriteSubPlan => refuse_kind(node, KindId::NestSubplan),
            // ret is false only from make_view_end, reached without RETURNING; MERGE into a view is write/merge_view.rs's
            Member::Returning | Member::ReturningOld | Member::ReturningNew | Member::MergeActionFn => crate::refuse!(node, Reason::Bug { payload: "a view write's RETURNING member without the RETURNING step" }),
            Member::CheckView if w.cmd == Cmd::Update => {}
            // a view carries no RLS check, and view checks are an INSERT's / UPDATE's (rewriteTargetView)
            Member::CheckView | Member::CheckRls => crate::refuse!(node, Reason::Bug { payload: "a view DELETE or RLS check" }),
            _ => crate::refuse!(node, Reason::Bug { payload: "a view UPDATE / DELETE carrying a member a view target never has" }),
        }
    }
    if !ret && (w.returning.is_some() || w.old_new.old || w.old_new.new) {
        crate::refuse!(node, Reason::Bug { payload: "a view write's RETURNING without the RETURNING step" });
    }
    // V4R-330: only the view checks of an UPDATE (ExecUpdateEpilogue's WCO_VIEW_CHECK); write.checks refuses by name what
    // the host cannot check (a SubPlan in the qual: nest.subplan)
    if !w.checks.is_empty() {
        if w.cmd != Cmd::Update || w.checks.iter().any(|c| c.kind != WCOKind::WCO_VIEW_CHECK) {
            crate::refuse!(node, Reason::Bug { payload: "a view DELETE or RLS check" });
        }
        if crate::frame::iface::need_strategy::<Checks>(&node, WRITE_CHECKS).plan(gx, node, w) != ChecksAt::Host {
            crate::refuse!(node, Reason::Bug { payload: "an IR checks plan over a view target" });
        }
    }
    if w.targets.len() != 1 || w.targets[0].kind != TargetKind::View || w.targets[0].wholerow_col <= 0 {
        crate::refuse!(node, Reason::Bug { payload: "a view write without its one view target and wholerow column" });
    }
    // the write ends the pipeline; under RETURNING its projection follows it (write.returning's `write_of` checks it)
    if !ret && gx.pipelines()[k].ops.iter().skip_while(|o| !matches!(o, Op::Write(_))).nth(1).is_some() {
        crate::refuse!(node, Reason::Bug { payload: "a view write with an op after it and no RETURNING" });
    }
    w.plan.target_rel
}

/// The view's cells in attno order, then the wholerow: UPDATE's SET entries are the projection's resnos 1..n in `set`
/// order (PG's updateColnos), every other column NULL (the host forms NEW from OLD); DELETE carries NULLs.
fn view_cells<'g>(gx: &Gx<'g, '_>, k: usize, j: usize, w: &WriteDesc<'_>, relid: u32) -> PgVec<'g, Cell> {
    let node = gx.pipelines()[k].node_id;
    let natts = w.site.natts() as i16;
    let set: PgVec<'g, i16> = w.set.map_or_else(|| PgVec::new_in(gx.mcx), |s| s.iter().map(|a| a as i16).collect_in(gx.mcx));
    let mut out = mcx::reserved_in(natts as usize + 1, gx.mcx);
    for a in 1..=natts {
        let Some(i) = set.iter().position(|&s| s == a) else {
            out.push_in(Cell::Null);
            continue;
        };
        let Some(tf) = gx.facts.att(relid, a).and_then(|att| typefacts::facts_of(gx.facts, att.typid)) else {
            shape(node, "a view UPDATE: a SET column type the catalog does not know")
        };
        out.push_in(Cell::Col(i as i16 + 1, tf));
    }
    let wr = w.targets[0].wholerow_col;
    let Some(tf) = projected_type(gx, k, j, wr).and_then(|t| typefacts::facts_of(gx.facts, t)) else {
        shape(node, "a view UPDATE / DELETE: the wholerow column's row type the catalog does not know")
    };
    out.push_in(Cell::Col(wr, tf));
    out
}

/// The type of resno `resno` of the projection the write op `j` reads (the last one before it).
pub(crate) fn projected_type(gx: &Gx<'_, '_>, k: usize, j: usize, resno: i16) -> Option<u32> {
    let list = gx.pipelines()[k].ops[..j].iter().rev().find_map(|o| if let Op::Project(l) = o { Some(*l) } else { None })?;
    let te = list.iter().filter_map(|n| n.as_target_entry()).find(|t| t.resno == resno)?;
    gravity_decompose::catalog::expr_type::type_of(te.expr) // exprType: a ROW(..) answers its row_typeid
}

impl EndMech for ViewEnd<'_> {
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        self.op.open(b, env);
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        // the row reached the write (the child's EXPLAIN ANALYZE rows) whatever its status, as write.update's end
        let out = b.int_bin(IntBinOp::Add, row.carry.out, env.one);
        let at = RowVals { row: row.row.clone(), vals: row.vals.clone(), cols: row.cols.clone(), carry: Carry { out, accs: mcx::to_vec_in(&row.carry.accs[..], env.mcx) }, next: row.next, at: row.at };
        let cells = self.cells(b, env, &at);
        self.op.call(b, env, &at, RowOpRow { cells: &cells[..], tid: None, rel: None }, &mut |_, env, r| r.carry.copy_in(env.mcx), None)
    }

    fn sink(&self) -> SinkKind {
        SinkKind::RowOp
    }
}

impl<'g> ViewEnd<'g> {
    fn cells(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) -> PgVec<'g, (Value, Value)> {
        let (zero, t, f) = (env.zero, b.bconst(true), b.bconst(false));
        let mut cx = env.lx(b, self.node);
        let mut v = mcx::reserved_in(self.cells.len(), self.mcx);
        for c in self.cells.iter() {
            match c {
                Cell::Null => v.push_in((zero, f)),
                Cell::Col(resno, tf) => {
                    let Some(cr) = row.col(*resno) else { crate::refuse!(self.node, Reason::Bug { payload: "a view cell past the child's projection" }) };
                    let (d, null) = rep::datum(&mut cx, cr, tf);
                    let valid = cx.b.int_bin(IntBinOp::Xor, null, t);
                    v.push_in((d, valid));
                }
            }
        }
        env.memo = cx.memo;
        v
    }
}

#[cfg(test)]
mod tests {
    use super::refusal;
    use gravity_decompose::ops::modifytable::trig_facts::{TrigExt, TRIG_DELETE, TRIG_INSERT, TRIG_UPDATE};
    use gravity_program::Cmd;

    /// Row, statement, transition, RI and deferrable constraint triggers and unknown facts (the host binds the real set) are
    /// served, and a MERGE target's BEFORE ROW triggers (V4R-313: the actions' prologue); an INSTEAD OF row trigger on a
    /// table refuses by name; a view's INSTEAD OF trigger is the write.
    #[test]
    fn the_plan_serves_what_the_host_fires() {
        let row = TrigExt { before_row: TRIG_INSERT, after_row: TRIG_INSERT | TRIG_UPDATE, statement: TRIG_INSERT, transition: TRIG_INSERT, ..TrigExt::NONE };
        assert_eq!(refusal(row, Cmd::Insert, false, false), None);
        let ri = TrigExt { after_row: TRIG_INSERT | TRIG_UPDATE, internal: TRIG_INSERT | TRIG_UPDATE, deferrable: TRIG_INSERT, ..TrigExt::NONE };
        assert_eq!(refusal(ri, Cmd::Insert, false, false), None);
        assert_eq!(refusal(TrigExt::UNKNOWN, Cmd::Delete, false, false), None);
        let instead = TrigExt { instead_row: TRIG_UPDATE | TRIG_DELETE, ..TrigExt::NONE };
        assert!(refusal(instead, Cmd::Update, false, false).is_some());
        assert_eq!(refusal(instead, Cmd::Update, true, false), None);
        assert_eq!(refusal(instead, Cmd::Delete, true, false), None);
        let before_upd = TrigExt { before_row: TRIG_UPDATE, ..TrigExt::NONE };
        assert_eq!(refusal(before_upd, Cmd::Merge, false, false), None);
    }

    /// EXPLAIN ANALYZE is no refusal of its own (D3381): the statement level counts the host's calls.
    #[test]
    fn explain_analyze_is_no_refusal() {
        let upd = TrigExt { after_row: TRIG_UPDATE, ..TrigExt::NONE };
        assert_eq!(refusal(upd, Cmd::Insert, false, true), None);
        assert_eq!(refusal(upd, Cmd::Update, false, false), None);
        assert_eq!(refusal(TrigExt::UNKNOWN, Cmd::Delete, false, false), None);
    }
}
