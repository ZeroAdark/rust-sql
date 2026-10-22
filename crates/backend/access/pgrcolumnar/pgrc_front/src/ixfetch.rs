//! A pgrc row fetched by its ID under any snapshot: an index entry's TID, a TID scan's, `WHERE CURRENT OF`'s
//! A row's TID is `tid_of` its stable ID, so a fetch decides visibility in order:
//! 1. the ID's home in the reader's generation ([`position::home_in`]: the snapshot's view for MVCC, the newest
//!    generation otherwise). No home, no row.
//! 2. a segment home is the heap visibility of the segment tuple under the snapshot;
//! 3. a part home's DV bit (horizon-passed deletes);
//! 4. the deletion the reader sees: the statement view's mask (MVCC, built once per fetch state through
//!    `pgrc_front_seams::statement_deletes`), else a tombstone probe per hit (Dirty, Self, non-vacuumable, a
//!    one-off fetch, a view the provider declines, or the `pgrust.pgrc_ixfetch_probe` knob).
//!
//! No HOT chains, so `call_again` is always false; no key re-check (an ID never changes its key and is never reused).
//! `all_dead` (the kill hint) is a row the reader does not see that is dead to everyone.
//! Never in recovery: a standby does not judge with the primary's horizon, and indexam ignores the hint there anyway.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use mcx::Mcx;
use pgrc_am::catalog::{Generation, RowMap};
use pgrc_am::rowcache;
use pgrc_am::scan::DeletionFilter;
use pgrc_format::class::ColSchema;
use pgrc_format::geom::GRANULE_ROWS;
use pgrc_format::manifest::{SegState, SegmentRecord};
use pgrc_format::rowid::{id_of, seg_local, tid_of, ROWID_FIRST, SUCC_MOVED, SUCC_NONE};
use pgrc_front_seams::{TombHit, TombState};
use tableam_vocab::Snapshot;
use types_core::{CommandId, InvalidTransactionId, Oid, TransactionId};
use types_error::{PgError, PgResult};
use types_rel::{AccessShareLock, Relation};
use types_slot::SlotData;
use types_snapshot::{SnapshotData, SnapshotType};
use types_tuple::tupdesc::FormData_pg_attribute;
use types_tuple::{ItemPointerData, ItemPointerGetBlockNumberNoCheck, ItemPointerGetOffsetNumberNoCheck, ItemPointerSet};

use crate::position::{self, Located};

const DV_GRANULE_WORDS: usize = GRANULE_ROWS as usize / 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Mvcc,
    SelfSnapshot,
    Dirty,
    Any,
    NonVacuumable,
}

