//! H3 step (4) (i) (give_back_spill under give_back_service; GIVEBACK-SERVICE-DESIGN step (4)): a spill region given back
//! before the statement's end (`spill_host::v4::give`, C1-a) goes to the give-back service as one job. Memory accounting's
//! witnesses, through a worker's context as a spilled join's reload gives its region back:
//! (a) across the enqueue the owner's account goes down by exactly the job's bytes (today's inline delta) and REAPING up
//!     by them;
//! (b) across the job's give REAPING goes down by them and the dropping thread's account nets 0, whichever thread it is:
//!     the service's (its own unbound binding), or a trim's past the cancel line;
//! (c) A7: REAPING less its base == the own runs' landing + the job lane's at each boundary, and back at the end;
//! (d) the giving thread gives no page back: the region is still resident after its give, until the service's drain;
//! and on every give the pool's room rises only after the region's pages are given (the give probe).
//! A draw's trim never waits for the service (take-back TB3): with no service draining, and with the service stalled, it
//! returns at once, gives nothing back on its own thread and leaves the job the service's; past the cancel line (S1) it
//! gives the jobs queued before it back itself, pages first (counted). S1: past the line a give runs on its caller as
//! before (counted, no job). With give_back_spill = 0 nothing here runs.
//! The service is memruntime's pg:memrelease in the server; here its hooks are set as its start sets them
//! (`allocarena::own::set_service`, the release wake) and its batch is `allocarena::own::drain_jobs_upto` on a thread of
//! its own. Its own [[test]]: a #[global_allocator] (door 1, as the server counts it), process-wide knobs and hooks.
//! pagepool P3 (branch malisper/pagepool; PGRUST_POOL_GRAVITY=1): when the page pool backs the region (`v4::is_pool`, the
//! region's own flag) its give is no job: each give then asserts the pool's path instead (`pool_give`: back to the pool,
//! no give_back_spill_* count moved, the owner's account released, REAPING and PARKED unchanged), and each trim that
//! would have waited for or given back the job asserts that it found none (`no_lane`).

use gravity_driver_rig as common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::mem::giveback;
use gravity_driver::mem::spill_host::v4;
use gravity_driver::{footprint, Claimed, Entry, Error, Grant, Open, Step, POOL, REGIONS};
use memledger::Acct;
use mempolicy::consts::K;

/// Door 1, as the server's allocator wrapper counts it (init_small oom.rs): a heap charge moves bytes the allocator counted
/// on the allocating thread's account, so this binary counts them too.
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
/// One reload region: well past a thread's fold slack, so the moves read exactly.
const REGION: usize = 8 << 20;
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

static WOKEN: AtomicU64 = AtomicU64::new(0);

fn wake() {
    WOKEN.fetch_add(1, Ordering::Relaxed);
}

/// The service's dead-man check, as the test sets it.
static STALLED: AtomicBool = AtomicBool::new(false);

fn stalled() -> bool {
    STALLED.load(Ordering::Relaxed)
}

/// The give probe's reads: gives seen, and at the last one the pool's held bytes and the region's bytes still resident.
static PROBES: AtomicU64 = AtomicU64::new(0);
static PROBE_HELD: AtomicU64 = AtomicU64::new(0);
static PROBE_RESIDENT: AtomicU64 = AtomicU64::new(0);

fn probe(at: usize, give: usize) {
    // the range a give hands back: its whole 64 KiB pages (`region::dontneed`)
    const PAGE: usize = 64 << 10;
    let (lo, hi) = (at.next_multiple_of(PAGE), (at + give) / PAGE * PAGE);
    PROBE_HELD.store(POOL.held(), Ordering::Relaxed);
    PROBE_RESIDENT.store(resident(lo, hi.saturating_sub(lo)) as u64, Ordering::Relaxed);
    PROBES.fetch_add(1, Ordering::Relaxed);
}

