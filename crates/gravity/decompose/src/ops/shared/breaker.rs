//! Closing an open into a breaker slot and reading it back (a Sort, TopK or Materialize slot, a spool): the
//! builders every family that keeps rows calls.
use gravity_abi::breaker::{BreakerKind, Budget, Spill};
use gravity_program::{limits, End, Pipeline, Reason};

use crate::admit::Refusal;
use crate::breakers;
use crate::graph::{Layout, Op, Order, Source};
use crate::ledger::Kind;
use crate::ops::shared::order::order_of;
use crate::ops::shared::sort::ROW_LAYOUT;
use crate::walk::{Builder, Open};

pub fn close<'p>(b: &mut Builder<'p, '_>, open: Open<'p>, kind: BreakerKind, layout: Layout<'p>, budget: Budget, spill: Spill, k: Kind, id: i32) -> Result<u8, Refusal> {
    let open = crate::ops::shared::union::resolve(&mut b.graph, open);
    if b.graph.pipelines.len() + 2 > limits::PIPELINES {
        return Err(Refusal::shape(k, id, "more than 65535 pipelines (the u16 pipeline id)"));
    }
    if breakers::free_slot(&b.graph, &open).is_none() {
        return Err(Refusal::shape(k, id, "more than 256 live breaker slots (the u8 breaker slot id, limits::BREAKERS)"));
    }
    let order = match kind {
        BreakerKind::Sort | BreakerKind::TopK => order_of(b, &open),
        BreakerKind::Materialize if !open.desc.is_eligible() => order_of(b, &open),
        _ => Order::NONE,
    };
    Ok(breakers::declare(&mut b.graph, open, kind, layout, budget, spill, order))
}

pub fn consume<'p>(b: &mut Builder<'p, '_>, open: &mut Open<'p>, slot: u8) {
    open.desc = breakers::consuming(b, open.desc, slot);
}

pub fn emit<'p>(b: &mut Builder<'p, '_>, node_id: i32, slot: u8) -> Open<'p> {
    let mut open = Open::new(b.arena, node_id, Source::Breaker { slot }, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
    consume(b, &mut open, slot);
    open
}

/// The open pipeline's rows as a breaker slot: a bare emit over a breaker (a Sort's) is that breaker; anything else is spooled.
pub fn spool<'p>(b: &mut Builder<'p, '_>, open: Open<'p>, k: Kind, id: i32) -> Result<u8, Refusal> {
    let open = crate::ops::shared::union::settle(&mut b.graph, open);
    if let Source::Breaker { slot } = open.source {
        if open.ops.len() == 1 && matches!(open.ops[0], Op::Project(_)) && open.desc.nconsumed() == 1 {
            breakers::unread(&mut b.graph, slot);
            // the dropped emit's nodes are counted at the producer's end: its breaker entry stops being a duplicate
            if let Some(p) = breakers::decl_of(&b.graph, slot).map(|d| d.producer as usize) {
                let q = &mut b.graph.pipelines[p];
                for e in open.node_ops.iter() {
                    match q.node_ops.last_mut() {
                        Some(t) if t.node_id == e.node_id && t.producer_top => t.producer_top = false,
                        _ => q.node_ops.push(crate::graph::NodeOps { node_id: e.node_id, first_op: q.ops.len() as u16, producer_top: false }),
                    }
                }
            }
            return Ok(slot);
        }
    }
    close(b, open, BreakerKind::Materialize, ROW_LAYOUT, Budget::Share, Spill::Runs, k, id)
}

/// A merge join's inner as one sealed run its reader indexes by row (lowerv4 face/slot_run.rs): a bare emit over a Sort's
/// run or a Materialize is that breaker (`spool`); a bare emit over any other breaker (a sorted group's table, a DISTINCT's,
/// a top-k's: CNM-26 SLOTRUN) holds no run to index, so its rows are spooled in their arrival order, as any other input's.
/// D3817: so is a bare CteScan (PG 18 keeps a CTE's sort order, so the planner puts no Sort over it): the CTE's store
/// fills only at its readers' pulls (lowerv4 ops/ctescan/lower.rs, `m8_cte_pull`), so taken as the merge's run it was read
/// before any reader pulled it (pgx table_bloat_check's anti join read no_stats empty); spooled, its reader pulls it.
pub fn spool_run<'p>(b: &mut Builder<'p, '_>, open: Open<'p>, k: Kind, id: i32) -> Result<u8, Refusal> {
    // a streamed union settles (in `spool`) into its own slot's emit, never a bare emit over another breaker
    let cte = |slot: u8| b.ctes.iter().any(|&(_, s)| s == slot);
    let other = open.union.is_none() && matches!(open.source, Source::Breaker { slot } if cte(slot) || !breakers::decl_of(&b.graph, slot).is_some_and(|d| matches!(d.breaker.kind, BreakerKind::Sort | BreakerKind::Materialize)));
    match other {
        true => close(b, open, BreakerKind::Materialize, ROW_LAYOUT, Budget::Share, Spill::Runs, k, id),
        false => spool(b, open, k, id),
    }
}

/// The respool: once the limit of decision 24 (two consumed slots), now a cost choice (decision 82): a
/// pipeline consumes any number of slots, and materialising its rows to consume one more writes and
/// rereads every row for nothing — the sets it consumes stay leased either way — so the choice today
/// is never; the hook stays for a cost that says otherwise (a probe row far wider than the build sets).
pub fn respool<'p>(b: &mut Builder<'p, '_>, open: Open<'p>, k: Kind, id: i32) -> Result<Open<'p>, Refusal> {
    if !RESPOOL_WIDE_ROWS || open.desc.nconsumed() < 2 {
        return Ok(open);
    }
    let slot = close(b, open, BreakerKind::Materialize, ROW_LAYOUT, Budget::Share, Spill::Runs, k, id)?;
    Ok(emit(b, id, slot))
}

/// The cost choice of `respool`: off — no measured shape wants the spool (decision 82).
const RESPOOL_WIDE_ROWS: bool = false;
