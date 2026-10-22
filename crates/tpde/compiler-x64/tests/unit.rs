// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#[path = "fixture.rs"]
mod fixture;
#[path = "semantics.rs"]
mod semantics;

use std::panic::{catch_unwind, AssertUnwindSafe};
use tpde_compiler_x64::{CCAssignerSysV, CompilerX64, Jump, X64CompilerState, CC_INFO};
use tpde_core::{
    calling_convention::{CCAssignment, CCAssignmentPolicy},
    RegBank,
};

fn argument(bank: u8, size: u32, align: u8) -> CCAssignment {
    let mut value = CCAssignment::new();
    value.bank = RegBank::new(bank);
    value.size = size;
    value.align = align;
    value
}
#[test]
fn sysv_gp_order_and_stack_alignment() {
    let mut cc = CCAssignerSysV::new(false);
    for reg in [7, 6, 2, 1, 8, 9] {
        let mut arg = argument(0, 8, 8);
        cc.assign_arg(&mut arg);
        assert_eq!(arg.reg.id(), reg);
    }
    let mut seventh = argument(0, 8, 16);
    cc.assign_arg(&mut seventh);
    assert!(seventh.reg.invalid());
    assert_eq!(seventh.stack_off, 0);
    let mut eighth = argument(0, 8, 32);
    cc.assign_arg(&mut eighth);
    assert_eq!(eighth.stack_off, 32);
    assert_eq!(cc.get_stack_size(), 40);
}
#[test]
fn sysv_vector_exhaustion_uses_sized_slots() {
    let mut cc = CCAssignerSysV::new(false);
    for id in 32..40 {
        let mut arg = argument(1, 8, 8);
        cc.assign_arg(&mut arg);
        assert_eq!(arg.reg.id(), id);
    }
    let mut small = argument(1, 4, 4);
    cc.assign_arg(&mut small);
    assert_eq!(small.stack_off, 0);
    let mut wide = argument(1, 16, 16);
    cc.assign_arg(&mut wide);
    assert_eq!(wide.stack_off, 16);
    assert_eq!(cc.get_stack_size(), 32);
}
#[test]
fn sysv_byval_preserves_pending_group_and_register_counts() {
    let mut cc = CCAssignerSysV::new(false);
    for _ in 0..5 {
        cc.assign_arg(&mut argument(0, 8, 8));
    }
    let mut pair = argument(0, 8, 8);
    pair.consecutive = 1;
    cc.assign_arg(&mut pair);
    assert!(pair.reg.invalid());
    let mut byval = argument(0, 3, 1);
    byval.set_byval(true);
    cc.assign_arg(&mut byval);
    assert_eq!(byval.stack_off, 8);
    let mut next = argument(0, 8, 8);
    cc.assign_arg(&mut next);
    assert!(next.reg.invalid());
    assert_eq!(next.stack_off, 16);
}
#[test]
fn sysv_reset_clears_vararg_and_both_return_banks() {
    let mut cc = CCAssignerSysV::new(true);
    for (bank, id) in [(0, 0), (1, 32), (0, 2), (1, 33)] {
        let mut a = argument(bank, 8, 8);
        cc.assign_ret(&mut a);
        assert_eq!(a.reg.id(), id);
    }
    cc.reset();
    assert!(!cc.is_vararg());
    assert_eq!(cc.get_stack_size(), 0);
    let mut a = argument(0, 8, 8);
    cc.assign_ret(&mut a);
    assert_eq!(a.reg.id(), 0);
}
#[test]
fn unsupported_return_count_fails_loudly() {
    if !tpde_util::CHECKED {
        return;
    }
    let mut cc = CCAssignerSysV::new(false);
    cc.assign_ret(&mut argument(0, 8, 8));
    cc.assign_ret(&mut argument(0, 8, 8));
    assert!(catch_unwind(AssertUnwindSafe(|| cc.assign_ret(&mut argument(0, 8, 8)))).is_err());
}
#[test]
fn stack_size_overflow_fails_before_wrapping() {
    let mut cc = CCAssignerSysV::new(false);
    let mut large = argument(0, u32::MAX, 1);
    large.set_byval(true);
    cc.assign_arg(&mut large);
    let mut next = argument(0, 1, 8);
    next.set_byval(true);
    assert!(catch_unwind(AssertUnwindSafe(|| cc.assign_arg(&mut next))).is_err());
}
#[test]
fn feature_masks_and_conditions_keep_architecture_values() {
    use tpde_compiler_x64::{emission, CPU_AVX, CPU_V2, CPU_V3, CPU_V4};
    assert_eq!((CPU_V2, CPU_V3, CPU_V4), (63, 16383, 524287));
    let state = tpde_compiler_x64::X64State::new(CPU_V3);
    assert!(state.has_cpu_feats(CPU_AVX));
    assert!(!state.has_cpu_feats(CPU_V4));
    for jump in [Jump::jo, Jump::jne, Jump::jge, Jump::jp] {
        assert_eq!(emission::invert_jump(emission::invert_jump(jump)), jump);
    }
    assert_eq!(emission::swap_jump(Jump::jg), Jump::jl);
    assert_eq!(CC_INFO.red_zone_size(), 128);
}
#[test]
fn complete_leaf_pipeline_emits_return_value_and_ret() {
    let output = fixture::run("init 0 0 0 1\nconstant 0 8 0 37 0\nepilog\nreset\n");
    assert_eq!(semantics::text(&output), [0xb8, 37, 0, 0, 0, 0xc3]);
    assert!(output.contains("reset 0 0 0 0 0\n"));
}
#[test]
fn stack_frame_and_dynamic_call_pipeline_complete() {
    let output=fixture::run("init 0 0 1 0\nafixed 24 32\ncallbegin 0\ncallarg 0 15 8 0 1 0 0 4096 0\ncalltarget 0 7\ncallend\nepilog\n");
    assert!(output.contains("compile 1\n"));
    assert_eq!(semantics::text(&output).last(), Some(&0xc3));
}
#[test]
fn vararg_tls_pipeline_reuses_resolver_symbol() {
    let output = fixture::run("init 0 1 0 0\ntls 0\ntls 1\nstate\nepilog\nreset\n");
    assert_eq!(output.lines().filter(|line| *line == "tls 0").count(), 2);
    assert!(output.contains("reset 0 0 0 0 0\n"));
}
#[test]
fn prologue_arguments_preserve_stack_alias_and_unused_branches() {
    let output=fixture::run("init 0 0 0 1\nproarg 0 0 8 1 255 0 0\nproarg 1 0 8 1 255 24 0\nproarg 1 0 8 1 255 40 1\nepilog\n");
    assert!(output
        .lines()
        .any(|line| line.starts_with("proarg 0 0 40 ")));
    assert!(output.lines().any(|line| line.starts_with("proarg 1 56 ")));
}
#[test]
fn corrected_emission_has_algebraic_semantics() {
    for case in semantics::corrections() {
        let code = semantics::text(&fixture::run(case.input));
        for seed in [1, 7, 173] {
            let initial = semantics::initial(seed);
            let result = semantics::evaluate(&code, initial);
            assert_eq!(
                result[case.register],
                (case.expected)(&initial),
                "{}",
                case.name
            );
        }
    }
}
#[test]
fn evaluator_rejects_uninitialized_and_double_added_values() {
    let initial = semantics::initial(3);
    assert_ne!(semantics::evaluate(&[0xc3], initial)[0], 0);
    assert_eq!(semantics::evaluate(&[0x31, 0xc0, 0xc3], initial)[0], 0);
    let correct = [0x48, 0x8d, 0x04, 0x7d, 9, 0, 0, 0, 0xc3];
    let wrong = [
        0x48, 0x8d, 0x04, 0x7d, 9, 0, 0, 0, 0x48, 0x83, 0xc0, 9, 0xc3,
    ];
    let expected = initial[7].wrapping_mul(2).wrapping_add(9);
    assert_eq!(semantics::evaluate(&correct, initial)[0], expected);
    assert_ne!(semantics::evaluate(&wrong, initial)[0], expected);
}
#[test]
fn compiler_reuses_state_after_cancellation_and_emission_error() {
    fixture::failure_reuse();
}
#[test]
fn return_offset_storage_grows_and_clears_with_capacity() {
    let context = mcx::MemoryContext::new_bump("x64-return-list");
    let mut target = CompilerX64::new(context.mcx(), (), 0);
    for offset in 0..33 {
        target.push_return(offset).unwrap();
    }
    assert_eq!(target.return_offsets().len(), 33);
    assert_eq!(target.pop_return(), Some(32));
    target.clear_returns();
    assert!(target.return_offsets().is_empty());
    target.push_return(99).unwrap();
    assert_eq!(target.return_offsets(), [99]);
}
#[test]
fn impossible_budget_refuses_compiler_creation() {
    fixture::budget_refusal();
}

