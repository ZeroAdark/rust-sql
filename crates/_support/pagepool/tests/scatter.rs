//! pagepool P3 (d): a span ask finds scattered warm superpages but no warm run of its length: it reuses the warm ones
//! (moved into its run's Cold superpages: no claim, no fault) before it claims new. Before, a stress run's pool claimed
//! up to its cap while 8 GiB sat warm and scattered.

mod common;
use common::*;
use pagepool::*;

#[test]
fn a_span_ask_reuses_scattered_warm_superpages_before_it_claims_cold() {
    setup();
    let h = Hint::new(Class::Task, Tag::OTHER);
    // eight single superpages, written, then every other one back: four warm, none next to another
    let ones: Vec<_> = (0..8).map(|_| span(SUPER, h).expect("a superpage")).collect();
    for s in &ones {
        touch(s.as_ptr() as usize, SUPER);
    }
    let mut kept = Vec::new();
    for (k, s) in ones.into_iter().enumerate() {
        if k % 2 == 0 {
            // SAFETY: ours, not used again
            unsafe { give_span(s) };
        } else {
            kept.push(s);
        }
    }
    let warm0 = stats().warm_dirty;
    assert!(warm0 >= 4 * SUPER, "four scattered warm superpages: {warm0}");
    let (c0, m0) = (stats().claimed, moved_in());
    // a 4-superpage span: no warm run of four, so its run is Cold: filled from the scattered warm ones
    let s = span(4 * SUPER, h).expect("a pool span");
    assert!(!s.is_lazy());
    let moved = moved_in() - m0;
    assert_eq!(moved, 4, "every Cold superpage of the run filled from a warm one");
    assert_eq!(stats().claimed, c0, "no claim: the warm pages were reused");
    assert_eq!(stats().warm_dirty, warm0 - 4 * SUPER, "four warm superpages went into the span");
    assert_eq!(resident(s.as_ptr() as usize, s.len()), s.len(), "the moved pages are in memory: no fault to write them");
    // SAFETY: ours, not used again
    unsafe { give_span(s) };
    for s in kept {
        // SAFETY: ours, not used again
        unsafe { give_span(s) };
    }
    let _ = purge_now(usize::MAX >> 1, None);
    balanced();
}
