// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use super::*;
use crate::image::{symbol_section, SymbolSection};
use mcx::{Mcx, MemoryContext};
use std::{vec, vec::Vec};
use tpde_assembler::{elf::*, Architecture, AssemblerElf, SecRef, SectionKind, SymBinding, SymRef};

const BASE: usize = 0x10_0000_0000;
const PAGE: usize = 4096;
const TARGETS: [Architecture; 2] = [Architecture::Aarch64, Architecture::X86_64];

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}
fn wide(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}
fn add(assembler: &mut AssemblerElf<'_>, kind: SectionKind, bytes: &[u8]) -> SecRef {
    let section = assembler.create_section(kind).unwrap();
    assembler
        .get_section_mut(section)
        .unwrap()
        .write(bytes)
        .unwrap();
    section
}
fn at(assembler: &AssemblerElf<'_>, section: SecRef) -> usize {
    assembler.get_section(section).unwrap().addr as usize
}
fn abs_kind(architecture: Architecture) -> u32 {
    match architecture {
        Architecture::Aarch64 => R_AARCH64_ABS64,
        Architecture::X86_64 => R_X86_64_64,
    }
}
fn pc_kind(architecture: Architecture) -> u32 {
    match architecture {
        Architecture::Aarch64 => R_AARCH64_PREL32,
        Architecture::X86_64 => R_X86_64_PC32,
    }
}
struct Fixture<'m> {
    assembler: AssemblerElf<'m>,
    mapper: MapperImage<'m>,
    symbol: SymRef,
    section: SecRef,
    offset: usize,
    bytes: Vec<u8>,
}
impl<'m> Fixture<'m> {
    fn new(
        mcx: Mcx<'m>,
        architecture: Architecture,
        kind: u32,
        addend: i32,
        initial: &[u8],
    ) -> Self {
        let mut assembler = AssemblerElf::new(mcx, architecture).unwrap();
        let section = add(&mut assembler, SectionKind::Text, initial);
        let symbol = assembler
            .sym_add_undef(b"target", SymBinding::Global)
            .unwrap();
        assembler
            .reloc_sec(section, symbol, kind, 0, i64::from(addend))
            .unwrap();
        let mut mapper = MapperImage::new(mcx).unwrap();
        let size = mapper.layout(&mut assembler, PAGE).unwrap();
        let offset = at(&assembler, section);
        Self {
            assembler,
            mapper,
            symbol,
            section,
            offset,
            bytes: vec![0xa5; size + 16],
        }
    }
    fn absolute(&mut self, address: u64) -> bool {
        self.assembler
            .sym_def_absolute(self.symbol, address, 0)
            .unwrap();
        self.mapper
            .relocate(&self.assembler, &mut self.bytes, BASE, |_| {
                panic!("absolute resolver")
            })
            .unwrap()
    }
    fn relative(&mut self, displacement: i64, addend: i32) -> bool {
        let address =
            (BASE as i128 + self.offset as i128 + displacement as i128 - addend as i128) as u64;
        self.absolute(address)
    }
    fn guards(&self) {
        assert_eq!(&self.bytes[self.mapper.size()..], &[0xa5; 16]);
    }
}

#[test]
fn empty_layout_creates_and_terminates_eh_frame_for_both_targets() {
    for architecture in TARGETS {
        let context = MemoryContext::new_bump("empty mapper");
        let mut assembler = AssemblerElf::new(context.mcx(), architecture).unwrap();
        let mut mapper = MapperImage::new(context.mcx()).unwrap();
        assert_eq!(mapper.size(), 0);
        assert!(mapper.permissions().is_empty());
        assert_eq!(mapper.layout(&mut assembler, PAGE).unwrap(), PAGE + 4);
        assert_eq!(mapper.architecture(), architecture);
        let eh = mapper.eh_frame_section();
        assert_eq!(
            eh,
            assembler.get_default_section(SectionKind::EHFrame).unwrap()
        );
        assert_eq!(at(&assembler, eh), PAGE);
        assert_eq!(
            mapper.permissions(),
            &[
                Permission {
                    offset: 0,
                    flags: SHF_ALLOC | SHF_EXECINSTR
                },
                Permission {
                    offset: PAGE,
                    flags: SHF_ALLOC
                },
                Permission {
                    offset: PAGE + 4,
                    flags: 0
                },
            ]
        );
        let mut bytes = vec![0xff; mapper.size()];
        assert!(mapper
            .relocate(&assembler, &mut bytes, BASE, |_| panic!("unused resolver"))
            .unwrap());
        assert!(bytes.iter().all(|&byte| byte == 0));
    }
}

#[test]
fn stable_section_order_and_permission_boundaries() {
    let context = MemoryContext::new_bump("section order");
    let mut a = AssemblerElf::new(context.mcx(), Architecture::Aarch64).unwrap();
    let bss = a.create_section(SectionKind::BSS).unwrap();
    a.get_section_mut(bss).unwrap().vsize = 33;
    a.get_section_mut(bss).unwrap().align = 16;
    let text1 = add(&mut a, SectionKind::Text, &[1; 3]);
    let ro1 = add(&mut a, SectionKind::ReadOnly, &[2; 3]);
    let data = add(&mut a, SectionKind::Data, &[3; 7]);
    let text2 = add(&mut a, SectionKind::Text, &[4]);
    a.get_section_mut(text2).unwrap().align = 64;
    let ro2 = add(&mut a, SectionKind::ReadOnly, &[5; 2]);
    a.get_section_mut(ro2).unwrap().align = 8;
    let mut mapper = MapperImage::new(context.mcx()).unwrap();
    assert_eq!(mapper.layout(&mut a, PAGE).unwrap(), 8241);
    assert_eq!(
        [
            at(&a, text1),
            at(&a, text2),
            at(&a, ro1),
            at(&a, ro2),
            at(&a, mapper.eh_frame_section()),
            at(&a, data),
            at(&a, bss)
        ],
        [16, 64, 4096, 4104, 4112, 8192, 8208]
    );
    assert_eq!(
        mapper.permissions(),
        &[
            Permission {
                offset: 0,
                flags: 6
            },
            Permission {
                offset: 4096,
                flags: 2
            },
            Permission {
                offset: 8192,
                flags: 3
            },
            Permission {
                offset: 8241,
                flags: 0
            },
        ]
    );
}

#[test]
fn complete_flags_split_pages_and_nobits_follows_initialized_storage() {
    let context = MemoryContext::new_bump("flag boundaries");
    let mut a = AssemblerElf::new(context.mcx(), Architecture::X86_64).unwrap();
    let tls_bss = a.create_section(SectionKind::ThreadBSS).unwrap();
    a.get_section_mut(tls_bss).unwrap().vsize = 1;
    let tls_data = add(&mut a, SectionKind::ThreadData, &[1]);
    let data = add(&mut a, SectionKind::Data, &[2]);
    let executable_write = add(&mut a, SectionKind::Text, &[3]);
    a.get_section_mut(executable_write).unwrap().flags |= SHF_WRITE;
    let mut mapper = MapperImage::new(context.mcx()).unwrap();
    mapper.layout(&mut a, PAGE).unwrap();
    assert_eq!(at(&a, executable_write), PAGE);
    assert!(at(&a, tls_data) < at(&a, data));
    assert!(at(&a, data) < at(&a, tls_bss));
    assert_eq!(at(&a, tls_bss) % PAGE, 0);
    for boundary in &mapper.permissions()[..mapper.permissions().len() - 1] {
        assert_eq!(boundary.offset % PAGE, 0);
    }
    assert!(mapper
        .permissions()
        .iter()
        .any(|p| p.flags == SHF_ALLOC | SHF_WRITE | SHF_TLS));
}

