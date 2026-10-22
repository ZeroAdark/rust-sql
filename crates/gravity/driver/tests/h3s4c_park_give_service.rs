//! H3 step (4) C1-c (give_back_spill under give_back_service): a breaker's reservation that committed past its share in
//! place, put into the region depot, parks after the give-back service gave its committed pages back (`giveback::park`):
//! - E1 / E3 at the put: the mapping's writable tail leaves its charge for REAPING (the charged delta), with its own
//!   residency entry; E2: the rest is the depot's at the put, so (e) the session that unbinds with the job still queued
//!   leaves nothing on its account;
//! - the fence: until the job parks it, a take of its shape misses (counted) and never gets it;
//! - the job, on the service's thread: the committed pages given back before their pool bytes (the give probe), REAPING
//!   down by the tail, the region parked; a take after it gets the region with the tail closed (PROT_NONE);
//! - a trim never waits for a park job queued before it (take-back TB3): a depot trim and a draw's trim (`reap::drain`)
//!   go on at once without the region, never giving the job back themselves; the service parks it.
//! Its own [[test]]: a #[global_allocator] (door 1, as the server counts it), process-wide knobs and hooks.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gravity_driver::mem::giveback;
use gravity_driver::mem::region::{park_witness_region, park_witness_take};
use gravity_driver::{POOL, REGIONS};
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

const MIB: usize = 1 << 20;
/// The reservation's share (well past the fold slack, so E2's move reads exactly), its address space and its commit.
const SHARE: usize = 4 * MIB;
const RESERVE: usize = 96 * MIB;
const MORE: usize = 8 * MIB;
/// What a thread's own fold may still hold back on an account, and a page or two of the rig's own.
const SLACK: i64 = 512 << 10;

static STALLED: AtomicBool = AtomicBool::new(false);

fn stalled() -> bool {
    STALLED.load(Ordering::Relaxed)
}

fn wake() {}

/// The give probe's reads at the last give: the pool's held bytes and the given range's resident bytes.
static PROBES: AtomicU64 = AtomicU64::new(0);
static PROBE_HELD: AtomicU64 = AtomicU64::new(0);
static PROBE_RESIDENT: AtomicU64 = AtomicU64::new(0);

fn probe(at: usize, give: usize) {
    // the range a give hands back: its whole 64 KiB pages (`region::dontneed`)
    const PAGE: usize = 64 << 10;
    let (lo, hi) = (at.next_multiple_of(PAGE), (at + give) / PAGE * PAGE);
    let mut buf = [0u8; 4096];
    PROBE_HELD.store(POOL.held(), Ordering::Relaxed);
    PROBE_RESIDENT.store(pagegive::resident_bytes(lo, hi.max(lo), &mut buf).unwrap_or(0) as u64, Ordering::Relaxed);
    PROBES.fetch_add(1, Ordering::Relaxed);
}

fn reaping() -> i64 {
    memledger::fold();
    memledger::account_row(Acct::REAPING).account_bytes
}

fn account(a: Acct) -> i64 {
    memledger::fold();
    memledger::account_row(a).account_bytes
}

/// The protection (`/proc/self/maps` perms, read / write / execute) of the mapping that holds `addr`.
fn prot_at(addr: usize) -> String {
    let maps = std::fs::read_to_string("/proc/self/maps").expect("/proc/self/maps");
    for line in maps.lines() {
        let mut f = line.split_whitespace();
        let (Some(range), Some(perms)) = (f.next(), f.next()) else { continue };
        let Some((lo, hi)) = range.split_once('-') else { continue };
        let (lo, hi) = (usize::from_str_radix(lo, 16).unwrap(), usize::from_str_radix(hi, 16).unwrap());
        if (lo..hi).contains(&addr) {
            return perms[..3].to_string();
        }
    }
    panic!("no mapping holds {addr:#x}");
}

/// What one put on a backend's session saw.
#[derive(Debug)]
struct Put {
    p: usize,
    len: usize,
    rw: usize,
    c0: giveback::Counts,
    c1: giveback::Counts,
    /// After a take of the region's shape while its job waits.
    c2: giveback::Counts,
    took: Option<usize>,
    r: (i64, i64),
    /// The session's account before the region was made, and after the session unbound.
    session: (i64, i64),
}

