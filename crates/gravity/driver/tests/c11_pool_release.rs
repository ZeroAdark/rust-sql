//! pagepool P3 (C.11): with PGRUST_POOL_GRAVITY=1 a parked region's bytes are the page pool's. An idle trim
//! (the idle release on the memory runtime's loop thread, no pressure) gives them back to the pool WARM: no release, and
//! the next region takes them with ~0 faults. A trim under pressure (the ladder's take) releases them: inline on the loop
//! thread (`memledger::ladder_pass::on_ladder_thread`), through pagegive's remap (no MADV_DONTNEED over a whole 2 MiB
//! range, CVE-2026-74674); on a query thread only asked of the pool's purger, nothing released inline. And (b) a pool
//! reservation charges its ask's pages, never the span the pool served it. And R1's pool ask off the loop
//! thread is landing, never room, and never past the watchdog's cancel line; a reclaim's settle() returns the pool's warm
//! runs before its resample; and the driver never huge-advises a pool span. Its own binary: it sets the switch before the
//! pool's first read; the depot and the pool are process-wide, so its tests run one at a time.

use gravity_driver_rig as common;

use common::*;
use gravity_driver::mem::budget::{self, actuator, Cfg, Mode};
use gravity_driver::mem::reclaim;
use gravity_driver::mem::region::{commit_witness, reservation_witness};
use gravity_driver::{try_region, REGIONS};
use memledger::ladder_pass::mark_ladder_thread;
use pagepool::hooks::{self, Op};

const MIB: usize = 1 << 20;
const BYTES: usize = 64 * MIB;
const SUPER: usize = 2 * MIB;
const ADVICE_DONTNEED: i32 = 4;
const RUN: usize = 64 << 10;

/// One test at a time (the depot and the pool's counters are process-wide).
static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The switch on and the pool armed, once, before any test reads them.
fn on() -> std::sync::MutexGuard<'static, ()> {
    let one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    static SET: std::sync::Once = std::sync::Once::new();
    SET.call_once(|| {
        // SAFETY: under the Once, before the pool reads its switches (once, `arm_pool`); libtest reads no variable after
        unsafe { std::env::set_var("PGRUST_POOL_GRAVITY", "1") };
        arm_pool();
    });
    assert!(pool_armed(), "the pool serves the regions");
    one
}

/// This thread's minor faults (/proc/thread-self/stat, field 10).
fn minflt() -> u64 {
    let s = std::fs::read_to_string("/proc/thread-self/stat").unwrap();
    let rest = &s[s.rfind(')').unwrap() + 2..];
    rest.split(' ').nth(7).unwrap().parse().unwrap()
}

/// A region of `BYTES` from the pool, every page written.
fn written() -> gravity_driver::RegionBuf {
    let mut r = try_region(BYTES).expect("a region");
    let p = r.as_mut_ptr();
    for i in (0..BYTES).step_by(4096) {
        // SAFETY: inside the region's `BYTES` bytes
        unsafe { *p.add(i) = 1 };
    }
    r
}

/// `f` on a thread marked as the memory runtime's loop thread (`idle_loop` marks its own at its start).
fn on_loop_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::spawn(move || {
        mark_ladder_thread();
        f()
    })
    .join()
    .unwrap()
}

fn holds_whole_pmd(lo: usize, len: usize) -> bool {
    let p = lo.next_multiple_of(SUPER);
    p.checked_add(SUPER).is_some_and(|e| e <= lo + len)
}

