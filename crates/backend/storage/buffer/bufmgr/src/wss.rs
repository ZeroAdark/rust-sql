//! Working-set size of the shared pool and buffer-read stall time: the
//! engine-side signals for choosing a pool size.
//!
//! Every shared-buffer lookup (hit or miss) passes its tag hash to [`note`].
//! One hash in 16 (a remix of the hash, so the choice is a fixed function of
//! the page) feeds a HyperLogLog of 16384 registers. Each register keeps, per
//! rank, the coarse second it was last seen, so the sketch answers "distinct
//! pages in the last W seconds" for any window: a register's value over W is
//! its highest rank stamped within W. The estimate is scaled back by 16.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

const SAMPLE_BITS: u32 = 4;
const REG_BITS: u32 = 14;
const RANK_BITS: u32 = 32 - SAMPLE_BITS - REG_BITS;
const REGS: usize = 1 << REG_BITS;
const RANKS: usize = RANK_BITS as usize + 1;
const SAMPLE_SCALE: f64 = (1u32 << SAMPLE_BITS) as f64;
const MIX: u32 = 0x9E37_79B1;

static SKETCH: [[AtomicU32; RANKS]; REGS] =
    [const { [const { AtomicU32::new(0) }; RANKS] }; REGS];

// Coarse monotonic seconds + 1 (0 marks a never-stamped slot). Advanced by
// stamping, by every timed buffer read, by the bgwriter's cycle and by each
// estimate, so a pure-hit workload still moves it at bgwriter cadence.
static NOW: AtomicU32 = AtomicU32::new(1);
static STALL_NS: AtomicU64 = AtomicU64::new(0);

/// Hit-path hook: one multiply and a predicted branch for 15 of 16 pages.
#[inline(always)]
pub(crate) fn note(hash: u32) {
    let x = hash.wrapping_mul(MIX);
    if x >> (32 - SAMPLE_BITS) == 0 {
        note_sampled(x);
    }
}

/// Register from the next REG_BITS, rank from the leading zeros of the
/// low RANK_BITS (rank index 0..=RANK_BITS is HLL rho - 1).
#[inline]
fn slot_of(x: u32) -> &'static AtomicU32 {
    let reg = ((x >> RANK_BITS) as usize) & (REGS - 1);
    let w = x & ((1 << RANK_BITS) - 1);
    let rank = (w.leading_zeros() - (32 - RANK_BITS)) as usize;
    &SKETCH[reg][rank]
}

#[cold]
#[inline(never)]
fn note_sampled(x: u32) {
    let slot = slot_of(x);
    let now = NOW.load(Ordering::Relaxed);
    if slot.load(Ordering::Relaxed) != now {
        slot.store(now, Ordering::Relaxed);
        tick();
    }
}

fn secs(ns: u64) -> u32 {
    (ns / 1_000_000_000).min(u32::MAX as u64 - 1) as u32 + 1
}

pub(crate) fn tick() {
    tick_at(pg_clock::mono_ns());
}

pub(crate) fn tick_at(ns: u64) {
    let t = secs(ns);
    if t > NOW.load(Ordering::Relaxed) {
        NOW.fetch_max(t, Ordering::Relaxed);
    }
}

/// One timed shared read (WaitReadBuffers): its wall time is stall time.
pub(crate) fn note_stall(start_ns: u64, end_ns: u64) {
    STALL_NS.fetch_add(end_ns.saturating_sub(start_ns), Ordering::Relaxed);
    tick_at(end_ns);
}

pub(crate) fn stall_seconds() -> f64 {
    STALL_NS.load(Ordering::Relaxed) as f64 / 1e9
}

/// Distinct shared pages looked up in the last `window_secs` seconds.
pub(crate) fn estimate_pages(window_secs: u32) -> f64 {
    tick();
    estimate_at(NOW.load(Ordering::Relaxed), window_secs)
}

fn estimate_at(now: u32, window_secs: u32) -> f64 {
    let cutoff = now.saturating_sub(window_secs);
    let m = REGS as f64;
    let mut sum = 0.0f64;
    let mut zeros = 0u32;
    for reg in SKETCH.iter() {
        let mut value = 0i32;
        for (idx, slot) in reg.iter().enumerate().rev() {
            let s = slot.load(Ordering::Relaxed);
            if s != 0 && s > cutoff {
                value = idx as i32 + 1;
                break;
            }
        }
        if value == 0 {
            zeros += 1;
        }
        sum += (-value as f64).exp2();
    }
    let alpha = 0.7213 / (1.0 + 1.079 / m);
    let mut e = alpha * m * m / sum;
    if e <= 2.5 * m && zeros > 0 {
        e = m * (m / zeros as f64).ln();
    }
    let space = (1u64 << (REG_BITS + RANK_BITS)) as f64;
    if e > space / 30.0 {
        e = -space * (1.0 - e / space).ln();
    }
    e * SAMPLE_SCALE
}

/// Crash-cycle reset: shared memory is re-created in C, so the sketch and
/// the stall total start over.
pub(crate) fn reset() {
    for reg in SKETCH.iter() {
        for slot in reg.iter() {
            slot.store(0, Ordering::Relaxed);
        }
    }
    STALL_NS.store(0, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    pub(crate) fn feed_at(hash: u32, now: u32) {
        let x = hash.wrapping_mul(MIX);
        if x >> (32 - SAMPLE_BITS) == 0 {
            slot_of(x).store(now, Ordering::Relaxed);
        }
    }

    pub(crate) fn sampled(hash: u32) -> bool {
        hash.wrapping_mul(MIX) >> (32 - SAMPLE_BITS) == 0
    }

    pub(crate) fn estimate(now: u32, window_secs: u32) -> f64 {
        estimate_at(now, window_secs)
    }
}
