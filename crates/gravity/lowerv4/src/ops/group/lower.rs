//! Kind `runs.group` (cards/M3.md L6, I42): Group and the sorted aggregate (GroupAggregate over sorted input). Form K,
//! the v2 key set's port (lowerv2 tail/keyset.rs): decompose holds a sorted group's records in a keyed breaker with the
//! key-ordered generic owner (`ORDER_KEY`), so the end is the hash aggregate's (keyed.hashagg: every row folds into its
//! group's record, every aggregate argument and FILTER evaluated as PostgreSQL does, D449) and the emit is the keyed group
//! emit (keyed.emit_group) with the records sorted by the group's own keys before they stream (runs_topk_groups
//! `key_words`: each key in the input's direction, its NULL placed by NULLS FIRST / LAST, D443). The finals, the HAVING
//! filter and the projection then run for every group, as the sorted aggregate's do. The same order a group closing on a
//! key change emits; no row is copied into a run. A hashed group is keyed.hashagg's and never reaches here (G4).
//! A sorted group whose aggregate argument or FILTER calls something VOLATILE streams instead, one group at a time
//! (D1071: decompose's `Op::GroupRun`, keyed.hashagg's step face, keyed/stream.rs). Budget 400 lines. Owner M3-GROUPED.
use gravity_decompose::ops::shared::agg::GroupForm;
use mcx::PgBox;

use crate::face::end::EndMech;
use crate::face::kinds::KindId;
use crate::face::source::SourceMech;
use crate::face::step::StepMech;
use crate::frame::frame::{EndMaker, SourceMaker, StepMaker};
use crate::frame::iface::kind_impl;
use crate::frame::px::Px;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;

pub(crate) const ROWS: Table = Table {
    kinds: &[(KindId::RunsGroup, &EndMaker(make_end)), (KindId::RunsGroup, &SourceMaker(make_emit)), (KindId::RunsGroup, &StepMaker(make_step))],
    ..crate::ops::NONE
};

/// Whether group form `f` is form K's: the sorted aggregate, or a Group node (a key set).
pub(crate) fn keyed_form(f: GroupForm) -> bool {
    matches!(f, GroupForm::Sorted | GroupForm::Group)
}

fn make_end<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    let Some(g) = px.gx.group_at(k) else { crate::refuse!(node, Reason::Bug { payload: "a sorted group end without its group" }) };
    if g.empty_no_row && !crate::graph::grouped(g) {
        // D345 / D350: a GroupAggregate over no key column (every key the planner dropped as redundant) whose pinned
        // columns decompose could not key (V4R-144: agg.rs pinned_word, one image per row); keyed ones serve below
        crate::refuse!(node, Reason::Shape { why: ShapeWhy("runs.group D345: every key the planner dropped as redundant, a pinned column read") });
    }
    if g.aggs.iter().any(crate::ops::shared::agg::sort_cols) && !crate::ops::group::regroup::riders_free(px.arena, px.gx.graph, g) {
        // D458: PG feeds a rider its GroupAggregate's input order (under aggpresorted, the ORDER BY's); the host sorts only
        // the ordered aggregate's own run
        crate::refuse!(node, Reason::Shape { why: ShapeWhy("runs.group D458: a rider whose value reads the row order beside an ordered aggregate (PG's presorted input)") });
    }
    if !keyed_form(g.form) {
        // D346: an ORDER BY inside an aggregate call (fold/plain.rs hands it here unless it is order-free, V4R-178); a
        // grouped one is keyed group_rec's to judge (the same D458 name); D458 (gravity_expr's aggorder flag) names it
        crate::refuse!(node, Reason::Shape { why: ShapeWhy("runs.group D458: an ordered aggregate (gravity_expr's aggorder flag)") });
    }
    (kind_impl::<EndMaker>(&node, KindId::KeyedHashagg).0)(px, k)
}

fn make_emit<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    (kind_impl::<SourceMaker>(&node, KindId::KeyedEmitGroup).0)(px, k)
}

/// D1071: a streamed sorted group (`Op::GroupRun`, a VOLATILE argument or FILTER) is keyed.hashagg's step face over the
/// keyed record (keyed/stream.rs).
fn make_step<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    (kind_impl::<StepMaker>(&node, KindId::KeyedHashagg).0)(px, k, j)
}
