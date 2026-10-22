// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use mcx::{Mcx, MemoryContext};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    panic::{catch_unwind, AssertUnwindSafe},
};
use tpde_base::ValLocalIdx;
use tpde_compiler::{
    branches::*, generic_value_part::GenericValuePart, guard_machine::GuardMachine,
    scratch_reg::ScratchReg, value_part_ref::ValuePartRef, value_ref::ValueRef,
};
use tpde_core::{
    compiler::ValRefSpecial,
    ir_adaptor::{IrAdaptor, NoHighestValue, PhiRef},
    value_assignment::{with_assignment_allocator, Assignment, AssignmentInit},
    AssignmentPartRef, BlockIndex, LivenessInfo, Reg, RegBank, RegisterFile,
};
use tpde_function_writer::Label;
use types_error::{PgError, PgResult};

type Range = std::vec::IntoIter<u32>;
struct Adaptor {
    successors: Vec<u32>,
    phis: Vec<u32>,
    incoming: Vec<u32>,
    is_phi: Vec<bool>,
}
struct Phi(u32);
impl PhiRef<u32, u32> for Phi {
    fn incoming_count(&self) -> u32 {
        1
    }
    fn incoming_val_for_slot(&self, slot: u32) -> u32 {
        assert_eq!(slot, 0);
        self.0
    }
    fn incoming_block_for_slot(&self, slot: u32) -> u32 {
        assert_eq!(slot, 0);
        0
    }
    fn incoming_val_for_block(&self, block: u32) -> u32 {
        assert_eq!(block, 0);
        self.0
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
    const TPDE_LIVENESS_VISIT_ARGS: bool = false;
    type Funcs = Range;
    type FuncsToCompile = Range;
    type Args<'a> = Range;
    type StaticAllocas<'a> = Range;
    type Blocks<'a> = Range;
    type Successors<'a> = Range;
    type Insts<'a> = Range;
    type Phis<'a> = Range;
    type Operands<'a> = Range;
    type Results<'a> = Range;
    type Phi<'a> = Phi;
    type BlockFormat<'a> = u32;
    type ValueFormat<'a> = u32;
    type InstFormat<'a> = u32;
    fn func_count(&self) -> u32 {
        1
    }
    fn funcs(&self) -> Range {
        vec![0].into_iter()
    }
    fn funcs_to_compile(&self) -> Range {
        self.funcs()
    }
    fn func_link_name(&self, _: u32) -> &[u8] {
        b"branch_fixture"
    }
    fn func_extern(&self, _: u32) -> bool {
        false
    }
    fn func_only_local(&self, _: u32) -> bool {
        true
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
    fn cur_args(&self) -> Range {
        Vec::new().into_iter()
    }
    fn cur_arg_is_byval(&self, _: u32) -> bool {
        false
    }
    fn cur_arg_byval_size(&self, _: u32) -> u32 {
        panic!("no byval arguments")
    }
    fn cur_arg_byval_align(&self, _: u32) -> u32 {
        panic!("no byval arguments")
    }
    fn cur_arg_is_sret(&self, _: u32) -> bool {
        false
    }
    fn cur_static_allocas(&self) -> Range {
        Vec::new().into_iter()
    }
    fn cur_has_dynamic_alloca(&self) -> bool {
        false
    }
    fn cur_entry_block(&self) -> u32 {
        0
    }
    fn cur_blocks(&self) -> Range {
        (0..8).collect::<Vec<_>>().into_iter()
    }
    fn block_succs(&self, block: u32) -> Range {
        assert_eq!(block, 0);
        self.successors.clone().into_iter()
    }
    fn block_insts(&self, _: u32) -> Range {
        Vec::new().into_iter()
    }
    fn block_phis(&self, block: u32) -> Range {
        if block == 1 {
            self.phis.clone().into_iter()
        } else {
            Vec::new().into_iter()
        }
    }
    fn block_info(&self, _: u32) -> u32 {
        0
    }
    fn block_set_info(&self, _: u32, _: u32) {
        panic!("branch fixture does not run analyzer")
    }
    fn block_info2(&self, _: u32) -> u32 {
        0
    }
    fn block_set_info2(&self, _: u32, _: u32) {
        panic!("branch fixture does not run analyzer")
    }
    fn block_fmt_ref(&self, block: u32) -> u32 {
        block
    }
    fn val_local_idx(&self, value: u32) -> ValLocalIdx {
        ValLocalIdx::new(value)
    }
    fn val_ignore_in_liveness_analysis(&self, _: u32) -> bool {
        false
    }
    fn val_is_phi(&self, value: u32) -> bool {
        self.is_phi[value as usize]
    }
    fn val_as_phi(&self, value: u32) -> Phi {
        Phi(self.incoming[value as usize])
    }
    fn val_alloca_size(&self, _: u32) -> u32 {
        panic!("no alloca values")
    }
    fn val_alloca_align(&self, _: u32) -> u32 {
        panic!("no alloca values")
    }
    fn value_fmt_ref(&self, value: u32) -> u32 {
        value
    }
    fn inst_operands(&self, _: u32) -> Range {
        Vec::new().into_iter()
    }
    fn inst_results(&self, _: u32) -> Range {
        Vec::new().into_iter()
    }
    fn inst_fused(&self, _: u32) -> bool {
        false
    }
    fn inst_fmt_ref(&self, inst: u32) -> u32 {
        inst
    }
    fn start_compile(&mut self) {}
    fn end_compile(&mut self) {}
    fn switch_func(&mut self, _: u32) -> bool {
        true
    }
    fn reset(&mut self) {}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Jump {
    Always,
    True,
    False,
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    Jump(Jump, u32),
    Label(u32),
    Align(u32),
    Equal(u32, u64, bool),
    Binary(u32, u32, u64, bool),
    Table(u32, u64, u64, bool, Vec<u32>),
    Load(u8, i32, u32),
    Store(u8, i32, u32),
    Mov(u8, u8, u32),
    Extend(u32, u32),
    Spill(u32, u32),
    Free(u8),
    Allocate(i32, u32),
    Release(i32, u32),
    Value(u32),
    Result(u32),
    ReleaseAssignment(u32),
}
struct Machine<'m, 'a, 'id> {
    adaptor: Adaptor,
    assignments: &'a [Assignment<'a, 'id>],
    registers: RefCell<RegisterFile<2, 32>>,
    words: RefCell<[u64; 64]>,
    stack: RefCell<BTreeMap<i32, u64>>,
    events: RefCell<Vec<Event>>,
    phi_scratch: RefCell<PhiScratch<'m, u32>>,
    switch_scratch: RefCell<SwitchScratch<'m, u32>>,
    branch: Cell<bool>,
    current: Cell<u32>,
    next_label: Cell<u32>,
    next_stack: Cell<i32>,
    multiple: Cell<bool>,
    has_phis: Cell<bool>,
    accept_table: Cell<bool>,
    fail_emit: Cell<Option<usize>>,
    fail_restore: Cell<bool>,
    failure: Cell<bool>,
    released: RefCell<Vec<u32>>,
    last: Cell<u32>,
}
impl<'m, 'a, 'id> Machine<'m, 'a, 'id> {
    fn new(mcx: Mcx<'m>, assignments: &'a [Assignment<'a, 'id>]) -> Self {
        let mut registers = RegisterFile::new();
        registers.allocatable.set(u64::MAX);
        Self {
            adaptor: Adaptor {
                successors: vec![1],
                phis: Vec::new(),
                incoming: vec![0; assignments.len()],
                is_phi: vec![false; assignments.len()],
            },
            assignments,
            registers: RefCell::new(registers),
            words: RefCell::new([0; 64]),
            stack: RefCell::new(BTreeMap::new()),
            events: RefCell::new(Vec::new()),
            phi_scratch: RefCell::new(PhiScratch::new(mcx)),
            switch_scratch: RefCell::new(SwitchScratch::new(mcx)),
            branch: Cell::new(false),
            current: Cell::new(0),
            next_label: Cell::new(1000),
            next_stack: Cell::new(-10000),
            multiple: Cell::new(false),
            has_phis: Cell::new(false),
            accept_table: Cell::new(true),
            fail_emit: Cell::new(None),
            fail_restore: Cell::new(false),
            failure: Cell::new(false),
            released: RefCell::new(Vec::new()),
            last: Cell::new(7),
        }
    }
    fn emit(&self, event: Event) -> PgResult<()> {
        if self.fail_emit.get() == Some(self.events.borrow().len()) {
            return Err(PgError::error("injected branch emission failure").into());
        }
        self.events.borrow_mut().push(event);
        Ok(())
    }
    fn part(&self, local: u32, part: u32) -> AssignmentPartRef<'_, 'a, 'id> {
        AssignmentPartRef::new(&self.assignments[local as usize], part)
    }
    fn assign_reg(&self, local: u32, part: u32, reg: u8, word: u64, fixed: bool) {
        let ap = self.part(local, part);
        ap.set_bank(RegBank::new(reg / 32));
        ap.set_reg(Reg::new(u64::from(reg)));
        ap.set_register_valid(true);
        ap.set_modified(true);
        ap.set_fixed_assignment(fixed);
        self.registers.borrow_mut().mark_used(
            Reg::new(u64::from(reg)),
            ValLocalIdx::new(local),
            part,
        );
        if fixed {
            self.registers
                .borrow_mut()
                .mark_fixed(Reg::new(u64::from(reg)));
        }
        self.words.borrow_mut()[reg as usize] = word;
    }
    fn setup_phis(&mut self, incoming: &[u32]) {
        self.adaptor.phis = (0..incoming.len() as u32).collect();
        self.adaptor.incoming[..incoming.len()].copy_from_slice(incoming);
        self.adaptor.is_phi[..incoming.len()].fill(true);
        self.has_phis.set(true);
    }
    fn initialize_stack(&self) {
        for (local, assignment) in self.assignments.iter().enumerate() {
            for part in 0..assignment.part_count() {
                let ap = self.part(local as u32, part);
                if !ap.variable_ref() {
                    self.stack
                        .borrow_mut()
                        .insert(ap.frame_off(), 100 * local as u64 + part as u64 + 1);
                    ap.set_stack_valid();
                }
            }
        }
    }
    fn result_words(&self, local: u32) -> Vec<u64> {
        (0..self.assignments[local as usize].part_count())
            .map(|part| {
                let ap = self.part(local, part);
                if ap.fixed_assignment() {
                    self.words.borrow()[ap.get_reg().id() as usize]
                } else {
                    self.stack.borrow()[&ap.frame_off()]
                }
            })
            .collect()
    }
}
impl GuardMachine<2> for Machine<'_, '_, '_> {
    type Special = ValRefSpecial<()>;
    const GP_BANK: RegBank = RegBank::new(0);
    fn with_registers<R>(&self, operation: impl FnOnce(&RegisterFile<2, 32>) -> R) -> R {
        operation(&*self.registers.borrow_mut())
    }
    fn with_assignment<R>(
        &self,
        local: ValLocalIdx,
        operation: impl FnOnce(&Assignment<'_, '_>) -> R,
    ) -> R {
        operation(&self.assignments[local.get() as usize])
    }
    fn with_part<R>(
        &self,
        local: ValLocalIdx,
        part: u32,
        operation: impl FnOnce(AssignmentPartRef<'_, '_, '_>) -> R,
    ) -> R {
        operation(self.part(local.get(), part))
    }
    fn liveness(&self, _: ValLocalIdx) -> LivenessInfo {
        LivenessInfo {
            first: BlockIndex(0),
            last: BlockIndex(self.last.get()),
            last_full: true,
            ..LivenessInfo::default()
        }
    }
    fn current_block(&self) -> BlockIndex {
        BlockIndex(self.current.get())
    }
    fn may_change_value_state(&self) -> bool {
        !self.branch.get()
    }
    fn release_assignment(&self, local: ValLocalIdx) {
        self.events
            .borrow_mut()
            .push(Event::ReleaseAssignment(local.get()));
        self.released.borrow_mut().push(local.get());
    }
    fn decrement_fixed_count(&self, _: RegBank) {
        panic!("fixture assignments stay live")
    }
    fn select_reg(&self, bank: RegBank) -> PgResult<Reg> {
        let reg = self.registers.borrow().find_first_free_excluding(bank, 0);
        assert!(reg.valid());
        Ok(reg)
    }
    fn evict_reg(&self, reg: Reg) -> PgResult<()> {
        let (local, part) = self.with_registers(|r| (r.reg_local_idx(reg), r.reg_part(reg)));
        self.spill_part(local, part)?;
        self.free_reg(reg);
        Ok(())
    }
    fn reload_to_reg(&self, reg: Reg, local: ValLocalIdx, part: u32) -> PgResult<()> {
        let ap = self.part(local.get(), part);
        if ap.variable_ref() {
            self.words.borrow_mut()[reg.id() as usize] =
                self.assignments[local.get() as usize].var_ref_custom_idx() as u64;
            Ok(())
        } else {
            assert!(ap.stack_valid());
            self.load_from_stack(reg, ap.frame_off(), ap.part_size())
        }
    }
    fn mov(&self, destination: Reg, source: Reg, size: u32) -> PgResult<()> {
        self.emit(Event::Mov(destination.id(), source.id(), size))?;
        let word = self.words.borrow()[source.id() as usize];
        self.words.borrow_mut()[destination.id() as usize] = word;
        Ok(())
    }
    fn materialize_constant(&self, words: &[u64], _: RegBank, _: u32, reg: Reg) -> PgResult<()> {
        self.words.borrow_mut()[reg.id() as usize] = words[0];
        Ok(())
    }
    fn generate_raw_intext(
        &self,
        destination: Reg,
        source: Reg,
        sign: bool,
        from: u32,
        to: u32,
    ) -> PgResult<()> {
        assert_eq!(destination, source);
        assert!(!sign);
        self.emit(Event::Extend(from, to))
    }
    fn special_part<'s>(&'s self, _: &'s Self::Special, _: u32) -> ValuePartRef<'s, Self, 2> {
        panic!("fixture has no special values")
    }
    fn gval_expr_as_reg<'c>(&'c self, _: &mut GenericValuePart<'c, Self, 2>) -> PgResult<Reg> {
        panic!("fixture has no expressions")
    }
}
impl BranchMachine<2> for Machine<'_, '_, '_> {
    type Adaptor = Adaptor;
    type Jump = Jump;
    fn adaptor(&self) -> &Adaptor {
        &self.adaptor
    }
    fn block_idx(&self, block: u32) -> BlockIndex {
        BlockIndex(block)
    }
    fn block_ref(&self, index: BlockIndex) -> u32 {
        index.0
    }
    fn block_has_phis(&self, index: BlockIndex) -> bool {
        index.0 == 1 && self.has_phis.get()
    }
    fn block_has_multiple_incoming(&self, _: u32) -> bool {
        self.multiple.get()
    }
    fn block_label(&self, index: BlockIndex) -> Label {
        Label(index.0 + 100)
    }
    fn label_create(&self) -> PgResult<Label> {
        let label = self.next_label.get();
        self.next_label.set(label + 1);
        Ok(Label(label))
    }
    fn label_place(&self, label: Label) -> PgResult<()> {
        self.emit(Event::Label(label.0))
    }
    fn align_text(&self, alignment: u32) -> PgResult<()> {
        self.emit(Event::Align(alignment))
    }
    fn unconditional_jump(&self) -> Jump {
        Jump::Always
    }
    fn invert_jump(&self, jump: Jump) -> Jump {
        match jump {
            Jump::True => Jump::False,
            Jump::False => Jump::True,
            Jump::Always => panic!("cannot invert unconditional jump"),
        }
    }
    fn generate_raw_jump(&self, jump: Jump, label: Label) -> PgResult<()> {
        self.emit(Event::Jump(jump, label.0))
    }
    fn spill_part(&self, local: ValLocalIdx, part: u32) -> PgResult<()> {
        self.emit(Event::Spill(local.get(), part))?;
        let ap = self.part(local.get(), part);
        assert!(ap.register_valid());
        self.allocate_spill_part(local, part)?;
        self.spill_reg(ap.get_reg(), ap.frame_off(), ap.part_size())?;
        ap.set_stack_valid();
        Ok(())
    }
    fn free_reg(&self, reg: Reg) {
        self.events.borrow_mut().push(Event::Free(reg.id()));
        let (local, part) = self.with_registers(|r| (r.reg_local_idx(reg), r.reg_part(reg)));
        self.part(local.get(), part).set_register_valid(false);
        self.with_registers(|r| r.unmark_used(reg));
    }
    fn allocate_spill_part(&self, local: ValLocalIdx, _: u32) -> PgResult<()> {
        let assignment = &self.assignments[local.get() as usize];
        if assignment.frame_off() == 0 {
            assignment.set_frame_off(self.allocate_stack_slot(assignment.size())?);
        }
        Ok(())
    }
    fn load_from_stack(&self, reg: Reg, offset: i32, size: u32) -> PgResult<()> {
        if self.fail_restore.get()
            && self
                .events
                .borrow()
                .iter()
                .any(|e| matches!(e, Event::Store(..)))
        {
            return Err(PgError::error("injected restore failure").into());
        }
        self.emit(Event::Load(reg.id(), offset, size))?;
        self.words.borrow_mut()[reg.id() as usize] = self.stack.borrow()[&offset];
        Ok(())
    }
    fn spill_reg(&self, reg: Reg, offset: i32, size: u32) -> PgResult<()> {
        self.emit(Event::Store(reg.id(), offset, size))?;
        self.stack
            .borrow_mut()
            .insert(offset, self.words.borrow()[reg.id() as usize]);
        Ok(())
    }
    fn allocate_stack_slot(&self, size: u32) -> PgResult<i32> {
        let offset = self.next_stack.get();
        self.next_stack.set(offset - size as i32);
        self.emit(Event::Allocate(offset, size))?;
        Ok(offset)
    }
    fn free_stack_slot(&self, offset: i32, size: u32) {
        self.events.borrow_mut().push(Event::Release(offset, size));
    }
    fn free_stack_slot_reclassified(&self, offset: i32, _: u32, freed: u32) -> PgResult<()> {
        self.events.borrow_mut().push(Event::Release(offset, freed));
        Ok(())
    }
    fn value_ref(&self, value: u32) -> PgResult<ValueRef<'_, Self, 2>> {
        self.emit(Event::Value(value))?;
        Ok(ValueRef::new(self, ValLocalIdx::new(value)))
    }
    fn result_ref(&self, value: u32) -> PgResult<ValueRef<'_, Self, 2>> {
        self.emit(Event::Result(value))?;
        Ok(ValueRef::new(self, ValLocalIdx::new(value)))
    }
    fn assignment_exists(&self, local: ValLocalIdx) -> bool {
        !self.released.borrow().contains(&local.get())
    }
    fn set_branch_region(&self, active: bool) {
        assert_ne!(self.branch.replace(active), active);
    }
    fn record_failure(&self, _: Box<PgError>) {
        self.failure.set(true);
    }
    fn with_phi_scratch<R>(
        &self,
        operation: impl FnOnce(&mut PhiScratch<'_, u32>) -> PgResult<R>,
    ) -> PgResult<R> {
        operation(&mut self.phi_scratch.borrow_mut())
    }
    fn with_switch_scratch<R>(
        &self,
        operation: impl FnOnce(&mut SwitchScratch<'_, u32>) -> PgResult<R>,
    ) -> PgResult<R> {
        operation(&mut self.switch_scratch.borrow_mut())
    }
    fn switch_emit_cmpeq(
        &self,
        label: Label,
        _: Reg,
        _: Reg,
        value: u64,
        width32: bool,
    ) -> PgResult<()> {
        self.emit(Event::Equal(label.0, value, width32))
    }
    fn switch_emit_binary_step(
        &self,
        equal: Label,
        greater: Label,
        _: Reg,
        _: Reg,
        value: u64,
        width32: bool,
    ) -> PgResult<()> {
        self.emit(Event::Binary(equal.0, greater.0, value, width32))
    }
    fn switch_create_jump_table(
        &self,
        default: Label,
        _: Reg,
        _: Reg,
        low: u64,
        high: u64,
        width32: bool,
        fill: impl FnOnce(&mut [Label]),
    ) -> PgResult<bool> {
        if !self.accept_table.get() {
            return Ok(false);
        }
        let mut labels = vec![Label(0); high.wrapping_sub(low).wrapping_add(1) as usize];
        fill(&mut labels);
        self.emit(Event::Table(
            default.0,
            low,
            high,
            width32,
            labels.into_iter().map(|label| label.0).collect(),
        ))?;
        Ok(true)
    }
}

fn with_machine<R>(
    parts: &[u32],
    run: impl for<'m, 'a, 'id> FnOnce(Machine<'m, 'a, 'id>) -> R,
) -> R {
    let context = MemoryContext::new("branch-fixture");
    with_assignment_allocator(context.mcx(), |allocator| {
        let assignments: Vec<_> = parts
            .iter()
            .enumerate()
            .map(|(i, parts)| {
                allocator.allocate(*parts).unwrap().initialize(
                    AssignmentInit {
                        frame_off: -100 * (i as i32 + 1),
                        references_left: 1000,
                        max_part_size: 8,
                        ..AssignmentInit::default()
                    },
                    |_| 3 << 12,
                )
            })
            .collect();
        let machine = Machine::new(context.mcx(), &assignments);
        machine.initialize_stack();
        run(machine)
    })
}

