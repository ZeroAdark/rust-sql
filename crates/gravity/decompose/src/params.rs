//! The executor parameters (PARAM_EXEC) an expression reads, and the ones a NestLoop sets, read in one place so that
//! lowering's re-open rules (srf/reopen_once.rs, rerun/store_read.rs) do not each walk the tree for Param nodes or read
//! a NestLoopParam themselves. An expression's reads are its Param nodes of kind PARAM_EXEC, wherever `expr::walk` reaches
//! (a SubPlan's test and `args` included, its plan not: a SubPlan's own params are bound by its call).
use types_nodes::primnodes::ParamKind;
use types_nodes::{Node, NodeList};

use crate::expr;

/// Each PARAM_EXEC id the expressions `nodes` read, ascending, each once.
pub fn read<'p>(nodes: impl IntoIterator<Item = Node<'p>>) -> Vec<i32> {
    let mut ids = Vec::new(); // alloc: Program
    for n in nodes {
        expr::walk(n, &mut |x| {
            ids.extend(exec_id(x)); // alloc: Program
            true
        });
    }
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// D1874: FROM function expression `e` as a store's own executor state evaluates it (hosts srf/store.rs `bare`): each
/// SubPlan in it replaced by a PARAM_EXEC Param numbered `base + plan_id - 1` (its type, typmod and collation), whose value
/// the program evaluates on the step's row and passes at every open (lowerv4 rerun/store_read.rs `fed`); None when `e`
/// holds none. `base` is past the plan's own exec params (its paramExecTypes' length), so each subplan has its own id; the
/// ids put in are added to `ids`. An AlternativeSubPlan stays (lowerv4 refuses such a call by D1874's name).
pub fn fed_params<'m>(mcx: mcx::Mcx<'m>, e: Node<'m>, base: i32, ids: &mut Vec<i32>) -> types_error::PgResult<Option<Node<'m>>> {
    crate::walk::stack_check(); // D3748
    if let Some(s) = e.as_sub_plan() {
        let paramid = base + s.plan_id - 1;
        if !ids.contains(&paramid) {
            ids.push(paramid); // alloc: Exec
        }
        let p = types_nodes::primnodes::Param { paramkind: types_nodes::primnodes::ParamKind::PARAM_EXEC, paramid, paramtype: nodes_core::expr_type(e), paramtypmod: nodes_core::expr_typmod(e), paramcollid: nodes_core::expr_collation(e), location: -1 };
        return Node::mk(mcx, p).map(Some);
    }
    if e.as_alternative_sub_plan().is_some() {
        return Ok(None);
    }
    nodes_core::expression_tree_mutator(mcx, e, &mut |c| fed_params(mcx, c, base, ids))
}

/// GENP-1: whether expression `n` reads an extern parameter (a generic plan's `$n`).
pub fn reads_extern(n: Node<'_>) -> bool {
    let mut any = false;
    expr::walk(n, &mut |x| {
        any |= x.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_EXTERN);
        !any
    });
    any
}

/// Whether expression `n` reads one of PARAM_EXEC ids `ids`.
pub fn reads_any(n: Node<'_>, ids: &[i32]) -> bool {
    let mut any = false;
    expr::walk(n, &mut |x| {
        any |= exec_id(x).is_some_and(|id| ids.contains(&id));
        !any
    });
    any
}

/// The PARAM_EXEC ids a NestLoop sets before each rescan of its inner: its `nestParams`' `paramno`s, in plan order.
pub fn nest_sets<'a>(nest_params: &'a NodeList<'_>) -> impl Iterator<Item = i32> + 'a {
    nest_params.iter().filter_map(|n| n.as_nest_loop_param()).map(|p| p.paramno)
}

/// The id of `x` when it is a PARAM_EXEC Param.
fn exec_id(x: Node<'_>) -> Option<i32> {
    x.as_param().filter(|p| p.paramkind == ParamKind::PARAM_EXEC).map(|p| p.paramid)
}

#[cfg(test)]
mod tests {
    use types_nodes::plannodes::NestLoopParam;
    use types_nodes::primnodes::{OpExpr, Param, ParamKind};
    use types_nodes::{Node, NodeList};

    /// srf rows 45 / 57 / 61: the ids an expression reads (an extern parameter is not one, a repeat counts once), whether it
    /// reads one of a set, and the ids a NestLoop's params set.
    #[test]
    fn executor_parameters_read_and_set() {
        let ctx = mcx::MemoryContext::new("srf-params-read");
        let mcx = ctx.mcx();
        let param = |kind, paramid| Node::mk(mcx, Param { paramkind: kind, paramid, paramtype: types_core::INT4OID, paramtypmod: -1, paramcollid: 0, location: -1 }).unwrap();
        let two = |a, b| Node::mk(mcx, OpExpr { opno: 551, opfuncid: 0, opresulttype: types_core::INT4OID, opretset: false, opcollid: 0, inputcollid: 0, args: NodeList::make2(mcx, a, b).unwrap(), location: -1 }).unwrap();
        let (e3, e1, x0) = (param(ParamKind::PARAM_EXEC, 3), param(ParamKind::PARAM_EXEC, 1), param(ParamKind::PARAM_EXTERN, 0));
        let sum = two(two(e3, x0), two(e1, e3));
        assert_eq!(super::read([sum]), [1, 3], "ascending, each once, no extern parameter");
        assert_eq!(super::read([x0, e1, e3]), [1, 3], "over several expressions");
        assert!(super::read([x0]).is_empty());
        assert!(super::reads_any(sum, &[7, 3]));
        assert!(!super::reads_any(sum, &[0, 2]), "the extern parameter 0 is not executor parameter 0");
        let nlp = |paramno| Node::mk(mcx, NestLoopParam { paramno, paramval: e1 }).unwrap();
        let params = NodeList::make2(mcx, nlp(4), nlp(2)).unwrap();
        assert!(super::nest_sets(&params).eq([4, 2]), "in plan order");
    }
}
