// The content key over a FieldStore (`INSERT INTO t (c.a, c.b) ...` / `UPDATE t SET c.a = ...`): the emitted store
// binds the row type and which fields it writes, and its new values are its children (decompose expr.rs `kids`). Two
// statements that differ only in the fields they assign must not share a program. Before this, `kids` knew no
// FieldStore and the key hashed its tag alone: `(c.a, c.b)` and `(c.b, c.a)` keyed alike.
use std::path::Path;

use gravity_decompose::{corpus, decompose_in};
use gravity_lower::inputs::key::key_and_nodes;
use gravity_program::Arena;
use mcx::MemoryContext;
use types_nodes::NodeTag;

fn key_of(text: &str) -> (u128, usize) {
    let ctx = MemoryContext::new("fixture");
    let fx = corpus::load(ctx.mcx(), text).unwrap();
    let arena = Arena::new(1 << 20);
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap();
    let mut stores = 0;
    let k = key_and_nodes(&g, &mut |n| stores += (n.node_tag() == NodeTag::T_FieldStore) as usize).0;
    (k, stores)
}

#[test]
fn a_field_store_keys_its_fields_and_its_new_values() {
    // regress insert-009: insert into inserttest (f3.if1, f3.if2) select 3, '{baz,quux}'
    let path = manifest_dir().join("../decompose/tests/plans/regress-full/insert-009.plan");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains(":fieldnums (i 1 2)"), "fixture changed");
    let (base, stores) = key_of(&text);
    assert_eq!(stores, 1, "the key walk visits the FieldStore");
    assert_eq!(key_of(&text).0, base, "the key is stable");
    // the same statement assigning the fields the other way round: another program
    let swapped = text.replace(":fieldnums (i 1 2)", ":fieldnums (i 2 1)");
    assert_ne!(key_of(&swapped).0, base, "fields assigned in another order share a key");
    // one field fewer is also another shape
    let one = text.replace(":fieldnums (i 1 2)", ":fieldnums (i 1 1)");
    assert_ne!(key_of(&one).0, base, "other fields share a key");
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
