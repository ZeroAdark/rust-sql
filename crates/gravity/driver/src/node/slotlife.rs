//! C4 SlotLife: the life of each of a Node's program slots, in one table. A slot is dead (no region open: its producer's
//! `open_slot` draws one), live (opened, until `free_slot` or the statement's end), pending (D824: a lazy build's slot
//! opened zeroed before its producer runs) or kept for the statement (M8 RT-2e / RT-2f: a rerun set's or a pulled set's
//! slot, which `free_after` leaves and each run resets in place). Every change of a slot's life is a method here and the
//! fields are this module's own, so no other driver function sets one by hand; the region, its grant and its stream stay
//! the Node's (`SlotRegion`, the spill table).

use crate::mem::arena::{ArenaRef, ArenaVec};

/// A set of slots (CAP-35): a bit per `u8` slot, so every slot a program names fits (the old `u64` masks refused slots
/// past 63 by name).
#[derive(Clone, Copy, Default)]
struct SlotSet([u64; 4]);

impl SlotSet {
    const EMPTY: SlotSet = SlotSet([0; 4]);

    fn has(&self, slot: u8) -> bool {
        gravity_program::laneset::has(&self.0, slot as usize)
    }

    fn set(&mut self, slot: u8) {
        gravity_program::laneset::set(&mut self.0, slot as usize);
    }

    fn unset(&mut self, slot: u8) {
        gravity_program::laneset::unset(&mut self.0, slot as usize);
    }

    fn is_empty(&self) -> bool {
        !gravity_program::laneset::any(&self.0)
    }
}

/// The slots' lives of one Node (one entry per program slot), in its arena.
pub(super) struct SlotLives {
    /// Bit per slot: its region open (`open`) until `free` or the statement's end (`clear`).
    live: ArenaVec<u64>,
    /// D3546: per slot, the producer whose open began its spill-stream life (`u32::MAX` before any open): a stream end
    /// `free_after` meets for another producer leaves that life alone (`Node::holds_life`).
    opener: ArenaVec<u32>,
    /// D824 (V4R-298): a lazy build's slot opened zeroed before its producer ran (`Node::open_pending`).
    pending: SlotSet,
    /// M8 (RT-2e): the slots a rerun set's member produces (`keep_unrun`): kept for the statement (`free_after` never
    /// frees it), so each rerun resets it in place (`Node::reset_in_place`) and draws no region again; a pulled set's store
    /// only until its last reader is done (`readers_left`, D2498).
    kept: SlotSet,
    /// M8 (RT-2e): the kept slots no member has produced yet this execution; only their owners may open over one.
    unrun: SlotSet,
    /// M8 (RT-2e): the kept slots reset in place for a rerun; the member's open adopts the region (`adopt`).
    reset: SlotSet,
    /// M8 (RT-2e / RT-2f): (kept slot, owner): the pipelines that may open over a kept slot before its members first run
    /// (a rerun set's owner, calling `m8_rerun`; every reader of a pulled CTE, pulling before it reads).
    owners: Vec<(u8, u16)>,
    /// D2498: the readers of each pulled set's store slot (`keep_unrun` without `every`) not yet done: the store is kept
    /// only for them, so `free_after` frees it after the last of them, as the plan's lifetime ends it (a rerun member
    /// consuming it keeps it for the statement: its entries go).
    readers_left: Vec<(u8, u16)>,
    /// D2242: bit per share-budgeted slot (< 64) an on-demand member produces (`defer`), until it opens: a member run nested
    /// inside its reader's pipeline opens beside the reader's live share, so a share opened while one waits leaves it room.
    deferred: u64,
    /// D4358: the slots whose run a consumer committed to read as it lies before its pipeline opened (merge.cosplit's
    /// cut laid its claims' row ranges over them, `Node::hold_as_laid`): no give-back puts them (`own_spillable`). Until the
    /// slot's next open or free, or the statement's end.
    laid: SlotSet,
}

impl SlotLives {
    pub(super) fn new_in(nslots: usize, a: ArenaRef) -> SlotLives {
        let words = gravity_program::laneset::words_for(nslots);
        let mut live: ArenaVec<u64> = ArenaVec::with_capacity_in(words, a);
        live.resize(words, 0);
        let mut opener: ArenaVec<u32> = ArenaVec::with_capacity_in(nslots, a);
        opener.resize(nslots, u32::MAX);
        let (pending, kept, unrun, reset) = (SlotSet::EMPTY, SlotSet::EMPTY, SlotSet::EMPTY, SlotSet::EMPTY);
        SlotLives { live, opener, pending, kept, unrun, reset, owners: Vec::new(), readers_left: Vec::new(), deferred: 0, laid: SlotSet::EMPTY } // alloc: Node
    }

