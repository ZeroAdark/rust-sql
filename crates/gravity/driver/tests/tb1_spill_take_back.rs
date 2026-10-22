//! Take-back (T1 + T2, give_back_spill under give_back_service): a plain region draw that finds no parked region of its
//! shape takes a queued whole give (a spill region its owner gave back, waiting for the give-back service) back instead
//! of a fresh allocation: the same block, its pages still resident (no decommit, no fault), off the job lane (its landing
//! 0, the service never gives it), REAPING down by the job's bytes and the taker's account up by them, the pool's held
//! bytes unchanged (the job's pool hold is the region's again), counted. Its charge moves REAPING -> PARKED -> the taker
//! (PARKED nets 0) and its residency entry with it (listed under the taker). A draw of another shape leaves the job
//! queued; one whose grant cannot hold it (S7) gives it back to the lane as its owner gave it: REAPING and the landing back
//! up by the job's bytes, one job more, no take-back counted.
//! Its own [[test]]: a #[global_allocator] (door 1, as the server counts it), process-wide knobs and hooks.
//! pagepool P3 (branch malisper/pagepool; PGRUST_POOL_GRAVITY=1): when the page pool backs the region (`v4::is_pool`, the
//! region's own flag) its give is no job: the give then asserts the pool's path instead (`pool_give`: back to the pool,
//! no give_back_spill_* count moved, the owner's account released, REAPING and PARKED unchanged), and each draw asserts
//! that it found no queued give to take back (`no_take_back`).

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

/// pagepool P3: one give of a region the pool backs, checked as the pool's path: no job and no give_back_spill_* count
/// moved, REAPING and PARKED unchanged, the owner's account released by the region's bytes (and nothing else moved past
/// the fold slack), the block out of the pool's in-use region bytes and back among its free ones.
fn pool_give(g: &PoolGave, bytes: usize, at: &str) {
    assert_eq!(g.c.1, g.c.0, "{at}: a pool give is no job and moves no give_back_spill_* count: {g:?}");
    assert_eq!((g.r.1 - g.r.0, g.parked.1 - g.parked.0), (0, 0), "{at}: REAPING and PARKED unchanged: {g:?}");
    assert!(g.moved.iter().any(|(_, d)| (d + bytes as i64).abs() <= SLACK), "{at}: the owner's account released by the region's bytes: {g:?}");
    assert!(g.moved.iter().filter(|(_, d)| (d + bytes as i64).abs() > SLACK).all(|(_, d)| d.abs() <= SLACK), "{at}: nothing else moved past the fold slack: {g:?}");
    assert!(g.in_use.0 >= g.in_use.1 + bytes, "{at}: the block left the pool's in-use region bytes: {g:?}");
    assert!(g.free.1 >= g.free.0 + bytes as u64, "{at}: the block back among the pool's free bytes (or released by its purger): {g:?}");
}

/// pagepool P3: a draw after a pool give found no queued give to take back: nothing taken, no take-back or job counted,
/// REAPING, PARKED and the lane's landing unchanged.
fn no_take_back(d: &Drew, jobs: u64, taken: (u64, u64), at: &str) {
    assert_eq!(d.at, None, "{at}: no queued give to take back: {d:?}");
    assert_eq!((d.r.1 - d.r.0, d.parked.1 - d.parked.0, giveback::landing_bytes()), (0, 0, 0), "{at}: REAPING, PARKED and the landing unchanged: {d:?}");
    assert_eq!((giveback::counts().jobs, (giveback::counts().taken_back, giveback::counts().taken_back_bytes)), (jobs, taken), "{at}: no job, no take-back counted");
}

