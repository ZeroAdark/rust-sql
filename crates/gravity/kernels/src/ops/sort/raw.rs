//! sort's raw half (rule R6: the family's new unsafe code lives here).
pub(crate) mod cosplit;
pub(crate) mod place;
pub(crate) mod rowsort;
pub(crate) mod runmerge;
pub(crate) mod runs;
pub(crate) mod topk;

use core::cmp::Ordering;

use gravity_abi::breaker::{SortRun, SORTRUN_HDR, TABLE_SORT};
use gravity_abi::ctx::cancel_pending;
use gravity_abi::program::{consumed, partial, SPILL_RUN_CLOSE, SPILL_RUN_DROP, SPILL_RUN_SHAPE, STREAM_CONSUMED};
use gravity_abi::status::{CANCELLED, ERROR};
use gravity_abi::Ctx;

use crate::contract::Host;
use crate::host_abi::{bare, cmp_failed, SortSpec};
use super::entries::{RunShape, DESCRIBED};
use super::rowsort::{cmp_rows, point_images, reader_of, relaid, spool_pointed, tbl_shape, var_alloc, var_shape, Order, RowCmp, VarShape, CMP_ERR, HEAPS, HEAPS_SWEPT, SPOOL_EMPTY, SPOOL_SPILLED_WHAT};
use super::cut::{first_n, PgHeap, CUT_EXACT, CUT_LENIENT, CUT_NATURAL, CUT_TIES};
use super::polled::{Poll, Stopped, SORT_RUN};
use crate::table::emit::{Road, Work};
use super::runmerge::{RunCur, RunMerge};
use super::runs::{index, RUN_CUT, RUN_HEAP, RUN_KEYLANE, RUN_ORDERED};

/// F3 (b): the merge laid at `own` given up before its end (its reader stopped reading and will not resume): each run
/// reader it holds open closed (a run whose buffer still holds rows; an over run's reader was dropped by its last fill,
/// and its handle may be another reader's since), the merge no longer active. The runs stay; a merge laid again reads
/// them from their start. 0, or `ERROR` when the host refused a close.
/// # Safety
/// `own` holds a merge `runmerge::open_impl` laid (its header, then `nruns` cursors).
pub(crate) unsafe fn abandon(ctx: *mut Ctx, own: *mut u8) -> i64 {
    let m = &mut *(own as *mut RunMerge);
    let curs = own.add(core::mem::size_of::<RunMerge>()) as *const RunCur;
    for r in 0..m.nruns as usize {
        let c = &*curs.add(r);
        if c.len != 0 && crate::util::spill(ctx, SPILL_RUN_CLOSE, c.handle as i64) < 0 {
            return ERROR;
        }
    }
    m.active = 0;
    0
}

/// D2004: `sort_finish(ctx, k, tbl)`, a run reader's end at its last window (lowerv4 ops/sort/lower.rs
/// `RunEmit::next_window`, in merge mode): the merge it laid over consumed region `k` (`u64::MAX`: its own region) and
/// left in flight given up (`abandon`). A reader that takes each window whole (a count-only fold's `end - cursor`, the
/// empty grouping set's `count(*)`) never pulls a row, so its merge never drained, and the run's next reader met it in
/// flight (D1061's Bug). A merge drained to its end, or another reader's, is left as it is. 0, or `ERROR`.
pub struct SortFinish;
impl Host for SortFinish {
    type Args = (u64, u64);
    /// # Safety
    /// `k` is a consumed region of the running pipeline (or `u64::MAX`) and `tbl` its reader's emit table.
    unsafe fn run(ctx: *mut Ctx, (k, tbl): (u64, u64)) -> u64 {
        let own = if k == u64::MAX { partial(ctx) } else { consumed(ctx, k as usize) };
        let reader = reader_of(ctx, tbl as *const u32);
        match relaid(own) {
            Some(m) if m.active != 0 && m.reader == reader => abandon(ctx, own) as u64,
            _ => 0,
        }
    }
}

/// NEW-43: `sort_stream_rearm(ctx, k, tbl)`, an INCSTREAM reader past the merge of its fallen-back stream's runs (lowerv4
/// ops/sort/inc_stream.rs `StreamEmit::next_window`, the stream not ended): the merge it laid over consumed region `k`
/// (`u64::MAX`: its own region) given up if still in flight, the stream's runs dropped (`SPILL_RUN_DROP`) and the region
/// armed again as an empty run of the room it had, so the producer streams into it again from the row it stopped at. The
/// stream has one reader (inc_stream.rs `form`), so the merge is this reader's. 0, or a status below zero.
pub struct SortStreamRearm;
impl Host for SortStreamRearm {
    type Args = (u64, u64);
    /// # Safety
    /// `k` is a consumed region of the running pipeline (or `u64::MAX`) and `tbl` its reader's emit table.
    unsafe fn run(ctx: *mut Ctx, (k, tbl): (u64, u64)) -> u64 {
        let (own, stream) = if k == u64::MAX { (partial(ctx), 0) } else { (consumed(ctx, k as usize), STREAM_CONSUMED | k) };
        let (bytes, active) = match relaid(own) {
            Some(m) => (m.region, m.active != 0),
            None => return bare("Bug: an incremental stream's region re-armed with no merge laid over it") as u64,
        };
        if active && abandon(ctx, own) < 0 {
            return ERROR as u64;
        }
        let r = crate::util::spill(ctx, SPILL_RUN_DROP, stream as i64);
        if r < 0 {
            return r as u64;
        }
        SortRun::rearm(own, bytes, *(tbl as *const u32));
        0
    }
}

/// C1 part 2: `sort_stream_recycle(ctx, k, tbl, closed)`, an INCSTREAM cursor reader (lowerv4 ops/sort/inc_stream.rs
/// `StreamEmit::cursor_pull`) that handed on the `closed` rows of consumed region `k` (`u64::MAX`: its own) before it pulls
/// again: the run emptied when it holds those rows only (its producer stopped at the row that opens the next group, taken
/// off the run), so the producer streams into the whole room again. Twice the run's room in rows, plus one when emptied.
pub struct SortStreamRecycle;
impl Host for SortStreamRecycle {
    type Args = (u64, u64, u64);
    /// # Safety
    /// `k` is a consumed region of the running pipeline (or `u64::MAX`) holding a live run, `tbl` its reader's emit table.
    unsafe fn run(ctx: *mut Ctx, (k, tbl, closed): (u64, u64, u64)) -> u64 {
        let own = if k == u64::MAX { partial(ctx) } else { consumed(ctx, k as usize) };
        if relaid(own).is_some() {
            return bare("Bug: an incremental stream's run recycled under a merge") as u64;
        }
        let r = SortRun::at_share(own, *(tbl as *const u32));
        let emptied = r.len as u64 == closed;
        if emptied {
            r.clear();
        }
        (r.cap as u64) << 1 | emptied as u64
    }
}

/// D2572-c: a var run's rows' layout told the host before a put (`SPILL_RUN_SHAPE`, advisory: a compressing spill file
/// compacts the rows by it; the answer ignored, a host without the op answers an error).
/// # Safety
/// `v` is the var section of the run's emit table.
pub(crate) unsafe fn declare_rows(ctx: *mut Ctx, row_bytes: u32, v: &VarShape) {
    let mut w: Vec<u32> = Vec::with_capacity(3 + v.nvar); // alloc: Exec
    w.extend([row_bytes, v.rec_off as u32, v.nvar as u32]);
    w.extend((0..v.nvar).map(|c| *v.slots.add(2 * c + 1)));
    let _ = crate::util::spill(ctx, SPILL_RUN_SHAPE, w.as_ptr() as i64);
}

