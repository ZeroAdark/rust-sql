//! The integration test files of gravity_parallel: tests/*.rs and tests/*/main.rs, what cargo's autotests would build.
//! Each is in exactly one of two places:
//! - a module of the one test binary gravity_parallel_suite (its root tests/gravity_parallel_suite.rs says `#[path = "<file>"]`,
//!   then `mod t_<stem>;`);
//! - a [[test]] entry of this package's Cargo.toml, named <stem>, with `path = "tests/<file>"`.
//! Every listed file exists. The package has autotests = false, so an unlisted file would never compile or run. ct keys a
//! module's tests as the former target <stem>, so the module name must be t_<stem>.
//! A plain `cargo test -p gravity_parallel` runs every module of the suite in ONE process, so a file is a module only if its tests
//! can share that process with every other module's. It must be standalone, and only such a file may be (or a [[test]]
//! with `required-features`, built only with its feature), if it is named like a binary of ct's serial tail (SERIAL: it
//! runs alone, its bounds are wall-clock time), if it has a #[global_allocator] or re-runs its own binary
//! (current_exe), if its text names state as process-wide or process-global, if it uses a `pub static` that holds state
//! (STATIC_SRCS, outside a thread_local! block, not a table of plain data or a unit struct's value), or if a code line
//! sets or reads process-wide state through a call (PROCESS_STATE, the package's RIG_STATE, or a
//! `<crate>_seams::<seam>::set(`). A module that never makes such a call may rely on the process's state being as its
//! own binary left it (a seam not installed, a GUC store not built), so one that does goes apart even when the call is
//! guarded to run once.
//! The suite's root and this file (its module t_registered) are not test files of the rule. The same guard, with its
//! package's SUITE, STATIC_SRCS and RIG_STATE, checks every package built this way.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The one test binary's target name (its root is tests/<SUITE>.rs).
const SUITE: &str = "gravity_parallel_suite";
/// This file: a module of the suite, outside the rule.
const GUARD: &str = "registered.rs";
/// Where the statics a test file may not use are declared (paths from this package's dir): the package's src, the
/// crates' src whose state its tests drive, and the rig's root (a file the test files include by #[path] is a copy per
/// module, not the process's).
const STATIC_SRCS: [&str; 2] = ["src", "../driver/src"];
/// The package's or its rig's own calls that change process-wide state (the rig's seam installs or one-per-process
/// fixture, a setter of the package's process-wide state), if it has any.
const RIG_STATE: [&str; 7] = [
    "install_pools(",
    "install_section_arm(",
    "install_mem_spill(",
    "run_spilling(",
    "run_spilling_in(",
    "SPILL_SETS_CREATED",
    "MEM_SPILL",
];

/// What makes a file unable to be a module of the suite: it needs its own crate root ...
const ROOT_ONLY: [&str; 2] = ["#[global_allocator]", "current_exe("];
/// ... or its text says its tests share state across the process ...
const SAYS: [&str; 2] = ["process-wide", "process-global"];
/// ... or its name is one of ct's serial tail (check.sh CHECK_CT_SERIAL: `cancel|timeout|v2udf|av2|^runtime-`): its
/// binary runs alone after the e2e, its bounds are wall-clock time ...
const SERIAL: [&str; 4] = ["cancel", "timeout", "v2udf", "av2"];
/// ... or a code line (not a `//` comment) sets or reads process-wide state through a call: an env var or the working
/// directory, the panic hook, the process's own /proc/self files (what every test maps shows in them), the spill
/// factory or the memory-wait hooks installed, the fingerprint seed, a crate's seams (`init_seams`, or every crate's:
/// seams_init), the arena draws seen per program (`footprint` reads them), the memory ledger, budget, policy or knobs,
/// mcx's ladder, revoke, governor or session cleanup sink, cancelpoll's process switches, fault points, a `Reservation`
/// of its own (what it draws depends on what the process's pool and depots hold), a process census (`*_census()`), the
/// part cache, the data directory, the resident fingerprint tables, IR pins, the refusal intern set, the gate stamps,
/// the kernels' poll, the machine slice or the driver's slot-bind switch.
const PROCESS_STATE: [&str; 40] = [
    "env::set_var",
    "env::remove_var",
    "set_current_dir",
    "panic::set_hook(",
    "panic::take_hook(",
    "/proc/self/",
    "install_spill_factory",
    "set_mem_wait",
    "fp::install(",
    "init_seams",
    "seams_init::",
    "pgtz::set_session_timezone",
    "footprint(",
    "footprint_regions(",
    "forget_arena_draws(",
    "memledger::",
    "budget::",
    "mempolicy::",
    "consts::set(",
    "consts::reset(",
    "mcx::ladder",
    "mcx::revoke",
    "mcx::governor",
    "mcx::global_footprint",
    "set_session_cleanup_sink(",
    "cancelpoll::set_",
    "cancelpoll::probe",
    "fires::",
    "Reservation::",
    "census(",
    "partcache::",
    "arm_part_cache",
    "SetDataDir(",
    "fp_cache_drop_all(",
    "ir_pins",
    "interned()",
    "set_gate_stamps(",
    "set_poll(",
    "set_machine_slice(",
    "set_bind_at_claim(",
];
/// What a static's type names when it can change at run time.
const MUTABLE: [&str; 6] = ["Atomic", "Cell", "Mutex", "RwLock", "Once", "Lazy"];

