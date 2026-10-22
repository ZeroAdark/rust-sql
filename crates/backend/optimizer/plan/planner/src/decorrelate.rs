//! Gravity decorrelation (a correlated scalar aggregate as a grouped join): under the Gravity
//! posture a correlated scalar aggregate SubLink
//!
//! ```sql
//! ... (SELECT f(agg(...)) FROM inner WHERE inner.k = outer.k AND ...) ...
//! ```
//!
//! is also planned as the join form: the outer's FROM gains
//! `LEFT JOIN (SELECT k, agg(...) FROM inner WHERE ... GROUP BY k) g ON g.k = outer.k`
//! and the SubLink becomes `f(g.agg)` (`coalesce(g.agg, 0)` for `count`). The transform is
//! a costed alternative, never forced: `standard_planner` plans the statement both ways and
//! keeps the cheaper plan ([`alternative`] builds the second Query).
//! The decomposer refuses the SubPlan form when the inner declares a breaker (decompose.md
//! §3 decision 26: "rebuilt per outer row"); the join form is what it serves.
//!
//! Equivalence: without GROUP BY the sub-select yields exactly one row per outer row; its
//! aggregates over an empty input are NULL for every admitted aggregate but `count`, which
//! is 0 — the LEFT JOIN's null-extended row reproduces both. A correlated conjunct
//! `inner_expr = outer_expr` is admitted when `=` is the btree equality of the inner
//! expression's default opfamily (the GROUP BY groups by that same equality) and the
//! collations agree; an outer NULL matches nothing on either side. A SELECT level is
//! rewritten grouped or not (the join is one-to-one), but a key naming the level's GROUP RTE —
//! a target-list SubLink's reference to a grouped column — is refused: the ON clause sits below
//! the grouping that defines that Var (the planner flattens GROUP Vars in the target list and
//! HAVING only). A WHERE refused as written is read again with a conjunct common to every OR arm
//! factored out ([`or_factored`]), the form PostgreSQL's planner evaluates.
//!
//! Laziness (Michael, 2026-09-17: the rewrite matches PostgreSQL exactly, errors included).
//! PostgreSQL evaluates the SubLink only where the expression holding it reaches it, and the
//! inner side only for the rows the correlation admits; the join form evaluates the outer key
//! for every outer row and the inner side over every inner row. So a SubLink is admitted only
//! where it is evaluated unconditionally — a target entry's top expression, a WHERE conjunct
//! or a direct operand of a conjunct's operator — never under CASE, the arms of AND / OR /
//! COALESCE, an aggregate's FILTER, HAVING or an ON clause; and every expression the join form
//! evaluates where the original would not — the correlation conjuncts, the kept conjuncts,
//! the aggregates' arguments and FILTERs, the inner's range-table expressions — is Vars, Consts
//! and pure control flow over functions the catalog marks leakproof (they raise no error) and
//! strict ([`eager_safe`], [`rtable_eager_safe`]). An aggregate's own transition function is not
//! held to this (none is marked leakproof; `sum(float8)` can overflow on an unmatched group —
//! a residual this rewrite accepts).
//! The wrapper `f(agg)` is evaluated where the SubLink was and is not held to that; lifted there, its references to
//! levels 2 and up shift by one.
//! Nothing but the correlation conjuncts may reach the outer level: after they are removed the
//! grouped Query holds no level-1 Var, PlaceHolderVar or aggregate anywhere — a function or
//! subquery RTE included, where the parser marks `lateral` only for level-0 references.
//! Deeper references (level 2 and up) are legal: the grouped Query sits in the outer's range
//! table at the SubLink's own depth, so they name the same level as before.
//!
//! CTE bodies: the alternative is built from the pristine parse, before
//! `subquery_planner` inlines CTEs, so a level's `cteList` bodies are
//! walked here too ([`cte_body`], [`cte_walked`]): such a body is a level like any other and is
//! rewritten under the same admission (laziness, [`eager_safe`], volatile and collation guards).
//! The CTE node keeps its materialization, reference count and name, so `ss_process_ctes` makes
//! the same inline / InitPlan choice as for the original body, and an InitPlan body still runs
//! once. Which bodies are walked is [`CteWalk`]: by default (`materialized`) every plain body, the
//! inlined ones, a body read more than once (an InitPlan by PG's default) and a MATERIALIZED one;
//! `all` leaves MATERIALIZED bodies as written, `on` also bodies read more than once. A
//! volatile, data-modifying or recursive body, and a MERGE level's cteList, are never walked.
//! `pgrust.gravity_decorrelate_ctes = off` keeps every CTE body as written.

use mcx::Mcx;
use types_core::Oid;
use types_error::PgResult;
use types_nodes::list::{IntList, NodeList};
use types_nodes::nodes_enums::CmdType;
use types_nodes::parsenodes::{CommonTableExpr, Query, RTEKind, RangeTblEntry, SortGroupClause};
use types_nodes::primnodes::{
    Aggref, Alias, CoalesceExpr, Const, FromExpr, JoinExpr, OpExpr, SubLink, SubLinkType, TargetEntry, Var,
    AGGKIND_NORMAL,
};
use types_nodes::{Bitmapset, JoinType, Node, NodeTag};

const INT8OID: Oid = 20;
pub(crate) const MAX_KEYS: usize = 8;
const MAX_AGGS: usize = 8;
pub(crate) const KEY_NAMES: [&str; MAX_KEYS] = ["k1", "k2", "k3", "k4", "k5", "k6", "k7", "k8"];
const AGG_NAMES: [&str; MAX_AGGS] = ["a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8"];
const SUBQUERY_ALIAS: &str = "gravity_decorrelated";
const JOIN_ALIAS: &str = "unnamed_join";
const EMPTY_NULL_AGGS: [&str; 7] = ["sum", "avg", "min", "max", "bool_and", "bool_or", "every"];

pub(crate) fn alternative<'mcx>(mcx: Mcx<'mcx>, parse: &Query<'mcx>) -> PgResult<Option<&'mcx mut Query<'mcx>>> {
    if !any_sublinks(parse) {
        return Ok(None);
    }
    let Some(q) = transform_level(mcx, parse)? else { return Ok(None) };
    let copy = copyfuncs::copy_query(mcx, &q)?;
    Ok(Some(mcx::leak_in(mcx::alloc_in(mcx, copy)?)))
}

fn any_sublinks(q: &Query<'_>) -> bool {
    if q.hasSubLinks {
        return true;
    }
    q.rtable.iter().any(|n| {
        n.as_range_tbl_entry().and_then(|r| r.subquery).is_some_and(any_sublinks)
    }) || (!q.cteList.is_nil() && cte_walk() != CteWalk::Off && q.cteList.iter().any(|n| cte_body(n).is_some_and(any_sublinks)))
}

