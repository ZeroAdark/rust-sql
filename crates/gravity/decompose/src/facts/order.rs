//! One order-free rule (specs/semantics.md design fix 8): whether an aggregate call folds to a result PostgreSQL accepts
//! in any input row order (`order_free`). The other copies (lowerv4 graph.rs `order_free`, ops/agg/trans/args.rs
//! `order_free_ref`, ops/hashjoin/drain/spool_admit.rs `order_free_class`, `Class::order_sensitive`) adopt it in turn.
//! D612's absorb (a sorted group drops its key Sort: ops/group/absorb.rs) and the stop road's kept Sort (facts/stream.rs)
//! read it too, as `absorb_free`: the same rule, less the classes whose result can still show the Sort's order
//! (`absorb_keeps`).
//!
//! Under the rulings:
//! - floats in any order (the user's ruling 3, 10-01: a float aggregate may sum in any order; DECISIONS.md 11 (d)): a
//!   float4 / float8 sum, average or variance is free, and so are money and interval sums and averages (their values are
//!   exact in any order);
//! - any type-equal member for min / max (ruling of 09-28, extended 09-29): min / max of every type are order-free, a
//!   float's -0 / 0, a numeric's 1.0 / 1.00, bpchar's trailing blanks and a nondeterministic collation's equal strings
//!   included;
//! - an error only where PostgreSQL raises (DECISIONS.md 4 (D)): the overflow of those folds is a raise by partial sums
//!   (float8pl, float8_accum, cash_pl, interval_pl raise when a finite pair overflows: 1e308 + 1e308 - 1e308 raises in
//!   that order only), so they are free only where a raise re-runs the statement in plan order under errorhold's licence
//!   (`effects::licence`, `Departure::Ahead`, recheck), and in that plan-order run (`effects::denying`) or a statement
//!   that acts (no re-run can repeat it: `lowering`) they keep the order; bit strings' length mismatch keeps it too;
//! - D2597: a float fold out of PostgreSQL's order raises (`rechecked`) wherever a finite input is large enough that some
//!   order would overflow, a NaN met first or not, so the plan-order run answers: NaN where PostgreSQL meets the NaN
//!   first, PostgreSQL's overflow otherwise (the order / multiset ruling names overflow vs NaN WRONG);
//! - range_agg's multirange is canonical (sorted and merged by its range type's comparator), the same in any input order.
//!
//! Collections (array / string / json / xml), user aggregates, and a normal call with its own ORDER BY or DISTINCT keep
//! the order (an ORDER BY's ties are broken by input order). Ordered-set and hypothetical-set aggregates sort their own
//! input (WITHIN GROUP), so they are free unless DISTINCT. A split call (partial / final) is not decided here.
use core::cell::Cell;

use types_nodes::plannodes::Agg;
use types_nodes::primnodes::Aggref;
use types_nodes::Node;

use crate::ops::shared::agg_class::{Class, Collect, Ext, Mods, Split};
use crate::walk::Builder;

/// A call's modifiers aside: no DISTINCT, ORDER BY, FILTER or direct argument, not split.
const SIMPLE: Mods = Mods { distinct: 0, order: 0, filter: false, direct: 0, split: Split::Simple, star: false };

/// PM2: whether a sort or DISTINCT clause's ordering operator (`SortGroupClause.sortop`) orders by its input type's
/// default btree family (`Facts::op_dir`). The type's own comparator then runs (its calls are `effects::of_type`'s), and
/// the clause's `eqop` is that family's equality, so equal under it is equal under the type's order. No sortop (a hashed
/// DISTINCT) and any other family's operator (USING a user operator, a non-default opclass) are not.
pub fn default_order(f: &dyn crate::catalog::Facts, sortop: u32) -> bool {
    sortop != 0 && f.key(crate::catalog::KeyReq::sort(sortop)).is_some_and(|o| o.default_family)
}

