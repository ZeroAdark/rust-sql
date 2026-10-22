//! The row sort's raw half (ops/sort/rowsort.rs keeps the sort spec, the emit table's words and flags, the row
//! comparator's layout, the hosted comparator's cache and the stash and want state): the sort entries over a worker's
//! run (open, full, last, adopt, the var and big runs' puts), the run reader (begin, next, total, stop, the spool),
//! the row and image compares and the stash. Moved unchanged from rowsort.rs (rule R6: the family's unsafe code lives
//! in raw); rowsort.rs re-exports them under their old paths.
use crate::host_abi::*;
use gravity_fn::typesem::keysem::call;
use gravity_abi::ctx::{cancel_pending, POLL_BATCH};
use gravity_abi::status::CANCELLED;
use crate::table::emit::{Road, Work};

use super::super::polled::{Poll, Stopped};
use super::super::rowsort::{order_failed, polled_scratch, sort_by_order_polled, var_alloc, var_cmp_as, want_begin, Order, SortSpec, VarShape, BIG, CMP_ERR, KFLAG_BPCHAR, KFLAG_EQTIE, KFLAG_HOSTED, KFLAG_SPEC_SHIFT, KFLAG_TOTAL, KFLAG_USER, KFLAG_WORDEXACT, ROWCMP_KEYS, SEALED, SMALL_SORT_ROWS, SMALL_SORT_ROW_BYTES, SORTRUN_BIG, SORT_MODE_MEM, SORT_MODE_MERGE, SORT_TBL_HDR, SPOOL_EMPTY, SPOOL_SPILLED_WHAT, STASH, VAR_TBL, VAR_WIDTH, WANT, WANT_GEN, WORD_WIDTH};

/// The run row comparator of a table: the key words, then a fixed image's bytes or a variable image's tail (the equal-short-key shortcut, the bytes under a bytes-ordered collation, `varstr_cmp` through the collation once per tie).
#[derive(Clone, Copy)]
pub struct RowCmp {
    nkey: usize,
    spec: SortSpec,
    coll: [u32; ROWCMP_KEYS],
    bpchar: u16,
    ext: [u16; ROWCMP_KEYS - 4],
    /// The keys ordered by their comparator (`KFLAG_HOSTED`), and the table's hosted section (each key's support function's
    /// resolution id, `gravity_fn::resolved`;
    /// an address, 0 with no hosted key): the struct stays the size a non-hosted sort compares with (HO condition (d)).
    hosted: u16,
    /// The hosted keys whose equal copies tie with no call (`KFLAG_EQTIE`).
    eqtie: u16,
    /// The hosted keys whose words mark themselves whole by their low bit (`KFLAG_WORDEXACT`).
    wexact: u16,
    /// The hosted keys whose comparator is user code (`KFLAG_USER`), and whether any key holds one (past `ROWCMP_KEYS` too).
    user: u16,
    exact: bool,
    /// TOTAL-ORDER: whether a hosted key (past `ROWCMP_KEYS` too) lacks `KFLAG_TOTAL`, its comparator one that may reach
    /// user code: the sorts order by PostgreSQL's qsort (`Order::Lenient`).
    lenient: bool,
    procs: usize,
    /// W8-SETOP-WIDEKEYS: the var section's `(coll, kflags)` words (`VarShape::keys`) when a key is past `ROWCMP_KEYS`
    /// (`cmp_far` reads theirs there, as `procs` points into the table); 0 otherwise.
    far: usize,
}

/// D1581 (G4): a run row's sort head (`RowCmp::head`), 24 bytes as PostgreSQL's SortTuple: key 0's word, its payload's
/// address, its payload's length with key 0's flags in the top two bits (0 a NULLS FIRST NULL, 1 a value, 2 a NULLS LAST
/// NULL; a varlena's payload is under 2^30 bytes), the row's index. Built per ordering of a live run (m3_sort.rs
/// `index_by`) and dropped with it: never across a put or a spill.
#[derive(Clone, Copy)]
#[repr(C)]
pub(crate) struct Head {
    w: u64,
    p: *const u8,
    nf: u32,
    pub(crate) i: u32,
}

const _: () = assert!(core::mem::size_of::<Head>() == 24, "a head is a SortTuple's 24 bytes");

/// D4411: a polled sort's scratch head (`Work::block_on`), written before it is read.
impl Default for Head {
    fn default() -> Head {
        Head { w: 0, p: core::ptr::null(), nf: 0, i: 0 }
    }
}

pub unsafe fn cmp_rows(nkey: usize, spec: SortSpec, a: *const u8, b: *const u8) -> Ordering {
    for k in 0..nkey {
        let (x, y) = (a as *const u64, b as *const u64);
        let (fa, fb) = (*x.add(2 * k), *y.add(2 * k));
        if fa != fb {
            return fa.cmp(&fb);
        }
        let (wa, wb) = (*x.add(2 * k + 1), *y.add(2 * k + 1));
        if wa != wb {
            return wa.cmp(&wb);
        }
        let (off, len, desc) = spec.key(k);
        if len == 0 || fa != 1 {
            continue;
        }
        let o = core::slice::from_raw_parts(a.add(off), len).cmp(core::slice::from_raw_parts(b.add(off), len));
        if o != Ordering::Equal {
            return if desc { o.reverse() } else { o };
        }
    }
    Ordering::Equal
}
unsafe fn sort_rows(ctx: *mut Ctx, run: *mut SortRun, nkey: usize, spec: SortSpec, road: Road) -> i64 {
    if nkey == 0 {
        return 0; // a spool's run (M8): no keys, the rows keep their order
    }
    sort_rows_by(ctx, run, &|a, b| cmp_rows(nkey / 2, spec, a, b), None, Order::Images, road)
}
/// The rows' `cmp` order as indexes, by `order` (`sort_by_order`; `Exact`, USEROP `RowCmp::exact`: PostgreSQL's qsort over
/// them in arrival order). D4373 (b): the index is a block of the statement's grant (`Work`: the allocator's under the
/// memory policy's floor); `Err` when the grant refuses it. D4411: the order is a polled step (`sort_by_order_polled`, its
/// scratch a block of the grant too): `Err(CANCELLED)` when the statement's abort is pending.
unsafe fn sorted_index(ctx: *mut Ctx, run: *mut SortRun, cmp: &dyn Fn(*const u8, *const u8) -> Ordering, order: Order, road: Road) -> Result<Work<usize>, i64> {
    let n = (*run).len as usize;
    let rb = (*run).row_bytes as usize;
    let base = (*run).row(0);
    let mut idx = Work::from_fn_on(ctx, n, |i| i, road)?;
    let mut scratch = Work::<usize>::block_on(ctx, polled_scratch(n, order), road)?;
    let pending = || !ctx.is_null() && cancel_pending(ctx);
    match sort_by_order_polled(&mut Poll::new(&pending), &mut idx, &mut scratch, order, &|a, b| cmp(base.add(a * rb), base.add(b * rb))) {
        Ok(()) => Ok(idx),
        Err(Stopped) => Err(CANCELLED),
    }
}
/// The rows permuted into `cmp` order in place (`order`: `sorted_index`'s); a var row's slots re-pointed by the distance
/// it moved. 0, a status below zero (the index refused, `CANCELLED` mid-permutation).
pub(crate) unsafe fn sort_rows_by(ctx: *mut Ctx, run: *mut SortRun, cmp: &dyn Fn(*const u8, *const u8) -> Ordering, var: Option<VarShape>, order: Order, road: Road) -> i64 {
    let idx = match sorted_index(ctx, run, cmp, order, road) {
        Ok(idx) => idx,
        Err(e) => return e,
    };
    let pending = || !ctx.is_null() && cancel_pending(ctx);
    if permute_rows(ctx, run, &|j| idx[j], var, &pending) {
        return CANCELLED;
    }
    0
}
/// The rows permuted into `idx`'s order (row `j` becomes row `idx(j)`, `idx` a permutation of the run's rows); a var row's
/// slots re-pointed by the distance it moved. D4373: through a copy of the run whose bytes are a block of the grant when it
/// has room now (`work_try`; the allocator's under the memory policy's floor or with no spill host, as before), else in
/// place by its cycles (`permute_in_place`: a row of scratch and a bit per row) rather than a refusal: the same bytes
/// either way. `pending` (the statement's cancel) is read every `POLL_BATCH` rows; true = it stopped, the run holding
/// every row once with its slots right, in no order.
pub(crate) unsafe fn permute_rows(ctx: *mut Ctx, run: *mut SortRun, idx: &dyn Fn(usize) -> usize, var: Option<VarShape>, pending: &dyn Fn() -> bool) -> bool {
    if (0..(*run).len as usize).all(|j| idx(j) == j) {
        return false;
    }
    let bytes = (*run).len as usize * (*run).row_bytes as usize;
    match if ctx.is_null() { None } else { gravity_abi::program::work_try(ctx, bytes as u64) } {
        None => {
            // LEDGER2 step 5: the allocator's road, from `ARENA_SCRATCH_MIN` bytes on a scratch arena under the slot's owner
            let mut tmp = super::runs::SortCopy::allocator(ctx, bytes);
            permute_by_copy(run, idx, var, pending, tmp.as_mut_ptr())
        }
        Some(p) if p.is_null() => permute_in_place(ctx, run, idx, var, pending),
        Some(p) => {
            let stopped = permute_by_copy(run, idx, var, pending, p);
            gravity_abi::program::work_give(ctx, p);
            stopped
        }
    }
}

/// `permute_rows` through `dst` (room for the run's rows): the rows gathered there in `idx`'s order, their slots
/// re-pointed, then laid back over the run. Stopped by `pending`, the run is left as it was.
pub(crate) unsafe fn permute_by_copy(run: *mut SortRun, idx: &dyn Fn(usize) -> usize, var: Option<VarShape>, pending: &dyn Fn() -> bool, dst: *mut u8) -> bool {
    let n = (*run).len as usize;
    let rb = (*run).row_bytes as usize;
    let base = (*run).row(0) as *mut u8;
    for j in 0..n {
        if j % POLL_BATCH == POLL_BATCH - 1 && pending() {
            return true;
        }
        let i = idx(j);
        let at = dst.add(j * rb);
        core::ptr::copy_nonoverlapping(base.add(i * rb), at, rb);
        if let Some(v) = var {
            for c in 0..v.nvar {
                let slot = at.add(*v.slots.add(2 * c + 1) as usize);
                if (slot as *const u32).read_unaligned() != 0 {
                    let off = (slot.add(4) as *const u32).read_unaligned() as i64 + (i as i64 - j as i64) * rb as i64;
                    (slot.add(4) as *mut u32).write_unaligned(off as u32);
                }
            }
        }
    }
    core::ptr::copy_nonoverlapping(dst, base, n * rb);
    false
}

/// `permute_rows` in place (D4373: the grant had no room for the copy): by the run's cycles, through one row of scratch
/// and a bit per row (no copy of the run's rows): each cycle `s -> idx(s) -> ..` moves row `idx(j)` to `j` until it comes back to `s`,
/// whose row waits in the scratch; the same order as a copy's, so the same bytes. `pending` (the statement's cancel) is
/// read every `POLL_BATCH` rows moved; true = it stopped: the cycle under way is closed first (its held row put where the
/// next move would have read), so the run holds every row once with its slots right, in no order.
/// LEDGER2 step 5: the bitmap is the allocator's road (`Work::scratch`: from `ARENA_SCRATCH_MIN` bytes on, a scratch arena
/// under `ctx`'s slot owner; `ctx` null: the allocator's).
pub(crate) unsafe fn permute_in_place(ctx: *mut Ctx, run: *mut SortRun, idx: &dyn Fn(usize) -> usize, var: Option<VarShape>, pending: &dyn Fn() -> bool) -> bool {
    // a bit per row (n / 8 bytes) and one row of scratch
    let mut done = Work::scratch(ctx, ((*run).len as usize).div_ceil(64), |_| 0u64, false).expect("an infallible scratch");
    let mut held: Vec<u8> = vec![0; (*run).row_bytes as usize]; // alloc: Exec
    permute_cycles(run, idx, var, pending, &mut done, held.as_mut_ptr())
}

