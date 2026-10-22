//! D1210 (V4R-403, over cards/M6.md's "pair set per argument"): host-sorted DISTINCT aggregates and the pair
//! groups re-declared for them before lowering.
//!
//! A DISTINCT aggregate the pair set cannot hold folds host-sorted (`sorted`): PG 18.6's own
//! process_ordered_aggregate_single (hosts v4/m6_agg_distinct.rs). The group keeps each such aggregate's inputs past its
//! FILTER and the strict input check; its final sorts them by the DISTINCT clause's operator and collation, drops the equal
//! ones and folds the rest in that order, so float, numeric, text and collations match PG exactly (fold/hosted.rs slots
//! it, fold/class_of.rs folds it by fmgr). A group decompose declared in the pair form (`GroupForm::Distinct`: the first
//! DISTINCT argument its last key) that the pair set cannot serve (`unpaired`) is re-declared here (`regroup`) as decompose
//! declares a group with no pair set: the argument key dropped, every DISTINCT aggregate host-sorted, the group sorted
//! (runs.group: PG never hashes a group with a DISTINCT aggregate, so its Agg is a GroupAggregate; the Distinct breaker
//! is the one runs.group keeps for a Group node) or, with no group key, plain (the plain end lays the breaker out as its
//! one-record region). The re-declared groups are an overlay the graph's readers consult first (`Overlay`, graph.rs
//! `Gx::group_of`); the graph stays decompose's. What cannot be re-declared (`blocked`) keeps the pair form,
//! whose makers hand it to keyed.distinct_multi, which names why.
use gravity_abi::breaker::{KeyKind, SlotKind};
use gravity_decompose::ops::shared::agg::fold_of;
use gravity_decompose::ops::shared::agg::{AggDecl, FinalForm, GroupDecl, GroupForm, NONE};
use gravity_decompose::ops::shared::agg_class::{Class, Trans};
use gravity_decompose::catalog::Facts;
use gravity_decompose::effects::{unobservable, Effects, Licence};
use gravity_decompose::facts::order::presorted_call;
use gravity_expr::Catalog;
use gravity_decompose::graph::Layout;
use gravity_decompose::{Op, PipelineGraph, Source};
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::{ArenaRef, ArenaVec};
use types_nodes::Node;

use crate::effects::visible::group::{builtin, fin_acts, trans_acts};
use crate::frame::px::Px;
use crate::ops::shared::agg::{sort_cols, sorted};
use mcx::CollectIn;
use mcx::{Mcx, PgVec};

fn pair(d: &AggDecl<'_>) -> bool {
    matches!(d.final_, FinalForm::PairCount | FinalForm::PairFold)
}

/// `d` as decompose declares a DISTINCT aggregate with no pair set: its class's register, no slot, a `Var` state, the
/// `Distinct` final; any other aggregate as it is.
fn host_sorted<'p>(d: &AggDecl<'p>) -> AggDecl<'p> {
    if d.mods.distinct == 0 {
        return *d;
    }
    let reg = fold_of(d.class, d.fnoid, false, false).0;
    AggDecl { reg, slot: NONE, count_slot: NONE, final_: FinalForm::Distinct, trans: Trans::Var, ..*d }
}

