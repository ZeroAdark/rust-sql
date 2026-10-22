// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

#[path = "../../test-support/oracle.rs"]
mod oracle;

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

pub fn checked(command: &mut Command, dir: &Path, name: &str) -> Output {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("{name}: {error}"));
    fs::write(dir.join(format!("{name}-stdout.txt")), &output.stdout).unwrap();
    fs::write(dir.join(format!("{name}-stderr.txt")), &output.stderr).unwrap();
    assert!(
        output.status.success(),
        "{name}: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

pub fn prepare(name: &str) -> PathBuf {
    let root = std::env::var_os("COMPONENT_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = root.join(format!(
        "disarm-runtime-{}-{name}-{}",
        tpde_disarm::GENERATION_PROFILE,
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    let suite = if name.contains("encoder") {
        "encoders"
    } else if name.contains("decode") {
        "decode"
    } else {
        "helpers"
    };
    let mut command = Command::new(std::env::var_os("PYTHON").unwrap_or_else(|| "python3".into()));
    command
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/prepare.py"))
        .arg(&dir)
        .args([
            "--suite",
            suite,
            "--profile",
            tpde_disarm::GENERATION_PROFILE,
        ]);
    checked(&mut command, &dir, "prepare");
    dir
}

pub fn compile_c(dir: &Path, wrapper: &str, assertions: bool, sanitizer: bool) -> PathBuf {
    let mode = format!(
        "{}-{}",
        if assertions { "assert" } else { "ndebug" },
        if sanitizer { "sanitized" } else { "native" }
    );
    let cc = std::env::var_os("CC")
        .or_else(|| std::env::var_os("CXX"))
        .unwrap_or_else(|| "clang".into());
    let mut objects = Vec::new();
    for source in ["classify.c", "decode.c", "encode.c", "format.c"] {
        let object = dir.join(format!("{source}-{mode}.o"));
        let mut command = Command::new(&cc);
        command
            .args(["-x", "c", "-std=c11", "-O3", "-fno-strict-aliasing", "-I"])
            .arg(dir);
        if !assertions {
            command.arg("-DNDEBUG");
        }
        if sanitizer {
            command.args([
                "-fsanitize=address,undefined",
                "-fno-sanitize-recover=all",
                "-fno-omit-frame-pointer",
            ]);
        }
        command
            .arg("-c")
            .arg(dir.join(source))
            .arg("-o")
            .arg(&object);
        checked(&mut command, dir, &format!("compile-{source}-{mode}"));
        objects.push(object);
    }
    let wrapper_path = if dir.join(wrapper).exists() {
        dir.join(wrapper)
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join(wrapper)
    };
    let executable = dir.join(format!("{}-{mode}", wrapper.trim_end_matches(".cpp")));
    let mut flags = objects
        .iter()
        .map(|p| p.to_str().unwrap())
        .collect::<Vec<_>>();
    if sanitizer {
        flags.extend([
            "-fsanitize=address,undefined",
            "-fno-sanitize-recover=all",
            "-fno-omit-frame-pointer",
        ]);
    }
    oracle::compile_cpp_with_flags(dir, &executable, &[wrapper_path], assertions, &flags);
    fs::write(dir.join(format!("build-{mode}.txt")), format!("C: {} -x c -std=c11 -O3 -fno-strict-aliasing; assertions={assertions}; address+undefined sanitizers={sanitizer}\nC++: {} -std=c++20 -O3; wrapper={wrapper}\n", cc.to_string_lossy(), std::env::var("CXX").unwrap_or_else(|_| "clang++".into()))).unwrap();
    executable
}

pub fn compare(dir: &Path, executable: &Path, input: &[u8], expected: &[u8], case: &str) {
    let name = executable.file_name().unwrap().to_str().unwrap();
    let input_path = dir.join("oracle-input.bin");
    fs::write(&input_path, input).unwrap();
    let result = Command::new(executable)
        .stdin(Stdio::from(fs::File::open(input_path).unwrap()))
        .output()
        .unwrap_or_else(|error| panic!("{case}: {name}: {error}"));
    fs::write(dir.join(format!("{name}-stdout.bin")), &result.stdout).unwrap();
    fs::write(dir.join(format!("{name}-stderr.txt")), &result.stderr).unwrap();
    assert!(
        result.status.success(),
        "{case}: {name} failed: {}\n{}",
        result.status,
        String::from_utf8_lossy(&result.stderr)
    );
    let mismatch = expected
        .iter()
        .zip(&result.stdout)
        .position(|(rust, reference)| rust != reference);
    assert!(
        mismatch.is_none() && expected.len() == result.stdout.len(),
        "{case}: {name}: first mismatch {:?}, Rust length {}, C length {}",
        mismatch.or_else(|| (expected.len() != result.stdout.len())
            .then_some(expected.len().min(result.stdout.len()))),
        expected.len(),
        result.stdout.len()
    );
}

pub struct Reference {
    assertions: bool,
    sanitized: bool,
    executable: PathBuf,
}

pub fn compile_all(dir: &Path, wrapper: &str) -> Vec<Reference> {
    let mut outcomes = String::from("assertions\tsanitized\tverdict\n");
    let mut failures = Vec::new();
    let mut references = Vec::new();
    for assertions in [true, false] {
        for sanitized in [false, true] {
            let result =
                std::panic::catch_unwind(|| compile_c(dir, wrapper, assertions, sanitized));
            let verdict = if result.is_ok() { "PASS" } else { "FAIL" };
            outcomes.push_str(&format!("{assertions}\t{sanitized}\t{verdict}\n"));
            fs::write(dir.join("c-compile-verdicts.tsv"), &outcomes).unwrap();
            match result {
                Ok(executable) => references.push(Reference {
                    assertions,
                    sanitized,
                    executable,
                }),
                Err(_) => failures.push(format!("assertions={assertions}, sanitized={sanitized}")),
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{wrapper}: C compilation failed: {}",
        failures.join("; ")
    );
    assert_eq!(references.len(), 4);
    references
}

pub fn compare_all(
    dir: &Path,
    references: &[Reference],
    input: &[u8],
    expected: &[u8],
    case: &str,
) -> usize {
    assert_eq!(references.len(), 4);
    let mut outcomes = String::from("assertions\tsanitized\tverdict\n");
    let mut failures = Vec::new();
    for reference in references {
        let Reference {
            assertions,
            sanitized,
            executable,
        } = reference;
        let result = std::panic::catch_unwind(|| compare(dir, executable, input, expected, case));
        let verdict = if result.is_ok() { "PASS" } else { "FAIL" };
        outcomes.push_str(&format!("{assertions}\t{sanitized}\t{verdict}\n"));
        fs::write(dir.join("c-mode-verdicts.tsv"), &outcomes).unwrap();
        if result.is_err() {
            failures.push(format!("assertions={assertions}, sanitized={sanitized}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{case}: C modes failed: {}",
        failures.join("; ")
    );
    references.len()
}

pub fn counts(path: &Path) -> BTreeMap<String, usize> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| {
            let (key, value) = line.split_once('\t').unwrap();
            (key.into(), value.parse().unwrap())
        })
        .collect()
}