#[test]
fn spill_decisions_preserve_fallthrough_and_liveness() {
    with_machine(&[1, 1, 1, 1], |mut machine| {
        for local in 0..4 {
            machine.assign_reg(local, 0, local as u8, local as u64 + 99, false);
        }
        assert_eq!(spill_before_branch(&machine, false).unwrap(), 0);
        assert!(machine.events.borrow().is_empty());
        machine.adaptor.successors = vec![1, 3];
        machine.last.set(2);
        assert_eq!(spill_before_branch(&machine, false).unwrap(), 0);
        assert!(machine.events.borrow().is_empty());
        machine.last.set(3);
        assert_eq!(spill_before_branch(&machine, false).unwrap(), 0);
        assert_eq!(
            machine
                .events
                .borrow()
                .iter()
                .filter(|event| matches!(event, Event::Spill(..)))
                .count(),
            4
        );
        machine.events.borrow_mut().clear();
        machine.multiple.set(true);
        assert_eq!(spill_before_branch(&machine, false).unwrap(), 15);
        assert!(machine.events.borrow().is_empty());
        release_spilled_regs(&machine, 15);
        assert_eq!(machine.registers.borrow().used.get(), 0);
    });
}

#[test]
fn forced_spill_skips_fixed_scratch_variable_and_dying_values() {
    with_machine(&[1, 1, 1, 1], |machine| {
        machine.assign_reg(0, 0, 0, 11, true);
        machine.assign_reg(1, 0, 1, 22, false);
        machine.assign_reg(2, 0, 2, 33, false);
        machine.assign_reg(3, 0, 3, 44, false);
        machine.assignments[2].set_variable_ref(true);
        machine.part(3, 0).set_stack_valid();
        let mut scratch = ScratchReg::new(&machine);
        scratch.alloc_specific(Reg::new(4)).unwrap();
        assert_eq!(spill_before_branch(&machine, true).unwrap(), 14);
        assert_eq!(
            machine
                .events
                .borrow()
                .iter()
                .filter(|event| matches!(event, Event::Spill(..)))
                .cloned()
                .collect::<Vec<_>>(),
            vec![Event::Spill(1, 0)]
        );
        machine.events.borrow_mut().clear();
        machine.part(1, 0).set_modified(true);
        machine.last.set(0);
        assert_eq!(spill_before_branch(&machine, true).unwrap(), 14);
        assert!(machine.events.borrow().is_empty());
    });
}