pub fn kind_of(snapshot: &Snapshot<'_>) -> PgResult<Kind> {
    Ok(match snapshot.as_deref().map(|s| s.snapshot_type) {
        None | Some(SnapshotType::SNAPSHOT_ANY) => Kind::Any,
        Some(SnapshotType::SNAPSHOT_MVCC) => Kind::Mvcc,
        Some(SnapshotType::SNAPSHOT_SELF) => Kind::SelfSnapshot,
        Some(SnapshotType::SNAPSHOT_DIRTY) => Kind::Dirty,
        Some(SnapshotType::SNAPSHOT_NON_VACUUMABLE) => Kind::NonVacuumable,
        Some(t) => {
            return Err(Box::new(
                PgError::error(format!("pgrcolumnar: a row fetch under snapshot type {t:?} is not supported")).with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED),
            ))
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tomb {
    Clear,
    Deleted { removable: bool },
    Busy { xmax: TransactionId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Verdict {
    pub(crate) visible: bool,
    pub(crate) all_dead: bool,
    pub(crate) busy: Option<TransactionId>,
}

impl Verdict {
    const GONE: Verdict = Verdict { visible: false, all_dead: false, busy: None };
    const LIVE: Verdict = Verdict { visible: true, all_dead: false, busy: None };
}

pub(crate) trait Facts {
    fn segment_visible(&mut self, relid: Oid, tid: &ItemPointerData) -> PgResult<bool>;
    fn dv(&self, part: usize, granule: u32, row: u32) -> bool;
    fn tomb(&mut self, at: &Located<'_>) -> PgResult<Tomb>;
}

pub(crate) fn decide(kind: Kind, at: Option<&Located<'_>>, facts: &mut dyn Facts) -> PgResult<Verdict> {
    let Some(at) = at else { return Ok(Verdict::GONE) };
    match at {
        Located::Segment { relid, tid, .. } => {
            if !facts.segment_visible(*relid, tid)? {
                return Ok(Verdict::GONE);
            }
        }
        Located::Part { part, granule, row, .. } => {
            if facts.dv(*part, *granule, *row) {
                return Ok(Verdict { visible: false, all_dead: true, busy: None });
            }
        }
    }
    if kind == Kind::Any {
        return Ok(Verdict::LIVE);
    }
    Ok(match facts.tomb(at)? {
        Tomb::Clear => Verdict::LIVE,
        Tomb::Deleted { removable } if kind == Kind::NonVacuumable && !removable => Verdict::LIVE,
        Tomb::Deleted { removable } => Verdict { visible: false, all_dead: removable, busy: None },
        Tomb::Busy { xmax } if kind == Kind::Dirty => Verdict { visible: true, all_dead: false, busy: Some(xmax) },
        Tomb::Busy { .. } => Verdict::LIVE,
    })
}

pub(crate) fn tomb_of_hit(hit: Option<&TombHit>, removable: &mut dyn FnMut(TransactionId) -> PgResult<bool>) -> PgResult<Tomb> {
    let Some(hit) = hit else { return Ok(Tomb::Clear) };
    // a LOCK row deletes nothing, as a lock-only xmax hides nothing from HeapTupleSatisfiesDirty (and sets no xmax); a
    // deleter of a locked row inserts its own tombstone, which outranks the LOCK row
    if hit.is_lock() {
        return Ok(Tomb::Clear);
    }
    Ok(match hit.state {
        TombState::InProgress => Tomb::Busy { xmax: hit.xmin },
        TombState::Own => Tomb::Deleted { removable: false },
        TombState::Committed => Tomb::Deleted { removable: removable(hit.xmin)? },
    })
}


struct Memo<'mcx> {
    snap: Snapshot<'mcx>,
    curcid: CommandId,
    gen: Option<Arc<Generation>>,
    newer: Vec<SegmentRecord>,
    path: crate::pathcache::Path,
}

enum Mask {
    Unbuilt,
    Built(Box<dyn DeletionFilter>),
    Clear,
    Probe,
}

struct Io<'mcx> {
    /// Each segment heap opened, with the blocks it was last seen to have (D4735: `segment_has`).
    segs: Vec<(Oid, Relation<'mcx>, u32)>,
    tombs: Option<Vec<crate::tombs::Opened<'mcx>>>,
    mask: Mask,
    last_mask: Option<(usize, u32, Option<Box<[u64; DV_GRANULE_WORDS]>>)>,
}

impl<'mcx> Io<'mcx> {
    fn segment(&mut self, mcx: Mcx<'mcx>, relid: Oid) -> PgResult<&Relation<'mcx>> {
        let i = match self.segs.iter().position(|(r, ..)| *r == relid) {
            Some(i) => i,
            None => {
                self.segs.push((relid, table::table_open(mcx, relid, AccessShareLock)?, 0));
                self.segs.len() - 1
            }
        };
        Ok(&self.segs[i].1)
    }

    /// Whether segment `relid`'s heap has block `blk` (ixclean.rs `segment_dead`'s check): an ID of a reserved extent past
    /// the rows its segment wrote names no tuple (D4735: a TABLESAMPLE or TID scan asks such IDs). The count is asked again
    /// only past the last one seen: the heap only grows under this fetch's AccessShareLock.
    fn segment_has(&mut self, mcx: Mcx<'mcx>, relid: Oid, blk: u32) -> PgResult<bool> {
        self.segment(mcx, relid)?;
        let Some(s) = self.segs.iter_mut().find(|s| s.0 == relid) else { return Ok(false) };
        if blk >= s.2 {
            s.2 = bufmgr::RelationGetNumberOfBlocksInFork(&s.1, types_core::ForkNumber::MAIN_FORKNUM)?;
        }
        Ok(blk < s.2)
    }

    fn tomb_heaps(&mut self, mcx: Mcx<'mcx>, parent: Oid) -> PgResult<&[crate::tombs::Opened<'mcx>]> {
        if self.tombs.is_none() {
            self.tombs = Some(crate::tombs::open_existing_cached(mcx, parent)?);
        }
        Ok(self.tombs.as_deref().expect("just opened"))
    }
}

pub struct PgrcIndexFetch<'mcx> {
    rel: Relation<'mcx>,
    needed: Option<Box<[bool]>>,
    schemas: Option<Rc<SchemaSet>>,
    memo: Option<Memo<'mcx>>,
    io: Io<'mcx>,
    one_off: bool,
    touches: rowcache::Touches,
}

pub fn begin<'mcx>(rel: &Relation<'mcx>) -> PgrcIndexFetch<'mcx> {
    PgrcIndexFetch {
        rel: rel.alias(),
        needed: None,
        schemas: None,
        memo: None,
        io: Io { segs: Vec::new(), tombs: None, mask: Mask::Unbuilt, last_mask: None },
        one_off: false,
        touches: rowcache::Touches::new(),
    }
}

impl<'mcx> PgrcIndexFetch<'mcx> {
    pub fn rel(&self) -> &Relation<'mcx> {
        &self.rel
    }

    pub fn set_needed_attrs(&mut self, needed: Option<Box<[bool]>>) {
        self.needed = needed;
    }

    pub fn reset(&mut self) {}

    pub fn end(mut self) {
        if let Some(t) = self.io.tombs.take() {
            let _ = crate::tombs::close_all(t);
        }
    }

    /// The memo for `snapshot` (MVCC: kept while the snapshot and its command are; any other kind reads the newest
    /// generation every fetch, as a dirty reader must see writers that published since).
    fn memo_for(&mut self, kind: Kind, snapshot: &Snapshot<'mcx>) -> PgResult<()> {
        let curcid = snapshot.as_deref().map_or(0, |s| s.curcid.get());
        if kind == Kind::Mvcc {
            if let Some(m) = &self.memo {
                let same = match (&m.snap, snapshot) {
                    (Some(a), Some(b)) => Rc::ptr_eq(a, b),
                    _ => false,
                };
                if same && m.curcid == curcid {
                    return Ok(());
                }
            }
        }
        let (gen, newer) = match (kind, snapshot.as_deref()) {
            (Kind::Mvcc, Some(s)) => {
                let gen = crate::ring::view_under(&self.rel, s)?;
                let newer = newer_than(&self.rel, gen.as_deref())?;
                (gen, newer)
            }
            _ => (crate::ring::newest(&self.rel)?, Vec::new()),
        };
        let path = crate::pathcache::path(&self.rel)?;
        self.memo = Some(Memo { snap: snapshot.clone(), curcid, gen, newer, path });
        self.io.mask = Mask::Unbuilt;
        self.io.last_mask = None;
        Ok(())
    }
}

const NAIVE_HEAD_POINT: &str = "pgrc-ixfetch-naive-head";
const HEAD_CHECKED_POINT: &str = "pgrc-ixfetch-head-checked";

/// The head is read after the view, as that walk runs after it: a publish since is another transaction's, after the
/// snapshot.
fn newer_than(rel: &Relation<'_>, view: Option<&Generation>) -> PgResult<Vec<SegmentRecord>> {
    if let Some(v) = view {
        if injection_point::is_attached(NAIVE_HEAD_POINT) || (!v.composed() && crate::ring::head_is(rel, v.gen)) {
            injection_point::injection_point(HEAD_CHECKED_POINT)?;
            return Ok(Vec::new());
        }
    }
    crate::ring::newer_segments(rel, view)
}


pub(crate) struct SchemaSet {
    pub(crate) schemas: Vec<ColSchema>,
    pub(crate) fp: u64,
}

/// What `pgrc_am::schema::derive_one` reads of an attribute; the type and collation facts it looks up are fixed per OID in a database.
#[derive(Clone, Copy, PartialEq, Eq)]
struct AttSig {
    attnum: i16,
    typid: Oid,
    typmod: i32,
    len: i16,
    byval: bool,
    align: i8,
    collation: Oid,
    dropped: bool,
}

impl AttSig {
    fn of(a: &FormData_pg_attribute) -> AttSig {
        AttSig { attnum: a.attnum, typid: a.atttypid, typmod: a.atttypmod, len: a.attlen, byval: a.attbyval, align: a.attalign, collation: a.attcollation, dropped: a.attisdropped }
    }
}

const SCHEMA_STALE_POINT: &str = "pgrc-schema-stale";
const SCHEMA_SLOTS: usize = 16;

#[derive(Default)]
struct SchemaCache {
    entries: Vec<((Oid, Oid), Vec<AttSig>, Rc<SchemaSet>)>,
    clock: usize,
}

impl SchemaCache {
    fn hit(&self, key: (Oid, Oid), atts: &[FormData_pg_attribute], stale: bool) -> Option<Rc<SchemaSet>> {
        let (_, sig, set) = self.entries.iter().find(|e| e.0 == key)?;
        let same = sig.len() == atts.len() && sig.iter().zip(atts).all(|(s, a)| *s == AttSig::of(a));
        (same || stale).then(|| Rc::clone(set))
    }

    fn file(&mut self, key: (Oid, Oid), atts: &[FormData_pg_attribute], set: &Rc<SchemaSet>) {
        let fresh = (key, atts.iter().map(AttSig::of).collect(), Rc::clone(set)); // alloc: Cold
        match self.entries.iter().position(|e| e.0 == key) {
            Some(i) => self.entries[i] = fresh,
            None if self.entries.len() < SCHEMA_SLOTS => self.entries.push(fresh), // alloc: Cold
            None => {
                let i = self.clock;
                self.entries[i] = fresh;
                self.clock = (i + 1) % SCHEMA_SLOTS;
            }
        }
    }
}

thread_local! {
    static SCHEMAS: RefCell<SchemaCache> = RefCell::new(SchemaCache::default());
}

fn shared_schemas(rel: &Relation<'_>) -> PgResult<Rc<SchemaSet>> {
    let atts: &[FormData_pg_attribute] = &rel.rd_att.attrs;
    let key = (init_small::globals::MyDatabaseId(), rel.rd_id);
    let stale = injection_point::is_attached(SCHEMA_STALE_POINT);
    if let Some(s) = SCHEMAS.with(|c| c.borrow().hit(key, atts, stale)) {
        stats(|s| s.schema_hits += 1);
        return Ok(s);
    }
    stats(|s| s.schema_derived += 1);
    let schemas = pgrc_am::schema::col_schemas(rel)?;
    let fp = pgrc_format::ident::schema_fingerprint(&schemas);
    let set = Rc::new(SchemaSet { schemas, fp });
    SCHEMAS.with(|c| c.borrow_mut().file(key, atts, &set));
    Ok(set)
}

const PROBE_GUC: &str = "pgrust.pgrc_ixfetch_probe";

const MASK_NAIVE_POINT: &str = "pgrc-ixfetch-mask-naive";

/// Whether the slots a view reads hold no block, `blocks` the reader's heaps among them (slot, blocks). A
/// slot 0 without a heap holds nothing (named before its first tombstone makes it, [`crate::tombs::open_read`]); any
/// other slot without one is not known empty: the full build waits for its heap.
pub(crate) fn heaps_empty(slots: &[u8], blocks: &[(u8, u32)]) -> bool {
    slots.iter().all(|&s| match blocks.iter().find(|b| b.0 == s) {
        Some(&(_, n)) => n == 0,
        None => s == 0,
    })
}

thread_local! {
    static PROBE: std::cell::Cell<Option<(u64, bool)>> = const { std::cell::Cell::new(None) };
}

fn probe_arm_forced() -> bool {
    PROBE.with(|p| crate::maint::per_guc_change(p, || crate::maint::setting(PROBE_GUC) == Some(1)))
}

/// How many leading attributes of a segment row a fetch deforms: through the last one `needed` names (one
/// past the end of `needed` counts as named, as `rowcache::fetch_cols` reads it), at most `natts`.
pub(crate) fn deform_width(needed: Option<&[bool]>, natts: usize) -> usize {
    match needed {
        Some(n) if n.len() >= natts => n[..natts].iter().rposition(|&b| b).map_or(0, |i| i + 1),
        _ => natts,
    }
}

fn deform_segment_row<'s>(mcx: Mcx<'s>, seg: &Relation<'_>, tup: &types_tuple::HeapTupleData<'_>, needed: Option<&[bool]>, slot: &mut SlotData<'s>) -> PgResult<()> {
    exectuples::exec_clear_tuple(slot, mcx);
    let base = slot.base_mut();
    let natts = seg.rd_att.natts as usize;
    if natts > base.tts_values.len() {
        return Err(Box::new(PgError::error(format!("pgrcolumnar: segment {} has more columns than its table", seg.rd_id))));
    }
    let want = deform_width(needed, natts);
    types_tuple::heap_deform_tuple_prefix(tup, &seg.rd_att, &mut base.tts_values[..natts], &mut base.tts_isnull[..natts], want);
    base.tts_isnull[want..].fill(true);
    base.tts_nvalid = base.tts_values.len() as types_core::AttrNumber;
    base.mark_not_empty();
    exectuples::exec_materialize_slot(slot, mcx)
}

struct Live<'a, 'mcx> {
    mcx: Mcx<'mcx>,
    rel: &'a Relation<'mcx>,
    memo: &'a Memo<'mcx>,
    io: &'a mut Io<'mcx>,
    kind: Kind,
    snapshot: &'a Snapshot<'mcx>,
    slot: Option<&'a mut SlotData<'mcx>>,
    needed: Option<&'a [bool]>,
    one_off: bool,
    want_dead: bool,
}

impl<'mcx> Facts for Live<'_, 'mcx> {
    fn segment_visible(&mut self, relid: Oid, tid: &ItemPointerData) -> PgResult<bool> {
        stats(|s| s.segment += 1);
        let mcx = self.mcx;
        let any;
        let snap = match self.snapshot.as_deref() {
            Some(s) => s,
            None => {
                any = SnapshotData::sentinel(mcx, SnapshotType::SNAPSHOT_ANY);
                &any
            }
        };
        if !self.io.segment_has(mcx, relid, ItemPointerGetBlockNumberNoCheck(tid))? {
            return Ok(false);
        }
        let seg = self.io.segment(mcx, relid)?;
        let res = heapam::heap_fetch(seg, snap, *tid, false)?;
        let out = match res.tuple() {
            Some(tup) if res.found => {
                if let Some(slot) = self.slot.as_deref_mut() {
                    deform_segment_row(mcx, seg, &tup, self.needed, slot)?;
                }
                true
            }
            _ => false,
        };
        drop(res);
        Ok(out)
    }

    fn dv(&self, part: usize, granule: u32, row: u32) -> bool {
        let mask = self.memo.gen.as_ref().and_then(|g| g.parts.get(part)).and_then(|p| p.deletes.as_ref()).and_then(|d| d.granule_mask(granule));
        mask.is_some_and(|m| m[row as usize / 64] >> (row % 64) & 1 == 1)
    }

    fn tomb(&mut self, at: &Located<'_>) -> PgResult<Tomb> {
        if self.kind == Kind::Mvcc {
            if let Some(deleted) = self.masked(at)? {
                // a kill hint asks whether a delete the mask reports is dead to everyone: the probe judges its
                // deleter (it finds the tombstone the mask did, under the same snapshot)
                if !(deleted && self.want_dead) {
                    return Ok(if deleted { Tomb::Deleted { removable: false } } else { Tomb::Clear });
                }
            }
        }
        self.probe(position::rowident(at)?)
    }
}

