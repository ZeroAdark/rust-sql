//! Material road (A) for a several-target UPDATE's check option SubPlan (regress update#90-93: an UPDATE through a WITH CHECK
//! OPTION view over a partitioned table whose qual is a correlated SubPlan). A helper of ops/append.rs `streams` / `dup`
//! (the call lines live there; HOTSPOT RULE), outside ops/ since it reads the subplan and write descs (rule RA).
//! PostgreSQL's ModifyTable pulls one row from its Append, writes it and evaluates that result relation's check options
//! before it pulls the next: every result relation's WithCheckOption qual calls the one SubPlan (its plan id), its
//! arguments read from that relation's new row. decompose places that SubPlan's step (form Nested) right before the write
//! op; the host computes the arguments over its own check slot and parks there (lowerv4 write/rowop_sub.rs: the step's
//! own `args` are never read for a check), so the step is the same in every arm. Each arm of the streamed union then
//! carries its own copy of the step's inner, as each arm's scan already carries its own copy of a view qual's SubPlan,
//! and the union streams into the write per arm instead of being kept in its Materialize form. Copied only when the
//! inner is one pipeline the copy cannot tell apart from the original: no union, no slot read, no rerun set of its own
//! (no closed pipelines, nothing kept across a re-open), a heap, index or Result source and only filters, gates,
//! projections and limits; and only for an UPDATE of several result relations whose check options alone read the
//! SubPlan (no RETURNING list or other op of the open reads it). D3373: an inner that is the emit of the SubPlan's own
//! rerun set (an aggregate's, `held_set`) is not copied but mapped again per further arm (`copy_sets`, at the statement's
//! root), each copy its own set and slots, as `map_subplan` maps a SubPlan once per calling node; append.rs hands arm `i`
//! its copy (`dup_held`). Every other plan keeps its form.
use gravity_program::{ArenaBox, ArenaVec, Cmd, Pipeline};
use types_nodes::primnodes::SubPlan;
use types_nodes::Node;

use crate::admit::Refusal;
use crate::graph::{slot_has, slot_set, Op, PipelineGraph, PipelineNode, SlotSet, Source};
use crate::ledger::Kind;
use crate::ops::shared::modifytable::WriteDesc;
use crate::ops::shared::subplan::{refers, refers_list};
use crate::ops::shared::subplan::{SubForm, SubPlanDesc, SubSite};
use crate::walk::{Builder, Open};

/// The copies `copy_sets` mapped for the step of SubPlan `plan_id` over union `union`: arm `i` > 0 takes `inners[i - 1]`.
pub(crate) struct ArmSub<'p> {
    pub union: u32,
    pub plan_id: i32,
    pub inners: ArenaVec<Option<ArenaBox<PipelineNode<'p>>>>,
}

/// A copy's mapping is not the first one's shape again (one new set holding every pipeline mapped for it).
const COPY_SHAPE: &str = "a check option SubPlan's per-arm copy mapped in another shape than its first mapping (D3373)";
/// Free breaker slots `copy_sets` leaves beyond the copies' sets.
const SLOT_SLACK: u32 = 8;

/// Whether `op`, one of the ops `ops` of an open over a streamed union, is a check option SubPlan step each arm can carry its
/// own copy of (the module doc): a copy of its inner, or (`held`: its plan's copies are mapped) its own set's emit.
pub(crate) fn per_arm<'p>(g: &PipelineGraph<'p>, op: &Op<'p>, ops: &[Op<'p>], held: &dyn Fn(i32) -> bool) -> bool {
    let Some((id, inner)) = check_step(op, ops) else { return false };
    let own = !g.reruns.iter().any(|r| r.plan_id == id) && copyable(inner);
    own || held(id) && held_set(g, id, inner).is_some()
}

/// `op`'s plan id and embedded inner when it is a nested check option SubPlan step of a several-target UPDATE no other op
/// of `ops` reads.
fn check_step<'a, 'p>(op: &'a Op<'p>, ops: &[Op<'p>]) -> Option<(i32, &'a PipelineNode<'p>)> {
    let Op::SubPlan(d) = op else { return None };
    let SubForm::Nested { inner } = &d.form else { return None };
    let id = d.plan_id;
    let w = write_of(ops)?;
    let checks = w.checks.iter().any(|c| refers(c.qual, id));
    // RETURNING (any target's list) or another op of the open reads it: its arguments would be the step's own, in the
    // first target's layout
    let returning = w.targets.iter().any(|t| t.returning.is_some_and(|r| refers_list(r, id)));
    let elsewhere = ops.iter().any(|o| match o {
        Op::Filter(n) | Op::Gate(n) => refers(*n, id),
        Op::Project(l) => refers_list(l, id),
        _ => false,
    });
    let write = w.cmd == Cmd::Update && w.targets.len() > 1 && w.merge.is_none() && w.on_conflict.is_none();
    (write && checks && !returning && !elsewhere && d.site == SubSite::Row).then_some((id, &**inner))
}

fn write_of<'a, 'p>(ops: &'a [Op<'p>]) -> Option<&'a WriteDesc<'p>> {
    ops.iter().find_map(|o| if let Op::Write(w) = o { Some(w) } else { None })
}