#[test]
fn an_idle_trim_keeps_parked_pool_bytes_warm_and_a_ladder_take_releases_them_on_the_loop_thread_only() {
    let _one = on();
    REGIONS.clear();

    // idle, no pressure: the loop thread's idle release drops the parked region; its bytes go back to the pool warm
    REGIONS.put(written());
    let s0 = pagepool::stats();
    on_loop_thread(gravity_driver::mem::grant::idle_release);
    let s1 = pagepool::stats();
    assert_eq!(REGIONS.parked_bytes(), 0, "the idle release dropped the depot");
    assert_eq!((s1.releases, s1.released_bytes), (s0.releases, s0.released_bytes), "an idle trim releases nothing (0 remaps)");
    assert!(s1.warm_free() >= s0.warm_free() + BYTES / 2, "its bytes went back warm: {} -> {}", s0.warm_free(), s1.warm_free());
    // the next region takes them warm: its pages fault no more (a fresh one: 32 huge pages, or 16384 base pages)
    let f0 = minflt();
    let r = written();
    let faults = minflt() - f0;
    assert!(faults < 8, "{faults} faults writing a region the idle trim left warm");

    // pressure on the loop thread: the ladder's take drops the parked region and the pool releases its bytes now, by remap
    REGIONS.put(r);
    hooks::clear();
    let freed = on_loop_thread(|| gravity_driver::mem::reclaim::ladder_reclaim(BYTES as u64));
    let s2 = pagepool::stats();
    assert!(freed >= BYTES as u64, "the ladder's take dropped the parked region ({freed})");
    assert!(s2.released_bytes >= s1.released_bytes + (BYTES / 2) as u64, "released inline: {} -> {}", s1.released_bytes, s2.released_bytes);
    let (mut gives, mut bad) = (0, Vec::new());
    let complete = hooks::each(|op, at, len, arg| match op {
        Op::Madvise if arg == ADVICE_DONTNEED && holds_whole_pmd(at, len) => bad.push((at, len)),
        Op::GiveBack => gives += 1,
        _ => {}
    });
    assert!(complete, "the pool's syscall log overflowed");
    assert!(bad.is_empty(), "MADV_DONTNEED over a whole 2 MiB range: {bad:x?}");
    assert!(gives > 0, "the release went through pagegive::give_back");

    // pressure on a query thread (this one): the purger is asked (none runs in this test), nothing released inline
    REGIONS.put(written());
    let s3 = pagepool::stats();
    let freed = gravity_driver::mem::reclaim::ladder_reclaim(BYTES as u64);
    let s4 = pagepool::stats();
    assert!(freed >= BYTES as u64, "the ladder's take dropped the parked region ({freed})");
    assert_eq!((s4.releases, s4.released_bytes), (s3.releases, s3.released_bytes), "no remap on a query thread");
}

/// pagepool P3 (b): a pool reservation charges its ask's pages, as the mapped road does (`map_reserved`), never the span
/// the pool served it. A pool span's superpages past the ask, and a warm lazy span's commit from its last owner, go dead
/// in the pool (the purger's to release under pressure; on no owner's account), and come live again as `commit` opens
/// them. Before: a pool span was charged whole (up to `pool_lazy_min`), a warm lazy span with its last owner's commit
/// (the stress's idle session held 1 GiB that way).
#[test]
fn a_pool_reservation_charges_its_ask_not_the_span_it_took() {
    let _one = on();
    // a pool span (8 MiB, under pool_lazy_min): 64 KiB asked
    let d0 = pagepool::stats().dead;
    let (mut r, rw) = reservation_witness(RUN, 8 * MIB).expect("a pool reservation");
    assert_eq!(rw, RUN, "the ask's pages charged, not the span's");
    let d1 = pagepool::stats().dead;
    assert!(d1 >= d0 + 8 * MIB - 2 * RUN, "the span's pages past the ask are dead in the pool: {d0} -> {d1}");
    // a commit opens 3 MiB more: charged in 2 MiB steps, those pages live first
    let rw2 = commit_witness(&mut r, 3 * MIB).expect("a commit inside the reservation");
    assert!(rw2 >= RUN + 3 * MIB && rw2 <= RUN + 3 * MIB + SUPER, "charged to the opened extent: {rw2}");
    let d2 = pagepool::stats().dead;
    assert!(d2 + (rw2 - rw - RUN) <= d1, "the opened extent came live: {d1} -> {d2} (opened {})", rw2 - rw);
    // SAFETY: the ask and the commit are the region's writable bytes
    unsafe { core::ptr::write_bytes(r.as_mut_ptr(), 0x5a, RUN + 3 * MIB) };
    drop(r);
    assert_eq!(pagepool::stats().dead, d0, "the span went back whole: its dead runs live in the pool again");

    // a lazy span (512 MiB): its first owner commits 256 MiB, the next asks 64 KiB of a span of the same size
    let (first, rw) = reservation_witness(256 * MIB, 512 * MIB).expect("a lazy pool reservation");
    assert_eq!(rw, 256 * MIB);
    let base = first.as_ptr() as usize;
    drop(first);
    let d3 = pagepool::stats().dead;
    let (next, rw) = reservation_witness(RUN, 512 * MIB).expect("a lazy pool reservation");
    assert_eq!(rw, RUN, "the ask's pages charged, not the warm span's last commit");
    if next.as_ptr() as usize == base {
        let d4 = pagepool::stats().dead;
        assert!(d4 >= d3 + 256 * MIB - 2 * RUN, "the warm span's last commit is dead in the pool: {d3} -> {d4}");
    }
    drop(next);
    assert_eq!(pagepool::stats().dead, d3, "its dead runs live in the pool again at the drop");
}

