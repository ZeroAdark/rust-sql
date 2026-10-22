// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tpde_fadec_gen::{Generated, INSTRUCTION_TABLE, Mode, Options, generate};

#[path = "encode2.rs"]
mod encode2_shape;

#[path = "generated-execution/runner.rs"]
mod generated_execution;

fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path.as_ref())
        .unwrap_or_else(|error| panic!("{}: {error}", path.as_ref().display()))
}

fn checked(command: &mut Command, target: &Path, name: &str) -> Output {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("required command {command:?}: {error}"));
    fs::write(target.join(format!("{name}.stdout")), &output.stdout).unwrap();
    fs::write(target.join(format!("{name}.stderr")), &output.stderr).unwrap();
    assert!(
        output.status.success(),
        "{command:?} failed: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(8192)])
    );
    output
}

fn identical(actual: &str, expected: &str, context: &str) {
    if actual == expected {
        return;
    }
    let line = actual
        .lines()
        .zip(expected.lines())
        .position(|(left, right)| left != right)
        .unwrap_or_else(|| actual.lines().count().min(expected.lines().count()));
    let left = actual.lines().nth(line).unwrap_or("<EOF>");
    let right = expected.lines().nth(line).unwrap_or("<EOF>");
    panic!(
        "{context}: mismatch at line {} ({} actual bytes, {} reference bytes)\nactual: {}\nreference: {}",
        line + 1,
        actual.len(),
        expected.len(),
        left.chars().take(1200).collect::<String>(),
        right.chars().take(1200).collect::<String>()
    );
}

fn attempt_stage<T>(
    name: &str,
    work: &Path,
    failures: &mut Vec<String>,
    action: impl FnOnce() -> T,
) -> Option<T> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(action));
    match result {
        Ok(value) => {
            fs::write(work.join(format!("{name}.verdict.txt")), "PASS\n").unwrap();
            Some(value)
        }
        Err(error) => {
            let message = error
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| error.downcast_ref::<&str>().copied())
                .unwrap_or("non-string panic");
            fs::write(
                work.join(format!("{name}.verdict.txt")),
                format!("FAIL\n{message}\n"),
            )
            .unwrap();
            failures.push(name.to_owned());
            None
        }
    }
}

fn semantic(output: &str, mode: Mode) -> String {
    if mode == Mode::Decode {
        return output.to_owned();
    }
    let mut lines: Vec<_> = output.lines().collect();
    lines.sort_unstable();
    lines.join("\n") + "\n"
}

fn compare_generated(actual: &Generated, expected: &Generated, name: &str) {
    identical(
        &actual.public_rs,
        &expected.public_rs,
        &format!("{name} public"),
    );
    identical(
        &actual.private_rs,
        &expected.private_rs,
        &format!("{name} private"),
    );
    identical(
        &actual.semantic_tsv,
        &expected.semantic_tsv,
        &format!("{name} semantics"),
    );
    identical(
        &actual.forms_tsv,
        &expected.forms_tsv,
        &format!("{name} forms"),
    );
    identical(&actual.stats, &expected.stats, &format!("{name} stats"));
    assert_eq!(
        (actual.source_rows, actual.admitted_rows),
        (expected.source_rows, expected.admitted_rows),
        "{name}"
    );
}

fn cli(
    target: &Path,
    source: &Path,
    mode: &str,
    options: &Options,
    stats: bool,
) -> (Generated, String) {
    let name = if stats { "cli-stats" } else { "cli-quiet" };
    let public = target.join(format!("{name}.public.rs"));
    let private = target.join(format!("{name}.private.rs"));
    let forms = target.join(format!("{name}.forms.tsv"));
    let semantics = target.join(format!("{name}.semantic.tsv"));
    let mut command = Command::new(env!("CARGO_BIN_EXE_fadec-gen"));
    for arch in &options.modes {
        command.arg(format!("--{arch}"));
    }
    if options.with_undoc {
        command.arg("--with-undoc");
    }
    if stats {
        command.arg("--stats");
    }
    command
        .arg("--forms")
        .arg(&forms)
        .arg("--semantic")
        .arg(&semantics)
        .arg(mode)
        .arg(source)
        .arg(&public)
        .arg(&private);
    let output = checked(&mut command, target, name);
    assert!(output.stderr.is_empty(), "unexpected CLI stderr for {name}");
    (
        Generated {
            public_rs: read(public),
            private_rs: read(private),
            semantic_tsv: read(semantics),
            forms_tsv: read(forms),
            stats: String::new(),
            source_rows: 0,
            admitted_rows: 0,
        },
        String::from_utf8(output.stdout).unwrap(),
    )
}

