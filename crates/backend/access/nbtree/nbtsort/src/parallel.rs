//! nbtsort.c parallel build: _bt_begin_parallel, _bt_end_parallel,
//! _bt_parallel_heapscan, _bt_leader_participate_as_worker,
//! _bt_parallel_build_main and _bt_parallel_scan_and_sort. Workers are
//! threads (docs/parallel-query-design.md): the shm_toc entries become the
//! typed fields of one Arc'd BTShared in the parallel context's private slot,
//! and the leader waits on its latch instead of a condition variable.

use std::sync::Arc;

use pgsync::Mutex;

use ::mcx::{Mcx, MemoryContext};
use ::types_core::instrument::{BufferUsage, WalUsage};
use ::types_core::{Oid, ProcNumber};
use ::types_error::PgResult;
use ::types_rel::lock::{
    AccessExclusiveLock, RowExclusiveLock, ShareLock, ShareUpdateExclusiveLock, LOCKMODE,
};
use ::types_rel::Relation;
use ::types_storage::waiteventset::{WL_EXIT_ON_PM_DEATH, WL_LATCH_SET, WL_TIMEOUT};
use backend_progress::pgstat_progress_update_param;
use backend_progress::progress::PROGRESS_CREATEIDX_SUBPHASE;
use execindexing::IndexInfo;
use tableam::ParallelTableScanDescShared;
use tuplesort::{SortCoordinate, Sharedsort, Tuplesort, TUPLESORT_NONE};

use crate::{PROGRESS_BTREE_PHASE_PERFORMSORT_1, PROGRESS_BTREE_PHASE_PERFORMSORT_2};

const ENTRYPOINT: &str = "_bt_parallel_build_main";

// PG_WAIT_IPC + the ParallelCreateIndexScan row of wait_event_names.txt.
const WAIT_EVENT_PARALLEL_CREATE_INDEX_SCAN: u32 = 0x0800_0000 + 39;

// Leader recheck cadence while waiting for participants: a worker's done
// signal sets the leader's latch, so this only bounds a lost wakeup.
const LEADER_RECHECK_MS: i64 = 1000;

/// `BTShared`: the fields every participant reads, plus the mutable scan
/// results workers report back.
pub(crate) struct BTShared {
    heaprelid: Oid,
    indexrelid: Oid,
    isunique: bool,
    nulls_not_distinct: bool,
    isconcurrent: bool,
    scantuplesortstates: i32,
    queryid: i64,
    query_text: Option<String>,
    // Who to wake when a participant finishes (C's workersdonecv sleeper).
    leader_proc: ProcNumber,
    mutable: Mutex<BTSharedMutable>,
    pscan: ParallelTableScanDescShared,
    pub(crate) sharedsort: Arc<Sharedsort>,
    pub(crate) sharedsort2: Option<Arc<Sharedsort>>,
    // PARALLEL_KEY_BUFFER_USAGE / PARALLEL_KEY_WAL_USAGE, one per worker.
    usage: Mutex<Vec<(BufferUsage, WalUsage)>>,
}

#[derive(Default)]
struct BTSharedMutable {
    nparticipantsdone: i32,
    reltuples: f64,
    havedead: bool,
    indtuples: f64,
    brokenhotchain: bool,
}

/// `BTLeader`: the leader's handle on a parallel build in progress.
pub(crate) struct BTLeader {
    pcxt: parallel::ParallelContextId,
    pub(crate) nparticipanttuplesorts: i32,
    pub(crate) btshared: Arc<BTShared>,
    snapshot: Option<snapmgr::Snapshot>,
}

/// What the participants' scans added up to (`_bt_parallel_heapscan`).
pub(crate) struct ParallelScanResult {
    pub(crate) reltuples: f64,
    pub(crate) havedead: bool,
    pub(crate) indtuples: f64,
    pub(crate) brokenhotchain: bool,
}

fn ensure_entrypoint_registered() {
    parallel::register_parallel_worker_entrypoint(ENTRYPOINT, bt_parallel_build_main);
}

