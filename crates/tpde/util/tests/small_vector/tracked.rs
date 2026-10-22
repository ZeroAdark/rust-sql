// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use std::{
    boxed::Box,
    cell::{Cell, RefCell},
    rc::Rc,
    thread_local,
    vec::Vec,
};
#[derive(Default)]
pub(super) struct Stats {
    pub(super) created: Cell<usize>,
    pub(super) released: Cell<usize>,
    pub(super) clone_budget: Cell<Option<usize>>,
    pub(super) default_budget: Cell<Option<usize>>,
    pub(super) panic_drop: Cell<Option<i64>>,
    pub(super) released_values: RefCell<Vec<i64>>,
}
thread_local! {
    static ACTIVE: RefCell<Option<Rc<Stats>>> = const { RefCell::new(None) };
}
struct Active;
impl Drop for Active {
    fn drop(&mut self) {
        ACTIVE.with(|slot| *slot.borrow_mut() = None);
    }
}
pub(super) fn tracked_scope(run: impl FnOnce(&Rc<Stats>)) {
    let stats = Rc::new(Stats::default());
    ACTIVE.with(|slot| assert!(slot.borrow_mut().replace(stats.clone()).is_none()));
    let _active = Active;
    run(&stats);
    assert_eq!(stats.created.get(), stats.released.get());
}
fn admit(budget: &Cell<Option<usize>>) {
    if let Some(left) = budget.get() {
        assert!(left != 0, "injected constructor failure");
        budget.set(Some(left - 1));
    }
}
#[repr(C)]
pub(super) struct Tracked {
    pub(super) resource: Box<i64>,
    pub(super) stats: Rc<Stats>,
}
impl Tracked {
    pub(super) fn new(value: i64) -> Self {
        let stats = ACTIVE.with(|slot| slot.borrow().as_ref().unwrap().clone());
        stats.created.set(stats.created.get() + 1);
        Self {
            resource: Box::new(value),
            stats,
        }
    }
}
impl Clone for Tracked {
    fn clone(&self) -> Self {
        admit(&self.stats.clone_budget);
        Self::new(*self.resource)
    }
}
impl Default for Tracked {
    fn default() -> Self {
        ACTIVE.with(|slot| admit(&slot.borrow().as_ref().unwrap().default_budget));
        Self::new(0)
    }
}
impl Drop for Tracked {
    fn drop(&mut self) {
        self.stats.released.set(self.stats.released.get() + 1);
        self.stats.released_values.borrow_mut().push(*self.resource);
        if self.stats.panic_drop.get() == Some(*self.resource) {
            self.stats.panic_drop.set(None);
            panic!("injected destructor failure");
        }
    }
}