/// Whether call class `c` with modifiers `m` folds to a result PostgreSQL accepts in any input order.
pub fn order_free(c: Class, m: &Mods) -> bool {
    if m.distinct != 0 || m.split != Split::Simple {
        return false;
    }
    match c {
        Class::OrderedSet(_) | Class::Hypothetical(_) => true,
        _ if m.order != 0 => false,
        Class::Collect(Collect::Range) => true,
        Class::Collect(_) | Class::User => false,
        Class::SumF32 | Class::SumF64 | Class::AvgF64 | Class::Stat3F64(_) | Class::Stat6F64(_) | Class::SumMoney | Class::SumInterval | Class::AvgInterval => !plan_order(),
        // a bit string AND / OR / XOR: any order folds the same string, and raises (bit strings of different sizes) iff two
        // lengths differ; only where it raises beside another row's error is the input order's (`raises_by_order`)
        Class::BitVar(_) => !plan_order(),
        Class::CountStar | Class::CountAny | Class::CountPair => true,
        Class::SumI64 | Class::SumI128 | Class::SumNum => true,
        Class::AvgI128 | Class::AvgNum => true,
        Class::Stat3I128(_) | Class::Stat3Num(_) => true,
        Class::Min(_) | Class::Max(_) | Class::BoolAnd | Class::BoolOr | Class::BitI64(_) => true,
    }
}

/// D612's absorb reading of `order_free`: whether call class `c`, its modifiers aside, folds to one result in any row
/// order of a group whose key Sort the group drops, so no reader of the group sees the Sort's order: `order_free`'s
/// answer (the plan-order run included) less `absorb_keeps`. `bytes_eq` answers whether the call's input collation
/// compares by bytes (asked for text and bytea extrema only).
pub fn absorb_free(c: Class, bytes_eq: impl FnOnce() -> bool) -> bool {
    order_free(c, &SIMPLE) && !absorb_keeps(c, bytes_eq)
}

/// Where the absorb keeps the Sort that `order_free` frees: a float fold (its last digits follow the partial sums'
/// order), a min / max whose equal members can print apart (float8 -0 / 0, numeric 1.0 / 1.00, an opaque type's, text or
/// bytea under a collation that does not compare by bytes), regr_count and the integer and numeric statistics,
/// range_agg, and the ordered-set and hypothetical calls (their WITHIN GROUP sort is their own; `call_order_free` keeps
/// them out by their ORDER BY as well). A class leaves this list with a witness of its own.
fn absorb_keeps(c: Class, bytes_eq: impl FnOnce() -> bool) -> bool {
    match c {
        Class::Min(e) | Class::Max(e) => match e {
            Ext::Int | Ext::U64 | Ext::Tid => false,
            Ext::Text | Ext::Bytes => !bytes_eq(),
            Ext::F64 | Ext::Num | Ext::Opaque => true,
        },
        Class::SumF32 | Class::SumF64 | Class::AvgF64 | Class::Stat3F64(_) | Class::Stat6F64(_) => true,
        Class::CountPair | Class::Stat3I128(_) | Class::Stat3Num(_) => true,
        Class::Collect(_) | Class::OrderedSet(_) | Class::Hypothetical(_) => true,
        _ => false,
    }
}

/// D612's absorb test: every aggregate call in sorted aggregate `a`'s lists folds to the same result in any row order
/// (`call_order_free`), and the aggregate is not split (partial / final). ops/group/absorb.rs names it `order_free`.
pub fn absorb_order_free<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>) -> bool {
    if a.aggsplit != 0 {
        return false;
    }
    let (mut calls, mut seen) = (true, true);
    for l in [&a.plan.targetlist, &a.plan.qual] {
        for n in l.iter() {
            // a node the walk cannot see into (false) could hide a call: keep the Sort
            seen &= crate::expr::walk(n, &mut |x: Node<'p>| {
                if let Some(r) = x.as_aggref() {
                    calls &= call_order_free(b, r);
                }
                true
            });
        }
    }
    calls && seen
}

