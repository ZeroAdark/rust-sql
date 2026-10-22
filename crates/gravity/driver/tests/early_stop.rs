//! D227 (found by V3-R1): a pool thread whose claim ends early under a LIMIT (DONE at its first row, the page still pinned) must
//! release that pin on its own thread. A buffer pin is its thread's (bufmgr's private refcount): released later by the leader
//! at detach, it was "buffer is not pinned". The fake source counts every unpin on a thread other than the pinning one.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::{Claimed, Entry};
use gravity_program::{Generation, Handle, PlanKey};

const KEY: PlanKey = PlanKey { handle: Handle(27), generation: Generation(1) };
const NULLS: [u8; 1] = [0];
const WIDTH: u16 = 4;
const BLOCKS: u64 = 24;

#[test]
fn a_w4_range_scan_under_limit_1_releases_each_workers_pin_on_its_own_thread_1000_times() {
    let table = RangeTable::new(BLOCKS, 10);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_first_row(4));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    for i in 0..1000 {
        let mut sink = LegSink::new();
        let mut b = binding(&[0], &NULLS, &mut sink, &flag);
        let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
        let open = entry.open_section(&mut exec, &b, &mut derive, 0, WIDTH).unwrap();
        let claims: Vec<_> = std::thread::scope(|s| {
            let open = &open;
            let hs: Vec<_> = (0..WIDTH).map(|o| s.spawn(move || open.run_claim(o, MorselRange::new(o as u64 * 6, o as u64 * 6 + 6)))).collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(claims.iter().all(|c| *c == Ok(Claimed::Done)), "iteration {i}: a LIMIT 1 claim ends DONE, not an error: {claims:?}");
        entry.close_section(open, &mut b, &NoMerge, true).unwrap();
        entry.end(exec, true);
        assert_eq!(sink.rows.lock().unwrap().len(), WIDTH as usize, "iteration {i}: one row per worker");
        assert_eq!(table.pins.load(Ordering::Relaxed), 0, "iteration {i}: no pin leaked");
        assert_eq!(table.foreign_unpins.load(Ordering::Relaxed), 0, "iteration {i}: every pin released on the thread that took it");
    }
}
