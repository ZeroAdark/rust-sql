// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause
#![cfg(feature = "upstream-oracle")]

#[path = "../../test-support/oracle.rs"]
mod oracle;

use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

use tpde_disarm_gen::{generate, Options};

const DESC: &str = include_str!("../reference/desc.txt");
const FEATURES: &str = include_str!("../reference/feat.txt");

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
    pub fn da_sext(_:i64,_:u32)->i64 { panic!("encoder runtime excluded") }
    pub fn da_immadd(_:i64)->u32 { panic!("encoder runtime excluded") }
    pub fn da_immlogical(_:u64,_:u32)->u32 { panic!("encoder runtime excluded") }
    pub fn da_immfmov32(_:f32)->u32 { panic!("encoder runtime excluded") }
    pub fn da_immfmov64(_:f64)->u32 { panic!("encoder runtime excluded") }
    pub fn da_immsimdmovi(_:u64)->u32 { panic!("encoder runtime excluded") }
}
"#;

const CONSUMER: &str = r#"
fn main() {
    use std::io::{Read, Write};
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).unwrap();
    assert_eq!(input.len() % 8, 0);
    let mut output = std::io::BufWriter::new(std::io::stdout().lock());
    for record in input.chunks_exact(8) {
        let inst = u32::from_le_bytes(record[..4].try_into().unwrap());
        let mnem = u32::from_le_bytes(record[4..].try_into().unwrap());
        assert_ne!(mnem, 0);
        assert_eq!(generated::da64_classify_impl(inst), mnem);
        let mut state = reset();
        generated::da64_decode_operands::<Recorder>(inst, mnem, &mut state);
        for word in finish(&state) {
            output.write_all(&word.to_le_bytes()).unwrap();
        }
    }
    output.flush().unwrap();
}
"#;

