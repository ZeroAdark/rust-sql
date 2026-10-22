// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use core::{
    alloc::Layout,
    cell::{Cell, RefCell},
    marker::PhantomData,
    ops::{Index, IndexMut},
    ptr::{self, NonNull},
};
use mcx::{Allocator, Mcx};
use types_error::PgResult;

use crate::{asan, invalid, SmallVec};

#[doc = include_str!("../tests/segmented-ownership.md")]
pub struct SegmentedVectorGuard<'m, T, const SEGMENT_SIZE: usize = 64> {
    allocations: RefCell<Allocations<'m, T, SEGMENT_SIZE>>,
    cur: Cell<*mut T>,
    available: Cell<usize>,
    len: Cell<usize>,
}

struct Allocations<'m, T, const N: usize> {
    segments: SmallVec<'m, *mut T, 32>,
    mcx: Mcx<'m>,
    owns: PhantomData<T>,
}

impl<'m, T, const N: usize> SegmentedVectorGuard<'m, T, N> {
    pub fn new(mcx: Mcx<'m>) -> Self {
        const {
            assert!(N != 0, "a segment must contain at least one element");
        }
        Self {
            allocations: RefCell::new(Allocations {
                segments: SmallVec::new(mcx),
                mcx,
                owns: PhantomData,
            }),
            cur: Cell::new(ptr::null_mut()),
            available: Cell::new(0),
            len: Cell::new(0),
        }
    }

    pub fn len(&self) -> usize {
        self.len.get()
    }
    pub fn size(&self) -> usize {
        self.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        if index >= self.len() {
            return None;
        }
        let pointer = self.allocations.borrow().segments[index / N];
        // SAFETY: the initialized element stays allocated and immutable until an exclusive clear.
        Some(unsafe { &*pointer.add(index % N) })
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        if index >= self.len() {
            return None;
        }
        let pointer = self.allocations.get_mut().segments[index / N];
        // SAFETY: the index is initialized and the exclusive receiver excludes every live element borrow.
        Some(unsafe { &mut *pointer.add(index % N) })
    }

    pub fn push_back(&self, value: T) -> PgResult<&T> {
        let end = self
            .len()
            .checked_add(1)
            .ok_or_else(|| invalid("segmented vector length overflow"))?;
        if self.available.get() == 0 {
            self.next_segment()?;
        }
        let pointer = self.cur.get();
        // SAFETY: this disjoint slot is reserved, aligned and uninitialized; publication follows the write.
        unsafe {
            asan::unpoison_memory_region(pointer.cast(), core::mem::size_of::<T>());
            pointer.write(value);
            self.cur.set(pointer.add(1));
            self.available.set(self.available.get() - 1);
            self.len.set(end);
            Ok(&*pointer)
        }
    }

    pub fn push_copy(&self, value: &T) -> PgResult<&T>
    where
        T: Clone,
    {
        self.push_back(value.clone())
    }

    pub fn emplace_back(&self, constructor: impl FnOnce() -> T) -> PgResult<&T> {
        self.push_back(constructor())
    }

    /// # Safety
    /// `constructor` must initialize exactly one valid `T` at its argument on
    /// success and release partial resources before unwinding. Derived pointers
    /// must respect the returned borrow; storing a raw self-address is permitted.
    /// This does not pin `T`: later exclusive access can still move the value.
    pub unsafe fn emplace_in_place(
        &mut self,
        constructor: impl FnOnce(*mut T),
    ) -> PgResult<&mut T> {
        let end = self
            .len()
            .checked_add(1)
            .ok_or_else(|| invalid("segmented vector length overflow"))?;
        if self.available.get() == 0 {
            self.next_segment()?;
        }
        let pointer = self.cur.get();
        let mut pending = PendingSlot {
            pointer: pointer.cast(),
            size: core::mem::size_of::<T>(),
            committed: false,
        };
        // SAFETY: this disjoint uninitialized slot is reserved; the caller establishes valid T before publication.
        unsafe {
            asan::unpoison_memory_region(pointer.cast(), pending.size);
            constructor(pointer);
            self.cur.set(pointer.add(1));
            self.available.set(self.available.get() - 1);
            self.len.set(end);
            pending.committed = true;
            Ok(&mut *pointer)
        }
    }

