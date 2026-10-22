//! Holistic unnesting (Neumann, BTW 2025), `pgrust.gravity_unnest` = off | strict | contained (default).
//!
//! A correlated scalar aggregate SubLink without GROUP BY becomes a LEFT JOIN to a derived table grouped by the
//! SubLink's bindings, as in decorrelate.rs, but the bindings are pushed into the subselect top-down: an outer reference
//! bound by an `inner = outer` conjunct is substituted (decorrelate.rs's form); any other is replaced by a column of the
//! domain D = SELECT DISTINCT <outer columns> FROM <the outer relation> WHERE <the outer level's eager-safe conjuncts on
//! it alone>, joined where the reference is read. A nested `x IN (SELECT ... [UNION ALL SELECT ...])` conjunct whose arms
//! read the outer level gets D in every arm and becomes a join to SELECT DISTINCT <member, binding> over the arms.
//! Planned as a costed alternative; nothing is built when off. With `pgrust.gravity_unnest_reach` = p2 (the default) a
//! second form joins D into the group of substituted bindings too (`restriction`) for each SubLink admitted only by P2
//! (`plan_time_const`), costed against the full pass; wide does so for every SubLink it can.
//!
//! Admission: the outer level as decorrelate.rs; the SubLink at an unconditional position (target entry top, WHERE
//! conjunct or its direct operand, and down through the arguments of an operator, a function or a relabel at such a
//! position: `args_always_run`, `pgrust.gravity_unnest_args`); D's columns NOT NULL in a relation the outer level does
//! not null-extend (a NULL binding gives the subselect's empty-input value, which the LEFT JOIN's null-extended row
//! reproduces); no volatile function in the subselect. D can hold bindings PostgreSQL never evaluates the SubLink for,
//! so every filter is eager-safe (decorrelate.rs's rule): strict, every expression of the subselect but the wrapper;
//! contained, also not the aggregate arguments, which may then raise for such a binding. A CTE of a level above that
//! the subselect reads is held to it as the subselect's own WITH is: its body wholly eager-safe. The wrapper (the
//! target expression above the aggregates) is lifted to the SubLink's position, so it runs only for outer rows that
//! reach it, as in PostgreSQL, and may read the outer level; an outer reference inside an aggregate is refused.

use core::cell::Cell;

use mcx::Mcx;
use types_core::Oid;
use types_error::PgResult;
use types_nodes::list::{IntList, NodeList, OidList};
use types_nodes::nodes_enums::CmdType;
use types_nodes::parsenodes::{Query, RTEKind, RangeTblEntry, SetOperation, SetOperationStmt, SortGroupClause};
use types_nodes::primnodes::{Alias, FromExpr, JoinExpr, OpExpr, ParamKind, SubLink, SubLinkType, TargetEntry, Var};
use types_nodes::{Bitmapset, JoinType, Node, NodeTag};

use crate::decorrelate::{self, AggLift, KEY_NAMES, MAX_KEYS};

const BOOLOID: Oid = 16;
const SUBQUERY_ALIAS: &str = "gravity_unnested";
const DOMAIN_ALIAS: &str = "gravity_domain";
const SCOPE_ALIAS: &str = "gravity_scope";
const D_NAMES: [&str; MAX_KEYS] = ["d1", "d2", "d3", "d4", "d5", "d6", "d7", "d8"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Strict,
    Contained,
}

/// One walk's settings: the mode, and whether a substituted binding's group is restricted to the outer level's keys
/// (`restriction`); `used` records that some level was.
#[derive(Clone, Copy)]
struct Pass<'a> {
    m: Mode,
    restrict: bool,
    used: &'a Cell<bool>,
}

/// `pgrust.gravity_unnest_reach`: base (0) = the forms of 2026-10-08; p2 (1, the default) also counts a plan-time
/// constant as eager-safe (`plan_time_const`) and offers the restricted form (`restriction`) beside the full one for a
/// SubLink that only P2 admits (the user's 2026-10-08 ruling: P2 only with P2D); wide (2) offers it for every SubLink
/// that can take it (a correlated aggregate subquery over the outer's key: it wins there by PostgreSQL's costs, yet
/// runs slower at width 8).
fn reach() -> u8 {
    if !guc_seams::knob_read::is_installed() {
        return 1;
    }
    match guc_seams::knob_read::call(guc_seams::Knob::GravityUnnestReach) {
        guc_seams::KnobRead::Word(w) => w.min(2),
        _ => 1,
    }
}

/// The unnested form, and when a level could restrict a substituted binding's group to the outer level's keys, the
/// restricted form too: both are planned and costed beside the statement as written.
pub(crate) fn alternatives<'mcx>(mcx: Mcx<'mcx>, parse: &Query<'mcx>) -> PgResult<(Option<&'mcx mut Query<'mcx>>, Option<&'mcx mut Query<'mcx>>)> {
    let m = match guc_tables::backing::pgrust_gravity_unnest() {
        guc_tables::consts::GRAVITY_UNNEST_STRICT => Mode::Strict,
        guc_tables::consts::GRAVITY_UNNEST_CONTAINED => Mode::Contained,
        _ => return Ok((None, None)),
    };
    if !any_sublinks(parse) {
        return Ok((None, None));
    }
    let used = Cell::new(false);
    let Some(q) = level(mcx, parse, Pass { m, restrict: false, used: &used }, &[])? else { return Ok((None, None)) };
    let full = mcx::leak_in(mcx::alloc_in(mcx, copyfuncs::copy_query(mcx, &q)?)?);
    if reach() < 1 {
        return Ok((Some(full), None));
    }
    let restricted = match level(mcx, parse, Pass { m, restrict: true, used: &used }, &[])? {
        Some(r) if used.get() => Some(mcx::leak_in(mcx::alloc_in(mcx, copyfuncs::copy_query(mcx, &r)?)?)),
        _ => None,
    };
    Ok((Some(full), restricted))
}

fn any_sublinks(q: &Query<'_>) -> bool {
    q.hasSubLinks || q.rtable.iter().any(|n| n.as_range_tbl_entry().and_then(|r| r.subquery).is_some_and(any_sublinks))
}

