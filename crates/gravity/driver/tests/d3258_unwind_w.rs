//! D3258: an Exec that unwinds after its section bound gives every worker's W back. The statement ledger's 53200
//! (`pgrust.statement_mem_limit`, the watchdog's cancel) is a panic from the memory-context block that passed the limit, so
//! a refusal right after a bind dropped the Exec in `run_parallel` with no `Entry::end`: `Node::reset`, W's only way back,
//! never ran, and each worker's carved arena stayed held by no statement (3 MiB x 16 at cbgen_C014's cancel in census 8).
//! The slot's drop gives it back. One test per process: the pool is process-wide.

use gravity_driver_rig as common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use gravity_driver::{footprint, Entry, Grant, POOL, REGIONS};

const MIB: u64 = 1 << 20;
const NULLS: [u8; 1] = [0];
const WIDTH: u16 = 2;

#[test]
fn an_exec_unwound_after_its_section_bound_gives_every_workers_w_back() {
    let program = Arc::new(p_group_share(4));
    let want = Grant::of(64 * MIB);
    let need = footprint(want, &program, WIDTH);
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    POOL.arm(need);
    let res = std::rc::Rc::new(POOL.admit_need(want, &|g| footprint(g, &program, WIDTH), Duration::from_millis(10)).unwrap());
    let table = RangeTable::new(24, 10);
    let mut derive = FakeDerive::ranges(table);
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let carved = std::cell::Cell::new((0u64, 0u64));
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut sink = LegSink::new();
        let mut b = binding(&[8], &NULLS, &mut sink, &flag);
        let mut exec = entry.start(None, &program, &mut derive, &mut b, res.clone()).unwrap();
        let open = entry.open_section(&mut exec, &b, &mut derive, 0, WIDTH).unwrap();
        carved.set((open.worker_room(0), open.worker_room(1)));
        // the statement ledger's refusal of the next block, raised as `mcx::governor` raises its 53200
        std::panic::panic_any("D3258: a 53200 right after the section's bind");
    }));
    assert!(unwound.is_err(), "the run unwound");
    assert_eq!(carved.get(), (4 * MIB, 4 * MIB), "each worker's arena carved into its W at bind");
    assert_eq!(std::rc::Rc::strong_count(&res), 1, "the unwound Exec dropped its lease");
    drop(res);
    drop(entry);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "every worker's W given back with no Entry::end (D3258)");
    POOL.arm(0);
}
