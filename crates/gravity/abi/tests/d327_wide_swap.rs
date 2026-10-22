//! D327: a wide record (`LAYOUT_WIDE`, past 256 B: a group of 120 sums) moves whole through the breaker's in-place
//! paths: the HashAgg grow's resettle, the dense sorts (`sort_dense_by_key`, `sort_dense_by`) and `SortRun::sort`. A
//! fixed `[u8; 256]` panicked there (resettle's slice, SortRun's assert) or was written past its end (the dense sorts).
use gravity_abi::breaker::{HashAgg, KeyKind, RecordLayout, SlotKind, SortRun, WideLayout};

const NSUMS: usize = 120;
const GROUPS: u64 = 300;

fn wide() -> RecordLayout {
    let keys: &'static [KeyKind] = &[KeyKind::I64];
    let mut slots = vec![SlotKind::SumI64; NSUMS];
    slots.push(SlotKind::Count);
    let slots: &'static [SlotKind] = Box::leak(slots.into_boxed_slice());
    let offs: &'static mut [u32] = Box::leak(vec![0u32; slots.len()].into_boxed_slice());
    let w: &'static WideLayout = Box::leak(Box::new(WideLayout::compute(keys, slots, offs, true)));
    let l = RecordLayout::wide(w, true, false);
    assert!(l.is_wide() && l.size() > 256, "a record past the old fixed buffer: {} B", l.size());
    l
}

fn sum(g: u64, j: usize) -> i64 {
    (g * 1000 + j as u64) as i64
}

/// A wide table of `GROUPS` groups, every sum its group's and its slot's, grown in place from 8 slots (six grows, each a resettle).
fn table(l: &RecordLayout, buf: &mut Vec<u64>) -> &'static mut HashAgg {
    let limit = HashAgg::bytes(l, 1024);
    *buf = vec![0u64; limit.div_ceil(8) as usize];
    // SAFETY: `buf` holds `limit` bytes and outlives every use in the test.
    let t: &'static mut HashAgg = unsafe { HashAgg::at(buf.as_mut_ptr() as *mut u8, l, 8, limit) };
    for g in 1..=GROUPS {
        // the insert grows the table at its load (the resettle), the limit holding every group
        let r = t.find_or_insert(&[g]).expect("the limit holds every group");
        for j in 0..NSUMS {
            r.add_i64(j, sum(g, j));
        }
        r.add_count(NSUMS, 1);
    }
    assert!(t.cap >= 512, "grown in place from 8 slots past {GROUPS} groups: cap {}", t.cap);
    t
}

fn check(r: &gravity_abi::breaker::Rec<'_>, g: u64) {
    for j in 0..NSUMS {
        assert_eq!(r.i64(j), sum(g, j), "group {g} slot {j}");
    }
    assert_eq!(r.count(NSUMS), 1, "group {g}");
}

#[test]
fn d327_a_wide_group_table_grows_in_place_with_every_record_whole() {
    let l = wide();
    let mut buf = Vec::new();
    let t = table(&l, &mut buf);
    for g in 1..=GROUPS {
        check(&t.find(&[g]).unwrap_or_else(|| panic!("group {g} lost in the grow")), g);
    }
}

#[test]
fn d327_the_dense_sorts_move_wide_records_whole() {
    let l = wide();
    let mut buf = Vec::new();
    let t = table(&l, &mut buf);
    t.sort_dense_by_key(0);
    for i in 0..GROUPS as usize {
        let r = t.record(i);
        assert_eq!(r.key(0), i as u64 + 1, "ascending by the key");
        check(&r, i as u64 + 1);
    }
    let mut buf = Vec::new();
    let t = table(&l, &mut buf);
    // SAFETY (the closure): each pointer is a live record whose first word is its key.
    t.sort_dense_by(&mut |a, b| unsafe { (a as *const u64).read_unaligned() > (b as *const u64).read_unaligned() });
    for i in 0..GROUPS as usize {
        let r = t.record(i);
        assert_eq!(r.key(0), GROUPS - i as u64, "descending by the order given");
        check(&r, GROUPS - i as u64);
    }
}

#[test]
fn d327_a_sort_run_of_wide_rows_sorts_them_whole() {
    const ROW: u32 = 1008;
    const ROWS: u32 = 200;
    let bytes = 64 + 128 + ROW * ROWS * 2;
    let mut buf = vec![0u64; bytes.div_ceil(8) as usize];
    let p = buf.as_mut_ptr() as *mut u8;
    // SAFETY: a share-armed region of `bytes` (word 2 = the share: `at_rows`'s first touch), `buf` outlives the run.
    let run = unsafe {
        (*(p as *mut SortRun)).row_bytes = bytes;
        SortRun::at_rows(p, ROW, ROWS)
    };
    let fill = |row: *mut u8, k: u64| unsafe {
        (row as *mut u64).write_unaligned(k);
        (row.add(8) as *mut u64).write_unaligned(0);
        for b in 16..ROW as usize {
            *row.add(b) = (k as u8).wrapping_mul(13) ^ b as u8;
        }
    };
    for i in 0..ROWS as u64 {
        fill(run.push(), (i * 7919) % ROWS as u64);
    }
    run.sort();
    for i in 0..ROWS as usize {
        let row = run.row(i);
        let k = unsafe { (row as *const u64).read_unaligned() };
        assert_eq!(k, i as u64, "ascending by the first two words");
        for b in 16..ROW as usize {
            assert_eq!(unsafe { *row.add(b) }, (k as u8).wrapping_mul(13) ^ b as u8, "row {i} byte {b}");
        }
    }
}
