//! D4076: a grouping expression the planner dropped from the keys keys the group again. PostgreSQL 16+ drops a GROUP BY
//! expression whose EquivalenceClass holds a constant or an earlier key (`WHERE lower(e_kind) = 'click' GROUP BY
//! lower(e_kind), date_trunc('week', e_day)`: processed_groupClause, remove_redundant), and make_group_input_target then
//! passes up the expression's columns instead of its value, so the node computes `lower(OUTER.e_kind)` over the raw
//! `e_kind`. That value is one per group (the planner's ground for the drop); the column is not: it is no GROUP BY column,
//! no primary key determines it and no filter pins it, so D416's `fd` arm (`duplicate_keys`) must not key it (keyed, it
//! split each group by its values, `Click` and `click`). Such a column's every read outside the aggregates sits inside
//! a target entry the node computes under a ressortgroupref (`dropped`); the innermost such expression of each read keys
//! the group again, as PostgreSQL 15 kept it: a pass-through projection appends it to the node's input, and the lists
//! read it from the key (`rekey`). A column read bare anywhere is grouped or determined (the parser admitted it alone),
//! so it keeps `duplicate_keys`' arms. Where no key can be had the node refuses by name: it never splits a group.
use super::*;

use types_nodes::primnodes::Var;

/// The refusal where a dropped grouping expression cannot key the group again (no node allocator, no input projection).
const NO_REKEY: &str = "a GROUP BY expression the planner dropped as redundant, over a column the keys do not hold (D4076)";

/// `dropped`'s answer: the non-key input columns the lists read only inside a dropped grouping or sort expression
/// (`covered`), and the innermost such expression of each of their reads (`exprs`, each once).
pub(super) struct Dropped<'p> {
    pub covered: ArenaVec<i16>,
    pub exprs: ArenaVec<Node<'p>>,
}

/// The input columns outside `keyed` that `tlist` and `qual` read outside the aggregates only inside an entry of `tlist`
/// the node computes from its input under a ressortgroupref (an expression, no bare input column; no aggregate, sub-plan
/// or GROUPING(); nothing that acts), and those expressions; none without `fd` (the node groups the query's own GROUP BY,
/// `query_grouping`) or a key, or over a node kind the walk does not know.
/// D4163, `alone`: a grouping node with no key left (numCols 0, never AGG_PLAIN: `WHERE lower(e_kind) = 'click' GROUP
/// BY lower(e_kind)`, every GROUP BY expression dropped) answers too, as D345 keys a pinned column of such a node, when
/// the dropped expressions are all the lists read outside the aggregates (no column is read bare or inside another
/// expression) and each one's equal values are one image (catalog keysem `one_image`): the planner dropped it as equal
/// to a constant, so every row computes the same image and the key's groups are the node's one group. Any other read,
/// or a type whose equal values show apart (a numeric's scale, a text under a nondeterministic collation: PostgreSQL
/// shows the first row's), answers none.
pub(super) fn dropped<'p>(b: &Builder<'p, '_>, tlist: &'p NodeList<'p>, qual: &'p NodeList<'p>, keyed: &[i16], fd: bool, alone: bool) -> Dropped<'p> {
    let mut out = Dropped { covered: ArenaVec::new_in(b.arena), exprs: ArenaVec::new_in(b.arena) }; // alloc: Cold
    if !fd || (keyed.is_empty() && !alone) {
        return out;
    }
    let free = |x: Node<'p>| x.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno > 0 && !keyed.contains(&v.varattno)).map(|v| v.varattno);
    let cx = crate::effects::Cx::new(b.stmt, b.facts, b.arena);
    let mut cands: ArenaVec<Node<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    for te in tlist.iter().filter_map(|n| n.as_target_entry()) {
        let e = te.expr;
        if te.ressortgroupref == 0 || relabels(e).as_var().is_some() || cands.iter().any(|&c| types_nodes::equal(c, e)) {
            continue;
        }
        let mut reads = false;
        let plain = expr::walk(e, &mut |x| {
            reads |= free(x).is_some();
            !matches!(x.node_tag(), NodeTag::T_Aggref | NodeTag::T_SubPlan | NodeTag::T_AlternativeSubPlan | NodeTag::T_GroupingFunc | NodeTag::T_WindowFunc)
        });
        if reads && plain && !crate::effects::expr_acts(&cx, e) {
            cands.push(e);
        }
    }
    if cands.is_empty() {
        return out;
    }
    // each read of a free column with the innermost candidate it sits in (none: outside every one)
    let mut reads: ArenaVec<(i16, Option<usize>)> = ArenaVec::new_in(b.arena); // alloc: Cold
    let mut known = true;
    for n in tlist.iter().filter_map(|n| n.as_target_entry().map(|t| t.expr)).chain(qual.iter()) {
        known &= reads_in(n, None, &cands, &free, &mut reads);
    }
    if !known {
        return out;
    }
    for &(c, _) in reads.iter() {
        if !out.covered.contains(&c) && reads.iter().all(|&(x, at)| x != c || at.is_some()) {
            out.covered.push(c);
        }
    }
    let mut taken: ArenaVec<usize> = ArenaVec::new_in(b.arena); // alloc: Cold
    for &(c, at) in reads.iter() {
        if let Some(i) = at.filter(|i| out.covered.contains(&c) && !taken.contains(i)) {
            taken.push(i);
            out.exprs.push(cands[i]);
        }
    }
    if keyed.is_empty() {
        // D4163: with no key left the expressions are the node's only keys
        let whole = reads.iter().all(|&(_, at)| at.is_some());
        let one = |e: Node<'p>| crate::catalog::keysem::one_image(b.facts, nodes_core::expr_type(e), nodes_core::expr_collation(e));
        if !whole || !out.exprs.iter().all(|&e| one(e)) {
            return Dropped { covered: ArenaVec::new_in(b.arena), exprs: ArenaVec::new_in(b.arena) }; // alloc: Cold
        }
    }
    out
}

