//! heap_window's unsafe (lint R6; construct bundle scan-05; its raw.rs child): the line-pointer walk of an all-visible
//! page and the rows' deform, each with its safety contract, moved as is from heap_window.rs (re-exported there, so
//! their paths are unchanged).
#![allow(unsafe_code)]
use super::*;
use crate::ops::shared::heap_fetch::is_sys;

/// The all-visible page without a serializable check: every normal line pointer is visible. A page whose line pointers
/// are all normal and in bounds (the common one) is answered in one branch-free pass over them: `vis` gets `1..=lines` and
/// no tuple is touched (V4R-371, D1001): `PAGE_UNREAD` when the recipe wants attributes (the IR tests each row's header
/// word), `PAGE_NONULLS | PAGE_FULL` at `want == 0` (H3). Any other page takes the walk: the line pointers last to first
/// and, when the recipe wants attributes, the two header halfwords the page flags need (`t_infomask2` for natts,
/// `t_infomask` for HEAP_HASNULL). `vis` gets the visible offsets ascending.
/// # Safety
/// `lines <= MaxHeapTuplesPerPage` on a pinned, share-locked page.
pub unsafe fn walk_lines(page: &PageRef<'_>, lines: u16, want: usize, vis: &mut [u16; MaxHeapTuplesPerPage]) -> PgResult<(usize, u16)> {
    let base = page.as_ptr();
    let lps = base.add(SizeOfPageHeaderData) as *const u32;
    let lp = |i: usize| core::ptr::read_unaligned(lps.add(i));
    if (0..lines as usize).fold(0, |bad, i| bad | lp_bad(lp(i))) == 0 {
        vis.iter_mut().take(lines as usize).zip(1..).for_each(|(v, off)| *v = off);
        return Ok((lines as usize, if want > 0 { PAGE_UNREAD } else { PAGE_NONULLS | PAGE_FULL }));
    }
    let (mut nvis, mut nonulls, mut full) = (0usize, true, true);
    // bounded("small-fixed: one page's line pointers, at most MaxHeapTuplesPerPage (291 at 8 K pages), a header test each")
    for off in (1..=lines).rev() {
        let lpp = lp(off as usize - 1);
        if (lpp >> 15) & 3 != 1 {
            continue;
        }
        if want > 0 {
            if lp_bad(lpp) != 0 {
                return Err(crate::pg_err("gravity: corrupted line pointer"));
            }
            let toff = (lpp & 0x7fff) as usize;
            let infomask2 = core::ptr::read_unaligned(base.add(toff + 18) as *const u16);
            let infomask = core::ptr::read_unaligned(base.add(toff + 20) as *const u16);
            nonulls &= infomask & HEAP_HASNULL == 0;
            full &= (infomask2 & HEAP_NATTS_MASK) as usize >= want;
        }
        vis[nvis] = off;
        nvis += 1;
    }
    vis[..nvis].reverse();
    Ok((nvis, (nonulls as u16) * PAGE_NONULLS | (full as u16) * PAGE_FULL))
}


/// The page's rows `rows` (indexes into `vis`; `None` = every one) deformed up to attribute `upto`: every lane whose
/// attribute is at most `upto` gets those rows' cells and validity bits (a prefix deform yields them together). A row
/// index outside `vis` is refused (the IR's selection names rows of this window only).
/// # Safety
/// `page` is pinned; each `vis` offset is a normal line pointer of it.
pub unsafe fn deform_rows(page: &PageRef<'_>, blk: BlockNumber, s: &Shape<'_>, out: &mut Lanes<'_>, vis: &[u16], rows: Option<&[u32]>, upto: usize) -> PgResult<()> {
    let n = rows.map_or(vis.len(), |r| r.len());
    // bounded("small-fixed: the rows of one page's window, at most PAGE_ROWS (MaxHeapTuplesPerPage, 291 at 8 K pages), a tuple deform each")
    for j in 0..n {
        let r = rows.map_or(j, |sel| sel[j] as usize);
        let Some(&off) = vis.get(r) else { return Err(crate::pg_err("gravity: a heap lane request names a row outside its window")) };
        let lpp = page.item_id_unchecked(off);
        let (ptr, len) = page.item_raw_unchecked(lpp);
        let tup = HeapTupleData::from_raw_parts(ptr, len, ItemPointerData::new(blk, off), s.heap);
        heap_deform_tuple_prefix_atts(&tup, &s.atts[..upto], &mut out.vals[..upto], &mut out.nulls[..upto], upto);
        // bounded("key-columns: one missing default per column the tuple predates, at most MaxTupleAttributeNumber (1664) columns")
        for i in (tup.t_data().natts() as usize).min(upto)..upto {
            if let (true, v) = s.missing[i] {
                out.vals[i] = datum::Datum::from_u64(v);
                out.nulls[i] = false;
            }
        }
        // bounded("key-columns: one cell per output column of a row, at most MaxTupleAttributeNumber (1664) columns")
        for (k, &a) in s.attnos.iter().enumerate() {
            let (word, valid) = if is_sys(a) {
                // V4R-88: C `heap_getsysattr` (xmin / xmax raw, cmin / cmax the raw command id), never null
                let mut null = false;
                (types_tuple::getattr::heap_getsysattr(&tup, (SYS_AT_BASE - a) as i32, &mut null).as_u64(), true)
            } else if a as usize <= upto {
                let i = a as usize - 1;
                (out.vals[i].as_u64(), !out.nulls[i])
            } else {
                continue;
            };
            out.lanes[k * PAGE_ROWS + r] = word;
            let w = &mut out.valid[k * PAGE_VALIDITY_WORDS + r / 64];
            *w = (*w & !(1 << (r % 64))) | (valid as u64) << (r % 64);
        }
    }
    Ok(())
}

