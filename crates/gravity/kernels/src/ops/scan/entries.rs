//! M2's host entries (M2-plan.md §6, cards/M2.md L1): the C-ABI functions M2's lowered IR calls beyond host_abi.rs's base
//! utilities (`seek`, `range_seek`, `advance`, `lane_fill` stay the generic entries), and the layouts M2's sources hand
//! the IR. Each entry is listed in the `hosts:` rows of the lowerv4 file that calls it; the heap page window's entries
//! and layout are m2_heap.rs's. Every M2 entry runs its body under `guard!` (host_abi.rs, `contract::pad`) from the start (V4R-16's rule): a bug's
//! panic never crosses the `extern "C"` frame. Created by M2 (owner M2-HOSTS).

/// A row's heap position as the word of an N-row index-family window's RowId lane (index-only batches, the TID source):
/// block in the high bits, line pointer offset in the low 16. A site that declares a RowId lane gets N-row windows with
/// the lane filled; a site without one keeps one row per window, its position the window's (M1's index form).
pub const fn tid_word(block: u32, off: u16) -> u64 {
    (block as u64) << 16 | off as u64
}

/// `tid_word`'s inverse: `(block, off)`.
pub const fn tid_of_word(w: u64) -> (u32, u16) {
    ((w >> 16) as u32, w as u16)
}

pub use crate::ops::scan::entries_heap::{heap_page_lane, heap_page_next, PageWin, LANE_LATE, PAGE_ALL_VISIBLE};
/// The session zone offset entry: ops/scan/raw.rs (`host!`).
pub use crate::ops::scan::raw::tz_fixed_offset;
pub use crate::ops::scan::raw::tz_offset_at;
pub use crate::ops::scan::raw::{zone_cross_bound, CROSS_DATE_TS, CROSS_DATE_TSTZ, CROSS_NONE, CROSS_TSTZ_DATE, CROSS_TS_DATE};

