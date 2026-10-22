// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use mcx::MemoryContext;
use std::cell::Cell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::vec::Vec;
use tpde_util::{SmallVec, SmallVectorGuard};
#[path = "tracked.rs"]
mod tracked;
use tracked::*;

#[test]
fn numeric_storage_all_mutating_forms() {
    let context = MemoryContext::new_bump("small-vector");
    let mut v = SmallVec::<i64, 4>::new(context.mcx());
    assert_eq!(v.capacity(), 4);
    v.append([1, 2, 3, 4].into_iter()).unwrap();
    let inline = v.as_ptr();
    v.reserve(4).unwrap();
    assert_eq!(v.as_ptr(), inline);
    v.push_clone(&5).unwrap();
    assert_eq!(v.capacity(), 9);
    *v.emplace_with(|| 6).unwrap() = 60;
    v.append_cloned([7, 8].iter()).unwrap();
    assert_eq!(&*v, [1, 2, 3, 4, 5, 60, 7, 8]);
    assert_eq!(v.erase(2..5).unwrap(), 2);
    assert_eq!(&*v, [1, 2, 60, 7, 8]);
    assert_eq!(v.erase(2..2).unwrap(), 2);
    assert_eq!(v.pop(), Some(8));
    v.pop_back().unwrap();
    v.resize(5, -1).unwrap();
    assert_eq!(&*v, [1, 2, 60, -1, -1]);
    v.resize_default(7).unwrap();
    assert_eq!(&*v, [1, 2, 60, -1, -1, 0, 0]);
    v.truncate(2);
    v.extend_from_slice(&[19, 20]).unwrap();
    v.append_forward((0..10).filter(|n| n % 3 == 0)).unwrap();
    v.append_cloned_forward([30, 31, 32, 33].iter().filter(|n| **n % 2 == 0))
        .unwrap();
    assert_eq!(&*v, [1, 2, 19, 20, 0, 3, 6, 9, 30, 32]);
    let capacity = v.capacity();
    v.clear();
    assert!(v.is_empty());
    assert_eq!(v.capacity(), capacity);
    v.release();
    assert_eq!(v.capacity(), 4);
    assert!(!core::mem::needs_drop::<SmallVec<'_, i64, 4>>());
}

#[test]
fn forward_move_admits_capacity_before_moving_each_resource_once() {
    struct MoveOnly(Tracked);
    tracked_scope(|stats| {
        let context = MemoryContext::new("small-vector-forward-move").with_limit(64);
        let source: [Cell<Option<MoveOnly>>; 5] = core::array::from_fn(|index| {
            Cell::new(Some(MoveOnly(Tracked::new(index as i64 + 10))))
        });
        let mut destination = SmallVectorGuard::<MoveOnly, 1>::new(context.mcx());
        let mut calls = 0;
        stats.clone_budget.set(Some(0));
        assert!(destination
            .append_forward_with(source.iter(), |slot| {
                calls += 1;
                slot.take().unwrap()
            })
            .is_err());
        assert_eq!(calls, 0);
        assert_eq!(destination.capacity(), 1);
        assert!(destination.is_empty());
        assert_eq!(stats.created.get(), 5);
        assert_eq!(stats.released.get(), 0);
        for slot in &source {
            let value = slot.take();
            assert!(value.is_some());
            slot.set(value);
        }
        destination
            .append_forward_with(
                source
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| index % 2 == 0),
                |(_, slot)| {
                    calls += 1;
                    slot.take().unwrap()
                },
            )
            .unwrap();
        assert_eq!(calls, 3);
        assert_eq!(
            destination
                .iter()
                .map(|v| *v.0.resource)
                .collect::<Vec<_>>(),
            [10, 12, 14]
        );
        assert_eq!(stats.created.get(), 5);
        assert_eq!(stats.released.get(), 0);
        for (index, slot) in source.iter().enumerate() {
            let value = slot.take();
            assert_eq!(value.is_some(), index % 2 != 0);
            if let Some(value) = value {
                assert_eq!(*value.0.resource, index as i64 + 10);
            }
        }
        assert_eq!(stats.released.get(), 2);
        drop(destination);
        assert_eq!(stats.released.get(), 5);
    });
}