    fn next_segment(&self) -> PgResult<()> {
        let mut allocations = self
            .allocations
            .try_borrow_mut()
            .map_err(|_| invalid("reentrant segmented vector allocation"))?;
        let index = self.len() / N;
        if index == allocations.segments.len() {
            let layout = segment_layout::<T, N>().ok_or_else(|| {
                allocations
                    .mcx
                    .oom(N.saturating_mul(core::mem::size_of::<T>()))
            })?;
            allocations.segments.reserve(
                index
                    .checked_add(1)
                    .ok_or_else(|| invalid("segmented vector segment count overflow"))?,
            )?;
            let pointer = if layout.size() == 0 {
                NonNull::<T>::dangling().as_ptr()
            } else {
                allocations
                    .mcx
                    .allocate(layout)
                    .map_err(|_| allocations.mcx.oom(layout.size()))?
                    .cast::<T>()
                    .as_ptr()
            };
            let mut pending = PendingSegment {
                pointer: pointer.cast(),
                layout,
                mcx: allocations.mcx,
                committed: false,
            };
            // SAFETY: this new allocation is exclusively owned and contains no initialized values.
            unsafe {
                asan::poison_memory_region(pointer.cast(), layout.size());
            }
            allocations.segments.push(pointer)?;
            pending.committed = true;
        }
        self.cur.set(allocations.segments[index]);
        self.available.set(N);
        Ok(())
    }

    pub fn clear(&mut self) {
        let len = self.len.replace(0);
        if let Some(&first) = self.allocations.get_mut().segments.first() {
            self.cur.set(first);
            self.available.set(N);
        }
        ClearGuard {
            vector: self,
            next: 0,
            len,
        }
        .drain();
    }
}

fn segment_layout<T, const N: usize>() -> Option<Layout> {
    let layout = Layout::array::<T>(N).ok()?;
    Layout::from_size_align(layout.size(), layout.align().max(8))
        .ok()
        .map(|layout| layout.pad_to_align())
}

struct ClearGuard<'a, 'm, T, const N: usize> {
    vector: &'a mut SegmentedVectorGuard<'m, T, N>,
    next: usize,
    len: usize,
}

impl<T, const N: usize> ClearGuard<'_, '_, T, N> {
    fn drain(&mut self) {
        while core::mem::needs_drop::<T>() && self.next < self.len {
            let index = self.next;
            self.next += 1;
            let pointer = self.vector.allocations.get_mut().segments[index / N];
            // SAFETY: each initialized element is destroyed once; advancing first makes unwinding resume safely.
            unsafe {
                ptr::drop_in_place(pointer.add(index % N));
            }
        }
        if self.len != 0 {
            let allocations = self.vector.allocations.get_mut();
            let size = segment_layout::<T, N>().unwrap().size();
            for &pointer in &allocations.segments[..self.len.div_ceil(N)] {
                // SAFETY: all live values have been destroyed and the retained allocation is exclusively borrowed.
                unsafe {
                    asan::poison_memory_region(pointer.cast(), size);
                }
            }
            self.len = 0;
        }
    }
}

impl<T, const N: usize> Drop for ClearGuard<'_, '_, T, N> {
    fn drop(&mut self) {
        self.drain();
    }
}

impl<T, const N: usize> Drop for SegmentedVectorGuard<'_, T, N> {
    fn drop(&mut self) {
        self.clear();
    }
}

impl<T, const N: usize> Drop for Allocations<'_, T, N> {
    fn drop(&mut self) {
        if self.segments.is_empty() {
            self.segments.release();
            return;
        }
        let layout = segment_layout::<T, N>().unwrap();
        for &pointer in self.segments.iter() {
            if layout.size() != 0 {
                // SAFETY: element cleanup has finished; the original allocation/layout is released exactly once.
                unsafe {
                    asan::unpoison_memory_region(pointer.cast(), layout.size());
                    self.mcx
                        .deallocate(NonNull::new_unchecked(pointer.cast()), layout);
                }
            }
        }
        self.segments.release();
    }
}

struct PendingSegment<'m> {
    pointer: *mut u8,
    layout: Layout,
    mcx: Mcx<'m>,
    committed: bool,
}

struct PendingSlot {
    pointer: *mut u8,
    size: usize,
    committed: bool,
}

