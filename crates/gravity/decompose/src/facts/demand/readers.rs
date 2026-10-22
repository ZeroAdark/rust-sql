//! D514 (V4R-167): an Agg's input column that its target list emits bare but that is no grouping key (a semi join's
//! unique-ification `(f1, f2) IN (SELECT f2, f3::int4 ..)` groups by `f2, int4(f3)` and emits `f3`) rides a
//! representative slot: the target entry becomes `min(col)` over the group, an ordinary aggregate cell, so the group's
//! emit needs no change. The order is irrelevant: `min` returns one of the group's own values (a strict `*smaller`
//! returns an argument, the non-NULL ones first; all NULL gives NULL, itself a member; a float NaN comes back as the
//! canonical NaN, equal to it), and PG's own value is its first hashed tuple's, an arbitrary member too. A text
//! column's minimum is taken in "C" (byte order: a member all the same).
//!
//! Guard (local, provable; anything else keeps the "a column the row does not carry" refusal): every reader above the
//! Agg (a Hash then its join, or the join directly: JOIN_UNIQUE_OUTER) reads the column only inside an expression
//! structurally equal (`equal`, the tracked column standing for the Agg input's expression) to one of the Agg's
//! grouping-key expressions, and the Agg's qual does not read it. Then every reader sees `f(rep)`, and `f(rep)` is the
//! group's key `f(x)` for any member `x` (the group is keyed by `f`). NULLs: the rep is a member either way, so
//! `f(rep)` is the key; for a strict `f` a NULL column's key is NULL, so a group holding one holds (among its keyed
//! rows) only rows whose `f` is NULL too, and for a non-strict `f` (coalesce) `f(NULL)` is the same key as the rest.
//! Volatility: PG emits an arbitrary member (its first hashed tuple) and the reader re-evaluates
//! `f` over it, so `f(min member)` is one of PG's own executions whatever `f`'s volatility; and a semi join never
//! unique-ifies a volatile rhs anyway (compute_semijoin_info refuses one).
//!
//! V4R-540: a SEMI hash join's bare inner column that no reader above reads (an equivalence-class member
//! the planner keeps for a join above) becomes a NULL of its type (`semi_unread`), and an Agg over that join emits its
//! bare passenger over the column as the same NULL (`rewrite`). Only in a statement with no build the lowerer may make
//! lazy (`lazy_build`: D2147 / D2168, an aggregate over one crashes the backend); such a statement keeps its refusal.
//! A CTE in the statement is no bar (D3201): every proof is local to the readers the statement map finds, and a CTE's
//!
//! This is the reader-walk family, moved unchanged from facts/passenger.rs into the column-demand home: the walk
//! from a node to the readers above it that hand a column on bare (a Hash to its join, a Sort or Material, a join's or
//! an ungrouped Agg's target list) and test how each reads it. Their next step reads the top-down `Demand` instead.
use datum::Datum;
use types_nodes::list::NodeList;
use types_nodes::plannodes::{Agg, HashJoin, NestLoop, NestLoopParam, Plan};
use types_nodes::primnodes::{Aggref, NullTest, NullTestType, Param, ParamKind, Var, AGGSPLIT_SIMPLE, INNER_VAR, OUTER_VAR};
use types_nodes::{Node, NodeTag};

use gravity_program::ArenaVec;

use crate::expr;
use crate::graph::{Op, Source};
use crate::ledger::Kind;
use crate::admit::Refusal;
use crate::walk::{Builder, Open};

const C_COLLATION_OID: u32 = 950;

/// `tlist` with each guarded passenger's bare entry as `min(col)` (aggno `arena_cols`'), or `tlist` itself.
/// `keyed`: the input columns the node's keys already carry; `above`: the ones a window above reads only inside a GROUP
/// BY expression the planner dropped (agg dropped.rs `window_dropped`, D4076), each a passenger with no further guard.
pub fn rewrite<'p>(b: &mut Builder<'p, '_>, a: &'p Agg<'p>, keyed: &[i16], above: &[i16], tlist: &'p NodeList<'p>, qual: &'p NodeList<'p>) -> &'p NodeList<'p> {
    let (Some(mcx), Some(child)) = (b.alloc, a.plan.lefttree.and_then(|n| n.as_plan())) else { return tlist };
    let (naggs, kept) = arena_cols(b, a);
    let mut keys: ArenaVec<Node<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    keys.extend(a.grpColIdx.iter().take(a.numCols.max(0) as usize).filter_map(|&c| entry(child, c)));
    let mut next = 0;
    let mut cols: ArenaVec<(i16, Node<'p>)> = ArenaVec::new_in(b.arena); // alloc: Cold
    for l in [tlist, qual] {
        for n in l.iter() {
            walk_out(n, &mut |x| {
                if let Some(ag) = x.as_aggref() {
                    next = next.max(ag.aggno + 1);
                    return false;
                }
                if let Some(v) = x.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno > 0) {
                    if !keyed.contains(&v.varattno) && !cols.iter().any(|c| c.0 == v.varattno) {
                        cols.push((v.varattno, x));
                    }
                }
                true
            });
        }
    }
    if !cols.is_empty() {
        fill_parents(b);
    }
    let mut take: ArenaVec<(i16, Node<'p>)> = ArenaVec::new_in(b.arena); // alloc: Cold
    let semi = a.plan.lefttree.and_then(|n| n.as_hash_join()).and_then(|hj| semi_dead(b, hj));
    let mut nulls: ArenaVec<i16> = ArenaVec::new_in(b.arena); // alloc: Cold
    let mut rows: ArenaVec<(i16, Node<'p>)> = ArenaVec::new_in(b.arena); // alloc: Cold
    let mut left: ArenaVec<(i16, Node<'p>, i16, Node<'p>)> = ArenaVec::new_in(b.arena); // alloc: Cold
    for &(c, var) in cols.iter() {
        let reads = |n: Node<'p>| reads_col(n, OUTER_VAR, &[c]);
        let bare = |t: Node<'p>| t.as_target_entry().is_some_and(|t| t.expr.as_var().is_some_and(|v| v.varno == OUTER_VAR && v.varattno == c));
        if qual.iter().any(reads) || tlist.iter().any(|t| reads(t) && !bare(t)) {
            continue;
        }
        // D4076: the window's expression over any member of the group is the group's one value; a type with no `min`
        // stays bare (the lowerer's "a column the row does not carry"), never keyed
        if above.contains(&c) {
            if min_of(var.as_var().unwrap().vartype).is_some() {
                take.push((c, var));
            }
            continue;
        }
        // V4R-540: the SEMI child's nulled inner column, emitted bare to readers semi_dead proved read none of it
        if semi.as_ref().is_some_and(|d| d.contains(&c)) {
            nulls.push(c);
            continue;
        }
        // IN-LIMIT (modifytable): a SubqueryScan's whole row whose every field is a key, or a REFERENCE-marked relation's
        // ctid / tableoid junk, rides an `any_value` cell
        if any_value().is_some() && a.plan.lefttree.zip(entry(child, c)).is_some_and(|(n, ce)| keyed_row(n, ce, &keys) || junk_origin(n, c).is_some_and(|o| marked_junk(b, o))) {
            rows.push((c, var));
            continue;
        }
        let Some(ce) = entry(child, c) else { continue };
        let mut sub: ArenaVec<Sub<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
        sub.extend(tlist.iter().filter(|&t| bare(t)).filter_map(|t| t.as_target_entry().map(|t| (t.resno, ce))));
        match readers_ok(b, a, &sub, &keys, mcx) {
            Some(true) if min_of(var.as_var().unwrap().vartype).is_some() => take.push((c, var)),
            // D3375 (m7_uniq_passenger's two keys): a column no reader above reads at all rides as a NULL of its type,
            // as `semi_dead`'s does (a value that never shows), inside an Append's member too (`through`, as
            // `unread_above` reads); a group's row carries no passenger cell, so the bare entry would read a column the
            // row does not carry. A group with no key (numCols 0: a plain fold, or a GroupAggregate whose every key the
            // planner dropped) carries its bare columns itself as kept columns (lowerv4 kept.rs: the group's first row),
            // and its kept columns keep the program serial: its entries stay as they were (m6_d798_keyed_hosted #44 at W8)
            _ if a.numCols > 0 && readers_of(b, &a.plan, &sub, &[], mcx, true) == Some(false) => nulls.push(c),
            _ if min_of(var.as_var().unwrap().vartype).is_some() && equiv_passenger(b, a, ce, &sub) => take.push((c, var)),
            _ => left.extend(sub.iter().map(|&(r, ce)| (c, var, r, ce))),
        }
    }
    let (joint, filter) = lockstep(b, a, child, &left, mcx).unwrap_or((ArenaVec::new_in(b.arena), None)); // alloc: Cold
    let carried = |c: i16| keyed.contains(&c) || nulls.contains(&c) || take.iter().chain(rows.iter()).chain(joint.iter()).any(|t| t.0 == c);
    let fed = if cols.is_empty() { ArenaVec::new_in(b.arena) } else { uniq(b, a, tlist, &carried) };
    let tlist = nulled(b, tlist, |te| fed.contains(&te.resno) || te.expr.as_var().is_some_and(|v| v.varno == OUTER_VAR && nulls.contains(&v.varattno)));
    // D2740: a group whose every key the planner dropped numbers its own kept columns after its cells (lowerv4 kept.rs),
    // and aggregates other than exec's 0..naggs would shift the arena's: no cell
    if (take.is_empty() && rows.is_empty() && joint.is_empty()) || naggs != next || (a.aggstrategy != crate::ops::shared::agg::AGG_PLAIN && a.numCols <= 0) {
        return tlist;
    }
    let mut past = naggs + kept.len() as i32;
    let mut m = |t: Node<'p>| -> types_error::PgResult<Option<Node<'p>>> {
        let Some(te) = t.as_target_entry() else { return Ok(None) };
        let pick = |l: &ArenaVec<(i16, Node<'p>)>| te.expr.as_var().and_then(|v| l.iter().find(|p| v.varno == OUTER_VAR && p.0 == v.varattno)).map(|p| p.1);
        let (var, row, aggfilter) = match (pick(&take), pick(&rows), pick(&joint)) {
            (Some(var), _, _) => (var, false, None),
            (None, Some(var), _) => (var, true, None),
            (None, None, Some(var)) => (var, true, filter),
            (None, None, None) => return Ok(None),
        };
        let v = var.as_var().unwrap();
        // D2740: a column outside the walk takes an aggno past it, which a hosted cell refuses by name at run time
        let no = match kept.iter().position(|&k| k == v.varattno) {
            Some(i) => naggs + i as i32,
            None => {
                past += 1;
                past - 1
            }
        };
        let arg = Node::mk_target_entry(mcx, var, 1, None, false)?;
        let coll = if v.varcollid != 0 && !row { C_COLLATION_OID } else { 0 };
        let ag = Aggref {
            aggfnoid: if row { any_value().unwrap() } else { min_of(v.vartype).unwrap() },
            aggtype: v.vartype,
            aggcollid: v.varcollid,
            inputcollid: coll,
            aggtranstype: v.vartype,
            aggargtypes: types_nodes::list::OidList::make1(mcx, v.vartype)?,
            args: NodeList::make1(mcx, arg)?,
            aggfilter,
            aggno: no,
            aggtransno: no,
            ..Aggref::default()
        };
        let e = Node::mk(mcx, ag)?;
        let n = Node::mk_target_entry(mcx, e, te.resno, te.resname, te.resjunk)?;
        Ok(Some(n))
    };
    match nodes_core::mutate_list_dyn(mcx, tlist, &mut m).and_then(|l| l.map(|l| Node::mk_list(mcx, l)).transpose()) {
        Ok(Some(n)) => n.as_list().unwrap_or(tlist),
        _ => tlist,
    }
}

/// D3426: the passengers `left` (with each bare entry's resno) that no reader reads alone, when they ride together: every
/// read of them sits inside an expression equal to one key `k` (`s.id::int8 * s.m + 1` emitting `s.id, s.m`) with all of
/// them substituted at once. D514's per-column `min` would mix two members' values; each rides an `any_value` cell
/// filtered by `k IS NOT NULL` (`k`'s input column) instead. The cells of a group fold the same rows in the same order and
/// keep the first non-NULL one handed (any_value's strict transfn; its combine is the same, so every cell keeps the
/// same partial's), and `k` is strict along each path down to a passenger (`strict_over`): a row whose key is not NULL
/// has every passenger set, so the cells hold one member's values and the readers' `k(rep)` is the group's key. A NULL
/// key's group hands them no row; they are NULL, and `k` over them is NULL, its key. `None`: no such key.
fn lockstep<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>, child: &'p Plan<'p>, left: &[(i16, Node<'p>, i16, Node<'p>)], mcx: mcx::Mcx<'p>) -> Option<(ArenaVec<(i16, Node<'p>)>, Option<Node<'p>>)> {
    let mut cols: ArenaVec<(i16, Node<'p>)> = ArenaVec::new_in(b.arena); // alloc: Cold
    let mut ces: ArenaVec<Node<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    for &(c, var, _, ce) in left {
        if !cols.iter().any(|x| x.0 == c) {
            cols.push((c, var));
            ces.push(ce);
        }
    }
    if cols.len() < 2 || any_value().is_none() {
        return None;
    }
    let mut sub: ArenaVec<Sub<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    sub.extend(left.iter().map(|&(_, _, r, ce)| (r, ce)));
    let mut keys = a.grpColIdx.iter().take(a.numCols.max(0) as usize).filter_map(|&kc| entry(child, kc).map(|k| (kc, k)));
    let (kc, k) = keys.find(|&(_, k)| ces.iter().all(|&ce| strict_over(b, k, &[ce]) == Some(true)) && readers_ok(b, a, &sub, &[k], mcx) == Some(true))?;
    let key = Node::mk_var(mcx, OUTER_VAR, kc, nodes_core::expr_type(k), nodes_core::expr_typmod(k), nodes_core::expr_collation(k), 0).ok()?;
    let test = Node::mk(mcx, NullTest { arg: Some(key), nulltesttype: NullTestType::IS_NOT_NULL, argisrow: false, location: -1 }).ok()?;
    Some((cols, Some(test)))
}

/// D3426: `Some(true)` when `k` reads one of `ces` (a sub-expression equal to it) and every node on each path down to one
/// is a call of a strict function or operator or a relabel, so a NULL there makes `k` NULL; `Some(false)` when it reads
/// none; `None` otherwise, or past a node kind the walker does not know.
fn strict_over<'p>(b: &Builder<'p, '_>, k: Node<'p>, ces: &[Node<'p>]) -> Option<bool> {
    crate::walk::stack_check(); // D3748
    if ces.iter().any(|&c| types_nodes::equal(k, c)) {
        return Some(true);
    }
    let (mut read, mut ok) = (false, true);
    let known = expr::kids(k, &mut |x| match strict_over(b, x, ces) {
        Some(r) => read |= r,
        None => ok = false,
    });
    if !(ok && known) {
        return None;
    }
    let strict = |f: u32| b.facts.func(f).is_some_and(|p| p.strict);
    let through = match k.node_tag() {
        NodeTag::T_FuncExpr => k.as_func_expr().is_some_and(|f| strict(f.funcid)),
        NodeTag::T_OpExpr => k.as_op_expr().is_some_and(|o| strict(o.opfuncid)),
        NodeTag::T_RelabelType => true,
        _ => false,
    };
    (!read || through).then_some(read)
}

/// V4R-540: a SEMI hash join whose target list carries an inner column (an equivalence-class member the
/// planner keeps for a join above): PostgreSQL takes it from the first matching inner tuple (nodeHashjoin.c
/// single_match), join.semi hands on the outer row alone. When no reader above reads it, the entries `semi_dead` names
/// become NULLs of their types, a value that never shows (a Var becomes a Const: no call, raise or order moves);
/// anything else keeps the list, and the refusal "a column the row does not carry".
pub fn semi_unread<'p>(b: &mut Builder<'p, '_>, hj: &'p HashJoin<'p>) -> &'p NodeList<'p> {
    let tlist = &hj.join.plan.targetlist;
    fill_parents(b);
    match semi_dead(b, hj) {
        Some(dead) => nulled(b, tlist, |te| dead.contains(&te.resno)),
        None => tlist,
    }
}