/// The `pub static` names in `src` (all capitals) that hold state: outside thread_local! blocks (whose statics are per
/// thread), and not a table of plain data (a `&[..]` or `[..; N]` type naming nothing of MUTABLE) or a unit struct's
/// one value (`pub static X: T = T;`).
fn pub_statics(src: &str, out: &mut BTreeSet<String>) {
    let mut depth: i64 = 0; // > 0: inside a thread_local! block, its brace depth
    for l in src.lines() {
        if depth > 0 || l.contains("thread_local!") {
            depth += l.matches('{').count() as i64 - l.matches('}').count() as i64;
            continue;
        }
        let Some(rest) = l.trim_start().strip_prefix("pub static ") else { continue };
        let name: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        let caps = name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        let ty = rest[name.len()..].trim_start().trim_start_matches(':').trim_start();
        let ty = ty.split('=').next().unwrap_or("");
        let table = (ty.starts_with('&') || ty.starts_with('[')) && !MUTABLE.iter().any(|m| ty.contains(m));
        // a unit struct's one value (`pub static X: T = T;`) holds nothing either
        let init = rest.split_once('=').map_or("", |(_, i)| i.trim().trim_end_matches(';').trim());
        let table = table || (!init.is_empty() && init == ty.trim());
        if caps && !table && name.starts_with(|c: char| c.is_ascii_uppercase()) {
            out.insert(name);
        }
    }
}

