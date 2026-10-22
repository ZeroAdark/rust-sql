//! D1596 (W1-D1596-PRESORTED, V4R-543; D1597 (a) for plain groups): split from src/regroup.rs (lib.rs declares it), M6-AGG,
//! budget 150 lines. PG 18.6 sorts a plain Agg's input by an aggpresorted DISTINCT and runs each row's transitions in
//! aggno order beside the riders (EEOP_AGG_PRESORTED_DISTINCT_SINGLE / _MULTI); `presorted` admits the groups v4 feeds
//! the same rows in the same order, so fold/plain.rs folds them per row and the host (hosts v4/m6_agg_presorted.rs) skips a
//! value equal to the last one advanced. regroup.rs `blocked` asks it before naming the rider refusal. G10's
//! `in_sort_order` (regroup.rs `fed_in_order`) shares its Sort walk.
use gravity_abi::breaker::{BreakerKind, SlotKind};
use gravity_decompose::ops::shared::agg::{AggDecl, GroupDecl, GroupForm, NONE};
use gravity_decompose::ops::shared::agg_class::Trans;
use gravity_decompose::graph::{Op, Source};
use gravity_decompose::ops::shared::sort::{SortDesc, SortForm};
use gravity_decompose::catalog::{Am, Facts};
use gravity_decompose::facts::order::presorted_call;
use gravity_decompose::effects;
use gravity_decompose::PipelineGraph;
use types_nodes::primnodes::OUTER_VAR;
use types_nodes::Node;

use crate::frame::iface::breaker_of;
use crate::ops::shared::agg::sorted;

/// D1596's names (W1-D1596-PRESORTED): a rider beside a DISTINCT PG sorts its input for; the presorted form's two refusals.
const RIDER: &str = "a rider whose value reads the row order beside a DISTINCT (PG's sorted input)";
const USER_KEY: &str = "D1596: a presorted DISTINCT whose sort or equality calls user code or whose equal keys can differ (PG's comparator calls and tie order)";
const COMPUTES: &str = "D1596: beside a presorted DISTINCT, an aggregate argument or FILTER that computes, or a native fold that can raise (PG raises at its row between the per-row transitions)";

/// D1596 (D1597 (a), plain groups): whether v4 feeds plain group `g` the rows PostgreSQL 18.6 feeds its Agg, so each
/// aggpresorted DISTINCT folds per row beside its riders in PG's order (nodeAgg's per-row transition in aggno order: FILTER,
/// arguments, the strict check, EEOP_AGG_PRESORTED_DISTINCT_SINGLE / _MULTI, the transfn; the finalfns after every row). Ok
/// when (a) `g` is plain with no set, two-pass, hint or kept column, its aggregates in rising aggno; (b) every host-sorted
/// one is an aggpresorted DISTINCT under built-in operators (nothing sorts at the final: D1598 is not widened); (c) the group
/// reads, through projections alone, the drain of a full Sort (one arm: the Sorted spec keeps the arena serial, so exec
/// runs the program width 1 and the drain is in sort order at every width); (d) each Sort key orders by a built-in
/// operator of its type's default family, the type's procs act on nothing (the effects home's `of_type`: a record, a user
/// type or a domain over one stays out) and its collation compares by bytes; (e) every aggregate argument
/// and FILTER is a Sort key column or a constant, and (f) every aggregate folds by PG's own transfn (hosted: slot `NONE`,
/// or the interval sum that raises as it: `pg_step`) or by a native class that never raises (the effects home's
/// `quiet_step`): tied rows then read alike and nothing but the transitions runs
/// between them, so any tie order gives PG's calls, NOTICEs and errors, and a tie's member (numeric 1.0 / 1.00, float8
/// -0 / 0, interval '1 day' / '24 hours') is some input member equal under the type's `=`, which the equal-member rule
/// (DISTINCT, min / max and group keys) accepts; but beside a rider that shows its rows in tie order
/// (`array_agg(n)`: not order-free), each Sort key's equal values must be one image (typesem `one_image`). S1 (design review): class.float's average
/// keeps its overflow to the final (D432), num_word its D299 22003, so neither rides here.
pub fn presorted(graph: &PipelineGraph<'_>, facts: &dyn Facts, g: &GroupDecl<'_>) -> Result<(), &'static str> {
    check(graph, facts, g).map(|_| ())
}

