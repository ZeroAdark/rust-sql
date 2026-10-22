//! M2's heap page-window host extras (cards/M2.md L3 `m2_heap.rs`, M2-inventory rows 3, 8, 10): the page walks and the
//! deform `heap_source.rs`'s range source runs for road `heap.pages` (kernels host_abi v4 m2_heap.rs: `heap_page_next`,
//! `heap_page_lane`).
//! - H2 (row 3): the hint-bit test `FAST_MASK` before the MVCC seam on a page that is not all-visible.
//! - H3 (row 10): the all-visible walk reads no tuple header when the recipe wants no attribute (count-only).
//! - V4R-371 (D1001): an all-visible page of normal line pointers is answered from them alone (`PAGE_UNREAD`), whatever the recipe.
//! - I8 (row 8): late decode. A lane asked with `LANE_LATE` is deformed to its own attribute only, over the site's selection
//!   when it has one: the filter lanes over every row, the rest for the survivors.
//! The header-reading walk goes last to first (Decision 11: tuples sit at ascending addresses) and publishes the offsets
//! ascending, PostgreSQL's order; the IR picks its own direction over them. No heap condition cache (the user 09-24).
//! - H03a (row 15, road `heap.stream`, V4R-32): the read-stream look-ahead of cold claims (`ReadAhead`).
//! Created by M2 (owner M2-HOSTS; the read-ahead M2-HEAP, Owner-Ack main 09-24).
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Weak};

use gravity_abi::site::site::{PAGE_FULL, PAGE_NONULLS, PAGE_UNREAD};
use gravity_program::protos::SYS_AT_BASE;
use pgsync::Mutex;
use types_core::{BlockNumber, Oid, TransactionId, TransactionIdPrecedes, BLCKSZ};
use types_error::PgResult;
use types_storage::RelFileLocator;
use types_storage::bufpage::{MaxHeapTuplesPerPage, PageRef, SizeOfPageHeaderData};
use types_tuple::tupdesc::CompactAttribute;
use types_tuple::{heap_deform_tuple_prefix_atts, HeapTupleData, ItemPointerData, HEAP_HASNULL, HEAP_NATTS_MASK, HEAP_XMAX_INVALID, HEAP_XMIN_COMMITTED, HEAP_XMIN_FROZEN};

pub use gravity_kernels::ops::scan::entries::{LANE_LATE, PAGE_ALL_VISIBLE};

mod raw;
pub use raw::{deform_rows, walk_lines};

const _: () = assert!(PAGE_UNREAD & (PAGE_FULL | PAGE_NONULLS | PAGE_ALL_VISIBLE) == 0);

/// A lane's rows on one page (a page holds at most `MaxHeapTuplesPerPage` tuples) and its validity words.
pub const PAGE_ROWS: usize = MaxHeapTuplesPerPage;
pub const PAGE_VALIDITY_WORDS: usize = PAGE_ROWS.div_ceil(64);

/// Hint bits that settle visibility without pg_xact once xmin is older than every transaction the snapshot sees running:
/// xmin committed (or frozen) and xmax invalid (lab `heap.rs` `FAST_MASK`).
pub const FAST_MASK: u16 = HEAP_XMIN_COMMITTED | HEAP_XMAX_INVALID;

