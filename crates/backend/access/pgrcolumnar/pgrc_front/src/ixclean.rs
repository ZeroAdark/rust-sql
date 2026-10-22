//! [`Check::dead`]: whether row `id` is dead to everyone, judged on the newest generation, and what its
//! entries' removal needs as a standby conflict horizon:
//!
//! - (i) a part home whose DV bit is set: the fold that set it folded only tombstones whose deleters were below the
//!   horizon; horizon `H_fold`;
//! - (ii) a tombstone whose deleter committed below `GlobalVisState` (kill hints and `index_delete_tuples` ask, VACUUM
//!   does not: a fold turns it into (i)); horizon its deleter's xid, as heap's xmax;
//! - (iii) a segment row heap's `SnapshotNonVacuumable` finds vacuumable (an aborted insert; a pruned `LP_DEAD` item,
//!   whose prune logged its horizon); horizon `HeapTupleHeaderAdvanceConflictHorizon`'s;
//! - (iv) no home, in no unlisted segment's extents and in no bulk reservation in flight (`rowid_alloc::in_flight`):
//!   an aborted or burned bulk range, an aborted segment insert a seal left behind, or a row compaction dropped
//!   (DV-marked before); horizon `H_fold`.
//!
//! `H_fold` is the newest generation's retired tombstone heap's `folded` when set: every earlier fold's reset logged
//! its own conflict record at its fold's xid before the switch past it, so a DV bit or a dropped row of an earlier fold
//! needs nothing newer.
//!
//! The order is the race's answer: the in-flight table is read first, then the newest generation
//! and the extent table, so a bulk load that publishes, commits and leaves the table between the two reads is found
//! in one of them. VACUUM reads both after collecting its candidates, so every load whose entries it collected had
//! registered before it read the table.

use std::rc::Rc;
use std::sync::Arc;

use bufmgr_seams::{BufferPin, BUFFER_LOCK_SHARE, BUFFER_LOCK_UNLOCK};
use mcx::Mcx;
use pgrc_am::catalog::{Generation, RowMap};
use pgrc_am::rowid_alloc::{self, Extents};
use pgrc_format::rowid::tid_of;
use pgrc_front_seams::{TombHit, TombState};
use tableam_vocab::{Snapshot, TM_IndexDelete, TM_IndexDeleteOp};
use types_core::xact::{InvalidTransactionId, TransactionIdIsValid, TransactionIdPrecedes};
use types_core::{ForkNumber, Oid, TransactionId};
use types_error::PgResult;
use types_rel::{AccessShareLock, NoLock, Relation, RelationData};
use types_snapshot::{SnapshotData, SnapshotType};
use types_tuple::{FirstOffsetNumber, HeapTupleHeaderData, ItemPointerData, ItemPointerGetBlockNumberNoCheck, ItemPointerGetOffsetNumberNoCheck, ItemPointerSet};

use crate::position::{self, Located};

pub const BETWEEN_POINT: &str = "pgrc-ixclean-between";

/// A bottom-up deletion checks at most this many TIDs (heap's reads at most six heap blocks).
const BOTTOMUP_MAX_CHECKS: usize = 256;

const AHEAD_FIRST: usize = 16;
const AHEAD_MAX: usize = 512;

/// A bottom-up pass judges tombstone-read TIDs in groups of this many and gives up after one that frees nothing.
const BOTTOMUP_GROUP: usize = AHEAD_FIRST;

/// Why a row is dead to everyone, which names its horizon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Why {
    Fold,
    Deleter(TransactionId),
    Segment(TransactionId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// VACUUM's prefilter while it collects: no in-flight table (nothing it judges dead is removed).
    Collect,
    Vacuum,
    Delete,
}

enum Home {
    None,
    Part { dv: bool },
    Segment { relid: Oid, tid: ItemPointerData },
}

pub(crate) struct Check<'mcx> {
    mcx: Mcx<'mcx>,
    mode: Mode,
    rel: Relation<'mcx>,
    flights: Vec<(u64, u64)>,
    gen: Option<Arc<Generation>>,
    dir: String,
    ext: Arc<Extents>,
    pub(crate) h_fold: u64,
    segs: Vec<(Oid, Option<Relation<'mcx>>)>,
    pending: Option<Vec<u32>>,
    tombs: Option<Option<Vec<crate::tombs::Opened<'mcx>>>>,
    ahead: Vec<u64>,
    read: Vec<(u64, Option<TombHit>)>,
    window: usize,
}