/// D3510: a RIGHT SEMI hash join drained in the marks form hands each build row on with no probe row (lowerv4
/// drain/source.rs: its probe cells NULL). `tlist`, its target list once join.rs `semi_build_keys` read the probe columns
/// its hash clauses make equal to a build key as that key: when no reader above reads the bare user probe columns left
/// (`OUTER_VAR`, a planner equivalence-class member kept for no one), they become NULLs of their types, as `semi_unread`
/// does for a SEMI join's inner columns; anything else keeps the list.
pub fn rsemi_unread<'p>(b: &mut Builder<'p, '_>, hj: &'p HashJoin<'p>, tlist: &'p NodeList<'p>) -> &'p NodeList<'p> {
    fill_parents(b);
    let Some(mcx) = b.alloc else { return tlist };
    if b.parents.as_ref().is_none_or(|p| p.lazy(b)) {
        return tlist;
    }
    let mut sub: ArenaVec<Sub<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    for te in tlist.iter().filter_map(|t| t.as_target_entry()) {
        let Some(v) = te.expr.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattnosyn > 0) else { continue };
        if crate::catalog::typefacts::facts_of(b.facts, v.vartype).is_some() {
            sub.push((te.resno, te.expr));
        }
    }
    if sub.is_empty() || !unread_above(b, &hj.join.plan, &sub, mcx) {
        return tlist;
    }
    nulled(b, tlist, |te| sub.iter().any(|s| s.0 == te.resno))
}

/// V4R-566: the NestLoop's parameters, and its inner leaf rewritten, by the loop's `uniq` decision: each `S` the leaf reads
/// becomes one PARAM_EXEC `id0` bound to the Agg's key output, `id0` replacing P's entries (the other P ids drop); with no
/// decision, the plan's. After the rewrite nothing in the inner may read a P id but the new node, else the loop refuses
/// by name (the passengers are already NULL); a decision that names a refusal refuses by it.
pub fn uniq_params<'p>(b: &Builder<'p, '_>, nl: &'p NestLoop<'p>, inner: &mut Open<'p>) -> Result<&'p NodeList<'p>, Refusal> {
    let (k, id) = (Kind::NestLoop, nl.join.plan.plan_node_id);
    let at = &nl.join.plan as *const Plan<'p> as usize;
    let Some(u) = b.parents.as_ref().and_then(|p| p.uniq.iter().find(|u| u.at == at)) else { return Ok(&nl.nestParams) };
    let sub = u.how.as_ref().map_err(|why| Refusal::shape(k, id, why))?;
    let fail = || Refusal::shape(k, id, UNIQ_APPLY);
    let mcx = b.alloc.ok_or_else(|| fail())?;
    uniq_apply(mcx, nl, sub, inner).map_err(|_| fail())?.ok_or_else(|| fail())
}

fn uniq_apply<'p>(mcx: mcx::Mcx<'p>, nl: &'p NestLoop<'p>, u: &UniqSub<'p>, inner: &mut Open<'p>) -> types_error::PgResult<Option<&'p NodeList<'p>>> {
    let param = Node::mk(mcx, Param { paramkind: ParamKind::PARAM_EXEC, paramid: u.id0, paramtype: u.ty, paramtypmod: -1, paramcollid: 0, location: -1 })?;
    let mut hits = 0;
    let mut swap = |n: Node<'p>| -> types_error::PgResult<Node<'p>> { Ok(swap_s(mcx, n, u.s, param, &mut hits)?.unwrap_or(n)) };
    let Source::Index { keys, recheck, .. } = &mut inner.source else { return Ok(None) };
    for key in keys.iter_mut() {
        key.value = swap(key.value)?;
    }
    let mut l = NodeList::nil();
    for n in recheck.iter() {
        l.lappend(mcx, swap(n)?)?;
    }
    *recheck = Node::mk_list(mcx, l)?.as_list().expect("a list");
    for op in inner.ops.iter_mut() {
        match op {
            Op::Filter(n) => *n = swap(*n)?,
            Op::Project(_) => {}
            _ => return Ok(None),
        }
    }
    let read = |n: Node<'p>| p_read(n, &u.pids, Some(param));
    let Source::Index { keys, recheck, .. } = &inner.source else { return Ok(None) };
    let ops = inner.ops.iter().any(|op| match op {
        Op::Filter(n) => read(*n),
        Op::Project(l) => l.iter().any(read),
        _ => true,
    });
    if hits == 0 || inner.union.is_some() || ops || keys.iter().any(|k| read(k.value)) || recheck.iter().any(read) {
        return Ok(None);
    }
    let mut params = NodeList::nil();
    let mut first = true;
    for p in nl.nestParams.iter() {
        match p.as_nest_loop_param().filter(|x| u.pids.contains(&x.paramno)) {
            None => params.lappend(mcx, p)?,
            Some(_) if first => {
                first = false;
                let v = Node::mk_var(mcx, OUTER_VAR, u.rk, u.ty, -1, 0, 0)?;
                params.lappend(mcx, Node::mk(mcx, NestLoopParam { paramno: u.id0, paramval: v })?)?;
            }
            Some(_) => {}
        }
    }
    Ok(Node::mk_list(mcx, params)?.as_list())
}

/// `n` with each sub-expression `equal` to `s` as `param`, counting the swaps in `hits`.
fn swap_s<'p>(mcx: mcx::Mcx<'p>, n: Node<'p>, s: Node<'p>, param: Node<'p>, hits: &mut u32) -> types_error::PgResult<Option<Node<'p>>> {
    crate::walk::stack_check(); // D3748
    if types_nodes::equal(n, s) {
        *hits += 1;
        return Ok(Some(param));
    }
    nodes_core::expression_tree_mutator_dyn(mcx, n, &mut |y| swap_s(mcx, y, s, param, hits))
}

