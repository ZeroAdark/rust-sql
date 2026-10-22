//! The agg family's lowering-half decisions in the effects home, moved unchanged from ops/agg: whether a
//! walked group's fold raises past its first key (walk_skip.rs, D449); whether a hosted aggregate in a sorted group reads
//! its rows' order (trans/hosted.rs, #530, with the comparator and type tests it reads); whether a call's ORDER BY can
//! change its answer (trans/args.rs, D458); and whether a finals.cse entry can raise (trans/strategies/finals_cse.rs).
use gravity_decompose::ops::shared::agg_class::{Class, Collect as C};
use gravity_decompose::ops::shared::agg::{AggDecl, GroupForm};
use gravity_decompose::catalog::typefacts::FIRST_NORMAL_OID;
use gravity_expr::Catalog;
use types_nodes::primnodes::Aggref;
use types_nodes::{Node, NodeTag};

use crate::face::kinds::KindId;
use crate::frame::px::Px;
use crate::graph::Gx;
use crate::table::rec::{Arg, GroupRec};

/// #530 (trans/hosted.rs `still`, group/regroup.rs D2824's `per_row`): whether PG runs no user code for `d` and its
/// transition raises on no row (its final runs per group at the emit, ahead of a cursor's pull): the classes listed, less
/// arrays of arrays, jsonb_agg over json (it parses the text again per row; json_agg appends it as is) or JSON over a
/// record (an output function), and a range or WITHIN GROUP one over a record, or a type not built in, or an ORDER BY /
/// DISTINCT whose comparison or equality support function is not built in (user comparators): the guard's one reader,
/// `effects::support_of`, resolves them through the sort operator's opfamily as PostgreSQL's executor does, not by the
/// operator's OID (aggtrans T5).
pub(crate) fn still(gx: &Gx<'_, '_>, d: &AggDecl<'_>) -> bool {
    still_in(gx.catalog, d)
}

/// `still` over catalog `cat` alone (group's regroup overlay asks it before the graph's `Gx` exists). xmlagg's transition,
/// xmlconcat2 (PG 18.6 xml.c `xmlconcat`: text_to_cstring, parse_xml_decl with its result ignored, appends), raises on no
/// row but the allocation ceiling, string_agg's class; its one argument type is xml. any_value's, any_value_transfn (PG
/// 18.6 misc.c: it returns its state; strict, so the first non-NULL argument is copied in as the state), calls no code of
/// the argument's type and raises on no row, array_agg's class (D3374).
pub(crate) fn still_in(cat: &dyn Catalog<'_>, d: &AggDecl<'_>) -> bool {
    use Class as K;
    // AGC-22: an argument type past FirstNormalObjectId whose output, comparison and hash procs are initdb's (`type_free`:
    // an enum, a domain over a free type, an array or a composite of them, e.g. a table's row type) collects as a builtin's
    let free = |t: u32| native(cat, t) || t >= FIRST_NORMAL_OID && crate::effects::type_free(cat, t, 0);
    let builtin = |r: &Aggref<'_>, json: bool| crate::effects::support_in(cat, d.node).is_ok() && r.aggargtypes.iter().all(|t| free(t) && !(json && crate::effects::json_text(cat, t)));
    match d.class {
        c @ (K::Collect(C::Json | C::Range | C::RangeIntersect) | K::OrderedSet(_) | K::Hypothetical(_)) => d.node.as_aggref().is_some_and(|r| builtin(r, c == K::Collect(C::Json) && crate::effects::json_reparsed(r))),
        c => matches!(c, K::CountStar | K::CountAny | K::CountPair | K::SumI64 | K::SumI128 | K::SumNum | K::AvgI128 | K::AvgNum | K::Min(_) | K::Max(_) | K::BoolAnd | K::BoolOr | K::BitI64(_) | K::Stat3I128(_) | K::Stat3Num(_) | K::Collect(C::Array | C::String | C::Xml | C::AnyValue)),
    }
}

