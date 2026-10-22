//! shared_buffers give-back (D4649; `buf_giveback` knob): the ladder asks for bytes above the configured
//! shared_buffers, the background writer drains them in bounded slices (`resize::background_slice`). Nothing here
//! drains inline: a request is a goal and a latch wake.

use core::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::time::Duration;

use init_small::globals;
use types_core::BLCKSZ;
use types_error::PgResult;

use crate::extent;
use crate::resize::{self, GivebackStep, BG_DIRTY, BG_END_NS, BG_GOAL, BG_SLICES, BG_T0_NS, BG_W0, GROW_GOAL};

/// The background writer's longest slice of drain work per cycle.
pub const GIVEBACK_SLICE: Duration = Duration::from_millis(10);

static DRAINER: AtomicI32 = AtomicI32::new(-1);

/// Give-back counters for the ladder (bytes, ms).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GivebackStats {
    pub on: bool,
    pub minimum_bytes: i64,
    pub active_bytes: i64,
    /// The pending drain's target (0: none pending).
    pub goal_bytes: i64,
    pub landing_bytes: i64,
    pub dirty_pending_bytes: i64,
    /// Written by the current (or last) give-back, and its rate over its run so far.
    pub written_bytes: i64,
    pub write_bytes_per_s: i64,
    pub elapsed_ms: i64,
    pub slices: i64,
}

fn bytes(buffers: i32) -> u64 {
    buffers.max(0) as u64 * BLCKSZ as u64
}

/// The configured shared_buffers (whole extents) or the pin floor: nothing under it is ever given back.
fn minimum_buffers() -> i32 {
    extent::boot_buffers().max(extent::pin_floor_buffers())
}

fn base_buffers() -> i32 {
    let active = extent::active_buffers();
    match BG_GOAL.load(Ordering::Acquire) {
        0 => active,
        g => g.min(active),
    }
}

/// Bytes above the configured shared_buffers a request can still schedule (whole extents; 0 with the knob off).
pub fn giveback_reclaimable() -> u64 {
    if !resize::giveback_on() {
        return 0;
    }
    let above = base_buffers() - minimum_buffers();
    bytes(above & !(extent::EXTENT_BUFFERS - 1))
}

/// The ladder's hook: give back up to `bytes` above the configured shared_buffers, in the background. O(1) and never
/// waits: it lowers the drain goal and wakes the background writer. Returns the bytes newly scheduled (whole extents).
pub fn giveback_request(bytes_wanted: u64) -> u64 {
    if !resize::giveback_on() || in_recovery() {
        return 0;
    }
    let want = (bytes_wanted / BLCKSZ as u64).min(i32::MAX as u64) as i32 & !(extent::EXTENT_BUFFERS - 1);
    if want == 0 {
        return 0;
    }
    let min = minimum_buffers();
    loop {
        let cur = BG_GOAL.load(Ordering::Acquire);
        let base = base_buffers();
        let target = (base as i64 - want as i64).max(min as i64) as i32;
        if target >= base {
            return 0;
        }
        let next = if cur == 0 { target } else { target.min(cur) };
        if BG_GOAL.compare_exchange(cur, next, Ordering::AcqRel, Ordering::Acquire).is_err() {
            continue;
        }
        if cur == 0 {
            resize::start_clock();
        }
        wake_drainer();
        return bytes(base - target);
    }
}

fn in_recovery() -> bool {
    transam_xlog_seams::recovery_in_progress::is_installed() && transam_xlog_seams::recovery_in_progress::call()
}

/// buf_grow: the valid pages the pool evicted since start, striped by buffer id so many backends' misses never write one
/// cache line (the ladder's demand for a grow, `evicted_bytes`).
#[repr(align(64))]
struct Stripe(AtomicU64);
static EVICTED: [Stripe; 16] = [const { Stripe(AtomicU64::new(0)) }; 16];

/// buf_grow: buffer `buf_id`'s valid page was evicted (a miss reusing it).
#[inline]
pub(crate) fn note_evicted(buf_id: i32) {
    EVICTED[buf_id as usize & 15].0.fetch_add(1, Ordering::Relaxed);
}

/// buf_grow: the bytes of the valid pages evicted since start (only growing).
pub fn evicted_bytes() -> u64 {
    EVICTED.iter().map(|s| s.0.load(Ordering::Relaxed)).sum::<u64>().saturating_mul(BLCKSZ as u64)
}

