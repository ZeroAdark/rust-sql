// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

#[path = "compiler_fixture/cases.rs"]
mod cases;
mod compiler_fixture;
#[path = "../../test-support/oracle.rs"]
mod oracle;

use std::{
    fmt::Write as _,
    fs,
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
};

const PROGRAMS: usize = 99;
const REJECTED_PROGRAMS: usize = 6;
const FRAMES_PER_DIRECTION: usize = 24;
const COMPARISONS: usize = 896;

fn has_record(output: &str, tag: &str) -> bool {
    output.lines().any(|line| {
        line == tag
            || line
                .strip_prefix(tag)
                .is_some_and(|rest| rest.starts_with(' '))
    })
}

struct Comparisons {
    attempts: usize,
    failures: Vec<String>,
    roster: String,
    artifacts: PathBuf,
}

impl Comparisons {
    fn check(
        &mut self,
        label: String,
        input: String,
        run: impl FnOnce() -> String,
        executable: &Path,
        work: &Path,
    ) {
        self.attempts += 1;
        let mut expected = None;
        let result = catch_unwind(AssertUnwindSafe(|| {
            let output = run();
            expected = Some(output);
            oracle::compare_bytes(
                executable,
                work,
                input.as_bytes(),
                expected.as_ref().unwrap().as_bytes(),
                &label,
            );
        }));
        let status = if let Err(error) = result {
            let message = error
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| error.downcast_ref::<&str>().copied())
                .unwrap_or("non-string panic");
            self.failures.push(format!("{label}: {message}"));
            let failed = self.artifacts.join("failures").join(&label);
            fs::create_dir_all(&failed).unwrap();
            fs::write(failed.join("request.txt"), input).unwrap();
            if let Some(output) = expected {
                fs::write(failed.join("rust.txt"), output).unwrap();
            }
            "FAIL"
        } else {
            "PASS"
        };
        writeln!(self.roster, "{label}\t{status}").unwrap();
        fs::write(self.artifacts.join("case-verdicts.tsv"), &self.roster).unwrap();
        eprintln!("TPDE_COMPILER_STATE_CASE {label} {status}");
    }
}