#[test]
fn releases_intersect_current_used_and_preserve_locks() {
    with_machine(&[1, 1, 1], |machine| {
        machine.assign_reg(0, 0, 0, 1, true);
        machine.assign_reg(1, 0, 1, 2, false);
        machine.assign_reg(2, 0, 2, 3, false);
        machine.part(1, 0).set_stack_valid();
        machine.part(2, 0).set_stack_valid();
        release_spilled_regs(&machine, 0b1011);
        assert_eq!(machine.registers.borrow().used.get(), 0b101);
        release_regs_after_return(&machine);
        assert_eq!(machine.registers.borrow().used.get(), 1);
        assert_eq!(
            *machine.events.borrow(),
            vec![Event::Free(1), Event::Free(2)]
        );
    });
}

#[test]
fn branch_order_uses_fallthrough_then_split_priority() {
    for (true_target, false_target, expected) in [
        (1, 2, vec![Event::Jump(Jump::False, 102)]),
        (2, 1, vec![Event::Jump(Jump::True, 102)]),
        (
            2,
            3,
            vec![Event::Jump(Jump::True, 102), Event::Jump(Jump::Always, 103)],
        ),
    ] {
        with_machine(&[], |machine| {
            generate_cond_branch(&machine, Jump::True, true_target, false_target).unwrap();
            assert_eq!(*machine.events.borrow(), expected);
            assert!(!machine.branch.get());
        });
    }
    with_machine(&[], |machine| {
        generate_uncond_branch(&machine, 1).unwrap();
        assert!(machine.events.borrow().is_empty());
        generate_uncond_branch(&machine, 3).unwrap();
        assert_eq!(
            *machine.events.borrow(),
            vec![Event::Jump(Jump::Always, 103)]
        );
    });
}

