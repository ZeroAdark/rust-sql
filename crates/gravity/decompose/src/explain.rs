//! The plan facts Gravity's EXPLAIN ANALYZE reads at print time (exec admit/entry instrument.rs, docs/gravity/entry.md §4).
use types_nodes::jointype::JoinType;
use types_nodes::plannodes::{Append, BitmapAnd, BitmapHeapScan, BitmapIndexScan, BitmapOr, IndexOnlyScan, MergeAppend, PlannedStmt, SubqueryScan};
use types_nodes::{Node, NodeTag};

/// One hash join above a node: the side the node is on and the nodes whose counts tell how PostgreSQL read that side.
pub struct HashSide {
    /// The node is under the join's inner (build) side; else under its outer side.
    pub inner: bool,
    /// The join keeps that side's unmatched rows (an outer side: LEFT, ANTI, FULL; an inner side: RIGHT, RIGHT ANTI,
    /// FULL), so PostgreSQL reads the side whole whatever the other side holds.
    pub keeps: bool,
    /// The join's node id.
    pub join: i32,
    /// Its outer child's node id.
    pub outer: Option<i32>,
    /// Its inner child's (the Hash node's) id.
    pub build: Option<i32>,
}

/// Node `id` in the statement's plan tree or its subplans: the plan nodes above it (each with the side taken, its inner
/// or not) and the node.
fn located<'m, 'a>(m: mcx::Mcx<'m>, stmt: &PlannedStmt<'a>, id: i32) -> Option<(mcx::PgVec<'m, (Node<'a>, bool)>, Node<'a>)> {
    let (mut path, mut found) = (mcx::PgVec::new_in(m), None);
    let mut roots = stmt.planTree.into_iter().chain(stmt.subplans.iter().flatten());
    roots.any(|r| find(r, id, &mut path, &mut found)).then_some(())?;
    found.map(|n| (path, n))
}

/// Node `id`'s rows are not a count the run can print as PostgreSQL's: a bitmap index scan, BitmapAnd or BitmapOr
/// (PostgreSQL counts the TIDs it puts in the bitmap, or none, not rows), or a node under a merge join (PostgreSQL's
/// merge join reads each input only until the other ends, and never starts its inner after an empty outer, where the
/// run sorts and reads both whole).
pub fn uncountable(stmt: &PlannedStmt<'_>, id: i32) -> bool {
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // the walk's path: gone with the call
    let Some((path, n)) = located(cx.mcx(), stmt, id) else { return false };
    matches!(n.node_tag(), NodeTag::T_BitmapIndexScan | NodeTag::T_BitmapAnd | NodeTag::T_BitmapOr)
        || path.iter().any(|(a, _)| a.node_tag() == NodeTag::T_MergeJoin)
}

/// The hash joins above node `id` in the statement's plan tree or its subplans, the nearest last (empty when `id` is
/// under none or is not found).
pub fn hash_sides<'m>(m: mcx::Mcx<'m>, stmt: &PlannedStmt<'_>, id: i32) -> mcx::PgVec<'m, HashSide> {
    let Some((path, _)) = located(m, stmt, id) else { return mcx::PgVec::new_in(m) };
    let id_of = |n: Option<Node<'_>>| n.and_then(|n| n.as_plan()).map(|p| p.plan_node_id);
    mcx::CollectIn::collect_in(
        path.iter().filter_map(|&(h, inner)| {
            let (p, j) = (h.as_plan()?, h.as_hash_join()?);
            let jt = j.join.jointype;
            let keeps = match inner {
                false => matches!(jt, JoinType::JOIN_LEFT | JoinType::JOIN_ANTI | JoinType::JOIN_FULL),
                true => matches!(jt, JoinType::JOIN_RIGHT | JoinType::JOIN_RIGHT_ANTI | JoinType::JOIN_FULL),
            };
            Some(HashSide { inner, keeps, join: p.plan_node_id, outer: id_of(p.lefttree), build: id_of(p.righttree) })
        }),
        m,
    )
}

/// Node `id`'s heap and index when it is an Index Only Scan and the only one over them in the statement's plan tree and
/// its subplans (a site that walks that index for its heap's rows is then this node's); `None` otherwise.
pub fn sole_index_only(stmt: &PlannedStmt<'_>, id: i32) -> Option<(u32, u32)> {
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // the walk's rows: gone with the call
    let mut scans = mcx::PgVec::new_in(cx.mcx());
    stmt.planTree.into_iter().chain(stmt.subplans.iter().flatten()).for_each(|r| index_only(r, &mut scans));
    let rel = |rti: u32| (rti as usize).checked_sub(1).filter(|&i| i < stmt.rtable.len()).and_then(|i| stmt.rtable.nth(i).as_range_tbl_entry()).map(|r| r.relid);
    let &(_, rti, index) = scans.iter().find(|s| s.0 == id)?;
    let heap = rel(rti)?;
    (scans.iter().filter(|s| s.2 == index && rel(s.1) == Some(heap)).count() == 1).then_some((heap, index))
}

/// Node `id`'s Bitmap Heap Scan and its heap and index (heap scan node, heap relid, index), when `id` is a Bitmap Heap
/// Scan whose bitmap is one Bitmap Index Scan, or that Bitmap Index Scan, and the pair is the only one over that heap and
/// index in the statement's plan tree and its subplans (a site that builds that index's bitmap for its heap's rows is
/// then this pair's); `None` otherwise (a BitmapAnd / BitmapOr input).
pub fn sole_bitmap_leaf(stmt: &PlannedStmt<'_>, id: i32) -> Option<(i32, u32, u32)> {
    bitmap_leaf(stmt, id).filter(|b| b.3).map(|b| (b.0, b.1, b.2))
}

