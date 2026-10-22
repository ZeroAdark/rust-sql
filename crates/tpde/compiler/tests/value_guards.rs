// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{
    generic_value_part::GenericValuePart,
    guard_machine::{GuardMachine, INVALID_VALUE},
    scratch_reg::ScratchReg,
    value_part_ref::ValuePartRef,
    value_ref::ValueRef,
};
use mcx::MemoryContext;
use std::{
    cell::{Cell, RefCell},
    panic::{catch_unwind, AssertUnwindSafe},
    vec::Vec,
};
use tpde_base::ValLocalIdx;
use tpde_core::{
    compiler::ValRefSpecial,
    value_assignment::{with_assignment_allocator, Assignment, AssignmentInit},
    AssignmentPartRef, BlockIndex, LivenessInfo, Reg, RegBank, RegisterFile,
};
use types_error::{PgError, PgResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Select,
    Evict,
    Reload,
    Move,
    Constant,
    Extend,
    Release(u32),
}

struct Machine<'a, 'id> {
    assignments: [Assignment<'a, 'id>; 3],
    registers: RefCell<RegisterFile<2, 32>>,
    values: RefCell<[[u64; 16]; 64]>,
    events: RefCell<Vec<Event>>,
    fail: Cell<Option<Event>>,
    evict_extra: Cell<Option<Reg>>,
    fixed: Cell<u32>,
    changes: Cell<bool>,
}

impl<'a, 'id> Machine<'a, 'id> {
    fn new(assignments: [Assignment<'a, 'id>; 3]) -> Self {
        let mut registers = RegisterFile::new();
        registers.allocatable.set(u64::MAX);
        Self {
            assignments,
            registers: RefCell::new(registers),
            values: RefCell::new([[0; 16]; 64]),
            events: RefCell::new(Vec::new()),
            fail: Cell::new(None),
            evict_extra: Cell::new(None),
            fixed: Cell::new(0),
            changes: Cell::new(true),
        }
    }
    fn event(&self, event: Event) -> PgResult<()> {
        self.events.borrow_mut().push(event);
        if self.fail.get() == Some(event) {
            self.fail.set(None);
            return Err(PgError::error("injected guard operation failure").into());
        }
        Ok(())
    }
    fn part(&self, local: u32, part: u32, owned: bool) -> ValuePartRef<'_, Self, 2> {
        let local = ValLocalIdx::new(local);
        let pointer = self.with_assignment(local, |assignment| assignment.pointer());
        ValuePartRef::from_assignment(self, local, pointer, part, owned)
    }
    fn used(&self) -> u64 {
        self.registers.borrow().used.get()
    }
    fn word(&self, reg: Reg) -> u64 {
        self.values.borrow()[reg.id() as usize][0]
    }
    fn install(&self, local: u32, part: u32, reg: Reg, fixed: bool, word: u64) {
        self.with_part(ValLocalIdx::new(local), part, |assignment| {
            assignment.set_reg(reg);
            assignment.set_register_valid(true);
            assignment.set_fixed_assignment(fixed);
        });
        self.with_registers(|registers| {
            registers.mark_used(reg, ValLocalIdx::new(local), part);
            if fixed {
                registers.mark_fixed(reg);
            }
        });
        if fixed {
            self.fixed.set(self.fixed.get() + 1);
        }
        self.values.borrow_mut()[reg.id() as usize][0] = word;
    }
}

