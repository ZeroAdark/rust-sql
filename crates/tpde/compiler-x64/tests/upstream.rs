// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

mod cases;
mod fixture;
mod reference_support;
mod semantics;

use std::{
    collections::BTreeSet,
    fmt::Write,
    fs,
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
};

#[test]
fn compiler_x64_matches_pinned_upstream() {
    let sanitizer_identity = address_sanitizer_identity();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let artifacts = PathBuf::from(
        std::env::var_os("COMPONENT_ARTIFACT_DIR").expect("component artifact directory"),
    )
    .join(format!(
        "compiler-x64-oracle-{}",
        u8::from(cfg!(tpde_address_sanitizer))
    ));
    fs::create_dir_all(&artifacts).unwrap();
    fs::write(
        artifacts.join("rust-sanitizer-identity.txt"),
        sanitizer_identity,
    )
    .unwrap();
    let executables = reference_support::load_prepared(root, &artifacts);
    assert_eq!(executables.len(), 3);
    for (index, executable) in executables.iter().enumerate() {
        let result = reference_support::run_reference(
            executable,
            &artifacts,
            b"sanitizer\n",
            &format!("sanitizer-mode-{index}"),
        );
        assert!(result.status.success());
        assert_eq!(
            result.stdout,
            format!("sanitizer {}\n", usize::from(index == 2)).as_bytes()
        );
    }
    let cases = cases::cases();
    assert_eq!(cases.len(), cases::CASE_COUNT);
    let mut names = BTreeSet::new();
    for case in &cases {
        assert!(names.insert(&case.name), "duplicate case {}", case.name);
    }
    let mut roster = String::from("case\tmode\tverdict\n");
    let mut failed = Vec::new();
    let mut comparisons = 0;
    for case in &cases {
        let work = artifacts.join(&case.name);
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("input.txt"), &case.input).unwrap();
        let run = catch_unwind(AssertUnwindSafe(|| fixture::run(&case.input)));
        let rust_output = match run {
            Ok(output) => {
                fs::write(work.join("rust.txt"), &output).unwrap();
                Some(output)
            }
            Err(error) => {
                let message = panic_message(&error);
                fs::write(work.join("rust.failure.txt"), &message).unwrap();
                failed.push(format!("{} Rust: {message}", case.name));
                None
            }
        };
        for (mode, executable) in ["assertions", "ndebug", "sanitizers"]
            .into_iter()
            .zip(&executables)
        {
            let mode_work = work.join(mode);
            fs::create_dir_all(&mode_work).unwrap();
            comparisons += 1;
            let result = catch_unwind(AssertUnwindSafe(|| {
                compare(
                    executable,
                    &mode_work,
                    case.input.as_bytes(),
                    rust_output
                        .as_ref()
                        .expect("Rust execution failed")
                        .as_bytes(),
                );
            }));
            let verdict = if let Err(error) = result {
                let message = panic_message(&error);
                fs::write(mode_work.join("failure.txt"), &message).unwrap();
                failed.push(format!("{} {mode}: {message}", case.name));
                "FAIL"
            } else {
                "PASS"
            };
            writeln!(roster, "{}\t{mode}\t{verdict}", case.name).unwrap();
            fs::write(artifacts.join("case-verdicts.tsv"), &roster).unwrap();
        }
    }
    assert_eq!(comparisons, cases.len() * 3);
    let mut control_failures = Vec::new();
    let mut control_roster = String::from("control\tverdict\n");
    let result = catch_unwind(AssertUnwindSafe(|| {
        preloaded_byval_refusals(&cases, &executables, &artifacts);
    }));
    record_control(
        &artifacts,
        "preloaded_byval",
        result,
        &mut control_roster,
        &mut control_failures,
    );
    let result = catch_unwind(AssertUnwindSafe(|| {
        assert_eq!(correction_observations(&executables, &artifacts), 18);
    }));
    record_control(
        &artifacts,
        "corrections",
        result,
        &mut control_roster,
        &mut control_failures,
    );
    let result = catch_unwind(AssertUnwindSafe(|| {
        high_register_observations(&executables, &artifacts);
    }));
    record_control(
        &artifacts,
        "high_registers",
        result,
        &mut control_roster,
        &mut control_failures,
    );
    failed.extend(control_failures);
    assert!(
        failed.is_empty(),
        "{} differential/control failures:\n{}",
        failed.len(),
        failed.join("\n")
    );
    println!("\nTPDE_COMPILER_X64_ORACLE PASS cases={} comparisons={comparisons} cpp_modes=3 corrections=6 correction_observations=18 high_register_comparisons=1 high_register_refusals=2 active={} exclusions=0", cases.len(), u8::from(cfg!(tpde_address_sanitizer)));
}

