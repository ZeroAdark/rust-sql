// MC-1 (V4R-419, main 09-28): the content key hashes a Memoize's hash equality operators and collations. nest.memo
// bakes each key's operator and collation into the lookup (the key block m8_memo_miss reads), so two plans differing
// only there must key apart; the key hashed the key expressions, entries, singlerow and binary_mode alone, and a cached
// program would compare keys under the other plan's collation.
use std::path::Path;

use gravity_decompose::{corpus, decompose_in};
use gravity_lower::inputs::key::key;
use gravity_program::{Arena, ContentKey};
use mcx::MemoryContext;

/// bench-131: a nest whose inner is a Memoize keyed by one text column (texteq, the default collation).
const PLAN: &str = "../decompose/tests/plans/bench-131.plan";
const OPS: &str = ":hashOperators ( 98) :collations ( 100)";

fn key_with(to: &str) -> ContentKey {
    let text = std::fs::read_to_string(manifest_dir().join(PLAN)).unwrap();
    assert_eq!(text.matches(OPS).count(), 1, "the fixture holds one Memoize keyed by texteq under the default collation");
    let text = text.replacen(OPS, to, 1);
    let ctx = MemoryContext::new("memo key");
    let fx = corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let arena = Arena::new(1 << 20);
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|r| panic!("decompose: {r}"));
    key(&g)
}

#[test]
fn a_memoize_keys_apart_by_its_collation_and_operator() {
    let base = key_with(OPS);
    assert_eq!(base, key_with(OPS), "one plan, one key");
    // 950 = "C": the same texteq under another collation
    assert_ne!(base, key_with(":hashOperators ( 98) :collations ( 950)"), "two Memoize collations share a key");
    // 1054 = bpchareq: another hash equality operator over the same key
    assert_ne!(base, key_with(":hashOperators ( 1054) :collations ( 100)"), "two Memoize hash operators share a key");
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process).
fn manifest_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
