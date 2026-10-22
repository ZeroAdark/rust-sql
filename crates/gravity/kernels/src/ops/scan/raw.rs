//! The scan family's raw half in kernels (rules R6 / R8): the IR host entries of the scan family,
//! written through the contract's `host!` with their landing pad as `#[guard]` (the expansion of the
//! `guard!(FAIL, body)` they ran under), and the page-window read they share, moved as is from entries.rs and
//! entries_heap.rs. Those files re-export the entries under their old paths, so the lowerers' addresses and the IR's host
//! names are unchanged.
use gravity_abi::PtrWord;
use gravity_abi::ctx::site_ptr;
use gravity_abi::site::{self, Site};
use gravity_abi::status::ERROR;
use gravity_abi::Ctx;

use super::entries_heap::PageWin;

crate::contract::host! {
    /// `tz_fixed_offset(ctx)`: the session zone's constant UTC offset in seconds (east positive) as an i64 word, or `i64::MIN`
    /// when the zone has daylight-saving rules or no zone is set (PostgreSQL's `pg_get_timezone_offset`). Read per call, so a
    /// `SET TimeZone` between two executions of one program is seen; expr.fusion folds a timestamptz step through it only when
    /// the word is not `i64::MIN`.
    /// # Safety
    /// `ctx` is unused (the entry reads session state only); any word is accepted.
    #[guard(i64::MIN as u64)]
    pub unsafe fn tz_fixed_offset(_ctx: u64) -> u64 {
        gravity_fn::session_fixed_offset().unwrap_or(i64::MIN) as u64
    }

    /// `tz_offset_at(ctx, ts)`: the session zone's UTC offset in seconds (east positive) at timestamptz word `ts`, as
    /// PostgreSQL's `timestamp2tm` reads it (`pg_localtime`'s `tm_gmtoff`), or `i64::MIN` when no zone is set, the instant
    /// is out of `pg_localtime`'s range, or its local fields are not the instant shifted by that offset (a leap second):
    /// expr.fusion's extract word under a zone with DST open-codes the field over `ts + offset`, else takes the hosted call.
    /// # Safety
    /// `ctx` is unused (the entry reads session state only); any word is accepted.
    #[guard(i64::MIN as u64)]
    pub unsafe fn tz_offset_at(_ctx: u64, ts: u64) -> u64 {
        gravity_fn::session_offset_at(ts as i64).unwrap_or(i64::MIN) as u64
    }

    /// `heap_pages`: the current page while rows of it are left (a stop inside the page resumes there), else the claim's next
    /// page with at least one visible tuple, pinned, into `out` — `util::advance`'s window protocol, its interrupt check once
    /// per window included (D389: a bare `site::next` skipped the rest of a page after a stop inside it). Returns the rows left
    /// in the window (>= 1), 0 at the claim's end (`out` untouched), or the site's failure status (below zero, as a u64).
    /// # Safety
    /// `ctx` is bound with a raw-page heap site `site`; `out` points at a writable `PageWin`.
    #[guard(ERROR as u64)]
    pub unsafe fn heap_page_next(ctx: u64, site: u64, out: *mut PageWin) -> u64 {
        let rc = crate::util::advance(ctx as *mut Ctx, site as usize);
        if rc > 0 {
            *out = page_win(&*site_ptr(ctx as *mut Ctx, site as usize));
        }
        rc as u64
    }

    /// `heap_pages`' slow page (natts / HASNULL / t_hoff not proven): lane `lane` of the current page through the host's general
    /// deform, for the site's selection when it has one (late decode: the filter lanes first over every row, the rest for the
    /// survivors only). Returns the rows published, or the site's failure status (below zero, as a u64).
    /// # Safety
    /// `ctx` is bound with a raw-page heap site `site` holding a current window.
    #[guard(ERROR as u64)]
    pub unsafe fn heap_page_lane(ctx: u64, site: u64, lane: u64) -> u64 {
        site::lane_fill(site_ptr(ctx as *mut Ctx, site as usize), lane as u32) as u64
    }
}

/// D2992 (lowerv4 verdict_zone.rs `cross_of`): the cross-unit datetime comparisons `zone_cross_bound` converts, as (lane
/// type, comparand type).
pub const CROSS_DATE_TS: u8 = 1;
pub const CROSS_DATE_TSTZ: u8 = 2;
pub const CROSS_TS_DATE: u8 = 3;
pub const CROSS_TSTZ_DATE: u8 = 4;
/// `zone_cross_bound`'s word when no bound holds (no predicate then). A bound of `i64::MIN` (timestamp -infinity) reads
/// so too, which only forgoes a skip.
pub const CROSS_NONE: u64 = i64::MIN as u64;

