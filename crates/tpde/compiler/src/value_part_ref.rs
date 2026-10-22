// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{
    guard_machine::{GuardMachine, RegallocEvent, INVALID_VALUE},
    scratch_reg::ScratchReg,
};
use core::ptr::NonNull;
use tpde_base::ValLocalIdx;
use tpde_core::{value_assignment::ValueAssignment, AssignmentPartRef, Reg, RegBank};
use tpde_util::{tpde_assert, tpde_assert_eq, tpde_assert_ne};
use types_error::PgResult;

#[derive(Clone, Copy)]
enum Data<'c> {
    Temporary {
        bank: RegBank,
        owned: bool,
    },
    Inline {
        word: u64,
        size: u32,
        bank: RegBank,
        owned: bool,
    },
    Borrowed {
        words: &'c [u64],
        size: u32,
        bank: RegBank,
        owned: bool,
    },
    Assignment {
        local: ValLocalIdx,
        part: u32,
        owned: bool,
        assignment: NonNull<ValueAssignment>,
    },
}

pub struct ValuePartRef<'c, M: GuardMachine<N>, const N: usize> {
    machine: &'c M,
    reg: Reg,
    data: Data<'c>,
}

pub type ValuePart<'c, M, const N: usize> = ValuePartRef<'c, M, N>;

impl<'c, M: GuardMachine<N>, const N: usize> ValuePartRef<'c, M, N> {
    pub fn empty(machine: &'c M) -> Self {
        Self {
            machine,
            reg: Reg::make_invalid(),
            data: Data::Temporary {
                bank: RegBank::default(),
                owned: false,
            },
        }
    }

    pub fn temporary(machine: &'c M, bank: RegBank) -> Self {
        tpde_assert!((bank.id() as usize) < N);
        Self {
            machine,
            reg: Reg::make_invalid(),
            data: Data::Temporary { bank, owned: false },
        }
    }

    pub fn constant_inline(machine: &'c M, word: u64, size: u32, bank: RegBank) -> Self {
        tpde_assert!(size <= 8);
        tpde_assert!((bank.id() as usize) < N);
        Self {
            machine,
            reg: Reg::make_invalid(),
            data: Data::Inline {
                word,
                size,
                bank,
                owned: false,
            },
        }
    }

    pub fn constant_borrowed(machine: &'c M, words: &'c [u64], size: u32, bank: RegBank) -> Self {
        tpde_assert!((bank.id() as usize) < N);
        tpde_assert!(words.len() >= (u64::from(size) + 7) as usize / 8);
        Self {
            machine,
            reg: Reg::make_invalid(),
            data: Data::Borrowed {
                words,
                size,
                bank,
                owned: false,
            },
        }
    }

    /// `assignment` is the owning `ValueRef`'s pinned pointer (see `ValueRef::pinned`);
    /// this part borrows that reference, so the same argument keeps it live here.
    pub(crate) fn from_assignment(
        machine: &'c M,
        local: ValLocalIdx,
        assignment: NonNull<ValueAssignment>,
        part: u32,
        owned: bool,
    ) -> Self {
        {
            // SAFETY: pinned by the owning ValueRef for this borrow.
            let header = unsafe { assignment.as_ref() };
            tpde_assert!(!header.pending_free());
            tpde_assert!(part < header.part_count());
            tpde_assert!(header.variable_ref() || header.references_left() != 0);
            tpde_assert!(!owned || header.references_left() == 1);
        }
        Self {
            machine,
            reg: Reg::make_invalid(),
            data: Data::Assignment {
                local,
                part,
                owned,
                assignment,
            },
        }
    }

    /// An unowned reference to `part` of an existing assignment (upstream
    /// `ValuePart{local_idx, assignment, part, owned=false}`), for frontends
    /// that materialize variable references outside `ValueRef`.
    pub fn assignment_unowned(machine: &'c M, local: ValLocalIdx, part: u32) -> Self {
        // The slot pointer is what a `ValueRef` over `local` would pin; variable
        // references live until the function ends, which this borrow of the
        // machine cannot outlive.
        let assignment = machine.with_assignment(local, |slot| slot.pointer());
        Self::from_assignment(machine, local, assignment, part, false)
    }