/// `permute_rows` of a run of at most `SMALL_SORT_ROWS` rows whose rows are at most `SMALL_SORT_ROW_BYTES` wide: `permute_in_place`'s
/// walk through a bitmap and a row on the stack (the same bytes as a copy's), no poll. False (the run untouched) for a
/// wider row or a longer run: the caller takes `permute_rows`.
pub(crate) unsafe fn permute_small(run: *mut SortRun, idx: &[u32], var: Option<VarShape>) -> bool {
    if (*run).row_bytes as usize > SMALL_SORT_ROW_BYTES || (*run).len as usize > SMALL_SORT_ROWS || idx.len() != (*run).len as usize {
        return false;
    }
    let mut done = [0u64; SMALL_SORT_ROWS.div_ceil(64)];
    let mut held = [0u64; SMALL_SORT_ROW_BYTES / 8];
    permute_cycles(run, &|j| idx[j] as usize, var, &|| false, &mut done, held.as_mut_ptr() as *mut u8);
    true
}

/// `permute_in_place`'s walk: `done` a zeroed bit per row, `held` one row's bytes.
unsafe fn permute_cycles(run: *mut SortRun, idx: &dyn Fn(usize) -> usize, var: Option<VarShape>, pending: &dyn Fn() -> bool, done: &mut [u64], held: *mut u8) -> bool {
    let n = (*run).len as usize;
    let rb = (*run).row_bytes as usize;
    let base = (*run).row(0) as *mut u8;
    // row `j` now holds the row that stood at `i`: its var slots' offsets (from the slot to the copy) move by `(i - j) x rb`
    let repoint = |j: usize, i: usize| {
        if let Some(v) = var {
            let row = base.add(j * rb);
            for c in 0..v.nvar {
                let slot = row.add(*v.slots.add(2 * c + 1) as usize);
                if (slot as *const u32).read_unaligned() != 0 {
                    let off = (slot.add(4) as *const u32).read_unaligned() as i64 + (i as i64 - j as i64) * rb as i64;
                    (slot.add(4) as *mut u32).write_unaligned(off as u32);
                }
            }
        }
    };
    let (mut moved, mut stopped) = (0usize, false);
    for s in 0..n {
        if done[s / 64] >> (s % 64) & 1 != 0 {
            continue;
        }
        done[s / 64] |= 1 << (s % 64);
        if idx(s) == s {
            continue;
        }
        core::ptr::copy_nonoverlapping(base.add(s * rb), held, rb);
        let mut j = s;
        loop {
            let i = idx(j);
            if i == s || stopped {
                // the cycle closes: the held row (`s`'s) goes to `j`
                core::ptr::copy_nonoverlapping(held, base.add(j * rb), rb);
                repoint(j, s);
                break;
            }
            core::ptr::copy_nonoverlapping(base.add(i * rb), base.add(j * rb), rb);
            repoint(j, i);
            done[i / 64] |= 1 << (i % 64);
            j = i;
            moved += 1;
            if moved % POLL_BATCH == 0 && pending() {
                stopped = true;
            }
        }
        if stopped {
            return true;
        }
    }
    false
}
/// The var run put as compacted records: every row followed by its copies (the slots re-pointed, the record word its length), the stream at the base.
unsafe fn put_var(ctx: *mut Ctx, run: *mut SortRun, tbl: *const u32) -> i64 {
    put_var_at(ctx, run, tbl, 0)
}
/// `put_var` of the run at `region` (`SPILL_RUN_PUT`'s arg: 0 the own region, else a D1330 big run's address).
unsafe fn put_var_at(ctx: *mut Ctx, run: *mut SortRun, tbl: *const u32, region: i64) -> i64 {
    if (*run).len == 0 {
        return 0;
    }
    let v = var_shape(tbl).expect("a var table");
    let cmp = RowCmp::of(tbl);
    CMP_ERR.with(|c| c.set(false));
    // a keyless run (a Material's, D3105 (b)) keeps its rows' order, as `sort_rows` keeps a fixed one's
    let idx = if *tbl.add(1) == 0 { Work::from_fn_on(ctx, (*run).len as usize, |i| i, Road::Put) } else { sorted_index(ctx, run, &|a, b| cmp.cmp(a, b), cmp.order(), Road::Put) };
    if CMP_ERR.with(|c| c.get()) {
        return cmp_failed("a sort compare failed");
    }
    let idx = match idx {
        Ok(idx) => idx,
        Err(e) => return e,
    };
    let rb = (*run).row_bytes as usize;
    let base = (*run).row(0) as *mut u8;
    // D4373 (b): the records' copy in a block of the grant, sized as `RunShape::compact`'s (the rows and the var region,
    // which the records never outgrow)
    let span = (SORTRUN_HDR + (*run).cap as usize * rb).saturating_sub((*run).var_top() as usize);
    let mut tmp = match super::runs::SortCopy::new(ctx, (*run).len as usize * rb + span, Road::Put) {
        Ok(t) => t,
        Err(e) => return e,
    };
    for &i in idx.iter() {
        let row = base.add(i * rb);
        let at = tmp.len();
        tmp.extend_from_slice(core::slice::from_raw_parts(row, rb));
        let mut acc = rb;
        for c in 0..v.nvar {
            let so = *v.slots.add(2 * c + 1) as usize;
            let size = (row.add(so) as *const u32).read_unaligned();
            if size == 0 {
                continue;
            }
            let off = (row.add(so + 4) as *const u32).read_unaligned() as usize;
            let alloc = var_alloc(size) as usize;
            (tmp.as_mut_ptr().add(at + so + 4) as *mut u32).write_unaligned((acc - so) as u32);
            tmp.extend_from_slice(core::slice::from_raw_parts(row.add(so + off), alloc));
            acc += alloc;
        }
        // D2572-c: the record word whole (its high half was never written) and the null bytes' padding zero
        (tmp.as_mut_ptr().add(at + v.rec_off) as *mut u64).write_unaligned(acc as u64);
        tmp.written(at + v.pad.0..at + v.pad.1).fill(0);
    }
    core::ptr::copy_nonoverlapping(tmp.as_ptr(), base, tmp.len());
    (*run).set_var_bytes(tmp.len() as u32);
    super::super::raw::declare_rows(ctx, rb as u32, &v);
    let put = spill(ctx, SPILL_RUN_PUT, region);
    if put < 0 {
        return put;
    }
    (*run).clear();
    0
}
unsafe fn last_var(ctx: *mut Ctx, run: *mut SortRun, tbl: *const u32) -> i64 {
    // D1330: a big run in flight (the section's last rows in it, or the own run's section end with it pending) is put
    // first: never kept in memory
    match last_big(ctx, run, tbl, put_var_at) {
        Err(e) => return e,
        Ok(true) => return 0,
        Ok(false) => {}
    }
    if (*run).len == 0 {
        return 0;
    }
    let keeps = (*ctx).width == 1 && match spill(ctx, SPILL_RUNS, 0) {
        n if n < 0 => return n,
        n => n == 0,
    };
    if !keeps {
        return put_var(ctx, run, tbl);
    }
    let cmp = RowCmp::of(tbl);
    CMP_ERR.with(|c| c.set(false));
    let st = sort_rows_by(ctx, run, &|a, b| cmp.cmp(a, b), var_shape(tbl), cmp.order(), Road::Seal);
    if CMP_ERR.with(|c| c.get()) {
        return cmp_failed("a sort compare failed");
    }
    if st < 0 {
        return st;
    }
    (*run).pad[0] = SEALED;
    0
}
crate::contract::host! {
    /// `sort_open` for a var row (M4): the table in place of the shape words.
    pub unsafe fn sort_open_var(ctx: *mut Ctx, tbl: *const u32) -> *mut u8 {
        guard!(ERROR as *mut u8, {
            want_begin();
            let run = SortRun::at_share(partial(ctx), *tbl) as *mut SortRun;
            if (*ctx).claim.is_section_end() {
                return last_var(ctx, run, tbl) as *mut u8;
            }
            open_big(ctx, run)
        })
    }
}
/// D1330: a section end's big run in flight put (`run` the big run itself: true, nothing else to do; `run` the own run:
/// its pending big run's put first, false).
pub(crate) unsafe fn last_big(ctx: *mut Ctx, run: *mut SortRun, tbl: *const u32, put: VarPut) -> Result<bool, i64> {
    let own = partial(ctx) as *mut SortRun;
    if (*run).pad[1] == SORTRUN_BIG {
        big_put(ctx, own, run, tbl, put)?;
        return Ok(true);
    }
    if (*own).pad[1] != BIG_PENDING {
        return Ok(false);
    }
    let big = big_of(ctx, own)?;
    if !big.is_null() {
        big_put(ctx, own, big, tbl, put)?;
    }
    (*own).pad[1] = 0;
    Ok(false)
}
/// D1330: a call's run: the big run in flight when the call before left rows in it, else the own run.
pub(crate) unsafe fn open_big(ctx: *mut Ctx, own: *mut SortRun) -> *mut u8 {
    if (*own).pad[1] != BIG_PENDING {
        return own as *mut u8;
    }
    match big_of(ctx, own) {
        Err(e) => e as *mut u8,
        Ok(b) if !b.is_null() => b as *mut u8,
        Ok(_) => own as *mut u8,
    }
}
crate::contract::host! {
    /// `sort_full` for a var row: the run's records compacted, sorted and put. D1330: a status above zero is the run the next
    /// row goes to (the IR's `w_run`): a big run past the region when one row's copies had no room in the empty run, the own
    /// run again once a big run is put.
    pub unsafe fn sort_full_var(ctx: *mut Ctx, run: *mut u8, tbl: *const u32) -> i64 {
        guard!(ERROR, {
            full_var_by(ctx, run as *mut SortRun, tbl, put_var_at, |_, _| {})
        })
    }
}
/// D1330: `pad[1]` of an own run whose big run may be in flight: only then does an open or a section end look it up (a
/// stale mark past the statement finds none: the lookup is the host's).
const BIG_PENDING: u8 = 0x43;
/// D1330: an empty run with no room for a row's copy of `alloc` bytes: armed and "no room" (1) the first time, the put
/// grows a big run; `UNSUPPORTED_SCRATCH` when the run was armed already (its put grew none).
#[cold]
unsafe fn want_room(run: *mut u8, r: &SortRun, alloc: u32) -> i64 {
    let generation = WANT_GEN.with(|g| g.get());
    let (w, g, _) = WANT.with(|c| c.get());
    if w == run as usize && g == generation {
        WANT.with(|c| c.set((0, 0, 0)));
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    // the row's copies so far sit between the var top and the rows' end
    let used = (SORTRUN_HDR as u32 + r.cap * r.row_bytes).saturating_sub(r.var_top());
    WANT.with(|c| c.set((run as usize, generation, (used + alloc) as usize)));
    1
}
/// D1330: the key of `own`'s big run among the host's big buffers (`SPILL_MERGE_BIG`; the merge's own buffers are keyed
/// `own`).
fn big_key(own: *mut SortRun) -> u64 {
    own as u64 + 1
}
/// D1330: `own`'s big run in flight (its rows not put yet), else null: the host's big buffer keyed `big_key` (none once
/// freed, and none past the statement's end: `drop_result_spills` clears them).
unsafe fn big_of(ctx: *mut Ctx, own: *mut SortRun) -> Result<*mut SortRun, i64> {
    let w = [big_key(own), u64::MAX];
    match spill(ctx, gravity_abi::program::SPILL_MERGE_BIG, w.as_ptr() as i64) {
        n if n < 0 => Err(n),
        n => Ok(n as usize as *mut SortRun),
    }
}
/// D1330: `own`'s big run freed for reuse.
unsafe fn big_free(ctx: *mut Ctx, own: *mut SortRun) -> Result<(), i64> {
    let w = [big_key(own), 0];
    match spill(ctx, gravity_abi::program::SPILL_MERGE_BIG, w.as_ptr() as i64) {
        n if n < 0 => Err(n),
        _ => Ok(()),
    }
}
/// D1330: an empty big run for a row whose copies want `want` bytes (twice that, so the rows behind it fill it too), drawn
/// from the grant (PG lets one tuple pass work_mem); the one before it, too small, freed first.
#[cold]
unsafe fn big_run(ctx: *mut Ctx, own: *mut SortRun, want: usize, tbl: *const u32, prep: VarPrep) -> Result<*mut SortRun, i64> {
    big_free(ctx, own)?;
    let rb = (*own).row_bytes as usize;
    let cap = (rb + 2 * want).div_ceil(rb).max(2);
    if cap > u32::MAX as usize / rb {
        return Err(unsupported(UNSUPPORTED_SCRATCH));
    }
    let bytes = SORTRUN_HDR + cap * rb + gravity_abi::breaker::SORTRUN_VAR_WORDS as usize;
    let w = [big_key(own), bytes as u64];
    let p = spill(ctx, gravity_abi::program::SPILL_MERGE_BIG, w.as_ptr() as i64) as *mut SortRun;
    if p.is_null() {
        return Err(ERROR);
    }
    *p = SortRun { cap: cap as u32, len: 0, row_bytes: rb as u32, pad: [0, SORTRUN_BIG, 0], kind: TABLE_SORT };
    (*p).clear();
    prep(p, tbl);
    Ok(p)
}
/// D1330: the big run's rows put (a run of their own, `SPILL_RUN_PUT` at its address), the buffer freed, the own run next.
#[cold]
pub(crate) unsafe fn big_put(ctx: *mut Ctx, own: *mut SortRun, big: *mut SortRun, tbl: *const u32, put: VarPut) -> Result<(), i64> {
    let r = put(ctx, big, tbl, big as i64);
    if r < 0 {
        return Err(r);
    }
    if (*own).pad[1] == BIG_PENDING {
        (*own).pad[1] = 0;
    }
    big_free(ctx, own)
}
/// D1330: a var run's put at a region (`SPILL_RUN_PUT`'s arg) and a new run's preparation (the resident road's table copy).
pub(crate) type VarPut = unsafe fn(*mut Ctx, *mut SortRun, *const u32, i64) -> i64;
pub(crate) type VarPrep = unsafe fn(*mut SortRun, *const u32);
/// A var run's put with the big-run turns (D1330): an armed empty run grows a big run (returned, the next row's run); a
/// big run's rows put and the own run returned; else `put`.
pub(crate) unsafe fn full_var_by(ctx: *mut Ctx, run: *mut SortRun, tbl: *const u32, put: VarPut, prep: VarPrep) -> i64 {
    if (*run).len != 0 && (*run).pad[1] != SORTRUN_BIG {
        return put(ctx, run, tbl, 0);
    }
    let own = partial(ctx) as *mut SortRun;
    let r = (|| -> Result<i64, i64> {
        if (*run).len == 0 {
            let (w, g, want) = WANT.with(|c| c.get());
            if w != run as usize || g != WANT_GEN.with(|c| c.get()) || want == 0 {
                return Ok(0);
            }
            WANT.with(|c| c.set((0, 0, 0)));
            let big = big_run(ctx, own, want, tbl, prep)?;
            (*own).pad[1] = BIG_PENDING;
            return Ok(big as i64);
        }
        big_put(ctx, own, run, tbl, put)?;
        Ok(own as i64)
    })();
    r.unwrap_or_else(|e| e)
}
crate::contract::host! {
    /// `sort_last` for a var row.
    pub unsafe fn sort_last_var(ctx: *mut Ctx, run: *mut u8, tbl: *const u32) -> i64 {
        guard!(ERROR, {
            last_var(ctx, run as *mut SortRun, tbl)
        })
    }
}
crate::contract::host! {
    /// The `len` bytes at `ptr` copied into the run's variable region as a 4-byte-header varlena for the row being built (row `len` of the run): its slot at `slot_off` written `[size, off]`, the record word at `rec_off` grown; `1` without room (the caller puts the run and retries), `UNSUPPORTED_SCRATCH` when an empty run has no room.
    pub unsafe fn sort_image(run: *mut u8, ptr: *const u8, len: u64, slot_off: u64, rec_off: u64) -> i64 {
        guard!(ERROR, {
            image_into(run, ptr, len as u32, slot_off, rec_off, false)
        })
    }
}
crate::contract::host! {
    /// `sort_image` for a column the sort never compares: the `total` bytes of the image at `ptr` copied verbatim, whatever its header (a compressed inline value or a TOAST pointer travels through the run untouched: the sink's copy and the wire's output function decode it).
    pub unsafe fn sort_image_raw(run: *mut u8, ptr: *const u8, total: u64, slot_off: u64, rec_off: u64) -> i64 {
        guard!(ERROR, {
            image_into(run, ptr, total as u32, slot_off, rec_off, true)
        })
    }
}
unsafe fn image_into(run: *mut u8, ptr: *const u8, len: u32, slot_off: u64, rec_off: u64, raw: bool) -> i64 {
    let r = &mut *(run as *mut SortRun);
    let size = if raw { len } else { 4 + len };
    let alloc = var_alloc(size);
    let Some(top) = r.reserve_var(alloc) else {
        return if r.len == 0 { want_room(run, r, alloc) } else { 1 };
    };
    let dst = run.add(top as usize);
    if raw {
        core::ptr::copy_nonoverlapping(ptr, dst, len as usize);
    } else {
        (dst as *mut u32).write_unaligned(size << 2);
        core::ptr::copy_nonoverlapping(ptr, dst.add(4), len as usize);
    }
    core::ptr::write_bytes(dst.add(size as usize), 0, (alloc - size) as usize);
    let row_off = SORTRUN_HDR + r.len as usize * r.row_bytes as usize;
    let slot = run.add(row_off + slot_off as usize);
    (slot as *mut u32).write_unaligned(size);
    (slot.add(4) as *mut u32).write_unaligned(top - (row_off + slot_off as usize) as u32);
    let rec = run.add(row_off + rec_off as usize) as *mut u32;
    rec.write_unaligned(rec.read_unaligned() + alloc);
    0
}
pub(crate) unsafe fn put(ctx: *mut Ctx, run: *mut SortRun, nkey: usize, spec: SortSpec) -> i64 {
    if (*run).len == 0 {
        return 0;
    }
    let st = sort_rows(ctx, run, nkey, spec, Road::Put);
    if st < 0 {
        return st;
    }
    let put = spill(ctx, SPILL_RUN_PUT, 0);
    if put < 0 {
        return put;
    }
    (*run).clear();
    0
}
unsafe fn sealed(own: *const u8, row_bytes: usize) -> bool {
    let run = &*(own as *const SortRun);
    run.cap != 0 && run.kind == TABLE_SORT && run.row_bytes as usize == row_bytes && run.pad[0] == SEALED && run.len <= run.cap
}
pub(crate) unsafe fn last(ctx: *mut Ctx, run: *mut SortRun, nkey: usize, spec: SortSpec) -> i64 {
    if (*run).len == 0 {
        return 0;
    }
    // the leader's run with no run put before stays sorted in the region (the emit reads it there); else put
    let keeps = (*ctx).width == 1 && match spill(ctx, SPILL_RUNS, 0) {
        n if n < 0 => return n,
        n => n == 0,
    };
    if !keeps {
        return put(ctx, run, nkey, spec);
    }
    let st = sort_rows(ctx, run, nkey, spec, Road::Seal);
    if st < 0 {
        return st;
    }
    (*run).pad[0] = SEALED;
    0
}
crate::contract::host! {
    /// The pipeline's partial armed as the run; at the section end the last run put and null (a status below zero as the word).
    pub unsafe fn sort_open(ctx: *mut Ctx, row_bytes: u64, nkey: u64, spec: u64) -> *mut u8 {
        guard!(ERROR as *mut u8, {
            let run = SortRun::at_share(partial(ctx), row_bytes as u32) as *mut SortRun;
            if (*ctx).claim.is_section_end() {
                return last(ctx, run, nkey as usize, SortSpec(spec)) as *mut u8;
            }
            run as *mut u8
        })
    }
}
crate::contract::host! {
    /// M7 L3b, the sorted drain's probe pipe: `sort_open` whose section end puts the last run even alone (never kept in the region: the drain pipe adopts the stream).
    pub unsafe fn sort_open_put(ctx: *mut Ctx, row_bytes: u64, nkey: u64, spec: u64) -> *mut u8 {
        guard!(ERROR as *mut u8, {
            let run = SortRun::at_share(partial(ctx), row_bytes as u32) as *mut SortRun;
            if (*ctx).claim.is_section_end() {
                return put(ctx, run, nkey as usize, SortSpec(spec)) as *mut u8;
            }
            run as *mut u8
        })
    }
}
crate::contract::host! {
    /// `sort_open_put` for a var row.
    pub unsafe fn sort_open_var_put(ctx: *mut Ctx, tbl: *const u32) -> *mut u8 {
        guard!(ERROR as *mut u8, {
            want_begin();
            let run = SortRun::at_share(partial(ctx), *tbl) as *mut SortRun;
            if (*ctx).claim.is_section_end() {
                // D1330: a big run in flight is put first
                if let Err(e) = last_big(ctx, run, tbl, put_var_at) {
                    return e as *mut u8;
                }
                return put_var(ctx, run, tbl) as *mut u8;
            }
            open_big(ctx, run)
        })
    }
}
crate::contract::host! {
    /// M7 L3b, the sorted drain: the runs the probe pipe put in slot `slot1 - 1` moved into this pipeline's own stream.
    pub unsafe fn sort_adopt(ctx: *mut Ctx, slot1: u64) -> i64 {
        match spill(ctx, SPILL_RUN_ADOPT, slot1 as i64) {
            n if n < 0 => n,
            _ => 0,
        }
    }
}
crate::contract::host! {
    /// The spool's run (M8 C.0): the partial opened at `SortRun::at_rows` for `rows` of `row_bytes` (2× the estimate, at least 64, within the share; the tail `grow`s it); at the section end the last run put and null, as `sort_open`.
    pub unsafe fn spool_open(ctx: *mut Ctx, row_bytes: u64, rows: u64) -> *mut u8 {
        guard!(ERROR as *mut u8, {
            let run = SortRun::at_rows(partial(ctx), row_bytes as u32, rows.min(u32::MAX as u64) as u32) as *mut SortRun;
            if (*ctx).claim.is_section_end() {
                return last(ctx, run, 0, SortSpec(0)) as *mut u8;
            }
            run as *mut u8
        })
    }
}
crate::contract::host! {
    /// A full run sorted by its key words and put to the stream.
    pub unsafe fn sort_full(ctx: *mut Ctx, run: *mut u8, nkey: u64, spec: u64) -> i64 {
        guard!(ERROR, {
            put(ctx, run as *mut SortRun, nkey as usize, SortSpec(spec))
        })
    }
}
crate::contract::host! {
    /// A serial producer's end (no section end): its last run.
    pub unsafe fn sort_last(ctx: *mut Ctx, run: *mut u8, nkey: u64, spec: u64) -> i64 {
        guard!(ERROR, {
            last(ctx, run as *mut SortRun, nkey as usize, SortSpec(spec))
        })
    }
}
pub(crate) unsafe fn tbl_shape(tbl: *const u32) -> (usize, usize, usize, SortSpec) {
    (*tbl as usize, *tbl.add(1) as usize, (*tbl.add(2) & !VAR_TBL) as usize, SortSpec(*tbl.add(4) as u64 | (*tbl.add(5) as u64) << 32))
}
/// # Safety
/// `tbl` is an emit table.
pub unsafe fn var_shape(tbl: *const u32) -> Option<VarShape> {
    if *tbl.add(2) & VAR_TBL == 0 {
        return None;
    }
    let at = tbl.add(SORT_TBL_HDR + 3 * *tbl.add(3) as usize);
    let (nvar, rec_off) = (*at as usize, *at.add(1) as usize);
    // face/runs.rs Shape::plan_coded: the key pairs, a datum word per column, the null bytes 8-rounded, the record word
    let (natts, nulls) = ((*tbl.add(2) & !VAR_TBL) as usize, 8 * *tbl.add(1) as usize + 8 * (*tbl.add(2) & !VAR_TBL) as usize);
    let pad = if nulls + natts.next_multiple_of(8) == rec_off { (nulls + natts, rec_off) } else { (0, 0) };
    Some(VarShape { nvar, rec_off, slots: at.add(2), keys: at.add(2 + 2 * nvar), pad })
}
/// The hosted section of a var table (`KFLAG_HOSTED`): one support-function oid per key after the `(coll, kflags)` words,
/// present when any key is hosted; `None` otherwise.
pub(crate) unsafe fn hosted_procs(tbl: *const u32) -> Option<*const u32> {
    let v = var_shape(tbl)?;
    let nkeys = *tbl.add(1) as usize / 2;
    (0..nkeys).any(|k| *v.keys.add(2 * k + 1) & KFLAG_HOSTED != 0).then(|| v.keys.add(2 * nkeys))
}
impl Head {
    const LEN: u32 = (1 << 30) - 1;

    fn f(&self) -> u64 {
        (self.nf >> 30) as u64
    }

    /// Key 0's payload (empty for a NULL).
    /// # Safety
    /// The head is a `RowCmp::head` of a live run that has not been put, sealed or grown since (`index_by`'s order).
    unsafe fn bytes(&self) -> &[u8] {
        if self.p.is_null() { &[] } else { core::slice::from_raw_parts(self.p, (self.nf & Self::LEN) as usize) }
    }
}
impl RowCmp {
    pub(crate) const fn fixed(nkey: usize, spec: SortSpec) -> RowCmp {
        RowCmp { nkey, spec, coll: [0; ROWCMP_KEYS], bpchar: 0, ext: [0; ROWCMP_KEYS - 4], hosted: 0, eqtie: 0, wexact: 0, user: 0, exact: false, lenient: false, procs: 0, far: 0 }
    }

    /// # Safety
    /// `tbl` is an emit table.
    pub unsafe fn of(tbl: *const u32) -> RowCmp {
        call::forget_seen(); // `order_seen`'s values are a comparator's own
        let (_, nkey, _, spec) = tbl_shape(tbl);
        let mut c = RowCmp::fixed(nkey / 2, spec);
        if let Some(v) = var_shape(tbl) {
            let procs = hosted_procs(tbl);
            for k in 0..c.nkey.min(ROWCMP_KEYS) {
                let f = *v.keys.add(2 * k + 1);
                c.coll[k] = *v.keys.add(2 * k);
                c.bpchar |= ((f & KFLAG_BPCHAR) as u16) << k;
                c.eqtie |= ((f & KFLAG_EQTIE != 0) as u16) << k;
                c.wexact |= ((f & KFLAG_WORDEXACT != 0) as u16) << k;
                c.user |= ((f & KFLAG_USER != 0) as u16) << k;
                if k >= 4 {
                    c.ext[k - 4] = (f >> KFLAG_SPEC_SHIFT) as u16;
                }
                if let Some(p) = procs.filter(|_| f & KFLAG_HOSTED != 0) {
                    c.hosted |= 1 << k;
                    c.procs = p as usize;
                }
            }
            if c.nkey > ROWCMP_KEYS {
                c.far = v.keys as usize;
            }
            c.exact = (0..c.nkey).any(|k| *v.keys.add(2 * k + 1) & KFLAG_USER != 0);
            c.lenient = (0..c.nkey).any(|k| *v.keys.add(2 * k + 1) & (KFLAG_HOSTED | KFLAG_TOTAL) == KFLAG_HOSTED);
        }
        c
    }

    /// USEROP: whether a key's comparator is user code (`KFLAG_USER`): the run orders by `sort_exact`.
    pub(crate) fn exact(&self) -> bool {
        self.exact
    }

    /// TOTAL-ORDER: how this comparator's sorts order (`Order`): `Exact` under a user comparator, `Lenient` with any key
    /// ordered through fmgr by a comparator that may reach user code (a hosted key without `KFLAG_TOTAL`, past
    /// `ROWCMP_KEYS` too), else `Images` (image keys, and builtin comparators that reach no user code: total orders).
    pub(crate) fn order(&self) -> Order {
        if self.exact {
            Order::Exact
        } else if self.lenient {
            Order::Lenient
        } else {
            Order::Images
        }
    }

    /// Whether a compare failed since the last call (its error stashed); clears the flag.
    pub fn failed() -> bool {
        CMP_ERR.with(|c| c.replace(false))
    }

    fn key(&self, k: usize) -> (usize, usize, bool) {
        match k {
            0..4 => self.spec.key(k),
            4..ROWCMP_KEYS => SortSpec(self.ext[k - 4] as u64).key(0),
            _ => (0, 0, false),
        }
    }

    /// # Safety
    /// `a` and `b` are run rows of the table.
    pub unsafe fn cmp(&self, a: *const u8, b: *const u8) -> Ordering {
        self.cmp_from(0, a, b)
    }

    /// D3190 (the co-split cut's cross-input compare, exec ops/sort/cut.rs): whether a run row of this table and one of
    /// `other`'s order alike on their first `nkey` keys, so `cmp_cross` is the order both runs are sorted by: per key the
    /// same image width and direction, collation and bpchar trim, and a hosted key the same support function and word /
    /// tie flags. Never under a user comparator or an order that may not be total (a merge over those stays serial).
    /// # Safety
    /// Both tables are live (a hosted key's support function is read from its table).
    pub unsafe fn cross_ok(&self, other: &RowCmp, nkey: usize) -> bool {
        if nkey == 0 || nkey > ROWCMP_KEYS || nkey > self.nkey || nkey > other.nkey || self.exact || other.exact || self.lenient || other.lenient {
            return false;
        }
        (0..nkey).all(|k| {
            let ((_, la, da), (_, lb, db)) = (self.key(k), other.key(k));
            let same = |x: u16, y: u16| (x ^ y) >> k & 1 == 0;
            let alike = la == lb && da == db && self.coll[k] == other.coll[k] && same(self.bpchar, other.bpchar) && same(self.hosted, other.hosted);
            alike && (self.hosted >> k & 1 == 0 || (same(self.eqtie, other.eqtie) && same(self.wexact, other.wexact) && *(self.procs as *const u32).add(k) == *(other.procs as *const u32).add(k)))
        })
    }

    /// D3190: run row `a` of this table against run row `b` of `other`'s on their first `nkey` keys (`cross_ok`): each
    /// key as `cmp_from` orders it, its image read where its own table lays it.
    /// # Safety
    /// `a` is a run row of this table, `b` one of `other`'s, and `cross_ok(other, nkey)`.
    pub unsafe fn cmp_cross(&self, a: *const u8, other: &RowCmp, b: *const u8, nkey: usize) -> Ordering {
        for k in 0..nkey {
            let (x, y) = (a as *const u64, b as *const u64);
            let ((oa, len, desc), (ob, _, _)) = (self.key(k), other.key(k));
            let img = || if len >= WORD_WIDTH as usize || self.hosted >> k & 1 != 0 {
                self.tail(k, *x.add(2 * k + 1), a.add(oa), b.add(ob), len)
            } else {
                core::slice::from_raw_parts(a.add(oa), len).cmp(core::slice::from_raw_parts(b.add(ob), len))
            };
            if let Some(o) = Self::step([*x.add(2 * k), *y.add(2 * k)], [*x.add(2 * k + 1), *y.add(2 * k + 1)], len, desc, img) {
                return o;
            }
        }
        Ordering::Equal
    }

    /// `cmp` from key `k0` on (a head's tie: its key 0 decided equal).
    unsafe fn cmp_from(&self, k0: usize, a: *const u8, b: *const u8) -> Ordering {
        for k in k0..self.nkey.min(ROWCMP_KEYS) {
            let (x, y) = (a as *const u64, b as *const u64);
            let (off, len, desc) = self.key(k);
            let img = || if len >= WORD_WIDTH as usize || self.hosted >> k & 1 != 0 {
                self.tail(k, *x.add(2 * k + 1), a.add(off), b.add(off), len)
            } else {
                core::slice::from_raw_parts(a.add(off), len).cmp(core::slice::from_raw_parts(b.add(off), len))
            };
            if let Some(o) = Self::step([*x.add(2 * k), *y.add(2 * k)], [*x.add(2 * k + 1), *y.add(2 * k + 1)], len, desc, img) {
                return o;
            }
        }
        if self.nkey > ROWCMP_KEYS {
            return self.cmp_far(k0.max(ROWCMP_KEYS), a, b);
        }
        Ordering::Equal
    }

    /// W8-SETOP-WIDEKEYS: keys `k0..nkey` past `ROWCMP_KEYS` (out of line: a sort by fewer keys keeps its code), each as
    /// `cmp_from` orders one, its `SortSpec` entry, collation, `KFLAG_BPCHAR` and `KFLAG_HOSTED` read from the table's var
    /// section (`far`); with none (a fixed run: its keys past four have no tail, runs/keylane.rs) by its pair alone.
    #[cold]
    #[inline(never)]
    unsafe fn cmp_far(&self, k0: usize, a: *const u8, b: *const u8) -> Ordering {
        let far = self.far as *const u32;
        for k in k0..self.nkey {
            let (x, y) = (a as *const u64, b as *const u64);
            let (coll, f) = if far.is_null() { (0, 0) } else { (*far.add(2 * k), *far.add(2 * k + 1)) };
            let (off, len, desc) = SortSpec((f >> KFLAG_SPEC_SHIFT) as u64).key(0);
            let img = || self.far_tail(k, coll, f, a.add(off), b.add(off), len);
            if let Some(o) = Self::step([*x.add(2 * k), *y.add(2 * k)], [*x.add(2 * k + 1), *y.add(2 * k + 1)], len, desc, img) {
                return o;
            }
        }
        Ordering::Equal
    }

    /// Key `k`'s image past `ROWCMP_KEYS` (`cmp_far`), under its var section words `coll` and `f`: a fixed image's bytes, a
    /// hosted key's comparator (the table's hosted section), a var copy's payloads (`var_cmp_as`).
    unsafe fn far_tail(&self, k: usize, coll: u32, f: u32, sa: *const u8, sb: *const u8, len: usize) -> Ordering {
        if f & KFLAG_HOSTED != 0 {
            return tail_pair(*(self.far as *const u32).add(2 * self.nkey + k), coll, f & KFLAG_EQTIE != 0, f & KFLAG_USER != 0, sa, sb, len);
        }
        if len < WORD_WIDTH as usize {
            return core::slice::from_raw_parts(sa, len).cmp(core::slice::from_raw_parts(sb, len));
        }
        if len != VAR_WIDTH as usize {
            return Ordering::Equal; // as `tail`: a WORD_WIDTH entry is laid only with its comparator
        }
        var_cmp_as(coll, f & KFLAG_BPCHAR != 0, var_payload(sa), var_payload(sb))
    }

    /// One key's order: its flags, its word, then (a present value with an image) the image's, reversed when descending;
    /// `None` when the key ties (the next key decides).
    #[inline(always)]
    fn step(f: [u64; 2], w: [u64; 2], len: usize, desc: bool, img: impl FnOnce() -> Ordering) -> Option<Ordering> {
        if f[0] != f[1] {
            return Some(f[0].cmp(&f[1]));
        }
        if w[0] != w[1] {
            return Some(w[0].cmp(&w[1]));
        }
        if len == 0 || f[0] != 1 {
            return None;
        }
        let o = img();
        (o != Ordering::Equal).then(|| if desc { o.reverse() } else { o })
    }

    /// D1581 (G4): key 0 is a by-reference key its tail compares by payload (not hosted): a sort may order `Head`s.
    pub(crate) fn heads_ok(&self) -> bool {
        self.nkey != 0 && self.key(0).1 == VAR_WIDTH as usize && self.hosted & 1 == 0
    }

    /// D1581 (G4): row `i`'s head: key 0's flags and word and its payload's address and length (none for a NULL), so a
    /// compare reads the payloads alone, as PostgreSQL's SortTuple `datum1`; the row only on a tie of key 0.
    /// # Safety
    /// `row` is run row `i` of the table; `heads_ok()`.
    pub(crate) unsafe fn head(&self, row: *const u8, i: u32) -> Head {
        let (f, w) = (*(row as *const u64), *(row as *const u64).add(1));
        let (p, n) = if f == 1 { let s = var_payload(row.add(self.key(0).0)); (s.as_ptr(), s.len() as u32) } else { (core::ptr::null(), 0) };
        debug_assert!(f <= 2 && n <= Head::LEN, "a key 0 flag past NULLS LAST or a payload past a varlena's");
        Head { w, p, nf: n | (f as u32) << 30, i }
    }

    /// D1581 (G4): `cmp` of the rows of two heads: key 0 by the heads (`step` and `var_cmp`, the rows' own code), the
    /// later keys by the rows.
    /// # Safety
    /// The heads are `head`s of this table's live run, `row` its rows by index.
    pub(crate) unsafe fn head_cmp(&self, x: &Head, y: &Head, row: &dyn Fn(u32) -> *const u8) -> Ordering {
        let (_, len, desc) = self.key(0);
        match Self::step([x.f(), y.f()], [x.w, y.w], len, desc, || self.var_cmp(0, x.bytes(), y.bytes())) {
            Some(o) => o,
            None => self.cmp_from(1, row(x.i), row(y.i)),
        }
    }

    /// A hosted key's tail (out of line: a non-hosted sort's compare keeps its code).
    #[cold]
    #[inline(never)]
    unsafe fn hosted_tail(&self, k: usize, sa: *const u8, sb: *const u8, len: usize) -> Ordering {
        tail_pair(*(self.procs as *const u32).add(k), self.coll[k], self.eqtie >> k & 1 != 0, self.user >> k & 1 != 0, sa, sb, len)
    }

    /// Key `k`'s tail on a tie of its word `w` (both rows').
    unsafe fn tail(&self, k: usize, w: u64, sa: *const u8, sb: *const u8, len: usize) -> Ordering {
        if self.hosted >> k & 1 != 0 {
            // a word the key marks whole (`KFLAG_WORDEXACT`: its low bit clear, set under DESC) is the value: no payload read
            if self.wexact >> k & 1 != 0 && w & 1 == self.key(k).2 as u64 {
                return Ordering::Equal;
            }
            // equal copies are equal values (a btree order is reflexive): a tie of exact words (a packed numeric) never calls
            return self.hosted_tail(k, sa, sb, len);
        }
        if len != VAR_WIDTH as usize {
            return Ordering::Equal; // a WORD_WIDTH entry without its comparator: never laid (runs/keylane.rs pairs it with one)
        }
        self.var_cmp(k, var_payload(sa), var_payload(sb))
    }

    /// A by-reference key's payloads compared (`tail`'s, a head's): a bpchar's trailing blanks trimmed, then the bytes or the
    /// collation's comparator.
    fn var_cmp(&self, k: usize, x: &[u8], y: &[u8]) -> Ordering {
        var_cmp_as(self.coll[k], self.bpchar >> k & 1 != 0, x, y)
    }
}
/// A hosted key's tail pair (`RowCmp::hosted_tail`, `far_tail`) ordered by KeySem (`keysem::call::order_pair`, which
/// decides when equal copies tie with no call): `proc` (a resolution id) under `coll` over the var copies' Datums,
/// (`WORD_WIDTH`) the datum words, or (a fixed image, under `WORD_WIDTH`: a `name` under a non-C collation) the images'
/// addresses, a by-reference Datum, with how the copies compare as bytes.
unsafe fn tail_pair(proc: u32, coll: u32, eqtie: bool, user: bool, sa: *const u8, sb: *const u8, len: usize) -> Ordering {
    let (da, db, copies) = if len == VAR_WIDTH as usize {
        let (pa, pb) = (var_payload(sa), var_payload(sb));
        (var_datum(sa) as u64, var_datum(sb) as u64, if !user && pa == pb { call::Copies::SameVar(pa) } else { call::Copies::Differ })
    } else if len < WORD_WIDTH as usize {
        let same = !user && core::slice::from_raw_parts(sa, len) == core::slice::from_raw_parts(sb, len);
        (sa as usize as u64, sb as usize as u64, if same { call::Copies::Same } else { call::Copies::Differ })
    } else {
        // D3238: the datum word is 8 bytes on every target (a usize read is its low half on wasm32)
        let (da, db) = ((sa as *const u64).read_unaligned(), (sb as *const u64).read_unaligned());
        (da, db, if !user && da == db { call::Copies::Same } else { call::Copies::Differ })
    };
    call::order_pair(proc, coll, eqtie, user, copies, da, db).unwrap_or_else(order_failed)
}
/// The copy a var slot `[size, off]` points at, as a Datum: a varlena behind its 4-byte header (`image_into`'s `size << 2`).
unsafe fn var_datum(slot: *const u8) -> usize {
    let off = (slot.add(4) as *const u32).read_unaligned() as usize;
    slot.add(off) as usize
}
/// The payload of the copy a var slot `[size, off]` points at: `off` counts from the slot itself (a row and its slots move together).
unsafe fn var_payload(slot: *const u8) -> &'static [u8] {
    let size = (slot as *const u32).read_unaligned() as usize;
    let off = (slot.add(4) as *const u32).read_unaligned() as usize;
    core::slice::from_raw_parts(slot.add(off + 4), size - 4)
}
fn stash_bytes(row_bytes: usize) -> usize {
    8 + row_bytes.next_multiple_of(8)
}
pub(crate) unsafe fn stash_of(own: *mut u8) -> *mut u8 {
    let m = &*(own as *const runmerge::RunMerge);
    let n = m.nruns as usize;
    own.add(size_of::<runmerge::RunMerge>() + n * size_of::<runmerge::RunCur>() + ((n * 4 + 7) & !7) + n * m.buf as usize)
}
/// The winner copied whole into the stash: `rb` bytes, or a var record's length word at `rec_off` when `rb` is 0.
/// D1328: never past the stash's room (a record wider than it is a layout bug, raised by name, never a write past the region).
pub(crate) unsafe fn stash_row(_ctx: *mut Ctx, row: *const u8) -> i64 {
    let (dst, rb, rec_off, room) = STASH.with(|c| c.get());
    let n = if rb == 0 { (row.add(rec_off) as *const u32).read_unaligned() as usize } else { rb };
    if n > room {
        return stash_overflow(n, room);
    }
    core::ptr::copy_nonoverlapping(row, dst, n);
    HOST_STOP
}
#[cold]
fn stash_overflow(n: usize, room: usize) -> i64 {
    bare(&format!("Bug: a sort merge's emit stash of {room} bytes under a record of {n} (D1328)")) // alloc: Cold
}
/// D1327: `stash_row` once a run's buffer grew past the region (`runmerge::grow`): a record longer than the region's stash
/// goes to the big stash (drawn as the big buffers, held until the statement's end, grown as needed).
#[cold]
unsafe fn stash_big(ctx: *mut Ctx, row: *const u8) -> i64 {
    let (dst, _, rec_off, room) = STASH.with(|c| c.get());
    let (_, mut big, mut cap, _, own) = BIG.with(|c| c.get());
    let n = (row.add(rec_off) as *const u32).read_unaligned() as usize;
    if n <= room {
        core::ptr::copy_nonoverlapping(row, dst, n);
        BIG.with(|c| c.set((room, big, cap, false, own)));
        return HOST_STOP;
    }
    if n > cap {
        cap = n.next_power_of_two();
        let w = [own as u64, cap as u64];
        big = spill(ctx, gravity_abi::program::SPILL_MERGE_BIG, w.as_ptr() as i64) as *mut u8;
        if big.is_null() {
            return ERROR;
        }
    }
    core::ptr::copy_nonoverlapping(row, big, n);
    BIG.with(|c| c.set((room, big, cap, true, own)));
    HOST_STOP
}
pub(crate) unsafe fn point_images(row: *mut u8, tbl: *const u32) {
    let (_, nkey, natts, _) = tbl_shape(tbl);
    let (dv, dn) = (8 * nkey, 8 * nkey + 8 * natts);
    for i in 0..*tbl.add(3) as usize {
        let e = tbl.add(SORT_TBL_HDR + 3 * i);
        let (col, off) = (*e as usize, *e.add(1) as usize);
        if *row.add(dn + col) == 0 {
            *(row.add(dv + 8 * col) as *mut u64) = row.add(off) as u64;
        }
    }
    if let Some(v) = var_shape(tbl) {
        for c in 0..v.nvar {
            let (col, so) = (*v.slots.add(2 * c) as usize, *v.slots.add(2 * c + 1) as usize);
            let slot = row.add(so);
            if (slot as *const u32).read_unaligned() != 0 {
                let off = (slot.add(4) as *const u32).read_unaligned() as usize;
                *(row.add(dv + 8 * col) as *mut u64) = slot.add(off) as u64;
            }
        }
    }
}
crate::contract::host! {
    /// The emit's open: `SORT_MODE_MEM` (the sealed run read in place), `SORT_MODE_MERGE` (the merge laid behind the stash on a fresh call, continued on a resume), `DONE` with nothing to send (an empty run is never sealed), `< 0` an error.
    pub unsafe fn sort_begin(ctx: *mut Ctx, tbl: *const u32) -> i64 {
        guard!(ERROR, begin_in(ctx, partial(ctx), tbl, 0))
    }
}
crate::contract::host! {
    /// D457 (V4R-124): `sort_begin` over consumed region `k`, the sort of slot `slot` feeding a pipeline whose own region is
    /// its breaker end's (a fold, a group): the merge laid in that region over slot `slot`'s run stream, its fills landing there.
    pub unsafe fn sort_begin_at(ctx: *mut Ctx, tbl: *const u32, k: u64, slot: u64) -> i64 {
        let _ = slot; // D1324: the stream is the consumed region's, resolved by the driver (a baked slot goes stale)
        guard!(ERROR, begin_in(ctx, consumed(ctx, k as usize), tbl, gravity_abi::program::STREAM_CONSUMED | k))
    }
}
/// D1061: the merge an earlier reader of the same run laid over `own` (each reader of a run K pipelines consume lays
/// its own at its open, over the bytes the first found); `None` over a run, an armed region or a merge no emit laid.
pub(crate) unsafe fn relaid<'a>(own: *const u8) -> Option<&'a runmerge::RunMerge> {
    let m = &*(own as *const runmerge::RunMerge);
    let hit = table_kind(own) != TABLE_SORT && m.kind == runmerge::RUNMERGE_KIND && m.tag == runmerge::EMIT_TAG;
    debug_assert!(!hit || m.heap_len < 1 << 24, "a merge's byte 15 is 0");
    hit.then_some(m)
}
/// F3 (b): the reader a merge is laid for (`RunMerge::reader`): its pipeline's own region beside its table.
pub(crate) unsafe fn reader_of(ctx: *mut Ctx, tbl: *const u32) -> u64 {
    partial(ctx) as u64 ^ (tbl as u64).rotate_left(32)
}
/// D2316 (op 14 "held at len" for runs.incremental, exec ops/sort/held.rs `stamp_at`): an Incremental Sort's fill that
/// raised left its rows in its region, every closed prefix group ordered and the open one as it came. Its reader reads
/// only the rows PostgreSQL's batches handed on before the raising row (whole closed groups, which order before every
/// later row) and raises the held error past them, so the run is sealed here as the fill's end would have sealed it:
/// sorted whole in place. `RUN_HELD` stays for that raise (`sort_held_end`). A fill that put runs before it raised (its
/// rows past the region's room) is not read in parts: UNSUPPORTED. lowerv4 holds fixed rows only (a var row's run may
/// hold a big run in flight).
unsafe fn held_at(ctx: *mut Ctx, run: *mut SortRun, tbl: *const u32, stream: u64) -> i64 {
    (*run).pad[2] &= !RUN_HELD_AT;
    match spill(ctx, SPILL_RUNS, stream as i64) {
        n if n < 0 => return n,
        0 => {}
        _ => return unsupported(UNSUPPORTED_SHAPE),
    }
    let (row_bytes, nkey, _, spec) = tbl_shape(tbl);
    if var_shape(tbl).is_some() || (*run).cap == 0 || (*run).row_bytes as usize != row_bytes || (*run).len > (*run).cap {
        return bare("Bug: a run held at its rows that is not a fixed row's open run (D2316)");
    }
    let st = sort_rows(ctx, run, nkey, spec, Road::Seal);
    if st < 0 {
        return st;
    }
    (*run).pad[0] = SEALED;
    0
}
crate::contract::host! {
    /// D2316: the end of a reader's rows over a run held at its rows (`RUN_HELD_AT`), region `k` (a consumed index;
    /// `u64::MAX` the reader's own region): the held error raised (`raise_held`, stashed again on this thread) and 1
    /// answers, for the IR's raise exit, where PostgreSQL's Incremental Sort reads on into the raising row; 0 for a run
    /// not held (its fill ended), the reader ends.
    pub unsafe fn sort_held_end(ctx: *mut Ctx, k: u64) -> i64 {
        let own = if k == u64::MAX { partial(ctx) } else { consumed(ctx, k as usize) };
        if own.is_null() || table_kind(own) != TABLE_SORT || (*(own as *const SortRun)).pad[2] & RUN_HELD == 0 {
            return 0;
        }
        (gravity_abi::program::raise_held(ctx) != 0) as i64
    }
}
/// The emit's open over `own`, the merge over run stream `stream` (0 the pipeline's own; else `slot + 1`, the fills based at `own`).
unsafe fn begin_in(ctx: *mut Ctx, own: *mut u8, tbl: *const u32, stream: u64) -> i64 {
    {
        let word = (*ctx).resume;
        // D449: a held run (op 14 for runs.sort: its fill raised, exec ops/sort/held.rs) has nothing to send, never its
        // partial rows or put runs; its reader raises the held error at the run's end (`m11_run_held`). D2316: one held
        // at its rows (`RUN_HELD_AT`) is sealed here and read as any sealed run, up to its reader's count
        if word == RESUME_NONE && table_kind(own) == TABLE_SORT && (*(own as *const SortRun)).pad[2] & RUN_HELD != 0 {
            if (*(own as *const SortRun)).pad[2] & RUN_HELD_AT == 0 {
                return DONE;
            }
            match held_at(ctx, own as *mut SortRun, tbl, stream) {
                0 => {}
                e => return e,
            }
        }
        let (row_bytes, nkey, _, spec) = tbl_shape(tbl);
        let resumes_mem = word != RESUME_NONE && resume::position(word) & MERGING == 0;
        if word == RESUME_NONE && sealed(own, row_bytes) || resumes_mem {
            return SORT_MODE_MEM;
        }
        if word == RESUME_NONE {
            if stream != 0 && spill(ctx, SPILL_RUN_BASE, own as i64) < 0 {
                return ERROR;
            }
            // F3 (b): the reader, its own region beside its table (both its pipeline's, call after call); a merge it laid
            // and left in flight is its own run before's, over and not resumed (a Merge Join that stopped reading, a
            // recursive term's generation rerun over a Sort kept whole, which PostgreSQL rewinds): its run readers are
            // closed and the merge laid again from the runs. Another reader's merge in flight stays a Bug (D1061).
            let reader = reader_of(ctx, tbl);
            let usable = match relaid(own) {
                Some(m) if m.active != 0 && m.reader == reader => match super::super::raw::abandon(ctx, own) {
                    0 => m.region as usize,
                    e => return e,
                },
                Some(m) if m.active != 0 => return bare("Bug: a sort emit's merge laid over another reader's merge in flight (D1061)"),
                Some(m) => m.region as usize,
                None => SortRun::region_bytes(own, row_bytes as u32),
            };
            let opened = match var_shape(tbl) {
                None => {
                    let sb = stash_bytes(row_bytes);
                    if usable < sb {
                        return bare("a sort region smaller than its row stash");
                    }
                    let cmp = |a: *const u8, b: *const u8| cmp_rows(nkey / 2, spec, a, b);
                    runmerge::open_by(ctx, own, usable - sb, row_bytes, nkey as u16, true, stream, &cmp)
                }
                Some(v) => {
                    // the stash holds one record: a run buffer's worth, the merge laid over the rest
                    let nruns = match spill(ctx, SPILL_RUNS, stream as i64) {
                        n if n < 0 => return n,
                        n => n as usize,
                    };
                    // D1327: the stash as wide as a run's buffer at the merge's fan-in (the runs past it merged in passes)
                    let n = runmerge::var_emit_fan_in(usable, row_bytes, nruns);
                    let fixed = runmerge::fixed_bytes(n);
                    if usable < fixed + 16 + 8 * (n + 1) {
                        return unsupported(UNSUPPORTED_SCRATCH);
                    }
                    let sb = runmerge::var_stash_bytes(usable, n);
                    let rc = RowCmp::of(tbl);
                    let cmp = |a: *const u8, b: *const u8| rc.cmp(a, b);
                    // D1293: the open's tree compares the runs' heads; a failed one raises here (`next_in` clears
                    // `CMP_ERR` before its drain, so the open's failure was lost and the first rows came out misordered)
                    CMP_ERR.with(|c| c.set(false));
                    let opened = runmerge::open_var_in(ctx, own, usable - sb, row_bytes, v.rec_off as u16, stream, &cmp);
                    if CMP_ERR.with(|c| c.get()) {
                        return cmp_failed("a sort merge compare failed");
                    }
                    opened
                }
            };
            match opened {
                Err(e) => return e,
                Ok(false) => return DONE, // nothing to send and no flush: an embedded inner's open would push its OWNER's out rows and drop the receiver's stop (D1339 row B S1)
                Ok(true) => {}
            }
            let m = &mut *(own as *mut runmerge::RunMerge);
            (m.region, m.active, m.tag, m.reader) = (usable.min(u32::MAX as usize) as u32, 1, runmerge::EMIT_TAG, reader);
            *(stash_of(own) as *mut u64) = 0;
        } else if resume::position(word) & MERGING == 0 {
            return bare("a sort resumed outside its merge");
        }
        SORT_MODE_MERGE
    }
}
crate::contract::host! {
    /// The spool reader's open (M8 C.0), `sort_begin`'s mode as a pointer: `SORT_MODE_MEM` → the sealed run (`len` at its header, the rows at `rows_ptr`), `SORT_MODE_MERGE` → null (the merge laid; `sort_next` drains it), a status below zero as the word.
    pub unsafe fn spool_begin(ctx: *mut Ctx, tbl: *const u32) -> *const u8 {
        match sort_begin(ctx, tbl) {
            SORT_MODE_MEM => partial(ctx) as *const u8,
            SORT_MODE_MERGE => core::ptr::null(),
            st => st as *const u8,
        }
    }
}
crate::contract::host! {
    /// The spool's full run (M8 L1): the capacity doubled within its region, else the named error — a spool never puts a run.
    pub unsafe fn spool_full(_ctx: *mut Ctx, run: *mut u8) -> i64 {
        guard!(ERROR, {
            if (*(run as *mut SortRun)).grow() { 0 } else { bare(SPOOL_SPILLED_WHAT) }
        })
    }
}
crate::contract::host! {
    /// The spool producer's end (serial, M8 L1): the run sealed in place, unsorted, every by-reference datum pointed at its image (the rows never move after).
    pub unsafe fn spool_last(ctx: *mut Ctx, run: *mut u8, tbl: *const u32) -> i64 {
        guard!(ERROR, {
            let r = &mut *(run as *mut SortRun);
            if (*ctx).width != 1 {
                return bare("a spool filled by more than one worker (M8 L5a)");
            }
            for i in 0..r.len as usize {
                point_images(r.row(i) as *mut u8, tbl);
            }
            r.pad[0] = SEALED;
            0
        })
    }
}
/// The run's images point into it where it lies now: the first non-NULL by-reference cell of the run (D2339 / D2374: of
/// the first row that has one, not of row 0 alone — a run sealed sorted by `last` / `last_var` is never pointed at its
/// seal, and a row 0 whose by-reference cells are all NULL read "pointed") against its image (a run copied whole — the
/// rig's leader — is pointed again). A run with no non-NULL by-reference cell has nothing to point. A table with no
/// in-row image entry whose run reserved no var copy (its var top still at the rows' end, not a dense put form) has no
/// non-NULL by-reference cell at all: every var copy is a `reserve_var` reservation below that end, so the answer is
/// read off the trailer, never by walking every row (a merge join's inner run whose 9 varlena
/// columns the scan never reads walked all its rows at each of the join's views).
pub(crate) unsafe fn spool_pointed(r: &SortRun, tbl: *const u32) -> bool {
    spool_pointed_in(r, tbl, 0, r.len as usize)
}

