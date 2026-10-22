// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::value_assignment::{
    self, AssignmentAllocationInfo, AssignmentInit, ValueAssignment, ASSIGNMENT_SLAB_SIZE,
    FIRST_PART_OFFSET, NUM_FREE_LISTS, NUM_PARTS_INCLUDED, VALUE_ASSIGNMENT_SIZE,
};
use core::{
    cell::{Cell, UnsafeCell},
    marker::PhantomData,
    ops::Deref,
    ptr::{self, NonNull},
};
use mcx::Mcx;
use tpde_util::{asan, BumpAllocationInfo, BumpAllocator, BumpStats};
use types_error::PgResult;

#[doc = include_str!("../tests/value-assignment-ownership.md")]
pub struct AssignmentAllocator<'m, 'id> {
    alloc: BumpAllocator<'m, ASSIGNMENT_SLAB_SIZE>,
    free_lists: UnsafeCell<[*mut ValueAssignment; NUM_FREE_LISTS as usize]>,
    brand: PhantomData<fn(&'id ()) -> &'id ()>,
}

pub struct AssignmentAllocation<'a, 'id> {
    pointer: NonNull<ValueAssignment>,
    borrow: PhantomData<(&'a (), fn(&'id ()) -> &'id ())>,
    part_count: u32,
}

pub struct Assignment<'a, 'id> {
    pointer: NonNull<ValueAssignment>,
    borrow: PhantomData<(&'a (), fn(&'id ()) -> &'id ())>,
}

pub fn with_assignment_allocator<'m, R>(
    mcx: Mcx<'m>,
    run: impl for<'id> FnOnce(AssignmentAllocator<'m, 'id>) -> R,
) -> R {
    run(AssignmentAllocator::new(mcx))
}

impl<'m, 'id> AssignmentAllocator<'m, 'id> {
    fn new(mcx: Mcx<'m>) -> Self {
        Self {
            alloc: BumpAllocator::new(mcx),
            free_lists: UnsafeCell::new([ptr::null_mut(); NUM_FREE_LISTS as usize]),
            brand: PhantomData,
        }
    }

    pub fn allocate(&self, part_count: u32) -> PgResult<AssignmentAllocation<'_, 'id>> {
        if part_count > NUM_PARTS_INCLUDED {
            return self.allocate_slow(part_count, false);
        }
        let pointer = match self.pop_free(0, VALUE_ASSIGNMENT_SIZE as usize) {
            Some(pointer) => pointer,
            None => self.fresh(VALUE_ASSIGNMENT_SIZE as usize)?,
        };
        Ok(AssignmentAllocation {
            pointer,
            borrow: PhantomData,
            part_count,
        })
    }

    pub fn allocate_slow(
        &self,
        part_count: u32,
        skip_free_list: bool,
    ) -> PgResult<AssignmentAllocation<'_, 'id>> {
        let info = AssignmentAllocationInfo::new(part_count)?;
        let reused = if skip_free_list {
            None
        } else {
            self.pop_free(info.free_list_idx, info.size as usize)
        };
        let pointer = match reused {
            Some(pointer) => pointer,
            None => {
                let pointer = self.fresh(info.alloc_size)?;
                // SAFETY: exclusive fresh storage; upstream value-initializes only the fixed header here.
                unsafe {
                    pointer
                        .as_ptr()
                        .write(ValueAssignment::new(0, AssignmentInit::default()));
                }
                pointer
            }
        };
        Ok(AssignmentAllocation {
            pointer,
            borrow: PhantomData,
            part_count,
        })
    }

    fn fresh(&self, size: usize) -> PgResult<NonNull<ValueAssignment>> {
        let bytes = self.alloc.allocate(size, 8)?;
        Ok(NonNull::new(bytes.as_mut_ptr().cast()).unwrap())
    }

    fn pop_free(&self, index: u32, size: usize) -> Option<NonNull<ValueAssignment>> {
        // SAFETY: !Sync state, no user callbacks or overlapping borrow across this operation.
        let head = unsafe { &mut (*self.free_lists.get())[index as usize] };
        let pointer = NonNull::new(*head)?;
        // SAFETY: free-list membership proves ownership; restore the logical extent before reading its link.
        unsafe {
            asan::unpoison_memory_region(pointer.as_ptr().cast(), size);
            *head = value_assignment::read_next(pointer.as_ptr());
        }
        Some(pointer)
    }

