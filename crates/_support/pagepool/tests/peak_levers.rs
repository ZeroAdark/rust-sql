//! ppround card 14 v3 (`PGRUST_POOL_PEAK_RELEASE` levers), on climbing passes: LEVER_SKIP, no walk part under `SKIP_MIN`
//! of releasable bytes; LEVER_AGE, a walk at most every `WALK_MS`, so a run freed after the last walk waits for the walk
//! after the next one; LEVER_DEAD1, a dead run goes back at the first walk after its give (v2: the second); LEVER_WARM1,
//! a freed lazy span goes back at the first walk `WALK_MS` after its give (v2: past the decay).

mod common;
use common::*;
use core::ptr::NonNull;
use pagepool::*;
use std::time::Duration;

const RUN: usize = 64 << 10;
const K: usize = 12;

/// 16 blocks of `K` runs (two a superpage); one kept per superpage, the rest given: free runs inside superpages in use.
fn free_runs(h: Hint) -> (Vec<NonNull<u8>>, Vec<usize>) {
    let blocks: Vec<_> = (0..16).map(|_| take(K * RUN, RUN, h).expect("a runs-road block").cast::<u8>()).collect();
    let (mut kept, mut given, mut supers) = (Vec::new(), Vec::new(), std::collections::HashSet::new());
    for b in blocks {
        // SAFETY: ours.
        unsafe { core::ptr::write_bytes(b.as_ptr(), 0x5a, K * RUN) };
        if supers.insert(b.as_ptr() as usize >> 21) {
            kept.push(b);
        } else {
            given.push(b.as_ptr() as usize);
            // SAFETY: ours, not used again.
            unsafe { give(b, K * RUN) };
        }
    }
    flush_shelf();
    (kept, given)
}

fn all_resident(given: &[usize]) -> bool {
    given.iter().all(|&a| resident(a, K * RUN) == K * RUN)
}

fn none_resident(given: &[usize]) -> bool {
    given.iter().all(|&a| resident(a, K * RUN) == 0)
}

fn walk_gap() {
    std::thread::sleep(Duration::from_millis(WALK_MS as u64 + 30));
}

#[test]
fn peak_levers() {
    // a decay no pass of this test reaches: only the forced first pass marks
    init_with(|c| c.lazy_decay_ms = 600_000);
    let sw = swap_counts();
    let h = Hint::new(Class::Task, Tag::OTHER);
    hooks::peak_release_switch(true);
    let (mut kept, given) = free_runs(h);
    assert!(given.len() >= 2 && given.len() * K * RUN < SKIP_MIN, "free runs under SKIP_MIN: {} given", given.len());
    assert_eq!(lazy_pass_now(), 0, "the decay's mark only sees them");
    hooks::peak_climb(Some(true));

    // SKIP: under SKIP_MIN of free runs (and no dead run), a climbing walk skips both parts; nothing goes
    hooks::peak_levers(LEVER_SKIP);
    let p0 = peak_counts();
    assert_eq!(lazy_pass_timed_now(), 0, "SKIP: nothing walked");
    let p1 = peak_counts();
    assert_eq!((p1.levers, p1.skips - p0.skips, p1.run_bytes), (LEVER_SKIP, 2, p0.run_bytes), "both parts skipped: {p1:?}");
    assert!(all_resident(&given) || swap_moved(sw), "skipped: the seen runs stay");

    // AGE: the walk takes the runs seen at the decay's mark; runs freed after it wait for two walks WALK_MS apart
    hooks::peak_levers(LEVER_AGE);
    walk_gap();
    let got = lazy_pass_timed_now();
    assert!(got >= given.len() * K * RUN, "AGE's first walk: the runs seen at the mark go: {got}");
    assert!(none_resident(&given), "their pages left");
    let (kept2, given2) = free_runs(h);
    let p2 = peak_counts();
    assert_eq!(lazy_pass_timed_now(), 0, "within WALK_MS: no walk");
    assert_eq!(peak_counts().waits - p2.waits, 1, "the pass waited");
    walk_gap();
    assert_eq!(lazy_pass_timed_now(), 0, "the next walk only sees the new free runs");
    assert!(all_resident(&given2) || swap_moved(sw), "freed less than a walk ago: kept");
    walk_gap();
    assert!(lazy_pass_timed_now() >= given2.len() * K * RUN, "seen at two walks WALK_MS apart: gone");
    assert!(none_resident(&given2));
    kept.extend(kept2);

    // DEAD1: a live block's dead runs go back at the first climbing walk (v2, no lever: the second)
    let dead_round = |levers: u32| -> u32 {
        hooks::peak_levers(levers);
        let b = take(16 * RUN, RUN, h).expect("a runs-road block").cast::<u8>();
        // SAFETY: ours.
        unsafe { core::ptr::write_bytes(b.as_ptr(), 0x6b, 16 * RUN) };
        let d0 = stats().dead;
        // SAFETY: inside our block.
        unsafe { give_pages(b, 4 * RUN, 8 * RUN) };
        assert_eq!(stats().dead - d0, 8 * RUN);
        let mut walks = 0;
        while stats().dead > d0 && walks < 4 {
            walk_gap();
            lazy_pass_timed_now();
            walks += 1;
        }
        assert_eq!(stats().dead, d0, "the dead runs went back");
        assert_eq!(resident(b.as_ptr() as usize + 4 * RUN, 8 * RUN), 0);
        // SAFETY: the range given above, taken back before the block is given.
        assert_eq!(unsafe { take_pages(b, 4 * RUN, 8 * RUN) }, Zeroed::Yes);
        // SAFETY: ours, given once.
        unsafe { give(b, 16 * RUN) };
        walks
    };
    assert_eq!(dead_round(LEVER_DEAD1), 1, "DEAD1: the first walk");
    assert_eq!(dead_round(0), 2, "v2: the second walk");

    // WARM1: a freed lazy span goes back at the first walk WALK_MS after its give (v2, no lever: only past the decay)
    let warm_round = |levers: u32| -> bool {
        hooks::peak_levers(levers);
        let mut l = span(64 * MIB, Hint::new(Class::Task, Tag::REGION)).expect("a lazy span");
        assert!(l.is_lazy());
        // SAFETY: ours.
        unsafe { commit_to(&mut l, 32 * MIB) };
        let la = l.as_ptr() as usize;
        touch(la, 32 * MIB);
        let c0 = stats().lazy_committed;
        // SAFETY: ours, not used again.
        unsafe { give_span(l) };
        walk_gap();
        lazy_pass_timed_now();
        let gone = stats().lazy_committed + 32 * MIB == c0;
        assert!(!gone || resident(la, 32 * MIB) == 0 || swap_moved(sw), "a span given back left the process");
        gone
    };
    assert!(warm_round(LEVER_WARM1), "WARM1: the warm span went back at the first walk");
    assert!(!warm_round(0), "v2: the warm span stays until the decay");

    hooks::peak_levers(0);
    hooks::peak_climb(None);
    hooks::peak_release_switch(false);
    for b in kept {
        // SAFETY: ours, given once.
        unsafe { give(b, K * RUN) };
    }
    flush_shelf();
    balanced();
}
