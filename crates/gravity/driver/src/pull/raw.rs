//! `pull`'s unsafe step (rule R6, the module's raw home): the adapter's boxed buffer lent to its own binding as the sink.
#![allow(unsafe_code)]

use super::*;

/// The adapter's buffer as its binding's sink, for the adapter's lifetime (`PullAdapter::open`).
pub(super) fn lend_buffer<'a>(buf: &mut PullBuffer) -> &'a mut PullBuffer {
    // SAFETY: `buf` is the adapter's own boxed buffer (a heap address its moves keep), lent only to the adapter's own
    // binding, which `open` replaces and `close` clears.
    unsafe { &mut *(buf as *mut PullBuffer) }
}
