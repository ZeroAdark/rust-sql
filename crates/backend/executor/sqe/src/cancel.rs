//! Statement-scoped cooperative cancellation: the dispatch slot arms a
//! token (with a shell hook converting a raised interrupt into a
//! type-erased error payload); claim loops check it at granule
//! boundaries and unwind at the next checkpoint — the armer with the
//! statement's real error, worker frames with the `Canceled` marker
//! (swallowed by the claim helpers once the token fired).

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub type Payload = Box<dyn Any + Send>;
pub type PollFn = fn() -> Option<Payload>;

/// Cacheline-aligned so `fired` (read on every claim by every worker)
/// never shares a line with the ArcInner refcounts the lifecycle paths
/// RMW ([sqe-stmt-constant]: the armed-only false-sharing tax).
#[repr(align(128))]
pub struct Token {
    fired: AtomicBool,
    payload: Mutex<Option<Payload>>,
}

pub struct Canceled;

thread_local! {
    // tls-dtor: plain-data — held type audited 2026-08-19: no Drop beyond plain collections/dealloc.
    static CURRENT: RefCell<Option<Arc<Token>>> = const { RefCell::new(None) };
    static POLL: Cell<Option<PollFn>> = const { Cell::new(None) };
}

pub fn arm(poll: PollFn) -> ArmGuard {
    let tok = Arc::new(Token {
        fired: AtomicBool::new(false),
        payload: Mutex::new(None),
    });
    CURRENT.with(|c| *c.borrow_mut() = Some(tok));
    POLL.with(|p| p.set(Some(poll)));
    ArmGuard
}

pub struct ArmGuard;

impl Drop for ArmGuard {
    fn drop(&mut self) {
        CURRENT.with(|c| *c.borrow_mut() = None);
        POLL.with(|p| p.set(None));
    }
}

pub fn current() -> Option<Arc<Token>> {
    CURRENT.with(|c| c.borrow().clone())
}

pub fn inherit(tok: &Option<Arc<Token>>) -> InheritGuard {
    let prev = CURRENT.with(|c| std::mem::replace(&mut *c.borrow_mut(), tok.clone()));
    InheritGuard(prev)
}

pub struct InheritGuard(Option<Arc<Token>>);

impl Drop for InheritGuard {
    fn drop(&mut self) {
        let prev = self.0.take();
        CURRENT.with(|c| *c.borrow_mut() = prev);
    }
}

#[inline]
pub fn fired_of(tok: &Option<Arc<Token>>) -> bool {
    tok.as_ref().is_some_and(|t| t.fired.load(Ordering::Relaxed))
}

#[inline]
pub fn stop_requested() -> bool {
    // Read under the borrow — no Arc clone (refcount RMW) on hot paths.
    CURRENT.with(|c| {
        c.borrow()
            .as_ref()
            .is_some_and(|t| t.fired.load(Ordering::Relaxed))
    })
}

#[inline]
pub fn armed_here() -> bool {
    POLL.with(|p| p.get().is_some())
}

pub fn poll_now() {
    let Some(poll) = POLL.with(|p| p.get()) else { return };
    // Fired probe under the borrow (no refcount RMW); the poll callback
    // runs OUTSIDE the borrow (it must be free to use TLS itself), so the
    // token is cloned only on the rare not-yet-fired armer cadence, never
    // per worker claim.
    match CURRENT.with(|c| c.borrow().as_ref().map(|t| t.fired.load(Ordering::Relaxed))) {
        None | Some(true) => return,
        Some(false) => {}
    }
    if let Some(p) = poll() {
        let Some(tok) = CURRENT.with(|c| c.borrow().clone()) else { return };
        *tok.payload.lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
        tok.fired.store(true, Ordering::SeqCst);
    }
}

/// Callers must hold no locks: this unwinds when the token fired.
pub fn checkpoint() {
    // Hot path (armed, not fired): one TLS borrow + one relaxed load —
    // no Arc clone ([sqe-stmt-constant]: leader loops checkpoint per
    // granule; the clone's refcount RMW was armed-only work the native
    // arm never paid).
    let fired = CURRENT.with(|c| c.borrow().as_ref().map(|t| t.fired.load(Ordering::Relaxed)));
    let Some(fired) = fired else { return };
    if armed_here() && !fired {
        poll_now();
    }
    // Re-probe post-poll; clone the Arc only on the fired (unwinding)
    // path — the steady state stays clone-free.
    let fired = CURRENT
        .with(|c| c.borrow().as_ref().map(|t| t.fired.load(Ordering::Relaxed)))
        .unwrap_or(false);
    if !fired {
        return;
    }
    let tok = CURRENT.with(|c| c.borrow().clone());
    let Some(tok) = tok else { return };
    // Only the armer carries the real error; worker frames raise the marker.
    if !armed_here() {
        std::panic::panic_any(Canceled);
    }
    let taken = tok.payload.lock().unwrap_or_else(|e| e.into_inner()).take();
    match taken {
        Some(p) => std::panic::resume_unwind(p),
        None => std::panic::panic_any(Canceled),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    fn real_error() -> Option<Payload> {
        Some(Box::new("the-real-57014-error"))
    }

    #[test]
    fn worker_checkpoint_raises_marker_and_keeps_armer_payload() {
        let _g = arm(real_error);
        poll_now();
        let tok = current().expect("armed");
        assert!(tok.fired.load(Ordering::SeqCst));
        let worker = std::thread::spawn(move || {
            let _inh = inherit(&Some(tok));
            assert!(!armed_here());
            catch_unwind(AssertUnwindSafe(checkpoint))
        })
        .join()
        .expect("worker joined");
        let err = worker.expect_err("fired token unwinds");
        assert!(err.is::<Canceled>(), "worker must raise the Canceled marker");
        let armer = catch_unwind(AssertUnwindSafe(checkpoint)).expect_err("armer unwinds");
        assert_eq!(armer.downcast_ref::<&str>().copied(), Some("the-real-57014-error"));
    }
}
