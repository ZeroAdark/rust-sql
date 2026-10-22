//! Canonical forms before any fast path reads the plan (v5 `normalize`, specs/normalize.md).
//!
//! One pass over the statement, run once by the builder. A rule rewrites a node into one canonical spelling with the
//! same value, NULL, collation and error set; it builds new nodes on the statement's allocator and never changes PG's
//! own `PlannedStmt` (the fallback runs PG's plan unchanged). Readers then test `is_canonical` instead of matching one
//! spelling.
//!
//! Expression rules apply to the graph's ops (`graph`), not to the plan: a copied plan node would miss the readers
//! keyed by its address (demand/readers.rs `Parents`). Rules:
//! - `commute`: a binary operator with a constant (or parameter) on the left and neither on the right becomes its
//!   commutator with the arguments swapped (`K < count(*)` is `count(*) > K`, `'' <> col` is `col <> ''`). The
//!   commutator is `oprcom`; collations stay (PG's `CommuteOpExpr`).
//! - `retype`: a cross-type btree comparison of an expression with a constant becomes its family's same-type comparison
//!   when the constant converts to the expression's type without changing the answer (`int4col < 5::int8` is
//!   `int4col < 5`; `datecol >= timestamp '2020-03-01 12:00'` is `datecol >= date '2020-03-02'`). A timestamp that is not
//!   a midnight rounds up for `<` / `>=`, down for `<=` / `>`, and keeps `=` cross-type. timestamptz (B2-31 / STEP-13)
//!   takes the session TimeZone, which exec's front walk lends (`Zone`): `tstzcol op date K` / `tstzcol op timestamp K`
//!   compare with K converted as PG converts it (`date2timestamptz` / `timestamp2timestamptz`, every operator), and
//!   `datecol op timestamptz K` with the least date whose zone midnight is >= K (`<` / `>=`) or the greatest whose
//!   midnight is <= K (`<=` / `>`): a date's midnight never falls as the date rises in any zone (a skipped local day
//!   shares the next date's), so the answer is PG's.
//!   `datecol = timestamptz K` and a timestamp column against timestamptz (a DST gap's wall times convert out of order)
//!   stay cross-type, and so does every timestamptz arm whose constant is within `ZONE_MARGIN` of the range's ends, so the
//!   front key's `retype_class` holds no zone. A constant out of the type's range and a parameter stay as PG planned them.
//! - `detext` (D3079): `x::text = 'c'` / `x::text <> 'c'`, `x` an int2 / int4 / int8 expression cast through text I/O
//!   (`CoerceViaIO`: int2out / int4out / int8out, GUC-free) and `c` a non-null text constant, is `x = v` / `x <> v` when `c`
//!   is exactly the output text of an in-range value `v` of `x`'s type (an optional `-`, then `0` or a nonzero digit and
//!   digits; never `-0`): the output function is injective, so the texts are equal exactly when the integers are, and
//!   NULL and error set are `x`'s (the cast raises nothing). Any other `c` (`'007'`, `'+7'`, `' 7'`, out of range, not an
//!   integer) is equal to no output text, so the text compare stays as PG planned it. Only under a collation whose equal
//!   bytes are equal text (`bytes_eq`). The front key holds whether it fires (`detext_class`) and files the integer as the
//!   statement's text constant parsed per execution (`detext_value`, bake `Derived::Detext`).
//! - `implied` (SRC-07): the two that stay cross-type, a scan's leading `tscol op timestamptz K` and `datecol = timestamptz
//!   K`, keep PG's operator per row and gain the same-type bound they imply in the session zone (a timestamp's widened
//!   by the zone's offset spread around K), converted per execution, which the zone predicate prunes on.
//! - `like_twin`: a constant prefix, suffix or substring test in another spelling becomes its LIKE (escape `\`):
//!   `starts_with(x, c)` / `x ^@ c` is `x LIKE c%`; an int4 compare of `strpos(x, c)` / `position(c IN x)` with a constant
//!   that is a membership test is `x [NOT] LIKE %c%`; `left(x, n) = c` (n = c's characters) is `x LIKE c%`, `right(x, n)
//!   = c` is `x LIKE %c` (`<>`: NOT LIKE); `x ~ c` / `x !~ c` with no regex metacharacter in `c` is `x [NOT] LIKE %c%`.
//!   Only under a UTF-8 database (a byte match starts on a character) and a set, deterministic collation (equal bytes
//!   are equal text), with a non-null text constant `c`; otherwise PG's function runs.
//! - `like_any`: `x LIKE ANY ('{a,b}')` over a constant text array of one or more elements, none NULL, is `x LIKE 'a' OR x
//!   LIKE 'b'` (`NOT LIKE ALL`: an AND of NOT LIKE; `LIKE ALL`: an AND of LIKE; `NOT LIKE ANY`: an OR of NOT LIKE), `x` a
//!   column (it is read once per arm). Same answer, NULL and error set: ANY is an OR, ALL an AND, over the elements in
//!   order, each stopping where PG's `ExecEvalScalarArrayOp` stops.
//! - `regexp3`: `regexp_replace(s, p, r, flags)` with constant flags `''` is the 3-argument `regexp_replace(s, p, r)` (the
//!   first match, PG's `textregexreplace_noopt`); so is flags `'g'` when `p` is a constant anchored at the string start
//!   (it starts with `^` and has no `|`: without a newline-sensitive flag ARE's `^` matches only at 0, so there is at
//!   most one match).
//! - `coalesce_notnull` (PS1-08): `COALESCE(x, ...)` in a leading filter of a pipeline whose source scans a relation, `x` a
//!   column of that scan (no nullingrels: no outer join nulls it) under a VALID not-null constraint, of the COALESCE's own
//!   type and collation, is `x`. COALESCE returns its first non-NULL argument and evaluates none after it, so value, NULL
//!   and error set are `x`'s. A NOT VALID not-null constraint (PG 18) may hold NULLs, so it never qualifies.
//! - `coalesce_filter`: a leading filter of a scan that is one whole comparison `COALESCE(x, c) op k` (constants `c`,
//!   `k`; a strict zone operator that raises on no input) is `x op k` when `c op k` is false or NULL, `x` of any
//!   nullability: a filter keeps a row only when it is true, and a false and a NULL stop the qual list alike. Never a
//!   comparison nested under NOT, OR, CASE or an AND, where a false and a NULL differ.
//! - `identity` (D3078): a same-type integer `x * 1`, `x / 1`, `x + 0` or `x - 0` (int2 / int4 / int8, the constant on the
//!   right after `commute`) is `x`. None of them raises at 1 or 0 (no overflow, no division by zero) or changes the value,
//!   so value, NULL and error set are `x`'s; a zone or bound reader then sees the column itself (`k::bigint * 1 = 10`
//!   keeps the sorted column's zone skip). The front key holds whether the constant is the unit (`identity_class`).
//! - `product`: a self-commutative arithmetic operator over two non-constant operands puts a column first, columns in
//!   (varno, varattno) order (`b * a` is `a * b`).
//! - `case` needs no rule for a searched CASE: PG's planner already drops a constant-false or NULL WHEN and folds a
//!   constant-true one (clauses.c `eval_const_expressions`, CaseExpr), and the parser spells a missing ELSE as a NULL
//!   constant, so a searched CASE reaches the builder in one form.
//! - `case_simple` (FOLD-19): a simple CASE `CASE x WHEN v1 THEN r1 .. ELSE d END` is the searched `CASE WHEN x = v1 THEN
//!   r1 .. ELSE d END`: each WHEN is the parser's test over a `CaseTestExpr` standing for `x` (transformCaseExpr), so the
//!   test with `x` put in its place is the same comparison. PG evaluates `x` once before the first test; the searched form
//!   evaluates it in every test it reaches. Same value when `x` does not act (effects: no VOLATILE call, nothing the
//!   catalog cannot answer) and holds no SubPlan, and only when evaluating `x` again costs about nothing (`cheap_twice`: a
//!   leaf, or word arithmetic of builtins); same error set, since the first test always evaluates `x` (a CASE has
//!   at least one WHEN; with none left, clauses.c returns the ELSE alone). A test holding another node that reads a
//!   `CaseTestExpr` of its own (a nested simple CASE, an array coercion's element expression, an assignment) stays as PG
//!   planned it. Applied to filters and to an aggregate's arguments (`agg_args`), so every CASE reader takes one form.
//! - `distinct_count` (a plan rule, `node`): `count(*)` over `GROUP BY k` or `SELECT DISTINCT k` (one NOT NULL key; an Agg,
//!   hashed or sorted, a Group or a Unique) is `count(DISTINCT k)` over the grouping's input below its Sorts; over a
//!   nullable key or several keys (DC-21) it is `count(DISTINCT ROW(k1, .., kn))`: a row is never NULL and record_eq
//!   holds two NULL fields equal, so each group, the NULL one too, counts once.

use datum::Datum;
use types_core::{BOOLOID, DATEOID, INT2OID, INT4OID, INT8OID, TIMESTAMPOID, TIMESTAMPTZOID};
use types_nodes::list::NodeList;
use types_nodes::bitmapset::Bitmapset;
use types_nodes::parsenodes::SortGroupClause;
use types_nodes::plannodes::{Agg, Plan};
use types_nodes::primnodes::{Aggref, BoolExpr, BoolExprType, CaseExpr, CaseWhen, CoercionForm, Const, FuncExpr, OpExpr, RowExpr, ScalarArrayOpExpr, TargetEntry, Var, OUTER_VAR};
use types_nodes::{Node, NodeTag};
use mcx::{CollectIn, GrowIn};

// the LIKE twins' spelling rules are gravity_program::twin's (NEW-19); re-exported for this module's callers
pub use gravity_program::twin::{textpos_test, twin_pattern};
use gravity_program::twin::twin_spelling;

use crate::admit::Refusal;
use crate::catalog::Facts;
use crate::graph::{Op, PipelineGraph};
use crate::ledger::Kind;
use crate::ops::shared::agg::{AGG_HASHED, AGG_PLAIN, AGG_SORTED};
use crate::walk::{Builder, STMT_NODE};

/// Bumped by every rule change: normalize's output is what the content key hashes, so the rule set is part of the key.
pub const NORMALIZE_VERSION: u32 = 14;

/// One plan node in canonical form, before the builder maps it (`Builder::plan`). Without a node allocator (plain
/// `decompose`) nothing is rewritten. The new node keeps the old one's `plan_node_id`; PG's `PlannedStmt` is untouched,
/// so readers keyed by a plan node's address (demand/readers.rs `Parents`) find no entry for it and fail closed.
pub fn node<'p>(b: &Builder<'p, '_>, n: Node<'p>) -> Result<Node<'p>, Refusal> {
    let Some(mcx) = b.alloc else { return Ok(n) };
    let oom = |_| Refusal::shape(Kind::Agg, STMT_NODE, "normalize: out of memory");
    match distinct_count(b, n) {
        Some(Count::One(m)) => distinct_agg(mcx, m).map_err(oom),
        Some(Count::Row(m)) => row_agg(mcx, m).map_err(oom),
        None => Ok(n),
    }
}

/// `distinct_count`'s two rewrites: one NOT NULL key, or the row of the keys (a nullable key, several keys).
enum Count<'p> {
    One(DistinctCount<'p>),
    Row(RowCount<'p>),
}

/// `distinct_count`'s match: the outer Agg, the one-key grouping's input below its Sorts, the key's column there, its
/// `=`, whether that `=` hashes, the key's Var and its btree `<`.
struct DistinctCount<'p> {
    outer: &'p Agg<'p>,
    input: Node<'p>,
    col: i16,
    eqop: u32,
    hashable: bool,
    key: Node<'p>,
    sortop: u32,
    /// The grouping's estimate of its groups (`group_estimate`).
    groups: i64,
}

