//! The readers (v5 interfaces §2.2, as corrected by proto1 and `specs/semantics.md` design fixes 1 and 9): every question
//! about whether code acts, may raise or returns the same rows is one of these calls.
//!
//! - `acts` and `same_rows` come from one walk over the whole expression or subtree (`crate::rerun`'s F5 walk): every node
//!   that calls a function (FuncExpr, OpExpr, ScalarArrayOpExpr, DistinctExpr, NullIfExpr, RowCompareExpr, CoerceViaIO's
//!   I/O functions, an aggregate, a window function, a sort / group operator), the plans of the SubPlans it calls, and
//!   any node the walk does not know. A call acts when its `provolatile` is VOLATILE or the catalog cannot answer it (fail
//!   closed); STABLE and IMMUTABLE calls never act (`DECISIONS.md` 3 (C1), ruled 2026-10-01 17:32Z: they may run any
//!   number of times, at any time; only their errors are judged), whoever wrote them.
//! - `may_raise` of an expression is the hash join's quiet rule (rtfilter's, moved here by proto1): a whitelist of nodes
//!   and calls PostgreSQL 18.6 evaluates without raising, so a node it does not list may raise. A plan subtree's is the
//!   walk's (`Found::raise_free`).
use datum::Datum;
use gravity_program::ArenaRef;
use types_nodes::list::NodeList;
use types_nodes::plannodes::PlannedStmt;
use types_nodes::primnodes::{Aggref, Const, ParamKind, AGGKIND_NORMAL};
use types_nodes::{CmdType, Node};

use super::{Effects, Parallel, Volatility};
use crate::catalog::Facts;
use crate::effects::found::Found;
use crate::catalog::typefacts::FIRST_NORMAL_OID;

/// What every reader needs: a SubPlan reaches its plan through `stmt.subplans`, so the catalog alone cannot follow it.
#[derive(Clone, Copy)]
pub struct Cx<'c, 'p> {
    /// None: a reader with no statement (lowering's sites hold the graph, not the plan): a SubPlan is then unknown.
    pub stmt: Option<&'p PlannedStmt<'p>>,
    pub cat: &'c dyn Facts,
    pub arena: ArenaRef,
}

impl<'c, 'p> Cx<'c, 'p> {
    pub fn new(stmt: &'p PlannedStmt<'p>, cat: &'c dyn Facts, arena: ArenaRef) -> Cx<'c, 'p> {
        Cx { stmt: Some(stmt), cat, arena }
    }

    /// A reader with no statement (lowering's sites, which hold the graph but not the plan): every SubPlan's plan is then
    /// out of reach, so a SubPlan is unknown and acts (fail closed); a plan subtree or the statement is unknown.
    pub fn without_stmt(cat: &'c dyn Facts, arena: ArenaRef) -> Cx<'c, 'p> {
        Cx { stmt: None, cat, arena }
    }

    fn walk(&self, n: Node<'p>) -> Found {
        match self.stmt {
            Some(st) => crate::rerun::of_expr(st, self.cat, self.arena, n),
            None => crate::rerun::of_expr_alone(self.cat, self.arena, n),
        }
    }
}

/// What decompose cannot know: the PARAM_EXEC ids the owner binds before the code runs (read as bound words: D1902's
/// `side_by`), and lowering's test for a true array's element read, which PostgreSQL 18.6 makes without raising
/// (`quiet_fetch`; `None`: not taken).
pub struct Scope<'s> {
    pub bound: &'s dyn Fn(i32) -> bool,
    pub fetch: Option<&'s dyn Fn(Node<'_>) -> bool>,
}

fn unbound(_: i32) -> bool {
    false
}

/// No PARAM_EXEC bound, no element read taken.
pub const PLAIN: Scope<'static> = Scope { bound: &unbound, fetch: None };

impl Effects {
    /// A walk's facts: a VOLATILE or unknown call acts; code that acts does not return the same rows.
    pub fn of_found(w: Found) -> Effects {
        let acts = w.volatility == Volatility::Volatile;
        Effects { acts, may_raise: !w.raise_free, same_rows: w.rescan_stable && !acts }
    }

    /// An InitPlan's own facts (graph.rs `InitPlan`: its plan's most volatile call and whether it may raise), as `of_found`.
    pub fn of_init_plan(i: &crate::graph::InitPlan<'_>) -> Effects {
        let acts = i.volatility == Volatility::Volatile;
        Effects { acts, may_raise: !i.raise_free, same_rows: !acts }
    }
}

/// Expression `n` (a SubPlan in it counts its plan).
pub fn of_expr<'p>(cx: &Cx<'_, 'p>, n: Node<'p>, sc: &Scope<'_>) -> Effects {
    let w = Effects::of_found(cx.walk(n));
    Effects { may_raise: !quiet(cx.cat, n, sc), ..w }
}

/// Node `n` alone, its children apart, by the rerun walk's per-node rules (`crate::rerun::of_node`): it acts when it calls a
/// VOLATILE function or one the catalog cannot answer, and may raise unless every call it makes is raise-free and strict
/// and the node itself cannot raise; a SubPlan counts its plan (unknown with no statement). A node the walk does not know
/// acts and may raise (fail closed). For a reader that visits the children itself, in PostgreSQL's evaluation order;
/// folding it over every node of an expression gives the walk's answer for the whole.
pub fn of_node<'p>(cx: &Cx<'_, 'p>, n: Node<'p>) -> Effects {
    Effects::of_found(crate::rerun::of_node(cx.stmt, cx.cat, cx.arena, n))
}

pub fn of_list<'p>(cx: &Cx<'_, 'p>, l: &'p NodeList<'p>, sc: &Scope<'_>) -> Effects {
    l.iter().fold(Effects::NONE, |e, n| e.and(of_expr(cx, n, sc)))
}

/// A list of ops: filters, gates and projections; a join by its sides (`of_join`); any other op is unknown. The family
/// folds over its own feeders.
pub fn of_ops<'p>(cx: &Cx<'_, 'p>, ops: &[crate::graph::Op<'p>], sc: &Scope<'_>) -> Effects {
    crate::walk::stack_check();
    use crate::graph::Op;
    ops.iter().fold(Effects::NONE, |e, op| {
        e.and(match op {
            Op::Filter(n) | Op::Gate(n) => of_expr(cx, *n, sc),
            Op::Project(l) => of_list(cx, l, sc),
            Op::Join(j) => of_join(cx, j, sc),
            _ => Effects::UNKNOWN,
        })
    })
}

/// aggtrans T6j (D1371 over a join feed, main 21:2xZ): a join op from its sides; the outer side is the pipeline the op is
/// in, which the caller reads. A nested loop runs, per outer row, the params it passes (the outer's values), its inner
/// pipeline (the inner's source by `of_source`, its ops by `of_ops`: a nested loop in it by its own sides), the params
/// bound there, and its join qual per pair. A merge join compares by its clauses' btree families (an initdb family's
/// support functions; another's are a user's: unknown) and runs its clauses and join qual per pair; its inner is a run
/// another pipeline spooled (decompose `sort::spool_run`), which the caller reads as that pipeline's. Any other join form
/// (a hash probe, a drain, a cache, a spool) is unknown.
pub fn of_join<'p>(cx: &Cx<'_, 'p>, j: &crate::join::JoinDesc<'p>, sc: &Scope<'_>) -> Effects {
    if let crate::join::JoinDesc::Merge { clauses, families, qual, .. } = j {
        let compare = if families.iter().all(|&f| f != 0 && f < FIRST_NORMAL_OID) { Effects::NONE } else { Effects::UNKNOWN };
        return compare.and(of_list(cx, clauses, sc)).and(of_list(cx, qual, sc));
    }
    let crate::join::JoinDesc::Nest { params, inner, qual, .. } = j else { return Effects::UNKNOWN };
    let passed = params.iter().fold(Effects::NONE, |e, p| e.and(p.as_nest_loop_param().map_or(Effects::UNKNOWN, |p| of_expr(cx, p.paramval, sc))));
    let bound = |id: i32| (sc.bound)(id) || params.iter().filter_map(|p| p.as_nest_loop_param()).any(|p| p.paramno == id);
    let isc = Scope { bound: &bound, fetch: sc.fetch };
    passed.and(of_source(cx, &inner.source, &isc)).and(of_ops(cx, &inner.ops, &isc)).and(of_list(cx, qual, &isc))
}