impl<'mcx> Check<'mcx> {
    pub(crate) fn new(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, mode: Mode) -> PgResult<Check<'mcx>> {
        let dir = rowid_alloc::dir_of(rel);
        let flights = match mode {
            Mode::Collect => Vec::new(),
            Mode::Vacuum | Mode::Delete => rowid_alloc::in_flight(&dir)?,
        };
        if mode == Mode::Vacuum {
            injection_point::injection_point(BETWEEN_POINT)?;
        }
        let gen = crate::ring::newest(rel)?;
        let ext = rowid_alloc::extents(&dir)?;
        let h_fold = crate::tombs::state(gen.as_deref())?.ret.map_or(0, |r| r.folded);
        Ok(Check { mcx, mode, rel: rel.alias(), flights, gen, dir, ext, h_fold, segs: Vec::new(), pending: None, tombs: None, ahead: Vec::new(), read: Vec::new(), window: AHEAD_FIRST })
    }

    pub(crate) fn end(self) -> PgResult<()> {
        let mut first = Ok(());
        for s in self.segs.into_iter().filter_map(|(_, s)| s) {
            let r = s.close(NoLock);
            if first.is_ok() {
                first = r;
            }
        }
        if let Some(Some(t)) = self.tombs {
            let r = crate::tombs::close_all(t);
            if first.is_ok() {
                first = r;
            }
        }
        first
    }

    fn home(&self, id: u64) -> PgResult<Home> {
        let gen = self.gen.as_ref();
        let dir = &self.dir;
        let map_of = |p: usize| -> PgResult<Arc<RowMap>> { gen.expect("a part home has a generation").row_map(p, dir) };
        Ok(match position::home_in(gen, &[], &self.ext, id, &map_of)? {
            None => Home::None,
            Some(Located::Segment { relid, tid, .. }) => Home::Segment { relid, tid },
            Some(Located::Part { gen, part, granule, row, .. }) => {
                let mask = gen.parts.get(part).and_then(|p| p.deletes.as_ref()).and_then(|d| d.granule_mask(granule));
                Home::Part { dv: mask.is_some_and(|m| m[row as usize / 64] >> (row % 64) & 1 == 1) }
            }
        })
    }

    /// VACUUM's prefilter: false for a part row whose DV bit is clear (not dead to VACUUM in this generation, and a
    /// fold since only defers its entry to the next VACUUM).
    pub(crate) fn may_be_dead(&self, id: u64) -> PgResult<bool> {
        Ok(!matches!(self.home(id)?, Home::Part { dv: false }))
    }

    fn in_flight(&self, id: u64) -> bool {
        self.flights.iter().any(|&(lo, hi)| lo <= id && id < hi)
    }

    pub(crate) fn dead(&mut self, id: u64, tombs: bool) -> PgResult<Option<Why>> {
        match self.home(id)? {
            Home::None => {
                if self.in_flight(id) {
                    return Ok(None);
                }
                let gen = self.gen.as_deref();
                if gen.is_some_and(|g| g.home(id).is_some()) {
                    return Ok(Some(Why::Fold));
                }
                let Some((p, _)) = self.ext.seg_of(id) else { return Ok(Some(Why::Fold)) };
                if gen.is_some_and(|g| g.segments.iter().any(|r| r.part_no == p)) {
                    return Ok(Some(Why::Fold));
                }
                Ok((!self.pending()?.contains(&p)).then_some(Why::Fold))
            }
            Home::Part { dv: true } => Ok(Some(Why::Fold)),
            Home::Part { dv: false } => if tombs { self.deleted(id) } else { Ok(None) },
            Home::Segment { relid, tid } => {
                let mcx = self.mcx;
                let Some(seg) = self.segment(relid)? else { return Ok(None) };
                if let Some(h) = segment_dead(mcx, seg, &tid)? {
                    return Ok(Some(Why::Segment(h)));
                }
                if tombs { self.deleted(id) } else { Ok(None) }
            }
        }
    }

    fn pending(&mut self) -> PgResult<&[u32]> {
        if self.pending.is_none() {
            let l = self.rel.rd_locator.get();
            let check = pgrc_am::probe::UnabortedCheck::new();
            let segs = match pgrc_am::catalog::visible((l.spcOid, l.dbOid, l.relNumber as u64), &check)? {
                pgrc_am::catalog::Lookup::Gen(g) => g.segments.iter().map(|r| r.part_no).collect(),
                pgrc_am::catalog::Lookup::Empty | pgrc_am::catalog::Lookup::Unknown => Vec::new(),
            };
            self.pending = Some(segs);
        }
        Ok(self.pending.as_deref().expect("just read"))
    }

    fn segment(&mut self, relid: Oid) -> PgResult<Option<&Relation<'mcx>>> {
        let i = match self.segs.iter().position(|(r, _)| *r == relid) {
            Some(i) => i,
            None => {
                let seg = self.open(relid)?;
                self.segs.push((relid, seg));
                self.segs.len() - 1
            }
        };
        Ok(self.segs[i].1.as_ref())
    }

    /// A companion heap under AccessShareLock; `index_delete_tuples` runs under an index page's buffer lock, so it
    /// only takes a lock it is granted at once (`None` otherwise: no rule that needs the heap holds).
    fn open(&self, relid: Oid) -> PgResult<Option<Relation<'mcx>>> {
        match self.mode {
            Mode::Delete => match lmgr::ConditionalLockRelationOid(relid, AccessShareLock)? {
                true => table::table_open(self.mcx, relid, NoLock).map(Some),
                false => Ok(None),
            },
            Mode::Collect | Mode::Vacuum => table::table_open(self.mcx, relid, AccessShareLock).map(Some),
        }
    }

    fn tomb_heaps(&mut self) -> PgResult<Option<&[crate::tombs::Opened<'mcx>]>> {
        if self.tombs.is_none() {
            let mut out = Vec::new();
            let mut all = true;
            for (slot, relid) in crate::tombs::all(self.rel.rd_id)? {
                match self.open(relid) {
                    Ok(Some(rel)) => out.push(crate::tombs::Opened { slot, rel }),
                    Ok(None) => all = false,
                    Err(e) => {
                        crate::tombs::close_all(out)?;
                        return Err(e);
                    }
                }
            }
            self.tombs = Some(match all {
                true => Some(out),
                false => {
                    crate::tombs::close_all(out)?;
                    None
                }
            });
        }
        Ok(self.tombs.as_ref().and_then(|t| t.as_deref()))
    }

    fn deleted(&mut self, id: u64) -> PgResult<Option<Why>> {
        if self.tomb_heaps()?.is_none() {
            return Ok(None);
        }
        let hit = match self.read.binary_search_by_key(&id, |&(r, _)| r) {
            Ok(i) => self.read[i].1,
            Err(_) => self.read_ahead(id)?,
        };
        let Some(hit) = hit else { return Ok(None) };
        if hit.state != TombState::Committed {
            return Ok(None);
        }
        let removable = procarray::GlobalVisTestIsRemovableXid(procarray::GlobalVisTestFor(&self.rel), hit.xmin)?;
        Ok(removable.then_some(Why::Deleter(hit.xmin)))
    }

    fn read_ahead(&mut self, id: u64) -> PgResult<Option<TombHit>> {
        let mut ids = vec![id];
        for k in self.ahead.iter().position(|&x| x == id).map_or(self.ahead.len(), |p| p + 1)..self.ahead.len() {
            if ids.len() >= self.window {
                break;
            }
            let x = self.ahead[k];
            if matches!(self.home(x)?, Home::Part { dv: false } | Home::Segment { .. }) {
                ids.push(x);
            }
        }
        ids.sort_unstable();
        ids.dedup();
        self.window = (self.window * 2).min(AHEAD_MAX);
        let mcx = self.mcx;
        let mut hits: Vec<Option<TombHit>> = Vec::new();
        hits.try_reserve(ids.len()).map_err(|_| mcx.oom(ids.len() * core::mem::size_of::<Option<TombHit>>()))?;
        hits.resize(ids.len(), None);
        let heaps = self.tomb_heaps()?.expect("deleted() opened them");
        for h in heaps {
            let todo: Vec<u64> = ids.iter().zip(&hits).filter(|(_, f)| f.is_none()).map(|(&x, _)| x).collect();
            if todo.is_empty() {
                break;
            }
            let dirty: Snapshot<'mcx> = Some(Rc::new(SnapshotData::sentinel(mcx, SnapshotType::SNAPSHOT_DIRTY)));
            crate::ixfetch::stats(|s| s.delete_tomb_scans += 1);
            pgrc_front_seams::tomb_probe_many::call(mcx, &h.rel, &todo, &dirty, &mut |r, f| {
                if !f.is_lock() {
                    if let Ok(i) = ids.binary_search(&r) {
                        hits[i] = Some(f);
                    }
                }
            })?;
        }
        let first = hits[ids.binary_search(&id).expect("the asked ID is in its window")];
        self.read = ids.into_iter().zip(hits).collect();
        Ok(first)
    }
}