#[test]
fn resource_guards_release_each_value_once() {
    tracked_scope(|stats| {
        let context = MemoryContext::new("small-vector");
        {
            let mut v = SmallVectorGuard::<Tracked, 2>::new(context.mcx());
            v.append((0..8).map(|n| Tracked::new(i64::from(n))))
                .unwrap();
            assert_eq!(stats.created.get() - stats.released.get(), 8);
            v.erase(2..5).unwrap();
            assert_eq!(
                v.iter().map(|v| *v.resource).collect::<Vec<_>>(),
                [0, 1, 5, 6, 7]
            );
            assert_eq!(stats.released_values.borrow().as_slice(), [2, 3, 4]);
            let popped = v.pop().unwrap();
            assert_eq!(stats.released.get(), 3);
            drop(popped);
            v.truncate(2);
            let mut moved =
                SmallVectorGuard::<Tracked, 0>::move_from(context.mcx(), &mut v).unwrap();
            assert!(v.is_empty());
            v.push(Tracked::new(8)).unwrap();
            moved.move_assign(&mut v).unwrap();
            moved.release();
            assert_eq!(moved.capacity(), 0);
            assert_eq!(stats.created.get(), stats.released.get());
        }
        let mut released = stats.released_values.borrow().clone();
        released.sort_unstable();
        assert_eq!(released, (0..9).collect::<Vec<_>>());
    });
    tracked_scope(|stats| {
        let context = MemoryContext::new("small-vector-forward-copy");
        let source: Vec<_> = (3..8).map(|n| Tracked::new(i64::from(n))).collect();
        let mut v = SmallVectorGuard::<Tracked, 0>::new(context.mcx());
        stats.clone_budget.set(Some(2));
        v.append_cloned_forward(source.iter().filter(|v| *v.resource & 1 == 0))
            .unwrap();
        assert_eq!(stats.clone_budget.get(), Some(0));
        assert_eq!(stats.created.get(), 7);
        assert_eq!(stats.released.get(), 0);
        assert_eq!(v.iter().map(|v| *v.resource).collect::<Vec<_>>(), [4, 6]);
        assert_eq!(
            source.iter().map(|v| *v.resource).collect::<Vec<_>>(),
            [3, 4, 5, 6, 7]
        );
        drop(source);
        assert_eq!(stats.released.get(), 5);
        drop(v);
        assert_eq!(
            stats.released_values.borrow().as_slice(),
            [3, 4, 5, 6, 7, 4, 6]
        );
    });
}

#[test]
fn constructor_panics_keep_only_initialized_prefix() {
    tracked_scope(|stats| {
        let context = MemoryContext::new("small-vector");
        let mut v = SmallVectorGuard::<Tracked, 1>::new(context.mcx());
        v.push(Tracked::new(1)).unwrap();
        stats.clone_budget.set(Some(2));
        assert!(catch_unwind(AssertUnwindSafe(|| v.resize(6, Tracked::new(9)).unwrap())).is_err());
        assert_eq!(v.iter().map(|v| *v.resource).collect::<Vec<_>>(), [1, 9, 9]);
        assert_eq!(stats.created.get() - stats.released.get(), v.len());
        stats.clone_budget.set(None);
        stats.default_budget.set(Some(1));
        assert!(catch_unwind(AssertUnwindSafe(|| v.resize_default(7).unwrap())).is_err());
        assert_eq!(
            v.iter().map(|v| *v.resource).collect::<Vec<_>>(),
            [1, 9, 9, 0]
        );
        assert_eq!(stats.created.get() - stats.released.get(), v.len());
        stats.default_budget.set(None);
        assert!(catch_unwind(AssertUnwindSafe(|| {
            v.emplace_with(|| panic!("emplace failure")).unwrap();
        }))
        .is_err());
        assert_eq!(v.len(), 4);
        let source = [Tracked::new(10), Tracked::new(11), Tracked::new(12)];
        stats.clone_budget.set(Some(1));
        assert!(
            catch_unwind(AssertUnwindSafe(|| v.append_cloned(source.iter()).unwrap())).is_err()
        );
        assert_eq!(v.len(), 5);
        assert_eq!(*v.last().unwrap().resource, 10);
        stats.clone_budget.set(None);
        v.clear();
        v.push(Tracked::new(99)).unwrap();
        drop(source);
        drop(v);
    });
}

