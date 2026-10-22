//! A compile, in the caller's transaction under the maintenance lock, does what the state allows, in order:
//! - **reset**: a retired heap folded by a transaction below the horizon (every snapshot that can run sees the fold's
//!   DVs) gets a new, empty relfilenode, after a standby conflict record at the fold's xid;
//! - **fold**: a retired heap whose every tombstone has an inserter below the horizon (so no writer needs one as a
//!   row lock) folds whole into the parts' DVs, with its state `folded`, in one publish;
//!   a tombstone whose row is still in a live segment is carried: inserted again, by the fold's transaction, into
//!   the current heap (every snapshot that can run once the retired heap resets sees the fold, so sees the copy);
//! - **switch**: with no retired heap (or a reset one) and a current heap past [`COMPILE_BLOCKS`] (unforced, at a
//!   moment no writer holds it) or forced, the other heap becomes current. A user's forced compile that gets a
//!   ShareLock on the current heap (no writer holds it) and finds it foldable switches and folds in one publish.
//!
//! No tombstone is heap-deleted: the heaps retire whole, and a reader whose view predates a fold reads the retired
//! heap until the reset, which waits for it.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use mcx::{Mcx, PgVec};
use pgrc_am::catalog::{Generation, RowMap};
use pgrc_format::dml::Tombstone;
use pgrc_format::manifest::{with_tombs, Manifest, RetiredTombs, TombsRecord, TOMB_SLOTS};
use pgrc_front_seams::TombInsert;
use pgrc_write::plan::PlanBuilder;
use tableam_vocab::{VacuumParams, SO_TYPE_SEQSCAN};
use types_core::xact::{TransactionIdPrecedes, TransactionIdPrecedesOrEquals};
use types_core::{ForkNumber, Oid, TransactionId};
use types_error::{PgError, PgResult};
use types_rel::{AccessExclusiveLock, AccessShareLock, NoLock, Relation, ShareLock, ShareUpdateExclusiveLock};
use types_scan::sdir::ScanDirection;
use types_snapshot::{SnapshotData, SnapshotType};
use types_storage::buf::BufferAccessStrategy;

use crate::position::{Finder, Located};
use pgrc_am::rowid_alloc::Extents;

pub const COMPILE_BLOCKS: u32 = 1024;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Compiled {
    pub tombstones: u64,
    pub parts: u32,
    pub folded: bool,
    pub switched: bool,
    pub reset: bool,
}

impl Compiled {
    pub fn progressed(&self) -> bool {
        self.folded || self.switched || self.reset
    }
}

type Tombs = Vec<Tombstone>;

type Marks = BTreeMap<u32, Vec<(u32, u16)>>;

/// What the last fold read that stopped saw: the xid bound of its read. A retired heap gains nothing after
/// its switch commits (a writer's insert precedes its recheck), so a tombstone that stopped the fold had an inserter
/// below that bound that the horizon had not passed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    slot: u8,
    xmax: TransactionId,
}

pgsync::process_global! {
    static SEEN: pgsync::Mutex<Vec<(Oid, Seen)>> = pgsync::Mutex::new(Vec::new());
}

/// An unforced fold rereads the retired heap only once the horizon reached the last read's bound: before that it
/// would stop again, and AGE's forced compile reads regardless.
fn worth_reading(last: Option<&Seen>, slot: u8, horizon: TransactionId) -> bool {
    let Some(last) = last.filter(|l| l.slot == slot) else { return true };
    TransactionIdPrecedesOrEquals(last.xmax, horizon)
}

#[derive(Default)]
struct Clock {
    read: f64,
    select: f64,
    publish: f64,
    retire: f64,
    scanned: u64,
}

