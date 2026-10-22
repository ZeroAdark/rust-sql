//! The exact fast path (float_cast.rs) against the text path it replaced, bit for bit.

use crate::fixed_scale::fixed_scale_unpack_i128;
use crate::float_cast::{fixed_scale_to_f32, fixed_scale_to_f64, float4_exact, float8_exact};
use crate::io::get_str_from_var;
use crate::ops::{numeric_float4, numeric_float8, numeric_float8_no_overflow};
use crate::var::{make_result_into, NumericImage, VarView};
use crate::{Num, NumericDigit, NUMERIC_NEG, NUMERIC_POS, NUMERIC_SHORT, NUMERIC_SHORT_SIGN_MASK};

fn bytes(words: &[u16]) -> &[u8] {
    // SAFETY: the bytes of a live, aligned u16 slice.
    unsafe { core::slice::from_raw_parts(words.as_ptr().cast::<u8>(), words.len() * 2) }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn digits(&mut self, d: u32) -> u128 {
        let lo = 10u128.pow(d - 1);
        let span = 9 * lo;
        let r = ((self.next() as u128) << 64) | self.next() as u128;
        lo + r % span
    }
}

/// make_result's image of (-1)^neg x m x 10^-scale at `dscale` (below the stored fraction: digits past it kept).
fn build(img: &mut NumericImage, neg: bool, m: u128, scale: i32, dscale: i32) {
    let (big, frac_groups) = if scale >= 0 {
        let pad = (4 - scale % 4) % 4;
        (m * 10u128.pow(pad as u32), (scale + pad) / 4)
    } else {
        let k = -scale;
        (m * 10u128.pow((k % 4) as u32), -(k / 4))
    };
    let mut groups = [0 as NumericDigit; 12];
    let mut n = 0usize;
    let mut x = big;
    while x != 0 {
        groups[n] = (x % 10_000) as NumericDigit;
        x /= 10_000;
        n += 1;
    }
    groups[..n].reverse();
    let view = VarView {
        ndigits: n as i32,
        weight: if n == 0 {
            0
        } else {
            n as i32 - 1 - frac_groups
        },
        sign: if neg { NUMERIC_NEG } else { NUMERIC_POS },
        dscale,
        digits: &groups[..n],
    };
    assert!(
        make_result_into(view, img),
        "test value overflows the packed format"
    );
}

#[derive(Default, Clone, Copy)]
struct Tally {
    values: u64,
    f8_fast: u64,
    f4_fast: u64,
    f8_err: u64,
}

impl Tally {
    fn add(&mut self, o: Tally) {
        self.values += o.values;
        self.f8_fast += o.f8_fast;
        self.f4_fast += o.f4_fast;
        self.f8_err += o.f8_err;
    }
}

fn check(num: Num<'_>, text: &mut Vec<u8>, t: &mut Tally) {
    t.values += 1;
    text.clear();
    get_str_from_var(num.view(), text);
    let s = core::str::from_utf8(text).expect("numeric text is ASCII");

    let want8 = adt_float::float8in(s, None)
        .map(f64::to_bits)
        .map_err(|_| ());
    let got8 = numeric_float8(num).map(f64::to_bits).map_err(|_| ());
    assert_eq!(got8, want8, "numeric_float8({s})");
    if let Some(v) = float8_exact(num) {
        t.f8_fast += 1;
        assert_eq!(Ok(v.to_bits()), want8, "float8_exact({s})");
    }
    if want8.is_err() {
        t.f8_err += 1;
    }
    let want_no = s
        .parse::<f64>()
        .expect("numeric text parses as f64")
        .to_bits();
    assert_eq!(
        numeric_float8_no_overflow(num).to_bits(),
        want_no,
        "numeric_float8_no_overflow({s})"
    );

    let want4 = adt_float::float4in(s, None)
        .map(f32::to_bits)
        .map_err(|_| ());
    let got4 = numeric_float4(num).map(f32::to_bits).map_err(|_| ());
    assert_eq!(got4, want4, "numeric_float4({s})");
    if let Some(v) = float4_exact(num) {
        t.f4_fast += 1;
        assert_eq!(Ok(v.to_bits()), want4, "float4_exact({s})");
    }
}

