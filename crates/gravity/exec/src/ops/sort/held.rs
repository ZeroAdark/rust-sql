//! D1236's held inner (op 14 for runs.sort): a full Sort whose run a merge join reads as its inner, and whose fill may
//! raise (lowerv4 ops/mergejoin/held_inner.rs marks the pipeline `held_sort`). PG's ExecSort reads its whole input at its
//! first call, which ExecMergeJoin makes at INITIALIZE_INNER (a matchable outer tuple) or ENDOUTER (a fill-inner join over
//! an ended outer), and never when it makes none. The fill runs in program order as any Sort's; when it raises, the merge
//! here takes the error as deferred (`Merge::defer_error`): the leader's run is stamped `RUN_HELD` with no rows, the driver
//! holds the error (`Entry::defer_failed`), and the merge's first inner call raises it (kernels `m11_run_held`). The run
//! reads as the empty run (`m11_run_view` / `m11_run_open`), never its partial rows. D1108 (op 14 "held at len"): a plan
//! Material's fill held the same way (lowerv4 rerun/spool/form.rs `held`): its records stay in the spool object, the slot's
//! region (which no reader reads) only carries the stamp, and the nest's reader raises the error at the record the fill
//! stopped at (lowerv4 ops/material/lower.rs, `m11_run_held` over that region). D2316 (op 14 "held at len" for
//! runs.incremental): an Incremental Sort's fill held the same way keeps its rows (`stamp_at`, `RUN_HELD_AT`): the emit's
//! open seals them (kernels `begin_in`) and the reader reads the rows PostgreSQL's batches handed on before the raising
//! row, raising the error past them (lowerv4 ops/sort/inc_held.rs, kernels `sort_held_end`).
use gravity_abi::breaker::{SORTRUN_HDR, TABLE_SORT};
use gravity_driver::{Merge, Partial, Result, State};
use gravity_kernels::ops::sort::entries::{RUN_HELD, RUN_HELD_AT};

/// The width-1 (and non-resident) merge of a `held_sort` pipeline: the plain rows merge (`RowsUnordered`: the leader's
/// own run is the slot's) that defers its pipeline's error.
pub(crate) struct HeldRows;

pub(crate) static HELD_ROWS: HeldRows = HeldRows;

/// D2316: `HeldRows` for a `held_at` pipeline (runs.incremental's held fill): its run stamped with its rows kept.
pub(crate) struct HeldAt;

pub(crate) static HELD_AT: HeldAt = HeldAt;

impl Merge for HeldAt {
    fn merge_into(&self, _leader: &mut State<'_>, _partial: Partial<'_>) -> Result<()> {
        Ok(())
    }

    fn defer_error(&self, leader: &mut State<'_>) -> Result<bool> {
        Ok(stamp_at(leader.bytes))
    }
}

impl Merge for HeldRows {
    fn merge_into(&self, _leader: &mut State<'_>, _partial: Partial<'_>) -> Result<()> {
        Ok(())
    }

    fn defer_error(&self, leader: &mut State<'_>) -> Result<bool> {
        Ok(stamp(leader.bytes))
    }
}

/// The leader's run (`SortRun` at the region's start) emptied and stamped `RUN_HELD`; `false` for a region shorter than
/// its header (nothing to stamp: the error is the statement's). Its kind is a sort run's, so a sort emit's open over it
/// (kernels `begin_in`, D449's pull-form Sort arm) reads the held run as one even when the error came before a width >= 2
/// section laid its run there (the leader's region still an armed share).
pub(crate) fn stamp(bytes: &mut [u8]) -> bool {
    if bytes.len() < SORTRUN_HDR {
        return false;
    }
    // SortRun: cap u32 at 0, len u32 at 4, row_bytes at 8, pad[3] at 12 (pad[2], the run flags, at 14), kind at 15
    bytes[4..8].fill(0);
    bytes[14] |= RUN_HELD;
    bytes[15] = TABLE_SORT;
    true
}

/// D2316: the leader's run stamped `RUN_HELD | RUN_HELD_AT` with its rows kept (`len` as the fill left it); a region
/// whose run never opened (`cap` 0: the error came before the first row) is `stamp`'s empty held run.
pub(crate) fn stamp_at(bytes: &mut [u8]) -> bool {
    if bytes.len() < SORTRUN_HDR || bytes[15] != TABLE_SORT || bytes[0..4] == [0; 4] {
        return stamp(bytes);
    }
    bytes[14] |= RUN_HELD | RUN_HELD_AT;
    true
}
