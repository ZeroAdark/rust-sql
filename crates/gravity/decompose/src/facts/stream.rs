//! The streamed-group / stop-early home (group_core-0259, main's design 20:44Z): whether rows PostgreSQL streams from
//! ordered input reach a consumer above that stops early, and where that consumer's demand stops the producer below. One
//! decision, computed once, top-down, at the stopping consumer (a counted LIMIT today; a nest's first match, EXISTS, a
//! cursor are its later consumers), handed to the node that keeps the stop (`PipelineGraph::stops`, read by lowerv4).
//!
//! The first form: `Limit → Unique → Incremental Sort` (a SELECT DISTINCT ... ORDER BY ... LIMIT over input sorted by a
//! prefix of the DISTINCT keys). PostgreSQL's Unique passes no bound down (`ExecSetTupleBound` stops at it), so the
//! Incremental Sort loads unbounded batches (nodeIncrementalSort.c: at least DEFAULT_MIN_GROUP_SIZE = 32 rows, then rows
//! while the 32nd row's prefix holds; past 64 rows it hands on the batch's earlier prefix groups and reads the rest of the
//! last one), and the Limit stops pulling once the Unique handed on count + offset rows: the code below the sort runs for
//! exactly the rows those batches read. Gravity's sort is a breaker that reads every row first, so where that code may
//! raise the Limit refused it (ops/limit.rs `AHEAD`); with the stop the sort's producer models PostgreSQL's batches and
//! ends where PostgreSQL's last load ends (lowerv4 ops/sort/incremental.rs `Batches`), and the Limit treats the sort as
//! one that keeps the stop itself (`limit::ahead`'s `kept`).
//!
//! The group form (the stop road, `group_stop`): a sorted GroupAggregate with no HAVING right under the Limit, or under a
//! top Incremental Sort presorted on every group key, over an unbounded Incremental Sort whose leading keys are the group
//! keys and whose presorted prefix lies inside them (each prefix group one group or more). The Limit
//! takes count + offset groups; a top's bounded batches take one group row more (every prefix group of the top is one
//! group row, and its last batch pulls the row it carries over); the GroupAggregate returns a group when it fetches the
//! next group's first row (fetched, never advanced). Only where nothing below the sort raises or acts: there the stop is a
//! cost and changes no visible effect, so the sort's producer ends at the first row past the groups PostgreSQL advances
//! (lowerv4 `Batches::groups` hands each prefix group on as it closes), where it read every row before; the group above
//! folds those groups and the one row past them, a part of what its breaker form folded before (no group PostgreSQL
//! advances is left out).
//!
//! The streamed form (`agg_stops`): a sorted GroupAggregate right under a counted LIMIT over an ordered input streams
//! (D1071's form, PostgreSQL's) where its fold-ahead form would read every input row first.
//!
//! The second form (`top_stops`): a counted top over a presorted prefix (an Incremental Sort right under a LIMIT) ends its
//! own walk once its bound's prefix group closes (lowerv4 runs/topk_rows.rs `Stop`, V4R-179), so what feeds that walk one
//! row at a time reads no further: a Merge Append's pull form under it (mergejoin r16 / r17: rnd-0028, rnd-4082) reads
//! PostgreSQL's rows where its breaker form read every arm whole.
use types_nodes::nodes_enums::LimitOption;
use types_nodes::plannodes::{Agg, IncrementalSort, Limit};
use types_nodes::primnodes::{AGGKIND_NORMAL, OUTER_VAR};
use types_nodes::{Node, NodeTag};

use crate::effects::{of_plan, Cx};
use crate::graph::{Bound, Col, Op, Order, OrderKey, MAX_ORDER_KEYS};
use crate::ledger::Kind;
use crate::ops::shared::agg_class::{Class, Collect};
use crate::ops::shared::agg::KeyDecl;
use crate::ops::shared::sort::SortForm;
use crate::catalog::typefacts::facts_of;
use crate::walk::{Builder, Open};

/// A stop handed to the sort below a stopping consumer: plan node `sort` (an Incremental Sort) ends its producer once the
/// rows PostgreSQL's batches read hold `need` distinct rows by all of its keys (the Unique above hands on count + offset),
/// or, with `groups` (the group form), at the first row past `need` prefix groups.
#[derive(Clone, Copy)]
pub struct Stop<'p> {
    pub sort: i32,
    pub need: u64,
    /// The group form: each prefix group the sort hands on holds one group or more of the sorted GroupAggregate above it,
    /// and nothing below the sort raises or acts (a stop at or past the groups PostgreSQL advances is then as good as the
    /// exact one).
    pub groups: bool,
    /// topn_core-0240: the Unique's columns are exactly the sort's presorted prefix (a `DISTINCT ON` over the leading
    /// keys of its ORDER BY): each prefix group hands on one distinct row, whatever its rows past the prefix.
    pub per_group: bool,
    /// agg_shop-0033: the consumer is a sorted GroupAggregate counted by distinct rows (not `groups`), which folds every
    /// row of each group: the sort keeps every row it reads (no D2690 cut of a group's repeated rows).
    pub folds: bool,
    /// group lane (the LIMIT-over-sorted-group perf debt): nothing below the sort raises or acts, so a stop past
    /// PostgreSQL's batches is as good as the exact one: a counted stop's distinct rows known only as a range (D2432, a
    /// prefix group split by a run put) that may meet `need` go on reading instead of leaving the statement UNSUPPORTED.
    pub late: bool,
    /// D3376 (slow_s2_window_stop w5b): the window form over code below the sort that may raise counts the rows handed on,
    /// not distinct rows: PostgreSQL's WindowAgg that only numbers its rows pulls exactly the rows the Limit takes (it spools
    /// to the current row; a numbering function looks back only), so the stop ends at PostgreSQL's last batch, exact.
    pub rows: bool,
    /// The window form's `f = 1` qual (`window_stop`, ops/window.rs `first_rows_only`): the Const the prefix-group count
    /// holds for. The content key holds it as a word, so lowerv4 files the stop as a literal guard on it (`lit_decide`): a
    /// statement of the key with another constant lowers again.
    pub one: Option<Node<'p>>,
}

/// PostgreSQL's AggStrategy AGG_PLAIN (nodes.h).
const AGG_PLAIN: u32 = 0;
/// PostgreSQL's AggStrategy AGG_SORTED (nodes.h).
const AGG_SORTED: u32 = 1;
/// PostgreSQL's AggStrategy AGG_HASHED (nodes.h).
const AGG_HASHED: u32 = 2;

/// The stop the Limit `l` hands down, or None: a constant count of at least 1 and a constant offset (or none), no WITH
/// TIES, a Unique with no qual right below over an Incremental Sort with a presorted prefix, the Unique's columns the
/// sort's keys (the same set, every key under its type's default btree family: the Unique's equality is the sort's) or
/// exactly its presorted prefix keys (a `DISTINCT ON` over the ORDER BY's leading keys, `Stop::per_group`), and no code
/// below the sort that acts (acting code keeps today's refusal; code that may raise needs the stop, and with none it is a
/// cost, `Stop::late`).
/// The group form after it (`group_stop`).
pub fn group_streams<'p>(b: &Builder<'p, '_>, l: &'p Limit<'p>) -> Option<Stop<'p>> {
    if l.limitOption == LimitOption::LIMIT_OPTION_WITH_TIES {
        return None;
    }
    let count = match crate::ops::limit_bounds(l).0 {
        Bound::Const(c) if c >= 1 => c as u64,
        _ => return None,
    };
    let offset = match crate::ops::limit_bounds(l).1 {
        Bound::All => 0,
        Bound::Const(o) if o >= 0 => o as u64,
        _ => return None,
    };
    let child = l.plan.lefttree?;
    match child.as_unique() {
        Some(_) => unique_stop(b, child, count.saturating_add(offset)),
        None => group_stop(b, child, count.saturating_add(offset)).or_else(|| window_stop(b, child, count.saturating_add(offset))),
    }
}

