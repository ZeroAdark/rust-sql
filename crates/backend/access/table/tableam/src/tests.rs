use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use mcx::{Mcx, MemoryContext, PgVec};
use types_core::primitive::InvalidBlockNumber;
use types_core::{Oid, INVALID_PROC_NUMBER, RELPERSISTENCE_PERMANENT};
use types_rel::{
    FormData_pg_class, LockInfoData, LockRelId, Relation, RelationData, RELKIND_FOREIGN_TABLE,
    RELKIND_PARTITIONED_TABLE, RELKIND_RELATION, RELKIND_VIEW, REPLICA_IDENTITY_DEFAULT,
};
use types_slot::TupleSlotKind;
use types_storage::Spinlock;
use types_tuple::{ItemPointerData, NameData, TupleDescData};

use crate::*;

pub(crate) fn make<'mcx>(mcx: Mcx<'mcx>, oid: Oid, relkind: u8, relam: Oid) -> Relation<'mcx> {
    let mut relname = NameData::default();
    relname.namestrcpy("t");
    let data = RelationData { rd_locator: Default::default(), rd_smgr: Default::default(),
        rd_id: oid,
        rd_backend: INVALID_PROC_NUMBER,
        rd_islocaltemp: false,
        rd_isvalid: Cell::new(true),
        rd_createSubid: Cell::new(0),
        rd_newRelfilelocatorSubid: Cell::new(0),
        rd_firstRelfilelocatorSubid: Cell::new(0),
        rd_droppedSubid: Cell::new(0),
        rd_lockInfo: LockInfoData {
            lockRelId: LockRelId { relId: oid, dbId: 5 },
        },
        rd_rel: FormData_pg_class {
            relname,
            relnamespace: 2200,
            reltype: 0,
            relowner: 10,
            relam,
            relfilenode: oid,
            reltablespace: 0,
            relpages: 0,
            reltuples: -1.0,
            relallvisible: 0,
            reltoastrelid: 0,
            relhasindex: false,
            relisshared: false,
            relpersistence: RELPERSISTENCE_PERMANENT,
            relkind,
            relhassubclass: false,
            relrowsecurity: false,
            relispopulated: true,
            relreplident: REPLICA_IDENTITY_DEFAULT,
            relispartition: false,
            relfrozenxid: 3,
            relminmxid: 1,
        },
        rd_att: Rc::new(TupleDescData {
            natts: 0,
            tdtypeid: 0,
            tdtypmod: -1,
            tdrefcount: 1,
            constr: None,
            compact_attrs: PgVec::new_in(mcx),
            attrs: PgVec::new_in(mcx),
        }),
        rd_index: None,
        rd_opcintype: PgVec::new_in(mcx),
        rd_opfamily: PgVec::new_in(mcx),
        rd_indoption: PgVec::new_in(mcx),
        rd_indcollation: PgVec::new_in(mcx),
        rd_options: None,
        pgstat_enabled: Cell::new(false),
        pgstat_link: core::cell::Cell::new((0, core::ptr::null_mut())),
        rd_amcache: Default::default(),
        rd_amcache_hash: Default::default(), rd_amcache_gin: Default::default(), rd_amcache_spgist: Default::default(), rd_amcache_bloom: Default::default(),
        rd_support: PgVec::new_in(mcx),
        rd_supportinfo: Default::default(),
        rd_opcoptions: Default::default(),
        rd_indexlist: Default::default(),
            rd_trigdesc: Default::default(),
            rd_hastriggers: false, rd_hasrules: false,
    };
    Relation::open(data, None)
}

fn pbscan(nblocks: u32) -> ParallelBlockTableScanDescData {
    ParallelBlockTableScanDescData {
        phs_locator: Default::default(),
        phs_syncscan: false,
        phs_snapshot_any: true,
        phs_snapshot_off: 0,
        phs_nblocks: nblocks,
        phs_mutex: Spinlock::new(),
        phs_startblock: AtomicU32::new(InvalidBlockNumber),
        phs_nallocated: AtomicU64::new(0),
    }
}

