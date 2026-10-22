// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use super::*;
use mcx::MemoryContext;
use std::panic::{catch_unwind, AssertUnwindSafe};
use tpde_core::{
    value_assignment::{with_assignment_allocator, AssignmentInit},
    Reg, RegBank,
};

#[test]
fn slot_views_and_recycled_tokens_share_the_original_allocation() {
    let context = MemoryContext::new("compiler-assignment-slots");
    let mut storage = AssignmentSlotStorage::new(context.mcx());
    with_assignment_allocator(context.mcx(), |allocator| {
        storage
            .with_slots(&allocator, 4, |mut slots| {
                let first = allocator.allocate(3).unwrap().initialize(
                    AssignmentInit {
                        references_left: 2,
                        max_part_size: 8,
                        ..AssignmentInit::default()
                    },
                    |_| 0x0200,
                );
                let pointer = first.as_ptr();
                slots.insert(1, first);
                let part = slots.part(1, 2).unwrap();
                part.set_bank(RegBank::new(1));
                part.set_reg(Reg::new(35));
                assert_eq!(slots.get(1).unwrap().part(2).get(), 0x0223);
                assert_eq!(part.part_off(), 16);
                assert!(core::ptr::eq(part.assignment(), pointer));
                assert_eq!(slots.occupied(), 1);
                assert!(slots.get(0).is_none());
                let first = slots.take(1).unwrap();
                assert_eq!(first.as_ptr(), pointer);
                assert!(slots.take(1).is_none());
                allocator.deallocate(first);
                let second = allocator
                    .allocate(3)
                    .unwrap()
                    .initialize(AssignmentInit::default(), |_| 71);
                assert_eq!(second.as_ptr(), pointer);
                slots.insert(3, second);
                assert_eq!(slots.get(3).unwrap().part(2).get(), 71);
                slots.discard(3);
                assert_eq!(slots.occupied(), 0);
            })
            .unwrap();
    });
}

#[test]
fn moving_an_alias_transfers_one_token_and_invalid_moves_preserve_it() {
    let context = MemoryContext::new("compiler-assignment-slots");
    let mut storage = AssignmentSlotStorage::new(context.mcx());
    with_assignment_allocator(context.mcx(), |allocator| {
        storage
            .with_slots(&allocator, 3, |mut slots| {
                let assignment = allocator.allocate(1).unwrap().initialize(
                    AssignmentInit {
                        references_left: 1,
                        ..AssignmentInit::default()
                    },
                    |_| 42,
                );
                let pointer = assignment.as_ptr();
                slots.insert(0, assignment);
                slots.move_slot(0, 2);
                assert!(slots.get(0).is_none());
                assert_eq!(slots.get(2).unwrap().as_ptr(), pointer);
                slots.move_slot(2, 2);
                assert_eq!(slots.occupied(), 1);
                if tpde_util::CHECKED {
                    assert!(catch_unwind(AssertUnwindSafe(|| slots.move_slot(2, 3))).is_err());
                    assert!(catch_unwind(AssertUnwindSafe(|| slots.move_slot(0, 1))).is_err());
                }
                assert_eq!(slots.get(2).unwrap().part(0).get(), 42);
                allocator.deallocate(slots.take(2).unwrap());
                assert_eq!(slots.occupied(), 0);
            })
            .unwrap();
    });
}

#[test]
fn spilled_capacity_survives_function_reset_owner_move_and_repeated_reuse() {
    for bump in [false, true] {
        let mut context = if bump {
            MemoryContext::new_bump("compiler-assignment-slots")
        } else {
            MemoryContext::new("compiler-assignment-slots")
        };
        let mut storage = AssignmentSlotStorage::new(context.mcx());
        let count = SMALL_VALUE_NUM + 17;
        let (keeper, used) = with_assignment_allocator(context.mcx(), |mut allocator| {
            let keeper = storage
                .with_slots(&allocator, count, |mut slots| {
                    let keeper = context.stats().arena_footprint;
                    for index in [0, count / 2, count - 1] {
                        slots.insert(
                            index,
                            allocator
                                .allocate(1)
                                .unwrap()
                                .initialize(AssignmentInit::default(), |_| index as u16),
                        );
                    }
                    slots.clear();
                    assert_eq!(slots.len(), count);
                    assert_eq!(slots.occupied(), 0);
                    keeper
                })
                .unwrap();
            allocator.reset();
            let capacity = storage.capacity();
            let used = context.used();
            let mut moved = storage;
            for next_count in [count - 1, 1, 0, count] {
                moved
                    .with_slots(&allocator, next_count, |mut slots| {
                        assert_eq!(slots.len(), next_count);
                        assert_eq!(slots.is_empty(), next_count == 0);
                        for index in 0..next_count {
                            assert!(slots.get(index).is_none());
                        }
                        if next_count != 0 {
                            slots.insert(
                                next_count - 1,
                                slots
                                    .allocator()
                                    .allocate(1)
                                    .unwrap()
                                    .initialize(AssignmentInit::default(), |_| 99),
                            );
                            allocator.deallocate(slots.take(next_count - 1).unwrap());
                        }
                    })
                    .unwrap();
                allocator.reset();
                assert_eq!(moved.capacity(), capacity);
                assert_eq!(context.used(), used);
            }
            moved.release();
            assert_eq!(moved.capacity(), SMALL_VALUE_NUM);
            if bump {
                assert!(used > keeper);
                assert_eq!(context.used(), used);
            }
            (keeper, used)
        });
        if bump {
            assert_eq!(context.used(), used);
            context.reset();
            assert_eq!(context.used(), keeper);
            assert_eq!(context.stats().arena_footprint, keeper);
        } else {
            assert_eq!(context.used(), 0);
        }
    }
}

