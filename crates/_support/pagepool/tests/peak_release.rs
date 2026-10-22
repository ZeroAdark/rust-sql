//! ppround card 14 (`PGRUST_POOL_PEAK_RELEASE`): while the pool's claim climbs, every lazy pass marks and gives back the
//! free runs of superpages still in use as a decay mark does (seen free at one pass, still free and never taken at the
//! next), with no decay mark due; switched off, or with the claim flat, only the decay's marks do. A run taken between
//! the passes stays; the runs in use keep their bytes; part of a superpage, never a whole one.

mod common;
use common::*;
use pagepool::*;

#[test]
fn peak_release() {
    // a decay no pass of this test reaches: only the forced first pass marks
    init_with(|c| c.lazy_decay_ms = 600_000);
    let h = Hint::new(Class::Task, Tag::OTHER);
    let run = 64 << 10;
    let k = 12;
    let blocks: Vec<_> = (0..16).map(|_| take(k * run, run, h).expect("a runs-road block").cast::<u8>()).collect();
    for b in &blocks {
        // SAFETY: ours.
        unsafe { core::ptr::write_bytes(b.as_ptr(), 0x5a, k * run) };
    }
    // one block kept per superpage, the rest given: free runs inside superpages still in use
    let (mut kept, mut given, mut supers) = (Vec::new(), Vec::new(), std::collections::HashSet::new());
    for b in blocks {
        if supers.insert(b.as_ptr() as usize >> 21) {
            kept.push(b);
        } else {
            given.push(b.as_ptr() as usize);
            // SAFETY: ours, not used again.
            unsafe { give(b, k * run) };
        }
    }
    flush_shelf();
    assert!(given.len() >= 2, "free runs beside a kept block: {} given", given.len());
    assert!(given.iter().all(|&a| resident(a, k * run) > 0));
    assert_eq!(lazy_pass_now(), 0, "the decay's mark only sees them");

    // switched off, a climb changes nothing before the decay's next mark
    hooks::peak_release_switch(false);
    hooks::peak_climb(Some(true));
    assert_eq!(lazy_pass_timed_now(), 0, "off: no mark due, nothing goes");
    // on, with the claim flat: no climb, nothing goes
    hooks::peak_release_switch(true);
    hooks::peak_climb(None);
    assert_eq!(lazy_pass_timed_now(), 0, "on, the claim flat: no climb");
    assert_eq!(peak_counts().passes, 0);
    assert!(given.iter().all(|&a| resident(a, k * run) > 0), "nothing given back yet");

    // one free stretch taken again before the climb
    let again = take(k * run, run, h).expect("a runs-road block").cast::<u8>();
    let aa = again.as_ptr() as usize;
    assert!(given.contains(&aa), "the free stretch of a superpage in use is taken first");
    // SAFETY: ours.
    unsafe { core::ptr::write_bytes(again.as_ptr(), 0x6b, k * run) };

    // the claim climbs (a lazy span's commit, 2 x CLIMB_MIN, right after a flat pass, well inside CLIMB_MS): the next
    // passes give back the runs seen free and still free, no decay mark due; one pass may run out of time on a loaded
    // host, the rest goes at the next
    assert_eq!(lazy_pass_timed_now(), 0, "still flat");
    let mut grow = span(4 * CLIMB_MIN, h).expect("a lazy span");
    // SAFETY: ours.
    unsafe { commit_to(&mut grow, 2 * CLIMB_MIN) };
    let r0 = stats().releases;
    let tails = supers.len() * (32 - 2 * k) * run;
    let want = (given.len() - 1) * k * run + tails;
    let mut got = 0;
    for _ in 0..4 {
        got += lazy_pass_timed_now();
        if got >= want {
            break;
        }
    }
    assert_eq!(got, want, "the climbing passes give back the runs seen free and still free");
    assert!(stats().releases > r0);
    let p = peak_counts();
    assert!(p.on && p.passes >= 1, "{p:?}");
    assert_eq!(p.run_bytes, want as u64, "{p:?}");
    assert!(p.run_resident >= ((given.len() - 1) * k * run) as u64, "the given stretches' pages: {p:?}");
    assert_eq!((p.dead_bytes, p.dead_resident), (0, 0), "no dead runs here: {p:?}");
    for &a in given.iter().filter(|&&a| a != aa) {
        assert_eq!(resident(a, k * run), 0);
    }
    assert!(resident(aa, k * run) >= k * run, "never a run taken between the passes");
    for b in &kept {
        assert!(resident(b.as_ptr() as usize, k * run) >= k * run, "the runs in use keep their bytes");
        // SAFETY: ours.
        assert!(unsafe { core::slice::from_raw_parts(b.as_ptr(), k * run) }.iter().all(|&x| x == 0x5a));
    }
    // SAFETY: ours.
    assert!(unsafe { core::slice::from_raw_parts(again.as_ptr(), k * run) }.iter().all(|&x| x == 0x6b));

    // the climb over (forced flat): a pass gives nothing back again
    hooks::peak_climb(Some(false));
    let n = peak_counts().passes;
    assert_eq!(lazy_pass_timed_now(), 0);
    assert_eq!(peak_counts().passes, n, "a flat pass is not a climbing one");
    hooks::peak_climb(None);
    hooks::peak_release_switch(false);

    // SAFETY: ours, not used again.
    unsafe {
        give_span(grow);
        give(again, k * run);
        for b in kept {
            give(b, k * run);
        }
    }
    flush_shelf();
    balanced();
}