impl Drop for PendingSlot {
    fn drop(&mut self) {
        if !self.committed {
            // SAFETY: the uncommitted slot remains owned and inaccessible; the initializer cleaned partial resources.
            unsafe {
                asan::poison_memory_region(self.pointer, self.size);
            }
        }
    }
}

impl Drop for PendingSegment<'_> {
    fn drop(&mut self) {
        if !self.committed && self.layout.size() != 0 {
            // SAFETY: failed publication leaves this guard as the sole owner of an uninitialized allocation.
            unsafe {
                asan::unpoison_memory_region(self.pointer, self.layout.size());
                self.mcx
                    .deallocate(NonNull::new_unchecked(self.pointer), self.layout);
            }
        }
    }
}

impl<T, const N: usize> Index<usize> for SegmentedVectorGuard<'_, T, N> {
    type Output = T;
    fn index(&self, index: usize) -> &T {
        self.get(index)
            .expect("segmented vector index out of bounds")
    }
}

impl<T, const N: usize> IndexMut<usize> for SegmentedVectorGuard<'_, T, N> {
    fn index_mut(&mut self, index: usize) -> &mut T {
        self.get_mut(index)
            .expect("segmented vector index out of bounds")
    }
}

pub type SegmentedVector<'m, T, const N: usize = 64> = SegmentedVectorGuard<'m, T, N>;

#[cfg(test)]
mod tests {
    use super::*;
    use mcx::MemoryContext;
    use std::{
        cell::RefCell,
        panic::{catch_unwind, AssertUnwindSafe},
        rc::Rc,
        vec,
        vec::Vec,
    };