impl<'mcx> Live<'_, 'mcx> {
    fn masked(&mut self, at: &Located<'_>) -> PgResult<Option<bool>> {
        if matches!(self.io.mask, Mask::Unbuilt) {
            self.io.mask = self.build_mask()?;
        }
        let f = match &self.io.mask {
            Mask::Built(f) => f,
            Mask::Clear => return Ok(Some(false)),
            Mask::Unbuilt | Mask::Probe => return Ok(None),
        };
        Ok(Some(match at {
            Located::Segment { slot, tid, .. } => {
                let local = seg_local(ItemPointerGetBlockNumberNoCheck(tid), ItemPointerGetOffsetNumberNoCheck(tid));
                local.is_some_and(|l| f.segment_positions(*slot).binary_search(&l).is_ok())
            }
            Located::Part { part, granule, row, .. } => {
                if !self.io.last_mask.as_ref().is_some_and(|(p, g, _)| p == part && g == granule) {
                    let mut m = None;
                    f.granule_into(*part, *granule, &mut m);
                    self.io.last_mask = Some((*part, *granule, m));
                }
                let m = self.io.last_mask.as_ref().and_then(|(_, _, m)| m.as_ref());
                m.is_some_and(|m| m[*row as usize / 64] >> (*row % 64) & 1 == 1)
            }
        }))
    }

    fn build_mask(&mut self) -> PgResult<Mask> {
        let snapshot = self.snapshot;
        let Some(snap) = snapshot.as_deref() else { return Ok(Mask::Probe) };
        if self.one_off || probe_arm_forced() || !pgrc_front_seams::statement_deletes::is_installed() {
            return Ok(Mask::Probe);
        }
        let m = self.memo;
        if self.tombstone_free()? {
            stats(|s| s.mask_clear += 1);
            return Ok(Mask::Clear);
        }
        let segs: Vec<SegmentRecord> = m.gen.iter().flat_map(|g| g.segments.iter()).filter(|r| r.state != SegState::Sealed).chain(&m.newer).copied().collect();
        stats(|s| s.mask_builds += 1);
        Ok(match pgrc_front_seams::statement_deletes::call(self.mcx, self.rel, m.gen.as_ref(), &segs, snap)? {
            Some(f) => Mask::Built(f),
            None => Mask::Probe,
        })
    }

    /// A tombstone or LOCK row any snapshot can see, its own included, was inserted (its heap extended) before its
    /// inserter committed, and a heap is reset to no blocks only after its tombstones are folded into DVs of a
    /// generation every running snapshot sees ([`crate::tombs::blocks`], front-bearing's test; the colcache view skips
    /// such a heap too). The counts are read here, inside a fetch, so after the statement's snapshot was taken: a
    /// tombstone the snapshot sees is in them.
    fn tombstone_free(&mut self) -> PgResult<bool> {
        if injection_point::is_attached(MASK_NAIVE_POINT) {
            return Ok(true);
        }
        let view = crate::tombs::state(self.memo.gen.as_deref())?;
        let newest = match self.memo.gen.as_deref() {
            Some(g) if !g.composed() && crate::ring::head_is(self.rel, g.gen) => view,
            _ => crate::tombs::newest_state(self.rel)?,
        };
        let slots = crate::tombs::read_slots(&view, &newest);
        let mcx = self.mcx;
        let parent = self.rel.rd_id;
        let mut blocks = ::mcx::stdvec::try_vec(slots.len(), "pgrc index fetch")?;
        for h in self.io.tomb_heaps(mcx, parent)? {
            if slots.contains(&h.slot) {
                blocks.push((h.slot, bufmgr::RelationGetNumberOfBlocksInFork(&h.rel, types_core::ForkNumber::MAIN_FORKNUM)?));
            }
        }
        Ok(heaps_empty(&slots, &blocks))
    }

    fn probe(&mut self, id: u64) -> PgResult<Tomb> {
        stats(|s| s.probes += 1);
        let mcx = self.mcx;
        let parent = self.rel.rd_id;
        let kind = self.kind;
        let snapshot = self.snapshot;
        let mut hit = None;
        for h in self.io.tomb_heaps(mcx, parent)? {
            // MVCC and Self probe under the reader's snapshot (the tombstones it sees); every other kind under a fresh
            // SnapshotDirty per heap: a dirty probe writes its tuple's xids into the snapshot it is given, which is
            // not the caller's to overwrite
            let snap: Snapshot<'mcx> = match kind {
                Kind::Mvcc | Kind::SelfSnapshot => snapshot.clone(),
                _ => Some(Rc::new(SnapshotData::sentinel(mcx, SnapshotType::SNAPSHOT_DIRTY))),
            };
            match pgrc_front_seams::tomb_probe::call(mcx, &h.rel, id, &snap)? {
                // a tombstone in any heap outranks a LOCK row in another (stageb::probe_all)
                Some(f) if !f.is_lock() => {
                    hit = Some(f);
                    break;
                }
                Some(f) => {
                    hit.get_or_insert(f);
                }
                None => {}
            }
        }
        let rel = self.rel;
        // SnapshotNonVacuumable's visibility is the horizon's: judged whether or not `all_dead` is asked
        let want = self.want_dead || kind == Kind::NonVacuumable;
        tomb_of_hit(hit.as_ref(), &mut |xid| if want { procarray::GlobalVisTestIsRemovableXid(procarray::GlobalVisTestFor(rel), xid) } else { Ok(false) })
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FetchStats {
    pub fetches: u64,
    pub visible: u64,
    pub segment: u64,
    pub part: u64,
    pub no_home: u64,
    pub mask_builds: u64,
    pub mask_clear: u64,
    pub probes: u64,
    pub kill_hints: u64,
    pub delete_calls: u64,
    pub delete_checked: u64,
    pub delete_deletable: u64,
    pub vacuum_candidates: u64,
    pub vacuum_dead: u64,
    pub schema_hits: u64,
    pub schema_derived: u64,
    pub tomb_hits: u64,
    pub tomb_lookups: u64,
    pub tomb_unhooked: u64,
    pub delete_tomb_scans: u64,
    /// RBPROF: the TIDs bottom-up passes judged past the ones no tombstone read judges (their part rows).
    pub delete_part_checked: u64,
}

