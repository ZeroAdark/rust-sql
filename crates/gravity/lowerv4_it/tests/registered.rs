//! Every integration test file of gravity_lowerv4 (crates/gravity/lowerv4/tests/*.rs and tests/*/main.rs, what cargo's
//! autotests would build) is in exactly one of two places: a module of the one test binary lowerv4_suite
//! (tests/lowerv4_suite.rs: `#[path = "../../lowerv4/tests/<file>"]` then `mod t_<stem>;`, BD-21), or a [[test]] entry of
//! this package's Cargo.toml, named <stem>. Every listed file exists. lowerv4 has autotests = false, so an unlisted file
//! would never compile or run. ct keys a module's tests as the former target <stem>, so the module name must be t_<stem>.
//! A plain `cargo test -p gravity_lowerv4_it` runs every module of lowerv4_suite in ONE process, so a file is a module only
//! if it can share a process (main 2026-10-09T20:15:49Z). It must be standalone if it has a #[global_allocator] (it would
//! become every module's allocator), if a test re-runs its own binary by exact test name (std::env::current_exe()), if it
//! sets a process-wide seam or state directly (`<crate>_seams::<seam>::set(`, pgtz::init_seams / set_session_timezone,
//! install_spill_factory, fp::install), if it pins IR (ir_pins: the IR depends on which seams the process has), or if it
//! counts the process-wide refusal intern set (interned()), or if it switches lowerv4's gate stamps (set_gate_stamps,
//! which every lowering in the process reads). Such a file must be standalone, and only such a file may be.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// What makes a file unable to be a module of lowerv4_suite: it needs its own crate root ...
const ROOT_ONLY: [&str; 2] = ["#[global_allocator]", "current_exe("];
/// ... or it changes (or pins what depends on) process-wide state that the other modules of one process would share.
/// Matched in code lines (not `//` comments); a line with both `_seams::` and `::set(` is a direct seam set.
const PROCESS_STATE: [&str; 7] = [
    "pgtz::init_seams",
    "pgtz::set_session_timezone",
    "install_spill_factory",
    "fp::install(",
    "ir_pins",
    "interned()",
    "set_gate_stamps(",
];

/// Why `src` cannot be a module of lowerv4_suite, if it cannot.
fn standalone_reason(src: &str) -> Option<String> {
    if let Some(p) = ROOT_ONLY.iter().find(|p| src.contains(**p)) {
        return Some(format!("has `{p}`"));
    }
    for l in src.lines().map(str::trim).filter(|l| !l.starts_with("//")) {
        if l.contains("_seams::") && l.contains("::set(") {
            return Some(format!("sets a process-wide seam: `{l}`"));
        }
        if let Some(p) = PROCESS_STATE.iter().find(|p| l.contains(**p)) {
            return Some(format!("has `{p}` (process-wide state)"));
        }
    }
    None
}

fn stem(file: &str) -> &str {
    file.strip_suffix("/main.rs").or_else(|| file.strip_suffix(".rs")).unwrap_or(file)
}

#[test]
fn every_lowerv4_test_file_is_registered() {
    // D793-b: the checkout under test, read at run time
    let me = std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)");
    let me = Path::new(&me);
    let dir = me.with_file_name("lowerv4").join("tests");
    let mut files = BTreeSet::new();
    for e in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        if p.is_file() && name.ends_with(".rs") {
            files.insert(name);
        } else if p.is_dir() && p.join("main.rs").is_file() {
            files.insert(format!("{name}/main.rs"));
        }
    }
    let mut bad = Vec::new();

    // the [[test]] entries of lowerv4's files (file -> target name); the [lib] path (the rig) is not a test file
    let toml = std::fs::read_to_string(me.join("Cargo.toml")).unwrap();
    let (mut section, mut name) = ("", String::new());
    let mut standalone = BTreeMap::new();
    for l in toml.lines().map(str::trim) {
        if l.starts_with('[') {
            (section, name) = (l, String::new());
        } else if section == "[[test]]" {
            if let Some(n) = l.strip_prefix("name = \"").and_then(|r| r.strip_suffix('"')) {
                name = n.to_string();
            } else if let Some(f) = l.strip_prefix("path = \"../lowerv4/tests/").and_then(|r| r.strip_suffix('"')) {
                standalone.insert(f.to_string(), name.clone());
            }
        }
    }
    for (f, n) in &standalone {
        if n != stem(f) {
            bad.push(format!("Cargo.toml: the [[test]] for {f} is named {n:?}; name it {:?} (ct keys it by name)", stem(f)));
        }
    }

    // the modules of lowerv4_suite (file -> module name)
    let root = me.join("tests").join("lowerv4_suite.rs");
    let text = std::fs::read_to_string(&root).unwrap_or_else(|e| panic!("{}: {e}", root.display()));
    let mut suite = BTreeMap::new();
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    while let Some(l) = lines.next() {
        let Some(f) = l.strip_prefix("#[path = \"../../lowerv4/tests/").and_then(|r| r.strip_suffix("\"]")) else { continue };
        let m = lines.next().and_then(|m| m.strip_prefix("mod ")).and_then(|m| m.strip_suffix(';')).unwrap_or("");
        if m != format!("t_{}", stem(f)) {
            bad.push(format!("lowerv4_suite.rs: {f} is `mod {m}`; write `mod t_{};` right after its #[path] line", stem(f)));
        }
        if suite.insert(f.to_string(), m.to_string()).is_some() {
            bad.push(format!("lowerv4_suite.rs: {f} is listed twice"));
        }
    }

    let listed: BTreeSet<String> = suite.keys().chain(standalone.keys()).cloned().collect();
    let missing: Vec<_> = files.difference(&listed).collect();
    let gone: Vec<_> = listed.difference(&files).collect();
    if !missing.is_empty() {
        bad.push(format!(
            "not registered: {missing:?}. Add `#[path = \"../../lowerv4/tests/<file>\"]` and `mod t_<stem>;` to \
             crates/gravity/lowerv4_it/tests/lowerv4_suite.rs, or, if the file cannot be a module (see standalone_reason), \
             `[[test]] name = \"<stem>\" path = \"../lowerv4/tests/<file>\"` to crates/gravity/lowerv4_it/Cargo.toml"
        ));
    }
    if !gone.is_empty() {
        bad.push(format!("registered but missing under {}: {gone:?}", dir.display()));
    }
    for f in suite.keys().filter(|f| standalone.contains_key(*f)) {
        bad.push(format!("{f} is both a module of lowerv4_suite and its own [[test]]: keep one"));
    }
    for f in files.iter().filter(|f| listed.contains(*f)) {
        let src = std::fs::read_to_string(dir.join(f)).unwrap_or_else(|e| panic!("{f}: {e}"));
        match (suite.contains_key(f), standalone_reason(&src)) {
            (true, Some(why)) => bad.push(format!("{f} {why}: it cannot share lowerv4_suite's process; give it a [[test]] entry")),
            (false, None) => bad.push(format!(
                "{f} is its own [[test]] but has none of {ROOT_ONLY:?}, {PROCESS_STATE:?} or a seam set: make it a module of lowerv4_suite"
            )),
            _ => {}
        }
    }
    assert!(bad.is_empty(), "crates/gravity/lowerv4_it and crates/gravity/lowerv4/tests disagree:\n  {}", bad.join("\n  "));
    assert!(files.len() > 50, "only {} lowerv4 test files under {}: wrong directory?", files.len(), dir.display());
    assert!(suite.len() > 50, "only {} modules in {}: wrong file?", suite.len(), root.display());
}
