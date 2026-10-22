// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use super::*;
use mcx::MemoryContext;
use std::{format, string::String};
use tpde_core::{
    Analyzer,
    ir_adaptor::{HighestValueIndexProvider, IrAdaptor, PhiRef},
};
#[path = "../tests/cases.rs"]
mod cases;

#[test]
fn parser_roster_and_atomic_failure() {
    let context = MemoryContext::new("test-ir-roster");
    let mut ir = TestIR::new(context.mcx());
    for case in cases::CASES {
        ir.clear();
        let result = ir.parse_ir(case.source);
        assert_eq!(result.is_ok(), case.accepted, "{}: {:?}", case.name, result);
        if !case.accepted {
            assert!(
                ir.functions.is_empty()
                    && ir.values.is_empty()
                    && ir.blocks.is_empty()
                    && ir.value_operands.is_empty()
            );
        }
    }
}

#[test]
fn parser_owns_names_and_append_preserves_previous_modules() {
    let context = MemoryContext::new("test-ir-own");
    let mut ir = TestIR::new(context.mcx());
    let source = String::from("first(%argument) {\nentry:\nret %argument\n}\n");
    ir.parse_ir(&source).unwrap();
    drop(source);
    ir.parse_ir("second()!\n").unwrap();
    assert_eq!(ir.name(ir.functions[0].name), "first");
    assert_eq!(ir.name(ir.values[0].name), "argument");
    assert_eq!(ir.functions.len(), 2);
    assert!(ir.parse_ir("third() {\nb:\nbr ^missing\n}\n").is_err());
    assert_eq!(ir.functions.len(), 2);
    ir.parse_ir("third()!\n").unwrap();
    assert_eq!(ir.name(ir.functions[2].name), "third");
}

#[test]
fn print_full_observable_program() {
    let context = MemoryContext::new("test-ir-print");
    let mut ir = TestIR::new(context.mcx());
    ir.parse_ir("f(%a) local {\nentry:\n%p = alloca 16, 8\n%x = call @g, %a\nbr ^exit\nexit:\n%y = phi [^entry, %x]\nret %y\n}\ng(%arg)!\n").unwrap();
    let mut text = String::new();
    ir.print(&mut text).unwrap();
    assert_eq!(
        text,
        "Printing IR\nLocal function f\n  Argument a\n  Block entry\n    Succ exit\n    Value p (alloca)\n      Op $16\n      Op $8\n    Value x (call)\n      Target g\n      Op a\n    Value  (br)\n      Op ^exit\n  Block exit\n    PHI y\n      x from entry\n    Value  (ret)\n      Op y\nExtern function g\n  Argument arg\n"
    );
}

#[test]
fn debug_trace_and_sink_failure() {
    struct Refuse;
    impl core::fmt::Write for Refuse {
        fn write_str(&mut self, _: &str) -> core::fmt::Result {
            Err(core::fmt::Error)
        }
    }
    let context = MemoryContext::new("test-ir-debug");
    let mut ir = TestIR::new(context.mcx());
    ir.parse_ir("f(%a) {\nentry:\nret %a\n}\n").unwrap();
    let mut text = String::new();
    ir.dump_debug(&mut text, true).unwrap();
    assert_eq!(
        text,
        "Dumping IR\nFunction 0: f\n  Arg 0->1\n  Arg 0: a\n  Block 0->1\n  Block 0: entry\n    Succ 1->1\n    Inst 1->2\n    Value 1 (ret): \n      Op 0: a\n"
    );
    assert!(ir.print(&mut Refuse).is_err());
    assert!(ir.dump_debug(&mut Refuse, false).is_err());
    ir.print(&mut String::new()).unwrap();
}

#[test]
fn eof_comments_and_unicode_are_defined() {
    let context = MemoryContext::new("test-ir-comment");
    let mut ir = TestIR::new(context.mcx());
    for source in [
        ";",
        "; comment",
        "f()! ; comment",
        "f() {\nb:\nterminate\n};",
    ] {
        ir.clear();
        ir.parse_ir(source).unwrap();
    }
    for source in ["é()!\n", "f(%é)!\n", "f() {\nβ:\nterminate\n}\n"] {
        ir.clear();
        assert!(ir.parse_ir(source).is_err());
    }
}

