//! The worked fit of the cost home's break-even rule (`cost::fit`): machine-profile unit costs on synthetic lanes at the dev
//! seed (never a benchmark's statements, the census or a bar cell). Five units, each over a lane that fits one thread's L1d
//! (one 8,192-row granule of u32), its L2, and DRAM: a sequential read of every row (the whole-input strategy's unit), a
//! gather of sorted random positions (the per-touched-unit strategy's, its per-survivor cost the least-squares slope over
//! survivor counts n/64 .. n/2), a per-row compare into mask words (a check's), a dependent load around a random cycle
//! (the miss latency a prefetch distance covers) and a hashed probe-and-count (a distinct accumulator's insert). It prints
//! a TSV and each size's `break_even_div(gather, read)`. A sixth unit, once: a run-time filter's check of one granule (its
//! facts record's range against the filter's, then a binary search of the filter's key set), the cost a pgrc source pays
//! per granule it asks the filter about. Run on a box (never the Mac), optimized:
//!   cargo test --profile fast-profile -p gravity_program --test cost_fit -- --ignored --nocapture
use gravity_program::cost::{break_even_div, fit::least_squares, Machine};
use std::hint::black_box;
use std::time::Instant;

/// The dev seed of the cost fits.
const DEV_SEED: u64 = 0x5eed_0de7;
const GRANULE_ROWS: usize = 8192;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

/// Nanoseconds per call of `f`, the least of five takes of at least ~20 ms each.
fn ns_per_call(mut f: impl FnMut() -> u64) -> f64 {
    let mut reps = 1u64;
    loop {
        let t = Instant::now();
        for _ in 0..reps {
            black_box(f());
        }
        if t.elapsed().as_millis() >= 20 {
            break;
        }
        reps *= 2;
    }
    (0..5)
        .map(|_| {
            let t = Instant::now();
            for _ in 0..reps {
                black_box(f());
            }
            t.elapsed().as_nanos() as f64 / reps as f64
        })
        .fold(f64::INFINITY, f64::min)
}

fn read_all(lane: &[u32]) -> u64 {
    lane.iter().fold(0u64, |s, &v| s.wrapping_add(v as u64))
}

fn gather(lane: &[u32], pos: &[u32]) -> u64 {
    pos.iter().fold(0u64, |s, &p| s.wrapping_add(lane[p as usize] as u64))
}

/// `steps` dependent loads around a random cycle over `next` (the miss latency at its size).
fn chase(next: &[u32], steps: usize) -> u64 {
    let mut i = 0u32;
    for _ in 0..steps {
        i = next[i as usize];
    }
    i as u64
}

/// A single random cycle over `n` slots (Sattolo's shuffle).
fn cycle(n: usize, rng: &mut Rng) -> Vec<u32> {
    let mut order: Vec<u32> = (0..n as u32).collect();
    for i in (1..n).rev() {
        let j = (rng.next() % i as u64) as usize;
        order.swap(i, j);
    }
    let mut next = vec![0u32; n];
    for k in 0..n {
        next[order[k] as usize] = order[(k + 1) % n];
    }
    next
}

/// One open-addressing probe and count per key into a power-of-two table of `slots` (a distinct-value accumulator's insert:
/// its per-item time with every miss hidden is the L1d size's).
fn update(slots: &mut [u32], keys: &[u32]) -> u64 {
    let m = slots.len() - 1;
    for &k in keys {
        let h = (k as usize).wrapping_mul(0x9e37_79b9) & m;
        slots[h] = slots[h].wrapping_add(1);
    }
    slots[0] as u64
}

/// One run-time filter check per granule record, as the pgrc source makes it: the record's `(min, max)` against the
/// filter's range, then the least key of the sorted key set at or past `min`, which cuts the granule when it is past `max`.
fn probe(recs: &[(i64, i64)], keys: &[i64], lo: i64, hi: i64) -> u64 {
    recs.iter().filter(|&&(mn, mx)| mx < lo || mn > hi || keys.get(keys.partition_point(|&k| k < mn)).is_none_or(|&k| k > mx)).count() as u64
}

fn check(lane: &[u32], c: u32, mask: &mut [u64]) -> u64 {
    for (j, m) in mask.iter_mut().enumerate() {
        let rows = &lane[j * 64..(j * 64 + 64).min(lane.len())];
        let mut bits = 0u64;
        for (b, &v) in rows.iter().enumerate() {
            bits |= ((v < c) as u64) << b;
        }
        *m = bits;
    }
    mask[0]
}