/// The resnos of a SEMI hash join's inner entries when the statement has no lazy build (`Parents::lazy`, filled by the
/// caller; unfilled: none), every inner read of its target list is a bare entry of a user column (`varattnosyn > 0`:
/// setrefs keeps the syntactic attno, so a ctid / tableoid / whole-row rowmark junk column keeps the refusal) of a type
/// with facts, its own qual reads none, and no reader above reads them (`unread_above`); else none. Every "reads none"
/// here is `unread`'s: a node kind the walker does not know counts as a read.
fn semi_dead<'p>(b: &Builder<'p, '_>, hj: &'p HashJoin<'p>) -> Option<ArenaVec<i16>> {
    let tlist = &hj.join.plan.targetlist;
    let mcx = b.alloc.filter(|_| hj.join.jointype == types_nodes::jointype::JoinType::JOIN_SEMI)?;
    if b.parents.as_ref().is_none_or(|p| p.lazy(b)) {
        return None;
    }
    let inner = |n: Node<'p>| !unread(n, &|v| v.varno == INNER_VAR);
    if hj.join.plan.qual.iter().any(inner) {
        return None;
    }
    let mut resnos: ArenaVec<i16> = ArenaVec::new_in(b.arena); // alloc: Cold
    let mut sub: ArenaVec<Sub<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    for t in tlist.iter() {
        let te = t.as_target_entry()?;
        if !inner(te.expr) {
            continue;
        }
        let v = te.expr.as_var().filter(|v| v.varattnosyn > 0)?;
        crate::catalog::typefacts::facts_of(b.facts, v.vartype)?;
        resnos.push(te.resno);
        sub.push((te.resno, te.expr));
    }
    (!sub.is_empty() && unread_above(b, &hj.join.plan, &sub, mcx)).then_some(resnos)
}

/// `list` with each bare column entry `pick` names as a NULL of the column's type, typmod and collation.
fn nulled<'p>(b: &Builder<'p, '_>, list: &'p NodeList<'p>, pick: impl Fn(&types_nodes::primnodes::TargetEntry<'p>) -> bool) -> &'p NodeList<'p> {
    let Some(mcx) = b.alloc else { return list };
    let mut m = |t: Node<'p>| -> types_error::PgResult<Option<Node<'p>>> {
        let Some(te) = t.as_target_entry().filter(|te| pick(te)) else { return Ok(None) };
        let Some((v, tf)) = te.expr.as_var().and_then(|v| crate::catalog::typefacts::facts_of(b.facts, v.vartype).map(|tf| (v, tf))) else { return Ok(None) };
        let e = Node::mk_const(mcx, v.vartype, v.vartypmod, v.varcollid, tf.typlen as i32, Datum::null(), true, tf.byval)?;
        Ok(Some(Node::mk_target_entry(mcx, e, te.resno, te.resname, te.resjunk)?))
    };
    match nodes_core::mutate_list_dyn(mcx, list, &mut m).and_then(|l| l.map(|l| Node::mk_list(mcx, l)).transpose()) {
        Ok(Some(n)) => n.as_list().unwrap_or(list),
        _ => list,
    }
}

const UNIQ_APPLY: &str = "a unique-ifying Agg's passenger parameters read outside the one key expression the loop's rewrite replaces";

/// A NestLoop's decision (`uniq`), by its Plan's address: the rewrite `uniq_params` makes, or the name it refuses by.
pub struct Uniq<'p> {
    at: usize,
    how: Result<UniqSub<'p>, &'static str>,
}

/// `s`, the key expression over the parameters `pids` the inner leaf reads, becomes parameter `id0` (type `ty`) bound to
/// the Agg's output column `rk` (the key's).
pub struct UniqSub<'p> {
    s: Node<'p>,
    pids: ArenaVec<i32>,
    id0: i32,
    ty: u32,
    rk: i16,
}

/// V4R-566 (W2-NOCOL-UNIQ-PASSENGER-R2): the output entries of a unique-ifying Agg `a` whose passengers the NestLoop above
/// reads only through its parameters, inside one key expression of its inner leaf; they become NULLs of their types and
/// the loop's inner reads the Agg's key instead (`uniq_params`). The loop L: JOIN_INNER, the Agg its outer; the Agg
/// AGG_HASHED, AGGSPLIT_SIMPLE, keyed, no sets / chain / qual / InitPlan, its target list bare input columns (no
/// Aggref). P: L's parameters bound to an uncarried passenger's entry (`carried`: keyed, D514's cell, a SEMI join's unread NULL); L's
/// own lists read none of those entries. L's inner is one IndexScan / IndexOnlyScan with no InitPlan; P is read only in
/// its index clauses, recheck and qual, each read inside a var-free `S` whose parameters are P's or EXTERN, all such `S`
/// `equal`; the target list and ORDER BY read none, no SubPlan. `S` mapped (P -> the Agg's entry -> the child's entry) is
/// one side of a conjunct C its JOIN_INNER child holds on every row it emits (a HashJoin's hash clause; a NestLoop's
/// IndexScan leaf's original index clause, `S` mapped through the leaf's bare target list, the other side a parameter
/// the child binds to its key), the other side key i's child entry (a bare column); C's and the key's operators the
/// same-type image equality (`image_eq_same`) of `S`'s type, no collation, typmod -1 (PostgreSQL's `exprTypmod`, `expr_type::typmod_of`). The Agg groups by key i, so each group's members hold `S` =
/// key i under an equality of one image: PostgreSQL evaluates `S` over one member (its first hashed tuple) and gets the
/// group's key word, which the rewrite binds; no passenger value is read. D3375, the equivalence road: C's operator is
/// key i's grouping operator, the key type's own built-in `=` (`keysem::own_equivalence`: numeric, a collatable type
/// under the key's deterministic collation; a nondeterministic one is not taken, fail closed), and the leaf reads `S` only as an operand of that operator under that collation
/// (`operand_only`). PostgreSQL's `S` over its member is then equal to the key, not always its image, and every leaf
/// clause holds for the key exactly when it holds for that `S`: the leaf returns the same rows (the equal-member ruling:
/// which member's image a group shows is free). A hash / index clause the child ran over the
/// same values raised nothing (the leaf's index expression is IMMUTABLE: `lazy_build` refuses any user or VOLATILE call).
/// A statement holding a SubPlan or InitPlan takes it too (D3286: a SubPlan reads a non-heap inner's cells at the inner's
/// site since D2213's fix, so its old fence here is gone); with `lazy` there is none.
fn uniq<'p>(b: &mut Builder<'p, '_>, a: &'p Agg<'p>, tlist: &'p NodeList<'p>, carried: &dyn Fn(i16) -> bool) -> ArenaVec<i16> {
    let mut fed: ArenaVec<i16> = ArenaVec::new_in(b.arena); // alloc: Cold
    let Some((at, how)) = uniq_of(b, a, tlist, carried, &mut fed) else {
        fed.clear();
        return fed;
    };
    if how.is_err() {
        fed.clear();
    }
    if let Some(p) = b.parents.as_mut().filter(|p| !p.uniq.iter().any(|u| u.at == at)) {
        p.uniq.push(Uniq { at, how });
    }
    fed
}

fn uniq_of<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>, tlist: &'p NodeList<'p>, carried: &dyn Fn(i16) -> bool, fed: &mut ArenaVec<i16>) -> Option<(usize, Result<UniqSub<'p>, &'static str>)> {
    let (mcx, parents) = (b.alloc?, b.parents.as_ref()?);
    let n = a.numCols.max(0) as usize;
    let agg_ok = a.aggstrategy == crate::ops::shared::agg::AGG_HASHED && n > 0 && a.grpColIdx.len() >= n && a.grpOperators.len() >= n && a.aggsplit == AGGSPLIT_SIMPLE;
    if !agg_ok || !a.groupingSets.is_nil() || !a.chain.is_nil() || !a.plan.qual.is_nil() || !a.plan.initPlan.is_nil() || parents.lazy(b) {
        return None;
    }
    let col = |t: Node<'p>| t.as_target_entry().map(|t| (t.resno, t.expr.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno > 0)));
    if !tlist.iter().all(|t| col(t).is_some_and(|c| c.1.is_some())) {
        return None;
    }
    let input = |r: i16| tlist.iter().filter_map(col).find(|c| c.0 == r).and_then(|c| c.1).map(|v| v.varattno);
    let child = a.plan.lefttree?;
    let (l, false) = find(b, &a.plan, true)? else { return None };
    let nl = l.as_nest_loop().filter(|nl| nl.join.jointype == types_nodes::jointype::JoinType::JOIN_INNER)?;
    let mut pids: ArenaVec<i32> = ArenaVec::new_in(b.arena); // alloc: Cold
    let mut vals: ArenaVec<(i32, i16)> = ArenaVec::new_in(b.arena); // alloc: Cold
    for p in nl.nestParams.iter() {
        let p = p.as_nest_loop_param()?;
        match p.paramval.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0) {
            Some(v) if input(v.varattno).is_some_and(|c| !carried(c)) => {
                pids.push(p.paramno);
                vals.push((p.paramno, input(v.varattno)?));
                fed.push(v.varattno);
            }
            Some(_) => {}
            None if unread(p.paramval, &|v| v.varno == OUTER_VAR) => {}
            None => return None,
        }
    }
    let hit = |v: &Var<'_>| v.varno == OUTER_VAR && fed.contains(&v.varattno);
    let own = [&nl.join.plan.targetlist, &nl.join.plan.qual, &nl.join.joinqual];
    if pids.is_empty() || !own.iter().all(|l| l.iter().all(|n| unread(n, &hit))) {
        return None;
    }
    let leaf = nl.join.plan.righttree?;
    let (reads, quiet): ([&'p NodeList<'p>; 3], [&'p NodeList<'p>; 3]) = match leaf.node_tag() {
        NodeTag::T_IndexScan => leaf.as_index_scan().map(|s| ([&s.indexqual, &s.indexqualorig, &s.scan.plan.qual], [&s.scan.plan.targetlist, &s.indexorderby, &s.indexorderbyorig]))?,
        NodeTag::T_IndexOnlyScan => leaf.as_index_only_scan().map(|s| ([&s.indexqual, &s.recheckqual, &s.scan.plan.qual], [&s.scan.plan.targetlist, &s.indexorderby, &s.indextlist]))?,
        _ => return None,
    };
    let lp = leaf.as_plan()?;
    if !lp.initPlan.is_nil() || lp.lefttree.is_some() || lp.righttree.is_some() || !quiet.iter().all(|l| l.iter().all(|n| !p_read(n, &pids, None))) {
        return None;
    }
    let mut occ: ArenaVec<Node<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    for n in reads.iter().flat_map(|l| l.iter()) {
        if expr::has_subplan(n) || !key_exprs(n, &pids, &mut occ) {
            return None;
        }
    }
    let s = *occ.first()?;
    let ty = crate::catalog::expr_type::type_of(s)?;
    if !occ.iter().all(|&o| types_nodes::equal(o, s)) || crate::catalog::expr_type::typmod_of(s) != -1 {
        return None;
    }
    let mapped = subst_params(mcx, s, &vals, child).ok().flatten()?;
    // the image road: no collation anywhere; else D3375's equivalence road, where the leaf reads `S` only as an operand
    // of the key's own `=` under the key's collation (`operand_only`)
    let image = |i: usize| crate::catalog::expr_type::collation_of(s) == 0 && holds(b, child, mapped, i, ty, a, None);
    let equiv = |i: usize| {
        let eq = (a.grpOperators[i], a.grpCollations.get(i).copied().unwrap_or(0));
        let det = eq.1 == 0 || b.facts.coll(eq.1).bytes_eq;
        det && crate::catalog::keysem::own_equivalence(b.facts, eq.0, ty) && holds(b, child, mapped, i, ty, a, Some(eq)) && reads.iter().all(|l| l.iter().all(|n| operand_only(n, s, eq)))
    };
    let i = (0..n).find(|&i| image(i)).or_else(|| (0..n).find(|&i| equiv(i)))?;
    let rk = tlist.iter().filter_map(col).find(|c| c.1.is_some_and(|v| v.varattno == a.grpColIdx[i]))?.0;
    let id0 = pids.iter().copied().filter(|&p| b.stmt.paramExecTypes.iter().nth(p as usize) == Some(ty)).min()?;
    let at = &nl.join.plan as *const Plan<'p> as usize;
    Some((at, Ok(UniqSub { s, pids, id0, ty, rk })))
}

