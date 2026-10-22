//! table.sized (R3-L02, main 09-24): the phase-1 table opened at its bound's size grows in place, zeroing each new half as it
//! comes (`T1::at_sized`, `T1::grow`). Every record stays in its home pair across doublings, a region poisoned before the open
//! never shows a stale slot, a grown table folds what a presized one folds, and past the reservation (or with a spent pool) the
//! table evicts, flushes and bypasses as before, with no row lost.
use std::collections::BTreeMap;

use gravity_abi::breaker::{KeyKind, Rec, RecordLayout, SlotKind, T1, T1_BYPASS, T1_HDR};
use gravity_abi::hash::{Depth, KeyHash};

const POISON: u8 = 0xA5;
const L: RecordLayout = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]);

/// A region of `bytes`, its header prefix zeroed (`T1::at`'s contract) and the rest `fill`.
fn region(bytes: usize, fill: u8) -> Vec<u64> {
    let mut v = vec![u64::from_ne_bytes([fill; 8]); bytes / 8];
    v[..T1_HDR / 8].fill(0);
    v
}

/// The keys the table and its flushes hold, each with its folded count.
#[derive(Default)]
struct Fold {
    got: BTreeMap<u64, i64>,
    grows: u32,
    flushes: u32,
}

impl Fold {
    /// The pool's records folded into `got` (a flush: what the driver's scatter takes), the pool emptied.
    fn flush(&mut self, t: *mut T1) {
        let tr = unsafe { &mut *t };
        for part in 0..tr.parts() as u32 {
            for b in tr.chain(part) {
                for i in 0..b.recs {
                    let r = unsafe { Rec::at(b.ptr.add(i * L.size() as usize) as *mut u8, &L) };
                    assert_eq!(r.route().part(tr.pbits as u32), part, "key {} in its partition", r.key(0));
                    *self.got.entry(r.key(0)).or_default() += r.count(0);
                }
            }
        }
        tr.reset_pool();
        self.flushes += 1;
    }

    /// One row of `key`: M1's miss (HOSTS' agg_locate) — a hit folds; on a full pair the table grows while it can, else the
    /// home's occupant is evicted (a spent pool flushed first); past the observation, the bypass appends.
    fn row(&mut self, t: *mut T1, key: u64, grow: bool) {
        let keys = [key];
        let h = KeyHash::of(&keys);
        let r = unsafe {
            if (*t).flags & T1_BYPASS != 0 {
                let part = (*t).part_of(&keys, h);
                match T1::append_new(t, part, &keys) {
                    Some(r) => r,
                    None => {
                        self.flush(t);
                        T1::append_new(t, part, &keys).expect("an empty pool takes a record")
                    }
                }
            } else if let Some(r) = T1::probe(t, &keys, h) {
                r
            } else if grow && (*t).can_grow() && T1::grow(t) {
                self.grows += 1;
                check_homes(t);
                return self.row(t, key, grow);
            } else {
                let home = (*t).home(h);
                if !T1::evict(t, home) {
                    self.flush(t);
                    assert!(T1::evict(t, home), "an empty pool takes the occupant");
                }
                // the row folds before the observation may drain the table (locate moves the record with it)
                Rec::at(T1::open_at(t, home, &keys), &L).add_count(0, 1);
                if (*t).observe() {
                    while !T1::drain(t) {
                        self.flush(t);
                    }
                    (*t).flags |= T1_BYPASS;
                }
                return;
            }
        };
        unsafe { Rec::at(r, &L) }.add_count(0, 1);
    }

    /// The table drained into the pool and the pool flushed: every row's key with its count.
    fn finish(mut self, t: *mut T1) -> Self {
        while !unsafe { T1::drain(t) } {
            self.flush(t);
        }
        self.flush(t);
        self
    }
}

/// Every live record at its home or the next slot under the current `cap`.
fn check_homes(t: *const T1) {
    let t = unsafe { &*t };
    let mask = t.cap as usize - 1;
    let mut live = 0;
    for i in 0..t.cap as usize {
        let r = t.record(i);
        if r.used() {
            let home = KeyHash::of(r.keys()).home(t.cap.trailing_zeros(), Depth::TOP);
            assert!(i == home || i == (home + 1) & mask, "key {} at slot {i}, home {home} (cap {})", r.key(0), t.cap);
            live += 1;
        }
    }
    assert_eq!(live, t.len, "len counts the live records");
}

fn want(rows: &[u64]) -> BTreeMap<u64, i64> {
    let mut w = BTreeMap::new();
    for &k in rows {
        *w.entry(k).or_default() += 1;
    }
    w
}

