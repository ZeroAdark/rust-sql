//! M7's rig (M7-plan §8, cards/M7.md L1 "tests/m7.rs"): hash-join types, the drain, chains and build spill. The seed
//! `fixtures/m7/corpus.d/<fixture>.tsv` pins each seeded fixture's verdict by fixture name (`served`, `refused <text>` or
//! `server-only`) and a served row's IR. Plans decompose with the server's member set (write_rig's `declared`). The m7x
//! fixtures (`fixtures/m7/m7x.sql`, captured by scripts/gravity-capture-plans.sh m7x) carry table-free statements per
//! join type whose rows are checked against PostgreSQL 18.6; the scale pass list is the census's M7 TPC-H bar statements.
use gravity_lowerv4_it as common;

#[allow(dead_code)]
#[path = "common/write_rig.rs"]
mod write_rig;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use common::multiset::{self, Class, Verdict};

fn repo(rel: &str) -> PathBuf {
    manifest_dir().join("../../..").join(rel)
}

fn plan_text(id: &str) -> Option<String> {
    let rel = id.strip_prefix("regress-full-").map_or_else(|| format!("{id}.plan"), |s| format!("regress-full/{s}.plan"));
    std::fs::read_to_string(common::plans_dir().join(rel)).ok().or_else(|| std::fs::read_to_string(common::milestone_dir(7).join(format!("{id}.plan"))).ok())
}