/// jmerge-0012 (the exact request's merge inner, face/slot_run.rs `pulled_end`): `spool_point(ctx, run, tbl, from)`,
/// rows `[from, len)` of a run its reader pulls pointed at their own images (a by-reference cell's datum word, which the
/// end leaves 0 until a seal points it, is what the reader reads): the row the end just stored, or every row after a
/// growth moved the rows and the var copies. 0.
pub struct SpoolPoint;
impl Host for SpoolPoint {
    type Args = (u64, u64, u64);
    /// # Safety
    /// `run` is a live resident run whose rows have the layout `tbl` names.
    unsafe fn run(_ctx: *mut Ctx, (run, tbl, from): (u64, u64, u64)) -> u64 {
        let r = &*(run as *const SortRun);
        for i in from as usize..r.len as usize {
            point_images(r.row(i) as *mut u8, tbl as *const u32);
        }
        0
    }
}

/// `num_order_word(ctx, p, len)`: a numeric sort key's order word from its payload's `len` bytes at `p` (lowerv4
/// rep/keylane.rs `pairs`; `typesem::numeric::order_word`, which never panics), inexact: its var tail decides ties.
pub struct NumOrderWord;
impl Host for NumOrderWord {
    type Args = (u64, u64);
    /// # Safety
    /// `p` is readable for `len` bytes.
    unsafe fn run(_ctx: *mut Ctx, (p, len): (u64, u64)) -> u64 {
        gravity_fn::typesem::numeric::order_word(core::slice::from_raw_parts(p as usize as *const u8, len as usize)) as u64
    }
}

/// `net_order_word(ctx, p, len)`: an inet / cidr sort key's order word from its payload (lowerv4 rep/keylane.rs `pairs`;
/// `typesem::prefix::net_word`, PostgreSQL's abbreviation, which never panics), inexact: its var tail decides ties.
pub struct NetOrderWord;
impl Host for NetOrderWord {
    type Args = (u64, u64);
    /// # Safety
    /// `p` is readable for `len` bytes.
    unsafe fn run(_ctx: *mut Ctx, (p, len): (u64, u64)) -> u64 {
        gravity_fn::typesem::prefix::net_word(core::slice::from_raw_parts(p as usize as *const u8, len as usize)) as u64
    }
}

/// `bits_order_word(ctx, p, len)`: a bit / varbit sort key's order word from its payload (`typesem::prefix::bits_word`):
/// exact up to 56 bits (its low bit clear, `KFLAG_WORDEXACT`: an equal word is an equal value), else its var tail decides ties.
pub struct BitsOrderWord;
impl Host for BitsOrderWord {
    type Args = (u64, u64);
    /// # Safety
    /// `p` is readable for `len` bytes.
    unsafe fn run(_ctx: *mut Ctx, (p, len): (u64, u64)) -> u64 {
        gravity_fn::typesem::prefix::bits_word(core::slice::from_raw_parts(p as usize as *const u8, len as usize)) as u64
    }
}

/// `bytes_order_word(ctx, p, len)`: a dictionary-coded bpchar sort key's order word from its render's trimmed payload
/// (`typesem::prefix::bytes_word`, under a bytes-ordered collation), inexact: its var tail (which trims) decides ties.
pub struct BytesOrderWord;
impl Host for BytesOrderWord {
    type Args = (u64, u64);
    /// # Safety
    /// `p` is readable for `len` bytes.
    unsafe fn run(_ctx: *mut Ctx, (p, len): (u64, u64)) -> u64 {
        let b = if len == 0 { &[][..] } else { core::slice::from_raw_parts(p as usize as *const u8, len as usize) };
        gravity_fn::typesem::prefix::bytes_word(b) as u64
    }
}

/// jmerge-0012: `spool_live(ctx, k, tbl)`, a pulled reader's view of consumed region `k` (face/slot_run.rs
/// `pulled_reader`, at each open: after each pull, which may have grown the run elsewhere): `live`'s run, or `ERROR`.
pub struct SpoolLive;
impl Host for SpoolLive {
    type Args = (u64, u64);
    /// # Safety
    /// `ctx` is bound; `k` is a region the calling pipeline consumes; `tbl` is the run's table.
    unsafe fn run(ctx: *mut Ctx, (k, tbl): (u64, u64)) -> u64 {
        live(consumed(ctx, k as usize) as *mut SortRun, tbl as *const u32) as u64
    }
}

/// The run at `r` as stored so far, sealed or still filling (its rows pointed as stored, `spool_point`); the empty run
/// when the producer stored nothing yet; a run whose rows moved since they were pointed (a seal's permutation, a copy)
/// pointed again, as `spool_view` does. A run that is not a resident sort run within its capacity is the spilled spool's
/// named error (a pulled run never puts).
/// # Safety
/// `r` is null or a live region of the pulled run's slot; `tbl` is its table.
pub(crate) unsafe fn live(r: *mut SortRun, tbl: *const u32) -> *const u8 {
    if r.is_null() || (*r).cap == 0 {
        return SPOOL_EMPTY.as_ptr() as *const u8;
    }
    if (*r).kind != TABLE_SORT || (*r).len > (*r).cap {
        return crate::host_abi::bare(SPOOL_SPILLED_WHAT) as *const u8;
    }
    if !spool_pointed(&*r, tbl) {
        for i in 0..(*r).len as usize {
            point_images((*r).row(i) as *mut u8, tbl);
        }
    }
    r as *const u8
}