fn level<'mcx>(mcx: Mcx<'mcx>, q: &Query<'mcx>, p: Pass<'_>, up: &[&Query<'mcx>]) -> PgResult<Option<Query<'mcx>>> {
    let mut chain = ::mcx::stdvec::try_to_vec(up, "unnest")?;
    chain.push(q);
    let mut changed = false;
    let mut rtable = NodeList::with_capacity(mcx, q.rtable.len() + 2)?;
    let descend = q.commandType != CmdType::CMD_MERGE;
    for n in q.rtable.iter() {
        let rte = n.as_range_tbl_entry().expect("rtable cell");
        match rte.subquery {
            Some(sub) if descend && rte.rtekind == RTEKind::RTE_SUBQUERY => match level(mcx, sub, p, &chain)? {
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
    let Some(jt) = q.jointree else { return Ok(None) };
    let group_rti = q.rtable.iter().position(|n| n.as_range_tbl_entry().expect("rtable cell").rtekind == RTEKind::RTE_GROUP).map_or(0, |i| i as i32 + 1);
    let mut ctx = Lv { mcx, q, chain: &chain, p, rtable, joins: Vec::new(), outer_ok: decorrelate::outer_admits(q), group_rti };
    let quals = ctx.mutate_quals(jt.quals)?;
    let mut tlist = NodeList::with_capacity(mcx, q.targetList.len())?;
    let mut tlist_changed = false;
    for n in q.targetList.iter() {
        let te = n.as_target_entry().expect("tlist cell");
        match ctx.mutate(te.expr, true)? {
            Some(expr) => {
                tlist.lappend(mcx, Node::mk(mcx, TargetEntry { expr, ..*te })?)?;
                tlist_changed = true;
            }
            None => tlist.lappend(mcx, n)?,
        }
    }
    if !(changed || quals.is_some() || tlist_changed || !ctx.joins.is_empty()) {
        return Ok(None);
    }
    let mut out = crate::subselect::query_cells_copy(mcx, q)?;
    out.rtable = ctx.rtable;
    if tlist_changed {
        out.targetList = tlist;
    }
    let quals = quals.or(jt.quals);
    let fromlist = if ctx.joins.is_empty() {
        jt.fromlist.clone_in(mcx)?
    } else {
        let mut cur = decorrelate::fold_fromlist(mcx, &mut out.rtable, &jt.fromlist)?;
        for j in ctx.joins.drain(..) {
            cur = Node::mk(
                mcx,
                JoinExpr {
                    jointype: JoinType::JOIN_LEFT,
                    isNatural: false,
                    larg: cur,
                    rarg: Node::mk_range_tbl_ref(mcx, j.sub_rti)?,
                    usingClause: NodeList::nil(),
                    join_using_alias: None,
                    quals: Some(j.on),
                    alias: None,
                    rtindex: j.join_rti,
                },
            )?;
        }
        NodeList::make1(mcx, cur)?
    };
    out.jointree = Some(mcx::alloc_leak_in(mcx, FromExpr { fromlist, quals })?);
    Ok(Some(out))
}

/// D3221 (UN-P1): `node` evaluates every argument for each row that reaches it, so a SubLink among its arguments is
/// reached as often as `node` is and keeps an admitted position. An operator or a function that returns no set
/// (PostgreSQL's ExecInitFunc compiles every argument ahead of the call step; a strict call tests its arguments for NULL
/// only after all of them ran) and a binary-compatible relabel (its argument alone). Every other node (BoolExpr, CASE,
/// COALESCE, NULLIF, GREATEST / LEAST, `op ANY (array)`, an aggregate, a window function) may skip an argument or runs it
/// per input row: not admitted below it.
fn args_always_run(node: Node<'_>) -> bool {
    match node.node_tag() {
        NodeTag::T_OpExpr => node.as_op_expr().is_some_and(|o| !o.opretset),
        NodeTag::T_FuncExpr => node.as_func_expr().is_some_and(|f| !f.funcretset),
        NodeTag::T_RelabelType => true,
        _ => false,
    }
}

/// `pgrust.gravity_unnest_args`: off / false / 0 / no admits no position below such a node (the forms before UN-P1);
/// anything else, or unset, does.
fn args_reach() -> bool {
    !(guc_seams::knob_read::is_installed()
        && matches!(guc_seams::knob_read::call(guc_seams::Knob::GravityUnnestArgs), guc_seams::KnobRead::Word(0..=3)))
}

struct PendingJoin<'mcx> {
    sub_rti: i32,
    join_rti: i32,
    on: Node<'mcx>,
}

struct Lv<'a, 'mcx> {
    mcx: Mcx<'mcx>,
    q: &'a Query<'mcx>,
    chain: &'a [&'a Query<'mcx>],
    p: Pass<'a>,
    rtable: NodeList<'mcx>,
    joins: Vec<PendingJoin<'mcx>>,
    outer_ok: bool,
    group_rti: i32,
}

struct Binding<'mcx> {
    key: Node<'mcx>,
    outer: Node<'mcx>,
    op: Option<&'mcx OpExpr<'mcx>>,
    key_left: bool,
}

impl<'a, 'mcx> Lv<'a, 'mcx> {
    fn mutate(&mut self, node: Node<'mcx>, here: bool) -> PgResult<Option<Node<'mcx>>> {
        let mcx = self.mcx;
        if let Some(sl) = node.as_sub_link() {
            if here && self.outer_ok && self.joins.len() < MAX_KEYS {
                if let Some(r) = self.unnest(sl)? {
                    return Ok(Some(r));
                }
            }
            return self.sublink_below(sl);
        }
        if node.node_tag() == NodeTag::T_Aggref {
            // an outer aggregate's arguments are evaluated per input row, its FILTER lazily: below the grouping the join
            // would add, never admitted
            return nodes_core::expression_tree_mutator(mcx, node, &mut |n| self.mutate_no(n));
        }
        let through = here && args_always_run(node) && args_reach();
        nodes_core::expression_tree_mutator(mcx, node, &mut |n| self.mutate(n, through))
    }

    fn mutate_no(&mut self, node: Node<'mcx>) -> PgResult<Option<Node<'mcx>>> {
        if let Some(sl) = node.as_sub_link() {
            return self.sublink_below(sl);
        }
        let mcx = self.mcx;
        nodes_core::expression_tree_mutator(mcx, node, &mut |n| self.mutate_no(n))
    }

    fn mutate_quals(&mut self, quals: Option<Node<'mcx>>) -> PgResult<Option<Node<'mcx>>> {
        let mcx = self.mcx;
        let conjuncts = clauses::make_ands_implicit(mcx, quals)?;
        let mut out = NodeList::with_capacity(mcx, conjuncts.len())?;
        let mut changed = false;
        for c in conjuncts.iter() {
            let new = match c.as_op_expr() {
                Some(op) if op.args.len() == 2 => {
                    let (l, r) = (op.args.nth(0), op.args.nth(1));
                    match (self.mutate(l, true)?, self.mutate(r, true)?) {
                        (None, None) => None,
                        (nl, nr) => {
                            let args = NodeList::make2(mcx, nl.unwrap_or(l), nr.unwrap_or(r))?;
                            Some(Node::mk(mcx, OpExpr { args, ..*op })?)
                        }
                    }
                }
                _ => self.mutate(c, true)?,
            };
            changed |= new.is_some();
            out.lappend(mcx, new.unwrap_or(c))?;
        }
        if !changed {
            return Ok(None);
        }
        Ok(Some(clauses::make_ands_explicit(mcx, &out)?))
    }

    fn sublink_below(&mut self, sl: &SubLink<'mcx>) -> PgResult<Option<Node<'mcx>>> {
        let mcx = self.mcx;
        let sub = sl.subselect.as_query().expect("a transformed SubLink holds a Query");
        let Some(new) = level(mcx, sub, self.p, self.chain)? else { return Ok(None) };
        Ok(Some(Node::mk(
            mcx,
            SubLink {
                subLinkType: sl.subLinkType,
                subLinkId: sl.subLinkId,
                testexpr: sl.testexpr,
                operName: sl.operName.clone_in(mcx)?,
                subselect: Node::mk(mcx, new)?,
                location: sl.location,
            },
        )?))
    }

    fn unnest(&mut self, sl: &SubLink<'mcx>) -> PgResult<Option<Node<'mcx>>> {
        let mcx = self.mcx;
        if sl.subLinkType != SubLinkType::EXPR_SUBLINK || sl.testexpr.is_some() {
            return Ok(None);
        }
        let sub = sl.subselect.as_query().expect("a transformed SubLink holds a Query");
        if !static_aggregate(sub) {
            return Ok(None);
        }
        let sub_node = Node::mk(mcx, crate::subselect::query_cells_copy(mcx, sub)?)?;
        if !vars::contain_vars_of_level(sub_node, 1)? || clauses::contain_volatile_functions(sub_node)? || rewrite_manip::contain_aggs_of_level(sub_node, 1)? {
            return Ok(None);
        }
        let te = sub.targetList.first().and_then(|n| n.as_target_entry()).expect("one target entry");
        // An outer reference inside an aggregate would need the outer value per group in g: refused. One in the wrapper
        // (outside every aggregate) is lifted with it below.
        if outer_in_aggs(te.expr)? || !lift_admits(mcx, te.expr)? {
            return Ok(None);
        }
        // The wrapper runs where the SubLink was, once per outer row reaching it, so neither mode checks it; strict
        // also holds the aggregates' arguments to eager safety, contained only their FILTERs.
        let filters_safe = match self.p.m {
            Mode::Strict => level_below_safe(sub)? && agg_inputs_safe(te.expr, true)?,
            Mode::Contained => level_below_safe(sub)? && !has_sublink(te.expr)? && agg_inputs_safe(te.expr, false)?,
        };
        if !filters_safe || !outer_ctes_safe(sub, self.chain)? {
            return Ok(None);
        }
        let mut bare = crate::subselect::query_cells_copy(mcx, sub)?;
        let jt = sub.jointree.expect("admitted: a jointree");
        bare.jointree = Some(mcx::alloc_leak_in(mcx, FromExpr { fromlist: jt.fromlist.clone_in(mcx)?, quals: None })?);
        bare.targetList = NodeList::nil();
        if vars::contain_vars_of_level(Node::mk(mcx, bare)?, 1)? {
            return Ok(None);
        }
        if let Some(r) = self.unnest_where(sub, te, jt, jt.quals)? {
            return Ok(Some(r));
        }
        match decorrelate::or_factored(mcx, jt.quals)? {
            Some(q) => self.unnest_where(sub, te, jt, Some(q)),
            None => Ok(None),
        }
    }

    fn unnest_where(&mut self, sub: &'mcx Query<'mcx>, te: &'mcx TargetEntry<'mcx>, jt: &'mcx FromExpr<'mcx>, quals: Option<Node<'mcx>>) -> PgResult<Option<Node<'mcx>>> {
        let mcx = self.mcx;
        let mut bindings: Vec<Binding<'mcx>> = Vec::new();
        let mut kept = NodeList::nil();
        let mut domain_conj = NodeList::nil();
        let mut scope: Option<&'mcx SubLink<'mcx>> = None;
        for c in clauses::make_ands_implicit(mcx, quals)?.iter() {
            if !vars::contain_vars_of_level(c, 1)? {
                kept.lappend(mcx, c)?;
                continue;
            }
            if let Some((inner, outer, op, inner_left)) = decorrelate::correlation_key(c)? {
                if (self.group_rti == 0 || !decorrelate::names_rte(outer, self.group_rti, 1)?) && bindings.len() < MAX_KEYS {
                    let outer = rewrite_manip::copy_node(mcx, outer)?;
                    rewrite_manip::IncrementVarSublevelsUp(outer, -1, 1)?;
                    bindings.push(Binding { key: inner, outer, op: Some(op), key_left: inner_left });
                    continue;
                }
            }
            if let Some(asl) = c.as_sub_link() {
                // an ANY whose subselect reads this level's own rows (`e.y = t.y`) stays a SubPlan, as PostgreSQL's
                // convert_ANY_sublink_to_join keeps it: the scope join would read t outside its qualscope
                if scope.is_none() && asl.subLinkType == SubLinkType::ANY_SUBLINK && !vars::contain_vars_of_level(asl.testexpr.expect("ANY testexpr"), 1)? && !vars::contain_vars_of_level(asl.subselect, 1)? {
                    scope = Some(asl);
                    continue;
                }
                return Ok(None);
            }
            if has_sublink(c)? {
                return Ok(None);
            }
            domain_conj.lappend(mcx, c)?;
        }

        if let Some(asl) = scope {
            if !any_is_equality(asl)? {
                return Ok(None);
            }
        }

        let mut refs: Vec<(i32, i16, Oid, i32, Oid)> = Vec::new();
        for c in domain_conj.iter() {
            collect_outer(c, 0, &mut refs)?;
        }
        if let Some(asl) = scope {
            collect_outer(asl.subselect, 1, &mut refs)?;
        }
        let domain = if refs.is_empty() {
            None
        } else {
            let r = refs[0].0;
            if refs.iter().any(|x| x.0 != r) || refs.len() + bindings.len() > MAX_KEYS || !self.domain_admits(r, &refs)? {
                return Ok(None);
            }
            Some((r, refs))
        };
        if bindings.is_empty() && domain.is_none() {
            return Ok(None);
        }

        let mut g = crate::subselect::query_cells_copy(mcx, sub)?;
        let mut fromlist = jt.fromlist.clone_in(mcx)?;
        let mut quals = kept;
        if let Some((r, refs)) = &domain {
            let dq = self.domain_query(*r, refs)?;
            match scope {
                None => {
                    let nd = append_subquery_rte(mcx, &mut g.rtable, dq, DOMAIN_ALIAS, &D_NAMES[..refs.len()])?;
                    fromlist.lappend(mcx, Node::mk_range_tbl_ref(mcx, nd)?)?;
                    for c in domain_conj.iter() {
                        quals.lappend(mcx, replace_outer(mcx, c, 1, *r, refs, nd)?)?;
                    }
                    for (j, x) in refs.iter().enumerate() {
                        bindings.push(Binding { key: dvar(mcx, nd, j, x, 0)?, outer: ovar(mcx, x)?, op: None, key_left: false });
                    }
                }
                Some(asl) => {
                    if !domain_conj.is_nil() || refs_beyond(asl.subselect, 1)? {
                        return Ok(None);
                    }
                    let Some(u) = push_scope(mcx, asl.subselect.as_query().expect("a Query"), 2, *r, refs, &dq)? else { return Ok(None) };
                    let u = sink_one_level(mcx, u)?;
                    let Some((s, member_type)) = scope_query(mcx, u, refs.len())? else { return Ok(None) };
                    let mut names: Vec<&'static str> = vec!["member"];
                    names.extend_from_slice(&D_NAMES[..refs.len()]);
                    let ns = append_subquery_rte(mcx, &mut g.rtable, s, SCOPE_ALIAS, &names)?;
                    fromlist.lappend(mcx, Node::mk_range_tbl_ref(mcx, ns)?)?;
                    let member = Node::mk(mcx, Var { varno: ns, varattno: 1, vartype: member_type.0, vartypmod: member_type.1, varcollid: member_type.2, varnosyn: ns as u32, varattnosyn: 1, ..Default::default() })?;
                    quals.lappend(mcx, replace_sublink_param(mcx, asl.testexpr.expect("ANY testexpr"), member)?)?;
                    for (j, x) in refs.iter().enumerate() {
                        bindings.push(Binding { key: dvar(mcx, ns, j + 1, x, 0)?, outer: ovar(mcx, x)?, op: None, key_left: false });
                    }
                }
            }
        }
        if domain.is_none() && scope.is_none() && self.p.restrict && (reach() >= 2 || relies_on_p2(sub)?) {
            if let Some((r, refs, idx)) = self.restriction(&bindings)? {
                let dq = self.domain_query(r, &refs)?;
                if dq.jointree.is_some_and(|j| j.quals.is_some()) {
                    let nd = append_subquery_rte(mcx, &mut g.rtable, dq, DOMAIN_ALIAS, &D_NAMES[..refs.len()])?;
                    fromlist.lappend(mcx, Node::mk_range_tbl_ref(mcx, nd)?)?;
                    for (j, &i) in idx.iter().enumerate() {
                        let b = &bindings[i];
                        let (key, d) = (rewrite_manip::copy_node(mcx, b.key)?, dvar(mcx, nd, j, &refs[j], 0)?);
                        let args = if b.key_left { NodeList::make2(mcx, key, d)? } else { NodeList::make2(mcx, d, key)? };
                        quals.lappend(mcx, Node::mk(mcx, OpExpr { args, ..*b.op.expect("a restricted binding has its operator") })?)?;
                    }
                    self.p.used.set(true);
                }
            }
        }
        g.jointree = Some(mcx::alloc_leak_in(
            mcx,
            FromExpr { fromlist, quals: if quals.is_nil() { None } else { Some(clauses::make_ands_explicit(mcx, &quals)?) } },
        )?);

        let sub_rti = self.rtable.len() as i32 + 1;
        let join_rti = sub_rti + 1;
        let mut nulling = Bitmapset::empty();
        nulling.add_member(mcx, join_rti)?;
        let mut gtlist = NodeList::nil();
        let mut group = NodeList::nil();
        let mut colnames = NodeList::nil();
        let mut on = NodeList::nil();
        for (i, b) in bindings.iter().enumerate() {
            let (ity, itm) = ::costsize::expr_type_typmod(b.key);
            let icoll = crate::pathkeys::expr_collation(b.key);
            let Some(sgc) = sort_group(mcx, ity, (i + 1) as u32)? else { return Ok(None) };
            let resno = (i + 1) as i16;
            gtlist.lappend(mcx, Node::mk(mcx, TargetEntry { expr: b.key, resno, resname: Some(KEY_NAMES[i]), ressortgroupref: resno as u32, resorigtbl: 0, resorigcol: 0, resjunk: false })?)?;
            group.lappend(mcx, sgc)?;
            colnames.lappend(mcx, Node::mk_string(mcx, KEY_NAMES[i])?)?;
            let gvar = Node::mk(mcx, Var { varno: sub_rti, varattno: resno, vartype: ity, vartypmod: itm, varcollid: icoll, varnosyn: sub_rti as u32, varattnosyn: resno, ..Default::default() })?;
            let cond = match b.op {
                Some(op) => {
                    let args = if b.key_left { NodeList::make2(mcx, gvar, b.outer)? } else { NodeList::make2(mcx, b.outer, gvar)? };
                    Node::mk(mcx, OpExpr { args, ..*op })?
                }
                None => {
                    let tc = typcache::lookup_type_cache(ity, typcache::TYPECACHE_EQ_OPR)?;
                    let eq = tc.eq_opr();
                    if eq == 0 {
                        return Ok(None);
                    }
                    Node::mk(
                        mcx,
                        OpExpr {
                            opno: eq,
                            opfuncid: lsyscache::get_opcode(eq)?,
                            opresulttype: BOOLOID,
                            opretset: false,
                            opcollid: 0,
                            inputcollid: icoll,
                            args: NodeList::make2(mcx, b.outer, gvar)?,
                            location: -1,
                        },
                    )?
                }
            };
            on.lappend(mcx, cond)?;
        }
        let mut lift = AggLift { mcx, sub_rti, nkeys: bindings.len(), next: 0, tlist: NodeList::nil(), colnames: NodeList::nil(), nulling };
        // The lifted wrapper moves up one level: its references to this level and above shift by one (a copy: the
        // shift is in place and the original SubLink is still planned).
        let replacement = rewrite_manip::copy_node(mcx, lift.mutate(te.expr)?.unwrap_or(te.expr))?;
        rewrite_manip::IncrementVarSublevelsUp(replacement, -1, 1)?;
        for n in lift.tlist.iter() {
            gtlist.lappend(mcx, n)?;
        }
        for n in lift.colnames.iter() {
            colnames.lappend(mcx, n)?;
        }
        g.targetList = gtlist;
        g.groupClause = group;
        g.distinctClause = NodeList::nil();
        g.sortClause = NodeList::nil();
        g.hasSubLinks = has_sublink(Node::mk(mcx, crate::subselect::query_cells_copy(mcx, &g)?)?)?;
        let gnode = Node::mk(mcx, g)?;
        if vars::contain_vars_of_level(gnode, 1)? || rewrite_manip::contain_aggs_of_level(gnode, 1)? {
            return Ok(None);
        }
        let g = gnode.as_query().expect("a Query node");
        let alias = mcx::alloc_leak_in(mcx, Alias { aliasname: Some(SUBQUERY_ALIAS), colnames: NodeList::nil() })?;
        let eref = mcx::alloc_leak_in(mcx, Alias { aliasname: Some(SUBQUERY_ALIAS), colnames })?;
        self.rtable.lappend(
            mcx,
            Node::mk(mcx, RangeTblEntry { rtekind: RTEKind::RTE_SUBQUERY, subquery: Some(g), alias: Some(alias), eref: Some(eref), lateral: false, inFromCl: true, ..Default::default() })?,
        )?;
        self.rtable.lappend(mcx, decorrelate::join_rte(mcx, JoinType::JOIN_LEFT)?)?;
        debug_assert_eq!(self.rtable.len() as i32, join_rti);
        self.joins.push(PendingJoin { sub_rti, join_rti, on: clauses::make_ands_explicit(mcx, &on)? });
        Ok(Some(replacement))
    }

    /// P2D (D3221, the user's 2026-10-08 ruling: P2 only with it): the substituted bindings `key = r.col` over one plain
    /// relation entry r of this level, to restrict the group to r's keys: g joins D = SELECT DISTINCT r.col FROM r WHERE
    /// <this level's eager-safe conjuncts on r alone> on the binding's own operator. Every outer row the join back keeps
    /// passes those conjuncts, so its key is in D (a NULL key matches no group either way), and D's DISTINCT under the
    /// same equality joins each inner row at most once. r as the domain's, but a nullable column is fine (D only filters).
    fn restriction(&self, bindings: &[Binding<'mcx>]) -> PgResult<Option<(i32, Vec<(i32, i16, Oid, i32, Oid)>, Vec<usize>)>> {
        let (mut r, mut refs, mut idx) = (0, Vec::new(), Vec::new());
        for (i, b) in bindings.iter().enumerate() {
            let (Some(op), Some(v)) = (b.op, b.outer.as_var()) else { continue };
            if v.varlevelsup != 0 || v.varattno <= 0 || (r != 0 && v.varno != r) || refs.iter().any(|x: &(i32, i16, Oid, i32, Oid)| x.1 == v.varattno) {
                continue;
            }
            if ::costsize::expr_type_typmod(b.key).0 != v.vartype || (lsyscache::type_is_collatable(v.vartype)? && op.inputcollid != v.varcollid) {
                continue;
            }
            r = v.varno;
            refs.push((v.varno, v.varattno, v.vartype, v.vartypmod, v.varcollid));
            idx.push(i);
        }
        if refs.is_empty() || r as usize > self.q.rtable.len() || r == self.group_rti {
            return Ok(None);
        }
        let rte = self.q.rtable.nth((r - 1) as usize).as_range_tbl_entry().expect("rtable cell");
        if rte.rtekind != RTEKind::RTE_RELATION || !rte.securityQuals.is_nil() || rte.tablesample.is_some() {
            return Ok(None);
        }
        for x in &refs {
            if !sortable(x.2)? {
                return Ok(None);
            }
        }
        if self.q.jointree.is_some_and(|jt| jt.fromlist.iter().any(|item| nullable_in(item, r, false))) {
            return Ok(None);
        }
        Ok(Some((r, refs, idx)))
    }

    /// The domain's relation: a plain relation entry of this level (no RLS quals), never null-extended by this level's
    /// joins, every referenced column NOT NULL and not a GROUP entry.
    fn domain_admits(&self, r: i32, refs: &[(i32, i16, Oid, i32, Oid)]) -> PgResult<bool> {
        if r <= 0 || r as usize > self.q.rtable.len() || r == self.group_rti {
            return Ok(false);
        }
        let rte = self.q.rtable.nth((r - 1) as usize).as_range_tbl_entry().expect("rtable cell");
        if rte.rtekind != RTEKind::RTE_RELATION || !rte.securityQuals.is_nil() || rte.tablesample.is_some() {
            return Ok(false);
        }
        for x in refs {
            if !sortable(x.2)? {
                return Ok(false);
            }
        }
        if let Some(jt) = self.q.jointree {
            for item in jt.fromlist.iter() {
                if nullable_in(item, r, false) {
                    return Ok(false);
                }
            }
        }
        let rel = table::table_open(self.mcx, rte.relid, types_rel::NoLock)?;
        let mut ok = true;
        for x in refs {
            let i = x.1 as usize;
            ok = ok && x.1 > 0 && i <= rel.rd_att.natts as usize && rel.rd_att.compact_attr(i - 1).attnullability == types_tuple::tupdesc::ATTNULLABLE_VALID;
        }
        table::table_close(rel, types_rel::NoLock)?;
        for x in refs {
            ok = ok && equal_image(self.mcx, x.2, x.4)?;
        }
        Ok(ok)
    }

    fn domain_query(&self, r: i32, refs: &[(i32, i16, Oid, i32, Oid)]) -> PgResult<Query<'mcx>> {
        let mcx = self.mcx;
        let rte = self.q.rtable.nth((r - 1) as usize).as_range_tbl_entry().expect("rtable cell");
        let mut perms = NodeList::nil();
        let pidx = if rte.perminfoindex > 0 {
            perms.lappend(mcx, self.q.rteperminfos.nth((rte.perminfoindex - 1) as usize))?;
            1
        } else {
            0
        };
        let copy = crate::prepjointree::rte_copy_with_perminfoindex(mcx, rte, pidx)?;
        // SAFETY: the copy is exclusively ours until it enters the list.
        unsafe { copy.with_mut::<RangeTblEntry, _>(|e| e.inFromCl = true) };
        // D only needs the bindings of rows that reach the SubLink: a WHERE conjunct of this level reading r alone filters
        // r's rows before any reaches it (r is never null-extended here), so it restricts D too when it cannot raise.
        let mut dquals = NodeList::nil();
        if let Some(jt) = self.q.jointree {
            for c in clauses::make_ands_implicit(mcx, jt.quals)?.iter() {
                if reads_only(c, r)? && expr_safe(c)? {
                    let c = rewrite_manip::copy_node(mcx, c)?;
                    rewrite_manip::ChangeVarNodes(mcx, c, r, 1, 0)?;
                    dquals.lappend(mcx, c)?;
                }
            }
        }
        let mut tlist = NodeList::nil();
        let mut distinct = NodeList::nil();
        for (j, x) in refs.iter().enumerate() {
            let resno = (j + 1) as i16;
            let v = Node::mk(mcx, Var { varno: 1, varattno: x.1, vartype: x.2, vartypmod: x.3, varcollid: x.4, varnosyn: 1, varattnosyn: x.1, ..Default::default() })?;
            tlist.lappend(mcx, Node::mk(mcx, TargetEntry { expr: v, resno, resname: Some(D_NAMES[j]), ressortgroupref: resno as u32, resorigtbl: 0, resorigcol: 0, resjunk: false })?)?;
            let Some(sgc) = sort_group(mcx, x.2, resno as u32)? else { return Err(types_error::PgError::error("unnest: domain column without btree equality".to_string()).into()) };
            distinct.lappend(mcx, sgc)?;
        }
        Ok(Query {
            commandType: CmdType::CMD_SELECT,
            canSetTag: true,
            rtable: NodeList::make1(mcx, copy)?,
            rteperminfos: perms,
            jointree: Some(mcx::alloc_leak_in(
                mcx,
                FromExpr {
                    fromlist: NodeList::make1(mcx, Node::mk_range_tbl_ref(mcx, 1)?)?,
                    quals: if dquals.is_nil() { None } else { Some(clauses::make_ands_explicit(mcx, &dquals)?) },
                },
            )?),
            targetList: tlist,
            distinctClause: distinct,
            ..Query::default()
        })
    }
}

fn static_aggregate(sub: &Query<'_>) -> bool {
    sub.commandType == CmdType::CMD_SELECT
        && sub.hasAggs
        && !sub.hasWindowFuncs
        && !sub.hasTargetSRFs
        && !sub.hasDistinctOn
        && !sub.hasForUpdate
        && !sub.hasGroupRTE
        && sub.cteList.is_nil()
        && sub.groupClause.is_nil()
        && sub.groupingSets.is_nil()
        && sub.havingQual.is_none()
        && sub.limitOffset.is_none()
        && sub.limitCount.is_none()
        && sub.setOperations.is_none()
        && sub.rowMarks.is_nil()
        && sub.windowClause.is_nil()
        && sub.targetList.len() == 1
        && sub.jointree.is_some_and(|jt| !jt.fromlist.is_nil())
        && sub.rtable.iter().all(|n| {
            let rte = n.as_range_tbl_entry().expect("rtable cell");
            !rte.lateral && rte.rtekind != RTEKind::RTE_JOIN
        })
}

fn lift_admits<'mcx>(mcx: Mcx<'mcx>, expr: Node<'mcx>) -> PgResult<bool> {
    struct W<'mcx> {
        mcx: Mcx<'mcx>,
        n: usize,
        ok: bool,
    }
    impl<'mcx> nodes_core::NodeWalker<'mcx> for W<'mcx> {
        fn visit(&mut self, n: Node<'mcx>) -> PgResult<bool> {
            if let Some(agg) = n.as_aggref() {
                self.n += 1;
                if decorrelate::agg_admits(self.mcx, agg)?.is_none() {
                    self.ok = false;
                    return Ok(true);
                }
                return Ok(false);
            }
            if n.as_var().is_some_and(|v| v.varlevelsup == 0) || n.node_tag() == NodeTag::T_SubLink {
                self.ok = false;
                return Ok(true);
            }
            nodes_core::expression_tree_walker(n, self)
        }
    }
    let mut w = W { mcx, n: 0, ok: true };
    nodes_core::NodeWalker::visit(&mut w, expr)?;
    Ok(w.ok && w.n > 0 && w.n <= 8)
}

fn has_sublink(node: Node<'_>) -> PgResult<bool> {
    struct W;
    impl<'m> nodes_core::NodeWalker<'m> for W {
        fn visit(&mut self, n: Node<'m>) -> PgResult<bool> {
            if n.node_tag() == NodeTag::T_SubLink {
                return Ok(true);
            }
            if let Some(q) = n.as_query() {
                return nodes_core::query_tree_walker(q, self, 0);
            }
            nodes_core::expression_tree_walker(n, self)
        }
        fn visit_query_ref(&mut self, q: &'m Query<'m>) -> PgResult<bool> {
            nodes_core::query_tree_walker(q, self, 0)
        }
    }
    nodes_core::NodeWalker::visit(&mut W, node)
}

fn collect_outer(node: Node<'_>, base: u32, out: &mut Vec<(i32, i16, Oid, i32, Oid)>) -> PgResult<()> {
    struct W<'v> {
        depth: u32,
        base: u32,
        out: &'v mut Vec<(i32, i16, Oid, i32, Oid)>,
    }
    impl<'m> nodes_core::NodeWalker<'m> for W<'_> {
        fn visit(&mut self, n: Node<'m>) -> PgResult<bool> {
            if let Some(v) = n.as_var() {
                if v.varlevelsup == self.base + self.depth + 1 {
                    let x = (v.varno, v.varattno, v.vartype, v.vartypmod, v.varcollid);
                    if !self.out.iter().any(|y| y.0 == x.0 && y.1 == x.1) {
                        self.out.push(x);
                    }
                }
                return Ok(false);
            }
            if let Some(q) = n.as_query() {
                self.depth += 1;
                let r = nodes_core::query_tree_walker(q, self, 0)?;
                self.depth -= 1;
                return Ok(r);
            }
            nodes_core::expression_tree_walker(n, self)
        }
        fn visit_query_ref(&mut self, q: &'m Query<'m>) -> PgResult<bool> {
            self.depth += 1;
            let r = nodes_core::query_tree_walker(q, self, 0)?;
            self.depth -= 1;
            Ok(r)
        }
    }
    let mut w = W { depth: 0, base, out };
    match node.as_query() {
        Some(q) => {
            nodes_core::query_tree_walker(q, &mut w, 0)?;
        }
        None => {
            nodes_core::NodeWalker::visit(&mut w, node)?;
        }
    }
    Ok(())
}

/// Whether `node`, an expression of a level, reads relation entry `r` of that level and nothing else: no other Var, no
/// SubLink, aggregate, window or grouping function, PlaceHolderVar or executor Param, no volatile function.
fn reads_only(node: Node<'_>, r: i32) -> PgResult<bool> {
    struct W {
        r: i32,
        ok: bool,
        n: u32,
    }
    impl<'m> nodes_core::NodeWalker<'m> for W {
        fn visit(&mut self, n: Node<'m>) -> PgResult<bool> {
            if let Some(v) = n.as_var() {
                if v.varlevelsup != 0 || v.varno != self.r {
                    self.ok = false;
                    return Ok(true);
                }
                self.n += 1;
                return Ok(false);
            }
            if let Some(p) = n.as_param() {
                if p.paramkind != ParamKind::PARAM_EXTERN {
                    self.ok = false;
                    return Ok(true);
                }
                return Ok(false);
            }
            if matches!(
                n.node_tag(),
                NodeTag::T_SubLink | NodeTag::T_Aggref | NodeTag::T_WindowFunc | NodeTag::T_GroupingFunc | NodeTag::T_PlaceHolderVar | NodeTag::T_Query
            ) {
                self.ok = false;
                return Ok(true);
            }
            nodes_core::expression_tree_walker(n, self)
        }
    }
    let mut w = W { r, ok: true, n: 0 };
    nodes_core::NodeWalker::visit(&mut w, node)?;
    Ok(w.ok && w.n > 0 && !clauses::contain_volatile_functions(node)?)
}

fn refs_beyond(node: Node<'_>, base: u32) -> PgResult<bool> {
    struct W {
        depth: u32,
        base: u32,
    }
    impl<'m> nodes_core::NodeWalker<'m> for W {
        fn visit(&mut self, n: Node<'m>) -> PgResult<bool> {
            if let Some(v) = n.as_var() {
                return Ok(v.varlevelsup > self.base + self.depth + 1);
            }
            if let Some(a) = n.as_aggref() {
                if a.agglevelsup > self.base + self.depth + 1 {
                    return Ok(true);
                }
            }
            if let Some(q) = n.as_query() {
                return self.visit_query_ref(q);
            }
            nodes_core::expression_tree_walker(n, self)
        }
        fn visit_query_ref(&mut self, q: &'m Query<'m>) -> PgResult<bool> {
            self.depth += 1;
            let r = nodes_core::query_tree_walker(q, self, 0)?;
            self.depth -= 1;
            Ok(r)
        }
    }
    let mut w = W { depth: 0, base };
    match node.as_query() {
        Some(q) => nodes_core::query_tree_walker(q, &mut w, 0),
        None => nodes_core::NodeWalker::visit(&mut w, node),
    }
}

fn nullable_in(node: Node<'_>, r: i32, under: bool) -> bool {
    if let Some(t) = node.as_range_tbl_ref() {
        return under && t.rtindex == r;
    }
    if let Some(j) = node.as_join_expr() {
        let (l, rr) = match j.jointype {
            JoinType::JOIN_LEFT | JoinType::JOIN_ANTI => (under, true),
            JoinType::JOIN_RIGHT => (true, under),
            JoinType::JOIN_FULL => (true, true),
            _ => (under, under),
        };
        return nullable_in(j.larg, r, l) || nullable_in(j.rarg, r, rr);
    }
    if let Some(f) = node.as_from_expr() {
        return f.fromlist.iter().any(|i| nullable_in(i, r, under));
    }
    false
}

fn sortable(ty: Oid) -> PgResult<bool> {
    let tc = typcache::lookup_type_cache(ty, typcache::TYPECACHE_EQ_OPR | typcache::TYPECACHE_LT_OPR)?;
    Ok(tc.eq_opr() != 0 && tc.lt_opr() != 0)
}

fn sort_group<'mcx>(mcx: Mcx<'mcx>, ty: Oid, sgref: u32) -> PgResult<Option<Node<'mcx>>> {
    let tc = typcache::lookup_type_cache(ty, typcache::TYPECACHE_EQ_OPR | typcache::TYPECACHE_LT_OPR | typcache::TYPECACHE_HASH_PROC)?;
    if tc.eq_opr() == 0 || tc.lt_opr() == 0 {
        return Ok(None);
    }
    Ok(Some(Node::mk(
        mcx,
        SortGroupClause { tleSortGroupRef: sgref, eqop: tc.eq_opr(), sortop: tc.lt_opr(), reverse_sort: false, nulls_first: false, hashable: tc.hash_proc() != 0 },
    )?))
}

fn append_subquery_rte<'mcx>(mcx: Mcx<'mcx>, rtable: &mut NodeList<'mcx>, q: Query<'mcx>, name: &'static str, cols: &[&'static str]) -> PgResult<i32> {
    let mut colnames = NodeList::nil();
    for c in cols {
        colnames.lappend(mcx, Node::mk_string(mcx, c)?)?;
    }
    let alias = mcx::alloc_leak_in(mcx, Alias { aliasname: Some(name), colnames: NodeList::nil() })?;
    let eref = mcx::alloc_leak_in(mcx, Alias { aliasname: Some(name), colnames })?;
    let q = mcx::alloc_leak_in(mcx, q)?;
    rtable.lappend(mcx, Node::mk(mcx, RangeTblEntry { rtekind: RTEKind::RTE_SUBQUERY, subquery: Some(q), alias: Some(alias), eref: Some(eref), lateral: false, inFromCl: true, ..Default::default() })?)?;
    Ok(rtable.len() as i32)
}

