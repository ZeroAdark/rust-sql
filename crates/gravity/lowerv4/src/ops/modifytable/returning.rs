//! Kind `write.returning` (M4-plan §3.1 / §3.10; cards/M4.md L5 `write/returning.rs`; D315): RETURNING in IR. Owner M4-RET.
//! PostgreSQL's ExecProcessReturning over the row as the host wrote it. Decompose puts the RETURNING list as a `Project`
//! after the write op (decompose write.rs), whose Vars name the target (`varno` = the result relation) by attno. The row
//! op runs in its kind's step (face/end.rs `RowOpMech`); on Done the kind hands this kind's continuation (face/ret.rs
//! `RetMech`) the written row as the host left it in the site (`RowOpMech::cells`: an INSERT's /
//! UPDATE's new row with the host's `written()` applied, a BEFORE ROW trigger's row and STORED generated values; a
//! DELETE's deleted version as ExecDelete fetches it) and hands it on keyed by target attno, so the projection after it
//! (step.project) reads the written row, never the row that came in. A Skipped row never reaches `done` and returns
//! nothing. The end is `end.sink` over the RETURNING list (the row description), reporting a row op; the command tag's
//! count stays the host's. The portal pages the rows (D315 route b, the exec runner).
//!
//! Three faces:
//! - `RetMaker`: the continuation a row-op kind fetches for its write op when it carries RETURNING (INSERT, UPDATE);
//! - `StepMaker`: DELETE's hand-off (L0-e, write/delete.rs admits, then hands its write op here): the DELETE row op as a
//!   step over the driving locator, its site carrying the target's cells for the host to fill, then the continuation;
//! - `EndMaker`: the RETURNING write's end (every row-op kind hands its end here when its write carries RETURNING).
//!
//! D669: a RETURNING column of another relation (an `OUTER_VAR` into the child's row: UPDATE ... FROM, DELETE ... USING,
//! MERGE's source) is the child's cell, laid in the inner row after the images (write/ret_cols.rs `lay_outer`).
//! Refused by name (a Shape each): a whole-row or system column of the target, OLD / NEW (PG 18; the ReturningOld / New
//! binds come later: decompose refuses them until then), `merge_action()` (write.merge), a view's `ReturningExpr`, a
//! step between the write and its projection or after it. R5 (D764): under several result relations (an UPDATE / DELETE
//! decompose streams into an Append's arms, V4R-214) each arm projects its own target's list over its target's row, which
//! the multi-target host hands back through the site every arm shares (write.route's `TargetsRoad`), a moved row the move
//! host's (V4R-349); a MERGE of several and OLD through the root (R4c) by name.
use gravity_abi::program::WRITE_DELETE;
use gravity_decompose::ops::shared::modifytable::{Member, Target, WriteDesc};
use gravity_decompose::{Op, PipelineNode};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;
use gravity_program::{Cmd, ExprLife, ExprOut, Pipeline, Rep};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};

use crate::cell::{Cell, Nullable, Row};
use crate::face::end::{Arm, EndMech, RetImages, RetTableoid, RowOpDecl, SYS_CELLS, RowOpMech, RowOpRow, SinkKind, TargetsRoad};
use crate::face::kinds::KindId;
use crate::face::strategy::WRITE_ROUTE;
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{EndMaker, Next, StepMaker};
use crate::frame::instr::{self, Ctr};
use crate::frame::open::OpenWords;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;

use crate::ops::modifytable::face::{RetMaker, RetMech};
use crate::ops::modifytable::ret_cols::{images, lay_outer, lay_sys, outer, outer_at, reads};
use crate::ops::modifytable::ret_sys::{sys_old_plan_at, sys_plan, tableoid};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::WriteReturning, &RetMaker(make_ret)), (KindId::WriteReturning, &StepMaker(make_delete)), (KindId::WriteReturning, &EndMaker(make_end)), (KindId::WriteReturning, &RetImages(images)), (KindId::WriteReturning, &RetTableoid(tableoid))], ..crate::ops::NONE };

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

