//! M8's rig (M8-plan §8, cards/M8.md L1 "tests/m8.rs"): nests and subqueries. The seed
//! `fixtures/m8/corpus.d/<fixture>.tsv` pins each seeded fixture's verdict by fixture name (`served`, `refused <text>`
//! or `server-only`) and a served row's IR. Plans decompose with the server's member set (write_rig's `declared`). The m8x
//! fixtures (`fixtures/m8/m8x.sql`, captured by scripts/gravity-capture-plans.sh m8x) carry what the corpora lack:
//! table-free statements per shape (InitPlan / SubPlan links, lateral loops, a Material inner, a CTE, a rerun set, a
//! conditional SubPlan) whose rows are checked against PostgreSQL 18.6.
use gravity_lowerv4_it as common;

#[allow(dead_code)]
#[path = "common/write_rig.rs"]
mod write_rig;

use std::collections::BTreeMap;

use common::multiset;

fn plan_text(id: &str) -> Option<String> {
    let rel = id.strip_prefix("regress-full-").map_or_else(|| format!("{id}.plan"), |s| format!("regress-full/{s}.plan"));
    std::fs::read_to_string(common::plans_dir().join(rel)).ok().or_else(|| std::fs::read_to_string(common::milestone_dir(8).join(format!("{id}.plan"))).ok())
}

/// `tests/fixtures/m8/<dir>/*.tsv` as (stem, its non-comment rows); each dir's README says what an entry means.
fn entries(dir: &str) -> BTreeMap<String, Vec<Vec<String>>> {
    let d = common::milestone_dir(8).join(dir);
    let mut out = BTreeMap::new();
    for e in std::fs::read_dir(&d).unwrap_or_else(|e| panic!("{}: {e}", d.display())) {
        let p = e.expect("dir entry").path();
        let Some(id) = p.file_name().and_then(|f| f.to_str()).and_then(|f| f.strip_suffix(".tsv")) else { continue };
        let t = std::fs::read_to_string(&p).expect("entry file");
        out.insert(id.to_owned(), t.lines().filter(|l| !l.starts_with('#') && !l.is_empty()).map(|l| l.split('\t').map(str::to_owned).collect()).collect());
    }
    out
}

fn verdict(text: &str) -> Option<String> {
    Some(match write_rig::lower_declared(text)? {
        Ok(_) => "served".to_owned(),
        Err(e) => format!("refused {e}"),
    })
}

/// The seed: (fixture, source, expect), from corpus.d; column 1 names the file.
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

/// Each seed row's verdict as corpus.d pins it (`server-only`, D537: a fixture the crate rig cannot judge, the server
/// rig's); `rig-gap` is a fixture with a `rig_gaps.d` entry that refuses a Shape or a missing seam in the crate rig (so a
/// real Bug never reads as expected); a served row's IR matches `fixtures/m8/<fixture>.ir` (LOWERV4_PIN=write records
/// it).
#[test]
fn m8_seed_verdicts_and_pins() {
    let (rows, dir, write) = (seed(), common::milestone_dir(8), std::env::var("LOWERV4_PIN").is_ok_and(|v| v == "write"));
    let (mut bad, gaps) = (Vec::new(), entries("rig_gaps.d"));
    for (id, src, want) in &rows {
        let text = plan_text(id);
        let got = text.as_deref().and_then(verdict).unwrap_or_else(|| "not decomposed".to_owned());
        println!("m8 seed\t{id}\t{src}\t{got}");
        if want == "rig-gap" {
            let gap = got.starts_with("refused lowerv4: Shape: ") || got.starts_with("refused lowerv4: Bug: seam not installed");
            if !gaps.contains_key(id.as_str()) || !gap {
                bad.push(format!("{id} ({src}): rig-gap needs a rig_gaps.d entry and a Shape or missing-seam refusal, got {got}"));
            }
        } else if want != "server-only" && got != *want {
            bad.push(format!("{id} ({src}): expected {want}, got {got}"));
        }
        let Some(Ok(ir)) = text.and_then(|t| write_rig::lower_declared(&t)) else { continue };
        let pin = dir.join(format!("{id}.ir"));
        match std::fs::read_to_string(&pin) {
            _ if write => std::fs::write(&pin, &ir).unwrap_or_else(|e| panic!("{}: {e}", pin.display())),
            Ok(p) if p == ir => {}
            _ => bad.push(format!("{id}: IR differs from (or has no) pin {} (LOWERV4_PIN=write re-records after review)", pin.display())),
        }
    }
    assert!(rows.len() >= MIN_SEED, "{} seed rows", rows.len());
    assert!(bad.is_empty(), "{} seed rows:\n{}", bad.len(), bad.join("\n"));
}

