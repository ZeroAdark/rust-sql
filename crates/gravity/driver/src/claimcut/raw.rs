//! `claimcut`'s unsafe step (rule R6, the module's raw home): the thread's current cut, read through the pointer its
//! claim's scope set.
#![allow(unsafe_code)]

use super::{Cut, CLAIM};

/// `f` on the cut of the pool claim this thread's claim node runs (`enter`); `None` outside one.
pub fn with_current<R>(f: impl FnOnce(&Cut) -> R) -> Option<R> {
    let p = CLAIM.with(|c| c.current.get());
    // SAFETY: non-null only inside an `Entered` scope (crate-internal, never leaked: the section's `claim_on` holds it on
    // its stack) of a cut its pool claim offered (`Board::run`), and `Board::run`'s `Begun` clears it at the claim's end.
    // The cut is the pool's per-ordinal cell of the running section, borrowed by `Board::run` for the claim's length.
    unsafe { p.as_ref() }.map(f)
}
