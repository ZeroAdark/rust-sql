use gravity_program::{ArenaRef, ArenaVec};
use types_nodes::nodes_enums::CmdType;
use types_nodes::parsenodes::RangeTblEntry;
use types_nodes::plannodes::{Plan, PlannedStmt};
use types_nodes::Node;

use crate::catalog::{Facts, RelFact};
use crate::graph::{Bound, PipelineGraph, PipelineNode};
use crate::ledger::Kind;
use crate::admit::Refusal;
use crate::ops::{agg, append, setop as setops, sort, scan as sources, srf, subplan, window, modifytable as write};
use crate::ops::scan::closure;
use crate::{join, ops};

pub type Open<'p> = PipelineNode<'p>;

/// D3748: PostgreSQL's check_stack_depth at a level of one of decompose's own recursions (the plan mapping `Builder::plan`,
/// whose set-operation, join and subquery arms recurse once per plan level; a plan-tree reader such as the rerun walk; the
/// expression walker `expr::walk`). Past the enforced max_stack_depth it raises 54001 'stack depth limit exceeded', as
/// PostgreSQL's planner recursion does, never running into the thread's stack end (a 640-deep EXCEPT chain aborted
/// postgres --single at ulimit -s 8192, its decompose frames about 16 KB per level). The error is the guard's PgError
/// panic payload (stack_depth_core::check_stack_depth_shadow_heavy_or_panic): a walk that returns a bool or an Option
/// raises it too, and lowerv4's `catch` and the statement boundary pass a PgError on unchanged. D3812: these frames keep
/// most of their state in by-memory aggregates, which wasm32 puts on its shadow stack, so on wasm32 the check counts a
/// part of them against the ceiling derived from the engine stack (the plain check stopped a 100-table join chain the
/// engine runs); everywhere else it is the plain check.
#[inline]
pub fn stack_check() {
    stack_depth_core::check_stack_depth_shadow_heavy_or_panic();
}

pub struct Builder<'p, 'f> {
    pub arena: ArenaRef,
    /// The admission's planning context: scratch only (`alloc` is the node allocator, a product).
    pub mcx: mcx::Mcx<'f>,
    pub stmt: &'p PlannedStmt<'p>,
    pub facts: &'f dyn Facts,
    pub graph: PipelineGraph<'p>,
    pub limit: Option<(i32, Bound<'p>, Bound<'p>, bool)>,
    /// D612: the key Sort (its node id) the sorted group above it absorbs (`absorb::key_sort`): `sort::map` maps its
    /// input alone.
    pub absorb: Option<i32>,
    /// D4676 (road `group.hashchain`, ops/group/absorb.rs `chain_of`): the Unique (its node id) under a sorted group
    /// that hashes its rows and orders only its own groups: `agg::map_set` absorbs the Unique's key Sort whatever its key
    /// types and emits the Unique's rows unordered.
    pub chain: Option<i32>,
    /// D3227 K3: the plan Material (its node id) a keyed nest reads its inner through (self_spool.rs `through_material`):
    /// `map_material` maps its child alone, and the nest closes the Materialize itself where the keyed form stands down.
    pub keyed_material: Option<i32>,
    pub words: u16,
    pub work: ArenaVec<(i32, u8)>,
    /// The CTEs whose rows are spooled (D4): `(ctePlanId, slot)`, pushed by `subplan::map_init_plans`.
    pub ctes: ArenaVec<(i32, u8)>,
    /// The plan node that closed each of `graph.pipelines` (parallel to it): `rerun::declare_rerun` reads its `allParam`.
    pub closers: ArenaVec<Node<'p>>,
    /// The node whose mapping is running (`plan`): the pipelines it pushes between two of its children's `plan` calls
    /// are its own (a grouping set's arm, a MergeAppend child's feed), attributed before the next child maps.
    pub mapping: Option<Node<'p>>,
    /// The node allocator (`decompose_in`): a MERGE's carried columns are synthesized nodes.
    pub alloc: Option<mcx::Mcx<'p>>,
    /// The MERGE target's columns carried through the child's projections (carry.rs).
    pub carry: Option<crate::ops::shared::carry::Carry>,
    /// V4R-540: each plan node's parent and whether the statement may build lazily (demand/readers.rs `Parents`), filled once.
    pub parents: Option<crate::facts::demand::readers::Parents<'p>>,
    /// C5 (c): the statement's column demand (facts/demand.rs `reads_above`), built top-down once by `demand::fill`.
    pub demand: Option<crate::facts::demand::Demand>,
    /// C5 (c)'s checker (debug builds): the demand answers logged for its census (facts/demand/census.rs).
    #[cfg(debug_assertions)]
    pub demand_log: crate::facts::demand::census::Log<'p>,
    /// v5 errorhold (D1814): the sorted groups (plan node ids) whose fold ahead runs under a granted licence
    /// (`gsets::fold_ahead`): the Limit's stop check (`limit::ahead`) leaves what such a fold raises to the re-run.
    pub ahead_licensed: ArenaVec<i32>,
    /// The node whose mapping runs (`plan`) may hand its rows up in any order: no reader above it depends on its row
    /// order (`order_free_under`). An index scan under such a reader may run as the bitmap morsel form (scan/mod.rs
    /// `map_index`), whose rows come in heap block order.
    pub order_free: bool,
    /// D4568: `order_free`, with a hash join's build input free too unless its chain order is visible
    /// (facts/order.rs `merge_free_under`). Read by the merge mark alone (ops/scan `map_seq_scan`).
    pub merge_free: bool,
    /// D3373 (E-1): the recursive unions (bit = CTE plan id) whose seed another union's `readers` reads through as far as
    /// their reader reads: each must pull its seed (ops/setop.rs `map_recursive_union`, else refused by D2212's name).
    pub seed_pulls: u64,
    /// D3373: each nested SubPlan's plan as mapped (`map_subplan`): plan id, first pipeline, end (nested_arms.rs `copy_sets`).
    pub nested_spans: ArenaVec<(i32, u16, u16)>,
    /// D3681: the Append (its plan node id; -1 none) a top-level UPDATE / DELETE over twin result relations reads as one
    /// relation set (modifytable/mod.rs `set_write`, ops/append.rs `relsets`).
    pub write_set: i32,
    /// D4779 (`pgrust.gravity_cte_pushdown` = fill): each SeqScan (its node id) in a fill-marked CTE's plan and the
    /// readers' filter it takes after its own quals (facts/cte_fill.rs `plan`, read by ops/scan `map_seq_scan`).
    pub fill_quals: ArenaVec<(i32, Node<'p>)>,
}

pub const STMT_NODE: i32 = -1;


impl<'p, 'f> Builder<'p, 'f> {
    /// The program's one kernel-word allocator (audit C4): `n` words above the fold's fixed ones (`agg::NWORDS`), `None` past
    /// `limits::KERNEL_WORDS`; no pipeline takes a word another pipeline declares.
    pub fn word_block(&mut self, n: u16) -> Option<u16> {
        let w = crate::ops::agg::NWORDS + self.words;
        if (w + n) as usize > gravity_program::limits::KERNEL_WORDS {
            return None;
        }
        self.words += n;
        Some(w)
    }