thread_local! {
    static STATS: std::cell::Cell<FetchStats> = std::cell::Cell::new(FetchStats::default());
}

pub(crate) fn stats(f: impl FnOnce(&mut FetchStats)) {
    STATS.with(|c| {
        let mut s = c.get();
        f(&mut s);
        c.set(s);
    });
}

pub fn fetch_stats() -> FetchStats {
    STATS.with(|c| c.get())
}

pub fn stats_text() -> String {
    let s = fetch_stats();
    let c = rowcache::col_stats();
    format!(
        "fetches={} visible={} segment={} part={} no_home={} mask_builds={} mask_clear={} probes={} col_fetches={} cols_decoded={} cols_cached={} cols_sliced={} kill_hints={} delete_calls={} delete_checked={} delete_deletable={} vacuum_candidates={} vacuum_dead={} schema_hits={} schema_derived={} tomb_hits={} tomb_lookups={} tomb_unhooked={} delete_tomb_scans={} delete_part_checked={}",
        s.fetches,
        s.visible,
        s.segment,
        s.part,
        s.no_home,
        s.mask_builds,
        s.mask_clear,
        s.probes,
        c.fetches,
        c.decoded,
        c.hits,
        c.sliced,
        s.kill_hints,
        s.delete_calls,
        s.delete_checked,
        s.delete_deletable,
        s.vacuum_candidates,
        s.vacuum_dead,
        s.schema_hits,
        s.schema_derived,
        s.tomb_hits,
        s.tomb_lookups,
        s.tomb_unhooked,
        s.delete_tomb_scans,
        s.delete_part_checked
    )
}