/// `_bt_begin_parallel`: launch up to `request` workers and join the heap
/// scan as a participant. None when no worker could be launched; the caller
/// then builds serially.
pub(crate) fn bt_begin_parallel<'mcx>(
    mcx: Mcx<'mcx>,
    heap: &Relation<'mcx>,
    index: &Relation<'mcx>,
    index_info: &IndexInfo<'mcx>,
    isconcurrent: bool,
    request: i32,
) -> PgResult<Option<BTLeader>> {
    debug_assert!(request > 0);
    ensure_entrypoint_registered();

    xact::EnterParallelMode();
    let pcxt = match parallel::CreateParallelContext("postgres", ENTRYPOINT, request) {
        Ok(p) => p,
        Err(e) => {
            xact::ExitParallelMode();
            return Err(e);
        }
    };
    let scantuplesortstates = request + 1;

    // A normal build scans with SnapshotAny and does its own visibility
    // checks; a concurrent build indexes what one MVCC snapshot sees.
    let snapshot = if !isconcurrent {
        None
    } else {
        let snap = snapmgr::GetTransactionSnapshot()?;
        snapmgr::RegisterSnapshot(Some(&snap))?
    };
    let unregister = |snapshot: &Option<snapmgr::Snapshot>| {
        if snapshot.is_some() {
            snapmgr::UnregisterSnapshot(snapshot.as_ref());
        }
    };

    if let Err(e) = parallel::InitializeParallelDSM(pcxt) {
        unregister(&snapshot);
        let _ = parallel::DestroyParallelContext(pcxt);
        xact::ExitParallelMode();
        return Err(e);
    }

    let setup = || -> PgResult<Arc<BTShared>> {
        let mut pscan = ParallelTableScanDescShared::default();
        tableam::table_parallelscan_initialize(heap, &mut pscan, &snapshot)?;
        let sharedsort = tuplesort::tuplesort_initialize_shared(scantuplesortstates)?;
        let sharedsort2 = if index_info.ii_Unique {
            Some(tuplesort::tuplesort_initialize_shared(scantuplesortstates)?)
        } else {
            None
        };
        let nworkers = parallel::nworkers(pcxt).max(0) as usize;
        Ok(Arc::new(BTShared {
            heaprelid: heap.rd_id,
            indexrelid: index.rd_id,
            isunique: index_info.ii_Unique,
            nulls_not_distinct: index_info.ii_NullsNotDistinct,
            isconcurrent,
            scantuplesortstates,
            queryid: backend_status::pgstat_get_my_query_id(),
            query_text: elog::with_debug_query_string(|q| q.map(str::to_owned)),
            leader_proc: init_small::globals::MyProcNumber(),
            mutable: Mutex::new(BTSharedMutable::default()),
            pscan,
            sharedsort,
            sharedsort2,
            usage: Mutex::new(::mcx::std_vec_from_elem(
                "access",
                (BufferUsage::default(), WalUsage::default()),
                nworkers,
            )?),
        }))
    };
    let btshared = match setup() {
        Ok(s) => s,
        Err(e) => {
            unregister(&snapshot);
            let _ = parallel::DestroyParallelContext(pcxt);
            xact::ExitParallelMode();
            return Err(e);
        }
    };
    parallel::set_private(pcxt, Arc::clone(&btshared) as Arc<dyn std::any::Any + Send + Sync>);

    parallel::LaunchParallelWorkers(pcxt)?;
    let btleader = BTLeader {
        pcxt,
        nparticipanttuplesorts: parallel::nworkers_launched(pcxt) + 1,
        btshared,
        snapshot,
    };

    // No worker launched: back out and build serially.
    if parallel::nworkers_launched(pcxt) == 0 {
        bt_end_parallel(btleader)?;
        return Ok(None);
    }

    bt_leader_participate_as_worker(mcx, heap, index, &btleader)?;

    // Make sure a worker that failed to start cannot hang the leader.
    parallel::WaitForParallelWorkersToAttach(pcxt)?;
    Ok(Some(btleader))
}

