//! A VALUES list's stream form, which the VALUES source and the per-cell lowering passes both read (lint RA): whether a
//! list streams its rows holding an expression past the compiled form's cost or capacity (CAP-29 / D2907), every such
//! cell one PostgreSQL's evaluator takes alone (nodeValuesscan.c ValuesNext's per-row ExecInitExprList + ExecEvalExpr).
use types_nodes::primnodes::ParamKind;
use types_nodes::Node;

/// CAP-29: whether PostgreSQL's evaluator takes cell `n` alone in its row's turn (the stream form, exec
/// ops/modifytable/raw.rs): no SubPlan and no column reference, every node one the walk knows, and no parameter but an
/// extern one or (D3374, result_cap29) an executor one (a lateral outer row's, a nest's, an InitPlan's), whose value the
/// program hands the evaluation at the row's turn (`exec_params`).
fn streams(n: Node<'_>) -> bool {
    let mut ok = true;
    let known = gravity_decompose::expr::walk(n, &mut |x| {
        ok &= !(x.as_param().is_some_and(|p| !matches!(p.paramkind, ParamKind::PARAM_EXTERN | ParamKind::PARAM_EXEC)) || x.as_sub_plan().is_some() || x.as_alternative_sub_plan().is_some() || x.as_var().is_some());
        ok
    });
    ok && known
}

/// D3374 (result_cap29): the executor parameters the list's cells that are not a Const read, one `Param` node per
/// paramid, ascending: the stream form's parameter slots in their order (protos `LiteralsStream::params`), which the
/// program fills (ops/values/lower.rs) and the evaluation reads as `es_param_exec_vals` (exec ops/modifytable/raw.rs).
pub fn exec_params<'p>(lists: &types_nodes::NodeList<'p>) -> Vec<Node<'p>> {
    let mut out: Vec<(i32, Node<'p>)> = Vec::new(); // alloc: Program
    for c in lists.iter().filter_map(|l| l.as_list()).flat_map(|l| l.iter()).filter(|c| c.as_const().is_none()) {
        gravity_decompose::expr::walk(c, &mut |x| {
            if let Some(p) = x.as_param().filter(|p| p.paramkind == ParamKind::PARAM_EXEC) {
                if !out.iter().any(|e| e.0 == p.paramid) {
                    out.push((p.paramid, x)); // alloc: Program
                }
            }
            true
        });
    }
    out.sort_by_key(|e| e.0);
    out.into_iter().map(|e| e.1).collect() // alloc: Program
}

/// Whether a cell of the list that is not a Const reads an executor parameter.
fn reads_exec(lists: &types_nodes::NodeList<'_>) -> bool {
    let mut any = false;
    for c in lists.iter().filter_map(|l| l.as_list()).flat_map(|l| l.iter()).filter(|c| c.as_const().is_none()) {
        gravity_decompose::expr::walk(c, &mut |x| {
            any |= x.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_EXEC);
            !any
        });
        if any {
            return true;
        }
    }
    false
}

/// D3374: whether the list may stream for its coded cells' cost or the word plan's bound: a list reading an executor
/// parameter streams only past the rows' capacity (where the compiled form cannot take it); under it, it keeps the
/// compiled form it had.
pub(crate) fn streams_for_cost(lists: &types_nodes::NodeList<'_>) -> bool {
    !reads_exec(lists)
}

/// D2907: whether the list streams its rows holding an expression whatever its words (ops/values/lower.rs `make`): past
/// the compiled form's cost (more than `cost::forms::values_coded_cells_max()` cells that are not a Const, R5-B: the class
/// table's rows; `cost::VALUES_CODED_CELLS_MAX` only for a table without them) or its capacity (more
/// than `cost::VALUES_CODED_ROWS_MAX` rows holding one), every such cell one PostgreSQL's evaluator takes alone
/// (`streams`). No code evaluates those cells, so the per-cell lowering passes skip the list: the props walk
/// (ops/scan/premise.rs) and a plain fold's effects of its rows (ops/agg/trans/plain.rs `rows_kept`, which then runs
/// every row: PostgreSQL's form), and exec admission's gate walk (admit/gates.rs `Sites`: the evaluator runs each streamed
/// cell's EXECUTE gate itself, D2915). D3374: a list reading an executor parameter streams past the capacity only
/// (`streams_for_cost`).
pub fn streams_past_cut(lists: &types_nodes::NodeList<'_>) -> bool {
    (over_rows(lists) || coded_cells(lists) > coded_cells_max() && streams_for_cost(lists)) && streamable(lists)
}

/// R5-B: the most cells that are not a Const a list compiles, read from the class table's rows
/// (`cost::forms::values_coded_cells_max`; `cost::VALUES_CODED_CELLS_MAX`, the rule of before, only for a table without them).
pub fn coded_cells_max() -> usize {
    gravity_program::cost::forms::values_coded_cells_max().unwrap_or(gravity_program::cost::VALUES_CODED_CELLS_MAX)
}

/// CAP-29: more than `cost::VALUES_CODED_ROWS_MAX` rows holding a cell that is not a Const (a row that is not a list
/// holds one).
pub(crate) fn over_rows(lists: &types_nodes::NodeList<'_>) -> bool {
    lists.iter().filter(|l| l.as_list().map_or(true, |l| l.iter().any(|n| n.as_const().is_none()))).count() > gravity_program::cost::VALUES_CODED_ROWS_MAX
}

/// CAP-29: every row a list whose every cell is a Const or one PostgreSQL's evaluator takes alone (`streams`).
pub(crate) fn streamable(lists: &types_nodes::NodeList<'_>) -> bool {
    lists.iter().all(|l| l.as_list().is_some_and(|l| l.iter().all(|n| n.as_const().is_some() || streams(n))))
}

/// D4449: whether every row of `lists` is a list of Consts by decompose's count (`PipelineGraph::values_coded`; a list
/// without one: no): a literal table, no cell of code, so none streams (`streams_past_cut` is false), and no premise, gate
/// or parameter is under a cell.
pub fn consts_only(graph: &gravity_decompose::PipelineGraph<'_>, lists: &types_nodes::NodeList<'_>) -> bool {
    graph.values_coded(lists).is_some_and(|f| f.consts_only())
}

/// D4449: whether VALUES row `l` is a list whose every cell is a Const (a literal table's row: no code, no premise).
pub(crate) fn all_consts(l: Node<'_>) -> bool {
    l.as_list().is_some_and(|l| l.iter().all(|c| c.as_const().is_some()))
}

/// D2907: the cells that are not a Const: what the compiled form's per-row leaves evaluate.
fn coded_cells(lists: &types_nodes::NodeList<'_>) -> usize {
    lists.iter().filter_map(|l| l.as_list()).map(|l| l.iter().filter(|c| c.as_const().is_none()).count()).sum()
}
