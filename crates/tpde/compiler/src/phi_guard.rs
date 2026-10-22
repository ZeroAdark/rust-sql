// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{branches::BranchMachine, guard_machine::INVALID_VALUE};
use alloc::boxed::Box;
use tpde_base::ValLocalIdx;
use tpde_core::{ir_adaptor::IrAdaptor, Reg, RegBank, RegisterFile};
use tpde_util::tpde_assert;
use types_error::{PgError, PgResult, ERRCODE_PROGRAM_LIMIT_EXCEEDED};

pub(crate) struct BranchRegionGuard<'c, M: BranchMachine<N>, const N: usize>(&'c M);
impl<'c, M: BranchMachine<N>, const N: usize> BranchRegionGuard<'c, M, N> {
    pub(crate) fn new(machine: &'c M) -> Self {
        tpde_assert!(machine.may_change_value_state());
        machine.set_branch_region(true);
        Self(machine)
    }
}
impl<M: BranchMachine<N>, const N: usize> Drop for BranchRegionGuard<'_, M, N> {
    fn drop(&mut self) {
        self.0.set_branch_region(false);
    }
}

pub(crate) struct PhiScratchReg<'c, M: BranchMachine<N>, const N: usize> {
    machine: &'c M,
    reg: Reg,
    backed_up: bool,
    modified: bool,
    part: u32,
    local: ValLocalIdx,
}
impl<'c, M: BranchMachine<N>, const N: usize> PhiScratchReg<'c, M, N> {
    pub(crate) fn new(machine: &'c M) -> Self {
        Self {
            machine,
            reg: Reg::make_invalid(),
            backed_up: false,
            modified: false,
            part: 0,
            local: INVALID_VALUE,
        }
    }
    pub(crate) fn cur_reg(&self) -> Reg {
        tpde_assert!(self.reg.valid());
        self.reg
    }
    pub(crate) fn reset(&mut self) -> PgResult<()> {
        if self.reg.invalid() {
            return Ok(());
        }
        let reg = self.reg;
        self.machine.with_registers(|registers| {
            registers.unmark_fixed(reg);
            registers.unmark_used(reg);
        });
        self.reg = Reg::make_invalid();
        let mut restored = Ok(());
        if self.backed_up && self.machine.assignment_exists(self.local) {
            let variable = self
                .machine
                .with_part(self.local, self.part, |ap| ap.variable_ref());
            if !variable {
                // `reload_to_reg`: a constant is rematerialized (it has no
                // slot, `spill_part`), anything else loads from its slot.
                restored = self.machine.reload_to_reg(reg, self.local, self.part);
            }
            self.machine.with_part(self.local, self.part, |ap| {
                ap.set_reg(reg);
                ap.set_register_valid(true);
                ap.set_modified(self.modified);
            });
            self.machine
                .with_registers(|registers| registers.mark_used(reg, self.local, self.part));
        }
        self.backed_up = false;
        restored
    }
    pub(crate) fn alloc_from_bank(&mut self, bank: RegBank) -> PgResult<Reg> {
        if self.reg.valid() && RegisterFile::<N, 32>::reg_bank(self.reg) == bank {
            return Ok(self.reg);
        }
        self.reset()?;
        let mut reg = self
            .machine
            .with_registers(|registers| registers.find_first_free_excluding(bank, 0));
        if reg.invalid() {
            reg = self
                .machine
                .with_registers(|registers| registers.find_first_nonfixed_excluding(bank, 0));
            if reg.invalid() {
                return Err(scratch_exhausted());
            }
            let (local, part) = self.machine.with_registers(|registers| {
                (registers.reg_local_idx(reg), registers.reg_part(reg))
            });
            let (modified, valid, current, stack, variable) =
                self.machine.with_part(local, part, |ap| {
                    (
                        ap.modified(),
                        ap.register_valid(),
                        ap.get_reg(),
                        ap.stack_valid(),
                        ap.variable_ref(),
                    )
                });
            tpde_assert!(valid && current == reg);
            // A constant is never stored: `reset` rematerializes it.
            let constant = self.machine.adaptor().local_const(local).is_some();
            if !stack && !variable && !constant {
                self.machine.allocate_spill_part(local, part)?;
                let (frame, size) = self
                    .machine
                    .with_part(local, part, |ap| (ap.frame_off(), ap.part_size()));
                self.machine.spill_reg(reg, frame, size)?;
                self.machine
                    .with_part(local, part, |ap| ap.set_stack_valid());
            }
            self.machine
                .with_part(local, part, |ap| ap.set_register_valid(false));
            self.machine
                .with_registers(|registers| registers.unmark_used(reg));
            self.backed_up = true;
            self.local = local;
            self.part = part;
            self.modified = modified;
        }
        self.machine.with_registers(|registers| {
            registers.mark_used(reg, INVALID_VALUE, 0);
            registers.mark_clobbered(reg);
            registers.mark_fixed(reg);
        });
        self.reg = reg;
        Ok(reg)
    }
}
impl<M: BranchMachine<N>, const N: usize> Drop for PhiScratchReg<'_, M, N> {
    fn drop(&mut self) {
        if let Err(error) = self.reset() {
            self.machine.record_failure(error);
        }
    }
}

pub(crate) struct TempStackGuard<'c, M: BranchMachine<N>, const N: usize> {
    machine: &'c M,
    slot: Option<(i32, u32)>,
}
impl<'c, M: BranchMachine<N>, const N: usize> TempStackGuard<'c, M, N> {
    pub(crate) fn new(machine: &'c M) -> Self {
        Self {
            machine,
            slot: None,
        }
    }
    pub(crate) fn allocate(&mut self, size: u32) -> PgResult<()> {
        tpde_assert!(self.slot.is_none());
        self.slot = Some((self.machine.allocate_stack_slot(size)?, size));
        Ok(())
    }
    pub(crate) fn offset(&self) -> i32 {
        self.slot.expect("PHI temporary stack slot is absent").0
    }
    pub(crate) fn reset(&mut self) {
        if let Some((offset, size)) = self.slot.take() {
            self.machine.free_stack_slot(offset, size);
        }
    }
}
impl<M: BranchMachine<N>, const N: usize> Drop for TempStackGuard<'_, M, N> {
    fn drop(&mut self) {
        self.reset();
    }
}

#[cold]
fn scratch_exhausted() -> Box<PgError> {
    PgError::error("ran out of registers for PHI scratch registers")
        .with_sqlstate(ERRCODE_PROGRAM_LIMIT_EXCEEDED)
        .into()
}
