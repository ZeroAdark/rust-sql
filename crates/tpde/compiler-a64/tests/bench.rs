// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

//! Paired raw-operation cost of the fresh A64 compiler target against the
//! pinned compiled C++ CompilerA64 (ndebug reference flags).
//!
//! Boundary: the qualified raw protocol scripts (see raw_cases.rs). `raw`
//! constructs a fresh compiler per iteration, runs every scripted CompilerA64
//! operation, and exports the object; `abi` replays the AAPCS assigner script.
//! Protocol output is accumulated in memory and checksummed on both sides.
//! Every round runs Rust then C++ per case; ratios are medians of paired
//! per-round ratios and verdicts use corpus totals of per-case medians.
#![cfg(feature = "upstream-oracle")]
#[allow(dead_code)]
#[path = "../../test-support/oracle.rs"]
mod oracle;
#[allow(dead_code)]
#[path = "../../test-support/perf.rs"]
mod perf;
mod raw_cases;
#[allow(dead_code)]
mod raw_oracle_build;
#[allow(dead_code)]
mod raw_support;

use raw_cases::Case;
use raw_support::{put, run_abi, Frontend};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tpde_assembler::Architecture;
use tpde_compiler::with_compiler_base;
use tpde_compiler_a64::CompilerA64;
use tpde_core::ir_adaptor::IrAdaptor;
use tpde_function_writer_a64::A64Target;
use tpde_test_ir::{TestIR, TestIRAdaptor};

const ROUNDS: usize = 5;
const ITERATIONS: u64 = 300;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// One fresh-compiler execution of a raw script: protocol bytes plus object.
fn raw_once(case: &Case, output: &RefCell<Vec<u8>>) {
    output.borrow_mut().clear();
    let context = mcx::MemoryContext::new_bump("A64 raw bench");
    let mut ir = TestIR::new(context.mcx());
    ir.parse_ir("raw() {\nentry:\nterminate\n}\n").unwrap();
    with_compiler_base(
        context.mcx(),
        TestIRAdaptor::new(&ir),
        CompilerA64::new(
            context.mcx(),
            Frontend {
                script: &case.operations,
                output,
            },
            0,
        ),
        A64Target::new(context.mcx()),
        Architecture::Aarch64,
        |mut compiler| {
            compiler.adaptor.start_compile();
            assert!(compiler.compile().unwrap(), "{}", case.name);
            compiler.adaptor.end_compile();
            let object = compiler.assembler().unwrap().build_object_file().unwrap();
            put(&mut output.borrow_mut(), object.len() as u64);
            output.borrow_mut().extend_from_slice(&object);
        },
    )
    .unwrap();
}

fn rust_once(case: &Case, output: &RefCell<Vec<u8>>, checksum: &mut u64) -> usize {
    if case.mode == 1 {
        let bytes = run_abi(case);
        *checksum = fnv1a(*checksum, &bytes);
        bytes.len()
    } else {
        raw_once(case, output);
        let bytes = output.borrow();
        *checksum = fnv1a(*checksum, &bytes);
        bytes.len()
    }
}

fn rust_leg(counter: &perf::Counter, case: &Case) -> (perf::Measurement, u64, usize) {
    let output = RefCell::new(Vec::new());
    let mut checksum = FNV_OFFSET;
    let mut produced = 0;
    for _ in 0..ITERATIONS / 10 + 1 {
        produced = rust_once(case, &output, &mut checksum);
    }
    checksum = FNV_OFFSET;
    let ((), measurement) = counter.measure(|| {
        for _ in 0..ITERATIONS {
            produced = rust_once(case, &output, &mut checksum);
        }
    });
    (measurement, checksum, produced)
}

fn cpp_leg(binary: &Path, case: &Case) -> (perf::Measurement, BTreeMap<String, String>) {
    let mut request = ITERATIONS.to_le_bytes().to_vec();
    request.extend_from_slice(&raw_cases::encode(case));
    let mut child = Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pinned C++ benchmark reference");
    child.stdin.take().unwrap().write_all(&request).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}: reference exited {}\n{}",
        case.name,
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut measurement = None;
    let mut extra = BTreeMap::new();
    for line in stdout.lines() {
        let mut fields = line.split_whitespace();
        let Some(leg) = fields.next() else { continue };
        if leg == "raw" || leg == "abi" {
            let instructions = match fields.next().unwrap() {
                "-" => None,
                value => Some(value.parse().unwrap()),
            };
            let nanoseconds = fields.next().unwrap().parse().unwrap();
            let minor_faults = fields.next().unwrap().parse().unwrap();
            measurement = Some(perf::Measurement {
                instructions,
                nanoseconds,
                minor_faults,
            });
            for field in fields {
                let (key, value) = field.split_once('=').unwrap();
                extra.insert(key.to_owned(), value.to_owned());
            }
        } else if let Some(rest) = fields.next() {
            extra.insert(leg.to_owned(), rest.to_owned());
        }
    }
    (measurement.expect("reference measurement line"), extra)
}

