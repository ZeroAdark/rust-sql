//! A set the kernel refuses leaves every helper's mask as it was and is counted.
#![cfg(target_os = "linux")]

mod common;
use common::*;
use pagepool::*;

#[test]
fn a_refused_set_leaves_the_helpers_unpinned() {
    init_with(|c| {
        c.warm_zero = 4 * MIB;
        c.warm_zero_max = 4 * MIB;
    });
    assert!(start(), "the pool's threads started");
    let all = current_cpus();
    let Some(out) = (0..HELPER_MAX_CPUS).rev().find(|c| !all.contains(c)) else {
        eprintln!("every CPU number is allowed here: nothing to refuse");
        return;
    };
    assert!(set_helper_cpus(&[out]), "the set is taken (the kernel judges it at each helper's wake)");
    wait_pins((0, 2)); // both helpers' sets refused, none applied
    wait_helpers_on(&["pg:poolclaim", "pg:poolpurge"], &all);
    stop();
}
