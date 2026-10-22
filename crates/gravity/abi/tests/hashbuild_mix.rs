//! `gravity_abi::hash` (lowerv4 M0): the KeyHash contract, and the HASH S0b rig (hash-contract.md §6.4, from
//! malisper/gravity-v3-hash 338463a8087) on the REAL `KeyHash`: the join build's probe loop (`par_hashjoin`'s: 16 B entries,
//! the home line prefetched 16 rows ahead, every probe walking its chain to the empty slot) at hash.rs's build load and at
//! 7/8, W0 and 16 slices, beside the lo/hi reference (§3.4) and trunk's `mix`, with the real `HashBuild` as the replica's
//! cross-check. Table quality over the degenerate key sets is `dict/tests/hash_quality.rs`.
//! `HBM_SETS=name=build:probe[@k],… HBM_OUT=out.tsv cargo test --profile fast-profile -p gravity_abi --test hashbuild_mix -- --ignored --nocapture`
use std::hint::black_box;
use std::time::Instant;

use gravity_abi::breaker::{HashBuild, BUILD_TABLE};

/// The retired record-table fold (`hash_keys` before the KeyHash port), kept here as the old arm this rig compares.
fn legacy_fold(keys: &[u64]) -> u32 {
    let mut h: u64 = 0x9E37_79B9_7F4A_7C15 ^ keys.len() as u64;
    for &k in keys {
        h = (h ^ k).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h ^= h >> 29;
    }
    ((h >> 32) as u32) ^ (h as u32)
}
use gravity_abi::hash::{self, agg_cap_bits, agg_grow_at, build_cap_bits, route_and_full, table_fits, Depth, KeyHash, AGG_GROW_LOAD, BUILD_IN_PLACE_LOAD, BUILD_LOAD, BUILD_MIN_CAP_BITS, BUILD_SPARSE_LOAD, PHI64, ROUTE_BITS, SPLIT_BITS};

fn rnd(mut x: u64) -> impl FnMut() -> u64 {
    move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    }
}

// ---- the contract

/// The one-word key whose `KeyHash::word` is `h` (abi's `word_preimage`).
fn preimage(h: u64) -> u64 {
    hash::word_preimage(h)
}

#[test]
fn one_word_is_fibonacci_and_a_bijection() {
    let mut r = rnd(7);
    let mut keys: Vec<u64> = (0..1u64 << 15).flat_map(|i| [i, i << 32, i << 48, !i, r()]).collect();
    for &k in &keys {
        assert_eq!(KeyHash::word(k), KeyHash::of(&[k]));
        assert_eq!(preimage(KeyHash::word(k).to_abi()), k, "the mix is invertible");
        if k >> 32 == 0 {
            assert_eq!(KeyHash::word(k).to_abi(), k.wrapping_mul(PHI64), "a key below 2^32 is Fibonacci-hashed");
        }
    }
    // D2076: 2^15 keys that differ only in their high bits (k << 32, k << 48) spread over a table's homes as random
    // hashes would (1 - 1/e of 2^15 homes distinct), at the top and past three splits
    for shift in [32u32, 48] {
        for at in [Depth::TOP, Depth::new(3, 3).unwrap()] {
            let homes: std::collections::BTreeSet<usize> = (0..1u64 << 15).map(|i| KeyHash::word(i << shift).home(15, at)).collect();
            assert!(homes.len() * 100 >= (1 << 15) * 60, "keys << {shift} at {at:?}: {} of 32768 homes", homes.len());
        }
    }
    keys.sort_unstable();
    keys.dedup();
    let mut hs: Vec<u64> = keys.iter().map(|&k| KeyHash::word(k).to_abi()).collect();
    hs.sort_unstable();
    hs.dedup();
    assert_eq!(hs.len(), keys.len(), "φ64 is odd: distinct one-word keys never share a hash");
    assert_ne!(KeyHash::of(&[5]), KeyHash::of(&[5, 0]), "the image length is mixed in");
    assert_ne!(KeyHash::of(&[1, 2]), KeyHash::of(&[2, 1]));
}

