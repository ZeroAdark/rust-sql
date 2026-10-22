//! merge.join's raw half (rules R6 / R8): the pulled inner's handle (the exact request; jmerge-0012) and `m11_pull` /
//! `m11_hold`, the entries its IR calls as `contract::entry1::<MergePull>` / `<MergeHold>`. The object behind the handle is
//! exec's (ops/mergejoin/proto.rs: the shared pulled-set runtime over the runner's frame); this side checks the handle and
//! calls its `op` / `hold`.
use gravity_abi::breaker::{SortRun, SORTRUN_HDR, SORTRUN_VAR_WORDS};
use gravity_abi::program::{consumed, SPILL_RUN_ROWS};
use gravity_abi::status::ERROR;
use gravity_abi::Ctx;

use crate::contract::Host;

pub(crate) mod window;

/// The pulled inner's object as the IR's handle word holds it: a magic, then its runtime.
#[repr(C)]
pub struct PullHead {
    /// `PULL_MAGIC` while the object lives; anything else is a stale or foreign handle.
    pub magic: u64,
    /// One pull: 1 when the inner's spool stored one more row (its end stopped after it), 0 once the inner ended, else
    /// a status below zero with the error stashed.
    pub op: unsafe fn(h: *mut PullHead, ctx: *mut Ctx) -> i64,
    /// The budgeted pull's hold (MJ-AHEAD): the error the last pull stashed, raised past the row the merge wanted, kept for
    /// the next pull (0), else stashed again (a status below zero: the IR raises it now).
    pub hold: unsafe fn(h: *mut PullHead, ctx: *mut Ctx) -> i64,
}

/// `merge.markable` with `MERGE_PULLED`: a merge join's pulled inner.
pub const PULL_MAGIC: u64 = 0x8a11_7e11_0000_0b01;

/// `m11_pull(h)` (jmerge-0012): the merge's inner run on one row past its stored rows (PG's ExecProcNode of the inner at a
/// fetch): 1 stored, 0 ended (sticky), else a status below zero.
pub struct MergePull;
impl Host for MergePull {
    type Args = (u64,);
    unsafe fn run(ctx: *mut Ctx, (h,): (u64,)) -> u64 {
        let head = h as *mut PullHead;
        if head.is_null() || (*head).magic != PULL_MAGIC {
            return stale(h) as u64;
        }
        ((*head).op)(head, ctx) as u64
    }
}

/// `m11_hold(h)` (MJ-AHEAD): a budgeted pull raised after storing the row the merge wanted: its error kept for the next
/// pull (0), or stashed again (below zero) when it is not one PG raises at its row.
pub struct MergeHold;
impl Host for MergeHold {
    type Args = (u64,);
    unsafe fn run(ctx: *mut Ctx, (h,): (u64,)) -> u64 {
        let head = h as *mut PullHead;
        if head.is_null() || (*head).magic != PULL_MAGIC {
            return stale(h) as u64;
        }
        ((*head).hold)(head, ctx) as u64
    }
}

#[cold]
#[inline(never)]
fn stale(h: u64) -> i64 {
    crate::host_abi::bare(&format!("m11_pull: a stale or foreign merge inner handle {h:#x}")) // alloc: Cold
}

/// Whether consumed region `r`'s run may have been put (`SPILL_RUN_ROWS` says): a fill's opened run left unsealed, or a
/// region never opened (`cap` 0): an empty fill's, or (D3190) a wide Sort fill's, whose workers each put their runs when
/// one of them passed its share (runs.sort's resident road), the leader's region untouched.
unsafe fn may_be_put(r: *const SortRun) -> bool {
    !r.is_null() && ((*r).cap == 0 || (*r).pad[0] != crate::host_abi::SEALED)
}

/// D1236's held inner: consumed region `r` is a Sort's run whose fill raised, its error held for the merge's first inner
/// call (`RUN_HELD`, exec ops/sort/held.rs): it reads as the empty run, never its partial rows or put runs.
unsafe fn held(r: *const SortRun) -> bool {
    !r.is_null() && (*r).pad[2] & crate::host_abi::RUN_HELD != 0
}

