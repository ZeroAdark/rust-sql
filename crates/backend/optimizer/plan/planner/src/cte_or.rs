//! D3342 CTE-OR, `pgrust.gravity_cte_pushdown` = off (the default) | strict | contained | fill.
//!
//! PostgreSQL computes a CTE read more than once as one InitPlan over its whole body, and pushes no reader's filter into it
//! (it pushes into a CTE only when it inlines one: read once, or NOT MATERIALIZED). A CTE grouped over a year may be
//! read for two of its months; one grouped over every year may be read for two of them. Here, for each such CTE
//! (`ss_process_ctes`, before its body is planned), when every reader filters it the OR of the readers' filters is
//! pushed into the body by PostgreSQL's own subquery pushdown (`subquery_push_qual`: into every UNION ALL arm, HAVING
//! over a grouped body, moved to WHERE by `subquery_planner` where it names grouping columns alone). The readers keep
//! their own filters, so each still reads exactly its rows; the CTE still runs once; it computes only the rows some
//! reader keeps.
//!
//! Admission:
//! - the CTE (`cte_pushable`): read at least twice, not MATERIALIZED / NOT MATERIALIZED (PG's fence, PG's inlining), a
//!   SELECT, not recursive, no SubLink or volatile function in its body (`subquery_is_pushdown_safe`: no LIMIT, ...);
//! - every reference a reader at this level, in the FROM list or under inner joins only (one under an outer join, or in a
//!   SubLink, a subquery or another CTE, leaves the CTE as written: the count must equal `cterefcount`);
//! - a reader's filters: the conjuncts of the WHERE and of the inner joins above it that name that reader alone, each
//!   raise-free (decorrelate.rs `eager_safe`: Vars, Consts, Params, control flow and leakproof strict functions under a
//!   determinate collation), pushdown-safe as `qual_is_pushdown_safe` reads it, and not over an output column the body
//!   aggregates or windows (such a filter could only sit in HAVING and keep nothing out of the grouping);
//! - every reader has one: one reader without leaves the CTE as written.
//!
//! strict also requires the body raise-free but for its aggregates' own transitions (`body_eager_safe`); contained admits
//! any body, so an error PG raises inside the body over a row no reader keeps (a division by zero in a month no reader
//! reads) does not arise: the trade PostgreSQL itself makes for a subquery it pushes into. Fires `plan.cte_or`.
//! fill (D4779): nothing pushed, the body plans as under off; `fill_admits` marks the CteScans for Gravity's fill.
use mcx::Mcx;
use types_error::PgResult;
use types_nodes::nodes_enums::CmdType;
use types_nodes::parsenodes::{CTEMaterialize, CommonTableExpr, Query, RTEKind, RangeTblEntry};
use types_nodes::{JoinType, Node, NodeList, NodeTag};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Strict,
    Contained,
    Fill,
}

fn mode() -> Option<Mode> {
    match guc_tables::backing::pgrust_gravity_cte_pushdown() {
        guc_tables::consts::GRAVITY_CTE_PUSHDOWN_STRICT => Some(Mode::Strict),
        guc_tables::consts::GRAVITY_CTE_PUSHDOWN_CONTAINED => Some(Mode::Contained),
        guc_tables::consts::GRAVITY_CTE_PUSHDOWN_FILL => Some(Mode::Fill),
        _ => None,
    }
}

fn cte_pushable<'mcx>(cte: &CommonTableExpr<'mcx>, query: &'mcx Query<'mcx>) -> PgResult<bool> {
    Ok(cte.cterefcount >= 2
        && cte.ctematerialized == CTEMaterialize::CTEMaterializeDefault
        && !cte.cterecursive
        && query.commandType == CmdType::CMD_SELECT
        && !query_has_sublinks(query)
        && !cte.ctequery.map_or(Ok(true), clauses::contain_volatile_functions)?)
}

pub(crate) fn fill_admits<'mcx>(cte: &CommonTableExpr<'mcx>, query: &'mcx Query<'mcx>) -> PgResult<bool> {
    Ok(mode() == Some(Mode::Fill) && cte_pushable(cte, query)?)
}

/// A reader of the CTE at this level: its range-table index and its filters (conjuncts naming it alone).
struct Reader<'mcx> {
    rti: usize,
    rte: &'mcx RangeTblEntry<'mcx>,
    quals: Vec<Node<'mcx>>, // alloc: planner (per CTE, a handful)
}

