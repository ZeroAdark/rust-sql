//! Take-back TB4 (give_back_spill_overdraft, under give_back_spill and give_back_service): a grant pool take the pool has
//! no room for holds past the pool's limit by at most the pool bytes the give-back lane's queued jobs are landing (and
//! the service's backlog cap), under the watchdog's cancel line only: no claim, no decommit, no wait; counted
//! (give_back_spill_overdraft n / bytes / max); the overage is gone once the jobs land.
//! - knob 0 (the default): a draw the full pool has no room for is refused, as before;
//! - knob 1: the draw holds on the landing, counted; one past the landing is refused;
//! - past the cancel line (S1) no overdraft: the draw's trim gives the job back on its thread first (counted), and the
//!   draw then takes the room that freed, back under the limit;
//! - the hand-off of a give (`Held::take_pool`: a region charged to a statement's G) with the pool full: a pool fallback
//!   (an inline give) with the knob off, a job with it on, the overdraft counted.
//! Its own [[test]]: a #[global_allocator] (door 1, as the server counts it), process-wide knobs and hooks.
//! pagepool P3 (branch malisper/pagepool; PGRUST_POOL_GRAVITY=1): when the page pool backs a region (`v4::is_pool`,
//! `RegionBuf::is_pool`: the region's own flag) its give is no job: each give then asserts the pool's path instead
//! (`pool_give`: back to the pool, no give_back_spill_* count moved, the owner's account released, REAPING and PARKED
//! unchanged), and each draw that would have held on the job's landing asserts that, with nothing landing, it held on
//! none (refused, no overdraft counted).

use gravity_driver_rig as common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::mem::giveback;
use gravity_driver::mem::spill_host::v4;
use gravity_driver::mem::region::{try_region, try_region_in};
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
/// What a thread's own fold may still hold back on an account, and a page or two of the rig's own.
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

/// pagepool P3: a give of a region the pool backs, read around the give on the giving thread: the give_back_spill_*
/// counts, every account, REAPING, PARKED, the pool's in-use region bytes and its free bytes (`pool_now`), before / after.
#[derive(Debug)]
struct PoolGave {
    c: (giveback::Counts, giveback::Counts),
    moved: Vec<(u32, i64)>,
    r: (i64, i64),
    parked: (i64, i64),
    in_use: (usize, usize),
    free: (u64, u64),
}

fn accounts() -> HashMap<u32, i64> {
    let mut m = HashMap::new();
    memledger::for_each_row(|r| {
        m.insert(r.acct.0, r.account_bytes);
    });
    m
}

/// The rows whose bytes moved between `a` and `b` (REAPING apart), and by how much.
fn moved(a: &HashMap<u32, i64>, b: &HashMap<u32, i64>) -> Vec<(u32, i64)> {
    let mut keys: Vec<u32> = a.keys().chain(b.keys()).copied().collect();
    keys.sort_unstable();
    keys.dedup();
    keys.into_iter()
        .filter(|k| *k != Acct::REAPING.0)
        .map(|k| (k, b.get(&k).copied().unwrap_or(0) - a.get(&k).copied().unwrap_or(0)))
        .filter(|(_, d)| *d != 0)
        .collect()
}

fn parked() -> i64 {
    memledger::fold();
    memledger::account_row(Acct::PARKED).account_bytes
}

/// pagepool P3: the pool's in-use region bytes, and its free bytes (warm and stranded) plus what its purger released so
/// far (so a release between two reads still counts the bytes as given back to the pool).
fn pool_now() -> (usize, u64) {
    let s = pagepool::stats();
    (s.in_use[pagepool::Tag::REGION.0 as usize], (s.warm_free() + s.stranded) as u64 + s.released_bytes)
}

/// pagepool P3: the counts, accounts, REAPING, PARKED and pool reads before a give (`pool_read`), then after it.
type PoolRead = (giveback::Counts, HashMap<u32, i64>, i64, i64, (usize, u64));

fn pool_read() -> PoolRead {
    (giveback::counts(), accounts(), reaping(), parked(), pool_now())
}

fn pool_gave(before: PoolRead) -> PoolGave {
    let ((c0, a0, r0, k0, s0), (c1, a1, r1, k1, s1)) = (before, pool_read());
    PoolGave { c: (c0, c1), moved: moved(&a0, &a1), r: (r0, r1), parked: (k0, k1), in_use: (s0.0, s1.0), free: (s0.1, s1.1) }
}

