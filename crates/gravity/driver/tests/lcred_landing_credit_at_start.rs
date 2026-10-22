//! The ladder pass's landing credit (`reap::landing_credit`) is the hook's from the driver's start (`reap::start`, which
//! the postmaster calls once memruntime started), not only from the reaper thread's first spawn (`reap::defer`): a
//! server that never hands a reservation to the reaper still nets what is landing. Here no reservation reaches the reaper
//! (no thread spawns) and the landing is the give-back service's job lane: one spill region given back as one job, which
//! no service drains yet.
//! - after `reap::start` the hook's landing (`mcx::ladder::unmap_landing`) is the credit, the job's bytes included;
//! - the hook's pass nets it: a read past the ladder's line by less than the landing leaves no need;
//! - past the watchdog's cancel line nothing nets (R5): the same read leaves the whole overage.
//! Its own [[test]]: a #[global_allocator] (door 1, as the server counts it), process-wide knobs and hooks.
//! pagepool P3 (branch malisper/pagepool; PGRUST_POOL_GRAVITY=1): when the page pool backs the spill region
//! (`v4::is_pool`, the region's own flag) its give is no job: the test then asserts the pool's path (back to the pool, no
//! give_back_spill_* count moved, the owner's account released, REAPING and PARKED unchanged), and that with nothing
//! queued nothing nets the read under the cancel line either; the hook / credit and R5 checks run as they are.

use gravity_driver_rig as common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::mem::{giveback, reap};
use gravity_driver::mem::spill_host::v4;
use gravity_driver::{footprint, Claimed, Entry, Error, Grant, Open, Step, POOL, REGIONS};
use memledger::Acct;
use mempolicy::consts::K;

/// Door 1, as the server's allocator wrapper counts it (init_small oom.rs).
struct Door1;

// SAFETY: forwards to the system allocator; `door_heap` is a const thread-local add (no allocation, no destructor).
unsafe impl GlobalAlloc for Door1 {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller's layout, forwarded.
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            memledger::door_heap(l.size() as isize);
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller's layout, forwarded.
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            memledger::door_heap(l.size() as isize);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: `p` came from `alloc` with `l`.
        unsafe { System.dealloc(p, l) };
        memledger::door_heap(-(l.size() as isize));
    }
}

#[global_allocator]
static ALLOC: Door1 = Door1;

const MIB: u64 = 1 << 20;
const NULLS: [u8; 1] = [0];
const BLOCKS: u64 = 24;
const PER_BLOCK: u64 = 10;
const WIDTH: u16 = 2;
const REGION: usize = 8 << 20;
/// The ladder's line in this test (any line past which the stub's read sits).
const LINE: u64 = 1 << 30;
/// What a thread's own fold may still hold back on an account (`DOOR_FOLD_BYTES`), and a page or two of the rig's own.
const SLACK: i64 = 512 << 10;

fn drain(open: &Open<'_>, cursor: &AtomicU64, ordinal: u16) -> Result<u64, Error> {
    let mut n = 0;
    loop {
        let lo = cursor.fetch_add(5, Ordering::AcqRel);
        if lo >= BLOCKS {
            return Ok(n);
        }
        match open.run_claim(ordinal, MorselRange::new(lo, (lo + 5).min(BLOCKS)))? {
            Claimed::Done => n += 1,
            Claimed::Stopped => return Ok(n),
        }
    }
}

fn wake() {}

fn stalled() -> bool {
    false
}

/// Every account's bytes now (the calling thread's counts folded first).
fn accounts() -> HashMap<u32, i64> {
    memledger::fold();
    let mut m = HashMap::new();
    memledger::for_each_row(|r| {
        m.insert(r.acct.0, r.account_bytes);
    });
    m
}

/// The ledger's REAPING and PARKED bytes now.
fn reaping_parked() -> (i64, i64) {
    memledger::fold();
    (memledger::account_row(Acct::REAPING).account_bytes, memledger::account_row(Acct::PARKED).account_bytes)
}

/// The pool's in-use region bytes, and its free bytes (warm and stranded) plus what its purger released so far.
fn pool_now() -> (usize, u64) {
    let s = pagepool::stats();
    (s.in_use[pagepool::Tag::REGION.0 as usize], (s.warm_free() + s.stranded) as u64 + s.released_bytes)
}