/// Every account's bytes now (the calling thread's counts folded first).
fn accounts() -> HashMap<u32, i64> {
    let mut m = HashMap::new();
    memledger::for_each_row(|r| {
        m.insert(r.acct.0, r.account_bytes);
    });
    m
}

fn reaping() -> i64 {
    memledger::fold();
    memledger::account_row(Acct::REAPING).account_bytes
}

/// pagepool P3: the depots' account (the ledger's PARKED).
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

/// pagepool P3: one give of a region the pool backs (`Gave::pooled`), checked as the pool's path: no job and no
/// give_back_spill_* count moved, REAPING and PARKED unchanged, the owner's account released by the region's bytes (and
/// nothing else moved past the fold slack), the block out of the pool's in-use region bytes and back among its free ones.
fn pool_give(g: &Gave, at: &str) {
    assert!(g.pooled, "{at}: the pool backs the region: {g:?}");
    assert_eq!(g.c1, g.c0, "{at}: a pool give is no job and moves no give_back_spill_* count: {g:?}");
    assert_eq!((g.r1 - g.r0, g.parked.1 - g.parked.0), (0, 0), "{at}: REAPING and PARKED unchanged: {g:?}");
    assert!(g.moved.iter().any(|(_, d)| (d + REGION as i64).abs() <= SLACK), "{at}: the owner's account released by the region's bytes: {:?}", g.moved);
    assert!(g.moved.iter().filter(|(_, d)| (d + REGION as i64).abs() > SLACK).all(|(_, d)| d.abs() <= SLACK), "{at}: nothing else moved past the fold slack: {:?}", g.moved);
    assert!(g.in_use.0 >= g.in_use.1 + REGION, "{at}: the block left the pool's in-use region bytes: {g:?}");
    assert!(g.free.1 >= g.free.0 + REGION as u64, "{at}: the block back among the pool's free bytes (or released by its purger): {g:?}");
}

/// pagepool P3: a trim after a pool give found no job: it waited for none (under `within`), gave none back (no
/// give_back_spill_* count moved, no give probed) and REAPING stayed.
fn no_lane(d: &Dropped, within: Duration, at: &str) {
    assert!(d.took < within, "{at}: no job to wait for: {d:?}");
    assert_eq!(d.c1, d.c0, "{at}: no give_back_spill_* count moved: {d:?}");
    assert_eq!((d.r.1 - d.r.0, d.probes), (0, 0), "{at}: REAPING unchanged, no give: {d:?}");
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

/// The bytes of `[p, p + len)` resident now (its whole pages).
fn resident(p: usize, len: usize) -> usize {
    let mut buf = [0u8; 4096];
    pagegive::resident_bytes(p, p + len, &mut buf).unwrap_or(0)
}

/// What one give through a worker's context saw on its thread.
#[derive(Debug)]
struct Gave {
    /// The region's base and the job lane's counters before and after the give.
    p: usize,
    c0: giveback::Counts,
    c1: giveback::Counts,
    /// REAPING before and after; every other account that moved, and by how much.
    r0: i64,
    r1: i64,
    moved: Vec<(u32, i64)>,
    /// The region's bytes still resident right after its give (only read when a job holds its block).
    resident_after: usize,
    /// pagepool P3: the region is the page pool's (`v4::is_pool`); PARKED before and after; the pool's in-use region bytes
    /// and its free bytes (warm and stranded, plus what its purger released) before and after the give.
    pooled: bool,
    parked: (i64, i64),
    in_use: (usize, usize),
    free: (u64, u64),
}

/// One run of a W2 statement whose worker 0, from a thread of its own, takes a reload region (`v4::take`), writes every
/// page of it and gives it back (`v4::give`); the pool armed with room past W for it.
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
                let (c0, before, r0, k0, s0) = (giveback::counts(), accounts(), reaping(), parked(), pool_now());
                // SAFETY: a `take` of this owner, read by nothing from now on
                unsafe { v4::give(ctx, p) };
                let (c1, after, r1, k1, s1) = (giveback::counts(), accounts(), reaping(), parked(), pool_now());
                let resident_after = if c1.jobs > c0.jobs { resident(p as usize, REGION) } else { 0 };
                Gave { p: p as usize, c0, c1, r0, r1, moved: moved(&before, &after), resident_after, pooled, parked: (k0, k1), in_use: (s0.0, s1.0), free: (s0.1, s1.1) }
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
    // no trim here: a depot trim past the cancel line gives the lane's jobs back (`reap::drain`), which the caller reads
    gave
}