/// Pushes the OR of CTE `cte`'s readers' filters (in `parse`, the level whose `cteList` holds it) into `body`, the copy
/// of its query `ss_process_ctes` is about to plan. Nothing when the mode is off or any admission test fails.
pub(crate) fn push_readers_or<'mcx>(
    mcx: Mcx<'mcx>,
    parse: &Query<'mcx>,
    cte: &CommonTableExpr<'mcx>,
    query: &'mcx Query<'mcx>,
    body: &mut Query<'mcx>,
) -> PgResult<()> {
    let Some(mode) = mode().filter(|&m| m != Mode::Fill) else { return Ok(()) };
    if !cte_pushable(cte, query)? {
        return Ok(());
    }
    let Some(name) = cte.ctename else { return Ok(()) };
    let Some(jt) = parse.jointree else { return Ok(()) };
    // the readers and the conjuncts that may filter them: the WHERE and the quals of the inner joins over them
    let mut readers: Vec<Reader<'mcx>> = Vec::new(); // alloc: planner
    let mut conjuncts: Vec<Node<'mcx>> = Vec::new(); // alloc: planner
    flatten_and(jt.quals, &mut conjuncts);
    for item in jt.fromlist.iter() {
        if !walk_item(parse, name, item, &mut readers, &mut conjuncts)? {
            return Ok(());
        }
    }
    if readers.len() != cte.cterefcount as usize {
        return Ok(());
    }
    let Some(safety) = crate::pushdown::cte_safety(mcx, query)? else { return Ok(()) };
    if mode == Mode::Strict && !body_eager_safe(query)? {
        return Ok(());
    }
    for r in readers.iter_mut() {
        for &c in conjuncts.iter() {
            if let Some(f) = reader_filter(mcx, query, r.rti, c, &safety)? {
                r.quals.push(f);
            }
        }
        if r.quals.is_empty() {
            return Ok(());
        }
    }
    // OR over the readers of the AND of each one's filters, every Var renamed to the first reader's index
    let (rti0, rte0) = (readers[0].rti, readers[0].rte);
    let mut arms = NodeList::nil();
    for r in readers.iter() {
        let mut conj = NodeList::nil();
        for &q in r.quals.iter() {
            let q = rewrite_manip::copy_node(mcx, q)?;
            if r.rti != rti0 {
                rewrite_manip::ChangeVarNodes(mcx, q, r.rti as i32, rti0 as i32, 0)?;
            }
            conj.lappend(mcx, q)?;
        }
        let arm = if conj.len() == 1 { conj.nth(0) } else { clauses::make_andclause(mcx, conj)? };
        arms.lappend(mcx, arm)?;
    }
    let or = clauses::make_orclause(mcx, arms)?;
    // a body whose every arm already keeps only the readers' rows by its own WHERE is left as written: the push would
    // add a redundant clause that moves only the estimates and the join order
    if crate::pushdown::cte_qual_implied(mcx, body, rte0, rti0, or)? {
        return Ok(());
    }
    crate::pushdown::cte_push_qual(mcx, body, rte0, rti0, or)?;
    fires::fire("plan.cte_or"); // shapecheck fire signal (D3342)
    Ok(())
}