#[test]
fn changed_permissions_use_page_alignment_and_equal_permissions_use_section_alignment() {
    let context = MemoryContext::new_bump("large alignment");
    let mut a = AssemblerElf::new(context.mcx(), Architecture::Aarch64).unwrap();
    let first = add(&mut a, SectionKind::ReadOnly, &[1]);
    let second = add(&mut a, SectionKind::ReadOnly, &[2]);
    a.get_section_mut(first).unwrap().align = 8192;
    a.get_section_mut(second).unwrap().align = 8192;
    let mut mapper = MapperImage::new(context.mcx()).unwrap();
    mapper.layout(&mut a, PAGE).unwrap();
    assert_eq!((at(&a, first), at(&a, second)), (4096, 8192));
}

#[test]
fn copies_live_bytes_zeroes_bss_gaps_and_eh_terminator_only_within_image() {
    for architecture in TARGETS {
        let context = MemoryContext::new_bump("copied image");
        let mut a = AssemblerElf::new(context.mcx(), architecture).unwrap();
        let text = add(&mut a, SectionKind::Text, &[0x10, 0x20, 0x30]);
        let eh = a.get_default_section(SectionKind::EHFrame).unwrap();
        a.get_section_mut(eh).unwrap().write(&[1u8; 12]).unwrap();
        let bss = a.create_section(SectionKind::BSS).unwrap();
        a.get_section_mut(bss).unwrap().vsize = 37;
        let mut mapper = MapperImage::new(context.mcx()).unwrap();
        let size = mapper.layout(&mut a, PAGE).unwrap();
        let mut bytes = vec![0xa5; size + 7];
        assert!(mapper.relocate(&a, &mut bytes, BASE, |_| 0).unwrap());
        assert_eq!(&bytes[at(&a, text)..at(&a, text) + 3], &[0x10, 0x20, 0x30]);
        assert!(bytes[..at(&a, text)].iter().all(|&byte| byte == 0));
        assert!(bytes[at(&a, text) + 3..at(&a, eh)]
            .iter()
            .all(|&byte| byte == 0));
        assert_eq!(&bytes[at(&a, eh)..at(&a, eh) + 12], &[1; 12]);
        assert_eq!(&bytes[at(&a, eh) + 12..at(&a, eh) + 16], &[0; 4]);
        assert!(bytes[at(&a, eh) + 16..at(&a, bss)]
            .iter()
            .all(|&byte| byte == 0));
        assert_eq!(&bytes[at(&a, bss)..at(&a, bss) + 37], &[0; 37]);
        assert_eq!(&bytes[size..], &[0xa5; 7]);
    }
}

#[test]
fn nonalloc_sections_and_their_relocations_are_not_mapped() {
    let context = MemoryContext::new_bump("ignored section");
    let mut a = AssemblerElf::new(context.mcx(), Architecture::X86_64).unwrap();
    let section = add(&mut a, SectionKind::Data, &[0xee; 8]);
    a.get_section_mut(section).unwrap().flags = 0;
    let symbol = a.sym_add_undef(b"unused", SymBinding::Global).unwrap();
    a.reloc_abs(section, symbol, 0, 0).unwrap();
    let mut mapper = MapperImage::new(context.mcx()).unwrap();
    let mut bytes = vec![0xff; mapper.layout(&mut a, PAGE).unwrap()];
    assert!(mapper
        .relocate(&a, &mut bytes, BASE, |_| panic!("nonalloc relocation"))
        .unwrap());
    assert!(bytes.iter().all(|&b| b == 0));
    assert_eq!(mapper.symbol_address(symbol), None);
}

#[test]
fn defined_globals_are_eager_and_locals_are_lazy() {
    for architecture in TARGETS {
        let context = MemoryContext::new_bump("defined symbols");
        let mut a = AssemblerElf::new(context.mcx(), architecture).unwrap();
        let section = add(&mut a, SectionKind::Text, &[0; 16]);
        let global = a.sym_predef_func(b"global", SymBinding::Global).unwrap();
        let weak = a.sym_predef_func(b"weak", SymBinding::Weak).unwrap();
        let local = a.sym_predef_func(b"local", SymBinding::Local).unwrap();
        let used = a.sym_predef_func(b"used", SymBinding::Local).unwrap();
        for (symbol, offset) in [(global, 1), (weak, 2), (local, 3), (used, 4)] {
            a.sym_def(symbol, section, offset, 0).unwrap();
        }
        a.reloc_abs(section, used, 8, 0).unwrap();
        let mut mapper = MapperImage::new(context.mcx()).unwrap();
        let mut bytes = vec![0; mapper.layout(&mut a, PAGE).unwrap()];
        assert_eq!(mapper.symbol_address(global), None);
        assert!(mapper
            .relocate(&a, &mut bytes, BASE, |_| panic!("defined resolver"))
            .unwrap());
        assert_eq!(
            mapper.symbol_address(global),
            Some(BASE + at(&a, section) + 1)
        );
        assert_eq!(
            mapper.symbol_address(weak),
            Some(BASE + at(&a, section) + 2)
        );
        assert_eq!(mapper.symbol_address(local), None);
        assert_eq!(
            mapper.symbol_address(used),
            Some(BASE + at(&a, section) + 4)
        );
        assert_eq!(
            wide(&bytes, at(&a, section) + 8),
            (BASE + at(&a, section) + 4) as u64
        );
    }
}

#[test]
fn absolute_globals_skip_eager_resolution_and_zero_remains_a_valid_absolute() {
    let context = MemoryContext::new_bump("absolute symbols");
    let mut a = AssemblerElf::new(context.mcx(), Architecture::X86_64).unwrap();
    let section = add(&mut a, SectionKind::Data, &[0; 16]);
    let unused = a.sym_add_undef(b"unused", SymBinding::Global).unwrap();
    let used = a.sym_add_undef(b"used", SymBinding::Global).unwrap();
    let zero = a.sym_add_undef(b"zero", SymBinding::Global).unwrap();
    for (symbol, value) in [(unused, 999), (used, u64::MAX), (zero, 0)] {
        a.sym_def_absolute(symbol, value, 0).unwrap();
    }
    a.reloc_abs(section, used, 0, 0).unwrap();
    a.reloc_abs(section, zero, 8, -1).unwrap();
    let mut mapper = MapperImage::new(context.mcx()).unwrap();
    let mut bytes = vec![0; mapper.layout(&mut a, PAGE).unwrap()];
    assert!(mapper
        .relocate(&a, &mut bytes, BASE, |_| panic!("ABS resolver"))
        .unwrap());
    assert_eq!(mapper.symbol_address(unused), None);
    assert_eq!(mapper.symbol_address(used), Some(usize::MAX));
    assert_eq!(mapper.symbol_address(zero), None);
    assert_eq!(wide(&bytes, at(&a, section)), u64::MAX);
    assert_eq!(wide(&bytes, at(&a, section) + 8), u64::MAX);
}