/// The graph.reruns index of the set of SubPlan `id` whose emit `inner` is: one pipeline of filters, gates, projections and
/// limits over a member's slot, every slot it and the members read filled by a member, no member kept from a first run.
fn held_set(g: &PipelineGraph<'_>, id: i32, inner: &PipelineNode<'_>) -> Option<usize> {
    let Source::Breaker { slot } = inner.source else { return None };
    let producer = crate::breakers::decl_of(g, slot)?.producer;
    let i = g.reruns.iter().position(|r| r.plan_id == id && r.pipelines.contains(&producer))?;
    let r = &g.reruns[i];
    let made = |s: &u8| crate::breakers::decl_of(g, *s).is_some_and(|d| r.pipelines.contains(&d.producer));
    let ops = inner.ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::Limit { .. }));
    let members = r.pipelines.iter().all(|&q| g.pipelines.get(q as usize).is_some_and(|p| p.desc.consumes.iter().all(made)));
    let kept = r.pipelines.iter().any(|q| g.kept.contains(q)) || g.kept_by.iter().any(|k| k.plan_id == id);
    (inner.union.is_none() && ops && inner.desc.consumes.iter().all(made) && members && !kept).then_some(i)
}

/// D3373: `open` (the statement's root) with the copies `ArmSub` holds mapped for each check option step whose inner is its
/// own set's emit (`held_set`), when its union then streams into the write (`union::streams`). PostgreSQL's one SubPlan
/// rescans its plan at every call (ExecScanSubPlan), so where the rescan keeps nothing (`rerun::rescan_keeps`) and the
/// plan's rows are the same at every run with the same arguments (`rescan_stable`), each arm's own set computes what that
/// call computes. The plan's first mapping must be one set holding every pipeline mapped for it (`Builder::nested_spans`);
/// too few free slots for the copies' sets keeps the Materialize form.
pub(crate) fn copy_sets<'p>(b: &mut Builder<'p, '_>, open: Open<'p>) -> Result<Open<'p>, Refusal> {
    let Some(u) = open.union else { return Ok(open) };
    let g = &b.graph;
    let rec = &g.unions[u as usize];
    let n = rec.arms.len();
    let mut want: ArenaVec<(i32, usize)> = ArenaVec::new_in(b.arena); // alloc: Cold
    for op in open.ops.iter() {
        let Some((id, inner)) = check_step(op, &open.ops) else { continue };
        if !g.reruns.iter().any(|r| r.plan_id == id) && copyable(inner) {
            continue;
        }
        let one = g.reruns.iter().filter(|r| r.plan_id == id).count() == 1 && b.nested_spans.iter().filter(|s| s.0 == id).count() == 1;
        let span = b.nested_spans.iter().find(|s| s.0 == id).map(|s| s.1..s.2);
        match held_set(g, id, inner) {
            Some(i) if one && span.is_some_and(|r| g.reruns[i].pipelines.iter().copied().eq(r)) => want.push((id, i)),
            _ => return Ok(open),
        }
    }
    let unordered_run = !rec.order.is_known() && open.ops.iter().any(|op| matches!(op, Op::GroupRun(_)));
    if want.is_empty() || n < 2 || unordered_run || !crate::ops::shared::union::streams(g, &open, &rec.arms, rec.order.is_known(), &|id| want.iter().any(|w| w.0 == id)) {
        return Ok(open);
    }
    let mut taken = crate::breakers::leased(g);
    for &s in open.desc.consumes.iter() {
        slot_set(&mut taken, s);
    }
    let free = gravity_program::limits::BREAKERS as u32 - taken.iter().map(|w| w.count_ones()).sum::<u32>();
    let sets: u32 = want.iter().map(|&(_, i)| g.reruns[i].slots.iter().map(|w| w.count_ones()).sum::<u32>()).sum();
    if sets * (n as u32 - 1) + SLOT_SLACK > free {
        return Ok(open);
    }
    let w = write_of(&open.ops).expect("check_step found the write");
    let mut plans: ArenaVec<(i32, usize, Node<'p>, &'p SubPlan<'p>)> = ArenaVec::with_capacity_in(want.len(), b.arena); // alloc: Cold
    for &(id, i) in want.iter() {
        let (Some(s), Some(root)) = (sub_of(w, id), crate::rerun::subplan_root(b.stmt, id)) else { return Ok(open) };
        if crate::rerun::rescan_keeps(b, s, &[]) || !crate::rerun::of_plan(b.stmt, b.facts, b.arena, root).rescan_stable {
            return Ok(open);
        }
        plans.push((id, i, root, s));
    }
    for (id, i, root, s) in plans {
        let (node_id, len0) = (b.graph.reruns[i].node_id, b.graph.reruns[i].pipelines.len());
        let mut inners = ArenaVec::with_capacity_in(n - 1, b.arena); // alloc: Cold
        for _ in 1..n {
            let (p0, sets0) = (b.graph.pipelines.len(), b.graph.reruns.len());
            let inner = b.plan(root)?;
            let inner = crate::ops::shared::union::settle(&mut b.graph, inner);
            b.closed_by(root);
            let mut params: ArenaVec<i32> = ArenaVec::new_in(b.arena); // alloc: Cold
            params.extend(s.parParam.iter());
            crate::rerun::declare_rerun(b, p0, node_id, id, &params)?;
            crate::rerun::declare_sub_kept(b, p0, node_id, id, &inner);
            let g = &b.graph;
            let fresh = g.reruns.len() == sets0 + 1 && g.reruns[sets0].pipelines.iter().copied().eq(p0 as u16..g.pipelines.len() as u16);
            if !fresh || g.reruns[i].pipelines.len() != len0 || held_set(g, id, &inner) != Some(sets0) {
                return Err(Refusal::shape(Kind::SubPlan, node_id, COPY_SHAPE));
            }
            for &c in inner.desc.consumes.iter() {
                b.graph.reads[c as usize] += 1;
            }
            inners.push(Some(ArenaBox::new_in(inner, b.arena)));
        }
        b.graph.arm_subs.push(ArmSub { union: u, plan_id: id, inners });
    }
    Ok(open)
}