/// `distinct_count`'s row match (DC-21): the outer Agg, the grouping's plan, its key columns (in the grouping's input),
/// the input below its Sorts, and whether every key's `=` hashes.
struct RowCount<'p> {
    outer: &'p Agg<'p>,
    inner: &'p Plan<'p>,
    cols: &'p [i16],
    input: Node<'p>,
    hashable: bool,
}

/// The most keys the row form takes (its field names are static: `ROW_NAMES`).
const ROW_KEYS: usize = 16;
/// The field names PG's parser gives a `ROW(..)` of that many fields (parse_expr.c transformRowExpr: "f1", "f2", ..).
const ROW_NAMES: [&str; ROW_KEYS] = ["f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12", "f13", "f14", "f15", "f16"];
/// pg_operator `=` and `<` for record (record_eq, record_lt: record_ops, the btree default for record).
const RECORD_EQ_OP: u32 = 2988;
const RECORD_LT_OP: u32 = 2990;

/// Rule `distinct_count` (landing 7): a plain Agg whose every output is a bare `count(*)`, over (a SubqueryScan over)
/// a grouping by one key that is a NOT NULL column of the scan below it, counts the distinct keys: `count(DISTINCT k)`
/// over the grouping's input. The grouping is any node the planner spells `GROUP BY k` or `SELECT DISTINCT k` with: an
/// Agg (hashed or sorted) with no aggregate, a Group or a Unique over the sorted input. There is one row per key value
/// and no NULL key, so the answers are equal. A Sort under the grouping only orders its rows and count(DISTINCT k)
/// reads them in any order, so the rewrite reads below it, as the spelled count(DISTINCT k) reads its scan.
/// DC-21: a grouping by a key that may be NULL, or by several keys (at most `ROW_KEYS`), each a column of the grouping's
/// input under its type's default btree `=` and its own collation, counts the distinct rows of its keys instead:
/// `count(DISTINCT ROW(k1, .., kn))` (`Count::Row`). The grouping puts NULL keys in one group per combination, and
/// record_eq holds two NULL fields equal and a row is never NULL, so the count is the number of groups, the NULL group
/// included. A HAVING, an aggregate in the inner Agg, any other outer output, a parameter or an initplan keep the plan.
fn distinct_count<'p>(b: &Builder<'p, '_>, n: Node<'p>) -> Option<Count<'p>> {
    let outer = n.as_agg().filter(|a| a.aggstrategy == AGG_PLAIN && a.numCols == 0 && simple(a))?;
    let stars = outer.plan.targetlist.iter().all(|t| {
        t.as_target_entry().and_then(|t| t.expr.as_aggref()).is_some_and(|a| {
            a.aggfnoid == F_COUNT_STAR && a.aggstar && a.aggfilter.is_none() && a.aggdistinct.is_nil() && a.aggorder.is_nil() && a.agglevelsup == 0
        })
    });
    let mut below = outer.plan.lefttree?;
    if let Some(s) = below.as_subquery_scan() {
        if !s.scan.plan.qual.is_nil() || !s.scan.plan.initPlan.is_nil() || !s.scan.plan.allParam.is_empty() {
            return None;
        }
        below = s.subplan?;
    }
    // the grouping: (its plan, the keys' input columns, their `=`, their collations); every key column indexes the input's list
    let (inner, cols, eqops, colls): (&'p Plan<'p>, &'p [i16], &'p [u32], &'p [u32]) = if let Some(a) = below.as_agg() {
        (matches!(a.aggstrategy, AGG_SORTED | AGG_HASHED) && a.numCols >= 1 && simple(a)).then_some((&a.plan, a.grpColIdx, a.grpOperators, a.grpCollations))?
    } else if let Some(g) = below.as_group() {
        (g.numCols >= 1 && plain(&g.plan)).then_some((&g.plan, g.grpColIdx, g.grpOperators, g.grpCollations))?
    } else {
        let u = below.as_unique()?;
        (u.numCols >= 1 && plain(&u.plan)).then_some((&u.plan, u.uniqColIdx, u.uniqOperators, u.uniqCollations))?
    };
    if !stars || outer.plan.targetlist.is_nil() || inner.targetlist.iter().any(|t| t.as_target_entry().is_none_or(|t| t.expr.as_var().is_none())) {
        return None;
    }
    if cols.is_empty() || cols.len() > ROW_KEYS || eqops.len() != cols.len() || colls.len() != cols.len() {
        return None;
    }
    let lower = inner.lefttree?;
    // one NOT NULL key: count(DISTINCT k) under the key's own `=` and its family's `<`
    if cols.len() == 1 && notnull_col(b, lower, cols[0]) {
        let (col, eqop) = (cols[0], eqops[0]);
        let key = inner.targetlist.iter().filter_map(|t| t.as_target_entry()).map(|t| t.expr).find(|e| {
            e.as_var().is_some_and(|v| v.varno == OUTER_VAR && v.varattno == col && v.varlevelsup == 0)
        })?;
        let ty = key.as_var()?.vartype;
        let (family, left, right, strategy) = crate::effects::builtin_btree_member(eqop)?;
        if left != ty || right != ty || strategy != 3 {
            return None;
        }
        let sortop = crate::effects::builtin_btree_op(family, ty, ty, 1)?;
        let (input, col) = below_sorts(lower, col)?;
        let hashable = crate::catalog::hash_eq::builtin_hash_eq(b.facts, eqop); // as the parser marks the spelled clause
        return Some(Count::One(DistinctCount { outer, input, col, eqop, hashable, key, sortop, groups: group_estimate(inner) }));
    }
    // DC-21: each key a Var of the grouping's list, grouped by its type's default btree `=` (the one record_eq applies
    // to the field) under the Var's own collation, and read below the grouping's Sorts (each passes the column through,
    // so every key reaches the same input)
    let mut hashable = true;
    for (i, &col) in cols.iter().enumerate() {
        let v = key_var(inner, col)?;
        // an anonymous record key (or an array of one) has no row form: its fields are known only per value, and the
        // record key the call would keep refuses a record field, where the grouping serves the key (keep the plan)
        if crate::catalog::anonymous_record(v.vartype) || v.vartype == types_core::catalog::RECORDARRAYOID {
            return None;
        }
        let (family, left, right, strategy) = crate::effects::builtin_btree_member(eqops[i])?;
        if left != v.vartype || right != v.vartype || strategy != 3 || colls[i] != v.varcollid {
            return None;
        }
        let sortop = crate::effects::builtin_btree_op(family, v.vartype, v.vartype, 1)?;
        if !b.facts.key(crate::catalog::KeyReq::sort(sortop)).is_some_and(|o| o.default_family) {
            return None;
        }
        below_sorts(lower, col)?;
        hashable &= crate::catalog::hash_eq::builtin_hash_eq(b.facts, eqops[i]);
    }
    let (input, _) = below_sorts(lower, cols[0])?;
    Some(Count::Row(RowCount { outer, inner, cols, input, hashable }))
}

/// The key Var of grouping list `inner` at input column `col` (an OUTER_VAR of this level).
fn key_var<'p>(inner: &'p Plan<'p>, col: i16) -> Option<&'p Var<'p>> {
    inner.targetlist.iter().filter_map(|t| t.as_target_entry()).filter_map(|t| t.expr.as_var()).find(|v| v.varno == OUTER_VAR && v.varattno == col && v.varlevelsup == 0)
}

/// Column `col` of `input` read below its Sorts: each Sort passes its input's column through (`notnull_col` walks the
/// same way). The node first not a Sort, and the column there.
fn below_sorts<'p>(mut input: Node<'p>, mut col: i16) -> Option<(Node<'p>, i16)> {
    while let (Some(_), Some(p)) = (input.as_sort(), input.as_plan()) {
        let at = usize::try_from(col).ok().and_then(|c| c.checked_sub(1)).filter(|&c| c < p.targetlist.len())?;
        let v = p.targetlist.nth(at).as_target_entry().and_then(|t| t.expr.as_var()).filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0)?;
        (input, col) = (p.lefttree?, v.varattno);
    }
    Some((input, col))
}

/// An Agg with no HAVING, grouping sets, chain, split, parameter, initplan or zone annotation.
fn simple(a: &Agg<'_>) -> bool {
    plain(&a.plan) && a.aggsplit == 0 && a.groupingSets.is_nil() && a.chain.is_nil() && a.gravity_rewrite.is_none()
}

/// A plan node with no qual (a Group's HAVING), parameter, initplan or second input.
fn plain(p: &Plan<'_>) -> bool {
    p.qual.is_nil() && p.initPlan.is_nil() && p.allParam.is_empty() && p.righttree.is_none()
}

/// Whether output column `col` of `n` is a NOT NULL column of a heap scan, through Sorts.
fn notnull_col(b: &Builder<'_, '_>, mut n: Node<'_>, mut col: i16) -> bool {
    loop {
        let Some(p) = n.as_plan() else { return false };
        let Some(e) = usize::try_from(col).ok().and_then(|c| c.checked_sub(1)).filter(|&c| c < p.targetlist.len()).and_then(|c| p.targetlist.nth(c).as_target_entry()).map(|t| t.expr) else { return false };
        let Some(v) = e.as_var().filter(|v| v.varlevelsup == 0 && v.varnullingrels.is_empty()) else { return false };
        if n.as_sort().is_some() {
            let (Some(l), true) = (p.lefttree, v.varno == OUTER_VAR) else { return false };
            (n, col) = (l, v.varattno);
            continue;
        }
        let scanrelid = match (n.as_seq_scan(), n.as_index_scan()) {
            (Some(s), _) => s.scan.scanrelid,
            (_, Some(s)) => s.scan.scanrelid,
            _ => return false,
        };
        let rel = b.rte(scanrelid).map_or(0, |r| r.relid);
        return v.varno == scanrelid as i32 && rel != 0 && b.facts.att(rel, v.varattno).is_some_and(|a| a.notnull);
    }
}

/// The rewritten Agg: plain, over the grouping's input below its Sorts, each `count(*)` now `count(DISTINCT key)` (PG's
/// own Aggref for that call: one argument entry with sort-group ref 1, its clause the group's `=` and the family's `<`).
fn distinct_agg<'p>(mcx: mcx::Mcx<'p>, m: DistinctCount<'p>) -> types_error::PgResult<Node<'p>> {
    let v = m.key.as_var().expect("distinct_count matched a Var");
    let key = Node::mk(mcx, types_nodes::primnodes::Var { varattno: m.col, varnullingrels: v.varnullingrels.clone_in(mcx)?, ..*v })?;
    let mut tlist = NodeList::with_capacity(mcx, m.outer.plan.targetlist.len())?;
    for t in m.outer.plan.targetlist.iter().filter_map(|t| t.as_target_entry()) {
        let a = t.expr.as_aggref().expect("distinct_count matched count(*)");
        let arg = Node::mk(mcx, TargetEntry { expr: key, resno: 1, resname: None, ressortgroupref: 1, resorigtbl: 0, resorigcol: 0, resjunk: false })?;
        let clause = Node::mk(mcx, SortGroupClause { tleSortGroupRef: 1, eqop: m.eqop, sortop: m.sortop, reverse_sort: false, nulls_first: false, hashable: m.hashable })?;
        let count = Aggref {
            aggfnoid: F_COUNT_ANY,
            inputcollid: v.varcollid,
            aggargtypes: types_nodes::list::OidList::make1(mcx, v.vartype)?,
            aggdirectargs: NodeList::nil(),
            args: NodeList::make1(mcx, arg)?,
            aggorder: NodeList::nil(),
            aggdistinct: NodeList::make1(mcx, clause)?,
            aggstar: false,
            ..*a
        };
        let expr = Node::mk(mcx, count)?;
        tlist.lappend(mcx, Node::mk(mcx, TargetEntry { expr, ..*t })?)?;
    }
    let p = &m.outer.plan;
    let plan = Plan { targetlist: tlist, qual: NodeList::nil(), lefttree: Some(m.input), righttree: None, initPlan: NodeList::nil(), extParam: Bitmapset::empty(), allParam: Bitmapset::empty(), ..*p };
    Node::mk(mcx, Agg { plan, aggstrategy: AGG_PLAIN, aggsplit: 0, numCols: 0, grpColIdx: &[], grpOperators: &[], grpCollations: &[], numGroups: m.groups, transitionSpace: m.outer.transitionSpace, aggParams: Bitmapset::empty(), groupingSets: NodeList::nil(), chain: NodeList::nil(), distinctHashed: false, gravity_rewrite: None, gravity_finals: &[], gravity_groups_default: false })
}

