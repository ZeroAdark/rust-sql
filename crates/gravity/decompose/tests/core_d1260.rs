//! D1260 (subplan.rs `read_as`): PostgreSQL evaluates an outer-level GROUPING() at the Agg that groups its columns and
//! passes it to the sub-query as a SubPlan parameter (the planner's replace_outer_grouping), so the GroupingFunc sits in
//! the SubPlan's `args` inside the Agg's target list. agg.rs `arm_lists` folds it to its constant in the Agg's lists, but
//! the SubPlan op took its arguments from the plan's own node, so the parameter still read the GroupingFunc and the
//! expression compiler refused `[GroupingFunc]` (M8-SUB B2b's census: regress groupingsets-029 / -032 / -038 / -091). The
//! op now binds the arguments its reader holds: the folded constant.
use std::fs;

use gravity_decompose::{corpus, decompose_in, dump};
use gravity_program::Arena;
use mcx::MemoryContext;

fn graph(name: &str) -> String {
    let path = manifest_dir().join("tests/plans/regress-full").join(format!("{name}.plan"));
    let text = fs::read_to_string(&path).unwrap();
    let ctx = MemoryContext::new("d1260");
    let fx = corpus::load(ctx.mcx(), &text).unwrap();
    let arena = Arena::new(1 << 20);
    let mut out = String::new();
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).expect("decomposes");
    dump::dump(&g, &mut out).unwrap();
    out
}

#[test]
fn an_outer_grouping_passed_to_a_subplan_binds_its_folded_constant() {
    for name in ["groupingsets-029", "groupingsets-032", "groupingsets-038", "groupingsets-039", "groupingsets-091"] {
        let g = graph(name);
        assert!(!g.contains("T_GroupingFunc"), "{name}: a parameter still reads the GroupingFunc\n{g}");
        assert!(g.contains("params=($exec0=c23:0)"), "{name}: GROUPING() of fully grouped columns is 0\n{g}");
    }
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
