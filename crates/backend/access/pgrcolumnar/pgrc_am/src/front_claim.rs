//! D1159 (c): which front writes must invalidate a `sorted_by` table's cached plans.
//!
//! A front-bearing table gets no `sorted_by` claim (`tableam::pgrc_planner_sorted_attnos`), and a plan holding the
//! claim was built while every front heap (active segment, tombstone heaps) had no block. D1159 (b) sent the
//! invalidation only from the writer that found a heap empty. That writer's registration dies with it when it
//! aborts, but its block stays; a writer that finds the block and commits first (after an abort, or while the
//! empty-finder is still open) used to send nothing, so the stale plan read its unsorted rows.
//!
//! The rule: every transaction's first write to a front heap of a `sorted_by` table registers
//! `CacheInvalidateRelcache` unless the heap is **covered** — a committed transaction that registered one has
//! written it since a writer last found it empty. Per heap (keyed by the heap's relid) a process-global cell holds
//! an epoch and the covered bit:
//! - a writer that finds the heap empty starts a new epoch, uncovered, and registers;
//! - a writer of an uncovered heap registers;
//! - a registration made at the top transaction level covers the heap, at the epoch it saw, once its commit has sent
//!   the invalidation. The commit callback runs before `AtEOXact_Inval`, so the commit only queues it (per backend)
//!   and the backend's next transaction event, its next front check or its exit promotes it; an epoch that moved
//!   meanwhile stays uncovered.
//!
//! A covered heap has had a block since before any plan that could still hold the claim was invalidated: a claim needs
//! every front heap at 0 blocks, and the writer that next extends it finds it empty (the heap is locked from its check
//! to its insert). Subtransaction registrations, aborts, PREPARE and restarts only leave heaps uncovered (more
//! invalidations, never fewer). Cost: a backend-local check per transaction and heap; one global lock on the
//! transaction's first write to each heap; no invalidation once covered. front_bearing is not read or changed.

use std::collections::BTreeMap;

use types_core::Oid;
use types_error::PgResult;
use types_rel::Relation;

pgsync::process_global! {
    /// heap relid → (epoch, covered), and the next epoch.
    static HEAPS: pgsync::Mutex<Cells> = pgsync::Mutex::new(Cells::new());
}

/// The per-heap cells (the state machine; [`front_write`] and the promotions drive it under the lock).
pub(crate) struct Cells {
    next: u64,
    map: BTreeMap<u32, (u64, bool)>,
}

impl Cells {
    pub(crate) const fn new() -> Cells {
        Cells { next: 1, map: BTreeMap::new() }
    }

    /// A transaction's first write to `heap` (or any write that found it empty): the epoch its registration is for,
    /// or `None` when the heap is covered and nothing is registered.
    pub(crate) fn write(&mut self, heap: u32, found_empty: bool) -> Option<u64> {
        match self.map.get(&heap).copied() {
            Some((e, covered)) if !found_empty => (!covered).then_some(e),
            _ => {
                let e = self.next;
                self.next += 1;
                self.map.insert(heap, (e, false));
                Some(e)
            }
        }
    }

    /// A committed registration's invalidation has been sent: `heap` is covered unless its epoch moved (false).
    pub(crate) fn cover(&mut self, heap: u32, epoch: u64) -> bool {
        match self.map.get_mut(&heap) {
            Some(cell) if cell.0 == epoch => {
                cell.1 = true;
                true
            }
            _ => false,
        }
    }
}

/// Nothing to check for this write: `rel` declares no `sorted_by` key, or this subtransaction already wrote front heap
/// `heap` (a backend-local look; the segment writer asks before it reads the heap's size).
pub fn front_checked(rel: &Relation<'_>, heap: Oid) -> bool {
    if !crate::reloptions::declares_sorted_by(rel) {
        return true;
    }
    let sub = xact::GetCurrentSubTransactionId();
    crate::session::with_front_seen(|s| s.iter().any(|&(h, x, _)| h == heap && x == sub))
}