#[test]
fn invalid_latest_tid_error_location() {
    let ctx = MemoryContext::new_wholesale("invalid_latest_tid");
    let mcx = ctx.mcx();
    let rel = make(mcx, 1, RELKIND_RELATION, HEAP_TABLE_AM_OID);
    let mut parallel = pbscan(0);
    let heap = heapam::heap_beginscan(
        mcx,
        &rel,
        None,
        0,
        PgVec::new_in(mcx),
        Some(core::ptr::NonNull::from(&mut parallel)),
        SO_TYPE_TIDSCAN,
    )
    .unwrap();
    let mut scan = TableScanDesc::Heap(heap);
    let mut tid = ItemPointerData::invalid();
    types_tuple::ItemPointerSet(&mut tid, 0, 1);
    let err = table_tuple_get_latest_tid(mcx, &mut scan, &mut tid).unwrap_err();
    assert_eq!(err.sqlstate, ERRCODE_INVALID_PARAMETER_VALUE);
    assert_eq!(err.message, "tid (0, 1) is not valid for relation \"t\"");
    let location = err.location.unwrap();
    assert_eq!(location.filename.as_deref(), Some("tableam.c"));
    assert_eq!(location.lineno, 259);
    assert_eq!(location.funcname.as_deref(), Some("table_tuple_get_latest_tid"));
}

#[test]
fn constants_match_c_headers() {
    assert_eq!(SO_TYPE_SEQSCAN, 1 << 0);
    assert_eq!(SO_TYPE_BITMAPSCAN, 1 << 1);
    assert_eq!(SO_TYPE_SAMPLESCAN, 1 << 2);
    assert_eq!(SO_TYPE_TIDSCAN, 1 << 3);
    assert_eq!(SO_TYPE_TIDRANGESCAN, 1 << 4);
    assert_eq!(SO_TYPE_ANALYZE, 1 << 5);
    assert_eq!(SO_ALLOW_STRAT, 1 << 6);
    assert_eq!(SO_ALLOW_SYNC, 1 << 7);
    assert_eq!(SO_ALLOW_PAGEMODE, 1 << 8);
    assert_eq!(SO_TEMP_SNAPSHOT, 1 << 9);

    assert_eq!(TABLE_INSERT_SKIP_FSM, 0x0002);
    assert_eq!(TABLE_INSERT_FROZEN, 0x0004);
    assert_eq!(TABLE_INSERT_NO_LOGICAL, 0x0008);
    assert_eq!(TUPLE_LOCK_FLAG_LOCK_UPDATE_IN_PROGRESS, 1 << 0);
    assert_eq!(TUPLE_LOCK_FLAG_FIND_LAST_VERSION, 1 << 1);

    assert_eq!(TM_Result::TM_Ok as u32, 0);
    assert_eq!(TM_Result::TM_WouldBlock as u32, 6);
    assert_eq!(TU_UpdateIndexes::TU_None as u32, 0);
    assert_eq!(TU_UpdateIndexes::TU_Summarizing as u32, 2);
    assert_eq!(LockTupleMode::LockTupleNoKeyExclusive as u32, 2);
    assert_eq!(LockWaitPolicy::LockWaitError as u32, 2);
    assert_eq!(HEAP_TABLE_AM_OID, 2);
    assert_eq!(DEFAULT_TABLE_ACCESS_METHOD, "heap");
}

#[test]
fn nextpower2_matches_pg_bitutils() {
    assert_eq!(pg_nextpower2_32(1), 1);
    assert_eq!(pg_nextpower2_32(2), 2);
    assert_eq!(pg_nextpower2_32(3), 4);
    assert_eq!(pg_nextpower2_32(4), 4);
    assert_eq!(pg_nextpower2_32(5), 8);
    assert_eq!(pg_nextpower2_32(4097), 8192);
    assert_eq!(pg_nextpower2_32(1 << 30), 1 << 30);
    assert_eq!(pg_nextpower2_32((1 << 30) + 1), 1 << 31);
}

#[test]
fn am_resolution_and_slot_callbacks() {
    let ctx = MemoryContext::new("test");
    let mcx = ctx.mcx();

    let heap = make(mcx, 1, RELKIND_RELATION, HEAP_TABLE_AM_OID);
    assert_eq!(TableAm::of(&heap), Some(TableAm::Heap));
    assert_eq!(table_slot_callbacks(&heap), TupleSlotKind::BufferHeapTuple);

    let foreign = make(mcx, 2, RELKIND_FOREIGN_TABLE, 0);
    assert_eq!(TableAm::of(&foreign), None);
    assert_eq!(table_slot_callbacks(&foreign), TupleSlotKind::HeapTuple);

    let view = make(mcx, 3, RELKIND_VIEW, 0);
    assert_eq!(table_slot_callbacks(&view), TupleSlotKind::Virtual);
    let part = make(mcx, 4, RELKIND_PARTITIONED_TABLE, 0);
    assert_eq!(table_slot_callbacks(&part), TupleSlotKind::Virtual);

    // finish_bulk_insert is a no-op for heap (NULL callback in heapam_methods)
    assert!(table_finish_bulk_insert(&heap, TABLE_INSERT_SKIP_FSM).is_ok());
}

