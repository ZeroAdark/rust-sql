// D2847 / D2848's class, the general form: a lowering decision on a by-value literal's value is filed as a literal guard
// (key.rs `lit_decide`), and a program serves another statement of its content key only where the guard decides alike on
// that statement's literal. Over bench-165's `g JOIN gk ON g.id % 1000 = gk.k`: the divisor 1000 and 7 share the content
// key (one divisor class, a word), a guard reading `divisor > 100` holds for 1000 and not for 7, and the key the program
// is filed under (`variant`) follows the guard's outcome, never the literal. PS1-21 step 1b: a decision on a relation's
// page bound is a guard of its own kind, decided again on the pages now.
use std::path::Path;

use gravity_decompose::{corpus, decompose_in, AttFact, ExprFp, Facts, IndexFact, NoFacts, PipelineGraph, RelFact};
use gravity_lower::inputs::key::{guarded, guards_hold, has_rels, key, key_and_nodes, lit_decide, lits_hold, place_lits, rel_decide, rels_hold, variant, EstGuard};
use gravity_program::Arena;
use mcx::MemoryContext;
use types_nodes::Node;

const PLAN: &str = "../decompose/tests/plans/bench-165.plan";
const DIVISOR: &str = ":constvalue 4 [ 232 3 0 0 0 0 0 0 ]";

fn text(bytes: &str) -> String {
    let t = std::fs::read_to_string(manifest_dir().join(PLAN)).unwrap();
    assert_eq!(t.matches(DIVISOR).count(), 2, "the fixture's divisor, in the join clause and the hash key");
    t.replace(DIVISOR, &format!(":constvalue 4 [ {bytes} ]"))
}

fn with_graph<T>(bytes: &str, f: impl FnOnce(&PipelineGraph<'_>) -> T) -> T {
    let ctx = MemoryContext::new("lit guard");
    let fx = corpus::load(ctx.mcx(), &text(bytes)).unwrap_or_else(|e| panic!("fixture: {e}"));
    let arena = Arena::new(1 << 22);
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|r| panic!("decompose: {r}"));
    f(&g)
}

/// The key walk's nodes of `g`.
fn nodes<'p>(g: &PipelineGraph<'p>) -> Vec<Node<'p>> {
    let mut v = Vec::new();
    key_and_nodes(g, &mut |n| v.push(n));
    v
}

fn above_100(word: u64, _: &[u64; 4]) -> u64 {
    (word as u32 > 100) as u64
}

/// The guards a decision on the divisor (its first by-value Const of value `of`) files, placed as the admission places
/// them (exec admit.rs `install`), and its outcome.
fn decided(g: &PipelineGraph<'_>, of: u64) -> (Vec<EstGuard>, Option<u64>) {
    let walk = nodes(g);
    let at = walk.iter().copied().find(|n| n.as_const().is_some_and(|c| c.constbyval && !c.constisnull && c.constvalue.as_u64() & 0xffff_ffff == of)).expect("the divisor in the key walk");
    let (want, mut guards) = guarded(|| lit_decide(at, above_100, [0; 4]));
    place_lits(&mut guards, &walk);
    (guards, want)
}

#[test]
fn a_literal_decision_files_a_guard_and_the_variant_follows_its_outcome() {
    let (guards, k1000, filed) = with_graph("232 3 0 0 0 0 0 0", |g| {
        let (guards, want) = decided(g, 1000);
        assert_eq!(want, Some(1), "1000 > 100");
        assert_eq!(guards.len(), 1, "one literal guard");
        assert!(guards_hold(&guards, g, &NoFacts) && lits_hold(&guards, &nodes(g)), "the guard holds on its own statement");
        let k = key(g).0;
        assert_eq!(variant(k, &guards, Some((g, &NoFacts))), variant(k, &guards, None), "its own outcome is the filed one");
        let filed = variant(k, &guards, None);
        (guards, k, filed)
    });
    with_graph("7 0 0 0 0 0 0 0", |g| {
        assert_eq!(key(g).0, k1000, "a nonzero divisor is a word: one content key");
        assert!(!guards_hold(&guards, g, &NoFacts) && !lits_hold(&guards, &nodes(g)), "7 decides the other way");
        assert_ne!(variant(k1000, &guards, Some((g, &NoFacts))), filed, "the statement looks for the program filed for its own outcome");
    });
    with_graph("208 7 0 0 0 0 0 0", |g| {
        assert_eq!(key(g).0, k1000, "2000: the same key");
        assert!(guards_hold(&guards, g, &NoFacts) && lits_hold(&guards, &nodes(g)), "2000 decides as 1000 did");
        assert_eq!(variant(k1000, &guards, Some((g, &NoFacts))), filed, "and finds the filed program");
    });
}