#[test]
fn duplicate_definitions_resolve_to_the_last_value() {
    let context = MemoryContext::new("test-ir-shadow");
    let mut ir = TestIR::new(context.mcx());
    ir.parse_ir("f(%a,%a) {\nb:\n%x = any\n%x = add %a, %a\nret %x\n}\n")
        .unwrap();
    assert_eq!(&*ir.value_operands, &[1, 1, 3]);
}

#[test]
fn phi_slots_missing_block_and_duplicate_first_match() {
    let context = MemoryContext::new("test-ir-phi");
    let mut ir = TestIR::new(context.mcx());
    ir.parse_ir(
        "f(%a,%b) {\nentry:\nbr ^exit\nexit:\n%x = phi [^entry, %a], [^entry, %b]\nret %x\n}\n",
    )
    .unwrap();
    let adaptor = TestIRAdaptor::new(&ir);
    let phi = adaptor.val_as_phi(IRValueRef(3));
    assert_eq!(phi.incoming_count(), 2);
    assert_eq!(phi.incoming_val_for_slot(1), IRValueRef(1));
    assert_eq!(phi.incoming_block_for_slot(1), IRBlockRef(0));
    assert_eq!(phi.incoming_val_for_block(IRBlockRef(0)), IRValueRef(0));
    assert_eq!(
        phi.incoming_val_for_block(IRBlockRef(99)),
        IRValueRef(u32::MAX)
    );
}

#[test]
fn adaptor_alloca_and_lifecycle_contracts() {
    let context = MemoryContext::new("test-ir-adaptor");
    let mut ir = TestIR::new(context.mcx());
    ir.parse_ir("f(%a!) {\nentry:\n%x = alloca 64, 16\nbr ^exit\nexit:\n%y = alloca 32, 8\nret %a\n}\ng()!\n").unwrap();
    let mut adaptor = TestIRAdaptor::new(&ir);
    adaptor.start_compile();
    assert_eq!(adaptor.funcs_to_compile().count(), 2);
    assert!(adaptor.func_extern(IRFuncRef(1)));
    assert!(adaptor.switch_func(IRFuncRef(0)));
    assert_eq!(
        adaptor.cur_static_allocas().collect::<std::vec::Vec<_>>(),
        [IRValueRef(1)]
    );
    assert_eq!(adaptor.val_alloca_size(IRValueRef(1)), 64);
    assert_eq!(adaptor.val_alloca_align(IRValueRef(1)), 16);
    assert!(adaptor.val_ignore_in_liveness_analysis(IRValueRef(3)));
    assert_eq!(adaptor.cur_highest_val_idx(), 4);
    adaptor.block_set_info(IRBlockRef(0), 77);
    adaptor.reset();
    assert_eq!(adaptor.cur_highest_val_idx(), 0);
    assert_eq!(adaptor.cur_func, 0);
    assert_eq!(adaptor.block_info(IRBlockRef(0)), 77);
    adaptor.end_compile();
}

#[test]
fn adaptor_default_keeps_all_allocas_out_of_liveness() {
    let context = MemoryContext::new("test-ir-alloca-default");
    let mut ir = TestIR::new(context.mcx());
    ir.parse_ir("f(%a) {\nentry:\n%small = alloca 16, 16\n%large = alloca 4294967295, 16\n%aligned = alloca 16, 32\nret %a\n}\n")
        .unwrap();
    let mut adaptor = TestIRAdaptor::new(&ir);
    adaptor.switch_func(IRFuncRef(0));
    for value in [IRValueRef(1), IRValueRef(2), IRValueRef(3)] {
        assert!(adaptor.val_ignore_in_liveness_analysis(value));
        assert_eq!(adaptor.inst_operands(IRInstRef(value.0)).count(), 0);
    }
    assert!(!adaptor.val_ignore_in_liveness_analysis(IRValueRef(0)));
    assert!(!adaptor.val_ignore_in_liveness_analysis(IRValueRef(4)));
}

