//! gravity §3 #8: aggregate algebra (A70; widened by GENERALIZE R1, catalog PL-01 / FOLD-06 / SRC-40, the user's GO
//! 09-29). An aggregate over an affine chain `e` of one column `x` (`x`, `e ± c`, `c ± e`, `c * e`, `e * c` and the
//! widening casts int2 → int4 → int8 → numeric; `c` a non-null Const or a `$n`; `x` an int2 / int4 / int8 / numeric
//! Var, or one under a RelabelType: a domain's values, read as its base type's) is rewritten at plan time over the base
//! aggregates of `x`, so many of them share four aggregates (`sum(x + 1)`,
//! `sum(x + 2)`, .. `sum(x + k)` all read `sum(x)` and `count(x)`). A member's FILTER rides each of its base
//! aggregates, so it runs once per base where PostgreSQL runs it once per member's transition: a member is rewritten
//! only under a FILTER that raises on no row and calls only builtins, since another run shows through user code (a
//! NOTICE per call, an array's element comparison running a user default opclass). A raising family under a FILTER is
//! rewritten only where its base aggregates (its guard's min / max included) are no more than its members, so the
//! FILTER runs no more often than as written (`profitable`):
//!
//! - `sum(e)` = S(e), built by mirroring `e`: `x` → `sum(x)`, `± c` → `± c · count(x)`, `c *` → `c *`, computed in
//!   the sum type (int2 / int4 levels in int8, int8 / numeric levels in numeric). `e ± c` is NULL iff `x` is (strict
//!   operators, `c` not NULL; a NULL Param makes every side NULL), so both sides aggregate the same rows, and the result
//!   is NULL over no rows as `sum` is. Numeric display scales mirror the chain exactly (`+` keeps the larger, `*` adds):
//!   a product is taken only where no rounding can happen (the scales' sum is at most 16383).
//! - `avg(e)` = numeric_div(S(e) in numeric, count(x) in numeric): what int8_avg / numeric_poly_avg / numeric_avg
//!   compute from their sum and count, so the display scale is PostgreSQL's (never a·avg(x) + b).
//! - `count(e)` = `count(x)`; `min / max(e)` = `e(min(x))` / `e(max(x))`, swapped when `e` decreases in `x`
//!   (ints only: numeric NaN sorts high in both directions).
//!
//! Errors. Each step is classified by exact interval arithmetic over the type ranges (a numeric(p,s) column is bounded
//! by its typmod, a Param by its type): a raise-free member (`smallint + 1`, `smallint * 2`, numeric(p,s) ± c, an int
//! cast to numeric) needs no guard and is rewritten in any statement. A raising member (`int4 + 1` can leave int4)
//! raises in PostgreSQL at the row, during the scan; the rewrite raises at the projection through
//! `CASE WHEN e(min(x)) IS NOT NULL AND e(max(x)) IS NOT NULL THEN … END` with the original operators (`e` is monotone
//! in `x`, so a row overflows iff an edge does). That moves the raise, so a raising member is rewritten only when
//! it cannot change whether or which error the statement raises (`order_safe`). That needs plain relations (tables,
//! partitioned tables and inheritance parents whose members are tables; one or a join of several, directly, through
//! pulled-up subqueries, a CTE whose query is one more such scan, or a UNION ALL's arms), with or without GROUP BY:
//! every group's projection runs when its row is read. A HAVING drops groups whose transitions PostgreSQL ran,
//! so under one the guards run first in it, for every group (`having_first`), and the HAVING must be `quiet`. Every
//! qual on the way (WHERE, JOIN ON, the subqueries', row security) and every other target (grouping keys included) must
//! be `quiet`, and one result type must cover every raising operator (one message).
//! Numeric raising members (an unconstrained numeric at the format's limit) are left as written: their min / max hide
//! finite edges behind NaN and infinity.
//!
//! Exactness. PostgreSQL sums an int2 / int4 `e` in int8 and wraps past 2^63 (int2_sum / int4_sum / int4_avg_accum
//! never raise); a wider `e` it sums exactly. Every S is PostgreSQL's value, with no statistics input:
//! - a wider `e`: S in numeric over exact bases (`sum(x)` of an int8 / numeric x, `sum(x::int8)` of an int2 / int4 x);
//! - an int2 / int4 `e`: the int8 form (`sum(x) + c · count(x)`, whose intermediates stay within M · count(x), M from
//!   x's type range and the constants) up to N = (2^63 - 1) / M rows, past them the numeric S taken modulo 2^64 as
//!   PostgreSQL's accumulator wraps (`wrap64`: exact, the wrap being a ring map). The bare int8 form, A70's (finals.cse
//!   serves it), is kept where one relation feeds the aggregate and M · 2^47 < 2^63: the capacity cap
//!   ROW_CAP (main 09-29).
//!
//! Where it applies. Statements with grouping sets are left as written; under window functions or set-returning
//! targets only raise-free members are rewritten. An all-min / max aggregation with no GROUP BY is left as written
//! where planagg could answer it from an index (`planagg_may_index`), so PostgreSQL's plan stays. A column's members are
//! rewritten only when that lowers the per-row evaluations (operators + transitions; a lone `sum(s + 1)` stays), or
//! when finals.cse answers them: two or more of A70's guarded sums in an order-safe plain aggregation. A member that
//! also appears in HAVING is left as written (its transition stays).
//!
//! Kept bounds (shape probe P2; each statement's does-not-apply witness is e2e/gravity/v5/algebra_p2_bounds.sql):
//! - float members (PSP2-19): float addition is not associative, so Σ(f + c) and Σf + c · n differ in the last bits.
//! - raising members over an unconstrained numeric (PSP2-20): NaN and ±Infinity sort past every finite value, so min /
//!   max cannot show the guard a finite edge that overflows.
//! - two raise types (PSP2-21): an int2 and an int4 overflow carry different messages, and which PostgreSQL raises
//!   depends on the row it meets first, which a guard in the projection cannot know.
//! - a qual that may raise (PSP2-22, `quiet` false: a non-leakproof call such as `1 / j` or LIKE): PostgreSQL meets its
//!   raise and a member's at their rows in scan order; the guard would move the member's after every row's qual.
//!
//! The mark. Every guarded `sum(x ± k)` this rewrite builds over an int2 / int4 `x` and an integer literal `k`
//! (both of its spellings: the bare int8 form and the count-guarded one) is recorded as built (`mark`); the Agg that
//! holds it as a target entry carries it as a `GravityFinal` (`finals_of`, createplan's `create_agg_plan`): the entry
//! and its min / max / sum / count aggnos. Gravity's finals.cse reads that mark, never the entry's spelling: one
//! producer, one source of truth.
use types_core::catalog::{BOOLOID, INT2OID, INT4OID, INT8OID, NUMERICOID};
use types_core::Oid;
use types_error::PgResult;
use types_nodes::list::OidList;
use types_nodes::primnodes::{
    Aggref, BoolExpr, BoolExprType, CaseExpr, CaseWhen, CoercionForm, FromExpr, FuncExpr, NullTest, NullTestType, OpExpr,
    ParamKind, TargetEntry, Var,
};
use types_nodes::parsenodes::{Query, RTEKind, RangeTblEntry};
use types_nodes::plannodes::GravityFinal;
use types_nodes::{Node, NodeList, NodeTag};
use types_rel::{NoLock, RELKIND_PARTITIONED_TABLE, RELKIND_RELATION};

use crate::run::PlannerRun;
use cost_profile::anchor::Anchor;
use cost_profile::fit::Fitted;

const AVG_INT8: Oid = 2100;
const AVG_INT4: Oid = 2101;
const AVG_INT2: Oid = 2102;
const AVG_NUMERIC: Oid = 2103;
const SUM_INT8: Oid = 2107;
const SUM_INT4: Oid = 2108;
const SUM_INT2: Oid = 2109;
const SUM_NUMERIC: Oid = 2114;
const COUNT_ANY: Oid = 2147;
const COUNT_STAR: Oid = 2803;
const MAX_INT8: Oid = 2115;
const MAX_INT4: Oid = 2116;
const MAX_INT2: Oid = 2117;
const MAX_NUMERIC: Oid = 2130;
const MIN_INT8: Oid = 2131;
const MIN_INT4: Oid = 2132;
const MIN_INT2: Oid = 2133;
const MIN_NUMERIC: Oid = 2146;
const INT8_OPS: [(Oid, Oid); 3] = [(684, 463), (685, 464), (686, 465)];
const NUMERIC_OPS: [(Oid, Oid); 3] = [(1758, 1724), (1759, 1725), (1760, 1726)];
const NUMERIC_DIV: (Oid, Oid) = (1761, 1727);
const INT8_LE: (Oid, Oid) = (414, 471);
const INT8_LT: (Oid, Oid) = (412, 469);
const INT8_GT: (Oid, Oid) = (413, 470);
const INT8_NUMERIC_PROC: Oid = 1781;
const NUMERIC_INT8_PROC: Oid = 1779;

const NUMERIC_MOD: (Oid, Oid) = (1762, 1728);
/// A stated capacity cap, not a tuned constant (main's kept-cap ruling 09-29): one relation feeds an aggregate at most
/// 2^47 rows. Heap's physical bound is 2^32 blocks · MaxHeapTuplesPerPage (291 at 8 kB) < 2^40.2 rows; pgrcolumnar's
/// row ids are 64-bit, and 2^47 rows there is its practical bound (a petabyte-scale table). The bare int8 form, which
/// finals.cse serves, relies on it; catalog NEW (W1: finals.cse accepts the count-guarded form) removes it.
const ROW_CAP: i128 = 1 << 47;
const INT8_LIMIT: i128 = i64::MAX as i128;
const NUMERIC_MAX_DIGITS: i32 = 131072;
const NUMERIC_DSCALE_MAX: i32 = 16383;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Add,
    Sub,
    Mul,
}

const ARITH: [(Oid, Kind); 30] = [
    (176, Kind::Add), (180, Kind::Sub), (177, Kind::Add), (181, Kind::Sub), (463, Kind::Add), (464, Kind::Sub),
    (178, Kind::Add), (182, Kind::Sub), (179, Kind::Add), (183, Kind::Sub), (1274, Kind::Add), (1275, Kind::Sub),
    (1278, Kind::Add), (1279, Kind::Sub), (837, Kind::Add), (838, Kind::Sub), (841, Kind::Add), (942, Kind::Sub),
    (152, Kind::Mul), (141, Kind::Mul), (465, Kind::Mul), (170, Kind::Mul), (171, Kind::Mul), (1280, Kind::Mul),
    (1276, Kind::Mul), (943, Kind::Mul), (839, Kind::Mul), (1724, Kind::Add), (1725, Kind::Sub), (1726, Kind::Mul),
];

const CASTS: [(Oid, Oid, Oid); 6] = [
    (313, INT2OID, INT4OID), (754, INT2OID, INT8OID), (481, INT4OID, INT8OID), (1782, INT2OID, NUMERICOID),
    (1740, INT4OID, NUMERICOID), (1781, INT8OID, NUMERICOID),
];

#[derive(Clone, Copy)]
enum Step<'mcx> {
    Arith { kind: Kind, opno: Oid, opfuncid: Oid, restype: Oid, c: Node<'mcx>, c_first: bool },
    Cast { funcid: Oid, restype: Oid, format: CoercionForm },
}

#[derive(Clone, Copy)]
enum Range {
    Int(i128, i128),
    Num { digits: Option<i32>, scale: Option<i32> },
}

pub(crate) struct Chain<'mcx> {
    x: Node<'mcx>,
    xtype: Oid,
    steps: Vec<Step<'mcx>>,
    raising: bool,
    raise_types: Vec<Oid>,
    dir: Option<i8>,
    /// M: every int8 intermediate of the int8 form (sum(x), c · count(x), each level's total) is at most M · count(x)
    /// in magnitude (|x|, |c| of `± c`, |level| over the int2 / int4 levels).
    row_magnitude: i128,
    tfinal: Oid,
}

fn int_range(t: Oid) -> Option<(i128, i128)> {
    match t {
        INT2OID => Some((i16::MIN.into(), i16::MAX.into())),
        INT4OID => Some((i32::MIN.into(), i32::MAX.into())),
        INT8OID => Some((i64::MIN.into(), i64::MAX.into())),
        _ => None,
    }
}

fn dec_digits(v: u128) -> i32 {
    let (mut d, mut p) = (0, 1u128);
    while p <= v {
        d += 1;
        match p.checked_mul(10) {
            Some(n) => p = n,
            None => return d,
        }
    }
    d
}

