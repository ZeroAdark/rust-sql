// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use core::{
    alloc::Layout,
    cell::UnsafeCell,
    marker::PhantomData,
    mem::{self, MaybeUninit},
    ops::{Deref, DerefMut},
    ptr::{self, NonNull},
};
use mcx::{Allocator, Mcx};
use types_error::PgResult;

use crate::{asan, ensure, invalid, SmallVec};

pub const BUMP_SLAB_ALIGNMENT: usize = 16;

struct State<'m> {
    current: Option<NonNull<u8>>,
    used: usize,
    end: usize,
    slabs: SmallVec<'m, NonNull<u8>, 32>,
    large: SmallVec<'m, (NonNull<u8>, usize), 0>,
}

#[doc = include_str!("../tests/bump-ownership.md")]
pub struct BumpAllocator<'m, const SLAB_SIZE: usize = 4096> {
    mcx: Mcx<'m>,
    state: UnsafeCell<State<'m>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BumpAllocationInfo {
    pub large: bool,
    pub index: usize,
    pub offset: usize,
    pub size: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BumpStats {
    pub regular_slabs: usize,
    pub large_slabs: usize,
    pub allocated_bytes: usize,
}

impl<'m, const SLAB_SIZE: usize> BumpAllocator<'m, SLAB_SIZE> {
    pub fn new(mcx: Mcx<'m>) -> Self {
        const {
            assert!(SLAB_SIZE > 0 && SLAB_SIZE <= isize::MAX as usize);
        }
        Self {
            mcx,
            state: UnsafeCell::new(State {
                current: None,
                used: 0,
                end: 0,
                slabs: SmallVec::new(mcx),
                large: SmallVec::new(mcx),
            }),
        }
    }

    pub fn slab_size(index: usize) -> PgResult<usize> {
        let multiplier = 1usize.checked_shl((index / 8).try_into().unwrap_or(u32::MAX));
        multiplier
            .and_then(|n| SLAB_SIZE.checked_mul(n))
            .filter(|&n| n <= isize::MAX as usize)
            .ok_or_else(|| invalid("bump slab size overflow"))
    }

    fn admit(size: usize, align: usize) -> PgResult<()> {
        ensure(
            size != 0 && size <= isize::MAX as usize,
            "invalid bump allocation size",
        )?;
        ensure(align.is_power_of_two(), "invalid bump allocation alignment")
    }

    fn allocate_pointer(
        &self,
        size: usize,
        align: usize,
        force_slab: bool,
    ) -> PgResult<NonNull<u8>> {
        Self::admit(size, align)?;
        // SAFETY: !Sync allocator; this borrow ends before construction or any user callback.
        let state = unsafe { &mut *self.state.get() };
        if !force_slab {
            if let Some(base) = state.current {
                let address = base
                    .as_ptr()
                    .addr()
                    .checked_add(state.used)
                    .and_then(|n| n.checked_add(align - 1))
                    .map(|n| n & !(align - 1));
                if let Some(offset) = address.and_then(|n| n.checked_sub(base.as_ptr().addr())) {
                    if let Some(end) = offset.checked_add(size).filter(|&n| n <= state.end) {
                        state.used = end;
                        // SAFETY: checked disjoint suffix of the current live slab, retaining provenance.
                        let pointer = unsafe { NonNull::new_unchecked(base.as_ptr().add(offset)) };
                        // SAFETY: only the newly reserved extent becomes accessible.
                        unsafe {
                            asan::unpoison_memory_region(pointer.as_ptr(), size);
                        }
                        return Ok(pointer);
                    }
                }
            }
        }
        ensure(
            align <= BUMP_SLAB_ALIGNMENT,
            "bump slab alignment unsupported",
        )?;
        let slab_size = Self::slab_size(state.slabs.len())?;
        let large = size > slab_size;
        let allocated = if large { size } else { slab_size };
        let layout = Layout::from_size_align(allocated, BUMP_SLAB_ALIGNMENT)
            .map_err(|_| invalid("invalid bump slab layout"))?;
        if large {
            state.large.reserve(
                state
                    .large
                    .len()
                    .checked_add(1)
                    .ok_or_else(|| invalid("large slab count overflow"))?,
            )?;
        } else {
            state.slabs.reserve(
                state
                    .slabs
                    .len()
                    .checked_add(1)
                    .ok_or_else(|| invalid("slab count overflow"))?,
            )?;
        }
        let pointer = self
            .mcx
            .alloc_uninit_bytes(layout)
            .map_err(|_| self.mcx.oom(allocated))?;
        if large {
            state.large.push((pointer, allocated))?;
        } else {
            state.slabs.push(pointer)?;
            state.current = Some(pointer);
            state.used = size;
            state.end = slab_size;
            // SAFETY: this fresh slab is inaccessible except for the returned prefix.
            unsafe {
                asan::poison_memory_region(pointer.as_ptr(), slab_size);
                asan::unpoison_memory_region(pointer.as_ptr(), size);
            }
        }
        Ok(pointer)
    }