/// D2076: a table at any path reads the hash's top `used + cap` bits (the sliced-slot test below), so keys that differ only
/// in their high half, and keys whose plain `k * φ64` products share their top 20 bits, must fill those bits as random
/// hashes would at every width a path and home reach.
#[test]
fn one_word_spreads_the_top_bits_of_high_half_keys() {
    let n = 1u64 << 15;
    let inv = (0..6).fold(PHI64, |x, _| x.wrapping_mul(2u64.wrapping_sub(PHI64.wrapping_mul(x))));
    assert_eq!(PHI64.wrapping_mul(inv), 1);
    let kc: Vec<u64> = (0..n).map(|j| ((0x5A5A5u64 << 44) | ((j * 0x9_E377_9B97) & ((1 << 44) - 1))).wrapping_mul(inv)).collect();
    let plain_tops: std::collections::BTreeSet<u64> = kc.iter().map(|k| k.wrapping_mul(PHI64) >> 49).collect();
    assert_eq!(plain_tops.len(), 1, "the kc keys defeat a plain Fibonacci hash");
    let mut sets: Vec<(String, Vec<u64>)> = (32u32..=49).map(|s| (format!("i << {s}"), (0..n).map(|i| i << s).collect())).collect();
    sets.push(("kc".into(), kc));
    sets.push(("i << 32 | i".into(), (0..n).map(|i| (i << 32) | i).collect()));
    for (name, keys) in &sets {
        for bits in [15u32, 16, 20, 24, 27, 31, 36] {
            let tops: std::collections::BTreeSet<u64> = keys.iter().map(|&k| KeyHash::word(k).to_abi() >> (64 - bits)).collect();
            let slots = (1u64 << bits) as f64;
            let random = slots * (1.0 - (-(n as f64) / slots).exp());
            assert!(tops.len() as f64 >= 0.9 * random, "{name}: {} distinct top {bits} bits, random hashes fill {random:.0}", tops.len());
        }
    }
}

#[test]
fn route_and_home_read_disjoint_bits_at_every_depth() {
    for pbits in [0u32, 1, 4, 11, 20] {
        for depth in 0..4u32 {
            let at = Depth::new(pbits, depth).unwrap();
            let used = at.used();
            assert_eq!(used, pbits + SPLIT_BITS * depth);
            for cap_bits in [1u32, 10, 20, 32] {
                let route_only = KeyHash::from_abi(if used == 0 { 0 } else { u64::MAX << (64 - used) });
                assert_eq!(route_only.home(cap_bits, at), 0, "p{pbits} d{depth} cap{cap_bits}");
                let below = KeyHash::from_abi(if used == 0 { u64::MAX } else { u64::MAX >> used });
                assert_eq!((below.part(pbits), below.sub(pbits, depth)), (0, 0));
                assert_eq!(below.home(cap_bits, at), (1usize << cap_bits) - 1);
            }
        }
    }
}

#[test]
fn a_sliced_slot_is_the_top_bits_and_splits_chain() {
    let mut r = rnd(3);
    for _ in 0..10_000 {
        let h = KeyHash::from_abi(r());
        let w = h.to_abi();
        for (pbits, sub_bits) in [(0u32, 10u32), (4, 12), (8, 20), (11, 21)] {
            let slot = ((h.part(pbits) as usize) << sub_bits) | h.home(sub_bits, Depth::part(pbits).unwrap());
            assert_eq!(slot, (w >> (64 - pbits - sub_bits)) as usize);
        }
        let d0 = Depth::part(4).unwrap();
        let d1 = d0.deeper().unwrap();
        assert_eq!((h.sub(4, 0), h.sub(4, 1), h.sub(4, 2)), (0, h.split(d0) as u64, (h.split(d0) << 3 | h.split(d1)) as u64));
        // D446: a path past 32 bits (depth 12 after 4 partition bits: 36 bits) is the route's bits 4..40
        assert_eq!(h.sub(4, 12), (w << 4) >> 28);
        assert_eq!((h.part(0), h.home(0, d1)), (0, 0));
        assert_eq!(h.stored(), (w ^ (w >> 32)) as u32);
        assert_eq!(KeyHash::from_abi(w), h);
    }
}

