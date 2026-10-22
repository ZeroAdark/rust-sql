//! pagepool P3 (F2 + the pass budget): with pool_lazy_cap and pool_age_pass_max, the purger's lazy pass gives back what
//! of the warm lazy spans and dead runs is past the cap, the oldest warm span first and from the top of its commit down,
//! then dead runs, at most the budget remapped a pass (the rest at the next).

mod common;
use common::*;
use core::ptr::NonNull;
use pagepool::*;
use std::time::Duration;

/// Exact, or a loud SKIP when the host swapped a page in or out since `$sw` (`swap_counts()`): a page on swap is not
/// resident, so a pass's budget (resident bytes given back) and the resident reads move with it.
macro_rules! exact {
    ($sw:expr, $cond:expr, $($msg:tt)+) => {
        if !$cond {
            if swap_moved($sw) {
                // straight to fd 2: libtest captures eprintln! and shows it only for a failed test (ppround card 2)
                use std::io::Write as _;
                let _ = writeln!(std::io::stderr(), "SKIP: the host swapped during the test (pswpin / pswpout moved): `{}` not exact here: {}", stringify!($cond), format!($($msg)+));
                return;
            }
            panic!("`{}` failed: {}", stringify!($cond), format!($($msg)+));
        }
    };
}

#[test]
fn lazy_cap() {
    init_with(|c| {
        c.lazy_cap = 16 * MIB;
        c.age_pass_max = 8 * MIB;
    });
    let sw = swap_counts();
    let h = Hint::new(Class::Task, Tag::REGION);
    let mut take = || {
        let mut s = span(64 * MIB, h).expect("a lazy span");
        // SAFETY: ours.
        unsafe { commit_to(&mut s, 32 * MIB) };
        touch(s.as_ptr() as usize, 32 * MIB);
        s
    };
    let (a, b) = (take(), take());
    let (aa, ba) = (a.as_ptr() as usize, b.as_ptr() as usize);
    // SAFETY: ours, not used again.
    unsafe { give_span(a) };
    std::thread::sleep(Duration::from_millis(5));
    // SAFETY: as above.
    unsafe { give_span(b) };
    // 64 MiB warm against a 16 MiB cap, 8 MiB a pass
    let got = lazy_pass_now();
    exact!(sw, got == 8 * MIB, "one budget a pass: {got}");
    exact!(sw, resident(aa + 24 * MIB, 8 * MIB) == 0, "the oldest span's top first");
    exact!(sw, resident(aa, 24 * MIB) >= 24 * MIB && resident(ba, 32 * MIB) >= 32 * MIB, "the rest resident");
    for _ in 0..3 {
        let got = lazy_pass_now();
        exact!(sw, got == 8 * MIB, "a budget: {got}");
    }
    exact!(sw, resident(aa, 32 * MIB) == 0, "the oldest span all given back");
    for _ in 0..2 {
        let got = lazy_pass_now();
        exact!(sw, got == 8 * MIB, "a budget: {got}");
    }
    let got = lazy_pass_now();
    exact!(sw, got == 0, "at the cap: nothing more: {got}");
    exact!(sw, resident(ba, 16 * MIB) >= 16 * MIB, "the newer span's bottom stays");
    exact!(sw, resident(ba + 16 * MIB, 16 * MIB) == 0, "its top given back");

    // dead runs count against the cap too, after the warm spans (a span too big for either warm one: a new one)
    let mut c = span(160 * MIB, h).expect("a lazy span");
    // SAFETY: ours.
    unsafe { commit_to(&mut c, 64 * MIB) };
    let ca = c.as_ptr() as usize;
    touch(ca, 64 * MIB);
    let p = NonNull::new(c.as_ptr()).unwrap();
    // SAFETY: inside our span; nothing reads it until take_pages.
    unsafe { give_pages(p, 0, 24 * MIB) };
    for _ in 0..2 {
        let got = lazy_pass_now();
        exact!(sw, got == 8 * MIB, "a budget: {got}");
    }
    exact!(sw, resident(ba, 32 * MIB) == 0, "the warm span first");
    exact!(sw, resident(ca, 24 * MIB) >= 24 * MIB, "the dead runs still resident");
    let got = lazy_pass_now();
    exact!(sw, got == 8 * MIB, "then dead runs, in address order: {got}");
    exact!(sw, resident(ca, 8 * MIB) == 0, "the lowest dead runs first");
    exact!(sw, resident(ca + 8 * MIB, 16 * MIB) >= 16 * MIB, "the rest of the dead runs stay");
    let got = lazy_pass_now();
    exact!(sw, got == 0, "at the cap: {got}");
    assert_eq!(stats().dead, 16 * MIB);
    // SAFETY: as above.
    unsafe {
        assert_eq!(take_pages(p, 0, 24 * MIB), Zeroed::Yes);
        give_span(c);
    }
    flush_shelf();
    balanced();
}
