//! access/table/tableam.c + the tableam.h dispatch layer.
//!
//! C dispatches through `rel->rd_tableam`, a ~45-slot fn-pointer vtable for
//! pluggable AMs. We ship heap only: dispatch is the closed [`TableAm`] enum
//! (rule 4, the types_slot precedent) — each `table_*` wrapper (C surface kept
//! 1:1) is a `match` compiling to a direct call, no vtable load or indirect
//! branch. A second AM = a new variant; exhaustive matches turn every dispatch
//! point into a compile error — never a fn-pointer table. The shared
//! vocabulary (tableam.h/relscan.h types + block parallel-scan helpers) lives
//! in tableam_vocab, below the heap AM crates, and is re-exported here.
//! `mod heap` binds heapam_handler's read/analyze lanes; DML/sample arms
//! stay loud until their heapam phases land.

#![allow(non_snake_case)]
#![allow(non_camel_case_types)]
#![allow(non_upper_case_globals)]

use core::ptr::NonNull;
use std::cell::RefCell;
use std::string::String;

use ::heapam::HeapScanDescData;
use ::heapam_handler::IndexFetchHeapData;
use ::mcx::{Mcx, PgVec};
use ::types_snapshot::IsMVCCSnapshot;
use ::types_core::fmgr::NAMEDATALEN;
use ::types_core::primitive::{BlockNumber, Buffer, ForkNumber, TransactionId};
use ::types_core::xact::{CommandId, TransactionIdIsValid};
use ::types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};
use ::types_rel::{
    Relation, HEAP_DEFAULT_FILLFACTOR, RELKIND_FOREIGN_TABLE, RELKIND_PARTITIONED_TABLE,
    RELKIND_RELATION, RELKIND_TOASTVALUE, RELKIND_VIEW,
};
use ::types_scan::scankey::ScanKeyData;
use ::types_scan::sdir::ScanDirection;
use ::types_slot::{SlotData, TupleSlotKind};
use ::types_storage::RelFileLocator;
use ::types_tuple::{ItemPointerData, ItemPointerGetBlockNumber, ItemPointerGetBlockNumberNoCheck, ItemPointerGetOffsetNumberNoCheck};

pub use ::tableam_vocab::*;
pub use ::pgrc_am::scan::TidNeed as PgrcTidNeed;
pub use ::pgrc_am::scan::TidNeed as ScanTidNeed;

#[cfg(test)]
mod tests;

pub mod write_buffer;

mod am_ext;
pub use am_ext::*;

pub fn init_seams() {
    guc_tables::vars::synchronize_seqscans.install(guc_tables::GucVarAccessors {
        get: synchronize_seqscans,
        set: set_synchronize_seqscans,
    });
    guc_tables::vars::default_table_access_method.install(guc_tables::GucVarAccessors {
        get: || Some(default_table_access_method()),
        set: |v| {
            set_default_table_access_method(v.as_deref().unwrap_or(DEFAULT_TABLE_ACCESS_METHOD))
        },
    });
    guc_tables::hooks::check_default_table_access_method
        .install(check_default_table_access_method);
    tableam_seams::table_tid_get_latest::set(table_tid_get_latest);
    tableam_seams::table_relation_dir_size::set(table_relation_dir_size);
    tableam_seams::database_dir_am_size::set(database_dir_am_size);
}

/// dbsize's pg_database_size and pg_tablespace_size: the bytes in the pgrcolumnar table directories of database
/// directory `db_path` (its flat walk counts only their dirents).
fn database_dir_am_size(db_path: &str) -> PgResult<u64> {
    let mut total = 0u64;
    for (_, name) in ::pgrc_am::dircopy::table_dirs(db_path)? {
        total += ::pgrc_am::dirpath::dir_bytes(&format!("{db_path}/{name}"))?;
    }
    Ok(total)
}

/// dbsize's pg_relation_size (main fork) and pg_table_size: the bytes the AM keeps outside its smgr forks.
/// pgrcolumnar keeps its parts, manifests and sections in its table directory beside an empty main fork; heap keeps
/// none.
fn table_relation_dir_size(rel: &Relation<'_>) -> PgResult<u64> {
    match TableAm::of(rel) {
        Some(TableAm::Pgrcolumnar) => {
            ::pgrc_am::dirpath::dir_bytes(&::pgrc_am::dirpath::table_dir_path(rel.rd_locator.get(), rel.rd_backend))
        }
        _ => Ok(0),
    }
}

// --- The dispatch-facing scan values (closed per-AM extensions, rule 4) ---

// C's TableScanDesc points at the rs_base embedded in the AM's scan state; by
// value the scan IS the AM extension, tagged by the closed set. Single
// variant: the tag is free and every match is a direct call.
pub enum TableScanDesc<'mcx> {
    Heap(HeapScanDescData<'mcx>),
    // The pgrcolumnar AM. Boxed: cold at scan-begin, keeps the enum
    // heap-sized for the hot arm.
    Pgrcolumnar(std::boxed::Box<::pgrc_am::scan::PgrcScanDescData<'mcx>>),
}

impl<'mcx> TableScanDesc<'mcx> {
    #[inline]
    pub fn base(&self) -> &TableScanDescData<'mcx> {
        match self {
            TableScanDesc::Heap(h) => &h.rs_base,
            TableScanDesc::Pgrcolumnar(c) => &c.rs_base,
        }
    }

    #[inline]
    pub fn base_mut(&mut self) -> &mut TableScanDescData<'mcx> {
        match self {
            TableScanDesc::Heap(h) => &mut h.rs_base,
            TableScanDesc::Pgrcolumnar(c) => &mut c.rs_base,
        }
    }
}

// C's IndexFetchTableData base embedded in IndexFetchHeapData, same treatment.
pub enum IndexFetchTableData<'mcx> {
    Heap(IndexFetchHeapData<'mcx>),
    Pgrc(Box<::pgrc_front::ixfetch::PgrcIndexFetch<'mcx>>),
}

impl<'mcx> IndexFetchTableData<'mcx> {
    #[inline]
    pub fn rel(&self) -> &Relation<'mcx> {
        match self {
            IndexFetchTableData::Heap(h) => &h.xs_rel,
            IndexFetchTableData::Pgrc(p) => p.rel(),
        }
    }
}

// C dereferences rd_tableam unconditionally: missing AM = C's NULL crash.
#[inline]
fn am(relation: &Relation<'_>) -> TableAm {
    match TableAm::of(relation) {
        Some(am) => am,
        None => no_table_am(relation),
    }
}

#[cold]
#[inline(never)]
fn no_table_am(relation: &Relation<'_>) -> ! {
    panic!(
        "relation \"{}\" has no supported table access method (relam {}); \
         C would dereference NULL rd_tableam",
        relation.name(),
        relation.rd_rel.relam
    )
}

#[cold]
#[inline(never)]
fn unported(unit: &'static str) -> ! {
    panic!("backend-access-tableam reached unported unit: {unit}")
}

// The shared parallel descriptor of a columnar scan: phs_nallocated is the
// claim cursor; block fields stay unused.
fn columnar_parallelscan_reinitialize(pscan: &ParallelBlockTableScanDescData) {
    pscan.phs_nallocated.store(0, std::sync::atomic::Ordering::SeqCst);
}

#[cold]
#[inline(never)]
fn cb2_refused(what: &'static str) -> Box<PgError> {
    // A typed ERROR, not a panic: DefineIndex fences indexes on pgrc, so these
    // are unreachable today, but a path that does reach one must not take the
    // backend down.
    Box::new(
        PgError::error(format!("pgrcolumnar does not support {what}"))
            .with_sqlstate(::types_error::ERRCODE_FEATURE_NOT_SUPPORTED),
    )
}

// pgrcolumnar arms (lanev3 M3-H; COPY + SELECT only per O-M3-1a —
// docs/design/lanev3-m3-chunks.md §2 M3-H row). Everything of substance
// lives in the pgrc_am crate; these are the dispatch shims.
mod cb2 {
    use super::*;

    pub(super) fn scan_begin<'mcx>(
        mcx: Mcx<'mcx>,
        rel: &Relation<'mcx>,
        snapshot: Snapshot<'mcx>,
        flags: u32,
        parallel: Option<NonNull<ParallelBlockTableScanDescData>>,
    ) -> PgResult<TableScanDesc<'mcx>> {
        let mut scan = ::pgrc_am::scan::PgrcScanDescData::begin(scan_base(mcx, rel, snapshot, flags, parallel))?;
        // IX0 L2: positions are row IDs, resolved through the statement's view; nothing is registered per scan.
        if scan.generation().is_some() {
            super::ensure_pgrc_buffer_hooks();
        }
        attach_filter(mcx, rel, &mut scan)?;
        Ok(TableScanDesc::Pgrcolumnar(scan))
    }

    /// F2 L3: the snapshot's tombstones, unioned into each granule's mask and skipped in segments (each parallel
    /// worker builds its own). First the live segments newer than its view, which hold the transaction's own writes past
    /// a switch its snapshot does not see (design §4.8).
    pub(super) fn attach_filter<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, scan: &mut ::pgrc_am::scan::PgrcScanDescData<'mcx>) -> PgResult<()> {
        let snap = scan.rs_base.rs_snapshot.clone();
        let gen = scan.generation().cloned();
        let newer = ::pgrc_front::ring::newer_segments(rel, gen.as_deref())?;
        scan.add_segments(newer.iter().map(|r| (r.slot, r.relid, r.part_no)));
        if gen.is_none() && newer.is_empty() {
            return ::pgrc_front::filter::predicate_lock(rel, &snap);
        }
        if let Some(f) = ::pgrc_front::filter::build(mcx, rel, gen.as_ref(), &newer, &snap)? {
            scan.filter = Some(Box::new(f));
        }
        Ok(())
    }

    pub(super) fn scan_base<'mcx>(
        mcx: Mcx<'mcx>,
        rel: &Relation<'mcx>,
        snapshot: Snapshot<'mcx>,
        flags: u32,
        parallel: Option<NonNull<ParallelBlockTableScanDescData>>,
    ) -> TableScanDescData<'mcx> {
        TableScanDescData {
            rs_rd: rel.alias(),
            rs_snapshot: snapshot,
            rs_nkeys: 0,
            rs_key: PgVec::new_in(mcx),
            rs_mintid: ItemPointerData::default(),
            rs_maxtid: ItemPointerData::default(),
            rs_flags: flags,
            rs_parallel: parallel,
            rs_am: TableAm::Pgrcolumnar,
        }
    }

    // Part-grain parallel claims over the shared phs_nallocated cursor
    // (the old-AM shape; block fields stay unused).
    pub(super) fn parallelscan_initialize(
        rel: &Relation<'_>,
        pscan: &mut ParallelBlockTableScanDescData,
    ) -> usize {
        pscan.phs_locator = rel.rd_locator.get();
        pscan.phs_syncscan = false;
        pscan.phs_nblocks = 0;
        pscan.phs_startblock.store(0, std::sync::atomic::Ordering::Relaxed);
        pscan.phs_nallocated.store(0, std::sync::atomic::Ordering::Relaxed);
        0
    }

    pub(super) fn tuple_insert<'mcx>(
        mcx: Mcx<'mcx>,
        rel: &Relation<'mcx>,
        slot: &mut SlotData<'mcx>,
        cid: CommandId,
        bulk: bool,
    ) -> PgResult<()> {
        if !bulk {
            return segment_insert(mcx, rel, slot, cid);
        }
        let id = bulk_row(mcx, rel, slot, cid)?;
        // IX0 L3: a bulk row's ctid is its row ID once its part's IDs are granted (IX1's index inserts use it).
        // IXBULKSORT: an ID its part's seal moves (a `sorted_by` table with indexes) is not the row's yet: the row waits,
        // with it, for its placed ID and its index entries (`table_take_placed`), and has no position until then.
        if let Some(::pgrc_am::ingest::RowId { id, handed: true }) = id {
            let mut cx = SCRATCH.take().unwrap_or_else(|| ::mcx::MemoryContext::new_bump("pgrc statement row"));
            let pended = exectuples::exec_copy_slot_heap_tuple(slot, mcx, cx.mcx()).and_then(|t| {
                let pended = ::pgrc_front::xact::pend_handed(rel.rd_id, ::xact::GetCurrentSubTransactionId(), t.image(), handed_tid(id));
                // the image dies with the scratch context's reset
                core::mem::forget(t);
                pended
            });
            cx.reset();
            SCRATCH.set(Some(cx));
            pended?;
        }
        let base = slot.base_mut();
        match id {
            Some(::pgrc_am::ingest::RowId { id, handed: false }) => {
                let (block, off) = ::pgrc_am::rowid_alloc::tid_of(id);
                ::types_tuple::ItemPointerSet(&mut base.tts_tid, block, off);
            }
            Some(_) => ::types_tuple::ItemPointerSetInvalid(&mut base.tts_tid),
            None if rel.rd_rel.relhasindex => {
                return Err(Box::new(PgError::error(format!("pgrcolumnar: a bulk row of \"{}\" got no row ID, which its indexes need", rel.name()))));
            }
            None => ::types_tuple::ItemPointerSetInvalid(&mut base.tts_tid),
        }
        base.tts_tableOid = rel.rd_id;
        Ok(())
    }

    fn handed_tid(id: u64) -> ItemPointerData {
        let (block, off) = ::pgrc_am::rowid_alloc::tid_of(id);
        let mut tid = ItemPointerData::invalid();
        ::types_tuple::ItemPointerSet(&mut tid, block, off);
        tid
    }

    fn bulk_row<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, slot: &mut SlotData<'mcx>, cid: CommandId) -> PgResult<Option<::pgrc_am::ingest::RowId>> {
        // Deform before reading (the old-AM CTAS lesson: buffer/heap-backed
        // slots arrive with tts_nvalid == 0).
        exectuples::exec_materialize_slot(slot, mcx)?;
        // D2973: the inserted row's relation, as heapam_tuple_insert sets it
        slot.base_mut().tts_tableOid = rel.rd_id;
        exectuples::slot_getallattrs(slot);
        if ::pgrc_front::logical::active(rel) {
            super::ensure_pgrc_buffer_hooks();
            ::pgrc_front::logical::bulk_row(mcx, rel, slot)?;
        }
        // The flat backing bytes of the just-materialized tuple: the
        // independent readable-byte witness that lets `ingest_row`/`raw_row`
        // bound each by-reference datum's header-claimed size against the
        // bytes the tuple actually backs (never trusting an on-tuple varlena
        // header to size a read — the OOB/info-leak class).
        let image = exectuples::slot_materialized_image(slot);
        let base = slot.base();
        ::pgrc_am::ingest::ingest_row(rel, cid, &base.tts_values, &base.tts_isnull, image)
    }

    pub(super) fn getnextslot<'mcx>(mcx: Mcx<'mcx>, c: &mut ::pgrc_am::scan::PgrcScanDescData<'mcx>, slot: &mut SlotData<'mcx>) -> PgResult<bool> {
        if c.segment.is_none() && c.getnextslot(slot)? {
            return Ok(true);
        }
        loop {
            if let Some(seg) = c.segment.as_mut() {
                if seg.next(slot)? {
                    return Ok(true);
                }
                end_segment(c)?;
            }
            let Some((segslot, relid, part_no)) = c.claim_segment() else {
                slot.base_mut().mark_empty();
                return Ok(false);
            };
            let snapshot = match c.rs_base.rs_flags & SO_PGRC_INDEX_BUILD {
                0 => c.rs_base.rs_snapshot.clone(),
                _ => ::pgrc_front::segscan::index_build_snapshot(mcx, &c.rs_base.rs_rd),
            };
            let parent = c.rs_base.rs_rd.rd_id;
            let deleted = match c.filter.as_ref() {
                Some(f) => ::mcx::stdvec::try_to_vec(f.segment_positions(segslot), "pgrc segment scan")?,
                None => Vec::new(),
            };
            let busy = match c.filter.as_ref() {
                Some(f) => ::mcx::stdvec::try_to_vec(f.busy_segment(segslot), "pgrc segment scan")?,
                None => Vec::new(),
            };
            c.segment = Some(Box::new(::pgrc_front::segscan::SegScan::begin(mcx, &c.rs_base.rs_rd, relid, segslot, part_no, snapshot, parent, deleted)?.with_busy(busy)));
        }
    }

    pub(super) fn end_segment(c: &mut ::pgrc_am::scan::PgrcScanDescData<'_>) -> PgResult<()> {
        let Some(s) = c.segment.take() else { return Ok(()) };
        c.stats.segment_rows(s.returned());
        s.end()
    }

    /// F1 L7 routing: a flagged statement (ModifyTable, COPY FROM) buffers; any other `BulkInsertState` or options
    /// (CTAS, refresh, rewrites) is the bulk lane; a bare insert (single-row INSERT, apply worker, Gravity) is one heap
    /// insert into the active segment.
    pub(super) fn insert<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, slot: &mut SlotData<'mcx>, cid: CommandId, options: i32, bistate: bool) -> PgResult<()> {
        ::pgrc_front::filter::conflict_in(rel)?;
        if options & ::tableam_vocab::TABLE_INSERT_DEFER != 0 {
            return buffered_insert(mcx, rel, slot, cid);
        }
        tuple_insert(mcx, rel, slot, cid, bistate || options != 0)
    }

    thread_local! {
        static SCRATCH: std::cell::Cell<Option<::mcx::MemoryContext>> = const { std::cell::Cell::new(None) };
    }

    /// The row's heap tuple image joins the statement buffer; a full buffer replays into the bulk lane, which takes
    /// the rest of the statement. The slot has no position yet (`tts_tid` invalid).
    fn buffered_insert<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, slot: &mut SlotData<'mcx>, cid: CommandId) -> PgResult<()> {
        let subid = ::xact::GetCurrentSubTransactionId();
        if ::pgrc_front::xact::switched(rel.rd_id, cid, subid) {
            return tuple_insert(mcx, rel, slot, cid, true);
        }
        super::ensure_pgrc_buffer_hooks();
        let mut cx = SCRATCH.take().unwrap_or_else(|| ::mcx::MemoryContext::new_bump("pgrc statement row"));
        let full = exectuples::exec_copy_slot_heap_tuple(slot, mcx, cx.mcx()).and_then(|t| {
            let full = ::pgrc_front::xact::push(rel.rd_id, cid, subid, t.image());
            // the image dies with the scratch context's reset
            core::mem::forget(t);
            full
        });
        cx.reset();
        SCRATCH.set(Some(cx));
        let base = slot.base_mut();
        base.tts_tid = ItemPointerData::invalid();
        base.tts_tableOid = rel.rd_id;
        if full? {
            replay(mcx, rel, cid, subid, ::pgrc_front::xact::spill(rel.rd_id, cid, subid))?;
        }
        Ok(())
    }

    /// A table with indexes keeps them, with their IDs, for ModifyTable's index inserts (`table_take_placed`); IDs
    /// their parts' seals move are kept as handed, to be placed.
    fn replay<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, cid: CommandId, subid: u32, mut rows: ::pgrc_front::xact::Rows) -> PgResult<()> {
        let mut slot = exectuples::make_tuple_table_slot(mcx, TupleSlotKind::HeapTuple, Some(rel.rd_att.clone()));
        let mut tids = ::mcx::stdvec::try_vec(if rel.rd_rel.relhasindex { rows.len() } else { 0 }, "pgrc buffered replay")?;
        let mut handed = false;
        let mut out = Ok(());
        for image in rows.images_mut() {
            // SAFETY: a heap tuple image, 8-byte aligned, alive until the slot is cleared below.
            let t = unsafe { ::types_tuple::htup::HeapTupleData::from_raw_parts(image.as_ptr(), image.len() as u32, ItemPointerData::invalid(), rel.rd_id) };
            exectuples::exec_store_heap_tuple(&mut slot, mcx, t);
            let id = match bulk_row(mcx, rel, &mut slot, cid) {
                Ok(id) => id,
                Err(e) => {
                    out = Err(e);
                    break;
                }
            };
            if rel.rd_rel.relhasindex {
                let Some(id) = id else {
                    out = Err(Box::new(PgError::error(format!("pgrcolumnar: a bulk row of \"{}\" got no row ID, which its indexes need", rel.name()))));
                    break;
                };
                // one writer's rows: all handed or none
                handed = id.handed;
                tids.push(handed_tid(id.id));
            }
        }
        exectuples::exec_clear_tuple(&mut slot, mcx);
        out?;
        if rel.rd_rel.relhasindex {
            ::pgrc_front::xact::pend_indexed(rel.rd_id, subid, rows, tids, handed);
        }
        Ok(())
    }

    const FLUSH_ROWS: usize = 1000;

    pub(super) fn flush(rel: &Relation<'_>) -> PgResult<()> {
        let stmts = ::pgrc_front::xact::take(rel.rd_id, ::xact::GetCurrentSubTransactionId());
        if stmts.is_empty() {
            return Ok(());
        }
        let cx = ::mcx::MemoryContext::new("pgrc statement flush");
        let parent = ::pgrc_front::open_segment(cx.mcx(), rel.rd_id)?;
        let subid = ::xact::GetCurrentSubTransactionId();
        let mut out = Ok(());
        for (cid, mut rows) in stmts {
            let mut tids = Vec::new();
            out = flush_rows(cx.mcx(), &parent, cid, &mut rows, &mut tids);
            if out.is_err() {
                break;
            }
            if parent.rd_rel.relhasindex {
                ::pgrc_front::xact::pend_indexed(parent.rd_id, subid, rows, tids, false);
            }
        }
        parent.close(::types_rel::NoLock)?;
        out
    }

    fn flush_rows<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, cid: CommandId, rows: &mut ::pgrc_front::xact::Rows, tids: &mut Vec<ItemPointerData>) -> PgResult<()> {
        let db = ::init_small::globals::MyDatabaseId();
        ::pgrc_front::route::backpressure(db, rel, cid)?;
        let seg = ::pgrc_front::route::segment(mcx, rel)?;
        let segrel = ::pgrc_front::open_segment(mcx, seg.relid)?;
        let mut bistate = ::heapam::GetBulkInsertState();
        let mut slots: Vec<SlotData<'mcx>> = Vec::new();
        let mut images = rows.images_mut().peekable();
        let mut out = Ok(());
        while images.peek().is_some() {
            let mut n = 0;
            for image in images.by_ref().take(FLUSH_ROWS) {
                if slots.len() == n {
                    slots.push(exectuples::make_tuple_table_slot(mcx, TupleSlotKind::HeapTuple, Some(segrel.rd_att.clone())));
                }
                // SAFETY: a heap tuple image, 8-byte aligned, alive until the slots are cleared below.
                let t = unsafe { ::types_tuple::htup::HeapTupleData::from_raw_parts(image.as_ptr(), image.len() as u32, ItemPointerData::invalid(), segrel.rd_id) };
                exectuples::exec_store_heap_tuple(&mut slots[n], mcx, t);
                n += 1;
            }
            let mut refs: Vec<&mut SlotData<'mcx>> = slots[..n].iter_mut().collect();
            out = super::heap::multi_insert(mcx, &segrel, &mut refs, cid, 0, Some(&mut bistate)).and_then(|()| {
                // AUTOSTATS: the parent's count (the segment's own multi-insert counted the segment)
                ::pgrc_front::stats::inserted(rel, n as u64);
                let segloc = segrel.rd_locator.get();
                let mut marked = u32::MAX;
                for s in &slots[..n] {
                    let tid = s.base().tts_tid;
                    if ItemPointerGetBlockNumber(&tid) != marked {
                        marked = ItemPointerGetBlockNumber(&tid);
                        ::pgrc_marks::written((segloc.spcOid, segloc.dbOid, segloc.relNumber), marked);
                    }
                    // IX0: each row's extent is reserved (its ID homed) before the statement ends
                    let id = ::pgrc_front::route::row_id(rel, &seg, ItemPointerGetBlockNumber(&tid), ItemPointerGetOffsetNumberNoCheck(&tid))?;
                    if rel.rd_rel.relhasindex {
                        let (block, off) = ::pgrc_am::rowid_alloc::tid_of(id);
                        let mut t = ItemPointerData::invalid();
                        ::types_tuple::ItemPointerSet(&mut t, block, off);
                        tids.push(t);
                    }
                    ::pgrc_front::maint::inserted(db, rel.rd_id, seg.relid, ItemPointerGetBlockNumber(&tid));
                }
                Ok(())
            });
            for s in slots.iter_mut() {
                exectuples::exec_clear_tuple(s, mcx);
            }
            if out.is_err() {
                break;
            }
        }
        drop(bistate);
        segrel.close(::types_rel::NoLock)?;
        out
    }

    fn segment_insert<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, slot: &mut SlotData<'mcx>, cid: CommandId) -> PgResult<()> {
        ::pgrc_front::route::insert_row(mcx, rel, slot, cid, 0)?;
        // AUTOSTATS: the parent's count (the segment's own heap insert counted the segment)
        ::pgrc_front::stats::inserted(rel, 1);
        Ok(())
    }
}