/// Backend `procno` binds, makes a reservation that committed past its share, puts it, tries a take of its shape and
/// unbinds, the park job still queued.
fn put_on_session(procno: u32) -> Put {
    std::thread::spawn(move || {
        memledger::bind_backend(procno, 9000 + procno);
        let acct = Acct::backend(procno);
        let s0 = account(acct);
        let r = park_witness_region(SHARE, RESERVE, MORE);
        let (p, len, rw) = (r.as_ptr() as usize, r.len(), r.writable());
        let (c0, r0) = (giveback::counts(), reaping());
        REGIONS.put(r);
        let (c1, r1) = (giveback::counts(), reaping());
        let took = park_witness_take(SHARE, RESERVE).map(|b| b.as_ptr() as usize);
        let c2 = giveback::counts();
        memledger::unbind_backend();
        Put { p, len, rw, c0, c1, c2, took, r: (r0, r1), session: (s0, account(acct)) }
    })
    .join()
    .unwrap()
}

/// The service's batch on a thread of its own: its time, REAPING before and after, the pool's held bytes after, the
/// lane's counters after, and the gives it probed.
fn service_batch() -> (i64, i64, u64, giveback::Counts, u64) {
    std::thread::spawn(|| {
        let (p0, r0) = (PROBES.load(Ordering::Relaxed), reaping());
        allocarena::own::drain_jobs_upto(usize::MAX);
        (r0, reaping(), POOL.held(), giveback::counts(), PROBES.load(Ordering::Relaxed) - p0)
    })
    .join()
    .unwrap()
}

/// A trim that does not wait for the service returns well inside this.
const NO_WAIT: Duration = Duration::from_millis(50);

