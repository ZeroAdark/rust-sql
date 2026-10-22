// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

#[allow(dead_code)]
mod common;

use common::{Case, Reader, Reference};
use std::{
    fs,
    io::{BufWriter, Write as _},
    path::Path,
};
use tpde_assembler::{elf::*, *};
use tpde_elf_mapper::{ElfMapper, MapperImage};

#[derive(Clone, Copy, Debug)]
enum Domain {
    Defined,
    Relative(i64),
    Fixed(u64),
    WeakNull,
    GlobalNull,
}
#[derive(Clone, Copy, Debug)]
enum Fixture {
    Layout(u8),
    Absolute(u64),
    Relocation {
        kind: u32,
        domain: Domain,
        addend: i32,
    },
    Reuse(bool),
    Extended,
    Native,
}
fn absolute(arch: Architecture) -> u32 {
    if arch == Architecture::Aarch64 {
        R_AARCH64_ABS64
    } else {
        R_X86_64_64
    }
}
fn fixtures(arch: Architecture) -> Vec<Fixture> {
    let kinds: &[u32] = if arch == Architecture::Aarch64 {
        &[
            R_AARCH64_ABS64,
            R_AARCH64_PREL32,
            R_AARCH64_CALL26,
            R_AARCH64_ADR_PREL_PG_HI21,
            R_AARCH64_ADD_ABS_LO12_NC,
            R_AARCH64_ADR_GOT_PAGE,
            R_AARCH64_LD64_GOT_LO12_NC,
        ]
    } else {
        &[
            R_X86_64_64,
            R_X86_64_PC32,
            R_X86_64_PLT32,
            R_X86_64_GOTPCREL,
        ]
    };
    let mut result: Vec<_> = (0..12).map(Fixture::Layout).collect();
    result.extend([0, 1, 4095, 4096, 0x1234_5678, 1 << 32, u64::MAX].map(Fixture::Absolute));
    for &kind in kinds {
        for domain in [
            Domain::Defined,
            Domain::Relative(0),
            Domain::Relative(1),
            Domain::Relative(-1),
            Domain::Relative(4095),
            Domain::Relative(4096),
            Domain::Relative(-4096),
            Domain::Relative((1 << 27) - 4),
            Domain::Relative(1 << 27),
            Domain::Relative(-(1 << 27)),
            Domain::Relative(1 << 31),
            Domain::Relative(-(1 << 31)),
            Domain::Relative(1 << 32),
            Domain::Relative(-(1 << 32)),
            Domain::Fixed(0x1234_5678),
            Domain::WeakNull,
            Domain::GlobalNull,
        ] {
            result.push(Fixture::Relocation {
                kind,
                domain,
                addend: 0,
            });
        }
        for addend in [i32::MIN, -4096, -4, -1, 1, 4, 4096, i32::MAX] {
            result.push(Fixture::Relocation {
                kind,
                domain: Domain::Fixed(0x1234_5678),
                addend,
            });
        }
    }
    result.push(Fixture::Relocation {
        kind: u32::MAX,
        domain: Domain::Defined,
        addend: 0,
    });
    result.push(Fixture::Relocation {
        kind: u32::MAX,
        domain: Domain::Fixed(0x1234_5678),
        addend: 0,
    });
    result.push(Fixture::Reuse(false));
    result.push(Fixture::Reuse(true));
    result.push(Fixture::Extended);
    if arch == Architecture::Aarch64 {
        result.push(Fixture::Native);
    }
    result
}
fn arguments() -> Vec<(u64, u64)> {
    let boundaries = [
        0,
        1,
        2,
        u32::MAX as u64,
        1 << 32,
        i64::MAX as u64,
        1 << 63,
        u64::MAX,
    ];
    let mut result = Vec::new();
    for a in boundaries {
        for b in boundaries {
            result.push((a, b));
        }
    }
    let mut state = 0x1357_9bdf_0246_8aceu64;
    for _ in 0..64 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let a = state;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        result.push((a, state));
    }
    assert_eq!(result.len(), 128);
    result
}
fn build<'m>(mcx: mcx::Mcx<'m>, arch: Architecture, fixture: Fixture) -> Case<'m> {
    let mut case = Case::new(mcx, arch);
    case.section(SectionKind::EHFrame, true);
    match fixture {
        Fixture::Layout(seed) => {
            let kinds = SectionKind::ALL;
            for index in 0..kinds.len() {
                let kind = kinds[(index + seed as usize) % kinds.len()];
                let section = case.section(kind, true);
                if kind == SectionKind::EHFrame {
                    continue;
                }
                let name = format!("layout.{kind:?}");
                let symbol = case.symbol(
                    2,
                    if index % 2 == 0 {
                        SymBinding::Global
                    } else {
                        SymBinding::Local
                    },
                    name.as_bytes(),
                );
                let length = (index * 13 + seed as usize * 7) % 67;
                if kind == SectionKind::BSS || kind == SectionKind::ThreadBSS {
                    case.zero(section, symbol, length as u32, 1 << (seed % 7));
                } else {
                    let bytes: Vec<_> = (0..length)
                        .map(|i| (i * 131 + index + seed as usize) as u8)
                        .collect();
                    case.write(section, &bytes);
                    case.define(symbol, section, 0, length as u64);
                }
            }
            for index in 0..12u32 {
                let flags = SectionFlags {
                    kind: SHT_PROGBITS,
                    flags: if index % 4 == 0 {
                        0
                    } else {
                        SHF_ALLOC
                            | if index % 3 == 0 { SHF_EXECINSTR } else { 0 }
                            | if index % 5 == 0 { SHF_WRITE } else { 0 }
                    },
                    name: 0,
                    align: 1 << ((index + seed as u32) % 7),
                    has_relocs: false,
                    is_bss: false,
                };
                let section = case.raw(flags);
                case.write(
                    section,
                    &vec![0x80 | index as u8; (index as usize * 11 + seed as usize) % 47],
                );
                if index == 11 && seed >= 8 {
                    case.alignment(section, 1 << (12 + seed - 8));
                }
            }
            case.symbol(0, SymBinding::Global, b"unused.unresolved.global");
            case.symbol(0, SymBinding::Weak, b"unused.unresolved.weak");
        }
        Fixture::Absolute(value) => {
            let text = case.section(SectionKind::Text, true);
            case.write(text, &[0x55; 32]);
            for (index, binding) in [SymBinding::Local, SymBinding::Global, SymBinding::Weak]
                .into_iter()
                .enumerate()
            {
                let name = format!("absolute.{index}");
                let symbol = case.symbol(0, binding, name.as_bytes());
                case.absolute(symbol, value);
                case.relocation(text, symbol, absolute(arch), index as u32 * 8, 0);
            }
        }
        Fixture::Relocation {
            kind,
            domain,
            addend,
        } => {
            let text = case.section(SectionKind::Text, true);
            case.write(text, &[0x55; 64]);
            let target = case.section(SectionKind::Data, true);
            case.write(target, &[0xa7; 32]);
            let symbol = case.symbol(
                0,
                if matches!(domain, Domain::WeakNull) {
                    SymBinding::Weak
                } else {
                    SymBinding::Global
                },
                b"target",
            );
            match domain {
                Domain::Defined => case.define(symbol, target, 8, 16),
                Domain::Relative(value) => case.resolver(b"target", 1, value as u64),
                Domain::Fixed(value) => case.resolver(b"target", 0, value),
                Domain::WeakNull | Domain::GlobalNull => case.resolver(b"target", 2, 0),
            }
            for offset in [0, 16, 32] {
                case.relocation(text, symbol, kind, offset, addend as i64);
            }
            if kind == u32::MAX && matches!(domain, Domain::Defined) {
                let witness = case.symbol(0, SymBinding::Global, b"allocation-witness");
                case.resolver(b"allocation-witness", 1, 0);
                case.relocation(text, witness, absolute(arch), 48, 0);
            }
        }
        Fixture::Reuse(weak) => {
            let text = case.section(SectionKind::Text, true);
            case.write(text, &[0x5a; 256]);
            let kinds: &[u32] = if arch == Architecture::Aarch64 {
                &[
                    R_AARCH64_CALL26,
                    R_AARCH64_ADR_GOT_PAGE,
                    R_AARCH64_LD64_GOT_LO12_NC,
                    R_AARCH64_ABS64,
                ]
            } else {
                &[R_X86_64_PLT32, R_X86_64_GOTPCREL, R_X86_64_64]
            };
            for index in 0..3 {
                let name = format!("cached.{index}");
                let symbol = case.symbol(
                    0,
                    if weak {
                        SymBinding::Weak
                    } else {
                        SymBinding::Global
                    },
                    name.as_bytes(),
                );
                case.resolver(
                    name.as_bytes(),
                    if weak { 2 } else { 0 },
                    0x1234_0000 + index as u64 * 4096,
                );
                for repeat in 0..2 {
                    for (kind_index, &kind) in kinds.iter().enumerate() {
                        case.relocation(
                            text,
                            symbol,
                            kind,
                            ((index * 2 + repeat) * 32 + kind_index * 8) as u32,
                            0,
                        );
                    }
                }
            }
        }
        Fixture::Extended => {
            case.input.push(24);
            case.word32(SHN_LORESERVE as u32);
            let last = loop {
                let section = case.asm.create_section(SectionKind::ReadOnly).unwrap();
                case.sections.push(section);
                if section.id() >= SHN_LORESERVE as u32 {
                    break section;
                }
            };
            case.write(last, b"extended-section-index");
            let target = case.symbol(2, SymBinding::Global, b"extended.global");
            case.define(target, last, 4, 8);
            let local = case.symbol(2, SymBinding::Local, b"extended.local");
            case.define(local, last, 0, 4);
            let text = case.section(SectionKind::Text, true);
            case.write(text, &[0; 24]);
            case.relocation(text, target, absolute(arch), 0, 0);
            case.relocation(text, local, absolute(arch), 8, 0);
        }
        Fixture::Native => {
            let text = case.section(SectionKind::Text, true);
            let function = case.symbol(1, SymBinding::Global, b"native.add");
            case.native_add(text, function, &arguments());
            let data = case.section(SectionKind::Data, true);
            case.write(data, &[0xa5; 32]);
            let bss = case.section(SectionKind::BSS, true);
            let storage = case.symbol(2, SymBinding::Global, b"native.bss");
            case.zero(bss, storage, 37, 16);
        }
    }
    case.finish();
    case
}
fn compare(
    case: &mut Case<'_>,
    fixture: Fixture,
    reference: &Reference,
    work: &Path,
    name: &str,
) -> (bool, usize) {
    let output = common::run(reference, work, name, &case.input);
    let mut reader = Reader(&output);
    let page = reader.word() as usize;
    let cpp_success = reader.word() != 0;
    let base = reader.word() as usize;
    assert_ne!(
        base, 0,
        "reference allocation did not reach the mapper fixture"
    );
    let mapped_size = reader.word() as usize;
    let context = mcx::MemoryContext::new_bump("mapper-portable-image");
    let mut image = MapperImage::new(context.mcx()).unwrap();
    let size = image.layout(&mut case.asm, page).unwrap();
    let mut bytes = vec![0xa5; size + 32];
    let mut calls = Vec::new();
    let success = image
        .relocate(&case.asm, &mut bytes[..size], base, |name| {
            calls.push(name.to_vec());
            case.resolvers
                .iter()
                .rev()
                .find(|entry| entry.0 == name)
                .map_or(0, |(_, kind, value)| match kind {
                    0 => *value as usize,
                    1 => base.wrapping_add(*value as usize),
                    2 => 0,
                    _ => unreachable!(),
                })
        })
        .unwrap();
    assert_eq!(&bytes[size..], &[0xa5; 32]);
    let divergence = matches!(fixture,Fixture::Relocation {kind:R_AARCH64_CALL26,domain:Domain::Fixed(0x1234_5678),addend:-1|1|i32::MAX} if reference.arch==Architecture::Aarch64)
        && cpp_success
        && !success;
    assert!(
        success == cpp_success || divergence,
        "{name}: Rust map={success}, C++ map={cpp_success}"
    );
    assert_eq!(
        mapped_size,
        if cpp_success { size } else { 0 },
        "{name}: mapping size"
    );
    assert_eq!(
        reader.word() as usize,
        case.sections.len(),
        "{name}: section count"
    );
    for &section in &case.sections {
        assert_eq!(reader.word(), section.id() as u64, "{name}: section id");
        let section = case.asm.get_section(section).unwrap();
        assert_eq!(reader.word(), section.addr, "{name}: section offset");
        assert_eq!(reader.word(), section.size(), "{name}: section length");
    }
    assert_eq!(
        reader.word() as usize,
        case.symbols.len(),
        "{name}: symbol count"
    );
    for &symbol in &case.symbols {
        assert_eq!(reader.word(), symbol.id() as u64, "{name}: symbol id");
        let address = reader.word() as usize;
        if divergence {
            assert!(image.symbol_address(symbol).is_none());
        } else {
            assert_eq!(
                image.symbol_address(symbol).unwrap_or(0),
                address,
                "{name}: symbol address"
            );
        }
    }
    assert_eq!(
        reader.word() as usize,
        calls.len(),
        "{name}: resolver count"
    );
    for name in calls {
        assert_eq!(reader.bytes(), name, "resolver call order");
    }
    let cpp_bytes = reader.bytes();
    if cpp_success {
        let mismatch = bytes[..size]
            .iter()
            .zip(&cpp_bytes)
            .position(|(rust, cpp)| rust != cpp);
        assert!(
            mismatch.is_none() && cpp_bytes.len() == size,
            "{name}: image bytes mismatch {mismatch:?}; Rust {size}, C++ {}",
            cpp_bytes.len()
        );
    } else {
        assert!(cpp_bytes.is_empty());
    }
    let native = reader.word() as usize;
    assert_eq!(native, if cpp_success { case.native.len() } else { 0 });
    for &(a, b) in &case.native {
        assert_eq!(
            reader.word(),
            a.wrapping_add(b),
            "{name}: native reference ADD"
        );
    }
    assert_eq!(
        [reader.word(), reader.word(), reader.word(), reader.word()],
        [0; 4],
        "{name}: reset idempotence"
    );
    assert!(reader.0.is_empty(), "{name}: trailing C++ output");
    image.reset();
    image.reset();
    assert_eq!(image.size(), 0);
    assert!(image.permissions().is_empty());
    for &symbol in &case.symbols {
        assert!(image.symbol_address(symbol).is_none());
    }
    (divergence, native)
}

