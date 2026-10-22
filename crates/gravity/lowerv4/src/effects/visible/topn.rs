//! topn's visible decisions in the effects home (a family sibling file of effects/visible.rs): the functions below
//! moved here unchanged from ops/sort/topn, where each was a lint/EFFECTS-DEBT row (RV). The top over group finals
//! (`runs.topk_groups`), its count cut and its two-pass split ask here whether a group row's way to the top, a final or
//! a fold's row step can raise, and whether an order expression makes a VOLATILE call.
//! - `can_raise` (was topn lower.rs, with `pure`, now a reading of the effects home's one raise-free walk): D449, the top
//!   cut keeps every group row PostgreSQL computes.
//! - `raises` (was topn lower.rs, and count_cut.rs's copy of the same body): D449 / D453 (a), a final's class.
//! - `quiet_step` (was topn strategies/twopass.rs): D452, a fold class whose row step raises on no input; also group's
//!   presorted form (D1596 (f): a native rider beside a presorted DISTINCT).
//! - `fold_quiet`: the two-pass HAVING's reading of a fold whose groups its count floor skips (decision 25): the class
//!   answers above, or the effects home's whole-fold reading (`effects::agg_quiet`).
//! - `order_volatile` (was topn order_slot.rs `volatile`): D1344, a VOLATILE call in an order expression. D2784: it asks
//!   the shared check (`calls`, fail closed, and `effects::volatile`'s operator nodes), no longer FuncExpr / OpExpr alone
//!   with a lookup failure read as not VOLATILE (a fail-open site).
use gravity_decompose::ops::shared::agg_class::Class;
use gravity_decompose::Op;
use types_nodes::Node;

use crate::frame::px::Px;
use crate::graph::Gx;

/// D449: whether a group row's way to the top can raise: a final whose class can (`raises`), or an entry of a HAVING
/// filter or a projection before the sort that is not raise-free (`pure`). PostgreSQL computes all of it for every group
/// before the Sort; the top cut must not skip any.
pub(crate) fn can_raise(px: &Px<'_, '_>, k: usize, g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>) -> bool {
    can_raise_but(px, k, g, None)
}

/// `can_raise`, final `lead` (an aggno) aside, read as raising nothing: 5c-F6's sum lead (topn lower.rs `top_lead`),
/// whose one raise (a numeric sum's OVERFLOW, D299) the two-pass rule admits to pass 2, which takes its final.
pub(crate) fn can_raise_but(px: &Px<'_, '_>, k: usize, g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>, lead: Option<i16>) -> bool {
    let p = &px.gx.pipelines()[k];
    let s = p.ops.iter().position(|op| matches!(op, Op::Sort(_))).unwrap_or(p.ops.len());
    // errorhold (D1344): a volatile order expression the order pass evaluated for every group passing HAVING raised there
    // (order_slot.rs `read_back_cols`; the drain reads its value back): its entry in the projection before the sort is
    // not run again for a raise
    let (passed, near) = (crate::ops::sort::topn::order_slot::read_back_cols(px.gx, g), p.ops[..s].iter().rposition(|op| matches!(op, Op::Project(_))));
    let ran = |i: usize, n: Node<'_>| Some(i) == near && n.as_target_entry().is_some_and(|te| passed.contains(&te.resno));
    g.aggs.iter().any(|a| Some(a.aggno) != lead && raises(a.class)) || p.ops[..s].iter().enumerate().any(|(i, op)| match op {
        Op::Project(l) => !l.iter().all(|n| pure_but(px.gx, g, n, lead) || ran(i, n)),
        Op::Filter(q) => !pure_but(px.gx, g, *q, lead),
        _ => false,
    })
}

/// Whether `n`, over group `g`'s row, raises on no input: the effects home's one walk under its `group_row` reading (the
/// shared leaf set, raise_free.rs `leaf` and `own_call`, with the home's quiet call or `quiet_args`: AGA-17), a final read
/// by its class: one `raises` passes (its arguments ran at the fold, for every row).
fn pure(gx: &crate::graph::Gx<'_, '_>, g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>, n: Node<'_>) -> bool {
    pure_but(gx, g, n, None)
}

