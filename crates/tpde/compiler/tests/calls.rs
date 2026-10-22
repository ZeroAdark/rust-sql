// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use super::*;
use crate::{generic_value_part::GenericValuePart, guard_machine::INVALID_VALUE};
use core::cell::{Cell, RefCell};
use mcx::MemoryContext;
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    vec::Vec,
};
use tpde_core::{
    compiler::ValRefSpecial,
    ir_adaptor::{NoHighestValue, PhiRef},
    value_assignment::{with_assignment_allocator, Assignment, AssignmentInit},
    AssignmentPartRef, BlockIndex, LivenessInfo, RegisterFile,
};
use types_error::PgError;

struct Adaptor;
impl PhiRef<u32, u32> for Adaptor {
    fn incoming_count(&self) -> u32 {
        0
    }
    fn incoming_val_for_slot(&self, _: u32) -> u32 {
        panic!("no phi")
    }
    fn incoming_block_for_slot(&self, _: u32) -> u32 {
        panic!("no phi")
    }
    fn incoming_val_for_block(&self, _: u32) -> u32 {
        panic!("no phi")
    }
}
impl IrAdaptor for Adaptor {
    type ValueRef = u32;
    type InstRef = u32;
    type BlockRef = u32;
    type FuncRef = u32;
    type HighestValue = NoHighestValue;
    const INVALID_VALUE_REF: u32 = u32::MAX;
    const INVALID_BLOCK_REF: u32 = u32::MAX;
    const INVALID_FUNC_REF: u32 = u32::MAX;
    const TPDE_LIVENESS_VISIT_ARGS: bool = true;
    type Funcs = [u32; 0];
    type FuncsToCompile = [u32; 0];
    type Args<'a> = [u32; 0];
    type StaticAllocas<'a> = [u32; 0];
    type Blocks<'a> = [u32; 0];
    type Successors<'a> = [u32; 0];
    type Insts<'a> = [u32; 0];
    type Phis<'a> = [u32; 0];
    type Operands<'a> = [u32; 0];
    type Results<'a> = [u32; 0];
    type Phi<'a> = Self;
    type BlockFormat<'a> = u32;
    type ValueFormat<'a> = u32;
    type InstFormat<'a> = u32;
    fn func_count(&self) -> u32 {
        0
    }
    fn funcs(&self) -> Self::Funcs {
        []
    }
    fn funcs_to_compile(&self) -> Self::FuncsToCompile {
        []
    }
    fn func_link_name(&self, _: u32) -> &[u8] {
        b"calls"
    }
    fn func_extern(&self, _: u32) -> bool {
        false
    }
    fn func_only_local(&self, _: u32) -> bool {
        false
    }
    fn func_has_weak_linkage(&self, _: u32) -> bool {
        false
    }
    fn cur_needs_unwind_info(&self) -> bool {
        false
    }
    fn cur_is_vararg(&self) -> bool {
        false
    }
    fn cur_args(&self) -> Self::Args<'_> {
        []
    }
    fn cur_arg_is_byval(&self, index: u32) -> bool {
        index == 0
    }
    fn cur_arg_byval_size(&self, _: u32) -> u32 {
        24
    }
    fn cur_arg_byval_align(&self, _: u32) -> u32 {
        16
    }
    fn cur_arg_is_sret(&self, index: u32) -> bool {
        index == 1
    }
    fn cur_static_allocas(&self) -> Self::StaticAllocas<'_> {
        []
    }
    fn cur_has_dynamic_alloca(&self) -> bool {
        false
    }
    fn cur_entry_block(&self) -> u32 {
        0
    }
    fn cur_blocks(&self) -> Self::Blocks<'_> {
        []
    }
    fn block_succs(&self, _: u32) -> Self::Successors<'_> {
        []
    }
    fn block_insts(&self, _: u32) -> Self::Insts<'_> {
        []
    }
    fn block_phis(&self, _: u32) -> Self::Phis<'_> {
        []
    }
    fn block_info(&self, _: u32) -> u32 {
        0
    }
    fn block_set_info(&self, _: u32, _: u32) {}
    fn block_info2(&self, _: u32) -> u32 {
        0
    }
    fn block_set_info2(&self, _: u32, _: u32) {}
    fn block_fmt_ref(&self, value: u32) -> u32 {
        value
    }
    fn val_local_idx(&self, value: u32) -> ValLocalIdx {
        ValLocalIdx::new(value)
    }
    fn val_ignore_in_liveness_analysis(&self, _: u32) -> bool {
        false
    }
    fn val_is_phi(&self, _: u32) -> bool {
        false
    }
    fn val_as_phi(&self, _: u32) -> Self {
        Adaptor
    }
    fn val_alloca_size(&self, _: u32) -> u32 {
        0
    }
    fn val_alloca_align(&self, _: u32) -> u32 {
        1
    }
    fn value_fmt_ref(&self, value: u32) -> u32 {
        value
    }
    fn inst_operands(&self, _: u32) -> Self::Operands<'_> {
        []
    }
    fn inst_results(&self, _: u32) -> Self::Results<'_> {
        []
    }
    fn inst_fused(&self, _: u32) -> bool {
        false
    }
    fn inst_fmt_ref(&self, value: u32) -> u32 {
        value
    }
    fn start_compile(&mut self) {}
    fn end_compile(&mut self) {}
    fn switch_func(&mut self, _: u32) -> bool {
        true
    }
    fn reset(&mut self) {}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Materialize(Reg, u64, RegBank, u32),
    Move(Reg, Reg, u32),
    Extend(Reg, Reg, bool, u32, u32),
    Evict(Reg),
    Reload(Reg, ValLocalIdx, u32),
    Stack(CCAssignment, u64),
    Byval(CCAssignment),
    Call(u32, u64),
    Prologue(CCAssignment),
    Release(u32),
    Free(u32),
    Variable(u32, i32),
    Epilog,
    ReleaseRegisters,
}

