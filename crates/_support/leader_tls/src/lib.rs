//! C7 runtime half (construct C7 "session carry"): `leader_tls!`, the declaration of a leader-class thread-local. The
//! thread-state registry (crates/gravity/lint/THREAD-STATE, rule RT) names a static's carry `leader` when only the
//! backend's own statement path reads it: a pool claim on another thread would read that pool thread's own copy, never
//! its leader's (D3093, D3532, D3542 and D3576 were such reads). The registry documents the class; this macro finds a
//! breach at test time.
//!
//! ```text
//! leader_tls::leader_tls! {
//!     thread_local! {
//!         static DEPTH: Cell<u32> = const { Cell::new(0) };
//!     }
//! }
//! ```
//!
//! It wraps one `thread_local!` block, so every census reads its statics as before (the session crate's tls-census, RT,
//! the tools' tls-census.py: each reads `thread_local!` lines). Release builds (no `debug_assertions`: fast-profile,
//! release, dist) expand to that block unchanged: the same `LocalKey`s and initializers, no extra TLS and no check. Debug
//! builds (dev, check-test) declare each static as a [`LeaderKey`] over the same `LocalKey`: every access goes through
//! its `Deref`, which asks the pool probe whether a Gravity pool claim is in progress on this thread and in one raises
//! ERROR XX000 `leader_tls: <path under crates/> <static> touched in a pool claim`, a PgError payload (the pool's
//! containment makes it the section's error, as any host ERROR). Per access, not per first initialization:
//! `LocalKey::set` initializes a lazy thread-local without running its initializer. The probe is hosts' `in_claim`
//! (SessionCarry::arm to its Carried's drop), installed by the process's first pool arming (`arm_once`, debug builds
//! only); before it no claim has run. Outside a claim a pool thread's other work (a parallel VACUUM's, the invalidation
//! processing it does) is no leader's session and is not checked.
//!
//! This file's macros name `thread_local!` only through [`__thread_local`] and never at a line start: only the
//! declaring site's block is a census line.
#![forbid(unsafe_code)]

use pgsync::OnceLock;
use std::thread::LocalKey;

#[doc(hidden)]
pub use std::thread_local as __thread_local;

static POOL_PROBE: OnceLock<fn() -> bool> = OnceLock::new();

/// Installs the probe "a Gravity pool claim is in progress on this thread" (hosts `in_claim`), once per process: later
/// calls keep the first.
pub fn install_pool_probe(armed: fn() -> bool) {
    let _ = POOL_PROBE.set(armed);
}

/// A leader-class thread-local in a debug build: its `LocalKey`, checked on every access.
#[doc(hidden)]
pub struct LeaderKey<T: 'static> {
    pub key: &'static LocalKey<T>,
    pub file: &'static str,
    pub name: &'static str,
}

impl<T: 'static> core::ops::Deref for LeaderKey<T> {
    type Target = LocalKey<T>;

    #[inline]
    fn deref(&self) -> &LocalKey<T> {
        touch(self.file, self.name);
        self.key
    }
}

/// The check: ERROR XX000 inside a pool claim, nothing elsewhere.
#[doc(hidden)]
#[inline]
pub fn touch(file: &'static str, name: &'static str) {
    if POOL_PROBE.get().is_some_and(|armed| armed()) {
        touched_on_pool_thread(file, name);
    }
}

#[cold]
#[inline(never)]
fn touched_on_pool_thread(file: &str, name: &str) -> ! {
    let path = file.strip_prefix("crates/").unwrap_or(file);
    let e = types_error::PgError::error(format!("leader_tls: {path} {name} touched in a pool claim")); // alloc: Cold
    std::panic::panic_any(e)
}

/// Declares the leader-class thread-locals of one `thread_local!` block (module docs): unchanged in release builds,
/// checked per access in debug builds.
#[macro_export]
macro_rules! leader_tls {
    (thread_local! { $($body:tt)* }) => {
        #[cfg(not(debug_assertions))] $crate::__thread_local! { $($body)* }
        #[cfg(debug_assertions)] $crate::__leader_tls_checked! { $($body)* }
    };
}

/// The debug form, one static at a time (the `thread_local!` grammar: a `const` block initializer or an expression).
#[doc(hidden)]
#[macro_export]
macro_rules! __leader_tls_checked {
    () => {};
    ($(#[$attr:meta])* $vis:vis static $name:ident: $t:ty = const $init:block $(; $($rest:tt)*)?) => {
        $(#[$attr])* $vis static $name: $crate::LeaderKey<$t> = $crate::LeaderKey {
            key: { $crate::__thread_local! { static KEY: $t = const $init; } &KEY },
            file: ::core::file!(),
            name: ::core::stringify!($name),
        };
        $($crate::__leader_tls_checked! { $($rest)* })?
    };
    ($(#[$attr:meta])* $vis:vis static $name:ident: $t:ty = $init:expr $(; $($rest:tt)*)?) => {
        $(#[$attr])* $vis static $name: $crate::LeaderKey<$t> = $crate::LeaderKey {
            key: { $crate::__thread_local! { static KEY: $t = $init; } &KEY },
            file: ::core::file!(),
            name: ::core::stringify!($name),
        };
        $($crate::__leader_tls_checked! { $($rest)* })?
    };
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    // Declared in every test build (a release one too: its expansion is the plain thread_local! block); checked only in
    // a debug one.
    crate::leader_tls! {
        thread_local! {
            /// const-initialized, as most leader statics are
            #[allow(dead_code)]
            static DEPTH: Cell<u32> = const { Cell::new(7) };
            #[allow(dead_code)]
            pub(crate) static SEEN: RefCell<Vec<u32>> = RefCell::new(vec![1])
        }
    }

    #[cfg(debug_assertions)]
    const POOL: &str = "leader-tls-pool";

    #[cfg(debug_assertions)]
    fn armed() -> bool {
        std::thread::current().name() == Some(POOL)
    }

    /// C7 runtime half: a leader static touched inside a pool claim raises ERROR XX000 naming it, whichever access comes
    /// first (a write too: `LocalKey::set` skips a lazy initializer, so the check is per access); a thread in no claim
    /// reads its own copy unchecked, and the leader's values stand.
    #[cfg(debug_assertions)]
    #[test]
    fn a_leader_static_touched_in_a_pool_claim_raises_xx000() {
        crate::install_pool_probe(armed);
        DEPTH.set(8);
        SEEN.with_borrow_mut(|s| s.push(2));
        let touches: [(fn(), &str); 4] = [
            (|| DEPTH.set(1), "DEPTH"),
            (|| assert_eq!(DEPTH.with(Cell::get), 7), "DEPTH"),
            (|| SEEN.set(Vec::new()), "SEEN"),
            (|| SEEN.with_borrow_mut(Vec::clear), "SEEN"),
        ];
        for (touch, name) in touches {
            let payload = std::thread::Builder::new().name(POOL.into()).spawn(touch).unwrap().join().expect_err(name);
            let e = types_error::pg_error_from_panic(payload).unwrap_or_else(|_| panic!("{name}: not a PgError payload"));
            assert_eq!(e.sqlstate(), types_error::ERRCODE_INTERNAL_ERROR, "{name}");
            assert_eq!(e.message(), format!("leader_tls: _support/leader_tls/src/lib.rs {name} touched in a pool claim"));
        }
        std::thread::spawn(|| assert_eq!((DEPTH.get(), SEEN.take()), (7, vec![1]))).join().unwrap();
        assert_eq!((DEPTH.get(), SEEN.take()), (8, vec![1, 2]));
    }
}