/// R5 (D764): the target whose RETURNING list the projection after write op `j` of `p` is: the one target's, or an arm's
/// own under several result relations (decompose append.rs `own_returning`: each arm projects its target's list, in that
/// relation's attnos, ExecProcessReturning's per-result-relation projection). D1112: the RETURNING list's SubPlan steps
/// stand between the write and its projection (`proj_at`).
pub(crate) fn target_at<'w, 'p>(p: &PipelineNode<'p>, j: usize, w: &'w WriteDesc<'p>) -> Option<&'w Target<'p>> {
    let Some(Op::Project(l)) = p.ops.get(proj_at(p, j)) else { return None };
    w.targets.iter().find(|t| t.returning.is_some_and(|r| core::ptr::eq(r, *l)))
}

/// D1112 (nest.subplan B4's RETURNING part): the projection's op after write op `j` of `p`, past the SubPlan steps the
/// RETURNING list reads (decompose ops/subplan `place` puts each right before its first reader, the projection: the write
/// op reads none of them), so nest.subplan evaluates each per written row over the row write.returning hands on, after
/// the row op, as ExecProcessReturning evaluates the list after the tuple is written.
pub(crate) fn proj_at(p: &PipelineNode<'_>, j: usize) -> usize {
    (j + 1..p.ops.len()).find(|&i| !matches!(p.ops[i], Op::SubPlan(_))).unwrap_or(p.ops.len())
}

/// D1112: the write op that op `j` of `p` (the RETURNING projection, or one of its SubPlan steps) follows, with only the
/// list's SubPlan steps between (`proj_at`), when that write carries RETURNING.
pub(crate) fn ret_write(p: &PipelineNode<'_>, j: usize) -> Option<usize> {
    let i = (0..j).rev().find(|&i| !matches!(p.ops[i], Op::SubPlan(_)))?;
    (matches!(&p.ops[i], Op::Write(w) if w.returning.is_some()) && matches!(p.ops.get(proj_at(p, i)), Some(Op::Project(_)))).then_some(i)
}

/// D1112 (nest.subplan B4's RETURNING part): every SubPlan of write `w`'s own lists is its RETURNING list's (none in a
/// check, ON CONFLICT or a MERGE action), over one target: each is a step after the write (`proj_at`). A MERGE's
/// RETURNING (write.merge's continuation) keeps B4.
pub(crate) fn returning_only(w: &WriteDesc<'_>) -> bool {
    use gravity_decompose::expr::has_subplan;
    let list = |l: &types_nodes::NodeList<'_>| l.iter().any(has_subplan);
    let oc = w.on_conflict.as_ref().is_some_and(|oc| list(oc.set) || oc.filter.is_some_and(has_subplan));
    // a SubPlan under a view column's OLD / NEW image (PG 18's ReturningExpr) is the RETURNING list's too (decompose
    // ops/subplan `walk_ret` places its step before the projection)
    let ret = |l: &types_nodes::NodeList<'_>| l.iter().any(ret_sub);
    matches!(w.cmd, Cmd::Insert | Cmd::Update | Cmd::Delete) && w.merge.is_none() && w.targets.len() == 1 && w.returning.is_some_and(ret) && !oc && !w.checks.iter().any(|c| has_subplan(c.qual))
}

/// D1112 (rxnested): every SubPlan of write `w`'s own lists is its RETURNING list's or a check option's (an INSERT's or
/// UPDATE's over one target, none in ON CONFLICT or a MERGE action), both kinds present: each is served as it is alone, a
/// check's at the host qual's read before the row is written (write/rowop_sub.rs, its step placed before the write) and
/// the list's as a step after the write (`proj_at`), so PostgreSQL's order holds (ExecWithCheckOptions inside the row op,
/// then ExecProcessReturning).
pub(crate) fn returning_and_checks(w: &WriteDesc<'_>) -> bool {
    use gravity_decompose::expr::has_subplan;
    // a check's qual is an implicit-AND list after the planner (has_subplan opens no list)
    let sub = |n: types_nodes::Node<'_>| n.as_list().map_or_else(|| has_subplan(n), |l| l.iter().any(has_subplan));
    matches!(w.cmd, Cmd::Insert | Cmd::Update) && w.merge.is_none() && w.on_conflict.is_none() && w.targets.len() == 1 && w.returning.is_some_and(|l| l.iter().any(ret_sub)) && w.checks.iter().any(|c| sub(c.qual))
}

/// `returning_and_checks`' write op `j` of `p`: only steps of SubPlans its RETURNING list alone reads stand between the write
/// and the list's projection (`ret_only_sub`; the checks' steps sit before the write).
fn ret_steps_only(p: &PipelineNode<'_>, j: usize, w: &WriteDesc<'_>) -> bool {
    returning_and_checks(w) && p.ops[j + 1..proj_at(p, j)].iter().all(|o| matches!(o, Op::SubPlan(d) if ret_only_sub(w, d.plan_id)))
}

