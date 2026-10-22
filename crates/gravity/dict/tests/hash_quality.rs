//! The hash-quality test (lowerv4 M0; the HASH S0 rig of malisper/gravity-v3-hash 338463a8087, hash-contract.md §6, ported
//! onto the REAL `gravity_abi::hash::KeyHash`, whatever mix and loads hash.rs holds). It is the Tuning reopen's pass: a
//! change to hash.rs's mix or load constants lands only with this file green (and `abi/tests/hashbuild_mix.rs`), plus a
//! before / after on the affected bar cells.
//!
//! The gate: open-addressed tables at hash.rs's own loads, W0 and 16 slices, over the degenerate key sets (dense, sparse,
//! filtered, strided, the TOP-BITS-ONLY shape, integral floats, timestamps, negatives, pairs, Fp128 pairs, flags words),
//! tables refilled from one route stream at depth 0..2, T1, and avalanche per half. Two bars: no set collapses (walks
//! within 6 × Knuth, χ² < 50), and random-like sets are uniform (1.5 × Knuth, χ² < 3). Fib's structured cases (a range
//! build probed past its end, strides such as 32 and 48) sit between the two and show in the report beside the lo/hi
//! reference (§3.4) and trunk's old mix.
//! `HASHQ_SETS=name=build[:probe][@k],… HASHQ_OUT=out.tsv cargo test --profile fast-profile -p gravity_dict --test hash_quality -- --ignored --nocapture`
use std::collections::HashSet;
use std::time::Instant;

use gravity_abi::hash::{self, Depth, KeyHash, Load, AGG_GROW_LOAD, BUILD_LOAD, BUILD_SPARSE_LOAD, PHI64};
use gravity_dict::fp::siphash128;

const T1_BYTES: u64 = 512 << 10;
const FP_K: (u64, u64) = (0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
type Key = [u64; 4];

/// Trunk's record fold (`hash_keys` before the KeyHash port), kept here as the old arm this rig compares.
fn hash_keys(keys: &[u64]) -> u32 {
    let mut h: u64 = 0x9E37_79B9_7F4A_7C15 ^ keys.len() as u64;
    for &k in keys {
        h = (h ^ k).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h ^= h >> 29;
    }
    ((h >> 32) as u32) ^ (h as u32)
}

/// The real `KeyHash`, the lo/hi finalizer (§3.4, S0's "new"), trunk's join `mix` / record `hash_keys`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hf { Key, LoHi, Old }
const HFS: [Hf; 3] = [Hf::Key, Hf::LoHi, Hf::Old];

fn lohi(w: &[u64]) -> u64 {
    let mut h = PHI64 ^ w.len() as u64;
    for &k in w {
        h = (h ^ k).wrapping_mul(PHI64);
        h ^= h >> 29;
    }
    h ^= h >> 32;
    h = h.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    h ^ (h >> 32)
}

/// `(home in a slice of 2^bits, slice of 2^pbits)`; the rows already consumed `pre` route bits upstream (a refill, unsliced).
fn place(hf: Hf, w: &[u64], bits: u32, pbits: u32, pre: u32) -> (usize, usize) {
    let mask = (1usize << bits) - 1;
    match hf {
        Hf::Key => {
            let h = KeyHash::of(w);
            (h.home(bits, Depth::new(pre + pbits, 0).expect("route bits")), if pre == 0 { h.part(pbits) as usize } else { 0 })
        }
        Hf::LoHi => {
            let h = lohi(w);
            (h as u32 as usize & mask, if pbits == 0 { 0 } else { ((h >> 32) as u32 >> (32 - pbits)) as usize })
        }
        Hf::Old => {
            let m = if w.len() == 1 && w[0] as i64 == w[0] as i32 as i64 { (w[0] as u32).wrapping_mul(0x9E37_79B9) } else { hash_keys(w) };
            (m as usize & mask, m.reverse_bits() as usize & ((1usize << pbits) - 1))
        }
    }
}

struct Set { name: String, nw: usize, build: Vec<Key>, probe: Vec<Key> }

fn xorshift(mut x: u64) -> impl FnMut() -> u64 {
    move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    }
}

