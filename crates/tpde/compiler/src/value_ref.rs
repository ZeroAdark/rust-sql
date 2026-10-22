// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{
    guard_machine::{GuardMachine, INVALID_VALUE},
    value_part_ref::ValuePartRef,
};
use core::ptr::NonNull;
use tpde_base::ValLocalIdx;
use tpde_core::{
    compiler::ValRefSpecialStruct,
    value_assignment::{Assignment, ValueAssignment},
};
use tpde_util::{tpde_assert, tpde_assert_eq, tpde_assert_ne};

#[derive(Clone, Copy)]
enum State<S> {
    Assignment {
        local: ValLocalIdx,
        mode: u8,
        assignment: Option<NonNull<ValueAssignment>>,
    },
    Special(S),
}

#[doc = include_str!("../tests/value-guards-ownership.md")]
pub struct ValueRef<'c, M: GuardMachine<N>, const N: usize> {
    machine: &'c M,
    state: State<M::Special>,
}

impl<'c, M: GuardMachine<N>, const N: usize> ValueRef<'c, M, N> {
    pub fn invalid(machine: &'c M) -> Self {
        Self {
            machine,
            state: State::Assignment {
                local: INVALID_VALUE,
                mode: 0,
                assignment: None,
            },
        }
    }

    pub fn new(machine: &'c M, local: ValLocalIdx) -> Self {
        machine.with_assignment(local, |assignment| {
            Self::from_token(machine, local, assignment)
        })
    }

    /// The upstream constructor body over the token already looked up in slot `local`.
    pub fn from_token(machine: &'c M, local: ValLocalIdx, assignment: &Assignment<'_, '_>) -> Self {
        assert!(!assignment.pending_free(), "access of freed assignment");
        let pointer = assignment.pointer();
        let mode = if assignment.variable_ref() {
            0
        } else {
            if tpde_util::CHECKED {
                let liveness = machine.liveness(local);
                tpde_assert!(
                    liveness.last.0 >= machine.current_block().0,
                    "value outside live range"
                );
                tpde_assert_ne!(assignment.references_left(), 0);
                if assignment.references_left() == 1 && !liveness.last_full {
                    tpde_assert_eq!(liveness.last.0, machine.current_block().0);
                }
            }
            if assignment.references_left() <= 1 && !assignment.delay_free() {
                2
            } else {
                1
            }
        };
        Self {
            machine,
            state: State::Assignment {
                local,
                mode,
                assignment: Some(pointer),
            },
        }
    }

    pub fn special(machine: &'c M, special: M::Special) -> Self {
        tpde_assert!(
            special.mode() >= 4,
            "special value overlaps assignment modes"
        );
        Self {
            machine,
            state: State::Special(special),
        }
    }

    pub fn has_assignment(&self) -> bool {
        matches!(self.state, State::Assignment { .. })
    }

    /// The cached pointer is the one read from slot `local` at construction. It
    /// stays valid because every free goes through `free_assignment`, which runs
    /// only when the reference count reaches zero (mode 1/2 hold one count each),
    /// for delayed frees of already zero-count assignments (construction refuses
    /// `pending_free`), or after the byval `reset()` of the only reference; variable
    /// references live until the function ends, which this borrow of the machine
    /// cannot outlive. Debug builds re-check the slot on every access.
    fn pinned(&self) -> (ValLocalIdx, NonNull<ValueAssignment>) {
        match self.state {
            State::Assignment {
                local,
                assignment: Some(assignment),
                ..
            } => {
                if tpde_util::CHECKED {
                    self.machine.with_assignment(local, |slot| {
                        tpde_assert!(
                            slot.pointer() == assignment,
                            "value reference outlived its assignment"
                        )
                    });
                }
                (local, assignment)
            }
            State::Assignment { .. } => panic!("invalid value reference"),
            State::Special(_) => panic!("special value has no local index"),
        }
    }

    pub fn with_assignment<R>(&self, operation: impl FnOnce(&ValueAssignment) -> R) -> R {
        let (_, assignment) = self.pinned();
        // SAFETY: `pinned` documents why the allocation is live; header mutation is Cell-only.
        operation(unsafe { assignment.as_ref() })
    }

    pub fn is_owned(&self) -> bool {
        matches!(self.state, State::Assignment { mode: 2, .. })
    }

    pub fn disown(&mut self) {
        if let State::Assignment { mode, .. } = &mut self.state {
            *mode = 0;
        }
    }

    pub fn disowned(&self) -> ValueRef<'_, M, N> {
        let mut result = ValueRef {
            machine: self.machine,
            state: self.state,
        };
        result.disown();
        result
    }

    pub fn local_idx(&self) -> ValLocalIdx {
        match self.state {
            State::Assignment { local, .. } => local,
            State::Special(_) => panic!("special value has no local index"),
        }
    }

    pub fn part(&self, part: u32) -> ValuePartRef<'_, M, N> {
        match &self.state {
            State::Assignment { mode, .. } => {
                let (local, assignment) = self.pinned();
                ValuePartRef::from_assignment(self.machine, local, assignment, part, *mode == 2)
            }
            State::Special(special) => self.machine.special_part(special, part),
        }
    }

    pub fn part_unowned(&self, part: u32) -> ValuePartRef<'_, M, N> {
        match &self.state {
            State::Assignment { .. } => {
                let (local, assignment) = self.pinned();
                ValuePartRef::from_assignment(self.machine, local, assignment, part, false)
            }
            State::Special(special) => self.machine.special_part(special, part),
        }
    }

    pub fn variable_ref(&self) -> bool {
        self.with_assignment(|assignment| assignment.variable_ref())
    }

    pub fn take(&mut self) -> Self {
        core::mem::replace(self, Self::invalid(self.machine))
    }

    pub fn reset(&mut self) {
        let old = core::mem::replace(
            &mut self.state,
            State::Assignment {
                local: INVALID_VALUE,
                mode: 0,
                assignment: None,
            },
        );
        if let State::Assignment {
            local,
            mode: 1 | 2,
            assignment: Some(assignment),
        } = old
        {
            if tpde_util::CHECKED {
                self.machine.with_assignment(local, |slot| {
                    tpde_assert!(
                        slot.pointer() == assignment,
                        "value reference outlived its assignment"
                    )
                });
            }
            // SAFETY: this reference still holds one count, so `pinned`'s argument applies.
            let assignment = unsafe { assignment.as_ref() };
            assert!(!assignment.pending_free(), "access of freed assignment");
            let count = assignment.references_left();
            tpde_assert_ne!(count, 0);
            assignment.set_references_left(count - 1);
            if count == 1 {
                self.machine.release_assignment(local);
            }
        }
    }
}

impl<M: GuardMachine<N>, const N: usize> Drop for ValueRef<'_, M, N> {
    fn drop(&mut self) {
        self.reset();
    }
}
