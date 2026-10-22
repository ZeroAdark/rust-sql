//! Kind `write.onconflict` (INSERT ... ON CONFLICT; plan §3.5, cards/M4.md L5 `write/onconflict.rs`). Owner M4-LOCK.
//!
//! The host does PostgreSQL's ExecInsert ON CONFLICT arm (hosts/src/write.rs `insert_on_conflict`): the arbiters'
//! pre-check (`ExecCheckIndexConstraints`), the speculative insertion under its token with the arbiters' index inserts in
//! deferred-check mode, the tuple confirmed or killed, and a concurrent inserter winning the race restarting from the
//! pre-check. The lowerer's part is the frozen write plan: `WritePlan::with_conflict(action, arbiters)`, which the driver
//! binds the write host from (the arbiter OIDs are in the program key, lower/src/key.rs, so two conflict targets never
//! share a program).
//!
//! L3a, DO NOTHING: a row that conflicts is `WRITE_SKIPPED` (the row op's carry as it came: not counted, no RETURNING
//! row), as PG. An empty arbiter list (no conflict target) is PG's: every unique / exclusion index arbitrates.
//!
//! L3b, DO UPDATE (ExecOnConflictUpdate): on a conflict the host locks the existing row in the executor's mode (a
//! concurrent update or delete under READ COMMITTED restarts from the pre-check, a row this command already wrote raises
//! 21000, REPEATABLE READ raises 40001) and puts its cells and position in the conflict site, the write site's successor
//! (driver dml.rs); the row op's `conflict` arm runs `ConflictArm::lower` over it. The arm's row is the existing row by
//! attno (the target's Vars, `Source::Scan`) with the proposed row behind it as the inner map (EXCLUDED, INNER_VAR after
//! setrefs). The WHERE (`onConflictWhere`) false or NULL leaves the row as it came (not counted, PG's filtered row); the
//! new row is the existing one with each SET entry (`onConflictSet`) at its `onConflictCols` attno
//! (ExecBuildUpdateProjection); it goes into the conflict site and ONE host call updates it in place (`write_row`,
//! WRITE_UPDATE at the conflict position), which runs ExecUpdate's constraints, stored generated columns and index
//! maintenance. A target with UPDATE USING policies has the host check the existing row against them after the WHERE
//! passed and before the SET (`WRITE_CONFLICT_CHECK`, WCO_RLS_CONFLICT_CHECK, H-b), ExecOnConflictUpdate's order. Done continues (counted by the host, the
//! command tag's), Skipped leaves the row, an error stops at it. The expressions compile through expr.core's face (V4R-240).
//! write.insert calls `plan` and `arm` directly; the arm takes the row op write.insert passes it.
//!
//! D661-a: EXCLUDED is the row ExecInsert formed, the write site as the host leaves it on Conflict (V4R-243: after the
//! BEFORE ROW INSERT triggers and ExecComputeStoredGenerated); a target firing INSERT or UPDATE triggers makes the
//! trigger plan (write.transition, which refuses by name what the host does not fire under DO UPDATE); a whole row of
//! the target or of EXCLUDED is expr.wholerow's ROW over the attributes. D661-b, a routed target (its partitioned root,
//! write.route admitting the leaves): the arm runs in the root's layout over the conflicting row's leaf (the route host
//! maps the rows leaf <-> root), EXCLUDED always read back; a new row outside the leaf is the leaf host's PG 0A000
//! "invalid ON UPDATE specification". Refused by name: a system column, a Var of another relation.
use gravity_abi::ctx::site_off;
use gravity_abi::program::{WRITE_CONFLICT_CHECK, WRITE_UPDATE};
use gravity_abi::site::site;
use gravity_abi::status::{unsupported, UNSUPPORTED_SHAPE, WRITE_DONE, WRITE_SKIPPED};
use gravity_decompose::ops::modifytable::trig_facts::{TRIG_INSERT, TRIG_UPDATE};
use gravity_decompose::catalog::typefacts::{self, TypeFacts};
use gravity_decompose::ops::shared::modifytable::{TargetKind, WriteDesc};
use gravity_decompose::Op;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_program::{ArenaRef, Cmd, Conflict, ExprLife, ExprOut, Rep, WritePlan};
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};
use types_nodes::parsenodes::WCOKind;
use types_nodes::primnodes::{SubLinkType, INNER_VAR};
use types_nodes::{Node, NodeTag};

