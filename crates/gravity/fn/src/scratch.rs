//! D4113: a thread's retained scratch memory context, one home for every copy (C7 clause (d), unwind-safe thread
//! scopes). The outer call holds the context's `RefMut`, whose drop gives the slot back on any exit, an unwinding ERROR
//! included; the slot stays dirty until a body returns, so the next outer call after an unwind resets it before use. No
//! drop resets: a reset fires the context's callbacks, and a panic inside a drop during an unwind aborts the server. A
//! re-entered call (a body that reaches the slot again) finds it borrowed and gets a context of its own.

use core::cell::{Cell, RefCell};
use mcx::{Mcx, MemoryContext};

pub struct ScratchSlot {
    cx: RefCell<MemoryContext>,
    dirty: Cell<bool>,
    nested: &'static str,
}

impl ScratchSlot {
    /// A slot whose retained context is `name`; a re-entered call's context is `nested`.
    pub fn new(name: &'static str, nested: &'static str) -> Self {
        Self { cx: RefCell::new(MemoryContext::new_bump(name)), dirty: Cell::new(false), nested } // alloc: Node
    }

    /// `f` against the retained context, reset after `f` (which copies its result out).
    #[inline]
    pub fn with<R>(&self, f: impl FnOnce(Mcx<'_>) -> R) -> R {
        let Ok(mut cx) = self.cx.try_borrow_mut() else {
            let mut cx = MemoryContext::new_bump(self.nested); // alloc: Cold
            let out = f(cx.mcx());
            cx.reset();
            return out;
        };
        if self.dirty.replace(true) {
            cx.reset();
        }
        let out = f(cx.mcx());
        cx.reset();
        self.dirty.set(false);
        out
    }
}
