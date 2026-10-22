//! The Governator's reconciliation check (`ladder_reconcile`, `mempolicy::reconcile`'s rules): the terms, each stored
//! where it is computed (`put`), and the ladder thread's check of them at each sample (`sample`) into per-row counts the
//! ladder's view prints (kind 'reconcile'). With the knob off a sample reads it once and does nothing else; the sites'
//! stores are one relaxed store each, at the rate of their own computation.

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

use mempolicy::reconcile::{self as rc, Split, Term, NONE, ROWS, TERMS};

/// The terms' last values (`NONE` until stored) and the most each reached since start.
static VALUES: [AtomicU64; TERMS] = [const { AtomicU64::new(NONE) }; TERMS];
static TERM_MAX: [AtomicU64; TERMS] = [const { AtomicU64::new(0) }; TERMS];

/// Per row: the samples that broke it, the most it was broken by (bytes past the tolerance), the value it compared at
/// the last sample, and the value at its worst break.
static HITS: [AtomicU64; ROWS] = [const { AtomicU64::new(0) }; ROWS];
static WORST: [AtomicU64; ROWS] = [const { AtomicU64::new(0) }; ROWS];
static LAST: [AtomicU64; ROWS] = [const { AtomicU64::new(0) }; ROWS];
static AT_WORST: [AtomicU64; ROWS] = [const { AtomicU64::new(0) }; ROWS];

/// The samples checked, the last sample's split and tolerance, and memory.current at the sample before (the growth).
static SAMPLES: AtomicU64 = AtomicU64::new(0);
static SPLIT: [AtomicU64; SPLIT_FIELDS] = [const { AtomicU64::new(0) }; SPLIT_FIELDS];
static TOLERANCE: AtomicU64 = AtomicU64::new(0);
static PREV_CURRENT: AtomicU64 = AtomicU64::new(0);

pub const SPLIT_FIELDS: usize = 8;
pub const SPLIT_NAMES: [&str; SPLIT_FIELDS] = ["current", "anon", "shmem", "file", "inactive_file", "clean_file", "line", "limit"];

#[inline]
pub fn on() -> bool {
    mempolicy::consts::get(mempolicy::consts::K::LadderReconcile) != 0
}

/// A term's value where it is computed.
#[inline]
pub fn put(t: Term, bytes: u64) {
    VALUES[t as usize].store(bytes, Relaxed);
}

/// A term's last value (`NONE` before the first store).
pub fn value(t: Term) -> u64 {
    VALUES[t as usize].load(Relaxed)
}

/// The ladder thread's sample (knob on): the ladder's own terms stored, then every term checked against `s`.
pub fn sample(s: Split) {
    crate::ladder::reconcile_terms();
    let mut v = [NONE; TERMS];
    for (i, a) in VALUES.iter().enumerate() {
        v[i] = a.load(Relaxed);
        if v[i] != NONE {
            TERM_MAX[i].fetch_max(v[i], Relaxed);
        }
    }
    let prev = PREV_CURRENT.swap(s.current, Relaxed);
    let tol = rc::tolerance(s.limit, if prev == 0 { 0 } else { s.current.abs_diff(prev) });
    let o = rc::check(&v, &s, tol);
    for i in 0..ROWS {
        LAST[i].store(o.value[i], Relaxed);
        if o.excess[i] != 0 {
            HITS[i].fetch_add(1, Relaxed);
            if WORST[i].fetch_max(o.excess[i], Relaxed) < o.excess[i] {
                AT_WORST[i].store(o.value[i], Relaxed);
            }
        }
    }
    let fields = [s.current, s.anon, s.shmem, s.file, s.inactive, s.clean, s.line, s.limit];
    for (a, x) in SPLIT.iter().zip(fields) {
        a.store(x, Relaxed);
    }
    TOLERANCE.store(tol, Relaxed);
    SAMPLES.fetch_add(1, Relaxed);
}

/// A row's counts (the view).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RowCounts {
    pub hits: u64,
    pub worst: u64,
    pub last: u64,
    pub at_worst: u64,
}

pub fn row(i: usize) -> RowCounts {
    RowCounts { hits: HITS[i].load(Relaxed), worst: WORST[i].load(Relaxed), last: LAST[i].load(Relaxed), at_worst: AT_WORST[i].load(Relaxed) }
}

/// A term's last value (`NONE` = never stored) and its most since start.
pub fn term(t: Term) -> (u64, u64) {
    (VALUES[t as usize].load(Relaxed), TERM_MAX[t as usize].load(Relaxed))
}

/// The samples checked, the last split (`SPLIT_NAMES`' order) and the last tolerance.
pub fn split() -> (u64, [u64; SPLIT_FIELDS], u64) {
    let mut f = [0u64; SPLIT_FIELDS];
    for (x, a) in f.iter_mut().zip(SPLIT.iter()) {
        *x = a.load(Relaxed);
    }
    (SAMPLES.load(Relaxed), f, TOLERANCE.load(Relaxed))
}

#[cfg(test)]
pub(crate) fn reset() {
    for a in VALUES.iter() {
        a.store(NONE, Relaxed);
    }
    for a in TERM_MAX.iter().chain(HITS.iter()).chain(WORST.iter()).chain(LAST.iter()).chain(AT_WORST.iter()).chain(SPLIT.iter()) {
        a.store(0, Relaxed);
    }
    for a in [&SAMPLES, &TOLERANCE, &PREV_CURRENT] {
        a.store(0, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mempolicy::reconcile::{Check, Row};

    const GIB: u64 = 1 << 30;

    /// A sample stores the ladder's own terms, counts each broken row once per sample with its worst excess, and keeps
    /// the split it checked against.
    #[test]
    fn a_sample_counts_each_broken_row_and_keeps_its_worst_and_the_split() {
        reset();
        let s = Split { current: 12 * GIB, anon: 9 * GIB, shmem: 0, file: 3 * GIB, inactive: GIB, clean: 2 * GIB, line: 14 * GIB, limit: 16 * GIB };
        let tol = rc::tolerance(s.limit, 0);
        put(Term::LandingPool, 2 * GIB);
        put(Term::Governed, 9 * GIB);
        sample(s);
        let pool = row(Row::WrongKind(Term::LandingPool).index());
        assert_eq!((pool.hits, pool.worst, pool.last, pool.at_worst), (1, 2 * GIB - tol, 2 * GIB, 2 * GIB));
        assert_eq!(row(Row::Split(Check::KillRead).index()).hits, 1, "a GiB under the split's governed resident");
        assert_eq!(row(Row::Split(Check::Landing).index()).hits, 0, "2 GiB of landing under an 11 GiB read");
        put(Term::LandingPool, GIB);
        put(Term::Governed, 10 * GIB);
        sample(Split { current: 12 * GIB + 256 * (1 << 20), ..s });
        let pool = row(Row::WrongKind(Term::LandingPool).index());
        assert_eq!((pool.hits, pool.worst, pool.last, pool.at_worst), (2, 2 * GIB - tol, GIB, 2 * GIB), "the worst stays the first");
        assert_eq!(row(Row::Split(Check::KillRead).index()).hits, 1, "back on the split");
        let (n, f, t) = split();
        assert_eq!((n, f[0], f[1], t), (2, 12 * GIB + 256 * (1 << 20), 9 * GIB, tol + 256 * (1 << 20)), "the tolerance takes the sample's growth");
        assert_eq!(term(Term::LandingPool), (GIB, 2 * GIB));
        reset();
    }
}