/// The first form: the Unique `child` over its Incremental Sort hands on `need` distinct rows.
fn unique_stop<'p>(b: &Builder<'p, '_>, child: Node<'p>, need: u64) -> Option<Stop<'p>> {
    let u = child.as_unique().filter(|u| u.plan.qual.is_nil())?;
    let inc = u.plan.lefttree?.as_incremental_sort()?;
    let s = &inc.sort;
    let n = s.numCols as usize;
    let keys = s.sortColIdx.get(..n)?;
    let uniq = u.uniqColIdx.get(..u.numCols.max(0) as usize)?;
    let same = |set: &[i16]| uniq.len() == set.len() && uniq.iter().all(|c| set.contains(c)) && set.iter().all(|c| uniq.contains(c));
    // topn_core-0240: a Unique over exactly the presorted prefix keys hands on one row per prefix group (`per_group`)
    let pre = (inc.nPresortedCols.max(0) as usize).min(n);
    let (all, per_group) = (same(keys), pre < n && same(&keys[..pre]));
    let default = s.sortOperators.get(..n).is_some_and(|ops| ops.iter().all(|&op| super::order::default_order(b.facts, op)));
    if pre < 1 || !s.plan.qual.is_nil() || !(all || per_group) || !default {
        return None;
    }
    // where code below the sort may raise the stop is needed (PostgreSQL's batches, no row past them); where none may it is
    // a cost (rnd-0237: a DISTINCT ON ... LIMIT 100 over an index's prefix order read the whole table where PostgreSQL
    // read 100 prefix groups), and a stop past PostgreSQL's batches is then as good as the exact one (`late`)
    let below = of_plan(&Cx::new(b.stmt, b.facts, b.arena), s.plan.lefttree?);
    if below.acts {
        return None;
    }
    Some(Stop { sort: s.plan.plan_node_id, need, groups: false, per_group, folds: false, late: !below.may_raise, rows: false, one: None })
}

/// The group form: `child` (the Limit's) a sorted GroupAggregate, or a top Incremental Sort presorted on every group key
/// over one (`top_on_keys`), the Limit taking `c` rows; the GroupAggregate has no HAVING, no grouping sets and no hint, its
/// input an unbounded Incremental Sort with no qual whose leading keys are the group keys and whose presorted prefix lies
/// inside them (one group or more per prefix group), each prefix key under its type's default btree family (the group's
/// equality is the sort's); nothing below the sort acts, and where it may raise the count of the Unique form (`folds`).
fn group_stop<'p>(b: &Builder<'p, '_>, child: Node<'p>, c: u64) -> Option<Stop<'p>> {
    let (a, rows) = match child.as_incremental_sort() {
        Some(t) => {
            let a = t.sort.plan.lefttree?.as_agg()?;
            (top_on_keys(t, a).then_some(a)?, c.saturating_add(1))
        }
        None => (child.as_agg()?, c),
    };
    let ngrp = a.numCols.max(0) as usize;
    if a.aggstrategy != AGG_SORTED || ngrp == 0 || !a.plan.qual.is_nil() || !a.groupingSets.is_nil() || !a.chain.is_nil() || a.gravity_rewrite.is_some() {
        return None;
    }
    let grp = a.grpColIdx.get(..ngrp)?;
    let window = a.plan.lefttree?.as_window_agg();
    let inc = match window {
        Some(w) => w.plan.lefttree?.as_incremental_sort()?,
        None => a.plan.lefttree?.as_incremental_sort()?,
    };
    let s = &inc.sort;
    let (n, pre) = (s.numCols.max(0) as usize, inc.nPresortedCols.max(0) as usize);
    // a group column as the sort's column: through the window's target list (a plain outer column) when one lies between
    let col = |g: i16| match window {
        Some(w) => outer(&w.plan.targetlist, g),
        None => Some(g),
    };
    // the sort's leading keys are the group keys (PostgreSQL's group pathkeys lead its input's) and its presorted prefix
    // lies inside them: each prefix group holds one group or more, so counting prefix groups stops no earlier
    let lead = s.sortColIdx.get(..n.min(ngrp))?;
    let same = lead.len() == ngrp && lead.iter().all(|&c| grp.iter().any(|&g| col(g) == Some(c))) && grp.iter().all(|&g| col(g).is_some_and(|c| lead.contains(&c)));
    if pre < 1 || pre > ngrp || !s.plan.qual.is_nil() || !same {
        return None;
    }
    if !s.sortOperators.get(..pre).is_some_and(|ops| ops.iter().all(|&op| super::order::default_order(b.facts, op))) {
        return None;
    }
    // wbig-0021 (perf): a WindowAgg between the group and the sort hands each prefix group on whole and adds no row when it
    // has no qual, its partitions lie inside prefix groups (PARTITION BY holds every presorted key; the planner sorts by the
    // partition's own clauses, so its equality is the sort's) and each partition keeps its first row (`partitions_never_empty`):
    // every prefix group then reaches the group as one group or more, as without it. Its rows past PostgreSQL's last batch
    // are computed too (the eager fill reads the stopped run whole), so only where it only numbers rows
    // (`numbering_only`) and nothing below the sort may raise or acts.
    if let Some(w) = window {
        let part = w.partColIdx.get(..w.partNumCols.max(0) as usize)?;
        let inside = s.sortColIdx.get(..pre).is_some_and(|keys| keys.iter().all(|c| part.contains(c)));
        if !w.plan.qual.is_nil() || !inside || !crate::ops::window::partitions_never_empty(w) || !crate::ops::window::numbering_only(w) {
            return None;
        }
    }
    let below = of_plan(&Cx::new(b.stmt, b.facts, b.arena), s.plan.lefttree?);
    if below.acts || (window.is_some() && below.may_raise) {
        return None;
    }
    // agg_shop-0033: the Unique form's count, PostgreSQL's batches exactly: the GroupAggregate returns its `rows`-th group
    // when it fetches the next group's first row, so PostgreSQL's loads end at the batch holding the (rows + 1)-th distinct
    // row by the group keys, which are every sort key, each under its type's default btree family (the group's equality
    // is the sort's). `rows` is the Limit's count right under it, one more under a top presorted on the group keys (its
    // bounded batches pull the row they carry over: nodeIncrementalSort.c, every group row its own prefix group; rnd-2339).
    // Needed where code below the sort may raise; also taken where it does not, as it reads no more than the prefix-group
    // count: a prefix group holding many groups (`GROUP BY customer_id, status, channel` over an index on customer_id,
    // LIMIT 40) read 40 prefix groups where PostgreSQL read 2
    let all = n == ngrp && s.sortOperators.get(..n).is_some_and(|ops| ops.iter().all(|&op| super::order::default_order(b.facts, op)));
    if all && window.is_none() {
        return Some(Stop { sort: s.plan.plan_node_id, need: rows.saturating_add(1), groups: false, per_group: false, folds: true, late: !below.may_raise, rows: false, one: None });
    }
    (!below.may_raise).then_some(Stop { sort: s.plan.plan_node_id, need: rows, groups: true, per_group: false, folds: false, late: true, rows: false, one: None })
}

