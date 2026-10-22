use super::*;

#[test]
fn gcd_matches_c() {
    assert_eq!(gcd(12, 18), 6);
    assert_eq!(gcd(7, 13), 1);
    assert_eq!(gcd(0, 5), 5);
}

#[test]
fn relative_prime_bounds_and_coprimality() {
    let mut r = sampler_random_init_state(42);
    assert_eq!(random_relative_prime(0, &mut r).unwrap(), 1);
    assert_eq!(random_relative_prime(1, &mut r).unwrap(), 1);
    for n in [2u32, 12, 97, 4096] {
        for seed in 0..8u32 {
            let mut r = sampler_random_init_state(seed);
            let p = random_relative_prime(n, &mut r).unwrap();
            assert!(p > 0 && p < n && gcd(p, n) == 1);
        }
    }
}

fn walk(s: &mut SystemTimeSampler, nblocks: u32) -> Vec<u32> {
    let mut v = vec![];
    loop {
        let b = s.next_sample_block(nblocks).unwrap();
        if b == InvalidBlockNumber {
            return v;
        }
        v.push(b);
    }
}

#[test]
fn zero_budget_returns_no_blocks() {
    let mut s = SystemTimeSampler::default();
    s.begin_sample_scan(0.0, 5).unwrap();
    assert_eq!(s.next_sample_block(31).unwrap(), InvalidBlockNumber);
}

#[test]
fn large_budget_visits_every_block_exactly_once() {
    for nblocks in [1u32, 2, 13, 37] {
        let mut s = SystemTimeSampler::default();
        s.begin_sample_scan(1e9, 12345).unwrap();
        let mut blocks = walk(&mut s, nblocks);
        assert_eq!(blocks.len(), nblocks as usize);
        blocks.sort_unstable();
        blocks.dedup();
        assert_eq!(blocks.len(), nblocks as usize, "walk missed or repeated a block");
    }
}

#[test]
fn tuple_walk_in_order() {
    let mut s = SystemTimeSampler::default();
    s.begin_sample_scan(1e9, 3).unwrap();
    for off in 1..=5u16 {
        assert_eq!(s.next_sample_tuple(5), off);
    }
    assert_eq!(s.next_sample_tuple(5), InvalidOffsetNumber);
}

#[test]
fn empty_relation() {
    let mut s = SystemTimeSampler::default();
    s.begin_sample_scan(1000.0, 3).unwrap();
    assert_eq!(s.next_sample_block(0).unwrap(), InvalidBlockNumber);
}

#[test]
fn negative_or_nan_time_is_2202h() {
    for bad in [-1.0, f64::NAN] {
        let mut s = SystemTimeSampler::default();
        let err = s.begin_sample_scan(bad, 0).unwrap_err();
        assert_eq!(err.sqlstate(), ERRCODE_INVALID_TABLESAMPLE_ARGUMENT);
        assert_eq!(err.message(), "sample collection time must not be negative");
    }
}

#[test]
fn sample_size_estimates_match_c() {
    assert_eq!(sample_scan_get_sample_size(Some(40.0), 4.0, 100, 10000.0), (10, 1000.0));
    assert_eq!(sample_scan_get_sample_size(None, 4.0, 100, 10000.0), (100, 10000.0));
    assert_eq!(sample_scan_get_sample_size(Some(f64::NAN), 4.0, 100, 10000.0), (100, 10000.0));
    assert_eq!(sample_scan_get_sample_size(Some(8.0), 0.0, 100, 10000.0), (8, 800.0));
    assert_eq!(sample_scan_get_sample_size(Some(7.0), 4.0, 0, 0.0), (1, 1.0));
}

/// lib.rs L125 (sampler_random_fract zero-draw retry) — phase-1 residual
/// witness (p1-wavea): xoroshiro128** with s0 == 0 emits val == 0 on the
/// first draw regardless of s1, so next_f64() == 0.0 exactly and the
/// C-parity do-while retry arm executes, then terminates on the mixed state.
/// Same behavior as C sampling.c sampler_random_fract.
#[test]
fn sampler_random_fract_retries_zero_draw() {
    assert_eq!(PgPrng::from_raw(0, 1).next_f64(), 0.0);
    let mut r = PgPrng::from_raw(0, 1);
    let res = sampler_random_fract(&mut r);
    assert!(res != 0.0 && res > 0.0 && res < 1.0);
}

#[test]
fn stride_retry_checks_pending_interrupt() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CHECKS: AtomicUsize = AtomicUsize::new(0);
    postgres_seams::check_for_interrupts::set(|| {
        if CHECKS.fetch_add(1, Ordering::Relaxed) == 0 {
            Ok(())
        } else {
            Err(Box::new(
                PgError::error("canceling statement due to user request")
                    .with_sqlstate(types_error::ERRCODE_QUERY_CANCELED),
            ))
        }
    });
    let seed = (0..100)
        .find(|&seed| {
            let mut r = sampler_random_init_state(seed);
            sampler_random_fract(&mut r);
            let candidate = (sampler_random_fract(&mut r) * 12.0) as u32;
            candidate == 0 || gcd(candidate, 12) > 1
        })
        .unwrap();
    let mut sampler = SystemTimeSampler::default();
    sampler.begin_sample_scan(1e9, seed).unwrap();
    init_small::globals::SetInterruptPending(true);
    let result = sampler.next_sample_block(12);
    init_small::globals::SetInterruptPending(false);
    assert_eq!(CHECKS.load(Ordering::Relaxed), 2);
    let err = result.unwrap_err();
    assert_eq!(err.sqlstate(), types_error::ERRCODE_QUERY_CANCELED);
    assert_eq!(err.message(), "canceling statement due to user request");
}