use crate::cell::{Cell, Nullable, Row};
use crate::face::end::RowOpMech;
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::px::{ExId, Px};
use crate::frame::row::{Carry, RowVals};
use crate::graph::Gx;
use crate::lx::CellRef;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::face::step::ExprFace;
use crate::ops::Table;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::WriteOnconflict, &())], ..crate::ops::NONE };

/// DO UPDATE over the conflict site: `row` the INSERT's row (its carry), `proposed` its cells `(datum, valid)` in attno
/// order, `op` the row op that reported the conflict; `done` continues after the update with `row`.
pub(crate) trait ConflictArm {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, proposed: &[(Value, Value)], op: &dyn RowOpMech, done: Next<'_, 'e>) -> Carry<'e>;
}

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

pub(crate) fn plan(gx: &Gx<'_, '_>, node: i32, w: &WriteDesc<'_>, plan: WritePlan, arena: ArenaRef) -> WritePlan {
    let Some(oc) = &w.on_conflict else { bug(node, "write.onconflict over a write without ON CONFLICT") };
    if !oc.update && (!oc.set.is_nil() || oc.filter.is_some()) {
        bug(node, "ON CONFLICT DO NOTHING with a SET list or a WHERE");
    }
    let arbiters: PgVec<'_, u32> = oc.arbiter.iter().collect_in(gx.mcx);
    plan.with_conflict(arena, if oc.update { Conflict::Update } else { Conflict::Nothing }, &arbiters[..])
}

/// The target's attribute `a` as the arm's cells carry it (a Datum in the site's form), `None` when dropped.
struct Att {
    out: ExprOut,
    tf: TypeFacts,
}

struct Arm<'g> {
    mcx: Mcx<'g>,
    atts: PgVec<'g, Option<Att>>,
    /// The WHERE's quals, ANDed in plan order (a false or NULL one stops).
    filter: PgVec<'g, ExId>,
    /// Each SET entry: its attno's index, its compiled expression, the form `emit` gives it in.
    set: PgVec<'g, (usize, ExId, ExprOut)>,
    /// D1531 x D1112 (writesubplan): each correlated MULTIEXPR SubPlan of the SET list (a junk entry), in list order: read
    /// before the SET entries, which read its output Params (ExecBuildUpdateProjection's setup steps); its value is dropped.
    multi: PgVec<'g, ExId>,
    node: i32,
    /// expr.core's face (V4R-240).
    ex: &'static ExprFace,
    /// EXCLUDED is read back from the write site (the host's row), else the IR's proposed cells are it.
    host_row: bool,
    /// The target has UPDATE USING policies over the existing row (WCO_RLS_CONFLICT_CHECK): the host checks it after the
    /// WHERE passed, before the SET (`WRITE_CONFLICT_CHECK`).
    rls: bool,
}

