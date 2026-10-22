// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use super::*;
use mcx::MemoryContext;
use tpde_assembler::{elf, Architecture, AssemblerElf, SectionKind};
use tpde_core::Reg;
use tpde_function_writer::{FunctionWriterTarget, Label, LabelFixupKind, ModuleWriter};

const NOP: u32 = 0xd503201f;
macro_rules! fixture {
    ($module:ident, $writer:ident, $assembler:ident, $context:ident) => {
        let $context = MemoryContext::new("A64 writer");
        let mut $assembler = AssemblerElf::new($context.mcx(), Architecture::Aarch64).unwrap();
        let text = $assembler.get_default_section(SectionKind::Text).unwrap();
        let mut $writer = FunctionWriterA64::new($context.mcx(), A64Target::new($context.mcx()));
        let mut $module = $writer.begin_module(&mut $assembler).unwrap();
        $module.switch_section(text).unwrap();
        $module.begin_func(4, 64).unwrap();
    };
}
fn word(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn pad(module: &mut ModuleWriter<'_, '_, '_, A64Target<'_>>, end: usize) {
    while module.offset() < end {
        module.skip((end - module.offset()).min(16384)).unwrap();
    }
}
fn reference(
    module: &mut ModuleWriter<'_, '_, '_, A64Target<'_>>,
    label: Label,
    kind: LabelFixupKind,
    instruction: u32,
) {
    module.ensure_space(4).unwrap();
    let off = module.offset() as u32;
    module.write_inst_unchecked(instruction).unwrap();
    module.label_ref(label, off, kind).unwrap();
}

#[test]
fn cie_and_byte_scaled_cfi_cover_encoding_boundaries() {
    fixture!(module, writer, assembler, context);
    assert_eq!(A64Target::CIE.instrs, &[0x0c, 31, 0]);
    assert_eq!(
        (
            A64Target::CIE.return_addr_register,
            A64Target::CIE.code_alignment_factor,
            A64Target::CIE.data_alignment_factor
        ),
        (30, 4, 120)
    );
    let start = module.eh_size();
    for bytes in [0, 3, 4, 252, 256, 1020, 1024, 262140, 262144] {
        module.eh_advance(bytes).unwrap();
    }
    assert_eq!(
        &module.eh_data().unwrap()[start..],
        &[
            0x40, 0x40, 0x41, 0x7f, 0x02, 64, 0x02, 255, 0x03, 0, 1, 0x03, 255, 255, 0x04, 0, 0, 1,
            0
        ]
    );
}

#[test]
fn zero_instruction_refusal_preserves_text_and_allows_success() {
    fixture!(module, writer, assembler, context);
    assert!(!module.try_write_inst(0).unwrap());
    assert!(module.write_inst(0).is_err());
    assert!(module.write_inst_unchecked(0).is_err());
    assert_eq!(module.offset(), 0);
    assert!(module.try_write_inst(NOP).unwrap());
    module.write_inst(NOP).unwrap();
    module.write_inst_unchecked(NOP).unwrap();
    assert_eq!(module.data().unwrap(), NOP.to_le_bytes().repeat(3));
}

#[test]
fn unchecked_write_still_obeys_reserved_capacity() {
    fixture!(module, writer, assembler, context);
    module.cap_reserved_end(0).unwrap();
    assert!(module.write_inst_unchecked(NOP).is_err());
    assert_eq!(module.offset(), 0);
    module.write_inst(NOP).unwrap();
    assert_eq!(module.data().unwrap(), &NOP.to_le_bytes());
}

#[test]
fn large_hint_is_capped_but_explicit_advancement_refuses_atomically() {
    fixture!(module, writer, assembler, context);
    module.begin_func(4, 1 << 20).unwrap();
    assert_eq!(module.allocated_size() - module.offset(), 16384);
    for size in [16385, 1 << 20, usize::MAX] {
        assert!(module.ensure_space(size).is_err());
        assert!(module.skip(size).is_err());
        assert_eq!(module.offset(), 0);
    }
    module.skip(16384).unwrap();
    module.write_inst(NOP).unwrap();
    assert_eq!(module.offset(), 16388);
}

#[test]
fn large_alignment_with_small_live_padding_remains_valid() {
    fixture!(module, writer, assembler, context);
    for align in [32768, 65536] {
        module.begin_func(align, 0).unwrap();
        assert_eq!(module.offset(), 0);
        assert_eq!(module.get_section().unwrap().align, align);
    }
    pad(&mut module, 65532);
    module.align(65536).unwrap();
    assert_eq!(module.offset(), 65536);
    assert_eq!(&module.data().unwrap()[65532..], &[0; 4]);
}

#[test]
fn near_branches_patch_all_kinds_without_touching_other_bits() {
    fixture!(module, writer, assembler, context);
    let label = module.label_create().unwrap();
    reference(&mut module, label, LabelFixupKind::AARCH64_BR, 0x14000000);
    reference(
        &mut module,
        label,
        LabelFixupKind::AARCH64_COND_BR,
        0xb4000003,
    );
    reference(
        &mut module,
        label,
        LabelFixupKind::AARCH64_TEST_BR,
        0x36300005,
    );
    module.label_place(label, 12).unwrap();
    module.finish_func().unwrap();
    let data = module.data().unwrap();
    assert_eq!(word(data, 0), 0x14000003);
    assert_eq!(word(data, 4), 0xb4000043);
    assert_eq!(word(data, 8), 0x36300025);
}

#[test]
fn prologue_removal_adjusts_near_fixups_and_labels() {
    fixture!(module, writer, assembler, context);
    module.skip(16).unwrap();
    let label = module.label_create().unwrap();
    reference(&mut module, label, LabelFixupKind::AARCH64_BR, 0x14000000);
    module.write_inst(NOP).unwrap();
    module.label_place(label, 24).unwrap();
    module.remove_prologue_bytes(0, 16).unwrap();
    module.finish_func().unwrap();
    assert_eq!(module.label_offset(label).unwrap(), 8);
    assert_eq!(word(module.data().unwrap(), 0), 0x14000002);
}

#[test]
fn invalid_label_and_unknown_fixup_are_explicit_and_module_is_reusable() {
    fixture!(module, writer, assembler, context);
    assert!(module
        .label_ref(Label(9), 0, LabelFixupKind::AARCH64_TEST_BR)
        .is_err());
    assert!(module.label_fixups().is_empty());
    let label = module.label_create().unwrap();
    module.write_inst(NOP).unwrap();
    module.label_place(label, 4).unwrap();
    module.label_ref(label, 0, LabelFixupKind(77)).unwrap();
    assert!(module.finish_func().is_err());
    drop(module);
    assembler.reset().unwrap();
    let text = assembler.get_default_section(SectionKind::Text).unwrap();
    let mut module = writer.begin_module(&mut assembler).unwrap();
    module.switch_section(text).unwrap();
    module.begin_func(4, 64).unwrap();
    module.write_inst(NOP).unwrap();
    module.finish_func().unwrap();
}

#[test]
fn backward_unconditional_fixup_refuses_then_new_module_succeeds() {
    fixture!(module, writer, assembler, context);
    let label = module.label_create().unwrap();
    module.label_place(label, 0).unwrap();
    module.write_inst(NOP).unwrap();
    reference(&mut module, label, LabelFixupKind::AARCH64_BR, 0x14000000);
    assert!(module.finish_func().is_err());
    drop(module);
    assembler.reset().unwrap();
    let text = assembler.get_default_section(SectionKind::Text).unwrap();
    let mut module = writer.begin_module(&mut assembler).unwrap();
    module.switch_section(text).unwrap();
    module.begin_func(4, 64).unwrap();
    module.write_inst(NOP).unwrap();
    module.finish_func().unwrap();
}

#[test]
fn small_tables_emit_both_index_widths_and_exact_relocations() {
    for index_is_32 in [false, true] {
        fixture!(module, writer, assembler, context);
        let first = module.label_create().unwrap();
        let last = module.label_create().unwrap();
        module.label_place(first, 0).unwrap();
        module
            .create_jump_table(3, Reg::new(3), Reg::new(9), index_is_32, |labels| {
                labels.copy_from_slice(&[first, last, first])
            })
            .unwrap();
        module.label_place(last, 24).unwrap();
        assert_eq!(module.jump_table_header(0).unwrap().off, 0);
        assert_eq!(
            module.jump_table_header(0).unwrap().misc,
            u8::from(index_is_32)
        );
        module.finish_func().unwrap();
        assert_eq!(module.offset(), 24);
        let table = module.get_jump_table_section().unwrap();
        assert_eq!(
            &module.assembler().get_section(table).unwrap().data[..],
            &[0, 6, 0]
        );
        let relocs = module.get_section().unwrap().relocations();
        assert_eq!(relocs.len(), 2);
        assert_eq!(relocs[0].offset, 0);
        assert_eq!(relocs[1].offset, 4);
        assert_eq!(relocs[0].kind, elf::R_AARCH64_ADR_PREL_PG_HI21);
        assert_eq!(relocs[1].kind, elf::R_AARCH64_ADD_ABS_LO12_NC);
    }
}

#[test]
fn empty_table_and_invalid_registers_have_bounded_effects() {
    fixture!(module, writer, assembler, context);
    assert!(module
        .create_jump_table(1, Reg::make_invalid(), Reg::new(1), false, |_| panic!(
            "invalid input reached fill"
        ))
        .is_err());
    assert_eq!(module.jump_table_count(), 0);
    module
        .create_jump_table(0, Reg::new(0), Reg::new(1), false, |labels| {
            assert!(labels.is_empty())
        })
        .unwrap();
    module.finish_func().unwrap();
    assert_eq!(module.offset(), 24);
}

#[test]
fn table_metadata_resets_between_functions() {
    fixture!(module, writer, assembler, context);
    for _ in 0..3 {
        module.begin_func(4, 64).unwrap();
        assert_eq!(module.jump_table_count(), 0);
        let label = module.label_create().unwrap();
        module
            .create_jump_table(1, Reg::new(2), Reg::new(5), false, |labels| {
                labels[0] = label
            })
            .unwrap();
        let off = module.offset() as u32;
        module.label_place(label, off).unwrap();
        module.finish_func().unwrap();
    }
    assert_eq!(module.offset(), 72);
}

#[test]
fn jump_table_growth_preserves_inserted_veneer() {
    fixture!(module, writer, assembler, context);
    module.begin_func(4, 32768).unwrap();
    let label = module.label_create().unwrap();
    reference(
        &mut module,
        label,
        LabelFixupKind::AARCH64_TEST_BR,
        0x36000000,
    );
    pad(&mut module, 16372);
    module
        .create_jump_table(1, Reg::new(0), Reg::new(1), false, |labels| {
            labels[0] = label
        })
        .unwrap();
    assert_eq!(module.jump_table_header(0).unwrap().off, 16380);
    assert_eq!(module.offset(), 16404);
    pad(&mut module, 65536);
    let off = module.offset() as u32;
    module.label_place(label, off).unwrap();
    module.finish_func().unwrap();
    let data = module.data().unwrap();
    assert_eq!(word(data, 16372), 0x14000002);
    assert_eq!(word(data, 16376), 0x14000000 | ((65536 - 16376) / 4));
    assert_eq!(word(data, 16380) & 0x9f000000, 0x90000000);
}

#[test]
fn exhausted_veneer_is_skipped_with_and_without_prologue_skew() {
    for skew in [0, 16] {
        fixture!(module, writer, assembler, context);
        module.begin_func(4, 32768).unwrap();
        module.skip(16).unwrap();
        let label = module.label_create().unwrap();
        reference(
            &mut module,
            label,
            LabelFixupKind::AARCH64_TEST_BR,
            0x36000000,
        );
        pad(&mut module, 16384);
        module.ensure_space(4).unwrap();
        assert_eq!(module.offset(), 16392);
        reference(
            &mut module,
            label,
            LabelFixupKind::AARCH64_TEST_BR,
            0x36000001,
        );
        pad(&mut module, 65536);
        let off = module.offset() as u32;
        module.label_place(label, off).unwrap();
        module.remove_prologue_bytes(0, skew).unwrap();
        module.finish_func().unwrap();
        let bytes = module.data().unwrap();
        let first = word(bytes, 16 - skew as usize);
        let second = word(bytes, 16392 - skew as usize);
        let first_veneer = 16 + ((first >> 5) & 0x3fff) as usize * 4;
        let second_veneer = 16392 + ((second >> 5) & 0x3fff) as usize * 4;
        assert_eq!(first_veneer, 16388);
        assert!(second_veneer > 16392);
        for veneer in [first_veneer, second_veneer] {
            let branch = word(bytes, veneer - skew as usize);
            assert_eq!(veneer + (branch & 0x03ffffff) as usize * 4, 65536);
        }
    }
}

#[test]
fn wide_backward_table_entries_sign_extend_to_the_intended_label() {
    fixture!(module, writer, assembler, context);
    let before = module.label_create().unwrap();
    let after = module.label_create().unwrap();
    module.label_place(before, 0).unwrap();
    module.skip(64).unwrap();
    module
        .create_jump_table(2, Reg::new(0), Reg::new(1), false, |labels| {
            labels.copy_from_slice(&[before, after])
        })
        .unwrap();
    module.label_place(after, 1 << 18).unwrap();
    module.finish_func().unwrap();
    let section = module.get_jump_table_section().unwrap();
    let data = &module.assembler().get_section(section).unwrap().data;
    let backward = i32::from_le_bytes(data[..4].try_into().unwrap());
    let forward = i32::from_le_bytes(data[4..8].try_into().unwrap());
    assert_eq!(76 + i64::from(backward) * 4, 0);
    assert_eq!(76 + i64::from(forward) * 4, 1 << 18);
}

#[test]
fn allocation_failure_abandons_function_and_preserves_owner_reuse() {
    let context = MemoryContext::new("A64 limited writer").with_limit(128 * 1024);
    let mut assembler = AssemblerElf::new(context.mcx(), Architecture::Aarch64).unwrap();
    let text = assembler.get_default_section(SectionKind::Text).unwrap();
    let mut writer = FunctionWriterA64::new(context.mcx(), A64Target::new(context.mcx()));
    let mut module = writer.begin_module(&mut assembler).unwrap();
    module.switch_section(text).unwrap();
    module.begin_func(4, 64).unwrap();
    assert!(module
        .create_jump_table(1 << 20, Reg::new(0), Reg::new(1), false, |_| panic!(
            "failed allocation reached fill"
        ))
        .is_err());
    assert_eq!(module.jump_table_count(), 0);
    assert!(module.begin_func(4, 1 << 20).is_err());
    module.begin_func(4, 64).unwrap();
    module.write_inst(NOP).unwrap();
    module.finish_func().unwrap();
    module.flush().unwrap();
    module.end_module().unwrap();
    assert!(context.stats().used <= context.stats().limit);
}

#[test]
fn partial_table_finalization_is_discarded_by_module_reset() {
    fixture!(module, writer, assembler, context);
    let placed = module.label_create().unwrap();
    let pending = module.label_create().unwrap();
    module
        .create_jump_table(1, Reg::new(0), Reg::new(1), false, |labels| {
            labels[0] = placed
        })
        .unwrap();
    module
        .create_jump_table(1, Reg::new(2), Reg::new(3), false, |labels| {
            labels[0] = pending
        })
        .unwrap();
    module.label_place(placed, 48).unwrap();
    assert!(module.finish_func().is_err());
    assert_eq!(module.get_section().unwrap().relocations().len(), 2);
    drop(module);
    assembler.reset().unwrap();
    let text = assembler.get_default_section(SectionKind::Text).unwrap();
    let mut module = writer.begin_module(&mut assembler).unwrap();
    module.switch_section(text).unwrap();
    module.begin_func(4, 64).unwrap();
    assert!(module.get_section().unwrap().relocations().is_empty());
    assert_eq!(module.jump_table_count(), 0);
    module.write_inst(NOP).unwrap();
    module.finish_func().unwrap();
    module.flush().unwrap();
    module.end_module().unwrap();
}

#[test]
fn module_reuse_keeps_arena_storage_charged_until_context_reset() {
    let mut context = MemoryContext::new_bump("A64 repeated module");
    let mut retained_text_capacity = 0;
    {
        let mut assembler = AssemblerElf::new(context.mcx(), Architecture::Aarch64).unwrap();
        let mut writer = FunctionWriterA64::new(context.mcx(), A64Target::new(context.mcx()));
        let mut expected = None;
        for _ in 0..3 {
            assembler.reset().unwrap();
            let text = assembler.get_default_section(SectionKind::Text).unwrap();
            let mut module = writer.begin_module(&mut assembler).unwrap();
            module.switch_section(text).unwrap();
            module.begin_func(4, 32768).unwrap();
            module.write_inst(NOP).unwrap();
            module.finish_func().unwrap();
            module.flush().unwrap();
            module.end_module().unwrap();
            retained_text_capacity += module.get_section().unwrap().data.capacity();
            assert!(context.stats().used >= retained_text_capacity);
            let bytes = module.eh_data().unwrap().to_vec();
            if let Some(previous) = &expected {
                assert_eq!(&bytes, previous);
            }
            expected = Some(bytes);
        }
    }
    assert!(retained_text_capacity >= 3 * 32768);
    assert!(context.stats().used >= retained_text_capacity);
    let before_reset = context.stats().used;
    context.reset();
    let after_reset = context.stats();
    assert_eq!(after_reset.used, after_reset.arena_footprint);
    assert!(after_reset.used > 0);
    assert!(after_reset.used < before_reset);
}
