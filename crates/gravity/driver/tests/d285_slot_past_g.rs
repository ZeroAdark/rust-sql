//! D285 (main 09-24): a leader's slot region never fails for want of G's room while the pool has room past it. A W0
//! statement whose one owner was placed again past the grant (m1_d285_owner_grow: 1 M groups under a stale 1,000-group
//! estimate) left G's room at 0 B; the next pipeline's slot (the outer aggregate's 120-byte region) then drew against G,
//! found none, and ERRORed out of memory with the pool nearly empty ("draw of 4096 B past the grant (0 B left)"). The
//! region now takes D275's road, reserved past G and counted as grown, and the statement completes with PG's count. With
//! no pool room past G either, the open still fails cleanly: out of memory (53200) within the pool's bounded wait, the
//! statement's bytes all back after. G's room is held here as the owner's re-placement left it; the program runs serially
//! (every slot opened by the leader). `Reservation::hold` still refuses past G. One test: it arms the process-wide pool.

use gravity_driver_rig as common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use gravity_driver::{footprint, Entry, Error, Grant, Result, POOL, REGIONS};

const NULLS: [u8; 1] = [0];

/// One serial run of `p_count` with G's room held whole, the pool armed `headroom` bytes past the admission's need:
/// the run's answer, the bytes counted as grown, the rows.
fn run_exhausted(headroom: u64) -> (Result<u64>, u64, Vec<Vec<u64>>) {
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    let program = Arc::new(p_count(4));
    let grant = Grant::of(1 << 20);
    POOL.arm(footprint(grant, &program, 1) + headroom);
    let res = std::rc::Rc::new(POOL.admit_need(grant, &|g| footprint(g, &program, 1), Duration::from_millis(10)).expect("admitted"));
    let table = RangeTable::new(24, 10);
    let mut derive = FakeDerive::ranges(table);
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[100], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, res.clone()).expect("started");

    // G's room gone, as an owner placed again past the grant leaves it
    let carved = res.hold(res.left()).expect("G's room held");
    assert_eq!(res.left(), 0);
    assert!(res.hold(1).is_none(), "Reservation::hold stays bounded by G");
    let grown = res.grown();
    let r = entry.run_to_end(&mut exec, &mut b);
    entry.end(exec, r.is_ok());
    let grown = res.grown() - grown;
    drop(b);
    drop(carved);
    drop(res);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "every byte back at the statement's end (headroom {headroom})");
    POOL.arm(0);
    (r, grown, sink.sorted())
}

#[test]
fn a_serial_statements_slot_region_opens_past_an_exhausted_g_and_fails_cleanly_without_pool_room() {
    let (r, grown, rows) = run_exhausted(256 << 20);
    assert!(r.is_ok(), "the slot regions open past G (D285), never out of memory: {r:?}");
    assert!(grown > 0, "the slot regions past G are counted as grown");
    assert_eq!(rows, vec![vec![100, (0..100).sum::<u64>()]], "the count, as PG's");

    // no room past G in the pool either: PG's out of memory within the bounded wait, nothing kept
    let t0 = Instant::now();
    let (r, _, rows) = run_exhausted(0);
    assert!(matches!(r, Err(Error::OutOfMemory { .. })), "53200 when the pool has no room past G: {r:?}");
    assert!(t0.elapsed() < Duration::from_secs(30), "the pool's wait is bounded ({:?})", t0.elapsed());
    assert!(rows.is_empty());
}
