//! memoize lane (main 10-03): a parameterized nest's key walk visits each NestLoopParam node (lower key.rs
//! `JoinDesc::Nest` hands `params` to the visitor), which the front walk keyed (its paramno and paramval) but never
//! placed, so `front::file` refused the shape ("a key-walk node outside the plan") and every execution of a Memoize nest
//! or any other parameterized nest decomposed and keyed again (psql#689 / #723 / #746 at 1.28-1.32x rowexec, all of it
//! decompose). Now the walk places the param node: the first execution files and the second is a front hit binding its
//! own plan's nodes.
use gravity_decompose::{corpus, decompose_in};
use gravity_exec::inputs::front;
use gravity_program::Arena;
use mcx::MemoryContext;
use types_nodes::NodeTag;

/// A Memoize nest (binary key, logical text key) and a parameterized index inner without one (literals, then parameters).
const FIXTURES: [&str; 4] = ["bench-130", "bench-131", "oltpc-056", "oltpc-117"];

#[test]
fn a_parameterized_nest_files_and_the_next_execution_hits() {
    for name in FIXTURES {
        let text = std::fs::read_to_string(manifest_dir().join(format!("../decompose/tests/plans/{name}.plan"))).unwrap();
        let (ctx, arena) = (MemoryContext::new("nest params"), Arena::new(1 << 22));
        let fx = corpus::load(ctx.mcx(), &text).unwrap();
        let f = front::walk(arena.handle(), fx.stmt, None).unwrap_or_else(|| panic!("{name}: the front walk keys the plan"));
        let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|_| panic!("{name}: decomposes"));
        let mut nodes = Vec::new();
        let key = gravity_lower::inputs::key::key_and_nodes(&g, &mut |n| nodes.push(n));
        assert!(nodes.iter().any(|n| n.node_tag() == NodeTag::T_NestLoopParam), "{name}: the key walk visits a nest param");
        front::clear();
        assert_eq!(front::fill_reading(&f, key, &nodes, front::needs(&f, fx.stmt, &fx.facts), &[]), Ok(()), "{name}: filed");
        // the second execution: the same text read again (other nodes, the same shape)
        let ctx2 = MemoryContext::new("nest params 2");
        let fx2 = corpus::load(ctx2.mcx(), &text).unwrap();
        let f2 = front::walk(arena.handle(), fx2.stmt, None).unwrap();
        let (k2, hit) = front::hit(&f2, arena.handle()).unwrap_or_else(|| panic!("{name}: a front hit on the 2nd execution"));
        assert_eq!(k2, key, "{name}");
        let g2 = decompose_in(arena.handle(), fx2.stmt, &fx2.facts, fx2.mcx).unwrap();
        let mut fresh = Vec::new();
        assert_eq!(gravity_lower::inputs::key::key_and_nodes(&g2, &mut |n| fresh.push(n)), key, "{name}");
        assert_eq!(hit.len(), fresh.len(), "{name}");
        assert!(hit.iter().zip(&fresh).all(|(a, b)| a.ptr_eq(*b)), "{name}: the hit's nodes are the new plan's");
    }
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process).
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
