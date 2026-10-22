// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use tpde_assembler::SymRef;
use tpde_core::{
    calling_convention::{
        CCAssigner, CCAssignment, CCAssignmentPolicy, CCInfo, CallingConventionAssigner,
    },
    compiler::{Compiler, CustomVariableReferenceCompiler, ValRefSpecial, ValueParts},
    compiler_config::{Banks, CompilerConfig, CompilerConfigDefault, DefaultVariableReferences},
    AssignmentPartRef, Reg, RegBank,
};

#[derive(Default)]
pub struct TestPolicy {
    pub stack_size: u32,
    pub next_reg: [u32; 2],
    pub arg_calls: u32,
    pub ret_calls: u32,
    pub reset_calls: u32,
}

impl CCAssignmentPolicy for TestPolicy {
    fn reset(&mut self) {
        self.stack_size = 0;
        self.next_reg = [0; 2];
        self.arg_calls = 0;
        self.ret_calls = 0;
        self.reset_calls += 1;
    }

    fn assign_arg(&mut self, assignment: &mut CCAssignment) {
        self.arg_calls += 1;
        let bank = usize::from(assignment.bank.id());
        let count = u32::from(assignment.consecutive) + 1;
        if !assignment.byval() && self.next_reg[bank] + count <= 4 {
            assignment.reg = Reg::new(bank as u64 * 32 + u64::from(self.next_reg[bank]));
            self.next_reg[bank] += count;
        } else {
            let align = u32::from(assignment.align);
            self.stack_size = (self.stack_size + align - 1) & !(align - 1);
            assignment.reg = Reg::make_invalid();
            assignment.stack_off = self.stack_size;
            self.stack_size += assignment.size;
        }
    }

    fn get_stack_size(&mut self) -> u32 {
        self.stack_size
    }

    fn assign_ret(&mut self, assignment: &mut CCAssignment) {
        self.ret_calls += 1;
        assignment.reg =
            Reg::new(u64::from(assignment.bank.id()) * 32 + if assignment.sret() { 3 } else { 0 });
        assignment.stack_off = 0;
    }
}

pub struct TestConfig;

impl CompilerConfig for TestConfig {
    const FRAME_INDEXING_NEGATIVE: bool = true;
    const PLATFORM_POINTER_SIZE: u32 = 8;
    const NUM_BANKS: u32 = 2;
    const DEFAULT_VAR_REF_HANDLING: bool = CompilerConfigDefault::DEFAULT_VAR_REF_HANDLING;
    type DefaultCCAssigner<'cc> = CCAssigner<'cc, TestPolicy>;
    type Assembler = TestAssembler;
    type AsmReg = Reg;
    type FunctionWriter = TestWriter;
    type Banks = Banks<2>;
    type VariableReferences = DefaultVariableReferences;
}

#[derive(Default)]
pub struct TestAssembler {
    pub finished: u32,
}
pub struct TestWriter;

pub struct Parts {
    pub count: u32,
    pub size: u32,
    pub bank: RegBank,
}

impl ValueParts for Parts {
    fn count(&self) -> u32 {
        self.count
    }
    fn size_bytes(&self, part: u32) -> u32 {
        assert!(part < self.count);
        self.size
    }
    fn reg_bank(&self, part: u32) -> RegBank {
        assert!(part < self.count);
        self.bank
    }
}

pub struct Fixture<'cc> {
    assigner: Option<CCAssigner<'cc, TestPolicy>>,
    pub assembler: TestAssembler,
    pub accumulator: u64,
    pub current: u32,
}

impl<'cc> Fixture<'cc> {
    pub fn new(info: &'cc CCInfo) -> Self {
        tpde_core::compiler::check_compiler::<TestConfig, Self>();
        Self {
            assigner: Some(CCAssigner::new(info, TestPolicy::default())),
            assembler: TestAssembler::default(),
            accumulator: 0,
            current: u32::MAX,
        }
    }

    fn record(&mut self, tag: u64, a: u64, b: u64, c: u64) {
        for value in [tag, a, b, c] {
            self.accumulator = self.accumulator.wrapping_mul(131).wrapping_add(value);
        }
    }
}

impl<'cc> Compiler<TestConfig> for Fixture<'cc> {
    const NUM_FIXED_ASSIGNMENTS: [u32; 2] = [5, 8];
    type IRValueRef = u32;
    type IRFuncRef = u32;
    type IRInstRef = u32;
    type InstRange = core::ops::Range<u32>;
    type GenericValuePart = u64;
    type ValuePart = u64;
    type Parts = Parts;
    type ValRefSpecial = ValRefSpecial<u64>;
    type CCAssigner = CCAssigner<'cc, TestPolicy>;

