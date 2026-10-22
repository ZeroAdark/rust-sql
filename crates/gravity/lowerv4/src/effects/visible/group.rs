//! group's raise / act questions over a sorted or host-sorted group, in the effects home (the one place that decides
//! volatility and raise-freedom, so a family cannot drift from the shared answer):
//! - `fin_acts` (was regroup.rs): a final that may raise or act (finalize_aggregates runs it after every sorted transition).
//! - `trans_acts` (was regroup.rs, with its helpers `builtin` and `user_sort`): host-sorted transitions run at the group's
//!   final that may raise or act, or whose host sort may call user code.
//! - `folds_ahead`: the host-sorted aggregates an end folds before a group's finals, where the order of a final and a
//!   later aggregate's sorted transitions could be seen.
//!
//! Each phase reads the stricter of two answers: the class list here and the shared reading of that phase (the final:
//! decompose effects `of_final`, the aggregate's own pg_proc row or a user aggregate's finalfn; the transition: the
//! effects home's `still`, visible/agg.rs, which reads support functions through their opfamilies and argument types by
//! `native`). A call either one says may raise or act does.
use gravity_decompose::catalog::expr_type::type_of_listed;
use gravity_decompose::catalog::Facts;
use gravity_decompose::ops::shared::agg_class::{Class, Collect};
use gravity_decompose::catalog::typefacts::FIRST_NORMAL_OID;
use gravity_decompose::ops::shared::agg::{AggDecl, GroupDecl};
use gravity_expr::Catalog;
use mcx::{CollectIn, PgVec};

use crate::graph::Gx;
use crate::ops::shared::agg::sorted;

/// Whether `d`'s final may raise or act (finalize_aggregates runs it after every sorted transition): a user finalfn, an
/// ordered-set or hypothetical final, numeric's (a sum's, an average's, a variance's) over an argument whose digits its
/// types do not bound (effects `agg_quiet` / `num_bounded`, AGC-23), PG 17's
/// interval sum / avg over both infinities, a jsonb object's unique keys, range_agg's over a range not `builtin`; or one the
/// shared final reading (`of_final`) says acts: a VOLATILE final, or an aggregate the catalog has no row for.
pub(crate) fn fin_acts(gx: &Gx<'_, '_>, d: &AggDecl<'_>) -> bool {
    // AGC-23: the argument's integer digits bounded by its types (effects `num_bounded`), not a bare typmod'd column
    let typmod = d.node.as_aggref().and_then(|r| r.args.iter().next()).is_some_and(gravity_decompose::effects::num_bounded);
    gravity_decompose::effects::of_final(gx.facts, d.fnoid).acts || match d.class {
        Class::User => gx.catalog.agg_finalfn(d.fnoid) != Some(0),
        Class::SumNum | Class::AvgNum | Class::Stat3Num(_) => !typmod,
        Class::Collect(Collect::Range) => !builtin(d),
        c => matches!(c, Class::OrderedSet(_) | Class::Hypothetical(_) | Class::SumInterval | Class::AvgInterval | Class::Collect(Collect::JsonObject)),
    }
}

/// Whether host-sorted `d`'s transitions (run at the group's final) may raise or act: all but counts, integer and numeric
/// sums and averages, extrema, bool and integer bit folds, the array, string, xml (xmlconcat2 only allocates, as
/// string_agg's transition), range_agg and any_value collects, and
/// range_intersect_agg over a `builtin` range (a user range type's subtype comparator and canonical function are user
/// code); with `cmp`, any of them whose host sort may (`user_sort`). A transition the shared reading (`still_in`) does not
/// call still acts too: range_agg over a type that is not `native` or with a support function that is not built in.
pub(crate) fn trans_acts(f: &dyn Facts, cat: &dyn Catalog<'_>, d: &AggDecl<'_>, cmp: bool) -> bool {
    use Class as C;
    let quiet = matches!(d.class, C::CountStar | C::CountAny | C::CountPair | C::SumI64 | C::SumI128 | C::SumNum | C::AvgI128 | C::AvgNum | C::Min(_) | C::Max(_) | C::BoolAnd | C::BoolOr | C::BitI64(_) | C::Collect(Collect::Array | Collect::String | Collect::Xml | Collect::Range | Collect::AnyValue));
    !(quiet || d.class == C::Collect(Collect::RangeIntersect) && builtin(d)) || !super::agg::still_in(cat, d) || cmp && sorted(d) && user_sort(f, cat, d)
}