fn const_range(c: Node<'_>) -> Option<(Range, Option<i8>)> {
    if let Some(p) = c.as_param() {
        return Some(match int_range(p.paramtype) {
            Some((lo, hi)) => (Range::Int(lo, hi), None),
            None if p.paramtype == NUMERICOID => (Range::Num { digits: None, scale: None }, None),
            None => return None,
        });
    }
    let k = c.as_const()?;
    if k.constisnull {
        return None;
    }
    let v: i128 = match k.consttype {
        INT2OID => k.constvalue.as_i16().into(),
        INT4OID => k.constvalue.as_i32().into(),
        INT8OID => k.constvalue.as_i64().into(),
        NUMERICOID => {
            let n = adt_numeric::Num::from_payload(crate::selfuncs::varlena_datum_payload(k.constvalue));
            if n.is_special() {
                return None;
            }
            let zero = n.ndigits() == 0;
            let sign = if zero { 0 } else if n.sign() == adt_numeric::NUMERIC_NEG { -1 } else { 1 };
            let digits = if zero { 0 } else { 4 * (n.weight() + 1) };
            return Some((Range::Num { digits: Some(digits.max(0)), scale: Some(n.dscale()) }, Some(sign)));
        }
        _ => return None,
    };
    Some((Range::Int(v, v), Some(v.signum() as i8)))
}

fn to_num(r: Range) -> Range {
    match r {
        Range::Int(lo, hi) => Range::Num { digits: Some(dec_digits(lo.unsigned_abs().max(hi.unsigned_abs()))), scale: Some(0) },
        n => n,
    }
}

fn magnitude(r: Range) -> i128 {
    match r {
        Range::Int(lo, hi) => lo.unsigned_abs().max(hi.unsigned_abs()).min(i128::MAX as u128) as i128,
        Range::Num { .. } => 0,
    }
}

fn column<'mcx>(x: Node<'mcx>) -> Option<&'mcx Var<'mcx>> {
    let mut n = x;
    while let Some(r) = n.as_relabel_type() {
        n = r.arg;
    }
    n.as_var()
}

/// PSP2-17: `x` an expression over this level's columns (`coalesce(k, 0)`) that raises on no row and runs no user code:
/// each base aggregate evaluates it once per row where PostgreSQL did once per member, which nothing observes. Its
/// range is its type's, as a column's.
fn row_expr(x: Node<'_>) -> PgResult<bool> {
    Ok(vars::contain_vars_of_level(x, 0)?
        && !vars::contain_uplevel_vars(x)?
        && quiet(x)?
        && builtin_calls(x)?
        && !clauses::contain_volatile_functions(x)?)
}

/// A `$n` read from the statement's parameter list. An initplan's output (PARAM_EXEC) is left out: PostgreSQL runs the
/// initplan only when a row reaches it, and the rewrite would run it at the projection over no rows too.
fn extern_param(n: Node<'_>) -> bool {
    n.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_EXTERN)
}

impl<'mcx> Chain<'mcx> {
    pub(crate) fn of(e: Node<'mcx>) -> PgResult<Option<Chain<'mcx>>> {
        let mut steps = Vec::new();
        let mut cur = e;
        let x = loop {
            if let Some(op) = cur.as_op_expr() {
                let f = if op.opfuncid != 0 { op.opfuncid } else { lsyscache::get_opcode(op.opno)? };
                let Some(&(_, kind)) = ARITH.iter().find(|a| a.0 == f) else { return Ok(None) };
                if op.args.len() != 2 {
                    return Ok(None);
                }
                let (a, b) = (op.args.nth(0), op.args.nth(1));
                let is_c = |n: Node<'_>| n.node_tag() == NodeTag::T_Const || extern_param(n);
                let (inner, c, c_first) = match (is_c(a), is_c(b)) {
                    (false, true) => (a, b, false),
                    (true, false) => (b, a, true),
                    _ => return Ok(None),
                };
                steps.push(Step::Arith { kind, opno: op.opno, opfuncid: f, restype: op.opresulttype, c, c_first });
                cur = inner;
            } else if let Some(fx) = cur.as_func_expr() {
                let Some(&(_, _, to)) = CASTS.iter().find(|k| k.0 == fx.funcid) else { return Ok(None) };
                if fx.args.len() != 1 || fx.funcretset || fx.funcresulttype != to {
                    return Ok(None);
                }
                steps.push(Step::Cast { funcid: fx.funcid, restype: to, format: fx.funcformat });
                cur = fx.args.nth(0);
            } else {
                break cur;
            }
        };
        // PSP2-04: a column under a RelabelType (a domain over its base type) is read as the relabel's type: the base
        // aggregates read that same expression, so no value is cast into the domain and its CHECK never runs.
        if !steps.iter().any(|s| matches!(s, Step::Arith { .. })) {
            return Ok(None);
        }
        let (xtype, xtypmod) = match column(x) {
            Some(var) if var.varlevelsup != 0 => return Ok(None),
            Some(var) => match x.as_relabel_type() {
                Some(r) => (r.resulttype, r.resulttypmod),
                None => (var.vartype, var.vartypmod),
            },
            None if row_expr(x)? => (nodes_core::expr_type(x), nodes_core::expr_typmod(x)),
            None => return Ok(None),
        };
        steps.reverse();
        let mut range = match int_range(xtype) {
            Some((lo, hi)) => Range::Int(lo, hi),
            None if xtype == NUMERICOID && xtypmod >= 4 => {
                let t = xtypmod - 4;
                let (p, s) = ((t >> 16) & 0xffff, ((t & 0x7ff) ^ 1024) - 1024);
                Range::Num { digits: Some((p - s).max(0)), scale: Some(s.max(0)) }
            }
            None if xtype == NUMERICOID => Range::Num { digits: None, scale: None },
            None => return Ok(None),
        };
        let mut chain = Chain {
            x,
            xtype,
            steps: Vec::new(),
            raising: false,
            raise_types: Vec::new(),
            dir: Some(1),
            row_magnitude: int_range(xtype).map_or(0, |(lo, hi)| magnitude(Range::Int(lo, hi))),
            tfinal: xtype,
        };
        for step in &steps {
            match *step {
                Step::Cast { restype, .. } => {
                    if restype == NUMERICOID {
                        range = to_num(range);
                    }
                    chain.tfinal = restype;
                }
                Step::Arith { kind, restype, c, c_first, .. } => {
                    let Some((cr, csign)) = const_range(c) else { return Ok(None) };
                    let (next, raises) = match (range, cr, int_range(restype)) {
                        (Range::Int(lo, hi), Range::Int(clo, chi), Some((tlo, thi))) => {
                            let r = match (kind, c_first) {
                                (Kind::Add, _) => lo.checked_add(clo).zip(hi.checked_add(chi)),
                                (Kind::Sub, false) => lo.checked_sub(chi).zip(hi.checked_sub(clo)),
                                (Kind::Sub, true) => clo.checked_sub(hi).zip(chi.checked_sub(lo)),
                                (Kind::Mul, _) => {
                                    let p = [lo.checked_mul(clo), lo.checked_mul(chi), hi.checked_mul(clo), hi.checked_mul(chi)];
                                    p.iter().all(Option::is_some).then(|| {
                                        let p = p.map(Option::unwrap);
                                        (*p.iter().min().unwrap(), *p.iter().max().unwrap())
                                    })
                                }
                            };
                            match r {
                                Some((rlo, rhi)) if rlo >= tlo && rhi <= thi => (Range::Int(rlo, rhi), false),
                                Some((rlo, rhi)) => (Range::Int(rlo.max(tlo), rhi.min(thi)), true),
                                None => (Range::Int(tlo, thi), true),
                            }
                        }
                        (a, cr, None) if restype == NUMERICOID => {
                            let (Range::Num { digits: da, scale: sa }, Range::Num { digits: dc, scale: sc }) = (to_num(a), to_num(cr))
                            else {
                                unreachable!("to_num gives Num")
                            };
                            let (digits, scale) = if kind == Kind::Mul {
                                let exact = matches!((sa, sc), (Some(0), _) | (_, Some(0)))
                                    || sa.zip(sc).is_some_and(|(a, b)| a + b <= NUMERIC_DSCALE_MAX);
                                if !exact {
                                    return Ok(None);
                                }
                                (da.zip(dc).map(|(a, b)| a + b), sa.zip(sc).map(|(a, b)| a + b))
                            } else {
                                (da.zip(dc).map(|(a, b)| a.max(b) + 1), sa.zip(sc).map(|(a, b)| a.max(b)))
                            };
                            let raises = digits.is_none_or(|d| d > NUMERIC_MAX_DIGITS);
                            (Range::Num { digits, scale }, raises)
                        }
                        _ => return Ok(None),
                    };
                    if raises {
                        chain.raising = true;
                        if !chain.raise_types.contains(&restype) {
                            chain.raise_types.push(restype);
                        }
                    }
                    if matches!(restype, INT2OID | INT4OID) {
                        if kind != Kind::Mul {
                            chain.row_magnitude = chain.row_magnitude.max(magnitude(cr));
                        }
                        chain.row_magnitude = chain.row_magnitude.max(magnitude(next));
                    }
                    chain.dir = match (kind, c_first) {
                        (Kind::Sub, true) => chain.dir.map(|d| -d),
                        (Kind::Mul, _) => chain.dir.zip(csign).map(|(d, s)| d * s),
                        _ => chain.dir,
                    };
                    chain.tfinal = restype;
                    range = next;
                }
            }
        }
        chain.steps = steps;
        Ok(Some(chain))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Agg {
    Sum,
    Avg,
    Count,
    Min,
    Max,
}

fn agg_kind(aggfnoid: Oid) -> Option<Agg> {
    Some(match aggfnoid {
        SUM_INT2 | SUM_INT4 | SUM_INT8 | SUM_NUMERIC => Agg::Sum,
        AVG_INT2 | AVG_INT4 | AVG_INT8 | AVG_NUMERIC => Agg::Avg,
        COUNT_ANY => Agg::Count,
        MIN_INT2 | MIN_INT4 | MIN_INT8 | MIN_NUMERIC => Agg::Min,
        MAX_INT2 | MAX_INT4 | MAX_INT8 | MAX_NUMERIC => Agg::Max,
        _ => return None,
    })
}

fn base_fns(t: Oid) -> (Oid, Oid, Oid) {
    match t {
        INT2OID => (SUM_INT2, MIN_INT2, MAX_INT2),
        INT4OID => (SUM_INT4, MIN_INT4, MAX_INT4),
        INT8OID => (SUM_INT8, MIN_INT8, MAX_INT8),
        _ => (SUM_NUMERIC, MIN_NUMERIC, MAX_NUMERIC),
    }
}

fn sum_type(t: Oid) -> Oid {
    if matches!(t, INT2OID | INT4OID) { INT8OID } else { NUMERICOID }
}

struct Member<'mcx> {
    aggref: Node<'mcx>,
    agg: Agg,
    chain: Chain<'mcx>,
    take: bool,
    guard: bool,
    filter: Option<Node<'mcx>>,
    row_cost: Option<f64>,
}

fn member<'mcx>(node: Node<'mcx>) -> PgResult<Option<Member<'mcx>>> {
    let a = node.as_aggref().expect("Aggref");
    let Some(agg) = agg_kind(a.aggfnoid) else { return Ok(None) };
    if a.aggstar
        || !a.aggdistinct.is_nil()
        || !a.aggorder.is_nil()
        || !a.aggdirectargs.is_nil()
        || a.agglevelsup != 0
        || a.aggkind != types_nodes::primnodes::AGGKIND_NORMAL
        || a.args.len() != 1
    {
        return Ok(None);
    }
    // PSP2-06: each base aggregate carries the member's FILTER, so it runs once per base instead of once per member
    // (PostgreSQL evaluates it per transition); any FILTER but a `filter_quiet` one stays as written.
    if let Some(f) = a.aggfilter {
        if !filter_quiet(f)? {
            return Ok(None);
        }
    }
    let arg = a.args.nth(0).as_target_entry().expect("agg arg is a TLE").expr;
    let Some(chain) = Chain::of(arg)? else { return Ok(None) };
    let numeric_x = chain.xtype == NUMERICOID;
    let ok = match agg {
        Agg::Sum => a.aggtype == sum_type(chain.tfinal),
        Agg::Avg => a.aggtype == NUMERICOID,
        Agg::Count => chain.steps.iter().all(|s| !matches!(s, Step::Arith { c, .. } if c.node_tag() == NodeTag::T_Param)),
        Agg::Min | Agg::Max => a.aggtype == chain.tfinal && chain.dir.is_some_and(|d| d > 0 || !numeric_x),
    };
    if !ok || (chain.raising && chain.raise_types.contains(&NUMERICOID)) {
        return Ok(None);
    }
    Ok(Some(Member { aggref: node, agg, chain, take: false, guard: false, filter: a.aggfilter, row_cost: None }))
}