fn cte_body<'mcx>(n: Node<'mcx>) -> Option<&'mcx Query<'mcx>> {
    let cte = n.as_common_table_expr().expect("cteList cell");
    cte.ctequery.and_then(|q| q.as_query()).filter(|q| !cte.cterecursive && q.commandType == CmdType::CMD_SELECT)
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CteWalk {
    Off,
    Inlined,
    /// `all`: also a body read more than once that is not MATERIALIZED (planned once as an InitPlan by PG's own default).
    Shared,
    /// `materialized`: also a MATERIALIZED body. The keyword is PG's user-visible optimization fence, so this is a
    /// PG-visible divergence; the default.
    Materialized,
}

fn cte_walk() -> CteWalk {
    const DEFAULT: CteWalk = CteWalk::Materialized;
    if !guc_seams::knob_read::is_installed() {
        return DEFAULT;
    }
    // the knob's words, trimmed and without case: off false 0 | on true 1 | all | materialized
    match guc_seams::knob_read::call(guc_seams::Knob::GravityDecorrelateCtes) {
        guc_seams::KnobRead::Word(0..=2) => CteWalk::Off,
        guc_seams::KnobRead::Word(3..=5) => CteWalk::Inlined,
        guc_seams::KnobRead::Word(6) => CteWalk::Shared,
        guc_seams::KnobRead::Word(7) => CteWalk::Materialized,
        _ => DEFAULT,
    }
}

/// The subselect's WHERE with a conjunct common to every OR arm factored out (`(c AND a) OR (c AND b)` -> `c AND (a OR
/// b)`): PostgreSQL's own canonicalize_qual, which preprocess_expression applies to that WHERE before the SubPlan's first
/// row, so the factored form is the one PostgreSQL evaluates. None when it changes nothing or
/// `pgrust.gravity_decorrelate_or_factor` is off; callers try the WHERE as written first.
pub(crate) fn or_factored<'mcx>(mcx: Mcx<'mcx>, quals: Option<Node<'mcx>>) -> PgResult<Option<Node<'mcx>>> {
    let Some(q) = quals.filter(|q| q.node_tag() != NodeTag::T_List) else { return Ok(None) };
    if guc_seams::knob_read::is_installed()
        && matches!(guc_seams::knob_read::call(guc_seams::Knob::GravityDecorrelateOrFactor), guc_seams::KnobRead::Word(0..=3))
    {
        return Ok(None);
    }
    let f = crate::prepqual::canonicalize_qual(mcx, q, false)?;
    Ok((!types_nodes::equal(f, q)).then_some(f))
}

/// whether the walk at `mode` rewrites this CTE's body. A body with volatile or data-modifying content, a recursive
/// body and an unreferenced one never are, so a body's side effects run exactly as often as before.
fn cte_walked(cte: &CommonTableExpr<'_>, mode: CteWalk) -> PgResult<bool> {
    use types_nodes::parsenodes::CTEMaterialize;
    if mode == CteWalk::Off || cte.cterefcount < 1 {
        return Ok(false);
    }
    if crate::cte::cte_inlines(cte)? {
        return Ok(true);
    }
    let needs = if cte.ctematerialized == CTEMaterialize::CTEMaterializeAlways { CteWalk::Materialized } else { CteWalk::Shared };
    Ok(mode >= needs && crate::cte::cte_plain_select(cte)?)
}

fn transform_ctes<'mcx>(mcx: Mcx<'mcx>, q: &Query<'mcx>) -> PgResult<Option<NodeList<'mcx>>> {
    let mode = cte_walk();
    if q.commandType == CmdType::CMD_MERGE || q.cteList.is_nil() || mode == CteWalk::Off {
        return Ok(None);
    }
    let mut out = NodeList::with_capacity(mcx, q.cteList.len())?;
    let mut changed = false;
    for n in q.cteList.iter() {
        let new = match cte_body(n) {
            Some(body) if cte_walked(n.as_common_table_expr().expect("cteList cell"), mode)? => {
                transform_level(mcx, body)?
            }
            _ => None,
        };
        match new {
            Some(new) => {
                let cte = n.as_common_table_expr().expect("cteList cell");
                out.lappend(
                    mcx,
                    Node::mk(
                        mcx,
                        CommonTableExpr {
                            ctename: cte.ctename,
                            aliascolnames: cte.aliascolnames.clone_in(mcx)?,
                            ctematerialized: cte.ctematerialized,
                            ctequery: Some(Node::mk(mcx, new)?),
                            search_clause: cte.search_clause,
                            cycle_clause: cte.cycle_clause,
                            location: cte.location,
                            cterecursive: cte.cterecursive,
                            cterefcount: cte.cterefcount,
                            ctecolnames: cte.ctecolnames.clone_in(mcx)?,
                            ctecoltypes: cte.ctecoltypes.clone_in(mcx)?,
                            ctecoltypmods: cte.ctecoltypmods.clone_in(mcx)?,
                            ctecolcollations: cte.ctecolcollations.clone_in(mcx)?,
                        },
                    )?,
                )?;
                changed = true;
            }
            None => out.lappend(mcx, n)?,
        }
    }
    Ok(changed.then_some(out))
}

fn transform_level<'mcx>(mcx: Mcx<'mcx>, q: &Query<'mcx>) -> PgResult<Option<Query<'mcx>>> {
    let mut changed = false;
    let mut rtable = NodeList::with_capacity(mcx, q.rtable.len() + 2)?;
    let descend = q.commandType != CmdType::CMD_MERGE;
    for n in q.rtable.iter() {
        let rte = n.as_range_tbl_entry().expect("rtable cell");
        match rte.subquery {
            Some(sub) if descend && rte.rtekind == RTEKind::RTE_SUBQUERY => match transform_level(mcx, sub)? {
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
    let ctes = transform_ctes(mcx, q)?;
    let Some(jt) = q.jointree else { return Ok(None) };
    let group_rti = q.rtable.iter().position(|n| n.as_range_tbl_entry().expect("rtable cell").rtekind == RTEKind::RTE_GROUP).map_or(0, |i| i as i32 + 1);
    let mut ctx = Level { mcx, rtable, joins: Vec::new(), outer_ok: outer_admits(q), group_rti };
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
    let having = match q.havingQual {
        Some(n) => ctx.mutate(n, false)?,
        None => None,
    };
    if !(changed || ctes.is_some() || quals.is_some() || tlist_changed || having.is_some() || !ctx.joins.is_empty()) {
        return Ok(None);
    }
    let mut out = crate::subselect::query_cells_copy(mcx, q)?;
    out.rtable = ctx.rtable;
    if let Some(ctes) = ctes {
        out.cteList = ctes;
    }
    if tlist_changed {
        out.targetList = tlist;
    }
    if having.is_some() {
        out.havingQual = having;
    }
    let quals = quals.or(jt.quals);
    let fromlist = if ctx.joins.is_empty() {
        jt.fromlist.clone_in(mcx)?
    } else {
        let mut cur = fold_fromlist(mcx, &mut out.rtable, &jt.fromlist)?;
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

/// A grouped outer level is admitted: the join is one-to-one, so WHERE keeps its
/// pre-grouping meaning and a lifted column in the target list or HAVING is functionally
/// dependent on the grouping keys the parser required of the SubLink's outer references.
pub(crate) fn outer_admits(q: &Query<'_>) -> bool {
    q.commandType == CmdType::CMD_SELECT
        && q.hasSubLinks
        && !q.hasTargetSRFs
        && q.groupingSets.is_nil()
        && q.setOperations.is_none()
        && q.rowMarks.is_nil()
        && q.jointree.is_some_and(|jt| !jt.fromlist.is_nil())
}

struct PendingJoin<'mcx> {
    sub_rti: i32,
    join_rti: i32,
    on: Node<'mcx>,
}

struct Level<'mcx> {
    mcx: Mcx<'mcx>,
    rtable: NodeList<'mcx>,
    joins: Vec<PendingJoin<'mcx>>,
    outer_ok: bool,
    group_rti: i32,
}

impl<'mcx> Level<'mcx> {
    fn mutate(&mut self, node: Node<'mcx>, here: bool) -> PgResult<Option<Node<'mcx>>> {
        let mcx = self.mcx;
        if let Some(sl) = node.as_sub_link() {
            if here && self.outer_ok && self.joins.len() < MAX_KEYS {
                if let Some(replacement) = self.decorrelate(sl)? {
                    return Ok(Some(replacement));
                }
            }
            return self.sublink_below(sl);
        }
        nodes_core::expression_tree_mutator(mcx, node, &mut |n| self.mutate(n, false))
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
        let Some(new) = transform_level(mcx, sub)? else { return Ok(None) };
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

    fn decorrelate(&mut self, sl: &SubLink<'mcx>) -> PgResult<Option<Node<'mcx>>> {
        let mcx = self.mcx;
        if sl.subLinkType != SubLinkType::EXPR_SUBLINK || sl.testexpr.is_some() {
            return Ok(None);
        }
        let sub = sl.subselect.as_query().expect("a transformed SubLink holds a Query");
        if !sub_admits(sub) || !rtable_eager_safe(sub)? {
            return Ok(None);
        }
        let jt = sub.jointree.expect("admitted: a jointree");
        if let Some(r) = self.decorrelate_where(sub, jt, jt.quals)? {
            return Ok(Some(r));
        }
        match or_factored(mcx, jt.quals)? {
            Some(q) => self.decorrelate_where(sub, jt, Some(q)),
            None => Ok(None),
        }
    }

    fn decorrelate_where(&mut self, sub: &'mcx Query<'mcx>, jt: &'mcx FromExpr<'mcx>, quals: Option<Node<'mcx>>) -> PgResult<Option<Node<'mcx>>> {
        let mcx = self.mcx;
        let mut keys: Vec<(Node<'mcx>, Node<'mcx>, &'mcx OpExpr<'mcx>, bool)> = Vec::new();
        let mut kept = NodeList::nil();
        for c in clauses::make_ands_implicit(mcx, quals)?.iter() {
            if clauses::contain_volatile_functions(c)? || !eager_safe(c)? {
                return Ok(None);
            }
            if !vars::contain_vars_of_level(c, 1)? {
                kept.lappend(mcx, c)?;
                continue;
            }
            let Some(key) = correlation_key(c)? else { return Ok(None) };
            if self.group_rti != 0 && names_rte(key.1, self.group_rti, 1)? {
                return Ok(None);
            }
            keys.push(key);
        }
        if keys.is_empty() || keys.len() > MAX_KEYS {
            return Ok(None);
        }
        let te = sub.targetList.first().and_then(|n| n.as_target_entry()).expect("one target entry");
        if vars::contain_vars_of_level(te.expr, 1)? || clauses::contain_volatile_functions(te.expr)? {
            return Ok(None);
        }
        let mut aggs = AggCollector { mcx, count: 0, admitted: true };
        aggs.walk(te.expr)?;
        if !aggs.admitted || aggs.count == 0 || aggs.count > MAX_AGGS {
            return Ok(None);
        }

        let sub_rti = self.rtable.len() as i32 + 1;
        let join_rti = sub_rti + 1;
        let mut nulling = Bitmapset::empty();
        nulling.add_member(mcx, join_rti)?;

        let mut g = crate::subselect::query_cells_copy(mcx, sub)?;
        let gquals = if kept.is_nil() { None } else { Some(clauses::make_ands_explicit(mcx, &kept)?) };
        g.jointree = Some(mcx::alloc_leak_in(mcx, FromExpr { fromlist: jt.fromlist.clone_in(mcx)?, quals: gquals })?);
        let mut gtlist = NodeList::nil();
        let mut group = NodeList::nil();
        let mut colnames = NodeList::nil();
        let mut on = NodeList::nil();
        for (i, (inner, outer, op, inner_left)) in keys.iter().enumerate() {
            let (inner, outer) = (*inner, *outer);
            let (ity, itm) = ::costsize::expr_type_typmod(inner);
            let icoll = crate::pathkeys::expr_collation(inner);
            let tc = typcache::lookup_type_cache(ity, typcache::TYPECACHE_EQ_OPR | typcache::TYPECACHE_LT_OPR | typcache::TYPECACHE_HASH_PROC)?;
            let resno = (i + 1) as i16;
            gtlist.lappend(
                mcx,
                Node::mk(
                    mcx,
                    TargetEntry {
                        expr: inner,
                        resno,
                        resname: Some(KEY_NAMES[i]),
                        ressortgroupref: resno as u32,
                        resorigtbl: 0,
                        resorigcol: 0,
                        resjunk: false,
                    },
                )?,
            )?;
            group.lappend(
                mcx,
                Node::mk(
                    mcx,
                    SortGroupClause {
                        tleSortGroupRef: resno as u32,
                        eqop: tc.eq_opr(),
                        sortop: tc.lt_opr(),
                        reverse_sort: false,
                        nulls_first: false,
                        hashable: tc.hash_proc() != 0,
                    },
                )?,
            )?;
            colnames.lappend(mcx, Node::mk_string(mcx, KEY_NAMES[i])?)?;
            let gvar = Node::mk(
                mcx,
                Var { varno: sub_rti, varattno: resno, vartype: ity, vartypmod: itm, varcollid: icoll, varnosyn: sub_rti as u32, varattnosyn: resno, ..Default::default() },
            )?;
            let outer = rewrite_manip::copy_node(mcx, outer)?;
            rewrite_manip::IncrementVarSublevelsUp(outer, -1, 1)?;
            let args = if *inner_left { NodeList::make2(mcx, gvar, outer)? } else { NodeList::make2(mcx, outer, gvar)? };
            on.lappend(mcx, Node::mk(mcx, OpExpr { args, ..**op })?)?;
        }
        let mut lift = AggLift { mcx, sub_rti, nkeys: keys.len(), next: 0, tlist: NodeList::nil(), colnames: NodeList::nil(), nulling };
        // The wrapper moves up one level to the SubLink's position: its references to levels 2 and up shift by one (a
        // copy: the shift is in place and the original SubLink is still planned).
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
        let gnode = Node::mk(mcx, g)?;
        if vars::contain_vars_of_level(gnode, 1)? || rewrite_manip::contain_aggs_of_level(gnode, 1)? {
            return Ok(None);
        }
        let g = gnode.as_query().expect("a Query node");

        let alias = mcx::alloc_leak_in(mcx, Alias { aliasname: Some(SUBQUERY_ALIAS), colnames: NodeList::nil() })?;
        let eref = mcx::alloc_leak_in(mcx, Alias { aliasname: Some(SUBQUERY_ALIAS), colnames })?;
        self.rtable.lappend(
            mcx,
            Node::mk(
                mcx,
                RangeTblEntry {
                    rtekind: RTEKind::RTE_SUBQUERY,
                    subquery: Some(g),
                    alias: Some(alias),
                    eref: Some(eref),
                    lateral: false,
                    inFromCl: true,
                    ..Default::default()
                },
            )?,
        )?;
        self.rtable.lappend(mcx, join_rte(mcx, JoinType::JOIN_LEFT)?)?;
        debug_assert_eq!(self.rtable.len() as i32, join_rti);
        self.joins.push(PendingJoin { sub_rti, join_rti, on: clauses::make_ands_explicit(mcx, &on)? });
        Ok(Some(replacement))
    }
}

pub(crate) fn join_rte<'mcx>(mcx: Mcx<'mcx>, jointype: JoinType) -> PgResult<Node<'mcx>> {
    let eref = mcx::alloc_leak_in(mcx, Alias { aliasname: Some(JOIN_ALIAS), colnames: NodeList::nil() })?;
    Node::mk(
        mcx,
        RangeTblEntry {
            rtekind: RTEKind::RTE_JOIN,
            jointype,
            joinmergedcols: 0,
            joinaliasvars: NodeList::nil(),
            joinleftcols: IntList::nil(),
            joinrightcols: IntList::nil(),
            join_using_alias: None,
            alias: None,
            eref: Some(eref),
            lateral: false,
            inFromCl: true,
            ..Default::default()
        },
    )
}

pub(crate) fn fold_fromlist<'mcx>(mcx: Mcx<'mcx>, rtable: &mut NodeList<'mcx>, fromlist: &NodeList<'mcx>) -> PgResult<Node<'mcx>> {
    let mut cur = fromlist.nth(0);
    for item in fromlist.iter().skip(1) {
        rtable.lappend(mcx, join_rte(mcx, JoinType::JOIN_INNER)?)?;
        cur = Node::mk(
            mcx,
            JoinExpr {
                jointype: JoinType::JOIN_INNER,
                isNatural: false,
                larg: cur,
                rarg: item,
                usingClause: NodeList::nil(),
                join_using_alias: None,
                quals: None,
                alias: None,
                rtindex: rtable.len() as i32,
            },
        )?;
    }
    Ok(cur)
}

fn sub_admits(sub: &Query<'_>) -> bool {
    sub.commandType == CmdType::CMD_SELECT
        && sub.hasAggs
        && !sub.hasWindowFuncs
        && !sub.hasTargetSRFs
        && !sub.hasSubLinks
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

/// The inner's range table, which the join form evaluates over every inner row before the
/// correlation applies — and, when the planner orders the LEFT JOIN below the outer's other
/// quals, for outer rows PostgreSQL never reached: a relation or result entry evaluates
/// nothing; a function, VALUES or subquery entry is admitted when every expression it holds is
/// [`eager_safe`] (a subquery recursively: its range table, target list, quals and limits, with
/// no aggregate, window, SRF, SubLink, set operation or CTE); a CTE reference (run at first use
/// in the original), a table function, a named tuplestore and a join entry refuse.
fn rtable_eager_safe(q: &Query<'_>) -> PgResult<bool> {
    for n in q.rtable.iter() {
        let rte = n.as_range_tbl_entry().expect("rtable cell");
        let ok = match rte.rtekind {
            RTEKind::RTE_RELATION | RTEKind::RTE_RESULT => true,
            RTEKind::RTE_FUNCTION => {
                let mut ok = true;
                for f in rte.functions.iter() {
                    let rtf = f.as_range_tbl_function().expect("a RangeTblFunction");
                    ok = ok && rtf.funcexpr.map_or(Ok(true), eager_safe)?;
                }
                ok
            }
            RTEKind::RTE_VALUES => {
                let mut ok = true;
                for l in rte.values_lists.iter() {
                    ok = ok && eager_safe(l)?;
                }
                ok
            }
            RTEKind::RTE_SUBQUERY => rte.subquery.map_or(Ok(false), nested_eager_safe)?,
            _ => false,
        };
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

fn nested_eager_safe(q: &Query<'_>) -> PgResult<bool> {
    if q.commandType != CmdType::CMD_SELECT
        || q.hasAggs
        || q.hasWindowFuncs
        || q.hasTargetSRFs
        || q.hasSubLinks
        || q.setOperations.is_some()
        || !q.cteList.is_nil()
        || !q.groupClause.is_nil()
        || q.havingQual.is_some()
        || !q.rowMarks.is_nil()
    {
        return Ok(false);
    }
    for n in q.targetList.iter() {
        if !eager_safe(n.as_target_entry().expect("tlist cell").expr)? {
            return Ok(false);
        }
    }
    for l in [q.limitOffset, q.limitCount].into_iter().flatten() {
        if !eager_safe(l)? {
            return Ok(false);
        }
    }
    let Some(jt) = q.jointree else { return Ok(false) };
    Ok(rtable_eager_safe(q)? && from_expr_eager_safe(jt)?)
}

fn from_expr_eager_safe(jt: &FromExpr<'_>) -> PgResult<bool> {
    if !jt.quals.map_or(Ok(true), eager_safe)? {
        return Ok(false);
    }
    for item in jt.fromlist.iter() {
        if !jointree_item_eager_safe(item)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn jointree_item_eager_safe(node: Node<'_>) -> PgResult<bool> {
    if node.as_range_tbl_ref().is_some() {
        return Ok(true);
    }
    if let Some(j) = node.as_join_expr() {
        return Ok(j.quals.map_or(Ok(true), eager_safe)? && jointree_item_eager_safe(j.larg)? && jointree_item_eager_safe(j.rarg)?);
    }
    match node.as_from_expr() {
        Some(f) => from_expr_eager_safe(f),
        None => Ok(false),
    }
}

/// An expression the join form evaluates for rows the original never reached: Vars, Consts,
/// Params and pure control flow over functions the catalog marks leakproof and strict, each call
/// under a determinate input collation (D2851: `createplan::collation_determinate`; texteq over two
/// columns of conflicting implicit collations is leakproof and raises 42P22 at its first row).
pub(crate) fn eager_safe(node: Node<'_>) -> PgResult<bool> {
    eager_safe_in(node, false)
}

/// D3342 (cte_or.rs, strict): [`eager_safe`] with aggregates admitted: an Aggref's arguments and FILTER are held to it,
/// its own transition is not (as above); a GroupingFunc is a value.
pub(crate) fn eager_safe_aggs(node: Node<'_>) -> PgResult<bool> {
    eager_safe_in(node, true)
}

fn eager_safe_in(node: Node<'_>, aggs: bool) -> PgResult<bool> {
    struct W(bool);
    impl<'m> nodes_core::NodeWalker<'m> for W {
        fn visit(&mut self, n: Node<'m>) -> PgResult<bool> {
            match n.node_tag() {
                NodeTag::T_Aggref if self.0 => {
                    let a = n.as_aggref().expect("an Aggref");
                    for t in a.args.iter() {
                        if !eager_safe_in(t.as_target_entry().map_or(t, |t| t.expr), true)? {
                            return Ok(true);
                        }
                    }
                    return Ok(!a.aggfilter.map_or(Ok(true), |f| eager_safe_in(f, true))?);
                }
                NodeTag::T_GroupingFunc if self.0 => return Ok(false),
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
                | NodeTag::T_CoalesceExpr => {}
                NodeTag::T_FuncExpr
                | NodeTag::T_OpExpr
                | NodeTag::T_DistinctExpr
                | NodeTag::T_NullIfExpr
                | NodeTag::T_ScalarArrayOpExpr => {
                    let leaky = |f| Ok(!(lsyscache::get_func_leakproof(f)? && lsyscache::func_strict(f)?));
                    if nodes_core::check_functions_in_node(n, &mut { leaky })? || !crate::createplan::collation_determinate(n)? {
                        return Ok(true);
                    }
                }
                _ => return Ok(true),
            }
            nodes_core::expression_tree_walker(n, self)
        }
    }
    Ok(!nodes_core::NodeWalker::visit(&mut W(aggs), node)?)
}

/// `(inner, outer, op, inner_is_left)` when `c` is `inner = outer` on the inner type's btree
/// equality; None otherwise.
pub(crate) fn correlation_key<'mcx>(c: Node<'mcx>) -> PgResult<Option<(Node<'mcx>, Node<'mcx>, &'mcx OpExpr<'mcx>, bool)>> {
    let Some(op) = c.as_op_expr() else { return Ok(None) };
    if op.args.len() != 2 || op.opretset || clauses::contain_volatile_functions(c)? {
        return Ok(None);
    }
    let (l, r) = (op.args.nth(0), op.args.nth(1));
    let (l1, r1) = (vars::contain_vars_of_level(l, 1)?, vars::contain_vars_of_level(r, 1)?);
    let (inner, outer, inner_left) = match (l1, r1) {
        (false, true) => (l, r, true),
        (true, false) => (r, l, false),
        _ => return Ok(None),
    };
    if vars::contain_vars_of_level(outer, 0)? || contains_aggs_or_sublinks(outer)? || contains_aggs_or_sublinks(inner)? {
        return Ok(None);
    }
    let (ity, _) = ::costsize::expr_type_typmod(inner);
    let tc = typcache::lookup_type_cache(ity, typcache::TYPECACHE_EQ_OPR | typcache::TYPECACHE_LT_OPR | typcache::TYPECACHE_HASH_PROC)?;
    if tc.eq_opr() == 0 || tc.lt_opr() == 0 || tc.btree_opf() == 0 {
        return Ok(None);
    }
    if lsyscache::get_op_opfamily_strategy(op.opno, tc.btree_opf())? != lsyscache::BTEqualStrategyNumber as i32 {
        return Ok(None);
    }
    if lsyscache::type_is_collatable(ity)? && op.inputcollid != crate::pathkeys::expr_collation(inner) {
        return Ok(None);
    }
    Ok(Some((inner, outer, op, inner_left)))
}

pub(crate) fn names_rte(node: Node<'_>, rti: i32, levelsup: u32) -> PgResult<bool> {
    struct W(i32, u32);
    impl<'m> nodes_core::NodeWalker<'m> for W {
        fn visit(&mut self, n: Node<'m>) -> PgResult<bool> {
            if let Some(v) = n.as_var() {
                return Ok(v.varlevelsup == self.1 && v.varno == self.0);
            }
            nodes_core::expression_tree_walker(n, self)
        }
    }
    nodes_core::NodeWalker::visit(&mut W(rti, levelsup), node)
}

fn contains_aggs_or_sublinks(node: Node<'_>) -> PgResult<bool> {
    struct W(bool);
    impl<'m> nodes_core::NodeWalker<'m> for W {
        fn visit(&mut self, n: Node<'m>) -> PgResult<bool> {
            if matches!(n.node_tag(), NodeTag::T_Aggref | NodeTag::T_SubLink | NodeTag::T_GroupingFunc) {
                self.0 = true;
                return Ok(true);
            }
            nodes_core::expression_tree_walker(n, self)
        }
    }
    let mut w = W(false);
    nodes_core::NodeWalker::visit(&mut w, node)?;
    Ok(w.0)
}

fn empty_input_value(mcx: Mcx<'_>, agg: &Aggref<'_>) -> PgResult<Option<bool>> {
    if lsyscache::get_func_namespace(agg.aggfnoid)? != types_core::catalog::PG_CATALOG_NAMESPACE {
        return Ok(None);
    }
    let Some(name) = lsyscache::get_func_name(mcx, agg.aggfnoid)? else { return Ok(None) };
    let name = name.as_str();
    if name == "count" {
        return Ok(Some(true));
    }
    Ok(EMPTY_NULL_AGGS.contains(&name).then_some(false))
}

pub(crate) fn agg_admits(mcx: Mcx<'_>, agg: &Aggref<'_>) -> PgResult<Option<bool>> {
    if agg.agglevelsup != 0 || agg.aggkind != AGGKIND_NORMAL || !agg.aggdirectargs.is_nil() {
        return Ok(None);
    }
    // Under a nondeterministic collation, min / max inputs can tie and differ in bytes: the grouped form keeps the last
    // tie, PostgreSQL's per-row min/max path the first.
    if agg.inputcollid != 0 && !lsyscache::get_collation_isdeterministic(agg.inputcollid)? {
        return Ok(None);
    }
    empty_input_value(mcx, agg)
}

struct AggCollector<'mcx> {
    mcx: Mcx<'mcx>,
    count: usize,
    admitted: bool,
}

impl<'mcx> AggCollector<'mcx> {
    fn walk(&mut self, node: Node<'mcx>) -> PgResult<bool> {
        nodes_core::NodeWalker::visit(self, node)
    }
}

impl<'mcx> nodes_core::NodeWalker<'mcx> for AggCollector<'mcx> {
    fn visit(&mut self, n: Node<'mcx>) -> PgResult<bool> {
        if let Some(agg) = n.as_aggref() {
            self.count += 1;
            let mut safe = match agg.aggfilter {
                Some(f) => eager_safe(f)?,
                None => true,
            };
            for a in agg.args.iter() {
                safe = safe && eager_safe(a.as_target_entry().expect("an aggregate argument").expr)?;
            }
            if agg_admits(self.mcx, agg)?.is_none() || !safe {
                self.admitted = false;
                return Ok(true);
            }
            return Ok(false);
        }
        if n.as_var().is_some_and(|v| v.varlevelsup == 0) {
            self.admitted = false;
            return Ok(true);
        }
        nodes_core::expression_tree_walker(n, self)
    }
}

pub(crate) struct AggLift<'mcx> {
    pub(crate) mcx: Mcx<'mcx>,
    pub(crate) sub_rti: i32,
    pub(crate) nkeys: usize,
    pub(crate) next: usize,
    pub(crate) tlist: NodeList<'mcx>,
    pub(crate) colnames: NodeList<'mcx>,
    pub(crate) nulling: Bitmapset<'mcx>,
}

impl<'mcx> AggLift<'mcx> {
    pub(crate) fn mutate(&mut self, node: Node<'mcx>) -> PgResult<Option<Node<'mcx>>> {
        let mcx = self.mcx;
        if let Some(agg) = node.as_aggref() {
            let is_count = agg_admits(mcx, agg)?.expect("admitted by the collector");
            let i = self.next;
            self.next += 1;
            let resno = (self.nkeys + i + 1) as i16;
            self.tlist.lappend(
                mcx,
                Node::mk(
                    mcx,
                    TargetEntry { expr: node, resno, resname: Some(AGG_NAMES[i]), ressortgroupref: 0, resorigtbl: 0, resorigcol: 0, resjunk: false },
                )?,
            )?;
            self.colnames.lappend(mcx, Node::mk_string(mcx, AGG_NAMES[i])?)?;
            let var = Node::mk(
                mcx,
                Var {
                    varno: self.sub_rti,
                    varattno: resno,
                    vartype: agg.aggtype,
                    vartypmod: -1,
                    varcollid: agg.aggcollid,
                    varnullingrels: self.nulling.clone_in(mcx)?,
                    varnosyn: self.sub_rti as u32,
                    varattnosyn: resno,
                    ..Default::default()
                },
            )?;
            if !is_count {
                return Ok(Some(var));
            }
            let zero = Node::mk(
                mcx,
                Const {
                    consttype: INT8OID,
                    consttypmod: -1,
                    constcollid: 0,
                    constlen: 8,
                    constvalue: datum::Datum::from_i64(0),
                    constisnull: false,
                    constbyval: true,
                    location: -1,
                },
            )?;
            return Ok(Some(Node::mk(
                mcx,
                CoalesceExpr { coalescetype: INT8OID, coalescecollid: 0, args: NodeList::make2(mcx, var, zero)?, location: -1 },
            )?));
        }
        nodes_core::expression_tree_mutator(mcx, node, &mut |n| self.mutate(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use types_nodes::parsenodes::RangeTblFunction;
    use types_nodes::primnodes::{BoolExpr, BoolExprType, CaseExpr, CaseWhen, FuncExpr, NullTest, NullTestType};

    const TBL: u32 = 16384;
    const INT4: u32 = 23;
    const INT8: u32 = 20;
    const BOOL: u32 = 16;
    const INT4EQ: (u32, u32) = (96, 65);
    const INT4GT: (u32, u32) = (521, 147);
    const INT4PL: (u32, u32) = (551, 177);
    const INT8GT: (u32, u32) = (413, 470);
    const COUNT_STAR: u32 = 2803;
    const SUM_INT4: u32 = 2108;
    const GENERATE_SERIES: u32 = 1067;

    fn cx() -> mcx::MemoryContext {
        crate::tests::install_fixtures();
        mcx::MemoryContext::new("decorrelate")
    }

    fn rel_rte(mcx: Mcx<'_>) -> Node<'_> {
        Node::mk(mcx, RangeTblEntry { rtekind: RTEKind::RTE_RELATION, relid: TBL, relkind: b'r', rellockmode: 1, inFromCl: true, ..Default::default() }).unwrap()
    }

    fn var(mcx: Mcx<'_>, varno: i32, attno: i16, levelsup: u32) -> Node<'_> {
        Node::mk_var(mcx, varno, attno, INT4, -1, 0, levelsup).unwrap()
    }

    fn int4(mcx: Mcx<'_>, v: i32) -> Node<'_> {
        Node::mk_const(mcx, INT4, -1, 0, 4, datum::Datum::from_i32(v), false, true).unwrap()
    }

    fn op<'m>(mcx: Mcx<'m>, (opno, opfuncid): (u32, u32), restype: u32, l: Node<'m>, r: Node<'m>) -> Node<'m> {
        Node::mk(mcx, OpExpr { opno, opfuncid, opresulttype: restype, opretset: false, opcollid: 0, inputcollid: 0, args: NodeList::make2(mcx, l, r).unwrap(), location: -1 }).unwrap()
    }

    fn count_star(mcx: Mcx<'_>) -> Node<'_> {
        Node::mk(mcx, Aggref { aggfnoid: COUNT_STAR, aggtype: INT8, aggstar: true, ..Aggref::default() }).unwrap()
    }

    fn sum_of<'m>(mcx: Mcx<'m>, expr: Node<'m>, filter: Option<Node<'m>>) -> Node<'m> {
        let mut aggargtypes = types_nodes::list::OidList::nil();
        aggargtypes.lappend(mcx, INT4).unwrap();
        let args = NodeList::make1(mcx, Node::mk_target_entry(mcx, expr, 1, None, false).unwrap()).unwrap();
        Node::mk(mcx, Aggref { aggfnoid: SUM_INT4, aggtype: INT8, aggargtypes, args, aggfilter: filter, ..Aggref::default() }).unwrap()
    }

    fn from_expr<'m>(mcx: Mcx<'m>, quals: Option<Node<'m>>) -> Option<&'m FromExpr<'m>> {
        let rtr = Node::mk_range_tbl_ref(mcx, 1).unwrap();
        Some(mcx::alloc_leak_in(mcx, FromExpr { fromlist: NodeList::make1(mcx, rtr).unwrap(), quals }).unwrap())
    }

    fn select<'m>(mcx: Mcx<'m>, rtable: NodeList<'m>, tlist: &[Node<'m>], quals: Option<Node<'m>>) -> Query<'m> {
        let mut target_list = NodeList::nil();
        for (i, e) in tlist.iter().enumerate() {
            target_list.lappend(mcx, Node::mk_target_entry(mcx, *e, (i + 1) as i16, None, false).unwrap()).unwrap();
        }
        let has_aggs = tlist.iter().any(|e| e.as_aggref().is_some());
        Query { commandType: CmdType::CMD_SELECT, canSetTag: true, hasAggs: has_aggs, rtable, jointree: from_expr(mcx, quals), targetList: target_list, ..Query::default() }
    }

    fn sublink<'m>(mcx: Mcx<'m>, sub: Query<'m>) -> Node<'m> {
        Node::mk(mcx, SubLink { subLinkType: SubLinkType::EXPR_SUBLINK, subLinkId: 0, testexpr: None, operName: NodeList::nil(), subselect: Node::mk(mcx, sub).unwrap(), location: -1 }).unwrap()
    }

    fn sub<'m>(mcx: Mcx<'m>, agg: Node<'m>, outer_key: Node<'m>, kept: Option<Node<'m>>) -> Node<'m> {
        let key = op(mcx, INT4EQ, BOOL, var(mcx, 1, 2, 0), outer_key);
        let quals = match kept {
            Some(k) => Node::mk(mcx, BoolExpr { boolop: BoolExprType::AND_EXPR, args: NodeList::make2(mcx, key, k).unwrap(), location: -1 }).unwrap(),
            None => key,
        };
        sublink(mcx, select(mcx, NodeList::make1(mcx, rel_rte(mcx)).unwrap(), &[agg], Some(quals)))
    }

    fn outer<'m>(mcx: Mcx<'m>, tlist: &[Node<'m>], quals: Option<Node<'m>>) -> Query<'m> {
        let mut q = select(mcx, NodeList::make1(mcx, rel_rte(mcx)).unwrap(), tlist, quals);
        q.hasSubLinks = true;
        q
    }

    fn rewritten<'m>(mcx: Mcx<'m>, q: &Query<'m>) -> Option<&'m mut Query<'m>> {
        alternative(mcx, q).unwrap()
    }

    fn grouped_rte<'m>(q: &Query<'m>) -> Option<&'m Query<'m>> {
        q.rtable.iter().find_map(|n| {
            let r = n.as_range_tbl_entry().unwrap();
            (r.alias.is_some_and(|a| a.aliasname == Some(SUBQUERY_ALIAS))).then(|| r.subquery.unwrap())
        })
    }

    #[test]
    fn tlist_top_count_is_a_grouped_left_join() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let q = outer(mcx, &[sub(mcx, count_star(mcx), var(mcx, 1, 2, 1), None)], None);
        let alt = rewritten(mcx, &q).expect("admitted");
        let g = grouped_rte(alt).expect("the grouped subquery");
        assert_eq!(g.groupClause.len(), 1);
        assert_eq!(g.targetList.len(), 2);
        assert!(g.jointree.unwrap().quals.is_none(), "the correlation conjunct left the inner");
        let join = alt.jointree.unwrap().fromlist.nth(0);
        assert_eq!(join.as_join_expr().unwrap().jointype, JoinType::JOIN_LEFT);
        let te = alt.targetList.nth(0).as_target_entry().unwrap();
        assert!(te.expr.as_coalesce_expr().is_some(), "count(*) is coalesce(a1, 0)");
    }

    #[test]
    fn where_conjunct_operand_is_admitted() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let cmp = op(mcx, INT8GT, BOOL, sub(mcx, count_star(mcx), var(mcx, 1, 2, 1), None), Node::mk_const(mcx, INT8, -1, 0, 8, datum::Datum::from_i64(0), false, true).unwrap());
        let and = Node::mk(mcx, BoolExpr { boolop: BoolExprType::AND_EXPR, args: NodeList::make2(mcx, op(mcx, INT4GT, BOOL, var(mcx, 1, 1, 0), int4(mcx, 0)), cmp).unwrap(), location: -1 }).unwrap();
        let q = outer(mcx, &[var(mcx, 1, 1, 0)], Some(and));
        let alt = rewritten(mcx, &q).expect("admitted");
        assert!(grouped_rte(alt).is_some());
    }

    #[test]
    fn leakproof_kept_conjunct_is_admitted() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let kept = op(mcx, INT4GT, BOOL, var(mcx, 1, 1, 0), int4(mcx, 0));
        let q = outer(mcx, &[sub(mcx, count_star(mcx), var(mcx, 1, 2, 1), Some(kept))], None);
        let alt = rewritten(mcx, &q).expect("admitted");
        let g = grouped_rte(alt).expect("the grouped subquery");
        assert!(g.jointree.unwrap().quals.unwrap().as_op_expr().is_some(), "the kept conjunct stays in the inner");
    }

    #[test]
    fn kept_text_comparison_needs_a_determinate_collation() {
        // t2.c1 = t2.c2 over a COLLATE "C" and a COLLATE "POSIX" column: texteq is leakproof and strict, yet
        // with no input collation it raises 42P22 at the first non-null pair, which the join form reaches on rows
        // PostgreSQL's subplan never reads. Under COLLATE "C" the call is determinate and admitted.
        const TEXT: u32 = 25;
        const TEXTEQ: (u32, u32) = (98, 67);
        let ctx = cx();
        let mcx = ctx.mcx();
        let text = |attno, coll| Node::mk_var(mcx, 1, attno, TEXT, -1, coll, 0).unwrap();
        let texteq = |inputcollid| {
            Node::mk(mcx, OpExpr { opno: TEXTEQ.0, opfuncid: TEXTEQ.1, opresulttype: BOOL, opretset: false, opcollid: 0, inputcollid, args: NodeList::make2(mcx, text(3, 950), text(4, 951)).unwrap(), location: -1 }).unwrap()
        };
        assert!(!crate::createplan::collation_determinate(texteq(0)).unwrap());
        assert!(crate::createplan::collation_determinate(texteq(950)).unwrap());
        assert!(crate::createplan::collation_determinate(op(mcx, INT4GT, BOOL, var(mcx, 1, 1, 0), int4(mcx, 0))).unwrap(), "no collatable argument");
        let co = || Node::mk(mcx, CoalesceExpr { coalescetype: TEXT, coalescecollid: 0, args: NodeList::make2(mcx, text(3, 950), text(4, 951)).unwrap(), location: -1 }).unwrap();
        let both = Node::mk(mcx, OpExpr { opno: TEXTEQ.0, opfuncid: TEXTEQ.1, opresulttype: BOOL, opretset: false, opcollid: 0, inputcollid: 0, args: NodeList::make2(mcx, co(), co()).unwrap(), location: -1 }).unwrap();
        assert!(!crate::createplan::collation_determinate(both).unwrap());
        let q = outer(mcx, &[sub(mcx, count_star(mcx), var(mcx, 1, 2, 1), Some(texteq(0)))], None);
        assert!(rewritten(mcx, &q).is_none(), "the indeterminate texteq refuses");
        let q = outer(mcx, &[sub(mcx, count_star(mcx), var(mcx, 1, 2, 1), Some(texteq(950)))], None);
        assert!(rewritten(mcx, &q).is_some(), "under COLLATE \"C\" it is admitted");
    }

    #[test]
    fn level_two_reference_in_a_kept_conjunct_is_admitted_unchanged() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let kept = op(mcx, INT4EQ, BOOL, var(mcx, 1, 1, 0), var(mcx, 1, 1, 2));
        let q = outer(mcx, &[sub(mcx, count_star(mcx), var(mcx, 1, 2, 1), Some(kept))], None);
        let alt = rewritten(mcx, &q).expect("admitted");
        let g = grouped_rte(alt).expect("the grouped subquery");
        let kept = g.jointree.unwrap().quals.unwrap().as_op_expr().unwrap();
        assert_eq!(kept.args.nth(1).as_var().unwrap().varlevelsup, 2);
    }

    #[test]
    fn level_one_reference_inside_a_function_rte_refuses() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let series = Node::mk(mcx, FuncExpr { funcid: GENERATE_SERIES, funcresulttype: INT4, funcretset: true, args: NodeList::make2(mcx, int4(mcx, 1), var(mcx, 1, 2, 1)).unwrap(), ..FuncExpr::default() }).unwrap();
        let rtf = Node::mk(mcx, RangeTblFunction { funcexpr: Some(series), funccolcount: 1, ..RangeTblFunction::default() }).unwrap();
        let rte = Node::mk(mcx, RangeTblEntry { rtekind: RTEKind::RTE_FUNCTION, functions: NodeList::make1(mcx, rtf).unwrap(), inFromCl: true, ..Default::default() }).unwrap();
        let key = op(mcx, INT4EQ, BOOL, var(mcx, 1, 1, 0), var(mcx, 1, 2, 1));
        let inner = select(mcx, NodeList::make1(mcx, rte).unwrap(), &[count_star(mcx)], Some(key));
        let q = outer(mcx, &[sublink(mcx, inner)], None);
        assert!(rewritten(mcx, &q).is_none());
    }

    #[test]
    fn level_one_reference_inside_a_subquery_rte_refuses() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let nested = select(mcx, NodeList::make1(mcx, rel_rte(mcx)).unwrap(), &[var(mcx, 1, 2, 0)], Some(op(mcx, INT4EQ, BOOL, var(mcx, 1, 1, 0), var(mcx, 1, 2, 2))));
        let rte = Node::mk(mcx, RangeTblEntry { rtekind: RTEKind::RTE_SUBQUERY, subquery: Some(mcx::alloc_leak_in(mcx, nested).unwrap()), inFromCl: true, ..Default::default() }).unwrap();
        let key = op(mcx, INT4EQ, BOOL, var(mcx, 1, 1, 0), var(mcx, 1, 2, 1));
        let inner = select(mcx, NodeList::make1(mcx, rte).unwrap(), &[count_star(mcx)], Some(key));
        let q = outer(mcx, &[sublink(mcx, inner)], None);
        assert!(rewritten(mcx, &q).is_none());
    }

    #[test]
    fn outer_aggregate_in_a_kept_conjunct_refuses() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let outer_count = Node::mk(mcx, Aggref { aggfnoid: COUNT_STAR, aggtype: INT8, aggstar: true, agglevelsup: 1, ..Aggref::default() }).unwrap();
        let kept = op(mcx, INT8GT, BOOL, outer_count, Node::mk_const(mcx, INT8, -1, 0, 8, datum::Datum::from_i64(0), false, true).unwrap());
        let mut q = outer(mcx, &[sub(mcx, count_star(mcx), var(mcx, 1, 2, 1), Some(kept))], None);
        q.hasAggs = true;
        assert!(rewritten(mcx, &q).is_none());
    }

    #[test]
    fn sublink_under_case_refuses() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let when = Node::mk(mcx, CaseWhen { expr: Some(op(mcx, INT4GT, BOOL, var(mcx, 1, 2, 0), int4(mcx, 0))), result: Some(sub(mcx, count_star(mcx), var(mcx, 1, 2, 1), None)), location: -1 }).unwrap();
        let case = Node::mk(mcx, CaseExpr { casetype: INT8, casecollid: 0, arg: None, args: NodeList::make1(mcx, when).unwrap(), defresult: None, location: -1 }).unwrap();
        let q = outer(mcx, &[case], None);
        assert!(rewritten(mcx, &q).is_none());
    }

    #[test]
    fn sublink_under_or_refuses() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let cmp = op(mcx, INT8GT, BOOL, sub(mcx, count_star(mcx), var(mcx, 1, 2, 1), None), Node::mk_const(mcx, INT8, -1, 0, 8, datum::Datum::from_i64(0), false, true).unwrap());
        let or = Node::mk(mcx, BoolExpr { boolop: BoolExprType::OR_EXPR, args: NodeList::make2(mcx, op(mcx, INT4GT, BOOL, var(mcx, 1, 1, 0), int4(mcx, 0)), cmp).unwrap(), location: -1 }).unwrap();
        let q = outer(mcx, &[var(mcx, 1, 1, 0)], Some(or));
        assert!(rewritten(mcx, &q).is_none());
    }

    #[test]
    fn sublink_in_a_coalesce_arm_refuses() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let c = Node::mk(mcx, CoalesceExpr { coalescetype: INT8, coalescecollid: 0, args: NodeList::make2(mcx, var(mcx, 1, 2, 0), sub(mcx, count_star(mcx), var(mcx, 1, 2, 1), None)).unwrap(), location: -1 }).unwrap();
        let q = outer(mcx, &[c], None);
        assert!(rewritten(mcx, &q).is_none());
    }

    #[test]
    fn sublink_below_a_tlist_expression_refuses() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let nt = Node::mk(mcx, NullTest { arg: Some(sub(mcx, count_star(mcx), var(mcx, 1, 2, 1), None)), nulltesttype: NullTestType::IS_NOT_NULL, argisrow: false, location: -1 }).unwrap();
        let q = outer(mcx, &[nt], None);
        assert!(rewritten(mcx, &q).is_none());
    }

    #[test]
    fn grouped_outer_key_naming_the_group_rte_refuses() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let group = Node::mk(mcx, RangeTblEntry { rtekind: RTEKind::RTE_GROUP, groupexprs: NodeList::make1(mcx, var(mcx, 1, 2, 0)).unwrap(), ..Default::default() }).unwrap();
        let mut q = outer(mcx, &[var(mcx, 2, 1, 0), sub(mcx, count_star(mcx), var(mcx, 2, 1, 1), None)], None);
        q.rtable.lappend(mcx, group).unwrap();
        q.hasGroupRTE = true;
        q.groupClause = NodeList::make1(mcx, Node::mk(mcx, SortGroupClause { tleSortGroupRef: 1, eqop: INT4EQ.0, sortop: 97, reverse_sort: false, nulls_first: false, hashable: true }).unwrap()).unwrap();
        // SAFETY: the entry is ours alone.
        unsafe { q.targetList.nth(0).with_mut::<TargetEntry, _>(|t| t.ressortgroupref = 1) }.unwrap();
        assert!(rewritten(mcx, &q).is_none());
        let cmp = op(mcx, INT8GT, BOOL, sub(mcx, count_star(mcx), var(mcx, 1, 2, 1), None), Node::mk_const(mcx, INT8, -1, 0, 8, datum::Datum::from_i64(0), false, true).unwrap());
        q.targetList = NodeList::make1(mcx, Node::mk_target_entry(mcx, var(mcx, 2, 1, 0), 1, None, false).unwrap()).unwrap();
        // SAFETY: as above.
        unsafe { q.targetList.nth(0).with_mut::<TargetEntry, _>(|t| t.ressortgroupref = 1) }.unwrap();
        q.jointree = from_expr(mcx, Some(cmp));
        assert!(rewritten(mcx, &q).is_some());
    }

    #[test]
    fn sublink_in_having_refuses() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let cmp = op(mcx, INT8GT, BOOL, sub(mcx, count_star(mcx), var(mcx, 1, 2, 1), None), Node::mk_const(mcx, INT8, -1, 0, 8, datum::Datum::from_i64(0), false, true).unwrap());
        let mut q = outer(mcx, &[var(mcx, 1, 2, 0)], None);
        q.havingQual = Some(cmp);
        q.groupClause = NodeList::make1(mcx, Node::mk(mcx, SortGroupClause { tleSortGroupRef: 1, eqop: INT4EQ.0, sortop: 97, reverse_sort: false, nulls_first: false, hashable: true }).unwrap()).unwrap();
        // SAFETY: the entry is ours alone.
        unsafe { q.targetList.nth(0).with_mut::<TargetEntry, _>(|t| t.ressortgroupref = 1) }.unwrap();
        assert!(rewritten(mcx, &q).is_none());
    }

    #[test]
    fn sublink_in_an_aggregate_filter_refuses() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let cmp = op(mcx, INT8GT, BOOL, sub(mcx, count_star(mcx), var(mcx, 1, 2, 1), None), Node::mk_const(mcx, INT8, -1, 0, 8, datum::Datum::from_i64(0), false, true).unwrap());
        let q = outer(mcx, &[sum_of(mcx, var(mcx, 1, 2, 0), Some(cmp))], None);
        assert!(rewritten(mcx, &q).is_none());
    }

    #[test]
    fn leaky_outer_key_refuses() {
        let ctx = cx();
        let mcx = ctx.mcx();
        // t2.val = t.val + 1: int4pl is not leakproof.
        let q = outer(mcx, &[sub(mcx, count_star(mcx), op(mcx, INT4PL, INT4, var(mcx, 1, 2, 1), int4(mcx, 1)), None)], None);
        assert!(rewritten(mcx, &q).is_none());
    }

    #[test]
    fn leaky_aggregate_argument_refuses() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let q = outer(mcx, &[sub(mcx, sum_of(mcx, op(mcx, INT4PL, INT4, var(mcx, 1, 1, 0), int4(mcx, 1)), None), var(mcx, 1, 2, 1), None)], None);
        assert!(rewritten(mcx, &q).is_none());
        let q = outer(mcx, &[sub(mcx, sum_of(mcx, var(mcx, 1, 1, 0), None), var(mcx, 1, 2, 1), None)], None);
        assert!(rewritten(mcx, &q).is_some(), "a plain Var argument is admitted");
    }

    #[test]
    fn leaky_aggregate_filter_refuses() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let filter = op(mcx, INT4GT, BOOL, op(mcx, INT4PL, INT4, var(mcx, 1, 1, 0), int4(mcx, 1)), int4(mcx, 0));
        let q = outer(mcx, &[sub(mcx, sum_of(mcx, var(mcx, 1, 1, 0), Some(filter)), var(mcx, 1, 2, 1), None)], None);
        assert!(rewritten(mcx, &q).is_none());
    }

    #[test]
    fn leaky_kept_conjunct_refuses() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let kept = op(mcx, INT4GT, BOOL, op(mcx, INT4PL, INT4, var(mcx, 1, 1, 0), int4(mcx, 1)), int4(mcx, 0));
        let q = outer(mcx, &[sub(mcx, count_star(mcx), var(mcx, 1, 2, 1), Some(kept))], None);
        assert!(rewritten(mcx, &q).is_none());
    }

    #[test]
    fn function_rte_without_outer_references_refuses() {
        let ctx = cx();
        let mcx = ctx.mcx();
        // (SELECT count(*) FROM generate_series(1, 3) g(i) WHERE g.i = t.val): the SRF is not leakproof.
        let series = Node::mk(mcx, FuncExpr { funcid: GENERATE_SERIES, funcresulttype: INT4, funcretset: true, args: NodeList::make2(mcx, int4(mcx, 1), int4(mcx, 3)).unwrap(), ..FuncExpr::default() }).unwrap();
        let rtf = Node::mk(mcx, RangeTblFunction { funcexpr: Some(series), funccolcount: 1, ..RangeTblFunction::default() }).unwrap();
        let rte = Node::mk(mcx, RangeTblEntry { rtekind: RTEKind::RTE_FUNCTION, functions: NodeList::make1(mcx, rtf).unwrap(), inFromCl: true, ..Default::default() }).unwrap();
        let key = op(mcx, INT4EQ, BOOL, var(mcx, 1, 1, 0), var(mcx, 1, 2, 1));
        let inner = select(mcx, NodeList::make1(mcx, rte).unwrap(), &[count_star(mcx)], Some(key));
        let q = outer(mcx, &[sublink(mcx, inner)], None);
        assert!(rewritten(mcx, &q).is_none());
    }

    #[test]
    fn nested_subquery_rte_target_is_held_to_eager_safety() {
        let ctx = cx();
        let mcx = ctx.mcx();
        fn build<'m>(mcx: Mcx<'m>, e: Node<'m>) -> Query<'m> {
            let nested = select(mcx, NodeList::make1(mcx, rel_rte(mcx)).unwrap(), &[e], None);
            let rte = Node::mk(mcx, RangeTblEntry { rtekind: RTEKind::RTE_SUBQUERY, subquery: Some(mcx::alloc_leak_in(mcx, nested).unwrap()), inFromCl: true, ..Default::default() }).unwrap();
            let key = op(mcx, INT4EQ, BOOL, var(mcx, 1, 1, 0), var(mcx, 1, 2, 1));
            let inner = select(mcx, NodeList::make1(mcx, rte).unwrap(), &[count_star(mcx)], Some(key));
            outer(mcx, &[sublink(mcx, inner)], None)
        }
        assert!(rewritten(mcx, &build(mcx, op(mcx, INT4PL, INT4, var(mcx, 1, 1, 0), int4(mcx, 1)))).is_none());
        assert!(rewritten(mcx, &build(mcx, var(mcx, 1, 1, 0))).is_some(), "a plain Var target is admitted");
    }

    #[test]
    fn merge_source_subquery_keeps_its_sublinks() {
        let ctx = cx();
        let mcx = ctx.mcx();
        fn build<'m>(mcx: Mcx<'m>, cmd: CmdType) -> Query<'m> {
            let source = outer(mcx, &[sub(mcx, count_star(mcx), var(mcx, 1, 2, 1), None)], None);
            let rte = Node::mk(mcx, RangeTblEntry { rtekind: RTEKind::RTE_SUBQUERY, subquery: Some(mcx::alloc_leak_in(mcx, source).unwrap()), inFromCl: true, ..Default::default() }).unwrap();
            let rtable = NodeList::make2(mcx, rel_rte(mcx), rte).unwrap();
            Query { commandType: cmd, canSetTag: true, resultRelation: 1, rtable, jointree: from_expr(mcx, None), ..Query::default() }
        }
        assert!(rewritten(mcx, &build(mcx, CmdType::CMD_MERGE)).is_none(), "a MERGE's source keeps its SubLink");
        for cmd in [CmdType::CMD_UPDATE, CmdType::CMD_DELETE, CmdType::CMD_INSERT] {
            assert!(rewritten(mcx, &build(mcx, cmd)).is_some(), "{cmd:?}");
        }
    }

    #[test]
    fn cte_rte_refuses() {
        let ctx = cx();
        let mcx = ctx.mcx();
        let rte = Node::mk(mcx, RangeTblEntry { rtekind: RTEKind::RTE_CTE, ctename: Some("c"), inFromCl: true, ..Default::default() }).unwrap();
        let key = op(mcx, INT4EQ, BOOL, var(mcx, 1, 1, 0), var(mcx, 1, 2, 1));
        let inner = select(mcx, NodeList::make1(mcx, rte).unwrap(), &[count_star(mcx)], Some(key));
        let q = outer(mcx, &[sublink(mcx, inner)], None);
        assert!(rewritten(mcx, &q).is_none());
    }
}
