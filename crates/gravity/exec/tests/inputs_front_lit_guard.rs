// D2848's general form, the front path: a program with a literal guard (lower key.rs `lit_decide`) files its front entry
// under its variant key (admit.rs: the D646 check keys the walk's epoch as `front_check` does, a guarded program's variant
// included), and only where a front hit rebuilds the guard's literal (`lits_walked`): a node of the front walk, or a
// constant decompose built (front.rs files it as `Built` only, rebuilt from the hit's own plan constant), so `lits_hold`
// reads the hit statement's literal there. Over bench-165's `g JOIN gk ON g.id % 1000 = gk.k`.
use std::path::Path;

use gravity_decompose::{corpus, decompose_in};
use gravity_exec::inputs::front;
use gravity_lower::inputs::key::{guarded, key_and_nodes, lit_decide, lits_hold, lits_walked, place_lits, variant};
use gravity_program::Arena;
use mcx::MemoryContext;

const PLAN: &str = "../decompose/tests/plans/bench-165.plan";

fn above_100(word: u64, _: &[u64; 4]) -> u64 {
    (word as u32 > 100) as u64
}

#[test]
fn a_literal_guard_on_a_plan_literal_is_walked_and_names_the_variant() {
    let text = std::fs::read_to_string(manifest_dir().join(PLAN)).unwrap();
    let (ctx, arena) = (MemoryContext::new("lit guard front"), Arena::new(1 << 22));
    let fx = corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let f = front::walk(arena.handle(), fx.stmt, None).expect("bench-165 walks");
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|r| panic!("decompose: {r}"));
    let mut nodes = Vec::new();
    let base = key_and_nodes(&g, &mut |n| nodes.push(n));
    let at = nodes.iter().copied().find(|n| n.as_const().is_some_and(|c| c.constbyval && !c.constisnull && c.constvalue.as_u64() & 0xffff_ffff == 1000)).expect("the divisor in the key walk");
    let (want, mut guards) = guarded(|| lit_decide(at, above_100, [0; 4]));
    place_lits(&mut guards, &nodes);
    assert_eq!((want, guards.len()), (Some(1), 1), "one literal guard, 1000 > 100");
    assert!(lits_walked(&guards, &nodes, &f.nodes), "the divisor is the plan's own Const: the front walk holds it");
    assert!(lits_hold(&guards, &nodes), "and the guard holds on its statement");
    assert_ne!(variant(base.0, &guards, None), base.0, "the entry names the variant, never the plain key");
    let walk: Vec<_> = f.nodes.iter().copied().filter(|n| n.as_raw().as_ptr() != at.as_raw().as_ptr()).collect();
    assert!(lits_walked(&guards, &nodes, &walk), "a by-value literal the front walk lacks is one decompose built: a hit rebuilds it (`Built`)");
    let (_, mut lost) = guarded(|| lit_decide(at, above_100, [0; 4]));
    place_lits(&mut lost, &[]);
    assert!(!lits_walked(&lost, &nodes, &f.nodes), "a guard no key-walk position names files no entry");
    assert!(lits_walked(&[], &nodes, &[]), "no literal guard, nothing to walk");
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process).
fn manifest_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