struct Machine<'a, 'id> {
    assignments: RefCell<Vec<Option<Assignment<'a, 'id>>>>,
    spare: RefCell<Option<Assignment<'a, 'id>>>,
    registers: RefCell<RegisterFile<1, 32>>,
    events: RefCell<Vec<Event>>,
    leaf: Cell<bool>,
    generated_call: Cell<bool>,
    byval_frame: Cell<Option<i32>>,
    fail_emit: Cell<bool>,
    fail_epilog: Cell<bool>,
    special_count: Cell<u32>,
}

fn with_machine(parts: &[u32], references: u32, run: impl for<'a, 'id> FnOnce(&Machine<'a, 'id>)) {
    let context = MemoryContext::new("compiler-calls-tests");
    with_assignment_allocator(context.mcx(), |allocator| {
        let init = AssignmentInit {
            references_left: references,
            max_part_size: 8,
            ..AssignmentInit::default()
        };
        let assignments = parts
            .iter()
            .map(|count| {
                Some(
                    allocator
                        .allocate(*count)
                        .unwrap()
                        .initialize(init, |_| 3 << 12 | 1 << 9),
                )
            })
            .collect();
        let spare = allocator
            .allocate(1)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 3 << 12);
        let mut registers = RegisterFile::new();
        registers.allocatable.set(u32::MAX as u64);
        let machine = Machine {
            assignments: RefCell::new(assignments),
            spare: RefCell::new(Some(spare)),
            registers: RefCell::new(registers),
            events: RefCell::new(Vec::new()),
            leaf: Cell::new(false),
            generated_call: Cell::new(false),
            byval_frame: Cell::new(None),
            fail_emit: Cell::new(false),
            fail_epilog: Cell::new(false),
            special_count: Cell::new(1),
        };
        run(&machine);
    });
}

impl Machine<'_, '_> {
    fn emit(&self, event: Event) -> PgResult<()> {
        self.events.borrow_mut().push(event);
        if self.fail_emit.get() {
            Err(PgError::error("injected call emission failure").into())
        } else {
            Ok(())
        }
    }
    fn assign_reg(&self, local: u32, part: u32, reg: Reg) {
        self.with_part(ValLocalIdx::new(local), part, |p| {
            p.set_reg(reg);
            p.set_register_valid(true);
        });
        self.with_registers(|r| r.mark_used(reg, ValLocalIdx::new(local), part));
    }
    fn mask(&self) -> u64 {
        self.registers.borrow().allocatable.get()
    }
}

impl GuardMachine<1> for Machine<'_, '_> {
    type Special = ValRefSpecial<u32>;
    const GP_BANK: RegBank = RegBank::new(0);
    fn with_registers<R>(&self, operation: impl FnOnce(&RegisterFile<1, 32>) -> R) -> R {
        operation(&*self.registers.borrow_mut())
    }
    fn with_assignment<R>(
        &self,
        local: ValLocalIdx,
        operation: impl FnOnce(&Assignment<'_, '_>) -> R,
    ) -> R {
        operation(
            self.assignments.borrow()[local.get() as usize]
                .as_ref()
                .unwrap(),
        )
    }
    fn with_part<R>(
        &self,
        local: ValLocalIdx,
        part: u32,
        operation: impl FnOnce(AssignmentPartRef<'_, '_, '_>) -> R,
    ) -> R {
        operation(AssignmentPartRef::new(
            self.assignments.borrow()[local.get() as usize]
                .as_ref()
                .unwrap(),
            part,
        ))
    }
    fn liveness(&self, _: ValLocalIdx) -> LivenessInfo {
        LivenessInfo {
            first: BlockIndex(0),
            last: BlockIndex(0),
            ref_count: 10,
            lowest_common_loop: 0,
            last_full: false,
            epoch: 0,
            weight: 0,
            crosses_call: false,
            crosses_cold_call: false,
        }
    }
    fn current_block(&self) -> BlockIndex {
        BlockIndex(0)
    }
    fn may_change_value_state(&self) -> bool {
        true
    }
    fn release_assignment(&self, local: ValLocalIdx) {
        self.events.borrow_mut().push(Event::Release(local.get()));
        self.call_free_assignment_if_present(local);
    }
    fn decrement_fixed_count(&self, _: RegBank) {
        panic!("fixture has no fixed assignments")
    }
    fn select_reg(&self, bank: RegBank) -> PgResult<Reg> {
        let reg = self.registers.borrow().find_first_free_excluding(bank, 0);
        assert!(reg.valid());
        Ok(reg)
    }
    fn evict_reg(&self, reg: Reg) -> PgResult<()> {
        self.emit(Event::Evict(reg))?;
        let (local, part) = self.with_registers(|r| (r.reg_local_idx(reg), r.reg_part(reg)));
        if local != INVALID_VALUE {
            self.with_part(local, part, |p| {
                p.set_register_valid(false);
                p.set_stack_valid();
            });
        }
        self.with_registers(|r| r.unmark_used(reg));
        Ok(())
    }
    fn reload_to_reg(&self, reg: Reg, local: ValLocalIdx, part: u32) -> PgResult<()> {
        self.emit(Event::Reload(reg, local, part))
    }
    fn mov(&self, dst: Reg, src: Reg, size: u32) -> PgResult<()> {
        self.emit(Event::Move(dst, src, size))
    }
    fn materialize_constant(
        &self,
        words: &[u64],
        bank: RegBank,
        size: u32,
        reg: Reg,
    ) -> PgResult<()> {
        self.emit(Event::Materialize(reg, words[0], bank, size))
    }
    fn generate_raw_intext(
        &self,
        dst: Reg,
        src: Reg,
        sign: bool,
        from: u32,
        to: u32,
    ) -> PgResult<()> {
        self.emit(Event::Extend(dst, src, sign, from, to))
    }
    fn gval_expr_as_reg<'c>(&'c self, _: &mut GenericValuePart<'c, Self, 1>) -> PgResult<Reg> {
        panic!("no generic expression")
    }
    fn special_part<'s>(
        &'s self,
        special: &'s Self::Special,
        part: u32,
    ) -> ValuePartRef<'s, Self, 1> {
        ValuePartRef::constant_inline(
            self,
            u64::from(special.payload) + u64::from(part),
            8,
            Self::GP_BANK,
        )
    }
}