#[test]
fn split_branch_emits_inverse_guard_and_restores_region_after_failure() {
    with_machine(&[1, 1], |mut machine| {
        machine.setup_phis(&[1]);
        generate_branch_to_block(&machine, Jump::True, 1, true, false).unwrap();
        assert_eq!(
            machine.events.borrow().first(),
            Some(&Event::Jump(Jump::False, 1000))
        );
        assert_eq!(
            &machine.events.borrow()[machine.events.borrow().len() - 2..],
            &[Event::Jump(Jump::Always, 101), Event::Label(1000)]
        );
        machine.events.borrow_mut().clear();
        machine.fail_emit.set(Some(0));
        assert!(generate_cond_branch(&machine, Jump::True, 2, 1).is_err());
        assert!(!machine.branch.get());
        machine.fail_emit.set(None);
        generate_uncond_branch(&machine, 1).unwrap();
        assert_eq!(machine.result_words(0), vec![101]);
    });
}

fn emit_switch(machine: &Machine<'_, '_, '_>, width: u32, cases: &[(u64, u32)]) -> PgResult<()> {
    let mut condition = ScratchReg::new(machine);
    condition.alloc_gp()?;
    generate_switch(machine, condition, width, 7, cases)
}

#[test]
fn switch_small_ranges_widths_and_empty_default() {
    for width in [0, 1, 31, 32, 33, 63, 64] {
        for count in 0..=4 {
            with_machine(&[], |machine| {
                let cases: Vec<_> = (0..count).map(|i| (i as u64 * 5, i + 2)).collect();
                emit_switch(&machine, width, &cases).unwrap();
                let events = machine.events.borrow();
                let compares: Vec<_> = events
                    .iter()
                    .filter_map(|e| {
                        if let Event::Equal(label, value, narrow) = e {
                            Some((*label, *value, *narrow))
                        } else {
                            None
                        }
                    })
                    .collect();
                assert_eq!(
                    compares,
                    cases
                        .iter()
                        .map(|(value, block)| (block + 100, *value, width <= 32))
                        .collect::<Vec<_>>()
                );
                assert_eq!(events.last(), Some(&Event::Jump(Jump::Always, 107)));
                assert_eq!(
                    events.iter().any(|e| matches!(e, Event::Extend(..))),
                    width % 32 != 0
                );
                assert!(!machine.branch.get());
                assert_eq!(machine.registers.borrow().used.get(), 0);
            });
        }
    }
}

