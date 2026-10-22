// D1103 (D1099's program-cache audit, main 10:12Z / 10:40Z): both keys hold an integer modulus's divisor class (zero or
// not), never its literal. lowerv4's raise-free analysis (join/rtfilter.rs `mod_by_const`) reads the divisor's word and
// bakes the build side / probe prefix it allows, so `x % 0` right after `x % 7` ran the raise-free program (e2e
// m1_d1103_mod_divisor_class). Over bench-165's `g JOIN gk ON g.id % 1000 = gk.k`: the literal still keys nothing.
use std::path::Path;

use gravity_decompose::{corpus, decompose_in};
use gravity_exec::inputs::front;
use gravity_program::{Arena, ContentKey};
use mcx::MemoryContext;

const PLAN: &str = "../decompose/tests/plans/bench-165.plan";
const DIVISOR: &str = ":constvalue 4 [ 232 3 0 0 0 0 0 0 ]";

/// (the front key's main hash, the content key) with the divisor 1000 as `bytes`.
fn keys(bytes: &str) -> (u128, ContentKey) {
    let text = std::fs::read_to_string(manifest_dir().join(PLAN)).unwrap();
    assert_eq!(text.matches(DIVISOR).count(), 2, "the fixture's divisor, in the join clause and the hash key");
    let text = text.replace(DIVISOR, &format!(":constvalue 4 [ {bytes} ]"));
    let (ctx, arena) = (MemoryContext::new("d1103"), Arena::new(1 << 22));
    let fx = corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let f = front::walk(arena.handle(), fx.stmt, None).expect("bench-165 walks");
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|r| panic!("decompose: {r}"));
    (f.main, gravity_lower::inputs::key::key(&g))
}

#[test]
fn the_divisor_class_keys_apart_never_the_literal() {
    let (k1000, k7, k0) = (keys("232 3 0 0 0 0 0 0"), keys("7 0 0 0 0 0 0 0"), keys("0 0 0 0 0 0 0 0"));
    assert_eq!(k1000, k7, "a nonzero literal is a word: one front key, one content key");
    assert_ne!(k1000.0, k0.0, "the front key holds the divisor's class");
    assert_ne!(k1000.1, k0.1, "the content key holds the divisor's class");
    assert_eq!(k0, keys("0 0 0 0 0 0 0 0"), "one class, one key");
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process).
fn manifest_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