/// `_bt_end_parallel`: wait for the workers, fold in their buffer/WAL usage,
/// destroy the parallel context and leave parallel mode.
pub(crate) fn bt_end_parallel(btleader: BTLeader) -> PgResult<()> {
    parallel::WaitForParallelWorkersToFinish(btleader.pcxt)?;
    {
        let launched = parallel::nworkers_launched(btleader.pcxt).max(0) as usize;
        let usage = btleader.btshared.usage.lock().unwrap_or_else(|e| e.into_inner());
        for (buf, wal) in usage.iter().take(launched) {
            instrument::instr_accum_parallel_query(buf, wal);
        }
    }
    if btleader.snapshot.is_some() {
        snapmgr::UnregisterSnapshot(btleader.snapshot.as_ref());
    }
    parallel::DestroyParallelContext(btleader.pcxt)?;
    xact::ExitParallelMode();
    Ok(())
}

/// `_bt_parallel_heapscan`: wait until every participant has finished its
/// scan and sort, then collect their totals.
pub(crate) fn bt_parallel_heapscan(btleader: &BTLeader) -> PgResult<ParallelScanResult> {
    let btshared = &btleader.btshared;
    let latch = init_small::globals::MyLatch().expect("parallel index build leader without MyLatch");
    loop {
        {
            let m = btshared.mutable.lock().unwrap_or_else(|e| e.into_inner());
            if m.nparticipantsdone == btleader.nparticipanttuplesorts {
                return Ok(ParallelScanResult {
                    reltuples: m.reltuples,
                    havedead: m.havedead,
                    indtuples: m.indtuples,
                    brokenhotchain: m.brokenhotchain,
                });
            }
        }
        latch::WaitLatch(
            Some(latch),
            WL_LATCH_SET | WL_TIMEOUT | WL_EXIT_ON_PM_DEATH,
            LEADER_RECHECK_MS,
            WAIT_EVENT_PARALLEL_CREATE_INDEX_SCAN,
        )?;
        latch::ResetLatch(latch);
        postgres_seams::check_for_interrupts::call()?;
    }
}

/// `_bt_leader_participate_as_worker`.
fn bt_leader_participate_as_worker<'mcx>(
    mcx: Mcx<'mcx>,
    heap: &Relation<'mcx>,
    index: &Relation<'mcx>,
    btleader: &BTLeader,
) -> PgResult<()> {
    // With fewer workers than requested this share is somewhat larger than
    // the workers'.
    let sortmem = init_small::globals::maintenance_work_mem() / btleader.nparticipanttuplesorts;
    bt_parallel_scan_and_sort(mcx, heap, index, &btleader.btshared, sortmem, true)
}

/// `_bt_parallel_build_main`: a launched worker's whole job.
fn bt_parallel_build_main(pshared: &parallel::ParallelShared) -> PgResult<()> {
    let btshared = pshared
        .private()
        .expect("_bt_parallel_build_main without shared build state")
        .downcast::<BTShared>()
        .unwrap_or_else(|_| panic!("_bt_parallel_build_main private state is not BTShared"));

    let _debug_query = btshared.query_text.as_deref().map(elog::debug_query_string_scope);
    backend_status::pgstat_report_activity(
        backend_status::BackendState::STATE_RUNNING,
        btshared.query_text.as_deref(),
    );

    // The lock modes index.c already holds in the leader.
    let (heap_lockmode, index_lockmode): (LOCKMODE, LOCKMODE) = if !btshared.isconcurrent {
        (ShareLock, AccessExclusiveLock)
    } else {
        (ShareUpdateExclusiveLock, RowExclusiveLock)
    };

    backend_status::pgstat_report_query_id(btshared.queryid, false);

    let ctx = MemoryContext::new("parallel btree build worker");
    let mcx = ctx.mcx();
    let heap = table::table_open(mcx, btshared.heaprelid, heap_lockmode)?;
    let index = indexam::index_open(mcx, btshared.indexrelid, index_lockmode)?;

    let save = instrument::instr_start_parallel_query();
    let sortmem = init_small::globals::maintenance_work_mem() / btshared.scantuplesortstates;
    bt_parallel_scan_and_sort(mcx, &heap, &index, &btshared, sortmem, false)?;
    {
        let mut usage = btshared.usage.lock().unwrap_or_else(|e| e.into_inner());
        let me = parallel::ParallelWorkerNumber().max(0) as usize;
        if let Some(slot) = usage.get_mut(me) {
            *slot = instrument::instr_end_parallel_query(&save);
        }
    }

    indexam::index_close(index, index_lockmode)?;
    table::table_close(heap, heap_lockmode)
}

