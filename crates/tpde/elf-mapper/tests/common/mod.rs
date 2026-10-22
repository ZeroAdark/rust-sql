// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#[path = "../../../test-support/oracle.rs"]
mod oracle;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
use tpde_assembler::*;

pub fn checked(command: &mut Command, work: &Path, label: &str) -> Output {
    fs::write(
        work.join(format!("{label}-command.txt")),
        format!("{command:?}\n"),
    )
    .unwrap();
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("{label}: {error}"));
    fs::write(work.join(format!("{label}-stdout.bin")), &output.stdout).unwrap();
    fs::write(work.join(format!("{label}-stderr.txt")), &output.stderr).unwrap();
    assert!(
        output.status.success(),
        "{label}: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

pub struct Reference {
    pub arch: Architecture,
    pub label: String,
    pub executable: PathBuf,
}

pub fn reference_directory() -> PathBuf {
    PathBuf::from(
        std::env::var_os("COMPONENT_ARTIFACT_DIR")
            .expect("component artifact directory is required"),
    )
    .join("elf-mapper-reference-prepared")
}

pub fn receipt(directory: &Path, mode: &str) {
    let mut command = Command::new(std::env::var_os("PYTHON").unwrap_or_else(|| "python3".into()));
    command
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/reference/receipt.py"))
        .arg("--directory")
        .arg(directory)
        .arg("--mode")
        .arg(mode);
    checked(&mut command, directory, &format!("receipt-{mode}"));
}

pub fn references() -> Vec<Reference> {
    let directory = reference_directory();
    receipt(&directory, "verify");
    let mut references = Vec::new();
    for assertions in ["assert", "ndebug"] {
        for sanitizer in ["native", "asan"] {
            for (arch, prefix) in [
                (Architecture::Aarch64, "a64"),
                (Architecture::X86_64, "x64"),
            ] {
                let label = format!("{prefix}-{assertions}-{sanitizer}");
                references.push(Reference {
                    arch,
                    executable: directory.join(&label),
                    label,
                });
            }
        }
    }
    references
}

pub fn build_references(work: &Path) -> Vec<Reference> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sources = root.join("tests/reference");
    let mut prepare = Command::new(std::env::var_os("PYTHON").unwrap_or_else(|| "python3".into()));
    prepare
        .arg(sources.join("prepare.py"))
        .arg("--output")
        .arg(work);
    checked(&mut prepare, work, "prepare");
    let cc = std::env::var_os("CC")
        .or_else(|| std::env::var_os("CXX"))
        .unwrap_or_else(|| "clang".into());
    let cxx = std::env::var_os("CXX").unwrap_or_else(|| "clang++".into());
    let mut references = Vec::new();
    for assertions in [true, false] {
        for sanitized in [false, true] {
            let mode = format!(
                "{}-{}",
                if assertions { "assert" } else { "ndebug" },
                if sanitized { "asan" } else { "native" }
            );
            let mut objects = Vec::new();
            for (encoder, source) in [("disarm", "encode.c"), ("fadec", "encode2.c")] {
                let object = work.join(format!("{encoder}-{mode}.o"));
                let fadec_sanitizer = sanitized && encoder == "fadec";
                let mut compile = Command::new(if fadec_sanitizer { &cxx } else { &cc });
                if fadec_sanitizer {
                    // C++20 defines the promoted sentinel shift that is undefined in the unchanged C11 reference.
                    compile.args(["-x", "c++", "-std=c++20", "-O1", "-g"]);
                } else {
                    compile.args(["-x", "c", "-std=c11", "-O3"]);
                }
                compile
                    .args(["-fno-strict-aliasing", "-I"])
                    .arg(work.join(encoder));
                if !assertions {
                    compile.arg("-DNDEBUG");
                }
                if sanitized {
                    compile.args([
                        if fadec_sanitizer {
                            "-fsanitize=address,undefined,bounds"
                        } else {
                            "-fsanitize=address,undefined"
                        },
                        "-fno-sanitize-recover=all",
                        "-fno-omit-frame-pointer",
                    ]);
                }
                compile
                    .arg("-c")
                    .arg(work.join(encoder).join(source))
                    .arg("-o")
                    .arg(&object);
                checked(&mut compile, work, &format!("compile-{encoder}-{mode}"));
                objects.push(object);
            }
            for arch in [Architecture::Aarch64, Architecture::X86_64] {
                let label = format!(
                    "{}-{mode}",
                    if arch == Architecture::Aarch64 {
                        "a64"
                    } else {
                        "x64"
                    }
                );
                let executable = work.join(&label);
                let mut flags = vec![
                    "-stdlib=libc++".to_owned(),
                    "-I".into(),
                    sources.display().to_string(),
                    "-I".into(),
                    work.join("disarm").display().to_string(),
                    "-I".into(),
                    work.join("fadec").display().to_string(),
                ];
                flags.extend(objects.iter().map(|path| path.display().to_string()));
                if arch == Architecture::X86_64 {
                    flags.push("-DTPDE_ORACLE_X64=1".into());
                }
                if sanitized {
                    flags.extend(
                        [
                            "-fsanitize=address,undefined",
                            "-fno-sanitize-recover=all",
                            "-fno-omit-frame-pointer",
                        ]
                        .map(str::to_owned),
                    );
                }
                let cpp_sources = [
                    "oracle.cpp",
                    "tpde/src/Assembler.cpp",
                    "tpde/src/AssemblerElf.cpp",
                    "tpde/src/StringTable.cpp",
                    "tpde/src/util/SmallVector.cpp",
                ]
                .map(|path| sources.join(path));
                fs::write(work.join(format!("{label}-compile.txt")), format!("CXX={:?}; assertions={assertions}; flags={flags:?}; sources={cpp_sources:?}\n", std::env::var_os("CXX"))).unwrap();
                oracle::compile_cpp_with_flags(
                    &sources.join("tpde/include"),
                    &executable,
                    &cpp_sources,
                    assertions,
                    &flags.iter().map(String::as_str).collect::<Vec<_>>(),
                );
                references.push(Reference {
                    arch,
                    label,
                    executable,
                });
            }
        }
    }
    references
}

