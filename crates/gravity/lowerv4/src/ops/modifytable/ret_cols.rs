//! What a RETURNING list of kind `write.returning` reads beyond the target's attributes (write/returning.rs; cards/M4.md
//! L5). Owner M4-RET. A helper file (split from write/returning.rs, M4-CLOSE B3): it registers nothing.
//!
//! `reads` names what the written row does not carry (a Shape each); the rest is served. D774: the whole row and OLD / NEW
//! whole rows where the image is always a row or a per-row flag says which (`whole_image`; D2729 MERGE's); system columns
//! are write/ret_sys.rs's. R5 (D764): under
//! several result relations each arm reads its own target's list (`Target`). `walk` opens PG 18's ReturningExpr (V4R-337).
//! D669: a column of another relation (`OUTER_VAR`, the child's resno) is the child's row's cell, laid in the inner row
//! after the images (`outer`, `outer_at`, `lay_outer`; gravity_expr `Returning.outer`). `images` (R4a, write.returning's
//! `RetImages`: where OLD / NEW, merge_action() and those columns sit) moved here from write/returning.rs with no logic
//! change (M4-CLOSE B3c: D669 took returning.rs past its gate). `lay_sys` lays the six system-column cells at their lanes:
//! the written row's (D774) and, V4R-501, the replaced version's as the OLD image's (write_ret_sys `old_sys`).
use gravity_decompose::ops::shared::modifytable::{Target, WriteDesc};
use gravity_decompose::Op;
use gravity_ir::Value;
use gravity_program::protos::{CMAX_AT, CMIN_AT, XMAX_AT, XMIN_AT};
use gravity_program::{Cmd, ExprLife, ExprOut, Rep};
use mcx::{GrowIn, PgVec};
use types_nodes::primnodes::{VarReturningType, INNER_VAR, OUTER_VAR};
use types_nodes::{Node, NodeTag};

use crate::ops::modifytable::ret_sys::{image, may_move, old_sys, sys_served, sys_site};
use crate::cell::{Cell, Nullable};
use crate::face::end::TABLEOID_AT;
use crate::frame::row::RowVals;
use crate::frame::site::CTID_AT;
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};

/// D774: a whole-row OLD / NEW is served where its image is always a row: the row written (an INSERT's NEW, a DELETE's
/// OLD) or both of an UPDATE's; ON CONFLICT DO UPDATE's OLD through its flag (`conflict_flag`: NULL for an inserted row).
/// D883 / D2729: a MERGE's both, through the image flags merge/ret.rs lays per action (`images`: OLD at the inner row's
/// offset 0, NEW at natts when an action deletes, else the written row), NULL where the action has none (an INSERT's OLD,
/// a DELETE's NEW), as ExecEvalWholeRowVar under EEO_FLAG_OLD_IS_NULL / NEW_IS_NULL; gravity_expr var.rs forms the row at
/// its offset.
fn whole_image(rt: VarReturningType, w: &WriteDesc<'_>, t: &Target<'_>) -> bool {
    let old = rt == VarReturningType::VAR_RETURNING_OLD;
    match w.cmd {
        Cmd::Update | Cmd::Merge => true,
        Cmd::Insert => !old || conflict_flag(w, t),
        Cmd::Delete => old,
        _ => false,
    }
}

/// ON CONFLICT DO UPDATE's OLD exists per row (the existing row DO UPDATE replaced; none for an inserted row), so where
/// target `t`'s RETURNING list tests it (a ReturningExpr over OLD, a whole-row OLD) write.returning lays a boolean cell
/// after the OLD image that says which, as ExecProcessReturning sets EEO_FLAG_OLD_IS_NULL per row (`images`' old_flag).
pub(crate) fn conflict_flag(w: &WriteDesc<'_>, t: &Target<'_>) -> bool {
    let tests = |x: Node<'_>| x.as_returning_expr().is_some_and(|r| r.retold) || x.as_var().is_some_and(|v| v.varattno == 0 && v.varreturningtype == VarReturningType::VAR_RETURNING_OLD);
    w.cmd == Cmd::Insert
        && w.on_conflict.as_ref().is_some_and(|c| c.update)
        && t.returning.is_some_and(|l| {
            l.iter().filter_map(|n| n.as_target_entry()).any(|te| {
                let mut hit = false;
                walk(te.expr, &mut |x| {
                    hit |= tests(x);
                    !hit
                });
                hit
            })
        })
}

/// gravity_decompose's `expr::walk`, opening PG 18's `ReturningExpr` (the rewriter's wrapper of a view's or a virtual
/// generated column's OLD / NEW expression) to its expression, as gravity_expr compiles it (misc.rs `compile_returning`).
pub(crate) fn walk<'p>(node: Node<'p>, f: &mut dyn FnMut(Node<'p>) -> bool) -> bool {
    crate::stack_check();
    if !f(node) {
        return false;
    }
    if let Some(r) = node.as_returning_expr() {
        return walk(r.retexpr, f);
    }
    let mut ok = true;
    let known = gravity_decompose::expr::kids(node, &mut |k| ok &= walk(k, f));
    ok && known
}