/// Whether `n` holds a SubPlan, a ReturningExpr opened to its expression (ret_cols.rs `walk`).
fn ret_sub(n: types_nodes::Node<'_>) -> bool {
    use types_nodes::NodeTag;
    let mut found = false;
    crate::ops::modifytable::ret_cols::walk(n, &mut |x| {
        found |= matches!(x.node_tag(), NodeTag::T_SubPlan | NodeTag::T_AlternativeSubPlan);
        !found
    });
    found
}

/// writesubplan (D1112, B4's MERGE RETURNING part): SubPlan `id` is read by write `w`'s RETURNING list and by none of its
/// pre-write lists (actions, checks, ON CONFLICT: decompose `refers`), so decompose places its step after the write.
pub(crate) fn ret_only_sub(w: &WriteDesc<'_>, id: i32) -> bool {
    w.reads_own(id) && !gravity_decompose::ops::shared::modifytable::refers(w, id)
}

/// writesubplan: a MERGE over one target whose ops between write op `j` and its RETURNING projection are steps of
/// SubPlans only that list reads (`ret_only_sub`): each runs per returned row over the row write.merge's continuation
/// hands on (merge/ret.rs: OLD, NEW, `merge_action()`), as ExecProcessReturning evaluates the list after the action.
pub(crate) fn merge_ret_steps(p: &PipelineNode<'_>, j: usize, w: &WriteDesc<'_>) -> bool {
    w.merge.is_some() && w.targets.len() == 1 && p.ops[j + 1..proj_at(p, j)].iter().all(|o| matches!(o, Op::SubPlan(d) if ret_only_sub(w, d.plan_id)))
}

/// Write op `j` of pipeline `k`, the target whose RETURNING list the projection right after it reads (the pipeline's
/// last op), and its node.
fn write_of<'g, 'p>(gx: &Gx<'g, 'p>, k: usize, j: usize) -> (&'g WriteDesc<'p>, &'g Target<'p>, i32) {
    let p = &gx.pipelines()[k];
    let node = p.node_id;
    let Some(Op::Write(w)) = p.ops.get(j) else { bug(node, "write.returning over an op that is not the write") };
    if w.returning.is_none() {
        bug(node, "write.returning over a write without RETURNING");
    }
    let Some(t) = target_at(p, j, w).filter(|_| proj_at(p, j) == j + 1 || returning_only(w) || ret_steps_only(p, j, w) || merge_ret_steps(p, j, w)) else { shape(node, "RETURNING: a step between the write and its projection") };
    let Some(list) = t.returning else { bug(node, "a RETURNING target without its list") };
    if w.targets.len() > 1 {
        // R5 (D764): an arm of an UPDATE / DELETE of several result relations (V4R-214) projects its own target's list over
        // the row its target's host hands back (a moved row's: the move host's, V4R-349); mergecmd G2(c): a routed
        // INSERT-only MERGE (V4R-498) projects the first result relation's list over the root's routed row (ExecInitMerge
        // maps that list onto rootRelInfo, merge/nm.rs `returned`); a MERGE that updates or deletes over several by name
        // G2(c) part 2: or a MERGE whose rows come out of a settled union of its result relations' feeds
        // (merge/route.rs `union_targets`): the first one's list over each row's own relation's cells mapped onto it
        // (merge/lower.rs `per_leaf`), as each relation's returningList is the first's mapped onto its attnos
        // writesubplan (gen mg-0179): or an arm of the (b) form (merge/route.rs `b_arm`) projects its own target's list over
        // the row its target's host hands back (a moved row's: the move host's), as R5's UPDATE arm
        let union = || crate::ops::modifytable::merge::route::union_targets(gx, k, w).is_some();
        let b_arm = || !union() && crate::ops::modifytable::merge::route::b_arm(gx, k, w);
        if !matches!(w.cmd, Cmd::Update | Cmd::Delete) && !(w.cmd == Cmd::Merge && ((crate::ops::modifytable::merge::nm::routed(w).is_some() || union()) && core::ptr::eq(t, &w.targets[0]) || b_arm())) {
            shape(node, "RETURNING over several result relations of a MERGE");
        }
    }
    if p.ops.len() != proj_at(p, j) + 1 {
        shape(node, "RETURNING: a step after its projection");
    }
    for n in list.iter() {
        let Some(te) = n.as_target_entry() else { bug(node, "a RETURNING entry that is not a TargetEntry") };
        let mut why = None;
        let known = crate::ops::modifytable::ret_cols::walk(te.expr, &mut |x| {
            why = why.or_else(|| reads(x, w, t));
            why.is_none()
        });
        if let Some(why) = why {
            shape(node, why);
        }
        if !known {
            shape(node, "RETURNING: an expression the walker does not open");
        }
    }
    // R4c (V4R-350): an UPDATE's OLD through a partitioned / inheritance root is its target's replaced version (a moved
    // row's: the one its move deleted); ON CONFLICT DO UPDATE's existing row through a routing root is the version the
    // conflicting row's leaf replaced, in the root's layout (hosts write_route.rs RouteHost::old, its old site counted in
    // sys_off), its system columns the leaf version's (V4R-501's six cells, unmapped: PG reads the leaf's oc_Existing).
    // Several targets by name
    if w.old_new.old && w.on_conflict.as_ref().is_some_and(|c| c.update) && w.members.has(Member::Targets) {
        shape(node, "RETURNING old of ON CONFLICT DO UPDATE through a partitioned root (M4-RET R4c)");
    }
    (w, t, node)
}