/// aggtrans T6j: the calls a pipeline's source makes per row: none for a table scan or a constant (its quals are filter
/// ops); an index scan's (T6b, here from lowerv4's fold_ahead) operator classes and key operators (`of_index`), key values
/// and recheck quals; a bitmap heap scan's, each index probe's the same way and its recheck quals; any other is unknown.
pub fn of_source<'p>(cx: &Cx<'_, 'p>, s: &crate::graph::Source<'p>, sc: &Scope<'_>) -> Effects {
    use crate::graph::{IndexKey, KeyForm, Source};
    use crate::ops::shared::scan::BitmapPlan;
    let keyed = |indexid: u32, keys: &[IndexKey<'p>]| {
        let opnos = keys.iter().filter(|x| !matches!(x.form, KeyForm::Null | KeyForm::NotNull)).map(|x| x.opno); // a row compare's value walks its members
        keys.iter().fold(super::of_index(cx.cat, indexid, opnos), |e, x| e.and(of_expr(cx, x.value, sc)))
    };
    fn probes<'p>(p: &BitmapPlan<'p>, keyed: &dyn Fn(u32, &[IndexKey<'p>]) -> Effects) -> Effects {
        crate::walk::stack_check(); // D3748
        match p {
            BitmapPlan::Index { indexid, keys, .. } => keyed(*indexid, keys),
            BitmapPlan::And { plans, .. } | BitmapPlan::Or { plans, .. } => plans.iter().fold(Effects::NONE, |e, q| e.and(probes(q, keyed))),
        }
    }
    match s {
        Source::Range { .. } | Source::Const => Effects::NONE,
        Source::Index { indexid, keys, recheck, .. } => keyed(*indexid, keys).and(of_list(cx, recheck, sc)),
        Source::Bitmap { plan, recheck, .. } => probes(plan, &keyed).and(of_list(cx, recheck, sc)),
        _ => Effects::UNKNOWN,
    }
}

/// Plan subtree `root`, with the InitPlans, SubPlans and CTEs it reads.
pub fn of_plan<'p>(cx: &Cx<'_, 'p>, root: Node<'p>) -> Effects {
    cx.stmt.map_or(Effects::UNKNOWN, |st| Effects::of_found(crate::rerun::of_plan(st, cx.cat, cx.arena, root)))
}

/// The statement's acting set: what a re-run of the whole statement would repeat. A write, a row lock or a modifying CTE
/// acts; then the plan tree and every subplan, where a call acts unless it is a pinned non-acting builtin (D3816,
/// `Found::acts`: a disk size or the lock status reads again alike, as a re-run's own calls do). Every other reading of a
/// walk keeps `of_found`'s (a VOLATILE call acts).
pub fn of_statement(cx: &Cx<'_, '_>) -> Effects {
    let Some(s) = cx.stmt else { return Effects::UNKNOWN };
    let write = s.commandType != CmdType::CMD_SELECT || s.hasModifyingCTE || s.rowMarks.iter().next().is_some();
    let plans = s.planTree.into_iter().chain(s.subplans.iter().flatten());
    let of = |r| {
        let f = crate::rerun::of_plan(s, cx.cat, cx.arena, r);
        Effects { acts: f.acts, ..Effects::of_found(f) }
    };
    plans.fold(if write { Effects::ACTS } else { Effects::NONE }, |e, r| e.and(of(r)))
}

/// `of_statement`'s `acts`, walked once per statement: `memo` keeps it (the graph's `PipelineGraph::acts_read`, D4554),
/// so mapping's askers (agg `held_whole`, limit, sort, gsets, self_spool) and lib.rs `acting` share one F5 walk of the
/// statement where each walked its own. The same statement, facts and arena give the same answer.
pub fn of_statement_acts(cx: &Cx<'_, '_>, memo: &mut Option<bool>) -> bool {
    *memo.get_or_insert_with(|| of_statement(cx).acts)
}

/// C2, the held-error allowlist: proof that the code a held error covers acts only through
/// call-local builtins (`types_core::quiet::CALL_LOCAL_ACT`: a fresh value per call, no state another call reads) in a
/// statement where no other call can see when they ran. Only `call_local` makes one; `licence::held_error` takes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallLocal {
    _only_effects: (),
}

/// `unit`, the code a held error covers (expressions, or plan subtrees with what they read), by the F5 walk: its acts
/// are all call-local, and the statement lets them run ahead of PostgreSQL's point. The statement must be there (fail
/// closed without) and write nothing (a trigger, rule or default may read what a call-local act moved); and nowhere in
/// it a user function or a call that runs code named at run time (SPI may read currval / lastval), nor a
/// `CALL_LOCAL_STATE` call (its read or set would see the act at another moment). `None` otherwise.
///
/// A nextval elsewhere in the statement, of the same sequence or another, does not deny: a held unit runs whole, as
/// PostgreSQL runs it (a full Sort's fill), so its own calls keep PostgreSQL's order (and its count where PostgreSQL
/// runs the fill), and only their place against the calls outside the unit moves, which a commuted merge join or a hash
/// join moves too under plan-only settings (`DECISIONS.md` 6b: a VOLATILE call order PostgreSQL's own plan decides).
/// Which sequence a call names is a literal the program binds per execution, so no rule here may read it.
pub fn call_local<'p>(cx: &Cx<'_, 'p>, unit: impl IntoIterator<Item = Node<'p>>) -> Option<CallLocal> {
    let s = cx.stmt?;
    let walk = |n: Node<'p>| if n.as_plan().is_some() { crate::rerun::of_plan(s, cx.cat, cx.arena, n) } else { crate::rerun::of_expr(s, cx.cat, cx.arena, n) };
    let mut own = Found::CLEAN;
    unit.into_iter().for_each(|n| own.add(walk(n)));
    let write = s.commandType != CmdType::CMD_SELECT || s.hasModifyingCTE;
    let all = crate::rerun::of_all_plans(s, cx.cat, cx.arena);
    local_in(own, (!write).then_some(all))
}

/// `call_local`'s rule over the unit's facts and its statement's (`None`: no statement, or one that writes). The
/// statement's facts are one walk (`rerun::of_all_plans`): a SubPlan's or a CTE's plan is walked once, whether its call
/// or the statement's subplans reach it.
pub(crate) fn local_in(unit: Found, stmt: Option<Found>) -> Option<CallLocal> {
    let all = stmt?;
    (!unit.acts_past_call && !unit.user_call && !all.user_call && !all.local_state).then_some(CallLocal { _only_effects: () })
}

/// Parallel safety, apart from `Effects` so the licence keeps its three inputs: every call `proparallel = 's'` and known,
/// every scanned relation known and not temporary.
pub fn pool_safe<'p>(cx: &Cx<'_, 'p>, n: Node<'p>) -> bool {
    cx.walk(n).parallel == Parallel::Safe
}

/// One call of function `funcid`, its arguments apart: it acts when VOLATILE or the catalog cannot answer it (fail
/// closed); it may raise unless `quiet_call`.
pub fn of_call(f: &dyn Facts, funcid: u32) -> Effects {
    let acts = f.func(funcid).is_none_or(|p| p.volatility == Volatility::Volatile);
    Effects { acts, may_raise: !quiet_call(f, funcid), same_rows: !acts }
}

/// D3816: the acting set's one answer to "a call of `funcid` acts" (a re-run of the statement repeats what it does):
/// VOLATILE, or a function the facts cannot answer, but not a pinned non-acting builtin (`types_core::quiet::non_acting`:
/// a disk size, the lock status), which writes and holds nothing once it returns. Only the acting set reads it: F5's
/// `Found::acts` (`of_statement`) and lowerv4's acting reading (volatile.rs `Call::Acting`, `Px::run_ahead`'s side rule).
/// `of_call(..).acts` and `Effects::acts` stay VOLATILE for the pinned builtins (the D3816 ruling: what may cache, skip a
/// rerun or run once reads them as VOLATILE), so neither stands for this one.
pub fn call_acts(f: &dyn Facts, funcid: u32) -> bool {
    types_core::quiet::non_acting(funcid).is_none() && f.func(funcid).is_none_or(|p| p.volatility == Volatility::Volatile)
}

/// D2044: a call of `funcid` is user code (a function past FIRST_NORMAL_OID), whose raise can leave transaction state the
/// statement cannot hold until the abort: PL/pgSQL returns its error before its SPI_finish (its SPI connection stays
/// pushed), a SET clause keeps its GUC level raised, a SQL function its snapshot. A pull runs such a call on no row past
/// its count (D439's Volatile protocol), and a fetch ends in its held error (exec runner `PlState`).
pub fn user_code(funcid: u32) -> bool {
    funcid >= crate::catalog::typefacts::FIRST_NORMAL_OID
}

/// D3923: builtin `funcid` runs code named at run time, which may be a user's (`types_core::quiet::DISPATCH`'s `Named`
/// rows: pg_input_is_valid / pg_input_error_info run a type's input function and its domain's CHECK, the *_to_xml family,
/// ts_rewrite and ts_stat run a query over the relation, cursor or text they are given).
pub fn runs_named_code(funcid: u32) -> bool {
    matches!(types_core::quiet::dispatch(funcid), Some(types_core::quiet::Reach::Named))
}