/// Node `id`'s Bitmap Heap Scan and its heap and index as `sole_bitmap_leaf` reads them, with whether the pair is the only
/// one over that heap and index (`.3`): where it is not (a write's arm over a leaf its source scans again, partition_prune
/// #420), only the site the heap scan's own pipeline drives is the pair's (exec instrument.rs `bitmap_counts`).
pub fn bitmap_leaf(stmt: &PlannedStmt<'_>, id: i32) -> Option<(i32, u32, u32, bool)> {
    fn walk(n: Node<'_>, out: &mut mcx::PgVec<'_, (i32, i32, u32, u32)>) {
        crate::walk::stack_check(); // D3748
        let Some(p) = n.as_plan() else { return };
        if let (Some(h), Some(leaf)) = (n.as_variant::<BitmapHeapScan>(), p.lefttree) {
            if let (Some(l), Some(lp)) = (leaf.as_variant::<BitmapIndexScan>(), leaf.as_plan()) {
                mcx::GrowIn::push_in(out, (p.plan_node_id, lp.plan_node_id, h.scan.scanrelid, l.indexid));
            }
        }
        children(n).for_each(|(c, _)| walk(c, out));
    }
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan");
    let mut pairs = mcx::PgVec::new_in(cx.mcx());
    stmt.planTree.into_iter().chain(stmt.subplans.iter().flatten()).for_each(|r| walk(r, &mut pairs));
    let rel = |rti: u32| (rti as usize).checked_sub(1).filter(|&i| i < stmt.rtable.len()).and_then(|i| stmt.rtable.nth(i).as_range_tbl_entry()).map(|r| r.relid);
    let &(heap_node, _, rti, index) = pairs.iter().find(|s| s.0 == id || s.1 == id)?;
    let heap = rel(rti)?;
    Some((heap_node, heap, index, pairs.iter().filter(|s| s.3 == index && rel(s.2) == Some(heap)).count() == 1))
}

/// Bitmap Heap Scan `id` reads, under a Materialize, a relation the statement writes (one of its result relations):
/// PostgreSQL fills a Materialize as its reader pulls, between the write's rows, where the run fills it whole at the
/// first outer row, so the entries its index met and the pages it read (a row the write moved in since) are not the
/// run's to print (exec instrument.rs `bitmap_counts`, partition_prune #420's source under the Materialize).
pub fn written_under_material(stmt: &PlannedStmt<'_>, id: i32) -> bool {
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // the walk's path: gone with the call
    let Some((path, n)) = located(cx.mcx(), stmt, id) else { return false };
    let Some(h) = n.as_variant::<BitmapHeapScan>() else { return false };
    if !path.iter().any(|(a, _)| a.as_variant::<types_nodes::plannodes::Material>().is_some()) {
        return false;
    }
    let rel = |rti: u32| (rti as usize).checked_sub(1).filter(|&i| i < stmt.rtable.len()).and_then(|i| stmt.rtable.nth(i).as_range_tbl_entry()).map(|r| r.relid);
    let Some(heap) = rel(h.scan.scanrelid) else { return false };
    stmt.resultRelations.iter().any(|r| rel(r as u32) == Some(heap))
}

/// Node `id` is a Materialize (its tuplestore: exec instrument.rs `material_store`).
pub fn is_material(stmt: &PlannedStmt<'_>, id: i32) -> bool {
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // the walk's path: gone with the call
    located(cx.mcx(), stmt, id).is_some_and(|(_, n)| n.as_variant::<types_nodes::plannodes::Material>().is_some())
}

/// Node `id`'s heap and index when it is an Index Scan and the only one over them in the statement's plan tree and its
/// subplans (a site that walks that index for its heap's rows is then this node's); `None` otherwise.
pub fn sole_index_scan(stmt: &PlannedStmt<'_>, id: i32) -> Option<(u32, u32)> {
    fn walk(n: Node<'_>, out: &mut mcx::PgVec<'_, (i32, u32, u32)>) {
        crate::walk::stack_check(); // D3748
        let Some(p) = n.as_plan() else { return };
        if let Some(s) = n.as_variant::<types_nodes::plannodes::IndexScan>() {
            mcx::GrowIn::push_in(out, (p.plan_node_id, s.scan.scanrelid, s.indexid));
        }
        children(n).for_each(|(c, _)| walk(c, out));
    }
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // the walk's rows: gone with the call
    let mut scans = mcx::PgVec::new_in(cx.mcx());
    stmt.planTree.into_iter().chain(stmt.subplans.iter().flatten()).for_each(|r| walk(r, &mut scans));
    let rel = |rti: u32| (rti as usize).checked_sub(1).filter(|&i| i < stmt.rtable.len()).and_then(|i| stmt.rtable.nth(i).as_range_tbl_entry()).map(|r| r.relid);
    let &(_, rti, index) = scans.iter().find(|s| s.0 == id)?;
    let heap = rel(rti)?;
    (scans.iter().filter(|s| s.2 == index && rel(s.1) == Some(heap)).count() == 1).then_some((heap, index))
}

/// Whether plan node `id` is an Index Only Scan.
pub fn is_index_only(stmt: &PlannedStmt<'_>, id: i32) -> bool {
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // the walk's path: gone with the call
    located(cx.mcx(), stmt, id).is_some_and(|(_, n)| n.node_tag() == NodeTag::T_IndexOnlyScan)
}

/// Seq Scan `id`'s relation, when it is the plan's only scan of it (its sites' counts are the node's).
pub fn sole_seq_scan(stmt: &PlannedStmt<'_>, id: i32) -> Option<u32> {
    fn walk(n: Node<'_>, out: &mut mcx::PgVec<'_, (i32, u32, bool)>) {
        crate::walk::stack_check(); // D3748
        let Some(p) = n.as_plan() else { return };
        if let Some(s) = n.as_variant::<types_nodes::plannodes::SeqScan>() {
            mcx::GrowIn::push_in(out, (p.plan_node_id, s.scan.scanrelid, true));
        } else if let Some(s) = n.as_variant::<types_nodes::plannodes::IndexScan>() {
            mcx::GrowIn::push_in(out, (p.plan_node_id, s.scan.scanrelid, false));
        } else if let Some(s) = n.as_variant::<IndexOnlyScan>() {
            mcx::GrowIn::push_in(out, (p.plan_node_id, s.scan.scanrelid, false));
        } else if let Some(s) = n.as_variant::<BitmapHeapScan>() {
            mcx::GrowIn::push_in(out, (p.plan_node_id, s.scan.scanrelid, false));
        }
        children(n).for_each(|(c, _)| walk(c, out));
    }
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // the walk's rows: gone with the call
    let mut scans = mcx::PgVec::new_in(cx.mcx());
    stmt.planTree.into_iter().chain(stmt.subplans.iter().flatten()).for_each(|r| walk(r, &mut scans));
    let rel = |rti: u32| (rti as usize).checked_sub(1).filter(|&i| i < stmt.rtable.len()).and_then(|i| stmt.rtable.nth(i).as_range_tbl_entry()).map(|r| r.relid);
    let &(_, rti, _) = scans.iter().find(|s| s.0 == id && s.2)?;
    let heap = rel(rti)?;
    (scans.iter().filter(|s| rel(s.1) == Some(heap)).count() == 1).then_some(heap)
}