#[test]
fn destructor_panic_repairs_erased_tail_without_double_drop() {
    tracked_scope(|stats| {
        let context = MemoryContext::new("small-vector");
        let mut v = SmallVectorGuard::<Tracked, 2>::new(context.mcx());
        v.append((0..6).map(|n| Tracked::new(i64::from(n))))
            .unwrap();
        stats.panic_drop.set(Some(2));
        assert!(catch_unwind(AssertUnwindSafe(|| v.erase(1..4).unwrap())).is_err());
        assert_eq!(v.iter().map(|v| *v.resource).collect::<Vec<_>>(), [0, 4, 5]);
        assert_eq!(stats.released_values.borrow().as_slice(), [1, 2, 3]);
        v.push(Tracked::new(6)).unwrap();
        drop(v);
        let mut released = stats.released_values.borrow().clone();
        released.sort_unstable();
        assert_eq!(released, (0..7).collect::<Vec<_>>());
    });
    tracked_scope(|stats| {
        let context = MemoryContext::new("small-vector-release-panic");
        let mut v = SmallVectorGuard::<Tracked, 0>::new(context.mcx());
        v.append((0..4).map(|n| Tracked::new(i64::from(n))))
            .unwrap();
        stats.panic_drop.set(Some(1));
        assert!(catch_unwind(AssertUnwindSafe(|| v.release())).is_err());
        assert!(v.is_empty());
        assert_eq!(v.capacity(), 0);
        assert_eq!(context.stats().used, 0);
        assert_eq!(stats.released_values.borrow().as_slice(), [0, 1, 2, 3]);
        v.push(Tracked::new(4)).unwrap();
        drop(v);
        assert_eq!(stats.released_values.borrow().as_slice(), [0, 1, 2, 3, 4]);
        assert_eq!(context.stats().used, 0);
    });
}

#[test]
fn inline_erase_shifts_trivial_suffix_without_spilling() {
    let context = MemoryContext::new("small-vector-inline-erase");
    let mut v = SmallVec::<u64, 8>::new(context.mcx());
    v.append([0, 1, 2, 3, 4, 5].into_iter()).unwrap();
    let pointer = v.as_ptr();
    assert_eq!(v.erase(1..3).unwrap(), 1);
    assert_eq!(&*v, &[0, 3, 4, 5]);
    assert_eq!(v.as_ptr(), pointer);
    assert_eq!(v.capacity(), 8);
    assert_eq!(context.stats().used, 0);
    v[1] = 30;
    assert_eq!(v.erase(0..1).unwrap(), 0);
    assert_eq!(&*v, &[30, 4, 5]);
    assert_eq!(v.as_ptr(), pointer);
}

#[test]
fn inline_erase_preserves_resource_suffix_and_unwind_cleanup() {
    tracked_scope(|stats| {
        let context = MemoryContext::new("small-vector-inline-resource-erase");
        let mut v = SmallVectorGuard::<Tracked, 8>::new(context.mcx());
        v.append((0..6).map(|n| Tracked::new(i64::from(n))))
            .unwrap();
        let pointer = v.as_ptr();
        assert_eq!(v.erase(1..3).unwrap(), 1);
        assert_eq!(
            v.iter().map(|v| *v.resource).collect::<Vec<_>>(),
            [0, 3, 4, 5]
        );
        assert_eq!(stats.released_values.borrow().as_slice(), [1, 2]);
        assert_eq!(v.as_ptr(), pointer);
        assert_eq!(v.capacity(), 8);
        assert_eq!(context.stats().used, 0);
        stats.panic_drop.set(Some(3));
        assert!(catch_unwind(AssertUnwindSafe(|| v.erase(1..2).unwrap())).is_err());
        assert_eq!(v.iter().map(|v| *v.resource).collect::<Vec<_>>(), [0, 4, 5]);
        assert_eq!(stats.released_values.borrow().as_slice(), [1, 2, 3]);
        assert_eq!(v.as_ptr(), pointer);
        drop(v);
        let mut released = stats.released_values.borrow().clone();
        released.sort_unstable();
        assert_eq!(released, (0..6).collect::<Vec<_>>());
    });
}