/// The run's first `keep` rows re-laid in `row`'s order; each var copy of theirs re-reserved from the top down (under the
/// table copy) and its slot re-pointed. D4373 (b): the rows and their copies pass through one copy (`SortCopy`: a block of
/// `ctx`'s grant on `road`; the allocator's on a null ctx or under the memory policy's floor); `Err` (the run untouched)
/// when the grant refuses it. `road` `None`: the allocator's road whatever the grant (a heap or span cut's, as before),
/// LEDGER2 step 5 its bytes carved from a scratch arena under `ctx`'s slot owner (`SortCopy::allocator`), never refused.
pub(crate) unsafe fn relay(ctx: *mut Ctx, road: Option<Road>, run: *mut SortRun, keep: usize, row: &dyn Fn(usize) -> *const u8, var: Option<VarShape>) -> Result<(), i64> {
    let (rb, base) = ((*run).row_bytes as usize, (*run).row(0) as *mut u8);
    let slot = |c: usize| *var.unwrap().slots.add(2 * c + 1) as usize;
    let nvar = var.map_or(0, |v| v.nvar);
    let size_at = |r: *const u8, so: usize| (r.add(so) as *const u32).read_unaligned();
    let mut copies = 0usize;
    for j in 0..keep {
        let r = row(j);
        for c in 0..nvar {
            let size = size_at(r, slot(c));
            if size != 0 {
                copies += var_alloc(size) as usize;
            }
        }
    }
    let mut tmp = match road {
        Some(road) => runs::SortCopy::new(ctx, keep * rb + copies, road)?,
        None => runs::SortCopy::allocator(ctx, keep * rb + copies),
    };
    for j in 0..keep {
        tmp.extend_from_slice(core::slice::from_raw_parts(row(j), rb));
    }
    for j in 0..keep {
        let r = row(j);
        for c in 0..nvar {
            let so = slot(c);
            let size = size_at(r, so);
            if size != 0 {
                let off = (r.add(so + 4) as *const u32).read_unaligned() as usize;
                tmp.extend_from_slice(core::slice::from_raw_parts(r.add(so + off), var_alloc(size) as usize));
            }
        }
    }
    core::ptr::copy_nonoverlapping(tmp.as_ptr(), base, keep * rb);
    (*run).len = keep as u32;
    if var.is_none() {
        return Ok(());
    }
    // the copies lie from under the table copy down, as `reserve_var` laid them: the kept ones take no more room
    let words = (run as *mut u8).add(SORTRUN_HDR + (*run).cap as usize * rb) as *mut u32;
    let mut at = SORTRUN_HDR + (*run).cap as usize * rb - table_bytes(run);
    let mut src = keep * rb;
    for j in 0..keep {
        for c in 0..nvar {
            let so = slot(c);
            let size = size_at(base.add(j * rb), so);
            if size != 0 {
                let len = var_alloc(size) as usize;
                at -= len;
                core::ptr::copy_nonoverlapping(tmp.as_ptr().add(src), (run as *mut u8).add(at), len);
                src += len;
                let slot = SORTRUN_HDR + j * rb + so;
                ((run as *mut u8).add(slot + 4) as *mut u32).write_unaligned((at - slot) as u32);
            }
        }
    }
    words.write(at as u32);
    Ok(())
}

/// USEROP K7: `sort_heap_step(ctx, run, tbl, n)`, an exact top's step after its row `len - 1` was stored (`n` = count +
/// offset; lowerv4 ops/sort/topk_rows.rs `heap_step`; cut.rs `PgHeap` is tuplesort's bounded heap): nothing while the run
/// holds `2n` rows or fewer; at `2n + 1`, `make_bounded_heap` over the rows in arrival order; past it, the new row
/// against the top. 0; `CANCELLED`, the run untouched, when the abort byte is set; `ERROR` when a compare raised (its
/// error stashed; the run's heap dropped).
pub struct SortHeapStep;
impl Host for SortHeapStep {
    type Args = (u64, u64, u64);
    /// # Safety
    /// `run` is a live var sort run of emit table `tbl`'s shape holding at least one row.
    unsafe fn run(ctx: *mut Ctx, (run, tbl, n): (u64, u64, u64)) -> u64 {
        if !ctx.is_null() && cancel_pending(ctx) {
            return CANCELLED as u64;
        }
        let (run, tbl) = (run as usize as *mut SortRun, tbl as usize as *const u32);
        let cmp = RowCmp::of(tbl);
        let (len, rb, base) = ((*run).len as usize, (*run).row_bytes as usize, (*run).row(0) as *const u8);
        let n = n.min(u32::MAX as u64 / 4) as usize;
        CMP_ERR.with(|c| c.set(false));
        let rc = |a: u32, b: u32| cmp.cmp(base.add(a as usize * rb), base.add(b as usize * rb)).reverse();
        let pg = PgHeap { rc: &rc };
        let heap = match take_heap(run, len, true) {
            Some(mut h) => {
                pg.offer(&mut h, n, len as u32 - 1);
                Some(h)
            }
            None if n > 0 && len > 2 * n => {
                (*run).pad[2] &= !(RUN_CUT | RUN_ORDERED);
                Some(pg.make(len, n))
            }
            None => None,
        };
        match heap {
            Some(h) if !CMP_ERR.with(|c| c.get()) => {
                keep_heap(run, len, h);
                0
            }
            _ => {
                (*run).pad[2] &= !RUN_HEAP;
                (if CMP_ERR.with(|c| c.get()) { crate::host_abi::cmp_failed("a sort compare failed") } else { 0 }) as u64
            }
        }
    }
}

/// The run's heap (indexes in PostgreSQL's memtuples order) when it is current: `RUN_HEAP` set and the run `len` rows,
/// one more than at its last step (`grew`) or as many (`!grew`); taken out of `HEAPS`. An entry left at the run's address
/// by an earlier run (one freed unsealed: its statement raised elsewhere) is dropped here too.
/// # Safety
/// `run` is a live sort run.
unsafe fn take_heap(run: *mut SortRun, len: usize, grew: bool) -> Option<Vec<u32>> {
    let key = run as usize;
    let h = HEAPS.with(|h| {
        let mut h = h.borrow_mut();
        h.iter().position(|e| e.0 == key).map(|i| h.swap_remove(i))
    })?;
    let at = h.1 as usize + grew as usize;
    ((*run).pad[2] & RUN_HEAP != 0 && at == len).then_some(h.2)
}

/// The run's heap kept for its next step (the session teardown empties `HEAPS`: `sweep_heaps`).
/// # Safety
/// `run` is a live sort run of `len` rows.
unsafe fn keep_heap(run: *mut SortRun, len: usize, heap: Vec<u32>) {
    (*run).pad[2] |= RUN_HEAP;
    if !HEAPS_SWEPT.with(|s| s.replace(true)) {
        gravity_fn::at_session_end(sweep_heaps);
    }
    HEAPS.with(|h| h.borrow_mut().push((run as usize, len as u32, heap))); // alloc: Exec
}

/// The session teardown's sweep: every heap a run left (its statement raised before the seal took it) dropped.
fn sweep_heaps() {
    let _ = HEAPS.try_with(|h| h.try_borrow_mut().map(|mut v| v.clear()));
    let _ = HEAPS_SWEPT.try_with(|s| s.set(false));
}

/// The soft cap's cut of an exact top (cut.rs `sort_cut_res_var`): the heap's rows re-laid in heap order (the rest
/// dropped), the heap their indexes `0..n` again. Before the heap (a run with no room under `2n + 1` rows: tuplesort's
/// `LACKMEM` switch), the heap is made first. Returns the rows kept; never arms the bound (`RUN_CUT`) nor marks the run
/// ordered.
/// # Safety
/// `run` is a live var sort run of `var`'s shape; `cmp` its row compare.
pub(crate) unsafe fn heap_cut(ctx: *mut Ctx, run: *mut SortRun, cmp: &dyn Fn(*const u8, *const u8) -> Ordering, var: VarShape, n: u64) -> i64 {
    let (len, rb, base) = ((*run).len as usize, (*run).row_bytes as usize, (*run).row(0) as *mut u8);
    (*run).pad[2] &= !(RUN_CUT | RUN_ORDERED);
    let n = n.min(u32::MAX as u64 / 4) as usize;
    let rc = |a: u32, b: u32| cmp(base.add(a as usize * rb), base.add(b as usize * rb)).reverse();
    let h = match take_heap(run, len, false) {
        Some(h) => h,
        None if n > 0 && len > n => PgHeap { rc: &rc }.make(len, n),
        None => return len as i64,
    };
    if CMP_ERR.with(|c| c.get()) {
        (*run).pad[2] &= !RUN_HEAP;
        return len as i64;
    }
    let keep = h.len();
    if let Err(e) = relay(ctx, None, run, keep, &|j| base.add(h[j] as usize * rb) as *const u8, Some(var)) {
        return e;
    }
    keep_heap(run, keep, (0..keep as u32).collect()); // alloc: Exec
    keep as i64
}

