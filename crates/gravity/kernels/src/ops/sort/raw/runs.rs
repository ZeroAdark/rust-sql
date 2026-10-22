//! The sort run hosts' raw half (ops/sort/runs.rs keeps the run flags and the resident cap): the resident seal and put,
//! the var table copy, the run order's index, the sealed run's compaction (`RunShape::compact`), road sort.keylane's radix
//! and sort.natural's skip. Moved unchanged from runs.rs (rule R6: the family's unsafe code lives in raw); runs.rs
//! re-exports the entries under their old paths.
use core::cmp::Ordering;

use gravity_abi::breaker::{prefetch_read, SortRun, SORTRUN_HDR};
use gravity_abi::program::{partial, SPILL_RUNS, SPILL_RUN_PUT, SPILL_SEAL_OVER};
use gravity_abi::ctx::cancel_pending;
use gravity_abi::status::{CANCELLED, ERROR};
use gravity_abi::Ctx;

use crate::ops::sort::cut::{CUT_EXACT, CUT_LENIENT, CUT_NATURAL};
use crate::ops::sort::entries::{RunShape, DESCRIBED};
use crate::ops::sort::polled::{Poll, Stopped, SORT_RUN};
use crate::ops::sort::rowsort::{polled_scratch, small_on, sort_by_order, sort_by_order_polled, Order, SMALL_SORT_ROWS};
use crate::host_abi::{full_var_by, last_big, open_big, want_begin, bare, cmp_failed, cmp_rows, hosted_procs, permute_rows, permute_small, tbl_shape, var_alloc, var_shape, Head, RowCmp, SortSpec, CMP_ERR, SEALED, SORT_TBL_HDR};
use crate::table::emit::{Road, Work};
use gravity_fn::arena::{Kind, OpArena, Owner};
use crate::util::spill;

use super::super::runs::{RUN_CUT, RUN_HEAP, RUN_KEYLANE, RUN_ORDERED};

/// `RunShape::compact`'s prefetch distance in rows (a row's copies at half of it, once the row's own line came).
const GATHER_AHEAD: usize = 16;

crate::contract::host! {
    /// Road `sort.resident`'s producer open (H26b, I31): `sort_open`'s run and full-run put (a worker past its share of the
    /// grant puts a run to its BufFile), but at the section end the worker's last run is sorted and sealed where it lies
    /// instead of put, when the slot's stream holds no run and the section's runs merged fit a region (`RESIDENT_MAX`, the
    /// leader's answer filed before the section end, D3259). Every put of the section's claims came before the section-end
    /// calls, a section-end call puts only when one did or the answer says past, so all workers take the same branch: all
    /// sealed (exec ops/sort/merge.rs merges them in place into the slot's run, which `sort_begin` reads as
    /// `SORT_MODE_MEM`), or all put (the stream's k-way merge, as `sort_open`'s). Returns the run, or at the section end
    /// null / a status below zero as the word.
    /// # Safety
    /// `ctx` is bound; its partial is the sort's share-armed region or its live run of `row_bytes` rows.
    pub unsafe fn sort_open_res(ctx: *mut Ctx, row_bytes: u64, nkey: u64, spec: u64) -> *mut u8 {
        guard!(ERROR as *mut u8, {
            let run = SortRun::at_share(partial(ctx), row_bytes as u32) as *mut SortRun;
            uncut(run);
            if (*ctx).claim.is_section_end() {
                let st = match resident(ctx) {
                    Err(e) => e,
                    Ok(true) => seal(ctx, run, nkey as usize, SortSpec(spec)),
                    Ok(false) => put_res(ctx, run, nkey as usize, SortSpec(spec)),
                };
                return st as *mut u8;
            }
            run as *mut u8
        })
    }
}

crate::contract::host! {
    /// D3027 (b): `sort_open_res` of a top-k producer (lowerv4 topk_rows.rs: no WITH TIES, no heap) whose bound is `n`
    /// (LIMIT + OFFSET, `i64::MAX` for none). At the section end the worker's last run is cut to its first `n` rows first
    /// (`flags`: the top's `CUT_NATURAL`), so each sealed run holds at most `n` rows: a top's runs at W > 1 stay in their
    /// regions (exec merge.rs merges them in place) where `resident` put every worker's run once `width` shares passed a
    /// region (cbgen C025 at W8: eight runs to the stream for 1000 rows; D3259 dropped that share test for every sort).
    /// # Safety
    /// As `sort_open_res`.
    pub unsafe fn sort_open_res_top(ctx: *mut Ctx, row_bytes: u64, nkey: u64, spec: u64, n: u64, flags: u64) -> *mut u8 {
        guard!(ERROR as *mut u8, {
            let run = SortRun::at_share(partial(ctx), row_bytes as u32) as *mut SortRun;
            uncut(run);
            if (*ctx).claim.is_section_end() {
                let (nkey, spec) = (nkey as usize, SortSpec(spec));
                let st = match cut_top(run, n, &mut |n, out| super::cut(ctx, run, nkey, &|a, b| cmp_rows(nkey / 2, spec, a, b), None, n, flags & CUT_NATURAL, out)) {
                    0 => match resident_top(ctx, n) {
                        Err(e) => e,
                        Ok(true) => seal(ctx, run, nkey, spec),
                        Ok(false) => put_res(ctx, run, nkey, spec),
                    },
                    e => e,
                };
                return st as *mut u8;
            }
            run as *mut u8
        })
    }
}

crate::contract::host! {
    /// D3027 (b): `sort_open_res_var` of a top-k producer, as `sort_open_res_top`: the section end's run cut to `n` rows
    /// (their copies re-laid) before `resident_top`. Copies are not bounded by `n`: a merge past a region is exec merge.rs's
    /// to put instead (`Resident::past_cap`).
    /// # Safety
    /// As `sort_open_res_var`.
    pub unsafe fn sort_open_res_var_top(ctx: *mut Ctx, tbl: *const u32, n: u64, flags: u64) -> *mut u8 {
        guard!(ERROR as *mut u8, {
            want_begin();
            let run = SortRun::at_share(partial(ctx), *tbl) as *mut SortRun;
            uncut(run);
            describe(run, tbl);
            if !(*ctx).claim.is_section_end() {
                return open_big(ctx, run);
            }
            // D1330: a big run in flight put first (never kept)
            if let Err(e) = last_big(ctx, run, tbl, put_var_res_at) {
                return e as *mut u8;
            }
            let (Some(v), cmp) = (var_shape(tbl), RowCmp::of(tbl)) else { return bare("a var sort without its var table") as *mut u8 };
            let mode = match cmp.order() {
                Order::Exact => CUT_EXACT,
                Order::Lenient => CUT_LENIENT,
                Order::Images => 0,
            };
            CMP_ERR.with(|c| c.set(false));
            let st = cut_top(run, n, &mut |n, out| super::cut(ctx, run, tbl_shape(tbl).1, &|a, b| cmp.cmp(a, b), Some(v), n, flags & CUT_NATURAL | mode, out));
            if CMP_ERR.with(|c| c.get()) {
                return cmp_failed("a sort compare failed") as *mut u8;
            }
            let st = match (st, resident_top(ctx, n), RunShape::of_run(run)) {
                (e, _, _) if e < 0 => e,
                (_, Err(e), _) => e,
                (_, Ok(true), Some(_)) => seal_var(ctx, run, tbl),
                _ => put_var_res(ctx, run, tbl),
            };
            st as *mut u8
        })
    }
}

