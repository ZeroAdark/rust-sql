//! A pull-form merge's Sort arm (a full sort's run: merge_admit.rs `merge_pull`'s `arm`; Merge Append over an index
//! and a Sort under a LIMIT) is read by the merge reader through sort's run cursor (frame/iface/sort.rs
//! `sort_cursor`) at the reader's consumed index `len(graph consumes) + i`, `i` the run arm's rank in arm order
//! (merge_pull.rs `make`). So the reader, not the arm, consumes the run's slot: its life runs through the reader's end
//! (lifetime.rs), and the arm (`Fused`) reads nothing. Owner M3-MERGE.
use gravity_program::{ArenaRef, LifeId, Pipeline};
#[cfg(debug_assertions)]
use mcx::{CollectIn, GrowIn, PgVec};

use crate::admit::refusal::Reason;
use crate::graph::Gx;
use crate::props::Merge;

/// Each pull-form merge's run arms re-home their slot from the arm's program consumes to the reader's, appended in arm
/// order. `at` maps a graph pipeline to its program pipeline. Run before frame.rs `renumber_deferred` (which renames a slot
/// in every reader of its life, so the reader's index stays put). A reader whose consumes an earlier rewrite moved (a LAZY
/// set's slot taken out) no longer holds a run at the index its cursor was lowered at: refused, never guessed.
pub(crate) fn rehome(gx: &Gx<'_, '_>, arena: ArenaRef, at: &dyn Fn(usize) -> Option<usize>, pipelines: &mut [Pipeline], node: i32) {
    let ps = gx.pipelines();
    #[cfg(debug_assertions)]
    let before: PgVec<'_, (PgVec<'_, u8>, PgVec<'_, LifeId>)> = pipelines.iter().map(|p| (mcx::to_vec_in(p.consumes, gx.mcx), mcx::to_vec_in(p.consumes_from, gx.mcx))).collect_in(gx.mcx);
    #[cfg(debug_assertions)]
    let mut touched: PgVec<'_, usize> = PgVec::new_in(gx.mcx);
    for r in 0..ps.len() {
        let Some(m) = Merge::of_reader(gx, r).filter(|m| crate::props::merge_pull(gx, m)) else { continue };
        let mut i = ps[r].desc.consumes.len();
        for &a in &m.arms {
            let gravity_decompose::Source::Breaker { slot } = ps[a].source else { continue };
            let (Some(ri), Some(ai)) = (at(r), at(a)) else { crate::refuse!(node, Reason::Bug { payload: "a pull-form run arm or its reader with no program pipeline" }) };
            let d = pipelines[ai];
            let Some(j) = d.consumes.iter().position(|&s| s == slot) else { crate::refuse!(node, Reason::Bug { payload: "a pull-form run arm that does not consume its run" }) };
            // the run's life: the arm's named one, or its slot's last producer before the arm (the admission keeps the slot
            // unnamed from the arm to the reader)
            let life = d.consumes_from.get(j).copied().or_else(|| (0..ai).rev().find(|&q| pipelines[q].produces() == Some(slot)).map(|q| LifeId(q as u16)));
            let keep = d.consumes.iter().enumerate().filter(|&(x, _)| x != j);
            pipelines[ai] = keep.fold(Pipeline { consumes: &[], consumes_from: &[], ..d }, |n, (x, &s)| match d.consumes_from.get(x) {
                Some(&l) => n.consuming_from_in(arena, s, l),
                None => n.consuming_in(arena, s),
            });
            let rd = pipelines[ri];
            if rd.consumes.len() != i || rd.consumes.contains(&slot) {
                crate::refuse!(node, Reason::Shape { why: crate::admit::refusal::ShapeWhy(MOVED) });
            }
            // a pipeline names the life of every slot it consumes or of none
            pipelines[ri] = match life {
                Some(l) if rd.consumes_from.len() == rd.consumes.len() => rd.consuming_from_in(arena, slot, l),
                _ if rd.consumes_from.is_empty() => rd.consuming_in(arena, slot),
                _ => crate::refuse!(node, Reason::Bug { payload: "a pull-form run with no producer before its arm" }),
            };
            #[cfg(debug_assertions)]
            touched.extend_in([ri, ai]);
            i += 1;
        }
    }
    // it moves only the run slots of pull-form merges' Sort arms to their readers: no other pipeline's consumes change
    #[cfg(debug_assertions)]
    for (x, (c, f)) in before.iter().enumerate().filter(|(x, _)| !touched.contains(x)) {
        debug_assert!(pipelines[x].consumes == &c[..] && pipelines[x].consumes_from == &f[..], "rehome changed pipeline {x}'s consumes");
    }
}

/// The refusal of a pull-form merge whose reader's consumes moved before its runs were re-homed.
pub(crate) const MOVED: &str = "a pull-form merge with a Sort arm whose reader's consumed slots an earlier rewrite moved (a LAZY hashed SubPlan's set)";
