// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![doc = include_str!("../tests/fixed-register-ownership.md")]

use tpde_core::{register_file::Assignment, Reg, RegisterFile};
use tpde_util::{tpde_assert, tpde_assert_eq};
use types_error::PgResult;

use crate::{
    guard_machine::{GuardMachine, INVALID_VALUE},
    ScratchReg,
};

#[must_use]
pub struct FixedRegisterBackup<'c, M: GuardMachine<N>, const N: usize> {
    machine: &'c M,
    reg: Reg,
    assignment: Assignment,
    lock_count: u8,
    scratch: ScratchReg<'c, M, N>,
    active: bool,
}

impl<'c, M: GuardMachine<N>, const N: usize> FixedRegisterBackup<'c, M, N> {
    pub fn begin(machine: &'c M, reg: Reg) -> PgResult<Self> {
        tpde_assert!(machine.may_change_value_state());
        tpde_assert!((reg.id() as usize) < RegisterFile::<N, 32>::NUM_REGS);
        tpde_assert_eq!(RegisterFile::<N, 32>::reg_bank(reg), M::GP_BANK);
        machine.with_registers(|registers| {
            tpde_assert!(registers.is_used(reg));
            tpde_assert!(registers.is_fixed(reg));
        });
        let mut scratch = ScratchReg::new(machine);
        scratch.alloc(M::GP_BANK)?;
        let bank = reg.id() as usize / 32;
        let slot = reg.id() as usize % 32;
        let (assignment, lock_count) = machine.with_registers(|registers| {
            let saved = (
                registers.assignments[bank][slot].get(),
                registers.lock_counts[bank][slot].get(),
            );
            registers.assignments[bank][slot].set(Assignment {
                local_idx: INVALID_VALUE,
                part: 0,
            });
            registers.lock_counts[bank][slot].set(0);
            saved
        });
        Ok(Self {
            machine,
            reg,
            assignment,
            lock_count,
            scratch,
            active: true,
        })
    }

    pub fn fixed_reg(&self) -> Reg {
        self.reg
    }

    pub fn backup_reg(&self) -> Reg {
        self.scratch.cur_reg()
    }

    pub fn restore_with(
        mut self,
        is_result: bool,
        emit: impl FnOnce(Reg, Reg) -> PgResult<()>,
    ) -> PgResult<Option<ScratchReg<'c, M, N>>> {
        self.restore_metadata();
        emit(self.reg, self.scratch.cur_reg())?;
        if is_result {
            Ok(Some(self.scratch.take()))
        } else {
            self.scratch.reset();
            Ok(None)
        }
    }

    fn restore_metadata(&mut self) {
        if !self.active {
            return;
        }
        let bank = self.reg.id() as usize / 32;
        let slot = self.reg.id() as usize % 32;
        self.machine.with_registers(|registers| {
            registers.assignments[bank][slot].set(self.assignment);
            registers.lock_counts[bank][slot].set(self.lock_count);
        });
        self.active = false;
    }
}

impl<M: GuardMachine<N>, const N: usize> Drop for FixedRegisterBackup<'_, M, N> {
    fn drop(&mut self) {
        self.restore_metadata();
    }
}
