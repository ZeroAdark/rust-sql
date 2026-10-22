//! The Append / MergeAppend descriptors other families read.
use gravity_program::ArenaVec;
use types_nodes::plannodes::PartitionPruneInfo;

use crate::graph::Order;
use crate::ops::shared::sort::SortKey;
use crate::walk::Open;

/// The pruning the planner left, per partitioned relation: `initial_pruning_steps` (bound once, over extern
/// parameters) and `exec_pruning_steps` (`exec`: decided each time the feeds run — once when the parameters are
/// InitPlan results, per outer row when the feeds are a rerun set's — over the exec parameters `execparamids`).
/// Feed `i` of the slot, in producer order, is subplan `i` of the node (`subplan_map`'s index); arm `i` is subplan `i`.
#[derive(Clone, Copy)]
pub struct PruneDesc<'p> {
    pub info: &'p PartitionPruneInfo<'p>,
    /// The info's position in `partPruneInfos` (`part_prune_index`): the pruning bind word names it.
    pub index: u32,
    pub initial: bool,
    pub exec: bool,
}

/// PSP3-01: the most arms over which a Merge Append whose reader may read every row takes the pull form (lowerv4
/// ops/append/merge_admit.rs `run_stops`). When this was measured the pull form picked each row through 4-wide nodes
/// (re-picking one node per level over the arm that emitted; PostgreSQL's binary heap since D2884, merge_pull_heap.rs);
/// the breaker form merges the arms' runs. Over a whole read of 192k rows (a HAVING no group passes; precheck box 67
/// quiet, W0 / W8) pull / breaker was 0.78 / 0.76 at 34 arms, 0.78 / 0.78 at 48, 0.77 / 0.77 at 64, 0.78 / 0.77 at 96
/// and 0.73 / 0.74 at 128 (mergeappend item 3; over the tree before the last-arm switch a first pass read 1.05 at 64,
/// 1.23 at 96 and 1.29 at 128: PSP3-17 stopped at 33). Past 33 arms a GROUP BY or a join under a LIMIT read 6-15x the
/// bar in the breaker form and 0.78-0.90 of it pulled. Past 128 arms, unmeasured, such a reader keeps the breaker form.
pub const PULL_WHOLE_ARMS: usize = 128;

/// PSP3-01: the most rows a window's cap (decompose ops/window.rs `cap_merge`) pulls, over any arm count: the pull form's
/// extra cost per row over the breaker form's (at most ~90 ns at 64 arms in the same sweep) stays under the landing bar's
/// +0.2 ms slack. A larger cap keeps the breaker form, whose arm reads run side by side at a width: a window reading
/// 190k of 192k rows at W8 took 24.6 / 25.2 ms breaker and 34.5 / 39.5 ms pulled over 12 / 16 arms (box 69, quiet).
pub const PULL_FEW_ROWS: i64 = 1000;

/// `Feed` closes a child into the shared slot; the emit over it carries `Union` (rows in child order)
/// or `Merge` (every child's rows ordered on `keys`: the runs the breaker merges). `Arm` is a streamed union's
/// marker (V3-R1C): child `i` of `n` continues past it into the union's and the parent's ops and the parent's end,
/// no slot between (`ordered`: the planner's ordered-Append mark held on every child, the arms run in child order).
pub enum AppendDesc<'p> {
    Feed { slot: u8, ordered: bool },
    Arm { i: u16, n: u16, ordered: bool, prune: Option<PruneDesc<'p>> },
    Union { slot: u8, children: u16, prune: Option<PruneDesc<'p>> },
    Merge { slot: u8, children: u16, keys: ArenaVec<SortKey>, prune: Option<PruneDesc<'p>> },
}

/// A streamed union's children, held until the parent's end is pushed (`push_union`), and its order (the ordered
/// mark's keys, or none).
pub struct UnionRec<'p> {
    pub arms: ArenaVec<Open<'p>>,
    pub order: Order,
    pub prune: Option<PruneDesc<'p>>,
    /// D1752: the pipeline count before each child was mapped, then after the last (empty when not recorded), and the
    /// arm pipelines `push_union` streamed: lowerv4 runs arm `i` after child `i`'s own pipelines, as PG's Append does.
    pub starts: ArenaVec<u16>,
    pub pushed: core::ops::Range<u16>,
    /// The union is a grouping-sets node's arms (ops/agg/gsets.rs `map_sets`): each arm folds the node's one input, or
    /// its run, by its own set, where an Append's child is a plan of its own. lowerv4 keeps such a union in graph order
    /// (ops/hashjoin/drain/arms.rs `arm_run`).
    pub sets: bool,
}