/// The grouping's estimate of its groups (its plan rows, at least 1). The count's distinct table holds exactly those
/// groups, so the rewritten plain Agg carries it as its numGroups (PostgreSQL's plain Agg says 1) and decompose agg's
/// `groups` sizes the distinct form's partitions from it, as the grouping's own table was, not from the input's rows.
fn group_estimate(inner: &Plan<'_>) -> i64 {
    (crate::admit::plan_rows(inner).round() as i64).max(1)
}

/// DC-21's rewritten Agg: plain, over the grouping's input below its Sorts, each `count(*)` now
/// `count(DISTINCT ROW(k1, .., kn))` (PG's own Aggref for that call over the parser's anonymous row: one argument entry
/// with sort-group ref 1, its clause record_eq / record_lt, hashable when every key's `=` is).
fn row_agg<'p>(mcx: mcx::Mcx<'p>, m: RowCount<'p>) -> types_error::PgResult<Node<'p>> {
    let lower = m.inner.lefttree.expect("distinct_count matched the grouping's input");
    let mut fields = NodeList::with_capacity(mcx, m.cols.len())?;
    let mut names = NodeList::with_capacity(mcx, m.cols.len())?;
    for (i, &col) in m.cols.iter().enumerate() {
        let v = key_var(m.inner, col).expect("distinct_count matched each key's Var");
        let (_, at) = below_sorts(lower, col).expect("distinct_count read each key below the Sorts");
        fields.lappend(mcx, Node::mk(mcx, Var { varattno: at, varnullingrels: v.varnullingrels.clone_in(mcx)?, ..*v })?)?;
        names.lappend(mcx, Node::mk_string(mcx, ROW_NAMES[i])?)?;
    }
    let row = Node::mk(mcx, RowExpr { args: fields, row_typeid: types_core::catalog::RECORDOID, row_format: CoercionForm::COERCE_IMPLICIT_CAST, colnames: names, location: -1 })?;
    let mut tlist = NodeList::with_capacity(mcx, m.outer.plan.targetlist.len())?;
    for t in m.outer.plan.targetlist.iter().filter_map(|t| t.as_target_entry()) {
        let a = t.expr.as_aggref().expect("distinct_count matched count(*)");
        let arg = Node::mk(mcx, TargetEntry { expr: row, resno: 1, resname: None, ressortgroupref: 1, resorigtbl: 0, resorigcol: 0, resjunk: false })?;
        let clause = Node::mk(mcx, SortGroupClause { tleSortGroupRef: 1, eqop: RECORD_EQ_OP, sortop: RECORD_LT_OP, reverse_sort: false, nulls_first: false, hashable: m.hashable })?;
        let count = Aggref {
            aggfnoid: F_COUNT_ANY,
            inputcollid: 0,
            aggargtypes: types_nodes::list::OidList::make1(mcx, types_core::catalog::RECORDOID)?,
            aggdirectargs: NodeList::nil(),
            args: NodeList::make1(mcx, arg)?,
            aggorder: NodeList::nil(),
            aggdistinct: NodeList::make1(mcx, clause)?,
            aggstar: false,
            ..*a
        };
        let expr = Node::mk(mcx, count)?;
        tlist.lappend(mcx, Node::mk(mcx, TargetEntry { expr, ..*t })?)?;
    }
    let p = &m.outer.plan;
    let plan = Plan { targetlist: tlist, qual: NodeList::nil(), lefttree: Some(m.input), righttree: None, initPlan: NodeList::nil(), extParam: Bitmapset::empty(), allParam: Bitmapset::empty(), ..*p };
    Node::mk(mcx, Agg { plan, aggstrategy: AGG_PLAIN, aggsplit: 0, numCols: 0, grpColIdx: &[], grpOperators: &[], grpCollations: &[], numGroups: group_estimate(m.inner), transitionSpace: m.outer.transitionSpace, aggParams: Bitmapset::empty(), groupingSets: NodeList::nil(), chain: NodeList::nil(), distinctHashed: false, gravity_rewrite: None, gravity_finals: &[], gravity_groups_default: false })
}

/// Each filter of the built graph in canonical form. Without a node allocator (plain `decompose`) nothing is rewritten.
/// A pipeline's leading filters (before its first other op) are its source's quals: `coalesce_notnull` reads their Vars
/// of the source's range table index as the scanned relation's columns, and `coalesce_filter` reads each as a whole qual
/// of the scan's list; a later filter (a join's, an upper node's) never.
pub fn graph<'p>(g: &mut PipelineGraph<'p>, facts: &dyn Facts, alloc: Option<mcx::Mcx<'p>>) -> Result<(), Refusal> {
    let Some(mcx) = alloc else { return Ok(()) };
    for p in g.pipelines.iter_mut() {
        let mut scan = p.source.relation().map(|(rti, relid, _)| (i32::from(rti), relid));
        for op in p.ops.iter_mut() {
            match op {
                Op::Filter(q) => *q = scan_filter(mcx, facts, scan, *q)?,
                _ => scan = None,
            }
        }
    }
    Ok(())
}

/// One leading filter of a scan (`scan`: its range table index and relation) in canonical form: `scan_expr`, then
/// `coalesce_filter` over the filter as a whole. Without `scan` it is `scan_expr`.
pub fn scan_filter<'p>(mcx: mcx::Mcx<'p>, facts: &dyn Facts, scan: Option<(i32, u32)>, n: Node<'p>) -> Result<Node<'p>, Refusal> {
    let cur = scan_expr(mcx, facts, scan, n)?;
    if scan.is_none() {
        return Ok(cur);
    }
    let oom = |_| Refusal::shape(Kind::Result, STMT_NODE, "normalize: out of memory");
    let cur = coalesce_filter(mcx, n, cur).map_err(oom)?.unwrap_or(cur);
    Ok(implied(mcx, scan, cur).map_err(oom)?.unwrap_or(cur))
}

/// Rule `implied` (SRC-07): a scan's leading filter whose whole qual is `x op K`, `x` a column of the scan and `K` a
/// timestamptz constant, where `x` is a timestamp column (any btree comparison) or a date column (`=`, the one
/// `retype` keeps cross-type), gains the same-type comparison(s) that qual implies in the session zone, just before it:
/// the qual itself stays (PG's operator, per row), and the implied one is a plain `x op' K'` a zone predicate prunes on
/// (a scan's zone predicates come from the leading quals up to the first that may raise, and the effects table does not
/// know PG 18's cross-type timestamp compares as raise-free: `timestamp_cmp_timestamptz_internal` maps an overflowing
/// conversion to an infinity, so a row the implied bound drops is one the qual rejects quietly),
/// its constant converted per execution (bake `Derived::Retype` with `IMPLIED`, `implied_value`). A timestamp column has
/// no exact same-type form (a DST gap's wall times convert out of order), so its bound is widened by the zone's offset
/// spread around K: exact away from a transition; `=` gains both `>=` and `<=`. A date equal to an instant gains both
/// too, at `retype`'s bounds (`zoned`): the dates whose midnight is the instant, two at a skipped local day (D3537), none
/// when it is no date's midnight. Fires under the same constant-alone class as `retype`'s zone arms (`implied_class`), so
/// the front key holds whether it fires (`implied_class_of`); a conversion moves `zone_reads` as `zoned`'s does.
fn implied<'p>(mcx: mcx::Mcx<'p>, scan: Option<(i32, u32)>, cur: Node<'p>) -> types_error::PgResult<Option<Node<'p>>> {
    let Some((rti, _)) = scan else { return Ok(None) };
    let quals: mcx::PgVec<'p, Node<'p>> = cur.as_list().map_or_else(|| core::iter::once(cur).collect_in(mcx), |l| l.iter().collect_in(mcx));
    let (mut all, mut added) = (mcx::reserved_in(quals.len() + 2, mcx), false);
    for q in quals.iter().copied() {
        let Some((v, k, kn, family, (left, right, s))) = implied_shape(q).filter(|m| m.0.varno == rti) else {
            all.push_in(q);
            continue;
        };
        let strategies: &[u8] = if s == 3 { &[4, 2] } else { core::slice::from_ref(&s) };
        let mut built = mcx::PgVec::new_in(mcx);
        for &s2 in strategies {
            let (Some(same), Some(value)) = (crate::effects::builtin_btree_op(family, left, left, s2), implied_value(right, left, s2, k.constvalue)) else {
                built.clear();
                break;
            };
            let Some(code) = crate::effects::builtin_oper(same).map(|p| p.code) else {
                built.clear();
                break;
            };
            built.push_in((same, code, value, s2));
        }
        for (same, code, value, s2) in built {
            let c = Node::mk_const(mcx, left, -1, 0, typlen(left).unwrap_or(8), value, false, true)?;
            crate::bake::derived_note(addr(c), crate::bake::Derived::Retype { src: addr(kn), strategy: s2 | IMPLIED });
            let x = Node::mk(mcx, Var { varnullingrels: v.varnullingrels.clone_in(mcx)?, ..*v })?;
            let args = NodeList::from_slice(mcx, &[x, c])?;
            all.push_in(Node::mk(mcx, OpExpr { opno: same, opfuncid: code, opresulttype: BOOLOID, opretset: false, opcollid: 0, inputcollid: 0, args, location: -1 })?);
            added = true;
        }
        all.push_in(q);
    }
    if !added {
        return Ok(None);
    }
    Ok(Some(Node::mk_list(mcx, NodeList::from_slice(mcx, &all[..])?)?))
}

/// `implied`'s match of one qual in canonical form (`commute`'s): the column, the constant, the btree family and the
/// cross-type member's `(column type, constant type, strategy)`.
fn implied_shape<'p>(q: Node<'p>) -> Option<(&'p Var<'p>, &'p Const, Node<'p>, u32, (u32, u32, u8))> {
    let o = q.as_op_expr().filter(|o| o.args.len() == 2)?;
    let v = o.args.nth(0).as_var().filter(|v| v.varlevelsup == 0 && v.varattno > 0)?;
    let kn = o.args.nth(1);
    let k = kn.as_const().filter(|k| !k.constisnull)?;
    let (family, left, right, s) = crate::effects::builtin_btree_member(o.opno)?;
    (k.consttype == right && v.vartype == left && implied_class(right, left, s, k.constvalue).is_some()).then_some((v, k, kn, family, (left, right, s)))
}

/// The front key's class of rule `implied` for plan OpExpr `n` (exec inputs/front.rs keys it, as `retype_class`): in
/// `commute`'s form, `Some(true)` when the rule adds its implied comparison for this constant, `Some(false)` when only
/// the constant (or the zone's being lent and active) keeps it from it, `None` when the shape never matches. Whether the
/// qual is a whole leading filter of the scan is the plan's shape, which the key holds.
pub fn implied_class_of(n: Node<'_>) -> Option<bool> {
    let o = n.as_op_expr().filter(|o| o.args.len() == 2)?;
    implied_class_at(o, commute(n))
}

/// `implied_class_of` of OpExpr `o` (two arguments) given its `commute` match `com`.
fn implied_class_at(o: &OpExpr<'_>, com: Commuted<'_>) -> Option<bool> {
    let (x, k, opno) = match com {
        Some((_, com, _)) => (o.args.nth(1), o.args.nth(0), com),
        None => (o.args.nth(0), o.args.nth(1), o.opno),
    };
    let (v, k) = (x.as_var()?, k.as_const()?);
    let (_, left, right, s) = crate::effects::builtin_btree_member(opno)?;
    if k.consttype != right || v.vartype != left {
        return None;
    }
    Some(!k.constisnull && implied_class(right, left, s, k.constvalue)?)
}