#[test]
fn limits_route_prefix_and_loads() {
    assert!(Depth::new(ROUTE_BITS, 0).is_some() && Depth::new(ROUTE_BITS + 1, 0).is_none());
    // D446: splits past the first 32 bits down to the last hash bit (ROUTE_BITS = 63: `h << used` stays defined)
    assert_eq!((ROUTE_BITS, hash::HOME_MIN_BITS), (63, 1));
    assert!(Depth::new(0, 21).is_some() && Depth::new(1, 21).is_none() && Depth::new(0, 21).unwrap().deeper().is_none());
    assert!(Depth::new(3, 11).is_some() && Depth::new(2, 10).unwrap().deeper().is_some());
    let deep = Depth::new(1, 19).unwrap();
    assert!(table_fits(32, deep) && !table_fits(33, deep) && table_fits(32, Depth::new(0, 21).unwrap()));
    // a table of more slots than the bits left homes them spread out: the 64 keys whose hashes agree in their top 58 bits
    // (k = preimage(C | i)) take 64 distinct homes, 4 apart, in a table of 2^8 slots at 58 route bits
    let homes: std::collections::BTreeSet<usize> = (0..64u64).map(|i| KeyHash::word(preimage(0xabcd_ef01_2345_6780 | i)).home(8, deep)).collect();
    assert_eq!((homes.len(), homes.iter().all(|h| h % 4 == 0)), (64, true));
    assert!(Depth::new(u32::MAX, u32::MAX).is_none());
    assert!(table_fits(32, Depth::new(32, 0).unwrap()) && !table_fits(33, Depth::TOP));
    let image = [3u64, 1 << 40, 0xdead_beef, 1];
    for n in 0..=image.len() + 1 {
        assert_eq!(route_and_full(&image, n), (KeyHash::of(&image[..n.min(image.len())]), KeyHash::of(&image)), "route over {n} words");
    }
    assert_eq!((BUILD_LOAD.num, BUILD_LOAD.den, BUILD_SPARSE_LOAD.num, BUILD_SPARSE_LOAD.den, hash::build_sparse_rows()), (1, 2, 1, 8, 2048));
    assert_eq!((AGG_GROW_LOAD.num, AGG_GROW_LOAD.den), (3, 4));
    assert_eq!((build_cap_bits(0), build_cap_bits(2048), build_cap_bits(2049), build_cap_bits(1 << 20), build_cap_bits((1 << 20) + 1)), (BUILD_MIN_CAP_BITS, 14, 13, 21, 22));
    for rows in (0..200_000u64).step_by(997).chain([2047, 2048, 2049, 1 << 31, u64::MAX / 4]) {
        let (bits, l) = (build_cap_bits(rows), if rows <= hash::build_sparse_rows() { BUILD_SPARSE_LOAD } else { BUILD_LOAD });
        let cap = 1u128 << bits;
        assert!(rows as u128 * l.den as u128 <= cap * l.num as u128, "{rows} rows in {cap} slots");
        assert!(bits == BUILD_MIN_CAP_BITS || rows as u128 * l.den as u128 > (cap / 2) * l.num as u128, "{rows}: the least power of two");
    }
    // the in-place / merged table: the least power of two holding `len` at its load, from the same named loads
    assert_eq!((BUILD_IN_PLACE_LOAD.num, BUILD_IN_PLACE_LOAD.den), (7, 8));
    assert_eq!([0, 12, 2048, 2049, 3584, 3585].map(HashBuild::table_cap_for), [1024, 1024, 16384, 4096, 4096, 8192]);
    for len in (0..400_000u64).step_by(991).chain([2047, 2048, 2049, 7 << 20]) {
        let (cap, l) = (HashBuild::table_cap_for(len) as u64, if len <= hash::build_sparse_rows() { BUILD_SPARSE_LOAD } else { BUILD_IN_PLACE_LOAD });
        assert!(len * l.den <= cap * l.num, "{len} entries in {cap} slots");
        assert!(cap == 1 << BUILD_MIN_CAP_BITS || len * l.den > (cap / 2) * l.num, "{len}: the least power of two");
    }
    assert_eq!((agg_grow_at(16), agg_grow_at(1 << 20)), (12, 3 << 18));
    assert_eq!((agg_cap_bits(0), agg_cap_bits(12), agg_cap_bits(13), agg_cap_bits(3 << 18), agg_cap_bits((3 << 18) + 1)), (4, 4, 5, 20, 21));
    assert_eq!(hash::HOME_BITS, 32);
}