/// D449 past the first key (walk_skip.rs `plan`): every key past the first a column or an expression `quiet_past`, every
/// aggregate `walk_folds`, and none of the group forms whose rows fold in another shape.
pub(crate) fn raise_free(gx: &Gx<'_, '_>, rec: &GroupRec<'_, '_>) -> bool {
    let g = rec.g;
    let keys = rec.ksrc.iter().zip(g.keys.iter()).all(|(s, k)| matches!(s, Some(Arg::Col(_))) || quiet_past(gx, k.expr.node()));
    let aggs = rec.aggs.iter().all(|a| (a.filter.is_none() || a.d.filter.is_some()) && walk_folds(gx, &a.d));
    keys && aggs && g.form != GroupForm::Distinct && g.two_pass.is_none() && g.set.is_none() && rec.nums.is_none()
}

/// D449 / D554: aggregate `d` folds a row a walk skips (walk_skip.rs, sort/strategies/walk.rs `skip_safe`) as PG does
/// none: no DISTINCT / ORDER BY / direct argument, a class whose row fold cannot raise (`folds`), and every argument and
/// its FILTER `quiet_past`.
pub(crate) fn walk_folds(gx: &Gx<'_, '_>, d: &AggDecl<'_>) -> bool {
    let m = d.mods;
    let args = || d.node.as_aggref().is_some_and(|r| r.args.iter().all(|t| t.as_target_entry().is_some_and(|t| quiet_past(gx, t.expr))));
    m.distinct == 0 && m.order == 0 && m.direct == 0 && folds(d.class) && d.filter.is_none_or(|f| quiet_past(gx, f)) && args()
}

/// An expression a skipped row may leave unevaluated: it raises on no row (the home's `raise_free`) and calls no user or
/// VOLATILE code (`user_or_volatile`, `user_typed`), which PG runs on every row it reaches.
fn quiet_past(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    gx.fx(n).quiet() && !gx.fx(n).user_or_volatile() && !gx.fx(n).user_typed()
}

/// The classes whose row fold cannot raise: counts, integer sums and averages, min / max, bool folds and integer bit
/// folds. That is runs/topk_groups.rs `raises`'s allowlist without the float sums (their overflow raises at the row,
/// class_float.rs) and without array_agg.
fn folds(c: Class) -> bool {
    matches!(c, Class::CountStar | Class::CountAny | Class::SumI64 | Class::SumI128 | Class::AvgI128 | Class::Min(_) | Class::Max(_) | Class::BoolAnd | Class::BoolOr | Class::BitI64(_))
}

/// #530 (trans/hosted.rs): whether hosted `d` folds a sorted group's rows in input order to PG's value but for its
/// elements' order (D1428: with no ORDER BY in the call every row of a group is a tie): an ordered-set / hypothetical one
/// (it sorts its own input, D1375), numeric sums, averages and exact variances, any extremum (a built-in comparator; equal
/// members, numeric 1.0 / 1.00 or an enum's, are any input member's), a collect, a float8 state
/// (a float aggregate may sum in any order), any_value (D3374: its pick is free, any non-NULL member of its
/// group, the judge masking it; it is NULL iff every member is, in any order). User-defined states, xmlagg,
/// json_object_agg, money / interval sums and an extremum over a user type (its comparator is user code PG calls in its
/// sort's order, an array of a user range's say) read the order. A float8 state, any_value and an extremum but a text /
/// bytea one (v4's; aggtrans T7a / T7c free the others) are sort-free only while the call's inputs do not act (a VOLATILE
/// call: the effects home) beside a column (corr(nextval(..), k)): its rows take the call's values in input order, whose
/// ties are PG's sort output order; alone (max(nextval(..))) the group takes the same values in any order. v4's classes
/// keep their served forms (D1071). D3744: in a streamed group (`streamed`: decompose keeps the Sort of a group whose
/// argument or FILTER calls something VOLATILE, D1069, and folds its rows group by group as they come, D1071) the rows
/// take the calls' values in the Sort's order, PG's but for ties (free: D1428, DECISIONS 6b), so such a call beside a
/// column (max(EXTRACT(EPOCH FROM clock_timestamp() - xact_start)), the Datadog check's) folds to a value PG's order
/// gives, as a streamed group's user aggregate does (trans/hosted.rs `keyed_slots`).
pub(crate) fn sort_free(px: &Px<'_, '_>, d: &AggDecl<'_>, streamed: bool) -> bool {
    let gx = px.gx;
    let exact = matches!(d.class, Class::SumNum | Class::AvgNum | Class::Stat3I128(_) | Class::Stat3Num(_) | Class::OrderedSet(_) | Class::Hypothetical(_));
    let float = matches!(d.class, Class::Stat3F64(_) | Class::Stat6F64(_));
    let extremum = matches!(d.class, Class::Min(_) | Class::Max(_)) && d.node.as_aggref().is_some_and(|r| crate::effects::arg_types(r).all(|t| builtin_order(gx, t)));
    let text = crate::props::class_kind(d) == KindId::ClassText;
    let any = d.class == Class::Collect(C::AnyValue);
    let quiet = || streamed || !d.node.as_aggref().is_some_and(|r| acts_beside_column(px, r));
    exact || collect(d) || (extremum && text) || ((float || extremum || any || crate::graph::one_image_extremum(d)) && quiet())
}

