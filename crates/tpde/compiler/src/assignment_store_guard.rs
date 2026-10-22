// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use core::mem::{align_of, size_of, MaybeUninit};
use mcx::Mcx;
use tpde_core::{
    analyzer::SMALL_VALUE_NUM,
    value_assignment::{Assignment, AssignmentAllocator},
    AssignmentPartRef,
};
use tpde_util::{tpde_assert, SmallVec};
use types_error::PgResult;

struct ErasedSlot {
    token: MaybeUninit<Option<Assignment<'static, 'static>>>,
}

impl ErasedSlot {
    const fn empty() -> Self {
        Self {
            token: MaybeUninit::new(None),
        }
    }
}

#[doc = include_str!("../tests/assignment-store-ownership.md")]
pub struct AssignmentSlotStorage<'m> {
    slots: SmallVec<'m, ErasedSlot, SMALL_VALUE_NUM>,
    active_len: usize,
    occupied: usize,
}

pub struct AssignmentSlots<'s, 'a, 'm, 'id> {
    slots: &'s mut [ErasedSlot],
    occupied: &'s mut usize,
    allocator: &'a AssignmentAllocator<'m, 'id>,
}

impl<'m> AssignmentSlotStorage<'m> {
    pub fn new(mcx: Mcx<'m>) -> Self {
        Self {
            slots: SmallVec::new(mcx),
            active_len: 0,
            occupied: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.slots.capacity()
    }

    pub fn with_slots<'a, 'id, R>(
        &mut self,
        allocator: &'a AssignmentAllocator<'m, 'id>,
        count: usize,
        operation: impl for<'s> FnOnce(AssignmentSlots<'s, 'a, 'm, 'id>) -> R,
    ) -> PgResult<R> {
        self.retire();
        if self.slots.len() < count {
            self.slots.resize_with(count, ErasedSlot::empty)?;
        }
        self.active_len = count;
        let result = operation(AssignmentSlots {
            slots: &mut self.slots[..count],
            occupied: &mut self.occupied,
            allocator,
        });
        self.retire();
        Ok(result)
    }

    pub fn release(&mut self) {
        self.retire();
        self.slots.release();
    }

    fn retire(&mut self) {
        if self.occupied != 0 {
            for slot in &mut self.slots[..self.active_len] {
                slot.token = MaybeUninit::new(None);
            }
            self.occupied = 0;
        }
        self.active_len = 0;
    }
}

impl<'a, 'm, 'id> AssignmentSlots<'_, 'a, 'm, 'id> {
    pub fn allocator(&self) -> &'a AssignmentAllocator<'m, 'id> {
        self.allocator
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn occupied(&self) -> usize {
        *self.occupied
    }

    #[inline]
    pub fn get(&self, index: usize) -> Option<&Assignment<'a, 'id>> {
        tpde_assert!(index < self.len(), "assignment slot index out of bounds");
        let slot = &self.slots[index];
        // SAFETY: slots contain None or this loan's token type; the outer borrow prevents removal and allocator reset.
        unsafe { (&*slot.token.as_ptr().cast::<Option<Assignment<'a, 'id>>>()).as_ref() }
    }

    #[inline]
    pub fn part(&self, index: usize, part: u32) -> Option<AssignmentPartRef<'_, 'a, 'id>> {
        self.get(index)
            .map(|assignment| AssignmentPartRef::new(assignment, part))
    }

    pub fn insert(&mut self, index: usize, assignment: Assignment<'a, 'id>) {
        tpde_assert!(index < self.len(), "assignment slot index out of bounds");
        let slot = &mut self.slots[index];
        // SAFETY: only this loan accesses the initialized Option; token lifetimes do not change its layout.
        unsafe {
            let token = &mut *slot
                .token
                .as_mut_ptr()
                .cast::<Option<Assignment<'a, 'id>>>();
            tpde_assert!(token.is_none(), "assignment slot already occupied");
            *token = Some(assignment);
        }
        *self.occupied += 1;
    }

    pub fn take(&mut self, index: usize) -> Option<Assignment<'a, 'id>> {
        tpde_assert!(index < self.len(), "assignment slot index out of bounds");
        let slot = &mut self.slots[index];
        // SAFETY: exclusive access excludes all views; taking the current loan's Option transfers its token exactly once.
        let assignment = unsafe {
            (&mut *slot
                .token
                .as_mut_ptr()
                .cast::<Option<Assignment<'a, 'id>>>())
                .take()
        };
        *self.occupied -= usize::from(assignment.is_some());
        assignment
    }

    pub fn move_slot(&mut self, source: usize, destination: usize) {
        tpde_assert!(source < self.len(), "assignment source index out of bounds");
        tpde_assert!(
            destination < self.len(),
            "assignment destination index out of bounds"
        );
        tpde_assert!(self.get(source).is_some(), "assignment source is vacant");
        if source == destination {
            return;
        }
        tpde_assert!(
            self.get(destination).is_none(),
            "assignment destination is occupied"
        );
        let assignment = self.take(source).unwrap();
        self.insert(destination, assignment);
    }

    pub fn discard(&mut self, index: usize) {
        let _ = self.take(index);
    }

    pub fn clear(&mut self) {
        if *self.occupied != 0 {
            for slot in self.slots.iter_mut() {
                slot.token = MaybeUninit::new(None);
            }
            *self.occupied = 0;
        }
    }
}

impl Drop for AssignmentSlots<'_, '_, '_, '_> {
    fn drop(&mut self) {
        self.clear();
    }
}

mcx::forget_safe_nodrop!(ErasedSlot, AssignmentSlotStorage<'_>);

const _: () = {
    assert!(size_of::<Assignment<'static, 'static>>() == size_of::<usize>());
    assert!(align_of::<Assignment<'static, 'static>>() == align_of::<usize>());
    assert!(size_of::<ErasedSlot>() == size_of::<usize>());
    assert!(align_of::<ErasedSlot>() == align_of::<usize>());
    assert!(!core::mem::needs_drop::<ErasedSlot>());
};

#[cfg(test)]
#[path = "../tests/assignment_store.rs"]
mod tests;