#[test]
#[ignore = "a measurement: run on a box with --ignored (module doc)"]
fn machine_unit_costs_at_the_dev_seed() {
    let m = Machine::REFERENCE;
    let mut rng = Rng(DEV_SEED);
    println!("fit\trows\tbytes\tunit\tns_per_unit\tnote");
    for rows in [GRANULE_ROWS, (m.l2_share() / 4 / 2) as usize, 1 << 24] {
        let lane: Vec<u32> = (0..rows).map(|_| rng.next() as u32).collect();
        let read = ns_per_call(|| read_all(&lane)) / rows as f64;
        let mut pts = Vec::new();
        for div in [64usize, 32, 16, 8, 4, 2] {
            let k = rows / div;
            let mut pos: Vec<u32> = (0..k).map(|_| (rng.next() % rows as u64) as u32).collect();
            pos.sort_unstable();
            let t = ns_per_call(|| gather(&lane, &pos));
            pts.push((k as f64, t));
        }
        let (fixed, per) = least_squares(&pts);
        let mut mask = vec![0u64; rows.div_ceil(64)];
        let c = u32::MAX / 3;
        let chk = ns_per_call(|| check(&lane, c, &mut mask)) / rows as f64;
        let next = cycle(rows, &mut rng);
        let steps = rows.min(1 << 16);
        let miss = ns_per_call(|| chase(&next, steps)) / steps as f64;
        let mut slots = vec![0u32; rows];
        let keys: Vec<u32> = lane[..rows.min(1 << 16)].to_vec();
        let upd = ns_per_call(|| update(&mut slots, &keys)) / keys.len() as f64;
        let bytes = rows * 4;
        println!("fit\t{rows}\t{bytes}\tread\t{read:.4}\tsequential sum");
        println!("fit\t{rows}\t{bytes}\tgather\t{per:.4}\tleast-squares slope over n/64..n/2 sorted positions (fixed {fixed:.0} ns)");
        println!("fit\t{rows}\t{bytes}\tcheck\t{chk:.4}\tcompare into mask words");
        println!("fit\t{rows}\t{bytes}\tmiss\t{miss:.4}\tdependent load around a random cycle");
        println!("fit\t{rows}\t{bytes}\tupdate\t{upd:.4}\thashed probe and count");
        println!("fit\t{rows}\t{bytes}\tbreak_even_div\t{}\tgather vs read", break_even_div(per, read));
        assert!(read > 0.0 && per > 0.0 && chk > 0.0, "every unit costs something");
    }
    // the run-time filter's check: 4,096 granule records (64 KiB, the facts of a 32M-row table) against a full set of 64
    // keys over the whole range, so every record reaches the key search; each record spans 1/128 of the keys' range, so
    // about two in five hold a key (an unpredictable cut)
    let span = 1i64 << 25;
    let mut keys: Vec<i64> = (0..64).map(|_| (rng.next() >> 32) as i64).collect();
    keys.sort_unstable();
    let recs: Vec<(i64, i64)> = (0..4096).map(|_| (rng.next() >> 32) as i64).map(|mn| (mn, mn + span)).collect();
    let chk = ns_per_call(|| probe(&recs, &keys, i64::MIN, i64::MAX)) / recs.len() as f64;
    println!("fit\t{}\t{}\tprobe\t{chk:.4}\trun-time filter check per granule record (64 keys)", recs.len(), recs.len() * 16);
    assert!(chk > 0.0, "a check costs something");
}

