//! D1753: PostgreSQL's Append starts an arm only when its reader pulls past the previous one (ExecAppend), and a blocking
//! child of that arm (a HashAggregate, a Sort, a Hash) consumes its input only then. Under a bound the consumer above the
//! union asks (a LIMIT, copied into every arm with one count: decompose append.rs `push_union`; frame/iface.rs's request,
//! through a Result too), PostgreSQL may stop inside an earlier arm and never make a later arm's feeders' calls. Where
//! frame/mechs.rs `arm_order` runs each arm's feeders right before the arm and the LIMIT's rows already read stop them
//! (rerun/union_arms.rs `full_fed`: the feeder's `Check::Full`, result/gate.rs), they run only where PostgreSQL starts the
//! arm. A feeder that still runs early (a union the order does not take, a count read at bind, an outer LIMIT, an end
//! that reads fewer rows than the LIMIT) and whose VOLATILE calls the user sees (effects/visible.rs `calls_visible`)
//! refuses by name. With no bound asked (the arms counted, sorted or returned
//! whole) PostgreSQL runs every arm too, so the union stays served (e2e m8_d1085_once's top-level unions). A feeder that only raises is not
//! fenced: its error is held to the arm's first read (errorhold). A request for no row (LIMIT 0) is not fenced either: the
//! feeders check the count first and run nothing (result/gate.rs, D1716). D4124: bound or not, a loud feeder making a call
//! whose place shows (effects/visible.rs `order_shown`: a sequence, clock or user call) that runs before a pipeline of an
//! earlier arm making one (program order: a union `arm_order` does not take keeps every child's feeders ahead of the
//! arms) would make its calls before that arm's, an order PG's Append fixes under every plan (each arm whole in turn).
//! drain/arms.rs `arm_run` takes such a union in PG's order whatever its end; where its structural terms fail, the union
//! refuses by name (the backstop). Owner M2-SOURCES.
use gravity_decompose::effects::{Effects, Licence};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::{Op, Source};
use mcx::{CollectIn, GrowIn, PgVec};

use crate::frame::iface::Bound;
use crate::frame::px::Px;
use crate::graph::{EndKind, Gx};

pub(crate) const FEEDER_BEFORE_EARLIER: &str = "a union's later arm fed by a pipeline that makes a sequence, clock or user call ahead of an earlier arm's such call (PG's Append runs each arm whole in turn; D4124)";

pub(crate) const LATER_FEEDERS: &str = "a streamed union's later arm fed by a pipeline that makes a VOLATILE call, under a LIMIT above the union (PG's Append never starts an arm it does not reach; D1753)";

/// Whether arm `i` (its marker op `j` of pipeline `k`, the Append's plan node `node`) is fenced (`Err`: the name the
/// caller refuses by): `i` is not the first arm, a node from the Append up is asked a bound (none of zero rows) or a
/// LIMIT past the marker has a count or offset read at bind (`late_limit`, D2693), and a pipeline that feeds this arm
/// but no earlier arm of the union makes a visible VOLATILE call and is not stopped where PostgreSQL never starts the
/// arm (`full_fed`). `Ok` otherwise, with the whole-read licence where, with no bound asked, such a feeder still runs
/// early for a reader that reads whole (D2697, `shared::whole::read`), or the accepted class's (`Px::accept`) where it
/// runs early for an end that is not the sink; `None`: none runs early, or its acts are not call-local.
pub(crate) fn fenced(px: &mut Px<'_, '_>, k: usize, j: usize, node: i32, i: u16) -> Result<Option<Licence>, &'static str> {
    let gx = px.gx;
    let mcx = px.mcx();
    if i == 0 || k >= gx.graph.pipelines.len() {
        return Ok(None);
    }
    let p = &gx.pipelines()[k];
    let above = core::iter::once(node).chain(p.node_ops.iter().filter(|n| n.first_op as usize > j).map(|n| n.node_id));
    let bounds: PgVec<'_, Bound> = above.map(|n| px.request(n).bound).collect_in(mcx);
    if bounds.contains(&Bound::Rows(0)) {
        return Ok(None);
    }
    // D3585: the earlier arms' feeders are gathered only for a feeder of this arm that makes a visible call: for every arm
    // of a union whose feeders call nothing (a partitioned table's scans) that walk of every pipeline was quadratic
    let visible: PgVec<'_, usize> = feeders(gx, k, j).into_iter().filter(|&x| crate::effects::visible::calls_visible(gx, x).is_some()).collect_in(mcx);
    let (loud, earlier): (PgVec<'_, usize>, PgVec<'_, usize>) = if visible.is_empty() {
        (visible, PgVec::new_in(mcx))
    } else {
        let earlier: PgVec<'_, usize> = (0..gx.graph.pipelines.len()).filter_map(|q| marker(gx, q, node).filter(|&(_, a)| a < i).map(|(jq, _)| (q, jq))).flat_map(|(q, jq)| feeders(gx, q, jq).into_iter().chain([q])).collect_in(mcx);
        (visible.into_iter().filter(|x| !earlier.contains(x)).collect_in(mcx), earlier)
    };
    if runs_before_earlier(gx, &loud[..], &earlier[..]) {
        return Err(FEEDER_BEFORE_EARLIER);
    }
    // D2693: a LIMIT whose count or offset is read at bind asks no bound, yet may stop PG inside an earlier arm
    if bounds.iter().all(|b| *b == Bound::All) && !crate::rerun::union_arms::late_limit(gx, k, j) {
        // D2697: with no bound, a reader that stops early (a cursor's FETCH, a SQL function's lazy row) still stops PG
        // inside an earlier arm. Arm order runs arm `i`'s own child's pipelines right before it, and a stop runs
        // nothing past it; a feeder that runs earlier (a union arm order does not take, or a feeder outside the child)
        // would run where PG never starts the arm, so the program is served to a reader that takes every row alone
        // (`Px::whole_read`, the portal-kind fact: the entry delegates the others). A loud feeder makes a visible
        // VOLATILE call: it acts (its raise is not read here), so the licence asks whether every act is call-local
        let early: PgVec<'_, usize> = loud.iter().copied().filter(|&x| crate::rerun::union_arms::arm_child(gx, node, x) != Some(i as usize)).collect_in(mcx);
        if !early.is_empty() && gx.end_kind(k) == EndKind::Sink {
            return Ok(crate::ops::shared::whole::read(px, k, Effects::UNKNOWN));
        }
        // Construct C2, the accepted class: a feeder still run early for an end that is not the statement's sink holds
        // `Px::accept`'s licence (its extra VOLATILE calls, call-local builtins' alone, and what it may raise); the arm's
        // licence where every such feeder holds one
        let arena = px.arena;
        let held: PgVec<'_, Option<Licence>> = early.iter().map(|&x| px.accept(x, crate::effects::pipeline_effects(gx, arena, x, true))).collect_in(mcx);
        return Ok(held.iter().all(Option::is_some).then(|| held.first().copied().flatten()).flatten());
    }
    if loud.into_iter().any(|x| !crate::rerun::union_arms::full_fed(px, x, node)) {
        return Err(LATER_FEEDERS);
    }
    Ok(None)
}

