
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    manifest_dir().to_path_buf()
        .join("../../..")
        .canonicalize()
        .expect("repo root")
}

fn run_lint(tree: Option<&std::path::Path>, allowlist: Option<&std::path::Path>) -> (bool, String) {
    let repo = repo_root();
    let script = repo.join("crates/_support/seams_init/tests/lint-determinism.sh");
    assert!(script.is_file(), "missing {}", script.display());
    let mut cmd = Command::new("bash");
    cmd.arg(&script).current_dir(&repo);
    if let Some(t) = tree {
        cmd.env("LINT_DETERMINISM_TREE", t);
    }
    if let Some(a) = allowlist {
        cmd.env("LINT_DETERMINISM_ALLOWLIST", a);
    }
    let out = cmd.output().expect("run lint-determinism.sh");
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), report)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lint-determinism-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

#[test]
fn determinism_lint_passes() {
    let (ok, report) = run_lint(None, None);
    assert!(
        ok,
        "lint-determinism.sh failed — a raw nondeterminism site was added \
         outside the ratchet ledger (or a budget grew):\n{report}"
    );
    assert!(
        report.contains("lint-determinism PASS"),
        "expected PASS marker in report:\n{report}"
    );
}

#[test]
fn new_raw_fs_site_fails_named() {
    let dir = scratch("newsite");
    let src_dir = dir.join("tree/crates/backend/synthetic/src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(
        src_dir.join("lib.rs"),
        "pub fn leak() -> String {\n    std::fs::read_to_string(\"/etc/hostname\").unwrap_or_default()\n}\n",
    )
    .unwrap();
    let allow = dir.join("empty.allow");
    fs::write(&allow, "# empty\n").unwrap();

    let (ok, report) = run_lint(Some(&dir.join("tree")), Some(&allow));
    assert!(!ok, "lint must fail on an unallowlisted raw fs site:\n{report}");
    assert!(
        report.contains("VIOLATION(new-site): [fs] crates/backend/synthetic/src/lib.rs"),
        "violation must name category and file:\n{report}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn budget_growth_fails_ratchet() {
    let dir = scratch("ratchet");
    let src_dir = dir.join("tree/crates/backend/synthetic/src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(
        src_dir.join("lib.rs"),
        "pub fn leak() -> String {\n    let _ = std::fs::metadata(\"/tmp\");\n    std::fs::read_to_string(\"/etc/hostname\").unwrap_or_default()\n}\n",
    )
    .unwrap();
    let allow = dir.join("one.allow");
    fs::write(&allow, "fs\tcrates/backend/synthetic/src/lib.rs\t1\n").unwrap();

    let (ok, report) = run_lint(Some(&dir.join("tree")), Some(&allow));
    assert!(!ok, "lint must fail when sites exceed the budget:\n{report}");
    assert!(
        report.contains("VIOLATION(ratchet): [fs] crates/backend/synthetic/src/lib.rs"),
        "ratchet violation must name category and file:\n{report}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn removed_site_warns_stale() {
    let dir = scratch("stale");
    let src_dir = dir.join("tree/crates/backend/synthetic/src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(src_dir.join("lib.rs"), "pub fn clean() -> u32 { 42 }\n").unwrap();
    let allow = dir.join("stale.allow");
    fs::write(&allow, "fs\tcrates/backend/synthetic/src/lib.rs\t2\n").unwrap();

    let (ok, report) = run_lint(Some(&dir.join("tree")), Some(&allow));
    assert!(ok, "stale rows warn, they do not fail:\n{report}");
    assert!(
        report.contains("WARN(stale-allowlist): [fs] crates/backend/synthetic/src/lib.rs"),
        "stale row must be warned about:\n{report}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn libc_surface_fails_per_category() {
    let dir = scratch("libc");
    let src_dir = dir.join("tree/crates/backend/synthetic/src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(
        src_dir.join("lib.rs"),
        "pub fn shapes() {\n\
         \x20   unsafe { libc::open(core::ptr::null(), 0) };\n\
         \x20   unsafe { libc::pread(0, core::ptr::null_mut(), 0, 0) };\n\
         \x20   unsafe { libc::clock_gettime(0, core::ptr::null_mut()) };\n\
         \x20   unsafe { libc::nanosleep(core::ptr::null(), core::ptr::null_mut()) };\n\
         }\n",
    )
    .unwrap();
    let allow = dir.join("empty.allow");
    fs::write(&allow, "# empty\n").unwrap();

    let (ok, report) = run_lint(Some(&dir.join("tree")), Some(&allow));
    assert!(!ok, "libc fs/time/blocking sites must fail:\n{report}");
    for want in [
        "VIOLATION(new-site): [fs] crates/backend/synthetic/src/lib.rs carries 2",
        "VIOLATION(new-site): [time] crates/backend/synthetic/src/lib.rs",
        "VIOLATION(new-site): [blocking] crates/backend/synthetic/src/lib.rs",
    ] {
        assert!(report.contains(want), "missing {want:?} in:\n{report}");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn import_line_and_sleep_evasions_fail() {
    let dir = scratch("imports");
    let src_dir = dir.join("tree/crates/backend/synthetic/src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(
        src_dir.join("lib.rs"),
        "use std::fs::{rename, copy};\n\
         use std::env::var as getenv;\n\
         pub fn nap() { std::thread::sleep(std::time::Duration::from_millis(1)); }\n",
    )
    .unwrap();
    let allow = dir.join("empty.allow");
    fs::write(&allow, "# empty\n").unwrap();

    let (ok, report) = run_lint(Some(&dir.join("tree")), Some(&allow));
    assert!(!ok, "import-line/sleep evasions must fail:\n{report}");
    for want in [
        "VIOLATION(new-site): [fs] crates/backend/synthetic/src/lib.rs",
        "VIOLATION(new-site): [env] crates/backend/synthetic/src/lib.rs",
        "VIOLATION(new-site): [blocking] crates/backend/synthetic/src/lib.rs",
    ] {
        assert!(report.contains(want), "missing {want:?} in:\n{report}");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn widened_arms_barrier_join_once_scope_fail() {
    let dir = scratch("widened");
    let src_dir = dir.join("tree/crates/backend/synthetic/src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(
        src_dir.join("lib.rs"),
        "use std::sync::{Arc, Barrier, OnceLock};\n\
         static KNOB: OnceLock<bool> = OnceLock::new();\n\
         pub fn gang(b: &Barrier) {\n\
         \x20   let _ = KNOB.get_or_init(|| true);\n\
         \x20   let h = std::thread::spawn(|| ());\n\
         \x20   let _ = h.join();\n\
         \x20   std::thread::scope(|s| { s.spawn(|| { b.wait(); }); });\n\
         }\n",
    )
    .unwrap();
    let allow = dir.join("empty.allow");
    fs::write(&allow, "# empty\n").unwrap();

    let (ok, report) = run_lint(Some(&dir.join("tree")), Some(&allow));
    assert!(!ok, "widened-arm sites must fail:\n{report}");
    for want in [
        "VIOLATION(new-site): [barrier] crates/backend/synthetic/src/lib.rs",
        "VIOLATION(new-site): [join] crates/backend/synthetic/src/lib.rs",
        "VIOLATION(new-site): [once] crates/backend/synthetic/src/lib.rs",
        "VIOLATION(new-site): [scope] crates/backend/synthetic/src/lib.rs",
        "VIOLATION(new-site): [spawn] crates/backend/synthetic/src/lib.rs",
    ] {
        assert!(report.contains(want), "missing {want:?} in:\n{report}");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn comment_tokens_inside_strings_do_not_hide_sites() {
    let dir = scratch("strlit");
    let src_dir = dir.join("tree/crates/backend/synthetic/src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(
        src_dir.join("lib.rs"),
        "pub fn leak() -> (&'static str, String) {\n\
         \x20   (\"http://x\", std::fs::read_to_string(\"/y\").unwrap_or_default())\n\
         }\n",
    )
    .unwrap();
    let allow = dir.join("empty.allow");
    fs::write(&allow, "# empty\n").unwrap();

    let (ok, report) = run_lint(Some(&dir.join("tree")), Some(&allow));
    assert!(!ok, "fs site after a string-literal // must still be seen:\n{report}");
    assert!(
        report.contains("VIOLATION(new-site): [fs] crates/backend/synthetic/src/lib.rs"),
        "string-aware scan must catch the site:\n{report}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cfg_edges_and_literal_poison_handled() {
    let dir = scratch("cfgedges");
    let src_dir = dir.join("tree/crates/backend/synthetic/src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(
        src_dir.join("any_feature.rs"),
        "#[cfg(any(feature = \"x\", test))]\n\
         mod helpers { pub fn t() { let _ = std::fs::read_to_string(\"/a\"); } }\n",
    )
    .unwrap();
    fs::write(
        src_dir.join("lib.rs"),
        "mod any_feature;\nmod nottest;\nmod charlit;\n",
    )
    .unwrap();
    fs::write(
        src_dir.join("nottest.rs"),
        "#[cfg(not(test))]\npub fn prod() { let _ = std::fs::read_to_string(\"/b\"); }\n",
    )
    .unwrap();
    fs::write(
        src_dir.join("charlit.rs"),
        "#[cfg(test)]\nmod tests {\n    fn f(c: char) -> bool { c == '{' }\n    const B: &[u8] = b\"/*\";\n}\n\
         pub fn after() { let _ = std::fs::read_to_string(\"/c\"); }\n",
    )
    .unwrap();
    let allow = dir.join("empty.allow");
    fs::write(&allow, "# empty\n").unwrap();

    let (ok, report) = run_lint(Some(&dir.join("tree")), Some(&allow));
    assert!(!ok, "nottest.rs and charlit.rs must violate:\n{report}");
    assert!(
        !report.contains("any_feature.rs"),
        "cfg(any(feature, test)) code must be stripped:\n{report}"
    );
    assert!(
        report.contains("VIOLATION(new-site): [fs] crates/backend/synthetic/src/nottest.rs"),
        "cfg(not(test)) is production code:\n{report}"
    );
    assert!(
        report.contains("VIOLATION(new-site): [fs] crates/backend/synthetic/src/charlit.rs"),
        "literals inside the test mod must not poison the prod tail:\n{report}"
    );
    assert!(!report.contains("WARN(scan-poison)"), "no poison expected:\n{report}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn eof_poison_warns_but_passes() {
    let dir = scratch("poison");
    let src_dir = dir.join("tree/crates/backend/synthetic/src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(src_dir.join("lib.rs"), "pub fn fine() {}\n/* unclosed at EOF\n").unwrap();
    let allow = dir.join("empty.allow");
    fs::write(&allow, "# empty\n").unwrap();

    let (ok, report) = run_lint(Some(&dir.join("tree")), Some(&allow));
    assert!(ok, "poison is a WARN, not a gate:\n{report}");
    assert!(
        report.contains("WARN(scan-poison): crates/backend/synthetic/src/lib.rs"),
        "EOF poison must be diagnosed:\n{report}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn review_marker_row_accepted_and_noted() {
    let dir = scratch("review");
    let src_dir = dir.join("tree/crates/backend/synthetic/src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(
        src_dir.join("lib.rs"),
        "pub fn leak() -> String {\n    std::fs::read_to_string(\"/etc/hostname\").unwrap_or_default()\n}\n",
    )
    .unwrap();
    let allow = dir.join("review.allow");
    fs::write(
        &allow,
        "fs\tcrates/backend/synthetic/src/lib.rs\t1\tDST-REVIEW(test): deliberate exception\n",
    )
    .unwrap();

    let (ok, report) = run_lint(Some(&dir.join("tree")), Some(&allow));
    assert!(ok, "a marked row within budget passes:\n{report}");
    assert!(
        report.contains("NOTE(review-marker): [fs] crates/backend/synthetic/src/lib.rs"),
        "marker rows must surface as NOTEs:\n{report}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// POSITIVE: the sanctioned routes the D238 fix moved Gravity onto are
/// NOT flagged, so converting a site never needs a ledger row: pgsync lock /
/// once types, pg_clock MonoStamp / Deadline (elapsed_ns, not the name-flagged
/// `.elapsed(`), the waiter's sleep, pg_strong_random, the fd layer's whole-file
/// helpers and the VFS's read_dir. And test support under a `test/` directory
/// (FixturePool's home behind the fixture-pool feature) is outside the scan.
#[test]
fn sanctioned_routes_and_test_dirs_are_clean() {
    let dir = scratch("sanctioned");
    let src_dir = dir.join("tree/crates/gravity/synthetic/src");
    fs::create_dir_all(src_dir.join("test")).unwrap();
    fs::write(
        src_dir.join("lib.rs"),
        "use pgsync::{Mutex, OnceLock, RwLock};\n\
         use pg_clock::MonoStamp;\n\
         static M: Mutex<u8> = Mutex::new(0);\n\
         static R: RwLock<u8> = RwLock::new(0);\n\
         static O: OnceLock<u8> = OnceLock::new();\n\
         pub fn routes(dir: &std::ffi::CStr) -> u64 {\n\
         \x20   let t0 = MonoStamp::now();\n\
         \x20   let deadline = pg_clock::Deadline::after(core::time::Duration::from_millis(2));\n\
         \x20   while !deadline.expired() { waiter::sleep(core::time::Duration::from_millis(2)); }\n\
         \x20   let mut kb = [0u8; 16];\n\
         \x20   let _ = pg_strong_random::pg_strong_random(&mut kb);\n\
         \x20   let _ = fd::sync::read_whole_file(\"global/seed\");\n\
         \x20   let _ = fd::sync::write_whole_file(\"global/seed.tmp\", &kb, false);\n\
         \x20   let _ = fd::sync::pg_rename(\"global/seed.tmp\", \"global/seed\");\n\
         \x20   let _ = vfs::read_dir(dir);\n\
         \x20   let _ = (M.lock(), R.read(), O.set(1));\n\
         \x20   t0.elapsed_ns()\n\
         }\n",
    )
    .unwrap();
    fs::write(
        src_dir.join("test/fixture.rs"),
        "pub fn raw() { let h = std::thread::spawn(|| std::thread::sleep(std::time::Duration::from_millis(1))); let _ = h.join(); }\n",
    )
    .unwrap();
    let allow = dir.join("empty.allow");
    fs::write(&allow, "# empty\n").unwrap();

    let (ok, report) = run_lint(Some(&dir.join("tree")), Some(&allow));
    assert!(ok, "a sanctioned route or a test/ file was flagged:\n{report}");
    assert!(report.contains("lint-determinism PASS"), "expected PASS marker:\n{report}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn env_group_import_arm_is_std_env_only() {
    let dir = scratch("envarm");
    let tree = dir.join("tree/crates/gravity");
    for (krate, body) in [
        ("synthetic_crate", "use crate::frame::env::{AggRow, Env};\nuse super::env::*;\n"),
        ("synthetic_std", "use std::env::{var, vars};\n"),
        ("synthetic_bare", "use std::env;\nuse env::{var as getenv};\n"),
        ("synthetic_nested", "use std::{env::{var_os}};\n"),
    ] {
        fs::create_dir_all(tree.join(krate).join("src")).unwrap();
        fs::write(tree.join(krate).join("src/lib.rs"), body).unwrap();
    }
    let allow = dir.join("empty.allow");
    fs::write(&allow, "# empty\n").unwrap();

    let (ok, report) = run_lint(Some(&dir.join("tree")), Some(&allow));
    assert!(!ok, "std's env group imports must fail:\n{report}");
    for want in ["synthetic_std", "synthetic_bare", "synthetic_nested"] {
        let want = format!("VIOLATION(new-site): [env] crates/gravity/{want}/src/lib.rs");
        assert!(report.contains(&want), "missing {want:?} in:\n{report}");
    }
    assert!(!report.contains("crates/gravity/synthetic_crate/"), "a crate-path env module was flagged:\n{report}");
    let _ = fs::remove_dir_all(&dir);
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