/// Each var-free sub-expression of `n` reading a parameter of `pids` (outermost first) into `occ`, when every such read sits
/// in one whose parameters are all `pids`' or EXTERN; a node kind the walker does not know fails.
fn key_exprs<'p>(n: Node<'p>, pids: &[i32], occ: &mut ArenaVec<Node<'p>>) -> bool {
    crate::walk::stack_check(); // D3748
    if !p_read(n, pids, None) {
        return true;
    }
    let mut free = true;
    let known = expr::walk(n, &mut |x| {
        free &= x.as_var().is_none() && x.as_param().is_none_or(|p| p.paramkind == ParamKind::PARAM_EXTERN || (p.paramkind == ParamKind::PARAM_EXEC && pids.contains(&p.paramid)));
        true
    });
    if known && free {
        occ.push(n);
        return true;
    }
    let mut ok = n.as_param().is_none();
    let known = expr::kids(n, &mut |k| ok &= key_exprs(k, pids, occ));
    ok && known
}

/// Whether `n` may read a PARAM_EXEC of `pids` other than the node `but` (a node kind the walker does not know counts as a read).
fn p_read(n: Node<'_>, pids: &[i32], but: Option<Node<'_>>) -> bool {
    !expr::walk(n, &mut |x| !x.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_EXEC && pids.contains(&p.paramid) && but.is_none_or(|m| !x.ptr_eq(m))))
}

/// `x` with each PARAM_EXEC `vals` names as the child's input column's expression.
fn subst_params<'p>(mcx: mcx::Mcx<'p>, x: Node<'p>, vals: &[(i32, i16)], child: Node<'p>) -> types_error::PgResult<Option<Node<'p>>> {
    crate::walk::stack_check(); // D3748
    if let Some(p) = x.as_param().filter(|p| p.paramkind == ParamKind::PARAM_EXEC) {
        return Ok(vals.iter().find(|v| v.0 == p.paramid).and_then(|v| entry(child.as_plan()?, v.1)));
    }
    nodes_core::expression_tree_mutator_dyn(mcx, x, &mut |y| subst_params(mcx, y, vals, child))
}

/// A join clause `x = y` under an image equality of `ty` (`image_eq_same`: no collation, both sides of `ty`), or with
/// `same` = (op, collation) under exactly that operator and collation (D3375's equivalence road): its two sides. AR-20: an
/// OpExpr of two arguments is a binary operator (a node-shape fact, no benchmark arity); a prefix one has no sides.
fn eq_sides<'p>(f: &dyn crate::catalog::Facts, c: Node<'p>, ty: u32, same: Option<(u32, u32)>) -> Option<(Node<'p>, Node<'p>)> {
    let o = c.as_op_expr()?;
    let op = match same {
        None => o.inputcollid == 0 && crate::catalog::keysem::image_eq_same(f, o.opno) == Some(ty),
        Some((op, coll)) => o.opno == op && o.inputcollid == coll,
    };
    let binary = o.args.len() == 2 && op;
    (binary && o.args.iter().all(|x| crate::catalog::expr_type::type_of(x) == Some(ty))).then(|| (o.args.nth(0), o.args.nth(1)))
}

/// D3375: every sub-expression of the leaf's clause `n` `equal` to `s` is an operand of the key's own `=` under the key's
/// collation (`eq`), so the clause holds for the key exactly when it holds for `s` (`keysem::own_equivalence`). A node the
/// child walk does not know fails.
fn operand_only<'p>(n: Node<'p>, s: Node<'p>, eq: (u32, u32)) -> bool {
    crate::walk::stack_check(); // D3748
    if types_nodes::equal(n, s) {
        return false;
    }
    if let Some(o) = n.as_op_expr().filter(|o| o.opno == eq.0 && o.inputcollid == eq.1 && o.args.len() == 2) {
        return o.args.iter().all(|x| types_nodes::equal(x, s) || operand_only(x, s, eq));
    }
    let mut ok = true;
    let known = expr::kids(n, &mut |k| ok &= operand_only(k, s, eq));
    ok && known
}

/// D3593 (D3375's equivalence road for D514's readers): passenger `ce` (bare entries `sub`) of a keyed Agg whose JOIN_INNER
/// child holds `ce = k` for a key `k` under the key's grouping operator, the type's own `=` under a deterministic collation
/// (`own_equivalence`: numeric 1.0 = 1.00), so a group's members are equal to each other. The Hash and join above read it
/// only as an operand of that operator: each answer is the same for every member, so `min`'s member serves.
fn equiv_passenger<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>, ce: Node<'p>, sub: &[Sub<'p>]) -> bool {
    let n = a.numCols.max(0) as usize;
    let (Some(parents), Some(child), Some(ty)) = (b.parents.as_ref(), a.plan.lefttree, crate::catalog::expr_type::type_of(ce)) else { return false };
    let flat = a.aggsplit == AGGSPLIT_SIMPLE && a.groupingSets.is_nil() && a.chain.is_nil() && a.plan.initPlan.is_nil();
    if !flat || n == 0 || a.grpColIdx.len() < n || a.grpOperators.len() < n || parents.lazy(b) {
        return false;
    }
    let key_eq = |i: usize| {
        let eq = (a.grpOperators[i], a.grpCollations.get(i).copied().unwrap_or(0));
        let k = child.as_plan().and_then(|c| entry(c, a.grpColIdx[i]))?;
        let det = eq.1 == 0 || b.facts.coll(eq.1).bytes_eq;
        let own = crate::catalog::keysem::own_equivalence(b.facts, eq.0, ty) && crate::catalog::expr_type::type_of(k) == Some(ty);
        (det && own && equated_by(child, ce, k, eq)).then_some(eq)
    };
    (0..n).find_map(key_eq).is_some_and(|eq| equiv_above(b, &a.plan, sub, eq))
}

/// Whether JOIN_INNER join `j` holds `x = k` under `eq` on every row it emits: a hash / merge clause or a qual conjunct.
fn equated_by<'p>(j: Node<'p>, x: Node<'p>, k: Node<'p>, eq: (u32, u32)) -> bool {
    let inner = types_nodes::jointype::JoinType::JOIN_INNER;
    let lists: [&'p NodeList<'p>; 3] = match j.node_tag() {
        NodeTag::T_HashJoin => match j.as_hash_join().filter(|h| h.join.jointype == inner) {
            Some(h) => [&h.hashclauses, &h.join.joinqual, &h.join.plan.qual],
            None => return false,
        },
        NodeTag::T_MergeJoin => match j.as_merge_join().filter(|m| m.join.jointype == inner) {
            Some(m) => [&m.mergeclauses, &m.join.joinqual, &m.join.plan.qual],
            None => return false,
        },
        _ => return false,
    };
    let pair = |p: Node<'p>, q: Node<'p>| types_nodes::equal(p, x) && types_nodes::equal(q, k);
    lists.iter().flat_map(|l| l.iter()).filter_map(|c| c.as_op_expr()).any(|o| {
        o.opno == eq.0 && o.inputcollid == eq.1 && o.args.len() == 2 && (pair(o.args.nth(0), o.args.nth(1)) || pair(o.args.nth(1), o.args.nth(0)))
    })
}

/// `equiv_passenger`'s readers from the Agg `n` up: a Hash handing the column on bare to its HashJoin, each Hash key reading
/// it the bare column under an `eq` clause; then the join, whose target list and parameters read none of it and whose
/// clauses and quals read it only as an operand of `eq` (`col_operand_only`).
fn equiv_above<'p>(b: &Builder<'p, '_>, n: &'p Plan<'p>, sub: &[Sub<'p>], eq: (u32, u32)) -> bool {
    let Some((p, right)) = find(b, n, false) else { return false };
    let clause_eq = |c: Option<Node<'p>>| c.and_then(|c| c.as_op_expr()).is_some_and(|o| o.opno == eq.0 && o.inputcollid == eq.1 && o.args.len() == 2);
    let mut up: ArenaVec<Sub<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    let (j, varno) = match p.as_hash() {
        Some(h) => {
            let hit = |v: &Var<'_>| v.varno == OUTER_VAR && has(sub, v.varattno);
            if !hand_on(&h.plan.targetlist, sub, &|v: &Var<'_>| v.varlevelsup == 0 && hit(v), &|e: Node<'p>| unread(e, &hit), &mut up) {
                return false;
            }
            let Some((j, true)) = find(b, &h.plan, false) else { return false };
            let Some(hj) = j.as_hash_join() else { return false };
            let bare = |k: Node<'p>| k.as_var().is_some_and(|v| v.varlevelsup == 0 && hit(v));
            let keys = h.hashkeys.iter().enumerate().all(|(i, k)| unread(k, &hit) || (bare(k) && clause_eq(hj.hashclauses.iter().nth(i))));
            if !keys || !h.plan.initPlan.is_nil() || !h.plan.qual.iter().all(|q| unread(q, &hit)) {
                return false;
            }
            (j, INNER_VAR)
        }
        None => {
            up.extend(sub.iter().copied());
            (p, if right { INNER_VAR } else { OUTER_VAR })
        }
    };
    let hit = |v: &Var<'_>| v.varno == varno && has(&up, v.varattno);
    let (jn, clauses, outer_keys, params): (&'p types_nodes::plannodes::Join<'p>, Option<&'p NodeList<'p>>, Option<(&'p NodeList<'p>, &'p NodeList<'p>)>, Option<&'p NodeList<'p>>) = match j.node_tag() {
        NodeTag::T_HashJoin => match j.as_hash_join() {
            Some(h) => (&h.join, Some(&h.hashclauses), Some((&h.hashkeys, &h.hashclauses)), None),
            None => return false,
        },
        NodeTag::T_MergeJoin => match j.as_merge_join() {
            Some(m) => (&m.join, Some(&m.mergeclauses), None, None),
            None => return false,
        },
        NodeTag::T_NestLoop => match j.as_nest_loop() {
            Some(nl) => (&nl.join, None, None, Some(&nl.nestParams)),
            None => return false,
        },
        _ => return false,
    };
    let quiet = |l: &NodeList<'p>| l.iter().all(|x| unread(x.as_nest_loop_param().map_or(x, |p| p.paramval), &hit));
    let keyed = outer_keys.is_none_or(|(ks, cs)| ks.iter().enumerate().all(|(i, k)| unread(k, &hit) || (k.as_var().is_some_and(|v| v.varlevelsup == 0 && hit(v)) && clause_eq(cs.iter().nth(i)))));
    let operands = [Some(&jn.joinqual), Some(&jn.plan.qual), clauses].into_iter().flatten().all(|l| l.iter().all(|x| col_operand_only(x, &hit, eq)));
    if !jn.plan.initPlan.is_nil() || !params.is_none_or(quiet) || !keyed || !operands {
        return false;
    }
    let mut out: ArenaVec<Sub<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    if !hand_on(&jn.plan.targetlist, &up, &|v: &Var<'_>| v.varlevelsup == 0 && hit(v), &|e: Node<'p>| unread(e, &hit), &mut out) {
        return false;
    }
    out.is_empty() || passenger_above(b, &jn.plan, &out)
}