#[test]
fn guc_storage_and_check_hook() {
    assert_eq!(default_table_access_method(), "heap");
    set_default_table_access_method("heap2");
    assert_eq!(default_table_access_method(), "heap2");
    set_default_table_access_method(DEFAULT_TABLE_ACCESS_METHOD);

    assert!(synchronize_seqscans());
    set_synchronize_seqscans(false);
    assert!(!synchronize_seqscans());
    set_synchronize_seqscans(true);

    let source = ::types_guc::GucSource::PGC_S_SESSION;
    let mut extra = None;

    let mut ok = Some(String::from("heap"));
    assert!(matches!(
        check_default_table_access_method(&mut ok, &mut extra, source),
        Ok(true)
    ));

    let mut empty = Some(String::new());
    assert!(matches!(
        check_default_table_access_method(&mut empty, &mut extra, source),
        Ok(false)
    ));
    let mut none = None;
    assert!(matches!(
        check_default_table_access_method(&mut none, &mut extra, source),
        Ok(false)
    ));

    let mut long = Some("x".repeat(64));
    assert!(matches!(
        check_default_table_access_method(&mut long, &mut extra, source),
        Ok(false)
    ));
    let mut fits = Some("x".repeat(63));
    assert!(matches!(
        check_default_table_access_method(&mut fits, &mut extra, source),
        Ok(true)
    ));
}

#[test]
fn parallel_allocator_hands_out_every_block_exactly_once() {
    let ctx = MemoryContext::new("test");
    let rel = make(ctx.mcx(), 1, RELKIND_RELATION, HEAP_TABLE_AM_OID);

    let nblocks: u32 = 4096;
    let pb = pbscan(nblocks);
    let mut w1 = ParallelBlockTableScanWorkerData::default();
    let mut w2 = ParallelBlockTableScanWorkerData::default();
    table_block_parallelscan_startblock_init(&rel, &mut w1, &pb).unwrap();
    table_block_parallelscan_startblock_init(&rel, &mut w2, &pb).unwrap();

    // 4096/2048 = 2-block chunks, non-sync scan starts at block 0.
    assert_eq!(w1.phsw_chunk_size, 2);
    assert_eq!(pb.phs_startblock.load(Ordering::Relaxed), 0);

    let mut seen = vec![0u32; nblocks as usize];
    let mut done1 = false;
    let mut done2 = false;
    while !(done1 && done2) {
        if !done1 {
            let p = table_block_parallelscan_nextpage(&rel, &mut w1, &pb).unwrap();
            if p == InvalidBlockNumber {
                done1 = true;
            } else {
                seen[p as usize] += 1;
            }
        }
        if !done2 {
            let p = table_block_parallelscan_nextpage(&rel, &mut w2, &pb).unwrap();
            if p == InvalidBlockNumber {
                done2 = true;
            } else {
                seen[p as usize] += 1;
            }
        }
    }
    assert!(seen.iter().all(|&c| c == 1));

    // reinitialize resets the shared allocation counter for rescan
    table_block_parallelscan_reinitialize(&rel, &pb);
    assert_eq!(pb.phs_nallocated.load(Ordering::SeqCst), 0);
}

#[test]
fn parallel_allocator_ramps_down_and_wraps_from_startblock() {
    let ctx = MemoryContext::new("test");
    let rel = make(ctx.mcx(), 1, RELKIND_RELATION, HEAP_TABLE_AM_OID);

    // Large enough that chunk_size starts > 1 (16384/2048 = 8) and must halve
    // to 1 near the end.
    let nblocks: u32 = 16384;
    let pb = pbscan(nblocks);
    let mut w = ParallelBlockTableScanWorkerData::default();
    table_block_parallelscan_startblock_init(&rel, &mut w, &pb).unwrap();
    assert_eq!(w.phsw_chunk_size, 8);

    // Simulate a syncscan-style nonzero start position.
    pb.phs_startblock.store(100, Ordering::Relaxed);

    let first = table_block_parallelscan_nextpage(&rel, &mut w, &pb).unwrap();
    assert_eq!(first, 100);

    let mut count = 1u64;
    let mut wrapped = false;
    loop {
        let p = table_block_parallelscan_nextpage(&rel, &mut w, &pb).unwrap();
        if p == InvalidBlockNumber {
            break;
        }
        if p < 100 {
            wrapped = true;
        }
        count += 1;
    }
    assert_eq!(count, nblocks as u64);
    assert!(wrapped);
    assert_eq!(w.phsw_chunk_size, 1); // rampdown reached single-block chunks
}

