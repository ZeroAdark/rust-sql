//! The record-write face (milestone-2.md §2.6 item 11): `rec_face`'s offsets agree with `Rec`'s
//! own accessors on every record shape the kernels use, and `rec_open` hands out the record
//! `HashAgg` would (opened once, found again), writes at the face's offsets landing in `Rec`'s view.

use gravity_abi::breaker::{rec_face, rec_open, HashAgg, KeyCell, KeyKind, Rec, RecordLayout, SlotKind, HASHAGG_HDR, MOVING, NO_VALIDITY};
use gravity_abi::ctx::{ctx_size, Ctx, Partials};
use gravity_abi::program::ProgramBlock;

/// The record layouts the kernels build: a scan count, scalar sums, a scalar min/max, a count by an int key, the
/// parallel hash agg (word and code keys), the distinct pass and its `LAYOUT2`, a 128-bit sum, an avg by an int8 key, a
/// partitioned two-fingerprint key with min codes, a float sum and wide numerics.
fn kernel_layouts() -> Vec<(&'static str, RecordLayout)> {
    vec![
        ("par_scan_count", RecordLayout::new(&[], &[SlotKind::Count])),
        ("scalar_sums", RecordLayout::new(&[], &[SlotKind::SumI64, SlotKind::Count, SlotKind::SumI128, SlotKind::Count])),
        ("scalar_min_max", RecordLayout::new(&[], &[SlotKind::MinI64, SlotKind::MaxI64])),
        ("count_by_i32", RecordLayout::new(&[KeyKind::I32], &[SlotKind::Count])),
        ("par_hashagg", RecordLayout::new(&[KeyKind::I32], &[SlotKind::SumI64, SlotKind::Count])),
        ("par_hashagg_codes", RecordLayout::new(&[KeyKind::Code], &[SlotKind::Count])),
        ("par_distinct", RecordLayout::new(&[KeyKind::I32, KeyKind::I32], &[])),
        ("par_distinct LAYOUT2", RecordLayout::new_nn(&[KeyKind::I32], &[SlotKind::Count])),
        ("scalar_sum128", RecordLayout::new(&[], &[SlotKind::SumI128, SlotKind::Count])),
        ("avg_by_i64", RecordLayout::new(&[KeyKind::I64], &[SlotKind::AvgI128, SlotKind::Count])),
        ("fp_pair_min_codes", RecordLayout::new_nn(&[KeyKind::Fp128, KeyKind::Fp128, KeyKind::I32], &[SlotKind::Count, SlotKind::MinCode, SlotKind::MinCode]).partitioned_by(2)),
        ("sum_f64", RecordLayout::new(&[KeyKind::I64], &[SlotKind::SumF64, SlotKind::MaxCode])),
        ("num_wide", RecordLayout::new(&[KeyKind::Fp128], &[SlotKind::SumNumWide, SlotKind::AvgNumWide, SlotKind::SumNum, SlotKind::Count])),
    ]
}

fn word(buf: &[u8], off: u32) -> u64 {
    u64::from_le_bytes(buf[off as usize..off as usize + 8].try_into().unwrap())
}

