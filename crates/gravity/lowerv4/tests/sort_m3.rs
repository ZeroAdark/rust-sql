//! M3's rig (M3-plan.md §8, cards/M3.md L1 "tests/m3.rs"): the M3 corpus lowered through lowerv4 and held to what the
//! crate alone decides. The server runner (scripts/gravity-v4/rig.py) runs the host-sited statements against PG.
//! - The seed corpus `tests/fixtures/m3/corpus.d/<fixture>.tsv` (a file per fixture) pins each fixture's verdict by
//!   fixture name: `served`, or `refused <text>`.
//! - Each seed row that serves is pinned by IR (`<fixture>.ir`; LOWERV4_PIN=write records it). Rows with a PG golden
//!   (`<fixture>.plan` + `.pg` in fixtures/m3) run through gravity_driver straight and under a stop, cancel and SCRIBBLE,
//!   compared by the one comparator: order exact where the SQL fixes it, a multiset over the ties it leaves open, float
//!   last digits within V4R-2's tolerance, an error against a value WRONG.
use gravity_lowerv4_it as common;

use std::collections::BTreeMap;

use common::multiset::{self, Class, Outcome, Verdict};

/// A census id's decompose plan fixture: `plans/<id>.plan`, or `plans/regress-full/<stem>.plan`; `None` when the capture
/// kept no fixture (e2e ids, bar-* ids, oversize plans).
fn plan_text(id: &str) -> Option<String> {
    let rel = id.strip_prefix("regress-full-").map_or_else(|| format!("{id}.plan"), |s| format!("regress-full/{s}.plan"));
    std::fs::read_to_string(common::plans_dir().join(rel)).ok()
}

/// One fixture's verdict in the corpus.d form: `served` or the refusal text.
fn verdict(text: &str) -> Option<String> {
    Some(match common::lower_fixture(text)? {
        Ok(_) => "served".to_owned(),
        Err(e) => format!("refused {e}"),
    })
}

/// Every `*.tsv` under `tests/fixtures/m3/<dir>` as (stem, its non-comment lines split on tabs); each dir's README says
/// what an entry means (the user 09-24: lists several lanes append to are directories).
/// - `corpus.d/<fixture>.tsv`: the seed row `fixture  source  expect` of one fixture.
/// - `rig_gaps.d/<fixture>.tsv`: every need built, but the rig catalog (the reference `.dat` files) cannot answer it.
/// - `oracle_gaps.d/<fixture>.tsv`: goldens whose rows read state the crate has no source for.
fn entries(dir: &str) -> BTreeMap<String, Vec<Vec<String>>> {
    let d = common::milestone_dir(3).join(dir);
    let mut out = BTreeMap::new();
    for e in std::fs::read_dir(&d).unwrap_or_else(|e| panic!("{}: {e}", d.display())) {
        let p = e.expect("dir entry").path();
        let Some(id) = p.file_name().and_then(|f| f.to_str()).and_then(|f| f.strip_suffix(".tsv")) else { continue };
        let t = std::fs::read_to_string(&p).expect("entry file");
        let rows = t.lines().filter(|l| !l.starts_with('#') && !l.is_empty()).map(|l| l.split('\t').map(str::to_owned).collect());
        out.insert(id.to_owned(), rows.collect());
    }
    out
}

/// The seed: (fixture, source, expect); column 1 names the file.
fn seed() -> Vec<(String, String, String)> {
    let mut rows = Vec::new();
    for (name, lines) in entries("corpus.d") {
        for f in lines {
            assert_eq!(f.len(), 3, "corpus.d/{name}.tsv row {f:?}");
            assert_eq!(f[0], name, "corpus.d/{name}.tsv: column 1 names the file");
            rows.push((f[0].clone(), f[1].clone(), f[2].clone()));
        }
    }
    rows
}