#[test]
fn unused_undefined_global_and_weak_symbols_do_not_require_resolution() {
    let context = MemoryContext::new_bump("unused symbols");
    let mut a = AssemblerElf::new(context.mcx(), Architecture::Aarch64).unwrap();
    let global = a.sym_add_undef(b"global", SymBinding::Global).unwrap();
    let weak = a.sym_add_undef(b"weak", SymBinding::Weak).unwrap();
    let mut mapper = MapperImage::new(context.mcx()).unwrap();
    let mut bytes = vec![0; mapper.layout(&mut a, PAGE).unwrap()];
    assert!(mapper
        .relocate(&a, &mut bytes, BASE, |_| panic!("unused undefined"))
        .unwrap());
    assert_eq!(
        (mapper.symbol_address(global), mapper.symbol_address(weak)),
        (None, None)
    );
}

#[test]
fn nonzero_resolver_results_are_cached_once_per_symbol_and_mapping() {
    let context = MemoryContext::new_bump("resolver cache");
    let mut f = Fixture::new(
        context.mcx(),
        Architecture::X86_64,
        R_X86_64_64,
        0,
        &[0; 24],
    );
    f.assembler.reloc_abs(f.section, f.symbol, 8, 1).unwrap();
    f.assembler.reloc_abs(f.section, f.symbol, 16, -1).unwrap();
    for address in [0x1234, 0x9876] {
        let mut calls = 0;
        assert!(f
            .mapper
            .relocate(&f.assembler, &mut f.bytes, BASE, |name| {
                assert_eq!(name, b"target");
                calls += 1;
                address
            })
            .unwrap());
        assert_eq!(calls, 1);
        assert_eq!(f.mapper.symbol_address(f.symbol), Some(address));
        assert_eq!(
            [
                wide(&f.bytes, f.offset),
                wide(&f.bytes, f.offset + 8),
                wide(&f.bytes, f.offset + 16)
            ],
            [address as u64, address as u64 + 1, address as u64 - 1]
        );
    }
    f.guards();
}

#[test]
fn weak_zero_is_retried_until_a_nonzero_resolution_can_be_cached() {
    let context = MemoryContext::new_bump("weak zero");
    let mut a = AssemblerElf::new(context.mcx(), Architecture::Aarch64).unwrap();
    let section = add(&mut a, SectionKind::Data, &[0; 24]);
    let symbol = a.sym_add_undef(b"weak", SymBinding::Weak).unwrap();
    for offset in [0, 8, 16] {
        a.reloc_abs(section, symbol, offset, 0).unwrap();
    }
    let mut mapper = MapperImage::new(context.mcx()).unwrap();
    let mut bytes = vec![0; mapper.layout(&mut a, PAGE).unwrap()];
    let mut calls = 0;
    assert!(mapper
        .relocate(&a, &mut bytes, BASE, |_| {
            calls += 1;
            0
        })
        .unwrap());
    assert_eq!(calls, 3);
    assert_eq!(mapper.symbol_address(symbol), None);
    calls = 0;
    assert!(mapper
        .relocate(&a, &mut bytes, BASE, |_| {
            calls += 1;
            if calls == 1 {
                0
            } else {
                7
            }
        })
        .unwrap());
    assert_eq!(calls, 2);
    assert_eq!(
        [
            wide(&bytes, at(&a, section)),
            wide(&bytes, at(&a, section) + 8),
            wide(&bytes, at(&a, section) + 16)
        ],
        [0, 7, 7]
    );
    assert_eq!(mapper.symbol_address(symbol), Some(7));
}

#[test]
fn failed_strong_resolution_clears_all_lookups_and_allows_reuse() {
    let context = MemoryContext::new_bump("strong zero");
    let mut f = Fixture::new(
        context.mcx(),
        Architecture::X86_64,
        R_X86_64_64,
        0,
        &[0; 16],
    );
    f.assembler.reloc_abs(f.section, f.symbol, 8, 0).unwrap();
    let defined = f
        .assembler
        .sym_predef_func(b"defined", SymBinding::Global)
        .unwrap();
    f.assembler.sym_def(defined, f.section, 0, 0).unwrap();
    let size = f.mapper.layout(&mut f.assembler, PAGE).unwrap();
    f.bytes.resize(size + 16, 0xa5);
    let mut calls = 0;
    assert!(!f
        .mapper
        .relocate(&f.assembler, &mut f.bytes, BASE, |_| {
            calls += 1;
            0
        })
        .unwrap());
    assert_eq!(calls, 2);
    assert_eq!(
        (
            f.mapper.symbol_address(f.symbol),
            f.mapper.symbol_address(defined)
        ),
        (None, None)
    );
    assert!(f
        .mapper
        .relocate(&f.assembler, &mut f.bytes, BASE, |_| 123)
        .unwrap());
    assert_eq!(f.mapper.symbol_address(f.symbol), Some(123));
    assert_eq!(
        f.mapper.symbol_address(defined),
        Some(BASE + at(&f.assembler, f.section))
    );
}

#[test]
fn every_reserved_symbol_section_has_explicit_classification() {
    assert_eq!(
        symbol_section(&Elf64_Sym::default()),
        SymbolSection::Undefined
    );
    for index in [1, 2, 0xfeff] {
        assert_eq!(
            symbol_section(&Elf64_Sym {
                st_shndx: index,
                ..Elf64_Sym::default()
            }),
            SymbolSection::Defined
        );
    }
    for index in 0xff00..=0xffff {
        let expected = match index {
            0xfff1 => SymbolSection::Absolute,
            0xffff => SymbolSection::Defined,
            _ => SymbolSection::Unsupported,
        };
        assert_eq!(
            symbol_section(&Elf64_Sym {
                st_shndx: index,
                ..Elf64_Sym::default()
            }),
            expected,
            "{index:x}"
        );
    }
}

#[test]
fn referenced_or_eager_symbols_in_nonalloc_sections_fail_closed() {
    for binding in [SymBinding::Global, SymBinding::Local] {
        let context = MemoryContext::new_bump("nonalloc symbol");
        let mut a = AssemblerElf::new(context.mcx(), Architecture::X86_64).unwrap();
        let ignored = add(&mut a, SectionKind::Data, &[0; 8]);
        a.get_section_mut(ignored).unwrap().flags = 0;
        let mapped = add(&mut a, SectionKind::Data, &[0; 8]);
        let symbol = a.sym_predef_data(b"symbol", binding).unwrap();
        a.sym_def(symbol, ignored, 0, 0).unwrap();
        if binding == SymBinding::Local {
            a.reloc_abs(mapped, symbol, 0, 0).unwrap();
        }
        let mut mapper = MapperImage::new(context.mcx()).unwrap();
        let mut bytes = vec![0; mapper.layout(&mut a, PAGE).unwrap()];
        assert!(!mapper.relocate(&a, &mut bytes, BASE, |_| 0).unwrap());
        assert_eq!(mapper.symbol_address(symbol), None);
    }
}

#[test]
fn section_end_symbols_are_valid_but_past_end_symbols_are_rejected() {
    for position in [0, 8, 9, u64::MAX] {
        let context = MemoryContext::new_bump("symbol bounds");
        let mut a = AssemblerElf::new(context.mcx(), Architecture::Aarch64).unwrap();
        let section = add(&mut a, SectionKind::Text, &[0; 8]);
        let symbol = a.sym_predef_func(b"symbol", SymBinding::Global).unwrap();
        a.sym_def(symbol, section, position, 0).unwrap();
        let mut mapper = MapperImage::new(context.mcx()).unwrap();
        let mut bytes = vec![0; mapper.layout(&mut a, PAGE).unwrap()];
        assert_eq!(
            mapper.relocate(&a, &mut bytes, BASE, |_| 0).unwrap(),
            position <= 8
        );
        assert_eq!(
            mapper.symbol_address(symbol),
            if position <= 8 {
                Some(BASE + at(&a, section) + position as usize)
            } else {
                None
            }
        );
    }
}