impl Effects {
    /// A SubPlan step's own facts (ops/subplan `SubPlanDesc`: F5 over its plan, test and arguments, the most volatile call
    /// and whether any of it may raise), read as `of_found` reads a walk's.
    pub fn of_sub_plan(d: &crate::ops::shared::subplan::SubPlanDesc<'_>) -> Effects {
        let acts = d.volatility == Volatility::Volatile;
        Effects { acts, may_raise: !d.raise_free, same_rows: !acts }
    }

    /// D4137: a SubPlan step's call (`SubPlanDesc::call`: its plan, test and args with a data-modifying CTE its plan reads
    /// taken as its store), read as `of_found` reads a walk's. PostgreSQL runs that CTE to its end whether or not the call
    /// runs (ExecPostprocessPlan), so its write is no act of the call's (D4051's reading of a Hash's build).
    pub fn of_sub_plan_call(d: &crate::ops::shared::subplan::SubPlanDesc<'_>) -> Effects {
        Effects::of_found(d.call)
    }
}

impl super::SubEffects {
    /// The one SubPlan answer (`SubPlanDesc::answer`): `Effects::of_sub_plan`'s raise and rows reading with its call's acts
    /// (`Effects::of_sub_plan_call`, a data-modifying CTE its plan reads taken as its store), and that call's user code and
    /// order flag (`SubPlanDesc::call`).
    pub fn of_sub_plan(d: &crate::ops::shared::subplan::SubPlanDesc<'_>) -> super::SubEffects {
        let effects = Effects { acts: Effects::of_sub_plan_call(d).acts, ..Effects::of_sub_plan(d) };
        super::SubEffects { effects, user_code: d.call.user_call, order_call: d.call.order_call }
    }
}

/// Expression `n` acts (a VOLATILE call, or one the catalog cannot answer; a SubPlan in it counts its plan): `of_expr`'s
/// `acts` alone, with no raise reading. ops/window.rs's D1354 function list asks it: PostgreSQL gives each such window
/// function its own entry (contain_volatile_functions in ExecInitWindowAgg's equal-function search).
pub fn expr_acts<'p>(cx: &Cx<'_, 'p>, n: Node<'p>) -> bool {
    Effects::of_found(cx.walk(n)).acts
}

/// Expression `n` neither acts nor may raise (landing 16 P3): the one "pure" premise a family asks, not a list of its own.
pub fn pure<'p>(cx: &Cx<'_, 'p>, n: Node<'p>, sc: &Scope<'_>) -> bool {
    let e = of_expr(cx, n, sc);
    !e.acts && !e.may_raise
}

/// KeySem (LANGUAGE internal; equivclass#53): the builtin a call of `funcid` runs (`Facts::builtin_body`, gravity_fn
/// typesem `keysem::body`): a builtin itself, a user function `LANGUAGE internal` over a builtin C function that PostgreSQL
/// calls exactly as it calls the builtin; none for any other function. Its raise is that builtin's (`quiet_call`,
/// `leakproof`); its volatility, strictness and parallel mark stay its own pg_proc row's, as PostgreSQL reads them.
pub fn body(f: &dyn Facts, funcid: u32) -> Option<u32> {
    if funcid != 0 && funcid < FIRST_NORMAL_OID {
        return Some(funcid);
    }
    f.builtin_body(funcid).filter(|&b| b != 0 && b < FIRST_NORMAL_OID)
}

/// A call of `funcid` raises on no input of its argument types: a leakproof builtin, or a builtin whose raise-free rule
/// needs no argument (`types_core::quiet::call`, or initdb's SQL function whose catalog row it knows: `quiet::sql_body`
/// over `Facts::proc_text`). A user function only as the builtin it runs (`body`: LANGUAGE internal), or (D3082) as the body
/// PostgreSQL's planner would inline for a call of it (`sql_inline_quiet`), never by its own mark.
pub fn quiet_call(f: &dyn Facts, funcid: u32) -> bool {
    body(f, funcid).is_some_and(|b| leakproof(f, b) || types_core::quiet::call(b) || f.proc_text(b, &mut types_core::quiet::sql_body)) || sql_inline_quiet(f, funcid)
}

/// D3979: a call of `funcid` under input collation `coll` whose raise-free rule reads the collation alone: the builtin
/// it runs (`body`) is lower / upper / initcap (text) under the default, C or POSIX
/// (`types_core::quiet::case_map_quiet`).
pub fn quiet_collated(f: &dyn Facts, funcid: u32, coll: u32) -> bool {
    body(f, funcid).is_some_and(|b| types_core::quiet::case_map_quiet(b, coll))
}

/// D3082: user `LANGUAGE sql` function `funcid` whose inlined body (`Facts::sql_inlined`) raises on no input by this
/// home's own walk (`quiet_body`; its parameters are PARAM_EXTERN leaves). D3713: of any volatility, as the planner
/// inlines a VOLATILE one too; whether the call acts stays its own mark's (`of_call`). A body naming another such
/// function reads it the same way (its in-flight reading answers false, so a body that reaches itself is not quiet).
fn sql_inline_quiet(f: &dyn Facts, funcid: u32) -> bool {
    funcid >= FIRST_NORMAL_OID && f.func(funcid).is_some() && f.sql_inlined(funcid, &mut |n| quiet_body(f, n))
}

/// D3713 (lowerv4 rep/keylane.rs `c_level_cmp`): a call of `funcid` raises on no pair of inputs: `quiet_call`, or a user
/// `LANGUAGE sql` function whose body as fmgr_sql runs it (`Facts::sql_fmgr_body`) raises on none. What
/// fmgr_security_definer itself may raise (its user switch, its SET clause) it raises at every call alike.
pub fn quiet_pairs(f: &dyn Facts, funcid: u32) -> bool {
    quiet_call(f, funcid) || funcid >= FIRST_NORMAL_OID && f.func(funcid).is_some() && f.sql_fmgr_body(funcid, &mut |n| quiet_body(f, n))
}