struct AggrefWalker<'mcx>(Vec<Node<'mcx>>);
impl<'mcx> nodes_core::NodeWalker<'mcx> for AggrefWalker<'mcx> {
    fn visit(&mut self, node: Node<'mcx>) -> PgResult<bool> {
        if node.node_tag() == NodeTag::T_Aggref {
            self.0.push(node);
            return Ok(false);
        }
        nodes_core::expression_tree_walker(node, self)
    }
}

fn aggrefs_in(node: Node<'_>) -> PgResult<Vec<Node<'_>>> {
    let mut w = AggrefWalker(Vec::new());
    nodes_core::NodeWalker::visit(&mut w, node)?;
    Ok(w.0)
}

pub fn deferrable(run: &PlannerRun<'_>) -> PgResult<bool> {
    let parse = run.parse();
    if parse.groupClause.is_nil() || !parse.groupingSets.is_nil() || parse.hasWindowFuncs || parse.hasTargetSRFs {
        return Ok(false);
    }
    let ordered = |n: &Node<'_>| n.as_aggref().is_some_and(|a| !a.aggorder.is_nil() || !a.aggdistinct.is_nil());
    if let Some(h) = parse.havingQual {
        if aggrefs_in(h)?.iter().any(ordered) {
            return Ok(false);
        }
    }
    for t in run.processed_tlist().iter() {
        let tle = t.as_target_entry().expect("tlist entry");
        let aggs = aggrefs_in(tle.expr)?;
        if aggs.iter().any(ordered) || (tle.ressortgroupref != 0 && !aggs.is_empty()) {
            return Ok(false);
        }
    }
    Ok(true)
}

pub fn rewrite_shifted_sums<'mcx>(run: &mut PlannerRun<'mcx>, at: Option<Deferred>) -> PgResult<Option<Rewrite<'mcx>>> {
    let parse = run.parse();
    if !parse.groupingSets.is_nil() {
        return Ok(None);
    }
    let above = parse.hasWindowFuncs || parse.hasTargetSRFs;
    let having = match parse.havingQual {
        Some(h) => aggrefs_in(h)?,
        None => Vec::new(),
    };
    let mut all = having.clone();
    for t in run.processed_tlist().iter() {
        all.extend(aggrefs_in(t.as_target_entry().expect("tlist entry").expr)?);
    }
    let mut members = Vec::new();
    for &n in all.iter().skip(having.len()) {
        if !having.iter().any(|h| types_nodes::equal(*h, n)) {
            if let Some(m) = member(n)? {
                members.push(m);
            }
        }
    }
    if members.is_empty() {
        return Ok(None);
    }
    if at.is_some() {
        fires::fire("agg_algebra.affine.deferred");
    }
    let minmax = |n: &Node<'_>| matches!(agg_kind(n.as_aggref().unwrap().aggfnoid), Some(Agg::Min | Agg::Max));
    let all_minmax = parse.groupClause.is_nil() && all.iter().all(minmax);
    if all_minmax && planagg_may_index(run, &members)? {
        return Ok(None); // planagg's index path (regress inherit.sql `min(1-id)`); it never takes a grouped query
    }
    let rel = relations(run)?;
    let g_ok = order_safe(run, &members, rel.as_ref())?;
    // One relation without children feeds at most ROW_CAP rows (the bare int8 form's cap).
    let capped = rel.as_ref().is_some_and(|r| r.single && !r.children);
    let plain_one = parse.groupClause.is_nil() && rel.as_ref().is_some_and(|r| r.single);
    let cse = g_ok && !above && plain_one && members.iter().filter(|m| cse_shape(m, capped)).count() >= 2;
    let others: Vec<Node<'mcx>> =
        all.iter().copied().filter(|n| !members.iter().any(|m| m.aggref.ptr_eq(*n))).collect();
    for m in members.iter_mut() {
        m.take = !m.chain.raising || (g_ok && !above);
        m.guard = m.chain.raising || (cse && cse_shape(m, capped));
    }
    if let Some(Deferred { rows, width, .. }) = at {
        if !members.iter().any(|m| m.take) || (!forced() && !bound(run, &mut members, rows, width)?) {
            return Ok(None);
        }
    }
    let (per_group, groups) = groups_per_row(run)?;
    let price = profitable(run, &mut members, &others, cse.then_some(capped), capped, per_group)?;
    if !members.iter().any(|m| m.take) {
        return Ok(None);
    }
    let raising = members.iter().any(|m| m.take && m.chain.raising);
    let mut built = Vec::new();
    let mut entries = Vec::new();
    let mut nodes = 0usize;
    let mut sorted = false;
    for tle_node in run.processed_tlist() {
        let tle = tle_node.as_target_entry().expect("tlist entry");
        if let Some(new) = rewrite(run, tle.expr, &members, capped, &mut built)? {
            nodes += node_count(new)?.saturating_sub(node_count(tle.expr)?);
            sorted |= tle.ressortgroupref != 0;
            entries.push((tle_node, tle.expr));
            // SAFETY: the planner exclusively owns the parse tree before it is sealed; no reference derived from
            // this entry is live.
            unsafe { tle_node.with_mut::<TargetEntry, _>(|t| t.expr = new) }.expect("TargetEntry");
        }
    }
    let marks_from = run.gravity_finals.as_ref().map(|l| l.len());
    mark(run, &built);
    let having = having_first(run, &members)?;
    let undoable = !cse && !all_minmax && !above && !sorted;
    Ok(Some(Rewrite { entries, having, marks_from, price, nodes: nodes as f64, groups, raising, cse, undoable, deferred: at }))
}

fn bound<'mcx>(run: &mut PlannerRun<'mcx>, members: &mut [Member<'mcx>], rows: f64, width: f64) -> PgResult<bool> {
    let mut orig = 0.0;
    for m in members.iter_mut().filter(|m| m.take) {
        orig += member_cost(run, m)?;
    }
    Ok(rows / width.max(1.0) * orig * UNIT.ns > FIXED.ns)
}

fn member_cost<'mcx>(run: &mut PlannerRun<'mcx>, m: &mut Member<'mcx>) -> PgResult<f64> {
    if let Some(c) = m.row_cost {
        return Ok(c);
    }
    let a = m.aggref.as_aggref().expect("Aggref");
    let c = agg_row_cost(run, a.aggfnoid, a.args.nth(0).as_target_entry().expect("aggregate argument").expr, 0)?;
    m.row_cost = Some(c);
    Ok(c)
}

pub struct Rewrite<'mcx> {
    entries: Vec<(Node<'mcx>, Node<'mcx>)>,
    /// the HAVING List `having_first` wrapped and its quals as written
    having: Option<(Node<'mcx>, NodeList<'mcx>)>,
    marks_from: Option<usize>,
    /// the taken families' PostgreSQL costs: per input row and per group, as written and rewritten
    price: Price,
    nodes: f64,
    groups: f64,
    raising: bool,
    cse: bool,
    undoable: bool,
    deferred: Option<Deferred>,
}

/// A deferrable statement's rewrite, built after query_planner at the scan / join rel's rows and Gravity's width.
/// `numbered`: preprocess_aggrefs numbered the list as written before query_planner (a semi join's unique path reads
/// the transition count there); else nothing is numbered yet.
#[derive(Clone, Copy)]
pub struct Deferred {
    pub rows: f64,
    pub width: f64,
    pub numbered: bool,
}

/// The taken families' costs in PostgreSQL's units: per input row (arguments + transitions) and per group (finals).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Price {
    orig: f64,
    new: f64,
    orig_final: f64,
    new_final: f64,
}

const MACHINE: &str = "box 72: c8g.16xlarge, Graviton 4 (Neoverse V2), 64 vCPU";
/// Tables of their own (never the judged cells: p2_nm3, alg-w*, c0b, the census): ft (400 000 rows, WHERE r < 400 ..
/// 400 000) and fs400 .. fs50000 whole; 3 / 10 / 30 one-shift int4 sums x 1 / 100 groups; W0 and W8; each statement as
/// written and rewritten on one binary, --perf 9 medians over 2 / 3 rounds; `least_squares` of the saving against the
/// row term over the row-dominated cells (W0 alone 758.6, W8 alone 757.8 ns a unit), then of the rest against the nodes
/// over the cells whose row term is small. The rule agrees with the measured sign on 90 / 102 cells.
const CORPUS: &str = "algebra fit0c + fit0c2 (lanes/algebra/work/fit0c)";
/// One anchor (the check anchor: a row's arithmetic in the executor), so the rule holds on every host.
const fn fitted(ns: f64) -> Fitted {
    Fitted { ns, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Check }
}
/// A PostgreSQL cost unit of per-row work the rewrite saves, per participant.
const UNIT: Fitted = fitted(759.8);
const NODE: Fitted = fitted(353.1);
const FIXED: Fitted = fitted(28_909.0);

pub(crate) fn pays(price: Price, nodes: f64, rows: f64, groups: f64, width: f64) -> bool {
    let saved = (rows * (price.orig - price.new) - groups * (price.new_final - price.orig_final)) / width.max(1.0);
    saved * UNIT.ns > FIXED.ns + nodes * NODE.ns
}

pub fn heap_width(run: &PlannerRun<'_>, rel: types_pathnodes::RelId) -> f64 {
    let granted = i64::from(crate::gravity_paths::gravity_width(run));
    let mut blocks = 0;
    for r in crate::relnode::relids_members(&run.root.rel(rel).relids) {
        if let Some(&Some(b)) = run.root.simple_rel_array.get(r as usize) {
            blocks = blocks.max(run.root.rel(b).pages);
        }
    }
    let w = (i64::from(blocks) / 64).min(granted);
    if w >= 2 { w as f64 } else { 1.0 }
}

#[cfg(test)]
thread_local! {
    pub(crate) static FORCED_IN_TEST: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

fn forced() -> bool {
    #[cfg(test)]
    if FORCED_IN_TEST.with(|f| f.get()) {
        return true;
    }
    guc_seams::get_config_option_missing_ok::is_installed()
        && guc_seams::get_config_option_missing_ok::call("pgrust.gravity_affine")
            .ok()
            .flatten()
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("force"))
}

/// Settles the rewrite after query_planner (D2862 0c): it stays when it pays at the scan / join rel's row estimate
/// (`pays`); else every rewritten entry gets its expression as written back and the rewrite's marks go, so the plan is
/// the one never rewriting makes. The aggregates are numbered (`renumber`) unless the numbering already read the list
/// settle leaves: a kept rewrite built before query_planner, or an undone deferred one numbered as written before
/// query_planner (a semi join).
pub fn settle<'mcx>(run: &mut PlannerRun<'mcx>, rw: Rewrite<'mcx>, rows: f64, width: f64) -> PgResult<()> {
    let undoable = rw.undoable && run.root.numOrderedAggs == 0;
    if !undoable || forced() || pays(rw.price, rw.nodes, rows, rw.groups.min(rows), width) {
        fires::fire("agg_algebra.affine"); // shapecheck fire signal (REGISTRY NEW-decompose-planner-1)
        if rw.raising {
            fires::fire("agg_algebra.affine.raising");
        }
        if rw.cse {
            fires::fire("agg_algebra.cse");
        }
        return if rw.deferred.is_some() { renumber(run) } else { Ok(()) };
    }
    fires::fire("agg_algebra.affine.undone");
    for (tle_node, expr) in rw.entries {
        // SAFETY: as in `rewrite_shifted_sums`: the planner owns the target list until it is sealed.
        unsafe { tle_node.with_mut::<TargetEntry, _>(|t| t.expr = expr) }.expect("TargetEntry");
    }
    if let Some((h, quals)) = rw.having {
        // SAFETY: as in `having_first`.
        unsafe { h.with_mut::<NodeList<'mcx>, _>(|l| *l = quals) }.expect("HAVING is a List");
    }
    match rw.marks_from {
        None => run.gravity_finals = None,
        Some(n) => {
            if let Some(l) = run.gravity_finals.as_mut() {
                l.truncate(n);
            }
        }
    }
    if rw.deferred.is_some_and(|d| d.numbered) {
        return Ok(());
    }
    renumber(run)
}

pub fn semi_join(parse: &types_nodes::parsenodes::Query<'_>) -> bool {
    fn walk(n: Node<'_>) -> bool {
        if let Some(f) = n.as_from_expr() {
            return f.fromlist.iter().any(walk);
        }
        n.as_join_expr().is_some_and(|j| j.jointype == types_nodes::JoinType::JOIN_SEMI || walk(j.larg) || walk(j.rarg))
    }
    parse.jointree.is_some_and(|f| f.fromlist.iter().any(walk))
}

pub(crate) fn renumber(run: &mut PlannerRun<'_>) -> PgResult<()> {
    let root = &mut run.root;
    root.agginfos.clear();
    root.aggtransinfos.clear();
    root.numOrderedAggs = 0;
    root.numOrderByAggs = 0;
    root.hasNonPartialAggs = false;
    root.hasNonSerialAggs = false;
    crate::prepagg::preprocess_aggrefs(run, run.processed_tlist())?;
    if let Some(having) = run.parse().havingQual {
        crate::prepagg::preprocess_aggrefs_node(run, having)?;
    }
    Ok(())
}

