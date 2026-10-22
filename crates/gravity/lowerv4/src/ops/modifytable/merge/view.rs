//! MERGE into a view (V4R-353, M4-CLOSE B7): kind `write.merge`'s view arm. Owner M4-CLOSE. PostgreSQL 18.6: a view
//! target (its INSTEAD OF row triggers, or none when every action is DO NOTHING) has no ctid. ExecModifyTable hands
//! ExecMerge the child's `wholerow` junk as the old tuple (a NULL one is the not-matched path), ExecMergeMatched stores
//! it in ri_oldTupleSlot, and an UPDATE action's projection takes every column its SET list leaves from it
//! (ExecBuildUpdateProjection's scan tuple). The INSTEAD OF row triggers are the write (hosts write.rs H-f
//! `bind_merge_view` / `view_write`: each action's CheckValidResultRel, D636's view check over the trigger's row). No
//! lock, no recheck and no EvalPlanQual, so the marks PG gives the view query's relations are never read. Here: the
//! wholerow cell (decompose `MergeDesc::rowid_col`), the old row's fields read in place (copy_row.rs `fields_of`), and
//! the refusals by name of the arm. An action reads the view query's columns as the child's (the rewriter points the
//! actions at the view's subquery); a Var of the target copy itself is refused by write/merge.rs's walk.
use gravity_decompose::catalog::typefacts::{self, TypeFacts};
use gravity_decompose::ops::shared::modifytable::{Member, TargetKind, WriteDesc};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, Value};
use mcx::{CollectIn, PgVec};
use types_nodes::parsenodes::WCOKind;

use crate::frame::env::Env;
use crate::frame::row::RowVals;
use crate::graph::Gx;
use crate::admit::refusal::Reason;
use crate::rep;

use crate::ops::modifytable::merge::admit::shape;

/// A view target's row: the child's wholerow column and its row type's facts, and the view's attribute types by attno.
pub(crate) struct ViewRow<'g> {
    wholerow: i16,
    tf: TypeFacts,
    typids: PgVec<'g, u32>,
}

/// The view arm's refusals by name, after write/merge_admit.rs's shared ones (members, joins, the chain).
/// D888: RETURNING reads each action's INSTEAD OF row as the view host hands it back (hosts write_view_rows.rs `written`:
/// an UPDATE's final NEW, a DELETE's OLD; the INSERT trigger's row), OLD the wholerow (`old`), as V4R-331's INSERT /
/// UPDATE / DELETE RETURNING reads them; the view's natts cells, never the wholerow cell after them (merge/lower.rs).
pub(crate) fn admit(node: i32, w: &WriteDesc<'_>) {
    // V4R-330's rule: the host checks an outer view's WITH CHECK OPTION after the trigger; an RLS check stays refused
    if w.checks.iter().any(|c| c.kind != WCOKind::WCO_VIEW_CHECK) {
        crate::refuse!(node, Reason::Bug { payload: "a view MERGE with an RLS check (a view carries none)" });
    }
    if w.targets.len() != 1 || w.targets[0].kind != TargetKind::View || w.targets[0].wholerow_col <= 0 || w.route.is_some() {
        crate::refuse!(node, Reason::Bug { payload: "a view MERGE without its one view target and wholerow column" });
    }
}

/// Pipeline `k`'s view row (the write op at `j`), `None` for a table target.
pub(crate) fn of<'g>(gx: &Gx<'g, '_>, k: usize, j: usize, w: &WriteDesc<'_>, node: i32) -> Option<ViewRow<'g>> {
    if !w.members.has(Member::TargetView) {
        return None;
    }
    let wholerow = w.targets[0].wholerow_col;
    let Some(tf) = crate::ops::modifytable::triggers::projected_type(gx, k, j, wholerow).and_then(|t| typefacts::facts_of(gx.facts, t)) else {
        shape(node, "MERGE into a view: the wholerow column's row type the catalog does not know")
    };
    let typids = (1..=w.site.natts() as i16).map(|a| gx.facts.att(w.plan.target_rel, a).map_or(0, |f| f.typid)).collect_in(gx.mcx);
    Some(ViewRow { wholerow, tf, typids })
}

impl ViewRow<'_> {
    /// The wholerow cell `(datum, valid)`: the old row (ExecMergeMatched's oldtuple); not valid = not matched.
    pub(crate) fn whole(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, at: &RowVals, node: i32) -> (Value, Value) {
        let Some(cr) = at.col(self.wholerow) else { crate::refuse!(node, Reason::Bug { payload: "a view MERGE's wholerow past the child's projection" }) };
        let mut cx = env.lx(b, node);
        let (d, null) = rep::datum(&mut cx, cr, &self.tf);
        env.memo = cx.memo;
        let t = b.bconst(true);
        (d, b.int_bin(IntBinOp::Xor, null, t))
    }

    /// The old row's fields `attnos` (1-based) out of the wholerow datum `img`, `(datum, valid)` each.
    pub(crate) fn fields(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, img: Value, attnos: &[i16]) -> Vec<(Value, Value)> {
        let cols: PgVec<'_, (i16, u32)> = attnos.iter().map(|&a| (a, self.typids[a as usize - 1])).collect_in(env.mcx);
        crate::rerun::epq::copy_row::fields_of(b, env, img, &cols[..])
    }

    /// Whether attno `a` (1-based) is a live column of the view.
    pub(crate) fn live(&self, a: i16) -> bool {
        self.typids.get(a as usize - 1).is_some_and(|&t| t != 0)
    }
}