// heapam_handler.c's heapam_methods, bound directly onto heapam /
// heapam_handler.
mod heap {
    use super::*;

    pub(super) fn slot_callbacks(_rel: &Relation<'_>) -> TupleSlotKind {
        TupleSlotKind::BufferHeapTuple
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn scan_begin<'mcx>(
        mcx: Mcx<'mcx>,
        rel: &Relation<'mcx>,
        snapshot: Snapshot<'mcx>,
        nkeys: i32,
        key: PgVec<'mcx, ScanKeyData>,
        parallel: Option<NonNull<ParallelBlockTableScanDescData>>,
        flags: u32,
    ) -> PgResult<TableScanDesc<'mcx>> {
        Ok(TableScanDesc::Heap(::heapam::heap_beginscan(
            mcx, rel, snapshot, nkeys, key, parallel, flags,
        )?))
    }

    pub(super) fn scan_end(scan: HeapScanDescData<'_>) -> PgResult<()> {
        ::heapam::heap_endscan(scan)
    }

    pub(super) fn scan_rescan(
        scan: &mut HeapScanDescData<'_>,
        key: Option<&[ScanKeyData]>,
        set_params: bool,
        allow_strat: bool,
        allow_sync: bool,
        allow_pagemode: bool,
    ) -> PgResult<()> {
        ::heapam::heap_rescan(scan, key, set_params, allow_strat, allow_sync, allow_pagemode)
    }

    #[inline]
    pub(super) fn scan_getnextslot<'mcx>(
        mcx: Mcx<'mcx>,
        scan: &mut HeapScanDescData<'mcx>,
        direction: ScanDirection,
        slot: &mut SlotData<'mcx>,
    ) -> PgResult<bool> {
        ::heapam::heap_getnextslot(mcx, scan, direction, slot)
    }

    pub(super) fn scan_set_tidrange(
        scan: &mut HeapScanDescData<'_>,
        mintid: &ItemPointerData,
        maxtid: &ItemPointerData,
    ) {
        ::heapam::heap_set_tidrange(scan, mintid, maxtid);
    }

    pub(super) fn scan_getnextslot_tidrange<'mcx>(
        mcx: Mcx<'mcx>,
        scan: &mut HeapScanDescData<'mcx>,
        direction: ScanDirection,
        slot: &mut SlotData<'mcx>,
    ) -> PgResult<bool> {
        ::heapam::heap_getnextslot_tidrange(mcx, scan, direction, slot)
    }

    pub(super) fn parallelscan_estimate(rel: &Relation<'_>) -> usize {
        table_block_parallelscan_estimate(rel)
    }

    pub(super) fn parallelscan_initialize(
        rel: &Relation<'_>,
        pscan: &mut ParallelBlockTableScanDescData,
    ) -> PgResult<usize> {
        table_block_parallelscan_initialize(rel, pscan)
    }

    pub(super) fn parallelscan_reinitialize(
        rel: &Relation<'_>,
        pscan: &ParallelBlockTableScanDescData,
    ) {
        table_block_parallelscan_reinitialize(rel, pscan);
    }

    pub(super) fn index_delete_tuples<'mcx>(
        mcx: Mcx<'mcx>,
        rel: &Relation<'mcx>,
        delstate: &mut TM_IndexDeleteOp<'mcx>,
    ) -> PgResult<TransactionId> {
        ::heapam::heap_index_delete_tuples(mcx, rel, delstate)
    }

    // heapam_tuple_insert (heapam_handler.c): fetch the slot's heap tuple in
    // place (virtual/minimal sources take the ExecFetchSlotHeapTuple copy
    // arm), heap_insert it, copy t_self back.
    pub(super) fn tuple_insert<'mcx>(
        mcx: Mcx<'mcx>,
        rel: &Relation<'mcx>,
        slot: &mut SlotData<'mcx>,
        cid: CommandId,
        options: i32,
        bistate: Option<&mut BulkInsertStateData>,
    ) -> PgResult<()> {
        exectuples::exec_materialize_slot(slot, mcx)?;
        slot.base_mut().tts_tableOid = rel.rd_id;
        let t_self = match slot {
            SlotData::Heap(h) => {
                let tuple = h.tuple.as_mut().expect("materialized heap slot holds a tuple");
                tuple.t_tableOid = rel.rd_id;
                ::heapam::heap_insert(rel, tuple, cid, options, bistate)?;
                tuple.t_self
            }
            SlotData::BufferHeap(b) => {
                let tuple = b.base.tuple.as_mut().expect("materialized heap slot holds a tuple");
                tuple.t_tableOid = rel.rd_id;
                ::heapam::heap_insert(rel, tuple, cid, options, bistate)?;
                tuple.t_self
            }
            // ExecFetchSlotHeapTuple copy arm (virtual/minimal source slots,
            // e.g. multi-row VALUES routed into partitions).
            _ => {
                let mut tuple = exectuples::exec_copy_slot_heap_tuple(slot, mcx, mcx)?;
                tuple.t_tableOid = rel.rd_id;
                ::heapam::heap_insert(rel, &mut tuple, cid, options, bistate)?;
                tuple.t_self
            }
        };
        slot.base_mut().tts_tid = t_self;
        Ok(())
    }

    // heapam_tuple_insert_speculative (heapam_handler.c): the token is stamped
    // into t_ctid before insert; hio asserts it when placing the tuple.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn tuple_insert_speculative<'mcx>(
        mcx: Mcx<'mcx>,
        rel: &Relation<'mcx>,
        slot: &mut SlotData<'mcx>,
        cid: CommandId,
        options: i32,
        bistate: Option<&mut BulkInsertStateData>,
        spec_token: u32,
    ) -> PgResult<()> {
        debug_assert!(bistate.is_none(), "GetBulkInsertState lane (COPY) not ported");
        exectuples::exec_materialize_slot(slot, mcx)?;
        slot.base_mut().tts_tableOid = rel.rd_id;
        let t_self = match slot {
            SlotData::Heap(h) => {
                let tuple = h.tuple.as_mut().expect("materialized heap slot holds a tuple");
                tuple.t_tableOid = rel.rd_id;
                tuple.t_data_mut().set_speculative_token(spec_token);
                ::heapam::heap_insert(rel, tuple, cid, options | ::heapam::hio::HEAP_INSERT_SPECULATIVE, bistate)?;
                tuple.t_self
            }
            SlotData::BufferHeap(b) => {
                let tuple = b.base.tuple.as_mut().expect("materialized heap slot holds a tuple");
                tuple.t_tableOid = rel.rd_id;
                tuple.t_data_mut().set_speculative_token(spec_token);
                ::heapam::heap_insert(rel, tuple, cid, options | ::heapam::hio::HEAP_INSERT_SPECULATIVE, bistate)?;
                tuple.t_self
            }
            // ExecFetchSlotHeapTuple copy arm (virtual/minimal source slots,
            // e.g. multi-row VALUES routed into partitions on ON CONFLICT).
            _ => {
                let mut tuple = exectuples::exec_copy_slot_heap_tuple(slot, mcx, mcx)?;
                tuple.t_tableOid = rel.rd_id;
                tuple.t_data_mut().set_speculative_token(spec_token);
                ::heapam::heap_insert(rel, &mut tuple, cid, options | ::heapam::hio::HEAP_INSERT_SPECULATIVE, bistate)?;
                tuple.t_self
            }
        };
        slot.base_mut().tts_tid = t_self;
        Ok(())
    }

    pub(super) fn tuple_complete_speculative<'mcx>(
        _mcx: Mcx<'mcx>,
        rel: &Relation<'mcx>,
        slot: &mut SlotData<'mcx>,
        _spec_token: u32,
        succeeded: bool,
    ) -> PgResult<()> {
        let tid = slot.base().tts_tid;
        if succeeded {
            ::heapam::heap_finish_speculative(rel, &tid)
        } else {
            ::heapam::heap_abort_speculative(rel, &tid)
        }
    }

    // heapam_multi_insert (heapam_handler.c).
    pub(super) fn multi_insert<'mcx>(
        mcx: Mcx<'mcx>,
        rel: &Relation<'mcx>,
        slots: &mut [&mut SlotData<'mcx>],
        cid: CommandId,
        options: i32,
        bistate: Option<&mut BulkInsertStateData>,
    ) -> PgResult<()> {
        ::heapam::heap_multi_insert(mcx, rel, slots, cid, options, bistate)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn tuple_delete<'mcx>(
        _mcx: Mcx<'mcx>,
        rel: &Relation<'mcx>,
        tid: &ItemPointerData,
        cid: CommandId,
        _snapshot: &Snapshot<'mcx>,
        crosscheck: &Snapshot<'mcx>,
        wait: bool,
        tmfd: &mut TM_FailureData,
        changing_part: bool,
    ) -> PgResult<TM_Result> {
        ::heapam::heap_delete(rel, tid, cid, crosscheck.as_deref(), wait, tmfd, changing_part)
    }

    // heapam_tuple_update: the TU_* verdict passes through unfiltered.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn tuple_update<'mcx>(
        mcx: Mcx<'mcx>,
        rel: &Relation<'mcx>,
        otid: &ItemPointerData,
        slot: &mut SlotData<'mcx>,
        cid: CommandId,
        _snapshot: &Snapshot<'mcx>,
        crosscheck: &Snapshot<'mcx>,
        wait: bool,
        tmfd: &mut TM_FailureData,
        lockmode: &mut LockTupleMode,
        update_indexes: &mut TU_UpdateIndexes,
    ) -> PgResult<TM_Result> {
        exectuples::exec_materialize_slot(slot, mcx)?;
        slot.base_mut().tts_tableOid = rel.rd_id;
        let (result, t_self) = match slot {
            SlotData::Heap(_) | SlotData::BufferHeap(_) => {
                let tuple = match slot {
                    SlotData::Heap(h) => h.tuple.as_mut(),
                    SlotData::BufferHeap(b) => b.base.tuple.as_mut(),
                    _ => unreachable!(),
                }
                .expect("materialized heap slot holds a tuple");
                tuple.t_tableOid = rel.rd_id;
                let result = ::heapam::heap_update(
                    rel,
                    otid,
                    tuple,
                    cid,
                    crosscheck.as_deref(),
                    wait,
                    tmfd,
                    lockmode,
                    update_indexes,
                )?;
                (result, tuple.t_self)
            }
            // ExecFetchSlotHeapTuple copy arm (virtual source slots, e.g. the
            // ON CONFLICT DO UPDATE projection over a partitioned target).
            _ => {
                let mut tuple = exectuples::exec_copy_slot_heap_tuple(slot, mcx, mcx)?;
                tuple.t_tableOid = rel.rd_id;
                let result = ::heapam::heap_update(
                    rel,
                    otid,
                    &mut tuple,
                    cid,
                    crosscheck.as_deref(),
                    wait,
                    tmfd,
                    lockmode,
                    update_indexes,
                )?;
                (result, tuple.t_self)
            }
        };
        slot.base_mut().tts_tid = t_self;
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn tuple_lock<'mcx>(
        mcx: Mcx<'mcx>,
        rel: &Relation<'mcx>,
        tid: &ItemPointerData,
        snapshot: &Snapshot<'mcx>,
        slot: &mut SlotData<'mcx>,
        cid: CommandId,
        mode: LockTupleMode,
        wait_policy: LockWaitPolicy,
        flags: u8,
        tmfd: &mut TM_FailureData,
    ) -> PgResult<TM_Result> {
        ::heapam_handler::heapam_tuple_lock(
            mcx, rel, tid, snapshot, slot, cid, mode, wait_policy, flags, tmfd,
        )
    }

    pub(super) fn relation_set_new_filelocator(
        rel: &Relation<'_>,
        newrlocator: &RelFileLocator,
        persistence: i8,
    ) -> PgResult<(TransactionId, TransactionId)> {
        let freeze_xid = procarray::RecentXmin();
        let min_multi = multixact::GetOldestMultiXactId()?;
        let srel =
            catalog_storage::RelationCreateStorage(*newrlocator, persistence as u8, true)?;
        if persistence as u8 == ::types_core::catalog::RELPERSISTENCE_UNLOGGED {
            debug_assert!(
                rel.rd_rel.relkind == RELKIND_RELATION
                    || rel.rd_rel.relkind == RELKIND_TOASTVALUE
            );
            smgr::smgrcreate(srel, ForkNumber::INIT_FORKNUM, false)?;
            catalog_storage::log_smgrcreate(newrlocator, ForkNumber::INIT_FORKNUM)?;
        }
        smgr::smgrclose(srel)?;
        Ok((freeze_xid, min_multi))
    }

    pub(super) fn relation_copy_data(
        rel: &Relation<'_>,
        newrlocator: &RelFileLocator,
    ) -> PgResult<()> {
        let src = ::types_storage::RelFileLocatorBackend {
            locator: rel.rd_locator.get(),
            backend: rel.rd_backend,
        };
        smgr::smgropen(src.locator, src.backend)?;
        ::bufmgr_seams::flush_relation_buffers::call(src)?;
        let persistence = rel.rd_rel.relpersistence;
        let dstrel = catalog_storage::RelationCreateStorage(*newrlocator, persistence, true)?;
        catalog_storage::RelationCopyStorage(
            src,
            dstrel,
            ForkNumber::MAIN_FORKNUM,
            persistence,
        )?;
        for fork_i in ForkNumber::MAIN_FORKNUM as i32 + 1..=::types_core::MAX_FORKNUM as i32 {
            let fork = ForkNumber::from_i32(fork_i).expect("valid fork number");
            if smgr::smgrexists(src, fork)? {
                smgr::smgrcreate(dstrel, fork, false)?;
                if rel.is_permanent()
                    || (persistence == ::types_core::catalog::RELPERSISTENCE_UNLOGGED
                        && fork == ForkNumber::INIT_FORKNUM)
                {
                    catalog_storage::log_smgrcreate(newrlocator, fork)?;
                }
                catalog_storage::RelationCopyStorage(src, dstrel, fork, persistence)?;
            }
        }
        catalog_storage::RelationDropStorage(rel)?;
        smgr::smgrclose(dstrel)
    }

    pub(super) fn relation_nontransactional_truncate(rel: &Relation<'_>) -> PgResult<()> {
        catalog_storage::RelationTruncate(rel, 0)
    }

    pub(super) fn relation_needs_toast_table(rel: &Relation<'_>) -> bool {
        use ::types_tuple::tupmacs::att_nominal_alignby;
        use ::types_tuple::{BITMAPLEN, MAXALIGN, SizeofHeapTupleHeader, TYPSTORAGE_PLAIN};
        const ATTRIBUTE_GENERATED_VIRTUAL: i8 = b'v' as i8;

        let tupdesc = &rel.rd_att;
        // C (-fwrapv) accumulates in int32 and compares the int32 total
        // against a uintptr_t threshold, so 128 x varchar(8388607) wraps to
        // a 40-byte tuple (no TOAST table) and a negative total means TOAST.
        let mut data_length: i32 = 0;
        let mut maxlength_unknown = false;
        let mut has_toastable_attrs = false;
        for i in 0..tupdesc.natts as usize {
            let att = tupdesc.attr(i);
            if att.attisdropped || att.attgenerated == ATTRIBUTE_GENERATED_VIRTUAL {
                continue;
            }
            data_length = att_nominal_alignby(
                data_length as i64 as usize,
                tupdesc.compact_attr(i).attalignby,
            ) as i32;
            if att.attlen > 0 {
                data_length = data_length.wrapping_add(att.attlen as i32);
            } else {
                let maxlen = format_type::type_maximum_size(att.atttypid, att.atttypmod);
                if maxlen < 0 {
                    maxlength_unknown = true;
                } else {
                    data_length = data_length.wrapping_add(maxlen);
                }
                if att.attstorage != TYPSTORAGE_PLAIN {
                    has_toastable_attrs = true;
                }
            }
        }
        if !has_toastable_attrs {
            return false;
        }
        if maxlength_unknown {
            return true;
        }
        let tuple_length = MAXALIGN(SizeofHeapTupleHeader + BITMAPLEN(tupdesc.natts) as usize)
            .wrapping_add(MAXALIGN(data_length as i64 as usize)) as i32;
        tuple_length as i64 as usize > ::heapam::dml::TOAST_TUPLE_THRESHOLD
    }

    pub(super) fn relation_toast_am(rel: &Relation<'_>) -> ::types_core::Oid {
        rel.rd_rel.relam
    }

    pub(super) fn relation_size(rel: &Relation<'_>, fork_number: ForkNumber) -> PgResult<u64> {
        table_block_relation_size(rel, fork_number)
    }

    // next_buffer replaces C's read stream: already-pinned buffers, Invalid = done.
    pub(super) fn scan_analyze_next_block<'mcx>(
        _mcx: Mcx<'mcx>,
        scan: &mut HeapScanDescData<'mcx>,
        next_buffer: &mut dyn FnMut() -> PgResult<Buffer>,
    ) -> PgResult<bool> {
        let Some(pin) = bufmgr_seams::BufferPin::adopt(next_buffer()?) else {
            return Ok(false);
        };
        scan.rs_cblock = pin.block_number();
        scan.rs_cbuf = Some(pin);
        scan.rs_cindex = 0;
        scan.rs_ntuples = 0;
        Ok(true)
    }

    // rs_vistuples entry of a dead line pointer; a plain offset is a sampled tuple.
    const ANALYZE_DEAD_ITEM: ::types_core::primitive::OffsetNumber = 0x8000;

    // C holds the share lock across the whole page; here the first call classifies
    // every line pointer under one lock hold and later calls serve rs_vistuples under the pin.
    pub(super) fn scan_analyze_next_tuple<'mcx>(
        mcx: Mcx<'mcx>,
        scan: &mut HeapScanDescData<'mcx>,
        oldest_xmin: TransactionId,
        liverows: &mut f64,
        deadrows: &mut f64,
        slot: &mut SlotData<'mcx>,
    ) -> PgResult<bool> {
        let pin = scan.rs_cbuf.take().expect("analyze scan positioned without a buffer");
        let rd_id = scan.rs_base.rs_rd.rd_id;
        let block = scan.rs_cblock;
        if scan.rs_ntuples == 0 && scan.rs_cindex == 0 {
            match analyze_classify_page(scan, &pin, oldest_xmin) {
                Ok(n) => scan.rs_ntuples = n,
                Err(e) => {
                    pin.release();
                    return Err(e);
                }
            }
        }
        let page = pin.page();
        while scan.rs_cindex < scan.rs_ntuples {
            let entry = scan.rs_vistuples[scan.rs_cindex as usize];
            scan.rs_cindex += 1;
            if entry & ANALYZE_DEAD_ITEM != 0 {
                *deadrows += 1.0;
                continue;
            }
            *liverows += 1.0;
            let (ptr, len) = page.item_raw(page.item_id(entry));
            // SAFETY: classified under the lock; the pin keeps it in place (pruning needs a cleanup lock).
            let tuple = unsafe {
                ::types_tuple::HeapTupleData::from_raw_parts(
                    ptr,
                    len,
                    ItemPointerData::new(block, entry),
                    rd_id,
                )
            };
            exectuples::exec_store_buffer_heap_tuple(slot, mcx, tuple, pin.buffer());
            scan.rs_cbuf = Some(pin);
            return Ok(true);
        }
        pin.release();
        exectuples::exec_clear_tuple(slot, mcx);
        Ok(false)
    }

    fn analyze_classify_page(
        scan: &mut HeapScanDescData<'_>,
        pin: &bufmgr_seams::BufferPin,
        oldest_xmin: TransactionId,
    ) -> PgResult<u32> {
        use ::types_snapshot::HTSV_Result::*;
        let rd_id = scan.rs_base.rs_rd.rd_id;
        let block = scan.rs_cblock;
        let _lock = pin.lock_share()?;
        let page = pin.page();
        let maxoffset = page.max_offset_number();
        if maxoffset as usize > ::types_storage::bufpage::MaxHeapTuplesPerPage {
            return Err(heapam::line_pointer_count_corrupted(maxoffset));
        }
        let mut n: u32 = 0;
        let mut offnum = ::types_tuple::FirstOffsetNumber;
        while offnum <= maxoffset {
            let itemid = page.item_id(offnum);
            if !itemid.is_normal() {
                if itemid.is_dead() {
                    scan.rs_vistuples[n as usize] = offnum | ANALYZE_DEAD_ITEM;
                    n += 1;
                }
                offnum += 1;
                continue;
            }
            let (ptr, len) = page.item_raw(itemid);
            // SAFETY: normal line pointer on the page pinned by `pin`.
            let mut targtuple = unsafe {
                ::types_tuple::HeapTupleData::from_raw_parts(
                    ptr,
                    len,
                    ItemPointerData::new(block, offnum),
                    rd_id,
                )
            };
            let sample_it = match heapam_visibility_seams::heap_tuple_satisfies_vacuum::call(
                &mut targtuple,
                oldest_xmin,
                pin.buffer(),
            )? {
                HEAPTUPLE_LIVE => true,
                HEAPTUPLE_DEAD | HEAPTUPLE_RECENTLY_DEAD => false,
                HEAPTUPLE_INSERT_IN_PROGRESS => {
                    if xact_seams::transaction_id_is_current_transaction_id::call(
                        targtuple.t_data().xmin(),
                    ) {
                        true
                    } else {
                        offnum += 1;
                        continue;
                    }
                }
                HEAPTUPLE_DELETE_IN_PROGRESS => {
                    !xact_seams::transaction_id_is_current_transaction_id::call(
                        ::heapam::HeapTupleHeaderGetUpdateXid(targtuple.t_data())?,
                    )
                }
            };
            scan.rs_vistuples[n as usize] = if sample_it { offnum } else { offnum | ANALYZE_DEAD_ITEM };
            n += 1;
            offnum += 1;
        }
        Ok(n)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn scan_bitmap_next_tuple<'mcx>(
        mcx: Mcx<'mcx>,
        scan: &mut HeapScanDescData<'mcx>,
        tbm: Option<&tidbitmap::TIDBitmap<'_>>,
        iterator: &mut tidbitmap::TbmIterator,
        slot: &mut SlotData<'mcx>,
        recheck: &mut bool,
        lossy_pages: &mut u64,
        exact_pages: &mut u64,
    ) -> PgResult<bool> {
        heapam::bitmap::heap_scan_bitmap_next_tuple(
            mcx, scan, tbm, iterator, slot, recheck, lossy_pages, exact_pages,
        )
    }

    pub(super) fn scan_sample_next_block<'mcx>(
        _mcx: Mcx<'mcx>,
        scan: &mut HeapScanDescData<'mcx>,
        scanstate: &mut dyn SampleScanDriver,
        donetuples: i64,
    ) -> PgResult<bool> {
        heapam::sample::heap_scan_sample_next_block(scan, scanstate, donetuples)
    }

    pub(super) fn scan_sample_next_tuple<'mcx>(
        mcx: Mcx<'mcx>,
        scan: &mut HeapScanDescData<'mcx>,
        scanstate: &mut dyn SampleScanDriver,
        donetuples: i64,
        slot: &mut SlotData<'mcx>,
    ) -> PgResult<bool> {
        heapam::sample::heap_scan_sample_next_tuple(mcx, scan, scanstate, donetuples, slot)
    }
}

