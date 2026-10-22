//! M1's tests (plan §5.4, §5.6; files.tsv: "its families vs PG, stop, cancel, SCRIBBLE"). The server runner
//! (scripts/gravity-v4/rig.py, main 09-24) checks rows against PostgreSQL 18.6 on real data in the four modes; this file
//! holds what the crate alone decides:
//! - the IR of each lowered M1 fixture is pinned (`tests/fixtures/m1/<name>.ir`; LOWERV4_PIN=write records them);
//! - the census Result / VALUES fixtures that need no data (regress-008, regress-009; main 09-24 decision 2) run through
//!   gravity_driver in all four modes against PostgreSQL 18.6's rows (`tests/fixtures/m1/<name>.plan` + `.pg`).
use gravity_lowerv4_it as common;

use std::collections::{BTreeMap, BTreeSet};

fn repo(rel: &str) -> String {
    let p = manifest_dir().join("../../..").join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// census/serve.tsv's first-serve milestone per id.
fn first_serve() -> BTreeMap<String, String> {
    repo("crates/gravity/lowerv4/testdata/gravity/lowerv4/census/serve.tsv").lines().skip(1).filter_map(|l| {
        let mut f = l.split('\t');
        Some((f.next()?.to_owned(), f.next()?.to_owned()))
    }).collect()
}

/// Each lowered M1 fixture's IR against its pin; a fixture with no pin yet is listed (LOWERV4_PIN=write records it).
#[test]
fn ir_pins() {
    let (first, dir) = (first_serve(), common::milestone_dir(1));
    let write = std::env::var("LOWERV4_PIN").is_ok_and(|v| v == "write");
    let (mut same, mut unpinned, mut diff) = (0, BTreeSet::new(), Vec::new());
    for (name, text) in common::files(&common::plans_dir(), "plan") {
        if first.get(&name).map(String::as_str) != Some("M1") {
            continue;
        }
        let Some(Ok(l)) = common::lower_fixture(&text) else { continue };
        let ir = common::pin_ir(&l);
        let pin = dir.join(format!("{name}.ir"));
        match std::fs::read_to_string(&pin) {
            _ if write => std::fs::write(&pin, &ir).unwrap_or_else(|e| panic!("{}: {e}", pin.display())),
            Ok(p) if p == ir => same += 1,
            Ok(_) => diff.push(name),
            Err(_) => drop(unpinned.insert(name)),
        }
    }
    eprintln!("IR pins: {same} same, {} differ, {} unpinned{}", diff.len(), unpinned.len(), if write { " (written)" } else { "" });
    assert!(diff.is_empty(), "IR differs from the pin (LOWERV4_PIN=write re-records after review): {diff:?}");
}

/// D3606: the rig lowers as a served statement runs: on a thread armed with its database's encoding, UTF8 (pgrust serves
/// no other; initdb and CREATE DATABASE refuse the rest), and with the rig databases' default collation (initdb
/// --no-locale: C). An unarmed test thread is SQL_ASCII with no default locale, and there every UTF8-gated text path
/// (text.memmem claims, like_bound, lower_like, text_frame, the RE2 dispatch) is off, so the pins never held them.
#[test]
fn the_rig_lowers_as_a_utf8_database() {
    let (_, text) = common::files(&common::plans_dir(), "plan").into_iter().find(|(n, _)| n == "fieldobs-007").expect("fieldobs-007's plan");
    assert!(matches!(common::lower_fixture(&text), Some(Ok(_))), "fieldobs-007 (source_url LIKE '%atlas%') lowers");
    assert_eq!(gravity_fn::typesem::collation::database_encoding(), 6, "the rig thread's database encoding: UTF8 (6), not SQL_ASCII (0)");
    assert!(pg_locale::default_locale_installed(), "the rig thread's database default locale");
}

/// Decision 2's in-crate runs: each `tests/fixtures/m1/*.plan` with a `.pg` golden, lowered, run by gravity_driver
/// (`common::run_fixture`) straight, stopped at every row, cancelled at the first push and under SCRIBBLE, and compared
/// with PostgreSQL's rows; every run prints an `m1 driver` line for `rig.py report --crate`. A named Shape refusal is not
/// a wrong answer (served coverage is v5probe's count).
#[test]
fn result_fixtures_run_through_the_driver() {
    let (mut ran, mut bad) = (0, Vec::new());
    for e in common::corpus(&common::milestone_dir(1)).into_iter().filter(|e| e.golden.is_some()) {
        for mode in common::MODES {
            let v = match common::run_fixture(&e.plan, mode) {
                None => "not decomposed".to_owned(),
                Some(Err(w)) if w.starts_with("refused: lowerv4: Shape: ") => format!("refused: {}", &w[9..]),
                Some(Err(w)) if w.starts_with("refused: ") && w.contains("NotBuilt") => format!("waiting: {}", &w[9..]),
                Some(Err(w)) => {
                    bad.push(format!("{} {}: {w}", e.name, common::mode_name(mode)));
                    format!("FAILED: {w}")
                }
                Some(Ok(got)) => {
                    ran += 1;
                    common::check(&e, mode, &got).map_or_else(|m| { bad.push(m.clone()); format!("WRONG: {m}") }, |()| "ok".to_owned())
                }
            };
            println!("m1 driver\t{}\t{}\t{v}", e.name, common::mode_name(mode));
        }
    }
    eprintln!("m1 driver: {ran} runs compared with PostgreSQL");
    assert!(bad.is_empty(), "{} driver runs:\n{}", bad.len(), bad.join("\n"));
}

/// The served-count proxy for the suites the server runner does not run (CB, TPCH, bench, OLTP, unit; `rig.py report
/// --crate`): every census plan fixture through lower_graph from its capture, one `m1 lower` line each (`lowered`, or
/// the refusal). Lowering is not execution, so the report labels these counts a proxy.
#[test]
fn census_lowering_proxy() {
    let (mut n, mut ok) = (0, 0);
    for (name, text) in common::files(&common::plans_dir(), "plan") {
        let Some(r) = common::lower_fixture(&text) else { continue };
        n += 1;
        ok += usize::from(r.is_ok());
        println!("m1 lower\t{name}\t{}", r.map_or_else(|r| format!("refused: {r}"), |_| "lowered".to_owned()));
    }
    eprintln!("m1 lower: {ok} of {n} census fixtures lowered");
}

/// D269: no census plan fixture refuses with a Bug. Lowering asks syscache seams outside the Catalog trait (gravity_expr's
/// aggregate and array paths); a seam the rig never installs panics "seam not installed", which lowering's catch turns
/// into a Bug refusal.
#[test]
fn census_fixtures_refuse_no_bug() {
    let bugs: Vec<String> = common::files(&common::plans_dir(), "plan").into_iter().filter_map(|(name, text)| match common::lower_fixture(&text) {
        Some(Err(r)) if r.what.starts_with("Bug") => Some(format!("{name}: {r}")),
        _ => None,
    }).collect();
    assert!(bugs.is_empty(), "{} census fixtures refuse with a Bug:\n{}", bugs.len(), bugs.join("\n"));
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(|d| std::path::Path::new(&d).with_file_name("lowerv4")).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