#[test]
fn h3s4c_a_park_gives_its_committed_pages_back_on_the_service_then_parks_and_a_take_meanwhile_misses() {
    mempolicy::consts::set(K::GiveBackService, 1);
    assert_eq!(mempolicy::consts::get(K::GiveBackSpill), 1, "give_back_spill defaults to 1 under the service's knob");
    allocarena::own::set_service(stalled, 8);
    allocarena::own::set_release_wake(wake);
    giveback::set_give_probe(probe);
    let base = reaping();
    // the share is whole pages under a 4 KiB or a 64 KiB kernel page: `uncommit` keeps it writable, the rest closes
    let keep = SHARE;

    // the put: one park job, E1-E3 made there
    let put = put_on_session(4071);
    assert!(put.rw >= put.len + MORE && put.len == SHARE, "a mapped reservation, its commit writable: {put:?}");
    let tail = (put.rw - keep) as i64;
    assert_eq!((put.c1.park_jobs - put.c0.park_jobs, put.c1.jobs - put.c0.jobs), (1, 0), "one park job: {put:?}");
    assert_eq!(put.r.1 - put.r.0, tail, "E1: REAPING up by the tail's charged bytes: {put:?}");
    assert_eq!(put.c1.landing - put.c0.landing, tail as u64, "the lane's charged landing is the tail");
    // the fence: a take of its shape misses while the job waits (counted) and never gets the region
    assert_eq!(put.took, None, "a take meanwhile gets nothing: {put:?}");
    assert_eq!(put.c2.park_take_misses - put.c1.park_take_misses, 1, "the miss counted: {put:?}");
    // (e): the session unbound with its job queued: E2 moved the share to the depot at the put, E1 the tail to REAPING
    assert!((put.session.1 - put.session.0).abs() <= SLACK, "(e) the unbound session's account holds nothing of the region: {put:?}");
    assert_eq!(reaping() - base, giveback::charged_landing_bytes() as i64, "A7 with the job queued");

    // a depot trim never waits for the job queued before it (take-back TB3) and never gives it back itself
    let t0 = Instant::now();
    let c0 = giveback::counts();
    REGIONS.trim();
    let c1 = giveback::counts();
    assert!(t0.elapsed() < NO_WAIT, "the trim did not wait for the service");
    assert_eq!((c1.park_jobs - c1.parked, c1.parked, c1.landing), (c0.park_jobs - c0.parked, c0.parked, c0.landing), "the job still the service's: {c1:?}");
    assert_eq!(c1.park_drained_here, c0.park_drained_here, "the trim gave nothing back itself");

    // the job, on the service's thread: pages first, then the pool bytes, the tail off REAPING, the region parked
    let (r0, r1, held1, c, probes) = service_batch();
    assert_eq!((c.parked - c1.parked, c.landing, c.pending), (1, 0, 0), "the service parked it: {c:?}");
    assert_eq!(r1 - r0, -tail, "REAPING down by the tail");
    assert_eq!(probes, 1, "one give");
    assert_eq!(PROBE_RESIDENT.load(Ordering::Relaxed), 0, "the committed pages given back before their pool bytes");
    assert!(PROBE_HELD.load(Ordering::Relaxed) >= held1 + MORE as u64, "the pool's room rose only after the pages were given");
    assert_eq!(c.park_protect_refused, 0);
    assert_eq!(prot_at(put.p + keep), "---", "the parked region's tail is closed again");
    assert_eq!(prot_at(put.p), "rw-", "its share stays writable");
    assert_eq!(reaping() - base, giveback::charged_landing_bytes() as i64, "A7 after the job");

    // a take after the job gets the region: its share, nothing committed, the tail closed
    let misses = giveback::counts().park_take_misses;
    let again = park_witness_take(SHARE, RESERVE).expect("the parked reservation");
    assert_eq!((again.as_ptr() as usize, again.len(), again.committed(), again.writable()), (put.p, SHARE, 0, keep), "the region back, as uncommit leaves it");
    assert_eq!(giveback::counts().park_take_misses, misses, "no miss once the job parked it");
    drop(again);

    // a draw's trim (`reap::drain`) with no service draining: no wait, nothing given back on its own thread, the job still
    // the service's, which parks the region
    let put = put_on_session(4074);
    assert_eq!(put.c1.park_jobs - put.c0.park_jobs, 1, "one park job: {put:?}");
    let (p0, c0, r0, t0) = (PROBES.load(Ordering::Relaxed), giveback::counts(), reaping(), Instant::now());
    gravity_driver::mem::reap::drain();
    let (c1, r1) = (giveback::counts(), reaping());
    assert!(t0.elapsed() < NO_WAIT, "the draw's trim did not wait for the service");
    assert_eq!((c1.park_drained_here - c0.park_drained_here, c1.parked, c1.landing, r1 - r0), (0, c0.parked, c0.landing, 0), "the trim gave nothing back itself: {c1:?}");
    assert_eq!(PROBES.load(Ordering::Relaxed) - p0, 0, "no give on the trim's thread");
    let (r0, r1, _, c, probes) = service_batch();
    assert_eq!((c.parked - c1.parked, c.landing, probes), (1, 0, 1), "the service parked it: {c:?}");
    assert_eq!(r1 - r0, -((put.rw - keep) as i64), "REAPING down by the tail");
    assert_eq!(PROBE_RESIDENT.load(Ordering::Relaxed), 0, "its pages given back before its pool bytes");
    assert_eq!(prot_at(put.p + keep), "---", "parked with its tail closed");
    drop(park_witness_take(SHARE, RESERVE).expect("parked by the service"));

    // past the cancel line a put parks on its caller as before (counted, no job)
    mcx::ladder::publish_cancel_line(1000);
    mcx::ladder::publish_read(2000);
    let put = put_on_session(4072);
    mcx::ladder::publish_cancel_line(0);
    mcx::ladder::publish_read(0);
    assert_eq!((put.c1.park_jobs - put.c0.park_jobs, put.c1.park_inline_past_cancel - put.c0.park_inline_past_cancel), (0, 1), "S1: no job past the line: {put:?}");
    assert_eq!(put.r.1, put.r.0, "S1: nothing on REAPING");
    assert!(put.took.is_some(), "parked at once: a take gets it");

    // give_back_spill = 0: as before
    mempolicy::consts::set(K::GiveBackSpill, 0);
    let put = put_on_session(4073);
    mempolicy::consts::set(K::GiveBackSpill, 1);
    assert_eq!(put.c1, put.c0, "give_back_spill 0: nothing here runs");
    assert!(put.took.is_some(), "parked at once");
    REGIONS.trim();
    assert_eq!(reaping() - base, 0, "REAPING back to its base");
}