    fn start_func(&mut self, func_idx: u32) {
        self.current = func_idx;
        self.record(1, u64::from(func_idx), 0, 0);
    }
    fn prologue_assign_arg<A: CallingConventionAssigner>(
        &mut self,
        assigner: &mut A,
        index: u32,
        value: u32,
    ) {
        let mut assignment = CCAssignment::new();
        assignment.bank = RegBank::new(0);
        assignment.size = 8;
        assignment.align = 8;
        assigner.assign_arg(&mut assignment);
        self.record(
            2,
            u64::from(index),
            u64::from(value),
            u64::from(assignment.reg.id()),
        );
    }
    fn finish_func(&mut self, func_idx: u32) {
        assert_eq!(func_idx, self.current);
        self.assembler.finished += 1;
        self.record(3, u64::from(func_idx), 0, 0);
    }
    fn spill_reg(&mut self, reg: Reg, frame_off: u32, size: u32) {
        self.record(
            4,
            u64::from(reg.id()),
            u64::from(frame_off),
            u64::from(size),
        );
    }
    fn load_from_stack(&mut self, reg: Reg, frame_off: u32, size: u32) {
        self.record(
            5,
            u64::from(reg.id()),
            u64::from(frame_off),
            u64::from(size),
        );
    }
    fn mov(&mut self, destination: Reg, source: Reg, size: u32) {
        self.record(
            6,
            u64::from(destination.id()),
            u64::from(source.id()),
            u64::from(size),
        );
    }
    fn gval_expr_as_reg(&mut self, value: &mut u64) -> Reg {
        let reg = Reg::new(*value & 63);
        *value += 1;
        self.record(7, u64::from(reg.id()), 0, 0);
        reg
    }
    fn select_fixed_assignment_reg(
        &mut self,
        part: AssignmentPartRef<'_, '_, '_>,
        value: u32,
    ) -> Reg {
        self.record(8, u64::from(part.get_reg().id()), u64::from(value), 0);
        part.get_reg()
    }
    fn cur_func_may_emit_calls(&self) -> bool {
        self.current & 1 != 0
    }
    fn cur_personality_func(&self) -> SymRef {
        SymRef::new(self.current + 1)
    }
    fn cur_cc_assigner(&mut self) -> &mut Self::CCAssigner {
        self.assigner
            .as_mut()
            .expect("current assigner already lent")
    }
    fn with_cur_cc_assigner<R>(
        &mut self,
        operation: impl FnOnce(&mut Self, &mut Self::CCAssigner) -> R,
    ) -> R {
        let mut assigner = self.assigner.take().expect("current assigner already lent");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            operation(self, &mut assigner)
        }));
        assert!(self.assigner.is_none());
        self.assigner = Some(assigner);
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
    fn try_force_fixed_assignment(&mut self, value: u32) -> bool {
        self.record(9, u64::from(value), 0, 0);
        value & 1 == 0
    }
    fn val_parts(&self, value: u32) -> Parts {
        Parts {
            count: 1 + value % 3,
            size: 1 << (value % 5),
            bank: RegBank::new((value % 2) as u8),
        }
    }
    fn val_ref_special(&mut self, value: u32) -> Option<Self::ValRefSpecial> {
        self.record(10, u64::from(value), 0, 0);
        (value & 1 == 1).then(|| ValRefSpecial::new(4, u64::from(value) * 17))
    }
    fn val_part_ref_special(&mut self, value: &mut Self::ValRefSpecial, part: u32) -> u64 {
        self.record(11, value.payload, u64::from(part), 0);
        value.payload += u64::from(part);
        value.payload
    }
    fn define_func_idx(&mut self, function: u32, index: u32) {
        self.record(12, u64::from(function), u64::from(index), 0);
    }
    fn compile_inst(&mut self, instruction: u32, range: Self::InstRange) -> bool {
        self.record(
            13,
            u64::from(instruction),
            u64::from(range.start),
            u64::from(range.end),
        );
        range.contains(&instruction)
    }
}

impl CustomVariableReferenceCompiler<TestConfig> for Fixture<'_> {
    fn setup_var_ref_assignments(&mut self) {
        self.record(14, 0, 0, 0);
    }
    fn load_address_of_var_reference(
        &mut self,
        destination: Reg,
        part: AssignmentPartRef<'_, '_, '_>,
    ) {
        self.record(
            15,
            u64::from(destination.id()),
            u64::from(part.variable_ref_data()),
            0,
        );
    }
}