fn fetch_id<'mcx>(mcx: Mcx<'mcx>, f: &mut PgrcIndexFetch<'mcx>, id: u64, kind: Kind, snapshot: &Snapshot<'mcx>, mut slot: Option<&mut SlotData<'mcx>>, want_dead: bool) -> PgResult<Verdict> {
    stats(|s| s.fetches += 1);
    f.memo_for(kind, snapshot)?;
    let PgrcIndexFetch { rel, needed, schemas, memo, io, one_off, touches } = f;
    let m = memo.as_ref().expect("memo_for set it");
    let map_of = |p: usize| -> PgResult<Arc<RowMap>> { m.gen.as_ref().expect("a part home has a generation").row_map(p, &m.path.dir) };
    let at = position::home_in(m.gen.as_ref(), &m.newer, &m.path.ext, id, &map_of)?;
    match &at {
        None => stats(|s| s.no_home += 1),
        Some(Located::Part { .. }) => stats(|s| s.part += 1),
        Some(Located::Segment { .. }) => {}
    }
    let mut v = {
        let mut facts = Live { mcx, rel, memo: m, io: &mut *io, kind, snapshot, slot: slot.as_deref_mut(), needed: needed.as_deref(), one_off: *one_off, want_dead };
        decide(kind, at.as_ref(), &mut facts)?
    };
    if want_dead && !v.visible && !v.all_dead {
        if let Some(Located::Segment { relid, tid, .. }) = &at {
            v.all_dead = crate::ixclean::segment_dead(mcx, io.segment(mcx, *relid)?, tid)?.is_some();
        }
    }
    let Some(slot) = slot else { return Ok(v) };
    if !v.visible {
        exectuples::exec_clear_tuple(slot, mcx);
        return Ok(v);
    }
    stats(|s| s.visible += 1);
    if let Some(Located::Part { gen, part, granule, row, .. }) = &at {
        if schemas.is_none() {
            *schemas = Some(shared_schemas(rel)?);
        }
        let set = schemas.as_deref().expect("just read");
        rowcache::fetch_cols(mcx, &gen.parts[*part], &set.schemas, set.fp, &rel.rd_att, *granule, *row, needed.as_deref(), touches, slot)?;
    }
    let (block, off) = tid_of(id);
    let base = slot.base_mut();
    ItemPointerSet(&mut base.tts_tid, block, off);
    base.tts_tableOid = rel.rd_id;
    Ok(v)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hit {
    Segment { relid: Oid, tid: ItemPointerData, id: u64 },
    Part { part: usize, granule: u32, row: u32, id: u64 },
}

impl Hit {
    pub(crate) fn id(&self) -> u64 {
        match self {
            Hit::Segment { id, .. } | Hit::Part { id, .. } => *id,
        }
    }
}

impl<'mcx> PgrcIndexFetch<'mcx> {
    pub(crate) fn stage_id(&mut self, mcx: Mcx<'mcx>, id: u64, kind: Kind, snapshot: &Snapshot<'mcx>) -> PgResult<Option<Hit>> {
        stats(|s| s.fetches += 1);
        self.memo_for(kind, snapshot)?;
        let PgrcIndexFetch { rel, needed, memo, io, one_off, .. } = self;
        let m = memo.as_ref().expect("memo_for set it");
        let map_of = |p: usize| -> PgResult<Arc<RowMap>> { m.gen.as_ref().expect("a part home has a generation").row_map(p, &m.path.dir) };
        let at = position::home_in(m.gen.as_ref(), &m.newer, &m.path.ext, id, &map_of)?;
        match &at {
            None => stats(|s| s.no_home += 1),
            Some(Located::Part { .. }) => stats(|s| s.part += 1),
            Some(Located::Segment { .. }) => {}
        }
        let v = {
            let mut facts = Live { mcx, rel, memo: m, io: &mut *io, kind, snapshot, slot: None, needed: needed.as_deref(), one_off: *one_off, want_dead: false };
            decide(kind, at.as_ref(), &mut facts)?
        };
        if !v.visible {
            return Ok(None);
        }
        stats(|s| s.visible += 1);
        Ok(at.map(|at| match at {
            Located::Segment { relid, tid, id, .. } => Hit::Segment { relid, tid, id },
            Located::Part { part, granule, row, id, .. } => Hit::Part { part, granule, row, id },
        }))
    }

    pub(crate) fn view(&self) -> Option<Arc<Generation>> {
        self.memo.as_ref().and_then(|m| m.gen.clone())
    }

    pub(crate) fn store_hit<'s>(
        &mut self,
        fmcx: Mcx<'mcx>,
        mcx: Mcx<'s>,
        gen: Option<&Arc<Generation>>,
        hit: &Hit,
        snapshot: &Snapshot<'mcx>,
        slot: &mut SlotData<'s>,
    ) -> PgResult<()> {
        match *hit {
            Hit::Part { part, granule, row, .. } => {
                let gen = gen.expect("a part row was staged in a generation");
                if self.schemas.is_none() {
                    self.schemas = Some(shared_schemas(&self.rel)?);
                }
                let set = self.schemas.as_deref().expect("just read");
                rowcache::fetch_cols(mcx, &gen.parts[part], &set.schemas, set.fp, &self.rel.rd_att, granule, row, self.needed.as_deref(), &mut self.touches, slot)?;
            }
            Hit::Segment { relid, tid, id } => {
                let any;
                let snap = match snapshot.as_deref() {
                    Some(s) => s,
                    None => {
                        any = SnapshotData::sentinel(fmcx, SnapshotType::SNAPSHOT_ANY);
                        &any
                    }
                };
                let needed = self.needed.as_deref();
                let seg = self.io.segment(fmcx, relid)?;
                let res = heapam::heap_fetch(seg, snap, tid, false)?;
                match res.tuple() {
                    Some(tup) if res.found => deform_segment_row(mcx, seg, &tup, needed, slot)?,
                    _ => {
                        return Err(Box::new(
                            PgError::error(format!("pgrcolumnar: row {id} of segment {relid} staged by a bitmap scan is no longer visible to it"))
                                .with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR),
                        ))
                    }
                }
                drop(res);
            }
        }
        let (block, off) = tid_of(hit.id());
        let base = slot.base_mut();
        ItemPointerSet(&mut base.tts_tid, block, off);
        base.tts_tableOid = self.rel.rd_id;
        Ok(())
    }
}

pub fn tid_bound(rel: &Relation<'_>) -> PgResult<u64> {
    let dir = crate::pathcache::dir(rel)?;
    Ok(pgrc_am::rowid_alloc::marks_in(rel, &dir)?.1)
}

/// D4735: a sample draws row IDs, so the rows of a part written before row IDs would never be drawn.
pub fn sample_ids_cover(rel: &Relation<'_>) -> PgResult<()> {
    for gen in [crate::ring::statement_view(rel)?, crate::ring::newest(rel)?].into_iter().flatten() {
        if crate::compact::idless_job(&gen).is_some() {
            return Err(Box::new(
                PgError::error(format!("pgrcolumnar: TABLESAMPLE over \"{}\" reads a part written before row IDs", rel.name()))
                    .with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED)
                    .with_hint("Compaction or VACUUM FULL gives the part row IDs."),
            ));
        }
    }
    Ok(())
}

pub(crate) fn id_at(tid: &ItemPointerData) -> Option<u64> {
    id_of(ItemPointerGetBlockNumberNoCheck(tid), ItemPointerGetOffsetNumberNoCheck(tid)).filter(|&id| id >= ROWID_FIRST)
}

/// `SnapshotDirty`'s out-fields start clear, as `HeapTupleSatisfiesDirty` clears them.
fn clear_dirty(snapshot: &Snapshot<'_>) {
    if let Some(s) = snapshot.as_deref() {
        s.dirty_xmin.set(InvalidTransactionId);
        s.dirty_xmax.set(InvalidTransactionId);
        s.dirty_speculative_token.set(0);
    }
}