fn splitmix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn distinct(keys: &[Key]) -> Vec<Key> {
    let mut seen = HashSet::with_capacity(keys.len());
    keys.iter().copied().filter(|k| seen.insert(*k)).collect()
}

/// Linear probing's expected walks at load α (Knuth): unsuccessful ½(1 + 1/(1−α)²), successful ½(1 + 1/(1−α)).
fn knuth_miss(a: f64) -> f64 { 0.5 * (1.0 + 1.0 / ((1.0 - a) * (1.0 - a))) }
fn knuth_hit(a: f64) -> f64 { 0.5 * (1.0 + 1.0 / (1.0 - a)) }
fn frac(l: Load) -> (u32, u32) { (l.num as u32, l.den as u32) }

#[derive(Default, Debug)]
struct Row { what: String, load: f64, hit: f64, miss: f64, chi_lo: f64, chi_hi: f64, overflow: usize, degen: bool, inserted: usize }

impl Row {
    /// Walks within `slack` × Knuth at the actual load, χ² under `chi` in the low and the top slot bits, nothing lost.
    fn check(&self, slack: f64, chi: f64) -> Result<(), String> {
        let bad = self.degen || self.overflow > 0 || self.hit > slack * knuth_hit(self.load) || self.miss > slack * knuth_miss(self.load) || self.chi_lo >= chi || self.chi_hi >= chi;
        if bad { Err(format!("{} (bar {slack}× Knuth, χ² < {chi}): {self:?}", self.what)) } else { Ok(()) }
    }
}

/// χ²/df of slots over 1024 groups, by the low 10 bits and by the top 10 bits (1.0 = uniform).
fn chi2(slots: &[usize], bits: u32) -> (f64, f64) {
    let g = 1024usize.min(1 << bits);
    let shift = bits.saturating_sub(g.trailing_zeros());
    let (mut lo, mut hi) = (vec![0u64; g], vec![0u64; g]);
    for &h in slots {
        lo[h & (g - 1)] += 1;
        hi[(h >> shift) & (g - 1)] += 1;
    }
    let e = slots.len() as f64 / g as f64;
    let x = |v: &[u64]| v.iter().map(|&c| (c as f64 - e).powi(2) / e).sum::<f64>() / (g - 1).max(1) as f64;
    (x(&lo), x(&hi))
}

/// One linear-probing table at load `l` in 2^pbits slices, its rows past `pre` upstream route bits. Inserts the first keys
/// of `dist`, then looks up `stream` (hits: the inserted keys; misses: the rest).
fn run(set: &Set, dist: &[Key], hf: Hf, l: (u32, u32), pbits: u32, pre: u32, stream: &[Key]) -> Row {
    let cap = (1usize << (usize::BITS - 1 - dist.len().max(1).leading_zeros())).max(1024);
    let bits = cap.trailing_zeros();
    let sub = cap >> pbits;
    let m = ((cap as u64 * l.0 as u64 / l.1 as u64) as usize).min(dist.len());
    let (mut slots, mut fill) = (vec![0u32; cap], vec![0usize; 1 << pbits]);
    let (mut homes, mut kept, mut walked, mut overflow, mut degen) = (Vec::with_capacity(m), HashSet::new(), 0usize, 0usize, false);
    for (i, k) in dist[..m].iter().enumerate() {
        if i > 1000 && walked > 256 * i {
            degen = true;
            break;
        }
        let (h, p) = place(hf, &k[..set.nw], bits - pbits, pbits, pre);
        if fill[p] == sub {
            overflow += 1;
            continue;
        }
        fill[p] += 1;
        let mut j = h;
        walked += 1;
        while slots[p * sub + j] != 0 {
            j = (j + 1) & (sub - 1);
            walked += 1;
        }
        slots[p * sub + j] = i as u32 + 1;
        homes.push(p * sub + h);
        kept.insert(*k);
    }
    let (mut hw, mut hn, mut mw, mut mn) = (0usize, 0usize, 0usize, 0usize);
    for k in stream {
        let key = &k[..set.nw];
        let (h, p) = place(hf, key, bits - pbits, pbits, pre);
        let (mut j, mut walk) = (h, 1);
        while slots[p * sub + j] != 0 && dist[slots[p * sub + j] as usize - 1][..set.nw] != *key && walk <= sub {
            j = (j + 1) & (sub - 1);
            walk += 1;
        }
        if kept.contains(k) { (hw, hn) = (hw + walk, hn + 1) } else { (mw, mn) = (mw + walk, mn + 1) }
        if hw + mw > 256 * stream.len().max(1024) {
            degen = true;
            break;
        }
    }
    let (chi_lo, chi_hi) = chi2(&homes, bits);
    let what = format!("{} {hf:?} L={}/{} p{pbits} pre{pre}", set.name, l.0, l.1);
    Row { what, load: kept.len() as f64 / cap as f64, hit: hw as f64 / hn.max(1) as f64, miss: mw as f64 / mn.max(1) as f64, chi_lo, chi_hi, overflow, degen, inserted: kept.len() }
}

