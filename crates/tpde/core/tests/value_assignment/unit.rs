// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use super::*;
use crate::value_assignment::{MAX_PART_COUNT_EXCLUSIVE, NUM_FREE_LISTS};
use mcx::MemoryContext;
use std::panic::{catch_unwind, AssertUnwindSafe};
use tpde_base::ValLocalIdx;
use tpde_util::SmallVec;

#[test]
fn tiny_reuse_and_multiple_live_assignments() {
    let context = MemoryContext::new("assignment");
    with_assignment_allocator(context.mcx(), |allocator| {
        let a = allocator
            .allocate(1)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 12);
        let b = allocator
            .allocate(1)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 42);
        let header: &ValueAssignment = &a;
        let part = a.part(0);
        let c = allocator
            .allocate(1)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 19);
        part.set(13);
        header.set_references_left(31);
        assert_eq!(
            (
                a.part(0).get(),
                b.part(0).get(),
                c.part(0).get(),
                a.references_left()
            ),
            (13, 42, 19, 31)
        );
        let pointer = a.as_ptr();
        allocator.deallocate(a);
        let reused = allocator
            .allocate(1)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 7);
        assert_eq!(reused.as_ptr(), pointer);
        assert_eq!(b.part(0).get(), 42);
    });
}

#[test]
fn all_size_classes_and_strict_upstream_limit() {
    for count in [0, 1] {
        assert_eq!(
            AssignmentAllocationInfo::new(count).unwrap(),
            AssignmentAllocationInfo {
                size: 16,
                alloc_size: 16,
                free_list_idx: 0
            }
        );
    }
    for class in 1..NUM_FREE_LISTS {
        let capacity = 16usize << class;
        let first_count = ((capacity / 2 - 14) / 2 + 1) as u32;
        let last_count = (((capacity - 14) / 2) as u32).min(MAX_PART_COUNT_EXCLUSIVE - 1);
        for count in [first_count, last_count] {
            let info = AssignmentAllocationInfo::new(count).unwrap();
            assert_eq!(
                (info.free_list_idx, info.alloc_size, info.size),
                (class, capacity, 14 + count * 2)
            );
        }
    }
    for count in [
        MAX_PART_COUNT_EXCLUSIVE,
        MAX_PART_COUNT_EXCLUSIVE + 1,
        u32::MAX,
    ] {
        assert!(AssignmentAllocationInfo::new(count).is_err());
    }
}

#[test]
fn packed_header_flags_and_intentional_unions() {
    let context = MemoryContext::new("assignment");
    with_assignment_allocator(context.mcx(), |allocator| {
        let assignment = allocator.allocate(5).unwrap().initialize(
            AssignmentInit {
                max_part_size: 8,
                ..AssignmentInit::default()
            },
            |i| i as u16 + 3,
        );
        for flags in 0..16u8 {
            assignment.set_pending_free(flags & 1 != 0);
            assignment.set_variable_ref(flags & 2 != 0);
            assignment.set_stack_variable(flags & 4 != 0);
            assignment.set_delay_free(flags & 8 != 0);
            assert_eq!(
                (
                    assignment.pending_free(),
                    assignment.variable_ref(),
                    assignment.stack_variable(),
                    assignment.delay_free()
                ),
                (
                    flags & 1 != 0,
                    flags & 2 != 0,
                    flags & 4 != 0,
                    flags & 8 != 0
                )
            );
            assert_eq!(assignment.part(4).get(), 7);
        }
        assignment.set_pending_free(false);
        assignment.set_variable_ref(true);
        assignment.set_var_ref_custom_idx(0xf123_4567);
        assignment.set_next_delayed_free_entry(ValLocalIdx::new(0x1234_5678));
        assert_eq!(assignment.frame_off(), 0xf123_4567u32 as i32);
        assert_eq!(assignment.references_left(), 0x1234_5678);
        assert!(!assignment.pending_free());
        assignment.set_frame_off(-32);
        assignment.set_references_left(9);
        assert_eq!(assignment.var_ref_custom_idx(), (-32i32) as u32);
        assert_eq!(assignment.next_delayed_free_entry(), ValLocalIdx::new(9));
        assignment.set_variable_ref(false);
        assert_eq!(assignment.size(), 40);
        assignment.set_max_part_size(0);
        assert_eq!(assignment.size(), 0);
    });
    let header = ValueAssignment::new(
        u32::MAX,
        AssignmentInit {
            max_part_size: 255,
            ..AssignmentInit::default()
        },
    );
    assert_eq!(header.size(), u32::MAX.wrapping_mul(255));
}

#[test]
fn variable_reference_size_is_rejected() {
    let panicked = catch_unwind(|| {
        ValueAssignment::new(
            1,
            AssignmentInit {
                variable_ref: true,
                ..AssignmentInit::default()
            },
        )
        .size()
    })
    .is_err();
    assert_eq!(panicked, tpde_util::CHECKED);
}