/// The resident read for the R1 witness: a base plus the pool's claimed bytes (what it holds from the OS).
fn resident_now() -> Option<usize> {
    Some((1 << 30) + pagepool::stats().claimed)
}

/// Witnesses (i) and (ii): R1's pool ask off the loop thread (`pagepool::return_warm`: the watchdog's backstop
/// past its line, the ladder's R1) is LANDING, never room. With the budget on and the resident read a base plus the pool's
/// claimed bytes, a draw's make_room on this (query) thread with only the pool's warm bytes to free is refused before the
/// ask and while it lands (no remap on this thread; the reaper's landing credit, the ladder's, shows it); once the
/// purger's pass served it (released by remap, no whole-2 MiB MADV_DONTNEED) the read fell and the same draw passes.
#[test]
fn a_draw_never_counts_the_pools_landing_release_as_room() {
    let _one = on();
    REGIONS.clear();
    let _ = pagepool::serve_ask_now();
    let regions: Vec<_> = (0..4).map(|_| written()).collect();
    drop(regions);
    let warm = pagepool::releasable();
    assert!(warm >= 4 * BYTES, "four written regions went back warm: {warm}");
    mcx::governor::set_resident_probe(resident_now);
    budget::set_fallback_limit(32 << 30);
    budget::configure(Cfg { mode: Mode::On, actuators: actuator::ALL, ..Cfg::DEFAULT });
    let free = budget::snapshot().expect("the budget is on").free_b;
    let ask = free + BYTES as u64;
    assert!(!reclaim::make_room(ask), "only the pool's warm bytes could make the room: refused");

    // R1 off the loop thread: the purger is asked, nothing released here; landing until its pass
    let s0 = pagepool::stats();
    let landing = pagepool::return_warm();
    assert_eq!(landing, warm, "every releasable byte asked");
    assert_eq!((pagepool::stats().releases, pagepool::stats().released_bytes), (s0.releases, s0.released_bytes), "no remap on this thread");
    assert!(gravity_driver::mem::reap::landing_credit() >= landing as u64, "the ladder's landing credit shows the ask");
    assert!(!reclaim::make_room(ask), "landing is no room: still refused");

    // the purger's pass (this test starts no pool thread): released by remap, the read falls, the draw has its room
    hooks::clear();
    let got = pagepool::serve_ask_now();
    assert!(got >= 2 * BYTES, "the pass released the warm bytes: {got}");
    assert_eq!(pagepool::landing(u32::MAX), 0, "served: no longer landing");
    let (mut gives, mut bad) = (0, Vec::new());
    let complete = hooks::each(|op, at, len, arg| match op {
        Op::Madvise if arg == ADVICE_DONTNEED && holds_whole_pmd(at, len) => bad.push((at, len)),
        Op::GiveBack => gives += 1,
        _ => {}
    });
    assert!(complete, "the pool's syscall log overflowed");
    assert!(bad.is_empty(), "MADV_DONTNEED over a whole 2 MiB range: {bad:x?}");
    assert!(gives > 0, "the release went through pagegive::give_back");
    assert!(reclaim::make_room(ask), "released: the read fell by {got} bytes, the draw has its room");
    budget::configure(Cfg::DEFAULT);
}