/// A table refilled from one route stream at `depth` (partition 0 of 2^4, sub-split path 0): KeyHash's home reads below the
/// stream's consumed route bits (the depth rule); the other mixes take the home from bits the route never reads.
fn refill(set: &Set, dist: &[Key], hf: Hf, l: (u32, u32), depth: u32) -> Row {
    let stream0 = |w: &[u64]| if hf == Hf::Key { KeyHash::of(w).part(4) == 0 && KeyHash::of(w).sub(4, depth) == 0 } else { place(hf, w, 0, 4 + 3 * depth, 0).1 == 0 };
    let part: Vec<Key> = dist.iter().copied().filter(|k| stream0(&k[..set.nw])).collect();
    let pre = if hf == Hf::Key { 4 + 3 * depth } else { 0 };
    let mut r = run(set, &part, hf, l, 0, pre, &part);
    r.what = format!("{} {hf:?} refill d{depth} L={}/{}", set.name, l.0, l.1);
    r
}

/// T1: a 512 KiB table probed at the home and the next slot, the home's occupant evicted on a double miss. Evictions/row.
fn t1(set: &Set, hf: Hf) -> f64 {
    let cap = 1usize << (usize::BITS - 1 - ((T1_BYTES / (8 * set.nw as u64 + 16)) as usize).leading_zeros());
    let bits = cap.trailing_zeros();
    let mut slots: Vec<Option<Key>> = vec![None; cap];
    let mut evict = 0usize;
    for k in &set.build {
        let (h, _) = place(hf, &k[..set.nw], bits, 0, 0);
        let h2 = (h + 1) & (cap - 1);
        match (slots[h], slots[h2]) {
            (Some(a), _) if a == *k => {}
            (_, Some(b)) if b == *k => {}
            (None, _) => slots[h] = Some(*k),
            (_, None) => slots[h2] = Some(*k),
            _ => (evict, slots[h]) = (evict + 1, Some(*k)),
        }
    }
    evict as f64 / set.build.len().max(1) as f64
}