/// A transaction is about to write (segment) or just wrote (tombstone) front heap `heap` of `rel`; `found_empty`: the
/// heap had no block before this write. Registers the relcache invalidation when the rule above asks for it.
pub fn front_write(rel: &Relation<'_>, heap: Oid, found_empty: bool) -> PgResult<()> {
    if !crate::reloptions::declares_sorted_by(rel) {
        return Ok(());
    }
    crate::session::ensure_session_hooks();
    ensure_exit_hook()?;
    promote();
    let sub = xact::GetCurrentSubTransactionId();
    if !found_empty && crate::session::with_front_seen(|s| s.iter().any(|&(h, x, _)| h == heap && x == sub)) {
        return Ok(());
    }
    let epoch = pgsync::lock(&HEAPS).write(heap, found_empty);
    if let Some(e) = epoch {
        inval::invalidate::CacheInvalidateRelcache(rel)?;
        let _ = elog::elog(types_error::DEBUG1, format!("pgrc front_claim: relcache invalidation of \"{}\" (front heap {heap} epoch {e}{})", rel.name(), if found_empty { ", found empty" } else { "" })); // alloc: Cold
    }
    crate::session::with_front_seen(|s| s.push((heap, sub, epoch)));
    Ok(())
}

/// The pgrc xact callback, every event: queued coverings of earlier commits are promoted (their `AtEOXact_Inval`
/// has run). `end` = Some(committed) at transaction end: this transaction's writes are forgotten, and at a commit its
/// top-level registrations are queued.
pub(crate) fn at_xact_event(end: Option<bool>) {
    promote();
    let Some(committed) = end else { return };
    let seen = crate::session::with_front_seen(std::mem::take);
    if committed {
        let top = types_core::xact::TopSubTransactionId;
        crate::session::with_front_pending(|p| {
            p.extend(seen.into_iter().filter_map(|(h, x, e)| if x == top { e.map(|e| (h, e)) } else { None }))
        });
    }
}

fn promote() {
    let pending = crate::session::with_front_pending(std::mem::take);
    if pending.is_empty() {
        return;
    }
    let mut g = pgsync::lock(&HEAPS);
    for (heap, epoch) in pending {
        let covered = g.cover(heap, epoch);
        let _ = elog::elog(types_error::DEBUG1, format!("pgrc front_claim: cover of front heap {heap} epoch {epoch} {}", if covered { "applied" } else { "discarded (epoch moved)" })); // alloc: Cold
    }
}

/// A backend's last commit is promoted at its exit (`before_shmem_exit`: its `AtEOXact_Inval` has run), so a
/// one-transaction session covers the heaps it wrote too.
fn ensure_exit_hook() -> PgResult<()> {
    if crate::session::with_front_exit_latch(|c| c.replace(true)) {
        return Ok(());
    }
    ipc_seams::before_shmem_exit::call(at_exit, datum::Datum::null())
}

fn at_exit(_code: i32, _arg: datum::Datum) -> PgResult<()> {
    promote();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Cells;

    #[test]
    fn empty_finder_then_uncovered_then_cover_promoted() {
        let mut c = Cells::new();
        let e1 = c.write(7, true).expect("the empty-finder registers");
        assert_eq!(c.write(7, false), Some(e1), "uncovered: every first write registers at the same epoch");
        c.cover(7, e1);
        assert_eq!(c.write(7, false), None, "covered: nothing registered");
    }

    #[test]
    fn abort_leaves_it_uncovered() {
        let mut c = Cells::new();
        let e1 = c.write(7, true).unwrap();
        // the empty-finder aborts: no cover is queued, its block stays
        assert_eq!(c.write(7, false), Some(e1));
        assert_eq!(c.write(7, false), Some(e1));
    }

    #[test]
    fn an_epoch_bump_discards_a_pending_cover() {
        let mut c = Cells::new();
        let e1 = c.write(7, true).unwrap();
        // the heap empties (rotation / truncation) and a writer finds it empty before e1's cover is promoted
        let e2 = c.write(7, true).unwrap();
        assert_ne!(e1, e2);
        assert!(!c.cover(7, e1), "a stale cover is discarded");
        assert_eq!(c.write(7, false), Some(e2), "the stale cover must not cover the new epoch");
        c.cover(7, e2);
        assert_eq!(c.write(7, false), None);
    }

    #[test]
    fn found_empty_uncovers_a_covered_heap() {
        let mut c = Cells::new();
        let e1 = c.write(7, true).unwrap();
        c.cover(7, e1);
        assert_eq!(c.write(7, false), None);
        let e2 = c.write(7, true).expect("the heap emptied: the next writer registers");
        assert_eq!(c.write(7, false), Some(e2));
    }

    #[test]
    fn heaps_are_independent() {
        let mut c = Cells::new();
        let a = c.write(7, true).unwrap();
        c.cover(7, a);
        assert!(c.write(8, false).is_some(), "an unknown heap is uncovered");
        assert_eq!(c.write(7, false), None);
    }
}