fn on_all_cores(work: impl Fn(u64, u64, &mut Tally) + Sync) -> Tally {
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(16) as u64;
    let work = &work;
    let mut total = Tally::default();
    std::thread::scope(|sc| {
        let hs: Vec<_> = (0..workers)
            .map(|w| {
                sc.spawn(move || {
                    let mut t = Tally::default();
                    work(w, workers, &mut t);
                    t
                })
            })
            .collect();
        for h in hs {
            total.add(
                h.join()
                    .expect("worker panicked: a fast-path value differs from the text path"),
            );
        }
    });
    total
}

const RANDOM_VALUES: u64 = 10_000_000;

fn one(
    img: &mut NumericImage,
    text: &mut Vec<u8>,
    neg: bool,
    m: u128,
    scale: i32,
    dscale: i32,
    t: &mut Tally,
) {
    build(img, neg, m, scale, dscale);
    check(img.num(), text, t);
}

fn edges() -> Vec<(u128, i32)> {
    let mut v = Vec::new();
    let p53 = 1u128 << 53;
    let p24 = 1u128 << 24;
    for scale in 0..=30 {
        for j in 0..=4000u128 {
            v.push((p53 - 2000 + j, scale));
        }
        for j in 0..=200u128 {
            for k in 0..=20u32 {
                let p = 10u128.pow(k);
                v.push((p + j, scale));
                if p > j {
                    v.push((p - j, scale));
                }
            }
        }
    }
    for scale in 0..=12 {
        for j in 0..=4000u128 {
            v.push((p24 - 2000 + j, scale));
        }
    }
    for k in 0..=30 {
        for j in 1..=2000u128 {
            v.push((j, k));
            v.push((j, -k));
            v.push((p53 - j, -k));
        }
    }
    for j in 0..2000u128 {
        v.push(((u64::MAX as u128) - j, 0));
        v.push(((u64::MAX as u128) + 1 + j, 0));
        v.push((p53 + 2 * j + 1, 0));
        v.push(((1u128 << 63) + 1024 * (2 * j + 1), 0));
        v.push(((1u128 << 64) - 1024 * (2 * j + 1), 0));
        v.push((p24 + 2 * j + 1, 0));
        v.push(((p24 + 2 * j + 1) * 10, 1));
    }
    v
}