// --- GUC variables (tableam.c) + check hook (tableamapi.c) ---

pub const DEFAULT_TABLE_ACCESS_METHOD: &str = "heap";

thread_local! {
    // Cold GUC store: bare String = guc.c's malloc'd string, outlives any mcx.
    static DEFAULT_TABLE_ACCESS_METHOD_GUC: RefCell<String> =
        RefCell::new(String::from(DEFAULT_TABLE_ACCESS_METHOD));
}

pub fn default_table_access_method() -> String {
    DEFAULT_TABLE_ACCESS_METHOD_GUC.with(|v| v.borrow().clone())
}

pub fn set_default_table_access_method(value: &str) {
    DEFAULT_TABLE_ACCESS_METHOD_GUC.with(|v| {
        let mut s = v.borrow_mut();
        s.clear();
        s.push_str(value);
    });
}

// check_default_table_access_method (tableamapi.c:102).
fn check_default_table_access_method(
    newval: &mut Option<String>,
    _extra: &mut Option<guc_tables::GucHookExtra>,
    source: ::types_guc::GucSource,
) -> PgResult<bool> {
    let errdetail = |d: String| {
        if guc_seams::guc_check_errdetail::is_installed() {
            guc_seams::guc_check_errdetail::call(d);
        }
    };
    let name = newval.as_deref().unwrap_or("");

    if name.is_empty() {
        errdetail("\"default_table_access_method\" cannot be empty.".to_string());
        return Ok(false);
    }

    if name.len() >= NAMEDATALEN as usize {
        errdetail(format!(
            "\"default_table_access_method\" is too long (maximum {} characters).",
            NAMEDATALEN - 1
        ));
        return Ok(false);
    }

    // C accepts on faith outside a transaction or before database selection;
    // uninstalled seams (unit tests) land on the same path.
    if xact_seams::is_transaction_state::is_installed()
        && tableam_seams::get_table_am_oid::is_installed()
        && xact_seams::is_transaction_state::call()
        && init_small::globals::MyDatabaseId() != ::types_core::InvalidOid
    {
        // A wrong-type AM (e.g. an index AM) raises hard from get_table_am_oid.
        if tableam_seams::get_table_am_oid::call(name, true)? == ::types_core::InvalidOid {
            if source == ::types_guc::GucSource::PGC_S_TEST {
                elog_seams::ereport_msg::call(
                    ::types_error::NOTICE,
                    format!("table access method \"{name}\" does not exist"),
                    None,
                )?;
            } else {
                errdetail(format!("Table access method \"{name}\" does not exist."));
                return Ok(false);
            }
        }
    }

    Ok(true)
}

// --- Shared helpers ---

// CheckXidAlive (xact.c) is set while logical decoding replays a prepared
// (or, phase-2, streamed) transaction; any table access then must go through
// the systable_* wrappers (which set bsysscan). Mirrors the tableam.h
// `unlikely(TransactionIdIsValid(CheckXidAlive) && !bsysscan)` checks.
#[inline]
fn unexpected_during_logical_decoding() -> bool {
    TransactionIdIsValid(xact::CheckXidAlive()) && !xact::bsysscan()
}

fn elog_error(message: impl Into<String>) -> Box<PgError> {
    Box::new(PgError::error(message))
}

// --- Slot functions (tableam.c) ---

pub fn table_slot_callbacks(relation: &Relation<'_>) -> TupleSlotKind {
    if let Some(am) = TableAm::of(relation) {
        match am {
            TableAm::Heap => heap::slot_callbacks(relation),
            TableAm::Pgrcolumnar => TupleSlotKind::Virtual,
        }
    } else if relation.rd_rel.relkind == RELKIND_FOREIGN_TABLE {
        // FDWs historically expect heap tuples in their slots.
        TupleSlotKind::HeapTuple
    } else {
        debug_assert!({
            let relkind = relation.rd_rel.relkind;
            relkind == RELKIND_VIEW || relkind == RELKIND_PARTITIONED_TABLE
        });
        TupleSlotKind::Virtual
    }
}

pub fn table_slot_create<'mcx>(
    mcx: Mcx<'mcx>,
    relation: &Relation<'mcx>,
) -> PgResult<SlotData<'mcx>> {
    let tts_cb = table_slot_callbacks(relation);
    // MakeSingleTupleTableSlot(RelationGetDescr(relation), tts_cb)
    Ok(exectuples::make_tuple_table_slot(
        mcx,
        tts_cb,
        Some(relation.rd_att.clone()),
    ))
}

// --- Table scan functions (tableam.h wrappers + tableam.c) ---

pub fn table_beginscan<'mcx>(
    mcx: Mcx<'mcx>,
    relation: &Relation<'mcx>,
    snapshot: Snapshot<'mcx>,
    nkeys: i32,
    key: PgVec<'mcx, ScanKeyData>,
) -> PgResult<TableScanDesc<'mcx>> {
    let flags = SO_TYPE_SEQSCAN | SO_ALLOW_STRAT | SO_ALLOW_SYNC | SO_ALLOW_PAGEMODE;
    match am(relation) {
        TableAm::Heap => heap::scan_begin(mcx, relation, snapshot, nkeys, key, None, flags),
        TableAm::Pgrcolumnar => {
            let _ = (nkeys, &key);
            cb2::scan_begin(mcx, relation, snapshot, flags, None)
        }
    }
}

/// An index build's scan of a pgrcolumnar table. Under `None` (SnapshotAny) its parts read every row no deletion vector
/// drops and its segments every row a snapshot may still see.
pub fn table_beginscan_index_build<'mcx>(
    mcx: Mcx<'mcx>,
    relation: &Relation<'mcx>,
    snapshot: Snapshot<'mcx>,
) -> PgResult<TableScanDesc<'mcx>> {
    let mut flags = SO_TYPE_SEQSCAN | SO_ALLOW_STRAT | SO_ALLOW_PAGEMODE;
    match am(relation) {
        TableAm::Heap => heap::scan_begin(mcx, relation, snapshot, 0, PgVec::new_in(mcx), None, flags),
        TableAm::Pgrcolumnar => {
            if snapshot.is_none() {
                flags |= SO_PGRC_INDEX_BUILD;
            }
            cb2::scan_begin(mcx, relation, snapshot, flags, None)
        }
    }
}

/// ALTER's rewrite scan: pgrcolumnar reads its rows under `desc`, the descriptor they were written with.
pub fn table_beginscan_as<'mcx>(
    mcx: Mcx<'mcx>,
    relation: &Relation<'mcx>,
    snapshot: Snapshot<'mcx>,
    desc: &types_tuple::tupdesc::TupleDescData<'_>,
) -> PgResult<TableScanDesc<'mcx>> {
    if am(relation) != TableAm::Pgrcolumnar {
        return table_beginscan(mcx, relation, snapshot, 0, PgVec::new_in(mcx));
    }
    let flags = SO_TYPE_SEQSCAN | SO_ALLOW_STRAT | SO_ALLOW_SYNC | SO_ALLOW_PAGEMODE;
    let rs_base = cb2::scan_base(mcx, relation, snapshot, flags, None);
    let mut scan = ::pgrc_am::scan::PgrcScanDescData::begin_as(rs_base, Some(desc))?;
    cb2::attach_filter(mcx, relation, &mut scan)?;
    Ok(TableScanDesc::Pgrcolumnar(scan))
}

pub fn table_beginscan_strat<'mcx>(
    mcx: Mcx<'mcx>,
    relation: &Relation<'mcx>,
    snapshot: Snapshot<'mcx>,
    nkeys: i32,
    key: PgVec<'mcx, ScanKeyData>,
    allow_strat: bool,
    allow_sync: bool,
) -> PgResult<TableScanDesc<'mcx>> {
    let mut flags = SO_TYPE_SEQSCAN | SO_ALLOW_PAGEMODE;
    if allow_strat {
        flags |= SO_ALLOW_STRAT;
    }
    if allow_sync {
        flags |= SO_ALLOW_SYNC;
    }
    match am(relation) {
        TableAm::Heap => heap::scan_begin(mcx, relation, snapshot, nkeys, key, None, flags),
        TableAm::Pgrcolumnar => {
            let _ = (nkeys, &key);
            cb2::scan_begin(mcx, relation, snapshot, flags, None)
        }
    }
}

pub fn table_beginscan_catalog<'mcx>(
    _mcx: Mcx<'mcx>,
    _relation: &Relation<'mcx>,
    _nkeys: i32,
    _key: PgVec<'mcx, ScanKeyData>,
) -> PgResult<TableScanDesc<'mcx>> {
    // RegisterSnapshot(GetCatalogSnapshot(relid)) + scan_begin(SEQSCAN|STRAT|SYNC|PAGEMODE|TEMP_SNAPSHOT).
    unported("backend-utils-time-snapmgr (GetCatalogSnapshot/RegisterSnapshot)")
}

pub fn table_beginscan_bm<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    snapshot: Snapshot<'mcx>,
) -> PgResult<TableScanDesc<'mcx>> {
    let flags = SO_TYPE_BITMAPSCAN | SO_ALLOW_PAGEMODE;
    match am(rel) {
        TableAm::Heap => heap::scan_begin(mcx, rel, snapshot, 0, PgVec::new_in(mcx), None, flags),
        // IX1 L4: a bitmap page is a virtual block of row IDs, each judged and fetched by its ID (pgrc_front::ixbitmap)
        TableAm::Pgrcolumnar => {
            ensure_pgrc_buffer_hooks();
            let source = ::pgrc_front::ixbitmap::begin(mcx, rel, snapshot.clone())?;
            Ok(TableScanDesc::Pgrcolumnar(::pgrc_am::scan::PgrcScanDescData::begin_bitmap(cb2::scan_base(mcx, rel, snapshot, flags, None), Box::new(source))))
        }
    }
}

/// Heap's `bitmap_next_block` without a buffer: the source judges the page's IDs; the page's staged count, `None`
/// once the bitmap is exhausted.
fn pgrc_bitmap_next_block(
    source: &mut (dyn ::pgrc_am::scan::BitmapSource + '_),
    tbm: Option<&tidbitmap::TIDBitmap<'_>>,
    iterator: &mut tidbitmap::TbmIterator,
    recheck: &mut bool,
    lossy_pages: &mut u64,
    exact_pages: &mut u64,
) -> PgResult<Option<u32>> {
    let mut offsets = [0 as ::types_core::OffsetNumber; tidbitmap::TBM_MAX_TUPLES_PER_PAGE];
    let (blockno, lossy, page_recheck, noffsets) = {
        let Some(tbmres) = iterator.next(tbm) else { return Ok(None) };
        let n = if tbmres.lossy { 0 } else { tbmres.extract_page_tuples(&mut offsets).min(offsets.len()) };
        (tbmres.blockno, tbmres.lossy, tbmres.recheck, n)
    };
    *recheck = page_recheck;
    let n = source.stage(blockno, if lossy { None } else { Some(&offsets[..noffsets]) })?;
    if lossy {
        *lossy_pages += 1;
    } else {
        *exact_pages += 1;
    }
    Ok(Some(n))
}

#[allow(clippy::too_many_arguments)]
pub fn table_beginscan_sampling<'mcx>(
    mcx: Mcx<'mcx>,
    relation: &Relation<'mcx>,
    snapshot: Snapshot<'mcx>,
    nkeys: i32,
    key: PgVec<'mcx, ScanKeyData>,
    allow_strat: bool,
    allow_sync: bool,
    allow_pagemode: bool,
) -> PgResult<TableScanDesc<'mcx>> {
    let mut flags = SO_TYPE_SAMPLESCAN;
    if allow_strat {
        flags |= SO_ALLOW_STRAT;
    }
    if allow_sync {
        flags |= SO_ALLOW_SYNC;
    }
    if allow_pagemode {
        flags |= SO_ALLOW_PAGEMODE;
    }
    match am(relation) {
        TableAm::Heap => heap::scan_begin(mcx, relation, snapshot, nkeys, key, None, flags),
        // D4735: heap's page sampling over the virtual blocks of the IDs the counter has reserved; each picked offset is
        // staged by its ID (pgrc_front::ixbitmap)
        TableAm::Pgrcolumnar => {
            ensure_pgrc_buffer_hooks();
            ::pgrc_front::ixfetch::sample_ids_cover(relation)?;
            let bound = ::pgrc_front::ixfetch::tid_bound(relation)?;
            let source = ::pgrc_front::ixbitmap::begin(mcx, relation, snapshot.clone())?;
            let base = cb2::scan_base(mcx, relation, snapshot, flags, None);
            Ok(TableScanDesc::Pgrcolumnar(::pgrc_am::scan::PgrcScanDescData::begin_sample(base, Box::new(source), bound)))
        }
    }
}