/// The join `n` hands the column on bare to an Agg that groups and filters by none of it and emits it only bare: a
/// passenger again, whose own `rewrite` proves its readers or keeps the lowerer's refusal.
fn passenger_above<'p>(b: &Builder<'p, '_>, n: &'p Plan<'p>, sub: &[Sub<'p>]) -> bool {
    let Some(a) = find(b, n, false).and_then(|(p, right)| (!right).then_some(p)).and_then(|p| p.as_agg()) else { return false };
    let hit = |v: &Var<'_>| v.varno == OUTER_VAR && has(sub, v.varattno);
    let grouped = a.grpColIdx.iter().take(a.numCols.max(0) as usize).any(|&c| has(sub, c));
    let bare = |t: Node<'p>| t.as_target_entry().is_some_and(|t| t.expr.as_var().is_some_and(|v| v.varlevelsup == 0 && hit(v)) || unread(t.expr, &hit));
    a.numCols > 0 && !grouped && a.plan.qual.iter().all(|q| unread(q, &hit)) && a.plan.targetlist.iter().all(bare)
}

/// Whether every read in `n` of a column `hit` picks is a bare operand (through relabels) of an `eq` OpExpr; a node kind the
/// child walk does not know fails.
fn col_operand_only<'p>(n: Node<'p>, hit: &dyn Fn(&Var<'_>) -> bool, eq: (u32, u32)) -> bool {
    crate::walk::stack_check(); // D3748
    let col = |x: Node<'p>| crate::facts::pinned::relabels(x).as_var().is_some_and(|v| v.varlevelsup == 0 && hit(v));
    if col(n) {
        return false;
    }
    if let Some(o) = n.as_op_expr().filter(|o| o.opno == eq.0 && o.inputcollid == eq.1 && o.args.len() == 2) {
        return o.args.iter().all(|x| col(x) || col_operand_only(x, hit, eq));
    }
    let mut ok = true;
    let known = expr::kids(n, &mut |k| ok &= col_operand_only(k, hit, eq));
    ok && known
}

/// Whether the Agg's JOIN_INNER child holds `mapped = key i` on every row it emits under an image equality of `ty` (see
/// `uniq`), or with `same` under the key's own `=` and collation, the Agg's grouping operator for key i (D3375).
fn holds<'p>(b: &Builder<'p, '_>, child: Node<'p>, mapped: Node<'p>, i: usize, ty: u32, a: &'p Agg<'p>, same: Option<(u32, u32)>) -> bool {
    let Some(k) = child.as_plan().and_then(|c| entry(c, a.grpColIdx[i])) else { return false };
    let kv = k.as_var().filter(|v| v.varlevelsup == 0 && v.vartype == ty && v.vartypmod == -1 && (same.is_some() || v.varcollid == 0));
    let image = same.is_some() || crate::catalog::keysem::image_eq_same(b.facts, a.grpOperators[i]) == Some(ty);
    if kv.is_none() || !image || same.is_some_and(|(op, _)| op != a.grpOperators[i]) {
        return false;
    }
    let eq = |c: Node<'p>| eq_sides(b.facts, c, ty, same);
    let inner = types_nodes::jointype::JoinType::JOIN_INNER;
    if let Some(hj) = child.as_hash_join().filter(|h| h.join.jointype == inner) {
        return hj.hashclauses.iter().filter_map(eq).any(|(x, y)| (types_nodes::equal(x, mapped) && types_nodes::equal(y, k)) || (types_nodes::equal(y, mapped) && types_nodes::equal(x, k)));
    }
    let (Some(cn), Some(mcx)) = (child.as_nest_loop().filter(|n| n.join.jointype == inner), b.alloc) else { return false };
    let Some(ix) = cn.join.plan.righttree.and_then(|r| r.as_index_scan()) else { return false };
    let mut bad = false;
    let Ok(Some(down)) = through_leaf(mcx, mapped, &ix.scan.plan.targetlist, &mut bad) else { return false };
    if bad {
        return false;
    }
    let bound = |p: Node<'p>| p.as_param().filter(|p| p.paramkind == ParamKind::PARAM_EXEC).is_some_and(|p| cn.nestParams.iter().filter_map(|x| x.as_nest_loop_param()).any(|x| x.paramno == p.paramid && types_nodes::equal(x.paramval, k)));
    ix.indexqualorig.iter().filter_map(eq).any(|(x, y)| (types_nodes::equal(x, down) && bound(y)) || (types_nodes::equal(y, down) && bound(x)))
}

/// `x` (over a nest child's row) with each inner column as its leaf's target entry, a bare column of the scan; `None` when
/// it reads an outer column or an entry of another form.
fn through_leaf<'p>(mcx: mcx::Mcx<'p>, x: Node<'p>, tlist: &'p NodeList<'p>, bad: &mut bool) -> types_error::PgResult<Option<Node<'p>>> {
    crate::walk::stack_check(); // D3748
    if let Some(v) = x.as_var() {
        let e = tlist.iter().filter_map(|t| t.as_target_entry()).find(|t| v.varno == INNER_VAR && v.varlevelsup == 0 && t.resno == v.varattno).map(|t| t.expr);
        let e = e.filter(|e| e.as_var().is_some_and(|w| w.varno > 0 && w.varlevelsup == 0));
        *bad |= e.is_none();
        return Ok(e);
    }
    nodes_core::expression_tree_mutator_dyn(mcx, x, &mut |y| through_leaf(mcx, y, tlist, bad))
}

/// No reader above the SEMI hash join `semi` reads its output columns `resnos`: `readers_at` with no key (a Hash then
/// its join, or a join directly), or an Agg over it that groups by none of them, has no grouping sets and no qual
/// reading them, and emits them only bare, to readers that do not read them.
fn unread_above<'p>(b: &Builder<'p, '_>, semi: &'p Plan<'p>, sub: &[Sub<'p>], mcx: mcx::Mcx<'p>) -> bool {
    let Some((p, right)) = find(b, semi, true) else { return false };
    let Some(a) = p.as_agg() else { return readers_at(b, (p, right), sub, &[], mcx, true) == Some(false) };
    let reads = |n: Node<'p>| !unread(n, &|v| v.varno == OUTER_VAR && has(sub, v.varattno));
    let grouped = a.grpColIdx.iter().take(a.numCols.max(0) as usize).any(|&c| has(sub, c));
    if right || grouped || !a.groupingSets.is_nil() || !a.chain.is_nil() || a.plan.qual.iter().any(reads) {
        return false;
    }
    let mut up: ArenaVec<Sub<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    if !hand_on(&a.plan.targetlist, sub, &|v: &Var<'_>| v.varno == OUTER_VAR && v.varlevelsup == 0 && has(sub, v.varattno), &|e: Node<'p>| !reads(e), &mut up) {
        return false;
    }
    up.is_empty() || readers_of(b, &a.plan, &up, &[], mcx, true) == Some(false)
}

/// Input column `c`'s expression (the child's target entry).
fn entry<'p>(child: &'p Plan<'p>, c: i16) -> Option<Node<'p>> {
    child.targetlist.iter().filter_map(|t| t.as_target_entry()).find(|t| t.resno == c).map(|t| t.expr)
}

/// IN-LIMIT (modifytable): whether the Agg's input column `ce` is the whole row of the SubqueryScan `child` (a semi
/// join's unique-ified IN subquery, whose whole row a write's ROW_MARK_COPY carries for EvalPlanQual) and every field
/// of that row (the subplan's non-junk outputs, in order) is a grouping key: field `i` is a key that is the scan's bare
/// column `i`. Every member of a group then carries the same row (equal fields: the equal-member ruling), so the group's
/// `any_value` is PostgreSQL's first hashed tuple's row.
fn keyed_row<'p>(child: Node<'p>, ce: Node<'p>, keys: &[Node<'p>]) -> bool {
    let Some(sq) = child.as_subquery_scan() else { return false };
    let rti = sq.scan.scanrelid as i32;
    let whole = ce.as_var().is_some_and(|v| v.varno == rti && v.varattno == 0 && v.varlevelsup == 0);
    let Some(sub) = sq.subplan.and_then(|n| n.as_plan()).filter(|_| whole) else { return false };
    let key = |r: i16| keys.iter().any(|k| k.as_var().is_some_and(|v| v.varno == rti && v.varattno == r && v.varlevelsup == 0));
    let mut fields = sub.targetlist.iter().filter_map(|t| t.as_target_entry()).filter(|t| !t.resjunk).peekable();
    fields.peek().is_some() && fields.all(|t| key(t.resno))
}

/// D2740: exec agg.arena's numbering of Agg `a` (exec ops/agg/trans/arena.rs `prepare`): its aggregates once each by
/// aggno (`naggs`), then its kept columns, each OUTER_VAR the non-junk target list then the qual reads outside an
/// aggregate, first seen. The hosted arena takes aggno `naggs + i` as kept column `i` of that column's type, the group's
/// first row's value (hosts hosted_kept.rs `kept_of` / `keep_first`): PG's representative tuple, a NULL too.
fn arena_cols<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>) -> (i32, ArenaVec<i16>) {
    let mut nos: ArenaVec<i32> = ArenaVec::new_in(b.arena); // alloc: Cold
    for e in a.plan.targetlist.iter().chain(a.plan.qual.iter()) {
        expr::run_aggrefs(e, &mut |x| {
            let n = x.as_aggref().map_or(-1, |g| g.aggno);
            if !nos.contains(&n) {
                nos.push(n);
            }
        });
    }
    let mut kept: ArenaVec<i16> = ArenaVec::new_in(b.arena); // alloc: Cold
    for e in a.plan.targetlist.iter().filter(|n| n.as_target_entry().is_none_or(|t| !t.resjunk)).chain(a.plan.qual.iter()) {
        expr::walk(e, &mut |x| {
            if let Some(v) = x.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && !kept.contains(&v.varattno)) {
                kept.push(v.varattno);
            }
            x.as_aggref().is_none()
        });
    }
    (nos.len() as i32, kept)
}

