/// Over it `put` drops the largest Nodes first.
pub const PARK_BYTES: usize = 2 << 30;

pub const REGION_DEPOT_BYTES: usize = 4 << 30;

pub const DEPOT_BYTES: usize = 8 << 30;

/// A set is taken back only under its own key, so the per-key cap stays over the statements of one key in flight.
pub const DEPOT_SETS: usize = 256;
pub const DEPOT_SETS_PER_KEY: usize = 8;

/// Reclaim never drops the newest parked slot set: a hot repeat retakes its own parked set.
pub const NEWEST_KEPT: usize = 1;

/// Each cap is its fixed value's share of the measured pool, or that fixed value while no pool is installed.
pub fn pool_share(capacity: u64, num: u64, den: u64, unset: usize) -> usize {
    match capacity {
        0 => unset,
        c => (c / den * num) as usize,
    }
}

pub fn park_bytes(capacity: u64) -> usize {
    pool_share(capacity, 1, 11, PARK_BYTES)
}

pub fn region_depot_bytes(capacity: u64) -> usize {
    pool_share(capacity, 1, 6, REGION_DEPOT_BYTES)
}

pub fn slot_depot_bytes(capacity: u64) -> usize {
    pool_share(capacity, 1, 3, DEPOT_BYTES)
}

pub fn idle_keep(capacity: usize) -> usize {
    capacity / 4
}

/// decide(End): regions go first (one fault run re-maps a region; a slot set also holds its sources and cursors).
pub fn end_regions_to(keep: usize, regions: usize, slots: usize) -> Option<usize> {
    (regions + slots > keep).then(|| keep.saturating_sub(slots))
}

/// decide(End): the slot sets' bytes to trim once the regions are at their target (`left`: regions and slots then).
pub fn end_slots_over(keep: usize, left: usize) -> usize {
    left.saturating_sub(keep)
}

/// decide(End): a statement's end hands the allocator's retained memory back once it freed `threshold` context bytes
/// past its peak (pgrust.memory_release_threshold; 0 = never).
pub fn end_release(freed: usize, threshold: usize) -> bool {
    threshold != 0 && freed >= threshold
}

/// D4878: what a statement end's hand-back (`end_release`) does with the page pool's free memory. The pool keeps freed
/// memory warm for the next take, so the read stays up until its purger's pass: under ladder pressure (`pressed`) that
/// free memory goes back to the OS too, past the watchdog's cancel line now on the ending thread (`Now`: a canceled
/// statement's bytes are out of the read before the ladder's next sample, as mimalloc's freed huge blocks are at their
/// free, so R6 never takes an innocent statement for them), under it asked of the pool's purger (`Ask`). Unpressed it
/// stays warm (`Keep`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolEnd {
    Keep,
    Ask,
    Now,
}

pub fn pool_end(pressed: bool, past_cancel: bool) -> PoolEnd {
    if past_cancel {
        PoolEnd::Now
    } else if pressed {
        PoolEnd::Ask
    } else {
        PoolEnd::Keep
    }
}

/// decide(Idle): an idle session's one-shot timer, `(release first, delay ms)`; 0 = off. Inside a transaction block
/// (`in_xact`: `idle_in_xact`'s answer) only the release stage: passivation drops caches the transaction may hold.
pub fn idle_stage(release_ms: i32, passivate_ms: i32, in_xact: Option<bool>) -> (bool, i32) {
    if in_xact == Some(false) {
        return (false, 0);
    }
    let in_xact = in_xact.is_some();
    let (release_ms, passivate_ms) = (release_ms.max(0), if in_xact { 0 } else { passivate_ms.max(0) });
    let release_first = release_ms > 0 && (passivate_ms == 0 || release_ms < passivate_ms);
    (release_first, if release_first { release_ms } else { passivate_ms })
}

/// decide(Idle): whether a session idle in a transaction block has anything to release: pool bytes its suspended
/// portals hold (`held`), or a last statement of at least `threshold` bytes. Else no timer: it would cost OLTP blocks.
pub fn idle_in_xact(held: i64, last_peak: u64, threshold: u64) -> bool {
    held > 0 || (threshold != 0 && last_peak >= threshold)
}

/// decide(Idle): after the release stage, the passivation delay still to wait (0 = none).
pub fn idle_after_release(release_ms: i32, passivate_ms: i32, in_xact: bool) -> i32 {
    if in_xact {
        0
    } else {
        passivate_ms.max(0).saturating_sub(release_ms.max(0)).max(0)
    }
}

