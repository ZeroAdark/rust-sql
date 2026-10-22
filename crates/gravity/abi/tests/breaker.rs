use gravity_abi::hash::{Depth, KeyHash};
use gravity_abi::breaker::{float4_pl, float8_combine, float8_pl, FloatOverflow, F64_ACCUM_TAINT, first_count, fits_inline_at, AggSlot, Breaker, BreakerKind, Budget, HashAgg, KeyCell, KeyKind, Rec, RecordLayout, SlotKind, Spill, DENSE, HASHAGG_HDR, HASHED, LAYOUT_IMPLICIT_COUNT, MAX_RECORD_KEYS, MAX_RECORD_SLOTS, RECORD_HDR};

/// Eight of the eleven slot kinds (a record holds at most eight); `MaxCode` is tested beside `MinCode`, `SumNum` / `AvgNum` in `numeric_slots`.
const ALL_SLOTS: [SlotKind; 8] = [SlotKind::Count, SlotKind::SumI64, SlotKind::SumI128, SlotKind::MinI64, SlotKind::MaxI64, SlotKind::AvgI128, SlotKind::SumF64, SlotKind::MinCode];
const NINE: [SlotKind; 9] = [SlotKind::Count, SlotKind::SumI64, SlotKind::SumI128, SlotKind::MinI64, SlotKind::MaxI64, SlotKind::AvgI128, SlotKind::SumF64, SlotKind::MinCode, SlotKind::MaxCode];
const ALL_KEYS: [KeyKind; 6] = [KeyKind::I32, KeyKind::I64, KeyKind::Code, KeyKind::Date, KeyKind::Timestamp, KeyKind::Fp128];

fn table(_layout: &RecordLayout, _cap: u32, limit: u32) -> Vec<u64> {
    vec![0u64; limit.div_ceil(8) as usize]
}

fn at<'a>(buf: &'a mut [u64], layout: &RecordLayout, cap: u32, limit: u32) -> &'a mut HashAgg {
    unsafe { HashAgg::at(buf.as_mut_ptr() as *mut u8, layout, cap, limit) }
}

#[test]
fn layouts_size_and_place_their_cells() {
    assert_eq!((MAX_RECORD_KEYS, MAX_RECORD_SLOTS, RECORD_HDR, HASHAGG_HDR, core::mem::size_of::<RecordLayout>()), (8, 8, 0, 56, 40));
    let empty = RecordLayout::new(&[], &[]);
    assert_eq!((empty.size(), empty.slot_kinds()), (16, &[SlotKind::Count][..]), "the implicit count sentinel and the validity word");
    let count = RecordLayout::new(&[], &[SlotKind::Count]);
    assert_eq!((count.size(), count.slot_off[0], count.sentinel_off, count.valid_off), (16, 0, 0, 8));
    let group = RecordLayout::new(&[KeyKind::I32], &[SlotKind::SumI64, SlotKind::Count]);
    assert_eq!((group.size(), group.key_off(0), group.slot_off[0], group.slot_off[1], group.sentinel_off, group.valid_off), (32, 0, 8, 16, 16, 24));
    let nn = RecordLayout::new_nn(&[KeyKind::I64, KeyKind::Fp128, KeyKind::Fp128], &[SlotKind::Count, SlotKind::MinCode]);
    assert_eq!((nn.size(), nn.has_validity(), nn.sentinel_off), (40, false, 24), "a three-key distinct record: 24 B of keys, count, representative");
    let wide = RecordLayout::new(&ALL_KEYS, &[SlotKind::Count, SlotKind::SumI128, SlotKind::AvgI128, SlotKind::SumF64, SlotKind::MinCode, SlotKind::MaxI64, SlotKind::MinI64, SlotKind::MaxCode]);
    assert_eq!(wide.size(), 6 * 8 + 8 + 16 + 24 + 8 + 8 + 8 + 8 + 8 + 8, "every kind once plus the validity word: 144 B");
    assert_eq!(&wide.slot_off[..8], &[48, 56, 72, 96, 104, 112, 120, 128]);
    for s in NINE {
        let l = RecordLayout::new(&[], &[s]);
        let implicit = if s == SlotKind::Count { 0 } else { 8 };
        assert_eq!(l.size(), s.bytes() as u32 + implicit + 8, "{s:?}");
        assert_eq!(l.slot_kinds()[0], s);
    }
    assert_eq!(HashAgg::bytes(&group, 4096), HASHAGG_HDR as u32 + 4096 * 32);
    assert_eq!(HashAgg::cap_of(&group, HashAgg::bytes(&group, 4096)), 4096);
    assert_eq!(HashAgg::cap_of(&group, HashAgg::bytes(&group, 4096) - 1), 2048);
    assert_eq!(HashAgg::cap_of(&group, 10), 0);
    let b = Breaker::sized(BreakerKind::HashAgg, 100).in_slot(2);
    assert_eq!(b, Breaker { kind: BreakerKind::HashAgg, slot: 2, bytes: 100, budget: Budget::Fixed(100), spill: Spill::None });
}

#[test]
fn the_sentinel_is_a_row_count_or_the_implicit_one() {
    // avg(v), count(f): count(f) is a Count slot but no row count (f nullable) — the implicit sentinel follows it
    let slots = [SlotKind::AvgI128, SlotKind::Count];
    let named = RecordLayout::with_sentinel(&[KeyKind::I32], &slots, true, Some(1));
    assert_eq!((named.nslots(), named.sentinel_off, named.flags & LAYOUT_IMPLICIT_COUNT, named.size()), (2, 32, 0, 48));
    let implicit = RecordLayout::with_sentinel(&[KeyKind::I32], &slots, true, None);
    assert_eq!((implicit.nslots(), implicit.sentinel_off, implicit.flags & LAYOUT_IMPLICIT_COUNT, implicit.size()), (3, 40, LAYOUT_IMPLICIT_COUNT, 56));
    assert_eq!((implicit.sentinel_slot(), implicit.slot_kinds()), (2, &[SlotKind::AvgI128, SlotKind::Count, SlotKind::Count][..]));
    assert!(fits_inline_at(&[KeyKind::I32], &slots, true, None) && !fits_inline_at(&[KeyKind::I32], &[SlotKind::Count; 8], true, None));
    // the implicit sentinel opens at 1 where the named count(f) slot stays 0 until a non-null f
    let mut buf = vec![0u64; implicit.size() as usize / 8];
    let r = unsafe { Rec::at(buf.as_mut_ptr() as *mut u8, &implicit) };
    r.open(&[7]);
    assert!(r.used());
    let mut buf2 = vec![0u64; named.size() as usize / 8];
    let r2 = unsafe { Rec::at(buf2.as_mut_ptr() as *mut u8, &named) };
    r2.open(&[7]);
    assert!(!r2.used(), "a nullable count as the sentinel reads as empty after open");
    // rekeyed keeps the sentinel slot and the validity, the count-first rule of `new` is unchanged
    let rk = implicit.rekeyed(&[KeyKind::I64, KeyKind::I64]);
    assert_eq!((rk.nslots(), rk.sentinel_slot(), rk.sentinel_off, rk.has_validity()), (3, 2, 48, true));
    assert_eq!(RecordLayout::new(&[], &slots).sentinel_off, 24);
    assert_eq!(first_count(&slots), Some(1));
}

#[test]
fn key_cells_round_trip_through_one_word() {
    let cells = [KeyCell::I32(-7), KeyCell::I64(i64::MIN + 3), KeyCell::Code { part: 9, code: 0xFFFF_FFFF }, KeyCell::Date(-1), KeyCell::Timestamp(1_700_000_000_000_000), KeyCell::Fp128(0xDEAD_BEEF_0000_0001)];
    for (c, k) in cells.iter().zip(ALL_KEYS) {
        assert_eq!(KeyCell::from_word(k, c.word()), *c);
    }
    assert_eq!(KeyCell::I32(-7).word(), (-7i64) as u64, "int keys are sign-extended, so `as i64` orders them");
    assert_eq!(KeyCell::code_word(9, 5), 9 << 32 | 5);
}

#[test]
fn hashing_covers_the_whole_key_tuple() {
    assert_eq!(KeyHash::of(&[1, 2]), KeyHash::of(&[1, 2]));
    assert_ne!(KeyHash::of(&[1, 2]), KeyHash::of(&[2, 1]));
    assert_ne!(KeyHash::of(&[1]), KeyHash::of(&[1, 0]));
    assert_ne!(KeyHash::of(&[0]), KeyHash::of(&[]));
    let mut seen = std::collections::HashSet::new();
    for k in 0..10_000u64 {
        seen.insert(KeyHash::of(&[k]).home(12, Depth::TOP));
    }
    assert!(seen.len() > 3500, "the homes spread sequential keys like a random hash would (~3740 of 4096 buckets): {}", seen.len());
}

#[test]
fn records_accumulate_per_slot_kind_under_valid_flags() {
    let l = RecordLayout::new(&[KeyKind::I64], &ALL_SLOTS);
    let mut buf = vec![0u64; l.size() as usize / 8];
    let r = unsafe { Rec::at(buf.as_mut_ptr() as *mut u8, &l) };
    assert!(!r.used());
    r.open(&[42]);
    assert!(!r.used(), "opened, not yet counted: the row's fold makes it live");
    r.add_count(0, 0);
    assert!(!r.used());
    r.add_count(0, 1);
    assert!(r.used() && r.keys() == [42] && r.key_cell(0) == KeyCell::I64(42) && r.hash() == KeyHash::of(&[42]));
    assert!(r.slot(0) == Some(AggSlot::Count(1)) && (1..8).all(|j| r.slot(j).is_none()), "every other slot starts invalid");
    r.add_count(0, 2);
    r.add_i64(1, -5);
    r.add_i128(2, i128::from(i64::MAX) * 4);
    r.min_i64(3, 10);
    r.min_i64(3, 4);
    r.min_i64(3, 9);
    r.max_i64(4, -3);
    r.max_i64(4, -1);
    r.max_i64(4, -2);
    r.add_avg(5, 7, 2);
    r.add_avg(5, 3, 1);
    r.add_f64(6, 1.5).expect("finite");
    r.add_f64(6, 2.25).expect("finite");
    r.min_code(7, 2, 5);
    r.min_code(7, 1, 9);
    r.min_code(7, 1, 10);
    let got: Vec<AggSlot> = (0..8).map(|j| r.slot(j).unwrap()).collect();
    assert_eq!(got, vec![AggSlot::Count(3), AggSlot::SumI64(-5), AggSlot::SumI128(i128::from(i64::MAX) * 4), AggSlot::MinI64(4), AggSlot::MaxI64(-1), AggSlot::AvgI128 { sum: 10, n: 3 }, AggSlot::SumF64(3.75), AggSlot::MinCode { part: 1, code: 9 }]);
    assert_eq!(r.valid_bits(), 0xFF);
    assert!(!r.null_seen());
    r.set_null_seen();
    assert!(r.null_seen());
    // a min over an all-negative input on an invalid slot takes the first value, not zero
    let mut other = vec![0u64; l.size() as usize / 8];
    let o = unsafe { Rec::at(other.as_mut_ptr() as *mut u8, &l) };
    o.open(&[42]);
    o.min_i64(3, -100);
    o.max_i64(4, -100);
    assert_eq!((o.slot(3), o.slot(4)), (Some(AggSlot::MinI64(-100)), Some(AggSlot::MaxI64(-100))));
    let mc = RecordLayout::new(&[], &[SlotKind::MaxCode, SlotKind::MinCode]);
    let mut m = vec![0u64; mc.size() as usize / 8];
    let rm = unsafe { Rec::at(m.as_mut_ptr() as *mut u8, &mc) };
    rm.open(&[]);
    rm.max_code(0, 1, 9);
    rm.max_code(0, 2, 0);
    rm.max_code(0, 1, 12);
    assert_eq!((rm.slot(0), rm.slot(1)), (Some(AggSlot::MaxCode { part: 2, code: 0 }), None), "codes order by (part, code)");
    r.clear();
    assert!(!r.used() && r.valid_bits() == 0);
}

#[test]
fn folding_combines_valid_slots_only_and_ors_null_seen() {
    let l = RecordLayout::new(&[], &ALL_SLOTS);
    let mut a = vec![0u64; l.size() as usize / 8];
    let mut b = vec![0u64; l.size() as usize / 8];
    let (ra, rb) = unsafe { (Rec::at(a.as_mut_ptr() as *mut u8, &l), Rec::at(b.as_mut_ptr() as *mut u8, &l)) };
    ra.open(&[]);
    rb.open(&[]);
    ra.add_count(0, 1);
    ra.min_i64(3, 5);
    ra.add_f64(6, 0.5).expect("finite");
    rb.add_count(0, 2);
    rb.add_i64(1, 8);
    rb.max_i64(4, 9);
    rb.min_i64(3, 7);
    rb.add_avg(5, 4, 4);
    rb.min_code(7, 3, 3);
    rb.set_null_seen();
    ra.fold(&rb).expect("no float overflow");
    assert_eq!((0..8).map(|j| ra.slot(j)).collect::<Vec<_>>(), vec![Some(AggSlot::Count(3)), Some(AggSlot::SumI64(8)), None, Some(AggSlot::MinI64(5)), Some(AggSlot::MaxI64(9)), Some(AggSlot::AvgI128 { sum: 4, n: 4 }), Some(AggSlot::SumF64(0.5)), Some(AggSlot::MinCode { part: 3, code: 3 })]);
    let mc = RecordLayout::new(&[], &[SlotKind::MaxCode]);
    assert_eq!(mc.size(), 24, "the code word, the implicit count, the validity word");
    let mut x = vec![0u64; 3];
    let mut y = vec![0u64; 3];
    let (rx, ry) = unsafe { (Rec::at(x.as_mut_ptr() as *mut u8, &mc), Rec::at(y.as_mut_ptr() as *mut u8, &mc)) };
    rx.open(&[]);
    ry.open(&[]);
    rx.max_code(0, 1, 5);
    ry.max_code(0, 1, 6);
    rx.fold(&ry).expect("no float overflow");
    assert_eq!(rx.slot(0), Some(AggSlot::MaxCode { part: 1, code: 6 }));
    assert!(ra.null_seen(), "null_seen ORs");
    assert_eq!(rb.slot(0), Some(AggSlot::Count(2)));
}