/// The continuation: the target's attributes in attno order (a dropped one `None`), as the written row's cells.
struct Ret<'g> {
    atts: PgVec<'g, Option<ExprOut>>,
    node: i32,
    /// The ModifyTable node: its EXPLAIN ANALYZE rows are the returned rows.
    write_node: i32,
    /// R4a: the list reads the image that is not the written row (ret_cols.rs `images`: an INSERT's / UPDATE's OLD, a DELETE's NEW),
    /// laid beside it as the inner row.
    inner_img: bool,
    cmd: Cmd,
    /// D774: the written row's system columns follow its cells (the op's sys site, `WritePlan.sys`); V4R-501 `sys_old`:
    /// the replaced version's follow the old site's (`WritePlan.sys_old`).
    sys: bool,
    sys_old: bool,
    /// R5 (D764): an arm of a write of several result relations: the op's site is as wide as the widest target (or the
    /// SET cells), and the written row is this arm's target's first `atts.len()` cells of it.
    several: bool,
    /// D669: the child's resnos the list reads (another relation's columns) and the inner attno before them (`outer_at`).
    outer: (Vec<i16>, i16),
    /// ON CONFLICT DO UPDATE's OLD flag follows the OLD image (ret_cols.rs `conflict_flag`): true after DO UPDATE's update.
    flag: bool,
    /// D3496: the union's columns of the ORIGINAL plan row on a recheck that scans its members again.
    stored: Option<super::ret_stored::Stored<'g>>,
}

pub(crate) fn make_ret<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn RetMech + 'g> {
    let mut r = ret_of(px.gx, k, j);
    let (w, _, node) = write_of(px.gx, k, j);
    r.stored = super::ret_stored::of(px, k, w, node);
    mcx::pg_box_dyn!(px.mcx(), r => dyn RetMech + 'g)
}

fn ret_of<'g>(gx: &Gx<'g, '_>, k: usize, j: usize) -> Ret<'g> {
    let (w, t, node) = write_of(gx, k, j);
    let p = &gx.pipelines()[k];
    let (relid, natts) = (t.relid, t.site.natts());
    if gx.facts.rel(relid).is_some_and(|r| r.natts.max(0) as usize != natts) {
        bug(node, "a write site whose cells are not the target's attributes");
    }
    let atts = (1..=natts as i16).map(|a| gx.facts.att(relid, a).map(|f| ExprOut { rep: Rep::of_storage(f.typlen, f.byval), typid: f.typid, life: ExprLife::Row })).collect_in(gx.mcx);
    let write_node = p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(node, |n| n.node_id);
    let inner_img = if w.cmd == Cmd::Delete { w.old_new.new } else { w.old_new.old };
    let outer = (t.returning.map_or_else(Vec::new, outer), outer_at(w, t));
    // V4R-501: OLD's system columns take the OLD image's lanes CTID_AT .. TABLEOID_AT, which D669's columns of another
    // relation (at `outer_at` + resno) reach past a target of ~1600 attributes: by name, never one lane over the other
    if sys_old_plan_at(gx, k, w) && outer.0.last().is_some_and(|&r| i32::from(outer.1) + i32::from(r) >= i32::from(crate::frame::site::CTID_AT)) {
        shape(node, "RETURNING old: a system column beside another relation's column laid past the system lanes (a target of ~1600 attributes)");
    }
    let flag = crate::ops::modifytable::ret_cols::conflict_flag(w, t);
    Ret { atts, node, write_node, inner_img, cmd: w.cmd, sys: crate::ops::modifytable::ret_sys::sys_plan_at(gx, k, w).is_some(), sys_old: sys_old_plan_at(gx, k, w), several: w.targets.len() > 1, outer, flag, stored: None }
}