#[test]
fn switch_density_threshold_holes_and_target_decline() {
    for (high, accepted) in [(38, true), (39, false)] {
        with_machine(&[], |machine| {
            let cases = [(0, 2), (1, 3), (2, 4), (3, 5), (high, 6)];
            emit_switch(&machine, 64, &cases).unwrap();
            let events = machine.events.borrow();
            let table = events.iter().find_map(|e| {
                if let Event::Table(default, low, top, _, labels) = e {
                    Some((*default, *low, *top, labels))
                } else {
                    None
                }
            });
            assert_eq!(table.is_some(), accepted);
            if let Some((default, low, top, labels)) = table {
                assert_eq!((low, top), (0, high));
                assert_eq!(&labels[..4], &[102, 103, 104, 105]);
                assert_eq!(labels[high as usize], 106);
                assert!(labels[4..high as usize]
                    .iter()
                    .all(|label| *label == default));
            }
        });
    }
    with_machine(&[], |machine| {
        machine.accept_table.set(false);
        emit_switch(&machine, 32, &[(0, 2), (1, 3), (2, 4), (3, 5), (4, 6)]).unwrap();
        assert!(machine
            .events
            .borrow()
            .iter()
            .any(|e| matches!(e, Event::Binary(_, _, 2, true))));
        assert!(!machine
            .events
            .borrow()
            .iter()
            .any(|e| matches!(e, Event::Table(..))));
    });
}

