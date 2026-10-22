// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use super::*;
use mcx::MemoryContext;
use std::{
    cell::{Cell, RefCell},
    panic::{catch_unwind, AssertUnwindSafe},
    rc::Rc,
    vec::Vec,
};

#[test]
fn disjoint_borrows_and_alignment_padding() {
    let context = MemoryContext::new("bump");
    let bump = BumpAllocator::<64>::new(context.mcx());
    let first = bump.allocate(3, 1).unwrap();
    let second = bump.allocate(8, 8).unwrap();
    first[0].write(11);
    second[0].write(22);
    assert_eq!(
        bump.allocation_info(second.as_ptr().cast()).unwrap().offset,
        8
    );
    // SAFETY: both bytes were initialized in their disjoint allocation extents.
    unsafe {
        assert_eq!((first[0].assume_init(), second[0].assume_init()), (11, 22));
    }
    assert_eq!(bump.stats().allocated_bytes, 64);
}

#[test]
fn slab_growth_at_every_eight_and_direct_slab_path() {
    let context = MemoryContext::new("bump");
    let bump = BumpAllocator::<64>::new(context.mcx());
    for index in 0..40 {
        let block = bump.allocate_slab(1, 1).unwrap();
        let info = bump.allocation_info(block.as_ptr().cast()).unwrap();
        assert_eq!(
            info,
            BumpAllocationInfo {
                large: false,
                index,
                offset: 0,
                size: 64 << (index / 8)
            }
        );
    }
    assert_eq!(bump.stats().regular_slabs, 40);
    assert!(context.used() > bump.stats().allocated_bytes);
    drop(bump);
    assert_eq!(context.used(), 0);
}

#[test]
fn large_slabs_do_not_change_current_regular_slab() {
    let context = MemoryContext::new("bump");
    let bump = BumpAllocator::<64>::new(context.mcx());
    bump.allocate(8, 8).unwrap();
    let large = bump.allocate(65, 16).unwrap();
    assert_eq!(
        bump.allocation_info(large.as_ptr().cast()).unwrap(),
        BumpAllocationInfo {
            large: true,
            index: 0,
            offset: 0,
            size: 65
        }
    );
    let next = bump.allocate(8, 8).unwrap();
    assert_eq!(
        bump.allocation_info(next.as_ptr().cast()).unwrap().offset,
        8
    );
    assert_eq!(bump.stats().allocated_bytes, 129);
    drop(bump);
    assert_eq!(context.used(), 0);
}

#[test]
fn reset_retains_first_and_releases_other_slabs() {
    let context = MemoryContext::new("bump");
    let mut bump = BumpAllocator::<64>::new(context.mcx());
    let first = bump.allocate(8, 8).unwrap().as_ptr();
    bump.allocate(64, 8).unwrap();
    bump.allocate(80, 8).unwrap();
    bump.reset();
    assert_eq!(
        bump.stats(),
        BumpStats {
            regular_slabs: 1,
            large_slabs: 0,
            allocated_bytes: 64
        }
    );
    assert_eq!(bump.allocate(8, 8).unwrap().as_ptr(), first);
    bump.reset();
    assert_eq!(bump.allocate(64, 16).unwrap().as_ptr(), first);
    drop(bump);
    assert_eq!(context.used(), 0);
}

#[test]
fn reset_empty_and_large_only_preserves_no_current_slab() {
    let context = MemoryContext::new("bump");
    let mut bump = BumpAllocator::<64>::new(context.mcx());
    bump.reset();
    bump.allocate(65, 16).unwrap();
    bump.reset();
    assert_eq!(bump.stats().allocated_bytes, 0);
    assert_eq!(bump.stats().regular_slabs, 0);
    bump.allocate(16, 16).unwrap();
    assert_eq!(bump.stats().regular_slabs, 1);
}

#[test]
fn invalid_requests_and_size_overflow_leave_allocator_reusable() {
    let context = MemoryContext::new("bump");
    let bump = BumpAllocator::<64>::new(context.mcx());
    for (size, align) in [(0, 1), (1, 0), (1, 3), (usize::MAX, 8), (1, 32)] {
        assert!(bump.allocate(size, align).is_err());
    }
    assert!(bump.allocate_slab(1, 32).is_err());
    assert!(BumpAllocator::<64>::slab_size(usize::MAX).is_err());
    assert!(bump.new_array_with::<u64>(usize::MAX, |_| Ok(0)).is_err());
    // pgrust's mcx raises an over-ceiling request as an unwind (PostgreSQL's "invalid
    // memory alloc request size" is an ERROR), where TPDE's expected `AllocError`.
    let refused = catch_unwind(AssertUnwindSafe(|| bump.allocate(0x4000_0000, 16).is_err()));
    assert!(matches!(refused, Ok(true) | Err(_)));
    assert_eq!(bump.stats().allocated_bytes, 0);
    assert_eq!(*bump.alloc(37u64).unwrap(), 37);
}

