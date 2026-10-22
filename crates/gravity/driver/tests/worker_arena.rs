//! D187 (S4): a worker's arena draws off the leader thread, where G cannot be reached. Admitted, its arena is carved into its W
//! at bind out of a reservation that holds it (`footprint`), its chunks land inside W with nothing more held and nothing reported,
//! the leader counts them in the statement's draws at section close, and past W a chunk is refused. One test per process: the
//! ledger mode is read once.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::grant_ledger::{counts, Check};
use gravity_driver::{footprint, Claimed, Entry, Error, Grant, Open, Step, POOL, REGIONS};

const MIB: u64 = 1 << 20;
const NULLS: [u8; 1] = [0];
const BLOCKS: u64 = 24;
const PER_BLOCK: u64 = 10;
const WIDTH: u16 = 2;

fn drain(open: &Open<'_>, cursor: &AtomicU64, ordinal: u16) -> Result<u64, Error> {
    let mut n = 0;
    loop {
        let lo = cursor.fetch_add(5, std::sync::atomic::Ordering::AcqRel);
        if lo >= BLOCKS {
            return Ok(n);
        }
        match open.run_claim(ordinal, MorselRange::new(lo, (lo + 5).min(BLOCKS)))? {
            Claimed::Done => n += 1,
            Claimed::Stopped => return Ok(n),
        }
    }
}

/// `arena_grow` of `bytes` through worker `ordinal`'s context from another thread: the chunk's size, 0 = refused.
fn grow_from_worker(open: &Open<'_>, ordinal: u16, bytes: u64) -> i64 {
    let ctx = open.worker_ctx(ordinal) as usize;
    std::thread::scope(|s| s.spawn(move || unsafe { gravity_abi::program::arena_grow(ctx as *mut gravity_abi::Ctx, bytes) }).join().unwrap())
}

#[test]
fn an_admitted_workers_arena_draws_its_w_is_counted_in_the_statement_and_is_refused_past_it() {
    std::env::set_var("PGRUST_GRAVITY_LEDGER", "report");
    let arena = 4 * MIB;
    let program = Arc::new(p_group_share(4));
    let want = Grant::of(64 * MIB);
    let need = footprint(want, &program, WIDTH);
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    POOL.arm(need);
    let res = std::rc::Rc::new(POOL.admit_need(want, &|g| footprint(g, &program, WIDTH), Duration::from_millis(10)).unwrap());
    assert_eq!((res.reserved, POOL.held()), (need, need), "the footprint holds every worker's arena");
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table);
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    {
        let mut sink = LegSink::new();
        let mut b = binding(&[8], &NULLS, &mut sink, &flag);
        let mut exec = entry.start(None, &program, &mut derive, &mut b, res.clone()).unwrap();
        let open = entry.open_section(&mut exec, &b, &mut derive, 0, WIDTH).unwrap();
        assert_eq!((open.worker_room(0), open.worker_room(1)), (arena, arena), "each worker's arena carved into its W at bind");
        let l1 = counts()[Check::L1 as usize];
        for i in 0..4 {
            assert_eq!(grow_from_worker(&open, 0, MIB), MIB as i64, "chunk {i} of 1 MiB inside W");
        }
        assert_eq!(POOL.held(), need, "inside W the pool holds nothing more");
        assert_eq!(counts()[Check::L1 as usize], l1, "D187: an admitted worker's draw is not a draw with no statement");
        assert_eq!(grow_from_worker(&open, 0, MIB), 0, "past W the chunk is refused, never taken from the capacity");
        assert_eq!(POOL.held(), need);
        assert_eq!(open.worker_room(1), arena, "the other worker's W is its own");
        let drawn = res.drawn();
        let cursor = AtomicU64::new(0);
        for o in 0..WIDTH {
            drain(&open, &cursor, o).unwrap();
        }
        entry.close_section(open, &mut b, &GroupMerge, true).unwrap();
        assert!(res.drawn() >= drawn + 4 * MIB, "the worker's 4 MiB counted in the statement's draws at section close");
        assert_eq!(entry.run(&mut exec, &mut b, gravity_driver::Call::batch(4)).unwrap(), Step::Done);
        entry.end(exec, true);
    }
    assert_eq!(std::rc::Rc::strong_count(&res), 1, "the Exec gave its lease back");
    drop(res);
    drop(entry);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "every region, chunk and W released at the statement's end");
    POOL.arm(0);
}
