// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::storage::{small_vector_methods, Storage};
use core::{
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
};
use mcx::Mcx;
use types_error::PgResult;

#[doc = include_str!("../tests/small_vector_ownership.md")]
pub struct SmallVectorGuard<'m, T, const N: usize> {
    storage: Storage<'m, T, N>,
}
impl<'m, T, const N: usize> SmallVectorGuard<'m, T, N> {
    pub fn new(mcx: Mcx<'m>) -> Self {
        Self {
            storage: Storage::new(mcx),
        }
    }
    small_vector_methods!();
    pub fn move_assign<const M: usize>(
        &mut self,
        other: &mut SmallVectorGuard<'m, T, M>,
    ) -> PgResult<()> {
        self.storage.move_assign(&mut other.storage)
    }
    pub fn move_from<const M: usize>(
        mcx: Mcx<'m>,
        other: &mut SmallVectorGuard<'m, T, M>,
    ) -> PgResult<Self> {
        let mut value = Self::new(mcx);
        value.move_assign(other)?;
        Ok(value)
    }
}
impl<T, const N: usize> Deref for SmallVectorGuard<'_, T, N> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.storage.slice()
    }
}
impl<T, const N: usize> DerefMut for SmallVectorGuard<'_, T, N> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.storage.slice_mut()
    }
}
impl<T, const N: usize> Drop for SmallVectorGuard<'_, T, N> {
    fn drop(&mut self) {
        self.storage.release();
    }
}

pub struct UninitResizeGuard<'a, 'm, T: Copy, const N: usize> {
    storage: &'a mut Storage<'m, T, N>,
    old: usize,
    end: usize,
}
impl<'a, 'm, T: Copy, const N: usize> UninitResizeGuard<'a, 'm, T, N> {
    pub(crate) fn new(storage: &'a mut Storage<'m, T, N>, old: usize, end: usize) -> Self {
        Self { storage, old, end }
    }
    /// # Safety
    /// Every exposed slot must contain an initialized T before committing.
    pub unsafe fn assume_init(self) {
        // SAFETY: caller initialized the full reserved suffix; Drop restores the spare shadow state.
        unsafe {
            self.storage.set_len(self.end);
        }
    }
    /// # Safety
    /// For an in-range count, the first `initialized` slots must contain initialized values.
    pub unsafe fn assume_init_prefix(self, initialized: usize) -> PgResult<()> {
        crate::ensure(
            initialized <= self.end - self.old,
            "initialized prefix exceeds reservation",
        )?;
        // SAFETY: the caller initialized this prefix; Drop poisons the remaining spare slots.
        unsafe {
            self.storage.set_len(self.old + initialized);
        }
        Ok(())
    }
}
impl<T: Copy, const N: usize> Deref for UninitResizeGuard<'_, '_, T, N> {
    type Target = [MaybeUninit<T>];
    fn deref(&self) -> &Self::Target {
        // SAFETY: reserved suffix is unpoisoned, and MaybeUninit permits its unwritten bytes.
        unsafe {
            core::slice::from_raw_parts(
                self.storage.as_ptr().add(self.old).cast(),
                self.end - self.old,
            )
        }
    }
}
impl<T: Copy, const N: usize> DerefMut for UninitResizeGuard<'_, '_, T, N> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: exclusive borrow owns the entire reserved and unpoisoned suffix.
        unsafe {
            core::slice::from_raw_parts_mut(
                self.storage.as_mut_ptr().add(self.old).cast(),
                self.end - self.old,
            )
        }
    }
}
impl<T: Copy, const N: usize> Drop for UninitResizeGuard<'_, '_, T, N> {
    fn drop(&mut self) {
        self.storage.poison_spare();
    }
}

struct EraseGuard<'a, 'm, T, const N: usize> {
    storage: &'a mut Storage<'m, T, N>,
    start: usize,
    end: usize,
    len: usize,
}
impl<T, const N: usize> Drop for EraseGuard<'_, '_, T, N> {
    fn drop(&mut self) {
        // SAFETY: removed values are destroyed; overlapping copy transfers the untouched suffix even on unwind.
        unsafe {
            let pointer = self.storage.as_mut_ptr();
            core::ptr::copy(
                pointer.add(self.end),
                pointer.add(self.start),
                self.len - self.end,
            );
            self.storage.set_len(self.len - (self.end - self.start));
        }
        self.storage.poison_spare();
    }
}
pub(crate) fn erase<T, const N: usize>(
    storage: &mut Storage<'_, T, N>,
    range: core::ops::Range<usize>,
) {
    let len = storage.len();
    let guard = EraseGuard {
        storage,
        start: range.start,
        end: range.end,
        len,
    };
    // SAFETY: bounds were checked; the guard repairs length after slice destruction, including unwinding.
    unsafe {
        guard.storage.set_len(range.start);
        core::ptr::drop_in_place(core::ptr::slice_from_raw_parts_mut(
            guard.storage.as_mut_ptr().add(range.start),
            range.end - range.start,
        ));
    }
}
