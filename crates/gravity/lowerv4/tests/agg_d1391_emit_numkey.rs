//! V4R-569 (D1391; census r48 regress subselect#124): a group reading a keyed group emit's narrow numeric key sizes its
//! record by the emit's kept copy (cell.rs `emit_copy`), so #124's sorted Group over its Unique lowers; every other
//! decompose plan fixture keeps its outcome. The corpus pass writes each fixture's outcome (its lowered program's printed IR
//! hashed, embedded addresses masked) to `D1391_MANIFEST`; with `D1391_BASE` (the same test's manifest at the base) it checks
//! that a fixture the base lowers keeps its program, that none turns refused, and that each changed one was refused there.
use gravity_lowerv4_it as common;

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

const SUBSELECT_124: &str = "regress-full-subselect-050";

/// Every decompose plan fixture, top level and regress-full, by its `-- name:` line.
fn corpus() -> Vec<(String, String)> {
    let dirs = [common::plans_dir(), common::plans_dir().join("regress-full")];
    dirs.iter().flat_map(|d| common::files(d, "plan")).map(|(stem, text)| (text.lines().find_map(|l| l.strip_prefix("-- name: ")).map_or(stem, str::to_owned), text)).collect()
}

/// The printed IR with every integer literal of ten or more digits (an embedded address) masked, hashed.
fn program_hash(l: &common::LoweredIn) -> u64 {
    let ir = gravity_ir::text::print_program(&l.ir);
    let (mut out, mut run) = (String::with_capacity(ir.len()), String::new());
    for c in ir.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_digit() {
            run.push(c);
            continue;
        }
        out.push_str(if run.len() >= 10 { "N" } else { &run });
        run.clear();
        out.push(c);
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    out.hash(&mut h);
    h.finish()
}

/// `LOWERED <hash>` (`LOWERED nondet` when two lowerings print apart), `REFUSED <refusal>` or `DECOMPOSE-REFUSED`.
fn outcome(text: &str) -> String {
    match common::lower_fixture(text) {
        None => "DECOMPOSE-REFUSED".to_owned(),
        Some(Err(r)) => format!("REFUSED {}", r.to_string().replace(['\t', '\n'], " ")),
        Some(Ok(l)) => {
            let h = program_hash(&l);
            match common::lower_fixture(text) {
                Some(Ok(again)) if program_hash(&again) == h => format!("LOWERED {h:016x}"),
                _ => "LOWERED nondet".to_owned(),
            }
        }
    }
}

#[test]
fn subselect_124_lowers() {
    let (_, text) = corpus().into_iter().find(|(n, _)| n == SUBSELECT_124).expect("regress-full/subselect-050.plan");
    assert!(text.contains("group by f1,f2,fs"), "the fixture is #124's statement");
    match common::lower_fixture(&text) {
        Some(Ok(_)) => {}
        Some(Err(r)) => panic!("#124 refused: {r}"),
        None => panic!("#124: decompose refused"),
    }
}

/// The corpus pass lowers every fixture twice (about 90 s): it runs only when asked (`D1391_MANIFEST` or `D1391_BASE`).
#[test]
fn corpus_keeps_its_programs() {
    if std::env::var_os("D1391_MANIFEST").is_none() && std::env::var_os("D1391_BASE").is_none() {
        return;
    }
    let tip: BTreeMap<String, String> = corpus().into_iter().map(|(n, t)| (n, outcome(&t))).collect();
    assert!(tip.len() > 5000, "the decompose corpus: {} fixtures", tip.len());
    if let Some(p) = std::env::var_os("D1391_MANIFEST") {
        std::fs::write(&p, tip.iter().map(|(n, o)| format!("{n}\t{o}\n")).collect::<String>()).expect("D1391_MANIFEST");
    }
    let Some(base) = std::env::var_os("D1391_BASE") else { return };
    let base: BTreeMap<String, String> = std::fs::read_to_string(base).expect("D1391_BASE").lines().filter_map(|l| l.split_once('\t')).map(|(n, o)| (n.to_owned(), o.to_owned())).collect();
    let (mut bad, mut changed, mut same) = (Vec::new(), Vec::new(), 0usize);
    for (n, t) in &tip {
        let b = base.get(n).map_or("MISSING", String::as_str);
        match (b.starts_with("LOWERED"), t.starts_with("LOWERED")) {
            _ if b == t => same += 1,
            (true, true) if b.ends_with("nondet") || t.ends_with("nondet") => same += 1,
            (true, _) => bad.push(format!("{n}: base {b} / tip {t}")),
            (false, _) => changed.push(format!("{n}: base {b} / tip {t}")),
        }
    }
    eprintln!("D1391-CORPUS fixtures {} same {same} changed {} (each refused at the base) bad {}", tip.len(), changed.len(), bad.len());
    changed.iter().chain(&bad).for_each(|l| eprintln!("D1391-CORPUS {l}"));
    assert!(bad.is_empty(), "a fixture the base lowers changed its program or turned refused:\n{}", bad.join("\n"));
    assert!(changed.iter().any(|l| l.starts_with(SUBSELECT_124)), "#124 changed from the base");
}