/// What a drop of the lane's jobs saw on the dropping thread: its time, REAPING and that thread's account before and
/// after, the pool's held bytes before and after, the lane's counters before and after, and the give probe's reads.
#[derive(Debug)]
struct Dropped {
    took: Duration,
    r: (i64, i64),
    own: (i64, i64),
    held: (u64, u64),
    c0: giveback::Counts,
    c1: giveback::Counts,
    probes: u64,
    probe_held: u64,
    probe_resident: u64,
}

/// `f` on a thread of its own (unbound, as the service's and as a backend's between statements), read as `Dropped`.
fn on_own_thread(f: fn()) -> Dropped {
    std::thread::spawn(move || {
        assert_eq!(memledger::current(), Acct::UNBOUND, "a thread of its own is on its unbound binding");
        let own = || {
            memledger::fold();
            memledger::account_row(Acct::UNBOUND).account_bytes
        };
        let p0 = PROBES.load(Ordering::Relaxed);
        let (c0, r0, u0, h0) = (giveback::counts(), reaping(), own(), POOL.held());
        let t0 = Instant::now();
        f();
        let took = t0.elapsed();
        let (c1, r1, u1, h1) = (giveback::counts(), reaping(), own(), POOL.held());
        Dropped {
            took,
            r: (r0, r1),
            own: (u0, u1),
            held: (h0, h1),
            c0,
            c1,
            probes: PROBES.load(Ordering::Relaxed) - p0,
            probe_held: PROBE_HELD.load(Ordering::Relaxed),
            probe_resident: PROBE_RESIDENT.load(Ordering::Relaxed),
        }
    })
    .join()
    .unwrap()
}

/// The service's batch over every job waiting.
fn service_batch() {
    allocarena::own::drain_jobs_upto(usize::MAX);
}

/// A draw's trim of the region depot (`RegionDepot::trim`: its parked regions, then `reap::drain`).
fn draw_trim() {
    REGIONS.trim();
}

/// (b) and the probe for one job of `n` bytes dropped on `d`'s thread: REAPING down by them, the dropping thread nets 0,
/// the pool's held bytes still had the region's at the give's probe, after its pages were given back.
fn dropped_one(d: &Dropped, n: i64, at: &str) {
    assert_eq!(d.r.1 - d.r.0, -n, "(b) {at}: REAPING down by the job's bytes: {d:?}");
    assert!((d.own.1 - d.own.0).abs() <= SLACK, "(b) {at}: the dropping thread nets 0: {d:?}");
    assert_eq!(d.probes, 1, "{at}: one give: {d:?}");
    assert_eq!(d.probe_resident, 0, "{at}: the region's pages given back before its pool bytes: {d:?}");
    assert!(d.probe_held >= d.held.1 + REGION as u64, "{at}: the pool's room rose only after the pages were given: {d:?}");
    assert_eq!(d.c1.landing, 0, "{at}: the lane is empty");
}

/// A7: REAPING less its base is the own runs' landing plus the job lane's.
fn a7(base: i64, at: &str) {
    let landing = (allocarena::own::landing_bytes() + giveback::charged_landing_bytes()) as i64;
    assert_eq!(reaping() - base, landing, "A7 at {at}: REAPING - base == own LANDING + queued job charges");
}