/// D3027 (b): the section end's cut of a top's run to its first `n` rows, only when it holds more (the seal orders the
/// rest): 0, or a status below zero.
unsafe fn cut_top(run: *mut SortRun, n: u64, cut: &mut dyn FnMut(u64, *mut u64) -> i64) -> i64 {
    if ((*run).len as u64) <= n {
        return 0;
    }
    let mut out = [0u64; 2];
    cut(n, out.as_mut_ptr()).min(0)
}

crate::contract::host! {
    /// `sort_full` of road `sort.keylane` / `sort.natural`: the full run ordered by its `pad[2]` bits and put.
    /// # Safety
    /// As `sort_full`.
    pub unsafe fn sort_full_res(ctx: *mut Ctx, run: *mut u8, nkey: u64, spec: u64) -> i64 {
        guard!(ERROR, put_res(ctx, run as *mut SortRun, nkey as usize, SortSpec(spec)))
    }
}

crate::contract::host! {
    /// `sort_last` of those roads: a serial producer's last run, kept sealed in the region when nothing was put (`sort_last`).
    /// # Safety
    /// As `sort_last`.
    pub unsafe fn sort_last_res(ctx: *mut Ctx, run: *mut u8, nkey: u64, spec: u64) -> i64 {
        let (run, nkey, spec) = (run as *mut SortRun, nkey as usize, SortSpec(spec));
        guard!(ERROR, last(ctx, || seal(ctx, run, nkey, spec), || put_res(ctx, run, nkey, spec)))
    }
}

/// A last run sealed in place when the producer is serial and nothing was put, else put.
unsafe fn last(ctx: *mut Ctx, seal: impl FnOnce() -> i64, put: impl FnOnce() -> i64) -> i64 {
    match spill(ctx, SPILL_RUNS, 0) {
        n if n < 0 => n,
        0 if (*ctx).width == 1 => seal(),
        _ => put(),
    }
}

unsafe fn seal(ctx: *mut Ctx, run: *mut SortRun, nkey: usize, spec: SortSpec) -> i64 {
    if (*run).len != 0 {
        let st = order(ctx, run, nkey, spec, Road::Seal);
        if st < 0 {
            return st;
        }
        (*run).pad[0] = SEALED;
    }
    0
}

unsafe fn put_res(ctx: *mut Ctx, run: *mut SortRun, nkey: usize, spec: SortSpec) -> i64 {
    if (*run).len == 0 {
        return 0;
    }
    let st = order(ctx, run, nkey, spec, Road::Put);
    if st < 0 {
        return st;
    }
    put(ctx, run)
}

/// An emptied run holds no cut rows (D508: a stale `RUN_CUT` would name a row no cut ordered).
unsafe fn uncut(run: *mut SortRun) {
    if (*run).len == 0 {
        (*run).pad[2] &= !(RUN_CUT | RUN_HEAP);
    }
}

/// The ordered run put to the slot's stream and cleared.
unsafe fn put(ctx: *mut Ctx, run: *mut SortRun) -> i64 {
    put_at(ctx, run, 0)
}

/// `put` of the run at `region` (`SPILL_RUN_PUT`'s arg).
unsafe fn put_at(ctx: *mut Ctx, run: *mut SortRun, region: i64) -> i64 {
    let st = spill(ctx, SPILL_RUN_PUT, region);
    if st >= 0 {
        (*run).clear();
        (*run).pad[2] &= !(RUN_CUT | RUN_HEAP);
    }
    st.min(0)
}

/// The run's rows in key order: kept as they are (`RUN_ORDERED`), by key 0's radix (`RUN_KEYLANE`), else compared. D4373:
/// permuted through a copy in the grant's room, else in place (`permute_rows`), its index a block of the grant; 0, or a status below zero.
unsafe fn order(ctx: *mut Ctx, run: *mut SortRun, nkey: usize, spec: SortSpec, road: Road) -> i64 {
    order_by(ctx, run, nkey, spec, road, small_on())
}

/// `order` with the small road (`small_index`, then the rows permuted on the stack: `permute_small`) on or off.
unsafe fn order_by(ctx: *mut Ctx, run: *mut SortRun, nkey: usize, spec: SortSpec, road: Road, small: bool) -> i64 {
    let cmp = |a: *const u8, b: *const u8| cmp_rows(nkey / 2, spec, a, b);
    let pending = || !ctx.is_null() && cancel_pending(ctx);
    if small && small_len(run) {
        let mut at = [0u32; SMALL_SORT_ROWS];
        if let Some(n) = small_index(run, nkey, &cmp, Order::Images, &mut at) {
            if !permute_small(run, &at[..n], None) && permute_rows(ctx, run, &|j| at[j] as usize, None, &pending) {
                return CANCELLED;
            }
            return 0;
        }
    }
    let idx = match index(ctx, run, nkey, &cmp, Order::Images, road) {
        Ok(Some(idx)) => idx,
        Ok(None) => return 0,
        Err(e) => return e,
    };
    if permute_rows(ctx, run, &|j| idx[j] as usize, None, &pending) {
        return CANCELLED;
    }
    0
}

/// The rows' key order as indexes; `None` when they stand as they are (`RUN_ORDERED`, no key, under two rows). `order`
/// (`RowCmp::order`): `Exact` (USEROP, `RowCmp::exact`) is PostgreSQL's qsort over the rows in arrival order
/// (`sort_exact`), never key 0's radix; `Lenient` (TOTAL-ORDER: a key ordered through fmgr) compares by PostgreSQL's qsort
/// wherever std's sort would, so an order that is not total never panics.
/// D4373 (b): the index is a block of the statement's grant (`Work`; the allocator's under the memory policy's floor):
/// `Err` when the grant refuses it. D4411: the order is a polled step (`sort_by_order_polled`, its scratch a block of the
/// grant too; key 0's radix ticks per pass and per `SORT_RUN` rows): `Err(CANCELLED)` when the statement's abort is pending.
pub(crate) unsafe fn index(ctx: *mut Ctx, run: *mut SortRun, nkey: usize, cmp: &dyn Fn(*const u8, *const u8) -> Ordering, order: Order, road: Road) -> Result<Option<Work<u32>>, i64> {
    let exact = order == Order::Exact;
    let (bits, n, rb) = ((*run).pad[2], (*run).len as usize, (*run).row_bytes as usize);
    // a heap top's run holding PostgreSQL's bounded heap (an exact top's, or ROW-TOPN's: lowerv4 topk_rows.rs `heap_key`):
    // `sort_bounded_heap`'s order (cut.rs), its heap taken whatever the run holds
    if bits & RUN_HEAP != 0 {
        if let Some(idx) = super::heap_order(ctx, run, &|a, b| cmp(a, b)) {
            return Ok(Some(idx));
        }
    }
    if bits & RUN_ORDERED != 0 || nkey == 0 || n < 2 {
        return Ok(None);
    }
    let base = (*run).row(0) as *const u8;
    let row = |i: u32| base.add(i as usize * rb);
    let pending = || !ctx.is_null() && cancel_pending(ctx);
    let mut poll = Poll::new(&pending);
    if exact || bits & RUN_KEYLANE == 0 {
        let mut idx = Work::from_fn_on(ctx, n, |i| i as u32, road)?;
        let mut scratch = Work::<u32>::block_on(ctx, polled_scratch(n, order), road)?;
        sort_by_order_polled(&mut poll, &mut idx, &mut scratch, order, &|a, b| cmp(row(a), row(b))).map_err(stopped)?;
        return Ok(Some(idx));
    }
    let key = |i: u32| (row(i) as *const [u64; 2]).read_unaligned();
    // LEDGER2 step 5: the radix pass's two arrays are the allocator's road (`Work::scratch`: from `ARENA_SCRATCH_MIN` bytes
    // on, a scratch arena under the slot's owner)
    let mut a = Work::scratch(ctx, n, |i| ((key(i as u32)[0] as u128) << 64 | key(i as u32)[1] as u128, i as u32), false).expect("an infallible scratch");
    let (and, or) = a.iter().fold((u128::MAX, 0u128), |(x, y), e| (x & e.0, y | e.0));
    let mut b = Work::scratch(ctx, n, |i| a[i], false).expect("an infallible scratch");
    for sh in (0..128).step_by(8).filter(|sh| (and ^ or) >> sh & 0xff != 0) {
        let mut at = [0usize; 257];
        for part in a.chunks(SORT_RUN) {
            poll.tick().map_err(stopped)?;
            part.iter().for_each(|e| at[(e.0 >> sh & 0xff) as usize + 1] += 1);
        }
        (0..256).for_each(|d| at[d + 1] += at[d]);
        for part in a.chunks(SORT_RUN) {
            poll.tick().map_err(stopped)?;
            for &e in part {
                let d = (e.0 >> sh & 0xff) as usize;
                (b[at[d]], at[d]) = (e, at[d] + 1);
            }
        }
        core::mem::swap(&mut a, &mut b);
    }
    // a tie on key 0's words: its image tail and the later keys decide (the radix's spare buffer is the polled sort's scratch)
    for t in a.chunk_by_mut(|x, y| x.0 == y.0).filter(|t| t.len() > 1) {
        let scratch = &mut b[..polled_scratch(t.len(), order)];
        sort_by_order_polled(&mut poll, t, scratch, order, &|x, y| cmp(row(x.1), row(y.1))).map_err(stopped)?;
    }
    Ok(Some(Work::from_fn_on(ctx, n, |i| a[i].1, road)?))
}

