// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use super::*;
#[path = "analyzer_fixture.rs"]
mod fixture;
use crate::ir_adaptor::{NoHighestValue, ProvidesHighestValue};
use fixture::{Fixture, Program};
use mcx::MemoryContext;
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    string::String,
    vec,
    vec::Vec,
};

fn metadata(program: &Program) -> Vec<(u32, u32)> {
    program
        .blocks
        .iter()
        .map(|block| (block.info.get(), block.info2.get()))
        .collect()
}

#[test]
fn plain_layout_accessors_and_no_drop_records() {
    let context = MemoryContext::new_bump("analyzer");
    let program = Program::linear(3);
    let fixture = <Fixture<'_>>::new(&program);
    let mut analyzer = Analyzer::new(context.mcx());
    analyzer.switch_func(&fixture, u32::MAX).unwrap();
    assert_eq!(&*analyzer.block_layout, &[0, 1, 2]);
    assert_eq!(&*analyzer.block_loop_map, &[0, 0, 0]);
    assert_eq!(analyzer.num_insts, 3);
    assert_eq!(program.successor_live.get(), 0);
    assert!(program.successor_drops.get() >= 6);
    assert_eq!(
        analyzer.loops[0],
        Loop {
            level: 0,
            parent: 0,
            begin: BlockIndex(0),
            end: BlockIndex(3),
            num_blocks: 3,
            definitions: 3,
            definitions_in_childs: 0
        }
    );
    assert_eq!(analyzer.block_ref(BlockIndex(3)), u32::MAX);
    assert_eq!(analyzer.block_idx(&fixture, 2), BlockIndex(2));
    assert_eq!(analyzer.block_loop_idx(BlockIndex(2)), 0);
    assert_eq!(analyzer.loop_from_idx(0), &analyzer.loops[0]);
    assert!(!analyzer.block_has_multiple_incoming(&fixture, BlockIndex(1)));
    assert!(!analyzer.block_ref_has_multiple_incoming(&fixture, 1));
    assert!(!analyzer.block_has_phis(&fixture, BlockIndex(1)));
    assert!(!analyzer.block_ref_has_phis(&fixture, 1));
    assert_eq!(analyzer.liveness_info(ValLocalIdx::new(0)).ref_count, 2);
    assert_eq!(
        analyzer.liveness_info(ValLocalIdx::new(1)).last,
        BlockIndex(1)
    );
    assert!(!core::mem::needs_drop::<Analyzer<'_, Fixture<'_>>>());
    assert!(!core::mem::needs_drop::<Loop>());
    assert!(!core::mem::needs_drop::<LivenessInfo>());
    assert_eq!(core::mem::size_of::<BlockIndex>(), 4);
}

fn policy_case<H, const VISIT: bool>()
where
    for<'a> H: HighestValuePolicy<Fixture<'a, H, VISIT>>,
{
    let context = MemoryContext::new_bump("analyzer-policy");
    let program = Program::linear(3);
    let fixture = <Fixture<'_, H, VISIT>>::new(&program);
    let mut analyzer = Analyzer::new(context.mcx());
    analyzer.switch_func(&fixture, 0).unwrap();
    assert_eq!(
        analyzer.liveness_info(ValLocalIdx::new(0)).ref_count,
        1 + u32::from(VISIT)
    );
    assert_eq!(analyzer.liveness_max_value, 3);
    assert_eq!(
        analyzer.liveness.len(),
        if <H as HighestValuePolicy<Fixture<'_, H, VISIT>>>::PROVIDED {
            4
        } else if VISIT {
            256
        } else {
            257
        }
    );
}
#[test]
fn capability_and_argument_policies_are_independent() {
    policy_case::<ProvidesHighestValue, true>();
    policy_case::<ProvidesHighestValue, false>();
    policy_case::<NoHighestValue, true>();
    policy_case::<NoHighestValue, false>();
}