/// Whether the pair set cannot hold pair-form group `g` (what keyed/pairs.rs, keyed/distinct_plain.rs and
/// keyed/hashagg_rec.rs `pair_filter` refuse): an aggregate with no record slot beside the pair (a second DISTINCT
/// argument, a fold the pair set does not take, a hosted rider), a DISTINCT fold the pair owner does not fold (a fourth,
/// a non-integer argument or slot), a FILTER on a DISTINCT aggregate beside a second one, or a pair that may run past
/// the owner's 255 words (`past_owner_words`).
fn unpaired(g: &GroupDecl<'_>) -> bool {
    // a tagged multi-argument form (ops/group/pairs_multi.rs) its pair end does not serve keeps the host-sorted form
    if crate::ops::shared::agg::tagged_multi(g) {
        return crate::ops::group::pairs_multi::blocked(g);
    }
    let Some(arg) = g.keys.last().filter(|k| k.resno == 0 && g.form == GroupForm::Distinct) else { return false };
    let int = matches!(arg.kind, KeyKind::I32 | KeyKind::I64);
    let folded = |d: &&AggDecl<'_>| int && matches!(fold_of(d.class, d.fnoid, false, false).1.abi(), Some(SlotKind::SumI64 | SlotKind::SumI128 | SlotKind::AvgI128 | SlotKind::MinI64 | SlotKind::MaxI64));
    let folds = || g.aggs.iter().filter(|d| d.final_ == FinalForm::PairFold);
    let filtered = g.aggs.iter().any(|d| pair(d) && d.filter.is_some()) && g.aggs.iter().filter(|d| pair(d)).count() > 1;
    g.aggs.iter().any(|d| !pair(d) && d.slot == NONE) || folds().count() > 3 || !folds().all(|d| folded(&d)) || filtered || past_owner_words(g)
}

/// D3374 (cr_cap06_pair_past_255, contract-runtime CAP-06): whether pair-form group `g`'s pair record may run past 255
/// words. The distinct owner names the pair's flags word by a byte and the group-flags form builds its accumulator keys
/// in a 255-word buffer (keyed/pairs.rs `owner`, exec owner.rs `ACC_KEYS_MAX`), while a record runs past 255 key words
/// (agg CAP-32), so such a group folds its DISTINCT aggregates host-sorted, as PostgreSQL's GroupAggregate does. An upper
/// bound read before lowering, with no second copy of the NULL rule (D521): decompose's key words (the key image takes
/// no more, dict/coderep.rs `image_layout`) and the flags words a nullable key adds (one per 64 keys, CAP-28).
fn past_owner_words(g: &GroupDecl<'_>) -> bool {
    g.layout.keys.len() + gravity_abi::breaker::flag_words(g.keys.len()) > u8::MAX as usize
}

/// Whether every aggregate of `g` but its DISTINCT and host-sorted ORDER BY ones (D458) gives the same result in any row
/// order (graph.rs `order_free`, D859): PG's GroupAggregate feeds them its sorted input (under aggpresorted, the ORDER BY's
/// order), a DISTINCT or ordered aggregate sorts its own. D1427: an ordered-set or hypothetical aggregate (WITHIN GROUP)
/// sorts its own input in its transition state (PG's ordered_set_transition tuplesort), so it reads no row order either.
/// D1371 R1: or a plain group none of whose aggregates PG presorted (no Aggref `aggpresorted`): each DISTINCT / ordered
/// aggregate sorts its own rows and PG feeds the riders its input order, the order the plain end folds them in.
/// G10 (D458): or a group whose riders meet their rows in PG's order (`fed_in_order`); rnde-0356: or a sorted one whose
/// riders meet them so beside host-sorted DISTINCT aggregates (`distinct_fed`).
pub fn riders_free(a: ArenaRef, graph: &PipelineGraph<'_>, g: &GroupDecl<'_>) -> bool {
    let own = |d: &&AggDecl<'_>| d.mods.distinct > 0 || sort_cols(d) || matches!(d.class, Class::OrderedSet(_) | Class::Hypothetical(_));
    let presorted = gravity_decompose::facts::order::presorted(g);
    g.form == GroupForm::Plain && !presorted || fed_in_order(graph, g) || distinct_fed(graph, g) || crate::graph::order_free(&copy(a, g, g.form, g.keys.len(), g.aggs.iter().filter(|d| !own(d)).copied()))
}

