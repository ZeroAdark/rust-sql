//! heapam_handler.c bitmap-scan lane (heapam_scan_bitmap_next_tuple +
//! BitmapHeapScanNextBlock). The TBM iterator + bitmap ride in as parameters
//! instead of rs_base.st.rs_tbmiterator (the iterator carries no bitmap
//! back-pointer), so the stream callback is built per call.
//! Divergence (D3608): the stream is begun at the first page that reads
//! through it, not at heap_beginscan, and the fused page batch
//! (heap_scan_bitmap_next_pagebatch, Gravity's) reads a resident page with
//! one pin, as before the stream; its first page that is not resident goes
//! to the stream, which reads it and the rest of the walk with its
//! look-ahead. A walk over cached pages (every OLTP statement) then pays no
//! stream setup, look-ahead or IO batching per page.

use tidbitmap::{TIDBitmap, TbmIterator, TbmPageResult, TBM_MAX_TUPLES_PER_PAGE};
use ::types_core::{BlockNumber, ForkNumber, InvalidBlockNumber, InvalidBuffer};

use crate::{
    batch_credit_page, batch_credit_upto, heap_hot_search_buffer, store_ctup_into_slot,
    HeapCheckForSerializableConflictOut, HeapScanDescData,
};
use ::bufmgr_seams::BufferPin;
use ::mcx::Mcx;
use ::tableam_vocab::SO_TYPE_BITMAPSCAN;
use ::types_error::PgResult;
use ::types_slot::SlotData;
use ::types_storage::bufpage::MaxHeapTuplesPerPage;
use ::types_tuple::{FirstOffsetNumber, HeapTupleData, ItemPointerData};

pub fn heap_scan_bitmap_next_tuple<'mcx>(
    mcx: Mcx<'mcx>,
    scan: &mut HeapScanDescData<'mcx>,
    tbm: Option<&TIDBitmap<'_>>,
    iterator: &mut TbmIterator,
    slot: &mut SlotData<'mcx>,
    recheck: &mut bool,
    lossy_pages: &mut u64,
    exact_pages: &mut u64,
) -> PgResult<bool> {
    while scan.rs_cindex >= scan.rs_ntuples {
        if !bitmap_next_block(mcx, scan, tbm, iterator, recheck, lossy_pages, exact_pages, false)? {
            return Ok(false);
        }
    }

    let i = scan.rs_cindex;
    heap_scan_bitmap_batch_store(mcx, scan, i, slot);
    Ok(true)
}

/// Fused-drive support: advance to the next page with visible tuples and
/// return the staged count (rs_vistuples[0..n], visibility resolved at
/// staging); 0 = bitmap exhausted. Resident pages are read directly; the
/// walk's first other page and the rest go through the stream (D3608).
pub fn heap_scan_bitmap_next_pagebatch<'mcx>(
    mcx: Mcx<'_>,
    scan: &mut HeapScanDescData<'mcx>,
    tbm: Option<&TIDBitmap<'_>>,
    iterator: &mut TbmIterator,
    recheck: &mut bool,
    lossy_pages: &mut u64,
    exact_pages: &mut u64,
) -> PgResult<u32> {
    loop {
        if !bitmap_next_block(mcx, scan, tbm, iterator, recheck, lossy_pages, exact_pages, true)? {
            return Ok(0);
        }
        if scan.rs_ntuples > 0 {
            return Ok(scan.rs_ntuples);
        }
    }
}

/// Store staged tuple `i` of the current bitmap page into `slot`.
#[inline(always)]
pub fn heap_scan_bitmap_batch_store<'mcx>(
    mcx: Mcx<'mcx>,
    scan: &mut HeapScanDescData<'mcx>,
    i: u32,
    slot: &mut SlotData<'mcx>,
) {
    debug_assert!(i < scan.rs_ntuples);
    // C heapam_scan_bitmap_next_tuple's pgstat_count_heap_fetch(rs_rd), per
    // tuple returned (the fused batch drive stores only its survivors; the
    // page's remainder is credited when the feed leaves it).
    batch_credit_upto(scan, i + 1);
    let targoffset = scan.rs_vistuples[i as usize];
    let block = scan.rs_cblock;
    let rd_id = scan.rs_base.rs_rd.rd_id;
    let pin = scan
        .rs_cbuf
        .as_ref()
        .expect("bitmap scan positioned without a buffer");
    let page = pin.page();
    let lp = page.item_id(targoffset);
    debug_assert!(lp.is_normal());
    let (ptr, len) = page.item_raw(lp);
    // SAFETY: normal line pointer on the page pinned by rs_cbuf; the struct
    // invariant ties rs_ctup's image to that pin.
    scan.rs_ctup = Some(unsafe {
        HeapTupleData::from_raw_parts(ptr, len, ItemPointerData::new(block, targoffset), rd_id)
    });

    store_ctup_into_slot(mcx, scan, slot);

    scan.rs_cindex = i + 1;
}

