//! Plan facts: what a WindowAgg reads before its first row, read from the plan node alone (its frame options, partition
//! and order key counts and window functions), for a family that must know how far PostgreSQL's WindowAgg reads its input
//! (the reader rule of decompose ops/setop.rs `readers`).
use types_nodes::plannodes::WindowAgg;
use types_nodes::rawnodes::{FRAMEOPTION_END_CURRENT_ROW, FRAMEOPTION_END_UNBOUNDED_FOLLOWING, FRAMEOPTION_ROWS};
use types_nodes::Node;

use crate::ops::window::{fn_kind, window_funcs_in, FuncList, WinFnKind};

/// Whether PostgreSQL's WindowAgg `w` surely reads its whole input before it returns its first
/// row, raising nothing on the way that a whole fill would not raise at the same row. With no PARTITION BY its partition
/// is its input; ExecWindowAgg evaluates the first row's functions before its projection, qual and run condition, the
/// true window functions first, then eval_windowaggregates, which reads rows until `row_is_in_frame` meets one past the
/// frame's end: the partition's end when the frame ends UNBOUNDED FOLLOWING, or at CURRENT ROW outside ROWS mode with no
/// ORDER BY (every row is a peer). percent_rank, cume_dist and ntile read it whole whatever the frame (window_percent_rank
/// / window_cume_dist / window_ntile ask WinGetPartitionRowCount, which spools the partition, at its first row; ntile
/// reads its bucket count, and raises on one below 1, only after that). Held: an aggregate with such a frame or a
/// percent_rank / cume_dist / ntile, and every function one that raises on no input row while PG reads (an aggregate by
/// `effects::agg_oid_quiet`, a ranking function, or ntile, whose own raise follows the whole read), since a raise in PG's
/// interleaved read would end it at a row the fill reads past (the fill evaluates the arguments as PG does, row by row
/// as it stores them).
pub fn reads_whole<'p>(w: &'p WindowAgg<'p>) -> bool {
    reads_whole_in(w, w.partNumCols == 0, w.ordNumCols == 0)
}

/// `reads_whole` with what decompose's window map knows beyond the plan node (D1486): `one_part`, its input is one
/// partition (no PARTITION BY, or every partition key one value in every row: ops/window.rs `fixed`); `peers`, every input
/// row is a peer of every other (no ORDER BY, or every order key equal in every row under its own equality, which
/// PostgreSQL's planner keeps in the node though it drops the pathkey). A frame ending at CURRENT ROW outside ROWS mode
/// then ends at the partition's end, as with no ORDER BY.
pub fn reads_whole_in<'p>(w: &'p WindowAgg<'p>, one_part: bool, peers: bool) -> bool {
    /// The node's functions (`window_funcs_in`, no list kept): one an aggregate, one a percent_rank / cume_dist / ntile
    /// (`count`, it counts the partition at its first row), every one of `w` and quiet.
    struct Funcs {
        winref: u32,
        agg: bool,
        count: bool,
        quiet: bool,
    }
    impl<'p> FuncList<'p> for Funcs {
        fn has(&self, _: Node<'p>, _: bool) -> bool {
            false
        }
        fn add(&mut self, n: Node<'p>) {
            let Some(f) = n.as_window_func() else { return };
            let kind = fn_kind(f.winfnoid, f.winagg);
            let quiet = match kind {
                WinFnKind::Aggregate => crate::effects::agg_oid_quiet(f.winfnoid, || f.args.iter().next().is_some_and(crate::effects::num_bounded)),
                // ntile: its `nbuckets <= 0` error is raised after WinGetPartitionRowCount read the partition whole
                k => matches!(k, WinFnKind::RowNumber | WinFnKind::Rank | WinFnKind::DenseRank | WinFnKind::PercentRank | WinFnKind::CumeDist | WinFnKind::Ntile),
            };
            self.agg |= f.winagg;
            self.count |= matches!(kind, WinFnKind::PercentRank | WinFnKind::CumeDist | WinFnKind::Ntile);
            self.quiet &= f.winref == self.winref && quiet;
        }
    }
    let o = w.frameOptions;
    let peers = o & FRAMEOPTION_ROWS == 0 && o & FRAMEOPTION_END_CURRENT_ROW != 0 && peers;
    let to_end = o & FRAMEOPTION_END_UNBOUNDED_FOLLOWING != 0 || peers;
    let mut fs = Funcs { winref: w.winref, agg: false, count: false, quiet: true };
    window_funcs_in(w, &mut fs, &|_| true);
    one_part && (to_end && fs.agg || fs.count) && fs.quiet
}
