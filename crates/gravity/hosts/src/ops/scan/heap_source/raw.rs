//! heap_source's unsafe (lint R6; construct bundle scan-05; its raw.rs child), each with its safety contract, moved as is
//! from heap_source.rs: the page walks (`normal_line`, `collect_visible`, `walk`) whole, and one-call wrappers for the rest
//! (a pinned buffer's page, the statement snapshot's dereference, a fast default's image, the staged page's deform) whose
//! callers keep their reasons beside the call.
#![allow(unsafe_code)]
use super::*;

/// A by-reference fast default's image length: a varlena's `varsize_any`, a cstring's bytes through its NUL, else
/// `attlen` bytes. The caller passes a present by-reference attmissingval: `p` addresses its image in the relcache entry
/// (varlena: a plain or short header, never toasted in pg_attribute's array; cstring: NUL-terminated; else attlen bytes).
pub(super) fn missing_len(attlen: i16, p: *const u8) -> usize {
    // SAFETY: the caller's contract above.
    unsafe {
        match attlen {
            -1 => types_tuple::varatt::varsize_any(p),
            -2 => core::ffi::CStr::from_ptr(p as *const core::ffi::c_char).to_bytes_with_nul().len(),
            n => n as usize,
        }
    }
}

/// The `len` bytes at `p` into `img`. The caller passes an `img` of at least `len` bytes and a `p` addressing `len` bytes
/// (`missing_len`'s image).
pub(super) fn copy_missing(p: *const u8, img: &mut [u64], len: usize) {
    // SAFETY: the caller's contract above.
    unsafe { core::ptr::copy_nonoverlapping(p, img.as_mut_ptr() as *mut u8, len) };
}

/// The statement's snapshot at `p`, `None` when `p` is null. The caller passes `snapshot_ptr`'s pointer: the leader's
/// registered snapshot, alive from bind to unbind (null on a claimless bind).
pub(super) fn snapshot_opt<'a>(p: *const SnapshotData<'static>) -> Option<&'a SnapshotData<'static>> {
    // SAFETY: the caller's contract above.
    unsafe { p.as_ref() }
}

/// The statement's snapshot at `p`. The caller passes a non-null `snapshot_ptr` pointer: the leader's registered snapshot,
/// alive and immutable from bind to unbind.
pub(super) fn snapshot_at<'a>(p: *const SnapshotData<'static>) -> &'a SnapshotData<'static> {
    // SAFETY: the caller's contract above.
    unsafe { &*p }
}

/// The page of buffer `buf`. The caller holds `buf` pinned by this thread while it reads the page (share-locked too where
/// the read needs the lock).
pub(super) fn pinned_page<'a>(buf: Buffer) -> PageRef<'a> {
    // SAFETY: the caller's contract above.
    unsafe { PageRef::from_raw(bufmgr_seams::buffer_page_ptr(buf)) }
}

/// heap_window's `deform_rows` over the staged page. The caller keeps the page pinned while it is staged, `vis` holding
/// its normal line pointers.
pub(super) fn deform_page(page: &PageRef<'_>, blk: BlockNumber, s: &Shape<'_>, out: &mut Lanes<'_>, vis: &[u16], rows: Option<&[u32]>, upto: usize) -> PgResult<()> {
    // SAFETY: the caller's contract above.
    unsafe { deform_rows(page, blk, s, out, vis, rows, upto) }
}

/// Whether `off` is a normal line pointer on the pinned, share-locked page.
pub(super) fn normal_line(buf: Buffer, off: u16) -> PgResult<bool> {
    // SAFETY: `buf` is pinned and share-locked by the caller.
    let page = unsafe { PageRef::from_raw(bufmgr_seams::buffer_page_ptr(buf)) };
    let lines = page.max_offset_number();
    if lines as usize > MaxHeapTuplesPerPage {
        return Err(err("gravity: corrupted line pointer count"));
    }
    if off > lines {
        return Ok(false);
    }
    // SAFETY: off <= lines <= MaxHeapTuplesPerPage on a locked page.
    Ok(unsafe { page.item_id_unchecked(off) }.is_normal())
}