/// D3500: the MERGE ModifyTable node whose outer plan (the source and target's join) is node `id`, if it is one.
pub fn merge_outer_of(stmt: &PlannedStmt<'_>, id: i32) -> Option<i32> {
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // the walk's path: gone with the call
    let (path, _) = located(cx.mcx(), stmt, id)?;
    let (parent, _) = path.last()?;
    let mt = parent.as_modify_table().filter(|m| m.operation == types_nodes::nodes_enums::CmdType::CMD_MERGE)?;
    mt.plan.lefttree.and_then(|l| l.as_plan()).filter(|p| p.plan_node_id == id).map(|_| mt.plan.plan_node_id)
}

/// Seq Scan node `id`'s relation (its range-table entry's relid).
pub fn seq_scan_rel(stmt: &PlannedStmt<'_>, id: i32) -> Option<u32> {
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // the walk's path: gone with the call
    let s = located(cx.mcx(), stmt, id)?.1.as_variant::<types_nodes::plannodes::SeqScan>()?;
    let i = (s.scan.scanrelid as usize).checked_sub(1).filter(|&i| i < stmt.rtable.len())?;
    stmt.rtable.nth(i).as_range_tbl_entry().map(|r| r.relid)
}

/// Node `id`'s arms, the plan node above it and its `part_prune_index` (-1: none), when `id` is an Append or MergeAppend:
/// the nodes whose starts are its own (PostgreSQL starts an Append with its parent and each arm it runs under it).
pub fn append_family<'m>(m: mcx::Mcx<'m>, stmt: &PlannedStmt<'_>, id: i32) -> Option<(mcx::PgVec<'m, i32>, Option<i32>, i32)> {
    let (path, n) = located(m, stmt, id)?;
    let prune = match n.node_tag() {
        NodeTag::T_Append => n.as_variant::<Append>()?.part_prune_index,
        NodeTag::T_MergeAppend => n.as_variant::<MergeAppend>()?.part_prune_index,
        _ => return None,
    };
    let arms = mcx::CollectIn::collect_in(children(n).filter_map(|(c, _)| c.as_plan().map(|p| p.plan_node_id)), m);
    Some((arms, path.last().and_then(|(a, _)| a.as_plan()).map(|p| p.plan_node_id), prune))
}

/// Node `id` and every plan node under it (its children, an Append's arms, a SubqueryScan's plan, a bitmap's inputs),
/// `id` first: the nodes whose work PostgreSQL's per-node counters for `id` include (empty when `id` is not found).
pub fn subtree<'m>(m: mcx::Mcx<'m>, stmt: &PlannedStmt<'_>, id: i32) -> mcx::PgVec<'m, i32> {
    let mut out = mcx::PgVec::new_in(m);
    if let Some((_, n)) = located(m, stmt, id) {
        under(n, &mut out);
    }
    out
}

/// The workers the statement's Gather and Gather Merge nodes plan (their `num_workers`, summed over the plan tree and its
/// subplans): what PostgreSQL's ExecGather / ExecGatherMerge add to `es_parallel_workers_to_launch` as each starts.
pub fn gather_workers(stmt: &PlannedStmt<'_>) -> i64 {
    fn walk(n: Node<'_>) -> i64 {
        crate::walk::stack_check(); // D3748
        let own = n.as_gather().map(|g| g.num_workers).or_else(|| n.as_gather_merge().map(|g| g.num_workers)).unwrap_or(0) as i64;
        own + children(n).map(|(c, _)| walk(c)).sum::<i64>()
    }
    stmt.planTree.into_iter().chain(stmt.subplans.iter().flatten()).map(walk).sum()
}

fn under(n: Node<'_>, out: &mut mcx::PgVec<'_, i32>) {
    crate::walk::stack_check(); // D3748
    let Some(p) = n.as_plan() else { return };
    mcx::GrowIn::push_in(out, p.plan_node_id);
    children(n).for_each(|(c, _)| under(c, out));
}

/// Index scan node `id`'s (plain, index-only or bitmap) index quals as btree keys: per qual its index column and its
/// operator (`None`: an IS [NOT] NULL test, an equality to the btree); `None` when a qual is another form (a row
/// comparison, an array) or the node is no index scan.
pub fn index_keys<'m>(m: mcx::Mcx<'m>, stmt: &PlannedStmt<'_>, id: i32) -> Option<mcx::PgVec<'m, (i16, Option<u32>)>> {
    let (_, n) = located(m, stmt, id)?;
    let quals = match n.node_tag() {
        NodeTag::T_IndexScan => &n.as_variant::<types_nodes::plannodes::IndexScan>()?.indexqual,
        NodeTag::T_IndexOnlyScan => &n.as_variant::<IndexOnlyScan>()?.indexqual,
        NodeTag::T_BitmapIndexScan => &n.as_variant::<types_nodes::plannodes::BitmapIndexScan>()?.indexqual,
        _ => return None,
    };
    let column = |e: Node<'_>| {
        let e = e.as_relabel_type().map_or(e, |r| r.arg);
        e.as_var().filter(|v| v.varno == types_nodes::primnodes::INDEX_VAR).map(|v| v.varattno)
    };
    let mut keys = mcx::PgVec::new_in(m);
    for e in quals.iter() {
        let key = match e.node_tag() {
            NodeTag::T_OpExpr => {
                let op = e.as_op_expr()?;
                (column(op.args.iter().next()?)?, Some(op.opno))
            }
            NodeTag::T_NullTest => (column(e.as_null_test()?.arg?)?, None),
            _ => return None,
        };
        mcx::GrowIn::push_in(&mut keys, key);
    }
    Some(keys)
}

