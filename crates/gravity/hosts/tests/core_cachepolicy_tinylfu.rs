//! cachepolicy (C5, `cache_policy`): the condition cache under W-TinyLFU (2) and under lru (0, the default). A hot set
//! looked up three times survives a one-off scan of ten times the cache under W-TinyLFU and none of it under lru; under lru
//! no W-TinyLFU state moves (every entry in the window, its bytes the cache's). Its own binary: the knob is process-wide.

use std::any::Any;
use std::sync::{Arc, Mutex};

use gravity_hosts::ops::scan::pgrc::cond_cache::{self, Hit};
use mempolicy::consts::{set, K};

/// The tests here move the process-wide `cache_policy` and the condition cache's budget: one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

const HOT: u64 = 50;
const SCAN: u64 = 1000;

/// One run at policy `policy`: the cache holds 100 entries; `HOT` entries looked up three times (each published), then
/// `SCAN` one-off entries. The hot entries still resident, and the W-TinyLFU census's moves over the run.
fn run(policy: u64) -> (u64, [u64; 7]) {
    set(K::CachePolicy, policy);
    cond_cache::drop_all();
    cond_cache::set_capacity(100 << 20);
    let o: Arc<dyn Any + Send + Sync> = Arc::new(0u64);
    let _ = cond_cache::entry_for([1, 0], [0, 0], &o, 8, 64);
    let one = cond_cache::held_bytes();
    assert!(one > 0);
    cond_cache::drop_all();
    cond_cache::set_capacity(one * 100);
    let c0 = cond_cache::tinylfu_census();
    // bounded("const-domain: the test's 3 rounds over HOT entries")
    for _ in 0..3 {
        for i in 0..HOT {
            cond_cache::entry_for([9, i], [0, 0], &o, 8, 64).put(0, &[0], 64, 0);
        }
    }
    // bounded("const-domain: the test's SCAN one-off entries")
    for i in 0..SCAN {
        let _ = cond_cache::entry_for([10, i], [0, 0], &o, 8, 64);
        assert!(cond_cache::held_bytes() <= one * 100, "the budget holds");
    }
    let c1 = cond_cache::tinylfu_census();
    let kept = (0..HOT).filter(|&i| cond_cache::entry_for([9, i], [0, 0], &o, 8, 64).get(0) == Hit::None).count() as u64;
    let mut d = c1;
    // bounded("const-domain: the census's 4 move counts")
    for i in 0..4 {
        d[i] = c1[i] - c0[i];
    }
    (kept, d)
}

#[test]
fn cachepolicy_tinylfu_a_hot_set_survives_a_one_off_scan_that_lru_keeps_none_of() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (kept, d) = run(2);
    assert_eq!(kept, HOT, "W-TinyLFU keeps every hot entry through the scan");
    assert!(d[1] >= SCAN - 100, "the scan's entries are refused at the window: {d:?}");
    assert_eq!(d[0], 0, "none of the scan's entries beats the main space's least recent one: {d:?}");
    assert!(d[2] >= HOT - 1, "the hot entries' probation hits promoted them: {d:?}");
    assert_eq!(d[4] + d[5] + d[6], cond_cache::held_bytes(), "the segments' bytes are the cache's");
    let (kept, d) = run(0);
    assert_eq!(kept, 0, "lru: the scan pushed every hot entry out");
    assert_eq!(&d[..4], &[0, 0, 0, 0], "lru moves no W-TinyLFU state");
    assert_eq!((d[4], d[5], d[6]), (cond_cache::held_bytes(), 0, 0), "lru: every entry in the window");
    set(K::CachePolicy, 0);
    cond_cache::drop_all();
}

/// Under W-TinyLFU an entry looked up more often than the main space's least recent one enters main when the window pushes it out,
/// and the memory budget's trim takes probation before the window and protected.
#[test]
fn cachepolicy_tinylfu_a_frequent_entry_gets_in_and_the_trim_takes_probation_first() {
    let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (kept, _) = run(2);
    assert_eq!(kept, HOT);
    let o: Arc<dyn Any + Send + Sync> = Arc::new(1u64);
    let c0 = cond_cache::tinylfu_census();
    // bounded("const-domain: the test's 6 lookups")
    for _ in 0..6 {
        cond_cache::entry_for([11, 0], [0, 0], &o, 8, 64).put(0, &[0], 64, 0);
    }
    let _ = cond_cache::entry_for([11, 1], [0, 0], &o, 8, 64);
    let c1 = cond_cache::tinylfu_census();
    assert_eq!(c1[0] - c0[0], 1, "the frequent entry admitted past the main space's least recent");
    assert_eq!(cond_cache::entry_for([11, 0], [0, 0], &o, 8, 64).get(0), Hit::None, "it is resident");
    // the trim: probation (now the frequent entry, promoted by the hit just above, is in protected) goes before protected
    let protected = cond_cache::tinylfu_census()[6];
    let window = cond_cache::tinylfu_census()[4];
    cond_cache::trim_to(protected + window);
    let c2 = cond_cache::tinylfu_census();
    assert_eq!((c2[5], c2[6], c2[4]), (0, protected, window), "probation went first, protected and the window stayed");
    set(K::CachePolicy, 0);
    cond_cache::drop_all();
}