/// pagepool P3: what the give saw when the pool backs the region (`v4::is_pool`), read around it on the giving thread.
#[derive(Debug)]
struct Gave {
    pooled: bool,
    c: (giveback::Counts, giveback::Counts),
    reaping_parked: ((i64, i64), (i64, i64)),
    /// Every account that moved (REAPING and PARKED apart), and by how much.
    moved: Vec<(u32, i64)>,
    in_use: (usize, usize),
    free: (u64, u64),
}

/// pagepool P3: the give of a region the pool backs, checked as the pool's path: no job and no give_back_spill_* count
/// moved, REAPING and PARKED unchanged, the owner's account released by the region's bytes (nothing else past the fold
/// slack), the block out of the pool's in-use region bytes and back among its free ones.
fn pool_give(g: &Gave) {
    assert!(g.pooled, "the pool backs the region: {g:?}");
    assert_eq!(g.c.1, g.c.0, "a pool give is no job and moves no give_back_spill_* count: {g:?}");
    assert_eq!(g.reaping_parked.1, g.reaping_parked.0, "REAPING and PARKED unchanged: {g:?}");
    assert!(g.moved.iter().any(|(_, d)| (d + REGION as i64).abs() <= SLACK), "the owner's account released by the region's bytes: {:?}", g.moved);
    assert!(g.moved.iter().filter(|(_, d)| (d + REGION as i64).abs() > SLACK).all(|(_, d)| d.abs() <= SLACK), "nothing else moved past the fold slack: {:?}", g.moved);
    assert!(g.in_use.0 >= g.in_use.1 + REGION, "the block left the pool's in-use region bytes: {g:?}");
    assert!(g.free.1 >= g.free.0 + REGION as u64, "the block back among the pool's free bytes (or released by its purger): {g:?}");
}

/// One run of a W2 statement whose worker 0 takes a reload region (`v4::take`), writes it and gives it back
/// (`v4::give`): with the service's knob on, one job on the lane (pagepool P3: none when the pool backs the region).
fn give_one() -> Gave {
    let program = Arc::new(p_group_share(4));
    let want = Grant::of(64 * MIB);
    let need = footprint(want, &program, WIDTH);
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    POOL.arm(need + REGION as u64 + 64 * MIB);
    let res = std::rc::Rc::new(POOL.admit_need(want, &|g| footprint(g, &program, WIDTH), Duration::from_millis(10)).unwrap());
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table);
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let gave;
    {
        let mut sink = LegSink::new();
        let mut b = binding(&[8], &NULLS, &mut sink, &flag);
        let mut exec = entry.start(None, &program, &mut derive, &mut b, res.clone()).unwrap();
        let open = entry.open_section(&mut exec, &b, &mut derive, 0, WIDTH).unwrap();
        let ctx = open.worker_ctx(0) as usize;
        gave = std::thread::scope(|s| {
            s.spawn(move || {
                let ctx = ctx as *mut gravity_abi::Ctx;
                // SAFETY: the section is open, worker 0's context is bound; the region is this thread's alone
                let p = unsafe { v4::take(ctx, REGION) };
                assert!(!p.is_null(), "the reload region taken");
                // SAFETY: `REGION` bytes at `p`, just taken
                unsafe { core::ptr::write_bytes(p, 0x5a, REGION) };
                // SAFETY: worker 0's context is bound; `p` is its live `take`
                let pooled = unsafe { v4::is_pool(ctx, p) };
                let (c0, a0, k0, s0) = (giveback::counts(), accounts(), reaping_parked(), pool_now());
                // SAFETY: a `take` of this owner, read by nothing from now on
                unsafe { v4::give(ctx, p) };
                let (c1, a1, k1, s1) = (giveback::counts(), accounts(), reaping_parked(), pool_now());
                let mut keys: Vec<u32> = a0.keys().chain(a1.keys()).copied().collect();
                keys.sort_unstable();
                keys.dedup();
                let moved = keys
                    .into_iter()
                    .filter(|k| *k != Acct::REAPING.0 && *k != Acct::PARKED.0)
                    .map(|k| (k, a1.get(&k).copied().unwrap_or(0) - a0.get(&k).copied().unwrap_or(0)))
                    .filter(|(_, d)| *d != 0)
                    .collect();
                Gave { pooled, c: (c0, c1), reaping_parked: (k0, k1), moved, in_use: (s0.0, s1.0), free: (s0.1, s1.1) }
            })
            .join()
            .unwrap()
        });
        let cursor = AtomicU64::new(0);
        for o in 0..WIDTH {
            drain(&open, &cursor, o).unwrap();
        }
        entry.close_section(open, &mut b, &GroupMerge, true).unwrap();
        assert_eq!(entry.run(&mut exec, &mut b, gravity_driver::Call::batch(4)).unwrap(), Step::Done);
        entry.end(exec, true);
    }
    drop(res);
    drop(entry);
    gave
}

