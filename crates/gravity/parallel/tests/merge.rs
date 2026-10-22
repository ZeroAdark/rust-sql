mod common;

use common::*;
use gravity_abi::Datum;
use gravity_driver::{Merge, Partial, SinkRef, State};
use gravity_parallel::{HashBuildPartial, RowsOrdered, SortedRun};
use gravity_abi::breaker::{HashBuild, BUILD_TABLE};

fn run_of(natts: u32, cap: u32, rows: &[&[Datum]]) -> Vec<u64> {
    let mut buf = vec![0u64; SortedRun::bytes(natts, cap).div_ceil(8) as usize];
    let r = unsafe { SortedRun::at(buf.as_mut_ptr() as *mut u8) };
    r.natts = natts;
    for row in rows {
        assert!(r.push(cap, row, &[0, 0]));
    }
    buf
}

#[test]
fn rows_ordered_merges_sorted_runs_and_pushes_once_at_finish() {
    let m = RowsOrdered { natts: 2, cap_rows: 8, keys: [0, 0, 0, 0], nkeys: 1 };
    let mut state = vec![0u8; 2 * SortedRun::bytes(2, 8) as usize + 8];
    let mut sink = LegSink::new(true);
    let runs = [run_of(2, 8, &[&[1, 10], &[4, 40], &[9, 90]]), run_of(2, 8, &[&[2, 20], &[3, 30]]), run_of(2, 8, &[]), run_of(2, 8, &[&[0, 0], &[9, 91]])];
    for (i, r) in runs.iter().enumerate() {
        let bytes = unsafe { core::slice::from_raw_parts(r.as_ptr() as *const u8, r.len() * 8) };
        let mut leader = State::new(&mut state, SinkRef::of(&mut sink));
        m.merge_into(&mut leader, Partial { ordinal: i as u16, bytes, rows_processed: 0 }).unwrap();
    }
    assert!(sink.rows.lock().unwrap().is_empty(), "nothing reaches the sink before finish");
    m.finish(&mut State::new(&mut state, SinkRef::of(&mut sink))).unwrap();
    let got = sink.rows.lock().unwrap().clone();
    assert_eq!(got, vec![vec![0, 0], vec![1, 10], vec![2, 20], vec![3, 30], vec![4, 40], vec![9, 90], vec![9, 91]]);
}

#[test]
fn hash_build_partials_land_in_the_leaders_table() {
    let m = HashBuildPartial;
    let mut state = vec![0u64; HashBuild::bytes(16) as usize / 8];
    // the leader region is share-armed the way the driver arms it: the merge lays its table out from the share
    unsafe { gravity_abi::breaker::HashAgg::arm(state.as_mut_ptr() as *mut u8, HashBuild::bytes(16)) };
    let mut sink = LegSink::new(true);
    for (ordinal, rows) in [[(1, 10), (2, 20)], [(1, 11), (3, 30)]].iter().enumerate() {
        let mut part = vec![0u64; HashBuild::bytes(16) as usize / 8];
        let t = unsafe { HashBuild::at(part.as_mut_ptr() as *mut u8, 16, BUILD_TABLE) };
        for (k, v) in rows {
            assert!(t.insert(*k, *v));
        }
        let bytes = unsafe { core::slice::from_raw_parts(part.as_ptr() as *const u8, part.len() * 8) };
        m.merge_into(&mut State::new(unsafe { core::slice::from_raw_parts_mut(state.as_mut_ptr() as *mut u8, state.len() * 8) }, SinkRef::of(&mut sink)), Partial { ordinal: ordinal as u16, bytes, rows_processed: 0 }).unwrap();
    }
    let t = unsafe { HashBuild::at(state.as_mut_ptr() as *mut u8, 16, BUILD_TABLE) };
    let mut hits: Vec<i64> = t.probe(1).map(|e| e.payload).collect();
    hits.sort();
    assert_eq!(hits, vec![10, 11]);
    assert_eq!(t.len, 4);
}

