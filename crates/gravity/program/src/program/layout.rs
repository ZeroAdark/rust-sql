//! The program's layout and slot plan: `Layout`, `SlotPlan` (split out of `program/mod.rs` by type, contract-runtime landing 3).

use super::*;

/// The by-reference region's size for `nbyref` by-reference output columns over `out_rows`-row blocks: room for the block at 64 bytes a datum, never under two heap pages (every on-page or fetched datum fits an
/// empty region; a row wider than the region itself has the sink regrow it from the grant, D645's `out_grow`).
pub const fn out_region_bytes(nbyref: u32, out_rows: u32) -> u32 {
    let want = nbyref.saturating_mul(out_rows).saturating_mul(64);
    if want < 2 * 8192 { 2 * 8192 } else { want }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Layout {
    pub nsites: u16,
    pub out_natts: u16,
    pub out_rows: u32,
    pub scratch_bytes: u32,
    pub exec_bytes: u32,
}

impl Layout {
    pub const fn ctx_size(&self) -> usize {
        ctx_size(self.nsites as usize)
    }

    pub fn site_off(&self, i: usize) -> usize {
        assert!(i < self.nsites as usize, "site {i} of {}", self.nsites);
        gravity_abi::ctx::site_off(i)
    }
}

/// The program's breaker slots as validated at construction: per slot the bytes a program-constant (`Budget::Fixed`) producer needs (0 for a slot only share-budgeted producers fill) and whether any producer is share-budgeted; per pipeline the slots live while it runs (a bit set) and
/// the peak of that count (`max_live`, the grant's first divisor).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SlotPlan {
    pub bytes: ArenaVec<u32>,
    pub shared: ArenaVec<bool>,
    pub(crate) live: ArenaVec<u64>,
    pub(crate) words: usize,
    pub max_live: u16,
    /// Per slot the most share slots (`shared`) live at once while it is (D150: a breaker's share divisor; `max_live`
    /// over-divides a sequential one; D3034: a fixed slot takes no part of the grant).
    pub peak: ArenaVec<u16>,
}

impl SlotPlan {
    pub(crate) fn new_in(a: ArenaRef, nslots: usize, npipelines: usize) -> SlotPlan {
        let words = crate::laneset::words_for(nslots);
        let mut p = SlotPlan { bytes: ArenaVec::with_capacity_in(nslots, a), shared: ArenaVec::with_capacity_in(nslots, a), live: ArenaVec::with_capacity_in(words * npipelines, a), words, max_live: 0, peak: ArenaVec::with_capacity_in(nslots, a) };
        p.bytes.resize(nslots, 0);
        p.peak.resize(nslots, 0);
        p.shared.resize(nslots, false);
        p.live.resize(words * npipelines, 0);
        p
    }

    /// The slots a program declares: the highest slot produced plus one.
    pub fn nslots(&self) -> usize {
        self.bytes.len()
    }

    /// The live words of pipeline `p` (bit `s` = slot `s`).
    pub fn live_words(&self, p: usize) -> &[u64] {
        &self.live[p * self.words..(p + 1) * self.words]
    }

    /// The first live word of pipeline `p`: slots 0..64 as a mask.
    pub fn live_mask(&self, p: usize) -> u64 {
        self.live_words(p).first().copied().unwrap_or(0)
    }

    /// The slots live while pipeline `p` runs.
    pub fn live_slots(&self, p: usize) -> impl Iterator<Item = usize> + '_ {
        crate::laneset::iter(self.live_words(p))
    }

    pub fn is_live(&self, p: usize, slot: u8) -> bool {
        crate::laneset::has(self.live_words(p), slot as usize)
    }

    /// The grant divisor of pipeline `p`'s breaker: its slot's `peak`, else `max_live`.
    pub fn share_live(&self, produces: Option<u8>) -> u16 {
        produces.map_or(self.max_live, |s| self.peak[s as usize].max(1))
    }

    /// The largest program-constant breaker: a worker's partial region for `Budget::Fixed` producers.
    pub fn fixed_max(&self) -> u32 {
        self.bytes.iter().copied().max().unwrap_or(0)
    }
}