    #[allow(clippy::mut_from_ref)]
    pub fn allocate(&self, size: usize, align: usize) -> PgResult<&mut [MaybeUninit<u8>]> {
        let pointer = self.allocate_pointer(size, align, false)?;
        // SAFETY: exclusive fresh extent; its borrow prevents reset or allocator destruction.
        Ok(unsafe { core::slice::from_raw_parts_mut(pointer.as_ptr().cast(), size) })
    }

    #[allow(clippy::mut_from_ref)]
    pub fn allocate_slab(&self, size: usize, align: usize) -> PgResult<&mut [MaybeUninit<u8>]> {
        let pointer = self.allocate_pointer(size, align, true)?;
        // SAFETY: exclusive fresh slab extent, tied to the allocator borrow.
        Ok(unsafe { core::slice::from_raw_parts_mut(pointer.as_ptr().cast(), size) })
    }

    fn typed_pointer<T>(&self, count: usize) -> PgResult<NonNull<T>> {
        let size = mem::size_of::<T>()
            .checked_mul(count)
            .ok_or_else(|| invalid("bump array size overflow"))?
            .max(1);
        let placement_align = size.min(BUMP_SLAB_ALIGNMENT);
        let align = if placement_align.is_power_of_two() {
            placement_align.max(mem::align_of::<T>())
        } else {
            mem::align_of::<T>()
        };
        self.allocate_pointer(size, align, false).map(NonNull::cast)
    }

    #[allow(clippy::mut_from_ref)]
    pub fn alloc<T>(&self, value: T) -> PgResult<&mut T> {
        const {
            assert!(!mem::needs_drop::<T>());
        }
        let pointer = self.typed_pointer::<T>(1)?;
        // SAFETY: typed_pointer satisfies T's layout and returns exclusive uninitialized storage.
        unsafe {
            pointer.as_ptr().write(value);
            Ok(&mut *pointer.as_ptr())
        }
    }

    pub fn new_with<T>(
        &self,
        construct: impl FnOnce() -> PgResult<T>,
    ) -> PgResult<BumpAllocUniquePtr<'_, T>> {
        let pointer = self.typed_pointer::<T>(1)?;
        let value = construct()?;
        // SAFETY: the reserved slot is exclusive and correctly aligned for T.
        unsafe {
            pointer.as_ptr().write(value);
        }
        Ok(BumpAllocUniquePtr {
            pointer,
            borrow: PhantomData,
        })
    }

    pub fn new_array_with<T>(
        &self,
        count: usize,
        mut construct: impl FnMut(usize) -> PgResult<T>,
    ) -> PgResult<BumpAllocArray<'_, T>> {
        let pointer = self.typed_pointer::<T>(count)?;
        let mut result = BumpAllocArray {
            pointer,
            len: 0,
            borrow: PhantomData,
        };
        for index in 0..count {
            let value = construct(index)?;
            // SAFETY: reserved array slot has not been initialized; the guard tracks its live prefix.
            unsafe {
                result.pointer.as_ptr().add(index).write(value);
            }
            result.len += 1;
        }
        Ok(result)
    }

    fn free(mcx: Mcx<'m>, pointer: NonNull<u8>, size: usize) {
        let layout = Layout::from_size_align(size, BUMP_SLAB_ALIGNMENT).unwrap();
        // SAFETY: exact allocation/layout, sole owner, restore shadow before allocator metadata access.
        unsafe {
            asan::unpoison_memory_region(pointer.as_ptr(), size);
            mcx.deallocate(pointer, layout);
        }
    }

    pub fn reset(&mut self) {
        let state = self.state.get_mut();
        for index in 1..state.slabs.len() {
            Self::free(
                self.mcx,
                state.slabs[index],
                Self::slab_size(index).unwrap(),
            );
        }
        state.slabs.truncate(1);
        if let Some(&first) = state.slabs.first() {
            state.current = Some(first);
            state.used = 0;
            state.end = SLAB_SIZE;
            // SAFETY: &mut self excludes all live allocation borrows.
            unsafe {
                asan::poison_memory_region(first.as_ptr(), SLAB_SIZE);
            }
        }
        for &(pointer, size) in state.large.iter() {
            Self::free(self.mcx, pointer, size);
        }
        state.large.clear();
    }

    pub fn allocation_info(&self, pointer: *const u8) -> Option<BumpAllocationInfo> {
        // SAFETY: no mutation/callback occurs during this inspection.
        let state = unsafe { &*self.state.get() };
        for (index, &base) in state.slabs.iter().enumerate() {
            let size = Self::slab_size(index).unwrap();
            if let Some(offset) = pointer
                .addr()
                .checked_sub(base.as_ptr().addr())
                .filter(|&n| n < size)
            {
                return Some(BumpAllocationInfo {
                    large: false,
                    index,
                    offset,
                    size,
                });
            }
        }
        for (index, &(base, size)) in state.large.iter().enumerate() {
            if let Some(offset) = pointer
                .addr()
                .checked_sub(base.as_ptr().addr())
                .filter(|&n| n < size)
            {
                return Some(BumpAllocationInfo {
                    large: true,
                    index,
                    offset,
                    size,
                });
            }
        }
        None
    }

    pub fn stats(&self) -> BumpStats {
        // SAFETY: no mutation/callback occurs during this inspection.
        let state = unsafe { &*self.state.get() };
        BumpStats {
            regular_slabs: state.slabs.len(),
            large_slabs: state.large.len(),
            allocated_bytes: (0..state.slabs.len())
                .map(|i| Self::slab_size(i).unwrap())
                .sum::<usize>()
                + state.large.iter().map(|&(_, size)| size).sum::<usize>(),
        }
    }
}