#[test]
fn invalid_symbol_lookups_are_none_before_and_after_resolution() {
    let context = MemoryContext::new_bump("lookup bounds");
    let mut a = AssemblerElf::new(context.mcx(), Architecture::X86_64).unwrap();
    let mut mapper = MapperImage::new(context.mcx()).unwrap();
    let invalid = [
        SymRef::default(),
        SymRef::new(1),
        SymRef::new(0x7fff_ffff),
        SymRef::new(0x8000_0000),
        SymRef::new(u32::MAX),
    ];
    for symbol in invalid {
        assert_eq!(mapper.symbol_address(symbol), None);
    }
    let mut bytes = vec![0; mapper.layout(&mut a, PAGE).unwrap()];
    assert!(mapper.relocate(&a, &mut bytes, BASE, |_| 0).unwrap());
    for symbol in invalid {
        assert_eq!(mapper.symbol_address(symbol), None);
    }
}

#[test]
fn absolute_relocations_preserve_all_64_bits_and_wrapping_addends() {
    for architecture in TARGETS {
        for (address, addend) in [
            (0, -1),
            (u64::MAX, 1),
            (0x1234_5678_9abc_def0, i32::MIN),
            (0x8000_0000_0000_0000, i32::MAX),
        ] {
            let context = MemoryContext::new_bump("ABS64");
            let mut f = Fixture::new(
                context.mcx(),
                architecture,
                abs_kind(architecture),
                addend,
                &[0xaa; 16],
            );
            assert!(f.absolute(address));
            assert_eq!(
                wide(&f.bytes, f.offset),
                address.wrapping_add(addend as i64 as u64)
            );
            assert_eq!(&f.bytes[f.offset + 8..f.offset + 16], &[0xaa; 8]);
            f.guards();
        }
    }
}

#[test]
fn pc32_relocations_accept_exact_signed_bounds_and_reject_adjacent_values() {
    for architecture in TARGETS {
        for displacement in [
            i32::MIN as i64 - 1,
            i32::MIN as i64,
            -1,
            0,
            1,
            i32::MAX as i64,
            i32::MAX as i64 + 1,
        ] {
            for addend in [-4, 0] {
                let context = MemoryContext::new_bump("PC32");
                let mut f = Fixture::new(
                    context.mcx(),
                    architecture,
                    pc_kind(architecture),
                    addend,
                    &[0xaa; 8],
                );
                let accepted = f.relative(displacement, addend);
                assert_eq!(accepted, i32::try_from(displacement).is_ok());
                assert_eq!(word(&f.bytes, f.offset), displacement as u32);
                assert_eq!(&f.bytes[f.offset + 4..f.offset + 8], &[0xaa; 4]);
                if !accepted {
                    assert_eq!(f.mapper.symbol_address(f.symbol), None);
                }
                f.guards();
            }
        }
    }
}

#[test]
fn x64_plt32_uses_direct_signed_bounds_and_real_plt_for_far_targets() {
    for displacement in [
        i32::MIN as i64 - 1,
        i32::MIN as i64,
        -4,
        0,
        i32::MAX as i64,
        i32::MAX as i64 + 1,
    ] {
        let context = MemoryContext::new_bump("PLT32");
        let mut f = Fixture::new(
            context.mcx(),
            Architecture::X86_64,
            R_X86_64_PLT32,
            -4,
            &[0; 8],
        );
        assert!(f.relative(displacement, -4));
        if i32::try_from(displacement).is_ok() {
            assert_eq!(word(&f.bytes, f.offset), displacement as u32);
            assert_eq!(&f.bytes[..16], &[0; 16]);
        } else {
            assert_eq!(word(&f.bytes, f.offset), (-(f.offset as i64) - 4) as u32);
            assert_eq!(&f.bytes[..8], &[0xff, 0x25, 2, 0, 0, 0, 0x0f, 0x0b]);
            assert_eq!(
                wide(&f.bytes, 8),
                (BASE as i64 + f.offset as i64 + displacement + 4) as u64
            );
        }
        f.guards();
    }
}

#[test]
fn x64_got_and_plt_share_slots_by_symbol_not_resolved_address() {
    let context = MemoryContext::new_bump("GOT slots");
    let mut f = Fixture::new(
        context.mcx(),
        Architecture::X86_64,
        R_X86_64_GOTPCREL,
        0,
        &[0; 20],
    );
    let second = f
        .assembler
        .sym_add_undef(b"same address", SymBinding::Global)
        .unwrap();
    f.assembler
        .reloc_sec(f.section, f.symbol, R_X86_64_PLT32, 4, -4)
        .unwrap();
    f.assembler
        .reloc_sec(f.section, f.symbol, R_X86_64_GOTPCREL, 8, 0)
        .unwrap();
    f.assembler
        .reloc_sec(f.section, second, R_X86_64_GOTPCREL, 12, 0)
        .unwrap();
    let size = f.mapper.layout(&mut f.assembler, PAGE).unwrap();
    f.offset = at(&f.assembler, f.section);
    f.bytes.resize(size + 16, 0xa5);
    let address = BASE + (1usize << 40);
    let mut calls = 0;
    assert!(f
        .mapper
        .relocate(&f.assembler, &mut f.bytes, BASE, |_| {
            calls += 1;
            address
        })
        .unwrap());
    assert_eq!(calls, 2);
    assert_eq!(
        [
            word(&f.bytes, f.offset),
            word(&f.bytes, f.offset + 4),
            word(&f.bytes, f.offset + 8),
            word(&f.bytes, f.offset + 12)
        ],
        [
            (8i64 - f.offset as i64) as u32,
            (-8i64 - f.offset as i64) as u32,
            (-(f.offset as i64)) as u32,
            (12i64 - f.offset as i64) as u32
        ]
    );
    for start in [0, 16] {
        assert_eq!(
            &f.bytes[start..start + 8],
            &[0xff, 0x25, 2, 0, 0, 0, 0x0f, 0x0b]
        );
        assert_eq!(wide(&f.bytes, start + 8), address as u64);
    }
    assert_eq!(&f.bytes[32..48], &[0; 16]);
}

#[test]
fn x64_got_and_replacement_plt_displacements_check_their_own_signed_bounds() {
    for kind in [R_X86_64_GOTPCREL, R_X86_64_PLT32] {
        for adjustment in [-1, 0, 1] {
            let context = MemoryContext::new_bump("slot range");
            let slot_bias = if kind == R_X86_64_GOTPCREL { 8 } else { 0 };
            let addend = i32::MIN + 32 - slot_bias + adjustment;
            let mut f = Fixture::new(context.mcx(), Architecture::X86_64, kind, addend, &[0; 4]);
            assert_eq!(f.offset, 32);
            assert_eq!(f.relative(1i64 << 35, addend), adjustment >= 0);
            assert_eq!(
                word(&f.bytes, f.offset),
                (i32::MIN as i64 + adjustment as i64) as u32
            );
            f.guards();
        }
    }
}

