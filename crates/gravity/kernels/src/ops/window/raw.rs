//! The window object's raw half (rules R6 / R8): the handle's head and the check every entry makes, the entries the IR
//! calls by name written through `contract::host!` (entries.rs re-exports them under their old path), and the entries
//! written as `contract::Host` impls the IR calls as `contract::entryN::<H>` (one landing pad, no `extern "C"` of their own).
use gravity_abi::status::ERROR;
use gravity_abi::Ctx;

use super::entries::{OP_ARGERR, OP_ASK, OP_OFFSETS, OP_OPEN, OP_PARAMS, OP_PUT, OP_READS, OP_REC, OP_ROW, OP_ROWS, OP_RUNFAIL, OP_SAMEPART, WIN_MAGIC};
use crate::contract::Host;

/// The first field of the window object a handle word points at.
#[repr(C)]
pub struct Head {
    /// `WIN_MAGIC` while the object lives; anything else is a stale or foreign handle.
    pub magic: u64,
    /// The object's runtime: `(h, ctx, code, a, b)` with the entry's `OP_*` code and its two argument words.
    pub op: unsafe fn(h: *mut Head, ctx: *mut Ctx, code: u64, a: u64, b: u64) -> i64,
}

/// Object `h` runs operation `code`; a null or foreign handle is an error by `entry`'s name.
/// # Safety
/// `h` is 0 or the address of a live `Head`, as the program's handle word holds it.
#[inline(always)]
pub(crate) unsafe fn call(ctx: *mut Ctx, h: u64, code: u64, a: u64, b: u64, entry: &str) -> i64 {
    let head = h as *mut Head;
    if head.is_null() || (*head).magic != WIN_MAGIC {
        return stale(entry, h);
    }
    ((*head).op)(head, ctx, code, a, b)
}

#[cold]
#[inline(never)]
fn stale(entry: &str, h: u64) -> i64 {
    crate::host_abi::bare(&format!("{entry}: a stale or foreign window handle {h:#x}")) // alloc: Cold
}