fn panic_message(error: &Box<dyn std::any::Any + Send>) -> String {
    error
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            error
                .downcast_ref::<&str>()
                .map(|message| (*message).to_owned())
        })
        .unwrap_or_else(|| "non-string panic".to_owned())
}

fn compare(executable: &Path, work: &Path, input: &[u8], expected: &[u8]) {
    let path = work.join("request.txt");
    fs::write(&path, input).unwrap();
    let result = reference_support::run_reference(executable, work, input, "compare");
    fs::write(work.join("cpp.stdout"), &result.stdout).unwrap();
    fs::write(work.join("cpp.stderr"), &result.stderr).unwrap();
    assert!(
        result.status.success(),
        "C++ reference {}: {}",
        result.status,
        String::from_utf8_lossy(&result.stderr)
    );
    let mismatch = expected
        .iter()
        .zip(&result.stdout)
        .position(|(a, b)| a != b);
    assert!(
        mismatch.is_none() && expected.len() == result.stdout.len(),
        "first mismatch {:?}; Rust length {}, C++ length {}",
        mismatch.or_else(|| (expected.len() != result.stdout.len())
            .then_some(expected.len().min(result.stdout.len()))),
        expected.len(),
        result.stdout.len()
    );
}

fn correction_observations(executables: &[PathBuf], artifacts: &Path) -> usize {
    let mut observations = 0;
    let mut roster = String::from("case\tmode\toriginal\tcorrected\tverdict\n");
    let mut failures = Vec::new();
    for case in semantics::corrections() {
        let work = artifacts.join("corrections").join(case.name);
        fs::create_dir_all(&work).unwrap();
        let input = work.join("input.txt");
        fs::write(&input, case.input).unwrap();
        let corrected = catch_unwind(AssertUnwindSafe(|| {
            let rust = fixture::run(case.input);
            fs::write(work.join("rust.txt"), &rust).unwrap();
            let code = semantics::text(&rust);
            for seed in [1, 7, 173] {
                let initial = semantics::initial(seed);
                assert_eq!(
                    semantics::evaluate(&code, initial)[case.register],
                    (case.expected)(&initial),
                    "corrected {}",
                    case.name
                );
            }
        }));
        let corrected_verdict = if let Err(error) = corrected {
            let message = panic_message(&error);
            fs::write(work.join("rust.failure.txt"), &message).unwrap();
            failures.push(format!("{} Rust: {message}", case.name));
            "FAIL"
        } else {
            "PASS"
        };
        for (mode, executable) in ["assertions", "ndebug", "sanitizers"]
            .into_iter()
            .zip(executables)
        {
            let original = catch_unwind(AssertUnwindSafe(|| {
                let result = reference_support::run_reference(
                    executable,
                    &work,
                    case.input.as_bytes(),
                    mode,
                );
                fs::write(work.join(format!("{mode}.stdout")), &result.stdout).unwrap();
                fs::write(work.join(format!("{mode}.stderr")), &result.stderr).unwrap();
                if mode == "sanitizers"
                    && matches!(case.name, "folded_zero_scale" | "minimum_signed_scale")
                {
                    assert!(
                        !result.status.success(),
                        "UBSan did not reject {}",
                        case.name
                    );
                }
                if result.status.success() {
                    let output = String::from_utf8(result.stdout).unwrap();
                    let code = semantics::text(&output);
                    let differs = [1, 7, 173].into_iter().any(|seed| {
                        let initial = semantics::initial(seed);
                        semantics::evaluate(&code, initial)[case.register]
                            != (case.expected)(&initial)
                    });
                    if case.name != "minimum_signed_scale" {
                        assert!(
                            differs,
                            "{} {mode}: original unexpectedly satisfies corrected semantics",
                            case.name
                        );
                    }
                    if differs {
                        "WRONG_VALUE"
                    } else {
                        "VALUE_MATCH_WITH_SOURCE_UB"
                    }
                } else {
                    let error = String::from_utf8_lossy(&result.stderr).to_lowercase();
                    let expected = match case.name {
                        "folded_zero_scale" => {
                            mode == "sanitizers"
                                && error.lines().any(|line| {
                                    line.ends_with("/tpde/util/misc.hpp:48:26: runtime error: passing zero to __builtin_ctz(), which is not a valid argument")
                                })
                                && error.lines().any(|line| {
                                    line.starts_with("summary: undefinedbehaviorsanitizer: undefined-behavior ")
                                        && line.trim_end().ends_with("/tpde/util/misc.hpp:48:26")
                                })
                        }
                        "minimum_signed_scale" => {
                            error.contains("compilerx64.hpp")
                                && (error.contains("signed integer overflow")
                                    || (mode == "assertions" && error.contains("n != 0")))
                        }
                        _ => false,
                    };
                    assert!(expected, "unexpected original refusal: {error}");
                    "UPSTREAM_REFUSAL"
                }
            }));
            let original = match original {
                Ok(verdict) => verdict,
                Err(error) => {
                    let message = panic_message(&error);
                    fs::write(work.join(format!("{mode}.failure.txt")), &message).unwrap();
                    failures.push(format!("{} {mode}: {message}", case.name));
                    "FAIL"
                }
            };
            let verdict = if original == "FAIL" || corrected_verdict == "FAIL" {
                "FAIL"
            } else {
                "PASS"
            };
            observations += 1;
            writeln!(
                roster,
                "{}\t{mode}\t{original}\t{corrected_verdict}\t{verdict}",
                case.name
            )
            .unwrap();
            fs::write(artifacts.join("correction-verdicts.tsv"), &roster).unwrap();
        }
    }
    assert_eq!(observations, 18);
    assert!(
        failures.is_empty(),
        "correction failures:\n{}",
        failures.join("\n")
    );
    observations
}