/// A key word of `j` that looks random to the hash (an arithmetic run of keys homes too evenly to ever fill a pair).
fn mix(j: u64) -> u64 {
    let z = j.wrapping_add(0x2545_F491_4F6C_DD1D);
    let z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// `n` rows over `keys` distinct keys, scattered (a multiplicative walk, so a key's rows are not adjacent).
fn rows(n: u64, keys: u64) -> Vec<u64> {
    (0..n).map(|i| mix(i.wrapping_mul(0x9E37_79B1) % keys)).collect()
}

#[test]
fn the_table_grows_in_place_across_doublings_every_record_in_its_home_pair() {
    let limit = 4 << 20;
    let mut buf = region(limit, 0);
    let t = unsafe { T1::at_sized(buf.as_mut_ptr() as *mut u8, &L, 4, 512 << 10, 1 << 10, limit as u32) } as *mut T1;
    let (first, max) = unsafe { ((*t).cap, (*t).max_cap()) };
    assert_eq!((first as usize * L.size() as usize, max * L.size() as usize), (1 << 10, 512 << 10), "opened at the bound, reserved to the default");
    let data = rows(200_000, (max / 8) as u64);
    let mut f = Fold::default();
    for &k in &data {
        f.row(t, k, true);
    }
    let cap = unsafe { (*t).cap } as usize;
    assert_eq!(cap, max, "grown to the reservation");
    assert_eq!(f.grows, (max / first as usize).trailing_zeros(), "one doubling at a time");
    let f = f.finish(t);
    assert_eq!(f.got, want(&data));
}

#[test]
fn lazy_zero_a_poisoned_region_never_shows_a_stale_slot() {
    let limit = 2 << 20;
    let mut buf = region(limit, POISON);
    let t = unsafe { T1::at_sized(buf.as_mut_ptr() as *mut u8, &L, 3, 256 << 10, 512, limit as u32) } as *mut T1;
    let rec = L.size() as usize;
    let untouched = |t: *const T1| {
        let t = unsafe { &*t };
        (t.cap as usize..t.max_cap()).all(|i| unsafe { core::slice::from_raw_parts(t.slot_ptr(i), rec) }.iter().all(|&b| b == POISON))
    };
    assert!(untouched(t), "past the opened slots nothing is zeroed at the open");
    let data = rows(40_000, 3_000);
    let keys: std::collections::BTreeSet<u64> = data.iter().copied().collect();
    let mut f = Fold::default();
    for &k in &data {
        let before = f.grows;
        f.row(t, k, true);
        if f.grows != before {
            let tr = unsafe { &*t };
            for i in 0..tr.cap as usize {
                let r = tr.record(i);
                assert!(!r.used() || keys.contains(&r.key(0)), "slot {i} holds a key no row had: {:#x}", r.key(0));
            }
            assert!(untouched(t), "past the grown cap nothing is zeroed");
        }
    }
    assert!(f.grows >= 3, "several doublings ({})", f.grows);
    assert_eq!(f.finish(t).got, want(&data));
}

#[test]
fn a_grown_table_folds_what_a_presized_table_folds() {
    let limit = 2 << 20;
    for keys in [100u64, 5_000, 40_000] {
        let data = rows(120_000, keys);
        let (mut a, mut b) = (region(limit, 0), region(limit, 0));
        let grown = unsafe { T1::at_sized(a.as_mut_ptr() as *mut u8, &L, 4, 512 << 10, 256, limit as u32) } as *mut T1;
        let fixed = unsafe { T1::at(b.as_mut_ptr() as *mut u8, &L, 4, 512 << 10, limit as u32) } as *mut T1;
        let (mut fg, mut ff) = (Fold::default(), Fold::default());
        for &k in &data {
            fg.row(grown, k, true);
            ff.row(fixed, k, false);
        }
        assert_eq!(unsafe { (*grown).max_cap() }, unsafe { (*fixed).cap } as usize, "the same reservation");
        let (g, f) = (fg.finish(grown), ff.finish(fixed));
        assert_eq!(g.got, f.got, "{keys} keys: grown against presized");
        assert_eq!(g.got, want(&data), "{keys} keys");
    }
}

#[test]
fn a_spill_after_a_grow_loses_no_row() {
    // a share whose pool holds a few thousand records: the grown table's evictions spend it and the flushes empty it
    let limit = 192 << 10;
    let mut buf = region(limit, POISON);
    let t = unsafe { T1::at_sized(buf.as_mut_ptr() as *mut u8, &L, 3, 512 << 10, 1 << 10, limit as u32) } as *mut T1;
    let max = unsafe { (*t).max_cap() };
    assert!(max * (L.size() as usize) <= limit / 2, "the reservation clipped to half the share");
    let data = rows(300_000, 60_000);
    let mut f = Fold::default();
    for &k in &data {
        f.row(t, k, true);
    }
    assert!(f.grows >= 2 && unsafe { (*t).cap } as usize == max, "grown to the reservation ({} grows)", f.grows);
    assert!(f.flushes >= 2, "the pool spilled after the grows ({} flushes)", f.flushes);
    assert_eq!(f.finish(t).got, want(&data));
}

#[test]
fn past_the_reservation_the_table_evicts_and_bypasses_as_before() {
    let limit = 4 << 20;
    let mut buf = region(limit, 0);
    let t = unsafe { T1::at_sized(buf.as_mut_ptr() as *mut u8, &L, 4, 64 << 10, 1 << 10, limit as u32) } as *mut T1;
    let max = unsafe { (*t).max_cap() };
    // a million distinct keys against a table sized for 64 groups: the grows stop at the reservation, then evictions, then
    // the observation's bypass
    let data: Vec<u64> = (0..1_000_000u64).map(mix).collect();
    let mut f = Fold::default();
    for &k in &data {
        f.row(t, k, true);
    }
    assert_eq!(unsafe { (*t).cap } as usize, max, "grown to the reservation, no further");
    assert!(unsafe { (*t).flags } & T1_BYPASS != 0, "the observation switched the bypass");
    assert_eq!(f.finish(t).got, want(&data));
}

#[test]
fn a_grow_evicts_a_record_with_no_free_slot_or_declines_on_a_spent_pool() {
    // three keys at the old cap 2^10: k1, k2 in old home o (slots o, o + 1), both new home 2o + 1; k3 in old home o + 1 (slot
    // o + 2), new home 2o + 2. The descending walk moves k3 to 2o + 2 and k2 to 2o + 1, so k1 finds its pair full
    let (ob, nb) = (10u32, 11u32);
    let home = |k: u64, b: u32| KeyHash::of(&[k]).home(b, Depth::TOP);
    let mut pick = (0..).filter(|&k| home(k, ob) == 100 && home(k, nb) == 201);
    let (k1, k2) = (pick.next().unwrap(), pick.next().unwrap());
    let k3 = (0..).find(|&k| home(k, ob) == 101 && home(k, nb) == 202).unwrap();
    for spent in [false, true] {
        let limit = 1 << 20;
        let mut buf = region(limit, 0);
        let t = unsafe { T1::at_sized(buf.as_mut_ptr() as *mut u8, &L, 3, 64 << 10, 16 << 10, limit as u32) } as *mut T1;
        assert_eq!(unsafe { (*t).cap }, 1 << ob);
        let mut f = Fold::default();
        for k in [k1, k2, k3] {
            f.row(t, k, false);
        }
        let slot = |k: u64| (0..unsafe { (*t).cap } as usize).find(|&i| unsafe { (*t).record(i) }.used() && unsafe { (*t).record(i) }.key(0) == k);
        assert_eq!((slot(k1), slot(k2), slot(k3)), (Some(100), Some(101), Some(102)));
        if spent {
            let part = unsafe { (*t).part_of(&[1], KeyHash::of(&[1])) };
            while unsafe { T1::append_new(t, part, &[1]) }.is_some() {}
            assert!(!unsafe { T1::grow(t) }, "a spent pool declines the grow");
            assert_eq!((unsafe { (*t).cap }, slot(k1), slot(k2), slot(k3)), (1 << ob, Some(100), Some(101), Some(102)), "declined: unchanged");
            continue;
        }
        assert!(unsafe { T1::grow(t) });
        check_homes(t);
        assert_eq!((unsafe { (*t).cap }, slot(k1), slot(k2), slot(k3)), (1 << nb, None, Some(201), Some(202)), "k1 evicted to its partition");
        assert_eq!(unsafe { ((*t).len, (*t).evictions) }, (2, 1));
        assert_eq!(f.finish(t).got, want(&[k1, k2, k3]));
    }
}

#[test]
fn a_recurring_collision_under_the_fill_grows_the_table_not_the_bypass() {
    // D4942: a table opened at exactly its groups' fill (32 groups x ADAPT_FACTOR = 256 slots) where three of the keys share
    // one home pair: by the keys it held (31, one of the three always out) it never reached the fill, so each row of the
    // three evicted another until the observation switched the bypass. By the keys it opened it doubles instead.
    let bits = 8u32;
    let home = |k: u64| KeyHash::of(&[k]).home(bits, Depth::TOP);
    let mut three = (0..).map(mix).filter(|&k| home(k) == 101);
    let mut keys: Vec<u64> = (0..3).map(|_| three.next().unwrap()).collect();
    let mut used = std::collections::BTreeSet::from([100usize, 101, 102, 103]);
    for k in (1_000..).map(mix) {
        if keys.len() == 32 {
            break;
        }
        let h = home(k);
        if h % 2 == 0 && !used.contains(&h) && !used.contains(&(h + 1)) {
            used.extend([h, h + 1]);
            keys.push(k);
        }
    }
    let limit = 4 << 20;
    let mut buf = region(limit, 0);
    let first = (1usize << bits) * L.size() as usize;
    let t = unsafe { T1::at_sized(buf.as_mut_ptr() as *mut u8, &L, 3, 512 << 10, first as u32, limit as u32) } as *mut T1;
    assert_eq!(unsafe { (*t).cap }, 1 << bits);
    let data: Vec<u64> = (0..60_000).map(|i| keys[i % 32]).collect();
    let mut f = Fold::default();
    for &k in &data {
        f.row(t, k, true);
    }
    let (cap, flags, evictions) = unsafe { ((*t).cap, (*t).flags, (*t).evictions) };
    assert_eq!(flags & T1_BYPASS, 0, "no bypass ({evictions} evictions, cap {cap})");
    assert!(f.grows >= 1 && cap > 1 << bits, "the recurring collision doubled the table ({} grows)", f.grows);
    assert!(evictions < 64, "a fill's worth of reopens at most before each doubling: {evictions} evictions");
    assert_eq!(f.finish(t).got, want(&data));
}