/// rnde-0356 (G10 beside a DISTINCT): whether sorted or plain group `g`'s riders fold in the order PostgreSQL's
/// Agg feeds them beside its DISTINCT aggregates, whatever their class (`avg(f)` over float8). PG 18.6 sorts the
/// Agg's input by the group keys (none for a plain Agg) and then the DISTINCT argument (Aggref `aggpresorted`) and folds
/// every rider per row in that order. Ok when `g` is sorted, or plain where D1596's per-row form does not take it by its
/// shape (a rider argument or FILTER that computes or reads a column outside the Sort's keys, a native fold that may
/// raise), with no grouping set, two-pass or hint. A rider collect (`collect`: an array, string, JSON or range collect)
/// prints the order of the rows tied on the group keys and the DISTINCT argument, the order PostgreSQL's quicksort leaves
/// them in: free (D3374, D1428: the ties of the planner's implicit DISTINCT presort are any order; the rig's `.ati` leg
/// checks it tie-aware, companion keys the group keys then the DISTINCT argument);
/// every DISTINCT aggregate folds host-sorted
/// (`sorted`: its final sorts its own inputs by the DISTINCT clause, drops the equal ones and folds the rest in that order,
/// process_ordered_aggregate_single: PG's values, PG's order), is built in over built-in types (`builtin`, not
/// `Class::User`: its transitions, which run at the final here and per row beside the riders' in PG, call no user code,
/// so no NOTICE or write of theirs moves among the riders'; a raise of theirs meets PG's in the same group, D775's error
/// order), beside any aggregate with an ORDER BY of its own (D4041: host-sorted as G10's `fed_in_order` holds them, its final
/// judged by `call_order`); and `g` reads the drain of the Sort PostgreSQL put under its Agg through projections alone
/// (`presorted::in_sort_order`; a streamed group's re-declaration, `redeclare_stream`, by its stream: the pipeline its
/// `Op::GroupRun` runs in, up to that op). The host-sorted states make the keyed record run its program width 1 (exec
/// agg.arena, V4R-375), so each group's rows fold in that drain's order. Rows tied on every Sort key arrive in an order
/// PostgreSQL leaves free (DECISIONS 5, the tie rule). The DISTINCT transitions run at the group's final, not
/// per row: `call_order` (D1371) and `redeclare_stream` (`trans_acts`) refuse by name where one may act or raise out of
/// PostgreSQL's order. Where float folds may go in any order the pair form serves these groups as they are; this is the
/// plan-order lowering's (`effects::denying`: the recheck after a raise, a statement that acts), whose riders must fold
/// as PostgreSQL's do (rnde-0356: a float8 average whose partial sums overflowed out of PG's order).
pub fn distinct_fed(graph: &PipelineGraph<'_>, g: &GroupDecl<'_>) -> bool {
    // a plain group D1596's per-row form admits by its shape keeps that form (presorted.rs `presorted`)
    let plain = g.form == GroupForm::Plain && !crate::ops::group::presorted::shaped(graph, g);
    let form = (g.form == GroupForm::Sorted || plain) && g.set.is_none() && g.two_pass.is_none() && g.hints.iter().all(Option::is_none);
    let quiet = |d: &AggDecl<'_>| sorted(d) && d.class != Class::User && builtin(d);
    let host = g.aggs.iter().any(|d| d.mods.distinct > 0) && g.aggs.iter().all(|d| d.mods.distinct == 0 || quiet(d));
    let fed = match graph.streams.iter().find(|s| s.node_id == g.node_id) {
        Some(s) => crate::ops::group::presorted::in_sort_order(graph, s),
        None => crate::ops::group::presorted::in_sort_order(graph, g),
    };
    form && host && fed
}

