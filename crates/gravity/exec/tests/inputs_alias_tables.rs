//! D4449 (Stage B, condition 1): the alias passes of both keys skip the literal-table runs (lower key.rs `Runs`: the
//! VALUES rows of Consts alone), so over the whole decompose corpus what each walk's alias puts with its runs must be what
//! it puts over every position, the front walk's and the content key's alike: the key bytes are unchanged.
use std::path::PathBuf;

use gravity_decompose::{corpus, decompose_in};
use gravity_exec::inputs::front;
use gravity_lower::inputs::key::{alias_walk, key_nodes_runs, AliasPut};
use gravity_program::Arena;
use mcx::MemoryContext;
use types_nodes::Node;

fn seams() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| syscache_seams::lookup_pg_type_typcache_shape::set(|_| Ok(None)));
}

fn fixtures() -> Vec<PathBuf> {
    // read at run time (D793-b): the checkout the test runs in, not the one it was built in
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR")).join("../decompose/tests/plans");
    let mut out = Vec::new();
    for d in [root.clone(), root.join("regress-full")] {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            if e.path().extension().is_some_and(|x| x == "plan") {
                out.push(e.path());
            }
        }
    }
    out.sort();
    out
}

fn puts(at: &[Node<'_>], runs: &[(u32, u32)]) -> Vec<(u8, u64)> {
    // the passes' scratch (a mapping: only the pages a walk touches are faulted in)
    let arena = Arena::new(1 << 28);
    let mut v = Vec::new();
    alias_walk(arena.handle(), at, runs, &mut |p| match p {
        AliasPut::Pair(i, j) => v.push((0, (u64::from(i) << 32) | u64::from(j))),
        AliasPut::Count(n) => v.push((1, n as u64)),
        AliasPut::Share(x) => v.push((2, u64::from(x))),
    });
    v
}

#[test]
fn both_walks_alias_the_same_with_their_literal_table_runs() {
    seams();
    let (mut front_runs, mut key_runs, mut cells) = (0, 0, 0u64);
    for path in fixtures() {
        let text = std::fs::read_to_string(&path).unwrap();
        let ctx = MemoryContext::new("alias tables");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { continue };
        let arena = Arena::new(1 << 22);
        if let Some(f) = front::walk(arena.handle(), fx.stmt, None) {
            if !f.tables().is_empty() {
                front_runs += 1;
                cells += f.tables().iter().map(|r| u64::from(r.1 - r.0)).sum::<u64>();
            }
            assert_eq!(puts(&f.nodes, f.tables()), puts(&f.nodes, &[]), "{}: the front walk's alias", path.display());
        }
        if let Ok(g) = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) {
            let mut nodes = Vec::new();
            let (_, runs) = key_nodes_runs(&g, &mut |n| nodes.push(n));
            if !runs.as_slice().is_empty() {
                key_runs += 1;
            }
            assert_eq!(puts(&nodes, runs.as_slice()), puts(&nodes, &[]), "{}: the content key's alias", path.display());
        };
    }
    eprintln!("alias tables: {front_runs} fixtures with front runs ({cells} positions), {key_runs} with key runs");
    assert!(front_runs > 50 && key_runs > 50, "the corpus holds VALUES lists of Consts: front {front_runs}, key {key_runs}");
}
