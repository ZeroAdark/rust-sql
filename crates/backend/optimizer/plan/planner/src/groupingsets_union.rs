//! Gravity grouping-sets union (docs/gravity/decompose.md §2.6.1, decision 70; ruling
//! 2026-09-12: rewrites go through the planner). A SELECT level
//!
//! ```sql
//! SELECT tl FROM f WHERE w GROUP BY GROUPING SETS (s1, …, sn) HAVING h [ORDER BY / DISTINCT / …]
//! ```
//!
//! is also planned as the union of its groupings: one grouped arm per expanded set, each a copy
//! of the level over its own walk of the input (`SELECT tl_i FROM f WHERE w GROUP BY s_i HAVING
//! h_i`), a `UNION ALL` of the arms, and the level's own ORDER BY / DISTINCT / window / LIMIT
//! layer above it. Inside arm `i` a grouped column outside `s_i` is a NULL constant of its type
//! and `GROUPING(…)` is the integer constant the set gives it; the target-list expressions are
//! pushed whole into the arms (the maximal `WindowFunc`-free subtrees when the level has window
//! functions, so the windows still run over the union). The transform is a costed alternative,
//! never forced: `standard_planner` plans the statement both ways and keeps the cheaper plan
//! (`pgrust.gravity_groupingsets_rewrite = force` keeps this one whenever it exists). The graph
//! the decomposer declares for the un-rewritten node (`agg.rs`'s `map_sets`) is this plan's:
//! one hashed (or plain) group per set over the input, a union slot, no order.
//!
//! Equivalence: PostgreSQL emits every set's groups (a duplicate set twice), nulls the columns
//! outside the set in the output and in HAVING (18's `nullingrels`), evaluates HAVING per set,
//! and orders nothing unless ORDER BY says so; the empty set is a plain aggregate (one row over
//! an empty input). The arms re-scan the input, so a level whose range table, quals, target
//! list, HAVING or grouping expressions hold a volatile function is not rewritten (its rows
//! could differ between walks). Outer references survive the two extra levels
//! (`IncrementVarSublevelsUp`, the level's own CTE references bumped by hand); sublinks inside
//! the level keep their depth relative to the arm. Not rewritten (the standard plan stands): a
//! level with more than [`MAX_ARMS`] expanded sets, one whose grouping sets the parser did not
//! pair with a GROUP RTE, or one grouping by a set-returning expression (its ProjectSet feeds
//! every set's input; an arm outside the set would aggregate the unexpanded rows).

use mcx::Mcx;
use types_error::PgResult;
use types_nodes::list::{IntList, NodeList, OidList};
use types_nodes::nodes_enums::{CmdType, LimitOption};
use types_nodes::parsenodes::{Query, RTEKind, RangeTblEntry, SetOperation, SetOperationStmt};
use types_nodes::primnodes::{Alias, FromExpr, GroupingFunc, TargetEntry, Var};
use types_nodes::{Node, NodeTag};

pub(crate) const MAX_ARMS: usize = 64;
const INT4OID: u32 = 23;
const OUTER_ALIAS: &str = "gravity_groupingsets";

pub(crate) fn alternative<'mcx>(mcx: Mcx<'mcx>, parse: &Query<'mcx>) -> PgResult<Option<&'mcx mut Query<'mcx>>> {
    if crate::gucs::gravity_groupingsets_rewrite() == guc_tables::consts::GRAVITY_GS_REWRITE_OFF || !any_grouping_sets(parse) {
        return Ok(None);
    }
    let Some(q) = transform_level(mcx, parse)? else { return Ok(None) };
    let copy = copyfuncs::copy_query(mcx, &q)?;
    Ok(Some(mcx::leak_in(mcx::alloc_in(mcx, copy)?)))
}

fn any_grouping_sets(q: &Query<'_>) -> bool {
    if !q.groupingSets.is_nil() {
        return true;
    }
    q.rtable.iter().any(|n| n.as_range_tbl_entry().and_then(|r| r.subquery).is_some_and(any_grouping_sets))
        || q.cteList.iter().any(|n| n.as_common_table_expr().and_then(|c| c.ctequery).and_then(|n| n.as_query()).is_some_and(any_grouping_sets))
}

