//! M2's rig (plan M2-plan.md §8, cards/M2.md "tests/m2.rs"): the M2 corpus lowered through lowerv4 and held to what
//! the crate alone decides. The server runner (scripts/gravity-v4/rig.py) runs the host-sited statements against PG.
//! - The seed corpus `tests/fixtures/m2/corpus.d/` (a file per fixture) pins each fixture's verdict by fixture name:
//!   `served`, or `refused <text>`. Rows: the M2-DRYRUN bar statements and a regress sample per family.
//! - Each seed row that serves is pinned by IR (`<fixture>.ir`; LOWERV4_PIN=write records it). Rows with a PG golden
//!   (`<fixture>.plan` + `.pg` in fixtures/m2) run through gravity_driver straight and under a stop, cancel and
//!   SCRIBBLE, compared against PG's rows. Host-sited rows are the server runner's.
use gravity_lowerv4_it as common;

use std::collections::BTreeMap;


/// A census id's decompose plan fixture: `plans/<id>.plan`, or `plans/regress-full/<stem>.plan` for the regress-full
/// capture. `None` when the capture kept no fixture (e2e ids, bar-* ids, oversize plans).
fn plan_text(id: &str) -> Option<String> {
    let rel = match id.strip_prefix("regress-full-") {
        Some(stem) => format!("regress-full/{stem}.plan"),
        None => format!("{id}.plan"),
    };
    std::fs::read_to_string(common::plans_dir().join(rel)).ok()
}

/// One fixture's verdict in the corpus.d form: `served` or the refusal text.
fn verdict(text: &str) -> Option<String> {
    Some(match common::lower_fixture(text)? {
        Ok(_) => "served".to_owned(),
        Err(e) => format!("refused {e}"),
    })
}

/// One file per entry under `tests/fixtures/m2/<dir>` (the user 09-24: lists several lanes append to are directories; each
/// dir's README says what an entry means): `<fixture>.tsv` -> its first line's tab fields. The lists:
/// - `rig_gaps.d`: every need built, but the rig catalog (the reference `.dat` files, no server) cannot answer the types or
///   expressions (user domains / types, xml, a syscache seam the rig never installs); the server runner judges them.
/// - `oracle_gaps.d`: goldens whose rows read state the crate has no source for (`CURRENT_DATE` is the transaction start's
///   date, 0 = 2000-01-01 without a transaction); pg_regress judges them through the server.
fn entries(dir: &str) -> BTreeMap<String, Vec<String>> {
    let d = common::milestone_dir(2).join(dir);
    let mut out = BTreeMap::new();
    for e in std::fs::read_dir(&d).unwrap_or_else(|e| panic!("{}: {e}", d.display())) {
        let p = e.expect("dir entry").path();
        let Some(id) = p.file_name().and_then(|f| f.to_str()).and_then(|f| f.strip_suffix(".tsv")) else { continue };
        let t = std::fs::read_to_string(&p).expect("entry file");
        let first = t.lines().find(|l| !l.starts_with('#')).unwrap_or("");
        out.insert(id.to_owned(), first.split('\t').map(str::to_owned).collect());
    }
    out
}

/// `tests/fixtures/m2/corpus.d/<fixture>.tsv`: its row(s) `fixture  source  expect` (expect = `served` | `refused <text>`),
/// one file per fixture (the user 09-24; the README there); column 1 names the file.
fn seed() -> Vec<(String, String, String)> {
    let d = common::milestone_dir(2).join("corpus.d");
    let mut files: Vec<_> = std::fs::read_dir(&d).expect("fixtures/m2/corpus.d").map(|e| e.expect("dir entry").path()).collect();
    files.retain(|p| p.extension().is_some_and(|x| x == "tsv"));
    files.sort();
    let mut rows = Vec::new();
    for p in files {
        let id = p.file_stem().and_then(|f| f.to_str()).expect("utf-8 name").to_owned();
        for l in std::fs::read_to_string(&p).expect("corpus.d file").lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
            let f: Vec<&str> = l.split('\t').collect();
            assert_eq!(f.len(), 3, "corpus.d/{id}.tsv row {l:?}");
            assert_eq!(f[0], id, "corpus.d/{id}.tsv: column 1 names the file");
            rows.push((f[0].to_owned(), f[1].to_owned(), f[2].to_owned()));
        }
    }
    rows
}