/// One load/store page-offset relocation the Rust mapper accepts beyond the
/// pinned `ElfMapper.cpp`: the relocation, a load instruction with every
/// immediate bit set, and that instruction after relocating `0x1234_5ff0`.
struct LowRelocation {
    kind: u32,
    label: &'static str,
    initial: u32,
    expected: u32,
}
const LOW_RELOCATIONS: [LowRelocation; 2] = [
    LowRelocation {
        kind: R_AARCH64_LDST128_ABS_LO12_NC,
        label: "ldst128",
        initial: 0x3dff_fe67,
        expected: 0x3dc3_fe67,
    },
    LowRelocation {
        kind: R_AARCH64_LDST64_ABS_LO12_NC,
        label: "ldst64",
        initial: 0xf97f_fe67,
        expected: 0xf947_fa67,
    },
];

fn vector_relocation_case<'m>(mcx: mcx::Mcx<'m>, relocation: &LowRelocation) -> Case<'m> {
    let mut case = Case::new(mcx, Architecture::Aarch64);
    case.section(SectionKind::EHFrame, true);
    let text = case.section(SectionKind::Text, true);
    for word in [0xd503_201fu32, relocation.initial, 0xd65f_03c0, 0x8765_4321] {
        case.write(text, &word.to_le_bytes());
    }
    let function = case.symbol(1, SymBinding::Global, b"vector.function");
    case.define(function, text, 0, 12);
    let target = case.symbol(0, SymBinding::Global, b"vector.target");
    case.resolver(b"vector.target", 0, 0x1234_5fe0);
    case.relocation(text, target, relocation.kind, 4, 16);
    case.finish();
    case
}