/// corpus.d's rows at M8-L1 (32 m8x rows + 57 census rows).
const MIN_SEED: usize = 89;

/// Rows against PostgreSQL 18.6: each `fixtures/m8/<id>.plan` with a `.pg` golden (iterator forms, no table: the crate has
/// no storage) runs through gravity_driver straight, under a stop / resume, a cancel and SCRIBBLE (common::MODES) and
/// compares by its order class. A named Shape refusal is not a wrong answer (served coverage is v5probe's count); a host
/// site (library / fmgr / PL / table function) is the server rig's.
#[test]
fn m8_rows_against_the_oracle() {
    let want: BTreeMap<String, String> = seed().into_iter().map(|(id, _, x)| (id, x)).collect();
    let host_raise = format!("Raised({})", gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST));
    pgtz::init_seams();
    xact_seams::get_current_sub_transaction_id::set(|| 1);
    pgtz::set_session_timezone(pgtz::pg_tzset_offset(0).expect("UTC offset zone"));
    let (mut ran, mut goldens, mut bad) = (0, 0, Vec::new());
    for e in common::corpus(&common::milestone_dir(8)).into_iter().filter(|e| e.golden.is_some()) {
        goldens += 1;
        for mode in common::MODES {
            let m = common::mode_name(mode);
            let v = match common::run_fixture(&e.plan, mode) {
                None => {
                    bad.push(format!("{}: decompose does not take it", e.name));
                    "not decomposed".to_owned()
                }
                Some(Err(w)) if w.contains("host site") => format!("server rig: {w}"),
                // a named Shape refusal (the lowerer declines the shape; served coverage is v5probe's count): not a wrong answer
                Some(Err(w)) if w.starts_with("refused: lowerv4: Shape: ") => format!("refused: {}", &w[9..]),
                // a server-only seed row's host raise (raise::HOST: the crate's fmgr host stashes no error): the server rig's
                Some(Ok(multiset::Outcome::Error { message, .. }))
                    if message == host_raise && want.get(&e.name).is_some_and(|w| w == "server-only") => format!("server rig: {message}"),
                // a served row whose output type common's renderer has no arm for (an array, ..): its lowering is pinned by
                // m8_seed_verdicts_and_pins, its rows are the server rig's (v4 kept it as claimed debt, D1257)
                Some(Err(w)) if w.starts_with("out type ") && want.get(&e.name).is_some_and(|w| w == "served") => format!("server rig: {w}"),
                Some(Err(w)) => {
                    bad.push(format!("{} {m}: {w}", e.name));
                    format!("FAILED: {w}")
                }
                Some(Ok(got)) => {
                    ran += 1;
                    common::check(&e, mode, &got).map_or_else(|x| { bad.push(x.clone()); format!("WRONG: {x}") }, |()| "ok".to_owned())
                }
            };
            println!("m8 driver\t{}\t{m}\t{v}", e.name);
        }
    }
    eprintln!("m8 driver: {goldens} PG goldens, {ran} runs compared");
    assert!(goldens >= MIN_GOLDENS, "{goldens} fixtures/m8 goldens");
    assert!(bad.is_empty(), "{} driver runs:\n{}", bad.len(), bad.join("\n"));
}

/// fixtures/m8's `.pg` goldens at M8-L1.
const MIN_GOLDENS: usize = 33;