/// `presorted`'s answer, with the feeding Sort's pipeline when (d) holds only in PostgreSQL's own order (D1596, D3375's
/// raise-order sort rows): a key whose sort or equality calls user code or whose equal keys can differ (a domain, enum,
/// bpchar, numeric or interval key; a nondeterministic collation; a non-default family's order) gives PG's comparator calls
/// and tie order when the Sort reads its rows in PG's arrival order (`arrival`) and orders them by PG's qsort over them
/// (sort/exact_feed.rs, through frame/iface `presorted_exact`): the group then reads PG's rows in PG's order, ties too, and
/// each per-row DISTINCT check calls the aggregate's own equality (hosts presorted.rs), as nodeAgg does.
pub fn check(graph: &PipelineGraph<'_>, facts: &dyn Facts, g: &GroupDecl<'_>) -> Result<Option<usize>, &'static str> {
    let plain = g.form == GroupForm::Plain && g.set.is_none() && g.two_pass.is_none() && g.hints.iter().all(Option::is_none) && pins_ok(g);
    let fed = |d: &AggDecl<'_>| d.mods.distinct > 0 && presorted_call(d);
    let (mut sorts, rising) = (g.aggs.iter().filter(|d| sorted(d)), g.aggs.windows(2).all(|w| w[0].aggno < w[1].aggno));
    if !plain || !rising || !g.aggs.iter().any(sorted) || !sorts.all(fed) {
        return Err(RIDER);
    }
    let (o, s) = group_feed_at(graph, g.producer.0 as usize, false).ok_or(RIDER)?;
    let mut user = false;
    let keys = &s.keys[..];
    for d in g.aggs.iter() {
        let r = d.node.as_aggref().ok_or(RIDER)?;
        let reads = r.args.iter().chain(r.aggdirectargs.iter()).chain(d.filter);
        for n in reads {
            read(n, keys)?;
        }
        if d.slot != NONE && !crate::effects::visible::topn::quiet_step(d.class) && !pg_step(d) {
            return Err(COMPUTES);
        }
        let mut ops = r.aggdistinct.iter().chain(r.aggorder.iter()).filter_map(|n| n.as_sort_group_clause());
        user |= sorted(d) && (!d.coll.bytes_eq || ops.any(|c| !gravity_decompose::facts::order::default_order(facts, c.sortop)));
    }
    let typ = |x: i16| s.tlist.iter().nth(usize::try_from(x).ok()?.checked_sub(1)?).and_then(|t| t.as_target_entry()).and_then(|t| t.expr.as_var()).map(|v| v.vartype);
    // a rider that shows its rows in tie order (a collection, a user aggregate: not order-free) needs one image per tie
    let shows = g.aggs.iter().any(|d| !fed(d) && !gravity_decompose::facts::order::order_free(d.class, &d.mods));
    let one = |x: &gravity_decompose::ops::shared::sort::SortKey| x.coll.bytes_eq && typ(x.resno).is_some_and(|t| !effects::of_type(facts, t).acts && (!shows || gravity_fn::typesem::keysem::eq::tie_image(t)));
    // PM2: a key its type's default btree family orders (neither `own_family` nor `no_proc`: a family holds one ordering
    // operator per strategy and type pair, so a user operator over a builtin type is never the default family's) over a
    // type whose compare effects knows (`one`), not an oid range
    // a key no btree proc orders keeps its name (PG raises at the Sort's first call)
    // D3374 (D345): a kept pinned column shows the Sort's first row, which rows tied on every Sort key decide: PG's own
    // order, ties too (the exact feed)
    let user = user || kept(g);
    match (user || !s.keys.iter().all(|x| !x.own_family && one(x)), s.keys.iter().all(|x| x.no_proc.is_none())) {
        (false, true) => Ok(None),
        (true, true) if arrival(graph, o) => Ok(Some(o)),
        _ => Err(USER_KEY),
    }
}

/// D1596 (D3375): Sort pipeline `o` reads its rows in PostgreSQL's arrival order: a heap scan's (taken serially, in block
/// and line order: sort/exact_feed.rs's `px.serial`) or a VALUES list's, through filters and projections alone, outside
/// every rerun set (a member re-runs alone, its rows another pass's).
fn arrival(graph: &PipelineGraph<'_>, o: usize) -> bool {
    let p = &graph.pipelines[o];
    let Some((Op::Sort(_), before)) = p.ops.split_last() else { return false };
    matches!(p.source, Source::Range { am: Am::Heap, .. } | Source::Values { .. })
        && before.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_)))
        && p.union.is_none()
        && !graph.reruns.iter().any(|r| r.pipelines.contains(&(o as u16)))
}

