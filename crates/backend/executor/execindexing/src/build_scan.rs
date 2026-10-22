// heapam_index_build_range_scan (heapam_handler.c); boundary hoisted above
// heapam_handler (execindexing already sits above the AM stack — a
// heapam_handler home would cycle). A parallel index build passes the shared
// parallel heap scan every participant joins.
use ::datum::Datum;
use ::mcx::{Mcx, PgVec};
use ::types_core::{BlockNumber, INDEX_MAX_KEYS, InvalidBlockNumber};
use ::types_error::PgResult;
use ::types_rel::Relation;
use ::types_slot::SlotData;
use ::types_snapshot::HTSV_Result::*;
use ::types_storage::bufpage::MaxHeapTuplesPerPage;
use ::types_tuple::itemptr::{InvalidOffsetNumber, ItemPointerData};
use ::types_tuple::HeapTupleData;
use tableam_vocab::{SO_ALLOW_PAGEMODE, SO_ALLOW_STRAT, SO_ALLOW_SYNC, SO_TYPE_SEQSCAN};

use backend_progress_seams::{PROGRESS_SCAN_BLOCKS_DONE, PROGRESS_SCAN_BLOCKS_TOTAL};

use crate::{index_predicate_passes, FormIndexDatum, IndexInfo};

/// `progress`: C's table_index_build_scan(..., progress, ...) — report
/// PROGRESS_SCAN_BLOCKS_TOTAL/DONE through pgstat_progress_update_param.
pub fn table_index_build_scan<'mcx, F>(
    mcx: Mcx<'mcx>,
    heap_relation: &Relation<'mcx>,
    index_relation: &Relation<'mcx>,
    index_info: &mut IndexInfo<'mcx>,
    allow_sync: bool,
    progress: bool,
    callback: F,
) -> PgResult<f64>
where
    F: FnMut(&Relation<'mcx>, &ItemPointerData, &[Datum], &[bool], bool) -> PgResult<()>,
{
    table_index_build_range_scan(
        mcx,
        heap_relation,
        index_relation,
        index_info,
        allow_sync,
        false,
        progress,
        0,
        InvalidBlockNumber,
        callback,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn table_index_build_range_scan<'mcx, F>(
    mcx: Mcx<'mcx>,
    heap_relation: &Relation<'mcx>,
    index_relation: &Relation<'mcx>,
    index_info: &mut IndexInfo<'mcx>,
    allow_sync: bool,
    anyvisible: bool,
    progress: bool,
    start_blockno: BlockNumber,
    numblocks: BlockNumber,
    callback: F,
) -> PgResult<f64>
where
    F: FnMut(&Relation<'mcx>, &ItemPointerData, &[Datum], &[bool], bool) -> PgResult<()>,
{
    table_index_build_range_scan_with_xmin(
        mcx,
        heap_relation,
        index_relation,
        index_info,
        allow_sync,
        anyvisible,
        progress,
        start_blockno,
        numblocks,
        None,
        callback,
    )
}

// heapam_scan_get_blocks_done (heapam_handler.c): blocks completed so far,
// wrapping when the scan did not start at block 0.
fn heapam_scan_get_blocks_done(
    scan: &heapam::HeapScanDescData<'_>,
    parallel: Option<&tableam::ParallelTableScanDescShared>,
) -> BlockNumber {
    let startblock = match parallel {
        Some(p) => p.pscan.phs_startblock.load(core::sync::atomic::Ordering::Relaxed),
        None => scan.rs_startblock,
    };
    if scan.rs_cblock > startblock {
        scan.rs_cblock - startblock
    } else {
        let nblocks = match parallel {
            Some(p) => p.pscan.phs_nblocks,
            None => scan.rs_nblocks,
        };
        nblocks - startblock + scan.rs_cblock
    }
}

/// `table_index_build_scan` with a parallel heap scan (the `scan != NULL`
/// arm): this participant claims blocks from `parallel_scan`, whose snapshot
/// (SnapshotAny, or the concurrent build's MVCC snapshot) drives the scan.
pub fn table_index_build_scan_parallel<'mcx, F>(
    mcx: Mcx<'mcx>,
    heap_relation: &Relation<'mcx>,
    index_relation: &Relation<'mcx>,
    index_info: &mut IndexInfo<'mcx>,
    progress: bool,
    parallel_scan: &tableam::ParallelTableScanDescShared,
    callback: F,
) -> PgResult<f64>
where
    F: FnMut(&Relation<'mcx>, &ItemPointerData, &[Datum], &[bool], bool) -> PgResult<()>,
{
    build_range_scan_core_inner(
        mcx,
        heap_relation,
        index_relation,
        index_info,
        true,
        false,
        progress,
        0,
        InvalidBlockNumber,
        None,
        None,
        Some(parallel_scan),
        callback,
    )
}

/// [`table_index_build_range_scan`] with a HOISTED `OldestXmin` (M4.2
/// parallel build): the SnapshotAny lane's horizon is per-tuple
/// conservative, so any single value valid at scan start works for every
/// range — the pool driver computes it ONCE on the leader and shares it, so
/// (i) per-claim `GetOldestNonRemovableTransactionId` procarray scans are
/// not paid at morsel cadence and (ii) every worker routes DEAD vs
/// RECENTLY_DEAD identically to a serial scan started at the same instant
/// (the parity leg's determinism). `None` keeps today's per-call compute.
#[allow(clippy::too_many_arguments)]
pub fn table_index_build_range_scan_with_xmin<'mcx, F>(
    mcx: Mcx<'mcx>,
    heap_relation: &Relation<'mcx>,
    index_relation: &Relation<'mcx>,
    index_info: &mut IndexInfo<'mcx>,
    allow_sync: bool,
    anyvisible: bool,
    progress: bool,
    start_blockno: BlockNumber,
    numblocks: BlockNumber,
    hoisted_oldest_xmin: Option<types_core::TransactionId>,
    callback: F,
) -> PgResult<f64>
where
    F: FnMut(&Relation<'mcx>, &ItemPointerData, &[Datum], &[bool], bool) -> PgResult<()>,
{
    build_range_scan_core(
        mcx,
        heap_relation,
        index_relation,
        index_info,
        allow_sync,
        anyvisible,
        progress,
        start_blockno,
        numblocks,
        hoisted_oldest_xmin,
        None,
        callback,
    )
}

/// verify_nbtree.c:557: the caller's registered snapshot drives the scan.
pub fn table_index_build_scan_with_snapshot<'mcx, F>(
    mcx: Mcx<'mcx>,
    heap_relation: &Relation<'mcx>,
    index_relation: &Relation<'mcx>,
    index_info: &mut IndexInfo<'mcx>,
    allow_sync: bool,
    progress: bool,
    snapshot: &snapmgr::Snapshot,
    callback: F,
) -> PgResult<f64>
where
    F: FnMut(&Relation<'mcx>, &ItemPointerData, &[Datum], &[bool], bool) -> PgResult<()>,
{
    build_range_scan_core(
        mcx,
        heap_relation,
        index_relation,
        index_info,
        allow_sync,
        false,
        progress,
        0,
        InvalidBlockNumber,
        None,
        Some(snapshot),
        callback,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_range_scan_core<'mcx, F>(
    mcx: Mcx<'mcx>,
    heap_relation: &Relation<'mcx>,
    index_relation: &Relation<'mcx>,
    index_info: &mut IndexInfo<'mcx>,
    allow_sync: bool,
    anyvisible: bool,
    progress: bool,
    start_blockno: BlockNumber,
    numblocks: BlockNumber,
    hoisted_oldest_xmin: Option<types_core::TransactionId>,
    snapshot: Option<&snapmgr::Snapshot>,
    callback: F,
) -> PgResult<f64>
where
    F: FnMut(&Relation<'mcx>, &ItemPointerData, &[Datum], &[bool], bool) -> PgResult<()>,
{
    build_range_scan_core_inner(
        mcx,
        heap_relation,
        index_relation,
        index_info,
        allow_sync,
        anyvisible,
        progress,
        start_blockno,
        numblocks,
        hoisted_oldest_xmin,
        snapshot,
        None,
        callback,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_range_scan_core_inner<'mcx, F>(
    mcx: Mcx<'mcx>,
    heap_relation: &Relation<'mcx>,
    index_relation: &Relation<'mcx>,
    index_info: &mut IndexInfo<'mcx>,
    allow_sync: bool,
    anyvisible: bool,
    progress: bool,
    start_blockno: BlockNumber,
    numblocks: BlockNumber,
    hoisted_oldest_xmin: Option<types_core::TransactionId>,
    snapshot: Option<&snapmgr::Snapshot>,
    parallel: Option<&tableam::ParallelTableScanDescShared>,
    mut callback: F,
) -> PgResult<f64>
where
    F: FnMut(&Relation<'mcx>, &ItemPointerData, &[Datum], &[bool], bool) -> PgResult<()>,
{
    // IX1 L1: a pgrcolumnar table's rows live outside its (empty) main fork; its own scan feeds the callback.
    if tableam_vocab::TableAm::of(heap_relation) == Some(tableam_vocab::TableAm::Pgrcolumnar) {
        return pgrc_build_range_scan(mcx, heap_relation, index_relation, index_info, start_blockno, numblocks, callback);
    }
    let concurrent = index_info.ii_Concurrent;
    let is_system_catalog = catalog::IsSystemRelation(heap_relation);
    let checking_uniqueness = index_info.ii_Unique || index_info.ii_HasExclusion;
    // C: "any visible" is incompatible with uniqueness checks and only valid
    // under SnapshotAny (non-concurrent).
    debug_assert!(!(anyvisible && checking_uniqueness));
    debug_assert!(!(anyvisible && concurrent));

    let mut slot = exectuples::make_tuple_table_slot(
        mcx,
        types_slot::TupleSlotKind::BufferHeapTuple,
        Some(heap_relation.rd_att.clone()),
    );

    // Concurrent builds scan under a registered MVCC snapshot; OldestXmin is
    // only for the SnapshotAny lane's HTSV routing.
    let (registered, owned) = if let Some(p) = parallel {
        // The parallel scan's own snapshot: SnapshotAny, or the leader's
        // registered MVCC snapshot restored for this participant.
        match &p.snapshot {
            Some(serialized) if !p.pscan.phs_snapshot_any => {
                let snap = snapmgr::RestoreSnapshot(serialized);
                (Some(snapmgr::RegisterSnapshot(Some(&snap))?.expect("registered snapshot")), true)
            }
            _ => (None, false),
        }
    } else if let Some(snap) = snapshot {
        (Some(snap.clone()), false)
    } else if concurrent {
        let snap = snapmgr::GetTransactionSnapshot()?;
        (Some(snapmgr::RegisterSnapshot(Some(&snap))?.expect("registered snapshot")), true)
    } else {
        (None, false)
    };
    let oldest_xmin = if concurrent {
        types_core::InvalidTransactionId
    } else if let Some(x) = hoisted_oldest_xmin {
        debug_assert!(x != 0);
        x
    } else {
        let x = procarray::GetOldestNonRemovableTransactionId(heap_relation)?;
        debug_assert!(x != 0);
        x
    };

    // C dispatches index_build_range_scan through rd_tableam; this scan is
    // heapam's. A columnar table's rows live outside its (empty) main fork, so
    // scanning it here would record a valid, empty index (UNIQUE unenforced).
    if tableam_vocab::TableAm::of(heap_relation) != Some(tableam_vocab::TableAm::Heap) {
        return Err(Box::new(
            types_error::PgError::error(format!(
                "cannot build an index on table \"{}\": its table access method is not supported by the index build scan",
                heap_relation.name()
            ))
            .with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED),
        ));
    }
    let mut flags = SO_TYPE_SEQSCAN | SO_ALLOW_STRAT | SO_ALLOW_PAGEMODE;
    if allow_sync {
        flags |= SO_ALLOW_SYNC;
    }
    debug_assert!(parallel.is_none() || allow_sync);
    let mut scan = heapam::heap_beginscan(
        mcx,
        heap_relation,
        registered.clone(), // None is SnapshotAny
        0,
        PgVec::new_in(mcx),
        parallel.map(|p| core::ptr::NonNull::from(&p.pscan)),
        flags,
    )?;
    let nblocks = match parallel {
        Some(p) => p.pscan.phs_nblocks,
        None => scan.rs_nblocks,
    };

    // heapam_handler.c:1298-1314: publish the number of blocks to scan
    // (the whole relation, even for a range scan).
    if progress {
        backend_progress_seams::pgstat_progress_update_param::call(
            PROGRESS_SCAN_BLOCKS_TOTAL,
            i64::from(nblocks),
        );
    }

    if !allow_sync {
        heapam::heap_setscanlimits(&mut scan, start_blockno, numblocks);
    } else {
        // C: syncscan can only be requested on the whole relation.
        debug_assert!(start_blockno == 0 && numblocks == InvalidBlockNumber);
    }

    // C heapam_handler.c ExecPrepareQual runs before the scan: predicate fold
    // errors surface even when no tuple is ever tested.
    crate::prepare_index_predicate(mcx, index_info)?;

    let mut reltuples = 0.0f64;
    let mut previous_blkno = InvalidBlockNumber;
    let mut root_blkno = InvalidBlockNumber;
    let mut root_offsets = [InvalidOffsetNumber; MaxHeapTuplesPerPage];
    let mut values = [Datum::null(); INDEX_MAX_KEYS as usize];
    let mut isnull = [false; INDEX_MAX_KEYS as usize];
    // C's per-tuple econtext reset (heapam_handler.c:1611): expression and
    // predicate results land here, consumed by `callback` within the
    // iteration, freed before the next tuple.
    let mut per_tuple = mcx::MemoryContext::new_wholesale("IndexBuildPerTuple");

    loop {
        per_tuple.reset();
        let Some((mut tuple, buffer)) = next_tuple(&mut scan)? else {
            break;
        };

        // heapam_handler.c:1338-1348: report scan progress, if asked to.
        if progress {
            let blocks_done = heapam_scan_get_blocks_done(&scan, parallel);
            if blocks_done != previous_blkno {
                backend_progress_seams::pgstat_progress_update_param::call(
                    PROGRESS_SCAN_BLOCKS_DONE,
                    i64::from(blocks_done),
                );
                previous_blkno = blocks_done;
            }
        }

        if scan.rs_cblock != root_blkno {
            let pin = scan.rs_cbuf.as_ref().expect("pinned page for returned tuple");
            let guard = pin.lock_share()?;
            pruneheap::heap_get_root_tuples(pin.page(), &mut root_offsets)?;
            drop(guard);
            root_blkno = scan.rs_cblock;
        }

        let tuple_is_alive;
        let index_it;
        if concurrent {
            // MVCC-snapshot lane: every returned tuple is visible and indexed.
            index_it = true;
            tuple_is_alive = true;
            reltuples += 1.0;
        } else {
            // C's `recheck:` loop: an XactLockTableWait drops the buffer lock,
            // waits out the in-progress xact, and re-runs SatisfiesVacuum.
            (index_it, tuple_is_alive) = loop {
                let pin = scan.rs_cbuf.as_ref().expect("pinned page for returned tuple");
                let guard = pin.lock_share()?;
                let htsv =
                    heapam_visibility::HeapTupleSatisfiesVacuum(&mut tuple, oldest_xmin, buffer)?;
                match htsv {
                    HEAPTUPLE_DEAD => break (false, false),
                    HEAPTUPLE_LIVE => {
                        reltuples += 1.0;
                        break (true, true);
                    }
                    HEAPTUPLE_RECENTLY_DEAD => {
                        if tuple.t_data().is_hot_updated() {
                            index_info.ii_BrokenHotChain = true;
                            break (false, false);
                        }
                        break (true, false);
                    }
                    HEAPTUPLE_INSERT_IN_PROGRESS if anyvisible => {
                        reltuples += 1.0;
                        break (true, true);
                    }
                    HEAPTUPLE_INSERT_IN_PROGRESS => {
                        let xwait = tuple.t_data().xmin();
                        if !xact::TransactionIdIsCurrentTransactionId(xwait) {
                            if !is_system_catalog {
                                elog::ereport_msg(
                                    types_error::WARNING,
                                    format!(
                                        "concurrent insert in progress within table \"{}\"",
                                        heap_relation.name()
                                    ),
                                    None,
                                )?;
                            }
                            // Indexing an uncommitted insert could raise a
                            // bogus uniqueness failure: wait it out, recheck.
                            if checking_uniqueness {
                                drop(guard);
                                lmgr::XactLockTableWait(
                                    xwait,
                                    Some(heap_relation),
                                    Some(&tuple.t_self),
                                    ::types_storage::lock::XLTW_Oper::InsertIndexUnique,
                                )?;
                                postgres_seams::check_for_interrupts::call()?;
                                continue;
                            }
                        } else {
                            reltuples += 1.0;
                        }
                        break (true, true);
                    }
                    HEAPTUPLE_DELETE_IN_PROGRESS if anyvisible => {
                        reltuples += 1.0;
                        break (true, false);
                    }
                    HEAPTUPLE_DELETE_IN_PROGRESS => {
                        let xwait = heapam::HeapTupleHeaderGetUpdateXid(tuple.t_data())?;
                        if !xact::TransactionIdIsCurrentTransactionId(xwait) {
                            if !is_system_catalog {
                                elog::ereport_msg(
                                    types_error::WARNING,
                                    format!(
                                        "concurrent delete in progress within table \"{}\"",
                                        heap_relation.name()
                                    ),
                                    None,
                                )?;
                            }
                            // Uniqueness: assuming dead could miss a
                            // violation. HOT: only the deleter's fate says
                            // whether this or the chain tip gets indexed.
                            // Either way, wait it out and recheck.
                            if checking_uniqueness || tuple.t_data().is_hot_updated() {
                                drop(guard);
                                lmgr::XactLockTableWait(
                                    xwait,
                                    Some(heap_relation),
                                    Some(&tuple.t_self),
                                    ::types_storage::lock::XLTW_Oper::InsertIndexUnique,
                                )?;
                                postgres_seams::check_for_interrupts::call()?;
                                continue;
                            }
                            // Index it but exclude from uniqueness, same as
                            // RECENTLY_DEAD; count as live to match ANALYZE.
                            reltuples += 1.0;
                            break (true, false);
                        } else if tuple.t_data().is_hot_updated() {
                            index_info.ii_BrokenHotChain = true;
                            break (false, false);
                        } else {
                            break (true, false);
                        }
                    }
                }
            };
        }

        if !index_it {
            continue;
        }

        exectuples::exec_store_buffer_heap_tuple(&mut slot, mcx, tuple, buffer);

        if !index_info.ii_Predicate.is_nil()
            && !index_predicate_passes(mcx, per_tuple.mcx(), index_info, &mut slot)?
        {
            continue;
        }

        FormIndexDatum(
            mcx,
            per_tuple.mcx(),
            index_info,
            &mut slot,
            &mut values[..],
            &mut isnull[..],
        )?;

        let self_tid = slot_tid(&slot);
        if slot_is_heap_only(&slot) {
            let offnum = self_tid.ip_posid;
            let mut root = root_offsets[offnum as usize - 1];
            if root == InvalidOffsetNumber {
                let pin = scan.rs_cbuf.as_ref().expect("pinned page for returned tuple");
                let guard = pin.lock_share()?;
                pruneheap::heap_get_root_tuples(pin.page(), &mut root_offsets)?;
                drop(guard);
                root = root_offsets[offnum as usize - 1];
            }
            let blkno = ::types_tuple::itemptr::ItemPointerGetBlockNumberNoCheck(&self_tid);
            if root == InvalidOffsetNumber {
                return Err(Box::new(types_error::PgError::error(format!(
                    "failed to find parent tuple for heap-only tuple at ({blkno},{offnum}) in table \"{}\"",
                    heap_relation.name()
                ))
                .with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED)));
            }
            let tid = ItemPointerData::new(blkno, root);
            callback(index_relation, &tid, &values[..], &isnull[..], tuple_is_alive)?;
        } else {
            callback(index_relation, &self_tid, &values[..], &isnull[..], tuple_is_alive)?;
        }
    }

    // heapam_handler.c:1712-1726: report scan progress one last time.
    if progress {
        backend_progress_seams::pgstat_progress_update_param::call(
            PROGRESS_SCAN_BLOCKS_DONE,
            i64::from(nblocks),
        );
    }

    exectuples::exec_clear_tuple(&mut slot, mcx);
    heapam::heap_endscan(scan)?;
    if owned {
        snapmgr::UnregisterSnapshot(registered.as_ref());
    }
    Ok(reltuples)
}

fn next_tuple<'mcx>(
    scan: &mut heapam::HeapScanDescData<'mcx>,
) -> PgResult<Option<(HeapTupleData<'mcx>, types_core::Buffer)>> {
    use types_scan::ScanDirection;
    if heapam::heap_getnext(scan, ScanDirection::ForwardScanDirection)?.is_none() {
        return Ok(None);
    }
    let t = scan.rs_ctup().expect("just returned Some");
    let buffer = scan.rs_cbuf.as_ref().expect("pinned").buffer();
    // SAFETY: same live pinned image rs_ctup views; hint-bit writes through
    // the &mut view are the C SatisfiesVacuum contract (buffer share-locked).
    let tuple = unsafe {
        HeapTupleData::from_raw_parts(t.header_ptr(), t.t_len, t.t_self, t.t_tableOid)
    };
    Ok(Some((tuple, buffer)))
}

fn slot_tid(slot: &SlotData<'_>) -> ItemPointerData {
    slot.base().tts_tid
}

fn slot_is_heap_only(slot: &SlotData<'_>) -> bool {
    match slot {
        SlotData::BufferHeap(b) => b
            .base
            .tuple
            .as_ref()
            .is_some_and(|t| t.t_data().is_heap_only()),
        _ => unreachable!("build scan slot is BufferHeap"),
    }
}

fn pgrc_refused(message: impl Into<String>) -> Box<types_error::PgError> {
    Box::new(types_error::PgError::error(message).with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED))
}

/// DefineIndex asks before any catalog work, with `concurrent` its effective CONCURRENTLY; the build scan asks again
/// for the rebuild paths (REINDEX, SET ACCESS METHOD, ALTER's rewrites, TRUNCATE), where a concurrent build is never
/// asked about (amcheck's MVCC lane uses it).
pub fn pgrc_index_check(am: types_relscan::IndexAmKind, amname: &str, unique: bool, exclusion: bool, concurrent: bool) -> PgResult<()> {
    use types_relscan::IndexAmKind as K;
    match am {
        K::Btree | K::Hash | K::Gist | K::Spgist | K::Gin => {}
        K::Brin => {
            return Err(Box::new(
                types_error::PgError::error("pgrcolumnar does not support BRIN indexes")
                    .with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED)
                    .with_hint("Zone maps already prune block ranges of pgrcolumnar tables."),
            ))
        }
        #[allow(unreachable_patterns)]
        _ => return Err(pgrc_refused(format!("pgrcolumnar does not support index access method \"{amname}\""))),
    }
    if unique {
        return Err(pgrc_refused("pgrcolumnar does not support unique indexes yet"));
    }
    if exclusion {
        return Err(pgrc_refused("pgrcolumnar does not support exclusion constraints yet"));
    }
    if concurrent {
        return Err(pgrc_refused("pgrcolumnar does not support CREATE INDEX CONCURRENTLY"));
    }
    Ok(())
}

fn pgrc_build_needed_attrs(natts: usize, index_info: &IndexInfo<'_>) -> PgResult<Option<Box<[bool]>>> {
    struct Cx {
        needed: Vec<bool>,
        all: bool,
    }
    impl<'mcx> nodes_core::NodeWalker<'mcx> for Cx {
        fn visit(&mut self, n: ::types_nodes::node_tree::Node<'mcx>) -> PgResult<bool> {
            if let Some(v) = n.as_var() {
                if v.varlevelsup == 0 {
                    match usize::try_from(v.varattno) {
                        Ok(a) if a >= 1 && a <= self.needed.len() => self.needed[a - 1] = true,
                        _ => self.all = true,
                    }
                }
                return Ok(self.all);
            }
            nodes_core::expression_tree_walker(n, self)
        }
    }
    let mut cx = Cx { needed: ::mcx::stdvec::try_filled(natts, false, "pgrc_build_needed_attrs")?, all: false };
    for &a in &index_info.ii_IndexAttrNumbers[..index_info.ii_NumIndexAttrs as usize] {
        match usize::try_from(a) {
            Ok(0) => {}
            Ok(a) if a <= natts => cx.needed[a - 1] = true,
            _ => cx.all = true,
        }
    }
    for list in [&index_info.ii_Expressions, &index_info.ii_Predicate] {
        for n in list.iter() {
            if nodes_core::NodeWalker::visit(&mut cx, n)? {
                return Ok(None);
            }
        }
    }
    Ok((!cx.all).then(|| cx.needed.into_boxed_slice()))
}

/// A non-concurrent build indexes every row whose insert did not abort and that no delete removed below the horizon:
/// the parts of the newest generation the table's commit check sees under `SnapshotAny` (committed and own publishes;
/// `EXT_UNDER` parts of aborted creators stay hidden), minus their DV marks, and the live segments' rows under
/// `SnapshotNonVacuumable` (an aborted insert is skipped, the transaction's own in-progress one kept). No tombstone
/// hides a row: a row deleted in flight stays indexed (recently dead), and a fetch decides each hit's visibility.
/// CREATE INDEX's ShareLock keeps writers out; seal, compile, compaction and rotation may run beside the scan (they
/// keep every row's ID, and the scan reads one generation). Every row is passed as alive (no unique index is built).
fn pgrc_build_range_scan<'mcx, F>(
    mcx: Mcx<'mcx>,
    heap_relation: &Relation<'mcx>,
    index_relation: &Relation<'mcx>,
    index_info: &mut IndexInfo<'mcx>,
    start_blockno: BlockNumber,
    numblocks: BlockNumber,
    mut callback: F,
) -> PgResult<f64>
where
    F: FnMut(&Relation<'mcx>, &ItemPointerData, &[Datum], &[bool], bool) -> PgResult<()>,
{
    let am = types_relscan::IndexAmKind::from_relam(index_relation.rd_rel.relam);
    pgrc_index_check(am, &format!("{am:?}").to_lowercase(), index_info.ii_Unique, index_info.ii_HasExclusion, false)?;
    if start_blockno != 0 || numblocks != InvalidBlockNumber {
        return Err(Box::new(types_error::PgError::error(format!(
            "pgrcolumnar: table \"{}\" has no blocks to build an index over a block range of",
            heap_relation.name()
        ))));
    }
    let registered = if index_info.ii_Concurrent {
        let snap = snapmgr::GetTransactionSnapshot()?;
        Some(snapmgr::RegisterSnapshot(Some(&snap))?.expect("registered snapshot"))
    } else {
        None
    };
    crate::prepare_index_predicate(mcx, index_info)?;
    let needed = pgrc_build_needed_attrs(heap_relation.rd_att.natts as usize, index_info)?;
    let mut scan = tableam::table_beginscan_index_build(mcx, heap_relation, registered.clone())?;
    if let Some(n) = needed.as_deref() {
        tableam::table_scan_set_needed_attrs(&mut scan, n);
    }
    let mut slot = tableam::table_slot_create(mcx, heap_relation)?;
    let mut values = [Datum::null(); INDEX_MAX_KEYS as usize];
    let mut isnull = [false; INDEX_MAX_KEYS as usize];
    let mut per_tuple = mcx::MemoryContext::new_bump("IndexBuildPerTuple");
    let mut reltuples = 0.0f64;
    let res = (|| -> PgResult<()> {
        loop {
            per_tuple.reset();
            postgres_seams::check_for_interrupts::call()?;
            if !tableam::table_scan_getnextslot(mcx, &mut scan, types_scan::ScanDirection::ForwardScanDirection, &mut slot)? {
                return Ok(());
            }
            let tid = slot_tid(&slot);
            if !::types_tuple::itemptr::ItemPointerIsValid(&tid) {
                return Err(Box::new(
                    types_error::PgError::error(format!(
                        "pgrcolumnar: table \"{}\" holds rows without row IDs, which no index can name",
                        heap_relation.name()
                    ))
                    .with_sqlstate(types_error::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE)
                    .with_hint("Rows written before row IDs get them from VACUUM (FULL, RENUMBER)."),
                ));
            }
            reltuples += 1.0;
            if !index_info.ii_Predicate.is_nil() && !index_predicate_passes(mcx, per_tuple.mcx(), index_info, &mut slot)? {
                continue;
            }
            FormIndexDatum(mcx, per_tuple.mcx(), index_info, &mut slot, &mut values[..], &mut isnull[..])?;
            callback(index_relation, &tid, &values[..], &isnull[..], true)?;
        }
    })();
    exectuples::exec_clear_tuple(&mut slot, mcx);
    let end = tableam::table_endscan(scan);
    if let Some(snap) = registered {
        snapmgr::UnregisterSnapshot(Some(&snap));
    }
    res?;
    end?;
    Ok(reltuples)
}
