
use std::path::Path;
use std::process::Command;

#[test]
fn walker_arms_lint_passes() {
    let repo = manifest_dir()
        .join("../../..")
        .canonicalize()
        .expect("repo root");
    let script = repo.join("crates/_support/seams_init/tests/lint-walker-arms.sh");
    assert!(script.is_file(), "missing {}", script.display());
    // The lint drives internal/tools/walker-arms/walker_arms.py, an internal
    // tree beside the public workspace (PGRUST_INTERNAL_TOOLS overrides it) that
    // the public release tree does not carry; there the census cannot run at
    // all, so skip loudly instead of failing `cargo test --workspace` for users.
    let tools = std::env::var("PGRUST_INTERNAL_TOOLS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| repo.join("../internal/tools"));
    let tool = tools.join("walker-arms/walker_arms.py");
    if !tool.is_file() {
        eprintln!("walker_arms_lint_passes: SKIP — {} not present (public tree)", tool.display());
        return;
    }

    let out = Command::new("bash")
        .arg(&script)
        .current_dir(&repo)
        .output()
        .expect("run lint-walker-arms.sh");

    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "lint-walker-arms.sh failed — a ported dispatch function lacks an arm \
         its C counterpart has (port the arm C-exactly, or adjudicate the row \
         in crates/_support/seams_init/tests/lint-walker-arms.allow):\n{report}"
    );
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
