use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
use gravity_program::{limits, ArenaVec, End, LifeId, Pipeline};

use crate::graph::{slot_has, slot_set, slot_unset, BreakerDecl, Layout, Order, PipelineGraph, SlotSet};
use crate::walk::{Builder, Open};

pub fn live_slots(g: &PipelineGraph<'_>) -> SlotSet {
    let mut live = [0u64; 4];
    for p in g.pipelines.iter() {
        for c in p.desc.consumes.iter() {
            slot_unset(&mut live, *c);
        }
        if let Some(s) = p.desc.produces() {
            slot_set(&mut live, s);
        }
    }
    live
}

/// The slots the lives still lease (S6, no pin kept by hand): a life not yet read by a pushed consumer, a slot an open
/// pipeline reads (an iteration driver's re-run range included), a rerun set's, a CTE's or a shared hashed SubPlan's set's
/// (D1920) slots for the statement, every slot a recursive term being mapped has filled or read (`term_from`), and every
/// slot a held re-run range filled or read, for the statement (`PipelineGraph::held`, D3646). A grouping-set cascade's
/// slots are leased by their reads alone (`consuming`'s `cascade_reads`, D4620).
pub fn leased(g: &PipelineGraph<'_>) -> SlotSet {
    let mut taken = live_slots(g);
    for (s, _) in g.reads.iter().enumerate().filter(|(_, n)| **n > 0) {
        slot_set(&mut taken, s as u8);
    }
    for r in g.reruns.iter() {
        for (t, h) in taken.iter_mut().zip(r.slots.iter()) {
            *t |= *h;
        }
    }
    for s in g.init_plans.iter().filter_map(|i| i.slot).chain(g.shared_sets.iter().map(|&(_, s)| s)) {
        slot_set(&mut taken, s);
    }
    for p in g.pipelines.iter().skip(g.term_from.map_or(usize::MAX, usize::from)) {
        for s in p.desc.consumes.iter().chain(p.desc.produces().iter()) {
            slot_set(&mut taken, *s);
        }
    }
    for &(from, end) in g.held.iter() {
        for p in g.pipelines.iter().take(end as usize).skip(from as usize) {
            for s in p.desc.consumes.iter().chain(p.desc.produces().iter()) {
                slot_set(&mut taken, *s);
            }
        }
    }
    taken
}

/// D3646: hold the slots of a re-run range while it is mapped (a nest's inner, D4584: with or without a NestLoopParam; a
/// correlated nested SubPlan's plan), as a recursive term's are (`term_from`): its rerun set is fixed only once the range
/// is mapped (`rerun::declare_rerun`), and a later pipeline of the range taking a slot an earlier one read (a kept hash
/// build's) would refill it under the set's next run (lifetime/rerun.rs: 'a slot a rerun set reads refilled by a later
/// pipeline'). Returns the hold before, which `release_range` puts back once the range is mapped.
pub fn hold_range(g: &mut PipelineGraph<'_>, p0: usize) -> Option<u16> {
    let before = g.term_from;
    g.term_from = Some(before.map_or(p0 as u16, |t| t.min(p0 as u16)));
    before
}

/// The range `hold_range` held from `p0` is mapped: the hold before comes back, and the range is recorded
/// (`PipelineGraph::held`), whose slots `leased` keeps for the statement: the range runs inside the owner's pipeline, a
/// fill of it the lowering builds lazily (a Hash under the nest, the HashAgg feeding it) at the first re-open, so a slot
/// it fills is live while a later pipeline (the owner's Sort) has its own open ('slot produced again', L3). D4584: every
/// nest's inner is held and recorded, a param-free one too.
pub fn release_range(g: &mut PipelineGraph<'_>, p0: usize, before: Option<u16>) {
    g.term_from = before;
    let end = g.pipelines.len();
    if end > p0 {
        g.held.push((p0 as u16, end as u16)); // alloc: Cold
    }
}

/// The lowest free slot (`limits::BREAKERS` of them: the breaker record's `u8`).
pub fn free_slot(g: &PipelineGraph<'_>, producer: &Open<'_>) -> Option<u8> {
    let mut taken = leased(g);
    for c in producer.desc.consumes.iter() {
        slot_set(&mut taken, *c);
    }
    (0..limits::BREAKERS as u16).map(|s| s as u8).find(|&s| !slot_has(&taken, s))
}

/// The slots every pipeline after the work table's life up to the body fills or reads again per generation.
pub fn iter_slots(g: &PipelineGraph<'_>, it: &crate::ops::shared::setop::IterDesc) -> SlotSet {
    let mut set = [0u64; 4];
    let first = decl_of(g, it.work).map_or(it.body as usize, |d| d.producer as usize + 1);
    for p in g.pipelines.iter().take(it.body as usize + 1).skip(first) {
        for s in p.desc.consumes.iter().chain(p.desc.produces().iter()) {
            slot_set(&mut set, *s);
        }
    }
    set
}