#[test]
fn hash_aggregate_grows_in_place_within_its_limit_and_refuses_past_it() {
    let l = RecordLayout::new(&[KeyKind::I32, KeyKind::Code], &[SlotKind::SumI64, SlotKind::Count]);
    let limit = HashAgg::bytes(&l, 64);
    let mut buf = table(&l, 4, limit);
    let t = at(&mut buf, &l, 4, limit);
    assert_eq!((t.cap, t.len, t.mode, t.limit), (4, 0, HASHED, limit));
    let key = |k: i32| [KeyCell::I32(k).word(), KeyCell::code_word(1, k as u32 % 3)];
    for round in 0..3 {
        for k in 0..50 {
            let r = t.find_or_insert(&key(k)).expect("room under the limit");
            r.add_i64(0, k as i64 * 10);
            r.add_count(1, 1);
        }
        assert_eq!(t.len, 50, "round {round}");
    }
    assert_eq!(t.cap, 64, "4 → 8 → 16 → 32 → 64 while 7/8 full");
    for k in 0..50 {
        let r = t.find(&key(k)).expect("every key found after growth");
        assert_eq!((r.i64(0), r.count(1), r.keys()), (k as i64 * 30, 3, &key(k)[..]));
    }
    assert!(t.find(&key(99)).is_none());
    assert_eq!(t.records().count(), 50);
    for k in 50..64 {
        t.find_or_insert(&key(k)).expect("the last capacity fills without growth").add_count(1, 1);
    }
    assert!(t.find_or_insert(&key(1000)).is_none(), "full at the limit: the insert refuses (the spill's hook)");
    assert_eq!((t.cap, t.len), (64, 64));
    assert!(t.find_or_insert(&key(7)).is_some(), "an existing key still folds");
    let again = unsafe { HashAgg::view(buf.as_ptr() as *const u8) };
    assert_eq!((again.cap, again.len, again.layout), (64, 64, l), "a partial describes itself");
    let t = at(&mut buf, &l, 4, limit);
    t.clear();
    assert_eq!((t.cap, t.len, t.records().count()), (64, 0, 0));
}

#[test]
#[should_panic(expected = "hash aggregate shape")]
fn a_live_table_refuses_another_layout() {
    let l = RecordLayout::new(&[KeyKind::I32], &[SlotKind::Count]);
    let m = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count]);
    let limit = HashAgg::bytes(&l, 8);
    let mut buf = table(&l, 8, limit);
    at(&mut buf, &l, 8, limit).find_or_insert(&[1]).unwrap();
    at(&mut buf, &m, 8, limit);
}

#[test]
fn dense_tables_append_sort_and_truncate() {
    let l = RecordLayout::new(&[KeyKind::I32], &[SlotKind::SumI64, SlotKind::Count]);
    let limit = HashAgg::bytes(&l, 32);
    let mut buf = table(&l, 8, limit);
    let t = at(&mut buf, &l, 8, limit);
    let keys = [5i32, -3, 40, 0, 7, -3, 12, 9, 1, 2, 3, 4, 8, 6, 11, 10, 13];
    for (i, k) in keys.iter().enumerate() {
        let r = t.find_or_insert(&[KeyCell::I32(*k).word()]).unwrap();
        r.add_i64(0, i as i64);
        r.add_count(1, 1);
    }
    assert_eq!(t.len, 16);
    t.sort_dense_by_key(0);
    assert_eq!(t.mode, DENSE);
    let ks: Vec<i64> = (0..t.len as usize).map(|i| t.record(i).key(0) as i64).collect();
    let mut want: Vec<i64> = keys.iter().map(|k| *k as i64).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    assert_eq!(ks, want, "sorted by the key as i64, dense from 0");
    assert_eq!(t.record(0).count(1), 2, "the payload travels with the record (-3 twice)");
    assert!((t.len as usize..t.cap as usize).all(|i| !t.record(i).used()));
    t.truncate_dense(3);
    want.truncate(3);
    assert_eq!((t.len, t.records().map(|r| r.key(0) as i64).collect::<Vec<_>>()), (3, want));
    t.clear();
    for i in 0..40u32 {
        let r = t.append(&[i as u64]).expect("dense growth to 32 then... ");
        r.add_count(1, 1);
        if i == 31 {
            break;
        }
    }
    assert_eq!((t.cap, t.len, t.mode), (32, 32, DENSE));
    assert!(t.append(&[99]).is_none(), "dense at the limit refuses");
    assert_eq!(t.record(31).key(0), 31);
}

#[test]
fn a_share_budgeted_table_takes_its_limit_from_the_armed_region() {
    use gravity_abi::breaker::SHARE;
    let l = RecordLayout::new(&[KeyKind::I32], &[SlotKind::Count]);
    let mut buf = table(&l, 0, HashAgg::bytes(&l, 32));
    unsafe { HashAgg::arm(buf.as_mut_ptr() as *mut u8, HashAgg::bytes(&l, 8)) };
    let t = at(&mut buf, &l, 256, SHARE);
    assert_eq!((t.cap, t.limit), (8, HashAgg::bytes(&l, 8)), "the opening capacity is cut to what the share holds");
    for k in 0..8 {
        t.find_or_insert(&[k]).expect("room").add_count(0, 1);
    }
    assert!(t.find_or_insert(&[99]).is_none(), "the armed limit bounds the table, not the region");
    let again = at(&mut buf, &l, 256, SHARE);
    assert_eq!(again.len, 8);
}

#[test]
#[should_panic(expected = "region without a limit")]
fn an_unarmed_share_region_is_refused() {
    let l = RecordLayout::new(&[], &[SlotKind::Count]);
    let mut buf = table(&l, 1, 64);
    at(&mut buf, &l, 1, gravity_abi::breaker::SHARE);
}

#[test]
fn a_one_record_table_is_the_plain_aggregate() {
    let l = RecordLayout::new(&[], &[SlotKind::Count, SlotKind::SumF64]);
    let limit = HashAgg::bytes(&l, 1);
    assert_eq!(limit, HASHAGG_HDR as u32 + 24);
    let mut buf = table(&l, 1, limit);
    let t = at(&mut buf, &l, 1, limit);
    t.single().add_count(0, 5);
    t.single().add_f64(1, 2.5).expect("finite");
    assert_eq!((t.len, t.single().count(0), t.single().f64(1)), (1, 5, 2.5));
    assert_eq!(t.records().count(), 1);
}

#[test]
fn growth_keeps_every_group_findable_under_clustered_keys() {
    let l = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count]);
    let limit = HashAgg::bytes(&l, 4096);
    let mut buf = table(&l, 4, limit);
    let t = at(&mut buf, &l, 4, limit);
    // sequential, strided and mirrored keys: long probe chains that must survive every settle
    let keys: Vec<u64> = (0..1200u64).chain((1..=1200).map(|k| k * 4096)).chain((0..1000).map(|k| u64::MAX - k * 7)).collect();
    for pass in 0..2 {
        for k in &keys {
            t.find_or_insert(&[*k]).expect("under the limit").add_count(0, 1);
        }
        assert_eq!(t.len, keys.len() as u32, "pass {pass}");
    }
    assert_eq!(t.cap, 4096);
    for k in &keys {
        assert_eq!(t.find(&[*k]).map(|r| r.count(0)), Some(2), "key {k}");
    }
    assert_eq!(t.records().count(), keys.len());
    assert_eq!(t.records().map(|r| r.count(0)).sum::<i64>(), 2 * keys.len() as i64);
}

#[test]
fn dense_sort_orders_a_hundred_hashed_groups() {
    let l = RecordLayout::new(&[KeyKind::I32], &[SlotKind::SumI64, SlotKind::Count]);
    let limit = HashAgg::bytes(&l, 4096);
    let mut buf = table(&l, 256, limit);
    let t = at(&mut buf, &l, 256, limit);
    for k in 0..100i32 {
        let r = t.find_or_insert(&[KeyCell::I32(k).word()]).unwrap();
        r.add_i64(0, k as i64 * 3);
        r.add_count(1, 1);
    }
    t.sort_dense_by_key(0);
    let ks: Vec<i64> = (0..t.cap as usize).map(|i| t.record(i)).filter(|r| r.used()).map(|r| r.key(0) as i64).collect();
    assert_eq!(ks, (0..100).collect::<Vec<i64>>());
    assert_eq!(t.record(99).i64(0), 297);
}

mod topk {
    use gravity_abi::breaker::{TopK, TOPK_TIES};

    fn run(_n: u32, cap: u32, _flags: u32) -> (Vec<u64>, u32) {
        (vec![0u64; TopK::bytes_cap(cap, 2).div_ceil(8) as usize], cap)
    }

    fn keys(t: &TopK) -> Vec<i64> {
        (0..t.len as usize).map(|i| t.key_of(i)).collect()
    }

    #[test]
    fn a_ties_run_keeps_every_row_at_the_bound_and_drops_rows_past_it() {
        let (mut buf, cap) = run(3, 6, TOPK_TIES);
        let t = unsafe { TopK::at_cap(buf.as_mut_ptr() as *mut u8, 3, cap, 2, TOPK_TIES) };
        assert!(t.ties() && t.bound().is_none());
        for (k, payload) in [(9i64, 90u64), (5, 50), (5, 51), (7, 70)] {
            assert_eq!(t.insert_ties(&[k as u64, payload], &[0, 0]), Some(true));
        }
        assert_eq!((keys(t), t.bound()), (vec![5, 5, 7, 9], Some(7)), "the bound is the 3rd smallest key; row 9 is past it but not yet evicted");
        assert_eq!(t.insert_ties(&[8, 80], &[0, 0]), Some(false), "past the bound: refused");
        assert_eq!(t.insert_ties(&[7, 71], &[0, 0]), Some(true), "a tie at the bound enters after its equals");
        assert_eq!(keys(t), vec![5, 5, 7, 7, 9]);
        assert_eq!(t.insert_ties(&[7, 72], &[0, 0]), Some(true));
        assert_eq!(keys(t), vec![5, 5, 7, 7, 7, 9], "full at capacity 6");
        assert_eq!(t.insert_ties(&[7, 73], &[0, 0]), Some(true), "full: the row past the bound (9) is dropped for the tie");
        assert_eq!(keys(t), vec![5, 5, 7, 7, 7, 7]);
        assert_eq!(t.insert_ties(&[7, 74], &[0, 0]), None, "full of ties at the bound: overflow, the kernel refuses");
        assert_eq!(t.insert_ties(&[1, 10], &[0, 0]), Some(true), "a smaller key moves the bound to 5: the last 7 is dead and dropped");
        assert_eq!((keys(t), t.bound()), (vec![1, 5, 5, 7, 7, 7], Some(5)));
        assert_eq!(t.insert_ties(&[7, 75], &[0, 0]), Some(false), "7 is past the new bound");
        assert_eq!(t.insert_ties(&[5, 52], &[0, 0]), Some(true), "a tie at 5 drops a dead 7");
        assert_eq!(keys(t), vec![1, 5, 5, 5, 7, 7]);
        assert_eq!(t.row(3).0[1], 52, "ties enter after their equals (stable)");
        t.truncate(3);
        assert_eq!(keys(t), vec![1, 5, 5]);
        let again = unsafe { TopK::view(buf.as_ptr() as *const u8) };
        assert_eq!((again.n, again.cap, again.flags, again.len), (3, 6, TOPK_TIES, 3), "a partial describes itself");
    }

    #[test]
    fn without_ties_the_run_is_exactly_n_rows_and_the_bound_excludes_equals() {
        let (mut buf, cap) = run(3, 3, 0);
        let t = unsafe { TopK::at_cap(buf.as_mut_ptr() as *mut u8, 3, cap, 2, 0) };
        for k in [9i64, 5, 7, 5, 11, 1] {
            t.insert(&[k as u64, (k * 10) as u64], &[0, 0]);
        }
        assert_eq!((keys(t), t.bound()), (vec![1, 5, 5], Some(5)));
        assert!(!t.insert(&[5, 53], &[0, 0]), "an equal key never enters without ties");
        assert!(t.insert(&[4, 40], &[0, 0]));
        assert_eq!(keys(t), vec![1, 4, 5]);
    }

    #[test]
    fn sort_by_reorders_rows_in_place() {
        let (mut buf, cap) = run(4, 4, 0);
        let t = unsafe { TopK::at_cap(buf.as_mut_ptr() as *mut u8, 4, cap, 2, 0) };
        for (k, p) in [(2i64, 20u64), (2, 5), (1, 7), (2, 1)] {
            t.insert(&[k as u64, p], &[0, 0]);
        }
        assert_eq!(keys(t), vec![1, 2, 2, 2]);
        t.sort_by(|a, b| (a.0[0] as i64, a.0[1]) < (b.0[0] as i64, b.0[1]));
        let rows: Vec<(i64, u64)> = (0..4).map(|i| (t.key_of(i), t.row(i).0[1])).collect();
        assert_eq!(rows, vec![(1, 7), (2, 1), (2, 5), (2, 20)]);
    }
}

#[test]
fn the_dense_front_caches_record_indexes_inside_its_domain() {
    use gravity_abi::breaker::{DenseFront, ACC_CELL};
    assert_eq!(DenseFront::domain(-3, 96), Some(100));
    assert_eq!(DenseFront::domain(0, u32::MAX as i64 - 1), Some(u32::MAX), "the structural bound is the front's u32 count");
    assert_eq!(DenseFront::domain(0, u32::MAX as i64), None, "one past the count");
    // the cost bound: the cells keep to half the cache partition, whatever the machine's L2 share
    for cache in [512u64 << 10, 2 << 20, 8 << 20] {
        let n = (cache / 2 / ACC_CELL as u64) as u32;
        assert!(DenseFront::acc_fits(n, cache) && !DenseFront::acc_fits(n + 1, cache), "cache {cache}");
    }
    assert!(DenseFront::acc_fits(65_536, 4 << 20), "a 65536-value domain pays on a 4 MiB partition");
    assert_eq!(DenseFront::domain(5, 4), None, "an empty domain is no front");
    assert_eq!(DenseFront::domain(i64::MIN, i64::MAX), None, "no overflow on the widest domain");
    assert_eq!(DenseFront::bytes(100), 16 + 400);
    assert_eq!(gravity_abi::breaker::DenseFront::bytes_with_acc(100), 416 + 100 * gravity_abi::breaker::ACC_CELL as u32);
    assert_eq!(gravity_abi::breaker::DenseFront::bytes_with_acc(3), 32 + 3 * 24, "the cells start 8-aligned");
    let mut buf = vec![0u64; (DenseFront::bytes(100) as usize).div_ceil(8)];
    let f = unsafe { DenseFront::at(buf.as_mut_ptr() as *mut u8, -3, 100) };
    assert_eq!((f.slot(-3), f.slot(96), f.slot(-4), f.slot(97)), (Some(0), Some(99), None, None));
    assert_eq!(f.get(0), None);
    f.set(0, 17);
    f.set(99, 0);
    assert_eq!((f.get(0), f.get(99), f.get(1)), (Some(17), Some(0), None));
    f.clear();
    assert_eq!((f.get(0), f.get(99)), (None, None));
}