fn node_count(n: Node<'_>) -> PgResult<usize> {
    struct Count(usize);
    impl<'mcx> nodes_core::NodeWalker<'mcx> for Count {
        fn visit(&mut self, n: Node<'mcx>) -> PgResult<bool> {
            self.0 += 1;
            nodes_core::expression_tree_walker(n, self)
        }
    }
    let mut c = Count(0);
    nodes_core::NodeWalker::visit(&mut c, n)?;
    Ok(c.0)
}

/// PSP2-03: under a HAVING, a group it drops never reaches the projection, where PostgreSQL ran the group's transitions
/// and raised at its row. So the HAVING becomes `CASE WHEN g1 AND .. AND gn THEN <its quals> ELSE false END`: each gi is
/// a taken raising member's guard (`guarded`, true or raising), run for every group before the quals can drop it. A
/// CASE keeps that order where a reordering of an AND's arguments or of the qual list would not.
fn having_first<'mcx>(run: &PlannerRun<'mcx>, members: &[Member<'mcx>]) -> PgResult<Option<(Node<'mcx>, NodeList<'mcx>)>> {
    let mcx = run.mcx;
    let Some(h) = run.parse().havingQual else { return Ok(None) };
    let mut guards = NodeList::nil();
    for (i, m) in members.iter().enumerate() {
        if !m.take || !m.chain.raising || members[..i].iter().any(|o| o.take && types_nodes::equal(o.aggref, m.aggref)) {
            continue;
        }
        let a = m.aggref.as_aggref().expect("Aggref");
        let (_, min_fn, max_fn) = base_fns(m.chain.xtype);
        let edge = |f: Oid| agg_of(mcx, f, m.chain.xtype, m.chain.x, m.chain.xtype, a.location, m.filter);
        guards.lappend(mcx, guarded(mcx, &m.chain, edge(min_fn)?, edge(max_fn)?, bool_const(mcx, true)?, BOOLOID, bool_const(mcx, true)?)?)?;
    }
    if guards.is_nil() {
        return Ok(None);
    }
    let quals = h.as_list().expect("HAVING is an implicit-AND List");
    let (mut args, mut orig) = (NodeList::nil(), NodeList::nil());
    for q in quals.iter() {
        args.lappend(mcx, q)?;
        orig.lappend(mcx, q)?;
    }
    let all = |l: NodeList<'mcx>| -> PgResult<Node<'mcx>> {
        if l.len() == 1 {
            Ok(l.nth(0))
        } else {
            Node::mk(mcx, BoolExpr { boolop: BoolExprType::AND_EXPR, args: l, location: -1 })
        }
    };
    let wrapped = case_of(mcx, BOOLOID, all(guards)?, all(args)?, bool_const(mcx, false)?)?;
    let one = NodeList::make1(mcx, wrapped)?;
    // SAFETY: the planner exclusively owns the parse tree before it is sealed; `quals` is not used past this point and no
    // other reference derived from the HAVING list is live.
    unsafe { h.with_mut::<NodeList<'mcx>, _>(|l| *l = one) }.expect("HAVING is a List");
    Ok(Some((h, orig)))
}

fn bool_const(mcx: mcx::Mcx<'_>, v: bool) -> PgResult<Node<'_>> {
    Node::mk_const(mcx, BOOLOID, -1, 0, 1, datum::Datum::from_bool(v), false, true)
}

fn mark<'mcx>(run: &mut PlannerRun<'mcx>, built: &[(Node<'mcx>, [Node<'mcx>; 4])]) {
    if built.is_empty() {
        return;
    }
    let mcx = run.mcx;
    let list = run.gravity_finals.get_or_insert_with(|| mcx::PgVec::new_in(mcx));
    for &b in built {
        list.push(b);
    }
}

pub fn finals_of<'mcx>(run: &PlannerRun<'mcx>, tlist: &NodeList<'mcx>) -> PgResult<&'mcx [GravityFinal]> {
    let Some(built) = run.gravity_finals.as_ref() else { return Ok(&[]) };
    let mut out = mcx::PgVec::new_in(run.mcx);
    for t in tlist.iter() {
        let te = t.as_target_entry().expect("tlist entry");
        let Some((_, a)) = built.iter().find(|(e, _)| e.ptr_eq(te.expr)) else { continue };
        let no = a.map(|n| n.as_aggref().map_or(-1, |r| r.aggno));
        if no.iter().all(|&n| n >= 0) {
            out.push(GravityFinal { resno: te.resno, min: no[0], max: no[1], sum: no[2], count: no[3] });
        }
    }
    mcx::vec_borrow_in(run.mcx, out)
}

fn rewrite<'mcx>(
    run: &PlannerRun<'mcx>,
    node: Node<'mcx>,
    members: &[Member<'mcx>],
    capped: bool,
    built: &mut Vec<(Node<'mcx>, [Node<'mcx>; 4])>,
) -> PgResult<Option<Node<'mcx>>> {
    if node.node_tag() == NodeTag::T_Aggref {
        return match members.iter().find(|m| m.take && m.aggref.ptr_eq(node)) {
            Some(m) => {
                let (new, aggs) = build(run.mcx, m, capped)?;
                if let Some(aggs) = aggs {
                    built.push((new, aggs));
                }
                Ok(Some(new))
            }
            None => Ok(None),
        };
    }
    nodes_core::expression_tree_mutator(run.mcx, node, &mut |n| rewrite(run, n, members, capped, &mut *built))
}

/// The plain relations the query scans (through pulled-up subqueries: PostgreSQL 18's GROUP RTE and the inert
/// subquery RTEs stay in the rtable, outside the join tree; partitioned tables and inheritance parents whose every member
/// is a table; a CTE scan whose query is one more such scan; a UNION ALL appendrel's members), whether one of them has
/// inheritance children, whether there is exactly one, and every qual and per-row expression on the way: the FromExprs'
/// and JoinExprs' quals, the relations' row-security quals, a CTE query's targets, an appendrel's translated columns.
/// `None` when the join tree reads anything else (a subquery kept as one, a function, VALUES, a foreign table).
struct Rel<'mcx> {
    children: bool,
    single: bool,
    quals: Vec<Node<'mcx>>,
}

fn relations<'mcx>(run: &PlannerRun<'mcx>) -> PgResult<Option<Rel<'mcx>>> {
    let parse = run.parse();
    let Some(from) = parse.jointree else { return Ok(None) };
    let mut rel = Rel { children: false, single: false, quals: Vec::new() };
    let mut count = 0;
    if !walk_from(run, parse, true, from, &mut rel, &mut count)? || count == 0 {
        return Ok(None);
    }
    rel.single = count == 1;
    Ok(Some(rel))
}

fn walk_from<'mcx>(
    run: &PlannerRun<'mcx>,
    q: &'mcx Query<'mcx>,
    top: bool,
    f: &FromExpr<'mcx>,
    rel: &mut Rel<'mcx>,
    count: &mut usize,
) -> PgResult<bool> {
    rel.quals.extend(f.quals);
    for item in f.fromlist.iter() {
        if !walk(run, q, top, item, rel, count)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn walk<'mcx>(
    run: &PlannerRun<'mcx>,
    q: &'mcx Query<'mcx>,
    top: bool,
    n: Node<'mcx>,
    rel: &mut Rel<'mcx>,
    count: &mut usize,
) -> PgResult<bool> {
    if let Some(f) = n.as_from_expr() {
        return walk_from(run, q, top, f, rel, count);
    }
    if let Some(j) = n.as_join_expr() {
        rel.quals.extend(j.quals);
        return Ok(walk(run, q, top, j.larg, rel, count)? && walk(run, q, top, j.rarg, rel, count)?);
    }
    let Some(rtr) = n.as_range_tbl_ref() else { return Ok(false) };
    let rte = q.rtable.nth(rtr.rtindex as usize - 1).as_range_tbl_entry().expect("rtable cell is a RangeTblEntry");
    match rte.rtekind {
        RTEKind::RTE_RELATION => relation(run, rte, rel, count),
        // PSP2-09: a CTE scan reads the CTE's rows as its query makes them, so that query must raise on no row.
        RTEKind::RTE_CTE if rte.ctelevelsup == 0 && !rte.self_reference => {
            let cte = q.cteList.iter().filter_map(|c| c.as_common_table_expr()).find(|c| c.ctename == rte.ctename);
            match cte.filter(|c| !c.cterecursive).and_then(|c| c.ctequery).and_then(|n| n.as_query()) {
                Some(cq) => scan_query(run, cq, rel, count),
                None => Ok(false),
            }
        }
        RTEKind::RTE_SUBQUERY if top && rte.inh => union_members(run, rtr.rtindex, rel, count),
        _ => Ok(false),
    }
}

/// PSP2-07 / PSP2-08: a partitioned table's or an inheritance parent's scan reads each member with the parent's column
/// types and typmods (MergeAttributes) and its quals translated, and evaluates nothing more per row unless a member is a
/// foreign table, whose scan can raise (a remote error, a conversion). The members are read from pg_inherits unlocked:
/// PostgreSQL locks only the partitions it keeps.
fn relation<'mcx>(run: &PlannerRun<'mcx>, rte: &RangeTblEntry<'mcx>, rel: &mut Rel<'mcx>, count: &mut usize) -> PgResult<bool> {
    let plain = |k: u8| matches!(k, RELKIND_RELATION | RELKIND_PARTITIONED_TABLE);
    if !plain(rte.relkind) {
        return Ok(false);
    }
    if rte.inh {
        for &c in pg_inherits::find_all_inheritors(run.mcx, rte.relid, NoLock)?.iter() {
            if !plain(lsyscache::get_rel_relkind(c)? as u8) {
                return Ok(false);
            }
        }
    }
    rel.quals.extend(rte.securityQuals.iter());
    let r = table::table_open(run.mcx, rte.relid, NoLock)?;
    rel.children |= r.rd_rel.relhassubclass;
    table::table_close(r, NoLock)?;
    *count += 1;
    Ok(true)
}

/// A CTE's (or an appendrel arm's) query that raises on no row as far as `order_safe` can tell: a plain SELECT over
/// relations as `walk` reads them, with no aggregate, window function, set-returning target, grouping, DISTINCT, ORDER BY,
/// LIMIT, locking clause, sublink or WITH of its own; its targets and quals join `rel.quals`, which order_safe reads.
fn scan_query<'mcx>(run: &PlannerRun<'mcx>, q: &'mcx Query<'mcx>, rel: &mut Rel<'mcx>, count: &mut usize) -> PgResult<bool> {
    let plain = q.commandType == types_nodes::CmdType::CMD_SELECT
        && q.setOperations.is_none()
        && !(q.hasAggs || q.hasWindowFuncs || q.hasTargetSRFs || q.hasSubLinks || q.hasModifyingCTE)
        && q.groupClause.is_nil()
        && q.groupingSets.is_nil()
        && q.havingQual.is_none()
        && q.distinctClause.is_nil()
        && q.sortClause.is_nil()
        && q.limitCount.is_none()
        && q.limitOffset.is_none()
        && q.rowMarks.is_nil()
        && q.cteList.is_nil();
    let Some(from) = q.jointree.filter(|_| plain) else { return Ok(false) };
    rel.quals.extend(q.targetList.iter());
    walk_from(run, q, false, from, rel, count)
}

fn union_members<'mcx>(run: &PlannerRun<'mcx>, rti: i32, rel: &mut Rel<'mcx>, count: &mut usize) -> PgResult<bool> {
    let parse = run.parse();
    let mut any = false;
    for ai in run.root.append_rel_list.iter().filter(|a| i64::from(a.parent_relid) == i64::from(rti)) {
        for &t in ai.translated_vars.iter() {
            if t == types_pathnodes::NodeId::default() {
                return Ok(false);
            }
            rel.quals.push(*run.root.expr_node(t));
        }
        let rte = parse.rtable.nth(ai.child_relid as usize - 1).as_range_tbl_entry().expect("rtable cell is a RangeTblEntry");
        let ok = match (rte.rtekind, rte.subquery) {
            (RTEKind::RTE_RELATION, _) => relation(run, rte, rel, count)?,
            (RTEKind::RTE_SUBQUERY, Some(sq)) => scan_query(run, sq, rel, count)?,
            _ => false,
        };
        if !ok {
            return Ok(false);
        }
        any = true;
    }
    Ok(any)
}

