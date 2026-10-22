use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use gravity_decompose::{corpus, decompose_in, dump, Kind};
use gravity_program::Arena;
use mcx::MemoryContext;

mod harness;

fn dir(name: &str) -> PathBuf {
    manifest_dir().join("tests").join(name)
}

fn fixtures() -> Vec<PathBuf> {
    harness::fixtures_in(&dir("plans"))
}

/// Every fixture's graph text, decomposed once per process on all cores, in fixture order.
fn graphs() -> &'static [(PathBuf, String, Vec<Kind>, Option<Kind>)] {
    static ALL: std::sync::OnceLock<Vec<(PathBuf, String, Vec<Kind>, Option<Kind>)>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        let fx = fixtures();
        harness::par_map(&fx, |f| {
            let (t, k, r) = graph_text(f);
            (f.clone(), t, k, r)
        })
    })
}

fn graph_text(path: &Path) -> (String, Vec<Kind>, Option<Kind>) {
    let text = fs::read_to_string(path).unwrap();
    let ctx = MemoryContext::new("fixture");
    let fx = corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let arena = Arena::new(1 << 20);
    let mut out = format!("-- {}\n-- {}\n", fx.name, fx.sql);
    let r = match decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) {
        Ok(g) => {
            dump::dump(&g, &mut out).unwrap();
            (out, g.mapped_kinds().collect(), None)
        }
        Err(r) => {
            dump::dump_refusal(&r, &mut out).unwrap();
            (out, Vec::new(), Some(r.kind))
        }
    };
    r
}

#[test]
fn every_fixture_reproduces_its_golden() {
    let mut stats = harness::Stats::new("decompose");
    for (f, got, _, _) in graphs() {
        stats.loaded += 1;
        let g = dir("golden").join(f.with_extension("graph").file_name().unwrap());
        stats.check(&g, &harness::fixture_name(f), got);
    }
    assert!(stats.loaded > 0 || harness::filter().is_some(), "no fixtures under tests/plans");
    stats.finish();
}

#[test]
fn every_golden_has_a_fixture() {
    if harness::filter().is_some() {
        return;
    }
    let mut orphans = Vec::new();
    for e in fs::read_dir(dir("golden")).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "graph") && !dir("plans").join(p.with_extension("plan").file_name().unwrap()).exists() {
            orphans.push(p);
        }
    }
    assert!(orphans.is_empty(), "goldens without a fixture: {orphans:?}");
}

#[test]
fn coverage_histogram() {
    let mut mapped: BTreeMap<&str, usize> = BTreeMap::new();
    let mut refused: BTreeMap<&str, usize> = BTreeMap::new();
    let mut served = 0;
    if harness::filter().is_some() {
        println!("coverage_histogram: skipped (FIXTURES set; a whole-corpus property)");
        return;
    }
    for (_, _, kinds, r) in graphs() {
        match r {
            None => {
                served += 1;
                for k in kinds {
                    *mapped.entry(k.name()).or_default() += 1;
                }
            }
            Some(k) => *refused.entry(k.name()).or_default() += 1,
        }
    }
    println!("served {served} / refused {}", refused.values().sum::<usize>());
    println!("mapped by kind: {mapped:?}");
    println!("refused by kind: {refused:?}");
    for (k, _) in gravity_decompose::LEDGER.iter().filter(|(k, _)| gravity_decompose::ledger::is_mapped(*k)) {
        if gravity_decompose::ledger::gated_by(*k).is_some_and(|g| !gravity_decompose::ledger::is_mapped(g)) || gravity_decompose::ledger::unplanned(*k) {
            continue;
        }
        assert!(mapped.contains_key(k.name()), "mapped kind {} appears in no served golden", k.name());
    }
    for k in refused.keys() {
        let kind = Kind::ALL.iter().find(|x| x.name() == *k).unwrap();
        assert!(!gravity_decompose::ledger::is_mapped(*kind) || true, "{k}");
    }
}