fn transform_level<'mcx>(mcx: Mcx<'mcx>, q: &Query<'mcx>) -> PgResult<Option<Query<'mcx>>> {
    let mut changed = false;
    let mut rtable = NodeList::with_capacity(mcx, q.rtable.len())?;
    for n in q.rtable.iter() {
        let rte = n.as_range_tbl_entry().expect("rtable cell");
        match rte.subquery {
            Some(sub) if rte.rtekind == RTEKind::RTE_SUBQUERY => match transform_level(mcx, sub)? {
                Some(new) => {
                    let copy = crate::prepjointree::rte_copy_with_perminfoindex(mcx, rte, rte.perminfoindex)?;
                    let new = mcx::alloc_leak_in(mcx, new)?;
                    // SAFETY: the copy is exclusively ours until it enters the list.
                    unsafe { copy.with_mut::<RangeTblEntry, _>(|r| r.subquery = Some(new)) };
                    rtable.lappend(mcx, copy)?;
                    changed = true;
                }
                None => rtable.lappend(mcx, n)?,
            },
            _ => rtable.lappend(mcx, n)?,
        }
    }
    let mut ctes = NodeList::with_capacity(mcx, q.cteList.len())?;
    for n in q.cteList.iter() {
        let cte = n.as_common_table_expr().expect("cteList cell");
        match cte.ctequery.and_then(|c| c.as_query()).map(|sub| transform_level(mcx, sub)).transpose()?.flatten() {
            Some(new) => {
                let copy = rewrite_manip::copy_node(mcx, n)?;
                let new = Node::mk(mcx, new)?;
                // SAFETY: as above.
                unsafe { copy.with_mut::<types_nodes::parsenodes::CommonTableExpr, _>(|c| c.ctequery = Some(new)) };
                ctes.lappend(mcx, copy)?;
                changed = true;
            }
            None => ctes.lappend(mcx, n)?,
        }
    }
    let mut level = rewrite_manip::flat_copy_query(mcx, q)?;
    level.rtable = rtable;
    level.cteList = ctes;
    if !level.groupingSets.is_nil() {
        if let Some(rewritten) = rewrite_level(mcx, &level)? {
            return Ok(Some(rewritten));
        }
    }
    Ok(changed.then_some(level))
}

struct Col<'mcx> {
    src: Node<'mcx>,
    name: &'mcx str,
    groupref: u32,
}