fn ms_since(t: std::time::Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// Unforced, a current heap under [`COMPILE_BLOCKS`] is not switched. A forced compile of a user (`pgrc_flush`, VACUUM)
/// may take a ShareLock on the current heap to switch and fold it at once; a background one (the worker, autovacuum)
/// never waits for or holds off a writer.
pub fn compile<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, force: bool) -> PgResult<Compiled> {
    let start = std::time::Instant::now();
    let Some(gen) = crate::ring::newest(rel)? else { return Ok(Compiled::default()) };
    let st = crate::tombs::state(Some(&gen))?;
    let blocks = crate::tombs::blocks(mcx, rel.rd_id)?;
    if blocks == 0 {
        pgsync::lock(&SEEN).retain(|m| m.0 != rel.rd_id);
        return Ok(Compiled::default());
    }
    let (mut out, mut clock) = (Compiled::default(), Clock::default());
    let mut free = st.ret.is_none();
    match st.ret {
        Some(ret) if ret.folded != 0 => {
            let t = std::time::Instant::now();
            (free, out.reset) = match reset(mcx, rel, ret)? {
                Reset::Done => (true, true),
                Reset::Empty => (true, false),
                Reset::Wait => (false, false),
            };
            clock.retire = ms_since(t);
        }
        Some(ret) => fold_retired(mcx, rel, &gen, st, ret, force, &mut out, &mut clock)?,
        None => {}
    }
    if free && !out.folded {
        switch(mcx, rel, &gen, st, force, &mut out, &mut clock)?;
    }
    if out.progressed() || clock.scanned > 0 {
        log(rel, blocks, force, &out, &clock, ms_since(start))?;
    }
    Ok(out)
}

fn log(rel: &Relation<'_>, blocks: u32, force: bool, out: &Compiled, c: &Clock, total: f64) -> PgResult<()> {
    let msg = format!(
        "pgrcolumnar: compiled {} tombstones of \"{}\" into {} parts ({} tombstone-heap blocks, {} tombstones read, forced {}) in {:.3} ms; read {:.1} select {:.1} publish {:.1} retire {:.1}; switched {} reset {}",
        out.tombstones,
        rel.name(),
        out.parts,
        blocks,
        c.scanned,
        force,
        total,
        c.read,
        c.select,
        c.publish,
        c.retire,
        out.switched,
        out.reset
    );
    elog::elog(types_error::DEBUG1, msg)
}

enum Reset {
    Done,
    Empty,
    Wait,
}

/// The reset of a folded retired heap once its fold's xid precedes the horizon, under a conditional
/// AccessExclusiveLock on the heap and its index (no scan or probe in flight; later ones find the new relfilenode):
/// every snapshot that can run then sees the fold's DVs, and a view with the heap folded never reads it.
fn reset<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, ret: RetiredTombs) -> PgResult<Reset> {
    let Some(relid) = crate::tombs::relid(rel.rd_id, ret.slot)? else { return Ok(Reset::Empty) };
    let heap = table::table_open(mcx, relid, AccessShareLock)?;
    let n = bufmgr::RelationGetNumberOfBlocksInFork(&heap, ForkNumber::MAIN_FORKNUM);
    let horizon = procarray::GetOldestNonRemovableTransactionId(&heap);
    heap.close(AccessShareLock)?;
    if n? == 0 {
        return Ok(Reset::Empty);
    }
    if !fold_precedes(ret.folded, horizon?)? || !lmgr::ConditionalLockRelationOid(relid, AccessExclusiveLock)? {
        return Ok(Reset::Wait);
    }
    let indexes = relcache_seams::relation_get_index_list::call(mcx, relid)?;
    for &idx in indexes.iter() {
        if !lmgr::ConditionalLockRelationOid(idx, AccessExclusiveLock)? {
            return Ok(Reset::Wait);
        }
    }
    let heap = table::table_open(mcx, relid, NoLock)?;
    let conflict = match (heap.is_permanent() && transam_xlog_seams::xlog_standby_info_active::call(), indexes.first()) {
        (true, Some(&idx)) => {
            let index = relation_seams::relation_open::call(mcx, idx, NoLock)?;
            let body = reuse_page(index.rd_locator.get(), ret.folded);
            index.close(NoLock)?;
            xloginsert_seams::xlog_insert_record::call(types_core::RmgrIds::RM_BTREE_ID as u8, types_nbtree::XLOG_BTREE_REUSE_PAGE, 0, &[&body], &[]).map(drop)
        }
        _ => Ok(()),
    };
    heap.close(NoLock)?;
    conflict?;
    pgrc_front_seams::reset_segment::call(mcx, relid)?;
    Ok(Reset::Done)
}

