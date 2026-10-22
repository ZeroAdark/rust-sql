// M5 L0-f (2) (M5-plan §1.3): the content key hashes an SRF op (`Op::Srf`, FROM and target list) by its structure, its
// calls through `node` (a literal placed as a word stays a word; the user's literal-free rule for programs). It used to
// hash the desc's dump text, which prints every argument's literal, so `generate_series(1, 5)` and `generate_series(1, 6)`
// keyed apart and shared no program.
use std::path::Path;

use gravity_decompose::{corpus, decompose_in, Op};
use gravity_lower::inputs::key::key;
use gravity_program::{Arena, ContentKey};
use mcx::MemoryContext;

/// `SELECT * FROM generate_series(1, 5) g(i) WHERE i > 2`.
const FROM: &str = "../decompose/tests/plans/srf-014.plan";

fn fixture(rel: &str) -> String {
    std::fs::read_to_string(manifest_dir().join(rel)).unwrap()
}

fn edit(text: &str, from: &str, to: &str) -> String {
    assert_eq!(text.matches(from).count(), 1, "the fixture holds {from} once");
    text.replacen(from, to, 1)
}

/// `text`'s content key, as the executor decomposes it; it must have an SRF op.
fn key_of(text: &str) -> ContentKey {
    let ctx = MemoryContext::new("srf key");
    let fx = corpus::load(ctx.mcx(), text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let arena = Arena::new(1 << 20);
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|r| panic!("decompose: {r}"));
    assert!(g.pipelines.iter().any(|p| p.ops.iter().any(|o| matches!(o, Op::Srf(_)))), "an SRF op");
    key(&g)
}

#[test]
fn a_series_bound_literal_is_no_part_of_the_key() {
    let base = fixture(FROM);
    // stop 5 -> 6: a by-value literal, a word of the program
    assert_eq!(key_of(&edit(&base, ":constvalue 4 [ 5 0 0 0 0 0 0 0 ]", ":constvalue 4 [ 6 0 0 0 0 0 0 0 ]")), key_of(&base), "another stop");
}

/// Does not apply (G4): another function keys apart.
#[test]
fn another_function_keys_apart() {
    let base = fixture(FROM);
    // int4 generate_series (1067) -> int8's (1069) is a different call with the same text shape
    let other = edit(&base, ":funcid 1067 ", ":funcid 1069 ");
    assert_ne!(key_of(&other), key_of(&base), "another function");
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