#[test]
fn zero_part_forms_and_class_zero_slow_deallocation() {
    let context = MemoryContext::new("assignment");
    with_assignment_allocator(context.mcx(), |allocator| {
        let a = allocator
            .allocate(0)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| panic!("zero parts"));
        assert!(a.parts().is_empty());
        assert_eq!(a.size(), 0);
        let pointer = a.as_ptr();
        allocator.deallocate_slow(a);
        let b = allocator
            .allocate_slow(0, false)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| panic!("zero parts"));
        assert_eq!(b.as_ptr(), pointer);
        allocator.deallocate_slow(b);
        let c = allocator
            .allocate_slow(0, true)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| panic!("zero parts"));
        assert_ne!(c.as_ptr(), pointer);
        let d = allocator
            .allocate(1)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 11);
        assert_eq!(d.as_ptr(), pointer);
    });
}

#[test]
fn lifo_and_cross_count_reuse_preserve_live_neighbors() {
    let context = MemoryContext::new("assignment");
    with_assignment_allocator(context.mcx(), |allocator| {
        let a = allocator
            .allocate(2)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 1);
        let b = allocator
            .allocate(9)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 2);
        let live = allocator
            .allocate(3)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 3);
        let pa = a.as_ptr();
        let pb = b.as_ptr();
        allocator.deallocate(a);
        allocator.deallocate(b);
        let c = allocator
            .allocate(2)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 4);
        let d = allocator
            .allocate(9)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 5);
        assert_eq!((c.as_ptr(), d.as_ptr()), (pb, pa));
        assert_eq!(d.part(8).get(), 5);
        assert_eq!(live.part(2).get(), 3);
    });
}

#[test]
fn skip_free_list_forces_fresh_fixed_variable_reference() {
    let context = MemoryContext::new("assignment");
    with_assignment_allocator(context.mcx(), |allocator| {
        let a = allocator
            .allocate(1)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 0xffff);
        let pointer = a.as_ptr();
        allocator.deallocate(a);
        let variable = allocator.allocate_slow(1, true).unwrap().initialize(
            AssignmentInit {
                variable_ref: true,
                max_part_size: 8,
                ..AssignmentInit::default()
            },
            |_| 0x3200,
        );
        assert_ne!(variable.as_ptr(), pointer);
        assert!(!variable.pending_free());
        assert!(!variable.delay_free());
        let reused = allocator
            .allocate(1)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 9);
        assert_eq!(reused.as_ptr(), pointer);
    });
}

#[test]
fn large_assignments_reuse_and_reset_retains_first_slab() {
    let context = MemoryContext::new("assignment");
    with_assignment_allocator(context.mcx(), |mut allocator| {
        let small = allocator
            .allocate(1)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 1);
        let first = small.as_ptr();
        allocator.deallocate(small);
        let large = allocator
            .allocate(10_000)
            .unwrap()
            .initialize(AssignmentInit::default(), |i| i as u16);
        let pointer = large.as_ptr();
        assert!(allocator.allocation_info(pointer).unwrap().large);
        assert_eq!(allocator.stats().large_slabs, 1);
        allocator.deallocate(large);
        assert_eq!(allocator.stats().large_slabs, 1);
        let reused = allocator
            .allocate(12_000)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 7);
        assert_eq!(reused.as_ptr(), pointer);
        allocator.deallocate(reused);
        allocator.reset();
        assert_eq!(
            allocator.stats(),
            BumpStats {
                regular_slabs: 1,
                large_slabs: 0,
                allocated_bytes: ASSIGNMENT_SLAB_SIZE
            }
        );
        let fresh = allocator
            .allocate(1)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 3);
        assert_eq!(fresh.as_ptr(), first);
        allocator.deallocate(fresh);
        allocator.reset();
    });
    assert_eq!(context.used(), 0);
}

#[test]
fn empty_and_large_only_reset() {
    let context = MemoryContext::new("assignment");
    with_assignment_allocator(context.mcx(), |mut allocator| {
        allocator.reset();
        let large = allocator
            .allocate(10_000)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 8);
        allocator.deallocate(large);
        allocator.reset();
        assert_eq!(allocator.stats().allocated_bytes, 0);
        allocator
            .allocate(1)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 1);
        assert_eq!(allocator.stats().regular_slabs, 1);
    });
}

#[test]
fn fallible_allocation_keeps_live_state_and_free_list_reusable() {
    let tiny = MemoryContext::new("assignment").with_limit(1024);
    with_assignment_allocator(tiny.mcx(), |allocator| {
        assert!(allocator.allocate(1).is_err());
        assert_eq!(allocator.stats().allocated_bytes, 0);
    });
    let context = MemoryContext::new("assignment").with_limit(20_000);
    with_assignment_allocator(context.mcx(), |allocator| {
        let live = allocator
            .allocate(1)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 42);
        for count in [
            20_000,
            MAX_PART_COUNT_EXCLUSIVE - 1,
            MAX_PART_COUNT_EXCLUSIVE,
            u32::MAX,
        ] {
            // pgrust's mcx raises an over-ceiling request as an unwind (PostgreSQL's
            // "invalid memory alloc request size" is an ERROR), where TPDE's expected
            // `AllocError`; either way the request must fail and leave the state intact.
            let refused = catch_unwind(AssertUnwindSafe(|| allocator.allocate(count).is_err()));
            assert!(matches!(refused, Ok(true) | Err(_)));
            assert_eq!(live.part(0).get(), 42);
        }
        let pointer = live.as_ptr();
        allocator.deallocate(live);
        let reused = allocator
            .allocate(1)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 15);
        assert_eq!(reused.as_ptr(), pointer);
    });
}