/// The window form (S2, ci-0065: rank() OVER (PARTITION BY cat ORDER BY amt DESC) ... ORDER BY cat, amt DESC, id LIMIT 5
/// over an index on cat): `child` (the Limit's) a WindowAgg, or a top Incremental Sort over one presorted on every key of
/// the sort below the window (`top_on_sort`), the Limit taking `c` rows. The WindowAgg has no qual and only numbers its
/// rows (`numbering_only`: computing those numbers over rows PostgreSQL never reads shows nothing); its input is an
/// unbounded Incremental Sort with no qual whose presorted prefix lies inside the PARTITION BY, so each batch PostgreSQL
/// loads (whole prefix groups) holds whole partitions and the numbers over it are PostgreSQL's; nothing below the sort
/// raises or acts, so a stop past PostgreSQL's last batch is as good as the exact one (`late`). With no run condition every
/// row the window reads reaches the Limit (or the top): the stop counts PostgreSQL's batches by distinct rows by every sort
/// key (`need` of them are `need` rows or more; under the top one more, the row that closes its last prefix group), every
/// row kept (`folds`). With a run condition that passes each partition's first row (`partitions_never_empty`) and no top,
/// each prefix group hands on one row or more: the stop counts prefix groups; so too under a SubqueryScan whose qual is
/// `f = 1` (`Stop::one`, guarded on its constant in lowerv4). Anything else reads every row as before.
/// D3376: where code below the sort may raise, only the no-run-condition form with no top stops, exactly: the stop counts
/// the rows handed on (`Stop::rows`), PostgreSQL's WindowAgg pulling exactly the Limit's `c` (`late` off).
fn window_stop<'p>(b: &Builder<'p, '_>, child: Node<'p>, c: u64) -> Option<Stop<'p>> {
    // the `f = 1` form (wcore-0249: ... WHERE rn = 1 ORDER BY customer_id LIMIT 100): a SubqueryScan right under the Limit
    // that hands on plain columns of the WindowAgg below it and whose qual keeps each partition's first row
    // (`first_rows_only`): each prefix group hands on one row or more, as under a run condition that passes it
    let (child, one) = match child.as_subquery_scan() {
        Some(sq) => {
            let sub = sq.subplan?;
            let plain = sq.scan.plan.targetlist.iter().filter_map(|t| t.as_target_entry()).all(|t| t.expr.as_var().is_some_and(|v| v.varlevelsup == 0));
            let k = crate::ops::window::first_rows_only(sub.as_window_agg()?, sq.scan.scanrelid, &sq.scan.plan.qual)?;
            (plain.then_some(sub)?, Some(k))
        }
        None => (child, None),
    };
    let (w, top) = match child.as_incremental_sort() {
        Some(t) => (t.sort.plan.lefttree?.as_window_agg()?, Some(t)),
        None => (child.as_window_agg()?, None),
    };
    let inc = w.plan.lefttree?.as_incremental_sort()?;
    let s = &inc.sort;
    let (n, pre) = (s.numCols.max(0) as usize, inc.nPresortedCols.max(0) as usize);
    let keys = s.sortColIdx.get(..n)?;
    let part = w.partColIdx.get(..w.partNumCols.max(0) as usize)?;
    let inside = pre >= 1 && pre <= n && keys[..pre].iter().all(|c| part.contains(c));
    let default = |k: usize| s.sortOperators.get(..k).is_some_and(|ops| ops.iter().all(|&op| super::order::default_order(b.facts, op)));
    if !inside || !s.plan.qual.is_nil() || !w.plan.qual.is_nil() || !crate::ops::window::numbering_only(w) {
        return None;
    }
    let below = of_plan(&Cx::new(b.stmt, b.facts, b.arena), s.plan.lefttree?);
    if below.acts {
        return None;
    }
    if w.runCondition.is_nil() && one.is_none() {
        if !default(n) {
            return None;
        }
        // D3376: code below the sort that may raise reads no row past PostgreSQL's batches: the Limit's `c` rows counted
        // exactly (`Stop::rows`), with no top (whose own batches pull past the window's rows)
        if below.may_raise {
            return top.is_none().then_some(Stop { sort: s.plan.plan_node_id, need: c, groups: false, per_group: false, folds: true, late: false, rows: true, one: None });
        }
        let rows = match top {
            Some(t) if top_on_sort(t, w, keys) => c.saturating_add(1),
            Some(_) => return None,
            None => c,
        };
        return Some(Stop { sort: s.plan.plan_node_id, need: rows, groups: false, per_group: false, folds: true, late: true, rows: false, one: None });
    }
    let groups = top.is_none() && default(pre) && crate::ops::window::partitions_never_empty(w) && !below.may_raise;
    groups.then_some(Stop { sort: s.plan.plan_node_id, need: c, groups: true, per_group: false, folds: false, late: true, rows: false, one })
}

/// Whether top Incremental Sort `t` (no qual) over WindowAgg `w` is presorted on exactly the keys `keys` of the sort below
/// the window, in their order: each presorted key a column of `t`'s input that is `w`'s plain output of that sort column.
/// The top's prefix groups are then the distinct rows the window form counts, so one distinct row past its bound's last
/// closes it.
fn top_on_sort<'p>(t: &'p IncrementalSort<'p>, w: &'p types_nodes::plannodes::WindowAgg<'p>, keys: &[i16]) -> bool {
    let pre = t.nPresortedCols.max(0) as usize;
    let Some(cols) = t.sort.sortColIdx.get(..pre) else { return false };
    if pre != keys.len() || !t.sort.plan.qual.is_nil() {
        return false;
    }
    let mapped = |i: usize| outer(&t.sort.plan.targetlist, cols[i]).and_then(|r| outer(&w.plan.targetlist, r));
    (0..pre).all(|i| mapped(i) == Some(keys[i]))
}

/// The stop road's streamed form (D1071's `Op::GroupRun`, decided before the group's input is walked): sorted
/// GroupAggregate `a` streams one group at a time where a counted LIMIT (`counted`) is right over it or over a top
/// Incremental Sort presorted on exactly its group keys, or over a top whose presorted keys stop exactly
/// (`exact_prefix`) and whose rows come from the group through row-at-a-time nodes (`spine_reaches`: a nest loop's
/// outer side), and its input hands on ordered rows one at a time (`ordered_feed`). The Limit's count (or the top's V4R-179 stop) then ends the walk that feeds the group where
/// PostgreSQL's Limit stops pulling, where the breaker form read every row of that walk first.
pub fn streams_to_stop<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>) -> bool {
    // a cursor's root (C1 part 2) stops the group itself only: a top over it sorts every group first
    let (id, cursor) = match b.limit.filter(|_| counted(b)) {
        Some((id, ..)) => (id, false),
        None => match cursor_root(b) {
            Some(id) => (id, true),
            None => return false,
        },
    };
    // over a full Sort only where the group keeps that Sort anyway (a call reads the row order: no key-sort absorb), so the
    // stream adds no sort the breaker form left out; the sort reads its input whole in PostgreSQL too, the stop saves the
    // groups past the Limit's
    let kept_sort = a.plan.lefttree.is_some_and(|n| n.as_sort().is_some()) && !crate::ops::group::absorb::order_free(b, a);
    // a collect with no ORDER BY of its own streams over a feed in PostgreSQL's order, ties too (`scan_spine`): its rows
    // reach it as they reach PG's (any_value's first non-NULL one too), and lowerv4 hosted.rs #530 licenses only the feed's
    // work before the run (aggtrans_core-0339, USEROP agg_shop-0044); over any other feed it keeps the breaker form
    let first_row = !kept_sort && scan_spine(b, a.plan.lefttree);
    if a.aggstrategy != AGG_SORTED || !(ordered_feed(a.plan.lefttree) || kept_sort) || !ties_free(a, first_row) || a.numCols < 1 || !a.groupingSets.is_nil() || !a.chain.is_nil() || a.gravity_rewrite.is_some() {
        return false;
    }
    let me = a.plan.plan_node_id;
    let mut top = false;
    crate::rerun::each_plan_node(b.stmt, &mut |n| {
        let t = n.as_incremental_sort().filter(|t| t.sort.plan.plan_node_id == id);
        top |= t.is_some_and(|t| t.sort.plan.lefttree.and_then(|l| l.as_plan()).is_some_and(|l| l.plan_node_id == me) && top_on_keys(t, a));
        // the top's rows may come from the group through a nest loop's outer side (rnd-0600): its stop is then exact
        top |= t.is_some_and(|t| spine_reaches(t.sort.plan.lefttree, me) && exact_prefix(b, t));
    });
    (id == me || top && !cursor) && leaves_late(b, a)
}

