//! SRC-38 slice 2: the generic owner observes its key range (`OwnerDesc::observe_key`, exec owner.rs `OBSERVE_KEY_BIT`).
//! Each reduce folds key 0's least / greatest datum of the partition it is handed into `OwnerTop::key_lo` / `key_hi`, an
//! int4 / date word read sign-extended, in every generic form (the runs, the key top, the count top), before any form
//! prunes a group; a nullable key's `(value, flags)` image (`OBSERVE_KEY_FLAGGED`) skips the NULL group. Without the
//! flag, or over a record it does not read (two key words, a float key), the owner keeps the empty range, and
//! `Owned::key_range` over the leader region answers none; over owners it is the least and greatest of theirs, an owner
//! that saw no key skipped.
use gravity_abi::breaker::{HashAgg, KeyKind, Owned, OwnerTop, RecordLayout, SlotKind, KEY_HI_NONE, KEY_LO_NONE, ORDER_KEY, ORDER_NONE, OWNED_HDR, TABLE_OWNED};
use gravity_exec::table::owner::owner_fns;
use gravity_program::{OwnerDesc, OBSERVE_KEY, OBSERVE_KEY_FLAGGED, OBSERVE_NONE};

fn layout(keys: &[KeyKind]) -> RecordLayout {
    RecordLayout::new(keys, &[SlotKind::Count])
}

/// The forms over `layout`: the runs, the key top, the count top, each observing as `observe` asks.
fn forms(layout: RecordLayout, observe: u8) -> [OwnerDesc; 3] {
    [OwnerDesc::generic(layout, ORDER_NONE, 0), OwnerDesc::generic(layout, ORDER_KEY, 3), OwnerDesc::count_topk(layout, 0, 2)].map(|d| OwnerDesc { observe_key: observe, ..d })
}

/// The owner's `(key_lo, key_hi)` after one reduce per partition of `parts` (each its groups' key words).
fn observed(d: &OwnerDesc, parts: &[&[&[u64]]]) -> (i64, i64) {
    let fns = owner_fns(d, 0);
    let cap = 1u32 << 6;
    let limit = HashAgg::bytes(&d.layout, cap);
    let bytes = (fns.min_bytes)(fns.params.as_ptr(), 1) as usize + 4 * limit as usize;
    let mut obuf = vec![0u64; bytes.div_ceil(8)];
    let o = obuf.as_mut_ptr() as *mut u8;
    assert!(unsafe { (fns.open)(fns.params.as_ptr(), o, bytes) } > 0, "the owner opens");
    for keys in parts {
        let mut tbuf = vec![0u64; limit.div_ceil(8) as usize];
        let t = unsafe { HashAgg::at(tbuf.as_mut_ptr() as *mut u8, &d.layout, cap, limit) };
        for &k in *keys {
            t.find_or_insert(k).expect("room").add_count(0, 1);
        }
        let r = unsafe { (fns.reduce)(fns.params.as_ptr(), o, t as *const HashAgg, core::ptr::null_mut(), core::ptr::null(), -1) };
        assert_eq!(r, 0, "the reduce keeps the partition");
    }
    let h = unsafe { OwnerTop::view(o) };
    (h.key_lo, h.key_hi)
}

fn narrow(v: i32) -> u64 {
    v as i64 as u64
}

#[test]
fn every_generic_form_observes_its_key_range() {
    for kind in [KeyKind::I32, KeyKind::Date] {
        let (a, b, c, d) = ([narrow(5)], [narrow(-7)], [narrow(i32::MIN)], [narrow(3)]);
        // a zero-extended int4 word (a writer that keeps the low half) reads as its value
        let z = [(-9i32) as u32 as u64];
        for f in forms(layout(&[kind]), OBSERVE_KEY) {
            assert_eq!(observed(&f, &[&[&a, &b, &d]]), (-7, 5), "{kind:?} one partition");
            assert_eq!(observed(&f, &[&[&a], &[&c, &d], &[]]), (i32::MIN as i64, 5), "{kind:?} across partitions, an empty one");
            assert_eq!(observed(&f, &[&[&z, &a]]), (-9, 5), "{kind:?} a zero-extended word");
            assert_eq!(observed(&f, &[&[&[narrow(i32::MAX)]]]), (i32::MAX as i64, i32::MAX as i64), "{kind:?} one key");
        }
    }
    for kind in [KeyKind::I64, KeyKind::Timestamp] {
        let (lo, hi) = ([i64::MIN as u64], [i64::MAX as u64]);
        let (m, p) = ([(-1i64) as u64], [(1i64 << 40) as u64]);
        for f in forms(layout(&[kind]), OBSERVE_KEY) {
            assert_eq!(observed(&f, &[&[&m, &p], &[&[0]]]), (-1, 1 << 40), "{kind:?}");
            assert_eq!(observed(&f, &[&[&lo], &[&hi]]), (i64::MIN, i64::MAX), "{kind:?} the whole range");
            assert_eq!(observed(&f, &[&[&hi]]), (i64::MAX, i64::MAX), "{kind:?} a lone greatest key is a range, not the empty one");
            assert_eq!(observed(&f, &[&[&lo]]), (i64::MIN, i64::MIN), "{kind:?} a lone least key");
        }
    }
}