/// `spool_pointed` over rows `lo..hi` of the run alone (D3190: a co-split claim's inner slice).
/// # Safety
/// As `spool_pointed`; `hi` is at most the run's rows.
pub(crate) unsafe fn spool_pointed_in(r: &SortRun, tbl: *const u32, lo: usize, hi: usize) -> bool {
    if *tbl.add(3) == 0 && r.var_bytes() == 0 && r.var_top() as usize == SORTRUN_HDR + r.cap as usize * r.row_bytes as usize {
        return true;
    }
    let (_, nkey, natts, _) = tbl_shape(tbl);
    let (dv, dn) = (8 * nkey, 8 * nkey + 8 * natts);
    let var = var_shape(tbl);
    // a table naming no image entry and no var section (a lean view's) has nothing to point: no row walk
    if *tbl.add(3) == 0 && var.is_none_or(|v| v.nvar == 0) {
        return true;
    }
    for j in lo..hi {
        let row = r.row(j) as *mut u8;
        for i in 0..*tbl.add(3) as usize {
            let e = tbl.add(SORT_TBL_HDR + 3 * i);
            let (col, off) = (*e as usize, *e.add(1) as usize);
            if *row.add(dn + col) == 0 {
                return *(row.add(dv + 8 * col) as *const u64) == row.add(off) as u64;
            }
        }
        if let Some(v) = var {
            for c in 0..v.nvar {
                let (col, so) = (*v.slots.add(2 * c) as usize, *v.slots.add(2 * c + 1) as usize);
                let slot = row.add(so);
                if (slot as *const u32).read_unaligned() != 0 {
                    let off = (slot.add(4) as *const u32).read_unaligned() as usize;
                    return *(row.add(dv + 8 * col) as *const u64) == slot.add(off) as u64;
                }
            }
        }
    }
    true
}
crate::contract::host! {
    /// The spool reader's view (M8 L1): consumed region `k` when it holds a sealed run (the pointer walk reads it; its images pointed again only when the run was copied since `spool_last`), an empty run when the producer never opened it, else the named error as the word.
    pub unsafe fn spool_view(ctx: *mut Ctx, k: u64, tbl: *const u32) -> *const u8 {
        guard!(ERROR as *const u8, {
            let r = consumed(ctx, k as usize) as *mut SortRun;
            if r.is_null() || (*r).cap == 0 {
                return SPOOL_EMPTY.as_ptr() as *const u8;
            }
            if (*r).kind == TABLE_SORT && (*r).pad[0] == SEALED && (*r).len <= (*r).cap {
                if !spool_pointed(&*r, tbl) {
                    for i in 0..(*r).len as usize {
                        point_images((*r).row(i) as *mut u8, tbl);
                    }
                }
                return r as *const u8;
            }
            bare(SPOOL_SPILLED_WHAT) as *const u8
        })
    }
}
/// D3190 (merge.cosplit): `spool_view` of rows `[lo, hi)` alone (each clamped to the run's rows): only those rows' images
/// are pointed (when they read unpointed), so workers reading disjoint slices of one sealed run write disjoint rows.
/// # Safety
/// As `spool_view`'s: consumed region `k` of `ctx` and the run's table `tbl`.
pub unsafe fn spool_view_slice(ctx: *mut Ctx, k: u64, tbl: *const u32, lo: u64, hi: u64) -> *const u8 {
    let r = consumed(ctx, k as usize) as *mut SortRun;
    if r.is_null() || (*r).cap == 0 {
        return SPOOL_EMPTY.as_ptr() as *const u8;
    }
    if (*r).kind == TABLE_SORT && (*r).pad[0] == SEALED && (*r).len <= (*r).cap {
        let n = (*r).len as u64;
        let (lo, hi) = (lo.min(n) as usize, hi.min(n) as usize);
        if lo < hi && !spool_pointed_in(&*r, tbl, lo, hi) {
            for i in lo..hi {
                point_images((*r).row(i) as *mut u8, tbl);
            }
        }
        return r as *const u8;
    }
    bare(SPOOL_SPILLED_WHAT) as *const u8
}
/// D3190 (merge.cosplit over a put outer): `begin_in`'s fresh merge for one co-split claim, over `area` (`usable` bytes this
/// owner drew, sized for every run's buffer in one merge: no pass) in place of the consumed region every worker reads:
/// the merge of the `nruns` slices `open(r)` opens (`SPILL_RUN_SLICE`, run r's rows of the claim's key range), the fills
/// based at the area. A merge this owner's earlier claim left in flight over the area is given up first. `SORT_MODE_MERGE`
/// (`next_slices` drains it), `DONE` when every slice is empty, a status below zero else.
/// # Safety
/// `area` is `usable` bytes this owner holds for the statement; `tbl` the run's table.
pub unsafe fn begin_slices(ctx: *mut Ctx, area: *mut u8, usable: usize, tbl: *const u32, nruns: usize, open: &dyn Fn(usize) -> i64) -> i64 {
    let (row_bytes, nkey, _, spec) = tbl_shape(tbl);
    if spill(ctx, SPILL_RUN_BASE, area as i64) < 0 {
        return ERROR;
    }
    if let Some(m) = relaid(area) {
        if m.active != 0 {
            match super::super::raw::abandon(ctx, area) {
                0 => {}
                e => return e,
            }
        }
    }
    let opened = match var_shape(tbl) {
        None => {
            let sb = stash_bytes(row_bytes);
            if usable < sb {
                return bare("a co-split merge area smaller than its row stash");
            }
            let cmp = |a: *const u8, b: *const u8| cmp_rows(nkey / 2, spec, a, b);
            runmerge::open_slices(ctx, area, usable - sb, row_bytes, nkey as u16, None, nruns, open, &cmp)
        }
        Some(v) => {
            // the stash as wide as a run's buffer (`begin_in`'s var layout at a fan-in of every run)
            if runmerge::var_emit_fan_in(usable, row_bytes, nruns) < nruns {
                return unsupported(UNSUPPORTED_SCRATCH);
            }
            let sb = runmerge::var_stash_bytes(usable, nruns);
            let rc = RowCmp::of(tbl);
            let cmp = |a: *const u8, b: *const u8| rc.cmp(a, b);
            CMP_ERR.with(|c| c.set(false));
            let opened = runmerge::open_slices(ctx, area, usable - sb, row_bytes, 0, Some(v.rec_off as u16), nruns, open, &cmp);
            if CMP_ERR.with(|c| c.get()) {
                return cmp_failed("a sort merge compare failed");
            }
            opened
        }
    };
    match opened {
        Err(e) => return e,
        Ok(false) => return DONE,
        Ok(true) => {}
    }
    let m = &mut *(area as *mut runmerge::RunMerge);
    (m.region, m.active, m.tag, m.reader) = (usable.min(u32::MAX as usize) as u32, 1, runmerge::EMIT_TAG, reader_of(ctx, tbl));
    *(stash_of(area) as *mut u64) = 0;
    SORT_MODE_MERGE
}
/// D3190: the bytes `begin_slices` lays a merge of `nruns` slices of `tbl`'s runs over: every run's buffer `SLICE_BUF`
/// bytes (at least its least buffer, whole rows of a fixed run), the stash beside them (a var run's as wide as a buffer).
/// # Safety
/// `tbl` is a sort run's table.
pub unsafe fn slices_area_bytes(tbl: *const u32, nruns: usize) -> usize {
    let (row_bytes, ..) = tbl_shape(tbl);
    let var = var_shape(tbl).is_some();
    let unit = if var { 8 } else { row_bytes.max(8) };
    let buf = SLICE_BUF.max(runmerge::min_buf(row_bytes, var)).next_multiple_of(unit);
    runmerge::fixed_bytes(nruns) + (nruns + 1) * buf + stash_bytes(row_bytes) + 64
}
/// D3190: a co-split claim's merge buffer per run (`slices_area_bytes`): a fill reads at most 64 kB, a slice's reader
/// decodes the run's blocks as it fills.
const SLICE_BUF: usize = 16 << 10;
/// D3190: `sort_next_at`'s `SORT_MODE_MERGE` row for a co-split claim's merge over `area` (`begin_slices`'), the fills
/// based there; `stream` the outer's (`STREAM_CONSUMED | k`).
/// # Safety
/// As `begin_slices`, after it answered `SORT_MODE_MERGE`.
pub unsafe fn next_slices(ctx: *mut Ctx, area: *mut u8, tbl: *const u32, i: u64, stream: u64) -> *const u8 {
    next_in(ctx, area, tbl, SORT_MODE_MERGE as u64, i, stream)
}
crate::contract::host! {
    /// The next sorted row (its image datums pointed inside it), null at the end, a status below zero as the word: `SORT_MODE_MEM` reads row `i` of the sealed run, `SORT_MODE_MERGE` pulls the merge's winner into the stash (the held row first).
    pub unsafe fn sort_next(ctx: *mut Ctx, tbl: *const u32, mode: u64, i: u64) -> *const u8 {
        guard!(ERROR as *const u8, next_in(ctx, partial(ctx), tbl, mode, i, 0))
    }
}
crate::contract::host! {
    /// V4R-136 (D463): the rows the emit opened in `mode` delivers in all, the emit window's exact end: `SORT_MODE_MEM` the
    /// sealed run's length, `SORT_MODE_MERGE` the rows of the stream's runs (the driver's count at each put / adoption), any
    /// other mode 0. Fixed for the statement: an IR files it beside the mode and reuses it on a resume.
    pub unsafe fn sort_total(ctx: *mut Ctx, _tbl: *const u32, mode: u64) -> i64 {
        guard!(ERROR, rows_in(ctx, partial(ctx), mode, 0))
    }
}
crate::contract::host! {
    /// V4R-136: `sort_total` of `sort_begin_at`'s emit (consumed region `k`, slot `slot`'s stream).
    pub unsafe fn sort_total_at(ctx: *mut Ctx, _tbl: *const u32, mode: u64, k: u64, slot: u64) -> i64 {
        let _ = slot; // D1324: as `sort_begin_at`
        guard!(ERROR, rows_in(ctx, consumed(ctx, k as usize), mode, gravity_abi::program::STREAM_CONSUMED | k))
    }
}
unsafe fn rows_in(ctx: *mut Ctx, own: *mut u8, mode: u64, stream: u64) -> i64 {
    match mode as i64 {
        SORT_MODE_MEM => (*(own as *const SortRun)).len as i64,
        SORT_MODE_MERGE => spill(ctx, SPILL_RUN_ROWS, stream as i64),
        _ => 0,
    }
}
crate::contract::host! {
    /// D457: `sort_next` over consumed region `k` (`sort_begin_at`'s), the fills of slot `slot`'s runs based there.
    pub unsafe fn sort_next_at(ctx: *mut Ctx, tbl: *const u32, mode: u64, i: u64, k: u64, slot: u64) -> *const u8 {
        let _ = slot; // D1324: as `sort_begin_at`
        guard!(ERROR as *const u8, next_in(ctx, consumed(ctx, k as usize), tbl, mode, i, gravity_abi::program::STREAM_CONSUMED | k))
    }
}
pub(crate) unsafe fn next_in(ctx: *mut Ctx, own: *mut u8, tbl: *const u32, mode: u64, i: u64, stream: u64) -> *const u8 {
    {
        let (row_bytes, nkey, _, spec) = tbl_shape(tbl);
        let row = if mode as i64 == SORT_MODE_MEM {
            let run = &*(own as *const SortRun);
            if i >= run.len as u64 {
                return core::ptr::null();
            }
            run.row(i as usize) as *mut u8
        } else {
            let hold = stash_of(own) as *mut u64;
            let stash = (hold as *mut u8).add(8);
            let m = &*(own as *const runmerge::RunMerge);
            // D1301: a stop inside row `i` (a write's recheck, an expanding step's offset) re-reads row `i` on its resume,
            // as SORT_MODE_MEM re-reads it by index: the merge's last row, still in the stash, never the next winner
            debug_assert!(i <= m.emitted && i + 1 >= m.emitted, "a sort merge's emit asked for row {i} after {} rows", m.emitted);
            if *hold != 0 || (m.emitted != 0 && i + 1 == m.emitted) {
                *hold = 0;
                return if m.big_last != 0 { m.big_stash as *const u8 } else { stash };
            }
            let var = var_shape(tbl);
            let m = &*(own as *const runmerge::RunMerge);
            let room = (own as usize + m.region as usize).saturating_sub(stash as usize);
            STASH.with(|c| c.set((stash, if var.is_some() { 0 } else { row_bytes }, var.map_or(0, |v| v.rec_off), room)));
            let big = {
                // D1327: a run's buffer wider than the stash (a merge after passes lays fewer runs than the fan-in the stash
                // was cut for) holds records the stash cannot: the big stash takes them, as it takes a grown buffer's
                let big = m.big_seen != 0 || (var.is_some() && m.buf as usize > room);
                if big {
                    BIG.with(|c| c.set((room, m.big_stash as *mut u8, m.big_cap as usize, false, own as usize)));
                }
                big
            };
            CMP_ERR.with(|c| c.set(false));
            let rc = RowCmp::of(tbl);
            let cmp = |a: *const u8, b: *const u8| if var.is_some() { rc.cmp(a, b) } else { cmp_rows(nkey / 2, spec, a, b) };
            if stream != 0 && spill(ctx, SPILL_RUN_BASE, own as i64) < 0 {
                return ERROR as *const u8;
            }
            let r = runmerge::drain_by(ctx, own, &cmp, if big { stash_big } else { stash_row });
            if CMP_ERR.with(|c| c.get()) {
                return cmp_failed("a sort merge compare failed") as *const u8;
            }
            match r {
                HOST_STOP => {
                    // the emit stops at `sort_total` rows: the merge's last row ends its reader
                    let m = &mut *(own as *mut runmerge::RunMerge);
                    if m.heap_len == 0 {
                        m.active = 0;
                    }
                    if big {
                        let (_, p, cap, last, _) = BIG.with(|c| c.get());
                        (m.big_stash, m.big_cap, m.big_last) = (p as u64, cap as u32, last as u8);
                        if last {
                            let row = p;
                            point_images(row, tbl);
                            return row;
                        }
                    }
                    stash
                }
                DONE => {
                    (*(own as *mut runmerge::RunMerge)).active = 0;
                    return core::ptr::null();
                }
                e => return (if e < 0 { e } else { ERROR }) as *const u8,
            }
        };
        point_images(row, tbl);
        row
    }
}
crate::contract::host! {
    /// The sink's stop after row `i` (`next` = `i + 1`): the resume word placed (`hold`: the row was not stored, the resume re-delivers it), the call suspended.
    pub unsafe fn sort_stop(ctx: *mut Ctx, _tbl: *const u32, mode: u64, next: u64, hold: u64) -> i64 {
        guard!(ERROR, stop_in(ctx, partial(ctx), mode, next, hold))
    }
}
crate::contract::host! {
    /// D457: `sort_stop` over consumed region `k` (`sort_begin_at`'s).
    pub unsafe fn sort_stop_at(ctx: *mut Ctx, _tbl: *const u32, mode: u64, next: u64, hold: u64, k: u64) -> i64 {
        guard!(ERROR, stop_in(ctx, consumed(ctx, k as usize), mode, next, hold))
    }
}
unsafe fn stop_in(ctx: *mut Ctx, own: *mut u8, mode: u64, next: u64, hold: u64) -> i64 {
    {
        if mode as i64 == SORT_MODE_MEM {
            (*ctx).resume = resume::word(resume::EMIT, next - hold);
        } else {
            let emitted = (*(own as *const runmerge::RunMerge)).emitted;
            *(stash_of(own) as *mut u64) = hold;
            (*ctx).resume = resume::word(resume::EMIT, MERGING | (emitted & (MERGING - 1)));
        }
        util::stopped(ctx)
    }
}