/// Lane P: the pinned seed template's `x86_regcallcc { i128, i64 }` return is
/// three general-purpose parts against SysV's `rax:rdx`.  `ret_fits` is the
/// register test of upstream `assign_ret` (CompilerX64.hpp 224-245) without
/// its `TPDE_UNREACHABLE("too many return values")` arms; `assign_ret` keeps
/// that arm as the contract check it is upstream.
#[test]
fn sysv_return_fit_is_queried_before_the_assigner_contract_assert() {
    let mut cc = CCAssignerSysV::new(false);
    for reg in [0, 2] {
        let mut part = argument(0, 8, 8);
        assert!(cc.ret_fits(&part));
        cc.assign_ret(&mut part);
        assert_eq!(part.reg.id(), reg);
    }
    let third = argument(0, 8, 8);
    assert!(!cc.ret_fits(&third));
    if tpde_util::CHECKED {
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let mut cc = cc;
            let mut third = third;
            cc.assign_ret(&mut third);
        }))
        .expect_err("assign_ret keeps the upstream contract check");
        let message: &str = panic
            .downcast_ref::<std::string::String>()
            .map(|s| s.as_str())
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(message.contains("too many return values"), "{message}");
    }

    let mut xmm = CCAssignerSysV::new(false);
    for reg in [32, 33] {
        let mut part = argument(1, 16, 16);
        assert!(xmm.ret_fits(&part));
        xmm.assign_ret(&mut part);
        assert_eq!(part.reg.id(), reg);
    }
    assert!(!xmm.ret_fits(&argument(1, 16, 16)));
    // The banks are independent: two more general-purpose parts still fit.
    assert!(xmm.ret_fits(&argument(0, 8, 8)));

    let mut pair = CCAssignerSysV::new(false);
    let mut consecutive = argument(0, 8, 8);
    consecutive.consecutive = 1;
    assert!(pair.ret_fits(&consecutive));
    pair.assign_ret(&mut argument(0, 8, 8));
    assert!(!pair.ret_fits(&consecutive));
    assert!(!pair.ret_fits(&argument(2, 8, 8)));
    let mut byval = argument(0, 8, 8);
    byval.set_byval(true);
    assert!(!pair.ret_fits(&byval));
    let mut sret = argument(0, 8, 8);
    sret.set_sret(true);
    assert!(!pair.ret_fits(&sret));
    pair.reset();
    assert!(pair.ret_fits(&consecutive));
}
