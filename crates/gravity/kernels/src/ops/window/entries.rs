//! M10's host entries (window.agg): the C-ABI functions the lowered window IR calls on the
//! WindowAgg's window object (hosts v4/m10.rs), which ports nodeWindowAgg.c over the rows the Materialize end stored.
//! The Materialize end appends each input row (`m10_win_put`: the child's columns, then each window function's
//! argument and FILTER cells, evaluated in IR; a cell whose evaluation raised is marked first by `m10_win_argerr`, and
//! its error is raised only when the object reads that cell; under a VOLATILE argument each is `CELL_LAZY`). The reader
//! walks the stored rows (`m10_win_rows`, `m10_win_rec`); the Window step asks for row `j`'s window function values
//! (`m10_win_row`, into the object's result cells) and reports a failed run condition (`m10_win_runfail`). Each open of
//! the fill or the reader (a rerun set's re-open too) first restarts the object (`m10_win_open`, PG's ExecReScanWindowAgg).
//!
//! The handle (host_abi/v4/m8.rs's pattern: this crate cannot name the hosts crate): every entry's second argument `h`
//! is the address of the execution's window object, laid by its protocol (exec v4/m10.rs) into the object's parameter
//! word before any pipeline runs. The object starts with a `Head` (`magic` while it lives, `op` its one runtime
//! function). A status below zero is PostgreSQL's error, stashed (`ERROR`). Every entry runs under the one landing pad
//! (raw.rs: `contract::host!`'s `#[guard(ERROR)]`, or a `contract::Host` impl).
//! Created by M10-WINDOW.
// The handle's head and the entries the IR calls by name live in the family's raw half (rules R6 / R8, contract::host);
// their paths here are unchanged, so the lowerers' addresses and the IR's host names stay as they were.
pub use super::raw::{m10_win_argerr, m10_win_offsets, m10_win_part, m10_win_put, m10_win_put_part, m10_win_rec, m10_win_row, m10_win_rows, m10_win_runfail, Head};

/// `window.agg`: a WindowAgg's window object.
pub const WIN_MAGIC: u64 = 0x8a11_7e11_0000_0a01;

/// The window object's cell as the IR reads it: the head, then the address of the result cells (`res`: one datum word
/// per window function, then one validity byte each, 1 = not NULL) and the stored row's layout (`ncols` cells: the
/// datum words, then the NULL bytes, 1 = NULL, padded to 8).
#[repr(C)]
pub struct WinCell {
    pub head: Head,
    pub res: u64,
    pub nfuncs: u64,
    pub ncols: u64,
}

/// D1232: a stored cell's NULL byte for an argument or FILTER the IR left to the object. The object evaluates it at each
/// read (PG's per-access ExecEvalExpr), used when the window has a VOLATILE argument.
pub const CELL_LAZY: u8 = 3;

/// The IR's loads of a `WinCell`.
pub const WIN_RES: usize = core::mem::offset_of!(WinCell, res);

/// The bytes of a stored row of `ncols` cells: the datum words, then the NULL bytes padded to 8.
pub const fn record_bytes(ncols: usize) -> usize {
    8 * ncols + ncols.next_multiple_of(8)
}

/// The offset of function `i`'s validity byte in the result cells of `nfuncs` functions.
pub const fn res_valid(nfuncs: usize, i: usize) -> usize {
    8 * nfuncs + i
}

/// `m10_win_row`'s answers (a status below zero is an error).
pub const ROW_EMIT: i64 = 0;
/// The row is not returned (PG's WINDOWAGG_PASSTHROUGH_STRICT: a top window past its run condition, in a partition).
pub const ROW_SKIP: i64 = 1;
/// No row of the WindowAgg is returned any more (PG's WINDOWAGG_DONE).
pub const ROW_DONE: i64 = 2;
/// The row is returned with every window function NULL and neither the run condition nor the qual checked (PG's
/// WINDOWAGG_PASSTHROUGH: a lower window past its run condition, in a partition).
pub const ROW_PASS: i64 = 3;

/// The operation codes `Head::op` receives, one per entry.
pub const OP_PUT: u64 = 1;
pub const OP_ARGERR: u64 = 2;
pub const OP_ROWS: u64 = 3;
pub const OP_REC: u64 = 4;
pub const OP_ROW: u64 = 5;
pub const OP_RUNFAIL: u64 = 6;
pub const OP_OFFSETS: u64 = 7;
pub const OP_OPEN: u64 = 8;
pub const OP_READS: u64 = 9;
pub const OP_PARAMS: u64 = 10;
pub const OP_ASK: u64 = 11;
/// D3336 (win.partruns): 1 when the record at `a` (the `m10_win_put` layout) is in the partition of the object's last
/// stored row, or the object stores none; 0 when it starts another partition.
pub const OP_SAMEPART: u64 = 12;

