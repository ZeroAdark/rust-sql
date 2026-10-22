// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use std::{
    cell::{Cell, RefCell},
    panic::{catch_unwind, AssertUnwindSafe},
    string::String,
    vec::Vec,
};
use tpde_base::ValLocalIdx;
use tpde_compiler::{
    fixed_register_guard::FixedRegisterBackup,
    guard_machine::{GuardMachine, INVALID_VALUE},
    GenericValuePart, ScratchReg, ValuePartRef,
};
use tpde_core::{
    compiler::ValRefSpecial, register_file::Assignment, AssignmentPartRef, BlockIndex,
    LivenessInfo, Reg, RegBank, RegisterFile,
};
use types_error::{PgError, PgResult};

#[derive(Clone, Debug, PartialEq, Eq)]
struct State {
    allocatable: u64,
    used: u64,
    clobbered: u64,
    clocks: [u8; 2],
    assignments: [[Assignment; 32]; 2],
    locks: [[u8; 32]; 2],
}

struct Machine {
    registers: RefCell<RegisterFile<2, 32>>,
    values: RefCell<[u64; 64]>,
    changes: Cell<bool>,
    fail_select: Cell<bool>,
    selections: RefCell<Vec<State>>,
    register_accesses: Cell<usize>,
}

impl Machine {
    fn new() -> Self {
        let mut registers = RegisterFile::new();
        registers.allocatable.set(u64::MAX);
        registers.clocks[0].set(13);
        registers.clocks[1].set(29);
        Self {
            registers: RefCell::new(registers),
            values: RefCell::new([0; 64]),
            changes: Cell::new(true),
            fail_select: Cell::new(false),
            selections: RefCell::new(Vec::new()),
            register_accesses: Cell::new(0),
        }
    }

    fn state(&self) -> State {
        let registers = self.registers.borrow();
        State {
            allocatable: registers.allocatable.get(),
            used: registers.used.get(),
            clobbered: registers.clobbered.get(),
            clocks: registers.clocks.each_ref().map(Cell::get),
            assignments: registers
                .assignments
                .each_ref()
                .map(|bank| bank.each_ref().map(Cell::get)),
            locks: registers
                .lock_counts
                .each_ref()
                .map(|bank| bank.each_ref().map(Cell::get)),
        }
    }

    fn install(&self, id: u64, locks: u8) -> Reg {
        let reg = Reg::new(id);
        let mut registers = self.registers.borrow_mut();
        registers.mark_used(reg, ValLocalIdx::new(91 + id as u32), 0xf123_4567);
        registers.lock_counts[id as usize / 32][id as usize % 32].set(locks);
        self.values.borrow_mut()[id as usize] = 0xa1b2_c3d4_5678_0000 | id;
        reg
    }

    fn copy(&self, destination: Reg, source: Reg) {
        let mut values = self.values.borrow_mut();
        values[destination.id() as usize] = values[source.id() as usize];
    }

    fn restored(&self, before: &State, fixed: Reg) {
        let now = self.state();
        let bank = fixed.id() as usize / 32;
        let slot = fixed.id() as usize % 32;
        assert_eq!(now.assignments[bank][slot], before.assignments[bank][slot]);
        assert_eq!(now.locks[bank][slot], before.locks[bank][slot]);
        assert_ne!(now.used & (1 << fixed.id()), 0);
        assert_eq!(
            now.clobbered & (1 << fixed.id()),
            before.clobbered & (1 << fixed.id())
        );
        assert_eq!(now.allocatable, before.allocatable);
        assert_eq!(now.clocks, before.clocks);
    }

    fn reuse(&self, fixed: Reg) {
        let before = self.state();
        let backup = FixedRegisterBackup::begin(self, fixed).unwrap();
        drop(backup);
        self.restored(&before, fixed);
        assert_eq!(self.state().used, before.used);
    }
}

impl GuardMachine<2> for Machine {
    type Special = ValRefSpecial<()>;
    const GP_BANK: RegBank = RegBank::new(0);