fn vector_relocation_extension(
    work: &Path,
    references: &[Reference],
    failures: &mut FailureCollector<'_>,
) -> (usize, usize) {
    let mut refusals = 0;
    let mut witnesses = 0;
    for (reference, relocation) in references
        .iter()
        .filter(|reference| reference.arch == Architecture::Aarch64)
        .flat_map(|reference| LOW_RELOCATIONS.iter().map(move |r| (reference, r)))
    {
        let label = format!("{}-{}-extension", reference.label, relocation.label);
        let stage = format!("{}-extension", relocation.label);
        if failures
            .attempt(&label, || {
                let context = mcx::MemoryContext::new_bump("mapper-vector-extension");
                let mut case = vector_relocation_case(context.mcx(), relocation);
                let output = common::run(reference, work, &stage, &case.input);
                let mut reader = Reader(&output);
                let page = usize::try_from(reader.word()).unwrap();
                assert_eq!(
                    reader.word(),
                    0,
                    "pristine C++ must refuse relocation {}",
                    relocation.kind
                );
                let base = usize::try_from(reader.word()).unwrap();
                assert_ne!(base, 0, "reference must reach relocation processing");
                assert_eq!(reader.word(), 0, "refusal must release the mapping");
                let mut image = MapperImage::new(context.mcx()).unwrap();
                let size = image.layout(&mut case.asm, page).unwrap();
                assert_eq!(reader.word() as usize, case.sections.len());
                for &section in &case.sections {
                    assert_eq!(reader.word(), section.id() as u64);
                    let section = case.asm.get_section(section).unwrap();
                    assert_eq!(reader.word(), section.addr);
                    assert_eq!(reader.word(), section.size());
                }
                assert_eq!(reader.word() as usize, case.symbols.len());
                for &symbol in &case.symbols {
                    assert_eq!(reader.word(), symbol.id() as u64);
                    assert_eq!(reader.word(), 0, "refusal must publish no symbols");
                }
                assert_eq!(reader.word(), 1);
                assert_eq!(reader.bytes(), b"vector.target");
                assert!(reader.bytes().is_empty(), "refusal must publish no image");
                assert_eq!(reader.word(), 0, "refusal must publish no native results");
                assert_eq!(
                    [reader.word(), reader.word(), reader.word(), reader.word()],
                    [0; 4]
                );
                assert!(reader.0.is_empty(), "trailing C++ refusal output");

                let section = case.asm.sym_section(case.symbols[0]).unwrap();
                let section = case.asm.get_section(section).unwrap();
                let start = section.addr as usize;
                let mut expected = vec![0; size];
                expected[start..start + 16].copy_from_slice(&section.data);
                expected[start + 4..start + 8].copy_from_slice(&relocation.expected.to_le_bytes());
                let mut bytes = vec![0xa5; size + 32];
                let mut calls = 0;
                assert!(image
                    .relocate(&case.asm, &mut bytes[..size], base, |name| {
                        assert_eq!(name, b"vector.target");
                        calls += 1;
                        0x1234_5fe0
                    })
                    .unwrap());
                assert_eq!(calls, 1);
                assert_eq!(&bytes[..size], expected);
                assert_eq!(&bytes[size..], &[0xa5; 32]);
                assert_eq!(image.symbol_address(case.symbols[0]), Some(base + start));
                assert_eq!(image.symbol_address(case.symbols[1]), Some(0x1234_5fe0));
                image.reset();
                image.reset();
                assert_eq!(image.size(), 0);
                assert!(image.permissions().is_empty());
                for &symbol in &case.symbols {
                    assert!(image.symbol_address(symbol).is_none());
                }
            })
            .is_some()
        {
            refusals += 1;
            witnesses += 1;
        }
    }
    (refusals, witnesses)
}

