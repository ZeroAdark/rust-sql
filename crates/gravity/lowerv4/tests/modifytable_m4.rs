//! M4's rig (M4-plan §8, cards/M4.md L1 "tests/m4.rs"). The crate executes writes (risk 8, M4-RIG): each write fixture
//! `tests/fixtures/m4/<name>.write` (a census plan fixture, its tables before, PostgreSQL 18.6's rows, count and tables
//! after; scripts/gravity-v4/m4_write_golden.py records them) runs through gravity_driver over the rig's in-memory
//! tables (common/write_rig.rs) straight, under a stop, a cancel before the first row op and after it, and SCRIBBLE; the
//! judgment is the table state as a multiset, the count and the rows returned; a cancel's 57014 leaves every table as
//! before. A fixture whose statement v4 does not serve yet names its refusal in `refused:` (the flip is its builder's).
//! The seed `fixtures/m4/corpus.d/<fixture>.tsv` pins each seeded fixture's verdict by fixture name (`served` or
//! `refused <text>`) and a served row's IR.
use gravity_lowerv4_it as common;

#[path = "common/write_rig.rs"]
mod write_rig;

use std::collections::BTreeMap;

use common::Mode;
use write_rig::{judge, parse_write, run_write, Fault};

const WRITE_MODES: [Mode; 5] = [Mode::Straight, Mode::Stop(1), Mode::Cancel(0), Mode::Cancel(1), Mode::Scribble];

fn write_cases() -> Vec<write_rig::WriteCase> {
    let cases: Vec<_> = common::files(&common::milestone_dir(4), "write").into_iter().map(|(n, t)| parse_write(&n, &t).unwrap_or_else(|e| panic!("{e}"))).collect();
    assert!(!cases.is_empty(), "no tests/fixtures/m4/*.write");
    cases
}