pub fn table_beginscan_tid<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    snapshot: Snapshot<'mcx>,
) -> PgResult<TableScanDesc<'mcx>> {
    let flags = SO_TYPE_TIDSCAN;
    match am(rel) {
        TableAm::Heap => heap::scan_begin(mcx, rel, snapshot, 0, PgVec::new_in(mcx), None, flags),
        // IX1 L0: each TID is fetched by the row ID it names; the IDs the counter has reserved bound the valid ones
        TableAm::Pgrcolumnar => {
            let high = ::pgrc_front::ixfetch::tid_bound(rel)?;
            Ok(TableScanDesc::Pgrcolumnar(::pgrc_am::scan::PgrcScanDescData::begin_tid(cb2::scan_base(mcx, rel, snapshot, flags, None), high)))
        }
    }
}

pub fn table_beginscan_analyze<'mcx>(
    mcx: Mcx<'mcx>,
    relation: &Relation<'mcx>,
) -> PgResult<TableScanDesc<'mcx>> {
    let flags = SO_TYPE_ANALYZE;
    match am(relation) {
        TableAm::Heap => heap::scan_begin(mcx, relation, None, 0, PgVec::new_in(mcx), None, flags),
        TableAm::Pgrcolumnar => cb2::scan_begin(mcx, relation, None, flags, None),
    }
}

// pgrcolumnar (WW-2, census S02/U-22): per-column whole-table NDV from the
// union of every part's NdvRegisters HLL sections (0 = unknown); None while
// the table has no committed publish. Served from pgrc_am's
// generation-keyed session fold cache (the planner asks on every plan).
pub fn pgrc_footer_ndv(rel: &Relation<'_>) -> PgResult<Option<Vec<u64>>> {
    ::pgrc_am::footer::footer_ndv(rel)
}

// pgrc-costing: per-column on-disk data bytes summed over every part's
// section table (planner column-fraction seqscan disk costing); None while
// the table has no committed
// publish. Same generation-keyed session fold cache as the NDV face.
pub fn pgrc_footer_col_bytes(rel: &Relation<'_>) -> PgResult<Option<Vec<u64>>> {
    ::pgrc_am::footer::footer_col_bytes(rel)
}

// gravity §3 #7: per-column qual evaluation class bits
// (pgrc_am::footer::QUAL_CLASS_*) for the planner's qual ordering; None
// while the table has no committed publish.
pub fn pgrc_footer_qual_classes(rel: &Relation<'_>) -> PgResult<Option<Vec<u8>>> {
    ::pgrc_am::footer::footer_qual_classes(rel)
}

// gravity §3 #6: per-column max character length (0 = unknown) bounding
// `length(col)` group counts; None while the table has no committed publish.
pub fn pgrc_footer_max_len(rel: &Relation<'_>) -> PgResult<Option<Vec<i32>>> {
    ::pgrc_am::footer::footer_max_len(rel)
}

// NEW-stats-2: the folded row total and per-column non-null counts (None =
// unknown) from the parts' Stats records; None while the table has no
// committed publish. A cost input only (the counts may lag the snapshot).
pub fn pgrc_footer_nonnull(rel: &Relation<'_>) -> PgResult<Option<(u64, Vec<Option<u64>>)>> {
    ::pgrc_am::footer::footer_nonnull(rel)
}

// The columnar scan cost model (costsize::pgrc_cost): the granule count
// with per-column codec kinds (pgrc_am::footer::CODEC_*) and dictionary
// sizes; None while the table has no committed publish.
pub use ::pgrc_am::footer::CodecFacts as PgrcCodecFacts;
pub fn pgrc_footer_codecs(rel: &Relation<'_>) -> PgResult<Option<PgrcCodecFacts>> {
    ::pgrc_am::footer::footer_codecs(rel)
}

// IXPLAN-1 (2): per unsealed front segment, (heap blocks now, relpages, reltuples): the planner estimates the
// front's rows from them as it estimates a heap's. Empty while the table has no committed publish.
pub fn pgrc_front_shapes<'mcx>(mcx: ::mcx::Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<Vec<(u64, i32, f64)>> {
    ::pgrc_front::ring::unsealed_shapes(mcx, rel)
}

// The columnar scan cost model: one column's zone facts — the planner's summary
// (the census input) and, expanded on first use,
// the per-granule [min_key, max_key] in granule order (the zone-walk
// paths); None without a committed publish or when the column carries no
// word zone key.
pub use ::pgrc_am::footer::ZoneColumn as PgrcZoneColumn;
pub fn pgrc_footer_zone_column(rel: &Relation<'_>, attno: u32) -> PgResult<Option<std::sync::Arc<PgrcZoneColumn>>> {
    ::pgrc_am::footer::footer_zone_column(rel, attno)
}

// pgrc-costing: per-column planner-convention average datum widths from
// the part-grain Stats fold (payload avg + VARHDRSZ; 0 = unknown); None
// while the table has no committed publish. Consumed by plancat's pgrc
// attr_widths pre-fill.
pub fn pgrc_footer_avg_widths(rel: &Relation<'_>) -> PgResult<Option<Vec<i32>>> {
    ::pgrc_am::footer::footer_avg_widths(rel)
}

// M5a: the engine ANALYZE fold (ST-2 LIVE — pgrc_am::analyze module doc).
// ANALYZE on a pgrcolumnar table folds seal-built facts into
// pg_statistic-shaped columns with NO sampling scan; "cannot serve" is a
// typed decline the caller fails OPEN on (the sampling path runs). The
// class/semantics/collation re-exports are the consumer's render gates
// (canonical bytes → datums is the AM boundary's render contract).
pub use ::pgrc_am::analyze::{
    AnalyzeFold as PgrcAnalyzeFold, ColFold as PgrcColFold, FoldDecline as PgrcFoldDecline,
    RelFold as PgrcRelFold, PGRC_STATS_MCV_K,
};
pub use ::pgrc_am::analyze::{
    CollationClass as PgrcCollationClass, PgStatColumn as PgrcPgStatColumn,
    StorageClass as PgrcStorageClass, TypeSemantics as PgrcTypeSemantics,
};
pub fn pgrc_analyze_fold(mcx: Mcx<'_>, rel: &Relation<'_>) -> PgResult<PgrcAnalyzeFold> {
    if !::pgrc_front::front_bearing_relid(mcx, rel.rd_id)? {
        return ::pgrc_am::analyze::analyze_fold(rel);
    }
    let residue = PgrcAnalyzeFold::Declined(::pgrc_am::analyze::FoldDecline::FrontResidue);
    // AUTOSTATS: autovacuum's visit no longer seals the front (the front worker does, within its age window), and its
    // ANALYZE should not pay the sampling scan, which reads the whole table. With rows in the front, an autovacuum
    // ANALYZE folds the parts when the front, counted by its heap blocks, is at most a tenth of the table (the parts
    // stand for it as a sample would; the front's rows join the total). Otherwise, and for a manual ANALYZE, it
    // declines to the sampling scan, which reads the front too.
    if !::pgrc_am::ingest::is_autovacuum() {
        return Ok(residue);
    }
    let (seg_blocks, tomb_blocks) = ::pgrc_front::front_blocks_relid(mcx, rel.rd_id)?;
    if seg_blocks == 0 && tomb_blocks == 0 {
        // bearing for another reason (a composed statement view)
        return Ok(residue);
    }
    let mut fold = match ::pgrc_am::analyze::analyze_fold(rel)? {
        PgrcAnalyzeFold::Folded(f) => f,
        declined => return Ok(declined),
    };
    // heap rows per block at the folded width (a tuple header, its line pointer); a tombstone is a header and an ident
    let per_block = |width: f64| (::types_core::BLCKSZ as f64 - 24.0) / (((24.0 + width) / 8.0).ceil() * 8.0 + 4.0);
    let width: f64 = fold.cols.iter().map(|c| c.stat.stawidth).sum();
    let front_rows = seg_blocks as f64 * per_block(width);
    let deleted_rows = tomb_blocks as f64 * per_block(16.0);
    let parts_rows = fold.total_rows as f64;
    if front_rows + deleted_rows > parts_rows / 10.0 {
        return Ok(residue);
    }
    fold.total_rows = (parts_rows + front_rows - deleted_rows).max(0.0) as u64;
    Ok(PgrcAnalyzeFold::Folded(fold))
}

/// The fold-vintage witness for the S-1 pg_statistic read leg's honest
/// source stamping (SketchFold only when THIS process wrote the rows via
/// the fold; after a restart the label conservatively degrades to
/// AnalyzeSample — the values are identical either way).
pub fn pgrc_stats_fold_vintage(rel: &Relation<'_>) -> bool {
    ::pgrc_am::analyze::is_fold_vintage(rel)
}

/// Record fold-vintage pg_statistic rows for `rel` (the ANALYZE fold
/// consumer calls this right after its `update_attstats` write).
pub fn pgrc_record_stats_fold_vintage(rel: &Relation<'_>) {
    ::pgrc_am::analyze::record_fold_vintage(rel)
}

/// v8 per-column NDV register sketch (whole-part HyperLogLog). Register
/// sketches union losslessly where NDV scalars cannot be combined — the
/// inherited-ANALYZE consumer unions these across pgrcolumnar children for
/// an honest parent-level NDV. None: no committed footer (zero committed
/// rows). Per-column None entries: sketch absent (pre-v8 part,
/// append-invalidated chain, or the writer kill switch).
// pgrc's ANALYZE sampling face: the exact LIVE row count for the open analyze scan, the stream `getnextslot` serves:
// the parts' rows less their deletions and the snapshot's tombstones, and the live rows of the front's segments (read
// through pgrc_front, which the parts cannot open: the segment pass needs the memory context and a slot). The
// sampler's totalrows must equal the stream it reservoirs over.
pub fn pgrc_analyze_total_rows<'mcx>(
    mcx: Mcx<'mcx>,
    scan: &mut TableScanDesc<'mcx>,
    slot: &mut SlotData<'mcx>,
) -> PgResult<u64> {
    match scan {
        TableScanDesc::Pgrcolumnar(c) => ::pgrc_front::sample::total_rows(mcx, c, slot),
        TableScanDesc::Heap(_) => panic!("pgrc_analyze_total_rows: heap scan"),
    }
}

/// The sampler's two passes: the ctid positions of the live rows at stream ordinals `ords` (ascending; no column
/// decodes), then each kept row by its position.
pub fn pgrc_analyze_live_positions(scan: &mut TableScanDesc<'_>, ords: &[u64], out: &mut Vec<u64>) -> PgResult<()> {
    match scan {
        TableScanDesc::Pgrcolumnar(c) => c.live_positions(ords, out),
        TableScanDesc::Heap(_) => panic!("pgrc_analyze_live_positions: heap scan"),
    }
}

/// The sample's positions (ascending), set before its fetches: each granule then decodes only the sampled rows.
pub fn pgrc_analyze_sample_positions(scan: &mut TableScanDesc<'_>, positions: Vec<u64>) {
    match scan {
        TableScanDesc::Pgrcolumnar(c) => c.set_sample_positions(positions),
        TableScanDesc::Heap(_) => panic!("pgrc_analyze_sample_positions: heap scan"),
    }
}

pub fn pgrc_analyze_fetch_position<'mcx>(
    mcx: Mcx<'mcx>,
    scan: &mut TableScanDesc<'mcx>,
    pos: u64,
    slot: &mut SlotData<'mcx>,
) -> PgResult<bool> {
    match scan {
        TableScanDesc::Pgrcolumnar(c) => ::pgrc_front::sample::fetch(mcx, c, pos, slot),
        TableScanDesc::Heap(_) => panic!("pgrc_analyze_fetch_position: heap scan"),
    }
}

pub use ::pgrc_am::scan::SampleFetch as PgrcSampleFetch;
/// r14 merge: the sample-position bit of a front segment row (`pgrc_front::sample::fetch`); part rows have it clear.
pub use ::pgrc_am::scan::SEGMENT_POSITION as PGRC_SEGMENT_POSITION;

/// D3742: the sample's rows at `positions` (ascending) decoded by `dop` threads, each handed to `emit` in position order
/// as its column values and NULL flags (`PgrcScanDescData::sample_rows_parallel`); `Declined` when the sample spans
/// fewer than `min_granules` granules (the caller fetches each position).
pub fn pgrc_analyze_fetch_parallel(
    scan: &mut TableScanDesc<'_>,
    positions: &[u64],
    dop: usize,
    min_granules: usize,
    emit: &mut dyn FnMut(&[datum::Datum], &[bool]) -> PgResult<()>,
) -> PgResult<PgrcSampleFetch> {
    match scan {
        TableScanDesc::Pgrcolumnar(c) => c.sample_rows_parallel(positions, dop, min_granules, emit),
        TableScanDesc::Heap(_) => panic!("pgrc_analyze_fetch_parallel: heap scan"),
    }
}

/// pgrc spec §8.9 (loadstats D): each part's live rows in `getnextslot` order and whether its stored Sample section
/// may stand for it (`PgrcScanDescData::sample_part_rows`).
pub fn pgrc_analyze_sample_parts(scan: &TableScanDesc<'_>) -> PgResult<Vec<(u64, bool)>> {
    match scan {
        TableScanDesc::Pgrcolumnar(c) => c.sample_part_rows(),
        TableScanDesc::Heap(_) => panic!("pgrc_analyze_sample_parts: heap scan"),
    }
}

/// pgrc spec §8.9 (loadstats D): part `p`'s stored sample rows `pick` chooses, each to `emit`; false = not served
/// (`PgrcScanDescData::stored_sample`).
pub fn pgrc_analyze_stored_sample(
    scan: &mut TableScanDesc<'_>,
    p: usize,
    pick: &mut dyn FnMut(usize) -> Option<Vec<usize>>,
    emit: &mut dyn FnMut(&[datum::Datum], &[bool]) -> PgResult<()>,
) -> PgResult<bool> {
    match scan {
        TableScanDesc::Pgrcolumnar(c) => c.stored_sample(p, pick, emit),
        TableScanDesc::Heap(_) => panic!("pgrc_analyze_stored_sample: heap scan"),
    }
}

// pgrc-relstats: committed (rows, on-disk part bytes) from the effective
// manifest — the SAME facts table_relation_estimate_size serves the
// planner, re-exported so ANALYZE writes a pg_class.relpages/reltuples
// that agrees with plan-time truth (P6-5). None: no committed publish.
pub fn pgrc_footer_size(rel: &Relation<'_>) -> PgResult<Option<(u64, u64)>> {
    ::pgrc_am::footer::footer_size(rel)
}

// `sorted_by` (pgrc_am::reloptions): DDL's per-column verdict, the
// ALTER SET emptiness gate, and the planner's claimable key.
pub fn pgrc_sorted_by_column_sortable(atts: &[types_tuple::FormData_pg_attribute], col: &str) -> Option<bool> {
    ::pgrc_am::reloptions::column_sortable(atts, col)
}

// F1: rows in the front's segments count as data, and a front-bearing table claims no order (its segment rows are
// unsorted).
fn pgrc_front_bearing(rel: &Relation<'_>) -> PgResult<bool> {
    let ctx = mcx::MemoryContext::new("pgrc front");
    ::pgrc_front::front_bearing_relid(ctx.mcx(), rel.rd_id)
}

pub use ::pgrc_am::schema::ColumnsVerdict as PgrcColumnsVerdict;

/// What ALTER does to `rel`'s stored columns, which were `old`'s (F1 L5; FASTADD, pgrc-plans/FASTADD.done): nothing
/// for a table that is not pgrcolumnar or whose stored columns did not change; metadata-only for added and dropped
/// columns; the rewrite for a changed one. An unsupported new column type refuses here.
pub fn pgrc_columns_verdict(rel: &Relation<'_>, old: &types_tuple::tupdesc::TupleDescData<'_>) -> PgResult<PgrcColumnsVerdict> {
    if am(rel) != TableAm::Pgrcolumnar {
        return Ok(PgrcColumnsVerdict::Same);
    }
    ::pgrc_am::schema::columns_verdict(rel, &old.attrs)
}

pub fn pgrc_table_has_data(rel: &Relation<'_>) -> PgResult<bool> {
    Ok(::pgrc_am::reloptions::table_has_data(rel)? || pgrc_front_bearing(rel)?)
}

pub fn pgrc_planner_sorted_attnos(rel: &Relation<'_>) -> PgResult<Vec<i16>> {
    if pgrc_front_bearing(rel)? {
        return Ok(Vec::new());
    }
    let view = ::pgrc_front::ring::statement_view(rel)?;
    ::pgrc_am::reloptions::planner_sorted_attnos(rel, view.as_deref())
}

// D4469 road 2: the committed part count, the merged sorted path's estimate of its streams (a cost input only: the scan
// merges the parts its snapshot sees); None while the table has no committed publish or its fold does not say.
pub fn pgrc_footer_part_count(rel: &Relation<'_>) -> PgResult<Option<u64>> {
    Ok(::pgrc_am::footer::part_count(rel)?.filter(|&p| p != ::pgrc_am::footer::PARTS_UNKNOWN))
}

pub fn table_endscan(scan: TableScanDesc<'_>) -> PgResult<()> {
    match scan {
        TableScanDesc::Heap(h) => heap::scan_end(h),
        // Dropping the scan releases its part pins (registry LRU handles
        // the rest).
        TableScanDesc::Pgrcolumnar(mut c) => {
            cb2::end_segment(&mut c)?;
            if let Some(b) = c.bitmap.take() {
                b.end()?;
            }
            if (c.rs_base.rs_flags & SO_TEMP_SNAPSHOT) != 0 {
                let snap = c
                    .rs_temp_snapshot
                    .take()
                    .expect("SO_TEMP_SNAPSHOT scan carries its registered snapshot");
                ::snapmgr::UnregisterSnapshot(Some(&snap));
            }
            drop(c);
            Ok(())
        }
    }
}

fn table_tid_get_latest<'mcx>(
    mcx: Mcx<'mcx>,
    rel: Relation<'mcx>,
    snapshot: std::rc::Rc<::types_snapshot::SnapshotData<'static>>,
    mut tid: ItemPointerData,
) -> PgResult<ItemPointerData> {
    let mut scan = table_beginscan_tid(mcx, &rel, Some(snapshot))?;
    table_tuple_get_latest_tid(mcx, &mut scan, &mut tid)?;
    table_endscan(scan)?;
    Ok(tid)
}

pub fn table_rescan<'mcx>(
    _mcx: Mcx<'mcx>,
    scan: &mut TableScanDesc<'mcx>,
    key: Option<&[ScanKeyData]>,
) -> PgResult<()> {
    match scan {
        TableScanDesc::Heap(h) => heap::scan_rescan(h, key, false, false, false, false),
        TableScanDesc::Pgrcolumnar(c) => {
            let _ = key;
            cb2::end_segment(c)?;
            c.reset_position();
            // IX1 L4: a bitmap scan's staged page goes with its bitmap
            if let Some(b) = c.bitmap.as_deref_mut() {
                b.reset();
            }
            Ok(())
        }
    }
}

pub fn table_rescan_set_params<'mcx>(
    _mcx: Mcx<'mcx>,
    scan: &mut TableScanDesc<'mcx>,
    key: Option<&[ScanKeyData]>,
    allow_strat: bool,
    allow_sync: bool,
    allow_pagemode: bool,
) -> PgResult<()> {
    match scan {
        TableScanDesc::Heap(h) => {
            heap::scan_rescan(h, key, true, allow_strat, allow_sync, allow_pagemode)
        }
        TableScanDesc::Pgrcolumnar(c) => {
            let _ = key;
            cb2::end_segment(c)?;
            c.reset_position();
            // IX1 L4: a bitmap scan's staged page goes with its bitmap
            if let Some(b) = c.bitmap.as_deref_mut() {
                b.reset();
            }
            Ok(())
        }
    }
}

