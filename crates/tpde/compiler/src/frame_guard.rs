// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use alloc::boxed::Box;
use mcx::{Mcx, PgFxHashMap};
use tpde_util::{tpde_assert, tpde_assert_eq, tpde_assert_ne, SmallVec};
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};

#[cold]
fn overflow() -> Box<PgError> {
    PgError::error("compiler stack frame exceeds representable size")
        .with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE)
        .into()
}

struct FreeList<'m, const N: usize> {
    slots: SmallVec<'m, i32, N>,
    issued: usize,
}

impl<'m, const N: usize> FreeList<'m, N> {
    fn new(mcx: Mcx<'m>) -> Self {
        Self {
            slots: SmallVec::new(mcx),
            issued: 0,
        }
    }

    fn reserve_fresh(&mut self) -> PgResult<()> {
        self.slots
            .reserve(self.issued.checked_add(1).ok_or_else(overflow)?)
    }

    fn put(&mut self, slot: i32) {
        tpde_assert!(
            self.slots.len() < self.issued,
            "free of unallocated stack slot"
        );
        self.slots
            .push(slot)
            .expect("stack release capacity reserved before allocation");
    }

    fn clear(&mut self) {
        self.slots.clear();
        self.issued = 0;
    }
}

mcx::forget_safe_nodrop!(FreeList<'_, 0>, FreeList<'_, 16>);

pub struct FrameState<'m> {
    pub frame_size: u32,
    pub has_dynamic_alloca: bool,
    pub is_leaf_function: bool,
    pub generated_call: bool,
    pub frame_used: bool,
    fixed: [FreeList<'m, 16>; 5],
    dynamic: PgFxHashMap<'m, u32, FreeList<'m, 0>>,
    mcx: Mcx<'m>,
}

impl<'m> FrameState<'m> {
    pub fn new(mcx: Mcx<'m>) -> Self {
        Self {
            frame_size: 0,
            has_dynamic_alloca: false,
            is_leaf_function: false,
            generated_call: false,
            frame_used: false,
            fixed: core::array::from_fn(|_| FreeList::new(mcx)),
            dynamic: PgFxHashMap::with_hasher_in(Default::default(), mcx),
            mcx,
        }
    }

    pub fn begin_function(&mut self, dynamic: bool, leaf: bool) {
        self.frame_size = u32::MAX;
        self.has_dynamic_alloca = dynamic;
        self.is_leaf_function = leaf;
        self.generated_call = false;
        self.frame_used = false;
        self.reset();
    }

    pub fn reset(&mut self) {
        for list in &mut self.fixed {
            list.clear();
        }
        for list in self.dynamic.values_mut() {
            list.clear();
        }
    }

    pub fn fixed_free_list(&self, index: usize) -> &[i32] {
        &self.fixed[index].slots
    }

    pub fn dynamic_free_lists(&self) -> impl Iterator<Item = (u32, &[i32])> {
        self.dynamic
            .iter()
            .map(|(size, list)| (*size, &list.slots[..]))
    }