#[test]
fn a_table_sorts_and_coalesces_dense_under_any_order() {
    // (I64, I64) → Count: 40 records keyed (r % 5, r % 3); ordered by the second key then the first
    // (a general comparator), equal second keys folded together: five groups with counts by r % 3
    let layout = RecordLayout::new(&[KeyKind::I64, KeyKind::I64], &[SlotKind::Count]);
    let limit = HashAgg::bytes(&layout, 64);
    let mut buf = table(&layout, 64, limit);
    let t = at(&mut buf, &layout, 64, limit);
    for r in 0..40u64 {
        t.find_or_insert(&[r % 5, r % 3]).unwrap().add_count(0, 1);
    }
    assert_eq!(t.len, 15);
    let key = |p: *const u8, k: usize| unsafe { *(p.add(8 * k) as *const u64) };
    t.sort_dense_by(&mut |a, b| (key(a, 1), key(a, 0)) < (key(b, 1), key(b, 0)));
    let keys: Vec<(u64, u64)> = (0..15).map(|i| (t.record(i).key(1), t.record(i).key(0))).collect();
    let mut want = keys.clone();
    want.sort();
    assert_eq!(keys, want, "ordered by (key 1, key 0)");
    let n = t.coalesce_dense(&mut |a, b| key(a, 1) == key(b, 1)).expect("no float slot");
    assert_eq!(n, 3);
    let counts: Vec<(u64, i64)> = (0..3).map(|i| (t.record(i).key(1), t.record(i).count(0))).collect();
    assert_eq!(counts, vec![(0, 14), (1, 13), (2, 13)], "the fold sums the counts of equal second keys");
    assert!(!t.record(3).used(), "the gap is closed");
}


#[test]
fn a_partitioned_build_probes_inside_its_slice_and_resumes_a_chain() {
    use gravity_abi::breaker::{table_kind, HashBuild, BUILD_TABLE, TABLE_BUILD};
    let bytes = HashBuild::bytes(1024);
    let mut buf = vec![0u64; (bytes as usize).div_ceil(8)];
    assert_eq!(unsafe { table_kind(buf.as_ptr() as *const u8) }, 0, "a zeroed region has no kind");
    let b = unsafe { HashBuild::at_partitioned(buf.as_mut_ptr() as *mut u8, bytes, 3) };
    assert_eq!((b.cap, b.pbits, b.table, b.kind), (1024, 3, BUILD_TABLE, TABLE_BUILD));
    assert_eq!(unsafe { table_kind(buf.as_ptr() as *const u8) }, TABLE_BUILD);
    let bp = buf.as_mut_ptr() as *mut HashBuild;
    for k in 0..300i32 {
        assert_eq!(unsafe { HashBuild::insert_partitioned(bp, k, k as i64 * 10, 1) }, Some(()));
        assert_eq!(unsafe { HashBuild::insert_partitioned(bp, k, k as i64 * 10 + 1, 1) }, Some(()), "a duplicate key");
    }
    let b = unsafe { HashBuild::view(buf.as_ptr() as *const u8) };
    assert_eq!(b.len, 600);
    for k in 0..300i32 {
        let mut got: Vec<i64> = b.probe(k).map(|e| e.payload).collect();
        got.sort();
        assert_eq!(got, vec![k as i64 * 10, k as i64 * 10 + 1], "key {k}");
        // every entry of the key lives inside the key's slice
        let sub = 1024 >> 3;
        let base = HashBuild::partition_of(k, 3) as usize * sub;
        for i in 0..1024 {
            let e = b.entry(i);
            if e.used != 0 && e.key == k {
                assert!(i >= base && i < base + sub, "entry {i} of key {k} outside its slice");
            }
        }
    }
    assert!(b.probe(1000).next().is_none());
    // the chain resumes after a stop: take one match, remember the position, continue from it
    let mut p = b.probe(7);
    let first = p.next().unwrap().payload;
    let at = p.position();
    let rest: Vec<i64> = b.probe_from(7, at).map(|e| e.payload).collect();
    assert_eq!(rest.len(), 1);
    assert_ne!(rest[0], first);
    // a slice fills without touching its neighbours
    let mut buf = vec![0u64; (HashBuild::bytes(16) as usize).div_ceil(8)];
    let s = unsafe { HashBuild::at_partitioned(buf.as_mut_ptr() as *mut u8, HashBuild::bytes(16), 2) } as *mut HashBuild;
    let k0 = (0..1000i32).find(|&k| HashBuild::partition_of(k, 2) == 0).unwrap();
    for i in 0..4 {
        assert_eq!(unsafe { HashBuild::insert_partitioned(s, k0, i, 1) }, Some(()));
    }
    assert_eq!(unsafe { HashBuild::insert_partitioned(s, k0, 4, 1) }, None, "slice 0 (4 entries) is full");
    let k1 = (0..1000i32).find(|&k| HashBuild::partition_of(k, 2) == 1).unwrap();
    assert_eq!(unsafe { HashBuild::insert_partitioned(s, k1, 0, 1) }, Some(()), "slice 1 still has room");
    assert_eq!(unsafe { (*s).len }, 5);
}

/// A table refilled from one spill partition (every key's top hash bits equal) must still use
/// every home: the partition bits are the top bits, the home the low bits, for both tables.
#[test]
fn a_table_refilled_from_one_partition_uses_every_home() {
    use gravity_abi::breaker::{HashBuild, BUILD_TABLE};
    let (cap, mask) = (4096u32, 4095usize);
    let bytes = HashBuild::bytes(cap);
    let mut buf = vec![0u64; (bytes as usize).div_ceil(8)];
    let b = unsafe { HashBuild::at(buf.as_mut_ptr() as *mut u8, cap, BUILD_TABLE) };
    let (mut k, mut n) = (0i32, 0);
    while n < 2048 {
        if HashBuild::partition_of(k, 4) == 5 {
            assert!(b.insert(k, k as i64));
            n += 1;
        }
        k += 1;
    }
    let (mut steps, mut misses) = (0usize, 0);
    while misses < 1000 {
        if HashBuild::partition_of(k, 4) == 5 {
            let mut p = b.probe(k);
            let home = p.position();
            assert!(p.by_ref().next().is_none());
            steps += (p.position().wrapping_sub(home)) & mask;
            misses += 1;
        }
        k += 1;
    }
    assert!(steps < 4 * misses, "build: {steps} slots over {misses} missing-key probes at load 1/2");

    let layout = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count]);
    let limit = HashAgg::bytes(&layout, cap);
    let mut buf = table(&layout, cap, limit);
    let t = at(&mut buf, &layout, cap, limit);
    let (mut key, mut n) = (0u64, 0);
    while n < 2048 {
        if KeyHash::of(&[key]).part(4) == 5 {
            assert!(t.find_or_insert(&[key]).is_some());
            n += 1;
        }
        key += 1;
    }
    let (mut steps, mut misses) = (0usize, 0);
    while misses < 1000 {
        let h = KeyHash::of(&[key]);
        if h.part(4) == 5 {
            let i = t.find_or_insert_index(&[key]).expect("room");
            steps += i.wrapping_sub(t.home(h)) & mask;
            misses += 1;
        }
        key += 1;
    }
    assert!(steps < 4 * misses, "agg: {steps} slots over {misses} new-key inserts at load 1/2..");
}

mod phase1 {
    use gravity_abi::breaker::{HashAgg, KeyKind, RecordLayout, SlotKind, T1, T1_HDR, TABLE_T1};
    use gravity_abi::hash::KeyHash;

    fn region(bytes: usize) -> Vec<u64> {
        vec![0u64; bytes / 8]
    }