/// NEW-51 (cost::drain_set): the points the absorbed DISTINCT set's lines are fitted through, PostgreSQL's presort
/// estimate (rows) against each form's median ms over 4 quiet rounds (corpus n41g; the module doc of `cost::drain_set`):
/// the Sort form, then the set. A by-value key: every int cell of the sweep (box 63, trunk's Sort against the ungated
/// set); a text key: the sweep's text cells with each form forced on NEW-53's keyed end (box 54). `least_squares` of them
/// is the module's lines: the by-value W8 lines cross where its cells flipped, the text set's lies under the Sort's.
#[test]
fn drain_set_lines() {
    use gravity_program::cost::drain_set::{self, Line, Pair};
    const INT_ROWS: [f64; 11] = [275.0, 932.0, 1870.0, 3886.0, 6987.0, 9868.0, 13853.0, 19527.0, 29694.0, 49360.0, 99773.0];
    const TEXT_ROWS: [f64; 5] = [932.0, 3886.0, 9868.0, 19527.0, 49360.0];
    let int_w0 = (vec![2.290, 2.290, 2.300, 2.455, 2.655, 2.905, 3.185, 3.485, 4.005, 5.010, 18.065], vec![2.415, 2.475, 2.500, 2.635, 2.885, 3.070, 3.265, 3.630, 4.135, 5.125, 7.945]);
    let int_w8 = (vec![1.120, 1.175, 1.310, 1.605, 2.085, 2.490, 3.030, 3.775, 5.280, 7.965, 15.615], vec![1.545, 1.575, 1.645, 1.670, 1.870, 2.050, 2.140, 2.440, 3.015, 4.065, 6.340]);
    let text_w0 = (vec![2.860, 3.680, 5.125, 8.000, 23.100], vec![2.815, 3.095, 3.625, 4.480, 6.830]);
    let text_w8 = (vec![1.450, 2.390, 4.055, 6.105, 12.700], vec![1.025, 1.140, 1.580, 1.885, 3.420]);
    let fit = |rows: &[f64], ms: &[f64]| {
        let pts: Vec<(f64, f64)> = rows.iter().zip(ms).map(|(&r, &m)| (r, m * 1e6)).collect();
        least_squares(&pts)
    };
    let close = |l: Line, (fixed, row): (f64, f64)| (l.fixed.ns - fixed).abs() < 100.0 && (l.row.ns - row).abs() < 0.01;
    let cases = [
        ("int W0", drain_set::WORD_W0, &INT_ROWS[..], int_w0),
        ("int W8", drain_set::WORD_W8, &INT_ROWS[..], int_w8),
        ("text W0", drain_set::WIDE_W0, &TEXT_ROWS[..], text_w0),
        ("text W8", drain_set::WIDE_W8, &TEXT_ROWS[..], text_w8),
    ];
    for (name, p, rows, (sort, set)) in cases {
        let (s, t) = (fit(rows, &sort), fit(rows, &set));
        println!("drain_set\t{name}\tsort {:.4} ms + {:.2} ns\tset {:.4} ms + {:.2} ns\tcross {:.0}", s.0 / 1e6, s.1, t.0 / 1e6, t.1, drain_set::crossover(p));
        assert!(close(p.sort, s) && close(p.set, t), "{name}: the module's lines are the fit of its points");
    }
    let flip = |p: Pair, below: f64, above: f64| (below..above).contains(&drain_set::crossover(p));
    assert!(flip(drain_set::WORD_W8, 3886.0, 6987.0), "W8 crosses where the by-value cells flipped");
    assert!(drain_set::crossover(drain_set::WIDE_W8) < TEXT_ROWS[0], "the text set led at every W8 cell");
}