fn rewrite_level<'mcx>(mcx: Mcx<'mcx>, q: &Query<'mcx>) -> PgResult<Option<Query<'mcx>>> {
    if q.commandType != CmdType::CMD_SELECT || q.resultRelation != 0 || q.setOperations.is_some() || !q.rowMarks.is_nil() || q.hasForUpdate {
        return Ok(None);
    }
    let Some(sets) = parse_agg::expand_grouping_sets(mcx, &q.groupingSets, q.groupDistinct, MAX_ARMS as i32)? else { return Ok(None) };
    let n = sets.len();
    if n == 0 || n > MAX_ARMS {
        return Ok(None);
    }
    let group_rti = q.rtable.iter().position(|n| n.as_range_tbl_entry().expect("rtable cell").rtekind == RTEKind::RTE_GROUP).map_or(0, |i| i as i32 + 1);
    let mut refs: Vec<u32> = ::mcx::stdvec::try_vec(q.groupClause.len(), "groupingsets_union")?;
    for gc in q.groupClause.iter() {
        refs.push(gc.as_sort_group_clause().expect("groupClause cell").tleSortGroupRef);
    }
    if (group_rti == 0) != q.groupClause.is_nil() || !q.hasGroupRTE && group_rti != 0 {
        return Ok(None);
    }
    if group_rti != 0 {
        let group = q.rtable.nth(group_rti as usize - 1).as_range_tbl_entry().expect("rtable cell");
        if group.groupexprs.len() != refs.len() {
            return Ok(None);
        }
        if group.groupexprs.iter().any(coerce::expression_returns_set) {
            return Ok(None);
        }
    }
    if clauses::contain_volatile_functions(Node::mk(mcx, rewrite_manip::flat_copy_query(mcx, q)?)?)? {
        return Ok(None);
    }

    let mut cols: Vec<Col<'mcx>> = Vec::new();
    let outer_tlist = split_tlist(mcx, &q.targetList, &refs, &mut |src, name, groupref| {
        cols.push(Col { src, name, groupref });
        Ok(cols.len() as i16)
    })?;
    let ncols = cols.len();
    let mut coltypes: Vec<(u32, i32, u32)> = ::mcx::stdvec::try_vec(ncols, "groupingsets_union")?;
    for c in cols.iter() {
        let (ty, tm) = ::costsize::expr_type_typmod(c.src);
        coltypes.push((ty, tm, crate::pathkeys::expr_collation(c.src)));
    }

    let mut arm_rtable = NodeList::with_capacity(mcx, n)?;
    let mut arms: Vec<&'mcx Query<'mcx>> = ::mcx::stdvec::try_vec(n, "groupingsets_union")?;
    for (i, set) in sets.iter().enumerate() {
        let members: Vec<i32> = set.iter().copied().collect();
        let arm_node = Node::mk(mcx, copyfuncs::copy_query(mcx, q)?)?;
        rewrite_manip::IncrementVarSublevelsUp(arm_node, 2, 1)?;
        bump_own_cte_refs(arm_node)?;
        let arm = arm_node.as_query().expect("Query");
        let mut pushed: Vec<Node<'mcx>> = ::mcx::stdvec::try_vec(ncols, "groupingsets_union")?;
        split_tlist(mcx, &arm.targetList, &refs, &mut |src, _, _| {
            pushed.push(src);
            Ok(pushed.len() as i16)
        })?;
        debug_assert_eq!(pushed.len(), ncols);
        let mut newattno: Vec<i16> = ::mcx::stdvec::try_filled(refs.len(), 0, "groupingsets_union")?;
        let mut kept = 0i16;
        for (i, r) in refs.iter().enumerate() {
            if members.contains(&(*r as i32)) {
                kept += 1;
                newattno[i] = kept;
            }
        }
        if group_rti != 0 {
            let rte_node = arm.rtable.nth(group_rti as usize - 1);
            let rte = rte_node.as_range_tbl_entry().expect("rtable cell");
            let mut groupexprs = NodeList::with_capacity(mcx, kept as usize)?;
            let mut colnames = NodeList::with_capacity(mcx, kept as usize)?;
            for (i, e) in rte.groupexprs.iter().enumerate() {
                if newattno[i] != 0 {
                    groupexprs.lappend(mcx, e)?;
                    if let Some(c) = rte.eref.filter(|a| a.colnames.len() > i) {
                        colnames.lappend(mcx, c.colnames.nth(i))?;
                    }
                }
            }
            let eref = mcx::alloc_leak_in(mcx, Alias { aliasname: rte.eref.and_then(|a| a.aliasname), colnames })?;
            // SAFETY: the arm is a fresh deep copy, exclusively ours.
            unsafe {
                rte_node.with_mut::<RangeTblEntry, _>(|r| {
                    r.groupexprs = groupexprs;
                    r.eref = Some(eref);
                })
            }
            .expect("RangeTblEntry");
        }
        let mut ctx = Subst { group_rti, refs: &refs, set: &members, newattno: &newattno, sublevels_up: 0 };
        let mut tlist = NodeList::with_capacity(mcx, ncols)?;
        for (k, (src, col)) in pushed.iter().zip(cols.iter()).enumerate() {
            // An entry that defines a grouping expression is that grouping column (PostgreSQL
            // resolves it by its sortgroupref): NULL outside the set even when its expression
            // also matches a column inside it (`g AS x, g AS y … GROUP BY x, ROLLUP (y)`).
            let expr = if col.groupref != 0 && !members.contains(&(col.groupref as i32)) {
                null_const(mcx, coltypes[k].0, coltypes[k].1, coltypes[k].2)?
            } else {
                subst(&mut ctx, mcx, *src)?.unwrap_or(*src)
            };
            let (ty, tm) = ::costsize::expr_type_typmod(expr);
            if ty != coltypes[k].0 {
                return Ok(None);
            }
            if tm != coltypes[k].1 {
                coltypes[k].1 = -1;
            }
            let in_set = col.groupref != 0 && members.contains(&(col.groupref as i32));
            tlist.lappend(
                mcx,
                Node::mk(mcx, TargetEntry { expr, resno: k as i16 + 1, resname: Some(col.name), ressortgroupref: if in_set { col.groupref } else { 0 }, resorigtbl: 0, resorigcol: 0, resjunk: false })?,
            )?;
        }
        let mut group = NodeList::nil();
        for gc in arm.groupClause.iter() {
            if members.contains(&(gc.as_sort_group_clause().expect("groupClause cell").tleSortGroupRef as i32)) {
                group.lappend(mcx, gc)?;
            }
        }
        let having = match arm.havingQual {
            Some(h) => Some(subst(&mut ctx, mcx, h)?.unwrap_or(h)),
            None => None,
        };
        let sets = if members.is_empty() && !arm.hasAggs {
            NodeList::make1(mcx, Node::mk_grouping_set(mcx, types_nodes::parsenodes::GroupingSetKind::GROUPING_SET_EMPTY, NodeList::nil(), -1)?)?
        } else {
            NodeList::nil()
        };
        // SAFETY: the arm is a fresh deep copy, exclusively ours.
        unsafe {
            arm_node.with_mut::<Query, _>(|a| {
                a.targetList = tlist;
                a.groupClause = group;
                a.groupingSets = sets;
                a.groupDistinct = false;
                a.havingQual = having;
                a.sortClause = NodeList::nil();
                a.distinctClause = NodeList::nil();
                a.hasDistinctOn = false;
                a.windowClause = NodeList::nil();
                a.hasWindowFuncs = false;
                a.limitOffset = None;
                a.limitCount = None;
                a.limitOption = LimitOption::LIMIT_OPTION_COUNT;
                a.cteList = NodeList::nil();
                a.hasRecursive = false;
                a.hasModifyingCTE = false;
            })
        };
        let arm_name = str_in(mcx, &format!("*SELECT* {}", i + 1))?;
        let mut colnames = NodeList::with_capacity(mcx, ncols)?;
        for c in cols.iter() {
            colnames.lappend(mcx, Node::mk_string(mcx, c.name)?)?;
        }
        arms.push(arm);
        let alias = mcx::alloc_leak_in(mcx, Alias { aliasname: Some(arm_name), colnames: NodeList::nil() })?;
        let eref = mcx::alloc_leak_in(mcx, Alias { aliasname: Some(arm_name), colnames })?;
        arm_rtable.lappend(
            mcx,
            Node::mk(mcx, RangeTblEntry { rtekind: RTEKind::RTE_SUBQUERY, subquery: Some(arm), alias: Some(alias), eref: Some(eref), lateral: false, inFromCl: true, ..Default::default() })?,
        )?;
    }

    let (mut col_types, mut col_typmods, mut col_colls) = (OidList::nil(), IntList::nil(), OidList::nil());
    for &(ty, tm, coll) in coltypes.iter() {
        col_types.lappend(mcx, ty)?;
        col_typmods.lappend(mcx, tm)?;
        col_colls.lappend(mcx, coll)?;
    }
    let mut setop = Node::mk_range_tbl_ref(mcx, 1)?;
    for i in 2..=n as i32 {
        setop = Node::mk(
            mcx,
            SetOperationStmt {
                op: SetOperation::SETOP_UNION,
                all: true,
                larg: Some(setop),
                rarg: Some(Node::mk_range_tbl_ref(mcx, i)?),
                colTypes: col_types.clone_in(mcx)?,
                colTypmods: col_typmods.clone_in(mcx)?,
                colCollations: col_colls.clone_in(mcx)?,
                groupClauses: NodeList::nil(),
            },
        )?;
    }
    let mut union_tlist = NodeList::with_capacity(mcx, ncols)?;
    let mut colnames = NodeList::with_capacity(mcx, ncols)?;
    for (k, c) in cols.iter().enumerate() {
        let (ty, tm, coll) = coltypes[k];
        let var = Node::mk_var(mcx, 1, k as i16 + 1, ty, tm, coll, 0)?;
        union_tlist.lappend(mcx, Node::mk_target_entry(mcx, var, k as i16 + 1, Some(c.name), false)?)?;
        colnames.lappend(mcx, Node::mk_string(mcx, c.name)?)?;
    }
    let union = Query {
        commandType: CmdType::CMD_SELECT,
        querySource: q.querySource,
        canSetTag: true,
        hasAggs: false,
        hasSubLinks: false,
        rtable: arm_rtable,
        jointree: Some(mcx::alloc_leak_in(mcx, FromExpr { fromlist: NodeList::nil(), quals: None })?),
        targetList: union_tlist,
        setOperations: Some(setop),
        stmt_location: q.stmt_location,
        stmt_len: q.stmt_len,
        ..Default::default()
    };
    let inner: &'mcx Query<'mcx> = if n == 1 { arms[0] } else { mcx::alloc_leak_in(mcx, union)? };

    let alias = mcx::alloc_leak_in(mcx, Alias { aliasname: Some(OUTER_ALIAS), colnames: NodeList::nil() })?;
    let eref = mcx::alloc_leak_in(mcx, Alias { aliasname: Some(OUTER_ALIAS), colnames })?;
    let outer_rte = Node::mk(mcx, RangeTblEntry { rtekind: RTEKind::RTE_SUBQUERY, subquery: Some(inner), alias: Some(alias), eref: Some(eref), lateral: false, inFromCl: true, ..Default::default() })?;
    let mut outer = rewrite_manip::flat_copy_query(mcx, q)?;
    outer.rtable = NodeList::make1(mcx, outer_rte)?;
    outer.rteperminfos = NodeList::nil();
    outer.jointree = Some(mcx::alloc_leak_in(mcx, FromExpr { fromlist: NodeList::make1(mcx, Node::mk_range_tbl_ref(mcx, 1)?)?, quals: None })?);
    outer.targetList = fix_outer_typmods(mcx, &outer_tlist, &coltypes)?;
    outer.groupClause = NodeList::nil();
    outer.groupDistinct = false;
    outer.groupingSets = NodeList::nil();
    outer.havingQual = None;
    outer.hasAggs = false;
    outer.hasGroupRTE = false;
    outer.hasTargetSRFs = false;
    outer.hasSubLinks = rewrite_manip::checkExprHasSubLink_opt(q.limitOffset)? || rewrite_manip::checkExprHasSubLink_opt(q.limitCount)?;
    Ok(Some(outer))
}