/// The bound PostgreSQL 18.6's ExecSetTupleBound hands Sort node `id` (a Limit's count plus offset reaching it through
/// Results and SubqueryScans without a qual, as its recompute_limits evaluates them at the first call), under which its
/// tuplesort keeps a top-N heap once its input passes twice the bound. `Some(None)`: none reaches it (no Limit, an absent
/// or NULL count, WITH TIES); `None`: not known here (a count or offset that is neither a constant nor an extern
/// parameter `ext` reads: `ext(paramid)`, the parameter's integer value as this execution bound it, `None` when NULL or
/// unknown). D3433: a generic plan's `LIMIT $1` is such a parameter.
pub fn sort_bound(stmt: &PlannedStmt<'_>, id: i32, ext: &dyn Fn(i32) -> Option<i64>) -> Option<Option<i64>> {
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // the walk's path: gone with the call
    let (path, _) = located(cx.mcx(), stmt, id)?;
    for &(a, _) in path.iter().rev() {
        match a.node_tag() {
            // ExecSetTupleBound's pass-through nodes: a projecting Result (its constant qual affects nothing it passes),
            // every arm of an Append or a MergeAppend, a Gather's or a Gather Merge's child, a SubqueryScan without a qual
            NodeTag::T_Result | NodeTag::T_Append | NodeTag::T_MergeAppend | NodeTag::T_Gather | NodeTag::T_GatherMerge => {}
            NodeTag::T_SubqueryScan if a.as_plan()?.qual.is_nil() => {}
            NodeTag::T_Limit => {
                let l = a.as_limit()?;
                if l.limitOption == types_nodes::nodes_enums::LimitOption::LIMIT_OPTION_WITH_TIES {
                    return Some(None);
                }
                // a bound's value: `Some(None)` ALL, `None` unknown
                let value = |b: crate::graph::Bound<'_>| match b {
                    crate::graph::Bound::All => Some(None),
                    crate::graph::Bound::Const(v) => Some(Some(v)),
                    crate::graph::Bound::Expr(n) => n.as_param().filter(|p| p.paramkind == types_nodes::primnodes::ParamKind::PARAM_EXTERN).and_then(|p| ext(p.paramid)).map(Some),
                };
                let (count, offset) = crate::ops::limit_bounds(l);
                let off = value(offset)?.unwrap_or(0).max(0);
                return match value(count)? {
                    None => Some(None),
                    Some(c) if c >= 0 => Some(c.checked_add(off)),
                    Some(_) => None,
                };
            }
            _ => return Some(None),
        }
    }
    Some(None)
}

/// Plan node `id`'s input row (its lefttree's target list, junk entries too): the column types of the tuple a Sort, a Hash
/// or a WindowAgg keeps per row, and that row's planned width.
pub fn input_row<'m>(m: mcx::Mcx<'m>, stmt: &PlannedStmt<'_>, id: i32) -> Option<(mcx::PgVec<'m, u32>, i32)> {
    let (_, n) = located(m, stmt, id)?;
    let child = n.as_plan()?.lefttree?.as_plan()?;
    let types = mcx::CollectIn::collect_in(child.targetlist.iter().filter_map(|t| t.as_target_entry()).map(|te| nodes_core::expr_type(te.expr)), m);
    Some((types, child.plan_width))
}

/// CTE Scan node `id`'s source: its CTE's subplan root (the node whose rows fill the CTE's tuplestore, which every CTE
/// Scan of the CTE reads), with that root's column types and width (the tuple the tuplestore holds).
pub fn cte_source<'m>(m: mcx::Mcx<'m>, stmt: &PlannedStmt<'_>, id: i32) -> Option<(i32, mcx::PgVec<'m, u32>, i32)> {
    let c = located(m, stmt, id)?.1.as_variant::<types_nodes::plannodes::CteScan>()?;
    let i = (c.ctePlanId as usize).checked_sub(1).filter(|&i| i < stmt.subplans.len())?;
    let root = stmt.subplans.nth(i)?.as_plan()?;
    let types = mcx::CollectIn::collect_in(root.targetlist.iter().filter_map(|t| t.as_target_entry()).map(|te| nodes_core::expr_type(te.expr)), m);
    Some((root.plan_node_id, types, root.plan_width))
}

/// WindowAgg node `id`'s partitioning and frame: whether it partitions its rows (PARTITION BY) and whether its frame
/// starts at the partition's first row (UNBOUNDED PRECEDING: its buffer keeps the partition whole).
pub fn window_frame(stmt: &PlannedStmt<'_>, id: i32) -> Option<(bool, bool)> {
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // the walk's path: gone with the call
    let w = located(cx.mcx(), stmt, id)?.1.as_variant::<types_nodes::plannodes::WindowAgg>()?;
    Some((w.partNumCols > 0, w.frameOptions & types_nodes::rawnodes::FRAMEOPTION_START_UNBOUNDED_PRECEDING != 0))
}

/// Plan node `id`'s width (its `plan_width`).
pub fn plan_width(stmt: &PlannedStmt<'_>, id: i32) -> Option<i32> {
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // the walk's path: gone with the call
    let (_, n) = located(cx.mcx(), stmt, id)?;
    n.as_plan().map(|p| p.plan_width)
}

/// Hash node `id`'s estimate PostgreSQL sizes its table by (ExecHashTableCreate: the outer plan's rows, or `rows_total`
/// for a parallel-aware one), the outer plan's width, and whether it keeps a skew table.
pub fn hash_estimate(stmt: &PlannedStmt<'_>, id: i32) -> Option<(f64, i32, bool)> {
    let cx = mcx::MemoryContext::new_bump("GravityExplainScan"); // the walk's path: gone with the call
    let (_, n) = located(cx.mcx(), stmt, id)?;
    let h = n.as_variant::<types_nodes::plannodes::Hash>()?;
    let outer = h.plan.lefttree?.as_plan()?;
    let rows = if h.plan.parallel_aware && h.rows_total > 0.0 { h.rows_total } else { outer.plan_rows };
    Some((rows, outer.plan_width, h.skewTable != 0))
}

/// How a plan node's own expressions read an InitPlan's output parameter (`init_readers`). PostgreSQL runs an InitPlan at
/// the first evaluation of one of its parameters (ExecEvalParamExec -> ExecSetParamPlan), never before.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ParamReads {
    /// At the node's first run, whatever rows it then reads: a Result's one-time filter, an index scan's runtime keys
    /// (ExecIndexEvalRuntimeKeys), a Limit's count or offset (recompute_limits), an Append's or MergeAppend's exec-time
    /// pruning (ExecFindMatchingSubPlans), the parameters a Gather or Gather Merge hands its workers.
    pub start: bool,
    /// In a top-level conjunct of its qual, on a path no branch skips (operators, functions, casts): every row the node
    /// returns evaluated it; `qual_first`: the first conjunct (every row the qual tests evaluates it).
    pub qual: bool,
    pub qual_first: bool,
    /// In an output column on such a path: evaluated for each row the node returns, and for no other.
    pub out: bool,
    /// Anywhere else (a join qual, under OR / CASE / COALESCE, an aggregate's argument, a SubPlan, a field this module
    /// does not read): whether the node evaluated it is not known from its counts.
    pub other: bool,
}

