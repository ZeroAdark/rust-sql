//! Positional SMA (spec §8.2): per armed granule, a 256-entry
//! leading-byte → candidate row range table (1 KiB block) that narrows
//! scans INSIDE granules that cannot be skipped (charter §5 — "shipped
//! nowhere mainstream, a deliberate differentiator").
//!
//! ## The index function (the frozen "leading-byte" reading, pinned here)
//!
//! For a granule with zone keys `[min, max]` (either kind — a value-eq
//! implies key-eq for coarse too, so PSMA candidates are sound for both):
//!
//! - `span = max − min` as u64 (keys are i64, min ≤ max);
//! - `shift = 8 × (span_bytes − 1)` where `span_bytes` = bytes needed to
//!   represent `span` (0 → shift 0);
//! - `idx(key) = (key − min) >> shift` — the LEADING BYTE of the key's
//!   granule-delta at the granule's delta width; always ≤ 255 because
//!   `delta ≤ span`.
//!
//! Soundness needs only determinism (the entries partition rows by
//! `idx`); effectiveness comes from the delta compaction. Probe uses the
//! SAME function with the granule's stats-record min/max — no extra stored
//! state beyond the block.
//!
//! ## Section body (spec §8.2, frozen)
//!
//! `[armed bitmap: ceil(granule_count/8) B][1 KiB blocks for armed
//! granules, in granule order]`; block = 256 × `{min_row: u16, max_row:
//! u16}` LE, `max_row` exclusive. An empty entry encodes
//! `{0xFFFF, 0}` (min > max ⇒ no candidates).

use crate::format::geom::GRANULE_ROWS;
use crate::format::meta::{PSMA_BLOCK_LEN, PSMA_ENTRIES};

/// The shift for a granule's key span (see module doc).
pub fn psma_shift(min_key: i64, max_key: i64) -> u32 {
    debug_assert!(min_key <= max_key);
    let span = (max_key as u64).wrapping_sub(min_key as u64);
    let bits = 64 - span.leading_zeros();
    let bytes = bits.div_ceil(8);
    8 * bytes.saturating_sub(1)
}

/// The table index of one key (see module doc). Caller guarantees
/// `min_key ≤ key ≤ max_key` (probe clamps first).
#[inline]
pub fn psma_index(min_key: i64, shift: u32, key: i64) -> u8 {
    let delta = (key as u64).wrapping_sub(min_key as u64);
    (delta >> shift) as u8
}

/// One in-build PSMA table (builder-side accumulator).
/// A granule's derived keys in row order with their granule-relative rows. While every key so far sits at the row
/// equal to its position (a granule with no NULL and no poisoned value) the rows are implicit.
#[derive(Debug, Clone)]
pub struct KeyBuf {
    keys: Vec<i64>,
    rows: Vec<u16>,
    dense: bool,
}

impl Default for KeyBuf {
    fn default() -> Self {
        KeyBuf { keys: Vec::new(), rows: Vec::new(), dense: true }
    }
}

impl KeyBuf {
    /// The staged keys' and rows' heap bytes (capacities): what the meta builder's charge follows (`kept.rs`).
    pub fn heap_bytes(&self) -> u64 {
        crate::kept::cap_bytes!(self.keys) + crate::kept::cap_bytes!(self.rows)
    }

    /// Append keys at ascending granule-relative rows (`rows[i] < GRANULE_ROWS`, above every row already held).
    pub fn extend(&mut self, rows: &[u32], keys: &[i64]) {
        debug_assert_eq!(rows.len(), keys.len());
        debug_assert!(rows.windows(2).all(|w| w[0] < w[1]) && rows.last().is_none_or(|&r| r < GRANULE_ROWS));
        if self.dense {
            let at = self.keys.len() as u32;
            match (rows.first(), rows.last()) {
                (None, _) => return,
                (Some(&a), Some(&b)) if a == at && b == at + rows.len() as u32 - 1 => {
                    self.keys.extend_from_slice(keys); // alloc: Exec charged kept
                    return;
                }
                _ => {
                    self.dense = false;
                    self.rows.extend((0..self.keys.len()).map(|i| i as u16)); // alloc: Exec charged kept
                }
            }
        }
        self.keys.extend_from_slice(keys); // alloc: Exec charged kept
        self.rows.extend(rows.iter().map(|&r| r as u16)); // alloc: Exec charged kept
    }

    pub fn push(&mut self, row: u32, key: i64) {
        self.extend(&[row], &[key]);
    }

    pub fn clear(&mut self) {
        self.keys.clear();
        self.rows.clear();
        self.dense = true;
    }

    pub fn capacity(&self) -> usize {
        self.keys.capacity()
    }
}

#[derive(Debug, Clone)]
pub struct PsmaAcc {
    /// (min_row, max_row_exclusive) per entry; empty = (0xFFFF, 0).
    entries: Box<[(u16, u16); PSMA_ENTRIES]>,
}

impl Default for PsmaAcc {
    fn default() -> Self {
        PsmaAcc {
            entries: Box::new([(u16::MAX, 0); PSMA_ENTRIES]),
        }
    }
}