/// D4124: a loud feeder of a later arm (`loud`) whose calls' place shows (effects/visible.rs `order_shown`: a sequence,
/// clock or user call) runs, in program order (frame/mechs.rs `arm_order`), before a pipeline of an earlier arm or of
/// its feeders (`earlier`) that makes such a call: its calls would come first where PG's Append makes the earlier arm's
/// first under every plan. Feeders that all run first (each arm hashed, its fill in arm order) or earlier arms that make
/// no such call keep PG's order; so do calls whose place shows nowhere (a data-modifying CTE's write, a read of server
/// state such as pg_lock_status).
fn runs_before_earlier(gx: &Gx<'_, '_>, loud: &[usize], earlier: &[usize]) -> bool {
    let shown = |k: usize| crate::effects::visible::order_shown(gx, k);
    let loud: PgVec<'_, usize> = loud.iter().copied().filter(|&x| shown(x)).collect_in(gx.mcx);
    if loud.is_empty() {
        return false;
    }
    let order = crate::frame::mechs::arm_order(gx.pipelines().len(), &gx.arm_runs(), gx.mcx);
    let at = |k: usize| order.iter().position(|&q| q == k);
    let Some(first) = loud.iter().filter_map(|&x| at(x)).min() else { return false };
    earlier.iter().any(|&e| at(e).is_some_and(|a| a > first) && shown(e))
}

/// Pipeline `q`'s arm marker of the Append at plan node `node`: its op index and arm.
fn marker(gx: &Gx<'_, '_>, q: usize, node: i32) -> Option<(usize, u16)> {
    let p = &gx.pipelines()[q];
    p.ops.iter().enumerate().find_map(|(j, o)| match o {
        Op::Append(AppendDesc::Arm { i, .. }) if p.node_ops.iter().rev().find(|n| n.first_op as usize <= j).map_or(p.node_id, |n| n.node_id) == node => Some((j, *i)),
        _ => None,
    })
}

/// The pipelines that feed pipeline `q` below op `j`: the producers of its breaker source and of the builds its joins
/// before `j` read, and of everything those read, transitively (a settled union's arms, a streamed run's arms included),
/// as join/build_class.rs `feed` walks them.
fn feeders<'g>(gx: &Gx<'g, '_>, q: usize, j: usize) -> PgVec<'g, usize> {
    let p = &gx.pipelines()[q];
    let src = if let Source::Breaker { slot } = p.source { Some(slot) } else { None };
    let joins = p.ops[..j].iter().filter_map(|op| match op {
        Op::Join(JoinDesc::Probe { slot, .. } | JoinDesc::Merge { slot, .. } | JoinDesc::Drain { matches: slot, .. }) => Some(*slot),
        _ => None,
    });
    let n = gx.pipelines().len();
    let lives = |k: usize, s: u8| gx.life_of(k, s).map(|l| l.0 as usize).filter(|&l| l < n && !gx.settled_work(k, s));
    let (mut out, mut todo): (PgVec<'_, usize>, PgVec<'_, usize>) = (PgVec::new_in(gx.mcx), src.into_iter().chain(joins).filter_map(|s| lives(q, s)).chain(gx.union_feeds(q)).collect_in(gx.mcx));
    while let Some(x) = todo.pop() {
        if !out.contains(&x) {
            out.push_in(x);
            todo.extend_in(gx.pipelines()[x].desc.consumes.iter().filter_map(|&s| lives(x, s)).chain(gx.union_feeds(x)).chain(gx.run_arms(x)));
        }
    }
    out
}