    fn with_registers<R>(&self, operation: impl FnOnce(&RegisterFile<2, 32>) -> R) -> R {
        self.register_accesses.set(self.register_accesses.get() + 1);
        operation(&*self.registers.borrow_mut())
    }
    fn with_assignment<R>(
        &self,
        _: ValLocalIdx,
        _: impl FnOnce(&tpde_core::value_assignment::Assignment<'_, '_>) -> R,
    ) -> R {
        panic!("fixture has no value allocations")
    }
    fn with_part<R>(
        &self,
        _: ValLocalIdx,
        _: u32,
        _: impl FnOnce(AssignmentPartRef<'_, '_, '_>) -> R,
    ) -> R {
        panic!("fixture has no value allocations")
    }
    fn liveness(&self, _: ValLocalIdx) -> LivenessInfo {
        panic!("fixture has no value allocations")
    }
    fn current_block(&self) -> BlockIndex {
        BlockIndex(0)
    }
    fn may_change_value_state(&self) -> bool {
        self.changes.get()
    }
    fn release_assignment(&self, _: ValLocalIdx) {
        panic!("fixture has no value allocations")
    }
    fn decrement_fixed_count(&self, _: RegBank) {
        panic!("fixture has no fixed assignments")
    }
    fn select_reg(&self, bank: RegBank) -> PgResult<Reg> {
        self.selections.borrow_mut().push(self.state());
        if self.fail_select.get() {
            return Err(PgError::error("injected backup allocation failure").into());
        }
        let free = self.registers.borrow().find_first_free_excluding(bank, 0);
        if free.valid() {
            return Ok(free);
        }
        for id in bank.id() as u64 * 32..bank.id() as u64 * 32 + 32 {
            let reg = Reg::new(id);
            let registers = self.registers.borrow();
            if !registers.is_used(reg) || registers.is_fixed(reg) {
                continue;
            }
            assert_ne!(
                registers.reg_local_idx(reg),
                INVALID_VALUE,
                "detached temporary cannot be evicted"
            );
            drop(registers);
            self.evict_reg(reg)?;
            return Ok(reg);
        }
        Err(PgError::error("all registers fixed").into())
    }
    fn evict_reg(&self, reg: Reg) -> PgResult<()> {
        self.registers.borrow_mut().unmark_used(reg);
        Ok(())
    }
    fn reload_to_reg(&self, _: Reg, _: ValLocalIdx, _: u32) -> PgResult<()> {
        panic!("fixture has no value allocations")
    }
    fn mov(&self, destination: Reg, source: Reg, size: u32) -> PgResult<()> {
        assert_eq!(size, 8);
        self.copy(destination, source);
        Ok(())
    }
    fn materialize_constant(&self, _: &[u64], _: RegBank, _: u32, _: Reg) -> PgResult<()> {
        panic!("unexpected constant materialization")
    }
    fn generate_raw_intext(&self, _: Reg, _: Reg, _: bool, _: u32, _: u32) -> PgResult<()> {
        panic!("unexpected integer extension")
    }
    fn gval_expr_as_reg<'c>(&'c self, _: &mut GenericValuePart<'c, Self, 2>) -> PgResult<Reg> {
        panic!("unexpected expression materialization")
    }
    fn special_part<'s>(&'s self, _: &'s Self::Special, _: u32) -> ValuePartRef<'s, Self, 2> {
        panic!("fixture has no special values")
    }
}

fn assert_panic_message(error: &(dyn core::any::Any + Send), expected: &str) {
    let message = error
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| error.downcast_ref::<&str>().copied())
        .expect("non-string panic payload");
    assert!(message.contains(expected), "unexpected panic: {message}");
}