/// The seal's order of an exact top's run (runs.rs `index`): `sort_bounded_heap`'s rows, then the rows the heap dropped
/// (the Limit above reads only the first n); `None` when the run holds no current heap (fewer than `2n + 1` rows: the
/// whole run in PostgreSQL's qsort order, `sort_exact`). LEDGER2 step 5: the bitmap of the heap's rows and the order are
/// the allocator's road (`Work::scratch`: from `ARENA_SCRATCH_MIN` bytes on, a scratch arena under `ctx`'s slot owner).
/// # Safety
/// `run` is a live sort run; `cmp` its row compare; `ctx` is null or bound.
pub(crate) unsafe fn heap_order(ctx: *mut Ctx, run: *mut SortRun, cmp: &dyn Fn(*const u8, *const u8) -> Ordering) -> Option<Work<u32>> {
    let (len, rb, base) = ((*run).len as usize, (*run).row_bytes as usize, (*run).row(0) as *const u8);
    let h = take_heap(run, len, false)?;
    (*run).pad[2] &= !RUN_HEAP;
    let rc = |a: u32, b: u32| cmp(base.add(a as usize * rb), base.add(b as usize * rb)).reverse();
    let top = PgHeap { rc: &rc }.sort(h);
    let mut held = Work::scratch(ctx, len, |_| false, false).expect("an infallible scratch");
    top.iter().for_each(|&i| held[i as usize] = true);
    let mut rest = (0..len as u32).filter(|&i| !held[i as usize]);
    Some(Work::scratch(ctx, len, |j| if j < top.len() { top[j] } else { rest.next().unwrap_or(0) }, false).expect("an infallible scratch"))
}

/// D2690 (`SpanCut` / `SpanSeen`'s middle word): keep one row of each run of equal rows (the Unique above the sort
/// compares every key), not every row.
pub const SPAN_DISTINCT: u64 = 1 << 63;

/// The middle word's parts: the open group's first row (low 32 bits), the count (`k`, or a cut's `2 n + full`; bits 32
/// to 62), `SPAN_DISTINCT`.
fn span_word(w: u64) -> (usize, usize, bool) {
    ((w & 0xffff_ffff) as usize, ((w >> 32) & 0x7fff_ffff) as usize, w & SPAN_DISTINCT != 0)
}

/// The run row compare of emit table `tbl`: a var run's `RowCmp`, a fixed run's key words (`cmp_rows`, as
/// `incsort_span` compares them); and how its sorts order (`RowCmp::order`: `Exact` under a user comparator, spicmp;
/// `Lenient` with a key ordered through fmgr, TOTAL-ORDER).
/// # Safety
/// `tbl` is an emit table.
unsafe fn table_cmp(tbl: *const u32) -> (impl Fn(*const u8, *const u8) -> Ordering, Order) {
    let (_, nkey, _, spec) = tbl_shape(tbl);
    let rc = var_shape(tbl).map(|_| RowCmp::of(tbl));
    let order = rc.as_ref().map_or(Order::Images, RowCmp::order);
    (move |a, b| rc.as_ref().map_or_else(|| cmp_rows(nkey / 2, spec, a, b), |c| c.cmp(a, b)), order)
}

/// D2690, the top-K DISTINCT incremental sort (lowerv4 ops/sort/incremental.rs `Top`): `sort_cut_span(ctx, run, word,
/// tbl)`, rows `[from, len)` of a live sort run (an incremental sort's open prefix group) ordered by the full row compare
/// and cut to their first `k` (under `SPAN_DISTINCT`: their first `k` distinct, the first row of each run of equal
/// rows), the rows before `from` kept as they lie and every kept row's var copies re-laid (the dropped rows' freed).
/// Returns the span's rows kept; `CANCELLED`, the run untouched, when the abort byte is set; `ERROR` when a compare
/// failed (its error stashed) or the scratch cannot be had.
pub struct SpanCut;
impl Host for SpanCut {
    type Args = (u64, u64, u64);
    /// # Safety
    /// `run` is a live sort run of emit table `tbl`'s shape; the word's `from` is at most its `len`.
    unsafe fn run(ctx: *mut Ctx, (run, word, tbl): (u64, u64, u64)) -> u64 {
        if !ctx.is_null() && cancel_pending(ctx) {
            return CANCELLED as u64;
        }
        let (from, k, distinct) = span_word(word);
        let (tbl, r) = (tbl as usize as *const u32, run as usize as *mut SortRun);
        let (cmp, order) = table_cmp(tbl);
        RowCmp::failed();
        let st = span_cut(ctx, r, from, k, distinct, order, &cmp, var_shape(tbl));
        (if RowCmp::failed() { ERROR } else { st }) as u64
    }
}

/// `order` `Exact` (spicmp, `RowCmp::exact`): the span orders by PostgreSQL's qsort (`sort_exact`), which stops at a raising
/// user comparator's first failure where std's sort may panic on the `Equal` it answers; `Lenient` (TOTAL-ORDER) by
/// PostgreSQL's qsort too, so a comparator through fmgr whose order is not total never panics.
/// LEDGER2 step 5: its index, kept list and relay copy are the allocator's road (`Work::scratch`, `relay`'s `None`: from
/// `ARENA_SCRATCH_MIN` bytes on, a scratch arena under `ctx`'s slot owner).
#[allow(clippy::too_many_arguments)]
unsafe fn span_cut(ctx: *mut Ctx, run: *mut SortRun, from: usize, k: usize, distinct: bool, order: Order, cmp: &dyn Fn(*const u8, *const u8) -> Ordering, var: Option<VarShape>) -> i64 {
    let (len, rb, base) = ((*run).len as usize, (*run).row_bytes as usize, (*run).row(0) as *mut u8);
    if from >= len {
        return 0;
    }
    let row = |i: usize| base.add(i * rb) as *const u8;
    let Some(mut idx) = Work::scratch(ctx, len - from, |i| (from + i) as u32, true) else { return ERROR };
    super::rowsort::sort_by_order(&mut idx, order, &|a: u32, b: u32| cmp(row(a as usize), row(b as usize)).then(a.cmp(&b)));
    if order == Order::Exact && crate::host_abi::CMP_ERR.with(|c| c.get()) {
        return ERROR; // the run untouched, the error stashed (no further call, as PostgreSQL's ERROR leaves the sort)
    }
    let mut kept = Work::scratch(ctx, k.min(idx.len()), |_| 0u32, false).expect("an infallible scratch");
    let mut nk = 0;
    for &i in idx.iter() {
        if nk == kept.len() {
            break;
        }
        if distinct && nk > 0 && cmp(row(kept[nk - 1] as usize), row(i as usize)).is_eq() {
            continue;
        }
        kept[nk] = i;
        nk += 1;
    }
    drop(idx);
    if let Err(e) = relay(ctx, None, run, from + nk, &|j| if j < from { row(j) } else { row(kept[j - from] as usize) }, var) {
        return e;
    }
    nk as i64
}

