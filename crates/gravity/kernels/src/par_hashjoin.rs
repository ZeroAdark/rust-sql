use gravity_abi::ctx::hdr;

use gravity_abi::Ctx;

use gravity_abi::status::ERROR;

use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};

use gravity_abi::program::{SPILL_PARTS, SPILL_REFILL, SPILL_SPLIT, SPILL_TARGET};

use crate::{Touch, SITE_EXEC_CELLS};

pub const INNER: usize = 0;

pub const OUTER: usize = 1;

/// The partitioned join's three slots (`breakers.md`): the build, the kept outer rows, the scratch.
pub const SLOT_BUILD: u8 = 0;

pub const SLOT_OUTER: u8 = 1;

pub const SLOT_PART: u8 = 2;

pub const TOUCH_BUILD: Touch = Touch { hdr: &[hdr::RESUME], sites: &[(INNER, SITE_EXEC_CELLS)] };

pub const TOUCH_PROBE: Touch = Touch { hdr: &[hdr::RESUME], sites: &[(OUTER, SITE_EXEC_CELLS)] };

/// One parameter word: the spill's partition bits (0: `DEFAULT_BITS`), the same for both streams.
pub const PARAM_SPILL_BITS: usize = 0;

pub const NPARAMS: usize = 1;

pub const DEFAULT_PARAMS: [u64; NPARAMS] = [0];

pub const DEFAULT_BITS: u8 = 4;

pub const ENTRY: u32 = gravity_abi::breaker::HASHBUILD_ENTRY as u32;

pub const fn spill_bits(build_rows: u64) -> u8 {
    crate::table::params::spill_bits(build_rows, ENTRY)
}

/// The kernel words of a program built for `build_rows` (the breaker's bits, as the runner derives them).
pub const fn params(build_rows: u64) -> [u64; NPARAMS] {
    [spill_bits(build_rows) as u64]
}

/// The join's breakers: the build (spilling), the outer rows (spilling, the same bits) and the
/// third phase's `Materialize` scratch (never merged; its rows go to the sink).
pub const fn breakers(build_rows: u64) -> [Breaker; 3] {
    breakers_bits(spill_bits(build_rows))
}

pub const fn breakers_bits(bits: u8) -> [Breaker; 3] {
    [
        Breaker { kind: BreakerKind::HashBuild, slot: SLOT_BUILD, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits } },
        Breaker { kind: BreakerKind::Materialize, slot: SLOT_OUTER, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits } },
        Breaker { kind: BreakerKind::Materialize, slot: SLOT_PART, bytes: 0, budget: Budget::Share, spill: Spill::None },
    ]
}

pub(crate) unsafe fn spill(ctx: *mut Ctx, op: u32, arg: i64) -> i64 {
    crate::util::spill(ctx, op, arg)
}

pub(crate) const STACK: usize = 64;

pub(crate) const MEMO: usize = 1024;

/// Outer token `token` (and the sub-tokens a split queues) against the refilled build table;
pub(crate) unsafe fn outer_split(ctx: *mut Ctx, outer_off: i64, t: u32, memo: &mut [(u32, u32, u32); MEMO], ostream: i64) -> core::result::Result<(u32, u32), i64> {
    if let Some(&(_, lo, hi)) = memo.iter().find(|m| m.0 == t) {
        return Ok((lo, hi));
    }
    let before = spill(ctx, SPILL_PARTS, ostream);
    if before < 0 || spill(ctx, SPILL_TARGET, outer_off) < 0 {
        return Err(ERROR);
    }
    let n = spill(ctx, SPILL_REFILL, t as i64 | (ostream << 32));
    if n < 0 {
        return Err(ERROR);
    }
    let mut after = spill(ctx, SPILL_PARTS, ostream);
    if after < 0 {
        return Err(ERROR);
    }
    if !(n == 0 && after > before) {
        after = spill(ctx, SPILL_SPLIT, t as i64 | (ostream << 32));
        if after < 0 {
            return Err(ERROR);
        }
    }
    if let Some(slot) = memo.iter_mut().find(|m| m.0 == u32::MAX) {
        *slot = (t, before as u32, after as u32);
    }
    Ok((before as u32, after as u32))
}

#[cfg(feature = "hand-kernels")]
mod hand;
#[cfg(feature = "hand-kernels")]
pub use hand::*;