crate::contract::host! {
    /// PG's error `code` names, stashed as the statement's error: `ERROR` (below zero), for the IR to leave with.
    pub unsafe fn m11_raise(_ctx: u64, code: u64) -> i64 {
        guard!(ERROR, super::entries::raise(code))
    }

    /// W8-EQUIVCLASS-SORTOP: a merge join's first inner call over a Sort whose ordering operator's family `family` has no order
    /// proc for `(typ, typ)`: PG's `FinishSortSupportFunction` elog (XX000), stashed; `ERROR` (below zero). No landing pad
    /// (crash policy, direction item 7): nothing here panics.
    pub unsafe fn m11_raise_no_proc(_ctx: u64, typ: u64, family: u64) -> i64 {
        super::entries::raise_no_proc(typ, family)
    }

    /// D1236's held inner (op 14 for runs.sort): at the merge's first inner call (INITIALIZE_INNER, ENDOUTER), consumed
    /// region `k` holds a Sort's run whose fill raised: its held error is raised here (`raise_held`, stashed again on this
    /// thread) and 1 answers, for the IR's raise exit; 0 for any other run (or nothing held), the merge reads on. PG's
    /// ExecSort reads its whole input at its first call, so the error is PG's there, and never raised where PG makes none.
    pub unsafe fn m11_run_held(ctx: *mut Ctx, k: u64) -> i64 {
        if !held(consumed(ctx, k as usize) as *const SortRun) {
            return 0;
        }
        (gravity_abi::program::raise_held(ctx) != 0) as i64
    }

    /// merge.join's inner (runs/slot_run.rs), consumed region `k` filling slot `slot`: `spool_view`'s sealed run; the empty
    /// run when no row came (runs.sort leaves an empty run unsealed). A keyless spool's end never puts a run (it grows
    /// resident, `m11_run_grow`: D1235), so a put run is a bug; a Sort's inner puts past its grant (`m11_run_open`).
    pub unsafe fn m11_run_view(ctx: *mut Ctx, k: u64, tbl: *const u32, slot: u64) -> *const u8 {
        let r = consumed(ctx, k as usize) as *const SortRun;
        if held(r) {
            return crate::host_abi::SPOOL_EMPTY.as_ptr() as *const u8;
        }
        if may_be_put(r) {
            let _ = slot; // D1324: the consumed region's stream, resolved by the driver (a baked slot goes stale)
            match guard!(ERROR, crate::util::spill(ctx, SPILL_RUN_ROWS, (gravity_abi::program::STREAM_CONSUMED | k) as i64)) {
                n if n < 0 => return n as *const u8,
                0 if (*r).len == 0 => return crate::host_abi::SPOOL_EMPTY.as_ptr() as *const u8,
                0 => {}
                _ => return crate::host_abi::bare("Bug: a merge join's inner run put past its grant (merge.join's end grows it resident)") as *const u8,
            }
        }
        crate::host_abi::spool_view(ctx, k, tbl)
    }

    /// D3105 (b): merge.join's inner over a Sort's run (runs/slot_run.rs's put road), consumed region `k`, at each window:
    /// when the Sort put runs past its grant (`m11_run_full`), the window over them merged into one run (laid at the
    /// first window, `window::lay`; the window as it stands at a later one); else `m11_run_view`'s sealed run. `tbl` the
    /// view's table, `full` the run's own (its record form and order); `point` 1: the window's rows pointed at their
    /// images (the merge reads a by-reference cell of the run).
    pub unsafe fn m11_run_open(ctx: *mut Ctx, k: u64, tbl: *const u32, full: *const u32, point: u64) -> *const u8 {
        guard!(ERROR as *const u8, {
            let own = consumed(ctx, k as usize);
            if held(own as *const SortRun) {
                return crate::host_abi::SPOOL_EMPTY.as_ptr() as *const u8;
            }
            if !own.is_null() && window::live(own) {
                return own as *const u8;
            }
            let r = own as *const SortRun;
            if may_be_put(r) {
                let stream = gravity_abi::program::STREAM_CONSUMED | k;
                match crate::util::spill(ctx, SPILL_RUN_ROWS, stream as i64) {
                    n if n < 0 => return n as *const u8,
                    0 => {}
                    n => return window::lay(ctx, own, full, stream, n as u64, point != 0),
                }
            }
            m11_run_view(ctx, k, tbl, 0)
        })
    }

    /// D3105 (b): row `j` of merge.join's window `w` (`m11_run_open`'s) when the IR's fast path missed it (`j` outside
    /// the window): the window moved to it (`window::row`). The row's address, else a status below zero.
    pub unsafe fn m11_window(ctx: *mut Ctx, w: *mut u8, j: u64, full: *const u32) -> i64 {
        guard!(ERROR, window::row(ctx, w, j, full))
    }

    /// D3190 (merge.cosplit): `m11_run_open` for a co-split claim, whose inner is rows `[lo, hi)` of the run (each clamped
    /// to its rows): an empty slice (the section's end, a first call) the empty run; a sealed run (nothing put) with only
    /// those rows' by-reference cells pointed, so the workers reading disjoint slices of one sealed run write disjoint rows
    /// (the reader offsets its rows by `lo`); a run put past its grant, which exec leaves to one claim read whole (`lo` 0,
    /// ops/sort/cosplit.rs), `m11_run_open`'s window.
    pub unsafe fn m11_run_slice(ctx: *mut Ctx, k: u64, tbl: *const u32, full: *const u32, point: u64, lo: u64, hi: u64) -> *const u8 {
        if lo >= hi {
            return crate::host_abi::SPOOL_EMPTY.as_ptr() as *const u8;
        }
        guard!(ERROR as *const u8, {
            let own = consumed(ctx, k as usize);
            if held(own as *const SortRun) {
                return crate::host_abi::SPOOL_EMPTY.as_ptr() as *const u8;
            }
            let whole = |w: *const u8| if lo == 0 { w } else { crate::host_abi::bare("Bug: a co-split claim past the first row of a put merge inner") as *const u8 };
            if !own.is_null() && window::live(own) {
                return whole(own as *const u8);
            }
            let r = own as *const SortRun;
            if may_be_put(r) {
                let stream = gravity_abi::program::STREAM_CONSUMED | k;
                match crate::util::spill(ctx, SPILL_RUN_ROWS, stream as i64) {
                    n if n < 0 => return n as *const u8,
                    0 if (*r).len == 0 => return crate::host_abi::SPOOL_EMPTY.as_ptr() as *const u8,
                    0 => {}
                    n => return whole(window::lay(ctx, own, full, stream, n as u64, point != 0)),
                }
            }
            crate::host_abi::spool_view_slice(ctx, k, tbl, lo, hi)
        })
    }

    /// D1235: a keyless spool's run (a Materialize inner, a pulled one) full grows resident, never put (the merge rewinds
    /// to its mark by row index): the region reopened through the driver's D254 `region_grow` (`grow`). The new base; 0
    /// when the allocation failed (the driver holds its error for the IR's raise); `ERROR` on a malformed run.
    pub unsafe fn m11_run_grow(ctx: *mut Ctx, run: *mut u8, tbl: *const u32) -> i64 {
        guard!(ERROR, grow(ctx, run, tbl, false))
    }

    /// D3105 (b): a Sort's inner run full (runs/slot_run.rs's put road) grows as `m11_run_grow` only within its owner's
    /// limit (W, or the leader's G: the driver's `region_grow_in`); past it 1: the IR puts the run (a sort's put, runs.sort's
    /// D1330 big runs too) and the merge reads the runs merged (`m11_run_open`), never pool bytes past the grant. A run
    /// that is not the region's own (a D1330 big run) is put at once.
    pub unsafe fn m11_run_full(ctx: *mut Ctx, run: *mut u8, tbl: *const u32) -> i64 {
        guard!(ERROR, {
            let r = &*(run as *const SortRun);
            if r.pad[1] == crate::host_abi::SORTRUN_BIG || r.region() != 0 { 1 } else { grow(ctx, run, tbl, true) }
        })
    }
}