#[test]
fn result_restores_metadata_before_exchange_and_transfers_one_owner() {
    let machine = Machine::new();
    let fixed = machine.install(4, 7);
    let before = machine.state();
    let original = machine.values.borrow()[4];
    let backup = FixedRegisterBackup::begin(&machine, fixed).unwrap();
    let saved = backup.backup_reg();
    machine.copy(saved, fixed);
    machine.values.borrow_mut()[4] = 0xfeed_face;
    let result = backup
        .restore_with(true, |destination, source| {
            assert_eq!((destination, source), (fixed, saved));
            machine.restored(&before, fixed);
            machine
                .values
                .borrow_mut()
                .swap(destination.id() as usize, source.id() as usize);
            Ok(())
        })
        .unwrap()
        .unwrap();
    assert_eq!(result.cur_reg(), saved);
    assert_eq!(machine.values.borrow()[4], original);
    assert_eq!(machine.values.borrow()[saved.id() as usize], 0xfeed_face);
    assert_eq!(machine.state().used, before.used | (1 << saved.id()));
    assert_eq!(machine.state().locks[0][saved.id() as usize], 1);
    drop(result);
    assert_eq!(machine.state().used, before.used);
    assert_eq!(
        machine.state().clobbered,
        before.clobbered | (1 << saved.id())
    );
    machine.reuse(fixed);
}

#[test]
fn nonresult_restores_before_move_and_releases_backup() {
    let machine = Machine::new();
    let fixed = machine.install(31, 255);
    machine.registers.borrow_mut().mark_clobbered(fixed);
    let before = machine.state();
    let original = machine.values.borrow()[31];
    let backup = FixedRegisterBackup::begin(&machine, fixed).unwrap();
    let saved = backup.backup_reg();
    machine.copy(saved, fixed);
    machine.values.borrow_mut()[31] = 0;
    assert!(backup
        .restore_with(false, |destination, source| {
            machine.restored(&before, fixed);
            machine.mov(destination, source, 8)
        })
        .unwrap()
        .is_none());
    assert_eq!(machine.values.borrow()[31], original);
    assert_eq!(machine.state().used, before.used);
    assert_eq!(machine.state().locks[0][saved.id() as usize], 0);
    assert_eq!(
        machine.state().clobbered,
        before.clobbered | (1 << saved.id())
    );
}

#[test]
fn every_nonzero_lock_depth_survives_active_drop() {
    for count in 1..=255 {
        let machine = Machine::new();
        let fixed = machine.install(5, count);
        let before = machine.state();
        let backup = FixedRegisterBackup::begin(&machine, fixed).unwrap();
        assert_eq!(machine.state().locks[0][5], 0);
        drop(backup);
        machine.restored(&before, fixed);
        assert_eq!(machine.state().used, before.used);
    }
}

#[test]
fn allocation_precedes_detachment_and_changes_only_target_metadata() {
    let machine = Machine::new();
    let fixed = machine.install(9, 3);
    machine.install(42, 11);
    let before = machine.state();
    let backup = FixedRegisterBackup::begin(&machine, fixed).unwrap();
    assert_eq!(machine.selections.borrow().as_slice(), &[before.clone()]);
    let mut expected = before.clone();
    let saved = backup.backup_reg().id() as usize;
    expected.used |= 1 << saved;
    expected.clobbered |= 1 << saved;
    expected.assignments[0][saved] = Assignment {
        local_idx: INVALID_VALUE,
        part: 0,
    };
    expected.locks[0][saved] = 1;
    expected.assignments[0][9] = Assignment {
        local_idx: INVALID_VALUE,
        part: 0,
    };
    expected.locks[0][9] = 0;
    assert_eq!(machine.state(), expected);
    assert_eq!(backup.fixed_reg(), fixed);
    drop(backup);
    machine.restored(&before, fixed);
}

#[test]
fn allocation_error_preserves_all_metadata_and_allows_reuse() {
    let machine = Machine::new();
    let fixed = machine.install(7, 13);
    let before = machine.state();
    machine.fail_select.set(true);
    assert!(FixedRegisterBackup::begin(&machine, fixed).is_err());
    assert_eq!(machine.state(), before);
    machine.fail_select.set(false);
    machine.reuse(fixed);
}