#[cfg(test)]
mod cross_tests {
    use super::*;

    /// Rows `[flags, word]` then an image: table A lays key 0's 4-byte image at 16, table B at 24 (another column first).
    fn row(flags: u64, word: u64, img: u32, at: usize) -> [u64; 4] {
        let mut r = [flags, word, 0, 0];
        r[at / 8] = img.to_be_bytes().iter().rev().fold(0u64, |w, &b| w << 8 | b as u64);
        r
    }

    /// D3190: rows of two tables whose key 0 is laid at different offsets compare by flags, word, then each one's own
    /// image, as one table's rows compare; tables whose key differs in width or direction are not cross-comparable.
    #[test]
    fn rows_of_two_tables_compare_by_their_own_layouts() {
        let a = RowCmp::fixed(1, SortSpec::pack(&[(Some((16, 4)), false)]).unwrap());
        let b = RowCmp::fixed(1, SortSpec::pack(&[(Some((24, 4)), false)]).unwrap());
        unsafe {
            assert!(a.cross_ok(&b, 1) && b.cross_ok(&a, 1));
            let rows_a = [row(1, 5, 0x0102_0304, 16), row(1, 5, 0x0102_0305, 16), row(1, 6, 0, 16), row(0, 0, 0, 16), row(2, 0, 0, 16)];
            let rows_b = [row(1, 5, 0x0102_0304, 24), row(1, 5, 0x0102_0305, 24), row(1, 6, 0, 24), row(0, 0, 0, 24), row(2, 0, 0, 24)];
            for (i, ra) in rows_a.iter().enumerate() {
                for (j, rb) in rows_b.iter().enumerate() {
                    let cross = a.cmp_cross(ra.as_ptr() as *const u8, &b, rb.as_ptr() as *const u8, 1);
                    let same = a.cmp(ra.as_ptr() as *const u8, rows_a[j].as_ptr() as *const u8);
                    assert_eq!(cross, same, "row {i} of A against row {j} of B");
                }
            }
            let wide = RowCmp::fixed(1, SortSpec::pack(&[(Some((24, 8)), false)]).unwrap());
            let desc = RowCmp::fixed(1, SortSpec::pack(&[(Some((24, 4)), true)]).unwrap());
            assert!(!a.cross_ok(&wide, 1) && !a.cross_ok(&desc, 1), "another width or direction");
            assert!(!a.cross_ok(&b, 0) && !a.cross_ok(&b, 2), "no key, or more keys than a table has");
        }
    }
}