fn native_vector_relocation_lifecycle() -> (usize, usize, usize) {
    let mut totals = (0, 0, 0);
    for relocation in &LOW_RELOCATIONS {
        let (maps, refusals, reuses) = native_low_relocation_lifecycle(relocation);
        totals.0 += maps;
        totals.1 += refusals;
        totals.2 += reuses;
    }
    totals
}

fn native_low_relocation_lifecycle(relocation: &LowRelocation) -> (usize, usize, usize) {
    #[repr(align(16))]
    struct VectorStorage([u8; 32]);
    let storage = VectorStorage([0; 32]);
    let target = storage.0.as_ptr() as usize;
    let context = mcx::MemoryContext::new_bump("mapper-native-vector-extension");
    let mut case = vector_relocation_case(context.mcx(), relocation);
    let mut mapper = ElfMapper::new(context.mcx()).unwrap();
    // SAFETY: EH metadata is empty and terminated; aligned storage outlives the non-invoked mapping.
    assert!(unsafe { mapper.map(&mut case.asm, |_| target) }.unwrap());
    assert!(mapper.get_mapped_range().is_some());
    assert!(mapper.get_sym_addr(case.symbols[0]).is_some());
    // SAFETY: only the symbol alignment changes; relocation refusal must release the prior mapping.
    assert!(!unsafe { mapper.map(&mut case.asm, |_| target + 1) }.unwrap());
    assert!(mapper.get_mapped_range().is_none());
    for &symbol in &case.symbols {
        assert!(mapper.get_sym_addr(symbol).is_none());
    }
    // SAFETY: the original aligned symbol address restores the same non-invoked fixture.
    assert!(unsafe { mapper.map(&mut case.asm, |_| target) }.unwrap());
    assert!(mapper.get_mapped_range().is_some());
    assert!(mapper.get_sym_addr(case.symbols[0]).is_some());
    mapper.reset();
    mapper.reset();
    assert!(mapper.get_mapped_range().is_none());
    for &symbol in &case.symbols {
        assert!(mapper.get_sym_addr(symbol).is_none());
    }
    (2, 1, 1)
}

struct FailureCollector<'a> {
    work: &'a Path,
    labels: Vec<String>,
    output: BufWriter<fs::File>,
}
impl<'a> FailureCollector<'a> {
    fn new(work: &'a Path) -> Self {
        let mut output = BufWriter::new(fs::File::create(work.join("failures.tsv")).unwrap());
        writeln!(output, "check\tdiagnostic").unwrap();
        output.flush().unwrap();
        Self {
            work,
            labels: Vec::new(),
            output,
        }
    }
    fn attempt<T>(&mut self, label: &str, action: impl FnOnce() -> T) -> Option<T> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(action)) {
            Ok(value) => Some(value),
            Err(payload) => {
                let diagnostic = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("non-string panic");
                let filename = format!("{label}-failure.txt");
                fs::write(self.work.join(&filename), diagnostic).unwrap();
                writeln!(self.output, "{label}\t{filename}").unwrap();
                self.output.flush().unwrap();
                self.labels.push(label.to_owned());
                None
            }
        }
    }
    fn finish(&self) {
        assert!(
            self.labels.is_empty(),
            "{} independent checks failed; see {}/failures.tsv",
            self.labels.len(),
            self.work.display()
        );
    }
}

