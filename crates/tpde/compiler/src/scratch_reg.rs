// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
use tpde_core::{Reg, RegBank, RegisterFile};
use tpde_util::{tpde_assert, tpde_assert_eq};
use types_error::PgResult;

use crate::guard_machine::{GuardMachine, INVALID_VALUE};

pub struct ScratchReg<'c, M: GuardMachine<N>, const N: usize> {
    machine: &'c M,
    reg: Reg,
}

impl<'c, M: GuardMachine<N>, const N: usize> ScratchReg<'c, M, N> {
    pub const fn new(machine: &'c M) -> Self {
        Self {
            machine,
            reg: Reg::make_invalid(),
        }
    }

    pub fn has_reg(&self) -> bool {
        self.reg.valid()
    }

    pub fn cur_reg(&self) -> Reg {
        tpde_assert!(self.has_reg());
        self.reg
    }

    pub fn alloc_specific(&mut self, reg: Reg) -> PgResult<Reg> {
        tpde_assert!(self.machine.may_change_value_state());
        self.machine.with_registers(|registers| {
            tpde_assert!(!registers.is_fixed(reg));
        });
        self.reset();
        if self
            .machine
            .with_registers(|registers| registers.is_used(reg))
        {
            self.machine.evict_reg(reg)?;
        }
        self.claim(reg);
        Ok(reg)
    }

    pub fn alloc_gp(&mut self) -> PgResult<Reg> {
        self.alloc(M::GP_BANK)
    }

    pub fn alloc(&mut self, bank: RegBank) -> PgResult<Reg> {
        tpde_assert!(self.machine.may_change_value_state());
        if self.has_reg() {
            tpde_assert_eq!(bank, RegisterFile::<N, 32>::reg_bank(self.reg));
            return Ok(self.reg);
        }
        let reg = self.machine.select_reg(bank)?;
        self.claim(reg);
        Ok(reg)
    }

    fn claim(&mut self, reg: Reg) {
        self.machine.with_registers(|registers| {
            registers.mark_used(reg, INVALID_VALUE, 0);
            registers.mark_clobbered(reg);
            registers.mark_fixed(reg);
        });
        self.reg = reg;
    }

    pub fn release(&mut self) -> Reg {
        let reg = self.reg;
        self.reset();
        reg
    }

    pub fn reset(&mut self) {
        if self.reg.invalid() {
            return;
        }
        self.machine.with_registers(|registers| {
            registers.unmark_fixed(self.reg);
            registers.unmark_used(self.reg);
        });
        self.reg = Reg::make_invalid();
    }

    pub fn take(&mut self) -> Self {
        core::mem::replace(self, Self::new(self.machine))
    }

    pub(crate) fn force_set_reg(&mut self, reg: Reg) {
        self.reg = reg;
    }

    pub(crate) fn machine(&self) -> &'c M {
        self.machine
    }
}

impl<M: GuardMachine<N>, const N: usize> Drop for ScratchReg<'_, M, N> {
    fn drop(&mut self) {
        self.reset();
    }
}