fn split_tlist<'mcx>(
    mcx: Mcx<'mcx>,
    tlist: &NodeList<'mcx>,
    group_refs: &[u32],
    push: &mut dyn FnMut(Node<'mcx>, &'mcx str, u32) -> PgResult<i16>,
) -> PgResult<NodeList<'mcx>> {
    let mut out = NodeList::with_capacity(mcx, tlist.len())?;
    for n in tlist.iter() {
        let te = n.as_target_entry().expect("tlist cell");
        let name = te.resname.unwrap_or("?column?");
        let expr = if clauses::contain_window_function(te.expr)? {
            split_expr(mcx, te.expr, name, push)?
        } else {
            let groupref = if group_refs.contains(&te.ressortgroupref) { te.ressortgroupref } else { 0 };
            let k = push(te.expr, name, groupref)?;
            column_var(mcx, te.expr, k)?
        };
        out.lappend(mcx, Node::mk(mcx, TargetEntry { expr, ..*te })?)?;
    }
    Ok(out)
}

fn split_expr<'mcx>(mcx: Mcx<'mcx>, node: Node<'mcx>, name: &'mcx str, push: &mut dyn FnMut(Node<'mcx>, &'mcx str, u32) -> PgResult<i16>) -> PgResult<Node<'mcx>> {
    if !clauses::contain_window_function(node)? {
        let k = push(node, name, 0)?;
        return column_var(mcx, node, k);
    }
    let mut m = |n: Node<'mcx>| split_expr(mcx, n, name, push).map(Some);
    Ok(nodes_core::expression_tree_mutator(mcx, node, &mut m)?.unwrap_or(node))
}