/// D3712: `quiet` over an inlined SQL body, whose integer `+ - *` also reads the body's own constants by value
/// (`exact_range` with `body`): they are the function's text, never a statement literal either key files.
fn quiet_body(f: &dyn Facts, n: Node<'_>) -> bool {
    crate::walk::stack_check(); // D3748
    let call = n.as_func_expr().map(|x| (x.funcid, &x.args)).or_else(|| n.as_op_expr().map(|o| (o.opfuncid, &o.args)));
    let in_range = |(fid, args): (u32, &NodeList<'_>)| types_core::quiet::int_arith(fid).is_some_and(|(op, ty)| exact_range(op, args, 0, true).is_some_and(|r| r.0 >= ty.0 && r.1 <= ty.1));
    let mut ok = quiet_node(f, n, &PLAIN) || call.is_some_and(in_range);
    let known = crate::expr::kids(n, &mut |c| ok &= quiet_body(f, c));
    ok && known
}

/// A builtin PostgreSQL 18.6 marks leakproof, or a user function as the builtin it runs (`body`); a user function never by
/// its own mark. A builtin's mark is the builtin table's (`builtin_func`, procfacts.rs from pg_proc.dat), never the live
/// catalog's: an ALTER FUNCTION .. [NOT] LEAKPROOF on a builtin changes no reading. A function the table does not hold
/// (initdb's SQL-made functions) keeps its catalog row's mark (`Facts::func`).
pub fn leakproof(f: &dyn Facts, funcid: u32) -> bool {
    body(f, funcid).is_some_and(|b| crate::effects::builtin_func(b).or_else(|| f.func(b)).is_some_and(|p| p.leakproof))
}

/// Expression `n` raises on no row (`may_raise`'s complement): every node's own step raises on no input (`quiet_node`)
/// and every child's does too.
fn quiet(f: &dyn Facts, n: Node<'_>, s: &Scope<'_>) -> bool {
    crate::walk::stack_check(); // D3748
    let mut ok = quiet_node(f, n, s);
    let known = crate::expr::kids(n, &mut |c| ok &= quiet(f, c, s));
    ok && known
}

/// Node `n`'s own step, its children apart, raises on no input in PostgreSQL 18.6: the one per-node rule every raise-free
/// reading asks (this home's `quiet`, lowerv4's effects/raise_free.rs walk). A call by `quiet_call`, `quiet_args` or
/// `quiet_pattern` (array_lower / array_upper too under `s.fetch`), over a set input collation (`collation_set`); an
/// aggregate by `agg_quiet`. A ScalarArrayOpExpr runs its operator's function per element (`quiet_call`, or a pattern
/// match over a constant list of good patterns: `quiet_pattern_list`, D3788); hashed
/// (PostgreSQL 14+ convert_saop_to_hashed_saop) it runs `hashfuncid` per element and per probe, which must be a builtin (a
/// user hash opclass's FUNCTION 1 is user code, D2125), and `negfuncid` (NOT IN's equality) as the compare. Leaves: a
/// column, a constant, a bound parameter (`s.bound`), a start-time SQLValueFunction, a target entry, AND / OR / NOT, IS
/// [NOT] NULL / TRUE. Structural nodes with no error path: a CollateExpr (a collation relabel; the planner keeps no run-time
/// step for it), a RowExpr (ExecEvalRow's heap_form_tuple: only resource limits), a ConvertRowtypeExpr (an inheritance
/// child's or partition's row read as an ancestor's by column name, ExecEvalConvertRowtype; the planner makes one only
/// where that map exists), a CASE (CaseExpr / CaseWhen: it only picks an arm, each a kid; a CaseTestExpr reads the value
/// already computed), a COALESCE (its first non-null argument). The node kinds F5 (`crate::rerun`) reads by the same rule:
/// IS DISTINCT FROM and NULLIF by their operator's function, a row comparison by its columns' (`row_compare_quiet`), a
/// one-dimensional ARRAY[..] (construct_md_array: a multidimensional one raises on mismatched dimensions), a field of a
/// composite, GROUPING(..). And three PostgreSQL's code shows quiet: an I/O coercion to a text-like type from a listed
/// source (`types_core::quiet::io_quiet`), `IS JSON` over jsonb or json (`json_is_quiet`), XMLCONCAT / XMLELEMENT /
/// XMLFOREST over listed value types (`xml_quiet`). Under `s.fetch` PostgreSQL 18.6's quiet array reads.
/// A call by `quiet_collated` too (D3979: a case map under the default, C or POSIX collation).
pub fn quiet_node(f: &dyn Facts, n: Node<'_>, s: &Scope<'_>) -> bool {
    let arr = s.fetch.is_some();
    if let Some(x) = n.as_func_expr() {
        return collation_set(x.funcid, x.inputcollid, &x.args) && (quiet_call(f, x.funcid) || quiet_args(x.funcid, &x.args) || quiet_pattern(f, x.funcid, &x.args, x.inputcollid) || quiet_collated(f, x.funcid, x.inputcollid) || arr && types_core::quiet::array_bound(x.funcid));
    }
    if let Some(o) = n.as_op_expr() {
        return collation_set(o.opfuncid, o.inputcollid, &o.args) && (quiet_call(f, o.opfuncid) || quiet_args(o.opfuncid, &o.args) || quiet_pattern(f, o.opfuncid, &o.args, o.inputcollid) || array_eq_args(o.opfuncid, &o.args));
    }
    if let Some(a) = n.as_aggref() {
        return agg_quiet(a);
    }
    // AGC-05: IS DISTINCT FROM and NULLIF call the operator's function on two non-null inputs only (ExecEvalDistinct,
    // EEOP_NULLIF): an OpExpr's rule
    if let Some(o) = n.as_distinct_expr() {
        let fid = op_fn(f, o.opno, o.opfuncid);
        return collation_set(fid, o.inputcollid, &o.args) && quiet_call(f, fid);
    }
    if let Some(o) = n.as_null_if_expr() {
        let fid = op_fn(f, o.opno, o.opfuncid);
        return collation_set(fid, o.inputcollid, &o.args) && quiet_call(f, fid);
    }
    if let Some(r) = n.as_row_compare_expr() {
        return row_compare_quiet(f, r);
    }
    n.as_scalar_array_op_expr().is_some_and(|x| {
        collation_set(x.opfuncid, x.inputcollid, &x.args)
            && (quiet_call(f, x.opfuncid) || quiet_pattern_list(f, x.opfuncid, &x.args, x.inputcollid))
            && (x.hashfuncid == 0 || body(f, x.hashfuncid).is_some())
            && (x.negfuncid == 0 || quiet_call(f, x.negfuncid))
    }) || n.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_EXTERN || p.paramkind == ParamKind::PARAM_EXEC && (s.bound)(p.paramid))
        || n.as_var().is_some()
        || n.as_const().is_some()
        || n.as_sql_value_function().is_some_and(|v| types_core::quiet::svf_raise_free(v.op as u32))
        // D3749: a window function that raises on no input (row_number, rank, dense_rank), as F5 reads it
        || n.as_window_func().is_some_and(|w| types_core::quiet::window_quiet(w.winfnoid, w.args.len()))
        || n.as_relabel_type().is_some()
        || n.as_collate_expr().is_some()
        || n.as_row_expr().is_some()
        || n.as_convert_rowtype_expr().is_some()
        || n.as_case_expr().is_some()
        || n.as_case_when().is_some()
        || n.as_case_test_expr().is_some()
        || n.as_target_entry().is_some()
        || n.as_bool_expr().is_some()
        || n.as_null_test().is_some()
        || n.as_boolean_test().is_some()
        || n.as_coalesce_expr().is_some() // D1018 (create_function_sql#92): its first non-null argument, no raise of its own
        || n.as_array_expr().is_some_and(|a| !a.multidims) // AGC-04: construct_md_array at ndims 1 (F5's reading)
        || n.as_field_select().is_some() // AGC-08: one attribute of the composite (ExecEvalFieldSelect)
        || n.as_grouping_func().is_some() // AGC-08: the grouping set's bitmask (EEOP_GROUPING_FUNC); its arguments are not run
        || n.as_coerce_via_io().is_some_and(|c| crate::catalog::expr_type::type_of(c.arg).is_some_and(|t| types_core::quiet::io_quiet(t, c.resulttype))) // AGC-30
        || n.as_json_is_predicate().is_some_and(json_is_quiet) // AGC-31
        || n.as_xml_expr().is_some_and(xml_quiet) // AGC-03
        || arr && (s.fetch.is_some_and(|q| q(n)) || n.as_case_test_expr().is_some() || n.as_array_coerce_expr().is_some_and(|a| a.elemexpr.is_none_or(|e| e.as_case_test_expr().is_some())))
}

/// An operator node's function: the one it names (`opfuncid`, set by the planner's fix_opfuncids), else the operator's
/// `oprcode`; 0 when the catalog has neither (no call is quiet).
fn op_fn(f: &dyn Facts, opno: u32, opfuncid: u32) -> u32 {
    if opfuncid != 0 { opfuncid } else { f.oper(opno).map_or(0, |o| o.code) }
}

/// AGC-06: a row comparison `(a, b) < (c, d)` (EEOP_ROWCOMPARE_STEP) calls, per column until the first unequal or NULL
/// pair, the btree comparison function of the column's operator family at its input types under the column's input
/// collation. Quiet when every column's family is initdb's (its comparison function a builtin; a user family's FUNCTION
/// 1 is user code), its operator's function `quiet_call` (an initdb family's comparison raises where its operators do:
/// text's on an indeterminate collation, a record's or an array's through their elements' comparators, which are not
/// leakproof), and its collation set (`collation_set` per column).
pub fn row_compare_quiet(f: &dyn Facts, r: &types_nodes::primnodes::RowCompareExpr<'_>) -> bool {
    let n = r.opnos.len();
    let coll = |c: u32, l: Node<'_>, rr: Node<'_>| c != 0 || crate::catalog::expr_type::collation_of(l) == 0 && crate::catalog::expr_type::collation_of(rr) == 0;
    n > 0 && r.opfamilies.len() == n && r.inputcollids.len() == n && r.largs.len() == n && r.rargs.len() == n
        && r.opnos.iter().zip(r.opfamilies.iter()).zip(r.inputcollids.iter()).zip(r.largs.iter().zip(r.rargs.iter()))
            .all(|(((op, fam), c), (l, rr))| fam != 0 && fam < FIRST_NORMAL_OID && quiet_call(f, op_fn(f, op, 0)) && coll(c, l, rr))
}

/// AGC-31: `x IS JSON` (ExecEvalJsonIsPredicate) over jsonb reads its root's kind; over json without WITH UNIQUE KEYS it
/// lexes one token (json_get_first_token, throw_error false). Over text, and over json WITH UNIQUE KEYS, it parses the
/// whole document (json_validate): its errors are answered false, but the recursive descent parser's check_stack_depth
/// raises on a document nested past max_stack_depth, so neither is quiet.
pub fn json_is_quiet(p: &types_nodes::primnodes::JsonIsPredicate<'_>) -> bool {
    use types_core::catalog::{JSONBOID, JSONOID};
    p.expr.and_then(crate::catalog::expr_type::type_of).is_some_and(|t| t == JSONBOID || t == JSONOID && !p.unique_keys)
}

/// AGC-03: XMLCONCAT (xmlconcat: each value's declaration read, the result ignored, then appended), XMLELEMENT and
/// XMLFOREST (each attribute and content value through map_sql_value_to_xml_value, then libxml's writer, whose only
/// checks are allocation) raise on no input when every value's type maps quietly (`types_core::quiet::xml_value_quiet`).
/// XMLPARSE, XMLPI, XMLROOT, XMLSERIALIZE and IS DOCUMENT parse or check their content: never quiet.
pub fn xml_quiet(x: &types_nodes::primnodes::XmlExpr<'_>) -> bool {
    use types_nodes::primnodes::XmlExprOp as X;
    matches!(x.op, X::IS_XMLCONCAT | X::IS_XMLELEMENT | X::IS_XMLFOREST)
        && x.named_args.iter().chain(x.args.iter()).all(|a| crate::catalog::expr_type::type_of(a).is_some_and(types_core::quiet::xml_value_quiet))
}