/// D669: the child's resnos a RETURNING list reads (its `OUTER_VAR` columns: another relation's), ascending.
pub(crate) fn outer(list: &types_nodes::NodeList<'_>) -> Vec<i16> {
    let mut out = Vec::new(); // alloc: Program
    for n in list.iter() {
        let Some(te) = n.as_target_entry() else { continue };
        walk(te.expr, &mut |x| {
            if let Some(v) = x.as_var().filter(|v| v.varno == OUTER_VAR && v.varattno > 0 && v.varlevelsup == 0) {
                out.push(v.varattno); // alloc: Program
            }
            true
        });
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// D669: the inner row's attno before the child's first column: past MERGE's images and action text (`lower_merge`), past
/// the other image when the list reads it (`Ret::inner_img`), else 0; `t` is the list's target (an arm's own under R5).
pub(crate) fn outer_at(w: &WriteDesc<'_>, t: &Target<'_>) -> i16 {
    let n = t.site.natts() as i16;
    match w.cmd {
        // past OLD, NEW, merge_action()'s text and the two image flags (merge/ret.rs `lower`)
        Cmd::Merge => 2 * n + 3,
        Cmd::Delete if w.old_new.new => n,
        Cmd::Delete => 0,
        // past ON CONFLICT DO UPDATE's OLD flag when it is laid (`conflict_flag`)
        _ if w.old_new.old => n + i16::from(conflict_flag(w, t)),
        _ => 0,
    }
}

/// R4a (`RetImages`): the sources the RETURNING projection at `(k, j)` reads OLD / NEW from, when it reads either: the
/// written row is the scan row (an INSERT's / UPDATE's NEW, a DELETE's OLD); the other image is the inner row the
/// continuation lays beside it (write/returning.rs `Ret::lower_old`: an UPDATE's replaced version, else NULLs). D789: a MERGE's images differ
/// by action, so both are the inner row's (OLD at 1..=natts, NEW after), `merge_action()`'s text cell last (`lower_merge`).
/// D669: the child's row's columns the list reads (another relation's, `OUTER_VAR`) follow in the inner row, at their resnos
/// past the images the continuation lays there (`outer_at`).
pub(crate) fn images(gx: &Gx<'_, '_>, k: usize, j: usize) -> Option<gravity_expr::nodes::Returning> {
    use gravity_expr::nodes::Returning;
    use gravity_expr::Source;
    let p = gx.pipelines().get(k)?;
    // D1112: op `j` the RETURNING projection, or a SubPlan step of its list before it (its arguments read the images too)
    let wi = crate::ops::modifytable::returning::ret_write(p, j)?;
    let Some(Op::Project(l)) = p.ops.get(crate::ops::modifytable::returning::proj_at(p, wi)) else { return None };
    let Some(Op::Write(w)) = p.ops.get(wi) else { return None };
    let action = w.merge.as_ref().is_some_and(|m| m.returning_action);
    // R5: an arm's own target's list (its natts)
    let t = w.targets.iter().find(|t| t.returning.is_some_and(|r| core::ptr::eq(r, *l)))?;
    let reads_outer = !outer(l).is_empty();
    if !(w.old_new.old || w.old_new.new || action || reads_outer) {
        return None;
    }
    let n = t.site.natts() as i16;
    let outer = reads_outer.then_some((Source::Inner, outer_at(w, t)));
    // which image exists for every row (a ReturningExpr over one that does not is NULL): MERGE's differ by action, an
    // INSERT's OLD by ON CONFLICT DO UPDATE's row (its flag after the OLD image where the list tests it, `conflict_flag`).
    // D883: a MERGE without a DELETE action has NEW = the written row (offset 0)
    let old_flag = conflict_flag(w, t).then_some((Source::Inner, n + 1));
    let (conflict, del) =(w.on_conflict.as_ref().is_some_and(|c| c.update), w.merge.as_ref().is_none_or(|m| m.actions.iter().any(|a| a.cmd == Some(Cmd::Delete))));
    // V4R-501: OLD's system columns at the OLD image's lanes (write/returning.rs lays them from the old site)
    let old_sys = crate::ops::modifytable::ret_sys::sys_old_plan_at(gx, k, w).then_some(gravity_program::protos::SYS_AT_BASE);
    // D3379 UNION-OLD: under the union form OLD's tableoid is the replaced version's (`old_sys`), not the first target's
    let old_tableoid = w.route.filter(|_| !crate::ops::modifytable::ret_sys::union_old(gx, k, w)).map(|_| t.relid);
    Some(match w.cmd {
        Cmd::Merge => Returning { old: (Source::Inner, 0), new: if del { (Source::Inner, n) } else { (Source::Scan, 0) }, action: Some((Source::Inner, 2 * n + 1)), old_exists: None, new_exists: None, old_flag: Some((Source::Inner, 2 * n + 2)), new_flag: Some((Source::Inner, 2 * n + 3)), outer, old_tableoid: None, old_sys: None },
        Cmd::Delete => Returning { old: (Source::Scan, 0), new: (Source::Inner, 0), action: None, old_exists: Some(true), new_exists: Some(false), old_flag: None, new_flag: None, outer, old_tableoid: None, old_sys: None },
        // R4c: under a root, OLD's tableoid is the arm's target (a moved row's source leaf), never the written row's
        Cmd::Update => Returning { old: (Source::Inner, 0), new: (Source::Scan, 0), action: None, old_exists: Some(true), new_exists: Some(true), old_flag: None, new_flag: None, outer, old_tableoid, old_sys },
        _ => Returning { old: (Source::Inner, 0), new: (Source::Scan, 0), action: None, old_exists: (!conflict).then_some(false), new_exists: Some(true), old_flag, new_flag: None, outer, old_tableoid: None, old_sys },
    })
}

/// D669: the child's row's columns `resnos` (another relation's, the row that came into the write: `row`) laid as the
/// inner row's attnos `at + r` after `map`'s (`images`' outer). Each is that row's cell as it is, its own `At` and value (a
/// lane cell's value is its element's address, read through the lane; never re-read as a datum), so the projection reads
/// the source row's column as the child produced it.
pub(crate) fn lay_outer(node: i32, row: &RowVals, (resnos, at): &(Vec<i16>, i16), map: &mut PgVec<'_, Option<u16>>, cells: &mut PgVec<'_, Cell>, vals: &mut PgVec<'_, (Value, Option<Value>)>) {
    if map.len() != *at as usize {
        crate::refuse!(node, Reason::Bug { payload: "RETURNING: the inner row's images are not where the child's columns start" });
    }
    for &r in resnos {
        let Some(c) = row.col(r) else { crate::refuse!(node, Reason::Shape { why: ShapeWhy("RETURNING: a column of another relation the child's row does not carry") }) };
        let k = (at + r) as usize;
        map.resize_in(map.len().max(k), None);
        map[k - 1] = Some(cells.len() as u16);
        cells.push_in(*c.cell);
        vals.push_in((c.val, c.null));
    }
}

/// A column map `lay_sys` grows to every system column's lane attno: the written row's or the OLD image's inner row
/// (`Env::inner`), both in the context.
pub(crate) trait SysMap {
    fn grown(&mut self) -> &mut [Option<u16>];
}

impl SysMap for PgVec<'_, Option<u16>> {
    fn grown(&mut self) -> &mut [Option<u16>] {
        self.resize_in(self.len().max(TABLEOID_AT as usize), None);
        self
    }
}

/// D774: six system-column cells (ctid, xmin, cmin, xmax, cmax, tableoid) at their lane attnos of `map` (the written
/// row's by attno, CTID_AT .. TABLEOID_AT; V4R-501 the OLD image's inner row too): `null`, the old site's NULL flags (no
/// replaced version: an inserted row), else never NULL.
pub(crate) fn lay_sys(sys: &[(Value, Value)], null: bool, map: &mut impl SysMap, cells: &mut PgVec<'_, Cell>, vals: &mut PgVec<'_, (Value, Option<Value>)>) {
    use types_core::catalog::{CIDOID, OIDOID, TIDOID, XIDOID};
    let map = map.grown();
    let word = |typid| ExprOut { rep: Rep::of_storage(4, true), typid, life: ExprLife::Row };
    let tid = ExprOut { rep: gravity_program::OutCol::new("", TIDOID, -1, 6, false).rep, typid: TIDOID, life: ExprLife::Row };
    let lanes = [(CTID_AT, tid), (XMIN_AT, word(XIDOID)), (CMIN_AT, word(CIDOID)), (XMAX_AT, word(XIDOID)), (CMAX_AT, word(CIDOID)), (TABLEOID_AT, word(OIDOID))];
    for (&(d, isnull), (a, out)) in sys.iter().zip(lanes) {
        let at = cells.len() as u16;
        cells.push_in(Cell::of_expr(out, at, at, if null { Nullable::Maybe } else { Nullable::Never }));
        vals.push_in((d, null.then_some(isnull)));
        map[a as usize - 1] = Some(at);
    }
}

/// What a RETURNING expression node of target `t`'s list reads that the written row does not carry.
pub(crate) fn reads(x: Node<'_>, w: &WriteDesc<'_>, t: &Target<'_>) -> Option<&'static str> {
    // the list's own relation: the one target's (`w.rti`), or under several result relations the arm's (R5)
    let rti = if w.targets.len() > 1 { t.rti } else { w.rti } as i32;
    match x.node_tag() {
        NodeTag::T_Var => {
            let v = x.as_var()?;
            let gen = t.generated.iter().find(|g| g.0 == v.varattno).map(|g| g.1);
            let img = v.varreturningtype != VarReturningType::VAR_RETURNING_DEFAULT;
            if img && (v.varno != rti || v.varlevelsup != 0) {
                Some("RETURNING old / new of another relation")
            } else if img && v.varattno <= 0 && image(&v, w).0 {
                // D774: NULL (an image the command never has)
                None
            } else if img && v.varattno < 0 && image(&v, w).1 && (sys_served(&v, w, t) || sys_site(w)) {
                // D774: the written row's, as a default reference's (the sys site, or the plan-time tableoid)
                None
            } else if img && v.varattno == -6 && w.cmd == Cmd::Update && may_move(w) {
                // R4c (V4R-350): OLD's tableoid under a root is the arm's target (`images`: old_tableoid); D3379 UNION-OLD
                // under the union form the replaced version's (ret_sys.rs `union_old`)
                None
            } else if img && v.varattno < 0 && v.varreturningtype == VarReturningType::VAR_RETURNING_OLD {
                // V4R-501: the replaced version's, the old site's last six cells (write_ret_sys `old_sys`)
                old_sys(&v, w).err()
            } else if img && v.varattno < 0 {
                // MERGE's NEW (its images differ by action)
                Some("RETURNING old / new: a system column of the replaced version (UPDATE's old ctid, xmin .. cmax), MERGE's or ON CONFLICT's")
            } else if img && v.varattno == 0 && !whole_image(v.varreturningtype, w, t) {
                // D774: PG's whole-row OLD / NEW is NULL where that image is (EEO_FLAG_OLD_IS_NULL / NEW_IS_NULL), where
                // the continuation lays NULL cells and no flag says which
                Some("RETURNING old / new: a whole row of an image that may be NULL (no per-row flag says which)")
            } else if img && gen == Some(b'v') {
                Some("RETURNING: a virtual generated column")
            } else if v.varreturningtype == VarReturningType::VAR_RETURNING_OLD {
                // the host's fetched replaced version (UPDATE), the deleted row (DELETE), NULLs (INSERT): generated
                // columns as stored
                None
            } else if v.varno == OUTER_VAR && v.varattno > 0 && v.varlevelsup == 0 {
                // D669: a column of another relation (UPDATE .. FROM, DELETE .. USING, MERGE's source), the child's row's
                // at its resno: the continuation lays that row's cell beside the written row (`outer`)
                None
            } else if v.varno == OUTER_VAR || v.varno == INNER_VAR || v.varno != rti || v.varlevelsup != 0 {
                Some("RETURNING: a column of another relation (UPDATE ... FROM, DELETE ... USING)")
            } else if v.varattno == 0 {
                // D774: the written row as the target's rowtype (expr.wholerow over the cells by attno, a dropped one NULL)
                None
            } else if v.varattno < 0 && (sys_served(&v, w, t) || sys_site(w)) {
                // D774: tableoid of the one heap target is a constant; the others (a routed row's tableoid too) the host
                // hands back after the row op (WritePlan.sys, hosts write_sys.rs; a DELETE's deleted version fetched for
                // them even where the site has no cell, D870)
                None
            } else if v.varattno < 0 {
                Some("RETURNING: a system column of the target")
            } else if gen == Some(b'v') {
                // the rewriter expands a virtual column's references; one left would read the stored NULL
                Some("RETURNING: a virtual generated column")
            } else if gen.is_some() && !matches!(w.cmd, Cmd::Delete | Cmd::Insert | Cmd::Update | Cmd::Merge) {
                // the host computes it (ExecComputeStoredGenerated) after the IR stored the row; H-c (841186c66f1)
                // hands it back through written() into the site. INSERT's and UPDATE's proofs are m4_returning's ret_gen
                // and ret_gs (generated_stored#58's shape, several targets' through their leaf hosts); MERGE's actions run the same host row ops (v5 mergecmd_ret_stored_gen)
                Some("RETURNING: a STORED generated column the host computes (H-c, D236)")
            } else {
                None
            }
        }
        // a view's or a virtual generated column's OLD / NEW expression, NULL where the image does not exist: known per
        // command (gravity_expr compile_returning), per MERGE action from the image flags merge/ret.rs lays, per ON
        // CONFLICT DO UPDATE row from the OLD flag write.returning lays (`images`, `conflict_flag`): served
        NodeTag::T_MergeSupportFunc if w.cmd != Cmd::Merge => Some("RETURNING merge_action()"),
        _ => None,
    }
}
