// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[test]
fn actual_runtime_matches_pinned_c() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let rustflags = std::env::var("CARGO_ENCODED_RUSTFLAGS")
        .ok()
        .filter(|flags| !flags.is_empty())
        .unwrap_or_else(|| std::env::var("RUSTFLAGS").unwrap_or_default());
    let work = std::env::var_os("COMPONENT_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target/oracle"))
        .join("fadec-runtime")
        .join(if rustflags.contains("sanitizer=address") {
            "asan"
        } else {
            "regular"
        });
    fs::create_dir_all(&work).unwrap();
    let target_directory = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .expect("the runtime oracle requires Cargo's target directory");
    let mut command = Command::new("python3");
    command
        .arg(root.join("tests/oracle.py"))
        .arg("--work")
        .arg(&work)
        .arg("--target-directory")
        .arg(target_directory)
        .arg("--build-directory")
        .arg(env!("OUT_DIR"))
        .env("PYTHONDONTWRITEBYTECODE", "1");
    let result = command.output().expect("the C oracle requires Python");
    fs::write(work.join("oracle.stdout"), &result.stdout).unwrap();
    fs::write(work.join("oracle.stderr"), &result.stderr).unwrap();
    eprint!("{}", String::from_utf8_lossy(&result.stdout));
    assert!(
        result.status.success(),
        "actual runtime oracle failed: {}\n{}",
        result.status,
        String::from_utf8_lossy(&result.stderr)
    );
    let verdict = fs::read_to_string(work.join("verdict.txt")).unwrap();
    assert!(verdict.starts_with("TPDE_FADEC_RUNTIME_ORACLE PASS "));
}
