//! `table_tuple_fetch_row_version` for pgrc positions: a segment row is a heap fetch of its segment TID under the
//! caller's snapshot; a part row comes from the row cache (`pgrc_am::rowcache`). `SnapshotAny` returns the row whatever
//! its tombstones (DML's refetch of a row its statement read). Every other snapshot (a TID fetch's MVCC one, an RI
//! check's `SnapshotSelf`) decides through `ixfetch`.

use mcx::Mcx;
use pgrc_am::rowcache::{self, GranuleKey};
use tableam_vocab::Snapshot;
use types_error::{PgError, PgResult};
use types_rel::{AccessShareLock, NoLock, Relation};
use types_slot::SlotData;
use types_snapshot::{SnapshotData, SnapshotType};
use types_tuple::ItemPointerData;

use crate::position::{self, Located, ViewRef};

pub fn fetch_row_version<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, tid: &ItemPointerData, snapshot: &Snapshot<'mcx>, slot: &mut SlotData<'mcx>) -> PgResult<bool> {
    if snapshot.as_deref().is_some_and(|s| s.snapshot_type != SnapshotType::SNAPSHOT_ANY) {
        position::id_at(rel, tid)?;
        return crate::ixfetch::fetch_once(mcx, rel, tid, snapshot, slot);
    }
    let mut scratch = ViewRef::default();
    let at = position::locate(mcx, rel, tid, &mut scratch)?;
    crate::pathcache::note_located(rel, types_tuple::ItemPointerGetBlockNumberNoCheck(tid), types_tuple::ItemPointerGetOffsetNumberNoCheck(tid))?;
    let found = match &at {
        Located::Segment { relid, tid: seg_tid, .. } => fetch_segment(mcx, *relid, seg_tid, snapshot, slot)?,
        Located::Part { gen, part, granule, row, .. } => {
            let schemas = pgrc_am::schema::col_schemas(rel)?;
            let key = GranuleKey { gen: gen.view_key(), part: *part as u32, granule: *granule };
            rowcache::fetch_row(mcx, rel.rd_id, &gen.parts[*part], &schemas, &rel.rd_att, key, *row, slot)?;
            true
        }
    };
    if !found {
        exectuples::exec_clear_tuple(slot, mcx);
        return Ok(false);
    }
    let base = slot.base_mut();
    base.tts_tid = *tid;
    base.tts_tableOid = rel.rd_id;
    Ok(true)
}

/// A deferred row trigger's event is fetched this way at COMMIT: its position belonged to a statement's view, which is
/// gone by then, while the RowIdent is stable across seals and compactions, and the row stays in the newest generation
/// until the transaction ends (the transaction's own tombstone is not compiled while it runs).
pub fn fetch_rowident<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, r: u64, slot: &mut SlotData<'mcx>) -> PgResult<bool> {
    let mut scratch = ViewRef::default();
    let Some(at) = position::find(mcx, rel, r, &mut scratch)? else {
        exectuples::exec_clear_tuple(slot, mcx);
        return Ok(false);
    };
    match &at {
        Located::Segment { relid, tid, .. } => {
            if !fetch_segment(mcx, *relid, tid, &None, slot)? {
                return Ok(false);
            }
        }
        Located::Part { gen, part, granule, row, .. } => {
            let schemas = pgrc_am::schema::col_schemas(rel)?;
            let key = GranuleKey { gen: gen.view_key(), part: *part as u32, granule: *granule };
            rowcache::fetch_row(mcx, rel.rd_id, &gen.parts[*part], &schemas, &rel.rd_att, key, *row, slot)?;
        }
    }
    // the row's position in this statement (a deferred RI check refetches it by position under SnapshotSelf)
    let base = slot.base_mut();
    base.tts_tid = position::stamp(rel, &at)?;
    base.tts_tableOid = rel.rd_id;
    Ok(true)
}

fn fetch_segment<'mcx>(mcx: Mcx<'mcx>, relid: types_core::Oid, tid: &ItemPointerData, snapshot: &Snapshot<'mcx>, slot: &mut SlotData<'mcx>) -> PgResult<bool> {
    let seg = table::table_open(mcx, relid, AccessShareLock)?;
    let any;
    let snap = match snapshot.as_deref() {
        Some(s) => s,
        None => {
            any = SnapshotData::sentinel(mcx, SnapshotType::SNAPSHOT_ANY);
            &any
        }
    };
    let res = heapam::heap_fetch(&seg, snap, *tid, false)?;
    let out = match res.tuple() {
        Some(tup) if res.found => {
            exectuples::exec_clear_tuple(slot, mcx);
            let base = slot.base_mut();
            let natts = seg.rd_att.natts as usize;
            if natts > base.tts_values.len() {
                return Err(Box::new(PgError::error(format!("pgrcolumnar: segment {relid} has more columns than its table"))));
            }
            types_tuple::heap_deform_tuple(&tup, &seg.rd_att, &mut base.tts_values[..natts], &mut base.tts_isnull[..natts]);
            base.tts_isnull[natts..].fill(true);
            base.tts_nvalid = base.tts_values.len() as types_core::AttrNumber;
            base.mark_not_empty();
            exectuples::exec_materialize_slot(slot, mcx).map(|()| true)
        }
        _ => Ok(false),
    };
    drop(res);
    seg.close(NoLock)?;
    out
}