#[test]
fn adaptor_static_alloca_policy_boundaries_without_allocation() {
    let context = MemoryContext::new("test-ir-alloca-policy");
    let mut ir = TestIR::new(context.mcx());
    for (limit, size, align, ignored) in [
        (0, 0, 1, true),
        (0, 1, 1, false),
        (0, 0, 32, false),
        (16, 15, 16, true),
        (16, 16, 16, true),
        (16, 17, 16, false),
        (16, 16, 32, false),
        (0x100000, 0x100000, 16, true),
        (0x100000, 0x100001, 16, false),
        (0x10000000, 0x10000000, 16, true),
        (0x10000000, 0x10000001, 16, false),
        (u32::MAX, u32::MAX, 16, true),
        (u32::MAX, u32::MAX, 32, false),
    ] {
        ir.clear();
        ir.parse_ir(&format!(
            "f(%a) {{\nentry:\n%p = alloca {}, {}\nret %a\n}}\n",
            size, align
        ))
        .unwrap();
        let used = context.used();
        let mut adaptor = TestIRAdaptor::new(&ir).with_static_alloca_limit(limit);
        adaptor.switch_func(IRFuncRef(0));
        assert_eq!(
            adaptor.val_ignore_in_liveness_analysis(IRValueRef(1)),
            ignored,
            "limit={limit} size={size} align={align}"
        );
        assert!(!adaptor.val_ignore_in_liveness_analysis(IRValueRef(0)));
        assert!(!adaptor.val_ignore_in_liveness_analysis(IRValueRef(2)));
        let mut allocas = adaptor.cur_static_allocas();
        assert_eq!(allocas.next(), Some(IRValueRef(1)));
        assert_eq!(allocas.next(), None);
        assert_eq!(adaptor.inst_operands(IRInstRef(1)).count(), 0);
        assert_eq!(context.used(), used);
    }
}

#[test]
fn analyzer_tracks_promoted_allocas_and_preserves_policy_on_reuse() {
    let context = MemoryContext::new("test-ir-promoted-alloca-analysis");
    let mut ir = TestIR::new(context.mcx());
    ir.parse_ir("f(%a) {\nentry:\n%static = alloca 16, 16\n%large = alloca 17, 16\n%aligned = alloca 16, 32\n%unused = alloca 32, 16\nbr ^exit\nexit:\n%x = add %large, %aligned\n%y = add %x, %large\n%z = add %y, %static\nret %z\n}\ng() {\nentry:\n%p = alloca 16, 32\nret %p\n}\nh() {\nentry:\n%p = alloca 16, 16\nret %p\n}\n")
        .unwrap();
    let mut adaptor = TestIRAdaptor::new(&ir);
    adaptor.switch_func(IRFuncRef(0));
    let mut analyzer = Analyzer::new(context.mcx());
    analyzer.switch_func(&adaptor, IRFuncRef(0)).unwrap();
    for index in 1..=4 {
        assert_ne!(analyzer.liveness[index].epoch, analyzer.liveness_epoch);
    }

    adaptor = adaptor.with_static_alloca_limit(16);
    analyzer.switch_func(&adaptor, IRFuncRef(0)).unwrap();
    assert_eq!(&*analyzer.block_layout, &[IRBlockRef(0), IRBlockRef(1)]);
    assert_ne!(analyzer.liveness[1].epoch, analyzer.liveness_epoch);
    for (index, refs, last) in [(2, 3, 1), (3, 2, 1), (4, 1, 0)] {
        let info = analyzer.liveness_info(tpde_base::ValLocalIdx::new(index));
        assert_eq!(info.ref_count, refs);
        assert_eq!(info.first.0, 0);
        assert_eq!(info.last.0, last);
    }
    assert_eq!(adaptor.cur_static_allocas().count(), 4);

    adaptor.reset();
    adaptor.switch_func(IRFuncRef(1));
    analyzer.switch_func(&adaptor, IRFuncRef(1)).unwrap();
    let pointer = IRValueRef(ir.functions[1].arg_begin_idx);
    assert!(!adaptor.val_ignore_in_liveness_analysis(pointer));
    let info = analyzer.liveness_info(adaptor.val_local_idx(pointer));
    assert_eq!(info.ref_count, 2);
    assert_eq!(info.first.0, 0);
    assert_eq!(info.last.0, 0);

    adaptor.reset();
    adaptor.switch_func(IRFuncRef(2));
    analyzer.switch_func(&adaptor, IRFuncRef(2)).unwrap();
    assert!(adaptor.val_ignore_in_liveness_analysis(IRValueRef(ir.functions[2].arg_begin_idx)));
    assert_ne!(analyzer.liveness[0].epoch, analyzer.liveness_epoch);

    adaptor.switch_func(IRFuncRef(1));
    analyzer.switch_func(&adaptor, IRFuncRef(1)).unwrap();
    let info = analyzer.liveness_info(adaptor.val_local_idx(pointer));
    assert_eq!(info.ref_count, 2);
    assert_eq!(info.first.0, 0);
    assert_eq!(info.last.0, 0);
}