/// pagepool P3: one give of a region the pool backs, checked as the pool's path: no job and no give_back_spill_* count
/// moved (no pool fallback, no overdraft), REAPING and PARKED unchanged, the owner's account released by the region's
/// bytes (and nothing else moved past the fold slack), the block out of the pool's in-use region bytes and back among its
/// free ones.
fn pool_give(g: &PoolGave, bytes: usize, at: &str) {
    assert_eq!(g.c.1, g.c.0, "{at}: a pool give is no job and moves no give_back_spill_* count: {g:?}");
    assert_eq!((g.r.1 - g.r.0, g.parked.1 - g.parked.0), (0, 0), "{at}: REAPING and PARKED unchanged: {g:?}");
    assert!(g.moved.iter().any(|(_, d)| (d + bytes as i64).abs() <= SLACK), "{at}: the owner's account released by the region's bytes: {g:?}");
    assert!(g.moved.iter().filter(|(_, d)| (d + bytes as i64).abs() > SLACK).all(|(_, d)| d.abs() <= SLACK), "{at}: nothing else moved past the fold slack: {g:?}");
    assert!(g.in_use.0 >= g.in_use.1 + bytes, "{at}: the block left the pool's in-use region bytes: {g:?}");
    assert!(g.free.1 >= g.free.0 + bytes as u64, "{at}: the block back among the pool's free bytes (or released by its purger): {g:?}");
}