impl<const SLAB_SIZE: usize> Drop for BumpAllocator<'_, SLAB_SIZE> {
    fn drop(&mut self) {
        let state = self.state.get_mut();
        for (index, &pointer) in state.slabs.iter().enumerate() {
            Self::free(self.mcx, pointer, Self::slab_size(index).unwrap());
        }
        for &(pointer, size) in state.large.iter() {
            Self::free(self.mcx, pointer, size);
        }
        state.slabs.release();
        state.large.release();
    }
}

pub struct BumpAllocUniquePtr<'a, T> {
    pointer: NonNull<T>,
    borrow: PhantomData<&'a mut T>,
}
impl<T> BumpAllocUniquePtr<'_, T> {
    pub fn as_ptr(&self) -> *const T {
        self.pointer.as_ptr()
    }
    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.pointer.as_ptr()
    }
    pub fn into_inner(self) -> T {
        let this = mem::ManuallyDrop::new(self);
        // SAFETY: consuming the unique guard transfers its initialized value without dropping it.
        unsafe { this.pointer.as_ptr().read() }
    }
}
impl<T> Deref for BumpAllocUniquePtr<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: live initialized value; this guard has sole destruction authority.
        unsafe { self.pointer.as_ref() }
    }
}
impl<T> DerefMut for BumpAllocUniquePtr<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: exclusive guard borrow over its initialized value.
        unsafe { self.pointer.as_mut() }
    }
}
impl<T> Drop for BumpAllocUniquePtr<'_, T> {
    fn drop(&mut self) {
        // SAFETY: sole destruction authority; allocator borrow keeps the bytes live.
        unsafe {
            ptr::drop_in_place(self.pointer.as_ptr());
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct BumpAllocatorDeleter;
impl BumpAllocatorDeleter {
    pub fn destroy<T>(&self, pointer: BumpAllocUniquePtr<'_, T>) {
        drop(pointer);
    }
}

pub struct BumpAllocArray<'a, T> {
    pointer: NonNull<T>,
    len: usize,
    borrow: PhantomData<&'a mut T>,
}
impl<T> Deref for BumpAllocArray<'_, T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        // SAFETY: the guard tracks exactly the initialized prefix in its reserved array.
        unsafe { core::slice::from_raw_parts(self.pointer.as_ptr(), self.len) }
    }
}
impl<T> DerefMut for BumpAllocArray<'_, T> {
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: exclusive borrow of exactly the guard's initialized prefix.
        unsafe { core::slice::from_raw_parts_mut(self.pointer.as_ptr(), self.len) }
    }
}
impl<T> BumpAllocArray<'_, T> {
    fn destroy_remaining(&mut self) {
        while self.len != 0 {
            self.len -= 1;
            // SAFETY: remove each initialized element from destruction authority before dropping.
            let pointer = unsafe { self.pointer.as_ptr().add(self.len) };
            let continuation = ArrayUnwindGuard(self);
            // SAFETY: the removed element is live; continuation destroys the remaining prefix on unwind.
            unsafe {
                ptr::drop_in_place(pointer);
            }
            mem::forget(continuation);
        }
    }
}

impl<T> Drop for BumpAllocArray<'_, T> {
    fn drop(&mut self) {
        self.destroy_remaining();
    }
}

struct ArrayUnwindGuard<'g, 'a, T>(&'g mut BumpAllocArray<'a, T>);
impl<T> Drop for ArrayUnwindGuard<'_, '_, T> {
    fn drop(&mut self) {
        self.0.destroy_remaining();
    }
}

#[cfg(test)]
#[path = "../tests/bump/unit.rs"]
mod tests;
