//! D3585: lowering's graph-wide answers, each worked out once per lowering. Several questions the lowering asks per
//! pipeline or per op walk every pipeline of the graph (`Pipes::inners`, `Gx::merged`, `Gx::group_at`, `Gx::term_arm`,
//! epq `wco_skipped`, hashjoin `quiet_statement`): over an Append whose N arms are N pipelines they made the lowering
//! quadratic in N (4000 partitions: 6.3 s to lower and compile `count(*) .. WHERE v > 3`, which PostgreSQL plans in
//! 0.06 s). `scoped` keeps the answers for the lowering of one graph (lib.rs `lower_scoped`); a reader takes them only
//! for that graph's own pipelines (matched by address and count: the graph is borrowed, never changed, while it lowers),
//! and walks as before anywhere else (a test's graph, a graph lowered inside another's lowering, outside any lowering).
use std::cell::RefCell;
use std::rc::Rc;

use gravity_decompose::PipelineGraph;

leader_tls::leader_tls! {
    thread_local! {
        /// The lowerings in progress on this thread, innermost last.
        static MEMO: RefCell<Vec<Memo>> = const { RefCell::new(Vec::new()) };
    }
}

/// One lowering's answers so far, `None` until first asked.
#[derive(Default)]
pub(crate) struct Memo {
    /// The lowered graph's pipelines: their address and count.
    pipes: (usize, usize),
    /// `Pipes::inners`: each embedded inner's outer `(pipeline, op)`, in index order.
    pub(crate) inners: Option<Rc<[(usize, usize)]>>,
    /// `Gx::merged`: every (slot, graph pipeline reading it, whether the read is a MergeAppend's), by slot then pipeline.
    pub(crate) readers: Option<Rc<[(u8, u32, bool)]>>,
    /// `Gx::group_at`, under the overlay at the address `.0`: per graph pipeline, the first pipeline of its breaker run
    /// from it that declares a group (`u32::MAX`: none).
    pub(crate) group_next: Option<(usize, Rc<[u32]>)>,
    /// `Gx::term_arm`: some graph pipeline holds a recursive union's `SetOp::Iter`.
    pub(crate) iters: Option<bool>,
    /// epq `wco_skipped`: each graph pipeline whose write's check options read builds (`wco_sets`), with those builds.
    pub(crate) wco: Option<Rc<[(u32, Box<[u32]>)]>>,
    /// sort_raise `wrap_feeder`: every (slot, graph pipeline) whose source is that slot's breaker and whose ops hold a
    /// Sort, by slot then pipeline (decompose `sort_over_feeder`'s candidates).
    pub(crate) sort_readers: Option<Rc<[(u8, u32)]>>,
    /// gate `limit_feeder`: every graph op that may make a feeder's check (a Limit's, a gate's), as (pipeline, op), in
    /// order.
    pub(crate) check_ops: Option<Rc<[(u32, u32)]>>,
    /// gate `masks`, under the overlay at the address `.0`: every run_empty mask as (bits, pipeline, op), in order.
    pub(crate) masks: Option<(usize, Rc<[(u64, u32, u32)]>)>,
    /// px `ahead_of_rows`: the first graph pipeline that hands rows to the statement (`None`: none), and the sets
    /// (InitPlans' pipelines, hashed SubPlans' producers) whose feeders it asks about.
    pub(crate) first_emit: Option<Option<u32>>,
    pub(crate) ahead_sets: Option<Rc<[u32]>>,
    /// epq `hashed_skipped`: the last graph pipeline that locks rows and writes none, or holds a write the runner
    /// rechecks (`None`: none).
    pub(crate) last_lock: Option<Option<u32>>,
    /// `Gx::merge_inner`: the lives a merge join's op reads (`life_of` of its slot, in any pipeline or inner), sorted.
    pub(crate) merge_lives: Option<Rc<[u16]>>,
    /// `Gx::set_role`: some graph pipeline holds a set operation's op.
    pub(crate) setops: Option<bool>,
    /// hashjoin `quiet_statement`, for the `Gx` whose overlay, facts and catalog are at the addresses `.0`.
    pub(crate) quiet: Option<((usize, usize, usize), bool)>,
    /// `Gx::drainers`, under the overlay at the address `.0`: every pipeline (inners included) that drains a group, as
    /// (the group's address, pipeline), sorted.
    pub(crate) drains: Option<(usize, Rc<[(usize, u32)]>)>,
    /// `Gx::sorted_right`, under the overlay at the address `.0`: the lives a sorted set operation reads as its right
    /// input (`life_of` of the op's `right` slot), sorted.
    pub(crate) sorted_rights: Option<(usize, Rc<[u16]>)>,
    /// effects `statement_user_code`, under the facts at the address `.0`.
    pub(crate) user_code: Option<(usize, bool)>,
    /// D3681: `Gx::reads_product`: how many graph pipelines produce a slot or hold no Append arm.
    pub(crate) products: Option<usize>,
    /// D3681: each several-target write whose target-wide checks passed in update.rs / delete.rs `admit` (every target's
    /// indexes maintained, the checks road's plan the host's), by its targets' address, with whether any target or the plan
    /// has an index. A refusal keeps nothing.
    pub(crate) targets_ok: Vec<(usize, bool)>,
    /// D3681: each several-target UPDATE whose targets' SET columns update_cells.rs `set_cells` found the root's, in the
    /// root's order, for every target, by its targets' address.
    pub(crate) set_roots_ok: Vec<usize>,
    /// D3703: `Gx::graph_group`: per graph pipeline, the index in `graph.groups` of the first group it produces (`u32::MAX`:
    /// none).
    pub(crate) group_ix: Option<Rc<[u32]>>,
    /// D3703: `cell::key_cell_rep`, for the `Gx` whose overlay, facts and catalog are at the addresses `.0`: per graph
    /// pipeline, the key reps asked so far, as (the key's resno, its expression's address, rep): what the answer reads.
    pub(crate) key_reps: Option<((usize, usize, usize), Vec<Vec<(i16, usize, Option<gravity_program::Rep>)>>)>,
}

