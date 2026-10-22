// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use super::*;
use mcx::MemoryContext;
use std::{vec, vec::Vec};
use tpde_assembler::{elf, Architecture, AssemblerElf, SectionKind, SymBinding, SymRef};
use tpde_core::Reg;
use tpde_util::dwarf;

#[derive(Default)]
struct RelativeTarget {
    grows: usize,
    aligns: usize,
    finishes: usize,
}
impl FunctionWriterTarget for RelativeTarget {
    const CIE: TargetCieInfo<'static> = TargetCieInfo {
        instrs: &[dwarf::DW_CFA_def_cfa, 7, 8],
        return_addr_register: 16,
        code_alignment_factor: 1,
        data_alignment_factor: 0x78,
    };
    fn handle_fixups(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
    ) -> types_error::PgResult<()> {
        for index in 0..base.label_fixups().len() {
            let fixup = base.label_fixups()[index];
            let off = fixup.off - base.label_skew();
            let dest = base.label_offset(fixup.label)?;
            let displacement = i32::try_from(i64::from(dest) - i64::from(off) - 4).unwrap();
            base.data_mut()?[off as usize..off as usize + 4]
                .copy_from_slice(&displacement.to_le_bytes());
        }
        self.finishes += 1;
        Ok(())
    }
    fn more_space(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        size: usize,
    ) -> types_error::PgResult<()> {
        self.grows += 1;
        base.more_space(size)
    }
    fn align(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        align: usize,
    ) -> types_error::PgResult<()> {
        self.aligns += 1;
        base.align(align)
    }
}

#[derive(Default)]
struct CappedTarget<const TABLE_BYTES: usize> {
    unresolved: [u32; 2],
    growth_unresolved: [u32; 2],
    begins: usize,
    grows: usize,
}
impl<const TABLE_BYTES: usize> FunctionWriterTarget for CappedTarget<TABLE_BYTES> {
    const CIE: TargetCieInfo<'static> = TargetCieInfo {
        code_alignment_factor: 4,
        ..RelativeTarget::CIE
    };
    fn before_begin_func(&mut self) {
        self.unresolved = [0; 2];
        self.begins += 1;
    }
    fn handle_fixups(
        &mut self,
        _base: &mut FunctionWriterBase<'_, '_, '_>,
    ) -> types_error::PgResult<()> {
        Ok(())
    }
    fn more_space(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        size: usize,
    ) -> types_error::PgResult<()> {
        self.grows += 1;
        self.growth_unresolved = self.unresolved;
        let prefix = if self.unresolved == [0; 2] { 0 } else { 8 };
        base.more_space(size + prefix)?;
        if prefix != 0 {
            base.write_bytes_unchecked(&[0xe0; 8])?;
            self.unresolved = [0; 2];
        }
        base.cap_reserved_end(base.allocated_size().min(base.offset() + 16384))
    }
    fn ensure_space(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        size: usize,
    ) -> types_error::PgResult<()> {
        if size > 16384 {
            return Err(types_error::PgError::error("advance exceeds target limit").into());
        }
        base.ensure_space_with(self, size)
    }
    fn label_ref(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        label: Label,
        off: u32,
        kind: LabelFixupKind,
    ) -> types_error::PgResult<()> {
        base.label_ref(label, off, kind)?;
        if kind == LabelFixupKind::AARCH64_COND_BR {
            self.unresolved[0] += 1;
        } else if kind == LabelFixupKind::AARCH64_TEST_BR {
            self.unresolved[1] += 1;
        }
        Ok(())
    }
    fn eh_advance(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        size: u64,
    ) -> types_error::PgResult<()> {
        base.eh_advance_raw(size / 4)
    }
}
impl<const TABLE_BYTES: usize> FunctionWriterJumpTables for CappedTarget<TABLE_BYTES> {
    fn create_jump_table(
        &mut self,
        base: &mut FunctionWriterBase<'_, '_, '_>,
        size: u32,
        index: Reg,
        temporary: Reg,
        index_is_32: bool,
        fill: impl FnOnce(&mut [Label]),
    ) -> types_error::PgResult<()> {
        let table_index = base.jump_table_count();
        base.alloc_jump_table(size, index, temporary)?
            .set_misc(if TABLE_BYTES == 24 {
                u8::from(index_is_32)
            } else {
                0
            });
        self.ensure_space(base, TABLE_BYTES)?;
        base.skip_unchecked(TABLE_BYTES)?;
        fill(base.jump_table_mut(table_index)?.labels_mut());
        Ok(())
    }
}
macro_rules! fixture {
    ($module:ident, $writer:ident, $assembler:ident, $context:ident) => {
        let $context = MemoryContext::new("function-writer");
        let mut $assembler = AssemblerElf::new($context.mcx(), Architecture::X86_64).unwrap();
        let text = $assembler.get_default_section(SectionKind::Text).unwrap();
        let mut $writer = FunctionWriter::new($context.mcx(), RelativeTarget::default());
        let mut $module = $writer.begin_module(&mut $assembler).unwrap();
        $module.switch_section(text).unwrap();
        $module.begin_func(1, 8).unwrap();
    };
}
fn word(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn uleb(bytes: &[u8], cursor: &mut usize) -> u64 {
    let mut value = 0;
    let mut shift = 0;
    loop {
        let byte = bytes[*cursor];
        *cursor += 1;
        value |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return value;
        }
        shift += 7;
    }
}
fn call_sites(bytes: &[u8]) -> Vec<[u64; 4]> {
    assert_eq!(bytes[0], dwarf::DW_EH_PE_omit);
    let mut cursor = 2;
    if bytes[1] != dwarf::DW_EH_PE_omit {
        uleb(bytes, &mut cursor);
    }
    assert_eq!(bytes[cursor], dwarf::DW_EH_PE_uleb128);
    cursor += 1;
    let length = uleb(bytes, &mut cursor) as usize;
    let end = cursor + length;
    let mut sites = Vec::new();
    while cursor + 2 < end {
        sites.push([
            uleb(bytes, &mut cursor),
            uleb(bytes, &mut cursor),
            uleb(bytes, &mut cursor),
            uleb(bytes, &mut cursor),
        ]);
    }
    assert_eq!(&bytes[cursor..end], &[0, 0]);
    sites
}