/// `sort_free`: whether call `r`'s arguments and FILTER act (the effects home: a VOLATILE call, a SubPlan or a function
/// the catalog cannot answer) and read a column of the row.
fn acts_beside_column(px: &Px<'_, '_>, r: &Aggref<'_>) -> bool {
    use gravity_decompose::effects;
    let column = r.args.iter().chain(r.aggfilter).any(|n| px.gx.fx(n).reads_column());
    let cx = effects::Cx::without_stmt(px.gx.facts, px.arena);
    column && r.args.iter().chain(r.aggfilter).any(|n| px.gx.fx(n).acts_plain(&cx))
}

/// #530 (trans/hosted.rs): whether type `t` orders by a built-in comparator: a built-in type but an `effects::rowless`
/// one (`native`), an enum, or a domain over one; aggtrans T6f: any type whose comparator and its components are built in
/// (`effects::cmp_free`: a composite or array of them).
pub(crate) fn builtin_order(gx: &Gx<'_, '_>, t: u32) -> bool {
    let built_in = |t: u32| native(gx.catalog, t);
    built_in(t) || gx.facts.type_row(t).is_some_and(|r| built_in(r.base) || gx.facts.type_row(r.base).is_some_and(|b| b.typtype == b'e')) || crate::effects::cmp_free(gx.catalog, t)
}

/// #530 (trans/hosted.rs `builtin_order` and `still`): type `t` is a built-in type (below FIRST_NORMAL_OID) but no
/// `effects::rowless` one.
pub(crate) fn native(c: &dyn Catalog<'_>, t: u32) -> bool {
    t < FIRST_NORMAL_OID && !crate::effects::rowless(c, t)
}

/// #530 (trans/hosted.rs): the array, string, JSON array, JSON object and range collects (a sorted keyed end takes them
/// under `quiet_rows`). A JSON object reads its rows' order as a JSON array does (json keeps every pair in input order,
/// jsonb the last of a key's pairs): PG's sort output order, ties free (D1428); its transition raises (a NULL key, WITH
/// UNIQUE's duplicate), so it is not `still` and folds ahead only under the licence (`effects::fold_ahead`).
pub(crate) fn collect(d: &AggDecl<'_>) -> bool {
    matches!(d.class, Class::Collect(C::Array | C::ArrayOfArrays | C::String | C::Json | C::JsonObject | C::Range | C::RangeIntersect))
}