#[test]
fn backup_allocation_can_evict_another_assignment_before_detachment() {
    let machine = Machine::new();
    for id in 0..32 {
        machine.install(id, u8::from(id != 0));
    }
    let fixed = Reg::new(4);
    let before = machine.state();
    let backup = FixedRegisterBackup::begin(&machine, fixed).unwrap();
    assert_eq!(backup.backup_reg(), Reg::new(0));
    assert_eq!(machine.selections.borrow().as_slice(), &[before.clone()]);
    drop(backup);
    machine.restored(&before, fixed);
    assert_eq!(machine.state().used, before.used & !1);
}

#[test]
fn invalid_register_bank_state_and_frozen_machine_refuse_before_allocation() {
    if !tpde_util::CHECKED {
        return;
    }
    for id in [255, 64, 32, 3, 4] {
        let machine = Machine::new();
        machine.install(32, 1);
        machine.install(4, 0);
        let before = machine.state();
        assert!(catch_unwind(AssertUnwindSafe(|| FixedRegisterBackup::begin(
            &machine,
            Reg::new(id)
        )))
        .is_err());
        assert_eq!(machine.state(), before);
        assert!(machine.selections.borrow().is_empty());
    }
    let machine = Machine::new();
    let fixed = machine.install(4, 1);
    let before = machine.state();
    machine.changes.set(false);
    assert!(catch_unwind(AssertUnwindSafe(|| FixedRegisterBackup::begin(
        &machine, fixed
    )))
    .is_err());
    assert_eq!(machine.state(), before);
    assert!(machine.selections.borrow().is_empty());
}

#[test]
fn initial_copy_failure_abandons_output_and_restores_for_reuse() {
    let machine = Machine::new();
    let fixed = machine.install(4, 3);
    let before = machine.state();
    let attempt = || -> PgResult<()> {
        let _backup = FixedRegisterBackup::begin(&machine, fixed)?;
        Err(PgError::error("injected initial move failure").into())
    };
    assert!(attempt().is_err());
    machine.restored(&before, fixed);
    assert_eq!(machine.state().used, before.used);
    machine.reuse(fixed);
}

#[test]
fn restore_emission_errors_drop_backup_once_for_both_result_modes() {
    for is_result in [false, true] {
        let machine = Machine::new();
        let fixed = machine.install(4, 19);
        let before = machine.state();
        let backup = FixedRegisterBackup::begin(&machine, fixed).unwrap();
        let accesses = machine.register_accesses.get();
        assert!(backup
            .restore_with(is_result, |_, _| {
                machine.restored(&before, fixed);
                assert_eq!(machine.register_accesses.get(), accesses + 1);
                Err(PgError::error("injected restoration emission failure").into())
            })
            .is_err());
        assert_eq!(machine.register_accesses.get(), accesses + 2);
        assert_eq!(machine.state().used, before.used);
        machine.reuse(fixed);
    }
}

#[test]
fn restore_emission_panics_do_not_repeat_metadata_restoration() {
    for is_result in [false, true] {
        let machine = Machine::new();
        let fixed = machine.install(4, 31);
        let before = machine.state();
        let backup = FixedRegisterBackup::begin(&machine, fixed).unwrap();
        let accesses = machine.register_accesses.get();
        assert!(catch_unwind(AssertUnwindSafe(|| backup.restore_with(
            is_result,
            |_, _| {
                machine.restored(&before, fixed);
                panic!("injected restoration emission unwind")
            }
        )))
        .is_err());
        assert_eq!(machine.register_accesses.get(), accesses + 2);
        assert_eq!(machine.state().used, before.used);
        machine.reuse(fixed);
    }
}

