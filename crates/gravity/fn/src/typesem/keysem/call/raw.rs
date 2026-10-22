//! D3375 N7: the bound-call table's raw ownership (call.rs's unsafe half, R6). A binding is owned through its raw pointer
//! (`Box::into_raw`), so the call running it reaches it while a nested call adds bindings to the table or the table drops
//! others. A busy binding is touched only field by field (`busy`, `stale`), never through a reference to the whole
//! binding, so the running call's borrow of its `f` (`Running::f`) is the only one.
use core::ptr::NonNull;

use super::alias::AliasVerdict;
use super::{Bound, BOUND};
use crate::FmgrCall;

/// D3402: the effects home's alias verdict (`alias::set_alias_verdict`), stored as its fn pointer; null until installed.
static VERDICT: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub(super) fn set_verdict(v: AliasVerdict) {
    VERDICT.store(v as *mut (), core::sync::atomic::Ordering::Release);
}

/// The installed alias verdict, if any.
pub(super) fn verdict() -> Option<AliasVerdict> {
    let p = VERDICT.load(core::sync::atomic::Ordering::Acquire);
    // SAFETY: a non-null value is an `AliasVerdict` fn pointer `set_verdict` stored; fn pointers and data pointers have
    // one size on every target Gravity builds for.
    (!p.is_null()).then(|| unsafe { core::mem::transmute::<*mut (), AliasVerdict>(p) })
}

/// A binding of the table, owned through its raw pointer.
pub(super) struct Slot(NonNull<Bound>);

impl Slot {
    pub(super) fn new(b: Bound) -> Slot {
        Slot(NonNull::from(Box::leak(Box::new(b)))) // alloc: Cold
    }

    pub(super) fn busy(&self) -> bool {
        // SAFETY: the slot owns a live binding; the field is read in place, no reference to the binding is made.
        unsafe { (*self.0.as_ptr()).busy }
    }

    pub(super) fn mark_stale(&self) {
        // SAFETY: as `busy`.
        unsafe { (*self.0.as_ptr()).stale = true }
    }

    /// The binding, when no call of it runs.
    pub(super) fn idle(&self) -> Option<&Bound> {
        // SAFETY: an idle binding is referenced by no running call.
        (!self.busy()).then(|| unsafe { self.0.as_ref() })
    }

    pub(super) fn idle_mut(&mut self) -> Option<&mut Bound> {
        // SAFETY: as `idle`; the table's borrow makes this the only reference.
        (!self.busy()).then(|| unsafe { &mut *self.0.as_ptr() })
    }

    /// A call of this (idle) binding starts: busy until the `Running` drops.
    pub(super) fn run(&mut self) -> Running {
        let b = self.idle_mut().expect("gravity: a support function's binding run while its call runs");
        b.busy = true;
        Running(self.0)
    }

    /// Whether `r` runs this binding.
    pub(super) fn runs(&self, r: &Running) -> bool {
        self.0 == r.0
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        debug_assert!(!self.busy(), "gravity: a support function's binding dropped while its call runs");
        // SAFETY: `Slot::new`'s box, dropped once; `find` and `sweep` keep a busy slot, so no call runs it.
        drop(unsafe { Box::from_raw(self.0.as_ptr()) });
    }
}

/// The binding a call runs (`Slot::run`), `busy` while this lives; one that went `stale` meanwhile leaves the table here.
pub(super) struct Running(NonNull<Bound>);

impl Running {
    /// The running binding's function: the only reference into a busy binding.
    pub(super) fn f(&mut self) -> &mut FmgrCall {
        // SAFETY: `Running` keeps the binding live and busy (no other caller references it); the reference covers `f`
        // only, and lives no longer than this borrow of the `Running`.
        unsafe { &mut (*self.0.as_ptr()).f }
    }

    /// C3: what the running binding's bind and first call read (`call::first_call`), kept with it.
    pub(super) fn hold(&mut self, held: gravity_deps::Held) {
        // SAFETY: as `f`: the binding is live and busy; the field is written in place, no reference to the binding is kept.
        unsafe { (*self.0.as_ptr()).held = Some(held) }
    }

    /// D3402: the running binding's argument permutation when it is a built-in alias (`alias`).
    #[inline]
    pub(super) fn perm(&self) -> Option<[u8; 2]> {
        // SAFETY: as `f`: the binding is live; the field is copied out in place, no reference to the binding is kept.
        unsafe { (*self.0.as_ptr()).alias.map(|a| a.perm) }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let p = self.0.as_ptr();
        // SAFETY: the binding is live (a busy slot is kept) and its call has ended; fields are written in place.
        let stale = unsafe {
            (*p).busy = false;
            (*p).stale
        };
        if stale {
            let gone = BOUND.try_with(|h| h.try_borrow_mut().ok().and_then(|mut v| v.iter().position(|s| s.0 == self.0).map(|i| v.remove(i))));
            drop(gone);
        }
    }
}