/// PSP2-14: whether planagg (preprocess_minmax_aggregates, run after this rewrite) could answer an all-min / max
/// aggregation from an index, where the rewrite would change PostgreSQL's plan: one relation (no join, no CTE, no window
/// function), and an index key that reads a member's column or is an expression. planagg takes only presorted paths
/// (build_minmax_path), so with no such index PostgreSQL aggregates the scan and the rewrite only saves work. A UNION
/// ALL appendrel, or a relation with inheritance children (their indexes feed a MergeAppend), counts as indexed.
fn planagg_may_index(run: &PlannerRun<'_>, members: &[Member<'_>]) -> PgResult<bool> {
    let parse = run.parse();
    if parse.hasWindowFuncs || !parse.cteList.is_nil() {
        return Ok(false);
    }
    let Some(mut from) = parse.jointree else { return Ok(false) };
    let rtr = loop {
        if from.fromlist.len() != 1 {
            return Ok(false);
        }
        let item = from.fromlist.nth(0);
        match (item.as_from_expr(), item.as_range_tbl_ref()) {
            (Some(f), _) => from = f,
            (None, Some(r)) => break r,
            (None, None) => return Ok(false),
        }
    };
    let rte = run.parse().rtable.nth(rtr.rtindex as usize - 1).as_range_tbl_entry().expect("rtable cell is a RangeTblEntry");
    match rte.rtekind {
        types_nodes::parsenodes::RTEKind::RTE_RELATION => {}
        types_nodes::parsenodes::RTEKind::RTE_SUBQUERY => return Ok(rte.inh),
        _ => return Ok(false),
    }
    let r = table::table_open(run.mcx, rte.relid, NoLock)?;
    let (children, indexed) = (r.rd_rel.relhassubclass, r.rd_rel.relhasindex);
    table::table_close(r, NoLock)?;
    if children || !indexed {
        return Ok(children);
    }
    let cols: Vec<i16> = members.iter().filter_map(|m| column(m.chain.x)).map(|v| v.varattno).collect();
    for &oid in relcache_seams::relation_get_index_list::call(run.mcx, rte.relid)?.iter() {
        let ix = indexam::index_open(run.mcx, oid, rte.rellockmode)?;
        let ind = ix.rd_index.as_ref().expect("index relation carries rd_index");
        let reads = (0..ind.indnkeyatts as usize).any(|i| ind.indkey[i] == 0 || cols.contains(&ind.indkey[i]));
        indexam::index_close(ix, NoLock)?;
        if reads {
            return Ok(true);
        }
    }
    Ok(false)
}

/// D298 / D1830 (the module's "Errors"): a raising member's raise moves to the projection, so it is rewritten only
/// when that cannot change whether or which error the statement raises. Whether: the rows reaching each aggregate are
/// PostgreSQL's, and every group's projection is evaluated when the group's row is read, so with no HAVING (which
/// drops groups whose transitions PostgreSQL ran) a group raises iff one of its rows does. A reader that stops early
/// (LIMIT, a cursor, a semi join) reads the groups that PostgreSQL's sorted GroupAggregate would have run the
/// transitions of, a legal PostgreSQL evaluation. Which: nothing else on the way can raise (quals, other targets:
/// `quiet`, grouping keys included), and one result type covers every raising operator (one message).
fn order_safe(run: &PlannerRun<'_>, members: &[Member<'_>], rel: Option<&Rel<'_>>) -> PgResult<bool> {
    let parse = run.parse();
    let Some(rel) = rel else { return Ok(false) };
    if let Some(h) = parse.havingQual {
        if h.as_list().is_none() || !quiet(h)? {
            return Ok(false);
        }
    }
    for &q in &rel.quals {
        if !quiet(q)? {
            return Ok(false);
        }
    }
    let mut types = Vec::new();
    for m in members.iter().filter(|m| m.chain.raising) {
        for &t in &m.chain.raise_types {
            if !types.contains(&t) {
                types.push(t);
            }
        }
    }
    if types.len() > 1 {
        return Ok(false);
    }
    for tle_node in run.processed_tlist() {
        let expr = tle_node.as_target_entry().expect("tlist entry").expr;
        if !members.iter().any(|m| m.aggref.ptr_eq(expr)) && !quiet(expr)? && !same_message(expr, &types)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// PSP2-15: a target (a grouping key such as `g + 1`) that is itself an affine chain of a column raises only its steps'
/// overflow, per row in both forms; when that is the raising members' one type, the statement raises the same error
/// whichever of the two PostgreSQL meets first.
fn same_message(expr: Node<'_>, types: &[Oid]) -> PgResult<bool> {
    Ok(!types.is_empty() && Chain::of(expr)?.is_some_and(|c| c.raise_types.iter().all(|t| types.contains(t))))
}

fn cse_shape(m: &Member<'_>, capped: bool) -> bool {
    let one_shift = matches!(m.chain.steps[..], [Step::Arith { kind: Kind::Add | Kind::Sub, c, .. }] if int_value(c).is_some());
    m.agg == Agg::Sum && m.filter.is_none() && one_shift && matches!(m.chain.tfinal, INT2OID | INT4OID) && capped && under_cap(m.chain.row_magnitude)
}

/// PostgreSQL's per-row cost of one aggregate over `arg` (get_agg_clause_costs: the transition function plus the
/// argument's evaluation), with `cast` (a function applied to `arg`, 0 for none). Priced without §3 #7's pgrc class
/// multipliers (`cost_qual_eval_node_pg`), which read the relations query_planner builds: before it they never apply,
/// so a rewrite decided after query_planner (0d) is priced as one decided before it, and as UNIT / NODE / FIXED were fit.
fn agg_row_cost<'mcx>(run: &mut PlannerRun<'mcx>, aggfnoid: Oid, arg: Node<'mcx>, cast: Oid) -> PgResult<f64> {
    let Some(shape) = syscache_seams::lookup_pg_aggregate_shape::call(aggfnoid)? else {
        return Err(crate::cache_lookup_failed("aggregate", aggfnoid));
    };
    let mut c = crate::costsize::cost_qual_eval_node_pg(&mut *run, arg)?;
    crate::plancat::add_function_cost(shape.aggtransfn, &mut c)?;
    if cast != 0 {
        crate::plancat::add_function_cost(cast, &mut c)?;
    }
    Ok(c.per_tuple)
}

fn agg_final_cost(aggfnoid: Oid) -> PgResult<f64> {
    let Some(shape) = syscache_seams::lookup_pg_aggregate_shape::call(aggfnoid)? else {
        return Err(crate::cache_lookup_failed("aggregate", aggfnoid));
    };
    let mut c = types_pathnodes::QualCost::default();
    if shape.aggfinalfn != 0 {
        crate::plancat::add_function_cost(shape.aggfinalfn, &mut c)?;
    }
    Ok(c.per_tuple)
}

/// Groups per input row: PostgreSQL's estimate_num_groups over its row estimate, read from the catalog because the
/// rewrite runs before query_planner builds its relations. Over one input (a FROM list of one item, `input_rows`): the
/// product of the grouping keys' distinct counts (`key_ndistinct`, DEFAULT_NUM_DISTINCT for a key it cannot read; with
/// two or more keys at most a tenth of the rows, never below the largest count), at most the rows, over the rows. The
/// quals are not applied: their selectivity is not known yet. 0 (the per-row comparison alone) when the rows are
/// unknown: a join or another input, a relation never analyzed or vacuumed, or an empty one. Beside it the groups
/// themselves, which `settle` prices at most the scan's rows at: PostgreSQL's get_number_of_groups reads a key over a
/// unique column (`id % 1000`) as one group a row.
fn groups_per_row(run: &PlannerRun<'_>) -> PgResult<(f64, f64)> {
    let parse = run.parse();
    let Some(rti) = one_item(parse) else { return Ok((0.0, 0.0)) };
    let Some(rows) = input_rows(run, parse, rti)? else { return Ok((0.0, 0.0)) };
    let (mut groups, mut most, mut keys) = (1.0f64, 1.0f64, 0);
    for sgc in parse.groupClause.iter() {
        let sgc = sgc.as_sort_group_clause().expect("groupClause holds SortGroupClauses");
        let key = crate::pathkeys::get_sortgroupclause_expr(sgc, run.processed_tlist());
        let d = key_ndistinct(run, parse, rti, key)?.unwrap_or(types_pathnodes::DEFAULT_NUM_DISTINCT).min(rows);
        groups *= d;
        most = most.max(d);
        keys += 1;
    }
    if keys > 1 {
        groups = groups.min((rows * 0.1).max(most));
    }
    Ok((groups.min(rows) / rows, groups.min(rows)))
}

fn top(run: &PlannerRun<'_>, q: &Query<'_>) -> bool {
    core::ptr::eq(q as *const Query<'_> as *const (), run.parse() as *const Query<'_> as *const ())
}

fn one_item(q: &Query<'_>) -> Option<i32> {
    let mut from = q.jointree?;
    loop {
        if from.fromlist.len() != 1 {
            return None;
        }
        let item = from.fromlist.nth(0);
        match (item.as_from_expr(), item.as_range_tbl_ref()) {
            (Some(f), _) => from = f,
            (None, Some(r)) => return Some(r.rtindex),
            (None, None) => return None,
        }
    }
}

fn rte_of<'m>(q: &'m Query<'m>, rti: i32) -> &'m RangeTblEntry<'m> {
    q.rtable.nth(rti as usize - 1).as_range_tbl_entry().expect("rtable cell is a RangeTblEntry")
}

fn cte_scan<'m>(q: &'m Query<'m>, rte: &RangeTblEntry<'_>) -> Option<(&'m Query<'m>, i32)> {
    if rte.ctelevelsup != 0 || rte.self_reference {
        return None;
    }
    let cte = q.cteList.iter().filter_map(|c| c.as_common_table_expr()).find(|c| c.ctename == rte.ctename)?;
    let cq = cte.ctequery.filter(|_| !cte.cterecursive)?.as_query()?;
    let keeps = cq.setOperations.is_none()
        && !(cq.hasAggs || cq.hasWindowFuncs || cq.hasTargetSRFs)
        && cq.groupClause.is_nil()
        && cq.groupingSets.is_nil()
        && cq.distinctClause.is_nil()
        && cq.limitCount.is_none()
        && cq.limitOffset.is_none();
    Some((cq, one_item(cq)?)).filter(|_| keeps)
}

/// The rows input `rti` of `q` feeds: a relation's (pg_class.reltuples; under inheritance the sum over its tables, a
/// partitioned table holding none itself), a CTE scan's (`cte_scan`), a UNION ALL appendrel's summed over its members
/// (a table, or an arm kept as a subquery that reads one item). `None` when unknown: another input, a table never
/// analyzed or vacuumed (-1), or no rows to estimate over.
fn input_rows<'m>(run: &PlannerRun<'_>, q: &'m Query<'m>, rti: i32) -> PgResult<Option<f64>> {
    let rte = rte_of(q, rti);
    let rows = match rte.rtekind {
        RTEKind::RTE_RELATION => {
            let tables = if rte.inh {
                ::mcx::stdvec::try_to_vec(&pg_inherits::find_all_inheritors(run.mcx, rte.relid, NoLock)?, "agg_algebra")?
            } else {
                vec![rte.relid]
            };
            let mut rows = 0.0;
            for oid in tables {
                // the range table's own relation is locked (the parser's lock) and opened as before; a member
                // is read unlocked, as `relation` reads them (this rewrite runs before the planner expands and locks
                // the tree, and PostgreSQL never locks a partition it prunes): its relcache entry as committed, taken
                // without relation_open, whose NoLock open asserts a lock this backend holds; a member gone since the
                // listing leaves the rows unknown
                let (kind, tuples) = if oid == rte.relid {
                    let r = table::table_open(run.mcx, oid, NoLock)?;
                    let kt = (r.rd_rel.relkind, r.rd_rel.reltuples);
                    table::table_close(r, NoLock)?;
                    kt
                } else {
                    let Some(r) = relcache_seams::relation_id_get_relation::call(oid)? else { return Ok(None) };
                    (r.rd_rel.relkind, r.rd_rel.reltuples)
                };
                if kind == RELKIND_PARTITIONED_TABLE {
                    continue;
                }
                if tuples < 0.0 {
                    return Ok(None);
                }
                rows += tuples as f64;
            }
            rows
        }
        RTEKind::RTE_CTE => match cte_scan(q, rte) {
            Some((cq, r)) => return input_rows(run, cq, r),
            None => return Ok(None),
        },
        RTEKind::RTE_SUBQUERY if rte.inh && top(run, q) => {
            let mut rows = 0.0;
            for ai in run.root.append_rel_list.iter().filter(|a| i64::from(a.parent_relid) == i64::from(rti)) {
                let child = ai.child_relid as i32;
                let r = match (rte_of(q, child).rtekind, rte_of(q, child).subquery) {
                    (RTEKind::RTE_RELATION, _) => input_rows(run, q, child)?,
                    (RTEKind::RTE_SUBQUERY, Some(sq)) => match one_item(sq) {
                        Some(i) => input_rows(run, sq, i)?,
                        None => None,
                    },
                    _ => None,
                };
                let Some(r) = r else { return Ok(None) };
                rows += r;
            }
            rows
        }
        _ => return Ok(None),
    };
    Ok((rows >= 1.0).then_some(rows))
}