fn dvar<'mcx>(mcx: Mcx<'mcx>, varno: i32, j: usize, x: &(i32, i16, Oid, i32, Oid), levelsup: u32) -> PgResult<Node<'mcx>> {
    Node::mk(mcx, Var { varno, varattno: (j + 1) as i16, vartype: x.2, vartypmod: x.3, varcollid: x.4, varlevelsup: levelsup, varnosyn: varno as u32, varattnosyn: (j + 1) as i16, ..Default::default() })
}

fn ovar<'mcx>(mcx: Mcx<'mcx>, x: &(i32, i16, Oid, i32, Oid)) -> PgResult<Node<'mcx>> {
    Node::mk(mcx, Var { varno: x.0, varattno: x.1, vartype: x.2, vartypmod: x.3, varcollid: x.4, varnosyn: x.0 as u32, varattnosyn: x.1, ..Default::default() })
}

fn replace_outer<'mcx>(mcx: Mcx<'mcx>, node: Node<'mcx>, base: u32, r: i32, refs: &[(i32, i16, Oid, i32, Oid)], nd: i32) -> PgResult<Node<'mcx>> {
    fn m<'mcx>(mcx: Mcx<'mcx>, n: Node<'mcx>, base: u32, r: i32, refs: &[(i32, i16, Oid, i32, Oid)], nd: i32) -> PgResult<Option<Node<'mcx>>> {
        if let Some(v) = n.as_var() {
            if v.varlevelsup == base && v.varno == r {
                let j = refs.iter().position(|x| x.1 == v.varattno).expect("a collected reference");
                return Ok(Some(dvar(mcx, nd, j, &refs[j], 0)?));
            }
            return Ok(None);
        }
        nodes_core::expression_tree_mutator(mcx, n, &mut |c| m(mcx, c, base, r, refs, nd))
    }
    Ok(m(mcx, node, base, r, refs, nd)?.unwrap_or(node))
}

