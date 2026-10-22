// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

fn main() {
    println!("cargo::rustc-check-cfg=cfg(tpde_address_sanitizer)");
    println!("cargo::rerun-if-env-changed=CARGO_CFG_SANITIZE");
    if std::env::var("CARGO_CFG_SANITIZE")
        .unwrap_or_default()
        .split(',')
        .any(|sanitizer| sanitizer == "address")
    {
        println!("cargo::rustc-cfg=tpde_address_sanitizer");
    }
}
