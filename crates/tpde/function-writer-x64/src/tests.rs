// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use super::*;
use mcx::MemoryContext;
use std::vec;
use tpde_assembler::{Architecture, AssemblerElf, SectionKind, SymRef};

macro_rules! fixture {
    ($module:ident, $writer:ident, $assembler:ident, $context:ident) => {
        let $context = MemoryContext::new("x64-writer");
        let mut $assembler = AssemblerElf::new($context.mcx(), Architecture::X86_64).unwrap();
        let text = $assembler.get_default_section(SectionKind::Text).unwrap();
        let mut $writer = FunctionWriterX64::new($context.mcx(), X64Target::new());
        let mut $module = $writer.begin_module(&mut $assembler).unwrap();
        $module.switch_section(text).unwrap();
        $module.begin_func(1, 8).unwrap();
    };
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

#[test]
fn cie_and_fde_preserve_x64_stack_contract() {
    fixture!(module, writer, assembler, context);
    assert_eq!(X64Target::CIE.instrs, &[0x0c, 7, 8, 0x90, 1]);
    assert_eq!(X64Target::CIE.return_addr_register, 16);
    assert_eq!(X64Target::CIE.code_alignment_factor, 1);
    assert_eq!(X64Target::CIE.data_alignment_factor, 120);
    assert!(module
        .eh_data()
        .unwrap()
        .windows(5)
        .any(|bytes| bytes == [12, 7, 8, 0x90, 1]));
    module.eh_begin_fde(SymRef::default()).unwrap();
    module.write_bytes(&[0x90; 17]).unwrap();
    module.eh_advance(17).unwrap();
    module.eh_end_fde().unwrap();
    module.finish_func().unwrap();
    module.flush().unwrap();
    module.end_module().unwrap();
    let eh = module.eh_data().unwrap();
    let fde = word(eh, 0) as usize + 4;
    assert_eq!(word(eh, fde + 12), 17);
    assert_eq!(eh[fde + 17], 0x51);
}

#[test]
fn alignment_preserves_prefix_and_handles_no_padding() {
    fixture!(module, writer, assembler, context);
    module.write(0xccu8).unwrap();
    module.align(4).unwrap();
    assert_eq!(module.data().unwrap(), &[0xcc, 0x0f, 0x1f, 0]);
    module.align(4).unwrap();
    assert_eq!(module.data().unwrap(), &[0xcc, 0x0f, 0x1f, 0]);
    module.write(0xc3u8).unwrap();
    module.align(4096).unwrap();
    assert_eq!(module.offset(), 4096);
    assert_eq!(&module.data().unwrap()[..5], &[0xcc, 0x0f, 0x1f, 0, 0xc3]);
    assert_eq!(module.get_section().unwrap().align, 4096);
    let previous = module.data().unwrap().to_vec();
    for alignment in [0, 3, 5, usize::MAX] {
        assert!(module.align(alignment).is_err());
        assert_eq!(module.data().unwrap(), previous);
    }
}

#[test]
fn rel32_preserves_positive_negative_and_wrapping_distances() {
    fixture!(module, writer, assembler, context);
    for target in [0, 4, 8, 12, 0x8000_0000, u32::MAX - 1] {
        module.begin_func(1, 8).unwrap();
        let start = module.offset() as u32;
        module.write_bytes(&[0x55; 12]).unwrap();
        let label = module.label_create().unwrap();
        module.label_place(label, target).unwrap();
        module
            .label_ref(label, start + 4, LabelFixupKind::X64_JMP_OR_MEM_DISP)
            .unwrap();
        module.finish_func().unwrap();
        assert_eq!(
            word(module.data().unwrap(), start as usize + 4),
            target.wrapping_sub(start + 8)
        );
        assert_eq!(
            &module.data().unwrap()[start as usize..start as usize + 4],
            &[0x55; 4]
        );
    }
}

#[test]
fn rel32_and_jump_table_offsets_follow_prologue_removal() {
    fixture!(module, writer, assembler, context);
    module.write_bytes(&[0xaa; 4]).unwrap();
    let label = module.label_create().unwrap();
    module.label_place(label, 4).unwrap();
    module.write_bytes(&[0xbb; 8]).unwrap();
    module
        .label_ref(label, 8, LabelFixupKind::X64_JMP_OR_MEM_DISP)
        .unwrap();
    module
        .create_jump_table(2, Reg::new(12), Reg::new(13), false, |labels| {
            labels.fill(label)
        })
        .unwrap();
    module.remove_prologue_bytes(0, 4).unwrap();
    module.finish_func().unwrap();
    assert_eq!(module.label_offset(label).unwrap(), 0);
    assert_eq!(word(module.data().unwrap(), 4), u32::MAX - 7);
    assert_eq!(module.offset(), 36);
    assert_eq!(word(module.data().unwrap(), 11), 13);
    assert_eq!(word(module.data().unwrap(), 28), 28);
    assert_eq!(word(module.data().unwrap(), 32), 28);
}

#[test]
fn invalid_fixups_refuse_before_any_patch_and_reuse() {
    fixture!(module, writer, assembler, context);
    module.write_bytes(&[0xaa; 8]).unwrap();
    let ready = module.label_create().unwrap();
    module.label_place(ready, 0).unwrap();
    let pending = module.label_create().unwrap();
    module
        .label_ref(ready, 0, LabelFixupKind::X64_JMP_OR_MEM_DISP)
        .unwrap();
    module
        .label_ref(pending, 4, LabelFixupKind::X64_JMP_OR_MEM_DISP)
        .unwrap();
    assert!(module.finish_func().is_err());
    assert_eq!(module.data().unwrap(), &[0xaa; 8]);
    module.label_place(pending, 8).unwrap();
    module.finish_func().unwrap();
    assert_eq!(
        module.data().unwrap(),
        &[0xfc, 0xff, 0xff, 0xff, 0, 0, 0, 0]
    );
    for (off, kind) in [(5, 0), (0, 1), (u32::MAX, 0)] {
        module.begin_func(1, 8).unwrap();
        let start = module.offset();
        module.write_bytes(&[0x6d; 8]).unwrap();
        let label = module.label_create().unwrap();
        module.label_place(label, start as u32).unwrap();
        let off = if off == u32::MAX {
            off
        } else {
            start as u32 + off
        };
        module.label_ref(label, off, LabelFixupKind(kind)).unwrap();
        assert!(module.finish_func().is_err());
        assert_eq!(&module.data().unwrap()[start..], &[0x6d; 8]);
    }
    module.begin_func(1, 8).unwrap();
    module.write(0xc3u8).unwrap();
    module.finish_func().unwrap();
}

#[test]
fn table_reserves_exact_code_and_preserves_descending_deltas() {
    fixture!(module, writer, assembler, context);
    let mut labels = [Label(0); 3];
    for label in &mut labels {
        *label = module.label_create().unwrap();
        let offset = module.offset() as u32;
        module.label_place(*label, offset).unwrap();
        module.write_bytes(&[0x90; 3]).unwrap();
    }
    let code = module.offset();
    module
        .create_jump_table(4, Reg::new(0), Reg::new(1), true, |table| {
            table.copy_from_slice(&[labels[0], labels[2], labels[1], labels[0]])
        })
        .unwrap();
    assert_eq!(module.offset(), code + 18);
    assert_eq!(module.jump_table_header(0).unwrap().off as usize, code);
    assert_eq!(module.jump_table_header(0).unwrap().misc, 0);
    module.finish_func().unwrap();
    let bytes = module.data().unwrap();
    assert_eq!(module.offset(), 44);
    assert_eq!(word(bytes, code + 3), 28 - code as u32 - 7);
    assert_eq!(&bytes[27..28], &[0x90]);
    assert_eq!(
        [
            word(bytes, 28),
            word(bytes, 32),
            word(bytes, 36),
            word(bytes, 40)
        ],
        [28, 22, 25, 28]
    );
}

#[test]
fn empty_tables_and_multiple_tables_have_independent_pc_bases() {
    fixture!(module, writer, assembler, context);
    let label = module.label_create().unwrap();
    module.label_place(label, 0).unwrap();
    module.write(0x90u8).unwrap();
    for size in [0, 1, 3] {
        module
            .create_jump_table(size, Reg::new(12), Reg::new(13), false, |labels| {
                labels.fill(label)
            })
            .unwrap();
    }
    module.finish_func().unwrap();
    assert_eq!(module.offset(), 72);
    let bytes = module.data().unwrap();
    assert_eq!(word(bytes, 4), 56 - 1 - 7);
    assert_eq!(word(bytes, 22), 56 - 19 - 7);
    assert_eq!(word(bytes, 40), 60 - 37 - 7);
    assert_eq!(word(bytes, 56), 56);
    assert_eq!([word(bytes, 60), word(bytes, 64), word(bytes, 68)], [60; 3]);
}

#[test]
fn all_gp_pairs_preserve_exact_table_capacity() {
    fixture!(module, writer, assembler, context);
    for index in 0..16 {
        if index == 4 {
            continue;
        }
        for temporary in 0..16 {
            module.begin_func(1, 8).unwrap();
            let label = module.label_create().unwrap();
            let offset = module.offset() as u32;
            module.label_place(label, offset).unwrap();
            let code = module.offset();
            module
                .create_jump_table(1, Reg::new(index), Reg::new(temporary), false, |labels| {
                    labels.fill(label)
                })
                .unwrap();
            module.finish_func().unwrap();
            let table = (code + 21) & !3;
            assert_eq!(module.offset(), table + 4);
            assert_eq!(word(module.data().unwrap(), table), (table - code) as u32);
            assert_eq!(
                word(module.data().unwrap(), code + 3),
                (table - code - 7) as u32
            );
        }
    }
}

#[test]
fn invalid_table_registers_do_not_allocate_or_fill() {
    fixture!(module, writer, assembler, context);
    for (index, temporary) in [(4, 0), (16, 0), (255, 0), (0, 16), (0, 255)] {
        let mut called = false;
        assert!(module
            .create_jump_table(1, Reg::new(index), Reg::new(temporary), false, |_| called =
                true)
            .is_err());
        assert!(!called);
        assert_eq!(module.offset(), 0);
        assert_eq!(module.jump_table_count(), 0);
    }
    let label = module.label_create().unwrap();
    module.label_place(label, 0).unwrap();
    module
        .create_jump_table(1, Reg::new(12), Reg::new(13), false, |labels| {
            labels.fill(label)
        })
        .unwrap();
    module.finish_func().unwrap();
}

#[test]
fn table_target_refusal_can_be_abandoned_and_reset() {
    fixture!(module, writer, assembler, context);
    let label = module.label_create().unwrap();
    module.label_place(label, 4096).unwrap();
    module
        .create_jump_table(1, Reg::new(0), Reg::new(1), false, |labels| {
            labels.fill(label)
        })
        .unwrap();
    assert!(module.finish_func().is_err());
    module.begin_func(1, 8).unwrap();
    assert_eq!(module.jump_table_count(), 0);
    assert!(module.label_fixups().is_empty());
    let label = module.label_create().unwrap();
    let offset = module.offset() as u32;
    module.label_place(label, offset).unwrap();
    module
        .create_jump_table(1, Reg::new(0), Reg::new(1), false, |labels| {
            labels.fill(label)
        })
        .unwrap();
    module.finish_func().unwrap();
}

#[test]
fn tables_survive_growth_and_reuse_after_callback_unwind() {
    fixture!(module, writer, assembler, context);
    let label = module.label_create().unwrap();
    module.label_place(label, 0).unwrap();
    for _ in 0..35 {
        module
            .create_jump_table(17, Reg::new(0), Reg::new(1), false, |labels| {
                labels.fill(label)
            })
            .unwrap();
    }
    module.write_bytes(&vec![0x90; 4097]).unwrap();
    module.finish_func().unwrap();
    assert_eq!(module.jump_table_count(), 35);
    assert_eq!(module.jump_table_labels(34).unwrap(), &[label; 17]);
    let first_storage = module.jump_table_labels(0).unwrap().as_ptr().addr();
    module.begin_func(1, 8).unwrap();
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        module
            .create_jump_table(17, Reg::new(0), Reg::new(1), false, |_| {
                panic!("fill cancelled")
            })
            .unwrap();
    }));
    assert!(caught.is_err());
    module.begin_func(1, 8).unwrap();
    let next = module.label_create().unwrap();
    let offset = module.offset() as u32;
    module.label_place(next, offset).unwrap();
    module
        .create_jump_table(17, Reg::new(0), Reg::new(1), false, |labels| {
            labels.fill(next)
        })
        .unwrap();
    assert_eq!(
        module.jump_table_labels(0).unwrap().as_ptr().addr(),
        first_storage
    );
    module.finish_func().unwrap();
}