/// settle() after a reclaim's cut: the pool's warm runs go back before the resample. Warm pool bytes
/// beside the parked region the cut drops: on the loop thread (`reclaim_fast` as the memory runtime's) settle releases
/// up to the cut more, inline and by remap (no whole-2 MiB MADV_DONTNEED); on any other thread nothing is released inline
/// and the purger is asked for every releasable byte (landing, `reap::landing_credit` shows it).
#[test]
fn a_reclaims_settle_returns_the_pools_warm_runs_before_the_resample() {
    let _one = on();
    REGIONS.clear();
    let _ = pagepool::serve_ask_now();

    // the loop thread: the Regions rung's own release (`pool_release`, the cut's bytes) plus settle's (up to the cut again)
    // four written regions back warm, then a fifth parked (it may take one of them: three at least stay warm)
    drop((0..4).map(|_| written()).collect::<Vec<_>>());
    REGIONS.put(written());
    assert!(pagepool::releasable() >= 3 * BYTES, "the written regions went back warm: {}", pagepool::releasable());
    hooks::clear();
    let s0 = pagepool::stats();
    let freed = on_loop_thread(|| reclaim::reclaim_fast(BYTES as u64));
    let s1 = pagepool::stats();
    assert!(freed >= BYTES as u64, "the cut dropped the parked region ({freed})");
    let released = s1.released_bytes - s0.released_bytes;
    assert!(released >= freed + (BYTES / 2) as u64, "settle released the pool's warm runs too: {released} for a cut of {freed}");
    let (mut gives, mut bad) = (0, Vec::new());
    let complete = hooks::each(|op, at, len, arg| match op {
        Op::Madvise if arg == ADVICE_DONTNEED && holds_whole_pmd(at, len) => bad.push((at, len)),
        Op::GiveBack => gives += 1,
        _ => {}
    });
    assert!(complete, "the pool's syscall log overflowed");
    assert!(bad.is_empty(), "MADV_DONTNEED over a whole 2 MiB range: {bad:x?}");
    assert!(gives > 0, "the release went through pagegive::give_back");

    // a query thread (this one): the purger asked for every releasable byte, nothing released here
    let _ = pagepool::serve_ask_now();
    drop((0..4).map(|_| written()).collect::<Vec<_>>());
    REGIONS.put(written());
    let s2 = pagepool::stats();
    let freed = reclaim::reclaim_fast(BYTES as u64);
    let s3 = pagepool::stats();
    assert!(freed >= BYTES as u64, "the cut dropped the parked region ({freed})");
    assert_eq!((s3.releases, s3.released_bytes), (s2.releases, s2.released_bytes), "no remap on a query thread");
    let landing = pagepool::landing(u32::MAX);
    assert!(landing >= 3 * BYTES, "settle asked the purger for the warm runs: {landing} landing");
    assert!(gravity_driver::mem::reap::landing_credit() >= landing as u64, "the ladder's landing credit shows the ask");
    let _ = pagepool::serve_ask_now();
    assert_eq!(pagepool::landing(u32::MAX), 0, "served: no longer landing");
}

static LADDER_READ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LADDER_CANCELS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The ladder's actuators for the past-cancel witness: a read that never falls, nothing to evict, each cancel counted.
static LADDER_ACTS: mcx::ladder::Acts = mcx::ladder::Acts {
    read: || Some(LADDER_READ.load(std::sync::atomic::Ordering::Relaxed)),
    reclaimable: || 0,
    reclaim: |_| 0,
    cancel: |_| {
        LADDER_CANCELS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        true
    },
    cancel_in_flight: || false,
    protected: || 0,
    evict_protected: |_| 0,
};