/// Whether `child`, a plan child (`lefttree` / `righttree`) of `parent` (the node whose mapping runs, `free` its own
/// `order_free`), may hand its rows up in any order: under a plain or hashed aggregate whose every call folds the same
/// in any row order (`absorb_order_free`), a hash table's build, or a full Sort (its ties may come out in any order, as
/// PostgreSQL's unstable tuplesort and Gather Merge give them; the judge compares equal sort keys as a multiset);
/// through a hash join, a Material or a Result, and to a nested loop's outer, when the parent's own rows may come in
/// any order. Everything else keeps the child's order: the statement's top, a merge join, a nested loop's inner, a
/// sorted group, a Limit, a window, an incremental sort (it reads its presorted prefix), a Unique, an Append, a
/// subquery scan, and an InitPlan's or a SubPlan's root.
pub fn order_free_under<'p>(b: &Builder<'p, '_>, parent: Option<Node<'p>>, child: Node<'p>, free: bool) -> bool {
    use types_nodes::NodeTag::*;
    let Some(p) = parent else { return false };
    // a plan child only: an InitPlan's or a SubPlan's root mapped under the node is no child of its rows
    let tree = p.as_plan().is_some_and(|pp| [pp.lefttree, pp.righttree].iter().flatten().any(|c| c.as_plan().zip(child.as_plan()).is_some_and(|(x, y)| core::ptr::eq(x, y))));
    if !tree {
        return false;
    }
    match p.node_tag() {
        T_Agg => p.as_agg().is_some_and(|a| matches!(a.aggstrategy, crate::ops::shared::agg::AGG_PLAIN | crate::ops::shared::agg::AGG_HASHED) && absorb_order_free(b, a)),
        T_Hash => true,
        T_Sort => {
            fires::fire("order.free.sort"); // shapecheck fire signal: a full Sort's child is order-free
            true
        }
        T_HashJoin | T_Material | T_Result => free,
        T_NestLoop => {
            let outer = free && p.as_plan().and_then(|pp| pp.lefttree).and_then(|o| o.as_plan()).zip(child.as_plan()).is_some_and(|(o, c)| core::ptr::eq(o, c));
            if outer {
                fires::fire("order.free.nl_outer"); // shapecheck fire signal: a free nested loop's outer is order-free
            }
            outer
        }
        _ => false,
    }
}

/// D4568 (road `scan.mergefree`): `order_free_under` with a hash join's build input free too. A Hash's rows reach its
/// join only through the bucket chains, so its input order shows only as each probe row's within-key match order, and
/// that order is visible only where `chain_order::visible` says so (D3307 keeps such a build serial). The build's
/// mapping runs under the join (`Builder::child` maps the Hash's input with the join as `parent`), so the Hash arm of
/// `order_free_under` never sees it: here a hash join's Hash input is free unless its chain order is visible. `free`
/// is the parent's own `merge_free`, `order_free` the child's `order_free` (true here too). Only the merge mark reads it
/// (ops/scan `map_seq_scan`); `order_free`'s other readers keep their answer.
pub fn merge_free_under<'p>(b: &Builder<'p, '_>, parent: Option<Node<'p>>, child: Node<'p>, free: bool, order_free: bool) -> bool {
    if order_free {
        return true;
    }
    let Some(p) = parent else { return false };
    if let Some(hj) = p.as_hash_join() {
        let build = hj.join.plan.righttree.and_then(|h| h.as_hash()).and_then(|h| h.plan.lefttree);
        if build.and_then(|c| c.as_plan()).zip(child.as_plan()).is_some_and(|(x, y)| core::ptr::eq(x, y)) {
            // D4585: the build arm is road `scan.mergefree.build`, default off: a small one-part dimension read as a hash
            // build input is cheaper as one leader merge than as a wide scan with a parallel build (D4585's A/B), so its
            // mark stays unless the road is turned on (`+scan.mergefree.build`)
            let free = b.facts.scan_mergefree_build() && !crate::facts::chain_order::visible(b, hj);
            if free {
                fires::fire("order.free.build"); // shapecheck fire signal (D4568): a hash build's input order is free
            }
            return free;
        }
    }
    free && order_free_under(b, parent, child, free)
}

