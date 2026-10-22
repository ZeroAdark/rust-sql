// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![cfg(feature = "upstream-oracle")]

#[path = "compiler_contracts_fixture.rs"]
mod fixture;
#[path = "../../test-support/oracle.rs"]
mod oracle;
#[path = "compiler_contracts_refusals.rs"]
mod refusals;

use fixture::*;
use mcx::MemoryContext;
use std::{fs, mem, path::Path, ptr};
use tpde_core::{
    calling_convention::{
        CCAssigner, CCAssignment, CCAssignmentPolicy, CCInfo, CallingConventionAssigner,
    },
    compiler::{
        Compiler, CustomVariableReferenceCompiler, ValRefSpecial, ValRefSpecialStruct, ValueParts,
        VariableReferenceRequirements,
    },
    compiler_config::{check_config, CompilerConfig, CompilerConfigDefault, RegisterBanks},
    value_assignment::{with_assignment_allocator, AssignmentInit},
    AssignmentPartRef, Reg, RegBank,
};

fn word(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn assignment(output: &mut Vec<u8>, value: &CCAssignment) {
    for value in [
        u32::from(value.reg.id()),
        u32::from(value.consecutive),
        u32::from(value.sret()),
        u32::from(value.byval()),
        u32::from(value.int_ext),
        u32::from(value.align),
        u32::from(value.bank.id()),
        value.size,
        value.stack_off,
    ] {
        word(output, value);
    }
}

fn fields_trace() -> Vec<u8> {
    let mut output = Vec::new();
    assignment(&mut output, &CCAssignment::default());
    let sizes = [0, 1, 7, 8, 255, 65536, 0x80000000, u32::MAX];
    for value in 0..=u8::MAX {
        for flags in 0..4 {
            for index in 0..8 {
                let mut item = CCAssignment::new();
                item.reg = Reg::new(u64::from(value));
                item.consecutive = value;
                item.set_sret(flags & 1 != 0);
                item.set_byval(flags & 2 != 0);
                item.int_ext = value ^ 0x80;
                item.align = u8::MAX - value;
                item.bank = RegBank::new(value);
                item.size = sizes[index];
                item.stack_off = sizes[7 - index];
                assignment(&mut output, &item);
            }
        }
    }
    assert_eq!(output.len(), 294948);
    output
}

fn info_trace<P: CCAssignmentPolicy>(
    output: &mut Vec<u8>,
    assigner: &CCAssigner<'_, P>,
    expected: &CCInfo,
) {
    let info = assigner.get_ccinfo();
    for value in [
        info.allocatable_regs(),
        info.callee_saved_regs(),
        info.arg_regs(),
    ] {
        output.extend_from_slice(&value.to_le_bytes());
    }
    for value in [
        u32::from(info.red_zone_size()),
        u32::from(ptr::eq(assigner.ccinfo, expected)),
        u32::from(ptr::eq(info, expected)),
        u32::from(assigner.is_vararg()),
    ] {
        word(output, value);
    }
}

fn state_trace(output: &mut Vec<u8>, assigner: &mut CCAssigner<'_, TestPolicy>) {
    word(output, assigner.get_stack_size());
    let state = assigner.policy();
    for value in [
        state.next_reg[0],
        state.next_reg[1],
        state.arg_calls,
        state.ret_calls,
        state.reset_calls,
    ] {
        word(output, value);
    }
}

struct Variadic(TestPolicy);

impl CCAssignmentPolicy for Variadic {
    fn reset(&mut self) {
        self.0.reset();
    }
    fn assign_arg(&mut self, value: &mut CCAssignment) {
        self.0.assign_arg(value);
    }
    fn get_stack_size(&mut self) -> u32 {
        self.0.get_stack_size()
    }
    fn is_vararg(&self) -> bool {
        true
    }
    fn assign_ret(&mut self, value: &mut CCAssignment) {
        self.0.assign_ret(value);
    }
}

fn assigner_trace() -> Vec<u8> {
    let original = CCInfo::new(0xfedcba9876543210, 0x8000000000000001, 0x000000030000000c);
    let alternative = CCInfo::with_red_zone(u64::MAX, 0, 0xaaaaaaaa55555555, 128);
    let mut assigner = CCAssigner::new(&original, TestPolicy::default());
    let mut output = Vec::new();
    info_trace(&mut output, &assigner, &original);
    assigner.ccinfo = &alternative;
    info_trace(&mut output, &assigner, &alternative);
    let variadic = CCAssigner::new(&original, Variadic(TestPolicy::default()));
    info_trace(&mut output, &variadic, &original);
    for epoch in 0..3 {
        assigner.reset();
        word(&mut output, epoch);
        state_trace(&mut output, &mut assigner);
        for j in 0u32..12 {
            let mut item = CCAssignment::new();
            item.consecutive = (j % 3) as u8;
            item.set_sret(j == 0);
            item.set_byval(j % 5 == 0);
            item.int_ext = (if j & 1 == 1 { 0x80 } else { 0 }) | (8 << (j % 4));
            item.align = 1 << (j % 5);
            item.bank = RegBank::new((j % 2) as u8);
            item.size = 1 << (j % 7);
            assigner.assign_arg(&mut item);
            assignment(&mut output, &item);
            state_trace(&mut output, &mut assigner);
        }
        let mut item = CCAssignment::new();
        item.consecutive = 2;
        item.set_sret(epoch & 1 == 1);
        item.int_ext = 0x88;
        item.align = 16;
        item.bank = RegBank::new((epoch % 2) as u8);
        item.size = 16;
        item.stack_off = u32::MAX;
        assigner.assign_ret(&mut item);
        assignment(&mut output, &item);
        state_trace(&mut output, &mut assigner);
    }
    assert_eq!(output.len(), 2544);
    output
}

fn layout_trace() -> Vec<u8> {
    let mut output = Vec::new();
    for value in [
        mem::size_of::<CCAssignment>(),
        mem::align_of::<CCAssignment>(),
        mem::offset_of!(CCAssignment, reg),
        mem::offset_of!(CCAssignment, consecutive),
        mem::offset_of!(CCAssignment, int_ext),
        mem::offset_of!(CCAssignment, align),
        mem::offset_of!(CCAssignment, bank),
        mem::offset_of!(CCAssignment, size),
        mem::offset_of!(CCAssignment, stack_off),
        mem::size_of::<CCInfo>(),
        mem::align_of::<CCInfo>(),
        mem::size_of::<ValRefSpecial<u64>>(),
        mem::align_of::<ValRefSpecial<u64>>(),
        mem::offset_of!(ValRefSpecial<u64>, mode),
        mem::offset_of!(ValRefSpecial<u64>, payload),
    ] {
        word(&mut output, value.try_into().unwrap());
    }
    output
}

#[test]
fn pinned_fields_assigner_and_concepts() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let work = std::env::temp_dir().join(format!("tpde-compiler-contracts-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    let fields = fields_trace();
    let assigner = assigner_trace();
    for assertions in [true, false] {
        let binary = work.join(format!("oracle-{assertions}"));
        oracle::compile_cpp_with_flags(
            &root.join("tests/compiler-contracts-reference/tpde/include"),
            &binary,
            &[root.join("tests/compiler-contracts-reference/oracle.cpp")],
            assertions,
            &["-stdlib=libc++"],
        );
        oracle::compare_bytes(
            &binary,
            &work,
            b"fields\n",
            &fields,
            "all CC assignment fields",
        );
        oracle::compare_bytes(
            &binary,
            &work,
            b"assigner\n",
            &assigner,
            "CC assigner borrow and dispatch",
        );
        oracle::compare_bytes(
            &binary,
            &work,
            b"layout\n",
            &layout_trace(),
            "CC and special carrier layout",
        );
        let roster =
            fs::read_to_string(root.join("tests/compiler-contracts-reference/COUNTS")).unwrap();
        assert_eq!(roster.trim(), "1 43 112");
        let mut expected = Vec::new();
        for count in roster.split_whitespace() {
            word(&mut expected, count.parse().unwrap());
        }
        assert_eq!(
            expected.len(),
            12,
            "version and positive/negative concept counts required"
        );
        oracle::compare_bytes(
            &binary,
            &work,
            b"concepts\n",
            &expected,
            "all compiler concept forms",
        );
    }
    fs::remove_dir_all(&work).unwrap();
    println!("TPDE_COMPILER_CONTRACT_ORACLE PASS cpp_modes=2 field_records=8193 assigner_epochs=3 assigner_rows=39 layout_words=15");
}

#[test]
fn special_layout_config_and_flag_reuse() {
    #[repr(align(64))]
    #[derive(Clone, Copy)]
    struct Aligned(u64);
    assert_eq!(mem::offset_of!(ValRefSpecial<u8>, mode), 0);
    assert_eq!(mem::offset_of!(ValRefSpecial<u64>, mode), 0);
    assert_eq!(mem::offset_of!(ValRefSpecial<[u8; 3]>, mode), 0);
    assert_eq!(mem::offset_of!(ValRefSpecial<Aligned>, mode), 0);
    assert_eq!(mem::align_of::<ValRefSpecial<Aligned>>(), 64);
    assert!(!mem::needs_drop::<ValRefSpecial<Aligned>>());
    assert!(!mem::needs_drop::<CCAssigner<'_, TestPolicy>>());
    for red_zone in 0..=u8::MAX {
        let info = CCInfo::with_red_zone(u64::MAX, 0, 0x8000000000000001, red_zone);
        assert_eq!(info.allocatable_regs(), u64::MAX);
        assert_eq!(info.callee_saved_regs(), 0);
        assert_eq!(info.arg_regs(), 0x8000000000000001);
        assert_eq!(info.red_zone_size(), red_zone);
    }
    let mut item = ValRefSpecial::new(0, Aligned(0x87654321));
    for mode in 0..=u8::MAX {
        *item.mode_mut() = mode;
        assert_eq!(item.mode(), mode);
        assert_eq!(item.payload.0, 0x87654321);
    }
    let mut fields = CCAssignment::new();
    for _ in 0..3 {
        fields.set_sret(true);
        fields.set_byval(true);
        fields.set_sret(false);
        assert!(fields.byval());
        fields.set_sret(true);
        fields.set_byval(false);
        assert!(fields.sret());
        fields.set_sret(false);
        assert_eq!(fields, CCAssignment::default());
    }
    check_config::<TestConfig>();
    assert!(CompilerConfigDefault::DEFAULT_VAR_REF_HANDLING);
    assert_eq!(<TestConfig as CompilerConfig>::NUM_BANKS, 2);
    assert_eq!(
        <<TestConfig as CompilerConfig>::Banks as RegisterBanks>::COUNT,
        2
    );
    assert_eq!(
        <Fixture<'_> as Compiler<TestConfig>>::NUM_FIXED_ASSIGNMENTS,
        [5, 8]
    );
    println!("TPDE_COMPILER_LAYOUT PASS layouts=4 mode_values=256 flag_reuse_epochs=3");
}

#[test]
fn complete_compiler_contract_calls() {
    let context = MemoryContext::new("compiler-contract");
    let info = CCInfo::new(0xffff, 0xff00, 0xf);
    let mut compiler = Fixture::new(&info);
    compiler.start_func(7);
    assert!(compiler.cur_func_may_emit_calls());
    assert_eq!(compiler.cur_personality_func().id(), 8);
    assert!(ptr::eq(compiler.cur_cc_assigner().get_ccinfo(), &info));
    current_prologue::<TestConfig, _>(&mut compiler, 2, 19);
    assert_eq!(compiler.cur_cc_assigner().policy().arg_calls, 1);
    compiler.spill_reg(Reg::new(3), 64, 8);
    compiler.load_from_stack(Reg::new(4), 64, 8);
    compiler.mov(Reg::new(5), Reg::new(4), 8);
    let mut generic = 73;
    assert_eq!(compiler.gval_expr_as_reg(&mut generic), Reg::new(9));
    assert_eq!(generic, 74);
    with_assignment_allocator(context.mcx(), |allocator| {
        let assignment = allocator
            .allocate(1)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 0);
        let part = AssignmentPartRef::new(&assignment, 0);
        part.set_bank(RegBank::new(1));
        part.set_reg(Reg::new(38));
        assert_eq!(compiler.select_fixed_assignment_reg(part, 23), Reg::new(38));
        allocator.deallocate(assignment);
    });
    assert!(compiler.try_force_fixed_assignment(24));
    assert!(!compiler.try_force_fixed_assignment(25));
    let parts = compiler.val_parts(7);
    assert_eq!(parts.count(), 2);
    for part in 0..parts.count() {
        assert_eq!(parts.size_bytes(part), 4);
        assert_eq!(parts.reg_bank(part), RegBank::new(1));
    }
    assert!(compiler.val_ref_special(8).is_none());
    let mut special = compiler.val_ref_special(9).unwrap();
    assert_eq!(special.mode, 4);
    assert_eq!(compiler.val_part_ref_special(&mut special, 2), 155);
    compiler.define_func_idx(81, 3);
    assert!(compiler.compile_inst(5, 2..9));
    assert!(!compiler.compile_inst(10, 2..9));
    compiler.setup_var_ref_assignments();
    with_assignment_allocator(context.mcx(), |allocator| {
        let assignment = allocator.allocate(1).unwrap().initialize(
            AssignmentInit {
                variable_ref: true,
                ..AssignmentInit::default()
            },
            |_| 0,
        );
        assignment.set_var_ref_custom_idx(27);
        let part = AssignmentPartRef::new(&assignment, 0);
        compiler.load_address_of_var_reference(Reg::new(6), part);
        allocator.deallocate(assignment);
    });
    compiler.finish_func(7);
    assert_eq!(compiler.assembler.finished, 1);
    let operations = [
        [1, 7, 0, 0],
        [2, 2, 19, 0],
        [4, 3, 64, 8],
        [5, 4, 64, 8],
        [6, 5, 4, 8],
        [7, 9, 0, 0],
        [8, 38, 23, 0],
        [9, 24, 0, 0],
        [9, 25, 0, 0],
        [10, 8, 0, 0],
        [10, 9, 0, 0],
        [11, 153, 2, 0],
        [12, 81, 3, 0],
        [13, 5, 2, 9],
        [13, 10, 2, 9],
        [14, 0, 0, 0],
        [15, 6, 27, 0],
        [3, 7, 0, 0],
    ];
    let expected = operations
        .into_iter()
        .flatten()
        .fold(0u64, |acc, value| acc.wrapping_mul(131).wrapping_add(value));
    assert_eq!(compiler.accumulator, expected);
    println!("TPDE_COMPILER_CALLBACKS PASS required_methods=17 custom_methods=2 recorded_calls=18");
}

fn current_prologue<C: CompilerConfig, T: Compiler<C>>(
    compiler: &mut T,
    index: u32,
    value: T::IRValueRef,
) where
    C::VariableReferences: VariableReferenceRequirements<T, C>,
{
    compiler.with_cur_cc_assigner(|compiler, assigner| {
        assigner.reset();
        compiler.prologue_assign_arg(assigner, index, value);
    });
}

#[test]
fn current_assigner_restored_after_unwind_and_reused() {
    let info = CCInfo::new(0xffff, 0xff00, 0xf);
    let mut compiler = Fixture::new(&info);
    current_prologue::<TestConfig, _>(&mut compiler, 0, 3);
    assert_eq!(compiler.cur_cc_assigner().policy().arg_calls, 1);
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        compiler.with_cur_cc_assigner(|owner, assigner| {
            owner.prologue_assign_arg(assigner, 1, 4);
            assert_eq!(assigner.policy().arg_calls, 2);
            panic!("ownership witness failure");
        });
    }));
    assert!(failure.is_err());
    assert!(ptr::eq(compiler.cur_cc_assigner().get_ccinfo(), &info));
    assert_eq!(compiler.cur_cc_assigner().policy().arg_calls, 2);
    let nested = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        compiler.with_cur_cc_assigner(|owner, _| {
            owner.with_cur_cc_assigner(|_, _| ());
        });
    }));
    assert!(nested.is_err());
    assert_eq!(compiler.cur_cc_assigner().policy().arg_calls, 2);
    current_prologue::<TestConfig, _>(&mut compiler, 2, 5);
    assert_eq!(compiler.cur_cc_assigner().policy().arg_calls, 1);
    assert_eq!(compiler.cur_cc_assigner().policy().reset_calls, 2);
    println!("TPDE_COMPILER_OWNERSHIP PASS same_owner=1 unwind_restore=1 nested_refusal=1 reuse=1");
}
