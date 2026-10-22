//! B-tree access method (nbtree.c/nbtsearch.c/nbtinsert.c/nbtpage.c/
//! nbtutils.c/nbtpreprocesskeys.c): read path (SAOP arrays + PG 18 skip
//! scan), insert/split, and the VACUUM lane (bulkdelete/cleanup + page
//! deletion) plus row comparisons. Phase 2, loud panics, never silent:
//! dedup, mark/restore across primitive scans.
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(clippy::too_many_arguments)]
#![allow(irrefutable_let_patterns)]

mod dedup;
mod delete;
mod fcframe;
mod insert;
pub mod image;
pub mod imagerel;
pub mod itup;
mod page;
mod pagedel;
mod parallel;
pub mod pivots;
mod preprocess;
pub mod readrel;
mod search;
mod splitloc;
mod utils;
pub mod vacuum;
mod wal;

#[cfg(test)]
mod tests;

pub use insert::btinsert;
pub use page::{bt_getrootheight, bt_initmetapage, bt_metaversion, bt_pageinit};
pub use parallel::{bt_parallel_abort, btparallelrescan};
pub use pivots::{bt_pivots, Pivots};
pub use image::IndexImage;
pub use imagerel::ImageRel;
pub use readrel::BtReadRel;
pub use vacuum::{
    bt_chunked_bulkdelete_begin, bt_chunked_bulkdelete_finish, bt_chunked_cleanup_begin,
    bt_chunked_cleanup_finish, bt_chunked_scan_step, btbulkdelete, btbulkdelete_collect,
    btvacuumcleanup, BtChunkedCleanup, BtVacChunkedScan, IndexVacuumInfo,
};

use ::mcx::Mcx;
use ::types_core::{InvalidSubTransactionId, BLCKSZ};
use ::types_error::PgResult;
use ::types_nbtree::{BTScanOpaqueData, BTScanPosInvalidate, BTScanPosIsPinned, BTScanPosIsValid};
use ::types_rel::Relation;
use ::types_relscan::{relation_get_index_scan, IndexScanDescData, IndexScanOpaque};
use ::types_scan::scankey::ScanKeyData;
use ::types_scan::sdir::ScanDirection;
use ::types_snapshot::IsMVCCSnapshot;

use search::{bt_first, bt_gettuple_continue, pos_unpin_if_pinned, restore_scanpos, ScanCtx};
use utils::bt_killitems;

pub use fcframe::OrderProcFrame;
pub use search::{bt_peek_same_block_tids, BtScanInsert};
pub use utils::{
    bt_allequalimage, bt_check_third_page, bt_keep_natts_fast, bt_kill_by_block, bt_mkscankey, bt_truncate,
    BTreeShmemInit, BTreeShmemResetAfterCrash, BTreeShmemSize,
};

/// skey.h SK_ROW_HEADER contract: sk_argument holds the pointer word of the
/// arena-owned subsidiary ScanKeyData array, SK_ROW_END-terminated. All
/// copies of the header share the one array, as C's struct assignment does.
///
/// # Safety
/// `header` must be a SK_ROW_HEADER key whose subsidiary array (built by
/// ExecIndexBuildScanKeys) outlives the scan; the caller must not hold
/// another live reference to the array.
// Awaits the row-compare preprocess/mark wiring (unported-census 2026-08-05 port program).
#[allow(dead_code)]
pub(crate) unsafe fn row_compare_members_mut<'a>(header: &ScanKeyData) -> &'a mut [ScanKeyData] {
    use ::types_scan::scankey::{SK_ROW_END, SK_ROW_HEADER};
    debug_assert!(header.sk_flags & SK_ROW_HEADER != 0);
    let first = header.sk_argument.as_usize() as *mut ScanKeyData;
    let mut n = 1usize;
    // SAFETY: caller contract — `first` addresses a live SK_ROW_END-
    // terminated array.
    unsafe {
        while (*first.add(n - 1)).sk_flags & SK_ROW_END == 0 {
            n += 1;
        }
        core::slice::from_raw_parts_mut(first, n)
    }
}

