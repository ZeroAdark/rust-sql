// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use core::cell::RefCell;
use tpde_util::tpde_assert;

pub(crate) struct AssignerLoan<'a, C> {
    slot: &'a RefCell<Option<C>>,
    value: Option<C>,
}
impl<'a, C> AssignerLoan<'a, C> {
    pub(crate) fn new(slot: &'a RefCell<Option<C>>) -> Self {
        let value = slot
            .borrow_mut()
            .take()
            .expect("calling convention assigner already borrowed");
        Self {
            slot,
            value: Some(value),
        }
    }
    pub(crate) fn get_mut(&mut self) -> &mut C {
        self.value.as_mut().unwrap()
    }
}
impl<C> Drop for AssignerLoan<'_, C> {
    fn drop(&mut self) {
        let previous = self.slot.borrow_mut().replace(self.value.take().unwrap());
        tpde_assert!(previous.is_none())
    }
}
