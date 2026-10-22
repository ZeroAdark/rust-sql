//! A set named before the helpers start: they start on it, a prefill thread too, and the caller keeps its own mask.
#![cfg(target_os = "linux")]

mod common;
use common::*;
use pagepool::*;

#[test]
fn helpers_started_after_the_set_run_on_it() {
    init_with(|c| {
        c.warm_zero = 4 * MIB;
        c.warm_zero_max = 4 * MIB;
    });
    let all = current_cpus();
    let want = vec![*all.last().expect("a cpu")];
    assert!(set_helper_cpus(&want), "the set, before start");
    assert!(start(), "the pool's threads started");
    wait_helpers_on(&["pg:poolclaim", "pg:poolpurge"], &want);
    wait_pins((2, 0)); // the claimer and the purger pinned themselves
    let _ = prefill(2 * SUPER, 2);
    assert_eq!(helper_pins(), (3, 0), "the prefill thread pinned itself");
    assert_eq!(current_cpus(), all, "the caller of prefill is not a helper: its mask is untouched");
    stop();
}
