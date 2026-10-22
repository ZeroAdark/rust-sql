//! The agg family's planning-half decisions in the effects home, moved unchanged from ops/agg/{gsets,keys,mod}.rs: whether a
//! group's lists act once per group (D2249) or in an aggregate's per-row inputs (A8), whether they fold a user aggregate
//! (K4.riderg, `user_folds`), whether PostgreSQL's hash raises on
//! a group key (D1292), and whether a group key's equality operator is a user one (map_set's USER_GROUP_EQ refusal).
use types_nodes::list::NodeList;
use types_nodes::{Node, NodeTag};

use crate::expr;
use crate::catalog::typefacts;

use super::Cx;

/// D2249: whether `lists` (a group's target list and HAVING) call code that acts once per group, read by the effects
/// home (a catalog miss acts). An aggregate call is not read: its arguments and FILTER are `volatile_inputs`' question,
/// and its transition and final functions run inside its own fold, which the aggregate's form orders (D1323 / D1371).
pub(crate) fn per_group_acts<'p>(cx: &Cx<'_, 'p>, lists: &[&'p NodeList<'p>]) -> bool {
    let mut hit = false;
    for e in lists.iter().flat_map(|l| l.iter()) {
        expr::walk(e, &mut |x| {
            if hit || x.as_aggref().is_some() {
                return false;
            }
            let mut folds = false;
            expr::walk(x, &mut |y| {
                folds |= y.as_aggref().is_some();
                !folds
            });
            if !folds {
                hit = crate::effects::of_expr(cx, x, &crate::effects::PLAIN).acts;
                return false;
            }
            // a call over an aggregate's result: its own function (unknown acts); its arguments are walked next. D4146: a
            // SubPlan over one (the aggregate passed as its argument) runs its plan per group: the plan's calls are the
            // SubPlan's own (`of_node`: a SubPlan counts its plan, its arguments apart)
            if matches!(x.node_tag(), NodeTag::T_SubPlan) {
                hit = super::read::of_node(cx, x).acts;
                return !hit;
            }
            let own = match x.node_tag() {
                NodeTag::T_FuncExpr => Some(x.as_func_expr().unwrap().funcid),
                NodeTag::T_OpExpr => Some(x.as_op_expr().unwrap().opfuncid),
                NodeTag::T_DistinctExpr => Some(x.as_distinct_expr().unwrap().opfuncid),
                NodeTag::T_NullIfExpr => Some(x.as_null_if_expr().unwrap().opfuncid),
                _ => None,
            };
            hit = own.is_some_and(|f| crate::effects::of_call(cx.cat, f).acts);
            !hit
        });
    }
    hit
}

/// Whether an aggregate of `lists` runs code that acts on its rows (an argument or its FILTER), read by the effects home
/// (A8: a VOLATILE call or a catalog miss acts).
pub(crate) fn volatile_inputs<'p>(cx: &Cx<'_, 'p>, lists: &[&'p NodeList<'p>]) -> bool {
    let mut hit = false;
    for e in lists.iter().flat_map(|l| l.iter()) {
        expr::walk(e, &mut |x| {
            if x.as_aggref().is_some() {
                hit |= agg_inputs(x).any(|i| crate::effects::of_expr(cx, i, &crate::effects::PLAIN).acts);
                return false;
            }
            !hit
        });
    }
    hit
}

/// D1371 (D3374): whether a sorted GroupAggregate whose `lists` (its target list and HAVING) are these streams so that
/// each group is handed on as PostgreSQL hands it on. That holds when the lists have an ORDER BY call whose aggregate
/// acts (a user transfn's NOTICE or sequence call), whose sort the host runs at the group's final or, aggpresorted, per
/// row.
/// PostgreSQL's GroupAggregate runs a group's sorted transitions and finals before it advances the next group's first
/// row. The keyed end folds every row's work before its first group leaves, so the group's NOTICEs, sequence values and
/// first error would come in another order (lowerv4 regroup.rs `call_order` refuses it there). The stream (D1071's form)
/// keeps PostgreSQL's order: lowerv4 runs a presorted call per row (effects `ordered_fed_acting`), and any other call
/// at the group's handoff. Only calls the stream builds qualify:
/// - (v0.4 refusals) a DISTINCT call streams too (D1120's per-group set, or lowerv4 regroup.rs `redeclare_stream`'s
///   host-sorted fold at the group's handoff, where PostgreSQL's finalize sorts and folds one it did not presort);
/// - (v0.4 refusals) an ORDER BY compared by a user comparator (`builtin` false) is an acting call: the stream sorts the
///   group at its handoff, where PostgreSQL's finalize sorts it (which pairs the comparator meets is the sort's own
///   choice, free under USEROP K10.ao).
/// An ordered-set call's WITHIN GROUP is the aggregate's own order (`user_folds`' rule), so it counts as other work.
/// D1371 (v0.4 refusals): every such call streams, beside quiet per-row work too: its calls run at each group's final,
/// so the order the groups are finalized in shows (a sequence value, a NOTICE), and the keyed end finalizes its groups
/// in its table's order, not PostgreSQL's key order (m6_d1371_user_ordered's `ORDER BY 2 DESC` cursor).
pub(crate) fn sorted_calls_stream<'p>(cx: &Cx<'_, 'p>, lists: &[&'p NodeList<'p>], builtin: &dyn Fn(&types_nodes::primnodes::Aggref<'p>) -> bool) -> bool {
    use crate::effects::of_agg;
    let mut sorted = false;
    for e in lists.iter().flat_map(|l| l.iter()) {
        expr::walk(e, &mut |x| {
            let Some(r) = x.as_aggref() else { return true };
            let ordered = (!r.aggorder.is_nil() || !r.aggdistinct.is_nil()) && r.aggkind == types_nodes::primnodes::AGGKIND_NORMAL;
            // D1371 (v0.4 refusals): a sort by a user comparator acts too (its calls, NOTICEs and error); the stream runs
            // it at the group's handoff, where PostgreSQL's finalize runs it (which pairs it meets is the sort's own
            // choice, free under USEROP K10.ao); so does a user-defined aggregate's (CREATE AGGREGATE over an internal
            // transfn: lowerv4 reads its calls as acting, visible/group.rs `trans_acts`)
            let user = crate::effects::agg_calls(cx.cat, r.aggfnoid) != crate::effects::AggCalls::Own;
            sorted |= ordered && (of_agg(cx.cat, r.aggfnoid).acts || user || !builtin(r));
            false
        });
    }
    sorted
}