/// `m10_win_open`'s `fill` word (`OP_OPEN`'s argument): a reader's open whose walk is the only one over the rows its fill
/// stored, so each passed partition's rows are freed as the walk leaves it (D3345, PG's release_partition).
pub const OPEN_READER: u64 = 0;
/// The fill's open: its input runs again, the stored rows are dropped (D2346).
pub const OPEN_FILL: u64 = 1;
/// D3951: a reader's open whose walk may run again over the same stored rows (a re-opened nest or SubPlan inner whose fill
/// is kept across the re-opens): the rows live until the fill's next open, no walk frees a passed partition.
pub const OPEN_REWALK: u64 = 2;

/// An `m10_win_params` record's layout-word bit (W3, window_core-0190 / -0191): the parameter is an InitPlan's, its datum
/// word the Init cell (the Init proto's handle word): the object leaves it pending and runs the Init at its first read.
pub const PARAM_INIT: u64 = 1 << 17;

/// `m10_win_reads`' answer bits (D2246): the row's window functions are evaluated (run mode); the asked function reads its
/// first-call argument at the row (a partition's first row; an ntile until its buckets are not NULL).
pub const READS_RUN: i64 = 1;
pub const READS_FIRST: i64 = 2;

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_abi::status::ERROR;
    use gravity_abi::Ctx;
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    static SEEN: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

    unsafe fn record(_h: *mut Head, _ctx: *mut Ctx, code: u64, a: u64, b: u64) -> i64 {
        SEEN.iter().zip([code, a, b]).for_each(|(s, v)| s.store(v, Relaxed));
        (code * 100 + a + b) as i64
    }

    #[test]
    fn each_entry_runs_the_objects_op_with_its_code_and_arguments() {
        let ctx: *mut Ctx = core::ptr::null_mut();
        let h = Box::into_raw(Box::new(Head { magic: WIN_MAGIC, op: record })) as u64; // alloc: Cold
        let v = [0u64; 4];
        let vp = v.as_ptr() as u64;
        let check = |got: i64, (code, a, b): (u64, u64, u64)| {
            assert_eq!(got, (code * 100 + a + b) as i64);
            assert_eq!([SEEN[0].load(Relaxed), SEEN[1].load(Relaxed), SEEN[2].load(Relaxed)], [code, a, b]);
        };
        // SAFETY: the handle is a live `Head` whose op reads no pointer; each call is checked before the next one
        unsafe {
            check(m10_win_put(ctx, h, v.as_ptr()), (OP_PUT, vp, 0));
            check(m10_win_argerr(ctx, h, 3, 8), (OP_ARGERR, 3, 8));
            check(m10_win_rows(ctx, h), (OP_ROWS, 0, 0));
            check(m10_win_rec(ctx, h, 5), (OP_REC, 5, 0));
            check(m10_win_row(ctx, h, 6), (OP_ROW, 6, 0));
            check(m10_win_runfail(ctx, h), (OP_RUNFAIL, 0, 0));
            check(m10_win_offsets(ctx, h, v.as_ptr()), (OP_OFFSETS, vp, 0));
            check(crate::contract::entry2::<super::super::raw::WinOpen>(ctx as u64, h, 1) as i64, (OP_OPEN, 1, 0));
            check(crate::contract::entry3::<super::super::raw::WinReads>(ctx as u64, h, 4, 3) as i64, (OP_READS, 4, 3));
            check(crate::contract::entry3::<super::super::raw::WinParams>(ctx as u64, h, 1, 2) as i64, (OP_PARAMS, 1, 2));
        }
        // SAFETY: made by Box::into_raw above.
        drop(unsafe { Box::from_raw(h as *mut Head) });
    }

    #[test]
    fn a_foreign_handle_is_an_error_by_the_entrys_name() {
        let other = Box::into_raw(Box::new(Head { magic: 1, op: record })) as u64; // alloc: Cold
        // SAFETY: a live Head of another kind: `call` reads only its magic.
        let st = unsafe { m10_win_row(core::ptr::null_mut(), other, 0) };
        assert_eq!(st, ERROR);
        let msg = gravity_fn::take_error().map(|e| e.message().to_owned()).unwrap_or_default();
        assert!(msg.contains("m10_win_row: a stale or foreign window handle"), "{msg}");
        // SAFETY: made by Box::into_raw above.
        drop(unsafe { Box::from_raw(other as *mut Head) });
    }

    #[test]
    fn the_cells_layout_is_the_irs() {
        assert_eq!(WIN_RES, 16);
        assert_eq!(record_bytes(3), 32);
        assert_eq!(res_valid(2, 1), 17);
    }
}