pub fn build_unwind_libraries(work: &Path) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (mode, flag, missing_remove) in [
        ("llvm", "-DLLVM_EXPORT=1", false),
        ("gcc", "-DGCC_EXPORT=1", false),
        ("llvm-missing-remove", "-DLLVM_EXPORT=1", true),
    ] {
        let library = work.join(format!("unwind-{mode}.so"));
        let mut command = Command::new(std::env::var_os("CXX").unwrap_or_else(|| "clang++".into()));
        command
            .args([
                "-std=c++20",
                "-O3",
                "-shared",
                "-fPIC",
                "-stdlib=libc++",
                flag,
                "-ldl",
            ])
            .arg(root.join("tests/reference/unwind-probe.cpp"))
            .arg("-o")
            .arg(&library);
        if missing_remove {
            command.arg("-DLLVM_MISSING_REMOVE=1");
        }
        checked(&mut command, work, &format!("compile-unwind-{mode}"));
    }
}

pub fn run(reference: &Reference, work: &Path, label: &str, input: &[u8]) -> Vec<u8> {
    let label = format!("{}-{label}", reference.label);
    let path = work.join(format!("{label}-input.bin"));
    fs::write(&path, input).unwrap();
    let mut command = Command::new(&reference.executable);
    command.stdin(Stdio::from(fs::File::open(path).unwrap()));
    checked(&mut command, work, &label).stdout
}

pub struct Reader<'a>(pub &'a [u8]);
impl Reader<'_> {
    pub fn word(&mut self) -> u64 {
        assert!(self.0.len() >= 8, "truncated C++ scalar");
        let (value, rest) = self.0.split_at(8);
        self.0 = rest;
        u64::from_le_bytes(value.try_into().unwrap())
    }
    pub fn bytes(&mut self) -> Vec<u8> {
        let length = usize::try_from(self.word()).unwrap();
        assert!(self.0.len() >= length, "truncated C++ byte vector");
        let (value, rest) = self.0.split_at(length);
        self.0 = rest;
        value.to_vec()
    }
}

