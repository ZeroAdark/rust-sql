//! The EXECUTE gates ExecutorStart runs over a plan, read from the plan alone (`plan_gates`): an EXPLAIN without ANALYZE
//! of a plan with no admitted program (a refused plan, EXPLAIN (GENERIC_PLAN), a parameter list that defers its slots)
//! checks them as standard_ExecutorStart does under EXEC_FLAG_EXPLAIN_ONLY (entry explain.rs). C gates a function when it
//! builds an ExprState: ExecInitFunc and its twins over the expressions each node's init compiles, ExecInitAgg's
//! aggregate gate, ExecBuildGroupingEqual / ExecBuildParamSetEqual over a grouping node's equality operators. The same
//! inits look up comparison procs and raise where a family lacks one (`init_lookups`, every EXPLAIN: a merge clause's, a
//! MergeAppend's or Gather Merge's sort key, a RowCompareExpr column, a MinMaxExpr, a hash join's hash functions; and
//! the admission of a hash join refused for an unknown hash function, D2036). Also the command id a write's EXPLAIN
//! marks used (`mark_output_cid`).

use memledger::{ChargedVec, Kind};
use types_core::Oid;
use types_error::PgResult;
use types_nodes::list::NodeList;
use types_nodes::nodes_enums::CmdType;
use types_nodes::plannodes::PlannedStmt;
use types_nodes::{Node, NodeTag};

use crate::ops::append::prune::Initial;
use crate::shapes::AclObject;

/// ExecutorStart's `es_output_cid` under EXEC_FLAG_EXPLAIN_ONLY too: a write, a row lock or a data-modifying CTE marks the
/// current command id used (`GetCurrentCommandId(true)`), so the CommandCounterIncrement after the EXPLAIN advances it
/// and a later statement of the transaction shows it (its cmin / cmax).
pub fn mark_output_cid(pstmt: &PlannedStmt<'_>) -> PgResult<()> {
    if pstmt.commandType != CmdType::CMD_SELECT || pstmt.rowMarks.iter().next().is_some() || pstmt.hasModifyingCTE {
        xact::GetCurrentCommandId(true)?;
    }
    Ok(())
}

/// The plan children ExecInitNode initializes under `n`: outer, inner, an Append's or MergeAppend's arms (those initial
/// pruning kept, per `prune`), a BitmapAnd's or BitmapOr's, a SubqueryScan's subplan.
fn each_child<'p>(n: Node<'p>, prune: Option<&Initial>, mut f: impl FnMut(Node<'p>)) {
    let Some(p) = n.as_plan() else { return };
    for c in [p.lefttree, p.righttree].into_iter().flatten() {
        f(c);
    }
    let arms = n.as_append().map(|a| (&a.appendplans, a.part_prune_index)).or(n.as_merge_append().map(|m| (&m.mergeplans, m.part_prune_index)));
    if let Some((arms, index)) = arms {
        let kept = prune.and_then(|p| p.valid.get(usize::try_from(index).ok()?).cloned().flatten());
        for (i, c) in arms.iter().enumerate() {
            if kept.as_ref().map_or(true, |k| k.contains(&(i as i32))) {
                f(c);
            }
        }
    }
    let bitmaps = n.as_bitmap_and().map(|a| &a.bitmapplans).or(n.as_bitmap_or().map(|o| &o.bitmapplans));
    for c in bitmaps.into_iter().flat_map(|l| l.iter()) {
        f(c);
    }
    for c in n.as_subquery_scan().and_then(|s| s.subplan) {
        f(c);
    }
}

