//! The per-row SubPlan's descriptors other families read.
use gravity_program::ArenaBox;
use types_nodes::list::{IntList, NodeList};
use types_nodes::{Node, NodeTag};

use crate::expr;
use crate::graph::PipelineNode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubLink {
    Exists,
    All,
    Any,
    RowCompare,
    Expr,
    MultiExpr,
    Array,
    Cte,
}

/// How a per-row subplan reaches its rows: a nested pipeline re-run per outer row (`inner`, its
/// `params` bound from `args`), or an identity set built once (`Build` on the producer, `Hashed`
/// on the consumer: `useHashTable`, the `IN (SELECT …)` that stayed a SubPlan).
pub enum SubForm<'p> {
    Nested { inner: ArenaBox<PipelineNode<'p>> },
    Build,
    Hashed { slot: u8 },
}

/// Where a per-row SubPlan is evaluated: on the pipeline's row before its first reader (`Row`); inside the join
/// op that follows it, per candidate pair — its `args` and `test` read `inner.*` as the join's quals do
/// (`JoinQual`: `IN` / `EXISTS` under an outer join's `ON`; the mark-join rewrite is the planner's, decision 60);
/// on the input row of the aggregate or window op that follows it, inside a function's argument or FILTER
/// (`AggArg`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubSite {
    Row,
    JoinQual,
    AggArg,
    /// Inside a VALUES row's expressions: evaluated by the `Source::Values` row (decision 80).
    Values,
}

pub struct SubPlanDesc<'p> {
    pub plan_id: i32,
    pub link: SubLink,
    pub test: Option<Node<'p>>,
    pub args: &'p NodeList<'p>,
    pub params: &'p IntList<'p>,
    /// A MULTIEXPR's output parameters (`setParam`): set per row, read by the target entries after it.
    pub sets: &'p IntList<'p>,
    /// The subplan's output columns as its test reads them (PostgreSQL's `paramIds`: column i + 1 = `ids[i]`), set from each
    /// inner row (`ExecScanSubPlan`) or the hashed row (`ExecHashSubPlan`); M8-SUB's producer of those params.
    pub ids: &'p IntList<'p>,
    pub site: SubSite,
    /// FACTS-1a: PostgreSQL may skip this SubPlan for a row that reaches its op, the interpreter
    /// stopping before the node (`ExecEvalSubPlan` runs only when reached): past the first WHEN test of a CASE, the first
    /// argument of a COALESCE / AND / OR, the first column of a row comparison, inside a FILTERed aggregate's or window
    /// function's arguments, in a join qual past its first conjunct, or in a MERGE action's WHEN qual or target list (an
    /// action runs only for the rows of its kind no earlier action took). Evaluating it for every row would raise an error
    /// PostgreSQL never raises (error-vs-value) and call a volatile subplan extra times. A conjunct of a plan's own qual
    /// is not conditional: each conjunct is its own `Op::Filter`, in order, and the SubPlan op sits right before its first
    /// reader, so it runs on exactly the rows that passed the conjuncts before it (`ExecQual`). A SubPlan met in several
    /// places is conditional only when every place is.
    pub conditional: bool,
    /// FACTS-1b F5 (`rescan.rs`) over the SubPlan's plan, its test and its args: the most volatile call, and whether any of
    /// it can raise (a road keeps results by param values only when not volatile, and runs one early only when raise-free).
    pub volatility: crate::effects::Volatility,
    pub raise_free: bool,
    /// D3374 (effects/found.rs `sub_plan_quiet_but_rows`): running it has no effect (no user function, nothing VOLATILE, no row lock) and may
    /// raise nothing but its own sublink's second row, which lowerv4 holds for an interpreter island's read.
    pub quiet_but_rows: bool,
    /// FACTS-1b F1 (`rescan::invariant`): the nested form's plan returns the same rows every run (it reads no param that
    /// changes, nothing in it is volatile or rescan-unstable); false for the hashed forms.
    pub invariant: bool,
    pub unknown_eq_false: bool,
    pub alternative: bool,
    /// D1911 G5: an aggregate's argument D1061's grouping-sets spool evaluates under a hashed or mixed head (agg.rs `Spool::phase_walk`).
    pub set_spool: bool,
    /// D2115 (an aggregate's argument): a LIMIT counting 0 or a parameter somewhere in the statement may stop PostgreSQL
    /// before it reads any node whole (`stops`), so a walk that computes every input row's arguments ahead (a sorted
    /// grouping-sets phase's spool) may do work PG never does; read in lowerv4 subplan/per_row.rs `order`.
    pub stops: bool,
    /// Construct C2: the licence this SubPlan's evaluation ahead of PostgreSQL's demand holds, where its place runs it
    /// ahead: a window argument's or FILTER's SubPlan evaluated per stored row by the partition fill, ahead of the
    /// frame reads (ops/subplan/mod.rs `attach_window_args`); a key value's SubPlan facts/hoist.rs moves onto a nest's
    /// outer row. lowerv4 ops/subplan/per_row.rs stamps it on the SubPlan's own pipelines for the run-ahead tracer
    /// (`Px::hold`; lint RH checks that the hold stays). `None` at every other place, for a hoisted value whose licence
    /// is denied, and on the fill for the window's one acting exact SubPlan (PostgreSQL's own calls: lowerv4
    /// window/lower.rs `admit` holds the fill's whole-read licence) and a quiet one whose rows may differ run to run
    /// (evaluated once per row where PostgreSQL evaluates it at each frame read: a fewer-run departure, which
    /// `effects::unobservable` grants only to `Effects::NONE`).
    pub ahead: Option<crate::effects::Licence>,
    /// D4137 (`rerun::of_subplan_stored`): the facts of its call (plan, test and args) with a data-modifying CTE its plan
    /// reads taken as its store, PostgreSQL running that CTE to its end whether or not the call runs (`Effects::of_sub_plan_call`).
    /// The one SubPlan answer's facts (D4144): read through `answer` (its acts, user code and a call whose place shows).
    pub call: crate::effects::found::Found,
    pub form: SubForm<'p>,
}