/// A TID past the heap, an unused or redirect item names no row: not dead.
pub(crate) fn segment_dead<'mcx>(mcx: Mcx<'mcx>, seg: &Relation<'mcx>, tid: &ItemPointerData) -> PgResult<Option<TransactionId>> {
    let blk = ItemPointerGetBlockNumberNoCheck(tid);
    if blk >= bufmgr::RelationGetNumberOfBlocksInFork(seg, ForkNumber::MAIN_FORKNUM)? {
        return Ok(None);
    }
    let pin = BufferPin::adopt(bufmgr_seams::read_buffer::call(seg, blk)?).expect("ReadBuffer returned InvalidBuffer");
    if let Err(e) = bufmgr_seams::lock_buffer::call(pin.buffer(), BUFFER_LOCK_SHARE) {
        pin.release();
        return Err(e);
    }
    let out = segment_item_dead(mcx, seg, &pin, tid);
    let unlock = bufmgr_seams::lock_buffer::call(pin.buffer(), BUFFER_LOCK_UNLOCK);
    pin.release();
    let out = out?;
    unlock?;
    Ok(out)
}

fn segment_item_dead<'mcx>(mcx: Mcx<'mcx>, seg: &Relation<'mcx>, pin: &BufferPin, tid: &ItemPointerData) -> PgResult<Option<TransactionId>> {
    let page = pin.page();
    let off = ItemPointerGetOffsetNumberNoCheck(tid);
    if off < FirstOffsetNumber || off > page.max_offset_number() {
        return Ok(None);
    }
    let lp = page.item_id(off);
    if lp.is_dead() {
        return Ok(Some(InvalidTransactionId));
    }
    if !lp.is_normal() {
        return Ok(None);
    }
    let mut snap = SnapshotData::sentinel(mcx, SnapshotType::SNAPSHOT_NON_VACUUMABLE);
    snap.vistest = procarray::GlobalVisTestFor(seg);
    if heapam::heap_hot_search_buffer(*tid, seg, pin, &snap, false, true)?.found {
        return Ok(None);
    }
    let (ptr, _) = page.item_raw(lp);
    // SAFETY: a normal line pointer on a page this backend holds pinned and share-locked.
    let htup = unsafe { &*ptr.cast::<HeapTupleHeaderData>() };
    let mut horizon = InvalidTransactionId;
    heapam::HeapTupleHeaderAdvanceConflictHorizon(htup, &mut horizon)?;
    Ok(Some(horizon))
}