#[test]
fn allocation_refusal_preserves_existing_table_and_allows_reuse() {
    let context = MemoryContext::new("limited-x64-writer").with_limit(128 * 1024);
    let mut assembler = AssemblerElf::new(context.mcx(), Architecture::X86_64).unwrap();
    let text = assembler.get_default_section(SectionKind::Text).unwrap();
    let mut writer = FunctionWriterX64::new(context.mcx(), X64Target::new());
    let mut module = writer.begin_module(&mut assembler).unwrap();
    module.switch_section(text).unwrap();
    module.begin_func(1, 8).unwrap();
    let label = module.label_create().unwrap();
    module.label_place(label, 0).unwrap();
    module
        .create_jump_table(1, Reg::new(0), Reg::new(1), false, |labels| {
            labels.fill(label)
        })
        .unwrap();
    let before = module.offset();
    let mut called = false;
    assert!(module
        .create_jump_table(1 << 20, Reg::new(0), Reg::new(1), false, |_| called = true)
        .is_err());
    assert!(!called);
    assert_eq!(module.offset(), before);
    assert_eq!(module.jump_table_count(), 1);
    assert_eq!(module.jump_table_labels(0).unwrap(), &[label]);
    module.finish_func().unwrap();
    module.begin_func(1, 8).unwrap();
    module.write(0xc3u8).unwrap();
    module.finish_func().unwrap();
}