/// `retype`'s strategy flag for an implied comparison's constant (`implied`): `convert` gives `implied_value`.
pub const IMPLIED: u8 = 0x80;

/// How far around a timestamptz constant `spread` reads the zone's offsets: past the widest offset gap (under 32 h)
/// both ways, so any wall time whose instant can sit on the other side of the constant is inside it.
pub const IMPLIED_WINDOW: i64 = 3 * USECS_PER_DAY;

/// `implied`'s class by the constant alone (`Some(false)` out of `ZONE_MARGIN`, an infinity, or no active zone; `None`
/// for any other pair): a timestamp column against a timestamptz constant under any btree strategy, a date column under
/// `=`.
fn implied_class(from: u32, to: u32, s: u8, v: Datum) -> Option<bool> {
    let ok = match (from, to) {
        (TIMESTAMPTZOID, TIMESTAMPOID) if (1..=5).contains(&s) => true,
        (TIMESTAMPTZOID, DATEOID) if s == 3 => true,
        _ => return None,
    };
    let t = v.as_i64();
    Some(ok && (MIN_TIMESTAMP + ZONE_MARGIN..END_TIMESTAMP - ZONE_MARGIN).contains(&t) && ZONE.get().is_some_and(|z| (z.active)()))
}

/// The implied comparison's constant (`implied`): timestamptz `K` as a bound of a column of type `to` under strategy `s`
/// that every row the cross-type compare keeps satisfies, in the session zone. A timestamp `x` compares as
/// `x - off(x)` (PG's `timestamp2timestamptz`, `off` an offset of the zone near that wall time: the one in effect, or a
/// transition's before / after one in a gap or a fold), so with `lo` / `hi` the zone's offsets within `IMPLIED_WINDOW`
/// of `K`: `x - off(x) < K` gives `x < K + off(x) <= K + hi`, and `x > K` gives `x > K + lo` (each the same for `<=` /
/// `>=`); a wall time more than 32 h from `K` is on its own side whatever its offset (every offset is under 16 h), and
/// one nearer has its instant inside the window. Away from a transition `lo = hi` and the bound is K's wall time, exact.
/// A date `x = K` implies `x >= lo` and `x <= hi` (strategies 4 and 2 of the qual's 3), `lo` the least date whose
/// midnight is >= K and `hi` the greatest whose midnight is <= K (`zoned`'s bounds): not just `timestamptz_date(K)`, as
/// a skipped local day's midnight is the next date's (D3537).
fn implied_value(from: u32, to: u32, s: u8, v: Datum) -> Option<Datum> {
    if implied_class(from, to, if to == DATEOID { 3 } else { s }, v) != Some(true) {
        return None;
    }
    let z = ZONE.get()?;
    zone_read();
    let k = v.as_i64();
    match to {
        TIMESTAMPOID => {
            let (lo, hi) = (z.spread)(k)?;
            let b = match s {
                1 | 2 => k.checked_add(hi)?,
                4 | 5 => k.checked_add(lo)?,
                _ => return None,
            };
            (MIN_TIMESTAMP..END_TIMESTAMP).contains(&b).then(|| Datum::from_i64(b))
        }
        DATEOID if matches!(s, 2 | 4) => zoned(TIMESTAMPTZOID, DATEOID, s, v),
        _ => None,
    }
}

/// One expression in canonical form: the rules applied bottom-up (each fires at most once per node).
pub fn expr<'p>(mcx: mcx::Mcx<'p>, facts: &dyn Facts, n: Node<'p>) -> Result<Node<'p>, Refusal> {
    scan_expr(mcx, facts, None, n)
}

/// `expr` over a scan's quals (`scan`: its range table index and relation, for `coalesce_notnull`).
pub fn scan_expr<'p>(mcx: mcx::Mcx<'p>, facts: &dyn Facts, scan: Option<(i32, u32)>, n: Node<'p>) -> Result<Node<'p>, Refusal> {
    let oom = |_| Refusal::shape(Kind::Result, STMT_NODE, "normalize: out of memory or stack");
    Ok(rewrite(mcx, facts, scan, n).map_err(oom)?.unwrap_or(n))
}

fn rewrite<'p>(mcx: mcx::Mcx<'p>, facts: &dyn Facts, scan: Option<(i32, u32)>, n: Node<'p>) -> types_error::PgResult<Option<Node<'p>>> {
    crate::walk::stack_check(); // D3748
    let kids = nodes_core::expression_tree_mutator_dyn(mcx, n, &mut |y| rewrite(mcx, facts, scan, y))?;
    let mut cur = kids.unwrap_or(n);
    // the node `like_twin` matches is the plan's, `commute`'s form aside (the front key's `twin_class` reads that one)
    let mut plain = kids.is_none();
    if let Some(x) = coalesce_notnull(facts, scan, cur) {
        cur = x;
        plain = false;
    }
    if let Some((o, com, code)) = commute(cur) {
        let args = NodeList::from_slice(mcx, &[o.args.nth(1), o.args.nth(0)])?;
        cur = Node::mk(mcx, OpExpr { opno: com, opfuncid: code, args, ..*o })?;
    }
    if let Some(x) = identity(cur) {
        cur = x;
        plain = false;
    }
    if let Some(o) = product(cur) {
        let args = NodeList::from_slice(mcx, &[o.args.nth(1), o.args.nth(0)])?;
        cur = Node::mk(mcx, OpExpr { args, ..*o })?;
        plain = false;
    }
    if let Some((o, opno, code, k, strategy)) = retype(cur) {
        let c = Node::mk_const(mcx, k.0, -1, 0, k.1, k.2, k.3, true)?;
        crate::bake::derived_note(addr(c), crate::bake::Derived::Retype { src: addr(o.args.nth(1)), strategy });
        let args = NodeList::from_slice(mcx, &[o.args.nth(0), c])?;
        cur = Node::mk(mcx, OpExpr { opno, opfuncid: code, args, ..*o })?;
        plain = false;
    }
    if let Some((o, x, opno, code, ty, v)) = detext(facts, cur) {
        let c = Node::mk_const(mcx, ty, -1, 0, typlen(ty).unwrap_or(8), v, false, true)?;
        let k = if o.args.nth(0).as_const().is_some() { o.args.nth(0) } else { o.args.nth(1) };
        crate::bake::derived_note(addr(c), crate::bake::Derived::Detext { src: addr(k) });
        let args = NodeList::from_slice(mcx, &[x, c])?;
        cur = Node::mk(mcx, OpExpr { opno, opfuncid: code, inputcollid: 0, args, ..*o })?;
        plain = false;
    }
    if let Some(t) = like_twin(facts, cur) {
        cur = like(mcx, t, plain)?;
    }
    if let Some((s, body)) = like_any(cur) {
        cur = like_arms(mcx, s, body)?.unwrap_or(cur);
    }
    if let Some(f) = regexp3(cur) {
        let args = NodeList::from_slice(mcx, &[f.args.nth(0), f.args.nth(1), f.args.nth(2)])?;
        cur = Node::mk(mcx, FuncExpr { funcid: F_REGEXP_REPLACE3, args, ..*f })?;
    }
    if let Some(c) = case_simple(mcx, facts, cur)? {
        cur = c;
    }
    Ok(if cur.ptr_eq(n) { None } else { Some(cur) })
}

/// `case_simple`'s match: a simple CASE whose test value `x` does not act and holds no SubPlan, each WHEN's test reading
/// no `CaseTestExpr` but `x`'s (`own_tests`).
fn simple_case<'p>(facts: &dyn Facts, n: Node<'p>) -> Option<(&'p CaseExpr<'p>, Node<'p>)> {
    let c = n.as_case_expr()?;
    let x = c.arg?;
    if c.args.is_nil() || crate::expr::has_subplan(x) || !c.args.iter().all(|w| w.as_case_when().and_then(|w| w.expr).is_some_and(own_tests)) {
        return None;
    }
    if !cheap_twice(x) {
        return None;
    }
    let arena = gravity_program::Arena::new(64 << 10); // alloc: Cold
    (!crate::effects::expr_acts(&crate::effects::Cx::without_stmt(facts, arena.handle()), x)).then_some((c, x))
}

/// The searched form evaluates the test value `x` in every test it reaches, so `x` must cost about nothing to evaluate
/// again: a leaf (a column, read once per row and memoised; a constant; a parameter), relabelled or not, or an expression
/// whose every node is a by-value word and whose every call is a builtin (word arithmetic the emitter open-codes). A
/// by-reference value inside (a text or numeric function: `substr(t, 1, 4)`) or a user function keeps PG's form, where
/// `x` is evaluated once (FOLD-19's ABAB worst case: ten arms over `substr` ran 3.1x the simple CASE).
fn cheap_twice(x: Node<'_>) -> bool {
    let mut leaf = x;
    while let Some(r) = leaf.as_relabel_type() {
        leaf = r.arg;
    }
    if matches!(leaf.node_tag(), NodeTag::T_Var | NodeTag::T_Const | NodeTag::T_Param) {
        return true;
    }
    let builtin = |f: u32| crate::effects::builtin_func(f).is_some();
    crate::expr::walk(x, &mut |n| {
        let word = crate::catalog::expr_type::type_of(n).is_none_or(|t| crate::catalog::typefacts::row_of(t).is_some_and(|f| f.byval));
        let builtin = match n.node_tag() {
            NodeTag::T_FuncExpr => n.as_func_expr().is_some_and(|f| builtin(f.funcid)),
            NodeTag::T_OpExpr => n.as_op_expr().is_some_and(|o| builtin(o.opfuncid)),
            _ => true,
        };
        word && builtin
    })
}

/// Test `e` reads a `CaseTestExpr` only as its CASE's test value: no node in it binds one of its own (a simple CASE, an
/// array coercion's element expression, a field or subscript assignment), and every node is one `expr::kids` knows.
fn own_tests(e: Node<'_>) -> bool {
    crate::expr::walk(e, &mut |n| match n.node_tag() {
        NodeTag::T_CaseExpr => n.as_case_expr().is_some_and(|c| c.arg.is_none()),
        NodeTag::T_ArrayCoerceExpr | NodeTag::T_FieldStore => false,
        NodeTag::T_SubscriptingRef => n.as_subscripting_ref().is_some_and(|r| r.refassgnexpr.is_none()),
        _ => true,
    })
}

/// Rule `case_simple` (FOLD-19): the searched CASE of a `simple_case` match, each WHEN's test with `x` in place of its
/// `CaseTestExpr`; the results, the ELSE, type and collation unchanged.
pub fn case_simple<'p>(mcx: mcx::Mcx<'p>, facts: &dyn Facts, n: Node<'p>) -> types_error::PgResult<Option<Node<'p>>> {
    let Some((c, x)) = simple_case(facts, n) else { return Ok(None) };
    let mut whens = NodeList::with_capacity(mcx, c.args.len())?;
    for w in c.args.iter() {
        let w = w.as_case_when().expect("simple_case matched CaseWhen arms");
        let test = w.expr.expect("simple_case matched a test per WHEN");
        let expr = Some(put_test(mcx, test, x)?.unwrap_or(test));
        whens.lappend(mcx, Node::mk(mcx, CaseWhen { expr, result: w.result, location: w.location })?)?;
    }
    fires::fire("normalize.case_simple"); // shapecheck fire signal (REGISTRY FOLD-19)
    Ok(Some(Node::mk(mcx, CaseExpr { casetype: c.casetype, casecollid: c.casecollid, arg: None, args: whens, defresult: c.defresult, location: c.location })?))
}

