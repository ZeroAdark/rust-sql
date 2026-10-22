
use mcx::Mcx;
use types_core::Oid;
use types_error::PgResult;
use types_nodes::parsenodes::{Query, RTEKind, RangeTblEntry};
use types_nodes::{CmdType, Node, NodeList};
use types_portal::ParamListHandle;

use crate::relnode::{build_simple_rel, relids_empty, relids_singleton, setup_simple_rel_arrays};
use crate::run::PlannerRun;

pub fn scan_restriction_rows<'mcx>(mcx: Mcx<'mcx>, relid: Oid, varno: u32, quals: &[Node<'mcx>], bound_params: ParamListHandle) -> PgResult<Option<f64>> {
    if varno == 0 || relid == 0 {
        return Ok(None);
    }
    let relkind = lsyscache::get_rel_relkind(relid)? as u8;
    if relkind != types_rel::RELKIND_RELATION && relkind != types_rel::RELKIND_MATVIEW {
        return Ok(None);
    }
    let rte_node = {
        let mut rte = Node::build::<RangeTblEntry>(mcx)?;
        rte.rtekind = RTEKind::RTE_RELATION;
        rte.relid = relid;
        rte.relkind = relkind;
        rte.rellockmode = types_rel::AccessShareLock;
        rte.inh = false;
        rte.seal()
    };
    let mut rtable = NodeList::make1(mcx, rte_node)?;
    for _ in 1..varno {
        rtable.lappend(mcx, rte_node)?;
    }
    let query = {
        let mut q = Node::build::<Query>(mcx)?;
        q.commandType = CmdType::CMD_SELECT;
        q.rtable = rtable;
        q.seal().as_query().expect("built Query")
    };
    let mut partition_directory = None;
    let mut run = PlannerRun::new(mcx);
    crate::plancat::install_partition_directory_slot(&mut run, &mut partition_directory);
    run.glob.bound_params = bound_params;
    let qid = run.intern_query(query);
    run.root.parse = qid;
    setup_simple_rel_arrays(&mut run.root, varno as usize)?;
    let rel_id = build_simple_rel(&mut run, varno, RTEKind::RTE_RELATION)?;
    let tuples = run.root.rel(rel_id).tuples;
    let mut rinfos = mcx::PgVec::new_in(mcx);
    for &q in quals {
        let q = clauses::estimate_expression_value_with_params(mcx, q, bound_params)?;
        let r = crate::initsplan::make_restrictinfo(&mut run, q, true, false, false, false, 0, relids_singleton(mcx, varno), relids_empty(), relids_empty())?;
        rinfos.push(r);
    }
    let selec = crate::clausesel::clauselist_selectivity(&mut run, &rinfos, 0, types_pathnodes::JOIN_INNER, None)?;
    Ok(Some(crate::costsize::clamp_row_est(tuples * selec)))
}