/// D3847: an array `=` / `<>` over two arguments of one listed built-in array type (`types_core::quiet::array_eq_quiet`:
/// a text[] key compared by a merge join, Hibernate's json_mergepatch paths). A domain over an array, a user or record
/// element type and an anyarray argument stay may-raise.
fn array_eq_args(funcid: u32, args: &NodeList<'_>) -> bool {
    let mut it = args.iter();
    let t = |n: Option<Node<'_>>| n.and_then(crate::catalog::expr_type::type_of);
    match (args.len(), t(it.next()), t(it.next())) {
        (2, Some(l), Some(r)) => types_core::quiet::array_eq_quiet(funcid, l, r),
        _ => false,
    }
}

/// A call of `funcid` over `args` with input collation `inputcollid` has the collation it reads: one is set, no argument
/// is collatable, or the function reads none (`types_core::quiet::collation_free`: the pattern comparisons, `||`).
/// Arguments of different implicit collations leave it indeterminate (0), and a collation-aware builtin then raises "could
/// not determine which collation to use" at its first non-null input (varlena.c check_collation_set: texteq, text_lt,
/// bttextcmp, hashtext ..), a raise PostgreSQL meets only on the rows it reaches. PostgreSQL marks those builtins leakproof
/// (the error shows no data). `funcid` 0 (a node that compares by its type's operator family, MinMaxExpr) reads one.
pub fn collation_set(funcid: u32, inputcollid: u32, args: &NodeList<'_>) -> bool {
    inputcollid != 0 || types_core::quiet::collation_free(funcid) || args.iter().all(|a| crate::catalog::expr_type::collation_of(a) == 0)
}

/// A call of `funcid` over `args` raises nothing by its arguments: an integer `%` by a non-null constant other than 0
/// (D567), an integer `/` by one other than 0 and -1 (div_by_const): `divisor_class`; a privilege function's oid form
/// over a constant privilege text its list accepts (`types_core::quiet::privilege_quiet`; the text's image is read
/// through `expr::const_bytes`, so both keys hold it); substr / substring(text, int4, int4) of a constant length >= 0
/// and left / right(text, int4) of a constant other than INT32_MIN: `text_arg_class`; get_bit(bit, int4) of a constant
/// inside a bit(len) column: `bit_index_class` (D3604). The classes read their
/// constants through the bake log here; both keys hold the classes, never the literals (D1103). AGB-20: an integer
/// `+`, `-`, `*` or unary `-` whose operands' types keep its exact result in its type (`range_quiet`; no constant read);
/// AGB-19: a numeric one whose operands' types bound its digits (`num_range_quiet`).
pub fn quiet_args(funcid: u32, args: &NodeList<'_>) -> bool {
    if range_quiet(funcid, args) || num_range_quiet(funcid, args) {
        return true;
    }
    let logged = |c: &Const| crate::bake::read(c);
    let divisor = divisor_class(funcid, args, &logged);
    // the text's image only for a privilege function (its last argument, D3754: has_column_privilege's third): reading
    // a constant's image keys it, so reading any other call's (LIKE 'pat', text = 'lit') would key its literal and the
    // front key would miss on another one
    let at = types_core::quiet::privilege_args(funcid).filter(|&n| n == args.len());
    let text_of = |n: usize| args.iter().nth(n - 1).and_then(|a| a.as_const()).filter(|c| !c.constisnull);
    let privilege = || at.and_then(text_of).and_then(|c| crate::expr::const_bytes(c)).and_then(|b| types_core::quiet::privilege_quiet(funcid, b));
    let text = text_arg_class(funcid, args, &logged) == Some(true) || bit_index_class(funcid, args, &logged) == Some(true);
    text || unit_class(funcid, args) == Some(true) || args.len() == 2 && divisor == Some(true) || privilege() == Some(true) || epoch_ns_class(funcid, args)
}

/// D3882 (`types_core::quiet::EPOCH_NS_CAST`'s proof): the two calls of
/// `(extract(epoch FROM <clock>) * k)::int8` that may raise elsewhere raise on no input in exactly that shape: the
/// numeric `*` (`EPOCH_NS_MUL`) of extract(epoch FROM now() / transaction_timestamp() / statement_timestamp() /
/// CURRENT_TIMESTAMP[(n)]) by a non-null numeric constant k with |k| <= 1e9 (`epoch_ns_k_quiet`), and the `::int8`
/// (`EPOCH_NS_CAST`) of that product. The extract and the clock are quiet by their own rules (`unit_class`'s epoch,
/// `quiet::start_time` / `start_time_value`). The unit's and k's images are read through the bake log
/// (`expr::const_bytes`), which keys the literals, as `unit_class` keys its unit.
pub fn epoch_ns_class(funcid: u32, args: &NodeList<'_>) -> bool {
    use types_core::quiet as q;
    fn call<'a>(n: Node<'a>) -> Option<(u32, &'a NodeList<'a>)> {
        n.as_func_expr().map(|x| (x.funcid, &x.args)).or_else(|| n.as_op_expr().map(|o| (o.opfuncid, &o.args)))
    }
    let clock = |n: Node<'_>| {
        n.as_func_expr().is_some_and(|x| q::start_time(x.funcid) && x.args.is_nil()) || n.as_sql_value_function().is_some_and(|v| q::epoch_clock_value(v.op as u32))
    };
    let epoch = |n: Node<'_>| {
        call(n).is_some_and(|(fid, a)| {
            fid == q::EPOCH_NS_EXTRACT
                && a.len() == 2
                && a.iter().next().and_then(|u| u.as_const()).and_then(|c| crate::expr::const_bytes(c)).is_some_and(|u| q::dt_unit(u, true) == Some(q::DtUnit::Epoch))
                && a.iter().nth(1).is_some_and(clock)
        })
    };
    // k is numeric_mul's second argument: the call's signature makes the constant's image a numeric's (a domain over
    // numeric has the same image), so no type test is needed before decoding it
    let k = |n: Node<'_>| n.as_const().and_then(|c| crate::expr::const_bytes(c)).is_some_and(q::epoch_ns_k_quiet);
    let product = |a: &NodeList<'_>| a.len() == 2 && a.iter().next().is_some_and(epoch) && a.iter().nth(1).is_some_and(k);
    match funcid {
        q::EPOCH_NS_MUL => product(args),
        q::EPOCH_NS_CAST => args.len() == 1 && args.iter().next().and_then(call).is_some_and(|(fid, a)| fid == q::EPOCH_NS_MUL && product(a)),
        _ => false,
    }
}

/// D3041: extract / date_part / date_trunc's constant unit class (`types_core::quiet::datetime_unit_quiet`: a unit PostgreSQL
/// 18.6 computes for every value of the argument type, infinities included), `Some(quiet)` for those calls over two
/// arguments (a NULL or non-constant unit is the other class), `None` for any other call. The unit is a by-reference text
/// word, so its image is read through the bake log (`expr::const_bytes`), which keys the literal (as a privilege text's):
/// `extract(fortnight FROM t)` after `extract(minute FROM t)` files another program (exec front census: a by-reference
/// word never moves the main key, and a read that decides the graph is logged). Only these calls read it.
pub fn unit_class(funcid: u32, args: &NodeList<'_>) -> Option<bool> {
    if !types_core::quiet::datetime_unit_fn(funcid) || args.len() != 2 {
        return None;
    }
    let unit = args.iter().next().and_then(|a| a.as_const()).and_then(|c| crate::expr::const_bytes(c));
    Some(unit.is_some_and(|u| types_core::quiet::datetime_unit_quiet(funcid, u) == Some(true)))
}

/// AGB-20: an integer `+`, `-`, `*` or unary `-` (`types_core::quiet::int_arith`: int.c / int8.c raise only when the exact
/// result leaves its type) over operands whose ranges keep that result inside its type raises on no input: `s::int4 +
/// t::int4` over int2 columns, `a::int8 * b` over int4 ones (|a * b| <= 2^62). Ranges come from TYPES alone
/// (`int_range_of`: a column's, a parameter's, a constant's type, through widening casts and nested `+ - *`, and a
/// builtin integer comparator's result, D3712), never a statement constant's value, so neither key needs a class for
/// this reading (D1103). Same-width arithmetic (`int4col + 1`, `int8col * 2`), a domain over an integer and any other
/// type stay a call that may raise.
pub fn range_quiet(funcid: u32, args: &NodeList<'_>) -> bool {
    types_core::quiet::int_arith(funcid).is_some_and(|(op, ty)| exact_range(op, args, 0, false).is_some_and(|r| r.0 >= ty.0 && r.1 <= ty.1))
}

