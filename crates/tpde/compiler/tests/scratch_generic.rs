// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use std::{
    cell::{Cell, RefCell},
    panic::{catch_unwind, AssertUnwindSafe},
};
use tpde_base::ValLocalIdx;
use tpde_compiler::{
    generic_value_part::{
        gval_as_reg, gval_as_reg_reuse_scratch, gval_as_reg_reuse_value, Expr, ExprReg,
        GenericValuePart,
    },
    guard_machine::{GuardMachine, INVALID_VALUE},
    scratch_reg::ScratchReg,
    value_part_ref::ValuePartRef,
};
use tpde_core::{
    compiler::ValRefSpecial, AssignmentPartRef, BlockIndex, LivenessInfo, Reg, RegBank,
    RegisterFile,
};
use types_error::{PgError, PgResult};

struct Machine {
    registers: RefCell<RegisterFile<2, 32>>,
    changes_allowed: Cell<bool>,
    fail_select: Cell<bool>,
    fail_evict: Cell<bool>,
    fail_expr: Cell<bool>,
    selections: Cell<u32>,
    evictions: Cell<u32>,
    expressions: Cell<u32>,
    constants: Cell<u32>,
}

impl Machine {
    fn new() -> Self {
        let mut registers = RegisterFile::new();
        registers.allocatable.set(u64::MAX);
        Self {
            registers: RefCell::new(registers),
            changes_allowed: Cell::new(true),
            fail_select: Cell::new(false),
            fail_evict: Cell::new(false),
            fail_expr: Cell::new(false),
            selections: Cell::new(0),
            evictions: Cell::new(0),
            expressions: Cell::new(0),
            constants: Cell::new(0),
        }
    }

    fn used(&self) -> u64 {
        self.registers.borrow().used.get()
    }
}

impl GuardMachine<2> for Machine {
    type Special = ValRefSpecial<()>;
    const GP_BANK: RegBank = RegBank::new(0);

    fn with_registers<R>(&self, operation: impl FnOnce(&RegisterFile<2, 32>) -> R) -> R {
        operation(&*self.registers.borrow_mut())
    }

    fn with_assignment<R>(
        &self,
        _: ValLocalIdx,
        _: impl FnOnce(&tpde_core::value_assignment::Assignment<'_, '_>) -> R,
    ) -> R {
        panic!("fixture contains no assignments")
    }

    fn with_part<R>(
        &self,
        _: ValLocalIdx,
        _: u32,
        _: impl FnOnce(AssignmentPartRef<'_, '_, '_>) -> R,
    ) -> R {
        panic!("fixture contains no assignments")
    }

    fn liveness(&self, _: ValLocalIdx) -> LivenessInfo {
        panic!("fixture contains no assignments")
    }

    fn current_block(&self) -> BlockIndex {
        BlockIndex(0)
    }

    fn may_change_value_state(&self) -> bool {
        self.changes_allowed.get()
    }

    fn release_assignment(&self, _: ValLocalIdx) {
        panic!("fixture contains no assignments")
    }

    fn decrement_fixed_count(&self, _: RegBank) {
        panic!("fixture contains no fixed assignments")
    }

    fn select_reg(&self, bank: RegBank) -> PgResult<Reg> {
        self.selections.set(self.selections.get() + 1);
        if self.fail_select.get() {
            return Err(PgError::error("injected selection failure").into());
        }
        let reg = self.registers.borrow().find_first_free_excluding(bank, 0);
        assert!(reg.valid());
        Ok(reg)
    }

    fn evict_reg(&self, reg: Reg) -> PgResult<()> {
        self.evictions.set(self.evictions.get() + 1);
        if self.fail_evict.get() {
            return Err(PgError::error("injected eviction failure").into());
        }
        self.registers.borrow_mut().unmark_used(reg);
        Ok(())
    }

    fn reload_to_reg(&self, _: Reg, _: ValLocalIdx, _: u32) -> PgResult<()> {
        panic!("fixture contains no assignments")
    }

    fn mov(&self, _: Reg, _: Reg, _: u32) -> PgResult<()> {
        panic!("unexpected copy instead of ownership transfer")
    }

    fn materialize_constant(&self, _: &[u64], _: RegBank, _: u32, _: Reg) -> PgResult<()> {
        self.constants.set(self.constants.get() + 1);
        Ok(())
    }

    fn generate_raw_intext(&self, _: Reg, _: Reg, _: bool, _: u32, _: u32) -> PgResult<()> {
        panic!("unexpected integer extension")
    }

    fn gval_expr_as_reg<'c>(&'c self, value: &mut GenericValuePart<'c, Self, 2>) -> PgResult<Reg> {
        assert!(value.is_expr());
        self.expressions.set(self.expressions.get() + 1);
        let mut scratch = ScratchReg::new(self);
        let reg = scratch.alloc_gp()?;
        if self.fail_expr.get() {
            return Err(PgError::error("injected expression failure").into());
        }
        *value = scratch.into();
        Ok(reg)
    }

