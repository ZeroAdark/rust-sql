// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use mcx::MemoryContext;
use tpde_util::{SmallVec, SmallVectorGuard};

#[test]
fn no_drop_and_resource_ownership_are_separate() {
    assert!(!core::mem::needs_drop::<SmallVec<'_, u64, 4>>());
    assert!(core::mem::needs_drop::<SmallVectorGuard<'_, u64, 4>>());
    let context = MemoryContext::new("small-vector-release");
    let mut values = SmallVec::<u64, 2>::new(context.mcx());
    values.append([1, 2, 3, 4].into_iter()).unwrap();
    assert_eq!(values.capacity(), 5);
    assert!(context.stats().used >= 5 * 8);
    values.clear();
    assert_eq!(values.capacity(), 5);
    values.release();
    assert_eq!(values.capacity(), 2);
    assert!(values.is_empty());
    assert_eq!(context.stats().used, 0);
    values.push(7).unwrap();
    assert_eq!(&*values, &[7]);
}

#[test]
fn reserve_and_move_failure_leave_both_vectors_intact() {
    let context = MemoryContext::new("small-vector-oom").with_limit(64);
    let mut values = SmallVec::<u64, 1>::new(context.mcx());
    values.push(3).unwrap();
    values.reserve(4).unwrap();
    let pointer = values.as_ptr();
    let used = context.stats().used;
    assert_eq!(values.capacity(), 4);
    assert!(values.reserve(5).is_err());
    assert_eq!(values.capacity(), 4);
    assert_eq!(values.as_ptr(), pointer);
    assert_eq!(&*values, &[3]);
    assert_eq!(context.stats().used, used);
    let mut source = SmallVec::<u64, 16>::new(context.mcx());
    source.append((10..20).map(|n| n as u64)).unwrap();
    assert!(values.move_assign(&mut source).is_err());
    assert_eq!(&*values, &[3]);
    assert_eq!(&*source, &[10, 11, 12, 13, 14, 15, 16, 17, 18, 19]);
    assert_eq!(values.capacity(), 4);
    assert_eq!(source.capacity(), 16);
    assert!(values.reserve(usize::MAX).is_err());
    assert_eq!(&*values, &[3]);
}

#[test]
fn cross_context_move_is_rejected_before_effects() {
    let context_a = MemoryContext::new("small-vector-context-a");
    let context_b = MemoryContext::new("small-vector-context-b");
    let mut a = SmallVectorGuard::<u64, 0>::new(context_a.mcx());
    let mut b = SmallVectorGuard::<u64, 0>::new(context_b.mcx());
    a.append([3, 4].into_iter()).unwrap();
    b.append([7, 8].into_iter()).unwrap();
    let old_a = a.as_ptr();
    let old_b = b.as_ptr();
    assert!(a.move_assign(&mut b).is_err());
    assert_eq!(&*a, &[3, 4]);
    assert_eq!(&*b, &[7, 8]);
    assert_eq!(a.as_ptr(), old_a);
    assert_eq!(b.as_ptr(), old_b);
}

#[test]
fn uninitialized_suffix_commit_cancel_and_shrink() {
    let context = MemoryContext::new("small-vector-uninit");
    let mut values = SmallVec::<u32, 1>::new(context.mcx());
    values.push(7).unwrap();
    {
        let mut suffix = values.resize_uninitialized(7).unwrap();
        assert_eq!(suffix.len(), 6);
        suffix[0].write(123);
    }
    assert_eq!(&*values, &[7]);
    {
        let mut suffix = values.resize_uninitialized(4).unwrap();
        for (index, slot) in suffix.iter_mut().enumerate() {
            slot.write(index as u32 + 10);
        }
        // SAFETY: every suffix slot was initialized immediately above.
        unsafe {
            suffix.assume_init();
        }
    }
    assert_eq!(&*values, &[7, 10, 11, 12]);
    drop(values.resize_uninitialized(2).unwrap());
    assert_eq!(&*values, &[7, 10]);
    assert!(values.erase(2..1).is_err());
    assert!(values.erase(0..3).is_err());
    values.clear();
    assert!(values.pop_back().is_err());
    assert_eq!(values.pop(), None);
}

#[test]
fn partial_uninitialized_commit_preserves_only_written_prefix() {
    fn exercise<const N: usize>() {
        let context = MemoryContext::new("small-vector-partial-commit");
        let mut values = SmallVec::<u64, N>::new(context.mcx());
        values.push(7).unwrap();
        let mut suffix = values.resize_uninitialized(8).unwrap();
        suffix[0].write(11);
        suffix[1].write(13);
        // SAFETY: only the two initialized slots are published; the remaining slots stay spare.
        unsafe { suffix.assume_init_prefix(2).unwrap() };
        assert_eq!(&*values, &[7, 11, 13]);
        let suffix = values.resize_uninitialized(8).unwrap();
        // SAFETY: an out-of-range count is rejected without publishing any elements.
        assert!(unsafe { suffix.assume_init_prefix(usize::MAX) }.is_err());
        assert_eq!(&*values, &[7, 11, 13]);
        let suffix = values.resize_uninitialized(8).unwrap();
        // SAFETY: committing no new elements needs no initialization.
        unsafe { suffix.assume_init_prefix(0).unwrap() };
        values.push(17).unwrap();
        assert_eq!(&*values, &[7, 11, 13, 17]);
    }
    exercise::<8>();
    exercise::<0>();
}

#[repr(align(64))]
#[derive(Clone, Copy)]
struct Aligned([u8; 64]);

#[test]
fn zero_inline_and_overaligned_values() {
    assert_eq!(tpde_util::default_inline_capacity::<u8>(), 256);
    assert_eq!(tpde_util::default_inline_capacity::<u64>(), 32);
    assert_eq!(tpde_util::default_inline_capacity::<[u8; 257]>(), 1);
    let context = MemoryContext::new("small-vector-alignment");
    let mut values = SmallVectorGuard::<Aligned, 0>::new(context.mcx());
    assert_eq!(values.capacity(), 0);
    for i in 0..17u8 {
        values.push(Aligned([i; 64])).unwrap();
        assert_eq!(values.as_ptr() as usize % 64, 0);
    }
    for (i, value) in values.iter().enumerate() {
        assert_eq!(value.0, [i as u8; 64]);
    }
}

#[test]
fn arena_reset_reuses_no_drop_vector_poisoned_storage() {
    let mut context = MemoryContext::new_bump("small-vector-reset");
    let old;
    {
        let mut values = SmallVec::<u8, 0>::new(context.mcx());
        values.reserve(64).unwrap();
        old = values.as_ptr();
    }
    context.reset();
    let mut values = mcx::vec_new_in::<u8>(context.mcx());
    values.try_reserve_exact(64).unwrap();
    assert_eq!(old, values.as_ptr());
    values.resize(64, 0xa5);
    assert_eq!(&*values, &[0xa5; 64]);
}

#[test]
fn arena_reset_reuses_forgotten_guard_poisoned_storage() {
    let mut context = MemoryContext::new_bump("small-vector-forgotten");
    let old;
    {
        let mut values = SmallVectorGuard::<u8, 0>::new(context.mcx());
        values.reserve(64).unwrap();
        old = values.as_ptr();
        core::mem::forget(values);
    }
    context.reset();
    let mut values = mcx::vec_new_in::<u8>(context.mcx());
    values.try_reserve_exact(64).unwrap();
    assert_eq!(old, values.as_ptr());
    values.resize(64, 0x5a);
    assert_eq!(&*values, &[0x5a; 64]);
}

#[test]
fn odd_capacity_neighbors_and_ordinary_moves_remain_accessible() {
    let context = MemoryContext::new_bump("small-vector-neighbors");
    let mut a = SmallVectorGuard::<u8, 1>::new(context.mcx());
    a.reserve(7).unwrap();
    a.push(1).unwrap();
    let mut b = SmallVectorGuard::<u8, 1>::new(context.mcx());
    b.reserve(9).unwrap();
    b.append([2, 3, 4].into_iter()).unwrap();
    let mut moved = a;
    moved.resize(7, 8).unwrap();
    assert_eq!(&*moved, &[1, 8, 8, 8, 8, 8, 8]);
    assert_eq!(&*b, &[2, 3, 4]);
    moved.clear();
    b.push(5).unwrap();
    assert_eq!(&*b, &[2, 3, 4, 5]);
}

#[cfg(tpde_address_sanitizer)]
#[test]
fn heap_shadow_tracks_every_live_prefix_transition() {
    extern "C" {
        fn __asan_address_is_poisoned(pointer: *const u8) -> i32;
    }
    let context = MemoryContext::new("small-vector-shadow");
    let mut values = SmallVectorGuard::<u64, 0>::new(context.mcx());
    values.reserve(8).unwrap();
    let pointer = values.as_ptr().cast::<u8>();
    let poisoned = |index: usize| {
        // SAFETY: query inspects shadow only; each address lies in the reserved live allocation.
        unsafe { __asan_address_is_poisoned(pointer.add(index * 8)) != 0 }
    };
    assert!(poisoned(0));
    values.push(7).unwrap();
    assert!(!poisoned(0));
    assert!(poisoned(1));
    values.pop_back().unwrap();
    assert!(poisoned(0));
    values.resize_default(5).unwrap();
    assert!(!poisoned(4));
    assert!(poisoned(5));
    values.erase(1..3).unwrap();
    assert!(!poisoned(2));
    assert!(poisoned(3));
    {
        let mut suffix = values.resize_uninitialized(7).unwrap();
        assert!(!poisoned(6));
        suffix[0].write(99);
    }
    assert!(poisoned(3));
    {
        let mut suffix = values.resize_uninitialized(7).unwrap();
        for slot in suffix.iter_mut() {
            slot.write(42);
        }
        // SAFETY: every reserved slot has been initialized immediately above.
        unsafe {
            suffix.assume_init();
        }
    }
    assert!(!poisoned(6));
    assert!(poisoned(7));
    values.clear();
    assert!(poisoned(0));
    assert!(poisoned(7));
    let mut suffix = values.resize_uninitialized(7).unwrap();
    suffix[0].write(23);
    suffix[1].write(29);
    // SAFETY: the published prefix contains exactly the two writes above.
    unsafe { suffix.assume_init_prefix(2).unwrap() };
    assert_eq!(&*values, &[23, 29]);
    assert!(!poisoned(1));
    assert!(poisoned(2));
    assert!(poisoned(6));
}