/// G10 (D458, spec "delete the order gates"): whether group `g`'s riders fold in the order PostgreSQL's Agg feeds them,
/// whatever their class. Every host-sorted aggregate is an ORDER BY one (`sort_cols`; a DISTINCT keeps D1596's presorted
/// form), so its hosted Sorted spec keeps the aggregate arena serial (exec agg.arena: one slot, every pipeline of the
/// program width 1); the group is plain, sorted or streamed (no grouping set, two-pass or hint) and reads the drain of
/// the Sort PostgreSQL put under its Agg through projections alone (`presorted::in_sort_order`: the ORDER BY's presort,
/// or the group keys' Sort, which stays a pipe beside an ordered call, absorb.rs). Each rider then folds per row in that
/// Sort's output order, as PostgreSQL feeds it; rows tied on every Sort key arrive in an order PostgreSQL leaves free (an
/// aggregate's input order without its own ORDER BY is unspecified: DECISIONS 5, the tie rule), so a float
/// sum's value and its overflow are PostgreSQL's for some tie order. The host sorts each ordered aggregate's own run at
/// the final (its ties free, D1428).
pub fn fed_in_order(graph: &PipelineGraph<'_>, g: &GroupDecl<'_>) -> bool {
    let form = matches!(g.form, GroupForm::Plain | GroupForm::Sorted) && g.set.is_none() && g.two_pass.is_none() && g.hints.iter().all(Option::is_none);
    let ordered = g.aggs.iter().any(sort_cols) && g.aggs.iter().all(|d| d.mods.distinct == 0);
    form && ordered && crate::ops::group::presorted::in_sort_order(graph, g)
}