#[test]
fn initializer_panics_and_reentrant_allocations_leave_allocator_valid() {
    let context = MemoryContext::new("assignment");
    with_assignment_allocator(context.mcx(), |mut allocator| {
        let calls = Cell::new(0);
        let panicked = catch_unwind(AssertUnwindSafe(|| {
            allocator
                .allocate(9)
                .unwrap()
                .initialize(AssignmentInit::default(), |i| {
                    calls.set(calls.get() + 1);
                    let nested = allocator
                        .allocate(1)
                        .unwrap()
                        .initialize(AssignmentInit::default(), |_| 22);
                    assert_eq!(nested.part(0).get(), 22);
                    allocator.deallocate(nested);
                    assert_ne!(i, 4, "initializer failure");
                    i as u16
                });
        }));
        assert!(panicked.is_err());
        assert_eq!(calls.get(), 5);
        let good = allocator
            .allocate(9)
            .unwrap()
            .initialize(AssignmentInit::default(), |i| i as u16);
        assert_eq!(good.part(8).get(), 8);
        allocator.deallocate(good);
        allocator.reset();
        assert_eq!(allocator.stats().regular_slabs, 1);
    });
}

#[test]
fn compiler_table_moves_delayed_links_and_nested_brands() {
    let context = MemoryContext::new("assignment");
    with_assignment_allocator(context.mcx(), |mut allocator| {
        for _ in 0..3 {
            let mut table = SmallVec::<Option<Assignment<'_, '_>>, 2>::new(context.mcx());
            for n in 0..5 {
                table
                    .push(Some(allocator.allocate(1).unwrap().initialize(
                        AssignmentInit {
                            references_left: n + 1,
                            ..AssignmentInit::default()
                        },
                        |_| n as u16,
                    )))
                    .unwrap();
            }
            let first = table[0].as_ref().unwrap().as_ptr();
            let moved = table[0].take();
            table.push(moved).unwrap();
            let moved = table[5].as_ref().unwrap();
            moved.set_references_left(9);
            moved.set_delay_free(true);
            assert_eq!(moved.as_ptr(), first);
            assert_eq!(moved.part(0).get(), 0);
            moved.set_next_delayed_free_entry(ValLocalIdx::new(3));
            moved.set_pending_free(true);
            let value = table[5].take().unwrap();
            allocator.deallocate(value);
            with_assignment_allocator(context.mcx(), |other| {
                let separate = other
                    .allocate(1)
                    .unwrap()
                    .initialize(AssignmentInit::default(), |_| 55);
                assert_eq!(table[1].as_ref().unwrap().part(0).get(), 1);
                other.deallocate(separate);
            });
            while let Some(item) = table.pop() {
                if let Some(value) = item {
                    allocator.deallocate(value);
                }
            }
            table.release();
            allocator.reset();
            assert_eq!(allocator.stats().regular_slabs, 1);
        }
    });
}

#[cfg(tpde_address_sanitizer)]
#[test]
fn active_asan_marks_only_requested_payload_and_restores_reused_extent() {
    unsafe extern "C" {
        fn __asan_address_is_poisoned(pointer: *const u8) -> i32;
    }
    let context = MemoryContext::new("assignment");
    with_assignment_allocator(context.mcx(), |mut allocator| {
        let a = allocator
            .allocate(2)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 1);
        let pointer = a.as_ptr().cast::<u8>();
        // SAFETY: the query inspects sanitizer shadow only for this live slab's allocation.
        unsafe {
            assert_eq!(__asan_address_is_poisoned(pointer), 0);
        }
        allocator.deallocate(a);
        // SAFETY: slab remains allocated; no poisoned payload is accessed.
        unsafe {
            assert_ne!(__asan_address_is_poisoned(pointer), 0);
            assert_eq!(__asan_address_is_poisoned(pointer.add(24)), 0);
        }
        let b = allocator
            .allocate(9)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 9);
        assert_eq!(b.as_ptr().cast::<u8>(), pointer);
        assert_eq!(b.part(8).get(), 9);
        // SAFETY: reused allocation has an unpoisoned logical prefix.
        unsafe {
            assert_eq!(__asan_address_is_poisoned(pointer.add(31)), 0);
        }
        allocator.deallocate(b);
        allocator.reset();
    });
}
