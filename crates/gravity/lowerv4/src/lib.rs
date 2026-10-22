//! Lowerer v4 (docs/gravity/lowerv4/plan.md): kinds and roads declared at M0, built by `registry/`'s tables; one catch here.
//! Admission (R-A, plan §3.1, `applies::admit`): only when every needed kind and applicable road is built; else `NotBuilt`.
#![allow(clippy::too_many_arguments)]
#![allow(clippy::module_inception)]
#![deny(unsafe_code)]

pub mod applies;
pub mod cell;
pub mod graph;
pub mod hir;
pub mod lx;
pub mod mechs_used;
/// D3585: the graph-wide answers kept for one lowering (memo.rs).
pub(crate) mod memo;
pub mod pipes;
pub mod rep;
pub mod road_off;

/// Graph properties computed once (plan §3): the struct and its walk (props/mod.rs).
pub mod props;

/// The hook interfaces (plan §3.1): the variant lists and one face per hook family (face/mod.rs).
pub mod face;

/// The frame (plan §2.1): one function per pipeline over the faces, its stop / resume, cancel and per-open words.
pub mod frame;

/// The session inputs the lowering reads (the inputs home: its GUC reads).
pub mod inputs;

use std::panic::resume_unwind;
use gravity_decompose::{Facts, PipelineGraph};
use gravity_expr::Catalog;
use gravity_lower::{Lowered, WordPlan};
use gravity_program::ArenaRef;
use mcx::Mcx;

pub use crate::admit::refusal::{Milestone, Reason, Refusal, Variant};
pub mod admit;
pub mod dict;
pub mod effects;
pub mod end;
pub mod join;
pub mod keyed;
pub mod lifetime;
pub mod nest;
pub mod ops;
pub mod rerun;
pub mod step;
pub mod table;

/// D3832: PostgreSQL's check_stack_depth at a level of one of the lowerer's recursions (a pipeline's step chain, a
/// rescanned inner's walk, the walks over nested pipelines): past max_stack_depth it raises 54001 as a `PgError` panic
/// payload, which `catch` passes on, never running into the thread's stack end (a 2400-deep LATERAL chain's step chain
/// killed a server backend with SIGSEGV).
#[inline]
pub(crate) fn stack_check() {
    stack_depth_core::check_stack_depth_or_panic();
}

/// D3833: one nest level of the rescan walk's recursion (`Inner::walk`), held across it. Its frames are heavier on
/// wasm32's engine stack than its shadow bytes say, so there it charges the guard `WASM_NEST_LEVEL_CHARGE` more.
#[inline]
pub(crate) fn nest_level() -> stack_depth_core::EngineLevel {
    stack_depth_core::EngineLevel::charging(stack_depth_core::WASM_NEST_LEVEL_CHARGE)
}

/// The one catch (v3 design §3.5): a `refuse!` payload is the refusal; a `PgError` (bare or boxed) or a thread exit is
/// rethrown unchanged, so the statement's error is Postgres's, never a refusal; any other panic is `Reason::Bug`.
pub fn catch<T>(node: i32, f: impl FnOnce() -> T) -> Result<T, Refusal> {
    let payload = match gravity_abi::status::unwind::catch(f) {
        Ok(v) => return Ok(v),
        Err(p) => match p.downcast::<Refusal>() {
            Ok(r) => return Err(*r),
            Err(p) => match p.downcast::<gravity_lower::admit::Refuse>() {
                Ok(r) => return Err(crate::admit::refusal::of_facade(*r)),
                Err(p) => p,
            },
        },
    };
    // D2962: every designed exit (types_error::is_exit_payload), not only PanicExitThread
    if payload.is::<types_error::PgError>() || payload.is::<Box<types_error::PgError>>() || types_error::is_exit_payload(&*payload) {
        resume_unwind(payload);
    }
    Err(crate::admit::refusal::bug(node, payload))
}