#[cold]
#[inline(never)]
fn non_btree_opaque() -> ! {
    panic!("nbtree entry point reached with a non-btree scan opaque")
}

pub(crate) fn check_for_interrupts() -> PgResult<()> {
    if init_small::globals::InterruptPending() {
        return postgres_seams::check_for_interrupts::call();
    }
    Ok(())
}

macro_rules! split_scan {
    ($scan:expr) => {
        split_scan!(@with $scan, indexRelation => indexRelation
            .as_ref()
            .expect("index scan parked (skeleton)"))
    };
    ($scan:expr, $rel:expr) => {
        split_scan!(@with $scan, _indexRelation => $rel)
    };
    (@with $scan:expr, $ir:ident => $rel:expr) => {{
        let IndexScanDescData {
            indexRelation: $ir,
            xs_snapshot,
            keyData,
            ignore_killed_tuples,
            xs_heaptid,
            xs_itup,
            xs_pgstat,
            xs_nsearches,
            opaque,
            parallel_scan,
            ..
        } = $scan;
        let IndexScanOpaque::Btree(so) = opaque else {
            non_btree_opaque()
        };
        ScanCtx {
            rel: $rel,
            so: &mut **so,
            snapshot: xs_snapshot.as_deref(),
            ignore_killed_tuples: *ignore_killed_tuples,
            input_keys: keyData.as_mut_slice(),
            xs_heaptid,
            xs_itup,
            xs_pgstat,
            xs_nsearches,
            parallel: parallel_scan.as_deref().map(|p| {
                let ::types_relscan::ParallelIndexAmShared::Btree(b) = &p.am;
                b
            }),
            frame: crate::fcframe::OrderProcFrame::new(),
        }
    }};
}

/// btbeginscan.
pub fn btbeginscan<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    nkeys: i32,
    norderbys: i32,
) -> PgResult<IndexScanDescData<'mcx>> {
    debug_assert!(norderbys == 0);
    let so = BTScanOpaqueData::alloc_in(mcx)?;
    let mut scan = relation_get_index_scan(
        mcx,
        rel,
        nkeys,
        norderbys,
        IndexScanOpaque::Btree(so),
        xact::TransactionStartedDuringRecovery(),
    )?;
    scan.xs_itupdesc = Some(rel.rd_att.clone());
    Ok(scan)
}

// RelationNeedsWAL (rel.h); XLogIsNeeded ≡ the xlog_standby_info_active seam.
pub(crate) fn relation_needs_wal(rel: &Relation<'_>) -> bool {
    relation_data_needs_wal(rel)
}

fn relation_data_needs_wal(rel: &::types_rel::RelationData<'_>) -> bool {
    rel.is_permanent()
        && (transam_xlog_seams::xlog_standby_info_active::call()
            || (rel.rd_createSubid.get() == InvalidSubTransactionId
                && rel.rd_firstRelfilelocatorSubid.get() == InvalidSubTransactionId))
}

/// RelationIsAccessibleInLogicalDecoding (utils/rel.h): the isCatalogRel flag
/// of xl_btree_delete (nbtpage.c:1527) and xl_btree_reuse_page
/// (nbtpage.c:947), evaluated on the HEAP relation.
pub fn relation_is_accessible_in_logical_decoding(rel: &::types_rel::RelationData<'_>) -> bool {
    transam_xlog_seams::xlog_logical_info_active::call()
        && relation_data_needs_wal(rel)
        && (catalog_seams::is_catalog_relation::call(rel) || rel.is_used_as_catalog_table())
}