/// Tuples whose visibility the hint bits settled (`hinted_visible`), process-wide, bumped once per page (a census for tests
/// and EXPLAIN-free diagnosis, as heap_source.rs's `page_census`).
static HINTED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn hinted_census() -> u64 {
    HINTED.load(core::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn count_hinted(n: u64) {
    if n > 0 {
        HINTED.fetch_add(n, core::sync::atomic::Ordering::Relaxed);
    }
}

/// HeapTupleSatisfiesMVCC's answer from the hint bits alone, when they decide it: xmin committed (hinted) and xmax invalid
/// (hinted) is visible when xmin is frozen or precedes the snapshot's xmin (no running transaction of the snapshot can be
/// xmin, `XidInMVCCSnapshot`'s first test). `false` = not decided here: the caller asks the MVCC seam.
#[inline(always)]
pub fn hinted_visible(infomask: u16, xmin: TransactionId, snapshot_xmin: TransactionId) -> bool {
    infomask & FAST_MASK == FAST_MASK && (infomask & HEAP_XMIN_FROZEN == HEAP_XMIN_FROZEN || TransactionIdPrecedes(xmin, snapshot_xmin))
}

/// A line pointer the single pass does not answer (1): not LP_NORMAL, or its tuple outside the page (the walk's checks).
#[inline(always)]
fn lp_bad(lpp: u32) -> u32 {
    let (toff, tlen) = (lpp & 0x7fff, (lpp >> 17) & 0x7fff);
    ((lpp >> 15) & 3 != 1) as u32 | (toff < SizeOfPageHeaderData as u32) as u32 | (tlen < types_tuple::SizeofHeapTupleHeader as u32) as u32 | (toff + tlen > BLCKSZ as u32) as u32
}

/// What the deform reads: the recipe's shape.
pub struct Shape<'a> {
    pub heap: Oid,
    /// The relation's first `want` attributes (`attcacheoff` cleared).
    pub atts: &'a [CompactAttribute],
    /// Per attribute `< want`: `(true, word)` = its fast default (`attmissingval`): the value, or a by-reference one's image
    /// pointer (the recipe owns the image).
    pub missing: &'a [(bool, u64)],
    /// Lane `k`'s attribute number.
    pub attnos: &'a [i16],
}

/// Where the deform writes: lane `k` row `r` at `lanes[k * PAGE_ROWS + r]`, its bit in `valid[k * PAGE_VALIDITY_WORDS..]`;
/// `vals` / `nulls` one tuple's cells (at least `want`).
pub struct Lanes<'a> {
    pub lanes: &'a mut [u64],
    pub valid: &'a mut [u64],
    pub vals: &'a mut [datum::Datum],
    pub nulls: &'a mut [bool],
}

/// A lane request's reach (`heap_source.rs` `lane_pg`): the attribute to deform to and the rows. A plain request deforms
/// every attribute the recipe wants over every row (one pass, the rest of the page's lanes with it); a `LANE_LATE`
/// request deforms only to its own attribute, over the site's selection when it selects fewer rows than the window has.
pub fn reach<'s>(late: bool, attno: usize, want: usize, sel: &'s [u32], nvis: usize) -> (usize, Option<&'s [u32]>) {
    match late {
        true => (attno, (!sel.is_empty() && sel.len() < nvis).then_some(sel)),
        false => (want, None),
    }
}

// ------------------------------------------------------------------------------------ heap.stream (H03a, V4R-32)

/// The read-stream look-ahead of one relation for one statement (road `heap.stream`; the kis kernel's `read_ahead`,
/// fb2930c8604 heap.rs:141-166): a combined read that missed shared buffers advises the blocks past it, one window shared
/// by the statement's workers, never a block twice, in calls of the device's I/O size (`Limits::piece`). The advice is the
/// STORAGE seam's, which applies PostgreSQL's rule (io_method = sync, effective_io_concurrency > 0, buffered data I/O) and
/// clips at the fork's end. Four rules keep one statement from taking the device or the page cache (the user 09-24):
/// 1. Ramp: the window starts at one call and doubles only when the scan has consumed a quarter of it (a refill), as the
///    kernel's readahead grows; a LIMIT, an early exit or a cancel stops reading, and advice is only ever issued from a read,
///    so it stops advising, at most one window past its last read; the statement's end drops the window.
/// 2. Bound: the window grows to `Limits::max`, the smallest of the device's concurrency (calls x
///    min(effective_io_concurrency, the queue's `nr_requests`)) and one backend's fair share of shared buffers and of the
///    page cache (NBuffers / max_connections: PostgreSQL's per-backend pin limit; MemAvailable / max_connections).
/// 3. Engage: only a relation larger than `Limits::engage` (the bounded window: a relation within one window is read in about
///    one window's I/O, which the kernel's own readahead already covers) and only the range source's scan (an index-driven
///    heap fetch never takes a window).
/// 4. Share: the blocks advised ahead of every window's last read, process-wide, stay within `Limits::budget` (the device's
///    queue: calls x nr_requests, and at most a quarter of shared buffers, heapam's bulk-read threshold), so N concurrent
///    scans split the device instead of each taking a full window.
pub struct ReadAhead {
    locator: RelFileLocator,
    nblocks: BlockNumber,
    lim: Limits,
    st: Mutex<Stream>,
}