const BTEQUALIMAGE_PROC: i16 = 4;

fn equal_image(mcx: Mcx<'_>, ty: Oid, coll: Oid) -> PgResult<bool> {
    let tc = typcache::lookup_type_cache(ty, typcache::TYPECACHE_BTREE_OPFAMILY)?;
    let (opf, opcintype) = (tc.btree_opf(), tc.btree_opintype());
    if opf == 0 {
        return Ok(false);
    }
    let proc = lsyscache::get_opfamily_proc(opf, opcintype, opcintype, BTEQUALIMAGE_PROC)?;
    if proc == 0 {
        return Ok(false);
    }
    Ok(fmgr_core::oid_function_call1_coll_in(proc, coll, mcx, datum::Datum::from_oid(opcintype))?.as_bool())
}

fn any_is_equality(asl: &SubLink<'_>) -> PgResult<bool> {
    let Some(op) = asl.testexpr.and_then(|t| t.as_op_expr()) else { return Ok(false) };
    if op.args.len() != 2 || op.opretset {
        return Ok(false);
    }
    let is_member = |n: Node<'_>| n.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_SUBLINK && p.paramid == 1);
    let member = match (is_member(op.args.nth(0)), is_member(op.args.nth(1))) {
        (true, false) => op.args.nth(0),
        (false, true) => op.args.nth(1),
        _ => return Ok(false),
    };
    let (mty, _) = ::costsize::expr_type_typmod(member);
    let tc = typcache::lookup_type_cache(mty, typcache::TYPECACHE_EQ_OPR | typcache::TYPECACHE_BTREE_OPFAMILY)?;
    if tc.btree_opf() == 0 || lsyscache::get_op_opfamily_strategy(op.opno, tc.btree_opf())? != lsyscache::BTEqualStrategyNumber as i32 {
        return Ok(false);
    }
    Ok(!lsyscache::type_is_collatable(mty)? || op.inputcollid == crate::pathkeys::expr_collation(member))
}