/// btrescan. `scankey: None` restarts with the keys already in scan.keyData.
pub fn btrescan(scan: &mut IndexScanDescData<'_>, scankey: Option<&[ScanKeyData]>) -> PgResult<()> {
    {
        let IndexScanOpaque::Btree(so) = &mut scan.opaque else {
            non_btree_opaque()
        };
        if BTScanPosIsValid(&so.currPos) {
            if so.numKilled > 0 {
                bt_killitems(
                    scan.indexRelation
                        .as_ref()
                        .expect("index scan parked (skeleton)"),
                    so,
                )?;
            }
            pos_unpin_if_pinned(&mut so.currPos)?;
            BTScanPosInvalidate(&mut so.currPos);
        }

        // off for index-only scans, non-MVCC snapshots, unlogged, bitmap.
        so.dropPin = !scan.xs_want_itup
            && scan.xs_snapshot.as_deref().is_some_and(IsMVCCSnapshot)
            && scan.indexRelation.as_ref().is_some_and(relation_needs_wal)
            && scan.heapRelation.is_some();

        so.markItemIndex = -1;
        so.needPrimScan = false;
        so.scanBehind = false;
        so.oppositeDirCheck = false;
        mark_unpin(so)?;
        mark_invalidate(so);

        // the mark's tuple buffer is made with the mark (search.rs copy_scanpos)
        if scan.xs_want_itup && so.currTuples.is_none() {
            let mcx = *so.keyData.allocator();
            so.currTuples = Some(::mcx::vec_with_capacity_in(mcx, BLCKSZ)?);
        }

        so.numberOfKeys = 0; // until _bt_preprocess_keys sets it
        so.numArrayKeys = 0; // ditto
    }

    if let Some(keys) = scankey {
        if scan.numberOfKeys > 0 {
            debug_assert!(keys.len() == scan.numberOfKeys as usize);
            scan.keyData.clear();
            scan.keyData.extend(keys.iter().cloned());
        }
    }
    Ok(())
}

/// btgettuple.
pub fn btgettuple(scan: &mut IndexScanDescData<'_>, dir: ScanDirection) -> PgResult<bool> {
    debug_assert!(scan.heapRelation.is_some());

    scan.xs_recheck = false;

    let kill_prior_tuple = scan.kill_prior_tuple;
    let mut ctx = split_scan!(&mut *scan);
    gettuple_loop(&mut ctx, dir, kill_prior_tuple)
}

/// A reader that marks the dead entries of the leaves it read later, by block (`bt_kill_by_block`): from now on the
/// scan reads each leaf's LSN under its read lock although an index-only scan keeps the pin (`lsnWithPin`).
pub fn bt_scan_lsn_with_pin(scan: &mut IndexScanDescData<'_>) {
    let IndexScanOpaque::Btree(so) = &mut scan.opaque else { non_btree_opaque() };
    so.lsnWithPin = true;
}

/// The entry btgettuple returned last: its leaf's block, its offset on the leaf (a posting tuple's TIDs share one) and
/// the leaf's LSN when the scan read it (`bt_scan_lsn_with_pin`, else 0). `None` without a current leaf.
#[inline]
pub fn bt_scan_entry(scan: &IndexScanDescData<'_>) -> Option<(::types_core::BlockNumber, ::types_core::OffsetNumber, ::types_core::XLogRecPtr)> {
    let IndexScanOpaque::Btree(so) = &scan.opaque else { non_btree_opaque() };
    let pos = &so.currPos;
    if !BTScanPosIsValid(pos) || pos.itemIndex < pos.firstItem || pos.itemIndex > pos.lastItem {
        return None;
    }
    // SAFETY: itemIndex within [firstItem, lastItem], written when the leaf was read.
    let item = unsafe { pos.item(pos.itemIndex as usize) };
    Some((pos.currPage, item.indexOffset, pos.lsn))
}

/// btgettuple over `rel` instead of the descriptor's index relation (which
/// may be absent). The descriptor's keys and position state are used as
/// btgettuple uses them. The caller must leave `kill_prior_tuple` false:
/// killed items are flushed at rescan/end through the descriptor's own
/// relation.
pub fn btgettuple_over<R: BtReadRel + ?Sized>(
    scan: &mut IndexScanDescData<'_>,
    rel: &R,
    dir: ScanDirection,
) -> PgResult<bool> {
    debug_assert!(!scan.kill_prior_tuple || scan.indexRelation.is_some());

    scan.xs_recheck = false;

    let kill_prior_tuple = scan.kill_prior_tuple;
    let mut ctx = split_scan!(&mut *scan, rel);
    gettuple_loop(&mut ctx, dir, kill_prior_tuple)
}