#[test]
fn estimate_size_math_matches_tableam_c() {
    let mut pages = 0u32;
    let mut tuples = 0.0f64;
    let mut allvisfrac = 0.0f64;

    // Vacuumed stats branch: density = reltuples/relpages scaled to curpages.
    block_relation_estimate_size_math(
        200,
        100,
        1000.0,
        50,
        false,
        |_| panic!("stats branch must not hit the fallback"),
        None,
        &mut pages,
        &mut tuples,
        &mut allvisfrac,
    )
    .unwrap();
    assert_eq!(pages, 200);
    assert_eq!(tuples, 2000.0);
    assert_eq!(allvisfrac, 0.25);

    // Empty relation: quick exit.
    block_relation_estimate_size_math(
        0,
        0,
        0.0,
        0,
        false,
        |_| panic!("unreachable"),
        None,
        &mut pages,
        &mut tuples,
        &mut allvisfrac,
    )
    .unwrap();
    assert_eq!((pages, tuples, allvisfrac), (0, 0.0, 0.0));

    // Never-vacuumed: 10-page floor + density fallback; allvisfrac clamps to 1.
    block_relation_estimate_size_math(
        3,
        0,
        -1.0,
        64,
        false,
        |_| Ok(25.0),
        None,
        &mut pages,
        &mut tuples,
        &mut allvisfrac,
    )
    .unwrap();
    assert_eq!(pages, 10);
    assert_eq!(tuples, 250.0);
    assert_eq!(allvisfrac, 1.0);

    // With inheritance children the 10-page floor is skipped.
    block_relation_estimate_size_math(
        3,
        0,
        -1.0,
        0,
        true,
        |_| Ok(25.0),
        None,
        &mut pages,
        &mut tuples,
        &mut allvisfrac,
    )
    .unwrap();
    assert_eq!(pages, 3);
    assert_eq!(tuples, 75.0);
    assert_eq!(allvisfrac, 0.0);
}

#[test]
fn parallelscan_estimate_is_struct_size_for_snapshot_any() {
    let ctx = MemoryContext::new("test");
    let rel = make(ctx.mcx(), 1, RELKIND_RELATION, HEAP_TABLE_AM_OID);
    let sz = table_parallelscan_estimate(&rel, &None).unwrap();
    assert_eq!(sz, core::mem::size_of::<ParallelBlockTableScanDescData>());
    assert_eq!(sz, table_block_parallelscan_estimate(&rel));
}

#[test]
fn write_buffer_begin_gates_on_switch_and_am() {
    let ctx = MemoryContext::new("test");
    let heap = make(ctx.mcx(), 1, RELKIND_RELATION, HEAP_TABLE_AM_OID);
    let foreign = make(ctx.mcx(), 2, RELKIND_FOREIGN_TABLE, 0);
    // pgrcolumnar relation for the F-1 multi-insert census pin below.
    const PGRC_TEST_AM_OID: Oid = 77104;
    register_pgrcolumnar_table_am(PGRC_TEST_AM_OID);
    let pgrc = make(ctx.mcx(), 3, RELKIND_RELATION, PGRC_TEST_AM_OID);

    // Default OFF: never arms, heap AM or not.
    crate::write_buffer::write_multi_insert_set_for_tests(false);
    assert!(crate::write_buffer::write_buffer_begin(&heap).is_none());
    assert!(crate::write_buffer::write_buffer_begin(&foreign).is_none());

    // ON: arms for the heap AM only (an armed buffer starts empty).
    crate::write_buffer::write_multi_insert_set_for_tests(true);
    let buf = crate::write_buffer::write_buffer_begin(&heap).expect("armed for heap");
    assert_eq!(buf.nused, 0);
    assert_eq!(buf.bytes, 0);
    assert!(buf.slots.is_empty());
    assert!(crate::write_buffer::write_buffer_begin(&foreign).is_none());
    // The F-1 cross-check's multi-insert census pin: the W1 write buffer is
    // the only non-COPY table_multi_insert driver in-tree, and it must
    // NEVER arm for a pgrcolumnar relation — its CTAS/matview receivers
    // stay on the per-tuple + table_finish_bulk_insert path (the ONE
    // publish point). A caller that reached table_multi_insert on a v2
    // relation and never called table_finish_bulk_insert would buffer rows
    // the eoxact purge silently abandons (the F-1 loss mechanism).
    assert!(crate::write_buffer::write_buffer_begin(&pgrc).is_none());

    crate::write_buffer::write_multi_insert_set_for_tests(false);
}

