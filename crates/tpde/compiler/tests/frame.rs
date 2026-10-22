// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use super::*;
use mcx::MemoryContext;
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    vec::Vec,
};

fn initialized(context: &MemoryContext, base: u32) -> FrameState<'_> {
    let mut frame = FrameState::new(context.mcx());
    frame.begin_function(false, true);
    frame.frame_size = base;
    frame
}

#[test]
fn alignment_holes_are_reused_in_both_frame_directions() {
    for negative in [false, true] {
        let context = MemoryContext::new("frame-holes");
        let mut frame = initialized(&context, 1);
        assert_eq!(
            frame.allocate(16, negative).unwrap(),
            if negative { -32 } else { 16 }
        );
        assert_eq!(frame.frame_size, 32);
        for (size, positive, backwards) in [(1, 1, -2), (2, 2, -4), (3, 4, -8), (8, 8, -16)] {
            assert_eq!(
                frame.allocate(size, negative).unwrap(),
                if negative { backwards } else { positive }
            );
            assert_eq!(frame.frame_size, 32);
        }
        assert!(frame
            .fixed
            .iter()
            .all(|list| list.issued == 1 && list.slots.is_empty()));
        assert_eq!(context.stats().used, 0);
    }
}

/// Upstream `LLVMCompilerX64::fp80_to_int` frees its 4-byte slot with size 2:
/// the slot must become a 2-byte reuse candidate (and not a 4-byte one), the
/// per-class accounting must stay exact, and a plain `free(slot, 2)` of a
/// 4-byte slot must still be refused.
#[test]
fn reclassified_free_moves_the_slot_between_fixed_classes() {
    let context = MemoryContext::new("frame-reclassify");
    let mut frame = initialized(&context, 16);
    let four = frame.allocate(4, true).unwrap();
    assert_eq!(four, -20);
    frame.free_reclassified(four, 4, 2).unwrap();
    assert_eq!(frame.fixed[2].issued, 0);
    assert_eq!(frame.fixed[1].issued, 1);
    // A new 4-byte request does not see the slot (it went to the 2-byte class).
    assert_eq!(frame.allocate(4, true).unwrap(), -24);
    // A 2-byte request reuses it.
    assert_eq!(frame.allocate(2, true).unwrap(), four);
    frame.free(four, 2);
    // The guard still refuses a misclassified plain free.
    let other = frame.allocate(4, true).unwrap();
    if tpde_util::CHECKED {
        let refused = catch_unwind(AssertUnwindSafe(|| {
            let mut frame = initialized(&context, 16);
            let slot = frame.allocate(4, true).unwrap();
            frame.free(slot, 2);
        }));
        assert!(refused.is_err());
    }
    frame.free(other, 4);
    // Same-class reclassification is a plain free.
    let slot = frame.allocate(8, true).unwrap();
    frame.free_reclassified(slot, 8, 5).unwrap();
    assert_eq!(frame.allocate(8, true).unwrap(), slot);
}

#[test]
fn every_fixed_class_and_dynamic_rounding_boundary_has_exact_offsets() {
    let cases = [
        (1, 16, 17),
        (2, 18, 20),
        (3, 20, 24),
        (4, 24, 28),
        (5, 32, 40),
        (8, 40, 48),
        (9, 48, 64),
        (15, 64, 80),
        (16, 80, 96),
        (17, 96, 128),
        (31, 128, 160),
        (32, 160, 192),
        (33, 192, 240),
        (63, 240, 304),
        (64, 304, 368),
        (65, 368, 448),
    ];
    for negative in [false, true] {
        let context = MemoryContext::new("frame-rounding");
        let mut frame = initialized(&context, 16);
        for (size, start, end) in cases {
            assert_eq!(
                frame.allocate(size, negative).unwrap(),
                if negative { -(end as i32) } else { start }
            );
            assert_eq!(frame.frame_size, end);
        }
        assert_eq!(frame.dynamic.len(), 4);
        assert!(frame.frame_used);
    }
}