pub fn refers(n: Node<'_>, plan_id: i32) -> bool {
    crate::walk::stack_check();
    // D1112: an implicit-AND qual list (a check option's planned qual, `preprocess_expression`'s EXPRKIND_QUAL): its
    // clauses (`expr::walk` has no List arm)
    if let Some(l) = n.as_list() {
        return refers_list(l, plan_id);
    }
    let mut found = false;
    walk_ret(n, &mut |x| {
        if let Some(s) = x.as_sub_plan() {
            found |= s.plan_id == plan_id;
        }
        !found
    });
    found
}

pub fn refers_list(l: &NodeList<'_>, plan_id: i32) -> bool {
    l.iter().any(|n| refers(n, plan_id))
}

/// D1112 (RETURNING over a view): `expr::walk`, opening PG 18's `ReturningExpr` (the rewriter's wrapper of a
/// view's or a virtual generated column's OLD / NEW expression in a RETURNING list, e.g. `old.d` of a view column
/// `(SELECT concat('b: ', b)) AS d`) to its expression, as gravity_expr compiles it (misc.rs `compile_returning`): a
/// SubPlan inside is read by the RETURNING projection, so its op is placed before that projection like any other.
pub(crate) fn walk_ret<'p>(node: Node<'p>, f: &mut dyn FnMut(Node<'p>) -> bool) -> bool {
    crate::walk::stack_check(); // D3748
    if !f(node) {
        return false;
    }
    if let Some(r) = node.as_returning_expr() {
        return walk_ret(r.retexpr, f);
    }
    let mut ok = true;
    let known = expr::kids(node, &mut |k| ok &= walk_ret(k, f));
    ok && known
}