/// D4411: a polled step's stop is the host entry's `CANCELLED`.
fn stopped(_: Stopped) -> i64 {
    CANCELLED
}

/// A run the small road may order: 2 to `SMALL_SORT_ROWS` rows.
unsafe fn small_len(run: *const SortRun) -> bool {
    (2..=SMALL_SORT_ROWS).contains(&((*run).len as usize))
}

/// The small road's `index` (`small_on`): a keyed run of 2 to `SMALL_SORT_ROWS` rows, neither kept as it stands
/// (`RUN_ORDERED`) nor a heap top's (`RUN_HEAP`), ordered into `at` (row `j` of the order is row `at[j]`) on the stack: no
/// grant block, no poll. The order is `index`'s: `sort_by_order` is the sort `sort_by_order_polled` runs, and key 0's
/// radix is stable, so its order is the sort of the `(pair, row)` words. `None`: the run is `index`'s. Out of line: its
/// stack arrays stay off its callers' frames.
#[inline(never)]
unsafe fn small_index(run: *mut SortRun, nkey: usize, cmp: &dyn Fn(*const u8, *const u8) -> Ordering, order: Order, at: &mut [u32; SMALL_SORT_ROWS]) -> Option<usize> {
    let (bits, n, rb) = ((*run).pad[2], (*run).len as usize, (*run).row_bytes as usize);
    if bits & (RUN_HEAP | RUN_ORDERED) != 0 || nkey == 0 || n < 2 || n > SMALL_SORT_ROWS {
        return None;
    }
    let base = (*run).row(0);
    let row = |i: u32| base.add(i as usize * rb);
    let at = &mut at[..n];
    if order == Order::Exact || bits & RUN_KEYLANE == 0 {
        at.iter_mut().enumerate().for_each(|(i, x)| *x = i as u32);
        sort_by_order(at, order, &|a, b| cmp(row(a), row(b)));
        return Some(n);
    }
    let mut words = [core::mem::MaybeUninit::<(u128, u32)>::uninit(); SMALL_SORT_ROWS];
    for (i, w) in words[..n].iter_mut().enumerate() {
        let k = (row(i as u32) as *const [u64; 2]).read_unaligned();
        w.write(((k[0] as u128) << 64 | k[1] as u128, i as u32));
    }
    // SAFETY: the first `n` words are written above
    let words = core::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut (u128, u32), n);
    words.sort_unstable();
    for t in words.chunk_by_mut(|x, y| x.0 == y.0).filter(|t| t.len() > 1) {
        sort_by_order(t, order, &|x, y| cmp(row(x.1), row(y.1)));
    }
    at.iter_mut().zip(words.iter()).for_each(|(x, w)| *x = w.1);
    Some(n)
}

/// `index_by`'s small road: where `index_by` orders `Head`s, the heads built and ordered on the stack; else `small_index`.
#[inline(never)]
unsafe fn small_index_by(run: *mut SortRun, nkey: usize, rc: &RowCmp, at: &mut [u32; SMALL_SORT_ROWS]) -> Option<usize> {
    let (bits, n) = ((*run).pad[2], (*run).len as usize);
    if bits & (RUN_ORDERED | RUN_KEYLANE | RUN_HEAP) != 0 || nkey == 0 || n < 2 || !rc.heads_ok() || rc.exact() {
        return small_index(run, nkey, &|a, b| rc.cmp(a, b), rc.order(), at);
    }
    if n > SMALL_SORT_ROWS {
        return None;
    }
    let (base, rb) = ((*run).row(0), (*run).row_bytes as usize);
    let row = |i: u32| base.add(i as usize * rb);
    let mut heads = [core::mem::MaybeUninit::<Head>::uninit(); SMALL_SORT_ROWS];
    for (i, h) in heads[..n].iter_mut().enumerate() {
        h.write(rc.head(row(i as u32), i as u32));
    }
    // SAFETY: the first `n` heads are written above
    let hs = core::slice::from_raw_parts_mut(heads.as_mut_ptr() as *mut Head, n);
    sort_by_order(hs, rc.order(), &|x, y| rc.head_cmp(&x, &y, &row));
    at[..n].iter_mut().zip(hs.iter()).for_each(|(x, h)| *x = h.i);
    Some(n)
}

/// D1581 (G4): `index` of a var run by `rc`; a compared run whose key 0 is a by-reference key orders `Head`s (key 0's
/// flags, word and payload address, PostgreSQL's SortTuple `datum1`), so a compare reads the two payloads, not the two
/// rows and their slots first; a tie of key 0 compares the rows from key 1. The heads are the arena's (charged as its
/// chunks are: the free-memory preference, 09-25) for this call alone: built over the live run, dropped before it is put
/// or sealed (never a pointer across a spill), their bytes given back to the arena after. Without arena room: `index`.
/// D4411: the heads are built and ordered as a polled step (`sort_by_order_polled`, its scratch a block of the grant):
/// `Err(CANCELLED)` when the statement's abort is pending, the heads' bytes given back.
unsafe fn index_by(ctx: *mut Ctx, run: *mut SortRun, nkey: usize, rc: &RowCmp, road: Road) -> Result<Option<Work<u32>>, i64> {
    let (bits, n) = ((*run).pad[2], (*run).len as usize);
    if bits & (RUN_ORDERED | RUN_KEYLANE | RUN_HEAP) != 0 || nkey == 0 || n < 2 || !rc.heads_ok() || rc.exact() {
        return index(ctx, run, nkey, &|a, b| rc.cmp(a, b), rc.order(), road);
    }
    let heads = gravity_abi::ctx::arena_alloc(ctx, n * core::mem::size_of::<Head>()) as *mut Head;
    if heads.is_null() {
        return index(ctx, run, nkey, &|a, b| rc.cmp(a, b), rc.order(), road);
    }
    let (base, rb, top) = ((*run).row(0), (*run).row_bytes as usize, (*run).var_top());
    let row = |i: u32| base.add(i as usize * rb);
    let pending = || cancel_pending(ctx);
    let mut poll = Poll::new(&pending);
    let order = rc.order();
    let ordered = (0..n)
        .try_for_each(|i| {
            poll.batch(i)?;
            heads.add(i).write(rc.head(row(i as u32), i as u32));
            Ok::<(), Stopped>(())
        })
        .map_err(stopped)
        .and_then(|()| {
            let hs = core::slice::from_raw_parts_mut(heads, n);
            let mut scratch = Work::<Head>::block_on(ctx, polled_scratch(n, order), road)?;
            sort_by_order_polled(&mut poll, hs, &mut scratch, order, &|x, y| rc.head_cmp(&x, &y, &row)).map_err(stopped)?;
            drop(scratch);
            // the payloads the heads point at are the run's: it neither grew nor moved while they were ordered
            debug_assert!((*run).len as usize == n && (*run).var_top() == top && (*run).row(0) == base, "a run moved under its heads");
            Work::from_fn_on(ctx, n, |i| hs[i].i, road)
        });
    (*ctx).arena_cur = gravity_abi::word(heads as *mut u8);
    ordered.map(Some)
}