/// One run of a W2 statement whose worker 0 takes a reload region of `bytes` (`v4::take`), writes it and gives it back
/// (`v4::give`): with the service's knob on, one job on the lane. pagepool P3: the pool's give read around it when the
/// pool backs the region.
fn give_one(bytes: usize) -> Option<PoolGave> {
    let program = Arc::new(p_group_share(4));
    let want = Grant::of(64 * MIB);
    let need = footprint(want, &program, WIDTH);
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    POOL.arm(POOL.held() + need + 2 * bytes as u64 + 64 * MIB);
    let res = std::rc::Rc::new(POOL.admit_need(want, &|g| footprint(g, &program, WIDTH), Duration::from_millis(10)).unwrap());
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table);
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let pool;
    {
        let mut sink = LegSink::new();
        let mut b = binding(&[8], &NULLS, &mut sink, &flag);
        let mut exec = entry.start(None, &program, &mut derive, &mut b, res.clone()).unwrap();
        let open = entry.open_section(&mut exec, &b, &mut derive, 0, WIDTH).unwrap();
        let ctx = open.worker_ctx(0) as usize;
        pool = std::thread::scope(|s| {
            s.spawn(move || {
                let ctx = ctx as *mut gravity_abi::Ctx;
                // SAFETY: the section is open, worker 0's context is bound; the region is this thread's alone
                let p = unsafe { v4::take(ctx, bytes) };
                assert!(!p.is_null(), "the reload region taken");
                // SAFETY: `bytes` bytes at `p`, just taken
                unsafe { core::ptr::write_bytes(p, 0x5a, bytes) };
                // SAFETY: worker 0's context is bound; `p` is its live `take`
                let before = unsafe { v4::is_pool(ctx, p) }.then(pool_read);
                // SAFETY: a `take` of this owner, read by nothing from now on
                unsafe { v4::give(ctx, p) };
                before.map(pool_gave)
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
    pool
}

fn reaping() -> i64 {
    memledger::fold();
    memledger::account_row(Acct::REAPING).account_bytes
}

#[test]
fn a_pool_take_holds_on_the_lanes_landing_under_the_cancel_line_counted_bounded_and_gone_once_it_lands() {
    // the server's start, as memruntime's service makes it: the knob, then the admission and the release wake; no service
    // drains the lane here unless the test runs its batch
    mempolicy::consts::set(K::GiveBackService, 1);
    allocarena::own::set_service(stalled, 64);
    allocarena::own::set_release_wake(wake);
    assert!(allocarena::own::service_cap() >= 4 * REGION, "the backlog cap past the jobs here: {}", allocarena::own::service_cap());
    REGIONS.trim();
    let base = reaping();

    // one job on the lane: its pool bytes are on their way back; then the pool full (its capacity what it holds)
    let pool = give_one(2 * REGION);
    REGIONS.trim_to(0);
    let n = giveback::landing_bytes();
    match &pool {
        Some(g) => {
            pool_give(g, 2 * REGION, "the give");
            assert_eq!(n, 0, "a pool give: nothing landing");
        }
        None => assert!(n >= 2 * REGION, "one job landing, not drained: {n}"),
    }
    // pagepool P3: with no job, a region holds the pool bytes the job would have held, so the pool armed at what it holds is
    // full (an armed capacity of 0 is no limit at all)
    let fill = pool.as_ref().map(|_| try_region(2 * REGION).expect("a region holding the job's pool bytes"));
    POOL.arm(POOL.held());
    let (held, c0) = (POOL.held(), giveback::counts());

    // knob 0 (the default): refused, as before
    assert_eq!(mempolicy::consts::get(K::GiveBackSpillOverdraft), 0, "give_back_spill_overdraft defaults to 0");
    assert!(try_region(REGION).is_err(), "no overdraft with the knob off");
    assert_eq!((POOL.held(), giveback::landing_bytes()), (held, n), "nothing held, the job still queued");

    // knob 1: the draw holds past the limit on the lane's landing, counted
    mempolicy::consts::set(K::GiveBackSpillOverdraft, 1);
    let r = match pool {
        None => Some(try_region(REGION).expect("the draw holds on the lane's landing")),
        Some(_) => {
            assert!(try_region(REGION).is_err(), "after a pool give nothing is landing: no room to hold on, refused");
            None
        }
    };
    let c1 = giveback::counts();
    match pool {
        None => {
            assert_eq!((c1.overdraft - c0.overdraft, c1.overdraft_bytes - c0.overdraft_bytes), (1, REGION as u64), "one overdraft of its bytes past the limit: {c1:?}");
            assert!(c1.overdraft_max >= REGION as u64, "the largest overage: {c1:?}");
            assert_eq!(POOL.held(), held + REGION as u64, "held past the capacity by the draw");
            // bounded: a draw past what the lane is landing is refused
            assert!(try_region(n).is_err(), "no draw past the landing");
            assert_eq!((giveback::counts().overdraft, giveback::landing_bytes()), (c1.overdraft, n), "the refusal counted nothing, the job still queued");
        }
        Some(_) => assert_eq!((c1, POOL.held()), (c0, held), "after a pool give: no overdraft counted, nothing held"),
    }

    // past the watchdog's cancel line (S1): no overdraft; the draw's trim gives the job back on its thread (counted) and
    // the draw takes the room that freed; the overage is gone with the job
    mcx::ladder::publish_cancel_line(1000);
    mcx::ladder::publish_read(2000);
    let s = try_region(REGION);
    mcx::ladder::publish_cancel_line(0);
    mcx::ladder::publish_read(0);
    let c2 = giveback::counts();
    match pool {
        None => {
            assert!(s.is_ok(), "S1: the draw took the room the job gave back");
            assert_eq!((c2.overdraft, c2.drained_here_past_cancel - c1.drained_here_past_cancel, c2.landing), (c1.overdraft, 1, 0), "S1: no overdraft, the job given back on the draw's thread: {c2:?}");
        }
        Some(_) => {
            assert!(s.is_err(), "S1 after a pool give: no job to give back, no room freed: refused");
            assert_eq!(c2, c1, "S1 after a pool give: no overdraft, nothing given back on the draw's thread: {c2:?}");
        }
    }
    assert!(POOL.held() <= POOL.capacity(), "back under the limit once the job landed: held {} capacity {}", POOL.held(), POOL.capacity());
    drop(s);
    drop(r);
    drop(fill);

    // the hand-off with the pool full (`Held::take_pool`): a job landing, then a region charged to a statement's G given
    // back whole with no pool room for its charge (a worker's regions hold pool bytes, no charge to hand off)
    if let Some(g) = give_one(2 * REGION) {
        pool_give(&g, 2 * REGION, "the second give");
    }
    let res = POOL.admit_need(Grant::of(64 * MIB), &|_| 32 * MIB, Duration::from_millis(10)).unwrap();
    let handoff = |knob: u64| {
        mempolicy::consts::set(K::GiveBackSpillOverdraft, knob);
        let mut r = try_region_in(&res, REGION).expect("a region in G");
        assert!(r.is_charged(), "charged to the statement's G");
        // SAFETY: `REGION` bytes at its base, this region's alone
        unsafe { core::ptr::write_bytes(r.as_mut_ptr(), 0x5a, REGION) };
        POOL.arm(POOL.held());
        let before = r.is_pool().then(pool_read);
        let c = giveback::counts();
        giveback::give_witness(r);
        let c1 = giveback::counts();
        let g = before.map(pool_gave);
        POOL.arm(POOL.held() + 64 * MIB);
        (c, c1, g)
    };
    let (c3, c4, g) = handoff(0);
    match &g {
        None => assert_eq!((c4.pool_fallbacks - c3.pool_fallbacks, c4.jobs - c3.jobs), (1, 0), "knob 0: a pool fallback, the give inline: {c4:?}"),
        Some(g) => pool_give(g, REGION, "knob 0: a pool region's hand-off"),
    }
    let (c4, c5, g) = handoff(1);
    match &g {
        None => assert_eq!((c5.pool_fallbacks - c4.pool_fallbacks, c5.jobs - c4.jobs, c5.overdraft - c4.overdraft), (0, 1, 1), "knob 1: a job on the overdraft: {c5:?}"),
        Some(g) => pool_give(g, REGION, "knob 1: a pool region's hand-off"),
    }
    drop(res);

    // every job lands: the overage gone, REAPING back to its base
    allocarena::own::drain_jobs_upto(usize::MAX);
    mempolicy::consts::set(K::GiveBackSpillOverdraft, 0);
    assert_eq!(giveback::landing_bytes(), 0);
    assert!(POOL.held() <= POOL.capacity(), "back under the limit once the jobs landed");
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(reaping(), base, "REAPING back to its base");
}