fn generated_source(mode: Mode, generated: &Generated) -> String {
    let mut source = String::from("#![allow(warnings)]\n");
    if mode == Mode::Encode2 {
        source.push_str(encode2_shape::SHAPE_RUNTIME);
    }
    source.push_str(&generated.public_rs);
    source.push('\n');
    source.push_str(&generated.private_rs);
    source
}

fn compile_generated(target: &Path, mode: Mode, generated: &Generated) {
    let mut source = generated_source(mode, generated);
    if mode != Mode::Encode2 {
        observe_generated_tables(target, mode, &mut source);
    }
    fs::write(target.join("generated.rs"), source).unwrap();
    checked(
        Command::new("rustc")
            .arg("--edition=2024")
            .arg(if mode == Mode::Encode2 {
                "--crate-type=lib"
            } else {
                "--crate-type=bin"
            })
            .arg("--crate-name=fadec_generated_profile")
            .arg(target.join("generated.rs"))
            .arg("-o")
            .arg(target.join("generated.rlib")),
        target,
        "rustc",
    );
    if mode != Mode::Encode2 {
        let output = checked(
            &mut Command::new(target.join("generated.rlib")),
            target,
            "observed",
        );
        assert!(output.stderr.is_empty());
        let actual = semantic(&String::from_utf8(output.stdout).unwrap(), mode);
        fs::write(target.join("observed.semantic.tsv"), &actual).unwrap();
        identical(
            &actual,
            &read(target.join("reference.semantic.tsv")),
            "executed generated table values",
        );
    }
}

fn verify_negative_cases(work: &Path) -> usize {
    let target = work.join("negative");
    let rows = read(work.join("negative.tsv"));
    let control = read(target.join("control.txt"));
    let control_forms = read(target.join("control.forms.tsv"));
    let mut seen = BTreeSet::new();
    for row in rows.lines().skip(1) {
        let (name, error) = row.split_once('\t').unwrap();
        assert!(!error.is_empty() && seen.insert(name));
        let source = read(target.join(format!("{name}.txt")));
        for undoc in [false, true] {
            assert!(
                tpde_fadec_gen::parser::parse_table(&source, undoc).is_err(),
                "negative parser case {name}, undoc={undoc}"
            );
            let entries = tpde_fadec_gen::parser::parse_table(&control, undoc).unwrap();
            identical(
                &tpde_fadec_gen::source_forms(&control, &entries).unwrap(),
                &control_forms,
                &format!("parser reuse {name}"),
            );
        }
        for (mode_name, mode) in [
            ("decode", Mode::Decode),
            ("encode", Mode::Encode),
            ("encode2", Mode::Encode2),
        ] {
            let options = Options {
                mode,
                modes: vec![64],
                with_undoc: false,
            };
            assert!(
                generate(&source, &options).is_err(),
                "negative generator case {name}, mode={mode_name}"
            );
            let generated = generate(&control, &options).unwrap();
            identical(
                &semantic(&generated.semantic_tsv, mode),
                &read(target.join(format!("control.{mode_name}.semantic.tsv"))),
                &format!("generator reuse {name}, mode={mode_name}"),
            );
        }
    }
    assert_eq!(seen.len(), 30);
    seen.len()
}

fn verify_pipeline_cases(work: &Path) -> usize {
    let target = work.join("pipeline");
    let rows = read(work.join("pipeline.tsv"));
    let mut names = BTreeSet::new();
    for row in rows.lines().skip(1) {
        let fields: Vec<_> = row.split('\t').collect();
        let [name, mode, arch, undoc, accepted] = fields.as_slice() else {
            panic!("invalid pipeline row: {row}")
        };
        assert!(names.insert(*name));
        let mode = Mode::parse(mode).unwrap();
        let options = Options {
            mode,
            modes: arch
                .split(',')
                .map(|value| value.parse().unwrap())
                .collect(),
            with_undoc: *undoc == "1",
        };
        let source = read(target.join(format!("{name}.txt")));
        let generated = generate(&source, &options);
        assert_eq!(generated.is_ok(), *accepted == "1", "pipeline case {name}");
        if let Ok(generated) = generated {
            identical(
                &semantic(&generated.semantic_tsv, mode),
                &read(target.join(format!("{name}.semantic.tsv"))),
                &format!("pipeline case {name}"),
            );
        }
    }
    assert_eq!(names.len(), 9);
    names.len()
}