impl CallMachine<1> for Machine<'_, '_> {
    type CallAdaptor = Adaptor;
    const PLATFORM_POINTER_SIZE: u32 = 8;
    fn call_adaptor(&self) -> &Adaptor {
        &Adaptor
    }
    fn call_val_parts_count(&self, value: &u32) -> u32 {
        if *value == 1000 {
            self.special_count.get()
        } else {
            self.with_assignment(ValLocalIdx::new(*value), |a| a.part_count())
        }
    }
    fn call_val_idx(&self, value: &u32) -> ValLocalIdx {
        ValLocalIdx::new(*value)
    }
    fn call_val_ref(&self, value: &u32) -> PgResult<ValueRef<'_, Self, 1>> {
        Ok(if *value == 1000 {
            ValueRef::special(self, ValRefSpecial::new(4, *value))
        } else {
            ValueRef::new(self, ValLocalIdx::new(*value))
        })
    }
    fn call_result_ref(&self, value: &u32) -> PgResult<ValueRef<'_, Self, 1>> {
        self.with_assignment(ValLocalIdx::new(*value), |a| {
            a.set_references_left(a.references_left() + 1)
        });
        Ok(ValueRef::new(self, ValLocalIdx::new(*value)))
    }
    fn call_is_leaf_function(&self) -> bool {
        self.leaf.get()
    }
    fn call_spill_part(&self, _: ValLocalIdx, _: u32) -> PgResult<()> {
        unreachable!("fixture keeps no fixed value in a caller-saved register")
    }
    fn call_set_generated_call(&self) {
        self.generated_call.set(true);
    }
    fn call_free_assignment_if_present(&self, local: ValLocalIdx) {
        if let Some(assignment) = self.assignments.borrow_mut()[local.get() as usize].take() {
            for part in 0..assignment.part_count() {
                let p = AssignmentPartRef::new(&assignment, part);
                if p.register_valid() {
                    self.with_registers(|r| r.unmark_used(p.get_reg()));
                }
            }
            self.events.borrow_mut().push(Event::Free(local.get()));
        }
    }
    fn call_init_stack_variable(&self, local: ValLocalIdx, frame_off: i32) -> PgResult<()> {
        self.emit(Event::Variable(local.get(), frame_off))?;
        let spare = self.spare.borrow_mut().take().unwrap();
        spare.set_variable_ref(true);
        spare.set_stack_variable(true);
        spare.set_frame_off(frame_off);
        self.assignments.borrow_mut()[local.get() as usize] = Some(spare);
        Ok(())
    }
    fn prologue_assign_arg_part(
        &self,
        mut value: ValuePartRef<'_, Self, 1>,
        assignment: CCAssignment,
    ) -> PgResult<Option<i32>> {
        self.emit(Event::Prologue(assignment))?;
        if !assignment.byval() {
            value.set_value_reg(assignment.reg)?;
        }
        Ok(if assignment.byval() {
            self.byval_frame.get()
        } else {
            None
        })
    }
    fn gen_func_epilog(&self) -> PgResult<()> {
        self.events.borrow_mut().push(Event::Epilog);
        if self.fail_epilog.get() {
            Err(PgError::error("injected epilog failure").into())
        } else {
            Ok(())
        }
    }
    fn release_regs_after_return(&self) {
        self.events.borrow_mut().push(Event::ReleaseRegisters);
    }
}

