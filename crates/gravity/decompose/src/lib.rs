//! Gravity decomposer: a `PlannedStmt` becomes a pipeline graph in the IR probe's desc vocabulary (docs/gravity/decompose.md).
#![allow(clippy::missing_safety_doc, clippy::not_unsafe_ptr_arg_deref, clippy::too_many_arguments)]
#![deny(unsafe_code)]

pub(crate) mod arm_nests;
pub mod bake;
pub mod breakers;
pub mod corpus;
pub mod dump;
pub mod explain;
pub mod expr;
pub mod graph;
pub mod join;
pub mod ledger;
pub(crate) mod nested_arms;
pub mod ops;
pub mod outs;
pub mod params;
pub mod pg_demand;
mod raw;
pub(crate) mod self_spool;
pub mod walk;

use gravity_program::ArenaRef;
use types_nodes::plannodes::PlannedStmt;

pub use crate::catalog::{Am, AttFact, ExprFp, Facts, IndexAm, IndexFact, InsteadOf, NoFacts, RelFact, TypeRow};
pub use graph::{Bound, BreakerDecl, IndexKey, InitPlan, Op, PipelineGraph, PipelineNode, Source};
pub use crate::facts::hints::Hint;
pub use crate::ops::shared::modifytable::{Member, MemberSet};
pub use ledger::{Kind, Milestone, State, CURRENT, LEDGER};
pub use crate::admit::Refusal;
/// D4497: the default-off roads decompose reads through `Facts::road_on` (each a lowerv4 strategy on road_off.rs
/// `DEFAULT_OFF`): D4676's hashed sorted chain (ops/group/absorb.rs), D4567 (a)'s numeric sum of a division by a
/// constant (ops/agg numeric.rs `div_form`) and D4723's min / max of a CTE's one-scale numeric column (ops/agg mod.rs
/// `cte_one_dscale`).
pub const ROADS: &[&str] = &[
    ops::group::absorb::HASH_CHAIN_ROAD,
    ops::shared::agg::NUM_DIV_ROAD,
    ops::shared::agg::NUM_CTE_ROAD,
];
/// D4565 / D4567 / D4497 / D3227 K3 / D3227 (b): the on-by-default roads decompose reads through
/// `Facts::default_on_road_on` (off only when the option names one plainly): agg's register var / stddev and decided
/// numeric scale, self_spool.rs's four nest keeps (on since the D4497 / D3227 flip) and its keyed-nest widening
/// `nest.key.eq`, and D4620's grouping-set cascade (ops/agg/gsets_tag.rs `cascade`; each on since
/// its flip). Each a lowerv4 strategy NOT on road_off.rs `DEFAULT_OFF` and none in `ROADS` (lowerv4 road_off.rs checks
/// both).
pub const ROADS_DEFAULT_ON: &[&str] = &[
    ops::shared::agg::STAT_REG_ROAD,
    ops::shared::agg::NUM_SCALE_ROAD,
    ops::shared::agg::GSETS_CASCADE_ROAD,
    self_spool::ROAD_KEEP_INITPLAN,
    self_spool::ROAD_KEEP_SUBQUERY,
    self_spool::ROAD_KEEP_SEMI,
    self_spool::ROAD_KEEP_MATERIAL,
    self_spool::ROAD_KEY_EQ,
];
pub mod admit;
pub mod catalog;
pub mod effects;
pub mod facts;
pub mod lifetime;
pub mod normalize;
pub mod removed;
pub mod rerun;

/// `scratch`: the caller's context for the walk's scratch (an admission's planning context, a test's own).
pub fn decompose<'p>(arena: ArenaRef, stmt: &'p PlannedStmt<'p>, facts: &dyn Facts, scratch: mcx::Mcx<'_>) -> Result<PipelineGraph<'p>, Refusal> {
    walk::Builder::new(arena, stmt, facts, scratch).run().map(|g| acting(g, arena, stmt, facts))
}

/// v5 errorhold: the graph with its statement's acting set (`PipelineGraph::acts`).
fn acting<'p>(mut g: PipelineGraph<'p>, arena: ArenaRef, stmt: &'p PlannedStmt<'p>, facts: &dyn Facts) -> PipelineGraph<'p> {
    // D4554: the walk mapping already took for the same statement (`effects::of_statement_acts`), else this one
    g.acts = g.acts_read.unwrap_or_else(|| effects::of_statement(&effects::Cx::new(stmt, facts, arena)).acts);
    g.stmt = Some(stmt);
    facts::order::license_raising(&mut g);
    g
}

/// `decompose` with a node allocator: a MERGE's carried target columns are synthesized target entries
/// (carry.rs); without one a MERGE refuses by name.
pub fn decompose_in<'p>(arena: ArenaRef, stmt: &'p PlannedStmt<'p>, facts: &dyn Facts, alloc: mcx::Mcx<'p>) -> Result<PipelineGraph<'p>, Refusal> {
    let scratch = alloc.context().new_child("GravityDecomposeScratch");
    decompose_planned(arena, stmt, facts, alloc, scratch.mcx())
}

/// `decompose_in` with the admission's planning context for its scratch.
pub fn decompose_planned<'p, 'f>(arena: ArenaRef, stmt: &'p PlannedStmt<'p>, facts: &'f dyn Facts, alloc: mcx::Mcx<'p>, scratch: mcx::Mcx<'f>) -> Result<PipelineGraph<'p>, Refusal> {
    let mut b = walk::Builder::new_in(arena, stmt, facts, scratch);
    b.alloc = Some(alloc);
    b.run().map(|g| acting(g, arena, stmt, facts))
}

pub fn decompose_plan<'p>(arena: ArenaRef, stmt: &'p PlannedStmt<'p>, root: types_nodes::Node<'p>, facts: &dyn Facts, scratch: mcx::Mcx<'_>) -> Result<PipelineGraph<'p>, Refusal> {
    walk::Builder::new(arena, stmt, facts, scratch).run_from(root).map(|g| acting(g, arena, stmt, facts))
}

/// D4554: the statement's acting answer is walked once: mapping's first asker (aggs-008's HashAggregate at the top: agg
/// `held_whole`) keeps it on the graph, and `acting` takes it, the same answer `effects::of_statement` gives.
#[cfg(test)]
mod d4554_one_acting_walk {
    use super::*;

    #[test]
    fn mapping_keeps_the_acting_answer_and_acting_takes_it() {
        let ctx = mcx::MemoryContext::new("d4554");
        let fx = corpus::load(ctx.mcx(), include_str!("../tests/plans/aggs-008.plan")).unwrap();
        let arena = gravity_program::Arena::new(1 << 22);
        let scratch = mcx::MemoryContext::new("d4554 scratch");
        let mut b = walk::Builder::new_in(arena.handle(), fx.stmt, &fx.facts, scratch.mcx());
        b.alloc = Some(fx.mcx);
        let g = b.run().expect("aggs-008 maps");
        let walked = effects::of_statement(&effects::Cx::new(fx.stmt, &fx.facts, arena.handle())).acts;
        assert_eq!(g.acts_read, Some(walked), "the top Agg asked it while mapping");
        let g = acting(g, arena.handle(), fx.stmt, &fx.facts);
        assert_eq!(g.acts, walked);
    }
}
