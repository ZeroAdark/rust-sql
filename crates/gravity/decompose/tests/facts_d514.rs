//! D514 (V4R-167, facts/demand/readers.rs `rewrite`): a unique-ification's bare non-key column rides a `min` cell only when every reader
//! above reads it inside a key expression. subselect-016 (the regress `(f1, f2) IN (SELECT f2, CAST(f3 AS int4) ..)`)
//! takes the cell; the same plan with its join also emitting `f3` raw (tests/d514/raw-read.plan, hand-edited) keeps
//! the bare column, so the lowerer's "a column the row does not carry" refusal stands.
use std::fs;
use std::path::Path;

use gravity_decompose::{corpus, decompose_in, dump};
use gravity_program::Arena;
use mcx::MemoryContext;

fn graph(rel: &str) -> String {
    let path = manifest_dir().join("tests").join(rel);
    let text = fs::read_to_string(&path).unwrap();
    let ctx = MemoryContext::new("d514");
    let fx = corpus::load(ctx.mcx(), &text).unwrap();
    let arena = Arena::new(1 << 20);
    let mut out = String::new();
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).expect("decomposes");
    dump::dump(&g, &mut out).unwrap();
    out
}

#[test]
fn a_passenger_read_only_under_its_key_rides_a_min_cell_and_a_raw_read_keeps_the_bare_column() {
    let ok = graph("plans/regress-full/subselect-016.plan");
    assert!(ok.contains("project (1=outer.1, 2=agg2136(1=outer.2))"), "{ok}");
    assert!(ok.contains("fn=2136 reg=MinF64"), "{ok}");
    let raw = graph("d514/raw-read.plan");
    assert!(raw.contains("project (1=outer.1, 2=outer.2)"), "{raw}");
    assert!(!raw.contains("agg2136"), "{raw}");
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
