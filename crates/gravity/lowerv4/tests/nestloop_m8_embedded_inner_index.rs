//! A pipeline index k may name an embedded inner (a nest's or a nested SubPlan's, `Pipes`: `k - len()` past the graph's
//! pipelines). keyed/emit_group.rs `fed_group` and graph.rs `Gx::drains_grouping` read pipeline k through `gx.pipelines()`
//! (NEST B1; before it both indexed graph.pipelines[k] and panicked out of bounds: 222 census fixtures under B2b at box 72
//! t86). A plain test (main: the fix landed with B1, no Defect row).
use gravity_lowerv4_it as common;

use gravity_decompose::Op;

/// Census plan fixtures whose graphs embed an inner: every pipeline index, embedded ones included, answers
/// `drains_grouping` (the graph.rs half); their lowering never refuses with an out-of-bounds panic (the emit_group.rs half,
/// reached by a projection over an embedded inner).
#[test]
fn an_embedded_inner_index_reads_through_the_pipes_view() {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    common::RigCatalog::install_seams();
    let dirs = std::iter::once(common::plans_dir()).chain(std::iter::once(common::plans_dir().join("regress-full"))).chain((1..=8).map(common::milestone_dir));
    let (mut graphs, mut embedded, mut projecting, mut bad) = (0usize, 0usize, 0usize, Vec::new());
    for dir in dirs {
        for (name, text) in common::files(&dir, "plan") {
            let (ctx, arena) = (mcx::MemoryContext::new("m8 embedded inner fixture"), gravity_program::Arena::new(1 << 20));
            let Ok(fx) = gravity_decompose::corpus::load(ctx.mcx(), &text) else { continue };
            let Ok(graph) = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { continue };
            let gx = gravity_lowerv4::graph::Gx { graph: &graph, facts: &fx.facts, catalog: common::RigCatalog::get(), over: None, mcx: rig.mcx() };
            // `len()` is the graph's pipelines; the embedded inners follow them (pipes.rs `end`)
            let all = gx.pipelines().end();
            if all == graph.pipelines.len() {
                continue;
            }
            graphs += 1;
            let mut projects = false;
            for k in 0..all {
                let _ = gx.drains_grouping(k);
                if k >= graph.pipelines.len() {
                    embedded += 1;
                    projects |= gx.pipelines()[k].ops.iter().any(|o| matches!(o, Op::Project(_)));
                }
            }
            if !projects {
                continue;
            }
            projecting += 1;
            if let Some(Err(e)) = common::lower_fixture(&text) {
                let w = e.to_string();
                if w.contains("panic") || w.contains("out of bounds") {
                    bad.push(format!("{name}: {w}"));
                }
            }
        }
    }
    eprintln!("m8 embedded inners: {graphs} graphs with {embedded} embedded inners ({projecting} with a projection) read through Pipes");
    assert!(embedded > 0, "no census fixture embeds an inner");
    assert!(bad.is_empty(), "{} fixtures panic lowering an embedded inner:\n{}", bad.len(), bad.join("\n"));
}