#[test]
fn granule_claims_never_cross_a_part_edge() {
    use gravity_parallel::{ClaimSize, GranuleRange, MorselSource};
    let mut g = GranuleRange::over_parts(20, ClaimSize::Fixed(4), &[0, 5, 12]);
    assert!(!g.whole_boundary_claims(), "the cursor defaults to sizer-cut claims inside a part; the runner turns whole-part claims on");
    let mut got = Vec::new();
    while let Some(r) = g.claim() {
        got.push((r.lo, r.hi));
    }
    assert_eq!(got, vec![(0, 4), (4, 5), (5, 9), (9, 12), (12, 16), (16, 20)]);
    assert_eq!((g.next_boundary_after(0), g.next_boundary_after(5), g.next_boundary_after(13)), (5, 12, 20));
    g.reset();
    assert_eq!(g.claim(), Some(gravity_abi::MorselRange::new(0, 4)));
    g.reset_to(6, &[0, 3]);
    let mut again = Vec::new();
    while let Some(r) = g.claim() {
        again.push((r.lo, r.hi));
    }
    assert_eq!(again, vec![(0, 3), (3, 6)], "the next execution's geometry replaces the last");
}

#[test]
fn top_k_partials_merge_into_a_bounded_run_and_prove_a_bound() {
    use gravity_parallel::{ClaimSize, GranuleRange, MorselSource, TopKPartial};
    use gravity_abi::breaker::TopK;
    let (n, natts) = (4u32, 2u32);
    let bytes = TopK::bytes(n, natts) as usize;
    let mut a = vec![0u8; bytes];
    let mut b = vec![0u8; bytes];
    let mut leader = vec![0u8; bytes];
    let ra = unsafe { TopK::at(a.as_mut_ptr(), n, natts) };
    for k in [9i64, 3, 7, 5, 11, 1] {
        ra.insert(&[k as u64, (k * 10) as u64], &[0, 0]);
    }
    assert_eq!((ra.len, ra.bound()), (4, Some(7)), "the run keeps the four smallest keys, bounded by the fourth");
    let rb = unsafe { TopK::at(b.as_mut_ptr(), n, natts) };
    for k in [4i64, 8] {
        rb.insert(&[k as u64, 0], &[0, 0]);
    }
    assert_eq!(rb.bound(), None, "a run that is not full proves nothing");
    let m = TopKPartial { n, natts };
    let mut sink = LegSink::new(true);
    let mut st = State::new(&mut leader, SinkRef::of(&mut sink));
    m.merge_into(&mut st, Partial { ordinal: 0, bytes: &a, rows_processed: 0 }).unwrap();
    m.merge_into(&mut st, Partial { ordinal: 1, bytes: &b, rows_processed: 0 }).unwrap();
    let l = unsafe { TopK::at(leader.as_mut_ptr(), n, natts) };
    let keys: Vec<i64> = (0..l.len as usize).map(|i| l.key_of(i)).collect();
    assert_eq!(keys, vec![1, 3, 4, 5], "the leader's run is the four smallest across both partials");
    assert_eq!(l.row(1).0[1], 30);
    assert_eq!(m.bound(Partial { ordinal: 0, bytes: &a, rows_processed: 0 }), Some(7));
    // the ordered cursor: units in key-minimum order, boundaries at part changes, the bound stops the hand-out
    let order = [2u32, 0, 5, 3, 1, 4];
    let mins = [1i64, 2, 2, 6, 9, 20];
    let cur = GranuleRange::ordered(&order, &mins, |g| (g / 3) as usize, ClaimSize::Fixed(4));
    assert_eq!(cur.total(), 6);
    assert_eq!(cur.cursor().granule_of(2), 5);
    let c1 = cur.claim().unwrap();
    assert_eq!((c1.lo, c1.hi), (0, 2), "units 0-1 are part 0 (granules 2, 0); unit 2 is part 1");
    cur.observe_bound(5);
    assert!(cur.beyond_bound(3) && !cur.beyond_bound(2));
    let c2 = cur.claim().unwrap();
    assert_eq!((c2.lo, c2.hi), (2, 4), "unit 2 (granule 5, min 2) still runs; the claim runs to its part edge (unit 3 rides along)");
    assert!(cur.claim().is_none(), "unit 4 has min 9 > bound 5: the hand-out ends");
    assert_eq!(cur.remaining(), 0);
}