/// A window's state: the furthest block advised, the current (ramping) window, and the blocks it holds of the budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stream {
    /// The read the last advice followed (`NONE_YET` before the first).
    pub last: BlockNumber,
    pub advised: BlockNumber,
    pub window: BlockNumber,
    pub held: BlockNumber,
}

/// The derived sizes, in blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub piece: BlockNumber,
    pub max: BlockNumber,
    pub engage: BlockNumber,
    pub budget: BlockNumber,
}

/// `Stream::last` before the first advice.
pub const NONE_YET: BlockNumber = BlockNumber::MAX;

/// Rule 4: the blocks advised ahead of their windows' last reads, process-wide.
static INFLIGHT: AtomicU32 = AtomicU32::new(0);

type Windows = HashMap<RelFileLocator, Weak<ReadAhead>>;
static WINDOWS: Mutex<Option<Windows>> = Mutex::new(None);

/// The statement's window over `locator`: the leader's bind (`leader`) opens a fresh one (a new statement: nothing advised
/// yet), a worker's bind takes the one its leader opened. `None` when the look-ahead is off or the relation is small (rule 3).
pub fn read_ahead(locator: RelFileLocator, nblocks: BlockNumber, leader: bool) -> Option<Arc<ReadAhead>> {
    let lim = limits(crate::inputs::effective_io_concurrency() as BlockNumber)?;
    if nblocks <= lim.engage {
        return None;
    }
    let mut g = WINDOWS.lock().unwrap_or_else(|p| p.into_inner());
    let t = g.get_or_insert_with(HashMap::new);
    t.retain(|_, w| w.strong_count() > 0);
    if let (false, Some(w)) = (leader, t.get(&locator).and_then(Weak::upgrade)) {
        return Some(w);
    }
    let w = Arc::new(ReadAhead { locator, nblocks, lim, st: Mutex::new(Stream { last: NONE_YET, advised: 0, window: lim.piece, held: 0 }) }); // alloc: Exec
    t.insert(locator, Arc::downgrade(&w));
    Some(w)
}

impl ReadAhead {
    /// After a combined read that missed shared buffers, the read starting at `from`: advise past it (`step`).
    pub fn advise(&self, from: BlockNumber) -> PgResult<()> {
        let (lo, hi) = {
            let mut st = self.st.lock().unwrap_or_else(|p| p.into_inner());
            let Some((next, lo, hi)) = step(*st, from, self.nblocks, &self.lim, &INFLIGHT) else { return Ok(()) };
            *st = next;
            (lo, hi)
        };
        let mut b = lo;
        // bounded("other: one prefetch call per Limits::piece blocks of the range just advised, which is at most the stream's window (Limits::max blocks)")
        while b < hi {
            let n = (hi - b).min(self.lim.piece);
            if !bufmgr_seams::prefetch_range_without_relcache::call(self.locator, b, n)? {
                return Ok(()); // PostgreSQL's rule advises nothing here (or the fork is gone): no further call will
            }
            b += n;
        }
        Ok(())
    }
}

