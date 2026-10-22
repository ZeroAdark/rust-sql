//! The Limit's stop contract (result lane; main 07:01Z, the M2 stop-past-limit rows): PostgreSQL's Limit stops pulling
//! once its count is met, and every node below it returns its rows as it reads them (a sorted GroupAggregate, a Group, a
//! Unique, a WindowAgg, a scan, a join's outer side) except one that reads all of its input before its first row (a
//! Sort, a hashed or plain Agg, a Hash, a hashed SetOp), so the code of the nodes it pulls through runs for no row past
//! the stop (the families of an Incremental Sort or a Merge Append right under the Limit keep that stop themselves:
//! V4R-179, D449). A breaker this plan puts below the Limit over such code runs it over its whole input first.
//! Where an expression there may raise or acts, the statement differs from PostgreSQL's (an error where it returns rows,
//! VOLATILE calls it never makes), so the Limit refuses it by name (fail closed) until the family streams to the stop
//! (D1071's group stream does) or holds the error (the batch re-run). Code that only may raise reads ahead under a
//! recheck licence (D1814's group fold, W4's window fill, D3736's other breakers): an error before any row left re-runs
//! the statement in plan order, where the licence is denied and the stop holds by the family's own form or this refusal.
//! A window op in the Limit's own pipeline holds its partition's rows before it hands one on, so what its pipeline runs
//! before it counts the same way. Code under a node that reads all of its input in PostgreSQL too departs from nothing.
use mcx::{GrowIn, PgVec};
use types_nodes::list::NodeList;
use types_nodes::Node;

use crate::effects::{of_list, of_ops, Cx, Effects, PLAIN};
use crate::graph::{Bound, Op, Source};
use crate::walk::{Builder, Open};

/// PostgreSQL's AggStrategy AGG_SORTED (nodes.h): a GroupAggregate over its sorted input, one group at a time.
const AGG_SORTED: u32 = 1;
/// PostgreSQL's SetOpStrategy SETOP_HASHED (nodes.h): a SetOp that reads all of its input into a hash table first.
const SETOP_HASHED: u32 = 1;

pub const AHEAD: &str = "a LIMIT over a node PostgreSQL runs a row at a time (a sorted GroupAggregate, Group, Unique, WindowAgg) that this plan runs over its whole input first, where code it runs there may raise or acts (PostgreSQL never evaluates it past the stop)";

/// Whether the Limit over `open` (its child's pipeline; `child`, its child's plan node) with count `count` would evaluate,
/// through a breaker below it, code PostgreSQL's stop never reaches that may raise or acts. No count (`LIMIT ALL`, an
/// OFFSET alone): nothing stops.
pub fn ahead<'p>(b: &mut Builder<'p, '_>, child: Option<Node<'p>>, open: &Open<'p>, count: Bound<'p>, kept: Option<i32>) -> bool {
    if matches!(count, Bound::All) {
        return false;
    }
    let cx = Cx::new(b.stmt, b.facts, b.arena);
    // a window op in the Limit's own pipeline holds its partition's rows before it hands one on: the filters and
    // projections its pipeline runs before it (another op there is its own family's to stop)
    if let Some(w) = open.ops.iter().position(|o| matches!(o, Op::Window(_))) {
        let exprs = open.ops[..w].iter().filter(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_)));
        let e = exprs.fold(Effects::NONE, |e, o| e.and(of_ops(&cx, core::slice::from_ref(o), &PLAIN)));
        if e.may_raise || e.acts {
            return true;
        }
    }
    let mut reader = open;
    let mut seen = 0usize;
    let mcx = b.mcx;
    let mut windows: PgVec<'_, (usize, i32, Effects)> = PgVec::new_in(mcx);
    let mut path: PgVec<'_, i32> = PgVec::new_in(mcx);
    let mut breakers: PgVec<'_, (Option<Node<'p>>, Effects)> = PgVec::new_in(mcx);
    // a breaker PostgreSQL runs only as far as the Limit pulls that is no window (a sorted group, say) passed on the way:
    // it reads its input ahead here, so a window below it cannot keep the stop by its own Exact
    let mut through = false;
    while let Source::Breaker { slot } = reader.source {
        let Some(p) = producer(b, reader, slot) else { return false };
        if seen > b.graph.pipelines.len() {
            return false;
        }
        let ahead = &b.graph.pipelines[p];
        let closer = b.closers.get(p).copied();
        let mut e = Effects::NONE;
        for n in ahead.node_ops.iter().filter_map(|o| node(b, o.node_id)).chain(closer) {
            if n.as_plan().is_some_and(|p| pulled(child, true, p.plan_node_id, kept)) {
                e = e.and(evaluated(&cx, n));
            }
        }
        // v5 errorhold (D1814): a sorted group whose fold ahead holds a granted licence (`gsets::fold_ahead`, which asks
        // with everything below it): an error before any row left re-runs the statement in plan order, where the group
        // streams to the stop; code that acts is never licensed
        let licensed = closer.and_then(|n| n.as_plan()).is_some_and(|c| b.ahead_licensed.contains(&c.plan_node_id));
        // the exact request (PLAN-v5 §2.5, window W4): a WindowAgg spools its fill only as far as its rows need, as PG's
        // spool_tuples, under a recheck licence or Exact (`window_keeps_stop`, after the walk)
        let window = closer.filter(|n| n.as_window_agg().is_some()).and_then(|n| n.as_plan()).map(|p| p.plan_node_id);
        match window {
            Some(_) if (e.acts || e.may_raise && !licensed) && through => return true,
            Some(w) if e.acts || e.may_raise && !licensed => windows.push_in((path.len(), w, e)),
            _ if e.acts => return true,
            // D3736: another breaker's code that only may raise asks the recheck licence after the walk (`breaker_keeps_stop`)
            _ if e.may_raise && !licensed => breakers.push_in((closer, e)),
            _ => {}
        }
        through |= window.is_none() && closer.and_then(|n| n.as_plan()).is_some_and(|c| pulled(child, true, c.plan_node_id, kept));
        path.extend_in(window);
        reader = ahead;
        seen += 1;
    }
    if !breakers.into_iter().all(|(closer, e)| breaker_keeps_stop(b, closer, e)) {
        return true;
    }
    // a window asked Exact stops only if every window between it and the Limit stops too: each spools its own fill (the
    // window above's reader) as far as its rows need, so they are asked Exact with it
    for (i, w, e) in windows {
        if window_keeps_stop(b, w, e).is_none() {
            for &u in &path[..i] {
                if !b.graph.exact.contains(&u) {
                    b.graph.exact.push(u);
                }
            }
        }
    }
    false
}