/// One call with no DISTINCT, ORDER BY or direct argument of its own whose class is `absorb_free`: a count, an exact sum
/// or average (integer, numeric, money, interval), an integer / tid / byte-identical min or max, a bool or bit fold.
pub(crate) fn call_order_free(b: &Builder<'_, '_>, r: &Aggref<'_>) -> bool {
    r.agglevelsup == 0 && r.aggorder.is_nil() && r.aggdistinct.is_nil() && r.aggdirectargs.is_nil() && class_absorb_free(b, r)
}

/// A DISTINCT call with no ORDER BY or direct argument whose distinct set folds the same in any row order: a count
/// (it counts the classes), or an `absorb_free` class over arguments whose equal values are one value (absorb.rs
/// `same_when_equal`: which of two equal values survives cannot show).
pub(crate) fn distinct_order_free(b: &Builder<'_, '_>, r: &Aggref<'_>) -> bool {
    if r.agglevelsup != 0 || r.aggdistinct.is_nil() || !r.aggorder.is_nil() || !r.aggdirectargs.is_nil() {
        return false;
    }
    let Some(row) = crate::ops::agg::trans::table::row(r.aggfnoid) else { return false };
    if matches!(row.class, Class::CountAny) {
        return true;
    }
    class_absorb_free(b, r)
        && r.args.iter().all(|t| {
            let Some(e) = t.as_target_entry().map(|t| t.expr) else { return false };
            let typmod = e.as_var().map_or(-1, |v| v.vartypmod);
            crate::catalog::expr_type::type_of(e).is_some_and(|ty| crate::ops::group::absorb::same_when_equal(b, ty, typmod, r.inputcollid))
        })
}

/// Call `r`'s class (from the transition table) is `absorb_free` under its input collation.
fn class_absorb_free(b: &Builder<'_, '_>, r: &Aggref<'_>) -> bool {
    let Some(row) = crate::ops::agg::trans::table::row(r.aggfnoid) else { return false };
    absorb_free(row.class, || r.inputcollid == 0 || b.facts.coll(r.inputcollid).bytes_eq)
}

/// D345: whether PostgreSQL presorted an aggregate of group `g` (an Aggref `aggpresorted`): a Sort under the Agg feeds
/// its input in that call's order, so the group's first input row is that Sort's first.
pub fn presorted(g: &crate::ops::shared::agg::GroupDecl<'_>) -> bool {
    g.aggs.iter().any(|d| presorted_call(d))
}

/// Whether PostgreSQL presorted call `d` (its Aggref's `aggpresorted`, nodeAgg.c: the planner put a Sort under the Agg in
/// the call's DISTINCT / ORDER BY order, so the call reads its input already sorted and skips its own sort).
pub fn presorted_call(d: &crate::ops::shared::agg::AggDecl<'_>) -> bool {
    d.node.as_aggref().is_some_and(|r| r.aggpresorted)
}

/// errorhold's licence for the folds `order_free` frees by `raises_by_order`: when graph `g` holds one, an error its
/// statement raises before any row left re-runs it in plan order (a recheck licence for a departure ahead, asked with the
/// statement's acting set: `PipelineGraph::licensed`), so an overflow only Gravity's order meets is never the statement's.
/// Denied (the statement acts), its lowering keeps the fold in plan order (`lowering`).
pub fn license_raising(g: &mut crate::graph::PipelineGraph<'_>) {
    use crate::effects::{licence, Departure, Effects};
    let free = |d: &crate::ops::shared::agg::AggDecl<'_>| crate::effects::raises_by_order(d.class) && order_free(d.class, &d.mods);
    // D2846: a stream over a feed in PostgreSQL's order folds in plan order (`in_order_scope`): no licence for its calls
    let streams = g.streams.iter().filter(|x| !g.in_order.contains(&x.node_id));
    // NEW-numeric-1: an order-key extreme raises on a value past its key wherever it meets one (ops/agg `held_whole`)
    let keyed = |d: &crate::ops::shared::agg::AggDecl<'_>| matches!(d.reg, crate::ops::shared::agg::Register::MinNumDyn | crate::ops::shared::agg::Register::MaxNumDyn);
    let any = g.groups.iter().chain(streams).any(|x| x.aggs.iter().any(free)) || g.groups.iter().any(|x| x.aggs.iter().any(keyed));
    if let (true, Ok(l)) = (any, licence(Effects { acts: g.acts, ..Effects::RAISES }, Departure::Ahead)) {
        g.licensed(&l);
    }
}