/// The EXECUTE gates ExecutorStart runs over a plan with no admitted program (a refused plan, a GENERIC_PLAN's), from
/// the plan alone, in InitPlan's order: each subplan in list order, then the plan tree, a node's children before its own
/// expressions; then the equality operators the grouping nodes and DISTINCT aggregates compile (ExecBuildGroupingEqual /
/// ExecBuildParamSetEqual: `node_gate_ops`, `distinct_eq_ops`). Where C compiles a node's expressions: `Gates::node`.
pub fn plan_gates(pstmt: &PlannedStmt<'_>, prune: Option<&Initial>) -> PgResult<()> {
    let owner = gravity_driver::stmt_context().map(|(o, _)| o);
    let mut g = Gates { gates: true, list: ChargedVec::new(owner, Kind::Region), ..Gates::default() }; // alloc: Exec charged owner
    for t in pstmt.subplans.iter().flatten().chain(pstmt.planTree) {
        g.node(t, prune);
    }
    let mut ops = core::mem::take(&mut g.eq_ops);
    gravity_decompose::admit::node_gate_ops(pstmt, &mut ops);
    for op in ops {
        g.push(Check::Gate(lsyscache::get_opcode(op)?, AclObject::Function));
    }
    g.run()
}

/// The comparison-proc lookups of ExecutorStart's inits alone (an admitted program's EXPLAIN, whose EXECUTE gates the
/// program files): `plan_gates`' walk without its gates.
pub fn init_lookups(pstmt: &PlannedStmt<'_>, prune: Option<&Initial>) -> PgResult<()> {
    let owner = gravity_driver::stmt_context().map(|(o, _)| o);
    let mut g = Gates { list: ChargedVec::new(owner, Kind::Region), ..Gates::default() }; // alloc: Exec charged owner
    for t in pstmt.subplans.iter().flatten().chain(pstmt.planTree) {
        g.node(t, prune);
    }
    g.run()
}

/// One check of a node's init, in the walk's order.
#[derive(Clone, Copy, PartialEq)]
enum Check {
    /// An EXECUTE gate (ExecInitFunc, nodeAgg's aggregate gate).
    Gate(Oid, AclObject),
    /// MJExamineQuals' clause: operator, family.
    MergeClause(Oid, Oid),
    /// PrepareSortSupportFromOrderingOp's sort key operator.
    SortKey(Oid),
    /// A RowCompareExpr column: operator, family.
    RowCompare(Oid, Oid),
    /// A MinMaxExpr's type.
    MinMax(Oid),
    /// ExecInitHashJoin's hash function lookup for one hash operator.
    HashOp(Oid),
}

#[derive(Default)]
struct Gates {
    /// The EXECUTE gates too (`plan_gates`), not the lookups alone (`init_lookups`).
    gates: bool,
    list: ChargedVec<Check>,
    eq_ops: Vec<Oid>,
}

impl Gates {
    fn push(&mut self, c: Check) {
        let skip = match c {
            Check::Gate(f, _) => f == 0 || !self.gates,
            _ => false,
        };
        if skip || self.list.contains(&c) {
            return;
        }
        self.list.with_vec(|v| v.push(c)); // alloc: Cold charged self.list.owner()
    }

    fn run(&self) -> PgResult<()> {
        for &c in self.list.iter() {
            match c {
                Check::Gate(f, AclObject::Aggregate) => gravity_expr::nodes::func::aggregate_gate(f)?,
                Check::Gate(f, AclObject::Function) => gravity_expr::nodes::func::execute_gate(f)?,
                Check::MergeClause(op, family) => gravity_fn::typesem::expr::init_merge_clause(op, family)?,
                Check::SortKey(op) => gravity_fn::typesem::expr::init_ordering_op(op)?,
                Check::RowCompare(op, family) => gravity_fn::typesem::expr::init_row_compare(op, family)?,
                Check::MinMax(t) => gravity_fn::typesem::expr::init_min_max(t)?,
                Check::HashOp(op) => gravity_fn::typesem::expr::init_hash_op(op)?,
            }
        }
        Ok(())
    }