/// The page's visible line pointers and their `page_flags`, in one walk, as heapam's `page_collect_tuples`: a page marked
/// PD_ALL_VISIBLE under a snapshot not taken during recovery is answered from its line pointers (every normal one is visible,
/// no tuple test); a serializable read (`ssi`: the relation, on the bind thread) still runs its conflict check per tuple.
/// Returns `(nvis, flags, direct)`, `direct` when no tuple went through the MVCC seam.
#[inline(always)]
pub(super) fn collect_visible(buf: Buffer, blk: BlockNumber, heap: Oid, want: usize, snapshot: &SnapshotData<'_>, vis: &mut [u16; MaxHeapTuplesPerPage], ssi: Option<&RelationData<'_>>) -> PgResult<(usize, u16, bool)> {
    // SAFETY: `buf` is pinned and share-locked by `stage`.
    let page = unsafe { PageRef::from_raw(bufmgr_seams::buffer_page_ptr(buf)) };
    let lines = page.max_offset_number();
    if lines as usize > MaxHeapTuplesPerPage {
        return Err(err("gravity: corrupted line pointer count"));
    }
    let all_visible = page.is_all_visible() && !snapshot.takenDuringRecovery;
    // SAFETY: lines <= MaxHeapTuplesPerPage on a locked page (the walk's contract).
    let (nvis, flags) = unsafe {
        if all_visible && ssi.is_none() {
            crate::ops::scan::heap_window::walk_lines(&page, lines, want, vis)?
        } else if all_visible {
            walk::<true>(&page, lines, buf, blk, heap, want, snapshot, vis, ssi)?
        } else {
            walk::<false>(&page, lines, buf, blk, heap, want, snapshot, vis, ssi)?
        }
    };
    let direct = all_visible && ssi.is_none();
    // V4R-371: the offsets ascend without repeats, so as many as there are line pointers are exactly 1..=lines
    Ok((nvis, flags | (direct as u16) * PAGE_ALL_VISIBLE | (nvis == lines as usize) as u16 * PAGE_IOTA, direct))
}

/// # Safety — `lines <= MaxHeapTuplesPerPage` on a pinned, share-locked page.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
unsafe fn walk<const ALL_VISIBLE: bool>(page: &PageRef<'_>, lines: u16, buf: Buffer, blk: BlockNumber, heap: Oid, want: usize, snapshot: &SnapshotData<'_>, vis: &mut [u16; MaxHeapTuplesPerPage], ssi: Option<&RelationData<'_>>) -> PgResult<(usize, u16)> {
    let mut memo = XidVisMemo::new();
    let (mut nvis, mut nonulls, mut full) = (0usize, true, true);
    let mut hinted_n = 0u64;
    let mut off: u16 = 1;
    // bounded("small-fixed: one page's line pointers, at most MaxHeapTuplesPerPage (291 at 8 K pages), a visibility test each")
    while off <= lines {
        let lpp = page.item_id_unchecked(off);
        if lpp.is_normal() {
            let (ptr, len) = page.item_raw_unchecked(lpp);
            let mut tup = HeapTupleData::from_raw_parts(ptr, len, ItemPointerData::new(blk, off), heap);
            // H2: the hint bits settle an old committed live tuple before the MVCC seam (M2-inventory row 3)
            let hinted = !ALL_VISIBLE && hinted_visible(tup.t_data().t_infomask, tup.t_data().xmin_raw(), snapshot.xmin);
            hinted_n += hinted as u64;
            let valid = ALL_VISIBLE || hinted || heapam_visibility_seams::heap_tuple_satisfies_mvcc_page::call(&mut tup, snapshot, buf, &mut memo)?;
            if let Some(rel) = ssi {
                HeapCheckForSerializableConflictOut(valid, rel, &mut tup, buf, snapshot)?;
            }
            if valid {
                let h = tup.t_data();
                nonulls &= h.t_infomask & HEAP_HASNULL == 0;
                full &= h.natts() as usize >= want;
                vis[nvis] = off;
                nvis += 1;
            }
        }
        off += 1;
    }
    count_hinted(hinted_n);
    Ok((nvis, (nonulls as u16) * PAGE_NONULLS | (full as u16) * PAGE_FULL))
}

