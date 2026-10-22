// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{
    value_assignment::{Assignment, ValueAssignment, FIRST_PART_OFFSET},
    Reg, RegBank,
};
use core::{cell::Cell, marker::PhantomData, mem, ptr::NonNull};
use tpde_util::{tpde_assert, tpde_assert_eq, tpde_assert_ne};

#[repr(C)]
#[derive(Clone, Copy)]
#[doc = include_str!("../tests/assignment-part-ref-ownership.md")]
pub struct AssignmentPartRef<'r, 'a, 'id> {
    assignment: NonNull<ValueAssignment>,
    part: u32,
    borrow: PhantomData<&'r Assignment<'a, 'id>>,
}

impl<'r, 'a, 'id> AssignmentPartRef<'r, 'a, 'id> {
    pub const fn new(assignment: &'r Assignment<'a, 'id>, part: u32) -> Self {
        // SAFETY: the token borrow keeps its initialized allocation live and shared for 'r.
        unsafe { Self::from_raw(assignment.pointer(), part) }
    }

    /// # Safety
    /// `assignment` must be the whole-allocation pointer of an initialized
    /// assignment that stays allocated, and is only accessed through `Cell`
    /// fields or shared reads, for as long as `'r` lasts. A token in a slot
    /// loan or an assignment pinned by a counted `ValueRef` satisfies this.
    pub const unsafe fn from_raw(assignment: NonNull<ValueAssignment>, part: u32) -> Self {
        Self {
            assignment,
            part,
            borrow: PhantomData,
        }
    }

    #[inline]
    fn word(&self) -> &Cell<u16> {
        assert!(
            self.part < self.assignment().part_count(),
            "assignment part index out of bounds"
        );
        // SAFETY: from_raw's contract plus the bounds check above put this part inside the
        // allocation's initialized trailing part words; the whole-allocation pointer carries
        // provenance for them.
        unsafe {
            &*self
                .assignment
                .as_ptr()
                .cast::<u8>()
                .add(FIRST_PART_OFFSET as usize)
                .cast::<Cell<u16>>()
                .add(self.part as usize)
        }
    }

    pub fn reset(&self) {
        self.word().set(1 << 9);
    }

    pub fn assignment(&self) -> &'r ValueAssignment {
        // SAFETY: from_raw's contract; the header is initialized and only Cell fields change.
        unsafe { self.assignment.as_ref() }
    }

    pub fn raw(&self) -> u16 {
        self.word().get()
    }

    pub fn set_raw(&self, value: u16) {
        self.word().set(value);
    }

    pub fn bank(&self) -> RegBank {
        RegBank::new(((self.word().get() >> 5) & 7) as u8)
    }

    pub fn set_bank(&self, bank: RegBank) {
        tpde_assert!(
            bank.id() <= 7,
            "assignment register bank exceeds three bits"
        );
        let word = self.word();
        word.set((word.get() & !0xe0) | (u16::from(bank.id()) << 5));
    }

    pub fn get_reg(&self) -> Reg {
        Reg::new(u64::from(self.word().get() & 0xff))
    }

    pub fn set_reg(&self, reg: Reg) {
        let word = self.word();
        let value = word.get();
        tpde_assert_eq!(
            (value >> 5) & 7,
            u16::from(reg.id() >> 5),
            "assignment register bank mismatch"
        );
        word.set((value & 0xff00) | u16::from(reg.id()));
    }

    pub fn modified(&self) -> bool {
        self.word().get() & (1 << 9) != 0
    }

    pub fn set_modified(&self, value: bool) {
        self.set_flag(1 << 9, value);
    }

    pub fn fixed_assignment(&self) -> bool {
        self.word().get() & (1 << 8) != 0
    }

    pub fn set_fixed_assignment(&self, value: bool) {
        self.set_flag(1 << 8, value);
    }

    /// A fixed assignment created by phi coalescing at an edge: counted in
    /// the pinned budget rather than the classic fixed one, never demoted.
    pub fn pinned(&self) -> bool {
        self.word().get() & (1 << 10) != 0
    }

    pub fn set_pinned(&self, value: bool) {
        self.set_flag(1 << 10, value);
    }

    /// A phi's part: its register is what every incoming edge writes, so a
    /// fixed assignment on it is never demoted.
    pub fn no_demote(&self) -> bool {
        self.word().get() & (1 << 15) != 0
    }

    pub fn set_no_demote(&self, value: bool) {
        self.set_flag(1 << 15, value);
    }

    pub fn variable_ref(&self) -> bool {
        self.assignment().variable_ref()
    }

    pub fn is_stack_variable(&self) -> bool {
        self.assignment().stack_variable()
    }

    pub fn register_valid(&self) -> bool {
        self.word().get() & (1 << 11) != 0
    }

    pub fn set_register_valid(&self, value: bool) {
        self.set_flag(1 << 11, value);
    }

    pub fn stack_valid(&self) -> bool {
        !self.modified()
    }

    pub fn set_stack_valid(&self) {
        self.set_modified(false);
    }

    #[inline]
    fn set_flag(&self, mask: u16, value: bool) {
        let word = self.word();
        word.set((word.get() & !mask) | if value { mask } else { 0 });
    }

    pub fn part_size(&self) -> u32 {
        1 << ((self.word().get() >> 12) & 7)
    }

    pub fn set_part_size(&self, part_size: u32) {
        tpde_assert!(
            part_size.is_power_of_two() && part_size <= 128,
            "assignment part size must be a power of two from 1 through 128"
        );
        let word = self.word();
        word.set((word.get() & !0x7000) | ((part_size.trailing_zeros() as u16) << 12));
    }

    pub fn frame_off(&self) -> i32 {
        tpde_assert!(
            !self.variable_ref(),
            "variable-ref has no normal frame offset"
        );
        let base = self.assignment().frame_off();
        tpde_assert_ne!(base, 0, "attempt to access uninitialized stack slot");
        (base as u32).wrapping_add(self.part_off()) as i32
    }

    pub fn variable_stack_off(&self) -> i32 {
        tpde_assert!(
            self.variable_ref() && self.is_stack_variable(),
            "assignment is not a stack variable reference"
        );
        tpde_assert_eq!(self.part, 0, "variable reference requires part zero");
        self.assignment().frame_off()
    }

    pub fn variable_ref_data(&self) -> u32 {
        tpde_assert!(
            self.variable_ref() && !self.is_stack_variable(),
            "assignment is not a custom variable reference"
        );
        tpde_assert_eq!(self.part, 0, "variable reference requires part zero");
        self.assignment().var_ref_custom_idx()
    }

    pub fn part_off(&self) -> u32 {
        u32::from(self.assignment().max_part_size()).wrapping_mul(self.part)
    }
}

mcx::forget_safe_nodrop!(AssignmentPartRef<'_, '_, '_>);

const _: () = {
    // 64-bit-host layout pin (TPDE is a native-only backend; wasm32 only check-builds it).
    #[cfg(target_pointer_width = "64")]
    assert!(mem::size_of::<AssignmentPartRef<'_, '_, '_>>() == 16);
    // 64-bit-host layout pin (TPDE is a native-only backend; wasm32 only check-builds it).
    #[cfg(target_pointer_width = "64")]
    assert!(mem::align_of::<AssignmentPartRef<'_, '_, '_>>() == 8);
    assert!(!mem::needs_drop::<AssignmentPartRef<'_, '_, '_>>());
};

#[cfg(test)]
#[path = "../tests/assignment_part_ref/unit.rs"]
mod tests;
