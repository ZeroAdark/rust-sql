
use std::path::Path;
use std::process::Command;

#[test]
fn seam_installs_lint_passes() {
    let repo = manifest_dir()
        .join("../../..")
        .canonicalize()
        .expect("repo root");
    let script = repo.join("crates/_support/seams_init/tests/lint-seam-installs.sh");
    assert!(script.is_file(), "missing {}", script.display());

    let out = Command::new("bash")
        .arg(&script)
        .current_dir(&repo)
        .output()
        .expect("run lint-seam-installs.sh");

    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "lint-seam-installs.sh failed — a seam slot with a ported \
         implementation is not installed by seams_init::init_all() (or an \
         unported seam gained a caller without an allowlist classification):\n\
         {report}"
    );
}

/// D793-b: the checkout under test, read at run time (cargo sets CARGO_MANIFEST_DIR for every test process), never
/// env!()'s path baked in at compile time: a test binary reused from another checkout would read that checkout's files.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