#[cfg(test)]
mod d4373_permute_in_place {
    use super::*;
    use gravity_abi::breaker::SORTRUN_HDR;

    const RB: usize = 16;

    /// A run of `n` rows of `RB` bytes: word 0 the row's id, a var slot at byte 8 (`[size, offset from the slot]`) whose
    /// 8-byte copy (the id x 3) lies past the rows, row `i`'s at `i x 8`.
    unsafe fn run_of(n: usize) -> Vec<u64> {
        let mut buf = vec![0u64; (SORTRUN_HDR + n * RB + n * 8) / 8 + 1]; // alloc: Cold
        let run = buf.as_mut_ptr() as *mut SortRun;
        *run = SortRun { cap: n as u32, len: n as u32, row_bytes: RB as u32, pad: [0; 3], kind: 0 };
        let base = buf.as_mut_ptr() as *mut u8;
        for i in 0..n {
            let row = base.add(SORTRUN_HDR + i * RB);
            (row as *mut u64).write_unaligned(i as u64);
            let copy = SORTRUN_HDR + n * RB + i * 8;
            (base.add(copy) as *mut u64).write_unaligned(3 * i as u64);
            (row.add(8) as *mut u32).write_unaligned(8);
            (row.add(12) as *mut u32).write_unaligned((copy - (SORTRUN_HDR + i * RB + 8)) as u32);
        }
        buf
    }

