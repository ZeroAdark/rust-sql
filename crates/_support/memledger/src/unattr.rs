//! The in-server unattributed check. While it is on (`set(true)`: the server at backend start under
//! `PGRUST_MEMLEDGER_UNATTR=1`), a backend's account is checked against its owners as its statement rises and at the
//! statement's end. Unattributed = the account's bytes less what the owners on it hold (the owners view's 'unattributed'
//! row, both doors); its bound is max(16 MiB, 2 % of the account's bytes). The server logs one line when the end or the
//! rises passed the bound (`end_report`).
//!
//! A rise check is triggered by a fold, usually of the allocation that pushed the level past the mark, before its owner's
//! charge commits it (door 1 counts a block at its allocation; `Charge::commit` / `ChargedVec` charge it right after). So a
//! rise check does not judge the bytes of the fold that triggered it (`inflight`, door 1's), and a rise counts only the
//! excess SUSTAINED over two consecutive rise checks (memunattr's rule): a block another thread has between its allocation
//! and its commit at one check is not an excess. The end check (after the statement's teardown) judges everything.
//!
//! The rise checks run on the allocator's fold path (`add`), so nothing here allocates or locks. Off (the default) costs
//! one relaxed load per fold and per statement begin and end.
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering::Relaxed};

use crate::{Acct, ACCOUNT_SLOTS};

static ON: AtomicBool = AtomicBool::new(false);

/// The bound's floor: an account under it is never over.
pub const FLOOR: i64 = 16 << 20;
/// The rise between two checks in one statement: at least this, at least a quarter of the level (about 20 checks for an
/// 8 GB statement).
pub const STEP_MIN: i64 = 64 << 20;
/// Reads of one check: the account's bytes before and after the owners' sum differ by more than half the bound (a writer
/// raced the sum) on every try: the check is skipped, never a false over.
const TRIES: usize = 3;

/// The check on or off, process-wide (the server at backend start; a test).
pub fn set(on: bool) {
    ON.store(on, Relaxed);
}

#[inline]
pub fn on() -> bool {
    ON.load(Relaxed)
}

/// The bound at `total` account bytes: max(16 MiB, 2 %).
pub fn bound(total: i64) -> i64 {
    FLOOR.max(total / 50)
}

struct State {
    /// A statement began while the check was on (`begin`); `end_report` takes it.
    began: AtomicBool,
    /// The statement level (bytes over its start) at which the next rise check runs.
    next: AtomicI64,
    /// The last rise check's excess over its bound (`i64::MIN`: none yet this statement), `biased` (`swap_prev`).
    prev: AtomicU64,
    /// The worst excess sustained over two consecutive rise checks (`i64::MIN`: none), `biased` (`max_worst`, `worst`),
    /// its unattributed bytes (the bound plus it), the bound and the level at the second check.
    worst: AtomicU64,
    worst_unattr: AtomicI64,
    worst_bound: AtomicI64,
    worst_level: AtomicI64,
    checks: AtomicU32,
    skips: AtomicU32,
}

#[allow(clippy::declare_interior_mutable_const)]
const STATE0: State = State {
    began: AtomicBool::new(false),
    next: AtomicI64::new(0),
    prev: AtomicU64::new(biased(i64::MIN)),
    worst: AtomicU64::new(biased(i64::MIN)),
    worst_unattr: AtomicI64::new(0),
    worst_bound: AtomicI64::new(0),
    worst_level: AtomicI64::new(0),
    checks: AtomicU32::new(0),
    skips: AtomicU32::new(0),
};

static STATE: [State; ACCOUNT_SLOTS] = [STATE0; ACCOUNT_SLOTS];

/// `v` with its sign bit flipped, the form `prev` and `worst` keep (D4512): `i64::MIN` ('none') is 0, so `STATE0` is all
/// zero and the 256 KiB `STATE` lands in .bss (no file bytes), and the order is kept (`a < b` signed iff
/// `biased(a) < biased(b)` unsigned), so an unsigned `fetch_max` of the biased words is the signed max.
const fn biased(v: i64) -> u64 {
    (v as u64) ^ (1 << 63)
}

const fn unbiased(raw: u64) -> i64 {
    (raw ^ (1 << 63)) as i64
}

impl State {
    /// `prev` = `v`; the previous `prev`.
    fn swap_prev(&self, v: i64) -> i64 {
        unbiased(self.prev.swap(biased(v), Relaxed))
    }

    /// `worst` = max(`worst`, `v`); the previous `worst`.
    fn max_worst(&self, v: i64) -> i64 {
        unbiased(self.worst.fetch_max(biased(v), Relaxed))
    }

    fn worst(&self) -> i64 {
        unbiased(self.worst.load(Relaxed))
    }

    /// `prev` and `worst` = `i64::MIN` (none).
    fn clear_prev_worst(&self) {
        self.prev.store(biased(i64::MIN), Relaxed);
        self.worst.store(biased(i64::MIN), Relaxed);
    }
}

/// Account `a`'s unattributed bytes now and its bytes: `None` when every try raced a writer. The keeper blocks mcx parked
/// on the account are on it and no owner's, but free context memory kept for reuse, not unattributed (`keepers`).
pub fn measure(a: Acct) -> Option<(i64, i64)> {
    for _ in 0..TRIES {
        let before = crate::account_net(a);
        let held = crate::charge::acct_held(a);
        let after = crate::account_net(a);
        if after.wrapping_sub(before).unsigned_abs() as i64 <= bound(before.max(after)) / 2 {
            return Some((after.wrapping_sub(held).wrapping_sub(crate::keepers(a)), after));
        }
    }
    None
}

