//! RECLAIMDEFER (i′): what a reclaim does while it holds a lock, counted process-wide (`pgrc_compact_stats`'s
//! `reclaim=` and `reclaim_holds=`). A table's publish lock serializes its publishes, and `CATALOG` every catalog
//! lookup of every table: whatever a reclaim does under either, every publisher of the table (under `CATALOG`, every
//! reader too) waits out. The 8-client probe measured a reclaim's publish-lock hold at 150-400 ms at 1k-5k parts:
//! 64-65 % the retirement (its part-registry walk and the drops of the retired generations, under both locks),
//! 10-17 % the unlinks, ~1 % the flushes.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

use pg_clock::MonoStamp;

/// The counters, in this order (`reclaim=`): reclaims that planned something (one with nothing to do returns before
/// that); of them, incremental ones ([`crate::ingest::reclaim_below`]'s path: no dead part, no rolled-back
/// generation's file, no sweep due, a logged sink, the relation locked against TRUNCATE and DROP); the publish lock's
/// hold of those reclaims, total and max (us); the retirements' `CATALOG` hold, total and max (us); the names an
/// incremental reclaim unlinked with its publish lock held; the retirements that walked the part registry or let go
/// of the retired generations with `CATALOG` or a reclaim's publish lock held; the reclaims whose plan made without the
/// publish lock (the hint) stood under it; the hints the check under the lock discarded (the reclaim planned again
/// under the lock).
pub const FIELDS: [&str; 10] = ["n", "inc", "lock_us", "lock_max_us", "catalog_us", "catalog_max_us", "inc_locked_unlinks", "locked_retire", "hinted", "replans"];
pub const N: usize = 0;
pub const INC: usize = 1;
pub const LOCK_US: usize = 2;
pub const LOCK_MAX_US: usize = 3;
pub const CATALOG_US: usize = 4;
pub const CATALOG_MAX_US: usize = 5;
pub const INC_LOCKED_UNLINKS: usize = 6;
pub const LOCKED_RETIRE: usize = 7;
pub const HINTED: usize = 8;
pub const REPLANS: usize = 9;
/// The last holds kept for `reclaim_holds=` (the probe's p50).
const RECENT: usize = 32;

pgsync::process_global! {
    static COUNTS: [AtomicU64; 10] = [const { AtomicU64::new(0) }; 10];
    static HOLDS: [AtomicU64; RECENT] = [const { AtomicU64::new(0) }; RECENT];
    static NEXT: AtomicUsize = AtomicUsize::new(0);
}

fn add(i: usize, v: u64) {
    COUNTS[i].fetch_add(v, Relaxed);
}

fn time(total: usize, max: usize, ns: u64) {
    let us = ns / 1000;
    add(total, us);
    COUNTS[max].fetch_max(us, Relaxed);
}

/// The counters, in [`FIELDS`] order.
pub fn get() -> [u64; 10] {
    std::array::from_fn(|i| COUNTS[i].load(Relaxed))
}

/// The publish-lock holds of the last reclaims that planned something (us), oldest first.
pub fn recent() -> Vec<u64> {
    let next = NEXT.load(Relaxed);
    (next.saturating_sub(RECENT)..next).map(|i| HOLDS[i % RECENT].load(Relaxed)).collect()
}

/// `reclaim=<n>,<inc>,... reclaim_holds=<us>,<us>,...` ([`FIELDS`], [`recent`]).
pub fn line() -> String {
    let join = |v: &[u64]| {
        v.iter().enumerate().fold(String::new(), |mut s, (i, x)| {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&x.to_string());
            s
        })
    };
    format!("reclaim={} reclaim_holds={}", join(&get()), join(&recent()))
}

/// A reclaim's hint stood under the publish lock (`true`), or was discarded there and the plan made again (`false`).
pub(crate) fn hint(stood: bool) {
    add(if stood { HINTED } else { REPLANS }, 1);
}

/// A retirement's `CATALOG` hold, `ns`.
pub(crate) fn catalog_held(ns: u64) {
    time(CATALOG_US, CATALOG_MAX_US, ns);
}

/// A reclaim's hold of its table's publish lock, timed from the grant to [`Hold::release`] (or the drop).
pub struct Hold<'a> {
    guard: Option<pgsync::MutexGuard<'a, ()>>,
    granted: MonoStamp,
    planned: bool,
}

impl<'a> Hold<'a> {
    pub fn take(lock: &'a pgsync::Mutex<()>) -> Hold<'a> {
        let guard = pgsync::lock(lock);
        Hold { guard: Some(guard), granted: MonoStamp::now(), planned: false }
    }

    /// The reclaim planned something to do; `inc`: on the incremental path.
    pub fn planned(&mut self, inc: bool) {
        self.planned = true;
        add(N, 1);
        add(INC, u64::from(inc));
    }

    pub fn held(&self) -> bool {
        self.guard.is_some()
    }

    /// `names` unlinks run now; `inc`: an incremental reclaim's.
    pub fn unlinking(&self, inc: bool, names: usize) {
        if inc && self.held() {
            add(INC_LOCKED_UNLINKS, names as u64);
        }
    }

    /// A retirement's registry walk and the drops of its generations run now.
    pub fn retiring(&self) {
        if self.held() {
            add(LOCKED_RETIRE, 1);
        }
    }

    /// The lock goes (once).
    pub fn release(&mut self) {
        let Some(guard) = self.guard.take() else { return };
        let ns = self.granted.elapsed_ns();
        drop(guard);
        if self.planned {
            time(LOCK_US, LOCK_MAX_US, ns);
            let i = NEXT.fetch_add(1, Relaxed);
            HOLDS[i % RECENT].store(ns / 1000, Relaxed);
        }
    }
}

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (`LOCKED_RETIRE` stays untouched here: the catalog's test reads it, and the tests run in parallel.)
    #[test]
    fn a_hold_is_timed_once_and_counts_only_what_runs_while_it_is_held() {
        let lock = pgsync::Mutex::new(());
        let before = get();
        let mut h = Hold::take(&lock);
        h.planned(true);
        h.unlinking(true, 3);
        h.unlinking(false, 5);
        assert!(h.held());
        h.release();
        assert!(!h.held(), "released");
        h.unlinking(true, 7);
        h.retiring();
        h.release();
        drop(h);
        let after = get();
        // other tests' reclaims may count too (the counters are process-wide): at least this one's
        assert!(after[N] > before[N] && after[INC] > before[INC]);
        assert!(after[INC_LOCKED_UNLINKS] - before[INC_LOCKED_UNLINKS] >= 3);
        assert!(!recent().is_empty());
        assert!(line().starts_with("reclaim=") && line().contains(" reclaim_holds="));
    }
}