/// A grouping key's distinct count, get_variable_numdistinct's, for a column of input `rti` (a Var under RelabelTypes,
/// or the column of an affine chain, which keeps distinct values distinct): of a relation, its rows when a unique index
/// of that one column has no predicate, else pg_statistic.stadistinct (a negative one a fraction of the rows); of a CTE
/// scan, its query's target's; of a UNION ALL appendrel, the sum of its members' (at most their union's). `None` for
/// any other key, or a column with no statistic.
fn key_ndistinct<'m>(run: &PlannerRun<'_>, q: &'m Query<'m>, rti: i32, key: Node<'_>) -> PgResult<Option<f64>> {
    let mut k = Chain::of(key)?.map_or(key, |c| c.x);
    while let Some(r) = k.as_relabel_type() {
        k = r.arg;
    }
    let Some(v) = k.as_var().filter(|v| v.varlevelsup == 0 && v.varno == rti && v.varattno > 0) else { return Ok(None) };
    let rte = rte_of(q, rti);
    match rte.rtekind {
        RTEKind::RTE_RELATION => {
            let r = table::table_open(run.mcx, rte.relid, NoLock)?;
            let (indexed, children) = (r.rd_rel.relhasindex, r.rd_rel.relhassubclass);
            table::table_close(r, NoLock)?;
            let Some(rows) = input_rows(run, q, rti)? else { return Ok(None) };
            if indexed {
                for &oid in relcache_seams::relation_get_index_list::call(run.mcx, rte.relid)?.iter() {
                    let ix = indexam::index_open(run.mcx, oid, rte.rellockmode)?;
                    let ind = ix.rd_index.as_ref().expect("index relation carries rd_index");
                    let unique =
                        ind.indisunique && ind.indnkeyatts == 1 && ind.indkey[0] == v.varattno && ind.indpred_src.is_none();
                    indexam::index_close(ix, NoLock)?;
                    if unique {
                        return Ok(Some(rows));
                    }
                }
            }
            Ok(match syscache_seams::lookup_pg_statistic_shape::call(rte.relid, v.varattno, rte.inh && children)? {
                Some(s) if s.stadistinct > 0.0 => Some((s.stadistinct as f64).min(rows)),
                Some(s) if s.stadistinct < 0.0 => Some((-(s.stadistinct as f64) * rows).min(rows)),
                _ => None,
            })
        }
        RTEKind::RTE_CTE => {
            let Some((cq, r)) = cte_scan(q, rte) else { return Ok(None) };
            let tle = cq.targetList.iter().filter_map(|t| t.as_target_entry()).find(|t| t.resno == v.varattno);
            match tle {
                Some(t) => key_ndistinct(run, cq, r, t.expr),
                None => Ok(None),
            }
        }
        RTEKind::RTE_SUBQUERY if rte.inh && top(run, q) => {
            let mut sum = 0.0;
            for ai in run.root.append_rel_list.iter().filter(|a| i64::from(a.parent_relid) == i64::from(rti)) {
                let Some(&t) = ai.translated_vars.get(v.varattno as usize - 1) else { return Ok(None) };
                if t == types_pathnodes::NodeId::default() {
                    return Ok(None);
                }
                let (e, child) = (*run.root.expr_node(t), ai.child_relid as i32);
                let d = match (rte_of(q, child).rtekind, rte_of(q, child).subquery) {
                    (RTEKind::RTE_RELATION, _) => key_ndistinct(run, q, child, e)?,
                    (RTEKind::RTE_SUBQUERY, Some(sq)) => {
                        let inner = e.as_var().and_then(|cv| {
                            sq.targetList.iter().filter_map(|t| t.as_target_entry()).find(|t| t.resno == cv.varattno)
                        });
                        match (inner, one_item(sq)) {
                            (Some(t), Some(i)) => key_ndistinct(run, sq, i, t.expr)?,
                            _ => None,
                        }
                    }
                    _ => None,
                };
                let Some(d) = d else { return Ok(None) };
                sum += d;
            }
            Ok(Some(sum))
        }
        _ => Ok(None),
    }
}

fn same_final(a: &Member<'_>, b: &Member<'_>, capped: bool) -> bool {
    let (x, y) = (&a.chain, &b.chain);
    let bare = |c: &Chain<'_>| capped && under_cap(c.row_magnitude);
    let form = |k: Node<'_>, kind: Kind, first: bool| {
        let v = int_value(k);
        let folded = kind == Kind::Sub && !first && v.and_then(i64::checked_neg).is_some();
        let t = k.as_const().map_or_else(|| k.as_param().map_or(0, |p| p.paramtype), |c| c.consttype);
        (k.node_tag(), t, v.is_some(), folded)
    };
    a.agg == b.agg
        && a.guard == b.guard
        && small_shift(x).is_some() == small_shift(y).is_some()
        && x.xtype == y.xtype
        && x.tfinal == y.tfinal
        && x.dir == y.dir
        && bare(x) == bare(y)
        && x.steps.len() == y.steps.len()
        && x.steps.iter().zip(&y.steps).all(|(s, t)| match (*s, *t) {
            (
                Step::Arith { kind: k1, opfuncid: f1, restype: r1, c: c1, c_first: o1, .. },
                Step::Arith { kind: k2, opfuncid: f2, restype: r2, c: c2, c_first: o2, .. },
            ) => k1 == k2 && f1 == f2 && r1 == r2 && o1 == o2 && form(c1, k1, o1) == form(c2, k2, o2),
            (Step::Cast { funcid: f1, restype: r1, format: g1 }, Step::Cast { funcid: f2, restype: r2, format: g2 }) => {
                f1 == f2 && r1 == r2 && g1 == g2
            }
            _ => false,
        })
}