pub(crate) fn reuse_page(locator: types_storage::RelFileLocator, horizon: u64) -> [u8; 25] {
    let mut b = [0u8; 25];
    b[0..4].copy_from_slice(&locator.spcOid.to_ne_bytes());
    b[4..8].copy_from_slice(&locator.dbOid.to_ne_bytes());
    b[8..12].copy_from_slice(&locator.relNumber.to_ne_bytes());
    b[16..24].copy_from_slice(&horizon.to_ne_bytes());
    b
}

/// Whether the fold's full xid precedes `horizon` (a recent 32-bit xid): a fold 2^31 or more xids back does.
fn fold_precedes(folded: u64, horizon: TransactionId) -> PgResult<bool> {
    let now = varsup::ReadNextFullTransactionId()?.value;
    Ok(now.saturating_sub(folded) >= 1 << 31 || TransactionIdPrecedes(folded as TransactionId, horizon))
}

enum Read {
    All(Tombs),
    Stopped,
}

fn read<'mcx>(mcx: Mcx<'mcx>, tomb: &Relation<'mcx>, horizon: TransactionId, scanned: &mut u64) -> PgResult<Read> {
    let snap = Rc::new(SnapshotData::sentinel(mcx, SnapshotType::SNAPSHOT_DIRTY));
    let mut hs = heapam::heap_beginscan(mcx, tomb, Some(Rc::clone(&snap)), 0, PgVec::new_in(mcx), None, SO_TYPE_SEQSCAN)?;
    let mut out = Vec::new();
    let res = (|| -> PgResult<bool> {
        while let Some(tup) = heapam::heap_getnext(&mut hs, ScanDirection::ForwardScanDirection)? {
            *scanned += 1;
            let xmin = tup.t_data().xmin();
            if snap.dirty_xmin.get() != 0 || snap.dirty_xmax.get() != 0 {
                return Ok(false);
            }
            let (mut v, mut n) = ([datum::Datum::null(); 2], [false; 2]);
            types_tuple::heap_deform_tuple(tup, &tomb.rd_att, &mut v, &mut n);
            let t = Tombstone::from_datums([v[0].as_i64(), v[1].as_i64()])
                .map_err(|e| Box::new(PgError::error(format!("pgrcolumnar: tombstone {e}")).with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED)))?;
            // LOCK: a LOCK row deletes nothing and is never carried: the fold's ShareLock on the heap means no row
            // lock in it is held (every locker holds RowExclusiveLock on its LOCK row's heap to transaction end), and
            // the reset drops it
            if t.is_lock() {
                continue;
            }
            if !TransactionIdPrecedes(xmin, horizon) {
                return Ok(false);
            }
            out.push(t);
        }
        Ok(true)
    })();
    heapam::heap_endscan(hs)?;
    Ok(if res? { Read::All(out) } else { Read::Stopped })
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Fold {
    marks: Marks,
    carry: Vec<Tombstone>,
    folded: u64,
}

/// The fold of `tombs` in `gen`. A RowIdent with no row in `gen` (a part retired whole, a position past its rows)
/// needs no DV: no view at or after the fold has the row.
fn select(gen: &Arc<Generation>, ext: &Extents, tombs: &[Tombstone], map_of: &dyn Fn(usize) -> PgResult<Arc<RowMap>>) -> PgResult<Fold> {
    let mut finder = Finder::new(gen, ext);
    let mut rows: Vec<Vec<(u32, u16)>> = ::mcx::stdvec::try_filled(gen.manifest.parts.len(), Vec::new(), "pgrc tombstone fold")?;
    let mut out = Fold::default();
    for t in tombs {
        match finder.find(t.rowident, map_of)? {
            Some(Located::Part { part, granule, row, .. }) => rows[part].push((granule, row as u16)),
            Some(Located::Segment { .. }) => {
                out.carry.push(*t);
                continue;
            }
            None => {}
        }
        out.folded += 1;
    }
    out.marks = rows.into_iter().enumerate().filter(|(_, m)| !m.is_empty()).map(|(p, m)| (gen.manifest.parts[p].part_no, m)).collect();
    Ok(out)
}

