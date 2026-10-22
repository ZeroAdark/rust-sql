//! V4R-26 (count(DISTINCT int8) over a pgrc column, cards/M2.md): the plan fixtures whose plain `count(DISTINCT col)` over a
//! pgrc scan column of an integer image take each window whole through `agg_distinct_window` (keyed/distinct_plain.rs
//! `whole`, the window host), and the shapes it must leave to the rows (G4) do not declare it: a text argument, an
//! expression argument (a filter, a heap scan and the other shapes run in e2e/gravity/v4/m2_distinct_count_alone.sql).
use gravity_lowerv4_it as common;

fn plan(name: &str) -> String {
    std::fs::read_to_string(common::plans_dir().join(format!("{name}.plan"))).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// Whether fixture `name` lowers and its program declares the window host.
fn window_host(name: &str) -> bool {
    let l = match common::lower_fixture(&plan(name)) {
        Some(Ok(l)) => l,
        Some(Err(e)) => panic!("{name} lowers: {e:?}"),
        None => panic!("{name}: decompose refused"),
    };
    l.ir.host_fns.iter().any(|h| h.name == "agg_distinct_window")
}

#[test]
fn a_plain_count_distinct_of_a_pgrc_integer_column_takes_windows_whole() {
    for f in ["fieldobs-009", "aggs-040"] {
        assert!(window_host(f), "{f}: count(DISTINCT int8 column) over pgrc declares agg_distinct_window");
    }
}

#[test]
fn g4_the_shapes_the_window_host_leaves_to_the_rows() {
    for (f, why) in [
        ("fieldobs-010", "a text argument (an Fp128 image)"),
        ("aggs-039", "an expression argument"),
        ("aggs-041", "a text column"),
    ] {
        assert!(!window_host(f), "{f}: {why} must not declare the window host");
    }
}