/// `tests/fixtures/m7/<dir>/*.tsv` as (stem, its non-comment rows); each dir's README says what an entry means.
fn entries(dir: &str) -> BTreeMap<String, Vec<Vec<String>>> {
    let d = common::milestone_dir(7).join(dir);
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
/// rig's); a served row's IR matches `fixtures/m7/<fixture>.ir` (LOWERV4_PIN=write records it).
#[test]
fn m7_seed_verdicts_and_pins() {
    let (rows, dir, write) = (seed(), common::milestone_dir(7), std::env::var("LOWERV4_PIN").is_ok_and(|v| v == "write"));
    let mut bad = Vec::new();
    for (id, src, want) in &rows {
        let text = plan_text(id);
        let got = text.as_deref().and_then(verdict).unwrap_or_else(|| "not decomposed".to_owned());
        println!("m7 seed\t{id}\t{src}\t{got}");
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

/// corpus.d's rows at M7-L1.
const MIN_SEED: usize = 49;

/// Every join's `:join.jointype` in a plan (nodes/nodes.h JoinType: 0 INNER, 1 LEFT, 2 FULL, 3 RIGHT, 4 SEMI, 5 ANTI,
/// 6 RIGHT_SEMI, 7 RIGHT_ANTI), or none when the plan has no hash join.
fn hash_join_types(text: &str) -> Vec<u32> {
    if !text.contains("{HASHJOIN ") {
        return Vec::new();
    }
    text.split(":join.jointype ").skip(1).filter_map(|x| x.split([' ', '}']).next()?.parse().ok()).collect()
}

/// A seed row's m7x source names the join type its statement was written for (`m7x/<type>`).
const JOIN_TYPES: [(&str, u32); 7] = [("left", 1), ("full", 2), ("right", 3), ("semi", 4), ("anti", 5), ("right_semi", 6), ("right_anti", 7)];

/// Each m7x row seeded as `m7x/<type>` plans a hash join of that type (a re-capture whose planner picked another join
/// or type would test the wrong shape).
#[test]
fn m7_the_join_types() {
    let mut bad = Vec::new();
    let mut typed = 0;
    for (id, src, _) in seed() {
        let Some(want) = src.strip_prefix("m7x/").and_then(|w| JOIN_TYPES.iter().find(|j| j.0 == w)) else { continue };
        typed += 1;
        let got = plan_text(&id).map(|t| hash_join_types(&t)).unwrap_or_default();
        println!("m7x type\t{id}\t{}\t{got:?}", want.0);
        if !got.contains(&want.1) {
            bad.push(format!("{id}: seeded as {src} but its hash joins are {got:?} (want jointype {}): re-capture m7x", want.1));
        }
    }
    assert!(typed >= JOIN_TYPES.len(), "{typed} m7x rows name a join type");
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// The per-level SF10 scale pass (M7-plan §8 "scale from day one"; M7-RIG runs it at W0 / W8 / W16 on both storages after
/// each M7 level): the census's M7 TPC-H bar statements. A census move that adds or removes one changes the pass.
#[test]
fn m7_scale_pass_list() {
    let t = std::fs::read_to_string(repo("crates/gravity/lowerv4/testdata/gravity/lowerv4/census/serve.tsv")).expect("serve.tsv");
    let got: BTreeSet<String> = t.lines().filter_map(|l| {
        let f: Vec<&str> = l.split('\t').collect();
        let rest = f.first()?.strip_prefix("bar-tpch-")?;
        let (storage, q) = rest.split_once("-w0-")?;
        let n = q.parse::<u32>().ok()?;
        (f.get(1) == Some(&"M7")).then(|| format!("{storage} TH{n:02}"))
    }).collect();
    let want: BTreeSet<String> = SCALE_PASS.iter().map(|s| s.to_string()).collect();
    println!("m7 scale pass\t{}", got.iter().cloned().collect::<Vec<_>>().join(", "));
    assert_eq!(got, want, "the census's M7 TPC-H bar statements moved: update SCALE_PASS and the scale pass (M7-plan §8)");
}

/// M7-plan §2: the 15 TPC-H bar statements (heap 5, pgrc 10).
const SCALE_PASS: [&str; 15] = [
    "heap TH04", "heap TH05", "heap TH07", "heap TH13", "heap TH18",
    "pgrc TH04", "pgrc TH05", "pgrc TH07", "pgrc TH08", "pgrc TH09", "pgrc TH10", "pgrc TH13", "pgrc TH18", "pgrc TH20", "pgrc TH21",
];

/// Rows against PostgreSQL 18.6: each `fixtures/m7/<id>.plan` with a `.pg` golden (VALUES / generate_series joins, no
/// table: the crate has no storage) runs through gravity_driver straight, under a stop / resume, a cancel and SCRIBBLE
/// (common::MODES) and compares by its order class. A named Shape refusal is not a wrong answer (served coverage is
/// v5probe's count); a host site (library / fmgr / PL / table function) is the server rig's.
#[test]
fn m7_rows_against_the_oracle() {
    seams();
    let (mut ran, mut goldens, mut bad) = (0, 0, Vec::new());
    for e in common::corpus(&common::milestone_dir(7)).into_iter().filter(|e| e.golden.is_some()) {
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
                Some(Err(w)) => {
                    bad.push(format!("{} {m}: {w}", e.name));
                    format!("FAILED: {w}")
                }
                Some(Ok(got)) => {
                    ran += 1;
                    common::check(&e, mode, &got).map_or_else(|x| { bad.push(x.clone()); format!("WRONG: {x}") }, |()| "ok".to_owned())
                }
            };
            println!("m7 driver\t{}\t{m}\t{v}", e.name);
        }
    }
    eprintln!("m7 driver: {goldens} PG goldens, {ran} runs compared");
    assert!(goldens >= MIN_GOLDENS, "{goldens} fixtures/m7 goldens");
    assert!(bad.is_empty(), "{} driver runs:\n{}", bad.len(), bad.join("\n"));
}

/// D1002: the rig finishes a hash build as the server's width-1 producer does (common/drive.rs), so a probe over a
/// non-empty build matches. m7x-008 as an INNER join (join.inner, built at M1; the fixture's `:join.jointype` 1 -> 0) must
/// return exactly its LEFT golden's matched rows (`b.y` non-NULL: 12 of 44), under every mode; without the finish it
/// returned none.
#[test]
fn m7_rig_finishes_a_hash_build() {
    seams();
    let e = common::corpus(&common::milestone_dir(7)).into_iter().find(|e| e.name == "m7x-008").expect("fixtures/m7/m7x-008");
    let g = e.golden.as_ref().expect("m7x-008.pg");
    let common::multiset::Outcome::Rows(left) = &g.pg else { panic!("m7x-008.pg is an error") };
    let matched: Vec<_> = left.iter().filter(|r| r[1].is_some()).cloned().collect();
    assert_eq!(matched.len(), 12, "m7x-008.pg's matched rows");
    let want = common::multiset::Outcome::Rows(matched);
    assert_eq!(e.plan.matches(":join.jointype 1 ").count(), 1, "m7x-008 is one LEFT hash join");
    let inner = e.plan.replacen(":join.jointype 1 ", ":join.jointype 0 ", 1);
    for mode in common::MODES {
        let got = match common::run_fixture(&inner, mode) {
            Some(Ok(o)) => o,
            other => panic!("m7x-008 as INNER, {}: {other:?}", common::mode_name(mode)),
        };
        if matches!(mode, common::Mode::Cancel(_)) && got == common::cancel_error() {
            continue;
        }
        let v = multiset::compare(&Class::Unordered, &want, None, &got, &Default::default());
        assert_eq!(v, Verdict::Match, "m7x-008 as INNER, {}: {got:?} is not the golden's 12 matched rows", common::mode_name(mode));
    }
}

/// The seams the driver runs need, installed once per test process (a seam installed twice panics, and the tests that
/// run fixtures share the process).
fn seams() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        pgtz::init_seams();
        xact_seams::get_current_sub_transaction_id::set(|| 1);
        pgtz::set_session_timezone(pgtz::pg_tzset_offset(0).expect("UTC offset zone"));
    });
}

/// fixtures/m7's `.pg` goldens at M7-L1 (m7x-008 .. m7x-022, the table-free rows).
const MIN_GOLDENS: usize = 15;

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(|d| std::path::Path::new(&d).with_file_name("lowerv4")).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