#[test]
fn actual_analyzer_from_runtime_text() {
    let context = MemoryContext::new("test-ir-analysis");
    let mut ir = TestIR::new(context.mcx());
    ir.parse_ir("f(%a) {\nentry:\nbr ^exit\nexit:\nret %a\n}\n")
        .unwrap();
    let mut adaptor = TestIRAdaptor::new(&ir);
    adaptor.switch_func(IRFuncRef(0));
    let mut analyzer = Analyzer::new(context.mcx());
    analyzer.switch_func(&adaptor, IRFuncRef(0)).unwrap();
    assert_eq!(&*analyzer.block_layout, &[IRBlockRef(0), IRBlockRef(1)]);
    let arg = analyzer.liveness_info(tpde_base::ValLocalIdx::new(0));
    assert_eq!(arg.ref_count, 2);
    assert_eq!(arg.first.0, 0);
    assert_eq!(arg.last.0, 1);
}

#[test]
fn parse_success_does_not_claim_dominance() {
    let context = MemoryContext::new("test-ir-dominance-boundary");
    let mut ir = TestIR::new(context.mcx());
    ir.parse_ir("f() {\nb:\n%x = add %y, %y\n%y = any\nret %x\n}\n")
        .unwrap();
    assert_eq!(&*ir.value_operands, &[1, 1, 0]);
}

#[test]
fn allocation_refusal_rolls_back_then_reuses() {
    let source = cases::linear(100);
    let mut refusals = 0;
    for budget in [1, 64, 256, 1024, 4096, 16384] {
        let context = MemoryContext::new("test-ir-budget").with_limit(budget);
        let mut ir = TestIR::new(context.mcx());
        if ir.parse_ir(&source).is_err() {
            refusals += 1;
            assert!(ir.functions.is_empty() && ir.values.is_empty());
            ir.release();
            ir.parse_ir("").unwrap();
            if budget >= 4096 {
                ir.parse_ir("f() {\nb:\nterminate\n}\n").unwrap();
            }
        }
    }
    assert!(refusals >= 3);
}

#[test]
fn retained_storage_and_no_drop_records() {
    let context = MemoryContext::new("test-ir-reuse");
    let mut ir = TestIR::new(context.mcx());
    ir.parse_ir(&cases::linear(100)).unwrap();
    let retained = ir.retained_bytes();
    let values = ir.values.as_ptr();
    ir.clear();
    ir.parse_ir("f() {\nb:\nterminate\n}\n").unwrap();
    assert_eq!(ir.values.as_ptr(), values);
    assert_eq!(ir.retained_bytes(), retained);
    ir.release();
    assert_eq!(ir.retained_bytes(), 0);
    assert!(!core::mem::needs_drop::<TestIR<'_>>());
    assert!(!core::mem::needs_drop::<Value>());
    assert!(!core::mem::needs_drop::<Block>());
    assert!(!core::mem::needs_drop::<Function>());
}

#[test]
fn condition_numbers_and_metadata_boundaries() {
    assert_eq!(
        [
            Cond::Eq as u8,
            Cond::Neq as u8,
            Cond::Uge as u8,
            Cond::Ugt as u8,
            Cond::Ule as u8,
            Cond::Ult as u8,
            Cond::Sge as u8,
            Cond::Sgt as u8,
            Cond::Sle as u8,
            Cond::Slt as u8
        ],
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]
    );
    let context = MemoryContext::new("test-ir-cond");
    let mut ir = TestIR::new(context.mcx());
    for cond in 0..10 {
        ir.clear();
        ir.parse_ir(&format!(
            "f(%a) {{\nb:\n%x = condselect %a, %a, %a, %a, {}\nret %x\n}}\n",
            cond
        ))
        .unwrap();
        assert_eq!(ir.value_operands[4], cond);
    }
}