/// The stop road's spine, one walk for every producer under a stopping consumer: node `to`'s rows reach the node above
/// `from` one at a time and in `to`'s order, through Nested Loop outer sides and Result projections only (PostgreSQL's
/// ExecNestLoop pulls its outer a row at a time and hands on each joined row before the next outer row; a Result with no
/// one-time qual and no qual hands each row on). So a stop above `from` stops `to` where PostgreSQL's pulls stop.
pub fn spine_reaches(from: Option<Node<'_>>, to: i32) -> bool {
    let mut n = from;
    while let Some(node) = n {
        if node.as_plan().is_some_and(|p| p.plan_node_id == to) {
            return true;
        }
        n = match (node.as_nest_loop(), node.as_result()) {
            (Some(nl), _) => nl.join.plan.lefttree,
            (None, Some(r)) if r.resconstantqual.is_none() && r.plan.qual.is_nil() => r.plan.lefttree,
            _ => return false,
        };
    }
    false
}

/// Every presorted key of top Incremental Sort `t` is a by-value type ordered by its raw or float word (an integer, a
/// float, a date or timestamp, a bool, an oid): lowerv4's V4R-179 stop (runs/topk_rows.rs `Stop::of`) is then exact,
/// PostgreSQL's rows and no more (nodeIncrementalSort.c: the bound's rows, then on to the prefix group's end and one row
/// past it), so a group streamed under it reads PostgreSQL's rows whatever row-at-a-time nodes run between.
fn exact_prefix<'p>(b: &Builder<'p, '_>, t: &'p IncrementalSort<'p>) -> bool {
    let pre = t.nPresortedCols.max(0) as usize;
    let Some(cols) = t.sort.sortColIdx.get(..pre) else { return false };
    let key = |resno: i16| t.sort.plan.targetlist.iter().find_map(|n| n.as_target_entry().filter(|te| te.resno == resno)).and_then(|te| crate::catalog::expr_type::type_of(te.expr));
    let exact = |ty: u32| facts_of(b.facts, ty).is_some_and(|tf| tf.byval && (tf.image.is_word() || tf.image.is_float()));
    pre > 0 && cols.iter().all(|&c| key(c).is_some_and(exact))
}

/// The streamed form under a merge join (mergejoin r106: mj-0136..0143): a sorted GroupAggregate that is a merge join's
/// inner, or the input of the merge's Material, streams (D1071's `Op::GroupRun`) over an ordered feed. PostgreSQL's
/// ExecMergeJoin fetches its inner one tuple at a time and none once the join ends, and the GroupAggregate reads one row
/// of its feed past each group it hands on, where the breaker form folded the whole feed before the merge's first row (a
/// LIMIT 10 over a LEFT JOIN to a GROUP BY read 100k rows where PostgreSQL reads 18). Streamed, lowerv4's merge pulls the
/// group as it pulls a scan (ops/mergejoin/exact_inner.rs `Form::Grows`), so the feed's walk ends where the merge stops;
/// where that form does not reach, the merge spools the stream whole as it spooled the breaker's groups. The streamed
/// form's rules (`streams_to_stop`'s): an ordered feed, no grouping sets, split or hint, every call's answer free of
/// its group's tie order, and plain calls (no DISTINCT, no aggregate ORDER BY, no ordered-set call: the streamed group
/// builds no per-group set nor an aggregate's sort, as `agg_stops`).
pub fn merge_inner<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>) -> bool {
    if a.aggstrategy != AGG_SORTED || a.aggsplit != 0 || a.numCols < 1 || !a.groupingSets.is_nil() || !a.chain.is_nil() || a.gravity_rewrite.is_some() {
        return false;
    }
    if !ordered_feed(a.plan.lefttree) || !ties_free(a, false) {
        return false;
    }
    let me = Some(a.plan.plan_node_id);
    let id = |n: Option<Node<'p>>| n.and_then(|n| n.as_plan()).map(|p| p.plan_node_id);
    let mut inner = false;
    crate::rerun::each_plan_node(b.stmt, &mut |n| {
        let r = n.as_merge_join().and_then(|mj| mj.join.plan.righttree);
        inner |= r.is_some_and(|r| id(Some(r)) == me || r.as_material().is_some_and(|m| id(m.plan.lefttree) == me));
    });
    let mut plain = inner;
    for e in a.plan.targetlist.iter().chain(a.plan.qual.iter()) {
        crate::expr::walk(e, &mut |x| {
            if let Some(r) = x.as_aggref() {
                plain &= r.aggdistinct.is_nil() && r.aggorder.is_nil() && r.aggkind == AGGKIND_NORMAL;
                return false;
            }
            plain
        });
    }
    plain && leaves_late(b, a)
}

/// USEROP #530 (kit K2.ragg): sorted GroupAggregate `a` holds a range collect (range_agg, range_intersect_agg) over a
/// type that is not built in (a user range: range_agg's final sorts its ranges by the subtype opclass's FUNCTION 1 and
/// range_intersect_agg's transition compares by it, user code) or a pseudo-type: lowerv4's keyed end folds every group's
/// rows and runs those calls ahead of a consumer that stops (a LIMIT, a cursor's FETCH), and refuses (hosted.rs #530).
/// The group streams instead (D1071's `Op::GroupRun`, PostgreSQL's form): each group is finalized as it is handed on,
/// before the next group's rows are folded, so the user code runs where PostgreSQL runs it and a consumer's stop stops it
/// (lowerv4 caps a pull, D439). A range collect's value does not read its rows' order; any other collect (array, string,
/// JSON, any_value) or a user aggregate in the group keeps the keyed end: the stream reads a kept Sort, whose tie order
/// is not PostgreSQL's. Plain calls only (every call AGGKIND_NORMAL with no DISTINCT and no aggregate ORDER BY: the
/// streamed group builds no aggregate sort), no grouping sets, no split, no hint.
pub fn collect_streams<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>) -> bool {
    if a.aggstrategy != AGG_SORTED || a.aggsplit != 0 || a.numCols < 1 || !a.groupingSets.is_nil() || !a.chain.is_nil() || a.gravity_rewrite.is_some() {
        return false;
    }
    let (mut plain, mut hit) = (true, false);
    for e in a.plan.targetlist.iter().chain(a.plan.qual.iter()) {
        crate::expr::walk(e, &mut |x| {
            if let Some(r) = x.as_aggref() {
                plain &= r.aggdistinct.is_nil() && r.aggorder.is_nil() && r.aggkind == AGGKIND_NORMAL;
                let row = crate::ops::agg::trans::table::row(r.aggfnoid);
                let range = row.is_some_and(|w| matches!(w.class, Class::Collect(Collect::Range | Collect::RangeIntersect)));
                plain &= range || row.is_some_and(|w| !matches!(w.class, Class::Collect(_)));
                hit |= range && r.aggargtypes.iter().any(|t| crate::effects::of_compare(b.facts, t).acts); // a user range's comparator
                return false;
            }
            plain
        });
    }
    plain && hit && leaves_late(b, a)
}

/// D2817: a streamed group's raise stays PostgreSQL's. A call of `a` that raises by its partial sums
/// (`effects::raises_by_order`: a float, money or interval sum or average, a float variance; or a call the class table does
/// not know) raises under D2597's recheck licence wherever some input order could overflow (lowerv4 class_float), and
/// the licence re-runs the statement in plan order only while no row has left it. A streamed group hands each group on as
/// it closes, so it streams with such a call only where its rows reach the top of the main plan through a node that reads
/// its whole input before its first row (`read_whole`), or where its feed hands it PostgreSQL's row order (`in_order`
/// over `order_spine`: its folds then raise where PostgreSQL's do, no re-run needed); elsewhere it keeps the breaker form,
/// which folds every row before its first group leaves.
fn leaves_late<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>) -> bool {
    let (raising, plain) = raising(a);
    !raising || read_whole(b.stmt.planTree, a.plan.plan_node_id) || (plain && order_spine(b, a.plan.lefttree))
}

