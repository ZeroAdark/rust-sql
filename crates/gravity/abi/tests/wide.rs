//! The wide tier of `RecordLayout` (decompose.md decision 82): a record past the eight inline keys / slots
//! reads and folds through the same accessors as the 8-key tier, in a `HashAgg` and a `T1` alike.
use gravity_abi::breaker::{fits_inline, validity_bytes, HashAgg, KeyKind, Rec, RecordLayout, SlotKind, WideLayout, LAYOUT_IMPLICIT_COUNT, LAYOUT_WIDE, HASHAGG_HDR};

const KEYS: [KeyKind; 10] = [KeyKind::I64; 10];
const SLOTS: [SlotKind; 12] = [SlotKind::SumI128, SlotKind::SumI128, SlotKind::SumI128, SlotKind::SumI128, SlotKind::SumI128, SlotKind::SumI128, SlotKind::SumI128, SlotKind::SumI128, SlotKind::MinI64, SlotKind::MaxI64, SlotKind::SumF64, SlotKind::Count];
static mut OFFS: [u32; 12] = [0; 12];

fn wide(validity: bool) -> RecordLayout {
    // SAFETY: the test's own static buffer, written once per layout build (the tests build it serially per thread).
    let offs: &'static mut [u32] = unsafe { core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(OFFS) as *mut u32, 12) };
    let w: &'static WideLayout = Box::leak(Box::new(WideLayout::compute(&KEYS, &SLOTS, offs, validity)));
    RecordLayout::wide(w, validity, false)
}

#[test]
fn the_wide_tier_lays_the_record_out_by_the_same_rule() {
    assert!(!fits_inline(&KEYS, &SLOTS, true) && fits_inline(&KEYS[..8], &SLOTS[..7], true) && !fits_inline(&KEYS[..8], &SLOTS[..8], false), "a Count among eight slots fits; nine words of keys do not");
    assert_eq!((validity_bytes(8), validity_bytes(9), validity_bytes(12), validity_bytes(56), validity_bytes(57)), (8, 8, 8, 8, 16));
    let l = wide(true);
    assert!(l.is_wide() && l.flags & LAYOUT_WIDE != 0 && l.flags & LAYOUT_IMPLICIT_COUNT == 0);
    assert_eq!((l.nkeys(), l.nslots(), l.key_kinds().len(), l.slot_kinds().len()), (10, 12, 10, 12));
    assert_eq!(l.slot_off(0), 80, "the slots follow the ten key words");
    assert_eq!(l.slot_off(8), 80 + 8 * 16);
    assert_eq!(l.sentinel_off(), 80 + 8 * 16 + 8 + 8 + 8, "the first Count slot is the sentinel");
    assert_eq!(l.valid_off(), l.sentinel_off() + 8);
    assert_eq!(l.size(), l.valid_off() + 8, "twelve slots' valid bits and the null_seen byte fit one word");
    assert_eq!(l.valid_bit(9), (l.valid_off() as usize + 1, 1));
    assert_eq!(l.null_seen_off(), l.valid_off() as usize + 2);
    let nn = wide(false);
    assert_eq!(nn.size(), nn.valid_off(), "no validity word");
    let small = RecordLayout::new(&KEYS[..2], &SLOTS[..3]);
    assert!(!small.is_wide() && small.wide.is_none());
    assert_ne!(l, nn);
    assert_eq!(l, wide(true), "equal by content, not by the tail's address");
}

#[test]
fn a_wide_record_folds_in_a_hashagg() {
    let l = wide(true);
    let cap = 16;
    let limit = HashAgg::bytes(&l, cap);
    assert_eq!(limit, HASHAGG_HDR as u32 + cap * l.size());
    let mut buf = vec![0u64; limit.div_ceil(8) as usize];
    let t = unsafe { HashAgg::at(buf.as_mut_ptr() as *mut u8, &l, cap, limit) };
    let keys: Vec<u64> = (1..=10).collect();
    for round in 0..3 {
        let r = t.find_or_insert(&keys).expect("room");
        assert_eq!(r.used(), round > 0, "the explicit Count slot is the sentinel: used once counted");
        r.add_i128(7, 1 << 70);
        r.min_i64(8, 5 - round);
        r.max_i64(9, 5 + round);
        r.add_f64(10, 0.5).expect("finite");
        r.add_count(11, 1);
        if round == 0 {
            assert!(!r.valid(0) && r.valid(7) && r.valid(8) && r.valid(9) && r.valid(10) && r.valid(11), "the bits past slot 7 live in the second validity byte");
        }
    }
    let other: Vec<u64> = (11..=20).collect();
    t.find_or_insert(&other).expect("room").add_count(11, 7);
    assert_eq!(t.len, 2);
    let r = t.find_or_insert(&keys).expect("found");
    assert_eq!((r.keys(), r.i128(7), r.i64(8), r.i64(9), r.f64(10), r.count(11)), (&keys[..], 3 << 70, 3, 7, 1.5, 3));
    assert!(!r.null_seen());
    r.set_null_seen();
    assert!(r.null_seen() && r.valid(11) && !r.valid(0), "null_seen has its own byte past the valid bits");
    // fold the second record into a fresh one of the same layout: every slot combines by kind
    let mut one = vec![0u64; l.size().div_ceil(8) as usize];
    let dst = unsafe { Rec::at(one.as_mut_ptr() as *mut u8, &l) };
    dst.open(&keys);
    dst.fold(&r).expect("no float overflow");
    assert_eq!((dst.i128(7), dst.i64(8), dst.i64(9), dst.count(11), dst.null_seen()), (3 << 70, 3, 7, 3, true));
}