impl Drop for ReadAhead {
    /// The statement's end (every bind of it dropped): its blocks leave the budget.
    fn drop(&mut self) {
        let held = self.st.get_mut().map_or_else(|p| p.into_inner().held, |s| s.held);
        INFLIGHT.fetch_sub(held, Ordering::Relaxed);
    }
}

/// One advice decision after a read from `from`: the stream's next state and the blocks `[lo, hi)` to advise, or `None`.
/// The first read advises one call; after that the scan must have read a quarter of the window past the read the last
/// advice followed before the window `[from, from + window)` is refilled (less what is advised already), and each refill
/// doubles the next window up to `max` (rule 1): a scan that stops reading stops the ramp. The budget (`inflight`, rule 4)
/// is re-taken for the blocks now ahead of `from` and clips `hi`.
pub fn step(st: Stream, from: BlockNumber, nblocks: BlockNumber, lim: &Limits, inflight: &AtomicU32) -> Option<(Stream, BlockNumber, BlockNumber)> {
    let want = from.saturating_add(st.window).min(nblocks);
    let progressed = st.last == NONE_YET || from >= st.last.saturating_add((st.window / 4).max(1));
    if want <= st.advised || !progressed {
        return None;
    }
    // rule 4: this stream now holds the blocks between its read and its advice; give back what the scan consumed, take what
    // the refill adds, as far as the budget has room
    let lo = st.advised.max(from);
    let keep = st.advised.saturating_sub(from);
    let mut granted = 0;
    let _ = inflight.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
        let others = cur.saturating_sub(st.held);
        granted = lim.budget.saturating_sub(others + keep).min(want - lo);
        Some(others + keep + granted)
    });
    let hi = lo + granted;
    let next = Stream { last: from, advised: st.advised.max(hi), window: (st.window * 2).min(lim.max), held: keep + granted };
    if hi <= lo {
        return Some((Stream { window: st.window, ..next }, lo, lo));
    }
    Some((next, lo, hi))
}

// the device and the page cache, probed once per process (sysfs / procfs reads: an env-read init, DST-REVIEW row)
static DEVICE: pgsync::OnceLock<(BlockNumber, BlockNumber, BlockNumber)> = pgsync::OnceLock::new();

fn device() -> (BlockNumber, BlockNumber, BlockNumber) {
    *DEVICE.get_or_init(|| (advice_piece(), device_queue().unwrap_or(0), mem_available_blocks().unwrap_or(0)))
}
/// The device probe ahead of the first bind (`prime::kick_off`, server ready: ~100 us of sysfs / procfs reads a cold
/// statement paid at its first range source); never an error, the same values `limits` would read.
pub fn probe_device() { let _ = device(); }
pub fn device_probed() -> bool { DEVICE.get().is_some() }

/// The derived sizes for `eic` = effective_io_concurrency (0 = look-ahead off), once per process for the device's.
pub fn limits(eic: BlockNumber) -> Option<Limits> {
    let (piece, queue, avail) = device();
    let nbuffers = match bufmgr::NBuffersInited() {
        0 => crate::inputs::nbuffers(),
        n => n,
    } as BlockNumber;
    derive(piece, eic, queue, nbuffers, crate::inputs::max_connections() as BlockNumber, avail)
}

/// Rules 2-4 from the device's call size `piece`, effective_io_concurrency `eic`, its queue depth `queue` (0 = unknown:
/// `eic` stands for it), shared buffers `nbuffers`, max_connections `conns` and the page cache's available blocks `avail`
/// (0 = unknown). `None` when any bound leaves less than one call.
pub fn derive(piece: BlockNumber, eic: BlockNumber, queue: BlockNumber, nbuffers: BlockNumber, conns: BlockNumber, avail: BlockNumber) -> Option<Limits> {
    let queue = if queue == 0 { eic } else { queue };
    let conns = conns.max(1);
    let mut max = piece.saturating_mul(eic.min(queue)).min(nbuffers / conns);
    if avail > 0 {
        max = max.min(avail / conns);
    }
    let budget = piece.saturating_mul(queue).min(nbuffers / 4).max(max);
    (max >= piece && piece > 0).then_some(Limits { piece, max, engage: max, budget })
}