    fn special_part<'s>(&'s self, _: &'s Self::Special, _: u32) -> ValuePartRef<'s, Self, 2> {
        panic!("fixture contains no special values")
    }
}

#[test]
fn scratch_claim_release_and_same_bank_fast_path() {
    let machine = Machine::new();
    let mut scratch = ScratchReg::new(&machine);
    assert!(!scratch.has_reg());
    assert!(scratch.release().invalid());
    for bank in [RegBank::new(0), RegBank::new(1)] {
        let reg = scratch.alloc(bank).unwrap();
        assert_eq!(reg.id(), bank.id() * 32);
        assert_eq!(scratch.alloc(bank).unwrap(), reg);
        machine.with_registers(|registers| {
            assert!(registers.is_used(reg));
            assert!(registers.is_fixed(reg));
            assert!(registers.is_clobbered(reg));
            assert_eq!(registers.reg_local_idx(reg), INVALID_VALUE);
            assert_eq!(registers.reg_part(reg), 0);
        });
        assert_eq!(scratch.release(), reg);
        assert_eq!(machine.used(), 0);
        assert!(machine.registers.borrow().is_clobbered(reg));
    }
    assert_eq!(machine.selections.get(), 2);
}

#[test]
fn specific_allocation_releases_previous_and_evicts_target() {
    let machine = Machine::new();
    let mut scratch = ScratchReg::new(&machine);
    scratch.alloc_specific(Reg::new(3)).unwrap();
    machine.with_registers(|registers| registers.mark_used(Reg::new(7), ValLocalIdx::new(9), 2));
    assert_eq!(scratch.alloc_specific(Reg::new(7)).unwrap(), Reg::new(7));
    assert_eq!(machine.used(), 1 << 7);
    assert_eq!(machine.evictions.get(), 1);
    scratch.reset();
    assert_eq!(machine.used(), 0);
}

#[test]
fn scratch_moves_and_unwinding_release_exactly_once() {
    let machine = Machine::new();
    let mut first = ScratchReg::new(&machine);
    first.alloc_specific(Reg::new(4)).unwrap();
    let mut moved = first.take();
    assert!(!first.has_reg());
    first.alloc_specific(Reg::new(9)).unwrap();
    first = moved.take();
    assert_eq!(machine.used(), 1 << 4);
    assert!(!moved.has_reg());
    drop(first);
    assert_eq!(machine.used(), 0);
    assert!(catch_unwind(AssertUnwindSafe(|| {
        let mut scratch = ScratchReg::new(&machine);
        scratch.alloc_gp().unwrap();
        panic!("injected unwind");
    }))
    .is_err());
    assert_eq!(machine.used(), 0);
}

#[test]
fn selection_and_eviction_errors_leave_reusable_empty_guard() {
    let machine = Machine::new();
    let mut scratch = ScratchReg::new(&machine);
    machine.fail_select.set(true);
    assert!(scratch.alloc_gp().is_err());
    assert!(!scratch.has_reg());
    assert_eq!(machine.used(), 0);
    machine.fail_select.set(false);
    scratch.alloc_specific(Reg::new(2)).unwrap();
    machine.with_registers(|registers| registers.mark_used(Reg::new(6), ValLocalIdx::new(1), 0));
    machine.fail_evict.set(true);
    assert!(scratch.alloc_specific(Reg::new(6)).is_err());
    assert!(!scratch.has_reg());
    assert_eq!(machine.used(), 1 << 6);
    machine.fail_evict.set(false);
    scratch.alloc_specific(Reg::new(6)).unwrap();
    scratch.reset();
    assert_eq!(machine.used(), 0);
}

#[test]
fn fixed_target_is_rejected_before_current_register_is_released() {
    let machine = Machine::new();
    let mut first = ScratchReg::new(&machine);
    let mut second = ScratchReg::new(&machine);
    first.alloc_specific(Reg::new(1)).unwrap();
    second.alloc_specific(Reg::new(2)).unwrap();
    for reg in [Reg::new(1), Reg::new(2)] {
        if !tpde_util::CHECKED {
            break;
        }
        assert!(catch_unwind(AssertUnwindSafe(|| {
            first.alloc_specific(reg).unwrap();
        }))
        .is_err());
        assert_eq!(first.cur_reg(), Reg::new(1));
        assert_eq!(machine.used(), (1 << 1) | (1 << 2));
    }
}

