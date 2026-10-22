//! Road `merge.ksort` (H19 / H26b, D3190): a merge join's inner Sort filled wide. Its pipeline keeps runs.sort's end on
//! every worker (each sorts its share, sealed in place while every share fits, else put) and the sort's merge protocol at
//! the close (exec ops/sort/merge.rs), which lays the workers' runs as one sealed run in the slot's region: merge.join's
//! reader reads it by position as a serial fill's (mark / restore a word), and runs the workers put merged into one through
//! the window (D3105 (b), kernels mergejoin raw/window.rs). The end is face/slot_run.rs's (`wide_fill`). Owner M11-MERGE.
use crate::ops::Table;

crate::strategies! {
    /// Parallel sorted side, markable k-way: a merge join's inner Sort filled on every worker, merged at the barrier.
    MERGE_KSORT = 116, "merge.ksort", Merge;
    /// D3336 (pmerge P1): a merge join's inner Sort over a group emit filled by the emit's claims (`emit_fill`); off: the leader.
    MERGE_WIDEEMIT = 137, "merge.wideemit", Merge;
}

pub(crate) const ROWS: Table = Table { strategies: &[(MERGE_KSORT, &()), (MERGE_WIDEEMIT, &())], ..crate::ops::NONE };