/// `put_var`'s compacted put (host_abi.rs) in `index`'s order.
unsafe fn put_var_res(ctx: *mut Ctx, run: *mut SortRun, tbl: *const u32) -> i64 {
    put_var_res_at(ctx, run, tbl, 0)
}

/// `put_var_res` of the run at `region` (`SPILL_RUN_PUT`'s arg: 0 the own region, else a D1330 big run's address).
unsafe fn put_var_res_at(ctx: *mut Ctx, run: *mut SortRun, tbl: *const u32, region: i64) -> i64 {
    let (Some(v), cmp) = (var_shape(tbl), RowCmp::of(tbl)) else { return bare("a var sort without its var table") };
    if (*run).len == 0 {
        return 0;
    }
    CMP_ERR.with(|c| c.set(false));
    let mut at = [0u32; SMALL_SORT_ROWS];
    let small = if small_on() && small_len(run) { small_index_by(run, tbl_shape(tbl).1, &cmp, &mut at) } else { None };
    let idx = if small.is_some() { Ok(None) } else { index_by(ctx, run, tbl_shape(tbl).1, &cmp, Road::Put) };
    if CMP_ERR.with(|c| c.get()) {
        return cmp_failed("a sort compare failed");
    }
    let idx = match idx {
        Ok(idx) => idx,
        Err(e) => return e,
    };
    let order = small.map_or(idx.as_deref(), |n| Some(&at[..n]));
    if let Err(e) = (RunShape { cmp, rb: (*run).row_bytes as usize, var: Some(v) }.compact_in(ctx, run, order)) {
        return e;
    }
    super::declare_rows(ctx, (*run).row_bytes, &v);
    put_at(ctx, run, region)
}

impl RunShape {
    /// A var run's rows compacted in place for its put (`put_var`'s form, `idx`'s order or as they lie): each row followed
    /// by its var copies, its slots re-pointed, its record word the record's bytes; the run's var bytes the records'.
    /// D4413: `pending` (the caller's poll of its statement's cancel) is read every `POLL_BATCH` rows; true = it stopped,
    /// the run as it lay.
    /// # Safety
    /// `run` is a live sort run of this shape.
    pub unsafe fn compact(&self, run: *mut SortRun, idx: Option<&[u32]>, pending: &dyn Fn() -> bool) -> bool {
        self.compact_for(run, idx, None, pending)
    }

    /// `compact`, its copy the allocator's (no ctx draws it: exec's merge holds its pool bytes around the call,
    /// `put_form`), charged to `owner` (the sort's slot owner) from `ARENA_SCRATCH_MIN` bytes on (`SortCopy::allocator_of`).
    /// # Safety
    /// As `compact`.
    pub unsafe fn compact_for(&self, run: *mut SortRun, idx: Option<&[u32]>, owner: Option<Owner>, pending: &dyn Fn() -> bool) -> bool {
        if self.var.is_none() || (*run).len == 0 {
            return false;
        }
        self.compact_through(run, idx, &mut SortCopy::allocator_of(owner, self.compact_bytes(run)), pending)
    }

    /// The bytes `compact` copies through: the rows' and the var region's (from its top to the rows' end), which the
    /// records never outgrow.
    /// # Safety
    /// `run` is a live sort run of this shape.
    pub unsafe fn compact_bytes(&self, run: *mut SortRun) -> usize {
        let (n, rb) = ((*run).len as usize, self.rb);
        if self.var.is_none() || n == 0 {
            return 0;
        }
        n * rb + (SORTRUN_HDR + (*run).cap as usize * rb).saturating_sub((*run).var_top() as usize)
    }

    /// `compact`, its copy a block of `ctx`'s grant (D4373 (b): `Work`; the allocator's on a null ctx or under the memory
    /// policy's floor); `Err` (the run untouched) when the grant refuses it. D4413: `Err(CANCELLED)` (the run untouched)
    /// when the statement's abort is pending.
    /// # Safety
    /// `run` is a live sort run of this shape; `ctx` is null or bound.
    pub unsafe fn compact_in(&self, ctx: *mut Ctx, run: *mut SortRun, idx: Option<&[u32]>) -> Result<(), i64> {
        self.compact_on(ctx, run, idx, &|| !ctx.is_null() && cancel_pending(ctx))
    }

    /// `compact_in` under the caller's `pending`.
    unsafe fn compact_on(&self, ctx: *mut Ctx, run: *mut SortRun, idx: Option<&[u32]>, pending: &dyn Fn() -> bool) -> Result<(), i64> {
        if self.var.is_none() || (*run).len == 0 {
            return Ok(());
        }
        let mut tmp = SortCopy::new(ctx, self.compact_bytes(run), Road::Put)?;
        if self.compact_through(run, idx, &mut tmp, pending) {
            return Err(CANCELLED);
        }
        Ok(())
    }

    /// `compact` through the caller's `compact_bytes(run)` bytes at `dst` (exec's copy past the pool: a mapped temp file);
    /// true = `pending` stopped it, the run as it lay.
    /// # Safety
    /// `run` is a live sort run of this shape; `dst` holds `compact_bytes(run)` writable bytes no one else reads.
    pub unsafe fn compact_at(&self, run: *mut SortRun, idx: Option<&[u32]>, dst: *mut u8, pending: &dyn Fn() -> bool) -> bool {
        self.var.is_some() && (*run).len != 0 && self.compact_through(run, idx, &mut SortCopy::at(dst, self.compact_bytes(run)), pending)
    }