fn column_var<'mcx>(mcx: Mcx<'mcx>, src: Node<'mcx>, k: i16) -> PgResult<Node<'mcx>> {
    let (ty, tm) = ::costsize::expr_type_typmod(src);
    Node::mk_var(mcx, 1, k, ty, tm, crate::pathkeys::expr_collation(src), 0)
}

/// The skeleton's column Vars carry the union's agreed typmod (-1 where the arms differ).
fn fix_outer_typmods<'mcx>(mcx: Mcx<'mcx>, tlist: &NodeList<'mcx>, coltypes: &[(u32, i32, u32)]) -> PgResult<NodeList<'mcx>> {
    let mut out = NodeList::with_capacity(mcx, tlist.len())?;
    for n in tlist.iter() {
        let te = n.as_target_entry().expect("tlist cell");
        match fix_var(mcx, te.expr, coltypes)? {
            Some(expr) => out.lappend(mcx, Node::mk(mcx, TargetEntry { expr, ..*te })?)?,
            None => out.lappend(mcx, n)?,
        }
    }
    Ok(out)
}

fn fix_var<'mcx>(mcx: Mcx<'mcx>, n: Node<'mcx>, coltypes: &[(u32, i32, u32)]) -> PgResult<Option<Node<'mcx>>> {
    match n.as_var() {
        Some(v) if v.varlevelsup == 0 && v.varno == 1 => {
            let (ty, tm, coll) = coltypes[v.varattno as usize - 1];
            if tm == v.vartypmod {
                return Ok(None);
            }
            Ok(Some(Node::mk_var(mcx, 1, v.varattno, ty, tm, coll, 0)?))
        }
        _ => nodes_core::expression_tree_mutator(mcx, n, &mut |n| fix_var(mcx, n, coltypes)),
    }
}

struct Subst<'a> {
    group_rti: i32,
    refs: &'a [u32],
    set: &'a [i32],
    newattno: &'a [i16],
    sublevels_up: u32,
}

fn null_const<'mcx>(mcx: Mcx<'mcx>, ty: u32, typmod: i32, coll: u32) -> PgResult<Node<'mcx>> {
    let (len, byval) = lsyscache::get_typlenbyval(ty)?;
    Node::mk_const(mcx, ty, typmod, coll, len as i32, datum::Datum::from_i32(0), true, byval)
}