#[derive(Clone, Copy)]
struct Policy {
    next_arg: u32,
    next_ret: u32,
    stack: bool,
    extension: u8,
    size_override: Option<u32>,
    resets: u32,
}
impl CCAssignmentPolicy for Policy {
    fn reset(&mut self) {
        self.next_arg = 0;
        self.next_ret = 0;
        self.resets += 1;
    }
    fn assign_arg(&mut self, assignment: &mut CCAssignment) {
        if let Some(size) = self.size_override {
            assignment.size = size;
        }
        if !self.stack && !assignment.byval() {
            assignment.reg = Reg::new(u64::from(self.next_arg));
        } else {
            assignment.reg = Reg::make_invalid();
            assignment.stack_off = self.next_arg * 8;
        }
        self.next_arg += 1;
    }
    fn get_stack_size(&mut self) -> u32 {
        self.next_arg * 8
    }
    fn assign_ret(&mut self, assignment: &mut CCAssignment) {
        if let Some(size) = self.size_override {
            assignment.size = size;
        }
        assignment.reg = Reg::new(u64::from(self.next_ret));
        assignment.int_ext = self.extension;
        self.next_ret += 1;
    }
}

fn policy(stack: bool) -> Policy {
    Policy {
        next_arg: 0,
        next_ret: 0,
        stack,
        extension: 0,
        size_override: None,
        resets: 0,
    }
}
use tpde_core::calling_convention::{CCAssigner, CCAssignmentPolicy, CCInfo};

#[derive(Default)]
struct Hooks {
    fail_call: bool,
    fail_stack: bool,
}
impl<'a, 'id> CallBuilderHooks<Machine<'a, 'id>, 1> for Hooks {
    fn add_arg_byval<A: CallingConventionAssigner>(
        &mut self,
        compiler: &Machine<'a, 'id>,
        _: &mut A,
        _: &mut ValuePartRef<'_, Machine<'a, 'id>, 1>,
        assignment: &mut CCAssignment,
    ) -> PgResult<()> {
        compiler.emit(Event::Byval(*assignment))
    }
    fn add_arg_stack<A: CallingConventionAssigner>(
        &mut self,
        compiler: &Machine<'a, 'id>,
        _: &mut A,
        value: &mut ValuePartRef<'_, Machine<'a, 'id>, 1>,
        assignment: &mut CCAssignment,
    ) -> PgResult<()> {
        compiler.emit(Event::Stack(
            *assignment,
            if value.is_const() {
                value.const_data()[0]
            } else {
                u64::MAX
            },
        ))?;
        if self.fail_stack {
            value.cur_reg_or_load()?;
            return Err(PgError::error("injected stack failure").into());
        }
        Ok(())
    }
    fn call_impl<A: CallingConventionAssigner>(
        &mut self,
        compiler: &Machine<'a, 'id>,
        assigner: &mut A,
        target: CallTarget<'_, Machine<'a, 'id>, 1>,
    ) -> PgResult<()> {
        let id = match target {
            CallTarget::Symbol(symbol) => symbol.id(),
            CallTarget::Value(mut value) => {
                let reg = value.cur_reg_or_load()?;
                value.reset();
                u32::from(reg.id()) | (1 << 31)
            }
        };
        let size = assigner.get_stack_size();
        compiler.emit(Event::Call(id, u64::from(size)))?;
        if self.fail_call {
            Err(PgError::error("injected call failure").into())
        } else {
            Ok(())
        }
    }
}

fn assignment(reg: u8, ext: u8) -> CCAssignment {
    let mut result = CCAssignment::new();
    result.reg = Reg::new(u64::from(reg));
    result.int_ext = ext;
    result
}

#[test]
fn call_flags_defaults_and_consecutive_saturation() {
    assert_eq!(
        CallArg::new(7),
        CallArg {
            value: 7,
            flag: CallArgFlag::None,
            byval_align: 1,
            ext_bits: 0,
            byval_size: 0
        }
    );
    for count in [0, 1, 2, 255, 256, 257, u32::MAX] {
        for part in [0, count.saturating_sub(1)] {
            if part < count {
                assert_eq!(consecutive_parts(count, part, true), 0);
                assert_eq!(
                    consecutive_parts(count, part, false),
                    if count >= 256 {
                        255
                    } else {
                        (count - part - 1) as u8
                    }
                );
            }
        }
    }
}

#[test]
fn complete_arguments_preserve_byval_sret_alignment_and_split() {
    with_machine(&[], 0, |machine| {
        let info = CCInfo::new(u32::MAX as u64, 0, 0);
        let mut assigner = CCAssigner::new(&info, policy(true));
        let mut hooks = Hooks::default();
        let mut builder = CallBuilderBase::new(machine, &mut assigner, &mut hooks);
        builder
            .add_arg(&CallArg::with_flags(1000, CallArgFlag::Byval, 32, 79))
            .unwrap();
        machine.special_count.set(3);
        for flag in [
            CallArgFlag::None,
            CallArgFlag::Sret,
            CallArgFlag::AllowSplit,
        ] {
            builder
                .add_arg(&CallArg::with_flags(1000, flag, 16, 0))
                .unwrap();
        }
        let events = machine.events.borrow();
        let Event::Byval(byval) = events[0] else {
            panic!()
        };
        assert!(byval.byval());
        assert_eq!(
            (byval.align, byval.size, byval.bank),
            (32, 79, RegBank::default())
        );
        for (index, event) in events[1..].iter().enumerate() {
            let Event::Stack(assignment, word) = event else {
                panic!()
            };
            assert_eq!(*word, 1000 + index as u64 % 3);
            assert_eq!(assignment.align, if index % 3 == 0 { 16 } else { 1 });
            assert_eq!(
                assignment.consecutive,
                if index >= 6 { 0 } else { (2 - index % 3) as u8 }
            );
            assert_eq!(assignment.sret(), (3..6).contains(&index));
        }
    });
}