    /// `compact`'s body through `tmp` (sized `compact_bytes(run)`). D4413: a polled step (polled.rs), `pending` read every
    /// `POLL_BATCH` rows; true = it stopped before the copy went back, the run as it lay.
    unsafe fn compact_through(&self, run: *mut SortRun, idx: Option<&[u32]>, tmp: &mut SortCopy, pending: &dyn Fn() -> bool) -> bool {
        let (n, rb, base) = ((*run).len as usize, self.rb, (*run).row(0) as *mut u8);
        let Some(v) = self.var.filter(|_| n != 0) else { return false };
        let row_at = |i: usize| base.add(idx.map_or(i, |x| x[i] as usize) * rb);
        let mut poll = Poll::new(pending);
        for i in 0..n {
            if poll.batch(i).is_err() {
                return true;
            }
            // `idx`'s order gathers a row and its copies from anywhere in the run, a miss each (m5's 4 MB sort spent 18%
            // of its time here): the row `GATHER_AHEAD` on is prefetched, and the copies of the row half that on
            if idx.is_some() {
                if i + GATHER_AHEAD < n {
                    let r = row_at(i + GATHER_AHEAD);
                    prefetch_read(r);
                    prefetch_read(r.add(rb - 1));
                }
                if i + GATHER_AHEAD / 2 < n {
                    let r = row_at(i + GATHER_AHEAD / 2);
                    for c in 0..v.nvar {
                        let so = *v.slots.add(2 * c + 1) as usize;
                        let size = (r.add(so) as *const u32).read_unaligned();
                        if size != 0 {
                            let p = r.add(so + (r.add(so + 4) as *const u32).read_unaligned() as usize);
                            prefetch_read(p);
                            prefetch_read(p.add(var_alloc(size) as usize - 1));
                        }
                    }
                }
            }
            let row = row_at(i);
            let at = tmp.len();
            tmp.extend_from_slice(core::slice::from_raw_parts(row, rb));
            for c in 0..v.nvar {
                let so = *v.slots.add(2 * c + 1) as usize;
                let size = (row.add(so) as *const u32).read_unaligned();
                if size != 0 {
                    let (off, len) = ((row.add(so + 4) as *const u32).read_unaligned() as usize, var_alloc(size) as usize);
                    let acc = tmp.len() - at;
                    (tmp.as_mut_ptr().add(at + so + 4) as *mut u32).write_unaligned((acc - so) as u32);
                    tmp.extend_from_slice(core::slice::from_raw_parts(row.add(so + off), len));
                }
            }
            // D2572-c: the record word whole (its high half was never written) and the null bytes' padding zero
            let rec = (tmp.len() - at) as u64;
            (tmp.as_mut_ptr().add(at + v.rec_off) as *mut u64).write_unaligned(rec);
            tmp.written(at + v.pad.0..at + v.pad.1).fill(0);
        }
        core::ptr::copy_nonoverlapping(tmp.as_ptr(), base, tmp.len());
        (*run).set_var_bytes(tmp.len() as u32);
        false
    }
}

/// D4373 (b): a copy's bytes in a block of the statement's grant (`Work::granted`), else the allocator's `Vec` as before
/// (a null ctx, under the memory policy's floor); filled front to back, never past the capacity it was sized for.
pub(crate) struct SortCopy {
    grant: Option<Work<u8>>,
    heap: Vec<u8>,
    // the caller's bytes (`RunShape::compact_at`: exec's copy in a mapped temp file), `cap` long; null: none
    ext: *mut u8,
    cap: usize,
    len: usize,
    // LEDGER2 step 5: the scratch arena `ext` was carved from (`allocator`), this copy's alone: dropped with it, after the
    // fields above
    scratch: Option<OpArena>,
}

impl SortCopy {
    /// # Safety
    /// `ctx` is null or bound.
    pub(crate) unsafe fn new(ctx: *mut Ctx, cap: usize, road: Road) -> Result<SortCopy, i64> {
        let grant = if ctx.is_null() { None } else { Work::granted(ctx, cap, road)? };
        if grant.is_none() {
            return Ok(SortCopy::allocator(ctx, cap));
        }
        Ok(SortCopy { grant, heap: Vec::new(), ext: core::ptr::null_mut(), cap, len: 0, scratch: None }) // alloc: Exec
    }

    /// The allocator's road (a null ctx, under the memory policy's floor, or a copy that never takes the grant: a heap or
    /// span cut's relay). LEDGER2 step 5: from `ARENA_SCRATCH_MIN` bytes on, carved from a scratch arena of this call under
    /// the breaker slot `ctx`'s pipeline runs for (`OpArena::scratch`: the allocator's chunks, charged to that owner once);
    /// else (smaller, no host, no slot owner, the carve refused) the allocator's `Vec` as before. Never refused.
    /// # Safety
    /// `ctx` is null or bound.
    pub(crate) unsafe fn allocator(ctx: *mut Ctx, cap: usize) -> SortCopy {
        let a = if !ctx.is_null() && cap >= gravity_abi::program::ARENA_SCRATCH_MIN { OpArena::scratch(ctx, Kind::Sort) } else { None };
        SortCopy::carved(a, cap)
    }

    /// CONVERT wave 2: `allocator` with no ctx in reach (exec's put of a resident run, `RunShape::compact_for`): from
    /// `ARENA_SCRATCH_MIN` bytes on, carved from a scratch arena charged to `owner` itself (the sort's slot owner,
    /// `OpArena::scratch_of`: the allocator's chunks); with no owner, the allocator's `Vec` as before. Never refused.
    pub(crate) fn allocator_of(owner: Option<Owner>, cap: usize) -> SortCopy {
        SortCopy::carved(owner.filter(|_| cap >= gravity_abi::program::ARENA_SCRATCH_MIN).map(|o| OpArena::scratch_of(Some(o), Kind::Sort)), cap)
    }

    /// `cap` bytes carved from `a`, else (no arena, the carve refused) the allocator's `Vec`.
    fn carved(a: Option<OpArena>, cap: usize) -> SortCopy {
        if let Some(a) = a {
            if let Ok(p) = core::alloc::Layout::from_size_align(cap, 8).map_err(|_| ()).and_then(|l| a.alloc(l).map_err(|_| ())) {
                return SortCopy { grant: None, heap: Vec::new(), ext: p.as_ptr(), cap, len: 0, scratch: Some(a) }; // alloc: Exec
            }
        }
        SortCopy { grant: None, heap: Vec::with_capacity(cap), ext: core::ptr::null_mut(), cap, len: 0, scratch: None } // alloc: Exec
    }