    pub fn new(arena: ArenaRef, stmt: &'p PlannedStmt<'p>, facts: &'f dyn Facts, mcx: mcx::Mcx<'f>) -> Builder<'p, 'f> {
        Self::new_in(arena, stmt, facts, mcx)
    }

    pub fn new_in(arena: ArenaRef, stmt: &'p PlannedStmt<'p>, facts: &'f dyn Facts, mcx: mcx::Mcx<'f>) -> Builder<'p, 'f> {
        Builder { arena, mcx, stmt, facts, graph: PipelineGraph::new(arena), limit: None, absorb: None, chain: None, keyed_material: None, words: 0, work: ArenaVec::new_in(arena), ctes: ArenaVec::new_in(arena), closers: ArenaVec::new_in(arena), mapping: None, alloc: None, carry: None, parents: None, demand: None, #[cfg(debug_assertions)] demand_log: crate::facts::demand::census::new_log(arena), ahead_licensed: ArenaVec::new_in(arena), order_free: false, merge_free: false, seed_pulls: 0, nested_spans: ArenaVec::new_in(arena), write_set: -1, fill_quals: ArenaVec::new_in(arena) }
    }

    pub fn run(mut self) -> Result<PipelineGraph<'p>, Refusal> {
        if self.stmt.commandType == CmdType::CMD_UTILITY || self.stmt.commandType == CmdType::CMD_UNKNOWN {
            return Err(Refusal::shape(Kind::Result, STMT_NODE, "a utility statement is not executed"));
        }
        let root = self.stmt.planTree.ok_or(Refusal::shape(Kind::Result, STMT_NODE, "no plan tree"))?;
        self.graph.partial = crate::admit::partial_root();
        if !self.stmt.subplans.is_nil() {
            subplan::map_stmt(&mut self)?;
        }
        self.run_from(root)
    }

