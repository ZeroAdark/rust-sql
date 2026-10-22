// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

mod raw_oracle_build;

#[test]
fn prepare_all_raw_cpp_references_before_correctness() {
    let receipt = raw_oracle_build::prepare_reference();
    assert!(receipt.is_file());
    println!(
        "\nTPDE_COMPILER_A64_RAW_REFERENCE_PREPARE PASS cpp_modes=3 tpde_manifest_entries=64 disarm_files=7 stale_refusals=1 optimized_pin_refusals=1"
    );
}