/// M2 kit K4.riderg: whether an aggregate of `lists` is a user-defined one (CREATE AGGREGATE's: its transition and final
/// functions are user code, `effects::agg_calls`) called with no DISTINCT and no ORDER BY. An ordered-set call's WITHIN
/// GROUP is no sort PostgreSQL runs (nodeAgg hands it the rows in input order, the aggregate orders them itself:
/// agg_udagg-0008), so it folds as a plain one. A sorted group holding one streams (D1071's form, PostgreSQL's): its
/// transition runs per row in the groups' key order, each group finalized as it is handed on before the next group's
/// first row is advanced, and stops where a consumer above stops, whatever the user code does (a VOLATILE transfn's
/// calls, a raise past the stop); the fold-ahead form would run every group's transitions first, in input order. Within a
/// group the rows' order is free (D1428: with no aggregate ORDER BY every row of a group is a tie), so the stream's own
/// sort order serves.
pub(crate) fn user_folds<'p>(cx: &Cx<'_, 'p>, lists: &[&'p NodeList<'p>]) -> bool {
    let mut hit = false;
    for e in lists.iter().flat_map(|l| l.iter()) {
        expr::walk(e, &mut |x| {
            if let Some(r) = x.as_aggref() {
                let sorts = !r.aggorder.is_nil() && r.aggkind == types_nodes::primnodes::AGGKIND_NORMAL;
                hit |= r.aggdistinct.is_nil() && !sorts && crate::effects::agg_calls(cx.cat, r.aggfnoid) != crate::effects::AggCalls::Own;
                return false;
            }
            !hit
        });
    }
    hit
}

/// An aggregate's per-row inputs: its arguments and FILTER (direct arguments are per group).
pub(crate) fn agg_inputs<'p>(r: Node<'p>) -> impl Iterator<Item = Node<'p>> {
    let ag = r.as_aggref().expect("an aggregate");
    ag.args.iter().filter_map(|t| t.as_target_entry().map(|t| t.expr)).chain(ag.aggfilter)
}

/// D1292 (agg A14 d): a hashed group's key of `typid` under collation `coll` that PG's hash raises 42P22 on ("could not
/// determine which collation to use for string hashing", `hashtext` / `hashbpchar`'s check) at the first row whose key is
/// not NULL. The group keeps the key under collation 0 and its end raises there (lowerv4 ops/agg/hash_raise.rs).
/// The keys it names: a collatable varlena string image (text, varchar, bpchar, a domain over one), whose hash is
/// `hashtext` / `hashbpchar`; `name` (fixed width) hashes its bytes under any collation (`hashname`).
pub fn hash_raises(facts: &dyn crate::catalog::Facts, typid: u32, coll: u32) -> bool {
    typefacts::facts_of(facts, typid).is_some_and(|t| hash_raises_of(&t, coll))
}

/// `hash_raises` over the key type's facts `t`.
pub fn hash_raises_of(t: &typefacts::TypeFacts, coll: u32) -> bool {
    coll == 0 && t.collatable && t.typlen == -1 && t.image.is_text()
}

/// A group key's equality operator `op` is a user one (past FirstNormalObjectId: CREATE OPERATOR's), whose procedure
/// map_set's group forms call only where they stream or run (`USER_GROUP_EQ` / `USER_GROUP_OP` otherwise).
pub(crate) fn user_op(op: u32) -> bool {
    op >= types_core::FirstNormalObjectId
}