    pub(crate) fn machine(&self) -> &'c M {
        self.machine
    }
    pub fn has_assignment(&self) -> bool {
        matches!(self.data, Data::Assignment { .. })
    }
    pub fn is_const(&self) -> bool {
        matches!(self.data, Data::Inline { .. } | Data::Borrowed { .. })
    }
    pub fn is_owned(&self) -> bool {
        tpde_assert!(self.has_assignment());
        self.owned()
    }
    fn owned(&self) -> bool {
        match self.data {
            Data::Temporary { owned, .. }
            | Data::Inline { owned, .. }
            | Data::Borrowed { owned, .. }
            | Data::Assignment { owned, .. } => owned,
        }
    }
    fn set_owned(&mut self, value: bool) {
        match &mut self.data {
            Data::Temporary { owned, .. }
            | Data::Inline { owned, .. }
            | Data::Borrowed { owned, .. }
            | Data::Assignment { owned, .. } => *owned = value,
        }
    }
    pub fn with_assignment<R>(
        &self,
        operation: impl FnOnce(AssignmentPartRef<'_, '_, '_>) -> R,
    ) -> R {
        let Data::Assignment {
            local,
            part,
            assignment,
            ..
        } = self.data
        else {
            panic!("part has no assignment")
        };
        if tpde_util::CHECKED {
            self.machine.with_assignment(local, |slot| {
                tpde_assert!(
                    slot.pointer() == assignment,
                    "value part outlived its assignment"
                )
            });
        }
        // SAFETY: pinned by the owning ValueRef for this borrow (from_assignment).
        operation(unsafe { AssignmentPartRef::from_raw(assignment, part) })
    }
    pub fn cur_reg(&self) -> Reg {
        tpde_assert!(self.reg.valid());
        self.reg
    }
    pub fn has_reg(&self) -> bool {
        self.reg.valid()
    }
    pub fn cur_reg_unlocked(&self) -> Reg {
        if self.has_reg() {
            return self.reg;
        }
        if self.has_assignment() {
            return self.with_assignment(|part| {
                if part.register_valid() {
                    part.get_reg()
                } else {
                    Reg::make_invalid()
                }
            });
        }
        Reg::make_invalid()
    }
    pub fn is_in_reg(&self, reg: Reg) -> bool {
        let current = self.cur_reg_unlocked();
        current.valid() && current == reg
    }
    pub fn local_idx(&self) -> ValLocalIdx {
        match self.data {
            Data::Assignment { local, .. } => local,
            _ => panic!("part has no assignment"),
        }
    }
    pub fn part(&self) -> u32 {
        match self.data {
            Data::Assignment { part, .. } => part,
            _ => panic!("part has no assignment"),
        }
    }
    pub fn bank(&self) -> RegBank {
        match self.data {
            Data::Temporary { bank, .. }
            | Data::Inline { bank, .. }
            | Data::Borrowed { bank, .. } => bank,
            Data::Assignment { .. } => self.with_assignment(|part| part.bank()),
        }
    }
    pub fn part_size(&self) -> u32 {
        match self.data {
            Data::Inline { size, .. } | Data::Borrowed { size, .. } => size,
            Data::Assignment { .. } => self.with_assignment(|part| part.part_size()),
            Data::Temporary { .. } => 0,
        }
    }
    pub fn const_data(&self) -> &[u64] {
        match &self.data {
            Data::Inline { word, .. } => core::slice::from_ref(word),
            Data::Borrowed { words, size, .. } => &words[..(u64::from(*size) + 7) as usize / 8],
            _ => panic!("value part is not constant"),
        }
    }
    pub fn take(&mut self) -> Self {
        let replacement = Self {
            machine: self.machine,
            reg: Reg::make_invalid(),
            data: Data::Temporary {
                bank: self.bank(),
                owned: false,
            },
        };
        core::mem::replace(self, replacement)
    }

    fn alloc_reg_impl(&mut self, reload: bool) -> PgResult<()> {
        tpde_assert!(self.machine.may_change_value_state());
        tpde_assert!(!self.has_reg());
        if self.has_assignment() && self.with_assignment(|part| part.register_valid()) {
            self.lock();
            return Ok(());
        }
        if !reload && self.has_assignment() {
            tpde_assert!(!self.with_assignment(|part| part.stack_valid()));
        }
        if reload && !self.has_assignment() {
            tpde_assert!(self.is_const(), "cannot reload temporary value");
        }
        let reg = self.machine.select_reg(self.bank())?;
        let (local, index) = if self.has_assignment() {
            (self.local_idx(), self.part())
        } else {
            (INVALID_VALUE, 0)
        };
        self.machine.with_registers(|registers| {
            registers.mark_clobbered(reg);
            registers.mark_used(reg, local, index);
            registers.mark_fixed(reg);
        });
        self.reg = reg;
        if self.has_assignment() {
            self.with_assignment(|part| {
                part.set_reg(reg);
                part.set_register_valid(true);
            });
        } else {
            self.set_owned(true);
        }
        if reload {
            let result = if self.has_assignment() {
                self.machine.regalloc_event(RegallocEvent::Reload);
                self.machine.reload_to_reg(reg, local, index)
            } else {
                self.machine.materialize_constant(
                    self.const_data(),
                    self.bank(),
                    self.part_size(),
                    reg,
                )
            };
            if let Err(error) = result {
                if self.has_assignment() {
                    self.with_assignment(|part| part.set_register_valid(false));
                }
                self.machine.with_registers(|registers| {
                    registers.unmark_fixed(reg);
                    registers.unmark_used(reg);
                });
                self.reg = Reg::make_invalid();
                return Err(error);
            }
        }
        Ok(())
    }

    pub fn alloc_reg(&mut self) -> PgResult<Reg> {
        self.alloc_reg_impl(false)?;
        Ok(self.cur_reg())
    }
    pub fn load_to_reg(&mut self) -> PgResult<Reg> {
        self.alloc_reg_impl(true)?;
        Ok(self.cur_reg())
    }
    pub fn cur_reg_or_alloc(&mut self) -> PgResult<Reg> {
        if !self.has_reg() {
            self.alloc_reg_impl(false)?;
        }
        Ok(self.cur_reg())
    }
    pub fn cur_reg_or_load(&mut self) -> PgResult<Reg> {
        if !self.has_reg() {
            self.alloc_reg_impl(true)?;
        }
        Ok(self.cur_reg())
    }
    pub fn alloc_try_reuse<'source: 'c>(
        &mut self,
        other: &mut ValuePartRef<'source, M, N>,
    ) -> PgResult<Reg> {
        tpde_assert!(core::ptr::eq(self.machine, other.machine));
        tpde_assert!(other.has_reg());
        if !self.has_assignment() || !self.with_assignment(|part| part.register_valid()) {
            tpde_assert!(
                !self.has_assignment() || !self.with_assignment(|part| part.fixed_assignment())
            );
            if other.can_salvage() {
                self.set_value_from(other)?;
                if self.has_assignment() {
                    self.lock();
                }
                return Ok(self.cur_reg());
            }
        }
        self.alloc_reg()
    }

    fn alloc_specific_impl(&mut self, reg: Reg, reload: bool) -> PgResult<()> {
        tpde_assert!(!self.has_reg());
        tpde_assert_eq!(reg.id() / 32, self.bank().id());
        let old = if self.has_assignment() {
            self.with_assignment(|part| {
                tpde_assert!(!part.fixed_assignment());
                if part.register_valid() {
                    part.get_reg()
                } else {
                    Reg::make_invalid()
                }
            })
        } else {
            Reg::make_invalid()
        };
        if old == reg {
            self.lock();
            return Ok(());
        }
        if self.has_assignment() {
            tpde_assert!(self.machine.may_change_value_state());
        }
        if !reload && self.has_assignment() {
            tpde_assert!(!self.with_assignment(|part| part.stack_valid()));
        }
        if reload && !self.has_assignment() {
            tpde_assert!(self.is_const(), "cannot reload temporary value");
        }
        if self
            .machine
            .with_registers(|registers| registers.is_used(reg))
        {
            self.machine.evict_reg(reg)?;
        }
        let old = if self.has_assignment() {
            self.with_assignment(|part| {
                if part.register_valid() {
                    part.get_reg()
                } else {
                    Reg::make_invalid()
                }
            })
        } else {
            Reg::make_invalid()
        };
        if !reload && self.has_assignment() {
            tpde_assert!(!self.with_assignment(|part| part.stack_valid()));
        }
        let (local, index) = if self.has_assignment() {
            (self.local_idx(), self.part())
        } else {
            (INVALID_VALUE, 0)
        };
        self.machine.with_registers(|registers| {
            registers.mark_clobbered(reg);
            registers.mark_used(reg, local, index);
            registers.mark_fixed(reg);
        });
        self.reg = reg;
        if self.has_assignment() {
            self.with_assignment(|part| {
                part.set_reg(reg);
                part.set_register_valid(true);
            });
        } else {
            self.set_owned(true);
        }
        if reload {
            let result = if self.has_assignment() {
                if old.valid() {
                    self.machine.mov(reg, old, self.part_size())
                } else {
                    self.machine.regalloc_event(RegallocEvent::Reload);
                    self.machine.reload_to_reg(reg, local, index)
                }
            } else {
                self.machine.materialize_constant(
                    self.const_data(),
                    self.bank(),
                    self.part_size(),
                    reg,
                )
            };
            if let Err(error) = result {
                self.machine.with_registers(|registers| {
                    registers.unmark_fixed(reg);
                    registers.unmark_used(reg);
                });
                if self.has_assignment() {
                    self.with_assignment(|part| {
                        if old.valid() {
                            part.set_reg(old);
                        }
                        part.set_register_valid(old.valid());
                    });
                }
                self.reg = Reg::make_invalid();
                return Err(error);
            }
            if old.valid() {
                self.machine
                    .with_registers(|registers| registers.unmark_used(old));
            }
        }
        Ok(())
    }
    pub fn alloc_specific(&mut self, reg: Reg) -> PgResult<()> {
        self.alloc_specific_impl(reg, false)
    }
    pub fn load_to_specific(&mut self, reg: Reg) -> PgResult<()> {
        self.alloc_specific_impl(reg, true)
    }

    pub fn reload_into_specific_fixed(&self, reg: Reg, size: u32) -> PgResult<Reg> {
        if self.is_const() {
            self.machine.materialize_constant(
                self.const_data(),
                self.bank(),
                self.part_size(),
                reg,
            )?;
            return Ok(reg);
        }
        if !self.has_assignment() {
            tpde_assert!(self.has_reg());
            tpde_assert_ne!(reg, self.cur_reg());
            tpde_assert_ne!(size, 0, "copying a temporary requires its size");
            self.machine.mov(reg, self.cur_reg(), size)?;
            return Ok(reg);
        }
        let current = self.cur_reg_unlocked();
        if current.valid() {
            tpde_assert_ne!(reg, current);
            self.machine.mov(reg, current, self.part_size())?;
        } else {
            // A fixed part without its register is one parked for a call
            // sequence (the A64 `calls.rs`): its slot is current, and the
            // load into `reg` owns nothing.
            self.machine
                .reload_to_reg(reg, self.local_idx(), self.part())?;
        }
        self.machine
            .with_registers(|registers| registers.mark_clobbered(reg));
        Ok(reg)
    }

    pub fn get_unowned(&self) -> ValuePartRef<'_, M, N> {
        tpde_assert!(self.has_reg());
        tpde_assert!((self.bank().id() as usize) < N);
        let mut result = ValuePartRef::empty(self.machine);
        result.reg = self.reg;
        result
    }
    pub fn get_unowned_ref(&self) -> ValuePartRef<'_, M, N> {
        self.get_unowned()
    }
    pub fn into_temporary(mut self) -> PgResult<Self> {
        if self.is_const() {
            let mut result = match self.data {
                Data::Inline {
                    word, size, bank, ..
                } => Self::constant_inline(self.machine, word, size, bank),
                Data::Borrowed {
                    words, size, bank, ..
                } => Self::constant_borrowed(self.machine, words, size, bank),
                _ => unreachable!(),
            };
            result.load_to_reg()?;
            return Ok(result);
        }
        tpde_assert!(
            self.has_assignment() || self.owned(),
            "into_temporary from unowned part is unimplemented upstream"
        );
        let mut result = Self::temporary(self.machine, self.bank());
        result.set_value(self.take())?;
        if !result.has_reg() {
            tpde_assert!(result.is_const());
            result.load_to_reg()?;
        }
        Ok(result)
    }
    pub fn into_scratch(mut self) -> PgResult<ScratchReg<'c, M, N>> {
        tpde_assert!(
            self.has_assignment() || self.owned() || self.is_const(),
            "into_scratch from unowned part is unimplemented upstream"
        );
        let mut result = ScratchReg::new(self.machine);
        if self.can_salvage() {
            result.alloc_specific(self.salvage())?;
        } else {
            let reg = result.alloc(self.bank())?;
            self.reload_into_specific_fixed(reg, 0)?;
        }
        Ok(result)
    }
    pub fn into_extended(mut self, sign: bool, from: u32, to: u32) -> PgResult<Self> {
        tpde_assert!(from < to);
        if self.is_const() && to <= 64 {
            tpde_assert!(from != 0);
            let word = self.const_data()[0];
            let shift = 64 - from;
            let extended = if sign {
                ((word << shift) as i64 >> shift) as u64
            } else {
                (word << shift) >> shift
            };
            return Ok(Self::constant_inline(
                self.machine,
                extended,
                (to + 7) / 8,
                self.bank(),
            ));
        }
        let mut result = Self::temporary(self.machine, self.bank());
        let source = self.cur_reg_or_load()?;
        if self.can_salvage() {
            result.set_value(self.take())?;
            tpde_assert_eq!(source, result.cur_reg());
        } else {
            result.alloc_reg()?;
        }
        self.machine
            .generate_raw_intext(result.cur_reg(), source, sign, from, to)?;
        Ok(result)
    }

    pub fn lock(&mut self) {
        tpde_assert!(self.has_assignment());
        tpde_assert!(!self.has_reg());
        let reg = self.with_assignment(|part| {
            tpde_assert!(part.register_valid());
            part.get_reg()
        });
        self.machine
            .with_registers(|registers| registers.inc_lock_count(reg));
        self.reg = reg;
    }
    pub fn unlock(&mut self) {
        tpde_assert!(self.has_assignment());
        if self.has_reg() {
            self.machine.with_registers(|registers| {
                registers.dec_lock_count(self.reg);
            });
            self.reg = Reg::make_invalid();
        }
    }
    pub fn set_modified(&self) {
        tpde_assert!(self.has_reg() && self.has_assignment());
        self.with_assignment(|part| part.set_modified(true));
    }

    pub fn set_value(&mut self, mut other: Self) -> PgResult<()> {
        self.set_value_from(&mut other)
    }

    pub fn set_value_from<'source: 'c>(
        &mut self,
        other: &mut ValuePartRef<'source, M, N>,
    ) -> PgResult<()> {
        tpde_assert!(core::ptr::eq(self.machine, other.machine));
        if !self.has_assignment() {
            tpde_assert!(!self.is_const());
            if !other.has_assignment() {
                self.reset();
                *self = other.take();
                return Ok(());
            }
            if !other.can_salvage() {
                let reg = self.alloc_reg()?;
                if let Err(error) = other.reload_into_specific_fixed(reg, 0) {
                    self.reset();
                    return Err(error);
                }
                other.reset();
                return Ok(());
            }
            self.reset();
            self.reg = other.salvage_keep_used();
            self.set_owned(true);
            self.machine.with_registers(|registers| {
                registers.mark_fixed(self.reg);
                registers.update_reg_assignment(self.reg, INVALID_VALUE, 0);
            });
            return Ok(());
        }
        tpde_assert!(!self.with_assignment(|part| part.variable_ref()));
        if self.with_assignment(|part| part.fixed_assignment()) || !other.can_salvage() {
            let old = self.with_assignment(|part| part.raw());
            self.with_assignment(|part| part.set_modified(true));
            let reg = match self.alloc_reg() {
                Ok(reg) => reg,
                Err(error) => {
                    self.with_assignment(|part| part.set_raw(old));
                    return Err(error);
                }
            };
            if let Err(error) = other.reload_into_specific_fixed(reg, self.part_size()) {
                self.unlock();
                if old & (1 << 11) == 0 {
                    self.machine
                        .with_registers(|registers| registers.unmark_used(reg));
                }
                self.with_assignment(|part| part.set_raw(old));
                return Err(error);
            }
            other.reset();
            self.unlock();
            self.with_assignment(|part| {
                part.set_register_valid(true);
                part.set_modified(true);
            });
            return Ok(());
        }
        if self.with_assignment(|part| part.register_valid()) {
            self.unlock();
            let reg = self.with_assignment(|part| part.get_reg());
            self.machine.with_registers(|registers| {
                tpde_assert!(!registers.is_fixed(reg));
                registers.unmark_used(reg);
            });
        }
        let reg = other.salvage_keep_used();
        self.machine.with_registers(|registers| {
            registers.update_reg_assignment(reg, self.local_idx(), self.part())
        });
        self.with_assignment(|part| {
            part.set_reg(reg);
            part.set_register_valid(true);
            part.set_modified(true);
        });
        Ok(())
    }

    pub fn set_scratch(&mut self, mut other: ScratchReg<'c, M, N>) -> PgResult<()> {
        tpde_assert!(core::ptr::eq(self.machine, other.machine()));
        tpde_assert!(self.machine.may_change_value_state());
        tpde_assert!(other.has_reg());
        tpde_assert!(!self.has_reg());
        let reg = other.cur_reg();
        self.machine.with_registers(|registers| {
            tpde_assert!(registers.is_fixed(reg));
            tpde_assert!(registers.is_used(reg));
            tpde_assert!(registers.is_clobbered(reg));
        });
        if !self.has_assignment() {
            tpde_assert!(!self.is_const());
            self.machine.with_registers(|registers| {
                tpde_assert_eq!(registers.reg_local_idx(reg), INVALID_VALUE);
                tpde_assert_eq!(registers.reg_part(reg), 0);
            });
            self.reg = reg;
            self.set_owned(true);
            other.force_set_reg(Reg::make_invalid());
            return Ok(());
        }
        tpde_assert!(!self.with_assignment(|part| part.variable_ref()));
        if self.with_assignment(|part| part.fixed_assignment()) {
            let destination = self.with_assignment(|part| {
                tpde_assert!(part.register_valid() && !part.stack_valid());
                part.get_reg()
            });
            self.machine.with_registers(|registers| {
                tpde_assert!(registers.is_used(destination));
                tpde_assert!(registers.is_fixed(destination));
                tpde_assert_eq!(registers.reg_local_idx(destination), self.local_idx());
            });
            tpde_assert_ne!(destination, reg);
            self.machine.mov(destination, reg, self.part_size())?;
            other.reset();
            return Ok(());
        }
        tpde_assert!(self.with_assignment(|part| !part.register_valid() && !part.stack_valid()));
        self.machine.with_registers(|registers| {
            registers.unmark_fixed(reg);
            registers.update_reg_assignment(reg, self.local_idx(), self.part());
        });
        self.with_assignment(|part| {
            part.set_reg(reg);
            part.set_register_valid(true);
            part.set_modified(true);
        });
        other.force_set_reg(Reg::make_invalid());
        Ok(())
    }

    pub fn set_value_reg(&mut self, reg: Reg) -> PgResult<()> {
        tpde_assert!(self.machine.may_change_value_state());
        tpde_assert!(reg.valid());
        tpde_assert!(!self.has_reg());
        if !self.has_assignment() {
            tpde_assert!(!self.is_const());
            self.machine.with_registers(|registers| {
                registers.mark_used(reg, INVALID_VALUE, 0);
                registers.mark_fixed(reg);
            });
            self.reg = reg;
            self.set_owned(true);
            return Ok(());
        }
        tpde_assert!(!self.with_assignment(|part| part.variable_ref()));
        if self.with_assignment(|part| part.fixed_assignment()) {
            let destination = self.with_assignment(|part| part.get_reg());
            self.machine.with_registers(|registers| {
                tpde_assert!(registers.is_used(destination));
                tpde_assert!(registers.is_fixed(destination));
                tpde_assert_eq!(registers.reg_local_idx(destination), self.local_idx());
            });
            tpde_assert_ne!(destination, reg);
            self.machine.mov(destination, reg, self.part_size())?;
            self.with_assignment(|part| {
                part.set_register_valid(true);
                part.set_modified(true);
            });
            return Ok(());
        }
        tpde_assert!(self.with_assignment(|part| !part.register_valid() && !part.stack_valid()));
        self.machine.with_registers(|registers| {
            registers.mark_used(reg, self.local_idx(), self.part());
            registers.mark_clobbered(reg);
        });
        self.with_assignment(|part| {
            part.set_reg(reg);
            part.set_register_valid(true);
            part.set_modified(true);
        });
        Ok(())
    }

    pub fn can_salvage(&self) -> bool {
        self.owned()
            && if self.has_assignment() {
                self.with_assignment(|part| part.register_valid())
                    && !self.machine.shadow_assignment(self.local_idx())
            } else {
                self.has_reg()
            }
    }
    fn salvage_keep_used(&mut self) -> Reg {
        tpde_assert!(self.machine.may_change_value_state());
        tpde_assert!(self.can_salvage());
        if !self.has_assignment() {
            let reg = self.reg;
            self.machine
                .with_registers(|registers| registers.unmark_fixed(reg));
            self.reg = Reg::make_invalid();
            return reg;
        }
        let (reg, fixed, pinned, bank) = self.with_assignment(|part| {
            (
                part.get_reg(),
                part.fixed_assignment(),
                part.pinned(),
                part.bank(),
            )
        });
        let locked = core::mem::replace(&mut self.reg, Reg::make_invalid());
        self.machine.with_registers(|registers| {
            if locked.valid() {
                registers.dec_lock_count(locked);
            }
            tpde_assert!(fixed || !registers.is_fixed(reg));
            if fixed {
                registers.dec_lock_count(reg);
            }
        });
        if fixed {
            if pinned {
                self.machine.decrement_pinned_count(bank);
            } else {
                self.machine.decrement_fixed_count(bank);
            }
        }
        self.with_assignment(|part| {
            part.set_register_valid(false);
            part.set_fixed_assignment(false);
            part.set_pinned(false);
        });
        reg
    }
    pub fn salvage(&mut self) -> Reg {
        let reg = self.salvage_keep_used();
        self.machine
            .with_registers(|registers| registers.unmark_used(reg));
        reg
    }
    pub fn reset(&mut self) {
        if !self.has_reg() {
            return;
        }
        let reg = self.reg;
        if self.owned() {
            if self.has_assignment() && self.machine.shadow_assignment(self.local_idx()) {
                // The register is a phi's (`shadow_phi`): drop only this
                // reference's lock; the assignment's free leaves it alone.
                self.machine
                    .with_registers(|registers| registers.dec_lock_count(reg));
            } else if self.has_assignment() {
                let (fixed, pinned, bank) = self.with_assignment(|part| {
                    let result = (part.fixed_assignment(), part.pinned(), part.bank());
                    part.set_register_valid(false);
                    part.set_fixed_assignment(false);
                    part.set_pinned(false);
                    result
                });
                self.machine.with_registers(|registers| {
                    registers.dec_lock_count_must_zero(reg, if fixed { 2 } else { 1 });
                    registers.unmark_used(reg)
                });
                if fixed {
                    if pinned {
                        self.machine.decrement_pinned_count(bank);
                    } else {
                        self.machine.decrement_fixed_count(bank);
                    }
                }
            } else {
                self.machine.with_registers(|registers| {
                    registers.unmark_fixed(reg);
                    registers.unmark_used(reg)
                });
            }
        } else if self.has_assignment() {
            self.machine.with_registers(|registers| {
                registers.dec_lock_count(reg);
            });
        }
        self.reg = Reg::make_invalid();
    }
}

impl<M: GuardMachine<N>, const N: usize> Drop for ValuePartRef<'_, M, N> {
    fn drop(&mut self) {
        self.reset();
    }
}