// The entries the lowered IR calls by name (entries.rs re-exports them): each runs the object's op with its code under the
// one landing pad (`#[guard(ERROR)]`, as kernels' guard! did), a status below zero PostgreSQL's error.
crate::contract::host! {
    /// PG `spool_tuples`' store: the row at `rec` (the object's `record_bytes(ncols)` layout) appended to window object `h`,
    /// its by-reference datums copied (they live until the execution's end); the cells `m10_win_argerr` marked since the
    /// last put keep their errors. 0, else a status below zero.
    #[guard(ERROR)]
    pub unsafe fn m10_win_put(ctx: *mut Ctx, h: u64, rec: *const u64) -> i64 {
        call(ctx, h, OP_PUT, rec as u64, 0, "m10_win_put")
    }

    /// Cell `col` of the row the next `m10_win_put` stores raised `err` (a raise mask; its HOST bit's error is the stashed
    /// one, taken here): the object keeps the error and raises it when it reads the cell. 0, else a status below zero.
    #[guard(ERROR)]
    pub unsafe fn m10_win_argerr(ctx: *mut Ctx, h: u64, col: u64, err: u64) -> i64 {
        call(ctx, h, OP_ARGERR, col, err, "m10_win_argerr")
    }

    /// The rows window object `h` stores (the reader's total), else a status below zero.
    #[guard(ERROR)]
    pub unsafe fn m10_win_rows(ctx: *mut Ctx, h: u64) -> i64 {
        call(ctx, h, OP_ROWS, 0, 0, "m10_win_rows")
    }

    /// Stored row `j`'s address (its layout `record_bytes(ncols)`, valid until the fill's next open or the execution's end),
    /// else a status below zero.
    #[guard(ERROR)]
    pub unsafe fn m10_win_rec(ctx: *mut Ctx, h: u64, j: u64) -> i64 {
        call(ctx, h, OP_REC, j, 0, "m10_win_rec")
    }

    /// PG `ExecWindowAgg` for stored row `j` (the rows are read in order): the window functions' values in the result cells
    /// (a by-reference value lives until the next call), then `ROW_EMIT`, `ROW_SKIP`, `ROW_DONE` or `ROW_PASS`, else a status
    /// below zero. A second call for the row just answered (a resumed row) answers it again, its cells unchanged.
    #[guard(ERROR)]
    pub unsafe fn m10_win_row(ctx: *mut Ctx, h: u64, j: u64) -> i64 {
        call(ctx, h, OP_ROW, j, 0, "m10_win_row")
    }

    /// The run condition failed at the last row `m10_win_row` answered: `ROW_EMIT` (a lower window: the row passes, later
    /// rows carry NULL values), `ROW_SKIP` (a top window with a PARTITION BY: the rest of the partition is skipped) or
    /// `ROW_DONE` (nothing more can match), else a status below zero.
    #[guard(ERROR)]
    pub unsafe fn m10_win_runfail(ctx: *mut Ctx, h: u64) -> i64 {
        call(ctx, h, OP_RUNFAIL, 0, 0, "m10_win_runfail")
    }

    /// PG `calculate_frame_offsets`: the frame's start / end offset values at `vals` (`[start, start NULL, end, end NULL]`,
    /// each word 0 / 1 for NULL), evaluated at the reader's open before the first row; checked (NULL, negative) and kept by the
    /// first call after `m10_win_open`, later calls ignored. 0, else a status below zero.
    #[guard(ERROR)]
    pub unsafe fn m10_win_offsets(ctx: *mut Ctx, h: u64, vals: *const u64) -> i64 {
        call(ctx, h, OP_OFFSETS, vals as u64, 0, "m10_win_offsets")
    }

    /// D3336 (win.partruns): the object a reader's call of a window read by claims works on, from its object table `tbl`
    /// (exec's `WINDOW_PARTED` lay: the count `n`, the `n` handles, the fill's turn): its claim's one object (`[i, i + 1)`),
    /// the only one of a one-object window under a whole claim (`ALL`), 0 under a claim of none (a first call, the
    /// section's end: no object read). Else a status below zero: a claim the table does not cover, by name.
    #[guard(ERROR)]
    pub unsafe fn m10_win_part(ctx: *mut Ctx, tbl: *const u64) -> i64 {
        part(ctx, tbl)
    }

    /// D3336 (win.partruns): `m10_win_put` into a window read by claims: the record goes to the object whose turn it is
    /// (table word `n + 1`), the turn passing to the next object (after the last, the first) when the record starts
    /// another partition than that object's last row (`OP_SAMEPART`), so every partition is stored whole in one object,
    /// in the fill's order. One serial fill moves the turn (the fill's pipeline is the leader's). 0, else a status below
    /// zero.
    #[guard(ERROR)]
    pub unsafe fn m10_win_put_part(ctx: *mut Ctx, tbl: *mut u64, rec: *const u64) -> i64 {
        put_part(ctx, tbl, rec)
    }
}

/// `m10_win_part`'s body.
/// # Safety
/// `ctx` is the calling pipeline's live context; `tbl` is null or the table exec laid, alive for the execution.
unsafe fn part(ctx: *const Ctx, tbl: *const u64) -> i64 {
    if ctx.is_null() || tbl.is_null() {
        return crate::table::fold_entries::bare("m10_win_part: a parted window with no object table (D3336)");
    }
    let n = *tbl;
    let at = |i: u64| *tbl.add(1 + i as usize) as i64;
    let (lo, hi) = ((*ctx).claim.lo, (*ctx).claim.hi);
    if lo >= hi {
        0
    } else if hi <= n {
        if hi - lo == 1 { at(lo) } else { bad_part("a reader's claim of several objects from", lo, n) }
    } else if n == 1 {
        at(0)
    } else {
        bad_part("a whole read of the objects from", lo, n)
    }
}