/// `pure`, final `lead` (an aggno) read as quiet (`can_raise_but`).
fn pure_but(gx: &crate::graph::Gx<'_, '_>, g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>, n: Node<'_>, lead: Option<i16>) -> bool {
    use crate::effects::raise_free::Reading;
    gx.fx(n).quiet_in(&Reading::group_row(&|a| g.aggs.iter().find(|x| x.node.as_aggref().is_some_and(|b| b.aggno == a.aggno)).is_some_and(|x| Some(x.aggno) == lead || !raises(x.class))))
}

/// D2862 (b): whether entry `n` of the projection before a top's sort over group `g` reads quiet (`pure`): the top
/// pipeline need not run it for a raise, so only the drain does (topn order_slot.rs `kept` keeps the others).
pub(crate) fn quiet_entry(gx: &Gx<'_, '_>, g: &gravity_decompose::ops::shared::agg::GroupDecl<'_>, n: Node<'_>) -> bool {
    pure(gx, g, n)
}

/// Whether a final of class `c` can raise at its final function (D449; D453 (a)). The allowlist is the built classes whose
/// final is a pure read of its slots: counts, integer sums and averages, a float sum (its overflow raises at the row:
/// class_float.rs), min / max, bool and integer bit folds, array_agg. Not in it: a float average (D432: its overflow is
/// deferred to the final, Sxx = +Inf), a numeric sum / average (D299: a sum past the fold's range is the sticky OVERFLOW
/// code every final raises; a wide one's render can fail), every statistic, the M6 folds (interval, money, bit strings:
/// class.other, not built), an ordered set, a user aggregate. The count cut (count_cut.rs) asks it too.
pub(crate) fn raises(c: Class) -> bool {
    !matches!(
        c,
        Class::CountStar | Class::CountAny | Class::SumI64 | Class::SumI128 | Class::SumF32 | Class::SumF64 | Class::AvgI128 | Class::Min(_) | Class::Max(_)
            | Class::BoolAnd | Class::BoolOr | Class::BitI64(_) | Class::Collect(_)
    )
}

/// D452: a class whose row step raises on no input: counts, integer sums and averages (int8 / int128 states), extrema,
/// bool and integer bit folds, integer statistics. Float, money, interval and numeric sums, bit strings, collections,
/// ordered sets and user aggregates may. The two-pass top-N's row pass and group's presorted form (D1596 (f): a native
/// rider folded per row beside an aggpresorted DISTINCT) ask it.
pub(crate) fn quiet_step(c: Class) -> bool {
    matches!(c, Class::CountStar | Class::CountAny | Class::CountPair | Class::SumI64 | Class::SumI128 | Class::AvgI128 | Class::Min(_) | Class::Max(_) | Class::BoolAnd | Class::BoolOr | Class::BitI64(_) | Class::Stat3I128(_))
}

/// Decision 25: whether fold `a` of a group the two-pass HAVING's count floor skips raises nothing PostgreSQL would raise
/// there: with `step`, its row steps (pass 2 folds no row of such a group: `quiet_step`), else its final (the emit takes
/// no final of such a group: `raises`). The class answers, or the effects home's reading of the whole fold
/// (`effects::agg_quiet`: transitions and final raise on no input, which also takes a numeric sum or average over a
/// column with a typmod and a count of distinct values); its arguments and FILTER are the callers'.
pub(crate) fn fold_quiet(a: &gravity_decompose::ops::shared::agg::AggDecl<'_>, step: bool) -> bool {
    (if step { quiet_step(a.class) } else { !raises(a.class) }) || a.node.as_aggref().is_some_and(|x| gravity_decompose::effects::agg_quiet(x))
}

/// D1344 / D2784: whether order expression `e` makes a VOLATILE call: `calls_visible`'s test (a VOLATILE function or
/// operator anywhere in it, one the catalog cannot answer, a cast to a domain whose CHECK makes one, a range function over
/// a user comparator) or an operator node `effects::volatile` reads (D1871: IS DISTINCT FROM, NULLIF, a scalar-array op's
/// functions). Missing one evaluated the expression again past the order pass, so the value shown was not the one sorted.
pub(crate) fn order_volatile(gx: &Gx<'_, '_>, e: Node<'_>) -> bool {
    super::calls(gx, e) || gx.fx(e).volatile_any()
}