#[test]
fn bump_parser_scratch_stabilizes_across_success_and_failure() {
    let context = MemoryContext::new_bump("test-ir-bump-reuse");
    let mut ir = TestIR::new(context.mcx());
    let source = cases::linear(100);
    let malformed = source.replace("ret %x99", "ret %missing");
    ir.parse_ir(&source).unwrap();
    let used = context.used();
    let retained = ir.retained_bytes();
    for _ in 0..2 {
        ir.clear();
        assert!(ir.parse_ir(&malformed).is_err());
        assert!(ir.functions.is_empty());
        assert_eq!(context.used(), used);
        ir.parse_ir(&source).unwrap();
        assert_eq!(context.used(), used);
        assert_eq!(ir.retained_bytes(), retained);
    }
    ir.release();
    assert_eq!(ir.retained_bytes(), 0);
    assert_eq!(context.used(), used);
}

#[test]
fn allocset_release_reclaims_all_parser_buffers() {
    let context = MemoryContext::new("test-ir-release-accounting");
    let initial = context.used();
    let mut ir = TestIR::new(context.mcx());
    ir.parse_ir(&cases::linear(100)).unwrap();
    assert!(context.used() > initial);
    ir.release();
    assert_eq!(context.used(), initial);
    ir.parse_ir("f() {\nb:\nterminate\n}\n").unwrap();
    ir.release();
    assert_eq!(context.used(), initial);
}

#[test]
fn appended_failure_preserves_every_record_and_metadata() {
    let context = MemoryContext::new("test-ir-prefix");
    let mut ir = TestIR::new(context.mcx());
    ir.parse_ir("f(%a) {\nb:\nret %a\n}\n").unwrap();
    ir.blocks[0].block_info.set(123);
    ir.blocks[0].block_info2.set(456);
    let values = ir.values.to_vec();
    let operands = ir.value_operands.to_vec();
    let blocks = ir.blocks.to_vec();
    let functions = ir.functions.to_vec();
    for source in [
        "g(%a) {\nb:\nbr ^missing\n}\n",
        "g(%a) {\nb:\nret %missing\n}\n",
        "g(%a) {\nb:\n%x = call @missing, %a\nret %x\n}\n",
    ] {
        let error = ir.parse_ir(source).unwrap_err();
        assert_eq!(error.sqlstate, types_error::ERRCODE_INVALID_PARAMETER_VALUE);
        assert_eq!(&*ir.values, &values);
        assert_eq!(&*ir.value_operands, &operands);
        assert_eq!(&*ir.blocks, &blocks);
        assert_eq!(&*ir.functions, &functions);
    }
    ir.parse_ir("g(%a) {\nb:\nret %a\n}\n").unwrap();
    assert_eq!(ir.name(ir.functions[1].name), "g");
}

#[test]
fn cancelled_output_preserves_ir_for_next_sink() {
    struct CancelAfter(usize);
    impl core::fmt::Write for CancelAfter {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            self.0 = self.0.checked_sub(text.len()).ok_or(core::fmt::Error)?;
            Ok(())
        }
    }
    let context = MemoryContext::new("test-ir-cancel-output");
    let mut ir = TestIR::new(context.mcx());
    ir.parse_ir(&cases::linear(4)).unwrap();
    let mut expected = String::new();
    ir.print(&mut expected).unwrap();
    for boundary in [1, 11, 32, 100, expected.len() - 1] {
        assert!(ir.print(&mut CancelAfter(boundary)).is_err());
        let mut actual = String::new();
        ir.print(&mut actual).unwrap();
        assert_eq!(actual, expected);
    }
}
