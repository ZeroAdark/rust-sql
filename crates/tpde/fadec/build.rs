// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

use std::fmt::Write;
use std::path::PathBuf;

use tpde_fadec_gen::{INSTRUCTION_TABLE, Mode, Options, generate};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../fadec-gen/src");
    println!("cargo:rerun-if-changed=../fadec-gen/data/instrs.txt");
    let generated = generate(
        INSTRUCTION_TABLE,
        &Options {
            mode: Mode::Encode2,
            modes: vec![64],
            with_undoc: true,
        },
    )
    .expect("pinned Fadec instruction generation failed");
    let mut wrappers = String::new();
    let mut count = 0;
    for function in generated.private_rs.split("\npub fn ").skip(1) {
        let (signature, body) = function
            .split_once('\n')
            .expect("unexpected generated encoder body");
        let admits_extended_xmm = body.contains("R::enc_evex_");
        let (name, tail) = signature
            .split_once("<R: Encode2Runtime>(buf: &mut [u8; 32], address: usize, flags: u32")
            .expect("unexpected generated encoder signature");
        assert!(name.starts_with("fe64_"));
        assert_ne!(name, "fe64_NOP", "NOP belongs to the handwritten runtime");
        let operands = tail
            .strip_suffix(") -> usize {")
            .expect("unexpected generated encoder result");
        writeln!(wrappers, "#[allow(non_snake_case)]\n#[inline]\npub fn {name}(output: &mut crate::Output<'_>, flags: u32{operands}) -> Result<usize, crate::EncodeError> {{").unwrap();
        let mut arguments = String::new();
        for operand in operands.split(',').filter(|operand| !operand.is_empty()) {
            let (argument, ty) = operand
                .trim()
                .split_once(": ")
                .expect("unexpected generated encoder operand");
            if ty == "impl Into<FeRegGPLH>" {
                writeln!(wrappers, "let {argument}: FeRegGPLH = {argument}.into();\nif !({argument}.index() < 16 || (36..=39).contains(&{argument}.index())) {{ return Err(crate::EncodeError::InvalidOperand); }}").unwrap();
            } else if ty == "FeRegGP" {
                writeln!(wrappers, "if {argument}.index() >= 16 {{ return Err(crate::EncodeError::InvalidOperand); }}").unwrap();
            } else if ty == "FeRegXMM" && !admits_extended_xmm {
                writeln!(wrappers, "if {argument}.index() >= 16 {{ return Err(crate::EncodeError::InvalidOperand); }}").unwrap();
            }
            write!(arguments, ", {argument}").unwrap();
        }
        writeln!(wrappers, "output.encode(|buf, address| crate::raw::{name}::<crate::runtime::Runtime>(buf, address, flags{arguments}))\n}}").unwrap();
        count += 1;
    }
    assert!(count > 0, "the pinned generator emitted no encoders");
    writeln!(
        wrappers,
        "pub const GENERATED_ENCODER_COUNT: usize = {count};"
    )
    .unwrap();
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo OUT_DIR missing"));
    std::fs::write(
        out.join("raw.rs"),
        generated.public_rs + &generated.private_rs,
    )
    .expect("cannot write generated encoder bodies");
    std::fs::write(out.join("wrappers.rs"), wrappers).expect("cannot write encoder wrappers");
    std::fs::write(out.join("forms.tsv"), generated.forms_tsv)
        .expect("cannot write encoder form manifest");
    std::fs::write(out.join("semantic.tsv"), generated.semantic_tsv)
        .expect("cannot write encoder semantic manifest");
    std::fs::write(
        out.join("build_directory.rs"),
        format!("pub const BUILD_DIRECTORY: &str = {:?};\n", out.display().to_string()),
    )
    .expect("cannot write build directory constant");
}