pub struct Case<'m> {
    pub input: Vec<u8>,
    pub asm: AssemblerElf<'m>,
    pub sections: Vec<SecRef>,
    pub symbols: Vec<SymRef>,
    pub resolvers: Vec<(Vec<u8>, u8, u64)>,
    pub native: Vec<(u64, u64)>,
    pub native_symbol: Option<SymRef>,
}
impl<'m> Case<'m> {
    pub fn new(mcx: mcx::Mcx<'m>, arch: Architecture) -> Self {
        Self {
            input: vec![u8::from(arch == Architecture::X86_64)],
            asm: AssemblerElf::new(mcx, arch).unwrap(),
            sections: Vec::new(),
            symbols: Vec::new(),
            resolvers: Vec::new(),
            native: Vec::new(),
            native_symbol: None,
        }
    }
    pub fn word32(&mut self, value: u32) {
        self.input.extend_from_slice(&value.to_le_bytes());
    }
    pub fn word64(&mut self, value: u64) {
        self.input.extend_from_slice(&value.to_le_bytes());
    }
    pub fn bytes(&mut self, bytes: &[u8]) {
        self.word32(bytes.len() as u32);
        self.input.extend_from_slice(bytes);
    }
    fn track_section(&mut self, section: SecRef) {
        if !self.sections.contains(&section) {
            self.sections.push(section);
        }
    }
    fn track_symbol(&mut self, symbol: SymRef) {
        if !self.symbols.contains(&symbol) {
            self.symbols.push(symbol);
        }
    }
    pub fn section(&mut self, kind: SectionKind, default: bool) -> SecRef {
        self.input
            .extend_from_slice(&[u8::from(default), kind as u8]);
        let section = if default {
            self.asm.get_default_section(kind)
        } else {
            self.asm.create_section(kind)
        }
        .unwrap();
        self.track_section(section);
        section
    }
    pub fn raw(&mut self, flags: SectionFlags) -> SecRef {
        self.input.push(2);
        self.word32(flags.kind);
        self.word32(flags.flags);
        self.word32(flags.name);
        self.input.extend_from_slice(&[
            flags.align,
            u8::from(flags.has_relocs),
            u8::from(flags.is_bss),
        ]);
        let section = self.asm.create_section_with_flags(flags).unwrap();
        self.track_section(section);
        section
    }
    pub fn symbol(&mut self, kind: u8, binding: SymBinding, name: &[u8]) -> SymRef {
        self.input.extend_from_slice(&[
            4,
            kind,
            match binding {
                SymBinding::Local => 0,
                SymBinding::Weak => 1,
                SymBinding::Global => 2,
            },
        ]);
        self.bytes(name);
        let symbol = match kind {
            0 => self.asm.sym_add_undef(name, binding),
            1 => self.asm.sym_predef_func(name, binding),
            2 => self.asm.sym_predef_data(name, binding),
            _ => panic!("bad symbol kind"),
        }
        .unwrap();
        self.track_symbol(symbol);
        symbol
    }
    pub fn define(&mut self, symbol: SymRef, section: SecRef, offset: u64, size: u64) {
        self.input.push(5);
        self.word32(symbol.id());
        self.word32(section.id());
        self.word64(offset);
        self.word64(size);
        self.asm.sym_def(symbol, section, offset, size).unwrap();
    }
    pub fn absolute(&mut self, symbol: SymRef, value: u64) {
        self.input.push(37);
        self.word32(symbol.id());
        self.word64(value);
        self.asm.sym_def_absolute(symbol, value, 0).unwrap();
    }
    pub fn alignment(&mut self, section: SecRef, alignment: u32) {
        self.input.push(38);
        self.word32(section.id());
        self.word32(alignment);
        self.asm.get_section_mut(section).unwrap().align = alignment;
    }
    pub fn zero(&mut self, section: SecRef, symbol: SymRef, size: u32, alignment: u32) {
        self.input.push(7);
        self.word32(section.id());
        self.word32(symbol.id());
        self.word32(size);
        self.word32(alignment);
        self.asm
            .sym_def_predef_zero(section, symbol, size, alignment)
            .unwrap();
    }
    pub fn write(&mut self, section: SecRef, bytes: &[u8]) {
        self.input.push(16);
        self.word32(section.id());
        self.bytes(bytes);
        self.asm
            .get_section_mut(section)
            .unwrap()
            .write(bytes)
            .unwrap();
    }
    pub fn relocation(
        &mut self,
        section: SecRef,
        symbol: SymRef,
        kind: u32,
        offset: u32,
        addend: i64,
    ) {
        self.input.push(18);
        self.word32(section.id());
        self.word32(symbol.id());
        self.word32(kind);
        self.word32(offset);
        self.word64(addend as u64);
        self.asm
            .reloc_sec(section, symbol, kind, offset, addend)
            .unwrap();
    }
    pub fn resolver(&mut self, name: &[u8], kind: u8, value: u64) {
        self.input.push(29);
        self.bytes(name);
        self.input.push(kind);
        self.word64(value);
        self.resolvers.push((name.to_vec(), kind, value));
    }
    pub fn native_add(&mut self, section: SecRef, symbol: SymRef, arguments: &[(u64, u64)]) {
        self.input.push(36);
        self.word32(section.id());
        self.word32(symbol.id());
        self.word32(arguments.len() as u32);
        for &(a, b) in arguments {
            self.word64(a);
            self.word64(b);
        }
        let offset = self.asm.get_section(section).unwrap().size();
        let r0 = tpde_disarm::GReg::new(0).unwrap();
        let r1 = tpde_disarm::GReg::new(1).unwrap();
        let lr = tpde_disarm::GReg::new(30).unwrap();
        let add = tpde_disarm::de64_ADDx(r0.into(), r0.into(), r1.into());
        let ret = tpde_disarm::de64_RET(lr.into());
        assert_ne!(add, 0);
        assert_ne!(ret, 0);
        self.asm
            .get_section_mut(section)
            .unwrap()
            .write(&add)
            .unwrap();
        self.asm
            .get_section_mut(section)
            .unwrap()
            .write(&ret)
            .unwrap();
        self.asm.sym_def(symbol, section, offset, 8).unwrap();
        self.native = arguments.to_vec();
        self.native_symbol = Some(symbol);
    }
    pub fn finish(&mut self) {
        self.input.push(30);
    }
}