fn work(name: &str) -> std::path::PathBuf {
    std::env::var_os("COMPONENT_ARTIFACT_DIR")
        .or_else(|| std::env::var_os("TPDE_DISARM_GEN_ARTIFACT_DIR"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("disarm-decoder-{name}-{}", std::process::id()))
}

fn compare_source(actual: &str, path: &Path) {
    let expected = fs::read(path).unwrap();
    let mismatch = actual
        .as_bytes()
        .iter()
        .zip(&expected)
        .position(|(a, b)| a != b)
        .or_else(|| (actual.len() != expected.len()).then_some(actual.len().min(expected.len())));
    assert!(
        mismatch.is_none(),
        "{}: source mismatch at {:?}; actual {}, expected {} bytes",
        path.display(),
        mismatch,
        actual.len(),
        expected.len()
    );
}

fn check_decoder(dir: &Path, desc: &str, profile: &str, coverage: bool, assertions: &str) -> usize {
    let name = dir.display().to_string();
    fs::create_dir_all(dir).unwrap();
    let descfile = dir.join("input.ini");
    fs::write(&descfile, desc).unwrap();
    let mut python = Command::new(std::env::var_os("PYTHON").unwrap_or_else(|| "python3".into()));
    python
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/decoder_cases.py"))
        .arg(dir)
        .args(["--features", profile])
        .arg("--desc")
        .arg(descfile);
    if coverage {
        python.arg("--coverage");
    }
    let prepared = python.output().expect("start decoder source oracle");
    fs::write(dir.join("decoder-prepare.txt"), &prepared.stdout).unwrap();
    fs::write(dir.join("decoder-prepare-stderr.txt"), &prepared.stderr).unwrap();
    assert!(
        prepared.status.success(),
        "{name}: decoder source preparation failed: {}\n{}",
        prepared.status,
        String::from_utf8_lossy(&prepared.stderr)
    );
    let generated = generate(
        &[desc],
        Some(FEATURES),
        &Options {
            features: Some(profile.into()),
            coverage,
            encode_in_header: false,
        },
    )
    .unwrap();
    compare_source(&generated.reference_public, &dir.join("public.inc"));
    compare_source(&generated.reference_private, &dir.join("private.inc"));
    fs::write(dir.join("public.rs"), generated.public_rust).unwrap();
    fs::write(dir.join("private.rs"), generated.private_rust).unwrap();
    fs::write(dir.join("manifest-rust.tsv"), generated.manifest).unwrap();
    let consumer = CONSUMER.replace("fn main() {", &format!("fn main() {{\n{assertions}"));
    let source = format!(
        "{RUNTIME}\nmod generated {{ include!({:?}); include!({:?}); }}\ninclude!({:?});\n{consumer}",
        dir.join("public.rs"), dir.join("private.rs"), dir.join("decoder-recorders.rs"),
    );
    fs::write(dir.join("decoder-consumer.rs"), source).unwrap();
    let rust = dir.join("decoder-consumer");
    let compile = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .args(["--edition=2024", "-A", "warnings"])
        .arg(dir.join("decoder-consumer.rs"))
        .arg("-o")
        .arg(&rust)
        .output()
        .expect("start Rust compiler");
    fs::write(dir.join("decoder-compile-stderr.txt"), &compile.stderr).unwrap();
    assert!(
        compile.status.success(),
        "{name}: decoder consumer compilation failed: {}\n{}",
        compile.status,
        String::from_utf8_lossy(&compile.stderr)
    );
    let cpp = dir.join("decoder-reference");
    oracle::compile_cpp(dir, &cpp, &[dir.join("decoder-reference.cpp")], true);
    let input = fs::read(dir.join("decoder-cases.bin")).unwrap();
    assert!(!input.is_empty(), "{name}: empty decoder case roster");
    assert_eq!(input.len() % 8, 0);
    let result = Command::new(&rust)
        .stdin(Stdio::from(
            fs::File::open(dir.join("decoder-cases.bin")).unwrap(),
        ))
        .output()
        .expect("start generated decoder consumer");
    assert!(
        result.status.success(),
        "{name}: decoder consumer failed: {}\n{}",
        result.status,
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout.len(), input.len() / 8 * 6 * 8);
    fs::write(dir.join("decoder-rust.bin"), &result.stdout).unwrap();
    oracle::compare_bytes(&cpp, dir, &input, &result.stdout, &name);
    input.len() / 8
}

#[test]
fn generated_decoder_calls_match_pinned_c_argument_semantics() {
    let root = work("full");
    let mut modes = 0;
    let mut cases = 0;
    for profile in ["all", "really-all"] {
        for coverage in [false, true] {
            let name = format!("{profile}-{coverage}");
            let dir = root.join(&name);
            cases += check_decoder(&dir, DESC, profile, coverage, "");
            modes += 1;
        }
    }
    println!("TPDE_DISARM_GEN_DECODER_CALLS_OK modes={modes} cases={cases}");
}

#[test]
fn decoder_integer_scope_and_unicode_tables_match_pinned_c() {
    let root = work("boundaries");
    let outer = "raw OPuimm(inst + 1 < inst) OPuimm(~mnem > 0xffffffffu) OPuimm(~DA64I_BASE > 0xffffffffu) OPuimm(~DA64G_BASE > 0xffffffffu) OPuimm(DA64_HAVE_ ? 1 : 0)";
    let check_outer = r#"
        let mut state = reset();
        generated::da64_decode_operands::<Recorder>(u32::MAX, generated::DA64I_BASE, &mut state);
        for (actual, expected) in state.ops.into_iter().zip([1, 0, 0, 0, 1]) {
            assert_eq!(actual, <Recorder as generated::DecoderHelpers>::OPuimm(expected));
        }
        assert_eq!(generated::de64_BASE(), generated::DA64I_BASE);
    "#;
    let mut entries = Vec::new();
    for field in ["raw", "inst", "mnem"] {
        entries.push((format!("outer-{field}"), format!(
            "[BASE]\ndesc: {{32:{field}}}\nmnem: BASE\ndecode: {outer}\nencmnem: BASE\nencops: {field}=const/val=DA64I_BASE\nenccond: DA64_HAVE_&&~DA64I_BASE<=0xffffffffu\n"
        ), check_outer));
    }
    entries.push(("referenced-shadow".into(),
        "[BASE]\ndesc: {32:mnem}\nmnem: BASE\ndecode: shadow OPuimm(@mnem + 1 < @mnem) OPuimm(~@mnem > 0xffffffffu)\n".into(),
        r#"
            let mut state = reset();
            generated::da64_decode_operands::<Recorder>(u32::MAX, generated::DA64I_BASE, &mut state);
            for (actual, expected) in state.ops.into_iter().take(2).zip([0, 1]) {
                assert_eq!(actual, <Recorder as generated::DecoderHelpers>::OPuimm(expected));
            }
        "#));
    entries.push((
        "unicode-tables".into(),
        format!(
        "[BASE]\ndesc: {}\nmnem: BASE\ndecode: é\n[SECOND]\ndesc: {}1\nmnem: SECOND\ndecode: z\n",
        "0".repeat(32), "0".repeat(31)),
        r#"
            assert_eq!(generated::DA64_DECSTR, "éz");
            assert_eq!(generated::DA64_DECSTRTAB[generated::DA64I_BASE as usize], 0x1000);
            assert_eq!(generated::DA64_DECSTRTAB[generated::DA64I_SECOND as usize], 0x1001);
        "#,
    ));
    let mut cases = 0;
    let mut modes = 0;
    let mut failures = Vec::new();
    for (name, desc, assertions) in entries {
        for coverage in [false, true] {
            let mode = format!("{name}-{coverage}");
            match std::panic::catch_unwind(|| {
                check_decoder(&root.join(&mode), &desc, "all", coverage, assertions)
            }) {
                Ok(count) => cases += count,
                Err(_) => failures.push(mode),
            }
            modes += 1;
        }
    }
    assert!(
        failures.is_empty(),
        "decoder boundary cases failed: {}",
        failures.join(", ")
    );
    println!("TPDE_DISARM_GEN_DECODER_BOUNDARIES_OK modes={modes} cases={cases}");
}