fn plan<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, tomb: &Relation<'mcx>, gen: &Arc<Generation>, horizon: TransactionId, clock: &mut Clock) -> PgResult<Option<Fold>> {
    let t = std::time::Instant::now();
    let read = read(mcx, tomb, horizon, &mut clock.scanned)?;
    clock.read += ms_since(t);
    let Read::All(tombs) = read else { return Ok(None) };
    let t = std::time::Instant::now();
    let dir = pgrc_am::dirpath::table_dir_path(rel.rd_locator.get(), rel.rd_backend);
    let ext = pgrc_am::rowid_alloc::extents(&dir)?;
    let fold = select(gen, &ext, &tombs, &|p| gen.row_map(p, &dir))?;
    clock.select += ms_since(t);
    Ok(Some(fold))
}

/// Insert the carried tombstones into heap `slot` (the current heap of the state this transaction publishes). No writer
/// holds their RowIdents there: until the fold commits, writers probe the unfolded retired heap and find the originals.
fn carry<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, slot: u8, tombs: &[Tombstone]) -> PgResult<()> {
    if tombs.is_empty() {
        return Ok(());
    }
    let relid = crate::tombs::relid(rel.rd_id, slot)?.ok_or_else(|| Box::new(PgError::error(format!("pgrcolumnar: relation {} has no tombstone relation {slot}", rel.rd_id))))?;
    let heap = table::table_open(mcx, relid, types_rel::RowExclusiveLock)?;
    let cid = xact_seams::get_current_command_id::call(true)?;
    let res = tombs.iter().try_for_each(|t| match pgrc_front_seams::tomb_insert::call(mcx, &heap, *t, cid, &mut || Ok(true))? {
        TombInsert::Won(_) => Ok(()),
        other => Err(Box::new(PgError::error(format!("pgrcolumnar: carrying tombstone {} of relation {}: {other:?}", t.rowident, rel.rd_id)))),
    });
    heap.close(NoLock)?;
    res
}

fn publish(rel: &Relation<'_>, marks: Option<&Marks>, st: TombsRecord, clock: &mut Clock) -> PgResult<u32> {
    let t = std::time::Instant::now();
    let mut edit = |base: Option<&Manifest>, b: &mut PlanBuilder<'_>| {
        b.ext(with_tombs(base.map_or(&[][..], |m| &m.ext), &st)?);
        Ok(())
    };
    let refs: Vec<(u32, &[(u32, u16)])> = marks.map_or_else(Vec::new, |m| m.iter().map(|(p, r)| (*p, &r[..])).collect());
    let parts = match refs.is_empty() {
        false if pgrc_am::ingest::publish_compile_ext(rel, &refs, &mut edit)?.is_some() => refs.len() as u32,
        _ => pgrc_am::ingest::publish_ext(rel, &mut edit).map(|_| 0)?,
    };
    clock.publish += ms_since(t);
    Ok(parts)
}

#[allow(clippy::too_many_arguments)]
fn fold_retired<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, gen: &Arc<Generation>, st: TombsRecord, ret: RetiredTombs, force: bool, out: &mut Compiled, clock: &mut Clock) -> PgResult<()> {
    let Some(relid) = crate::tombs::relid(rel.rd_id, ret.slot)? else { return Ok(()) };
    // LOCK: no row locker or writer holds the retired heap (each holds RowExclusiveLock on the heap of the LOCK row
    // it locked, to transaction end; a prepared transaction keeps it), so no LOCK row the fold drops is a held lock,
    // and none is locked before the fold commits (a locker locks the heap, then rechecks it is still unfolded)
    if !lmgr::ConditionalLockRelationOid(relid, ShareLock)? {
        return Ok(());
    }
    let tomb = table::table_open(mcx, relid, AccessShareLock)?;
    let res = (|| {
        let horizon = procarray::GetOldestNonRemovableTransactionId(&tomb)?;
        if !force && !worth_reading(pgsync::lock(&SEEN).iter().find(|m| m.0 == rel.rd_id).map(|m| &m.1), ret.slot, horizon) {
            return Ok(());
        }
        pgrc_am::ingest::defer_if_busy(rel)?;
        let xmax = snapmgr::GetLatestSnapshot()?.xmax;
        let planned = plan(mcx, rel, &tomb, gen, horizon, clock)?;
        let mut memo = pgsync::lock(&SEEN);
        memo.retain(|m| m.0 != rel.rd_id);
        let Some(fold) = planned else {
            memo.push((rel.rd_id, Seen { slot: ret.slot, xmax }));
            return Ok(());
        };
        drop(memo);
        let folded = ::xact::GetCurrentFullTransactionId()?.value;
        carry(mcx, rel, st.cur, &fold.carry)?;
        out.parts = publish(rel, Some(&fold.marks), TombsRecord { cur: st.cur, ret: Some(RetiredTombs { slot: ret.slot, folded }) }, clock)?;
        (out.tombstones, out.folded) = (fold.folded, true);
        Ok(())
    })();
    tomb.close(AccessShareLock)?;
    res
}