fn gettuple_loop<R: BtReadRel + ?Sized>(
    ctx: &mut ScanCtx<'_, '_, R>,
    dir: ScanDirection,
    kill_prior_tuple: bool,
) -> PgResult<bool> {
    // Each loop iteration performs another primitive index scan.
    loop {
        let res = if !BTScanPosIsValid(&ctx.so.currPos) {
            bt_first(ctx, dir)?
        } else {
            bt_gettuple_continue(ctx, dir, kill_prior_tuple)?
        };
        if res {
            return Ok(true);
        }
        if ctx.so.numArrayKeys == 0 || !utils::bt_start_prim_scan(ctx.so, ctx.parallel) {
            return Ok(false);
        }
    }
}

/// btgetbitmap: drain all matching heap TIDs into `tbm`, forward only.
pub fn btgetbitmap(
    scan: &mut IndexScanDescData<'_>,
    tbm: &mut tidbitmap::TIDBitmap<'_>,
) -> PgResult<i64> {
    debug_assert!(scan.heapRelation.is_none());
    let mut ctx = split_scan!(&mut *scan);
    getbitmap_loop(&mut ctx, tbm)
}

/// btgetbitmap over `rel` instead of the descriptor's index relation (which
/// may be absent): the same walk, reading pages and catalog answers through
/// `rel`.
pub fn btgetbitmap_over<R: BtReadRel + ?Sized>(
    scan: &mut IndexScanDescData<'_>,
    rel: &R,
    tbm: &mut tidbitmap::TIDBitmap<'_>,
) -> PgResult<i64> {
    debug_assert!(scan.heapRelation.is_none());
    let mut ctx = split_scan!(&mut *scan, rel);
    getbitmap_loop(&mut ctx, tbm)
}

fn getbitmap_loop<R: BtReadRel + ?Sized>(
    ctx: &mut ScanCtx<'_, '_, R>,
    tbm: &mut tidbitmap::TIDBitmap<'_>,
) -> PgResult<i64> {
    let mut ntids: i64 = 0;
    // Each loop iteration performs another primitive index scan.
    loop {
        if bt_first(ctx, ::types_scan::sdir::ForwardScanDirection)? {
            tbm.add_tuples(core::slice::from_ref(ctx.xs_heaptid), false)?;
            ntids += 1;

            loop {
                ctx.so.currPos.itemIndex += 1;
                if ctx.so.currPos.itemIndex > ctx.so.currPos.lastItem {
                    if !search::bt_next(ctx, ::types_scan::sdir::ForwardScanDirection)? {
                        break;
                    }
                }
                // SAFETY: itemIndex in [firstItem, lastItem], written by bt_readpage.
                let item = unsafe { ctx.so.currPos.item(ctx.so.currPos.itemIndex as usize) };
                tbm.add_tuples(core::slice::from_ref(&item.heapTid), false)?;
                ntids += 1;
            }
        }
        if ctx.so.numArrayKeys == 0 || !utils::bt_start_prim_scan(ctx.so, ctx.parallel) {
            break;
        }
    }
    Ok(ntids)
}

/// btendscan. Storage is freed with the scan value (mcx lifetime).
pub fn btendscan(scan: &mut IndexScanDescData<'_>) -> PgResult<()> {
    let IndexScanOpaque::Btree(so) = &mut scan.opaque else {
        non_btree_opaque()
    };

    if BTScanPosIsValid(&so.currPos) {
        if so.numKilled > 0 {
            bt_killitems(
                scan.indexRelation
                    .as_ref()
                    .expect("index scan parked (skeleton)"),
                so,
            )?;
        }
        pos_unpin_if_pinned(&mut so.currPos)?;
    }

    so.markItemIndex = -1;
    mark_unpin(so)?;
    Ok(())
}