    #[test]
    fn the_phase1_table_probes_two_slots_evicts_the_home_and_drains_into_partitioned_buffers() {
        let l = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]);
        let limit = 256 << 10;
        let mut buf = region(limit);
        let p = buf.as_mut_ptr() as *mut u8;
        let t = unsafe { T1::at(p, &l, 3, 8 << 10, limit as u32) } as *mut T1;
        let (cap, parts, blocks) = unsafe { ((*t).cap, (*t).parts(), (*t).nblocks) };
        assert!(cap.is_power_of_two() && cap as usize * l.size() as usize <= 8 << 10 && parts == 8 && blocks >= 8, "cap {cap} blocks {blocks}");
        assert_eq!(unsafe { gravity_abi::breaker::table_kind(p) }, TABLE_T1);
        assert!(T1_HDR == 160);
        // every key 0..4·cap offered three times: the table holds at most cap, the rest went to the buffers whole
        let n = 4 * cap as u64;
        for round in 0..3 {
            for k in 0..n {
                let keys = [k];
                let h = KeyHash::of(&keys);
                let r = match unsafe { T1::probe(t, &keys, h) } {
                    Some(r) => r,
                    None => {
                        let home = unsafe { (*t).home(h) };
                        assert!(unsafe { T1::evict(t, home) }, "the pool has room (round {round})");
                        unsafe { T1::open_at(t, home, &keys) }
                    }
                };
                unsafe { gravity_abi::breaker::Rec::at(r, &l) }.add_count(0, 1);
            }
        }
        let held = unsafe { (*t).held() };
        assert!(unsafe { (*t).len } as u64 <= cap as u64 && held >= n, "held {held} of {n} keys");
        assert!(unsafe { T1::drain(t) }, "the drain fits the pool");
        assert_eq!(unsafe { ((*t).len, (*t).held()) }, (0, held));
        // the buffers fold back to exactly the groups with their counts, and every record sits in its partition
        let mut agg = region(HashAgg::bytes(&l, 4 * n.next_power_of_two() as u32) as usize + 64);
        let a = unsafe { HashAgg::at(agg.as_mut_ptr() as *mut u8, &l, 256, HashAgg::bytes(&l, 4 * n.next_power_of_two() as u32)) };
        let t = unsafe { &*t };
        for part in 0..parts as u32 {
            for b in t.chain(part) {
                for i in 0..b.recs {
                    let r = unsafe { gravity_abi::breaker::Rec::at(b.ptr.add(i * l.size() as usize) as *mut u8, &l) };
                    assert_eq!(r.route().part(t.pbits as u32), part);
                    a.find_or_insert(r.keys()).unwrap().fold(&r);
                }
            }
        }
        assert_eq!(a.len as u64, n);
        assert!(a.records().all(|r| r.count(0) == 3), "three per key across the table and the buffers");
        assert_eq!(t.nth_smallest_key(0, 1), None, "an empty table proves no bound");
    }

    /// D203: a record under the 64 KiB share floor at 2,048 partitions (TPC-H Q1's shape at W16 with work_mem 64kB) left the
    /// pool a quarter per chain under one record and `at` aborted the worker; now a block holds one record, the chains share the few
    /// blocks there are, and an exhausted pool refuses the eviction (the caller's flush) instead.
    #[test]
    fn a_share_at_the_floor_lays_out_one_record_blocks_and_refuses_when_the_pool_is_spent() {
        let l = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count, SlotKind::SumI128, SlotKind::SumF64]);
        let limit = 64 << 10;
        let mut buf = region(limit);
        let t = unsafe { T1::at(buf.as_mut_ptr() as *mut u8, &l, 11, 1 << 20, limit as u32) } as *mut T1;
        let (cap, parts, blocks, recs) = unsafe { ((*t).cap, (*t).parts(), (*t).nblocks, (*t).block_recs) };
        assert!(parts == 2048 && recs == 1 && blocks >= 1 && (blocks as usize) < parts, "cap {cap} blocks {blocks} recs {recs}");
        let mut refused = false;
        for k in 0..16 * cap as u64 {
            let keys = [k];
            let h = KeyHash::of(&keys);
            let r = match unsafe { T1::probe(t, &keys, h) } {
                Some(r) => r,
                None => {
                    let home = unsafe { (*t).home(h) };
                    if !unsafe { T1::evict(t, home) } {
                        refused = true;
                        break;
                    }
                    unsafe { T1::open_at(t, home, &keys) }
                }
            };
            unsafe { gravity_abi::breaker::Rec::at(r, &l) }.add_count(0, 1);
        }
        assert!(refused, "the pool's {blocks} one-record blocks run out before {} keys", 16 * cap);
        let t = unsafe { &*t };
        let chained: usize = (0..parts as u32).map(|p| t.chain(p).map(|b| b.recs).sum::<usize>()).sum();
        assert_eq!(chained, blocks as usize, "every block holds its one record");
    }

    /// D2988: `T1::held_chains` marks exactly the partitions whose chain holds a record, as the per-partition chain walk
    /// does: none for a fresh table or a reset pool, the few buffered partitions across the mask words' edges (0, 63, 64,
    /// 200, 255 of 256), every one when every partition buffers.
    #[test]
    fn held_chains_marks_the_partitions_whose_chain_holds_a_record() {
        let l = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]);
        let limit = 4 << 20;
        let mut buf = region(limit);
        let t = unsafe { T1::at(buf.as_mut_ptr() as *mut u8, &l, 8, 4 << 10, limit as u32) } as *mut T1;
        let (parts, pbits) = unsafe { ((*t).parts(), (*t).pbits as u32) };
        assert_eq!(parts, 256);
        let walk = |t: *mut T1| -> Vec<u64> {
            let t = unsafe { &*t };
            let mut m = vec![0u64; parts / 64];
            for part in 0..parts as u32 {
                if t.chain(part).any(|b| b.recs != 0) {
                    m[part as usize / 64] |= 1 << (part % 64);
                }
            }
            m
        };
        let held = |t: *mut T1| -> Vec<u64> {
            let mut m = vec![0u64; parts / 64];
            unsafe { &*t }.held_chains(&mut m);
            m
        };
        let insert = |t: *mut T1, k: u64| {
            let keys = [k];
            let h = KeyHash::of(&keys);
            let r = match unsafe { T1::probe(t, &keys, h) } {
                Some(r) => r,
                None => {
                    let home = unsafe { (*t).home(h) };
                    assert!(unsafe { T1::evict(t, home) }, "the pool has room");
                    unsafe { T1::open_at(t, home, &keys) }
                }
            };
            unsafe { gravity_abi::breaker::Rec::at(r, &l) }.add_count(0, 1);
        };
        assert_eq!(held(t), vec![0; 4], "a fresh table buffers nothing");
        let edges = [0u32, 63, 64, 200, 255];
        let mut k = 0u64;
        let mut offered = 0;
        while unsafe { (*t).buffered } < 64 {
            if edges.contains(&KeyHash::of(&[k]).part(pbits)) {
                insert(t, k);
                offered += 1;
            }
            k += 1;
        }
        let m = held(t);
        assert_eq!(m, walk(t), "after {offered} keys");
        assert!(m.iter().map(|w| w.count_ones()).sum::<u32>() >= 2, "{m:x?}");
        assert!((0..parts as u32).all(|p| m[p as usize / 64] >> (p % 64) & 1 == 0 || edges.contains(&p)), "{m:x?}");
        unsafe { (*t).reset_pool() };
        assert_eq!(held(t), vec![0; 4], "a reset pool buffers nothing");
        let mut k = 0u64;
        while walk(t) != vec![u64::MAX; 4] && k < 64 * parts as u64 {
            insert(t, k);
            k += 1;
        }
        assert_eq!(held(t), vec![u64::MAX; 4], "every partition buffers after {k} keys");
    }

    /// A drain proportional to `len` (lowerer-ladder.md §6.1's need): the slots opened since the last drain are
    /// listed behind the hashes, so a claim's few groups drain without the `cap` walk — the buffers' records in
    /// the walk's order (slot order), claim after claim; evictions relist a slot; a list that fills falls back to
    /// the walk and the next claim lists again.
    #[test]
    fn the_drain_walks_the_opened_slots_in_slot_order_and_falls_back_to_the_walk_when_the_list_fills() {
        use gravity_abi::breaker::{open_cap, OPENED_OVER, T1_SIDE_BYTES};
        let l = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]);
        let limit = 1 << 20;
        let (mut buf, mut model) = (region(limit), region(limit));
        let t = unsafe { T1::at(buf.as_mut_ptr() as *mut u8, &l, 3, 64 << 10, limit as u32) } as *mut T1;
        let m = unsafe { T1::at(model.as_mut_ptr() as *mut u8, &l, 3, 64 << 10, limit as u32) } as *mut T1;
        let cap = unsafe { (*t).cap } as usize;
        assert!(unsafe { (*t).pool_off - (*t).hash_off } as usize == 4 * cap + 4 * open_cap(cap) + T1_SIDE_BYTES && open_cap(cap) == cap / 8);
        let insert = |t: *mut T1, k: u64| {
            let keys = [k];
            let h = KeyHash::of(&keys);
            let r = match unsafe { T1::probe(t, &keys, h) } {
                Some(r) => r,
                None => {
                    let home = unsafe { (*t).home(h) };
                    assert!(unsafe { T1::evict(t, home) });
                    unsafe { T1::open_at(t, home, &keys) }
                }
            };
            unsafe { gravity_abi::breaker::Rec::at(r, &l) }.add_count(0, 1);
        };
        let chains = |t: *mut T1| -> Vec<Vec<(u64, i64)>> {
            let t = unsafe { &*t };
            let mut out = Vec::new();
            for part in 0..t.parts() as u32 {
                let mut recs = Vec::new();
                for b in t.chain(part) {
                    for i in 0..b.recs {
                        let r = unsafe { gravity_abi::breaker::Rec::at(b.ptr.add(i * 16) as *mut u8, &l) };
                        recs.push((r.keys()[0], r.count(0)));
                    }
                }
                out.push(recs);
            }
            out
        };
        // claims of few groups (19), of colliding keys (evictions), and of more groups than the list holds;
        // the model table forgets its list before every drain (the walk): the buffers must agree record for record
        for (claim, groups) in [19usize, 19, 3 * cap, 19, cap / 8 + 1, 1, 0, 19].into_iter().enumerate() {
            for i in 0..4 * groups.max(1) as u64 {
                let k = claim as u64 * 1_000_003 + (i * 7) % groups.max(1) as u64;
                if groups > 0 {
                    insert(t, k);
                    insert(m, k);
                }
            }
            let opened = unsafe { (*t).opened };
            match groups {
                19 | 1 => assert!(opened != OPENED_OVER && opened as usize >= unsafe { (*t).len } as usize && opened <= 4 * 19, "claim {claim}: listed ({opened})"),
                0 => assert_eq!(opened, 0),
                _ => assert_eq!(opened, OPENED_OVER, "claim {claim}: the list filled"),
            }
            unsafe { (*m).opened = OPENED_OVER };
            assert!(unsafe { T1::drain(t) && T1::drain(m) });
            assert_eq!(unsafe { ((*t).len, (*t).opened, (*t).held()) }, (0, 0, unsafe { (*m).held() }));
            assert_eq!(chains(t), chains(m), "claim {claim}: the listed drain is the walk's, record for record");
        }
    }

    /// WC-02: the held bound's distinct check (a probe set beside the bounded heap) agrees with the scratch form's sort for
    /// every n, over thousands of held records: keys repeated within a claim and across drains (a group recurs as a fresh
    /// record), negative keys and far keys; over 4,000 records the probe runs collide and removals shift them back.
    #[test]
    fn the_held_bound_agrees_with_the_sorted_scratch_over_many_repeated_keys() {
        let l = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]);
        let limit = 2 << 20;
        let mut buf = region(limit);
        let t = unsafe { T1::at(buf.as_mut_ptr() as *mut u8, &l, 3, 8 << 10, limit as u32) } as *mut T1;
        let insert = |t: *mut T1, keys: [u64; 1]| {
            let h = KeyHash::of(&keys);
            let r = match unsafe { T1::probe(t, &keys, h) } {
                Some(r) => r,
                None => {
                    let home = unsafe { (*t).home(h) };
                    assert!(unsafe { T1::evict(t, home) });
                    unsafe { T1::open_at(t, home, &keys) }
                }
            };
            unsafe { gravity_abi::breaker::Rec::at(r, &l) }.add_count(0, 1);
        };
        for claim in 0..6u64 {
            for i in 0..700u64 {
                let v = ((i * 7919 + claim * 104_729) % 1500) as i64 - 600;
                insert(t, [v as u64]);
            }
            // a key far from the rest (high bits set)
            insert(t, [(claim << 60) | 5]);
            assert!(unsafe { T1::drain_below(t, 0, i64::MAX) });
        }
        for i in 0..300u64 {
            insert(t, [((i * 31) % 997) as u64]);
        }
        let h = unsafe { (*t).held() } as usize;
        assert!(h > 4000, "held {h}");
        for n in [1usize, 2, 3, 10, 63, 64, 65, 127, 128, 129, 300, 511, 512, 513, 900, 1023, 1024] {
            let sorted = unsafe { (*t).nth_smallest_key_held_in(0, n, &mut vec![0; h]) };
            assert_eq!(unsafe { (*t).nth_smallest_key_held(0, n) }, sorted, "n = {n}");
            assert!(sorted.is_some(), "n = {n}: the held records hold over 1024 distinct keys");
        }
        assert_eq!(unsafe { (*t).nth_smallest_key_held(0, 1025) }, None, "past SELECT_MAX: the scratch form's");
    }

    /// WC-02: the group walk's prune drops exactly the held records past the bound (every chain keeps its records under it
    /// in append order), leaves every bound up to its n as it was, frees the emptied blocks for the next appends, and
    /// empties every chain when nothing is at or under the bound; appends and drains after it hold what they should.
    #[test]
    fn the_held_prune_drops_the_records_past_the_bound_and_frees_their_blocks() {
        let l = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]);
        let limit = 256 << 10;
        let mut buf = region(limit);
        let t = unsafe { T1::at(buf.as_mut_ptr() as *mut u8, &l, 3, 8 << 10, limit as u32) } as *mut T1;
        let insert = |t: *mut T1, keys: [u64; 1]| {
            let h = KeyHash::of(&keys);
            let r = match unsafe { T1::probe(t, &keys, h) } {
                Some(r) => r,
                None => {
                    let home = unsafe { (*t).home(h) };
                    assert!(unsafe { T1::evict(t, home) });
                    unsafe { T1::open_at(t, home, &keys) }
                }
            };
            unsafe { gravity_abi::breaker::Rec::at(r, &l) }.add_count(0, 1);
        };
        // the held keys, chain by chain in append order
        let chains = |t: *mut T1| -> Vec<Vec<i64>> {
            let t = unsafe { &*t };
            (0..t.parts() as u32).map(|p| t.chain(p).flat_map(|b| (0..b.recs).map(move |i| unsafe { gravity_abi::breaker::Rec::at(b.ptr.add(i * l.size() as usize) as *mut u8, &l) }.key(0) as i64)).collect()).collect()
        };
        for claim in 0..6u64 {
            for i in 0..700u64 {
                insert(t, [((i * 7919 + claim * 104_729) % 1500) as u64 + 100]);
            }
            assert!(unsafe { T1::drain_below(t, 0, i64::MAX) });
        }
        let before = chains(t);
        let (held0, blocks0) = unsafe { ((*t).held(), (*t).next_block) };
        assert!(held0 > 4000 && unsafe { (*t).prune_due(64) } && !unsafe { (*t).prune_due(4096) }, "held {held0}");
        assert!(blocks0 as usize > unsafe { (*t).parts() }, "a small pool: chains span several blocks ({blocks0})");
        let bound = unsafe { (*t).nth_smallest_key_held(0, 64) }.expect("over 64 distinct keys held");
        let bounds: Vec<_> = (1..=64).map(|n| unsafe { (*t).nth_smallest_key_held(0, n) }).collect();
        let mut map = vec![0u32; unsafe { (*t).nblocks } as usize];
        let kept = unsafe { T1::prune_held_above(t, 0, bound, &mut map) };
        let want: Vec<Vec<i64>> = before.iter().map(|c| c.iter().copied().filter(|&k| k <= bound).collect()).collect();
        assert_eq!(chains(t), want, "each chain keeps its records at or under the bound, in append order");
        assert_eq!((kept, unsafe { (*t).held() }), (want.iter().map(Vec::len).sum::<usize>() as u64, kept));
        assert!(unsafe { (*t).next_block } < blocks0, "the emptied blocks are freed ({blocks0} -> {})", unsafe { (*t).next_block });
        assert_eq!((1..=64).map(|n| unsafe { (*t).nth_smallest_key_held(0, n) }).collect::<Vec<_>>(), bounds, "every bound up to n unchanged");
        assert!(!unsafe { (*t).prune_due(64) });
        // appends after the prune land behind the kept records, in the freed blocks
        for i in 0..3000u64 {
            insert(t, [(i * 31) % 997]);
        }
        assert!(unsafe { T1::drain_below(t, 0, i64::MAX) });
        let mut all: Vec<i64> = chains(t).concat();
        let mut want_all: Vec<i64> = want.concat();
        want_all.extend(0..997i64);
        (all.sort_unstable(), want_all.sort_unstable());
        (all.dedup(), want_all.dedup());
        assert_eq!(all, want_all, "the kept keys and the new ones (a key evicted and reopened may hold two records)");
        // a bound under every key empties every chain; the pool is whole again and takes appends
        let mut map = vec![0u32; unsafe { (*t).nblocks } as usize];
        assert_eq!(unsafe { T1::prune_held_above(t, 0, -1, &mut map) }, 0);
        assert_eq!((unsafe { (*t).held() }, unsafe { (*t).next_block }), (0, 0));
        assert!(chains(t).iter().all(Vec::is_empty));
        for i in 0..50u64 {
            insert(t, [i]);
        }
        assert!(unsafe { T1::drain_below(t, 0, i64::MAX) });
        assert_eq!(unsafe { (*t).nth_smallest_key_held(0, 50) }, Some(49));
    }

    #[test]
    fn the_topk_compaction_keeps_the_groups_under_the_bound_and_the_held_bound_counts_distinct_keys() {
        let l = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]);
        let limit = 256 << 10;
        let mut buf = region(limit);
        let t = unsafe { T1::at(buf.as_mut_ptr() as *mut u8, &l, 3, 8 << 10, limit as u32) } as *mut T1;
        let insert = |t: *mut T1, keys: [u64; 1]| {
            let h = KeyHash::of(&keys);
            let r = match unsafe { T1::probe(t, &keys, h) } {
                Some(r) => r,
                None => {
                    let home = unsafe { (*t).home(h) };
                    assert!(unsafe { T1::evict(t, home) });
                    unsafe { T1::open_at(t, home, &keys) }
                }
            };
            unsafe { gravity_abi::breaker::Rec::at(r, &l) }.add_count(0, 1);
        };
        for k in [7u64, 3, 0, 9, 1, 2, 3, 3] {
            insert(t, [k]);
        }
        let held = |n: usize| unsafe { (*t).nth_smallest_key_held(0, n) };
        let state = || unsafe { ((*t).len, (*t).held()) };
        // the scratch select (any n, scratch sized from held()) agrees with the stack heap's, and a short scratch proves no bound
        let agree = || {
            let h = unsafe { (*t).held() } as usize;
            for n in 0..=h + 1 {
                assert_eq!(unsafe { (*t).nth_smallest_key_held_in(0, n, &mut vec![i64::MIN; h]) }, held(n), "n = {n}");
            }
            assert_eq!(unsafe { (*t).nth_smallest_key_held_in(0, 1, &mut vec![0; h - 1]) }, None);
        };
        assert_eq!((held(3), held(6), held(7)), (Some(2), Some(9), None));
        agree();
        // the compaction: 0, 1, 2 to the buffers, the rest dropped, the table empty
        assert!(unsafe { T1::drain_below(t, 0, 2) });
        assert_eq!(state(), (0, 3));
        // a group drained earlier recurs as a fresh record: the held bound counts it once
        insert(t, [1]);
        insert(t, [5]);
        assert_eq!(state(), (2, 5));
        assert_eq!((unsafe { (*t).nth_smallest_key(0, 3) }, held(3), held(4)), (None, Some(2), Some(5)));
        agree();
        assert!(unsafe { T1::drain_below(t, 0, 2) });
        assert_eq!(state(), (0, 4));
        agree();
        let t = unsafe { &*t };
        let mut keys: Vec<u64> = (0..t.parts() as u32).flat_map(|p| t.chain(p)).flat_map(|b| (0..b.recs).map(move |i| unsafe { gravity_abi::breaker::Rec::at(b.ptr.add(i * l.size() as usize) as *mut u8, &l) }.key(0))).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec![0, 1, 1, 2]);
    }

    #[test]
    fn the_bypass_appends_and_a_full_pool_refuses_until_reset() {
        let l = RecordLayout::new(&[KeyKind::I32, KeyKind::I32], &[SlotKind::Count, SlotKind::SumI64]);
        let limit = 16 << 10;
        let mut buf = region(limit);
        let p = buf.as_mut_ptr() as *mut u8;
        let t = unsafe { T1::at(p, &l, 3, 4 << 10, limit as u32) } as *mut T1;
        let mut appended = 0u64;
        let refused = loop {
            let keys = [appended, appended % 7];
            let part = KeyHash::of(&keys).part(3);
            match unsafe { T1::append_new(t, part, &keys) } {
                Some(r) => {
                    let r = unsafe { gravity_abi::breaker::Rec::at(r, &l) };
                    r.add_count(0, 1);
                    r.add_i64(1, 5);
                    appended += 1;
                }
                None => break appended,
            }
        };
        assert!(refused > 0 && unsafe { (*t).buffered } == refused && unsafe { (*t).next_block == (*t).nblocks });
        let total: usize = (0..8).map(|p| unsafe { &*t }.chain(p).map(|b| b.recs).sum::<usize>()).sum();
        assert_eq!(total as u64, refused, "every appended record is in a chain");
        unsafe { (*t).reset_pool() };
        assert_eq!(unsafe { ((*t).buffered, (*t).next_block, (*t).flushes) }, (0, 0, 1));
        assert!(unsafe { T1::append_new(t, 0, &[1, 2]) }.is_some(), "the pool is free again");
        assert_eq!(unsafe { &*t }.chain(1).count(), 0);
    }

    /// D3312: a table under a quarter full that keeps evicting (a home pair two other keys hold, an input that cycles
    /// them) arms its image dedupe once `T1_DEDUPE_ARM` such evictions come between two flushes, and its cache then answers
    /// a copy of bytes it names with that image; evictions at a fuller load (keys outrunning the slots) never arm it.
    #[test]
    fn sparse_evictions_arm_the_image_dedupe_and_fuller_ones_do_not() {
        use gravity_abi::breaker::{T1_DEDUPE_ARM, T1_DEDUPE_ENTRIES};
        let l = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]);
        let limit = 1 << 20;
        let pinned: std::cell::RefCell<Vec<Vec<u64>>> = Default::default();
        let pin = |bytes: u64| {
            let mut v = vec![7u64; bytes as usize / 8];
            let p = v.as_mut_ptr() as *mut u8;
            pinned.borrow_mut().push(v);
            p
        };
        // one key opened at its home and evicted again, `n` times
        let cycle = |t: *mut T1, n: u64, key: u64| {
            for _ in 0..n {
                let keys = [key];
                let home = unsafe { (*t).home(KeyHash::of(&keys)) };
                unsafe { T1::open_at(t, home, &keys) };
                assert!(unsafe { T1::evict(t, home) }, "the pool has room");
            }
        };
        let mut buf = region(limit);
        let t = unsafe { T1::at(buf.as_mut_ptr() as *mut u8, &l, 3, 64 << 10, limit as u32) } as *mut T1;
        cycle(t, T1_DEDUPE_ARM - 1, 1);
        assert!(unsafe { T1::img_cache(t, pin) }.is_null(), "one sparse eviction short");
        unsafe { T1::dedupe_flushed(t) };
        cycle(t, T1_DEDUPE_ARM - 1, 2);
        assert!(unsafe { T1::img_cache(t, pin) }.is_null(), "a flush starts the count again");
        cycle(t, 1, 3);
        let c = unsafe { T1::img_cache(t, pin) };
        assert!(!c.is_null(), "armed");
        assert_eq!(pinned.borrow().len(), 1);
        assert_eq!(pinned.borrow()[0].len(), 2 * T1_DEDUPE_ENTRIES, "a fixed cache, all entries empty");
        assert!(pinned.borrow()[0].iter().all(|&w| w == 0));
        // a miss names the entry to fill; the copy noted there answers the same bytes, never other bytes of its length
        let same = |img: &'static [u8]| move |q: *const u8| unsafe { core::slice::from_raw_parts(q, img.len()) } == img;
        let (a, b): (&'static [u8], &'static [u8]) = (b"p123 a text key", b"p124 a text key");
        let e = unsafe { T1::img_find(c, T1::img_hash(0, a), a.len(), same(a)) }.unwrap_err();
        let copy = a.to_vec();
        unsafe { T1::img_note(c, e, copy.as_ptr(), copy.len()) };
        assert_eq!(unsafe { T1::img_find(c, T1::img_hash(0, a), a.len(), same(a)) }, Ok(copy.as_ptr() as *mut u8));
        assert!(unsafe { T1::img_find(c, T1::img_hash(0, b), b.len(), same(b)) }.is_err());
        unsafe { T1::dedupe_flushed(t) };
        assert_eq!(unsafe { T1::img_cache(t, pin) }, c, "a flush keeps an armed cache");
        assert_eq!(pinned.borrow().len(), 1);
        // a table half full: its evictions are its keys outrunning the slots, and count nothing
        let mut buf2 = region(limit);
        let t2 = unsafe { T1::at(buf2.as_mut_ptr() as *mut u8, &l, 3, 64 << 10, limit as u32) } as *mut T1;
        let cap = unsafe { (*t2).cap } as usize;
        let mut homes = std::collections::HashSet::new();
        let mut first = None;
        for k in 100u64.. {
            if unsafe { (*t2).len } as usize >= cap / 2 {
                break;
            }
            let keys = [k];
            let home = unsafe { (*t2).home(KeyHash::of(&keys)) };
            if homes.insert(home) {
                unsafe { T1::open_at(t2, home, &keys) };
                first.get_or_insert((k, home));
            }
        }
        let (k, home) = first.unwrap();
        for _ in 0..4 * T1_DEDUPE_ARM {
            assert!(unsafe { T1::evict(t2, home) });
            unsafe { T1::open_at(t2, home, &[k]) };
        }
        assert!(unsafe { (*t2).evictions } >= 4 * T1_DEDUPE_ARM);
        assert!(unsafe { T1::img_cache(t2, pin) }.is_null(), "evictions at half full never arm");
    }
}

