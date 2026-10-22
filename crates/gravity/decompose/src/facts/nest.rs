//! A nested loop's facts, read by the nest roads (nest.selfspool / eqkey / parallel / bulk /
//! memo): F1 rescan invariance and F5 over the inner (`NestFacts`), and F2, the equality conjunct a keyed walk of the
//! inner can take (`EqConjunct`, derived here by `eq_conjunct` from the join qual and the catalog's operator rows).
use core::fmt::{self, Write};

use types_nodes::list::NodeList;
use types_nodes::primnodes::INNER_VAR;
use types_nodes::Node;

use crate::expr;
use crate::walk::Builder;

/// A nested loop's facts, read by the nest roads (nest.selfspool / eqkey / parallel / bulk / memo).
#[derive(Clone, Copy, Debug)]
pub struct NestFacts<'p> {
    /// The NestLoop's plan node id (`ParamOwner::Nest` names it).
    pub node: i32,
    /// F1 (`rescan::invariant`): every rescan of the inner returns the same rows, so its first run may be kept and walked
    /// again: its `allParam` holds no param but ones fixed for the statement (none of this nest's, an enclosing nest's or
    /// SubPlan's), nothing in it is volatile, and nothing in it makes different rows per run.
    pub invariant: bool,
    /// F5 (`rescan::of_plan`) over the inner: its most volatile call and whether it can raise.
    pub volatility: crate::effects::Volatility,
    pub raise_free: bool,
    /// F2: the equality conjunct a keyed walk of the inner can take (`eq_conjunct`); none keeps the join qual a walk.
    pub eq: Option<EqConjunct<'p>>,
}

/// F2: the first join-qual conjunct `outer_expr = inner_col` whose operator hashes (`oprcanhash` with a hash opfamily
/// holding it as `=`; the type-dependent array / record / range equalities excluded), taken only when PostgreSQL could
/// never tell it was tested first: every conjunct before it (evaluated per pair ahead of it, `ExecQual`) is non-volatile
/// and raise-free. The outer side reads no inner column and is non-volatile. The rest of the qual stays as it is.
#[derive(Clone, Copy, Debug)]
pub struct EqConjunct<'p> {
    /// The conjunct's position in `joinqual` (the residual qual is the list without it).
    pub pos: u16,
    pub outer: Node<'p>,
    /// The inner column (a resno of the inner's output) the other side reads, bare or under a word-keeping relabel; 0 for
    /// a computed inner side (`eq_keyed`).
    pub inner_attno: i16,
    /// The inner side as the conjunct writes it: the column (`inner_attno`) or, from `eq_keyed`, an expression over inner
    /// columns only.
    pub inner: Node<'p>,
    pub opno: u32,
    /// The hash opfamily (`get_op_hash_functions`' first).
    pub opfamily: u32,
    /// Each side's type as the operator takes it: (outer, inner); 0 = unknown.
    pub types: (u32, u32),
    /// The operator's input collation (`inputcollid`), not the columns'.
    pub coll: u32,
    /// The outer side is the operator's left argument.
    pub outer_left: bool,
    /// Evaluating the outer side cannot raise. When false, PostgreSQL evaluates it only against an inner row (never for
    /// an empty inner), so a road computes the outer key once the inner has produced a row.
    pub outer_raise_free: bool,
}

/// F2 (`EqConjunct`): the first hashable equality of `qual`, when every conjunct before it is non-volatile and raise-free.
pub(crate) fn eq_conjunct<'p>(b: &Builder<'p, '_>, qual: &'p NodeList<'p>) -> Option<EqConjunct<'p>> {
    eq_by(b, qual, false)
}

/// D3014 build-once (nest.eqkey over a computed key): `eq_conjunct`, whose inner side may also be an expression over the
/// inner's columns alone (no other Var, no executor parameter, no SubPlan) that is non-volatile, acts nowhere and raises
/// on no input by the effects home's reading: the keyed form computes it once per inner row while it builds, where
/// PostgreSQL computes it per pair, so a key that may raise would raise on rows PostgreSQL may never pair (a unique inner
/// stops at its first match, an earlier conjunct may be false, a LIMIT may stop the outer). Such a key keeps the Nest.
pub(crate) fn eq_keyed<'p>(b: &Builder<'p, '_>, qual: &'p NodeList<'p>) -> Option<EqConjunct<'p>> {
    eq_by(b, qual, true)
}