/// Walks a FROM-list item: a reader of `name` is recorded; an inner join's quals join the conjuncts and its sides are
/// walked; false when a reader sits under any other join (the CTE stays as written).
fn walk_item<'mcx>(parse: &Query<'mcx>, name: &str, item: Node<'mcx>, readers: &mut Vec<Reader<'mcx>>, conjuncts: &mut Vec<Node<'mcx>>) -> PgResult<bool> {
    if let Some(r) = item.as_range_tbl_ref() {
        let rti = r.rtindex as usize;
        let rte = parse.rtable.nth(rti - 1).as_range_tbl_entry().expect("rtable cell");
        if is_reader(rte, name) {
            readers.push(Reader { rti, rte, quals: Vec::new() }); // alloc: planner
        }
        return Ok(true);
    }
    if let Some(j) = item.as_join_expr() {
        if j.jointype != JoinType::JOIN_INNER {
            return Ok(!mentions_reader(parse, name, j.larg) && !mentions_reader(parse, name, j.rarg));
        }
        flatten_and(j.quals, conjuncts);
        return Ok(walk_item(parse, name, j.larg, readers, conjuncts)? && walk_item(parse, name, j.rarg, readers, conjuncts)?);
    }
    match item.as_from_expr() {
        Some(f) => {
            flatten_and(f.quals, conjuncts);
            for i in f.fromlist.iter() {
                if !walk_item(parse, name, i, readers, conjuncts)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        None => Ok(false),
    }
}

fn is_reader(rte: &RangeTblEntry<'_>, name: &str) -> bool {
    rte.rtekind == RTEKind::RTE_CTE && rte.ctelevelsup == 0 && !rte.self_reference && rte.ctename == Some(name)
}

/// A reader of `name` anywhere under jointree item `item`.
fn mentions_reader(parse: &Query<'_>, name: &str, item: Node<'_>) -> bool {
    if let Some(r) = item.as_range_tbl_ref() {
        return parse.rtable.nth(r.rtindex as usize - 1).as_range_tbl_entry().is_some_and(|rte| is_reader(rte, name));
    }
    if let Some(j) = item.as_join_expr() {
        return mentions_reader(parse, name, j.larg) || mentions_reader(parse, name, j.rarg);
    }
    item.as_from_expr().is_some_and(|f| f.fromlist.iter().any(|i| mentions_reader(parse, name, i)))
}

/// The conjuncts of `qual` (an AND tree flattened; a constant TRUE is none).
fn flatten_and<'mcx>(qual: Option<Node<'mcx>>, out: &mut Vec<Node<'mcx>>) {
    let Some(q) = qual else { return };
    if let Some(b) = q.as_bool_expr().filter(|b| b.boolop == types_nodes::primnodes::BoolExprType::AND_EXPR) {
        for a in b.args.iter() {
            flatten_and(Some(a), out);
        }
        return;
    }
    if q.as_const().is_some_and(|c| !c.constisnull && c.constvalue.as_bool()) {
        return;
    }
    out.push(q);
}

/// Conjunct `c` folded (`eval_const_expressions`, as the parent's own preprocessing folds it: e.g. `d_moy = 1 + 1`)
/// when it filters reader `rti` alone, raise-free, pushdown-safe, and over no aggregated or windowed column; else None.
fn reader_filter<'mcx>(mcx: Mcx<'mcx>, query: &'mcx Query<'mcx>, rti: usize, c: Node<'mcx>, safety: &crate::pushdown::CteSafety<'mcx>) -> PgResult<Option<Node<'mcx>>> {
    let reads_alone = |c: Node<'mcx>| -> PgResult<Option<Vec<(i32, i16)>>> {
        let mut vars = Vec::new(); // alloc: planner
        let ok = plain_vars(c, &mut vars)? && !vars.is_empty() && vars.iter().all(|&(no, _)| no == rti as i32);
        Ok(ok.then_some(vars))
    };
    if reads_alone(c)?.is_none() {
        return Ok(None);
    }
    let c = clauses::eval_const_expressions(mcx, c)?;
    let Some(vars) = reads_alone(c)? else { return Ok(None) };
    if !crate::decorrelate::eager_safe(c)? || !crate::pushdown::cte_clause_safe(mcx, query, rti, c, safety)? {
        return Ok(None);
    }
    for &(_, att) in vars.iter() {
        if column_computed(query, att)? {
            return Ok(None);
        }
    }
    Ok(Some(c))
}

/// `node`'s level-0 Vars as (varno, varattno) into `out`; false at an upper-level Var, a PlaceHolderVar, an aggregate, a
/// grouping function, a window function or a SubLink.
fn plain_vars(node: Node<'_>, out: &mut Vec<(i32, i16)>) -> PgResult<bool> {
    struct W<'a>(&'a mut Vec<(i32, i16)>);
    impl<'m> nodes_core::NodeWalker<'m> for W<'_> {
        fn visit(&mut self, n: Node<'m>) -> PgResult<bool> {
            match n.node_tag() {
                NodeTag::T_Var => {
                    let v = n.as_var().unwrap();
                    if v.varlevelsup != 0 {
                        return Ok(true);
                    }
                    self.0.push((v.varno, v.varattno));
                    return Ok(false);
                }
                NodeTag::T_PlaceHolderVar | NodeTag::T_Aggref | NodeTag::T_GroupingFunc | NodeTag::T_WindowFunc | NodeTag::T_SubLink | NodeTag::T_SubPlan => return Ok(true),
                _ => {}
            }
            nodes_core::expression_tree_walker(n, self)
        }
    }
    Ok(!nodes_core::NodeWalker::visit(&mut W(out), node)?)
}