/// `_bt_parallel_scan_and_sort`: scan this participant's share of the heap
/// into its own partial tuplesort(s), sort them, and report the totals.
fn bt_parallel_scan_and_sort<'mcx>(
    mcx: Mcx<'mcx>,
    heap: &Relation<'mcx>,
    index: &Relation<'mcx>,
    btshared: &BTShared,
    sortmem: i32,
    progress: bool,
) -> PgResult<()> {
    let coordinate = SortCoordinate {
        is_worker: true,
        n_participants: -1,
        sharedsort: Arc::clone(&btshared.sharedsort),
    };
    let mut sortstate = Tuplesort::begin_index_btree_coordinated(
        heap,
        index,
        btshared.isunique,
        btshared.nulls_not_distinct,
        sortmem,
        Some(&coordinate),
        TUPLESORT_NONE,
    )?;
    // The dead-tuple spool gets work_mem, unless this participant's share is
    // smaller.
    let mut spool2 = match &btshared.sharedsort2 {
        Some(sharedsort2) => {
            let coordinate2 = SortCoordinate {
                is_worker: true,
                n_participants: -1,
                sharedsort: Arc::clone(sharedsort2),
            };
            Some(Tuplesort::begin_index_btree_coordinated(
                heap,
                index,
                false,
                false,
                sortmem.min(init_small::globals::work_mem()),
                Some(&coordinate2),
                TUPLESORT_NONE,
            )?)
        }
        None => None,
    };

    let mut index_info = execindexing::BuildIndexInfo(mcx, index)?;
    index_info.ii_Concurrent = btshared.isconcurrent;
    let mut havedead = false;
    let mut indtuples = 0.0f64;
    let reltuples = execindexing::table_index_build_scan_parallel(
        mcx,
        heap,
        index,
        &mut index_info,
        progress,
        &btshared.pscan,
        |_index_rel, tid, values, isnull, tuple_is_alive| {
            if tuple_is_alive || spool2.is_none() {
                sortstate.putindextuplevalues(*tid, values, isnull)?;
            } else {
                havedead = true;
                spool2.as_mut().expect("spool2").putindextuplevalues(*tid, values, isnull)?;
            }
            indtuples += 1.0;
            Ok(())
        },
    )?;

    if progress {
        pgstat_progress_update_param(PROGRESS_CREATEIDX_SUBPHASE, PROGRESS_BTREE_PHASE_PERFORMSORT_1);
    }
    sortstate.performsort()?;
    if let Some(s2) = spool2.as_mut() {
        if progress {
            pgstat_progress_update_param(
                PROGRESS_CREATEIDX_SUBPHASE,
                PROGRESS_BTREE_PHASE_PERFORMSORT_2,
            );
        }
        s2.performsort()?;
    }

    {
        let mut m = btshared.mutable.lock().unwrap_or_else(|e| e.into_inner());
        m.nparticipantsdone += 1;
        m.reltuples += reltuples;
        if havedead {
            m.havedead = true;
        }
        m.indtuples += indtuples;
        if index_info.ii_BrokenHotChain {
            m.brokenhotchain = true;
        }
    }
    latch::set_latch(&lmgr_proc::GetPGProcByNumber(btshared.leader_proc).procLatch);

    drop(sortstate);
    drop(spool2);
    Ok(())
}