/// Whether a call of `a` raises by its partial sums (`effects::raises_by_order`, or a call the class table does not
/// know), and whether every such call folds its rows in its input's order (no DISTINCT, no ORDER BY of its own, a plain
/// aggregate: PostgreSQL sorts those calls' rows per group).
fn raising<'p>(a: &'p Agg<'p>) -> (bool, bool) {
    let (mut raising, mut plain) = (false, true);
    for e in a.plan.targetlist.iter().chain(a.plan.qual.iter()) {
        crate::expr::walk(e, &mut |x| {
            if let Some(r) = x.as_aggref() {
                let raises = crate::ops::agg::trans::table::row(r.aggfnoid).is_none_or(|w| crate::effects::raises_by_order(w.class));
                raising |= raises;
                plain &= !raises || (r.aggdistinct.is_nil() && r.aggorder.is_nil() && r.aggkind == AGGKIND_NORMAL);
                return false;
            }
            true
        });
    }
    (raising, plain)
}

/// D2846: streamed group `a` (no breaker above it reads its whole input) holds a call that raises by its partial sums,
/// each such call a plain one over a feed that hands it PostgreSQL's row order, ties too (`order_spine`): its folds run in
/// plan order (`PipelineGraph::in_order`, `facts::order::in_order_scope`), raising at the row PostgreSQL raises at, which
/// is what lets `leaves_late` stream it under a LIMIT: a row handed on before the raise is one PostgreSQL hands on too.
pub fn in_order<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>) -> bool {
    let (raising, plain) = raising(a);
    raising && plain && !read_whole(b.stmt.planTree, a.plan.plan_node_id) && order_spine(b, a.plan.lefttree)
}

/// D3374: whether PostgreSQL reads plan node `id`'s every row when the statement is read to its end: `id` sits in the
/// statement's plan tree (not a SubPlan's or a CTE's), under nodes that each read their whole input before their own end
/// (a Result without a one-time qual, a projection set, a subquery scan, a sort, an aggregate, a group, a unique, a
/// Materialize, a row lock, an Append or MergeAppend member, a Gather) or a ModifyTable, which writes every row its source
/// returns. A Limit, a join or anything else on the way answers no (fail closed). The sorted groups it holds for are
/// `PipelineGraph::whole`.
pub(crate) fn whole_written(b: &Builder<'_, '_>, id: i32) -> bool {
    fn whole(n: Node<'_>) -> bool {
        n.as_result().is_some_and(|r| r.resconstantqual.is_none())
            || n.as_project_set().is_some()
            || n.as_subquery_scan().is_some()
            || n.as_sort().is_some()
            || n.as_incremental_sort().is_some()
            || n.as_agg().is_some()
            || n.as_group().is_some()
            || n.as_unique().is_some()
            || n.as_material().is_some()
            || n.as_lock_rows().is_some()
            || n.as_append().is_some()
            || n.as_merge_append().is_some()
            || n.as_gather().is_some()
            || n.as_gather_merge().is_some()
            || n.as_modify_table().is_some()
    }
    fn go(n: Node<'_>, id: i32) -> Option<bool> {
        crate::walk::stack_check(); // D3748
        let p = n.as_plan()?;
        if p.plan_node_id == id {
            return Some(true);
        }
        let sub = n.as_subquery_scan().and_then(|s| s.subplan);
        let members = n.as_append().map(|a| &a.appendplans).or_else(|| n.as_merge_append().map(|m| &m.mergeplans));
        let kids = [p.lefttree, p.righttree, sub].into_iter().flatten().chain(members.into_iter().flat_map(|l| l.iter()));
        let found = kids.into_iter().find_map(|c| go(c, id))?;
        Some(found && whole(n))
    }
    b.stmt.planTree.and_then(|r| go(r, id)).unwrap_or(false)
}

/// Whether plan node `me`'s rows reach the top of the plan from `n` through a node that reads its whole input before its
/// first row: a Sort, or a plain or hashed Agg (an AGG_MIXED one hands on its sorted phase's groups as they close). Fail
/// closed: `me` under a node the walk does not descend (an Append's arms, a SubPlan's or a CTE's plan) is not.
fn read_whole(n: Option<Node<'_>>, me: i32) -> bool {
    fn found(n: Option<Node<'_>>, me: i32, whole: bool) -> Option<bool> {
        crate::walk::stack_check(); // D3748
        let node = n?;
        let p = node.as_plan()?;
        if p.plan_node_id == me {
            return Some(whole);
        }
        let whole = whole || node.as_sort().is_some() || node.as_agg().is_some_and(|a| a.aggstrategy == AGG_PLAIN || a.aggstrategy == AGG_HASHED);
        let sub = node.as_subquery_scan().and_then(|s| found(s.subplan, me, whole));
        sub.or_else(|| found(p.lefttree, me, whole)).or_else(|| found(p.righttree, me, whole))
    }
    found(n, me, false) == Some(true)
}

/// Sorted GroupAggregate `a` holds an aggregate whose state keeps a sort of its group's rows (an ordered-set or
/// hypothetical-set call: ordered_set_transition's tuplesort; an ORDER BY call the host sorts at its final, or folds per
/// row in the Sort's order): it streams one group at a time (D1071's `Op::GroupRun`), as PostgreSQL's agg_retrieve_direct
/// runs it, so each group's states are finalized and freed (the aggregate context reset at the next group) before the
/// next group's rows are folded, where the breaker form keeps every group's sort open to the end of its input. Only over
/// an input in the group's order that the breaker form reads as well (a scan, join or merge; or a Sort or Incremental Sort
/// the group keeps because a call reads the row order; no Append, whose pruned arms the stream does not run), with no
/// DISTINCT call (the streamed group builds no DISTINCT set), calls whose answer no tie order changes (`ties_free`) and
/// sorts by built-in comparators (`sorts_builtin`), where its groups are read as the breaker form's are (`stream_read`),
/// and a call that raises by its partial sums only where no row leaves before its last group (`leaves_late`, D2817).
/// D3578: so does a group holding a call whose state the host keeps by reference (`hosted_state`: a min / max of a
/// computed text, a bytea or array extreme, ..), its Sort kept even under an order-free parent: absorbed, the keyed form
/// held every group's state to the input's end (500,000 max(text) states of 16.5 kB filled a 12 GiB server), where
/// PostgreSQL's Sort spills at work_mem and its GroupAggregate frees each group's states at the next group.
pub fn sorts_stream<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>) -> bool {
    if a.aggstrategy != AGG_SORTED || a.aggsplit != 0 || a.numCols < 1 || !a.groupingSets.is_nil() || !a.chain.is_nil() || a.gravity_rewrite.is_some() {
        return false;
    }
    let (mut plain, mut hit, mut hosted) = (true, false, false);
    for e in a.plan.targetlist.iter().chain(a.plan.qual.iter()) {
        crate::expr::walk(e, &mut |x| {
            if let Some(r) = x.as_aggref() {
                plain &= r.aggdistinct.is_nil() && sorts_builtin(b, r);
                hit |= r.aggkind != AGGKIND_NORMAL || !r.aggorder.is_nil();
                hosted |= hosted_state(b, r);
                return false;
            }
            true
        });
    }
    let sorted = a.plan.lefttree.is_some_and(|n| n.as_sort().is_some() || n.as_incremental_sort().is_some());
    let union = a.plan.lefttree.is_some_and(|n| n.as_append().is_some()); // a pruned union's arms do not stream
    if union || !(ordered_feed(a.plan.lefttree) || sorted && (hosted || !crate::ops::group::absorb::order_free(b, a))) || !ties_free(a, false) {
        return false;
    }
    plain && (hit || hosted) && stream_read(b.stmt.planTree, a.plan.plan_node_id) && leaves_late(b, a)
}

