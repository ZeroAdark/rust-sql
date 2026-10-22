//! The compiled owner fold, the counted loops and `HashAgg::fold_run` agree on the return, the table and the candidates.

use gravity_abi::breaker::{fits_inline_at, Candidates, HashAgg, IntervalSum, KeyKind, Rec, RecordLayout, SlotKind, SubSplit, FOLD_OVERFLOW, HASHAGG_HDR, MAX_RECORD_KEYS, MAX_RECORD_SLOTS};
use gravity_abi::hash::Depth;
use gravity_exec::table::fold_code::compile;
use gravity_kernels::table::fold_jit::{self, Made};
use gravity_kernels::table::owner_fold::Fold;

fn lcg(s: &mut u64) -> u64 {
    *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *s >> 17
}

use SlotKind::*;

/// `RecordLayout::new_nn` refuses the integer and code maxima.
const NN_KINDS: &[SlotKind] = &[Count, SumI64, SumI128, AvgI128, SumF64, MinCode, SumF32, F64Accum, SumNum, AvgNum, SumNumWide, AvgNumWide, IntervalMin, IntervalMax];
const VALID_KINDS: &[SlotKind] = &[Count, SumI64, SumI128, AvgI128, SumF64, MinCode, MaxCode, MinI64, MaxI64, SumF32, F64Accum, BitAnd, BitOr, BitXor, SumNum, AvgNum, SumNumWide, AvgNumWide, IntervalMin, IntervalMax, IntervalSum];
const PLAIN_KINDS: &[SlotKind] = &[Count, SumI64, SumI128, AvgI128, SumF64, MinCode, MaxCode, MinI64, MaxI64];

fn layouts(count: usize, seed: u64, pool: &[SlotKind]) -> Vec<RecordLayout> {
    let mut rng = seed;
    let mut out = Vec::new();
    while out.len() < count {
        let nw = 1 + (lcg(&mut rng) % MAX_RECORD_KEYS as u64) as usize;
        let validity = lcg(&mut rng) % 2 == 0;
        let kinds: Vec<SlotKind> = pool.iter().copied().filter(|k| (if validity { VALID_KINDS } else { NN_KINDS }).contains(k)).collect();
        let ns = 1 + (lcg(&mut rng) % (MAX_RECORD_SLOTS as u64 - 1)) as usize;
        let mut slots: Vec<SlotKind> = (0..ns).map(|_| kinds[(lcg(&mut rng) % kinds.len() as u64) as usize]).collect();
        let sentinel = match lcg(&mut rng) % 3 {
            0 => None,
            1 => slots.iter().rposition(|&k| k == Count),
            _ => {
                let j = (lcg(&mut rng) % ns as u64) as usize;
                slots[j] = Count;
                slots.iter().position(|&k| k == Count)
            }
        };
        let keys = vec![KeyKind::I64; nw];
        if !fits_inline_at(&keys, &slots, validity, sentinel) {
            continue;
        }
        let l = RecordLayout::with_sentinel(&keys, &slots, validity, sentinel);
        let pw = (lcg(&mut rng) % 3 == 0).then(|| 1 + (lcg(&mut rng) % nw as u64) as u8).unwrap_or(0);
        out.push(l.partitioned_by(pw));
    }
    out
}

fn make_record(buf: &mut [u8; 256], layout: &RecordLayout, keys: &[u64], v: u64, null: bool, big: bool) {
    buf.fill(0);
    let r = unsafe { Rec::at(buf.as_mut_ptr(), layout) };
    r.open(keys);
    for j in 0..layout.nslots() {
        if layout.has_validity() && layout.slot_off(j) != layout.sentinel_off() && (v >> (8 + j)) % 3 == 0 {
            continue;
        }
        let s = v.rotate_left(7 * j as u32);
        match layout.slot_kind(j) {
            Count => r.add_count(j, 1 + (s % 3) as i64),
            SumI64 => r.add_i64(j, (s % 1000) as i64 - 500),
            SumI128 => r.add_i128(j, ((s % (1 << 40)) as i128 * 1_000_000_007).wrapping_mul(if s % 2 == 0 { 1 } else { -(1 << 50) })),
            MinI64 => r.min_i64(j, (s % 5000) as i64 - 2500),
            MaxI64 => r.max_i64(j, (s % 5000) as i64 - 2500),
            AvgI128 => r.add_avg(j, (s % 5000) as i128 - 2500, 1),
            SumF64 => r.add_f64(j, if big { f64::MAX / 1.5 } else { (s % 100) as f64 / 7.0 - 5.0 }).expect("finite"),
            SumF32 => r.add_f32(j, (s % 100) as f64 / 8.0).expect("finite"),
            F64Accum => r.combine_f64_accum(j, [1.0, (s % 100) as f64 / 8.0, 0.0]).expect("finite"),
            BitAnd | BitOr | BitXor => r.fold_bits(j, (s % 4096) as i64),
            MinCode => r.min_code(j, (s % 4) as u32, (s % 977) as u32),
            MaxCode => r.max_code(j, (s % 4) as u32, (s % 977) as u32),
            SumNum => r.add_i128(j, (s % (1 << 40)) as i128 * 1_000_000_007),
            AvgNum => r.add_avg(j, (s % 5000) as i128, 1),
            SumNumWide => r.add_num256(j, [s.wrapping_mul(1_000_000_007), s % 7, 0, 0]),
            AvgNumWide => r.add_avg_num256(j, [s % 5000, 0, 0, 0], 1),
            IntervalMin | IntervalMax => r.fold_interval(j, ((s % 5000) as i64 * 3_600_000_000, (s % 7) as i32 - 3, (s % 3) as i32), layout.slot_kind(j) == IntervalMin),
            IntervalSum => {
                let mut i = IntervalSum::default();
                i.accum((s % 5000) as i64 * 3_600_000_000, (s % 7) as i32 - 3, (s % 3) as i32);
                r.fold_interval_sum(j, &i).expect("in range")
            }
            k => unreachable!("{k:?}"),
        }
    }
    if null {
        r.set_null_seen();
    }
}

