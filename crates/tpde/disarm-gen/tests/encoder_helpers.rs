// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause
#![cfg(feature = "upstream-oracle")]

#[path = "../../test-support/oracle.rs"]
mod oracle;

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
use tpde_disarm_gen::{generate, Options};

fn work() -> PathBuf {
    let root = std::env::var_os("TPDE_DISARM_GEN_ARTIFACT_DIR")
        .or_else(|| std::env::var_os("COMPONENT_ARTIFACT_DIR"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = root.join(format!("disarm-gen-helpers-{}", std::process::id()));
    fs::create_dir_all(&path).unwrap();
    fs::canonicalize(path).unwrap()
}

fn compare_source(actual: &str, reference: &Path) {
    let expected = fs::read(reference).unwrap();
    let mismatch = actual
        .as_bytes()
        .iter()
        .zip(&expected)
        .position(|(a, b)| a != b)
        .or_else(|| (actual.len() != expected.len()).then_some(actual.len().min(expected.len())));
    assert!(
        mismatch.is_none(),
        "{}: source mismatch {:?}, Rust bytes {}, upstream bytes {}",
        reference.display(),
        mismatch,
        actual.len(),
        expected.len()
    );
}

fn checked_output(command: &mut Command, dir: &Path, stage: &str) -> Output {
    let result = command
        .output()
        .unwrap_or_else(|error| panic!("{stage}: {error}"));
    fs::write(
        dir.join(format!("helper-{stage}-stdout.bin")),
        &result.stdout,
    )
    .unwrap();
    fs::write(
        dir.join(format!("helper-{stage}-stderr.txt")),
        &result.stderr,
    )
    .unwrap();
    assert!(
        result.status.success(),
        "{stage}: {}\n{}\nfull diagnostics: {}",
        result.status,
        String::from_utf8_lossy(&result.stderr[..result.stderr.len().min(8192)]),
        dir.display()
    );
    result
}

#[test]
fn generated_helper_expressions_match_compiled_upstream() {
    let root = work();
    let mut expected_counts = None;
    let mut total_cases = 0;
    let mut selected_forms = 0;
    for encode_in_header in [false, true] {
        let dir = root.join(if encode_in_header {
            "header"
        } else {
            "private"
        });
        fs::create_dir_all(&dir).unwrap();
        let mut python =
            Command::new(std::env::var_os("PYTHON").unwrap_or_else(|| "python3".into()));
        python
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/encoder_helpers.py"))
            .arg(&dir);
        if encode_in_header {
            python.arg("--encode-in-header");
        }
        checked_output(&mut python, &dir, "prepare");
        let counts: BTreeMap<String, usize> = fs::read_to_string(dir.join("helpers-counts.tsv"))
            .unwrap()
            .lines()
            .map(|line| {
                let (name, value) = line.split_once('\t').unwrap();
                (name.to_owned(), value.parse().unwrap())
            })
            .collect();
        assert!(counts["selected"] > 0);
        assert!(counts["cases"] >= counts["selected"]);
        assert!(counts["record_bytes"] > 0);
        let mode_counts = (counts["selected"], counts["cases"], counts["record_bytes"]);
        if let Some(previous) = expected_counts {
            assert_eq!(
                previous, mode_counts,
                "helper case counts changed between modes"
            );
        }
        expected_counts = Some(mode_counts);
        let generated = generate(
            &[include_str!("../reference/desc.txt")],
            Some(include_str!("../reference/feat.txt")),
            &Options {
                features: Some("really-all".into()),
                coverage: false,
                encode_in_header,
            },
        )
        .unwrap();
        compare_source(&generated.reference_public, &dir.join("public.inc"));
        compare_source(&generated.reference_private, &dir.join("private.inc"));
        fs::write(dir.join("public.rs"), generated.public_rust).unwrap();
        fs::write(dir.join("private.rs"), generated.private_rust).unwrap();
        fs::write(dir.join("manifest-rust.tsv"), generated.manifest).unwrap();
        let source = format!(
            "include!({:?});\nmod generated {{ include!({:?}); include!({:?}); }}\ninclude!({:?});\n",
            dir.join("helpers-runtime.rs"),
            dir.join("public.rs"),
            dir.join("private.rs"),
            dir.join("helpers-main.rs"),
        );
        fs::write(dir.join("helper-consumer.rs"), source).unwrap();
        let executable = dir.join("helper-consumer");
        let mut compiler =
            Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()));
        compiler
            .args(["--edition=2024", "-A", "warnings"])
            .arg(dir.join("helper-consumer.rs"))
            .arg("-o")
            .arg(&executable);
        checked_output(&mut compiler, &dir, "compile");
        let cpp = dir.join("helper-reference");
        oracle::compile_cpp(&dir, &cpp, &[dir.join("helpers.cpp")], true);
        let input_path = dir.join("helpers-input.bin");
        let input = fs::read(&input_path).unwrap();
        assert_eq!(
            input.len(),
            counts["cases"].checked_mul(counts["record_bytes"]).unwrap()
        );
        let mut consumer = Command::new(executable);
        consumer.stdin(Stdio::from(fs::File::open(input_path).unwrap()));
        let rust = checked_output(&mut consumer, &dir, "consumer");
        assert_eq!(rust.stdout.len(), counts["cases"].checked_mul(4).unwrap());
        oracle::compare_bytes(
            &cpp,
            &dir,
            &input,
            &rust.stdout,
            &format!(
                "generated helper expressions, header={encode_in_header}; roster {}",
                dir.display()
            ),
        );
        selected_forms = counts["selected"];
        total_cases += counts["cases"];
    }
    println!(
        "TPDE_DISARM_GEN_HELPER_EXPRESSIONS_OK forms={selected_forms} modes=2 cases={total_cases}"
    );
}