#[test]
fn a64_call26_direct_limits_and_fallback_preserve_opcode_bits() {
    for displacement in [
        -(1i64 << 27) - 4,
        -(1i64 << 27),
        -4,
        0,
        4,
        (1i64 << 27) - 4,
        1i64 << 27,
        1,
    ] {
        let context = MemoryContext::new_bump("CALL26");
        let mut f = Fixture::new(
            context.mcx(),
            Architecture::Aarch64,
            R_AARCH64_CALL26,
            0,
            &0x97ff_ffffu32.to_le_bytes(),
        );
        assert!(f.relative(displacement, 0));
        let direct = (-(1i64 << 27)..(1i64 << 27)).contains(&displacement) && displacement % 4 == 0;
        let selected = if direct {
            displacement
        } else {
            -(f.offset as i64)
        };
        assert_eq!(
            word(&f.bytes, f.offset),
            0x9400_0000 | ((selected / 4) as u32 & 0x03ff_ffff)
        );
        if direct {
            assert_eq!(&f.bytes[..16], &[0; 16]);
        } else {
            assert_eq!(
                [word(&f.bytes, 0), word(&f.bytes, 4)],
                [0x5800_0050, 0xd61f_0200]
            );
            assert_eq!(
                wide(&f.bytes, 8),
                (BASE as i64 + f.offset as i64 + displacement) as u64
            );
        }
        f.guards();
    }
}

#[test]
fn a64_call26_rejects_misaligned_or_out_of_range_replacement_addends() {
    for addend in [
        1,
        2,
        3,
        i32::MIN,
        i32::MAX,
        -(1 << 27) + 28,
        -(1 << 27) + 32,
        (1 << 27) + 28,
        (1 << 27) + 32,
    ] {
        let context = MemoryContext::new_bump("CALL26 rejection");
        let mut f = Fixture::new(
            context.mcx(),
            Architecture::Aarch64,
            R_AARCH64_CALL26,
            addend,
            &0x9400_0000u32.to_le_bytes(),
        );
        let replacement = addend as i64 - f.offset as i64;
        let accepted = replacement % 4 == 0 && (-(1i64 << 27)..(1i64 << 27)).contains(&replacement);
        assert_eq!(f.relative(1i64 << 35, addend), accepted);
        if !accepted {
            assert_eq!(f.mapper.symbol_address(f.symbol), None);
        }
        assert_eq!(word(&f.bytes, f.offset) & 0xfc00_0000, 0x9400_0000);
        f.guards();
    }
}

#[test]
fn a64_adrp_signed_page_limits_preserve_non_immediate_bits() {
    for pages in [
        -(1i64 << 20) - 1,
        -(1i64 << 20),
        -1,
        0,
        1,
        (1i64 << 20) - 1,
        1i64 << 20,
    ] {
        for low in [0, 4095] {
            let context = MemoryContext::new_bump("ADRP");
            let initial = 0xbfff_ffffu32;
            let mut f = Fixture::new(
                context.mcx(),
                Architecture::Aarch64,
                R_AARCH64_ADR_PREL_PG_HI21,
                -7,
                &initial.to_le_bytes(),
            );
            let symbol =
                ((BASE as i64 + f.offset as i64) / 4096 * 4096 + pages * 4096 + low + 7) as u64;
            assert_eq!(
                f.absolute(symbol),
                (-(1i64 << 20)..(1i64 << 20)).contains(&pages)
            );
            let immediate = pages as u32 & 0x1f_ffff;
            let expected =
                (initial & !0x60ff_ffe0) | ((immediate & 3) << 29) | ((immediate >> 2) << 5);
            assert_eq!(word(&f.bytes, f.offset), expected);
            f.guards();
        }
    }
}

#[test]
fn a64_add_low12_covers_every_bit_and_wrapping_addend() {
    for low in [0, 1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4095] {
        let context = MemoryContext::new_bump("ADD low12");
        let initial = 0xffff_ffffu32;
        let mut f = Fixture::new(
            context.mcx(),
            Architecture::Aarch64,
            R_AARCH64_ADD_ABS_LO12_NC,
            -1,
            &initial.to_le_bytes(),
        );
        assert!(f.absolute(if low == 4095 { 0 } else { low + 1 }));
        assert_eq!(
            word(&f.bytes, f.offset),
            (initial & !(0xfff << 10)) | (low as u32) << 10
        );
        f.guards();
    }
}

#[test]
fn a64_vector_low12_scales_all_page_offsets_and_wraps_addends() {
    for low in (0..4096u64).step_by(16) {
        let context = MemoryContext::new_bump("vector low12");
        let initial = 0x3dff_feefu32;
        let mut f = Fixture::new(
            context.mcx(),
            Architecture::Aarch64,
            R_AARCH64_LDST128_ABS_LO12_NC,
            0,
            &initial.to_le_bytes(),
        );
        assert!(f.absolute(0x1234_5000 + low));
        assert_eq!(
            word(&f.bytes, f.offset),
            0x3dc0_02ef | ((low as u32 / 16) << 10)
        );
        f.guards();
    }
    for (symbol, addend, immediate) in [
        (0xff0u64, 32i32, 1u32),
        (0x1000, -16, 255),
        (u64::MAX - 15, 32, 1),
        (0, -16, 255),
        (0x1001, -1, 0),
        (15, 1, 1),
    ] {
        let context = MemoryContext::new_bump("vector addend");
        let mut f = Fixture::new(
            context.mcx(),
            Architecture::Aarch64,
            R_AARCH64_LDST128_ABS_LO12_NC,
            addend,
            &0x3dff_feefu32.to_le_bytes(),
        );
        assert!(f.absolute(symbol));
        assert_eq!(word(&f.bytes, f.offset), 0x3dc0_02ef | (immediate << 10));
        f.guards();
    }
}

#[test]
fn a64_vector_low12_refuses_misalignment_and_truncation_then_reuses() {
    for low in 1..16u64 {
        let context = MemoryContext::new_bump("vector alignment");
        let initial = 0x3dff_feefu32;
        let mut f = Fixture::new(
            context.mcx(),
            Architecture::Aarch64,
            R_AARCH64_LDST128_ABS_LO12_NC,
            0,
            &initial.to_le_bytes(),
        );
        assert!(!f
            .mapper
            .relocate(&f.assembler, &mut f.bytes, BASE, |_| 0x1000 + low as usize)
            .unwrap());
        assert_eq!(word(&f.bytes, f.offset), initial);
        assert_eq!(f.mapper.symbol_address(f.symbol), None);
        f.guards();
        assert!(f
            .mapper
            .relocate(&f.assembler, &mut f.bytes, BASE, |_| 0x1ff0)
            .unwrap());
        assert_eq!(word(&f.bytes, f.offset), 0x3dc3_feef);
        assert_eq!(f.mapper.symbol_address(f.symbol), Some(0x1ff0));
        f.guards();
    }
    for length in 0..=4 {
        let context = MemoryContext::new_bump("vector relocation bounds");
        let mut f = Fixture::new(
            context.mcx(),
            Architecture::Aarch64,
            R_AARCH64_LDST128_ABS_LO12_NC,
            0,
            &0x3dff_feefu32.to_le_bytes()[..length],
        );
        assert_eq!(f.absolute(0x1ff0), length == 4);
        if length < 4 {
            assert_eq!(f.mapper.symbol_address(f.symbol), None);
            assert_eq!(
                &f.bytes[f.offset..f.offset + length],
                &0x3dff_feefu32.to_le_bytes()[..length]
            );
        }
        f.guards();
    }
}