/// A three-key count at W = 8 (2026-09-16): the worker share is `u32::MAX / 8` = 536,870,911 bytes, the phase-1 record 56 B,
/// 64 partitions — the block count came out odd and the table's 8-alignment overran the share by one byte.
#[test]
fn the_phase1_layout_fits_an_odd_share() {
    use gravity_abi::breaker::{T1, t1_default_bytes};
    // three key words (the phrase's fingerprint pair and the gmark word), four slots: count(*), min(URL), min(Title), count(distinct)
    let l = RecordLayout::new_nn(&[KeyKind::I64, KeyKind::I64, KeyKind::I64], &[SlotKind::Count, SlotKind::MinCode, SlotKind::MinCode, SlotKind::Count]);
    assert_eq!(l.size(), 56, "the three-key phase-1 record");
    let fits = |limit: u32, pbits: u8, t1: u32| {
        let mut region = vec![0u64; (limit as usize).div_ceil(8)];
        let t = unsafe { T1::at(region.as_mut_ptr() as *mut u8, &l, pbits, t1, limit) };
        assert!(t.pool_off as usize + t.nblocks as usize * t.block_bytes as usize <= limit as usize, "limit {limit}");
        (t.nblocks, t.block_bytes)
    };
    assert_eq!(fits(u32::MAX / 8, 6, t1_default_bytes()), (16_398, 32_704), "one block fewer than the unaligned count (16,399)");
    // a window of shares holding the odd counts (1,141,028..=1,141,031 overran before the fix)
    for limit in 1_140_000..1_142_000 {
        fits(limit, 3, 64 << 10);
    }
}

/// The setop posture (option c'): a T1 laid in a region reserved past its share keeps the share's table, block size and
/// block count, with chain words for the reservation's blocks (`T1_RESERVED`); `grow_pool` commits more blocks in place
/// (nothing laid moves: a head's block address and every chain kept) up to the reservation; a reservation at or under the
/// share lays the table exactly as `at_sized`.
#[test]
fn a_reserved_t1_grows_its_pool_in_place() {
    use gravity_abi::breaker::{T1, T1_RESERVED};
    let l = RecordLayout::new_nn(&[KeyKind::I64, KeyKind::I64], &[SlotKind::Count, SlotKind::SumI64]);
    let (share, reserve, pbits, t1) = (4u32 << 20, 64u64 << 20, 3u8, 256u32 << 10);
    let mut plain = vec![0u64; share as usize / 8];
    let p = unsafe { T1::at_sized(plain.as_mut_ptr() as *mut u8, &l, pbits, t1, t1, share) };
    let (cap, bb, nb) = (p.cap, p.block_bytes, p.nblocks);
    assert_eq!(p.flags & T1_RESERVED, 0);
    assert_eq!(p.reserved_blocks(), nb, "an unreserved table's chain words are its blocks");
    assert_eq!(p.pool_growth(), 0);
    let mut same = vec![0u64; share as usize / 8];
    let q = unsafe { T1::at_reserved(same.as_mut_ptr() as *mut u8, &l, pbits, t1, t1, share, share as u64) };
    assert_eq!((q.cap, q.block_bytes, q.nblocks, q.table_off, q.pool_off, q.flags), (cap, bb, nb, p.table_off, p.pool_off, 0), "a reservation at the share lays as at_sized");
    let mut region = vec![0u64; reserve as usize / 8];
    let base = region.as_mut_ptr() as *mut u8;
    let t = unsafe { T1::at_reserved(base, &l, pbits, t1, t1, share, reserve) } as *mut T1;
    let r = unsafe { &*t };
    assert_eq!(r.flags & T1_RESERVED, T1_RESERVED);
    assert_eq!((r.cap, r.block_bytes, r.limit), (cap, bb, share), "the share's table and blocks");
    assert!(r.nblocks >= 1 && r.nblocks <= nb && r.pool_off as usize + r.nblocks as usize * bb as usize <= share as usize, "the committed pool fits the share");
    let max = r.reserved_blocks();
    assert!(max > 8 * r.nblocks && r.pool_off as u64 + max as u64 * bb as u64 <= reserve, "the chain words cover the reservation, its blocks fit it");
    // fill the committed pool, then grow and keep appending: the earlier blocks and heads stay where they are
    let fill = |t: *mut T1| -> u64 {
        let mut n = 0u64;
        while let Some(rec) = unsafe { T1::append_new(t, (n % 8) as u32, &[n, n]) } {
            unsafe { Rec::at(rec, &l) }.add_count(0, 1);
            n += 1;
        }
        n
    };
    let first = fill(t);
    let block0 = unsafe { &*t }.chain(0).next().map(|b| b.ptr);
    assert_eq!(unsafe { (*t).next_block }, unsafe { (*t).nblocks }, "the share's pool spent");
    let mut grows = 0;
    loop {
        let need = unsafe { (*t).pool_growth() };
        if need == 0 {
            break;
        }
        let before = unsafe { (*t).nblocks };
        assert!(unsafe { T1::grow_pool(t, need) });
        assert_eq!(unsafe { (*t).nblocks }, before + (need / bb as u64) as u32);
        assert_eq!(unsafe { &*t }.chain(0).next().map(|b| b.ptr), block0, "nothing laid moves");
        assert!(fill(t) > 0, "the grown pool takes records");
        grows += 1;
    }
    assert!(grows >= 3 && unsafe { (*t).nblocks } == max, "doubled up to the reservation");
    assert!(!unsafe { T1::grow_pool(t, bb as u64) }, "past the reservation: refused");
    let total: usize = (0..8).map(|p| unsafe { &*t }.chain(p).map(|b| b.recs).sum::<usize>()).sum();
    assert_eq!(total as u64, unsafe { (*t).buffered }, "every record is in a chain");
    assert!(total as u64 > 8 * first, "the reservation held more than eight shares' pools");
}

/// C5 (revoke's give-back): `shrink_pool` is `grow_pool`'s reverse: once a grown reserved table's records are all written
/// (its pool reset), the growth's blocks leave and the table spills at its share's pool again (the same blocks, the same
/// records per fill); refused while it buffers anything, for bytes that are not whole blocks, or for its last block.
#[test]
fn a_reserved_t1_gives_its_pool_growth_back() {
    use gravity_abi::breaker::T1;
    let l = RecordLayout::new_nn(&[KeyKind::I64, KeyKind::I64], &[SlotKind::Count, SlotKind::SumI64]);
    let (share, reserve, pbits, t1) = (4u32 << 20, 64u64 << 20, 3u8, 256u32 << 10);
    let mut region = vec![0u64; reserve as usize / 8];
    let t = unsafe { T1::at_reserved(region.as_mut_ptr() as *mut u8, &l, pbits, t1, t1, share, reserve) } as *mut T1;
    let fill = |t: *mut T1| -> u64 {
        let mut n = 0u64;
        while let Some(rec) = unsafe { T1::append_new(t, (n % 8) as u32, &[n, n]) } {
            unsafe { Rec::at(rec, &l) }.add_count(0, 1);
            n += 1;
        }
        n
    };
    let (nb, limit, bb) = unsafe { ((*t).nblocks, (*t).limit, (*t).block_bytes as u64) };
    let first = fill(t);
    let mut grown = 0u64;
    for _ in 0..2 {
        let need = unsafe { (*t).pool_growth() };
        assert!(unsafe { T1::grow_pool(t, need) });
        grown += need;
        fill(t);
    }
    assert!(!unsafe { T1::shrink_pool(t, grown) }, "records buffered: refused");
    unsafe { (*t).reset_pool() };
    assert!(!unsafe { T1::shrink_pool(t, grown + 1) }, "not whole blocks: refused");
    assert!(!unsafe { T1::shrink_pool(t, (*t).nblocks as u64 * bb) }, "no block left: refused");
    assert!(unsafe { T1::shrink_pool(t, grown) });
    assert_eq!(unsafe { ((*t).nblocks, (*t).limit) }, (nb, limit), "back at the share's pool");
    assert_eq!(fill(t), first, "a fill holds what the share's pool held");
    assert_eq!(unsafe { (*t).pool_growth() }, unsafe { (*t).nblocks } as u64 * bb, "it could grow again (a revoked one is refused by its region)");
}

#[test]
fn numeric_slots() {
    let l = RecordLayout::new_nn(&[KeyKind::I32], &[SlotKind::SumNum, SlotKind::AvgNum, SlotKind::Count]);
    assert_eq!((l.size(), l.slot_off[0], l.slot_off[1], l.slot_off[2]), (56, 8, 24, 48), "16 + 24 + 8 after the key word");
    let mut buf = vec![0u64; l.size() as usize / 8];
    let r = unsafe { Rec::at(buf.as_mut_ptr() as *mut u8, &l) };
    r.open(&[7]);
    assert_eq!((r.slot(0), r.slot(1)), (Some(AggSlot::SumNum(0)), None), "a sum without a validity word reads as its word; an AvgNum is valid by its n");
    r.add_i128(0, 12_345);
    r.add_avg(1, 300, 2);
    r.add_count(2, 2);
    assert_eq!((r.slot(0), r.slot(1)), (Some(AggSlot::SumNum(12_345)), Some(AggSlot::AvgNum { sum: 300, n: 2 })));
    let mut other = vec![0u64; l.size() as usize / 8];
    let o = unsafe { Rec::at(other.as_mut_ptr() as *mut u8, &l) };
    o.open(&[7]);
    o.add_i128(0, -45);
    o.add_avg(1, -100, 1);
    o.add_count(2, 1);
    r.fold(&o).expect("no float overflow");
    assert_eq!((r.slot(0), r.slot(1), r.slot(2)), (Some(AggSlot::SumNum(12_300)), Some(AggSlot::AvgNum { sum: 200, n: 3 }), Some(AggSlot::Count(3))));
}

