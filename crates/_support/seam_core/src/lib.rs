// INVARIANT (set-once): SLOT is written only by `set()` during single-threaded
// startup, before any call, never again; it always holds a fn pointer of the
// seam's exact `Signature` — so `call()` is one relaxed load + indirect call.

// Tap boot phase: a `tap!` may `install()` only before this is closed. Closed
// once, from the postmaster's single-threaded boot sequence, strictly before
// any backend thread spawns — the same window `seam::set()` relies on. Tests
// never close it, so `install()` in test binaries only ever hits the
// double-install panic, never this one.
static TAP_BOOT_PHASE_OPEN: ::std::sync::atomic::AtomicBool =
    ::std::sync::atomic::AtomicBool::new(true);

pub fn close_tap_boot_phase() {
    TAP_BOOT_PHASE_OPEN.store(false, ::std::sync::atomic::Ordering::Relaxed);
}

pub fn tap_boot_phase_open() -> bool {
    TAP_BOOT_PHASE_OPEN.load(::std::sync::atomic::Ordering::Relaxed)
}

// Default-empty variant of `seam!`: the slot starts null (no consumer), and
// the hot-path form is `name::call_if(|f| f(args))` — one relaxed load, a
// predicted-not-taken null test, and (only when installed) an indirect call.
// No chaining: install-once, like `seam!`, plus a boot-phase gate (`seam!`
// carries no such gate because its single mandatory impl always installs
// during boot by construction; a tap's consumer is optional and boot-loaded,
// so the gate catches a stray runtime install directly).
#[macro_export]
macro_rules! tap {
    (
        $(#[$attr:meta])*
        $vis:vis fn $name:ident $(<$($lt:lifetime),+ $(,)?>)? ( $($arg:ident : $arg_ty:ty),* $(,)? ) $(-> $ret:ty)?
    ) => {
        $(#[$attr])*
        $vis mod $name {
            #![allow(dead_code, unused_imports)]
            use super::*;

            pub type Signature = $(for<$($lt),+>)? fn($($arg_ty),*) $(-> $ret)?;

            static SLOT: ::std::sync::atomic::AtomicPtr<()> =
                ::std::sync::atomic::AtomicPtr::new(::std::ptr::null_mut());

            pub fn install(implementation: Signature) {
                if !$crate::tap_boot_phase_open() {
                    panic!(concat!("tap installed after boot: ", module_path!()));
                }
                let prev = SLOT.swap(
                    implementation as *mut (),
                    ::std::sync::atomic::Ordering::Relaxed,
                );
                if !prev.is_null() {
                    panic!(concat!("tap installed twice: ", module_path!()));
                }
            }

            #[inline(always)]
            pub fn is_installed() -> bool {
                !SLOT.load(::std::sync::atomic::Ordering::Relaxed).is_null()
            }

            // Empty cost: relaxed load + null test + not-taken branch. `f`
            // is only invoked (and its captured args only materialize) on
            // the installed path, so callers can defer expensive argument
            // construction into the closure body.
            #[inline(always)]
            pub fn call_if(f: impl FnOnce(Signature)) {
                let raw = SLOT.load(::std::sync::atomic::Ordering::Relaxed);
                if !raw.is_null() {
                    __invoke(raw, f);
                }
            }

            // Fallible-tap form: a tap declared `-> R` (the executor end
            // hook, whose C consumers may ereport(ERROR) with no PG_TRY
            // between them and the caller) yields the consumer's value when
            // installed and `default` otherwise. Same not-installed cost as
            // `call_if`.
            #[inline(always)]
            pub fn call_if_or<R>(default: R, f: impl FnOnce(Signature) -> R) -> R {
                let raw = SLOT.load(::std::sync::atomic::Ordering::Relaxed);
                if raw.is_null() {
                    default
                } else {
                    __invoke(raw, f)
                }
            }

            #[cold]
            #[inline(never)]
            fn __invoke<R>(raw: *mut (), f: impl FnOnce(Signature) -> R) -> R {
                // SAFETY: install-once invariant — a non-null SLOT always
                // holds a valid `Signature` written by `install()`.
                let g: Signature = unsafe { ::std::mem::transmute::<*mut (), Signature>(raw) };
                f(g)
            }
        }
    };
}

#[macro_export]
macro_rules! seam {
    (
        $(#[$attr:meta])*
        $vis:vis fn $name:ident $(<$($lt:lifetime),+ $(,)?>)? ( $($arg:ident : $arg_ty:ty),* $(,)? ) $(-> $ret:ty)?
    ) => {
        $(#[$attr])*
        $vis mod $name {
            #![allow(dead_code, unused_imports)]
            use super::*;

            pub type Signature = $(for<$($lt),+>)? fn($($arg_ty),*) $(-> $ret)?;

            #[cold]
            #[inline(never)]
            fn __uninstalled_stub $(<$($lt),+>)? ($(_: $arg_ty),*) $(-> $ret)? {
                panic!(concat!("seam not installed: ", module_path!()))
            }

            #[inline(always)]
            fn __stub_ptr() -> *mut () {
                __uninstalled_stub as Signature as *mut ()
            }

            static SLOT: ::std::sync::atomic::AtomicPtr<()> =
                ::std::sync::atomic::AtomicPtr::new(__uninstalled_stub as Signature as *mut ());

            pub fn set(implementation: Signature) {
                // compare_exchange from the stub, NOT swap-then-panic: a
                // second install must FAIL WITHOUT CLOBBERING the first.
                // (Found by the wave-3 fuzz train: test harnesses that
                // tolerate the double-install panic via catch_unwind were
                // silently replacing the owning module's seam — the panic
                // fired AFTER the swap, so "tolerated" meant
                // last-writer-wins and a mixed seam environment.)
                if SLOT
                    .compare_exchange(
                        __stub_ptr(),
                        implementation as *mut (),
                        ::std::sync::atomic::Ordering::Relaxed,
                        ::std::sync::atomic::Ordering::Relaxed,
                    )
                    .is_err()
                {
                    panic!(concat!("seam installed twice: ", module_path!()));
                }
            }

            pub fn is_installed() -> bool {
                SLOT.load(::std::sync::atomic::Ordering::Relaxed) != __stub_ptr()
            }

            pub fn wrap(implementation: Signature) -> Signature {
                if !$crate::tap_boot_phase_open() {
                    panic!(concat!("seam wrapped after boot: ", module_path!()));
                }
                let prev = SLOT.swap(
                    implementation as *mut (),
                    ::std::sync::atomic::Ordering::Relaxed,
                );
                if prev == __stub_ptr() {
                    panic!(concat!("seam wrapped before install: ", module_path!()));
                }
                // SAFETY: a non-stub SLOT always holds a valid `Signature`.
                unsafe { ::std::mem::transmute::<*mut (), Signature>(prev) }
            }

            // Inlined into the caller: a seam call is the slot load and one indirect call, not a call to this function
            // first (a separate function per seam, each its own instruction-cache line on the statement path).
            #[inline]
            pub fn call $(<$($lt),+>)? ($($arg: $arg_ty),*) $(-> $ret)? {
                let raw = SLOT.load(::std::sync::atomic::Ordering::Relaxed);
                // SAFETY: set-once invariant (crate top) — always a valid `Signature`.
                let f: Signature = unsafe { ::std::mem::transmute::<*mut (), Signature>(raw) };
                f($($arg),*)
            }
        }
    };
}

// A `seam!` whose callers' lines matter (testflow K11, main 2026-10-12T21:08:00Z; D4214 / D4229). A #[track_caller]
// implementation reached through a seam's fn pointer reports its own definition line, so every CHECK_FOR_INTERRUPTS through
// the seam read 'crates/backend/tcop/postgres/src/lib.rs:312' in Gravity's poll-gap probe, whatever code called it.
// `seam_tracked!` declares a zero-argument seam with `seam!`'s API (set / is_installed / wrap / call) plus `set_tracked`: an
// implementation that takes the call's own location. `call()` is #[inline(always)] #[track_caller], so the location is a
// constant at each call site (or its #[track_caller] caller's), and a call costs what `seam!`'s does: one relaxed load and
// one indirect call. Until `set_tracked`, the tracked slot holds an adapter that calls the `set` implementation (a test's
// mock), so a seam with only `set` behaves as a `seam!`.
#[macro_export]
macro_rules! seam_tracked {
    (
        $(#[$attr:meta])*
        $vis:vis fn $name:ident () $(-> $ret:ty)?
    ) => {
        $(#[$attr])*
        $vis mod $name {
            #![allow(dead_code, unused_imports)]
            use super::*;

            pub type Signature = fn() $(-> $ret)?;
            pub type Tracked = fn(&'static ::core::panic::Location<'static>) $(-> $ret)?;

            #[cold]
            #[inline(never)]
            fn __uninstalled_stub() $(-> $ret)? {
                panic!(concat!("seam not installed: ", module_path!()))
            }

            #[inline(always)]
            fn __stub_ptr() -> *mut () {
                __uninstalled_stub as Signature as *mut ()
            }

            static SLOT: ::std::sync::atomic::AtomicPtr<()> =
                ::std::sync::atomic::AtomicPtr::new(__uninstalled_stub as Signature as *mut ());

            // The tracked slot until `set_tracked`: the `set` implementation (or the stub), the location dropped.
            fn __adapter(_site: &'static ::core::panic::Location<'static>) $(-> $ret)? {
                let raw = SLOT.load(::std::sync::atomic::Ordering::Relaxed);
                // SAFETY: set-once invariant (crate top) — SLOT always holds a valid `Signature`.
                let f: Signature = unsafe { ::std::mem::transmute::<*mut (), Signature>(raw) };
                f()
            }

            #[inline(always)]
            fn __adapter_ptr() -> *mut () {
                __adapter as Tracked as *mut ()
            }

            static TRACKED: ::std::sync::atomic::AtomicPtr<()> =
                ::std::sync::atomic::AtomicPtr::new(__adapter as Tracked as *mut ());

            pub fn set(implementation: Signature) {
                // compare_exchange from the stub, as `seam!`: a second install fails without clobbering the first
                if SLOT
                    .compare_exchange(
                        __stub_ptr(),
                        implementation as *mut (),
                        ::std::sync::atomic::Ordering::Relaxed,
                        ::std::sync::atomic::Ordering::Relaxed,
                    )
                    .is_err()
                {
                    panic!(concat!("seam installed twice: ", module_path!()));
                }
            }

            /// The implementation `call()` runs from now on, with the call's own location. Set once, after `set`, during
            /// single-threaded startup (the same window as `set`).
            pub fn set_tracked(implementation: Tracked) {
                if TRACKED
                    .compare_exchange(
                        __adapter_ptr(),
                        implementation as *mut (),
                        ::std::sync::atomic::Ordering::Relaxed,
                        ::std::sync::atomic::Ordering::Relaxed,
                    )
                    .is_err()
                {
                    panic!(concat!("tracked seam installed twice: ", module_path!()));
                }
            }

            pub fn is_installed() -> bool {
                SLOT.load(::std::sync::atomic::Ordering::Relaxed) != __stub_ptr()
            }

            /// `set_tracked` ran: `call()` passes its location on.
            pub fn is_tracked() -> bool {
                TRACKED.load(::std::sync::atomic::Ordering::Relaxed) != __adapter_ptr()
            }

            pub fn wrap(implementation: Signature) -> Signature {
                if !$crate::tap_boot_phase_open() {
                    panic!(concat!("seam wrapped after boot: ", module_path!()));
                }
                if is_tracked() {
                    // `call()` runs the tracked implementation: a wrap of the untracked one would never run
                    panic!(concat!("seam wrapped after set_tracked: ", module_path!()));
                }
                let prev = SLOT.swap(
                    implementation as *mut (),
                    ::std::sync::atomic::Ordering::Relaxed,
                );
                if prev == __stub_ptr() {
                    panic!(concat!("seam wrapped before install: ", module_path!()));
                }
                // SAFETY: a non-stub SLOT always holds a valid `Signature`.
                unsafe { ::std::mem::transmute::<*mut (), Signature>(prev) }
            }

            // Inlined into the caller, #[track_caller]: the location is the call site's constant (one more register
            // argument, no branch). The probe's poll site reads it (K11).
            #[inline(always)]
            #[track_caller]
            pub fn call() $(-> $ret)? {
                let raw = TRACKED.load(::std::sync::atomic::Ordering::Relaxed);
                // SAFETY: TRACKED always holds a valid `Tracked` (the adapter, or set_tracked's, set once).
                let f: Tracked = unsafe { ::std::mem::transmute::<*mut (), Tracked>(raw) };
                f(::core::panic::Location::caller())
            }
        }
    };
}

// Link-time seam: direct `extern "Rust"` symbol; fat-LTO inlines the body.
// PRECONDITION: exactly one impl, always linked; test-mocked seams stay `seam!`.
#[macro_export]
macro_rules! seam_linktime {
    (
        link_name = $sym:literal;
        $(#[$attr:meta])*
        $vis:vis fn $name:ident $(<$($lt:lifetime),+ $(,)?>)? ( $($arg:ident : $arg_ty:ty),* $(,)? ) $(-> $ret:ty)?
    ) => {
        $(#[$attr])*
        $vis mod $name {
            #![allow(dead_code, unused_imports)]
            use super::*;

            pub type Signature = $(for<$($lt),+>)? fn($($arg_ty),*) $(-> $ret)?;

            pub const LINK_NAME: &str = $sym;

            extern "Rust" {
                #[link_name = $sym]
                fn __seam_impl $(<$($lt),+>)? ($($arg : $arg_ty),*) $(-> $ret)?;
            }

            #[inline(always)]
            pub fn call $(<$($lt),+>)? ($($arg : $arg_ty),*) $(-> $ret)? {
                // SAFETY: defined once by `seam_linktime_impl!`, const-checked against `Signature`.
                unsafe { __seam_impl($($arg),*) }
            }
        }
    };
}

#[macro_export]
macro_rules! seam_linktime_impl {
    (
        export_fn = $export:ident;
        link_name = $sym:literal;
        signature = $sigpath:path;
        impl = $imp:expr;
        fn $(<$($lt:lifetime),+ $(,)?>)? ( $($arg:ident : $arg_ty:ty),* $(,)? ) $(-> $ret:ty)?
    ) => {
        // Pins the impl to the consumer-facing Signature (no ABI drift).
        const _: () = {
            #[allow(unused)]
            const __SEAM_SIG_CHECK: $sigpath = $imp;
        };

        #[export_name = $sym]
        pub extern "Rust" fn $export $(<$($lt),+>)? ($($arg : $arg_ty),*) $(-> $ret)? {
            ($imp)($($arg),*)
        }
    };
}

#[cfg(test)]
mod tests {
    crate::seam!(pub fn double(x: i32) -> i32);
    crate::seam!(pub fn never_installed(x: i32) -> i32);
    crate::seam!(pub fn installed_twice(x: i32) -> i32);
    crate::seam!(pub fn first<'a>(xs: &'a [i32]) -> &'a i32);

    #[test]
    fn install_and_call() {
        assert!(!double::is_installed());
        double::set(|x| x * 2);
        assert!(double::is_installed());
        assert_eq!(double::call(21), 42);
    }

    #[test]
    #[should_panic(expected = "seam not installed: seam_core::tests::never_installed")]
    fn uninstalled_call_panics_with_path() {
        never_installed::call(1);
    }

    #[test]
    #[should_panic(expected = "seam installed twice: seam_core::tests::installed_twice")]
    fn double_install_panics_with_path() {
        installed_twice::set(|x| x);
        installed_twice::set(|x| x + 1);
    }

    #[test]
    fn lifetime_generic_seam_returns_borrow() {
        first::set(|xs| &xs[0]);
        let data = [7, 8, 9];
        assert_eq!(*first::call(&data), 7);
    }

    crate::seam_tracked!(pub fn tracked_mock() -> u32);
    crate::seam_tracked!(pub fn tracked_site() -> u32);
    crate::seam_tracked!(pub fn tracked_never() -> u32);
    crate::seam_tracked!(pub fn tracked_twice() -> u32);

    // K11: a seam with only `set` (a test's mock) runs it through the adapter; set_tracked's implementation then gets each
    // call's own line, and a #[track_caller] caller passes its caller's
    #[test]
    fn tracked_seam_set_alone_runs_the_untracked_implementation() {
        assert!(!tracked_mock::is_installed() && !tracked_mock::is_tracked());
        tracked_mock::set(|| 7);
        assert!(tracked_mock::is_installed() && !tracked_mock::is_tracked());
        assert_eq!(tracked_mock::call(), 7);
    }

    #[track_caller]
    fn through() -> u32 {
        tracked_site::call()
    }

    #[test]
    fn tracked_seam_passes_the_call_sites_line() {
        tracked_site::set(|| 0);
        tracked_site::set_tracked(|site| site.line());
        assert!(tracked_site::is_tracked());
        let here = line!() + 1;
        let a = tracked_site::call();
        let there = line!() + 1;
        let b = through(); // a #[track_caller] caller passes its own caller's line
        assert_eq!((a, b), (here, there));
        let (c, at) = (tracked_site::call(), line!());
        assert_eq!(c, at, "a constant per call site");
    }

    #[test]
    #[should_panic(expected = "seam not installed: seam_core::tests::tracked_never")]
    fn tracked_seam_uninstalled_call_panics_with_path() {
        tracked_never::call();
    }

    #[test]
    #[should_panic(expected = "tracked seam installed twice: seam_core::tests::tracked_twice")]
    fn tracked_seam_double_set_tracked_panics_with_path() {
        tracked_twice::set_tracked(|_| 1);
        tracked_twice::set_tracked(|_| 2);
    }

    crate::tap!(pub fn counter(x: i32));
    crate::tap!(pub fn counter_empty(x: i32));
    crate::tap!(pub fn install_twice_tap(x: i32));
    crate::tap!(pub fn never_installed_tap(x: i32));

    #[test]
    fn tap_default_empty_and_call_if_skips() {
        // Its OWN tap, which no sibling test installs on: taps are
        // process-global and never uninstalled, so asserting emptiness of
        // the shared `counter` tap raced `tap_install_and_call_if_fires`
        // under the harness's intra-binary test parallelism (SE2-GATES
        // shard-B flake, 2026-07-17 — one rerun-adjudicated failure).
        assert!(!never_installed_tap::is_installed());
        let mut seen = None;
        never_installed_tap::call_if(|f| {
            seen = Some(());
            f(1);
        });
        assert!(seen.is_none());
    }

    #[test]
    fn tap_install_and_call_if_fires() {
        use std::sync::atomic::{AtomicI32, Ordering};
        static TOTAL: AtomicI32 = AtomicI32::new(0);
        fn add(x: i32) {
            TOTAL.fetch_add(x, Ordering::Relaxed);
        }
        counter::install(add);
        assert!(counter::is_installed());
        counter::call_if(|f| f(5));
        counter::call_if(|f| f(7));
        assert_eq!(TOTAL.load(Ordering::Relaxed), 12);
    }

    #[test]
    #[should_panic(expected = "tap installed twice: seam_core::tests::install_twice_tap")]
    fn tap_double_install_panics_with_path() {
        install_twice_tap::install(|_| {});
        install_twice_tap::install(|_| {});
    }

    crate::seam_linktime!(
        link_name = "__pgrust_seam_test_triple";
        pub fn triple(x: i32) -> i32
    );
    crate::seam_linktime!(
        link_name = "__pgrust_seam_test_first_lt";
        pub fn first_lt<'a>(xs: &'a [i32]) -> &'a i32
    );

    fn triple_impl(x: i32) -> i32 {
        x * 3
    }
    crate::seam_linktime_impl!(
        export_fn = __seam_export_triple;
        link_name = "__pgrust_seam_test_triple";
        signature = triple::Signature;
        impl = triple_impl;
        fn (x: i32) -> i32
    );

    fn first_lt_impl(xs: &[i32]) -> &i32 {
        &xs[0]
    }
    crate::seam_linktime_impl!(
        export_fn = __seam_export_first_lt;
        link_name = "__pgrust_seam_test_first_lt";
        signature = first_lt::Signature;
        impl = first_lt_impl;
        fn <'a>(xs: &'a [i32]) -> &'a i32
    );

    #[test]
    fn linktime_seam_calls_owner_export() {
        assert_eq!(triple::call(14), 42);
    }

    #[test]
    fn linktime_lifetime_seam_returns_borrow() {
        let data = [11, 22, 33];
        assert_eq!(*first_lt::call(&data), 11);
    }
}