/// The lowerer hook (`gravity_lower`'s seam): the graph lowered, or its first refusal by name (field values logged once).
pub fn lower_graph<'p>(arena: ArenaRef, mcx: Mcx<'_>, graph: &PipelineGraph<'p>, facts: &dyn Facts, catalog: &dyn Catalog<'p>, plan: &mut dyn WordPlan<'p>, opts: gravity_lower::LowerOpts) -> gravity_lower::Result<Lowered> {
    lower_in(arena, mcx, graph, facts, catalog, plan, opts, frame::diag::on().then_some(&mut frame::diag::Report::default()))
}

/// `lower_graph`; `Some(report)`: in the all-blockers mode, `report` its blockers (frame/diag.rs `lower_graph_diag`).
/// D2597: inside `facts::order::lowering`, so a statement that acts keeps its raising folds in plan order and a licensed
/// one's float folds raise wherever an input order could overflow.
pub(crate) fn lower_in<'p>(arena: ArenaRef, mcx: Mcx<'_>, graph: &PipelineGraph<'p>, facts: &dyn Facts, catalog: &dyn Catalog<'p>, plan: &mut dyn WordPlan<'p>, opts: gravity_lower::LowerOpts, diag: Option<&mut frame::diag::Report>) -> gravity_lower::Result<Lowered> {
    gravity_decompose::facts::order::lowering(graph, || memo::scoped(graph, || lower_scoped(arena, mcx, graph, facts, catalog, plan, opts, diag)))
}

/// D1752 / D3437: the graph's pipelines in program order (frame/mechs.rs `arm_order` over drain/arms.rs `arm_runs`),
/// before the passes a maker adds; read where `lower_in` reads it. The dump's `order` line and the arm-order tests.
pub fn program_order<'p>(arena: ArenaRef, mcx: Mcx<'_>, graph: &PipelineGraph<'p>, facts: &dyn Facts, catalog: &dyn Catalog<'p>) -> Vec<usize> {
    gravity_decompose::facts::order::lowering(graph, || {
        memo::scoped(graph, || {
            let over = crate::ops::group::regroup::regroup(arena, mcx, graph, facts, catalog);
            let gx = graph::Gx { graph, facts, catalog, over: over.as_ref(), mcx };
            let runs = gx.arm_runs();
            frame::mechs::arm_order(graph.pipelines.len(), &runs, mcx)
        })
    })
}

fn lower_scoped<'p>(arena: ArenaRef, mcx: Mcx<'_>, graph: &PipelineGraph<'p>, facts: &dyn Facts, catalog: &dyn Catalog<'p>, plan: &mut dyn WordPlan<'p>, opts: gravity_lower::LowerOpts, diag: Option<&mut frame::diag::Report>) -> gravity_lower::Result<Lowered> {
    let over = crate::ops::group::regroup::regroup(arena, mcx, graph, facts, catalog); // D1210: the pair groups the pair set cannot hold, re-declared
    let gx = graph::Gx { graph, facts, catalog, over: over.as_ref(), mcx };
    crate::rerun::params::supply(&gx, plan); // M8 L0-w: every executor parameter's word, in both modes
    road_off::apply();
    // V4R-110: a program that read a list the content key keys by its class is its list's: run once, never filed
    let (r, lists) = gravity_lower::inputs::key::lowering_reads_lists(graph, || match diag {
        Some(report) => frame::diag::lower(arena, &gx, plan, opts, report), // every blocker logged (gate.md §16), then this refusal
        None => catch(gx.root_node(), || {
            let p = props::Props::of(&gx);
            applies::admit(&gx, &p);
            frame::program::lower(arena, &gx, &p, plan, opts, None)
        }),
    });
    r.map(|mut l| {
        crate::effects::end_check::check(&gx, arena); // C1: the three expression readers agree (off unless asked)
        // v5 errorhold: the plan-order program (every licence denied, `effects::deny_all`) is its statement's, never filed
        let hold = gravity_decompose::effects::denying();
        if !l.descs.no_cache && (lists || hold) {
            l.descs.no_cache_why = if hold { "errorhold" } else { "list" };
        }
        l.descs.no_cache |= lists || hold;
        l
    })
    .map_err(|r| {
        if !matches!(r.reason, Reason::NotBuilt { .. } | Reason::Interrupted) {
            eprintln!("gravity lowerv4: refused node={} {}", r.node, r.reason);
        }
        r.to_seam()
    })
}