/// The exact request (PLAN-v5 §2.5; window W4, D2357): the Limit stops over WindowAgg `id`'s fill, whose code `e` (its
/// filters and projections, and what they read) may raise or acts. PostgreSQL's WindowAgg spools its input only as far as
/// its rows need (`spool_tuples`). Code that may raise reads ahead under a recheck licence (the window's fill as it is; an
/// error before any row left re-runs the statement in plan order); otherwise (code that acts, or inside the plan-order run)
/// the Limit asks Exact of the window (`PipelineGraph::exact`), whose fill then evaluates nothing ahead of its spool.
/// The licence the fill reads ahead under; `None` when it asked Exact (construct C2: the read ahead holds a `Licence`).
fn window_keeps_stop<'p>(b: &mut Builder<'p, '_>, id: i32, e: Effects) -> Option<crate::effects::Licence> {
    let ask = if e.may_raise { e.and(Effects { acts: crate::effects::of_statement_acts(&crate::effects::Cx::new(b.stmt, b.facts, b.arena), &mut b.graph.acts_read), ..Effects::NONE }) } else { e };
    match crate::effects::licence(ask, crate::effects::Departure::Ahead) {
        Ok(l) => {
            b.graph.licensed(&l);
            b.ahead_licensed.push(id);
            b.graph.licensed_nodes.push(id);
            Some(l)
        }
        Err(_) => {
            if !b.graph.exact.contains(&id) {
                b.graph.exact.push(id);
            }
            None
        }
    }
}

/// D3736 (Django's `values_list(named=True).first()` over an `F() + 1` group key: Limit -> Group -> Incremental Sort, the
/// sort key `num + 1` computed below the sort): the Limit stops over a breaker `closer` that is no window, which this plan
/// runs over its whole input where PostgreSQL pulls it only as far as the Limit needs (an Incremental Sort's batches), and
/// whose code `e` may raise but does not act. As D1814's sorted-group fold (gsets `fold_ahead`) and W4's window fill
/// (`window_keeps_stop`): the breaker reads ahead under a recheck licence, asked with the statement's acting set (an error
/// before any row left re-runs the statement in plan order, where the licence is denied and the Limit refuses by AHEAD);
/// none where the breaker may run after a row left (rerun.rs `runs_late`: no plan-order run answers its error there).
/// Whether it reads ahead under one.
fn breaker_keeps_stop<'p>(b: &mut Builder<'p, '_>, closer: Option<Node<'p>>, e: Effects) -> bool {
    let Some(c) = closer.and_then(|n| n.as_plan()) else { return false };
    if crate::rerun::runs_late(b.stmt, b.arena, c) {
        return false;
    }
    let ask = e.and(Effects { acts: crate::effects::of_statement_acts(&crate::effects::Cx::new(b.stmt, b.facts, b.arena), &mut b.graph.acts_read), ..Effects::NONE });
    match crate::effects::licence(ask, crate::effects::Departure::Ahead) {
        Ok(l) => {
            b.graph.licensed(&l);
            b.ahead_licensed.push(c.plan_node_id);
            b.graph.licensed_nodes.push(c.plan_node_id);
            true
        }
        Err(_) => false,
    }
}