#[test]
fn the_face_offsets_agree_with_the_record_accessors_on_every_kernel_layout() {
    for (name, l) in kernel_layouts() {
        let f = rec_face(&l);
        assert_eq!(f.base_off as usize, HASHAGG_HDR, "{name}");
        assert_eq!(f.size, l.size(), "{name}");
        assert_eq!(f.nslots as usize, l.nslots(), "{name}");
        assert_eq!(f.sentinel_off as usize, l.sentinel_off as usize, "{name}");
        assert_eq!(f.moving_bit, MOVING, "{name}");
        assert_eq!(f.valid_off == NO_VALIDITY, !l.has_validity(), "{name}");
        let mut buf = vec![0u8; l.size() as usize];
        // SAFETY: `size()` zeroed bytes, 8-aligned (a Vec<u8> of u64-multiple length from the allocator).
        let r = unsafe { Rec::at(buf.as_mut_ptr(), &l) };
        let keys: Vec<u64> = (0..l.nkeys()).map(|k| 0x1000 + k as u64).collect();
        assert!(!r.used(), "{name}: a zeroed record is empty");
        r.open(&keys);
        for (k, &w) in keys.iter().enumerate() {
            assert_eq!(word(&buf, 8 * k as u32), w, "{name}: key {k} at the key words");
        }
        for j in 0..l.nslots() {
            let off = f.slot_off[j];
            assert_eq!(off as usize, l.slot_off[j] as usize, "{name} slot {j}");
            match l.slots[j] {
                SlotKind::Count => {
                    r.add_count(j, 5);
                    assert_eq!(word(&buf, off), 5 + (l.flags & gravity_abi::breaker::LAYOUT_IMPLICIT_COUNT != 0 && j + 1 == l.nslots()) as u64, "{name} slot {j}: the count word");
                }
                SlotKind::SumI64 => {
                    r.add_i64(j, -7);
                    assert_eq!(word(&buf, off) as i64, -7, "{name} slot {j}");
                }
                SlotKind::SumI128 => {
                    r.add_i128(j, -(1i128 << 70));
                    assert_eq!(((word(&buf, off + 8) as u128) << 64 | word(&buf, off) as u128) as i128, -(1i128 << 70), "{name} slot {j}");
                }
                SlotKind::MinI64 | SlotKind::MaxI64 => {
                    if matches!(l.slots[j], SlotKind::MinI64) { r.min_i64(j, -3) } else { r.max_i64(j, -3) }
                    assert_eq!(word(&buf, off) as i64, -3, "{name} slot {j}");
                }
                SlotKind::IntervalMin | SlotKind::IntervalMax => {
                    r.fold_interval(j, (-3, 2, -1), matches!(l.slots[j], SlotKind::IntervalMin));
                    assert_eq!((word(&buf, off) as i64, word(&buf, off + 8)), (-3, 2 | (u32::MAX as u64) << 32), "{name} slot {j}: time, day | month << 32");
                }
                SlotKind::IntervalSum => {
                    let mut v = gravity_abi::breaker::IntervalSum::default();
                    assert!(!v.accum(-3, 2, -1), "{name} slot {j}: in range");
                    r.fold_interval_sum(j, &v).expect("unlicensed");
                    let got = (word(&buf, off) as i64, word(&buf, off + 8), word(&buf, off + 16), word(&buf, off + 32), word(&buf, off + 40));
                    assert_eq!(got, (-3, 2 | (u32::MAX as u64) << 32, 3, 1, 1), "{name} slot {j}: time, day | month << 32, |time|, |month|, n");
                }
                SlotKind::AvgI128 => {
                    r.add_avg(j, 9, 2);
                    assert_eq!((word(&buf, off), word(&buf, off + 16)), (9, 2), "{name} slot {j}");
                }
                SlotKind::SumF64 => {
                    r.add_f64(j, 1.5).expect("finite");
                    assert_eq!(f64::from_bits(word(&buf, off)), 1.5, "{name} slot {j}");
                }
                SlotKind::SumF32 => {
                    r.add_f32(j, 1.5).expect("finite");
                    assert_eq!(f64::from_bits(word(&buf, off)), 1.5, "{name} slot {j}: float4's value as f64");
                }
                SlotKind::BitAnd | SlotKind::BitOr | SlotKind::BitXor => {
                    r.fold_bits(j, 0b1010);
                    assert_eq!(word(&buf, off), 0b1010, "{name} slot {j}: the first value copied or combined with the zeroed word");
                }
                SlotKind::F64Accum => {
                    r.combine_f64_accum(j, [2.0, 3.0, 0.5]).expect("finite");
                    assert_eq!([0, 8, 16].map(|k| f64::from_bits(word(&buf, off + k))), [2.0, 3.0, 0.5], "{name} slot {j}: {{N, Sx, Sxx}}");
                }
                SlotKind::MinCode => {
                    r.min_code(j, 4, 9);
                    assert_eq!(r.code(j), (4, 9), "{name} slot {j}");
                    assert_eq!(word(&buf, off), KeyCell::code_word(4, 9), "{name} slot {j}: the code word");
                }
                SlotKind::MaxCode => {
                    r.max_code(j, 4, 9);
                    assert_eq!(word(&buf, off), KeyCell::code_word(4, 9), "{name} slot {j}");
                }
                SlotKind::SumNum => {
                    r.add_i128(j, -(1i128 << 70));
                    assert_eq!(((word(&buf, off + 8) as u128) << 64 | word(&buf, off) as u128) as i128, -(1i128 << 70), "{name} slot {j}");
                }
                SlotKind::AvgNum => {
                    r.add_avg(j, 9, 2);
                    assert_eq!((word(&buf, off), word(&buf, off + 16)), (9, 2), "{name} slot {j}");
                }
                SlotKind::SumNumWide | SlotKind::AvgNumWide => {
                    let v = [5, 0, u64::MAX, 7];
                    if matches!(l.slots[j], SlotKind::SumNumWide) { r.add_num256(j, v) } else { r.add_avg_num256(j, v, 2) }
                    assert_eq!([0, 8, 16, 24].map(|k| word(&buf, off + k)), v, "{name} slot {j}: the i256 words");
                }
                SlotKind::MinVar | SlotKind::MaxVar | SlotKind::AggState | SlotKind::NumSideMin | SlotKind::NumSideMax => continue,
            }
            assert!(r.valid(j), "{name} slot {j}: marked");
            if f.valid_off != NO_VALIDITY {
                assert_eq!(buf[f.valid_off as usize] >> j & 1, 1, "{name} slot {j}: the validity bit");
            }
        }
        assert!(r.used(), "{name}: the sentinel");
        assert_ne!(word(&buf, f.sentinel_off), 0, "{name}: the sentinel word");
        if f.valid_off != NO_VALIDITY {
            assert!(!r.null_seen());
            buf[f.valid_off as usize + 1] = 1;
            assert!(r.null_seen(), "{name}: the null-seen byte");
        }
        // the moving bit: set on the sentinel, the count reads through it
        let count_slot = (0..l.nslots()).find(|&j| l.slot_off[j] as u32 == f.sentinel_off).unwrap();
        let before = r.count(count_slot);
        let s = f.sentinel_off as usize;
        let w = word(&buf, f.sentinel_off) | f.moving_bit;
        buf[s..s + 8].copy_from_slice(&w.to_le_bytes());
        assert_eq!(r.count(count_slot), before, "{name}: `count` masks the moving bit");
    }
}