/// Executor-skeleton park (no C counterpart): btendscan's release work —
/// killed-item flush, pin release — but the opaque (and its ~27KB workspace)
/// stays allocated for reuse. Positions are invalidated and the kill count
/// cleared so the eventual btrescan never replays a stale page.
pub fn btparkscan(scan: &mut IndexScanDescData<'_>) -> PgResult<()> {
    let IndexScanOpaque::Btree(so) = &mut scan.opaque else {
        non_btree_opaque()
    };

    if BTScanPosIsValid(&so.currPos) {
        if so.numKilled > 0 {
            bt_killitems(
                scan.indexRelation
                    .as_ref()
                    .expect("index scan parked (skeleton)"),
                so,
            )?;
        }
        pos_unpin_if_pinned(&mut so.currPos)?;
        BTScanPosInvalidate(&mut so.currPos);
    }
    so.numKilled = 0;
    so.markItemIndex = -1;
    mark_unpin(so)?;
    mark_invalidate(so);
    Ok(())
}

/// The mark's pin dropped, when the scan has made one (`markPos` is made at the first mark saved off its page).
fn mark_unpin(so: &mut BTScanOpaqueData<'_>) -> PgResult<()> {
    match so.markPos.as_deref_mut() {
        Some(m) => pos_unpin_if_pinned(m),
        None => Ok(()),
    }
}

/// The mark invalidated, when the scan has made one.
fn mark_invalidate(so: &mut BTScanOpaqueData<'_>) {
    if let Some(m) = so.markPos.as_deref_mut() {
        BTScanPosInvalidate(m);
    }
}

/// btmarkpos.
pub fn btmarkpos(scan: &mut IndexScanDescData<'_>) -> PgResult<()> {
    let IndexScanOpaque::Btree(so) = &mut scan.opaque else {
        non_btree_opaque()
    };

    mark_unpin(so)?;

    // the scan leaves the page before the mark is moved.
    if BTScanPosIsValid(&so.currPos) {
        so.markItemIndex = so.currPos.itemIndex;
    } else {
        mark_invalidate(so);
        so.markItemIndex = -1;
    }
    Ok(())
}

/// btrestrpos.
pub fn btrestrpos(scan: &mut IndexScanDescData<'_>) -> PgResult<()> {
    let IndexScanOpaque::Btree(so) = &mut scan.opaque else {
        non_btree_opaque()
    };

    if so.markItemIndex >= 0 {
        so.currPos.itemIndex = so.markItemIndex;
        return Ok(());
    }

    if BTScanPosIsValid(&so.currPos) {
        if so.numKilled > 0 {
            bt_killitems(
                scan.indexRelation
                    .as_ref()
                    .expect("index scan parked (skeleton)"),
                so,
            )?;
        }
        pos_unpin_if_pinned(&mut so.currPos)?;
    }

    if let Some(m) = so.markPos.as_deref().filter(|m| BTScanPosIsValid(m)) {
        if BTScanPosIsPinned(m) {
            bufmgr_seams::incr_buffer_ref_count::call(m.buf);
        }
        restore_scanpos(so)?;
        // Reset the scan's array keys (see _bt_steppage for why).
        if so.numArrayKeys != 0 {
            let dir = so.currPos.dir;
            utils::bt_start_array_keys(so, dir);
            so.needPrimScan = false;
        }
    } else {
        BTScanPosInvalidate(&mut so.currPos);
    }
    Ok(())
}

#[cfg(feature = "bench-internals")]
pub mod bench_internals {
    pub use crate::fcframe::OrderProcFrame;
    pub use crate::page::{bt_relbuf, page_item, page_opaque};
    pub use crate::search::{bt_binsrch, bt_compare, bt_search, order_procinfo};
}

/// Internals reachable only by contrib/amcheck's verifier (verify_nbtree.c),
/// which reads local page copies and re-searches the live tree.
pub mod amcheck {
    pub use crate::fcframe::OrderProcFrame;
    pub use crate::insert::bt_rootdescend;
    pub use crate::page::{bt_checkpage_ref, page_item, page_meta, page_opaque};
    pub use crate::search::bt_compare;
    pub use crate::utils::{bt_allequalimage, bt_check_natts};
}