fn replace_sublink_param<'mcx>(mcx: Mcx<'mcx>, testexpr: Node<'mcx>, member: Node<'mcx>) -> PgResult<Node<'mcx>> {
    fn m<'mcx>(mcx: Mcx<'mcx>, n: Node<'mcx>, member: Node<'mcx>) -> PgResult<Option<Node<'mcx>>> {
        if let Some(p) = n.as_param() {
            if p.paramkind == ParamKind::PARAM_SUBLINK && p.paramid == 1 {
                return Ok(Some(member));
            }
            return Ok(None);
        }
        nodes_core::expression_tree_mutator(mcx, n, &mut |c| m(mcx, c, member))
    }
    Ok(m(mcx, testexpr, member)?.unwrap_or(testexpr))
}

fn push_scope<'mcx>(mcx: Mcx<'mcx>, u: &Query<'mcx>, base: u32, r: i32, refs: &[(i32, i16, Oid, i32, Oid)], dq: &Query<'mcx>) -> PgResult<Option<Query<'mcx>>> {
    if u.commandType != CmdType::CMD_SELECT || u.targetList.iter().filter(|n| !n.as_target_entry().expect("tlist cell").resjunk).count() != 1 {
        return Ok(None);
    }
    match u.setOperations {
        None => push_arm(mcx, u, base, r, refs, dq),
        Some(set) => {
            if !u.cteList.is_nil() || u.limitCount.is_some() || u.limitOffset.is_some() || !u.sortClause.is_nil() {
                return Ok(None);
            }
            let mut out = crate::subselect::query_cells_copy(mcx, u)?;
            let mut rtable = NodeList::nil();
            for (i, n) in u.rtable.iter().enumerate() {
                let rte = n.as_range_tbl_entry().expect("rtable cell");
                if !in_setop(set, (i + 1) as i32) {
                    rtable.lappend(mcx, n)?;
                    continue;
                }
                let Some(arm) = rte.subquery else { return Ok(None) };
                let Some(arm2) = push_arm(mcx, arm, base + 1, r, refs, dq)? else { return Ok(None) };
                let copy = crate::prepjointree::rte_copy_with_perminfoindex(mcx, rte, rte.perminfoindex)?;
                let mut cols = NodeList::nil();
                if let Some(e) = rte.eref {
                    for c in e.colnames.iter() {
                        cols.lappend(mcx, c)?;
                    }
                }
                for d in D_NAMES[..refs.len()].iter() {
                    cols.lappend(mcx, Node::mk_string(mcx, d)?)?;
                }
                let eref = mcx::alloc_leak_in(mcx, Alias { aliasname: rte.eref.and_then(|e| e.aliasname), colnames: cols })?;
                let arm2 = mcx::alloc_leak_in(mcx, arm2)?;
                // SAFETY: the copy is exclusively ours until it enters the list.
                unsafe {
                    copy.with_mut::<RangeTblEntry, _>(|e| {
                        e.subquery = Some(arm2);
                        e.eref = Some(eref);
                    })
                };
                rtable.lappend(mcx, copy)?;
            }
            let Some(set2) = widen_setop(mcx, set, refs)? else { return Ok(None) };
            out.rtable = rtable;
            out.setOperations = Some(set2);
            let left = leftmost(set);
            let te0 = u.targetList.nth(0).as_target_entry().expect("tlist cell");
            let mut tlist = NodeList::make1(mcx, u.targetList.nth(0))?;
            for (j, x) in refs.iter().enumerate() {
                let resno = te0.resno + 1 + j as i16;
                let v = Node::mk(mcx, Var { varno: left, varattno: resno, vartype: x.2, vartypmod: x.3, varcollid: x.4, varnosyn: left as u32, varattnosyn: resno, ..Default::default() })?;
                tlist.lappend(mcx, Node::mk(mcx, TargetEntry { expr: v, resno, resname: Some(D_NAMES[j]), ressortgroupref: 0, resorigtbl: 0, resorigcol: 0, resjunk: false })?)?;
            }
            out.targetList = tlist;
            Ok(Some(out))
        }
    }
}

