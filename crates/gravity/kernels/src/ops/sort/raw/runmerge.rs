//! The run merge's raw half (ops/sort/runmerge.rs keeps the merge's header and cursor layouts, its sizing and pass
//! planning): the k-way merge's opens, its passes over the slot's runs, the drain to the emit and the tree of losers.
//! Moved unchanged from runmerge.rs (rule R6: the family's unsafe code lives in raw); runmerge.rs re-exports the
//! entries under their old paths.
use gravity_abi::ctx::resume;
use gravity_abi::limits::MAXORDER;
use gravity_abi::status::{suspend, unsupported, UNSUPPORTED_SCRATCH};
use gravity_abi::Ctx;
use gravity_abi::status::{DONE, ERROR};
use gravity_abi::program::{MERGE_FRESH, SPILL_MERGE_BEGIN, SPILL_MERGE_BIG, SPILL_MERGE_WRITE, SPILL_RUNS, SPILL_RUN_BASE, SPILL_RUN_FILL, SPILL_RUN_OPEN};
use gravity_abi::status::CANCELLED;
use crate::util::HOST_STOP;
use crate::util::flush;

use super::super::runmerge::{fan_in, fixed_bytes, group_len, key_words, min_buf, pass_cut, Cmp, RunCur, RunMerge, MERGING, RUNMERGE_KIND};