#[test]
fn large_arguments_hold_consecutive_at_255_in_every_part() {
    with_machine(&[], 0, |machine| {
        let info = CCInfo::new(u32::MAX as u64, 0, 0);
        let mut assigner = CCAssigner::new(&info, policy(true));
        let mut hooks = Hooks::default();
        let mut builder = CallBuilderBase::new(machine, &mut assigner, &mut hooks);
        builder.add_arg_parts(&CallArg::new(1000), 256).unwrap();
        assert_eq!(machine.events.borrow().len(), 256);
        assert!(machine
            .events
            .borrow()
            .iter()
            .all(|e| matches!(e, Event::Stack(a, _) if a.consecutive == 255)));
    });
}

#[test]
fn constants_extend_directly_for_registers_and_for_stack() {
    for stack in [false, true] {
        for (ext, expected) in [(8, 0x80), (0x88, 0xffff_ffff_ffff_ff80)] {
            with_machine(&[], 0, |machine| {
                let info = CCInfo::new(u32::MAX as u64, 0, 0);
                let mut assigner = CCAssigner::new(&info, policy(stack));
                let mut hooks = Hooks::default();
                let mut builder = CallBuilderBase::new(machine, &mut assigner, &mut hooks);
                builder
                    .add_arg_part(
                        ValuePartRef::constant_inline(machine, 0x180, 2, RegBank::new(0)),
                        assignment(0, ext),
                    )
                    .unwrap();
                let event = machine.events.borrow()[0];
                if stack {
                    assert!(matches!(event, Event::Stack(_, word) if word == expected));
                } else {
                    assert_eq!(
                        event,
                        Event::Materialize(Reg::new(0), expected, RegBank::new(0), 8)
                    );
                }
            });
        }
    }
}

#[test]
fn abi_moves_cover_owned_and_borrowed_same_and_other_registers() {
    for references in [1, 2] {
        for source in [0, 7] {
            for ext in [0, 0x88] {
                with_machine(&[1], references, |machine| {
                    machine.assign_reg(0, 0, Reg::new(source));
                    let info = CCInfo::new(u32::MAX as u64, 0, 0);
                    let mut assigner = CCAssigner::new(&info, policy(false));
                    let mut hooks = Hooks::default();
                    let mut builder = CallBuilderBase::new(machine, &mut assigner, &mut hooks);
                    let value = machine.call_val_ref(&0).unwrap();
                    builder
                        .add_arg_part(value.part(0), assignment(0, ext))
                        .unwrap();
                    assert_eq!(builder.argument_registers(), 1);
                    assert_eq!(machine.mask() & 1, 0);
                    let events = machine.events.borrow();
                    if source == 0 && references == 2 {
                        assert_eq!(events[0], Event::Evict(Reg::new(0)));
                    }
                    if source != 0 {
                        assert_eq!(
                            events[0],
                            if ext != 0 && references == 1 {
                                Event::Extend(Reg::new(0), Reg::new(7), true, 8, 64)
                            } else {
                                Event::Move(Reg::new(0), Reg::new(7), 8)
                            }
                        );
                    }
                    if ext != 0 {
                        assert!(events
                            .iter()
                            .any(|e| matches!(e, Event::Extend(_, _, true, 8, 64))));
                    }
                });
            }
        }
    }
}

#[test]
fn call_evicts_only_clobbered_live_registers_and_keeps_target_until_hook() {
    with_machine(&[1, 1, 1], 1, |machine| {
        for (local, reg) in [(0, 5), (1, 8), (2, 9)] {
            machine.assign_reg(local, 0, Reg::new(reg));
        }
        let target = machine.call_val_ref(&0).unwrap();
        let info = CCInfo::new(u32::MAX as u64, 1 << 8, 0);
        let mut assigner = CCAssigner::new(&info, policy(false));
        let mut hooks = Hooks::default();
        let mut builder = CallBuilderBase::new(machine, &mut assigner, &mut hooks);
        builder.add_arg(&CallArg::new(1000)).unwrap();
        builder.call(CallTarget::Value(target.part(0))).unwrap();
        assert!(machine.generated_call.get());
        assert_eq!(machine.mask(), u32::MAX as u64);
        let events = machine.events.borrow();
        assert!(events.contains(&Event::Evict(Reg::new(9))));
        assert!(!events.contains(&Event::Evict(Reg::new(5))));
        assert!(!events.contains(&Event::Evict(Reg::new(8))));
        assert!(events.contains(&Event::Call(1 << 31 | 5, 8)));
        assert_eq!(machine.registers.borrow().used.get(), 1 << 8);
    });
}

#[test]
fn call_failure_and_cancellation_restore_reserved_registers() {
    for fail in [false, true] {
        with_machine(&[], 0, |machine| {
            let info = CCInfo::new(u32::MAX as u64, 0, 0);
            let mut assigner = CCAssigner::new(&info, policy(false));
            let mut hooks = Hooks {
                fail_call: fail,
                ..Hooks::default()
            };
            {
                let mut builder = CallBuilderBase::new(machine, &mut assigner, &mut hooks);
                builder.add_arg(&CallArg::new(1000)).unwrap();
                if fail {
                    assert!(builder.call(CallTarget::Symbol(SymRef::new(12))).is_err());
                    assert_eq!(builder.argument_registers(), 0);
                }
            }
            assert_eq!(machine.mask(), u32::MAX as u64);
            assert_eq!(machine.registers.borrow().used.get(), 0);
        });
    }
}

