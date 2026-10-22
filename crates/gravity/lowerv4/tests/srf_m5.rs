//! M5's rig (M5-plan §8, cards/M5.md L1 "tests/m5.rs"): function sources. The seed `fixtures/m5/corpus.d/<fixture>.tsv`
//! pins each seeded fixture's verdict by fixture name (`served`, `refused <text>`, or `server-only`) and a served row's
//! IR. Plans decompose with the server's member set (write_rig's `declared`, M4-RIG). A fixture the crate rig cannot run
//! (a PL / SQL body, a library, fmgr or table function: the rig has no user catalog and runs no host site, D537) is
//! judged by the server rig only; its seed row says `server-only`. The m5x fixtures (`fixtures/m5/m5x.sql`, captured by
//! scripts/gravity-capture-plans.sh m5x) carry the user SRFs the corpora lack: L0-k's build sides and G4's
//! does-not-apply shapes.
use gravity_lowerv4_it as common;

#[allow(dead_code)]
#[path = "common/write_rig.rs"]
mod write_rig;

use std::collections::BTreeMap;

use common::multiset;

fn sql_key(s: &str) -> String {
    s.trim().trim_end_matches(';').trim().to_owned()
}

fn plan_text(id: &str) -> Option<String> {
    let rel = id.strip_prefix("regress-full-").map_or_else(|| format!("{id}.plan"), |s| format!("regress-full/{s}.plan"));
    std::fs::read_to_string(common::plans_dir().join(rel)).ok().or_else(|| std::fs::read_to_string(common::milestone_dir(5).join(format!("{id}.plan"))).ok())
}