fn subst<'mcx>(ctx: &mut Subst<'_>, mcx: Mcx<'mcx>, node: Node<'mcx>) -> PgResult<Option<Node<'mcx>>> {
    match node.node_tag() {
        NodeTag::T_Var => {
            let v = node.as_var().expect("Var");
            if ctx.group_rti == 0 || v.varlevelsup != ctx.sublevels_up || v.varno != ctx.group_rti {
                return Ok(None);
            }
            let i = v.varattno as usize - 1;
            if ctx.set.contains(&(ctx.refs[i] as i32)) {
                if v.varnullingrels.is_empty() && v.varattno == ctx.newattno[i] {
                    return Ok(None);
                }
                return Ok(Some(Node::mk(mcx, Var { varattno: ctx.newattno[i], varattnosyn: ctx.newattno[i], varnullingrels: types_nodes::Bitmapset::empty(), ..*v })?));
            }
            null_const(mcx, v.vartype, v.vartypmod, v.varcollid).map(Some)
        }
        NodeTag::T_GroupingFunc => {
            let g: &GroupingFunc<'mcx> = node.as_grouping_func().expect("GroupingFunc");
            if g.agglevelsup != ctx.sublevels_up {
                return Ok(None);
            }
            let mut v: i32 = 0;
            for r in g.refs.iter() {
                v = (v << 1) | i32::from(!ctx.set.contains(&r));
            }
            Ok(Some(Node::mk_const(mcx, INT4OID, -1, 0, 4, datum::Datum::from_i32(v), false, true)?))
        }
        NodeTag::T_SubLink => {
            let sl = node.as_sub_link().expect("SubLink");
            subst_query(ctx, mcx, sl.subselect)?;
            nodes_core::expression_tree_mutator(mcx, node, &mut |n| subst(ctx, mcx, n))
        }
        NodeTag::T_Query => {
            subst_query(ctx, mcx, node)?;
            Ok(None)
        }
        _ => nodes_core::expression_tree_mutator(mcx, node, &mut |n| subst(ctx, mcx, n)),
    }
}

fn subst_query<'mcx>(ctx: &mut Subst<'_>, mcx: Mcx<'mcx>, qnode: Node<'mcx>) -> PgResult<()> {
    ctx.sublevels_up += 1;
    nodes_core::query_tree_mutator(mcx, qnode, &mut |n| subst(ctx, mcx, n), nodes_core::QTW_DONT_COPY_QUERY)?;
    ctx.sublevels_up -= 1;
    Ok(())
}

fn str_in<'mcx>(mcx: Mcx<'mcx>, s: &str) -> PgResult<&'mcx str> {
    let bytes = mcx::slice_borrow_in(mcx, s.as_bytes())?;
    // SAFETY: a byte-for-byte copy of a &str.
    Ok(unsafe { core::str::from_utf8_unchecked(bytes) })
}

