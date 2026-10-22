//! M8-WRITE W6: write.merge's NOT MATCHED [BY TARGET] actions (PostgreSQL 18's ExecMergeNotMatched). The planner joins
//! source LEFT JOIN target; the target is a producer on the join's nullable side (hashed on the build side or sorted as
//! a merge join's inner, merge_admit.rs), so a row whose target ctid is NULL is a source row no target row joined. Per
//! such row each NOT MATCHED action's quals run in order over the source row; the first that holds runs: DO NOTHING
//! leaves the row, INSERT forms the new tuple from its value list (the rewriter put every default in it; a column it
//! does not name is NULL) for ONE `WRITE_INSERT` on the MERGE's write site with no position (write/rowop_step.rs). Under
//! RETURNING the inserted row goes on through write.returning with `merge_action()` 'INSERT' and a NULL OLD.
//! Concurrency: a MATCHED row whose target row a concurrent transaction deleted takes the host's `WRITE_NOT_MATCHED`
//! (write/rowop_step.rs, `Arm::Merge`) and runs these actions over its source row, as PostgreSQL 18's ExecMergeMatched
//! falls through on TM_Deleted; a target row that moved or no longer joins reruns the join (the EvalPlanQual recheck).
//! Under RETURNING the fall-through's INSERT returns its row too (mergecmd 14a: merge/lower.rs shares the continuation).
//! V4R-495: NOT MATCHED BY SOURCE. The planner joins target LEFT JOIN source (the target its own scan or sorted, the
//! source on the nullable side); per joined row the join condition (PG 18's ri_MergeJoinCondition, `cond`) decides, as
//! ExecMergeMatched does: true runs the WHEN MATCHED actions, else (`split`) the BY SOURCE ones, UPDATE / DELETE / DO
//! NOTHING over the target row like MATCHED's (write/merge.rs). A null-extended source's recheck position is the empty
//! row (marks.rs `store`). Helper file of write.merge: no rows of its own.
use gravity_abi::program::WRITE_INSERT;
use gravity_decompose::catalog::typefacts::{self, TypeFacts};
use gravity_decompose::ops::shared::modifytable::{MergeDesc, WriteDesc};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::{Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntBinOp, Type, Value};
use gravity_program::{ArenaRef, Cmd, ExprLife, ExprOut, Rep, WritePlan};
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::jointype::JoinType;
use types_nodes::plannodes::RowMarkType;
use types_nodes::primnodes::MergeMatchKind;
use types_nodes::Node;

use crate::cell::{Cell, Nullable};
use crate::face::end::{Arm, RowOpMech, RowOpRow};
use crate::face::step::ExprFace;
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::px::{ExId, Px};
use crate::frame::row::{Carry, RowVals};
use crate::graph::Gx;
use crate::lx::CellRef;
use crate::admit::refusal::Reason;
use crate::face::strategy::WRITE_ROUTE;
use crate::rep;

use crate::ops::modifytable::face::RetMech;
use crate::ops::modifytable::merge::admit::shape;

/// One NOT MATCHED action: its quals, ANDed in order, and its INSERT's values (attno index, expression, form, the
/// column type's facts), `None` for DO NOTHING.
pub(crate) struct Nm<'g> {
    quals: PgVec<'g, ExId>,
    insert: Option<PgVec<'g, (usize, ExId, ExprOut, TypeFacts)>>,
    /// The inserted row's attribute count: the target's, or a routed MERGE's root's (V4R-498).
    natts: usize,
    /// INHERIT-ROOT: RETURNING's attnos into the written row: per attribute of the first result relation (the
    /// list's Vars), its column's root attno (0: none) under a routed MERGE, whose written row is the root's; empty: the
    /// written row's own.
    ret_at: PgVec<'g, i16>,
    /// writesubplan (gen mg-0178): the written row's system columns after the site's cells (merge/lower.rs `sys`).
    sys: usize,
}

