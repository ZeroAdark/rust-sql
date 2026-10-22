//! A live segment read under the statement's snapshot, deformed into the parent's slot; the rows the scan's tombstones
//! name are skipped.

use heapam::HeapScanDescData;
use mcx::{Mcx, PgVec};
use pgrc_am::scan::SegmentSource;
use tableam_vocab::{Snapshot, SO_ALLOW_PAGEMODE, SO_ALLOW_STRAT, SO_TYPE_SEQSCAN};
use types_core::{Oid, TransactionId};
use types_error::{PgError, PgResult};
use types_rel::{AccessShareLock, NoLock, Relation};
use types_scan::sdir::ScanDirection;
use types_slot::SlotData;
use types_tuple::{ItemPointerGetBlockNumber, ItemPointerGetOffsetNumberNoCheck, ItemPointerSet};

pub struct SegScan<'mcx> {
    rel: Relation<'mcx>,
    scan: HeapScanDescData<'mcx>,
    parent: Oid,
    deleted: Vec<u32>,
    busy: Vec<(u32, TransactionId)>,
    bases: Vec<u64>,
    dir: String,
    part_no: u32,
    skipped: u64,
    returned: u64,
}

/// An index build's segment snapshot: every row a snapshot may still see, as heapam's build indexes recently dead
/// rows and skips dead ones (an aborted insert).
pub fn index_build_snapshot<'mcx>(mcx: Mcx<'mcx>, table: &Relation<'_>) -> Snapshot<'mcx> {
    let mut snapshot = types_snapshot::SnapshotData::sentinel(mcx, types_snapshot::SnapshotType::SNAPSHOT_NON_VACUUMABLE);
    snapshot.vistest = procarray::GlobalVisTestFor(table);
    Some(std::rc::Rc::new(snapshot))
}

impl<'mcx> SegScan<'mcx> {
    #[allow(clippy::too_many_arguments)]
    pub fn begin(mcx: Mcx<'mcx>, table: &Relation<'_>, relid: Oid, _slot: u16, part_no: u32, snapshot: Snapshot<'mcx>, parent: Oid, deleted: Vec<u32>) -> PgResult<SegScan<'mcx>> {
        let dir = pgrc_am::dirpath::table_dir_path(table.rd_locator.get(), table.rd_backend);
        let bases = pgrc_am::rowid_alloc::extents(&dir)?.bases(part_no);
        let rel = table::table_open(mcx, relid, AccessShareLock)?;
        let scan = heapam::heap_beginscan(mcx, &rel, snapshot, 0, PgVec::new_in(mcx), None, SO_TYPE_SEQSCAN | SO_ALLOW_STRAT | SO_ALLOW_PAGEMODE)?;
        Ok(SegScan { rel, scan, parent, deleted, busy: Vec::new(), bases, dir, part_no, skipped: 0, returned: 0 })
    }

    pub fn with_busy(mut self, busy: Vec<(u32, TransactionId)>) -> SegScan<'mcx> {
        self.busy = busy;
        self
    }

    fn id_at(&mut self, local: u32) -> PgResult<Option<u64>> {
        if let Some(id) = pgrc_format::rowid::seg_id(&self.bases, local) {
            return Ok(Some(id));
        }
        self.bases = pgrc_am::rowid_alloc::extents(&self.dir)?.bases(self.part_no);
        if self.bases.is_empty() {
            return Ok(None);
        }
        pgrc_format::rowid::seg_id(&self.bases, local).map(Some).ok_or_else(|| {
            Box::new(PgError::error(format!("pgrcolumnar: segment {} row at local position {local} has no reserved extent", self.rel.rd_id)).with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED))
        })
    }
}

impl SegmentSource for SegScan<'_> {
    fn next(&mut self, slot: &mut SlotData<'_>) -> PgResult<bool> {
        loop {
            let Some(tup) = heapam::heap_getnext(&mut self.scan, ScanDirection::ForwardScanDirection)? else { return Ok(false) };
            let (block, off) = (ItemPointerGetBlockNumber(&tup.t_self), ItemPointerGetOffsetNumberNoCheck(&tup.t_self));
            let Some(local) = pgrc_format::rowid::seg_local(block, off) else {
                return Err(Box::new(PgError::error(format!("pgrcolumnar: segment {} row ({block},{off}) past the position range", self.rel.rd_id))));
            };
            if !self.deleted.is_empty() && self.deleted.binary_search(&local).is_ok() {
                self.skipped += 1;
                continue;
            }
            let base = slot.base_mut();
            let natts = self.rel.rd_att.natts as usize;
            if natts > base.tts_values.len() {
                return Err(Box::new(PgError::error(format!("pgrcolumnar: segment {} has more columns than its table", self.rel.rd_id))));
            }
            types_tuple::heap_deform_tuple(tup, &self.rel.rd_att, &mut base.tts_values[..natts], &mut base.tts_isnull[..natts]);
            base.tts_isnull[natts..].fill(true);
            base.tts_nvalid = base.tts_values.len() as types_core::AttrNumber;
            let id = self.id_at(local)?;
            let base = slot.base_mut();
            match id {
                Some(id) => {
                    let (block, off) = pgrc_format::rowid::tid_of(id);
                    ItemPointerSet(&mut base.tts_tid, block, off);
                }
                None => types_tuple::ItemPointerSetInvalid(&mut base.tts_tid),
            }
            base.tts_tableOid = self.parent;
            base.mark_not_empty();
            if !self.busy.is_empty() {
                if let (Ok(i), Some(s)) = (self.busy.binary_search_by_key(&local, |b| b.0), self.scan.rs_base.rs_snapshot.as_deref()) {
                    s.dirty_xmax.set(self.busy[i].1);
                }
            }
            if self.scan.rs_base.rs_rd.pgstat_enabled.get() {
                self.returned += 1;
            }
            return Ok(true);
        }
    }

    fn returned(&self) -> u64 {
        self.returned
    }

    fn end(self: Box<Self>) -> PgResult<()> {
        let SegScan { rel, scan, .. } = *self;
        heapam::heap_endscan(scan)?;
        rel.close(NoLock)
    }
}