impl<'g> RetMech for Ret<'g> {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, got: &[(Value, Value)], next: Next<'_, 'e>) -> Carry<'e> {
        self.lower_old(b, env, row, got, &[], next)
    }

    fn lower_old<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, got: &[(Value, Value)], old: &[(Value, Value)], next: Next<'_, 'e>) -> Carry<'e> {
        // R5: this arm's target's cells, the first of the shared site's (as wide as the widest target), then its sys cells
        let n_sys = if self.sys { SYS_CELLS as usize } else { 0 };
        let arm: PgVec<'_, (Value, Value)>;
        let got = if self.several && got.len() > self.atts.len() + n_sys {
            arm = mcx::concat_in(&[&got[..self.atts.len()], &got[got.len() - n_sys..]], env.mcx);
            &arm[..]
        } else {
            got
        };
        if got.len() != self.atts.len() + n_sys {
            bug(self.node, "a written row whose cells are not the target's attributes");
        }
        let (got, sys) = got.split_at(self.atts.len());
        // V4R-501: the old site's last six cells are the replaced version's system columns (`WritePlan::with_sys_old`)
        let (old, old_sys) = old.split_at(if self.sys_old { old.len().saturating_sub(SYS_CELLS as usize) } else { old.len() });
        // R4c: an arm's replaced version is its target's first cells of the shared old site
        let old = if self.several && old.len() > self.atts.len() { &old[..self.atts.len()] } else { old };
        let mcx = env.mcx;
        let (mut cells, mut vals, mut cols) = (mcx::reserved_in(got.len(), mcx), mcx::reserved_in(got.len(), mcx), mcx::reserved_in(got.len(), mcx));
        for (att, &(d, isnull)) in self.atts.iter().zip(got.iter()) {
            let Some(out) = att else {
                cols.push_in(None);
                continue;
            };
            let at = cells.len() as u16;
            cells.push_in(Cell::of_expr(*out, at, at, Nullable::Maybe));
            vals.push_in((d, Some(isnull)));
            cols.push_in(Some(at));
        }
        // R4a: the other image as the inner row by attno (ret_cols.rs `images`): an UPDATE's replaced version (the old site), else
        // NULLs (an INSERT's OLD, a DELETE's NEW)
        let mut inner_img = self.inner_img.then(|| {
            if (self.cmd == Cmd::Update && old.len() != self.atts.len()) || (!old.is_empty() && old.len() != self.atts.len()) {
                bug(self.node, "RETURNING old over an UPDATE that fetched no replaced version");
            }
            let (zero, null) = (b.iconst(gravity_ir::Type::I64, 0), b.bconst(true));
            let mut map = mcx::reserved_in(self.atts.len(), mcx);
            for (k, att) in self.atts.iter().enumerate() {
                let Some(out) = att else {
                    map.push_in(None);
                    continue;
                };
                // the replaced version (an UPDATE's, DO UPDATE's existing row), else NULL (a plain INSERT's OLD, a DELETE's NEW)
                let (d, isnull) = if old.is_empty() { (zero, null) } else { old[k] };
                let at = cells.len() as u16;
                cells.push_in(Cell::of_expr(*out, at, at, Nullable::Maybe));
                vals.push_in((d, Some(isnull)));
                map.push_in(Some(at));
            }
            if self.flag {
                // the insert path hands no replaced version, DO UPDATE's the existing row: whether OLD exists here
                let (flag, v, f) = (ExprOut { rep: Rep::of_storage(1, true), typid: 16, life: ExprLife::Row }, b.iconst(gravity_ir::Type::I64, i64::from(!old.is_empty())), b.bconst(false));
                let at = cells.len() as u16;
                cells.push_in(Cell::of_expr(flag, at, at, Nullable::Maybe));
                vals.push_in((v, Some(f)));
                map.push_in(Some(at));
            }
            map
        });
        // D774: the system columns at their lane attnos (ctid CTID_AT, xmin .. cmax XMIN_AT .., tableoid TABLEOID_AT)
        if !sys.is_empty() {
            lay_sys(sys, false, &mut cols, &mut cells, &mut vals);
        }
        if !self.outer.0.is_empty() {
            let stored = self.stored.as_ref().map(|s| s.row(b, env, self.node, row));
            lay_outer(self.node, stored.as_ref().unwrap_or(row), &self.outer, inner_img.get_or_insert_with(|| PgVec::new_in(mcx)), &mut cells, &mut vals);
        }
        // V4R-501: the replaced version's system columns at the OLD image's lanes, NULL where no version was replaced
        if self.sys_old {
            let null = [(b.iconst(gravity_ir::Type::I64, 0), b.bconst(true)); SYS_CELLS as usize];
            lay_sys(if old_sys.is_empty() { &null[..] } else { old_sys }, true, inner_img.get_or_insert_with(|| PgVec::new_in(mcx)), &mut cells, &mut vals);
        }
        if self.write_node != self.node {
            instr::count_one(b, env.ctx, self.write_node, Ctr::Rows);
        }
        let out = RowVals { row: Row { cells }, vals, cols, carry: row.carry.copy_in(env.mcx), next: row.next, at: row.at };
        // the memo keys cells by `At`: the written row's `Value(k)` must not answer from the incoming row's
        env.memo.clear();
        let inner = core::mem::replace(&mut env.inner, inner_img);
        let c = next(b, env, &out);
        env.inner = inner;
        env.memo.clear();
        c
    }

    fn lower_merge<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, got: &[(Value, Value)], old: &[(Value, Value)], new: bool, action: &'static [u8], next: Next<'_, 'e>) -> Carry<'e> {
        // writesubplan (gen mg-0178): the written row's system columns after its cells (D774: the sys site)
        let n_sys = if self.sys { SYS_CELLS as usize } else { 0 };
        let (got, sys) = if n_sys > 0 && got.len() == self.atts.len() + n_sys { got.split_at(self.atts.len()) } else { (got, &[][..]) };
        crate::ops::modifytable::merge::ret::lower(b, env, row, (&self.atts[..], self.node, self.write_node, &self.outer), (got, sys), old, new, action, next)
    }
}