/// D1371 (D831's class: NOTICEs, writes and sequence values in PG's order, D775's: PG's
/// error): why group `g` cannot fold its host-sorted aggregates at its final (`None`: it can). PG runs every sorted
/// transition before any final; the ends fold each host-sorted aggregate at its own final (aggno order), and where that
/// order could be seen they fold every one ahead of the group's finals (effects `folds_ahead`). A host-sorted aggregate that may
/// raise or act refuses in a grouping set's keyed arm (the emit's group order is not PG's; decompose gsets.rs `arm_order`
/// streams the one set of a sorted ROLLUP (k) / GROUPING SETS ((k), ()) instead) and in a keyed end that does not stream (`Op::GroupRun`
/// at `nops`, D1071) unless the per-row work is `still` (D1602's class: PG finalizes each group before it reads the next's
/// rows); served there, D439's pull cap: the emit finalizes a group as it hands it on, so FETCH n finalizes n groups, as PG.
/// An ordered user-defined one sorted by user code (`user_sort`) is served in any group (USEROP):
/// the host's sort meets the rows in their arrival order, PG's in its Sort's, join's or hash table's output
/// order, so the comparator sees other pairs; which pairs a sort compares is PG's own implementation choice (qsort or an
/// external sort under work_mem, presorted detection, pivots), so a comparator raising on one pair, its call count and its
/// NOTICEs are free under the plan-dependent and counts-free rules; that comparator does not count in a
/// keyed end for one PG presorted through the Sort under its Agg (effects `resorts_presorted`).
/// A presorted ordered user-defined one (PG calls its transfn per row) must be the only one. `still`: every argument,
/// FILTER, other aggregate's transition and op up to `nops` raise_free (`quiet`) over rows made first (a scan's, a Sort's).
/// D3374: a plain or streamed group whose host-sorted aggregates all advance per row in PG's order (effects
/// `ordered_fed_acting`) runs every transition where PG does, so none of this applies.
pub fn call_order(px: &mut Px<'_, '_>, g: &GroupDecl<'_>, (k, nops): (usize, usize), quiet: &dyn Fn(Node<'_>) -> bool) -> Result<Option<Licence>, &'static str> {
    let gx = px.gx;
    if crate::effects::ordered_fed_acting(gx, g) {
        return Ok(None);
    }
    let hs: PgVec<'_, &AggDecl<'_>> = g.aggs.iter().filter(|d| sorted(d)).collect_in(px.mcx());
    let cmp = |d: &AggDecl<'_>| !crate::effects::resorts_presorted(gx, g, d);
    let (p, acts) = (&gx.pipelines()[k], hs.iter().any(|d| trans_acts(gx.facts, gx.catalog, d, cmp(d)) || fin_acts(gx, d)));
    if acts && g.set.is_some() { return Err("D1371: a host-sorted aggregate that may raise or act in a grouping set (the emit runs its groups in another order than PG's)") }
    let keyed = acts && g.form != GroupForm::Plain && !matches!(p.ops.get(nops), Some(Op::GroupRun(_)));
    let user = g.aggs.iter().any(|d| sort_cols(d) && d.class == Class::User) && hs.iter().any(|d| presorted_call(d));
    let own = |d: &AggDecl<'_>| (sorted(d) || !trans_acts(gx.facts, gx.catalog, d, true)) && d.node.as_aggref().is_some_and(|r| r.args.iter().all(quiet) && r.aggfilter.is_none_or(quiet));
    let op = |op: &Op<'_>| match op { Op::Filter(n) | Op::Gate(n) => quiet(*n), Op::Project(l) => l.iter().all(quiet), _ => false };
    let made = |q: usize| gx.pipelines().get(q).is_some_and(|x| matches!(x.ops.last(), Some(Op::Sort(s)) if s.presorted == 0)) && gx.drained_group(q).is_none();
    // D1371 (v0.4 refusals): an index scan's rows too, its keys and recheck `quiet` (PG's IndexScan hands them on in the
    // index's order, both engines making nothing but the rows; its filters are this pipeline's ops, judged below)
    let rows = match &p.source {
        Source::Breaker { slot } => gx.life_of(k, *slot).is_some_and(|l| made(l.0 as usize)),
        Source::Range { .. } | Source::Const => true,
        Source::Index { keys, recheck, .. } => keys.iter().all(|x| quiet(x.value)) && recheck.iter().all(quiet),
        _ => false,
    };
    let still = (keyed || user) && rows && p.ops[..nops].iter().all(op) && g.aggs.iter().all(own);
    // over rows made first, what is not `still` (or a presorted user aggregate beside another) folds ahead under the licence;
    // T6b: over an index scan's rows too (PG streams them in the same order; the batch counts the scan's own calls); T6c: over
    // a Sort PG runs incrementally (presorted keys) on a scan's rows, which PG streams too: the batch counts its input's work
    let scan = matches!(p.source, Source::Index { .. });
    // T6h: over a full Sort of another group's rows: PG's Sort reads that group's whole output before its first row, so the
    // other group's work (its finals, HAVING) runs before this group's first row in PG too and only this pipeline's ops batch
    let drained_sort = |q: usize| gx.drained_group(q).is_some() && gx.pipelines().get(q).is_some_and(|x| matches!(x.ops.last(), Some(Op::Sort(s)) if s.presorted == 0));
    let sorted_groups = match &p.source { Source::Breaker { slot } => gx.life_of(k, *slot).is_some_and(|l| drained_sort(l.0 as usize)), _ => false };
    let inc = crate::effects::incremental_feed(gx, k); // its ops, scan and Sort are the batch's feed
    let feeds: PgVec<'_, (usize, usize)> = core::iter::once((k, nops)).chain(inc.map(|q| (q, gx.pipelines()[q].ops.len()))).collect_in(px.mcx());
    let licence = ((keyed || user) && (rows || scan || sorted_groups || inc.is_some()) && !(still && (!user || hs.len() == 1))).then(|| crate::effects::fold_ahead(px, &g.aggs[..], &feeds)).flatten();
    let licensed = licence.is_some();
    if user && !(hs.len() == 1 && still) && !licensed { return Err("D1371: an ordered user-defined aggregate PG presorted beside other per-row work (PG calls its transfn per input row)") }
    // D2824 (after D1371's own refusals, which keep their names): a plain group folds its host-sorted aggregates at its final beside riders that read no row order
    // (`riders_free`), where PG 18.6 runs an aggpresorted one's transition per input row in aggno order between the other
    // aggregates' FILTERs, arguments and transitions (execExpr.c ExecBuildAggTrans): beside one that may raise or act,
    // every aggregate must be `own`, else a FILTER reads what the transition changed (currval after its nextval) or
    // raises before the transition's own error. Only an aggregate PG presorted (its Aggref's `aggpresorted`) runs its
    // transition per input row: one PG did not presort, PG sorts and folds at its final (finalize_aggregates'
    // process_ordered_aggregate_*, aggno order), where the host folds it too. Its transition is the per-row work: one
    // whose class may raise or act (`trans_acts` without the host sort's comparator, which PG's presorted Agg never calls
    // per row) and that #530 does not call `still` (effects visible::agg: no user code, a transition raising on no row,
    // as jsonb_agg / mode() over built-in types)
    let per_row = |d: &AggDecl<'_>| presorted_call(d) && trans_acts(gx.facts, gx.catalog, d, false) && !crate::effects::visible::agg::still(gx, d);
    // A presorted user aggregate the T6 licence already folds ahead (`licensed`: nothing in the batch acts, a raise re-runs
    // the statement in plan order) needs no refusal here.
    // Beside it, an aggregate is quiet when `own` says so or when `still` reads its transition (no user code, raising on no
    // row: json_agg over int4, var_samp over numeric, mode() over a built-in type) with quiet arguments and FILTER: `own`'s
    // class list alone flagged such transitions as may-raise
    let calm = |d: &AggDecl<'_>| own(d) || crate::effects::visible::agg::still(gx, d) && d.node.as_aggref().is_some_and(|r| r.args.iter().all(quiet) && r.aggfilter.is_none_or(quiet));
    // error versus error passes: where no aggregate acts (effects visible::group `acts`: only raises), every
    // evaluation PG makes per row runs here too, so the statement raises exactly where PG's does and only which error comes
    // first moves; a group none of whose aggregates acts is served whatever its per-row raises
    if g.form == GroupForm::Plain && !licensed && hs.iter().copied().any(per_row) && riders_free(px.arena, gx.graph, g) && !g.aggs.iter().all(calm) && crate::effects::visible::group::acts(gx, g) {
        return Err("D2824: beside an aggregate PG presorted that may raise or act, an aggregate argument, FILTER or transition that may (PG runs its transition per input row between them)");
    }
    if keyed && (still || licensed) { px.declare_proto(Proto::new(ProtoKind::Volatile, k as u16)) } // D439: a pull runs no group ahead of its count
    if keyed && !still && !licensed {
        return Err("D1371: a host-sorted aggregate that may raise or act in a keyed group beside per-row work that may (PG finalizes each group before the next group's rows)");
    }
    // construct C2: the licence of the per-row work a keyed group runs ahead of PG's group-by-group finals: T6's (`fold_ahead`),
    // else `still`'s (raise-free and acting nowhere: `effects::unobservable`); a plain or streamed group runs none ahead
    let l = if licence.is_some() || !(keyed && still) { licence } else { unobservable(Effects::NONE) };
    // construct C2 (b): the run-ahead tracer reads it on the group's pipeline (T6's is held on its feeds by `fold_ahead`)
    if let Some(l) = &l {
        px.hold(k, l);
    }
    Ok(l)
}