/// Output column `att` of `q` (every UNION ALL arm's) is an aggregate's or a window function's value, or holds one.
fn column_computed(q: &Query<'_>, att: i16) -> PgResult<bool> {
    if q.setOperations.is_some() {
        for n in q.rtable.iter() {
            let rte = n.as_range_tbl_entry().expect("rtable cell");
            if let (RTEKind::RTE_SUBQUERY, Some(sub)) = (rte.rtekind, rte.subquery) {
                if column_computed(sub, att)? {
                    return Ok(true);
                }
            }
        }
        return Ok(false);
    }
    let Some(tle) = q.targetList.iter().filter_map(|n| n.as_target_entry()).find(|t| t.resno == att) else { return Ok(true) };
    Ok(clauses::contain_agg_clause(tle.expr)? || clauses::contain_window_function(tle.expr)?)
}

/// `q` or a query below it (its range table's subqueries, its CTEs) holds a SubLink.
fn query_has_sublinks(q: &Query<'_>) -> bool {
    q.hasSubLinks
        || q.rtable.iter().any(|n| n.as_range_tbl_entry().and_then(|r| r.subquery).is_some_and(query_has_sublinks))
        || q.cteList.iter().any(|n| n.as_common_table_expr().and_then(|c| c.ctequery).and_then(|c| c.as_query()).is_some_and(query_has_sublinks))
}

/// strict: every expression of the body (its range table's subqueries and UNION arms too) is raise-free but the
/// aggregates' own transitions, whose arguments and FILTERs are held to it (decorrelate.rs `eager_safe_aggs`); no window
/// function, SRF, CTE or locking clause.
fn body_eager_safe(q: &Query<'_>) -> PgResult<bool> {
    if q.commandType != CmdType::CMD_SELECT || q.hasWindowFuncs || q.hasTargetSRFs || q.hasSubLinks || !q.cteList.is_nil() || !q.rowMarks.is_nil() {
        return Ok(false);
    }
    for n in q.targetList.iter() {
        if !crate::decorrelate::eager_safe_aggs(n.as_target_entry().expect("tlist cell").expr)? {
            return Ok(false);
        }
    }
    for e in [q.havingQual, q.limitOffset, q.limitCount].into_iter().flatten() {
        if !crate::decorrelate::eager_safe_aggs(e)? {
            return Ok(false);
        }
    }
    for n in q.rtable.iter() {
        let rte = n.as_range_tbl_entry().expect("rtable cell");
        let ok = match rte.rtekind {
            RTEKind::RTE_RELATION | RTEKind::RTE_RESULT | RTEKind::RTE_JOIN | RTEKind::RTE_GROUP => true,
            RTEKind::RTE_SUBQUERY => rte.subquery.map_or(Ok(false), body_eager_safe)?,
            RTEKind::RTE_VALUES => {
                let mut ok = true;
                for l in rte.values_lists.iter() {
                    ok = ok && crate::decorrelate::eager_safe(l)?;
                }
                ok
            }
            _ => false,
        };
        if !ok {
            return Ok(false);
        }
    }
    match q.jointree {
        Some(jt) => Ok(jt.quals.map_or(Ok(true), crate::decorrelate::eager_safe)? && all_items(&jt.fromlist)?),
        None => Ok(true),
    }
}

fn all_items(items: &NodeList<'_>) -> PgResult<bool> {
    for item in items.iter() {
        if !item_eager_safe(item)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn item_eager_safe(item: Node<'_>) -> PgResult<bool> {
    if item.as_range_tbl_ref().is_some() {
        return Ok(true);
    }
    if let Some(j) = item.as_join_expr() {
        return Ok(j.quals.map_or(Ok(true), crate::decorrelate::eager_safe)? && item_eager_safe(j.larg)? && item_eager_safe(j.rarg)?);
    }
    match item.as_from_expr() {
        Some(f) => Ok(f.quals.map_or(Ok(true), crate::decorrelate::eager_safe)? && all_items(&f.fromlist)?),
        None => Ok(false),
    }
}