/// The SubPlan node of plan `id` a check option of `w` calls.
fn sub_of<'p>(w: &WriteDesc<'p>, id: i32) -> Option<&'p SubPlan<'p>> {
    let mut hit: Option<&'p SubPlan<'p>> = None;
    for c in w.checks.iter() {
        let mut see = |n: Node<'p>| {
            crate::expr::walk(n, &mut |x| {
                if let Some(s) = x.as_sub_plan().filter(|s| s.plan_id == id) {
                    hit.get_or_insert(s);
                }
                hit.is_none()
            });
        };
        match c.qual.as_list() {
            Some(l) => l.iter().for_each(&mut see),
            None => see(c.qual),
        }
    }
    hit
}

/// The copies of union `u` (`copy_sets`), taken out of the graph for its push.
pub(crate) fn take_subs<'p>(g: &mut PipelineGraph<'p>, u: u32) -> ArenaVec<ArmSub<'p>> {
    let mut out = ArenaVec::new_in(*g.pipelines.allocator()); // alloc: Cold
    let mut j = 0;
    while j < g.arm_subs.len() {
        if g.arm_subs[j].union == u {
            out.push(g.arm_subs.remove(j));
        } else {
            j += 1;
        }
    }
    out
}

/// The slots the steps `subs` copies read in `ops` (the parent's): each arm reads its own copy's instead.
pub(crate) fn held_slots(ops: &[Op<'_>], subs: &[ArmSub<'_>]) -> SlotSet {
    let mut set = [0u64; 4];
    for op in ops {
        if let Op::SubPlan(SubPlanDesc { plan_id, form: SubForm::Nested { inner }, .. }) = op {
            if subs.iter().any(|s| s.plan_id == *plan_id) {
                inner.desc.consumes.iter().for_each(|&s| slot_set(&mut set, s));
            }
        }
    }
    set
}

/// `d` without the slots of `skip`.
pub(crate) fn without(arena: gravity_program::ArenaRef, d: Pipeline, skip: &SlotSet) -> Pipeline {
    if !d.consumes.iter().any(|&s| slot_has(skip, s)) {
        return d;
    }
    let named = d.consumes_from.len() == d.consumes.len();
    let mut out = d;
    out.consumes = &[];
    out.consumes_from = &[];
    for (j, &s) in d.consumes.iter().enumerate().filter(|(_, s)| !slot_has(skip, **s)) {
        out = match named {
            true => out.consuming_from_in(arena, s, d.consumes_from[j]),
            false => out.consuming_in(arena, s),
        };
    }
    out
}