fn bump_own_cte_refs<'mcx>(arm: Node<'mcx>) -> PgResult<()> {
    struct Bump {
        depth: u32,
    }
    impl<'mcx> nodes_core::NodeWalker<'mcx> for Bump {
        fn visit(&mut self, node: Node<'mcx>) -> PgResult<bool> {
            match node.node_tag() {
                NodeTag::T_RangeTblEntry => {
                    let depth = self.depth;
                    // SAFETY: exclusive tree (the arm is a fresh deep copy).
                    unsafe {
                        node.with_mut::<RangeTblEntry, _>(|rte| {
                            if rte.rtekind == RTEKind::RTE_CTE && rte.ctelevelsup == depth {
                                rte.ctelevelsup += 2;
                            }
                        })
                    }
                    .expect("RangeTblEntry");
                    Ok(false)
                }
                NodeTag::T_Query => self.visit_query_ref(node.as_query().expect("Query")),
                _ => nodes_core::expression_tree_walker(node, self),
            }
        }
        fn visit_query_ref(&mut self, q: &'mcx Query<'mcx>) -> PgResult<bool> {
            self.depth += 1;
            let r = nodes_core::query_tree_walker(q, self, nodes_core::QTW_EXAMINE_RTES_BEFORE)?;
            self.depth -= 1;
            Ok(r)
        }
    }
    let mut w = Bump { depth: 0 };
    nodes_core::query_tree_walker(arm.as_query().expect("Query"), &mut w, nodes_core::QTW_EXAMINE_RTES_BEFORE)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use types_nodes::parsenodes::{GroupingSetKind, SortGroupClause};
    use types_nodes::primnodes::Aggref;

    const TBL: u32 = 16384;
    const INT4: u32 = 23;
    const INT8: u32 = 20;
    const INT4EQ: u32 = 96;
    const INT4LT: u32 = 97;
    const COUNT_STAR: u32 = 2803;

    fn cx() -> mcx::MemoryContext {
        crate::tests::install_fixtures();
        mcx::MemoryContext::new("groupingsets_union")
    }

    fn int4(mcx: Mcx<'_>, v: i32) -> Node<'_> {
        Node::mk_const(mcx, INT4, -1, 0, 4, datum::Datum::from_i32(v), false, true).unwrap()
    }

    fn sgc(mcx: Mcx<'_>, r: u32) -> Node<'_> {
        Node::mk(mcx, SortGroupClause { tleSortGroupRef: r, eqop: INT4EQ, sortop: INT4LT, reverse_sort: false, nulls_first: false, hashable: true }).unwrap()
    }

    fn group_var<'m>(mcx: Mcx<'m>, attno: i16, nullable: bool) -> Node<'m> {
        let varnullingrels = if nullable { types_nodes::Bitmapset::make_singleton(mcx, 2).unwrap() } else { types_nodes::Bitmapset::empty() };
        Node::mk(mcx, Var { varno: 2, varattno: attno, vartype: INT4, vartypmod: -1, varcollid: 0, varnullingrels, varlevelsup: 0, varnosyn: 2, varattnosyn: attno, location: -1, ..Default::default() }).unwrap()
    }

    fn te<'m>(mcx: Mcx<'m>, expr: Node<'m>, resno: i16, name: &'m str, r: u32) -> Node<'m> {
        Node::mk(mcx, TargetEntry { expr, resno, resname: Some(name), ressortgroupref: r, resorigtbl: 0, resorigcol: 0, resjunk: false }).unwrap()
    }

    fn simple<'m>(mcx: Mcx<'m>, r: i32) -> Node<'m> {
        Node::mk_grouping_set(mcx, GroupingSetKind::GROUPING_SET_SIMPLE, NodeList::make1(mcx, Node::mk_integer(mcx, r).unwrap()).unwrap(), -1).unwrap()
    }

    fn rollup(mcx: Mcx<'_>) -> Query<'_> {
        let rel = Node::mk(mcx, RangeTblEntry { rtekind: RTEKind::RTE_RELATION, relid: TBL, relkind: b'r', rellockmode: 1, inFromCl: true, ..Default::default() }).unwrap();
        let mut groupexprs = NodeList::nil();
        groupexprs.lappend(mcx, Node::mk_var(mcx, 1, 1, INT4, -1, 0, 0).unwrap()).unwrap();
        groupexprs.lappend(mcx, Node::mk_var(mcx, 1, 2, INT4, -1, 0, 0).unwrap()).unwrap();
        let mut colnames = NodeList::nil();
        colnames.lappend(mcx, Node::mk_string(mcx, "a").unwrap()).unwrap();
        colnames.lappend(mcx, Node::mk_string(mcx, "b").unwrap()).unwrap();
        let eref = mcx::alloc_leak_in(mcx, Alias { aliasname: Some("*GROUP*"), colnames }).unwrap();
        let group = Node::mk(mcx, RangeTblEntry { rtekind: RTEKind::RTE_GROUP, groupexprs, eref: Some(eref), ..Default::default() }).unwrap();
        let mut refs = IntList::nil();
        refs.lappend(mcx, 1).unwrap();
        refs.lappend(mcx, 2).unwrap();
        let grouping = Node::mk(mcx, GroupingFunc { args: NodeList::make2(mcx, group_var(mcx, 1, false), group_var(mcx, 2, true)).unwrap(), refs, cols: IntList::nil(), agglevelsup: 0, location: -1 }).unwrap();
        let count = Node::mk(mcx, Aggref { aggfnoid: COUNT_STAR, aggtype: INT8, aggstar: true, ..Aggref::default() }).unwrap();
        let mut tlist = NodeList::nil();
        tlist.lappend(mcx, te(mcx, group_var(mcx, 1, false), 1, "a", 1)).unwrap();
        tlist.lappend(mcx, te(mcx, group_var(mcx, 2, true), 2, "b", 2)).unwrap();
        tlist.lappend(mcx, te(mcx, grouping, 3, "grouping", 0)).unwrap();
        tlist.lappend(mcx, te(mcx, count, 4, "count", 0)).unwrap();
        let rollup = Node::mk_grouping_set(mcx, GroupingSetKind::GROUPING_SET_ROLLUP, NodeList::make2(mcx, simple(mcx, 1), simple(mcx, 2)).unwrap(), -1).unwrap();
        Query {
            commandType: CmdType::CMD_SELECT,
            canSetTag: true,
            hasAggs: true,
            hasGroupRTE: true,
            rtable: NodeList::make2(mcx, rel, group).unwrap(),
            jointree: Some(mcx::alloc_leak_in(mcx, FromExpr { fromlist: NodeList::make1(mcx, Node::mk_range_tbl_ref(mcx, 1).unwrap()).unwrap(), quals: None }).unwrap()),
            targetList: tlist,
            groupClause: NodeList::make2(mcx, sgc(mcx, 1), sgc(mcx, 2)).unwrap(),
            groupingSets: NodeList::make1(mcx, rollup).unwrap(),
            sortClause: NodeList::make1(mcx, sgc(mcx, 1)).unwrap(),
            ..Query::default()
        }
    }

    fn arm_by_keys<'m>(union: &Query<'m>, keys: usize) -> &'m Query<'m> {
        union
            .rtable
            .iter()
            .map(|n| n.as_range_tbl_entry().unwrap().subquery.unwrap())
            .find(|a| a.groupClause.len() == keys)
            .unwrap_or_else(|| panic!("no arm with {keys} keys"))
    }

    fn const_of(n: Node<'_>) -> (bool, i32) {
        let c = n.as_const().expect("a Const");
        (c.constisnull, c.constvalue.as_i32())
    }

    #[test]
    fn rollup_is_three_arms_under_the_level() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let q = rollup(mcx);
        let outer = alternative(mcx, &q).unwrap().expect("rewritten");
        assert_eq!(outer.rtable.len(), 1);
        assert!(outer.groupingSets.is_nil() && outer.groupClause.is_nil() && !outer.hasAggs && !outer.hasGroupRTE);
        assert_eq!(outer.sortClause.len(), 1, "the level's ORDER BY stays above the union");
        assert_eq!(outer.targetList.len(), 4);
        for (k, n) in outer.targetList.iter().enumerate() {
            let te = n.as_target_entry().unwrap();
            let v = te.expr.as_var().expect("a column of the union");
            assert_eq!((v.varno, v.varattno as usize), (1, k + 1));
        }
        let union = outer.rtable.nth(0).as_range_tbl_entry().unwrap().subquery.unwrap();
        assert!(union.setOperations.is_some());
        assert_eq!(union.rtable.len(), 3);
        assert_eq!(union.targetList.len(), 4);

        let both = arm_by_keys(union, 2);
        assert!(both.groupingSets.is_nil());
        assert_eq!(const_of(both.targetList.nth(2).as_target_entry().unwrap().expr), (false, 0));
        let b = both.targetList.nth(1).as_target_entry().unwrap();
        assert_eq!(b.ressortgroupref, 2);
        let bv = b.expr.as_var().unwrap();
        assert!(bv.varnullingrels.is_empty() && bv.varno == 2 && bv.varattno == 2, "in the set: the GROUP Var, no longer nullable");

        let a_only = arm_by_keys(union, 1);
        assert_eq!(const_of(a_only.targetList.nth(2).as_target_entry().unwrap().expr), (false, 1));
        let b = a_only.targetList.nth(1).as_target_entry().unwrap();
        assert_eq!(b.ressortgroupref, 0);
        assert_eq!(const_of(b.expr), (true, 0), "outside the set: NULL");
        let group = a_only.rtable.nth(1).as_range_tbl_entry().unwrap();
        assert_eq!(group.groupexprs.len(), 1, "the arm's GROUP RTE is parallel to its GROUP BY");

        let none = arm_by_keys(union, 0);
        assert!(none.groupingSets.is_nil() && none.groupClause.is_nil(), "the empty set over aggregates is the plain aggregate");
        assert_eq!(const_of(none.targetList.nth(2).as_target_entry().unwrap().expr), (false, 3));
        assert_eq!(const_of(none.targetList.nth(0).as_target_entry().unwrap().expr), (true, 0));
        assert!(none.targetList.nth(3).as_target_entry().unwrap().expr.as_aggref().is_some());
    }

    #[test]
    fn the_empty_set_without_aggregates_stays_group_by_unit() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let mut q = rollup(mcx);
        q.targetList = NodeList::make2(mcx, q.targetList.nth(0), q.targetList.nth(1)).unwrap();
        q.hasAggs = false;
        let outer = alternative(mcx, &q).unwrap().expect("rewritten");
        let union = outer.rtable.nth(0).as_range_tbl_entry().unwrap().subquery.unwrap();
        let none = arm_by_keys(union, 0);
        assert_eq!(none.groupingSets.len(), 1, "GROUP BY (): one group over an empty input too");
        assert!(none.groupingSets.nth(0).as_grouping_set().is_some_and(|g| g.kind == GroupingSetKind::GROUPING_SET_EMPTY));
    }

    #[test]
    fn a_set_returning_grouping_expression_is_not_rewritten() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let q = rollup(mcx);
        let srf = Node::mk(mcx, types_nodes::primnodes::FuncExpr { funcid: 1067, funcresulttype: INT4, funcretset: true, funcvariadic: false, funcformat: types_nodes::primnodes::CoercionForm::COERCE_EXPLICIT_CALL, funccollid: 0, inputcollid: 0, args: NodeList::make2(mcx, int4(mcx, 1), int4(mcx, 2)).unwrap(), location: -1 }).unwrap();
        let group = q.rtable.nth(1);
        let mut groupexprs = NodeList::nil();
        groupexprs.lappend(mcx, srf).unwrap();
        groupexprs.lappend(mcx, Node::mk_var(mcx, 1, 2, INT4, -1, 0, 0).unwrap()).unwrap();
        // SAFETY: the test's own tree.
        unsafe { group.with_mut::<RangeTblEntry, _>(|r| r.groupexprs = groupexprs) }.unwrap();
        assert!(alternative(mcx, &q).unwrap().is_none());
    }

    #[test]
    fn a_level_without_sets_is_untouched() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let mut q = rollup(mcx);
        q.groupingSets = NodeList::nil();
        assert!(alternative(mcx, &q).unwrap().is_none());
    }
}