/// What a suspended portal's reservation lends the pool until its next fetch re-takes it forced: all of its room not
/// drawn and not carved for a hold. A small-lane grant lends nothing.
pub fn suspend_lend(room: u64, small: bool) -> u64 {
    if small {
        0
    } else {
        room
    }
}

pub const IDLE_RELEASE_MS: u64 = 10_000;

/// pppress card 10 (`PGRUST_POOL_IDLE_TAIL`): the wait (ms) between two slices of a session's idle tail cut that stopped
/// at its time budget (pagepool's, about 0.5 ms on the session's thread): the idle timer comes back this much later and
/// the cut goes on from the run it stopped at, so a command that arrives meanwhile finds at most one slice running.
pub const IDLE_TAIL_SLICE_MS: i32 = 10;

#[inline]
pub fn idle_period_ms(reclaiming: bool, tick_ms: u64, idle_ms: u64) -> u64 {
    if reclaiming {
        tick_ms
    } else {
        (idle_ms / 4).clamp(5, 2_500)
    }
}

/// `last` = 0: none yet; `done`: the last take the release already answered.
#[inline]
pub fn idle_due(last: u64, done: u64, now: u64, idle_ms: u64) -> bool {
    last != done && now.saturating_sub(last) >= idle_ms
}

#[inline]
pub fn past_bar(projected: u64, bar: u64) -> bool {
    bar != 0 && projected > bar
}

/// D4554: the kept-pool switch (the `kept_pool` knob, on by default): a scan source's kept decode cursors charge their
/// own tables to one charge of the source's, which it moves once at each owner change, not once per cursor.
#[inline]
pub fn kept_pool() -> bool {
    crate::consts::get(crate::consts::K::KeptPool) != 0
}

/// D4554: the kept-decay bound (the `kept_decay` knob, 0 = off): a scan source closes its kept cursors over a part none of
/// its last this-many binds opened, at its unbind.
#[inline]
pub fn kept_decay() -> u64 {
    crate::consts::get(crate::consts::K::KeptDecay)
}

/// D4554: whether a source's cursors over a part its last `binds_since` binds did not open close at this unbind (the
/// `kept_decay` bound; never while it is 0).
#[inline]
pub fn kept_stale(binds_since: u64) -> bool {
    let d = kept_decay();
    d != 0 && binds_since >= d
}

/// D4554: the keep-warm switch (the `keep_warm` knob, off by default): a worker set parked in the slot depot keeps what its
/// statement's next run needs again, on the depot's holder, instead of freeing it at the park and building it at the bind.
#[inline]
pub fn keep_warm() -> bool {
    crate::consts::get(crate::consts::K::KeepWarm) != 0
}

/// D4554: the most code counts a pgrc part keeps for code.heads' emits (`OpenPart::keep_code_counts`, the `heads_cache`
/// switch): a LIMIT's emitted codes, a few a statement; 12 bytes each, the part cache's.
pub const CODE_COUNTS_KEPT: usize = 256;

/// D4554: the heads-cache switch (the `heads_cache` knob, off by default): a part keeps the code counts code.heads' emit
/// asked of it, and a statement whose asked codes it keeps counts nothing of it.
#[inline]
pub fn heads_cache() -> bool {
    crate::consts::get(crate::consts::K::HeadsCache) != 0
}

/// D4554: the kept-parked switch (the `kept_parked` knob, on by default): a scan source's kept cursors stay parked while a
/// statement reads through them, and its per-statement walks visit only the parts the statement opened.
#[inline]
pub fn kept_parked() -> bool {
    crate::consts::get(crate::consts::K::KeptParked) != 0
}

