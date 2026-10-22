//! merged.rs's unsafe (lint R6), each step with its contract: the recipe its members borrow, a member's window lanes and
//! selection read through its site, a key cell's value and payload bytes, and the merge state's arena markers.
#![allow(unsafe_code)]

use gravity_abi::site::Site;
use gravity_abi::Lane;
use gravity_driver::ArenaVec;
use gravity_fn::arena::{AVec, OpArena};

use super::{Cell, Stream};
use crate::ops::scan::pgrc::granule_source::GranuleRecipe;

mcx::forget_safe_nodrop!(Cell);
mcx::forget_safe_struct!(Stream { start, end, g, row, at, member, done, own });

/// The recipe a merged source owns, lent to its members for as long as they live. The caller is that source: it drops its
/// members before its recipe (field order; `unbind` keeps them, the source's drop drops them first), and it never moves
/// after its build (the Node's arena box it lives in, which `Site.src` addresses), so the address stays valid.
pub(super) fn lent(r: &GranuleRecipe) -> &'static GranuleRecipe {
    // SAFETY: the caller's contract above: the reference outlives every member that holds it.
    unsafe { &*(r as *const GranuleRecipe) }
}

/// Member `at / lp`'s window lanes: `lanes[at..]`, a buffer the build sized once and never grows.
pub(super) fn lanes_at(lanes: &mut ArenaVec<Lane>, at: usize) -> *mut Lane {
    assert!(at < lanes.len(), "a member's lanes inside the reserved lanes");
    lanes.as_mut_ptr().wrapping_add(at)
}

/// Lane `k` of the window `site` holds (the member's own window: `k < nlanes`, checked by the callers).
pub(super) fn site_lane(site: &Site, k: usize) -> Lane {
    assert!(k < site.nlanes as usize, "lane {k} of a window of {} lanes", site.nlanes);
    // SAFETY: the site's lane array holds `nlanes` lanes its source published.
    unsafe { *site.lanes().as_ptr().add(k) }
}

/// Whether the window carries a selection.
pub(super) fn has_sel(site: &Site) -> bool {
    !gravity_abi::layout::unword(site.sel).is_null()
}

/// The window's selection (`sel_n` granule rows, ascending).
pub(super) fn sel(site: &Site) -> &[u32] {
    let p = gravity_abi::layout::unword(site.sel);
    if p.is_null() {
        return &[];
    }
    // SAFETY: a published selection is `sel_n` u32s its source holds until its next window or seek.
    unsafe { core::slice::from_raw_parts(p, site.sel_n as usize) }
}

/// The granule row behind index `at` of the window (`at < end()`).
pub(super) fn row_at(site: &Site, at: u32) -> u32 {
    if has_sel(site) {
        sel(site)[at as usize]
    } else {
        at
    }
}

/// The first index of the window whose row is `from` or after (`end()` when none is).
pub(super) fn first_at_or_after(site: &Site, from: u32) -> u32 {
    if has_sel(site) {
        sel(site).partition_point(|&r| r < from) as u32
    } else {
        from.min(site.rows)
    }
}

/// Row `row`'s key cell of lane `l` (a by-value integer lane, sign-extended, or a by-reference lane's payload); `None` for a
/// by-reference image that is neither a plain nor a short-header varlena (a granule source decodes neither form).
pub(super) fn cell_at(l: &Lane, row: usize, byref: bool) -> Option<Cell> {
    // SAFETY: `row` is a row of the window that published `l` (an index the window's rows or selection gave), and `l` is
    // one of its decoded lanes (`check_window`: a non-null pointer).
    unsafe {
        if !l.is_valid(row) {
            return Some(Cell::NULL);
        }
        let w = l.datum(row);
        if !byref {
            return Some(Cell { null: false, w, p: core::ptr::null(), n: 0 });
        }
        // the by-reference cell is a varlena image in the source's arena: a short (1-byte) or a plain 4-byte header
        let p = w as *const u8;
        let b0 = *p;
        if b0 & 0x01 == 0x01 && b0 != 0x01 {
            return Some(Cell { null: false, w: 0, p: p.add(1), n: ((b0 >> 1) as u32).saturating_sub(1) });
        }
        if b0 & 0x03 == 0 {
            let h = (p as *const u32).read_unaligned();
            return Some(Cell { null: false, w: 0, p: p.add(4), n: (h >> 2).saturating_sub(4) });
        }
        None
    }
}

/// `n` bytes at `p`: a key cell's payload (in a live window's arena, or a stream's own copy in the operator's arena).
pub(super) fn bytes<'a>(p: *const u8, n: u32) -> &'a [u8] {
    if n == 0 {
        return &[];
    }
    // SAFETY: `(p, n)` is a cell `cell_at` read from a live window or `evict` repointed into the stream's own bytes, both
    // alive while the cell is a head (a window until its member moves, which repoints or replaces the cell first).
    unsafe { core::slice::from_raw_parts(p, n as usize) }
}

/// The address of byte `at` of a stream's own key bytes (its buffer stays put until the stream's next eviction).
pub(super) fn own_at(own: &AVec<u8>, arena: &OpArena, at: usize) -> *const u8 {
    own.as_slice(arena).as_ptr().wrapping_add(at)
}
