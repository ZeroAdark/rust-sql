//! pgrust's table AM extensions beyond tableam.h (no C counterpart).

use ::mcx::Mcx;
use ::types_core::Oid;
use ::types_error::{PgError, PgResult};
use ::types_nodes::CmdType;
use ::types_rel::Relation;
use ::types_slot::{SlotData, TupleSlotKind};
use ::types_tuple::ItemPointerData;

use crate::{am, IndexFetchTableData, ScanRoad, TableAm, TableScanDesc, ZoneQual};

/// The caller holds a lock on the relation. An AM that answers `Native` takes here the predicate locks its own scan
/// would have taken, since that reader begins none.
pub fn table_scan_road(mcx: Mcx<'_>, relid: Oid) -> PgResult<ScanRoad> {
    Ok(match ::pgrc_front::front_bearing_served(mcx, relid)? {
        true => ScanRoad::Rows,
        false => ScanRoad::Native,
    })
}

/// Once before its first row: an AM that does not take the statement raises its own typed error here, as the
/// statement would at its first row.
pub fn table_modify_admit(rel: &Relation<'_>, op: CmdType, on_conflict: bool) -> PgResult<()> {
    match TableAm::of(rel) {
        Some(TableAm::Pgrcolumnar) => ::pgrc_front::dml::admit(op, on_conflict),
        Some(TableAm::Heap) | None => Ok(()),
    }
}

/// PGRCSERVE M3: an UPDATE / DELETE of `rel` refetches its rows by TID until [`table_modify_refetch_end`].
pub fn table_modify_will_refetch(rel: &Relation<'_>) {
    if TableAm::of(rel) == Some(TableAm::Pgrcolumnar) {
        ::pgrc_am::rowcache::arm(rel.rd_id);
    }
}

pub fn table_modify_refetch_end() {
    ::pgrc_am::rowcache::disarm_all();
}

/// The rows of `rel` the AM placed after their `TABLE_INSERT_DEFER` insert returned (at a later insert, or at
/// `table_finish_bulk_insert`), each handed to `f` in a slot whose `tts_tid` is the row's, for the caller's index
/// entries; the AM forgets them. Heap places every row at once: there are none.
pub fn table_take_placed<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    f: &mut dyn FnMut(&mut SlotData<'mcx>) -> PgResult<()>,
) -> PgResult<()> {
    if am(rel) != TableAm::Pgrcolumnar {
        return Ok(());
    }
    let mut placer = |tid: ItemPointerData| {
        let id = ::pgrc_format::rowid::id_of(::types_tuple::ItemPointerGetBlockNumber(&tid), ::types_tuple::ItemPointerGetOffsetNumber(&tid))?;
        let (block, off) = ::pgrc_am::rowid_alloc::tid_of(::pgrc_am::ingest::place(rel, id)?);
        let mut at = ItemPointerData::invalid();
        ::types_tuple::ItemPointerSet(&mut at, block, off);
        Some(at)
    };
    // a row is placed only after its part's cut: until then (most drains of a statement) nothing is tried
    let place: Option<&mut dyn FnMut(ItemPointerData) -> Option<ItemPointerData>> =
        if ::pgrc_am::ingest::placements_waiting(rel) { Some(&mut placer) } else { None };
    let batches = ::pgrc_front::xact::take_indexed(rel.rd_id, place)?;
    if !::pgrc_am::ingest::bulk_open(rel) && ::pgrc_front::xact::unplaced(rel.rd_id) {
        return Err(Box::new(
            PgError::error(format!("pgrcolumnar: bulk rows of \"{}\" have no placed row ID after their parts were sealed", rel.name()))
                .with_sqlstate(::types_error::ERRCODE_INTERNAL_ERROR),
        ));
    }
    if batches.is_empty() {
        return Ok(());
    }
    let mut slot = ::exectuples::make_tuple_table_slot(mcx, TupleSlotKind::HeapTuple, Some(rel.rd_att.clone()));
    let mut out = Ok(());
    'rows: for (mut rows, tids) in batches {
        for (image, tid) in rows.images_mut().zip(tids) {
            // SAFETY: a heap tuple image, 8-byte aligned, alive until the slot is cleared below.
            let t = unsafe { ::types_tuple::htup::HeapTupleData::from_raw_parts(image.as_ptr(), image.len() as u32, tid, rel.rd_id) };
            ::exectuples::exec_store_heap_tuple(&mut slot, mcx, t);
            let base = slot.base_mut();
            base.tts_tid = tid;
            base.tts_tableOid = rel.rd_id;
            out = f(&mut slot);
            ::exectuples::exec_clear_tuple(&mut slot, mcx);
            if out.is_err() {
                break 'rows;
            }
        }
    }
    out
}