/// One run of a W2 statement whose worker 0 takes a reload region of `bytes` (`v4::take`), writes it and gives it back
/// (`v4::give`): with the service's knob on, one job on the lane. The region's base; pagepool P3: and the pool's give
/// read around it when the pool backs the region.
fn give_one(bytes: usize) -> (usize, Option<PoolGave>) {
    let program = Arc::new(p_group_share(4));
    let want = Grant::of(64 * MIB);
    let need = footprint(want, &program, WIDTH);
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    POOL.arm(need + 2 * bytes as u64 + 64 * MIB);
    let res = std::rc::Rc::new(POOL.admit_need(want, &|g| footprint(g, &program, WIDTH), Duration::from_millis(10)).unwrap());
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table);
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let at;
    {
        let mut sink = LegSink::new();
        let mut b = binding(&[8], &NULLS, &mut sink, &flag);
        let mut exec = entry.start(None, &program, &mut derive, &mut b, res.clone()).unwrap();
        let open = entry.open_section(&mut exec, &b, &mut derive, 0, WIDTH).unwrap();
        let ctx = open.worker_ctx(0) as usize;
        at = std::thread::scope(|s| {
            s.spawn(move || {
                let ctx = ctx as *mut gravity_abi::Ctx;
                // SAFETY: the section is open, worker 0's context is bound; the region is this thread's alone
                let p = unsafe { v4::take(ctx, bytes) };
                assert!(!p.is_null(), "the reload region taken");
                // SAFETY: `bytes` bytes at `p`, just taken
                unsafe { core::ptr::write_bytes(p, 0x5a, bytes) };
                // SAFETY: worker 0's context is bound; `p` is its live `take`
                let pooled = unsafe { v4::is_pool(ctx, p) };
                let s0 = pooled.then(|| (giveback::counts(), accounts(), reaping(), parked(), pool_now()));
                // SAFETY: a `take` of this owner, read by nothing from now on
                unsafe { v4::give(ctx, p) };
                let pool = s0.map(|(c0, before, r0, k0, s0)| {
                    let (c1, after, r1, k1, s1) = (giveback::counts(), accounts(), reaping(), parked(), pool_now());
                    PoolGave { c: (c0, c1), moved: moved(&before, &after), r: (r0, r1), parked: (k0, k1), in_use: (s0.0, s1.0), free: (s0.1, s1.1) }
                });
                (p as usize, pool)
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
    at
}

fn reaping() -> i64 {
    memledger::fold();
    memledger::account_row(Acct::REAPING).account_bytes
}

/// The bytes of `[p, p + len)` resident now (its whole pages).
fn resident(p: usize, len: usize) -> usize {
    let mut buf = [0u8; 4096];
    pagegive::resident_bytes(p, p + len, &mut buf).unwrap_or(0)
}

/// What a draw of `bytes` on a thread of its own (unbound, as a backend's between statements) saw: the region's base,
/// REAPING and that thread's account before and after, the pool's held bytes before and after; the region dropped after.
#[derive(Debug)]
struct Drew {
    at: Option<usize>,
    resident: usize,
    r: (i64, i64),
    own: (i64, i64),
    held: (u64, u64),
    /// PARKED before and after; the taken region's residency entry's account.
    parked: (i64, i64),
    listed: Option<Acct>,
}

/// `draw` with a grant (`grant` bytes, `Some`): the take charged to that statement's G, which a region past it does not fit.
fn draw_in(bytes: usize, grant: Option<u64>) -> Drew {
    std::thread::spawn(move || {
        let row = |a: Acct| {
            memledger::fold();
            memledger::account_row(a).account_bytes
        };
        let res = grant.map(|g| POOL.admit_need(Grant::of(g), &|_| g, Duration::from_millis(10)).unwrap());
        let (r0, u0, h0, p0) = (reaping(), row(Acct::UNBOUND), POOL.held(), row(Acct::PARKED));
        let b = REGIONS.take_parked(bytes, res.as_ref(), None);
        let (r1, u1, h1, p1) = (reaping(), row(Acct::UNBOUND), POOL.held(), row(Acct::PARKED));
        let at = b.as_ref().map(|b| b.as_ptr() as usize);
        let resident = at.map_or(0, |p| resident(p, bytes));
        let listed = b.as_ref().and_then(|b| b.residency_acct());
        drop(b);
        drop(res);
        Drew { at, resident, r: (r0, r1), own: (u0, u1), held: (h0, h1), parked: (p0, p1), listed }
    })
    .join()
    .unwrap()
}

fn draw(bytes: usize) -> Drew {
    draw_in(bytes, None)
}

#[test]
fn a_region_draw_takes_a_queued_whole_give_of_its_shape_back() {
    // the server's start, as memruntime's service makes it: the knob, then the admission and the release wake; no service
    // drains the lane here
    mempolicy::consts::set(K::GiveBackService, 1);
    allocarena::own::set_service(stalled, 8);
    allocarena::own::set_release_wake(wake);
    REGIONS.trim();
    let base = reaping();
    let (at, pool) = give_one(REGION);
    // the statement's own regions parked at its end leave the depot (no wait for the lane: `trim_to`), so a draw finds none
    REGIONS.trim_to(0);
    assert_eq!(REGIONS.stats().bytes, 0, "nothing parked");
    let taken = || (giveback::counts().taken_back, giveback::counts().taken_back_bytes);
    let job = match &pool {
        Some(g) => {
            pool_give(g, REGION, "the give");
            assert_eq!((giveback::landing_bytes(), reaping()), (0, base), "a pool give: nothing on the lane, REAPING at its base");
            None
        }
        None => {
            let n = giveback::landing_bytes();
            assert!(n >= REGION, "one job on the lane, not drained: {n}");
            assert_eq!(reaping() - base, n as i64, "the job's bytes on REAPING");
            Some(n)
        }
    };
    let t0 = taken();

    // a draw of another shape leaves it queued
    let other = draw(4 * REGION);
    assert_ne!(other.at, Some(at), "a draw of another shape does not take the job: {other:?}");
    match job {
        Some(n) => assert_eq!(giveback::landing_bytes(), n, "the job still queued"),
        None => no_take_back(&other, giveback::counts().jobs, t0, "another shape, after a pool give"),
    }
    assert_eq!(taken(), t0, "no take-back counted");

    // a draw of its shape whose grant cannot hold it (S7): the region goes back to the lane as its owner gave it
    let jobs0 = giveback::counts().jobs;
    let refused = draw_in(REGION, Some(MIB));
    assert_eq!(refused.at, None, "the taker's grant does not hold it: {refused:?}");
    match job {
        Some(n) => {
            assert_eq!((refused.r.1 - refused.r.0, giveback::landing_bytes()), (0, n), "REAPING and the landing back up by the job's bytes: {refused:?}");
            assert_eq!((giveback::counts().jobs - jobs0, taken()), (1, t0), "one job more (the re-give), no take-back counted");
        }
        None => no_take_back(&refused, jobs0, t0, "S7, after a pool give"),
    }
    let jobs = giveback::counts().jobs;

    // a draw of its shape takes it back: the same block, still resident, off the lane, REAPING to the taker
    let d = draw(REGION);
    match job {
        Some(n) => {
            assert_eq!(d.at, Some(at), "the draw took the queued region back: {d:?}");
            assert!(d.resident + 2 * 4096 >= REGION, "no decommit, no fault: its pages still resident: {d:?}");
            assert_eq!(giveback::landing_bytes(), 0, "off the job lane: {d:?}");
            assert_eq!(d.r.1 - d.r.0, -(n as i64), "REAPING down by the job's bytes: {d:?}");
            assert!((d.own.1 - d.own.0 - n as i64).abs() <= SLACK, "the taker's account up by them: {d:?}");
            assert_eq!(d.held.1, d.held.0, "the pool's held bytes unchanged: the job's hold is the region's: {d:?}");
            assert_eq!(d.parked.1, d.parked.0, "PARKED nets 0: the charge passed through the depot's holder: {d:?}");
            assert_eq!(d.listed, Some(Acct::UNBOUND), "its residency entry listed under the taker: {d:?}");
            assert_eq!(giveback::counts().jobs, jobs, "no new job");
            assert_eq!(taken(), (t0.0 + 1, t0.1 + n as u64), "one take-back of the job's bytes counted (give_back_spill_taken_back)");
        }
        None => no_take_back(&d, jobs, t0, "its shape, after a pool give"),
    }
    assert_eq!(reaping(), base, "REAPING back to its base");
}