/// Backend account `a`'s statement begins (`statement_begin`, while on): its checks start over.
pub(crate) fn begin(a: Acct) {
    let st = &STATE[a.0 as usize];
    st.next.store(STEP_MIN, Relaxed);
    st.clear_prev_worst();
    st.checks.store(0, Relaxed);
    st.skips.store(0, Relaxed);
    st.began.store(true, Relaxed);
}

/// Backend account `a`'s statement is at `level` bytes over its start (`add`, while on), `inflight` of them door 1's bytes
/// of the fold that got it there: a check when it passed the next mark. One thread takes it (the mark moves first); the
/// others go on.
#[inline]
pub(crate) fn rise(a: Acct, level: i64, inflight: i64) {
    let st = &STATE[a.0 as usize];
    let next = st.next.load(Relaxed);
    if level < next || !st.began.load(Relaxed) {
        return;
    }
    let to = level.saturating_add(STEP_MIN.max(level / 4));
    if st.next.compare_exchange(next, to, Relaxed, Relaxed).is_ok() {
        check_rise(a, st, level, inflight.max(0));
    }
}

#[cold]
#[inline(never)]
fn check_rise(a: Acct, st: &State, level: i64, inflight: i64) {
    st.checks.fetch_add(1, Relaxed);
    let Some((u, total)) = measure(a) else {
        st.skips.fetch_add(1, Relaxed);
        return;
    };
    let b = bound(total);
    let ex = u.saturating_sub(inflight).saturating_sub(b);
    let sustained = st.swap_prev(ex).min(ex);
    if sustained > i64::MIN && st.max_worst(sustained) < sustained {
        st.worst_unattr.store(b.saturating_add(sustained), Relaxed);
        st.worst_bound.store(b, Relaxed);
        st.worst_level.store(level, Relaxed);
    }
}

/// One statement's check, when its end or a rise passed the bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    /// The account's unattributed bytes at the end and the bound then (`None`: the end check raced a writer every try).
    pub end: Option<(i64, i64)>,
    /// The worst rise excess sustained over two consecutive rise checks: the unattributed bytes, the bound then and the
    /// statement's level at the second check (`None`: no sustained rise passed its bound).
    pub peak: Option<(i64, i64, i64)>,
    pub checks: u32,
    pub skips: u32,
}

/// The calling backend's statement ended (`statement_end` ran): its end check, and the report when the end or a rise
/// passed its bound. `None` while off, or when no statement began since the last call.
pub fn end_report() -> Option<Report> {
    if !on() {
        return None;
    }
    crate::fold();
    let a = crate::current();
    if !a.is_backend() {
        return None;
    }
    let st = &STATE[a.0 as usize];
    if !st.began.swap(false, Relaxed) {
        return None;
    }
    let end = measure(a).map(|(u, total)| (u, bound(total)));
    let (checks, skips) = (st.checks.load(Relaxed) + 1, st.skips.load(Relaxed) + end.is_none() as u32);
    let peak = (st.worst() > 0).then(|| (st.worst_unattr.load(Relaxed), st.worst_bound.load(Relaxed), st.worst_level.load(Relaxed)));
    (end.is_some_and(|(u, b)| u > b) || peak.is_some()).then_some(Report { end, peak, checks, skips })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D4512: `STATE` lies in .bss (an anonymous writable mapping: no file bytes). Born red on trunk: the `i64::MIN` start of
    /// each slot's `prev` and `worst` kept the 256 KiB table in .data.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_state_table_is_in_bss() {
        let last = core::ptr::addr_of!(STATE[ACCOUNT_SLOTS - 1]) as usize + core::mem::size_of::<State>() - 1;
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        let line = maps
            .lines()
            .find(|l| {
                let (a, z) = l.split(' ').next().unwrap().split_once('-').unwrap();
                (usize::from_str_radix(a, 16).unwrap()..usize::from_str_radix(z, 16).unwrap()).contains(&last)
            })
            .unwrap_or_else(|| panic!("no mapping holds {last:#x}"));
        let f: Vec<&str> = line.split_whitespace().collect();
        assert!(f[1].starts_with("rw") && (f.len() == 5 || f[5] == "[heap]"), "the unattributed check's table is mapped from the binary (.data), not .bss: {line}");
    }

    /// D4512: `prev` and `worst` round-trip every value through the biased words: a fresh slot and `clear_prev_worst` read
    /// `i64::MIN` (none yet), `swap_prev` returns the previous value, `max_worst` is the signed max (negatives below 0,
    /// `i64::MIN` below all) and returns the previous worst.
    #[test]
    fn prev_and_worst_round_trip_with_min_as_none() {
        let st = STATE0;
        assert_eq!(st.worst(), i64::MIN);
        assert_eq!(st.swap_prev(-5), i64::MIN);
        assert_eq!(st.swap_prev(i64::MAX), -5);
        assert_eq!(st.swap_prev(0), i64::MAX);
        assert_eq!(st.swap_prev(i64::MIN), 0);
        assert_eq!(st.max_worst(-10), i64::MIN);
        assert_eq!(st.max_worst(-20), -10);
        assert_eq!(st.worst(), -10);
        assert_eq!(st.max_worst(0), -10);
        assert_eq!(st.max_worst(7 << 40), 0);
        assert_eq!(st.max_worst(3), 7 << 40);
        assert_eq!(st.worst(), 7 << 40);
        assert_eq!(st.max_worst(i64::MAX), 7 << 40);
        assert_eq!(st.worst(), i64::MAX);
        st.clear_prev_worst();
        assert_eq!((st.swap_prev(1), st.worst()), (i64::MIN, i64::MIN));
        for v in [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX] {
            assert_eq!(unbiased(biased(v)), v);
        }
        assert_eq!(biased(i64::MIN), 0);
    }
}