    pub fn deallocate(&self, assignment: Assignment<'_, 'id>) {
        let part_count = assignment.part_count();
        if part_count > NUM_PARTS_INCLUDED {
            self.recycle(
                assignment.pointer,
                AssignmentAllocationInfo::new(part_count).unwrap(),
            );
        } else {
            self.recycle(
                assignment.pointer,
                AssignmentAllocationInfo {
                    size: VALUE_ASSIGNMENT_SIZE,
                    alloc_size: VALUE_ASSIGNMENT_SIZE as usize,
                    free_list_idx: 0,
                },
            );
        }
    }

    pub fn deallocate_slow(&self, assignment: Assignment<'_, 'id>) {
        let info = AssignmentAllocationInfo::new(assignment.part_count()).unwrap();
        self.recycle(assignment.pointer, info);
    }

    fn recycle(&self, pointer: NonNull<ValueAssignment>, info: AssignmentAllocationInfo) {
        // SAFETY: !Sync state and the consumed non-Clone token exclude all live views of this allocation.
        unsafe {
            let head = &mut (*self.free_lists.get())[info.free_list_idx as usize];
            value_assignment::write_next(pointer.as_ptr(), *head);
            *head = pointer.as_ptr();
            asan::poison_memory_region(pointer.as_ptr().cast(), info.size as usize);
        }
    }

    pub fn reset(&mut self) {
        self.alloc.reset();
        *self.free_lists.get_mut() = [ptr::null_mut(); NUM_FREE_LISTS as usize];
    }

    pub fn stats(&self) -> BumpStats {
        self.alloc.stats()
    }
    pub fn allocation_info(&self, pointer: *const ValueAssignment) -> Option<BumpAllocationInfo> {
        self.alloc.allocation_info(pointer.cast())
    }
}

impl<'a, 'id> AssignmentAllocation<'a, 'id> {
    pub fn as_ptr(&self) -> *const ValueAssignment {
        self.pointer.as_ptr()
    }
    pub fn part_count(&self) -> u32 {
        self.part_count
    }
    pub fn initialize(
        self,
        init: AssignmentInit,
        mut part: impl FnMut(u32) -> u16,
    ) -> Assignment<'a, 'id> {
        // SAFETY: the unique unpublished reservation owns a fixed header and enough trailing part storage.
        unsafe {
            self.pointer
                .as_ptr()
                .write(ValueAssignment::new(self.part_count, init));
        }
        for index in 0..self.part_count {
            let value = part(index);
            // SAFETY: each part is written once within the reserved logical extent before publishing views.
            unsafe {
                self.pointer
                    .as_ptr()
                    .cast::<u8>()
                    .add(FIRST_PART_OFFSET as usize)
                    .cast::<Cell<u16>>()
                    .add(index as usize)
                    .write(Cell::new(value));
            }
        }
        Assignment {
            pointer: self.pointer,
            borrow: PhantomData,
        }
    }
}

impl Deref for Assignment<'_, '_> {
    type Target = ValueAssignment;
    fn deref(&self) -> &ValueAssignment {
        // SAFETY: initialized header, with shared mutation confined to Cell fields; this borrow prevents free/reset.
        unsafe { self.pointer.as_ref() }
    }
}
impl Assignment<'_, '_> {
    pub fn as_ptr(&self) -> *const ValueAssignment {
        self.pointer.as_ptr()
    }
    pub const fn pointer(&self) -> NonNull<ValueAssignment> {
        self.pointer
    }
    pub fn parts(&self) -> &[Cell<u16>] {
        let count = self.part_count();
        // SAFETY: initialized trailing parts retain the original whole-allocation pointer provenance.
        unsafe {
            core::slice::from_raw_parts(
                self.pointer
                    .as_ptr()
                    .cast::<u8>()
                    .add(FIRST_PART_OFFSET as usize)
                    .cast(),
                count as usize,
            )
        }
    }
    pub fn part(&self, index: u32) -> &Cell<u16> {
        &self.parts()[index as usize]
    }
}

const _: () = {
    assert!(!core::mem::needs_drop::<Assignment<'_, '_>>());
    // 64-bit-host layout pin (TPDE is a native-only backend; wasm32 only check-builds it).
    #[cfg(target_pointer_width = "64")]
    assert!(core::mem::size_of::<Assignment<'_, '_>>() == 8);
    assert!(!core::mem::needs_drop::<AssignmentAllocation<'_, '_>>());
};

mcx::forget_safe_nodrop!(Assignment<'_, '_>, AssignmentAllocation<'_, '_>);

#[cfg(test)]
#[path = "../tests/value_assignment/unit.rs"]
mod tests;