#[test]
fn occupied_bank_and_branch_region_preconditions_are_preserved() {
    if !tpde_util::CHECKED {
        return;
    }
    let machine = Machine::new();
    let mut scratch = ScratchReg::new(&machine);
    scratch.alloc_gp().unwrap();
    assert!(catch_unwind(AssertUnwindSafe(|| {
        scratch.alloc(RegBank::new(1)).unwrap();
    }))
    .is_err());
    machine.changes_allowed.set(false);
    assert!(catch_unwind(AssertUnwindSafe(|| {
        scratch.alloc_gp().unwrap();
    }))
    .is_err());
    assert_eq!(scratch.cur_reg(), Reg::new(0));
    scratch.reset();
    assert_eq!(machine.used(), 0);
}

#[test]
fn expression_base_index_and_scale_invariants() {
    if !tpde_util::CHECKED {
        return;
    }
    let mut expr = Expr::<Machine, 2>::new();
    assert!(!expr.has_base());
    assert!(!expr.has_index());
    assert!(expr.base_reg().invalid());
    assert!(catch_unwind(AssertUnwindSafe(|| expr.index_reg())).is_err());
    expr.index = ExprReg::Borrowed(Reg::new(7));
    expr.scale = 1;
    assert!(expr.has_index());
    assert!(catch_unwind(AssertUnwindSafe(|| expr.index_reg())).is_err());
    expr.scale = 2;
    assert_eq!(expr.index_reg(), Reg::new(7));
    expr.base = ExprReg::Borrowed(Reg::new(3));
    expr.scale = 1;
    assert_eq!(expr.index_reg(), Reg::new(7));
    assert_eq!(expr.base_reg(), Reg::new(3));
    let machine = Machine::new();
    let invalid_scratch = ScratchReg::new(&machine);
    let expr = Expr::from_scratch(invalid_scratch, 0);
    assert!(expr.has_base());
    assert!(catch_unwind(AssertUnwindSafe(|| expr.base_reg())).is_err());
}

#[test]
fn generic_move_and_reset_drop_both_expression_resources() {
    let machine = Machine::new();
    let mut base = ScratchReg::new(&machine);
    let mut index = ScratchReg::new(&machine);
    base.alloc_specific(Reg::new(5)).unwrap();
    index.alloc_specific(Reg::new(8)).unwrap();
    let mut expr = Expr::from_scratch(base, -19);
    expr.index = ExprReg::Scratch(index);
    expr.scale = 4;
    assert_eq!(expr.index_reg(), Reg::new(8));
    let mut generic = GenericValuePart::from(expr);
    let mut moved = generic.take();
    assert!(matches!(generic, GenericValuePart::Empty));
    assert_eq!(machine.used(), (1 << 5) | (1 << 8));
    moved.reset();
    assert_eq!(machine.used(), 0);
    assert!(!moved.is_expr());
    assert!(!moved.is_imm());
}

#[test]
fn immediate_access_preserves_full_word_and_width_precondition() {
    let machine = Machine::new();
    let mut value = GenericValuePart::from(ValuePartRef::constant_inline(
        &machine,
        u64::MAX,
        1,
        RegBank::new(0),
    ));
    assert!(value.is_imm());
    assert_eq!(value.imm_size(), 1);
    assert_eq!(value.imm64(), u64::MAX);
    value.val_ref_mut().reset();
    assert!(value.is_imm());
    assert_eq!(value.imm64(), u64::MAX);
    let words = [0x1234, 0x5678];
    let value = GenericValuePart::from(ValuePartRef::constant_borrowed(
        &machine,
        &words,
        16,
        RegBank::new(1),
    ));
    assert_eq!(value.imm_size(), 16);
    if tpde_util::CHECKED {
        assert!(catch_unwind(AssertUnwindSafe(|| value.imm64())).is_err());
    }
}

