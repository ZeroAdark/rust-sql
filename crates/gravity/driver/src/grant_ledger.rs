//! The R4 ledger's report-only probes (docs/gravity/lowerv2/refactor-ledger.md §4.5, step S0): grant and slot-life invariants checked off the row path.
use core::sync::atomic::{AtomicU64, Ordering};

/// `PGRUST_GRAVITY_LEDGER`: `report` (the default since S7c) logs each violation, `abort` panics (rigs), `off` turns them off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    Report,
    Abort,
}

/// L1 a draw past the statement's reservation (or with no statement), L3 a slot produced again while live (refused by name), L8 a
/// stale per-open word (an error in debug builds, reset in release), L2 the live shares past the grant and the reservation (a debug assert).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    L1 = 0,
    L3 = 1,
    L8 = 2,
    L2 = 3,
}

const CHECKS: usize = 4;
static COUNTS: [AtomicU64; CHECKS] = [const { AtomicU64::new(0) }; CHECKS];
static LAST: [AtomicU64; CHECKS] = [const { AtomicU64::new(u64::MAX) }; CHECKS];
pub(crate) static EPOCH: AtomicU64 = AtomicU64::new(0);

pub fn mode() -> Mode {
    static MODE: pgsync::OnceLock<Mode> = pgsync::OnceLock::new();
    *MODE.get_or_init(|| match crate::env::text("PGRUST_GRAVITY_LEDGER").as_deref() {
        Some("abort") => Mode::Abort,
        Some("off") => Mode::Off,
        _ => Mode::Report,
    })
}

#[inline]
pub fn on() -> bool {
    mode() != Mode::Off
}

/// Violations per check since the process started (L1, L3, L8, L2).
pub fn counts() -> [u64; CHECKS] {
    core::array::from_fn(|i| COUNTS[i].load(Ordering::Relaxed))
}

pub(crate) fn next_id() -> u64 {
    EPOCH.fetch_add(1, Ordering::Relaxed) + 1
}

#[cold]
pub(crate) fn violation(check: Check, stmt: u64, what: core::fmt::Arguments<'_>) {
    let i = check as usize;
    COUNTS[i].fetch_add(1, Ordering::Relaxed);
    let key = if stmt == 0 { EPOCH.load(Ordering::Relaxed) } else { stmt };
    if LAST[i].swap(key, Ordering::Relaxed) != key {
        eprintln!("gravity: ledger: {check:?} stmt={stmt} thread={:?} {what}", std::thread::current().id());
    }
    if mode() == Mode::Abort {
        panic!("gravity: ledger: {check:?} stmt={stmt} {what}");
    }
}

/// L2's bound for `n` live share-budgeted slots: the grant (at least `SHARE_FLOOR` a share) plus a page and a floor a share
/// (D2587: a share is at least its kind's PostgreSQL budget), or the statement's `reserved` bytes when more (D3863: a learned
/// start raises a share past a grant with no floors, `grant::share_of`, and admission reserved the raise).
pub(crate) fn l2_bound(g: crate::mem::grant::Grant, n: u64, reserved: u64) -> u64 {
    let floor = crate::mem::grant::SHARE_FLOOR as u64;
    (g.bytes.max(n * floor) + n * (4096 + g.sort_floor.max(g.hash_floor))).max(reserved)
}

#[cfg(test)]
mod d3863 {
    use super::l2_bound;
    use crate::mem::grant::Grant;

    const KIB: u64 = 1 << 10;
    const MIB: u64 = 1 << 20;

    #[test]
    fn learned_shares_inside_the_reservation_are_no_overdraw() {
        let g = Grant::of(128 * KIB);
        assert_eq!(l2_bound(g, 2, 0), 128 * KIB + 2 * 4096, "no reservation: the grant, two 64 KiB floors, a page each");
        assert!(2_027_520 <= l2_bound(g, 2, 10_428 * KIB), "m1_d1887_row_collation W0 (work_mem 64 kB): two raised shares inside the 10428 KiB reservation");
        assert!(10_428 * KIB + 1 > l2_bound(g, 2, 10_428 * KIB), "past the grant and the reservation: still an overdraw");
        let f = Grant::of(128 * KIB).with_floors(4 * MIB, 8 * MIB);
        assert_eq!(l2_bound(f, 2, 0), 128 * KIB + 2 * (4096 + 8 * MIB), "D2587: a floor a share past the grant");
    }
}