/// D3578: call `r`'s state is one the host keeps by reference (`Trans::Var`) under the most native fold its class has
/// (`agg::fold_of` with a decided numeric scale, and a text extreme over a column taken as a code lane, whose codes fold
/// in a word): a min / max of a computed text or of a by-reference type with no word extreme (bytea, arrays, records,
/// ..), a bit string's, a numeric statistic's, a collect's. The host's states live in the arena's contexts, which nothing
/// bounds by work_mem. A state of a by-value type is held in its slot (`by_ref_state`: an enum extreme's 4-byte oid), and
/// a money sum's is a word: no context growth; a call the class table does not know is `ties_free`'s.
fn hosted_state<'p>(b: &Builder<'p, '_>, r: &types_nodes::primnodes::Aggref<'_>) -> bool {
    let Some(row) = crate::ops::agg::trans::table::row(r.aggfnoid) else { return false };
    let arg = r.args.iter().filter_map(|n| n.as_target_entry()).map(|t| t.expr).next();
    let column = arg.is_some_and(|e| e.as_var().is_some() || e.as_relabel_type().is_some_and(|x| x.arg.as_var().is_some()));
    row.class != Class::SumMoney
        && crate::ops::shared::agg::fold_of(row.class, r.aggfnoid, column, true).1 == crate::ops::shared::agg_class::Trans::Var
        && by_ref_state(b, row.transtype, r.aggtranstype)
}

/// D3578: whether a call whose catalog transition type is `declared` (pg_aggregate's, polymorphic or not) and whose
/// resolved one is `resolved` (the Aggref's) keeps its state by reference: `internal` (a pointer to a state in the
/// aggregate's context), a type that is not by-value, or a type nothing knows; an enum's (anyenum: its 4-byte oid) and a
/// known by-value type's are held in the slot.
fn by_ref_state<'p>(b: &Builder<'p, '_>, declared: u32, resolved: u32) -> bool {
    use types_core::catalog::{ANYENUMOID, INTERNALOID};
    match declared {
        INTERNALOID => true,
        ANYENUMOID => false,
        _ => facts_of(b.facts, resolved).is_none_or(|t| !t.byval),
    }
}

/// Every sort clause of call `r` (its ORDER BY, or an ordered-set call's WITHIN GROUP) compares by built-in code: no user
/// operator outside its type's default family (`effects::user_order`) and a column whose type's comparator is built in
/// (`effects::of_compare`). A user comparator meets the pairs its sort compares, which differ with the order rows reach
/// the sort, so its calls, NOTICEs and error stay where the breaker form's call order puts them (lowerv4 regroup.rs
/// `call_order` refuses the ones that may act there; D1371).
pub(crate) fn sorts_builtin(b: &Builder<'_, '_>, r: &types_nodes::primnodes::Aggref<'_>) -> bool {
    let col = |i: u32| r.args.iter().filter_map(|t| t.as_target_entry()).find(|t| t.ressortgroupref == i).and_then(|t| crate::catalog::expr_type::type_of(t.expr));
    r.aggorder.iter().all(|n| n.as_sort_group_clause().is_some_and(|c| !crate::effects::user_order(b.facts, c.sortop) && col(c.tleSortGroupRef).is_some_and(|t| !crate::effects::of_compare(b.facts, t).acts)))
}

/// Whether sorted GroupAggregate `me` (in the plan from `n`) hands its groups to nodes that read a streamed group as they
/// read the breaker form's groups: a row at a time or whole, through an Agg, Sort, Incremental Sort, Limit, Unique,
/// SubqueryScan or a plain Result, a Nested Loop's outer side, or as a Merge Join's inner (directly or under its
/// Material: `merge_inner`'s). A nest's inner or another Material re-reads its input per outer row, and a merge's outer
/// meets the join's own fetch rules; those keep the breaker form. The main plan only: a SubPlan's or CTE's group keeps
/// the breaker form. A call that raises by its partial sums is `leaves_late`'s (D2817).
fn stream_read(n: Option<Node<'_>>, me: i32) -> bool {
    crate::walk::stack_check(); // D3748
    let Some(node) = n else { return false };
    let Some(p) = node.as_plan() else { return false };
    if p.plan_node_id == me {
        return true;
    }
    let is_me = |c: Option<Node<'_>>| c.and_then(|c| c.as_plan()).is_some_and(|q| q.plan_node_id == me);
    if let Some(mj) = node.as_merge_join() {
        let r = mj.join.plan.righttree;
        return is_me(r) || r.and_then(|r| r.as_material()).is_some_and(|m| is_me(m.plan.lefttree));
    }
    if let Some(s) = node.as_subquery_scan() {
        return stream_read(s.subplan, me);
    }
    let result = node.as_result().is_some_and(|r| r.resconstantqual.is_none() && r.plan.qual.is_nil());
    let passes = result || node.as_nest_loop().is_some() || node.as_agg().is_some() || node.as_sort().is_some() || node.as_incremental_sort().is_some() || node.as_limit().is_some() || node.as_unique().is_some();
    passes && stream_read(p.lefttree, me)
}

/// Every aggregate call of `a` answers the same whatever order the Sort leaves its equal-key rows in: Gravity's sort does
/// not keep PostgreSQL's tie order, so over a kept Sort a collect with no ORDER BY of its own (array_agg, string_agg,
/// json(b)_agg, range_agg, ..) or a user-defined aggregate would read its group's rows in another order than
/// PostgreSQL's (the breaker form names those, e.g. #530). A collect ordering its own input (`jsonb_agg(x ORDER BY x)`:
/// only its ties are free, D1428), a DISTINCT, an ordered-set call or a float sum (compared within the platform
/// tolerance) answers the same. The aggregates are found outermost first (their arguments are not searched); a node
/// outside them the walk does not know could hide one, so no stream then.
fn ties_free<'p>(a: &'p Agg<'p>, first_row: bool) -> bool {
    fn calls<'p>(n: Node<'p>, ok: &mut bool, first_row: bool) {
        crate::walk::stack_check(); // D3748
        if let Some(r) = n.as_aggref() {
            let row = crate::ops::agg::trans::table::row(r.aggfnoid);
            *ok &= row.is_some_and(|row| match row.class {
                Class::User => false,
                // a collect reads its rows' order (any_value its first non-NULL argument): PostgreSQL's when the feed is in
                // its order, ties too
                Class::Collect(_) => !r.aggorder.is_nil() || first_row,
                _ => true,
            });
            return;
        }
        let known = crate::expr::kids(n, &mut |k| calls(k, &mut *ok, first_row));
        *ok &= known;
    }
    let mut ok = true;
    for l in [&a.plan.targetlist, &a.plan.qual] {
        for n in l.iter() {
            calls(n, &mut ok, first_row);
        }
    }
    ok
}

/// agg_shop-0224: whether `input` hands a group its rows in PostgreSQL's order, ties too: a btree index scan of a heap
/// relation, or a merge join whose outer is such an input and whose inner such a scan (lowerv4 hosted.rs `scan_order`
/// reads the same off the graph: any_value in a sorted group is served there alone; a nested loop's order is no known
/// order here, so its group keeps the keyed end, which `scan_order` serves too).
fn scan_spine<'p>(b: &Builder<'p, '_>, input: Option<Node<'p>>) -> bool {
    let scan = |n: Node<'p>| heap_index_scan(b, n);
    let mut n = input;
    while let Some(node) = n {
        if scan(node) {
            return true;
        }
        let Some(mj) = node.as_merge_join().filter(|mj| mj.join.plan.righttree.is_some_and(scan)) else { return false };
        n = mj.join.plan.lefttree;
    }
    false
}

/// Whether `n` is an Index or Index Only Scan of a heap relation.
fn heap_index_scan<'p>(b: &Builder<'p, '_>, n: Node<'p>) -> bool {
    let heap = |scanrelid: u32| b.stmt.rtable.iter().nth((scanrelid as usize).wrapping_sub(1)).and_then(|e| e.as_range_tbl_entry()).is_some_and(|r| b.facts.rel(r.relid).is_some_and(|f| f.am == crate::catalog::Am::Heap));
    n.as_index_scan().map(|s| s.scan.scanrelid).or_else(|| n.as_index_only_scan().map(|s| s.scan.scanrelid)).is_some_and(|r| heap(r as u32))
}