/// Begin the scan's bitmap stream if it has none (C does at heap_beginscan).
fn ensure_bitmap_stream(mcx: Mcx<'_>, scan: &mut HeapScanDescData<'_>) -> PgResult<()> {
    if scan.rs_read_stream.is_none() && bufmgr_seams::start_read_buffers::is_installed() {
        // The callback takes no lock a backend waits on for IO (spinlocks /
        // atomics when parallel), so batch mode is safe.
        let stream = ::read_stream::read_stream_begin_relation(
            mcx,
            ::read_stream::READ_STREAM_DEFAULT | ::read_stream::READ_STREAM_USE_BATCHING,
            scan.rs_strategy.clone(),
            &scan.rs_base.rs_rd,
            ForkNumber::MAIN_FORKNUM,
        )?;
        scan.rs_read_stream = Some(Box::new(crate::HeapReadStream::Bitmap(stream)));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn bitmap_next_block(
    mcx: Mcx<'_>,
    scan: &mut HeapScanDescData<'_>,
    tbm: Option<&TIDBitmap<'_>>,
    iterator: &mut TbmIterator,
    recheck: &mut bool,
    lossy_pages: &mut u64,
    exact_pages: &mut u64,
    direct: bool,
) -> PgResult<bool> {
    debug_assert!((scan.rs_base.rs_flags & SO_TYPE_BITMAPSCAN) != 0);

    // Leaving the staged page: every row on it was handed to the consumer.
    batch_credit_page(scan);
    scan.rs_cindex = 0;
    scan.rs_ntuples = 0;
    scan.rs_batch_credited = 0;

    if let Some(pin) = scan.rs_cbuf.take() {
        pin.release();
    }

    let serializable = xact_seams::isolation_is_serializable::call();
    let nblocks = scan.rs_nblocks;
    let mut offsets = [0 as ::types_core::OffsetNumber; TBM_MAX_TUPLES_PER_PAGE];
    // D3608: the page batch reads resident pages itself, one pin each; the
    // first page that is not goes to the stream (`handed`, its first block),
    // and so does the rest of the walk.
    let mut handed = None;
    if direct
        && !scan.rs_bitmap_streaming
        && bufmgr_seams::read_buffer_if_valid::is_installed()
        && bufmgr_seams::start_read_buffers::is_installed()
    {
        loop {
            crate::check_for_interrupts()?;
            let Some(tbmres) = iterator.next(tbm) else {
                return Ok(false);
            };
            if !serializable && tbmres.blockno >= nblocks {
                continue;
            }
            let buf = bufmgr_seams::read_buffer_if_valid::call(
                &scan.rs_base.rs_rd,
                tbmres.blockno,
                scan.rs_strategy.clone(),
            )?;
            if buf == InvalidBuffer {
                handed = Some(tbmres.to_page_result());
                scan.rs_bitmap_streaming = true;
                break;
            }
            let noffsets = if !tbmres.lossy { tbmres.extract_page_tuples(&mut offsets) } else { 0 };
            *recheck = tbmres.recheck;
            return bitmap_stage_page(
                scan, buf, tbmres.blockno, tbmres.lossy, &offsets[..noffsets], serializable,
                lossy_pages, exact_pages,
            );
        }
    }
    // C bitmapheap_stream_read_next.
    let mut next_page = |res: &mut TbmPageResult| -> PgResult<BlockNumber> {
        if let Some(h) = handed.take() {
            *res = h;
            return Ok(res.blockno);
        }
        loop {
            crate::check_for_interrupts()?;
            let Some(tbmres) = iterator.next(tbm) else {
                return Ok(InvalidBlockNumber);
            };
            // Entries past our snapshot of the relation end cannot be visible;
            // serializable isolation must still examine them.
            if !serializable && tbmres.blockno >= nblocks {
                continue;
            }
            *res = tbmres.to_page_result();
            return Ok(res.blockno);
        }
    };
    ensure_bitmap_stream(mcx, scan)?;
    let (buf, tbmres) = match scan.rs_read_stream.as_deref_mut() {
        Some(crate::HeapReadStream::Bitmap(stream)) => {
            let (buf, res) = stream.next_buffer_with_data(&mut next_page)?;
            if buf == InvalidBuffer {
                // Ready for an iterator installed after this one ran dry.
                stream.reset()?;
                scan.rs_bitmap_streaming = false;
                return Ok(false);
            }
            (buf, res)
        }
        _ => {
            let mut res = TbmPageResult::default();
            if next_page(&mut res)? == InvalidBlockNumber {
                return Ok(false);
            }
            let buf = bufmgr_seams::read_buffer_strategy::call(
                &scan.rs_base.rs_rd,
                res.blockno,
                scan.rs_strategy.clone(),
            )?;
            (buf, res)
        }
    };
    let noffsets = if !tbmres.lossy { tbmres.extract_page_tuples(&mut offsets) } else { 0 };
    *recheck = tbmres.recheck;
    bitmap_stage_page(
        scan, buf, tbmres.blockno, tbmres.lossy, &offsets[..noffsets], serializable, lossy_pages,
        exact_pages,
    )
}

/// The rest of BitmapHeapScanNextBlock for the page pinned in `buf`: prune,
/// then stage its visible tuples (exact: the bitmap's `offsets`; lossy: every
/// line pointer).
#[allow(clippy::too_many_arguments)]
fn bitmap_stage_page(
    scan: &mut HeapScanDescData<'_>,
    buf: ::types_core::Buffer,
    blockno: BlockNumber,
    lossy: bool,
    offsets: &[::types_core::OffsetNumber],
    serializable: bool,
    lossy_pages: &mut u64,
    exact_pages: &mut u64,
) -> PgResult<bool> {
    scan.rs_cblock = blockno;
    scan.rs_cbuf = BufferPin::adopt(buf);
    let pin = scan
        .rs_cbuf
        .as_ref()
        .expect("read_buffer returned an invalid buffer");

    pruneheap_seams::heap_page_prune_opt::call(&scan.rs_base.rs_rd, pin.buffer())?;

    let relation = &scan.rs_base.rs_rd;
    let snapshot = scan
        .rs_base
        .rs_snapshot
        .as_deref()
        .expect("bitmap heap scan requires an MVCC snapshot");
    let buffer = pin.buffer();

    // Found-visible tuples stay good under the pin alone after unlock.
    let lock = pin.lock_share()?;
    let mut ntup: u32 = 0;
    if !lossy {
        for &offnum in offsets {
            let tid = ItemPointerData::new(blockno, offnum);
            let res = heap_hot_search_buffer(tid, relation, pin, snapshot, false, true)?;
            if res.found {
                scan.rs_vistuples[ntup as usize] =
                    ::types_tuple::itemptr::ItemPointerGetOffsetNumber(&res.tid);
                ntup += 1;
            }
        }
    } else {
        let page = lock.page();
        let maxoff = page.max_offset_number();
        if maxoff as usize > MaxHeapTuplesPerPage {
            return Err(crate::line_pointer_count_corrupted(maxoff));
        }
        let mut offnum = FirstOffsetNumber;
        while offnum <= maxoff {
            let lp = page.item_id(offnum);
            if !lp.is_normal() {
                offnum += 1;
                continue;
            }
            let (ptr, len) = page.item_raw(lp);
            // SAFETY: normal line pointer on a pinned + share-locked heap page.
            let mut loctup = unsafe {
                HeapTupleData::from_raw_parts(
                    ptr,
                    len,
                    ItemPointerData::new(blockno, offnum),
                    relation.rd_id,
                )
            };
            let valid = crate::hv_seam::heap_tuple_satisfies_visibility::call(
                &mut loctup,
                snapshot,
                buffer,
            )?;
            if valid {
                scan.rs_vistuples[ntup as usize] = offnum;
                ntup += 1;
                // PredicateLockTID (predicate.c): per-tuple SIREAD lock under
                // lossy pages (C heapam_scan_bitmap_next_block); the exact-TID
                // branch takes it inside heap_hot_search_buffer.
                if serializable {
                    predicate_seams::predicate_lock_tid::call(
                        relation,
                        ItemPointerData::new(blockno, offnum),
                        snapshot,
                        loctup.t_data().xmin(),
                    )?;
                }
            }
            HeapCheckForSerializableConflictOut(valid, relation, &mut loctup, buffer, snapshot)?;
            offnum += 1;
        }
    }
    drop(lock);

    debug_assert!(ntup as usize <= MaxHeapTuplesPerPage);
    scan.rs_ntuples = ntup;

    if lossy {
        *lossy_pages += 1;
    } else {
        *exact_pages += 1;
    }

    Ok(true)
}