/// The thread's pool of nested kept executions gives back its oldest first while this says no.
#[inline]
pub fn nested_kept_keeps(held: u64, more: u64) -> bool {
    held.saturating_add(more) <= crate::consts::get(crate::consts::K::NestedKeptBytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_idle_loop_keeps_its_cadence() {
        assert_eq!((idle_period_ms(true, 50, 10_000), idle_period_ms(false, 50, 10_000)), (50, 2_500));
        assert_eq!((idle_period_ms(false, 50, 8), idle_period_ms(false, 50, 400)), (5, 100));
        assert!(idle_due(100, 0, 10_100, 10_000) && !idle_due(100, 0, 10_099, 10_000));
        assert!(!idle_due(100, 100, u64::MAX, 10_000), "answered already");
        assert!(past_bar(11, 10) && !past_bar(10, 10) && !past_bar(u64::MAX, 0));
    }

    #[test]
    fn the_caps_follow_the_pool() {
        let pool = 66u64 << 30;
        assert_eq!((park_bytes(pool), region_depot_bytes(pool), slot_depot_bytes(pool)), (6 << 30, 11 << 30, 22 << 30));
        assert_eq!(park_bytes(0), PARK_BYTES, "no pool installed: the fixed value");
        assert_eq!(idle_keep(16 << 30), 4 << 30);
    }

    #[test]
    fn the_nested_pool_keeps_up_to_its_bound() {
        use crate::consts::{get, set, K};
        use super::nested_kept_keeps;
        let _one = pgsync::lock(&crate::consts::TEST_LOCK);
        crate::consts::reset();
        let cap = get(K::NestedKeptBytes);
        assert!(nested_kept_keeps(0, cap));
        assert!(nested_kept_keeps(cap - 10, 10));
        assert!(!nested_kept_keeps(cap - 10, 11));
        assert!(!nested_kept_keeps(u64::MAX, 1));
        set(K::NestedKeptBytes, 0);
        assert!(!nested_kept_keeps(0, 1));
        crate::consts::reset();
    }

    #[test]
    fn a_suspended_portal_lends_its_room() {
        assert_eq!((suspend_lend(54 << 20, false), suspend_lend(0, false), suspend_lend(1 << 20, true)), (54 << 20, 0, 0));
    }

    #[test]
    fn a_statement_end_trims_regions_first_and_releases_past_the_threshold() {
        assert_eq!((end_regions_to(10, 4, 6), end_regions_to(10, 8, 6), end_regions_to(10, 2, 14)), (None, Some(4), Some(0)));
        assert_eq!((end_slots_over(10, 14), end_slots_over(10, 10)), (4, 0));
        assert!(end_release(64 << 20, 64 << 20) && !end_release(64 << 20, 0) && !end_release((64 << 20) - 1, 64 << 20));
    }

    #[test]
    fn a_statement_end_under_pressure_gives_the_pools_free_memory_back() {
        assert_eq!((pool_end(false, false), pool_end(true, false), pool_end(true, true), pool_end(false, true)), (PoolEnd::Keep, PoolEnd::Ask, PoolEnd::Now, PoolEnd::Now));
    }

    #[test]
    fn an_idle_session_releases_first_and_passivates_only_outside_a_transaction() {
        assert_eq!((idle_stage(1000, 30_000, None), idle_after_release(1000, 30_000, false)), ((true, 1000), 29_000));
        assert_eq!((idle_stage(1000, 0, None), idle_stage(0, 30_000, None), idle_stage(40_000, 30_000, None)), ((true, 1000), (false, 30_000), (false, 30_000)));
        assert_eq!((idle_stage(1000, 30_000, Some(true)), idle_after_release(1000, 30_000, true)), ((true, 1000), 0));
        assert_eq!((idle_stage(0, 30_000, Some(true)), idle_stage(-1, -1, None)), ((false, 0), (false, 0)));
        assert_eq!(idle_stage(1000, 30_000, Some(false)), (false, 0), "a block with nothing to give back arms nothing");
        let mib = 1u64 << 20;
        assert!(idle_in_xact(1, 0, 64 * mib) && idle_in_xact(0, 64 * mib, 64 * mib));
        assert!(!idle_in_xact(0, 64 * mib - 1, 64 * mib) && !idle_in_xact(0, u64::MAX, 0) && !idle_in_xact(-5, 0, 64 * mib));
    }
}

/// D4679: the most one backend thread's growth shelf keeps (mcx aset: a dropped context's blocks after its first, kept for
/// the thread's next context block of the same size; Memory Accounting's cap, 10-16). CHOSEN, not measured: the Speedtest
/// scripts' per-statement planner contexts churn 8 + 16 (+ 32) KiB a statement.
pub const GROWTH_SHELF_BYTES: usize = 64 << 10;

/// D4679: the growth shelf's cap now: `GROWTH_SHELF_BYTES` under `keep_growth` = 1, else 0 (off: a dropped context frees its
/// blocks, today's road).
#[inline]
pub fn growth_shelf_bytes() -> usize {
    if crate::consts::get(crate::consts::K::KeepGrowth) == 0 {
        0
    } else {
        GROWTH_SHELF_BYTES
    }
}
