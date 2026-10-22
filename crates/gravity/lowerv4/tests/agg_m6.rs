//! M6's rig (M6-plan §8, cards/M6.md L1 "tests/m6.rs"): varlena keys and values. The seed
//! `fixtures/m6/corpus.d/<fixture>.tsv` pins each seeded fixture's verdict by fixture name (`served`, `refused <text>`
//! or `server-only`) and a served row's IR. Plans decompose with the server's member set (write_rig's `declared`). A
//! fixture the crate rig cannot run (a user aggregate, type or function: the rig has no user catalog and runs no host
//! site, D537) is judged by the server rig only; its seed row says `server-only`. The m6x
//! fixtures (`fixtures/m6/m6x.sql`, captured by scripts/gravity-capture-plans.sh m6x) carry what the corpora lack: G4's
//! does-not-apply shapes, the collation matrix (C / the default / libc en_US / ICU / nondeterministic) and table-free
//! statements per kind whose rows are checked against PostgreSQL 18.6.
use gravity_lowerv4_it as common;

#[allow(dead_code)]
#[path = "common/write_rig.rs"]
mod write_rig;

use std::collections::{BTreeMap, BTreeSet};

use common::multiset;

fn sql_key(s: &str) -> String {
    s.trim().trim_end_matches(';').trim().to_owned()
}

fn plan_text(id: &str) -> Option<String> {
    let rel = id.strip_prefix("regress-full-").map_or_else(|| format!("{id}.plan"), |s| format!("regress-full/{s}.plan"));
    std::fs::read_to_string(common::plans_dir().join(rel)).ok().or_else(|| std::fs::read_to_string(common::milestone_dir(6).join(format!("{id}.plan"))).ok())
}