#[allow(clippy::too_many_arguments)]
fn switch<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, gen: &Arc<Generation>, st: TombsRecord, force: bool, out: &mut Compiled, clock: &mut Clock) -> PgResult<()> {
    let Some(cur) = crate::tombs::relid(rel.rd_id, st.cur)? else { return Ok(()) };
    let tomb = table::table_open(mcx, cur, AccessShareLock)?;
    let res = (|| {
        let n = bufmgr::RelationGetNumberOfBlocksInFork(&tomb, ForkNumber::MAIN_FORKNUM)?;
        if n == 0 || (!force && n < COMPILE_BLOCKS) {
            return Ok(());
        }
        // An unforced switch waits for a moment when no writer holds the current heap, up to WRITER_WAIT. Under a
        // writer, its statement's tombstones would split across both heaps: the retired part could not fold before
        // the statement ends, and the rest, under COMPILE_BLOCKS, would wait for AGE. The worker's next visit comes
        // back; AGE's forced compile switches regardless.
        if !force && !switch_now(rel.rd_id, cur)? {
            return Ok(());
        }
        const _: () = assert!(TOMB_SLOTS == 2);
        let next = 1 - st.cur;
        match crate::tombs::relid(rel.rd_id, next)? {
            Some(r) if blocks_of(mcx, r)? > 0 => return Ok(()),
            Some(_) => {}
            None => {
                pgrc_front_seams::create_tomb::call(mcx, rel.rd_id, next)?;
            }
        }
        pgrc_am::ingest::defer_if_busy(rel)?;
        let hold = force && !pgrc_am::ingest::is_background() && lmgr::ConditionalLockRelationOid(cur, ShareLock)?;
        let planned = match hold {
            true => plan(mcx, rel, &tomb, gen, procarray::GetOldestNonRemovableTransactionId(&tomb)?, clock)?,
            false => None,
        };
        let folded = match &planned {
            Some(fold) => {
                carry(mcx, rel, next, &fold.carry)?;
                ::xact::GetCurrentFullTransactionId()?.value
            }
            None => 0,
        };
        let new = TombsRecord { cur: next, ret: Some(RetiredTombs { slot: st.cur, folded }) };
        out.parts = publish(rel, planned.as_ref().map(|p| &p.marks), new, clock)?;
        pgsync::lock(&SEEN).retain(|m| m.0 != rel.rd_id);
        out.switched = true;
        if let Some(fold) = planned {
            (out.tombstones, out.folded) = (fold.folded, true);
        }
        Ok(())
    })();
    tomb.close(AccessShareLock)?;
    res
}

const WRITER_WAIT_NS: u64 = 30_000_000_000;

pgsync::process_global! {
    static HELD_OFF: pgsync::Mutex<Vec<(Oid, pg_clock::MonoStamp)>> = pgsync::Mutex::new(Vec::new());
}

fn switch_now(parent: Oid, cur: Oid) -> PgResult<bool> {
    let clear = lmgr::ConditionalLockRelationOid(cur, ShareLock)?;
    if clear {
        lmgr::UnlockRelationOid(cur, ShareLock)?;
    }
    let now = pg_clock::MonoStamp::now();
    let mut held = pgsync::lock(&HELD_OFF);
    let first = match held.iter().find(|h| h.0 == parent) {
        Some(h) => h.1,
        None => {
            held.push((parent, now));
            now
        }
    };
    let go = clear || now.since_ns(first) >= WRITER_WAIT_NS;
    if go {
        held.retain(|h| h.0 != parent);
    }
    Ok(go)
}