#[test]
fn relative_target_resolves_forward_backward_and_prologue_fixups() {
    fixture!(module, writer, assembler, context);
    module.write_bytes(&[0xaa; 4]).unwrap();
    let start = module.label_create().unwrap();
    module.label_place(start, 4).unwrap();
    let end = module.label_create().unwrap();
    module
        .label_ref(end, 4, LabelFixupKind::X64_JMP_OR_MEM_DISP)
        .unwrap();
    module.write(0u32).unwrap();
    module
        .label_ref(start, 8, LabelFixupKind::X64_JMP_OR_MEM_DISP)
        .unwrap();
    module.write(0u32).unwrap();
    module.label_place(end, 12).unwrap();
    module.remove_prologue_bytes(0, 4).unwrap();
    module.finish_func().unwrap();
    assert_eq!(module.data().unwrap(), &[4, 0, 0, 0, 248, 255, 255, 255]);
    assert_eq!(module.target().finishes, 1);
    assert_eq!(module.label_offset(start).unwrap(), 0);
    assert_eq!(module.label_offset(end).unwrap(), 8);
}

#[test]
fn function_alignment_zeroes_padding_and_calls_target() {
    fixture!(module, writer, assembler, context);
    module.write(0xabu8).unwrap();
    module.begin_func(16, 32).unwrap();
    assert_eq!(module.func_begin(), 16);
    assert_eq!(module.offset(), 16);
    assert_eq!(
        module.data().unwrap(),
        &[0xab, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(module.get_section().unwrap().align, 16);
    assert_eq!(module.target().aligns, 2);
    assert!(module.begin_func(3, 8).is_err());
    assert!(module.align(0).is_err());
    assert_eq!(module.offset(), 16);
}

#[test]
fn text_capacity_and_growth_are_separate_from_initialized_bytes() {
    fixture!(module, writer, assembler, context);
    assert_eq!(module.allocated_size(), 256);
    assert_eq!(module.data().unwrap(), &[]);
    assert_eq!(module.growth_size(), 8);
    module.write_bytes(&[7; 255]).unwrap();
    let grows = module.target().grows;
    module.ensure_space(2).unwrap();
    assert_eq!(module.target().grows, grows + 1);
    assert_eq!(module.growth_size(), 12);
    assert_eq!(module.offset(), 255);
    assert_eq!(module.data().unwrap(), &[7; 255]);
    module.write(0x1234u16).unwrap();
    assert_eq!(&module.data().unwrap()[255..], &[0x34, 0x12]);
}

#[test]
fn capped_reservation_rediscovers_retained_capacity() {
    fixture!(module, writer, assembler, context);
    module.write_bytes(&[1, 2, 3, 4]).unwrap();
    module.cap_reserved_end(4).unwrap();
    assert!(module.write_unchecked(5u8).is_err());
    module.ensure_space(1).unwrap();
    assert_eq!(module.allocated_size(), 256);
    module.write_unchecked(5u8).unwrap();
    assert_eq!(module.data().unwrap(), &[1, 2, 3, 4, 5]);
}

#[test]
fn target_reset_precedes_hint_growth_without_relaxing_actual_reservations() {
    let context = MemoryContext::new("capped-writer");
    let mut assembler = AssemblerElf::new(context.mcx(), Architecture::Aarch64).unwrap();
    let text = assembler.get_default_section(SectionKind::Text).unwrap();
    let mut writer = FunctionWriter::new(context.mcx(), CappedTarget::<24>::default());
    let mut module = writer.begin_module(&mut assembler).unwrap();
    module.switch_section(text).unwrap();
    assert!(module.begin_func(3, 8).is_err());
    assert_eq!(module.target().begins, 0);
    module.begin_func(4, 0x20000).unwrap();
    assert_eq!(module.allocated_size(), 16384);
    assert_eq!(module.offset(), 0);
    let grows = module.target().grows;
    assert!(module.skip(16385).is_err());
    assert!(module.reserve_uninitialized(16385).is_err());
    assert_eq!(module.target().grows, grows);
    assert_eq!(module.data().unwrap(), &[]);
    module.write(0xaau32).unwrap();
    let label = module.label_create().unwrap();
    module
        .label_ref(label, 0, LabelFixupKind::AARCH64_TEST_BR)
        .unwrap();
    let end = module.offset();
    module.cap_reserved_end(end).unwrap();
    module.begin_func(4, 0x20000).unwrap();
    assert_eq!(module.target().begins, 2);
    assert_eq!(module.target().grows, grows + 1);
    assert_eq!(module.target().growth_unresolved, [0; 2]);
    assert_eq!(module.func_begin(), 4);
    assert_eq!(module.data().unwrap(), &0xaau32.to_ne_bytes());
    assert_eq!(module.label_count(), 0);
}

#[test]
fn failed_target_label_reference_preserves_unresolved_counts_and_fixups() {
    let context = MemoryContext::new("target-labels");
    let mut assembler = AssemblerElf::new(context.mcx(), Architecture::Aarch64).unwrap();
    let text = assembler.get_default_section(SectionKind::Text).unwrap();
    let mut writer = FunctionWriter::new(context.mcx(), CappedTarget::<24>::default());
    let mut module = writer.begin_module(&mut assembler).unwrap();
    module.switch_section(text).unwrap();
    module.begin_func(4, 8).unwrap();
    let label = module.label_create().unwrap();
    module
        .label_ref(label, 0, LabelFixupKind::AARCH64_COND_BR)
        .unwrap();
    assert!(module
        .label_ref(Label(1), 0, LabelFixupKind::AARCH64_TEST_BR)
        .is_err());
    assert_eq!(module.target().unresolved, [1, 0]);
    assert_eq!(module.label_fixups().len(), 1);
    module
        .label_ref(label, 0, LabelFixupKind::AARCH64_TEST_BR)
        .unwrap();
    module
        .label_ref(label, 0, LabelFixupKind::AARCH64_BR)
        .unwrap();
    assert_eq!(module.target().unresolved, [1, 1]);
    assert_eq!(module.label_fixups().len(), 3);
}

#[test]
fn target_cfi_distance_scaling_preserves_byte_and_raw_entry_points() {
    let context = MemoryContext::new("target-cfi");
    let mut assembler = AssemblerElf::new(context.mcx(), Architecture::Aarch64).unwrap();
    let mut writer = FunctionWriter::new(context.mcx(), CappedTarget::<24>::default());
    let mut module = writer.begin_module(&mut assembler).unwrap();
    let start = module.eh_data().unwrap().len();
    for distance in [3, 255, 256, 1023, 1024, 262144] {
        module.eh_advance(distance).unwrap();
    }
    module.base.eh_advance(64).unwrap();
    assert_eq!(
        &module.eh_data().unwrap()[start..],
        &[0x40, 0x7f, 2, 64, 2, 255, 3, 0, 1, 4, 0, 0, 1, 0, 2, 64]
    );
    let end = module.eh_data().unwrap().len();
    assert!(module.eh_advance((u64::from(u32::MAX) + 1) * 4).is_err());
    assert_eq!(module.eh_data().unwrap().len(), end);
}

#[test]
fn target_jump_table_metadata_and_fill_survive_growth() {
    fn check<const TABLE_BYTES: usize>() {
        let context = MemoryContext::new("target-tables");
        let mut assembler = AssemblerElf::new(context.mcx(), Architecture::Aarch64).unwrap();
        let text = assembler.get_default_section(SectionKind::Text).unwrap();
        let mut writer = FunctionWriter::new(context.mcx(), CappedTarget::<TABLE_BYTES>::default());
        let mut module = writer.begin_module(&mut assembler).unwrap();
        module.switch_section(text).unwrap();
        module.begin_func(4, 8).unwrap();
        module.write(0xaau32).unwrap();
        let first = module.label_create().unwrap();
        let second = module.label_create().unwrap();
        module
            .label_ref(first, 0, LabelFixupKind::AARCH64_TEST_BR)
            .unwrap();
        let before = module.offset();
        module.cap_reserved_end(before).unwrap();
        let grows = module.target().grows;
        for index_is_32 in [true, false] {
            let table_index = module.jump_table_count();
            let off = module.offset();
            module
                .create_jump_table(129, Reg::new(3), Reg::new(7), index_is_32, |labels| {
                    for (i, label) in labels.iter_mut().enumerate() {
                        *label = if i % 2 == 0 { first } else { second };
                    }
                })
                .unwrap();
            let header = module.jump_table_header(table_index).unwrap();
            assert_eq!((header.off, header.size()), (off as u32, 129));
            assert_eq!((header.idx, header.tmp), (Reg::new(3), Reg::new(7)));
            assert_eq!(header.misc, u8::from(TABLE_BYTES == 24 && index_is_32));
            assert!(module
                .jump_table_labels(table_index)
                .unwrap()
                .iter()
                .enumerate()
                .all(|(i, &label)| label == if i % 2 == 0 { first } else { second }));
        }
        assert_eq!(module.target().grows, grows + 1);
        assert_eq!(module.offset(), before + 8 + 2 * TABLE_BYTES);
        assert_eq!(&module.data().unwrap()[before..before + 8], &[0xe0; 8]);
        assert!(module.data().unwrap()[before + 8..]
            .iter()
            .all(|&byte| byte == 0));
    }
    check::<18>();
    check::<24>();
}

#[test]
fn invalid_cursor_reservation_and_size_leave_text_unchanged() {
    fixture!(module, writer, assembler, context);
    module.write_bytes(&[1, 2, 3]).unwrap();
    assert!(module.set_offset(4).is_err());
    assert!(module.cap_reserved_end(2).is_err());
    assert!(module.cap_reserved_end(257).is_err());
    assert!(module.ensure_space(usize::MAX).is_err());
    assert!(module.skip(usize::MAX).is_err());
    assert_eq!(module.offset(), 3);
    assert_eq!(module.allocated_size(), 256);
    assert_eq!(module.data().unwrap(), &[1, 2, 3]);
    module.set_offset(1).unwrap();
    module.write(9u8).unwrap();
    module.flush().unwrap();
    assert_eq!(&module.get_section().unwrap().data[..], &[1, 9]);
}

#[test]
fn section_switch_requires_flush_and_rejects_virtual_sections() {
    fixture!(module, writer, assembler, context);
    let text = module.get_sec_ref();
    let data = module
        .assembler_mut()
        .get_default_section(SectionKind::Data)
        .unwrap();
    let bss = module
        .assembler_mut()
        .get_default_section(SectionKind::BSS)
        .unwrap();
    module.write(17u8).unwrap();
    assert!(module.switch_section(data).is_err());
    module.flush().unwrap();
    assert!(module.switch_section(bss).is_err());
    assert_eq!(module.get_sec_ref(), text);
    module.switch_section(data).unwrap();
    module.begin_func(4, 4).unwrap();
    module.write(23u8).unwrap();
    module.flush().unwrap();
    module.switch_section(text).unwrap();
    assert_eq!(module.offset(), 1);
    assert_eq!(module.data().unwrap(), &[17]);
}

#[test]
fn module_move_keeps_inline_text_and_eh_cursors_valid() {
    fixture!(module, writer, assembler, context);
    module.write(1u8).unwrap();
    let mut moved = module;
    moved.write(0x040302u32).unwrap();
    moved.eh_write_inst(dwarf::DW_CFA_nop).unwrap();
    assert_eq!(moved.data().unwrap(), &[1, 2, 3, 4, 0]);
    moved.flush().unwrap();
    moved.eh_align_frame().unwrap();
    moved.end_module().unwrap();
}

#[test]
fn relocation_errors_do_not_add_records() {
    fixture!(module, writer, assembler, context);
    let symbol = module
        .assembler_mut()
        .sym_add_undef(b"symbol", SymBinding::Global)
        .unwrap();
    module.reloc(symbol, elf::R_X86_64_PC32, 1, -4).unwrap();
    assert!(module
        .reloc(symbol, elf::R_X86_64_PC32, u32::MAX as u64 + 1, 0)
        .is_err());
    assert!(module
        .reloc(SymRef::new(0x1234), elf::R_X86_64_PC32, 0, 0)
        .is_err());
    assert_eq!(module.get_section().unwrap().relocations().len(), 1);
    assert_eq!(module.get_section().unwrap().relocations()[0].addend, -4);
}

#[test]
fn prologue_removal_adjusts_only_current_function_relocations() {
    fixture!(module, writer, assembler, context);
    let symbol = module
        .assembler_mut()
        .sym_add_undef(b"callee", SymBinding::Global)
        .unwrap();
    module.write_bytes(&[0; 8]).unwrap();
    module.reloc(symbol, elf::R_X86_64_PC32, 4, -4).unwrap();
    module.begin_func(1, 8).unwrap();
    module.write_bytes(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    module.reloc(symbol, elf::R_X86_64_PC32, 14, 7).unwrap();
    module.remove_prologue_bytes(9, 2).unwrap();
    assert_eq!(&module.data().unwrap()[8..], &[1, 4, 5, 6, 7, 8]);
    let relocs = module.get_section().unwrap().relocations();
    assert_eq!((relocs[0].offset, relocs[1].offset), (4, 12));
    assert_eq!((relocs[0].addend, relocs[1].addend), (-4, 7));
}

#[test]
fn invalid_prologue_ranges_preserve_labels_and_bytes() {
    fixture!(module, writer, assembler, context);
    module.write_bytes(&[1, 2, 3, 4]).unwrap();
    let label = module.label_create().unwrap();
    module.label_place(label, 2).unwrap();
    assert!(module.remove_prologue_bytes(0, 3).is_err());
    assert!(module.remove_prologue_bytes(0, 5).is_err());
    assert!(module.remove_prologue_bytes(u32::MAX, 1).is_err());
    assert_eq!(module.data().unwrap(), &[1, 2, 3, 4]);
    module.remove_prologue_bytes(0, 0).unwrap();
    module.remove_prologue_bytes(0, 1).unwrap();
    assert!(module.remove_prologue_bytes(0, 1).is_err());
    assert_eq!(module.label_offset(label).unwrap(), 1);
    let pending = module.label_create().unwrap();
    assert!(module.label_place(pending, 2).is_err());
}

#[test]
fn labels_reject_pending_duplicate_and_unknown_then_reset() {
    fixture!(module, writer, assembler, context);
    let label = module.label_create().unwrap();
    assert_eq!(label, Label(0));
    assert!(module.label_is_pending(label).unwrap());
    assert!(module.label_offset(label).is_err());
    assert!(module.label_is_pending(Label(1)).is_err());
    assert!(module
        .label_ref(Label(1), 0, LabelFixupKind::ARCH_KIND_BEGIN)
        .is_err());
    module.label_place(label, 0).unwrap();
    assert!(module.label_place(label, 1).is_err());
    module
        .label_ref(label, 0, LabelFixupKind::ARCH_KIND_BEGIN)
        .unwrap();
    module.begin_func(1, 8).unwrap();
    assert!(module.label_fixups().is_empty());
    assert!(module.label_is_pending(label).is_err());
    assert_eq!(module.label_create().unwrap(), Label(0));
}

#[test]
fn pending_fixup_failure_can_be_placed_and_retried() {
    fixture!(module, writer, assembler, context);
    let label = module.label_create().unwrap();
    module
        .label_ref(label, 0, LabelFixupKind::ARCH_KIND_BEGIN)
        .unwrap();
    module.write(0u32).unwrap();
    assert!(module.finish_func().is_err());
    assert_eq!(module.data().unwrap(), &[0; 4]);
    module.label_place(label, 4).unwrap();
    module.finish_func().unwrap();
    assert_eq!(module.target().finishes, 1);
}

#[test]
fn label_and_fixup_spills_keep_all_indices() {
    fixture!(module, writer, assembler, context);
    for index in 0..83 {
        let label = module.label_create().unwrap();
        assert_eq!(label, Label(index));
        module.label_place(label, index * 4).unwrap();
        module
            .label_ref(label, index * 4, LabelFixupKind::ARCH_KIND_BEGIN)
            .unwrap();
        module.write(0u32).unwrap();
    }
    module.finish_func().unwrap();
    assert_eq!(module.label_fixups().len(), 83);
    for (index, bytes) in module.data().unwrap().chunks_exact(4).enumerate() {
        assert_eq!(bytes, (-4i32).to_le_bytes(), "relative fixup {index}");
    }
    module.label_fixups_mut()[0].kind = LabelFixupKind::AARCH64_BR;
}

#[test]
fn zero_entry_jump_table_has_live_header_and_empty_tail() {
    fixture!(module, writer, assembler, context);
    let mut table = module
        .alloc_jump_table(0, Reg::new(2), Reg::new(3))
        .unwrap();
    assert_eq!(table.size(), 0);
    assert!(table.labels().is_empty());
    table.set_misc(7);
    drop(table);
    assert!(module.jump_table_labels(0).unwrap().is_empty());
    assert_eq!(module.jump_table_header(0).unwrap().misc, 7);
    assert!(module.jump_table_labels(1).is_err());
    assert!(module.jump_table_mut(1).is_err());
}

#[test]
fn jump_tables_survive_spills_shared_borrows_and_writer_moves() {
    fixture!(module, writer, assembler, context);
    let mut addresses = Vec::new();
    for table_index in 0..37 {
        module.write(table_index as u8).unwrap();
        let mut table = module
            .alloc_jump_table(40, Reg::new(table_index), Reg::new(17))
            .unwrap();
        addresses.push(table.labels().as_ptr());
        for (index, label) in table.labels_mut().iter_mut().enumerate() {
            *label = Label((table_index as u32) * 1000 + index as u32);
        }
        table.set_misc(table_index as u8);
        table.set_off(40 + table_index as u32);
        table.set_idx(Reg::new(22));
        table.set_tmp(Reg::new(23));
    }
    for (index, &address) in addresses.iter().enumerate() {
        assert_eq!(module.jump_table_labels(index).unwrap().as_ptr(), address);
        assert_eq!(
            module.jump_table_labels(index).unwrap()[39],
            Label(index as u32 * 1000 + 39)
        );
    }
    module.jump_table_mut(3).unwrap().labels_mut()[39] = Label(777);
    assert_eq!(module.jump_table_labels(3).unwrap()[39], Label(777));
    module.flush().unwrap();
    module.end_module().unwrap();
    drop(module);
    let mut moved = writer;
    let mut module = moved.begin_module(&mut assembler).unwrap();
    assert_eq!(module.jump_table_count(), 37);
    assert_eq!(module.jump_table_labels(3).unwrap()[39], Label(777));
    assert_eq!(module.jump_table_header(8).unwrap().off, 48);
    assert_eq!(module.jump_table_header(8).unwrap().idx, Reg::new(22));
    assert_eq!(module.jump_table_header(8).unwrap().tmp, Reg::new(23));
    assert_eq!(module.jump_table_header(8).unwrap().misc, 8);
    module.jump_table_mut(0).unwrap().labels_mut()[0] = Label(55);
    assert_eq!(module.jump_table_labels(0).unwrap()[0], Label(55));
}

#[test]
fn new_functions_clear_jump_tables_and_reuse_slab_storage() {
    fixture!(module, writer, assembler, context);
    let first_address = {
        let mut table = module
            .alloc_jump_table(17, Reg::new(0), Reg::new(1))
            .unwrap();
        table.labels_mut().fill(Label(99));
        table.labels().as_ptr()
    };
    for epoch in 0..4 {
        module.begin_func(1, 8).unwrap();
        assert_eq!(module.jump_table_count(), 0);
        assert!(module.jump_table_header(0).is_err());
        let mut table = module
            .alloc_jump_table(17, Reg::new(0), Reg::new(1))
            .unwrap();
        assert_eq!(table.labels().as_ptr(), first_address, "slab reuse {epoch}");
        assert_eq!(table.labels(), &[Label(0); 17]);
        table.labels_mut().fill(Label(epoch));
    }
}

#[test]
fn failed_large_jump_table_keeps_previous_tables_usable() {
    let context = MemoryContext::new("limited-writer").with_limit(128 * 1024);
    let mut assembler = AssemblerElf::new(context.mcx(), Architecture::X86_64).unwrap();
    let text = assembler.get_default_section(SectionKind::Text).unwrap();
    let mut writer = FunctionWriter::new(context.mcx(), RelativeTarget::default());
    let mut module = writer.begin_module(&mut assembler).unwrap();
    module.switch_section(text).unwrap();
    module.begin_func(1, 8).unwrap();
    module
        .alloc_jump_table(2, Reg::new(0), Reg::new(1))
        .unwrap()
        .labels_mut()
        .copy_from_slice(&[Label(3), Label(4)]);
    assert!(module
        .alloc_jump_table(1 << 20, Reg::new(0), Reg::new(1))
        .is_err());
    assert_eq!(module.jump_table_count(), 1);
    assert_eq!(module.jump_table_labels(0).unwrap(), &[Label(3), Label(4)]);
    assert!(module.ensure_space(1 << 20).is_err());
    module.write(0xaau8).unwrap();
    assert_eq!(module.data().unwrap(), &[0xaa]);
}

#[test]
fn assembler_and_writer_can_be_reused_across_empty_modules() {
    let context = MemoryContext::new("reused-writer");
    let mut assembler = AssemblerElf::new(context.mcx(), Architecture::X86_64).unwrap();
    let mut writer = FunctionWriter::new(context.mcx(), RelativeTarget::default());
    let mut expected = None;
    for _ in 0..3 {
        assembler.reset().unwrap();
        let mut module = writer.begin_module(&mut assembler).unwrap();
        module.end_module().unwrap();
        assert_eq!(module.eh_data().unwrap().len(), 24);
        assert_eq!(word(module.eh_data().unwrap(), 0), 20);
        drop(module);
        let object = assembler.build_object_file().unwrap();
        if let Some(ref bytes) = expected {
            assert_eq!(&object, bytes);
        } else {
            expected = Some(object);
        }
    }
}

#[test]
fn primary_cfi_operands_and_short_buffers_fail_before_writing() {
    let mut bytes = [0xa5; 21];
    assert!(FunctionWriterBase::write_eh_inst1(&mut [], dwarf::DW_CFA_offset, 1).is_err());
    assert!(FunctionWriterBase::write_eh_inst1(&mut bytes, dwarf::DW_CFA_offset, 64).is_err());
    assert_eq!(bytes, [0xa5; 21]);
    assert!(
        FunctionWriterBase::write_eh_inst1(&mut bytes[..1], dwarf::DW_CFA_def_cfa, 128).is_err()
    );
    assert_eq!(bytes, [0xa5; 21]);
    assert!(
        FunctionWriterBase::write_eh_inst2(&mut bytes[..2], dwarf::DW_CFA_def_cfa, 128, 128)
            .is_err()
    );
    assert_eq!(bytes, [0xa5; 21]);
    assert_eq!(
        FunctionWriterBase::write_eh_inst1(&mut bytes, dwarf::DW_CFA_offset, 63).unwrap(),
        1
    );
    assert_eq!(bytes[0], 0xbf);
}

#[test]
fn cfi_two_operands_preserve_full_width_uleb_values() {
    let mut bytes = [0; 21];
    let len =
        FunctionWriterBase::write_eh_inst2(&mut bytes, dwarf::DW_CFA_def_cfa, u64::MAX, u64::MAX)
            .unwrap();
    assert_eq!(len, 21);
    assert_eq!(bytes[0], dwarf::DW_CFA_def_cfa);
    let mut cursor = 1;
    assert_eq!(uleb(&bytes, &mut cursor), u64::MAX);
    assert_eq!(uleb(&bytes, &mut cursor), u64::MAX);
    assert_eq!(cursor, len);
}

#[test]
fn cfi_advance_selects_each_encoding_boundary() {
    fixture!(module, writer, assembler, context);
    let begin = module.eh_size();
    for advance in [0, 63, 64, 255, 256, 65535, 65536, u32::MAX as u64] {
        module.eh_advance_raw(advance).unwrap();
    }
    assert_eq!(
        &module.eh_data().unwrap()[begin..],
        &[0x40, 0x7f, 2, 64, 2, 255, 3, 0, 1, 3, 255, 255, 4, 0, 0, 1, 0, 4, 255, 255, 255, 255,]
    );
    let end = module.eh_size();
    assert!(module.eh_advance(u32::MAX as u64 + 1).is_err());
    assert_eq!(module.eh_size(), end);
}

#[test]
fn eh_guard_commits_its_cursor_and_alignment_pads_only_live_bytes() {
    fixture!(module, writer, assembler, context);
    let begin = module.eh_size();
    {
        let mut eh = module.eh_writer().unwrap();
        eh.write(&[9, 8, 7]).unwrap();
        eh.unskip(1).unwrap();
    }
    assert_eq!(module.eh_size(), begin + 2);
    module.eh_align_frame().unwrap();
    assert_eq!(&module.eh_data().unwrap()[begin..begin + 4], &[9, 8, 0, 0]);
    module.eh_write_inst1(dwarf::DW_CFA_offset, 2).unwrap();
    module
        .eh_write_inst2(dwarf::DW_CFA_def_cfa, 7, 128)
        .unwrap();
    assert_eq!(
        &module.eh_data().unwrap()[begin + 4..],
        &[0x82, 0x0c, 7, 0x80, 1]
    );
}

#[test]
fn fde_tracks_function_size_and_section_relative_start() {
    fixture!(module, writer, assembler, context);
    module.write_bytes(&[0; 3]).unwrap();
    module.begin_func(8, 8).unwrap();
    let start = module.eh_size();
    assert!(module.eh_end_fde().is_err());
    module.eh_begin_fde(SymRef::default()).unwrap();
    assert!(module.eh_begin_fde(SymRef::default()).is_err());
    module.write_bytes(&[0x90; 5]).unwrap();
    module.eh_end_fde().unwrap();
    assert!(module.eh_end_fde().is_err());
    assert_eq!(word(module.eh_data().unwrap(), start + 12), 5);
    let eh = module
        .assembler_mut()
        .get_default_section(SectionKind::EHFrame)
        .unwrap();
    let relocs = module.assembler().get_section(eh).unwrap().relocations();
    assert_eq!(relocs.len(), 1);
    assert_eq!(relocs[0].offset, start as u32 + 8);
    assert_eq!(relocs[0].addend, 8);
    assert_eq!(relocs[0].kind, elf::R_X86_64_PC32);
}

#[test]
fn personality_transitions_create_cies_only_when_changed() {
    fixture!(module, writer, assembler, context);
    let personality = module
        .assembler_mut()
        .sym_add_undef(b"personality", SymBinding::Global)
        .unwrap();
    module.write(0x90u8).unwrap();
    module.eh_begin_fde(personality).unwrap();
    module.eh_end_fde().unwrap();
    let first_end = module.eh_size();
    module.eh_begin_fde(personality).unwrap();
    module.eh_end_fde().unwrap();
    assert_eq!(module.eh_size() - first_end, 24);
    let second_end = module.eh_size();
    module.eh_begin_fde(SymRef::default()).unwrap();
    module.eh_end_fde().unwrap();
    assert_eq!(module.eh_size() - second_end, 40);
    let eh = module
        .assembler_mut()
        .get_default_section(SectionKind::EHFrame)
        .unwrap();
    let relocs = module.assembler().get_section(eh).unwrap().relocations();
    assert_eq!(
        relocs
            .iter()
            .filter(|reloc| reloc.symbol == personality)
            .count(),
        1
    );
    assert_eq!(relocs.len(), 4);
}

#[test]
fn end_module_requires_flushed_text_closed_fde_and_aligned_eh() {
    fixture!(module, writer, assembler, context);
    module.write(0x90u8).unwrap();
    assert!(module.end_module().is_err());
    module.flush().unwrap();
    module.eh_begin_fde(SymRef::default()).unwrap();
    assert!(module.end_module().is_err());
    module.eh_end_fde().unwrap();
    module.eh_write_inst(dwarf::DW_CFA_nop).unwrap();
    assert!(module.end_module().is_err());
    module.eh_align_frame().unwrap();
    module.end_module().unwrap();
    assert_eq!(module.eh_size() % 8, 0);
}

#[test]
fn exception_cleanup_includes_uncovered_ranges_and_prologue_skew() {
    fixture!(module, writer, assembler, context);
    let personality = module
        .assembler_mut()
        .sym_add_undef(b"personality", SymBinding::Global)
        .unwrap();
    module.eh_begin_fde(personality).unwrap();
    module.write_bytes(&[0; 16]).unwrap();
    let landing = module.label_create().unwrap();
    module.label_place(landing, 14).unwrap();
    module.except_add_call_site(6, 3, landing, true).unwrap();
    module.remove_prologue_bytes(0, 2).unwrap();
    module.except_encode_func().unwrap();
    let lsda = module
        .assembler_mut()
        .get_default_section(SectionKind::LSDA)
        .unwrap();
    assert_eq!(
        call_sites(&module.assembler().get_section(lsda).unwrap().data),
        vec![[0, 4, 0, 0], [4, 3, 12, 0], [7, 7, 0, 0],]
    );
    module.eh_end_fde().unwrap();
}

#[test]
fn exception_actions_deduplicate_types_and_reset_between_functions() {
    fixture!(module, writer, assembler, context);
    let personality = module
        .assembler_mut()
        .sym_add_undef(b"personality", SymBinding::Global)
        .unwrap();
    let first = module
        .assembler_mut()
        .sym_add_undef(b"type_a", SymBinding::Global)
        .unwrap();
    let second = module
        .assembler_mut()
        .sym_add_undef(b"type_b", SymBinding::Global)
        .unwrap();
    module.eh_begin_fde(personality).unwrap();
    module.write_bytes(&[0; 16]).unwrap();
    let landing = module.label_create().unwrap();
    module.label_place(landing, 15).unwrap();
    module.except_add_call_site(0, 8, landing, false).unwrap();
    module.except_add_action(true, first).unwrap();
    module.except_add_action(false, second).unwrap();
    module.except_add_action(false, first).unwrap();
    module.except_add_action(false, SymRef::default()).unwrap();
    module.except_add_empty_spec_action(false).unwrap();
    module.except_add_cleanup_action().unwrap();
    assert_eq!(module.except_type_idx_for_sym(first).unwrap(), 2);
    assert_eq!(module.except_type_idx_for_sym(second).unwrap(), 3);
    assert!(module.except_type_idx_for_sym(SymRef::default()).is_err());
    module.except_encode_func().unwrap();
    let lsda = module
        .assembler_mut()
        .get_default_section(SectionKind::LSDA)
        .unwrap();
    let section = module.assembler().get_section(lsda).unwrap();
    assert_eq!(call_sites(&section.data), vec![[0, 8, 15, 3], [8, 8, 0, 0]]);
    assert_eq!(section.relocations().len(), 2);
    assert_eq!(section.relocations()[0].symbol, first);
    assert_eq!(section.relocations()[1].symbol, second);
    assert_eq!(
        section.relocations()[0].offset,
        section.relocations()[1].offset + 4
    );
    assert_eq!(&section.data[section.data.len() - 4..], &[0; 4]);
    module.eh_end_fde().unwrap();
    module.begin_func(1, 8).unwrap();
    assert!(module.except_type_idx_for_sym(first).is_err());
}

#[test]
fn invalid_exception_ranges_fail_before_lsda_creation() {
    fixture!(module, writer, assembler, context);
    let personality = module
        .assembler_mut()
        .sym_add_undef(b"personality", SymBinding::Global)
        .unwrap();
    module.eh_begin_fde(personality).unwrap();
    module.write_bytes(&[0; 8]).unwrap();
    let landing = module.label_create().unwrap();
    module.label_place(landing, 8).unwrap();
    module.except_add_call_site(1, 3, landing, true).unwrap();
    assert!(module.except_add_call_site(1, 1, landing, true).is_err());
    assert!(module.except_add_call_site(0, 1, landing, true).is_err());
    let count = module.assembler().section_count();
    assert!(module.except_encode_func().is_err());
    assert_eq!(module.assembler().section_count(), count);
}

#[test]
fn exceptions_without_personality_do_not_create_lsda() {
    fixture!(module, writer, assembler, context);
    module.write_bytes(&[0; 4]).unwrap();
    let landing = module.label_create().unwrap();
    module.except_add_call_site(0, 1, landing, true).unwrap();
    let count = module.assembler().section_count();
    module.except_encode_func().unwrap();
    assert_eq!(module.assembler().section_count(), count);
}

#[test]
fn jump_table_section_is_shared_read_only_section() {
    fixture!(module, writer, assembler, context);
    let section = module.get_jump_table_section().unwrap();
    let before = module.assembler().section_count();
    assert_eq!(module.get_jump_table_section().unwrap(), section);
    assert_eq!(module.assembler().section_count(), before);
    assert_eq!(module.assembler().sec_name(section).unwrap(), b".rodata");
}

#[test]
fn overwrite_and_restore_cursor_preserve_initialized_suffix() {
    fixture!(module, writer, assembler, context);
    module.write_bytes(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    module.set_offset(1).unwrap();
    module.write(0xaabbu16).unwrap();
    module.set_offset(8).unwrap();
    assert_eq!(module.data().unwrap(), &[1, 0xbb, 0xaa, 4, 5, 6, 7, 8]);
    module.set_offset(3).unwrap();
    module.write_bytes(&[9, 10]).unwrap();
    module.skip(1).unwrap();
    module.set_offset(8).unwrap();
    assert_eq!(module.data().unwrap(), &[1, 0xbb, 0xaa, 9, 10, 0, 7, 8]);
}

#[test]
fn reservation_cap_can_precede_initialized_suffix() {
    fixture!(module, writer, assembler, context);
    module.write_bytes(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    module.set_offset(2).unwrap();
    module.cap_reserved_end(4).unwrap();
    module.write(0xaabbu16).unwrap();
    assert_eq!(module.text_len(), 8);
    assert!(module.owns_text() && module.get_section().unwrap().locked);
    assert!(module.set_offset(8).is_err());
    module.ensure_space(4).unwrap();
    module.set_offset(8).unwrap();
    assert_eq!(module.data().unwrap(), &[1, 2, 0xbb, 0xaa, 5, 6, 7, 8]);
}

#[test]
fn flush_discards_initialized_suffix_when_cursor_equals_reservation_cap() {
    fixture!(module, writer, assembler, context);
    module.write_bytes(&[7; 32]).unwrap();
    let capacity = module.text_capacity();
    module.set_offset(16).unwrap();
    module.cap_reserved_end(16).unwrap();
    assert_eq!(module.text_len(), 32);
    assert!(module.get_section().unwrap().locked);
    module.flush().unwrap();
    assert!(!module.owns_text() && !module.get_section().unwrap().locked);
    assert_eq!(module.allocated_size(), 16);
    assert_eq!(&module.get_section().unwrap().data[..], &[7; 16]);
    assert_eq!(module.get_section().unwrap().data.capacity(), capacity);
    assert!(module.set_offset(17).is_err());
    module.ensure_space(4).unwrap();
    module.write_bytes(&[9; 4]).unwrap();
    module.flush().unwrap();
    assert_eq!(module.get_section().unwrap().data.len(), 20);
    assert_eq!(module.data().unwrap()[16..], [9; 4]);
    module.flush().unwrap();
    assert_eq!(module.get_section().unwrap().data.len(), 20);
    module.set_offset(0).unwrap();
    module.cap_reserved_end(0).unwrap();
    module.flush().unwrap();
    assert!(module.get_section().unwrap().data.is_empty());
}

#[test]
fn empty_module_flush_preserves_unselected_section() {
    let context = MemoryContext::new("empty-module-flush");
    let mut assembler = AssemblerElf::new(context.mcx(), Architecture::X86_64).unwrap();
    let mut writer = FunctionWriter::new(context.mcx(), RelativeTarget::default());
    let mut module = writer.begin_module(&mut assembler).unwrap();
    let sections = module.assembler().section_count();
    module.flush().unwrap();
    module.flush().unwrap();
    assert!(!module.get_sec_ref().valid());
    assert_eq!(module.offset(), 0);
    assert_eq!(module.allocated_size(), 0);
    assert_eq!(module.assembler().section_count(), sections);
    module.end_module().unwrap();
}

#[test]
fn const_cfi_encoders_build_runtime_borrowed_cie() {
    const ENCODED: ([u8; 21], usize) =
        FunctionWriterBase::encode_eh_inst2(dwarf::DW_CFA_def_cfa, 7, 128);
    const ONE: ([u8; 11], usize) = FunctionWriterBase::encode_eh_inst1(dwarf::DW_CFA_offset, 16);
    assert_eq!(&ENCODED.0[..ENCODED.1], &[0x0c, 7, 0x80, 1]);
    assert_eq!(&ONE.0[..ONE.1], &[0x90]);
    let instructions = vec![
        ENCODED.0[0],
        ENCODED.0[1],
        ENCODED.0[2],
        ENCODED.0[3],
        ONE.0[0],
        2,
    ];
    let context = MemoryContext::new("borrowed-cie");
    let mut assembler = AssemblerElf::new(context.mcx(), Architecture::X86_64).unwrap();
    let info = TargetCieInfo {
        instrs: &instructions,
        return_addr_register: 18,
        code_alignment_factor: 2,
        data_alignment_factor: 0x7c,
    };
    let mut writer = FunctionWriter::with_cie(context.mcx(), RelativeTarget::default(), info);
    let mut module = writer.begin_module(&mut assembler).unwrap();
    assert_eq!(&module.eh_data().unwrap()[12..17], &[2, 0x7c, 18, 1, 0x1b]);
    assert_eq!(&module.eh_data().unwrap()[17..23], &instructions);
    module.end_module().unwrap();
}

#[test]
fn target_parts_mut_can_patch_through_the_same_module() {
    fixture!(module, writer, assembler, context);
    module.write(0u32).unwrap();
    let label = module.label_create().unwrap();
    module.label_place(label, 12).unwrap();
    module
        .label_ref(label, 0, LabelFixupKind::ARCH_KIND_BEGIN)
        .unwrap();
    let (target, base) = module.parts_mut();
    target.handle_fixups(base).unwrap();
    assert_eq!(base.data().unwrap(), &[8, 0, 0, 0]);
    assert_eq!(target.finishes, 1);
}

#[test]
fn native_reservation_publishes_only_initialized_prefix() {
    fixture!(module, writer, assembler, context);
    module.write(0x55u8).unwrap();
    let mut reservation = module.reserve_uninitialized(16).unwrap();
    for (slot, byte) in reservation.iter_mut().zip([0x48, 0x83, 0xc0, 0x01]) {
        slot.write(byte);
    }
    // SAFETY: the four published bytes were initialized above; the suffix stays unpublished.
    unsafe {
        reservation.commit(4).unwrap();
    }
    assert_eq!(module.data().unwrap(), &[0x55, 0x48, 0x83, 0xc0, 0x01]);
    assert_eq!(module.text_len(), 5);
    module.write(0xc3u8).unwrap();
    assert_eq!(
        module.data().unwrap(),
        &[0x55, 0x48, 0x83, 0xc0, 0x01, 0xc3]
    );
}

#[test]
fn cancelled_native_reservations_restore_inline_and_heap_live_prefixes() {
    fixture!(module, writer, assembler, context);
    for initial in [7, 300] {
        module.write_bytes(&vec![0x5a; initial]).unwrap();
        let saved = module.data().unwrap().to_vec();
        let mut reservation = module.reserve_uninitialized(17).unwrap();
        reservation[0].write(0x44);
        drop(reservation);
        assert_eq!(module.data().unwrap(), &saved);
        module.write(0xccu8).unwrap();
        assert_eq!(module.data().unwrap().last(), Some(&0xcc));
        let reservation = module.reserve_uninitialized(0).unwrap();
        // SAFETY: an empty prefix requires no initialized bytes.
        unsafe {
            reservation.commit(0).unwrap();
        }
        assert_eq!(module.text_len(), saved.len() + 1);
    }
}

#[test]
fn native_reservation_requires_append_cursor() {
    fixture!(module, writer, assembler, context);
    module.write_bytes(&[1, 2, 3, 4]).unwrap();
    module.set_offset(2).unwrap();
    assert!(module.reserve_uninitialized(1).is_err());
    assert_eq!(module.offset(), 2);
    module.set_offset(4).unwrap();
    assert_eq!(module.data().unwrap(), &[1, 2, 3, 4]);
}

#[test]
fn oversized_jump_table_and_regular_slabs_recycle_independently() {
    fixture!(module, writer, assembler, context);
    for epoch in 0..3 {
        module.begin_func(1, 8).unwrap();
        module
            .alloc_jump_table(1500, Reg::new(0), Reg::new(1))
            .unwrap()
            .labels_mut()
            .fill(Label(epoch));
        module
            .alloc_jump_table(2, Reg::new(2), Reg::new(3))
            .unwrap()
            .labels_mut()
            .copy_from_slice(&[Label(7), Label(8)]);
        assert_eq!(module.jump_table_labels(0).unwrap().len(), 1500);
        assert!(module
            .jump_table_labels(0)
            .unwrap()
            .iter()
            .all(|&label| label == Label(epoch)));
        assert_eq!(module.jump_table_labels(1).unwrap(), &[Label(7), Label(8)]);
    }
}