/// tpde-asm-diff diagnostic: disassemble the host-compiler symbols of one side
/// that match `patterns` into `asm-<label>.s`. Runs outside every timed leg.
fn capture_asm(directory: &Path, label: &str, exe: &Path, patterns: &[&str]) {
    let tool = |base: &str| {
        [format!("{base}-21"), base.to_owned()]
            .into_iter()
            .find(|candidate| {
                Command::new(candidate)
                    .arg("--version")
                    .output()
                    .is_ok_and(|output| output.status.success())
            })
    };
    let target = directory.join(format!("asm-{label}.s"));
    let (Some(nm), Some(objdump)) = (tool("llvm-nm"), tool("llvm-objdump")) else {
        fs::write(target, "UNRUN: llvm-nm/llvm-objdump unavailable\n").unwrap();
        return;
    };
    let names = Command::new(&nm).arg("--defined-only").arg(exe).output().unwrap();
    let mut symbols: Vec<String> = String::from_utf8_lossy(&names.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _address = fields.next()?;
            let kind = fields.next()?;
            let name = fields.next()?;
            (matches!(kind, "T" | "t" | "W" | "w") && patterns.iter().any(|p| name.contains(p)))
                .then(|| name.to_owned())
        })
        .collect();
    symbols.sort();
    symbols.dedup();
    let mut command = Command::new(&objdump);
    command.args(["-d", "--no-show-raw-insn", "--symbolize-operands"]);
    for symbol in &symbols {
        command.arg(format!("--disassemble-symbols={symbol}"));
    }
    let output = command.arg(exe).output().unwrap();
    let mut text = format!(
        "== tool {objdump}\n== exe {}\n== symbols\n{}\n",
        exe.display(),
        symbols.join("\n")
    );
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    fs::write(target, text).unwrap();
}

fn build_reference(shared: &Path, directory: &Path) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let compiler_reference = root.join("../compiler/tests/reference");
    let writer_reference = root.join("../function-writer-a64/tests/upstream");
    let disarm = writer_reference.join("disarm");
    let generated = shared.join("disarm-generated");
    let binary = directory.join("bench-ndebug");
    let cxx = std::env::var("CXX").expect("the runner profile must configure CXX");
    let mut command = Command::new(&cxx);
    command
        .args(["-std=c++20", "-O3", "-DNDEBUG", "-stdlib=libc++"])
        .arg("-I")
        .arg(root.join("tests/raw_reference/tpde/include"))
        .arg("-I")
        .arg(&compiler_reference)
        .arg("-I")
        .arg(compiler_reference.join("tpde/include"))
        .arg("-I")
        .arg(writer_reference.join("tpde/include"))
        .arg("-I")
        .arg(&disarm)
        .arg("-I")
        .arg(&generated)
        .arg("-I")
        .arg(root.join("tests"))
        .arg("-I")
        .arg(root.join("../test-support"))
        .arg(root.join("tests/raw_bench.cpp"));
    for source in [
        "tpde/src/FunctionWriter.cpp",
        "tpde/src/Assembler.cpp",
        "tpde/src/AssemblerElf.cpp",
        "tpde/src/StringTable.cpp",
        "tpde/src/ValueAssignment.cpp",
        "tpde/src/test/TestIR.cpp",
        "tpde/src/base.cpp",
        "tpde/src/util/SmallVector.cpp",
    ] {
        command.arg(compiler_reference.join(source));
    }
    command
        .arg(writer_reference.join("tpde/src/arm64/FunctionWriterA64.cpp"))
        .arg(shared.join("disarm-encode-ndebug.o"))
        .arg("-o")
        .arg(&binary);
    fs::write(
        directory.join("compile-bench-ndebug.command.txt"),
        format!("{command:?}\n"),
    )
    .unwrap();
    let output = command.output().expect("run pinned C++ compiler");
    fs::write(
        directory.join("compile-bench-ndebug.stderr"),
        &output.stderr,
    )
    .unwrap();
    assert!(
        output.status.success(),
        "bench-ndebug reference compilation failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    binary
}