#[test]
fn trivial_expression_fast_path_keeps_owned_base_in_expression() {
    let machine = Machine::new();
    let mut base = ScratchReg::new(&machine);
    base.alloc_specific(Reg::new(3)).unwrap();
    let mut value = GenericValuePart::from(Expr::from_scratch(base, 0));
    let mut destination = ScratchReg::new(&machine);
    assert_eq!(
        gval_as_reg_reuse_scratch(&machine, &mut value, &mut destination).unwrap(),
        Reg::new(3)
    );
    assert!(!destination.has_reg());
    assert!(value.is_expr());
    assert_eq!(machine.expressions.get(), 0);
    value.reset();
    assert_eq!(machine.used(), 0);
    let mut borrowed = GenericValuePart::from(Expr::from_reg(Reg::new(17), 0));
    assert_eq!(gval_as_reg(&machine, &mut borrowed).unwrap(), Reg::new(17));
    assert_eq!(machine.selections.get(), 0);
}

#[test]
fn materialized_expression_reuse_observes_replaced_variant() {
    let machine = Machine::new();
    let mut value = GenericValuePart::from(Expr::from_reg(Reg::new(12), 24));
    let mut destination = ScratchReg::new(&machine);
    let reg = gval_as_reg_reuse_scratch(&machine, &mut value, &mut destination).unwrap();
    assert_eq!(destination.cur_reg(), reg);
    assert!(matches!(&value, GenericValuePart::Scratch(scratch) if !scratch.has_reg()));
    assert_eq!(machine.expressions.get(), 1);
    let mut other = GenericValuePart::from(Expr::from_reg(Reg::new(14), 32));
    let other_reg = gval_as_reg_reuse_scratch(&machine, &mut other, &mut destination).unwrap();
    assert_ne!(other_reg, reg);
    assert_eq!(destination.cur_reg(), reg);
    assert!(matches!(&other, GenericValuePart::Scratch(scratch) if scratch.has_reg()));
    destination.reset();
    other.reset();
    assert_eq!(machine.used(), 0);
}

#[test]
fn expression_failure_releases_attempt_and_allows_retry() {
    let machine = Machine::new();
    let mut value = GenericValuePart::from(Expr::from_reg(Reg::new(3), 1));
    machine.fail_expr.set(true);
    assert!(gval_as_reg(&machine, &mut value).is_err());
    assert!(value.is_expr());
    assert_eq!(machine.used(), 0);
    machine.fail_expr.set(false);
    assert_eq!(gval_as_reg(&machine, &mut value).unwrap(), Reg::new(0));
    value.reset();
    assert_eq!(machine.used(), 0);
}

#[test]
fn scratch_and_value_destinations_reuse_materialized_constants() {
    let machine = Machine::new();
    let mut value = GenericValuePart::from(ValuePartRef::constant_inline(
        &machine,
        91,
        8,
        RegBank::new(0),
    ));
    let mut scratch = ScratchReg::new(&machine);
    let reg = gval_as_reg_reuse_scratch(&machine, &mut value, &mut scratch).unwrap();
    assert_eq!(scratch.cur_reg(), reg);
    assert!(!value.val_ref().has_reg());
    assert_eq!(machine.constants.get(), 1);
    scratch.reset();
    let mut destination = ValuePartRef::temporary(&machine, RegBank::new(0));
    let reg = gval_as_reg_reuse_value(&machine, &mut value, &mut destination).unwrap();
    assert_eq!(destination.cur_reg(), reg);
    assert!(!value.val_ref().has_reg());
    assert_eq!(machine.constants.get(), 2);
    destination.reset();
    assert_eq!(machine.used(), 0);
}

#[test]
fn scratch_transfer_into_temporary_value_keeps_register_fixed() {
    let machine = Machine::new();
    let mut scratch = ScratchReg::new(&machine);
    scratch.alloc_specific(Reg::new(6)).unwrap();
    let mut value = GenericValuePart::from(scratch);
    let mut destination = ValuePartRef::temporary(&machine, RegBank::new(0));
    assert_eq!(
        gval_as_reg_reuse_value(&machine, &mut value, &mut destination).unwrap(),
        Reg::new(6)
    );
    assert_eq!(destination.cur_reg(), Reg::new(6));
    assert!(matches!(&value, GenericValuePart::Scratch(scratch) if !scratch.has_reg()));
    assert!(machine.registers.borrow().is_fixed(Reg::new(6)));
    destination.reset();
    assert_eq!(machine.used(), 0);
}

#[test]
fn empty_generic_rejects_register_materialization() {
    let machine = Machine::new();
    let mut value = GenericValuePart::<Machine, 2>::new();
    assert!(catch_unwind(AssertUnwindSafe(|| {
        gval_as_reg(&machine, &mut value).unwrap();
    }))
    .is_err());
    if tpde_util::CHECKED {
        assert!(catch_unwind(AssertUnwindSafe(|| value.imm_size())).is_err());
    }
    assert_eq!(machine.used(), 0);
}
