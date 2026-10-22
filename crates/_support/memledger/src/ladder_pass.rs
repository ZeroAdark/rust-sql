//! D4398: the memory ladder's thread never blocks on a cache's lock. Its R2a and R2b reach the caches through their hooks
//! (gravity's `mem::budget`: current / floor / trimmable / reclaim, the tail's ladder reclaims, the region and slot
//! depots), each taking its cache's own lock; a thread that held one long (a trim, a load, a block's asker waiting on the
//! ladder, D4397) stalled the ladder thread: no sample, no cancel, for that long. Inside a `LadderPass` (the ladder's
//! sample, memruntime) `lock_or_skip` only tries the lock: one held elsewhere skips that cache for this pass (its hook
//! returns its nothing and marks the thread, `take_skipped`, so the walk leaves it out) and the walk goes on to the next
//! cache, then the next rung. Outside a pass `lock_or_skip` is the plain lock, a poisoned one taken over as `pgsync::lock`
//! does: the reclaimer, the backstop and every gate are as before.

use core::cell::Cell;

use pgsync::{Mutex, MutexGuard, TryLockError};

std::thread_local! {
    /// The ladder passes open on this thread (nested `enter`s), and whether a lock was skipped since `take_skipped`.
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    static SKIPPED: Cell<bool> = const { Cell::new(false) };
}

/// The ladder thread's pass, while it lives: `lock_or_skip` tries instead of waiting.
#[must_use]
pub struct LadderPass(());

impl LadderPass {
    pub fn enter() -> LadderPass {
        let _ = DEPTH.try_with(|d| d.set(d.get() + 1));
        LadderPass(())
    }
}

impl Drop for LadderPass {
    fn drop(&mut self) {
        let _ = DEPTH.try_with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Whether this thread is inside a ladder pass.
#[inline]
pub fn in_pass() -> bool {
    DEPTH.try_with(|d| d.get() != 0).unwrap_or(false)
}

/// `m`'s lock: inside a ladder pass only if it is free now (`None`: held elsewhere, and the thread marked skipped);
/// outside one, waited for as `m.lock()` (a poisoned lock taken over).
#[inline]
pub fn lock_or_skip<T: ?Sized>(m: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    if !in_pass() {
        return Some(m.lock().unwrap_or_else(|e| e.into_inner()));
    }
    match m.try_lock() {
        Ok(g) => Some(g),
        Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
        Err(TryLockError::WouldBlock) => {
            let _ = SKIPPED.try_with(|s| s.set(true));
            None
        }
    }
}

/// Whether a `lock_or_skip` skipped since the last call (and the mark cleared): a walk asks it after each cache's hooks.
#[inline]
pub fn take_skipped() -> bool {
    SKIPPED.try_with(|s| s.replace(false)).unwrap_or(false)
}

std::thread_local! {
    /// pagepool P3 (C.11): the memory runtime's loop thread (`memruntime::loops::idle_loop`: the background reclaimer's
    /// tick and the idle release run on it), marked once at the loop's start. Its own flag, apart from `DEPTH`: a ladder
    /// pass is entered on query threads too (the inline R2a, `ladder_room_evict`), so `in_pass` never names the thread.
    static LADDER_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Marks the calling thread as the memory runtime's loop thread (`memruntime::loops::idle_loop`, once at its start).
pub fn mark_ladder_thread() {
    let _ = LADDER_THREAD.try_with(|t| t.set(true));
}

/// Whether the calling thread is the memory runtime's loop thread (`mark_ladder_thread`): false on every other thread, a
/// query thread inside a ladder pass included. Work that may wait on the kernel (pagepool P3's remaps) runs inline only
/// here; anywhere else it is asked of its own thread.
#[inline]
pub fn on_ladder_thread() -> bool {
    LADDER_THREAD.try_with(|t| t.get()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inside_a_pass_a_held_lock_is_skipped_and_outside_it_is_waited_for() {
        static M: Mutex<u32> = Mutex::new(7);
        let _ = take_skipped();
        assert_eq!(lock_or_skip(&M).map(|g| *g), Some(7), "outside a pass: the plain lock");
        let held = M.lock().unwrap();
        let other = std::thread::spawn(|| {
            let pass = LadderPass::enter();
            assert!(in_pass());
            let got = lock_or_skip(&M).is_some();
            let skipped = take_skipped();
            drop(pass);
            (got, skipped, in_pass(), take_skipped())
        });
        assert_eq!(other.join().unwrap(), (false, true, false, false), "held elsewhere: skipped, marked once");
        drop(held);
        let pass = LadderPass::enter();
        assert_eq!(lock_or_skip(&M).map(|g| *g), Some(7), "free: taken inside a pass too");
        assert!(!take_skipped());
        drop(pass);
        assert!(!in_pass());
    }

    /// pagepool P3 (C.11): the loop thread's mark is a flag of its own: a pass on another thread is no ladder thread, the
    /// mark enters no pass, and one thread's mark is no other's.
    #[test]
    fn the_ladder_threads_mark_is_its_own_and_never_a_passs() {
        assert!(!on_ladder_thread(), "unmarked");
        let query = std::thread::spawn(|| {
            let _pass = LadderPass::enter();
            (in_pass(), on_ladder_thread())
        });
        assert_eq!(query.join().unwrap(), (true, false), "a query thread's ladder pass (the inline R2a) is no ladder thread");
        let marked = std::thread::spawn(|| {
            mark_ladder_thread();
            let mark = (on_ladder_thread(), in_pass());
            let _pass = LadderPass::enter();
            (mark, on_ladder_thread())
        });
        assert_eq!(marked.join().unwrap(), ((true, false), true), "the mark alone enters no pass; a pass keeps it");
        assert!(!on_ladder_thread(), "another thread's mark is not this one's");
    }
}