fn synthetic(n: u64) -> Vec<Set> {
    let one = |name: &str, b: Vec<u64>, p: Vec<u64>| Set { name: name.into(), nw: 1, build: b.into_iter().map(|k| [k, 0, 0, 0]).collect(), probe: p.into_iter().map(|k| [k, 0, 0, 0]).collect() };
    let two = |name: &str, b: Vec<[u64; 2]>| Set { name: name.into(), nw: 2, build: b.into_iter().map(|[a, c]| [a, c, 0, 0]).collect(), probe: Vec::new() };
    let mut rnd = xorshift(0x2545_F491_4F6C_DD1D);
    let okey = |i: u64| (i / 8) * 32 + i % 8 + 1; // dbgen's sparse order keys; lineitem carries 1..7 lines per order
    let orders: Vec<u64> = (0..n).map(okey).collect();
    let lineitem: Vec<u64> = (0..n).flat_map(|i| std::iter::repeat_n(okey(i), 1 + (splitmix(i) % 7) as usize)).collect();
    let top = |s: u32| (0..n.min(1 << (64 - s))).map(|i| i << s).collect::<Vec<u64>>();
    vec![
        one("seq", (1..=n).collect(), Vec::new()),
        one("orderkey-f7", orders.iter().copied().filter(|&k| splitmix(k) % 7 == 0).collect(), lineitem.clone()),
        one("orderkey", orders, lineitem),
        one("stride32", (0..n).map(|i| i * 32).collect(), Vec::new()),
        one("stride4096", (0..n).map(|i| i * 4096).collect(), Vec::new()),
        one("ts-days", (0..n).map(|i| i * 86_400_000_000).collect(), Vec::new()),
        one("highbits", top(32), Vec::new()),
        one("topbits40", top(40), Vec::new()),
        one("topbits48", top(48), Vec::new()),
        one("topbits56", top(56), Vec::new()),
        one("float-int", (1..=n).map(|i| (i as f64).to_bits()).collect(), Vec::new()),
        one("neg", (1..=n).map(|i| (-(i as i64)) as u64).collect(), Vec::new()),
        one("random", (0..n).map(|_| rnd()).collect(), Vec::new()),
        two("pair", (0..n).map(|i| [i / 256, i % 256]).collect()),
        two("fp128", (0..n).map(|i| siphash128(FP_K.0, FP_K.1, format!("phrase {i}").as_bytes())).collect()),
        two("flags", (0..n).map(|i| if i % 10 == 0 { [0, 1] } else { [i, 0] }).collect()),
    ]
}

/// Random-like sets: every mix must be uniform on them.
const RANDOM_LIKE: [&str; 5] = ["orderkey-f7", "random", "pair", "fp128", "flags"];

#[test]
fn key_hash_never_collapses_and_is_uniform_on_random_like_keys() {
    let mut fails = Vec::new();
    for set in synthetic(1 << 16) {
        let dist = distinct(&set.build);
        let stream = if set.probe.is_empty() { dist.clone() } else { set.probe.clone() };
        let (slack, chi) = if RANDOM_LIKE.contains(&set.name.as_str()) { (1.5, 3.0) } else { (6.0, 50.0) };
        for l in [frac(BUILD_SPARSE_LOAD), frac(BUILD_LOAD), frac(AGG_GROW_LOAD)] {
            for pbits in [0, 4] {
                fails.extend(run(&set, &dist, Hf::Key, l, pbits, 0, &stream).check(slack, chi).err());
            }
        }
        let (key, old) = (t1(&set, Hf::Key), t1(&set, Hf::Old));
        if key > old * 1.05 + 0.01 {
            fails.push(format!("{}: T1 evictions/row {key:.4} > trunk's {old:.4}", set.name));
        }
    }
    // refills: a table filled from one route stream at depth 0..2 keeps its homes spread (the depth rule)
    for set in synthetic(1 << 18) {
        let dist = distinct(&set.build);
        let (slack, chi) = if RANDOM_LIKE.contains(&set.name.as_str()) { (1.5, 3.0) } else { (6.0, 50.0) };
        for l in [frac(BUILD_LOAD), frac(AGG_GROW_LOAD)] {
            for depth in 0..3 {
                let r = refill(&set, &dist, Hf::Key, l, depth);
                if r.inserted >= 128 {
                    fails.extend(r.check(slack, chi).err());
                }
            }
        }
    }
    assert!(fails.is_empty(), "{} failures:\n{}", fails.len(), fails.join("\n"));
}

