//! The composed statement the entry runs: InitPlans, the main program, an Append's arms (today the lowerer's one program).
use gravity_decompose::PipelineGraph;
use gravity_expr::Catalog;
use gravity_program::ArenaRef;

use crate::{lower_graph, LowerOpts, Lowered, Result, WordPlan};

pub struct InitDesc {
    pub plan_id: i32,
    pub param: i32,
    pub typ: types_core::Oid,
    pub link: gravity_decompose::ops::shared::subplan::SubLink,
    pub lowered: Lowered,
}

pub struct Composed {
    pub inits: Vec<InitDesc>,
    pub main: Lowered,
    pub arms: Vec<Lowered>,
    pub merge: Vec<crate::mergearms::MergeKey>,
}

pub fn lower_composed<'p>(arena: ArenaRef, mcx: mcx::Mcx<'_>, graph: &PipelineGraph<'p>, facts: &dyn gravity_decompose::Facts, catalog: &dyn Catalog<'p>, plan: &mut dyn WordPlan<'p>, opts: LowerOpts) -> Result<Composed> {
    Ok(Composed { inits: Vec::new(), main: lower_graph(arena, mcx, graph, facts, catalog, plan, opts)?, arms: Vec::new(), merge: Vec::new() }) // alloc: Program
}