fn later(a: TransactionId, b: TransactionId) -> TransactionId {
    if !TransactionIdIsValid(a) || (TransactionIdIsValid(b) && TransactionIdPrecedes(a, b)) { b } else { a }
}

/// A full xid's 32-bit horizon: none for 0, or for one 2^31 or more xids back (every snapshot that can run is newer).
fn xid_of_full(full: u64) -> PgResult<TransactionId> {
    if full == 0 {
        return Ok(InvalidTransactionId);
    }
    let now = varsup::ReadNextFullTransactionId()?.value;
    Ok(if now.saturating_sub(full) >= 1 << 31 { InvalidTransactionId } else { full as TransactionId })
}

/// A recent 32-bit xid's full xid (0 for none): the epoch of the next xid, or the one before for an xid past it.
fn full_of_xid(xid: TransactionId) -> PgResult<u64> {
    if !TransactionIdIsValid(xid) {
        return Ok(0);
    }
    let next = varsup::ReadNextFullTransactionId()?.value;
    let full = (next & !0xffff_ffff) | u64::from(xid);
    Ok(if full > next { full.saturating_sub(1 << 32) } else { full })
}

fn id_at(tid: &ItemPointerData) -> Option<u64> {
    crate::ixfetch::id_at(tid)
}

fn tid_at(id: u64) -> ItemPointerData {
    let (block, off) = tid_of(id);
    let mut tid = ItemPointerData::default();
    ItemPointerSet(&mut tid, block, off);
    tid
}

