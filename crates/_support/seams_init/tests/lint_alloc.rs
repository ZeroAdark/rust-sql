
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    // D793-b: the checkout under test, read at run time, never the compile-time path
    PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset: run under cargo test"))
        .join("../../..")
        .canonicalize()
        .expect("repo root")
}

fn run_lint(tree: Option<&std::path::Path>, allowlist: Option<&std::path::Path>) -> (bool, String) {
    let repo = repo_root();
    let script = repo.join("crates/_support/seams_init/tests/lint-alloc.sh");
    assert!(script.is_file(), "missing {}", script.display());
    let mut cmd = Command::new("bash");
    cmd.arg(&script).current_dir(&repo);
    if let Some(t) = tree {
        cmd.env("LINT_ALLOC_TREE", t);
    }
    if let Some(a) = allowlist {
        cmd.env("LINT_ALLOC_ALLOWLIST", a);
    }
    let out = cmd.output().expect("run lint-alloc.sh");
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), report)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lint-alloc-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn synthetic(dir: &std::path::Path, src: &str) -> (PathBuf, PathBuf) {
    let src_dir = dir.join("tree/crates/backend/synthetic/src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(src_dir.join("lib.rs"), src).unwrap();
    let allow = dir.join("empty.allow");
    fs::write(&allow, "# empty\n").unwrap();
    (dir.join("tree"), allow)
}

#[test]
fn alloc_lint_passes() {
    let (ok, report) = run_lint(None, None);
    assert!(
        ok,
        "lint-alloc.sh failed — an infallible std-allocation site was added \
         outside the ratchet ledger (or a budget grew):\n{report}"
    );
    assert!(report.contains("lint-alloc PASS"), "expected PASS marker in report:\n{report}");
}

#[test]
fn new_sites_fail_per_category() {
    let dir = scratch("newsite");
    let (tree, allow) = synthetic(
        &dir,
        "pub fn shapes(n: usize, s: &[u8]) -> Vec<u8> {\n\
         \x20   let mut v: Vec<u8> = Vec::with_capacity(n);\n\
         \x20   let w = vec![0u8; n];\n\
         \x20   v.reserve(w.len());\n\
         \x20   v.resize(n, 0);\n\
         \x20   let c = s.iter().copied().collect::<Vec<u8>>();\n\
         \x20   let d = s.to_vec();\n\
         \x20   v.extend(c); v.extend(d); v\n\
         }\n",
    );
    let (ok, report) = run_lint(Some(&tree), Some(&allow));
    assert!(!ok, "lint must fail on unallowlisted std-allocation sites:\n{report}");
    for want in [
        "VIOLATION(new-site): [cap] crates/backend/synthetic/src/lib.rs carries 2",
        "VIOLATION(new-site): [reserve] crates/backend/synthetic/src/lib.rs carries 2",
        "VIOLATION(new-site): [collect] crates/backend/synthetic/src/lib.rs carries 2",
    ] {
        assert!(report.contains(want), "missing {want:?} in:\n{report}");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn budget_growth_fails_ratchet() {
    let dir = scratch("ratchet");
    let (tree, _) = synthetic(
        &dir,
        "pub fn two(n: usize) -> Vec<u8> {\n\
         \x20   let mut v: Vec<u8> = Vec::with_capacity(n);\n\
         \x20   let w: Vec<u8> = Vec::with_capacity(n * 2);\n\
         \x20   v.extend(w); v\n\
         }\n",
    );
    let allow = dir.join("one.allow");
    fs::write(&allow, "cap\tcrates/backend/synthetic/src/lib.rs\t1\n").unwrap();
    let (ok, report) = run_lint(Some(&tree), Some(&allow));
    assert!(!ok, "lint must fail when sites exceed the budget:\n{report}");
    assert!(
        report.contains("VIOLATION(ratchet): [cap] crates/backend/synthetic/src/lib.rs"),
        "ratchet violation must name category and file:\n{report}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sanctioned_idioms_and_constants_are_clean() {
    let dir = scratch("clean");
    let (tree, allow) = synthetic(
        &dir,
        "const CAP: usize = 8;\n\
         pub fn clean(mcx: ::mcx::Mcx<'_>, n: usize) -> Result<(), ()> {\n\
         \x20   let a: Vec<u8> = Vec::with_capacity(64);\n\
         \x20   let b = vec![0u8; CAP];\n\
         \x20   let c = vec![0u8; ::types_core::BLCKSZ];\n\
         \x20   let d: Vec<u8> = Vec::with_capacity(2 * CAP + core::mem::size_of::<u64>());\n\
         \x20   let e = vec![1, 2, 3];\n\
         \x20   let mut f: Vec<u8> = Vec::new();\n\
         \x20   f.try_reserve(n).map_err(|_| ())?;\n\
         \x20   f.resize(CAP, 0);\n\
         \x20   let g = ::mcx::vec_with_capacity_in::<u8>(mcx, n).map_err(|_| ())?;\n\
         \x20   let h = NodeList::with_capacity(mcx, n)?;\n\
         \x20   let i = vec![\n\
         \x20       1u8,\n\
         \x20       2u8,\n\
         \x20   ];\n\
         \x20   let _ = (a, b, c, d, e, f, g, h, i);\n\
         \x20   Ok(())\n\
         }\n\
         #[cfg(test)]\n\
         mod tests { pub fn t(n: usize) -> Vec<u8> { Vec::with_capacity(n) } }\n",
    );
    let (ok, report) = run_lint(Some(&tree), Some(&allow));
    assert!(ok, "sanctioned idioms and constant capacities must be clean:\n{report}");
    assert!(!report.contains("VIOLATION"), "no violation expected:\n{report}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn stale_rows_warn_and_review_markers_are_checked() {
    let dir = scratch("markers");
    let (tree, _) = synthetic(
        &dir,
        "pub fn one(n: usize) -> Vec<u8> { Vec::with_capacity(n) }\n",
    );
    let allow = dir.join("marked.allow");
    fs::write(
        &allow,
        "cap\tcrates/backend/synthetic/src/lib.rs\t1\tALLOC-REVIEW(test): deliberate exception\n\
         reserve\tcrates/backend/synthetic/src/lib.rs\t1\n",
    )
    .unwrap();
    let (ok, report) = run_lint(Some(&tree), Some(&allow));
    assert!(ok, "a marked row within budget passes; a stale row only warns:\n{report}");
    assert!(
        report.contains("NOTE(review-marker): [cap] crates/backend/synthetic/src/lib.rs"),
        "marker rows must surface as NOTEs:\n{report}"
    );
    assert!(
        report.contains("WARN(stale-allowlist): [reserve] crates/backend/synthetic/src/lib.rs"),
        "stale row must be warned about:\n{report}"
    );

    let bad = dir.join("bad.allow");
    fs::write(&bad, "cap\tcrates/backend/synthetic/src/lib.rs\t1\tALLOC-REVIEW no-who\n").unwrap();
    let (ok, report) = run_lint(Some(&tree), Some(&bad));
    assert!(!ok && report.contains("CONFIG-FAIL"), "malformed marker is a CONFIG-FAIL:\n{report}");
    let _ = fs::remove_dir_all(&dir);
}