/// `tests/fixtures/m5/<dir>/*.tsv` as (stem, its non-comment rows); each dir's README says what an entry means.
fn entries(dir: &str) -> BTreeMap<String, Vec<Vec<String>>> {
    let d = common::milestone_dir(5).join(dir);
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
/// rig's); a served row's IR matches `fixtures/m5/<fixture>.ir` (LOWERV4_PIN=write records it).
#[test]
fn m5_seed_verdicts_and_pins() {
    let (rows, dir, write) = (seed(), common::milestone_dir(5), std::env::var("LOWERV4_PIN").is_ok_and(|v| v == "write"));
    let mut bad = Vec::new();
    for (id, src, want) in &rows {
        let text = plan_text(id);
        let got = text.as_deref().and_then(verdict).unwrap_or_else(|| "not decomposed".to_owned());
        println!("m5 seed\t{id}\t{src}\t{got}");
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

/// corpus.d's rows at M5-L1.
const MIN_SEED: usize = 34;

/// The m5x fixture of statement `sql` (fixtures/m5/m5x-<n>.plan, never a body's nested `.k` plan), found by its text.
fn m5x(sql: &str) -> String {
    let hits: Vec<(String, String)> = common::files(&common::milestone_dir(5), "plan").into_iter()
        .filter(|(n, t)| n.starts_with("m5x-") && !n.contains('.') && t.lines().any(|l| l.strip_prefix("-- sql: ").is_some_and(|q| sql_key(q) == sql_key(sql))))
        .collect();
    assert_eq!(hits.len(), 1, "m5x fixtures of {sql}: {:?}", hits.iter().map(|h| &h.0).collect::<Vec<_>>());
    hits.into_iter().next().expect("one").1
}

/// Whether the plan calls a user function (an OID past FirstNormalObjectId).
fn calls_user_fn(text: &str) -> bool {
    text.split(":funcid ").skip(1).filter_map(|x| x.split([' ', '}']).next()?.parse::<u32>().ok()).any(|f| f >= 16384)
}

/// L0-k's fixtures (M5-SERIES's join-side test reads them): a user SRF as the hash join's build side, plain (Hash over the
/// FunctionScan) and fed (Hash over an aggregate over it), with a plpgsql and a SQL body. D1005: both serve (a lazy build; the
/// fed one defers the aggregate with it). G4: a library SRF on the build
/// side and an inlined SQL SRF (a subquery, its body's generate_series) call no user function; the crate rig never
/// serves a user SRF (no user catalog, D537: the server rig judges it).
#[test]
fn m5_user_srf_fixtures() {
    let tree = |t: &str| t.split('{').skip(1).filter_map(|x| x.split([' ', '}']).next()).filter(|n| n.ends_with("SCAN") || ["HASH", "HASHJOIN", "AGG", "PROJECTSET"].contains(n)).collect::<Vec<_>>().join(" ");
    let mut bad = Vec::new();
    for (sql, nodes, user) in [
        ("SELECT t.a, t.b FROM t JOIN three() f(x) ON t.a = f.x", "HASHJOIN SEQSCAN HASH FUNCTIONSCAN", true),
        ("SELECT t.a FROM t JOIN (SELECT DISTINCT x FROM three() f(x)) s ON t.a = s.x", "HASHJOIN SEQSCAN HASH AGG FUNCTIONSCAN", true),
        ("SELECT t.a FROM t JOIN three_sql() f(x) ON t.a = f.x", "HASHJOIN SEQSCAN HASH FUNCTIONSCAN", true),
        ("SELECT t.a FROM t JOIN generate_series(1, 3) g(x) ON t.a = g.x", "HASHJOIN SEQSCAN HASH FUNCTIONSCAN", false),
        ("SELECT t.a FROM t JOIN upto(3) u(x) ON t.a = u.x", "HASHJOIN SEQSCAN HASH FUNCTIONSCAN", false),
        ("SELECT three(), a FROM t WHERE a < 3", "PROJECTSET INDEXONLYSCAN", true),
        ("SELECT x FROM three() f(x) WHERE x <> 2", "FUNCTIONSCAN", true),
    ] {
        let t = m5x(sql);
        let v = verdict(&t);
        println!("m5x\t{sql}\t{}\t{v:?}", tree(&t));
        if tree(&t) != nodes || calls_user_fn(&t) != user {
            bad.push(format!("{sql}: plan {} (want {nodes}), user fn {} (want {user}): re-capture m5x", tree(&t), calls_user_fn(&t)));
        }
        // M5-PL: a user SRF alone in FROM serves (it reads PG's own store, no user catalog needed); on a join build
        // side (L0-k, D762) it is D824 / D1005's lazy build (plain, or fed under an aggregate: its producers deferred with
        // it, the probe on the leader); in the target list (PSET) it refuses by name
        if user && (v.as_deref() == Some("served")) != !nodes.starts_with("PROJECTSET") {
            bad.push(format!("{sql}: a user SRF {v:?}: served alone in FROM (srf.pl) and on a join build side (a lazy build), refused by name in the target list"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// Rows against PostgreSQL 18.6: each `fixtures/m5/<id>.plan` with a `.pg` golden (iterator forms, no table: the crate has
/// no storage) runs through gravity_driver straight, under a stop / resume, a cancel and SCRIBBLE (common::MODES) and
/// compares by its order class. A named Shape refusal is not a wrong answer (served coverage is v5probe's count); a host
/// site (library / fmgr / PL / table function) is the server rig's.
#[test]
fn m5_rows_against_the_oracle() {
    let want: BTreeMap<String, String> = seed().into_iter().map(|(id, _, x)| (id, x)).collect();
    let host_raise = format!("Raised({})", gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST));
    pgtz::init_seams();
    xact_seams::get_current_sub_transaction_id::set(|| 1);
    pgtz::set_session_timezone(pgtz::pg_tzset_offset(0).expect("UTC offset zone"));
    let (mut ran, mut goldens, mut bad) = (0, 0, Vec::new());
    for e in common::corpus(&common::milestone_dir(5)).into_iter().filter(|e| e.golden.is_some()) {
        goldens += 1;
        for mode in common::MODES {
            let m = common::mode_name(mode);
            let v = match common::run_fixture(&e.plan, mode) {
                None => {
                    bad.push(format!("{}: decompose does not take it", e.name));
                    "not decomposed".to_owned()
                }
                // a named Shape refusal (the lowerer declines the shape): not a wrong answer
                Some(Err(w)) if w.starts_with("refused: lowerv4: Shape: ") => format!("refused: {}", &w[9..]),
                Some(Err(w)) if w.contains("host site") => format!("server rig: {w}"),
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
            println!("m5 driver\t{}\t{m}\t{v}", e.name);
        }
    }
    eprintln!("m5 driver: {goldens} PG goldens, {ran} runs compared");
    assert!(goldens >= MIN_GOLDENS, "{goldens} fixtures/m5 goldens");
    assert!(bad.is_empty(), "{} driver runs:\n{}", bad.len(), bad.join("\n"));
}

/// fixtures/m5's `.pg` goldens at M5-L1.
const MIN_GOLDENS: usize = 11;

