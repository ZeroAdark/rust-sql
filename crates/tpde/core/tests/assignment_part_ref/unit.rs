// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use super::*;
use crate::value_assignment::{with_assignment_allocator, AssignmentInit};
use mcx::MemoryContext;
use std::panic::{catch_unwind, AssertUnwindSafe};

#[test]
fn shared_views_observe_same_word_and_reset_exactly() {
    let context = MemoryContext::new("assignment-part");
    with_assignment_allocator(context.mcx(), |allocator| {
        let assignment = allocator
            .allocate(3)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 0xffff);
        let first = AssignmentPartRef::new(&assignment, 1);
        let copy = first;
        let other = AssignmentPartRef::new(&assignment, 2);
        first.reset();
        assert_eq!(assignment.part(1).get(), 0x0200);
        assert_eq!(assignment.part(0).get(), 0xffff);
        assert_eq!(other.get_reg(), Reg::new(255));
        assert!(copy.modified());
        assert!(!copy.stack_valid());
        assert!(!copy.register_valid());
        assert!(!copy.fixed_assignment());
        assert_eq!((copy.bank().id(), copy.part_size()), (0, 1));
        copy.set_bank(RegBank::new(7));
        copy.set_reg(Reg::new(255));
        assert_eq!(first.get_reg(), Reg::new(255));
        assert!(core::ptr::eq(first.assignment(), &*assignment));
        first.assignment().set_max_part_size(16);
        assert_eq!(other.part_off(), 32);
    });
}

#[test]
fn setters_preserve_reserved_bits_and_unrelated_fields() {
    let context = MemoryContext::new("assignment-part");
    with_assignment_allocator(context.mcx(), |allocator| {
        let assignment = allocator
            .allocate(1)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 0xa55a);
        let part = AssignmentPartRef::new(&assignment, 0);
        for bank in 0..8 {
            assignment.part(0).set(0xa55a);
            part.set_bank(RegBank::new(bank));
            assert_eq!(
                assignment.part(0).get(),
                (0xa55a & !0xe0) | (u16::from(bank) << 5)
            );
            for low in [0, 1, 15, 31] {
                let before = assignment.part(0).get();
                part.set_reg(Reg::new(u64::from(bank) * 32 + low));
                assert_eq!(assignment.part(0).get() & 0xff00, before & 0xff00);
                assert_eq!(part.bank().id(), bank);
            }
        }
        for size in [1, 2, 4, 8, 16, 32, 64, 128] {
            assignment.part(0).set(0x8c73);
            part.set_part_size(size);
            assert_eq!(part.part_size(), size);
            assert_eq!(assignment.part(0).get() & !0x7000, 0x8c73);
        }
    });
}

#[test]
fn all_register_and_stack_validity_states_are_independent() {
    let context = MemoryContext::new("assignment-part");
    with_assignment_allocator(context.mcx(), |allocator| {
        let assignment = allocator
            .allocate(1)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 0xf4a3);
        let part = AssignmentPartRef::new(&assignment, 0);
        for state in 0..8 {
            part.set_modified(state & 1 != 0);
            part.set_register_valid(state & 2 != 0);
            part.set_fixed_assignment(state & 4 != 0);
            assert_eq!(
                (
                    part.modified(),
                    part.stack_valid(),
                    part.register_valid(),
                    part.fixed_assignment()
                ),
                (
                    state & 1 != 0,
                    state & 1 == 0,
                    state & 2 != 0,
                    state & 4 != 0
                )
            );
            assert_eq!(
                assignment.part(0).get() & !(0x100 | 0x200 | 0x800),
                0xf4a3 & !(0x100 | 0x200 | 0x800)
            );
            let before = assignment.part(0).get();
            part.set_stack_valid();
            assert_eq!(assignment.part(0).get(), before & !0x200);
        }
    });
}