/// A dirty reader's in-progress deleter is its snapshot's `dirty_xmax`, an in-progress inserter of a segment row its
/// `dirty_xmin` (heap's own report).
pub fn fetch<'mcx>(
    mcx: Mcx<'mcx>,
    f: &mut PgrcIndexFetch<'mcx>,
    tid: &ItemPointerData,
    snapshot: &Snapshot<'mcx>,
    slot: &mut SlotData<'mcx>,
    call_again: &mut bool,
    all_dead: Option<&mut bool>,
) -> PgResult<bool> {
    *call_again = false;
    let kind = kind_of(snapshot)?;
    if kind == Kind::Dirty {
        clear_dirty(snapshot);
    }
    let want = all_dead.is_some() && !transam_xlog_seams::recovery_in_progress::call();
    let v = match id_at(tid) {
        Some(id) => fetch_id(mcx, f, id, kind, snapshot, Some(&mut *slot), want)?,
        None => {
            exectuples::exec_clear_tuple(slot, mcx);
            Verdict::GONE
        }
    };
    if let Some(a) = all_dead {
        *a = want && !v.visible && v.all_dead;
        if *a {
            stats(|s| s.kill_hints += 1);
        }
    }
    if let (Some(x), Some(s)) = (v.busy, snapshot.as_deref()) {
        s.dirty_xmax.set(x);
    }
    Ok(v.visible)
}

pub fn fetch_once<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, tid: &ItemPointerData, snapshot: &Snapshot<'mcx>, slot: &mut SlotData<'mcx>) -> PgResult<bool> {
    let mut f = begin(rel);
    f.one_off = true;
    let out = fetch(mcx, &mut f, tid, snapshot, slot, &mut false, None);
    f.end();
    out
}

fn successor<'mcx>(mcx: Mcx<'mcx>, heaps: &[crate::tombs::Opened<'mcx>], id: u64) -> PgResult<Option<u64>> {
    for h in heaps {
        let dirty: Snapshot<'mcx> = Some(Rc::new(SnapshotData::sentinel(mcx, SnapshotType::SNAPSHOT_DIRTY)));
        if let Some(hit) = pgrc_front_seams::tomb_probe::call(mcx, &h.rel, id, &dirty)? {
            if hit.is_lock() {
                continue;
            }
            return Ok(match hit.succ {
                SUCC_NONE | SUCC_MOVED => None,
                s => Some(s),
            });
        }
    }
    Ok(None)
}

