// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::asan::{poison_memory_region, unpoison_memory_region};
use core::{
    mem::{ManuallyDrop, MaybeUninit},
    ops::{Deref, DerefMut, Range},
};
use mcx::{ForgetSafe, Mcx, PgVec};
use types_error::PgResult;

pub const fn default_inline_capacity<T>() -> usize {
    assert!(core::mem::size_of::<T>() != 0);
    if core::mem::size_of::<T>() < 256 {
        256 / core::mem::size_of::<T>()
    } else {
        1
    }
}

pub(crate) struct Storage<'m, T, const N: usize> {
    inline: [MaybeUninit<T>; N],
    inline_len: usize,
    inline_cap: usize,
    spill: ManuallyDrop<PgVec<'m, T>>,
}
impl<'m, T, const N: usize> Storage<'m, T, N> {
    pub(crate) fn new(mcx: Mcx<'m>) -> Self {
        const {
            assert!(core::mem::size_of::<T>() != 0);
        }
        Self {
            inline: [const { MaybeUninit::uninit() }; N],
            inline_len: 0,
            inline_cap: N,
            spill: ManuallyDrop::new(PgVec::new_in(mcx)),
        }
    }
    fn spilled(&self) -> bool {
        self.spill.capacity() != 0
    }
    pub(crate) fn len(&self) -> usize {
        if self.spilled() {
            self.spill.len()
        } else {
            self.inline_len
        }
    }
    pub(crate) fn capacity(&self) -> usize {
        if self.spilled() {
            self.spill.capacity()
        } else {
            self.inline_cap
        }
    }
    pub(crate) fn as_ptr(&self) -> *const T {
        if self.spilled() {
            self.spill.as_ptr()
        } else {
            self.inline.as_ptr().cast()
        }
    }
    pub(crate) fn as_mut_ptr(&mut self) -> *mut T {
        if self.spilled() {
            self.spill.as_mut_ptr()
        } else {
            self.inline.as_mut_ptr().cast()
        }
    }
    /// `(pointer, len, capacity)` resolved with one inline/spill branch, for hot
    /// paths that would otherwise re-resolve the storage kind per accessor.
    #[inline(always)]
    pub(crate) fn raw_parts_mut(&mut self) -> (*mut T, usize, usize) {
        if self.spilled() {
            (
                self.spill.as_mut_ptr(),
                self.spill.len(),
                self.spill.capacity(),
            )
        } else {
            (
                self.inline.as_mut_ptr().cast(),
                self.inline_len,
                self.inline_cap,
            )
        }
    }
    /// `(pointer, len)` of the live prefix with one inline/spill branch.
    #[inline(always)]
    pub(crate) fn raw_parts(&self) -> (*const T, usize) {
        if self.spilled() {
            (self.spill.as_ptr(), self.spill.len())
        } else {
            (self.inline.as_ptr().cast(), self.inline_len)
        }
    }
    #[inline(always)]
    pub(crate) fn slice(&self) -> &[T] {
        let (pointer, len) = self.raw_parts();
        // SAFETY: storage exposes only its initialized live prefix.
        unsafe { core::slice::from_raw_parts(pointer, len) }
    }
    #[inline(always)]
    pub(crate) fn slice_mut(&mut self) -> &mut [T] {
        let (pointer, len, _) = self.raw_parts_mut();
        // SAFETY: exclusive receiver owns the initialized live prefix.
        unsafe { core::slice::from_raw_parts_mut(pointer, len) }
    }
    #[inline(always)]
    pub(crate) unsafe fn set_len(&mut self, len: usize) {
        if self.spilled() {
            // SAFETY: caller guarantees len initialized elements within capacity.
            unsafe {
                self.spill.set_len(len);
            }
        } else {
            self.inline_len = len;
        }
    }
    #[inline(always)]
    fn unpoison(&mut self, start: usize, end: usize) {
        if self.spilled() {
            // SAFETY: only this owner changes shadow state within its heap allocation.
            unsafe {
                unpoison_memory_region(
                    self.as_ptr().add(start).cast(),
                    (end - start) * core::mem::size_of::<T>(),
                );
            }
        }
    }
    pub(crate) fn poison_spare(&mut self) {
        if self.spilled() {
            // SAFETY: unused heap suffix is inaccessible through initialized slices; inline bytes stay movable.
            unsafe {
                poison_memory_region(
                    self.as_ptr().add(self.len()).cast(),
                    (self.capacity() - self.len()) * core::mem::size_of::<T>(),
                );
            }
        }
    }
    pub(crate) fn reserve(&mut self, end: usize) -> PgResult<()> {
        if end <= self.capacity() {
            return Ok(());
        }
        let mcx = *self.spill.allocator();
        let cap = self
            .capacity()
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .map(|n| n.max(end))
            .filter(|&n| n <= isize::MAX as usize / core::mem::size_of::<T>())
            .ok_or_else(|| mcx.oom(end.saturating_mul(core::mem::size_of::<T>())))?;
        self.unpoison(0, self.capacity());
        let additional = cap - self.spill.len();
        let result = self.spill.try_reserve_exact(additional);
        if result.is_err() {
            self.poison_spare();
            return Err(mcx
                .oom(cap.saturating_mul(core::mem::size_of::<T>()))
                .into());
        }
        if self.inline_len != 0 {
            // SAFETY: reserved destination is disjoint; bitwise move transfers ownership and retires inline length.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.inline.as_ptr().cast::<T>(),
                    self.spill.as_mut_ptr(),
                    self.inline_len,
                );
                self.spill.set_len(self.inline_len);
            }
            self.inline_len = 0;
        }
        self.poison_spare();
        Ok(())
    }
    #[inline]
    pub(crate) fn push(&mut self, value: T) -> PgResult<()> {
        let (_, len, capacity) = self.raw_parts_mut();
        let end = len
            .checked_add(1)
            .ok_or_else(|| crate::invalid("vector length overflow"))?;
        if end > capacity {
            self.reserve(end)?;
        }
        self.unpoison(len, end);
        // The store and the length publication resolve the storage kind once,
        // after reserve/unpoison, so the pointer is the newest borrow of the
        // (possibly reallocated) storage (upstream `push_back`: one store, one
        // size increment).
        // SAFETY: reserved slot is uninitialized, and value ownership is transferred before length publication.
        unsafe {
            if self.spilled() {
                self.spill.as_mut_ptr().add(len).write(value);
                self.spill.set_len(end);
            } else {
                self.inline.as_mut_ptr().cast::<T>().add(len).write(value);
                self.inline_len = end;
            }
        }
        Ok(())
    }
    pub(crate) fn emplace_with(&mut self, make: impl FnOnce() -> T) -> PgResult<&mut T> {
        let end = self
            .len()
            .checked_add(1)
            .ok_or_else(|| crate::invalid("vector length overflow"))?;
        self.reserve(end)?;
        let value = make();
        self.push(value)?;
        Ok(&mut self.slice_mut()[end - 1])
    }
    pub(crate) fn pop(&mut self) -> Option<T> {
        let len = self.len().checked_sub(1)?;
        // SAFETY: last live value is moved out exactly once and the retired slot becomes inaccessible.
        let result = unsafe {
            let value = self.as_ptr().add(len).read();
            self.set_len(len);
            value
        };
        self.poison_spare();
        Some(result)
    }
    pub(crate) fn truncate(&mut self, len: usize) {
        let old = self.len();
        if len >= old {
            return;
        }
        // SAFETY: retire the suffix before destruction, so unwinding never double-drops; slice glue finishes cleanup.
        unsafe {
            self.set_len(len);
            core::ptr::drop_in_place(core::ptr::slice_from_raw_parts_mut(
                self.as_mut_ptr().add(len),
                old - len,
            ));
        }
        self.poison_spare();
    }
    pub(crate) fn release(&mut self) {
        let mcx = *self.spill.allocator();
        self.inline_cap = N;
        if self.spilled() {
            self.unpoison(0, self.capacity());
            let old = core::mem::replace(&mut self.spill, ManuallyDrop::new(PgVec::new_in(mcx)));
            drop(ManuallyDrop::into_inner(old));
        } else {
            let len = core::mem::replace(&mut self.inline_len, 0);
            // SAFETY: length is retired first; slice drop cleans remaining elements if one destructor unwinds.
            unsafe {
                core::ptr::drop_in_place(core::ptr::slice_from_raw_parts_mut(
                    self.inline.as_mut_ptr().cast::<T>(),
                    len,
                ));
            }
        }
    }
    pub(crate) fn resize_with(&mut self, len: usize, mut make: impl FnMut() -> T) -> PgResult<()> {
        let old = self.len();
        if len <= old {
            self.truncate(len);
            return Ok(());
        }
        self.reserve(len)?;
        self.unpoison(old, len);
        // The storage kind is resolved once for the whole extension (upstream
        // `resize` fills the reserved tail in place); each value is published
        // as soon as it is written, so a panicking `make` leaves exactly the
        // constructed prefix live (the small_vector unwind contract).
        // SAFETY: every slot is reserved and uninitialized until written; the
        // pointer is re-derived after each `make` (which cannot touch this
        // storage) and the length only ever covers written slots.
        unsafe {
            if self.spilled() {
                for index in old..len {
                    let value = make();
                    self.spill.as_mut_ptr().add(index).write(value);
                    self.spill.set_len(index + 1);
                }
            } else {
                for index in old..len {
                    let value = make();
                    self.inline.as_mut_ptr().cast::<T>().add(index).write(value);
                    self.inline_len = index + 1;
                }
            }
        }
        Ok(())
    }
    pub(crate) fn extend_copy(&mut self, values: &[T]) -> PgResult<()>
    where
        T: Copy,
    {
        let old = self.len();
        let end = old
            .checked_add(values.len())
            .ok_or_else(|| crate::invalid("vector length overflow"))?;
        self.reserve(end)?;
        self.unpoison(old, end);
        // SAFETY: disjoint initialized Copy source and reserved destination; publication follows one memcpy.
        unsafe {
            core::ptr::copy_nonoverlapping(
                values.as_ptr(),
                self.as_mut_ptr().add(old),
                values.len(),
            );
            self.set_len(end);
        }
        Ok(())
    }
    pub(crate) fn append(&mut self, values: impl ExactSizeIterator<Item = T>) -> PgResult<()> {
        let end = self
            .len()
            .checked_add(values.len())
            .ok_or_else(|| crate::invalid("vector length overflow"))?;
        self.reserve(end)?;
        for value in values {
            self.push(value)?;
        }
        Ok(())
    }
    pub(crate) fn erase(&mut self, range: Range<usize>) -> PgResult<usize> {
        crate::ensure(
            range.start <= range.end && range.end <= self.len(),
            "invalid vector erase range",
        )?;
        let start = range.start;
        if start != range.end {
            crate::small_vector_guard::erase(self, range);
        }
        Ok(start)
    }
    pub(crate) fn move_assign<const M: usize>(
        &mut self,
        other: &mut Storage<'m, T, M>,
    ) -> PgResult<()> {
        crate::ensure(
            core::ptr::eq(
                self.spill.allocator().context(),
                other.spill.allocator().context(),
            ),
            "vector move requires the same memory context",
        )?;
        if other.spilled() {
            self.release();
            let mcx = *other.spill.allocator();
            let replacement = ManuallyDrop::new(PgVec::new_in(mcx));
            self.spill = core::mem::replace(&mut other.spill, replacement);
            other.inline_cap = 0;
            other.inline_len = 0;
        } else {
            self.reserve(other.len())?;
            self.truncate(0);
            let len = other.len();
            self.unpoison(0, len);
            // SAFETY: separate owners, sufficient capacity; source live prefix is retired after its bitwise move.
            unsafe {
                core::ptr::copy_nonoverlapping(other.as_ptr(), self.as_mut_ptr(), len);
                self.set_len(len);
                other.set_len(0);
            }
        }
        self.poison_spare();
        Ok(())
    }
    pub(crate) fn prepare_uninit(&mut self, len: usize) -> PgResult<usize> {
        self.reserve(len)?;
        self.truncate(len);
        let old = self.len();
        self.unpoison(old, len);
        Ok(old)
    }
}