#[test]
fn argument_failure_releases_prior_mask_and_new_scratch() {
    with_machine(&[], 0, |machine| {
        let info = CCInfo::new(u32::MAX as u64, 0, 0);
        let mut assigner = CCAssigner::new(&info, policy(false));
        let mut hooks = Hooks::default();
        let mut builder = CallBuilderBase::new(machine, &mut assigner, &mut hooks);
        builder.add_arg(&CallArg::new(1000)).unwrap();
        machine.fail_emit.set(true);
        assert!(builder.add_arg(&CallArg::new(1000)).is_err());
        assert_eq!(builder.argument_registers(), 0);
        assert_eq!(machine.mask(), u32::MAX as u64);
        assert_eq!(machine.registers.borrow().used.get(), 0);
    });
    with_machine(&[], 0, |machine| {
        let info = CCInfo::new(u32::MAX as u64, 0, 0);
        let mut assigner = CCAssigner::new(&info, policy(true));
        let mut hooks = Hooks {
            fail_stack: true,
            ..Hooks::default()
        };
        let mut builder = CallBuilderBase::new(machine, &mut assigner, &mut hooks);
        assert!(builder.add_arg(&CallArg::new(1000)).is_err());
        assert_eq!(machine.registers.borrow().used.get(), 0);
    });
}

#[test]
fn leaf_and_extension_preconditions_unwind_without_mask_leak() {
    if !tpde_util::CHECKED {
        return;
    }
    with_machine(&[], 0, |machine| {
        let info = CCInfo::new(u32::MAX as u64, 0, 0);
        let mut assigner = CCAssigner::new(&info, policy(false));
        let mut hooks = Hooks::default();
        machine.leaf.set(true);
        assert!(catch_unwind(AssertUnwindSafe(|| {
            let mut builder = CallBuilderBase::new(machine, &mut assigner, &mut hooks);
            builder.add_arg(&CallArg::new(1000)).unwrap();
            builder.call(CallTarget::Symbol(SymRef::new(1))).unwrap();
        }))
        .is_err());
        assert!(!machine.generated_call.get());
        assert_eq!(machine.mask(), u32::MAX as u64);
        assert!(catch_unwind(AssertUnwindSafe(|| {
            let mut builder = CallBuilderBase::new(machine, &mut assigner, &mut hooks);
            builder
                .add_arg(&CallArg::with_flags(1000, CallArgFlag::Sext, 1, 0))
                .unwrap();
        }))
        .is_err());
        assert_eq!(machine.mask(), u32::MAX as u64);
    });
}

#[test]
fn return_assignment_uses_all_parts_and_preserves_definitions() {
    with_machine(&[2], 2, |machine| {
        let info = CCInfo::new(u32::MAX as u64, 0, 0);
        let mut assigner = CCAssigner::new(&info, policy(false));
        let mut hooks = Hooks::default();
        let mut builder = CallBuilderBase::new(machine, &mut assigner, &mut hooks);
        let mut value = machine.call_result_ref(&0).unwrap();
        builder.add_ret(&mut value).unwrap();
        drop(value);
        assert_eq!(machine.registers.borrow().used.get(), 3);
        machine.with_part(ValLocalIdx::new(0), 1, |part| {
            assert_eq!(part.get_reg(), Reg::new(1));
            assert!(part.modified());
        });
        machine.with_assignment(ValLocalIdx::new(0), |assignment| {
            assert_eq!(assignment.references_left(), 2)
        });
    });
}

#[test]
fn return_builder_resets_assigner_and_extends_after_materializing() {
    with_machine(&[], 0, |machine| {
        let info = CCInfo::new(u32::MAX as u64, 0, 0);
        let mut p = policy(false);
        p.next_ret = 7;
        p.extension = 0x88;
        let mut assigner = CCAssigner::new(&info, p);
        {
            let mut builder = RetBuilder::new(machine, &mut assigner);
            builder
                .add_part(
                    ValuePartRef::constant_inline(machine, 0x180, 2, RegBank::new(0)),
                    CCAssignment::new(),
                )
                .unwrap();
            assert_eq!(builder.return_registers(), 1);
            builder.ret().unwrap();
        }
        assert_eq!(assigner.policy().resets, 1);
        assert_eq!(machine.mask(), u32::MAX as u64);
        assert_eq!(
            *machine.events.borrow(),
            [
                Event::Materialize(Reg::new(0), 0x180, RegBank::new(0), 2),
                Event::Extend(Reg::new(0), Reg::new(0), true, 8, 64),
                Event::Epilog,
                Event::ReleaseRegisters
            ]
        );
    });
}

