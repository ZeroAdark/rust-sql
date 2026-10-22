//! D2855: a sorted group under a nest's inner with no NestLoopParam does not absorb its key Sort (PostgreSQL initializes
//! the Sort with REWIND and keeps its rows at a rescan), so the rerun set keeps the Sort and the join below it runs once.
//! regress-full stats-010 (pg_stat_user_tables x pg_statio_user_tables, grouped by an oid key) lowers; with the Sort
//! absorbed the join was re-run at every re-open and refused (KEPT_OUTER_RAISES). select_parallel-025 (a text key) lowers
//! with its Sort kept too.
#[allow(unused_imports, dead_code)]
use gravity_lowerv4_it as common;

#[allow(dead_code)]
#[path = "common/write_rig.rs"]
mod write_rig;

fn lowers(fixture: &str) -> Result<(), String> {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    let path = common::plans_dir().join("regress-full").join(format!("{fixture}.plan"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    write_rig::declared(&text, |a, graph, facts| {
        assert!(graph.reruns.iter().any(|r| graph.kept.iter().any(|k| r.pipelines.contains(k))), "{fixture}: no kept rerun set");
        gravity_expr::nodes::func::acl_defer_begin();
        let l = gravity_lowerv4::lower_graph(a.handle(), rig.mcx(), graph, facts, common::RigCatalog::get(), &mut gravity_lower::Sequential::new(a.handle()), gravity_lower::LowerOpts::default());
        gravity_expr::nodes::func::acl_defer_end();
        l.map(|_| ()).map_err(|e| e.to_string())
    })
    .unwrap_or_else(|| Err(format!("{fixture}: decompose declined")))
}

#[test]
fn a_kept_sort_under_a_nest_inner_lowers() {
    for f in ["stats-010", "select_parallel-025"] {
        if let Err(e) = lowers(f) {
            panic!("{f}: {e}");
        }
    }
}