// ---- S0b: the probe loop

const KEY: u8 = 0;
const LOHI: u8 = 1;
const OLD: u8 = 2;
const NAMES: [&str; 3] = ["keyhash", "lohi", "old"];
const AHEAD: usize = 16;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Entry {
    key: u64,
    /// build row + 1; 0 = empty
    payload: u64,
}

struct Table {
    e: Vec<Entry>,
    bits: u32,
    pbits: u32,
}

/// The slot a key starts at: its slice base plus its home inside the slice.
#[inline(always)]
fn slot<const M: u8>(k: u64, bits: u32, pbits: u32) -> usize {
    let sub_bits = bits - pbits;
    let (part, home) = match M {
        KEY => {
            let h = KeyHash::word(k);
            (h.part(pbits) as usize, h.home(sub_bits, Depth::part(pbits).unwrap()))
        }
        LOHI => {
            let mut h = ((PHI64 ^ 1) ^ k).wrapping_mul(PHI64);
            h ^= h >> 29;
            h ^= h >> 32;
            h = h.wrapping_mul(0xD6E8_FEB8_6659_FD93);
            h ^= h >> 32;
            (if pbits == 0 { 0 } else { ((h >> 32) as u32 >> (32 - pbits)) as usize }, h as u32 as usize & ((1 << sub_bits) - 1))
        }
        _ => {
            let m = if k as i64 == k as i32 as i64 { (k as u32).wrapping_mul(0x9E37_79B9) } else { legacy_fold(&[k]).wrapping_mul(0x9E37_79B9) };
            (if pbits == 0 { 0 } else { m.reverse_bits() as usize & ((1 << pbits) - 1) }, m as usize & ((1 << sub_bits) - 1))
        }
    };
    (part << sub_bits) | home
}

#[inline(always)]
fn prefetch(p: *const u8) {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("prfm pldl1keep, [{0}]", in(reg) p, options(nostack, preserves_flags, readonly));
    }
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::x86_64::_mm_prefetch(p as *const i8, core::arch::x86_64::_MM_HINT_T0);
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    let _ = p;
}

/// Inserts `keys` (linear probing inside the slice); `None` when a slice fills.
fn build<const M: u8>(keys: &[u64], bits: u32, pbits: u32) -> Option<(Table, f64)> {
    let mut e = vec![Entry::default(); 1 << bits];
    let sub_mask = (1usize << (bits - pbits)) - 1;
    let t0 = Instant::now();
    for (r, &k) in keys.iter().enumerate() {
        let s = slot::<M>(k, bits, pbits);
        let (base, mut i, mut n) = (s & !sub_mask, s & sub_mask, 0);
        while e[base | i].payload != 0 {
            i = (i + 1) & sub_mask;
            n += 1;
            if n > sub_mask {
                return None;
            }
        }
        e[base | i] = Entry { key: k, payload: r as u64 + 1 };
    }
    Some((Table { e, bits, pbits }, t0.elapsed().as_nanos() as f64 / keys.len().max(1) as f64))
}

/// Every probe walks its chain to the empty slot; returns (matches, slots walked, payload sum).
#[inline(never)]
fn probe<const M: u8, const PF: bool>(t: &Table, keys: &[u64]) -> (u64, u64, u64) {
    let (bits, pbits) = (t.bits, t.pbits);
    let sub_mask = (1usize << (bits - pbits)) - 1;
    let e = t.e.as_ptr();
    let (mut hits, mut walked, mut sum) = (0u64, 0u64, 0u64);
    for j in 0..keys.len() {
        if PF && j + AHEAD < keys.len() {
            prefetch(unsafe { e.add(slot::<M>(keys[j + AHEAD], bits, pbits)) } as *const u8);
        }
        let k = keys[j];
        let s = slot::<M>(k, bits, pbits);
        let (base, mut i) = (s & !sub_mask, s & sub_mask);
        loop {
            let x = unsafe { *e.add(base | i) };
            walked += 1;
            if x.payload == 0 {
                break;
            }
            if x.key == k {
                hits += 1;
                sum = sum.wrapping_add(x.payload);
            }
            i = (i + 1) & sub_mask;
        }
    }
    (hits, walked, sum)
}