/// The run's buffer at `pos`.
#[inline(always)]
unsafe fn at(own: *mut u8, c: &RunCur, pos: u32) -> *mut u8 {
    own.offset(c.off as isize + pos as isize)
}
/// A row's sort key as two words compared lexicographically.
pub type KeyOf = unsafe fn(*const u8) -> (u64, u64);
/// Emit one row of a run; `HOST_STOP` when the sink said stop, `HOST_CONTINUE` else, `< 0` an error.
pub type EmitRow = unsafe fn(*mut Ctx, *const u8) -> i64;
unsafe fn spill(ctx: *mut Ctx, op: u32, arg: i64) -> i64 {
    crate::util::spill(ctx, op, arg)
}
/// Lay the merge over `own` (`usable` bytes) and fill the buffers; `Ok(false)` without runs.
/// # Safety
/// `own` is the kernel's own region, its table no longer needed.
pub unsafe fn open(ctx: *mut Ctx, own: *mut u8, usable: usize, rec: usize, key_of: KeyOf) -> Result<bool, i64> {
    open_by(ctx, own, usable, rec, 0, false, 0, &|a, b| key_of(a).cmp(&key_of(b)))
}
/// `open` over the rows' first `nkey` words (unsigned, lexicographic), the caller-owned protocol
/// flagged in the header. # Safety — as `open`.
pub unsafe fn open_words(ctx: *mut Ctx, own: *mut u8, usable: usize, rec: usize, nkey: u16) -> Result<bool, i64> {
    open_by(ctx, own, usable, rec, nkey, true, 0, &|a, b| cmp_words(nkey as usize, a, b))
}
/// `open_words` over run stream `stream` (`slot + 1`; 0 the pipeline's own) laid in memory the caller owns (`home`, `usable` bytes): a pipeline that merges one slot's runs while producing another's (lower.md §15.20 — the caller
/// declares `SPILL_RUN_BASE home` before this and before every `drain_words`, so the fills land in `home`).
/// # Safety — as `open`, over `home`.
pub unsafe fn open_words_in(ctx: *mut Ctx, home: *mut u8, usable: usize, rec: usize, nkey: u16, stream: u64) -> Result<bool, i64> {
    open_by(ctx, home, usable, rec, nkey, true, stream, &|a, b| cmp_words(nkey as usize, a, b))
}
unsafe fn cmp_words(n: usize, a: *const u8, b: *const u8) -> core::cmp::Ordering {
    for i in 0..n {
        let (x, y) = (*(a as *const u64).add(i), *(b as *const u64).add(i));
        if x != y {
            return x.cmp(&y);
        }
    }
    core::cmp::Ordering::Equal
}
pub unsafe fn open_by(ctx: *mut Ctx, own: *mut u8, usable: usize, rec: usize, nkey: u16, own_protocol: bool, stream: u64, cmp: Cmp<'_>) -> Result<bool, i64> {
    open_impl(ctx, own, usable, rec, nkey, own_protocol, stream, None, cmp)
}
/// `open_by` over variable-length records (M4 sort rows carrying their by-reference bytes): `rec` is the fixed head every record has, the record's total length (8-rounded) is the `u32` at `rec_off` inside it; a record longer than a run's buffer grows that run's buffer past the region (D1327 `grow`).
/// # Safety — as `open`.
pub unsafe fn open_var_by(ctx: *mut Ctx, own: *mut u8, usable: usize, rec: usize, rec_off: u16, cmp: Cmp<'_>) -> Result<bool, i64> {
    open_impl(ctx, own, usable, rec, 0, true, 0, Some(rec_off), cmp)
}
/// `open_var_by` over run stream `stream` (`slot + 1`; 0 the pipeline's own), D457's consumed sort.
/// # Safety — as `open`.
pub unsafe fn open_var_in(ctx: *mut Ctx, own: *mut u8, usable: usize, rec: usize, rec_off: u16, stream: u64, cmp: Cmp<'_>) -> Result<bool, i64> {
    open_impl(ctx, own, usable, rec, 0, true, stream, Some(rec_off), cmp)
}
/// The record at `row`'s length: the `rec_off` word for variable records, `rec` else.
#[inline(always)]
unsafe fn rec_len(m: &RunMerge, row: *const u8) -> usize {
    match var_off(m) {
        Some(o) => (row.add(o) as *const u32).read_unaligned() as usize,
        None => m.rec as usize,
    }
}
fn var_off(m: &RunMerge) -> Option<usize> {
    (m._pad[2] != 0).then(|| u16::from_le_bytes([m._pad[3], m._pad[4]]) as usize)
}
unsafe fn open_impl(ctx: *mut Ctx, own: *mut u8, usable: usize, rec: usize, nkey: u16, own_protocol: bool, stream: u64, var: Option<u16>, cmp: Cmp<'_>) -> Result<bool, i64> {
    let nruns = spill(ctx, SPILL_RUNS, stream as i64);
    if nruns < 0 {
        return Err(ERROR);
    }
    let nruns = nruns as usize;
    if nruns == 0 {
        return Ok(false);
    }
    debug_assert!(nruns < 1 << 24, "heap_len's top byte (a region's byte 15) stays 0");
    let min = min_buf(rec, var.is_some());
    let ranges = if nruns <= MAXORDER && usable >= fixed_bytes(nruns) + nruns * min {
        [(0, nruns), (0, 0)]
    } else {
        merge_passes(ctx, own, usable, rec, stream, var, cmp, nruns)?
    };
    let token = |r: usize| if r < ranges[0].1 { ranges[0].0 + r as u32 } else { ranges[1].0 + (r - ranges[0].1) as u32 };
    let open = |r: usize| spill(ctx, SPILL_RUN_OPEN, (token(r) as u64 | stream << 32) as i64);
    lay(ctx, own, usable, rec, nkey, own_protocol, var, cmp, ranges[0].1 + ranges[1].1, &open)
}
/// D3190: a co-split claim's merge, `open_by` / `open_var_in` over `nruns` runs whose readers `open(r)` opens (each outer
/// run's slice, `SPILL_RUN_SLICE`) in one merge: no pass (the caller sized `usable` for every run's least buffer, else
/// `UNSUPPORTED_SCRATCH`); `Ok(false)` without runs.
/// # Safety — as `open`, over `own`.
pub unsafe fn open_slices(ctx: *mut Ctx, own: *mut u8, usable: usize, rec: usize, nkey: u16, var: Option<u16>, nruns: usize, open: &dyn Fn(usize) -> i64, cmp: Cmp<'_>) -> Result<bool, i64> {
    if nruns == 0 {
        return Ok(false);
    }
    if nruns > MAXORDER || usable < fixed_bytes(nruns) + nruns * min_buf(rec, var.is_some()) {
        return Err(unsupported(UNSUPPORTED_SCRATCH));
    }
    lay(ctx, own, usable, rec, nkey, true, var, cmp, nruns, open)
}
/// D1327: the runs past the region's fan-in merged in passes into longer runs (PG tuplesort `mergeruns`), each pass's
/// groups of `fan` runs into one merged run of the stream (`SPILL_MERGE_BEGIN`), until the last pass's runs fit one
/// merge; the put runs are only read. D2572: the last pass merges only the runs the final merge cannot read beside the
/// rest (groups of at most `fan` over the first runs, until the merged runs and the untouched ones number `last`), not
/// every run: a run rewritten is its bytes on disk twice until the statement ends (PG reuses a read tape's blocks), so
/// 4x PG's sort spilled 5.4x PG's temp bytes. Returns the final runs as two token ranges: the last pass's merged runs
/// (contiguous: a pass writes its runs in order into its own file) and the untouched runs after them (`lay`'s ranges).
#[cold]
unsafe fn merge_passes(ctx: *mut Ctx, own: *mut u8, usable: usize, rec: usize, stream: u64, var: Option<u16>, cmp: Cmp<'_>, nruns: usize) -> Result<[(u32, usize); 2], i64> {
    let min = min_buf(rec, var.is_some());
    let (fan, last) = (fan_in(usable, min, min, nruns), fan_in(usable, min, 0, nruns));
    let (mut base, mut n, mut pass) = (0u32, nruns, 0u64);
    while n > last {
        let mut cut = pass_cut(n, fan, last);
        let (mut first, mut outs, mut i) = (0u32, 0usize, 0usize);
        while i < n && cut > 0 {
            let k = group_len(n - i, fan, cut);
            let fresh = if outs == 0 { MERGE_FRESH } else { 0 };
            let idx = merge_pass(ctx, own, usable, rec, stream | pass << 32 | fresh, var, cmp, base + i as u32, k)?;
            if outs == 0 {
                first = idx;
            }
            outs += 1;
            i += k;
            cut = cut.saturating_sub(k - 1);
            if crate::util::interrupted(ctx) {
                return Err(CANCELLED);
            }
        }
        if i < n {
            return Ok([(first, outs), (base + i as u32, n - i)]);
        }
        (base, n, pass) = (first, outs, pass + 1);
    }
    Ok([(base, n), (0, 0)])
}
/// D3105 (b): every run of stream `stream` merged into one run (PG tuplesort's final merge onto one tape for a sort read
/// with random access: a merge join's inner, which restores its mark), in passes of at most the region's fan-in over
/// `own`'s `usable` bytes (`merge_pass`, D1327), each pass's groups as even as the fan-in leaves them, until one run is
/// left: its token (`SPILL_RUN_OPEN`'s), `None` without runs. A lone put run is read as put (token 0: no pass).
/// # Safety — as `open`.
#[cold]
pub(crate) unsafe fn merge_to_one(ctx: *mut Ctx, own: *mut u8, usable: usize, rec: usize, stream: u64, var: Option<u16>, cmp: Cmp<'_>) -> Result<Option<u32>, i64> {
    let nruns = spill(ctx, SPILL_RUNS, stream as i64);
    if nruns < 0 {
        return Err(ERROR);
    }
    let mut n = nruns as usize;
    if n <= 1 {
        return Ok((n == 1).then_some(0));
    }
    let min = min_buf(rec, var.is_some());
    let fan = fan_in(usable, min, min, n);
    let (mut base, mut pass) = (0u32, 0u64);
    while n > 1 {
        let groups = n.div_ceil(fan);
        let (mut first, mut i) = (0u32, 0usize);
        for g in 0..groups {
            let k = (n - i).div_ceil(groups - g);
            let fresh = if g == 0 { MERGE_FRESH } else { 0 };
            let idx = merge_pass(ctx, own, usable, rec, stream | pass << 32 | fresh, var, cmp, base + i as u32, k)?;
            if g == 0 {
                first = idx;
            }
            i += k;
            if crate::util::interrupted(ctx) {
                return Err(CANCELLED);
            }
        }
        (base, n, pass) = (first, groups, pass + 1);
    }
    Ok(Some(base))
}
/// Lay the merge of `nruns` runs, run `r`'s reader `open(r)` (D2572: `merge_passes`' final runs, the first token range's
/// then the second's; D3190: a co-split claim's slices), over `own`'s `usable` bytes and fill the buffers.
unsafe fn lay(ctx: *mut Ctx, own: *mut u8, usable: usize, rec: usize, nkey: u16, own_protocol: bool, var: Option<u16>, cmp: Cmp<'_>, nruns: usize, open: &dyn Fn(usize) -> i64) -> Result<bool, i64> {
    if var.is_some() {
        // D1327: the region's big buffers of the merge laid before (a pass's, a rescan's) free for this one
        let w = [own as u64, 0u64];
        if spill(ctx, SPILL_MERGE_BIG, w.as_ptr() as i64) < 0 {
            return Err(ERROR);
        }
    }
    let hdr = core::mem::size_of::<RunMerge>();
    let fixed = fixed_bytes(nruns);
    let unit = if var.is_some() { 8 } else { rec };
    let buf = (usable.saturating_sub(fixed) / nruns) / unit * unit;
    core::ptr::write_bytes(own, 0, fixed);
    let m = &mut *(own as *mut RunMerge);
    let vo = var.unwrap_or(0).to_le_bytes();
    *m = RunMerge { nruns: nruns as u32, rec: rec as u32, buf: buf as u32, heap_len: 0, emitted: 0, _pad: [nkey as u8, (nkey != 0 || own_protocol) as u8, var.is_some() as u8, vo[0], vo[1], (nkey >> 8) as u8, 0], kind: RUNMERGE_KIND, region: 0, active: 0, tag: 0, _pad2: [0; 2], big_stash: 0, big_cap: 0, big_last: 0, big_seen: 0, _pad3: [0; 2], reader: 0 };
    let curs = own.add(hdr) as *mut RunCur;
    let tree = own.add(hdr + nruns * core::mem::size_of::<RunCur>()) as *mut u32;
    for r in 0..nruns {
        let h = open(r);
        if h < 0 {
            return Err(ERROR);
        }
        *curs.add(r) = RunCur { handle: h as u32, len: 0, pos: 0, cap: buf as u32, off: (fixed + r * buf) as i64, big: 0 };
        let alive = fill(ctx, own, m, &mut *curs.add(r))?;
        m.heap_len += alive as u32;
        *tree.add(r) = NONE;
    }
    for r in 0..nruns {
        tree_insert(own, curs, tree, nruns, r as u32, cmp);
    }
    Ok(true)
}
/// D1327: one pass's merge of runs `base .. base + k` into a new merged run of the stream in `at` (the stream, its pass in
/// the high 32 bits), laid over `own`'s `usable` bytes (the runs' buffers and an output buffer of the same width); returns
/// the merged run's token.
#[cold]
unsafe fn merge_pass(ctx: *mut Ctx, own: *mut u8, usable: usize, rec: usize, at_pass: u64, var: Option<u16>, cmp: Cmp<'_>, base: u32, k: usize) -> Result<u32, i64> {
    let stream = at_pass & 0xffff_ffff; // the pass and `MERGE_FRESH` ride the high bits
    let idx = spill(ctx, SPILL_MERGE_BEGIN, at_pass as i64);
    if idx < 0 {
        return Err(ERROR);
    }
    let idx = idx as u32;
    let unit = if var.is_some() { 8 } else { rec };
    let fixed = fixed_bytes(k);
    let buf = (usable.saturating_sub(fixed) / (k + 1)) / unit * unit;
    // the runs' buffers take `k * buf` after the fixed bytes; the output buffer the next `buf`
    let open = |r: usize| spill(ctx, SPILL_RUN_OPEN, ((base + r as u32) as u64 | stream << 32) as i64);
    if !lay(ctx, own, fixed + k * buf, rec, 0, true, var, cmp, k, &open)? {
        return Ok(idx);
    }
    let m = &mut *(own as *mut RunMerge);
    let hdr = core::mem::size_of::<RunMerge>();
    let curs = own.add(hdr) as *mut RunCur;
    let tree = own.add(hdr + k * core::mem::size_of::<RunCur>()) as *mut u32;
    let out = own.add(fixed + k * buf);
    let mut olen = 0usize;
    let write = |p: *const u8, len: usize| -> Result<(), i64> {
        let w = [stream, idx as u64, p as u64, len as u64];
        if len > 0 && spill(ctx, SPILL_MERGE_WRITE, w.as_ptr() as i64) < 0 {
            return Err(ERROR);
        }
        Ok(())
    };
    while m.heap_len > 0 {
        let r = *tree as usize;
        let c = &mut *curs.add(r);
        let row = at(own, c, c.pos);
        let n = rec_len(m, row);
        // D2217: a variable record of length 0 never advances its run (a corrupt run read past its end): the merge ends
        // as `fill` ends a truncated run, never a spin
        if n == 0 && var_off(m).is_some() {
            return Err(unsupported(UNSUPPORTED_SCRATCH));
        }
        if olen + n > buf {
            write(out, olen)?;
            olen = 0;
            // D2217: a cancel, a terminate or the postmaster's quit (the abort byte) ends the pass at each output buffer,
            // not only between groups: a pass over a large spill runs for tens of seconds
            if crate::util::interrupted(ctx) {
                return Err(CANCELLED);
            }
        }
        if n > buf {
            write(row, n)?;
        } else {
            core::ptr::copy_nonoverlapping(row, out.add(olen), n);
            olen += n;
        }
        c.pos += n as u32;
        let alive = head_ready(m, own, c) || fill(ctx, own, m, c)?;
        if !alive {
            m.heap_len -= 1;
        }
        tree_replay(own, curs, tree, k, r as u32, cmp);
    }
    write(out, olen)?;
    Ok(idx)
}
/// Emit the winner until every run is spent; a stop suspends with `(EMIT, MERGING | emitted)`.
/// # Safety
/// `own` holds a merge `open` laid out (or one a suspend left in place).
pub unsafe fn drain(ctx: *mut Ctx, own: *mut u8, key_of: KeyOf, emit_row: EmitRow) -> i64 {
    let r = drain_rows(ctx, own, &|a, b| key_of(a).cmp(&key_of(b)), emit_row);
    if r != DONE {
        return r;
    }
    if flush(ctx) < 0 {
        return ERROR;
    }
    DONE
}
/// `drain_rows` over an `open_words` merge: no flush, the resume word untouched; `DONE` when every
/// run is spent, `HOST_STOP` when `emit_row` said stop (the row consumed). # Safety — as `drain`.
pub unsafe fn drain_words(ctx: *mut Ctx, own: *mut u8, emit_row: EmitRow) -> i64 {
    let nkey = key_words(&*(own as *const RunMerge));
    drain_rows(ctx, own, &|a, b| cmp_words(nkey, a, b), emit_row)
}
pub unsafe fn drain_by(ctx: *mut Ctx, own: *mut u8, cmp: Cmp<'_>, emit_row: EmitRow) -> i64 {
    drain_rows(ctx, own, cmp, emit_row)
}
unsafe fn drain_rows(ctx: *mut Ctx, own: *mut u8, cmp: Cmp<'_>, emit_row: EmitRow) -> i64 {
    let m = &mut *(own as *mut RunMerge);
    if m.kind != RUNMERGE_KIND {
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    let nruns = m.nruns as usize;
    let hdr = core::mem::size_of::<RunMerge>();
    let curs = own.add(hdr) as *mut RunCur;
    let tree = own.add(hdr + nruns * core::mem::size_of::<RunCur>()) as *mut u32;
    while m.heap_len > 0 {
        // D2217: the abort byte every 1024 rows (a cancel, a terminate or the postmaster's quit ends the drain); a consumer
        // that never checks (a count) otherwise reads a large spill to its end first
        if m.emitted & 1023 == 1023 && crate::util::interrupted(ctx) {
            return CANCELLED;
        }
        let r = *tree as usize;
        let c = &mut *curs.add(r);
        let n = rec_len(m, at(own, c, c.pos));
        if n == 0 && var_off(m).is_some() {
            return unsupported(UNSUPPORTED_SCRATCH); // D2217: a corrupt run that never advances (merge_pass's guard)
        }
        let f = emit_row(ctx, at(own, c, c.pos));
        if f < 0 {
            return ERROR;
        }
        m.emitted += 1;
        c.pos += n as u32;
        // the run's next head, refilled when the buffer is spent; an exhausted run plays as +inf
        let alive = if head_ready(m, own, c) {
            true
        } else {
            match fill(ctx, own, m, c) {
                Err(e) => return e,
                Ok(alive) => alive,
            }
        };
        if !alive {
            m.heap_len -= 1;
        }
        tree_replay(own, curs, tree, nruns, r as u32, cmp);
        if f == HOST_STOP {
            if m._pad[1] != 0 {
                return HOST_STOP;
            }
            (*ctx).resume = resume::word(resume::EMIT, MERGING | (m.emitted & (MERGING - 1)));
            return suspend(crate::util::out_count(ctx) == 0);
        }
    }
    if m._pad[1] == 0 {
        (*ctx).resume = gravity_abi::ctx::RESUME_NONE;
    }
    DONE
}
/// The head record is whole in the buffer (a variable record's length word first, then its bytes).
unsafe fn head_ready(m: &RunMerge, own: *mut u8, c: &RunCur) -> bool {
    let avail = (c.len - c.pos) as usize;
    avail >= m.rec as usize && avail >= rec_len(m, at(own, c, c.pos))
}
/// Refill a run's buffer (the unread tail first); false when the run is over. D1327: a record longer than the buffer
/// grows the run's buffer past the region (`grow`), never `UNSUPPORTED_SCRATCH`.
unsafe fn fill(ctx: *mut Ctx, own: *mut u8, m: &mut RunMerge, c: &mut RunCur) -> Result<bool, i64> {
    let rec = m.rec as usize;
    let tail = (c.len - c.pos) as usize;
    if tail > 0 {
        core::ptr::copy(at(own, c, c.pos), at(own, c, 0), tail);
    }
    c.pos = 0;
    c.len = tail as u32;
    let mut need = if tail >= rec { rec.max(rec_len(m, at(own, c, 0))) } else { rec };
    while (c.len as usize) < need {
        if need > c.cap as usize {
            grow(ctx, own, m, c, need)?;
        }
        // handle | offset (32 bits) | cap (16 bits): a fill reads at most 64 kB, the loop tops the buffer up
        let cap = (c.cap - c.len).min(0xffff) as i64;
        let n = if c.big == 0 {
            spill(ctx, SPILL_RUN_FILL, c.handle as i64 | ((c.off + c.len as i64) << 16) | (cap << 48))
        } else {
            fill_big(ctx, own, c, cap)
        };
        if n < 0 {
            return Err(ERROR);
        }
        if n == 0 {
            if c.len != 0 {
                return Err(unsupported(UNSUPPORTED_SCRATCH));
            }
            return Ok(false);
        }
        c.len += n as u32;
        if c.len as usize >= rec {
            need = rec.max(rec_len(m, at(own, c, 0)));
        }
    }
    Ok(true)
}
/// D1327: a run's buffer grown to hold a record of `need` bytes (PG lets one tuple pass work_mem): a buffer drawn past
/// the region (`SPILL_MERGE_BIG`, held until the statement's end), the unread bytes moved into it.
#[cold]
unsafe fn grow(ctx: *mut Ctx, own: *mut u8, m: &mut RunMerge, c: &mut RunCur, need: usize) -> Result<(), i64> {
    let cap = need.next_power_of_two().max(2 * c.cap as usize);
    if cap > u32::MAX as usize {
        return Err(unsupported(UNSUPPORTED_SCRATCH));
    }
    let w = [own as u64, cap as u64];
    let p = spill(ctx, SPILL_MERGE_BIG, w.as_ptr() as i64) as *mut u8;
    if p.is_null() {
        return Err(ERROR);
    }
    core::ptr::copy_nonoverlapping(at(own, c, 0), p, c.len as usize);
    (c.big, c.off, c.cap) = (p as u64, p as i64 - own as i64, cap as u32);
    m.big_seen = 1;
    Ok(())
}
/// D1327: a fill into a big buffer: the fills based at it for the one read, then at the region again.
#[cold]
unsafe fn fill_big(ctx: *mut Ctx, own: *mut u8, c: &RunCur, cap: i64) -> i64 {
    if spill(ctx, SPILL_RUN_BASE, c.big as i64) < 0 {
        return ERROR;
    }
    let n = spill(ctx, SPILL_RUN_FILL, c.handle as i64 | ((c.len as i64) << 16) | (cap << 48));
    if spill(ctx, SPILL_RUN_BASE, own as i64) < 0 {
        return ERROR;
    }
    n
}
const NONE: u32 = u32::MAX;
/// Run `a`'s play beats run `b`'s: by the heads' keys, an exhausted run past every key, the run
/// index last (a stable merge).
#[inline(always)]
unsafe fn less(own: *mut u8, curs: *mut RunCur, a: u32, b: u32, cmp: Cmp<'_>) -> bool {
    let (ca, cb) = (&*curs.add(a as usize), &*curs.add(b as usize));
    match (ca.len == 0, cb.len == 0) {
        (true, true) => a < b,
        (true, false) => false,
        (false, true) => true,
        (false, false) => match cmp(at(own, ca, ca.pos), at(own, cb, cb.pos)) {
            core::cmp::Ordering::Less => true,
            core::cmp::Ordering::Greater => false,
            core::cmp::Ordering::Equal => a < b,
        },
    }
}
/// The tree of losers over `nruns` leaves (`tree[0]` the winner, `tree[1..]` the internal nodes, leaf `r` under node `(r + nruns) / 2`): run `r`'s play walks up from its leaf, the loser of every match staying at the node and the winner going on; the first arrival at an empty node waits.
unsafe fn tree_insert(own: *mut u8, curs: *mut RunCur, tree: *mut u32, nruns: usize, r: u32, cmp: Cmp<'_>) {
    let mut cur = r;
    let mut node = (r as usize + nruns) / 2;
    while node > 0 {
        let held = *tree.add(node);
        if held == NONE {
            *tree.add(node) = cur;
            return;
        }
        if less(own, curs, held, cur, cmp) {
            *tree.add(node) = cur;
            cur = held;
        }
        node /= 2;
    }
    *tree = cur;
}
/// The winner's head moved (or its run ended): replay its leaf's path, four matches at 16 runs.
unsafe fn tree_replay(own: *mut u8, curs: *mut RunCur, tree: *mut u32, nruns: usize, r: u32, cmp: Cmp<'_>) {
    let mut cur = r;
    let mut node = (r as usize + nruns) / 2;
    while node > 0 {
        let held = *tree.add(node);
        if less(own, curs, held, cur, cmp) {
            *tree.add(node) = cur;
            cur = held;
        }
        node /= 2;
    }
    *tree = cur;
}
