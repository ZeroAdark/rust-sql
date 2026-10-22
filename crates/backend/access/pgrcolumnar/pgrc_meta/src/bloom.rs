//! Per-granule equality blooms (spec §8.3; charter §5). Build-side insert
//! and probe-side membership share one bit-selection function over the
//! crate hash family ([`crate::hash`]) — one definition, both sides.
//!
//! ## Arming policy (unclustered; block size by NDV)
//!
//! A granule's bloom is KEPT (armed) iff:
//! - the profile says equality blooms are sound (`eq_bloomable` — the
//!   byte-eq == value-eq lattice, `profile.rs`), AND
//! - the granule is UNCLUSTERED (`Sortedness::Unknown`): a
//!   sorted/constant granule prunes by zone keys already, AND
//! - the granule holds at least two distinct values.
//!
//! ## Sizing (P5-12: the rate from cost)
//!
//! A column's `k` comes from its value width ([`bloom_k_for`]:
//! `cost_profile::bloom`, the rate that prices the section's bytes against
//! a wasted granule read). Every granule builds at the block of a whole
//! granule of distinct values and folds, at its seal, to its own NDV's
//! block ([`bloom_bytes_for`]); the part writes one block size, its
//! largest, and tiles every smaller block up to it ([`bloom_tile`]).
//!
//! Policy constants are meta-builder policy (this crate's), NOT frozen
//! format vocabulary: the section header is self-describing (`k`,
//! `bytes_per_granule`), so probe never assumes the policy.
//!
//! ## Section body (spec §8.3, frozen)
//!
//! `BloomHdr { k: u32, bytes_per_granule: u32 }` (8 B LE) +
//! `[armed bitmap: ceil(granule_count/8) B]` + one
//! `bytes_per_granule` filter block per ARMED granule, in granule order.

use crate::format::meta::Sortedness;

// The pure bloom core (constants, insert, probe — one bit-selection
// definition serving build and probe) lives in the shared `order_key`
// support crate, extracted verbatim from this file per the 2026-08-08
// ruling (the M3-B vendoring charter fulfilled). Re-exported so this
// crate's API and every call site are unchanged.
pub use order_key::bloom::{
    bloom_bytes_for, bloom_fold, bloom_insert, bloom_insert_hashed, bloom_may_contain,
    bloom_may_contain_packed, bloom_pack, bloom_tile, BLOOM_BYTES_MIN,
};
use order_key::bloom::bloom_false_positive_at;

use crate::format::class::StorageClass;
use crate::format::geom::GRANULE_ROWS;

/// The bytes a value of `class` occupies for the bloom's rate: a word's width, a fixed image's length, and a
/// varlena's 4-byte header (the least such a value occupies: a varlena column's bloom spends no more bytes than its
/// narrowest values pay for).
pub fn bloom_value_bytes(class: StorageClass) -> u64 {
    match class {
        StorageClass::ByvalWord { width, .. } => width as u64,
        StorageClass::F32 => 4,
        StorageClass::F64 => 8,
        StorageClass::Bool => 1,
        StorageClass::Fixed { len } => len as u64,
        StorageClass::VarlenaVerbatim => 4,
    }
}

/// P5-12: the hash functions a column of `class` builds its blooms with: `cost_profile::bloom::bloom_k` over
/// [`GRANULE_ROWS`]-row granules of [`bloom_value_bytes`] in the warm device regime (the module doc there).
pub fn bloom_k_for(class: StorageClass) -> u32 {
    cost_profile::bloom::bloom_k(GRANULE_ROWS as u64, bloom_value_bytes(class), &cost_profile::device::Device::WARM)
}

/// The block a granule builds into before its NDV is known: a whole granule of distinct values under `k`.
pub fn bloom_build_bytes(k: u32) -> u32 {
    bloom_bytes_for(GRANULE_ROWS, k)
}

/// D3070: one sealed granule's bloom candidate for the part-level choice ([`bloom_part_choice`]).
#[derive(Clone, Copy, Debug)]
pub struct BloomCandidate {
    /// The distinct values the granule's block holds (its NDV estimate, at most its nonnull count).
    pub ndv: u32,
    /// The granule's own-NDV block ([`bloom_bytes_for`]): a larger part block holds it tiled (no rate gain), a smaller
    /// one folds it.
    pub own: u32,
    /// The bytes a probe without the bloom reads for this granule: its lane as stored (after the codec), else raw.
    pub lane_bytes: u64,
}

