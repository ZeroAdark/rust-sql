// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use super::*;
use elf::*;
use mcx::MemoryContext;

fn word(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}
fn wide(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

#[test]
fn empty_headers_and_both_targets() {
    for (target, machine) in [
        (Architecture::Aarch64, EM_AARCH64),
        (Architecture::X86_64, EM_X86_64),
    ] {
        let ctx = MemoryContext::new_bump("asm");
        let a = AssemblerElf::new(ctx.mcx(), target).unwrap();
        let obj = a.build_object_file().unwrap();
        assert_eq!(&obj[..7], b"\x7fELF\x02\x01\x01");
        assert_eq!(u16::from_le_bytes(obj[18..20].try_into().unwrap()), machine);
        assert_eq!(wide(&obj, 40), 64);
        assert_eq!(u16::from_le_bytes(obj[60..62].try_into().unwrap()), 5);
        assert_eq!(word(&obj, 64 + 2 * 64 + 44), 1);
    }
}
#[test]
fn defaults_and_reserved_relocation_slots() {
    let ctx = MemoryContext::new_bump("asm");
    let mut a = AssemblerElf::new(ctx.mcx(), Architecture::Aarch64).unwrap();
    assert_eq!(a.data_sections().count(), 0);
    assert_eq!(a.local_symbol_count(), 1);
    assert!(a.global_symbols().is_empty());
    let mut expected = [SecRef::default(); SectionKind::ALL.len()];
    for kind in SectionKind::ALL {
        let before = a.section_count();
        let sec = a.get_default_section(kind).unwrap();
        expected[kind as usize] = sec;
        assert_eq!(sec.id() as usize, before);
        let has_relocs = a.target().section_flags[kind as usize].has_relocs;
        assert_eq!(a.section_count(), before + if has_relocs { 2 } else { 1 });
        assert_eq!(a.get_default_section(kind).unwrap(), sec);
        assert_eq!(a.get_section(sec).unwrap().get_ref(), sec);
        if has_relocs {
            assert!(a.get_section(SecRef::new(sec.id() + 1)).is_err());
        }
    }
    let mut actual = a.data_sections();
    for sec in expected {
        assert_eq!(actual.next().unwrap().get_ref(), sec);
    }
    assert!(actual.next().is_none());
    drop(actual);
    let local = a.sym_predef_func(b"local", SymBinding::Local).unwrap();
    let global = a.sym_add_undef(b"global", SymBinding::Global).unwrap();
    let weak = a.sym_add_undef(b"weak", SymBinding::Weak).unwrap();
    assert_eq!(a.local_symbol_count(), 2);
    assert!(AssemblerElf::is_local(local));
    assert_eq!(AssemblerElf::sym_index(local), 1);
    let globals = a.global_symbols();
    assert_eq!(globals.len(), 2);
    for (index, sym) in [global, weak].into_iter().enumerate() {
        assert!(!AssemblerElf::is_local(sym));
        assert_eq!(AssemblerElf::sym_index(sym), index);
        assert!(core::ptr::eq(&globals[index], a.symbol(sym).unwrap()));
    }
    a.reset().unwrap();
    assert_eq!(a.data_sections().count(), 0);
    assert_eq!(a.local_symbol_count(), 1);
    assert!(a.global_symbols().is_empty());
    assert_eq!(a.create_section(SectionKind::Text).unwrap(), expected[0]);
    assert_eq!(a.data_sections().next().unwrap().get_ref(), expected[0]);
}
#[test]
fn symbols_copy_preserves_name_binding_and_visibility() {
    let ctx = MemoryContext::new_bump("asm");
    let mut a = AssemblerElf::new(ctx.mcx(), Architecture::X86_64).unwrap();
    let sec = a.create_section(SectionKind::Text).unwrap();
    let src = a.sym_predef_func(b"source", SymBinding::Local).unwrap();
    let dst = a.sym_predef_data(b"dest\0tail", SymBinding::Weak).unwrap();
    a.sym_def(src, sec, 7, 13).unwrap();
    a.sym_set_visibility(dst, SymVisibility::Hidden).unwrap();
    a.sym_copy(dst, src).unwrap();
    assert_eq!(src.id(), 1);
    assert_eq!(dst.id(), 0x8000_0000);
    assert_eq!(a.sym_name(dst).unwrap(), b"dest");
    assert_eq!(a.sym_section(dst).unwrap(), sec);
    let sym = a.symbol(dst).unwrap();
    assert_eq!((sym.st_value, sym.st_size), (7, 13));
    assert_eq!(
        (sym.st_bind(), sym.st_type(), sym.st_other),
        (STB_WEAK, STT_OBJECT, STV_HIDDEN)
    );
}
#[test]
fn aligned_data_padding_and_defined_allocation() {
    let ctx = MemoryContext::new_bump("asm");
    let mut a = AssemblerElf::new(ctx.mcx(), Architecture::Aarch64).unwrap();
    let sec = a.create_section(SectionKind::Data).unwrap();
    a.get_section_mut(sec).unwrap().write(&0xaau8).unwrap();
    let (sym, off) = a
        .sym_def_data(sec, b"data", &[1, 2, 3], 8, SymBinding::Global)
        .unwrap();
    assert_eq!(off, 8);
    assert_eq!(a.symbol(sym).unwrap().st_size, 3);
    let second = a.sym_predef_data(b"second", SymBinding::Local).unwrap();
    assert_eq!(a.sym_def_predef_data(sec, second, 3, 16).unwrap(), 16);
    let data = &a.get_section(sec).unwrap().data;
    assert_eq!(&data[..8], &[0xaa, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(&data[8..11], &[1, 2, 3]);
    assert!(data[11..].iter().all(|&b| b == 0));
    assert_eq!(a.get_section(sec).unwrap().align, 16);
}
#[test]
fn virtual_zero_data_and_tls() {
    let ctx = MemoryContext::new_bump("asm");
    let mut a = AssemblerElf::new(ctx.mcx(), Architecture::Aarch64).unwrap();
    for kind in [SectionKind::BSS, SectionKind::ThreadBSS] {
        let sec = a.create_section(kind).unwrap();
        let sym = a.sym_predef_tls(b"tls", SymBinding::Global).unwrap();
        assert_eq!(a.sym_def_predef_zero(sec, sym, 37, 16).unwrap(), 0);
        assert_eq!(a.get_section(sec).unwrap().size(), 37);
        assert!(a.get_section(sec).unwrap().data.is_empty());
        assert!(a.get_section_mut(sec).unwrap().write(&1u32).is_err());
    }
    a.build_object_file().unwrap();
}
#[test]
fn group_members_and_structors() {
    let ctx = MemoryContext::new_bump("asm");
    let mut a = AssemblerElf::new(ctx.mcx(), Architecture::X86_64).unwrap();
    let sig = a.sym_add_undef(b"signature", SymBinding::Global).unwrap();
    let group = a.create_group_section(sig, true).unwrap();
    let sec = a.create_structor_section(true, group).unwrap();
    let fini = a.create_structor_section(false, SecRef::default()).unwrap();
    assert_eq!(a.get_section(fini).unwrap().kind, SHT_FINI_ARRAY);
    let data = &a.get_section(group).unwrap().data;
    assert_eq!(data.len(), 12);
    assert_eq!(word(data, 0), GRP_COMDAT);
    assert_eq!(word(data, 4), sec.id());
    assert_eq!(word(data, 8), sec.id() + 1);
    let before = a.build_object_file().unwrap();
    assert!(a.add_to_group(group, sec).is_err());
    assert!(a.add_to_group(group, group).is_err());
    assert_eq!(a.build_object_file().unwrap(), before);
}
#[test]
fn relocation_growth_signed_addends_and_offset_adjustment() {
    let ctx = MemoryContext::new_bump("asm");
    let mut a = AssemblerElf::new(ctx.mcx(), Architecture::Aarch64).unwrap();
    let sec = a.create_section(SectionKind::Text).unwrap();
    let sym = a.sym_add_undef(b"external", SymBinding::Global).unwrap();
    for i in 0..5 {
        a.reloc_pc32(sec, sym, i, -4).unwrap();
    }
    let s = a.get_section_mut(sec).unwrap();
    assert_eq!(s.reloc_count().unwrap(), 5);
    assert_eq!(s.relocs.capacity(), 9);
    s.adjust_relocation_offsets(3, 4);
    assert_eq!(s.relocations()[3].offset, u32::MAX);
    assert_eq!(s.relocations()[4].offset, 0);
    assert_eq!(s.relocations()[0].kind, R_AARCH64_PREL32);
    a.build_object_file().unwrap();
}
#[test]
fn section_inline_bytes_and_move_spill() {
    let ctx = MemoryContext::new_bump("asm");
    let mut a = AssemblerElf::new(ctx.mcx(), Architecture::Aarch64).unwrap();
    let sec = a.create_section(SectionKind::ReadOnly).unwrap();
    a.get_section_mut(sec).unwrap().write(&[3u8; 256]).unwrap();
    assert_eq!(a.get_section(sec).unwrap().data.capacity(), 256);
    a.get_section_mut(sec).unwrap().write(&4u8).unwrap();
    assert_eq!(a.get_section(sec).unwrap().data.capacity(), 513);
    let mut moved = a;
    for _ in 0..20 {
        moved.create_section(SectionKind::ReadOnly).unwrap();
    }
    assert_eq!(moved.get_section(sec).unwrap().data[256], 4);
    assert_eq!(moved.get_section(sec).unwrap().data[0], 3);
}
#[test]
fn custom_names_and_cached_section_symbol() {
    let ctx = MemoryContext::new_bump("asm");
    let mut a = AssemblerElf::new(ctx.mcx(), Architecture::Aarch64).unwrap();
    let sec = a.create_section(SectionKind::Text).unwrap();
    assert_eq!(a.sec_name(sec).unwrap(), b".text");
    a.rename_section(sec, b".custom\0suffix").unwrap();
    assert_eq!(a.sec_name(sec).unwrap(), b".custom");
    let sym = a.section_symbol(sec).unwrap();
    assert_eq!(a.section_symbol(sec).unwrap(), sym);
    assert_eq!(a.sym_name(sym).unwrap(), b"");
    assert!(a.rename_section(sec, b"forbidden").is_err());
    a.build_object_file().unwrap();
}
#[test]
fn reset_clears_extended_rows_and_reuses_identifiers() {
    let ctx = MemoryContext::new_bump("asm");
    let mut a = AssemblerElf::new(ctx.mcx(), Architecture::Aarch64).unwrap();
    let initial = a.build_object_file().unwrap();
    let sec = a.create_section(SectionKind::Text).unwrap();
    a.sym_def_data(sec, b"old", &[7; 8], 4, SymBinding::Local)
        .unwrap();
    a.state.local_shndx.push(0xff00).unwrap();
    a.state.global_shndx.push(0xff01).unwrap();
    a.reset().unwrap();
    assert_eq!(a.build_object_file().unwrap(), initial);
    assert_eq!(a.create_section(SectionKind::Text).unwrap(), sec);
    assert_eq!(
        a.sym_predef_func(b"new", SymBinding::Local).unwrap().id(),
        1
    );
}
#[test]
fn invalid_operations_preserve_object_and_allow_reuse() {
    let ctx = MemoryContext::new_bump("asm");
    let mut a = AssemblerElf::new(ctx.mcx(), Architecture::Aarch64).unwrap();
    let sec = a.create_section(SectionKind::Data).unwrap();
    let sym = a.sym_predef_data(b"pending", SymBinding::Global).unwrap();
    let before = a.build_object_file().unwrap();
    assert!(a.sym_def_predef_data(sec, sym, 7, 3).is_err());
    assert!(a.sym_def_predef_data(sec, sym, u64::MAX, 8).is_err());
    assert!(a.reloc_abs(sec, sym, 0, i64::MAX).is_err());
    assert!(a.sym_def(sym, SecRef::new(0), 0, 0).is_err());
    assert!(a.symbol(SymRef::default()).is_err());
    assert_eq!(a.build_object_file().unwrap(), before);
    a.sym_def_predef_bytes(sec, sym, &[1, 2], 1).unwrap();
    assert!(a.sym_def(sym, sec, 0, 2).is_err());
    assert_eq!(a.get_section(sec).unwrap().size(), 2);
}
#[test]
fn absolute_symbols_preserve_metadata_copy_and_atomic_refusal() {
    let ctx = MemoryContext::new_bump("absolute");
    let mut a = AssemblerElf::new(ctx.mcx(), Architecture::Aarch64).unwrap();
    for binding in [SymBinding::Local, SymBinding::Global, SymBinding::Weak] {
        let sym = a.sym_predef_data(b"absolute", binding).unwrap();
        a.sym_set_visibility(sym, SymVisibility::Hidden).unwrap();
        let before = *a.symbol(sym).unwrap();
        a.sym_def_absolute(sym, u64::MAX, 0).unwrap();
        let defined = *a.symbol(sym).unwrap();
        assert_eq!(defined.st_name, before.st_name);
        assert_eq!(defined.st_info, before.st_info);
        assert_eq!(defined.st_other, before.st_other);
        assert_eq!(defined.st_shndx, SHN_ABS);
        assert_eq!(defined.st_value, u64::MAX);
        assert_eq!(defined.st_size, 0);
        assert!(a.sym_section(sym).is_err());
        let object = a.build_object_file().unwrap();
        assert!(a.sym_def_absolute(sym, 0, 7).is_err());
        assert!(a.sym_def_absolute(SymRef::default(), 0, 7).is_err());
        assert!(a.sym_def_absolute(SymRef::new(0x7fff_ffff), 0, 7).is_err());
        assert_eq!(a.build_object_file().unwrap(), object);
        let copy = a.sym_predef_func(b"copy", SymBinding::Global).unwrap();
        let copy_info = a.symbol(copy).unwrap().st_info;
        a.sym_copy(copy, sym).unwrap();
        assert_eq!(a.symbol(copy).unwrap().st_shndx, SHN_ABS);
        assert_eq!(a.symbol(copy).unwrap().st_value, u64::MAX);
        assert_eq!(a.symbol(copy).unwrap().st_info, copy_info);
        let zero = a.sym_add_undef(b"zero", binding).unwrap();
        a.sym_def_absolute(zero, 0, u64::MAX).unwrap();
        assert_eq!(a.symbol(zero).unwrap().st_value, 0);
        assert_eq!(a.symbol(zero).unwrap().st_size, u64::MAX);
    }
    a.reset().unwrap();
    let fresh = a.sym_predef_data(b"fresh", SymBinding::Local).unwrap();
    assert_eq!(fresh.id(), 1);
    a.sym_def_absolute(fresh, 7, 3).unwrap();
    assert_eq!(a.symbol(fresh).unwrap().st_value, 7);
}
#[test]
fn allocation_failure_is_atomic_and_fitting_append_succeeds() {
    let ctx = MemoryContext::new("limited").with_limit(8192);
    let mut a = AssemblerElf::new(ctx.mcx(), Architecture::Aarch64).unwrap();
    let sec = a.create_section(SectionKind::Data).unwrap();
    let sym = a.sym_predef_data(b"pending", SymBinding::Local).unwrap();
    let before = a.build_object_file().unwrap();
    assert!(a.reserve_relocations(sec, usize::MAX).is_err());
    assert!(a.reserve_relocations(sec, 1000).is_err());
    assert_eq!(a.build_object_file().unwrap(), before);
    a.reserve_relocations(sec, 4).unwrap();
    assert_eq!(a.get_section(sec).unwrap().reloc_count().unwrap(), 0);
    assert!(a.sym_def_predef_data(sec, sym, 9000, 1).is_err());
    assert_eq!(a.symbol(sym).unwrap().st_shndx, SHN_UNDEF);
    assert_eq!(a.get_section(sec).unwrap().size(), 0);
    assert_eq!(a.build_object_file().unwrap(), before);
    a.sym_def_predef_bytes(sec, sym, &[4, 5], 1).unwrap();
    assert_eq!(&*a.get_section(sec).unwrap().data, &[4, 5]);
}
#[test]
fn elf_bit_packing_and_layouts() {
    for info in 0..=255u8 {
        let sym = Elf64_Sym {
            st_info: info,
            ..Elf64_Sym::default()
        };
        assert_eq!(
            st_info(sym.st_bind() as u32, sym.st_type() as u32),
            info as u32
        );
    }
    assert_eq!(r_info(u64::MAX, u32::MAX), u64::MAX);
    assert_eq!(core::mem::offset_of!(Elf64_Sym, st_value), 8);
    let sym = Elf64_Sym {
        st_name: 0x12345678,
        st_info: 0xfe,
        st_other: 2,
        st_shndx: 0x9876,
        st_value: 0xabcdef,
        st_size: 9,
    };
    assert_eq!(
        &sym.to_le_bytes()[..8],
        &[0x78, 0x56, 0x34, 0x12, 0xfe, 2, 0x76, 0x98]
    );
}