#[test]
fn a64_scalar_low12_scales_all_page_offsets_and_wraps_addends() {
    for low in (0..4096u64).step_by(8) {
        let context = MemoryContext::new_bump("scalar low12");
        let initial = 0xf97f_feefu32;
        let mut f = Fixture::new(
            context.mcx(),
            Architecture::Aarch64,
            R_AARCH64_LDST64_ABS_LO12_NC,
            0,
            &initial.to_le_bytes(),
        );
        assert!(f.absolute(0x1234_5000 + low));
        assert_eq!(
            word(&f.bytes, f.offset),
            0xf940_02ef | ((low as u32 / 8) << 10)
        );
        f.guards();
    }
    for (symbol, addend, immediate) in [
        (0xff8u64, 16i32, 1u32),
        (0x1000, -8, 511),
        (u64::MAX - 7, 16, 1),
        (0, -8, 511),
        (0x1001, -1, 0),
        (7, 1, 1),
    ] {
        let context = MemoryContext::new_bump("scalar addend");
        let mut f = Fixture::new(
            context.mcx(),
            Architecture::Aarch64,
            R_AARCH64_LDST64_ABS_LO12_NC,
            addend,
            &0xf97f_feefu32.to_le_bytes(),
        );
        assert!(f.absolute(symbol));
        assert_eq!(word(&f.bytes, f.offset), 0xf940_02ef | (immediate << 10));
        f.guards();
    }
}

#[test]
fn a64_scalar_low12_refuses_misalignment_and_truncation_then_reuses() {
    for low in 1..8u64 {
        let context = MemoryContext::new_bump("scalar alignment");
        let initial = 0xf97f_feefu32;
        let mut f = Fixture::new(
            context.mcx(),
            Architecture::Aarch64,
            R_AARCH64_LDST64_ABS_LO12_NC,
            0,
            &initial.to_le_bytes(),
        );
        assert!(!f
            .mapper
            .relocate(&f.assembler, &mut f.bytes, BASE, |_| 0x1000 + low as usize)
            .unwrap());
        assert_eq!(word(&f.bytes, f.offset), initial);
        assert_eq!(f.mapper.symbol_address(f.symbol), None);
        f.guards();
        assert!(f
            .mapper
            .relocate(&f.assembler, &mut f.bytes, BASE, |_| 0x1ff8)
            .unwrap());
        assert_eq!(word(&f.bytes, f.offset), 0xf947_feef);
        assert_eq!(f.mapper.symbol_address(f.symbol), Some(0x1ff8));
        f.guards();
    }
    for length in 0..=4 {
        let context = MemoryContext::new_bump("scalar relocation bounds");
        let mut f = Fixture::new(
            context.mcx(),
            Architecture::Aarch64,
            R_AARCH64_LDST64_ABS_LO12_NC,
            0,
            &0xf97f_feefu32.to_le_bytes()[..length],
        );
        assert_eq!(f.absolute(0x1ff8), length == 4);
        if length < 4 {
            assert_eq!(f.mapper.symbol_address(f.symbol), None);
            assert_eq!(
                &f.bytes[f.offset..f.offset + length],
                &0xf97f_feefu32.to_le_bytes()[..length]
            );
        }
        f.guards();
    }
}

#[test]
fn a64_got_page_load_and_call_reuse_one_slot_and_ignore_got_addends() {
    let context = MemoryContext::new_bump("A64 GOT");
    let mut f = Fixture::new(
        context.mcx(),
        Architecture::Aarch64,
        R_AARCH64_ADR_GOT_PAGE,
        i32::MAX,
        &[0xff; 12],
    );
    f.assembler
        .reloc_sec(
            f.section,
            f.symbol,
            R_AARCH64_LD64_GOT_LO12_NC,
            4,
            i32::MIN as i64,
        )
        .unwrap();
    f.assembler
        .reloc_sec(f.section, f.symbol, R_AARCH64_CALL26, 8, 0)
        .unwrap();
    f.assembler.get_section_mut(f.section).unwrap().align = PAGE as u32;
    let size = f.mapper.layout(&mut f.assembler, PAGE).unwrap();
    f.offset = at(&f.assembler, f.section);
    f.bytes.resize(size + 16, 0xa5);
    assert_eq!(f.offset, PAGE);
    let address = BASE + (1usize << 40);
    assert!(f
        .mapper
        .relocate(&f.assembler, &mut f.bytes, BASE, |_| address)
        .unwrap());
    assert_eq!(
        [word(&f.bytes, 0), word(&f.bytes, 4)],
        [0x5800_0050, 0xd61f_0200]
    );
    assert_eq!(wide(&f.bytes, 8), address as u64);
    assert_eq!(&f.bytes[16..32], &[0; 16]);
    assert_eq!(word(&f.bytes, f.offset), 0xffff_ffff);
    assert_eq!(
        word(&f.bytes, f.offset + 4),
        (0xffff_ffff & !(0xfff << 10)) | (1 << 10)
    );
    assert_eq!(
        word(&f.bytes, f.offset + 8),
        0xfc00_0000 | (((-(f.offset as i64) - 8) / 4) as u32 & 0x03ff_ffff)
    );
}

#[test]
fn unsupported_relocations_fail_without_overwriting_the_target_word() {
    for architecture in TARGETS {
        for kind in [
            0,
            u32::MAX,
            match architecture {
                Architecture::Aarch64 => R_X86_64_64,
                Architecture::X86_64 => R_AARCH64_ABS64,
            },
        ] {
            let context = MemoryContext::new_bump("unsupported relocation");
            let mut f = Fixture::new(context.mcx(), architecture, kind, 0, &[0x5a; 8]);
            assert!(!f.absolute(123));
            assert_eq!(&f.bytes[f.offset..f.offset + 8], &[0x5a; 8]);
            assert_eq!(f.mapper.symbol_address(f.symbol), None);
            f.guards();
        }
    }
}

#[test]
fn relocation_width_bounds_accept_exact_fit_and_reject_every_short_suffix() {
    for architecture in TARGETS {
        for (kind, width) in [(abs_kind(architecture), 8), (pc_kind(architecture), 4)] {
            for length in 0..=width {
                let context = MemoryContext::new_bump("relocation width");
                let mut f = Fixture::new(context.mcx(), architecture, kind, 0, &vec![0xa7; length]);
                assert_eq!(f.relative(0, 0), length == width);
                if length < width {
                    assert_eq!(&f.bytes[f.offset..f.offset + length], &vec![0xa7; length]);
                }
                f.guards();
            }
        }
        let context = MemoryContext::new_bump("relocation offset");
        let mut f = Fixture::new(
            context.mcx(),
            architecture,
            abs_kind(architecture),
            0,
            &[0; 8],
        );
        f.assembler
            .reloc_abs(f.section, f.symbol, u32::MAX, 0)
            .unwrap();
        assert!(!f.absolute(7));
        f.guards();
    }
}

