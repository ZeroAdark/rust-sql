//! Running helpers follow the CPU set named by `set_helper_cpus`; the refusals change nothing.
#![cfg(target_os = "linux")]

mod common;
use common::*;
use pagepool::*;

#[test]
fn running_helpers_follow_the_named_cpus() {
    init_with(|c| {
        c.warm_zero = 4 * MIB;
        c.warm_zero_max = 4 * MIB;
    });
    assert!(start(), "the pool's threads started");
    let all = current_cpus();
    let helpers = ["pg:poolclaim", "pg:poolpurge"];
    wait_helpers_on(&helpers, &all);
    assert!(!set_helper_cpus(&[]), "an empty set is refused");
    assert!(!set_helper_cpus(&[HELPER_MAX_CPUS]), "a CPU past CPU_SETSIZE is refused");
    assert_eq!(helper_pins(), (0, 0), "a refused set pins nothing");
    let want = vec![all[0]];
    assert!(set_helper_cpus(&want), "the set");
    assert!(!set_helper_cpus(&all), "a second set is refused");
    wait_helpers_on(&helpers, &want);
    wait_pins((2, 0)); // the claimer and the purger pinned themselves, once each
    assert_eq!(current_cpus(), all, "the caller is not a helper: its mask is untouched");
    stop();
}