/// Blocks per advice call (read once per process, inside `limits`' probe): the data directory's device's largest I/O (Linux `max_sectors_kb`:
/// one WILLNEED advice reads at most that much), else `io_combine_limit` (PostgreSQL's combined read, the size its read
/// stream advises per call).
fn advice_piece() -> BlockNumber {
    match device_max_io_kb().map(|kb| kb * 1024 / BLCKSZ as u32) {
        Some(n) if n > 0 => n,
        _ => crate::inputs::io_combine_blocks() as BlockNumber,
    }
}

/// `nr_requests` of the data directory's block device: the requests its queue holds at once.
fn device_queue() -> Option<BlockNumber> {
    device_attr("nr_requests")
}

/// MemAvailable of /proc/meminfo in blocks (the page cache a statement's advice would fill).
fn mem_available_blocks() -> Option<BlockNumber> {
    let m = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 = m.lines().find_map(|l| l.strip_prefix("MemAvailable:"))?.trim().trim_end_matches("kB").trim().parse().ok()?;
    Some((kb * 1024 / BLCKSZ as u64).min(BlockNumber::MAX as u64) as BlockNumber)
}

/// `max_sectors_kb` of the block device holding the working directory (the data directory), when sysfs has it.
fn device_max_io_kb() -> Option<u32> {
    device_attr("max_sectors_kb")
}

/// One `queue/<name>` attribute of the block device holding the working directory (the data directory), when sysfs has it
/// (granule_source's read.advise sizes its WILLNEED calls from it too).
#[cfg(target_family = "wasm")]
pub(crate) fn device_attr(_name: &str) -> Option<u32> {
    None // wasm32-wasip1: no st_dev, no sysfs
}

/// One `queue/<name>` attribute (see the wasm arm above).
#[cfg(not(target_family = "wasm"))]
pub(crate) fn device_attr(name: &str) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let dev = std::fs::metadata(".").ok()?.dev();
    let (major, minor) = (((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff), (dev & 0xff) | ((dev >> 12) & !0xff));
    let base = format!("/sys/dev/block/{major}:{minor}"); // alloc: Exec
    let read = |p: String| std::fs::read_to_string(p).ok().and_then(|s| s.trim().parse::<u32>().ok());
    read(format!("{base}/queue/{name}")).or_else(|| read(format!("{base}/../queue/{name}"))) // alloc: Exec
}

#[cfg(test)]
mod tests {
    use super::*;
    use types_tuple::{HEAP_XMAX_COMMITTED, HEAP_XMIN_INVALID};

    #[test]
    fn the_hint_bits_decide_only_an_old_committed_live_tuple() {
        let old = FAST_MASK;
        assert!(hinted_visible(old, 100, 200), "committed, live, older than the snapshot's xmin");
        assert!(!hinted_visible(old, 200, 200), "xmin at the snapshot's xmin may be running for the snapshot: the seam decides");
        assert!(!hinted_visible(old, 300, 200));
        assert!(hinted_visible(old | HEAP_XMIN_INVALID, 300, 200), "frozen: visible to every snapshot");
        assert!(!hinted_visible(HEAP_XMIN_COMMITTED, 100, 200), "xmax not hinted invalid: the seam decides");
        assert!(!hinted_visible(HEAP_XMAX_INVALID, 100, 200), "xmin not hinted committed: the seam (pg_xact) decides");
        assert!(!hinted_visible(HEAP_XMIN_COMMITTED | HEAP_XMAX_COMMITTED, 100, 200));
        assert!(!hinted_visible(old, 100, 0), "an invalid snapshot xmin decides nothing");
        assert!(hinted_visible(old, 2, 200), "FrozenTransactionId precedes every normal xid");
        assert!(hinted_visible(old, u32::MAX - 5, 10), "xid wraparound: modulo-2^32 order");
    }