#[test]
fn return_full_value_and_error_restore_masks_and_cleanup() {
    for fail_epilog in [false, true] {
        with_machine(&[], 0, |machine| {
            let info = CCInfo::new(u32::MAX as u64, 0, 0);
            let mut assigner = CCAssigner::new(&info, policy(false));
            machine.special_count.set(2);
            machine.fail_epilog.set(fail_epilog);
            let mut builder = RetBuilder::new(machine, &mut assigner);
            builder.add(&1000).unwrap();
            assert_eq!(builder.return_registers(), 3);
            assert_eq!(builder.ret().is_err(), fail_epilog);
            assert_eq!(machine.mask(), u32::MAX as u64);
            assert_eq!(
                machine.events.borrow().last(),
                Some(&Event::ReleaseRegisters)
            );
        });
    }
    with_machine(&[], 0, |machine| {
        let info = CCInfo::new(u32::MAX as u64, 0, 0);
        let mut assigner = CCAssigner::new(&info, policy(false));
        let mut builder = RetBuilder::new(machine, &mut assigner);
        builder.add(&1000).unwrap();
        machine.fail_emit.set(true);
        assert!(builder.add(&1000).is_err());
        assert_eq!(machine.mask(), u32::MAX as u64);
    });
}

#[test]
fn byval_prologue_replaces_assignments_once_even_after_zero_use_release() {
    for references in [0, 2] {
        for frame_off in [None, Some(-72), Some(0)] {
            with_machine(&[1], references, |machine| {
                machine.byval_frame.set(frame_off);
                let info = CCInfo::new(u32::MAX as u64, 0, 0);
                let mut assigner = CCAssigner::new(&info, policy(false));
                prologue_assign_arg(machine, &mut assigner, 0, &0, 1, false).unwrap();
                let events = machine.events.borrow();
                let Event::Prologue(assignment) = events[0] else {
                    panic!()
                };
                assert!(assignment.byval());
                assert_eq!((assignment.align, assignment.size), (16, 24));
                if frame_off.is_some() {
                    assert_eq!(
                        events
                            .iter()
                            .filter(|event| matches!(event, Event::Free(0)))
                            .count(),
                        1
                    );
                    assert_eq!(events.last(), Some(&Event::Variable(0, frame_off.unwrap())));
                    machine.with_assignment(ValLocalIdx::new(0), |a| {
                        assert!(a.variable_ref());
                        assert!(a.stack_variable());
                        assert_eq!(a.frame_off(), frame_off.unwrap());
                    });
                }
            });
        }
    }
}

#[test]
fn prologue_sret_and_multipart_alignments_are_assigned_in_order() {
    for (arg_index, count, split) in [(1, 1, false), (2, 3, false), (2, 3, true)] {
        with_machine(&[count], 2, |machine| {
            let info = CCInfo::new(u32::MAX as u64, 0, 0);
            let mut assigner = CCAssigner::new(&info, policy(false));
            prologue_assign_arg(machine, &mut assigner, arg_index, &0, 272, split).unwrap();
            for (index, event) in machine.events.borrow().iter().enumerate() {
                let Event::Prologue(a) = event else { panic!() };
                assert_eq!(a.reg, Reg::new(index as u64));
                assert_eq!(a.size, 8);
                assert_eq!(a.sret(), arg_index == 1);
                if arg_index != 1 {
                    assert_eq!(a.align, if index == 0 { 16 } else { 1 });
                    assert_eq!(
                        a.consecutive,
                        if split {
                            0
                        } else {
                            (count - index as u32 - 1) as u8
                        }
                    );
                }
            }
        });
    }
}

#[test]
fn prologue_failure_releases_defining_reference_and_preserves_uses() {
    with_machine(&[2], 2, |machine| {
        machine.fail_emit.set(true);
        let info = CCInfo::new(u32::MAX as u64, 0, 0);
        let mut assigner = CCAssigner::new(&info, policy(false));
        assert!(prologue_assign_arg(machine, &mut assigner, 2, &0, 1, false).is_err());
        machine.with_assignment(ValLocalIdx::new(0), |a| assert_eq!(a.references_left(), 2));
        assert_eq!(machine.registers.borrow().used.get(), 0);
    });
}

#[test]
fn assigner_size_changes_affect_argument_moves_but_not_return_moves() {
    for returning in [false, true] {
        with_machine(&[1], 1, |machine| {
            machine.assign_reg(0, 0, Reg::new(7));
            let info = CCInfo::new(u32::MAX as u64, 0, 0);
            let mut p = policy(false);
            p.size_override = Some(4);
            let mut assigner = CCAssigner::new(&info, p);
            let value = machine.call_val_ref(&0).unwrap();
            if returning {
                let mut builder = RetBuilder::new(machine, &mut assigner);
                builder
                    .add_part(value.part(0), CCAssignment::new())
                    .unwrap();
            } else {
                let mut hooks = Hooks::default();
                let mut builder = CallBuilderBase::new(machine, &mut assigner, &mut hooks);
                builder
                    .add_arg_part(value.part(0), CCAssignment::new())
                    .unwrap();
            }
            assert_eq!(
                machine.events.borrow()[0],
                Event::Move(Reg::new(0), Reg::new(7), if returning { 8 } else { 4 })
            );
        });
    }
}