/// The re-run ranges a driver's `Iter` ops lease: taken by `lease_iter`, returned by `PipelineGraph::push`.
pub fn iter_leases(g: &PipelineGraph<'_>, ops: &[crate::graph::Op<'_>]) -> SlotSet {
    let mut set = [0u64; 4];
    for op in ops {
        if let crate::graph::Op::SetOp(crate::ops::shared::setop::SetDesc::Iter(it)) = op {
            for (t, h) in set.iter_mut().zip(iter_slots(g, it).iter()) {
                *t |= *h;
            }
        }
    }
    set
}

/// Push `it` onto the driver `open` and lease its re-run range to it.
pub fn lease_iter<'p>(b: &mut Builder<'p, '_>, open: &mut Open<'p>, it: crate::ops::shared::setop::IterDesc) {
    let set = iter_slots(&b.graph, &it);
    for s in (0..=u8::MAX).filter(|s| slot_has(&set, *s)) {
        b.graph.reads[s as usize] += 1;
    }
    open.push(crate::graph::Op::SetOp(crate::ops::shared::setop::SetDesc::Iter(it)));
}

/// `open`, a bare emit over its slot, is dropped for the slot itself (`sort::spool`): its read goes with it.
pub fn unread(g: &mut PipelineGraph<'_>, slot: u8) {
    g.reads[slot as usize] = g.reads[slot as usize].saturating_sub(1);
}

/// `open` filling a breaker as one pass in its rows' order (a WindowAgg's partition buffer straight over an Append): its
/// union kept in the Materialize form (append.rs `settle`: the arms feed the union's slot, one emit reads it in the arms'
/// order), since lowerv4 continues no Materialize end over a streamed union's arms (append/lower.rs D393); any other
/// open unchanged.
pub fn one_fill<'p>(g: &mut PipelineGraph<'p>, open: Open<'p>) -> Open<'p> {
    crate::ops::shared::union::settle(g, open)
}

pub fn declare<'p>(g: &mut PipelineGraph<'p>, producer: Open<'p>, kind: BreakerKind, layout: Layout<'p>, budget: Budget, spill: Spill, order: Order) -> u8 {
    let producer = crate::ops::shared::union::resolve(g, producer);
    let slot = free_slot(g, &producer).expect("checked by the caller: a free breaker slot");
    declare_breaker(g, producer, Breaker { kind, slot, bytes: 0, budget, spill }, layout, order)
}

/// `declare` with the breaker given whole (a sized `PlainAgg`, its `slot` the caller's `free_slot`) and read `order`.
pub fn declare_breaker<'p>(g: &mut PipelineGraph<'p>, producer: Open<'p>, b: Breaker, layout: Layout<'p>, order: Order) -> u8 {
    declare_producers(g, producer, b, layout, order);
    b.slot
}

pub fn declare_producers<'p>(g: &mut PipelineGraph<'p>, mut producer: Open<'p>, b: Breaker, layout: Layout<'p>, order: Order) -> ArenaVec<u16> {
    producer.desc.ends_in = End::Breaker(b);
    let ends = g.push(producer);
    for &p in ends.iter() {
        g.breakers.push(BreakerDecl { breaker: b, layout, producer: p, order });
    }
    ends
}

/// The latest declaration of a slot (the one an open consumer of it reads).
pub fn decl_of<'a, 'p>(g: &'a PipelineGraph<'p>, slot: u8) -> Option<&'a BreakerDecl<'p>> {
    g.breakers.iter().rev().find(|d| d.breaker.slot == slot)
}

/// A settled union's arms (append.rs `settle_arms`, a nest's embedded Append inner): `emit` reads its slot and opens with
/// `Union { slot, children }`, and the slot's last `children` declarations are the arm pipelines that feed it, each
/// ending in its `Feed`; None for any other open, or when the declarations do not read so.
pub fn union_arms<'a, 'p>(g: &'a PipelineGraph<'p>, emit: &Open<'p>) -> Option<impl Iterator<Item = &'a Open<'p>> + Clone> {
    use crate::ops::shared::append::AppendDesc;
    let crate::graph::Source::Breaker { slot } = emit.source else { return None };
    let Some(crate::graph::Op::Append(AppendDesc::Union { slot: s, children, .. })) = emit.ops.first() else { return None };
    let n = *children as usize;
    let feeds = |p: &Open<'p>| matches!(p.ops.last(), Some(crate::graph::Op::Append(AppendDesc::Feed { slot: f, .. })) if *f == slot);
    let arms = g.breakers.iter().rev().filter(move |d| d.breaker.slot == slot).take(n).filter_map(move |d| g.pipelines.get(d.producer as usize));
    (*s == slot && n > 0 && arms.clone().count() == n && arms.clone().all(feeds)).then_some(arms)
}