#[test]
fn text_growth_failure_retains_unfilled_table_until_abandonment() {
    let context = MemoryContext::new("x64-growth-budget").with_limit(128 * 1024);
    let mut assembler = AssemblerElf::new(context.mcx(), Architecture::X86_64).unwrap();
    let text = assembler.get_default_section(SectionKind::Text).unwrap();
    let mut writer = FunctionWriterX64::new(context.mcx(), X64Target::new());
    let mut module = writer.begin_module(&mut assembler).unwrap();
    module.switch_section(text).unwrap();
    module.begin_func(1, 8).unwrap();
    module
        .create_jump_table(0, Reg::new(0), Reg::new(1), false, |_| {})
        .unwrap();
    module.base.more_space(0).unwrap();
    let remaining = module.allocated_size() - module.offset();
    module.skip(remaining).unwrap();
    let layout = core::alloc::Layout::from_size_align(64, 8).unwrap();
    while context.mcx().alloc_uninit_bytes(layout).is_ok() {}
    let before = module.offset();
    let mut called = false;
    assert!(module
        .create_jump_table(1, Reg::new(0), Reg::new(1), false, |_| called = true)
        .is_err());
    assert!(!called);
    assert_eq!(module.offset(), before);
    assert_eq!(module.jump_table_count(), 2);
    assert_eq!(module.jump_table_header(1).unwrap().off as usize, before);
    module.set_offset(0).unwrap();
    module.base.begin_func().unwrap();
    assert_eq!(module.jump_table_count(), 0);
    module.write(0xc3u8).unwrap();
    module.finish_func().unwrap();
    module.flush().unwrap();
    assert_eq!(module.data().unwrap(), &[0xc3]);
}

#[test]
fn malformed_table_and_pre_skew_fixup_fail_without_panicking() {
    fixture!(module, writer, assembler, context);
    module.write_bytes(&[0x90; 8]).unwrap();
    let label = module.label_create().unwrap();
    module.label_place(label, 4).unwrap();
    module
        .label_ref(label, 0, LabelFixupKind::X64_JMP_OR_MEM_DISP)
        .unwrap();
    module.remove_prologue_bytes(0, 4).unwrap();
    assert!(module.finish_func().is_err());
    module.begin_func(1, 8).unwrap();
    module
        .create_jump_table(0, Reg::new(0), Reg::new(1), false, |_| {})
        .unwrap();
    module.jump_table_mut(0).unwrap().set_off(u32::MAX);
    assert!(module.finish_func().is_err());
    module.begin_func(1, 8).unwrap();
    module.write(0xc3u8).unwrap();
    module.finish_func().unwrap();
}