    #[test]
    fn a_late_request_reaches_its_attribute_over_the_survivors() {
        let sel = [0u32, 3];
        assert_eq!(reach(false, 2, 5, &sel, 10), (5, None), "a plain request: every wanted attribute, every row");
        assert_eq!(reach(true, 2, 5, &sel, 10), (2, Some(&sel[..])), "late: its attribute, the survivors");
        assert_eq!(reach(true, 2, 5, &[], 10), (2, None), "late with no selection: every row");
        assert_eq!(reach(true, 2, 5, &[0, 1], 2), (2, None), "a selection of every row is every row");
    }

    #[test]
    fn the_window_ramps_from_one_call_and_never_advises_a_block_twice() {
        let lim = Limits { piece: 16, max: 128, engage: 128, budget: 1 << 20 };
        let f = AtomicU32::new(0);
        let s0 = Stream { last: NONE_YET, advised: 0, window: 16, held: 0 };
        let (s1, lo, hi) = step(s0, 0, 10_000, &lim, &f).unwrap();
        assert_eq!((lo, hi, s1.window), (0, 16, 32), "the first cold read advises one call; the next window doubles");
        assert!(step(s1, 7, 10_000, &lim, &f).is_none(), "under a quarter of the window read since: nothing (a LIMIT stops here)");
        let (s2, lo, hi) = step(s1, 16, 10_000, &lim, &f).unwrap();
        assert_eq!((lo, hi, s2.window), (16, 48, 64), "a refill: the rest past what is advised");
        let mut s = s2;
        for from in [48, 112, 240, 368] {
            s = step(s, from, 10_000, &lim, &f).unwrap().0;
        }
        assert_eq!(s.window, 128, "the window stops at the bound");
        assert!(s.advised <= 368 + 128);
        assert_eq!(f.load(Ordering::Relaxed), s.held, "one stream holds what it advised past its read");
    }

    #[test]
    fn concurrent_windows_split_the_budget() {
        let lim = Limits { piece: 16, max: 128, engage: 128, budget: 160 };
        let f = AtomicU32::new(0);
        let w = Stream { last: NONE_YET, advised: 0, window: 128, held: 0 };
        let (a, _, ahi) = step(w, 0, 10_000, &lim, &f).unwrap();
        let (b, blo, bhi) = step(w, 0, 10_000, &lim, &f).unwrap();
        assert_eq!((ahi, bhi - blo), (128, 32), "the second scan gets what the first left of the budget");
        assert_eq!(f.load(Ordering::Relaxed), 160);
        let (a2, _, _) = step(a, 100, 10_000, &lim, &f).unwrap();
        assert!(f.load(Ordering::Relaxed) <= 160 && a2.held + b.held == f.load(Ordering::Relaxed), "never over the budget");
    }

    #[test]
    fn the_bounds_derive_from_the_device_and_the_fair_shares() {
        // 256 KB calls (32 blocks), eic 16, queue 64, 1 GB shared buffers (131072), 100 connections, 8 GB page cache
        let l = derive(32, 16, 64, 131_072, 100, 1_048_576).unwrap();
        assert_eq!((l.max, l.engage, l.budget), (512, 512, 2048), "max = calls x eic; budget = calls x queue");
        let small = derive(32, 16, 64, 16_384, 100, 1_048_576).unwrap();
        assert_eq!(small.max, 163, "128 MB shared buffers: one backend's share (NBuffers / max_connections)");
        assert!(derive(32, 0, 64, 131_072, 100, 0).is_none(), "effective_io_concurrency 0: off");
        assert!(derive(32, 16, 0, 131_072, 100, 0).unwrap().budget == 512, "unknown queue: eic stands for it");
    }
}