impl GuardMachine<2> for Machine<'_, '_> {
    type Special = ValRefSpecial<u64>;
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
        operation(AssignmentPartRef::new(
            &self.assignments[local.get() as usize],
            part,
        ))
    }
    fn liveness(&self, _: ValLocalIdx) -> LivenessInfo {
        LivenessInfo {
            first: BlockIndex(0),
            last: BlockIndex(2),
            ref_count: 9,
            lowest_common_loop: 0,
            last_full: false,
            epoch: 0,
            weight: 0,
            crosses_call: false,
            crosses_cold_call: false,
        }
    }
    fn current_block(&self) -> BlockIndex {
        BlockIndex(2)
    }
    fn may_change_value_state(&self) -> bool {
        self.changes.get()
    }
    fn release_assignment(&self, local: ValLocalIdx) {
        self.events.borrow_mut().push(Event::Release(local.get()));
        let assignment = &self.assignments[local.get() as usize];
        for index in 0..assignment.part_count() {
            let part = AssignmentPartRef::new(assignment, index);
            if part.register_valid() {
                let reg = part.get_reg();
                if part.fixed_assignment() {
                    self.with_registers(|registers| {
                        registers.dec_lock_count_must_zero(reg, 1);
                    });
                    self.decrement_fixed_count(part.bank());
                }
                self.with_registers(|registers| registers.unmark_used(reg));
                part.set_register_valid(false);
                part.set_fixed_assignment(false);
            }
        }
        assignment.set_pending_free(true);
    }
    fn decrement_fixed_count(&self, _: RegBank) {
        self.fixed.set(self.fixed.get().checked_sub(1).unwrap());
    }
    fn select_reg(&self, bank: RegBank) -> PgResult<Reg> {
        self.event(Event::Select)?;
        let free = self.registers.borrow().find_first_free_excluding(bank, 0);
        if free.valid() {
            return Ok(free);
        }
        let reg = self
            .registers
            .borrow()
            .find_first_nonfixed_excluding(bank, 0);
        if !reg.valid() {
            return Err(PgError::error("all registers fixed").into());
        }
        self.evict_reg(reg)?;
        Ok(reg)
    }
    fn evict_reg(&self, reg: Reg) -> PgResult<()> {
        self.event(Event::Evict)?;
        if let Some(extra) = self.evict_extra.take() {
            self.evict_reg(extra)?;
        }
        let (local, part) = self.with_registers(|registers| {
            assert!(!registers.is_fixed(reg));
            (registers.reg_local_idx(reg), registers.reg_part(reg))
        });
        if local != INVALID_VALUE {
            self.with_part(local, part, |assignment| {
                assignment.set_register_valid(false);
                assignment.set_stack_valid();
            });
        }
        self.with_registers(|registers| registers.unmark_used(reg));
        Ok(())
    }
    fn reload_to_reg(&self, reg: Reg, local: ValLocalIdx, part: u32) -> PgResult<()> {
        self.event(Event::Reload)?;
        self.values.borrow_mut()[reg.id() as usize][0] =
            0x1000 + u64::from(local.get()) * 16 + u64::from(part);
        Ok(())
    }
    fn mov(&self, destination: Reg, source: Reg, size: u32) -> PgResult<()> {
        self.event(Event::Move)?;
        let mut values = self.values.borrow_mut();
        let source = values[source.id() as usize];
        let count = size.div_ceil(8) as usize;
        values[destination.id() as usize][..count].copy_from_slice(&source[..count]);
        Ok(())
    }
    fn materialize_constant(&self, words: &[u64], _: RegBank, size: u32, reg: Reg) -> PgResult<()> {
        self.event(Event::Constant)?;
        let count = size.div_ceil(8) as usize;
        self.values.borrow_mut()[reg.id() as usize][..count].copy_from_slice(&words[..count]);
        Ok(())
    }
    fn generate_raw_intext(
        &self,
        destination: Reg,
        source: Reg,
        sign: bool,
        from: u32,
        _: u32,
    ) -> PgResult<()> {
        self.event(Event::Extend)?;
        let word = self.word(source);
        self.values.borrow_mut()[destination.id() as usize][0] = if sign {
            ((word << (64 - from)) as i64 >> (64 - from)) as u64
        } else {
            (word << (64 - from)) >> (64 - from)
        };
        Ok(())
    }
    fn gval_expr_as_reg<'c>(&'c self, _: &mut GenericValuePart<'c, Self, 2>) -> PgResult<Reg> {
        panic!("fixture has no expressions")
    }
    fn special_part<'s>(
        &'s self,
        special: &'s Self::Special,
        part: u32,
    ) -> ValuePartRef<'s, Self, 2> {
        ValuePartRef::constant_inline(self, special.payload + u64::from(part), 8, Self::GP_BANK)
    }
}

fn fixture(run: impl FnOnce(&Machine<'_, '_>)) {
    let context = MemoryContext::new("value-guard-tests");
    with_assignment_allocator(context.mcx(), |allocator| {
        let assignments = core::array::from_fn(|_| {
            allocator.allocate(2).unwrap().initialize(
                AssignmentInit {
                    references_left: 1,
                    max_part_size: 8,
                    ..AssignmentInit::default()
                },
                |_| 0x3200,
            )
        });
        let machine = Machine::new(assignments);
        run(&machine);
    });
}