#[test]
fn free_lists_are_lifo_and_share_equivalent_size_classes() {
    for negative in [false, true] {
        let context = MemoryContext::new("frame-lifo");
        let mut frame = initialized(&context, 16);
        let slots = [
            frame.allocate(17, negative).unwrap(),
            frame.allocate(31, negative).unwrap(),
            frame.allocate(32, negative).unwrap(),
        ];
        for (slot, size) in slots.into_iter().zip([17, 31, 32]) {
            frame.free(slot, size);
        }
        let used = context.stats().used;
        frame.frame_size = u32::MAX;
        for (slot, size) in slots.into_iter().rev().zip([17, 31, 32]) {
            assert_eq!(frame.allocate(size, negative).unwrap(), slot);
        }
        assert_eq!(frame.frame_size, u32::MAX);
        assert_eq!(context.stats().used, used);
        frame.frame_size = 16;
        let slot = frame.allocate(3, negative).unwrap();
        frame.free(slot, 4);
        frame.frame_size = u32::MAX;
        assert_eq!(frame.allocate(4, negative).unwrap(), slot);
    }
}

#[test]
fn zero_size_preserves_frame_and_marks_it_used_before_initialization() {
    let context = MemoryContext::new("frame-zero").with_limit(0);
    let mut frame = FrameState::new(context.mcx());
    assert!(!frame.frame_used);
    assert_eq!(frame.allocate(0, false).unwrap(), 0);
    assert!(frame.frame_used);
    assert_eq!(frame.frame_size, 0);
    frame.begin_function(true, false);
    assert_eq!(frame.allocate(0, true).unwrap(), 0);
    frame.free(0, 0);
    assert_eq!(frame.frame_size, u32::MAX);
    assert_eq!(context.stats().used, 0);
    if tpde_util::CHECKED {
        assert!(catch_unwind(AssertUnwindSafe(|| frame.free(1, 0))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| frame.allocate(1, false).unwrap())).is_err());
    }
}

#[test]
fn release_never_allocates_after_an_admitted_fixed_allocation() {
    let context = MemoryContext::new("frame-fixed-budget").with_limit(0);
    let mut frame = initialized(&context, 16);
    let slots: Vec<_> = (0..16).map(|_| frame.allocate(1, false).unwrap()).collect();
    assert_eq!(frame.frame_size, 32);
    for _ in 0..3 {
        assert!(frame.allocate(1, false).is_err());
        assert_eq!(frame.frame_size, 32);
        assert_eq!(frame.fixed[0].issued, 16);
    }
    for slot in &slots {
        frame.free(*slot, 1);
    }
    assert_eq!(context.stats().used, 0);
    for slot in slots.iter().rev() {
        assert_eq!(frame.allocate(1, false).unwrap(), *slot);
    }
    assert_eq!(frame.frame_size, 32);
}

#[test]
fn failed_hole_reservation_leaves_all_issued_counts_and_frame_unchanged() {
    let context = MemoryContext::new("frame-hole-budget").with_limit(0);
    let mut frame = initialized(&context, 16);
    for _ in 0..16 {
        frame.allocate(1, false).unwrap();
    }
    frame.frame_size = 33;
    for _ in 0..3 {
        assert!(frame.allocate(16, false).is_err());
        assert_eq!(frame.frame_size, 33);
        assert_eq!(frame.fixed[0].issued, 16);
        assert!(frame.fixed[1..]
            .iter()
            .all(|list| list.issued == 0 && list.slots.is_empty()));
    }
    frame.begin_function(false, true);
    frame.frame_size = 33;
    assert_eq!(frame.allocate(16, false).unwrap(), 48);
    assert_eq!(frame.frame_size, 64);
}

#[test]
fn dynamic_map_and_vector_failures_are_reusable_and_do_not_publish_slots() {
    let denied = MemoryContext::new("frame-map-budget").with_limit(0);
    let mut frame = initialized(&denied, 16);
    assert!(frame.allocate(17, false).is_err());
    assert!(frame.dynamic.is_empty());
    assert_eq!(frame.frame_size, 16);
    let probe = MemoryContext::new("frame-map-probe");
    let map_bytes = {
        let mut frame = initialized(&probe, 16);
        frame.dynamic.try_reserve(1).unwrap();
        probe.stats().used
    };
    assert!(map_bytes > 0);
    assert_eq!(probe.stats().used, 0);
    let limited = MemoryContext::new("frame-vector-budget").with_limit(map_bytes);
    let mut frame = initialized(&limited, 16);
    for _ in 0..3 {
        assert!(frame.allocate(17, false).is_err());
        assert_eq!(frame.frame_size, 16);
        assert_eq!(frame.dynamic.len(), 1);
        let list = frame.dynamic.get(&32).unwrap();
        assert_eq!(list.issued, 0);
        assert_eq!(list.slots.capacity(), 0);
        assert_eq!(limited.stats().used, map_bytes);
    }
    assert_eq!(frame.allocate(8, false).unwrap(), 16);
}