/// DELETE ... RETURNING (L0-e's hand-off): the row op as a step over the driving locator, then the continuation. The
/// write site carries the target's cells, which the host fills with the deleted version on Done (ExecDelete's
/// `table_tuple_fetch_row_version(SnapshotAny)`); the IR stores none of them.
struct DeleteRet<'g> {
    op: PgBox<'g, dyn RowOpMech + 'g>,
    ret: Ret<'g>,
    tid: crate::ops::modifytable::update_built::Tid,
    node: i32,
    /// D655 Row C (V4R-534): the op rechecks in the IR (a data-modifying CTE's): the recheck its `Arm::Epq` reruns.
    epq: Option<crate::rerun::epq::epq_ir::Epq<'g>>,
    /// ROWC-UNION: under the union form each row's result relation by its junk tableoid (write/update_built.rs `union_rel`).
    union: crate::ops::modifytable::update_built::Union<'g>,
    /// ROWC-UNION: the site's cells (as many as the widest target's) and the row's own relation's cells onto the first
    /// target's attributes (write/delete_built.rs `ret_map`).
    map: Option<(usize, PgVec<'g, (u32, PgVec<'g, Option<usize>>)>)>,
}

pub(crate) fn make_delete<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let gx = px.gx;
    let (w, t, node) = write_of(gx, k, j);
    if w.cmd != Cmd::Delete {
        bug(node, "write.returning's DELETE step over a write that is not a DELETE");
    }
    // D667: a view's DELETE is write.transition's INSTEAD OF row op, the continuation over the view's OLD
    if w.members.has(Member::TargetView) {
        return crate::ops::modifytable::triggers::view_step(px, k, j);
    }
    let mut ret = ret_of(gx, k, j);
    // D2734 ROWC-UNION (gen mt-0354): or the rows of a settled union of the result relations' feeds (write/delete_built.rs
    // `union_end`: the plan names every target, the position is the junk ctid at the first feed's site and the row's
    // relation its junk tableoid); the first target's list reads the row's own cells mapped onto its attributes
    if let Some(feeds) = super::delete_built::union_feeds(gx, k, w) {
        let (relid, plan) = super::delete_built::union_plan(px, k, w, node);
        let plan = match crate::ops::modifytable::ret_sys::sys_plan_at(gx, k, w) {
            Some(header) => plan.with_sys(header),
            None => plan,
        };
        let union = crate::ops::modifytable::update_built::union_facts(px, k, node, w);
        let (loc, tid) = crate::ops::modifytable::update_built::locate(px, node, w, Some(feeds[0]));
        let natts = super::rowop_epq::epq_width(w) as u16;
        let op = super::rowop_step::make_op(px, k, RowOpDecl { op: WRITE_DELETE, relid, natts, plan, loc, conflict: false });
        let epq = op.epq_in_ir().then(|| super::delete_built::union_epq(px, (k, j), w, &feeds, node));
        let map = super::delete_built::ret_map(gx, w).map(|m| (natts as usize, m));
        ret.stored = super::ret_stored::of(px, k, w, node);
        return mcx::pg_box_dyn!(px.mcx(), DeleteRet { op, ret, tid, node, epq, union, map } => dyn StepMech + 'g);
    }
    // R5 (D764): an arm of several result relations deletes its own target's row under the plan write.route routed over
    // every target (write/delete.rs's own end), through the one site every arm shares (write/rowop.rs `claim`): as wide
    // as the widest target, whose first cells the host fills with the arm's deleted row
    let (relid, natts, plan) = match w.targets.len() > 1 {
        true => {
            let (_, relid, plan) = (crate::frame::iface::need_strategy::<TargetsRoad>(&node, WRITE_ROUTE).0)(gx, k, w, px.arena);
            (relid, w.targets.iter().map(|t| t.site.natts()).max().unwrap_or(0), plan)
        }
        // as write/delete.rs's own end: a root pruned to one leaf (D665) or to none (D829, dummy.rs pruned_root) writes
        // under the root (WritePlan::with_route, no leaves), whose statement triggers fire (ExecInitModifyTable's rootRelation)
        false => (w.plan.target_rel, t.site.natts(), match &w.route {
            Some(r) if w.members.has(Member::Route) || w.members.has(Member::Targets) => w.plan.copy_in(px.arena).with_route(px.arena, r.root_relid, &[]),
            _ => w.plan.copy_in(px.arena),
        }),
    };
    let plan = match sys_plan(w) {
        Some(header) => plan.with_sys(header),
        None => plan,
    };
    // D882: a target hashed on a join's build side is deleted at its junk ctid, as write.lock's end (write/delete_built.rs)
    // D1933B: or the inner of one nested loop (write/nest_write.rs `dml`), as write.lock's end admits it
    let built = if w.targets.len() > 1 { None } else { crate::ops::modifytable::nest_write::dml(gx, k, w, node).or_else(|| crate::ops::modifytable::merge::admit::built_target(gx, k, w)) };
    let (loc, tid) = crate::ops::modifytable::update_built::locate(px, node, w, built);
    let decl = RowOpDecl { op: WRITE_DELETE, relid, natts: natts as u16, plan, loc, conflict: false };
    let op = super::rowop_step::make_op(px, k, decl);
    ret.stored = super::ret_stored::of(px, k, w, node);
    // D2739 NEST-INNER (subq-0323): the target a nest's inner (write/rowop_epq.rs `nest_inner`): its own redo
    let inner = op.epq_in_ir().then(|| super::rowop_epq::nest_inner(gx, k, j, w)).flatten();
    // D3824 DRAIN-COPY: or the target hashed under a RIGHT SEMI drain probed by a COPY-marked CTE's rows
    // (write/rowop_epq.rs `drain_copy`)
    let drain = op.epq_in_ir().then(|| super::rowop_epq::drain_copy(gx, k, j, w)).flatten();
    // D3870 BUILT-HELD: or the target hashed, probed by a COPY-marked CTE's rows (write/rowop_epq.rs `built_held`)
    let held = op.epq_in_ir().then(|| super::rowop_epq::built_held(gx, k, j, w)).flatten();
    let copy = w.mark_descs.iter().find(|m| m.rti != w.rti).map(|m| (m.ctid_col, m.rti));
    let epq = op.epq_in_ir().then(|| match (drain, copy, inner, held) {
        // D3838 ROWC-INIT: each form arms the InitPlans the CTE write's first recheck runs again (`cte_armed`)
        (Some((_, f)), Some(copy), _, _) => {
            let e = crate::rerun::epq::epq_ir::epq_of_drain_copy(px, f.build, f.probe, (k, f.drain, j), copy, node, relid, crate::rerun::epq::epq_ir::ROWC_SYS);
            crate::rerun::epq::epq_ir::cte_armed(px, k, e)
        }
        (_, _, _, Some((_, f))) => {
            let e = crate::rerun::epq::epq_ir::epq_of_built_held(px, f.build, (k, f.probe, j), node, relid, crate::rerun::epq::epq_ir::ROWC_SYS);
            crate::rerun::epq::epq_ir::cte_armed(px, k, e)
        }
        (_, _, Some((_, ni)), _) => {
            let e = crate::rerun::epq::epq_ir::epq_of_inner(px, (k, j), ni, relid, node, crate::rerun::epq::epq_ir::ROWC_SYS);
            crate::rerun::epq::epq_ir::cte_armed(px, k, e)
        }
        _ => crate::rerun::epq::epq_ir::epq_of_subs(px, k, j, node, relid, crate::rerun::epq::epq_ir::ROWC_SYS),
    });
    mcx::pg_box_dyn!(px.mcx(), DeleteRet { op, ret, tid, node, epq, union: None, map: None } => dyn StepMech + 'g)
}

impl StepMech for DeleteRet<'_> {
    fn prologue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _x: &Exits) {
        self.op.open(b, env);
    }

    fn wants_locator(&self) -> bool {
        true
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let (op, ret) = (&*self.op, &self.ret);
        let tid = crate::ops::modifytable::update_built::junk_tid(&self.tid, self.node, b, env, row);
        let rel = crate::ops::modifytable::update_built::union_rel(&self.union, self.node, b, env, row);
        let mut none = |_: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _: &RowVals| PgVec::<(Value, Value)>::new_in(env.mcx);
        let arm = match &self.epq { Some(e) => Arm::Epq(e, &mut none), None => Arm::None };
        op.call_op(b, env, row, WRITE_DELETE, RowOpRow { cells: &[], tid, rel }, &mut |b, env, r| {
            let mut written = op.cells(b, env, false);
            // ROWC-UNION: the row's own relation's cells (the site as wide as the widest target) as the first one's,
            // its system cells after them
            if let Some((n, m)) = &self.map {
                let cut = written.len().min(*n);
                let mut out = super::delete_built::per_leaf(b, env, r, (&self.union, self.node), m, &written[..cut]);
                out.extend_from_slice_in(&written[cut..]);
                written = out;
            }
            ret.lower(b, env, r, &written, &mut *next)
        }, arm)
    }
}

