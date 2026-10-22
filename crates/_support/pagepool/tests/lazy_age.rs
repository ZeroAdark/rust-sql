//! pagepool P3 (F1): with pool_lazy_decay_ms, the purger's lazy pass gives back a warm lazy span's pages once it has been
//! warm that long (not before; the span stays warm with its commit 0, and the next ask takes it), and a dead run at the
//! second pass that finds it dead (never one taken live between); a dead range with no page resident goes back with no
//! remap, and one with a page on a swap device reads zero after (cleared); pressure still takes the runs a pass has seen.

mod common;
use common::*;
use core::ptr::NonNull;
use pagepool::*;
use std::time::Duration;

#[test]
fn lazy_age() {
    init_with(|c| c.lazy_decay_ms = 200);
    // card 2 (ppround): resident reads, pass budgets and remap counts are exact, or a loud SKIP if the host swapped
    let sw = swap_counts();
    let h = Hint::new(Class::Task, Tag::REGION);

    // (a) a warm lazy span: back only past the decay
    let mut l = span(64 * MIB, h).expect("a lazy span");
    assert!(l.is_lazy());
    // SAFETY: ours.
    unsafe { commit_to(&mut l, 32 * MIB) };
    let la = l.as_ptr() as usize;
    touch(la, 32 * MIB);
    let s0 = stats();
    // SAFETY: ours, not used again.
    unsafe { give_span(l) };
    assert_eq!(lazy_pass_now(), 0, "a span warm for less than the decay stays");
    swap_exact!(sw, resident(la, 32 * MIB) >= 32 * MIB, "the warm span resident");
    std::thread::sleep(Duration::from_millis(250));
    let r = resident(la, 32 * MIB);
    let got = lazy_pass_now();
    swap_exact!(sw, got == 32 * MIB, "past the decay its commit goes back: {got}");
    assert_eq!(resident(la, 32 * MIB), 0);
    let s1 = stats();
    assert_eq!(s1.lazy_committed, s0.lazy_committed - 32 * MIB);
    // released bytes count resident pages only: on a swap host under memory pressure (a loaded precheck box) some of the
    // span may sit on swap by the pass, and those pages are cleared, not counted
    assert!(r == 0 || (s1.releases > s0.releases && s1.released_bytes > s0.released_bytes), "{r} resident: {s0:?} -> {s1:?}");
    assert!(s1.released_bytes <= s0.released_bytes + (32 * MIB) as u64, "never more than the commit");
    let mut l = span(64 * MIB, h).expect("the warm span again");
    assert_eq!(l.as_ptr() as usize, la, "still warm: the next ask takes it");
    assert_eq!(l.committed(), 0);

    // (b) dead runs: back at the second pass, never one taken live between, no remap for one with no page resident
    // SAFETY: ours.
    unsafe { commit_to(&mut l, 64 * MIB) };
    touch(la, 48 * MIB);
    let p = NonNull::new(l.as_ptr()).unwrap();
    // SAFETY: inside our span; nothing reads these until take_pages.
    unsafe {
        give_pages(p, 0, 16 * MIB);
        give_pages(p, 16 * MIB, 16 * MIB);
        give_pages(p, 48 * MIB, 16 * MIB);
    }
    assert_eq!(stats().dead, 48 * MIB);
    assert_eq!(lazy_pass_now(), 0, "the first pass only sees them");
    swap_exact!(sw, resident(la, 16 * MIB) >= 16 * MIB && resident(la + 16 * MIB, 16 * MIB) >= 16 * MIB, "the dead runs resident");
    // SAFETY: as above.
    assert_eq!(unsafe { take_pages(p, 16 * MIB, 16 * MIB) }, Zeroed::No, "live again, its bytes kept");
    let r0 = stats().releases;
    pagepool::hooks::clear();
    let got = lazy_pass_now();
    swap_exact!(sw, got == 32 * MIB, "the second pass gives back the two still dead: {got}");
    assert_eq!(resident(la, 16 * MIB), 0);
    swap_exact!(sw, resident(la + 16 * MIB, 16 * MIB) >= 16 * MIB, "never a run taken live between");
    // off Linux no residency is read: every range is remapped there
    let remaps = if cfg!(target_os = "linux") { 8 } else { 16 };
    let got = stats().releases - r0;
    swap_exact!(sw, got == remaps, "16 MiB in 2 MiB remaps; the range with no page resident with none: {got}");
    // never faulted, so on no swap device either, yet D4869: cleared by MADV_DONTNEED all the same (non-residency is never
    // trusted to read zero: a swap device enabled after the pool's one /proc/swaps read could hold its pages)
    let mut cleared = 0;
    assert!(pagepool::hooks::each(|op, at, len, _| {
        if op == pagepool::hooks::Op::Madvise && at >= la + 48 * MIB && at < la + 64 * MIB {
            cleared += len;
        }
    }));
    if cfg!(target_os = "linux") {
        assert_eq!(cleared, 16 * MIB, "D4869: the range with no page resident or swapped is cleared, not trusted");
    }
    assert_eq!(stats().dead, 0);
    // SAFETY: as above.
    unsafe {
        assert_eq!(take_pages(p, 0, 16 * MIB), Zeroed::Yes);
        assert_eq!(take_pages(p, 48 * MIB, 16 * MIB), Zeroed::Yes);
    }

    // (c) pressure takes a run a pass has seen
    // SAFETY: as above.
    unsafe { give_pages(p, 32 * MIB, 16 * MIB) };
    assert_eq!(lazy_pass_now(), 0);
    set_pressure(Pressure::Floor);
    purge_now(usize::MAX, None);
    assert_eq!(stats().dead, 0);
    assert_eq!(resident(la + 32 * MIB, 16 * MIB), 0);
    // SAFETY: as above.
    assert_eq!(unsafe { take_pages(p, 32 * MIB, 16 * MIB) }, Zeroed::Yes);

    // (d) a dead range whose pages went out to a swap device (MADV_PAGEOUT; with none they stay resident) still reads
    // zero once given back: cleared (or remapped), never taken as empty
    let d = la + 32 * MIB;
    // SAFETY: inside our span, live again.
    unsafe { core::ptr::write_bytes(d as *mut u8, 0xab, 2 * MIB) };
    // SAFETY: as above.
    unsafe { give_pages(p, 32 * MIB, 2 * MIB) };
    #[cfg(target_os = "linux")]
    // SAFETY: advice on our own dead range: its pages go out, or stay.
    unsafe { libc::madvise(d as *mut libc::c_void, 2 * MIB, libc::MADV_PAGEOUT) };
    // the page-out above is this test's own swap: count the host's from here
    let sw = swap_counts();
    assert_eq!(lazy_pass_now(), 0, "the first pass only sees it");
    let got = lazy_pass_now();
    swap_exact!(sw, got == 2 * MIB, "the second gives it back: {got}");
    // SAFETY: as above.
    assert_eq!(unsafe { take_pages(p, 32 * MIB, 2 * MIB) }, Zeroed::Yes);
    // SAFETY: inside our span, live again.
    let back = unsafe { core::slice::from_raw_parts(d as *const u8, 2 * MIB) };
    assert!(back.iter().all(|&b| b == 0), "a page given back reads zero");

    // SAFETY: ours, not used again.
    unsafe { give_span(l) };
    flush_shelf();
    balanced();
}