pub fn latest_tid<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, snapshot: &Snapshot<'mcx>, tid: &mut ItemPointerData) -> PgResult<()> {
    let Some(mut id) = id_at(tid) else { return Ok(()) };
    let kind = kind_of(snapshot)?;
    let mut f = begin(rel);
    f.one_off = true;
    let out = (|| -> PgResult<()> {
        let parent = rel.rd_id;
        loop {
            if kind == Kind::Dirty {
                clear_dirty(snapshot);
            }
            if fetch_id(mcx, &mut f, id, kind, snapshot, None, false)?.visible {
                let (block, off) = tid_of(id);
                ItemPointerSet(tid, block, off);
            }
            let heaps = f.io.tomb_heaps(mcx, parent)?;
            match successor(mcx, heaps, id)? {
                // a successor is a newer ID (IDs are handed out increasing and never reused): the chain ends
                Some(next) if next > id => id = next,
                _ => return Ok(()),
            }
            postgres_seams::check_for_interrupts::call()?;
        }
    })();
    f.end();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgrc_am::rowid_alloc::Extents;
    use pgrc_format::manifest::{encode_segments, with_next_row_id, with_ranges, IdRanges, Manifest, ManifestHeader, PartRecord};
    use pgrc_format::rowid::{seg_id, SegExtent, SEG_EXTENT_IDS};
    use std::collections::HashMap;

    const E: u64 = SEG_EXTENT_IDS;

    fn part(part_no: u32, rows: u64) -> PartRecord {
        let granule_count = rows.div_ceil(u64::from(GRANULE_ROWS)) as u32;
        PartRecord { rows, file_len: 0, footer_off: 0, dv_gen: 0, dv_len: 0, part_no, flags: 0, granule_count, band_count: 1, dv_crc: 0, granule_rows: 0 }
    }

    fn seg(slot: u16, part_no: u32, state: SegState) -> SegmentRecord {
        SegmentRecord { slot, part_no, relid: 16000 + u32::from(slot), state, fxid: 1 }
    }

    fn gen(g: u64, parts: Vec<PartRecord>, ranges: &[(u32, IdRanges)], segs: &[SegmentRecord]) -> Arc<Generation> {
        let header = ManifestHeader { gen: g, prev_gen: 0, publisher_fxid: 1, relfilenumber: 9, schema_fingerprint: 1, magic: 0, format_version: 0, spc: 1663, db: 5, part_count: parts.len() as u32, next_part_no: 100, flags: 0, reserved: 0 };
        let mut ext = with_next_row_id(&with_ranges(&[], ranges).unwrap(), 1 << 36).unwrap();
        if !segs.is_empty() {
            encode_segments(segs, &mut ext);
        }
        Arc::new(Generation::detached(Manifest::new(header, parts).with_ext(ext)).expect("generation"))
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    enum RowKind {
        Segment,
        NewerSegment,
        Sealed,
        Bulk,
        Runs,
        Pairs,
        UnderSeen,
        UnderHidden,
        Unpublished,
    }

    const KINDS: [RowKind; 9] = [RowKind::Segment, RowKind::NewerSegment, RowKind::Sealed, RowKind::Bulk, RowKind::Runs, RowKind::Pairs, RowKind::UnderSeen, RowKind::UnderHidden, RowKind::Unpublished];

    struct World {
        reader: Arc<Generation>,
        maps: Vec<Arc<RowMap>>,
        hidden: Arc<Generation>,
        newer: Vec<SegmentRecord>,
        ext: Extents,
        ids: HashMap<RowKind, u64>,
    }

    fn world() -> World {
        let (s5, s7, s9, s11) = (291 * 5_000u64, 291 * 20_000u64, 291 * 100u64, 291 * 40_000u64);
        let (runs_lo, pairs_lo, under_lo) = (291 * 60_000u64, 291 * 70_000u64, 291 * 80_000u64);
        let ext = Extents::new(vec![
            SegExtent { part_no: 5, idx: 0, base: s5 },
            SegExtent { part_no: 7, idx: 0, base: s7 },
            SegExtent { part_no: 9, idx: 0, base: s9 },
            SegExtent { part_no: 11, idx: 0, base: s11 },
        ]);
        let parts = vec![part(0, 3), part(9, 2), part(20, 2), part(21, 2), part(30, 4)];
        let ranges = vec![(0, vec![(291, 291 + 600)]), (9, vec![(s9, s9 + E)]), (20, vec![(runs_lo, runs_lo + 291)]), (21, vec![(pairs_lo, pairs_lo + 291)]), (30, vec![(under_lo, under_lo + 291)])];
        let segs = [seg(1, 5, SegState::Active), seg(3, 9, SegState::Sealed)];
        let reader = gen(10, parts.clone(), &ranges, &segs);
        let hidden = gen(10, parts[..4].to_vec(), &ranges[..4], &segs);
        let granules = |ids: &[u64]| {
            let body = pgrc_format::rowmap::encode(ids, pgrc_format::rowmap::Opts::default()).expect("the map");
            RowMap::Granules(Arc::new(pgrc_am::catalog::GranuleMap::new(pgrc_format::rowmap::GranMap::decode(&body, ids.len() as u64).expect("the map decodes"))))
        };
        let runs = granules(&[runs_lo, runs_lo + 1]);
        let pairs = granules(&[pairs_lo + 9, pairs_lo + 4]);
        let maps = vec![
            Arc::new(RowMap::Implicit(291)),
            Arc::new(granules(&[s9 + 4, s9 + 8])),
            Arc::new(runs),
            Arc::new(pairs),
            Arc::new(RowMap::Implicit(under_lo)),
        ];
        let ids = HashMap::from([
            (RowKind::Segment, seg_id(&[s5], seg_local(2, 7).unwrap()).unwrap()),
            (RowKind::NewerSegment, s7 + 3),
            (RowKind::Sealed, s9 + 8),
            (RowKind::Bulk, 292),
            (RowKind::Runs, runs_lo + 1),
            (RowKind::Pairs, pairs_lo + 4),
            (RowKind::UnderSeen, under_lo + 2),
            (RowKind::UnderHidden, under_lo + 2),
            (RowKind::Unpublished, 291 * 30_000 + 17),
        ]);
        World { reader, maps, hidden, newer: vec![seg(2, 7, SegState::Active)], ext, ids }
    }

    impl World {
        fn home(&self, kind: RowKind) -> Option<Located<'_>> {
            let g = if kind == RowKind::UnderHidden { &self.hidden } else { &self.reader };
            let newer: &[SegmentRecord] = if kind == RowKind::NewerSegment { &self.newer } else { &[] };
            position::home_in(Some(g), newer, &self.ext, self.ids[&kind], &|p| Ok(Arc::clone(&self.maps[p]))).unwrap()
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Del {
        None,
        Dv,
        TombInProgress,
        TombCommitted,
        TombRemovable,
        TombOwn,
        Locked,
    }

    const DELS: [Del; 7] = [Del::None, Del::Dv, Del::TombInProgress, Del::TombCommitted, Del::TombRemovable, Del::TombOwn, Del::Locked];

    fn hit(del: Del, kind: Kind) -> Option<TombHit> {
        let mk = |state, succ, xmin| TombHit { tid: ItemPointerData::default(), succ, xmin, cmin: 0, state, spec_token: 0, xmax: 0, own_unfrozen: false };
        let dirty = !matches!(kind, Kind::Mvcc | Kind::SelfSnapshot);
        match del {
            Del::None | Del::Dv => None,
            Del::TombInProgress if dirty => Some(mk(TombState::InProgress, SUCC_NONE, 700)),
            Del::TombInProgress => None,
            Del::TombCommitted => Some(mk(TombState::Committed, SUCC_NONE, 600)),
            Del::TombRemovable => Some(mk(TombState::Committed, SUCC_NONE, 500)),
            Del::TombOwn => Some(mk(TombState::Own, SUCC_NONE, 800)),
            Del::Locked => Some(mk(TombState::Committed, pgrc_format::rowid::SUCC_LOCK, 650)),
        }
    }

    struct Fake {
        del: Del,
        kind: Kind,
        seg_heap: bool,
        segment_asks: u32,
    }

    impl Facts for Fake {
        fn segment_visible(&mut self, _relid: Oid, _tid: &ItemPointerData) -> PgResult<bool> {
            self.segment_asks += 1;
            Ok(self.seg_heap)
        }
        fn dv(&self, _part: usize, _granule: u32, _row: u32) -> bool {
            self.del == Del::Dv
        }
        fn tomb(&mut self, _at: &Located<'_>) -> PgResult<Tomb> {
            tomb_of_hit(hit(self.del, self.kind).as_ref(), &mut |xid| Ok(xid == 500))
        }
    }

    fn heap_twin(row: RowKind, del: Del, kind: Kind) -> Verdict {
        if matches!(row, RowKind::UnderHidden | RowKind::Unpublished) {
            return Verdict::GONE;
        }
        let gone = Verdict::GONE;
        let live = Verdict::LIVE;
        let dead = Verdict { visible: false, all_dead: true, busy: None };
        match (del, kind) {
            (Del::Dv, _) => dead,
            (Del::None | Del::Locked, _) => live,
            (_, Kind::Any) => live,
            (Del::TombInProgress, Kind::Dirty) => Verdict { visible: true, all_dead: false, busy: Some(700) },
            (Del::TombInProgress, _) => live,
            (Del::TombCommitted, Kind::NonVacuumable) => live,
            (Del::TombCommitted, _) => gone,
            (Del::TombRemovable, _) => dead,
            (Del::TombOwn, Kind::NonVacuumable) => live,
            (Del::TombOwn, _) => gone,
        }
    }

    #[test]
    fn the_visibility_matrix_answers_as_the_heap_twin() {
        let w = world();
        let mut checked = 0;
        for row in KINDS {
            let at = w.home(row);
            let homed = !matches!(row, RowKind::UnderHidden | RowKind::Unpublished);
            assert_eq!(at.is_some(), homed, "{row:?}: home {}", if at.is_some() { "found" } else { "missing" });
            if let Some(at) = &at {
                assert_eq!(position::rowident(at).unwrap(), w.ids[&row], "{row:?}: the home is the ID's");
                let segment = matches!(at, Located::Segment { .. });
                assert_eq!(segment, matches!(row, RowKind::Segment | RowKind::NewerSegment), "{row:?}");
            }
            for del in DELS {
                if del == Del::Dv && matches!(row, RowKind::Segment | RowKind::NewerSegment) {
                    continue;
                }
                for kind in [Kind::Mvcc, Kind::Dirty, Kind::SelfSnapshot, Kind::Any, Kind::NonVacuumable] {
                    let mut f = Fake { del, kind, seg_heap: true, segment_asks: 0 };
                    let got = decide(kind, at.as_ref(), &mut f).unwrap();
                    assert_eq!(got, heap_twin(row, del, kind), "{row:?} {del:?} {kind:?}");
                    assert_eq!(f.segment_asks, u32::from(at.as_ref().is_some_and(|a| matches!(a, Located::Segment { .. }))), "{row:?}: the segment heap is asked exactly for a segment home");
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, (9 * 7 - 2) * 5);
    }

    #[test]
    fn a_segment_row_the_heap_hides_is_gone_before_its_tombstones() {
        struct Hidden(u32);
        impl Facts for Hidden {
            fn segment_visible(&mut self, _: Oid, _: &ItemPointerData) -> PgResult<bool> {
                Ok(false)
            }
            fn dv(&self, _: usize, _: u32, _: u32) -> bool {
                unreachable!("a segment row has no DV")
            }
            fn tomb(&mut self, _: &Located<'_>) -> PgResult<Tomb> {
                self.0 += 1;
                Ok(Tomb::Busy { xmax: 9 })
            }
        }
        let w = world();
        for row in [RowKind::Segment, RowKind::NewerSegment] {
            let at = w.home(row);
            for kind in [Kind::Mvcc, Kind::Dirty, Kind::SelfSnapshot] {
                let mut f = Hidden(0);
                assert_eq!(decide(kind, at.as_ref(), &mut f).unwrap(), Verdict::GONE);
                assert_eq!(f.0, 0);
            }
        }
    }

    #[test]
    fn a_dv_marked_row_is_dead_without_a_probe() {
        struct Dv(u32);
        impl Facts for Dv {
            fn segment_visible(&mut self, _: Oid, _: &ItemPointerData) -> PgResult<bool> {
                unreachable!()
            }
            fn dv(&self, _: usize, _: u32, _: u32) -> bool {
                true
            }
            fn tomb(&mut self, _: &Located<'_>) -> PgResult<Tomb> {
                self.0 += 1;
                Ok(Tomb::Clear)
            }
        }
        let w = world();
        for row in [RowKind::Sealed, RowKind::Bulk, RowKind::Runs, RowKind::Pairs, RowKind::UnderSeen] {
            let mut f = Dv(0);
            assert_eq!(decide(Kind::Mvcc, w.home(row).as_ref(), &mut f).unwrap(), Verdict { visible: false, all_dead: true, busy: None }, "{row:?}");
            assert_eq!(f.0, 0, "{row:?}");
        }
    }

    #[test]
    fn compacted_rows_resolve_through_their_maps() {
        let w = world();
        let Some(Located::Part { part: 2, granule: 0, row: 1, .. }) = w.home(RowKind::Runs) else { panic!("runs ordinal 1") };
        let Some(Located::Part { part: 3, granule: 0, row: 1, .. }) = w.home(RowKind::Pairs) else { panic!("pairs ordinal 1: the second row of the merge holds the smaller ID") };
        let lacks = position::home_in(Some(&w.reader), &[], &w.ext, 291 * 70_000 + 5, &|p| Ok(Arc::clone(&w.maps[p]))).unwrap();
        assert!(lacks.is_none(), "an ID inside the pairs part's range its map lacks");
    }

    #[test]
    fn only_tids_that_name_an_id_reach_a_home() {
        let tid = |b: u32, o: u16| {
            let mut t = ItemPointerData::default();
            ItemPointerSet(&mut t, b, o);
            t
        };
        assert_eq!(id_at(&tid(1, 1)), Some(291));
        assert_eq!(id_at(&tid(1, 0)), None);
        assert_eq!(id_at(&tid(1, 292)), None);
        assert_eq!(id_at(&tid(0, 5)), None, "virtual block 0 is never handed out");
        assert_eq!(id_at(&tid(u32::MAX, 0)), None, "the invalid TID");
    }

    #[test]
    fn snapshot_kinds() {
        let ctx = mcx::MemoryContext::new("ixfetch-kinds");
        let mcx = ctx.mcx();
        let of = |t| kind_of(&Some(Rc::new(SnapshotData::sentinel(mcx, t))));
        assert_eq!(kind_of(&None).unwrap(), Kind::Any);
        assert_eq!(of(SnapshotType::SNAPSHOT_MVCC).unwrap(), Kind::Mvcc);
        assert_eq!(of(SnapshotType::SNAPSHOT_SELF).unwrap(), Kind::SelfSnapshot);
        assert_eq!(of(SnapshotType::SNAPSHOT_DIRTY).unwrap(), Kind::Dirty);
        assert_eq!(of(SnapshotType::SNAPSHOT_NON_VACUUMABLE).unwrap(), Kind::NonVacuumable);
        assert_eq!(of(SnapshotType::SNAPSHOT_HISTORIC_MVCC).unwrap_err().sqlstate(), types_error::ERRCODE_FEATURE_NOT_SUPPORTED);
    }

    #[test]
    fn a_view_is_tombstone_free_only_when_every_slot_it_reads_is_empty() {
        assert!(heaps_empty(&[0], &[(0, 0)]));
        assert!(heaps_empty(&[0], &[]), "slot 0 not made yet");
        assert!(heaps_empty(&[0, 1], &[(0, 0), (1, 0)]));
        assert!(!heaps_empty(&[0], &[(0, 1)]));
        assert!(!heaps_empty(&[0, 1], &[(0, 0), (1, 3)]));
        assert!(!heaps_empty(&[1], &[(0, 0)]), "a switch's slot 1 the filed set lacks: the full build waits for it");
        assert!(heaps_empty(&[1], &[(1, 0)]), "a folded retired slot 0 is not read");
    }

    #[test]
    fn a_segment_row_deforms_through_its_last_needed_attribute() {
        assert_eq!(deform_width(None, 20), 20);
        assert_eq!(deform_width(Some(&[true, false, false, true, false]), 5), 4);
        assert_eq!(deform_width(Some(&[false, false, true]), 3), 3);
        assert_eq!(deform_width(Some(&[false; 4]), 4), 0, "a visibility-only fetch deforms nothing");
        assert_eq!(deform_width(Some(&[true, false]), 3), 3, "attribute 3 is past the mask: needed");
        assert_eq!(deform_width(Some(&[false, false, false, false, true]), 3), 0);
        assert_eq!(deform_width(Some(&[false, true, false, false, true]), 3), 2);
    }

    fn att(attnum: i16, typid: Oid) -> FormData_pg_attribute {
        FormData_pg_attribute { attnum, atttypid: typid, attlen: 4, attbyval: true, attalign: b'i' as i8, atttypmod: -1, ..Default::default() }
    }

    fn set(fp: u64) -> Rc<SchemaSet> {
        Rc::new(SchemaSet { schemas: Vec::new(), fp })
    }

    #[test]
    fn a_schema_set_answers_while_its_attributes_read_the_same() {
        let mut c = SchemaCache::default();
        let atts = vec![att(1, 20), att(2, 23)];
        assert!(c.hit((1, 9), &atts, false).is_none());
        c.file((1, 9), &atts, &set(1));
        assert_eq!(c.hit((1, 9), &atts, false).map(|s| s.fp), Some(1));
        assert!(c.hit((1, 10), &atts, false).is_none(), "another relation");
        assert!(c.hit((2, 9), &atts, false).is_none(), "another database");
        let mut typed = atts.clone();
        typed[1].atttypid = 25;
        let mut typmod = atts.clone();
        typmod[1].atttypmod = 8;
        let mut dropped = atts.clone();
        dropped[1].attisdropped = true;
        let mut added = atts.clone();
        added.push(att(3, 23));
        for (what, other) in [("type", &typed), ("typmod", &typmod), ("drop", &dropped), ("add", &added)] {
            assert!(c.hit((1, 9), other, false).is_none(), "{what}");
            assert_eq!(c.hit((1, 9), other, true).map(|s| s.fp), Some(1), "{what}: the teeth answer on the OID");
        }
        c.file((1, 9), &added, &set(2));
        assert_eq!(c.hit((1, 9), &added, false).map(|s| s.fp), Some(2));
        assert!(c.hit((1, 9), &atts, false).is_none(), "refiled over");
        for r in 100..100 + SCHEMA_SLOTS as Oid {
            c.file((1, r), &atts, &set(u64::from(r)));
        }
        assert_eq!(c.entries.len(), SCHEMA_SLOTS);
        assert!(c.hit((1, 9), &added, false).is_none(), "the clock's slot went first");
        assert_eq!(c.hit((1, 115), &atts, false).map(|s| s.fp), Some(115));
    }
}