crate::contract::host! {
    /// `zone_cross_bound(kind, op, v)`: comparand word `v` of a cross-unit comparison `lane op v` of kind `kind`
    /// (`CROSS_*`) as a bound in the lane's units, exact under PostgreSQL's own comparators (`date_cmp_timestamp_internal`,
    /// `date_cmp_timestamptz_internal` under the session TimeZone): for `>=` / `>` the least lane value that passes (read
    /// as `lane >= bound`), for `<=` / `<` the greatest (`lane <= bound`), for `=` the one value that passes. `op` is a
    /// `ZoneOp` discriminant (Lt 0, Le 1, Eq 2, Ge 3, Gt 4). `CROSS_NONE` when no lane value passes, for `=` unless exactly
    /// one does, and for any other operator.
    /// # Safety
    /// Reads session state only; any words are accepted.
    #[guard(CROSS_NONE)]
    pub unsafe fn zone_cross_bound(kind: u64, op: u64, v: u64) -> u64 {
        cross_bound(kind as u8, op as u8, v)
    }
}

/// The finite dates, `[MIN_DATE, END_DATE)` in days from 2000-01-01; a stored date is one of them or an infinity
/// (`i32::MIN`, `i32::MAX`).
const MIN_DATE: i64 = -2_451_545;
const END_DATE: i64 = 2_145_031_949;

/// `zone_cross_bound`'s body. The comparison is monotone in the lane value (a date maps to its midnight, a timestamp is
/// itself; a date past the timestamp range and the infinities keep their order), so a binary search over the lane's
/// domain finds the bound, its edge cases by construction: a date lane's finite dates with the infinities at either
/// end (no invalid date is ever converted), a timestamp lane's whole i64 (never converted).
fn cross_bound(kind: u8, op: u8, v: u64) -> u64 {
    use gravity_fn::{date_cmp_timestamp_internal as dts, date_cmp_timestamptz_internal as dtz};
    let date_lane = match kind {
        CROSS_DATE_TS | CROSS_DATE_TSTZ => true,
        CROSS_TS_DATE | CROSS_TSTZ_DATE => false,
        _ => return CROSS_NONE,
    };
    // lane value x against the comparand, the lane on the left
    let cmp = |x: i64| match kind {
        CROSS_DATE_TS => dts(x as i32, v as i64),
        CROSS_DATE_TSTZ => dtz(x as i32, v as i64),
        CROSS_TS_DATE => -dts(v as i32, x),
        _ => -dtz(v as i32, x),
    };
    // the domain in order: its least and greatest values, and the finite span searched between them
    let (first, last, lo, hi) = if date_lane { (i32::MIN as i64, i32::MAX as i64, MIN_DATE, END_DATE - 1) } else { (i64::MIN, i64::MAX, i64::MIN, i64::MAX) };
    // the least value whose `pass` holds (false then true over the domain)
    let least = |pass: &dyn Fn(i64) -> bool| {
        if pass(first) {
            return Some(first);
        }
        if !pass(hi) {
            return pass(last).then_some(last);
        }
        let (mut a, mut b) = (lo, hi);
        while a < b {
            let m = ((a as i128 + b as i128) >> 1) as i64;
            if pass(m) {
                b = m;
            } else {
                a = m + 1;
            }
        }
        Some(a)
    };
    // the greatest value whose `pass` holds (true then false over the domain)
    let greatest = |pass: &dyn Fn(i64) -> bool| {
        if pass(last) {
            return Some(last);
        }
        if !pass(lo) {
            return pass(first).then_some(first);
        }
        let (mut a, mut b) = (lo, hi);
        while a < b {
            let m = ((a as i128 + b as i128 + 1) >> 1) as i64;
            if pass(m) {
                a = m;
            } else {
                b = m - 1;
            }
        }
        Some(a)
    };
    let bound = match op {
        0 => greatest(&|x| cmp(x) < 0),
        1 => greatest(&|x| cmp(x) <= 0),
        2 => least(&|x| cmp(x) >= 0).filter(|&a| greatest(&|x| cmp(x) <= 0) == Some(a)),
        3 => least(&|x| cmp(x) >= 0),
        4 => least(&|x| cmp(x) > 0),
        _ => None,
    };
    bound.map_or(CROSS_NONE, |x| x as u64)
}

/// The window's `PageWin`: the line-pointer count read from `pd_lower` (the page header's u16 at 12, 24 header bytes,
/// 4 per line pointer).
pub(super) unsafe fn page_win(s: &Site) -> PageWin {
    let lower = if s.page.ptr().is_null() { 0 } else { core::ptr::read_unaligned(s.page.ptr().add(12) as *const u16) as u32 };
    PageWin { page: s.page.ptr() as u64, nlp: lower.saturating_sub(24) / 4, flags: s.page_flags as u32, block: s.pos.block, buf: s.granule }
}