fn eq_by<'p>(b: &Builder<'p, '_>, qual: &'p NodeList<'p>, computed: bool) -> Option<EqConjunct<'p>> {
    for (i, c) in qual.iter().enumerate() {
        if let Some(e) = eq_of(b, c, i, computed) {
            return Some(e);
        }
        let e = crate::effects::Effects::of_found(crate::rerun::of_expr(b.stmt, b.facts, b.arena, c));
        if e.acts || e.may_raise {
            return None;
        }
    }
    None
}

fn eq_of<'p>(b: &Builder<'p, '_>, c: Node<'p>, pos: usize, computed: bool) -> Option<EqConjunct<'p>> {
    let o = c.as_op_expr()?;
    let op = b.facts.oper(o.opno)?;
    if o.args.len() != 2 || !op.canhash || op.hash_family == 0 || op.poly {
        return None;
    }
    let inner_col = |n: Node<'p>| expr::through_relabel(n).unwrap_or(n).as_var().filter(|v| v.varno == INNER_VAR && v.varlevelsup == 0 && v.varattno > 0).map(|v| v.varattno);
    let (l, r) = (o.args.nth(0), o.args.nth(1));
    let (outer, inner, attno, outer_left) = match (inner_col(l), inner_col(r)) {
        (None, Some(a)) => (l, r, a, true),
        (Some(a), None) => (r, l, a, false),
        (None, None) if computed && inner_only(b, r) && !inner_only(b, l) => (l, r, 0, true),
        (None, None) if computed && inner_only(b, l) && !inner_only(b, r) => (r, l, 0, false),
        _ => return None,
    };
    let mut reads_inner = false;
    expr::walk(outer, &mut |x| {
        reads_inner |= x.as_var().is_some_and(|v| v.varno == INNER_VAR);
        !reads_inner
    });
    let e = crate::effects::Effects::of_found(crate::rerun::of_expr(b.stmt, b.facts, b.arena, outer));
    if reads_inner || e.acts {
        return None;
    }
    let types = (crate::catalog::expr_type::type_of(outer).unwrap_or(0), crate::catalog::expr_type::type_of(inner).unwrap_or(0));
    Some(EqConjunct { pos: pos as u16, outer, inner_attno: attno, inner, opno: o.opno, opfamily: op.hash_family, types, coll: o.inputcollid, outer_left, outer_raise_free: !e.may_raise })
}

/// `eq_keyed`'s computed inner side: expression `n` reads inner columns (`INNER_VAR`, this level) and no other Var, no
/// executor parameter (a statement parameter is fixed for the run) and no SubPlan; it is non-volatile, acts nowhere and
/// raises on no input (the effects home's reading).
fn inner_only<'p>(b: &Builder<'p, '_>, n: Node<'p>) -> bool {
    use types_nodes::primnodes::ParamKind;
    let (mut cols, mut other) = (0usize, false);
    expr::walk(n, &mut |x| {
        if let Some(v) = x.as_var() {
            if v.varno == INNER_VAR && v.varlevelsup == 0 && v.varattno > 0 { cols += 1 } else { other = true }
        }
        other |= x.as_param().is_some_and(|p| p.paramkind != ParamKind::PARAM_EXTERN) || x.as_sub_plan().is_some();
        !other
    });
    if cols == 0 || other || expr::has_subplan(n) {
        return false;
    }
    // `acts` reads a VOLATILE call (effects `of_found`)
    let e = crate::effects::Effects::of_found(crate::rerun::of_expr(b.stmt, b.facts, b.arena, n));
    !e.acts && !e.may_raise
}

impl NestFacts<'_> {
    /// ` invariant`, ` vol=..` / ` raise-free` (F5), ` eq=#<pos>:op<opno>:fam<opfamily>:<outer type>/<inner type>[@<coll>]:<outer
    /// side L|R>[:raises]` (F2).
    pub fn dump(&self, out: &mut dyn Write) -> fmt::Result {
        if self.invariant {
            write!(out, " invariant")?;
        }
        crate::rerun::dump_f5(out, self.volatility, self.raise_free)?;
        if let Some(e) = &self.eq {
            write!(out, " eq=#{}:op{}:fam{}:{}/{}", e.pos, e.opno, e.opfamily, e.types.0, e.types.1)?;
            if e.coll != 0 {
                write!(out, "@{}", e.coll)?;
            }
            write!(out, ":{}{}", if e.outer_left { "L" } else { "R" }, if e.outer_raise_free { "" } else { ":raises" })?;
        }
        Ok(())
    }
}
