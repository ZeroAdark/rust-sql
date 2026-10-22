//! A graph's pipelines as an admission walks them: the graph pipeline, or a nest's or SubPlan's inner under one, that a
//! plan node drives.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::subplan::{SubForm, SubPlanDesc};
use gravity_decompose::{Op, PipelineNode};

/// The pipeline (a graph pipeline, or a nest's or SubPlan's inner under one) whose source is plan node `id`.
pub(crate) fn walk_of<'a, 'p>(p: &'a PipelineNode<'p>, id: i32) -> Option<&'a PipelineNode<'p>> {
    stack_depth_core::check_stack_depth_or_panic();
    if p.node_id == id {
        return Some(p);
    }
    p.ops.iter().find_map(|op| match op {
        Op::Join(JoinDesc::Nest { inner, .. }) | Op::SubPlan(SubPlanDesc { form: SubForm::Nested { inner }, .. }) => walk_of(inner, id),
        _ => None,
    })
}