/// D3024 (b): a top whose slot holds no bytes (a sorted wide emit's top keeps its records in the emit's array, its claims'
/// runs in the run table): its partials fold nothing into the leader and prove no bound.
#[test]
fn top_k_partials_of_no_bytes_fold_nothing() {
    use gravity_parallel::TopKPartial;
    let m = TopKPartial { n: 4, natts: 2 };
    let mut leader: Vec<u8> = Vec::new();
    let mut sink = LegSink::new(true);
    let mut st = State::new(&mut leader, SinkRef::of(&mut sink));
    for ordinal in 0..3 {
        m.merge_into(&mut st, Partial { ordinal, bytes: &[], rows_processed: 0 }).unwrap();
    }
    assert_eq!(m.bound(Partial { ordinal: 0, bytes: &[], rows_processed: 0 }), None);
}

mod record {
    use super::*;
    use gravity_parallel::PlainAggPartial;
    use gravity_abi::breaker::{HashAgg, Rec, RecordLayout, SlotKind};

    const ALL: [SlotKind; 8] = [SlotKind::Count, SlotKind::SumI64, SlotKind::SumI128, SlotKind::MinI64, SlotKind::MaxI64, SlotKind::AvgI128, SlotKind::SumF64, SlotKind::MaxCode];

    fn region(layout: &RecordLayout, cap: u32) -> Vec<u64> {
        vec![0u64; HashAgg::bytes(layout, cap).div_ceil(8) as usize]
    }