#[test]
fn compiler_state_matches_pinned_upstream() {
    let active = cfg!(tpde_address_sanitizer);
    assert_eq!(
        active,
        tpde_util::asan::ADDRESS_SANITIZER_ACTIVE,
        "compiler and allocator utility must agree on actual sanitizer activation"
    );
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let work = std::env::temp_dir().join(format!(
        "tpde-compiler-oracle-{}-{}",
        std::process::id(),
        u8::from(active)
    ));
    fs::create_dir_all(&work).unwrap();
    let artifacts = std::env::var_os("COMPONENT_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| work.clone())
        .join(format!("compiler-state-asan-{}", u8::from(active)));
    fs::create_dir_all(artifacts.join("programs")).unwrap();
    let programs = cases::programs();
    assert_eq!(programs.len(), PROGRAMS);
    assert_eq!(
        programs.iter().filter(|case| !case.accepted).count(),
        REJECTED_PROGRAMS
    );
    let mut names = std::collections::BTreeSet::new();
    for program in &programs {
        assert!(
            names.insert(&program.name),
            "duplicate program name {}",
            program.name
        );
        fs::write(
            artifacts
                .join("programs")
                .join(format!("{}.ir", program.name)),
            &program.source,
        )
        .unwrap();
    }
    let reference = root.join("tests/reference");
    let include = reference.join("tpde/include");
    let sources: Vec<_> = [
        "oracle.cpp",
        "tpde/src/Assembler.cpp",
        "tpde/src/AssemblerElf.cpp",
        "tpde/src/FunctionWriter.cpp",
        "tpde/src/StringTable.cpp",
        "tpde/src/ValueAssignment.cpp",
        "tpde/src/base.cpp",
        "tpde/src/test/TestIR.cpp",
        "tpde/src/util/SmallVector.cpp",
    ]
    .into_iter()
    .map(|source| reference.join(source))
    .collect();
    let mut comparisons = Comparisons {
        attempts: 0,
        failures: Vec::new(),
        roster: String::from("case\tverdict\n"),
        artifacts,
    };
    for assertions in [true, false] {
        let executable = work.join(if assertions {
            "assert-oracle"
        } else {
            "ndebug-oracle"
        });
        let mut flags = vec!["-stdlib=libc++", "-I", reference.to_str().unwrap()];
        if active {
            flags.extend(["-fsanitize=address,undefined", "-fno-sanitize-recover=all"]);
        }
        oracle::compile_cpp_with_flags(&include, &executable, &sources, assertions, &flags);
        for negative in [false, true] {
            let prefix = format!(
                "assert_{}_negative_{}",
                u8::from(assertions),
                u8::from(negative)
            );
            let header = |mode: &str, fixed: bool, stride: u64, tables: bool| {
                format!(
                    "{mode} {} {} {stride} {}\n",
                    u8::from(negative),
                    u8::from(fixed),
                    u8::from(tables)
                )
            };
            comparisons.check(
                format!("{prefix}_sanitizer"),
                header("sanitizer", true, 1, true),
                || {
                    let output = compiler_fixture::run("sanitizer", "", negative, true, 1, true);
                    assert_eq!(
                        output,
                        format!(
                            "tpde.compiler.state 1 {}\nsanitizer {}\n",
                            u8::from(negative),
                            u8::from(active)
                        )
                    );
                    output
                },
                &executable,
                &work,
            );
            let frames = cases::frames(negative);
            assert_eq!(frames.len(), FRAMES_PER_DIRECTION);
            for (name, source) in frames {
                let input = header("frame", true, 1, true) + &source;
                comparisons.check(
                    format!("{prefix}_frame_{name}"),
                    input,
                    || {
                        let output =
                            compiler_fixture::run("frame", &source, negative, true, 1, true);
                        assert!(has_record(&output, "alloc"));
                        assert_eq!(
                            output
                                .lines()
                                .filter(|line| line.starts_with("freelist "))
                                .count(),
                            5
                        );
                        output
                    },
                    &executable,
                    &work,
                );
            }
            comparisons.check(
                format!("{prefix}_guards"),
                header("guards", true, 1, true),
                || {
                    let output = compiler_fixture::run("guards", "", negative, true, 1, true);
                    for tag in [
                        "scratch.empty",
                        "scratch.alloc",
                        "scratch.same",
                        "scratch.move",
                        "scratch.release",
                        "part.default",
                        "part.unowned",
                        "part.owner",
                        "immediate",
                        "generic.scratch",
                        "generic.value",
                        "specific.overwrite",
                        "specific.after.eviction",
                        "assigned.reuse",
                        "assigned.source",
                        "assigned.destination",
                        "assigned.fixed",
                        "assigned.fixed.state",
                        "assigned.scratch",
                        "assigned.scratch.state",
                        "expression.trivial",
                        "expression.materialized",
                        "expression.owned",
                    ] {
                        assert!(has_record(&output, tag), "missing guard witness {tag}");
                    }
                    output
                },
                &executable,
                &work,
            );
            for fixed in [false, true] {
                for program in &programs {
                    let label = format!(
                        "{prefix}_fixed_{}_compile_{}",
                        u8::from(fixed),
                        program.name
                    );
                    let input =
                        header("compile", fixed, program.stride, program.tables) + &program.source;
                    comparisons.check(
                        label,
                        input,
                        || {
                            let output = compiler_fixture::run(
                                "compile",
                                &program.source,
                                negative,
                                fixed,
                                program.stride,
                                program.tables,
                            );
                            assert!(output.starts_with(&format!(
                                "tpde.compiler.state 1 {}\nparse {}\n",
                                u8::from(negative),
                                u8::from(program.accepted)
                            )));
                            if program.accepted {
                                assert!(
                                    output.ends_with("compile 1\nreset 0 0 0\n"),
                                    "accepted program did not complete compilation and reset"
                                );
                                for tag in program.required {
                                    assert!(
                                        has_record(&output, tag),
                                        "missing required event {tag}"
                                    );
                                }
                                for tag in program.forbidden {
                                    assert!(!has_record(&output, tag), "unexpected event {tag}");
                                }
                            } else {
                                assert!(!has_record(&output, "compile"));
                            }
                            output
                        },
                        &executable,
                        &work,
                    );
                }
            }
        }
    }
    assert_eq!(comparisons.attempts, COMPARISONS);
    fs::write(
        comparisons.artifacts.join("failures.txt"),
        comparisons.failures.join("\n"),
    )
    .unwrap();
    assert!(
        comparisons.failures.is_empty(),
        "{} compiler differential failures:\n{}",
        comparisons.failures.len(),
        comparisons.failures.join("\n")
    );
    eprintln!(
        "TPDE_COMPILER_STATE_SANITIZER_CONFIG PASS active={} cpp_modes=2 checks=4",
        u8::from(active)
    );
    eprintln!("TPDE_COMPILER_STATE_ORACLE PASS programs={PROGRAMS} accepted={} rejected={REJECTED_PROGRAMS} frame_cases={} guard_cases=4 comparisons={COMPARISONS} cpp_modes=2 active={}", PROGRAMS - REJECTED_PROGRAMS, FRAMES_PER_DIRECTION * 4, u8::from(active));
    fs::remove_dir_all(work).unwrap();
}