macro_rules! small_vector_methods {
    () => {
        pub fn capacity(&self) -> usize {
            self.storage.capacity()
        }
        pub fn len(&self) -> usize {
            self.storage.len()
        }
        pub fn is_empty(&self) -> bool {
            self.len() == 0
        }
        pub fn max_size() -> usize {
            usize::MAX / core::mem::size_of::<T>()
        }
        pub fn as_ptr(&self) -> *const T {
            self.storage.as_ptr()
        }
        pub fn as_mut_ptr(&mut self) -> *mut T {
            self.storage.as_mut_ptr()
        }
        pub fn reserve(&mut self, end: usize) -> types_error::PgResult<()> {
            self.storage.reserve(end)
        }
        pub fn push(&mut self, value: T) -> types_error::PgResult<()> {
            self.storage.push(value)
        }
        pub fn push_clone(&mut self, value: &T) -> types_error::PgResult<()>
        where
            T: Clone,
        {
            self.storage.emplace_with(|| value.clone()).map(|_| ())
        }
        pub fn emplace_with(&mut self, make: impl FnOnce() -> T) -> types_error::PgResult<&mut T> {
            self.storage.emplace_with(make)
        }
        pub fn pop(&mut self) -> Option<T> {
            self.storage.pop()
        }
        pub fn pop_back(&mut self) -> types_error::PgResult<()> {
            crate::ensure(!self.is_empty(), "pop from empty vector")?;
            self.truncate(self.len() - 1);
            Ok(())
        }
        pub fn truncate(&mut self, len: usize) {
            self.storage.truncate(len);
        }
        pub fn clear(&mut self) {
            self.truncate(0);
        }
        pub fn release(&mut self) {
            self.storage.release();
        }
        pub fn resize(&mut self, len: usize, value: T) -> types_error::PgResult<()>
        where
            T: Clone,
        {
            self.storage.resize_with(len, || value.clone())
        }
        pub fn resize_default(&mut self, len: usize) -> types_error::PgResult<()>
        where
            T: Default,
        {
            self.storage.resize_with(len, T::default)
        }
        pub fn resize_with(
            &mut self,
            len: usize,
            make: impl FnMut() -> T,
        ) -> types_error::PgResult<()> {
            self.storage.resize_with(len, make)
        }
        pub fn extend_from_slice(&mut self, values: &[T]) -> types_error::PgResult<()>
        where
            T: Copy,
        {
            self.storage.extend_copy(values)
        }
        pub fn append(
            &mut self,
            values: impl ExactSizeIterator<Item = T>,
        ) -> types_error::PgResult<()> {
            self.storage.append(values)
        }
        pub fn append_cloned<'a>(
            &mut self,
            values: impl ExactSizeIterator<Item = &'a T>,
        ) -> types_error::PgResult<()>
        where
            T: Clone + 'a,
        {
            let end = self
                .len()
                .checked_add(values.len())
                .ok_or_else(|| crate::invalid("vector length overflow"))?;
            self.reserve(end)?;
            for value in values {
                self.push_clone(value)?;
            }
            Ok(())
        }
        pub fn append_forward_with<I, F>(
            &mut self,
            cursor: I,
            mut make: F,
        ) -> types_error::PgResult<()>
        where
            I: Iterator + Clone,
            I::Item: Copy,
            F: FnMut(I::Item) -> T,
        {
            let count = cursor.clone().count();
            let end = self
                .len()
                .checked_add(count)
                .ok_or_else(|| crate::invalid("vector length overflow"))?;
            self.reserve(end)?;
            for position in cursor {
                self.push(make(position))?;
            }
            Ok(())
        }
        pub fn append_forward(
            &mut self,
            values: impl Iterator<Item = T> + Clone,
        ) -> types_error::PgResult<()>
        where
            T: Copy,
        {
            self.append_forward_with(values, |value| value)
        }
        pub fn append_cloned_forward<'a>(
            &mut self,
            values: impl Iterator<Item = &'a T> + Clone,
        ) -> types_error::PgResult<()>
        where
            T: Clone + 'a,
        {
            self.append_forward_with(values, T::clone)
        }
        pub fn erase(&mut self, range: core::ops::Range<usize>) -> types_error::PgResult<usize> {
            self.storage.erase(range)
        }
        pub fn resize_uninitialized(
            &mut self,
            len: usize,
        ) -> types_error::PgResult<crate::small_vector_guard::UninitResizeGuard<'_, 'm, T, N>>
        where
            T: Copy,
        {
            let old = self.storage.prepare_uninit(len)?;
            Ok(crate::small_vector_guard::UninitResizeGuard::new(
                &mut self.storage,
                old,
                len,
            ))
        }
    };
}
pub(crate) use small_vector_methods;

