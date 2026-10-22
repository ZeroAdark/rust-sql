//! `SELECT k, sum(v), count(*) FROM t GROUP BY k [ORDER BY k] [the zone top-k]` (§2 decisions 6–8,

use gravity_abi::ctx::hdr;

use gravity_abi::breaker::{Breaker, BreakerKind, Budget, KeyKind, RecordLayout, SlotKind, Spill, SELECT_MAX, T1_MAX_PARTS};

use crate::{Touch, SITE_EXEC_CELLS};

/// `(k int4) → sum(v) int8, count`: 32 B per group (the validity word: `sum` is NULL under all-NULL values).
pub const LAYOUT: RecordLayout = RecordLayout::new(&[KeyKind::I32], &[SlotKind::SumI64, SlotKind::Count]);

pub const INIT: u32 = 256;

pub const TOUCH: Touch = Touch { hdr: &[hdr::RESUME], sites: &[(0, SITE_EXEC_CELLS)] };

/// The kernel words the runner derives (`breakers.md`): the top-k bound (0: none), the dense
pub const PARAM_TOPK_N: usize = 0;

pub const PARAM_DENSE_LO: usize = 1;

pub const PARAM_DENSE_HI: usize = 2;

pub const PARAM_SPILL_BITS: usize = 3;

pub const PARAM_ORDER: usize = 4;

pub const NPARAMS: usize = 5;

pub const DEFAULT_PARAMS: [u64; NPARAMS] = [0, 1, 0, 0, 0];

pub const DEFAULT_BITS: u8 = gravity_abi::breaker::PARTITION_DEFAULT_BITS;

/// The largest LIMIT the zone top-k serves (`T1::nth_smallest_key`'s select).
pub const MAX_N: u32 = SELECT_MAX as u32;

pub const MAX_RUNS: usize = T1_MAX_PARTS as usize;

/// The partition bits of `groups` records of `record` bytes: the one rule (DR-6, `gravity_abi::breaker::partition_bits`)
/// over the reference slice.
pub const fn spill_bits(groups: u64, record: u32) -> u8 {
    gravity_abi::breaker::partition_bits(groups, record as u64, gravity_abi::breaker::PARTITION_SLICE)
}

pub const fn breaker(groups: u64) -> Breaker {
    Breaker { kind: BreakerKind::HashAgg, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits: spill_bits(groups, LAYOUT.size()) } }
}

#[cfg(feature = "hand-kernels")]
#[path = "../par_hashagg/hand.rs"]
mod hand;
#[cfg(feature = "hand-kernels")]
pub use hand::*;