/// `ctid` / `tableoid` (htup.h's SelfItemPointerAttributeNumber / TableOidAttributeNumber).
const CTID_ATTNO: i16 = -1;
const TABLEOID_ATTNO: i16 = -6;

/// IN-LIMIT (modifytable, mt-0336's form): whether the Agg's input column `ce` is the ctid or tableoid of a relation
/// the statement marks ROW_MARK_REFERENCE: the junk EvalPlanQual refetches that relation's row at (the version the
/// statement joined). A write or lock has no grouping of its own, so the Agg is a semi join's unique-ification, whose
/// inner relations' columns only its join clauses (the keys) and their row marks' junk read. Any member's row is one
/// whose key is the group's, and the recheck evaluates the join over that row's key; every such cell of the group is an
/// `any_value`, which keeps the first row folded, so a ctid and its tableoid come from the same member.
/// window_core-0218 / wcore-0312 (`DELETE .. WHERE id IN (SELECT id FROM (SELECT id, row_number() OVER (..) rn ..) s
/// WHERE rn > 2)`): or the whole row of a relation the statement marks ROW_MARK_COPY (a subquery's or function's row
/// whose fields are not all keys, so `keyed_row` does not hold). EvalPlanQual hands the scan the stored row, re-checks
/// the scan's own quals on it (`rn > 2`: every member passed them, over the same fields) and evaluates the join over its
/// key (the group's), so any member's row gives PostgreSQL's recheck result (its own is the first hashed tuple's).
/// lockrows (regress inherit#117, `UPDATE bar .. WHERE f1 IN (SELECT f1 FROM foo)` over inheritance parents): below an
/// Append (or any node whose target list setrefs rewrote to OUTER_VAR / INNER_VAR copies, `set_dummy_tlist_references`)
/// the column is a copy whose `varnosyn` / `varattnosyn` keep the parent's `ctid` / `tableoid`; the parent's mark
/// (isParent, ROW_MARK_REFERENCE, its tableoid junk naming the member's leaf) is the one EvalPlanQual reads.
fn marked_junk<'p>(b: &Builder<'p, '_>, ce: Node<'p>) -> bool {
    use types_nodes::plannodes::RowMarkType::{ROW_MARK_COPY, ROW_MARK_REFERENCE};
    let syn = |v: &Var<'p>| if matches!(v.varno, OUTER_VAR | INNER_VAR) { (v.varnosyn as i32, v.varattnosyn) } else { (v.varno, v.varattno) };
    let Some((rti, att)) = ce.as_var().filter(|v| v.varlevelsup == 0).map(syn).filter(|&(r, a)| r > 0 && matches!(a, CTID_ATTNO | TABLEOID_ATTNO | 0)) else { return false };
    let kind = if att == 0 { ROW_MARK_COPY } else { ROW_MARK_REFERENCE };
    let marked = |m: &types_nodes::plannodes::PlanRowMark| m.rti as i32 == rti && m.markType == kind;
    b.stmt.rowMarks.iter().filter_map(|m| m.as_plan_row_mark()).any(marked)
}

/// rules#336 (`DELETE FROM shoelace WHERE EXISTS (SELECT * FROM shoelace_candelete ..)`, the candelete view a NOT EXISTS
/// over joins): the expression the plan node `n` emits as its column `c`, followed down through each join's, Hash's,
/// Sort's or Material's bare `OUTER_VAR` / `INNER_VAR` entry to the child entry it copies (setrefs' copies of one Var).
/// The unique-ified semi join's input is then often a join whose row marks' ctid junk sits under such copies, not a scan.
/// A copy is followed where each row's value is the child's or NULL (`copies`): a join's either side (an outer join's
/// nullable one and an anti join's other one NULL on some or every row), a Hash / Sort / Material. The group's cell keeps
/// its first row's value, a NULL too (D2740: hosts hosted_kept.rs `keep_first`), PG's representative tuple's.
fn junk_origin<'p>(n: Node<'p>, c: i16) -> Option<Node<'p>> {
    crate::walk::stack_check(); // D3748
    let p = n.as_plan()?;
    let e = entry(p, c)?;
    match e.as_var().filter(|v| v.varlevelsup == 0 && v.varattno > 0 && copies(n, v.varno)) {
        Some(v) if v.varno == OUTER_VAR => junk_origin(p.lefttree?, v.varattno),
        Some(v) if v.varno == INNER_VAR => junk_origin(p.righttree?, v.varattno),
        _ => Some(e),
    }
}

/// `junk_origin`'s rule: whether plan node `n`'s `side` (`OUTER_VAR` / `INNER_VAR`) column is its child's or NULL on
/// every row the node emits.
fn copies(n: Node<'_>, side: i32) -> bool {
    use types_nodes::jointype::JoinType as J;
    if n.as_hash().is_some() || n.as_sort().is_some() || n.as_material().is_some() {
        return side == OUTER_VAR;
    }
    let jt = n.as_hash_join().map(|j| j.join.jointype).or(n.as_nest_loop().map(|j| j.join.jointype)).or(n.as_merge_join().map(|j| j.join.jointype));
    match (jt, side) {
        (Some(J::JOIN_INNER | J::JOIN_ANTI | J::JOIN_RIGHT_ANTI | J::JOIN_LEFT | J::JOIN_RIGHT | J::JOIN_FULL), OUTER_VAR | INNER_VAR) => true,
        (Some(J::JOIN_SEMI), OUTER_VAR) | (Some(J::JOIN_RIGHT_SEMI), INNER_VAR) => true,
        _ => false,
    }
}

/// `any_value(anyelement)`: an IN-LIMIT whole row's cell (`keyed_row`).
fn any_value() -> Option<u32> {
    crate::ops::agg::trans::table::AGG_TABLE.iter().find(|r| r.name == "any_value").map(|r| r.fnoid)
}

/// The type's `min` aggregate (a member-returning `*smaller`), none for a type without one.
fn min_of(typid: u32) -> Option<u32> {
    crate::ops::agg::trans::table::AGG_TABLE.iter().find(|r| r.name == "min" && r.args == [typid]).map(|r| r.fnoid)
}

/// `f` over `n` and its sub-expressions outside an aggregate's arguments (`f` false: not below).
fn walk_out<'p>(n: Node<'p>, f: &mut dyn FnMut(Node<'p>) -> bool) {
    crate::walk::stack_check(); // D3748
    if f(n) {
        crate::expr::kids(n, &mut |k| walk_out(k, f));
    }
}

/// Whether `n` surely reads no column `hit` picks: `expr::walk` meets none and knows every node kind on the way (a kind
/// it does not list counts as a read, so a proof that a column is unread fails closed).
fn unread(n: Node<'_>, hit: &dyn Fn(&Var<'_>) -> bool) -> bool {
    crate::expr::walk(n, &mut |x| !x.as_var().is_some_and(|v| v.varlevelsup == 0 && hit(v)))
}

/// A reader's column (at the reader's level) and the Agg input's expression it stands for (its child's target entry).
type Sub<'p> = (i16, Node<'p>);

fn ce_of<'p>(sub: &[Sub<'p>], c: i16) -> Option<Node<'p>> {
    sub.iter().find(|s| s.0 == c).map(|s| s.1)
}

fn has(sub: &[Sub<'_>], c: i16) -> bool {
    sub.iter().any(|s| s.0 == c)
}

/// `v`'s entry of `sub` as the column `resno` of the node that hands `v` on bare.
fn lift<'p>(sub: &[Sub<'p>], v: &Var<'_>, resno: i16) -> Option<Sub<'p>> {
    ce_of(sub, v.varattno).map(|ce| (resno, ce))
}

/// The hand-on step of every reader walk, over a node's target list `tl`: an entry that is a bare column `bare` picks
/// hands that column (`from`'s) on as the entry's own resno (`lift`, onto `up`); any other entry reads no picked column
/// (`quiet`). False when one does: the walk stops at this node. One loop for the six walks that wrote it out (a Hash's
/// and a join's in `equiv_above`, an Agg's in `unread_above`, a Hash's and a Sort / Material's in `readers_at`, a join's
/// or an Agg's in `dead_above`), each with its own picks.
fn hand_on<'p>(tl: &'p NodeList<'p>, from: &[Sub<'p>], bare: &dyn Fn(&Var<'_>) -> bool, quiet: &dyn Fn(Node<'p>) -> bool, up: &mut ArenaVec<Sub<'p>>) -> bool {
    for t in tl.iter().filter_map(|t| t.as_target_entry()) {
        match t.expr.as_var() {
            Some(v) if bare(v) => up.extend(lift(from, v, t.resno)),
            _ if !quiet(t.expr) => return false,
            _ => {}
        }
    }
    true
}

fn reads_sub(n: Node<'_>, varno: i32, sub: &[Sub<'_>]) -> bool {
    let mut hit = false;
    walk_out(n, &mut |x| {
        hit |= x.as_var().is_some_and(|v| v.varno == varno && v.varlevelsup == 0 && has(sub, v.varattno));
        !hit
    });
    hit
}

fn reads_col(n: Node<'_>, varno: i32, cols: &[i16]) -> bool {
    let mut hit = false;
    walk_out(n, &mut |x| {
        hit |= x.as_var().is_some_and(|v| v.varno == varno && v.varlevelsup == 0 && cols.contains(&v.varattno));
        !hit
    });
    hit
}

/// Whether every read of `(varno, cols)` in `n` sits inside a sub-expression equal to a key (the column standing for
/// `ce`); a node kind the walker does not know fails. With no key, whether `n` reads none of them (`unread`).
fn inside<'p>(n: Node<'p>, varno: i32, sub: &[Sub<'p>], keys: &[Node<'p>], mcx: mcx::Mcx<'p>) -> bool {
    crate::walk::stack_check(); // D3748
    if keys.is_empty() {
        return unread(n, &|v| v.varno == varno && has(sub, v.varattno));
    }
    if !reads_sub(n, varno, sub) {
        return true;
    }
    if n.as_var().is_some() {
        return false;
    }
    if keys.iter().any(|k| k.node_tag() == n.node_tag()) {
        if let Ok(Some(s)) = subst(mcx, n, varno, sub) {
            if keys.iter().any(|&k| types_nodes::equal(s, k)) {
                return true;
            }
        }
    }
    let mut ok = true;
    let known = crate::expr::kids(n, &mut |k| ok &= inside(k, varno, sub, keys, mcx));
    ok && known
}

fn subst<'p>(mcx: mcx::Mcx<'p>, x: Node<'p>, varno: i32, sub: &[Sub<'p>]) -> types_error::PgResult<Option<Node<'p>>> {
    crate::walk::stack_check(); // D3748
    if let Some(ce) = x.as_var().filter(|v| v.varno == varno && v.varlevelsup == 0).and_then(|v| ce_of(sub, v.varattno)) {
        return Ok(Some(ce));
    }
    nodes_core::expression_tree_mutator_dyn(mcx, x, &mut |y| subst(mcx, y, varno, sub))
}