/// Node `id` in the plan of an InitPlan (a SubPlan with output parameters in some plan node's `initPlan`): the InitPlan's
/// plan id and its plan's root node, and each plan node of the statement (its tree and its subplans) whose own expressions
/// read one of its parameters, with how. `None` when `id` is in no InitPlan's plan. A node whose `extParam` holds a
/// parameter that no field read here and no child accounts for reads it `other` (fail closed).
pub fn init_readers<'m>(m: mcx::Mcx<'m>, stmt: &PlannedStmt<'_>, id: i32) -> Option<(i32, i32, mcx::PgVec<'m, (i32, ParamReads)>)> {
    let roots = || stmt.planTree.into_iter().chain(stmt.subplans.iter().flatten());
    let mut inits: mcx::PgVec<'_, (i32, mcx::PgVec<'_, i32>)> = mcx::PgVec::new_in(m);
    roots().for_each(|r| init_plans(m, r, &mut inits));
    let holds = |plan_id: i32| {
        let mut ids = mcx::PgVec::new_in(m);
        if let Some(r) = subplan(stmt, plan_id) {
            under(r, &mut ids);
        }
        ids.contains(&id)
    };
    let (plan_id, params) = inits.into_iter().find(|(plan_id, _)| holds(*plan_id))?;
    let root = subplan(stmt, plan_id)?.as_plan()?.plan_node_id;
    let mut out = mcx::PgVec::new_in(m);
    roots().for_each(|r| readers(stmt, r, &params, &mut out));
    Some((plan_id, root, out))
}

/// Every InitPlan attached at or under `n` with output parameters: (its plan id, its `setParam`). A CTE's subplan is no
/// such InitPlan (its parameter only holds the CTE's shared state; its CTE Scans run it).
fn init_plans<'m>(m: mcx::Mcx<'m>, n: Node<'_>, out: &mut mcx::PgVec<'m, (i32, mcx::PgVec<'m, i32>)>) {
    crate::walk::stack_check(); // D3748
    let Some(p) = n.as_plan() else { return };
    let init = |s: &&types_nodes::primnodes::SubPlan<'_>| !s.setParam.is_nil() && s.subLinkType != types_nodes::primnodes::SubLinkType::CTE_SUBLINK;
    for s in p.initPlan.iter().filter_map(|s| s.as_sub_plan()).filter(init) {
        mcx::GrowIn::push_in(out, (s.plan_id, mcx::CollectIn::collect_in(s.setParam.iter(), m)));
    }
    children(n).for_each(|(c, _)| init_plans(m, c, out));
}

/// The nodes at or under `n` that read one of `params` (`init_readers`).
fn readers(stmt: &PlannedStmt<'_>, n: Node<'_>, params: &[i32], out: &mut mcx::PgVec<'_, (i32, ParamReads)>) {
    crate::walk::stack_check(); // D3748
    let Some(p) = n.as_plan() else { return };
    let r = own_reads(stmt, n, params);
    if r != ParamReads::default() {
        mcx::GrowIn::push_in(out, (p.plan_node_id, r));
    }
    children(n).for_each(|(c, _)| readers(stmt, c, params, out));
}

/// How plan node `n`'s own expressions read one of `params`.
fn own_reads(stmt: &PlannedStmt<'_>, n: Node<'_>, params: &[i32]) -> ParamReads {
    let Some(p) = n.as_plan() else { return ParamReads::default() };
    let mut r = ParamReads::default();
    let list = |l: &types_nodes::NodeList<'_>, r: &mut ParamReads| l.iter().for_each(|e| r.other |= reads(stmt, e, params).is_some());
    let start = |e: Option<Node<'_>>, r: &mut ParamReads| r.start |= e.is_some_and(|e| reads(stmt, e, params).is_some());
    for (i, e) in p.qual.iter().enumerate() {
        match reads(stmt, e, params) {
            Some(true) => {
                r.qual = true;
                r.qual_first |= i == 0;
            }
            Some(false) => r.other = true,
            None => {}
        }
    }
    for e in p.targetlist.iter() {
        match reads(stmt, e.as_target_entry().map_or(e, |t| t.expr), params) {
            Some(true) => r.out = true,
            Some(false) => r.other = true,
            None => {}
        }
    }
    let set = |b: &types_nodes::bitmapset::Bitmapset<'_>| params.iter().any(|&x| b.is_member(x));
    let known = match n.node_tag() {
        NodeTag::T_SeqScan | NodeTag::T_Material | NodeTag::T_Sort | NodeTag::T_IncrementalSort | NodeTag::T_Unique | NodeTag::T_Agg | NodeTag::T_SubqueryScan | NodeTag::T_CteScan => true,
        NodeTag::T_Result => {
            start(n.as_result().and_then(|x| x.resconstantqual), &mut r);
            true
        }
        NodeTag::T_IndexScan => n.as_index_scan().is_some_and(|x| {
            x.indexqual.iter().chain(x.indexqualorig.iter()).chain(x.indexorderby.iter()).chain(x.indexorderbyorig.iter()).for_each(|e| start(Some(e), &mut r));
            true
        }),
        NodeTag::T_IndexOnlyScan => n.as_index_only_scan().is_some_and(|x| {
            x.indexqual.iter().chain(x.indexorderby.iter()).for_each(|e| start(Some(e), &mut r));
            list(&x.recheckqual, &mut r);
            true
        }),
        NodeTag::T_BitmapIndexScan => n.as_bitmap_index_scan().is_some_and(|x| {
            x.indexqual.iter().chain(x.indexqualorig.iter()).for_each(|e| start(Some(e), &mut r));
            true
        }),
        NodeTag::T_BitmapHeapScan => n.as_bitmap_heap_scan().is_some_and(|x| {
            list(&x.bitmapqualorig, &mut r);
            true
        }),
        NodeTag::T_Limit => n.as_limit().is_some_and(|x| {
            start(x.limitOffset, &mut r);
            start(x.limitCount, &mut r);
            true
        }),
        NodeTag::T_Append | NodeTag::T_MergeAppend => {
            let index = n.as_append().map(|a| a.part_prune_index).or_else(|| n.as_merge_append().map(|a| a.part_prune_index)).unwrap_or(-1);
            let info = usize::try_from(index).ok().and_then(|i| stmt.partPruneInfos.as_slice().get(i).copied()).and_then(|i| i.as_partition_prune_info());
            r.start |= info.is_some_and(|i| i.prune_infos.iter().filter_map(|l| l.as_list()).flat_map(|l| l.iter()).any(|x| x.as_partitioned_rel_prune_info().is_none_or(|x| set(&x.execparamids))));
            true
        }
        NodeTag::T_Gather => n.as_gather().is_some_and(|x| {
            r.start |= set(&x.initParam);
            true
        }),
        NodeTag::T_GatherMerge => n.as_gather_merge().is_some_and(|x| {
            r.start |= set(&x.initParam);
            true
        }),
        NodeTag::T_NestLoop => n.as_nest_loop().is_some_and(|x| {
            list(&x.join.joinqual, &mut r);
            true
        }),
        _ => false,
    };
    // a parameter in the node's extParam that the fields read here and its children (and the InitPlans attached to it)
    // do not account for: read somewhere this module does not look
    let below = |x: i32| {
        children(n).any(|(c, _)| c.as_plan().is_some_and(|c| c.extParam.is_member(x)))
            || p.initPlan.iter().filter_map(|s| s.as_sub_plan()).any(|s| sub_reads(stmt, s, x))
    };
    if r == ParamReads::default() && params.iter().any(|&x| p.extParam.is_member(x) && (!known || !below(x))) {
        r.other = true;
    }
    r
}