/// The MERGE's NOT MATCHED [BY TARGET] actions inserting into `relid` (the target, or a routed MERGE's root: `routed`) of
/// `natts` attributes, their expressions checked by `check`. `hashed`: the target a producer on the join's nullable side
/// (merge_admit.rs), the only form a NOT MATCHED action runs over.
/// `ret_at`: RETURNING's attnos into the written row (`Nm::ret_at`); `sys`: its system-column cells (`Nm::sys`).
pub(crate) fn actions<'g, 'p>(px: &mut Px<'g, 'p>, m: &MergeDesc<'p>, ((relid, natts), node, hashed): ((u32, usize), i32, bool), ex: &'static ExprFace, check: &dyn Fn(Node<'_>), (ret_at, sys): (&[i16], usize)) -> PgVec<'g, Nm<'g>> {
    let gx = px.gx;
    let mcx = px.mcx();
    let mut out = PgVec::new_in(mcx);
    // DO NOTHING only: PG 18.6 joins INNER (transform_MERGE_to_join counts no CMD_NOTHING action), so no source row
    // arrives unmatched; with quiet quals (none, D3470: or ones that raise and call nothing) a concurrent fall-through
    // leaves no trace either: no arm (`idle`)
    let nms = || m.actions.iter().filter(|a| a.kind == MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_TARGET);
    let nothing = nms().all(|a| a.cmd.is_none());
    if idle(gx, m) {
        return out;
    }
    for a in nms() {
        let mut quals = PgVec::new_in(mcx);
        for q in a.quals.iter().flat_map(|l| l.iter()) {
            check(q);
            quals.push_in(ex.0.compile(px, q, None, node));
        }
        let insert = match a.cmd {
            None => None,
            Some(Cmd::Insert) => {
                let mut vals = mcx::reserved_in(a.tlist.len(), mcx);
                for e in a.tlist.iter() {
                    let Some(te) = e.as_target_entry() else { crate::refuse!(node, Reason::Bug { payload: "a MERGE INSERT entry that is not a TargetEntry" }) };
                    // a dropped column's entry is the NULL int4 Const expand_insert_targetlist puts there: its cell stays
                    // NULL (`image`), as PostgreSQL writes it
                    if gx.facts.att(relid, te.resno).is_none() && gx.facts.dropped_layout(relid, te.resno).is_some() && te.expr.as_const().is_some_and(|c| c.constisnull) {
                        continue;
                    }
                    let Some((i, f)) = usize::try_from(te.resno - 1).ok().and_then(|i| gx.facts.att(relid, te.resno).map(|f| (i, f))) else { shape(node, "MERGE: an INSERT column the target does not have") };
                    let Some(tf) = typefacts::facts_of(gx.facts, f.typid) else { shape(node, "MERGE: a target column type the catalog does not know") };
                    check(te.expr);
                    let out = gx.out_of(te.expr).unwrap_or(ExprOut { rep: Rep::of_storage(f.typlen, f.byval), typid: f.typid, life: ExprLife::Row });
                    vals.push_in((i, ex.0.compile(px, te.expr, None, node), out, tf));
                }
                Some(vals)
            }
            Some(_) => crate::refuse!(node, Reason::Bug { payload: "a WHEN NOT MATCHED action that is not INSERT / DO NOTHING" }),
        };
        out.push_in(Nm { quals, insert, natts, ret_at: mcx::to_vec_in(ret_at, mcx), sys });
    }
    if !out.is_empty() && !hashed && !nothing {
        shape(node, "MERGE: a NOT MATCHED action over a target that is not hashed or sorted on the join's nullable side");
    }
    out
}

/// D3470: whether every NOT MATCHED [BY TARGET] action is DO NOTHING over quiet quals (none, or ones the effects home's
/// `expr_quiet` passes: no raise on any input, no user or VOLATILE call). PostgreSQL 18.6 plans no outer join for such
/// actions (transform_MERGE_to_join counts no CMD_NOTHING action), so they run only on a MATCHED row's concurrent
/// fall-through (ExecMergeNotMatched over the source row after TM_Deleted or a failed join recheck), where DO NOTHING
/// leaves no trace whichever qual holds: write.merge lowers no arm for them and admits the join as if they were absent.
pub(crate) fn idle(gx: &Gx<'_, '_>, m: &MergeDesc<'_>) -> bool {
    let quiet = |l: Option<&types_nodes::list::NodeList<'_>>| l.is_none_or(|l| l.iter().all(|q| crate::effects::expr_quiet(gx, q)));
    m.actions.iter().filter(|a| a.kind == MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_TARGET).all(|a| a.cmd.is_none() && quiet(a.quals))
}

/// Whether a NOT MATCHED action inserts: only then does a MATCHED row's concurrent fall-through write a row
/// (RETURNING's continuation); DO NOTHING actions write none.
pub(crate) fn inserts(nm: &[Nm<'_>]) -> bool {
    nm.iter().any(|n| n.insert.is_some())
}

/// The NOT MATCHED arm (none without `nm`): from the current block, a row whose target ctid (`tid`) is NULL runs `nm`
/// over `arm` (the source row) and joins `l_join` with its carry; a matched row goes on in the current block after it.
pub(crate) fn lower<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, (nm, ex, node, natts, mt): (&[Nm<'_>], &'static ExprFace, i32, usize, i32), (at, arm, tid, l_join): (&RowVals, &RowVals, Option<(Value, Value)>, Block), op: &dyn RowOpMech, ret: Option<&dyn RetMech>, next: &mut Option<Next<'_, 'e>>) {
    if nm.is_empty() {
        return;
    }
    let Some((_, valid)) = tid else { crate::refuse!(node, Reason::Bug { payload: "a MERGE NOT MATCHED action without the target's ctid" }) };
    let (l_m, l_nm) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(valid, (l_m, &[]), (l_nm, &[]));
    b.switch_to(l_nm);
    let c = chain(b, env, (nm, ex, node, natts, mt), (at, arm), op, ret, next);
    b.br(l_join, &c.params());
    b.switch_to(l_m);
    env.memo.clear();
}

/// The NOT MATCHED actions over `arm` (the source row) from the current block, their carry at the end: the NULL-ctid
/// branch above and a MATCHED action's fall-through (merge.rs, `ret` none there).
#[allow(clippy::too_many_arguments)]
pub(crate) fn chain<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, (nm, ex, node, natts, mt): (&[Nm<'_>], &'static ExprFace, i32, usize, i32), (at, arm): (&RowVals, &RowVals), op: &dyn RowOpMech, ret: Option<&dyn RetMech>, next: &mut Option<Next<'_, 'e>>) -> Carry<'e> {
    let types: PgVec<'_, Type> = at.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
    let l_join = b.create_block(&types[..]);
    for a in nm.iter() {
        // an action's quals run only after every earlier action failed: nothing memoised under one dominates the next
        env.memo.clear();
        let l_next = b.create_block(&[]);
        for &q in a.quals.iter() {
            let (pass, l_on) = (ex.0.pred(b, env, node, arm, q), b.create_block(&[]));
            b.cond_br(pass, (l_on, &[]), (l_next, &[]));
            b.switch_to(l_on);
        }
        let c = match &a.insert {
            None => at.carry.copy_in(env.mcx),
            Some(vals) => {
                let cells = image(b, env, (ex, node, a.natts), arm, vals);
                // EXPLAIN ANALYZE: the row the INSERT action wrote counts on the ModifyTable node
                let mut done = |b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, r: &RowVals| {
                    crate::frame::instr::count_one(b, env.ctx, mt, crate::frame::instr::Ctr::MergeInserted);
                    match (ret, next.as_deref_mut()) {
                        (Some(ret), Some(n)) => {
                            let mut cells = op.cells(b, env, false);
                            // writesubplan (gen mg-0178): the written row's system columns after the site's cells
                            let cut = cells.len().saturating_sub(a.sys);
                            let tail = mcx::to_vec_in(&cells[cut..], env.mcx);
                            cells.truncate(cut);
                            // D888: a view's site carries the wholerow cell after its natts cells
                            let mut written = returned(b, env, &a.ret_at, cells);
                            written.truncate(natts);
                            written.extend_in(tail);
                            ret.lower_merge(b, env, r, &written, &[], true, crate::ops::modifytable::merge::ret::merge_action_insert(), n)
                        }
                        _ => r.carry.copy_in(env.mcx),
                    }
                };
                op.call_op(b, env, at, WRITE_INSERT, RowOpRow { cells: &cells[..], tid: None, rel: None }, &mut done, Arm::None)
            }
        };
        b.br(l_join, &c.params());
        b.switch_to(l_next);
    }
    b.br(l_join, &at.carry.params());
    b.switch_to(l_join);
    env.memo.clear();
    Carry::from_params(b.block_params(l_join), env.mcx)
}

/// INHERIT-ROOT: the written row (`(datum, isnull)` in the written relation's attno order) at RETURNING's attnos
/// (`ret_at`: a routed MERGE's root row read through its first result relation, as ExecInitMerge maps that relation's
/// returningList onto rootRelInfo's attnos), NULL at an attribute the root does not map (a dropped column, never read);
/// as written when `ret_at` is empty.
fn returned<'e>(b: &mut FunctionBuilder<'_>, env: &Env<'e, '_>, ret_at: &[i16], written: PgVec<'e, (Value, Value)>) -> PgVec<'e, (Value, Value)> {
    if ret_at.is_empty() {
        return written;
    }
    let null = b.bconst(true);
    ret_at.iter().map(|&r| usize::try_from(r - 1).ok().and_then(|j| written.get(j).copied()).unwrap_or((env.zero, null))).collect_in(env.mcx)
}

/// An INSERT's tuple, `(datum, valid)` per attribute: its listed values, NULL elsewhere.
fn image<'g>(b: &mut FunctionBuilder<'_>, env: &mut Env<'g, '_>, (ex, node, natts): (&'static ExprFace, i32, usize), arm: &RowVals, vals: &[(usize, ExId, ExprOut, TypeFacts)]) -> PgVec<'g, (Value, Value)> {
    let (t, f) = (b.bconst(true), b.bconst(false));
    let mut cells: PgVec<'_, (Value, Value)> = mcx::pg_vec![in env.mcx; (env.zero, f); natts];
    for (a, x, out, tf) in vals.iter() {
        let (v, valid) = ex.0.emit(b, env, node, arm, *x);
        let isnull = b.int_bin(IntBinOp::Xor, valid, t);
        let cell = Cell::of_expr(*out, 0, 0, Nullable::Maybe);
        let mut cx = env.lx(b, node);
        let (d, null) = rep::datum(&mut cx, CellRef { cell: &cell, val: v, null: Some(isnull) }, tf);
        env.memo = cx.memo;
        if let Some(c) = cells.get_mut(*a) {
            *c = (d, b.int_bin(IntBinOp::Xor, null, t));
        }
    }
    cells
}


/// V4R-495: the join condition's quals (`MergeDesc::join_cond`, ANDed), checked by `check`; none without BY SOURCE actions.
pub(crate) fn cond<'g, 'p>(px: &mut Px<'g, 'p>, m: &MergeDesc<'p>, ex: &'static ExprFace, node: i32, check: &dyn Fn(Node<'_>)) -> PgVec<'g, ExId> {
    let mut out = PgVec::new_in(px.mcx());
    for q in m.join_cond.iter().flat_map(|l| l.iter()) {
        check(q);
        out.push_in(ex.0.compile(px, q, None, node));
    }
    out
}

/// From the current block: the join condition over `arm`; the block goes on where it holds, and the returned block
/// (none without a condition: every row MATCHED, ExecQual of NIL) is where it fails, the BY SOURCE actions' entry.
pub(crate) fn split(b: &mut FunctionBuilder<'_>, env: &mut Env, (cond, ex, node): (&[ExId], &'static ExprFace, i32), arm: &RowVals) -> Option<Block> {
    if cond.is_empty() {
        return None;
    }
    let l_bs = b.create_block(&[]);
    for &q in cond.iter() {
        let (pass, l_on) = (ex.0.pred(b, env, node, arm, q), b.create_block(&[]));
        b.cond_br(pass, (l_on, &[]), (l_bs, &[]));
        b.switch_to(l_on);
    }
    Some(l_bs)
}

/// D1684 (V4R-495): a BY SOURCE row's null-extended source rechecks at the empty row position (marks.rs `store`), which
/// the heap range source reads without a block: a REFERENCE-marked source a tid scan reads refuses. mergecmd D1684
/// (held-out tw-core-149): the index source (D2048) and the bitmap source (hosts bitmap_source.rs `next_substituted`) read
/// it as no row too, so the recheck's build is empty and the row null-extends again, as EvalPlanQualFetchRowMark's no-row.
/// D1685: a COPY-marked source (VALUES, a subquery, a function) on the LEFT join's nullable side stores its NULL row as
/// the empty copy (marks.rs `store`), which the producer making the join's build reads on the recheck as no row (values
/// `next_window`, frame.rs `copy_entry`): NOT MATCHED BY SOURCE again, as PostgreSQL's copy leg gives no test tuple. A
/// copy row no producer takes (the rechecking pipeline's own, a nest inner's) keeps the refusal.
pub(crate) fn keyed_source(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>, node: i32) {
    let produced = |rti: u32| (0..gx.pipelines().len()).any(|q| q != k && gx.pipelines()[q].desc.produces().is_some() && crate::rerun::epq::marks::own_copy_at(gx, q, rti).is_some());
    if w.mark_descs.iter().any(|m| m.mark == RowMarkType::ROW_MARK_COPY && !produced(m.rti)) {
        shape(node, "MERGE: NOT MATCHED BY SOURCE over a VALUES, subquery or function source no producer takes (its empty copy has no build to empty, D1685)");
    }
    let keyed = |q: usize| matches!(gx.pipelines()[q].source, Source::Tid { scanrelid, .. }
        if u32::from(scanrelid) != w.rti && w.mark_descs.iter().any(|m| m.mark == RowMarkType::ROW_MARK_REFERENCE && m.rti == u32::from(scanrelid)));
    if (0..gx.pipelines().len()).any(|q| q != k && keyed(q)) {
        shape(node, "MERGE: NOT MATCHED BY SOURCE over a source a tid scan reads (the recheck's empty row position, D1684)");
    }
}

/// A junk column `(col, facts)` of `at` as `(datum, valid)` (write/merge.rs's target ctid).
pub(crate) fn junk(b: &mut FunctionBuilder<'_>, env: &mut Env, at: &RowVals, (col, tf): &(i16, TypeFacts), node: i32) -> Option<(Value, Value)> {
    let Some(cr) = at.col(*col) else { crate::refuse!(node, Reason::Bug { payload: "a MERGE target's ctid past the child's projection" }) };
    let mut cx = env.lx(b, node);
    let (d, null) = rep::datum(&mut cx, cr, tf);
    env.memo = cx.memo;
    let t = b.bconst(true);
    Some((d, b.int_bin(IntBinOp::Xor, null, t)))
}

/// V4R-498 (write.route's MERGE form): every action DO NOTHING or a NOT MATCHED [BY TARGET] INSERT, which PostgreSQL
/// routes through the root (ExecInitMerge: INSERT actions use the root's layout and its tuple routing).
pub(crate) fn insert_only(m: &MergeDesc<'_>) -> bool {
    m.actions.iter().all(|a| a.cmd.is_none() || a.kind == MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_TARGET && a.cmd == Some(Cmd::Insert))
}

/// V4R-498: a routed MERGE's root `(relid, natts)`: its INSERT image is the root's row, routed by the host (hosts
/// write_route.rs RouteHost); none unrouted, or for write.route's UPDATE / DELETE forms (merge/route.rs).
pub(crate) fn routed(w: &WriteDesc<'_>) -> Option<(u32, usize)> {
    w.route.as_ref().filter(|_| w.merge.as_ref().is_some_and(|m| insert_only(m))).map(|r| (r.root_relid, r.root_natts as usize))
}

/// V4R-498: the routed plan (write.route's `Route::plan`: the leaves admitted, the root set). G2(d): its check options
/// are the result relations' lists, each in its own relation's layout (a pruned leaf's, or several), so the host maps
/// the first one onto each routed leaf by name (hosts write_route.rs RouteHost `checks_of`, ExecInitPartitionInfo);
/// none when the root is its own result relation (its list is in the root's layout).
pub(crate) fn route_plan(gx: &Gx<'_, '_>, node: i32, w: &WriteDesc<'_>, plan: WritePlan, a: ArenaRef) -> WritePlan {
    let plan = crate::frame::iface::need_strategy::<crate::ops::modifytable::strategies::route::Route>(&node, WRITE_ROUTE).plan(gx, node, w, plan, a);
    let root = w.route.as_ref().map_or(0, |r| r.root_relid);
    let checks_of = match w.targets.first() {
        Some(t) if !w.checks.is_empty() && t.relid != root => t.relid,
        _ => 0,
    };
    WritePlan { checks_of, ..plan }
}

/// V4R-498: a routed MERGE's target side, the producer of the LEFT join's nullable slot: a leaf's scan, an Append of
/// several leaves (the union's eager form, V4R-491) or (a partition-less root) the planner's dummy relation, whose rows
/// never join (the routed INSERT then raises PostgreSQL's no-partition error). Its INSERTs never locate or recheck a
/// target row: only the ctid's NULL is read.
pub(crate) fn routed_target(gx: &Gx<'_, '_>, k: usize) -> Option<usize> {
    let ps = gx.pipelines();
    let slot = ps[k].ops.iter().take_while(|o| !matches!(o, Op::Write(_))).find_map(|o| match o {
        Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_LEFT, slot, .. } | JoinDesc::Merge { jointype: JoinType::JOIN_LEFT, slot, .. }) => Some(*slot),
        _ => None,
    })?;
    (0..ps.len()).find(|&q| q != k && ps[q].desc.produces() == Some(slot))
}

/// V4R-498: the target's junk ctid column and the tid type's facts (a routed MERGE's NULL-ctid test, no locator).
pub(crate) fn junk_tid(gx: &Gx<'_, '_>, w: &WriteDesc<'_>, node: i32) -> Option<(i16, TypeFacts)> {
    let col = w.targets[0].junk_col;
    let Some(tf) = typefacts::facts_of(gx.facts, crate::cell::TIDOID).filter(|_| col > 0) else { crate::refuse!(node, Reason::Bug { payload: "a routed MERGE target without its ctid junk column" }) };
    Some((col, tf))
}