#[test]
fn full_unsigned_index_domain_and_signed_frame_conversion() {
    let context = MemoryContext::new("assignment-part");
    with_assignment_allocator(context.mcx(), |allocator| {
        let assignment = allocator
            .allocate(0)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| unreachable!());
        for size in [0, 1, 8, 128, 255] {
            assignment.set_max_part_size(size);
            for index in [0, 1, 2, 0x0101_0102, 0x7fff_ffff, 0x8000_0000, u32::MAX] {
                let part = AssignmentPartRef::new(&assignment, index);
                let offset = ((u64::from(size) * u64::from(index)) & 0xffff_ffff) as u32;
                assert_eq!(part.part_off(), offset);
                for base in [i32::MIN, -1, 1, i32::MAX] {
                    assignment.set_frame_off(base);
                    let expected =
                        ((i64::from(base) + i64::from(offset)) & 0xffff_ffff) as u32 as i32;
                    assert_eq!(part.frame_off(), expected);
                }
            }
        }
        assignment.set_max_part_size(1);
        assignment.set_frame_off(-1);
        assert_eq!(AssignmentPartRef::new(&assignment, 1).frame_off(), 0);
    });
}

#[test]
fn variable_reference_headers_need_no_backing_part_word() {
    let context = MemoryContext::new("assignment-part");
    with_assignment_allocator(context.mcx(), |allocator| {
        let assignment = allocator.allocate(0).unwrap().initialize(
            AssignmentInit {
                variable_ref: true,
                ..AssignmentInit::default()
            },
            |_| unreachable!(),
        );
        let part = AssignmentPartRef::new(&assignment, 0);
        assert!(part.variable_ref());
        assert!(!part.is_stack_variable());
        for value in [0, 1, 0x8000_0000, u32::MAX] {
            assignment.set_var_ref_custom_idx(value);
            assert_eq!(part.variable_ref_data(), value);
            assignment.set_stack_variable(true);
            assert_eq!(part.variable_stack_off(), value as i32);
            assignment.set_stack_variable(false);
        }
        let arbitrary = AssignmentPartRef::new(&assignment, u32::MAX);
        assert!(arbitrary.variable_ref());
        assignment.set_stack_variable(true);
        assert!(arbitrary.is_stack_variable());
    });
}

#[test]
fn invalid_setters_preserve_existing_word() {
    if !tpde_util::CHECKED {
        return;
    }
    let context = MemoryContext::new("assignment-part");
    with_assignment_allocator(context.mcx(), |allocator| {
        let assignment = allocator
            .allocate(1)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 0xffff);
        let part = AssignmentPartRef::new(&assignment, 0);
        for bank in [8, 255] {
            assert!(catch_unwind(AssertUnwindSafe(|| part.set_bank(RegBank::new(bank)))).is_err());
            assert_eq!(assignment.part(0).get(), 0xffff);
        }
        for size in [0, 3, 127, 129, 256, 1 << 31, u32::MAX] {
            assert!(catch_unwind(AssertUnwindSafe(|| part.set_part_size(size))).is_err());
            assert_eq!(assignment.part(0).get(), 0xffff);
        }
        for reg in [0, 31, 32, 223] {
            assert!(catch_unwind(AssertUnwindSafe(|| part.set_reg(Reg::new(reg)))).is_err());
            assert_eq!(assignment.part(0).get(), 0xffff);
        }
    });
}