/// The statement's subplan `plan_id` (1-based, as a SubPlan names it).
fn subplan<'a>(stmt: &PlannedStmt<'a>, plan_id: i32) -> Option<Node<'a>> {
    usize::try_from(plan_id).ok()?.checked_sub(1).and_then(|i| stmt.subplans.as_slice().get(i).copied().flatten())
}

/// Whether SubPlan `s`'s plan reads parameter `x` from outside it (its plan's `extParam`, less the ones it is passed).
fn sub_reads(stmt: &PlannedStmt<'_>, s: &types_nodes::primnodes::SubPlan<'_>, x: i32) -> bool {
    let plan = subplan(stmt, s.plan_id).and_then(|r| r.as_plan());
    plan.is_some_and(|p| p.extParam.is_member(x)) && !s.parParam.iter().any(|y| y == x)
}

/// Whether expression `e` reads one of `params` (PARAM_EXEC): `Some(true)` on a path every evaluation of `e` takes
/// (operators, functions, casts, NULL and boolean tests), `Some(false)` only elsewhere (under a BoolExpr, CASE or
/// COALESCE, an aggregate's arguments, a SubPlan), `None` nowhere. A walk that fails reads `Some(false)` (fail closed).
fn reads(stmt: &PlannedStmt<'_>, e: Node<'_>, params: &[i32]) -> Option<bool> {
    if certain(e, params) {
        return Some(true);
    }
    struct Find<'a, 's, 'p> {
        stmt: &'s PlannedStmt<'p>,
        params: &'a [i32],
    }
    impl<'m> nodes_core::NodeWalker<'m> for Find<'_, '_, '_> {
        fn visit(&mut self, n: Node<'m>) -> types_error::PgResult<bool> {
            if n.as_param().is_some_and(|p| p.paramkind == types_nodes::primnodes::ParamKind::PARAM_EXEC && self.params.contains(&p.paramid)) {
                return Ok(true);
            }
            if let Some(s) = n.as_sub_plan() {
                if self.params.iter().any(|&x| sub_reads(self.stmt, s, x)) {
                    return Ok(true);
                }
            }
            nodes_core::expression_tree_walker(n, self)
        }
    }
    nodes_core::NodeWalker::visit(&mut Find { stmt, params }, e).unwrap_or(true).then_some(false)
}

/// `reads`' sure path: `e` is one of `params`, or an operator, function, cast or test whose argument is.
fn certain(e: Node<'_>, params: &[i32]) -> bool {
    crate::walk::stack_check(); // D3748
    let any = |l: &types_nodes::NodeList<'_>| l.iter().any(|a| certain(a, params));
    if let Some(p) = e.as_param() {
        return p.paramkind == types_nodes::primnodes::ParamKind::PARAM_EXEC && params.contains(&p.paramid);
    }
    match e.node_tag() {
        NodeTag::T_OpExpr => e.as_op_expr().is_some_and(|o| any(&o.args)),
        NodeTag::T_DistinctExpr => e.as_distinct_expr().is_some_and(|o| any(&o.args)),
        NodeTag::T_NullIfExpr => e.as_null_if_expr().is_some_and(|o| any(&o.args)),
        NodeTag::T_ScalarArrayOpExpr => e.as_scalar_array_op_expr().is_some_and(|o| any(&o.args)),
        NodeTag::T_FuncExpr => e.as_func_expr().is_some_and(|f| any(&f.args)),
        NodeTag::T_RelabelType => e.as_relabel_type().is_some_and(|r| certain(r.arg, params)),
        NodeTag::T_CoerceViaIO => e.as_coerce_via_io().is_some_and(|c| certain(c.arg, params)),
        NodeTag::T_NullTest => e.as_null_test().and_then(|t| t.arg).is_some_and(|a| certain(a, params)),
        NodeTag::T_BooleanTest => e.as_boolean_test().and_then(|t| t.arg).is_some_and(|a| certain(a, params)),
        _ => false,
    }
}

/// Every Index Only Scan at or under `n`: (node id, scanrelid, index).
fn index_only(n: Node<'_>, out: &mut mcx::PgVec<'_, (i32, u32, u32)>) {
    crate::walk::stack_check(); // D3748
    let Some(p) = n.as_plan() else { return };
    if let Some(s) = n.as_variant::<IndexOnlyScan>() {
        mcx::GrowIn::push_in(out, (p.plan_node_id, s.scan.scanrelid, s.indexid));
    }
    children(n).for_each(|(c, _)| index_only(c, out));
}

