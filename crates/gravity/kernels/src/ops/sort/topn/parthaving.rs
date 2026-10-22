//! The part-collapsing owner face (lower.md §15.26 need 6): partmin's collapse without the top.
//! `reduce` sums a group's parts, takes each `min(text)` as the byte minimum of the rendered minima, keeps every group at the floor.
use core::mem::size_of;

use gravity_abi::breaker::{OwnerFns, RecordLayout, ORDER_NONE, OWNER_PARAMS, OWNER_RUN_OFF};

use crate::dict::reps::{pack_slot, Reps, REP_WORDS};

use super::raw::parthaving::{owner_end, owner_min, owner_open, owner_reduce};

/// What the collapse takes: the record (`partitioned_by` its group words, one part word after them), the
/// `(MinCode slot, code lane)` of each `min(text)`, the count slot, the partition bits, and the floor: a group
/// whose count slot, summed over its parts, is under it leaves the owner (any lower bound a HAVING puts on a count
/// final: `count(*)`, `count(x)` or a filtered count, by `>`, `>=` or `=`; lowerv4 twopass_having.rs `qual_floor`;
/// 0: every group is kept).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartHavingSpec {
    pub layout: RecordLayout,
    pub mins: Reps,
    pub count_slot: u8,
    pub bits: u8,
    pub floor: i64,
}

pub(super) const P_HEAD: usize = 0;
pub(super) const P_MINS: usize = 1;
pub(super) const P_FLOOR: usize = 2;
pub(super) const P_LAYOUT: usize = 3;
pub(super) const LAYOUT_WORDS: usize = size_of::<RecordLayout>() / 8;
/// The minima's slot words past `P_MINS` (`pack_slot`'s words 1..`REP_WORDS`), after the layout.
pub(super) const P_MINS_MORE: usize = P_LAYOUT + LAYOUT_WORDS;
const _: () = assert!(P_MINS_MORE + REP_WORDS - 1 <= OWNER_PARAMS, "the spec fits the owner's words");
/// The generic owner's runs form: the directory at `OWNERTOP_HDR` (`(off, m)` per run), the records from `OWNER_RUN_OFF`.

/// The owner that collapses a group's part records to one in `reduce` (partmin's sort-and-sum over the folded
/// partition; each declared minimum the byte minimum of the parts' rendered minima, kept as its coded word so
/// `join_resolve` renders it once; the part word zeroed) and keeps every group at the floor in the runs form
/// `join_gather` reads — so the gather's join is a pass over the survivors, not over `groups × parts`.
pub fn owner_fns(spec: &PartHavingSpec) -> OwnerFns {
    assert!(spec.layout.pkeys == 0 || spec.layout.nkeys() == spec.layout.partition_keys() + 1, "the group words, then the part word");
    let mut p = [0u64; OWNER_PARAMS];
    p[P_HEAD] = spec.count_slot as u64 | (spec.bits as u64) << 8;
    let mw = pack_slot(0, &spec.mins);
    p[P_MINS] = mw[0];
    p[P_MINS_MORE..P_MINS_MORE + REP_WORDS - 1].copy_from_slice(&mw[1..]);
    p[P_FLOOR] = spec.floor as u64;
    let words: [u64; LAYOUT_WORDS] = spec.layout.words();
    p[P_LAYOUT..P_LAYOUT + LAYOUT_WORDS].copy_from_slice(&words);
    OwnerFns { params: p, rounds: 1, order_slot: ORDER_NONE, order_slot2: ORDER_NONE, min_bytes: owner_min, open: owner_open, reduce: owner_reduce, end_round: owner_end, reduce2: owner_reduce, fold: Some(crate::table::owner_fold::owner_fold) }
}

/// The kept records' area of an owner region of `bytes` (`owner_open`'s split; the admission's bound, `exec/src/admit.rs`).
pub const fn runs_area(bytes: usize) -> usize {
    (bytes.saturating_sub(OWNER_RUN_OFF) / 2) & !7
}

/// The floor prune's groups at the bound: their words back to back and an open-addressed index (entry `k + 1` = group `k`,
/// 0 empty) kept at most 1/4 full, doubled and rebuilt as the groups arrive; every allocation fallible. The hash takes
/// every group word, so a constant leading key does not cluster. A one-word filter (a bit per group, from the hash's middle
/// bits; the index takes its high bits) answers nearly every group the set does not hold — the compaction asks for every
/// record, and the groups at the bound are few — without a probe of the index.
pub(super) struct GroupSet {
    g: usize,
    filter: u64,
    words: Vec<u64>, // alloc: Exec
    index: Vec<u32>, // alloc: Exec
}

impl GroupSet {
    pub(super) fn new(g: usize) -> GroupSet {
        GroupSet { g, filter: 0, words: Vec::new(), index: Vec::new() } // alloc: Exec
    }

    #[inline(always)]
    fn hash(w: &[u64]) -> u64 {
        gravity_abi::hash::phi_fold(w)
    }

    #[inline(always)]
    fn bit(h: u64) -> u64 {
        1u64 << ((h >> 32) & 63)
    }

    /// `Ok` when group `w` (hash `h`) is in the set, else the empty slot it would take; the index is not empty.
    #[inline(always)]
    fn find(&self, w: &[u64], h: u64) -> Result<(), usize> {
        let (g, mask) = (self.g, self.index.len() - 1);
        let mut i = (h >> (64 - self.index.len().trailing_zeros())) as usize;
        loop {
            match self.index[i] {
                0 => return Err(i),
                e if self.words[(e as usize - 1) * g..e as usize * g] == *w => return Ok(()),
                _ => i = (i + 1) & mask,
            }
        }
    }

    #[inline(always)]
    pub(super) fn contains(&self, w: &[u64]) -> bool {
        let h = Self::hash(w);
        self.filter & Self::bit(h) != 0 && self.find(w, h).is_ok()
    }

    /// Adds group `w` (once); `false` when there is no memory for it.
    #[inline]
    pub(super) fn insert(&mut self, w: &[u64]) -> bool {
        let n = self.words.len() / self.g;
        if 4 * (n + 1) > self.index.len() && !self.grow(n + 1) {
            return false;
        }
        let h = Self::hash(w);
        match self.find(w, h) {
            Ok(()) => true,
            Err(slot) => {
                if self.words.try_reserve(self.g).is_err() {
                    return false;
                }
                self.words.extend_from_slice(w);
                self.index[slot] = (n + 1) as u32;
                self.filter |= Self::bit(h);
                true
            }
        }
    }

    /// The index rebuilt at the smallest power of two at least four times `groups` slots.
    fn grow(&mut self, groups: usize) -> bool {
        let slots = (4 * groups).next_power_of_two();
        let mut index = Vec::new(); // alloc: Exec
        if index.try_reserve_exact(slots).is_err() {
            return false;
        }
        index.resize(slots, 0u32);
        self.index = index;
        for k in 0..self.words.len() / self.g {
            let w = &self.words[k * self.g..(k + 1) * self.g];
            let Err(slot) = self.find(w, Self::hash(w)) else { continue };
            self.index[slot] = (k + 1) as u32;
        }
        true
    }
}