/// Past the watchdog's cancel line the pool's landing holds nothing off. The read is 128 MiB past the ladder's line and
/// the pool's R1 ask is landing more than that: under the cancel line the ladder thread's pass counts it as landing and
/// runs no rung; past it (`waive_wait`, memruntime's cancel test) the same pass climbs to R6 and raises a cancel, and the
/// flag ends with the pass.
#[test]
fn past_the_cancel_line_the_pools_landing_never_holds_off_r6() {
    use std::sync::atomic::Ordering::Relaxed;
    let _one = on();
    REGIONS.clear();
    let _ = pagepool::serve_ask_now();
    drop((0..4).map(|_| written()).collect::<Vec<_>>());
    let landing = pagepool::return_warm();
    assert!(landing >= 4 * BYTES, "the pool's R1 ask is landing: {landing}");

    const LINE: u64 = 8 << 30;
    LADDER_READ.store(LINE + 128 * MIB as u64, Relaxed);
    mcx::ladder::install(&LADDER_ACTS);
    mcx::ladder::set_unmap_landing_with_pool(gravity_driver::mem::reap::landing_credit);
    mcx::ladder::publish(mcx::ladder::ON, LINE);
    let now = mcx::governor::decide_ms();

    let under = mcx::ladder::hook_pass(now);
    assert_eq!(under.ran, 0, "under the cancel line the pool's landing covers the read: no rung");
    assert_eq!(LADDER_CANCELS.load(Relaxed), 0);

    mcx::ladder::waive_wait();
    assert!(mcx::ladder::past_cancel(), "the sample is past the cancel line");
    assert!(gravity_driver::mem::reap::landing_credit() < landing as u64, "past it the pool's landing is no credit");
    let past = mcx::ladder::hook_pass(now + 1);
    assert_ne!(past.ran & (1 << mempolicy::ladder::Rung::Cancel as u8), 0, "R6 ran: {:#b}", past.ran);
    assert!(LADDER_CANCELS.load(Relaxed) >= 1, "a cancel raised");
    assert!(!mcx::ladder::past_cancel(), "the flag ends with the pass");
    assert!(gravity_driver::mem::reap::landing_credit() >= landing as u64, "after the pass the pool's landing counts again");

    mcx::ladder::publish(mcx::ladder::OFF, 0);
    let _ = mcx::ladder::hook_pass(now + 2);
    let _ = pagepool::serve_ask_now();
}

/// The driver never huge-advises a page pool span: the pool maps its superpages with its own advice, and a madvise on the
/// query thread that binds a region can take milliseconds. A region the pool serves takes no advice from `advise_huge`;
/// allocator memory still does wherever the kernel has transparent huge pages.
#[test]
fn the_driver_never_huge_advises_a_pool_span() {
    let _one = on();
    let mut r = written();
    assert!(pagepool::owns(r.as_mut_ptr()), "the region is the pool's");
    assert!(!gravity_driver::mem::region::advise_huge_witness(r.as_mut_ptr(), BYTES), "no advice over a pool span");
    let (mut lazy, _) = reservation_witness(RUN, 64 * MIB).expect("a lazy pool reservation");
    assert!(pagepool::owns(lazy.as_mut_ptr()), "the reservation is the pool's");
    assert!(!gravity_driver::mem::region::advise_huge_witness(lazy.as_mut_ptr(), 64 * MIB), "no advice over a pool reservation");
    drop(lazy);
    let thp = std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/hpage_pmd_size").ok().and_then(|t| t.trim().parse::<usize>().ok()).is_some_and(|h| h > 4096);
    let mut v = vec![0u8; 8 * MIB];
    assert!(!pagepool::owns(v.as_mut_ptr()), "the allocator's, not the pool's");
    assert_eq!(gravity_driver::mem::region::advise_huge_witness(v.as_mut_ptr(), v.len()), thp, "allocator memory advised as before");
}