/// `(0, 0)`: every tombstone and LOCK row is folded or reset, so no heap a reader of a newer generation reads names a
/// row ID.
pub fn unfolded<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<(u64, u64)> {
    let st = crate::tombs::newest_state(rel)?;
    let (mut deletes, mut locks) = (0u64, 0u64);
    for slot in crate::tombs::probe_slots(&st) {
        let Some(relid) = crate::tombs::relid(rel.rd_id, slot)? else { continue };
        let tomb = table::table_open(mcx, relid, AccessShareLock)?;
        let snap = Rc::new(SnapshotData::sentinel(mcx, SnapshotType::SNAPSHOT_DIRTY));
        let mut hs = heapam::heap_beginscan(mcx, &tomb, Some(snap), 0, PgVec::new_in(mcx), None, SO_TYPE_SEQSCAN)?;
        let res = (|| -> PgResult<()> {
            while let Some(tup) = heapam::heap_getnext(&mut hs, ScanDirection::ForwardScanDirection)? {
                let (mut v, mut n) = ([datum::Datum::null(); 2], [false; 2]);
                types_tuple::heap_deform_tuple(tup, &tomb.rd_att, &mut v, &mut n);
                let t = Tombstone::from_datums([v[0].as_i64(), v[1].as_i64()])
                    .map_err(|e| Box::new(PgError::error(format!("pgrcolumnar: tombstone {e}")).with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED)))?;
                match t.is_lock() {
                    true => locks += 1,
                    false => deletes += 1,
                }
            }
            Ok(())
        })();
        heapam::heap_endscan(hs)?;
        tomb.close(AccessShareLock)?;
        res?;
    }
    Ok((deletes, locks))
}

fn blocks_of(mcx: Mcx<'_>, relid: Oid) -> PgResult<u32> {
    let heap = table::table_open(mcx, relid, AccessShareLock)?;
    let n = bufmgr::RelationGetNumberOfBlocksInFork(&heap, ForkNumber::MAIN_FORKNUM);
    heap.close(AccessShareLock)?;
    n
}

pub fn blocks<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<u32> {
    crate::tombs::blocks(mcx, rel.rd_id)
}