/// D3070: the part's bloom block size and which candidates keep a bloom, under `k` hash functions. `None` writes no
/// Bloom section.
///
/// The writer stores ONE block size per part (`bytes_per_granule`) and tiles every smaller armed block up to it, so a
/// granule's stored bloom bytes are the part's block, not its own: the choice is the part's. For each power-of-two
/// block `B` from [`BLOOM_BYTES_MIN`] to the largest own block, each granule pays the lesser of `W` (no bloom: a probe
/// reads its lane) and `B * seq_cold + fp(ndv, k, min(own, B)) * W`; the part takes the `B` of least total, and arms
/// exactly the granules whose bloom was the lesser.
///
/// Storage is priced, in two regimes on purpose: the bloom's stored bytes are paid on every read of the part from disk
/// (data size; the column's meta sections are read as one cluster whenever a scan marks the column), so a stored byte
/// costs the device's COLD sequential read; the lane read it saves is paid only on an equality probe of an absent
/// value, priced at its cheapest, WARM (`W = fixed + lane_bytes * random`, `lane_bytes` the lane as stored, the bytes
/// the reader pays). Pricing both in one regime keeps P5-12's full block on every granule (per probe a bloom always
/// pays) and never prices data size at all (D3070: +1.1 GB on the reference load). The constants are
/// `cost_profile::device::Device::{COLD, WARM}`, measured by `tests/device_fit.rs` on the reference box (2026-10-03):
/// a kept block costs at most ~2.3% of its granule's stored lane plus ~383 B (`random_warm / seq_cold`,
/// `fixed_warm / seq_cold`), so a near-unique 8-byte granule keeps none and a granule of few values keeps its block.
pub fn bloom_part_choice(k: u32, cands: &[Option<BloomCandidate>]) -> Option<(u32, Vec<bool>)> {
    use cost_profile::device::Device;
    let (cold, warm) = (Device::COLD, Device::WARM);
    let max_own = cands.iter().flatten().map(|c| c.own).max()?;
    let wasted = |c: &BloomCandidate| warm.fixed_ns + c.lane_bytes as f64 * warm.random_ns_per_byte;
    let with = |c: &BloomCandidate, b: u32| {
        b as f64 * cold.seq_ns_per_byte + bloom_false_positive_at(c.ndv as f64, k, b.min(c.own)) * wasted(c)
    };
    let mut best: Option<(f64, u32)> = None;
    let mut b = BLOOM_BYTES_MIN.min(max_own);
    loop {
        let total: f64 = cands.iter().flatten().map(|c| with(c, b).min(wasted(c))).sum();
        if best.map_or(true, |(t, _)| total < t) {
            best = Some((total, b));
        }
        if b >= max_own {
            break;
        }
        b *= 2;
    }
    let (_, b) = best?;
    let armed: Vec<bool> = cands.iter().map(|c| c.is_some_and(|c| with(&c, b) < wasted(&c))).collect();
    armed.iter().any(|&a| a).then_some((b, armed))
}

/// The arming decision for one sealed granule (see module doc; the
/// `eq_bloomable` leg is the caller's — the builder consults its profile).
pub fn bloom_armed(sortedness: Sortedness, ndv_est: u32) -> bool {
    sortedness == Sortedness::Unknown && ndv_est >= 2
}

// ---------------------------------------------------------------------------
// section-body access (spec §8.3 wire form; probe side)
// ---------------------------------------------------------------------------

use crate::format::meta::BLOOM_HDR_LEN;
use crate::format::{FormatError, FormatResult};

/// Locate granule `g`'s filter block in a `Bloom` section body. Returns
/// `Ok(None)` when the granule is not armed; typed refusal on a malformed
/// body (the reader CRC-validates, this still bounds-checks — the
/// #66/#340 law). Also returns the header's `k`.
pub fn bloom_block_for(
    body: &[u8],
    granule_count: u32,
    g: u32,
) -> FormatResult<Option<(u32, &[u8])>> {
    if g >= granule_count {
        return Err(FormatError::Bounds {
            at: "bloom granule ordinal",
        });
    }
    if body.len() < BLOOM_HDR_LEN {
        return Err(FormatError::Truncated { at: "BloomHdr" });
    }
    let k = u32::from_le_bytes(body[0..4].try_into().expect("len 4"));
    let bpg = u32::from_le_bytes(body[4..8].try_into().expect("len 4")) as usize;
    if k == 0 || bpg == 0 {
        return Err(FormatError::Corrupt { at: "BloomHdr" });
    }
    let bitmap_len = (granule_count as usize).div_ceil(8);
    let bitmap_end = BLOOM_HDR_LEN + bitmap_len;
    if body.len() < bitmap_end {
        return Err(FormatError::Truncated {
            at: "bloom armed bitmap",
        });
    }
    let bitmap = &body[BLOOM_HDR_LEN..bitmap_end];
    if bitmap[(g / 8) as usize] & (1 << (g % 8)) == 0 {
        return Ok(None);
    }
    // Rank: armed granules before g.
    let mut rank = 0usize;
    for i in 0..g {
        if bitmap[(i / 8) as usize] & (1 << (i % 8)) != 0 {
            rank += 1;
        }
    }
    let start = bitmap_end + rank * bpg;
    let end = start + bpg;
    if body.len() < end {
        return Err(FormatError::Truncated { at: "bloom block" });
    }
    Ok(Some((k, &body[start..end])))
}