#[test]
fn invalid_page_alignment_section_alignment_and_size_are_recoverable() {
    let context = MemoryContext::new_bump("layout refusal");
    let mut a = AssemblerElf::new(context.mcx(), Architecture::Aarch64).unwrap();
    let section = a.create_section(SectionKind::BSS).unwrap();
    let mut mapper = MapperImage::new(context.mcx()).unwrap();
    for page in [0, 3, 4095] {
        assert!(mapper.layout(&mut a, page).is_err());
    }
    for align in [0, 3, 4095] {
        a.get_section_mut(section).unwrap().align = align;
        assert!(mapper.layout(&mut a, PAGE).is_err());
    }
    a.get_section_mut(section).unwrap().align = 1;
    for size in [isize::MAX as u64, u64::MAX] {
        a.get_section_mut(section).unwrap().vsize = size;
        assert!(mapper.layout(&mut a, PAGE).is_err());
    }
    a.get_section_mut(section).unwrap().vsize = 8;
    let mut bytes = vec![0; mapper.layout(&mut a, PAGE).unwrap()];
    assert!(mapper.relocate(&a, &mut bytes, BASE, |_| 0).unwrap());
}

#[test]
fn invalid_eh_frame_storage_is_rejected_before_plt_mapping_and_can_be_repaired() {
    for variant in 0..3 {
        let context = MemoryContext::new_bump("EH admission");
        let mut f = Fixture::new(
            context.mcx(),
            Architecture::Aarch64,
            R_AARCH64_CALL26,
            0,
            &0x9400_0000u32.to_le_bytes(),
        );
        let address = BASE as u64 + (1u64 << 40);
        assert!(f.absolute(address));
        let eh = f.mapper.eh_frame_section();
        let section = f.assembler.get_section_mut(eh).unwrap();
        match variant {
            0 => section.flags &= !SHF_ALLOC,
            1 => section.kind = SHT_NOBITS,
            _ => section.is_virtual = true,
        }
        assert!(f.mapper.layout(&mut f.assembler, PAGE).is_err());
        assert_eq!(f.mapper.size(), 0);
        assert_eq!(f.mapper.symbol_address(f.symbol), None);
        let section = f.assembler.get_section_mut(eh).unwrap();
        section.flags = SHF_ALLOC;
        section.kind = SHT_PROGBITS;
        section.is_virtual = false;
        let size = f.mapper.layout(&mut f.assembler, PAGE).unwrap();
        f.bytes.resize(size + 16, 0xa5);
        assert!(f
            .mapper
            .relocate(&f.assembler, &mut f.bytes, BASE, |_| panic!("ABS resolver"))
            .unwrap());
        assert_eq!(f.mapper.symbol_address(f.symbol), Some(address as usize));
        assert_eq!(
            [word(&f.bytes, 0), word(&f.bytes, 4)],
            [0x5800_0050, 0xd61f_0200]
        );
        assert_eq!(wide(&f.bytes, 8), address);
        assert_eq!(
            &f.bytes[at(&f.assembler, eh)..at(&f.assembler, eh) + 4],
            &[0; 4]
        );
        f.guards();
    }
}

#[test]
fn unsupported_elf_machine_is_rejected_before_layout() {
    let context = MemoryContext::new_bump("unsupported machine");
    let template = AssemblerElf::new(context.mcx(), Architecture::Aarch64).unwrap();
    let mut target = *template.target();
    target.elf_machine = 0;
    let mut assembler = AssemblerElf::with_target(context.mcx(), &target).unwrap();
    let mut mapper = MapperImage::new(context.mcx()).unwrap();
    assert!(mapper.layout(&mut assembler, PAGE).is_err());
    assert_eq!(mapper.size(), 0);
    assert!(mapper.permissions().is_empty());
}

#[test]
fn changed_section_extent_and_invalid_initialized_storage_are_rejected() {
    for variant in 0..3 {
        let context = MemoryContext::new_bump("changed section");
        let mut f = Fixture::new(context.mcx(), Architecture::X86_64, R_X86_64_64, 0, &[0; 8]);
        let section = f.assembler.get_section_mut(f.section).unwrap();
        match variant {
            0 => section.addr = u64::MAX,
            1 => {
                section.is_virtual = true;
                section.vsize = 8;
            }
            _ => {
                section.is_virtual = true;
                section.vsize = u64::MAX;
            }
        }
        assert!(f
            .mapper
            .relocate(&f.assembler, &mut f.bytes, BASE, |_| 7)
            .is_err());
        assert_eq!(f.mapper.symbol_address(f.symbol), None);
        f.guards();
    }
}

#[test]
fn changed_symbol_tables_require_a_new_layout() {
    for binding in [SymBinding::Local, SymBinding::Global] {
        let context = MemoryContext::new_bump("changed symbols");
        let mut f = Fixture::new(
            context.mcx(),
            Architecture::Aarch64,
            R_AARCH64_ABS64,
            0,
            &[0; 8],
        );
        f.assembler.sym_add_undef(b"new", binding).unwrap();
        assert!(f
            .mapper
            .relocate(&f.assembler, &mut f.bytes, BASE, |_| 7)
            .is_err());
        let size = f.mapper.layout(&mut f.assembler, PAGE).unwrap();
        f.bytes.resize(size + 16, 0xa5);
        assert!(f
            .mapper
            .relocate(&f.assembler, &mut f.bytes, BASE, |_| 7)
            .unwrap());
        assert_eq!(f.mapper.symbol_address(f.symbol), Some(7));
    }
}

#[test]
fn short_buffer_or_invalid_base_invalidates_lookup_without_touching_the_buffer() {
    let context = MemoryContext::new_bump("relocate admission");
    let mut f = Fixture::new(context.mcx(), Architecture::X86_64, R_X86_64_64, 0, &[0; 8]);
    for base in [0, BASE + 1, usize::MAX & !(PAGE - 1)] {
        assert!(f
            .mapper
            .relocate(&f.assembler, &mut f.bytes, BASE, |_| 7)
            .unwrap());
        assert_eq!(f.mapper.symbol_address(f.symbol), Some(7));
        let before = f.bytes.clone();
        assert!(f
            .mapper
            .relocate(&f.assembler, &mut f.bytes, base, |_| panic!(
                "invalid base resolver"
            ))
            .is_err());
        assert_eq!(f.mapper.symbol_address(f.symbol), None);
        assert_eq!(f.bytes, before);
    }
    assert!(f
        .mapper
        .relocate(&f.assembler, &mut f.bytes, BASE, |_| 7)
        .unwrap());
    let size = f.mapper.size();
    let mut short = vec![0x33; size - 1];
    assert!(f
        .mapper
        .relocate(&f.assembler, &mut short, BASE, |_| panic!(
            "short buffer resolver"
        ))
        .is_err());
    assert_eq!(f.mapper.symbol_address(f.symbol), None);
    assert!(short.iter().all(|&v| v == 0x33));
    assert!(f
        .mapper
        .relocate(&f.assembler, &mut f.bytes, BASE, |_| 9)
        .unwrap());
    assert_eq!(f.mapper.symbol_address(f.symbol), Some(9));
}