/// D2690: `sort_span_seen(ctx, run, word, tbl)`, the run's last row (`len - 1`, just stored) dropped when a cut open
/// group already holds what it would add: the word's count is the group's last cut, `2 n + full` (`n` rows ordered from
/// row `from`, distinct under `SPAN_DISTINCT`; `full`: they are the group's first `k`). Dropped when `full` and it
/// orders at or after row `from + n - 1` (the `k`-th: a row equal to it or past it is never among the first `k`), or
/// under `SPAN_DISTINCT` when it equals one of the `n` (a binary search: the Unique above hands on one of them). Its var
/// copies are freed (the run's last reservations: `var_top` moves up past them; copies that are not the last stay,
/// room the next cut re-lays). 1 dropped, 0 kept, `ERROR` when a compare failed (its error stashed).
pub struct SpanSeen;
impl Host for SpanSeen {
    type Args = (u64, u64, u64);
    /// # Safety
    /// As `SpanCut`; rows `[from, from + n)` are a cut's, all before row `len - 1`.
    unsafe fn run(_ctx: *mut Ctx, (run, word, tbl): (u64, u64, u64)) -> u64 {
        let (from, cut, distinct) = span_word(word);
        let (tbl, r) = (tbl as usize as *const u32, run as usize as *mut SortRun);
        let (cmp, _) = table_cmp(tbl);
        RowCmp::failed();
        let st = span_seen(r, from, cut >> 1, cut & 1 != 0, distinct, &cmp, var_shape(tbl));
        (if RowCmp::failed() { ERROR } else { st }) as u64
    }
}

unsafe fn span_seen(r: *mut SortRun, from: usize, n: usize, full: bool, distinct: bool, cmp: &dyn Fn(*const u8, *const u8) -> Ordering, var: Option<VarShape>) -> i64 {
    let len = (*r).len as usize;
    if n == 0 || from + n >= len {
        return 0;
    }
    let last = (*r).row(len - 1);
    let o = cmp(last, (*r).row(from + n - 1));
    let seen = if full && o.is_ge() {
        true
    } else if distinct && o.is_lt() {
        let (mut lo, mut hi) = (from, from + n - 1);
        loop {
            if lo >= hi {
                break false;
            }
            let mid = lo + (hi - lo) / 2;
            match cmp(last, (*r).row(mid)) {
                Ordering::Equal => break true,
                Ordering::Less => hi = mid,
                Ordering::Greater => lo = mid + 1,
            }
        }
    } else {
        distinct && o.is_eq()
    };
    if !seen {
        return 0;
    }
    if let Some(v) = var {
        let (mut bytes, mut low) = (0u32, u32::MAX);
        for c in 0..v.nvar {
            let so = *v.slots.add(2 * c + 1) as usize;
            let size = (last.add(so) as *const u32).read_unaligned();
            if size != 0 {
                let off = (last.add(so + 4) as *const u32).read_unaligned() as usize;
                bytes += var_alloc(size);
                low = low.min((last as usize + so + off - r as usize) as u32);
            }
        }
        if bytes != 0 && low == (*r).var_top() {
            let words = (r as *mut u8).add(SORTRUN_HDR + (*r).cap as usize * (*r).row_bytes as usize) as *mut u32;
            words.write(low + bytes);
        }
    }
    (*r).len -= 1;
    1
}


// The cut's host entries and its in-place cut, moved unchanged from cut.rs (its consts, first_n and PgHeap stay there).
crate::contract::host! {
    /// The fixed-row cut: `nkey` and `spec` as `sort_open_res` takes them. Returns the new length; with `n` rows or more
    /// kept, `out[0..2]` is row n-1's key-0 `(flags, word)` pair (the bound). `CANCELLED`, the run untouched, when the abort
    /// byte is set (D1028: each cut is one sort of the run; the caller's retry reaches the check once per cut).
    /// # Safety
    /// `ctx` is bound; `run` is a live sort run of fixed rows; `out` has room for two words.
    pub unsafe fn sort_cut_res(ctx: *mut Ctx, run: *mut u8, nkey: u64, spec: u64, n: u64, flags: u64, out: *mut u64) -> i64 {
        if cancel_pending(ctx) {
            return CANCELLED;
        }
        let (nkey, spec) = (nkey as usize, SortSpec(spec));
        guard!(ERROR, cut(ctx, run as *mut SortRun, nkey, &|a, b| cmp_rows(nkey / 2, spec, a, b), None, n, flags & !(CUT_EXACT | CUT_LENIENT), out))
    }
}

crate::contract::host! {
    /// The var-row cut (`sort_open_res_var`'s table `tbl`): the kept rows' copies re-laid, the dropped rows' freed.
    /// # Safety
    /// As `sort_cut_res`; `tbl` is the sort's emit table.
    pub unsafe fn sort_cut_res_var(ctx: *mut Ctx, run: *mut u8, tbl: *const u32, n: u64, flags: u64, out: *mut u64) -> i64 {
        if cancel_pending(ctx) {
            return CANCELLED;
        }
        guard!(ERROR, {
            let Some(v) = var_shape(tbl) else { return bare("a var cut without its var table") };
            let cmp = RowCmp::of(tbl);
            CMP_ERR.with(|c| c.set(false));
            // a heap top (an exact one, or ROW-TOPN's once its heap is made: `RUN_HEAP`) cuts to its heap's rows
            let st = if (cmp.exact() || (*(run as *mut SortRun)).pad[2] & RUN_HEAP != 0) && flags & CUT_TIES == 0 {
                heap_cut(ctx, run as *mut SortRun, &|a, b| cmp.cmp(a, b), v, n)
            } else {
                let mode = match cmp.order() {
                    Order::Exact => CUT_EXACT,
                    Order::Lenient => CUT_LENIENT,
                    Order::Images => 0,
                };
                cut(ctx, run as *mut SortRun, tbl_shape(tbl).1, &|a, b| cmp.cmp(a, b), Some(v), n, flags & !(CUT_EXACT | CUT_LENIENT) | mode, out)
            };
            if CMP_ERR.with(|c| c.get()) { cmp_failed("a sort compare failed") } else { st }
        })
    }
}