/// D458 (V4R-178; trans/args.rs): an ORDER BY inside the call that cannot change the answer, so the call folds as if it
/// had none: the call without its ORDER BY is order-free by the one rule (`facts::order::order_free`: counts, integer and
/// numeric sums, averages and variances, min / max of every type, the bool and integer bit folds, ordered-set calls;
/// float, money and interval folds keep the order, their overflow by partial sums raising in PG's order only), and every
/// ORDER BY entry of its own is a bare column or constant (nothing that raises, D449, and nothing volatile).
/// aggregates#251, `evaluated`: the entries of any form (`sum(x ORDER BY two, random())`): the keyed end folds the call
/// unordered and evaluates each entry that is not a bare column or constant for every row the call reads, dropping its
/// value (table/args.rs `compile_args_at`, table/rec.rs `discard`), as PostgreSQL evaluates them as transition inputs
/// (ExecBuildAggTrans, past the FILTER) for a sort whose order the class's answer never shows: an entry raises on the
/// rows PostgreSQL's does, a VOLATILE one is called per row. The plain fold does the same (trans/args.rs `discards`).
pub(crate) fn order_free(a: &AggDecl<'_>, evaluated: bool) -> bool {
    a.node.as_aggref().is_some_and(|r| order_free_ref(r, evaluated))
}

/// `order_free` over the call itself (its catalog class), as a projection's compile sees it; `any`: its ORDER BY entries
/// of any form (`evaluated`).
pub(crate) fn order_free_ref(r: &types_nodes::primnodes::Aggref<'_>, any: bool) -> bool {
    use gravity_decompose::ops::shared::agg_class::{Mods, Split};
    if r.aggkind != types_nodes::primnodes::AGGKIND_NORMAL {
        return r.aggdistinct.is_nil(); // V4R-425: WITHIN GROUP input is sorted by the call itself, a user one's too (no table row)
    }
    let class = gravity_decompose::ops::agg::trans::table::row(r.aggfnoid).map_or(Class::User, |w| w.class);
    let mods = Mods { distinct: r.aggdistinct.len() as u8, order: 0, filter: false, direct: r.aggdirectargs.len() as u8, split: Split::of(r.aggsplit), star: r.aggstar };
    let bare = |t: &types_nodes::primnodes::TargetEntry<'_>| !t.resjunk || t.expr.as_var().is_some() || t.expr.as_const().is_some();
    gravity_decompose::facts::order::order_free(class, &mods) && (any || r.args.iter().all(|t| t.as_target_entry().is_none_or(bare)))
}

/// finals.cse (trans/strategies/finals_cse.rs): a bare Aggref, Var or Const (through RelabelType): an entry that cannot
/// raise.
pub(crate) fn quiet(e: Node<'_>) -> bool {
    crate::stack_check();
    matches!(e.node_tag(), NodeTag::T_Aggref | NodeTag::T_Var | NodeTag::T_Const) || e.as_relabel_type().is_some_and(|r| quiet(r.arg))
}

/// D1108 (rerun/spool/form.rs `first_pull`): aggregate `d`'s final raises nothing PostgreSQL's would not raise at the
/// Material's first fetch (PostgreSQL runs it per group as the Agg hands the group on): a class whose final reads its slots
/// (topn.rs `raises`: counts, integer and float sums, integer averages, min / max, boolean and integer bit folds), no
/// collecting one; or a float average, whose overflow D432 defers to the final, raised inside the whole fill, where
/// PostgreSQL's transition raises it at that first fetch. Each finalized by its slot's rendering.
pub(crate) fn final_quiet(d: &AggDecl<'_>) -> bool {
    use gravity_decompose::ops::shared::agg::FinalForm as F;
    let class = !crate::effects::visible::topn::raises(d.class) && !matches!(d.class, Class::Collect(_)) || matches!(d.class, Class::AvgF64);
    let final_ = matches!(d.final_, F::Identity | F::Int128Sum | F::Int128Avg | F::F64Avg | F::Rendered | F::Numeric | F::NumericAvg | F::F64Ordered | F::Bool | F::F32);
    class && final_
}

/// K4.riderg: whether record `rec` folds a user-defined aggregate (CREATE AGGREGATE's transition and final are user code
/// that may act or raise): a streamed group of one caps a pull (D439, ops/group/stream.rs).
pub(crate) fn folds_user(rec: &GroupRec<'_, '_>) -> bool {
    rec.aggs.iter().any(|a| a.d.class == Class::User)
}