#[test]
fn pgrc_tuple_update_delete_reach_the_front_at_the_dispatch() {
    // F2 L4: UPDATE/DELETE on a pgrcolumnar relation go to pgrc_front::dml (Stage B); a position no scan of the
    // statement stamped refuses there (22023) before any catalog or tombstone access.
    const PGRC_TEST_AM_OID: Oid = 77103;
    register_pgrcolumnar_table_am(PGRC_TEST_AM_OID);
    let ctx = MemoryContext::new("test");
    let rel = make(ctx.mcx(), 7, RELKIND_RELATION, PGRC_TEST_AM_OID);
    let tid = ItemPointerData::default();
    let mut tmfd = TM_FailureData::default();
    let not_valid = "pgrcolumnar: position not valid in this statement";

    let e = table_tuple_delete(ctx.mcx(), &rel, &tid, 0, &None, &None, true, &mut tmfd, false)
        .err().unwrap();
    assert_eq!(e.sqlstate(), types_error::ERRCODE_INVALID_PARAMETER_VALUE);
    assert_eq!(e.message(), not_valid);

    let mut slot = table_slot_create(ctx.mcx(), &rel).expect("virtual slot for a pgrc rel");
    let mut lockmode = LockTupleMode::LockTupleShare;
    let mut update_indexes = TU_UpdateIndexes::TU_None;
    let e = table_tuple_update(
        ctx.mcx(),
        &rel,
        &tid,
        &mut slot,
        0,
        &None,
        &None,
        true,
        &mut tmfd,
        &mut lockmode,
        &mut update_indexes,
    )
    .err().unwrap();
    assert_eq!(e.sqlstate(), types_error::ERRCODE_INVALID_PARAMETER_VALUE);
    assert_eq!(e.message(), not_valid);
    // LOCK: a pgrcolumnar UPDATE is a no-key update (no unique index), as heap_update's lock mode for it; IX1 L2:
    // the new version has a new ID, so every index takes an entry for it
    assert_eq!((lockmode, update_indexes), (LockTupleMode::LockTupleNoKeyExclusive, TU_UpdateIndexes::TU_All));
}

#[test]
fn pgrc_index_callbacks_raise_a_typed_error_not_a_panic() {
    // IX1 L0: a pgrcolumnar index fetch state begins (the fetch path by row ID), reads no pages (no prefetch, no
    // same-block batches) and never answers all-visible. IX1 L4: table_beginscan_bm attaches a bitmap source; a pgrc
    // descriptor without one (no bitmap scan began it) refuses the bitmap callbacks with 0A000.
    const PGRC_TEST_AM_OID: Oid = 77105;
    register_pgrcolumnar_table_am(PGRC_TEST_AM_OID);
    let ctx = MemoryContext::new("pgrc-index-callbacks");
    let rel = make(ctx.mcx(), 8, RELKIND_RELATION, PGRC_TEST_AM_OID);
    let mut fetch = table_index_fetch_begin(&rel).expect("IX1 L0: a pgrc fetch state");
    assert!(matches!(fetch, IndexFetchTableData::Pgrc(_)));
    assert!(!table_index_fetch_reads_pages(&fetch));
    assert_eq!(fetch.rel().rd_id, rel.rd_id);
    table_index_fetch_set_needed_attrs(&mut fetch, Some(vec![true, false].into_boxed_slice()));
    table_index_fetch_reset(&mut fetch);
    table_index_fetch_end(fetch);
    let tid = ItemPointerData::new(1, 1);
    assert!(!table_index_fetch_all_visible(&rel, &tid, |_| unreachable!("pgrc consults no visibility map")).unwrap());
    let e = cb2_refused("bitmap scans");
    assert_eq!(e.sqlstate(), types_error::ERRCODE_FEATURE_NOT_SUPPORTED);
    assert_eq!(e.message(), "pgrcolumnar does not support bitmap scans");
}