fn in_setop(n: Node<'_>, rti: i32) -> bool {
    if let Some(r) = n.as_range_tbl_ref() {
        return r.rtindex == rti;
    }
    n.as_set_operation_stmt().is_some_and(|s| s.larg.is_some_and(|l| in_setop(l, rti)) || s.rarg.is_some_and(|r| in_setop(r, rti)))
}

fn leftmost(n: Node<'_>) -> i32 {
    if let Some(r) = n.as_range_tbl_ref() {
        return r.rtindex;
    }
    leftmost(n.as_set_operation_stmt().and_then(|s| s.larg).expect("a set operation's left input"))
}

fn widen_setop<'mcx>(mcx: Mcx<'mcx>, n: Node<'mcx>, refs: &[(i32, i16, Oid, i32, Oid)]) -> PgResult<Option<Node<'mcx>>> {
    if n.as_range_tbl_ref().is_some() {
        return Ok(Some(n));
    }
    let Some(s) = n.as_set_operation_stmt() else { return Ok(None) };
    if s.op != SetOperation::SETOP_UNION || !s.all {
        return Ok(None);
    }
    let (Some(l), Some(r)) = (s.larg, s.rarg) else { return Ok(None) };
    let (Some(l), Some(r)) = (widen_setop(mcx, l, refs)?, widen_setop(mcx, r, refs)?) else { return Ok(None) };
    let mut types = OidList::nil();
    let mut typmods = IntList::nil();
    let mut colls = OidList::nil();
    for t in s.colTypes.iter() {
        types.lappend(mcx, t)?;
    }
    for t in s.colTypmods.iter() {
        typmods.lappend(mcx, t)?;
    }
    for t in s.colCollations.iter() {
        colls.lappend(mcx, t)?;
    }
    for x in refs {
        types.lappend(mcx, x.2)?;
        typmods.lappend(mcx, x.3)?;
        colls.lappend(mcx, x.4)?;
    }
    Ok(Some(Node::mk(
        mcx,
        SetOperationStmt { op: s.op, all: s.all, larg: Some(l), rarg: Some(r), colTypes: types, colTypmods: typmods, colCollations: colls, groupClauses: NodeList::nil() },
    )?))
}

fn push_arm<'mcx>(mcx: Mcx<'mcx>, arm: &Query<'mcx>, base: u32, r: i32, refs: &[(i32, i16, Oid, i32, Oid)], dq: &Query<'mcx>) -> PgResult<Option<Query<'mcx>>> {
    if arm.commandType != CmdType::CMD_SELECT
        || arm.hasAggs
        || arm.hasWindowFuncs
        || arm.hasTargetSRFs
        || arm.hasSubLinks
        || arm.setOperations.is_some()
        || !arm.cteList.is_nil()
        || !arm.groupClause.is_nil()
        || !arm.groupingSets.is_nil()
        || arm.havingQual.is_some()
        || !arm.distinctClause.is_nil()
        || arm.limitCount.is_some()
        || arm.limitOffset.is_some()
        || !arm.rowMarks.is_nil()
    {
        return Ok(None);
    }
    let Some(jt) = arm.jointree else { return Ok(None) };
    // The outer level only in the WHERE conjuncts and target entries (never a join qual or a range-table entry).
    let mut bare = crate::subselect::query_cells_copy(mcx, arm)?;
    bare.jointree = Some(mcx::alloc_leak_in(mcx, FromExpr { fromlist: jt.fromlist.clone_in(mcx)?, quals: None })?);
    bare.targetList = NodeList::nil();
    let mut other = Vec::new();
    collect_outer(Node::mk(mcx, bare)?, base - 1, &mut other)?;
    if !other.is_empty() {
        return Ok(None);
    }
    let mut found = Vec::new();
    if let Some(w) = jt.quals {
        collect_outer(w, base - 1, &mut found)?;
    }
    for n in arm.targetList.iter() {
        collect_outer(n.as_target_entry().expect("tlist cell").expr, base - 1, &mut found)?;
    }
    if found.iter().any(|x| x.0 != r || !refs.iter().any(|y| y.1 == x.1)) {
        return Ok(None);
    }
    if let Some(out) = substitute_arm(mcx, arm, jt, base, r, refs)? {
        return Ok(Some(out));
    }
    let mut out = crate::subselect::query_cells_copy(mcx, arm)?;
    let nd = append_subquery_rte(mcx, &mut out.rtable, copy_domain(mcx, dq)?, DOMAIN_ALIAS, &D_NAMES[..refs.len()])?;
    let mut fromlist = jt.fromlist.clone_in(mcx)?;
    fromlist.lappend(mcx, Node::mk_range_tbl_ref(mcx, nd)?)?;
    let quals = match jt.quals {
        Some(w) => Some(replace_outer(mcx, w, base, r, refs, nd)?),
        None => None,
    };
    out.jointree = Some(mcx::alloc_leak_in(mcx, FromExpr { fromlist, quals })?);
    let mut tlist = NodeList::nil();
    let mut last = 0i16;
    for n in arm.targetList.iter() {
        let te = n.as_target_entry().expect("tlist cell");
        if te.resjunk {
            return Ok(None);
        }
        last = te.resno;
        tlist.lappend(mcx, Node::mk(mcx, TargetEntry { expr: replace_outer(mcx, te.expr, base, r, refs, nd)?, ..*te })?)?;
    }
    for (j, x) in refs.iter().enumerate() {
        let resno = last + 1 + j as i16;
        tlist.lappend(mcx, Node::mk(mcx, TargetEntry { expr: dvar(mcx, nd, j, x, 0)?, resno, resname: Some(D_NAMES[j]), ressortgroupref: 0, resorigtbl: 0, resorigcol: 0, resjunk: false })?)?;
    }
    out.targetList = tlist;
    Ok(Some(out))
}