    /// The gates of one compiled expression: an operator's, a function's, a scalar-array operator's (and its hash
    /// function's), an aggregate's (OBJECT_AGGREGATE, nodeAgg.c), a window function's (execExpr.c, nodeWindowAgg.c); and
    /// the comparison procs a RowCompareExpr or a MinMaxExpr looks up.
    fn expr(&mut self, e: Node<'_>) {
        gravity_decompose::expr::walk(e, &mut |n| {
            let gate = |f: Oid| Check::Gate(f, AclObject::Function);
            match n.node_tag() {
                NodeTag::T_OpExpr => self.push(gate(n.as_op_expr().unwrap().opfuncid)),
                NodeTag::T_DistinctExpr => self.push(gate(n.as_distinct_expr().unwrap().opfuncid)),
                NodeTag::T_NullIfExpr => self.push(gate(n.as_null_if_expr().unwrap().opfuncid)),
                NodeTag::T_FuncExpr => self.push(gate(n.as_func_expr().unwrap().funcid)),
                NodeTag::T_ScalarArrayOpExpr => {
                    let s = n.as_scalar_array_op_expr().unwrap();
                    self.push(gate(s.opfuncid));
                    self.push(gate(s.hashfuncid));
                }
                NodeTag::T_Aggref => {
                    self.push(Check::Gate(n.as_aggref().unwrap().aggfnoid, AclObject::Aggregate));
                    gravity_decompose::admit::distinct_eq_ops(n, &mut self.eq_ops);
                }
                NodeTag::T_WindowFunc => self.push(gate(n.as_window_func().unwrap().winfnoid)),
                NodeTag::T_RowCompareExpr => {
                    let r = n.as_row_compare_expr().unwrap();
                    for c in r.opnos.iter().zip(r.opfamilies.iter()).map(|(op, family)| Check::RowCompare(op, family)) {
                        self.push(c);
                    }
                }
                NodeTag::T_MinMaxExpr => self.push(Check::MinMax(n.as_min_max_expr().unwrap().minmaxtype)),
                _ => {}
            }
            true
        });
    }

    fn exprs(&mut self, l: &NodeList<'_>) {
        for e in l.iter() {
            self.expr(e);
        }
    }

    fn opt(&mut self, e: Option<Node<'_>>) {
        if let Some(e) = e {
            self.expr(e);
        }
    }

    /// PrepareSortSupportFromOrderingOp per sort key (ExecInitMergeAppend, ExecInitGatherMerge).
    fn sort_keys(&mut self, ops: &[Oid]) {
        for &op in ops {
            self.push(Check::SortKey(op));
        }
    }