#[test]
fn switch_full_word_overflow_and_phi_intermediate_blocks() {
    with_machine(&[], |machine| {
        emit_switch(
            &machine,
            64,
            &[(0, 2), (1, 3), (2, 4), (3, 5), (u64::MAX, 6)],
        )
        .unwrap();
        assert!(machine
            .events
            .borrow()
            .iter()
            .any(|e| matches!(e, Event::Binary(_, _, 2, false))));
        assert!(!machine
            .events
            .borrow()
            .iter()
            .any(|e| matches!(e, Event::Table(..))));
    });
    with_machine(&[1, 1], |mut machine| {
        machine.setup_phis(&[1]);
        emit_switch(&machine, 32, &[(1, 1), (2, 1)]).unwrap();
        let events = machine.events.borrow();
        assert_eq!(events.iter().filter(|e| **e == Event::Align(8)).count(), 2);
        assert_eq!(events.iter().filter(|e| **e == Event::Result(0)).count(), 2);
        let labels: Vec<_> = events
            .iter()
            .filter_map(|e| {
                if let Event::Equal(label, _, _) = e {
                    Some(*label)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(labels, vec![1000, 1001]);
        assert_eq!(machine.result_words(0), vec![101]);
    });
}

#[test]
fn switch_failure_releases_scratch_and_allows_reuse() {
    with_machine(&[], |machine| {
        machine.fail_emit.set(Some(1));
        assert!(emit_switch(&machine, 32, &[(1, 2), (2, 3)]).is_err());
        assert!(!machine.branch.get());
        assert_eq!(machine.registers.borrow().used.get(), 0);
        machine.fail_emit.set(None);
        machine.events.borrow_mut().clear();
        emit_switch(&machine, 32, &[(9, 5)]).unwrap();
        assert_eq!(
            machine
                .events
                .borrow()
                .iter()
                .filter(|e| matches!(e, Event::Equal(..)))
                .count(),
            1
        );
        if tpde_util::CHECKED {
            assert!(catch_unwind(AssertUnwindSafe(|| emit_switch(&machine, 65, &[]))).is_err());
        }
        assert_eq!(machine.registers.borrow().used.get(), 0);
    });
}

#[test]
fn phi_self_references_are_counted_on_both_paths() {
    with_machine(&[1], |mut machine| {
        machine.setup_phis(&[0]);
        move_to_phi_nodes(&machine, BlockIndex(1)).unwrap();
        assert_eq!(
            *machine.events.borrow(),
            vec![Event::Result(0), Event::Value(0)]
        );
        assert_eq!(machine.assignments[0].references_left(), 998);
    });
    with_machine(&[1, 1, 1], |mut machine| {
        machine.setup_phis(&[0, 2, 1]);
        move_to_phi_nodes(&machine, BlockIndex(1)).unwrap();
        assert_eq!(
            &machine.events.borrow()[..2],
            &[Event::Value(0), Event::Value(0)]
        );
        assert_eq!(machine.result_words(0), vec![1]);
        assert_eq!(machine.result_words(1), vec![201]);
        assert_eq!(machine.result_words(2), vec![101]);
    });
}

#[test]
fn phi_chains_fanout_and_external_phi_use_original_values() {
    for incoming in [&[1, 2, 3][..], &[2, 2, 3][..], &[3, 4, 3][..]] {
        with_machine(&[1, 1, 1, 1, 1], |mut machine| {
            machine.setup_phis(incoming);
            machine.adaptor.is_phi[3] = true;
            let expected: Vec<_> = incoming
                .iter()
                .map(|local| machine.result_words(*local))
                .collect();
            move_to_phi_nodes(&machine, BlockIndex(1)).unwrap();
            for (local, expected) in expected.iter().enumerate() {
                assert_eq!(&machine.result_words(local as u32), expected);
            }
            let order: Vec<_> = machine
                .events
                .borrow()
                .iter()
                .filter_map(|e| {
                    if let Event::Result(local) = e {
                        Some(*local)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(order, vec![0, 1, 2]);
        });
    }
}

#[test]
fn phi_cycles_preserve_one_two_and_many_parts_across_banks() {
    for parts in [1, 2, 3, 5] {
        for incoming in [&[1, 0][..], &[1, 2, 0][..], &[1, 0, 3, 2][..]] {
            with_machine(&vec![parts; incoming.len()], |mut machine| {
                machine.setup_phis(incoming);
                for local in 0..incoming.len() as u32 {
                    for part in 0..parts {
                        machine
                            .part(local, part)
                            .set_bank(RegBank::new((part % 2) as u8));
                    }
                }
                let expected: Vec<_> = incoming
                    .iter()
                    .map(|local| machine.result_words(*local))
                    .collect();
                move_to_phi_nodes(&machine, BlockIndex(1)).unwrap();
                for (local, expected) in expected.iter().enumerate() {
                    assert_eq!(&machine.result_words(local as u32), expected);
                }
                assert_eq!(machine.registers.borrow().used.get(), 0);
                assert!(!machine.failure.get());
                let events = machine.events.borrow();
                let allocations: Vec<_> = events
                    .iter()
                    .filter_map(|e| {
                        if let Event::Allocate(offset, size) = e {
                            Some((*offset, *size))
                        } else {
                            None
                        }
                    })
                    .collect();
                let releases: Vec<_> = events
                    .iter()
                    .filter_map(|e| {
                        if let Event::Release(offset, size) = e {
                            Some((*offset, *size))
                        } else {
                            None
                        }
                    })
                    .collect();
                assert_eq!(allocations, releases);
                assert_eq!(allocations.is_empty(), parts <= 2);
            });
        }
    }
}

#[test]
fn phi_fixed_cycle_and_multi_part_registered_capture() {
    with_machine(&[1, 1], |mut machine| {
        machine.setup_phis(&[1, 0]);
        machine.assign_reg(0, 0, 7, 11, true);
        machine.assign_reg(1, 0, 8, 22, true);
        move_to_phi_nodes(&machine, BlockIndex(1)).unwrap();
        assert_eq!(machine.result_words(0), vec![22]);
        assert_eq!(machine.result_words(1), vec![11]);
        assert_eq!(machine.registers.borrow().used.get(), (1 << 7) | (1 << 8));
    });
    with_machine(&[3, 3], |mut machine| {
        machine.setup_phis(&[1, 0]);
        for local in 0..2 {
            for part in 0..3 {
                machine.assign_reg(
                    local,
                    part,
                    (local * 3 + part) as u8,
                    100 * local as u64 + part as u64 + 1,
                    false,
                );
            }
        }
        machine.assignments[0].set_frame_off(0);
        move_to_phi_nodes(&machine, BlockIndex(1)).unwrap();
        assert_eq!(machine.result_words(0), vec![101, 102, 103]);
        assert_eq!(machine.result_words(1), vec![1, 2, 3]);
    });
}

#[test]
fn phi_scratch_backup_restores_register_ownership_and_dirty_state() {
    with_machine(&[1, 1, 1], |mut machine| {
        machine.setup_phis(&[1]);
        machine.assign_reg(2, 0, 0, 333, false);
        machine.registers.borrow_mut().allocatable.set(1);
        let before = machine.registers.borrow().used.get();
        move_to_phi_nodes(&machine, BlockIndex(1)).unwrap();
        assert_eq!(machine.result_words(0), vec![101]);
        assert_eq!(machine.registers.borrow().used.get(), before);
        assert_eq!(
            machine.registers.borrow().reg_local_idx(Reg::new(0)),
            ValLocalIdx::new(2)
        );
        assert!(machine.part(2, 0).register_valid());
        assert!(machine.part(2, 0).modified());
        assert_eq!(machine.words.borrow()[0], 333);
        assert!(!machine.registers.borrow().is_fixed(Reg::new(0)));
    });
}

#[test]
fn phi_scratch_variable_backup_preserves_upstream_no_reload() {
    with_machine(&[1, 1, 1], |mut machine| {
        machine.setup_phis(&[1]);
        machine.assign_reg(2, 0, 0, 333, false);
        machine.assignments[2].set_variable_ref(true);
        machine.registers.borrow_mut().allocatable.set(1);
        move_to_phi_nodes(&machine, BlockIndex(1)).unwrap();
        assert_eq!(machine.words.borrow()[0], 101);
        assert!(machine.part(2, 0).register_valid());
        assert!(machine.part(2, 0).modified());
        assert_eq!(
            machine
                .events
                .borrow()
                .iter()
                .filter(|e| matches!(e, Event::Load(..)))
                .count(),
            1
        );
    });
}

#[test]
fn phi_temporary_stack_and_registers_release_on_emission_failure() {
    with_machine(&[3, 3], |mut machine| {
        machine.setup_phis(&[1, 0]);
        machine.fail_emit.set(Some(3));
        assert!(move_to_phi_nodes(&machine, BlockIndex(1)).is_err());
        assert_eq!(machine.registers.borrow().used.get(), 0);
        assert!(machine
            .events
            .borrow()
            .iter()
            .any(|e| matches!(e, Event::Release(_, 24))));
        machine.fail_emit.set(None);
        machine.events.borrow_mut().clear();
        move_to_phi_nodes(&machine, BlockIndex(1)).unwrap();
        assert_eq!(machine.result_words(0), vec![101, 102, 103]);
        assert_eq!(machine.result_words(1), vec![1, 2, 3]);
    });
}

#[test]
fn cycle_destination_reference_releases_before_temporary_stack() {
    with_machine(&[3, 3], |mut machine| {
        machine.setup_phis(&[1, 0]);
        machine.assignments[1].set_references_left(2);
        move_to_phi_nodes(&machine, BlockIndex(1)).unwrap();
        let events = machine.events.borrow();
        let reference_release = events
            .iter()
            .position(|e| *e == Event::ReleaseAssignment(1))
            .unwrap();
        let stack_release = events
            .iter()
            .position(|e| matches!(e, Event::Release(_, 24)))
            .unwrap();
        assert!(reference_release < stack_release);
        assert_eq!(machine.assignments[1].references_left(), 0);
        assert_eq!(machine.result_words(0), vec![101, 102, 103]);
        assert_eq!(machine.result_words(1), vec![1, 2, 3]);
    });
}

#[test]
fn switch_rejects_condition_from_another_compiler_before_emission() {
    if !tpde_util::CHECKED {
        return;
    }
    let context = MemoryContext::new("branch-owner-check");
    let first = Machine::new(context.mcx(), &[]);
    let second = Machine::new(context.mcx(), &[]);
    let mut condition = ScratchReg::new(&second);
    condition.alloc_gp().unwrap();
    assert!(catch_unwind(AssertUnwindSafe(|| generate_switch(
        &first,
        condition,
        32,
        7,
        &[]
    )))
    .is_err());
    assert!(first.events.borrow().is_empty());
    assert!(second.events.borrow().is_empty());
    assert_eq!(first.registers.borrow().used.get(), 0);
    assert_eq!(second.registers.borrow().used.get(), 0);
}

#[test]
fn phi_packed_waiting_boundaries_and_scratch_exhaustion() {
    for count in [16, 17, 32, 33, 64, 65, 256, 257] {
        with_machine(&vec![1; count], |mut machine| {
            let incoming: Vec<_> = (0..count).map(|i| ((i + 1) % count) as u32).collect();
            machine.setup_phis(&incoming);
            let expected: Vec<_> = incoming
                .iter()
                .map(|local| machine.result_words(*local))
                .collect();
            move_to_phi_nodes(&machine, BlockIndex(1)).unwrap();
            for (local, expected) in expected.iter().enumerate() {
                assert_eq!(&machine.result_words(local as u32), expected);
            }
            assert_eq!(machine.registers.borrow().used.get(), 0);
        });
    }
    with_machine(&vec![1; 258], |mut machine| {
        let mut incoming: Vec<_> = (0..258).collect();
        incoming.swap(256, 257);
        machine.setup_phis(&incoming);
        move_to_phi_nodes(&machine, BlockIndex(1)).unwrap();
        assert_eq!(machine.result_words(256), vec![25701]);
        assert_eq!(machine.result_words(257), vec![25601]);
        assert_eq!(machine.result_words(255), vec![25501]);
        assert_eq!(machine.registers.borrow().used.get(), 0);
    });
    with_machine(&[1, 1], |mut machine| {
        machine.setup_phis(&[1]);
        machine.registers.borrow_mut().allocatable.set(0);
        assert!(move_to_phi_nodes(&machine, BlockIndex(1)).is_err());
        assert_eq!(machine.registers.borrow().used.get(), 0);
        machine.registers.borrow_mut().allocatable.set(1);
        move_to_phi_nodes(&machine, BlockIndex(1)).unwrap();
        assert_eq!(machine.result_words(0), vec![101]);
    });
}