/// The exact range of integer operation `op` over `args` (one operand for `Neg`, two otherwise) by `int_range_of`; none
/// for another arity or an operand of no integer type.
fn exact_range(op: types_core::quiet::IntOp, args: &NodeList<'_>, depth: u8, body: bool) -> Option<(i128, i128)> {
    let n = if op == types_core::quiet::IntOp::Neg { 1 } else { 2 };
    if args.len() != n {
        return None;
    }
    let a = int_range_of(args.iter().next()?, depth, body)?;
    let b = if n == 2 { int_range_of(args.iter().nth(1)?, depth, body)? } else { a };
    Some(types_core::quiet::int_arith_range(op, a, b))
}

/// The range of integer expression `n`'s value by types alone: its type's (`types_core::quiet::int_type_range`: int2,
/// int4, int8), narrowed through a widening cast to its operand's, through an integer `+ - *` to the exact range of its
/// operands' (within its type: a value that would leave it raises instead) and to a builtin integer comparator's result
/// (`types_core::quiet::cmp_range`), at most 8 levels deep; none for a node of any other type (a domain over an integer
/// included: its base is not read here). Under `body` (`quiet_body`) a non-null integer constant is its value.
fn int_range_of(n: Node<'_>, depth: u8, body: bool) -> Option<(i128, i128)> {
    crate::walk::stack_check(); // D3748
    let ty = types_core::quiet::int_type_range(crate::catalog::expr_type::type_of(n)?)?;
    let clamp = |r: (i128, i128)| (r.0.max(ty.0), r.1.min(ty.1));
    if let Some(v) = n.as_const().filter(|c| body && !c.constisnull && c.constbyval).and_then(|c| types_core::quiet::int_word(plan_word(c).as_usize(), c.constlen)) {
        return Some(clamp((i128::from(v), i128::from(v))));
    }
    let call = n.as_func_expr().map(|x| (x.funcid, &x.args)).or_else(|| n.as_op_expr().map(|o| (o.opfuncid, &o.args)));
    let Some((fid, args)) = call.filter(|_| depth < 8) else { return Some(ty) };
    if let Some(r) = types_core::quiet::cmp_range(fid) {
        return Some(clamp(r));
    }
    if types_core::quiet::int_widen(fid) && args.len() == 1 {
        return Some(args.iter().next().and_then(|a| int_range_of(a, depth + 1, body)).map_or(ty, clamp));
    }
    Some(types_core::quiet::int_arith(fid).and_then(|(op, _)| exact_range(op, args, depth + 1, body)).map_or(ty, clamp))
}

/// Argument `i` of `args` as an integer (`types_core::quiet::int_word`: its low bytes) when it is a non-null by-value
/// constant, its word read through `word`.
fn arg_int(args: &NodeList<'_>, i: usize, word: &dyn Fn(&Const) -> Datum) -> Option<i64> {
    args.iter().nth(i).and_then(|a| a.as_const()).filter(|c| !c.constisnull && c.constbyval).and_then(|c| types_core::quiet::int_word(word(c).as_usize(), c.constlen))
}

/// An integer `%` or `/`'s divisor class, `Some(quiet)` over any argument list of two (`types_core::quiet::divisor_quiet`:
/// a non-null by-value constant other than 0, and for a division other than -1, raises nothing; a NULL or non-constant
/// divisor is the other class); a list of another length or another function reads no argument. The raise-free analyses
/// (decompose's and lowerv4's effects homes, D567) read the divisor's word and bake what follows (the build side, the
/// probe's prefix, a pgrc lead qual, F5's eager fill). `word` reads the constant: `quiet_args` through the bake log; the
/// content key (lower inputs/key.rs) and the front key (exec inputs/front.rs) the plan's own word (`plan_word`), since
/// they hold this class and never the literal (D1103): `x % 0` right after `x % 7` would otherwise run the raise-free
/// program, raising where PostgreSQL never evaluates the modulus, or not where it does; `x / -1` after `x / 10` the same.
pub fn divisor_class(funcid: u32, args: &NodeList<'_>, word: &dyn Fn(&Const) -> Datum) -> Option<bool> {
    // the function first (`divisor_quiet` is `None` for any other, whatever the divisor): the keys ask it of every call
    if args.len() != 2 || types_core::quiet::divisor_quiet(funcid, None).is_none() {
        return None;
    }
    types_core::quiet::divisor_quiet(funcid, arg_int(args, 1, word))
}

/// substr / substring(text, int4, int4)'s constant length class and left / right(text, int4)'s constant n class,
/// `Some(quiet)` for those calls (`types_core::quiet::text_arg_quiet`; NULL or a non-constant is the other class), `None`
/// for any other call. `word` as `divisor_class`'s: `substr(x, 1, -1)` right after `substr(x, 1, 1)` would otherwise run
/// the quiet program, raising where PostgreSQL's order would not.
pub fn text_arg_class(funcid: u32, args: &NodeList<'_>, word: &dyn Fn(&Const) -> Datum) -> Option<bool> {
    let i = types_core::quiet::const_arg(funcid, args.len())?;
    Some(types_core::quiet::text_arg_quiet(funcid, arg_int(args, i, word)))
}

/// D3604: get_bit(bit, int4)'s class, `Some(quiet)` for that call (types_core quiet `bit_index`: its operand a column of type
/// bit(len), read by its typmod, its index a constant inside it), `None` for any other call. `word` as `divisor_class`'s:
/// both keys hold the class, so `get_bit(b, 9)` right after `get_bit(b, 7)` files another program.
pub fn bit_index_class(funcid: u32, args: &NodeList<'_>, word: &dyn Fn(&Const) -> Datum) -> Option<bool> {
    // the function first (`bit_index` is `None` for any other, whatever the column and index)
    types_core::quiet::bit_index(funcid, -1, None)?;
    let len = args.iter().next().and_then(|a| a.as_var()).filter(|v| v.vartype == types_core::catalog::BITOID).map_or(-1, |v| v.vartypmod);
    let n = if args.len() == 2 { arg_int(args, 1, word) } else { None };
    types_core::quiet::bit_index(funcid, len, n)
}

/// D1920 / D1644: a BERNOULLI or SYSTEM TABLESAMPLE clause's own raise class, `Some(quiet)` (`None` for another method,
/// which raises by its own rules). PostgreSQL raises at each scan's start: nodeSamplescan.c tablesample_init over a NULL
/// argument or REPEATABLE value, tsm_bernoulli.c / tsm_system.c BeginSampleScan over a percentage outside 0 .. 100 or NaN.
/// A non-null constant percentage inside that range with no REPEATABLE value or a non-null constant one raises nothing.
/// `word` as `divisor_class`'s: the content key (lower inputs/key.rs) holds this class and the front key the values
/// themselves (exec inputs/front.rs keys a sample's arguments by value), so `BERNOULLI (150)` right after `BERNOULLI (50)`
/// files another program.
pub fn sample_class(handler: u32, args: &NodeList<'_>, repeatable: Option<Node<'_>>, word: &dyn Fn(&Const) -> Datum) -> Option<bool> {
    if !matches!(handler, crate::ops::shared::scan::TSM_BERNOULLI | crate::ops::shared::scan::TSM_SYSTEM) {
        return None;
    }
    let percent = |n: Node<'_>| n.as_const().filter(|c| !c.constisnull && c.constbyval && c.consttype == types_core::FLOAT4OID).is_some_and(|c| (0.0..=100.0).contains(&word(c).as_f32()));
    let seed = repeatable.is_none_or(|r| r.as_const().is_some_and(|c| !c.constisnull));
    Some(args.len() == 1 && args.iter().all(percent) && seed)
}

/// PS4-21: numeric's typmod cast's typmod (catalog casts `numeric_typmod_cast`; -1 for a NULL or non-constant one),
/// `None` for any other call. lowerv4's num.window cast node bakes the typmod's precision and scale, so the content key
/// holds it as the front key does (exec inputs/front.rs keys a cast's trailing arguments by value): `x::numeric(9, 2)`
/// right after `x::numeric(12, 2)` would otherwise run the wider program and miss its overflow.
pub fn numeric_typmod_class(funcid: u32, args: &NodeList<'_>, word: &dyn Fn(&Const) -> Datum) -> Option<i64> {
    crate::catalog::casts::numeric_typmod_cast(funcid).then(|| arg_int(args, 1, word).unwrap_or(-1))
}

/// A constant's own word, unlogged: the keys' read of a constant whose class they hold (`divisor_class`,
/// `text_arg_class`, `numeric_typmod_class`), and an inlined SQL body's integer constant (`int_range_of` under `body`).
pub fn plan_word(c: &Const) -> Datum {
    c.constvalue
}

