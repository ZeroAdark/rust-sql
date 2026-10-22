// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

#[path = "../../test-support/oracle.rs"]
mod oracle;
mod raw_cases;
mod raw_oracle_build;
mod raw_support;

use raw_cases::Case;
use raw_support::{run_abi, run_raw, Frontend};
use std::{cell::RefCell, fs, os::unix::process::ExitStatusExt};
use tpde_assembler::Architecture;
use tpde_compiler::with_compiler_base;
use tpde_compiler_a64::CompilerA64;
use tpde_core::ir_adaptor::IrAdaptor;
use tpde_function_writer_a64::A64Target;
use tpde_test_ir::{TestIR, TestIRAdaptor};

#[test]
fn complete_raw_and_aapcs_output_matches_pinned_compiler() {
    let work = raw_oracle_build::work_directory();
    let references = raw_oracle_build::require_references(&work);
    let cases = raw_cases::cases();
    assert_eq!(cases.len(), 28);
    let mut roster = String::from("case\tmode\toperations\tverdict\n");
    let mut comparisons = 0;
    let mut operations = 0;
    for case in &cases {
        operations += case.operations.len();
        let expected = if case.mode == 0 {
            run_raw(case)
        } else {
            run_abi(case)
        };
        let input = raw_cases::encode(case);
        for reference in &references {
            let name = format!("{}-{}", case.name, reference.name);
            oracle::compare_bytes(&reference.executable, &work, &input, &expected, &name);
            roster.push_str(&format!(
                "{}\t{}\t{}\tPASS\n",
                case.name,
                reference.name,
                case.operations.len()
            ));
            comparisons += 1;
        }
    }
    fs::write(work.join("raw-roster.tsv"), roster).unwrap();
    assert_eq!(operations, 826);
    assert_eq!(comparisons, 84);
    let mut diagnostics = 0;
    for (name, operation) in [
        ("zero-width-insert", raw_cases::op(9, &[0, 1, 0, 0])),
        ("out-of-range-insert", raw_cases::op(10, &[0, 1, 64, 1])),
        ("zero-width-extension", raw_cases::op(8, &[0, 1, 0, 0, 64])),
    ] {
        let case = Case {
            name,
            mode: 0,
            operations: vec![operation],
        };
        let input = raw_cases::encode(&case);
        for reference in &references {
            let prefix = format!("{name}-{}", reference.name);
            let result = oracle::run_reference(&reference.executable, &work, &input, &prefix);
            if reference.assertions {
                let stderr = String::from_utf8_lossy(&result.stderr);
                assert_eq!(result.status.signal(), Some(6), "{prefix}: {stderr}");
                assert!(
                    stderr.contains("FunctionWriterA64.hpp")
                        && stderr.contains("Assertion")
                        && stderr.contains("inst != 0")
                        && stderr.contains("failed")
                        && !stderr.contains("ERROR: AddressSanitizer")
                        && !stderr.contains("runtime error:"),
                    "{prefix}: unexpected refusal: {stderr}"
                );
            } else {
                assert_eq!(result.status.code(), Some(0), "{prefix}");
                assert!(
                    !result.stdout.is_empty(),
                    "{prefix}: missing diagnostic output"
                );
            }
            diagnostics += 1;
        }
    }
    assert_eq!(diagnostics, 9);
    println!(
        "\nTPDE_COMPILER_A64_RAW PASS cases={} operations={} comparisons={} reuse_compilations=54 diagnostics=9",
        cases.len(),
        operations,
        comparisons
    );
}

#[test]
fn invalid_raw_operands_abandon_output_then_allow_compiler_reuse() {
    use raw_cases::op;
    let invalid = [
        vec![op(1, &[31, 9, 0, 8, 0])],
        vec![op(1, &[0, 9, 0, 16, 0])],
        vec![op(1, &[32, 9, 0, 8, 1])],
        vec![op(2, &[9, 0, 32, 3])],
        vec![op(3, &[31, 0, 8])],
        vec![op(3, &[0, 32, 16])],
        vec![op(4, &[2, 8, 0, 0, 0])],
        vec![op(4, &[0, 16, 0, 0, 0])],
        vec![op(8, &[0, 1, 0, 32, 32])],
        vec![op(8, &[0, 1, 0, 0, 64])],
        vec![op(9, &[0, 1, 0, 0])],
        vec![op(10, &[0, 1, 64, 1])],
        vec![op(12, &[]), op(11, &[15, 31, 0, 0, 0])],
        vec![op(12, &[]), op(11, &[17, 0, 0, 64, 0])],
    ];
    let valid = [op(4, &[0, 8, 0, 0x1234_5678, 0])];
    for (index, script) in invalid.iter().enumerate() {
        let context = mcx::MemoryContext::new_bump("A64 raw invalid operands");
        let mut ir = TestIR::new(context.mcx());
        ir.parse_ir("raw() {\nentry:\nterminate\n}\n").unwrap();
        let output = RefCell::new(Vec::new());
        with_compiler_base(
            context.mcx(),
            TestIRAdaptor::new(&ir),
            CompilerA64::new(
                context.mcx(),
                Frontend {
                    script,
                    output: &output,
                },
                0,
            ),
            A64Target::new(context.mcx()),
            Architecture::Aarch64,
            |mut compiler| {
                compiler.adaptor.start_compile();
                assert!(compiler.compile().is_err(), "invalid raw case {index}");
                compiler.adaptor.end_compile();
                assert!(compiler.assembler().is_err());
                compiler.reset().unwrap();
                assert_eq!(compiler.reset_state_counts(), (0, 0, 0));
                compiler.target_mut().frontend.script = &valid;
                compiler.adaptor.start_compile();
                assert!(
                    compiler.compile().unwrap(),
                    "reuse after invalid raw case {index}"
                );
                compiler.adaptor.end_compile();
                assert!(!compiler
                    .assembler()
                    .unwrap()
                    .build_object_file()
                    .unwrap()
                    .is_empty());
            },
        )
        .unwrap();
    }
}