#[test]
fn invalid_word_access_and_header_preconditions_are_loud() {
    let context = MemoryContext::new("assignment-part");
    with_assignment_allocator(context.mcx(), |allocator| {
        let assignment = allocator
            .allocate(1)
            .unwrap()
            .initialize(AssignmentInit::default(), |_| 0x1234);
        let bad = AssignmentPartRef::new(&assignment, 1);
        assert!(catch_unwind(AssertUnwindSafe(|| bad.get_reg())).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| bad.reset())).is_err());
        assert_eq!(assignment.part(0).get(), 0x1234);
        if !tpde_util::CHECKED {
            return;
        }
        let part = AssignmentPartRef::new(&assignment, 0);
        assert!(catch_unwind(AssertUnwindSafe(|| part.frame_off())).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| part.variable_ref_data())).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| part.variable_stack_off())).is_err());
        assignment.set_variable_ref(true);
        assignment.set_frame_off(-16);
        assert!(catch_unwind(AssertUnwindSafe(|| part.frame_off())).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| bad.variable_ref_data())).is_err());
        assignment.set_stack_variable(true);
        assert!(catch_unwind(AssertUnwindSafe(|| part.variable_ref_data())).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| bad.variable_stack_off())).is_err());
        assignment.set_stack_variable(false);
        assert!(catch_unwind(AssertUnwindSafe(|| part.variable_stack_off())).is_err());
    });
}

#[test]
fn views_end_before_recycle_and_allocator_reset() {
    let context = MemoryContext::new("assignment-part");
    with_assignment_allocator(context.mcx(), |mut allocator| {
        let mut first = core::ptr::null();
        for epoch in 0..3 {
            let assignment = allocator.allocate(1).unwrap().initialize(
                AssignmentInit {
                    frame_off: 16,
                    max_part_size: 8,
                    ..AssignmentInit::default()
                },
                |_| 0,
            );
            if epoch == 0 {
                first = assignment.as_ptr();
            } else {
                assert_eq!(assignment.as_ptr(), first);
            }
            let view = AssignmentPartRef::new(&assignment, 0);
            let copy = view;
            view.set_bank(RegBank::new(3));
            copy.set_reg(Reg::new(111));
            assert_eq!(view.get_reg(), Reg::new(111));
            allocator.deallocate(assignment);
            let reused = allocator
                .allocate(1)
                .unwrap()
                .initialize(AssignmentInit::default(), |_| 0xffff);
            assert_eq!(reused.as_ptr(), first);
            AssignmentPartRef::new(&reused, 0).reset();
            assert_eq!(reused.part(0).get(), 0x200);
            allocator.deallocate(reused);
            allocator.reset();
        }
    });
}

#[test]
fn live_base_view_survives_inserting_a_distinct_assignment_slot() {
    let context = MemoryContext::new("assignment-part-table");
    with_assignment_allocator(context.mcx(), |mut allocator| {
        let mut table = tpde_util::SmallVec::<Option<Assignment<'_, '_>>, 2>::new(context.mcx());
        table
            .push(Some(allocator.allocate(1).unwrap().initialize(
                AssignmentInit {
                    frame_off: -64,
                    max_part_size: 8,
                    variable_ref: true,
                    stack_variable: true,
                    ..AssignmentInit::default()
                },
                |_| 0x3200,
            )))
            .unwrap();
        table.push(None).unwrap();
        {
            let (source, destination) = table.split_at_mut(1);
            let base = AssignmentPartRef::new(source[0].as_ref().unwrap(), 0);
            destination[0] = Some(allocator.allocate_slow(1, true).unwrap().initialize(
                AssignmentInit {
                    max_part_size: 8,
                    variable_ref: true,
                    ..AssignmentInit::default()
                },
                |_| 0x3200,
            ));
            let destination_assignment = destination[0].as_ref().unwrap();
            destination_assignment.set_stack_variable(true);
            destination_assignment.set_frame_off(base.variable_stack_off() + 16);
            assert_eq!(base.variable_stack_off(), -64);
            let result = AssignmentPartRef::new(destination_assignment, 0);
            assert_eq!(result.variable_stack_off(), -48);
            base.set_register_valid(true);
            assert!(!result.register_valid());
        }
        let source = table[0].take().unwrap();
        allocator.deallocate(source);
        while let Some(entry) = table.pop() {
            if let Some(assignment) = entry {
                allocator.deallocate(assignment);
            }
        }
        table.release();
        allocator.reset();
    });
}