#[test]
fn reference_modes_disown_moves_specials_and_exact_release() {
    fixture(|machine| {
        machine.assignments[0].set_references_left(3);
        for remaining in [2, 1, 0] {
            let mut value = ValueRef::new(machine, ValLocalIdx::new(0));
            assert_eq!(value.is_owned(), remaining == 0);
            let mut moved = value.take();
            assert_eq!(value.local_idx(), INVALID_VALUE);
            assert_eq!(
                moved.with_assignment(|assignment| assignment.part_count()),
                2
            );
            {
                let unowned = moved.disowned();
                assert!(!unowned.is_owned());
            }
            moved.reset();
            moved.reset();
            assert_eq!(machine.assignments[0].references_left(), remaining);
        }
        assert_eq!(
            machine
                .events
                .borrow()
                .iter()
                .filter(|event| **event == Event::Release(0))
                .count(),
            1
        );
        machine.assignments[1].set_variable_ref(true);
        machine.assignments[1].set_references_left(0);
        {
            let value = ValueRef::new(machine, ValLocalIdx::new(1));
            assert!(value.variable_ref());
        }
        machine.assignments[2].set_delay_free(true);
        let mut delayed = ValueRef::new(machine, ValLocalIdx::new(2));
        assert!(!delayed.is_owned());
        delayed.disown();
        drop(delayed);
        assert_eq!(machine.assignments[2].references_left(), 1);
        let special = ValueRef::special(machine, ValRefSpecial::new(4, 0x1234));
        assert!(!special.has_assignment());
        assert_eq!(special.part(2).const_data(), &[0x1236]);
        assert_eq!(special.part_unowned(3).const_data(), &[0x1237]);
    });
}

#[test]
fn unowned_nested_locks_and_fixed_owned_release() {
    fixture(|machine| {
        machine.install(0, 0, Reg::new(3), false, 91);
        let mut first = machine.part(0, 0, false);
        let mut second = machine.part(0, 0, false);
        assert_eq!(first.cur_reg_unlocked(), Reg::new(3));
        assert!(first.is_in_reg(Reg::new(3)));
        assert!(!first.has_reg());
        first.lock();
        second.lock();
        assert_eq!(machine.registers.borrow().lock_counts[0][3].get(), 2);
        first.unlock();
        first.unlock();
        second.reset();
        assert_eq!(machine.registers.borrow().lock_counts[0][3].get(), 0);
        assert_eq!(machine.used(), 1 << 3);
        machine.install(1, 0, Reg::new(9), true, 17);
        let value = ValueRef::new(machine, ValLocalIdx::new(1));
        {
            let mut part = value.part(0);
            part.lock();
            assert_eq!(machine.registers.borrow().lock_counts[0][9].get(), 2);
        }
        assert_eq!(machine.fixed.get(), 0);
        assert_eq!(machine.used(), 1 << 3);
        drop(value);
        assert!(machine.assignments[1].pending_free());
    });
}

#[test]
fn constants_materialize_all_words_and_preserve_unowned_borrow() {
    fixture(|machine| {
        let words = [0x1234, 0x5678, 0x9abc];
        let mut wide = ValuePartRef::constant_borrowed(machine, &words, 17, RegBank::new(1));
        assert_eq!(wide.const_data(), &words);
        wide.load_to_specific(Reg::new(40)).unwrap();
        assert_eq!(&machine.values.borrow()[40][..3], &words);
        {
            let unowned = wide.get_unowned();
            assert_eq!(unowned.cur_reg(), Reg::new(40));
            assert_eq!(unowned.bank(), RegBank::default());
            assert_eq!(unowned.part_size(), 0);
        }
        assert_eq!(machine.used(), 1 << 40);
        assert_eq!(wide.cur_reg_or_load().unwrap(), Reg::new(40));
        wide.reset();
        wide.reset();
        assert!(wide.is_const());
        assert_eq!(machine.used(), 0);
        let mut inline = ValuePartRef::constant_inline(machine, u64::MAX, 1, RegBank::new(0));
        assert_eq!(inline.load_to_reg().unwrap(), Reg::new(0));
        assert_eq!(machine.word(inline.cur_reg()), u64::MAX);
    });
}