/// Why group `g` cannot fold its DISTINCT aggregates host-sorted, `None` when it can. D1596: a rider reading the row order
/// passes when `presorted` admits the group (a pair-form one as `host` re-declares it, so keyed.distinct_multi names why
/// `redeclare` could not), else its name.
pub fn blocked(a: ArenaRef, graph: &PipelineGraph<'_>, facts: &dyn Facts, g: &GroupDecl<'_>) -> Option<&'static str> {
    if g.aggs.iter().any(|d| d.mods.distinct > 0 && !sorted(&host_sorted(d))) {
        return Some("a DISTINCT aggregate of a user-defined or ordered-set aggregate (PG's hosted sort)");
    }
    if let (false, Err(why)) = (riders_free(a, graph, g), presorted(graph, facts, host(a, g).as_ref().unwrap_or(g))) {
        return Some(why);
    }
    if g.two_pass.is_some() || g.hints.iter().any(Option::is_some) {
        return Some("a DISTINCT in a planned two-pass group");
    }
    None
}

/// D1596 (D3375): the full Sort pipeline whose PostgreSQL order group `g`'s presorted form needs (presorted.rs `check`), read
/// as `blocked` reads the group (its riders read the row order; a pair-form one re-declared by `host`): sort/exact_feed.rs
/// orders that Sort's run by PG's qsort in arrival order (frame/iface `presorted_exact`).
pub fn exact_feed(a: ArenaRef, graph: &PipelineGraph<'_>, facts: &dyn Facts, g: &GroupDecl<'_>) -> Option<usize> {
    let h = host(a, g);
    // D3374 (D345): a pinned group's kept columns read its first row, which only its plain re-declaration keeps
    // (`redeclare`), so its riders are judged there
    let riders = if g.empty_no_row && !crate::graph::grouped(g) { h.as_ref().unwrap_or(g) } else { g };
    if riders_free(a, graph, riders) {
        return None;
    }
    crate::ops::group::presorted::check(graph, facts, h.as_ref().unwrap_or(g)).ok().flatten()
}

