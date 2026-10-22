//! V4R-277 (M6-FOLD seam): a hosted aggregate's `AggState` slot (lowerv4 face/aggcall.rs, kernels host_abi/v4/m6_agg.rs)
//! is never merged by bytes. `Rec::fold` moves the one held state of a partial into an empty record; two held states are a
//! Bug (the hosted plain merge combines them first, D806): a debug build asserts, a release one raises.
use gravity_abi::breaker::{AggSlot, FloatOverflow, Rec, RecordLayout, SlotKind, AGGSTATE_HELD};

/// Slot `j` of the record at `p` holds a state of value word `v` (its held byte set, as the host's first call leaves it).
unsafe fn hold(p: *mut u8, l: &RecordLayout, j: usize, v: u64) {
    let o = l.slot_off(j) as usize;
    (p.add(o) as *mut u64).write_unaligned(v);
    p.add(o + AGGSTATE_HELD).write(1);
}

/// A held state moves into an empty slot (its value word and flags; the slot then valid); an unheld one folds nothing;
/// two held states are a Bug (a debug assert; a release build raises) and the destination keeps its own. Both validity forms.
#[test]
fn a_held_hosted_state_moves_into_an_empty_slot_and_two_raise() {
    assert_eq!(SlotKind::AggState.bytes(), 16);
    for l in [RecordLayout::new(&[], &[SlotKind::Count, SlotKind::AggState]), RecordLayout::new_nn(&[], &[SlotKind::Count, SlotKind::AggState])] {
        let (mut ba, mut bb, mut be) = (vec![0u64; 16], vec![0u64; 16], vec![0u64; 16]);
        let (pa, pb, pe) = (ba.as_mut_ptr() as *mut u8, bb.as_mut_ptr() as *mut u8, be.as_mut_ptr() as *mut u8);
        let (ra, rb, re) = unsafe { (Rec::at(pa, &l), Rec::at(pb, &l), Rec::at(pe, &l)) };
        let word = |p: *mut u8| unsafe { (p.add(l.slot_off(1) as usize) as *const u64).read_unaligned() };
        assert!(!ra.valid(1), "an unheld state is not valid");
        ra.fold(&re).expect("an unheld state folds nothing");
        assert!(!ra.valid(1) && word(pa) == 0);
        unsafe { hold(pb, &l, 1, 0xfeed) };
        re.fold(&rb).expect("a held state moves into an empty slot");
        assert!(re.valid(1), "the moved state is valid");
        assert_eq!(re.slot(1), Some(AggSlot::AggState(0xfeed)));
        unsafe { hold(pa, &l, 1, 0xbeef) };
        let two = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ra.fold(&rb)));
        if cfg!(debug_assertions) {
            assert!(two.is_err(), "two held states assert in a debug build (D806)");
        } else {
            assert_eq!(two.unwrap(), Err(FloatOverflow), "two held states have no byte merge");
        }
        assert_eq!(word(pa), 0xbeef, "the destination keeps its state");
    }
}

/// V4R-375 (D798 part 1): the keyed owner fold over hosted states (`HashAgg::fold_run_with`). A group's first partial is
/// copied with its states emptied and every partial's held state combines in, the first included, in source order (the
/// combine here is order-sensitive: `into * 100 + from`); an unheld state combines nothing; the sources' bytes never
/// change, so folding them again (a re-read: the owner's TABLE_RETRY, the split) gives the same owner states, with one
/// combine per held partial each time; the handle slot (`MaxI64` after the states) reaches the combine through `src`.
#[test]
fn the_hosted_owner_fold_combines_every_partial_in_order_and_a_reread_counts_nothing_twice() {
    use gravity_abi::breaker::{Candidates, HashAgg, KeyKind, SubSplit};
    let l = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count, SlotKind::AggState, SlotKind::MaxI64]);
    let size = l.size() as usize;
    // (key, state value; 0 = a partial whose state no row initialized)
    let parts: [(u64, u64); 8] = [(1, 3), (2, 5), (1, 7), (3, 0), (2, 11), (1, 13), (3, 17), (4, 0)];
    let mut src = vec![0u64; parts.len() * size / 8 + 1];
    let base = src.as_mut_ptr() as *mut u8;
    for (i, &(k, v)) in parts.iter().enumerate() {
        let p = unsafe { base.add(i * size) };
        let r = unsafe { Rec::at(p, &l) };
        r.open(&[k]);
        r.add_count(0, 1);
        r.max_i64(2, 0x5eed);
        if v != 0 {
            unsafe { hold(p, &l, 1, v) };
        }
    }
    let before = src.clone();
    let fold = || {
        let mut calls = Vec::new();
        let mut region = vec![0u64; 1024];
        let t = unsafe { HashAgg::at(region.as_mut_ptr() as *mut u8, &l, 16, 8192) };
        let mut combine = |s: &Rec<'_>, j: usize, into: *mut u8, from: *const u8| -> bool {
            assert_eq!((j, s.i64(2)), (1, 0x5eed), "the state slot, the handle in the source");
            let f = unsafe { (from as *const u64).read_unaligned() };
            let held = unsafe { into.add(AGGSTATE_HELD).read() } != 0;
            let old = unsafe { (into as *const u64).read_unaligned() };
            let v = if held { old * 100 + f } else { f };
            unsafe { (into as *mut u64).write_unaligned(v) };
            unsafe { into.add(AGGSTATE_HELD).write(1) };
            calls.push(f);
            true
        };
        let n = unsafe { t.fold_run_with(base, parts.len(), SubSplit::NONE, &mut Candidates::none(), &mut combine) };
        assert_eq!(n, parts.len() as i64, "every partial folded");
        let state = |k: u64| t.find(&[k]).map(|r| (r.count(0), r.slot(1)));
        let got = [1, 2, 3, 4].map(state);
        (got, calls)
    };
    let (first, calls) = fold();
    assert_eq!(first[0], Some((3, Some(AggSlot::AggState(30_713)))), "group 1: 3, then 7, then 13 in source order");
    assert_eq!(first[1], Some((2, Some(AggSlot::AggState(511)))), "group 2: 5 then 11");
    assert_eq!(first[2], Some((2, Some(AggSlot::AggState(17)))), "group 3: its unheld first partial combines nothing");
    assert_eq!(first[3].map(|(n, s)| (n, s.is_some())), Some((1, false)), "group 4: no held state, the slot stays empty");
    assert_eq!(calls, vec![3, 5, 7, 11, 13, 17], "one combine per held partial");
    assert!(src == before, "the sources are never written");
    let (again, calls2) = fold();
    assert_eq!((again, calls2), (first, calls), "a re-read of the same sources folds the same states");
}