pub fn vacuum<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, params: &VacuumParams, bstrategy: BufferAccessStrategy) -> PgResult<()> {
    for (_, relid) in crate::tombs::all(rel.rd_id)? {
        if !lmgr::ConditionalLockRelationOid(relid, ShareUpdateExclusiveLock)? {
            continue;
        }
        let tomb = table::table_open(mcx, relid, NoLock)?;
        let res = tableam_seams::table_relation_vacuum::call(mcx, &tomb, params, bstrategy.clone());
        tomb.close(NoLock)?;
        res?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgrc_format::geom::GRANULE_ROWS;
    use pgrc_format::manifest::{encode_segments, ManifestHeader, PartRecord, SegState, SegmentRecord};

    fn part(part_no: u32, rows: u64) -> PartRecord {
        let granule_count = rows.div_ceil(u64::from(GRANULE_ROWS)) as u32;
        PartRecord { rows, file_len: 0, footer_off: 0, dv_gen: 0, dv_len: 0, part_no, flags: 0, granule_count, band_count: 1, dv_crc: 0, granule_rows: 0 }
    }

    fn gen(parts: Vec<PartRecord>, segs: &[SegmentRecord]) -> Arc<Generation> {
        let header = ManifestHeader { gen: 7, prev_gen: 0, publisher_fxid: 1, relfilenumber: 9, schema_fingerprint: 1, magic: 0, format_version: 0, spc: 1663, db: 5, part_count: parts.len() as u32, next_part_no: 100, flags: 0, reserved: 0 };
        let mut ext = Vec::new();
        if !segs.is_empty() {
            encode_segments(segs, &mut ext);
        }
        Arc::new(Generation::detached(Manifest::new(header, parts).with_ext(ext)).expect("generation"))
    }

    fn tomb(rowident: u64) -> Tombstone {
        Tombstone { rowident, succ: pgrc_format::rowid::SUCC_NONE }
    }

    #[test]
    fn a_heap_folds_whole_carrying_live_segment_tombstones() {
        use pgrc_format::manifest::{with_next_row_id, with_ranges};
        use pgrc_format::rowid::{seg_local, SegExtent, SEG_EXTENT_IDS};
        let seg = |slot, part_no, state| SegmentRecord { slot, part_no, relid: 16000, state, fxid: 1 };
        let (s9, s12) = (291 * 1000, 291 * 3000);
        let g = gen(vec![part(0, u64::from(GRANULE_ROWS) + 20), part(9, 3)], &[seg(0, 9, SegState::Sealed), seg(1, 12, SegState::Active)]);
        let ranges = vec![(0, vec![(291, 291 * 900)]), (9, vec![(s9, s9 + SEG_EXTENT_IDS)])];
        let ext = with_next_row_id(&with_ranges(&g.manifest.ext, &ranges).unwrap(), 1 << 30).unwrap();
        let g = Arc::new(Generation::detached((*g.manifest).clone().with_ext(ext)).unwrap());
        let segext = Extents::new(vec![SegExtent { part_no: 12, idx: 0, base: s12 }]);
        let local = |b, o| u64::from(seg_local(b, o).unwrap());
        let sealed: Vec<u64> = [(0, 1), (0, 4), (3, 2)].iter().map(|&(b, o)| s9 + local(b, o)).collect();
        let body = pgrc_format::rowmap::encode(&sealed, pgrc_format::rowmap::Opts::default()).unwrap();
        let sealed_map = pgrc_am::catalog::GranuleMap::new(pgrc_format::rowmap::GranMap::decode(&body, 3).unwrap());
        let maps = [Arc::new(RowMap::Implicit(291)), Arc::new(RowMap::Granules(Arc::new(sealed_map)))];
        let map_of = |p: usize| Ok(Arc::clone(&maps[p]));
        let tombs = [
            tomb(291 + 5),                                  // part 0, granule 0, row 5
            tomb(291 + u64::from(GRANULE_ROWS) + 19),       // granule 1, row 19
            tomb(s9 + local(3, 2)),                         // the sealed segment's row: part 9, ordinal 2
            tomb(291 + u64::from(GRANULE_ROWS) + 20),       // past the part's rows: no row, retired with the heap
            tomb(s9 + local(1, 1)),                         // a position the sealed map lacks: likewise
            tomb(1 << 29),                                  // an ID no part homes: likewise
        ];
        let f = select(&g, &segext, &tombs, &map_of).unwrap();
        assert_eq!(f.marks.into_iter().collect::<Vec<_>>(), vec![(0, vec![(0, 5), (1, 19)]), (9, vec![(0, 2)])]);
        assert_eq!((f.folded, f.carry.len()), (6, 0));
        let live = Tombstone { rowident: s12 + local(0, 3), succ: 77 };
        let mut with_live = tombs.to_vec();
        with_live.insert(1, live);
        let f = select(&g, &segext, &with_live, &map_of).unwrap();
        assert_eq!((f.folded, f.carry), (6, vec![live]), "a live segment's row: carried to the current heap, succ kept");
        assert_eq!(f.marks.len(), 2);
        assert_eq!(select(&g, &segext, &[], &map_of).unwrap(), Fold::default(), "an empty heap folds with no marks");
    }

    #[test]
    fn an_unforced_fold_rereads_only_when_a_stopped_read_may_now_fold() {
        let last = Seen { slot: 1, xmax: 700 };
        assert!(worth_reading(None, 1, 600), "no history");
        assert!(worth_reading(Some(&last), 0, 600), "another retired heap");
        assert!(!worth_reading(Some(&last), 1, 699), "the horizon has not reached the last read's bound");
        assert!(worth_reading(Some(&last), 1, 700), "every inserter the last read saw is below the horizon");
        assert!(!worth_reading(Some(&last), 1, 4_000_000_000), "a wrapped-around xid precedes 700");
    }

    #[test]
    fn the_conflict_record_is_cs_xl_btree_reuse_page() {
        let b = reuse_page(types_storage::RelFileLocator::new(1663, 5, 16400), 0x1_0000_0203);
        assert_eq!(&b[0..4], &1663u32.to_ne_bytes());
        assert_eq!(&b[8..12], &16400u32.to_ne_bytes());
        assert_eq!(&b[12..16], &[0; 4], "block 0");
        assert_eq!(u64::from_ne_bytes(b[16..24].try_into().unwrap()), 0x1_0000_0203);
        assert_eq!(b[24], 0, "not a catalog relation");
    }
}