    fn bytes(v: &[u64]) -> &[u8] {
        unsafe { core::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 8) }
    }

    fn merge_all(m: &dyn Merge, leader: &mut [u64], partials: &[Vec<u64>]) {
        let mut sink = LegSink::new(true);
        for (i, p) in partials.iter().enumerate() {
            let lb = unsafe { core::slice::from_raw_parts_mut(leader.as_mut_ptr() as *mut u8, leader.len() * 8) };
            m.merge_into(&mut State::new(lb, SinkRef::of(&mut sink)), Partial { ordinal: i as u16, bytes: bytes(p), rows_processed: 0 }).unwrap();
        }
    }


    /// An implicit count's word is `1 + rows` (`open` seeds the 1): W empty partials must not open the leader's record — a plain
    /// `sum(NOT NULL column)` over no row rendered 0 for NULL under every parallel width (the gate's e2e tpch Q6, 2026-09-22).
    #[test]
    fn empty_partials_do_not_open_an_implicit_count() {
        let l = RecordLayout::new(&[], &[SlotKind::SumI64]);
        assert!(l.flags & gravity_abi::breaker::LAYOUT_IMPLICIT_COUNT != 0, "the fixture's layout carries the implicit count");
        let limit = HashAgg::bytes(&l, 1);
        let partial = |rows: i64, v: i64| {
            let mut p = region(&l, 1);
            let r = unsafe { HashAgg::at(p.as_mut_ptr() as *mut u8, &l, 1, limit) }.single();
            r.open(&[]);
            if rows > 0 {
                r.add_i64(0, v);
                r.add_count(1, rows);
            }
            p
        };
        let mut leader = region(&l, 1);
        unsafe { HashAgg::at(leader.as_mut_ptr() as *mut u8, &l, 1, limit) }.single().open(&[]);
        merge_all(&PlainAggPartial, &mut leader, &[partial(0, 0), partial(0, 0)]);
        let rec = unsafe { HashAgg::view(leader.as_ptr() as *const u8) }.record(0);
        assert_eq!(rec.count(1), 1, "two empty partials leave the leader's word at the seed: not opened, the sum is NULL");
        merge_all(&PlainAggPartial, &mut leader, &[partial(3, 42)]);
        let rec = unsafe { HashAgg::view(leader.as_ptr() as *const u8) }.record(0);
        assert_eq!((rec.count(1), rec.i64(0)), (4, 42), "a partial with rows brings its rows alone");
    }

    /// A slot a hash build used earlier is armed at its open (the slot is shared): a fixed-budget record merged into it keeps the
    /// fold's own limit, so the emit's `record_consumed(bytes)` re-opens the same table (a chain under a plain aggregate at W4).
    #[test]
    fn a_fixed_record_merged_into_an_armed_leader_keeps_its_own_limit() {
        let l = RecordLayout::new(&[], &[SlotKind::SumI64]);
        let limit = HashAgg::bytes(&l, 1);
        let partial = |v: i64| {
            let mut p = vec![0u64; 4096];
            let r = unsafe { HashAgg::at(p.as_mut_ptr() as *mut u8, &l, 1, limit) }.single();
            r.open(&[]);
            r.add_i64(0, v);
            r.add_count(1, 1);
            p
        };
        let mut leader = vec![0u64; 4096];
        unsafe { HashAgg::arm(leader.as_mut_ptr() as *mut u8, 4096 * 8) };
        merge_all(&PlainAggPartial, &mut leader, &[partial(5), partial(7)]);
        assert_eq!(unsafe { HashAgg::view(leader.as_ptr() as *const u8) }.limit, limit, "the record's limit, not the arm");
        let rec = unsafe { HashAgg::at(leader.as_mut_ptr() as *mut u8, &l, 1, limit) }.single();
        assert_eq!((rec.count(1), rec.i64(0)), (3, 12));
    }

    #[test]
    fn float_sums_fold_in_worker_ordinal_order() {
        let l = RecordLayout::new(&[], &[SlotKind::SumF64]);
        let limit = HashAgg::bytes(&l, 1);
        let vals = [1e16, 1.0, -1e16, 1.0];
        let partials: Vec<Vec<u64>> = vals
            .iter()
            .map(|v| {
                let mut p = region(&l, 1);
                unsafe { HashAgg::at(p.as_mut_ptr() as *mut u8, &l, 1, limit) }.single().add_f64(0, *v).expect("finite");
                p
            })
            .collect();
        let mut leader = region(&l, 1);
        merge_all(&PlainAggPartial, &mut leader, &partials);
        let got = unsafe { HashAgg::view(leader.as_ptr() as *const u8) }.record(0).f64(0);
        let ordinal: f64 = vals.iter().fold(0.0, |a, v| a + v);
        let reversed: f64 = vals.iter().rev().fold(0.0, |a, v| a + v);
        assert_ne!(ordinal, reversed, "the fixture's sums are order-sensitive");
        assert_eq!(got, ordinal, "the leader folds partials in ordinal order (decision 10)");
    }

    /// D252: bit_and / bit_or / bit_xor partials at W8: NULL partials (a worker with no row) are skipped, the rest merge by
    /// `&`, `|`, `^`; all-NULL partials leave the slot NULL.
    #[test]
    fn bit_partials_merge_at_w8_skipping_null_workers() {
        let l = RecordLayout::new(&[], &[SlotKind::BitAnd, SlotKind::BitOr, SlotKind::BitXor]);
        let limit = HashAgg::bytes(&l, 1);
        let vals: [Option<i64>; 8] = [Some(0b1111), None, Some(0b0110), Some(0b0111), None, None, Some(-2), None];
        let parts = |vs: &[Option<i64>]| -> Vec<Vec<u64>> {
            vs.iter().map(|v| {
                let mut p = region(&l, 1);
                let r = unsafe { HashAgg::at(p.as_mut_ptr() as *mut u8, &l, 1, limit) }.single();
                if let Some(v) = v {
                    (0..3).for_each(|j| r.fold_bits(j, *v));
                }
                p
            }).collect()
        };
        let mut leader = region(&l, 1);
        merge_all(&PlainAggPartial, &mut leader, &parts(&vals));
        let rec = unsafe { HashAgg::view(leader.as_ptr() as *const u8) }.record(0);
        let seen: Vec<i64> = vals.iter().flatten().copied().collect();
        assert_eq!(rec.i64(0), seen.iter().fold(-1, |a, v| a & v), "bit_and");
        assert_eq!(rec.i64(1), seen.iter().fold(0, |a, v| a | v), "bit_or");
        assert_eq!(rec.i64(2), seen.iter().fold(0, |a, v| a ^ v), "bit_xor");
        let mut empty = region(&l, 1);
        merge_all(&PlainAggPartial, &mut empty, &parts(&[None; 8]));
        let rec = unsafe { HashAgg::view(empty.as_ptr() as *const u8) }.record(0);
        assert!((0..3).all(|j| !rec.valid(j)), "every worker NULL: the aggregate is NULL");
    }

    /// D244: the leader's merge raises PG's overflow (22003) where only the combination of finite partials overflows:
    /// sum(float8) by `float8_pl` at W8 and W16; NaN / Infinity propagate. avg(float8)'s `float8_combine` overflow taints
    /// the state instead (D432: Sxx = +Inf), which the final raises unless Sx is NaN (lowerv4 class_float).
    #[test]
    fn float_partials_that_overflow_only_combined_raise_at_w8_and_w16() {
        let first_err = |l: &RecordLayout, fill: &dyn Fn(Rec<'_>, usize)| -> Option<gravity_driver::Error> {
            let limit = HashAgg::bytes(l, 1);
            let parts: Vec<Vec<u64>> = (0..W).map(|i| {
                let mut p = region(l, 1);
                fill(unsafe { HashAgg::at(p.as_mut_ptr() as *mut u8, l, 1, limit) }.single(), i);
                p
            }).collect();
            let mut leader = region(l, 1);
            let mut sink = LegSink::new(true);
            parts.iter().enumerate().find_map(|(i, p)| {
                let lb = unsafe { core::slice::from_raw_parts_mut(leader.as_mut_ptr() as *mut u8, leader.len() * 8) };
                PlainAggPartial.merge_into(&mut State::new(lb, SinkRef::of(&mut sink)), Partial { ordinal: i as u16, bytes: bytes(p), rows_processed: 0 }).err()
            })
        };
        const W: usize = 16;
        let raised = |e: Option<gravity_driver::Error>| matches!(e, Some(gravity_driver::Error::Raised(m)) if m == gravity_abi::breaker::FloatOverflow::MASK);
        let sum = RecordLayout::new(&[], &[SlotKind::SumF64]);
        let avg = RecordLayout::new(&[], &[SlotKind::F64Accum]);
        for w in [8usize, 16] {
            let v = f64::MAX / (w as f64 / 2.0 + 0.5);
            assert!(raised(first_err(&sum, &|r, i| if i < w { r.add_f64(0, v).unwrap() })), "W{w}: sum");
            // avg over {1e200, -1e200}: the first and the last worker's rows, the others empty (N = 0 takes the other side)
            assert!(first_err(&avg, &|r, i| if i == 0 || i == w - 1 { r.combine_f64_accum(0, [1.0, if i == 0 { 1e200 } else { -1e200 }, 0.0]).unwrap() }).is_none(), "W{w}: avg's overflow is the final's (D432)");
            assert!(first_err(&sum, &|r, i| if i < w { r.add_f64(0, if i == 0 { f64::INFINITY } else { v }).unwrap() }).is_none(), "W{w}: an infinite input is no overflow");
            assert!(first_err(&avg, &|r, i| if i < w { r.combine_f64_accum(0, [1.0, if i == 1 { f64::NAN } else { 1.0 }, 0.0]).unwrap() }).is_none(), "W{w}: NaN propagates");
        }
    }

}