#[test]
fn allocation_reloads_reuses_and_specific_moves_preserve_value() {
    fixture(|machine| {
        let mut part = machine.part(0, 0, false);
        assert_eq!(part.load_to_reg().unwrap(), Reg::new(0));
        assert_eq!(machine.word(part.cur_reg()), 0x1000);
        assert_eq!(part.cur_reg_or_alloc().unwrap(), Reg::new(0));
        part.unlock();
        let before = machine.events.borrow().len();
        part.load_to_reg().unwrap();
        assert_eq!(machine.events.borrow().len(), before);
        part.unlock();
        part.load_to_specific(Reg::new(6)).unwrap();
        assert_eq!(machine.word(part.cur_reg()), 0x1000);
        assert_eq!(machine.used(), 1 << 6);
        part.unlock();
        part.alloc_specific(Reg::new(6)).unwrap();
        part.set_modified();
        assert!(part.with_assignment(|assignment| assignment.modified()));
        let mut scratch = ScratchReg::new(machine);
        let reg = scratch.alloc_gp().unwrap();
        part.reload_into_specific_fixed(reg, 0).unwrap();
        assert_eq!(machine.word(reg), 0x1000);
        part.reset();
    });
}

#[test]
fn last_use_salvage_moves_assignment_register_and_keeps_source_metadata() {
    fixture(|machine| {
        machine.install(0, 0, Reg::new(4), true, 73);
        let mut source = machine.part(0, 0, true);
        source.lock();
        let mut result = machine.part(1, 0, false);
        assert_eq!(result.alloc_try_reuse(&mut source).unwrap(), Reg::new(4));
        assert!(source.has_assignment());
        assert_eq!(source.local_idx(), ValLocalIdx::new(0));
        assert!(!source.has_reg());
        assert!(!source.can_salvage());
        assert_eq!(machine.fixed.get(), 0);
        assert_eq!(
            machine.registers.borrow().reg_local_idx(Reg::new(4)),
            ValLocalIdx::new(1)
        );
        assert_eq!(machine.word(result.cur_reg()), 73);
        result.unlock();
        assert_eq!(machine.registers.borrow().lock_counts[0][4].get(), 0);
    });
}

#[test]
fn non_last_use_copy_and_fixed_destination_keep_source_register() {
    fixture(|machine| {
        machine.install(0, 0, Reg::new(4), false, 81);
        machine.install(1, 0, Reg::new(8), true, 0);
        let mut result = machine.part(1, 0, false);
        result.set_value(machine.part(0, 0, false)).unwrap();
        assert!(!result.has_reg());
        assert_eq!(machine.word(Reg::new(8)), 81);
        assert_eq!(machine.registers.borrow().lock_counts[0][8].get(), 1);
        assert_eq!(machine.used(), (1 << 4) | (1 << 8));
        let mut temporary = ValuePartRef::temporary(machine, RegBank::new(0));
        temporary.set_value(machine.part(0, 0, false)).unwrap();
        assert_eq!(machine.word(temporary.cur_reg()), 81);
        assert_ne!(temporary.cur_reg(), Reg::new(4));
    });
}

#[test]
fn temporary_constant_and_scratch_transfers_release_once() {
    fixture(|machine| {
        let mut temporary = ValuePartRef::temporary(machine, RegBank::new(0));
        temporary.alloc_reg().unwrap();
        let mut constant = ValuePartRef::constant_inline(machine, 19, 8, RegBank::new(1));
        constant.load_to_reg().unwrap();
        temporary.set_value_from(&mut constant).unwrap();
        assert_eq!(temporary.bank(), RegBank::new(1));
        assert_eq!(constant.bank(), RegBank::new(1));
        assert!(!constant.is_const());
        assert!(!constant.has_reg());
        assert_eq!(machine.used(), 1 << 32);
        let mut scratch = temporary.into_scratch().unwrap();
        assert_eq!(scratch.cur_reg(), Reg::new(32));
        let mut result = ValuePartRef::temporary(machine, RegBank::new(1));
        result.set_scratch(scratch.take()).unwrap();
        assert!(!scratch.has_reg());
        assert_eq!(result.cur_reg(), Reg::new(32));
        result.reset();
        assert_eq!(machine.used(), 0);
    });
}