    pub fn allocate(&mut self, mut size: u32, negative: bool) -> PgResult<i32> {
        self.frame_used = true;
        if size == 0 {
            return Ok(0);
        }
        let fixed = size <= 16;
        let bits = if fixed {
            size.next_power_of_two().trailing_zeros()
        } else {
            4
        };
        size = if fixed {
            1 << bits
        } else {
            size.checked_add(15).ok_or_else(overflow)? & !15
        };
        let reused = if fixed {
            self.fixed[bits as usize].slots.pop()
        } else {
            self.dynamic
                .get_mut(&size)
                .and_then(|list| list.slots.pop())
        };
        if let Some(slot) = reused {
            return Ok(slot);
        }
        tpde_assert_ne!(self.frame_size, u32::MAX, "stack frame not initialized");
        tpde_assert_ne!(self.frame_size, 0, "stack slot zero is reserved");
        let mask = (1u32 << bits) - 1;
        let aligned = self.frame_size.checked_add(mask).ok_or_else(overflow)? & !mask;
        let end = aligned.checked_add(size).ok_or_else(overflow)?;
        if fixed {
            self.fixed[bits as usize].reserve_fresh()?;
        } else {
            if !self.dynamic.contains_key(&size) {
                self.dynamic
                    .try_reserve(1)
                    .map_err(|_| self.mcx.oom(core::mem::size_of::<(u32, FreeList<'m, 0>)>()))?;
                self.dynamic.insert(size, FreeList::new(self.mcx));
            }
            self.dynamic.get_mut(&size).unwrap().reserve_fresh()?;
        }
        let mut preview = self.frame_size;
        while preview.trailing_zeros() < bits {
            let index = preview.trailing_zeros();
            self.fixed[index as usize].reserve_fresh()?;
            preview += 1 << index;
        }
        if fixed {
            self.fixed[bits as usize].issued += 1;
        } else {
            self.dynamic.get_mut(&size).unwrap().issued += 1;
        }
        while self.frame_size.trailing_zeros() < bits {
            let index = self.frame_size.trailing_zeros();
            let next = self.frame_size + (1 << index);
            let slot = if negative {
                next.wrapping_neg() as i32
            } else {
                self.frame_size as i32
            };
            self.fixed[index as usize].issued += 1;
            self.fixed[index as usize].put(slot);
            self.frame_size = next;
        }
        let slot = if negative {
            end.wrapping_neg() as i32
        } else {
            self.frame_size as i32
        };
        self.frame_size = end;
        Ok(slot)
    }

    pub fn free(&mut self, slot: i32, mut size: u32) {
        if size == 0 {
            tpde_assert_eq!(slot, 0);
            return;
        }
        if size <= 16 {
            self.fixed[size.next_power_of_two().trailing_zeros() as usize].put(slot);
        } else {
            size = size
                .checked_add(15)
                .expect("free size was admitted at allocation")
                & !15;
            self.dynamic
                .get_mut(&size)
                .expect("free of unallocated stack class")
                .put(slot);
        }
    }
}

impl<'m> FrameState<'m> {
    /// Upstream `free_stack_slot(slot, freed_size)` on a slot that was
    /// allocated with a different fixed size: the C++ free lists carry no
    /// per-class accounting, so the slot silently joins the `freed_size`
    /// class (pinned `LLVMCompilerX64::fp80_to_int` frees its 4-byte control
    /// word slot with size 2). The guard keeps its invariant by transferring
    /// the outstanding count from the allocated class to the freed class
    /// before the release; both sizes must be fixed (<= 16) and the slot must
    /// be outstanding in its allocated class.
    pub fn free_reclassified(
        &mut self,
        slot: i32,
        allocated_size: u32,
        freed_size: u32,
    ) -> PgResult<()> {
        tpde_assert!(
            (1..=16).contains(&allocated_size) && (1..=16).contains(&freed_size),
            "reclassified free requires two fixed size classes"
        );
        let from = allocated_size.next_power_of_two().trailing_zeros() as usize;
        let to = freed_size.next_power_of_two().trailing_zeros() as usize;
        if from == to {
            self.fixed[to].put(slot);
            return Ok(());
        }
        tpde_assert!(
            self.fixed[from].slots.len() < self.fixed[from].issued,
            "reclassified free of an unallocated stack slot"
        );
        self.fixed[to].reserve_fresh()?;
        self.fixed[from].issued -= 1;
        self.fixed[to].issued += 1;
        self.fixed[to].put(slot);
        Ok(())
    }
}

impl Drop for FrameState<'_> {
    fn drop(&mut self) {
        for list in &mut self.fixed {
            list.slots.release();
        }
        for list in self.dynamic.values_mut() {
            list.slots.release();
        }
    }
}

#[cfg(test)]
#[path = "../tests/frame.rs"]
mod tests;
