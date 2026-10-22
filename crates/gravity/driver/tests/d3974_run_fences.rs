//! D3974: a stream keeps its put runs' fences (D3190, the co-split cut's) for at most `MAXORDER` runs an owner. The cut
//! takes at most `MAXORDER` runs of a stream (exec ops/sort/cosplit.rs), so at an owner's `MAXORDER`-th run its earlier
//! fences are dropped and its later runs are put without them. A long run formation then keeps a bounded set of fences,
//! not one per run. sort_bigvar (100k runs of one 640 KB row at work_mem 64kB) kept each run's first record whole,
//! ~39 GB resident. Every run still reads back as it was put.
use gravity_driver_rig as common;

use common::*;
use gravity_abi::breaker::{SortRun, SORTRUN_HDR, TABLE_SORT};
use gravity_abi::limits::MAXORDER;
use gravity_driver::{install_spill_factory, SpillTable};

/// A fixed sort run of one 16-byte row `(tag, 7)` in an 8-aligned buffer.
fn run_of(tag: u64) -> Vec<u64> {
    let (rb, cap) = (16u32, 4u32);
    let mut buf = vec![0u64; (SORTRUN_HDR + (cap * rb) as usize + 64) / 8]; // alloc: Exec
    let p = buf.as_mut_ptr() as *mut u8;
    unsafe {
        let r = &mut *(p as *mut SortRun);
        (r.cap, r.row_bytes, r.kind) = (cap, rb, TABLE_SORT);
        r.clear();
        let row = r.rows_ptr() as *mut u64;
        (*row, *row.add(1)) = (tag, 7);
        r.push();
    }
    buf
}

/// Run token `i`'s rows read back, as words.
fn read(t: &mut SpillTable, i: u32) -> Vec<u64> {
    let mut out = Vec::new(); // alloc: Exec
    if let Some(mut r) = t.run_open(0, i).unwrap() {
        let mut buf = [0u8; 4096];
        loop {
            let n = r.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
    }
    out.chunks(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()
}

fn put(t: &mut SpillTable, owner: usize, tag: u64) {
    let mut r = run_of(tag);
    unsafe { t.run_put(0, owner, r.as_mut_ptr() as *mut u8).unwrap() };
}

/// The tokens of slot 0 whose runs keep fences, and the bytes those fences hold.
fn fenced(t: &SpillTable) -> (Vec<u32>, usize) {
    let n = t.stream(0).total_runs();
    let toks: Vec<u32> = (0..n).filter(|&i| t.run_fences(0, i).is_some()).collect(); // alloc: Exec
    let bytes = toks.iter().map(|&i| t.run_fences(0, i).unwrap().bytes()).sum();
    (toks, bytes)
}

#[test]
fn an_owners_runs_past_maxorder_keep_no_fences_and_read_back() {
    install_spill_factory(&MEM_SPILL);
    let mut t = SpillTable::with_slots(1);
    t.begin_life(0);
    let m = MAXORDER as u64;
    for i in 0..m {
        put(&mut t, 1, i);
    }
    let (toks, held) = fenced(&t);
    assert_eq!(toks.len(), MAXORDER, "an owner's first MAXORDER runs keep their fences");
    assert!(held > 0);
    // owner 2's runs (tokens after owner 1's), then owner 1 past MAXORDER runs
    put(&mut t, 2, 1_000_000);
    put(&mut t, 2, 1_000_001);
    let n1 = m + 40;
    for i in m..n1 {
        put(&mut t, 1, i);
    }
    let total = t.stream(0).total_runs();
    assert_eq!(total as u64, n1 + 2);
    let (toks, held2) = fenced(&t);
    assert_eq!(toks, vec![n1 as u32, n1 as u32 + 1], "owner 1 past MAXORDER runs keeps no fences; owner 2's two are kept");
    assert!(held2 < held, "fences held {held2} B, under the {held} B of MAXORDER runs");
    for i in 0..n1 as u32 {
        assert_eq!(read(&mut t, i), vec![i as u64, 7], "owner 1's run {i} reads back");
    }
    assert_eq!(read(&mut t, n1 as u32), vec![1_000_000, 7]);
    assert_eq!(read(&mut t, n1 as u32 + 1), vec![1_000_001, 7]);
    // a fresh life's runs are fenced again
    t.end_life(0);
    t.begin_life(0);
    put(&mut t, 1, 5);
    assert_eq!(fenced(&t).0, vec![0], "a new life's first run is fenced");
    t.end_life(0);
}