/// The attributes (by `attno - 1`; `None` = every one) the fetches of `scan` must deliver; the others may read
/// NULL. Heap fetches read whole tuples and ignore it; a pgrcolumnar fetch decodes only these (IX1 L0, OC-A a).
pub fn table_index_fetch_set_needed_attrs(scan: &mut IndexFetchTableData<'_>, needed: Option<Box<[bool]>>) {
    match scan {
        IndexFetchTableData::Heap(_) => {}
        IndexFetchTableData::Pgrc(p) => p.set_needed_attrs(needed),
    }
}

/// PGRCSERVE M0 (T5): a reader's conjuncts for the scan's next pass; pgrcolumnar may skip failing rows, heap none.
pub fn table_scan_push_quals(scan: &mut TableScanDesc<'_>, quals: &[ZoneQual]) {
    match scan {
        TableScanDesc::Heap(_) => {}
        TableScanDesc::Pgrcolumnar(c) => c.push_quals(quals),
    }
}

#[cfg(test)]
mod tests {
    use ::mcx::MemoryContext;
    use ::types_rel::RELKIND_RELATION;

    use super::*;
    use crate::{
        table_index_fetch_begin, table_index_fetch_end, HEAP_TABLE_AM_OID, TABLE_INSERT_DEFER,
        TABLE_INSERT_FROZEN, TABLE_INSERT_NO_LOGICAL, TABLE_INSERT_SKIP_FSM,
    };

    #[test]
    fn heap_answers_the_identity() {
        let ctx = MemoryContext::new("test");
        let mcx = ctx.mcx();
        let heap = crate::tests::make(mcx, 1, RELKIND_RELATION, HEAP_TABLE_AM_OID);

        for op in [CmdType::CMD_INSERT, CmdType::CMD_UPDATE, CmdType::CMD_DELETE, CmdType::CMD_MERGE] {
            assert!(table_modify_admit(&heap, op, false).is_ok());
        }
        assert!(table_modify_admit(&heap, CmdType::CMD_INSERT, true).is_ok());

        let mut placed = 0;
        let mut count = |_: &mut SlotData<'_>| -> PgResult<()> {
            placed += 1;
            Ok(())
        };
        table_take_placed(mcx, &heap, &mut count).unwrap();
        assert_eq!(placed, 0);

        let mut fetch = table_index_fetch_begin(&heap).unwrap();
        table_index_fetch_set_needed_attrs(&mut fetch, Some(vec![false].into_boxed_slice()));
        table_index_fetch_set_needed_attrs(&mut fetch, None);
        table_index_fetch_end(fetch);
    }

    // heapam.h's insert options (HEAP_INSERT_SKIP_FSM .. HEAP_INSERT_SPECULATIVE = 0x0010) reach heap_insert
    // unchanged, so the pgrust flag must share no bit with them.
    #[test]
    fn defer_flag_is_clear_of_the_insert_options() {
        assert_eq!(TABLE_INSERT_DEFER, 0x8000);
        for option in [
            TABLE_INSERT_SKIP_FSM,
            TABLE_INSERT_FROZEN,
            TABLE_INSERT_NO_LOGICAL,
            0x0010,
            ::tableam_vocab::TABLE_INSERT_SINGLE_PAGE_EXTEND,
        ] {
            assert_eq!(TABLE_INSERT_DEFER & option, 0);
        }
    }
}
