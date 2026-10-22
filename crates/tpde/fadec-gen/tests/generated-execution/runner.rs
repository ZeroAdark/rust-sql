// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use std::fs;
use std::path::Path;
use std::process::Command;

pub fn run(root: &Path, work: &Path) -> usize {
    let output = Command::new("python3")
        .arg(root.join("tests/generated_execution.py"))
        .arg("--work")
        .arg(work)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("generated execution harness requires Python");
    fs::write(work.join("generated-execution.stdout"), &output.stdout).unwrap();
    fs::write(work.join("generated-execution.stderr"), &output.stderr).unwrap();
    assert!(
        output.status.success(),
        "generated execution failed: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    eprint!("{}", String::from_utf8_lossy(&output.stdout));
    fs::read_to_string(work.join("generated-execution/case-count.txt"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}
