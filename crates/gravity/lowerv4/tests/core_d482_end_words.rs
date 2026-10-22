//! D482: a pipeline that reads a run (`sort_begin_at` / `sort_begin`) and ends in a sort (`sort_open_res` /
//! `sort_claim_res`) keeps the reader's mode and the end's run in different frame words. Both keyed
//! `Open { Runs, k, j: MAX, i: 0 }`, so `sort_open_res`'s run pointer was overwritten by the reader's mode and the first row
//! write loaded the run from address 1 (SIGSEGV at 0x5, part-034's shape under the D463 flip). Checked on every pinned M3
//! program (`m3_ir_pins` holds the lowerer to the pins): no (base, offset) is stored from both a run host and a reader host.
use std::collections::{BTreeMap, BTreeSet};

const RUN: &[&str] = &["sort_open_res", "sort_open_res_top", "sort_claim_res"];
const READER: &[&str] = &["sort_begin", "sort_begin_at"];

/// The host calls a value derives from, through the conversions and selects the lowerer puts between a call and its store.
fn roots<'a>(v: &'a str, defs: &BTreeMap<&'a str, Vec<&'a str>>, seen: &mut BTreeSet<&'a str>, out: &mut BTreeSet<&'a str>) {
    if !seen.insert(v) {
        return;
    }
    let Some(t) = defs.get(v) else { return };
    match t.first().copied() {
        Some("call_host") => {
            out.insert(t[1]);
        }
        Some("ptrtoint" | "inttoptr" | "select") => t[1..].iter().filter(|a| a.starts_with('v')).for_each(|a| roots(a, defs, seen, out)),
        _ => {}
    }
}

/// Every `(fn, base, offset)` a run host's result and a reader host's result are both stored at.
fn collisions(ir: &str) -> Vec<String> {
    let mut bad = Vec::new();
    for f in ir.split("(fn ").skip(1) {
        let name = f.split_whitespace().next().unwrap_or("?");
        let toks = |l: &str| l.replace(['(', ')'], " ").split_whitespace().map(str::to_owned).collect::<Vec<_>>();
        let lines: Vec<Vec<String>> = f.lines().map(toks).collect();
        let mut defs = BTreeMap::new();
        for t in lines.iter().filter(|t| t.len() > 1 && t[0].starts_with('v') && t[1] != "ptr" && t[1] != "i64") {
            defs.insert(t[0].as_str(), t[1..].iter().map(String::as_str).collect::<Vec<_>>());
        }
        let mut at: BTreeMap<(String, String), BTreeSet<&str>> = BTreeMap::new();
        for t in lines.iter().filter(|t| t.len() == 4 && t[0] == "store") {
            let mut hosts = BTreeSet::new();
            roots(&t[3], &defs, &mut BTreeSet::new(), &mut hosts);
            at.entry((t[1].clone(), t[2].clone())).or_default().extend(hosts);
        }
        for ((base, off), hosts) in at {
            if hosts.iter().any(|h| RUN.contains(h)) && hosts.iter().any(|h| READER.contains(h)) {
                bad.push(format!("{name}: store {base} {off} holds {hosts:?}"));
            }
        }
    }
    bad
}

#[test]
fn a_run_reader_and_a_sort_end_keep_their_own_words() {
    let dir = manifest_dir().join("tests/fixtures/m3");
    let (mut both, mut bad) = (0, Vec::new());
    for e in std::fs::read_dir(&dir).expect("fixtures/m3").flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "ir") {
            continue;
        }
        let ir = std::fs::read_to_string(&p).expect("pin");
        both += usize::from(RUN.iter().any(|h| ir.contains(h)) && ir.contains("sort_begin_at"));
        bad.extend(collisions(&ir).into_iter().map(|c| format!("{}: {c}", p.display())));
    }
    // part-034 (M3-MERGE) is the pinned program of the shape; a tree without it still runs the synthetic check below
    eprintln!("D482: {both} pinned programs read a run and end in a sort");
    assert!(bad.is_empty(), "a reader's mode and a sort end's run share a word:\n{}", bad.join("\n"));
}

/// The check itself: the D482 program (one word for the run and the mode) is caught, the fixed one is not.
#[test]
fn the_check_catches_the_shared_word() {
    let ir = |run_off: u32| {
        format!(
            "(fn p1 ((v0 ptr)) (i64)\n (block b0 ((v0 ptr))\n (v15 (load ptr v0 528))\n (v33 (call_host sort_open_res v0 v1 v2 v3))\n \
             (v34 (ptrtoint v33))\n ((store v15 {run_off} v34))\n (v47 (call_host sort_begin_at v0 v4 v5 v6))\n \
             (v50 (select v49 v11 v47))\n ((store v15 8 v50))))\n"
        )
    };
    assert_eq!(collisions(&ir(8)).len(), 1, "{:?}", collisions(&ir(8)));
    assert!(collisions(&ir(32)).is_empty());
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(|d| std::path::Path::new(&d).with_file_name("lowerv4")).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
