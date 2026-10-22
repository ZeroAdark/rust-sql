//! M5-SERIES (cards/M5.md `srf/function_scan.rs`, `srf/iter.rs`): FunctionScan statements over the iterator form, lowered
//! as srf.function and run by the driver in the crate (`common::run_fixture`: a Result source runs here) straight,
//! stopped at every row (the offset word resumes the set inside its row, never re-opening it), cancelled at the first push
//! and under SCRIBBLE, each compared with PostgreSQL 18.6's rows.
use gravity_lowerv4_it as common;

use common::{check, parse_case, run_fixture, Entry, MODES};

/// (decompose plan fixture, PostgreSQL 18.6's result in the `.pg` case format).
const CASES: &[(&str, &str)] = &[
    ("srf-014", "sql: SELECT * FROM generate_series(1, 5) g(i) WHERE i > 2\n[pg]\n3\n4\n5\n"),
    ("srf-015", "sql: SELECT g, n FROM generate_series(10, 20) WITH ORDINALITY g(g, n) WHERE g > 13\n[pg]\n14\t5\n15\t6\n16\t7\n17\t8\n18\t9\n19\t10\n20\t11\n"),
    ("srf-027", "sql: SELECT i FROM generate_series(1, 100) g(i) WHERE i % 10 = 0 ORDER BY i\n[pg]\n10\n20\n30\n40\n50\n60\n70\n80\n90\n100\n"),
];

fn entry(name: &str, pg: &str) -> Entry {
    let plan = std::fs::read_to_string(common::plans_dir().join(format!("{name}.plan"))).unwrap_or_else(|e| panic!("{name}.plan: {e}"));
    Entry { name: name.to_owned(), plan, golden: Some(parse_case(name, pg).unwrap_or_else(|e| panic!("{e}"))) }
}

#[test]
fn from_series_equal_pg_in_every_mode() {
    let mut bad = Vec::new();
    for &(name, pg) in CASES {
        let e = entry(name, pg);
        for mode in MODES {
            match run_fixture(&e.plan, mode) {
                Some(Ok(got)) => bad.extend(check(&e, mode, &got).err()),
                other => bad.push(format!("{name} {}: did not run: {other:?}", common::mode_name(mode))),
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