impl PsmaAcc {
    /// [`PsmaAcc::observe`] over every `(row, key)` of a granule's [`KeyBuf`] (rows ascending): one pass; an entry's
    /// first row is the row of its first touch (its max is still 0 then) and its last row the last store.
    pub fn from_keybuf(min_key: i64, shift: u32, kb: &KeyBuf) -> PsmaAcc {
        let mut acc = PsmaAcc::default();
        let e = &mut acc.entries;
        let row = |i: usize| if kb.dense { i as u16 } else { kb.rows[i] };
        for (i, &key) in kb.keys.iter().enumerate() {
            let r = row(i);
            let t = &mut e[psma_index(min_key, shift, key) as usize];
            if t.1 == 0 {
                t.0 = r;
            }
            t.1 = r + 1;
        }
        acc
    }

    /// Record row `row` (granule-relative ordinal) at table index `idx`.
    pub fn observe(&mut self, idx: u8, row: u32) {
        debug_assert!(row < GRANULE_ROWS);
        let e = &mut self.entries[idx as usize];
        let row16 = row as u16;
        if e.0 > row16 {
            e.0 = row16;
        }
        if e.1 < row16 + 1 {
            e.1 = row16 + 1;
        }
    }

    /// Encode the 1 KiB block (spec §8.2 wire form).
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let start = out.len();
        for &(min_row, max_row) in self.entries.iter() {
            out.extend_from_slice(&min_row.to_le_bytes());
            out.extend_from_slice(&max_row.to_le_bytes());
        }
        debug_assert_eq!(out.len() - start, PSMA_BLOCK_LEN);
    }
}

/// Decode one entry of a 1 KiB block. Returns None on a short block
/// (typed refusal is the reader's; this helper just declines).
fn entry(block: &[u8], idx: u8) -> Option<(u16, u16)> {
    let off = idx as usize * 4;
    if block.len() < off + 4 {
        return None;
    }
    let min_row = u16::from_le_bytes(block[off..off + 2].try_into().expect("len 2"));
    let max_row = u16::from_le_bytes(block[off + 2..off + 4].try_into().expect("len 2"));
    Some((min_row, max_row))
}

/// Equality-probe candidates: the row range (granule-relative, `max`
/// exclusive) that MAY contain rows whose key equals `probe_key`. Empty
/// range = no candidates (advisory: the caller may still scan — pruning is
/// advisory-only by construction). Returns None when the block is
/// malformed (consult nothing).
pub fn psma_candidates_eq(
    block: &[u8],
    min_key: i64,
    max_key: i64,
    probe_key: i64,
) -> Option<(u16, u16)> {
    if block.len() < PSMA_BLOCK_LEN {
        return None;
    }
    if probe_key < min_key || probe_key > max_key {
        return Some((0, 0));
    }
    let shift = psma_shift(min_key, max_key);
    let idx = psma_index(min_key, shift, probe_key);
    let (lo, hi) = entry(block, idx)?;
    if lo >= hi {
        return Some((0, 0));
    }
    Some((lo, hi))
}

/// Locate granule `g`'s 1 KiB block in a `Psma` section body
/// (`[armed bitmap][blocks for armed granules]`, spec §8.2). `Ok(None)` =
/// not armed; typed refusal on a malformed body.
pub fn psma_block_for(
    body: &[u8],
    granule_count: u32,
    g: u32,
) -> crate::format::FormatResult<Option<&[u8]>> {
    use crate::format::FormatError;
    if g >= granule_count {
        return Err(FormatError::Bounds {
            at: "psma granule ordinal",
        });
    }
    let bitmap_len = (granule_count as usize).div_ceil(8);
    if body.len() < bitmap_len {
        return Err(FormatError::Truncated {
            at: "psma armed bitmap",
        });
    }
    let bitmap = &body[..bitmap_len];
    if bitmap[(g / 8) as usize] & (1 << (g % 8)) == 0 {
        return Ok(None);
    }
    let mut rank = 0usize;
    for i in 0..g {
        if bitmap[(i / 8) as usize] & (1 << (i % 8)) != 0 {
            rank += 1;
        }
    }
    let start = bitmap_len + rank * PSMA_BLOCK_LEN;
    let end = start + PSMA_BLOCK_LEN;
    if body.len() < end {
        return Err(FormatError::Truncated { at: "psma block" });
    }
    Ok(Some(&body[start..end]))
}

/// Range-probe candidates for keys in `[lo_key, hi_key]` (inclusive,
/// clamped to the granule's zone): the union of the touched buckets'
/// ranges. Sound because bucket index is monotone in the key.
pub fn psma_candidates_range(
    block: &[u8],
    min_key: i64,
    max_key: i64,
    lo_key: i64,
    hi_key: i64,
) -> Option<(u16, u16)> {
    if block.len() < PSMA_BLOCK_LEN {
        return None;
    }
    let lo_key = lo_key.max(min_key);
    let hi_key = hi_key.min(max_key);
    if lo_key > hi_key {
        return Some((0, 0));
    }
    let shift = psma_shift(min_key, max_key);
    let lo_idx = psma_index(min_key, shift, lo_key);
    let hi_idx = psma_index(min_key, shift, hi_key);
    let mut best: (u16, u16) = (u16::MAX, 0);
    for idx in lo_idx..=hi_idx {
        let (a, b) = entry(block, idx)?;
        if a < b {
            best.0 = best.0.min(a);
            best.1 = best.1.max(b);
        }
    }
    if best.0 >= best.1 {
        return Some((0, 0));
    }
    Some(best)
}