/// `expr::walk` with the conditional context (`SubPlanDesc::conditional`): `f(node, cond)` and, when it returns true, the
/// node's children, each conditional when `cond` is or when PostgreSQL's interpreter can stop before it.
pub fn walk_cond<'p>(x: Node<'p>, cond: bool, f: &mut dyn FnMut(Node<'p>, bool) -> bool) {
    crate::walk::stack_check(); // D3748
    if !f(x, cond) {
        return;
    }
    let past_first = |l: &NodeList<'p>, f: &mut dyn FnMut(Node<'p>, bool) -> bool| {
        for (i, c) in l.iter().enumerate() {
            walk_cond(c, cond || i > 0, f);
        }
    };
    match x.node_tag() {
        // AND / OR stop at the deciding argument (NOT has one)
        NodeTag::T_BoolExpr => past_first(&x.as_bool_expr().unwrap().args, f),
        // COALESCE stops at the first non-NULL argument
        NodeTag::T_CoalesceExpr => past_first(&x.as_coalesce_expr().unwrap().args, f),
        // a row comparison stops at the first column pair that is not equal (`EEOP_ROWCOMPARE_STEP`)
        NodeTag::T_RowCompareExpr => {
            let r = x.as_row_compare_expr().unwrap();
            past_first(&r.largs, f);
            past_first(&r.rargs, f);
        }
        // CASE: its test argument and the first WHEN's condition always run; every result, later condition and the ELSE
        // only on the rows that reach them
        NodeTag::T_CaseExpr => {
            let c = x.as_case_expr().unwrap();
            if let Some(a) = c.arg {
                walk_cond(a, cond, f);
            }
            for (i, w) in c.args.iter().enumerate() {
                match w.as_case_when().filter(|_| i == 0) {
                    Some(w) => {
                        if let Some(e) = w.expr {
                            walk_cond(e, cond, f);
                        }
                        if let Some(r) = w.result {
                            walk_cond(r, true, f);
                        }
                    }
                    None => walk_cond(w, true, f),
                }
            }
            if let Some(d) = c.defresult {
                walk_cond(d, true, f);
            }
        }
        NodeTag::T_Aggref | NodeTag::T_WindowFunc => agg_kids(x, cond, &mut |c, cond| walk_cond(c, cond, f)),
        // D3374: JSON_VALUE / JSON_QUERY / JSON_EXISTS run their ON EMPTY / ON ERROR behavior's expression only for the rows
        // whose path finds nothing / raises (ExecEvalJsonExprPath jumps to it); the context item, path and PASSING always run
        NodeTag::T_JsonExpr => {
            let j = x.as_json_expr().unwrap();
            for n in [j.formatted_expr, j.path_spec].into_iter().flatten().chain(j.passing_values.iter()) {
                walk_cond(n, cond, f);
            }
            for b in [j.on_empty, j.on_error].into_iter().flatten() {
                walk_cond(b, true, f);
            }
        }
        // D1112: RETURNING's OLD / NEW image of a view column runs its expression only for a row whose image
        // exists (ExecEvalReturningExpr: EEOP_RETURNINGEXPR yields NULL and jumps past it when the image is NULL: a MERGE
        // INSERT's OLD, a DELETE's NEW)
        NodeTag::T_ReturningExpr => walk_cond(x.as_returning_expr().unwrap().retexpr, true, f),
        _ => {
            expr::kids(x, &mut |c| walk_cond(c, cond, f));
        }
    }
}

/// An aggregate's or window function's children: with a FILTER, the arguments (and an aggregate's ORDER BY / DISTINCT
/// expressions) run only on the rows the filter passes; the filter and the direct arguments on every row.
pub(crate) fn agg_kids<'p>(x: Node<'p>, cond: bool, f: &mut dyn FnMut(Node<'p>, bool)) {
    if let Some(a) = x.as_aggref() {
        let args_cond = cond || a.aggfilter.is_some();
        a.aggdirectargs.iter().for_each(|c| f(c, cond));
        a.args.iter().chain(a.aggorder.iter()).chain(a.aggdistinct.iter()).for_each(|c| f(c, args_cond));
        if let Some(q) = a.aggfilter {
            f(q, cond);
        }
    } else if let Some(w) = x.as_window_func() {
        w.args.iter().for_each(|c| f(c, cond || w.aggfilter.is_some()));
        if let Some(q) = w.aggfilter {
            f(q, cond);
        }
    }
}