#[test]
fn nested_backups_allow_other_allocations_and_restore_in_either_order() {
    for first_first in [false, true] {
        let machine = Machine::new();
        let a = machine.install(4, 7);
        let b = machine.install(5, 11);
        let before = machine.state();
        let first = FixedRegisterBackup::begin(&machine, a).unwrap();
        let second = FixedRegisterBackup::begin(&machine, b).unwrap();
        let mut unrelated = ScratchReg::new(&machine);
        let unrelated_reg = unrelated.alloc_gp().unwrap();
        assert_ne!(unrelated_reg, first.backup_reg());
        assert_ne!(unrelated_reg, second.backup_reg());
        if first_first {
            drop(first);
            drop(second);
        } else {
            drop(second);
            drop(first);
        }
        machine.restored(&before, a);
        machine.restored(&before, b);
        assert_eq!(
            machine.state().used,
            before.used | (1 << unrelated_reg.id())
        );
        assert!(machine.registers.borrow().is_fixed(unrelated_reg));
        drop(unrelated);
        assert_eq!(machine.state().used, before.used);
    }
}

#[test]
fn inherited_pressure_refusal_unwinds_backups_and_can_reuse_machine() {
    let machine = Machine::new();
    for id in 1..32 {
        machine.install(id, 1);
    }
    let before = machine.state();
    let first_succeeded = Cell::new(false);
    let error = catch_unwind(AssertUnwindSafe(|| {
        let _first = FixedRegisterBackup::begin(&machine, Reg::new(4)).unwrap();
        first_succeeded.set(true);
        assert_eq!(machine.state().locks[0][4], 0);
        assert_eq!(machine.state().assignments[0][4].local_idx, INVALID_VALUE);
        assert_ne!(machine.state().used & (1 << 4), 0);
        let _second = FixedRegisterBackup::begin(&machine, Reg::new(5)).unwrap();
    }))
    .unwrap_err();
    assert!(first_succeeded.get());
    assert_panic_message(&*error, "detached temporary cannot be evicted");
    machine.restored(&before, Reg::new(4));
    machine.restored(&before, Reg::new(5));
    assert_eq!(machine.state().used, before.used);
    machine.reuse(Reg::new(4));
}

#[test]
fn live_scratch_owner_reset_is_rejected_until_backup_restores_it() {
    if !tpde_util::CHECKED {
        return;
    }
    let machine = Machine::new();
    let mut owner = ScratchReg::new(&machine);
    let fixed = owner.alloc_specific(Reg::new(4)).unwrap();
    let before = machine.state();
    let backup = FixedRegisterBackup::begin(&machine, fixed).unwrap();
    let detached = machine.state();
    let error = catch_unwind(AssertUnwindSafe(|| owner.reset())).unwrap_err();
    assert_panic_message(&*error, "register does not have one lock");
    assert_eq!(owner.cur_reg(), fixed);
    assert_eq!(machine.state(), detached);
    drop(backup);
    machine.restored(&before, fixed);
    owner.reset();
    assert_eq!(machine.state().used, 0);
}

#[test]
fn dropping_original_owner_is_invalid_and_requires_abandoned_function_reset() {
    if !tpde_util::CHECKED {
        return;
    }
    let machine = Machine::new();
    let mut owner = ScratchReg::new(&machine);
    let fixed = owner.alloc_specific(Reg::new(4)).unwrap();
    let before = machine.state();
    let backup = FixedRegisterBackup::begin(&machine, fixed).unwrap();
    let error = catch_unwind(AssertUnwindSafe(|| drop(owner))).unwrap_err();
    assert_panic_message(&*error, "register does not have one lock");
    drop(backup);
    machine.restored(&before, fixed);
    assert_eq!(machine.state().used, 1 << 4);
    machine.registers.borrow_mut().reset();
    let fixed = machine.install(4, 1);
    machine.reuse(fixed);
}

#[test]
fn moving_guard_preserves_one_cleanup_owner_and_small_stack_storage() {
    assert!(core::mem::size_of::<FixedRegisterBackup<'_, Machine, 2>>() <= 64);
    let machine = Machine::new();
    let fixed = machine.install(4, 1);
    let before = machine.state();
    let first = FixedRegisterBackup::begin(&machine, fixed).unwrap();
    let second = first;
    let accesses = machine.register_accesses.get();
    drop(second);
    assert_eq!(machine.register_accesses.get(), accesses + 2);
    machine.restored(&before, fixed);
    assert_eq!(machine.state().used, before.used);
}