fn high_register_observations(executables: &[PathBuf], artifacts: &Path) {
    let case = cases::high_registers();
    let work = artifacts.join(&case.name);
    fs::create_dir_all(&work).unwrap();
    fs::write(work.join("input.txt"), &case.input).unwrap();
    let mut failures = Vec::new();
    let rust = catch_unwind(AssertUnwindSafe(|| {
        let rust = fixture::run(&case.input);
        fs::write(work.join("rust.txt"), &rust).unwrap();
        rust
    }));
    let rust = match rust {
        Ok(rust) => Some(rust),
        Err(error) => {
            let message = panic_message(&error);
            fs::write(work.join("rust.failure.txt"), &message).unwrap();
            failures.push(format!("Rust: {message}"));
            None
        }
    };
    let mut roster = String::from("mode\tverdict\n");
    let mut observations = 0;
    for (index, mode) in ["assertions", "ndebug", "sanitizers"]
        .into_iter()
        .enumerate()
    {
        let result = catch_unwind(AssertUnwindSafe(|| {
            let result = reference_support::run_reference(
                &executables[index],
                &work,
                case.input.as_bytes(),
                mode,
            );
            fs::write(work.join(format!("{mode}.stdout")), &result.stdout).unwrap();
            fs::write(work.join(format!("{mode}.stderr")), &result.stderr).unwrap();
            if index == 1 {
                assert!(result.status.success());
                assert_eq!(
                    result.stdout,
                    rust.as_ref()
                        .expect("Rust high-register execution failed")
                        .as_bytes()
                );
                "BYTE_MATCH"
            } else {
                assert!(!result.status.success());
                let error = String::from_utf8_lossy(&result.stderr);
                assert!(
                    error.contains("CompilerX64.hpp")
                        && error.contains("id <= R15 || (id >= XMM0 && id <= XMM15)"),
                    "wrong upstream register refusal: {error}"
                );
                "EXACT_UPSTREAM_RANGE_REFUSAL"
            }
        }));
        let verdict = match result {
            Ok(verdict) => verdict,
            Err(error) => {
                let message = panic_message(&error);
                fs::write(work.join(format!("{mode}.failure.txt")), &message).unwrap();
                failures.push(format!("{mode}: {message}"));
                "FAIL"
            }
        };
        observations += 1;
        writeln!(roster, "{mode}\t{verdict}").unwrap();
        fs::write(work.join("verdicts.tsv"), &roster).unwrap();
    }
    assert_eq!(observations, 3);
    assert!(
        failures.is_empty(),
        "high-register failures:\n{}",
        failures.join("\n")
    );
}

