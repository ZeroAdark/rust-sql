// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

#[allow(dead_code)]
mod common;

#[test]
fn compile_every_pinned_reference_and_publish_verified_receipt() {
    let directory = common::reference_directory();
    std::fs::create_dir_all(&directory).unwrap();
    assert_eq!(common::build_references(&directory).len(), 8);
    common::build_unwind_libraries(&directory);
    common::receipt(&directory, "write");
    common::receipt(&directory, "verify");
    let provenance = directory.join("a64-assert-native-compile.txt");
    let original = std::fs::read(&provenance).unwrap();
    let mut changed = original.clone();
    changed.extend_from_slice(b"\npreflight-stale-provenance-control\n");
    std::fs::write(&provenance, changed).unwrap();
    let mut command =
        std::process::Command::new(std::env::var_os("PYTHON").unwrap_or_else(|| "python3".into()));
    command
        .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/reference/receipt.py"))
        .arg("--directory")
        .arg(&directory)
        .args(["--mode", "verify"]);
    let result = command.output();
    std::fs::write(&provenance, original).unwrap();
    let output = result.unwrap();
    std::fs::write(
        directory.join("receipt-negative-stdout.bin"),
        &output.stdout,
    )
    .unwrap();
    std::fs::write(
        directory.join("receipt-negative-stderr.txt"),
        &output.stderr,
    )
    .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.lines().any(|line| line == "RuntimeError: stale or corrupt reference receipt: receipt.artifacts.a64-assert-native-compile.txt.sha256"),
        "{stderr}"
    );
    common::receipt(&directory, "verify");
    println!("\nTPDE_ELF_MAPPER_REFERENCE_CACHE PASS stale_refusals=1 restored_verifications=1");
    println!("\nTPDE_ELF_MAPPER_REFERENCE_READY PASS cpp_modes=8 unwind_variants=3");
}