#[test]
fn a_run_sized_for_its_rows_opens_at_twice_them_and_doubles_within_its_region() {
    use gravity_abi::breaker::{SortRun, SORTRUN_HDR, SORTRUN_SIZED_WORDS};
    let mut region = vec![0u64; 1 << 17];
    let p = region.as_mut_ptr() as *mut u8;
    unsafe {
        (*(p as *mut SortRun)).row_bytes = 1 << 20; // armed: word 2 = the share
        let r = SortRun::at_rows(p, 512, 32);
        assert_eq!((r.cap, r.len, r.row_bytes, r.region()), (64, 0, 512, 1 << 20), "2 × 32");
        assert_eq!(r.rows_ptr(), p.add(SORTRUN_HDR) as *const u8);
        // through the run's own borrow (D1358: a read through `p` beside `r` would freeze `r`, and `push` then writes)
        assert_eq!(SortRun::region_bytes(r as *mut SortRun as *const u8, 512), 1 << 20, "the merge lays over the whole region");
        for i in 0..64u64 {
            *(r.push() as *mut u64) = i;
        }
        let mut caps = Vec::new();
        while r.grow() {
            caps.push(r.cap);
            assert_eq!(r.var_top(), SORTRUN_HDR as u32 + r.cap * 512, "the trailer moved past the new rows");
        }
        assert_eq!(caps, vec![128, 256, 512, 1024, ((1 << 20) - SORTRUN_HDR as u32 - SORTRUN_SIZED_WORDS) / 512], "five doublings, the last clamped to the region");
        assert_eq!(r.cap, 2047);
        assert!(!r.grow(), "the region is full");
        assert_eq!((r.len, r.region(), r.var_bytes()), (64, 1 << 20, 0));
        assert!((0..64).all(|i| *(r.row(i) as *const u64) == i as u64), "the rows stayed where they were");
        let again = SortRun::at_rows(p, 512, 32);
        assert_eq!((again.cap, again.len), (2047, 64), "read back, not re-opened");
    }
    // D17: the floor is a page of rows: 64 of 64 bytes (the old fixed floor), 8 of 512
    for (row, want) in [(64u32, 64u32), (512, 8)] {
        let mut fresh = vec![0u64; 1 << 17];
        let p = fresh.as_mut_ptr() as *mut u8;
        unsafe {
            (*(p as *mut SortRun)).row_bytes = 1 << 20;
            assert_eq!(SortRun::at_rows(p, row, 3).cap, want, "row {row}");
        }
    }
    let mut small = vec![0u64; 1 << 17];
    let p = small.as_mut_ptr() as *mut u8;
    unsafe {
        (*(p as *mut SortRun)).row_bytes = 1 << 20;
        let r = SortRun::at_rows(p, 512, 10_000_000);
        assert_eq!(r.cap, 2047, "an estimate past the share is bounded by it");
        assert!(!r.grow());
        r.clear();
        assert_eq!(r.region(), 1 << 20, "a put's clear keeps the region");
        let r = SortRun::at_share(p.add(0), 512);
        assert_eq!(r.cap, 2047, "a live run reads back through at_share too");
    }
    let mut held = vec![0u64; 1 << 17];
    let mut wide = vec![0u64; 1 << 17];
    let (p, q) = (held.as_mut_ptr() as *mut u8, wide.as_mut_ptr() as *mut u8);
    unsafe {
        (*(p as *mut SortRun)).row_bytes = 1 << 20;
        let r = SortRun::at_rows(p, 512, 23);
        assert!(r.reserve_var(64).is_some());
        assert!(!r.grow(), "a by-reference copy held: the run is put instead of moved");
        (*(q as *mut SortRun)).row_bytes = 1 << 20;
        let plain = SortRun::at_share(q, 512);
        assert_eq!((plain.region(), plain.grow()), (0, false), "a share-wide run has no region to grow into");
    }
}

/// D244: PG's float merges. `float8_pl` / `float4_pl` raise an infinite sum of finite operands only; `float8_combine` takes
/// an empty side's other, adds N, Sx by `float8_pl`, and taints (D432: Sxx = +Inf, the final raises) an infinite Sxx from
/// finite ones (avg over {1e200, -1e200}) or an Sx overflow; a tainted side keeps its taint.
#[test]
fn float_merges_raise_where_pg_raises() {
    assert!(float8_pl(1.5e308, 1.5e308).is_err());
    assert_eq!(float8_pl(f64::INFINITY, 1.0).ok(), Some(f64::INFINITY));
    assert!(float8_pl(f64::INFINITY, f64::NEG_INFINITY).unwrap().is_nan());
    assert!(float8_pl(f64::NAN, 1.0).unwrap().is_nan());
    assert!(float4_pl(3e38, 3e38).is_err());
    assert_eq!(float4_pl(f32::INFINITY, 3e38).ok(), Some(f32::INFINITY));
    let (a, b) = ([1.0, 1e200, 0.0], [1.0, -1e200, 0.0]);
    assert_eq!(float8_combine(a, b).unwrap()[2], F64_ACCUM_TAINT, "Sxx = 1*1*(2e200)^2/2 is infinite from finite parts");
    assert_eq!(float8_combine([0.0; 3], b).ok(), Some(b));
    assert_eq!(float8_combine(a, [0.0; 3]).ok(), Some(a));
    assert_eq!(float8_combine([2.0, 3.0, 0.5], [1.0, 3.0, 0.0]).ok(), Some([3.0, 6.0, 0.5 + 2.0 * 1.0 * 2.25 / 3.0]));
    assert_eq!(float8_combine([1.0, 1e308, 0.0], [1.0, 1e308, 0.0]).unwrap()[2], F64_ACCUM_TAINT, "Sx overflows by float8_pl");
    assert_eq!(float8_combine([2.0, 1e300, F64_ACCUM_TAINT], [1.0, 1.0, 0.0]).unwrap()[2], F64_ACCUM_TAINT, "a tainted side taints");
    assert!(float8_combine([2.0, 1e300, F64_ACCUM_TAINT], [1.0, f64::NAN, f64::NAN]).unwrap()[1].is_nan(), "a NaN Sx still wins at the final");
    let nan = float8_combine([1.0, f64::INFINITY, f64::NAN], [1.0, 1.0, 0.0]).unwrap();
    assert!(nan[1] == f64::INFINITY && nan[2].is_nan(), "an infinite input keeps NaN Sxx, no error");
    assert!(FloatOverflow::STATUS < 0);
}

/// D244: `Rec::fold` merges SumF64 by `float8_pl`, SumF32 by `float4_pl` and F64Accum by `float8_combine`, each overflow
/// the fold's error; a validity layout's first value is copied (a lone -0 stays -0).
#[test]
fn rec_fold_raises_a_float_merges_overflow() {
    let l = RecordLayout::new(&[], &[SlotKind::SumF64, SlotKind::SumF32, SlotKind::F64Accum]);
    let (mut ba, mut bb) = (vec![0u64; 16], vec![0u64; 16]);
    let (ra, rb) = unsafe { (Rec::at(ba.as_mut_ptr() as *mut u8, &l), Rec::at(bb.as_mut_ptr() as *mut u8, &l)) };
    ra.add_f64(0, -0.0).unwrap();
    assert!(ra.f64(0).is_sign_negative(), "the first value is copied, not added to +0");
    rb.add_f64(0, 1.0).unwrap();
    ra.fold(&rb).unwrap();
    assert_eq!(ra.f64(0), 1.0);
    for (slot, v) in [(0usize, 1.5e308f64), (1, 3e38)] {
        let (mut x, mut y) = (vec![0u64; 16], vec![0u64; 16]);
        let (rx, ry) = unsafe { (Rec::at(x.as_mut_ptr() as *mut u8, &l), Rec::at(y.as_mut_ptr() as *mut u8, &l)) };
        if slot == 0 {
            rx.add_f64(0, v).unwrap();
            ry.add_f64(0, v).unwrap();
        } else {
            rx.add_f32(1, v).unwrap();
            ry.add_f32(1, v).unwrap();
        }
        assert!(rx.fold(&ry).is_err(), "slot {slot}: partials finite, their merge overflows");
    }
    let (mut x, mut y) = (vec![0u64; 16], vec![0u64; 16]);
    let (rx, ry) = unsafe { (Rec::at(x.as_mut_ptr() as *mut u8, &l), Rec::at(y.as_mut_ptr() as *mut u8, &l)) };
    rx.combine_f64_accum(2, [1.0, 1e200, 0.0]).unwrap();
    ry.combine_f64_accum(2, [1.0, -1e200, 0.0]).unwrap();
    assert_eq!(rx.f64_accum(2), [1.0, 1e200, 0.0]);
    rx.fold(&ry).unwrap();
    assert_eq!(rx.f64_accum(2)[2], F64_ACCUM_TAINT, "avg's Sxx overflow at the merge taints the state (D432)");
    let nn = RecordLayout::new_nn(&[], &[SlotKind::Count, SlotKind::F64Accum]);
    let mut z = vec![0u64; 16];
    let rz = unsafe { Rec::at(z.as_mut_ptr() as *mut u8, &nn) };
    assert!(!rz.valid(1), "an empty accumulator (N = 0) is not valid");
    rz.combine_f64_accum(1, [2.0, 3.0, 0.5]).unwrap();
    assert!(rz.valid(1));
}

/// D252: `bit_and / bit_or / bit_xor` slots start NULL (a validity layout), take the first value, then merge by `&`, `|`,
/// `^`; an empty record folds nothing (the group stays NULL).
#[test]
fn bit_slots_start_null_and_merge_by_and_or_xor() {
    let l = RecordLayout::new(&[], &[SlotKind::BitAnd, SlotKind::BitOr, SlotKind::BitXor]);
    let (mut ba, mut bb, mut be) = (vec![0u64; 8], vec![0u64; 8], vec![0u64; 8]);
    let (ra, rb, re) = unsafe { (Rec::at(ba.as_mut_ptr() as *mut u8, &l), Rec::at(bb.as_mut_ptr() as *mut u8, &l), Rec::at(be.as_mut_ptr() as *mut u8, &l)) };
    assert!((0..3).all(|j| !ra.valid(j)), "a fresh record's bit slots are NULL");
    for j in 0..3 {
        ra.fold_bits(j, 0b1100);
        rb.fold_bits(j, 0b1010);
    }
    assert_eq!((0..3).map(|j| ra.i64(j)).collect::<Vec<_>>(), vec![0b1100; 3], "the first value is copied, not combined with 0");
    ra.fold(&re).expect("no float slot");
    assert_eq!((0..3).map(|j| ra.i64(j)).collect::<Vec<_>>(), vec![0b1100; 3], "an empty record folds nothing");
    ra.fold(&rb).expect("no float slot");
    assert_eq!((ra.i64(0), ra.i64(1), ra.i64(2)), (0b1000, 0b1110, 0b0110));
    re.fold(&ra).expect("no float slot");
    assert_eq!((re.i64(0), re.i64(1), re.i64(2)), (0b1000, 0b1110, 0b0110), "into an empty record: copied");
    assert!(!RecordLayout::new(&[], &[SlotKind::BitAnd]).slot_kinds().is_empty() && SlotKind::BitAnd.bytes() == 8);
}

/// D299: a merge past |2^126| is OVERFLOW, never a wrapped value (nor one on the sentinel's hi word, read as NaN); it is
/// sticky under finite sums and loses to NaN and to an infinity, as a finite sum does.
#[test]
fn d299_a_merge_past_the_range_is_overflow() {
    use gravity_abi::breaker::{num_add, NUM_SENT_HI};
    let sent = |c: i64| ((NUM_SENT_HI as i128) << 64) | c as u64 as i128;
    let (nan, pinf, ninf, ovf) = (sent(1), sent(2), sent(3), sent(4));
    let big = (1i128 << 126) - 1;
    assert_eq!(num_add(big, 0), big);
    assert_eq!(num_add(big, 1), ovf);
    assert_eq!(num_add(-big, -1), -big - 1);
    assert_eq!(num_add(-big - 1, -1), ovf);
    assert_eq!(num_add(i128::MAX - 5, 1 << 100), ovf);
    assert_eq!(num_add(ovf, 12_345), ovf);
    assert_eq!(num_add(-7, ovf), ovf);
    assert_eq!(num_add(ovf, ovf), ovf);
    assert_eq!(num_add(ovf, nan), nan);
    assert_eq!(num_add(pinf, ovf), pinf);
    assert_eq!(num_add(ovf, ninf), ninf);
}

#[test]
fn v4r168_the_words_sort_orders_as_the_heap_sort_at_every_width() {
    // Layouts of 3..=16 words (the fixed-width sort) and wider (the heap sort's fallback), each keyed by 1..=3 i64
    // words then a part word; many records share their first `g` words. `sort_dense_by_words(g)` must leave the first
    // `g` words in the order `sort_dense_by` leaves them, and move every record whole (the same multiset of records).
    let slot_sets: [&[SlotKind]; 6] = [
        &[SlotKind::Count],
        &[SlotKind::Count, SlotKind::MinCode],
        &[SlotKind::Count, SlotKind::SumI128],
        &[SlotKind::Count, SlotKind::SumI128, SlotKind::MinI64, SlotKind::MaxI64],
        &[SlotKind::Count, SlotKind::SumI128, SlotKind::AvgI128, SlotKind::SumF64, SlotKind::MinCode, SlotKind::MaxI64],
        &[SlotKind::Count, SlotKind::SumI128, SlotKind::AvgI128, SlotKind::AvgI128, SlotKind::SumI128, SlotKind::SumF64, SlotKind::MinCode, SlotKind::MaxI64],
    ];
    let mut widths = std::collections::BTreeSet::new();
    for groups in 1..=3usize {
        for slots in slot_sets {
            for validity in [false, true] {
                let keys = vec![KeyKind::I64; groups + 1];
                let layout = RecordLayout::with_sentinel(&keys, slots, validity, Some(0));
                let rec = layout.size() as usize;
                widths.insert(rec);
                let (cap, n) = (4096u32, 1500u64);
                let limit = HashAgg::bytes(&layout, cap);
                let mut a = table(&layout, cap, limit);
                let t = at(&mut a, &layout, cap, limit);
                let mut x = 0x9E37_79B9_7F4A_7C15u64;
                for r in 0..n {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    // the group words: few distinct first words (high bits set), then r's part
                    let mut k = vec![(x % 61) << 40 | (x >> 63) << 63, x % 3, x % 2];
                    k.truncate(groups);
                    k.push(r);
                    t.find_or_insert(&k).expect("room").add_count(0, (r % 7) as i64 + 1);
                }
                assert_eq!(t.len as u64, n);
                for g in 1..=groups {
                    let (mut h, mut w) = (a.clone(), a.clone());
                    let th = at(&mut h, &layout, cap, limit);
                    let words = |p: *const u8| unsafe { core::slice::from_raw_parts(p as *const u64, g) };
                    th.sort_dense_by(&mut |p, q| words(p) < words(q));
                    let tw = at(&mut w, &layout, cap, limit);
                    tw.sort_dense_by_words(g);
                    assert_eq!((th.len, tw.len), (n as u32, n as u32));
                    let image = |t: &HashAgg, i: usize| unsafe { core::slice::from_raw_parts(t.record(i).as_ptr() as *const u8, rec).to_vec() };
                    let heads = |t: &HashAgg| (0..n as usize).map(|i| words(t.record(i).as_ptr()).to_vec()).collect::<Vec<_>>();
                    assert_eq!(heads(th), heads(tw), "rec {rec} g {g}: the order of the first words");
                    let (mut all_h, mut all_w): (Vec<_>, Vec<_>) = ((0..n as usize).map(|i| image(th, i)).collect(), (0..n as usize).map(|i| image(tw, i)).collect());
                    all_h.sort();
                    all_w.sort();
                    assert_eq!(all_h, all_w, "rec {rec} g {g}: every record moved whole");
                }
            }
        }
    }
    assert!(widths.iter().any(|&r| r <= 64) && widths.iter().any(|&r| (72..=128).contains(&r)) && widths.iter().any(|&r| r > 128), "every fixed width and the fallback: {widths:?}");
}