#[test]
fn fast_path_equals_text_path() {
    let edges = edges();
    let edges = &edges;
    let t = on_all_cores(|w, workers, t| {
        let mut rng = Rng(0x6e75_6d63_6173_7400 ^ w);
        let mut img = NumericImage::empty();
        let mut text = Vec::with_capacity(512);
        let (img, text) = (&mut img, &mut text);

        for _ in 0..RANDOM_VALUES / workers {
            let d = 1 + rng.below(20) as u32;
            let mut m = rng.digits(d);
            if rng.below(4) == 0 {
                let z = 10u128.pow(rng.below(d as u64) as u32);
                m = m / z * z;
            }
            let scale = rng.below(31) as i32;
            let neg = rng.below(2) == 1;
            one(img, text, neg, m, scale, scale, t);
        }
        for _ in 0..1_000_000 / workers {
            let d = 1 + rng.below(17) as u32;
            let k = rng.below(31) as i32;
            let neg = rng.below(2) == 1;
            let m = rng.digits(d);
            one(img, text, neg, m, -k, 0, t);
        }
        for _ in 0..1_000_000 / workers {
            let d = 1 + rng.below(20) as u32;
            let scale = rng.below(31) as i32;
            let dscale = if rng.below(2) == 0 {
                (scale - 1 - rng.below(8) as i32).max(0)
            } else {
                scale + 1 + rng.below(80) as i32
            };
            let neg = rng.below(2) == 1;
            let m = rng.digits(d);
            one(img, text, neg, m, scale, dscale, t);
        }
        for _ in 0..200_000 / workers {
            let d = 1 + rng.below(30) as u32;
            let k = rng.below(340) as i32;
            let scale = if rng.below(2) == 0 { k } else { -k };
            let neg = rng.below(2) == 1;
            let m = rng.digits(d);
            one(img, text, neg, m, scale, scale.max(0), t);
        }
        for (i, &(m, scale)) in edges.iter().enumerate() {
            if i as u64 % workers == w {
                one(img, text, false, m, scale, scale.max(0), t);
                one(img, text, true, m, scale, scale.max(0), t);
            }
        }
    });

    let mut s = Tally::default();
    let mut text = Vec::new();
    assert!(numeric_float8(NumericImage::nan().num()).unwrap().is_nan());
    assert!(numeric_float4(NumericImage::nan().num()).unwrap().is_nan());
    assert_eq!(
        numeric_float8(NumericImage::pinf().num()).unwrap(),
        f64::INFINITY
    );
    assert_eq!(
        numeric_float8(NumericImage::ninf().num()).unwrap(),
        f64::NEG_INFINITY
    );
    assert_eq!(
        numeric_float4(NumericImage::pinf().num()).unwrap(),
        f32::INFINITY
    );
    assert_eq!(
        numeric_float4(NumericImage::ninf().num()).unwrap(),
        f32::NEG_INFINITY
    );
    assert!(numeric_float8_no_overflow(NumericImage::nan().num()).is_nan());
    s.values += 3;
    let mut img = NumericImage::empty();
    for dscale in 0..=100 {
        one(&mut img, &mut text, false, 0, dscale, dscale, &mut s);
    }
    let neg_zero = [NUMERIC_SHORT | NUMERIC_SHORT_SIGN_MASK];
    let nz = Num::from_payload(bytes(&neg_zero));
    check(nz, &mut text, &mut s);
    assert_eq!(numeric_float8(nz).unwrap().to_bits(), (-0.0f64).to_bits());
    assert_eq!(numeric_float4(nz).unwrap().to_bits(), (-0.0f32).to_bits());
    let bad = [NUMERIC_SHORT, 10_000];
    let bn = Num::from_payload(bytes(&bad));
    assert_eq!(float8_exact(bn), None);
    assert_eq!(float4_exact(bn), None);
    assert!(numeric_float8(bn).is_err());
    assert!(numeric_float4(bn).is_err());

    let mut total = t;
    total.add(s);
    println!(
        "numcast equality: {} values bit-identical (float8 fast path {}, float4 fast path {}, float8 errors {}, edges {})",
        total.values,
        total.f8_fast,
        total.f4_fast,
        total.f8_err,
        edges.len()
    );
    assert!(total.values >= 10_000_000);
    assert!(
        total.f8_fast >= 5_000_000 && total.f4_fast >= 1_000_000,
        "the fast paths must fire"
    );
}

#[test]
fn fixed_scale_core_equals_cast() {
    let t = on_all_cores(|w, workers, t| {
        let mut rng = Rng(0x6669_7865_6473_6361 ^ w);
        let one = |m: i128, scale: i32, t: &mut Tally| {
            let img = fixed_scale_unpack_i128(m, scale).expect("fixed-scale image");
            t.values += 1;
            let want8 = numeric_float8(img.num()).map(f64::to_bits).map_err(|_| ());
            if let Some(v) = fixed_scale_to_f64(m, scale) {
                t.f8_fast += 1;
                assert_eq!(Ok(v.to_bits()), want8, "fixed_scale_to_f64({m}, {scale})");
            }
            let want4 = numeric_float4(img.num()).map(f32::to_bits).map_err(|_| ());
            if let Some(v) = fixed_scale_to_f32(m, scale) {
                t.f4_fast += 1;
                assert_eq!(Ok(v.to_bits()), want4, "fixed_scale_to_f32({m}, {scale})");
            }
        };
        for _ in 0..2_000_000 / workers {
            let d = 1 + rng.below(17) as u32;
            let mut m = rng.digits(d) as i128;
            if m >= 1 << 55 {
                m %= 1 << 55;
            }
            if rng.below(2) == 1 {
                m = -m;
            }
            let scale = rng.below(31) as i32;
            one(m, scale, &mut *t);
        }
        for _ in 0..200_000 / workers {
            // 18-34 digits: times 10^3 (the fraction pad) stays within i128
            let d = 18 + rng.below(17) as u32;
            let m = rng.digits(d) as i128 * if rng.below(2) == 1 { -1 } else { 1 };
            one(m, rng.below(31) as i32, &mut *t);
        }
        let mut i = 0u64;
        for scale in 0..=30 {
            for j in 0..=3000i128 {
                for m in [
                    j,
                    -j,
                    (1 << 53) - 1500 + j,
                    (1 << 24) - 1500 + j,
                    (1 << 55) - 1 - j,
                    10i128.pow(15) + j,
                ] {
                    i += 1;
                    if i % workers == w {
                        one(m, scale, &mut *t);
                    }
                }
            }
        }
    });
    println!(
        "numcast fixed-scale entry: {} values bit-identical to the cast of their image (float8 answered {}, float4 answered {})",
        t.values, t.f8_fast, t.f4_fast
    );
    assert!(
        t.f8_fast * 2 > t.values && t.f4_fast > 100_000,
        "the fixed-scale entry must answer in its range"
    );
}

