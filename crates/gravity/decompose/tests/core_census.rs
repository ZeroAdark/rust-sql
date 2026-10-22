// The corpus census (decompose.md §5.6): every corpus under tests/plans — the in-crate corpora by name prefix,
// regress-full by directory — decomposed once on all cores, then one table of corpus × {fixtures, served, refused
// by a named ruling (ledger::OWNERS), refused unowned, panics}, the by-ruling rows with their owners and the
// unowned rows by refusal text with every fixture that carries them; byte-compared to tests/golden/census.txt
// (REGEN=1 / REGEN=changed rewrite it). `scripts/gravity-capture-plans.sh <corpus>` regenerates a corpus.
use std::collections::BTreeMap;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use gravity_decompose::{corpus, decompose_in, ledger};
use gravity_program::Arena;
use mcx::MemoryContext;

mod harness;

thread_local!(static CAPTURING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });

enum Outcome {
    Served,
    Refused(&'static str, &'static str),
    Panic(String),
}

struct Row {
    corpus: String,
    name: String,
    outcome: Outcome,
}

/// A fixture's corpus: its directory under tests/plans, else its name without the `-nnn` suffix.
fn corpus_of(path: &Path) -> String {
    let name = harness::fixture_name(path);
    match name.split_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => match name.rsplit_once('-') {
            Some((c, n)) if n.chars().all(|ch| ch.is_ascii_digit()) => c.to_string(),
            _ => name,
        },
    }
}

fn fixtures() -> Vec<PathBuf> {
    let root = harness::plans_root();
    let mut v = harness::fixtures_in(&root);
    let rf = root.join("regress-full");
    if rf.is_dir() {
        v.extend(harness::fixtures_in(&rf));
    }
    v
}

fn run(path: &Path) -> Row {
    let raw = fs::read_to_string(path).unwrap();
    let ctx = MemoryContext::new("fixture");
    let fx = corpus::load(ctx.mcx(), &raw).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let arena = Arena::new(1 << 20);
    CAPTURING.with(|c| c.set(true));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).map(|_| ()).map_err(|r| (r.kind.name(), r.reason))
    }));
    CAPTURING.with(|c| c.set(false));
    let outcome = match result {
        Err(p) => Outcome::Panic(p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default()),
        Ok(Ok(())) => Outcome::Served,
        Ok(Err((k, r))) => Outcome::Refused(k, r),
    };
    Row { corpus: corpus_of(path), name: harness::fixture_name(path), outcome }
}

#[derive(Default)]
struct Cell {
    fixtures: usize,
    served: usize,
    ruled: usize,
    unowned: usize,
    panics: usize,
}

fn names(v: &[String]) -> String {
    const SHOWN: usize = 12;
    let mut s = v.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", ");
    if v.len() > SHOWN {
        write!(s, ", … {} more", v.len() - SHOWN).unwrap();
    }
    s
}

/// Every corpus decomposed once per process (the two tests share it).
fn rows() -> &'static [Row] {
    static ALL: std::sync::OnceLock<Vec<Row>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        std::panic::set_hook(Box::new(|info| {
            if !CAPTURING.with(|c| c.get()) {
                eprintln!("{info}");
            }
        }));
        harness::par_map(&fixtures(), |f| run(f))
    })
}

#[test]
fn corpus_census() {
    let rows = rows();
    let mut table: BTreeMap<String, Cell> = BTreeMap::new();
    let mut ruled: BTreeMap<(&str, &str), (BTreeMap<String, usize>, &str)> = BTreeMap::new();
    let mut unowned: BTreeMap<(&str, &str), Vec<String>> = BTreeMap::new();
    let mut panics: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for r in rows {
        let c = table.entry(r.corpus.clone()).or_default();
        c.fixtures += 1;
        match &r.outcome {
            Outcome::Served => c.served += 1,
            Outcome::Panic(m) => {
                c.panics += 1;
                panics.entry(m.clone()).or_default().push(r.name.clone());
            }
            Outcome::Refused(k, reason) => match ledger::owner(reason) {
                Some(o) => {
                    c.ruled += 1;
                    let e = ruled.entry((k, reason)).or_insert_with(|| (BTreeMap::new(), o));
                    *e.0.entry(r.corpus.clone()).or_default() += 1;
                }
                None => {
                    c.unowned += 1;
                    unowned.entry((k, reason)).or_default().push(r.name.clone());
                }
            },
        }
    }
    let mut out = String::new();
    let w = table.keys().map(|k| k.len()).max().unwrap_or(6).max(6);
    writeln!(out, "{:w$}  fixtures  served  by-ruling  unowned  panics", "corpus", w = w).unwrap();
    let mut total = Cell::default();
    for (name, c) in &table {
        writeln!(out, "{:w$}  {:8}  {:6}  {:9}  {:7}  {:6}", name, c.fixtures, c.served, c.ruled, c.unowned, c.panics, w = w).unwrap();
        total.fixtures += c.fixtures;
        total.served += c.served;
        total.ruled += c.ruled;
        total.unowned += c.unowned;
        total.panics += c.panics;
    }
    writeln!(out, "{:w$}  {:8}  {:6}  {:9}  {:7}  {:6}", "total", total.fixtures, total.served, total.ruled, total.unowned, total.panics, w = w).unwrap();
    writeln!(out, "refused by a named ruling ({}):", total.ruled).unwrap();
    for ((k, reason), (per, owner)) in &ruled {
        let n: usize = per.values().sum();
        let per = per.iter().map(|(c, n)| format!("{c} {n}")).collect::<Vec<_>>().join(", ");
        writeln!(out, "  {k}: {reason} {n} [{per}] -- {owner}").unwrap();
    }
    writeln!(out, "refused unowned ({}):", total.unowned).unwrap();
    for ((k, reason), fx) in &unowned {
        writeln!(out, "  {k}: {reason} {} -- {}", fx.len(), names(fx)).unwrap();
    }
    writeln!(out, "decomposer panics ({}):", total.panics).unwrap();
    for (m, fx) in &panics {
        writeln!(out, "  {m} {} -- {}", fx.len(), names(fx)).unwrap();
    }
    println!("{out}");
    if harness::filter().is_some() {
        println!("corpus_census: census.txt not compared (FIXTURES set; a whole-corpus property)");
        return;
    }
    let mut stats = harness::Stats::new("census");
    stats.loaded = rows.len();
    stats.check(&manifest_dir().join("tests").join("golden").join("census.txt"), "census.txt", &out);
    stats.finish();
}

/// Every owner row in the ledger names a reason some corpus still carries, or the row is stale.
#[test]
fn owner_rows_are_live() {
    let rows = rows();
    let stale: Vec<&str> = ledger::OWNERS
        .iter()
        .map(|(p, _)| *p)
        .filter(|p| !rows.iter().any(|r| matches!(&r.outcome, Outcome::Refused(_, reason) if reason.starts_with(p))))
        .collect();
    assert!(stale.is_empty() || harness::filter().is_some(), "owner rows no corpus reaches (delete them): {stale:?}");
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
