//! M5 1a-4 (M5-plan §9 Wave 1a; main 09-25): EXECUTE on a set-returning function is checked before the program runs, at
//! every execution, as PG's `init_sexpr` does at executor start (execSRF.c:704-707: under `LIMIT 0` too). The check is
//! admission's (admit.rs `program_funcs` over the content key's node traversal -> `GraphTpl.funcs` -> runner/keys.rs
//! `eval_keys` -> `recheck_execute_acls`), not FmgrSet's open. It holds while the key traversal visits every `SrfFn`'s call
//! (key.rs `Op::Srf`): this pins that for a ProjectSet's entries, a FunctionScan's function and each ROWS FROM function,
//! so a change to how `Op::Srf` is keyed (L0-f) cannot silently drop a set-returning function from the ACL list.
use std::path::Path;

use gravity_decompose::{corpus, decompose_in, Op};
use gravity_program::Arena;
use mcx::MemoryContext;
use types_nodes::NodeTag;

/// Every `SrfFn` funcid of `rel`'s graph, and the FuncExpr funcids the key traversal hands admission (`program_funcs`'
/// FuncExpr arm reads exactly these).
fn funcs(rel: &str) -> (Vec<u32>, Vec<u32>) {
    let path = manifest_dir().join("../decompose/tests/plans").join(rel);
    let ctx = MemoryContext::new("m5 srf acl");
    let fx = corpus::load(ctx.mcx(), &std::fs::read_to_string(&path).unwrap()).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let arena = Arena::new(1 << 20);
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|r| panic!("{rel} refused: {r:?}"));
    let srfs: Vec<u32> = g.pipelines.iter().flat_map(|p| p.ops.iter()).filter_map(|op| if let Op::Srf(d) = op { Some(d) } else { None }).flat_map(|d| d.functions.iter().map(|f| f.funcid)).collect();
    let mut seen = Vec::new();
    gravity_lower::inputs::key::key_and_nodes(&g, &mut |n| {
        if n.node_tag() == NodeTag::T_FuncExpr {
            seen.push(n.as_func_expr().unwrap().funcid);
        }
    });
    (srfs, seen)
}

#[test]
fn every_set_returning_function_reaches_the_execute_recheck() {
    // srf-003: two ProjectSet entries; srf-018: ROWS FROM (generate_series, unnest); srf-023: a user function in FROM
    for (rel, n) in [("srf-003.plan", 2), ("srf-018.plan", 2), ("srf-023.plan", 1), ("bench-089.plan", 1)] {
        let (srfs, seen) = funcs(rel);
        assert_eq!(srfs.len(), n, "{rel}: its set-returning functions {srfs:?}");
        for f in &srfs {
            assert!(seen.contains(f), "{rel}: function {f} is not in the key traversal admission checks EXECUTE over ({seen:?})");
        }
    }
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
