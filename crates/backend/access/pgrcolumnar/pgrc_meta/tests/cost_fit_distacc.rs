//! P5-36's fit (the stats lane; gap-optoverfit mechanism C): the distance the word lane's DistAcc slot prefetch runs ahead of
//! its inserts (`batch.rs` `DIST_PREFETCH_AHEAD`), timed on dev data: synthetic word streams at the dev seed (never a
//! benchmark's columns), NDV 2^10 (the table in L1d), 2^16 (L2) and 2^22 (DRAM), each distance 0 (no prefetch) .. 32 in the
//! writer's own loop (`prefetch(h1[i + ahead])` then `observe_hashed_word`). It prints a TSV (ns per value) and each NDV's
//! fastest distance; the miss it covers is the cost home's (`cost_profile::fit::prefetch_distance`). Run on a box (never the
//! Mac), optimized:
//!   cargo test --profile fast-profile -p pgrc_meta --test cost_fit_distacc -- --ignored --nocapture
use pgrc_meta::hash::meta_hash128_word;
use pgrc_meta::sketch::DistAcc;
use std::hint::black_box;
use std::time::Instant;

/// The dev seed of the cost fits (the same as `crates/gravity/program/tests/cost_fit.rs`).
const DEV_SEED: u64 = 0x5eed_0de7;
const ROWS: usize = 1 << 22;

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s >> 12;
    *s ^= *s << 25;
    *s ^= *s >> 27;
    s.wrapping_mul(0x2545_f491_4f6c_dd1d)
}

/// One stream through a fresh accumulator (its growth included, as the writer pays it), ns per value.
fn take(h: &[(u64, u64)], w: &[u64], ahead: usize) -> f64 {
    let t = Instant::now();
    let mut d = DistAcc::for_words();
    for i in 0..w.len() {
        if ahead > 0 && i + ahead < h.len() {
            d.prefetch(h[i + ahead].0);
        }
        d.observe_hashed_word(h[i].0, h[i].1, w[i], 8);
    }
    black_box(&d);
    t.elapsed().as_nanos() as f64 / w.len() as f64
}

#[test]
#[ignore = "a measurement: run on a box with --ignored (module doc)"]
fn distacc_prefetch_distance_at_the_dev_seed() {
    let mut s = DEV_SEED;
    println!("fit\tndv\tahead\tns_per_value");
    for ndv in [1u64 << 10, 1 << 16, 1 << 22] {
        let w: Vec<u64> = (0..ROWS).map(|_| (xorshift(&mut s) % ndv).wrapping_mul(0x9e37_79b9_7f4a_7c15)).collect();
        let h: Vec<(u64, u64)> = w.iter().map(|&v| meta_hash128_word(v, 8)).collect();
        let mut best = (f64::INFINITY, 0usize);
        for ahead in [0usize, 2, 4, 8, 16, 32] {
            let ns = (0..3).map(|_| take(&h, &w, ahead)).fold(f64::INFINITY, f64::min);
            println!("fit\t{ndv}\t{ahead}\t{ns:.3}");
            if ns < best.0 {
                best = (ns, ahead);
            }
        }
        println!("fit\t{ndv}\tbest\t{}\t{:.3} ns", best.1, best.0);
    }
}