// Per-tuple at M2: the single-variant match monomorphizes to the direct
// heap_getnextslot call — zero glue over heapam's loop.
#[inline]
pub fn table_scan_getnextslot<'mcx>(
    mcx: Mcx<'mcx>,
    scan: &mut TableScanDesc<'mcx>,
    direction: ScanDirection,
    slot: &mut SlotData<'mcx>,
) -> PgResult<bool> {
    match scan {
        TableScanDesc::Heap(h) => heap::scan_getnextslot(mcx, h, direction, slot),
        TableScanDesc::Pgrcolumnar(c) => {
            if direction == ScanDirection::BackwardScanDirection {
                return Err(::pgrc_am::unsupported("backward scans"));
            }
            cb2::getnextslot(mcx, c, slot)
        }
    }
}

pub fn table_scan_supports_pagebatch(scan: &TableScanDesc<'_>) -> bool {
    match scan {
        TableScanDesc::Heap(h) => {
            (h.rs_base.rs_flags & SO_ALLOW_PAGEMODE) != 0 && h.rs_base.rs_parallel.is_none()
        }
        // DELIBERATELY false (was `true` on the old branch): this is the
        // INCUMBENT fused drives' gate (fused agg/sort/hash batched paths in
        // the row executor) plus the lane's admission-economics probe. The
        // lane-v2 pipeline owns pgrcolumnar batching through the PARALLEL
        // variant below instead; keeping the incumbents off pgrcolumnar keeps
        // lane-OFF the pure per-row Volcano drive (`getnextslot`) — the
        // byte-parity oracle for every pgrcolumnar lane path (phase4 design §6).
        // pgrcolumnar (M3-H): per-row Volcano only; batching is M3-G's.
        TableScanDesc::Pgrcolumnar(_) => false,
    }
}

/// As `table_scan_supports_pagebatch`, but also admits parallel scan
/// descriptors: the batched page feed (`heap_getnextpagebatch` →
/// `pagemode_next_page` → `heap_fetch_next_buffer`) acquires blocks through
/// `parallel_next_block` — the shared DSM block cursor — exactly as the
/// per-tuple pagemode walk does, so page-at-a-time batching partitions blocks
/// across workers without gaps or overlaps. The strict gate above stays for
/// the fused agg/sort/hash drives until each is audited under parallel; only
/// the lane-v2 SeqScan drive routes here.
pub fn table_scan_supports_pagebatch_parallel(scan: &TableScanDesc<'_>) -> bool {
    match scan {
        TableScanDesc::Heap(h) => (h.rs_base.rs_flags & SO_ALLOW_PAGEMODE) != 0,
        // The PgrcolumnarSource tranche: the lane owns pgrcolumnar scan staging
        // (`next_window` windows <= WINDOW_ROWS, RG/granule/block pruning
        // inside produce). Serial and parallel both admit — the window feed
        // claims row groups through the shared `phs_nallocated` cursor
        // exactly as the per-tuple drive does, partitioning RGs across
        // workers without gaps or overlaps.
        // pgrcolumnar (M3-H): no window feed yet — M3-G owns lane staging.
        TableScanDesc::Pgrcolumnar(_) => false,
    }
}

/// Total committed pgrcolumnar rows (footer metadata; no decode) — the lane's
/// tiny-input admission floor. None = heap (the floor is pgrcolumnar-only today).
pub fn table_scan_cb_total_rows(scan: &TableScanDesc<'_>) -> Option<u64> {
    match scan {
        TableScanDesc::Heap(_) => None,
        // pgrcolumnar: keep the lane admission machinery off this AM
        // until M3-G registers its implementor.
        TableScanDesc::Pgrcolumnar(_) => None,
    }
}

/// Part-global granule geometry of a pgrcolumnar scan (runtime morsel source,
/// M1 scan pipelines): (total granules, row-group-start prefix sums = the
/// hard morsel boundaries). None = heap or empty part.
pub fn table_scan_cb_granule_geometry(scan: &TableScanDesc<'_>) -> Option<(u64, Vec<u64>)> {
    match scan {
        TableScanDesc::Heap(_) => None,
        TableScanDesc::Pgrcolumnar(_) => None,
    }
}

/// Heap block-range geometry for the runtime morsel source (M1 heap
/// source): total blocks at scan start = the granule count (granule = one
/// block; heap has no dictionary epochs, so a heap source has no interior
/// morsel boundaries). `None` = not a heap scan.
pub fn table_scan_heap_block_geometry(scan: &TableScanDesc<'_>) -> Option<u64> {
    match scan {
        TableScanDesc::Heap(h) => Some(h.rs_nblocks as u64),
        TableScanDesc::Pgrcolumnar(_) => None,
    }
}

/// Position a PGRCOLUMNAR scan on the granule claim [g0, g1). Ok(false) =
/// heap, scan untouched — the m2 sink arms' fail-closed worker check
/// (their admission is pgrcolumnar-only; AM-dispatching them onto heap blocks
/// would silently change the claim unit under a divergent worker plan).
/// The AM-dispatched positioning is `table_scan_set_morsel_range` below.
pub fn table_scan_cb_set_granule_range(
    scan: &mut TableScanDesc<'_>,
    _g0: u64,
    _g1: u64,
) -> PgResult<bool> {
    match scan {
        TableScanDesc::Heap(_) => Ok(false),
        // Fail-closed like heap: the m2 sink arms admit pgrcolumnar only.
        TableScanDesc::Pgrcolumnar(_) => Ok(false),
    }
}

/// Position the scan on the morsel claim [g0, g1), dispatching on the AM
/// (the runtime morsel drive): pgrcolumnar absolute granules (must lie inside
/// one row group — the runtime clamps claims to the boundaries
/// `table_scan_cb_granule_geometry` reports) or heap blocks (any contiguous
/// in-relation range; heap reports no interior boundaries).
pub fn table_scan_set_morsel_range(
    scan: &mut TableScanDesc<'_>,
    g0: u64,
    g1: u64,
) -> PgResult<()> {
    match scan {
        TableScanDesc::Heap(h) => ::heapam::heap_set_block_range(h, g0, g1),
        // The runtime morsel drive never engages here (geometry above
        // reports None); defensive typed refusal.
        TableScanDesc::Pgrcolumnar(_) => Err(::pgrc_am::unsupported("morsel positioning")),
    }
}

/// End-of-claim pin release (single-executor wave 2, WS-O inc-2,
/// append-only seam): heap resets to the drained state, dropping the
/// current page pin if the claim ended early (`heap_end_claim_release` —
/// the R3 zero-pins-at-settle law); pgrcolumnar stages no page pins — no-op.
pub fn table_scan_end_claim_release(scan: &mut TableScanDesc<'_>) {
    match scan {
        TableScanDesc::Heap(h) => ::heapam::heap_end_claim_release(h),
        TableScanDesc::Pgrcolumnar(_) => {}
    }
}

/// Cursor-suspension park point (WS-AI wave-9.5, lane-cursors.md §2,
/// append-only seam): the staged page batch's reposition window
/// `(b0, b1)` — restageable remainder `[b0, b1)` including the staged
/// block — iff the scan holds a settleable mid-claim heap page
/// (`heap_cursor_park_point`). None = nothing staged/pinned, a
/// wrap-capable walk (syncscan-started), a parallel scan, or pgrcolumnar
/// (whose staged windows hold no bufmgr pins — R4 decode scratch; nothing
/// to settle, node-resident by design).
pub fn table_scan_cursor_park_point(scan: &TableScanDesc<'_>) -> Option<(u64, u64)> {
    match scan {
        TableScanDesc::Heap(h) => ::heapam::heap_cursor_park_point(h),
        TableScanDesc::Pgrcolumnar(_) => None,
    }
}

/// Mid-page batch adoption (SE-R41 v2, the page-remainder defect fix): when
/// a batch drive freshly engages over a scan the per-tuple walk already
/// advanced mid-page, adopt the current page's unconsumed remainder
/// `[start, n)` instead of advancing past it (`heap_adopt_midpage_batch`).
/// None = nothing to adopt — a fresh/drained scan stages the next page as
/// before. pgrcolumnar: its staged windows are not page-cursor-interleaved
/// (different staging machinery) — always None below the AM dispatch.
pub fn table_scan_adopt_midpage_batch(scan: &mut TableScanDesc<'_>) -> Option<(u32, u32)> {
    match scan {
        TableScanDesc::Heap(h) => ::heapam::heap_adopt_midpage_batch(h),
        TableScanDesc::Pgrcolumnar(_) => None,
    }
}

/// R3 settle probe (debug-assert face of the zero-pins-at-settle law):
/// does this scan still hold a claim page pin? pgrcolumnar stages no page
/// pins — always false below the AM dispatch.
pub fn table_scan_holds_claim_pin(scan: &TableScanDesc<'_>) -> bool {
    match scan {
        TableScanDesc::Heap(h) => h.rs_cbuf.is_some(),
        TableScanDesc::Pgrcolumnar(_) => false,
    }
}

/// GCUT zone summary of a pgrcolumnar scan's key column (night/sort-merge-
/// redesign inc-2): per-granule best direction-folded order words + the
/// zone-max seed word — see `CbScanDescData::zone_topk_words` for the
/// correctness posture. `None` = heap or no columnar part.
pub fn table_scan_cb_zone_topk_words(
    scan: &TableScanDesc<'_>,
    _col: u16,
    _desc: bool,
    _bound: u64,
) -> PgResult<Option<(Vec<u64>, Option<u64>)>> {
    match scan {
        TableScanDesc::Heap(_) => Ok(None),
        TableScanDesc::Pgrcolumnar(_) => Ok(None),
    }
}

/// Direct bounded top-N granule drive of a pgrcolumnar scan (the runtime
/// sort arm's PGRUST_RUNTIME_TOPN_HEAP feed): hand out the current
/// granule-range claim's next granule whole — (nrows, rowref_base) — with
/// the per-RG visibility gate, no window/SoA staging. `None` = claim
/// exhausted. Heap AMs error (the arm admits pgrcolumnar only).
pub fn table_scan_topn_direct_next_granule(
    scan: &mut TableScanDesc<'_>,
) -> PgResult<Option<(u32, u64)>> {
    match scan {
        TableScanDesc::Heap(_) => Err(elog_error(
            "direct top-N granule drive on a non-columnar scan",
        )),
        TableScanDesc::Pgrcolumnar(_) => Err(elog_error(
            "direct top-N granule drive on a pgrcolumnar scan",
        )),
    }
}

/// The decoded datum lane of column `col` for the granule
/// `table_scan_topn_direct_next_granule` just handed out. None = dict
/// column / out of range (the caller fails closed to the staged feed).
pub fn table_scan_topn_direct_lane<'a>(
    scan: &'a mut TableScanDesc<'_>,
    _col: usize,
) -> Option<&'a [::datum::Datum]> {
    match scan {
        TableScanDesc::Heap(_) => None,
        TableScanDesc::Pgrcolumnar(_) => None,
    }
}

/// Drive-scaling observability counters of a pgrcolumnar scan (the runtime
/// WFIN channel): (rg_switches, dict_builds, granules_scanned,
/// windows_staged). None = heap.
pub fn table_scan_cb_drive_counters(scan: &TableScanDesc<'_>) -> Option<(u64, u64, u64, u64)> {
    match scan {
        TableScanDesc::Heap(_) => None,
        TableScanDesc::Pgrcolumnar(_) => None,
    }
}

/// EA-on-morsels: snapshot the pgrcolumnar scan descriptor's cumulative debug
/// counters (the CBSCAN line's fields) for the EXPLAIN ANALYZE prune fold —
/// [granules_scanned, granules_pruned, granules_bloom_pruned, granules_meta,
/// granules_bound_skipped, blocks_pruned, windows_staged]. None on heap.
/// Read-only; the counters are the pre-existing unconditional scan-desc
/// accumulators (docs/design/ea-morsels.md §1).
pub fn table_scan_cb_ea_counters(scan: &TableScanDesc<'_>) -> Option<[u64; 7]> {
    match scan {
        TableScanDesc::Heap(_) => None,
        TableScanDesc::Pgrcolumnar(_) => None,

    }
}

/// Relation size at scan start (heap rs_nblocks): the deform-JIT page gate.
pub fn table_scan_nblocks(scan: &TableScanDesc<'_>) -> u32 {
    match scan {
        TableScanDesc::Heap(h) => h.rs_nblocks,
        // pgrcolumnar stages no pages; 0 keeps the deform-JIT gate cold.
        TableScanDesc::Pgrcolumnar(_) => 0,
    }
}

/// Columnar column projection: only these attributes' chunks are decoded.
/// Heap scans always materialize whole tuples — no-op.
pub fn table_scan_set_needed_attrs(scan: &mut TableScanDesc<'_>, needed: &[bool]) {
    match scan {
        TableScanDesc::Heap(_) => {}
        // D259: the row scan decodes only the plan's consumed columns (IX1 L4: a bitmap scan's rows too).
        TableScanDesc::Pgrcolumnar(c) => match c.bitmap.as_deref_mut() {
            Some(b) => b.set_needed(needed),
            None => c.set_needed_attrs(needed),
        },
    }
}

/// ROWMAP2 M0: who reads a pgrcolumnar scan's positions (`tts_tid`); a scan whose need is `None` loads no row map and
/// leaves its part rows' positions invalid. A scan never told stamps them. Heap scans always have positions — no-op.
pub fn table_scan_set_tid_need(scan: &mut TableScanDesc<'_>, need: PgrcTidNeed) {
    match scan {
        TableScanDesc::Heap(_) => {}
        TableScanDesc::Pgrcolumnar(c) => c.set_tid_need(need),
    }
}

/// pgrcolumnar post-qual materialization (pgrcolumnar_prewhere): granule decode
/// becomes per-column on demand — deform pulls only the columns it fills,
/// store_slot completes the needed set for surviving rows. Heap no-op.
pub fn table_scan_set_lazy_decode(scan: &mut TableScanDesc<'_>, _on: bool) {
    match scan {
        TableScanDesc::Heap(_) => {}
        TableScanDesc::Pgrcolumnar(_) => {}
    }
}

/// pgrcolumnar zone-map pruning conjuncts (advisory-only; executor still
/// evaluates the full qual on surviving rows).
/// RG-altitude meta-answerability census over the scan's PUSHED zone quals
/// (pgrcolumnar::zone_meta_rg_census doc — the GL-SERIALTERM-META qual-zone
/// helper). None = heap or no columnar part.
pub fn table_scan_cb_zone_meta_census(
    scan: &TableScanDesc<'_>,
    _need_sums: bool,
) -> PgResult<Option<(u64, u64)>> {
    match scan {
        TableScanDesc::Heap(_) => Ok(None),
        TableScanDesc::Pgrcolumnar(_) => Ok(None),
    }
}

pub fn table_scan_push_zone_quals(scan: &mut TableScanDesc<'_>, quals: &[ZoneQual]) {
    match scan {
        TableScanDesc::Heap(_) => {}
        // ZONEPRUNE: the scan skips a part granule whose zone fails one (advisory: a dropped qual only loses a skip).
        TableScanDesc::Pgrcolumnar(c) => c.push_zone_quals(quals),
    }
}

/// Zone-ordered adaptive granule traversal (pgrcolumnar serial scans over
/// exact-zone int-family columns; docs/design/pgrcolumnar-zone-adaptive.md).
/// false = refused; the physical claim order stays.
pub fn table_scan_arm_adaptive_order(
    scan: &mut TableScanDesc<'_>,
    _col: usize,
    _desc: bool,
    _strict: bool,
) -> PgResult<bool> {
    match scan {
        TableScanDesc::Heap(_) => Ok(false),
        TableScanDesc::Pgrcolumnar(_) => Ok(false),
    }
}

/// Drop an armed adaptive traversal (consumer demotion back to the physical
/// claim order); the caller must rescan before pulling again. No-op when
/// unarmed or not pgrcolumnar.
pub fn table_scan_disarm_adaptive_order(scan: &mut TableScanDesc<'_>) {
    match scan {
        TableScanDesc::Heap(_) => {}
        TableScanDesc::Pgrcolumnar(_) => {}
    }
}

/// Consumer bound feedback for an armed adaptive scan (top-k heap floor or
/// running MIN/MAX best); no-op when unarmed.
pub fn table_scan_update_scan_bound(scan: &mut TableScanDesc<'_>, _key: ::datum::Datum) {
    match scan {
        TableScanDesc::Heap(_) => {}
        TableScanDesc::Pgrcolumnar(_) => {}
    }
}

/// Footer value min/max covering every row of the staged window (pgrcolumnar
/// granule zone entry; int-family columns only). None = no zone metadata.
pub fn table_scan_window_value_minmax(
    scan: &TableScanDesc<'_>,
    _col: usize,
) -> Option<(i64, i64)> {
    match scan {
        TableScanDesc::Heap(_) => None,
        TableScanDesc::Pgrcolumnar(_) => None,
    }
}

/// `table_scan_granule_meta_peek` verdict (granule length-stats metadata).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CbGranuleMetaStep {
    /// Scan exhausted (next_window would return 0).
    Exhausted,
    /// Not at a metadata-answerable fresh granule; stage windows normally.
    NotMeta,
    /// The upcoming granule, wholly visible, described by footer metadata.
    Meta { rows: u32 },
}

/// v7 granule length-stats metadata peek (pgrcolumnar only; see
/// `CbScanDescData::granule_meta_peek`). Heap scans are never meta.
pub fn table_scan_granule_meta_peek(
    scan: &mut TableScanDesc<'_>,
    _key_cols: &[u16],
    _len_cols: &[u16],
    _key_mm: &mut [(i64, i64)],
    _len_stats: &mut [(u64, u32, u32)],
) -> PgResult<CbGranuleMetaStep> {
    match scan {
        TableScanDesc::Heap(_) => Ok(CbGranuleMetaStep::NotMeta),
        TableScanDesc::Pgrcolumnar(_) => Ok(CbGranuleMetaStep::NotMeta),
    }
}

/// Consume the granule the peek just answered (pgrcolumnar only).
pub fn table_scan_granule_meta_consume(scan: &mut TableScanDesc<'_>) {
    match scan {
        TableScanDesc::Heap(_) => unreachable!("granule meta is pgrcolumnar-only"),
        TableScanDesc::Pgrcolumnar(_) => unreachable!("granule meta is pgrcolumnar-only"),
    }
}

/// `table_scan_agg_meta_peek` verdict (footer-stat aggregate metadata).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CbAggMetaStep {
    /// Scan exhausted (next_window would return 0).
    Exhausted,
    /// Not at a metadata-answerable position; stage windows normally.
    NotMeta,
    /// The upcoming WHOLE row group, described by footer metadata.
    MetaRg { rows: u64 },
    /// The upcoming granule, described by footer metadata.
    MetaGranule { rows: u32 },
}

/// Footer-stat aggregate metadata peek (pgrcolumnar only; see
/// `CbScanDescData::agg_meta_peek`). Heap scans are never meta.
#[allow(clippy::too_many_arguments)]
pub fn table_scan_agg_meta_peek(
    scan: &mut TableScanDesc<'_>,
    _mm_cols: &[u16],
    _sum_cols: &[u16],
    _len_cols: &[u16],
    _mm: &mut [(i64, i64)],
    _sums: &mut [i128],
    _lens: &mut [i64],
) -> PgResult<CbAggMetaStep> {
    match scan {
        TableScanDesc::Heap(_) => Ok(CbAggMetaStep::NotMeta),
        TableScanDesc::Pgrcolumnar(_) => Ok(CbAggMetaStep::NotMeta),
    }
}

/// Consume the row group the peek just answered (`MetaRg`; pgrcolumnar only).
pub fn table_scan_agg_meta_consume_rg(scan: &mut TableScanDesc<'_>) {
    match scan {
        TableScanDesc::Heap(_) => unreachable!("agg meta is pgrcolumnar-only"),
        TableScanDesc::Pgrcolumnar(_) => unreachable!("agg meta is pgrcolumnar-only"),
    }
}

/// Consume the granule the peek just answered (`MetaGranule`; pgrcolumnar only).
pub fn table_scan_agg_meta_consume_granule(scan: &mut TableScanDesc<'_>) {
    match scan {
        TableScanDesc::Heap(_) => unreachable!("agg meta is pgrcolumnar-only"),
        TableScanDesc::Pgrcolumnar(_) => unreachable!("agg meta is pgrcolumnar-only"),
    }
}