/// One give, checked as one job of the region's charge: its bytes. pagepool P3: a region the pool backs is checked as the
/// pool's give instead (`pool_give`): None.
fn one_job(at: &str) -> Option<i64> {
    let g = give_one();
    if g.pooled {
        pool_give(&g, at);
        None
    } else {
        let n = (g.c1.job_bytes - g.c0.job_bytes) as i64;
        assert_eq!(g.c1.jobs - g.c0.jobs, 1, "{at}: the give is one job: {g:?}");
        assert!(n >= REGION as i64, "{at}: its bytes are the region's charge: {n}");
        assert_eq!(g.r1 - g.r0, n, "(a) {at}: REAPING up by the job's bytes: {g:?}");
        Some(n)
    }
}

/// A trim that does not wait for the service returns well inside this.
const NO_WAIT: Duration = Duration::from_millis(50);

#[test]
fn h3s4_a_spill_give_is_one_service_job_and_a_draw_never_waits_for_it() {
    // the server's start, as memruntime's service makes it: the knob, then the admission and the release wake
    mempolicy::consts::set(K::GiveBackService, 1);
    assert_eq!(mempolicy::consts::get(K::GiveBackSpill), 1, "give_back_spill defaults to 1 under the service's knob");
    allocarena::own::set_service(stalled, 8);
    allocarena::own::set_release_wake(wake);
    giveback::set_give_probe(probe);
    let base = reaping();
    a7(base, "the start");

    // under the cancel line: one job, given back by the service's batch
    let w0 = WOKEN.load(Ordering::Relaxed);
    let g = give_one();
    if g.pooled {
        // pagepool P3: the pool's give, and the service's batch finds nothing queued
        pool_give(&g, "under the cancel line");
        a7(base, "the pool give");
        let d = on_own_thread(service_batch);
        no_lane(&d, NO_WAIT, "the service's batch after a pool give");
        a7(base, "the service's batch after a pool give");
    } else {
        let n = (g.c1.job_bytes - g.c0.job_bytes) as i64;
        assert_eq!(g.c1.jobs - g.c0.jobs, 1, "the give is one job: {g:?}");
        assert!(n >= REGION as i64, "its bytes are the region's charge: {n}");
        // (a)
        assert_eq!(g.r1 - g.r0, n, "(a) REAPING up by the job's bytes: {g:?}");
        // the owner's row (the worker's context owner) by the job's bytes; on the giving thread's own account a few rig and
        // test bytes beside them, within the fold slack
        assert!(g.moved.iter().any(|(_, d)| (d + n).abs() <= SLACK), "(a) the owner's account down by the job's bytes: {:?}", g.moved);
        assert!(g.moved.iter().filter(|(_, d)| (d + n).abs() > SLACK).all(|(_, d)| d.abs() <= SLACK), "(a) nothing else moved past the fold slack: {:?}", g.moved);
        // (d)
        assert!(g.resident_after + 2 * 4096 >= REGION, "(d) the giving thread gave no page back: {} of {REGION} resident at {:#x}", g.resident_after, g.p);
        assert_eq!(g.c1.landing, n as u64, "the lane's landing is the job's bytes");
        assert!(WOKEN.load(Ordering::Relaxed) > w0, "the service woken for the lane's first job");
        a7(base, "the enqueue");
        // (b), on the service's thread
        let d = on_own_thread(service_batch);
        assert_eq!((d.c1.given - d.c0.given, (d.c1.given_bytes - d.c0.given_bytes) as i64), (1, n), "the service's batch gave the job back: {d:?}");
        dropped_one(&d, n, "the service's give");
        a7(base, "the service's give");
    }

    // a draw's trim never waits for the service (take-back TB3): with no service draining it returns at once, gives nothing
    // back on its own thread and leaves the job the service's
    let job = one_job("no wait");
    let d = on_own_thread(draw_trim);
    if let Some(n) = job {
        assert!(d.took < NO_WAIT, "the trim did not wait for the service: {d:?}");
        assert_eq!((d.probes, d.c1.given, d.c1.landing), (0, d.c0.given, n as u64), "nothing given back on the trim's thread, the job still queued: {d:?}");
        assert_eq!(d.r.1, d.r.0, "REAPING unchanged across the trim: {d:?}");
    } else {
        no_lane(&d, NO_WAIT, "no wait, after a pool give");
    }
    a7(base, "a trim with the job queued");
    // the service stalled (its dead-man check): the same
    STALLED.store(true, Ordering::Relaxed);
    let d = on_own_thread(draw_trim);
    STALLED.store(false, Ordering::Relaxed);
    if let Some(n) = job {
        assert!(d.took < NO_WAIT, "no wait for a stalled service: {d:?}");
        assert_eq!((d.probes, d.c1.landing), (0, n as u64), "the job still the service's with the service stalled: {d:?}");
    } else {
        no_lane(&d, NO_WAIT, "the service stalled, after a pool give");
    }
    // the service's batch gives it back
    let d = on_own_thread(service_batch);
    if let Some(n) = job {
        assert_eq!((d.c1.given - d.c0.given, (d.c1.given_bytes - d.c0.given_bytes) as i64), (1, n), "the service's batch gave the job back: {d:?}");
        dropped_one(&d, n, "the service's give after the trims");
    } else {
        no_lane(&d, NO_WAIT, "the service's batch after the trims, after a pool give");
    }
    a7(base, "the service's give after the trims");

    // past the cancel line the trim gives the job back at once, pages first (S1), counted apart
    let job = one_job("the trim past the line");
    mcx::ladder::publish_cancel_line(1000);
    mcx::ladder::publish_read(2000);
    let d = on_own_thread(draw_trim);
    mcx::ladder::publish_cancel_line(0);
    mcx::ladder::publish_read(0);
    if let Some(n) = job {
        assert_eq!((d.c1.drained_here_past_cancel - d.c0.drained_here_past_cancel, (d.c1.drained_here_past_cancel_bytes - d.c0.drained_here_past_cancel_bytes) as i64), (1, n), "S1: past the line the trim gave it back: {d:?}");
        assert!(d.took < NO_WAIT, "S1: no wait: {d:?}");
        dropped_one(&d, n, "the trim past the line");
    } else {
        no_lane(&d, NO_WAIT, "the trim past the line, after a pool give");
    }
    a7(base, "the trim past the line");

    // S1: past the watchdog's cancel line, on the caller as before
    mcx::ladder::publish_cancel_line(1000);
    mcx::ladder::publish_read(2000);
    assert!(mcx::ladder::past_cancel_line());
    let g = give_one();
    if g.pooled {
        pool_give(&g, "S1: past the line");
    } else {
        assert_eq!((g.c1.jobs - g.c0.jobs, g.c1.inline_past_cancel - g.c0.inline_past_cancel), (0, 1), "S1: past the line no job, one give on its caller: {g:?}");
        assert_eq!((g.c1.landing, g.r1 - g.r0), (0, 0), "S1: nothing waits, nothing on REAPING");
    }
    mcx::ladder::publish_cancel_line(0);
    mcx::ladder::publish_read(0);
    assert!(!mcx::ladder::past_cancel_line());

    // give_back_spill = 0: as with the service's knob off
    mempolicy::consts::set(K::GiveBackSpill, 0);
    let g = give_one();
    assert_eq!(g.c1, g.c0, "give_back_spill 0: nothing here runs");
    if g.pooled {
        pool_give(&g, "give_back_spill 0");
    }
    mempolicy::consts::set(K::GiveBackSpill, 1);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    a7(base, "the end");
    assert_eq!(POOL.held(), 0, "every region, chunk, W and job's pool bytes released");
    POOL.arm(0);
}