    pub(super) fn is_live(&self, slot: u8) -> bool {
        gravity_program::laneset::has(&self.live, slot as usize)
    }

    /// Slot `slot`'s producer `p` opens it while it is live: whether the open adopts it. D824: a lazy build (`lazy`) adopts
    /// the slot its probe's pipeline opened over it, zeroed (`pend`). M8 (RT-2e): a rerun member adopts its kept slot,
    /// opened zeroed for the owner (`keep_unrun`) or reset in place (`reset_kept`). Otherwise the open is the caller's to
    /// judge (an append.arms run's later arm, else L3).
    pub(super) fn adopt(&mut self, slot: u8, lazy: bool) -> bool {
        if self.pending.has(slot) && lazy {
            return true;
        }
        if self.reset.has(slot) || self.unrun.has(slot) {
            self.reset.unset(slot);
            self.unrun.unset(slot);
            return true;
        }
        false
    }

    /// D3546: producer `p`'s open of slot `slot` begins its spill-stream life.
    pub(super) fn begin_stream(&mut self, slot: u8, p: usize) {
        self.opener[slot as usize] = p as u32;
    }

    /// D3546: the producer whose open began slot `slot`'s spill-stream life (`u32::MAX` before any open).
    pub(super) fn opener(&self, slot: u8) -> u32 {
        self.opener[slot as usize]
    }

    /// Slot `slot`'s region is open: live, produced (no longer a never-run kept slot), no longer waiting (`defer`).
    pub(super) fn open(&mut self, slot: u8) {
        debug_assert!(!self.is_live(slot), "slot {slot} opened while live");
        gravity_program::laneset::set(&mut self.live, slot as usize);
        self.unrun.unset(slot);
        self.laid.unset(slot);
        if slot < 64 {
            self.deferred &= !(1u64 << slot);
        }
    }

    /// Slot `slot`'s region is given back (a live slot; the caller frees the region).
    pub(super) fn free(&mut self, slot: u8) {
        gravity_program::laneset::unset(&mut self.live, slot as usize);
        self.laid.unset(slot);
    }

    /// D4358: slot `slot`'s run is read as it lies by a cut laid over it before its reader opened (`Node::hold_as_laid`).
    pub(super) fn hold_as_laid(&mut self, slot: u8) {
        self.laid.set(slot);
    }

    /// D4358: whether a cut was laid over slot `slot`'s run (`hold_as_laid`) since it opened.
    pub(super) fn is_held_as_laid(&self, slot: u8) -> bool {
        self.laid.has(slot)
    }

    /// D824: lazy build slot `slot` opened zeroed before its producer runs.
    pub(super) fn pend(&mut self, slot: u8) {
        debug_assert!(self.is_live(slot), "lazy build slot {slot} pending while not live");
        self.pending.set(slot);
    }

    /// D824: slot `slot`'s lazy build has run: its slot is the probe's table now.
    pub(super) fn built(&mut self, slot: u8) {
        self.pending.unset(slot);
    }

    pub(super) fn is_pending(&self, slot: u8) -> bool {
        self.pending.has(slot)
    }