/// Whether `id` is `n` or under it; `path` gets every plan node on the way down (the node, its inner side taken) and
/// `found` the node itself.
fn find<'a>(n: Node<'a>, id: i32, path: &mut mcx::PgVec<'_, (Node<'a>, bool)>, found: &mut Option<Node<'a>>) -> bool {
    crate::walk::stack_check(); // D3748
    let Some(p) = n.as_plan() else { return false };
    if p.plan_node_id == id {
        *found = Some(n);
        return true;
    }
    for (c, inner) in children(n) {
        mcx::GrowIn::push_in(path, (n, inner));
        if find(c, id, path, found) {
            return true;
        }
        path.pop();
    }
    false
}

/// Plan node `n`'s children (each with whether it is the inner side): lefttree, righttree, then an Append's or
/// MergeAppend's arms, a SubqueryScan's subplan, a BitmapAnd's or BitmapOr's inputs.
pub(crate) fn children<'a>(n: Node<'a>) -> impl Iterator<Item = (Node<'a>, bool)> {
    let p = n.as_plan();
    let list = match n.node_tag() {
        NodeTag::T_Append => n.as_variant::<Append>().map(|a| &a.appendplans),
        NodeTag::T_MergeAppend => n.as_variant::<MergeAppend>().map(|a| &a.mergeplans),
        NodeTag::T_BitmapAnd => n.as_variant::<BitmapAnd>().map(|a| &a.bitmapplans),
        NodeTag::T_BitmapOr => n.as_variant::<BitmapOr>().map(|a| &a.bitmapplans),
        _ => None,
    };
    let sub = if n.node_tag() == NodeTag::T_SubqueryScan { n.as_variant::<SubqueryScan>().and_then(|q| q.subplan) } else { None };
    let tree = p.into_iter().flat_map(|p| [(p.lefttree, false), (p.righttree, true)]).filter_map(|(c, inner)| c.map(|c| (c, inner)));
    tree.chain(list.into_iter().flat_map(|l| l.iter()).map(|c| (c, false))).chain(sub.map(|c| (c, false)))
}

/// `DECISIONS.md` 4 (D) b (the run-ahead tag, program `AheadTag`): plan node `id`'s index in EXPLAIN's node order (0: the
/// node EXPLAIN prints first) and EXPLAIN's Node Type for it. The order is ExplainNode's walk (explain.c): at each node,
/// the node, its InitPlans, its outer, its inner, an Append's or MergeAppend's members, a BitmapAnd's or BitmapOr's
/// inputs, a Subquery Scan's subplan, then the SubPlans its own expressions call (`expr_subplans`); each subplan once
/// (`printed_subplans`); an invisible Gather on top is not printed (ExplainPrintPlan). `None` when `id` is not in that
/// walk. Members that initial pruning removes still count here, where EXPLAIN prints them as Subplans Removed: the
/// harness's Node Type check then fails such a tag closed.
pub fn explain_position(stmt: &PlannedStmt<'_>, id: i32) -> Option<(u32, &'static str)> {
    let mut top = stmt.planTree?;
    if top.as_gather().is_some_and(|g| g.invisible) {
        top = top.as_plan()?.lefttree?;
    }
    ExplainOrder { stmt, id, at: 0, printed: Vec::new() }.node(top) // alloc: Cold
}

/// `explain_position`'s walk: `at` counts the nodes printed before the one met, `printed` the subplans walked.
struct ExplainOrder<'s, 'a> {
    stmt: &'s PlannedStmt<'a>,
    id: i32,
    at: u32,
    printed: Vec<i32>,
}

impl<'a> ExplainOrder<'_, 'a> {
    fn node(&mut self, n: Node<'a>) -> Option<(u32, &'static str)> {
        crate::walk::stack_check(); // D3748
        let p = n.as_plan()?;
        if p.plan_node_id == self.id {
            return Some((self.at, node_type(n)));
        }
        self.at += 1;
        for s in p.initPlan.iter().filter_map(|s| s.as_sub_plan()) {
            if let Some(f) = self.sub(s.plan_id) {
                return Some(f);
            }
        }
        let members = [n.as_append().map(|a| &a.appendplans), n.as_merge_append().map(|a| &a.mergeplans), n.as_bitmap_and().map(|a| &a.bitmapplans), n.as_bitmap_or().map(|a| &a.bitmapplans)];
        let kids = p.lefttree.into_iter().chain(p.righttree).chain(members.into_iter().flatten().flat_map(|l| l.iter())).chain(n.as_subquery_scan().and_then(|s| s.subplan));
        for c in kids {
            if let Some(f) = self.node(c) {
                return Some(f);
            }
        }
        if self.stmt.subplans.is_nil() {
            return None;
        }
        for plan_id in expr_subplans(n) {
            if let Some(f) = self.sub(plan_id) {
                return Some(f);
            }
        }
        None
    }

    fn sub(&mut self, plan_id: i32) -> Option<(u32, &'static str)> {
        if self.printed.contains(&plan_id) {
            return None;
        }
        self.printed.push(plan_id); // alloc: Cold
        self.node(subplan(self.stmt, plan_id)?)
    }
}

