//! A nest over a plan Material copied into every arm of a several-target write's streamed union (material, regress
//! rowsecurity#342 / #343: `UPDATE t1 t1_1 SET .. FROM t1 t1_2 WHERE ..`, PostgreSQL's Nested Loop over an Append of the
//! targets with a Materialize over a second Append as its inner). ops/append.rs `copies` / `dup` call these (the HOTSPOT
//! rule keeps the helpers here, outside the ops families: rule RA). PostgreSQL runs one Nested Loop whose outer Append
//! hands it the targets' rows arm after arm, and one Materialize: its child runs once, at the first rescan, and every later
//! rescan rewinds the store. A copy per arm keeps that: each copy's inner reads the same Material slot (append.rs counts a
//! read per arm), so the store filled once serves every arm, and the per-arm write (V4R-214) keeps ModifyTable's per-child
//! order. Only that per-child shape: every arm scans one of the write's result relations (the nest's outer side is the
//! targets' Append; a union whose arms are not the targets, the result rows coming from the inner, keeps its settled form
//! and its one reader's pull, regress inherit#145), and only a nest with no parameters (nothing changes per outer row, so
//! no rescan refills the store) and no rerun set of its own, whose inner is one pipeline of filters and projections over a
//! plan Material's slot. Which reader fills the store, and when, is lowerv4's (rerun/spool/readers.rs, form.rs, lazy.rs).
use gravity_program::{ArenaBox, ArenaVec, Cmd};

use crate::graph::{Op, PipelineGraph, PipelineNode, Source};
use crate::join::JoinDesc;

/// Nest `op` among `ops` (a streamed union's parent ops, the union's `arms`) may be copied into every arm (the module doc).
pub(crate) fn per_arm(g: &PipelineGraph<'_>, op: &Op<'_>, ops: &[Op<'_>], arms: &[PipelineNode<'_>]) -> bool {
    let Op::Join(JoinDesc::Nest { params, inner, facts, .. }) = op else { return false };
    let targets = |w: &crate::ops::shared::modifytable::WriteDesc<'_>| arms.iter().all(|a| a.source.relation().is_some_and(|(rti, _, _)| w.targets.iter().any(|t| t.rti == u32::from(rti))));
    let write = ops.iter().any(|o| matches!(o, Op::Write(w) if w.targets.len() > 1 && matches!(w.cmd, Cmd::Update | Cmd::Delete) && w.merge.is_none() && w.on_conflict.is_none() && targets(w)));
    let spool = |s: u8| crate::breakers::decl_of(g, s).is_some_and(|d| g.pipelines.get(d.producer as usize).is_some_and(|p| matches!(p.ops.last(), Some(Op::Join(JoinDesc::Spool)))));
    let source = matches!(inner.source, Source::Breaker { slot } if spool(slot));
    let plain = inner.union.is_none() && inner.ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_)));
    (write || select(g, ops, facts.node)) && params.is_nil() && source && plain && !g.reruns.iter().any(|r| r.node_id == facts.node)
}

/// D4569: the SELECT half (a channel's sales UNION ALL returns under a nest over a small dimension). A nest over a
/// UNION ALL distributes over its arms (each outer row meets the inner rows alone, whatever arm it came from), so each
/// arm may carry its own copy and stream, where the union kept its Materialize form and filled it on the leader. Only a
/// nest `map_nest_loop` found quiet (`PipelineGraph::union_nests`: no parameters, a calm inner, a quiet join qual)
/// under an open that writes and locks nothing: the copies' inner reads the one plan Material, filled once ahead of the
/// arms (lowerv4 nest.spool's eager form), so nothing the copies do shows which arm ran first.
fn select(g: &PipelineGraph<'_>, ops: &[Op<'_>], node: i32) -> bool {
    g.union_nests.contains(&node) && !ops.iter().any(|o| matches!(o, Op::Write(_) | Op::Lock(_)))
}

/// D4569: a streamed union's parent ops `ops` (ops/append.rs `push_union`) carry a SELECT's union nest into its arms: the
/// shapecheck fire signal `append.nest` (lowerv4 road append.nest, read by exec admission ahead of this decomposition).
pub(crate) fn fire_copies(g: &PipelineGraph<'_>, ops: &[Op<'_>]) {
    if ops.iter().any(|o| matches!(o, Op::Join(JoinDesc::Nest { facts, .. }) if select(g, ops, facts.node))) {
        fires::fire("append.nest");
    }
}

/// A copy of nest `op` for one arm: the same descriptor over a copy of its inner (`dup` copies the inner's ops). `None`
/// for anything `per_arm` does not take.
pub(crate) fn dup_nest<'p>(op: &Op<'p>, dup: fn(&Op<'p>) -> Option<Op<'p>>) -> Option<Op<'p>> {
    let Op::Join(JoinDesc::Nest { jointype, params, inner, qual, unique, inner_node, facts }) = op else { return None };
    let Source::Breaker { slot } = inner.source else { return None };
    if inner.union.is_some() {
        return None;
    }
    let arena = *inner.ops.allocator();
    let mut ops = ArenaVec::with_capacity_in(inner.ops.len(), arena); // alloc: Cold
    for o in inner.ops.iter() {
        ops.push(dup(o)?);
    }
    let copy = PipelineNode { node_id: inner.node_id, source: Source::Breaker { slot }, ops, desc: inner.desc, union: None, node_ops: inner.node_ops.clone(), mark: inner.mark };
    Some(Op::Join(JoinDesc::Nest { jointype: *jointype, params: *params, inner: ArenaBox::new_in(copy, arena), qual: *qual, unique: *unique, inner_node: *inner_node, facts: *facts }))
}