    /// A copy through the caller's `cap` bytes at `ext`.
    pub(crate) fn at(ext: *mut u8, cap: usize) -> SortCopy {
        SortCopy { grant: None, heap: Vec::new(), ext, cap, len: 0, scratch: None } // alloc: Exec
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn as_ptr(&self) -> *const u8 {
        if !self.ext.is_null() {
            return self.ext;
        }
        match &self.grant {
            Some(w) => w.as_ptr(),
            None => self.heap.as_ptr(),
        }
    }

    pub(crate) fn as_mut_ptr(&mut self) -> *mut u8 {
        if !self.ext.is_null() {
            return self.ext;
        }
        match &mut self.grant {
            Some(w) => w.as_mut_ptr(),
            None => self.heap.as_mut_ptr(),
        }
    }

    pub(crate) fn extend_from_slice(&mut self, b: &[u8]) {
        if !self.ext.is_null() {
            assert!(self.len + b.len() <= self.cap, "a sort copy past the bytes it was sized for");
            // SAFETY: `at`'s caller lent `cap` bytes at `ext`, this copy's alone while it lives
            unsafe { core::ptr::copy_nonoverlapping(b.as_ptr(), self.ext.add(self.len), b.len()) };
            self.len += b.len();
            return;
        }
        match &mut self.grant {
            Some(w) => {
                let end = self.len + b.len();
                assert!(end <= w.len(), "a sort copy past the bytes it was sized for");
                w[self.len..end].copy_from_slice(b);
            }
            None => self.heap.extend_from_slice(b),
        }
        self.len += b.len();
    }

    /// Bytes `r` of what was written.
    pub(crate) fn written(&mut self, r: core::ops::Range<usize>) -> &mut [u8] {
        let len = self.len;
        // SAFETY: the first `len` bytes were written (`extend_from_slice`)
        unsafe { &mut core::slice::from_raw_parts_mut(self.as_mut_ptr(), len)[r] }
    }
}


crate::contract::host! {
    /// `sort_full_var` of roads sort.keylane / sort.natural: the full var run put in its `pad[2]` bits' order.
    /// # Safety
    /// As `sort_full_var`.
    pub unsafe fn sort_full_res_var(ctx: *mut Ctx, run: *mut u8, tbl: *const u32) -> i64 {
        // D1330: the big-run turns of `sort_full_var` (a big run described as the own run is)
        guard!(ERROR, full_var_by(ctx, run as *mut SortRun, tbl, put_var_res_at, describe))
    }
}

crate::contract::host! {
    /// `sort_last_var` of those roads: a serial producer's last var run, kept sealed in the region when nothing was put.
    /// # Safety
    /// As `sort_last_var`.
    pub unsafe fn sort_last_res_var(ctx: *mut Ctx, run: *mut u8, tbl: *const u32) -> i64 {
        let run = run as *mut SortRun;
        guard!(ERROR, {
            // D1330: a big run in flight put first (never kept)
            match last_big(ctx, run, tbl, put_var_res_at) {
                Err(e) => return e,
                Ok(true) => return 0,
                Ok(false) => {}
            }
            last(ctx, || seal_var(ctx, run, tbl), || put_var_res(ctx, run, tbl))
        })
    }
}

/// A var run sealed where it lies in `index`'s order, its slots re-pointed (host_abi.rs `permute_rows`).
unsafe fn seal_var(ctx: *mut Ctx, run: *mut SortRun, tbl: *const u32) -> i64 {
    seal_var_by(ctx, run, tbl, small_on())
}

/// `seal_var` with the small road (`small_index_by`, then `permute_small`) on or off.
unsafe fn seal_var_by(ctx: *mut Ctx, run: *mut SortRun, tbl: *const u32, small: bool) -> i64 {
    let cmp = RowCmp::of(tbl);
    CMP_ERR.with(|c| c.set(false));
    if small && small_len(run) {
        let mut at = [0u32; SMALL_SORT_ROWS];
        if let Some(n) = small_index_by(run, tbl_shape(tbl).1, &cmp, &mut at) {
            let pending = || !ctx.is_null() && cancel_pending(ctx);
            if !permute_small(run, &at[..n], var_shape(tbl)) && permute_rows(ctx, run, &|j| at[j] as usize, var_shape(tbl), &pending) {
                return CANCELLED;
            }
            if CMP_ERR.with(|c| c.get()) {
                return cmp_failed("a sort compare failed");
            }
            (*run).pad[0] = SEALED;
            return 0;
        }
    }
    match index_by(ctx, run, tbl_shape(tbl).1, &cmp, Road::Seal) {
        // D1580: the order is known; the rows move into it directly (no second sort by rank); D4373: `permute_rows`
        Ok(Some(idx)) => {
            let pending = || cancel_pending(ctx);
            if permute_rows(ctx, run, &|j| idx[j] as usize, var_shape(tbl), &pending) {
                return CANCELLED;
            }
        }
        Ok(None) => {}
        Err(e) => return e,
    }
    if CMP_ERR.with(|c| c.get()) {
        return cmp_failed("a sort compare failed");
    }
    if (*run).len != 0 {
        (*run).pad[0] = SEALED;
    }
    0
}

crate::contract::host! {
    /// `sort_open_res` for a var row (`sort_open_var`'s table): an emptied run first reserves a copy of the table at its
    /// rows' end, the merge's shape (`RunShape::of_run`).
    /// # Safety
    /// As `sort_open_res`; `tbl` is the sort's emit table.
    pub unsafe fn sort_open_res_var(ctx: *mut Ctx, tbl: *const u32) -> *mut u8 {
        guard!(ERROR as *mut u8, {
            want_begin();
            let run = SortRun::at_share(partial(ctx), *tbl) as *mut SortRun;
            uncut(run);
            describe(run, tbl);
            if !(*ctx).claim.is_section_end() {
                return open_big(ctx, run);
            }
            // D1330: a big run in flight put first (never kept)
            if let Err(e) = last_big(ctx, run, tbl, put_var_res_at) {
                return e as *mut u8;
            }
            let st = match (resident(ctx), RunShape::of_run(run)) {
                (Err(e), _) => e,
                (Ok(true), Some(_)) => seal_var(ctx, run, tbl),
                _ => put_var_res(ctx, run, tbl),
            };
            st as *mut u8
        })
    }
}

/// An emptied var run's table copy, reserved at its rows' end (a copy a put cleared is laid again); a share too small for
/// it takes none, the same for every worker, and the run is put.
/// # Safety
/// `run` is a live sort run of `tbl`'s rows.
pub unsafe fn describe(run: *mut SortRun, tbl: *const u32) {
    let end = SORTRUN_HDR as u32 + (*run).cap * (*run).row_bytes;
    if (*run).len == 0 && (*run).var_top() == end {
        let n = tbl_words(tbl);
        if let Some(top) = (*run).reserve_var((4 * n as u32 + 8).next_multiple_of(8)) {
            core::ptr::copy_nonoverlapping(tbl, (run as *mut u8).add(top as usize) as *mut u32, n);
            ((run as *mut u8).add(end as usize - 8) as *mut [u32; 2]).write_unaligned([n as u32, DESCRIBED]);
        }
    }
}

/// The section end's branch, the same for every worker: seal when no run is in the slot's stream (no worker put during
/// the section) and the section's runs, merged, fit one region. D3259: that last test was "`width` shares fit a region",
/// a test of the grant, not of the rows, so a W8 sort with a grant past ~4 GiB put every worker's run and merged on one
/// thread at any size (a 100 M-row, 4.0 GB sort: 1.9-2.1 s sealed at 4000 MiB, 5.2-5.5 s put at 7870 / 14000 MiB). Now
/// it is the leader's answer over the runs' real rows, filed before the section end (`SPILL_SEAL_OVER`: exec
/// ops/sort/merge.rs `seal_over`), so a merge past a region still puts each run on its own worker at once.
unsafe fn resident(ctx: *mut Ctx) -> Result<bool, i64> {
    match (no_runs_put(ctx)?, spill(ctx, SPILL_SEAL_OVER, 0)) {
        (_, w) if w < 0 => Err(w),
        // bit 0 the answer; the rows it read above it (D4373, `Stream::seal_word`)
        (sealed, w) => Ok(sealed && w & 1 == 0),
    }
}

/// D3027 (b): a top's branch (`sort_open_res_top`, its runs cut to `n` rows first): seal when no run is in the slot's
/// stream. The leader's `SPILL_SEAL_OVER` answer was read over the runs uncut, so a top whose cut drops rows does not take
/// it; a merge of its runs past a region is the leader's to put (exec merge.rs `past_cap`). D4373: a top whose bound `n`
/// is at least every row that answer read (filed beside it: no cut drops a row, as a top of every row) takes it as a
/// full sort does (`resident`): the runs merged past a region are put by each worker at once, never sealed four 2 GiB
/// runs at a time. The rows, the answer and `n` are the section's, so every worker takes the same branch.
unsafe fn resident_top(ctx: *mut Ctx, n: u64) -> Result<bool, i64> {
    if !no_runs_put(ctx)? {
        return Ok(false);
    }
    match spill(ctx, SPILL_SEAL_OVER, 0) {
        w if w < 0 => Err(w),
        w => Ok(!(w & 1 != 0 && ((w as u64) >> 1).checked_sub(1).is_some_and(|rows| rows <= n))),
    }
}

/// No run of the slot's stream is put yet (`SPILL_RUNS`).
unsafe fn no_runs_put(ctx: *mut Ctx) -> Result<bool, i64> {
    match spill(ctx, SPILL_RUNS, 0) {
        n if n < 0 => Err(n),
        n => Ok(n == 0),
    }
}

/// An emit table's words (host_abi.rs `SORT_TBL_HDR`): the header, the images, the var section with its key words, and
/// the hosted section's one word per key when any key is hosted (`KFLAG_HOSTED`).
unsafe fn tbl_words(tbl: *const u32) -> usize {
    let (_, nkey, _, _) = tbl_shape(tbl);
    let hosted = if hosted_procs(tbl).is_some() { nkey / 2 } else { 0 };
    SORT_TBL_HDR + 3 * *tbl.add(3) as usize + var_shape(tbl).map_or(0, |v| 2 + 2 * v.nvar + nkey) + hosted
}

#[cfg(test)]
mod small_road_tests {
    use super::*;
    use crate::host_abi::{sort_image, KFLAG_BPCHAR, VAR_TBL, VAR_WIDTH};
    use crate::ops::sort::rowsort::SMALL_SORT_ROW_BYTES;
    use core::cell::Cell;