/// `e` with `x` in place of every `CaseTestExpr` (`own_tests` holds: each is the CASE's test value).
fn put_test<'p>(mcx: mcx::Mcx<'p>, e: Node<'p>, x: Node<'p>) -> types_error::PgResult<Option<Node<'p>>> {
    crate::walk::stack_check(); // D3748
    if e.node_tag() == NodeTag::T_CaseTestExpr {
        return Ok(Some(x));
    }
    nodes_core::expression_tree_mutator_dyn(mcx, e, &mut |y| put_test(mcx, y, x))
}

/// An aggregate `a`'s arguments and FILTER in canonical form for its readers: `case_simple` applied throughout (the
/// other rules are the filters' own). `a` itself when nothing changes or there is no node allocator.
pub fn agg_args<'p>(b: &Builder<'p, '_>, a: Node<'p>) -> Result<Node<'p>, Refusal> {
    let Some(mcx) = b.alloc.filter(|_| a.as_aggref().is_some()) else { return Ok(a) };
    let oom = |_| Refusal::shape(Kind::Agg, STMT_NODE, "normalize: out of memory or stack");
    let facts = b.facts;
    let r = nodes_core::expression_tree_mutator_dyn(mcx, a, &mut |y| case_rule(mcx, facts, y)).map_err(oom)?;
    Ok(r.unwrap_or(a))
}

/// `case_simple` bottom-up over `n` (an aggregate's argument or FILTER), an inner CASE first.
fn case_rule<'p>(mcx: mcx::Mcx<'p>, facts: &dyn Facts, n: Node<'p>) -> types_error::PgResult<Option<Node<'p>>> {
    crate::walk::stack_check(); // D3748
    let kids = nodes_core::expression_tree_mutator_dyn(mcx, n, &mut |y| case_rule(mcx, facts, y))?;
    let cur = kids.unwrap_or(n);
    Ok(case_simple(mcx, facts, cur)?.or(kids))
}

/// Rule `coalesce_notnull` (PS1-08): `COALESCE(x, ...)` is `x` when `x` is a column of the scan `scan` reads (its range
/// table index and relation: level 0, no nullingrels, so no outer join nulls it) that a VALID not-null constraint covers
/// (`AttFact.notnull` is the relcache's attnullability, D2838: PG 18's NOT NULL ... NOT VALID sets attnotnull over rows
/// that may still be NULL, so it never qualifies), of the COALESCE's own type and collation (a cast or another collation
/// keeps the COALESCE). PG's COALESCE returns its first non-NULL argument and evaluates none after it, so the value, NULL
/// and error set are `x`'s.
fn coalesce_notnull<'p>(facts: &dyn Facts, scan: Option<(i32, u32)>, n: Node<'p>) -> Option<Node<'p>> {
    let ((rti, relid), c) = (scan?, n.as_coalesce_expr()?);
    let x = c.args.iter().next()?;
    let v = x.as_var().filter(|v| v.varno == rti && v.varlevelsup == 0 && v.varattno > 0 && v.varnullingrels.is_empty())?;
    let same = v.vartype == c.coalescetype && v.varcollid == c.coalescecollid;
    (same && facts.att(relid, v.varattno).is_some_and(|a| a.notnull && a.typid == v.vartype)).then_some(x)
}

/// Rule `coalesce_filter`: a scan's leading filter that is one whole comparison `COALESCE(x, c) op k` (the plan's, or
/// `commute`'s form of it), `c` and `k` constants, is `x op k` when `c op k` is false or NULL, whatever `x`'s
/// nullability. A filter keeps a row only when it is true, and PG's qual list stops on a false and on a NULL alike
/// (ExecQual), so both forms keep the same rows and stop at the same place: a NULL `x` gives `c op k` in one and NULL in
/// the other, neither true; a non-NULL `x` gives `x op k` in both. `x` runs once in both, `c` is a constant, and `op`
/// (`coalesce_cmp`) is strict and raises on no input, so the error set is `x`'s. Only a whole filter: under NOT, OR,
/// CASE or as an AND's argument a false and a NULL differ, so a nested comparison keeps its COALESCE; so does one whose
/// constant `retype` rebuilt (`k` must be the plan's). The rule reads both constants' values, so the front key holds
/// whether it fires (`coalesce_filter_class`).
fn coalesce_filter<'p>(mcx: mcx::Mcx<'p>, plan: Node<'p>, cur: Node<'p>) -> types_error::PgResult<Option<Node<'p>>> {
    let Some((k, false)) = coalesce_cmp(plan) else { return Ok(None) };
    let Some(o) = cur.as_op_expr().filter(|o| o.args.len() == 2 && o.args.nth(1).ptr_eq(k)) else { return Ok(None) };
    let Some(c) = o.args.nth(0).as_coalesce_expr().filter(|c| c.args.len() == 2) else { return Ok(None) };
    let args = NodeList::from_slice(mcx, &[c.args.nth(0), k])?;
    Ok(Some(Node::mk(mcx, OpExpr { args, ..*o })?))
}

/// `coalesce_filter`'s match of plan comparison `n` in the form `rewrite` hands it (`commute`'s when it fires): its
/// constant `k` and whether `c op k` is true. `op` is a zone operator (`zone_op`: a btree comparison, strict and immutable
/// as every btree member is, or a btree `=`'s negator, over two types whose words compare as the values do) whose function
/// raises on no input (`effects::quiet_call`), and both constants are by-value words of those types, so `c op k` is the
/// two words' comparison; a NULL constant gives NULL. `None` when the shape never matches.
fn coalesce_cmp<'p>(n: Node<'p>) -> Option<(Node<'p>, bool)> {
    let o = n.as_op_expr().filter(|o| o.args.len() == 2)?;
    coalesce_cmp_at(o, commute(n))
}

/// `coalesce_cmp` of OpExpr `o` (two arguments) given its `commute` match `com`.
fn coalesce_cmp_at<'p>(o: &'p OpExpr<'p>, com: Commuted<'p>) -> Option<(Node<'p>, bool)> {
    use gravity_program::ZoneOp;
    let (a, k, opno) = match com {
        Some((_, com, _)) => (o.args.nth(1), o.args.nth(0), com),
        None => (o.args.nth(0), o.args.nth(1), o.opno),
    };
    let c = a.as_coalesce_expr().filter(|c| c.args.len() == 2)?.args.nth(1).as_const()?;
    let kc = k.as_const()?;
    let op = crate::ops::shared::scan::zone_op(opno)?;
    let code = crate::effects::builtin_oper(opno)?.code;
    if !crate::effects::quiet_call(&crate::catalog::NoFacts, code) {
        return None;
    }
    if c.constisnull || kc.constisnull {
        return Some((k, false));
    }
    let word = |c: &Const| crate::ops::shared::scan::zone_word(c).filter(|_| c.constlen != 1);
    let (l, r) = (word(c)?, word(kc)?);
    let holds = match op {
        ZoneOp::Lt => l < r,
        ZoneOp::Le => l <= r,
        ZoneOp::Eq => l == r,
        ZoneOp::Ge => l >= r,
        ZoneOp::Gt => l > r,
        ZoneOp::Ne => l != r,
        _ => return None,
    };
    Some((k, holds))
}

/// The front key's class of rule `coalesce_filter` for plan OpExpr `n` (exec inputs/front.rs keys it, as
/// `retype_class`): `Some(true)` when `c op k` is false or NULL (the rule rewrites `n` where it is a whole leading
/// filter), `Some(false)` when it is true, `None` when the shape never matches. The two constants' values are the only
/// inputs it reads that the plan's shape does not decide.
pub fn coalesce_filter_class(n: Node<'_>) -> Option<bool> {
    coalesce_cmp(n).map(|(_, holds)| !holds)
}

/// `commute`'s answer for one OpExpr (`op_classes` shares it among the classes).
type Commuted<'p> = Option<(&'p OpExpr<'p>, u32, u32)>;

/// `commute`'s match: the OpExpr, its commutator and the commutator's function. D3577: never into a commutator whose
/// function is LANGUAGE sql (`interval + timestamp` is `select $2 + $1`): PostgreSQL runs the operator as written, its C
/// function, and the swapped form would run a SQL function per row (each call admitted on its own, ~380x) and name it in an
/// error's CONTEXT.
fn commute<'p>(n: Node<'p>) -> Commuted<'p> {
    let o = n.as_op_expr().filter(|o| o.args.len() == 2)?;
    let fixed = |x: Node<'_>| x.as_const().is_some() || x.as_param().is_some();
    if !fixed(o.args.nth(0)) || fixed(o.args.nth(1)) {
        return None;
    }
    let com = crate::effects::builtin_oper(o.opno)?.commutator;
    let code = crate::effects::builtin_oper(com).filter(|_| com != 0)?.code;
    if crate::effects::builtin_sql_body(code) {
        return None;
    }
    Some((o, com, code))
}

/// Rule `product` (landing 8): a self-commutative arithmetic operator (`x * y`, `x + y`: its commutator is itself, its
/// result not a boolean) over two non-constant operands orders them, a column before any other expression and columns
/// by (varno, varattno), so `y * x` and `x * y` are one form. Constants are already on the right (`commute`), the form
/// `agg_algebra`'s products keep in projections is not read here. Evaluation order is free (user ruling); the value,
/// NULL and error set of a commutative operator do not depend on it.
fn product<'p>(n: Node<'p>) -> Option<&'p OpExpr<'p>> {
    let o = n.as_op_expr().filter(|o| o.args.len() == 2 && o.opresulttype != BOOLOID)?;
    let rank = |x: Node<'_>| x.as_var().map_or((1, 0, 0), |v| (0, v.varno, i32::from(v.varattno)));
    let (l, r) = (o.args.nth(0), o.args.nth(1));
    let fixed = |x: Node<'_>| x.as_const().is_some() || x.as_param().is_some();
    if fixed(l) || fixed(r) || rank(r) >= rank(l) {
        return None;
    }
    crate::effects::builtin_oper(o.opno).filter(|p| p.commutator == o.opno).map(|_| o)
}

/// Integer `+` / `-` (unit 0) and `*` / `/` (unit 1) whose result is the left operand's type: int4, int2, int8 of one
/// type (`int4pl` 177 .. `int8div` 466) and a narrower constant on the right (`int84pl` 1274 .. `int84div` 1277, `int82pl`
/// 837 / `int82mi` 838 / `int82mul` 839, `int42pl` 179 / `int42mi` 183 / `int42mul` 171): an int4 column's
/// `k::bigint * 1` is int84mul.
fn identity_unit(funcid: u32) -> Option<i64> {
    match funcid {
        177 | 181 | 176 | 180 | 463 | 464 | 1274 | 1275 | 837 | 838 | 179 | 183 => Some(0),
        141 | 154 | 152 | 153 | 465 | 466 | 1276 | 1277 | 839 | 171 => Some(1),
        _ => None,
    }
}

/// `identity`'s test of `x op k` (`k` on the right): `Some(true)` when `k` is the operator's unit, `Some(false)` when only
/// `k`'s value keeps it, `None` when the shape never matches (another operator, a cross-type one, a NULL or non-constant `k`).
fn identity_shape(o: &OpExpr<'_>, x: Node<'_>, k: Node<'_>) -> Option<bool> {
    let k = k.as_const().filter(|k| !k.constisnull)?;
    let unit = identity_unit(o.opfuncid)?;
    if x.as_const().is_some() || crate::catalog::expr_type::type_of(x) != Some(o.opresulttype) {
        return None;
    }
    let raw = k.constvalue.as_i64(); // one by-value read, narrowed to the constant's type
    let v = match k.consttype {
        INT2OID => i64::from(raw as i16),
        INT4OID => i64::from(raw as i32),
        INT8OID => raw,
        _ => return None,
    };
    Some(v == unit)
}