/// Whether plan node `id` is one PostgreSQL runs only as far as the Limit pulls: `n` or a node below it,
/// except a node that reads all of its input before it returns a row (a Sort, a hashed or plain Agg, a Hash, a hashed
/// SetOp), which runs its own code over all of its input in PostgreSQL too, and what lies below that; and except an
/// Incremental Sort or a Merge Append right under the Limit (`top`) and what lies below it, whose families keep the stop of
/// a bounded sort or merge themselves (the bounded top's exact stop, V4R-179, else D449's rule; a merge arm's D449 rule),
/// and except the sort a stop was handed to (`kept`: `facts::stream::group_streams`, PostgreSQL's batches exactly).
fn pulled(n: Option<Node<'_>>, top: bool, id: i32, kept: Option<i32>) -> bool {
    crate::walk::stack_check(); // D3748
    let Some(node) = n else { return false };
    let Some(plan) = node.as_plan() else { return false };
    let whole = node.as_sort().is_some()
        || node.as_agg().is_some_and(|a| a.aggstrategy != AGG_SORTED)
        || node.as_hash().is_some()
        || node.as_set_op().is_some_and(|s| s.strategy == SETOP_HASHED);
    let keeps = (top && (node.as_incremental_sort().is_some() || node.as_merge_append().is_some())) || kept == Some(plan.plan_node_id);
    if whole || keeps {
        return false;
    }
    plan.plan_node_id == id
        || pulled(plan.lefttree, false, id, kept)
        || pulled(plan.righttree, false, id, kept)
        || node.as_append().is_some_and(|a| a.appendplans.iter().any(|arm| pulled(Some(arm), false, id, kept)))
        || pulled(node.as_subquery_scan().and_then(|s| s.subplan), false, id, kept)
}

/// The pipeline producing breaker `slot` for `reader`: its declared life, else the slot's latest producer.
fn producer(b: &Builder<'_, '_>, reader: &Open<'_>, slot: u8) -> Option<usize> {
    let d = &reader.desc;
    match d.consumes.iter().position(|&s| s == slot) {
        Some(j) if j < d.consumes_from.len() => Some(d.consumes_from[j].0 as usize),
        _ => crate::breakers::decl_of(&b.graph, slot).map(|d| d.producer as usize),
    }
}

/// The plan node numbered `id`, from the statement's trees (its main tree and its subplans: numbers are statement-wide).
fn node<'p>(b: &Builder<'p, '_>, id: i32) -> Option<Node<'p>> {
    let mut found = None;
    crate::rerun::each_plan_node(b.stmt, &mut |n| {
        if found.is_none() && n.as_plan().is_some_and(|p| p.plan_node_id == id) {
            found = Some(n);
        }
    });
    found
}

/// The expressions node `n` evaluates per input row in the pipeline that fills the breaker: its target list and quals, its
/// join quals. Nothing of an Agg's, a Group's, a Unique's or a WindowAgg's own: their target lists and quals run per group
/// or per window row, after the breaker, as far as the Limit pulls, and an aggregate's arguments, FILTER and transitions,
/// folded per input row, are the agg family's stop to keep (D1071's stream; FOR MAIN: arguments that may raise).
fn evaluated<'p>(cx: &Cx<'_, 'p>, n: Node<'p>) -> Effects {
    let Some(plan) = n.as_plan() else { return Effects::NONE };
    let of = |l: &'p NodeList<'p>| of_list(cx, l, &PLAIN);
    let joinqual = n.as_nest_loop().map(|j| &j.join.joinqual).or_else(|| n.as_hash_join().map(|j| &j.join.joinqual)).or_else(|| n.as_merge_join().map(|j| &j.join.joinqual));
    let own = n.as_agg().is_some() || n.as_window_agg().is_some() || n.as_group().is_some() || n.as_unique().is_some();
    if own {
        return Effects::NONE;
    }
    joinqual.map_or(Effects::NONE, of).and(of(&plan.targetlist)).and(of(&plan.qual))
}