    pub(super) fn any_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Slot `slot` kept for the statement (D3515 a rerun member's from its set's first run, D2701 a rewound emit's
    /// consumed slot, D1430 / D1431 a rerun member's consumed slot): `free_after` leaves it.
    pub(super) fn keep(&mut self, slot: u8) {
        self.kept.set(slot);
    }

    pub(super) fn is_kept(&self, slot: u8) -> bool {
        self.kept.has(slot)
    }

    /// D1431: slot `slot` a rerun member consumes kept for the statement; with `every` (a rerun member's, not a pulled
    /// set's) it is no pulled store's any more: its readers' entries go (D2498).
    pub(super) fn keep_consumed(&mut self, slot: u8, every: bool) {
        self.kept.set(slot);
        if every {
            self.readers_left.retain(|&(x, _)| x != slot);
        }
    }

    /// M8 (RT-2e / RT-2f): rerun member slot `slot` stepped past (`Node::defer_rerun`) is kept for the statement and marked
    /// never run, and `owners` may open over it before it runs. A pulled set's store (`every` false, not kept yet or already
    /// a store) is kept only for its readers, `owners`, until the last of them is done (D2498, `reader_done`).
    pub(super) fn keep_unrun(&mut self, slot: u8, owners: &[usize], every: bool) {
        let pulled = !every && (!self.kept.has(slot) || self.readers_left.iter().any(|&(x, _)| x == slot));
        if !pulled {
            self.readers_left.retain(|&(x, _)| x != slot);
        }
        self.kept.set(slot);
        self.unrun.set(slot);
        for &o in owners {
            if !self.owners.contains(&(slot, o as u16)) {
                self.owners.push((slot, o as u16)); // alloc: Exec
            }
            if pulled && !self.readers_left.contains(&(slot, o as u16)) {
                self.readers_left.push((slot, o as u16)); // alloc: Exec
            }
        }
    }

    /// M8 (RT-2e): kept slot `slot` reset in place for a rerun (its region, life and grant kept): the member's open adopts it.
    pub(super) fn reset_kept(&mut self, slot: u8) {
        debug_assert!(self.kept.has(slot) && self.is_live(slot), "slot {slot} reset in place while not kept and live");
        self.reset.set(slot);
    }

    /// M8 (RT-2e): a rerun's reset of slot `slot` its member did not open (an error or a cancel stopped the run first) dropped.
    pub(super) fn drop_reset(&mut self, slot: u8) {
        self.reset.unset(slot);
    }

    /// M8 (RT-2e): whether `slot` is a rerun set's slot no member has produced yet this execution.
    pub(super) fn is_unrun(&self, slot: u8) -> bool {
        self.unrun.has(slot)
    }

    pub(super) fn any_unrun(&self) -> bool {
        !self.unrun.is_empty()
    }

    /// M8 (RT-2e): pipeline `p` reads never-run kept slot `slot` and is none of its owners: that slot's owner
    /// (`u16::MAX` when it has none), for the caller's Bug refusal. `None`: the read is the slot's life's. D3914: `pass`
    /// is the pipeline `p` is a pass of (`Pipeline::pass_of`): a pass opens where that pipeline may.
    pub(super) fn unrun_reader(&self, slot: u8, p: usize, pass: Option<usize>) -> Option<u16> {
        let may = |q: usize| self.owners.contains(&(slot, q as u16));
        if !self.unrun.has(slot) || may(p) || pass.is_some_and(may) {
            return None;
        }
        Some(self.owners.iter().find(|&&(s, _)| s == slot).map_or(u16::MAX, |&(_, o)| o))
    }

    /// D2498: reader `p` of a pulled set's store is done: the next store slot it was the last reader of, its entry gone
    /// (the caller frees it when its producer ran to its end, `unkeep`). D3271: a store the program holds past `p` (`held`:
    /// its last consumer past `p`, a lock's recheck re-running an InitPlan that reads it, lowerv4 `hold_epq_reads`) waits for
    /// that pipeline instead.
    pub(super) fn reader_done(&mut self, p: usize, held: impl Fn(u8) -> Option<usize>) -> Option<u8> {
        let mut i = 0;
        while i < self.readers_left.len() {
            let (slot, o) = self.readers_left[i];
            if o as usize != p {
                i += 1;
                continue;
            }
            if let Some(end) = held(slot) {
                self.readers_left[i].1 = end as u16;
                i += 1;
                continue;
            }
            self.readers_left.swap_remove(i);
            if !self.readers_left.iter().any(|&(x, _)| x == slot) {
                return Some(slot);
            }
        }
        None
    }

    /// D2498: pulled store slot `slot`'s kept marks dropped (its last reader done, its producer ran out), so a later
    /// producer of the slot number opens it as any.
    pub(super) fn unkeep(&mut self, slot: u8) {
        self.kept.unset(slot);
        self.unrun.unset(slot);
        self.reset.unset(slot);
        self.owners.retain(|&(x, _)| x != slot);
    }

    /// D2242: share-budgeted slot `slot`'s on-demand producer waits to open (slots < 64; a later one is not counted).
    pub(super) fn defer(&mut self, slot: u8) {
        if slot < 64 {
            self.deferred |= 1u64 << slot;
        }
    }

    /// D2242: the on-demand slots waiting to open besides `slot`.
    pub(super) fn deferred_besides(&self, slot: u8) -> u64 {
        let own = if slot < 64 { 1u64 << slot } else { 0 };
        (self.deferred & !own).count_ones() as u64
    }

    /// The statement's end (`Node::reset_timed`): every slot dead, nothing pending, kept or waiting. The stream openers
    /// stay (each open sets its own, `begin_stream`).
    pub(super) fn clear(&mut self) {
        self.live.fill(0);
        (self.pending, self.kept, self.unrun, self.reset, self.laid) = (SlotSet::EMPTY, SlotSet::EMPTY, SlotSet::EMPTY, SlotSet::EMPTY, SlotSet::EMPTY);
        self.owners.clear();
        self.readers_left.clear();
        self.deferred = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::arena::Arena;

    #[test]
    fn a_pulled_store_frees_after_its_last_reader() {
        let arena = Arena::new(1 << 14);
        let mut l = SlotLives::new_in(4, arena.handle());
        l.open(2);
        l.keep_unrun(2, &[5, 7], false);
        assert!(l.is_kept(2) && l.is_unrun(2));
        assert_eq!(l.unrun_reader(2, 5, None), None, "an owner reads a never-run kept slot");
        assert_eq!(l.unrun_reader(2, 6, None), Some(5), "any other reader is named with the slot's owner");
        assert_eq!(l.unrun_reader(2, 6, Some(5)), None, "a pass of an owner opens where the owner does (D3914)");
        assert_eq!(l.unrun_reader(2, 6, Some(4)), Some(5), "a pass of another pipeline is still named");
        assert_eq!(l.reader_done(5, |_| None), None, "reader 7 still reads the store");
        assert_eq!(l.reader_done(7, |_| Some(9)), None, "the program holds the store past 7, to pipeline 9");
        assert_eq!(l.reader_done(9, |_| None), Some(2));
        l.unkeep(2);
        assert!(!l.is_kept(2) && !l.is_unrun(2) && l.unrun_reader(2, 6, None).is_none());
    }

    #[test]
    fn a_kept_slot_reset_in_place_is_adopted_once() {
        let arena = Arena::new(1 << 14);
        let mut l = SlotLives::new_in(4, arena.handle());
        l.begin_stream(1, 3);
        l.open(1);
        l.keep(1);
        l.reset_kept(1);
        assert!(l.adopt(1, false), "the member's open adopts its reset slot");
        assert!(!l.adopt(1, false), "a second open of the live slot is not an adoption (L3 is the caller's)");
        assert_eq!(l.opener(1), 3);
        l.free(1);
        l.clear();
        assert!(!l.is_live(1) && !l.is_kept(1));
        assert_eq!(l.opener(1), 3, "a stream's opener stays until the slot's next open");
    }

    #[test]
    fn a_lazy_build_adopts_its_pending_slot_and_a_deferred_slot_stops_waiting_at_its_open() {
        let arena = Arena::new(1 << 14);
        let mut l = SlotLives::new_in(70, arena.handle());
        l.open(0);
        l.pend(0);
        assert!(!l.adopt(0, false), "only the lazy build adopts a pending slot");
        assert!(l.adopt(0, true));
        l.built(0);
        assert!(!l.any_pending());
        l.defer(4);
        l.defer(65);
        assert_eq!(l.deferred_besides(5), 1, "a slot past 63 is not counted");
        assert_eq!(l.deferred_besides(4), 0);
        l.open(4);
        assert_eq!(l.deferred_besides(5), 0);
    }

    /// D4358: a co-split's cut holds a slot's run as it lies (`hold_as_laid`) until the slot opens again (a rerun's fresh run),
    /// is freed, or the statement ends; another slot is never held by it.
    #[test]
    fn a_cut_holds_its_slot_as_laid_until_the_slot_opens_again_or_frees() {
        let arena = Arena::new(1 << 14);
        let mut l = SlotLives::new_in(70, arena.handle());
        for s in [1u8, 2, 66] {
            l.open(s);
            l.hold_as_laid(s);
        }
        assert!(l.is_held_as_laid(1) && l.is_held_as_laid(2) && l.is_held_as_laid(66) && !l.is_held_as_laid(3));
        l.free(1);
        assert!(!l.is_held_as_laid(1), "a freed slot's run is gone");
        l.free(2);
        l.open(2);
        assert!(!l.is_held_as_laid(2), "a reopened slot holds a new run no cut was laid over");
        l.clear();
        assert!(!l.is_held_as_laid(66), "the statement's end clears every hold");
    }
}