    /// The graph of one plan tree of the statement (a subplan's, for D4's CTE and the tests); the statement-level hooks are the caller's.
    pub fn run_from(mut self, root: Node<'p>) -> Result<PipelineGraph<'p>, Refusal> {
        // D4779: the fill filters of the statement's fill-marked CTEs, before any of their scans maps
        crate::facts::cte_fill::plan(&mut self);
        if !self.stmt.partPruneInfos.is_nil() {
            append::map_prune(&mut self)?;
        }
        if !self.stmt.rowMarks.is_nil() && Kind::of(root.node_tag()) != Some(Kind::LockRows) {
            write::map_marks(&mut self)?;
        }
        let open = self.plan(root)?;
        let open = crate::nested_arms::copy_sets(&mut self, open)?;
        // D4120: every rerun set is declared now; a kept fill's param none of them claimed refuses
        if let Some(r) = crate::lifetime::refills_left(&self.graph) {
            return Err(r);
        }
        #[cfg(debug_assertions)]
        crate::facts::demand::census::check(&self)?;
        let facts = self.facts;
        let mut g = self.graph;
        // a streamed union's root ends in one sink per arm: the last is the root
        g.root = *g.push(open).last().expect("a push ends at least one pipeline");
        crate::normalize::graph(&mut g, facts, self.alloc)?;
        if let Some(r) = g.fault.or_else(|| crate::ops::subplan::unplaced(&g)) {
            return Err(r);
        }
        if g.pipelines.len() > gravity_program::limits::PIPELINES {
            return Err(Refusal::shape(Kind::Append, STMT_NODE, "more than 65535 pipelines (the u16 pipeline id)"));
        }
        crate::outs::annotate(&mut g, facts);
        crate::outs::param_reps(&mut g, facts);
        Ok(g)
    }

    pub fn plan(&mut self, node: Node<'p>) -> Result<Open<'p>, Refusal> {
        stack_check(); // D3748
        // D240: what the parent pushed since its last child mapped is the parent's (its allParam holds every child's,
        // as PG's rescan reaches every child); left open, the child's own attribution would claim it
        if let Some(parent) = self.mapping {
            self.closed_by(parent);
        }
        let free = crate::facts::order::order_free_under(self, self.mapping, node, self.order_free);
        let mfree = crate::facts::order::merge_free_under(self, self.mapping, node, self.merge_free, free);
        let node = crate::normalize::node(self, node)?;
        let saved = core::mem::replace(&mut self.order_free, free);
        let msaved = core::mem::replace(&mut self.merge_free, mfree);
        let parent = self.mapping.replace(node);
        let open = self.map_node(node);
        self.mapping = parent;
        self.order_free = saved;
        self.merge_free = msaved;
        open
    }

    /// D3748: one plan node's mapping. Never inlined into `plan`, and its arms (`map_kind`) and its close (`map_tail`) run in
    /// frames of their own, the open passed back in the return value, so a nest of plan nodes keeps small frames live per
    /// level.
    #[inline(never)]
    fn map_node(&mut self, node: Node<'p>) -> Result<Open<'p>, Refusal> {
        let k = Kind::of(node.node_tag()).ok_or(Refusal::shape(Kind::Result, STMT_NODE, "not a plan node"))?;
        let id = node.as_plan().map_or(STMT_NODE, |p| p.plan_node_id);
        let outer = self.graph.cur_node.take(); // an InitPlan's pipelines close under no node
        if node.as_plan().is_some_and(|p| !p.initPlan.is_nil()) {
            subplan::map_init_plans(self, node, k)?;
        }
        self.graph.cur_node = Some(id);
        let c0 = self.closers.len();
        let mut open = self.map_kind(node, k, id);
        if open.is_ok() {
            self.map_tail(node, k, id, outer, c0, &mut open);
        }
        open
    }

    /// D3748: node `node`'s own mapping, by its kind.
    #[inline(never)]
    fn map_kind(&mut self, node: Node<'p>, k: Kind, id: i32) -> Result<Open<'p>, Refusal> {
        match k {
            Kind::Result => ops::map_result(self, node.as_result().unwrap()),
            Kind::SeqScan => sources::map_seq_scan(self, node.as_seq_scan().unwrap()),
            Kind::IndexScan => sources::map_index_scan(self, node.as_index_scan().unwrap()),
            Kind::IndexOnlyScan => sources::map_index_only_scan(self, node.as_index_only_scan().unwrap()),
            Kind::ValuesScan => sources::map_values_scan(self, node.as_values_scan().unwrap()),
            Kind::Limit => ops::map_limit(self, node.as_limit().unwrap()),
            Kind::Agg | Kind::Group | Kind::Unique => agg::map(self, node, k),
            Kind::NestLoop | Kind::MergeJoin | Kind::HashJoin | Kind::Hash | Kind::Material | Kind::Memoize | Kind::NestLoopParam => join::map(self, node, k),
            Kind::Sort | Kind::IncrementalSort => sort::map(self, node, k),
            Kind::WindowAgg => window::map(self, node, k),
            Kind::SetOp | Kind::RecursiveUnion | Kind::WorkTableScan => setops::map(self, node, k),
            Kind::SubqueryScan | Kind::CteScan | Kind::SubPlan | Kind::AlternativeSubPlan => subplan::map(self, node, k),
            Kind::Append | Kind::MergeAppend | Kind::PartitionPruneInfo => append::map(self, node, k),
            Kind::ModifyTable | Kind::LockRows | Kind::PlanRowMark => write::map(self, node, k),
            Kind::ProjectSet | Kind::FunctionScan => srf::map(self, node, k),
            Kind::BitmapAnd | Kind::BitmapOr | Kind::BitmapIndexScan | Kind::BitmapHeapScan | Kind::TidScan | Kind::TidRangeScan | Kind::SampleScan | Kind::TableFuncScan | Kind::NamedTuplestoreScan => closure::map(self, node, k),
            Kind::ForeignScan | Kind::CustomScan | Kind::Gather | Kind::GatherMerge | Kind::PlanInvalItem => Err(Refusal::kind(k, id)),
        }
    }

    /// D3748: what follows a node's own mapping (its carried columns, its SubPlans, its close), in a frame of its own:
    /// `r`, the open `map_kind` returned, becomes the node's.
    #[inline(never)]
    fn map_tail(&mut self, node: Node<'p>, k: Kind, id: i32, outer: Option<i32>, c0: usize, r: &mut Result<Open<'p>, Refusal>) {
        *r = match core::mem::replace(r, Err(Refusal::kind(k, id))) {
            Ok(open) => self.map_close(node, k, id, outer, c0, open),
            Err(e) => Err(e),
        };
    }

    fn map_close(&mut self, node: Node<'p>, k: Kind, id: i32, outer: Option<i32>, c0: usize, open: Open<'p>) -> Result<Open<'p>, Refusal> {
        let open = crate::ops::modifytable::carry::after(self, node, k, open)?;
        self.graph.cur_node = None; // a per-row SubPlan's hashed set is no node's breaker
        let mut open = subplan::attach(self, node, k, open)?;
        open.close_node(id);
        // DR-1 / DR-4: the rows the pipeline carries are this node's, at the planner's width (the breaker a parent closes over it sizes its record by it)
        open.desc.row_width = node.as_plan().map_or(open.desc.row_width, |p| p.plan_width.max(0) as u32);
        // D3205: and at its rows (a serial pipeline's breaker is sized by them, its `predicted_rows` being 0)
        open.desc.end_rows = node.as_plan().map_or(open.desc.end_rows, Self::rows);
        self.graph.cur_node = outer;
        self.closed_by(node);
        if let Some(child) = node.as_limit().and_then(|l| l.plan.lefttree).and_then(|c| c.as_plan()) {
            // A Limit's bounds fold into the breaker its child closes (a top-k sort): the Limit closed it.
            for c in self.closers[c0..].iter_mut().filter(|c| c.as_plan().is_some_and(|p| p.plan_node_id == child.plan_node_id)) {
                *c = node;
            }
        }
        Ok(open)
    }

    /// Every pipeline pushed since the last attribution was closed by `node` (a mapping that closes a child's
    /// pipeline names the child: a Hash, a merge join's inner, a subplan's root).
    pub fn closed_by(&mut self, node: Node<'p>) {
        while self.closers.len() < self.graph.pipelines.len() {
            self.closers.push(node);
        }
    }

    pub fn child(&mut self, plan: &Plan<'p>, k: Kind) -> Result<Open<'p>, Refusal> {
        match plan.lefttree {
            Some(l) => {
                if plan.righttree.is_some() {
                    return Err(Refusal::shape(k, plan.plan_node_id, "an unexpected right subtree"));
                }
                self.plan(l)
            }
            None => Err(Refusal::shape(k, plan.plan_node_id, "no subtree")),
        }
    }

    pub fn rte(&self, scanrelid: u32) -> Option<&'p RangeTblEntry<'p>> {
        let n = (scanrelid as usize).checked_sub(1)?;
        if n >= self.stmt.rtable.len() {
            return None;
        }
        self.stmt.rtable.nth(n).as_range_tbl_entry()
    }

    pub fn relation(&self, scanrelid: u32, k: Kind, node_id: i32) -> Result<RelFact, Refusal> {
        let rte = self.rte(scanrelid).ok_or(Refusal::shape(k, node_id, "scanrelid outside the range table"))?;
        if rte.relid == 0 {
            return Err(Refusal::shape(k, node_id, "the range table entry is not a relation"));
        }
        self.facts.rel(rte.relid).ok_or(Refusal::shape(k, node_id, "no relation facts"))
    }

    pub fn rows(plan: &Plan<'_>) -> u64 {
        let rows = crate::admit::plan_rows(plan);
        if rows.is_finite() && rows > 0.0 {
            rows as u64
        } else {
            0
        }
    }
}