/// D2922 (R1): whether `input` is a Merge Append whose every arm is an Index or Index Only Scan of a heap relation (a
/// partitioned table's or a UNION ALL's index merge). Each arm hands its rows in index order, ties in heap TID order, and
/// lowerv4's pull form of the merge replays PostgreSQL's binary heap over them (ops/append/merge_pull_heap.rs, D2884):
/// a row per arm at the open, then the emitting arm's next, ties across arms in the heap's order. A group streamed over it
/// with its raising folds in plan order (`in_order`) refuses by name where lowerv4 does not pull the merge
/// (ops/group/stream.rs), so this order is PostgreSQL's wherever such a group runs.
fn merge_spine<'p>(b: &Builder<'p, '_>, input: Option<Node<'p>>) -> bool {
    input.and_then(|n| n.as_merge_append()).is_some_and(|m| !m.mergeplans.is_nil() && m.mergeplans.iter().all(|arm| heap_index_scan(b, arm)))
}

/// A feed in PostgreSQL's row order, ties too: `scan_spine`'s, or `merge_spine`'s index merge (D2922). Only the raising
/// folds' rule reads it (`leaves_late`, `in_order`); a collect's first row keeps `scan_spine` (lowerv4 hosted.rs
/// `scan_order` serves #530's collects over a scan's order alone).
fn order_spine<'p>(b: &Builder<'p, '_>, input: Option<Node<'p>>) -> bool {
    scan_spine(b, input) || merge_spine(b, input)
}

/// The same for a Group or Unique node `plan` (no aggregates): right under a counted LIMIT over an ordered feed, its runs
/// stream through a first-of-run op (V4R-147's), so the Limit after it ends the walk where PostgreSQL's does.
pub fn limit_streams<'p>(b: &Builder<'p, '_>, plan: &'p types_nodes::plannodes::Plan<'p>) -> bool {
    (counted(b) && b.limit.is_some_and(|(id, ..)| id == plan.plan_node_id) || cursor_root(b) == Some(plan.plan_node_id)) && ordered_feed(plan.lefttree)
}

/// C1 part 2: the plan node a named cursor's FETCH count reaches (`PipelineGraph::partial`), a stopping consumer of
/// unknown count at the statement's root: the root, through a Result that hands every row it pulls on (as a Limit's bound
/// reaches its child: ops/mod.rs `bounded`; a SCROLL cursor's plan carries no Material: the portal's store serves its
/// backward fetches). A stream to it stops where each FETCH stops pulling (the entry's adapter asks for the rows a FETCH
/// owes), and the next FETCH resumes it; no road that needs the count (a top's or a batch's bound) reads it.
pub fn cursor_root(b: &Builder<'_, '_>) -> Option<i32> {
    if !b.graph.partial {
        return None;
    }
    let mut n = b.stmt.planTree;
    loop {
        let c = n?;
        match c.as_result() {
            Some(r) if r.resconstantqual.is_none() && r.plan.lefttree.is_some() && r.plan.qual.is_nil() => n = r.plan.lefttree,
            _ => return c.as_plan().map(|p| p.plan_node_id),
        }
    }
}

/// D2926: whether a partial reader's admission of `stmt` (`PipelineGraph::partial`) may stop where the whole reader's reads
/// its input whole first: a sorted group, a Group, a Unique or an Incremental Sort at the root (through Results, as
/// `cursor_root` walks: the group's, `limit_streams`' and the Incremental Sort's cursor roads), or a Merge Append under
/// the root's row-at-a-time chain (a group, a Unique, a nest loop's or merge join's outer: merge_admit.rs `run_stops`).
/// Any other plan's two programs are alike, so the entry admits no second one for a counted run.
pub fn partial_stops(stmt: &types_nodes::plannodes::PlannedStmt<'_>) -> bool {
    let mut n = stmt.planTree;
    let mut root = true;
    while let Some(c) = n {
        if let Some(r) = c.as_result() {
            if r.resconstantqual.is_some() || r.plan.lefttree.is_none() {
                return false;
            }
            n = r.plan.lefttree;
            continue;
        }
        if c.as_merge_append().is_some() || c.as_unique().is_some() || c.as_group().is_some() || c.as_agg().is_some_and(|a| a.aggstrategy == AGG_SORTED) {
            return true;
        }
        if root && c.as_incremental_sort().is_some() {
            return true;
        }
        n = match (c.as_nest_loop(), c.as_merge_join()) {
            (Some(j), _) => j.join.plan.lefttree,
            (_, Some(j)) => j.join.plan.lefttree,
            _ => return false,
        };
        root = false;
    }
    false
}

/// Whether the Limit being walked (`Builder::limit`) counts: a constant count of at least 1, a constant offset (or none),
/// no WITH TIES.
fn counted(b: &Builder<'_, '_>) -> bool {
    b.limit.is_some_and(|(_, count, offset, ties)| !ties && matches!(count, Bound::Const(c) if c >= 1) && matches!(offset, Bound::All | Bound::Const(0..=i64::MAX)))
}

/// Whether `input` hands on ordered rows one at a time: a scan, a join or a merge, no Sort, Incremental Sort or Gather
/// (an Incremental Sort below a group keeps `group_stop`'s stop; a Sort reads its input whole in PostgreSQL too).
fn ordered_feed(input: Option<Node<'_>>) -> bool {
    input.is_some_and(|n| n.as_sort().is_none() && n.as_incremental_sort().is_none() && n.as_gather_merge().is_none() && n.as_gather().is_none())
}

/// The column of `tlist` at `resno`, when its entry is a plain outer column.
fn outer(tlist: &types_nodes::list::NodeList<'_>, resno: i16) -> Option<i16> {
    tlist.iter().find_map(|n| n.as_target_entry().filter(|te| te.resno == resno).and_then(|te| te.expr.as_var()).filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0).map(|v| v.varattno))
}

/// Whether top Incremental Sort `t` (no qual) is presorted on exactly the group keys of `a` right under it: each presorted
/// key a column of `a`'s target list that is a group key column, and every group key so taken.
fn top_on_keys<'p>(t: &'p IncrementalSort<'p>, a: &'p Agg<'p>) -> bool {
    let pre = t.nPresortedCols.max(0) as usize;
    let grp = &a.grpColIdx[..(a.numCols.max(0) as usize).min(a.grpColIdx.len())];
    let Some(cols) = t.sort.sortColIdx.get(..pre) else { return false };
    if pre != grp.len() || !t.sort.plan.qual.is_nil() {
        return false;
    }
    let mapped = |i: usize| outer(&t.sort.plan.targetlist, cols[i]).and_then(|r| outer(&a.plan.targetlist, r));
    (0..pre).all(|i| mapped(i).is_some_and(|g| grp.contains(&g) && (0..i).all(|j| mapped(j) != Some(g))))
}

/// The streamed form: a sorted GroupAggregate right under a counted LIMIT over an index's order (an Index or Index Only
/// Scan, or a Nested Loop or Merge Join, right below; no Sort, no union: PostgreSQL's group reads a row at a time and stops
/// with the Limit; `keys::ordered` holds the input's order in either form). The fold-ahead form reads
/// every input row before its first group leaves, and where an argument may raise re-runs the statement in plan order
/// after an error past the stop (agg_core-0318: 99.5k index rows and a compile each run where PostgreSQL reads 306);
/// streamed, the scan stops at the Limit's last group, no licence asked. Plain calls only (no DISTINCT, no aggregate ORDER
/// BY, no ordered-set call): the streamed group builds no per-group set (M6) nor an aggregate's sort, and a refusal there
/// would unserve what the fold-ahead form serves.
pub fn agg_stops<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>) -> bool {
    let counted = b.limit.is_some_and(|(id, count, _, _)| id == a.plan.plan_node_id && !matches!(count, Bound::All)) || cursor_root(b) == Some(a.plan.plan_node_id);
    let ordered = a.plan.lefttree.is_some_and(|n| matches!(n.node_tag(), NodeTag::T_IndexScan | NodeTag::T_IndexOnlyScan | NodeTag::T_NestLoop | NodeTag::T_MergeJoin));
    if !counted || !ordered || a.aggstrategy != AGG_SORTED || a.aggsplit != 0 || a.numCols < 1 || !a.groupingSets.is_nil() || !a.chain.is_nil() {
        return false;
    }
    let mut plain = true;
    for e in a.plan.targetlist.iter().chain(a.plan.qual.iter()) {
        crate::expr::walk(e, &mut |x| {
            if let Some(r) = x.as_aggref() {
                // group lane (entry 14's R-KEEP rule, as `ties_free`): a collect with no ORDER BY of its own (but any_value,
                // no #530 collect) keeps the breaker form: lowerv4 hosted.rs #530 refuses it streamed beside an index scan's
                // rows, which the breaker form serves under fold_ahead's licence (group_stop_streamed_scan,
                // agg_shop-0040..0043, agg_typed-0015..0017)
                let collect = crate::ops::agg::trans::table::row(r.aggfnoid).is_some_and(|w| matches!(w.class, Class::Collect(c) if c != Collect::AnyValue));
                plain &= r.aggdistinct.is_nil() && r.aggorder.is_nil() && r.aggkind == AGGKIND_NORMAL && !collect;
                return false;
            }
            plain
        });
    }
    plain && leaves_late(b, a)
}