/// `desc` consuming `slot`, reading the slot's latest declared life (every consumer names the life it reads; a slot
/// with no producer yet, a recursive work table's, leaves the pipeline's lives unnamed).
pub fn consuming(b: &mut Builder<'_, '_>, desc: Pipeline, slot: u8) -> Pipeline {
    let life = decl_of(&b.graph, slot).map(|d| LifeId(d.producer));
    let desc = read_life(b, desc, slot, life);
    cascade_reads(b, desc, slot)
}

/// `desc` reading `slot` from `life` (its latest declaration's), one more read of the slot.
fn read_life(b: &mut Builder<'_, '_>, desc: Pipeline, slot: u8, life: Option<LifeId>) -> Pipeline {
    b.graph.reads[slot as usize] += 1;
    match life {
        Some(l) if desc.consumes_from.len() == desc.consumes.len() => desc.consuming_from_in(b.arena, slot, l),
        _ => {
            let mut d = desc.consuming_in(b.arena, slot);
            d.consumes_from = &[];
            d
        }
    }
}

/// D4620: `desc`, now reading `slot`, also reading a grouping-set cascade's base and cascaded sets when `slot` is a Sort or
/// TopK its producer filled from the base's emit (that producer reads the base's slot from the base's life): a drain of
/// that top reads the records of every one by pointer, past the emit (lowerv4 topn `make_drain` holds their lives). So
/// their slots stay leased exactly while a reader of the top is open (S6, no pin past its reads), and no breaker its
/// pipeline opens (the drain's own) takes one of their numbers. A reader that keeps the top's rows by value reads them
/// no further: so a bare emit over the top, reading the cascade's slots too, is spooled (`sort::spool` drops only a
/// one-slot emit), its rows a Materialize's by value, and no later reader of the top's slot reads past their lease.
fn cascade_reads(b: &mut Builder<'_, '_>, mut desc: Pipeline, slot: u8) -> Pipeline {
    if b.graph.cascades.is_empty() {
        return desc;
    }
    let Some(top) = decl_of(&b.graph, slot).filter(|d| matches!(d.breaker.kind, BreakerKind::Sort | BreakerKind::TopK)).map(|d| d.producer as usize) else {
        return desc;
    };
    // bounded("other: one pass over the graph's cascades, at most limits::CASCADE_SETS_MAX per grouping-sets node")
    for i in 0..b.graph.cascades.len() {
        let c = &b.graph.cascades[i];
        let (base_slot, base, set, life) = (c.base_slot, c.base, c.slot, c.producer);
        let Some(t) = b.graph.pipelines.get(top) else { return desc };
        let reads_base = t.desc.consumes.iter().enumerate().any(|(j, &s)| s == base_slot && t.desc.consumes_from.get(j).is_none_or(|&l| l == base));
        if !reads_base {
            continue;
        }
        for (s, l) in [(base_slot, base), (set, life)] {
            if !desc.consumes.contains(&s) {
                desc = read_life(b, desc, s, Some(l));
            }
        }
    }
    desc
}

/// `d`, the pipeline an inner is embedded in (a nest's, a nested SubPlan's), also reading every slot the inner reads (main
/// 07:52Z): appended after `d`'s own in the inner's order, each from the inner's life, so the program pipeline that runs
/// the inner holds its regions. The inner keeps its entry. A slot `d` reads already stays one entry; another life of it
/// is `Err(slot)`. `reads`: an open `d`'s reader counts, one more per slot (its push takes it back, as `consuming`'s):
/// the inner's own read, never pushed, keeps the slot leased as before.
pub fn embed(arena: gravity_program::ArenaRef, mut d: Pipeline, inner: &Pipeline, mut reads: Option<&mut [u16; 256]>) -> Result<Pipeline, u8> {
    let named = inner.consumes_from.len() == inner.consumes.len();
    for (j, &s) in inner.consumes.iter().enumerate() {
        let life = inner.consumes_from.get(j).copied().filter(|_| named);
        if let Some(k) = d.consumes.iter().position(|&x| x == s) {
            match (d.consumes_from.get(k), life) {
                (Some(&have), Some(l)) if have != l => return Err(s),
                _ => continue,
            }
        }
        if let Some(r) = reads.as_deref_mut() {
            r[s as usize] += 1;
        }
        d = match life {
            Some(l) if d.consumes_from.len() == d.consumes.len() => d.consuming_from_in(arena, s, l),
            _ => {
                let mut x = d.consuming_in(arena, s);
                x.consumes_from = &[];
                x
            }
        };
    }
    Ok(d)
}

pub fn life_read(b: &Builder<'_, '_>, desc: &Pipeline, slot: u8) -> Option<LifeId> {
    let j = desc.consumes.iter().position(|&s| s == slot);
    match j.and_then(|j| desc.consumes_from.get(j)) {
        Some(&l) => Some(l),
        None => decl_of(&b.graph, slot).map(|d| LifeId(d.producer)),
    }
}