/// A bound `Ctx` whose program block's `partial.own` is `region`: what `rec_open` reads.
struct Bound {
    _ctx: Vec<u64>,
    _block: Box<ProgramBlock>,
    _partials: Box<Partials>,
    ctx: *mut Ctx,
}

fn bind(region: *mut u8) -> Bound {
    let mut ctx = vec![0u64; ctx_size(1) / 8];
    let mut partials = Box::new(Partials::two(region, [core::ptr::null_mut(); 2]));
    let mut block = Box::new(ProgramBlock { partial: &mut *partials as *mut Partials, rec_open: Some(rec_open), ..ProgramBlock::NONE });
    let p = ctx.as_mut_ptr() as *mut Ctx;
    // SAFETY: a zeroed header of `ctx_size(1)` bytes.
    unsafe { (*p).program = &mut *block as *mut ProgramBlock };
    Bound { _ctx: ctx, _block: block, _partials: partials, ctx: p }
}

#[test]
fn rec_open_hands_out_the_table_record_and_face_writes_land_in_the_record() {
    const L: RecordLayout = RecordLayout::new(&[KeyKind::I32], &[SlotKind::Count]);
    const CAP: u32 = 16;
    let bytes = HashAgg::bytes(&L, CAP);
    let mut region = vec![0u64; bytes as usize / 8];
    let b = bind(region.as_mut_ptr() as *mut u8);
    let f = rec_face(&L);
    let ctx = b.ctx;
    // SAFETY: the bound ctx; the region is zeroed and `bytes` long.
    unsafe {
        let host = (*(*ctx).program).rec_open.unwrap();
        let keys = [7u64];
        let p = host(ctx, 0, &L, CAP, bytes, keys.as_ptr());
        assert_ne!(p, 0);
        let t = HashAgg::view(region.as_ptr() as *const u8);
        assert_eq!(t.len, 1);
        let base = region.as_ptr() as usize;
        assert!((p as usize) >= base + f.base_off as usize && (p as usize) < base + bytes as usize);
        assert_eq!((p as usize - base - f.base_off as usize) % f.size as usize, 0, "a record boundary");
        // the row's write follows the open before any other table operation (the count is the sentinel)
        let rec = p as *mut u8;
        *(rec.add(f.slot_off[0] as usize) as *mut u64) += 3;
        *rec.add(f.valid_off as usize) |= 1;
        let again = rec_open(ctx, 0, &L, CAP, bytes, keys.as_ptr());
        assert_eq!(p, again, "found again, not opened twice");
        assert_eq!(t.len, 1);
        let r = t.find(&[7]).expect("the record");
        assert_eq!(r.as_ptr() as i64, p);
        assert_eq!(r.count(0), 3);
        assert!(r.valid(0) && r.used());
        let other = [8u64];
        let q = rec_open(ctx, 0, &L, CAP, bytes, other.as_ptr());
        assert_ne!(q, 0);
        assert_ne!(q, p);
        *((q as *mut u8).add(f.slot_off[0] as usize) as *mut u64) = 1;
        assert_eq!(t.len, 2);
        // a keyless layout: the single record behind the header
        const S: RecordLayout = RecordLayout::new(&[], &[SlotKind::MinI64, SlotKind::MaxI64]);
        let sbytes = HashAgg::bytes(&S, 1);
        let mut single = vec![0u64; sbytes as usize / 8];
        let b2 = bind(single.as_mut_ptr() as *mut u8);
        let sp = rec_open(b2.ctx, 0, &S, 1, sbytes, core::ptr::null());
        assert_eq!(sp as usize, single.as_ptr() as usize + rec_face(&S).base_off as usize);
        assert_eq!(rec_open(b2.ctx, 0, &S, 1, sbytes, core::ptr::null()), sp);
        // a full table refuses
        for k in 9..(9 + CAP as u64 * 2) {
            let kk = [k];
            let r = rec_open(ctx, 0, &L, CAP, bytes, kk.as_ptr());
            if r == 0 {
                return;
            }
            *((r as *mut u8).add(f.slot_off[0] as usize) as *mut u64) = 1;
        }
        panic!("a {CAP}-record table under `bytes` never refused");
    }
}