/// `m10_win_put_part`'s body.
/// # Safety
/// As `m10_win_part`'s, the table writable by the one fill; `rec` the IR's record (`m10_win_put`'s).
unsafe fn put_part(ctx: *mut Ctx, tbl: *mut u64, rec: *const u64) -> i64 {
    if tbl.is_null() || *tbl == 0 {
        return crate::table::fold_entries::bare("m10_win_put_part: a parted window with no object table (D3336)");
    }
    let n = *tbl;
    let turn = tbl.add(1 + n as usize);
    let mut i = *turn % n;
    if n > 1 {
        let same = call(ctx, *tbl.add(1 + i as usize), OP_SAMEPART, rec as u64, 0, "m10_win_put_part");
        if same < 0 {
            return same;
        }
        if same == 0 {
            i = (i + 1) % n;
            *turn = i;
        }
    }
    call(ctx, *tbl.add(1 + i as usize), OP_PUT, rec as u64, 0, "m10_win_put_part")
}

#[cold]
#[inline(never)]
fn bad_part(what: &str, i: u64, n: u64) -> i64 {
    crate::table::fold_entries::bare(&format!("window.agg: {what} {i} past a window read by claims over {n} objects (D3336)")) // alloc: Cold
}

/// `m10_win_open(h, fill)`: PG `ExecReScanWindowAgg` at a pipeline's open (D2346): object `h`'s walk starts again (run
/// mode, the first partition, the frame offsets evaluated anew by the next `m10_win_offsets`) and, when `fill` is
/// `OPEN_FILL` (the fill's open: its input runs again), its stored rows are dropped too; a reader's open says whether its
/// walk may run again over the same rows (`OPEN_REWALK`, D3951) or not (`OPEN_READER`). 0, else a status below zero.
pub struct WinOpen;
impl Host for WinOpen {
    type Args = (u64, u64);
    unsafe fn run(ctx: *mut Ctx, (h, fill): (u64, u64)) -> u64 {
        call(ctx, h, OP_OPEN, fill, 0, "m10_win_open") as u64
    }
}

/// `m10_win_reads(h, j, f)` (D2246): what PG's `ExecWindowAgg` reads at stored row `j`, the next row the walk answers,
/// asked before the step fills the row's `step_arg` cells (WinGetFuncArgCurrent's: lead / lag's offset, nth_value's n,
/// ntile's buckets): `READS_RUN` when its functions are evaluated, with `READS_FIRST` when function `f - 1` (`f` 0: none)
/// reads its first-call argument there (a partition's first row; for an ntile, every row until its buckets are not NULL);
/// 0 for the row just answered (a resumed row reads no cell again) and once the WindowAgg is done (a run condition's
/// pass-through rows read none). Else a status below zero.
pub struct WinReads;
impl Host for WinReads {
    type Args = (u64, u64, u64);
    unsafe fn run(ctx: *mut Ctx, (h, j, f): (u64, u64, u64)) -> u64 {
        call(ctx, h, OP_READS, j, f, "m10_win_reads") as u64
    }
}

/// `m10_win_params(h, vals, n)` (W3, window_core-0190 / -0191): the executor parameters (PARAM_EXEC) object `h`'s lazy
/// cells read (D1232), evaluated at the reader's open: `n` records of four words at `vals`, `[paramid, datum, NULL 0 / 1,
/// typlen | byval << 16]`. The object binds them beside the extern list (D1355) until the next call; a by-reference value
/// is copied. A record whose layout word holds `PARAM_INIT` is an InitPlan's, unevaluated: its datum word is the Init
/// cell, run at the parameter's first read. 0, else a status below zero.
pub struct WinParams;
impl Host for WinParams {
    type Args = (u64, u64, u64);
    unsafe fn run(ctx: *mut Ctx, (h, vals, n): (u64, u64, u64)) -> u64 {
        call(ctx, h, OP_PARAMS, vals, n, "m10_win_params") as u64
    }
}

/// `m10_win_ask(h, j)` (the exact request, PLAN-v5 §2.5; W4): object `h`'s stored rows reaching row `j`, its pulled fill
/// run one row per pull until row `j` is stored or the fill ends (PG's `spool_tuples` at the reader's next row): the stored
/// rows, 0 when `j` is past them all, else a status below zero.
pub struct WinAsk;
impl Host for WinAsk {
    type Args = (u64, u64);
    unsafe fn run(ctx: *mut Ctx, (h, j): (u64, u64)) -> u64 {
        call(ctx, h, OP_ASK, j, 0, "m10_win_ask") as u64
    }
}
