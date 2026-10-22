//! D793: the lints scan the checkout under test, read at run time (`CARGO_MANIFEST_DIR` as cargo sets it for the test
//! process), never the compile-time path baked into the binary: a lint binary reused from another checkout's build (a kache
//! hit, a shared target dir) scanned that other tree and passed (kernel_boundary on M6-HOSTS's H2, 09-26).
use std::path::{Path, PathBuf};

use gravity_lint::{gravity_root, lint_dir, root_from};

/// A scratch directory holding a Cargo.toml for `package`.
fn crate_dir(tag: &str, package: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gravity-lint-d793-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("Cargo.toml"), format!("[package]\nname = \"{package}\"\n")).unwrap();
    d
}

#[test]
fn a_reused_binary_scans_the_checkout_it_runs_in() {
    let here = crate_dir("here", "gravity_lint");
    // built in another checkout, run in this one: this one
    assert_eq!(root_from("/elsewhere/crates/gravity/lint", Some(&here)).unwrap(), here);
    // no run-time directory, or one that is not gravity_lint's: a loud error naming the build's path
    let e = root_from("/elsewhere/crates/gravity/lint", None).unwrap_err();
    assert!(e.contains("/elsewhere/crates/gravity/lint"), "{e}");
    let other = crate_dir("other", "gravity_kernels");
    assert!(root_from("/elsewhere/crates/gravity/lint", Some(&other)).is_err());
    let _ = std::fs::remove_dir_all(here);
    let _ = std::fs::remove_dir_all(other);
}

#[test]
fn the_root_is_the_run_time_manifest_dir() {
    let run = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from).expect("cargo sets CARGO_MANIFEST_DIR for tests");
    assert_eq!(lint_dir(), run);
    assert_eq!(gravity_root(), run.parent().unwrap());
    assert!(Path::new(&gravity_root()).join("kernels/Cargo.toml").is_file());
}