/// The ladder's rungs with a fixed read and nothing to reclaim; the landing is the live hook's, as the server's rungs
/// read it (`mcx::ladder`'s `Live::landing`).
struct Read(u64);

impl mcx::ladder::Rungs for Read {
    fn read(&self) -> Option<u64> {
        Some(self.0)
    }
    fn landing(&self) -> u64 {
        mcx::ladder::unmap_landing()
    }
    fn reclaimable(&self) -> u64 {
        0
    }
    fn reclaim(&self, _bytes: u64) -> u64 {
        0
    }
    fn asker(&self) -> (u64, u64, u64) {
        (0, 0, 0)
    }
    fn revocable(&self) -> u64 {
        0
    }
    fn own_spill(&self, _bytes: u64) {}
    fn revoke(&self, _bytes: u64) {}
    fn alloc_return(&self) {}
    fn now_ms(&self) -> u64 {
        1
    }
}

/// Whether a thread of this process is named `name` (`/proc/self/task/*/comm`).
fn thread_named(name: &str) -> bool {
    std::fs::read_dir("/proc/self/task").unwrap().flatten().any(|t| std::fs::read_to_string(t.path().join("comm")).is_ok_and(|c| c.trim() == name))
}

#[test]
fn the_landing_credit_is_the_hooks_from_the_drivers_start_with_no_reaper() {
    // the server's start, as memruntime's service makes it: the knob, then the admission and the release wake
    mempolicy::consts::set(K::GiveBackService, 1);
    allocarena::own::set_service(stalled, 8);
    allocarena::own::set_release_wake(wake);
    let g = give_one();
    let n = giveback::landing_bytes() as u64;
    if g.pooled {
        pool_give(&g);
        assert_eq!(n, 0, "pagepool P3: a pool give queues nothing on the lane: {g:?}");
    } else {
        assert!(n >= REGION as u64, "one job on the lane, not drained: {n}");
    }

    // the driver's start: the credit is the hook's, and no reaper exists
    reap::start();
    assert_eq!(reap::HANDED.load(Ordering::Relaxed), 0, "no reservation reached the reaper");
    assert!(!thread_named("gravity-reap"), "no reaper thread");
    assert_eq!(mcx::ladder::unmap_landing(), reap::landing_credit(), "the hook's landing is the driver's credit");
    assert!(mcx::ladder::unmap_landing() >= n, "the job's bytes are landing to the pass");

    // the hook's pass nets it under the cancel line: a read past the line by half the landing leaves no need. pagepool P3:
    // after a pool give nothing is landing, so a read past the line by a MiB is left whole
    mcx::ladder::publish(mcx::ladder::ON, LINE);
    let over = if g.pooled { MIB } else { n / 2 };
    let c = mcx::ladder::hook_pass_with(&Read(LINE + over), LINE, 1);
    if g.pooled {
        assert_eq!(c.need, over, "pagepool P3: nothing queued, nothing landing to net the read: {c:?}");
    } else {
        assert_eq!(c.need, 0, "the landing netted: {c:?}");
    }
    // past the watchdog's cancel line nothing nets (R5): the whole overage is left
    mcx::ladder::waive_wait();
    let c = mcx::ladder::hook_pass_with(&Read(LINE + over), LINE, 1);
    assert_eq!(c.need, over, "past the cancel line no landing nets the read: {c:?}");
    mcx::ladder::publish(mcx::ladder::OFF, 0);

    // the job given back as the service's batch: nothing landing
    allocarena::own::drain_jobs_upto(usize::MAX);
    assert_eq!(giveback::landing_bytes(), 0);
    assert_eq!(mcx::ladder::unmap_landing(), reap::landing_credit());
}