/// Rule `identity`'s match (D3078): `x` of an integer `x op unit`.
fn identity<'p>(n: Node<'p>) -> Option<Node<'p>> {
    let o = n.as_op_expr().filter(|o| o.args.len() == 2)?;
    identity_shape(o, o.args.nth(0), o.args.nth(1)).filter(|&u| u).map(|_| o.args.nth(0))
}

/// The front key's class of rule `identity` for plan OpExpr `n`, in the form `rewrite` hands it (`commute`'s when it
/// fires): whether its constant is the operator's unit; `None` when its shape never matches.
pub fn identity_class(n: Node<'_>) -> Option<bool> {
    let o = n.as_op_expr().filter(|o| o.args.len() == 2)?;
    identity_class_at(o, commute(n))
}

/// `identity_class` of OpExpr `o` (two arguments) given its `commute` match `com`.
fn identity_class_at(o: &OpExpr<'_>, com: Commuted<'_>) -> Option<bool> {
    match com {
        Some(_) => identity_shape(o, o.args.nth(1), o.args.nth(0)),
        None => identity_shape(o, o.args.nth(0), o.args.nth(1)),
    }
}

/// `retype`'s match: the OpExpr, the same-type operator and its function, the converted constant
/// `(type, length, value, null)`, and the operator's btree strategy.
fn retype<'p>(n: Node<'p>) -> Option<(&'p OpExpr<'p>, u32, u32, (u32, i32, Datum, bool), u8)> {
    let o = n.as_op_expr().filter(|o| o.args.len() == 2)?;
    let (opno, code, k, (left, right, strategy)) = retype_shape(o.args.nth(0), o.args.nth(1), o.opno)?;
    let len = typlen(left)?;
    let value = if k.constisnull { Datum::from_i64(0) } else { convert(right, left, strategy, k.constvalue)? };
    Some((o, opno, code, (left, len, value, k.constisnull), strategy))
}

/// `retype`'s match of `x opno k` without the constant's value: the same-type operator and its function, the constant,
/// and the cross-type member's `(left type, right type, strategy)`.
fn retype_shape<'p>(x: Node<'p>, k: Node<'p>, opno: u32) -> Option<(u32, u32, &'p Const, (u32, u32, u8))> {
    let k = k.as_const()?;
    if x.as_const().is_some() || x.as_param().is_some() {
        return None;
    }
    let (family, left, right, strategy) = crate::effects::builtin_btree_member(opno)?;
    if left == right || k.consttype != right {
        return None;
    }
    typlen(left)?;
    let same = crate::effects::builtin_btree_op(family, left, left, strategy)?;
    let code = crate::effects::builtin_oper(same)?.code;
    Some((same, code, k, (left, right, strategy)))
}

/// PG's text `=` operator (`texteq`).
const TEXT_EQ: u32 = 98;
/// btree opfamily `integer_ops` (int2 / int4 / int8).
const INTEGER_OPS: u32 = 1976;

/// Whether `ty` is one of btree integer_ops' own types (int2, int4, int8): the family's equality operator over it.
pub fn integer_type(ty: u32) -> bool {
    crate::effects::builtin_btree_op(INTEGER_OPS, ty, ty, 3).is_some()
}

/// `detext`'s match without the constant's value and the collation: the OpExpr, the cast's argument, the text constant,
/// the integer type of the argument, and the integer operator (`=` or `<>`, as the text one) and its function. The
/// constant may stand on either side (the plan's form; `rewrite` sees `commute`'s, the cast first).
fn detext_shape<'p>(n: Node<'p>) -> Option<(&'p OpExpr<'p>, Node<'p>, &'p Const, u32, u32, u32)> {
    let o = n.as_op_expr().filter(|o| o.args.len() == 2)?;
    let eq = o.opno == TEXT_EQ;
    if !eq && o.opno != crate::effects::builtin_oper(TEXT_EQ)?.negator {
        return None;
    }
    let (l, r) = (o.args.nth(0), o.args.nth(1));
    let (cast, k) = if l.as_const().is_some() { (r, l) } else { (l, r) };
    let x = cast.as_coerce_via_io().filter(|c| c.resulttype == crate::catalog::TEXTOID)?.arg;
    if x.as_const().is_some() || x.as_param().is_some() {
        return None;
    }
    let ty = nodes_core::expr_type(x);
    if !matches!(ty, INT2OID | INT4OID | INT8OID) {
        return None;
    }
    let k = k.as_const().filter(|k| !k.constisnull && k.consttype == crate::catalog::TEXTOID)?;
    let same = crate::effects::builtin_btree_op(INTEGER_OPS, ty, ty, 3)?;
    let opno = if eq { same } else { crate::effects::builtin_oper(same)?.negator };
    if opno == 0 {
        return None;
    }
    let code = crate::effects::builtin_oper(opno)?.code;
    Some((o, x, k, ty, opno, code))
}

/// Rule `detext` (D3079): the OpExpr, the cast's argument, the integer operator and its function, the integer type and
/// the constant's value, when the text constant is the canonical output of one under a collation whose equal bytes are
/// equal text.
fn detext<'p>(facts: &dyn Facts, n: Node<'p>) -> Option<(&'p OpExpr<'p>, Node<'p>, u32, u32, u32, Datum)> {
    let (o, x, k, ty, opno, code) = detext_shape(n)?;
    if o.inputcollid == 0 || !facts.coll(o.inputcollid).bytes_eq {
        return None;
    }
    let v = detext_value(ty, crate::expr::const_bytes_keyed(k)?)?;
    Some((o, x, opno, code, ty, v))
}

/// D3079: the value of integer type `ty` (int2 / int4 / int8) whose output text (int2out / int4out / int8out) is exactly
/// `s`: an optional `-`, then `0` or a nonzero digit followed by digits, in range, never `-0`. `None` for any other text
/// (a leading `+`, zero or space, a sign alone, out of range), which equals no output text.
pub fn detext_value(ty: u32, s: &[u8]) -> Option<Datum> {
    let d = s.strip_prefix(b"-").unwrap_or(s);
    if d.is_empty() || !d.iter().all(u8::is_ascii_digit) || (d[0] == b'0' && (d.len() > 1 || d.len() < s.len())) {
        return None;
    }
    let v: i64 = core::str::from_utf8(s).ok()?.parse().ok()?;
    match ty {
        INT2OID => i16::try_from(v).ok().map(Datum::from_i16),
        INT4OID => i32::try_from(v).ok().map(Datum::from_i32),
        INT8OID => Some(Datum::from_i64(v)),
        _ => None,
    }
}

/// The front key's class of rule `detext` for plan OpExpr `n` (exec inputs/front.rs keys it, as `retype_class`):
/// `Some(true)` when the text constant is the canonical output of a value of the cast's integer type, `Some(false)` when
/// it is not (the text compare stays), `None` when the shape never matches. The operator, its collation (whose
/// `bytes_eq` decides too) and the types are the plan's, which the front key holds; the constant's form is the only input
/// it does not.
pub fn detext_class(n: Node<'_>) -> Option<bool> {
    let (_, _, k, ty, _, _) = detext_shape(n)?;
    Some(crate::expr::const_bytes_keyed(k).and_then(|s| detext_value(ty, s)).is_some())
}

/// The front key's class of rule `retype` for plan OpExpr `n` (exec inputs/front.rs keys it, as `pattern_class`): in
/// the form `rewrite` hands `retype` (`commute`'s when it fires), `Some(true)` when `retype` rewrites it (its constant
/// converts, or is NULL), `Some(false)` when only the constant's value keeps it cross-type, `None` when its shape never
/// matches. The value is the only input `retype` reads that the plan's shape does not decide.
pub fn retype_class(n: Node<'_>) -> Option<bool> {
    let o = n.as_op_expr().filter(|o| o.args.len() == 2)?;
    retype_class_at(o, commute(n))
}

/// `retype_class` of OpExpr `o` (two arguments) given its `commute` match `com`.
fn retype_class_at(o: &OpExpr<'_>, com: Commuted<'_>) -> Option<bool> {
    let (x, k, opno) = match com {
        Some((_, com, _)) => (o.args.nth(1), o.args.nth(0), com),
        None => (o.args.nth(0), o.args.nth(1), o.opno),
    };
    let (_, _, k, (left, right, strategy)) = retype_shape(x, k, opno)?;
    let v = k.constvalue;
    Some(k.constisnull || zone_class(right, left, strategy, v).unwrap_or_else(|| convert(right, left, strategy, v).is_some()))
}

/// The classes exec's front walk keys for each plan OpExpr, in its order: `retype_class`, `detext_class`,
/// `identity_class`, `implied_class_of`, `twin_class` and `coalesce_filter_class`, each that fn's own answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpClasses {
    pub retype: Option<bool>,
    pub detext: Option<bool>,
    pub identity: Option<bool>,
    pub implied: Option<bool>,
    pub twin: Option<u64>,
    pub coalesce: Option<bool>,
}

/// `OpClasses` of plan OpExpr `n` from one `commute` match (each class fn matches it again on its own); all `None` for
/// any other node or an argument list of another length than two, as each fn answers there.
pub fn op_classes(n: Node<'_>) -> OpClasses {
    let none = OpClasses { retype: None, detext: None, identity: None, implied: None, twin: None, coalesce: None };
    let Some(o) = n.as_op_expr().filter(|o| o.args.len() == 2) else { return none };
    let com = commute(n);
    OpClasses {
        retype: retype_class_at(o, com),
        detext: detext_class(n),
        identity: identity_class_at(o, com),
        implied: implied_class_at(o, com),
        twin: twin_class_at(n, com),
        coalesce: coalesce_cmp_at(o, com).map(|(_, holds)| !holds),
    }
}

/// The session TimeZone's casts a timestamptz `retype` reads (decompose reads no zone): PG's `date2timestamptz`,
/// `timestamp2timestamptz` and `timestamptz_date` in the session zone, `None` where PG raises, the zone's identity
/// (`stamp`: equal while the session zone is the same) and whether this admission may convert at all (`active`: false
/// for a portal fetched over several calls, between which a SET TimeZone may come while PG compares each row in the zone
/// of its FETCH). Exec's front walk lends them (inputs/front.rs `run`); unset or inactive, every timestamptz compare
/// stays as PG planned it.
#[derive(Clone, Copy)]
pub struct Zone {
    pub date_tstz: fn(i32) -> Option<i64>,
    pub ts_tstz: fn(i64) -> Option<i64>,
    pub tstz_date: fn(i64) -> Option<i32>,
    /// SRC-07: the least and greatest UTC offset (microseconds, local minus UTC) the session zone takes at any instant
    /// within `IMPLIED_WINDOW` of the given one (pgtz's DST boundaries); `None` where it cannot tell.
    pub spread: fn(i64) -> Option<(i64, i64)>,
    pub stamp: fn() -> usize,
    pub active: fn() -> bool,
}

static ZONE: pgsync::OnceLock<Zone> = pgsync::OnceLock::new();

/// Lends `retype` the session zone's casts (the first call wins; the casts read the calling thread's zone each time).
pub fn set_zone(z: Zone) {
    let _ = ZONE.set(z);
}

leader_tls::leader_tls! {
    thread_local! {
        /// The timestamptz conversions `zoned` (and `implied_value`) have made on this thread: a backend's session, whose
        /// zone the lent casts read. An admission that sees the count move (`zone_held`) holds a constant of the session
        /// TimeZone, which exec's plan memo serves again only under that zone (`zone_moved`). D3740: one count per thread,
        /// not per process: another session's conversion during this admission marked it too, and a named cursor's (or a
        /// counted portal's) admission so marked was refused ZONE_REFUSAL at its FETCH though it held no constant at all
        /// (exec keyed.rs re-admits a zoned cursor admission once; the next foreign conversion marked that one again).
        static ZONE_READS: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
    }
}

/// One conversion made in the session zone, on this thread's count (`ZONE_READS`).
fn zone_read() {
    ZONE_READS.with(|c| c.set(c.get().wrapping_add(1)));
}