/// D-04 (cost_profile front.rs `PERCODE_*`): the points the per-code front's decline is fitted through, the dev corpus of
/// `PERCODE_CORPUS` (2 x 1M-row parts; a key derived from `k<key>/<leaf>` text in runs of r rows over l codes a key,
/// count(*) FILTER (LIKE) a key; W0, one take an arm): l, r, the kernel's sums of listed codes and opened records, and the
/// kept and declined arms' ms. The first eight cells (runs up to 8, 1 or 8 codes a key) are the fit: least squares of the
/// kept arm's ns a row over the declined's as code x codes + opened x records - row. On every cell the rule costs no more
/// than the 50 % opened rule before it, and where it misses the better arm by more than the A/A (1.02x) it keeps the
/// front, as that rule did.
#[test]
fn percode_front_fit() {
    use cost_profile::front::{percode_declines, PERCODE_CODE, PERCODE_CODE_W, PERCODE_OPENED, PERCODE_OPENED_W, PERCODE_ROW};
    const ROWS: u64 = 2_000_000;
    const CELLS: [(u32, u32, u64, u64, f64, f64); 23] = [
        (1, 1, 2_000_000, 2_000_000, 305.988, 193.443),
        (1, 2, 1_000_244, 1_000_000, 196.752, 193.392),
        (1, 4, 500_000, 500_000, 135.770, 180.073),
        (1, 8, 250_000, 250_000, 105.227, 173.130),
        (8, 1, 2_000_000, 2_000_000, 206.460, 95.395),
        (8, 2, 2_000_000, 1_000_000, 128.274, 89.447),
        (8, 4, 2_000_000, 500_000, 99.711, 90.859),
        (8, 8, 2_000_000, 250_000, 97.158, 94.934),
        (1, 3, 666_831, 666_668, 155.895, 184.300),
        (1, 16, 125_000, 125_000, 76.495, 150.573),
        (1, 32, 62_500, 62_500, 42.375, 51.205),
        (1, 64, 31_250, 31_250, 23.810, 32.722),
        (2, 4, 1_000_488, 500_000, 119.228, 130.371),
        (2, 5, 800_294, 400_000, 106.631, 127.578),
        (4, 8, 1_000_976, 250_000, 87.878, 105.031),
        (8, 16, 1_001_952, 125_000, 69.913, 59.064),
        (8, 20, 801_176, 100_000, 63.417, 58.386),
        (8, 24, 667_984, 83_335, 59.908, 56.111),
        (8, 28, 572_832, 71_430, 56.719, 56.426),
        (8, 32, 500_000, 62_500, 54.701, 55.467),
        (8, 64, 250_000, 31_250, 47.539, 52.982),
        (16, 16, 2_000_000, 12_500, 74.747, 54.202),
        (32, 32, 2_000_000, 6_250, 70.017, 48.139),
    ];
    // the normal equations of [codes, opened, -1] a row against the kept arm's ns a row over the declined's
    let mut a = [[0.0f64; 4]; 3];
    for &(_, _, n, o, k, d) in &CELLS[..8] {
        let (x, y) = ([n as f64 / ROWS as f64, o as f64 / ROWS as f64, -1.0], (k - d) * 1e6 / ROWS as f64);
        for i in 0..3 {
            for j in 0..3 {
                a[i][j] += x[i] * x[j];
            }
            a[i][3] += x[i] * y;
        }
    }
    for i in 0..3 {
        let p = (i..3).max_by(|&u, &v| a[u][i].abs().total_cmp(&a[v][i].abs())).unwrap();
        a.swap(i, p);
        for k in i + 1..3 {
            let f = a[k][i] / a[i][i];
            for j in i..4 {
                a[k][j] -= f * a[i][j];
            }
        }
    }
    let mut w = [0.0f64; 3];
    for i in (0..3).rev() {
        w[i] = (a[i][3] - (i + 1..3).map(|j| a[i][j] * w[j]).sum::<f64>()) / a[i][i];
    }
    println!("percode_front\tcode {:.2} ns\topened {:.2} ns\trow {:.2} ns", w[0], w[1], w[2]);
    let fitted = [PERCODE_CODE.ns, PERCODE_OPENED.ns, PERCODE_ROW.ns];
    assert!(w.iter().zip(fitted).all(|(f, c)| (f - c).abs() < 0.01), "the module's costs are the fit of its points");
    let new = |n: u64, o: u64| percode_declines(n, ROWS, o, PERCODE_CODE_W, PERCODE_OPENED_W);
    let old = |n: u64, o: u64| n * 4096 >= 512 * ROWS && o * 100 >= ROWS * 50;
    let mut kept_as_before = Vec::new();
    for &(l, r, n, o, k, d) in &CELLS {
        let cost = |decline: bool| if decline { d } else { k };
        let (now, before) = (cost(new(n, o)), cost(old(n, o)));
        println!("percode_front\tl{l} r{r}\tkept/declined {:.3}\tnow {}\tbefore {}", k / d, if new(n, o) { "declines" } else { "keeps" }, if old(n, o) { "declines" } else { "keeps" });
        assert!(now <= before, "l{l} r{r}: never dearer than the 50 % opened rule");
        if now > 1.02 * k.min(d) {
            assert!(!new(n, o) && !old(n, o), "l{l} r{r}: a miss keeps the front, as the 50 % opened rule did");
            kept_as_before.push((l, r));
        }
    }
    assert_eq!(kept_as_before, [(8, 8), (8, 16), (8, 20), (8, 24), (16, 16), (32, 32)], "long runs over many codes a key");
    assert!(new(2_000_000, 500_000) && !old(2_000_000, 500_000), "fresh codes, a record in four rows: the cell the fit gains");
}

