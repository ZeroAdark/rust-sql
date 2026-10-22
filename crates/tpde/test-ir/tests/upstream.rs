// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

mod cases;
#[path = "../../test-support/oracle.rs"]
mod oracle;
mod snapshot;
use mcx::MemoryContext;
use std::{fs, path::Path};

#[test]
fn full_pinned_parser_printer_adaptor_and_analyzer() {
    assert_eq!(
        cfg!(tpde_address_sanitizer),
        tpde_util::asan::ADDRESS_SANITIZER_ACTIVE,
        "TestIR and its allocator utility must agree on sanitizer activation"
    );
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let work = std::env::temp_dir().join(format!("tpde-test-ir-oracle-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let include = root.join("reference/tpde/include");
    let sources = [
        root.join("tests/oracle.cpp"),
        root.join("reference/tpde/src/test/TestIR.cpp"),
        root.join("reference/tpde/src/util/SmallVector.cpp"),
    ];
    let mut comparisons = 0;
    let mut programs = 0;
    for assertions in [true, false] {
        let executable = work.join(if assertions {
            "debug-oracle"
        } else {
            "release-oracle"
        });
        oracle::compile_cpp_with_flags(
            &include,
            &executable,
            &sources,
            assertions,
            if cfg!(tpde_address_sanitizer) {
                &[
                    "-stdlib=libc++",
                    "-fsanitize=address,undefined",
                    "-fno-sanitize-recover=all",
                ]
            } else {
                &["-stdlib=libc++"]
            },
        );
        oracle::compare_bytes(
            &executable,
            &work,
            b"sanitizer\n",
            format!("sanitizer {}\n", usize::from(cfg!(tpde_address_sanitizer))).as_bytes(),
            "actual C++ and Rust sanitizer activation",
        );
        for case in cases::CASES {
            for mode in ["parse", "records", "print", "adaptor", "analyze"] {
                if mode != "parse" && !case.accepted {
                    continue;
                }
                if mode == "analyze" && !case.analyze {
                    continue;
                }
                let context = MemoryContext::new_bump("test-ir-oracle");
                let expected = snapshot::run(mode, case.source, context.mcx());
                assert_eq!(
                    expected.starts_with("parse 1\n"),
                    case.accepted,
                    "{}",
                    case.name
                );
                let input = format!("{}\n{}", mode, case.source);
                let label = format!("{}:{}:assertions={}", case.name, mode, assertions);
                oracle::compare_bytes(
                    &executable,
                    &work,
                    input.as_bytes(),
                    expected.as_bytes(),
                    &label,
                );
                eprintln!("TPDE_TEST_IR_CASE {} PASS", label);
                comparisons += 1;
            }
        }
        for count in [31, 32, 33, 63, 64, 65, 127, 128, 129, 257] {
            let source = cases::linear(count);
            for mode in ["records", "print", "adaptor", "analyze"] {
                let context = MemoryContext::new_bump("test-ir-scaled");
                let expected = snapshot::run(mode, &source, context.mcx());
                let input = format!("{}\n{}", mode, source);
                let label = format!("linear-{}:{}:assertions={}", count, mode, assertions);
                oracle::compare_bytes(
                    &executable,
                    &work,
                    input.as_bytes(),
                    expected.as_bytes(),
                    &label,
                );
                eprintln!("TPDE_TEST_IR_CASE {} PASS", label);
                comparisons += 1;
            }
        }
        programs += cases::CASES.len() + 10;
    }
    assert_eq!((programs, comparisons), (174, 492));
    eprintln!(
        "TPDE_TEST_IR_SANITIZER_CONFIG PASS active={} cpp_modes=2 checks=2",
        usize::from(cfg!(tpde_address_sanitizer))
    );
    eprintln!(
        "TPDE_TEST_IR_ORACLE PASS programs={} comparisons={} cpp_modes=2 active={}",
        programs,
        comparisons,
        usize::from(cfg!(tpde_address_sanitizer))
    );
    fs::remove_dir_all(&work).unwrap();
}

#[test]
fn runtime_text_binary() {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    for (input, accepted) in [
        ("f(%a) {\nentry:\nbr ^exit\nexit:\nret %a\n}\n", true),
        ("f() {\nentry:\nbr ^missing\n}\n", false),
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tpde-test-ir"))
            .arg("analyze")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(
            output.status.success(),
            accepted,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let context = MemoryContext::new_bump("test-ir-cli");
        assert_eq!(
            output.stdout,
            snapshot::run("analyze", input, context.mcx()).as_bytes()
        );
        if accepted {
            assert!(String::from_utf8_lossy(&output.stdout).contains("layout 2 0 1\n"));
        }
    }
    eprintln!("TPDE_TEST_IR_CLI PASS programs=2");
}
