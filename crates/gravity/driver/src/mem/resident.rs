//! The Gravity regions on the residency registry (`memledger::resident`): every region of at least
//! `mempolicy::watchdog::RESIDENCY_MIN` bytes is listed at the region events that already make a system call or take a
//! depot lock; the pass (memruntime's sampler, and a view before it reads the accounts) reads them with mincore.

use std::sync::atomic::{AtomicU64, Ordering};

use pg_clock::MonoStamp;

pub(crate) use memledger::resident::{resident_of, unregister, update_at, NONE};

/// Passes run, their total time, the last one's and the longest one's (ns), and the regions and bytes the last one read.
pub static PASSES: AtomicU64 = AtomicU64::new(0);
pub static PASS_NS_SUM: AtomicU64 = AtomicU64::new(0);
pub static PASS_NS_LAST: AtomicU64 = AtomicU64::new(0);
pub static PASS_NS_MAX: AtomicU64 = AtomicU64::new(0);
pub static PASS_REGIONS_LAST: AtomicU64 = AtomicU64::new(0);
pub static PASS_SCANNED_LAST: AtomicU64 = AtomicU64::new(0);

/// `NONE` under `RESIDENCY_MIN`: its account counts it whole. LEDGER2 design A: its not-resident bytes book on its charge's
/// owner too (`home`: `Charge::nr_home`).
pub(crate) fn register(lo: usize, span: usize, charged: usize, acct: memledger::Acct, mapped: bool, fresh: bool, home: memledger::charge::NrHome) -> u32 {
    if charged < mempolicy::watchdog::RESIDENCY_MIN {
        return NONE;
    }
    memledger::resident::register_at(lo, span, charged, acct, mapped, fresh, home)
}

/// The sampler's pass: at most `RESIDENCY_READ_BYTES` of unchanged, partly resident ranges. False when its scratch
/// could not be had.
pub fn pass() -> bool {
    run(mempolicy::watchdog::RESIDENCY_READ_BYTES)
}

/// A view's pass: every range it would read, whatever their size.
pub fn pass_all() -> bool {
    run(usize::MAX)
}

fn run(budget: usize) -> bool {
    let t0 = MonoStamp::now();
    let Some(read) = memledger::resident::pass(pagegive::resident_bytes, budget) else { return false };
    let ns = t0.elapsed_ns();
    PASSES.fetch_add(1, Ordering::Relaxed);
    PASS_NS_SUM.fetch_add(ns, Ordering::Relaxed);
    PASS_NS_LAST.store(ns, Ordering::Relaxed);
    PASS_REGIONS_LAST.store(read.regions, Ordering::Relaxed);
    PASS_NS_MAX.fetch_max(ns, Ordering::Relaxed);
    PASS_SCANNED_LAST.store(read.bytes, Ordering::Relaxed);
    true
}