/// D-04 (cost_profile sparse.rs): the points code.sparse's walk and the whole decode are fitted through (dev data: 2 x
/// 1M-row parts, a text column on one value but for s % of the rows, shuffled; count(*) and an int's sum under an EQ, a
/// LIKE prefix and a byte range; each form forced, one take an arm): each form's ms at s = 20 .. 95 %, W0 and W8, the
/// walk's then the whole decode's. The module's lines are the least squares of the W0 cells' ns a row over the non-base
/// share, the quals pooled (EQ from 40 %, below which its bound answers without either form); each qual's own lines cross
/// within 66 .. 72 % at both widths.
#[test]
fn code_sparse_lines() {
    use cost_profile::sparse::{crossover_pct, DECODE_NONBASE, DECODE_ROW, DENSE_PCT, WALK_NONBASE, WALK_ROW};
    type Cells = [([f64; 9], [f64; 9]); 3];
    const SHARE: [f64; 9] = [0.20, 0.30, 0.40, 0.50, 0.60, 0.70, 0.80, 0.90, 0.95];
    const W0: Cells = [
        ([0.305, 0.296, 2.634, 3.133, 3.514, 3.870, 4.341, 4.771, 5.017], [0.306, 0.309, 3.861, 3.859, 3.869, 3.862, 3.794, 3.804, 3.796]),
        ([2.189, 2.510, 2.992, 3.649, 4.160, 4.471, 4.971, 5.382, 5.520], [3.940, 4.122, 4.243, 4.367, 4.574, 4.612, 4.536, 4.733, 4.658]),
        ([2.279, 2.564, 3.087, 3.746, 4.244, 4.652, 5.095, 5.604, 5.688], [4.129, 4.293, 4.355, 4.456, 4.553, 4.734, 4.773, 4.906, 4.895]),
    ];
    const W8: Cells = [
        ([0.445, 0.439, 0.829, 0.885, 0.948, 0.983, 1.022, 1.111, 1.119], [0.470, 0.445, 0.960, 0.980, 0.971, 1.010, 0.968, 0.965, 0.985]),
        ([0.754, 0.802, 0.860, 0.950, 1.016, 1.019, 1.087, 1.160, 1.191], [0.985, 0.979, 0.994, 1.011, 1.023, 1.026, 1.034, 1.043, 1.054]),
        ([0.779, 0.818, 0.877, 1.019, 1.040, 1.091, 1.117, 1.174, 1.174], [0.982, 1.014, 1.039, 1.049, 1.070, 1.073, 1.080, 1.138, 1.077]),
    ];
    let line = |cells: &Cells, quals: &[usize], walk: bool| {
        let pts: Vec<(f64, f64)> = quals
            .iter()
            .flat_map(|&q| (if q == 0 { 2 } else { 0 }..9).map(move |i| (q, i)))
            .map(|(q, i)| (SHARE[i], (if walk { cells[q].0[i] } else { cells[q].1[i] }) * 1e6 / 2_000_000.0))
            .collect();
        least_squares(&pts)
    };
    let (walk, whole) = (line(&W0, &[0, 1, 2], true), line(&W0, &[0, 1, 2], false));
    println!("code_sparse\twalk {:.3} + {:.3} x s ns\twhole {:.3} + {:.3} x s ns", walk.0, walk.1, whole.0, whole.1);
    let close = |(row, nonbase): (f64, f64), r: f64, s: f64| (row - r).abs() < 0.001 && (nonbase - s).abs() < 0.001;
    assert!(close(walk, WALK_ROW.ns, WALK_NONBASE.ns) && close(whole, DECODE_ROW.ns, DECODE_NONBASE.ns), "the module's lines are the fit of its points");
    assert_eq!(DENSE_PCT, 70);
    for (width, cells) in [("W0", &W0), ("W8", &W8)] {
        for (q, qual) in ["EQ", "LIKE", "range"].into_iter().enumerate() {
            let (a, b) = (line(cells, &[q], true), line(cells, &[q], false));
            let pct = crossover_pct(a.0, a.1, b.0, b.1);
            println!("code_sparse\t{width} {qual}\tcrosses at {pct} %");
            assert!((66..=72).contains(&pct), "{width} {qual}: {pct} %");
        }
    }
}