/// Every entry of the two lists names a plan fixture, and no fixture is in two of them.
#[test]
fn m2_lists_name_fixtures_once() {
    let mut seen = BTreeMap::<String, &str>::new();
    let mut bad = Vec::new();
    for dir in ["rig_gaps.d", "oracle_gaps.d"] {
        for (id, _) in entries(dir) {
            if plan_text(&id).is_none() {
                bad.push(format!("{dir}/{id}.tsv: no plan fixture {id}"));
            }
            if let Some(o) = seen.insert(id.clone(), dir) {
                bad.push(format!("{id}: in {o} and {dir}"));
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn m2_seed_verdicts() {
    let rows = seed();
    let mut bad = Vec::new();
    for (id, src, want) in &rows {
        let text = plan_text(id).unwrap_or_else(|| panic!("{id}: no plan fixture"));
        let got = verdict(&text).unwrap_or_else(|| "not decomposed".to_owned());
        println!("m2 seed\t{id}\t{src}\t{got}");
        if &got != want {
            bad.push(format!("{id} ({src}): expected {want}, got {got}"));
        }
    }
    assert!(rows.len() >= 60, "{} seed rows", rows.len());
    assert!(bad.is_empty(), "{} seed rows:\n{}", bad.len(), bad.join("\n"));
}

/// Each served seed row's IR against `fixtures/m2/<fixture>.ir`; LOWERV4_PIN=write records it.
#[test]
fn m2_ir_pins() {
    let (dir, write) = (common::milestone_dir(2), std::env::var("LOWERV4_PIN").is_ok_and(|v| v == "write"));
    let (mut same, mut unpinned, mut diff) = (0, Vec::new(), Vec::new());
    for (id, _, _) in seed() {
        let Some(Ok(l)) = plan_text(&id).and_then(|t| common::lower_fixture(&t)) else { continue };
        let (ir, pin) = (common::pin_ir(&l), dir.join(format!("{id}.ir")));
        match std::fs::read_to_string(&pin) {
            _ if write => std::fs::write(&pin, &ir).unwrap_or_else(|e| panic!("{}: {e}", pin.display())),
            Ok(p) if p == ir => same += 1,
            Ok(_) => diff.push(id),
            Err(_) => unpinned.push(id),
        }
    }
    eprintln!("M2 IR pins: {same} same, {} differ, {} unpinned", diff.len(), unpinned.len());
    assert!(diff.is_empty(), "IR differs from the pin (LOWERV4_PIN=write re-records after review): {diff:?}");
}

/// Rows against the PG oracle: each `fixtures/m2/<name>.plan` with a `.pg` golden through gravity_driver in every mode
/// (straight, stop / resume, cancel, SCRIBBLE). A fixture corpus.d expects refused prints its refusal; a host-sited
/// statement is the server runner's (`m2 driver` lines feed `rig.py report --crate`).
#[test]
fn m2_rows_against_the_oracle() {
    let want: BTreeMap<String, String> = seed().into_iter().map(|(id, _, x)| (id, x)).collect();
    let oracle_gaps = entries("oracle_gaps.d");
    // a backend always has a session zone (the goldens' initdb: UTC); the hosted datetime calls read it
    pgtz::init_seams();
    xact_seams::get_current_sub_transaction_id::set(|| 1); // pg_tzset's zone cache reads it (hosts tests/v4_m2_tz.rs)
    pgtz::set_session_timezone(pgtz::pg_tzset_offset(0).expect("UTC offset zone"));
    let (mut ran, mut bad) = (0, Vec::new());
    for e in common::corpus(&common::milestone_dir(2)).into_iter().filter(|e| e.golden.is_some()) {
        for mode in common::MODES {
            let v = match common::run_fixture(&e.plan, mode) {
                None => "not decomposed".to_owned(),
                Some(Err(w)) if w.starts_with("refused: ") && want.get(&e.name).is_some_and(|x| x.starts_with("refused ")) => format!("refused: {}", &w[9..]),
                Some(Err(w)) if w.contains("host site") => format!("server runner: {w}"),
                Some(Err(w)) => {
                    bad.push(format!("{} {}: {w}", e.name, common::mode_name(mode)));
                    format!("FAILED: {w}")
                }
                Some(Ok(_)) if oracle_gaps.contains_key(&e.name) => "rig gap: reads the transaction start".to_owned(),
                Some(Ok(got)) => {
                    ran += 1;
                    common::check(&e, mode, &got).map_or_else(|m| { bad.push(m.clone()); format!("WRONG: {m}") }, |()| "ok".to_owned())
                }
            };
            println!("m2 driver\t{}\t{}\t{v}", e.name, common::mode_name(mode));
        }
    }
    eprintln!("m2 driver: {ran} runs compared with PostgreSQL");
    assert!(bad.is_empty(), "{} driver runs:\n{}", bad.len(), bad.join("\n"));
}