/// DC-14 (main's ruling on nestloop's DC-14 / NEST-01, option (a)): a builtin pattern match (`effects::builtin_pattern`:
/// LIKE, ILIKE, `~`, `~*`, their negations; SIMILAR TO folds to `~`) of a non-NULL constant pattern raises on no row once
/// the pattern is known good at lowering (`pattern_good`). The pattern's image is read unlogged (`pattern_operand`), so the
/// program is keyed by that class, never the literal: exec front.rs puts the same `pattern_good` answer, and the content
/// key hashes the image (a LIKE over another good literal hits the same program, as before DC-14).
pub fn quiet_pattern(f: &dyn Facts, funcid: u32, args: &NodeList<'_>, coll: u32) -> bool {
    use crate::effects::Pattern;
    // D3039: a LIKE / ILIKE over a text extern parameter this execution binds to a pattern no match raises on
    // (`pattern_quiet`, which records the fact the admission's reuse checks)
    let param = || {
        crate::effects::builtin_pattern(funcid).filter(|f| matches!(f, Pattern::Like | Pattern::ILike))?;
        let p = args.iter().nth(1)?.as_param()?;
        (args.len() == 2 && coll != 0 && f.coll(coll).bytes_eq).then_some(())?;
        pattern_quiet(p)
    };
    if param() == Some(true) {
        return true;
    }
    pattern_operand(funcid, args).is_some_and(|(form, pat)| pattern_good(form, pat, coll, f.coll(coll).bytes_eq, || f.regex_compiles(pat, form == Pattern::Regex(true), coll)))
}

/// D3019: whether extern parameter `p` (a text, varchar or bpchar `$n`) is, in this execution, a LIKE pattern no row's
/// match raises on (admit.rs `QUIET_FACT`); recorded as taken, so the admission's reuse finds the same answer. `None`
/// outside the scope or for another parameter. A lowering that decides on it is keyed apart from one that would decide
/// otherwise (`admit::externs_salt`).
pub fn pattern_quiet(p: &types_nodes::primnodes::Param) -> Option<bool> {
    if p.paramkind != ParamKind::PARAM_EXTERN || !crate::admit::text_like(p.paramtype) {
        return None;
    }
    let f = gravity_program::externs::take(p.paramid, |_| true)?;
    Some((f - crate::admit::QUIET_FACT) & crate::admit::QUIET_BIT != 0)
}

/// DC-14: builtin `funcid`'s pattern form and its constant pattern's bytes, for a two-argument call whose second argument is
/// a non-NULL text constant; none otherwise. Read without the bake log (`expr::const_bytes_keyed`): the decision it feeds is
/// keyed as `pattern_good`'s class (exec front.rs), never as the bytes.
pub fn pattern_operand<'p>(funcid: u32, args: &NodeList<'p>) -> Option<(crate::effects::Pattern, &'p [u8])> {
    let form = crate::effects::builtin_pattern(funcid)?;
    let pat = args.iter().nth(1).and_then(|a| a.as_const()).filter(|c| !c.constisnull).and_then(|c| crate::expr::const_bytes_keyed(c))?;
    (args.len() == 2).then_some((form, pat))
}

/// DC-14: the class both keys hold, whether a match of `form` over constant pattern `pat` under collation `coll` raises on
/// no row. A collation is named (PostgreSQL's LIKE and regex raise "could not determine which collation" on none) and
/// deterministic (`bytes_eq`: ILIKE and regexes refuse a nondeterministic one); a LIKE / ILIKE pattern's trailing
/// backslashes pair up (MatchText's one error is a pattern ending in its escape; the planner folds an ESCAPE clause to the
/// default escape); a regular expression compiles (`compiles`: the server compiles it as the first row would,
/// `Facts::regex_compiles`). A pattern that fails stays a call that may raise.
pub fn pattern_good(form: crate::effects::Pattern, pat: &[u8], coll: u32, bytes_eq: bool, compiles: impl FnOnce() -> bool) -> bool {
    use crate::effects::Pattern;
    coll != 0
        && bytes_eq
        && match form {
            Pattern::Like | Pattern::ILike => pat.iter().rev().take_while(|&&b| b == b'\\').count() % 2 == 0,
            Pattern::Regex(_) => compiles(),
        }
}

/// D3788 (DC-14 over a list): `x op ANY / ALL (c)`'s pattern list: builtin `funcid`'s pattern form and the bytes after
/// the length word of `c`, its second argument, a non-NULL constant array (`each_list_text` reads its elements); none
/// otherwise. Read without the bake log (`expr::const_bytes_keyed`), as `pattern_operand`: the decision it feeds is keyed
/// as `pattern_list_good`'s class in both keys (exec front.rs, lower key.rs), never as the bytes (a list is keyed by its
/// class, V4R-110).
pub fn pattern_list<'p>(funcid: u32, args: &NodeList<'p>) -> Option<(crate::effects::Pattern, &'p [u8])> {
    let form = crate::effects::builtin_pattern(funcid)?;
    let body = args.iter().nth(1).and_then(|a| a.as_const()).filter(|c| !c.constisnull).and_then(|c| crate::expr::const_bytes_keyed(c))?;
    (args.len() == 2).then_some((form, body))
}

/// D3788: `f` over each non-NULL element of the one-dimensional text array whose bytes after its length word are `body`,
/// an image as PostgreSQL builds it (construct_md_array: `ndim`, `dataoffset` (0: no NULL bitmap), `elemtype`, the
/// dimension and its lower bound, the NULL bitmap (a set bit: a value), then each element at int alignment from the
/// array's start, detoasted to a 4-byte header). A NULL element is skipped: the match is strict, so ExecEvalScalarArrayOp
/// never calls it there. Some(whether `f` held for every element), an empty array's none; None for an image this does
/// not read (another element type, two or more dimensions, a short or compressed element, a length past the image),
/// where the caller fails closed.
pub fn each_list_text(body: &[u8], f: &mut dyn FnMut(&[u8]) -> bool) -> Option<bool> {
    // the field at offset `at` from the array's start, its length word included (`body` starts past it, at 4)
    let word = |at: usize| body.get(at.checked_sub(4)?..at).map(|w| u32::from_ne_bytes([w[0], w[1], w[2], w[3]]));
    let (ndim, dataoffset, elemtype) = (word(4)? as i32, word(8)? as i32, word(12)?);
    if ndim == 0 {
        return Some(true);
    }
    if ndim != 1 || elemtype != crate::catalog::TEXTOID || dataoffset < 0 {
        return None;
    }
    let n = usize::try_from(word(16)? as i32).ok()?;
    // ARR_OVERHEAD_NONULLS(1): MAXALIGN(16 + 8); else the bitmap at offset 24 and the data at `dataoffset`
    let mut off = if dataoffset == 0 { 24 } else { dataoffset as usize };
    let mut all = true;
    for i in 0..n {
        if dataoffset != 0 && body.get(20 + i / 8)? & (1 << (i % 8)) == 0 {
            continue;
        }
        off = off.checked_add(3)? & !3;
        let head = body.get(off.checked_sub(4)?..off)?;
        // VARSIZE_4B over the little-endian header; a 1-byte or compressed header is no construct_md_array element
        let hdr = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
        let len = (hdr >> 2) as usize;
        if hdr & 0x03 != 0 || len < 4 {
            return None;
        }
        all &= f(body.get(off..off.checked_add(len - 4)?)?);
        off += len;
    }
    Some(all)
}

/// D3788: the class both keys hold for `x op ANY / ALL (c)`: whether every non-NULL element of constant pattern list `c`
/// (`pattern_list`, `each_list_text`) is a good pattern of the match's form under `coll` (`pattern_good`; `compiles`:
/// whether a pattern compiles as a regular expression, case-insensitive or not); none where `c` is no constant pattern
/// list this reads. exec front.rs and lower key.rs put it with the server's own regex facts, `quiet_pattern_list` asks
/// it with the decomposer's.
pub fn pattern_list_good(funcid: u32, args: &NodeList<'_>, coll: u32, bytes_eq: bool, compiles: &dyn Fn(&[u8], bool) -> bool) -> Option<bool> {
    use crate::effects::Pattern;
    let (form, body) = pattern_list(funcid, args)?;
    each_list_text(body, &mut |pat| pattern_good(form, pat, coll, bytes_eq, || compiles(pat, form == Pattern::Regex(true))))
}

/// D3788 (DC-14 over a list): a builtin pattern match `x op ANY / ALL (c)` over a non-NULL
/// constant text array `c` whose every non-NULL element is a good pattern raises on no row. ExecEvalScalarArrayOp calls
/// the match once per element (never for a NULL one: the match is strict), and each such call raises on no row
/// (`quiet_pattern`'s rule). pgwatch's table metrics read `nspname NOT LIKE ALL (ARRAY[..])` in a recursive union's seed.
/// Keyed as `pattern_list_good`'s class, never the list's values.
pub fn quiet_pattern_list(f: &dyn Facts, funcid: u32, args: &NodeList<'_>, coll: u32) -> bool {
    pattern_list_good(funcid, args, coll, f.coll(coll).bytes_eq, &|pat, icase| f.regex_compiles(pat, icase, coll)) == Some(true)
}