/// The real `HashBuild` (old mix, W0, i32 keys) with the same prefetch: the replica's cross-check.
fn real(build: &[u64], probes: &[u64], bits: u32) -> Option<(f64, f64, u64)> {
    if build.iter().chain(probes).any(|&k| k as i64 != k as i32 as i64) {
        return None;
    }
    let cap = 1u32 << bits;
    let mut buf = vec![0u64; (HashBuild::bytes(cap) as usize).div_ceil(8)];
    let t = unsafe { HashBuild::at(buf.as_mut_ptr() as *mut u8, cap, BUILD_TABLE) };
    let t0 = Instant::now();
    for (r, &k) in build.iter().enumerate() {
        assert!(t.insert(k as i32, r as i64 + 1));
    }
    let ns_b = t0.elapsed().as_nanos() as f64 / build.len() as f64;
    let (t0, mut hits) = (Instant::now(), 0u64);
    for j in 0..probes.len() {
        if j + AHEAD < probes.len() {
            prefetch(t.entry(t.probe(probes[j + AHEAD] as i32).position()) as *const _ as *const u8);
        }
        for m in t.probe(probes[j] as i32) {
            hits += 1;
            black_box(m.payload);
        }
    }
    Some((ns_b, t0.elapsed().as_nanos() as f64 / probes.len() as f64, hits))
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn run_mix<const M: u8>(dist: &[u64], probes: &[u64], shuffled: &[u64], bits: u32, pbits: u32, reps: usize) -> Option<String> {
    let mut nb = Vec::new();
    let mut t = None;
    for _ in 0..reps {
        let (tb, ns) = build::<M>(dist, bits, pbits)?;
        nb.push(ns);
        t = Some(tb);
    }
    let t = t.unwrap();
    let time = |f: &dyn Fn() -> (u64, u64, u64), n: usize| {
        let (mut v, mut out) = (Vec::new(), (0, 0, 0));
        for _ in 0..reps {
            let t0 = Instant::now();
            out = black_box(f());
            v.push(t0.elapsed().as_nanos() as f64 / n.max(1) as f64);
        }
        (median(v), out)
    };
    let (pf, (hits, walked, _)) = time(&|| probe::<M, true>(&t, probes), probes.len());
    let (nopf, _) = time(&|| probe::<M, false>(&t, probes), probes.len());
    let (pf_sh, _) = time(&|| probe::<M, true>(&t, shuffled), shuffled.len());
    let (nopf_sh, _) = time(&|| probe::<M, false>(&t, shuffled), shuffled.len());
    Some(format!("{}\t{:.1}\t{pf:.1}\t{nopf:.1}\t{pf_sh:.1}\t{nopf_sh:.1}\t{:.3}\t{hits}", NAMES[M as usize], median(nb), walked as f64 / probes.len().max(1) as f64))
}

/// One key per line: an integer, two tab-separated integers (`hi << 32 | lo`), or anything else as its FNV-1a 64 digest.
fn load(path: &str) -> Vec<u64> {
    let s = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    s.lines()
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            match (f.first().and_then(|a| a.parse::<i64>().ok()), f.get(1).and_then(|b| b.parse::<i64>().ok())) {
                (Some(a), None) if f.len() == 1 => a as u64,
                (Some(a), Some(b)) => ((a as u64) << 32) | (b as u32 as u64),
                _ => l.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3)),
            }
        })
        .collect()
}

