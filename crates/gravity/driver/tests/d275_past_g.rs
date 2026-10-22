//! D275 (main 09-24): a worker's region at a section's bind never fails for want of G's room. At W8 an earlier section's
//! bind carves G's room as the workers' W; the next section's bind then drew each worker's program-constant region
//! (`Budget::Fixed`, a plain aggregate's) against G, found none, and ERRORed out of memory with the pool nearly empty
//! (m1_d254_grow's two W8 joins under a 1 MiB flat grant: "draw of 4096 B past the grant (0 B left: reserved 30413056
//! used 16384 kept 30396672)"). The region now comes from the worker's W, else is reserved past G and counted as grown
//! (the arena's and the grows' road, D254), and the statement completes with PG's count. G's room is held here as the
//! earlier carve left it, but for the leader's own slot region (open before the workers bind, as the build's was in the
//! join). `Reservation::hold`, a caller's own bound, still refuses past G. One test: it arms the process-wide pool.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::{footprint, Claimed, Entry, Grant, POOL, REGIONS, REGION_MIN};

const NULLS: [u8; 1] = [0];
const BLOCKS: u64 = 24;
const PER_BLOCK: u64 = 10;
const CLAIM: u64 = 3;
const WIDTH: u16 = 8;

#[test]
fn a_w8_sections_worker_regions_bind_past_an_exhausted_g_and_the_statement_completes() {
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    let program = Arc::new(p_count(4));
    let grant = Grant::of(1 << 20);
    POOL.arm(footprint(grant, &program, WIDTH) + (256 << 20));
    let res = std::rc::Rc::new(POOL.admit_need(grant, &|g| footprint(g, &program, WIDTH), Duration::from_millis(10)).expect("admitted"));
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table);
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[100], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, res.clone()).expect("started");

    // G's room gone but the leader slot's one region, as an earlier section's bind leaves it after carving the workers' W
    let carved = res.hold(res.left() - REGION_MIN as u64).expect("G's room held");
    assert_eq!(res.left(), REGION_MIN as u64);
    assert!(res.hold(REGION_MIN as u64 + 1).is_none(), "Reservation::hold stays bounded by G");
    let grown = res.grown();

    let open = entry.open_section(&mut exec, &b, &mut derive, 0, WIDTH).expect("the workers' program-constant regions bind past G (D275), never out of memory");
    let cursor = AtomicU64::new(0);
    'claims: loop {
        for o in 0..WIDTH {
            let lo = cursor.fetch_add(CLAIM, Ordering::AcqRel);
            if lo >= BLOCKS {
                break 'claims;
            }
            assert_eq!(open.run_claim(o, MorselRange::new(lo, (lo + CLAIM).min(BLOCKS))).unwrap(), Claimed::Done);
        }
    }
    entry.close_section(open, &mut b, &CountMerge, true).unwrap();
    entry.run_to_end(&mut exec, &mut b).expect("the emit");
    entry.end(exec, true);
    assert!(res.grown() > grown, "the regions past G are counted as grown ({} B)", res.grown() - grown);
    assert_eq!(sink.sorted(), vec![vec![100, (0..100).sum::<u64>()]], "the count, as PG's");

    drop(carved);
    drop(res);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "every byte back at the statement's end");
    POOL.arm(0);
}
