//! The lowering's one raise-free walk: whether an expression raises on no input (D237: a row PostgreSQL would not have
//! evaluated must not raise here). Each node's own step is the effects home's per-node rule (decompose
//! `effects::quiet_node`, which the home's own `quiet` asks: the one leaf set and call rule) or GREATEST / LEAST over a
//! type whose compare is pure; what differs between the decisions that ask it is named in a `Reading`: which executor
//! parameters are bound words, how an aggregate reads, whether a true array's element reads are taken, and whether a
//! List reads as an AND. The per-node rule reads its constants (a divisor's word) at the same points every time, so they
//! key the program the same way; a constant pattern's bytes are read unlogged, its class held by both keys.
//!
//! The readings: `rtfilter` (effects.rs `raise_free_by`), `merge` (effects/visible/merge.rs `pure_in`: D449, D1236,
//! D1417), `group_row` (effects/visible/topn.rs `pure`: D449 over a group's row) and `union_arms`
//! (effects/visible/nestloop.rs `quiet_in`: D1486's quiet arms).
use gravity_decompose::effects::{collation_set, quiet_node, Scope};
use types_nodes::primnodes::Aggref;
use types_nodes::Node;

use crate::graph::Gx;

/// How an aggregate reads.
#[derive(Clone, Copy)]
pub(crate) enum Agg<'r> {
    /// An Aggref is no leaf (its kids are still walked).
    None,
    /// The aggregate's answer is this function's, its arguments not walked (an Aggref read as its group's final).
    Final(&'r dyn Fn(&Aggref<'_>) -> bool),
    /// The fold reading: an aggregate decompose's `agg_quiet` takes (a builtin plain aggregate whose transitions and final
    /// raise on no input) with no ORDER BY, DISTINCT or direct argument (each sorts or compares through the argument
    /// type's procs, which a Var argument does not show); its arguments and FILTER are kids.
    Fold,
}

/// One raise-free reading: what a decision asks beside the effects home's per-node rule (`quiet_node`).
#[derive(Clone, Copy)]
pub(crate) struct Reading<'r> {
    /// The PARAM_EXEC ids that are words bound before the expression runs (any other may run an InitPlan or SubPlan that
    /// raises); an extern parameter is always one.
    pub(crate) param: &'r dyn Fn(i32) -> bool,
    pub(crate) agg: Agg<'r>,
    /// PostgreSQL 18.6's quiet array reads: array_lower / array_upper (NULL out of range), a true array's element fetch
    /// (`quiet_fetch`), an element CaseTestExpr, a no-op array coercion (int2vector::int2[]).
    pub(crate) array_reads: bool,
    /// A List at any level reads as the AND of its members (a Result's one-time filter holds resconstantqual whole), each
    /// member tried in turn until one is not raise-free.
    pub(crate) list: bool,
}

impl<'r> Reading<'r> {
    /// effects.rs `raise_free_by`: the quiet reading's fold form (`quiet`), the PARAM_EXEC ids `quiet` names bound words;
    /// under `arr` the quiet array reads too.
    pub(crate) fn rtfilter(quiet: &'r dyn Fn(i32) -> bool, arr: bool) -> Reading<'r> {
        Reading::quiet(quiet, Agg::Fold, arr)
    }

    /// effects/visible/merge.rs `pure_in` (D449, D1236): the quiet reading's row form, the PARAM_EXEC ids `vals` names
    /// values; with `counts` (D1417) its fold form.
    pub(crate) fn merge(vals: &'r dyn Fn(i32) -> bool, counts: bool) -> Reading<'r> {
        Reading::quiet(vals, if counts { Agg::Fold } else { Agg::None }, false)
    }

    /// Quiet calls over the shared leaf set; `agg`: the fold form (an aggregate `Agg::Fold` takes) or the row form (no
    /// aggregate: its transitions ran in the pipelines that fold into the slot it reads, which the caller reads apart);
    /// the PARAM_EXEC ids `bound` names are bound words; under `arr` the quiet array reads.
    pub(crate) fn quiet(bound: &'r dyn Fn(i32) -> bool, agg: Agg<'r>, arr: bool) -> Reading<'r> {
        Reading { param: bound, agg, array_reads: arr, list: false }
    }

    /// effects/visible/topn.rs `pure` (D449 over a group's row): the one reading's row form (AGA-17 took its quiet calls and
    /// node kinds), where an extern parameter is a bound word and no PARAM_EXEC is (an InitPlan's or a SubPlan's may
    /// raise), an aggregate read as its group's final by `final_quiet`.
    pub(crate) fn group_row(final_quiet: &'r dyn Fn(&Aggref<'_>) -> bool) -> Reading<'r> {
        Reading { param: &no_exec, agg: Agg::Final(final_quiet), array_reads: false, list: false }
    }

    /// effects/visible/nestloop.rs `quiet_in` (D1486's quiet arms): quiet calls, a List as an AND; the PARAM_EXEC ids
    /// `bound` names (a nest's or a per-row SubPlan's, written before the re-open) are bound words. No aggregate.
    pub(crate) fn union_arms(bound: &'r dyn Fn(i32) -> bool) -> Reading<'r> {
        Reading { param: bound, agg: Agg::None, array_reads: false, list: true }
    }
}

/// No PARAM_EXEC id is a bound word.
fn no_exec(_: i32) -> bool {
    false
}

/// Whether `n` and everything under it raises on no input under reading `r` (a node the walker cannot see does not).
pub(in crate::effects) fn walk(gx: &Gx<'_, '_>, n: Node<'_>, r: &Reading<'_>) -> bool {
    crate::stack_check();
    if r.list {
        if let Some(l) = n.as_list() {
            return l.iter().all(|c| walk(gx, c, r));
        }
    }
    if let Agg::Final(f) = r.agg {
        if let Some(a) = n.as_aggref() {
            return f(a);
        }
    }
    let fetch = |x: Node<'_>| super::quiet_fetch(gx, x);
    let scope = Scope { bound: r.param, fetch: r.array_reads.then_some(&fetch as &dyn Fn(Node<'_>) -> bool) };
    let own = match n.as_aggref() {
        Some(a) => matches!(r.agg, Agg::Fold) && gravity_decompose::effects::agg_quiet(a) && a.aggorder.is_nil() && a.aggdistinct.is_nil() && a.aggdirectargs.is_nil(),
        None => quiet_node(gx.facts, n, &scope) || n.as_min_max_expr().is_some_and(|m| cmp_pure(gx, m.minmaxtype) && collation_set(0, m.inputcollid, &m.args)),
    };
    let mut ok = own;
    let known = gravity_decompose::expr::kids(n, &mut |c| ok &= walk(gx, c, r));
    ok && known
}

/// D1236 (MERGE-3b), PM2: a type whose default btree procs (GREATEST / LEAST's and a Sort key's compare) neither act nor
/// raise on any input: effects' answer for the type's compare (`effects::of_compare`), not a type list. A record, array,
/// range or user type compares through procs effects does not know, which may run user code or raise.
pub(crate) fn cmp_pure(gx: &Gx<'_, '_>, typid: u32) -> bool {
    let e = gravity_decompose::effects::of_compare(gx.facts, typid);
    !e.acts && !e.may_raise
}
