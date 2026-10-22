// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

mod reference_support;

#[test]
fn prepare_x64_writer_reference() {
    reference_support::prepare(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
    println!("\nTPDE_X64_WRITER_REFERENCE_PREPARE PASS cpp_modes=3 encoder_modes=2");
}
