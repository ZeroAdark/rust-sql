//! M5-PL (cards/M5.md `srf/pl_function.rs`; M5-plan §3.2, M5-ENTRY-SPI §4.1 R1-R7, Decisions 6 / 7): kind srf.pl, a FROM
//! entry of user functions read from PG's own FunctionScan store (the form (c) step, srf/function_store.rs). The crate runs
//! no SetFn protocol (no incumbent node, no body), so the server runner judges the rows (e2e m5_pl, m5_spi_*); here: what
//! the lowering declares. G4: a builtin set function in FROM is srf.function, and the target-list form stays PSET's refusal.
use gravity_lowerv4_it as common;

use gravity_kernels::ops::srf::entries::{setfn_parts, setfn_private};
use gravity_program::protos::ProtoKind;

fn text(rel: &str) -> String {
    let p = common::plans_dir().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// PL2 (bench-104: `SELECT count(*), sum(a1) FROM p2()`, a plpgsql function): srf.pl lowers to one SetFn protocol that
/// names the FunctionScan (node 1: the store the incumbent fills at the first pull), so the program is its session's
/// (Decision 7, V4R-258); its walk names srf.pl.
#[test]
fn a_pl_function_in_from_reads_the_incumbent_store_and_stays_private() {
    let (kinds, _) = common::props_fixture(&text("bench-104.plan")).expect("decomposes");
    assert!(kinds.contains("srf.pl"), "bench-104 needs srf.pl: {kinds:?}");
    let l = common::lower_fixture(&text("bench-104.plan")).expect("decomposes").unwrap_or_else(|r| panic!("bench-104: {r}"));
    let sets: Vec<u64> = l.descs.v4.protos.iter().filter(|p| p.kind == ProtoKind::SetFn).map(|p| p.arg).collect();
    assert_eq!(sets.len(), 1, "one SetFn protocol");
    assert_eq!(setfn_parts(sets[0]).2, Some(1), "it names the FunctionScan (plan node 1)");
    assert!(setfn_private(sets[0]), "a PL set's program stays the session's (Decision 7)");
}

/// G4: a builtin set function in FROM (`unnest`, `jsonb_each`) is not srf.pl.
#[test]
fn a_builtin_set_function_in_from_is_not_srf_pl() {
    for name in ["srf-016", "srf-022"] {
        let (kinds, _) = common::props_fixture(&text(&format!("{name}.plan"))).expect("decomposes");
        assert!(!kinds.contains("srf.pl") && kinds.contains("srf.function"), "{name}: {kinds:?}");
    }
}