#[test]
#[ignore = "a measurement: HBM_SETS=name=build:probe[@k],… HBM_OUT=out.tsv cargo test --profile fast-profile -p gravity_abi --test hashbuild_mix -- --ignored --nocapture"]
fn hashbuild_probe_ns_per_mix() {
    let reps: usize = std::env::var("HBM_REPS").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
    let spec = std::env::var("HBM_SETS").expect("HBM_SETS");
    let mut out = vec!["set\tload\tpbits\tbuild_rows\tcap\tprobes\tmix\tns_build_row\tns_probe_pf\tns_probe_nopf\tns_probe_pf_shuffled\tns_probe_nopf_shuffled\twalk_per_probe\tmatches".to_string()];
    for item in spec.split(',') {
        let (name, rest) = item.split_once('=').unwrap();
        let (paths, keep) = rest.split_once('@').map_or((rest, 1), |(p, k)| (p, k.parse::<u64>().unwrap()));
        let (bp, pp) = paths.split_once(':').unwrap();
        let mut bkeys = load(bp);
        bkeys.retain(|&k| keep <= 1 || k.wrapping_mul(0xBF58_476D_1CE4_E5B9).rotate_left(31) % keep == 0);
        let mut seen = std::collections::HashSet::with_capacity(bkeys.len());
        bkeys.retain(|k| seen.insert(*k));
        let probes = load(pp);
        let mut shuffled = probes.clone();
        let mut r = rnd(0x2545_F491_4F6C_DD1D);
        for i in (1..shuffled.len()).rev() {
            shuffled.swap(i, (r() % (i as u64 + 1)) as usize);
        }
        let bits = (usize::BITS - 1 - bkeys.len().max(1).leading_zeros()).max(10);
        for (l0, l1) in [(BUILD_LOAD.num, BUILD_LOAD.den), (7, 8)] {
            let m = (((1u64 << bits) * l0 / l1) as usize).min(bkeys.len());
            for pbits in [0u32, 4] {
                let head = format!("{name}\t{l0}/{l1}\t{pbits}\t{m}\t{}\t{}", 1u64 << bits, probes.len());
                for row in [run_mix::<KEY>(&bkeys[..m], &probes, &shuffled, bits, pbits, reps), run_mix::<LOHI>(&bkeys[..m], &probes, &shuffled, bits, pbits, reps), run_mix::<OLD>(&bkeys[..m], &probes, &shuffled, bits, pbits, reps)] {
                    out.push(format!("{head}\t{}", row.unwrap_or_else(|| "?\tslice full".into())));
                    println!("{}", out.last().unwrap());
                }
                if let Some((nb, np, hits)) = (pbits == 0).then(|| real(&bkeys[..m], &probes, bits)).flatten() {
                    out.push(format!("{head}\treal-HashBuild(old)\t{nb:.1}\t{np:.1}\t-\t-\t-\t-\t{hits}"));
                    println!("{}", out.last().unwrap());
                }
            }
        }
    }
    std::fs::write(std::env::var("HBM_OUT").unwrap_or_else(|_| "hashbuild_mix.tsv".into()), out.join("\n") + "\n").unwrap();
}

#[test]
fn every_mix_finds_the_same_matches() {
    let bk: Vec<u64> = (0..3000u64).map(|i| (i / 8) * 32 + i % 8 + 1).collect();
    let probes: Vec<u64> = (0..20_000u64).map(|i| i % 13_000).collect();
    let set: std::collections::HashSet<u64> = bk.iter().copied().collect();
    let want = probes.iter().filter(|p| set.contains(p)).count() as u64;
    for pbits in [0, 4] {
        let (t, _) = build::<KEY>(&bk, 12, pbits).unwrap();
        assert_eq!((probe::<KEY, true>(&t, &probes).0, probe::<KEY, false>(&t, &probes).0), (want, want));
        let (t, _) = build::<LOHI>(&bk, 12, pbits).unwrap();
        assert_eq!(probe::<LOHI, true>(&t, &probes).0, want);
        let (t, _) = build::<OLD>(&bk, 12, pbits).unwrap();
        assert_eq!(probe::<OLD, true>(&t, &probes).0, want);
    }
    assert_eq!(real(&bk, &probes, 12).unwrap().2, want);
}
