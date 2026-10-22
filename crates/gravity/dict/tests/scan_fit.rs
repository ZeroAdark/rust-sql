//! The substring scan's unit costs (`cost_profile::scan::Scan`, the re-check rule of `Matcher::build_bits_over`), fitted on
//! dev data: text of random lowercase words at the dev seed (never a benchmark's data) with near-miss windows of an
//! 8-byte dev needle planted at swept densities. A rejected candidate's cost is the least-squares slope of scan time over
//! the rejected count (`RarePair::find_rechecking` counts them); a counted byte's cost is `count_bytes` time per byte.
//! Run on a box (never the Mac), optimized:
//!   cargo test --profile fast-profile -p gravity_dict --test scan_fit -- --ignored --nocapture
use gravity_dict::substr::{count_bytes, RarePair, Step};
use std::time::Instant;

const DEV_SEED: u64 = 0x5eed_0de7;
const HAY_MIB: usize = 32;
const NEEDLE: &[u8] = b"vqkzjwxp";
const TAKES: usize = 7;

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s >> 12;
    *s ^= *s << 25;
    *s ^= *s >> 27;
    s.wrapping_mul(0x2545_f491_4f6c_dd1d)
}

/// Random lowercase words with a near miss of `NEEDLE` (its last byte changed) every `every` bytes (0: none).
fn hay(every: usize) -> Vec<u8> {
    let mut s = DEV_SEED;
    let mut h = Vec::with_capacity(HAY_MIB << 20);
    while h.len() < HAY_MIB << 20 {
        let w = 2 + (xorshift(&mut s) % 9) as usize;
        (0..w).for_each(|_| h.push(b'a' + (xorshift(&mut s) % 26) as u8));
        h.push(b' ');
    }
    h.truncate(HAY_MIB << 20);
    if every > 0 {
        let mut at = every;
        while at + NEEDLE.len() < h.len() {
            h[at..at + NEEDLE.len()].copy_from_slice(NEEDLE);
            h[at + NEEDLE.len() - 1] = b'#';
            at += every;
        }
    }
    h
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

#[test]
#[ignore = "a measurement: run on a box with --ignored (module doc)"]
fn scan_unit_costs_at_the_dev_seed() {
    let p = RarePair::new(NEEDLE).unwrap();
    let mut pts = Vec::new();
    for every in [0usize, 4096, 1024, 256, 64, 32] {
        let h = hay(every);
        let mut ns = Vec::new();
        let mut rej = 0u64;
        for _ in 0..TAKES {
            let mut r = 0u64;
            let t = Instant::now();
            let s = p.find_rechecking(NEEDLE, std::hint::black_box(&h), 0, &mut r, |_, _| false);
            ns.push(t.elapsed().as_nanos() as f64);
            assert_eq!(s, Step::End, "the dev text never holds the needle");
            rej = r;
        }
        let m = median(ns);
        println!("scan_fit every {every:>5} rejected {rej:>9} ns {m:>12.0}");
        pts.push((rej as f64, m));
    }
    let n = pts.len() as f64;
    let (mx, my) = (pts.iter().map(|p| p.0).sum::<f64>() / n, pts.iter().map(|p| p.1).sum::<f64>() / n);
    let (sxx, sxy) = pts.iter().fold((0.0, 0.0), |(a, b), &(x, y)| (a + (x - mx) * (x - mx), b + (x - mx) * (y - my)));
    let reject_ns = sxy / sxx;
    let h = hay(0);
    let mut ns = Vec::new();
    for _ in 0..TAKES {
        let mut c = [0u64; 256];
        let t = Instant::now();
        count_bytes(std::hint::black_box(&h), &mut c);
        ns.push(t.elapsed().as_nanos() as f64);
        std::hint::black_box(c);
    }
    let count_ns_per_byte = median(ns) / h.len() as f64;
    println!("scan_fit reject_ns {reject_ns:.3} count_ns_per_byte {count_ns_per_byte:.4} (intercept {:.0} ns over {} MiB)", my - reject_ns * mx, HAY_MIB);
    assert!(reject_ns > 0.0 && count_ns_per_byte > 0.0);
}