/// Metadata COUNT(*) capability (pgrcolumnar: footer row counts per
/// wholly-visible RG, fail-open per RG otherwise).
pub fn table_scan_supports_meta_count(_scan: &TableScanDesc<'_>) -> bool {
    false
}

/// Metadata COUNT(*) feed: visible-row count of one row group; 0 = exhausted.
pub fn table_scan_meta_count_next(scan: &mut TableScanDesc<'_>) -> PgResult<u32> {
    match scan {
        TableScanDesc::Heap(_) => unreachable!("meta count is pgrcolumnar-only"),
        TableScanDesc::Pgrcolumnar(_) => unreachable!("meta count is pgrcolumnar-only"),
    }
}

/// An exact metadata answer of `table_scan_meta_agg`: the row count and the
/// per-column (min, max) / sums.
pub struct MetaAggScan {
    pub rows: u64,
    pub minmax: Vec<(u16, i64, i64)>,
    pub sums: Vec<(u16, i128)>,
}

/// The zero-count metadata qual: the scan qual is EXACTLY one `col <> 0` /
/// `col = 0` conjunct over an int-family column (`keep_nonzero` = `<>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetaZeroQual {
    pub col: u16,
    pub keep_nonzero: bool,
}

/// Metadata MIN/MAX/COUNT/SUM answer (pgrcolumnar zone maps + footer row counts
/// + footer sums; `zq` = the v7 zero-count qual arm); None = the AM has no
/// exact metadata answer for these columns.
pub fn table_scan_meta_agg(
    scan: &TableScanDesc<'_>,
    _cols: &[u16],
    _sum_cols: &[u16],
    _zq: Option<MetaZeroQual>,
) -> PgResult<Option<MetaAggScan>> {
    match scan {
        TableScanDesc::Heap(_) => Ok(None),
        TableScanDesc::Pgrcolumnar(_) => Ok(None),
    }
}

/// Compressed-domain constant-fold of `q` against the currently staged
/// pgrcolumnar granule (int/date/timestamp). Heap has no granule metadata, so
/// its verdict is always Mixed (the staged drive is pgrcolumnar-only anyway).
pub fn table_scan_staged_granule_verdict(
    scan: &TableScanDesc<'_>,
    _q: &ZoneQual,
) -> ZoneVerdict {
    match scan {
        TableScanDesc::Heap(_) => ZoneVerdict::Mixed,
        TableScanDesc::Pgrcolumnar(_) => ZoneVerdict::Mixed,
    }
}

/// Arm the pgrcolumnar condition cache (pgrust.condition_cache) on this scan:
/// `fp` = the staged lane prefix's canonical fingerprint, `capacity` = the
/// byte-budget GUC. Heap scans have no granule verdicts — always false.
pub fn table_scan_condcache_arm(scan: &mut TableScanDesc<'_>, _fp: u128, _capacity: u64) -> bool {
    match scan {
        TableScanDesc::Heap(_) => false,
        TableScanDesc::Pgrcolumnar(_) => false,
    }
}

/// Condition-cache lookup for the currently staged pgrcolumnar window: true =
/// hit, `sel` now holds the staged prefix's survivor bits and the qual's
/// decode+eval legs are skipped. false = miss/unarmed (heap: always).
pub fn table_scan_condcache_lookup(scan: &mut TableScanDesc<'_>, _sel: &mut [u64]) -> bool {
    match scan {
        TableScanDesc::Heap(_) => false,
        TableScanDesc::Pgrcolumnar(_) => false,
    }
}

/// Record the currently staged pgrcolumnar window's freshly evaluated survivor
/// bits into the condition cache. No-op when unarmed or heap.
pub fn table_scan_condcache_store(scan: &mut TableScanDesc<'_>, _sel: &[u64]) {
    match scan {
        TableScanDesc::Heap(_) => {}
        TableScanDesc::Pgrcolumnar(_) => {}
    }
}

/// Fold the scan's per-scan condition-cache stat cells into the process
/// counters (before a `condcache_stats` read; scan teardown folds anyway).
pub fn table_scan_condcache_fold_stats(scan: &mut TableScanDesc<'_>) {
    match scan {
        TableScanDesc::Heap(_) => {}
        TableScanDesc::Pgrcolumnar(_) => {}
    }
}



/// Condition-cache cumulative counters (hits, misses, insertions,
/// evictions) — the lane stats dump's source.

/// Page-batch scan feed (upstream batch scan API, CF 6176): 0 = exhausted.
pub fn table_scan_getnextpagebatch<'mcx>(scan: &mut TableScanDesc<'mcx>) -> PgResult<u32> {
    match scan {
        TableScanDesc::Heap(h) => ::heapam::heap_getnextpagebatch(h),
        // Unreachable: supports_pagebatch* is false for pgrcolumnar.
        TableScanDesc::Pgrcolumnar(_) => unreachable!("pgrcolumnar has no page-batch feed"),
    }
}

/// SoA-deform the staged page batch's column prefix per `plan`.
pub fn table_scan_batch_deform<'mcx>(
    scan: &mut TableScanDesc<'mcx>,
    plan: &::exectuples::SoaDeformPlan<'_>,
    soa: &mut ::exectuples::SoaBatch<'_>,
    qual_col_only: Option<u16>,
) {
    table_scan_batch_deform_sel(scan, plan, soa, qual_col_only, None)
}

/// `table_scan_batch_deform` under an optional PREWHERE selection (the
/// survivor-window COMPLETING deform): `sel` narrows lazy sub-framed dict
/// ensures to selected rows (pgrcolumnar only; cell writes identical, heap
/// ignores it — its deform has no lazy bytes).
pub fn table_scan_batch_deform_sel<'mcx>(
    scan: &mut TableScanDesc<'mcx>,
    plan: &::exectuples::SoaDeformPlan<'_>,
    soa: &mut ::exectuples::SoaBatch<'_>,
    qual_col_only: Option<u16>,
    _sel: Option<&[u64]>,
) {
    match scan {
        TableScanDesc::Heap(h) => ::heapam::heap_batch_deform_soa(h, plan, soa, qual_col_only),
        TableScanDesc::Pgrcolumnar(_) => unreachable!("pgrcolumnar has no batch deform"),
    }
}

/// K1 inc-2 late-materialization STAGING deform (wave-9 WS-AH): the staged
/// page batch's kind-0 column pass narrowed to an explicit column set
/// ({qual clause cols ∪ the grouped feed's key cols}); classification is
/// the full deform's. Heap scans only — the pgrcolumnar staging narrows
/// through PREWHERE's per-clause late materialization instead, so the
/// columnar arm falls open to the full window deform (and debug-asserts:
/// the arming seam refuses non-heap scans).
pub fn table_scan_batch_deform_cols<'mcx>(
    scan: &mut TableScanDesc<'mcx>,
    plan: &::exectuples::SoaDeformPlan<'_>,
    soa: &mut ::exectuples::SoaBatch<'_>,
    cols: &[u16],
) {
    match scan {
        TableScanDesc::Heap(h) => ::heapam::heap_batch_deform_soa_cols(h, plan, soa, cols),
        TableScanDesc::Pgrcolumnar(_) => unreachable!("pgrcolumnar has no batch deform"),
    }
}

/// K1 inc-2 COMPLETION deform (the sel-honoring heap arm of the staged
/// batch deform dispatch): fill `cols` for `sel`-selected rows of the
/// ALREADY-staged batch — all-zero 64-row selection words skip whole,
/// full words take the dense fill. Heap = the deform split's completion
/// half (page still pinned, ABI R3); pgrcolumnar staged everything at
/// window fill — no-op (the storage seam default's "sources that staged
/// everything" contract).
pub fn table_scan_batch_complete_deform<'mcx>(
    scan: &mut TableScanDesc<'mcx>,
    plan: &::exectuples::SoaDeformPlan<'_>,
    soa: &mut ::exectuples::SoaBatch<'_>,
    cols: &[u16],
    sel: &[u64],
) {
    match scan {
        TableScanDesc::Heap(h) => {
            ::heapam::heap_batch_complete_deform_soa(h, plan, soa, cols, sel)
        }
        TableScanDesc::Pgrcolumnar(_) => {}
    }
}

/// Prewhere staged deform: fill (or dict-answer) one staged column of the
/// pgrcolumnar window. Callable only on pgrcolumnar scans (the staged qual drive is
/// pgrcolumnar-shaped); the caller owns soa.begin and the needed-set contract.
/// Length-lane re-fill of one staged pgrcolumnar column (post-qual gather for
/// dict-tier qual columns armed as length lanes). Heap scans never arm
/// length lanes (`seq_scan_batch_len_want` is pgrcolumnar-batch-scoped), so the
/// heap arm is unreachable.
pub fn table_scan_batch_fill_len<'mcx>(
    scan: &mut TableScanDesc<'mcx>,
    _c: u16,
    _chars: bool,
    _soa: &mut ::exectuples::SoaBatch<'_>,
) {
    match scan {
        _ => unreachable!("length lanes arm on pgrcolumnar batches only"),
    }
}

/// Dict-code side channel of one staged pgrcolumnar window column (str MIN/MAX
/// code folds, lane-v2-dictminmax): `Some` only when the column's current
/// chunk is dict-encoded and decoded for the staged window. Heap scans have
/// no dictionaries — always `None`.
#[inline]
pub fn table_scan_batch_dict_codes<'mcx>(
    scan: &TableScanDesc<'mcx>,
    _c: u16,
) -> Option<::exectuples::SoaDictLane> {
    match scan {
        TableScanDesc::Heap(_) => None,
        TableScanDesc::Pgrcolumnar(_) => None,
    }
}

/// `table_scan_batch_dict_codes` with the v7 part-global stitch published
/// when the scan carries one (the DictCode sort-key class,
/// docs/design/dict-code-flow.md inc-1). Consumers gate order use on
/// `table.has_stitch()` and fail closed otherwise. Heap scans have no
/// dictionaries — always `None`.
#[inline]
pub fn table_scan_batch_dict_codes_global<'mcx>(
    scan: &mut TableScanDesc<'mcx>,
    _c: u16,
) -> Option<::exectuples::SoaDictLane> {
    match scan {
        TableScanDesc::Heap(_) => None,
        TableScanDesc::Pgrcolumnar(_) => None,
    }
}

/// Physical rowref base of the CURRENT staged pgrcolumnar window (tie-ordering
/// rule 2): staged row `i`'s rowref is `base + i`. Heap scans carry no
/// rowrefs — always `None` (the rowref-armed consumer demotes).
#[inline]
pub fn table_scan_batch_rowref_base<'mcx>(scan: &TableScanDesc<'mcx>) -> Option<u64> {
    match scan {
        TableScanDesc::Heap(_) => None,
        TableScanDesc::Pgrcolumnar(_) => None,
    }
}

pub fn table_scan_batch_deform_col<'mcx>(
    scan: &mut TableScanDesc<'mcx>,
    _c: u16,
    _soa: &mut ::exectuples::SoaBatch<'_>,
) {
    match scan {
        TableScanDesc::Heap(_) => unreachable!("staged deform is pgrcolumnar-only"),
        TableScanDesc::Pgrcolumnar(_) => unreachable!("staged deform is pgrcolumnar-only"),
    }
}

/// Stage the staged page batch's varlena sort-key column per `plan`.
pub fn table_scan_batch_stage_varkey<'mcx>(
    scan: &mut TableScanDesc<'mcx>,
    plan: &::exectuples::SoaVarKeyPlan,
    soa: &mut ::exectuples::SoaBatch<'_>,
) {
    match scan {
        TableScanDesc::Heap(h) => ::heapam::heap_batch_stage_varkey(h, plan, soa),
        TableScanDesc::Pgrcolumnar(_) => unreachable!("staged varkey is pgrcolumnar-only"),
    }
}

/// Store tuple `i` of the staged page batch into `slot`.
#[inline(always)]
pub fn table_scan_batch_store_slot<'mcx>(
    mcx: Mcx<'mcx>,
    scan: &mut TableScanDesc<'mcx>,
    i: u32,
    slot: &mut SlotData<'mcx>,
) {
    match scan {
        TableScanDesc::Heap(h) => ::heapam::heap_batch_store_slot(mcx, h, i, slot),
        TableScanDesc::Pgrcolumnar(_) => unreachable!("batch store is pgrcolumnar-only"),
    }
}

/// Staged pgrcolumnar window base for ref-carrying consumers: (row group,
/// rg-global row index of staged row 0); per-row refs are base + i. None =
/// heap or nothing staged (ref mode never starts).
pub fn table_scan_window_ref(scan: &TableScanDesc<'_>) -> Option<(u32, u32)> {
    match scan {
        TableScanDesc::Heap(_) => None,
        TableScanDesc::Pgrcolumnar(_) => None,
    }
}

/// Materialize rg-global row `row` of row group `rg` into `slot` under the
/// scan's current needed set (the ref-carrying bounded-sort drain). false =
/// unsupported AM; the caller must have materialized eagerly at feed time.
pub fn table_scan_gather_row<'mcx>(
    scan: &mut TableScanDesc<'mcx>,
    _rg: u32,
    _row: u32,
    _slot: &mut SlotData<'mcx>,
) -> bool {
    match scan {
        TableScanDesc::Heap(_) => false,
        TableScanDesc::Pgrcolumnar(_) => false,
    }
}

/// Bitmap page-batch feed for the fused drive: stage the next page with
/// visible tuples (visibility resolved at staging); 0 = bitmap exhausted.
pub fn table_scan_bitmap_next_pagebatch<'mcx>(
    mcx: Mcx<'_>,
    scan: &mut TableScanDesc<'mcx>,
    tbm: Option<&tidbitmap::TIDBitmap<'_>>,
    iterator: &mut tidbitmap::TbmIterator,
    recheck: &mut bool,
    lossy_pages: &mut u64,
    exact_pages: &mut u64,
) -> PgResult<u32> {
    match scan {
        TableScanDesc::Heap(h) => ::heapam::bitmap::heap_scan_bitmap_next_pagebatch(
            mcx, h, tbm, iterator, recheck, lossy_pages, exact_pages,
        ),
        TableScanDesc::Pgrcolumnar(c) => {
            let source = c.bitmap.as_deref_mut().ok_or_else(|| cb2_refused("bitmap scans"))?;
            loop {
                match pgrc_bitmap_next_block(source, tbm, iterator, recheck, lossy_pages, exact_pages)? {
                    None => return Ok(0),
                    Some(0) => continue,
                    Some(n) => return Ok(n),
                }
            }
        }
    }
}

/// Store staged bitmap tuple `i` of the current page into `slot`.
#[inline(always)]
pub fn table_scan_bitmap_batch_store_slot<'mcx>(
    mcx: Mcx<'mcx>,
    scan: &mut TableScanDesc<'mcx>,
    i: u32,
    slot: &mut SlotData<'mcx>,
) -> PgResult<()> {
    match scan {
        TableScanDesc::Heap(h) => {
            ::heapam::bitmap::heap_scan_bitmap_batch_store(mcx, h, i, slot);
            Ok(())
        }
        TableScanDesc::Pgrcolumnar(c) => c.bitmap.as_deref_mut().ok_or_else(|| cb2_refused("bitmap scans"))?.store(mcx, i, slot),
    }
}

pub fn table_beginscan_tidrange<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    snapshot: Snapshot<'mcx>,
    mintid: &ItemPointerData,
    maxtid: &ItemPointerData,
) -> PgResult<TableScanDesc<'mcx>> {
    let flags = SO_TYPE_TIDRANGESCAN | SO_ALLOW_PAGEMODE;
    match am(rel) {
        TableAm::Heap => {
            let mut sscan =
                heap::scan_begin(mcx, rel, snapshot, 0, PgVec::new_in(mcx), None, flags)?;
            match &mut sscan {
                TableScanDesc::Heap(h) => heap::scan_set_tidrange(h, mintid, maxtid),
                _ => unreachable!(),
            }
            Ok(sscan)
        }
        TableAm::Pgrcolumnar => Err(::pgrc_am::unsupported("TID range scans")),
    }
}

pub fn table_rescan_tidrange<'mcx>(
    _mcx: Mcx<'mcx>,
    scan: &mut TableScanDesc<'mcx>,
    mintid: &ItemPointerData,
    maxtid: &ItemPointerData,
) -> PgResult<()> {
    debug_assert!((scan.base().rs_flags & SO_TYPE_TIDRANGESCAN) != 0);
    match scan {
        TableScanDesc::Heap(h) => {
            heap::scan_rescan(h, None, false, false, false, false)?;
            heap::scan_set_tidrange(h, mintid, maxtid);
            Ok(())
        }
        TableScanDesc::Pgrcolumnar(_) => Err(::pgrc_am::unsupported("TID range scans")),
    }
}

pub fn table_scan_getnextslot_tidrange<'mcx>(
    mcx: Mcx<'mcx>,
    scan: &mut TableScanDesc<'mcx>,
    direction: ScanDirection,
    slot: &mut SlotData<'mcx>,
) -> PgResult<bool> {
    match scan {
        TableScanDesc::Heap(h) => heap::scan_getnextslot_tidrange(mcx, h, direction, slot),
        TableScanDesc::Pgrcolumnar(_) => Err(::pgrc_am::unsupported("TID range scans")),
    }
}

// --- Parallel table scan (tableam.c) ---

// C's ParallelTableScanDescData shm block: the typed snapshot field replaces
// the phs_snapshot_off byte image (docs/parallel-query-design.md); stable
// address — worker scans hold NonNull into `pscan`.
#[derive(Default)]
pub struct ParallelTableScanDescShared {
    pub pscan: ParallelBlockTableScanDescData,
    pub snapshot: Option<::snapmgr::SerializedSnapshot>,
}

pub fn table_parallelscan_estimate(
    rel: &Relation<'_>,
    snapshot: &Snapshot<'_>,
) -> PgResult<usize> {
    let mut sz: usize = 0;

    match snapshot {
        // EstimateSnapshotSpace adds nothing: the snapshot crosses typed.
        Some(s) => debug_assert!(IsMVCCSnapshot(s)),
        None => {} // Assert(snapshot == SnapshotAny)
    }

    let am_sz = match am(rel) {
        TableAm::Heap => heap::parallelscan_estimate(rel),
        TableAm::Pgrcolumnar => {
            std::mem::size_of::<ParallelBlockTableScanDescData>()
        }
    };
    sz = mcx::add_size(sz, am_sz)?;

    Ok(sz)
}

pub fn table_parallelscan_initialize(
    rel: &Relation<'_>,
    target: &mut ParallelTableScanDescShared,
    snapshot: &Snapshot<'static>,
) -> PgResult<()> {
    let snapshot_off = match am(rel) {
        TableAm::Heap => heap::parallelscan_initialize(rel, &mut target.pscan)?,
        TableAm::Pgrcolumnar => cb2::parallelscan_initialize(rel, &mut target.pscan),
    };
    target.pscan.phs_snapshot_off = snapshot_off;

    match snapshot {
        Some(s) if IsMVCCSnapshot(s) => {
            target.snapshot = Some(::snapmgr::SerializeSnapshot(s));
            target.pscan.phs_snapshot_any = false;
        }
        _ => {
            debug_assert!(snapshot.is_none());
            target.snapshot = None;
            target.pscan.phs_snapshot_any = true;
        }
    }

    Ok(())
}

pub fn table_parallelscan_reinitialize(
    rel: &Relation<'_>,
    pscan: &ParallelBlockTableScanDescData,
) {
    match am(rel) {
        TableAm::Heap => heap::parallelscan_reinitialize(rel, pscan),
        TableAm::Pgrcolumnar => columnar_parallelscan_reinitialize(pscan),
    }
}