mod topk_codes {
    use super::*;
    use gravity_parallel::{order_topk, TopKPartial, TOPK_TIES};
    use gravity_abi::breaker::{KeyCell, TopK};

    /// A fake dictionary: part p's code c is the text `WORDS[p][c]` as a 4B-U varlena in a leaked buffer.
    const WORDS: [&[&str]; 2] = [&["", "apple", "pear"], &["banana", "kiwi", "pear"]];

    fn varlena(s: &str) -> u64 {
        let mut v = Vec::with_capacity(4 + s.len());
        v.extend_from_slice(&(((s.len() + 4) as u32) << 2).to_le_bytes());
        v.extend_from_slice(s.as_bytes());
        Box::leak(v.into_boxed_slice()).as_ptr() as u64
    }

    fn text_of(datum: u64) -> String {
        let p = datum as *const u8;
        let hdr = unsafe { u32::from_le_bytes([*p, *p.add(1), *p.add(2), *p.add(3)]) };
        let len = (hdr >> 2) as usize - 4;
        String::from_utf8(unsafe { core::slice::from_raw_parts(p.add(4), len) }.to_vec()).unwrap()
    }

    #[test]
    fn ties_runs_merge_and_the_leader_orders_them_by_the_rendered_bytes() {
        let (n, cap, natts) = (3u32, 8u32, 2u32);
        let bytes = TopK::bytes_cap(cap, natts) as usize;
        let mut mk = |rows: &[(i64, u32, u32)]| {
            let mut buf = vec![0u8; bytes];
            let t = unsafe { TopK::at_cap(buf.as_mut_ptr(), n, cap, natts, TOPK_TIES) };
            for &(k, part, code) in rows {
                assert_eq!(t.insert_ties(&[k as u64, KeyCell::code_word(part, code)], &[0, 0]), Some(true));
            }
            buf
        };
        // worker 0 over part 0, worker 1 over part 1: key 5 ties across parts with different byte orders
        let a = mk(&[(5, 0, 2), (5, 0, 1), (9, 0, 0), (5, 0, 0)]);
        let b = mk(&[(5, 1, 0), (7, 1, 1), (5, 1, 2)]);
        let m = TopKPartial { n, natts };
        assert_eq!(m.bound(Partial { ordinal: 0, bytes: &a, rows_processed: 0 }), Some(5));
        let mut leader = vec![0u8; bytes];
        let mut sink = LegSink::new(true);
        let mut st = State::new(&mut leader, SinkRef::of(&mut sink));
        m.merge_into(&mut st, Partial { ordinal: 0, bytes: &a, rows_processed: 0 }).unwrap();
        m.merge_into(&mut st, Partial { ordinal: 1, bytes: &b, rows_processed: 0 }).unwrap();
        let run = unsafe { TopK::at_cap(leader.as_mut_ptr(), n, cap, natts, TOPK_TIES) };
        assert_eq!((0..run.len as usize).map(|i| run.key_of(i)).collect::<Vec<_>>(), vec![5, 5, 5, 5, 5], "every tie at the bound survives the merge; 9 and 7 arrive past the leader's bound and are refused");
        order_topk(&gravity_driver::Reservation::unadmitted(gravity_driver::Grant::of(64 << 20)), run, 0b10, Some(1), |part, lane, code| {
            assert_eq!(lane, 1);
            Ok((varlena(WORDS[part as usize][code as usize]), WORDS[part as usize][code as usize].len() as u32))
        })
        .unwrap();
        let rows: Vec<(i64, String)> = (0..run.len as usize).map(|i| (run.key_of(i), text_of(run.row(i).0[1]))).collect();
        assert_eq!(rows, vec![(5, "".into()), (5, "apple".into()), (5, "banana".into())], "ORDER BY key, text LIMIT 3 across both parts' dictionaries");
    }

