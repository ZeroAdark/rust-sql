// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#[path = "../../test-support/oracle.rs"]
mod oracle;
mod support;
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use support::Script;
use tpde_assembler::{elf::*, *};

fn compile(root: &Path, output: &Path, sources: &[PathBuf], debug: bool) {
    oracle::compile_cpp(
        &root.join("tests/upstream/tpde/include"),
        output,
        sources,
        debug,
    );
}
fn rich(s: &mut Script<'_>, seed: usize) {
    s.snapshot();
    let external = s.symbol(0, SymBinding::Global, b"external");
    let func = s.symbol(1, SymBinding::Local, b"local.function");
    let weak = s.symbol(2, SymBinding::Weak, b"weak\0name");
    let local_undefined = s.symbol(0, SymBinding::Local, b"");
    let sections: Vec<_> = SectionKind::ALL
        .into_iter()
        .map(|k| {
            let sec = s.section(k, true);
            assert_eq!(s.section(k, true), sec);
            sec
        })
        .collect();
    let text = sections[0];
    let ro = sections[1];
    let data = sections[4];
    s.write(text, &[0xcc; 64]);
    s.define(func, text, 0, 64);
    s.define(local_undefined, text, 64, 0);
    let lengths = [0, 1, 22, 255, 256, 257, 511, 1024];
    let bytes: Vec<_> = (0..lengths[seed]).map(|i| (i * 131 + seed) as u8).collect();
    s.write(ro, &bytes);
    s.write_u64(data, 0x0123456789abcdef);
    let datum = s.data(data, b"value", SymBinding::Global, 16, &bytes);
    s.def_bytes(data, weak, 8, b"\x00\xffpayload");
    let allocated = s.symbol(2, SymBinding::Local, b"allocated");
    s.allocate_fill(data, allocated, 17, 32, 0xa5);
    let zero = s.symbol(2, SymBinding::Global, b"zero.data");
    s.zero(data, zero, 13, 16);
    for kind in [
        SectionKind::BSS,
        SectionKind::ThreadData,
        SectionKind::ThreadBSS,
    ] {
        let sec = sections[kind as usize];
        let sym = s.symbol(
            if kind == SectionKind::BSS { 2 } else { 3 },
            SymBinding::Global,
            b"storage",
        );
        if kind == SectionKind::ThreadData {
            s.def_bytes(sec, sym, 16, b"\x80\0tls");
        } else {
            s.zero(sec, sym, 37, 32);
        }
        s.query_symbol(sym, true);
    }
    for (i, kind) in SectionKind::ALL.into_iter().enumerate() {
        let flags = s.asm.target().section_flags[kind as usize];
        if i == 2 || i == 3 || i == 5 {
            s.write(sections[i], &[seed as u8; 9]);
        }
        s.query_section(sections[i], flags.has_relocs, true);
    }
    for visibility in [
        SymVisibility::Default,
        SymVisibility::Internal,
        SymVisibility::Hidden,
        SymVisibility::Protected,
    ] {
        s.visibility(weak, visibility);
    }
    s.copy(weak, func);
    s.copy(weak, weak);
    s.query_symbol(weak, true);
    s.query_symbol(external, false);
    for i in 0..12u32 {
        s.target_reloc(
            i % 2 == 0,
            text,
            if i % 3 == 0 { func } else { external },
            i * 4,
            if i % 2 == 0 { -4 } else { 8 },
        );
    }
    s.relocation(text, datum, 0x1234, 48, i32::MIN as i64);
    s.relocation(text, weak, 0x5678, 56, i32::MAX as i64);
    s.adjust(text, 10, 4);
    s.adjust(text, 100, 99);
    let secsym = s.section_symbol(text);
    assert_eq!(s.section_symbol(text), secsym);
    s.query_symbol(secsym, true);
    s.query_section(text, true, true);
    s.snapshot();
    let group = s.group(external, seed % 2 == 0);
    s.add_group(group, ro);
    let extra = s.section(SectionKind::Text, false);
    s.rename(extra, format!(".text.fixture.{seed}").as_bytes());
    s.write(extra, &[1; 17]);
    s.add_group(group, extra);
    let init = s.structor(true, group);
    s.write_u64(init, 0);
    s.target_reloc(true, init, external, 0, 0);
    let fini = s.structor(false, SecRef::default());
    s.write_u64(fini, 0);
    s.target_reloc(true, fini, func, 0, 0);
    let raw = s.raw_section(s.asm.target().section_flags[SectionKind::ReadOnly as usize]);
    s.rename(raw, b".raw\xff\0suffix");
    s.write(raw, b"binary\0\xff");
    let _ = s.section_symbol(raw);
    s.snapshot();
    s.reset();
    s.snapshot();
    let text = s.section(SectionKind::Text, true);
    s.write(text, &[seed as u8; 16]);
    let sym = s.symbol(1, SymBinding::Global, b"after.reset");
    s.define(sym, text, 0, 16);
    s.snapshot();
}
fn large(s: &mut Script<'_>) {
    let low = s.bulk(SHN_LORESERVE as u32 - 2);
    let local = s.symbol(2, SymBinding::Local, b"low");
    s.define(local, low, 0, 0);
    s.snapshot();
    let boundary = s.section(SectionKind::ReadOnly, false);
    assert_eq!(boundary.id(), SHN_LORESERVE as u32 - 1);
    let global = s.symbol(2, SymBinding::Global, b"boundary");
    s.define(global, boundary, 0, 0);
    s.snapshot();
    let high = s.section(SectionKind::ReadOnly, false);
    assert_eq!(high.id(), SHN_LORESERVE as u32);
    let hs = s.section_symbol(high);
    s.query_symbol(hs, true);
    let highglobal = s.data(high, b"high", SymBinding::Global, 16, &[0xff; 17]);
    s.query_symbol(highglobal, true);
    let copied = s.symbol(2, SymBinding::Local, b"copy.to.local");
    s.copy(copied, highglobal);
    s.query_symbol(copied, true);
    let copiedglobal = s.symbol(1, SymBinding::Weak, b"copy.to.global");
    s.copy(copiedglobal, hs);
    s.query_symbol(copiedglobal, true);
    let group = s.group(global, true);
    let init = s.structor(true, group);
    s.write_u64(init, 0);
    s.target_reloc(true, init, highglobal, 0, -8);
    s.snapshot();
}
fn compare(script: &Script<'_>, work: &Path, oracle: &Path, name: &str) {
    crate::oracle::compare_bytes(oracle, work, &script.input, &script.expected, name);
}
fn elf_contract(root: &Path, work: &Path) {
    let header = fs::read_to_string(root.join("tests/upstream/tpde/include/tpde/ELF.hpp")).unwrap();
    let constants: BTreeMap<_, _> = [IDENT, HALF, BYTE, WORD, X64, A64]
        .into_iter()
        .flatten()
        .copied()
        .collect();
    let mut source = String::from(
        "#include <cstddef>\n#include <iostream>\n#include \"tpde/ELF.hpp\"\nusing namespace tpde; using namespace tpde::elf; int main() {\n",
    );
    let mut expected = String::new();
    let mut in_enum = false;
    let mut found = std::collections::BTreeSet::new();
    for line in header.lines().map(str::trim) {
        if line.starts_with("enum ") {
            in_enum = true;
            continue;
        }
        if line == "};" {
            in_enum = false;
        }
        if !in_enum {
            continue;
        }
        if let Some((name, _)) = line.split_once('=') {
            let name = name.trim();
            let value = constants
                .get(name)
                .unwrap_or_else(|| panic!("missing Rust constant {name}"));
            found.insert(name.to_owned());
            writeln!(
                source,
                "std::cout << \"{name} \" << static_cast<unsigned long long>({name}) << '\\n';"
            )
            .unwrap();
            writeln!(expected, "{name} {value}").unwrap();
        }
    }
    assert_eq!(found.len(), constants.len());
    let mut layouts = BTreeMap::<String, usize>::new();
    macro_rules! layout {($t:ty; $($f:ident),*) => {{
        let name=stringify!($t);
        layouts.insert(format!("size:{name}"),std::mem::size_of::<$t>());
        layouts.insert(format!("align:{name}"),std::mem::align_of::<$t>());
        $(layouts.insert(format!("off:{name}.{}",stringify!($f)),std::mem::offset_of!($t,$f));)*
    }};}
    layout!(Elf64_Ehdr; e_ident,e_type,e_machine,e_version,e_entry,e_phoff,e_shoff,e_flags,e_ehsize,e_phentsize,e_phnum,e_shentsize,e_shnum,e_shstrndx);
    layout!(Elf64_Shdr; sh_name,sh_type,sh_flags,sh_addr,sh_offset,sh_size,sh_link,sh_info,sh_addralign,sh_entsize);
    layout!(Elf64_Sym; st_name,st_info,st_other,st_shndx,st_value,st_size);
    layout!(Elf64_Rela; r_offset,r_info,r_addend);
    let mut struct_name = String::new();
    let mut metrics = 0;
    for line in header.lines().map(str::trim) {
        if let Some(name) = line
            .strip_prefix("struct ")
            .and_then(|x| x.strip_suffix(" {"))
        {
            struct_name = name.to_owned();
            for (prefix, expr) in [
                ("size", format!("sizeof({name})")),
                ("align", format!("alignof({name})")),
            ] {
                let key = format!("{prefix}:{name}");
                let value = layouts[&key];
                metrics += 1;
                writeln!(source, "std::cout << \"{key} \" << {expr} << '\\n';").unwrap();
                writeln!(expected, "{key} {value}").unwrap();
            }
        } else if line == "};" {
            struct_name.clear();
        } else if !struct_name.is_empty() && line.ends_with(';') && !line.contains('(') {
            if let Some(field) = line.split_whitespace().nth(1) {
                let field = field.trim_end_matches(';').split('[').next().unwrap();
                let key = format!("off:{struct_name}.{field}");
                let value = layouts[&key];
                metrics += 1;
                writeln!(
                    source,
                    "std::cout << \"{key} \" << offsetof({struct_name},{field}) << '\\n';"
                )
                .unwrap();
                writeln!(expected, "{key} {value}").unwrap();
            }
        }
    }
    assert_eq!(metrics, layouts.len());
    source.push_str("for(unsigned b=0;b<256;++b) for(unsigned t=0;t<256;++t) std::cout << ELF64_ST_INFO(b,t) << '\\n';\nfor(unsigned i=0;i<256;++i) {Elf64_Sym s{};s.st_info=i;std::cout << unsigned(s.st_bind()) << ' ' << unsigned(s.st_type()) << '\\n';}\n");
    for b in 0..256 {
        for t in 0..256 {
            writeln!(expected, "{}", st_info(b, t)).unwrap();
        }
    }
    for i in 0..=255 {
        let s = Elf64_Sym {
            st_info: i,
            ..Elf64_Sym::default()
        };
        writeln!(expected, "{} {}", s.st_bind(), s.st_type()).unwrap();
    }
    for sym in [0u64, 1, u32::MAX as u64, u64::MAX] {
        for kind in [0u32, 1, u32::MAX] {
            writeln!(
                source,
                "std::cout << ELF64_R_INFO({sym}ULL,{kind}U) << '\\n';"
            )
            .unwrap();
            writeln!(expected, "{}", r_info(sym, kind)).unwrap();
        }
    }
    source.push_str("for(unsigned i=0;i<sizeof(ELFMAG);++i) std::cout << unsigned((unsigned char)ELFMAG[i]) << '\\n';\n}\n");
    for b in ELFMAG {
        writeln!(expected, "{b}").unwrap();
    }
    let path = work.join("elf-contract.cpp");
    fs::write(&path, source).unwrap();
    let oracle = work.join("elf-contract");
    compile(root, &oracle, &[path], false);
    let result = Command::new(oracle).output().unwrap();
    assert!(result.status.success());
    assert_eq!(String::from_utf8(result.stdout).unwrap(), expected);
    eprintln!(
        "TPDE_ELF_CONTRACT PASS constants={} layout_metrics={} symbol_pairs=65536",
        constants.len(),
        metrics
    );
}
#[test]
fn complete_objects_and_elf_contract_match_upstream() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let work = std::env::temp_dir().join(format!("tpde-assembler-oracle-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let sources = [
        "tests/oracle.cpp",
        "tests/upstream/tpde/src/Assembler.cpp",
        "tests/upstream/tpde/src/AssemblerElf.cpp",
        "tests/upstream/tpde/src/StringTable.cpp",
        "tests/upstream/tpde/src/util/SmallVector.cpp",
    ]
    .map(|p| root.join(p));
    let release = work.join("oracle-release");
    let debug = work.join("oracle-debug");
    compile(root, &release, &sources, false);
    compile(root, &debug, &sources, true);
    elf_contract(root, &work);
    let mut snapshots = 0;
    let mut cases = 0;
    for arch in [Architecture::Aarch64, Architecture::X86_64] {
        for seed in 0..8 {
            let ctx = mcx::MemoryContext::new_bump("object-oracle");
            let mut script = Script::new(ctx.mcx(), arch);
            rich(&mut script, seed);
            for oracle in [&release, &debug] {
                compare(&script, &work, oracle, &format!("{arch:?} seed {seed}"));
            }
            snapshots += script.snapshots;
            cases += 1;
        }
        let ctx = mcx::MemoryContext::new_bump("large-object-oracle");
        let mut script = Script::new(ctx.mcx(), arch);
        large(&mut script);
        for oracle in [&release, &debug] {
            compare(
                &script,
                &work,
                oracle,
                &format!("{arch:?} extended indices"),
            );
        }
        snapshots += script.snapshots;
        cases += 1;
        script.asm.reset().unwrap();
        let fresh_ctx = mcx::MemoryContext::new_bump("fresh");
        let fresh = AssemblerElf::new(fresh_ctx.mcx(), arch).unwrap();
        assert_eq!(
            script.asm.build_object_file().unwrap(),
            fresh.build_object_file().unwrap()
        );
    }
    eprintln!(
        "TPDE_ASSEMBLER_ORACLE PASS cases={cases} snapshots={snapshots} cpp_modes=2 extended_targets=2"
    );
    fs::remove_dir_all(work).unwrap();
}