/// EXPLAIN's Node Type (FORMAT JSON) of plan node `n` (explain.c ExplainNode's `sname`).
fn node_type(n: Node<'_>) -> &'static str {
    match n.node_tag() {
        NodeTag::T_Result => "Result",
        NodeTag::T_ProjectSet => "ProjectSet",
        NodeTag::T_ModifyTable => "ModifyTable",
        NodeTag::T_Append => "Append",
        NodeTag::T_MergeAppend => "Merge Append",
        NodeTag::T_RecursiveUnion => "Recursive Union",
        NodeTag::T_BitmapAnd => "BitmapAnd",
        NodeTag::T_BitmapOr => "BitmapOr",
        NodeTag::T_NestLoop => "Nested Loop",
        NodeTag::T_MergeJoin => "Merge Join",
        NodeTag::T_HashJoin => "Hash Join",
        NodeTag::T_SeqScan => "Seq Scan",
        NodeTag::T_SampleScan => "Sample Scan",
        NodeTag::T_Gather => "Gather",
        NodeTag::T_GatherMerge => "Gather Merge",
        NodeTag::T_IndexScan => "Index Scan",
        NodeTag::T_IndexOnlyScan => "Index Only Scan",
        NodeTag::T_BitmapIndexScan => "Bitmap Index Scan",
        NodeTag::T_BitmapHeapScan => "Bitmap Heap Scan",
        NodeTag::T_TidScan => "Tid Scan",
        NodeTag::T_TidRangeScan => "Tid Range Scan",
        NodeTag::T_SubqueryScan => "Subquery Scan",
        NodeTag::T_FunctionScan => "Function Scan",
        NodeTag::T_TableFuncScan => "Table Function Scan",
        NodeTag::T_ValuesScan => "Values Scan",
        NodeTag::T_CteScan => "CTE Scan",
        NodeTag::T_NamedTuplestoreScan => "Named Tuplestore Scan",
        NodeTag::T_WorkTableScan => "WorkTable Scan",
        NodeTag::T_ForeignScan => "Foreign Scan",
        NodeTag::T_CustomScan => "Custom Scan",
        NodeTag::T_Material => "Materialize",
        NodeTag::T_Memoize => "Memoize",
        NodeTag::T_Sort => "Sort",
        NodeTag::T_IncrementalSort => "Incremental Sort",
        NodeTag::T_Group => "Group",
        NodeTag::T_Agg => "Aggregate",
        NodeTag::T_WindowAgg => "WindowAgg",
        NodeTag::T_Unique => "Unique",
        NodeTag::T_SetOp => "SetOp",
        NodeTag::T_LockRows => "LockRows",
        NodeTag::T_Limit => "Limit",
        NodeTag::T_Hash => "Hash",
        _ => "???",
    }
}

/// The SubPlans plan node `n`'s own expressions call, by plan id, in the order ExecInit<Node> initializes them (its
/// planstate's `subPlan`): the node's lists in its ExecInit order, a SubPlan after the ones its arguments and its test
/// call (ExecInitSubPlanExpr), an Aggregate's inputs after its target list and qual (ExecInitAgg), none for an index
/// qual's runtime keys (plain EXPLAIN stops before ExecIndexBuildScanKeys) or a pruning step.
fn expr_subplans(n: Node<'_>) -> Vec<i32> {
    use types_nodes::NodeList;
    let Some(p) = n.as_plan() else { return Vec::new() }; // alloc: Cold
    let mut w = SubPlanOrder { ids: Vec::new(), aggrefs: Vec::new() }; // alloc: Cold
    let (tl, q) = (Some(&p.targetlist), Some(&p.qual));
    let lists: [Option<&NodeList<'_>>; 5] = match n.node_tag() {
        NodeTag::T_NestLoop => [q, n.as_nest_loop().map(|j| &j.join.joinqual), tl, None, None],
        NodeTag::T_MergeJoin => [q, n.as_merge_join().map(|j| &j.join.joinqual), tl, None, None],
        NodeTag::T_HashJoin => n.as_hash_join().map_or([None; 5], |j| [tl, Some(&j.hashkeys), q, Some(&j.join.joinqual), Some(&j.hashclauses)]),
        NodeTag::T_Hash => [n.as_hash().map(|h| &h.hashkeys), None, None, None, None],
        NodeTag::T_IndexScan => n.as_index_scan().map_or([None; 5], |s| [tl, q, Some(&s.indexqualorig), Some(&s.indexorderbyorig), None]),
        NodeTag::T_IndexOnlyScan => [tl, q, n.as_index_only_scan().map(|s| &s.recheckqual), None, None],
        NodeTag::T_BitmapHeapScan => [tl, q, n.as_bitmap_heap_scan().map(|s| &s.bitmapqualorig), None, None],
        NodeTag::T_TidScan => [tl, q, n.as_tid_scan().map(|s| &s.tidquals), None, None],
        NodeTag::T_TidRangeScan => [tl, q, n.as_tid_range_scan().map(|s| &s.tidrangequals), None, None],
        NodeTag::T_BitmapIndexScan | NodeTag::T_Append | NodeTag::T_MergeAppend => [None; 5],
        _ => [tl, q, None, None, None],
    };
    // ExecInitFunctionScan builds each function's call before the projection and the qual
    for f in n.as_function_scan().into_iter().flat_map(|s| s.functions.iter()).filter_map(|f| f.as_range_tbl_function()) {
        w.opt(f.funcexpr);
    }
    lists.into_iter().flatten().for_each(|l| w.list(l));
    if let Some(r) = n.as_result() {
        w.opt(r.resconstantqual);
    }
    for row in n.as_values_scan().into_iter().flat_map(|v| v.values_lists.iter()) {
        w.opt(Some(row));
    }
    let aggs = core::mem::take(&mut w.aggrefs);
    if n.node_tag() == NodeTag::T_Agg {
        // ExecInitAgg: each aggregate's direct arguments once, then per transition its filter and its inputs
        for (i, a) in aggs.iter().enumerate() {
            if !aggs[..i].iter().any(|b| b.aggno == a.aggno) {
                w.list(&a.aggdirectargs);
            }
        }
        for t in 0..aggs.iter().map(|a| a.aggtransno + 1).max().unwrap_or(0) {
            if let Some(a) = aggs.iter().find(|a| a.aggtransno == t) {
                w.opt(a.aggfilter);
                w.list(&a.args);
            }
        }
    }
    w.ids
}

/// `expr_subplans`' walk: the SubPlans met, and the Aggrefs (ExecInitExprRec never descends into one).
struct SubPlanOrder<'a> {
    ids: Vec<i32>,
    aggrefs: Vec<&'a types_nodes::primnodes::Aggref<'a>>,
}

impl<'a> SubPlanOrder<'a> {
    fn list(&mut self, l: &types_nodes::NodeList<'a>) {
        let _ = nodes_core::walk_list(l, self);
    }

    fn opt(&mut self, n: Option<Node<'a>>) {
        let _ = nodes_core::walk_opt(n, self);
    }
}

impl<'a> nodes_core::NodeWalker<'a> for SubPlanOrder<'a> {
    fn visit(&mut self, n: Node<'a>) -> types_error::PgResult<bool> {
        if let Some(s) = n.as_sub_plan() {
            nodes_core::walk_list(&s.args, self)?;
            nodes_core::walk_opt(s.testexpr, self)?;
            self.ids.push(s.plan_id); // alloc: Cold
            return Ok(false);
        }
        if let Some(a) = n.as_aggref() {
            self.aggrefs.push(a); // alloc: Cold
            return Ok(false);
        }
        if n.node_tag() == NodeTag::T_GroupingFunc {
            return Ok(false);
        }
        nodes_core::expression_tree_walker(n, self)
    }
}