/// buf_grow (user 10-18: the buffer pool grows into free memory; with `buf_giveback`, whose rung gives it back).
pub(crate) fn grow_on() -> bool {
    resize::giveback_on() && mempolicy::consts::get(mempolicy::consts::K::BufGrow) != 0
}

/// The ladder's grow hook: grow the pool by up to `bytes_wanted` (whole extents, at most to pgrust.buffer_pool_max), in
/// the background. O(1) and never waits: it raises the grow goal and wakes the background writer, which activates the
/// extents (`resize::apply_grow`). Nothing while a give-back drains (its goal wins) or in recovery. The bytes scheduled.
pub fn grow_request(bytes_wanted: u64) -> u64 {
    if !grow_on() || in_recovery() || BG_GOAL.load(Ordering::Acquire) != 0 {
        return 0;
    }
    let want = (bytes_wanted / BLCKSZ as u64).min(i32::MAX as u64 / 2) as i32 & !(extent::EXTENT_BUFFERS - 1);
    if want == 0 {
        return 0;
    }
    let max = extent::max_buffers();
    loop {
        let cur = GROW_GOAL.load(Ordering::Acquire);
        let base = extent::active_buffers().max(cur);
        let target = (base as i64 + want as i64).min(max as i64) as i32;
        if target <= base {
            return 0;
        }
        if GROW_GOAL.compare_exchange(cur, target, Ordering::AcqRel, Ordering::Acquire).is_err() {
            continue;
        }
        wake_drainer();
        return bytes(target - base);
    }
}

/// Scheduled and not yet released: the ladder counts these as landing, never as room.
pub fn giveback_landing() -> u64 {
    match BG_GOAL.load(Ordering::Acquire) {
        0 => 0,
        g => bytes(extent::active_buffers() - g),
    }
}

/// L6: these hooks as the memory ladder's shared_buffers rung reads them (`mcx::ladder::install_buffers`, from
/// `init_seams`): after R2a, before any cancel, on the ladder thread's passes only; inert with `buf_giveback` off.
pub(crate) static LADDER_BUFFERS: mcx::ladder::Buffers =
    mcx::ladder::Buffers {
        reclaimable: giveback_reclaimable,
        request: giveback_request,
        landing: giveback_landing,
        grow: grow_request,
        evicted: evicted_bytes,
    };

pub fn giveback_stats() -> GivebackStats {
    let goal = BG_GOAL.load(Ordering::Acquire);
    let t0 = BG_T0_NS.load(Ordering::Relaxed);
    let end = match BG_END_NS.load(Ordering::Relaxed) {
        0 => pg_clock::mono_ns(),
        e => e,
    };
    let elapsed_ns = if t0 == 0 { 0 } else { end.saturating_sub(t0) };
    let written = resize::written().saturating_sub(BG_W0.load(Ordering::Relaxed)) * BLCKSZ as u64;
    GivebackStats {
        on: resize::giveback_on(),
        minimum_bytes: bytes(minimum_buffers()) as i64,
        active_bytes: bytes(extent::active_buffers()) as i64,
        goal_bytes: bytes(goal) as i64,
        landing_bytes: giveback_landing() as i64,
        dirty_pending_bytes: bytes(BG_DIRTY.load(Ordering::Relaxed)) as i64,
        written_bytes: written as i64,
        write_bytes_per_s: if elapsed_ns == 0 { 0 } else { (written as u128 * 1_000_000_000 / elapsed_ns as u128) as i64 },
        elapsed_ms: (elapsed_ns / 1_000_000) as i64,
        slices: BG_SLICES.load(Ordering::Relaxed) as i64,
    }
}

/// The background writer's cycle: one slice of a pending give-back. The caller's latch is what a request wakes.
pub fn giveback_step() -> PgResult<GivebackStep> {
    DRAINER.store(globals::MyProcNumber(), Ordering::Relaxed);
    // buf_grow: a pending grow first (a few descriptor ranges and the free list; never a wait)
    resize::apply_grow();
    resize::background_slice(GIVEBACK_SLICE)
}

pub(crate) fn wake_drainer() {
    let p = DRAINER.load(Ordering::Relaxed);
    if p >= 0 {
        latch::SetLatch(types_storage::latch::LatchHandle::proc(p));
    }
}