#[test]
fn scratch_and_raw_result_initialization_handle_fixed_and_unfixed_parts() {
    fixture(|machine| {
        let mut scratch = ScratchReg::new(machine);
        scratch.alloc_specific(Reg::new(2)).unwrap();
        machine.values.borrow_mut()[2][0] = 28;
        let mut result = machine.part(0, 0, false);
        result.set_scratch(scratch).unwrap();
        assert!(!result.has_reg());
        assert_eq!(machine.registers.borrow().lock_counts[0][2].get(), 0);
        result.lock();
        assert_eq!(machine.word(result.cur_reg()), 28);
        result.unlock();
        machine.install(1, 0, Reg::new(7), true, 0);
        let mut scratch = ScratchReg::new(machine);
        scratch.alloc_specific(Reg::new(3)).unwrap();
        machine.values.borrow_mut()[3][0] = 39;
        let mut fixed = machine.part(1, 0, false);
        fixed.set_scratch(scratch).unwrap();
        assert_eq!(machine.word(Reg::new(7)), 39);
        machine.values.borrow_mut()[5][0] = 55;
        fixed.set_value_reg(Reg::new(5)).unwrap();
        assert_eq!(machine.word(Reg::new(7)), 55);
        let mut fresh = machine.part(2, 0, false);
        fresh.set_value_reg(Reg::new(5)).unwrap();
        assert_eq!(fresh.cur_reg_unlocked(), Reg::new(5));
        assert!(!fresh.has_reg());
    });
}

#[test]
fn constant_extensions_avoid_registers_and_dynamic_extensions_reuse_last_use() {
    fixture(|machine| {
        for (sign, expected) in [(false, 0x80), (true, 0xffff_ffff_ffff_ff80)] {
            let constant = ValuePartRef::constant_inline(machine, 0x1280, 2, RegBank::new(0));
            let extended = constant.into_extended(sign, 8, 64).unwrap();
            assert_eq!(extended.const_data(), &[expected]);
            assert_eq!(extended.part_size(), 8);
        }
        assert_eq!(machine.used(), 0);
        machine.install(0, 0, Reg::new(10), false, 0x80);
        let extended = machine.part(0, 0, true).into_extended(true, 8, 64).unwrap();
        assert_eq!(extended.cur_reg(), Reg::new(10));
        assert_eq!(machine.word(extended.cur_reg()), 0xffff_ffff_ffff_ff80);
        drop(extended);
        assert_eq!(machine.used(), 0);
        machine.install(1, 0, Reg::new(11), false, 0x180);
        let extended = machine
            .part(1, 0, false)
            .into_extended(false, 8, 64)
            .unwrap();
        assert_ne!(extended.cur_reg(), Reg::new(11));
        assert_eq!(machine.word(extended.cur_reg()), 0x80);
        drop(extended);
        assert_eq!(machine.used(), 1 << 11);
    });
}

#[test]
fn temporary_conversion_rematerializes_constants_and_salvages_assignments() {
    fixture(|machine| {
        let mut constant = ValuePartRef::constant_inline(machine, 51, 8, RegBank::new(0));
        let first = constant.load_to_reg().unwrap();
        let temporary = constant.into_temporary().unwrap();
        assert_ne!(temporary.cur_reg(), first);
        assert_eq!(machine.word(temporary.cur_reg()), 51);
        assert_eq!(machine.used(), 1 << temporary.cur_reg().id());
        drop(temporary);
        machine.install(0, 0, Reg::new(3), false, 89);
        let temporary = machine.part(0, 0, true).into_temporary().unwrap();
        assert_eq!(temporary.cur_reg(), Reg::new(3));
        assert!(!temporary.has_assignment());
        assert_eq!(machine.word(temporary.cur_reg()), 89);
    });
}

#[test]
fn failed_materialization_reload_move_and_copy_leave_reusable_guards() {
    fixture(|machine| {
        let mut constant = ValuePartRef::constant_inline(machine, 9, 8, RegBank::new(0));
        for event in [Event::Select, Event::Constant] {
            machine.fail.set(Some(event));
            assert!(constant.load_to_reg().is_err());
            assert!(!constant.has_reg());
            assert_eq!(machine.used(), 0);
        }
        constant.load_to_reg().unwrap();
        constant.reset();
        let mut part = machine.part(0, 0, false);
        machine.fail.set(Some(Event::Reload));
        assert!(part.load_to_reg().is_err());
        assert!(!part.has_reg());
        assert!(!part.with_assignment(|assignment| assignment.register_valid()));
        assert_eq!(machine.used(), 0);
        part.load_to_reg().unwrap();
        part.unlock();
        machine.fail.set(Some(Event::Move));
        assert!(part.load_to_specific(Reg::new(9)).is_err());
        assert_eq!(part.cur_reg_unlocked(), Reg::new(0));
        assert_eq!(machine.used(), 1);
        assert_eq!(machine.registers.borrow().lock_counts[0][0].get(), 0);
        let mut destination = machine.part(1, 0, false);
        let before = machine.assignments[1].part(0).get();
        machine.fail.set(Some(Event::Move));
        assert!(destination.set_value(machine.part(0, 0, false)).is_err());
        assert!(!destination.has_reg());
        assert_eq!(machine.assignments[1].part(0).get(), before);
        assert_eq!(machine.used(), 1);
        destination.set_value(machine.part(0, 0, false)).unwrap();
        assert_eq!(machine.word(destination.cur_reg_unlocked()), 0x1000);
    });
}