    /// The copy the permutation replaced (rows gathered into a buffer in `idx`'s order, slots moved by `(i - j) x RB`).
    unsafe fn by_copy(run: *mut SortRun, idx: &[usize]) {
        let base = (*run).row(0) as *mut u8;
        let mut tmp = Vec::with_capacity(idx.len() * RB);
        for (j, &i) in idx.iter().enumerate() {
            let at = tmp.len();
            tmp.extend_from_slice(core::slice::from_raw_parts(base.add(i * RB), RB));
            let off = (tmp.as_ptr().add(at + 12) as *const u32).read_unaligned() as i64 + (i as i64 - j as i64) * RB as i64;
            (tmp.as_mut_ptr().add(at + 12) as *mut u32).write_unaligned(off as u32);
        }
        core::ptr::copy_nonoverlapping(tmp.as_ptr(), base, tmp.len());
    }

    fn shuffled(n: usize, seed: u64) -> Vec<usize> {
        let mut v: Vec<usize> = (0..n).collect();
        let mut x = seed;
        for i in (1..n).rev() {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            v.swap(i, (x >> 33) as usize % (i + 1));
        }
        v
    }

    fn shape(slots: &[u32; 2]) -> VarShape {
        VarShape { nvar: 1, rec_off: 0, slots: slots.as_ptr(), keys: core::ptr::null(), pad: (0, 0) }
    }

