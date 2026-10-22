//! `pg_tz *session_timezone` / `pg_tz *log_timezone` (pgtz.c globals). They
//! live here, below elog in the crate DAG, because elog's log_line_prefix
//! timestamps (`setup_formatted_log_time`) read `log_timezone` and pgtz sits
//! above elog. pgtz owns the setters' callers (pg_timezone_initialize, the
//! GUC assign hooks); everything else reads through pgtz's re-exports.
//!
//! Backend-private in C (inherited at fork, GUC assign hooks re-run per
//! backend), so thread-local here.
//! Values are shared handles ([`PgTzRef`]) into the session's pgtz cache.

use core::cell::RefCell;

use crate::{PgTz, PgTzRef};

thread_local! {
    static SESSION_TIMEZONE: RefCell<Option<PgTzRef>> = const { RefCell::new(None) };
    static LOG_TIMEZONE: RefCell<Option<PgTzRef>> = const { RefCell::new(None) };
}

/// A new handle on the session zone: for a reader that keeps it past the call (a cache entry, a statement's
/// captured zone, a SRF's first-call zone). A per-datum reader borrows it with [`with_session_timezone`].
#[inline]
pub fn session_timezone() -> Option<PgTzRef> {
    SESSION_TIMEZONE.with(|c| c.borrow().clone())
}

/// The session zone, borrowed for `f`'s duration: no refcount traffic. Every Gravity pool worker holds a handle on the
/// SAME zone, so a per-row `session_timezone()` (clone + drop) is two atomic read-modify-writes on one cache line that
/// all workers share (a date-filtered scan at W4: 35-39 % of the query's CPU in `__aarch64_ldadd8_*`).
///
/// `f` must not set the session zone: `set_session_timezone`'s `borrow_mut` would meet this borrow. Keep `f` to
/// tz-engine reads (offsets, `pg_localtime`, names), take the answer out, and act on it after `f` returns. Nested
/// reads (`session_timezone()`, another `with_session_timezone`) inside `f` are shared borrows and fine.
#[inline]
pub fn with_session_timezone<R>(f: impl FnOnce(Option<&PgTz>) -> R) -> R {
    SESSION_TIMEZONE.with(|c| f(c.borrow().as_deref()))
}

pub fn set_session_timezone(tz: Option<PgTzRef>) {
    SESSION_TIMEZONE.with(|c| *c.borrow_mut() = tz);
}

#[inline]
pub fn log_timezone() -> Option<PgTzRef> {
    LOG_TIMEZONE.with(|c| c.borrow().clone())
}

pub fn set_log_timezone(tz: Option<PgTzRef>) {
    LOG_TIMEZONE.with(|c| *c.borrow_mut() = tz);
}
