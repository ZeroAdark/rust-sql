//! D1108 row 1 (V4R-541, M8-NEST): nest.spool's quiet child (effects/visible/material.rs `quiet`). A plan Material whose child is made of
//! constants only (FROM generate_series over int4 / int8, unnest, D449-pure filters and projections, INNER nests without parameters of
//! the same) takes the eager form: the census fixtures serve. A zero step serves too: its child raises 22023 ("step size cannot equal
//! zero"), code that only raises runs ahead tagged (`licence_unchecked`), and AGENTS.md "Errors do not constrain evaluation order"
//! (2026-10-17) frees the read side of an INSERT ... SELECT as a SELECT's: 16e3cb228bb stopped the statement's INSERT from denying it.
//! C PostgreSQL 18.6 raises the same 22023 for every zero-step form of this statement (checked 2026-10-20). Both keys hold the step's
//! class (lower key.rs `series_step`), never its literal, so a zero step still keys apart from the program a nonzero one lowered.
use gravity_lowerv4_it as common;

use gravity_decompose::corpus;

/// spgist#12: INSERT .. SELECT over generate_series(1, 100, 5) i, generate_series(1, 100, 5) j, generate_series(1, 10) s.
const SPGIST: &str = "regress-full/spgist-002.plan";
/// pg_lsn#30: SELECT DISTINCT over three series, two Materials deep.
const PG_LSN: &str = "regress-full/pg_lsn-001.plan";
/// spgist-002's two step constants (5), by location.
const STEPS: [&str; 2] = [":location 97 :constvalue 4 [ 5 0 0 0 0 0 0 0 ]", ":location 132 :constvalue 4 [ 5 0 0 0 0 0 0 0 ]"];

fn text(rel: &str) -> String {
    let p = common::plans_dir().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// spgist-002 with both steps `step`.
fn spgist(step: u8) -> String {
    STEPS.iter().fold(text(SPGIST), |t, s| {
        assert_eq!(t.matches(s).count(), 1, "the fixture's step {s}");
        t.replace(s, &s.replace("[ 5 ", &format!("[ {step} ")))
    })
}

fn key(text: &str) -> gravity_program::ContentKey {
    let (ctx, arena) = (mcx::MemoryContext::new("d1108 key"), gravity_program::Arena::new(1 << 20));
    let fx = corpus::load(ctx.mcx(), text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let g = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).ok().expect("decomposes");
    gravity_lower::inputs::key::key(&g)
}

#[test]
fn the_census_forms_serve() {
    for rel in [SPGIST, PG_LSN] {
        if let Err(r) = common::lower_fixture(&text(rel)).expect("decomposes") {
            panic!("{rel} refused: {r:?}");
        }
    }
}

#[test]
fn a_zero_step_lowers_and_keys_apart() {
    if let Err(r) = common::lower_fixture(&spgist(0)).expect("decomposes") {
        panic!("a zero step refused: {r:?}");
    }
    assert!(common::lower_fixture(&spgist(3)).expect("decomposes").is_ok(), "a step of 3 serves");
    let (k5, k3, k0) = (key(&text(SPGIST)), key(&spgist(3)), key(&spgist(0)));
    assert_eq!(k5, k3, "a nonzero step is a word: one content key");
    assert_ne!(k5, k0, "the content key holds the step's class");
    assert_eq!(k0, key(&spgist(0)), "one class, one key");
}