pub(crate) fn arm<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn ConflictArm + 'g> {
    let gx = px.gx;
    let mcx = px.mcx();
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Some(Op::Write(w)) = p.ops.get(j) else { bug(node, "write.onconflict's arm over an op that is not the write") };
    let Some(oc) = &w.on_conflict else { bug(node, "write.onconflict's arm over a write without ON CONFLICT") };
    if !oc.update {
        bug(node, "write.onconflict's arm over DO NOTHING");
    }
    let relid = w.plan.target_rel;
    // D661-b: a routed target is its partitioned root (the route host runs the arm's UPDATE on the conflicting row's leaf)
    let routed = w.route.is_some() && w.targets.len() == 1 && w.targets[0].kind == TargetKind::Partitioned;
    if !routed && (w.route.is_some() || w.targets.len() != 1 || w.targets[0].kind != TargetKind::Heap) {
        shape(node, "ON CONFLICT DO UPDATE over a target that is neither a table nor a routed partitioned root");
    }
    let t = gx.facts.trig_ext(relid);
    // EXCLUDED differs from the IR's proposed cells only when the host rewrites the row before the arbiter check: a
    // BEFORE ROW INSERT trigger or a stored generated column (or facts the source could not answer); a routed row's
    // leaf may have its own BEFORE ROW INSERT trigger (the route host hands back its row in the root's layout)
    let host_row = routed || !t.known || t.fires_before(Cmd::Insert) || w.targets[0].generated.iter().any(|g| g.1 != b'v');
    if !t.known || (t.before_row | t.after_row | t.instead_row | t.statement | t.transition) & (TRIG_INSERT | TRIG_UPDATE) != 0 {
        // the arm's UPDATE fires the target's UPDATE triggers in the host (ExecOnConflictUpdate -> ExecUpdate): the trigger
        // plan refuses by name what the host does not fire under conflict_update (write.insert makes it for INSERT's)
        drop(super::triggers::make_plan(px, k, j));
    }
    let natts = w.site.natts();
    let atts: PgVec<'g, Option<Att>> = (1..=natts as i16)
        .map(|a| {
            let f = gx.facts.att(relid, a)?;
            let Some(tf) = typefacts::facts_of(gx.facts, f.typid) else { shape(node, "an ON CONFLICT DO UPDATE column type the catalog does not know") };
            Some(Att { out: ExprOut { rep: Rep::of_storage(f.typlen, f.byval), typid: f.typid, life: ExprLife::Row }, tf })
        })
        .collect_in(mcx);
    let check = |x: Node<'_>| {
        let mut why = None;
        let known = gravity_decompose::expr::walk(x, &mut |n| {
            why = why.or_else(|| reads(n, w.rti as i32, natts));
            why.is_none()
        });
        if let Some(why) = why {
            shape(node, why);
        }
        if !known {
            shape(node, "ON CONFLICT DO UPDATE: an expression the walker does not open");
        }
    };
    // onConflictWhere is an implicit-AND list after the planner (make_ands_implicit): each qual in order, ExecQual's
    let ex: &'static ExprFace = crate::frame::iface::expr_face(&node);
    let quals: PgVec<'_, Node<'_>> = oc.filter.map_or_else(|| PgVec::new_in(mcx), |f| f.as_list().map_or_else(|| mcx::pg_vec![in mcx; f], |l| l.iter().collect_in(mcx)));
    let filter: PgVec<'g, ExId> = quals.into_iter().map(|q| { check(q); ex.0.compile(px, q, None, node) }).collect_in(mcx);
    let cols: PgVec<'_, i32> = oc.cols.iter().collect_in(mcx);
    let mut set: PgVec<'g, (usize, ExId, ExprOut)> = mcx::reserved_in(cols.len(), mcx);
    let mut multi: PgVec<'g, ExId> = PgVec::new_in(mcx);
    for n in oc.set.iter() {
        let Some(te) = n.as_target_entry() else { bug(node, "an ON CONFLICT SET entry that is not a TargetEntry") };
        // D1531: a multiple-column SET `(b, a) = (SELECT ..)` keeps its MULTIEXPR sublink as a junk entry, which PG's
        // extract_update_targetlist_colnos leaves out of the columns. Correlated, it is a SubPlan that
        // ExecBuildUpdateProjection runs first (ExecPushExprSetupSteps: every MULTIEXPR SubPlan of the list, after the
        // inputs' deform and before any entry), its output Params feeding the entries after it: lent with no row
        // (subplan/per_row.rs `conflict`), read here over the arm's row ahead of the SET entries. Uncorrelated, it is an
        // initplan and the entry a NULL record constant (subselect.c build_subplan), a no-op: the entries after it read
        // the initplan's Params.
        if te.resjunk {
            match te.expr.node_tag() {
                NodeTag::T_SubPlan if te.expr.as_sub_plan().is_some_and(|s| s.subLinkType == SubLinkType::MULTIEXPR_SUBLINK) => {
                    check(te.expr);
                    multi.push_in(ex.0.compile(px, te.expr, None, node));
                    continue;
                }
                NodeTag::T_SubPlan => shape(node, "a SubPlan in a write's own expressions, a check, RETURNING, ON CONFLICT or a MERGE action (D1112: nest.subplan B4)"),
                NodeTag::T_Const if te.expr.as_const().is_some_and(|c| c.constisnull && gravity_fn::typesem::is_anon_record(c.consttype)) => continue,
                _ => bug(node, "a junk ON CONFLICT SET entry that is no MULTIEXPR sublink"),
            }
        }
        let Some(&a) = cols.get(set.len()) else { bug(node, "an ON CONFLICT SET list longer than its columns") };
        let Some(Some(att)) = usize::try_from(a).ok().and_then(|a| a.checked_sub(1)).and_then(|a| atts.get(a)) else {
            bug(node, "an ON CONFLICT SET column that is not a live attribute")
        };
        check(te.expr);
        let out = gx.out_of(te.expr).unwrap_or(att.out);
        set.push_in(((a - 1) as usize, ex.0.compile(px, te.expr, None, node), out));
    }
    if set.len() != cols.len() {
        bug(node, "an ON CONFLICT SET list shorter than its columns");
    }
    let rls = w.checks.iter().any(|c| matches!(c.kind, WCOKind::WCO_RLS_CONFLICT_CHECK));
    mcx::pg_box_dyn!(mcx, Arm { mcx, atts, filter, set, multi, node, ex, host_row, rls } => dyn ConflictArm + 'g)
}