/// Every entry of the two lists names a plan fixture, and no fixture is in two of them.
#[test]
fn m3_lists_name_fixtures_once() {
    let mut seen = BTreeMap::<String, String>::new();
    let mut bad = Vec::new();
    for dir in ["rig_gaps.d", "oracle_gaps.d"] {
        for (id, _) in entries(dir) {
            if plan_text(&id).is_none() && !common::milestone_dir(3).join(format!("{id}.plan")).is_file() {
                bad.push(format!("{dir}/{id}.tsv: no plan fixture {id}"));
            }
            if let Some(o) = seen.insert(id.clone(), dir.to_owned()) {
                bad.push(format!("{id}: in {o} and {dir}"));
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn m3_seed_verdicts() {
    let rows = seed();
    let mut bad = Vec::new();
    for (id, src, want) in &rows {
        let text = plan_text(id).or_else(|| std::fs::read_to_string(common::milestone_dir(3).join(format!("{id}.plan"))).ok());
        let got = text.as_deref().and_then(verdict).unwrap_or_else(|| "not decomposed".to_owned());
        println!("m3 seed\t{id}\t{src}\t{got}");
        if &got != want {
            bad.push(format!("{id} ({src}): expected {want}, got {got}"));
        }
    }
    assert!(rows.len() >= 60, "{} seed rows", rows.len());
    assert!(bad.is_empty(), "{} seed rows:\n{}", bad.len(), bad.join("\n"));
}

/// Each served seed row's IR against `fixtures/m3/<fixture>.ir`; LOWERV4_PIN=write records it.
#[test]
fn m3_ir_pins() {
    let (dir, write) = (common::milestone_dir(3), std::env::var("LOWERV4_PIN").is_ok_and(|v| v == "write"));
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
    eprintln!("M3 IR pins: {same} same, {} differ, {} unpinned", diff.len(), unpinned.len());
    assert!(diff.is_empty(), "IR differs from the pin (LOWERV4_PIN=write re-records after review): {diff:?}");
}

/// A golden's outcome through the driver, as the line the server runner's report reads; a failure goes to `bad`.
fn judge(e: &common::Entry, mode: common::Mode, bad: &mut Vec<String>) -> (String, Option<Outcome>) {
    let m = common::mode_name(mode);
    match common::run_fixture(&e.plan, mode) {
        None => ("not decomposed".to_owned(), None),
        // a named Shape refusal (the lowerer declines the shape; served coverage is v5probe's count): not a wrong answer
        Some(Err(w)) if w.starts_with("refused: lowerv4: Shape: ") => (format!("refused: {}", &w[9..]), None),
        Some(Err(w)) if w.contains("host site") => (format!("server runner: {w}"), None),
        Some(Err(w)) => {
            bad.push(format!("{} {m}: {w}", e.name));
            (format!("FAILED: {w}"), None)
        }
        Some(Ok(got)) => {
            let v = common::check(e, mode, &got).map_or_else(|x| { bad.push(x.clone()); format!("WRONG: {x}") }, |()| "ok".to_owned());
            (v, Some(got))
        }
    }
}

/// The process's seams and session zone, once (every driver test of this binary reads them; a seam is installed once).
fn seams() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        pgtz::init_seams();
        xact_seams::get_current_sub_transaction_id::set(|| 1);
    });
    pgtz::set_session_timezone(pgtz::pg_tzset_offset(0).expect("UTC offset zone"));
}

/// Rows against the PG oracle: each `fixtures/m3/<name>.plan` with a `.pg` golden through gravity_driver in every mode
/// (straight, stop / resume, cancel, SCRIBBLE), compared by its order class (the tie rule: `m3_tie_order_rule`).
#[test]
fn m3_rows_against_the_oracle() {
    let oracle_gaps = entries("oracle_gaps.d");
    seams();
    let (mut ran, mut bad) = (0, Vec::new());
    for e in common::corpus(&common::milestone_dir(3)).into_iter().filter(|e| e.golden.is_some()) {
        for mode in common::MODES {
            let (v, got) = judge(&e, mode, &mut bad);
            let v = if got.is_some() && oracle_gaps.contains_key(&e.name) { "rig gap: reads the transaction start".to_owned() } else { v };
            ran += usize::from(got.is_some());
            println!("m3 driver\t{}\t{}\t{v}", e.name, common::mode_name(mode));
        }
    }
    eprintln!("m3 driver: {ran} runs compared with PostgreSQL");
    assert!(bad.is_empty(), "{} driver runs:\n{}", bad.len(), bad.join("\n"));
}

/// D4620: regress groupingsets-041 (ROLLUP over a VALUES list) and -089 (DISTINCT over rollup x rollup) with road
/// agg.gsets.cascade named on (`common::ROADS_ON`, as `+agg.gsets.cascade` on a server), through the driver in every mode:
/// no run fails. Under the cascade the base's and each set's groups are owner regions, which the server runner's width-1
/// finish lays (exec `finish_single`) and this rig does not, so the cascaded set's fold asks the server's section as the
/// base's emit does (Unsupported: the server runner's statement, judged there) where it raised a bare error before.
#[test]
fn d4620_cascade_runs_against_the_oracle() {
    seams();
    let names = ["regress-full-groupingsets-041", "regress-full-groupingsets-089"];
    common::ROADS_ON.with(|r| *r.borrow_mut() = vec![gravity_decompose::ops::shared::agg::GSETS_CASCADE_ROAD]);
    let (mut seen, mut bad) = (0, Vec::new());
    for e in common::corpus(&common::milestone_dir(3)).into_iter().filter(|e| e.golden.is_some() && names.contains(&e.name.as_str())) {
        assert!(common::cascades_of(&e.plan).is_some_and(|n| n > 0), "{}: the road folds the coarser sets from the finer", e.name);
        for mode in common::MODES {
            let (v, _) = judge(&e, mode, &mut bad);
            println!("d4620 driver\t{}\t{}\t{v}", e.name, common::mode_name(mode));
        }
        seen += 1;
    }
    common::ROADS_ON.with(|r| r.borrow_mut().clear());
    assert_eq!(seen, names.len(), "every witness fixture is in the corpus with its golden");
    assert!(bad.is_empty(), "{} driver runs:\n{}", bad.len(), bad.join("\n"));
}

/// The tie-order gate (M3-plan §8; the 09-25 multiset ruling), on the comparator every M3 golden goes through: order is
/// exact on the keys the SQL orders by, the rows inside a tie group are a multiset, a LIMIT that cuts a tie group may keep
/// any of its members (PG's rows without the LIMIT decide), and an error against a value is WRONG.
#[test]
fn m3_tie_order_rule() {
    let row = |a: &str, b: &str| vec![Some(a.to_owned()), Some(b.to_owned())];
    let rows = |r: &[(&str, &str)]| Outcome::Rows(r.iter().map(|x| row(x.0, x.1)).collect());
    let cols = ["a".to_owned(), "b".to_owned()];
    let ordered = multiset::classify("SELECT a, b FROM t ORDER BY a", &cols);
    assert_eq!(ordered, Class::Ordered(vec![0]));
    let pg = rows(&[("1", "x"), ("1", "y"), ("2", "z")]);
    let tol = multiset::Tol::new();
    let cmp = |c: &Class, got: &Outcome, full: Option<&[multiset::Row]>| multiset::compare(c, &pg, full, got, &tol);
    assert_eq!(cmp(&ordered, &rows(&[("1", "y"), ("1", "x"), ("2", "z")]), None), Verdict::Match, "ties are a multiset");
    assert_eq!(cmp(&ordered, &rows(&[("2", "z"), ("1", "x"), ("1", "y")]), None), Verdict::MismatchOrder, "keys are exact");
    assert_eq!(cmp(&ordered, &rows(&[("1", "x"), ("1", "x"), ("2", "z")]), None), Verdict::Mismatch, "a tie group's rows");
    let limited = multiset::classify("SELECT a, b FROM t ORDER BY a LIMIT 3", &cols);
    assert_eq!(limited, Class::Limited(vec![0]));
    let full = [row("1", "x"), row("1", "y"), row("2", "z"), row("2", "w")];
    assert_eq!(cmp(&limited, &rows(&[("1", "y"), ("1", "x"), ("2", "w")]), Some(&full)), Verdict::Match, "a cut tie group");
    assert_ne!(cmp(&limited, &rows(&[("1", "x"), ("2", "z"), ("2", "w")]), Some(&full)), Verdict::Match, "the cut keeps the lower keys");
    let err = Outcome::Error { sqlstate: "22012".into(), message: "division by zero".into() };
    assert_eq!(multiset::compare(&ordered, &err, None, &pg, &tol), Verdict::Mismatch, "error against a value");
}

/// D564: C12h's `j->>'k' IN (..)` (bench-098) decodes its array once per call into frame words (host
/// `jsonb_field_any_decode` at the pipeline's entry) and compares each row's value in IR; the per-row host walk
/// (`jsonb_field_any`) stays only as the fallback an undecoded array takes.
#[test]
fn d564_in_list_decodes_once_per_call() {
    let text = plan_text("bench-098").expect("bench-098's plan");
    let l = common::lower_fixture(&text).expect("decomposes").expect("bench-098 serves");
    let ir = gravity_ir::text::print_program(&l.ir);
    assert!(ir.contains("(host jsonb_field_any_decode "), "the array decoded once per call");
    assert!(ir.contains("(call_host jsonb_field_any_decode "), "called at the entry");
}
