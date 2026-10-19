//! `START_CRIT_SECTION()` / `END_CRIT_SECTION()` (miscadmin.h:150-157) as a
//! scoped guard over `CritSectionCount`: `Drop` is END (an early `return`
//! cannot leave the section open), and `run` promotes a hand-built `PgError`
//! escaping the body to PANIC like `errstart` does for ereport errors.

use ::types_error::PgResult;

use crate::stack::panic_on_crit_section_escape;

/// A held critical section; not `Send` (the count is per-backend).
#[must_use = "dropping the guard immediately ends the critical section"]
pub struct CritSection {
    _not_send: core::marker::PhantomData<*const ()>,
}

impl CritSection {
    /// `START_CRIT_SECTION()`.
    #[inline]
    pub fn enter() -> Self {
        init_small::globals::StartCriticalSection();
        CritSection {
            _not_send: core::marker::PhantomData,
        }
    }

    /// `END_CRIT_SECTION()`, spelled at the point C spells it.
    #[inline]
    pub fn exit(self) {
        drop(self);
    }

    /// START, `body`, END on `Ok`; an escaping `Err` is promoted to PANIC as
    /// an `ereport` inside the bracket would have been.
    #[inline]
    pub fn run<T>(body: impl FnOnce() -> PgResult<T>) -> PgResult<T> {
        let guard = CritSection::enter();
        match body() {
            Ok(value) => {
                guard.exit();
                Ok(value)
            }
            Err(err) => {
                // Never returns while the guard is alive (count > 0).
                panic_on_crit_section_escape(&err);
                guard.exit();
                Err(err)
            }
        }
    }
}

impl Drop for CritSection {
    #[inline]
    fn drop(&mut self) {
        init_small::globals::EndCriticalSection();
    }
}