#[test]
fn fast_path_equals_text_path_exhaustive_7_digits() {
    let t = on_all_cores(|w, workers, t| {
        let mut img = NumericImage::empty();
        let mut text = Vec::with_capacity(64);
        let mut m = w as u128;
        while m < 10_000_000 {
            for scale in 0..=6 {
                one(&mut img, &mut text, false, m, scale, scale, t);
                if m % 7 == 0 {
                    one(&mut img, &mut text, true, m, scale, scale, t);
                }
            }
            m += workers as u128;
        }
    });
    println!(
        "numcast exhaustive <= 7 digits, scales 0-6: {} values bit-identical (float8 fast path {}, float4 fast path {})",
        t.values, t.f8_fast, t.f4_fast
    );
    assert_eq!(t.values, 70_000_000 + 7 * 1_428_572);
    assert_eq!(t.f8_fast, t.values);
}

/// `cargo test --profile fast-profile -p adt_numeric --lib numcast_bench -- --ignored --nocapture`
#[test]
#[ignore]
fn numcast_bench() {
    use core::hint::black_box;
    use std::time::Instant;

    fn before8(num: Num<'_>) -> f64 {
        let mut buf = Vec::with_capacity(32);
        get_str_from_var(num.view(), &mut buf);
        adt_float::float8in(core::str::from_utf8(&buf).unwrap(), None).unwrap()
    }
    fn before4(num: Num<'_>) -> f32 {
        let mut buf = Vec::with_capacity(32);
        get_str_from_var(num.view(), &mut buf);
        adt_float::float4in(core::str::from_utf8(&buf).unwrap(), None).unwrap()
    }

    const N: usize = 1_000_000;
    let mut rng = Rng(42);
    let classes: [(&str, Box<dyn Fn(&mut Rng) -> (u128, i32)>); 5] = [
        (
            "scale 2, 200.00-24200.00",
            Box::new(|r: &mut Rng| (20_000 + r.below(2_400_000) as u128, 2)),
        ),
        (
            "scale 3, 0.050-4.000",
            Box::new(|r: &mut Rng| (50 + r.below(3951) as u128, 3)),
        ),
        (
            "integer, 1-9 digits",
            Box::new(|r: &mut Rng| {
                let d = 1 + r.below(9) as u32;
                (r.digits(d), 0)
            }),
        ),
        (
            "15 digits, scale 10",
            Box::new(|r: &mut Rng| (r.digits(15), 10)),
        ),
        (
            "25 digits, scale 12 (text path)",
            Box::new(|r: &mut Rng| (r.digits(25), 12)),
        ),
    ];
    for (name, make) in classes.iter() {
        let imgs: Vec<NumericImage> = (0..N)
            .map(|_| {
                let (m, s) = make(&mut rng);
                let mut img = NumericImage::empty();
                build(&mut img, false, m, s, s);
                img
            })
            .collect();
        let time = |f: &dyn Fn(Num<'_>) -> f64| {
            let mut best = f64::MAX;
            for _ in 0..5 {
                let t0 = Instant::now();
                let mut acc = 0.0;
                for img in &imgs {
                    acc += f(black_box(img.num()));
                }
                black_box(acc);
                best = best.min(t0.elapsed().as_nanos() as f64 / N as f64);
            }
            best
        };
        let b8 = time(&|n| before8(n));
        let a8 = time(&|n| numeric_float8(n).unwrap());
        let b4 = time(&|n| before4(n) as f64);
        let a4 = time(&|n| numeric_float4(n).unwrap() as f64);
        println!(
            "numcast_bench {name:34} float8 {b8:6.1} -> {a8:5.1} ns/value ({:4.1}x)   float4 {b4:6.1} -> {a4:5.1} ns/value ({:4.1}x)",
            b8 / a8,
            b4 / a4
        );
    }
}