#[test]
fn reset_invalidates_layout_and_symbols_and_relayout_changes_target() {
    let context = MemoryContext::new_bump("mapper reset");
    let mut f = Fixture::new(
        context.mcx(),
        Architecture::Aarch64,
        R_AARCH64_ABS64,
        0,
        &[0; 8],
    );
    assert!(f.absolute(7));
    for _ in 0..2 {
        f.mapper.reset();
    }
    assert_eq!(f.mapper.size(), 0);
    assert!(f.mapper.permissions().is_empty());
    assert!(!f.mapper.eh_frame_section().valid());
    assert_eq!(f.mapper.symbol_address(f.symbol), None);
    assert!(f
        .mapper
        .relocate(&f.assembler, &mut f.bytes, BASE, |_| 0)
        .is_err());
    let mut replacement = AssemblerElf::new(context.mcx(), Architecture::X86_64).unwrap();
    let size = f.mapper.layout(&mut replacement, PAGE).unwrap();
    f.bytes.resize(size, 0);
    assert!(f
        .mapper
        .relocate(&replacement, &mut f.bytes, BASE, |_| 0)
        .unwrap());
    assert_eq!(f.mapper.architecture(), Architecture::X86_64);
}

#[test]
fn allocation_budget_failure_can_be_followed_by_a_smaller_mapping() {
    let empty = MemoryContext::new("no mapper allocation").with_limit(0);
    assert!(MapperImage::new(empty.mcx()).is_err());
    let source_context = MemoryContext::new_bump("large symbols");
    let mut large = AssemblerElf::new(source_context.mcx(), Architecture::X86_64).unwrap();
    for _ in 0..4096 {
        large.sym_add_undef(b"unused", SymBinding::Global).unwrap();
    }
    let mapper_context = MemoryContext::new("limited mapper").with_limit(32768);
    let mut mapper = MapperImage::new(mapper_context.mcx()).unwrap();
    assert!(mapper.layout(&mut large, PAGE).is_err());
    assert!(mapper_context.stats().used <= 32768);
    let mut small = AssemblerElf::new(source_context.mcx(), Architecture::Aarch64).unwrap();
    let mut bytes = vec![0; mapper.layout(&mut small, PAGE).unwrap()];
    assert!(mapper.relocate(&small, &mut bytes, BASE, |_| 0).unwrap());
    assert_eq!(mapper.architecture(), Architecture::Aarch64);
}

#[test]
fn spilled_mapper_scratch_is_charged_and_retained_across_reset() {
    for bump in [false, true] {
        let context = if bump {
            MemoryContext::new_bump("bump mapper")
        } else {
            MemoryContext::new("pool mapper")
        };
        let mut mapper = MapperImage::new(context.mcx()).unwrap();
        let initial = context.stats().used;
        let symbols = 300.max(initial / core::mem::size_of::<usize>() + 1);
        let source_context = MemoryContext::new_bump("spill source");
        let mut a = AssemblerElf::new(source_context.mcx(), Architecture::X86_64).unwrap();
        for _ in 0..symbols {
            a.sym_add_undef(b"unused", SymBinding::Global).unwrap();
        }
        for _ in 0..24 {
            add(&mut a, SectionKind::Text, &[1]);
        }
        a.get_default_section(SectionKind::EHFrame).unwrap();
        let size = mapper.layout(&mut a, PAGE).unwrap();
        let retained = context.stats().used;
        let live_cache_bytes = (symbols + 1) * 2 * core::mem::size_of::<usize>();
        assert!(
            retained > initial,
            "bump={bump} initial={initial} retained={retained}"
        );
        assert!(retained >= live_cache_bytes);
        let peak = context.stats().peak;
        assert!(peak >= retained);
        let mut bytes = vec![0; size];
        assert!(mapper.relocate(&a, &mut bytes, BASE, |_| 0).unwrap());
        for _ in 0..3 {
            mapper.reset();
            assert_eq!(context.stats().used, retained);
            assert_eq!(mapper.layout(&mut a, PAGE).unwrap(), size);
            assert!(mapper.relocate(&a, &mut bytes, BASE, |_| 0).unwrap());
            assert_eq!(context.stats().used, retained);
            assert_eq!(context.stats().peak, peak);
        }
    }
}

/// The image's tables (its state and their spilled storage) go back to the mapper's context when the image drops, so a
/// context that must be empty when its holder drops it (a code-cache entry's root, an mcx `SendRoot`) is; a permission
/// stays plain data.
#[test]
fn permission_storage_has_no_drop_glue_and_a_dropped_image_frees_its_tables() {
    assert!(!core::mem::needs_drop::<Permission>());
    assert!(core::mem::size_of::<MapperImage<'static>>() <= 128);
    let context = MemoryContext::new("pool mapper");
    let source_context = MemoryContext::new_bump("spill source");
    let mut a = AssemblerElf::new(source_context.mcx(), Architecture::X86_64).unwrap();
    for _ in 0..300 {
        a.sym_add_undef(b"unused", SymBinding::Global).unwrap();
    }
    for _ in 0..24 {
        add(&mut a, SectionKind::Text, &[1]);
    }
    a.get_default_section(SectionKind::EHFrame).unwrap();
    let mut mapper = MapperImage::new(context.mcx()).unwrap();
    let initial = context.stats().used;
    mapper.layout(&mut a, PAGE).unwrap();
    assert!(context.stats().used > initial, "the tables spilled");
    drop(mapper);
    assert_eq!(context.stats().used, 0, "the image's tables went back with it");
}

/// The mapping arena (docs/design/gravity-simple/compile-latency.md §2,
/// lever 2b): a released image's range is reused by the next mapping,
/// zeroed outside the image's own bytes, and two live images never share
/// a range.
#[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
#[test]
fn arena_reuses_a_released_range_zeroed_and_keeps_live_images_apart() {
    #[cfg(target_arch = "aarch64")]
    let architecture = Architecture::Aarch64;
    #[cfg(target_arch = "x86_64")]
    let architecture = Architecture::X86_64;
    let context = MemoryContext::new_bump("arena test");
    let build = |text: &[u8]| {
        let mut a = AssemblerElf::new(context.mcx(), architecture).unwrap();
        let t = add(&mut a, SectionKind::Text, text);
        (a, t)
    };
    let map = |a: &mut AssemblerElf<'_>| {
        let mut m = ElfMapper::new(context.mcx()).unwrap();
        // SAFETY: no unwind records beyond the terminator, nothing executes.
        assert!(unsafe { m.map(a, |_| 0) }.unwrap());
        m
    };
    let (mut a1, text1) = build(&[0x10, 0x20, 0x30, 0x40]);
    let m1 = map(&mut a1);
    let r1 = m1.get_mapped_range().unwrap();
    // SAFETY: nothing runs or writes the mapping.
    let (addr1, bytes1) = (r1.address(), unsafe { r1.bytes() }.to_vec());
    assert_eq!(&bytes1[at(&a1, text1)..][..4], &[0x10, 0x20, 0x30, 0x40]);
    // A second live image sits elsewhere.
    let (mut a2, _) = build(&[0x50, 0x60]);
    let m2 = map(&mut a2);
    let addr2 = m2.get_mapped_range().unwrap().address();
    assert_ne!(addr1, addr2);
    drop(m1);
    drop(m2);
    // Both released: the chunk is empty and reused from its start — the
    // first image's range comes back, without the first image's bytes.
    let (mut a3, text3) = build(&[0x70]);
    let m3 = map(&mut a3);
    let r3 = m3.get_mapped_range().unwrap();
    assert_eq!(r3.address(), addr1);
    // SAFETY: as above.
    let bytes3 = unsafe { r3.bytes() }.to_vec();
    let text3 = at(&a3, text3);
    assert_eq!(bytes3[text3], 0x70);
    assert!(bytes3[text3 + 1..text3 + 4].iter().all(|&b| b == 0), "the previous image's bytes survived the reuse");
}
