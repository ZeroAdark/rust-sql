//! Where PostgreSQL runs a statement's EXECUTE gates, for the per-execution gate list `program_funcs` (mod.rs) files with a
//! program (D2771, D2775, D2785, D2786, D2787). C gates a function when it builds an ExprState: ExecInitFunc and its twins
//! over the plan's expressions, all at ExecutorStart, plus gates that are no expression's own: ExecBuildParamSetEqual over a
//! Memoize's hash operators and ExecBuildGroupingEqual over a grouping node's equality operators (the plan reader's
//! `gravity_decompose::admit::node_gate_ops` / `distinct_eq_ops` name the operators; `op_gates` files them). Some
//! expressions of the plan are compiled elsewhere or never (`Sites`): a merge clause and a TID qual have only their
//! arguments compiled (MJExamineQuals, TidExprListCreate / MakeTidOpExprState), and a VALUES row that holds no SubPlan is
//! compiled at the row's visit (nodeValuesscan.c ValuesNext), so a row the scan never reaches is never gated.
use std::collections::HashSet;

use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::subplan::{SubForm, SubPlanDesc};
use gravity_decompose::{Op, PipelineGraph, PipelineNode, Source};
use types_core::Oid;
use types_error::PgResult;
use types_nodes::{Node, NodeTag};


/// Where C runs an expression node's own gate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum At {
    /// ExecutorStart: every expression a plan node's init compiles.
    Start,
    /// The visit of the VALUES row holding it: the program's own gate at the row's turn (D2775, lowerv4 ops/values).
    Visit,
    /// Never: a merge clause's or a TID qual's own operator.
    Never,
}

/// The graph's expression nodes whose own gate is not ExecutorStart's, by arena address (a plan node is one allocation:
/// the key's traversal hands the same addresses).
#[derive(Default)]
pub(super) struct Sites {
    never: HashSet<usize>, // alloc: Program
    visit: HashSet<usize>, // alloc: Program
}

fn addr(n: Node<'_>) -> usize {
    n.as_raw().as_ptr() as usize
}

/// D2915: the expression nodes whose own gate the gate list files (`program_funcs`): an operator's, a function's, an
/// aggregate's or a window function's call; no other node is asked where its gate runs.
pub(super) fn gated(tag: NodeTag) -> bool {
    matches!(tag, NodeTag::T_OpExpr | NodeTag::T_DistinctExpr | NodeTag::T_NullIfExpr | NodeTag::T_FuncExpr | NodeTag::T_ScalarArrayOpExpr | NodeTag::T_Aggref | NodeTag::T_WindowFunc)
}

impl Sites {
    pub(super) fn of(graph: &PipelineGraph<'_>) -> Sites {
        let mut s = Sites::default();
        for p in graph.pipelines.iter() {
            s.pipeline(graph, p);
        }
        s
    }

    pub(super) fn at(&self, n: Node<'_>) -> At {
        if self.never.is_empty() && self.visit.is_empty() {
            return At::Start;
        }
        let a = addr(n);
        if self.never.contains(&a) {
            At::Never
        } else if self.visit.contains(&a) {
            At::Visit
        } else {
            At::Start
        }
    }

    fn pipeline(&mut self, graph: &PipelineGraph<'_>, p: &PipelineNode<'_>) {
        stack_depth_core::check_stack_depth_or_panic();
        match &p.source {
            // TidExprListCreate / MakeTidOpExprState compile only the TID value's side of each clause
            Source::Tid { quals, .. } => {
                let ops = quals.iter().filter(|q| matches!(q.node_tag(), NodeTag::T_OpExpr | NodeTag::T_ScalarArrayOpExpr));
                self.never.extend(ops.map(addr)); // alloc: Program
            }
            // ExecInitValuesScan compiles a row at the start only when it holds a SubPlan; ValuesNext compiles the rest
            // per visit. A node whose kind `walk` does not open keeps its gate at the start (the gate list before D2775).
            // D2915: a list past the compiled form's cut streams its rows to PostgreSQL's evaluator (lowerv4
            // `streams_past_cut`), whose per-row ExecInitExprList runs each cell's EXECUTE gate as ValuesNext does (exec
            // ops/modifytable/raw.rs `Stream::row`): its calls are never the program's gates. Only gate-carrying nodes are
            // marked (the gate list asks of no other node) and a Const cell is not walked. D4449: a list of Consts alone
            // (`consts_only`) has no cell to walk and never streams.
            Source::Values { lists, .. } if !gravity_lowerv4::ops::shared::values::consts_only(graph, lists) => {
                let streamed = gravity_lowerv4::ops::shared::values::streams_past_cut(lists);
                let set = if streamed { &mut self.never } else { &mut self.visit };
                for row in lists.iter().filter_map(|l| l.as_list()) {
                    if !streamed && row.iter().any(gravity_decompose::expr::has_subplan) {
                        continue;
                    }
                    for e in row.iter().filter(|e| e.as_const().is_none()) {
                        gravity_decompose::expr::walk(e, &mut |n| {
                            if gated(n.node_tag()) {
                                set.insert(addr(n)); // alloc: Program
                            }
                            true
                        });
                    }
                }
            }
            _ => {}
        }
        for op in p.ops.iter() {
            match op {
                // MJExamineQuals compiles a merge clause's two arguments, never the clause
                Op::Join(JoinDesc::Merge { clauses, .. }) => self.never.extend(clauses.iter().map(addr)), // alloc: Program
                Op::Join(JoinDesc::Nest { inner, .. }) | Op::SubPlan(SubPlanDesc { form: SubForm::Nested { inner }, .. }) => self.pipeline(graph, inner),
                _ => {}
            }
        }
    }
}

/// The functions of the equality operators `ops` (decompose's `node_gate_ops` / `distinct_eq_ops`: the gates C runs at a
/// plan node's init, D2771 / D2786), appended to `out` as gates at the start.
pub(super) fn op_gates(ops: &[Oid], out: &mut Vec<Oid>) -> PgResult<()> {
    for &op in ops {
        out.push(lsyscache::get_opcode(op)?); // alloc: Program
    }
    Ok(())
}