#[test]
fn forgotten_and_unwinding_loans_never_reconstruct_expired_tokens() {
    let context = MemoryContext::new("compiler-assignment-slots");
    let mut storage = AssignmentSlotStorage::new(context.mcx());
    let count = SMALL_VALUE_NUM + 1;
    let panic = catch_unwind(AssertUnwindSafe(|| {
        with_assignment_allocator(context.mcx(), |allocator| {
            let _ = storage.with_slots(&allocator, count, |mut slots| {
                slots.insert(
                    count - 1,
                    allocator
                        .allocate(1)
                        .unwrap()
                        .initialize(AssignmentInit::default(), |_| 0xbeef),
                );
                core::mem::forget(slots);
                panic!("abort after forgetting the assignment loan");
            });
        });
    }));
    assert!(panic.is_err());
    with_assignment_allocator(context.mcx(), |mut allocator| {
        storage
            .with_slots(&allocator, count, |mut slots| {
                assert_eq!(slots.occupied(), 0);
                assert!(slots.get(count - 1).is_none());
                slots.insert(
                    0,
                    allocator
                        .allocate(1)
                        .unwrap()
                        .initialize(AssignmentInit::default(), |_| 17),
                );
                core::mem::forget(slots);
            })
            .unwrap();
        allocator.reset();
        storage
            .with_slots(&allocator, count, |slots| {
                assert_eq!(slots.occupied(), 0);
                assert!(slots.get(0).is_none());
                assert!(slots.get(count - 1).is_none());
            })
            .unwrap();
    });
    storage.release();
    assert_eq!(context.used(), 0);
}

#[test]
fn panic_cleanup_and_clear_discard_without_changing_allocator_recycle_order() {
    let context = MemoryContext::new("compiler-assignment-slots");
    let mut storage = AssignmentSlotStorage::new(context.mcx());
    with_assignment_allocator(context.mcx(), |mut allocator| {
        let before = allocator.stats();
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = storage.with_slots(&allocator, 2, |mut slots| {
                slots.insert(
                    0,
                    allocator
                        .allocate(1)
                        .unwrap()
                        .initialize(AssignmentInit::default(), |_| 1),
                );
                panic!("abort with an assignment still present");
            });
        }));
        assert!(panic.is_err());
        assert!(allocator.stats().allocated_bytes > before.allocated_bytes);
        storage
            .with_slots(&allocator, 2, |mut slots| {
                assert!(slots.get(0).is_none());
                let value = allocator
                    .allocate(1)
                    .unwrap()
                    .initialize(AssignmentInit::default(), |_| 2);
                let discarded = value.as_ptr();
                slots.insert(0, value);
                slots.clear();
                assert_eq!(slots.len(), 2);
                let next = allocator
                    .allocate(1)
                    .unwrap()
                    .initialize(AssignmentInit::default(), |_| 3);
                assert_ne!(next.as_ptr(), discarded);
                slots.insert(1, next);
            })
            .unwrap();
        allocator.reset();
        storage
            .with_slots(&allocator, 2, |slots| {
                assert!(slots.get(0).is_none());
                assert!(slots.get(1).is_none());
            })
            .unwrap();
    });
}

#[test]
fn reservation_failure_precedes_the_callback_and_preserves_reusability() {
    let context = MemoryContext::new("compiler-assignment-slots").with_limit(20_000);
    let mut storage = AssignmentSlotStorage::new(context.mcx());
    with_assignment_allocator(context.mcx(), |mut allocator| {
        storage
            .with_slots(&allocator, 2, |mut slots| {
                slots.insert(
                    0,
                    allocator
                        .allocate(1)
                        .unwrap()
                        .initialize(AssignmentInit::default(), |_| 77),
                );
                assert!(allocator.allocate(100_000).is_err());
                assert_eq!(slots.get(0).unwrap().part(0).get(), 77);
                allocator.deallocate(slots.take(0).unwrap());
            })
            .unwrap();
        allocator.reset();
        let used = context.used();
        let mut called = false;
        assert!(storage
            .with_slots(&allocator, 10_000, |_| called = true)
            .is_err());
        assert!(!called);
        assert_eq!(context.used(), used);
        storage
            .with_slots(&allocator, 2, |slots| {
                assert_eq!(slots.occupied(), 0);
                assert!(slots.get(0).is_none());
            })
            .unwrap();
    });
}