    /// A ties run of 600 rows over three distinct secondary words and a third code column: the
    /// secondary renders once per distinct word, the third column only for the n survivors.
    #[test]
    fn a_ties_run_renders_each_distinct_word_once_and_the_other_columns_for_the_survivors() {
        let (n, cap, natts) = (4u32, 1024u32, 3u32);
        let mut buf = vec![0u8; TopK::bytes_cap(cap, natts) as usize];
        let run = unsafe { TopK::at_cap(buf.as_mut_ptr(), n, cap, natts, TOPK_TIES) };
        for i in 0..600u32 {
            assert_eq!(run.insert_ties(&[7, KeyCell::code_word(1, i % 3), KeyCell::code_word(0, i % 2 + 1)], &[0, 0, 0]), Some(true));
        }
        let mut calls = [0u32; 3];
        order_topk(&gravity_driver::Reservation::unadmitted(gravity_driver::Grant::of(64 << 20)), run, 0b110, Some(1), |part, lane, code| {
            calls[lane as usize] += 1;
            Ok((varlena(WORDS[part as usize][code as usize]), 0))
        })
        .unwrap();
        assert_eq!(run.len, 4);
        let words: Vec<String> = (0..4).map(|i| text_of(run.row(i).0[1])).collect();
        assert_eq!(words, vec!["banana", "banana", "banana", "banana"], "the 200 banana ties fill the four");
        assert!((0..4).all(|i| ["apple", "pear"].contains(&text_of(run.row(i).0[2]).as_str())));
        assert_eq!((calls[0], calls[1]), (0, 3), "three secondary words over 600 rows");
        assert!((1..=2).contains(&calls[2]), "the third column renders for the four survivors' distinct words only: {calls:?}");
    }

