//! The home-hash measurement (pass 5 item 4): for a key file (one integer per line;
//! `HOME_HASH_KEYS=<path>`, else synthetic sets), insert every distinct key into an open-addressed
//! table at 75 % load and replay the stream as lookups, per candidate home hash — the join build's
//! multiplicative low bits (`key × φ & mask`), Fibonacci top bits, the record table's KeyHash home,
//! and an avalanching finaliser — reporting probes per insert / lookup, the longest probe, ns per
//! insert / lookup. `cargo test -p gravity_abi --test home_hash -- --ignored --nocapture`.
use std::time::Instant;

use gravity_abi::hash::{Depth, KeyHash};

const PHI64: u64 = 0x9E37_79B9_7F4A_7C15;
const PHI32: u32 = 0x9E37_79B9;

fn fmix64(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    h = h.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    h ^ (h >> 33)
}

fn fmix32(mut h: u32) -> u32 {
    h ^= h >> 16;
    h = h.wrapping_mul(0x85EB_CA6B);
    h ^= h >> 13;
    h = h.wrapping_mul(0xC2B2_AE35);
    h ^ (h >> 16)
}

type HashFn = fn(u64, u32) -> usize;

fn mul_low64(k: u64, bits: u32) -> usize {
    (k.wrapping_mul(PHI64) as usize) & ((1usize << bits) - 1)
}

fn mul_low32(k: u64, bits: u32) -> usize {
    ((k as u32).wrapping_mul(PHI32) as usize) & ((1usize << bits) - 1)
}

fn fib_top64(k: u64, bits: u32) -> usize {
    (k.wrapping_mul(PHI64) >> (64 - bits)) as usize
}

fn fib_top32(k: u64, bits: u32) -> usize {
    ((k as u32).wrapping_mul(PHI32) >> (32 - bits)) as usize
}

fn record_hash(k: u64, bits: u32) -> usize {
    KeyHash::of(&[k]).home(bits, Depth::TOP)
}

fn fmix64_low(k: u64, bits: u32) -> usize {
    fmix64(k) as usize & ((1usize << bits) - 1)
}

fn fmix32_low(k: u64, bits: u32) -> usize {
    fmix32(k as u32) as usize & ((1usize << bits) - 1)
}

fn identity_low(k: u64, bits: u32) -> usize {
    k as usize & ((1usize << bits) - 1)
}

struct Outcome {
    probes_insert: f64,
    probes_lookup: f64,
    max_probe: usize,
    ns_insert: f64,
    ns_lookup: f64,
}

/// Linear probing over `(key + 1)` cells (0 = empty): the distinct keys inserted once, then every
/// key of the stream looked up (a hit each).
fn measure(f: HashFn, distinct: &[u64], stream: &[u64]) -> Outcome {
    let bits = (distinct.len() as f64 / 0.75).log2().ceil() as u32;
    let mask = (1usize << bits) - 1;
    let mut table = vec![0u64; 1 << bits];
    let mut probes = 0usize;
    let mut max_probe = 0usize;
    let t0 = Instant::now();
    for &k in distinct {
        let mut i = f(k, bits);
        let mut p = 1;
        while table[i] != 0 {
            i = (i + 1) & mask;
            p += 1;
        }
        table[i] = k + 1;
        probes += p;
        max_probe = max_probe.max(p);
    }
    let ns_insert = t0.elapsed().as_nanos() as f64 / distinct.len() as f64;
    let probes_insert = probes as f64 / distinct.len() as f64;
    let mut probes = 0usize;
    let mut found = 0usize;
    let t0 = Instant::now();
    for &k in stream {
        let mut i = f(k, bits);
        let mut p = 1;
        while table[i] != k + 1 {
            debug_assert!(table[i] != 0);
            i = (i + 1) & mask;
            p += 1;
        }
        found += 1;
        probes += p;
    }
    let ns_lookup = t0.elapsed().as_nanos() as f64 / stream.len() as f64;
    assert_eq!(found, stream.len());
    Outcome { probes_insert, probes_lookup: probes as f64 / stream.len() as f64, max_probe, ns_insert, ns_lookup }
}

fn distinct_in_order(stream: &[u64]) -> Vec<u64> {
    let mut seen = std::collections::HashSet::with_capacity(stream.len());
    stream.iter().copied().filter(|k| seen.insert(*k)).collect()
}

fn report(name: &str, stream: &[u64], width: u32) {
    let distinct = distinct_in_order(stream);
    println!("\n{name}: {} rows, {} distinct, table 2^{} ({} % load)", stream.len(), distinct.len(), (distinct.len() as f64 / 0.75).log2().ceil() as u32, (100.0 * distinct.len() as f64 / (1u64 << (distinct.len() as f64 / 0.75).log2().ceil() as u32) as f64).round());
    println!("{:<22} {:>10} {:>10} {:>9} {:>10} {:>10}", "home hash", "probes/ins", "probes/get", "max probe", "ns/insert", "ns/lookup");
    let cands: Vec<(&str, HashFn)> = if width == 32 {
        vec![("identity low (ctl)", identity_low), ("mul low32 (join now)", mul_low32), ("fib top32", fib_top32), ("fmix32", fmix32_low), ("KeyHash (records)", record_hash)]
    } else {
        vec![("identity low (ctl)", identity_low), ("mul low64", mul_low64), ("fib top64", fib_top64), ("fmix64", fmix64_low), ("KeyHash (records)", record_hash)]
    };
    for (label, f) in cands {
        // the identity control can degenerate on clustered keys: cap its work
        if label.starts_with("identity") && distinct.len() > 4_000_000 {
            continue;
        }
        let o = measure(f, &distinct, stream);
        println!("{label:<22} {:>10.3} {:>10.3} {:>9} {:>10.1} {:>10.1}", o.probes_insert, o.probes_lookup, o.max_probe, o.ns_insert, o.ns_lookup);
    }
}

fn load(path: &str) -> Vec<u64> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    text.lines().filter_map(|l| l.trim().parse::<i64>().ok()).map(|v| v as u64).collect()
}

#[test]
#[ignore = "a measurement: HOME_HASH_KEYS=<file> [HOME_HASH_WIDTH=32] cargo test -p gravity_abi --test home_hash -- --ignored --nocapture"]
fn home_hash_probe_lengths_and_ns_per_key() {
    let width: u32 = std::env::var("HOME_HASH_WIDTH").ok().and_then(|v| v.parse().ok()).unwrap_or(64);
    if let Ok(path) = std::env::var("HOME_HASH_KEYS") {
        for p in path.split(',') {
            let keys = load(p);
            report(p, &keys, width);
        }
        return;
    }
    let n = 2_000_000u64;
    report("sequential 0..n", &(0..n).collect::<Vec<_>>(), width);
    report("stride 8", &(0..n).map(|i| i * 8).collect::<Vec<_>>(), width);
    report("stride 4096", &(0..n).map(|i| i * 4096).collect::<Vec<_>>(), width);
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let random: Vec<u64> = (0..n).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x }).collect();
    report("xorshift random", &random, width);
}
