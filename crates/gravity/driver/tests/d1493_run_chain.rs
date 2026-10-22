//! D1493: an owner's sort runs past `MAX_RUNS` (a file's parts) go on in chained files, and so do a merge pass's runs; a
//! run token reads back the rows its put wrote, the stream counts every chained file's bytes, and a life's end, a merge's
//! fresh pass and a table dropped mid-merge (an ERROR's unwind) leave no file behind. It was `UNSUPPORTED_SPILL` at the
//! 4097th run, printed as the out region's kind 5 (a W0 sort of 200k 1.3 kB rows at work_mem 64 kB).
use gravity_driver_rig as common;

use std::sync::atomic::Ordering;

use common::*;
use gravity_abi::breaker::{SortRun, SORTRUN_HDR, TABLE_SORT};
use gravity_abi::program::MERGED_RUN;
use gravity_driver::{install_spill_factory, SpillTable, MAX_RUNS};

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

/// Run token `i`'s bytes read back (D2572: the run's LZ4 blocks decoded).
fn read(t: &mut SpillTable, i: u32) -> Vec<u8> {
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
    out
}

/// D2572: `raw`'s bytes on disk, its block (a run of up to one block; the memory file keeps runs raw).
fn on_disk(raw: &[u8]) -> u64 {
    let mut v = memledger::ChargedVec::<u8>::new(None, memledger::Kind::Sort);
    gravity_driver::mem::runcodec::encode_block(raw, &mut v.vec_mut(), false);
    v.len() as u64
}

/// `run_of(tag)`'s row bytes.
fn row(tag: u64) -> Vec<u8> {
    [tag.to_le_bytes(), 7u64.to_le_bytes()].concat()
}

fn words(b: &[u8]) -> Vec<u64> {
    b.chunks(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()
}

fn live() -> i64 {
    SPILL_FILES_LIVE.load(Ordering::Relaxed)
}

/// One test (the live-file count is process-wide): put runs past two files, merged runs past one pass file, then every
/// way a chain goes away.
#[test]
fn runs_past_a_files_parts_chain_and_every_chained_file_goes_away() {
    install_spill_factory(&MEM_SPILL);
    let base = live();
    let mut t = SpillTable::with_slots(1);
    t.begin_life(0);
    let n1 = 2 * MAX_RUNS + 3;
    unsafe {
        for i in 0..n1 {
            let mut r = run_of(i as u64);
            assert_eq!(t.run_put(0, 1, r.as_mut_ptr() as *mut u8).unwrap(), i, "owner 1's run {i}");
        }
        for i in 0..2u64 {
            let mut r = run_of(1_000_000 + i);
            t.run_put(0, 2, r.as_mut_ptr() as *mut u8).unwrap();
        }
    }
    let s = t.stream(0);
    assert_eq!((s.total_runs(), s.total_rows()), (n1 + 2, n1 as u64 + 2));
    assert_eq!(live() - base, 3 + 1, "owner 1: three chained run files; owner 2: one");
    let put: u64 = (0..n1 as u64).chain([1_000_000, 1_000_001]).map(|t| on_disk(&row(t))).sum();
    assert_eq!(s.spilled_bytes(), put, "every chained file's bytes");
    for i in [0, MAX_RUNS - 1, MAX_RUNS, 2 * MAX_RUNS, n1 - 1] {
        assert_eq!(words(&read(&mut t, i)), vec![i as u64, 7], "token {i}: its own run, whichever file holds it");
    }
    assert_eq!(words(&read(&mut t, n1)), vec![1_000_000, 7], "owner 2's first run follows owner 1's");
    assert_eq!(words(&read(&mut t, n1 + 1)), vec![1_000_001, 7]);

    // a merge pass whose runs pass one file's parts (pass 0 over more than MAX_RUNS * fan put runs)
    let m = MAX_RUNS + 2;
    for i in 0..m {
        let tok = t.merge_begin(0, 0, i == 0).unwrap();
        assert_eq!(tok, MERGED_RUN | i, "pass 0's run {i}");
        t.merge_write(0, tok, &(i as u64 + 5).to_le_bytes()).unwrap();
    }
    assert_eq!(live() - base, 4 + 2, "pass 0: two chained merge files");
    for i in [0, MAX_RUNS - 1, MAX_RUNS, m - 1] {
        assert_eq!(words(&read(&mut t, MERGED_RUN | i)), vec![i as u64 + 5], "merged run {i}");
    }
    assert_eq!(t.stream(0).total_runs(), n1 + 2, "merged runs are not put runs");
    assert_eq!(t.stream(0).spilled_bytes(), put + (0..m as u64).map(|i| on_disk(&(i + 5).to_le_bytes())).sum::<u64>());
    // pass 1 reads pass 0; pass 2's first run deletes pass 0's whole chain
    let p1 = t.merge_begin(0, 1, true).unwrap();
    t.merge_write(0, p1, &[1u8; 8]).unwrap();
    assert_eq!(live() - base, 4 + 3);
    t.merge_begin(0, 2, true).unwrap();
    assert_eq!(live() - base, 4 + 2, "pass 0's two files deleted, passes 1 and 2 open");
    // a rescan's merge (pass 0 fresh) deletes every pass's files
    t.merge_begin(0, 0, true).unwrap();
    assert_eq!(live() - base, 4 + 1);

    // the life's end drops every chained run file and merge file
    t.end_life(0);
    assert_eq!(live(), base, "no file left after the life's end");

    // a table dropped mid-merge (an ERROR unwinding the statement, a cancel) drops them too
    let mut t = SpillTable::with_slots(1);
    t.begin_life(0);
    unsafe {
        for i in 0..MAX_RUNS + 1 {
            let mut r = run_of(i as u64);
            t.run_put(0, 0, r.as_mut_ptr() as *mut u8).unwrap();
        }
    }
    let tok = t.merge_begin(0, 0, true).unwrap();
    t.merge_write(0, tok, &[0u8; 8]).unwrap();
    let _reader = t.run_open(0, MAX_RUNS).unwrap();
    assert_eq!(live() - base, 2 + 1);
    drop(_reader);
    drop(t);
    assert_eq!(live(), base, "no file left after a table dropped mid-merge");
}