#[test]
fn a_literal_outside_the_key_walk_holds_for_no_statement() {
    with_graph("232 3 0 0 0 0 0 0", |g| {
        let (mut guards, _) = decided(g, 1000);
        place_lits(&mut guards, &[]);
        assert!(!guards_hold(&guards, g, &NoFacts) && !lits_hold(&guards, &nodes(g)), "an unplaced literal decides for no other statement");
        let k = key(g).0;
        assert_ne!(variant(k, &guards, Some((g, &NoFacts))), variant(k, &guards, None), "so the statement never finds the program");
    });
}

/// A relation's rows bounded by its pages now (hosts facts.rs `rel_rows_bound`), set by the test.
struct Pages(core::cell::Cell<Option<u64>>);

impl Facts for Pages {
    fn rel(&self, _: u32) -> Option<RelFact> {
        None
    }

    fn att(&self, _: u32, _: i16) -> Option<AttFact> {
        None
    }

    fn index(&self, _: u32) -> Option<IndexFact> {
        None
    }

    fn indexes_of(&self, _: u32, _: &mut dyn FnMut(IndexFact)) {}

    fn index_expr(&self, _: u32, _: u8) -> Option<ExprFp> {
        None
    }

    fn rel_rows_bound(&self, relid: u32) -> Option<u64> {
        self.0.get().filter(|_| relid == 16384)
    }
}

fn below(rows: u64, a: &[u64; 4]) -> u64 {
    (rows < a[0]) as u64
}

/// PS1-21 / PS1-26 step 1b: a decision on a relation's page bound (join.buildside's heap side) is a relation guard: no
/// literal (placing the literals leaves it), decided again on the relation's pages now by every check that serves its
/// program (`guards_hold`, `rels_hold`), and the key its program is filed under follows its outcome there (`variant`): a
/// heap grown past the bound (an INSERT, no invalidation) takes the program filed for the other outcome, else lowers.
#[test]
fn a_relation_guard_decides_on_the_pages_now() {
    with_graph("232 3 0 0 0 0 0 0", |g| {
        let (want, mut guards) = guarded(|| rel_decide(16384, 582, below, [200_000, 0, 0, 0]));
        assert_eq!(want, 1, "582 rows bound the heap side below 200,000");
        assert!(guards.len() == 1 && has_rels(&guards), "one relation guard");
        place_lits(&mut guards, &nodes(g));
        assert!(lits_hold(&guards, &nodes(g)), "no literal guard: a front hit's literal check passes it to `rels_hold`");
        let k = key(g).0;
        let filed = variant(k, &guards, None);
        let pages = Pages(core::cell::Cell::new(Some(582)));
        assert!(rels_hold(&guards, &pages) && guards_hold(&guards, g, &pages), "the pages it was lowered on");
        assert_eq!(variant(k, &guards, Some((g, &pages))), filed, "finds its own program");
        pages.0.set(Some(3_000_000 / 291 * 291 + 291));
        assert!(!rels_hold(&guards, &pages) && !guards_hold(&guards, g, &pages), "the heap grew past the bound");
        assert_ne!(variant(k, &guards, Some((g, &pages))), filed, "the statement looks for the program of the other outcome");
        pages.0.set(None);
        assert!(!rels_hold(&guards, &pages) && !guards_hold(&guards, g, &pages), "no bound (the relation is no heap now): no serve");
        assert!(guards_hold(&[], g, &pages) && rels_hold(&[], &pages) && !has_rels(&[]), "no guard: nothing to decide");
    });
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process).
fn manifest_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