fn address_sanitizer_identity() -> String {
    let active = cfg!(tpde_address_sanitizer);
    let mode = std::env::var("COMPONENT_ORACLE_MODE").expect("component oracle mode");
    assert_eq!(mode, if active { "asan" } else { "normal" });
    assert_eq!(active, tpde_util::asan::ADDRESS_SANITIZER_ACTIVE);
    #[repr(align(8))]
    struct Region([u8; 16]);
    let mut region = Region([0xa5; 16]);
    let pointer = region.0.as_mut_ptr();
    #[cfg(tpde_address_sanitizer)]
    let shadow = {
        extern "C" {
            fn __asan_address_is_poisoned(pointer: *const u8) -> i32;
        }
        // SAFETY: The aligned region is exclusively owned; restore shadow state before access.
        unsafe {
            let before = __asan_address_is_poisoned(pointer);
            tpde_util::asan::poison_memory_region(pointer, 8);
            let poisoned = __asan_address_is_poisoned(pointer);
            let adjacent = __asan_address_is_poisoned(pointer.add(8));
            tpde_util::asan::unpoison_memory_region(pointer, 8);
            let restored = __asan_address_is_poisoned(pointer);
            [before, poisoned, adjacent, restored]
        }
    };
    #[cfg(not(tpde_address_sanitizer))]
    let shadow = {
        // SAFETY: The exclusively owned region remains live through both operations.
        unsafe {
            tpde_util::asan::poison_memory_region(pointer, 8);
            tpde_util::asan::unpoison_memory_region(pointer, 8);
        }
        [0; 4]
    };
    assert_eq!(shadow, [0, i32::from(active), 0, 0]);
    assert_eq!(region.0, [0xa5; 16]);
    format!(
        "active={} before={} poisoned={} adjacent={} restored={} bytes_unchanged=1\n",
        u8::from(active),
        shadow[0],
        shadow[1],
        shadow[2],
        shadow[3]
    )
}

fn preloaded_byval_refusals(cases: &[cases::Case], executables: &[PathBuf], artifacts: &Path) {
    let case = cases
        .iter()
        .find(|case| case.name == "call_arguments_preloaded_registers")
        .unwrap();
    let valid = "callarg 0 15 16 0 1 0 0 4096 0\n";
    let invalid = "callarg_loaded 0 15 16 0 1 0 0 4096 0\n";
    assert_eq!(case.input.matches(valid).count(), 1);
    let input = case.input.replace(valid, invalid);
    let work = artifacts.join("preloaded_byval_refusals");
    fs::create_dir_all(&work).unwrap();
    fs::write(work.join("input.txt"), &input).unwrap();
    let mut roster = String::from("mode\tverdict\n");
    if tpde_util::CHECKED {
        let error = catch_unwind(AssertUnwindSafe(|| fixture::run(&input)))
            .expect_err("preloaded byval pointer must be rejected");
        let message = panic_message(&error);
        fs::write(work.join("rust.failure.txt"), &message).unwrap();
        assert_eq!(message, "assertion failed: !self.has_reg()");
        roster.push_str("Rust\tEXACT_REGISTER_PRECONDITION_REFUSAL\n");
    } else {
        roster.push_str("Rust\tUNCHECKED_BUILD_CONTRACT_NOT_EXERCISED\n");
    }
    for (index, mode) in [(0, "assertions"), (2, "sanitizers")] {
        let result =
            reference_support::run_reference(&executables[index], &work, input.as_bytes(), mode);
        fs::write(work.join(format!("{mode}.stdout")), &result.stdout).unwrap();
        fs::write(work.join(format!("{mode}.stderr")), &result.stderr).unwrap();
        assert!(!result.status.success());
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(
            error.contains("ValuePartRef.hpp") && error.contains("!state.c.reg.valid()"),
            "wrong upstream byval refusal: {error}"
        );
        writeln!(roster, "{mode}\tEXACT_REGISTER_PRECONDITION_REFUSAL").unwrap();
        fs::write(work.join("verdicts.tsv"), &roster).unwrap();
    }
}

fn record_control(
    artifacts: &Path,
    name: &str,
    result: std::thread::Result<()>,
    roster: &mut String,
    failures: &mut Vec<String>,
) {
    let verdict = if let Err(error) = result {
        let message = panic_message(&error);
        fs::write(artifacts.join(format!("{name}.failure.txt")), &message).unwrap();
        failures.push(format!("{name}: {message}"));
        "FAIL"
    } else {
        "PASS"
    };
    writeln!(roster, "{name}\t{verdict}").unwrap();
    fs::write(artifacts.join("control-verdicts.tsv"), roster.as_bytes()).unwrap();
}
