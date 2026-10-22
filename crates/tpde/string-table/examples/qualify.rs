// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

fn main() {
    let status = std::process::Command::new("python3")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/qualify.py"))
        .status()
        .expect("start the qualification runner");
    std::process::exit(status.code().unwrap_or(1));
}