/// D2471: a cleared run (its rows put) carries no seal. A sort slot written by two pipelines (a Hash Right Join's probe, then
/// its drain) kept the first writer's seal on the empty region after the drain's put, and the reader took that empty
/// region for the whole sorted input (0 rows where PostgreSQL 18.6 read 2465).
#[test]
fn a_cleared_run_is_never_sealed() {
    use gravity_abi::breaker::SortRun;
    let mut region = vec![0u64; 1 << 12];
    let p = region.as_mut_ptr() as *mut u8;
    unsafe {
        (*(p as *mut SortRun)).row_bytes = 1 << 15; // armed: word 2 = the share
        let r = SortRun::at_share(p, 64);
        *(r.push() as *mut u64) = 7;
        r.pad[0] = 0x5e; // the first writer's section end sealed its rows in the region (kernels rowsort SEALED)
        *(r.push() as *mut u64) = 3; // the second writer appends, then its overflow puts the run
        r.clear();
        assert_eq!((r.len, r.pad[0]), (0, 0), "an empty region is no sealed run");
    }
}

/// DR-6: the one partition rule answers what decision 3's copies answered (rows x record over 512 KiB slices, a power of
/// two, 3..=11 bits, 4 with no rows) and saturates where they overflowed; `opened_bits` is ruling 12's W8 floor and the
/// arm run's fixed bits.
#[test]
fn dr6_one_partition_rule() {
    use gravity_abi::breaker::{opened_bits, partition_bits, PARTITION_SLICE};
    let old = |rows: u64, record: u64| -> u8 {
        if rows == 0 {
            return 4;
        }
        let bits = (rows * record).div_ceil(512 << 10).next_power_of_two().trailing_zeros() as u8;
        bits.clamp(3, 11)
    };
    for rows in [0u64, 1, 100, 16_384, 65_536, 1 << 20, 3_000_000, 1 << 30] {
        for record in [8u64, 24, 32, 64, 200, 4096] {
            assert_eq!(partition_bits(rows, record, PARTITION_SLICE), old(rows, record), "rows {rows} record {record}");
        }
    }
    assert_eq!(partition_bits(u64::MAX, u64::MAX, PARTITION_SLICE), 11, "saturates at T1_MAX_PARTS' bits");
    assert_eq!(partition_bits(1 << 20, 64, 2 << 20), 5, "a larger slice, fewer partitions");
    assert_eq!((opened_bits(3, 0, false), opened_bits(3, 7, false), opened_bits(3, 8, false), opened_bits(9, 8, false)), (3, 3, 6, 9));
    assert_eq!((opened_bits(3, 1, true), opened_bits(11, 16, true)), (6, 6), "an arm run opens at ARM_RUN_PBITS");
}

/// DR-6 step 2: the run-time bind. `machine_bits` re-cuts the reference bits to a machine slice exactly as
/// `partition_bits` would have cut the rows (where neither clamps) and is the identity on the reference slice;
/// `granted_bits` keeps four halved blocks per chain beside the table inside the share, never under 3 bits, and is the
/// identity with no share or a share that holds the pool.
#[test]
fn dr6_bits_bound_to_the_machine_and_the_grant() {
    use gravity_abi::breaker::{granted_bits, machine_bits, partition_bits, BLOCK_BYTES, PARTITION_SLICE};
    for rows in [1u64 << 14, 1 << 17, 1 << 20, 3_000_000] {
        for record in [24u64, 64, 200] {
            let r = partition_bits(rows, record, PARTITION_SLICE);
            assert_eq!(machine_bits(r, PARTITION_SLICE), r);
            assert_eq!(machine_bits(r, 0), r);
            for slice in [128u64 << 10, 256 << 10, 1 << 20, 2 << 20] {
                let m = partition_bits(rows, record, slice);
                if (4..=10).contains(&r) && (4..=10).contains(&m) {
                    assert_eq!(machine_bits(r, slice), m, "rows {rows} record {record} slice {slice}");
                }
            }
        }
    }
    assert_eq!((machine_bits(3, 64 << 20), machine_bits(11, 64 << 10)), (3, 11), "clamped as partition_bits is");
    let block = BLOCK_BYTES as u64;
    assert_eq!(granted_bits(9, 0, 512 << 10), 9, "no share known");
    assert_eq!(granted_bits(9, (block << 9) + (512 << 10), 512 << 10), 9, "a share that holds the pool");
    assert_eq!(granted_bits(9, (block << 9) + (512 << 10) - 1, 512 << 10), 8, "one byte short: half the partitions");
    assert_eq!(granted_bits(11, 4 << 20, 512 << 10), 6, "3.5 MiB of pool: 64 chains of 4 x 8 KiB");
    assert_eq!(granted_bits(11, 64 << 10, 512 << 10), 3, "never under 3 bits");
    assert_eq!(granted_bits(2, 64 << 10, 0), 2, "never raises the bits");
}

/// B3-17: `fit_bits`, the floor under the share cap below the wide width: the fewest bits whose partition table of twice its expected
/// groups (a power of two of records) fits the share, between `PARTITION_MIN_BITS` and the bits.
#[test]
fn fit_bits_floors_the_cap_where_a_partition_still_fits_the_share() {
    use gravity_abi::breaker::{fit_bits, granted_bits};
    assert_eq!(fit_bits(9, 0, 64, 1 << 20), 9, "no estimate: no cap");
    assert_eq!(fit_bits(9, 1 << 20, 64, 0), 9, "no share known");
    assert_eq!(fit_bits(9, 1 << 20, 0, 1 << 20), 9, "no record size");
    assert_eq!((fit_bits(3, 1 << 22, 64, 1), fit_bits(2, 1 << 22, 64, 1)), (3, 2), "never moves bits at or under the floor");
    assert_eq!(fit_bits(9, 1 << 16, 64, 1 << 20), 3, "8192 groups a partition at 3 bits: 16384 x 64 B = 1 MiB fits");
    assert_eq!(fit_bits(11, 1 << 22, 64, 2 << 20), 8, "4M groups in 2 MiB: 16384 a partition at 8 bits");
    assert_eq!(fit_bits(11, 1 << 22, 64, (2 << 20) - 1), 9, "one byte short: twice the partitions");
    assert_eq!(fit_bits(6, 1 << 22, 64, 2 << 20), 6, "never past the bits");
    assert_eq!(fit_bits(9, 3 << 20, 64, 2 << 20), 8, "3M groups: 2 x 12288 rounds up to 32768 records, 2 MiB, at 8 bits");
    assert_eq!(fit_bits(5, 5, 64, 64), 5, "a partition of one group that never fits: the bits");
    assert_eq!(fit_bits(11, u64::MAX, 1 << 10, 1 << 20), 11, "saturating: no estimate overflows");
    let w0 = |bits: u8, groups: u64, rec: u64, share: u64| granted_bits(bits, share, 512 << 10).max(fit_bits(bits, groups, rec, share));
    assert_eq!(w0(11, 1 << 22, 64, 4 << 20), 7, "4M groups in 4 MiB: the cap's 6 bits floored at 7");
    assert_eq!(w0(11, 1 << 14, 64, 4 << 20), 6, "16K groups fit at 3 bits: the cap's 6");
    assert_eq!(w0(11, 0, 64, 4 << 20), 11, "no estimate: no cap at W0");
}

/// B3-16: the machine slice the kernels' fold opens read is the reference's until the driver sets it, and a set
/// of 0 restores it.
#[test]
fn an_unset_machine_slice_reads_the_reference_slice() {
    use gravity_abi::breaker::{machine_slice, set_machine_slice, PARTITION_SLICE};
    assert_eq!(machine_slice(), PARTITION_SLICE, "unset");
    set_machine_slice(256 << 10);
    assert_eq!(machine_slice(), 256 << 10);
    set_machine_slice(0);
    assert_eq!(machine_slice(), PARTITION_SLICE, "0 restores the reference's");
}

/// The tests whose flagged merges raise share `KEY_TIES`, one process-wide counter: the test that counts it and the one
/// that raises beside it run one at a time.
static KEY_TIES_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// NEW-numeric-1 (N11 (ii')): a `LAYOUT_KEY_TIES` layout's merge raises (`Rec::fold`'s error, counted in `KEY_TIES`) when
/// two valid extremes tie on value (`>> 4`) at two dscales, and only then; an unflagged layout merges its words as before.
#[test]
fn a_key_layout_raises_where_its_extremes_tie_at_two_dscales() {
    use gravity_abi::breaker::{KEY_TIES, LAYOUT_KEY_TIES};
    use core::sync::atomic::Ordering;
    let _serial = KEY_TIES_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut l = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count, SlotKind::MinI64, SlotKind::MaxI64]);
    l.flags |= LAYOUT_KEY_TIES;
    let key = |v: i64, ds: i64| v << 4 | ds;
    let merge = |l: &RecordLayout, (lo, hi): (i64, i64), (lo2, hi2): (i64, i64), empty: bool| {
        let (mut a, mut b) = (vec![0u64; l.size() as usize / 8], vec![0u64; l.size() as usize / 8]);
        let (ra, rb) = unsafe { (Rec::at(a.as_mut_ptr() as *mut u8, l), Rec::at(b.as_mut_ptr() as *mut u8, l)) };
        ra.open(&[7]);
        rb.open(&[7]);
        if !empty {
            ra.min_i64(1, lo);
            ra.max_i64(2, hi);
        }
        rb.min_i64(1, lo2);
        rb.max_i64(2, hi2);
        let r = ra.fold(&rb);
        (r.is_ok(), ra.slot(1), ra.slot(2))
    };
    let before = KEY_TIES.load(Ordering::Relaxed);
    // no tie: the extremes as the word order picks them
    assert_eq!(merge(&l, (key(10, 1), key(20, 1)), (key(5, 2), key(30, 2)), false), (true, Some(AggSlot::MinI64(key(5, 2))), Some(AggSlot::MaxI64(key(30, 2)))));
    // a tie at one dscale is one value's one text: no raise
    assert_eq!(merge(&l, (key(10, 1), key(20, 1)), (key(10, 1), key(20, 1)), false).0, true);
    // the destination empty: it takes the source, nothing to tie
    assert_eq!(merge(&l, (0, 0), (key(10, 2), key(20, 2)), true), (true, Some(AggSlot::MinI64(key(10, 2))), Some(AggSlot::MaxI64(key(20, 2)))));
    // 1.0 against 1.00 at the min, then at the max (a negative value's key too): the merge cannot know the later row
    assert!(!merge(&l, (key(10, 1), key(20, 1)), (key(10, 2), key(30, 2)), false).0);
    assert!(!merge(&l, (key(5, 1), key(20, 1)), (key(10, 2), key(20, 2)), false).0);
    assert!(!merge(&l, (key(-10, 1), key(20, 1)), (key(-10, 3), key(20, 1)), false).0);
    assert_eq!(KEY_TIES.load(Ordering::Relaxed) - before, 3, "each raised merge counted once");
    // the same words in an unflagged layout: plain word extremes, 16 and 17 never a tie
    let plain = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count, SlotKind::MinI64, SlotKind::MaxI64]);
    assert_eq!(merge(&plain, (16, 16), (17, 17), false), (true, Some(AggSlot::MinI64(16)), Some(AggSlot::MaxI64(17))));
}

/// NEW-numeric-1 (P): `Rec::fold_newer` (the source holds the later rows) takes the source's extreme where the two tie
/// on value (`>> 4`) at two dscales, as numeric_smaller / numeric_larger return their second argument, and never
/// raises; a value past the held one moves it as `fold` does; an unflagged layout folds its words as `fold`.
#[test]
fn a_key_layout_takes_the_newer_member_of_a_tie() {
    use gravity_abi::breaker::LAYOUT_KEY_TIES;
    let mut l = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count, SlotKind::MinI64, SlotKind::MaxI64]);
    l.flags |= LAYOUT_KEY_TIES;
    let key = |v: i64, ds: i64| v << 4 | ds;
    let merge = |l: &RecordLayout, (lo, hi): (i64, i64), (lo2, hi2): (i64, i64), empty: bool| {
        let (mut a, mut b) = (vec![0u64; l.size() as usize / 8], vec![0u64; l.size() as usize / 8]);
        let (ra, rb) = unsafe { (Rec::at(a.as_mut_ptr() as *mut u8, l), Rec::at(b.as_mut_ptr() as *mut u8, l)) };
        ra.open(&[7]);
        rb.open(&[7]);
        if !empty {
            ra.min_i64(1, lo);
            ra.max_i64(2, hi);
        }
        rb.min_i64(1, lo2);
        rb.max_i64(2, hi2);
        let r = ra.fold_newer(&rb);
        (r.is_ok(), ra.slot(1), ra.slot(2))
    };
    // 1.0 against a newer 1.00 at both ends, and a negative value: the newer member, no raise
    assert_eq!(merge(&l, (key(10, 1), key(20, 1)), (key(10, 2), key(20, 3)), false), (true, Some(AggSlot::MinI64(key(10, 2))), Some(AggSlot::MaxI64(key(20, 3)))));
    assert_eq!(merge(&l, (key(-10, 3), key(20, 2)), (key(-10, 1), key(20, 1)), false), (true, Some(AggSlot::MinI64(key(-10, 1))), Some(AggSlot::MaxI64(key(20, 1)))));
    // no tie: the extremes as the values order them, whichever is newer
    assert_eq!(merge(&l, (key(5, 1), key(30, 1)), (key(10, 2), key(20, 2)), false), (true, Some(AggSlot::MinI64(key(5, 1))), Some(AggSlot::MaxI64(key(30, 1)))));
    assert_eq!(merge(&l, (key(10, 1), key(20, 1)), (key(5, 2), key(30, 2)), false), (true, Some(AggSlot::MinI64(key(5, 2))), Some(AggSlot::MaxI64(key(30, 2)))));
    // the destination empty: it takes the source
    assert_eq!(merge(&l, (0, 0), (key(10, 2), key(20, 2)), true), (true, Some(AggSlot::MinI64(key(10, 2))), Some(AggSlot::MaxI64(key(20, 2)))));
    // unflagged: plain words, 16 and 17 no tie (the flagged fold takes the newer 17 as the min)
    let plain = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count, SlotKind::MinI64, SlotKind::MaxI64]);
    assert_eq!(merge(&plain, (16, 17), (17, 16), false), (true, Some(AggSlot::MinI64(16)), Some(AggSlot::MaxI64(17))));
    assert_eq!(merge(&l, (16, 17), (17, 16), false), (true, Some(AggSlot::MinI64(17)), Some(AggSlot::MaxI64(16))));
}