/// The standby conflict record ahead of a removal (`xl_btree_reuse_page` on the table's first index, a pure conflict
/// point, as a fold's reset logs it): standby snapshots at or before `horizon` (a full xid) are cancelled.
fn log_conflict<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, horizon: u64) -> PgResult<()> {
    if horizon == 0 || !rel.is_permanent() || !transam_xlog_seams::xlog_standby_info_active::call() {
        return Ok(());
    }
    let indexes = relcache_seams::relation_get_index_list::call(mcx, rel.rd_id)?;
    let Some(&idx) = indexes.first() else { return Ok(()) };
    let index = relation_seams::relation_open::call(mcx, idx, NoLock)?;
    let body = crate::compile::reuse_page(index.rd_locator.get(), horizon);
    index.close(NoLock)?;
    xloginsert_seams::xlog_insert_record::call(types_core::RmgrIds::RM_BTREE_ID as u8, types_nbtree::XLOG_BTREE_REUSE_PAGE, 0, &[&body], &[]).map(drop)
}

pub fn vacuum_dead<'mcx>(
    mcx: Mcx<'mcx>,
    parent: &RelationData<'mcx>,
    scan: &mut dyn FnMut(&mut dyn FnMut(&ItemPointerData) -> PgResult<()>) -> PgResult<()>,
) -> PgResult<Vec<ItemPointerData>> {
    let rel = table::table_open(mcx, parent.rd_id, NoLock)?;
    let out = vacuum_dead_in(mcx, &rel, scan);
    rel.close(NoLock)?;
    out
}

fn vacuum_dead_in<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    scan: &mut dyn FnMut(&mut dyn FnMut(&ItemPointerData) -> PgResult<()>) -> PgResult<()>,
) -> PgResult<Vec<ItemPointerData>> {
    let mut ids = Vec::new();
    {
        let pre = Check::new(mcx, rel, Mode::Collect)?;
        let out = scan(&mut |tid| {
            if let Some(id) = id_at(tid) {
                if pre.may_be_dead(id)? {
                    ids.push(id);
                }
            }
            Ok(())
        });
        pre.end()?;
        out?;
    }
    ids.sort_unstable();
    ids.dedup();
    let mut chk = Check::new(mcx, rel, Mode::Vacuum)?;
    let (mut dead, mut fold, mut seg) = (Vec::new(), false, InvalidTransactionId);
    let judged = (|| -> PgResult<()> {
        for (k, &id) in ids.iter().enumerate() {
            if k % 4096 == 4095 {
                postgres_seams::check_for_interrupts::call()?;
            }
            match chk.dead(id, false)? {
                Some(Why::Fold) => fold = true,
                Some(Why::Segment(h)) => seg = later(seg, h),
                Some(Why::Deleter(_)) | None => continue,
            }
            dead.push(tid_at(id));
        }
        Ok(())
    })();
    let h_fold = chk.h_fold;
    let end = chk.end();
    judged?;
    end?;
    if !dead.is_empty() {
        let horizon = full_of_xid(seg)?.max(if fold { h_fold } else { 0 });
        log_conflict(mcx, rel, horizon)?;
    }
    crate::ixfetch::stats(|s| {
        s.vacuum_candidates += ids.len() as u64;
        s.vacuum_dead += dead.len() as u64;
    });
    Ok(dead)
}