/// D2824 (where PostgreSQL raises and Gravity raises another real error, the statement passes: error versus error):
/// whether some aggregate of group `g` may ACT, not merely raise: an argument, direct argument or
/// FILTER that calls user code or a VOLATILE function in whole scope (`subplan::calls_whole`, D4138: a SubPlan's plan,
/// test and args too), a user-defined aggregate (its transfn,
/// finalfn), an argument type that is not built in (its I/O, compare and support functions may be user code) and not
/// `type_free` either (AGC-22: an enum, a domain over a free type, an array or a composite of them, a table's row type:
/// initdb's procs), a host sort through a user comparator (`user_sort`) or a final the shared reading says acts
/// (`of_final`: VOLATILE). Where none acts, a plain group's per-row work (every argument and FILTER over every input row)
/// and its host-sorted transitions (every input row, at the final, in the ORDER BY's order) are the evaluations
/// PostgreSQL makes per row,
/// so the statement raises in Gravity exactly when it raises in PG; only which error raises first moves, which the
/// ruling frees. A NOTICE, a write, a sequence value or a FILTER reading what a transition changed (D2824's 55000:
/// currval after a transition's nextval) would show PG's per-row order, so each of those keeps the refusal.
pub(crate) fn acts(gx: &Gx<'_, '_>, g: &GroupDecl<'_>) -> bool {
    let calls = |n: types_nodes::Node<'_>| super::subplan::calls_whole(gx, n);
    g.aggs.iter().any(|d| {
        let reads = d.node.as_aggref().is_none_or(|r| r.args.iter().chain(r.aggdirectargs.iter()).chain(r.aggfilter).any(calls));
        let free = d.node.as_aggref().is_some_and(|r| r.aggargtypes.iter().all(|t| t < FIRST_NORMAL_OID || crate::effects::type_free(gx.catalog, t, 0)));
        let user = d.class == Class::User || d.fnoid >= FIRST_NORMAL_OID || !free;
        reads || d.filter.is_some_and(calls) || user || sorted(d) && user_sort(gx.facts, gx.catalog, d) || gravity_decompose::effects::of_final(gx.facts, d.fnoid).acts
    })
}

/// Every argument type of `d` is built in (below FirstNormalObjectId, access/transam.h).
pub(crate) fn builtin(d: &AggDecl<'_>) -> bool { d.node.as_aggref().is_some_and(|r| r.aggargtypes.iter().all(|t| t < FIRST_NORMAL_OID)) }

/// Whether `d`'s DISTINCT / ORDER BY sort may call user code: a user operator outside its type's default btree family
/// (USING: its opfamily's comparator; `effects::user_order`), or a column whose type's default comparator is not
/// built-in code (the effects home's `cmp_free`, not a type list): a user base type's, a user range's subtype
/// one, an anonymous record's or a record array's (their columns are named per row), a composite's or array's whose
/// component's is. A domain sorts by its base type's comparator (the planner's sortop is the base's, e.g.
/// count(DISTINCT dom) over a domain of int4); an enum by enum_cmp, a named composite of built-in columns by btrecordcmp.
/// A ROW(..) constructor's anonymous record names its columns in the expression (D3374): btrecordcmp compares them by
/// their own types' default comparators, so it sorts by built-in code when each of those types does.
fn user_sort(f: &dyn Facts, cat: &dyn Catalog<'_>, d: &AggDecl<'_>) -> bool {
    let Some(r) = d.node.as_aggref() else { return true };
    let free = |e: types_nodes::Node<'_>| match e.as_row_expr() {
        Some(x) => x.args.iter().all(|a| type_of_listed(a).is_some_and(|t| crate::effects::cmp_free(cat, t))),
        None => type_of_listed(e).is_some_and(|t| crate::effects::cmp_free(cat, t)),
    };
    let col = |i: u32| r.args.iter().filter_map(|t| t.as_target_entry()).find(|t| t.ressortgroupref == i).is_none_or(|t| !free(t.expr));
    r.aggorder.iter().chain(r.aggdistinct.iter()).any(|n| n.as_sort_group_clause().is_none_or(|c| gravity_decompose::effects::user_order(f, c.sortop) || col(c.tleSortGroupRef)))
}

/// D1371: group `g`'s host-sorted aggregates (indices into `g.aggs`) an end folds ahead of the group's finals, each
/// transition state once, in PG's transno order (the Aggref's `aggtransno`), where a final that may raise or act
/// (`fin_acts`) comes before a host-sorted aggregate whose transitions may (`trans_acts`) in aggno order: PG's
/// finalize_aggregates runs every sorted state's process_ordered_aggregate_* before any finalfn, and a host-sorted
/// aggregate folds at its own final otherwise (an earlier final would raise or act before the later one's transitions).
/// A shared state (V4R-424) folds once, under its opener (the first aggregate on it). Empty where the order cannot be
/// seen: every program without the condition keeps its finals as they are.
pub(crate) fn folds_ahead<'g>(gx: &Gx<'g, '_>, g: &GroupDecl<'_>) -> PgVec<'g, usize> {
    let tno = |d: &AggDecl<'_>| d.node.as_aggref().map_or(-1, |r| r.aggtransno);
    let late = |i: usize| g.aggs[i + 1..].iter().any(|d| sorted(d) && trans_acts(gx.facts, gx.catalog, d, true) && (tno(d) < 0 || g.aggs[..=i].iter().all(|e| tno(e) != tno(d))));
    if !g.aggs.iter().enumerate().any(|(i, d)| fin_acts(gx, d) && late(i)) {
        return PgVec::new_in(gx.mcx);
    }
    let mut ahead: PgVec<'g, usize> = (0..g.aggs.len()).filter(|&i| sorted(&g.aggs[i])).collect_in(gx.mcx);
    ahead.sort_by_key(|&i| (tno(&g.aggs[i]) < 0, tno(&g.aggs[i]), i));
    ahead.dedup_by_key(|i| match tno(&g.aggs[*i]) { t if t < 0 => -1 - *i as i32, t => t });
    ahead
}