    #[test]
    fn the_permutation_in_place_lays_the_bytes_the_copy_laid() {
        let slots = [0u32, 8];
        for (n, seed) in [(1, 1), (2, 7), (17, 3), (1000, 11), (70_000, 5)] {
            for idx in [shuffled(n, seed), (0..n).rev().collect(), (0..n).collect()] {
                unsafe {
                    let (mut a, mut b) = (run_of(n), run_of(n));
                    let ra = a.as_mut_ptr() as *mut SortRun;
                    assert!(!permute_in_place(core::ptr::null_mut(), ra, &|j| idx[j], Some(shape(&slots)), &|| false));
                    by_copy(b.as_mut_ptr() as *mut SortRun, &idx);
                    assert!(a == b, "n {n}: the same bytes as the copy's");
                    let base = (*ra).row(0) as *const u8;
                    for j in 0..n {
                        let row = base.add(j * RB);
                        let copy = row.add(8 + (row.add(12) as *const u32).read_unaligned() as usize);
                        assert_eq!(((row as *const u64).read_unaligned(), (copy as *const u64).read_unaligned()), (idx[j] as u64, 3 * idx[j] as u64));
                    }
                }
            }
        }
    }

    #[test]
    fn a_cancel_mid_cycle_leaves_every_row_once_with_its_copy() {
        let (slots, n) = ([0u32, 8], 3 * POLL_BATCH + 5);
        let idx = shuffled(n, 9);
        unsafe {
            let mut a = run_of(n);
            let ra = a.as_mut_ptr() as *mut SortRun;
            assert!(permute_in_place(core::ptr::null_mut(), ra, &|j| idx[j], Some(shape(&slots)), &|| true), "stopped at the first poll");
            let base = (*ra).row(0) as *const u8;
            let mut seen = vec![false; n]; // alloc: Cold
            for j in 0..n {
                let row = base.add(j * RB);
                let id = (row as *const u64).read_unaligned() as usize;
                let copy = row.add(8 + (row.add(12) as *const u32).read_unaligned() as usize);
                assert_eq!((copy as *const u64).read_unaligned(), 3 * id as u64, "row {j}'s slot points at its own copy");
                assert!(!seen[id], "row {id} once");
                seen[id] = true;
            }
        }
    }