/// The count `zone_held` compares, read before an admission (this thread's: `ZONE_READS`).
pub fn zone_reads() -> u64 {
    ZONE_READS.with(core::cell::Cell::get)
}

/// The session zone's `stamp` when a timestamptz conversion was made since `zone_reads` read `since`; `None` when none
/// was (the admission holds no zone).
pub fn zone_held(since: u64) -> Option<usize> {
    (zone_reads() != since).then(zone_stamp)
}

/// Whether an admission holding `held` (`zone_held`) converted its constants in another zone than the session's now.
pub fn zone_moved(held: Option<usize>) -> bool {
    held.is_some_and(|z| z != zone_stamp())
}

fn zone_stamp() -> usize {
    ZONE.get().map_or(0, |z| (z.stamp)())
}

/// How far inside the timestamp range a timestamptz arm's constant must sit: wider than any zone's offset (under 16 h),
/// so whether it converts is the constant's alone, never the zone's.
const ZONE_MARGIN: i64 = 2 * USECS_PER_DAY;

/// `retype_class` of a timestamptz arm, by the constant alone; `None` for the zone-free arms (`convert` decides).
fn zone_class(from: u32, to: u32, s: u8, v: Datum) -> Option<bool> {
    let inside = |t: i64| t == i64::MIN || t == i64::MAX || (MIN_TIMESTAMP + ZONE_MARGIN..END_TIMESTAMP - ZONE_MARGIN).contains(&t);
    let ok = match (from, to) {
        (DATEOID, TIMESTAMPTZOID) => match v.as_i32() {
            i32::MIN | i32::MAX => true,
            d => i64::from(d).checked_mul(USECS_PER_DAY).is_some_and(inside),
        },
        (TIMESTAMPOID, TIMESTAMPTZOID) => inside(v.as_i64()),
        (TIMESTAMPTZOID, DATEOID) => s != 3 && inside(v.as_i64()),
        _ => return None,
    };
    Some(ok && ZONE.get().is_some_and(|z| (z.active)()))
}

/// `convert`'s timestamptz arms (`zone_class` true): the tstz column's constant as PG casts it; a date column's bound,
/// found from the constant's own date, one step either way, by the zone midnights that bracket it. A date's midnight is
/// never earlier than the previous date's, but a local day a zone skipped whole (Pacific/Apia's date-line move) shares
/// the next date's midnight (`date2timestamptz` reads a gap's wall time at the offset before it): `<` / `>=` take the
/// least date whose midnight is >= K, `<=` / `>` the greatest whose midnight is <= K, past every date sharing it (D3537).
fn zoned(from: u32, to: u32, s: u8, v: Datum) -> Option<Datum> {
    let z = ZONE.get()?;
    zone_read();
    match (from, to) {
        (DATEOID, TIMESTAMPTZOID) => match v.as_i32() {
            i32::MIN => Some(i64::MIN),
            i32::MAX => Some(i64::MAX),
            d => (z.date_tstz)(d),
        }
        .map(Datum::from_i64),
        (TIMESTAMPOID, TIMESTAMPTZOID) => match v.as_i64() {
            t @ (i64::MIN | i64::MAX) => Some(t),
            t => (z.ts_tstz)(t),
        }
        .map(Datum::from_i64),
        (TIMESTAMPTZOID, DATEOID) => {
            let t = v.as_i64();
            let d = match t {
                i64::MIN => i32::MIN,
                i64::MAX => i32::MAX,
                _ => {
                    let mid = |d: i32| (z.date_tstz)(d);
                    let mut d = (z.tstz_date)(t)?;
                    if mid(d)? < t {
                        d += 1;
                    } else if mid(d - 1)? >= t {
                        d -= 1;
                    }
                    let (at, before) = (mid(d)?, mid(d - 1)?);
                    if at < t || before >= t {
                        return None;
                    }
                    match s {
                        1 | 4 => d,
                        2 | 5 if at == t => {
                            let mut e = d;
                            for _ in 0..2 {
                                if mid(e + 1)? != t {
                                    break;
                                }
                                e += 1;
                            }
                            if mid(e + 1)? <= t {
                                return None;
                            }
                            e
                        }
                        2 | 5 => d - 1,
                        _ => return None,
                    }
                }
            };
            Some(Datum::from_i32(d))
        }
        _ => None,
    }
}

fn addr(n: Node<'_>) -> usize {
    n.as_raw().as_ptr() as usize
}

/// The value `retype` gives a retyped constant: plan constant value `v` of type `from` as type `to` under btree strategy
/// `strategy` (`convert`); `None` when `retype` keeps the comparison cross-type.
pub fn retype_value(from: u32, to: u32, strategy: u8, v: Datum) -> Option<Datum> {
    convert(from, to, strategy, v)
}

/// The by-value types `convert` reads and writes, with their `typlen`.
fn typlen(t: u32) -> Option<i32> {
    match t {
        INT2OID => Some(2),
        INT4OID | DATEOID => Some(4),
        INT8OID | TIMESTAMPOID | TIMESTAMPTZOID => Some(8),
        _ => None,
    }
}

const USECS_PER_DAY: i64 = 86_400_000_000;
/// PG's timestamp range `[MIN_TIMESTAMP, END_TIMESTAMP)` (datatype/timestamp.h).
const MIN_TIMESTAMP: i64 = -211_813_488_000_000_000;
const END_TIMESTAMP: i64 = 9_223_371_331_200_000_000;

/// `v` (type `from`) as a value of `to` such that `x op v` keeps its answer for every `x` of type `to`, where `op` is
/// btree strategy `s` (1 `<`, 2 `<=`, 3 `=`, 4 `>=`, 5 `>`); none when no such value exists. Infinities map to infinities
/// (PG's date / timestamp cross compares treat them so).
fn convert(from: u32, to: u32, s: u8, v: Datum) -> Option<Datum> {
    let int = |t: u32, v: Datum| match t {
        INT2OID => Some(v.as_i16() as i64),
        INT4OID => Some(v.as_i32() as i64),
        INT8OID => Some(v.as_i64()),
        _ => None,
    };
    if s & IMPLIED != 0 {
        return implied_value(from, to, s & !IMPLIED, v);
    }
    match (from, to) {
        (_, INT2OID) => int(from, v).and_then(|x| i16::try_from(x).ok()).map(Datum::from_i16),
        (_, INT4OID) => int(from, v).and_then(|x| i32::try_from(x).ok()).map(Datum::from_i32),
        (_, INT8OID) => int(from, v).map(Datum::from_i64),
        (DATEOID, TIMESTAMPOID) => {
            let d = v.as_i32();
            let t = match d {
                i32::MIN => i64::MIN,
                i32::MAX => i64::MAX,
                _ => i64::from(d).checked_mul(USECS_PER_DAY).filter(|t| (MIN_TIMESTAMP..END_TIMESTAMP).contains(t))?, // D3530: a date past the range wrapped back into it
            };
            Some(Datum::from_i64(t))
        }
        (TIMESTAMPOID, DATEOID) => {
            let t = v.as_i64();
            let d = match t {
                i64::MIN => i32::MIN,
                i64::MAX => i32::MAX,
                _ => {
                    let (day, rest) = (t.div_euclid(USECS_PER_DAY), t.rem_euclid(USECS_PER_DAY));
                    let day = match s {
                        _ if rest == 0 => day,
                        1 | 4 => day + 1,
                        2 | 5 => day,
                        _ => return None,
                    };
                    i32::try_from(day).ok()?
                }
            };
            Some(Datum::from_i32(d))
        }
        _ if zone_class(from, to, s, v) == Some(true) => zoned(from, to, s, v),
        _ => None,
    }
}

const F_TEXTEQ: u32 = 67;
const F_TEXTNE: u32 = 157;
const F_TEXT_LEFT: u32 = 3060;
const F_TEXT_RIGHT: u32 = 3061;
const F_TEXTREGEXEQ: u32 = 1254;
const F_TEXTREGEXNE: u32 = 1256;
/// starts_with (the `^@` operator's procedure) and the textpos functions strpos / position: the plan side's names for
/// gravity_program::twin's spellings (the program side's are gravity_fn typesem's `starts_with` / `text_position`; exec's
/// `twin_homes_name_one_list` pins the two equal).
pub const F_STARTS_WITH: u32 = 3696;
pub const F_TEXTPOS: [u32; 2] = [868, 849]; // strpos, position
const OP_TEXTLIKE: (u32, u32) = (1209, 850);
const OP_TEXTNLIKE: (u32, u32) = (1210, 851);

/// `like_twin`'s answer: `text [NOT] LIKE pattern` under `coll`, the pattern built from the plan text constant `needle`
/// (`twin_pattern` with `head` / `tail`).
struct Twin<'p> {
    text: Node<'p>,
    needle: Node<'p>,
    pattern: Vec<u8>,
    negated: bool,
    head: bool,
    tail: bool,
    coll: u32,
}

/// A two-argument call (function or operator): its function, input collation and arguments.
fn call2(n: Node<'_>) -> Option<(u32, u32, Node<'_>, Node<'_>)> {
    let (f, coll, args) = match (n.as_func_expr(), n.as_op_expr()) {
        (Some(f), _) => (f.funcid, f.inputcollid, &f.args),
        (_, Some(o)) => (o.opfuncid, o.inputcollid, &o.args),
        _ => return None,
    };
    (args.len() == 2).then(|| (f, coll, args.nth(0), args.nth(1)))
}

/// `like_twin`'s match of the call `f(a, b)` (input collation `coll`) without the server's facts: (text, needle, collation
/// of the search, prefix '%', suffix '%', negated). Shapes are matched with the constant on the right (`commute` has run
/// below). The needle's image is read through the bake log (the pattern the content key hashes is built from it, so a
/// filed variant carries it); the by-value length and strpos constant the front key holds (`twin_class`).
fn twin_of<'p>(f: u32, coll: u32, a: Node<'p>, b: Node<'p>) -> Option<(Node<'p>, Node<'p>, u32, bool, bool, bool)> {
    let t = if f == F_STARTS_WITH {
        let (head, tail, negated) = twin_spelling(true, f, None, None)?;
        (a, b, coll, head, tail, negated)
    } else if f == F_TEXTREGEXEQ || f == F_TEXTREGEXNE {
        let lit = crate::expr::const_bytes(b.as_const()?)?;
        if lit.iter().any(|c| b".[]()*+?{}|^$\\".contains(c)) {
            return None;
        }
        (a, b, coll, true, true, f == F_TEXTREGEXNE)
    } else if f == F_TEXTEQ || f == F_TEXTNE {
        let (g, _, x, len) = call2(a)?;
        let c = crate::expr::const_bytes(b.as_const()?)?;
        let len = len.as_const().filter(|k| k.consttype == types_core::INT4OID && !k.constisnull)?;
        let chars = c.iter().filter(|&&x| x & 0xC0 != 0x80).count();
        if usize::try_from(len.constvalue.as_i32()).ok() != Some(chars) {
            return None;
        }
        match g {
            F_TEXT_LEFT => (x, b, coll, false, true, f == F_TEXTNE),
            F_TEXT_RIGHT => (x, b, coll, true, false, f == F_TEXTNE),
            _ => return None,
        }
    } else {
        let k = b.as_const().filter(|k| k.consttype == types_core::INT4OID && !k.constisnull)?;
        let (_, scoll, x, needle) = call2(a).filter(|c| F_TEXTPOS.contains(&c.0))?;
        let (head, tail, negated) = twin_spelling(false, f, Some(true), Some(k.constvalue.as_i32()))?;
        (x, needle, scoll, head, tail, negated)
    };
    t.1.as_const().filter(|c| !c.constisnull && c.consttype == crate::catalog::TEXTOID)?;
    Some(t)
}