/// `presorted`'s conditions but (d)'s: plain group `g` fed in its Sort's order whose aggregates (e) read Sort key columns or
/// constants and (f) fold quietly or as PG's transfn; regroup.rs `distinct_fed` leaves the groups it holds to the per-row
/// form, which also asks (d).
pub fn shaped(graph: &PipelineGraph<'_>, g: &GroupDecl<'_>) -> bool {
    let plain = g.form == GroupForm::Plain && g.set.is_none() && g.two_pass.is_none() && g.hints.iter().all(Option::is_none) && pins_ok(g);
    let fed = |d: &AggDecl<'_>| d.mods.distinct > 0 && presorted_call(d);
    let rising = g.aggs.windows(2).all(|w| w[0].aggno < w[1].aggno);
    if !plain || !rising || !g.aggs.iter().any(sorted) || !g.aggs.iter().filter(|d| sorted(d)).all(fed) {
        return false;
    }
    let Some(s) = feed(graph, g.producer.0 as usize) else { return false };
    let keys = &s.keys[..];
    g.aggs.iter().all(|d| {
        let Some(r) = d.node.as_aggref() else { return false };
        let reads = r.args.iter().chain(r.aggdirectargs.iter()).chain(d.filter).all(|n| read(n, keys).is_ok());
        reads && (d.slot == NONE || crate::effects::visible::topn::quiet_step(d.class) || pg_step(d))
    })
}

/// D3374 (D345): whether plain group `g` keeps its first row's pinned columns (fold/kept.rs: every key the planner
/// dropped, a pinned column decompose did not key). PostgreSQL's representative row is its Agg's first input row, the
/// first row of the Sort it put under the Agg for a presorted DISTINCT, so the per-row form serves such a group only
/// fed in that Sort's exact order (`check`'s exact feed), its first row read by the plain end (kept.rs `refuse_unfed`).
fn kept(g: &GroupDecl<'_>) -> bool {
    g.form == GroupForm::Plain && g.empty_no_row && !g.pinned.is_empty()
}

/// The pins the per-row form takes: none (a group with a key, or every key dropped and nothing kept is not one of its
/// shapes), or a kept group's (`kept`).
fn pins_ok(g: &GroupDecl<'_>) -> bool {
    !g.empty_no_row && g.pinned.is_empty() || kept(g)
}

/// (f): a native fold that raises where PG's transfn raises, row by row in the fold's order, as the hosted fold does: the
/// interval sum (class_interval_sum.rs: without the recheck licence its add is PG's `finite_interval_pl`; under it a state
/// that may overflow raises and the statement re-runs in plan order, without the licence).
fn pg_step(d: &AggDecl<'_>) -> bool {
    matches!(d.trans, Trans::Abi(SlotKind::IntervalSum))
}