/// Whether `ops` (a pipeline's ops after the op that feeds them one row at a time; projections may sit between) end in a
/// counted top over a presorted prefix with no WITH TIES: its walk stops once its bound's prefix group closes, where the
/// top's stop is exact (V4R-179), and reads every row where it is not (its own D449 rule then holds).
pub fn top_stops(ops: &[Op<'_>]) -> bool {
    for op in ops {
        match op {
            Op::Project(_) => {}
            Op::Sort(d) => return d.presorted > 0 && matches!(d.form, SortForm::TopK { count, ties: false, .. } if !matches!(count, Bound::All)),
            _ => return false,
        }
    }
    false
}

/// D2313: a sorted group's input leads with its keys, one or more of them in another btree family's order (a
/// text_pattern_ops index, a Sort by `~<~`): the group streams (D1071's `Op::GroupRun`) and hands its groups on in input
/// order as PostgreSQL's GroupAggregate does, where its breaker form's emit sorts them by each key type's default family.
pub fn family_order<'p>(b: &Builder<'p, '_>, child: &Open<'p>, keys: &[KeyDecl<'p>]) -> bool {
    if keys.is_empty() || keys.len() > MAX_ORDER_KEYS {
        return false;
    }
    let o = crate::ops::shared::order::order_of(b, child);
    let mut want = Order::NONE;
    for key in keys {
        let col = Col::outer(key.resno);
        let m = (0..o.n as usize).find(|&m| if o.fams[m] == 0 { o.same(o.keys[m].col, col) } else { o.keys[m].col == col });
        match m {
            Some(m) => want.push_fam(OrderKey { col, ..o.keys[m] }, o.fams[m]),
            None => want.push(OrderKey { col, desc: false, nulls_first: false }),
        }
    }
    want.fams[..want.n as usize].iter().any(|&f| f != 0) && crate::ops::shared::order::ordered_as_order(&o, crate::ops::shared::order::projection(child), &want, Kind::Agg, 0, "", "").is_ok()
}

/// The stop road under a join: a group streamed to a counted LIMIT (`streams_to_stop`, `limit_streams`) ends the walk of
/// its outer spine where PostgreSQL's Limit stops, but a merge join on that spine spooled its whole inner first, where
/// PostgreSQL's ExecMergeJoin fetches its inner one tuple at a time and none past the join's end. Each merge join on the
/// spine below group node `plan` (through merge and nest loop outers) asks Exact of its inner when that is a relation scan
/// or a Material over one (as `mergejoin::ask_exact` asks an acting one): lowerv4's merge pulls it one row per fetch
/// where its first form reaches (ops/mergejoin/exact_inner.rs), and spools it as before where it does not.
pub fn ask_spine<'p>(b: &mut Builder<'p, '_>, plan: &'p types_nodes::plannodes::Plan<'p>) {
    let scan = |n: Option<Node<'p>>| n.is_some_and(|n| n.as_seq_scan().is_some() || n.as_index_scan().is_some() || n.as_index_only_scan().is_some() || n.as_bitmap_heap_scan().is_some());
    let mut n = plan.lefttree;
    while let Some(node) = n {
        if let Some(mj) = node.as_merge_join() {
            let inner = mj.join.plan.righttree.filter(|&i| scan(Some(i)) || i.as_material().is_some_and(|m| scan(m.plan.lefttree)));
            if let Some(id) = inner.and_then(|i| i.as_plan()).map(|p| p.plan_node_id).filter(|id| !b.graph.exact.contains(id)) {
                b.graph.exact.push(id);
            }
            n = mj.join.plan.lefttree;
        } else if let Some(nl) = node.as_nest_loop() {
            n = nl.join.plan.lefttree;
        } else {
            return;
        }
    }
}

/// The stop road over a DISTINCT group's presort (agg_shop-0234's form): a sorted GroupAggregate right under a counted
/// LIMIT (a HAVING too: the Limit counts the groups that pass it), or under a top Incremental Sort presorted on exactly its
/// group keys right under that LIMIT (`top_on_keys`, as `streams_to_stop`; D2922 R2), over an Incremental Sort PostgreSQL
/// adds only to order each group's DISTINCT arguments: its presorted prefix is exactly the group keys, over an index scan
/// or an index merge (`merge_spine`, D2922 R2: the group reads the merge's rows in its key order either form), and every
/// call folds to one result in any order of a group's rows given its distinct values (`absorb::distinct_free`). The group
/// reads the scan in its key order and streams (D1071's `Op::GroupRun`, runs.group_distinct's per-group set), so the Limit
/// ends the scan's walk where PostgreSQL's stops, where the sort read every row first. A raising call keeps D2817's rule
/// (`leaves_late` over the absorbed sort: PostgreSQL folds it in that sort's order, ties its quicksort's). The Incremental
/// Sort's node id (absorbed).
pub fn presort_stop<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>) -> Option<i32> {
    let (id, ..) = b.limit.filter(|_| counted(b))?;
    let t = a.plan.lefttree?.as_incremental_sort()?;
    let sp = &t.sort.plan;
    let n = a.numCols.max(0) as usize;
    let (grp, pre) = (a.grpColIdx.get(..n)?, t.nPresortedCols.max(0) as usize);
    let scan = sp.lefttree.is_some_and(|l| l.as_index_scan().is_some() || l.as_index_only_scan().is_some()) || merge_spine(b, sp.lefttree);
    let me = a.plan.plan_node_id;
    let mut top = false;
    if id != me {
        crate::rerun::each_plan_node(b.stmt, &mut |n| {
            let u = n.as_incremental_sort().filter(|u| u.sort.plan.plan_node_id == id);
            top |= u.is_some_and(|u| u.sort.plan.lefttree.and_then(|l| l.as_plan()).is_some_and(|l| l.plan_node_id == me) && top_on_keys(u, a));
        });
    }
    if (id != me && !top) || a.aggstrategy != AGG_SORTED || n == 0 || pre != n || !scan || a.aggsplit != 0 || !a.groupingSets.is_nil() || !a.chain.is_nil() || a.gravity_rewrite.is_some() {
        return None;
    }
    if !sp.qual.is_nil() || !sp.initPlan.is_nil() || crate::facts::hints::Hint::of_sort(&t.sort).is_some() {
        return None;
    }
    let cols = t.sort.sortColIdx.get(..pre)?;
    if !(cols.iter().all(|c| grp.contains(c)) && grp.iter().all(|g| cols.contains(g))) {
        return None;
    }
    for (i, e) in sp.targetlist.iter().enumerate() {
        let v = e.as_target_entry()?.expr.as_var()?;
        if v.varno != OUTER_VAR || v.varlevelsup != 0 || v.varattno as usize != i + 1 {
            return None;
        }
    }
    (crate::ops::group::absorb::distinct_free(b, a) && leaves_late(b, a)).then_some(sp.plan_node_id)
}
