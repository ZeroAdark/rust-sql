//! Kind `runs.unique` (cards/M3.md L6, I42): Unique / DISTINCT over sorted input. Form K, the v2 key set's port (lowerv2
//! tail/keyset.rs): decompose holds a Unique node's keys in a keyed breaker (the distinct kind) with the key-ordered
//! generic owner, a record of the keys alone. The end is the hash aggregate's over that zero-slot record (keyed.hashagg:
//! one record per distinct key, NULLs equal as the sort's equality treats them), and the emit is the keyed group emit
//! with the records sorted by the keys in the input's direction (runs_topk_groups `key_words`, D443), then the node's
//! filter and projection. A Unique emitting a column that is not one of its keys (DISTINCT ON) keeps each run's first
//! row: decompose maps it to `Op::FirstOfRun` (V4R-147), runs.unique's step face, served by road run.pergroup
//! (runs/per_group.rs). A HashAggregate DISTINCT is keyed.hashagg's and never reaches here (G4).
//! Budget 400 lines. Owner M3-GROUPED.
use mcx::PgBox;

use crate::face::end::EndMech;
use crate::face::kinds::KindId;
use crate::face::strategy::RUN_PERGROUP;
use crate::face::source::SourceMech;
use crate::face::step::StepMech;
use crate::frame::frame::{EndMaker, SourceMaker, StepMaker};
use crate::frame::iface::kind_impl;
use crate::frame::px::Px;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;
use crate::frame::iface::need_strategy;
use gravity_decompose::ops::shared::agg::GroupDecl;
use gravity_decompose::{Op, PipelineNode};
use types_nodes::primnodes::OUTER_VAR;

pub(crate) const ROWS: Table = Table {
    kinds: &[(KindId::RunsUnique, &EndMaker(make_end)), (KindId::RunsUnique, &SourceMaker(make_emit)), (KindId::RunsUnique, &StepMaker(make_step))],
    ..crate::ops::NONE
};

fn make_end<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    (kind_impl::<EndMaker>(&node, KindId::KeyedHashagg).0)(px, k)
}

fn make_emit<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    if px.gx.drained_group(k).is_some_and(|g| reads_non_key(&px.gx.pipelines()[k], g)) {
        // D527: the record holds the keys alone; a DISTINCT ON reading another column is l2's first-of-run (V4R-147)
        crate::refuse!(node, Reason::Shape { why: ShapeWhy("runs.unique D527: a DISTINCT ON reads a non-key column") });
    }
    (kind_impl::<SourceMaker>(&node, KindId::KeyedEmitGroup).0)(px, k)
}

/// D527: whether pipeline `p` reads an outer column that is not one of `g`'s keys, in its filters up to and in its first
/// projection (the ops that read the emitted record).
fn reads_non_key(p: &PipelineNode<'_>, g: &GroupDecl<'_>) -> bool {
    let mut hit = false;
    let mut see = |n| {
        gravity_decompose::expr::walk(n, &mut |x| {
            let v = x.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0);
            hit |= v.is_some_and(|v| !g.keys.iter().any(|kd| kd.resno == v.varattno));
            true
        });
    };
    for op in p.ops.iter() {
        match op {
            Op::Filter(n) | Op::Gate(n) => see(*n),
            Op::Project(l) => {
                l.iter().for_each(&mut see);
                break;
            }
            _ => {}
        }
    }
    hit
}

/// DISTINCT ON's first-of-run step (V4R-147): road run.pergroup's.
fn make_step<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    (need_strategy::<StepMaker>(&node, RUN_PERGROUP).0)(px, k, j)
}