/// `dropped`'s walk: the free columns `n` reads outside the aggregates, each with the innermost of `cands` it sits in
/// (`at` on entry: the one around `n`). False past a node kind `expr::kids` does not know.
fn reads_in<'p>(n: Node<'p>, at: Option<usize>, cands: &[Node<'p>], free: &dyn Fn(Node<'p>) -> Option<i16>, out: &mut ArenaVec<(i16, Option<usize>)>) -> bool {
    crate::walk::stack_check(); // D3748
    if n.node_tag() == NodeTag::T_Aggref {
        return true;
    }
    let at = cands.iter().position(|&c| types_nodes::equal(c, n)).or(at);
    if let Some(c) = free(n) {
        out.push((c, at));
    }
    let mut ok = true;
    let known = expr::kids(n, &mut |k| ok &= reads_in(k, at, cands, free, out));
    ok && known
}

/// Key each of `d.exprs` that reads a column `sh.dropped` names and no later arm keyed (`pk_keys`), and answer `tlist`
/// and `qual` reading those expressions from the keys. The expressions read the node's input row (`OUTER_VAR`), so a
/// projection after `child`'s ops passes that row on and appends them; a key's word is the group's one value.
pub(super) fn rekey<'p>(b: &Builder<'p, '_>, child: &mut Open<'p>, sh: &mut Shape<'p>, d: &Dropped<'p>, tlist: &'p NodeList<'p>, qual: &'p NodeList<'p>, input: Option<&'p NodeList<'p>>, k: Kind, id: i32) -> Result<(&'p NodeList<'p>, &'p NodeList<'p>), Refusal> {
    let left = |c: i16| sh.dropped.contains(&c) && !sh.keys.iter().any(|key| key.resno == c);
    let mut exprs: ArenaVec<Node<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    for &e in d.exprs.iter() {
        let mut reads = false;
        expr::walk(e, &mut |x| {
            reads |= x.as_var().is_some_and(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && left(v.varattno));
            true
        });
        if reads {
            exprs.push(e);
        }
    }
    if exprs.is_empty() {
        return Ok((tlist, qual));
    }
    let refuse = || Refusal::shape(k, id, NO_REKEY);
    let mcx = b.alloc.ok_or_else(refuse)?;
    let oom = |_| refuse();
    // the input row: the last projection (or a Sort's target list after it) of `child`; D4163, `input`: or the input
    // plan node's target list, the row the node's OUTER_VAR references number (a union's rows, an absorbed Sort's:
    // `child` then projects nothing of its own)
    let row = child.ops.iter().rev().find_map(|op| match op {
        Op::Project(l) => Some(*l),
        Op::Sort(s) => Some(s.tlist),
        _ => None,
    });
    let row = row.or(input).ok_or_else(refuse)?;
    let mut list = NodeList::with_capacity(mcx, row.len() + exprs.len()).map_err(oom)?;
    for (i, n) in row.iter().enumerate() {
        let te = n.as_target_entry().filter(|t| t.resno as usize == i + 1).ok_or_else(refuse)?;
        let syn = relabels(te.expr).as_var().map_or((0, 0), |v| (v.varnosyn, v.varattnosyn));
        let v = pass_var(mcx, te.resno, te.expr, syn).map_err(oom)?;
        list.lappend(mcx, Node::mk_target_entry(mcx, v, te.resno, te.resname, te.resjunk).map_err(oom)?).map_err(oom)?;
    }
    let mut swaps: ArenaVec<(Node<'p>, Node<'p>)> = ArenaVec::new_in(b.arena); // alloc: Cold
    for (j, &e) in exprs.iter().enumerate() {
        let resno = i16::try_from(row.len() + j + 1).map_err(|_| refuse())?;
        list.lappend(mcx, Node::mk_target_entry(mcx, e, resno, None, false).map_err(oom)?).map_err(oom)?;
        swaps.push((e, pass_var(mcx, resno, e, (0, 0)).map_err(oom)?));
    }
    child.push(Op::Project(Node::mk_list(mcx, list).map_err(oom)?.as_list().ok_or_else(refuse)?));
    for &(e, v) in swaps.iter() {
        fires::fire("agg.rekey_dropped"); // shapecheck fire signal (D4076): a dropped grouping expression keys the group again
        sh.key(b, child, v.as_var().map_or(0, |v| v.varattno), nodes_core::expr_collation(e), k, id)?;
    }
    let swap = |l: &'p NodeList<'p>| -> Result<&'p NodeList<'p>, Refusal> {
        let out = nodes_core::mutate_list_dyn(mcx, l, &mut |n| swap_in(mcx, n, &swaps)).map_err(oom)?;
        match out {
            Some(o) => Node::mk_list(mcx, o).map_err(oom)?.as_list().ok_or_else(refuse),
            None => Ok(l),
        }
    };
    Ok((swap(tlist)?, swap(qual)?))
}

