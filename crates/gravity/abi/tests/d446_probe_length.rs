//! D446 (V4R-123): linear-probe length of a table whose keys agree in their route bits, at the old 32-bit wall and past
//! it. Once the route runs into the hash's low bits a table homes on the bits left (zero-filled past the last), so homes
//! correlate with routes; main 09-25 asks that the probe length be no worse than at the wall.
use gravity_abi::hash::{self, Depth, KeyHash};

/// A bijection on the low `m` bits (odd multiplies and xorshifts mod 2^m): distinct keys for a sub-partition's free bits.
fn mix_m(i: u64, m: u32) -> u64 {
    let mask = if m == 64 { u64::MAX } else { (1u64 << m) - 1 };
    let h = (m / 2).max(1);
    let mut x = i.wrapping_mul(0x9E37_79B9_7F4A_7C15) & mask;
    x ^= x >> h;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9) & mask;
    x ^ (x >> h)
}

/// Linear probing of `n` one-word keys whose hashes agree in their top `used` route bits (key set `seed`), in a table sized
/// for them at the aggregate's grow load (3/4): (mean, max) probe distance from the home.
fn probes_at(used: u32, n: u64, seed: u64) -> (f64, usize) {
    let at = Depth::new(used % 3, used / 3).unwrap();
    assert_eq!(at.used(), used);
    let free = 64 - used;
    let cap_bits = (64 - (n * 4 / 3).max(64).leading_zeros()).min(hash::HOME_BITS);
    let cap = 1usize << cap_bits;
    let mut taken = vec![false; cap];
    let (mut sum, mut max) = (0usize, 0usize);
    let pre = 0x5a3c_96e1_d2b4_8f07u64.rotate_left(seed as u32 * 7);
    let prefix = (pre >> free) << free;
    let salt = seed.wrapping_mul(0x2545_f491_4f6c_dd1d);
    let mask = if free == 64 { u64::MAX } else { (1u64 << free) - 1 };
    for i in 0..n {
        let h = prefix | mix_m(i.wrapping_add(salt) & mask, free);
        let kh = KeyHash::word(hash::word_preimage(h)); // the key whose hash is `h`
        assert_eq!(kh.to_abi() >> free, h >> free);
        let home = kh.home(cap_bits, at);
        let mut d = 0;
        while taken[(home + d) & (cap - 1)] {
            d += 1;
        }
        taken[(home + d) & (cap - 1)] = true;
        sum += d;
        max = max.max(d);
    }
    (sum as f64 / n as f64, max)
}

/// D446: once the route runs into the hash's low bits, a table's homes come from the bits left (zero-filled past the last),
/// so homes correlate with routes. 1M keys at the deepest depth trunk reaches (32 route bits), eight key sets, against the
/// depths D446 opens (as many keys as the bits left allow, up to 1M), four key sets each: the mean probe no worse than the
/// worst at-wall set's, the max within the at-wall sets' spread (a max over 1M probes is noisy); from 44 route bits the
/// keys fit their homes one apiece (probe 0).
#[test]
fn d446_probe_length_past_the_old_wall_is_no_worse_than_at_it() {
    let wall: Vec<(f64, usize)> = (0..8).map(|s| probes_at(32, 1 << 20, s)).collect();
    let (m32, x32) = (wall.iter().map(|w| w.0).fold(0.0, f64::max), wall.iter().map(|w| w.1).max().unwrap());
    eprintln!("used 32 n {} x8: means {:?} maxes {:?}", 1 << 20, wall.iter().map(|w| (w.0 * 1000.0).round() / 1000.0).collect::<Vec<_>>(), wall.iter().map(|w| w.1).collect::<Vec<_>>());
    for used in [33u32, 36, 40, 43, 44, 45, 48, 52, 56, 60, 62, 63] {
        let n = (1u64 << 20).min(1u64 << (64 - used));
        for seed in 0..4 {
            let (m, x) = probes_at(used, n, 100 + seed);
            eprintln!("used {used} n {n} set {seed}: mean {m:.3} max {x}");
            assert!(m <= m32 + 0.01 && x <= x32 + x32 / 4, "used {used} set {seed}: mean {m} max {x} vs at the wall {m32} {x32}");
            if used >= 44 {
                assert_eq!((m, x), (0.0, 0), "used {used}: the keys fit their homes one apiece");
            }
        }
    }
}