/// `f`, the lowering of graph `g`: there a fold `raises_by_order` keeps PostgreSQL's order when `g` acts (a write, a row
/// lock, a VOLATILE call: no plan-order re-run can repeat the statement, so no recheck licence), and `rechecked` answers
/// whether `g` carries the recheck licence. Restored when `f` returns or unwinds (nested lowerings included).
pub fn lowering<T>(g: &crate::graph::PipelineGraph<'_>, f: impl FnOnce() -> T) -> T {
    crate::effects::lowering(g.acts, g.recheck, f)
}

/// Whether folds `raises_by_order` keep PostgreSQL's order here: the plan-order run (`effects::denying`), the lowering
/// of a statement that acts, or the step of a stream over a feed in PostgreSQL's order (`in_order_scope`). A float
/// average then raises its overflow at the row, as `float8_accum` does (D2597).
pub fn plan_order() -> bool {
    crate::effects::denying() || crate::effects::statement_acts() || IN_ORDER.with(Cell::get)
}

/// Whether a raise here re-runs the statement in plan order: inside `lowering` of a graph holding the recheck licence,
/// outside the plan-order run and outside `in_order_scope`. A fold `raises_by_order` then raises wherever some input
/// order could overflow (D2597).
pub fn rechecked() -> bool {
    !crate::effects::denying() && crate::effects::statement_rechecks() && !IN_ORDER.with(Cell::get)
}

leader_tls::leader_tls! {
    thread_local! {
        /// Inside `in_order_scope(true, ..)`.
        static IN_ORDER: Cell<bool> = const { Cell::new(false) };
    }
}