#[test]
fn budget_failure_retains_only_admitted_metadata_and_reuses_it() {
    let context = MemoryContext::new("bump").with_limit(80);
    let mut bump = BumpAllocator::<64>::new(context.mcx());
    assert!(bump.allocate(65, 16).is_err());
    assert_eq!(bump.stats().allocated_bytes, 0);
    assert_eq!(context.used(), 16);
    bump.allocate(64, 16).unwrap();
    assert!(bump.allocate(1, 1).is_err());
    assert_eq!(bump.stats().allocated_bytes, 64);
    bump.reset();
    bump.allocate(64, 16).unwrap();
    drop(bump);
    assert_eq!(context.used(), 0);
}

struct Tracked<'a> {
    value: u64,
    destroyed: &'a Cell<usize>,
}
impl Drop for Tracked<'_> {
    fn drop(&mut self) {
        self.destroyed.set(self.destroyed.get() + 1);
    }
}

#[test]
fn unique_move_deleter_and_extract_destroy_once() {
    let context = MemoryContext::new("bump");
    let bump = BumpAllocator::<64>::new(context.mcx());
    let destroyed = Cell::new(0);
    let first = bump
        .new_with(|| {
            Ok(Tracked {
                value: 41,
                destroyed: &destroyed,
            })
        })
        .unwrap();
    let mut moved = first;
    moved.value += 1;
    BumpAllocatorDeleter.destroy(moved);
    assert_eq!(destroyed.get(), 1);
    let second = bump
        .new_with(|| {
            Ok(Tracked {
                value: 43,
                destroyed: &destroyed,
            })
        })
        .unwrap();
    let value = second.into_inner();
    assert_eq!(value.value, 43);
    assert_eq!(destroyed.get(), 1);
    drop(value);
    assert_eq!(destroyed.get(), 2);
}

struct Ordered {
    index: usize,
    order: Rc<RefCell<Vec<usize>>>,
}
impl Drop for Ordered {
    fn drop(&mut self) {
        self.order.borrow_mut().push(self.index);
    }
}

#[test]
fn array_constructor_failure_destroys_live_prefix_in_reverse() {
    let context = MemoryContext::new("bump");
    let bump = BumpAllocator::<64>::new(context.mcx());
    let order = Rc::new(RefCell::new(Vec::new()));
    let result = bump.new_array_with(6, |index| {
        if index == 3 {
            return Err(invalid("constructor failed"));
        }
        Ok(Ordered {
            index,
            order: order.clone(),
        })
    });
    assert!(result.is_err());
    assert_eq!(*order.borrow(), [2, 1, 0]);
    assert_eq!(bump.stats().large_slabs, 1);
    order.borrow_mut().clear();
    let values = bump
        .new_array_with(3, |index| {
            Ok(Ordered {
                index,
                order: order.clone(),
            })
        })
        .unwrap();
    assert_eq!(values.len(), 3);
    drop(values);
    assert_eq!(*order.borrow(), [2, 1, 0]);
}

#[test]
fn constructor_panic_and_recursive_allocation_cleanup() {
    let context = MemoryContext::new("bump");
    let bump = BumpAllocator::<128>::new(context.mcx());
    let order = Rc::new(RefCell::new(Vec::new()));
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = bump.new_array_with(6, |index| {
            if index == 3 {
                panic!("constructor failed");
            }
            Ok(Ordered {
                index,
                order: order.clone(),
            })
        });
    }));
    assert!(failed.is_err());
    assert_eq!(*order.borrow(), [2, 1, 0]);
    let nested = bump
        .new_with(|| {
            let inner = bump.alloc(99u64)?;
            Ok(*inner + 1)
        })
        .unwrap();
    assert_eq!(*nested, 100);
}

#[test]
fn zero_sized_and_non_power_of_two_typed_values_are_valid() {
    let context = MemoryContext::new("bump");
    let bump = BumpAllocator::<128>::new(context.mcx());
    assert_eq!(*bump.alloc(()).unwrap(), ());
    assert_eq!(*bump.alloc([1u8, 2, 3]).unwrap(), [1, 2, 3]);
    let values = bump.new_array_with(17, |_| Ok(())).unwrap();
    assert_eq!(values.len(), 17);
    assert!(bump
        .new_array_with::<u64>(0, |_| unreachable!())
        .unwrap()
        .is_empty());
    #[repr(align(32))]
    struct OverAligned(u8);
    let fresh = BumpAllocator::<128>::new(context.mcx());
    assert!(fresh.alloc(OverAligned(1)).is_err());
    bump.allocate(1, 1).unwrap();
    let value = bump.alloc(OverAligned(42)).unwrap();
    assert_eq!(value.0, 42);
    assert_eq!((value as *const OverAligned).addr() % 32, 0);
}