/// What a WHERE / SET node reads that the arm's row does not carry: the target's attributes (`rti`) and EXCLUDED's
/// (INNER_VAR), each whole row included (expr.wholerow's ROW over the attributes), are all it has.
fn reads(x: Node<'_>, rti: i32, natts: usize) -> Option<&'static str> {
    if x.node_tag() != NodeTag::T_Var {
        return None;
    }
    let v = x.as_var()?;
    if v.varlevelsup != 0 || (v.varno != rti && v.varno != INNER_VAR) {
        Some("ON CONFLICT DO UPDATE: a column of another relation")
    } else if v.varattno < 0 || v.varattno as usize > natts {
        Some("ON CONFLICT DO UPDATE: a system column")
    } else {
        None
    }
}

impl<'g> ConflictArm for Arm<'g> {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, proposed: &[(Value, Value)], op: &dyn RowOpMech, done: Next<'_, 'e>) -> Carry<'e> {
        if proposed.len() != self.atts.len() {
            bug(self.node, "an ON CONFLICT proposed row that is not the target's width");
        }
        // the existing row's cells (V4R-501: not the sys cells `cells` appends for RETURNING, read after the update)
        let existing_all = op.cells(b, env, true);
        let existing = &existing_all[..self.atts.len()];
        let t = b.bconst(true);
        // EXCLUDED is the row ExecInsert formed: the write site as the host left it on Conflict (V4R-243), after the
        // BEFORE ROW INSERT triggers and ExecComputeStoredGenerated; without either it is the IR's proposed row
        let host;
        let formed;
        let excluded: &[(Value, Value)] = if self.host_row {
            host = op.cells(b, env, false);
            &host[..]
        } else {
            formed = proposed.iter().map(|&(d, valid)| (d, b.int_bin(IntBinOp::Xor, valid, t))).collect_in(self.mcx);
            &formed[..]
        };
        // the arm's row: the existing row by attno, then the proposed row (EXCLUDED) as the inner map
        let n = self.atts.len();
        let mcx = env.mcx;
        let (mut cells, mut vals, mut cols) = (mcx::reserved_in(2 * n, mcx), mcx::reserved_in(2 * n, mcx), mcx::reserved_in(n, mcx));
        let mut inner = mcx::reserved_in(n, mcx);
        for (att, &(d, isnull)) in self.atts.iter().zip(existing.iter()) {
            cols.push_in(att.as_ref().map(|a| push(&mut cells, &mut vals, a.out, d, isnull)));
        }
        for (att, &(d, isnull)) in self.atts.iter().zip(excluded.iter()) {
            inner.push_in(att.as_ref().map(|a| push(&mut cells, &mut vals, a.out, d, isnull)));
        }
        let r = RowVals { row: Row { cells }, vals, cols, carry: row.carry.copy_in(env.mcx), next: row.next, at: row.at };
        env.memo.clear();
        let outer_inner = env.inner.replace(inner);
        let types: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(self.mcx);
        let l_join = b.create_block(&types[..]);
        for &f in self.filter.iter() {
            let (pass, l_upd) = (self.ex.0.pred(b, env, self.node, &r, f), b.create_block(&[]));
            if crate::frame::instr::on() && op.write_node() >= 0 {
                // EXPLAIN ANALYZE's Rows Removed by Conflict Filter (ExecOnConflictUpdate's InstrCountFiltered1)
                let l_out = b.create_block(&[]);
                b.cond_br(pass, (l_upd, &[]), (l_out, &[]));
                b.switch_to(l_out);
                crate::frame::instr::count_one(b, env.ctx, op.write_node(), crate::frame::instr::Ctr::Filtered1);
                b.br(l_join, &row.carry.params());
            } else {
                b.cond_br(pass, (l_upd, &[]), (l_join, &row.carry.params()));
            }
            b.switch_to(l_upd);
        }
        if self.rls {
            // ExecOnConflictUpdate: the UPDATE USING policies over the existing row, after the WHERE and before the SET
            let st = host_op(b, env, op.site() + 1, WRITE_CONFLICT_CHECK);
            let (l_ok, l_err) = (b.create_block(&[]), b.create_block(&[]));
            b.mark_cold(l_err);
            let neg = b.icmp(IntCond::Slt, st, env.zero);
            b.cond_br(neg, (l_err, &[]), (l_ok, &[]));
            b.switch_to(l_err);
            b.br(env.x.l_stop_in, &[row.carry.out, row.at, st]);
            b.switch_to(l_ok);
        }
        // D1531 x D1112: each MULTIEXPR SubPlan, its setParams written for the entries after it (the value is PG's dummy NULL)
        for &m in self.multi.iter() {
            let _ = self.ex.0.emit(b, env, self.node, &r, m);
        }
        // the new row: the existing one, each SET entry at its attno, as Datums into the conflict site; a dropped column
        // NULL, as ExecBuildUpdateProjection stores it (D2763: the existing row's dropped value stayed in the new version)
        let mut new: PgVec<'_, (Value, Value)> = existing.iter().zip(self.atts.iter()).map(|(&c, a)| if a.is_some() { c } else { (env.zero, t) }).collect_in(self.mcx);
        for &(a, ex, out) in self.set.iter() {
            let (v, valid) = self.ex.0.emit(b, env, self.node, &r, ex);
            let Some(att) = &self.atts[a] else { bug(self.node, "an ON CONFLICT SET column that is not a live attribute") };
            let isnull = b.int_bin(IntBinOp::Xor, valid, t);
            let cell = Cell::of_expr(out, 0, 0, Nullable::Maybe);
            let mut cx = env.lx(b, self.node);
            new[a] = crate::rep::datum(&mut cx, CellRef { cell: &cell, val: v, null: Some(isnull) }, &att.tf);
            env.memo = cx.memo;
        }
        env.inner = outer_inner;
        env.memo.clear();
        let at = site_off(op.site() + 1) as i32;
        let (vp, np) = (b.load(Type::Ptr, env.ctx, at + site::VALUES as i32), b.load(Type::Ptr, env.ctx, at + site::ISNULL as i32));
        let (z8, o8) = (b.iconst(Type::I8, 0), b.iconst(Type::I8, 1));
        for (a, &(d, isnull)) in new.iter().enumerate() {
            let v = b.select(isnull, env.zero, d);
            b.store(vp, a as i32 * 8, v);
            let nb = b.select(isnull, o8, z8);
            b.store(np, a as i32, nb);
        }
        let st = host_op(b, env, op.site() + 1, WRITE_UPDATE);
        // Done continues; Skipped leaves the row as it came; an error stops at the row; any other status is unsupported
        let (dn, sk) = (b.iconst(Type::I64, WRITE_DONE), b.iconst(Type::I64, WRITE_SKIPPED));
        let (l_done, l_not, l_rest, l_cold) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64]));
        b.mark_cold(l_cold);
        let is_done = b.icmp(IntCond::Eq, st, dn);
        b.cond_br(is_done, (l_done, &[]), (l_not, &[]));
        b.switch_to(l_not);
        let is_skip = b.icmp(IntCond::Eq, st, sk);
        b.cond_br(is_skip, (l_join, &row.carry.params()), (l_rest, &[]));
        b.switch_to(l_rest);
        let neg = b.icmp(IntCond::Slt, st, env.zero);
        let uns = b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE));
        let err = b.select(neg, st, uns);
        b.br(l_cold, &[err]);
        b.switch_to(l_cold);
        let e = b.block_params(l_cold)[0];
        b.br(env.x.l_stop_in, &[row.carry.out, row.at, e]);
        b.switch_to(l_done);
        let c = done(b, env, row);
        b.br(l_join, &c.params());
        b.switch_to(l_join);
        Carry::from_params(b.block_params(l_join), env.mcx)
    }
}

/// A cell of the arm's row in its Datum form: its index.
fn push(cells: &mut PgVec<'_, Cell>, vals: &mut PgVec<'_, (Value, Option<Value>)>, out: ExprOut, d: Value, isnull: Value) -> u16 {
    let at = cells.len() as u16;
    cells.push_in(Cell::of_expr(out, at, at, Nullable::Maybe));
    vals.push_in((d, Some(isnull)));
    at
}

/// A host op on the conflict site's row: `write_row(ctx, site, op)` (write/rowop.rs `host_op`'s call), WRITE_UPDATE updating
/// it in place.
fn host_op(b: &mut FunctionBuilder<'_>, env: &Env, at: usize, op: u32) -> Value {
    let mem = HostFlags { touches_memory: true, ..HostFlags::default() };
    let write_row = gravity_kernels::ops::modifytable::entries::write_row as *const () as usize;
    let h = b.declare_host_once("write_row", &[Type::Ptr, Type::I64, Type::I64], Some(Type::I64), write_row, mem);
    let (site_c, op_c) = (b.iconst(Type::I64, at as i64), b.iconst(Type::I64, op as i64));
    b.call_host(h, &[env.ctx, site_c, op_c], Some(Type::I64)).unwrap_or(env.zero)
}