#[test]
fn paired_raw_operation_cost_against_pinned_cpp() {
    assert!(
        cfg!(target_arch = "aarch64"),
        "A64 benchmark requires an A64 runner"
    );
    let directory = PathBuf::from(std::env::var_os("COMPONENT_ARTIFACT_DIR").unwrap())
        .join("compiler-a64-bench");
    fs::create_dir_all(&directory).unwrap();
    let references = raw_oracle_build::require_references(&directory);
    let shared = references[0].executable.parent().unwrap().to_path_buf();
    let binary = build_reference(&shared, &directory);
    capture_asm(
        &directory,
        "rust",
        &std::env::current_exe().unwrap(),
        &[
            "raw_support7execute",
            "3raw4emit",
            "write_bytes",
            "ensure_space",
            "more_space",
            "write_unchecked",
            "skip_unchecked",
            "write_at",
            "with_output",
        ],
    );
    capture_asm(
        &directory,
        "cpp",
        &binary,
        &["RawCompiler", "FunctionWriter", "more_space", "compile_inst"],
    );
    let counter = perf::Counter::new();
    let mut environment = String::new();
    for name in ["/proc/sys/kernel/perf_event_paranoid", "/proc/cpuinfo"] {
        writeln!(
            environment,
            "== {name}\n{}",
            fs::read_to_string(name).unwrap_or_else(|error| error.to_string())
        )
        .unwrap();
    }
    writeln!(
        environment,
        "== pmu\n{}\n== rounds={ROUNDS} iterations={ITERATIONS}",
        counter.unavailable.as_deref().unwrap_or("ok")
    )
    .unwrap();
    fs::write(directory.join("environment.txt"), &environment).unwrap();
    let cases = raw_cases::cases();
    assert_eq!(cases.len(), 28);
    let raw_cases = cases.iter().filter(|case| case.mode == 0).count();
    let mut rows = String::from(
        "round\tcase\tleg\tside\tinstructions\tnanoseconds\tminor_faults\tchecksum\toutput_size\n",
    );
    let mut series: BTreeMap<(String, String), perf::Paired> = BTreeMap::new();
    let mut mismatches = Vec::new();
    for case in &cases {
        rust_leg(&counter, case);
    }
    for round in 1..=ROUNDS {
        for case in &cases {
            let leg = if case.mode == 1 { "abi" } else { "raw" };
            let (rust, checksum, produced) = rust_leg(&counter, case);
            let (cpp, extra) = cpp_leg(&binary, case);
            for (side, measurement, checksum, produced) in [
                (
                    "rust",
                    rust,
                    format!("{checksum:016x}"),
                    produced.to_string(),
                ),
                (
                    "cpp",
                    cpp,
                    extra.get("checksum").cloned().unwrap_or_default(),
                    extra.get("output_size").cloned().unwrap_or_default(),
                ),
            ] {
                writeln!(
                    rows,
                    "{round}\t{}\t{leg}\t{side}\t{}\t{}\t{}\t{checksum}\t{produced}",
                    case.name,
                    measurement
                        .instructions
                        .map_or("-".to_owned(), |v| v.to_string()),
                    measurement.nanoseconds,
                    measurement.minor_faults
                )
                .unwrap();
            }
            if extra.get("checksum").map(String::as_str) != Some(&format!("{checksum:016x}")) {
                mismatches.push(format!(
                    "{}: checksum rust={checksum:016x} cpp={}",
                    case.name,
                    extra.get("checksum").cloned().unwrap_or_default()
                ));
            }
            let entry = series
                .entry((case.name.to_owned(), leg.to_owned()))
                .or_default();
            entry.rust.push(rust);
            entry.cpp.push(cpp);
        }
        fs::write(directory.join("rows.tsv"), &rows).unwrap();
        println!("TPDE_COMPILER_A64_BENCH_ROUND {round} complete");
    }
    mismatches.sort();
    mismatches.dedup();
    fs::write(
        directory.join("output-mismatches.txt"),
        mismatches.join("\n"),
    )
    .unwrap();
    let (summary, verdicts, records) = perf::summarize(&series, &["raw", "abi"]);
    fs::write(directory.join("summary.tsv"), &summary).unwrap();
    fs::write(directory.join("verdicts.tsv"), &verdicts).unwrap();
    print!("{verdicts}");
    for record in &records {
        println!(
            "TPDE_COMPILER_A64_BENCH_LEG {} cases={} instruction_ratio={} time_ratio={:.4} verdict={}",
            record.leg,
            record.cases,
            record
                .instruction_ratio
                .map_or("UNRUN".to_owned(), |v| format!("{v:.4}")),
            record.time_ratio,
            record.verdict
        );
    }
    println!("TPDE_COMPILER_A64_BENCH_MISMATCHES {}", mismatches.len());
    println!(
        "\nTPDE_COMPILER_A64_BENCH PASS raw_cases={raw_cases} abi_cases={} rounds={ROUNDS}",
        cases.len() - raw_cases
    );
}