/// An `OUTER_VAR` reference to column `resno` of the row before, typed as `e` (its syntactic column `syn`, or none).
fn pass_var<'p>(mcx: mcx::Mcx<'p>, resno: i16, e: Node<'p>, syn: (u32, i16)) -> types_error::PgResult<Node<'p>> {
    let v = Var {
        varno: OUTER_VAR,
        varattno: resno,
        vartype: nodes_core::expr_type(e),
        vartypmod: nodes_core::expr_typmod(e),
        varcollid: nodes_core::expr_collation(e),
        varlevelsup: 0,
        varnosyn: syn.0,
        varattnosyn: syn.1,
        location: -1,
        ..Var::default()
    };
    Node::mk(mcx, v)
}

/// `n` with each sub-expression outside the aggregates `equal` to a re-keyed expression as its key's column.
fn swap_in<'p>(mcx: mcx::Mcx<'p>, n: Node<'p>, swaps: &[(Node<'p>, Node<'p>)]) -> types_error::PgResult<Option<Node<'p>>> {
    crate::walk::stack_check(); // D3748
    if n.node_tag() == NodeTag::T_Aggref {
        return Ok(None);
    }
    if let Some(&(_, v)) = swaps.iter().find(|(e, _)| types_nodes::equal(n, *e)) {
        return Ok(Some(v));
    }
    nodes_core::expression_tree_mutator_dyn(mcx, n, &mut |y| swap_in(mcx, y, swaps))
}

/// D4076 under a window: make_window_input_target flattens a GROUP BY expression the planner dropped (no longer in
/// processed_groupClause) to its columns, so the node emits the bare column (no ressortgroupref) and the WindowAgg above
/// computes the expression. The bare non-key columns of `tlist` the nodes above read only so (`Read::Grouped`, facts
/// demand.rs: inside a target entry a WindowAgg computes under a ressortgroupref, one value per group): the node must not
/// key them (each group would split by their values); they ride a `min` cell (passenger.rs `rewrite`: the reader's
/// expression over any member is the group's one value). None without `fd` or a key.
pub(super) fn window_dropped<'p>(b: &Builder<'p, '_>, plan: &'p Plan<'p>, tlist: &'p NodeList<'p>, keyed: &[i16], fd: bool) -> ArenaVec<i16> {
    let mut out: ArenaVec<i16> = ArenaVec::new_in(b.arena); // alloc: Cold
    if !fd || keyed.is_empty() {
        return out;
    }
    for te in tlist.iter().filter_map(|n| n.as_target_entry()) {
        let col = te.expr.as_var().filter(|v| te.ressortgroupref == 0 && v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno > 0 && !keyed.contains(&v.varattno));
        if let Some(v) = col.filter(|_| reads_above(b, plan, te.resno) == Read::Grouped) {
            if !out.contains(&v.varattno) {
                out.push(v.varattno);
            }
        }
    }
    out
}