#[test]
fn the_top_bits_only_shape_never_shares_a_home() {
    // keys varying only above bit s, in tables whose home reads below s: no collapse onto a few homes. The one-word mix
    // folds the key's high half into its low half (D2076), so these keys home as hashed keys do (Fibonacci alone kept them
    // a bijection here and collapsed keys whose hashes share their route bits instead): at least 4/5 of the distinct homes
    // n keys take in `slots` at random, slots * (1 - e^(-n / slots))
    for s in [40u32, 48, 52, 56] {
        let n = (1u64 << (64 - s)).min(1 << 15);
        let keys: Vec<u64> = (0..n).map(|i| i << s).collect();
        for (pbits, bits) in [(0u32, 16u32), (4, 12), (4, 16), (8, 16), (11, 20)] {
            let at = Depth::part(pbits).unwrap();
            let homes: HashSet<(u32, usize)> = keys.iter().map(|&k| (KeyHash::word(k).part(pbits), KeyHash::word(k).home(bits, at))).collect();
            let slots = (1u64 << (pbits + bits)) as f64;
            let random = slots * (1.0 - (-(n as f64) / slots).exp());
            assert!(homes.len() as f64 >= 0.8 * random, "i << {s}, p{pbits} + {bits} bits: {} homes for {n} keys (random {random:.0})", homes.len());
        }
    }
}

#[test]
fn avalanche_per_half() {
    // a multi-word image (the fold) reaches both halves of the top word a table reads: flipping any bit of any word changes
    // about half of the route bits (the top 16) and of the home bits below them (the next 16: 2^16 slots under 16 route
    // bits). A last word's top bits reach down only IMAGE_SHIFT bits before the final multiply, so bits below 32 are not
    // held to it. One-word keys (the high half mixed into the low half, then a multiply: D2076) are held by the gate above.
    let mut rnd = xorshift(99);
    for nw in [2usize, 3, 4] {
        for word in 0..nw {
            for bit in [0u32, 1, 17, 31, 32, 48, 62, 63] {
                let (mut route, mut home, trials) = (0u32, 0u32, 2000u32);
                for _ in 0..trials {
                    let a: Vec<u64> = (0..nw).map(|_| rnd()).collect();
                    let mut b = a.clone();
                    b[word] ^= 1 << bit;
                    let d = KeyHash::of(&a).to_abi() ^ KeyHash::of(&b).to_abi();
                    route += (d >> 48).count_ones();
                    home += ((d >> 32) & 0xffff).count_ones();
                }
                let (r, h) = (route as f64 / (16 * trials) as f64, home as f64 / (16 * trials) as f64);
                assert!((0.4..0.6).contains(&r) && (0.4..0.6).contains(&h), "{nw} words, word {word} bit {bit}: route half {r:.3}, home half {h:.3}");
            }
        }
    }
}

#[test]
fn the_fingerprint_and_bucket_hashes_are_dicts() {
    let mut rnd = xorshift(5);
    for len in (0..=300).chain([511, 512, 513, 1024, 4096]) {
        let b: Vec<u8> = (0..len).map(|_| rnd() as u8).collect();
        let (k0, k1) = (rnd(), rnd());
        assert_eq!(hash::siphash128(k0, k1, &b), siphash128(k0, k1, &b), "siphash128 len {len}");
        assert_eq!(hash::fp128(hash::FpKey { k0, k1 }, &b), siphash128(k0, k1, &b), "fp128 len {len}");
        assert_eq!(hash::hash_bytes64(&b), hash_bytes_ref(&b), "hash_bytes64 len {len}");
    }
}

/// The bits `hash_bytes64` keeps (dict's derived and bits tables were built with them): multiply-rotate over 8-byte words,
/// the tail as one little-endian word, murmur3's fmix64.
fn hash_bytes_ref(b: &[u8]) -> u64 {
    const K1: u64 = 0x9E37_79B9_7F4A_7C15;
    const K2: u64 = 0xC2B2_AE3D_27D4_EB4F;
    let mut h = (b.len() as u64).wrapping_mul(K1) ^ K2;
    for w in b.chunks_exact(8) {
        h = (h ^ u64::from_le_bytes(w.try_into().unwrap())).wrapping_mul(K1).rotate_left(29);
    }
    let tail = b.chunks_exact(8).remainder().iter().enumerate().fold(0u64, |t, (i, &x)| t | (x as u64) << (8 * i));
    let mut h = (h ^ tail).wrapping_mul(K2);
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    h = h.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    h ^ (h >> 33)
}