    /// The fake spill host behind a ctx: `SPILL_WORK_TRY` answered by `MODE` (a block of the grant, no room, the floor),
    /// the ops it saw and the blocks it lent.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Mode {
        Room,
        NoRoom,
        Floor,
    }

    struct Host {
        mode: Mode,
        ops: Vec<(u32, u64)>,
        lent: Vec<(usize, Vec<u64>)>,
    }

    static HOST: std::sync::Mutex<Host> = std::sync::Mutex::new(Host { mode: Mode::Room, ops: Vec::new(), lent: Vec::new() }); // alloc: Cold

    unsafe extern "C" fn spill(_ctx: *mut Ctx, op: u32, arg: u64) -> i64 {
        if op == gravity_abi::program::SPILL_ARENA_CHUNK {
            return 0; // a kernel scratch's owner ask (0 bytes, `OpArena::own_owner_kind` / `slot_owner`): no slot owner, uncharged
        }
        let mut h = HOST.lock().unwrap();
        h.ops.push((op, arg));
        match op {
            gravity_abi::program::SPILL_WORK_TRY => match h.mode {
                Mode::NoRoom => 0,
                Mode::Floor => 1,
                Mode::Room => {
                    let mut b = vec![0xA5A5_A5A5_A5A5_A5A5u64; (arg as usize).div_ceil(8)]; // alloc: Cold
                    let p = b.as_mut_ptr() as usize;
                    h.lent.push((p, b));
                    p as i64
                }
            },
            gravity_abi::program::SPILL_WORK_GIVE => {
                let i = h.lent.iter().position(|(p, _)| *p == arg as usize).expect("a give of a lent block");
                h.lent.swap_remove(i);
                0
            }
            _ => panic!("op {op}"),
        }
    }

    /// D4373 (C): the copy's draw from the grant refused (no room now), the rows are permuted in place, not refused; with
    /// room the copy is a block of the grant, given back; under the floor (and with no spill host) the allocator's copy.
    /// Every road lays the bytes the trunk's copy laid, and no road asks for more than the copy's bytes or raises.
    #[test]
    fn the_copys_refused_draw_permutes_in_place_with_the_same_bytes() {
        let slots = [0u32, 8];
        let n = 3 * POLL_BATCH + 17;
        let idx = shuffled(n, 13);
        let mut block = gravity_abi::ProgramBlock { spill: gravity_abi::word(Some(spill as unsafe extern "C" fn(*mut Ctx, u32, u64) -> i64)), ..gravity_abi::ProgramBlock::NONE };
        for mode in [Some(Mode::Room), Some(Mode::NoRoom), Some(Mode::Floor), None] {
            unsafe {
                // SAFETY: a ctx is plain words; zero is its closed state, its program block the fake host's when a mode is given
                let mut ctx: Ctx = core::mem::zeroed();
                {
                    let mut h = HOST.lock().unwrap();
                    h.ops.clear();
                    if let Some(m) = mode {
                        h.mode = m;
                    }
                }
                if mode.is_some() {
                    ctx.program = gravity_abi::word(&mut block as *mut gravity_abi::ProgramBlock);
                }
                let c: *mut Ctx = if mode.is_some() { &mut ctx } else { core::ptr::null_mut() };
                let (mut a, mut b) = (run_of(n), run_of(n));
                assert!(!permute_rows(c, a.as_mut_ptr() as *mut SortRun, &|j| idx[j], Some(shape(&slots)), &|| false), "{mode:?}: not stopped");
                by_copy(b.as_mut_ptr() as *mut SortRun, &idx);
                assert!(a == b, "{mode:?}: the bytes the copy laid");
                let h = HOST.lock().unwrap();
                let tries: Vec<u64> = h.ops.iter().filter(|o| o.0 == gravity_abi::program::SPILL_WORK_TRY).map(|o| o.1).collect();
                let gives = h.ops.iter().filter(|o| o.0 == gravity_abi::program::SPILL_WORK_GIVE).count();
                match mode {
                    None => assert!(h.ops.is_empty(), "no spill host: the allocator's copy"),
                    Some(m) => {
                        assert_eq!(tries, [(n * RB) as u64], "{m:?}: one draw of the copy's bytes, from the grant's room only");
                        assert_eq!(gives, usize::from(m == Mode::Room), "{m:?}: a lent block given back, nothing else");
                        assert!(h.lent.is_empty(), "{m:?}: no block held after");
                    }
                }
            }
        }
    }

    /// The sort phase's permutation, timed (`D4373_BENCH=<inplace|copy>:<rows>:<fixed|var>`, run alone under `perf stat -e
    /// instructions:u`): the in-place cycles against the copy (`permute_by_copy`), over a random order (a sort's index), 32-byte
    /// fixed rows or 16-byte rows with a var slot. Prints the permutation's wall time.
    #[test]
    #[ignore]
    fn bench_permute() {
        let Ok(spec) = std::env::var("D4373_BENCH") else { return };
        let f: Vec<&str> = spec.split(':').collect();
        let (how, n, var) = (f[0], f[1].parse::<usize>().unwrap(), f[2] == "var");
        let slots = [0u32, 8];
        let idx = shuffled(n, 42);
        unsafe {
            let mut a = run_of(n);
            let run = a.as_mut_ptr() as *mut SortRun;
            if !var {
                // fixed rows: the same 16 bytes, read as a fixed row (no slot to move)
                (*run).row_bytes = RB as u32;
            }
            let t = std::time::Instant::now();
            let v = var.then(|| shape(&slots));
            match how {
                "inplace" => {
                    permute_in_place(core::ptr::null_mut(), run, &|j| idx[j], v, &|| false);
                }
                _ => {
                    let mut tmp: Vec<u8> = Vec::with_capacity(n * RB);
                    permute_by_copy(run, &|j| idx[j], v, &|| false, tmp.as_mut_ptr());
                }
            }
            eprintln!("D4373_BENCH {spec} {:.3} ms", t.elapsed().as_secs_f64() * 1e3);
            std::hint::black_box(&a);
        }
    }
}