/// `like_twin`'s match.
fn like_twin<'p>(facts: &dyn Facts, n: Node<'p>) -> Option<Twin<'p>> {
    let (f, coll, a, b) = call2(n)?;
    let (text, needle, coll, head, tail, negated) = twin_of(f, coll, a, b)?;
    if coll == 0 || !facts.coll(coll).bytes_eq || !facts.database_utf8() {
        return None;
    }
    let pattern = twin_pattern(crate::expr::const_bytes(needle.as_const()?)?, head, tail);
    Some(Twin { text, needle, pattern, negated, head, tail, coll })
}

/// A front hit's `like_twin` pattern for plan text constant `needle` (exec inputs/front.rs, `Derived::Twin`): the
/// constant `like` builds, under collation `coll`; `None` for a NULL or non-text needle, or out of memory.
pub fn twin_const<'p>(mcx: mcx::Mcx<'p>, needle: &Const, head: bool, tail: bool, coll: u32) -> Option<Node<'p>> {
    text_const(mcx, &twin_pattern(crate::expr::const_bytes_keyed(needle)?, head, tail), coll).ok()
}

/// The front key's class of rule `like_twin` for plan node `n` (exec inputs/front.rs keys it, as `retype_class`), in the
/// form `rewrite` hands `like_twin` (`commute`'s when it fires), from its by-value constants only: `left` / `right`'s
/// length, and whether a strpos / position compare is a membership test (`textpos_test`); `None` when its shape never
/// matches. Every other input `like_twin` reads is the needle's image, which it logs (a variant carries it), the plan's
/// shape, or a collation and database fact that follows `inputcollid` and the database.
pub fn twin_class(n: Node<'_>) -> Option<u64> {
    twin_class_at(n, commute(n))
}

/// `twin_class` of plan node `n` given its `commute` match `com`.
fn twin_class_at<'p>(n: Node<'p>, com: Commuted<'p>) -> Option<u64> {
    let (f, _, a, b) = match com {
        Some((o, _, code)) => (code, o.inputcollid, o.args.nth(1), o.args.nth(0)),
        None => call2(n)?,
    };
    if [F_STARTS_WITH, F_TEXTREGEXEQ, F_TEXTREGEXNE].contains(&f) {
        return Some(1);
    }
    let int4 = |x: Node<'_>| x.as_const().filter(|k| k.consttype == types_core::INT4OID).map(|k| (!k.constisnull).then(|| k.constvalue.as_i32()));
    if f == F_TEXTEQ || f == F_TEXTNE {
        let (g, _, _, len) = call2(a).filter(|c| c.0 == F_TEXT_LEFT || c.0 == F_TEXT_RIGHT)?;
        return Some(int4(len).map_or(2, |l| 3 | u64::from(l.map_or(u32::MAX, |l| l as u32)) << 8) | u64::from(g == F_TEXT_LEFT) << 4);
    }
    call2(a).filter(|c| F_TEXTPOS.contains(&c.0))?;
    let k = int4(b)?;
    Some(8 | k.map_or(0, |k| match textpos_test(f, k, true) { None => 16, Some(true) => 32, Some(false) => 64 }))
}

/// The twin as a node: `text [NOT] LIKE 'pattern'`, the pattern a text constant under the search's collation, noted as the
/// needle's twin (`Derived::Twin`) when `plain` (the rewritten node is the plan's, `twin_class`'s form).
fn like<'p>(mcx: mcx::Mcx<'p>, t: Twin<'p>, plain: bool) -> types_error::PgResult<Node<'p>> {
    let pat = text_const(mcx, &t.pattern, t.coll)?;
    if plain {
        crate::bake::derived_note(addr(pat), crate::bake::Derived::Twin { src: addr(t.needle), head: t.head, tail: t.tail });
    }
    let (opno, opfuncid) = if t.negated { OP_TEXTNLIKE } else { OP_TEXTLIKE };
    let args = NodeList::from_slice(mcx, &[t.text, pat])?;
    Node::mk(mcx, OpExpr { opno, opfuncid, opresulttype: types_core::BOOLOID, opretset: false, opcollid: 0, inputcollid: t.coll, args, location: -1 })
}

/// A text constant of `bytes`: a 4-byte-header varlena, copied as words so the image is aligned.
pub fn text_const<'p>(mcx: mcx::Mcx<'p>, bytes: &[u8], coll: u32) -> types_error::PgResult<Node<'p>> {
    let mut image = mcx::to_vec_in(&datum::varlena::set_varsize_4b(datum::varlena::VARHDRSZ + bytes.len())[..], mcx);
    image.extend_from_slice_in(bytes);
    image.resize_in(image.len().div_ceil(4) * 4, 0);
    let words: mcx::PgVec<'_, u32> = image.chunks(4).map(|w| u32::from_ne_bytes([w[0], w[1], w[2], w[3]])).collect_in(mcx);
    let image = mcx::slice_borrow_in(mcx, &words[..])?;
    Node::mk_const(mcx, crate::catalog::TEXTOID, -1, coll, -1, Datum::from_usize(image.as_ptr() as usize), false, false)
}

/// `like_any`'s match: `col [NOT] LIKE ANY (c)` / `col [NOT] LIKE ALL (c)` (PS4-11: every one of the four) with `c` a
/// constant one-dimensional text array of at least one element and no NULL element (no length cap: the OR / AND
/// evaluates the arms ANY / ALL does, in order); the array's bytes after its length word.
fn like_any<'p>(n: Node<'p>) -> Option<(&'p ScalarArrayOpExpr<'p>, &'p [u8])> {
    let s = n.as_scalar_array_op_expr().filter(|s| s.args.len() == 2)?;
    let op = (s.opno, s.opfuncid);
    if op != OP_TEXTLIKE && op != OP_TEXTNLIKE {
        return None;
    }
    let x = s.args.nth(0);
    x.as_relabel_type().map_or(x, |r| r.arg).as_var()?;
    let body = crate::expr::const_bytes(s.args.nth(1).as_const()?)?;
    let word = |i: usize| body.get(i * 4..i * 4 + 4).map(|w| i32::from_ne_bytes([w[0], w[1], w[2], w[3]]));
    // ArrayType after the length word: ndim, dataoffset (0: no NULL bitmap), elemtype, dims, lower bounds
    let n = usize::try_from(word(3)?).ok()?;
    let text = u32::try_from(word(2)?).ok() == Some(crate::catalog::TEXTOID);
    (word(0)? == 1 && word(1)? == 0 && text && n >= 1).then_some((s, body))
}

/// The arms of a `like_any` match: one `col [NOT] LIKE element` each, OR-ed (AND-ed for NOT LIKE ALL); none when an
/// element is not a plain inline text.
fn like_arms<'p>(mcx: mcx::Mcx<'p>, s: &'p ScalarArrayOpExpr<'p>, body: &[u8]) -> types_error::PgResult<Option<Node<'p>>> {
    let mut image = mcx::to_vec_in(&[0u8; 4][..], mcx);
    image.extend_from_slice_in(body);
    let (elems, _) = datum::array_build::deconstruct_array_image_nulls(mcx, &image[..], -1, false, b'i')?;
    let coll = s.args.nth(1).as_const().map_or(s.inputcollid, |c| c.constcollid);
    let mut arms = mcx::reserved_in(elems.len(), mcx);
    for e in elems.iter() {
        let Some(pattern) = image.get(e.as_usize().wrapping_sub(image.as_ptr() as usize)..).and_then(inline_payload) else {
            return Ok(None);
        };
        let args = NodeList::from_slice(mcx, &[s.args.nth(0), text_const(mcx, pattern, coll)?])?;
        let (opno, opfuncid) = (s.opno, s.opfuncid);
        arms.push_in(Node::mk(mcx, OpExpr { opno, opfuncid, opresulttype: BOOLOID, opretset: false, opcollid: 0, inputcollid: s.inputcollid, args, location: -1 })?);
    }
    if arms.len() == 1 {
        return Ok(arms.pop());
    }
    let boolop = if s.useOr { BoolExprType::OR_EXPR } else { BoolExprType::AND_EXPR };
    Ok(Some(Node::mk(mcx, BoolExpr { boolop, args: NodeList::from_slice(mcx, &arms[..])?, location: -1 })?))
}

/// The payload of an inline, uncompressed varlena at the start of `v` (1-byte or 4-byte header).
fn inline_payload(v: &[u8]) -> Option<&[u8]> {
    let b0 = *v.first()?;
    if b0 & 0x01 == 0x01 {
        (b0 != 0x01).then(|| v.get(1..usize::from(b0 >> 1)))?
    } else if b0 & 0x03 == 0 {
        let size = u32::from_ne_bytes(v.get(..4)?.try_into().ok()?) >> 2;
        v.get(4..usize::try_from(size).ok()?)
    } else {
        None
    }
}

const F_REGEXP_REPLACE3: u32 = 2284;
const F_REGEXP_REPLACE4: u32 = 2285;

/// `regexp3`'s match: a `regexp_replace(s, p, r, flags)` call whose flags make it the 3-argument form.
fn regexp3<'p>(n: Node<'p>) -> Option<&'p FuncExpr<'p>> {
    let f = n.as_func_expr().filter(|f| f.funcid == F_REGEXP_REPLACE4 && f.args.len() == 4)?;
    let text = |x: Node<'p>| x.as_const().filter(|c| !c.constisnull && c.consttype == crate::catalog::TEXTOID).and_then(crate::expr::const_bytes);
    let anchored = || text(f.args.nth(1)).is_some_and(|p| p.first() == Some(&b'^') && !p.contains(&b'|'));
    match text(f.args.nth(3))? {
        b"" => Some(f),
        b"g" if anchored() => Some(f),
        _ => None,
    }
}

/// The three arguments of a `regexp_replace` call in its 3-argument form, or of one `regexp3` rewrites to it (a reader
/// that has the expression before `graph` ran, such as a grouping key).
pub fn regexp_replace3<'p>(n: Node<'p>) -> Option<[Node<'p>; 3]> {
    let f = n.as_func_expr().filter(|f| (f.funcid == F_REGEXP_REPLACE3 && f.args.len() == 3) || regexp3(n).is_some())?;
    Some([f.args.nth(0), f.args.nth(1), f.args.nth(2)])
}

/// The four arguments of a `regexp_replace(s, p, r, flags)` call (a reader of a flags form `regexp3` leaves, such as
/// lowerv4's derived key for flag 'i').
pub fn regexp_replace4<'p>(n: Node<'p>) -> Option<[Node<'p>; 4]> {
    let f = n.as_func_expr().filter(|f| f.funcid == F_REGEXP_REPLACE4 && f.args.len() == 4)?;
    Some([f.args.nth(0), f.args.nth(1), f.args.nth(2), f.args.nth(3)])
}

/// Whether `n` is already in the form `expr` returns (an adopter's `debug_assert` before it reads the expression).
pub fn is_canonical(facts: &dyn Facts, n: Node<'_>) -> bool {
    crate::walk::stack_check(); // D3748
    let mut ok = commute(n).is_none() && identity(n).is_none() && product(n).is_none() && retype(n).is_none() && like_twin(facts, n).is_none() && like_any(n).is_none() && regexp3(n).is_none() && simple_case(facts, n).is_none();
    crate::expr::kids(n, &mut |k| ok &= is_canonical(facts, k));
    ok
}

/// Rule `count_star` (landing 6): `count(x)` (2147) over an `x` never null where the aggregate reads it, without
/// DISTINCT, counts every row: `count(*)` (2803). A FILTER only picks rows and count ignores its ORDER BY, so both stay.
/// The caller answers "never null" (agg: a NOT NULL column of the scan the aggregate reads directly), so a column on
/// the nullable side of an outer join never qualifies.
pub fn count_star(fnoid: u32, distinct: bool, never_null: bool) -> Option<u32> {
    (fnoid == F_COUNT_ANY && !distinct && never_null).then_some(F_COUNT_STAR)
}

pub(crate) const F_COUNT_ANY: u32 = 2147;
const F_COUNT_STAR: u32 = 2803;