#[test]
fn source_unimplemented_unowned_conversions_and_bad_preconditions_refuse() {
    if !tpde_util::CHECKED {
        return;
    }
    fixture(|machine| {
        let mut temporary = ValuePartRef::temporary(machine, RegBank::new(0));
        temporary.alloc_reg().unwrap();
        assert!(catch_unwind(AssertUnwindSafe(|| temporary
            .get_unowned()
            .into_temporary()
            .unwrap()))
        .is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| temporary
            .get_unowned()
            .into_scratch()
            .unwrap()))
        .is_err());
        assert_eq!(machine.used(), 1);
        assert!(
            catch_unwind(AssertUnwindSafe(|| ValuePartRef::constant_inline(
                machine,
                0,
                9,
                RegBank::new(0)
            )))
            .is_err()
        );
        assert!(
            catch_unwind(AssertUnwindSafe(|| ValuePartRef::constant_borrowed(
                machine,
                &[],
                1,
                RegBank::new(0)
            )))
            .is_err()
        );
        machine.changes.set(false);
        let mut fresh = ValuePartRef::temporary(machine, RegBank::new(0));
        assert!(catch_unwind(AssertUnwindSafe(|| fresh.alloc_reg().unwrap())).is_err());
        assert_eq!(machine.used(), 1);
    });
}

