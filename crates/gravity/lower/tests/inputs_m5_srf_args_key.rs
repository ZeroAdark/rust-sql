// D745 (M5-FROM): a FROM set function's by-reference arguments outside a scalar call or an inlined body's expression are
// keyed by their class (key.rs `Op::Srf`, the `lit` rule): form (c)'s incumbent FunctionScan evaluates them and the IR only
// deforms its store, so `jsonb_each('{"k": 1, ...}')` and `jsonb_each('{"k": 3, ...}')` share one program. They were keyed
// by image: one program per literal (the user's literal-free ruling). The class-keyed literal is logged with the lists
// (`class_keyed_lists`), so a lowering that read it files no program.
use std::path::Path;

use gravity_decompose::{corpus, decompose_in, PipelineGraph};
use gravity_lower::inputs::key::{class_keyed_lists, key};
use gravity_program::{Arena, ContentKey};
use mcx::MemoryContext;

/// `SELECT * FROM jsonb_each('{"k": 1, "j": 2}'::jsonb)`.
const EACH: &str = "../decompose/tests/plans/srf-022.plan";
/// The jsonb argument's last value, 1 (a numeric's base-10000 digit).
const ARG_TAIL: &str = "0 128 1 0 ]}";

fn fixture(rel: &str) -> String {
    std::fs::read_to_string(manifest_dir().join(rel)).unwrap()
}

fn with<R>(text: &str, f: impl FnOnce(&PipelineGraph<'_>) -> R) -> R {
    let ctx = MemoryContext::new("srf args key");
    let fx = corpus::load(ctx.mcx(), text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let arena = Arena::new(1 << 20);
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|r| panic!("decompose: {r}"));
    f(&g)
}

fn key_of(text: &str) -> ContentKey {
    with(text, key)
}

#[test]
fn another_argument_keys_the_same() {
    let base = fixture(EACH);
    assert_eq!(base.matches(ARG_TAIL).count(), 1, "the fixture holds the argument's tail once");
    // '{"k": 1, "j": 2}' -> '{"k": 3, "j": 2}'
    assert_eq!(key_of(&base.replacen(ARG_TAIL, "0 128 3 0 ]}", 1)), key_of(&base), "another jsonb_each argument");
}

#[test]
fn the_argument_is_class_keyed() {
    assert!(with(&fixture(EACH), |g| !class_keyed_lists(g).is_empty()), "the argument is logged with the class-keyed lists");
}

/// Does not apply: another function keys apart (the key holds the funcid, only the literal is its class).
#[test]
fn another_function_keys_apart() {
    let base = fixture(EACH);
    let text = base.replace(":funcid 3208 ", ":funcid 3932 ");
    assert_ne!(key_of(&text), key_of(&base), "jsonb_each_text is another program");
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