/// The readers above the Agg (see the module doc): a Hash passing the column through to its join's inner side, or a
/// join reading it directly; the join's lists read it only inside a key expression (its target list never carries it).
/// `Some(read)`: the guard holds, and whether any reader reads the column at all; `None`: it fails.
fn readers_ok<'p>(b: &Builder<'p, '_>, a: &'p Agg<'p>, sub: &[Sub<'p>], keys: &[Node<'p>], mcx: mcx::Mcx<'p>) -> Option<bool> {
    readers_of(b, &a.plan, sub, keys, mcx, false)
}

/// `readers_ok` over the plan node `n`; `through` as `find`'s.
fn readers_of<'p>(b: &Builder<'p, '_>, n: &'p Plan<'p>, sub: &[Sub<'p>], keys: &[Node<'p>], mcx: mcx::Mcx<'p>, through: bool) -> Option<bool> {
    readers_at(b, find(b, n, through)?, sub, keys, mcx, through)
}

/// `readers_of` from the node's parent `p` (`right`: the node is its inner child).
fn readers_at<'p>(b: &Builder<'p, '_>, (p, right): (Node<'p>, bool), sub: &[Sub<'p>], keys: &[Node<'p>], mcx: mcx::Mcx<'p>, through: bool) -> Option<bool> {
    crate::walk::stack_check(); // D3748
    if let Some(h) = p.as_hash() {
        let mut up: ArenaVec<Sub<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
        if !hand_on(&h.plan.targetlist, sub, &|v: &Var<'_>| v.varno == OUTER_VAR && has(sub, v.varattno), &|e: Node<'p>| inside(e, OUTER_VAR, sub, keys, mcx), &mut up) {
            return None;
        }
        let lists = [&h.plan.qual, &h.hashkeys];
        if !lists.iter().all(|l| l.iter().all(|n| inside(n, OUTER_VAR, sub, keys, mcx))) {
            return None;
        }
        let read = lists.iter().any(|l| l.iter().any(|n| reads_sub(n, OUTER_VAR, sub)));
        let Some((j, true)) = find(b, &h.plan, through) else { return None };
        if let Some(r) = keys.is_empty().then(|| dead_above(b, (j, true), &up, mcx, through)).flatten() {
            return r.map(|r| r || read);
        }
        return join_ok(j, INNER_VAR, &up, keys, mcx).map(|r| r || read);
    }
    // D3375 (m11_merge_spool_inner): a Sort or a Material between the node and its reader passes the column on bare and
    // reads it nowhere (no sort key on it, no qual): its own parent's readers decide
    let pass = match p.node_tag() {
        NodeTag::T_Sort => p.as_sort().map(|s| (&s.plan, s.sortColIdx)),
        NodeTag::T_Material => p.as_material().map(|m| (&m.plan, &[][..])),
        _ => None,
    };
    if let Some((plan, sorted)) = pass {
        let mut up: ArenaVec<Sub<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
        let quiet = |e: Node<'p>| unread(e, &|v| v.varno == OUTER_VAR && has(sub, v.varattno));
        if !hand_on(&plan.targetlist, sub, &|v: &Var<'_>| v.varno == OUTER_VAR && v.varlevelsup == 0 && has(sub, v.varattno), &quiet, &mut up) {
            return None;
        }
        if right || !plan.qual.is_nil() || !plan.initPlan.is_nil() || sorted.iter().any(|&c| has(&up, c)) {
            return None;
        }
        if up.is_empty() {
            return Some(false);
        }
        return readers_at(b, find(b, plan, through)?, &up, keys, mcx, through);
    }
    if keys.is_empty() {
        if let Some(r) = dead_above(b, (p, right), sub, mcx, through) {
            return r;
        }
    }
    join_ok(p, if right { INNER_VAR } else { OUTER_VAR }, sub, keys, mcx)
}

/// D3375 (m11_merge_spool_inner's nested IN): with no key (the unread reading), a join whose target list hands the column
/// on bare and whose other lists read none of it, or an Agg that emits it bare and groups, filters and aggregates by none
/// of it, passes the question to its own parent (`Some(answer)`); `None`: not such a node, `join_ok` decides.
fn dead_above<'p>(b: &Builder<'p, '_>, (p, right): (Node<'p>, bool), sub: &[Sub<'p>], mcx: mcx::Mcx<'p>, through: bool) -> Option<Option<bool>> {
    let varno = if right { INNER_VAR } else { OUTER_VAR };
    let hit = |v: &Var<'_>| v.varno == varno && has(sub, v.varattno);
    let (plan, lists): (&'p Plan<'p>, [Option<&'p NodeList<'p>>; 4]) = match p.node_tag() {
        NodeTag::T_HashJoin => p.as_hash_join().map(|h| (&h.join.plan, [Some(&h.join.joinqual), Some(&h.hashclauses), Some(&h.hashkeys), None]))?,
        NodeTag::T_MergeJoin => p.as_merge_join().map(|m| (&m.join.plan, [Some(&m.join.joinqual), Some(&m.mergeclauses), None, None]))?,
        NodeTag::T_NestLoop => p.as_nest_loop().map(|n| (&n.join.plan, [Some(&n.join.joinqual), Some(&n.nestParams), None, None]))?,
        NodeTag::T_Agg => {
            let a = p.as_agg()?;
            let grouped = a.grpColIdx.iter().take(a.numCols.max(0) as usize).any(|&c| has(sub, c));
            if grouped || !a.groupingSets.is_nil() || !a.chain.is_nil() {
                return Some(None);
            }
            (&a.plan, [None, None, None, None])
        }
        _ => return None,
    };
    let param = |n: Node<'p>| n.as_nest_loop_param().map_or(n, |x| x.paramval);
    let quiet = lists.into_iter().flatten().chain([&plan.qual]).all(|l| l.iter().all(|n| unread(param(n), &hit)));
    let mut up: ArenaVec<Sub<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    if !hand_on(&plan.targetlist, sub, &|v: &Var<'_>| v.varlevelsup == 0 && hit(v), &|e: Node<'p>| unread(e, &hit), &mut up) {
        return Some(None);
    }
    if !quiet || !plan.initPlan.is_nil() {
        return Some(None);
    }
    if up.is_empty() {
        return Some(Some(false));
    }
    Some(readers_of(b, plan, &up, &[], mcx, through))
}

/// The join's lists (`Some(read)` as `readers_ok`): its target list, quals, and hash / merge clauses and NestLoop params.
fn join_ok<'p>(j: Node<'p>, varno: i32, sub: &[Sub<'p>], keys: &[Node<'p>], mcx: mcx::Mcx<'p>) -> Option<bool> {
    let (jn, extra, params): (&'p types_nodes::plannodes::Join<'p>, Option<&'p NodeList<'p>>, Option<&'p NodeList<'p>>) = match j.node_tag() {
        NodeTag::T_HashJoin => j.as_hash_join().map(|h| (&h.join, Some(&h.hashclauses), Some(&h.hashkeys)))?,
        NodeTag::T_MergeJoin => j.as_merge_join().map(|m| (&m.join, Some(&m.mergeclauses), None))?,
        NodeTag::T_NestLoop => j.as_nest_loop().map(|n| (&n.join, None, Some(&n.nestParams)))?,
        _ => return None,
    };
    let exprs = || {
        [Some(&jn.plan.targetlist), Some(&jn.plan.qual), Some(&jn.joinqual), extra, params]
            .into_iter()
            .flatten()
            .flat_map(|l| l.iter())
            .map(|n| n.as_nest_loop_param().map_or((n, None), |p| (p.paramval, Some(p.paramno))))
    };
    // producerchain (4): a NestLoop parameter that passes the bare column to the loop's inner leaf, which reads it only
    // inside a key (`leaf_inside`): `t.id IN (SELECT s.id + 100 FROM s)` unique-ifies s grouped by `s.id + 100` and binds
    // s.id for the inner's index key `t.id = ($0 + 100)`
    let bare = |n: Node<'p>| n.as_var().filter(|v| v.varno == varno && v.varlevelsup == 0).and_then(|v| ce_of(sub, v.varattno));
    let ok = |(n, pid): (Node<'p>, Option<i32>)| inside(n, varno, sub, keys, mcx) || pid.zip(bare(n)).is_some_and(|(pid, ce)| leaf_inside(j, pid, keys, ce, mcx));
    exprs().all(ok).then(|| exprs().any(|(n, _)| reads_sub(n, varno, sub)))
}

/// producerchain (4): whether the NestLoop `j`'s inner is one scan leaf (no child, no InitPlan, no SubPlan in its lists)
/// that reads PARAM_EXEC `pid` only inside a sub-expression equal to a key with the parameter standing for `ce`
/// (`param_inside`). The parameter carries the group's representative's column, so each read is `f(rep)`, the group's key
/// (the module doc's guard, through the parameter).
fn leaf_inside<'p>(j: Node<'p>, pid: i32, keys: &[Node<'p>], ce: Node<'p>, mcx: mcx::Mcx<'p>) -> bool {
    let Some(leaf) = j.as_nest_loop().and_then(|nl| nl.join.plan.righttree) else { return false };
    let lists: [&'p NodeList<'p>; 6] = match leaf.node_tag() {
        NodeTag::T_IndexScan => match leaf.as_index_scan() {
            Some(s) => [&s.indexqual, &s.indexqualorig, &s.scan.plan.qual, &s.scan.plan.targetlist, &s.indexorderby, &s.indexorderbyorig],
            None => return false,
        },
        NodeTag::T_IndexOnlyScan => match leaf.as_index_only_scan() {
            Some(s) => [&s.indexqual, &s.recheckqual, &s.scan.plan.qual, &s.scan.plan.targetlist, &s.indexorderby, &s.indextlist],
            None => return false,
        },
        _ => return false,
    };
    let Some(lp) = leaf.as_plan() else { return false };
    if !lp.initPlan.is_nil() || lp.lefttree.is_some() || lp.righttree.is_some() {
        return false;
    }
    lists.iter().flat_map(|l| l.iter()).all(|n| !expr::has_subplan(n) && param_inside(n, pid, keys, ce, mcx))
}

/// `inside` for PARAM_EXEC `pid`: every read of it in `n` sits inside a sub-expression equal to a key once the parameter
/// stands for `ce`; a node kind the walker does not know fails.
fn param_inside<'p>(n: Node<'p>, pid: i32, keys: &[Node<'p>], ce: Node<'p>, mcx: mcx::Mcx<'p>) -> bool {
    crate::walk::stack_check(); // D3748
    if !p_read(n, &[pid], None) {
        return true;
    }
    if n.as_param().is_some() {
        return false;
    }
    if keys.iter().any(|k| k.node_tag() == n.node_tag()) {
        if let Ok(Some(s)) = subst_param(mcx, n, pid, ce) {
            if keys.iter().any(|&k| types_nodes::equal(s, k)) {
                return true;
            }
        }
    }
    let mut ok = true;
    let known = expr::kids(n, &mut |k| ok &= param_inside(k, pid, keys, ce, mcx));
    ok && known
}

/// `x` with each PARAM_EXEC `pid` as `ce`.
fn subst_param<'p>(mcx: mcx::Mcx<'p>, x: Node<'p>, pid: i32, ce: Node<'p>) -> types_error::PgResult<Option<Node<'p>>> {
    crate::walk::stack_check(); // D3748
    if x.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_EXEC && p.paramid == pid) {
        return Ok(Some(ce));
    }
    nodes_core::expression_tree_mutator_dyn(mcx, x, &mut |y| subst_param(mcx, y, pid, ce))
}

