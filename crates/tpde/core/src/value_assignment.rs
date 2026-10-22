// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use core::{cell::Cell, mem, ptr};
use tpde_base::ValLocalIdx;
use tpde_util::tpde_assert;
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};

pub use crate::assignment_guard::{
    with_assignment_allocator, Assignment, AssignmentAllocation, AssignmentAllocator,
};

pub const VALUE_ASSIGNMENT_SIZE: u32 = 16;
pub const FIRST_PART_OFFSET: u32 = 14;
pub const NUM_PARTS_INCLUDED: u32 = 1;
pub const NUM_FREE_LISTS: u32 = 29;
pub const ASSIGNMENT_SLAB_SIZE: usize = 16 * 1024;
pub const MAX_PART_COUNT_EXCLUSIVE: u32 = (u32::MAX - VALUE_ASSIGNMENT_SIZE) / 2;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AssignmentInit {
    pub frame_off: i32,
    pub references_left: u32,
    pub max_part_size: u8,
    pub pending_free: bool,
    pub variable_ref: bool,
    pub stack_variable: bool,
    pub delay_free: bool,
}

#[repr(C, align(8))]
pub struct ValueAssignment {
    frame: Cell<u32>,
    part_count: u32,
    references: Cell<u32>,
    max_part_size: Cell<u8>,
    flags: Cell<u8>,
    first_part: Cell<u16>,
}

impl ValueAssignment {
    pub(crate) fn new(part_count: u32, init: AssignmentInit) -> Self {
        Self {
            frame: Cell::new(init.frame_off as u32),
            part_count,
            references: Cell::new(init.references_left),
            max_part_size: Cell::new(init.max_part_size),
            flags: Cell::new(
                u8::from(init.pending_free)
                    | (u8::from(init.variable_ref) << 1)
                    | (u8::from(init.stack_variable) << 2)
                    | (u8::from(init.delay_free) << 3),
            ),
            first_part: Cell::new(0),
        }
    }

    pub fn frame_off(&self) -> i32 {
        self.frame.get() as i32
    }
    pub fn set_frame_off(&self, value: i32) {
        self.frame.set(value as u32);
    }
    pub fn var_ref_custom_idx(&self) -> u32 {
        self.frame.get()
    }
    pub fn set_var_ref_custom_idx(&self, value: u32) {
        self.frame.set(value);
    }
    pub fn part_count(&self) -> u32 {
        self.part_count
    }
    pub fn references_left(&self) -> u32 {
        self.references.get()
    }
    pub fn set_references_left(&self, value: u32) {
        self.references.set(value);
    }
    pub fn next_delayed_free_entry(&self) -> ValLocalIdx {
        ValLocalIdx::new(self.references.get())
    }
    pub fn set_next_delayed_free_entry(&self, value: ValLocalIdx) {
        self.references.set(value.get());
    }
    pub fn max_part_size(&self) -> u8 {
        self.max_part_size.get()
    }
    pub fn set_max_part_size(&self, value: u8) {
        self.max_part_size.set(value);
    }
    pub fn pending_free(&self) -> bool {
        self.flags.get() & 1 != 0
    }
    pub fn set_pending_free(&self, value: bool) {
        self.set_flag(1, value);
    }
    pub fn variable_ref(&self) -> bool {
        self.flags.get() & 2 != 0
    }
    pub fn set_variable_ref(&self, value: bool) {
        self.set_flag(2, value);
    }
    pub fn stack_variable(&self) -> bool {
        self.flags.get() & 4 != 0
    }
    pub fn set_stack_variable(&self, value: bool) {
        self.set_flag(4, value);
    }
    pub fn delay_free(&self) -> bool {
        self.flags.get() & 8 != 0
    }
    pub fn set_delay_free(&self, value: bool) {
        self.set_flag(8, value);
    }
    fn set_flag(&self, mask: u8, value: bool) {
        self.flags
            .set((self.flags.get() & !mask) | if value { mask } else { 0 });
    }
    pub fn size(&self) -> u32 {
        tpde_assert!(!self.variable_ref(), "variable-ref has no allocation size");
        self.part_count
            .wrapping_mul(u32::from(self.max_part_size.get()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssignmentAllocationInfo {
    pub size: u32,
    pub alloc_size: usize,
    pub free_list_idx: u32,
}

impl AssignmentAllocationInfo {
    pub fn new(part_count: u32) -> PgResult<Self> {
        if part_count >= MAX_PART_COUNT_EXCLUSIVE {
            return Err(invalid("assignment part count exceeds upstream limit"));
        }
        if part_count <= NUM_PARTS_INCLUDED {
            return Ok(Self {
                size: VALUE_ASSIGNMENT_SIZE,
                alloc_size: VALUE_ASSIGNMENT_SIZE as usize,
                free_list_idx: 0,
            });
        }
        let size = VALUE_ASSIGNMENT_SIZE + (part_count - NUM_PARTS_INCLUDED) * 2;
        let free_list_idx = 28 - (size - 1).leading_zeros();
        let alloc_size = 1usize
            .checked_shl(4 + free_list_idx)
            .ok_or_else(|| invalid("assignment allocation size exceeds address space"))?;
        Ok(Self {
            size,
            alloc_size,
            free_list_idx,
        })
    }
}

#[cold]
pub(crate) fn invalid(message: &'static str) -> alloc::boxed::Box<PgError> {
    PgError::error(message)
        .with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE)
        .into()
}

extern crate alloc;
mcx::forget_safe_nodrop!(ValueAssignment, AssignmentInit, AssignmentAllocationInfo);

const _: () = {
    assert!(mem::size_of::<ValueAssignment>() == VALUE_ASSIGNMENT_SIZE as usize);
    assert!(mem::align_of::<ValueAssignment>() == 8);
    assert!(mem::offset_of!(ValueAssignment, first_part) == FIRST_PART_OFFSET as usize);
    // 64-bit-host layout pin (TPDE is a native-only backend; wasm32 only check-builds it).
    #[cfg(target_pointer_width = "64")]
    assert!(mem::offset_of!(ValueAssignment, references) == 8);
    assert!(mem::size_of::<Cell<u16>>() == 2);
    assert!(!mem::needs_drop::<ValueAssignment>());
    // 64-bit-host layout pin (TPDE is a native-only backend; wasm32 only check-builds it).
    #[cfg(target_pointer_width = "64")]
    assert!(mem::size_of::<*mut ValueAssignment>() == 8);
};

pub(crate) unsafe fn write_next(pointer: *mut ValueAssignment, next: *mut ValueAssignment) {
    // SAFETY: caller consumes the exclusive live allocation before overlaying its last eight header bytes.
    unsafe {
        pointer
            .cast::<u8>()
            .add(8)
            .cast::<*mut ValueAssignment>()
            .write(next)
    };
}
pub(crate) unsafe fn read_next(pointer: *mut ValueAssignment) -> *mut ValueAssignment {
    // SAFETY: caller has unpoisoned this free node, whose link was initialized by write_next.
    unsafe { ptr::read(pointer.cast::<u8>().add(8).cast::<*mut ValueAssignment>()) }
}
