// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#[path = "../../tests/snapshot.rs"]
mod snapshot;
use std::io::{Read, Write};
fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "analyze".into());
    if !["parse", "records", "print", "adaptor", "analyze"].contains(&mode.as_str()) {
        eprintln!("usage: qualify [parse|records|print|adaptor|analyze] < program.testir");
        std::process::exit(64);
    }
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .expect("read TestIR text");
    let context = mcx::MemoryContext::new_bump("test-ir-component");
    let output = snapshot::run(&mode, &input, context.mcx());
    std::io::stdout()
        .write_all(output.as_bytes())
        .expect("write component result");
    if output.starts_with("parse 0") {
        std::process::exit(1);
    }
}
