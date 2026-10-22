// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![doc = include_str!("../tests/register_ownership.md")]

use core::cell::Cell;
use tpde_base::ValLocalIdx;
use tpde_util::{misc::BitSetIterator, tpde_assert, tpde_assert_eq};

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reg(u8);

impl Reg {
    pub const fn new(id: u64) -> Self {
        tpde_assert!(id <= u8::MAX as u64, "register id exceeds byte range");
        Self(id as u8)
    }

    pub const fn id(self) -> u8 {
        self.0
    }

    pub const fn invalid(self) -> bool {
        self.0 == u8::MAX
    }

    pub const fn valid(self) -> bool {
        !self.invalid()
    }

    pub const fn make_invalid() -> Self {
        Self(u8::MAX)
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegBank(u8);

impl RegBank {
    pub const fn new(bank: u8) -> Self {
        Self(bank)
    }

    pub const fn id(self) -> u8 {
        self.0
    }
}

impl Default for RegBank {
    fn default() -> Self {
        Self(u8::MAX)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Assignment {
    pub local_idx: ValLocalIdx,
    pub part: u32,
}

pub type RegBitSet = u64;

#[repr(C)]
pub struct RegisterFile<const NUM_BANKS: usize, const REGS_PER_BANK: usize> {
    pub allocatable: Cell<RegBitSet>,
    pub used: Cell<RegBitSet>,
    pub clobbered: Cell<RegBitSet>,
    pub clocks: [Cell<u8>; NUM_BANKS],
    pub assignments: [[Cell<Assignment>; REGS_PER_BANK]; NUM_BANKS],
    pub lock_counts: [[Cell<u8>; REGS_PER_BANK]; NUM_BANKS],
    _dimensions_checked: (),
}

impl<const B: usize, const R: usize> RegisterFile<B, R> {
    pub const NUM_REGS: usize = {
        assert!(
            R.is_power_of_two(),
            "registers per bank must be a power of two"
        );
        assert!(B <= 64 / R, "register file exceeds 64 registers");
        B * R
    };

    pub const fn new() -> Self {
        let _ = Self::NUM_REGS;
        Self {
            allocatable: Cell::new(0),
            used: Cell::new(0),
            clobbered: Cell::new(0),
            clocks: [const { Cell::new(0) }; B],
            assignments: [const {
                [const {
                    Cell::new(Assignment {
                        local_idx: ValLocalIdx::new(0),
                        part: 0,
                    })
                }; R]
            }; B],
            lock_counts: [const { [const { Cell::new(0) }; R] }; B],
            _dimensions_checked: (),
        }
    }

    pub fn reset(&self) {
        self.used.set(0);
        self.clobbered.set(0);
        for clock in &self.clocks {
            clock.set(0);
        }
        for count in self.lock_counts.as_flattened() {
            count.set(0);
        }
    }

    #[inline]
    fn index(reg: Reg) -> usize {
        let id = reg.id() as usize;
        tpde_assert!(id < Self::NUM_REGS, "register outside register file");
        id
    }
    #[inline]
    fn assignment(&self, reg: Reg) -> &Cell<Assignment> {
        &self.assignments.as_flattened()[Self::index(reg)]
    }
    #[inline]
    fn lock_count(&self, reg: Reg) -> u8 {
        self.lock_counts.as_flattened()[Self::index(reg)].get()
    }
    #[inline]
    fn lock_count_cell(&self, reg: Reg) -> &Cell<u8> {
        &self.lock_counts.as_flattened()[Self::index(reg)]
    }

    #[inline]
    pub fn is_used(&self, reg: Reg) -> bool {
        Self::index(reg);
        self.used.get() & (1u64 << reg.id()) != 0
    }

    #[inline]
    pub fn is_fixed(&self, reg: Reg) -> bool {
        self.lock_count(reg) > 0
    }

    /// The lock count: the fixed-assignment lock plus one per outstanding
    /// locked value-part reference.
    #[inline]
    pub fn reg_lock_count(&self, reg: Reg) -> u8 {
        self.lock_count(reg)
    }

    #[inline]
    pub fn is_clobbered(&self, reg: Reg) -> bool {
        Self::index(reg);
        self.clobbered.get() & (1u64 << reg.id()) != 0
    }

    #[inline]
    pub fn mark_used(&self, reg: Reg, local_idx: ValLocalIdx, part: u32) {
        tpde_assert!(!self.is_used(reg), "register already used");
        tpde_assert!(!self.is_fixed(reg), "register is fixed");
        self.used.set(self.used.get() | 1u64 << reg.id());
        self.assignment(reg).set(Assignment { local_idx, part });
    }

    #[inline]
    pub fn update_reg_assignment(&self, reg: Reg, local_idx: ValLocalIdx, part: u32) {
        tpde_assert!(self.is_used(reg), "register is not used");
        self.assignment(reg).set(Assignment { local_idx, part });
    }

    #[inline]
    pub fn unmark_used(&self, reg: Reg) {
        tpde_assert!(self.is_used(reg), "register is not used");
        tpde_assert!(!self.is_fixed(reg), "register is fixed");
        self.used.set(self.used.get() & !(1u64 << reg.id()));
    }

    #[inline]
    pub fn mark_fixed(&self, reg: Reg) {
        tpde_assert!(self.is_used(reg), "register is not used");
        let count = self.lock_count_cell(reg);
        tpde_assert_eq!(count.get(), 0, "register already fixed");
        count.set(1);
    }

    #[inline]
    pub fn unmark_fixed(&self, reg: Reg) {
        tpde_assert!(self.is_used(reg), "register is not used");
        let count = self.lock_count_cell(reg);
        tpde_assert_eq!(count.get(), 1, "register does not have one lock");
        count.set(0);
    }

    #[inline]
    pub fn inc_lock_count(&self, reg: Reg) {
        tpde_assert!(self.is_used(reg), "register is not used");
        let count = self.lock_count_cell(reg);
        tpde_assert!(count.get() < u8::MAX, "register lock count overflow");
        count.set(count.get().wrapping_add(1));
    }

    #[inline]
    pub fn dec_lock_count(&self, reg: Reg) -> bool {
        tpde_assert!(self.is_used(reg), "register is not used");
        let count = self.lock_count_cell(reg);
        tpde_assert!(count.get() > 0, "register is not locked");
        let value = count.get().wrapping_sub(1);
        count.set(value);
        value == 0
    }

    #[inline]
    pub fn dec_lock_count_must_zero(&self, reg: Reg, sub: u8) {
        tpde_assert!(self.is_used(reg), "register is not used");
        let count = self.lock_count_cell(reg);
        tpde_assert_eq!(count.get(), sub, "register lock count mismatch");
        count.set(0);
    }

    #[inline]
    pub fn mark_clobbered(&self, reg: Reg) {
        Self::index(reg);
        self.clobbered.set(self.clobbered.get() | 1u64 << reg.id());
    }

    #[inline]
    pub fn reg_local_idx(&self, reg: Reg) -> ValLocalIdx {
        tpde_assert!(self.is_used(reg), "register is not used");
        self.assignment(reg).get().local_idx
    }

    #[inline]
    pub fn reg_part(&self, reg: Reg) -> u32 {
        tpde_assert!(self.is_used(reg), "register is not used");
        self.assignment(reg).get().part
    }

    #[inline]
    pub fn used_regs(&self) -> BitSetIterator {
        BitSetIterator::new(self.used.get())
    }

    #[inline]
    pub fn find_first_free_excluding(&self, bank: RegBank, exclusion_mask: u64) -> Reg {
        let selectable =
            self.allocatable.get() & !self.used.get() & Self::bank_regs(bank) & !exclusion_mask;
        if selectable == 0 {
            Reg::make_invalid()
        } else {
            Reg::new(selectable.trailing_zeros() as u64)
        }
    }

    #[inline]
    pub fn find_first_nonfixed_excluding(&self, bank: RegBank, exclusion_mask: u64) -> Reg {
        for id in BitSetIterator::<false>::new(self.used.get() & Self::bank_regs(bank)) {
            let reg = Reg::new(id);
            if !self.is_fixed(reg) && exclusion_mask & (1u64 << id) == 0 {
                return reg;
            }
        }
        Reg::make_invalid()
    }

    #[inline]
    pub fn reg_bank(reg: Reg) -> RegBank {
        let _ = Self::NUM_REGS;
        RegBank::new((reg.id() as usize / R) as u8)
    }

    #[inline]
    pub fn bank_regs(bank: RegBank) -> RegBitSet {
        let _ = Self::NUM_REGS;
        tpde_assert!(bank.id() <= 1, "bank mask supports bank zero or one");
        let shift = bank.id() as usize * R;
        tpde_assert!(shift < 64, "bank mask shift exceeds word width");
        let mask = if R == 64 { u64::MAX } else { (1u64 << R) - 1 };
        mask << shift
    }
}

impl<const B: usize, const R: usize> Default for RegisterFile<B, R> {
    fn default() -> Self {
        Self::new()
    }
}

const _: () = {
    assert!(core::mem::size_of::<Reg>() == 1);
    assert!(core::mem::size_of::<RegBank>() == 1);
    assert!(core::mem::size_of::<Assignment>() == 8);
    assert!(!core::mem::needs_drop::<RegisterFile<2, 32>>());
    assert!(core::mem::size_of::<RegisterFile<2, 32>>() == 608);
    assert!(core::mem::size_of::<RegisterFile<0, 1>>() == 24);
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    #[test]
    fn register_and_bank_byte_forms() {
        for id in 0..=255 {
            let reg = Reg::new(id);
            assert_eq!(reg.id() as u64, id);
            assert_eq!(reg.valid(), id != 255);
            assert_eq!(reg.invalid(), id == 255);
            assert_eq!(RegBank::new(id as u8).id(), id as u8);
            assert_eq!(RegisterFile::<2, 32>::reg_bank(reg).id(), (id / 32) as u8);
        }
        assert_eq!(RegBank::default().id(), 255);
        assert_eq!(Reg::make_invalid(), Reg::new(255));
        if !tpde_util::CHECKED {
            return;
        }
        assert!(catch_unwind(|| Reg::new(256)).is_err());
        assert!(catch_unwind(|| Reg::new(u64::MAX)).is_err());
    }

    #[test]
    fn all_register_bits_assign_update_release_and_clobber() {
        let mut file = RegisterFile::<2, 32>::new();
        for id in 0..64 {
            let reg = Reg::new(id);
            file.mark_used(reg, ValLocalIdx::new(id as u32), u32::MAX);
            file.mark_clobbered(reg);
            assert_eq!(file.reg_local_idx(reg).get(), id as u32);
            assert_eq!(file.reg_part(reg), u32::MAX);
            file.update_reg_assignment(reg, ValLocalIdx::new(u32::MAX), id as u32);
            assert_eq!(file.reg_local_idx(reg).get(), u32::MAX);
            assert_eq!(file.reg_part(reg), id as u32);
        }
        assert_eq!(file.used.get(), u64::MAX);
        assert_eq!(file.clobbered.get(), u64::MAX);
        assert!(file.used_regs().eq(0..64));
        for id in 0..64 {
            let reg = Reg::new(id);
            file.unmark_used(reg);
            assert!(!file.is_used(reg));
            assert!(file.is_clobbered(reg));
            let assignment = file.assignments[id as usize / 32][id as usize % 32].get();
            assert_eq!(assignment.local_idx.get(), u32::MAX);
            assert_eq!(assignment.part, id as u32);
        }
        assert_eq!(file.used.get(), 0);
        assert_eq!(file.used_regs().next(), None);
    }

    #[test]
    fn nested_lock_transitions_and_bulk_unlock() {
        let mut file = RegisterFile::<1, 1>::new();
        let reg = Reg::new(0);
        file.mark_used(reg, ValLocalIdx::new(0), 0);
        file.mark_fixed(reg);
        file.unmark_fixed(reg);
        for count in 1..=255 {
            file.inc_lock_count(reg);
            assert_eq!(file.lock_counts[0][0].get(), count);
        }
        if tpde_util::CHECKED {
            assert!(catch_unwind(AssertUnwindSafe(|| file.inc_lock_count(reg))).is_err());
            assert_eq!(file.lock_counts[0][0].get(), 255);
        }
        for count in (0..255).rev() {
            assert_eq!(file.dec_lock_count(reg), count == 0);
            assert_eq!(file.lock_counts[0][0].get(), count);
        }
        file.dec_lock_count_must_zero(reg, 0);
        for _ in 0..255 {
            file.inc_lock_count(reg);
        }
        file.dec_lock_count_must_zero(reg, 255);
        assert!(!file.is_fixed(reg));
    }

    #[test]
    fn reset_preserves_assignments_and_allocatable() {
        let mut file = RegisterFile::<2, 32>::new();
        file.allocatable.set(0xaaaa_ffff_5555_ffff);
        file.clocks[0].set(17);
        file.clocks[1].set(31);
        file.mark_used(Reg::new(63), ValLocalIdx::new(72), 31);
        file.inc_lock_count(Reg::new(63));
        file.mark_clobbered(Reg::new(4));
        file.reset();
        assert_eq!(file.allocatable.get(), 0xaaaa_ffff_5555_ffff);
        assert_eq!((file.used.get(), file.clobbered.get()), (0, 0));
        assert_eq!(file.clocks.each_ref().map(Cell::get), [0, 0]);
        assert!(file
            .lock_counts
            .iter()
            .flatten()
            .all(|count| count.get() == 0));
        assert_eq!(
            file.assignments[1][31].get(),
            Assignment {
                local_idx: ValLocalIdx::new(72),
                part: 31
            }
        );
        file.mark_used(Reg::new(63), ValLocalIdx::new(3), 2);
        assert_eq!(file.reg_local_idx(Reg::new(63)).get(), 3);
    }

    #[test]
    fn selections_follow_bank_masks_exclusions_and_lock_order() {
        let mut file = RegisterFile::<2, 32>::new();
        file.allocatable.set(u64::MAX);
        for id in 0..64 {
            let bank = RegBank::new((id / 32) as u8);
            assert_eq!(file.find_first_free_excluding(bank, 0), Reg::new(id));
            file.mark_used(Reg::new(id), ValLocalIdx::new(id as u32), 0);
            file.mark_fixed(Reg::new(id));
        }
        for bank in [RegBank::new(0), RegBank::new(1)] {
            assert!(file.find_first_free_excluding(bank, 0).invalid());
            assert!(file.find_first_nonfixed_excluding(bank, 0).invalid());
        }
        for id in (0..64).rev() {
            let reg = Reg::new(id);
            let bank = RegBank::new((id / 32) as u8);
            file.unmark_fixed(reg);
            assert_eq!(file.find_first_nonfixed_excluding(bank, 0), reg);
            assert!(file.find_first_nonfixed_excluding(bank, u64::MAX).invalid());
            assert_ne!(file.find_first_nonfixed_excluding(bank, 1 << id), reg);
        }
        file.unmark_used(Reg::new(63));
        assert_eq!(
            file.find_first_free_excluding(RegBank::new(1), 0),
            Reg::new(63)
        );
        assert!(file
            .find_first_free_excluding(RegBank::new(1), 1 << 63)
            .invalid());
    }

    #[test]
    fn bank_boundaries_include_single_full_width_and_source_bank_one() {
        assert_eq!(
            RegisterFile::<2, 32>::bank_regs(RegBank::new(0)),
            u32::MAX as u64
        );
        assert_eq!(
            RegisterFile::<2, 32>::bank_regs(RegBank::new(1)),
            (u32::MAX as u64) << 32
        );
        assert_eq!(RegisterFile::<1, 1>::bank_regs(RegBank::new(1)), 2);
        assert_eq!(
            RegisterFile::<4, 16>::bank_regs(RegBank::new(1)),
            0xffff_0000
        );
        assert_eq!(RegisterFile::<1, 64>::bank_regs(RegBank::new(0)), u64::MAX);
        if tpde_util::CHECKED {
            assert!(catch_unwind(|| RegisterFile::<1, 64>::bank_regs(RegBank::new(1))).is_err());
            assert!(catch_unwind(|| RegisterFile::<2, 32>::bank_regs(RegBank::new(2))).is_err());
        }
        let mut file = RegisterFile::<1, 64>::new();
        file.allocatable.set(1 << 63);
        assert_eq!(
            file.find_first_free_excluding(RegBank::new(0), 0),
            Reg::new(63)
        );
        file.mark_used(Reg::new(63), ValLocalIdx::new(1), 2);
        assert_eq!(
            file.find_first_nonfixed_excluding(RegBank::new(0), 0),
            Reg::new(63)
        );
        assert_eq!(RegisterFile::<0, 1>::new().used.get(), 0);
    }

    #[test]
    fn invalid_transitions_panic_before_changing_state() {
        if !tpde_util::CHECKED {
            return;
        }
        let mut file = RegisterFile::<1, 1>::new();
        let reg = Reg::new(0);
        assert!(catch_unwind(AssertUnwindSafe(|| file.is_used(Reg::new(1)))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| file.is_fixed(Reg::make_invalid()))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| file.is_clobbered(Reg::new(1)))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| file.mark_clobbered(Reg::new(1)))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| file.unmark_used(reg))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| file.mark_fixed(reg))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| file.unmark_fixed(reg))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| file.inc_lock_count(reg))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| file.dec_lock_count(reg))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| file.dec_lock_count_must_zero(reg, 0))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| file.update_reg_assignment(
            reg,
            ValLocalIdx::new(1),
            2
        )))
        .is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| file.reg_local_idx(reg))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| file.reg_part(reg))).is_err());
        assert_eq!(
            (
                file.used.get(),
                file.clobbered.get(),
                file.lock_counts[0][0].get()
            ),
            (0, 0, 0)
        );
        file.mark_used(reg, ValLocalIdx::new(8), 9);
        assert!(catch_unwind(AssertUnwindSafe(|| file.mark_used(
            reg,
            ValLocalIdx::new(1),
            2
        )))
        .is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| file.dec_lock_count(reg))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| file.unmark_fixed(reg))).is_err());
        file.mark_fixed(reg);
        assert!(catch_unwind(AssertUnwindSafe(|| file.mark_fixed(reg))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| file.unmark_used(reg))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| file.dec_lock_count_must_zero(reg, 2))).is_err());
        file.inc_lock_count(reg);
        assert!(catch_unwind(AssertUnwindSafe(|| file.unmark_fixed(reg))).is_err());
        assert_eq!(
            (
                file.used.get(),
                file.lock_counts[0][0].get(),
                file.reg_part(reg)
            ),
            (1, 2, 9)
        );
        file.dec_lock_count_must_zero(reg, 2);
        file.unmark_used(reg);
        file.mark_used(reg, ValLocalIdx::new(5), 6);
        assert_eq!(file.reg_part(reg), 6);
    }
}