/// `f`, with `graph`'s answers kept until it returns (or unwinds).
pub(crate) fn scoped<R>(graph: &PipelineGraph<'_>, f: impl FnOnce() -> R) -> R {
    struct Pop;
    impl Drop for Pop {
        fn drop(&mut self) {
            MEMO.with(|m| m.borrow_mut().pop());
        }
    }
    let pipes = (graph.pipelines.as_ptr() as usize, graph.pipelines.len());
    MEMO.with(|m| m.borrow_mut().push(Memo { pipes, ..Memo::default() })); // alloc: Program
    let _pop = Pop;
    f()
}

/// The key of a pipelines slice.
pub(crate) fn key_of<T>(pipes: &[T]) -> (usize, usize) {
    (pipes.as_ptr() as usize, pipes.len())
}

/// `get` over the answers of the lowering of `pipes` (innermost first); `None` outside one.
pub(crate) fn read<T>(pipes: (usize, usize), get: impl FnOnce(&Memo) -> T) -> Option<T> {
    MEMO.with(|m| m.borrow().iter().rev().find(|x| x.pipes == pipes).map(get))
}

/// `put` into the answers of the lowering of `pipes`, when one is in progress.
pub(crate) fn write(pipes: (usize, usize), put: impl FnOnce(&mut Memo)) {
    MEMO.with(|m| {
        if let Some(x) = m.borrow_mut().iter_mut().rev().find(|x| x.pipes == pipes) {
            put(x);
        }
    });
}

#[cfg(test)]
mod tests {
    use gravity_abi::breaker::{Breaker, BreakerKind};
    use gravity_decompose::catalog::NoFacts;
    use gravity_decompose::ops::shared::append::AppendDesc;
    use gravity_decompose::{Op, PipelineGraph, PipelineNode, Source};
    use gravity_program::{Arena, ArenaVec, End, Pipeline, Reason};

    use crate::graph::Gx;

    /// A graph of `n` pipelines, each ending in a breaker over one of 3 slots and 3 kinds (in runs, as an Append's arms
    /// into one breaker) or in the sink, each reading a slot's breaker or nothing, some with a MergeAppend's op over a slot.
    fn graph(a: gravity_program::ArenaRef, n: usize, seed: u64) -> PipelineGraph<'static> {
        let mut g = PipelineGraph::new(a);
        let mut x = seed | 1;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let kinds = [BreakerKind::Sort, BreakerKind::HashAgg, BreakerKind::Distinct];
        let (mut slot, mut kind) = (0u8, BreakerKind::Sort);
        for k in 0..n {
            let r = next();
            if r % 5 == 0 {
                (slot, kind) = ((r >> 8) as u8 % 3, kinds[(r >> 16) as usize % 3]);
            }
            let end = if r >> 24 & 7 == 0 { End::Sink } else { End::Breaker(Breaker::sized(kind, 64).in_slot(slot)) };
            let source = if r >> 28 & 3 == 0 { Source::Breaker { slot: (r >> 32) as u8 % 3 } } else { Source::Const };
            let mut p = PipelineNode::new(a, k as i32, source, Pipeline::serial(-1, end, Reason::NoDrivingSite));
            if r >> 36 & 7 == 0 {
                p.push(Op::Append(AppendDesc::Merge { slot: (r >> 40) as u8 % 3, children: 2, keys: ArenaVec::new_in(a), prune: None }));
            }
            // D3681: arms of a union, some of them (`Gx::reads_product` counts the pipelines that are no arm)
            if r >> 44 & 3 != 0 {
                p.push(Op::Append(AppendDesc::Arm { i: k as u16, n: n as u16, ordered: false, prune: None }));
            }
            g.pipelines.push(p); // alloc: Cold
        }
        g
    }

    /// D3585: the kept answers equal the walks: `merged`, `term_arm`, `group_at` (with its group's `drainers`),
    /// `sorted_right`, `inners`, D3681's `reads_product` for every pipeline, read twice
    /// in the lowering's scope (the first read keeps, the second reads what was kept) and once outside it.
    #[test]
    fn kept_answers_equal_the_walks() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        for seed in 1..=24u64 {
            // a pipeline node is about 1 KiB and the graph's list doubles: 300 of them take more than 1 MiB
            let arena = Arena::new(1 << 24);
            let g = graph(arena.handle(), 1 + (seed as usize * 37) % 300, seed << 24 ^ seed);
            let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
            let ask = || (0..g.pipelines.len()).map(|k| (gx.merged(k), gx.term_arm(k), gx.group_at(k).map(|g| gx.drainers(g)), gx.sorted_right(gravity_program::LifeId(k as u16)), gx.pipelines().inners().len(), gx.reads_product(k))).collect::<Vec<_>>(); // alloc: Cold
            let walked = ask();
            let (first, kept) = super::scoped(&g, || (ask(), ask()));
            assert_eq!(first, walked, "seed {seed}: the first answers in the scope");
            assert_eq!(kept, walked, "seed {seed}: the kept answers");
            assert!(super::read(super::key_of(&g.pipelines), |_| ()).is_none(), "the scope's answers are dropped at its end");
        }
    }
}