/// The arm without the domain when WHERE conjuncts `col = outer` (the type's own btree equality, same type, typmod and
/// collation) bind every reference and nothing else in the arm reads the outer level: the binding is that column (the
/// paper's substitution). The arm then also yields bindings outside D; their groups find no outer row in the join back.
fn substitute_arm<'mcx>(
    mcx: Mcx<'mcx>,
    arm: &Query<'mcx>,
    jt: &FromExpr<'mcx>,
    base: u32,
    r: i32,
    refs: &[(i32, i16, Oid, i32, Oid)],
) -> PgResult<Option<Query<'mcx>>> {
    let conj = clauses::make_ands_implicit(mcx, jt.quals)?;
    let mut by: Vec<Option<Node<'mcx>>> = ::mcx::stdvec::try_filled(refs.len(), None, "unnest")?;
    let mut used = ::mcx::stdvec::try_filled(conj.len(), false, "unnest")?;
    for (i, c) in conj.iter().enumerate() {
        let Some(op) = c.as_op_expr() else { continue };
        if op.args.len() != 2 {
            continue;
        }
        for (o, l) in [(op.args.nth(0), op.args.nth(1)), (op.args.nth(1), op.args.nth(0))] {
            let (Some(ov), Some(lv)) = (o.as_var(), l.as_var()) else { continue };
            if ov.varlevelsup != base || ov.varno != r || lv.varlevelsup != 0 {
                continue;
            }
            let Some(j) = refs.iter().position(|x| x.1 == ov.varattno) else { continue };
            let x = &refs[j];
            if by[j].is_some() || lv.vartype != x.2 || lv.vartypmod != x.3 || lv.varcollid != x.4 {
                continue;
            }
            let tc = typcache::lookup_type_cache(x.2, typcache::TYPECACHE_EQ_OPR)?;
            if tc.eq_opr() == 0 || tc.eq_opr() != op.opno {
                continue;
            }
            by[j] = Some(l);
            used[i] = true;
            break;
        }
    }
    if by.iter().any(|b| b.is_none()) {
        return Ok(None);
    }
    let by: Vec<Node<'mcx>> = by.into_iter().map(|b| b.expect("checked")).collect();
    let mut kept = NodeList::nil();
    let mut rest = Vec::new();
    for (i, c) in conj.iter().enumerate() {
        if !used[i] {
            collect_outer(c, base - 1, &mut rest)?;
            kept.lappend(mcx, c)?;
        }
    }
    for n in arm.targetList.iter() {
        collect_outer(n.as_target_entry().expect("tlist cell").expr, base - 1, &mut rest)?;
    }
    if !rest.is_empty() {
        return Ok(None);
    }
    let mut out = crate::subselect::query_cells_copy(mcx, arm)?;
    out.jointree = Some(mcx::alloc_leak_in(
        mcx,
        FromExpr { fromlist: jt.fromlist.clone_in(mcx)?, quals: if kept.is_nil() { None } else { Some(clauses::make_ands_explicit(mcx, &kept)?) } },
    )?);
    let mut tlist = NodeList::nil();
    let mut last = 0i16;
    for n in arm.targetList.iter() {
        let te = n.as_target_entry().expect("tlist cell");
        if te.resjunk {
            return Ok(None);
        }
        last = te.resno;
        tlist.lappend(mcx, n)?;
    }
    for (j, b) in by.iter().enumerate() {
        let resno = last + 1 + j as i16;
        tlist.lappend(mcx, Node::mk(mcx, TargetEntry { expr: rewrite_manip::copy_node(mcx, *b)?, resno, resname: Some(D_NAMES[j]), ressortgroupref: 0, resorigtbl: 0, resorigcol: 0, resjunk: false })?)?;
    }
    out.targetList = tlist;
    Ok(Some(out))
}

fn copy_domain<'mcx>(mcx: Mcx<'mcx>, dq: &Query<'mcx>) -> PgResult<Query<'mcx>> {
    crate::subselect::query_cells_copy(mcx, dq)
}

fn sink_one_level<'mcx>(mcx: Mcx<'mcx>, u: Query<'mcx>) -> PgResult<Query<'mcx>> {
    let n = rewrite_manip::copy_node(mcx, Node::mk(mcx, u)?)?;
    rewrite_manip::IncrementVarSublevelsUp(n, 1, 1)?;
    crate::subselect::query_cells_copy(mcx, n.as_query().expect("a Query"))
}

fn scope_query<'mcx>(mcx: Mcx<'mcx>, u: Query<'mcx>, nrefs: usize) -> PgResult<Option<(Query<'mcx>, (Oid, i32, Oid))>> {
    let mut cols: Vec<(i16, Oid, i32, Oid)> = Vec::new();
    for n in u.targetList.iter() {
        let te = n.as_target_entry().expect("tlist cell");
        if te.resjunk {
            continue;
        }
        let (t, tm) = ::costsize::expr_type_typmod(te.expr);
        cols.push((te.resno, t, tm, crate::pathkeys::expr_collation(te.expr)));
    }
    if cols.len() != 1 + nrefs {
        return Ok(None);
    }
    let mut names: Vec<&'static str> = vec!["member"];
    names.extend_from_slice(&D_NAMES[..nrefs]);
    let mut rtable = NodeList::nil();
    append_subquery_rte(mcx, &mut rtable, u, SCOPE_ALIAS, &names)?;
    let mut tlist = NodeList::nil();
    let mut distinct = NodeList::nil();
    for (i, c) in cols.iter().enumerate() {
        let resno = (i + 1) as i16;
        let v = Node::mk(mcx, Var { varno: 1, varattno: c.0, vartype: c.1, vartypmod: c.2, varcollid: c.3, varnosyn: 1, varattnosyn: c.0, ..Default::default() })?;
        tlist.lappend(mcx, Node::mk(mcx, TargetEntry { expr: v, resno, resname: Some(names[i]), ressortgroupref: resno as u32, resorigtbl: 0, resorigcol: 0, resjunk: false })?)?;
        let Some(sgc) = sort_group(mcx, c.1, resno as u32)? else { return Ok(None) };
        distinct.lappend(mcx, sgc)?;
    }
    let q = Query {
        commandType: CmdType::CMD_SELECT,
        canSetTag: true,
        rtable,
        jointree: Some(mcx::alloc_leak_in(mcx, FromExpr { fromlist: NodeList::make1(mcx, Node::mk_range_tbl_ref(mcx, 1)?)?, quals: None })?),
        targetList: tlist,
        distinctClause: distinct,
        ..Query::default()
    };
    Ok(Some((q, (cols[0].1, cols[0].2, cols[0].3))))
}