/// The RETURNING write's end: `end.sink` over the RETURNING list, declared a row op (the frame sets the locator).
struct RetEnd<'g> {
    sink: PgBox<'g, dyn EndMech + 'g>,
}

pub(crate) fn make_end<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let p = &px.gx.pipelines()[k];
    let node = p.node_id;
    let Some(j) = p.ops.iter().position(|o| matches!(o, Op::Write(w) if w.returning.is_some())) else {
        bug(node, "write.returning's end over a pipeline whose write has no RETURNING")
    };
    write_of(px.gx, k, j);
    let sink = (crate::frame::iface::kind_impl::<EndMaker>(&node, KindId::EndSink).0)(px, k);
    mcx::pg_box_dyn!(px.mcx(), RetEnd { sink } => dyn EndMech + 'g)
}

impl EndMech for RetEnd<'_> {
    fn state_bytes(&self) -> u32 {
        self.sink.state_bytes()
    }
    fn open_words(&self, w: &mut OpenWords) {
        self.sink.open_words(w)
    }
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        self.sink.open(b, env)
    }
    fn carried<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>) -> PgVec<'e, Value> {
        self.sink.carried(b, env)
    }
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, x: &Exits) -> Carry<'e> {
        self.sink.lower(b, env, row, x)
    }
    fn window_end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, c: &Carry) {
        self.sink.window_end(b, env, c)
    }
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        self.sink.finish(b, env, x)
    }
    fn sink(&self) -> SinkKind {
        SinkKind::RowOp
    }
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize, d: Pipeline) -> Pipeline {
        self.sink.pipeline_desc(px, k, d)
    }
}
