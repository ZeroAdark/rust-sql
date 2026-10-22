// div_by_const (main 2026-10-03 19:43Z, D1103's pattern): both keys hold an integer division's divisor class (0 or -1,
// or neither), never its literal. The effects home (decompose `effects::quiet_args`) passes `x / c` as raise-free for
// a constant other than 0 and -1, and F5's eager fill bakes on it, so `x / -1` right after `x / 10` would run the
// raise-free program. Over bench-016's `id / 100` (in its GROUP BY key): the literal still keys nothing.
use std::path::Path;

use gravity_decompose::{corpus, decompose_in};
use gravity_exec::inputs::front;
use gravity_program::{Arena, ContentKey};
use mcx::MemoryContext;

const PLAN: &str = "../decompose/tests/plans/bench-016.plan";
const DIVISOR: &str = ":constvalue 4 [ 100 0 0 0 0 0 0 0 ]";

/// (the front key's main hash, the content key) with the divisor 100 as `bytes`.
fn keys(bytes: &str) -> (u128, ContentKey) {
    let text = std::fs::read_to_string(manifest_dir().join(PLAN)).unwrap();
    assert_eq!(text.matches(DIVISOR).count(), 1, "the fixture's divisor, in the grouping key's int4div");
    let text = text.replace(DIVISOR, &format!(":constvalue 4 [ {bytes} ]"));
    let (ctx, arena) = (MemoryContext::new("div_by_const"), Arena::new(1 << 22));
    let fx = corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let f = front::walk(arena.handle(), fx.stmt, None).expect("bench-016 walks");
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|r| panic!("decompose: {r}"));
    (f.main, gravity_lower::inputs::key::key(&g))
}

#[test]
fn the_division_class_keys_apart_never_the_literal() {
    let (k100, k7, k_2) = (keys("100 0 0 0 0 0 0 0"), keys("7 0 0 0 0 0 0 0"), keys("254 255 255 255 255 255 255 255"));
    let (k0, k_1) = (keys("0 0 0 0 0 0 0 0"), keys("255 255 255 255 255 255 255 255"));
    assert_eq!(k100, k7, "a quiet literal is a word: one front key, one content key");
    assert_eq!(k100, k_2, "-2 is quiet too");
    assert_ne!(k100.0, k0.0, "the front key holds the divisor's class (0)");
    assert_ne!(k100.1, k0.1, "the content key holds the divisor's class (0)");
    assert_ne!(k100.0, k_1.0, "the front key holds the divisor's class (-1)");
    assert_ne!(k100.1, k_1.1, "the content key holds the divisor's class (-1)");
    assert_eq!(k0, k_1, "0 and -1 are one class, one key");
    assert_eq!(k_1, keys("255 255 255 255 0 0 0 0"), "an int4 -1 reads its low four bytes, however the word is extended");
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process).
fn manifest_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