fn level_below_safe(q: &Query<'_>) -> PgResult<bool> {
    if let Some(jt) = q.jointree {
        for i in jt.fromlist.iter() {
            if !jointree_safe(i)? {
                return Ok(false);
            }
        }
        if let Some(w) = jt.quals {
            if !expr_safe(w)? {
                return Ok(false);
            }
        }
    }
    for n in q.targetList.iter() {
        let te = n.as_target_entry().expect("tlist cell");
        if te.ressortgroupref != 0 && !expr_safe(te.expr)? {
            return Ok(false);
        }
    }
    for l in [q.limitCount, q.limitOffset].into_iter().flatten() {
        if !expr_safe(l)? {
            return Ok(false);
        }
    }
    for n in q.rtable.iter() {
        let rte = n.as_range_tbl_entry().expect("rtable cell");
        let ok = match rte.rtekind {
            RTEKind::RTE_RELATION => rte.tablesample.is_none() && rte.securityQuals.is_nil(),
            RTEKind::RTE_RESULT | RTEKind::RTE_JOIN | RTEKind::RTE_GROUP | RTEKind::RTE_CTE => true,
            RTEKind::RTE_FUNCTION => {
                let mut ok = true;
                for f in rte.functions.iter() {
                    let rtf = f.as_range_tbl_function().expect("a RangeTblFunction");
                    ok = ok && rtf.funcexpr.map_or(Ok(true), expr_safe)?;
                }
                ok
            }
            RTEKind::RTE_VALUES => {
                let mut ok = true;
                for l in rte.values_lists.iter() {
                    ok = ok && expr_safe(l)?;
                }
                ok
            }
            RTEKind::RTE_SUBQUERY => match rte.subquery {
                Some(s) => query_all_safe(s)?,
                None => false,
            },
            _ => false,
        };
        if !ok {
            return Ok(false);
        }
    }
    for n in q.cteList.iter() {
        if let Some(body) = n.as_common_table_expr().and_then(|c| c.ctequery).and_then(|b| b.as_query()) {
            if !query_all_safe(body)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn outer_ctes_safe<'mcx>(sub: &Query<'mcx>, chain: &[&Query<'mcx>]) -> PgResult<bool> {
    struct W<'c, 'mcx> {
        depth: u32,
        chain: &'c [&'c Query<'mcx>],
        ok: bool,
    }
    impl<'mcx> nodes_core::NodeWalker<'mcx> for W<'_, 'mcx> {
        fn visit(&mut self, n: Node<'mcx>) -> PgResult<bool> {
            if let Some(rte) = n.as_range_tbl_entry() {
                if rte.rtekind != RTEKind::RTE_CTE || rte.ctelevelsup <= self.depth {
                    return Ok(false);
                }
                let above = (rte.ctelevelsup - self.depth) as usize;
                let owner = self.chain.len().checked_sub(above).map(|i| self.chain[i]);
                let cte = owner.and_then(|o| o.cteList.iter().filter_map(|c| c.as_common_table_expr()).find(|c| c.ctename == rte.ctename));
                let ok = match cte.and_then(|c| c.ctequery) {
                    Some(body) => body.as_query().map_or(Ok(false), query_all_safe)? && !clauses::contain_volatile_functions(body)?,
                    None => false,
                };
                self.ok = ok;
                return Ok(!ok);
            }
            if let Some(q) = n.as_query() {
                return self.visit_query_ref(q);
            }
            nodes_core::expression_tree_walker(n, self)
        }
        fn visit_query_ref(&mut self, q: &'mcx Query<'mcx>) -> PgResult<bool> {
            self.depth += 1;
            let r = nodes_core::query_tree_walker(q, self, nodes_core::QTW_EXAMINE_RTES_BEFORE)?;
            self.depth -= 1;
            Ok(r)
        }
    }
    let mut w = W { depth: 0, chain, ok: true };
    nodes_core::query_tree_walker(sub, &mut w, nodes_core::QTW_EXAMINE_RTES_BEFORE)?;
    Ok(w.ok)
}

fn query_all_safe(q: &Query<'_>) -> PgResult<bool> {
    if !level_below_safe(q)? {
        return Ok(false);
    }
    for n in q.targetList.iter() {
        if !expr_safe(n.as_target_entry().expect("tlist cell").expr)? {
            return Ok(false);
        }
    }
    q.havingQual.map_or(Ok(true), expr_safe)
}

fn jointree_safe(n: Node<'_>) -> PgResult<bool> {
    if n.as_range_tbl_ref().is_some() {
        return Ok(true);
    }
    if let Some(j) = n.as_join_expr() {
        return Ok(j.quals.map_or(Ok(true), expr_safe)? && jointree_safe(j.larg)? && jointree_safe(j.rarg)?);
    }
    if let Some(f) = n.as_from_expr() {
        for i in f.fromlist.iter() {
            if !jointree_safe(i)? {
                return Ok(false);
            }
        }
        return f.quals.map_or(Ok(true), expr_safe);
    }
    Ok(false)
}

fn agg_inputs_safe(expr: Node<'_>, args: bool) -> PgResult<bool> {
    struct W {
        args: bool,
        ok: bool,
    }
    impl<'m> nodes_core::NodeWalker<'m> for W {
        fn visit(&mut self, n: Node<'m>) -> PgResult<bool> {
            if let Some(agg) = n.as_aggref() {
                let mut ok = agg.aggfilter.map_or(Ok(true), expr_safe)?;
                if self.args {
                    for a in agg.args.iter() {
                        ok = ok && expr_safe(a)?;
                    }
                }
                if !ok {
                    self.ok = false;
                    return Ok(true);
                }
                return Ok(false);
            }
            nodes_core::expression_tree_walker(n, self)
        }
    }
    let mut w = W { args, ok: true };
    nodes_core::NodeWalker::visit(&mut w, expr)?;
    Ok(w.ok)
}

fn outer_in_aggs(expr: Node<'_>) -> PgResult<bool> {
    struct W;
    impl<'m> nodes_core::NodeWalker<'m> for W {
        fn visit(&mut self, n: Node<'m>) -> PgResult<bool> {
            if n.node_tag() == NodeTag::T_Aggref {
                return vars::contain_vars_of_level(n, 1);
            }
            nodes_core::expression_tree_walker(n, self)
        }
    }
    nodes_core::NodeWalker::visit(&mut W, expr)
}

/// decorrelate.rs's eager-safety, with aggregates and window functions taken apart and a SubLink's subselect required
/// wholly safe.
fn expr_safe(node: Node<'_>) -> PgResult<bool> {
    let mut s = Safe { ok: true, consts: reach() >= 1 };
    nodes_core::NodeWalker::visit(&mut s, node)?;
    Ok(s.ok)
}

struct Safe {
    ok: bool,
    consts: bool,
}

/// P2 (D3221): a call tree whose leaves are Consts under immutable calls that return no set and no record
/// (evaluate_function's conditions) is folded by eval_const_expressions while PostgreSQL plans the subselect as written,
/// which the planner does before this form: it raises there or is never evaluated per row. A fold that raises only in
/// this form (a SubLink PostgreSQL never plans) is a data exception the planner drops the form for (lib.rs).
fn plan_time_const(n: Node<'_>) -> PgResult<bool> {
    if n.as_const().is_some() {
        return Ok(true);
    }
    if let Some(r) = n.as_relabel_type() {
        return plan_time_const(r.arg);
    }
    let (func, args) = match (n.as_func_expr(), n.as_op_expr()) {
        (Some(f), _) if !f.funcretset && f.funcresulttype != types_core::catalog::RECORDOID => (f.funcid, &f.args),
        (None, Some(o)) if !o.opretset && o.opresulttype != types_core::catalog::RECORDOID => {
            (if o.opfuncid != 0 { o.opfuncid } else { lsyscache::get_opcode(o.opno)? }, &o.args)
        }
        _ => return Ok(false),
    };
    if lsyscache::func_volatile(func)? != clauses::classify::PROVOLATILE_IMMUTABLE {
        return Ok(false);
    }
    for a in args.iter() {
        if !plan_time_const(a)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether a subselect's admission needs P2: a plan-time constant call tree (`plan_time_const`) that the plain eager-safety
/// check refuses (a date + interval) in its WHERE / JOIN quals or anywhere in a query below them. Only
/// such a SubLink gets the restricted form at reach p2 (the user's 2026-10-08 ruling: P2 only with P2D).
fn relies_on_p2(sub: &Query<'_>) -> PgResult<bool> {
    struct W;
    impl<'m> nodes_core::NodeWalker<'m> for W {
        fn visit(&mut self, n: Node<'m>) -> PgResult<bool> {
            if matches!(n.node_tag(), NodeTag::T_FuncExpr | NodeTag::T_OpExpr) && plan_time_const(n)? {
                let mut plain = Safe { ok: true, consts: false };
                nodes_core::NodeWalker::visit(&mut plain, n)?;
                return Ok(!plain.ok);
            }
            if let Some(q) = n.as_query() {
                return nodes_core::query_tree_walker(q, self, 0);
            }
            nodes_core::expression_tree_walker(n, self)
        }
        fn visit_query_ref(&mut self, q: &'m Query<'m>) -> PgResult<bool> {
            nodes_core::query_tree_walker(q, self, 0)
        }
    }
    let mut w = W;
    if let Some(jt) = sub.jointree {
        for i in jt.fromlist.iter().chain(jt.quals) {
            if nodes_core::NodeWalker::visit(&mut w, i)? {
                return Ok(true);
            }
        }
    }
    for n in sub.rtable.iter() {
        if let Some(q) = n.as_range_tbl_entry().and_then(|r| r.subquery) {
            if nodes_core::query_tree_walker(q, &mut w, 0)? {
                return Ok(true);
            }
        }
    }
    for n in sub.cteList.iter() {
        if let Some(body) = n.as_common_table_expr().and_then(|c| c.ctequery) {
            if nodes_core::NodeWalker::visit(&mut w, body)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

impl<'mcx> nodes_core::NodeWalker<'mcx> for Safe {
    fn visit(&mut self, n: Node<'mcx>) -> PgResult<bool> {
        if self.consts && matches!(n.node_tag(), NodeTag::T_FuncExpr | NodeTag::T_OpExpr) && plan_time_const(n)? {
            return Ok(false);
        }
        match n.node_tag() {
            NodeTag::T_Var
            | NodeTag::T_Const
            | NodeTag::T_Param
            | NodeTag::T_List
            | NodeTag::T_RelabelType
            | NodeTag::T_BoolExpr
            | NodeTag::T_NullTest
            | NodeTag::T_BooleanTest
            | NodeTag::T_CaseExpr
            | NodeTag::T_CaseWhen
            | NodeTag::T_CaseTestExpr
            | NodeTag::T_CoalesceExpr
            | NodeTag::T_RowExpr
            | NodeTag::T_FieldSelect
            | NodeTag::T_TargetEntry
            | NodeTag::T_SortGroupClause
            | NodeTag::T_Aggref
            | NodeTag::T_GroupingFunc
            | NodeTag::T_WindowFunc => {}
            NodeTag::T_ArrayExpr => {
                if n.as_array_expr().expect("an ArrayExpr").multidims {
                    self.ok = false;
                    return Ok(true);
                }
            }
            NodeTag::T_FuncExpr | NodeTag::T_OpExpr | NodeTag::T_DistinctExpr | NodeTag::T_NullIfExpr | NodeTag::T_ScalarArrayOpExpr => {
                let leaky = |f| Ok(!(lsyscache::func_strict(f)? && (lsyscache::get_func_leakproof(f)? || cannot_raise(f, n))));
                if nodes_core::check_functions_in_node(n, &mut { leaky })? || !crate::createplan::collation_determinate(n)? {
                    self.ok = false;
                    return Ok(true);
                }
            }
            NodeTag::T_SubLink => {
                let sl = n.as_sub_link().expect("a SubLink");
                if let Some(t) = sl.testexpr {
                    if self.visit(t)? {
                        return Ok(true);
                    }
                }
                if !query_all_safe(sl.subselect.as_query().expect("a transformed SubLink holds a Query"))? {
                    self.ok = false;
                    return Ok(true);
                }
                return Ok(false);
            }
            _ => {
                self.ok = false;
                return Ok(true);
            }
        }
        nodes_core::expression_tree_walker(n, self)
    }
}

/// date_{lt,le,eq,gt,ge,ne}_timestamp and timestamp_{lt,le,eq,gt,ge,ne}_date: date_cmp_timestamp_internal (PostgreSQL 18.6
/// date.c:808) converts with an overflow flag and orders an overflowed date just below the end of time; its error
/// branch (date.c:656) runs only without the flag. Date against timestamptz is not listed (the session time zone).
const DATE_TIMESTAMP_CMP: [Oid; 12] = [2338, 2339, 2340, 2341, 2342, 2343, 2364, 2365, 2366, 2367, 2368, 2369];

const LIKE: [Oid; 4] = [850, 851, 1631, 1632];

/// A call of `f` that raises on no input though the catalog does not mark it leakproof (read by this walker only): a
/// numeric comparison (types_core::quiet::numeric_cmp), a date against timestamp comparison, or LIKE / NOT LIKE whose
/// pattern is a constant that does not end in an unpaired escape (types_core::quiet::pattern_quiet: MatchText's one
/// data error) with at most 16 '%' (MatchText recurses once per '%' and checks the stack depth each time).
fn cannot_raise(f: Oid, call: Node<'_>) -> bool {
    if types_core::quiet::numeric_cmp(f) || DATE_TIMESTAMP_CMP.contains(&f) {
        return true;
    }
    if !LIKE.contains(&f) {
        return false;
    }
    let (args, collid) = match (call.as_op_expr(), call.as_func_expr()) {
        (Some(o), _) => (&o.args, o.inputcollid),
        (None, Some(fe)) => (&fe.args, fe.inputcollid),
        _ => return false,
    };
    let Some(c) = args.iter().nth(1).and_then(|a| a.as_const()) else { return false };
    if c.constisnull {
        return true;
    }
    if c.constbyval || c.constlen != -1 {
        return false;
    }
    // SAFETY: a by-reference varlena Const's first header byte; only a plain inline image (short or 4-byte header) is read
    // (createplan.rs quiet_qual reads patterns the same way).
    let b0 = unsafe { *(c.constvalue.as_usize() as *const u8) };
    if b0 == 0x01 || (b0 & 0x01 == 0 && b0 & 0x03 != 0) {
        return false;
    }
    let pattern = crate::selfuncs::varlena_datum_payload(c.constvalue);
    types_core::quiet::pattern_quiet(f, pattern, collid) == Some(true) && pattern.iter().filter(|&&b| b == b'%').count() <= 16
}