/// N13: numeric payloads (NumericData after the varlena header) for the side tests: the short form, the long form, and the
/// specials' headers.
fn num_short(neg: bool, weight: i32, ds: u16, digits: &[u16]) -> Vec<u8> {
    let h = 0x8000 | if neg { 0x2000 } else { 0 } | (ds & 0x3F) << 7 | if weight < 0 { 0x40 } else { 0 } | (weight as u16 & 0x3F);
    core::iter::once(h).chain(digits.iter().copied()).flat_map(u16::to_le_bytes).collect()
}

fn num_long(neg: bool, weight: i16, ds: u16, digits: &[u16]) -> Vec<u8> {
    let h = if neg { 0x4000 } else { 0 } | (ds & 0x3FFF);
    [h, weight as u16].into_iter().chain(digits.iter().copied()).flat_map(u16::to_le_bytes).collect()
}

/// A side copy: `pay` as a 4-byte-header varlena in an 8-aligned buffer (its address's low bit free for the tie bit).
fn side_img(pay: &[u8]) -> Vec<u64> {
    let mut v = vec![0u64; (pay.len() + 4).div_ceil(8) + 1];
    let p = v.as_mut_ptr() as *mut u8;
    unsafe {
        (p as *mut u32).write_unaligned((((pay.len() + 4) as u32) << 2).to_le());
        core::ptr::copy_nonoverlapping(pay.as_ptr(), p.add(4), pay.len());
    }
    v
}

/// N13: `numeric_payload_cmp` orders as numeric.c `cmp_numerics`: -Infinity < finite < +Infinity < NaN (NaN = NaN), a
/// finite pair by value whatever its form (short / long header, dscale, trailing zero digits).
#[test]
fn numeric_payloads_compare_as_cmp_numerics() {
    use core::cmp::Ordering::{Equal, Greater, Less};
    use gravity_abi::breaker::numeric_payload_cmp as cmp;
    let (nan, pinf, ninf) = (0xC000u16.to_le_bytes().to_vec(), 0xD000u16.to_le_bytes().to_vec(), 0xF000u16.to_le_bytes().to_vec());
    let one5 = num_short(false, 0, 1, &[1, 5000]);
    let cases: &[(Vec<u8>, Vec<u8>, core::cmp::Ordering)] = &[
        (one5.clone(), num_short(false, 0, 2, &[1, 5000]), Equal),     // 1.5 = 1.50
        (one5.clone(), num_long(false, 0, 1, &[1, 5000]), Equal),      // the short and long forms of one value
        (num_short(false, 0, 0, &[1, 0]), num_short(false, 0, 0, &[1]), Equal), // a trailing zero digit
        (num_short(false, 0, 0, &[2]), one5.clone(), Greater),
        (num_short(true, 0, 0, &[2]), num_short(true, 0, 0, &[1]), Less), // -2 < -1
        (num_short(false, 0, 0, &[]), num_short(false, 0, 0, &[1]), Less), // 0 < 1
        (num_short(false, 0, 0, &[]), num_short(true, 0, 0, &[1]), Greater), // 0 > -1
        (num_short(false, 1, 0, &[1]), num_short(false, 0, 0, &[9999]), Greater), // 10000 > 9999
        (num_short(false, -1, 4, &[1]), num_short(false, -1, 3, &[10]), Less), // 0.0001 < 0.001
        (num_long(false, 25, 0, &[1]), num_short(false, 0, 20, &[0, 3333, 3333, 3333, 3333, 3333]), Greater), // 1e100 > 0.333...
        (pinf.clone(), num_long(false, 25, 0, &[1]), Greater),
        (ninf.clone(), num_long(true, 25, 0, &[1]), Less),
        (nan.clone(), pinf.clone(), Greater),
        (nan.clone(), nan.clone(), Equal),
        (ninf.clone(), ninf.clone(), Equal),
    ];
    for (i, (a, b, want)) in cases.iter().enumerate() {
        assert_eq!(cmp(a, b), *want, "case {i}");
        assert_eq!(cmp(b, a), want.reverse(), "case {i} reversed");
    }
}

/// N13: `side_fold` folds a source's side word into a held one by value (the newer source taking a tie under `NEWER`, an
/// unordered fold flagging a tie of two byte forms), and composes the tie bit from the two forms' takes: both from the
/// source keep its bit, both kept keep the held bit, a form taken from the source alone holds the later member.
#[test]
fn a_side_fold_takes_by_value_and_composes_the_tie_bit() {
    use gravity_abi::breaker::{side_fold, NUM_SIDE_T as T};
    let (x, x2, y, two) = (side_img(&num_short(false, 0, 1, &[1, 5000])), side_img(&num_short(false, 0, 1, &[1, 5000])), side_img(&num_short(false, 0, 2, &[1, 5000])), side_img(&num_short(false, 0, 0, &[2])));
    let (a, a2, b, c) = (x.as_ptr() as u64, x2.as_ptr() as u64, y.as_ptr() as u64, two.as_ptr() as u64);
    // one side empty: the other's image; its tie bit as the key's take says
    assert_eq!(side_fold::<true>(0, b, false, true), (b | T, true, false), "(F, T): the side holds the later member");
    assert_eq!(side_fold::<true>(0, b | T, true, true), (b | T, true, false), "(T, T): the source's bit");
    assert_eq!(side_fold::<true>(0, b, true, true), (b, true, false), "(T, T): the source's bit (clear)");
    assert_eq!(side_fold::<true>(a | T, 0, true, true), (a, false, false), "(T, F): the key holds the later member");
    assert_eq!(side_fold::<true>(a | T, 0, false, true), (a | T, false, false), "(F, F): the held bit");
    assert_eq!(side_fold::<true>(0, 0, true, true), (0, false, false), "no image");
    // a tie of 1.5 and 1.50: the newer source's member
    assert_eq!(side_fold::<true>(a, b, false, true), (b | T, true, false));
    assert_eq!(side_fold::<true>(a, b, false, false), (b | T, true, false));
    assert_eq!(side_fold::<true>(a | T, b, true, true), (b, true, false));
    // by value, whichever is newer
    assert_eq!(side_fold::<true>(c, a, false, true), (a | T, true, false), "min: 1.5 under 2");
    assert_eq!(side_fold::<true>(a, c, false, true), (a, false, false), "min: 2 not under 1.5");
    assert_eq!(side_fold::<true>(a, c, false, false), (c | T, true, false), "max: 2 over 1.5");
    assert_eq!(side_fold::<true>(c | T, a, true, false), (c, false, false), "max: kept, the key took the source's");
    // unordered: a tie of two byte forms is flagged; one byte form is one text, nothing to order
    assert_eq!(side_fold::<false>(a, b, false, true), (a, false, true));
    assert_eq!(side_fold::<false>(a, a2, false, true), (a, false, false));
    assert_eq!(side_fold::<false>(c, a, false, true), (a | T, true, false));
}

/// N13: a `LAYOUT_KEY_TIES` record's side companion (the slot after its key) merges with its key: `fold_newer` composes
/// the tie bit from the two takes (an empty key takes the source's member), and `fold` (unordered) raises where the
/// key and the side would come from two records.
#[test]
fn a_key_layout_folds_its_side_companion_with_its_key() {
    use gravity_abi::breaker::{LAYOUT_KEY_TIES, NUM_SIDE_T as T};
    let _serial = KEY_TIES_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut l = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count, SlotKind::MinI64, SlotKind::NumSideMin]);
    l.flags |= LAYOUT_KEY_TIES;
    let key = |v: i64, ds: i64| v << 4 | ds;
    let (x, y) = (side_img(&num_short(false, 0, 1, &[1, 5000])), side_img(&num_short(false, 0, 0, &[1])));
    let (big, small) = (x.as_ptr() as u64, y.as_ptr() as u64);
    // a record of key `k` (None: none) and side word `s` (0: none)
    let fill = |buf: &mut Vec<u64>, k: Option<i64>, s: u64| {
        let r = unsafe { Rec::at(buf.as_mut_ptr() as *mut u8, &l) };
        r.open(&[7]);
        if let Some(k) = k {
            r.min_i64(1, k);
        }
        if s != 0 {
            let p = buf.as_mut_ptr() as *mut u8;
            let (o, bit) = l.valid_bit(2);
            unsafe {
                (p.add(l.slot_off(2) as usize) as *mut u64).write(s);
                *p.add(o as usize) |= 1 << bit;
            }
        }
    };
    let merge = |newer: bool, (k1, s1): (Option<i64>, u64), (k2, s2): (Option<i64>, u64)| {
        let (mut a, mut b) = (vec![0u64; l.size() as usize / 8], vec![0u64; l.size() as usize / 8]);
        fill(&mut a, k1, s1);
        fill(&mut b, k2, s2);
        let (ra, rb) = unsafe { (Rec::at(a.as_mut_ptr() as *mut u8, &l), Rec::at(b.as_mut_ptr() as *mut u8, &l)) };
        let r = if newer { ra.fold_newer(&rb) } else { ra.fold(&rb) };
        (r.is_ok(), ra.slot(1), ra.slot(2))
    };
    // the key takes the newer source's member, the side stays: the key holds the later member (bit clear)
    assert_eq!(merge(true, (Some(key(9, 1)), big | T), (Some(key(5, 0)), 0)), (true, Some(AggSlot::MinI64(key(5, 0))), Some(AggSlot::NumSide(big))));
    // the side takes the source's image, the key stays: the side holds the later member
    assert_eq!(merge(true, (Some(key(5, 0)), big), (Some(key(9, 0)), small)), (true, Some(AggSlot::MinI64(key(5, 0))), Some(AggSlot::NumSide(small | T))));
    // an empty key takes the source's member: the held side's bit clears
    assert_eq!(merge(true, (None, big | T), (Some(key(5, 0)), 0)), (true, Some(AggSlot::MinI64(key(5, 0))), Some(AggSlot::NumSide(big))));
    // both from the source: its bit
    assert_eq!(merge(true, (Some(key(9, 0)), big), (Some(key(5, 0)), small | T)), (true, Some(AggSlot::MinI64(key(5, 0))), Some(AggSlot::NumSide(small | T))));
    // neither from the source: the held bit
    assert_eq!(merge(true, (Some(key(5, 0)), small | T), (Some(key(9, 0)), big)), (true, Some(AggSlot::MinI64(key(5, 0))), Some(AggSlot::NumSide(small | T))));
    // unordered: the key from the source and the side held cannot be ordered; both from one record can
    assert!(!merge(false, (Some(key(9, 1)), big | T), (Some(key(5, 0)), 0)).0);
    assert!(merge(false, (Some(key(9, 0)), big), (Some(key(5, 0)), small | T)).0);
    assert!(merge(false, (Some(key(5, 0)), small), (Some(key(9, 0)), big)).0);
}

/// D3312: the image arena's one bump (`img_bump`): each image's start marked, so `img_extent` gives the image holding any
/// byte; past a chunk the next is pinned and linked in the chain, all naming the first.
mod img_arena {
    use gravity_abi::breaker::{img_bump, img_chunk, img_data_off, img_extent, img_open_chunk, T1_IMAGE_CHUNK};

    #[test]
    fn a_bumped_images_extent_is_found_from_any_of_its_bytes() {
        let mut pins: Vec<Vec<u64>> = Vec::new();
        let mut pin = |bytes: u64| {
            let mut v = vec![0u64; bytes as usize / 8];
            let p = v.as_mut_ptr() as *mut u8;
            pins.push(v);
            p
        };
        let mut w = [0u64; 3];
        // SAFETY: `w` is a fresh cursor only `img_bump` bumps; `pin` hands out live zeroed words kept in `pins`
        unsafe {
            let a = img_bump(w.as_mut_ptr(), 8, &mut pin) as usize;
            let b = img_bump(w.as_mut_ptr(), 40, &mut pin) as usize;
            let c = img_bump(w.as_mut_ptr(), 16, &mut pin) as usize;
            let base = img_open_chunk(w.as_ptr());
            assert_eq!((a - base, b, c), (img_data_off(T1_IMAGE_CHUNK), a + 8, b + 40), "packed past the header and start map");
            let fill = w[0] as usize;
            assert_eq!(img_extent(base, a, fill), Some((a, b)));
            assert_eq!(img_extent(base, b + 24, fill), Some((b, c)), "an interior byte names its image");
            assert_eq!(img_extent(base, c + 8, fill), Some((c, fill)), "the last image ends at the cursor");
            // a chunk's worth more: the next chunk is pinned, doubled, and linked
            let mut last = c;
            while img_open_chunk(w.as_ptr()) == base {
                last = img_bump(w.as_mut_ptr(), 1024, &mut pin) as usize;
            }
            let next = img_open_chunk(w.as_ptr());
            assert_eq!(img_chunk(base), (next, T1_IMAGE_CHUNK, base), "the first chunk links the next and names itself first");
            assert_eq!(img_chunk(next), (0, 2 * T1_IMAGE_CHUNK, base), "the next is doubled, last, and names the first");
            assert_eq!((last - next, w[2] as usize), (img_data_off(2 * T1_IMAGE_CHUNK), 2 * T1_IMAGE_CHUNK));
        }
    }
}