/// Arm `i`'s copy of step `op` when `subs` holds its plan's copies: arm 0 a copy of the step's own inner over the same slot,
/// arm `i` > 0 the inner `copy_sets` mapped for it; `None` for any other op.
pub(crate) fn dup_held<'p>(op: &Op<'p>, i: usize, subs: &mut [ArmSub<'p>], dup: fn(&Op<'p>) -> Option<Op<'p>>) -> Option<Op<'p>> {
    let Op::SubPlan(d) = op else { return None };
    let SubForm::Nested { inner } = &d.form else { return None };
    let sub = subs.iter_mut().find(|s| s.plan_id == d.plan_id)?;
    let inner = match i {
        0 => copy_emit(inner, dup)?,
        _ => sub.inners.get_mut(i - 1)?.take()?,
    };
    let form = SubForm::Nested { inner };
    Some(Op::SubPlan(SubPlanDesc { plan_id: d.plan_id, link: d.link, test: d.test, args: d.args, params: d.params, sets: d.sets, ids: d.ids, site: d.site, conditional: d.conditional, volatility: d.volatility, raise_free: d.raise_free, quiet_but_rows: d.quiet_but_rows, invariant: d.invariant, unknown_eq_false: d.unknown_eq_false, alternative: d.alternative, set_spool: d.set_spool, stops: d.stops, ahead: d.ahead, call: d.call, form }))
}

/// Arm `a`'s desc reading the inners of its copied steps (`dup_held`); their reads were counted when each was mapped.
pub(crate) fn embed_held<'p>(g: &mut PipelineGraph<'p>, a: &mut PipelineNode<'p>, subs: &[ArmSub<'p>]) {
    let arena = *g.pipelines.allocator();
    for op in a.ops.iter() {
        let Op::SubPlan(SubPlanDesc { plan_id, form: SubForm::Nested { inner }, .. }) = op else { continue };
        if !subs.iter().any(|s| s.plan_id == *plan_id) {
            continue;
        }
        match crate::breakers::embed(arena, a.desc, &inner.desc, None) {
            Ok(d) => a.desc = d,
            Err(_) => {
                g.fault.get_or_insert(Refusal::shape(Kind::SubPlan, a.node_id, COPY_SHAPE));
            }
        }
    }
}

/// A copy of `inner`, its own set's emit (`held_set`), over the same slot.
fn copy_emit<'p>(inner: &PipelineNode<'p>, dup: fn(&Op<'p>) -> Option<Op<'p>>) -> Option<ArenaBox<PipelineNode<'p>>> {
    let Source::Breaker { slot } = inner.source else { return None };
    let arena = *inner.ops.allocator();
    let mut ops = ArenaVec::with_capacity_in(inner.ops.len(), arena); // alloc: Cold
    for o in inner.ops.iter() {
        ops.push(dup(o)?);
    }
    let copy = PipelineNode { node_id: inner.node_id, source: Source::Breaker { slot }, ops, desc: inner.desc, union: None, node_ops: inner.node_ops.clone(), mark: inner.mark };
    Some(ArenaBox::new_in(copy, arena))
}

/// A copy of `inner` (`per_arm` checked it is `copyable`), its ops copied by `dup` (append.rs).
pub(crate) fn dup_inner<'p>(inner: &PipelineNode<'p>, dup: fn(&Op<'p>) -> Option<Op<'p>>) -> Option<ArenaBox<PipelineNode<'p>>> {
    if !copyable(inner) {
        return None;
    }
    let arena = *inner.ops.allocator();
    let mut ops = ArenaVec::with_capacity_in(inner.ops.len(), arena); // alloc: Cold
    for o in inner.ops.iter() {
        ops.push(dup(o)?);
    }
    let source = source(&inner.source)?;
    let copy = PipelineNode { node_id: inner.node_id, source, ops, desc: inner.desc, union: None, node_ops: inner.node_ops.clone(), mark: inner.mark };
    Some(ArenaBox::new_in(copy, arena))
}

/// One pipeline whose copy runs as the original does (the module doc).
fn copyable(inner: &PipelineNode<'_>) -> bool {
    let ops = inner.ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::Limit { .. }));
    inner.union.is_none() && inner.desc.consumes.is_empty() && matches!(inner.source, Source::Range { .. } | Source::Index { .. } | Source::Const) && ops
}

fn source<'p>(s: &Source<'p>) -> Option<Source<'p>> {
    Some(match s {
        Source::Range { site, am, scanrelid, relid } => Source::Range { site: site.clone(), am: *am, scanrelid: *scanrelid, relid: *relid },
        Source::Index { site, am, scanrelid, relid, indexid, keys, only, backward, recheck, iam } => {
            Source::Index { site: site.clone(), am: *am, scanrelid: *scanrelid, relid: *relid, indexid: *indexid, keys: keys.clone(), only: *only, backward: *backward, recheck: *recheck, iam: *iam }
        }
        Source::Const => Source::Const,
        _ => return None,
    })
}