/// `f`, the lowering of a streamed group's step; with `on` (the group is one of `PipelineGraph::in_order`: its feed hands
/// it PostgreSQL's row order, ties too) its folds `raises_by_order` run in plan order: a float sum, average or variance
/// raises at the row PostgreSQL raises at and nowhere else, so a row the stream handed on before it is PostgreSQL's too
/// and no re-run is needed (D2846; D2817's breaker form is for the other feeds). Restored when `f` returns or unwinds.
pub fn in_order_scope<T>(on: bool, f: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            IN_ORDER.with(|c| c.set(self.0));
        }
    }
    let _r = Restore(IN_ORDER.with(|c| c.replace(on)));
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::{raises_by_order, scan_raises};
    use crate::ops::shared::agg_class::{Bit, Ext};

    const PLAIN: Mods = Mods { distinct: 0, order: 0, filter: false, direct: 0, split: Split::Simple, star: false };

    #[test]
    fn the_rule() {
        for c in [Class::CountStar, Class::SumNum, Class::Min(Ext::F64), Class::Max(Ext::Text), Class::Min(Ext::Opaque), Class::Collect(Collect::Range)] {
            assert!(order_free(c, &PLAIN), "{c:?}");
        }
        for c in [Class::SumMoney, Class::SumInterval, Class::AvgInterval, Class::SumF64, Class::SumF32, Class::AvgF64, Class::BitVar(Bit::Or), Class::BitVar(Bit::Xor)] {
            assert!(order_free(c, &PLAIN) && raises_by_order(c), "{c:?}: free, its raise re-run in plan order");
            assert!(!crate::effects::deny_all(|| order_free(c, &PLAIN)), "{c:?}: the plan-order run keeps the order");
        }
        let arena = gravity_program::Arena::new(1 << 12);
        let mut g = crate::graph::PipelineGraph::new(arena.handle());
        assert!(!rechecked() && !plan_order(), "outside a lowering: no licence, the free order");
        g.acts = true;
        assert!(lowering(&g, || !order_free(Class::SumF64, &PLAIN) && plan_order() && !rechecked()), "a statement that acts keeps the order");
        (g.acts, g.recheck) = (false, true);
        assert!(lowering(&g, || order_free(Class::AvgF64, &PLAIN) && rechecked() && !crate::effects::deny_all(rechecked)), "licensed: raises re-run");
        assert!(!rechecked() && !plan_order(), "lowering restores");
        assert!(scan_raises(Class::SumMoney) && scan_raises(Class::AvgInterval) && !scan_raises(Class::SumF64) && !scan_raises(Class::SumI64), "no guard of their own");
        for c in [Class::User, Class::Collect(Collect::Array), Class::Collect(Collect::RangeIntersect)] {
            assert!(!order_free(c, &PLAIN), "{c:?}");
        }
        assert!(!order_free(Class::CountAny, &Mods { distinct: 1, ..PLAIN }), "DISTINCT keeps the order");
        assert!(!order_free(Class::SumI64, &Mods { order: 1, ..PLAIN }), "an ORDER BY's ties read the input order");
        assert!(!order_free(Class::SumI64, &Mods { split: Split::Final, ..PLAIN }), "a split call is not decided here");
    }

    /// The absorb's reading frees exactly counts, exact sums and averages (integer, numeric, money, interval), bool and bit
    /// folds, and integer / tid extrema (text and bytea ones under a collation that compares by bytes); in the plan-order
    /// run the folds that raise by their partial sums keep the order, as `order_free` says.
    #[test]
    fn the_absorb_reading() {
        use crate::ops::shared::agg_class::Stat;
        let free = [Class::CountStar, Class::CountAny, Class::SumI64, Class::SumI128, Class::SumNum, Class::AvgI128, Class::AvgNum, Class::BoolAnd, Class::BoolOr, Class::BitI64(Bit::And), Class::Min(Ext::Int), Class::Max(Ext::U64), Class::Min(Ext::Tid)];
        let by_order = [Class::SumMoney, Class::SumInterval, Class::AvgInterval, Class::BitVar(Bit::Or)];
        let kept = [Class::SumF32, Class::SumF64, Class::AvgF64, Class::CountPair, Class::Min(Ext::F64), Class::Max(Ext::Num), Class::Min(Ext::Opaque), Class::Collect(Collect::Range), Class::Collect(Collect::Array), Class::User];
        for c in free.into_iter().chain(by_order) {
            assert!(absorb_free(c, || false), "{c:?}");
        }
        for c in kept {
            assert!(!absorb_free(c, || true), "{c:?}");
        }
        assert!(absorb_free(Class::Max(Ext::Text), || true) && !absorb_free(Class::Max(Ext::Text), || false), "text extrema: by the collation");
        assert!(!absorb_free(Class::Stat3I128(Stat::VarSamp), || true) && !absorb_free(Class::Stat3Num(Stat::VarPop), || true));
        for c in by_order {
            assert!(!crate::effects::deny_all(|| absorb_free(c, || true)), "{c:?}: the plan-order run keeps the order");
        }
        assert!(crate::effects::deny_all(|| absorb_free(Class::SumI64, || true)), "an exact fold stays free there");
    }

    /// PM2: a clause orders by its type's default family by the family's membership, not an oid range: int4 `<` 97 and
    /// `>` 521 do; text_pattern_ops' `~<~` 2314 (a builtin operator of a non-default family), no sortop and an operator
    /// the source never saw do not.
    #[test]
    fn a_clause_orders_by_its_default_family() {
        let f = crate::catalog::NoFacts;
        assert!(default_order(&f, 97) && default_order(&f, 521));
        assert!(!default_order(&f, 2314) && !default_order(&f, 0) && !default_order(&f, 70_000));
    }
}