/// A copy of `g` in `a` with its first `n` keys (its layout's key words theirs) and aggregates `aggs`, of form `form`.
fn copy<'p>(a: ArenaRef, g: &GroupDecl<'p>, form: GroupForm, n: usize, aggs: impl Iterator<Item = AggDecl<'p>>) -> GroupDecl<'p> {
    let words = g.keys.get(n).map_or(g.layout.keys.len(), |k| k.word as usize);
    let mut keys = ArenaVec::with_capacity_in(n, a);
    keys.extend(g.keys[..n].iter().copied());
    let mut v = ArenaVec::with_capacity_in(g.aggs.len(), a);
    v.extend(aggs);
    let mut pinned = ArenaVec::with_capacity_in(g.pinned.len(), a);
    pinned.extend(g.pinned.iter().copied());
    let layout = Layout::with_sentinel(a, g.layout.keys[..words].iter().copied(), g.layout.slots.iter().copied(), g.layout.nn, g.layout.sentinel);
    GroupDecl { slot: g.slot, producer: g.producer, node_id: g.node_id, form, hints: g.hints, layout, keys, aggs: v, two_pass: g.two_pass, groups: g.groups, top: g.top, set: g.set, empty_no_row: g.empty_no_row, pinned, finals: g.finals }
}

/// Pair-form group `g` (its DISTINCT argument its last key) with no pair set: that key dropped, every DISTINCT aggregate
/// host-sorted, sorted with a group key and plain without one; `None` for any other group. A grouping set's arm stays
/// one (each set sorts its own DISTINCT inputs, as nodeAgg's per-set sorts) but the empty set's, which is a plain fold
/// like the empty set's arm with no DISTINCT (decompose `map_sets` gives that one no `SetDecl`).
fn host<'p>(a: ArenaRef, g: &GroupDecl<'p>) -> Option<GroupDecl<'p>> {
    let n = g.keys.len().checked_sub(1).filter(|&n| g.form == GroupForm::Distinct && g.keys[n].resno == 0)?;
    // a tagged form's arguments are every resno-0 key after the group keys
    let n = if crate::ops::shared::agg::tagged_multi(g) { g.keys.iter().position(|k| k.resno == 0).unwrap_or(n) } else { n };
    let mut d = copy(a, g, if n > 0 { GroupForm::Sorted } else { GroupForm::Plain }, n, g.aggs.iter().map(host_sorted));
    d.set = g.set.filter(|_| n > 0);
    Some(d)
}

/// Pair-form group `g` re-declared with no pair set (`host`) where the pair set cannot hold it, or (rnde-0356) where its
/// riders read the row order (the pair ends fold them per pair, out of PG's order), with a group key or none (a plain
/// Agg over PG's Sort by the DISTINCT argument); `None` otherwise or when it cannot be (`blocked`; the riders' case passes
/// there by `distinct_fed`).
fn redeclare<'p>(a: ArenaRef, graph: &PipelineGraph<'p>, facts: &dyn Facts, g: &GroupDecl<'p>) -> Option<GroupDecl<'p>> {
    let reads = || !crate::graph::order_free(g);
    // D3374 (dict_d345_grouped_pins): a group whose every key the planner dropped (D345's `empty_no_row`) folds plain,
    // its pinned columns the plain end's kept columns (fold/kept.rs: the group's first row), its DISTINCT host-sorted;
    // the pair ends keep no first row
    let pinned = g.empty_no_row && !crate::graph::grouped(g);
    let d = (unpaired(g) || reads() || pinned).then(|| host(a, g)).flatten()?;
    blocked(a, graph, facts, &d).is_none().then_some(d)
}