    #[test]
    fn rendering_without_a_secondary_key_keeps_the_key_order_and_cuts_to_n() {
        let (n, natts) = (2u32, 2u32);
        let mut buf = vec![0u8; TopK::bytes(n, natts) as usize];
        let run = unsafe { TopK::at(buf.as_mut_ptr(), n, natts) };
        run.insert(&[3, KeyCell::code_word(1, 1)], &[0, 0]);
        run.insert(&[1, KeyCell::code_word(0, 2)], &[0, 0]);
        run.insert(&[2, 0], &[0, 1]);
        order_topk(&gravity_driver::Reservation::unadmitted(gravity_driver::Grant::of(64 << 20)), run, 0b10, None, |part, _, code| Ok((varlena(WORDS[part as usize][code as usize]), 0))).unwrap();
        assert_eq!(run.len, 2);
        assert_eq!((run.key_of(0), text_of(run.row(0).0[1])), (1, "pear".into()));
        assert_eq!((run.key_of(1), run.row(1).1[1]), (2, 1), "a NULL code cell is left NULL, not rendered");
    }
}

/// A spool-encoded row without images: the length word, the values, one null word.
fn stored_row(values: &[u64]) -> Vec<u8> {
    let mut out = ((values.len() as u64) + 2).to_ne_bytes().to_vec();
    for v in values {
        out.extend(v.to_ne_bytes());
    }
    out.extend(0u64.to_ne_bytes());
    out
}

/// A share-armed build region with a store of `parts` slices, its table holding `keys` and their rows.
fn stored_partial(total: usize, parts: u16, keys: &[i32]) -> Vec<u64> {
    use gravity_abi::buildrows::RowStore;
    let mut buf = vec![0u64; total / 8];
    let p = buf.as_mut_ptr() as *mut u8;
    unsafe { gravity_abi::breaker::HashAgg::arm(p, total as u32) };
    let s = unsafe { RowStore::carve(p, 32, parts) }.expect("room");
    let t = unsafe { HashBuild::at_share_sized(p, BUILD_TABLE, 16) };
    for &k in keys {
        let off = s.append(s.part_for(k), &stored_row(&[k as u64, k as u64 * 100])).unwrap();
        assert!(t.insert(k, off as i64));
    }
    buf
}

fn check_stored(leader: *const u8, keys: &[i32]) {
    use gravity_abi::buildrows::RowStore;
    let t = unsafe { HashBuild::view(leader) };
    let s = unsafe { RowStore::of(leader) }.expect("the leader's store");
    for &k in keys {
        let hits: Vec<Vec<u8>> = t.probe(k).map(|e| s.at(e.payload as u32).to_vec()).collect();
        let want = stored_row(&[k as u64, k as u64 * 100]);
        assert_eq!(hits.iter().filter(|h| **h == want).count(), keys.iter().filter(|x| **x == k).count(), "key {k}");
    }
    assert_eq!(t.len as usize, keys.len());
}