/// D3019: `pattern_good`'s LIKE rule over a bound extern parameter's varlena image at `p`: its payload in place (an
/// external or compressed image is not read: `false`) and its trailing backslashes paired, so no row's match raises.
pub fn like_image_quiet(p: *const u8) -> bool {
    crate::expr::varlena_payload(p).is_some_and(|b| b.iter().rev().take_while(|&&x| x == b'\\').count() % 2 == 0)
}

/// A builtin plain aggregate whose transitions and final raise on no input (its arguments and FILTER not included); a
/// text extremum only under a collation its comparison never fails in (`types_core::quiet::text_extremum_quiet`, main's
/// rule-4 consent 2026-10-04 19:36:20Z; the input collation is in both keys).
pub fn agg_quiet(a: &Aggref<'_>) -> bool {
    // AGB-19: a numeric argument whose integer digits its types bound (`num_bounded`), not only a bare column with a typmod
    let typmod = || a.args.iter().next().is_some_and(num_bounded);
    a.aggkind == AGGKIND_NORMAL && (agg_oid_quiet(a.aggfnoid, typmod) || types_core::quiet::text_extremum_quiet(a.aggfnoid, a.inputcollid))
}

/// `agg_quiet`'s rule by the aggregate's OID (an `Aggref`'s `aggfnoid`, a window aggregate's `winfnoid`); `typmod`: its
/// numeric argument's integer digits are bounded (`num_bounded`; a window reader: a column with a typmod), since a
/// numeric sum's, average's or variance's overflow needs an unbounded one. AGB-19: a variance / standard deviation over
/// int2 / int4 (`Stat3I128`: n, the sum and the sum of squares in int128, the final in numeric: int.c int4_accum /
/// numeric.c numeric_poly_*) raises at no row count; one over numeric or int8 (`Stat3Num`) under the bound.
pub fn agg_oid_quiet(fnoid: u32, typmod: impl FnOnce() -> bool) -> bool {
    use crate::ops::shared::agg_class::Class as C;
    crate::ops::agg::trans::table::row(fnoid).is_some_and(|r| match r.class {
        C::CountStar | C::CountAny | C::CountPair | C::SumI64 | C::SumI128 | C::AvgI128 | C::BoolAnd | C::BoolOr | C::BitI64(_) => true,
        // NEW-44: a builtin min / max over a type whose comparison raises nothing (types_core `extremum_quiet`), folded by
        // value (Int, U64, Tid, F64, Num) or through its own *_larger / *_smaller (timetz, interval: `Ext::Opaque`)
        C::Min(_) | C::Max(_) => types_core::quiet::extremum_quiet(fnoid),
        C::SumNum | C::AvgNum | C::Stat3Num(_) => typmod(),
        C::Stat3I128(_) => true,
        _ => false,
    })
}

/// AGB-19: integer digits a numeric sum, average or variance argument may have and stay raise-free: a sum of at most
/// 2^63 rows adds 19 digits, a sum of squares doubles them and adds those, and numeric holds 131072 (NUMERIC_WEIGHT_MAX).
pub const NUM_BOUNDED_DIGITS: u32 = 65_000;

/// AGB-19: expression `n` (a target entry: its expression) is an integer or a numeric whose integer digits its types bound
/// within `NUM_BOUNDED_DIGITS` (`num_digits`).
pub fn num_bounded(n: Node<'_>) -> bool {
    num_digits(n.as_target_entry().map_or(n, |t| t.expr), 0).is_some_and(|d| d <= NUM_BOUNDED_DIGITS)
}

/// AGB-19: an upper bound on expression `n`'s integer digits (|v| < 10^d) by types alone: an integer's
/// (`types_core::quiet::int_digits`); a numeric column's or parameter's typmod (precision - scale); through numeric
/// arithmetic and the casts from integers (`num_op_digits`), at most 8 levels deep. None for an unconstrained numeric, a
/// domain, a constant's value (never read: neither key holds a class for it, D1103) or anything else.
pub fn num_digits(n: Node<'_>, depth: u8) -> Option<u32> {
    crate::walk::stack_check();
    let t = crate::catalog::expr_type::type_of(n)?;
    match t {
        types_core::catalog::NUMERICOID => {}
        _ => return types_core::quiet::int_digits(t),
    }
    if let Some(v) = n.as_var() {
        return typmod_digits(v.vartypmod);
    }
    if let Some(p) = n.as_param() {
        return typmod_digits(p.paramtypmod);
    }
    let (fid, args) = n.as_func_expr().map(|x| (x.funcid, &x.args)).or_else(|| n.as_op_expr().map(|o| (o.opfuncid, &o.args)))?;
    num_op_digits(fid, args, depth)
}

/// The integer digits of numeric call `funcid` over `args` (`types_core::quiet::num_arith`: `+` / `-` the wider
/// operand's and one more, floor / ceil the operand's and one more, `*` the sum, unary `-` and a cast from an integer the
/// operand's), its operands read by `num_digits` one level below `depth`.
fn num_op_digits(funcid: u32, args: &NodeList<'_>, depth: u8) -> Option<u32> {
    use types_core::quiet::NumOp;
    let (op, _) = types_core::quiet::num_arith(funcid).filter(|&(_, k)| k == args.len())?;
    let arg = |i: usize| args.iter().nth(i).filter(|_| depth < 8).and_then(|a| num_digits(a, depth + 1));
    match op {
        NumOp::Neg | NumOp::Cast => arg(0),
        NumOp::Integral => Some(arg(0)? + 1),
        NumOp::AddSub => Some(arg(0)?.max(arg(1)?) + 1),
        NumOp::Mul => Some(arg(0)? + arg(1)?),
    }
}

/// A numeric typmod's integer digits, precision - scale (`((precision << 16) | (scale & 0x7ff)) + VARHDRSZ`, the scale a
/// signed 11-bit field since PostgreSQL 15), 0 when the scale is the larger; none for no typmod (-1).
fn typmod_digits(typmod: i32) -> Option<u32> {
    let t = typmod.checked_sub(4).filter(|&t| t >= 0)?;
    let (p, s) = ((t >> 16) & 0xffff, ((t & 0x7ff) ^ 1024) - 1024);
    Some((p - s).max(0) as u32)
}

/// AGB-19: numeric `+`, `-`, `*`, unary `-`, floor and ceil (`quiet::num_arith`) raise only when the result's integer
/// digits pass numeric's 131072 ("value overflows numeric format"; a product's scale past the display limit is rounded,
/// never raised): quiet when their operands' types bound the result's digits within that (`num_op_digits`).
pub fn num_range_quiet(funcid: u32, args: &NodeList<'_>) -> bool {
    types_core::quiet::num_arith(funcid).is_some_and(|(op, _)| op != types_core::quiet::NumOp::Cast)
        && num_op_digits(funcid, args, 0).is_some_and(|d| d <= 131_072)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_extremum_reading_is_types_core_extremum_quiet() {
        // unify agg-quiet (algebra, main 2026-10-06T07:08:28Z): the planner's InitPlan proof (types_core agg_quiet) reads
        // a plain min / max as quiet exactly where this home does (D2875's subset rule), over every builtin extremum;
        // NEW-44: timetz and interval (`Ext::Opaque`) too.
        use crate::ops::shared::agg_class::Class as C;
        let mut n = 0;
        for r in crate::ops::agg::trans::table::AGG_TABLE.iter().filter(|r| r.kind == b'n' && matches!(r.class, C::Min(_) | C::Max(_))) {
            assert_eq!(super::agg_oid_quiet(r.fnoid, || false), types_core::quiet::agg_quiet(r.fnoid, 0), "{} {}", r.name, r.fnoid);
            n += 1;
        }
        assert!(n >= 40, "{n} extrema");
    }

    #[test]
    fn a_numeric_typmod_bounds_its_integer_digits() {
        let tm = |p: i32, s: i32| ((p << 16) | (s & 0x7ff)) + 4;
        assert_eq!(super::typmod_digits(tm(10, 2)), Some(8), "numeric(10,2): |v| < 10^8");
        assert_eq!(super::typmod_digits(tm(5, -2)), Some(7), "numeric(5,-2): |v| < 10^7 (PostgreSQL 15+ negative scale)");
        assert_eq!(super::typmod_digits(tm(3, 5)), Some(0), "numeric(3,5): |v| < 1");
        assert_eq!(super::typmod_digits(tm(1000, 0)), Some(1000));
        assert_eq!(super::typmod_digits(-1), None, "an unconstrained numeric");
        assert!(1000 + 1000 <= super::NUM_BOUNDED_DIGITS, "a product of two widest typmod columns stays bounded");
    }
}