#[test]
fn guard_types_remain_small_and_unwinding_releases_resources() {
    assert!(core::mem::size_of::<ValuePartRef<'_, Machine<'_, '_>, 2>>() <= 64);
    assert!(core::mem::size_of::<ValueRef<'_, Machine<'_, '_>, 2>>() <= 64);
    fixture(|machine| {
        assert!(catch_unwind(AssertUnwindSafe(|| {
            let value = ValueRef::new(machine, ValLocalIdx::new(0));
            let mut part = value.part(0);
            part.load_to_reg().unwrap();
            panic!("injected guard unwind");
        }))
        .is_err());
        assert_eq!(machine.used(), 0);
        assert!(machine.assignments[0].pending_free());
    });
}

#[test]
fn default_sentinel_and_borrowed_register_keep_source_metadata() {
    fixture(|machine| {
        let mut empty = ValuePartRef::empty(machine);
        assert!(!empty.has_assignment());
        assert!(!empty.is_const());
        assert!(!empty.has_reg());
        assert_eq!(empty.bank(), RegBank::default());
        assert_eq!(empty.part_size(), 0);
        let mut moved = empty.take();
        assert_eq!(moved.bank(), RegBank::default());
        assert_eq!(empty.bank(), RegBank::default());
        moved.reset();
        let mut temporary = ValuePartRef::temporary(machine, RegBank::new(1));
        assert_eq!(temporary.part_size(), 0);
        temporary.alloc_reg().unwrap();
        let mut borrowed = temporary.get_unowned_ref();
        assert_eq!(borrowed.bank().id(), 255);
        assert_eq!(borrowed.part_size(), 0);
        if tpde_util::CHECKED {
            assert!(catch_unwind(AssertUnwindSafe(|| borrowed.get_unowned())).is_err());
        }
        let moved = borrowed.take();
        assert_eq!(moved.bank().id(), 255);
        assert_eq!(moved.cur_reg().id(), 32);
        assert_eq!(borrowed.bank().id(), 255);
        drop(moved);
        drop(borrowed);
        assert_eq!(machine.used(), 1 << 32);
    });
}

#[test]
fn frozen_same_register_specific_fast_paths_only_change_lock_count() {
    fixture(|machine| {
        machine.install(0, 0, Reg::new(3), false, 81);
        machine.changes.set(false);
        let mut part = machine.part(0, 0, false);
        part.alloc_specific(Reg::new(3)).unwrap();
        assert_eq!(part.cur_reg(), Reg::new(3));
        part.unlock();
        part.load_to_specific(Reg::new(3)).unwrap();
        assert_eq!(part.cur_reg(), Reg::new(3));
        assert!(machine.events.borrow().is_empty());
        part.unlock();
        assert_eq!(machine.registers.borrow().lock_counts[0][3].get(), 0);
        assert_eq!(machine.used(), 1 << 3);
    });
}

#[test]
fn destination_eviction_can_invalidate_old_source_before_specific_reload() {
    fixture(|machine| {
        machine.install(0, 0, Reg::new(3), false, 0xdead);
        machine.install(1, 0, Reg::new(6), false, 0xbeef);
        machine.evict_extra.set(Some(Reg::new(3)));
        let mut part = machine.part(0, 0, false);
        part.load_to_specific(Reg::new(6)).unwrap();
        assert_eq!(machine.word(part.cur_reg()), 0x1000);
        assert_eq!(
            &*machine.events.borrow(),
            &[Event::Evict, Event::Evict, Event::Reload]
        );
        assert_eq!(machine.used(), 1 << 6);
        part.unlock();
        machine.install(2, 0, Reg::new(9), false, 0);
        machine.evict_extra.set(Some(Reg::new(6)));
        machine.fail.set(Some(Event::Reload));
        assert!(part.load_to_specific(Reg::new(9)).is_err());
        assert!(!part.has_reg());
        assert!(!part.with_assignment(|assignment| assignment.register_valid()));
        assert!(part.with_assignment(|assignment| assignment.stack_valid()));
        assert_eq!(machine.used(), 0);
    });
}

#[test]
fn no_reload_specific_preserves_pinned_old_register_used_bit() {
    fixture(|machine| {
        machine.install(0, 0, Reg::new(3), false, 0x1234);
        let mut part = machine.part(0, 0, false);
        part.alloc_specific(Reg::new(6)).unwrap();
        assert_eq!(part.cur_reg(), Reg::new(6));
        assert_eq!(machine.used(), (1 << 3) | (1 << 6));
        assert_eq!(
            machine.registers.borrow().reg_local_idx(Reg::new(3)),
            ValLocalIdx::new(0)
        );
        assert_eq!(
            machine.registers.borrow().reg_local_idx(Reg::new(6)),
            ValLocalIdx::new(0)
        );
        assert!(machine.events.borrow().is_empty());
        part.unlock();
    });
}

#[test]
fn salvage_overwrites_old_destination_and_nonowned_scratch_conversion_copies() {
    fixture(|machine| {
        machine.install(0, 0, Reg::new(4), false, 0x81);
        machine.install(1, 0, Reg::new(8), false, 0x19);
        let mut source = machine.part(0, 0, true);
        source.lock();
        let mut destination = machine.part(1, 0, false);
        destination.set_value_from(&mut source).unwrap();
        assert!(source.has_assignment());
        assert!(!source.can_salvage());
        assert_eq!(destination.cur_reg_unlocked(), Reg::new(4));
        assert!(!destination.has_reg());
        assert_eq!(machine.used(), 1 << 4);
        let scratch = machine.part(1, 0, false).into_scratch().unwrap();
        assert_ne!(scratch.cur_reg(), Reg::new(4));
        assert_eq!(machine.word(scratch.cur_reg()), 0x81);
        assert_eq!(machine.word(Reg::new(4)), 0x81);
    });
}

#[test]
fn temporary_raw_claim_copy_width_and_borrowed_constant_conversion() {
    fixture(|machine| {
        machine.values.borrow_mut()[7][0] = 0xab;
        let mut temporary = ValuePartRef::temporary(machine, RegBank::new(0));
        temporary.set_value_reg(Reg::new(7)).unwrap();
        assert_eq!(temporary.cur_reg(), Reg::new(7));
        let mut scratch = ScratchReg::new(machine);
        let copied = scratch.alloc_gp().unwrap();
        temporary.reload_into_specific_fixed(copied, 8).unwrap();
        assert_eq!(machine.word(copied), 0xab);
        let words = [11, 19];
        let constant = ValuePartRef::constant_borrowed(machine, &words, 16, RegBank::new(1));
        let converted = constant.into_temporary().unwrap();
        assert_eq!(converted.part_size(), 16);
        assert_eq!(
            &machine.values.borrow()[converted.cur_reg().id() as usize][..2],
            &words
        );
    });
}