/// A column's members are rewritten only when that lowers PostgreSQL's cost, or when finals.cse answers them (`cse`:
/// finals.cse serves the statement, over a relation `capped` or not; a family with an A70 sum). The cost is
/// cost_agg's: per row the members' arguments and transitions against the base aggregates the query lacks (a guard's
/// min / max included); per group (`per_group` groups per input row, `groups_per_row`) the finals `build` makes,
/// costed as the Agg's projection, and the base aggregates' final functions against the members' own. With one row
/// per group (GROUP BY a unique key) the finals cost more than the transitions save, and the members stay as written.
fn profitable<'mcx>(
    run: &mut PlannerRun<'mcx>,
    members: &mut [Member<'mcx>],
    others: &[Node<'mcx>],
    cse: Option<bool>,
    capped: bool,
    per_group: f64,
) -> PgResult<Price> {
    let mut price = Price::default();
    let same = |a: Option<Node<'_>>, b: Option<Node<'_>>| match (a, b) {
        (Some(a), Some(b)) => types_nodes::equal(a, b),
        (a, b) => a.is_none() && b.is_none(),
    };
    let family = |a: &Member<'_>, b: &Member<'_>| types_nodes::equal(a.chain.x, b.chain.x) && same(a.filter, b.filter);
    for i in 0..members.len() {
        if !members[i].take || (0..i).any(|j| family(&members[j], &members[i])) {
            continue;
        }
        let fam: Vec<usize> = (i..members.len()).filter(|&j| members[j].take && family(&members[i], &members[j])).collect();
        let (mut orig, mut orig_final, mut written) = (0.0, 0.0, 0);
        for (n, &j) in fam.iter().enumerate() {
            if !fam[..n].iter().any(|&k| types_nodes::equal(members[k].aggref, members[j].aggref)) {
                written += 1;
                orig += member_cost(run, &mut members[j])?;
                orig_final += agg_final_cost(members[j].aggref.as_aggref().expect("Aggref").aggfnoid)?;
            }
        }
        let m = &members[i];
        if cse.is_some_and(|capped| fam.iter().any(|&j| cse_shape(&members[j], capped))) {
            continue;
        }
        let (sum_fn, min_fn, max_fn) = base_fns(m.chain.xtype);
        let wants = |a: Agg| fam.iter().any(|&j| members[j].agg == a);
        let edges = wants(Agg::Min) || wants(Agg::Max) || fam.iter().any(|&j| members[j].guard);
        let summed = |wide: bool| {
            fam.iter().any(|&j| {
                let c = &members[j].chain;
                matches!(members[j].agg, Agg::Sum | Agg::Avg)
                    && wide == (matches!(c.xtype, INT2OID | INT4OID) && !matches!(c.tfinal, INT2OID | INT4OID))
            })
        };
        let widen = CASTS.iter().find(|c| c.1 == m.chain.xtype && c.2 == INT8OID).map_or(0, |c| c.0);
        let needed = [
            (sum_fn, 0, summed(false)),
            (SUM_INT8, widen, summed(true)),
            (COUNT_ANY, 0, wants(Agg::Sum) || wants(Agg::Avg) || wants(Agg::Count)),
            (min_fn, 0, edges),
            (max_fn, 0, edges),
        ];
        let present = |f: Oid| {
            others.iter().any(|o| {
                let a = o.as_aggref().unwrap();
                a.aggfnoid == f
                    && a.aggdistinct.is_nil()
                    && same(a.aggfilter, m.filter)
                    && a.args.len() == 1
                    && types_nodes::equal(a.args.nth(0).as_target_entry().unwrap().expr, m.chain.x)
            })
        };
        let x = m.chain.x;
        let (mut new, mut new_final, mut bases) = (0.0, 0.0, 0);
        for &(f, cast, w) in &needed {
            if w && (cast != 0 || !present(f)) {
                bases += 1;
                new += agg_row_cost(run, f, x, cast)?;
                new_final += agg_final_cost(f)?;
            }
        }
        // PSP2-06: PostgreSQL runs a FILTER once per aggregate and row, and every base carries it. A raising family's
        // guard adds min / max, so it is rewritten only where its bases are no more than its distinct members: three
        // FILTERed raising sums as four FILTERed bases measured 1.03x-1.11x slower at W8 (algebra q10), four or more
        // faster; agg_row_cost does not price the FILTER.
        let filter_runs = m.filter.is_some() && bases > written && fam.iter().any(|&j| members[j].chain.raising);
        {
            let mut costs = mcx::PgVec::new_in(run.mcx);
            for (n, &j) in fam.iter().enumerate() {
                let known = fam[..n].iter().position(|&k| same_final(&members[k], &members[j], capped)).map(|p| costs[p]);
                let per = match known {
                    Some(c) => c,
                    None => {
                        let (e, _) = build(run.mcx, &members[j], capped)?;
                        crate::costsize::cost_qual_eval_node_pg(&mut *run, e)?.per_tuple
                    }
                };
                costs.push(per);
                new_final += per;
            }
        }
        if filter_runs || new + per_group * new_final >= orig + per_group * orig_final {
            for j in fam {
                members[j].take = false;
            }
        } else {
            price.orig += orig;
            price.new += new;
            price.orig_final += orig_final;
            price.new_final += new_final;
        }
    }
    Ok(price)
}

/// Aggregates whose transition and final cannot raise over any input: count, and sum / avg / min / max of int2 /
/// int4 / int8 (an int8 state, exact int128 / numeric accumulation).
const QUIET_AGGS: [Oid; 14] = [
    COUNT_STAR, COUNT_ANY, AVG_INT8, AVG_INT4, AVG_INT2, SUM_INT8, SUM_INT4, SUM_INT2, MAX_INT8, MAX_INT4, MAX_INT2,
    MIN_INT8, MIN_INT4, MIN_INT2,
];

/// `n` cannot raise: Vars, Consts, Params, leakproof functions (PostgreSQL's contract: no error that depends on the
/// arguments) under a determinate input collation (D2852: `createplan::collation_determinate`; texteq over two columns
/// of conflicting implicit collations is leakproof and raises 42P22), an IN list over one, a one-dimensional ARRAY[],
/// COALESCE, IS [NOT] TRUE, and `QUIET_AGGS` / `types_core::quiet::extremum_quiet` (min / max of text under a set collation
/// that `text_extremum_quiet` admits) with no ORDER BY, a FILTER only where `filter_quiet` (DISTINCT only in
/// `types_core::quiet::distinct_count_quiet`), over such arguments; an implicit-AND list of them.
pub(crate) fn quiet(n: Node<'_>) -> PgResult<bool> {
    fn all(l: &NodeList<'_>) -> PgResult<bool> {
        for x in l.iter() {
            if !quiet(x)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
    Ok(match n.node_tag() {
        NodeTag::T_Var | NodeTag::T_Const => true,
        NodeTag::T_Param => extern_param(n),
        NodeTag::T_RelabelType => quiet(n.as_relabel_type().expect("RelabelType").arg)?,
        NodeTag::T_TargetEntry => quiet(n.as_target_entry().expect("TargetEntry").expr)?,
        NodeTag::T_List => all(n.as_list().expect("List"))?,
        NodeTag::T_BoolExpr => all(&n.as_bool_expr().expect("BoolExpr").args)?,
        NodeTag::T_NullTest => n.as_null_test().expect("NullTest").arg.map_or(Ok(true), quiet)?,
        NodeTag::T_BooleanTest => n.as_boolean_test().expect("BooleanTest").arg.map_or(Ok(true), quiet)?,
        NodeTag::T_OpExpr => {
            let o = n.as_op_expr().expect("OpExpr");
            let f = if o.opfuncid != 0 { o.opfuncid } else { lsyscache::get_opcode(o.opno)? };
            lsyscache::get_func_leakproof(f)? && crate::createplan::collation_determinate(n)? && all(&o.args)?
        }
        NodeTag::T_FuncExpr => {
            let f = n.as_func_expr().expect("FuncExpr");
            lsyscache::get_func_leakproof(f.funcid)? && crate::createplan::collation_determinate(n)? && all(&f.args)?
        }
        // PSP2-12: `x IN (..)` over a leakproof builtin operator, its array a Const or a one-dimensional ARRAY[] (a
        // multidimensional one raises on mismatched dimensions); COALESCE picks an argument and compares nothing.
        NodeTag::T_ScalarArrayOpExpr => {
            let o = n.as_scalar_array_op_expr().expect("ScalarArrayOpExpr");
            let f = if o.opfuncid != 0 { o.opfuncid } else { lsyscache::get_opcode(o.opno)? };
            f < types_core::catalog::FirstNormalObjectId
                && lsyscache::get_func_leakproof(f)?
                && crate::createplan::collation_determinate(n)?
                && all(&o.args)?
        }
        NodeTag::T_ArrayExpr => {
            let a = n.as_array_expr().expect("ArrayExpr");
            !a.multidims && all(&a.elements)?
        }
        NodeTag::T_CoalesceExpr => all(&n.as_coalesce_expr().expect("CoalesceExpr").args)?,
        NodeTag::T_Aggref => {
            let a = n.as_aggref().expect("Aggref");
            let arg_type = || a.args.iter().next().and_then(|t| t.as_target_entry()).map(|t| nodes_core::expr_type(t.expr));
            let distinct = a.aggdistinct.is_nil()
                || arg_type().is_some_and(|t| types_core::quiet::distinct_count_quiet(a.aggfnoid, t));
            let text = types_core::quiet::text_extremum_quiet(a.aggfnoid, a.inputcollid) && a.inputcollid != 0;
            (QUIET_AGGS.contains(&a.aggfnoid) || types_core::quiet::extremum_quiet(a.aggfnoid) || text)
                && a.aggkind == types_nodes::primnodes::AGGKIND_NORMAL
                && a.aggfilter.map_or(Ok(true), filter_quiet)?
                && a.aggorder.is_nil()
                && distinct
                && all(&a.args)?
        }
        _ => false,
    })
}

fn filter_quiet(f: Node<'_>) -> PgResult<bool> {
    Ok(quiet(f)? && builtin_calls(f)? && !clauses::contain_volatile_functions(f)?)
}

fn builtin_calls(n: Node<'_>) -> PgResult<bool> {
    struct Builtins(bool);
    impl<'mcx> nodes_core::NodeWalker<'mcx> for Builtins {
        fn visit(&mut self, node: Node<'mcx>) -> PgResult<bool> {
            let f = match (node.as_op_expr(), node.as_func_expr()) {
                (Some(o), _) => Some(if o.opfuncid != 0 { o.opfuncid } else { lsyscache::get_opcode(o.opno)? }),
                (_, Some(f)) => Some(f.funcid),
                _ => None,
            };
            if f.is_some_and(|f| f >= types_core::catalog::FirstNormalObjectId) {
                self.0 = false;
                return Ok(true);
            }
            nodes_core::expression_tree_walker(node, self)
        }
    }
    let mut w = Builtins(true);
    nodes_core::NodeWalker::visit(&mut w, n)?;
    Ok(w.0)
}

fn build<'mcx>(mcx: mcx::Mcx<'mcx>, m: &Member<'mcx>, capped: bool) -> PgResult<(Node<'mcx>, Option<[Node<'mcx>; 4]>)> {
    let a = m.aggref.as_aggref().expect("Aggref");
    let (c, loc, x, xt) = (&m.chain, a.location, m.chain.x, m.chain.xtype);
    let (sum_fn, min_fn, max_fn) = base_fns(xt);
    let base = |f: Oid, t: Oid| agg_of(mcx, f, t, x, xt, loc, m.filter);
    let count_x = || base(COUNT_ANY, INT8OID);
    let rettype = a.aggtype;
    // PostgreSQL accumulates an int2 / int4 `e` in int8 and wraps (int2_sum / int4_sum / int4_avg_accum never raise);
    // a wider `e` exactly (int128 / numeric).
    let int8_acc = matches!(c.tfinal, INT2OID | INT4OID);
    // No int8 intermediate can pass 2^63 up to N rows; under the cap, never.
    let n = rows_n(c.row_magnitude);
    let bare = capped && under_cap(c.row_magnitude);
    let shift = small_shift(c).filter(|_| capped && !bare);
    let k_count = |k: i64| -> PgResult<Node<'mcx>> { op_of(mcx, INT8_OPS[2], INT8OID, int8_const(mcx, k)?, count_x()?) };
    // S(e) in numeric, exact: sum(x) of an int8 / numeric x, else PostgreSQL's wrapped int8 sum(x) when `e`
    // accumulates in int8 too (the wrap is taken modulo 2^64 below), else sum(x::int8).
    let numeric_s = || -> PgResult<Node<'mcx>> {
        match xt {
            INT2OID | INT4OID if !int8_acc => {
                let cast = CASTS.iter().find(|k| k.1 == xt && k.2 == INT8OID).expect("int -> int8").0;
                let x8 = func_of(mcx, cast, INT8OID, x, CoercionForm::COERCE_IMPLICIT_CAST)?;
                let sum8 = agg_of(mcx, SUM_INT8, NUMERICOID, x8, INT8OID, loc, m.filter)?;
                sum_expr(mcx, c, sum8, NUMERICOID, &count_x, true)
            }
            _ => sum_expr(mcx, c, base(sum_fn, sum_type(xt))?, sum_type(xt), &count_x, true),
        }
    };
    // Below N rows the value, past them PostgreSQL's accumulator: the numeric total modulo 2^64.
    let upto_n = |t: Oid, below: Node<'mcx>, past: Node<'mcx>| -> PgResult<Node<'mcx>> {
        let le = op_of(mcx, INT8_LE, 16, count_x()?, int8_const(mcx, n as i64)?)?;
        case_of(mcx, t, le, below, past)
    };
    let mut marks = None;
    let value = match m.agg {
        Agg::Count => count_x()?,
        Agg::Min | Agg::Max => {
            let low = (m.agg == Agg::Min) == c.dir.is_some_and(|d| d >= 0);
            let edge = if low { base(min_fn, xt)? } else { base(max_fn, xt)? };
            apply(mcx, c, edge)?
        }
        // An int2 / int4 `e` below N rows: S(e) in the int8 form (as `sum`'s, every intermediate exact there) and one
        // conversion, so the final is PostgreSQL's int8_avg over it: numeric_div of the two as numeric. S(e) built in
        // numeric step by step is the same value at scale 0, but costs a hosted numeric call per step and group.
        Agg::Avg if int8_acc => {
            let s = match shift {
                Some(k) => int8_numeric(mcx, wrap_add(mcx, k, &|| base(sum_fn, INT8OID), &|| k_count(k))?)?,
                None => {
                    let s = int8_numeric(mcx, sum_expr(mcx, c, base(sum_fn, INT8OID)?, INT8OID, &count_x, false)?)?;
                    if bare { s } else { upto_n(NUMERICOID, s, wrap64(mcx, numeric_s()?)?)? }
                }
            };
            op_of(mcx, NUMERIC_DIV, NUMERICOID, s, int8_numeric(mcx, count_x()?)?)?
        }
        Agg::Avg => op_of(mcx, NUMERIC_DIV, NUMERICOID, numeric_s()?, int8_numeric(mcx, count_x()?)?)?,
        Agg::Sum if !int8_acc => numeric_s()?,
        Agg::Sum if shift.is_some() => {
            let k = shift.unwrap_or(0);
            let (first_sum, first_count) = (core::cell::Cell::new(None), core::cell::Cell::new(None));
            let a = || -> PgResult<Node<'mcx>> {
                let n = base(sum_fn, INT8OID)?;
                first_sum.set(first_sum.get().or(Some(n)));
                Ok(n)
            };
            let b = || -> PgResult<Node<'mcx>> {
                let n = count_x()?;
                first_count.set(first_count.get().or(Some(n)));
                op_of(mcx, INT8_OPS[2], INT8OID, int8_const(mcx, k)?, n)
            };
            let v = wrap_add(mcx, k, &a, &b)?;
            marks = first_sum.get().zip(first_count.get());
            v
        }
        Agg::Sum => {
            let (sum_x, first_count) = (base(sum_fn, INT8OID)?, core::cell::Cell::new(None));
            let count_once = || -> PgResult<Node<'mcx>> {
                let n = count_x()?;
                first_count.set(first_count.get().or(Some(n)));
                Ok(n)
            };
            let s = sum_expr(mcx, c, sum_x, INT8OID, &count_once, false)?;
            marks = first_count.get().map(|n| (sum_x, n));
            if bare {
                s
            } else {
                let past = func_of(mcx, NUMERIC_INT8_PROC, INT8OID, wrap64(mcx, numeric_s()?)?, CoercionForm::COERCE_EXPLICIT_CAST)?;
                upto_n(INT8OID, s, past)?
            }
        }
    };
    if !m.guard {
        return Ok((value, None));
    }
    let otherwise = if m.agg == Agg::Count { count_x()? } else { typed_null(mcx, rettype)? };
    let (min_x, max_x) = (base(min_fn, xt)?, base(max_fn, xt)?);
    let case = guarded(mcx, c, min_x, max_x, value, rettype, otherwise)?;
    let one_shift = matches!(c.steps[..], [Step::Arith { kind: Kind::Add | Kind::Sub, c: k, .. }] if int_value(k).is_some());
    Ok((case, marks.filter(|_| one_shift).map(|(sum_x, n)| [min_x, max_x, sum_x, n])))
}

fn rows_n(row_magnitude: i128) -> i128 {
    INT8_LIMIT / row_magnitude.max(1)
}

fn under_cap(row_magnitude: i128) -> bool {
    row_magnitude.checked_mul(ROW_CAP).is_some_and(|p| p <= INT8_LIMIT)
}

/// k of a one-step `x ± k` (or `k + x`), an integer literal with |k| < 2^16: over one relation k · count(x) is exact in int8
/// (count(x) <= ROW_CAP), so PostgreSQL's wrapping int8 total sum(x) + k · count(x) has an int8-operator spelling.
fn small_shift(c: &Chain<'_>) -> Option<i64> {
    let [Step::Arith { kind, c: k, c_first, .. }] = c.steps[..] else { return None };
    let v = int_value(k)?;
    let v = match (kind, c_first) {
        (Kind::Add, _) => v,
        (Kind::Sub, false) => v.checked_neg()?,
        _ => return None,
    };
    (v.unsigned_abs() < 1 << 16).then_some(v)
}

/// a + b modulo 2^64 (b has k's sign) in int8 operators none of which can overflow: past i64::MAX it is
/// (a + MIN) + (b + MIN), below i64::MIN (a - MIN) + (b - MIN).
pub(crate) fn wrap_add<'mcx>(
    mcx: mcx::Mcx<'mcx>,
    k: i64,
    a: &dyn Fn() -> PgResult<Node<'mcx>>,
    b: &dyn Fn() -> PgResult<Node<'mcx>>,
) -> PgResult<Node<'mcx>> {
    let add = |l: Node<'mcx>, r: Node<'mcx>| op_of(mcx, INT8_OPS[0], INT8OID, l, r);
    let sub = |l: Node<'mcx>, r: Node<'mcx>| op_of(mcx, INT8_OPS[1], INT8OID, l, r);
    let min = || int8_const(mcx, i64::MIN);
    let plain = add(a()?, b()?)?;
    if k >= 0 {
        let past = op_of(mcx, INT8_GT, 16, a()?, sub(int8_const(mcx, i64::MAX)?, b()?)?)?;
        case_of(mcx, INT8OID, past, add(add(a()?, min()?)?, add(b()?, min()?)?)?, plain)
    } else {
        let below = op_of(mcx, INT8_LT, 16, a()?, sub(min()?, b()?)?)?;
        case_of(mcx, INT8OID, below, add(sub(a()?, min()?)?, sub(b()?, min()?)?)?, plain)
    }
}