/// The run's region reopened at least twice its length (`within`: only within the owner's limit, else 1), the rows kept,
/// the var copies moved up to the new rows' end and every row's var slot offsets moved by as much. The new base; 0 when the
/// allocation failed (the driver holds its error for the IR's raise); `ERROR` on a malformed run.
unsafe fn grow(ctx: *mut Ctx, run: *mut u8, tbl: *const u32, within: bool) -> i64 {
    let r = &*(run as *const SortRun);
    let (cap, rb, n) = (r.cap as usize, r.row_bytes as usize, r.len as usize);
    let (end, words) = (SORTRUN_HDR + cap * rb, SORTRUN_VAR_WORDS as usize);
    let (top, var_bytes) = (r.var_top() as usize, r.var_bytes());
    if r.region() != 0 || rb == 0 || top > end || top < SORTRUN_HDR + n * rb {
        return crate::host_abi::bare("Bug: m11_run_grow over a sized or malformed run");
    }
    let (need, head) = ((rb + words) as u64, (end + words) as u64);
    let nb = if within { gravity_abi::program::region_grow_in(ctx, need, head, 0) } else { gravity_abi::program::region_grow(ctx, need, head, 0) };
    if nb <= 1 {
        return nb as i64; // 0: the allocation failed; 1 (within): the limit refused it, nothing drawn
    }
    let base = nb as usize as *mut u8;
    // region_grow re-arms the u32 at offset 8 to the new length: the run's `row_bytes`, restored
    let len = (base.add(8) as *const u32).read() as usize;
    (base.add(8) as *mut u32).write(rb as u32);
    let ncap = (len - SORTRUN_HDR - words) / rb;
    let delta = (ncap.max(cap) - cap) * rb;
    core::ptr::copy(base.add(top), base.add(top + delta), end - top);
    (*(base as *mut SortRun)).cap = ncap.max(cap) as u32;
    let w = base.add(end + delta) as *mut u32;
    w.write_unaligned((top + delta) as u32);
    w.add(1).write_unaligned(var_bytes);
    if let (Some(v), true) = (crate::host_abi::var_shape(tbl), delta > 0) {
        for i in 0..n {
            let row = base.add(SORTRUN_HDR + i * rb);
            for c in 0..v.nvar {
                let slot = row.add(*v.slots.add(2 * c + 1) as usize);
                if (slot as *const u32).read_unaligned() != 0 {
                    let off = slot.add(4) as *mut u32;
                    off.write_unaligned(off.read_unaligned() + delta as u32);
                }
            }
        }
    }
    nb as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn once(_h: *mut PullHead, _ctx: *mut Ctx) -> i64 {
        1
    }

    /// A live head runs its op; a null or foreign handle is an error by the entry's name, never a call.
    #[test]
    fn a_pull_checks_its_handle() {
        let mut live = PullHead { magic: PULL_MAGIC, op: once, hold: once };
        // SAFETY: a live head on this stack frame; `run` reads its magic and calls its op.
        assert_eq!(unsafe { MergePull::run(core::ptr::null_mut(), (&mut live as *mut PullHead as u64,)) }, 1);
        // SAFETY: as above, through the hold.
        assert_eq!(unsafe { MergeHold::run(core::ptr::null_mut(), (&mut live as *mut PullHead as u64,)) }, 1);
        let mut foreign = PullHead { magic: 7, op: once, hold: once };
        for h in [0u64, &mut foreign as *mut PullHead as u64] {
            // SAFETY: 0, or a live head of another kind: only its magic is read.
            assert_eq!(unsafe { MergePull::run(core::ptr::null_mut(), (h,)) } as i64, ERROR);
            let msg = gravity_fn::take_error().map(|e| e.message().to_owned()).unwrap_or_default();
            assert!(msg.contains("m11_pull: a stale or foreign merge inner handle"), "{msg}");
        }
    }
}