/// # Safety — `ctx` is bound; `run` is a live sort run (the host entries' contracts).
unsafe fn cut(ctx: *mut Ctx, run: *mut SortRun, nkey: usize, cmp: &dyn Fn(*const u8, *const u8) -> Ordering, var: Option<VarShape>, n: u64, flags: u64, out: *mut u64) -> i64 {
    let (len, rb, base) = ((*run).len as usize, (*run).row_bytes as usize, (*run).row(0) as *mut u8);
    let exact = flags & CUT_EXACT != 0;
    let order = if exact { Order::Exact } else if flags & CUT_LENIENT != 0 { Order::Lenient } else { Order::Images };
    let idx = if order == Order::Images && (n as usize) < len && n > 0 && nkey > 0 && (*run).pad[2] & (RUN_ORDERED | RUN_KEYLANE) == 0 {
        // D4140 (C9's kernel face, polled.rs): the cut's sort polls the statement through the kernels' own check
        let pending = || unsafe { cancel_pending(ctx) };
        // D4373 (b): the index and the sort's scratch are blocks of the grant
        let mut idx = match Work::from_fn_on(ctx, len, |i| i as u32, Road::Seal) {
            Ok(w) => w,
            Err(e) => return e,
        };
        let mut scratch = match Work::<u32>::block_on(ctx, if len > SORT_RUN { len } else { 0 }, Road::Seal) {
            Ok(w) => w,
            Err(e) => return e,
        };
        match first_n(&mut Poll::new(&pending), &mut idx, n as usize, flags & CUT_TIES != 0, &mut scratch, &|i| base.add(i as usize * rb) as *const u8, cmp) {
            Ok(()) => Some(idx),
            Err(Stopped) => return CANCELLED,
        }
    } else {
        match index(ctx, run, nkey, cmp, order, Road::Seal) {
            Ok(idx) => idx,
            Err(e) => return e,
        }
    };
    let row = |i: usize| base.add(idx.as_ref().map_or(i, |x| x[i] as usize) * rb) as *const u8;
    let mut keep = len.min(n as usize);
    if exact && CMP_ERR.with(|c| c.get()) {
        return ERROR; // the run untouched, the error stashed (no further call, as PostgreSQL's ERROR leaves the sort)
    }
    if flags & CUT_TIES != 0 && keep > 0 {
        while keep < len && cmp(row(keep - 1), row(keep)).is_eq() {
            keep += 1;
        }
    }
    match &idx {
        // D4373: a cut that keeps every row (a top whose bound passes the run) only orders it: the rows permuted where
        // they lie (`permute_rows`: a copy in the grant's room, else in place by cycles), the copies left where they lie,
        // never a relay copy of the whole run
        Some(x) if keep == len => {
            let pending = || !ctx.is_null() && cancel_pending(ctx);
            if self::rowsort::permute_rows(ctx, run, &|j| x[j] as usize, var, &pending) {
                return CANCELLED;
            }
        }
        _ if idx.is_some() || keep < len => {
            if let Err(e) = relay(ctx, Some(Road::Seal), run, keep, &row, var) {
                return e;
            }
        }
        _ => {}
    }
    if flags & CUT_NATURAL != 0 {
        (*run).pad[2] |= RUN_ORDERED;
    }
    if n > 0 && keep as u64 >= n {
        (*run).pad[2] |= RUN_CUT; // D508: row n-1 in place, the row walk's bound (exec v4/m3_sort.rs run_bound)
        let b = (base.add((n as usize - 1) * rb) as *const [u64; 2]).read_unaligned();
        (out.write(b[0]), out.add(1).write(b[1]));
    } else {
        (*run).pad[2] &= !RUN_CUT;
    }
    keep as i64
}

// SAFETY: a var shape's slot words are the table copy in a worker run, read-only and live while its merge runs.
unsafe impl Send for RunShape {}
unsafe impl Sync for RunShape {}

impl RunShape {
    /// A var run's shape from the table copy `sort_open_res_var` reserved at its rows' end; `None` without one.
    /// # Safety
    /// `run` is a live sort run.
    pub unsafe fn of_run(run: *const SortRun) -> Option<RunShape> {
        let end = (run as *const u8).add(SORTRUN_HDR + (*run).cap as usize * (*run).row_bytes as usize);
        let [n, magic] = (end.sub(8) as *const [u32; 2]).read_unaligned();
        let copy = (4 * n as usize + 8).next_multiple_of(8);
        // a put's clear leaves the copy's bytes but not its reservation
        if magic != DESCRIBED || (*run).var_top() as usize > end.offset_from(run as *const u8) as usize - copy {
            return None;
        }
        let tbl = end.sub(copy) as *const u32;
        Some(RunShape { cmp: RowCmp::of(tbl), rb: (*run).row_bytes as usize, var: var_shape(tbl) })
    }

    /// The bytes a sealed run's var copies take (its table copy not counted).
    /// # Safety
    /// `run` is a live sort run of this shape.
    pub unsafe fn var_bytes(&self, run: *const SortRun) -> u64 {
        if self.var.is_none() {
            return 0;
        }
        let end = SORTRUN_HDR + (*run).cap as usize * self.rb;
        let copy = (4 * ((run as *const u8).add(end - 8) as *const u32).read_unaligned() as usize + 8).next_multiple_of(8);
        (end - copy - (*run).var_top() as usize) as u64
    }

    /// # Safety
    /// `a` and `b` are rows of this shape.
    pub unsafe fn cmp(&self, a: *const u8, b: *const u8) -> Ordering {
        self.cmp.cmp(a, b)
    }

    /// D3190: whether rows of this shape and of `other`'s order alike on their first `nkey` keys (`RowCmp::cross_ok`).
    /// # Safety
    /// Both shapes' tables are live.
    pub unsafe fn cross_ok(&self, other: &RunShape, nkey: usize) -> bool {
        self.cmp.cross_ok(&other.cmp, nkey)
    }

    /// D3190: row `a` of this shape against row `b` of `other`'s on their first `nkey` keys (`RowCmp::cmp_cross`).
    /// # Safety
    /// `a` is a row of this shape, `b` one of `other`'s, and `cross_ok(other, nkey)`.
    pub unsafe fn cmp_cross(&self, a: *const u8, other: &RunShape, b: *const u8, nkey: usize) -> Ordering {
        self.cmp.cmp_cross(a, &other.cmp, b, nkey)
    }

    /// The bytes the var copies of `n` rows from `row` on take in a merge's output.
    /// # Safety
    /// The rows are live rows of this shape.
    pub unsafe fn var_len(&self, row: *const u8, n: u32) -> u64 {
        let Some(v) = self.var else { return 0 };
        let mut t = 0u64;
        for i in 0..n as usize {
            for c in 0..v.nvar {
                let size = (row.add(i * self.rb + *v.slots.add(2 * c + 1) as usize) as *const u32).read_unaligned();
                t += if size == 0 { 0 } else { var_alloc(size) as u64 };
            }
        }
        t
    }

    /// Rows `srcs` (`(first row, rows)`, each in this shape's order) merged by a tree of losers into `out`, their var copies
    /// packed from `var` on and their slots re-pointed; ties in any order (the SQL fixes none). False when a collation
    /// compare failed (its error stashed).
    /// # Safety
    /// Every source row, `out`'s rows and the var bytes from `var` are live, disjoint and sized for the rows.
    pub unsafe fn merge(&self, srcs: &mut [(usize, u32)], out: *mut u8, var: *mut u8) -> bool {
        let (k, rb) = (srcs.len(), self.rb);
        if k == 1 && self.var.is_none() {
            core::ptr::copy_nonoverlapping(srcs[0].0 as *const u8, out, srcs[0].1 as usize * rb);
            return true;
        }
        CMP_ERR.with(|c| c.set(false));
        let beats = |s: &[(usize, u32)], a: usize, b: usize| match (s[a].1 == 0, s[b].1 == 0) {
            (true, _) => false,
            (false, true) => true,
            _ => self.cmp.cmp(s[a].0 as *const u8, s[b].0 as *const u8) != Ordering::Greater,
        };
        let mut tree = vec![usize::MAX; k.max(1)]; // alloc: Exec
        for leaf in 0..k {
            let (mut cur, mut node) = (leaf, (leaf + k) / 2);
            while node > 0 && tree[node] != usize::MAX {
                if beats(srcs, tree[node], cur) {
                    core::mem::swap(&mut tree[node], &mut cur);
                }
                node /= 2;
            }
            tree[node] = cur;
        }
        let (mut o, mut vo) = (out, var);
        while k > 0 && srcs[tree[0]].1 != 0 {
            let w = tree[0];
            let src = srcs[w].0 as *const u8;
            core::ptr::copy_nonoverlapping(src, o, rb);
            for c in 0..self.var.map_or(0, |v| v.nvar) {
                let so = *self.var.expect("a var shape").slots.add(2 * c + 1) as usize;
                let size = (src.add(so) as *const u32).read_unaligned();
                if size != 0 {
                    let alloc = var_alloc(size) as usize;
                    core::ptr::copy_nonoverlapping(src.add(so + (src.add(so + 4) as *const u32).read_unaligned() as usize), vo, alloc);
                    (o.add(so + 4) as *mut u32).write_unaligned(vo.offset_from(o.add(so)) as u32);
                    vo = vo.add(alloc);
                }
            }
            o = o.add(rb);
            srcs[w] = (srcs[w].0 + rb, srcs[w].1 - 1);
            let (mut cur, mut node) = (w, (w + k) / 2);
            while node > 0 {
                if beats(srcs, tree[node], cur) {
                    core::mem::swap(&mut tree[node], &mut cur);
                }
                node /= 2;
            }
            tree[0] = cur;
        }
        !CMP_ERR.with(|c| c.get())
    }
}