#[test]
fn stored_build_partials_merge_serially_with_their_offsets_rebased() {
    let m = HashBuildPartial;
    let total = 32usize << 10;
    let parts: Vec<Vec<u64>> = vec![stored_partial(total, 4, &[1, 2, 7]), stored_partial(total, 4, &[1, 3, 8, 9])];
    let mut state = vec![0u64; total / 8];
    unsafe { gravity_abi::breaker::HashAgg::arm(state.as_mut_ptr() as *mut u8, total as u32) };
    let mut sink = LegSink::new(true);
    for (ordinal, part) in parts.iter().enumerate() {
        let bytes = unsafe { core::slice::from_raw_parts(part.as_ptr() as *const u8, part.len() * 8) };
        let leader = unsafe { core::slice::from_raw_parts_mut(state.as_mut_ptr() as *mut u8, state.len() * 8) };
        assert!(m.room_for(leader, Partial { ordinal: ordinal as u16, bytes, rows_processed: 0 }));
        m.merge_into(&mut State::new(leader, SinkRef::of(&mut sink)), Partial { ordinal: ordinal as u16, bytes, rows_processed: 0 }).unwrap();
    }
    check_stored(state.as_ptr() as *const u8, &[1, 2, 7, 1, 3, 8, 9]);
    assert_eq!(unsafe { gravity_abi::buildrows::RowStore::of(state.as_ptr() as *const u8) }.unwrap().len, 7);
}

#[test]
fn stored_build_partials_merge_by_partition_jobs_with_their_offsets_rebased() {
    use gravity_driver::{PartitionPlan, Region};
    let m = HashBuildPartial;
    let total = 32usize << 10;
    let parts: Vec<Vec<u64>> = vec![stored_partial(total, 4, &[1, 2, 7, 11]), stored_partial(total, 4, &[1, 3, 8, 9, 12])];
    let partials: Vec<Partial<'_>> = parts.iter().enumerate().map(|(i, p)| Partial { ordinal: i as u16, bytes: unsafe { core::slice::from_raw_parts(p.as_ptr() as *const u8, p.len() * 8) }, rows_processed: 0 }).collect();
    let mut state = vec![0u64; total / 8];
    let leader = unsafe { core::slice::from_raw_parts_mut(state.as_mut_ptr() as *mut u8, state.len() * 8) };
    let wrong = PartitionPlan { pbits: 3, groups: 9, record: 16 };
    assert!(m.prepare_partitions(leader, wrong, &partials).is_err(), "the store's slices must be the plan's partitions");
    let plan = PartitionPlan { pbits: 2, groups: 9, record: 16 };
    m.prepare_partitions(leader, plan, &partials).unwrap();
    let region = Region { ptr: state.as_mut_ptr() as *mut u8, len: total };
    for part in 0..4u32 {
        m.merge_partition(region, part, plan, &partials).unwrap();
    }
    check_stored(state.as_ptr() as *const u8, &[1, 2, 7, 11, 1, 3, 8, 9, 12]);
    assert_eq!(unsafe { gravity_abi::buildrows::RowStore::of(state.as_ptr() as *const u8) }.unwrap().len, 9);
}

/// D4170: a hash aggregate's merge with no owner functions has no owner phase: its owner open, partition fold and round
/// close refuse by name, never kind 2, which an owner fold reads as a region too small and grows.
#[test]
fn a_hash_aggregate_merge_with_no_owner_functions_refuses_its_owner_phase_by_name() {
    use gravity_driver::{no_owner_phase, Region};
    use gravity_parallel::HashAggPartial;
    let m = HashAggPartial::new(0, None);
    let mut state = vec![0u64; 512];
    let owner = Region { ptr: state.as_mut_ptr() as *mut u8, len: state.len() * 8 };
    assert_eq!(m.open_owner(owner, 0), Err(no_owner_phase()));
    assert_eq!(m.own_partition(owner, 0, 3, &[], &[], core::ptr::null_mut(), core::ptr::null(), 1, &mut || Ok(())), Err(no_owner_phase()));
    assert_eq!(m.end_round(owner, &mut || Ok(())), Err(no_owner_phase()));
}