/// Every write fixture v4 serves matches PG in every mode: rows returned, count, every table after.
#[test]
fn m4_writes_run_in_the_crate() {
    let (mut ran, mut bad) = (0, Vec::new());
    for c in write_cases() {
        if let Some(why) = &c.refused {
            match run_write(&c, Mode::Straight, Fault::None) {
                Err(e) if e.starts_with("refused: ") && e.contains(why.as_str()) => {}
                r => bad.push(format!("{}: expected the refusal {why:?}, got {r:?} (served: delete its refused: line)", c.name)),
            }
            continue;
        }
        for mode in WRITE_MODES {
            match run_write(&c, mode, Fault::None) {
                Ok(r) => match judge(&c, mode, &r) {
                    Ok(()) => ran += 1,
                    Err(e) => bad.push(e),
                },
                Err(e) => bad.push(format!("{} {mode:?}: {e}", c.name)),
            }
        }
    }
    eprintln!("m4 write rig: {ran} fixture x mode runs match PG");
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// The rig's selftest: a writer that keeps a cancelled statement's rows, or reports a row op it never applied, fails the
/// judgment (the rig can tell a partial effect and a lost write from PG's answer).
#[test]
fn m4_write_rig_catches_faults() {
    let cases = write_cases();
    let multi = cases.iter().find(|c| c.tag >= 2 && c.refused.is_none()).expect("a write fixture that writes two rows or more");
    let r = run_write(multi, Mode::Straight, Fault::Drop).unwrap_or_else(|e| panic!("{e}"));
    assert!(judge(multi, Mode::Straight, &r).is_err(), "{}: a dropped row op passed the judgment", multi.name);
    // a cancel raised at the first row op: the statement ends in 57014 (the rows go), else it ran to the end (PG's rows);
    // with the rollback removed, a cancelled statement's first row stays and the judgment fails
    let r = run_write(multi, Mode::Cancel(1), Fault::NoRollback).unwrap_or_else(|e| panic!("{e}"));
    if r.outcome == common::cancel_error() {
        assert!(judge(multi, Mode::Cancel(1), &r).is_err(), "{}: a cancelled statement's partial rows passed the judgment", multi.name);
    } else {
        eprintln!("{}: the cancel at the first row op landed after the last one (the statement completed)", multi.name);
    }
}

fn plan_text(id: &str) -> Option<String> {
    let rel = id.strip_prefix("regress-full-").map_or_else(|| format!("{id}.plan"), |s| format!("regress-full/{s}.plan"));
    std::fs::read_to_string(common::plans_dir().join(rel)).ok().or_else(|| std::fs::read_to_string(common::milestone_dir(4).join(format!("{id}.plan"))).ok())
}

/// `tests/fixtures/m4/<dir>/*.tsv` as (stem, its non-comment rows); each dir's README says what an entry means.
fn entries(dir: &str) -> BTreeMap<String, Vec<Vec<String>>> {
    let d = common::milestone_dir(4).join(dir);
    let mut out = BTreeMap::new();
    for e in std::fs::read_dir(&d).unwrap_or_else(|e| panic!("{}: {e}", d.display())) {
        let p = e.expect("dir entry").path();
        let Some(id) = p.file_name().and_then(|f| f.to_str()).and_then(|f| f.strip_suffix(".tsv")) else { continue };
        let t = std::fs::read_to_string(&p).expect("entry file");
        out.insert(id.to_owned(), t.lines().filter(|l| !l.starts_with('#') && !l.is_empty()).map(|l| l.split('\t').map(str::to_owned).collect()).collect());
    }
    out
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

fn verdict(text: &str) -> Option<String> {
    Some(match write_rig::lower_declared(text)? {
        Ok(_) => "served".to_owned(),
        Err(e) => format!("refused {e}"),
    })
}

/// Each seed row's verdict as corpus.d pins it; a served row's IR matches `fixtures/m4/<fixture>.ir` (LOWERV4_PIN=write records it).
#[test]
fn m4_seed_verdicts_and_pins() {
    let (rows, dir, write) = (seed(), common::milestone_dir(4), std::env::var("LOWERV4_PIN").is_ok_and(|v| v == "write"));
    let mut bad = Vec::new();
    for (id, src, want) in &rows {
        let text = plan_text(id);
        let got = text.as_deref().and_then(verdict).unwrap_or_else(|| "not decomposed".to_owned());
        println!("m4 seed\t{id}\t{src}\t{got}");
        if &got != want {
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

/// corpus.d's rows at M4-L1.
const MIN_SEED: usize = 48;

/// M4-LOCK's hooks in the write rig: `lock:` (the lock host's answers, then `done`) and `[locked]` (the rows the
/// statement must lock, a multiset) parse, and a run that locked another row fails the judgment. The EPQ recheck
/// (`recheck=<slot>`) runs once write.lock serves its first fixture (oltp-022).
#[test]
fn m4_write_rig_lock_script() {
    let text = "plan: oltp-022\ntable: 7 t k:int4 v:int4\nlock: done skip recheck=2\n[before 7]\n1\t10\n2\t20\n[pg]\n1\n[tag]\n1\n[locked]\n7\t1\n7\t0\n[after 7]\n1\t10\n2\t20\n";
    let c = parse_write("lockcase", text).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(c.locks, vec![write_rig::LockAns::Done, write_rig::LockAns::Skip, write_rig::LockAns::Recheck(2)]);
    assert_eq!(c.locked, Some(vec![(7, 0), (7, 1)]));
    let ran = |locked: Vec<(u32, u32)>| write_rig::Ran { outcome: c.pg.clone(), tag: 1, after: c.after.clone(), locked };
    assert!(judge(&c, Mode::Straight, &ran(vec![(7, 0), (7, 1)])).is_ok(), "the locked rows as [locked]");
    let e = judge(&c, Mode::Straight, &ran(vec![(7, 0), (7, 1), (7, 2)])).expect_err("a row locked past [locked]");
    assert!(e.contains("rows locked"), "{e}");
    // D604's concurrent version (`~` row): the scan sees the old one, the table after the new one
    let d = write_cases().into_iter().find(|c| c.name == "d604-share-nokey").expect("d604-share-nokey.write");
    assert_eq!((d.tables[&16462].ver.clone(), d.tables[&16462].digest()), (vec![1, 2, 0], d.after[&16462].clone()));
}