pub struct SmallVec<'m, T: ForgetSafe, const N: usize> {
    storage: Storage<'m, T, N>,
}
impl<'m, T: ForgetSafe, const N: usize> SmallVec<'m, T, N> {
    pub fn new(mcx: Mcx<'m>) -> Self {
        const {
            assert!(!core::mem::needs_drop::<T>());
        }
        Self {
            storage: Storage::new(mcx),
        }
    }
    small_vector_methods!();
    pub fn move_assign<const M: usize>(&mut self, other: &mut SmallVec<'m, T, M>) -> PgResult<()> {
        self.storage.move_assign(&mut other.storage)
    }
    pub fn move_from<const M: usize>(
        mcx: Mcx<'m>,
        other: &mut SmallVec<'m, T, M>,
    ) -> PgResult<Self> {
        let mut value = Self::new(mcx);
        value.move_assign(other)?;
        Ok(value)
    }
}
impl<const N: usize> SmallVec<'_, u8, N> {
    /// Copies `bytes` to `offset`, which must lie inside the live prefix, and
    /// grows the live length when the copy ends beyond it. The destination
    /// must already be reserved: nothing is allocated here.
    #[inline(always)]
    pub fn write_at(&mut self, offset: usize, bytes: &[u8]) -> PgResult<()> {
        let (_, len, capacity) = self.storage.raw_parts_mut();
        let end = offset
            .checked_add(bytes.len())
            .ok_or_else(|| crate::invalid("writer size overflow"))?;
        crate::ensure(
            offset <= len && end <= capacity,
            "write outside reserved storage",
        )?;
        if end > len {
            self.storage.unpoison(len, end);
        }
        // Taken after the unpoison retag so the raw pointer is the newest borrow
        // (Stacked Borrows); without ASan the two raw_parts_mut resolve to one branch.
        let (pointer, _, _) = self.storage.raw_parts_mut();
        // SAFETY: offset <= len <= capacity and end <= capacity keep the copy inside the
        // owned allocation; `bytes` cannot alias the exclusively borrowed storage; every
        // byte up to the published length is written before set_len.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.add(offset), bytes.len());
            if end > len {
                self.storage.set_len(end);
            }
        }
        Ok(())
    }
    /// Zeroes `size` bytes at `offset` under the same contract as `write_at`.
    #[inline(always)]
    pub fn zero_at(&mut self, offset: usize, size: usize) -> PgResult<()> {
        let (_, len, capacity) = self.storage.raw_parts_mut();
        let end = offset
            .checked_add(size)
            .ok_or_else(|| crate::invalid("writer size overflow"))?;
        crate::ensure(
            offset <= len && end <= capacity,
            "write outside reserved storage",
        )?;
        if end > len {
            self.storage.unpoison(len, end);
        }
        // Taken after the unpoison retag so the raw pointer is the newest borrow
        // (Stacked Borrows); without ASan the two raw_parts_mut resolve to one branch.
        let (pointer, _, _) = self.storage.raw_parts_mut();
        // SAFETY: as in write_at; the zeroed range is inside the owned allocation and
        // fully initialized before the larger length is published.
        unsafe {
            core::ptr::write_bytes(pointer.add(offset), 0, size);
            if end > len {
                self.storage.set_len(end);
            }
        }
        Ok(())
    }
    pub fn resize_bytes(&mut self, end: usize) -> PgResult<()> {
        let old = self.storage.prepare_uninit(end)?;
        // SAFETY: every new byte is initialized before the larger length is published.
        unsafe {
            core::ptr::write_bytes(self.storage.as_mut_ptr().add(old), 0, end - old);
            self.storage.set_len(end);
        }
        Ok(())
    }
}
impl<T: ForgetSafe, const N: usize> Deref for SmallVec<'_, T, N> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.storage.slice()
    }
}
impl<T: ForgetSafe, const N: usize> DerefMut for SmallVec<'_, T, N> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.storage.slice_mut()
    }
}