#[test]
fn retained_buffers_stabilize_across_bump_context_function_reuse() {
    let context = MemoryContext::new_bump("frame-retained");
    let mut frame = initialized(&context, 16);
    let mut baseline = None;
    for _ in 0..3 {
        let mut slots = Vec::new();
        for size in [1, 3, 8, 16, 17, 33] {
            for _ in 0..40 {
                slots.push((frame.allocate(size, false).unwrap(), size));
            }
        }
        let before_free = context.stats().used;
        for (slot, size) in slots {
            frame.free(slot, size);
        }
        assert_eq!(context.stats().used, before_free);
        let actual = (context.stats().used, context.stats().arena_footprint);
        if let Some(expected) = baseline {
            assert_eq!(actual, expected);
        } else {
            baseline = Some(actual);
        }
        frame.begin_function(true, false);
        frame.frame_size = 16;
        assert!(frame
            .fixed
            .iter()
            .all(|list| list.issued == 0 && list.slots.is_empty()));
        assert!(frame
            .dynamic
            .values()
            .all(|list| list.issued == 0 && list.slots.is_empty()));
    }
}

#[test]
fn checked_overflow_preserves_slots_and_admits_boundary_bit_patterns() {
    let context = MemoryContext::new("frame-overflow");
    let mut frame = initialized(&context, 16);
    for size in [u32::MAX, u32::MAX - 14] {
        assert!(frame.allocate(size, false).is_err());
        assert_eq!(frame.frame_size, 16);
        assert!(frame.dynamic.is_empty());
    }
    frame.frame_size = u32::MAX - 1;
    assert!(frame.allocate(2, false).is_err());
    assert_eq!(frame.frame_size, u32::MAX - 1);
    assert!(frame.fixed.iter().all(|list| list.issued == 0));
    assert_eq!(frame.allocate(1, false).unwrap(), -2);
    assert_eq!(frame.frame_size, u32::MAX);
    frame.begin_function(false, false);
    frame.frame_size = i32::MAX as u32;
    assert_eq!(frame.allocate(1, true).unwrap(), i32::MIN);
    frame.fixed[0].issued = usize::MAX;
    assert!(frame.allocate(1, true).is_err());
    assert_eq!(frame.frame_size, 1 << 31);
}

#[test]
fn reset_retains_storage_and_begin_function_resets_only_function_flags() {
    let context = MemoryContext::new("frame-reset");
    let mut frame = initialized(&context, 16);
    let slot = frame.allocate(32, false).unwrap();
    frame.free(slot, 32);
    frame.generated_call = true;
    let used = context.stats().used;
    frame.reset();
    assert_eq!(frame.frame_size, 48);
    assert!(frame.generated_call && frame.is_leaf_function && frame.frame_used);
    assert_eq!(frame.dynamic.len(), 1);
    assert_eq!(context.stats().used, used);
    frame.begin_function(true, false);
    assert_eq!(frame.frame_size, u32::MAX);
    assert!(frame.has_dynamic_alloca);
    assert!(!frame.generated_call && !frame.is_leaf_function && !frame.frame_used);
    assert_eq!(context.stats().used, used);
}

#[test]
fn owner_drop_and_unwind_release_both_fixed_and_dynamic_buffers() {
    for unwind in [false, true] {
        let context = MemoryContext::new("frame-drop");
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut frame = initialized(&context, 16);
            for _ in 0..24 {
                frame.allocate(8, false).unwrap();
                frame.allocate(32, false).unwrap();
            }
            assert!(context.stats().used > 0);
            if unwind {
                panic!("injected compiler abort");
            }
        }));
        assert_eq!(result.is_err(), unwind);
        assert_eq!(context.stats().used, 0);
    }
}