    #[derive(Clone)]
    struct Tracked {
        value: u64,
        destroyed: Rc<RefCell<Vec<u64>>>,
        panic: bool,
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.destroyed.borrow_mut().push(self.value);
            assert!(!self.panic, "requested destructor panic");
        }
    }

    #[test]
    fn segmented_stable_borrows_across_growth_and_reuse() {
        let context = MemoryContext::new_bump("segmented-borrows");
        let mut values = SegmentedVector::<u64, 3>::new(context.mcx());
        assert!(values.is_empty());
        assert_eq!(values.size(), 0);
        assert_eq!(values.get(0), None);
        values.clear();
        let first = values.push_back(17).unwrap();
        for value in 1..120 {
            values.emplace_back(|| value).unwrap();
        }
        assert_eq!(*first, 17);
        assert!(ptr::eq(first, &values[0]));
        let old_pointers: Vec<_> = (0..values.len())
            .map(|i| values.get(i).unwrap() as *const u64)
            .collect();
        assert_eq!(values.len(), 120);
        values[1] = 19;
        assert_eq!(values[1], 19);
        assert!(values.get_mut(120).is_none());
        let charged = context.used();
        values.clear();
        values.clear();
        for (index, old) in old_pointers.into_iter().enumerate() {
            let new = values.push_back(index as u64).unwrap();
            assert_eq!(new as *const u64, old);
        }
        assert_eq!(context.used(), charged);
    }

    #[test]
    fn segmented_clear_and_drop_preserve_forward_destruction() {
        let context = MemoryContext::new("segmented-drop");
        let destroyed = Rc::new(RefCell::new(Vec::new()));
        {
            let mut values = SegmentedVector::<Tracked, 3>::new(context.mcx());
            for value in 0..8 {
                values
                    .push_back(Tracked {
                        value,
                        destroyed: destroyed.clone(),
                        panic: false,
                    })
                    .unwrap();
            }
            values.clear();
            assert_eq!(*destroyed.borrow(), (0..8).collect::<Vec<_>>());
            values.clear();
            values
                .push_back(Tracked {
                    value: 8,
                    destroyed: destroyed.clone(),
                    panic: false,
                })
                .unwrap();
        }
        assert_eq!(*destroyed.borrow(), (0..9).collect::<Vec<_>>());
        assert_eq!(context.used(), 0);
    }

    #[test]
    fn segmented_clear_panic_destroys_remaining_elements_and_reuses() {
        let context = MemoryContext::new("segmented-unwind-clear");
        let destroyed = Rc::new(RefCell::new(Vec::new()));
        let mut values = SegmentedVector::<Tracked, 3>::new(context.mcx());
        for value in 0..8 {
            values
                .push_back(Tracked {
                    value,
                    destroyed: destroyed.clone(),
                    panic: value == 2,
                })
                .unwrap();
        }
        assert!(catch_unwind(AssertUnwindSafe(|| values.clear())).is_err());
        assert_eq!(*destroyed.borrow(), (0..8).collect::<Vec<_>>());
        assert_eq!(values.len(), 0);
        values
            .push_back(Tracked {
                value: 9,
                destroyed: destroyed.clone(),
                panic: false,
            })
            .unwrap();
        values.clear();
        assert_eq!(destroyed.borrow().last(), Some(&9));
    }

    #[test]
    fn segmented_drop_panic_releases_all_allocations() {
        let context = MemoryContext::new("segmented-unwind-drop");
        let destroyed = Rc::new(RefCell::new(Vec::new()));
        assert!(catch_unwind(AssertUnwindSafe(|| {
            let values = SegmentedVector::<Tracked, 1>::new(context.mcx());
            for value in 0..40 {
                values
                    .push_back(Tracked {
                        value,
                        destroyed: destroyed.clone(),
                        panic: value == 2,
                    })
                    .unwrap();
            }
        }))
        .is_err());
        assert_eq!(*destroyed.borrow(), (0..40).collect::<Vec<_>>());
        assert_eq!(context.used(), 0);
    }

    #[test]
    fn segmented_constructor_and_clone_panics_do_not_publish_slots() {
        #[derive(Debug)]
        struct Panics;
        impl Clone for Panics {
            fn clone(&self) -> Self {
                panic!("clone")
            }
        }
        let context = MemoryContext::new("segmented-construction");
        let values = SegmentedVector::<Panics, 1>::new(context.mcx());
        assert!(catch_unwind(AssertUnwindSafe(|| {
            values.emplace_back(|| panic!("constructor")).unwrap();
        }))
        .is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| {
            values.push_copy(&Panics).unwrap();
        }))
        .is_err());
        assert!(values.is_empty());
        assert_eq!(context.used(), 0);
        values.push_back(Panics).unwrap();
        assert_eq!(values.len(), 1);
    }

    #[test]
    fn segmented_constructor_reentry_preserves_committed_length() {
        let context = MemoryContext::new("segmented-reentry");
        let values = SegmentedVector::<u64, 1>::new(context.mcx());
        let last = values
            .emplace_back(|| {
                for i in 0..40 {
                    values.push_back(i).unwrap();
                }
                91
            })
            .unwrap();
        assert_eq!(*last, 91);
        assert_eq!(values.len(), 41);
        for i in 0..40 {
            assert_eq!(values[i], i as u64);
        }
        assert_eq!(values[40], 91);
    }

    #[test]
    fn segmented_in_place_construction_observes_final_address() {
        struct AtAddress {
            address: *const AtAddress,
            value: u64,
        }
        let context = MemoryContext::new("segmented-in-place");
        let mut values = SegmentedVector::<AtAddress, 3>::new(context.mcx());
        for value in 0..120 {
            // SAFETY: the initializer writes one complete value and retains its address only as non-owning data.
            let inserted = unsafe {
                values.emplace_in_place(|slot| {
                    slot.write(AtAddress {
                        address: slot,
                        value,
                    })
                })
            }
            .unwrap();
            assert!(ptr::eq(inserted.address, inserted));
        }
        for value in 0..120 {
            assert_eq!(values[value].value, value as u64);
            assert!(ptr::eq(values[value].address, &values[value]));
        }
    }

    #[test]
    fn segmented_in_place_admission_and_panic_rollback() {
        let context = MemoryContext::new("segmented-place-budget").with_limit(8);
        let mut values = SegmentedVector::<u64, 1>::new(context.mcx());
        assert!(catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: the initializer panics before initialization and owns no partial resources.
            unsafe {
                values
                    .emplace_in_place(|_| panic!("in-place constructor"))
                    .unwrap();
            }
        }))
        .is_err());
        assert_eq!(values.len(), 0);
        // SAFETY: one valid u64 is initialized in the admitted slot.
        assert_eq!(
            *unsafe { values.emplace_in_place(|slot| slot.write(7)) }.unwrap(),
            7
        );
        let called = Cell::new(false);
        // SAFETY: if called, the closure writes one complete u64 without retaining the pointer.
        let failure = unsafe {
            values.emplace_in_place(|slot| {
                called.set(true);
                slot.write(9);
            })
        };
        assert!(failure.is_err());
        assert!(!called.get());
        assert_eq!(values.len(), 1);
    }

    #[test]
    fn segmented_budget_failure_drops_value_and_keeps_old_elements() {
        let context =
            MemoryContext::new("segmented-budget").with_limit(core::mem::size_of::<Tracked>() * 3);
        let destroyed = Rc::new(RefCell::new(Vec::new()));
        let mut values = SegmentedVector::<Tracked, 3>::new(context.mcx());
        for value in 0..3 {
            values
                .push_back(Tracked {
                    value,
                    destroyed: destroyed.clone(),
                    panic: false,
                })
                .unwrap();
        }
        assert!(values
            .push_back(Tracked {
                value: 9,
                destroyed: destroyed.clone(),
                panic: false
            })
            .is_err());
        assert_eq!(values.len(), 3);
        assert_eq!(*destroyed.borrow(), vec![9]);
        assert_eq!(values[2].value, 2);
        values.clear();
        values
            .push_back(Tracked {
                value: 4,
                destroyed: destroyed.clone(),
                panic: false,
            })
            .unwrap();
        assert_eq!(values[0].value, 4);
        drop(values);
        assert_eq!(*destroyed.borrow(), vec![9, 0, 1, 2, 4]);
        assert_eq!(context.used(), 0);
    }

    #[test]
    fn segmented_pointer_list_growth_failure_is_recoverable() {
        let context = MemoryContext::new("segmented-list-budget").with_limit(32 * 8);
        let mut values = SegmentedVector::<u64, 1>::new(context.mcx());
        for value in 0..32 {
            values.push_back(value).unwrap();
        }
        assert!(values.push_back(32).is_err());
        assert_eq!(values.len(), 32);
        values.clear();
        for value in 0..32 {
            values.push_back(value + 100).unwrap();
        }
        assert_eq!(values[31], 131);
        drop(values);
        assert_eq!(context.used(), 0);
    }

    #[test]
    fn segmented_segment_failure_retains_admitted_metadata_then_releases() {
        let context = MemoryContext::new("segmented-partial-budget").with_limit(776);
        let mut values = SegmentedVector::<u64, 1>::new(context.mcx());
        for value in 0..32 {
            values.push_back(value).unwrap();
        }
        assert!(values.push_back(32).is_err());
        assert_eq!(values.len(), 32);
        assert_eq!(values[31], 31);
        assert_eq!(context.used(), 776);
        values.clear();
        for value in 0..32 {
            values.push_back(value + 100).unwrap();
        }
        assert_eq!(values[31], 131);
        assert_eq!(context.used(), 776);
        drop(values);
        assert_eq!(context.used(), 0);
    }

    #[test]
    fn segmented_releases_spilled_pointer_list() {
        let context = MemoryContext::new("segmented-release");
        {
            let values = SegmentedVector::<u64, 1>::new(context.mcx());
            for value in 0..100 {
                values.push_back(value).unwrap();
            }
            assert!(context.used() > 800);
        }
        assert_eq!(context.used(), 0);
    }

    #[test]
    fn segmented_zero_sized_elements_and_large_alignment() {
        #[repr(align(128))]
        struct Aligned(u64);
        let context = MemoryContext::new("segmented-layout");
        let mut empty = SegmentedVector::<(), 3>::new(context.mcx());
        for _ in 0..100 {
            empty.push_back(()).unwrap();
        }
        assert_eq!(empty.len(), 100);
        empty.clear();
        assert!(empty.is_empty());
        let values = SegmentedVector::<Aligned, 3>::new(context.mcx());
        for value in 0..12 {
            let inserted = values.emplace_back(|| Aligned(value)).unwrap();
            assert_eq!(inserted as *const Aligned as usize % 128, 0);
            assert_eq!(inserted.0, value);
        }
    }

    #[test]
    fn segmented_layout_overflow_is_fallible() {
        let context = MemoryContext::new("segmented-overflow");
        let values = SegmentedVector::<u64, { usize::MAX }>::new(context.mcx());
        assert!(values.push_back(1).is_err());
        assert_eq!(values.len(), 0);
        assert_eq!(context.used(), 0);
    }

    #[cfg(tpde_address_sanitizer)]
    #[test]
    fn segmented_asan_partial_granules_and_clear() {
        extern "C" {
            fn __asan_address_is_poisoned(pointer: *const u8) -> i32;
        }
        fn observe<const N: usize>(context: &MemoryContext) {
            let mut values = SegmentedVector::<u8, N>::new(context.mcx());
            let size = segment_layout::<u8, N>().unwrap().size();
            let mut pointers = Vec::new();
            for segment in 0..40 {
                let first = values.push_back(segment as u8).unwrap() as *const u8;
                pointers.push(first);
                for i in 0..size {
                    // SAFETY: ASAN inspects shadow only; the pointer remains within this live segment allocation.
                    assert_eq!(
                        unsafe { __asan_address_is_poisoned(first.add(i)) } != 0,
                        i != 0
                    );
                }
                for i in 1..N {
                    values.push_back(i as u8).unwrap();
                }
                for i in 0..size {
                    // SAFETY: the full live prefix and inaccessible tail belong to this retained allocation.
                    assert_eq!(
                        unsafe { __asan_address_is_poisoned(first.add(i)) } != 0,
                        i >= N
                    );
                }
            }
            values.clear();
            for &first in &pointers {
                for i in 0..size {
                    // SAFETY: clear retains allocations; querying shadow does not access destroyed elements.
                    assert_ne!(unsafe { __asan_address_is_poisoned(first.add(i)) }, 0);
                }
            }
            let reused = values.push_back(91).unwrap() as *const u8;
            assert_eq!(reused, pointers[0]);
            for i in 0..size {
                // SAFETY: the retained first segment has exactly one initialized byte after reuse.
                assert_eq!(
                    unsafe { __asan_address_is_poisoned(reused.add(i)) } != 0,
                    i != 0
                );
            }
            let failed = Cell::new(ptr::null_mut());
            assert!(catch_unwind(AssertUnwindSafe(|| {
                // SAFETY: the initializer panics before constructing a value and saves only an address for shadow inspection.
                unsafe {
                    values
                        .emplace_in_place(|slot| {
                            failed.set(slot);
                            panic!("in-place ASAN rollback");
                        })
                        .unwrap();
                }
            }))
            .is_err());
            assert_eq!(values.len(), 1);
            assert_eq!(values[0], 91);
            // SAFETY: both retained allocations remain live; shadow inspection never accesses either element.
            unsafe {
                assert_eq!(__asan_address_is_poisoned(reused), 0);
                assert_ne!(__asan_address_is_poisoned(failed.get()), 0);
            }
        }
        for context in [
            MemoryContext::new("segmented-asan"),
            MemoryContext::new_bump("segmented-asan-bump"),
        ] {
            observe::<1>(&context);
            observe::<3>(&context);
            observe::<9>(&context);
        }
    }

    #[cfg(tpde_address_sanitizer)]
    #[test]
    fn segmented_forgotten_guard_allows_context_reset_and_reallocation() {
        for mut context in [
            MemoryContext::new("segmented-forgotten"),
            MemoryContext::new_bump("segmented-forgotten-bump"),
            MemoryContext::new_bumpforget("segmented-forgotten-bumpforget"),
            MemoryContext::new_bumpdrop("segmented-forgotten-bumpdrop"),
            MemoryContext::new_generation("segmented-forgotten-generation"),
        ] {
            let values = SegmentedVector::<u8, 9>::new(context.mcx());
            values.push_back(1).unwrap();
            core::mem::forget(values);
            context.reset();
            let layout = Layout::from_size_align(16, 8).unwrap();
            let mcx = context.mcx();
            let pointer = mcx.allocate(layout).unwrap().cast::<u8>();
            // SAFETY: the context reset retired all forgotten values; this newly allocated region is exclusively owned.
            unsafe {
                pointer.as_ptr().write_bytes(0x5a, layout.size());
                assert_eq!(
                    core::slice::from_raw_parts(pointer.as_ptr(), layout.size()),
                    &[0x5a; 16]
                );
                mcx.deallocate(pointer, layout);
            }
        }
    }
}