#[test]
fn full_argument_flags_supply_extension_sign_and_source_width() {
    for flag in [CallArgFlag::Zext, CallArgFlag::Sext] {
        with_machine(&[], 0, |machine| {
            let info = CCInfo::new(u32::MAX as u64, 0, 0);
            let mut assigner = CCAssigner::new(&info, policy(true));
            let mut hooks = Hooks::default();
            let mut builder = CallBuilderBase::new(machine, &mut assigner, &mut hooks);
            let mut arg = CallArg::with_flags(1000, flag, 1, 0);
            arg.ext_bits = 8;
            builder.add_arg(&arg).unwrap();
            let Event::Stack(assignment, word) = machine.events.borrow()[0] else {
                panic!()
            };
            assert_eq!(
                assignment.int_ext,
                if flag == CallArgFlag::Sext { 0x88 } else { 8 }
            );
            assert_eq!(
                word,
                if flag == CallArgFlag::Sext {
                    (-24i64) as u64
                } else {
                    232
                }
            );
        });
    }
}

#[test]
fn borrowed_indirect_targets_are_evicted_and_reloaded_or_preserved_on_failure() {
    for fail in [false, true] {
        with_machine(&[1], 2, |machine| {
            machine.assign_reg(0, 0, Reg::new(5));
            let info = CCInfo::new(u32::MAX as u64, 0, 0);
            let mut assigner = CCAssigner::new(&info, policy(false));
            let mut hooks = Hooks::default();
            let mut builder = CallBuilderBase::new(machine, &mut assigner, &mut hooks);
            builder.add_arg(&CallArg::new(1000)).unwrap();
            let value = machine.call_val_ref(&0).unwrap();
            machine.fail_emit.set(fail);
            assert_eq!(
                builder.call(CallTarget::Value(value.part(0))).is_err(),
                fail
            );
            assert_eq!(machine.mask(), u32::MAX as u64);
            let events = machine.events.borrow();
            assert!(events.contains(&Event::Evict(Reg::new(5))));
            if !fail {
                assert!(events.contains(&Event::Reload(Reg::new(1), ValLocalIdx::new(0), 0)));
                assert!(events.contains(&Event::Call(1 << 31 | 1, 8)));
            } else {
                assert_eq!(machine.registers.borrow().used.get(), 1 << 5);
                assert!(!events
                    .iter()
                    .any(|event| matches!(event, Event::Call(_, _))));
            }
        });
    }
}

#[test]
fn occupied_destination_is_evicted_before_salvage_move() {
    with_machine(&[1, 1], 1, |machine| {
        machine.assign_reg(0, 0, Reg::new(7));
        machine.assign_reg(1, 0, Reg::new(0));
        let info = CCInfo::new(u32::MAX as u64, 0, 0);
        let mut assigner = CCAssigner::new(&info, policy(false));
        let mut hooks = Hooks::default();
        let mut builder = CallBuilderBase::new(machine, &mut assigner, &mut hooks);
        let value = machine.call_val_ref(&0).unwrap();
        builder
            .add_arg_part(value.part(0), CCAssignment::new())
            .unwrap();
        assert_eq!(
            *machine.events.borrow(),
            [
                Event::Evict(Reg::new(0)),
                Event::Move(Reg::new(0), Reg::new(7), 8)
            ]
        );
        machine.with_part(ValLocalIdx::new(1), 0, |part| {
            assert!(part.stack_valid());
            assert!(!part.register_valid());
        });
    });
}

#[test]
fn explicit_argument_shapes_and_zero_part_uses_are_preserved() {
    with_machine(&[0], 1, |machine| {
        let info = CCInfo::new(u32::MAX as u64, 0, 0);
        let mut assigner = CCAssigner::new(&info, policy(true));
        let mut hooks = Hooks::default();
        let mut builder = CallBuilderBase::new(machine, &mut assigner, &mut hooks);
        let mut assignment = CCAssignment::new();
        assignment.bank = RegBank::new(0);
        assignment.size = 77;
        builder
            .add_arg_part(
                ValuePartRef::constant_inline(machine, 19, 2, RegBank::new(0)),
                assignment,
            )
            .unwrap();
        let Event::Stack(assigned, word) = machine.events.borrow()[0] else {
            panic!()
        };
        assert_eq!(
            (assigned.bank, assigned.size, word),
            (RegBank::new(0), 77, 19)
        );
        builder.add_arg(&CallArg::new(0)).unwrap();
        assert_eq!(
            &machine.events.borrow()[1..],
            [Event::Release(0), Event::Free(0)]
        );
        assert!(machine.assignments.borrow()[0].is_none());
    });
}

#[test]
fn failed_early_return_assignment_restores_argument_reservations() {
    with_machine(&[1], 2, |machine| {
        machine.assign_reg(0, 0, Reg::new(7));
        machine.with_part(ValLocalIdx::new(0), 0, |part| {
            part.set_fixed_assignment(true)
        });
        machine.with_registers(|registers| registers.mark_fixed(Reg::new(7)));
        let info = CCInfo::new(u32::MAX as u64, 0, 0);
        let mut assigner = CCAssigner::new(&info, policy(false));
        let mut hooks = Hooks::default();
        let mut builder = CallBuilderBase::new(machine, &mut assigner, &mut hooks);
        builder.add_arg(&CallArg::new(1000)).unwrap();
        let value = machine.call_result_ref(&0).unwrap();
        machine.fail_emit.set(true);
        assert!(builder
            .add_ret_part_owned(value.part(0), CCAssignment::new())
            .is_err());
        assert_eq!(machine.mask(), u32::MAX as u64);
        assert_eq!(builder.argument_registers(), 0);
        assert_eq!(machine.registers.borrow().used.get(), 1 << 7);
    });
}
