//! M2's heap page-window entries (cards/M2.md L1, road `heap.pages`): the IR's page protocol over a raw-page heap site
//! (`SiteDesc::raw_pages`; hosts `heap_source.rs` `fill_raw` / `lane_pg`, hosts v4/m2_heap.rs). One window is one pinned
//! page of the claim: the page pointer, its line-pointer count, the page flags and the visible offsets in the site's
//! offset lane (ascending line-pointer order). The walk's direction is the IR's: last to first by default (Decision 11:
//! tuples sit at ascending addresses), first to last where a lowering-time `OrderUse` fact asks for physical order; both
//! read the same offsets. Created by M2 (owner M2-HOSTS); the entries are the users' (`source/heap_pages.rs`) `hosts:` rows.
use gravity_abi::site::{self, site::PAGE_FULL, site::PAGE_NONULLS};

/// The page-window entries: ops/scan/raw.rs (`host!`).
pub use super::raw::{heap_page_lane, heap_page_next};

/// A page flag beside `PAGE_NONULLS` / `PAGE_FULL`: the page is PD_ALL_VISIBLE under a snapshot not taken in recovery and
/// no serializable check ran, so every LP_NORMAL line pointer in `1..=nlp` is visible (the IR may walk them in place).
pub const PAGE_ALL_VISIBLE: u16 = 4;
const _: () = assert!(PAGE_ALL_VISIBLE & (PAGE_FULL | PAGE_NONULLS) == 0);

/// `heap_page_lane(site, k | LANE_LATE)` (late decode, M2-inventory row 8): lane k deformed only to its own attribute, over
/// the site's selection when it selects fewer rows than the window has; a plain `k` deforms every wanted attribute of every
/// row. On a raw-page heap site the bit rides where a program table's `LANE_DONE` does (no heap site has one).
pub const LANE_LATE: u32 = site::LANE_DONE;

/// The page window `heap_page_next` writes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PageWin {
    /// The pinned page (valid until the site's next window: `Until::Window`).
    pub page: u64,
    /// The page's line-pointer count (`PageGetMaxOffsetNumber`); the visible ones are the site's `rows` offsets.
    pub nlp: u32,
    /// `PAGE_NONULLS` | `PAGE_FULL` over the visible tuples, | `PAGE_ALL_VISIBLE`.
    pub flags: u32,
    /// The page's block number.
    pub block: u32,
    /// The pinned buffer (the site's `granule` word of a raw-page window; 0 = not published).
    pub buf: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::scan::raw::page_win;
    use gravity_abi::ctx::site_ptr;
    use gravity_abi::site::Site;
    use gravity_abi::status::ERROR;
    use gravity_abi::Ctx;

    #[test]
    fn page_win_layout() {
        assert_eq!((core::mem::size_of::<PageWin>(), core::mem::align_of::<PageWin>()), (24, 8));
        assert_eq!((core::mem::offset_of!(PageWin, nlp), core::mem::offset_of!(PageWin, flags), core::mem::offset_of!(PageWin, block), core::mem::offset_of!(PageWin, buf)), (8, 12, 16, 20));
    }

    #[test]
    fn page_win_reads_the_line_pointer_count_from_pd_lower() {
        let mut page = [0u8; 64];
        page[12..14].copy_from_slice(&(24u16 + 4 * 7).to_ne_bytes());
        let mut lanes = [gravity_abi::Lane::NONE; 1];
        let mut s = zeroed_site(lanes.as_mut_ptr());
        s.page = page.as_ptr();
        s.page_flags = PAGE_FULL | PAGE_ALL_VISIBLE;
        s.pos.block = 9;
        s.granule = 3;
        let w = unsafe { page_win(&s) };
        assert_eq!(w, PageWin { page: page.as_ptr() as u64, nlp: 7, flags: (PAGE_FULL | PAGE_ALL_VISIBLE) as u32, block: 9, buf: 3 });
        s.page = core::ptr::null();
        assert_eq!(unsafe { page_win(&s) }.nlp, 0, "no page, no line pointers");
    }

    /// A zeroed site over `lanes` (the blank site of the crates above this one).
    fn zeroed_site(lanes: *mut gravity_abi::Lane) -> Site {
        // SAFETY: `Site` is a plain `repr(C)` block of pointers and integers; all-zero is its closed state.
        let mut s: Site = unsafe { core::mem::zeroed() };
        s.lanes = lanes;
        s
    }

    static NEXTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    /// A source's `next` staging a 4-row page at block 11.
    unsafe extern "C" fn next_page(s: *mut Site) -> i64 {
        NEXTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        (*s).rows = 4;
        (*s).cursor = 0;
        (*s).pos.block = 11;
        4
    }

    /// D389: a stop inside the page resumes there (the rows left, no new page); a spent page stages the next one.
    #[test]
    fn heap_page_next_resumes_inside_the_page() {
        let mut buf = vec![0u64; gravity_abi::ctx::ctx_size(1) / 8]; // alloc: Cold
        let ctx = buf.as_mut_ptr() as *mut Ctx;
        let mut lanes = [gravity_abi::Lane::NONE; 1];
        let mut site = zeroed_site(lanes.as_mut_ptr());
        let mut w = PageWin::default();
        unsafe {
            (*ctx).sites = &mut site;
            let s = site_ptr(ctx, 0);
            (*s).next = Some(next_page);
            (*s).rows = 5;
            (*s).cursor = 2;
            assert_eq!(heap_page_next(ctx as u64, 0, &mut w), 3, "the page's rows left");
            assert_eq!(NEXTS.load(std::sync::atomic::Ordering::Relaxed), 0, "no page staged over a live one");
            (*s).cursor = 5;
            assert_eq!(heap_page_next(ctx as u64, 0, &mut w), 4);
            assert_eq!((NEXTS.load(std::sync::atomic::Ordering::Relaxed), w.block), (1, 11), "the next page, once");
        }
    }

    #[test]
    fn a_panicking_body_is_parked_not_unwound() {
        let r: u64 = guard!(ERROR as u64, {
            let v = [7u64; 1];
            v[std::hint::black_box(1)]
        });
        assert_eq!(r, ERROR as u64);
        assert!(std::panic::catch_unwind(gravity_abi::status::unwind::resume).is_err(), "the parked payload resumes at the caller");
    }
}
