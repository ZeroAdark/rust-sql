//! D644 (M4-INSERT; main 09-26): a partial-width UPDATE's key walk holds nodes decompose synthesized (carry.rs `after`: a
//! carried column's Var and its resjunk TargetEntry), which no front walk holds, and `front::file` returned without
//! filing, silently: every simple-protocol execution decomposed again and keyed its by-reference literals by their images
//! (C35 3.75x v4rx). Now such a node files as a sentinel wherever the template reads no word or check, the next execution
//! is a front hit binding its own plan's nodes, and every shape not filed says why.
use std::path::Path;

use gravity_decompose::{corpus, decompose_in};
use gravity_exec::inputs::front;
use gravity_program::Arena;
use mcx::MemoryContext;
use types_nodes::Node;

fn fixture() -> String {
    std::fs::read_to_string(manifest_dir().join("../decompose/tests/plans/oltp-001.plan")).unwrap()
}

#[test]
fn a_synthesized_key_walk_node_files_by_sentinel_and_the_next_execution_hits() {
    let text = fixture();
    let (ctx, arena) = (MemoryContext::new("d644"), Arena::new(1 << 22));
    let fx = corpus::load(ctx.mcx(), &text).unwrap();
    let f = front::walk(arena.handle(), fx.stmt, None).unwrap();
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap();
    let mut nodes = Vec::new();
    let key = gravity_lower::inputs::key::key_and_nodes(&g, &mut |n| nodes.push(n));
    let plain = nodes.len();
    // carry.rs `after`: a carried column, made by decompose's allocator (no plan node), its TargetEntry junk
    let var = Node::mk_var(fx.mcx, 1, 2, 23, -1, 0, 0).unwrap();
    nodes.push(Node::mk_target_entry(fx.mcx, var, 99, None, true).unwrap());
    nodes.push(var);
    let (te_at, var_at) = (plain as u32, plain as u32 + 1);
    let needs = front::needs(&f, fx.stmt, &fx.facts);
    front::clear();

    // a word or check located at a synthesized node: never filed (a stand-in would bind)
    assert_eq!(front::fill_reading(&f, key, &nodes, needs, &[var_at]), Err("a word or check at a synthesized node"));
    // no locators to prove against (the tests' `fill`): never filed
    front::fill(&f, key, &nodes, needs);
    assert!(front::hit(&f, arena.handle()).is_none(), "not filed without the template's locators");
    // a literal outside the plan (a non-null Const decompose would have built): never filed
    let mut foreign = nodes.clone();
    foreign.push(Node::mk_const(fx.mcx, 23, -1, 0, 4, datum::Datum::from_i32(7), false, true).unwrap());
    assert_eq!(front::fill_reading(&f, key, &foreign, needs, &[]), Err("a key-walk node outside the plan"));
    assert!(front::hit(&f, arena.handle()).is_none());

    // the fix: filed, the synthesized positions by sentinel
    assert_eq!(front::fill_reading(&f, key, &nodes, needs, &[]), Ok(()));
    // the second execution: the same text read again (other nodes, the same shape) is a front hit
    let ctx2 = MemoryContext::new("d644 2");
    let fx2 = corpus::load(ctx2.mcx(), &text).unwrap();
    let f2 = front::walk(arena.handle(), fx2.stmt, None).unwrap();
    let (k2, hit) = front::hit(&f2, arena.handle()).expect("the partial-width shape is a front hit on its 2nd execution");
    assert_eq!(k2, key);
    assert_eq!(hit.len(), nodes.len());
    assert_eq!(front::filed_synth(&f2), vec![te_at, var_at]);
    // every other position binds the new plan's own node
    let g2 = decompose_in(arena.handle(), fx2.stmt, &fx2.facts, fx2.mcx).unwrap();
    let mut fresh = Vec::new();
    assert_eq!(gravity_lower::inputs::key::key_and_nodes(&g2, &mut |n| fresh.push(n)), key);
    assert!(hit[..plain].iter().zip(&fresh).all(|(a, b)| a.ptr_eq(*b)), "the hit's nodes are the new plan's");
}

/// C3 S4d: the front key holds no catalog epoch. A variant holds what its admission read (gravity_deps), and a hit serves
/// it only while those reads stand (replaying them into the hit's recording); a change to another relation leaves it.
#[test]
fn a_front_variant_serves_while_its_recorded_reads_stand() {
    let (r, other): (u32, u32) = (0xFFFF_E3D1, 0xFFFF_E3D2);
    let text = fixture();
    let (ctx, arena) = (MemoryContext::new("s4d"), Arena::new(1 << 22));
    let fx = corpus::load(ctx.mcx(), &text).unwrap();
    let f = front::walk(arena.handle(), fx.stmt, None).unwrap();
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap();
    let mut nodes = Vec::new();
    let key = gravity_lower::inputs::key::key_and_nodes(&g, &mut |n| nodes.push(n));
    let needs = front::needs(&f, fx.stmt, &fx.facts);
    front::clear();
    let ((), read) = gravity_deps::recording(|| gravity_deps::note_rel(r));
    assert_eq!(front::fill_reading_est(&f, key, &nodes, needs, &[], None, None, &read), Ok(()));
    let main = f.main;
    let (hit, replayed) = gravity_deps::recording(|| front::hit(&f, arena.handle()).map(|h| h.0));
    assert_eq!(hit, Some(key), "nothing moved: a hit");
    assert!(replayed.contains(gravity_deps::Dep::Rel(r)), "a hit's recording holds the variant's reads");
    l2cache::invalidate_key(l2cache::Domain::Rel(other), l2cache::rel_key(other, 0));
    let f2 = front::walk(arena.handle(), fx.stmt, None).unwrap();
    assert_eq!(f2.main, main, "an invalidation keys alike");
    assert_eq!(front::hit(&f2, arena.handle()).map(|h| h.0), Some(key), "another relation moved: still a hit");
    l2cache::invalidate_key(l2cache::Domain::Rel(r), l2cache::rel_key(r, 0));
    assert!(front::hit(&f2, arena.handle()).is_none(), "its read moved: no hit");
    assert_eq!(front::memo_len(), 0, "and the variant is gone");
    let ((), again) = gravity_deps::recording(|| gravity_deps::note_rel(r));
    assert_eq!(front::fill_reading_est(&f2, key, &nodes, needs, &[], None, None, &again), Ok(()));
    assert_eq!(front::hit(&f2, arena.handle()).map(|h| h.0), Some(key), "filed again after the change: a hit");
    front::clear();
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