// D3063: the derived grant pool fits RAM beside shared_buffers and the part cache, and never grows past the old
// (RAM - shared_buffers) x 3/4.
#[test]
fn derived_pool_cap_fits_ram_beside_shared_buffers_and_the_part_cache() {
    use gravity_program::cost::{derived_pool_cap, part_cache_cap};
    let gib = 1u64 << 30;
    let ram = 30 * gib + 3 * gib / 4;
    let sb = ram / 4;
    let pool = derived_pool_cap(ram, sb);
    assert!(pool < (ram - sb) / 4 * 3, "{pool}");
    assert!(sb + pool + part_cache_cap(ram, pool) <= ram / 64 * 54, "{pool}");
    assert!(pool > 14 * gib && pool < 15 * gib, "{pool}");
    // a small shared_buffers keeps the old three quarters where it is the smaller
    assert_eq!(derived_pool_cap(ram, ram / 2), 0.max((ram / 64 * 54).saturating_sub(ram / 2).saturating_sub(ram / 8)).min((ram - ram / 2) / 4 * 3));
    assert_eq!(derived_pool_cap(0, 0), 0);
}

// Memory step 5: one budget B = min(B_auto, pgrust.memory_target, the governor's bar), the free reserve
// inside it (plan section 7, C5), B_auto = limit - max(1 GiB, limit / 32) - the page-cache floor (limit / 16 by default).
#[test]
fn the_memory_budget_is_the_limit_less_the_os_and_page_cache_capped_at_the_bar() {
    use gravity_program::cost::{budget_auto, budget_of, os_reserve, page_cache_floor_default, reserve_high, reserve_low};
    let gib = 1u64 << 30;
    let mib = 1u64 << 20;
    let bar_of = |l: u64| (l / 100) * 90;
    // a 30.75 GiB box: the OS keeps 1 GiB (L / 32 is less), the page cache L / 16, and the governor's bar binds
    let l = 30 * gib + 3 * gib / 4;
    assert_eq!(os_reserve(l), gib);
    assert_eq!(page_cache_floor_default(l), l / 16);
    assert_eq!(budget_auto(l, gib, l / 16), l - gib - l / 16);
    let b = budget_of(l, 0, 0, 0, bar_of(l));
    assert_eq!(b.b_auto, l - gib - l / 16);
    assert_eq!(b.b, bar_of(l), "the bar binds: B_auto is 0.906 L, the bar 0.9 L");
    assert!(b.b > 27 * gib && b.b < 28 * gib, "{}", b.b);
    assert_eq!((b.r_low, b.r_high), (gib, 2 * gib), "the reserve inside B: max(1 GiB, B / 32) and 1 GiB more");
    // no governor: B_auto; a target only lowers B, never raises it
    assert_eq!(budget_of(l, 0, 0, 0, 0).b, l - gib - l / 16);
    assert_eq!(budget_of(l, 4096 * mib, 0, 0, bar_of(l)).b, 4096 * mib);
    assert_eq!(budget_of(l, 64 * gib, 0, 0, 0).b, l - gib - l / 16);
    // the page-cache floor and the reserve as set
    assert_eq!(budget_of(l, 0, 2 * gib, 0, 0).b, l - gib - 2 * gib);
    let r = budget_of(l, 0, 0, 512 * mib, 0);
    assert_eq!((r.r_low, r.r_high), (512 * mib, gib));
    // a 123 GiB box: the OS keeps L / 32, the reserve is B / 32 and as much again
    let l = 123 * gib;
    let b = budget_of(l, 0, 0, 0, bar_of(l));
    assert_eq!(b.os_reserve, l / 32);
    assert_eq!(b.b_auto, l - l / 32 - l / 16);
    assert_eq!(b.b, bar_of(l));
    assert_eq!(b.r_low, reserve_low(b.b));
    assert_eq!(b.r_low, b.b / 32);
    assert_eq!(b.r_high, reserve_high(b.b));
    assert_eq!(b.r_high, 2 * (b.b / 32));
    // an unknown limit: no budget
    let z = budget_of(0, 0, 0, 0, 0);
    assert_eq!((z.b_auto, z.b), (0, 0));
}