fn records(layout: &RecordLayout, n: usize, domain: u64, seed: u64, big: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rng = seed;
    let mut buf = [0u8; 256];
    for i in 0..n {
        let keys: Vec<u64> = (0..layout.nkeys()).map(|_| lcg(&mut rng) % domain).collect();
        let v = lcg(&mut rng) | lcg(&mut rng) << 40;
        make_record(&mut buf, layout, &keys, v, layout.has_validity() && i % 5 == 0, big);
        out.extend_from_slice(&buf[..layout.size() as usize]);
    }
    out
}

struct Table {
    region: Vec<u64>,
    cand: Vec<u32>,
}

impl Table {
    fn open(layout: &RecordLayout, cap: u32, route: Option<Depth>, room: usize) -> (Table, *mut HashAgg) {
        let mut t = Table { region: vec![0u64; (HASHAGG_HDR + cap as usize * layout.size() as usize) / 8 + 1], cand: vec![0u32; room.max(1)] };
        let p = unsafe { HashAgg::at(t.region.as_mut_ptr() as *mut u8, layout, cap, (t.region.len() * 8) as u32) };
        if let Some(d) = route {
            p.set_route_bits(d);
        }
        let p = p as *mut HashAgg;
        (t, p)
    }
}

#[derive(Clone, Copy, Debug)]
struct Case {
    cap: u32,
    route: Option<(u32, u32)>,
    thr: i64,
    room: usize,
}

fn fold_three(e: gravity_abi::program::Entry, layout: &RecordLayout, recs: &[u8], n: usize, c: Case, what: &str) -> i64 {
    let f = Fold::of(layout).expect("a generated fold");
    let route = c.route.map(|(p, d)| Depth::new(p, d).expect("a depth"));
    let mut out = Vec::new();
    for arm in 0..3 {
        let (mut t, tp) = Table::open(layout, c.cap, route, c.room);
        let mut cand = unsafe { Candidates::at(t.cand.as_mut_ptr() as *mut u8, c.room, layout.sentinel_off() as usize, c.thr) };
        let v = unsafe {
            match arm {
                0 => fold_jit::run(e, tp, recs.as_ptr(), n, &mut cand),
                1 => f.run(tp, recs.as_ptr(), n, SubSplit::NONE, &mut cand),
                _ => (*tp).fold_run(recs.as_ptr(), n, SubSplit::NONE, &mut cand),
            }
        };
        let cands = t.cand[..cand.len].to_vec();
        out.push((v, cand.len, cand.overflow, cands, std::mem::take(&mut t.region)));
    }
    for (arm, o) in out.iter().enumerate().skip(1) {
        let name = ["compiled", "loops", "generic"][arm];
        assert_eq!(out[0].0, o.0, "{what} {c:?}: the return, compiled vs {name}");
        if out[0].0 == FOLD_OVERFLOW {
            continue;
        }
        assert_eq!((out[0].1, out[0].2), (o.1, o.2), "{what} {c:?}: the candidate list, compiled vs {name}");
        assert!(out[0].3 == o.3, "{what} {c:?}: the candidate indexes, compiled vs {name}");
        assert!(out[0].4 == o.4, "{what} {c:?}: the table bytes, compiled vs {name}");
    }
    out[0].0
}

fn what(l: &RecordLayout) -> String {
    format!("{} keys / {} partition, {:?}, validity {}, sentinel at {}", l.nkeys(), l.partition_keys(), l.slot_kinds(), l.has_validity(), l.sentinel_off())
}

