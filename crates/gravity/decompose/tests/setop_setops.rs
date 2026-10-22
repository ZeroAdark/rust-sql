use std::fs;
use std::path::Path;

use gravity_decompose::graph::Op;
use gravity_decompose::ops::shared::setop::SetDesc;
use gravity_decompose::{corpus, decompose_plan, dump, Kind};
use gravity_program::Arena;
use mcx::MemoryContext;

/// A recursive CTE is a subplan (`PlannedStmt.subplans[ctePlanId − 1]`, the main tree `Agg → CteScan`), so
/// until D4 maps CteScan no fixture reaches the RecursiveUnion from its root: this lifts the subplan and
/// decomposes it as a plan tree of its own.
#[test]
fn a_recursive_union_lifted_from_its_cte_decomposes_to_the_iteration_desc() {
    let dir = manifest_dir().join("tests/plans");
    let mut found = 0;
    for e in fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if !p.extension().is_some_and(|x| x == "plan") {
            continue;
        }
        let text = fs::read_to_string(&p).unwrap();
        if !text.contains("RECURSIVEUNION") {
            continue;
        }
        let ctx = MemoryContext::new("fixture");
        let fx = corpus::load(ctx.mcx(), &text).unwrap();
        let sub = fx.stmt.subplans.iter().flatten().find(|n| n.as_recursive_union().is_some()).expect("the recursive union is a subplan");
        let arena = Arena::new(1 << 20);
        let g = decompose_plan(arena.handle(), fx.stmt, sub, &fx.facts, ctx.mcx()).unwrap_or_else(|r| panic!("{}: {r}", p.display()));
        let mut out = String::new();
        dump::dump(&g, &mut out).unwrap();
        println!("{}\n{out}", p.display());
        assert!(g.is_mapped(Kind::RecursiveUnion) && g.is_mapped(Kind::WorkTableScan), "{}", p.display());
        assert!(out.contains("iterate body=p") && out.contains("source breaker slot="), "{out}");
        let stem = p.file_stem().unwrap().to_string_lossy();
        let it = g.pipelines.iter().flat_map(|q| q.ops.iter()).find_map(|o| match o {
            Op::SetOp(SetDesc::Iter(it)) => Some(it),
            _ => None,
        });
        let it = it.expect("the driver's Iter op");
        // D241: cte-028's term builds a hash over the work table (re-run per generation) and probes `tree` with it;
        // setops-002's term builds its hash over g2 (no wtParam: built once, as PG keeps a one-batch table)
        let want: Option<Vec<u16>> = match &*stem {
            "cte-028" => Some(vec![it.body - 1, it.body]),
            "setops-002" => Some(vec![it.body]),
            _ => None,
        };
        if let Some(w) = want {
            assert_eq!(it.reruns.to_vec(), w, "{stem}\n{out}");
            let build = g.pipelines[it.body as usize - 1].desc.produces();
            assert!(build.is_some_and(|s| g.pipelines[it.body as usize].desc.consumes.contains(&s)), "{stem}: the body probes the term's build\n{out}");
        }
        found += 1;
    }
    assert!(found > 0, "no fixture with a RecursiveUnion under tests/plans");
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