/// The bytes of `run`'s table copy (`sort_open_res_var`'s), 0 without one.
/// # Safety
/// `run` is a live sort run.
pub unsafe fn table_bytes(run: *const SortRun) -> usize {
    let end = (run as *const u8).add(SORTRUN_HDR + (*run).cap as usize * (*run).row_bytes as usize);
    let [n, magic] = (end.sub(8) as *const [u32; 2]).read_unaligned();
    if magic == DESCRIBED { (4 * n as usize + 8).next_multiple_of(8) } else { 0 }
}

/// `dst`, emptied, takes `src`'s table copy at its top as `describe` lays one: a run exec v4 holds past its worker's region
/// keeps its shape (`RunShape::of_run`). False without a copy or the room.
/// # Safety
/// `dst` is a live emptied sort run, `src` a live sort run.
pub unsafe fn take_table(dst: *mut SortRun, src: *const SortRun) -> bool {
    let copy = table_bytes(src);
    let Some(top) = (copy != 0).then(|| (*dst).reserve_var(copy as u32)).flatten() else { return false };
    let end = (src as *const u8).add(SORTRUN_HDR + (*src).cap as usize * (*src).row_bytes as usize);
    core::ptr::copy_nonoverlapping(end.sub(copy), (dst as *mut u8).add(top as usize), copy);
    true
}

#[cfg(test)]
mod pulled_tests {
    use super::*;
    use gravity_abi::breaker::SORTRUN_HDR;

    /// One key word, one column: its datum word at 8, its NULL byte at 16, its 8-byte image at 24; no var section.
    const TBL: [u32; 9] = [48, 1, 1, 1, 0, 0, 0, 24, u32::MAX];

    /// Row `i` stored as the end stores it: the datum word 0, the NULL byte, the image.
    unsafe fn store(p: *mut u8, i: usize, null: bool) {
        let r = &mut *(p as *mut SortRun);
        let row = r.row(i) as *mut u8;
        *row.add(16) = null as u8;
        *(row.add(24) as *mut u64) = 100 + i as u64;
        r.len += 1;
    }

    unsafe fn pointed(r: &SortRun, i: usize) -> bool {
        *(r.row(i).add(8) as *const u64) == r.row(i).add(24) as u64
    }

    /// jmerge-0012: a filling run reads as stored so far, each row pointed at its image as the end stores it (a NULL cell
    /// left alone), the view taken between rows; a run whose rows moved (a growth's new region) is pointed again whole by
    /// the view; no region yet, or an unarmed one, is the empty run; a run past its capacity is the named error.
    #[test]
    fn a_filling_run_reads_its_rows_pointed_wherever_it_lies() {
        let mut buf = vec![0u64; (SORTRUN_HDR + 4 * 48) / 8]; // alloc: Cold
        let p = buf.as_mut_ptr() as *mut u8;
        let tbl = TBL.as_ptr();
        unsafe {
            *(p as *mut SortRun) = SortRun { cap: 4, len: 0, row_bytes: 48, pad: [0; 3], kind: TABLE_SORT };
            assert_eq!(live(p as *mut SortRun, tbl), p as *const u8, "a run with no row yet is itself");
            for i in 0..3 {
                store(p, i, i == 1);
                assert_eq!(SpoolPoint::run(core::ptr::null_mut(), (p as u64, tbl as u64, i as u64)), 0);
                let r = &*(live(p as *mut SortRun, tbl) as *const SortRun);
                assert_eq!(r.len as usize, i + 1, "the rows stored so far");
            }
            let r = &*(p as *const SortRun);
            assert!(pointed(r, 0) && pointed(r, 2));
            assert_eq!(*(r.row(1).add(8) as *const u64), 0, "a NULL cell is not pointed");
            // the rows moved (a growth reopened the region elsewhere): the view points them where they lie now
            let mut moved = buf.clone(); // alloc: Cold
            let q = moved.as_mut_ptr() as *mut u8;
            assert!(!pointed(&*(q as *const SortRun), 0), "a moved row's datum names its old place");
            let r = &*(live(q as *mut SortRun, tbl) as *const SortRun);
            assert!(pointed(r, 0) && pointed(r, 2), "pointed again where they lie");
            // a growth that reopened in place moved only the var copies: the end points every row again (`from` 0)
            *(r.row(2).add(8) as *mut u64) = 7;
            assert_eq!(SpoolPoint::run(core::ptr::null_mut(), (q as u64, tbl as u64, 0)), 0);
            assert!(pointed(r, 2));
            assert_eq!(live(core::ptr::null_mut(), tbl), SPOOL_EMPTY.as_ptr() as *const u8, "no region: the empty run");
            let mut unarmed = vec![0u64; 8]; // alloc: Cold
            assert_eq!(live(unarmed.as_mut_ptr() as *mut SortRun, tbl), SPOOL_EMPTY.as_ptr() as *const u8, "cap 0: the empty run");
            (*(q as *mut SortRun)).len = 5;
            assert!((live(q as *mut SortRun, tbl) as i64) < 0, "past its capacity: the named error");
        }
    }
}

#[cfg(test)]
mod span_tests {
    use super::*;
    use crate::host_abi::{SORT_TBL_HDR, VAR_TBL};

    const NVAR: usize = 2;
    const RB: usize = 16 + 8 * NVAR;
    const CAP: usize = 16;

    /// A var run of `keys.len()` rows: key pair `[1, k]` at 0 (one key, no spec), then `NVAR` var slots, each a 16-byte copy
    /// reserved as `image_into` reserves it (from the region's top down, row by row) holding `100 k + c`; and its table.
    fn var_run(keys: &[u64]) -> (Vec<u64>, Vec<u32>) {
        let mut buf = vec![0u64; (SORTRUN_HDR + CAP * RB) / 8 + 4]; // alloc: Cold
        let run = buf.as_mut_ptr() as *mut u8;
        let mut tbl = vec![RB as u32, 2, NVAR as u32 | VAR_TBL, 0, 0, 0, NVAR as u32, 0]; // alloc: Cold
        (0..NVAR).for_each(|c| tbl.extend([c as u32, 16 + 8 * c as u32]));
        tbl.extend([0, 0]);
        assert_eq!(tbl.len(), SORT_TBL_HDR + 2 + 2 * NVAR + 2);
        unsafe {
            let r = &mut *(run as *mut SortRun);
            (r.cap, r.row_bytes) = (CAP as u32, RB as u32);
            r.clear();
            for &k in keys {
                push(run, k);
            }
        }
        (buf, tbl)
    }