fn compiled(layout: &RecordLayout) -> fold_jit::Compiled {
    let f = Fold::of(layout).unwrap_or_else(|| panic!("{}: no generated fold", what(layout)));
    match compile(&f) {
        Made::Code(c) => c,
        Made::Refused => panic!("{}: the compiler refused it", what(layout)),
        Made::Later => panic!("{}: no compile", what(layout)),
    }
}

fn cases(n: usize) -> Vec<Case> {
    vec![
        Case { cap: 8192, route: None, thr: 2, room: 4096 },
        Case { cap: 8192, route: Some((3, 0)), thr: 3, room: 16 },
        Case { cap: 64, route: None, thr: 0, room: 4096 },
        Case { cap: 2, route: None, thr: 0, room: 4096 },
        Case { cap: 1, route: None, thr: 0, room: 4096 },
        Case { cap: (n as u32).next_power_of_two(), route: Some((2, 1)), thr: i64::MAX, room: 4096 },
        Case { cap: 4096, route: Some((2, 2)), thr: 2, room: 4096 },
    ]
}

#[test]
fn every_layout_compiles_and_folds_as_the_loops_and_the_generic_fold() {
    let (mut bytes, mut us) = (Vec::new(), Vec::new());
    for (pool, seed, count) in [(PLAIN_KINDS, 0xd01u64, 120usize), (VALID_KINDS, 0x9e3, 80), (NN_KINDS, 0x51, 40)] {
        for layout in layouts(count, seed, pool) {
            let w = what(&layout);
            let t0 = std::time::Instant::now();
            let c = compiled(&layout);
            us.push(t0.elapsed().as_micros());
            bytes.push(c.bytes);
            for (n, domain) in [(500usize, 50u64), (3000, 700), (2000, 1 << 40)] {
                let recs = records(&layout, n, domain, seed ^ n as u64 ^ layout.size() as u64, false);
                for case in cases(n) {
                    let v = fold_three(c.entry, &layout, &recs, n, case, &w);
                    if case.cap == 8192 {
                        assert_eq!(v, n as i64, "{w} {case:?}: every record folded");
                    }
                }
            }
        }
    }
    bytes.sort_unstable();
    us.sort_unstable();
    let max = *bytes.last().expect("a fold");
    eprintln!("compiled folds: {} layouts, code bytes min {} median {} max {}; compile us median {} p90 {} max {}", bytes.len(), bytes[0], bytes[bytes.len() / 2], max, us[us.len() / 2], us[us.len() * 9 / 10], us[us.len() - 1]);
    assert!(max < 64 << 10, "a compiled fold's code stays small: {max} bytes");
}

#[test]
fn a_float_sum_that_overflows_refuses() {
    for validity in [false, true] {
        let layout = RecordLayout::with_sentinel(&[KeyKind::I64], &[Count, SumF64, SumI64], validity, Some(0));
        let c = compiled(&layout);
        let recs = records(&layout, 64, 4, 3, true);
        let v = fold_three(c.entry, &layout, &recs, 64, cases(64)[0], &what(&layout));
        assert_eq!(v, FOLD_OVERFLOW, "{}", what(&layout));
    }
}

#[test]
fn the_compiled_fold_continues_a_table_the_loops_began() {
    for layout in layouts(12, 0x77, VALID_KINDS) {
        let c = compiled(&layout);
        let f = Fold::of(&layout).expect("a generated fold");
        let (n, rec) = (4000usize, layout.size() as usize);
        let recs = records(&layout, n, 900, 0x77 ^ rec as u64, false);
        let (mut a, ta) = Table::open(&layout, 4096, None, 4096);
        let (mut b, tb) = Table::open(&layout, 4096, None, 4096);
        let mut ca = unsafe { Candidates::at(a.cand.as_mut_ptr() as *mut u8, 4096, layout.sentinel_off() as usize, 4) };
        let mut cb = unsafe { Candidates::at(b.cand.as_mut_ptr() as *mut u8, 4096, layout.sentinel_off() as usize, 4) };
        for (k, chunk) in recs.chunks(rec * 333).enumerate() {
            let m = chunk.len() / rec;
            let va = unsafe {
                if k % 2 == 0 {
                    f.run(ta, chunk.as_ptr(), m, SubSplit::NONE, &mut ca)
                } else {
                    fold_jit::run(c.entry, ta, chunk.as_ptr(), m, &mut ca)
                }
            };
            let vb = unsafe { f.run(tb, chunk.as_ptr(), m, SubSplit::NONE, &mut cb) };
            assert_eq!(va, vb);
        }
        assert_eq!((ca.len, ca.overflow), (cb.len, cb.overflow));
        assert!(a.region == b.region && a.cand == b.cand, "{}", what(&layout));
    }
}