#[test]
fn complete_mapper_images_match_compiled_upstream_and_native_add_executes() {
    if std::env::var_os("TPDE_MAPPER_UNWIND_CHILD").is_some() {
        unwind_child();
        return;
    }
    assert!(
        cfg!(all(target_arch = "aarch64", target_os = "linux")),
        "native qualification requires an AArch64 Linux runner"
    );
    println!();
    let root = std::env::var_os("COMPONENT_ARTIFACT_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let work = root.join(format!("elf-mapper-oracle-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let references = common::references();
    assert_eq!(references.len(), 8);
    let mut failures = FailureCollector::new(&work);
    let mut native_cpp = unwind_probes(&work, &references, &mut failures);
    let (vector_cpp_refusals, vector_rust_witnesses) =
        vector_relocation_extension(&work, &references, &mut failures);
    let vector_native = failures
        .attempt(
            "native-vector-relocation-lifecycle",
            native_vector_relocation_lifecycle,
        )
        .unwrap_or_default();
    let mut roster = BufWriter::new(fs::File::create(work.join("executed-cases.tsv")).unwrap());
    writeln!(roster, "case\tarchitecture\tfixture\tcpp_mode\tverdict").unwrap();
    let mut cases = 0;
    let mut comparisons = 0;
    let mut completed = 0;
    let mut corrections = 0;
    for arch in [Architecture::Aarch64, Architecture::X86_64] {
        for (index, fixture) in fixtures(arch).into_iter().enumerate() {
            let name = format!("case-{index:04}");
            for reference in references.iter().filter(|r| r.arch == arch) {
                comparisons += 1;
                let label = format!("{}-{name}", reference.label);
                let outcome = failures.attempt(&label, || {
                    let context = mcx::MemoryContext::new_bump("mapper-differential");
                    let mut case = build(context.mcx(), arch, fixture);
                    compare(&mut case, fixture, reference, &work, &name)
                });
                let verdict = match outcome {
                    Some((corrected, native)) => {
                        corrections += usize::from(corrected);
                        native_cpp += native;
                        completed += 1;
                        if corrected {
                            "SAFE_REFUSAL_UPSTREAM_TRUNCATES"
                        } else {
                            "MATCH"
                        }
                    }
                    None => "FAIL",
                };
                writeln!(
                    roster,
                    "{index}\t{arch:?}\t{fixture:?}\t{}\t{verdict}",
                    reference.label
                )
                .unwrap();
                roster.flush().unwrap();
            }
            cases += 1;
        }
    }
    let native_rust = failures
        .attempt("native-lifecycle", native_lifecycle)
        .unwrap_or(0);
    let permissions = failures
        .attempt("native-permissions-budget", permissions_and_budget)
        .unwrap_or(0);
    let publication = failures
        .attempt("native-publication-rounds", native_publication_rounds)
        .unwrap_or_default();
    failures.attempt("complete-execution-floors", || {
        assert_eq!(native_rust, 384);
        assert_eq!(permissions, 4);
        assert_eq!(native_cpp, 768);
        assert_eq!(cases, 324);
        assert_eq!(comparisons, 1296);
        assert_eq!(completed, 1296);
        assert_eq!(corrections, 12);
        assert_eq!((vector_cpp_refusals, vector_rust_witnesses), (8, 8));
        assert_eq!(vector_native, (4, 2, 2));
        assert_eq!((publication.0, publication.1), (192, 576));
        assert!(publication.2 >= 1);
    });
    fs::write(work.join("execution-summary.tsv"), format!("cases\t{cases}\nattempted_comparisons\t{comparisons}\ncompleted_comparisons\t{completed}\ncorrections\t{corrections}\nrust_native_calls\t{native_rust}\ncpp_native_calls\t{native_cpp}\nfailed_checks\t{}\n", failures.labels.len())).unwrap();
    println!("TPDE_ELF_MAPPER_EXECUTION cases={cases} attempted={comparisons} completed={completed} failed_checks={}", failures.labels.len());
    failures.finish();
    println!("TPDE_ELF_MAPPER_VECTOR_EXTENSION PASS cpp_refusals={vector_cpp_refusals} rust_witnesses={vector_rust_witnesses} native_maps={} native_refusals={} native_reuses={}", vector_native.0, vector_native.1, vector_native.2);
    println!("TPDE_ELF_MAPPER_ORACLE PASS cases={cases} comparisons={comparisons} cpp_modes=8 corrections={corrections}");
    println!("TPDE_ELF_MAPPER_NATIVE PASS rust_calls={native_rust} cpp_calls={native_cpp} operation=disarm-ADDx-RET integration=encoder-assembler-map-invoke");
    println!("TPDE_ELF_MAPPER_SAFE_REFUSAL PASS cases={corrections} reason=CALL26-truncated-upstream-fallback");
    println!(
        "TPDE_ELF_MAPPER_PUBLICATION PASS rounds=48 pages=4 functions={} calls={} explicit_maintenance={}",
        publication.0,
        publication.1,
        u8::from(!cfg!(target_os = "linux"))
    );
    println!("TPDE_ELF_MAPPER_PUBLICATION_REUSE base_reuses={}", publication.2);
    println!("TPDE_ELF_MAPPER_LIFECYCLE PASS successful_maps=3 relocation_refusals=1 reuse_after_failure=1 implicit_eh=1 repeated_reset=1");
    println!(
        "TPDE_ELF_MAPPER_SANITIZER_CONFIG PASS active={}",
        u8::from(cfg!(tpde_address_sanitizer))
    );
}

fn native_lifecycle() -> usize {
    let context = mcx::MemoryContext::new_bump("mapper-native-installation");
    let mut native = build(context.mcx(), Architecture::Aarch64, Fixture::Native);
    let mut mapper = ElfMapper::new(context.mcx()).unwrap();
    // SAFETY: the fixture contains an empty terminated EH section, no imports, and a verified ADD/RET ABI body.
    assert!(unsafe { mapper.map(&mut native.asm, |_| 0) }.unwrap());
    let symbol = mapper.get_sym_addr(native.native_symbol.unwrap()).unwrap();
    for &(a, b) in &native.native {
        // SAFETY: the freshly encoded function consumes two u64 ABI arguments and returns their wrapping sum.
        assert_eq!(unsafe { symbol.call2_u64(a, b) }, a.wrapping_add(b));
    }
    let mut native_rust = native.native.len();
    let function = native.native_symbol.unwrap();
    let text = native.asm.sym_section(function).unwrap();
    native
        .asm
        .reloc_sec(text, function, u32::MAX, 0, 0)
        .unwrap();
    // SAFETY: the valid fixture is changed only by an unsupported relocation; rejection must release the prior mapping.
    assert!(!unsafe { mapper.map(&mut native.asm, |_| 0) }.unwrap());
    assert!(mapper.get_mapped_range().is_none());
    assert!(mapper.get_sym_addr(function).is_none());
    let mut reused = build(context.mcx(), Architecture::Aarch64, Fixture::Native);
    // SAFETY: this independent fixture has the same verified ADD/RET ABI and empty EH metadata.
    assert!(unsafe { mapper.map(&mut reused.asm, |_| 0) }.unwrap());
    let symbol = mapper.get_sym_addr(reused.native_symbol.unwrap()).unwrap();
    for &(a, b) in &reused.native {
        // SAFETY: the mapped body is the newly encoded two-argument ADD/RET fixture.
        assert_eq!(unsafe { symbol.call2_u64(a, b) }, a.wrapping_add(b));
    }
    native_rust += reused.native.len();
    mapper.reset();
    mapper.reset();
    assert!(mapper.get_mapped_range().is_none());
    let mut implicit_eh = Case::new(context.mcx(), Architecture::Aarch64);
    let text = implicit_eh.section(SectionKind::Text, true);
    let function = implicit_eh.symbol(1, SymBinding::Global, b"implicit-eh.add");
    implicit_eh.native_add(text, function, &arguments());
    // SAFETY: the mapper creates an empty EH section; the only code is the verified ADD/RET fixture.
    assert!(unsafe { mapper.map(&mut implicit_eh.asm, |_| 0) }.unwrap());
    let symbol = mapper.get_sym_addr(function).unwrap();
    for &(a, b) in &implicit_eh.native {
        // SAFETY: the mapped entry has the declared two-u64 ABI and no imported references.
        assert_eq!(unsafe { symbol.call2_u64(a, b) }, a.wrapping_add(b));
    }
    native_rust += implicit_eh.native.len();
    mapper.reset();
    assert!(mapper.get_mapped_range().is_none());
    assert_eq!(native_rust, 384);
    native_rust
}

/// Fresh code on several pages, written into the RW mapping and published
/// by `mprotect` alone (no explicit maintenance on Linux), executed, then
/// replaced at the same addresses by different code: a stale instruction
/// line would return the previous round's constant.
fn native_publication_rounds() -> (usize, usize, usize) {
    const ROUNDS: usize = 48;
    const PAGES: usize = 4;
    const INPUTS: [(u64, u64); 3] = [(1, 2), (u64::MAX, 7), (0x5555_aaaa, 0x1234)];
    let context = mcx::MemoryContext::new_bump("mapper-native-publication");
    // SAFETY: this sysconf key has no pointer or ownership preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    assert!(page.is_power_of_two());
    let reg = |index| tpde_disarm::GReg::new(index).unwrap();
    let (r0, r1, r2, lr) = (reg(0), reg(1), reg(2), reg(30));
    let mut mapper = ElfMapper::new(context.mcx()).unwrap();
    let mut calls = 0;
    let mut functions = 0;
    let mut reuses = 0;
    let mut previous = None;
    for round in 0..ROUNDS {
        let mut case = Case::new(context.mcx(), Architecture::Aarch64);
        case.section(SectionKind::EHFrame, true);
        let text = case.section(SectionKind::Text, true);
        let mut symbols = Vec::new();
        for index in 0..PAGES {
            let name = format!("publication.{round}.{index}");
            let symbol = case.symbol(1, SymBinding::Global, name.as_bytes());
            let section = case.asm.get_section_mut(text).unwrap();
            let target = index * page;
            let padding = vec![0u8; target - section.size() as usize];
            section.write(&padding[..]).unwrap();
            let constant = ((round * PAGES + index) & 0xffff) as u64;
            let words = [
                tpde_disarm::de64_MOVZx(r2.into(), constant),
                tpde_disarm::de64_ADDx(r0.into(), r0.into(), r1.into()),
                tpde_disarm::de64_ADDx(r0.into(), r0.into(), r2.into()),
                tpde_disarm::de64_RET(lr.into()),
            ];
            for word in words {
                assert_ne!(word, 0);
                section.write(&word).unwrap();
            }
            case.asm.sym_def(symbol, text, target as u64, 16).unwrap();
            symbols.push((symbol, constant));
            functions += 1;
        }
        // SAFETY: the EH section is empty and terminated; every body is a verified MOVZ/ADD/ADD/RET sequence with no imports.
        assert!(unsafe { mapper.map(&mut case.asm, |_| 0) }.unwrap());
        let base = mapper.get_mapped_range().unwrap().address();
        reuses += usize::from(previous == Some(base));
        previous = Some(base);
        for (symbol, constant) in symbols {
            let function = mapper.get_sym_addr(symbol).unwrap();
            for (a, b) in INPUTS {
                // SAFETY: the mapped body has the two-u64 ABI and only computes a + b + constant.
                let value = unsafe { function.call2_u64(a, b) };
                assert_eq!(
                    value,
                    a.wrapping_add(b).wrapping_add(constant),
                    "round {round} constant {constant:#x} at {base:#x}"
                );
                calls += 1;
            }
        }
    }
    mapper.reset();
    assert!(reuses >= 1, "no mapping address was reused across rounds");
    (functions, calls, reuses)
}

fn permissions_and_budget() -> usize {
    let source = mcx::MemoryContext::new_bump("mapper-os-source");
    let mut case = build(source.mcx(), Architecture::Aarch64, Fixture::Native);
    let memory = mcx::MemoryContext::new("mapper-os-accounting");
    let mut mapper = ElfMapper::new(memory.mcx()).unwrap();
    let retained = memory.used();
    // SAFETY: this sysconf key has no pointer or ownership preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    assert!(page > 0);
    let page = page as usize;
    assert!(page.is_power_of_two());
    // SAFETY: all code is a verified ADD/RET body and EH metadata is empty.
    assert!(unsafe { mapper.map(&mut case.asm, |_| 0) }.unwrap());
    let range = mapper.get_mapped_range().unwrap();
    let size = range.len();
    let base = range.address();
    assert_eq!(memory.used(), retained + (size + page - 1) / page * page);
    let maps = fs::read_to_string("/proc/self/maps").unwrap();
    let mut checks = 0;
    for (kind, expected) in [
        (SectionKind::Text, "r-x"),
        (SectionKind::EHFrame, "r--"),
        (SectionKind::Data, "rw-"),
        (SectionKind::BSS, "rw-"),
    ] {
        let section = case.asm.get_default_section(kind).unwrap();
        let section = case.asm.get_section(section).unwrap();
        let address = base + section.addr as usize;
        let permissions = maps
            .lines()
            .find_map(|line| {
                let mut fields = line.split_whitespace();
                let (start, end) = fields.next()?.split_once('-')?;
                let start = usize::from_str_radix(start, 16).unwrap();
                let end = usize::from_str_radix(end, 16).unwrap();
                let permissions = fields.next()?;
                (start <= address && address < end).then_some(permissions)
            })
            .expect("mapped section absent from proc maps");
        assert_eq!(&permissions[..3], expected, "native section {kind:?}");
        checks += 1;
    }
    mapper.reset();
    assert_unmapped(base, size, page);
    assert_eq!(memory.used(), retained);
    let limited = mcx::MemoryContext::new("mapper-os-limit").with_limit(page * 16);
    let mut bounded = ElfMapper::new(limited.mcx()).unwrap();
    let bounded_retained = limited.used();
    let bss = case.asm.get_default_section(SectionKind::BSS).unwrap();
    let original = case.asm.get_section(bss).unwrap().vsize;
    case.asm.get_section_mut(bss).unwrap().vsize = (page * 256) as u64;
    // SAFETY: only zero-initialized BSS length changes; admission must reject its external page charge.
    let error = unsafe { bounded.map(&mut case.asm, |_| 0) }.unwrap_err();
    assert_eq!(error.sqlstate, types_error::ERRCODE_OUT_OF_MEMORY);
    assert!(bounded.get_mapped_range().is_none());
    assert_eq!(limited.used(), bounded_retained);
    case.asm.get_section_mut(bss).unwrap().vsize = original;
    // SAFETY: restoring the small valid image tests reuse after the budget refusal.
    assert!(unsafe { bounded.map(&mut case.asm, |_| 0) }.unwrap());
    let size = bounded.get_mapped_range().unwrap().len();
    let base = bounded.get_mapped_range().unwrap().address();
    assert_eq!(
        limited.used(),
        bounded_retained + (size + page - 1) / page * page
    );
    drop(bounded);
    assert_unmapped(base, size, page);
    assert_eq!(limited.used(), bounded_retained);
    assert_eq!(checks, 4);
    println!("TPDE_ELF_MAPPER_OS PASS permissions=4 page_charge=2 releases=2 unmaps=2 budget_refusals=1 budget_reuse=1");
    checks
}

fn assert_unmapped(base: usize, size: usize, page: usize) {
    for offset in (0..size).step_by(page) {
        let mut residency = 0u8;
        // SAFETY: mincore validates the old aligned address without dereferencing it; its one-page result has valid storage.
        let result =
            unsafe { libc::mincore((base + offset) as *mut libc::c_void, page, &mut residency) };
        let error = std::io::Error::last_os_error().raw_os_error();
        assert_eq!(result, -1, "released mapping page remains present");
        assert_eq!(error, Some(libc::ENOMEM));
    }
}

fn unwind_child() {
    let mode = std::env::var("TPDE_MAPPER_UNWIND_CHILD").unwrap();
    assert!(matches!(
        mode.as_str(),
        "gcc" | "llvm" | "llvm-missing-remove"
    ));
    let context = mcx::MemoryContext::new_bump("unwind-hook-probe");
    let mut case = build(context.mcx(), Architecture::Aarch64, Fixture::Native);
    let mut mapper = ElfMapper::new(context.mcx()).unwrap();
    let retained = context.used();
    std::env::set_var("TPDE_MAPPER_PROBE_ARMED", "1");
    if mode == "llvm-missing-remove" {
        // SAFETY: valid empty EH metadata reaches the controlled missing-unregister admission check.
        assert!(!unsafe { mapper.map(&mut case.asm, |_| 0) }.unwrap());
        assert!(mapper.get_mapped_range().is_none());
        assert_eq!(context.used(), retained);
        std::env::remove_var("TPDE_MAPPER_PROBE_ARMED");
        println!("TPDE_ELF_MAPPER_UNWIND_MISSING_REMOVE PASS refusal=1 registrations=0 budget_released=1");
        return;
    }
    // SAFETY: this probe only registers empty EH metadata and the verified ADD/RET body.
    assert!(unsafe { mapper.map(&mut case.asm, |_| 0) }.unwrap());
    mapper.reset();
    // SAFETY: the same empty-EH image remains valid after reset.
    assert!(unsafe { mapper.map(&mut case.asm, |_| 0) }.unwrap());
    drop(mapper);
    std::env::remove_var("TPDE_MAPPER_PROBE_ARMED");
    println!("TPDE_ELF_MAPPER_UNWIND_CHILD PASS mappings=2 reset=1 drop=1");
}

fn hook_log(path: &Path, mode: &str, pairs: usize) {
    let log = fs::read_to_string(path).unwrap();
    let lines: Vec<_> = log.lines().collect();
    assert_eq!(lines.len(), pairs * 2, "{mode}: {log}");
    for events in lines.chunks_exact(2) {
        let add: Vec<_> = events[0].split('\t').collect();
        let remove: Vec<_> = events[1].split('\t').collect();
        assert_eq!(add.len(), 3);
        assert_eq!(remove.len(), 3);
        assert_eq!(add[0], format!("{mode}-register"));
        assert_eq!(remove[0], format!("{mode}-deregister"));
        assert_eq!(add[1], remove[1]);
        assert_eq!(add[2], "0");
        assert_eq!(remove[2], "0");
    }
}

fn unwind_probes(
    work: &Path,
    references: &[Reference],
    failures: &mut FailureCollector<'_>,
) -> usize {
    let directory = common::reference_directory();
    let reference = references
        .iter()
        .find(|r| r.label == "a64-assert-native")
        .unwrap();
    let mut native_calls = 0;
    let mut rust_completed = 0;
    let mut cpp_completed = 0;
    for (mode, missing_remove) in [
        ("llvm", false),
        ("gcc", false),
        ("llvm-missing-remove", true),
    ] {
        let library = directory.join(format!("unwind-{mode}.so"));
        if failures
            .attempt(&format!("unwind-{mode}-rust-check"), || {
                run_rust_unwind(work, mode, missing_remove, &library)
            })
            .is_some()
        {
            rust_completed += 1;
        }
        if !missing_remove {
            if let Some(calls) = failures.attempt(&format!("unwind-{mode}-cpp-check"), || {
                run_cpp_unwind(work, mode, &library, reference)
            }) {
                cpp_completed += 1;
                native_calls += calls;
            }
        }
    }
    if rust_completed == 3 && cpp_completed == 2 {
        assert_eq!(native_calls, 256);
        println!("TPDE_ELF_MAPPER_UNWIND PASS llvm_pairs=3 gcc_pairs=3 readable_during_deregister=6 missing_remove_refusals=1");
    }
    native_calls
}

fn run_rust_unwind(work: &Path, mode: &str, missing_remove: bool, library: &Path) {
    use std::process::Command;
    let rust_log = work.join(format!("unwind-{mode}-rust.tsv"));
    fs::write(&rust_log, []).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "complete_mapper_images_match_compiled_upstream_and_native_add_executes",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("LD_PRELOAD", &library)
        .env("TPDE_MAPPER_UNWIND_CHILD", mode)
        .env("TPDE_MAPPER_PROBE_LOG", &rust_log)
        .env_remove("TPDE_MAPPER_PROBE_ARMED");
    let result = common::checked(&mut command, work, &format!("unwind-{mode}-rust"));
    if missing_remove {
        assert!(String::from_utf8_lossy(&result.stdout).contains("TPDE_ELF_MAPPER_UNWIND_MISSING_REMOVE PASS refusal=1 registrations=0 budget_released=1"));
        assert!(fs::read(&rust_log).unwrap().is_empty());
        return;
    }
    assert!(String::from_utf8_lossy(&result.stdout)
        .contains("TPDE_ELF_MAPPER_UNWIND_CHILD PASS mappings=2 reset=1 drop=1"));
    hook_log(&rust_log, mode, 2);
}

fn run_cpp_unwind(work: &Path, mode: &str, library: &Path, reference: &Reference) -> usize {
    use std::process::{Command, Stdio};
    let mut native_calls = 0;
    let source = mcx::MemoryContext::new_bump("cpp-unwind-source");
    let case = build(source.mcx(), Architecture::Aarch64, Fixture::Native);
    let input = work.join(format!("unwind-{mode}-cpp-input.bin"));
    fs::write(&input, &case.input).unwrap();
    let cpp_log = work.join(format!("unwind-{mode}-cpp.tsv"));
    let mut command = Command::new(&reference.executable);
    command
        .stdin(Stdio::from(fs::File::open(input).unwrap()))
        .env("LD_PRELOAD", &library)
        .env("TPDE_MAPPER_PROBE", mode)
        .env("TPDE_MAPPER_PROBE_LOG", &cpp_log)
        .env_remove("TPDE_MAPPER_PROBE_ARMED");
    let result = common::checked(&mut command, work, &format!("unwind-{mode}-cpp"));
    let mut reader = Reader(&result.stdout);
    assert!(reader.word().is_power_of_two());
    assert_eq!(reader.word(), 1);
    assert_ne!(reader.word(), 0);
    let size = reader.word();
    for _ in 0..reader.word() {
        reader.word();
        reader.word();
        reader.word();
    }
    for _ in 0..reader.word() {
        reader.word();
        reader.word();
    }
    assert_eq!(reader.word(), 0);
    assert_eq!(reader.bytes().len() as u64, size);
    assert_eq!(reader.word(), case.native.len() as u64);
    for &(a, b) in &case.native {
        assert_eq!(reader.word(), a.wrapping_add(b));
        native_calls += 1;
    }
    assert_eq!(
        [reader.word(), reader.word(), reader.word(), reader.word()],
        [0; 4]
    );
    assert!(reader.0.is_empty());
    hook_log(&cpp_log, mode, 1);
    native_calls
}
