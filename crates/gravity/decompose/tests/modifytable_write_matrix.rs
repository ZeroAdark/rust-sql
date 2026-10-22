// The write family's coverage matrix (decompose.md §5.5): rows = `Member::LIST`, columns = the corpora
// (regress-full, then tests/plans by prefix). A member's cell counts the decomposed graphs carrying it (the lowering
// family admits or refuses each by name). Byte-compared to
// tests/golden/write-matrix.txt (REGEN=1 rewrites).
use std::collections::BTreeMap;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use gravity_decompose::ops::modifytable::{members_of, unclassified};
use gravity_decompose::ops::shared::modifytable::Member;
use gravity_decompose::{corpus, decompose_in, MemberSet};
use gravity_program::Arena;
use mcx::MemoryContext;

const CORPORA: &[&str] = &["regress-full", "fieldobs", "oltp", "regress", "window", "setops", "tpch", "srf", "cte", "part"];

fn dir(name: &str) -> PathBuf {
    manifest_dir().join("tests").join(name)
}

fn plans(corpus: &str) -> Vec<PathBuf> {
    let (d, prefix) = if corpus == "regress-full" { (dir("plans").join("regress-full"), String::new()) } else { (dir("plans"), format!("{corpus}-")) };
    let mut v: Vec<PathBuf> = fs::read_dir(d).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "plan") && p.file_name().unwrap().to_str().unwrap().starts_with(&prefix)).collect();
    v.sort();
    v
}

/// (served members, the refusal reason) of one fixture; a decomposer panic counts as no members.
fn outcome(path: &Path) -> (MemberSet, Option<&'static str>, usize) {
    let text = fs::read_to_string(path).unwrap();
    let ctx = MemoryContext::new("fixture");
    let fx = corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let arena = Arena::new(1 << 20);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) {
        Ok(g) => (members_of(&g), None, unclassified(&g)),
        Err(r) => (MemberSet::EMPTY, Some(r.reason), 0),
    }));
    r.unwrap_or((MemberSet::EMPTY, None, 0))
}

#[test]
fn write_matrix() {
    let mut served: BTreeMap<(&str, u8), usize> = BTreeMap::new();
    let mut totals: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let mut unknowns = Vec::new();
    for &c in CORPORA {
        for p in plans(c) {
            let (m, r, unknown) = outcome(&p);
            if unknown > 0 {
                unknowns.push(p.display().to_string());
            }
            let t = totals.entry(c).or_default();
            t.0 += 1;
            if r.is_none() {
                t.1 += 1;
            }
            for m in m.iter() {
                *served.entry((c, m as u8)).or_default() += 1;
            }
        }
    }
    assert!(unknowns.is_empty(), "indexes of a written relation without constraint facts (re-capture: `-- idx … nkeyatts=`): {unknowns:#?}");
    // W5 multilock (main 09-29 04:31Z): PG 18's planner never makes a locking mark on a ModifyTable (planner.c:2238; the
    // lowering refuses one as a Bug), so no fixture of any corpus, the full regress run's shapes among them, carries it
    let bug = CORPORA.iter().map(|&c| served.get(&(c, Member::LockingWriteMark as u8)).copied().unwrap_or(0)).sum::<usize>();
    assert_eq!(bug, 0, "fixtures carrying LockingWriteMark (planner.c:2238)");
    let mut out = String::new();
    write!(out, "| member |").unwrap();
    for c in CORPORA {
        write!(out, " {c} |").unwrap();
    }
    writeln!(out).unwrap();
    write!(out, "|---|").unwrap();
    for _ in CORPORA {
        write!(out, "---|").unwrap();
    }
    writeln!(out).unwrap();
    for m in Member::LIST {
        write!(out, "| {m:?} |").unwrap();
        for &c in CORPORA {
            write!(out, " {} |", served.get(&(c, m as u8)).copied().unwrap_or(0)).unwrap();
        }
        writeln!(out).unwrap();
    }
    write!(out, "| fixtures (served / total) |").unwrap();
    for c in CORPORA {
        let (n, s) = totals.get(c).copied().unwrap_or((0, 0));
        write!(out, " {s} / {n} |").unwrap();
    }
    writeln!(out).unwrap();
    let g = dir("golden").join("write-matrix.txt");
    if std::env::var_os("REGEN").is_some() {
        fs::write(&g, &out).unwrap();
        return;
    }
    let want = fs::read_to_string(&g).unwrap_or_default();
    assert!(want == out, "{}: differs from its golden\n--- want\n{want}\n--- got\n{out}", g.display());
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