#[test]
fn functions_reuse_storage_and_epoch_entries() {
    let context = MemoryContext::new("analyzer-reuse");
    let large = Program::linear(200);
    let small = Program::linear(3);
    let mut analyzer = Analyzer::new(context.mcx());
    analyzer
        .switch_func(&<Fixture<'_>>::new(&large), 0)
        .unwrap();
    let layout_ptr = analyzer.block_layout.as_ptr();
    let live_ptr = analyzer.liveness.as_ptr();
    let live_len = analyzer.liveness.len();
    analyzer
        .switch_func(&<Fixture<'_>>::new(&small), 0)
        .unwrap();
    assert_eq!(analyzer.liveness_epoch, 2);
    assert_eq!(analyzer.liveness.len(), live_len);
    assert_eq!(analyzer.liveness[200].epoch, 1);
    assert_eq!(analyzer.block_layout.as_ptr(), layout_ptr);
    assert_eq!(analyzer.liveness.as_ptr(), live_ptr);
    let used = context.stats().used;
    for _ in 0..3 {
        analyzer
            .switch_func(&<Fixture<'_>>::new(&small), 0)
            .unwrap();
        assert_eq!(context.stats().used, used);
    }
    assert_eq!(&*analyzer.block_layout, &[0, 1, 2]);
    assert_eq!(analyzer.loops[0].definitions, 3);
    assert_eq!(large.successor_live.get(), 0);
    assert_eq!(small.successor_live.get(), 0);
}

#[test]
fn epoch_wrap_clears_stale_info_before_reinitialization() {
    let context = MemoryContext::new_bump("analyzer-epoch");
    let program = Program::linear(4);
    let fixture = <Fixture<'_>>::new(&program);
    let mut analyzer = Analyzer::new(context.mcx());
    analyzer.switch_func(&fixture, 0).unwrap();
    analyzer.liveness[1].ref_count = 991;
    analyzer.liveness_epoch = u16::MAX;
    analyzer.switch_func(&fixture, 0).unwrap();
    assert_eq!(analyzer.liveness_epoch, 1);
    assert_eq!(analyzer.liveness_info(ValLocalIdx::new(1)).ref_count, 2);
    assert_eq!(analyzer.liveness_info(ValLocalIdx::new(4)).ref_count, 1);
}

#[test]
fn ignored_results_count_definitions_and_fused_instructions_count() {
    let context = MemoryContext::new_bump("analyzer-ignored");
    let mut program = Program::linear(2);
    program.values[1].ignore = true;
    program.insts[0].fused = true;
    let fixture = <Fixture<'_>>::new(&program);
    let mut analyzer = Analyzer::new(context.mcx());
    analyzer.switch_func(&fixture, 0).unwrap();
    assert_eq!(analyzer.num_insts, 2);
    assert_eq!(analyzer.loops[0].definitions, 2);
    assert_ne!(analyzer.liveness[1].epoch, analyzer.liveness_epoch);
    if tpde_util::CHECKED {
        assert!(catch_unwind(AssertUnwindSafe(
            || analyzer.liveness_info(ValLocalIdx::new(1))
        ))
        .is_err());
    }
}

#[test]
fn unreachable_phi_predecessor_is_not_a_liveness_use() {
    let context = MemoryContext::new_bump("analyzer-phi");
    let mut program = Program::new("phi", 4);
    program.blocks[0].succs = vec![1, 1];
    program.blocks[1].succs = vec![2];
    let unreachable_value = program.value(false);
    let phi = program.value(false);
    program.values[phi as usize].incoming = vec![(1, 0), (3, unreachable_value)];
    program.blocks[2].phis.push(phi);
    let fixture = <Fixture<'_>>::new(&program);
    let mut analyzer = Analyzer::new(context.mcx());
    analyzer.switch_func(&fixture, 0).unwrap();
    assert_eq!(&*analyzer.block_layout, &[0, 1, 2]);
    assert!(analyzer.block_ref_has_multiple_incoming(&fixture, 1));
    assert!(analyzer.block_ref_has_phis(&fixture, 2));
    let info = analyzer.liveness_info(ValLocalIdx::new(phi));
    assert_eq!(
        (info.first, info.last, info.ref_count),
        (BlockIndex(1), BlockIndex(1), 1)
    );
    assert_ne!(
        analyzer.liveness[unreachable_value as usize].epoch,
        analyzer.liveness_epoch
    );
    assert_eq!(program.blocks[3].info2.get(), 0);
}

#[test]
fn empty_unvisited_liveness_has_defined_diagnostics() {
    let context = MemoryContext::new_bump("analyzer-empty-live");
    let mut program = Program::new("empty", 1);
    program.args.clear();
    let fixture = <Fixture<'_, NoHighestValue>>::new(&program);
    let mut analyzer = Analyzer::new(context.mcx());
    analyzer.switch_func(&fixture, 0).unwrap();
    assert!(analyzer.liveness.is_empty());
    let mut text = String::new();
    analyzer.print_liveness(&fixture, &mut text).unwrap();
    assert_eq!(text, "  0: ignored\n");
}

#[test]
fn invalid_cfg_discards_results_and_metadata_then_can_retry() {
    let context = MemoryContext::new_bump("analyzer-invalid");
    let valid = Program::linear(3);
    let mut invalid_program = Program::linear(4);
    invalid_program.blocks[3].succs.push(0);
    let mut analyzer = Analyzer::new(context.mcx());
    analyzer
        .switch_func(&<Fixture<'_>>::new(&valid), 0)
        .unwrap();
    assert!(analyzer
        .switch_func(&<Fixture<'_>>::new(&invalid_program), 0)
        .is_err());
    assert!(analyzer.block_layout.is_empty());
    assert!(analyzer.loops.is_empty());
    assert!(analyzer.liveness.is_empty());
    assert_eq!(analyzer.liveness_epoch, 0);
    assert_eq!(metadata(&invalid_program), vec![(u32::MAX, 0); 4]);
    let invalid_fixture = <Fixture<'_>>::new(&invalid_program);
    assert_eq!(analyzer.block_idx(&invalid_fixture, 2), INVALID_BLOCK_IDX);
    assert!(!analyzer.block_ref_has_phis(&invalid_fixture, 2));
    analyzer
        .switch_func(&<Fixture<'_>>::new(&valid), 0)
        .unwrap();
    assert_eq!(analyzer.liveness_epoch, 1);
    assert_eq!(&*analyzer.block_layout, &[0, 1, 2]);
}

#[test]
fn empty_cfg_and_missing_entry_return_errors() {
    let context = MemoryContext::new_bump("analyzer-entry");
    let empty = Program::new("empty", 0);
    let mut missing = Program::linear(2);
    missing.entry = 19;
    let mut analyzer = Analyzer::new(context.mcx());
    assert!(analyzer
        .switch_func(&<Fixture<'_>>::new(&empty), 0)
        .is_err());
    assert!(analyzer
        .switch_func(&<Fixture<'_>>::new(&missing), 0)
        .is_err());
    assert_eq!(metadata(&missing), vec![(u32::MAX, 0); 2]);
}

#[test]
fn budget_failure_reuses_inline_state_for_a_smaller_function() {
    let context = MemoryContext::new("analyzer-oom").with_limit(0);
    let large = Program::linear(100);
    let small = Program::linear(3);
    let mut analyzer = Analyzer::new(context.mcx());
    assert!(analyzer
        .switch_func(&<Fixture<'_>>::new(&large), 0)
        .is_err());
    assert_eq!(context.stats().used, 0);
    assert_eq!(metadata(&large), vec![(u32::MAX, 0); 100]);
    analyzer
        .switch_func(&<Fixture<'_>>::new(&small), 0)
        .unwrap();
    assert_eq!(&*analyzer.block_layout, &[0, 1, 2]);
    assert_eq!(context.stats().used, 0);
}

#[test]
fn liveness_allocation_failure_and_growth_overflow_leave_retryable_state() {
    let context = MemoryContext::new("analyzer-live-oom").with_limit(0);
    let mut sparse = Program::linear(1);
    sparse.highest = 4096;
    let small = Program::linear(1);
    let mut analyzer = Analyzer::new(context.mcx());
    assert!(analyzer
        .switch_func(&<Fixture<'_>>::new(&sparse), 0)
        .is_err());
    analyzer
        .switch_func(&<Fixture<'_>>::new(&small), 0)
        .unwrap();
    let mut overflow = Program::linear(1);
    overflow.highest = u32::MAX;
    assert!(analyzer
        .switch_func(&<Fixture<'_>>::new(&overflow), 0)
        .is_err());
    let mut unchecked = Program::linear(1);
    unchecked.values[0].local_idx = u32::MAX - 100;
    let no_highest_context = MemoryContext::new_bump("analyzer-overflow-retry");
    let mut no_highest = Analyzer::new(no_highest_context.mcx());
    assert!(no_highest
        .switch_func(&<Fixture<'_, NoHighestValue>>::new(&unchecked), 0)
        .is_err());
    no_highest
        .switch_func(&<Fixture<'_, NoHighestValue>>::new(&small), 0)
        .unwrap();
}

struct FailingWriter;
impl fmt::Write for FailingWriter {
    fn write_str(&mut self, _: &str) -> fmt::Result {
        Err(fmt::Error)
    }
}
struct PanickingWriter;
impl fmt::Write for PanickingWriter {
    fn write_str(&mut self, _: &str) -> fmt::Result {
        panic!("diagnostic writer panic")
    }
}
#[test]
fn diagnostic_failure_and_panic_restore_exact_metadata() {
    let context = MemoryContext::new_bump("analyzer-print");
    let mut program = Program::new("print", 4);
    program.blocks[0].succs = vec![1];
    program.blocks[1].succs = vec![2, 3];
    program.blocks[2].succs = vec![1];
    let phi = program.value(false);
    program.values[phi as usize].incoming = vec![(0, 0), (2, 0)];
    program.blocks[1].phis.push(phi);
    let fixture = <Fixture<'_>>::new(&program);
    let mut analyzer = Analyzer::new(context.mcx());
    analyzer.switch_func(&fixture, 0).unwrap();
    let before = metadata(&program);
    assert!(analyzer.print_rpo(&fixture, &mut FailingWriter).is_err());
    assert_eq!(metadata(&program), before);
    assert!(catch_unwind(AssertUnwindSafe(
        || analyzer.print_rpo(&fixture, &mut PanickingWriter)
    ))
    .is_err());
    assert_eq!(metadata(&program), before);
    let mut out = String::new();
    analyzer.print_rpo(&fixture, &mut out).unwrap();
    assert_eq!(metadata(&program), before);
    assert!(out.starts_with("  0: 0\n"));
    assert!(analyzer
        .print_block_layout(&fixture, &mut FailingWriter)
        .is_err());
    assert!(analyzer.print_loops(&fixture, &mut FailingWriter).is_err());
    assert!(analyzer
        .print_liveness(&fixture, &mut FailingWriter)
        .is_err());
    assert_eq!(metadata(&program), before);
}

#[test]
fn diagnostic_allocation_failure_restores_saved_metadata() {
    let context = MemoryContext::new("analyzer-print-oom").with_limit(0);
    let mut program = Program::new("wide", 34);
    program.blocks[0].succs = (1..34).collect();
    for (index, block) in program.blocks.iter().enumerate() {
        block.info.set(100 + index as u32);
        block.info2.set(70 + index as u32);
    }
    let fixture = <Fixture<'_>>::new(&program);
    let mut analyzer = Analyzer::new(context.mcx());
    let before = metadata(&program);
    assert!(analyzer.print_rpo(&fixture, &mut String::new()).is_err());
    assert_eq!(metadata(&program), before);
}

#[test]
fn out_of_range_accessors_panic_without_mutating_analysis() {
    let context = MemoryContext::new_bump("analyzer-indices");
    let program = Program::linear(1);
    let fixture = <Fixture<'_>>::new(&program);
    let mut analyzer = Analyzer::new(context.mcx());
    analyzer.switch_func(&fixture, 0).unwrap();
    assert!(catch_unwind(AssertUnwindSafe(|| analyzer.block_ref(BlockIndex(2)))).is_err());
    assert!(catch_unwind(AssertUnwindSafe(|| analyzer.block_loop_idx(BlockIndex(1)))).is_err());
    assert!(catch_unwind(AssertUnwindSafe(|| analyzer.loop_from_idx(1))).is_err());
    assert!(catch_unwind(AssertUnwindSafe(
        || analyzer.liveness_info(ValLocalIdx::new(99))
    ))
    .is_err());
    assert_eq!(&*analyzer.block_layout, &[0]);
}

#[test]
fn borrowed_successor_resources_drop_when_dfs_growth_fails() {
    let context = MemoryContext::new("analyzer-dfs-oom").with_limit(4096);
    let deep = Program::linear(65);
    let small = Program::linear(3);
    let mut analyzer = Analyzer::new(context.mcx());
    assert!(analyzer.switch_func(&<Fixture<'_>>::new(&deep), 0).is_err());
    assert_eq!(deep.successor_live.get(), 0);
    assert_eq!(deep.successor_drops.get(), 130);
    assert_eq!(metadata(&deep), vec![(u32::MAX, 0); 65]);
    analyzer
        .switch_func(&<Fixture<'_>>::new(&small), 0)
        .unwrap();
    assert_eq!(small.successor_live.get(), 0);
    assert_eq!(&*analyzer.block_layout, &[0, 1, 2]);
}

#[test]
fn nested_and_irreducible_loops_keep_each_block_and_definition() {
    let context = MemoryContext::new_bump("analyzer-nested");
    let mut nested = Program::linear(6);
    nested.blocks[0].succs = vec![1];
    nested.blocks[1].succs = vec![2, 5];
    nested.blocks[2].succs = vec![3, 4];
    nested.blocks[3].succs = vec![2];
    nested.blocks[4].succs = vec![1];
    nested.blocks[5].succs.clear();
    let mut irreducible = Program::linear(5);
    irreducible.blocks[0].succs = vec![1, 2];
    irreducible.blocks[1].succs = vec![3];
    irreducible.blocks[2].succs = vec![3];
    irreducible.blocks[3].succs = vec![1, 4];
    irreducible.blocks[4].succs.clear();
    let mut analyzer = Analyzer::new(context.mcx());
    for program in [&nested, &irreducible] {
        let fixture = <Fixture<'_>>::new(program);
        analyzer.switch_func(&fixture, 0).unwrap();
        let mut blocks = analyzer.block_layout.to_vec();
        blocks.sort_unstable();
        assert_eq!(blocks, (0..program.blocks.len() as u32).collect::<Vec<_>>());
        assert_eq!(analyzer.loops[0].num_blocks as usize, program.blocks.len());
        assert_eq!(
            (analyzer.loops[0].definitions + analyzer.loops[0].definitions_in_childs) as usize,
            program.insts.len()
        );
        assert_eq!(program.successor_live.get(), 0);
        for info in analyzer.loops.iter().skip(1) {
            let parent = &analyzer.loops[info.parent as usize];
            assert_eq!(parent.level + 1, info.level);
            assert!(parent.begin <= info.begin && parent.end >= info.end);
            assert_eq!(info.end.0 - info.begin.0, info.num_blocks);
        }
    }
}

std::thread_local! {
    static TRACE: core::cell::RefCell<String> = const { core::cell::RefCell::new(String::new()) };
}
fn trace_sink(_: tpde_base::Level, message: fmt::Arguments<'_>) {
    use core::fmt::Write as _;
    TRACE.with(|trace| writeln!(trace.borrow_mut(), "{}", message).unwrap());
}
#[test]
fn optional_trace_logging_visits_real_analysis_paths() {
    let context = MemoryContext::new_bump("analyzer-traces");
    let mut program = Program::linear(3);
    program.values[1].ignore = true;
    let fixture = <Fixture<'_>>::new(&program);
    let mut analyzer = Analyzer::new(context.mcx());
    TRACE.with(|trace| trace.borrow_mut().clear());
    let old = tpde_base::set_log_config(tpde_base::LogConfig {
        level: tpde_base::Level::Trace,
        sink: trace_sink,
    });
    let result = catch_unwind(AssertUnwindSafe(|| {
        analyzer.switch_func(&fixture, 0).unwrap();
        analyzer.print_rpo(&fixture, &mut String::new()).unwrap();
    }));
    tpde_base::set_log_config(old);
    result.unwrap();
    TRACE.with(|trace| {
        let trace = trace.borrow();
        if tpde_base::LOGGING && cfg!(debug_assertions) {
            assert!(trace.contains("Finished building RPO for blocks:"));
            assert!(trace.contains("Index 0: 0"));
            assert!(trace.contains("Starting Liveness Analysis"));
            assert!(trace.contains("Visiting value 0 in block 0"));
            assert!(trace.contains("value is ignored"));
            assert!(trace.contains("increasing ref_count to 2"));
            assert!(trace.contains("Analyzing instruction 0"));
            assert!(trace.contains("Finished Liveness Analysis"));
        } else {
            assert!(trace.is_empty());
        }
    });
}