/// Every .rs file at or under `p`.
fn rs_files(p: &Path, out: &mut Vec<PathBuf>) {
    if p.is_file() {
        out.push(p.to_path_buf());
        return;
    }
    for e in std::fs::read_dir(p).unwrap_or_else(|e| panic!("{}: {e}", p.display())) {
        let p = e.unwrap().path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Why `src` cannot be a module of the suite, if it cannot (`statics`: the process-wide statics it may not use).
fn standalone_reason(file: &str, src: &str, statics: &BTreeSet<String>) -> Option<String> {
    let name = stem(file);
    let serial = SERIAL.iter().copied().find(|p| name.contains(*p));
    let serial = serial.or(name.starts_with("runtime-").then_some("runtime-"));
    if let Some(p) = serial {
        return Some(format!("is named like ct's serial tail (`{p}`: it runs alone, its bounds are wall-clock time)"));
    }
    if let Some(p) = ROOT_ONLY.iter().find(|p| src.contains(**p)) {
        return Some(format!("has `{p}`"));
    }
    if let Some(p) = SAYS.iter().find(|p| src.contains(**p)) {
        return Some(format!("says its tests share `{p}` state"));
    }
    for l in src.lines().map(str::trim).filter(|l| !l.starts_with("//")) {
        if l.contains("_seams::") && l.contains("::set(") {
            return Some(format!("sets a process-wide seam: `{l}`"));
        }
        if let Some(p) = PROCESS_STATE.iter().find(|p| l.contains(**p)) {
            return Some(format!("has `{p}` (process-wide state)"));
        }
        if let Some(p) = RIG_STATE.iter().find(|p| l.contains(**p)) {
            return Some(format!("calls `{p}` (process-wide state)"));
        }
        if let Some(w) = l.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).find(|w| statics.contains(*w)) {
            return Some(format!("uses the process-wide static `{w}`"));
        }
    }
    None
}

fn stem(file: &str) -> &str {
    file.strip_suffix("/main.rs").or_else(|| file.strip_suffix(".rs")).unwrap_or(file)
}

#[test]
fn every_test_file_is_registered() {
    // D793-b: the checkout under test, read at run time
    let me = std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)");
    let me = Path::new(&me);
    let dir = me.join("tests");
    let root_file = format!("{SUITE}.rs");
    let mut files = BTreeSet::new();
    for e in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        if p.is_file() && name.ends_with(".rs") && name != root_file {
            files.insert(name);
        } else if p.is_dir() && p.join("main.rs").is_file() {
            files.insert(format!("{name}/main.rs"));
        }
    }
    let mut bad = Vec::new();

    // the [[test]] entries: (name, file under tests/, built only with a feature)
    let toml = std::fs::read_to_string(me.join("Cargo.toml")).unwrap();
    let mut entries: Vec<(String, Option<String>, bool)> = Vec::new();
    let mut section = "";
    for l in toml.lines().map(str::trim) {
        if l.starts_with('[') {
            section = l;
            if l == "[[test]]" {
                entries.push((String::new(), None, false));
            }
        } else if let Some(e) = entries.last_mut().filter(|_| section == "[[test]]") {
            if let Some(n) = l.strip_prefix("name = \"").and_then(|r| r.strip_suffix('"')) {
                e.0 = n.to_string();
            } else if let Some(f) = l.strip_prefix("path = \"tests/").and_then(|r| r.strip_suffix('"')) {
                e.1 = Some(f.to_string());
            } else if l.starts_with("required-features") {
                e.2 = true;
            }
        }
    }
    let (mut targets, mut gated) = (BTreeMap::new(), BTreeSet::new());
    for (n, f, g) in entries {
        let Some(f) = f else {
            bad.push(format!("Cargo.toml: the [[test]] {n:?} has no `path = \"tests/<file>\"` line"));
            continue;
        };
        if g {
            gated.insert(f.clone());
        }
        targets.insert(f, n);
    }
    if targets.remove(&root_file).as_deref() != Some(SUITE) {
        bad.push(format!("Cargo.toml: no [[test]] name = \"{SUITE}\" with path = \"tests/{root_file}\""));
    }
    for (f, n) in &targets {
        if n != stem(f) {
            bad.push(format!("Cargo.toml: the [[test]] for tests/{f} is named {n:?}; name it {:?} (ct keys by name)", stem(f)));
        }
    }

    // the modules of the suite (file -> module name)
    let root = dir.join(&root_file);
    let text = std::fs::read_to_string(&root).unwrap_or_else(|e| panic!("{}: {e}", root.display()));
    let mut suite = BTreeMap::new();
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    while let Some(l) = lines.next() {
        let Some(f) = l.strip_prefix("#[path = \"").and_then(|r| r.strip_suffix("\"]")) else { continue };
        let m = lines.next().and_then(|m| m.strip_prefix("mod ")).and_then(|m| m.strip_suffix(';')).unwrap_or("");
        if m != format!("t_{}", stem(f)) {
            bad.push(format!("{root_file}: {f} is `mod {m}`; write `mod t_{};` right after its #[path] line", stem(f)));
        }
        if suite.insert(f.to_string(), m.to_string()).is_some() {
            bad.push(format!("{root_file}: {f} is listed twice"));
        }
    }
    if !suite.contains_key(GUARD) {
        bad.push(format!("{root_file}: this guard ({GUARD}) is not its module t_registered"));
    }

    let listed: BTreeSet<String> = suite.keys().chain(targets.keys()).cloned().collect();
    let missing: Vec<_> = files.difference(&listed).collect();
    let gone: Vec<_> = listed.difference(&files).collect();
    if !missing.is_empty() {
        bad.push(format!(
            "not registered: {missing:?}. Add `#[path = \"<file>\"]` and `mod t_<stem>;` to {}, or, if the file cannot be a \
             module (see standalone_reason), `[[test]] name = \"<stem>\" path = \"tests/<file>\"` to {}",
            root.display(),
            me.join("Cargo.toml").display()
        ));
    }
    if !gone.is_empty() {
        bad.push(format!("registered but missing under {}: {gone:?}", dir.display()));
    }
    for f in suite.keys().filter(|f| targets.contains_key(*f)) {
        bad.push(format!("{f} is both a module of {SUITE} and its own [[test]]: keep one"));
    }

    // the process-wide statics
    let mut srcs = Vec::new();
    STATIC_SRCS.iter().for_each(|s| rs_files(&me.join(s), &mut srcs));
    let mut statics = BTreeSet::new();
    for p in &srcs {
        pub_statics(&std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display())), &mut statics);
    }
    for f in files.iter().filter(|f| *f != GUARD && listed.contains(*f)) {
        let src = std::fs::read_to_string(dir.join(f)).unwrap_or_else(|e| panic!("{f}: {e}"));
        match (suite.contains_key(f), standalone_reason(f, &src, &statics)) {
            (true, Some(why)) => bad.push(format!("{f} {why}: it cannot share {SUITE}'s process; give it a [[test]] entry")),
            (false, None) if !gated.contains(f) => bad.push(format!(
                "{f} is its own [[test]] but has none of a name of {SERIAL:?}, {ROOT_ONLY:?}, {SAYS:?}, {PROCESS_STATE:?}, \
                 {RIG_STATE:?}, a seam set, a process-wide static or required-features: make it a module of {SUITE}"
            )),
            _ => {}
        }
    }
    assert!(bad.is_empty(), "{}: tests/ and its test targets disagree:\n  {}", me.display(), bad.join("\n  "));
    assert!(files.len() > 5, "only {} test files under {}: wrong directory?", files.len(), dir.display());
    assert!(suite.len() > 2, "only {} modules in {}: wrong file?", suite.len(), root.display());
    let gone_src: Vec<_> = STATIC_SRCS.iter().filter(|s| !me.join(s).exists()).collect();
    assert!(gone_src.is_empty(), "STATIC_SRCS not found under {}: {gone_src:?}", me.display());
}