#[test]
fn outer_bump_context_defers_reclamation_until_its_reset() {
    let mut context = MemoryContext::new_bump("outer");
    let mut seed = mcx::PgVec::<u64>::new_in(context.mcx());
    seed.try_reserve_exact(1).unwrap();
    seed.push(7);
    drop(seed);
    let keeper = context.stats().arena_footprint;
    assert!(keeper >= mem::size_of::<u64>());
    let charged;
    {
        let mut bump = BumpAllocator::<64>::new(context.mcx());
        bump.allocate(64, 16).unwrap();
        bump.allocate(64, 16).unwrap();
        bump.allocate(1 << 20, 16).unwrap();
        charged = context.used();
        bump.reset();
        assert_eq!(context.used(), charged);
        assert_eq!(bump.stats().allocated_bytes, 64);
    }
    assert_eq!(context.used(), charged);
    assert!(charged >= keeper + 128 + (1 << 20));
    context.reset();
    assert_eq!(context.used(), keeper);
    assert_eq!(context.stats().arena_footprint, keeper);
    for _ in 0..3 {
        let bump = BumpAllocator::<64>::new(context.mcx());
        bump.allocate(64, 16).unwrap();
        drop(bump);
        context.reset();
        assert_eq!(context.used(), keeper);
        assert_eq!(context.stats().arena_footprint, keeper);
    }
}

#[test]
fn failed_single_construction_retains_reserved_cursor_space() {
    let context = MemoryContext::new("bump");
    let bump = BumpAllocator::<64>::new(context.mcx());
    assert!(bump
        .new_with::<u64>(|| Err(invalid("constructor failed")))
        .is_err());
    let value = bump.allocate(8, 8).unwrap();
    assert_eq!(
        bump.allocation_info(value.as_ptr().cast()).unwrap().offset,
        8
    );
}

#[cfg(tpde_address_sanitizer)]
#[test]
fn poison_transitions_follow_exact_live_extent_and_reset() {
    extern "C" {
        fn __asan_address_is_poisoned(pointer: *const u8) -> i32;
    }
    let context = MemoryContext::new("bump");
    let mut bump = BumpAllocator::<64>::new(context.mcx());
    let first = bump.allocate(8, 8).unwrap().as_ptr().cast::<u8>();
    // SAFETY: ASAN shadow queries do not access the exclusively owned slab bytes.
    unsafe {
        assert_eq!(__asan_address_is_poisoned(first), 0);
        assert_ne!(__asan_address_is_poisoned(first.add(8)), 0);
    }
    bump.reset();
    // SAFETY: reset retained this first slab, now poisoned in full.
    unsafe {
        assert_ne!(__asan_address_is_poisoned(first), 0);
    }
    bump.allocate(16, 16).unwrap();
    // SAFETY: the retained first slab remains live with a sixteen-byte accessible prefix.
    unsafe {
        assert_eq!(__asan_address_is_poisoned(first.add(15)), 0);
        assert_ne!(__asan_address_is_poisoned(first.add(16)), 0);
    }
}

#[test]
fn array_destructor_panic_still_destroys_remaining_prefix() {
    struct Fails {
        index: usize,
        order: Rc<RefCell<Vec<usize>>>,
    }
    impl Drop for Fails {
        fn drop(&mut self) {
            self.order.borrow_mut().push(self.index);
            if self.index == 2 {
                panic!("destructor failed");
            }
        }
    }
    let context = MemoryContext::new("bump");
    let bump = BumpAllocator::<128>::new(context.mcx());
    let order = Rc::new(RefCell::new(Vec::new()));
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let values = bump
            .new_array_with(4, |index| {
                Ok(Fails {
                    index,
                    order: order.clone(),
                })
            })
            .unwrap();
        drop(values);
    }));
    assert!(failed.is_err());
    assert_eq!(*order.borrow(), [3, 2, 1, 0]);
}

#[test]
fn forgotten_owner_then_outer_reset_remains_reusable() {
    let mut context = MemoryContext::new_bump("outer");
    let bump = BumpAllocator::<64>::new(context.mcx());
    for _ in 0..40 {
        bump.allocate_slab(8, 8).unwrap();
    }
    bump.allocate(4097, 16).unwrap();
    mem::forget(bump);
    context.reset();
    let mut reused = mcx::PgVec::<u8>::new_in(context.mcx());
    reused.try_reserve_exact(2048).unwrap();
    reused.resize(2048, 0x5a);
    drop(reused);
    let bump = BumpAllocator::<64>::new(context.mcx());
    let bytes = bump.allocate(64, 16).unwrap();
    for byte in bytes {
        byte.write(0x5a);
    }
    assert_eq!(bump.stats().allocated_bytes, 64);
}