pub fn table_beginscan_parallel<'mcx>(
    mcx: Mcx<'mcx>,
    relation: &Relation<'mcx>,
    parallel_scan: &ParallelTableScanDescShared,
) -> PgResult<TableScanDesc<'mcx>> {
    debug_assert!(relation.rd_locator.get() == parallel_scan.pscan.phs_locator);

    let mut flags = SO_TYPE_SEQSCAN | SO_ALLOW_STRAT | SO_ALLOW_SYNC | SO_ALLOW_PAGEMODE;
    let mut registered = None;
    let snapshot: Snapshot<'mcx> = if !parallel_scan.pscan.phs_snapshot_any {
        let serialized = parallel_scan
            .snapshot
            .as_ref()
            .expect("MVCC parallel scan carries its serialized snapshot");
        let snap = ::snapmgr::RestoreSnapshot(serialized);
        let snap = ::snapmgr::RegisterSnapshot(Some(&snap))?.expect("registered a snapshot");
        flags |= SO_TEMP_SNAPSHOT;
        registered = Some(snap.clone());
        Some(snap)
    } else {
        None
    };

    // heapam rs_parallel contract: the shared descriptor outlives the scan.
    let pscan_ptr = NonNull::from(&parallel_scan.pscan);
    match am(relation) {
        TableAm::Heap => {
            let mut scan = heap::scan_begin(
                mcx,
                relation,
                snapshot,
                0,
                PgVec::new_in(mcx),
                Some(pscan_ptr),
                flags,
            )?;
            match &mut scan {
                TableScanDesc::Heap(h) => h.rs_temp_snapshot = registered,
                _ => unreachable!(),
            }
            Ok(scan)
        }
        TableAm::Pgrcolumnar => {
            let mut scan = cb2::scan_begin(mcx, relation, snapshot, flags, Some(pscan_ptr))?;
            match &mut scan {
                TableScanDesc::Pgrcolumnar(c) => c.rs_temp_snapshot = registered,
                _ => unreachable!(),
            }
            Ok(scan)
        }
    }
}

// --- Index scan related functions (tableam.h / tableam.c) ---

pub fn table_index_fetch_begin<'mcx>(rel: &Relation<'mcx>) -> PgResult<IndexFetchTableData<'mcx>> {
    match am(rel) {
        TableAm::Heap => {
            Ok(IndexFetchTableData::Heap(::heapam_handler::heapam_index_fetch_begin(rel)))
        }
        TableAm::Pgrcolumnar => {
            ensure_pgrc_buffer_hooks();
            Ok(IndexFetchTableData::Pgrc(Box::new(::pgrc_front::ixfetch::begin(rel))))
        }
    }
}

/// IX1 L0: whether a TID's block number is a page of the relation's main fork, so an index scan may prefetch it and
/// batch the TIDs of one block (heap); a pgrcolumnar TID names a row ID, not a page.
pub fn table_index_fetch_reads_pages(scan: &IndexFetchTableData<'_>) -> bool {
    matches!(scan, IndexFetchTableData::Heap(_))
}

/// IX1 L0 (choice 5): whether an index-only scan may skip the fetch of `tid` of `rel` (heap: its page is all-visible
/// in the visibility map, `vm_all_visible`). No pgrcolumnar row is known all-visible: every one is fetched.
pub fn table_index_fetch_all_visible(rel: &Relation<'_>, tid: &ItemPointerData, vm_all_visible: impl FnOnce(BlockNumber) -> PgResult<bool>) -> PgResult<bool> {
    match am(rel) {
        TableAm::Heap => vm_all_visible(ItemPointerGetBlockNumber(tid)),
        TableAm::Pgrcolumnar => Ok(false),
    }
}

pub fn table_index_fetch_reset(scan: &mut IndexFetchTableData<'_>) {
    match scan {
        IndexFetchTableData::Heap(h) => ::heapam_handler::heapam_index_fetch_reset(h),
        IndexFetchTableData::Pgrc(p) => p.reset(),
    }
}

pub fn table_index_fetch_end(scan: IndexFetchTableData<'_>) {
    match scan {
        IndexFetchTableData::Heap(h) => ::heapam_handler::heapam_index_fetch_end(h),
        IndexFetchTableData::Pgrc(p) => p.end(),
    }
}

pub fn table_index_fetch_tuple<'mcx>(
    mcx: Mcx<'mcx>,
    scan: &mut IndexFetchTableData<'mcx>,
    tid: &mut ItemPointerData,
    snapshot: &mut Snapshot<'mcx>,
    slot: &mut SlotData<'mcx>,
    call_again: &mut bool,
    all_dead: Option<&mut bool>,
) -> PgResult<bool> {
    if unexpected_during_logical_decoding() {
        return Err(elog_error(
            "unexpected table_index_fetch_tuple call during logical decoding",
        ));
    }
    match scan {
        IndexFetchTableData::Heap(h) => ::heapam_handler::heapam_index_fetch_tuple(
            mcx, h, tid, snapshot, slot, call_again, all_dead,
        ),
        IndexFetchTableData::Pgrc(p) => ::pgrc_front::ixfetch::fetch(mcx, p, tid, snapshot, slot, call_again, all_dead),
    }
}

pub use ::heapam_handler::{BatchFetch, INDEX_FETCH_BATCH_MAX};

pub fn table_index_fetch_batch_fill<'mcx>(
    mcx: Mcx<'mcx>,
    scan: &mut IndexFetchTableData<'mcx>,
    first_tid: &ItemPointerData,
    rest: &[ItemPointerData],
    snapshot: &Snapshot<'mcx>,
) -> PgResult<()> {
    match scan {
        IndexFetchTableData::Heap(h) => {
            ::heapam_handler::heapam_index_fetch_batch_fill(mcx, h, first_tid, rest, snapshot)
        }
        // never armed: `table_index_fetch_reads_pages` is false
        IndexFetchTableData::Pgrc(_) => Ok(()),
    }
}

pub fn table_index_fetch_batch_next<'mcx>(
    mcx: Mcx<'mcx>,
    scan: &mut IndexFetchTableData<'mcx>,
    tid: &mut ItemPointerData,
    slot: &mut SlotData<'mcx>,
) -> ::heapam_handler::BatchFetch {
    match scan {
        IndexFetchTableData::Heap(h) => {
            ::heapam_handler::heapam_index_fetch_batch_next(mcx, h, tid, slot)
        }
        IndexFetchTableData::Pgrc(_) => ::heapam_handler::BatchFetch::Miss,
    }
}

pub fn table_index_fetch_tuple_check<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    tid: &mut ItemPointerData,
    snapshot: &mut Snapshot<'mcx>,
    all_dead: Option<&mut bool>,
) -> PgResult<bool> {
    let mut call_again = false;

    let mut slot = table_slot_create(mcx, rel)?;
    let mut scan = table_index_fetch_begin(rel)?;
    let found = table_index_fetch_tuple(
        mcx,
        &mut scan,
        tid,
        snapshot,
        &mut slot,
        &mut call_again,
        all_dead,
    )?;
    table_index_fetch_end(scan);
    // ExecDropSingleTupleTableSlot: the owned slot drops here.
    exectuples::exec_clear_tuple(&mut slot, mcx);

    Ok(found)
}

pub fn table_index_delete_tuples<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    delstate: &mut TM_IndexDeleteOp<'mcx>,
) -> PgResult<TransactionId> {
    match am(rel) {
        TableAm::Heap => heap::index_delete_tuples(mcx, rel, delstate),
        // IX1 L3: each TID judged dead to everyone on the newest generation (DV-marked, a committed tombstone below the
        // horizon, a vacuumable segment row, no home and no bulk load in flight), with its conflict horizon.
        TableAm::Pgrcolumnar => ::pgrc_front::ixclean::index_delete_tuples(mcx, rel, delstate),
    }
}

// --- Non-modifying operations on individual tuples ---

pub fn table_tuple_fetch_row_version<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    tid: &ItemPointerData,
    snapshot: &Snapshot<'mcx>,
    slot: &mut SlotData<'mcx>,
) -> PgResult<bool> {
    if unexpected_during_logical_decoding() {
        return Err(elog_error(
            "unexpected table_tuple_fetch_row_version call during logical decoding",
        ));
    }
    match am(rel) {
        TableAm::Heap => {
            ::heapam_handler::heapam_fetch_row_version(mcx, rel, tid, snapshot, slot)
        }
        TableAm::Pgrcolumnar => ::pgrc_front::dml::fetch(mcx, rel, tid, snapshot, slot),
    }
}

/// TRIGGERS: the stable identity of a pgrcolumnar row at a position its statement minted (`None`: another AM, whose
/// row positions outlive the statement). A deferrable AFTER row trigger event keeps it to fetch the row at COMMIT.
pub fn table_tuple_stable_ident<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, tid: &ItemPointerData) -> PgResult<Option<u64>> {
    match am(rel) {
        TableAm::Pgrcolumnar => ::pgrc_front::dml::rowident_of(mcx, rel, tid).map(Some),
        TableAm::Heap => Ok(None),
    }
}

/// TRIGGERS: [`table_tuple_fetch_row_version`] under `SnapshotAny` by the identity [`table_tuple_stable_ident`]
/// returned; `false`: no such row.
pub fn table_tuple_fetch_stable_ident<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, ident: u64, slot: &mut SlotData<'mcx>) -> PgResult<bool> {
    match am(rel) {
        TableAm::Pgrcolumnar => ::pgrc_front::fetch::fetch_rowident(mcx, rel, ident, slot),
        TableAm::Heap => Err(elog_error("table_tuple_fetch_stable_ident: the table access method has no stable row identities")),
    }
}

/// IX1 L0 (OC-C c, main 2026-10-06): the position of the row `scan` last returned, for a caller whose slot carries
/// none (`WHERE CURRENT OF` over a pgrcolumnar scan that stamps no positions, ROWMAP2's `TidNeed::None`): that one
/// row's ID resolved through its part's map. `None`: heap (its slots always carry one), or no such row.
pub fn table_scan_current_tid(scan: &TableScanDesc<'_>) -> PgResult<Option<ItemPointerData>> {
    match scan {
        TableScanDesc::Heap(_) => Ok(None),
        TableScanDesc::Pgrcolumnar(c) => Ok(c.current_rowid()?.map(|id| {
            let (block, off) = ::pgrc_format::rowid::tid_of(id);
            ItemPointerData::new(block, off)
        })),
    }
}

pub fn table_tuple_tid_valid(scan: &mut TableScanDesc<'_>, tid: &ItemPointerData) -> bool {
    match scan {
        TableScanDesc::Heap(h) => ::heapam_handler::heapam_tuple_tid_valid(h, tid),
        // IX1 L0: a TID that names a row ID the counter reserved before the scan began
        TableScanDesc::Pgrcolumnar(c) => c.tid_valid(ItemPointerGetBlockNumberNoCheck(tid), ItemPointerGetOffsetNumberNoCheck(tid)),
    }
}

pub fn table_tuple_get_latest_tid<'mcx>(
    mcx: Mcx<'mcx>,
    scan: &mut TableScanDesc<'mcx>,
    tid: &mut ItemPointerData,
) -> PgResult<()> {
    if unexpected_during_logical_decoding() {
        return Err(elog_error(
            "unexpected table_tuple_get_latest_tid call during logical decoding",
        ));
    }

    // User-supplied TID: don't trust the input too much.
    if !table_tuple_tid_valid(scan, tid) {
        let blk = ItemPointerGetBlockNumber(tid);
        let off = tid.ip_posid;
        let relname = scan.base().rs_rd.name();
        return Err(Box::new(
            PgError::error(format!(
                "tid ({blk}, {off}) is not valid for relation \"{relname}\""
            ))
            .with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE)
            .with_location("tableam.c", 259, "table_tuple_get_latest_tid"),
        ));
    }

    match scan {
        TableScanDesc::Heap(h) => ::heapam_handler::heapam_tuple_get_latest_tid(h, tid),
        // IX1 L0: along the row's update chain (its tombstones' successors), the newest version the scan's snapshot sees
        TableScanDesc::Pgrcolumnar(c) => {
            let snap = c.rs_base.rs_snapshot.clone();
            ::pgrc_front::ixfetch::latest_tid(mcx, &c.rs_base.rs_rd, &snap, tid)
        }
    }
}

pub fn table_tuple_satisfies_snapshot<'mcx>(
    rel: &Relation<'mcx>,
    slot: &mut SlotData<'mcx>,
    snapshot: &Snapshot<'mcx>,
) -> PgResult<bool> {
    match am(rel) {
        TableAm::Heap => ::heapam_handler::heapam_tuple_satisfies_snapshot(rel, slot, snapshot),
        TableAm::Pgrcolumnar => Err(::pgrc_am::unsupported("tuple visibility rechecks")),
    }
}

// --- Manipulations of physical tuples (tableam.h wrappers) ---

thread_local! {
    static CB2_BUFFER_HOOKS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The pgrcolumnar statement buffer (`pgrc_front::xact`) dies with the (sub)transaction that filled it: an errored
/// statement's rows never reach the next statement. Rows still buffered at commit missed their
/// `table_finish_bulk_insert` and refuse the commit, like the bulk lane's writers. Row caches empty here too (D4674).
fn ensure_pgrc_buffer_hooks() {
    CB2_BUFFER_HOOKS.with(|c| {
        if !c.get() {
            xact::RegisterXactCallback(pgrc_buffer_xact_callback, ::datum::Datum::null());
            xact::RegisterSubXactCallback(pgrc_buffer_subxact_callback, ::datum::Datum::null());
            c.set(true);
        }
    });
}

fn pgrc_buffer_xact_callback(event: ::types_core::xact::XactEvent, _arg: ::datum::Datum) -> PgResult<()> {
    use ::types_core::xact::XactEvent::*;
    match event {
        XACT_EVENT_PRE_COMMIT | XACT_EVENT_PARALLEL_PRE_COMMIT | XACT_EVENT_PRE_PREPARE => match ::pgrc_front::xact::pending() {
            Some(relid) => Err(Box::new(
                PgError::error(format!("pgrcolumnar: buffered rows for relation {relid} reached commit without table_finish_bulk_insert"))
                    .with_sqlstate(::types_error::ERRCODE_INTERNAL_ERROR),
            )),
            None => match ::pgrc_front::xact::pending_indexed() {
                Some(relid) => Err(Box::new(
                    PgError::error(format!("pgrcolumnar: rows of relation {relid} reached commit without their index entries"))
                        .with_sqlstate(::types_error::ERRCODE_INTERNAL_ERROR),
                )),
                None => ::pgrc_front::logical::at_xact_event(event),
            },
        },
        _ => {
            ::pgrc_front::xact::discard(::types_core::xact::TopSubTransactionId);
            ::pgrc_am::rowcache::disarm_all();
            ::pgrc_front::logical::at_xact_event(event)
        }
    }
}

fn pgrc_buffer_subxact_callback(
    event: ::types_core::xact::SubXactEvent,
    my: ::types_core::xact::SubTransactionId,
    parent: ::types_core::xact::SubTransactionId,
    _arg: ::datum::Datum,
) -> PgResult<()> {
    use ::types_core::xact::SubXactEvent::*;
    match event {
        SUBXACT_EVENT_ABORT_SUB => {
            ::pgrc_front::xact::discard(my);
            ::pgrc_front::logical::at_subxact_abort(my);
        }
        SUBXACT_EVENT_COMMIT_SUB => {
            ::pgrc_front::xact::reparent(my, parent);
            ::pgrc_front::logical::at_subxact_commit(my, parent);
        }
        SUBXACT_EVENT_START_SUB | SUBXACT_EVENT_PRE_COMMIT_SUB => {}
    }
    Ok(())
}

pub fn table_tuple_insert<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    slot: &mut SlotData<'mcx>,
    cid: CommandId,
    options: i32,
    bistate: Option<&mut BulkInsertStateData>,
) -> PgResult<()> {
    match am(rel) {
        TableAm::Heap => heap::tuple_insert(mcx, rel, slot, cid, options, bistate),
        TableAm::Pgrcolumnar => cb2::insert(mcx, rel, slot, cid, options, bistate.is_some()),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn table_tuple_insert_speculative<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    slot: &mut SlotData<'mcx>,
    cid: CommandId,
    options: i32,
    bistate: Option<&mut BulkInsertStateData>,
    spec_token: u32,
) -> PgResult<()> {
    match am(rel) {
        TableAm::Heap => {
            heap::tuple_insert_speculative(mcx, rel, slot, cid, options, bistate, spec_token)
        }
        TableAm::Pgrcolumnar => Err(::pgrc_am::unsupported("INSERT ... ON CONFLICT")),
    }
}

pub fn table_tuple_complete_speculative<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    slot: &mut SlotData<'mcx>,
    spec_token: u32,
    succeeded: bool,
) -> PgResult<()> {
    match am(rel) {
        TableAm::Heap => heap::tuple_complete_speculative(mcx, rel, slot, spec_token, succeeded),
        TableAm::Pgrcolumnar => Err(::pgrc_am::unsupported("INSERT ... ON CONFLICT")),
    }
}

/// Caller contract for the buffering AMs (the F-1 census, M5e scout §1):
/// on a pgrcolumnar / pgrcolumnar relation the inserted rows BUFFER in the
/// per-statement writer and are only published by
/// [`table_finish_bulk_insert`]; anything still buffered at transaction end
/// is unconditionally purged (the eoxact abandonment law). Every caller
/// MUST therefore reach `table_finish_bulk_insert` on its success path —
/// COPY FROM does, the CTAS/matview receivers do, and the W1 write buffer
/// never arms for these AMs (`write_buffer_begin` is heap-only, test-pinned).
/// ModifyTable-driven trickle DML never gets here: it refuses typed at
/// `check_valid_result_rel` (the restored M3-H gate).
pub fn table_multi_insert<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    slots: &mut [&mut SlotData<'mcx>],
    cid: CommandId,
    options: i32,
    bistate: Option<&mut BulkInsertStateData>,
) -> PgResult<()> {
    match am(rel) {
        TableAm::Heap => heap::multi_insert(mcx, rel, slots, cid, options, bistate),
        TableAm::Pgrcolumnar => {
            let bistate = bistate.is_some();
            for slot in slots.iter_mut() {
                cb2::insert(mcx, rel, slot, cid, options, bistate)?;
            }
            Ok(())
        }
    }
}