    /// Both sides of the small road's bound, and runs past it.
    const SIZES: [usize; 10] = [0, 1, 2, 3, 37, SMALL_SORT_ROWS - 1, SMALL_SORT_ROWS, SMALL_SORT_ROWS + 1, 2 * SMALL_SORT_ROWS + 5, 700];
    /// Fixed rows: key 0's (flags, word) at 0 and its 8-byte image at 32, key 1's (flags, word) at 16, the row's tag at 40.
    const FIXED: u32 = 48;
    /// Var rows: key 0's (flags, word) at 0 and its text's slot at 32, key 1's at 16, the tag at 40, the record word at 48.
    const VAR: u32 = 56;

    fn next(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    unsafe fn open(region: &mut [u64], rb: u32) -> *mut SortRun {
        let p = region.as_mut_ptr() as *mut u8;
        (*(p as *mut SortRun)).row_bytes = (region.len() * 8) as u32;
        SortRun::at_share(p, rb) as *mut SortRun
    }

    unsafe fn tag(run: *const SortRun, i: usize) -> u64 {
        *((*run).row(i) as *const u64).add(5)
    }

    /// `n` fixed rows of `spread` values a key (few: many rows tie whole, told apart by their tags alone), a NULL in every
    /// ninth row (NULLS FIRST or LAST), key 0 DESC or not; `bits` the run's road bits.
    fn fixed(n: usize, spread: u64, nulls_first: bool, desc: bool, bits: u8, seed: u64) -> Vec<u64> {
        let mut reg = vec![0u64; ((n + 4) * FIXED as usize + 256) / 8]; // alloc: Cold
        let mut s = seed | 1;
        unsafe {
            let run = open(&mut reg, FIXED);
            for i in 0..n {
                let row = (*run).push() as *mut u64;
                let v = next(&mut s) % spread;
                let (f, w, img) = match i % 9 == 4 {
                    true => (if nulls_first { 0 } else { 2 }, 0, 0),
                    false => (1, if desc { !(v >> 2) } else { v >> 2 }, u64::from_ne_bytes(v.to_be_bytes())),
                };
                (*row, *row.add(1), *row.add(2), *row.add(3), *row.add(4), *row.add(5)) = (f, w, 1, next(&mut s) % 3, img, i as u64);
            }
            (*run).pad[2] |= bits;
        }
        reg
    }

    fn fixed_spec(desc: bool) -> SortSpec {
        SortSpec::pack(&[(Some((32, 8)), desc), (None, false)]).unwrap()
    }

    /// The var table of `VAR` rows: key 0 a text (bytes order; `bpchar`: without its trailing blanks), key 1 a word.
    fn var_tbl(desc: bool, bpchar: bool) -> [u32; 14] {
        let spec = SortSpec::pack(&[(Some((32, VAR_WIDTH)), desc), (None, false)]).unwrap().0;
        [VAR, 4, 3 | VAR_TBL, 0, spec as u32, (spec >> 32) as u32, 1, 48, 2, 32, 0, if bpchar { KFLAG_BPCHAR } else { 0 }, 0, 0]
    }

    /// `n` var rows: texts sharing their first eight bytes in half the rows (the words tie, the payloads decide), trailing
    /// blanks on some, few values (whole ties), a NULL in every seventh row; key 0's word its first eight bytes (a
    /// bpchar's without its trailing blanks), complemented under DESC.
    fn var_run(n: usize, desc: bool, bpchar: bool, nulls_first: bool, bits: u8, seed: u64) -> Vec<u64> {
        let mut reg = vec![0u64; ((n + 4) * (VAR as usize + 64) + 256) / 8]; // alloc: Cold
        let mut s = seed | 1;
        unsafe {
            let run = open(&mut reg, VAR);
            for i in 0..n {
                let p = (*run).rows_ptr().add((*run).len as usize * VAR as usize) as *mut u64;
                *p.add(6) = 0;
                let v = next(&mut s);
                let text = match v % 4 {
                    0 | 1 => format!("abcdefgh{}", v % 5), // alloc: Cold
                    2 => format!("{}{}", v % 7, " ".repeat((v % 3) as usize)), // alloc: Cold
                    _ => format!("k{}x", v % 11), // alloc: Cold
                };
                let null = i % 7 == 3;
                if !null {
                    assert_eq!(sort_image(run as *mut u8, text.as_ptr(), text.len() as u64, 32, 48), 0);
                }
                (*run).push();
                let b = if bpchar { text.trim_end_matches(' ').as_bytes() } else { text.as_bytes() };
                let mut w = [0u8; 8];
                w[..b.len().min(8)].copy_from_slice(&b[..b.len().min(8)]);
                let w = u64::from_be_bytes(w);
                let (f, w) = if null { (if nulls_first { 0 } else { 2 }, 0) } else { (1, if desc { !w } else { w }) };
                (*p, *p.add(1), *p.add(2), *p.add(3), *p.add(5)) = (f, w, 1, next(&mut s) % 2, i as u64);
            }
            (*run).pad[2] |= bits;
        }
        reg
    }

    /// A ctx with an arena (`index_by`'s heads) and no host: the grant's road is the allocator's.
    fn arena_ctx(arena: &mut [u64]) -> Ctx {
        // SAFETY: a ctx is plain words; zero is its closed state
        let mut ctx: Ctx = unsafe { core::mem::zeroed() };
        ctx.arena_cur = gravity_abi::word(arena.as_mut_ptr() as *mut u8);
        ctx.arena_end = gravity_abi::word(unsafe { (arena.as_mut_ptr() as *mut u8).add(arena.len() * 8) });
        ctx
    }

    unsafe fn assert_ordered(run: *const SortRun, cmp: &dyn Fn(*const u8, *const u8) -> Ordering, what: &str) {
        let n = (*run).len as usize;
        assert!((1..n).all(|i| cmp((*run).row(i - 1), (*run).row(i)) != Ordering::Greater), "{what}: ordered");
        let mut tags: Vec<u64> = (0..n).map(|i| tag(run, i)).collect();
        tags.sort_unstable();
        assert!(tags.iter().copied().eq(0..n as u64), "{what}: every row once");
    }

    #[test]
    fn a_fixed_runs_small_road_lays_the_bytes_the_full_road_lays() {
        for n in SIZES {
            for spread in [3u64, 1 << 40] {
                for (nulls_first, desc) in [(false, false), (true, false), (false, true), (true, true)] {
                    for bits in [0, RUN_KEYLANE] {
                        let what = format!("n {n} spread {spread} nulls_first {nulls_first} desc {desc} bits {bits}"); // alloc: Cold
                        let spec = fixed_spec(desc);
                        let (mut off, mut on) = (fixed(n, spread, nulls_first, desc, bits, 7 + n as u64), fixed(n, spread, nulls_first, desc, bits, 7 + n as u64));
                        unsafe {
                            let (a, b) = (off.as_mut_ptr() as *mut SortRun, on.as_mut_ptr() as *mut SortRun);
                            assert_eq!(order_by(core::ptr::null_mut(), a, 4, spec, Road::Seal, false), 0);
                            assert_eq!(order_by(core::ptr::null_mut(), b, 4, spec, Road::Seal, true), 0);
                            assert!(off == on, "{what}: the same bytes, ties in the same order");
                            assert_ordered(b, &|x, y| cmp_rows(2, spec, x, y), &what);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_var_runs_small_seal_lays_the_bytes_the_full_seal_lays() {
        for n in SIZES {
            for (desc, bpchar, nulls_first) in [(false, false, false), (true, false, true), (false, true, false), (true, true, false), (false, true, true)] {
                for bits in [0, RUN_KEYLANE] {
                    let what = format!("n {n} desc {desc} bpchar {bpchar} nulls_first {nulls_first} bits {bits}"); // alloc: Cold
                    let tbl = var_tbl(desc, bpchar);
                    let (mut off, mut on) = (var_run(n, desc, bpchar, nulls_first, bits, 3 + n as u64), var_run(n, desc, bpchar, nulls_first, bits, 3 + n as u64));
                    let mut arena = vec![0u64; 1 << 14]; // alloc: Cold
                    let mut ctx = arena_ctx(&mut arena);
                    unsafe {
                        let (a, b) = (off.as_mut_ptr() as *mut SortRun, on.as_mut_ptr() as *mut SortRun);
                        assert_eq!(seal_var_by(&mut ctx, a, tbl.as_ptr(), false), 0);
                        assert_eq!(seal_var_by(&mut ctx, b, tbl.as_ptr(), true), 0);
                        assert!(off == on, "{what}: the same bytes, slots re-pointed alike");
                        let rc = RowCmp::of(tbl.as_ptr());
                        assert_ordered(b, &|x, y| rc.cmp(x, y), &what);
                        assert_eq!((*b).pad[0], if n == 0 { 0 } else { SEALED }, "{what}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_small_index_is_indexs_order_under_every_order() {
        let spec = fixed_spec(false);
        for n in SIZES {
            for bits in [0, RUN_KEYLANE] {
                for order in [Order::Images, Order::Lenient, Order::Exact] {
                    let what = format!("n {n} bits {bits} {order:?}"); // alloc: Cold
                    let mut reg = fixed(n, 5, false, false, bits, 11 + n as u64);
                    let run = reg.as_mut_ptr() as *mut SortRun;
                    let calls = Cell::new(0usize);
                    let pair = |x: *const u8, y: *const u8| unsafe { (*(x as *const [u64; 2])).cmp(&*(y as *const [u64; 2])) };
                    let mix = |p: *const u8| unsafe { (*(p as *const u64).add(5)).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 40 };
                    let cmp = |x: *const u8, y: *const u8| -> Ordering {
                        match order {
                            // a collation's order: key 0's pair, then images equal past their low bit (distinct bytes tie)
                            Order::Images => pair(x, y).then_with(|| unsafe { (*(x as *const u64).add(4) | 1 << 56).cmp(&(*(y as *const u64).add(4) | 1 << 56)) }),
                            // an order that is not total: a pure function of the pair, neither transitive nor antisymmetric
                            Order::Lenient => pair(x, y).then_with(|| [Ordering::Less, Ordering::Equal, Ordering::Greater][((mix(x) ^ mix(y).rotate_left(7)) % 3) as usize]),
                            // a comparator that raises at its 40th call (its error flagged, the pair equal)
                            Order::Exact => {
                                calls.set(calls.get() + 1);
                                if calls.get() >= 40 {
                                    CMP_ERR.with(|c| c.set(true));
                                    return Ordering::Equal;
                                }
                                unsafe { cmp_rows(2, spec, x, y) }
                            }
                        }
                    };
                    CMP_ERR.with(|c| c.set(false));
                    let full = unsafe { index(core::ptr::null_mut(), run, 4, &cmp, order, Road::Seal) }.ok().flatten().map(|w| w.to_vec());
                    let (full_err, full_calls) = (CMP_ERR.with(|c| c.replace(false)), calls.replace(0));
                    let mut at = [0u32; SMALL_SORT_ROWS];
                    let small = unsafe { small_index(run, 4, &cmp, order, &mut at) }.map(|k| at[..k].to_vec());
                    let (small_err, small_calls) = (CMP_ERR.with(|c| c.replace(false)), calls.replace(0));
                    match n {
                        0 | 1 => assert_eq!((small, full), (None, None), "{what}"),
                        n if n > SMALL_SORT_ROWS => assert_eq!(small, None, "{what}: past the bound, index's"),
                        _ => {
                            assert_eq!(small, full, "{what}: the same order");
                            assert_eq!((small_err, small_calls), (full_err, full_calls), "{what}: the same compares, the same raise");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_heap_top_a_kept_run_and_a_wide_row_keep_their_roads() {
        let spec = fixed_spec(false);
        let mut at = [0u32; SMALL_SORT_ROWS];
        for bits in [RUN_HEAP, RUN_ORDERED] {
            let mut reg = fixed(40, 5, false, false, bits, 5);
            assert_eq!(unsafe { small_index(reg.as_mut_ptr() as *mut SortRun, 4, &|x, y| cmp_rows(2, spec, x, y), Order::Images, &mut at) }, None, "bits {bits}: index's road");
        }
        let kept = fixed(40, 5, false, false, RUN_ORDERED, 5);
        let mut reg = kept.clone();
        assert_eq!(unsafe { order_by(core::ptr::null_mut(), reg.as_mut_ptr() as *mut SortRun, 4, spec, Road::Seal, true) }, 0);
        assert!(reg == kept, "a run kept as it stands is not moved");
        // a row past one row of stack scratch: the small order, permuted by `permute_rows`
        const WIDE: u32 = SMALL_SORT_ROW_BYTES as u32 + 64;
        let build = || {
            let mut reg = vec![0u64; (64 * WIDE as usize + 512) / 8]; // alloc: Cold
            let mut s = 99u64;
            unsafe {
                let run = open(&mut reg, WIDE);
                for i in 0..60u64 {
                    let row = (*run).push() as *mut u64;
                    (*row, *row.add(1), *row.add(5), *row.add(WIDE as usize / 8 - 1)) = (1, next(&mut s) % 4, i, i);
                }
            }
            reg
        };
        let (mut off, mut on) = (build(), build());
        let spec = SortSpec::pack(&[(None, false)]).unwrap();
        unsafe {
            assert_eq!(order_by(core::ptr::null_mut(), off.as_mut_ptr() as *mut SortRun, 2, spec, Road::Seal, false), 0);
            assert_eq!(order_by(core::ptr::null_mut(), on.as_mut_ptr() as *mut SortRun, 2, spec, Road::Seal, true), 0);
        }
        assert!(off == on, "a wide row: the same bytes");
    }
}