/// `v` (an integer-valued numeric) as int8 two's complement wraps it: mod(mod(v + 2^63, 2^64) + 2^64, 2^64) - 2^63,
/// spelled with -2^63 = i64::MIN and 2^64 = i64::MIN · -2 from by-value literals (`numeric_const`).
fn wrap64<'mcx>(mcx: mcx::Mcx<'mcx>, v: Node<'mcx>) -> PgResult<Node<'mcx>> {
    let min = || numeric_const(mcx, i64::MIN);
    let w = || op_of(mcx, NUMERIC_OPS[2], NUMERICOID, numeric_const(mcx, i64::MIN)?, numeric_const(mcx, -2)?);
    let r = op_of(mcx, NUMERIC_MOD, NUMERICOID, op_of(mcx, NUMERIC_OPS[1], NUMERICOID, v, min()?)?, w()?)?;
    let r = op_of(mcx, NUMERIC_OPS[0], NUMERICOID, r, w()?)?;
    let r = op_of(mcx, NUMERIC_MOD, NUMERICOID, r, w()?)?;
    op_of(mcx, NUMERIC_OPS[0], NUMERICOID, r, min()?)
}

fn sum_expr<'mcx>(
    mcx: mcx::Mcx<'mcx>,
    c: &Chain<'mcx>,
    sum_x: Node<'mcx>,
    sum_t: Oid,
    count_x: &dyn Fn() -> PgResult<Node<'mcx>>,
    numeric: bool,
) -> PgResult<Node<'mcx>> {
    let st = |t: Oid| if numeric { NUMERICOID } else { sum_type(t) };
    let (mut s, mut t) = (sum_x, sum_t);
    for step in &c.steps {
        let restype = match *step {
            Step::Arith { restype, .. } | Step::Cast { restype, .. } => restype,
        };
        let next = st(restype);
        if t != next {
            s = int8_numeric(mcx, s)?;
            t = next;
        }
        let Step::Arith { kind, c: k, c_first, .. } = *step else { continue };
        let ops = if t == INT8OID { INT8_OPS } else { NUMERIC_OPS };
        let count = || if t == INT8OID { count_x() } else { int8_numeric(mcx, count_x()?) };
        let folded = (kind == Kind::Sub && !c_first).then(|| int_value(k).and_then(i64::checked_neg)).flatten();
        s = match (kind, c_first) {
            (Kind::Mul, _) => op_of(mcx, ops[2], t, const_as(mcx, k, t, None)?, s)?,
            (Kind::Sub, true) => {
                let term = op_of(mcx, ops[2], t, const_as(mcx, k, t, None)?, count()?)?;
                op_of(mcx, ops[1], t, term, s)?
            }
            (Kind::Sub, false) if folded.is_none() => {
                let term = op_of(mcx, ops[2], t, const_as(mcx, k, t, None)?, count()?)?;
                op_of(mcx, ops[1], t, s, term)?
            }
            _ => {
                let term = op_of(mcx, ops[2], t, const_as(mcx, k, t, folded)?, count()?)?;
                op_of(mcx, ops[0], t, s, term)?
            }
        };
    }
    Ok(s)
}

fn int_value(k: Node<'_>) -> Option<i64> {
    let c = k.as_const().filter(|c| !c.constisnull)?;
    Some(match c.consttype {
        INT2OID => c.constvalue.as_i16().into(),
        INT4OID => c.constvalue.as_i32().into(),
        INT8OID => c.constvalue.as_i64(),
        _ => return None,
    })
}

fn const_as<'mcx>(mcx: mcx::Mcx<'mcx>, k: Node<'mcx>, t: Oid, value: Option<i64>) -> PgResult<Node<'mcx>> {
    if let Some(v) = value.or_else(|| int_value(k)) {
        return if t == INT8OID { int8_const(mcx, v) } else { numeric_const(mcx, v) };
    }
    let kt = match k.as_param() {
        Some(p) => p.paramtype,
        None => k.as_const().expect("a Const or a Param").consttype,
    };
    if kt == t {
        return Ok(k);
    }
    let &(f, _, _) = CASTS.iter().find(|x| x.1 == kt && x.2 == t).expect("a widening cast to the sum type");
    func_of(mcx, f, t, k, CoercionForm::COERCE_IMPLICIT_CAST)
}

fn apply<'mcx>(mcx: mcx::Mcx<'mcx>, c: &Chain<'mcx>, base: Node<'mcx>) -> PgResult<Node<'mcx>> {
    let mut v = base;
    for step in &c.steps {
        v = match *step {
            Step::Arith { opno, opfuncid, restype, c: k, c_first, .. } => {
                let (l, r) = if c_first { (k, v) } else { (v, k) };
                Node::mk(
                    mcx,
                    OpExpr {
                        opno,
                        opfuncid,
                        opresulttype: restype,
                        opretset: false,
                        opcollid: 0,
                        inputcollid: 0,
                        args: NodeList::make2(mcx, l, r)?,
                        location: -1,
                    },
                )?
            }
            Step::Cast { funcid, restype, format } => func_of(mcx, funcid, restype, v, format)?,
        };
    }
    Ok(v)
}

/// `CASE WHEN e(min x) IS NOT NULL AND e(max x) IS NOT NULL THEN value ELSE otherwise END`: raises where a row at an
/// edge would have; over no rows the edges are NULL.
pub(crate) fn guarded<'mcx>(
    mcx: mcx::Mcx<'mcx>,
    c: &Chain<'mcx>,
    min_x: Node<'mcx>,
    max_x: Node<'mcx>,
    value: Node<'mcx>,
    rettype: Oid,
    otherwise: Node<'mcx>,
) -> PgResult<Node<'mcx>> {
    let not_null = |edge| -> PgResult<Node<'mcx>> {
        let arg = Some(apply(mcx, c, edge)?);
        Node::mk(mcx, NullTest { arg, nulltesttype: NullTestType::IS_NOT_NULL, argisrow: false, location: -1 })
    };
    let guard = BoolExpr { boolop: BoolExprType::AND_EXPR, args: NodeList::make2(mcx, not_null(min_x)?, not_null(max_x)?)?, location: -1 };
    case_of(mcx, rettype, Node::mk(mcx, guard)?, value, otherwise)
}

fn case_of<'mcx>(mcx: mcx::Mcx<'mcx>, t: Oid, when: Node<'mcx>, then: Node<'mcx>, other: Node<'mcx>) -> PgResult<Node<'mcx>> {
    let w = Node::mk(mcx, CaseWhen { expr: Some(when), result: Some(then), location: -1 })?;
    Node::mk(mcx, CaseExpr { casetype: t, casecollid: 0, arg: None, args: NodeList::make1(mcx, w)?, defresult: Some(other), location: -1 })
}

fn typed_null(mcx: mcx::Mcx<'_>, t: Oid) -> PgResult<Node<'_>> {
    let (len, byval) = if t == NUMERICOID { (-1, false) } else if t == INT2OID { (2, true) } else if t == INT4OID { (4, true) } else { (8, true) };
    Node::mk_const(mcx, t, -1, 0, len, datum::Datum::null(), true, byval)
}

fn int8_const(mcx: mcx::Mcx<'_>, v: i64) -> PgResult<Node<'_>> {
    Node::mk_const(mcx, INT8OID, -1, 0, 8, datum::Datum::from_i64(v), false, true)
}

/// The integer `v` as numeric: `v::int8::numeric`, a by-value literal cast at run time (the same value and display scale
/// 0). A rewritten member carries several constants that its spelling did not (the count <= N bound, wrap64's), and
/// the lowerer gives each literal its own parameter word: past its budget (PARAM_WORDS) a by-value literal folds into the
/// code, a by-reference one refuses the statement (90 members of `sum(i + k)` over an int4 column).
fn numeric_const(mcx: mcx::Mcx<'_>, v: i64) -> PgResult<Node<'_>> {
    int8_numeric(mcx, int8_const(mcx, v)?)
}

fn int8_numeric<'mcx>(mcx: mcx::Mcx<'mcx>, n: Node<'mcx>) -> PgResult<Node<'mcx>> {
    func_of(mcx, INT8_NUMERIC_PROC, NUMERICOID, n, CoercionForm::COERCE_EXPLICIT_CAST)
}

fn func_of<'mcx>(mcx: mcx::Mcx<'mcx>, funcid: Oid, t: Oid, arg: Node<'mcx>, format: CoercionForm) -> PgResult<Node<'mcx>> {
    Node::mk(
        mcx,
        FuncExpr {
            funcid,
            funcresulttype: t,
            funcretset: false,
            funcvariadic: false,
            funcformat: format,
            funccollid: 0,
            inputcollid: 0,
            args: NodeList::make1(mcx, arg)?,
            location: -1,
        },
    )
}

fn agg_of<'mcx>(
    mcx: mcx::Mcx<'mcx>,
    aggfnoid: Oid,
    aggtype: Oid,
    x: Node<'mcx>,
    xtype: Oid,
    location: types_core::ParseLoc,
    aggfilter: Option<Node<'mcx>>,
) -> PgResult<Node<'mcx>> {
    let mut aggargtypes = OidList::nil();
    aggargtypes.lappend(mcx, xtype)?;
    let arg = Node::mk_target_entry(mcx, x, 1, None, false)?;
    Node::mk(
        mcx,
        Aggref { aggfnoid, aggtype, aggargtypes, args: NodeList::make1(mcx, arg)?, aggfilter, location, ..Aggref::default() },
    )
}

fn op_of<'mcx>(mcx: mcx::Mcx<'mcx>, op: (Oid, Oid), t: Oid, l: Node<'mcx>, r: Node<'mcx>) -> PgResult<Node<'mcx>> {
    Node::mk(
        mcx,
        OpExpr {
            opno: op.0,
            opfuncid: op.1,
            opresulttype: t,
            opretset: false,
            opcollid: 0,
            inputcollid: 0,
            args: NodeList::make2(mcx, l, r)?,
            location: -1,
        },
    )
}

#[cfg(test)]
mod bound_tests {
    use super::{dec_digits, pays, rows_n, under_cap, Price};

    #[test]
    fn the_settle_rule_follows_the_fit() {
        let m = |n: f64| Price { orig: 0.005 * n, new: 0.01, orig_final: 0.0, new_final: 0.0275 * n };
        let (m3, m30) = ((m(3.0), 132.0), (m(30.0), 1320.0));
        for ((price, nodes), rows, width, delta) in [
            (m30, 2_000.0, 1.0, 303),
            (m30, 10_000.0, 1.0, -340),
            (m30, 50_000.0, 4.0, -783),
            (m3, 10_000.0, 1.0, 67),
            (m3, 400_000.0, 1.0, -600),
            (m3, 50_000.0, 4.0, 73),
        ] {
            assert_eq!(pays(price, nodes, rows, 1.0, width), delta < 0, "{nodes} nodes, {rows} rows, width {width}");
        }
    }

    #[test]
    fn int8_form_cap_and_wrap() {
        // smallint + 89, M = 32857: bare under the cap; M = 2^16 reaches 2^63; an int4 x never.
        assert!(under_cap(32_857) && under_cap(65_535) && !under_cap(65_536) && !under_cap(1 << 31));
        for m in [1i128, 89, 32_857, 1 << 31] {
            let n = rows_n(m);
            assert!(m * n <= i64::MAX as i128 && m * (n + 1) > i64::MAX as i128, "M {m}");
        }
        // wrap64 = PostgreSQL's int4_sum accumulator (int8, wrapping): the exact total modulo 2^64. The formula runs on
        // numeric's truncating mod, which Rust's i128 % matches.
        let wrap = |v: i128| ((v + (1 << 63)) % (1 << 64) + (1 << 64)) % (1 << 64) - (1 << 63);
        let (mut acc, mut exact) = (Some(i64::MAX - 5), (i64::MAX - 5) as i128);
        for x in [i32::MAX, 7, i32::MIN, -1, i32::MAX, i32::MAX] {
            acc = adt_numeric::builtins::int4_sum(acc, Some(x));
            exact += x as i128;
            assert_eq!(wrap(exact), acc.unwrap() as i128, "after {x}");
        }
        for v in [0i128, -1, i64::MIN as i128, i64::MIN as i128 - 1, 5 << 64, -(7 << 70) + 3] {
            assert_eq!(wrap(v), v as i64 as i128, "{v}");
        }
    }

    #[test]
    fn decimal_digits() {
        assert_eq!((dec_digits(0), dec_digits(9), dec_digits(10), dec_digits(32767)), (0, 1, 2, 5));
        assert_eq!(dec_digits(u128::MAX), 39);
    }
}