/// Key files: one key per line, tab-separated columns (integers as words, anything else as its Fp128 pair).
fn load_keys(path: &str, max: usize) -> (Vec<Key>, usize) {
    let s = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let lines: Vec<&str> = s.lines().collect();
    let (mut out, mut nw) = (Vec::new(), 1);
    for line in lines.iter().step_by(lines.len().div_ceil(max.max(1)).max(1)) {
        let (mut k, mut n) = ([0u64; 4], 0);
        for f in line.split('\t') {
            match f.trim().parse::<i64>() {
                Ok(v) if n < 4 => (k[n], n) = (v as u64, n + 1),
                Err(_) if n + 2 <= 4 => ([k[n], k[n + 1]], n) = (siphash128(FP_K.0, FP_K.1, f.as_bytes()), n + 2),
                _ => {}
            }
        }
        nw = nw.max(n);
        out.push(k);
    }
    (out, nw)
}

#[test]
#[ignore = "a measurement: HASHQ_SETS=name=build[:probe][@k],… HASHQ_OUT=out.tsv cargo test --profile fast-profile -p gravity_dict --test hash_quality -- --ignored --nocapture"]
fn hash_quality_report() {
    let max: usize = std::env::var("HASHQ_MAX_ROWS").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000_000);
    let sets: Vec<Set> = match std::env::var("HASHQ_SETS") {
        Err(_) => synthetic(std::env::var("HASHQ_N").ok().and_then(|v| v.parse().ok()).unwrap_or(1 << 20)),
        Ok(spec) => spec
            .split(',')
            .map(|item| {
                let (name, rest) = item.split_once('=').expect("name=build[:probe][@k]");
                let (paths, keep) = rest.split_once('@').map_or((rest, 1), |(p, k)| (p, k.parse::<u64>().expect("@k")));
                let mut ps = paths.splitn(2, ':');
                let (mut build, nw) = load_keys(ps.next().unwrap(), max);
                build.retain(|k| keep <= 1 || splitmix(k[0] ^ k[1].rotate_left(17)) % keep == 0);
                Set { name: name.into(), nw, build, probe: ps.next().map(|p| load_keys(p, max).0).unwrap_or_default() }
            })
            .collect(),
    };
    let mut out = vec!["what\tload\thit\tmiss\tchi_lo\tchi_hi\toverflow\tdegen".to_string()];
    for set in &sets {
        let dist = distinct(&set.build);
        let stream = if set.probe.is_empty() { dist.clone() } else { set.probe.clone() };
        for l in [(1, 2), (5, 8), (3, 4), (7, 8)] {
            for hf in HFS {
                let mut rows: Vec<Row> = [0, 4].iter().map(|&p| run(set, &dist, hf, l, p, 0, &stream)).collect();
                rows.extend((0..3).map(|d| refill(set, &dist, hf, l, d)));
                for r in rows {
                    out.push(format!("{}\t{:.3}\t{:.3}\t{:.3}\t{:.2}\t{:.2}\t{}\t{}", r.what, r.load, r.hit, r.miss, r.chi_lo, r.chi_hi, r.overflow, r.degen));
                }
            }
        }
        for hf in HFS {
            out.push(format!("{} {hf:?} t1\tevictions/row {:.4}", set.name, t1(set, hf)));
        }
        let t0 = Instant::now();
        let acc = set.build.iter().fold(0u64, |a, k| a ^ KeyHash::of(&k[..set.nw]).to_abi());
        out.push(format!("{} keyhash-ns\t{:.2}\t{acc:x}", set.name, t0.elapsed().as_nanos() as f64 / set.build.len().max(1) as f64));
    }
    let path = std::env::var("HASHQ_OUT").unwrap_or_else(|_| "hash_quality.tsv".into());
    std::fs::write(&path, out.join("\n") + "\n").unwrap();
    eprintln!("wrote {path}");
}