/// An `LP_DEAD` item no rule confirms (a kill hint the table cannot vouch for now) is kept by btree
/// (unmarked), and an AM that deletes its killed items regardless (hash, gist: `index_compute_xid_horizon_for_tuples`)
/// gets the newest horizon for it. A bottom-up pass stops at the free space it asked for, after
/// [`BOTTOMUP_MAX_CHECKS`] TIDs, or after a [`BOTTOMUP_GROUP`] of the TIDs that need a tombstone read that freed
/// nothing, and keeps those it checked (`ndeltids`), as heap's.
pub fn index_delete_tuples<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, delstate: &mut TM_IndexDeleteOp<'mcx>) -> PgResult<TransactionId> {
    let n = delstate.ndeltids as usize;
    if n == 0 {
        return Ok(InvalidTransactionId);
    }
    let mut chk = Check::new(mcx, rel, Mode::Delete)?;
    let out = (|| -> PgResult<TransactionId> {
        let parts = match delstate.bottomup {
            true => bottomup_order(&chk, delstate, n)?,
            false => {
                delstate.deltids[..n].sort_unstable_by_key(|d| (ItemPointerGetBlockNumberNoCheck(&d.tid), ItemPointerGetOffsetNumberNoCheck(&d.tid)));
                n
            }
        };
        let asked = if delstate.bottomup { n.min(BOTTOMUP_MAX_CHECKS) } else { n };
        let mut ahead_to = if delstate.bottomup { asked.min(parts + BOTTOMUP_GROUP) } else { asked };
        chk.ahead = delstate.deltids[..ahead_to].iter().filter_map(|d| id_at(&d.tid)).collect();
        chk.ahead.dedup();
        let h_fold = xid_of_full(chk.h_fold)?;
        let (mut horizon, mut freed, mut done, mut deletable, mut at_group) = (InvalidTransactionId, 0i32, n, 0u64, 0u64);
        for i in 0..n {
            if delstate.bottomup && i >= parts && (i - parts) % BOTTOMUP_GROUP == 0 {
                if i > parts && deletable == at_group {
                    done = i;
                    break;
                }
                at_group = deletable;
                let to = asked.min(i + BOTTOMUP_GROUP);
                if to > ahead_to {
                    chk.ahead.extend(delstate.deltids[ahead_to..to].iter().filter_map(|d| id_at(&d.tid)));
                    ahead_to = to;
                }
            }
            let d = delstate.deltids[i];
            let why = match id_at(&d.tid) {
                Some(id) => chk.dead(id, true)?,
                None => None,
            };
            let st = &mut delstate.status[d.id as usize];
            match why {
                Some(w) => {
                    st.knowndeletable = true;
                    deletable += 1;
                    horizon = later(horizon, match w {
                        Why::Fold => h_fold,
                        Why::Deleter(x) | Why::Segment(x) => x,
                    });
                    if delstate.bottomup {
                        freed += i32::from(st.freespace);
                    }
                }
                None if st.knowndeletable => {
                    st.knowndeletable = false;
                    let next = varsup::ReadNextFullTransactionId()?.value;
                    horizon = later(horizon, xid_of_full(next.saturating_sub(1))?);
                }
                None => {}
            }
            if delstate.bottomup && (freed >= delstate.bottomupfreespace || i + 1 >= BOTTOMUP_MAX_CHECKS) {
                done = i + 1;
                break;
            }
        }
        if delstate.bottomup {
            delstate.ndeltids = done as i32;
        }
        crate::ixfetch::stats(|s| {
            s.delete_calls += 1;
            s.delete_checked += done as u64;
            s.delete_deletable += deletable;
            s.delete_part_checked += done.saturating_sub(parts) as u64;
        });
        Ok(horizon)
    })();
    let end = chk.end();
    let h = out?;
    end?;
    Ok(h)
}

/// A bottom-up pass's judging order, heap's `bottomup_sort_and_shrink` in pgrc terms: first the TIDs no tombstone read
/// judges (no home, a DV bit, a segment row), then the part rows that need one, then a TID that names no ID; within
/// each, nbtree's promising TIDs, then the newest IDs (a rolled-back statement's versions). Only the order the budget is
/// spent in changes, never what is deletable. Returns how many TIDs come before the part rows.
fn bottomup_order(chk: &Check<'_>, delstate: &mut TM_IndexDeleteOp<'_>, n: usize) -> PgResult<usize> {
    let mut keyed: Vec<((u8, bool, std::cmp::Reverse<u64>), TM_IndexDelete)> = Vec::new();
    keyed.try_reserve(n).map_err(|_| chk.mcx.oom(n * core::mem::size_of::<((u8, bool, std::cmp::Reverse<u64>), TM_IndexDelete)>()))?;
    let mut cheap = 0;
    for &d in &delstate.deltids[..n] {
        let (class, id) = match id_at(&d.tid) {
            Some(id) => (u8::from(matches!(chk.home(id)?, Home::Part { dv: false })), id),
            None => (2, 0),
        };
        cheap += usize::from(class == 0);
        keyed.push(((class, !delstate.status[d.id as usize].promising, std::cmp::Reverse(id)), d));
    }
    keyed.sort_unstable_by_key(|&(k, _)| k);
    for (slot, (_, d)) in delstate.deltids[..n].iter_mut().zip(keyed) {
        *slot = d;
    }
    Ok(cheap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_later_horizon_wins_and_an_invalid_one_is_none() {
        assert_eq!(later(InvalidTransactionId, 700), 700);
        assert_eq!(later(700, InvalidTransactionId), 700);
        assert_eq!(later(700, 900), 900);
        assert_eq!(later(900, 700), 900);
        // across the wrap: 2^32 - 10 precedes 5
        assert_eq!(later(u32::MAX - 10, 5), 5);
    }

    #[test]
    fn a_tid_names_the_id_it_was_made_from() {
        for id in [pgrc_format::rowid::ROWID_FIRST, 1_000_003, 6_000_000_123] {
            assert_eq!(id_at(&tid_at(id)), Some(id));
        }
    }
}