    /// Row `len` written with key `k` and its copies (`reserve_var`), `len` bumped.
    unsafe fn push(run: *mut u8, k: u64) {
        let r = &mut *(run as *mut SortRun);
        let at = run.add(SORTRUN_HDR + r.len as usize * RB);
        (at as *mut u64).write_unaligned(1);
        (at.add(8) as *mut u64).write_unaligned(k);
        for c in 0..NVAR {
            let top = r.reserve_var(16).expect("room for a copy") as usize;
            let so = 16 + 8 * c;
            (at.add(so) as *mut u32).write_unaligned(8);
            (at.add(so + 4) as *mut u32).write_unaligned((top - (SORTRUN_HDR + r.len as usize * RB + so)) as u32);
            (run.add(top) as *mut u64).write_unaligned(100 * k + c as u64);
        }
        r.len += 1;
    }

    /// Each row's key and the values its copies hold (`100 k + c` when the slot still points at its own row's copy).
    unsafe fn rows(run: *const u8) -> Vec<(u64, Vec<u64>)> {
        let r = &*(run as *const SortRun);
        (0..r.len as usize).map(|i| {
            let at = run.add(SORTRUN_HDR + i * RB);
            let copies = (0..NVAR).map(|c| {
                let slot = at.add(16 + 8 * c);
                (slot.add((slot.add(4) as *const u32).read_unaligned() as usize) as *const u64).read_unaligned()
            }).collect(); // alloc: Cold
            ((at.add(8) as *const u64).read_unaligned(), copies)
        }).collect() // alloc: Cold
    }

    fn own(keys: &[u64]) -> Vec<(u64, Vec<u64>)> {
        keys.iter().map(|&k| (k, (0..NVAR as u64).map(|c| 100 * k + c).collect())).collect() // alloc: Cold
    }

    fn word(from: u64, n: u64, distinct: bool) -> u64 {
        from | n << 32 | if distinct { SPAN_DISTINCT } else { 0 }
    }

    unsafe fn cut(run: *mut u8, w: u64, tbl: &[u32]) -> u64 {
        SpanCut::run(core::ptr::null_mut(), (run as u64, w, tbl.as_ptr() as u64))
    }

    unsafe fn seen(run: *mut u8, w: u64, tbl: &[u32]) -> u64 {
        SpanSeen::run(core::ptr::null_mut(), (run as u64, w, tbl.as_ptr() as u64))
    }

    /// D2690: the open group (rows 2..8) cut to its first three distinct rows, ordered; the rows before it kept as they lie,
    /// every slot pointing at its own row's copy, and the dropped rows' copies freed (`var_top` past five rows' copies).
    #[test]
    fn a_span_cut_keeps_its_first_distinct_rows_and_frees_the_rest() {
        unsafe {
            let (mut buf, tbl) = var_run(&[5, 3, 9, 1, 3, 7, 3, 1]);
            let run = buf.as_mut_ptr() as *mut u8;
            assert_eq!(cut(run, word(2, 3, true), &tbl), 3);
            assert_eq!(rows(run), own(&[5, 3, 1, 3, 7]));
            let r = &*(run as *const SortRun);
            assert_eq!(r.var_top() as usize, SORTRUN_HDR + CAP * RB - 5 * NVAR * 16, "five rows' copies left");
            // without SPAN_DISTINCT the first three rows, equal ones too
            let (mut buf, tbl) = var_run(&[5, 3, 9, 1, 3, 7, 3, 1]);
            let run = buf.as_mut_ptr() as *mut u8;
            assert_eq!(cut(run, word(2, 3, false), &tbl), 3);
            assert_eq!(rows(run), own(&[5, 3, 1, 1, 3]));
            // fewer distinct rows than `k`: one of each kept
            let (mut buf, tbl) = var_run(&[4, 4, 2, 4, 2]);
            let run = buf.as_mut_ptr() as *mut u8;
            assert_eq!(cut(run, word(0, 3, true), &tbl), 2);
            assert_eq!(rows(run), own(&[2, 4]));
        }
    }

    /// D2690: past a cut that kept `k` rows, a stored row at or after the bound (row `from + k - 1`) is dropped and its
    /// copies freed (`var_top` back where it was before the row), and under `SPAN_DISTINCT` a row equal to a kept one; a
    /// row before the bound and unequal to every kept one stays. A fixed run compares its key words.
    #[test]
    fn a_row_the_cut_group_holds_already_is_dropped_as_it_is_stored() {
        unsafe {
            let (mut buf, tbl) = var_run(&[8, 6, 2, 9, 4, 4]);
            let run = buf.as_mut_ptr() as *mut u8;
            assert_eq!(cut(run, word(1, 3, true), &tbl), 3);
            assert_eq!(rows(run), own(&[8, 2, 4, 6]));
            let r = run as *mut SortRun;
            let top = (*r).var_top();
            // the cut's word: three rows, the group's first three (full)
            for k in [6, 7, 2, 4] {
                push(run, k);
                assert_eq!(seen(run, word(1, 2 * 3 + 1, true), &tbl), 1, "{k} is held or past the bound");
                assert_eq!(((*r).len, (*r).var_top()), (4, top));
            }
            push(run, 3);
            assert_eq!(seen(run, word(1, 2 * 3 + 1, true), &tbl), 0);
            assert_eq!(rows(run), own(&[8, 2, 4, 6, 3]));
            // not full: a row past the last kept one stays, an equal one is dropped
            push(run, 7);
            assert_eq!(seen(run, word(1, 2 * 3, true), &tbl), 0);
            push(run, 6);
            assert_eq!(seen(run, word(1, 2 * 3, true), &tbl), 1);
            // without SPAN_DISTINCT (one row per group): only the bound drops
            push(run, 2);
            assert_eq!(seen(run, word(1, 2 * 3 + 1, false), &tbl), 0);
            // a fixed run: the key words alone (its table has no var section)
            let mut fixed = vec![0u64; (SORTRUN_HDR + 4 * 16) / 8 + 2]; // alloc: Cold
            let f = fixed.as_mut_ptr() as *mut u8;
            let fr = &mut *(f as *mut SortRun);
            (fr.cap, fr.row_bytes, fr.len) = (4, 16, 3);
            for (i, k) in [2u64, 5, 5].into_iter().enumerate() {
                (f.add(SORTRUN_HDR + 16 * i) as *mut u64).write_unaligned(1);
                (f.add(SORTRUN_HDR + 16 * i + 8) as *mut u64).write_unaligned(k);
            }
            let ft = [16u32, 2, 0, 0, 0, 0];
            assert_eq!(seen(f, word(1, 2 + 1, false), &ft), 1);
            assert_eq!(fr.len, 2);
        }
    }
}