#[test]
fn command_line_argument_admission() {
    for arguments in [
        "decode in public private",
        "encode2 in public private --forms",
        "encode2 in public private --semantic",
        "encode in public private --unknown",
        "unknown in public private",
        "encode in public",
        "encode in public private extra",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_fadec-gen"))
            .args(arguments.split_whitespace())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{arguments}");
        assert!(output.stdout.is_empty(), "{arguments}");
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .starts_with("fadec-gen:")
        );
    }
    let output = Command::new(env!("CARGO_BIN_EXE_fadec-gen"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success() && output.stderr.is_empty());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("decode,encode,encode2")
    );
}

#[test]
fn all_generator_profiles_match_pinned_upstream() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let work = std::env::var_os("FADEC_GEN_ARTIFACT_DIR")
        .or_else(|| std::env::var_os("COMPONENT_ARTIFACT_DIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("tpde-fadec-gen-oracle-{}", std::process::id()))
        });
    fs::create_dir_all(&work).unwrap();
    fs::write(work.join("verdict.txt"), "FADEC_GEN_ORACLE_UNRUN\n").unwrap();
    let mut failures = Vec::new();
    let profile_path = work.join("profiles.tsv");
    if profile_path.exists() {
        fs::remove_file(&profile_path).unwrap();
    }
    let version = checked(
        Command::new("rustc").arg("--version"),
        &work,
        "rustc-version",
    );
    let version = String::from_utf8(version.stdout).unwrap();
    let number = version.split_whitespace().nth(1).unwrap();
    let mut parts = number.split('.');
    assert_eq!(parts.next(), Some("1"), "unexpected rustc: {version}");
    assert!(
        parts.next().unwrap().parse::<u32>().unwrap() >= 96,
        "the runner requires rustc >= 1.96: {version}"
    );
    let reference = attempt_stage("reference-oracle", &work, &mut failures, || {
        checked(
            Command::new("python3")
                .arg(root.join("tests/oracle.py"))
                .arg("--out")
                .arg(&work)
                .env("PYTHONHASHSEED", "0")
                .env("PYTHONDONTWRITEBYTECODE", "1"),
            &work,
            "reference",
        )
    });
    if let Some(reference) = reference {
        eprint!("{}", String::from_utf8(reference.stdout).unwrap());
    }
    assert_eq!(
        tpde_fadec_gen::UPSTREAM_REVISION,
        "3994d89500985491b1a7ccc17827a22058b3de49"
    );
    assert_eq!(
        tpde_fadec_gen::INPUT_SHA256,
        "bd8911845e6d9900e9dbcc75bf3f36cce9ca378460a0fca4fa8bb5a45d066819"
    );
    identical(
        INSTRUCTION_TABLE,
        &read(root.join("data/instrs.txt")),
        "embedded production input",
    );
    let profile_manifest = read(work.join("profiles.tsv"));
    let mut profiles = BTreeSet::new();
    let mut total_semantic_rows = 0;
    let mut verdicts = String::from("profile\tverdict\tdetail\n");
    for row in profile_manifest.lines().skip(1) {
        let fields: Vec<_> = row.split('\t').collect();
        assert_eq!(fields.len(), 7);
        let [
            name,
            mode_name,
            arch,
            undoc,
            source_count,
            admitted_count,
            semantic_count,
        ] = fields.as_slice()
        else {
            unreachable!()
        };
        assert!(profiles.insert(*name), "duplicate profile {name}");
        let mode = Mode::parse(mode_name).unwrap();
        let options = Options {
            mode,
            modes: arch
                .split(',')
                .filter(|value| !value.is_empty())
                .map(|value| value.parse().unwrap())
                .collect(),
            with_undoc: *undoc == "1",
        };
        let target = work.join(name);
        if mode == Mode::Encode2 {
            let source = target.join("generated.rs");
            if source.exists() {
                fs::remove_file(source).unwrap();
            }
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let generated = generate(INSTRUCTION_TABLE, &options)
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            fs::write(target.join("rust.public.rs"), &generated.public_rs).unwrap();
            fs::write(target.join("rust.private.rs"), &generated.private_rs).unwrap();
            if mode == Mode::Encode2 {
                fs::write(
                    target.join("generated.rs"),
                    generated_source(mode, &generated),
                )
                .unwrap();
            }
            fs::write(target.join("rust.forms.tsv"), &generated.forms_tsv).unwrap();
            fs::write(target.join("rust.semantic.tsv"), &generated.semantic_tsv).unwrap();
            let normalized = semantic(&generated.semantic_tsv, mode);
            fs::write(target.join("rust.normalized.semantic.tsv"), &normalized).unwrap();
            identical(
                &normalized,
                &read(target.join("reference.semantic.tsv")),
                &format!("{name} reference semantics"),
            );
            identical(
                &generated.forms_tsv,
                &read(target.join("reference.forms.tsv")),
                &format!("{name} source identities"),
            );
            assert_eq!(
                generated.source_rows,
                source_count.parse::<usize>().unwrap(),
                "{name}"
            );
            assert_eq!(
                generated.admitted_rows,
                admitted_count.parse::<usize>().unwrap(),
                "{name}"
            );
            assert_eq!(generated.source_rows, 2337);
            assert_eq!(
                generated.admitted_rows,
                if options.with_undoc { 2337 } else { 2323 }
            );
            assert_eq!(
                normalized.lines().count(),
                semantic_count.parse::<usize>().unwrap(),
                "{name}"
            );
            total_semantic_rows += normalized.lines().count();
            let repeat = generate(INSTRUCTION_TABLE, &options).unwrap();
            compare_generated(&repeat, &generated, &format!("{name} deterministic rerun"));
            let (mut quiet, quiet_stdout) = cli(
                &target,
                &root.join("data/instrs.txt"),
                mode_name,
                &options,
                false,
            );
            let (mut stats, stats_stdout) = cli(
                &target,
                &root.join("data/instrs.txt"),
                mode_name,
                &options,
                true,
            );
            assert!(quiet_stdout.is_empty(), "{name} quiet CLI emitted stdout");
            let expected_stdout = if generated.stats.is_empty() {
                String::new()
            } else {
                generated.stats.clone() + "\n"
            };
            identical(
                &stats_stdout,
                &expected_stdout,
                &format!("{name} CLI stats"),
            );
            identical(
                &stats_stdout,
                &read(target.join("reference.stats.txt")),
                &format!("{name} upstream stats"),
            );
            for output in [&mut quiet, &mut stats] {
                output.stats = generated.stats.clone();
                output.source_rows = generated.source_rows;
                output.admitted_rows = generated.admitted_rows;
                compare_generated(output, &generated, &format!("{name} CLI output"));
            }
            compile_generated(&target, mode, &generated);
            let counts: BTreeMap<_, _> = read(target.join("counts.tsv"))
                .lines()
                .skip(1)
                .map(|line| {
                    let (kind, count) = line.split_once('\t').unwrap();
                    (kind.to_owned(), count.parse::<usize>().unwrap())
                })
                .collect();
            eprintln!(
                "FADEC_GEN_ORACLE_PROFILE mode={mode_name} arch={arch} undoc={undoc} source={} admitted={} semantic={} counts={counts:?}",
                generated.source_rows,
                generated.admitted_rows,
                normalized.lines().count()
            );
        }));
        match result {
            Ok(()) => verdicts.push_str(&format!("{name}\tPASS\tcomplete\n")),
            Err(error) => {
                let message = error
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| error.downcast_ref::<&str>().copied())
                    .unwrap_or("non-string panic");
                verdicts.push_str(&format!(
                    "{name}\tFAIL\t{}\n",
                    message.replace(['\n', '\r', '\t'], " ")
                ));
                failures.push((*name).to_owned());
            }
        }
        fs::write(work.join("rust-profile-verdicts.tsv"), &verdicts).unwrap();
    }
    attempt_stage("profile-count", &work, &mut failures, || {
        assert_eq!(profiles.len(), 24)
    });
    let negative = attempt_stage("negative-controls", &work, &mut failures, || {
        verify_negative_cases(&work)
    })
    .unwrap_or(0);
    let pipeline = attempt_stage("pipeline-controls", &work, &mut failures, || {
        verify_pipeline_cases(&work)
    })
    .unwrap_or(0);
    let function_count = attempt_stage("function-coverage", &work, &mut failures, || {
        let functions = read(work.join("function-coverage.tsv"));
        let mut count = 0;
        for row in functions.lines().skip(1) {
            let fields: Vec<_> = row.split('\t').collect();
            assert_eq!(fields.len(), 3);
            assert!(
                fields[2].parse::<usize>().unwrap() > 0,
                "unexercised upstream function: {row}"
            );
            count += 1;
        }
        assert_eq!(count, 32);
        count
    })
    .unwrap_or(0);
    let emitted_execution_cases =
        attempt_stage("generated-execution", &work, &mut failures, || {
            let count = generated_execution::run(root, &work);
            assert!(count > 0);
            count
        })
        .unwrap_or(0);
    if !failures.is_empty() {
        fs::write(
            work.join("verdict.txt"),
            format!(
                "FADEC_GEN_ORACLE_FAIL failed_stages={} stages={failures:?}\n",
                failures.len()
            ),
        )
        .unwrap();
    }
    assert!(
        failures.is_empty(),
        "generator profile failures: {failures:?}; see rust-profile-verdicts.tsv"
    );
    let verdict = format!(
        "FADEC_GEN_ORACLE_PASS profiles=24 primary_profiles=18 boundary_profiles=6 source=2337 documented=2323 weak=13 negative={negative} pipeline={pipeline} functions={function_count} semantic_rows={total_semantic_rows} deterministic=24 stats_pairs=24 generated_compiles=24 table_executions=16 emitted_execution_cases={emitted_execution_cases} revision={} artifacts={}\n",
        tpde_fadec_gen::UPSTREAM_REVISION,
        work.display()
    );
    fs::write(work.join("verdict.txt"), &verdict).unwrap();
    eprint!("{verdict}");
}
fn observe_generated_tables(target: &Path, mode: Mode, source: &mut String) {
    let reference = read(target.join("reference.semantic.tsv"));
    source.push_str("\nfn main() {\n");
    if mode == Mode::Decode {
        for row in reference.lines() {
            let fields: Vec<_> = row.split('\t').collect();
            if fields[0] == "MNEM" {
                source.push_str(&format!("assert_eq!(FDI_{}, {});\n", fields[2], fields[1]));
            } else if fields[0] == "ROOT" {
                source.push_str(&format!(
                    "assert_eq!(FD_TABLE_OFFSET_{}, {});\n",
                    fields[1], fields[2]
                ));
            }
        }
        source.push_str(
            r#"
for (index, word) in FD_DECODE_TABLE_DATA.iter().enumerate() {
    println!("TABLE\t{index}\t{word}");
}
for (index, desc) in FD_DECODE_TABLE_DESCS.iter().enumerate() {
    println!("DESC\t{index}\t{}\t{}\t{}\t{}", desc[0], desc[1], desc[2], desc[3]);
}
assert_eq!(FD_MNEMONICS.len(), FD_DECODE_TABLE_STRTAB2.len());
assert_eq!(FD_MNEMONICS.len(), FD_DECODE_TABLE_STRTAB3.len());
for (index, name) in FD_MNEMONICS.iter().enumerate() {
    let offset = usize::from(FD_DECODE_TABLE_STRTAB2[index]);
    let length = usize::from(FD_DECODE_TABLE_STRTAB3[index]);
    let intel = &FD_DECODE_TABLE_STRTAB1[offset..offset + length];
    println!("MNEM\t{index}\t{name}\t{intel}\t{offset}\t{length}");
}
println!("STRTAB\t{}", FD_DECODE_TABLE_STRTAB1);
for (mode, offset) in FD_TABLE_ROOTS {
    println!("ROOT\t{mode}\t{offset}");
}
"#,
        );
    } else {
        assert_eq!(mode, Mode::Encode);
        for row in reference.lines() {
            let fields: Vec<_> = row.split('\t').collect();
            if fields[0] == "F" {
                source.push_str(&format!(
                    "println!(\"F\\t{}\\t{{}}\", {});\n",
                    fields[1], fields[1]
                ));
            }
        }
        source.push_str("for (index, word) in FE_ALT_TABLE.iter().enumerate() { println!(\"A\\t{index}\\t{word}\"); }\n");
    }
    source.push_str("}\n");
}