/// (e): argument or FILTER `n` a Sort key column (`keys`: their resnos are the Agg's `OUTER_VAR` numbers under the Sort's
/// identity list) or a constant, through RelabelType; a column outside the keys reads the tie order (the old name).
fn read(n: Node<'_>, keys: &[gravity_decompose::ops::shared::sort::SortKey]) -> Result<(), &'static str> {
    crate::stack_check();
    let n = n.as_target_entry().map_or(n, |t| t.expr);
    match (n.as_relabel_type(), n.as_var()) {
        (Some(r), _) => read(r.arg, keys),
        (_, Some(v)) if v.varno == OUTER_VAR && v.varlevelsup == 0 && keys.iter().any(|x| x.resno == v.varattno) => Ok(()),
        (_, Some(_)) => Err(RIDER),
        _ if n.as_const().is_some() => Ok(()),
        _ => Err(COMPUTES),
    }
}

/// (c): the full Sort whose drain the group producer pipeline `k` reads through projections alone, `None` for any other
/// feed: a union or Append arm, another op, another breaker, an incremental or bounded sort, a Sort that projects.
fn feed<'g, 'p>(graph: &'g PipelineGraph<'p>, k: usize) -> Option<&'g SortDesc<'p>> {
    group_feed(graph, k, false)
}

/// D3374 (effects `ordered_fed_acting`): `in_sort_order` through a full Sort only, which PG reads whole before its first
/// row (an incremental one reads its input between the Agg's rows). D1371 (v0.4 refusals): the ops between the Sort's
/// drain and the group may filter too (a SubqueryScan's qual over an ordered subquery kept by OFFSET 0): a filter drops
/// rows and keeps the others' order, and PostgreSQL evaluates it per row before the Agg advances that row, as the
/// pipeline does.
pub fn full_feed(graph: &PipelineGraph<'_>, g: &GroupDecl<'_>) -> bool {
    let k = g.producer.0 as usize;
    let Some(p) = graph.pipelines.get(k) else { return false };
    let stream = graph.streams.iter().position(|s| core::ptr::eq(s, g));
    match stream.and_then(|i| p.ops.iter().position(|o| matches!(o, Op::GroupRun(r) if *r as usize == i))) {
        Some(j) => sort_feed_at(graph, k, j, false, true).is_some(),
        None => stream.is_none() && group_feed_at_by(graph, k, false, true).is_some(),
    }
}

/// `group_feed` with the Sort's own pipeline.
fn group_feed_at<'g, 'p>(graph: &'g PipelineGraph<'p>, k: usize, incremental: bool) -> Option<(usize, &'g SortDesc<'p>)> {
    group_feed_at_by(graph, k, incremental, false)
}

/// D1371 (v0.4 refusals): pipeline `o` (a CTE's store fill) reads, through filters and projections, the drain of a full
/// Sort, so the store holds that Sort's output order (ties as it left them).
pub fn sorted_fill(graph: &PipelineGraph<'_>, o: usize) -> bool {
    group_feed_at_by(graph, o, false, true).is_some()
}

/// `group_feed_at`; with `filters`, through filters too (`full_feed`).
fn group_feed_at_by<'g, 'p>(graph: &'g PipelineGraph<'p>, k: usize, incremental: bool, filters: bool) -> Option<(usize, &'g SortDesc<'p>)> {
    let ps = &graph.pipelines;
    let (p, Some(e)) = (ps.get(k)?, breaker_of(&ps.get(k)?.desc)) else { return None };
    let arm = |j: usize| ps.get(j).and_then(|q| breaker_of(&q.desc)).is_some_and(|f| f.slot == e.slot && f.kind == e.kind);
    if (k > 0 && arm(k - 1)) || arm(k + 1) {
        return None;
    }
    sort_feed_at(graph, k, p.ops.len(), incremental, filters)
}

/// `feed` of the group whose records producer pipeline `k` fills; with `incremental`, an Incremental Sort's too (its
/// breaker sorts by every key, so its drain is in the full key order).
fn group_feed<'g, 'p>(graph: &'g PipelineGraph<'p>, k: usize, incremental: bool) -> Option<&'g SortDesc<'p>> {
    group_feed_at(graph, k, incremental).map(|(_, s)| s)
}

/// The full Sort whose drain pipeline `k` reads (no union: one producer) through its first `upto` ops, all projections.
fn sort_feed<'g, 'p>(graph: &'g PipelineGraph<'p>, k: usize, upto: usize, incremental: bool) -> Option<&'g SortDesc<'p>> {
    sort_feed_at(graph, k, upto, incremental, false).map(|(_, s)| s)
}

/// `sort_feed` with the Sort's own pipeline; with `filters`, through filters and projections.
fn sort_feed_at<'g, 'p>(graph: &'g PipelineGraph<'p>, k: usize, upto: usize, incremental: bool, filters: bool) -> Option<(usize, &'g SortDesc<'p>)> {
    let ps = &graph.pipelines;
    let p = ps.get(k)?;
    let Source::Breaker { slot } = p.source else { return None };
    if p.union.is_some() || !p.ops.get(..upto)?.iter().all(|o| matches!(o, Op::Project(_)) || filters && matches!(o, Op::Filter(_))) {
        return None;
    }
    // Gx::life_of's rule: the declared life, else the nearest earlier producer of the slot
    let at = p.desc.consumes.iter().position(|&x| x == slot).and_then(|j| p.desc.consumes_from.get(j)).map(|l| l.0 as usize);
    let o = at.or_else(|| (0..k).rev().find(|&i| breaker_of(&ps[i].desc).is_some_and(|b| b.slot == slot)))?;
    let (Some(b), Some(Op::Sort(s))) = (breaker_of(&ps[o].desc), ps[o].ops.last()) else { return None };
    let ident = s.tlist.iter().enumerate().all(|(i, t)| t.as_target_entry().and_then(|t| t.expr.as_var()).is_some_and(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno as usize == i + 1));
    (b.kind == BreakerKind::Sort && (s.presorted == 0 || incremental) && matches!(s.form, SortForm::Runs) && ident).then_some((o, s))
}

/// G10 (D458): whether group `g` reads, through projections alone, the drain of the Sort PostgreSQL put under its Agg (a
/// full or incremental sort, one producer), so its rows arrive in that Sort's output order: a plain or sorted group's
/// records producer (`group_feed`), or the pipeline a streamed group's `Op::GroupRun` runs in, up to that op (D1071).
pub fn in_sort_order(graph: &PipelineGraph<'_>, g: &GroupDecl<'_>) -> bool {
    let k = g.producer.0 as usize;
    let Some(p) = graph.pipelines.get(k) else { return false };
    let stream = graph.streams.iter().position(|s| core::ptr::eq(s, g));
    match stream.and_then(|i| p.ops.iter().position(|o| matches!(o, Op::GroupRun(r) if *r as usize == i))) {
        Some(j) => sort_feed(graph, k, j, true).is_some(),
        None => stream.is_none() && group_feed(graph, k, true).is_some(),
    }
}