// heapam_methods leaves the optional finish_bulk_insert slot NULL, so for the
// only shipped AM the C inline is a no-op after the rd_tableam probe.
pub fn table_finish_bulk_insert(rel: &Relation<'_>, _options: i32) -> PgResult<()> {
    match am(rel) {
        TableAm::Heap => Ok(()),
        TableAm::Pgrcolumnar => {
            cb2::flush(rel)?;
            // COMPACTP item 4: the L0 cap's wait, before the publish and within the statement's budget
            if ::pgrc_am::ingest::bulk_open(rel) {
                ::pgrc_front::route::l0_backpressure(rel)?;
            }
            ::pgrc_front::route::statement_end();
            ::pgrc_front::logical::flush(rel)?;
            // AUTOSTATS: the bulk lane's rows count as the parent's inserts once published
            let published = ::pgrc_am::ingest::finish_bulk(rel)?;
            ::pgrc_front::stats::inserted(rel, published);
            // COMPACTP item 1: the part-count trigger
            if published > 0 {
                ::pgrc_front::compact::bulk_published(rel);
            }
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn table_tuple_delete<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    tid: &ItemPointerData,
    cid: CommandId,
    snapshot: &Snapshot<'mcx>,
    crosscheck: &Snapshot<'mcx>,
    wait: bool,
    tmfd: &mut TM_FailureData,
    changingPart: bool,
) -> PgResult<TM_Result> {
    match am(rel) {
        TableAm::Heap => heap::tuple_delete(
            mcx,
            rel,
            tid,
            cid,
            snapshot,
            crosscheck,
            wait,
            tmfd,
            changingPart,
        ),
        TableAm::Pgrcolumnar => ::pgrc_front::dml::delete(mcx, rel, tid, cid, snapshot, wait, tmfd, changingPart),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn table_tuple_update<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    otid: &ItemPointerData,
    slot: &mut SlotData<'mcx>,
    cid: CommandId,
    snapshot: &Snapshot<'mcx>,
    crosscheck: &Snapshot<'mcx>,
    wait: bool,
    tmfd: &mut TM_FailureData,
    lockmode: &mut LockTupleMode,
    update_indexes: &mut TU_UpdateIndexes,
) -> PgResult<TM_Result> {
    match am(rel) {
        TableAm::Heap => heap::tuple_update(
            mcx,
            rel,
            otid,
            slot,
            cid,
            snapshot,
            crosscheck,
            wait,
            tmfd,
            lockmode,
            update_indexes,
        ),
        TableAm::Pgrcolumnar => {
            // IX1 L2: the new version has a new ID, so every index takes an entry for it
            *update_indexes = TU_UpdateIndexes::TU_All;
            ::pgrc_front::dml::update(mcx, rel, otid, slot, cid, snapshot, wait, tmfd, lockmode)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn table_tuple_lock<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    tid: &ItemPointerData,
    snapshot: &Snapshot<'mcx>,
    slot: &mut SlotData<'mcx>,
    cid: CommandId,
    mode: LockTupleMode,
    wait_policy: LockWaitPolicy,
    flags: u8,
    tmfd: &mut TM_FailureData,
) -> PgResult<TM_Result> {
    match am(rel) {
        TableAm::Heap => heap::tuple_lock(
            mcx,
            rel,
            tid,
            snapshot,
            slot,
            cid,
            mode,
            wait_policy,
            flags,
            tmfd,
        ),
        TableAm::Pgrcolumnar => ::pgrc_front::dml::lock(mcx, rel, tid, snapshot, slot, cid, mode, wait_policy, flags, tmfd),
    }
}

// --- DDL-adjacent dispatch (tableam.h wrappers) ---

pub fn table_relation_set_new_filelocator(
    rel: &Relation<'_>,
    newrlocator: &RelFileLocator,
    relpersistence: i8,
) -> PgResult<(TransactionId, TransactionId)> {
    match am(rel) {
        // Storage create/reset is AM-independent (main fork file); pgrcolumnar
        // reuses the heap arm — an empty file is a valid empty part.
        TableAm::Heap => {
            heap::relation_set_new_filelocator(rel, newrlocator, relpersistence)
        }
        // pgrcolumnar: the (empty) main fork rides the heap arm and the table directory is
        // created beside it, logged like the fork (a temp relation logs neither). The old
        // locator's directory goes with its fork (catalog_storage).
        TableAm::Pgrcolumnar => {
            let ret = heap::relation_set_new_filelocator(rel, newrlocator, relpersistence)?;
            ::pgrc_am::dirpath::create_table_dir(*newrlocator, rel.rd_backend)?;
            if relpersistence as u8 != ::types_core::catalog::RELPERSISTENCE_TEMP {
                ::pgrc_wal::insert::log_dir_create(::pgrc_wal::dir_loc(*newrlocator))?;
            }
            ::pgrc_am::inval::invalidate_relid(rel.rd_id);
            Ok(ret)
        }
    }
}

/// Drop the relation's physical storage at transaction commit (`heap_drop_with_catalog`'s
/// `RelationDropStorage`); a pgrcolumnar directory is removed with the main fork.
pub fn table_relation_drop_storage(rel: &Relation<'_>) -> PgResult<()> {
    catalog_storage::RelationDropStorage(rel)
}

pub fn table_relation_copy_data(rel: &Relation<'_>, newrlocator: &RelFileLocator) -> PgResult<()> {
    match am(rel) {
        TableAm::Heap => heap::relation_copy_data(rel, newrlocator),
        TableAm::Pgrcolumnar => Err(::pgrc_am::unsupported("CLUSTER / rewrites")),
    }
}

pub fn table_relation_nontransactional_truncate(rel: &Relation<'_>) -> PgResult<()> {
    match am(rel) {
        TableAm::Heap => heap::relation_nontransactional_truncate(rel),
        // pgrcolumnar: truncate the main fork and empty the table directory, logged like
        // `RelationTruncate` (callers guarantee safety — the heap_truncate_one_rel contract).
        TableAm::Pgrcolumnar => {
            heap::relation_nontransactional_truncate(rel)?;
            let locator = rel.rd_locator.get();
            if ::pgrc_wal::insert::relation_needs_wal(rel) {
                ::pgrc_wal::insert::log_truncate(::pgrc_wal::dir_loc(locator))?;
            }
            ::pgrc_am::dirpath::truncate_table_dir(locator, rel.rd_backend)?;
            ::pgrc_am::inval::invalidate_relid(rel.rd_id);
            Ok(())
        }
    }
}

pub fn table_relation_needs_toast_table(rel: &Relation<'_>) -> bool {
    match am(rel) {
        TableAm::Heap => heap::relation_needs_toast_table(rel),
        TableAm::Pgrcolumnar => false,
    }
}

pub fn table_relation_toast_am(rel: &Relation<'_>) -> ::types_core::Oid {
    match am(rel) {
        TableAm::Heap => heap::relation_toast_am(rel),
        TableAm::Pgrcolumnar => 0,
    }
}

pub fn table_relation_size(rel: &Relation<'_>, forkNumber: ForkNumber) -> PgResult<u64> {
    match am(rel) {
        // pgrcolumnar's main fork is an empty file: relation_size 0 keeps
        // the planner honest about pages (row counts come from ANALYZE).
        TableAm::Heap | TableAm::Pgrcolumnar => {
            heap::relation_size(rel, forkNumber)
        }
    }
}

// --- ANALYZE / bitmap / sample scan dispatch (tableam.h wrappers) ---

pub fn table_scan_analyze_next_block<'mcx>(
    mcx: Mcx<'mcx>,
    scan: &mut TableScanDesc<'mcx>,
    next_buffer: &mut dyn FnMut() -> PgResult<Buffer>,
) -> PgResult<bool> {
    match scan {
        TableScanDesc::Heap(h) => heap::scan_analyze_next_block(mcx, h, next_buffer),
        TableScanDesc::Pgrcolumnar(_) => Err(::pgrc_am::unsupported("ANALYZE")),
    }
}

pub fn table_scan_analyze_next_tuple<'mcx>(
    mcx: Mcx<'mcx>,
    scan: &mut TableScanDesc<'mcx>,
    oldest_xmin: TransactionId,
    liverows: &mut f64,
    deadrows: &mut f64,
    slot: &mut SlotData<'mcx>,
) -> PgResult<bool> {
    match scan {
        TableScanDesc::Heap(h) => {
            heap::scan_analyze_next_tuple(mcx, h, oldest_xmin, liverows, deadrows, slot)
        }
        TableScanDesc::Pgrcolumnar(_) => Err(::pgrc_am::unsupported("ANALYZE")),
    }
}

// C divergence: the TBM iterator + bitmap arrive as parameters (C rides them
// in rs_base.st.rs_tbmiterator; ours carries no bitmap back-pointer).
#[allow(clippy::too_many_arguments)]
pub fn table_scan_bitmap_next_tuple<'mcx>(
    mcx: Mcx<'mcx>,
    scan: &mut TableScanDesc<'mcx>,
    tbm: Option<&tidbitmap::TIDBitmap<'_>>,
    iterator: &mut tidbitmap::TbmIterator,
    slot: &mut SlotData<'mcx>,
    recheck: &mut bool,
    lossy_pages: &mut u64,
    exact_pages: &mut u64,
) -> PgResult<bool> {
    match scan {
        TableScanDesc::Heap(h) => {
            heap::scan_bitmap_next_tuple(mcx, h, tbm, iterator, slot, recheck, lossy_pages, exact_pages)
        }
        // IX1 L4: the staged page's next row, else the next page's
        TableScanDesc::Pgrcolumnar(c) => {
            let source = c.bitmap.as_deref_mut().ok_or_else(|| cb2_refused("bitmap scans"))?;
            loop {
                if source.next(mcx, slot)? {
                    return Ok(true);
                }
                if pgrc_bitmap_next_block(source, tbm, iterator, recheck, lossy_pages, exact_pages)?.is_none() {
                    return Ok(false);
                }
            }
        }
    }
}

pub fn table_scan_sample_next_block<'mcx>(
    mcx: Mcx<'mcx>,
    scan: &mut TableScanDesc<'mcx>,
    scanstate: &mut dyn SampleScanDriver,
    donetuples: i64,
) -> PgResult<bool> {
    if unexpected_during_logical_decoding() {
        return Err(elog_error(
            "unexpected table_scan_sample_next_block call during logical decoding",
        ));
    }
    match scan {
        TableScanDesc::Heap(h) => heap::scan_sample_next_block(mcx, h, scanstate, donetuples),
        TableScanDesc::Pgrcolumnar(c) => c.sample_next_block(scanstate, donetuples),
    }
}

pub fn table_scan_sample_next_tuple<'mcx>(
    mcx: Mcx<'mcx>,
    scan: &mut TableScanDesc<'mcx>,
    scanstate: &mut dyn SampleScanDriver,
    donetuples: i64,
    slot: &mut SlotData<'mcx>,
) -> PgResult<bool> {
    if unexpected_during_logical_decoding() {
        return Err(elog_error(
            "unexpected table_scan_sample_next_tuple call during logical decoding",
        ));
    }
    match scan {
        TableScanDesc::Heap(h) => {
            heap::scan_sample_next_tuple(mcx, h, scanstate, donetuples, slot)
        }
        TableScanDesc::Pgrcolumnar(c) => c.sample_next_tuple(mcx, scanstate, donetuples, slot),
    }
}

// --- Functions to make modifications a bit simpler (tableam.c) ---

pub fn simple_table_tuple_insert<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    slot: &mut SlotData<'mcx>,
) -> PgResult<()> {
    let cid = xact_seams::get_current_command_id::call(true)?;
    table_tuple_insert(mcx, rel, slot, cid, 0, None)
}

pub fn simple_table_tuple_delete<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    tid: &ItemPointerData,
    snapshot: &Snapshot<'mcx>,
) -> PgResult<()> {
    let mut tmfd = TM_FailureData::default();

    let cid = xact_seams::get_current_command_id::call(true)?;
    let result = table_tuple_delete(
        mcx, rel, tid, cid, snapshot, &None, /* InvalidSnapshot */
        true,  /* wait for commit */
        &mut tmfd, false, /* changingPart */
    )?;

    match result {
        TM_Result::TM_SelfModified => Err(elog_error("tuple already updated by self")),
        TM_Result::TM_Ok => Ok(()),
        TM_Result::TM_Updated => Err(elog_error("tuple concurrently updated")),
        TM_Result::TM_Deleted => Err(elog_error("tuple concurrently deleted")),
        other => Err(elog_error(format!(
            "unrecognized table_tuple_delete status: {}",
            other as u32
        ))),
    }
}

pub fn simple_table_tuple_update<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    otid: &ItemPointerData,
    slot: &mut SlotData<'mcx>,
    snapshot: &Snapshot<'mcx>,
    update_indexes: &mut TU_UpdateIndexes,
) -> PgResult<()> {
    let mut tmfd = TM_FailureData::default();
    let mut lockmode = LockTupleMode::LockTupleNoKeyExclusive;

    let cid = xact_seams::get_current_command_id::call(true)?;
    let result = table_tuple_update(
        mcx,
        rel,
        otid,
        slot,
        cid,
        snapshot,
        &None, /* InvalidSnapshot */
        true,  /* wait for commit */
        &mut tmfd,
        &mut lockmode,
        update_indexes,
    )?;

    match result {
        TM_Result::TM_SelfModified => Err(elog_error("tuple already updated by self")),
        TM_Result::TM_Ok => Ok(()),
        TM_Result::TM_Updated => Err(elog_error("tuple concurrently updated")),
        TM_Result::TM_Deleted => Err(elog_error("tuple concurrently deleted")),
        other => Err(elog_error(format!(
            "unrecognized table_tuple_update status: {}",
            other as u32
        ))),
    }
}

// --- Parallel-scan sizing for block-oriented AMs (tableam.c) ---

pub fn table_block_parallelscan_estimate(_rel: &Relation<'_>) -> usize {
    core::mem::size_of::<ParallelBlockTableScanDescData>()
}

pub fn table_block_parallelscan_initialize(
    rel: &Relation<'_>,
    pscan: &mut ParallelBlockTableScanDescData,
) -> PgResult<usize> {
    use core::sync::atomic::{AtomicU32, AtomicU64};
    use ::types_core::primitive::InvalidBlockNumber;
    use ::types_storage::Spinlock;

    pscan.phs_locator = relation_locator(rel);
    let phs_nblocks = relation_nblocks(rel)?;
    pscan.phs_nblocks = phs_nblocks;
    // compare phs_syncscan initialization to similar logic in initscan
    pscan.phs_syncscan = synchronize_seqscans()
        && !rel.uses_local_buffers()
        && phs_nblocks > (nbuffers()? / 4) as BlockNumber;
    pscan.phs_mutex = Spinlock::new();
    pscan.phs_startblock = AtomicU32::new(InvalidBlockNumber);
    pscan.phs_nallocated = AtomicU64::new(0);

    Ok(core::mem::size_of::<ParallelBlockTableScanDescData>())
}

pub fn table_block_parallelscan_reinitialize(
    _rel: &Relation<'_>,
    pscan: &ParallelBlockTableScanDescData,
) {
    pscan
        .phs_nallocated
        .store(0, core::sync::atomic::Ordering::SeqCst);
}

// --- Relation-sizing helpers for block-oriented AMs (tableam.c) ---

pub fn table_block_relation_size(rel: &Relation<'_>, forkNumber: ForkNumber) -> PgResult<u64> {
    let h = smgr::RelationGetSmgr(rel)?;
    let mut nblocks: u64 = 0;
    if forkNumber == ForkNumber::InvalidForkNumber {
        // C sums forks i < MAX_FORKNUM, i.e. INIT_FORKNUM excluded (bug-compat).
        for fork in [
            ForkNumber::MAIN_FORKNUM,
            ForkNumber::FSM_FORKNUM,
            ForkNumber::VISIBILITYMAP_FORKNUM,
        ] {
            nblocks += smgr::smgrnblocks_h(h, fork)? as u64;
        }
    } else {
        nblocks = smgr::smgrnblocks_h(h, forkNumber)? as u64;
    }
    Ok(nblocks * ::types_core::BLCKSZ as u64)
}

/// `table_relation_estimate_size` for the heap AM (heapam_estimate_rel_size ->
/// table_block_relation_estimate_size). `get_rel_data_width` lives in the
/// planner's plancat (a direct dep here would cycle), so the never-vacuumed
/// density fallback arrives as `data_width` (called with the attr-width cache).
#[allow(clippy::too_many_arguments)]
pub fn table_relation_estimate_size(
    rel: &Relation<'_>,
    overhead_bytes_per_tuple: usize,
    usable_bytes_per_page: usize,
    data_width: impl FnOnce(Option<&mut [i32]>) -> PgResult<i32>,
    mut attr_widths: Option<&mut [i32]>,
    pages: &mut BlockNumber,
    tuples: &mut f64,
    allvisfrac: &mut f64,
) -> PgResult<()> {
    // pgrcolumnar (WW-2): the main fork is a deliberately EMPTY smgr file
    // (the O-7 directory lifecycle), so the heap math below sized every
    // pgrc rel at 0 pages / 0 tuples — plan-time tuples were structurally
    // zero, analyzed or not (curpages == 0 short-circuits before reltuples
    // is consulted). tuples/pages come from the effective manifest's part
    // records (rows exact, pages = on-disk part bytes at BLCKSZ grain). allvisfrac 0.0: no
    // visibility map, index-only scans refuse. A never-published table
    // falls through to C's never-analyzed convention below.
    if am(rel) == TableAm::Pgrcolumnar {
        if let Some((nrows, nbytes)) = ::pgrc_am::footer::footer_size(rel)? {
            *pages = nbytes.div_ceil(::types_core::BLCKSZ as u64) as BlockNumber;
            *tuples = nrows as f64;
            *allvisfrac = 0.0;
            return Ok(());
        }
    }
    let curpages =
        bufmgr_seams::relation_get_number_of_blocks_in_fork::call(rel, ForkNumber::MAIN_FORKNUM)?;
    let fillfactor = rel.get_fillfactor(HEAP_DEFAULT_FILLFACTOR);
    block_relation_estimate_size_math(
        curpages,
        rel.rd_rel.relpages as BlockNumber,
        rel.rd_rel.reltuples as f64,
        rel.rd_rel.relallvisible as BlockNumber,
        rel.rd_rel.relhassubclass,
        |aw| {
            let mut tuple_width = data_width(aw)? as usize;
            tuple_width += overhead_bytes_per_tuple;
            // C Size arithmetic: integer division is intentional.
            let raw = usable_bytes_per_page * fillfactor as usize / 100 / tuple_width;
            Ok(clamp_row_est(raw as f64))
        },
        attr_widths.take(),
        pages,
        tuples,
        allvisfrac,
    )
}

// clamp_row_est (costsize.c); grounded here for the density fallback (the
// costsize copy lives with the planner).
fn clamp_row_est(nrows: f64) -> f64 {
    const MAXIMUM_ROWCOUNT: f64 = 1e100;
    if nrows > MAXIMUM_ROWCOUNT || nrows.is_nan() {
        MAXIMUM_ROWCOUNT
    } else if nrows <= 1.0 {
        1.0
    } else {
        nrows.round_ties_even()
    }
}

#[allow(clippy::too_many_arguments)]
pub fn table_block_relation_estimate_size(
    rel: &Relation<'_>,
    attr_widths: Option<&mut [i32]>,
    pages: &mut BlockNumber,
    tuples: &mut f64,
    allvisfrac: &mut f64,
    overhead_bytes_per_tuple: usize,
    usable_bytes_per_page: usize,
) -> PgResult<()> {
    let curpages = relation_nblocks(rel)?;
    let _ = rel.get_fillfactor(HEAP_DEFAULT_FILLFACTOR);
    let _ = (overhead_bytes_per_tuple, usable_bytes_per_page);
    block_relation_estimate_size_math(
        curpages,
        rel.rd_rel.relpages as BlockNumber,
        rel.rd_rel.reltuples as f64,
        rel.rd_rel.relallvisible as BlockNumber,
        rel.rd_rel.relhassubclass,
        // Never-vacuumed density fallback needs planner units, unported.
        |_aw| unported("backend-optimizer-util-plancat (get_rel_data_width)"),
        attr_widths,
        pages,
        tuples,
        allvisfrac,
    )
}

fn block_relation_estimate_size_math(
    mut curpages: BlockNumber,
    relpages: BlockNumber,
    reltuples: f64,
    relallvisible: BlockNumber,
    relhassubclass: bool,
    density_fallback: impl FnOnce(Option<&mut [i32]>) -> PgResult<f64>,
    attr_widths: Option<&mut [i32]>,
    pages: &mut BlockNumber,
    tuples: &mut f64,
    allvisfrac: &mut f64,
) -> PgResult<()> {
    // Never-vacuumed (reltuples < 0) tables get a 10-page floor (cached-plan
    // guard); skipped when there are inheritance children.
    if curpages < 10 && reltuples < 0.0 && !relhassubclass {
        curpages = 10;
    }

    *pages = curpages;
    if curpages == 0 {
        *tuples = 0.0;
        *allvisfrac = 0.0;
        return Ok(());
    }

    let density: f64 = if reltuples >= 0.0 && relpages > 0 {
        reltuples / relpages as f64
    } else {
        density_fallback(attr_widths)?
    };
    // C rint(): round half to even.
    *tuples = (density * curpages as f64).round_ties_even();

    // relallvisible is used as-is (pages added since VACUUM are likely not
    // all-visible), converted to the fraction costsize.c wants.
    if relallvisible == 0 || curpages == 0 {
        *allvisfrac = 0.0;
    } else if relallvisible as f64 >= curpages as f64 {
        *allvisfrac = 1.0;
    } else {
        *allvisfrac = relallvisible as f64 / curpages as f64;
    }

    Ok(())
}

// --- Unported providers, loud per rule 5 ---

fn relation_locator(rel: &Relation<'_>) -> RelFileLocator {
    bufmgr_seams::relation_smgr_locator::call(rel).locator
}

fn relation_nblocks(rel: &Relation<'_>) -> PgResult<BlockNumber> {
    bufmgr_seams::relation_get_number_of_blocks_in_fork::call(rel, ForkNumber::MAIN_FORKNUM)
}

fn nbuffers() -> PgResult<i32> {
    Ok(bufmgr_seams::active_buffers_or(init_small::globals::NBuffers()))
}