/// G10 (group_core-0271): streamed pair-form group `g` (D1071) re-declared as `redeclare` does. PG feeds a streamed group's
/// DISTINCT aggregate presorted (its transfn per new value as the row comes, between the VOLATILE arguments' calls); the
/// host-sorted one runs its transitions at the group's end, so each must be quiet (`trans_acts`: no raise, no user code).
/// A stream keeps a group key, so the re-declaration is sorted. D1371 (v0.4 refusals): one PG did not presort (no Aggref
/// `aggpresorted`) sorts and folds at the group's final in PG too (process_ordered_aggregate_single, after every row's
/// arguments and before the next group's first row), as the stream hands the group on: it may act.
fn redeclare_stream<'p>(a: ArenaRef, graph: &PipelineGraph<'p>, facts: &dyn Facts, catalog: &dyn Catalog<'_>, g: &GroupDecl<'p>) -> Option<GroupDecl<'p>> {
    let d = redeclare(a, graph, facts, g)?;
    (d.form == GroupForm::Sorted && d.aggs.iter().all(|x| x.mods.distinct == 0 || !presorted_call(x) || !trans_acts(facts, catalog, x, true))).then_some(d)
}

/// The groups lowering reads in place of decompose's (graph.rs `Gx::group_of` consults it first): each pair-form group
/// the pair set cannot hold, re-declared. A plain re-declaration keeps decompose's Distinct breaker in the graph; the
/// plain end lays it out as the plain one-record region (fold/hosted.rs `region`).
pub struct Overlay<'g, 'p> {
    groups: PgVec<'g, GroupDecl<'p>>,
    /// The streamed groups (D1071) by their `Op::GroupRun` index, each re-declared or `None`.
    streams: PgVec<'g, Option<GroupDecl<'p>>>,
}

impl<'g, 'p> Overlay<'g, 'p> {
    /// The re-declared group whose records pipeline `life` produces, if any.
    pub fn group(&self, life: gravity_program::LifeId) -> Option<&GroupDecl<'p>> {
        self.groups.iter().find(|g| g.producer == life)
    }

    /// The re-declared streamed group `Op::GroupRun(i)` runs, if any.
    pub fn stream(&self, i: usize) -> Option<&GroupDecl<'p>> {
        self.streams.get(i)?.as_ref()
    }
}

/// `graph`'s overlay, `None` when no group re-declares.
pub fn regroup<'g, 'p>(a: ArenaRef, mcx: Mcx<'g>, graph: &PipelineGraph<'p>, facts: &dyn Facts, catalog: &dyn Catalog<'_>) -> Option<Overlay<'g, 'p>> {
    let groups: PgVec<'g, GroupDecl<'p>> = graph.groups.iter().filter_map(|g| redeclare(a, graph, facts, g)).collect_in(mcx);
    let mut streams: PgVec<'g, Option<GroupDecl<'p>>> = graph.streams.iter().map(|g| redeclare_stream(a, graph, facts, catalog, g)).collect_in(mcx);
    // D1018 (nestloop, regress aggregates#527): a streamed group an embedded inner runs names that inner (pipes.rs
    // `stream_producers`; decompose names graph pipelines only)
    for (i, k) in crate::pipes::Pipes::of(&graph.pipelines).stream_producers(&graph.streams, mcx) {
        let g = streams[i].as_ref().unwrap_or(&graph.streams[i]);
        let mut c = copy(a, g, g.form, g.keys.len(), g.aggs.iter().copied());
        c.producer = gravity_program::LifeId(k as u16);
        streams[i] = Some(c);
    }
    (!groups.is_empty() || streams.iter().any(Option::is_some)).then_some(Overlay { groups, streams })
}

/// D1596 (V4R-543): a presorted DISTINCT beside riders reading the row order, folded per row (plain groups).
pub use crate::ops::group::presorted::presorted;