    /// Each element of a list of lists (a ModifyTable's per-relation lists).
    fn lists(&mut self, l: &NodeList<'_>, mut each: impl FnMut(&mut Self, Node<'_>)) {
        for e in l.iter().filter_map(|x| x.as_list()).flat_map(|x| x.iter()) {
            each(self, e);
        }
    }

    /// A clause of which C compiles the arguments only: a merge clause (MJExamineQuals), a TID qual (TidExprListCreate,
    /// MakeTidOpExprState).
    fn args_of(&mut self, l: &NodeList<'_>) {
        for c in l.iter() {
            if let Some(o) = c.as_op_expr() {
                self.exprs(&o.args);
            } else if let Some(s) = c.as_scalar_array_op_expr() {
                self.exprs(&s.args);
            }
        }
    }

    /// ExecInitNode over `n`: its children, then what its init compiles beside its targetlist and qual (an index scan's
    /// original quals, not the index-key forms it builds only past its EXPLAIN_ONLY return; a VALUES row only when it holds
    /// a SubPlan, ValuesNext compiling the rest at each row's visit).
    fn node(&mut self, n: Node<'_>, prune: Option<&Initial>) {
        stack_depth_core::check_stack_depth_or_panic();
        let Some(p) = n.as_plan() else { return };
        each_child(n, prune, |c| self.node(c, prune));
        self.exprs(&p.targetlist);
        self.exprs(&p.qual);
        self.exprs(&p.initPlan);
        match n.node_tag() {
            NodeTag::T_Result => self.opt(n.as_result().unwrap().resconstantqual),
            NodeTag::T_SampleScan => {
                if let Some(t) = n.as_sample_scan().unwrap().tablesample.and_then(|t| t.as_table_sample_clause()) {
                    self.exprs(&t.args);
                    self.opt(t.repeatable);
                }
            }
            NodeTag::T_TidScan => self.args_of(&n.as_tid_scan().unwrap().tidquals),
            NodeTag::T_TidRangeScan => self.args_of(&n.as_tid_range_scan().unwrap().tidrangequals),
            NodeTag::T_IndexScan => {
                let s = n.as_index_scan().unwrap();
                self.exprs(&s.indexqualorig);
                self.exprs(&s.indexorderbyorig);
            }
            NodeTag::T_IndexOnlyScan => self.exprs(&n.as_index_only_scan().unwrap().recheckqual),
            NodeTag::T_BitmapHeapScan => self.exprs(&n.as_bitmap_heap_scan().unwrap().bitmapqualorig),
            NodeTag::T_FunctionScan => {
                for f in n.as_function_scan().unwrap().functions.iter().filter_map(|f| f.as_range_tbl_function()) {
                    self.opt(f.funcexpr);
                }
            }
            NodeTag::T_TableFuncScan => {
                if let Some(t) = n.as_table_func_scan().unwrap().tablefunc.and_then(|t| t.as_table_func()) {
                    self.exprs(&t.ns_uris);
                    self.opt(t.docexpr);
                    self.opt(t.rowexpr);
                    for e in t.colexprs.iter().chain(t.coldefexprs.iter()).chain(t.colvalexprs.iter()).flatten() {
                        self.expr(e);
                    }
                    self.exprs(&t.passingvalexprs);
                }
            }
            NodeTag::T_ValuesScan => {
                for row in n.as_values_scan().unwrap().values_lists.iter().filter_map(|r| r.as_list()) {
                    if row.iter().any(gravity_decompose::expr::has_subplan) {
                        self.exprs(row);
                    }
                }
            }
            NodeTag::T_NestLoop => self.exprs(&n.as_nest_loop().unwrap().join.joinqual),
            NodeTag::T_MergeJoin => {
                let j = n.as_merge_join().unwrap();
                self.exprs(&j.join.joinqual);
                self.args_of(&j.mergeclauses);
                for (c, &family) in j.mergeclauses.iter().zip(j.mergeFamilies) {
                    if let Some(o) = c.as_op_expr() {
                        self.push(Check::MergeClause(o.opno, family));
                    }
                }
            }
            NodeTag::T_MergeAppend => self.sort_keys(n.as_merge_append().unwrap().sortOperators),
            NodeTag::T_GatherMerge => self.sort_keys(n.as_gather_merge().unwrap().sortOperators),
            NodeTag::T_HashJoin => {
                let j = n.as_hash_join().unwrap();
                // its hash functions are looked up before its hash keys and clauses compile
                for op in j.hashoperators.iter() {
                    self.push(Check::HashOp(op));
                }
                self.exprs(&j.hashkeys);
                self.exprs(&j.join.joinqual);
                self.exprs(&j.hashclauses);
            }
            NodeTag::T_Hash => self.exprs(&n.as_hash().unwrap().hashkeys),
            NodeTag::T_Memoize => self.exprs(&n.as_memoize().unwrap().param_exprs),
            NodeTag::T_WindowAgg => {
                let w = n.as_window_agg().unwrap();
                self.exprs(&w.runCondition);
                self.opt(w.startOffset);
                self.opt(w.endOffset);
            }
            NodeTag::T_Limit => {
                let l = n.as_limit().unwrap();
                self.opt(l.limitOffset);
                self.opt(l.limitCount);
            }
            NodeTag::T_ModifyTable => {
                let m = n.as_modify_table().unwrap();
                self.lists(&m.mergeJoinConditions, |g, e| g.expr(e));
                self.lists(&m.mergeActionLists, |g, e| {
                    if let Some(a) = e.as_merge_action() {
                        g.opt(a.qual);
                        g.exprs(&a.targetList);
                    }
                });
                self.lists(&m.withCheckOptionLists, |g, e| g.opt(e.as_with_check_option().and_then(|w| w.qual)));
                self.lists(&m.returningLists, |g, e| g.expr(e));
                self.exprs(&m.onConflictSet);
                self.opt(m.onConflictWhere);
            }
            _ => {}
        }
    }
}