/// Interval kinds: `interval_smaller / larger(a, b)` keep `b` on a tie, so a slot takes an equal later row ('1 day' then
/// '24:00:00' keeps '24:00:00'; '1 mon' then '30 days' keeps '30 days'), keeps a strictly better value, and a record merge
/// takes the later part's (`src`) member on a tie; the infinities order at the ends.
#[test]
fn interval_extremes_keep_the_later_of_equal_members() {
    let l = RecordLayout::new(&[], &[SlotKind::IntervalMin, SlotKind::IntervalMax]);
    let (mut a, mut b) = (vec![0u8; l.size() as usize], vec![0u8; l.size() as usize]);
    // SAFETY: `size()` zeroed bytes each, 8-aligned.
    let (ra, rb) = unsafe { (Rec::at(a.as_mut_ptr(), &l), Rec::at(b.as_mut_ptr(), &l)) };
    ra.open(&[]);
    rb.open(&[]);
    let (day, h24, mon, d30) = ((0i64, 1i32, 0i32), (86_400_000_000i64, 0, 0), (0i64, 0, 1), (0i64, 30, 0));
    let (two_days, ninf, pinf) = ((0i64, 2, 0), (i64::MIN, i32::MIN, i32::MIN), (i64::MAX, i32::MAX, i32::MAX));
    ra.fold_interval(0, day, true);
    ra.fold_interval(0, h24, true);
    assert_eq!(ra.interval(0), h24, "min: the later of equal rows");
    ra.fold_interval(0, two_days, true);
    assert_eq!(ra.interval(0), h24, "min: a larger row leaves the state");
    ra.fold_interval(1, mon, false);
    ra.fold_interval(1, d30, false);
    assert_eq!(ra.interval(1), d30, "max: the later of equal rows");
    ra.fold_interval(1, day, false);
    assert_eq!(ra.interval(1), d30, "max: a smaller row leaves the state");
    rb.fold_interval(0, day, true);
    rb.fold_interval(1, mon, false);
    ra.fold(&rb).expect("no float slot");
    assert_eq!((ra.interval(0), ra.interval(1)), (day, mon), "merge: the later part's member on a tie");
    rb.fold_interval(0, ninf, true);
    rb.fold_interval(1, pinf, false);
    ra.fold(&rb).expect("no float slot");
    assert_eq!((ra.interval(0), ra.interval(1)), (ninf, pinf), "the infinities at the ends");
    assert_eq!(gravity_abi::breaker::interval_cmp_value(0, 1, 0), gravity_abi::breaker::interval_cmp_value(86_400_000_000, 0, 0));
}