/// A nullable key's `(value, flags)` image (lowerv4 coderep.rs `image_layout`: an int4's unified `I64` word, then the
/// `I64` flags word): the NULL group (value zeroed, flags bit 0) is never part of the range.
#[test]
fn a_flagged_image_skips_the_null_group() {
    let l = layout(&[KeyKind::I64, KeyKind::I64]);
    let null: &[u64] = &[0, 1];
    for f in forms(l, OBSERVE_KEY_FLAGGED) {
        assert_eq!(observed(&f, &[&[&[40, 0], null, &[(-3i64) as u64, 0]], &[&[17, 0]]]), (-3, 40), "keys around the NULL group's zero");
        assert_eq!(observed(&f, &[&[&[40, 0], null], &[&[50, 0]]]), (40, 50), "zero is no key here");
        assert_eq!(observed(&f, &[&[null]]), (KEY_LO_NONE, KEY_HI_NONE), "only the NULL group");
    }
    for f in forms(layout(&[KeyKind::Date, KeyKind::I64]), OBSERVE_KEY_FLAGGED) {
        assert_eq!(observed(&f, &[&[&[narrow(-20), 0], null, &[narrow(9), 0]]]), (-20, 9), "a nullable date");
    }
}

#[test]
fn an_owner_not_asked_or_not_readable_keeps_the_empty_range() {
    let none = (KEY_LO_NONE, KEY_HI_NONE);
    for f in forms(layout(&[KeyKind::I64]), OBSERVE_NONE) {
        assert_eq!(observed(&f, &[&[&[1], &[9]]]), none, "the flag off");
    }
    for f in forms(layout(&[KeyKind::I64]), OBSERVE_KEY_FLAGGED) {
        assert_eq!(observed(&f, &[&[&[1], &[9]]]), none, "a flagged image needs its flags word");
    }
    for f in forms(layout(&[KeyKind::F64]), OBSERVE_KEY) {
        assert_eq!(observed(&f, &[&[&[1.5f64.to_bits()]]]), none, "a float key");
    }
    for f in forms(layout(&[KeyKind::I32, KeyKind::I32]), OBSERVE_KEY) {
        assert_eq!(observed(&f, &[&[&[1, 2], &[3, 4]]]), none, "two key words read as one key");
    }
}

/// A leader region of `owners` behind its `Owned` header, each `(key_lo, key_hi)`.
fn region(owners: &[(i64, i64)]) -> Vec<u64> {
    const OB: usize = 256;
    let mut buf = vec![0u64; (OWNED_HDR + owners.len() * OB) / 8];
    let p = buf.as_mut_ptr() as *mut u8;
    unsafe {
        let o = p as *mut Owned;
        (*o).nowners = owners.len() as u32;
        (*o).owner_bytes = OB as u32;
        (*o).kind = TABLE_OWNED;
        for (i, &(lo, hi)) in owners.iter().enumerate() {
            let t = p.add(OWNED_HDR + i * OB) as *mut OwnerTop;
            (*t).key_lo = lo;
            (*t).key_hi = hi;
            (*t).records = 1;
            (*t).groups = 1;
        }
    }
    buf
}

fn bytes(buf: &[u64]) -> &[u8] {
    unsafe { core::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 8) }
}

#[test]
fn the_leader_range_spans_the_owners_that_observed() {
    let none = (KEY_LO_NONE, KEY_HI_NONE);
    assert_eq!(Owned::key_range(bytes(&region(&[(3, 9), none, (-4, 2)]))), Some((-4, 9)));
    assert_eq!(Owned::key_range(bytes(&region(&[none, (7, 7)]))), Some((7, 7)));
    assert_eq!(Owned::key_range(bytes(&region(&[none, none]))), None, "no owner observed a key");
    assert_eq!(Owned::key_range(bytes(&region(&[]))), None, "no owner");
    let mut other = region(&[(1, 2)]);
    unsafe { (*(other.as_mut_ptr() as *mut Owned)).kind = TABLE_OWNED + 1 };
    assert_eq!(Owned::key_range(bytes(&other)), None, "not an Owned leader region");
    assert_eq!(Owned::key_range(&bytes(&region(&[(1, 2)]))[..OWNED_HDR - 1]), None, "shorter than the header");
}