/// The parent of the plan node `n`, and whether `n` is the parent's right (inner) child: over the statement's plan tree
/// and its sub-plans, through left / right children only, and with `through` an Append's / MergeAppend's members too (a
/// member's parent is the Append, which no reader rule admits). A node under a Subquery Scan has none (`find_in`).
fn find<'p>(b: &Builder<'p, '_>, n: &'p Plan<'p>, through: bool) -> Option<(Node<'p>, bool)> {
    find_in(b, n, through, false)
}

/// `find`, and with `subquery` through a Subquery Scan's subplan too (the scan the parent of its subplan's root).
fn find_in<'p>(b: &Builder<'p, '_>, n: &'p Plan<'p>, through: bool, subquery: bool) -> Option<(Node<'p>, bool)> {
    let (map, key) = (&b.parents.as_ref()?.map, n as *const Plan<'p> as usize);
    let i = map.partition_point(|e| e.0 < key);
    let (&(k, p, f), again) = (map.get(i)?, map.get(i + 1).is_some_and(|e| e.0 == key));
    (k == key && !again && (through || f & MEMBER == 0) && (subquery || f & SUBQUERY == 0)).then_some((p, f & RIGHT != 0))
}

/// The plan node `n`'s parent and whether `n` is its right child, outside any Append / MergeAppend member (`find`), the
/// memo filled on first use: facts::demand `parent_reads`' walk to a scan's readers.
pub(crate) fn parent_of<'p>(b: &mut Builder<'p, '_>, n: &'p Plan<'p>) -> Option<(Node<'p>, bool)> {
    fill_parents(b);
    find(b, n, false)
}

/// D4497 (K2): `parent_of`, a Subquery Scan's subplan root having the scan as its parent: ExecReScanSubqueryScan rescans
/// its subplan whenever it is rescanned itself, so a walk up to the root passes the scan as any one-child node.
pub(crate) fn parent_through_subquery<'p>(b: &mut Builder<'p, '_>, n: &'p Plan<'p>) -> Option<(Node<'p>, bool)> {
    fill_parents(b);
    find_in(b, n, false, true)
}

const RIGHT: u8 = 1;
const MEMBER: u8 = 2;
const SUBQUERY: u8 = 4;

/// The statement's facts the reader search reads, walked once per statement by the first entry point that searches
/// readers (`rewrite`, `semi_unread`; both hold the Builder exclusively, so `Builder::parents` is a plain memo; only its
/// `lazy` answer is a cell, read under a shared Builder): a walk per lookup made a plan of N SEMIs cost O(N^2)
/// visits. Unfilled, `find` finds nothing and `semi_dead` nulls nothing: every proof fails closed.
pub struct Parents<'p> {
    /// Each plan node's `(address of its Plan, parent, flags)` (`parents`).
    map: ArenaVec<(usize, Node<'p>, u8)>,
    /// `lazy_build`, walked on its first read (`Parents::lazy`): a statement that asks only the map (agg `held_whole`'s
    /// climb to the root, on this memo since C5 (c)) walks no F5 rerun of its whole plan for it, which cost a plan of
    /// many calls a third walk of them per statement (D4554). Every reader holds a shared Builder, hence the cell; none
    /// until the first read.
    lazy: core::cell::Cell<Option<bool>>,
    /// V4R-566: each NestLoop's `uniq` decision.
    uniq: ArenaVec<Uniq<'p>>,
}

fn fill_parents(b: &mut Builder<'_, '_>) {
    if b.parents.is_none() {
        b.parents = Some(Parents { map: parents(b), lazy: core::cell::Cell::new(None), uniq: ArenaVec::new_in(b.arena) });
    }
}

impl<'p> Parents<'p> {
    /// `lazy_build` over `b`'s statement (the Builder that filled the map), walked once, on the first read.
    fn lazy(&self, b: &Builder<'p, '_>) -> bool {
        if let Some(l) = self.lazy.get() {
            return l;
        }
        let l = lazy_build(b);
        self.lazy.set(Some(l));
        l
    }
}

/// D2147 / D2168 (pre-existing on trunk, DEFERRED to the bug phase): the lowerer builds a hash side lazily (D824 / D1005:
/// the side or a pipeline feeding it calls a user function or a VOLATILE builtin, or locks), and an aggregate over such
/// a build crashes the backend. A statement the rule would newly serve holds none: over its whole plan
/// (`rescan::of_plan`: every node, InitPlan, SubPlan and CTE it runs; a build's side is any subtree, its feeders
/// anywhere), no VOLATILE call, no Parallel::Unsafe one (the server's facts: every user function and unknown oid), and
/// no node whose rows may differ per run or that the walk does not know (a lock, a write, Gather). The existing F5
/// fields, no new test: the guard keeps a crash out; it makes no call order or count exact. The acting
/// question is the effects home's (`Effects::of_found`, what `effects::of_plan` reads); the parallel one stays Unsafe-only, not `effects::pool_safe`
/// (Safe-only): a temporary table reads Restricted, and the guard is about user calls, not temporary relations.
fn lazy_build<'p>(b: &Builder<'p, '_>) -> bool {
    use crate::effects::{Effects, Parallel};
    b.stmt.planTree.is_none_or(|t| {
        let f = crate::rerun::of_plan(b.stmt, b.facts, b.arena, t);
        let e = Effects::of_found(f);
        e.acts || !e.same_rows || f.parallel == Parallel::Unsafe
    })
}

/// Each plan node's `(address of its Plan, parent, flags)` by address (`find` takes a node met twice for none): RIGHT
/// the parent's inner child, MEMBER a path from the root through an Append's / MergeAppend's member, SUBQUERY one through
/// a Subquery Scan's subplan (only `find_in` with `subquery` reads those).
fn parents<'p>(b: &Builder<'p, '_>) -> ArenaVec<(usize, Node<'p>, u8)> {
    fn go<'p>(n: Node<'p>, under: u8, out: &mut ArenaVec<(usize, Node<'p>, u8)>) {
        crate::walk::stack_check(); // D3748
        let Some(p) = n.as_plan() else { return };
        let members = n.as_append().map(|a| &a.appendplans).or_else(|| n.as_merge_append().map(|m| &m.mergeplans));
        let kids = [(p.lefttree, under), (p.righttree, under | RIGHT)].into_iter().filter_map(|(k, f)| k.map(|k| (k, f)));
        let sub = n.as_subquery_scan().and_then(|q| q.subplan).map(|k| (k, under | SUBQUERY));
        for (k, f) in kids.chain(members.into_iter().flat_map(|l| l.iter()).map(|k| (k, MEMBER | under & SUBQUERY))).chain(sub) {
            if let Some(kp) = k.as_plan() {
                out.push((kp as *const Plan<'p> as usize, n, f));
            }
            go(k, f & (MEMBER | SUBQUERY), out);
        }
    }
    let mut out = ArenaVec::new_in(b.arena); // alloc: Cold
    b.stmt.planTree.into_iter().chain(b.stmt.subplans.iter().flatten()).for_each(|r| go(r, 0, &mut out));
    out.sort_unstable_by_key(|e| e.0);
    out
}

#[cfg(test)]
mod tests {
    use types_nodes::primnodes::{OpExpr, Param, ParamKind};
    use types_nodes::{Node, NodeList};

    /// AR-20 (overfit, keep): `eq_sides` reads a clause's two sides only of a binary OpExpr (int4 `=` 96 over two
    /// int4 operands). Does not apply: a prefix operator's one argument is not a side (no index past it is read), and a
    /// binary clause over another type is not this type's equality.
    /// The server's answer for int4 `=` (96) alone: one image of int4 on both sides.
    struct Int4Eq;
    impl crate::catalog::Facts for Int4Eq {
        fn rel(&self, _: u32) -> Option<crate::catalog::RelFact> {
            None
        }
        fn att(&self, _: u32, _: i16) -> Option<crate::catalog::AttFact> {
            None
        }
        fn index(&self, _: u32) -> Option<crate::catalog::IndexFact> {
            None
        }
        fn indexes_of(&self, _: u32, _: &mut dyn FnMut(crate::catalog::IndexFact)) {}
        fn index_expr(&self, _: u32, _: u8) -> Option<crate::catalog::ExprFp> {
            None
        }
        fn op_image_eq(&self, opno: u32) -> Option<(u32, u32)> {
            (opno == 96).then_some((types_core::INT4OID, types_core::INT4OID))
        }
    }

    #[test]
    fn eq_sides_reads_only_a_binary_clause() {
        let ctx = mcx::MemoryContext::new("ar-20-image-eq-sides");
        let mcx = ctx.mcx();
        let int4 = types_core::INT4OID;
        let param = |paramid| Node::mk(mcx, Param { paramkind: ParamKind::PARAM_EXEC, paramid, paramtype: int4, paramtypmod: -1, paramcollid: 0, location: -1 }).unwrap();
        let op = |args| Node::mk(mcx, OpExpr { opno: 96, opfuncid: 65, opresulttype: types_core::BOOLOID, args, location: -1, ..OpExpr::default() }).unwrap();
        let (a, b) = (param(1), param(2));
        let sides = super::eq_sides(&Int4Eq, op(NodeList::make2(mcx, a, b).unwrap()), int4, None).expect("a binary int4 equality has two sides");
        assert!(types_nodes::equal(sides.0, a) && types_nodes::equal(sides.1, b));
        assert!(super::eq_sides(&Int4Eq, op(NodeList::make1(mcx, a).unwrap()), int4, None).is_none(), "does not apply: a prefix operator has no sides");
        assert!(super::eq_sides(&Int4Eq, op(NodeList::make2(mcx, a, b).unwrap()), types_core::INT8OID, None).is_none(), "another type's equality");
    }
}

/// D4554: agg `held_whole`'s climb to the root (`parent_of` from the top Agg) fills the map and walks no lazy answer;
/// the first read walks it, once, with `lazy_build`'s answer.
#[cfg(test)]
mod d4554_lazy_on_first_read {
    use super::*;

    #[test]
    fn the_climb_alone_leaves_the_lazy_answer_unwalked() {
        let ctx = mcx::MemoryContext::new("d4554");
        let fx = crate::corpus::load(ctx.mcx(), include_str!("../../../tests/plans/aggs-008.plan")).unwrap();
        let arena = gravity_program::Arena::new(1 << 22);
        let scratch = mcx::MemoryContext::new("d4554 scratch");
        let mut b = Builder::new_in(arena.handle(), fx.stmt, &fx.facts, scratch.mcx());
        let top = fx.stmt.planTree.and_then(|n| n.as_plan()).expect("aggs-008's HashAggregate");
        assert!(parent_of(&mut b, top).is_none(), "the top has no parent");
        let p = b.parents.as_ref().expect("the climb filled the map");
        assert_eq!(p.lazy.get(), None, "the climb walked no lazy answer");
        let first = p.lazy(&b);
        assert_eq!((p.lazy.get(), first), (Some(lazy_build(&b)), lazy_build(&b)), "the first read walks lazy_build's answer and keeps it");
    }
}
