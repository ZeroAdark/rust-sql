// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

#[path = "../../test-support/oracle.rs"]
mod oracle;

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tpde_disarm_gen::{generate, Options};

const RUNTIME: &str = r#"
mod runtime {
    #[derive(Clone, Copy)] pub struct GReg(pub u8);
    #[derive(Clone, Copy)] pub struct GRegZr(pub u8);
    #[derive(Clone, Copy)] pub struct GRegSp(pub u8);
    #[derive(Clone, Copy)] pub struct VReg(pub u8);
    impl GReg { pub fn value(self)->u8 { self.0 } }
    impl GRegZr { pub fn value(self)->u8 { self.0 } }
    impl GRegSp { pub fn value(self)->u8 { self.0 } }
    impl VReg { pub fn value(self)->u8 { self.0 } }
    pub type Cond = u32;
    pub type PrfOp = u32;
    pub fn da_sext(_:i64,_:u32)->i64 { panic!("excluded runtime helper") }
    pub fn da_immadd(_:i64)->u32 { panic!("excluded runtime helper") }
    pub fn da_immlogical(_:u64,_:u32)->u32 { panic!("excluded runtime helper") }
    pub fn da_immfmov32(_:f32)->u32 { panic!("excluded runtime helper") }
    pub fn da_immfmov64(_:f64)->u32 { panic!("excluded runtime helper") }
    pub fn da_immsimdmovi(_:u64)->u32 { panic!("excluded runtime helper") }
}
"#;

fn work() -> PathBuf {
    let root = std::env::var_os("TPDE_DISARM_GEN_ARTIFACT_DIR")
        .or_else(|| std::env::var_os("COMPONENT_ARTIFACT_DIR"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = root.join(format!("disarm-gen-encoders-{}", std::process::id()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn compare_source(actual: &str, reference: &Path) {
    let expected = fs::read(reference).unwrap();
    let mismatch = actual
        .as_bytes()
        .iter()
        .zip(&expected)
        .position(|(a, b)| a != b);
    assert!(
        mismatch.is_none() && actual.len() == expected.len(),
        "{}: source mismatch {:?}, Rust bytes {}, upstream bytes {}",
        reference.display(),
        mismatch,
        actual.len(),
        expected.len()
    );
}

#[test]
fn every_helper_free_generated_encoder_matches_compiled_upstream() {
    let root = work();
    let mut total_cases = 0;
    let mut selected_forms = 0;
    let mut excluded_forms = 0;
    for encode_in_header in [false, true] {
        let dir = root.join(if encode_in_header {
            "header"
        } else {
            "private"
        });
        let mut command =
            Command::new(std::env::var_os("PYTHON").unwrap_or_else(|| "python3".into()));
        command
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/encoder_cases.py"))
            .arg(&dir);
        if encode_in_header {
            command.arg("--encode-in-header");
        }
        let result = command.output().unwrap();
        assert!(
            result.status.success(),
            "encoder case generation failed: {}\n{}",
            result.status,
            String::from_utf8_lossy(&result.stderr)
        );
        let counts: BTreeMap<String, usize> = fs::read_to_string(dir.join("encoder-counts.tsv"))
            .unwrap()
            .lines()
            .map(|line| {
                let (name, value) = line.split_once('\t').unwrap();
                (name.to_owned(), value.parse().unwrap())
            })
            .collect();
        assert!(counts["selected"] > 0 && counts["excluded"] > 0);
        assert!(counts["cases"] >= counts["selected"]);
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
            "{RUNTIME}\nmod generated {{ include!({:?}); include!({:?}); }}\n{}",
            dir.join("public.rs"),
            dir.join("private.rs"),
            fs::read_to_string(dir.join("encoder-main.rs")).unwrap(),
        );
        fs::write(dir.join("consumer.rs"), source).unwrap();
        let executable = dir.join("consumer");
        let compiled = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .args(["--edition=2024", "-A", "warnings"])
            .arg(dir.join("consumer.rs"))
            .arg("-o")
            .arg(&executable)
            .output()
            .unwrap();
        assert!(
            compiled.status.success(),
            "generated encoder Rust failed to compile: {}\n{}",
            compiled.status,
            String::from_utf8_lossy(&compiled.stderr)
        );
        let input_path = dir.join("encoder-input.bin");
        let input = fs::read(&input_path).unwrap();
        assert_eq!(input.len(), counts["cases"] * counts["record_bytes"]);
        let rust = Command::new(executable)
            .stdin(Stdio::from(fs::File::open(input_path).unwrap()))
            .output()
            .unwrap();
        assert!(
            rust.status.success(),
            "generated encoder Rust failed: {}\n{}",
            rust.status,
            String::from_utf8_lossy(&rust.stderr)
        );
        assert_eq!(rust.stdout.len(), counts["cases"] * 4);
        fs::write(dir.join("encoder-rust-output.bin"), &rust.stdout).unwrap();
        let cpp = dir.join("encoder-reference");
        oracle::compile_cpp(&dir, &cpp, &[dir.join("encoder.cpp")], true);
        oracle::compare_bytes(
            &cpp,
            &dir,
            &input,
            &rust.stdout,
            &format!(
                "all helper-free encoders, header={encode_in_header}; roster {}",
                dir.display()
            ),
        );
        if encode_in_header {
            assert_eq!(selected_forms, counts["selected"]);
            assert_eq!(excluded_forms, counts["excluded"]);
        }
        selected_forms = counts["selected"];
        excluded_forms = counts["excluded"];
        total_cases += counts["cases"];
    }
    println!(
        "TPDE_DISARM_GEN_ENCODER_RUST_OK forms={selected_forms} excluded_helpers={excluded_forms} modes=2 cases={total_cases}"
    );
}