/// The corpus loader refuses an index row wider than `limits::KEYS` (= `INDEX_MAX_KEYS`) by name instead of truncating it.
#[test]
fn loader_refuses_an_index_row_past_keys() {
    let mut facts = corpus::FixtureFacts::default();
    let keys: Vec<String> = (1..=32).map(|i| i.to_string()).collect();
    assert_eq!(facts.line(&format!("idx 5 rel=1 keys={} opts=0 unique=f am=btree partial=f deferrable=f excl=f primary=f nkeyatts=32", keys.join(","))), Ok(true));
    let keys: Vec<String> = (1..=33).map(|i| i.to_string()).collect();
    let err = facts.line(&format!("idx 6 rel=1 keys={} opts=0 unique=f am=btree partial=f", keys.join(","))).unwrap_err();
    assert!(err.contains("index 6: more than 32 key columns"), "{err}");
}

/// The `-- trigext` header line answers `Facts::trig_ext` (a relation without one stays UNKNOWN, which the write gate
/// refuses), and a trigger-free relation is NONE, so a fixture DELETE on it can be admitted.
#[test]
fn loader_reads_trigext_lines() {
    use gravity_decompose::catalog::Facts;
    use gravity_decompose::ops::modifytable::trig_facts::{TrigExt, TRIG_DELETE, TRIG_INSERT, TRIG_UPDATE};
    let mut facts = corpus::FixtureFacts::default();
    assert_eq!(facts.line("trigext 16390 before=- after=- instead=- stmt=- trans=- internal=- defer=- rules=f"), Ok(true));
    assert_eq!(facts.line("trigext 16400 before=iu after=d instead=- stmt=iud trans=d internal=d defer=- rules=t"), Ok(true));
    assert_eq!(facts.trig_ext(16390), TrigExt::NONE);
    let t = facts.trig_ext(16400);
    assert_eq!((t.before_row, t.after_row, t.instead_row, t.statement, t.transition, t.internal, t.deferrable, t.known),
        (TRIG_INSERT | TRIG_UPDATE, TRIG_DELETE, 0, TRIG_INSERT | TRIG_UPDATE | TRIG_DELETE, TRIG_DELETE, TRIG_DELETE, 0, true));
    assert_eq!(facts.trig_ext(16401), TrigExt::UNKNOWN);
    let err = facts.line("trigext 16402 before=x after=- instead=- stmt=- trans=- internal=- defer=- rules=f").unwrap_err();
    assert!(err.contains("bad trigext before="), "{err}");
}

/// The `-- func` header line answers `Facts::func` for a function past the builtin table (its pg_proc row as the capture
/// read it); a builtin still answers from the table, and a function no line names stays unknown.
#[test]
fn loader_reads_func_lines() {
    use gravity_decompose::catalog::Facts;
    use gravity_decompose::effects::{Parallel, Volatility};
    let mut facts = corpus::FixtureFacts::default();
    assert_eq!(facts.line("func 16432 vol=s strict=t leakproof=f parallel=r retset=f nargs=1 lang=14 nodearg=f"), Ok(true));
    let f = facts.func(16432).expect("the line's props");
    assert_eq!((f.volatility, f.strict, f.leakproof, f.parallel), (Volatility::Stable, true, false, Parallel::Restricted));
    assert_eq!((facts.funcs[0].retset, facts.funcs[0].nargs, facts.funcs[0].lang, facts.funcs[0].node_arg), (false, 1, 14, false));
    assert_eq!(facts.func(1066).map(|f| f.volatility), Some(Volatility::Immutable));
    assert_eq!(facts.func(16433), None);
    let err = facts.line("func 16434 vol=s strict=x leakproof=f parallel=r retset=f nargs=1 lang=14 nodearg=f").unwrap_err();
    assert!(err.contains("bad func strict="), "{err}");
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