/// `tests/fixtures/m6/<dir>/*.tsv` as (stem, its non-comment rows); each dir's README says what an entry means.
fn entries(dir: &str) -> BTreeMap<String, Vec<Vec<String>>> {
    let d = common::milestone_dir(6).join(dir);
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

/// The process's fingerprint seed, as the server installs it (hosts text_hosts::fp_seed: postmaster start, each Backend),
/// first in every test that lowers or runs: a condition's fingerprint (verdict.cond) and a varlena key's Fp128 read it,
/// and a once-set seed installed by one test alone would make the others' IR depend on the order the tests run in.
fn fp_seed() {
    let _ = gravity_dict::fp::install(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
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
/// rig's); a served row's IR matches `fixtures/m6/<fixture>.ir` (LOWERV4_PIN=write records it).
#[test]
fn m6_seed_verdicts_and_pins() {
    fp_seed();
    let (rows, dir, write) = (seed(), common::milestone_dir(6), std::env::var("LOWERV4_PIN").is_ok_and(|v| v == "write"));
    let mut bad = Vec::new();
    for (id, src, want) in &rows {
        let text = plan_text(id);
        let got = text.as_deref().and_then(verdict).unwrap_or_else(|| "not decomposed".to_owned());
        println!("m6 seed\t{id}\t{src}\t{got}");
        if want != "server-only" && got != *want {
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

/// corpus.d's rows at M6-L1.
const MIN_SEED: usize = 108;

/// The m6x fixture of statement `sql` (fixtures/m6/m6x-<n>.plan, never a body's nested `.k` plan), found by its text.
fn m6x(sql: &str) -> String {
    let hits: Vec<(String, String)> = common::files(&common::milestone_dir(6), "plan").into_iter()
        .filter(|(n, t)| n.starts_with("m6x-") && !n.contains('.') && t.lines().any(|l| l.strip_prefix("-- sql: ").is_some_and(|q| sql_key(q) == sql_key(sql))))
        .collect();
    assert_eq!(hits.len(), 1, "m6x fixtures of {sql}: {:?}", hits.iter().map(|h| &h.0).collect::<Vec<_>>());
    hits.into_iter().next().expect("one").1
}

/// Every collation oid a plan names (expression / Sort / Agg / hash-key collations; 0 = none).
fn collations(text: &str) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    for tag in [":inputcollid ", ":varcollid ", ":collid ", ":collations (", ":grpCollations (", ":hashcollations ("] {
        for x in text.split(tag).skip(1) {
            out.extend(x.split(['}', ')', ':']).next().unwrap_or("").split(' ').filter_map(|n| n.parse::<u32>().ok()).filter(|&n| n != 0));
        }
    }
    out
}

/// The collation matrix (M6-plan §8, Decision 17): one text key under "C" (bytes), the database default (100: the
/// capture's --no-locale database is C, the oid is the database's), libc "en_US" and ICU "und-x-icu" (initdb's imports:
/// deterministic, not bytes order) and the nondeterministic `ci` (a user collation: equal is not bytes-equal): each plan
/// carries the collation it was written with (a re-capture that loses it would test C five times). The verdicts are
/// corpus.d's (`m6x/coll-*` rows).
#[test]
fn m6_the_collation_matrix() {
    let mut bad = Vec::new();
    let (c, default, user) = (|o: &u32| *o == 950, |o: &u32| *o == 100, |o: &u32| *o >= 16384);
    let initdb = |o: &u32| *o != 100 && *o != 950 && *o != 951 && *o < 16384;
    let coll: [(&str, &dyn Fn(&u32) -> bool); 5] = [("\"C\"", &c), ("", &default), ("\"en_US\"", &initdb), ("\"und-x-icu\"", &initdb), ("ci", &user)];
    for sql in ["SELECT t{c}, count(*) FROM tt GROUP BY {k}", "SELECT k, t FROM tt ORDER BY t{c}, k"] {
        for (name, class) in &coll {
            let q = sql.replace("{c}", &if name.is_empty() { String::new() } else { format!(" COLLATE {name}") }).replace("{k}", if name.is_empty() { "t" } else { "1" });
            let t = m6x(&q);
            let cs = collations(&t);
            println!("m6x coll\t{q}\t{cs:?}\t{:?}", verdict(&t));
            if !cs.iter().any(|o| class(o)) || cs.iter().any(|o| *o != 100 && !class(o)) {
                bad.push(format!("{q}: the plan's collations {cs:?} are not {name}'s: re-capture m6x"));
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// Rows against PostgreSQL 18.6: each `fixtures/m6/<id>.plan` with a `.pg` golden (iterator forms, no table: the crate has
/// no storage) runs through gravity_driver straight, under a stop / resume, a cancel and SCRIBBLE (common::MODES) and
/// compares by its order class. A named Shape refusal is not a wrong answer (served coverage is v5probe's count); a host
/// site (library / fmgr / PL / table function) is the server rig's.
#[test]
fn m6_rows_against_the_oracle() {
    fp_seed();
    let want: BTreeMap<String, String> = seed().into_iter().map(|(id, _, x)| (id, x)).collect();
    let gaps = entries("rig_gaps.d"); // a fixture the crate rig cannot run (a locale's comparator): the server runner's
    let host_raise = format!("Raised({})", gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST));
    pgtz::init_seams();
    xact_seams::get_current_sub_transaction_id::set(|| 1);
    // a text function's character count (length(t): varlena::text_length) reads the database encoding, the server's UTF8
    // (mbutils installs both there); the golden fixtures' text is valid UTF-8
    mbutils_seams::pg_database_encoding_max_length::set(|| 4);
    mbutils_seams::pg_mbstrlen_with_len::set(|s| Ok(String::from_utf8_lossy(s).chars().count() as i32));
    pgtz::set_session_timezone(pgtz::pg_tzset_offset(0).expect("UTC offset zone"));
    let (mut ran, mut goldens, mut bad) = (0, 0, Vec::new());
    for e in common::corpus(&common::milestone_dir(6)).into_iter().filter(|e| e.golden.is_some()) {
        goldens += 1;
        if let Some(g) = gaps.get(&e.name) {
            println!("m6 driver\t{}\t-\tserver rig (rig_gaps.d): {}", e.name, g[0][0]);
            continue;
        }
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
                Some(Err(w)) => {
                    bad.push(format!("{} {m}: {w}", e.name));
                    format!("FAILED: {w}")
                }
                Some(Ok(got)) => {
                    ran += 1;
                    common::check(&e, mode, &got).map_or_else(|x| { bad.push(x.clone()); format!("WRONG: {x}") }, |()| "ok".to_owned())
                }
            };
            println!("m6 driver\t{}\t{m}\t{v}", e.name);
        }
    }
    eprintln!("m6 driver: {goldens} PG goldens, {ran} runs compared");
    assert!(goldens >= MIN_GOLDENS, "{goldens} fixtures/m6 goldens");
    assert!(bad.is_empty(), "{} driver runs:\n{}", bad.len(), bad.join("\n"));
}

/// fixtures/m6's `.pg` goldens at M6-L1.
const MIN_GOLDENS: usize = 18;

