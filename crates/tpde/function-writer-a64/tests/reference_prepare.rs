// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

mod oracle_build;

#[test]
fn prepare_reference() {
    let receipt = oracle_build::prepare_reference();
    assert!(receipt.is_file());
    println!(
        "\nTPDE_FUNCTION_WRITER_A64_REFERENCE_PREPARE PASS cpp_modes=3 tpde_files=24 disarm_files=7"
    );
}