/// `IntervalSum::compose`, the one merge of `sum / avg(interval)` states, against PG's sequential fold: random sequences of
/// intervals near the component edges (and small ones), cut into parts at random and composed in order. (1) The composed
/// state equals the one-part fold: the counts, the absolute sums, and the sums wherever PG's sequential fold does not raise.
/// (2) Whenever PG's sequential fold raises (`do_interval_accum`: a component past its range or an infinity image), the
/// composed state may overflow, so a licensed fold raises. (3) A state that may not overflow overflows in no order (each
/// sequence also folded reversed and rotated).
#[test]
fn interval_sum_compose_matches_the_sequential_fold() {
    use gravity_abi::breaker::{IntervalSum, IVSUM_LICENSED};
    let mut seed = 0x1d2c_3b4a_5968_7788u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for case in 0..4000 {
        let len = 1 + (next() % 9) as usize;
        let edge = case % 2 == 0;
        let vals: Vec<(i64, i32, i32)> = (0..len)
            .map(|_| {
                let r = next();
                let big = |r: u64, max: u64| -> u64 { max - (r % 3) };
                let sign = |r: u64, v: u64| -> i128 { if r & 1 == 0 { v as i128 } else { -(v as i128) } };
                if r % 11 == 0 {
                    return if r & 1 == 0 { (i64::MAX, i32::MAX, i32::MAX) } else { (i64::MIN, i32::MIN, i32::MIN) };
                }
                if edge {
                    let t = if r >> 8 & 3 == 0 { sign(r >> 10, big(r >> 12, i64::MAX as u64 / 2 + 1)) } else { sign(r >> 10, r >> 20 & 0xffff) };
                    let d = if r >> 30 & 3 == 0 { sign(r >> 32, big(r >> 34, i32::MAX as u64 / 2 + 1)) } else { sign(r >> 32, r >> 40 & 0xff) };
                    let m = if r >> 50 & 3 == 0 { sign(r >> 52, big(r >> 54, i32::MAX as u64 / 2 + 1)) } else { sign(r >> 52, r >> 58 & 0xf) };
                    (t as i64, d as i32, m as i32)
                } else {
                    ((r & 0xffff) as i64 - 0x8000, (r >> 16 & 0xff) as i32 - 0x80, (r >> 24 & 0xf) as i32 - 8)
                }
            })
            .collect();
        // PG's sequential fold over `seq`: raises?
        let pg_raises = |seq: &[(i64, i32, i32)]| {
            let mut s = IntervalSum::default();
            seq.iter().any(|&(t, d, m)| s.accum(t, d, m))
        };
        let mut one = IntervalSum { flags: IVSUM_LICENSED, ..IntervalSum::default() };
        let raised = vals.iter().fold(false, |r, &(t, d, m)| one.accum(t, d, m) || r);
        assert_eq!(raised, pg_raises(&vals), "case {case}");
        // the parts, composed in order
        let mut cuts: Vec<usize> = (0..len).filter(|_| next() % 3 == 0).collect();
        cuts.push(len);
        let (mut acc, mut from, mut flagged) = (IntervalSum { flags: IVSUM_LICENSED, ..IntervalSum::default() }, 0, false);
        for &c in &cuts {
            let mut part = IntervalSum { flags: IVSUM_LICENSED, ..IntervalSum::default() };
            for &(t, d, m) in &vals[from..c] {
                part.accum(t, d, m);
            }
            flagged |= acc.compose(&part);
            from = c;
        }
        assert_eq!((acc.n, acc.pinf, acc.ninf, acc.abs_time, acc.abs_day, acc.abs_month), (one.n, one.pinf, one.ninf, one.abs_time, one.abs_day, one.abs_month), "case {case}: counts and absolute sums");
        if !raised {
            assert_eq!((acc.time, acc.day, acc.month), (one.time, one.day, one.month), "case {case}: the sums");
        }
        if raised {
            assert!(acc.may_overflow() && flagged, "case {case}: PG raises, so the licensed compose raises");
        }
        if !acc.may_overflow() {
            let mut rev = vals.clone();
            rev.reverse();
            let rot: Vec<_> = vals.iter().cycle().skip(len / 2).take(len).copied().collect();
            assert!(!raised && !pg_raises(&rev) && !pg_raises(&rot), "case {case}: no order overflows below the bounds");
        }
    }
}
