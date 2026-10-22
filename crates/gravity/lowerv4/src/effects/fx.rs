//! C1 (b), the one effects answer of a lowering expression. `gx.fx(n)` is node `n`'s answers, and `Fx::ask` is the
//! home's one producer: every question a lowering decider asks of one expression (a VOLATILE call, user code, a raise, a
//! user type, a domain CHECK's call or act) is an `Ask`, answered there through the one walk of its reading (volatile.rs,
//! user_fn.rs, raise_free.rs) or one of the walks kept in this file (`typed`, `range_cmp`, `casts`, `outside`, `heard`).
//! A decider reads the answer, never a walker (lint RW): effects.rs's primitives (`volatile`, `user_fn`, `raise_free_by`,
//! ..) are delegates of these answers until their last reader moves onto `gx.fx(n)`.
//!
//! `PlanFx` is the second producer, for an expression of a PlannedStmt's tree read in admit before any `Gx` exists (a
//! Catalog, and the statement's subplans where an answer reads them): visible.rs's rewind deciders ask it
//! (`rewind_quiet`, `fixed`, through the walks `again` and `steady`).
//!
//! No memo yet: each answer walks as the primitive it replaces did (gen-only, the same calls in the same order). A memo
//! keyed by node comes on its own, built so that no entry can outlive its node or survive a rewrite of it.
use gravity_decompose::catalog::expr_type::type_of;
use gravity_decompose::catalog::typefacts::FIRST_NORMAL_OID;
use gravity_decompose::expr::kids;
use gravity_expr::catalog::Support;
use gravity_expr::Catalog;
use gravity_decompose::{Op, PipelineNode};
use types_core::quiet::{Reach, Via};
use types_nodes::primnodes::ParamKind;
use types_nodes::Node;

use super::{raise_free, user_fn, volatile};
use crate::face::kinds::KindId;
use crate::graph::Gx;

/// Node `n`'s effects answers under `gx` (no state of its own: each answer is asked of the producer, `ask`).
#[derive(Clone, Copy)]
pub(crate) struct Fx<'a, 'g, 'p, 'n> {
    gx: &'a Gx<'g, 'p>,
    n: Node<'n>,
}

impl<'g, 'p> Gx<'g, 'p> {
    /// Node `n`'s effects answers: the home's one producer (`Fx::ask`) answers each.
    #[inline]
    pub(crate) fn fx<'a, 'n>(&'a self, n: Node<'n>) -> Fx<'a, 'g, 'p, 'n> {
        Fx { gx: self, n }
    }
}

/// A question a lowering decider asks of one expression.
#[derive(Clone, Copy)]
enum Ask<'q, 'r> {
    /// D1005: a VOLATILE call anywhere, the fail-closed union of the VOLATILE readings (`volatile::Reading::any`), or a
    /// range function over a range whose comparison is user code (D2065, `RangeUserCmp`).
    Volatile,
    /// D1899: a cast to a domain whose CHECK constraints make a VOLATILE call (`volatile::Reading::check`).
    CheckVolatile,
    /// D2065: a function or operator over a range or multirange whose comparison or canonical function is user code.
    RangeUserCmp,
    /// A non-builtin function or operator anywhere (`user_fn::Reading::calls`; D1871: every operator node).
    UserCalls,
    /// W4b-RowX: a value of a type whose support procs reach user code, a node the walk cannot see, or a builtin that
    /// runs a query of its own.
    UserTyped,
    /// D237 / D1902: raises nothing on any input (`raise_free::Reading::rtfilter`), the PARAM_EXEC ids `bound` names read
    /// as bound words, under `arr` PG 18.6's quiet array reads; a hash key whose hash raises (D2849) never.
    Quiet { bound: &'q dyn Fn(i32) -> bool, arr: bool },
    /// `Quiet` per expression, a list member by member, `arr` where the member holds no user type (`each`).
    QuietEach { bound: &'q dyn Fn(i32) -> bool },
    /// D449's rule (props merge.rs `ops_can_raise`): raises nothing on any input and calls no user or VOLATILE function.
    QuietCalm,
    /// A named reading of the one VOLATILE walk (volatile.rs `Reading`: `any`, `visible`, `spool`, `draw`, ..).
    VolatileBy(&'q volatile::Reading<'r>),
    /// A named reading of the one USER-FN walk (user_fn.rs `Reading`: `calls_body`, `hidden`, ..).
    UserBy(&'q user_fn::Reading),
    /// A named reading of the one raise-free walk (raise_free.rs `Reading`: `merge`, `group_row`, `union_arms`, ..); a
    /// raising hash key is the reading's caller's to ask.
    QuietIn(&'q raise_free::Reading<'r>),
    /// A column of the row (a Var) anywhere in the node.
    ReadsColumn,
    /// MJ-AHEAD: no call in the node may raise a NOTICE (`effects::may_notice` over each FuncExpr's and OpExpr's function),
    /// as the child walk reads it (`expr::walk`'s answer).
    NoticeFree,
    /// D1486 / D3373: every node may raise but does nothing else that shows: no call that may raise a NOTICE, no SubPlan,
    /// no domain coercion (its CHECK may call anything), no PARAM_EXEC parameter `bound` does not name (an InitPlan's or a
    /// SubPlan's output, whose run is a call), and only nodes the child walk knows (`expr::walk`'s answer).
    RaiseOnly { bound: &'q dyn Fn(i32) -> bool },
    /// A cast to a domain anywhere in the node (a CoerceToDomain: its CHECK may call anything).
    DomainCast,
    /// Whether evaluating the node may raise or call a VOLATILE function anywhere in it, pipeline `p`'s SubPlan steps read
    /// by their facts (`heard`; an unknown node counts).
    Loud(&'q PipelineNode<'r>),
    /// `Loud` of the node itself, its children not read (`heard_at`).
    LoudHere(&'q PipelineNode<'r>),
    /// GUARD-G1 `support`: the node is an operator clause of two arguments whose hash support is built in (a probe's
    /// clause hashes and compares through no user code).
    HashBuiltin,
    /// D393 / D3932: a comparison the home reads quiet past PostgreSQL's leakproof mark (`effects::quiet_call`) under a set
    /// input collation, each argument quiet (`Quiet`, the PARAM_EXEC ids `bound` names bound words) or such a comparison
    /// itself (`cmp`).
    QuietCmp { bound: &'q dyn Fn(i32) -> bool },
    /// D3126: a comparison the home reads quiet whose every argument is quiet (`Quiet` over `bound`) or reads no column
    /// (`comparand`).
    Comparand { bound: &'q dyn Fn(i32) -> bool },
    /// A hash clause whose equality or hash procs may be a user's (visible/drain.rs `user_clause`; `hash_reach`).
    UserClause,
    /// An operator clause whose operator and function are built in (no user code).
    BuiltinOp,
}

impl<'a, 'g, 'p, 'n> Fx<'a, 'g, 'p, 'n> {
    /// The one producer: the answer to `q` for this node.
    #[inline(always)]
    fn ask(&self, q: Ask<'_, '_>) -> bool {
        let (gx, n) = (self.gx, self.n);
        match q {
            Ask::Volatile => volatile::walk(gx, n, &volatile::Reading::any()) || range_cmp(gx, n),
            Ask::CheckVolatile => volatile::walk(gx, n, &volatile::Reading::check(false)),
            Ask::RangeUserCmp => range_cmp(gx, n),
            Ask::UserCalls => user_fn::walk(gx.facts, n, &user_fn::Reading::calls()),
            Ask::UserTyped => typed(gx, n),
            Ask::Quiet { bound, arr } => !super::raising_key(gx, n) && raise_free::walk(gx, n, &raise_free::Reading::rtfilter(bound, arr)),
            Ask::QuietEach { bound } => each(gx, n, bound),
            Ask::QuietCalm => !super::raising_key(gx, n) && !crate::props::ops_can_raise(gx, &[Op::Filter(n)]),
            Ask::VolatileBy(r) => volatile::walk(gx, n, r),
            Ask::UserBy(r) => user_fn::walk(gx.facts, n, r),
            Ask::QuietIn(r) => raise_free::walk(gx, n, r),
            Ask::ReadsColumn => {
                let mut column = false;
                gravity_decompose::expr::walk(n, &mut |x| {
                    column |= x.as_var().is_some();
                    !column
                });
                column
            }
            Ask::NoticeFree => {
                let notice = |x: Node<'_>| x.as_func_expr().map(|f| f.funcid).or_else(|| x.as_op_expr().map(|o| o.opfuncid)).is_some_and(super::may_notice);
                gravity_decompose::expr::walk(n, &mut |x| !notice(x))
            }
            Ask::RaiseOnly { bound } => gravity_decompose::expr::walk(n, &mut |x| {
                let notice = x.as_func_expr().is_some_and(|f| super::may_notice(f.funcid)) || x.as_op_expr().is_some_and(|o| super::may_notice(o.opfuncid));
                let unbound = x.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_EXEC && !bound(p.paramid));
                x.as_sub_plan().is_none() && x.as_coerce_to_domain().is_none() && !notice && !unbound
            }),
            Ask::DomainCast => {
                let mut dom = false;
                gravity_decompose::expr::walk(n, &mut |x| {
                    dom |= x.as_coerce_to_domain().is_some();
                    !dom
                });
                dom
            }
            Ask::Loud(p) => heard(gx, p, n),
            Ask::LoudHere(p) => heard_at(gx, p, n),
            Ask::HashBuiltin => n
                .as_op_expr()
                .filter(|o| o.args.len() == 2)
                .and_then(|o| Some(super::Key { op: o.opno, lefttype: type_of(o.args.nth(0))?, righttype: type_of(o.args.nth(1))?, family: 0, coll: 0 }))
                .is_some_and(|k| super::support(gx, super::KeyUse::Hash, &[k], &[]).is_ok()),
            Ask::QuietCmp { bound } => cmp(gx, n, bound),
            Ask::Comparand { bound } => comparand(gx, n, bound),
            Ask::UserClause => hash_reach(gx, n),
            Ask::BuiltinOp => n.as_op_expr().is_some_and(|o| o.opno < FIRST_NORMAL_OID && o.opfuncid != 0 && o.opfuncid < FIRST_NORMAL_OID),
        }
    }

    /// decompose's effects of the node read plainly (`effects::of_expr` under `PLAIN`): the planning half's one answer.
    #[inline(always)]
    fn effects(&self, cx: &gravity_decompose::effects::Cx<'_, 'n>) -> gravity_decompose::effects::Effects {
        gravity_decompose::effects::of_expr(cx, self.n, &gravity_decompose::effects::PLAIN)
    }

    /// D1005: a VOLATILE call anywhere in the node (effects.rs `volatile`).
    #[inline]
    pub(crate) fn volatile_any(&self) -> bool {
        self.ask(Ask::Volatile)
    }

    /// D1899: a cast in the node to a domain whose CHECKs make a VOLATILE call (effects.rs `volatile_check`).
    #[inline]
    pub(crate) fn volatile_check(&self) -> bool {
        self.ask(Ask::CheckVolatile)
    }

    /// D2065: a range function in the node over a range whose comparison is user code (effects.rs `range_user_cmp`).
    #[inline]
    pub(crate) fn range_user_cmp(&self) -> bool {
        self.ask(Ask::RangeUserCmp)
    }

    /// A non-builtin function or operator anywhere in the node (effects.rs `user_fn`).
    #[inline]
    pub(crate) fn user_calls(&self) -> bool {
        self.ask(Ask::UserCalls)
    }

    /// A non-builtin or VOLATILE function or operator anywhere in the node (`user_calls`, then `volatile_any`; effects.rs
    /// `user_or_volatile`).
    #[inline]
    pub(crate) fn user_or_volatile(&self) -> bool {
        self.ask(Ask::UserCalls) || self.ask(Ask::Volatile)
    }

    /// W4b-RowX: the node holds a value of a type whose support procs reach user code (effects.rs `user_typed`).
    #[inline]
    pub(crate) fn user_typed(&self) -> bool {
        self.ask(Ask::UserTyped)
    }

    /// D237: the node raises nothing on any input (effects.rs `raise_free`).
    #[inline]
    pub(crate) fn quiet(&self) -> bool {
        self.ask(Ask::Quiet { bound: &|_| false, arr: false })
    }

    /// D1902: `quiet` with the PARAM_EXEC ids `bound` names read as bound words, under `arr` PG 18.6's quiet array reads
    /// (effects.rs `raise_free_by`).
    #[inline]
    pub(crate) fn quiet_by(&self, bound: &dyn Fn(i32) -> bool, arr: bool) -> bool {
        self.ask(Ask::Quiet { bound, arr })
    }

    /// `quiet_by` per expression, a list member by member (effects.rs `raise_free_each`).
    #[inline]
    pub(crate) fn quiet_each(&self, bound: &dyn Fn(i32) -> bool) -> bool {
        self.ask(Ask::QuietEach { bound })
    }

    /// D449's rule: the node raises nothing on any input and calls no user or VOLATILE function (effects.rs `expr_quiet`).
    #[inline]
    pub(crate) fn quiet_calm(&self) -> bool {
        self.ask(Ask::QuietCalm)
    }

    /// The node under a named reading of the one VOLATILE walk (volatile.rs `Reading`).
    #[inline]
    pub(crate) fn volatile_by(&self, r: &volatile::Reading<'_>) -> bool {
        self.ask(Ask::VolatileBy(r))
    }

    /// The node under a named reading of the one USER-FN walk (user_fn.rs `Reading`).
    #[inline]
    pub(crate) fn user_by(&self, r: &user_fn::Reading) -> bool {
        self.ask(Ask::UserBy(r))
    }

    /// The node under a named reading of the one raise-free walk (raise_free.rs `Reading`).
    #[inline]
    pub(crate) fn quiet_in(&self, r: &raise_free::Reading<'_>) -> bool {
        self.ask(Ask::QuietIn(r))
    }

    /// The node reads a column of the row (a Var anywhere in it).
    #[inline]
    pub(crate) fn reads_column(&self) -> bool {
        self.ask(Ask::ReadsColumn)
    }

    /// MJ-AHEAD: no call in the node may raise a NOTICE (face/pull.rs's pulled code).
    #[inline]
    pub(crate) fn notice_free(&self) -> bool {
        self.ask(Ask::NoticeFree)
    }

    /// D1486 / D3373: the node's calls, parameters and nodes may raise but do nothing else that shows (`Ask::RaiseOnly`); the
    /// caller asks the user, VOLATILE and user-type answers first.
    #[inline]
    pub(crate) fn raise_only_by(&self, bound: &dyn Fn(i32) -> bool) -> bool {
        self.ask(Ask::RaiseOnly { bound })
    }

    /// The node holds a cast to a domain (its CHECK may call anything: a reader that cannot see it fails closed).
    #[inline]
    pub(crate) fn domain_cast(&self) -> bool {
        self.ask(Ask::DomainCast)
    }

    /// Whether evaluating the node may raise or call a VOLATILE function anywhere in it (`Ask::Loud`; visible/nestloop.rs
    /// `loud`, a computed key's re-open).
    #[inline]
    pub(crate) fn loud(&self, p: &PipelineNode<'_>) -> bool {
        self.ask(Ask::Loud(p))
    }

    /// Whether evaluating the node itself, its children not read, may raise or call a VOLATILE function (`Ask::LoudHere`).
    #[inline]
    pub(crate) fn loud_here(&self, p: &PipelineNode<'_>) -> bool {
        self.ask(Ask::LoudHere(p))
    }

    /// The node is an operator clause of two arguments whose hash support is built in (`Ask::HashBuiltin`).
    #[inline]
    pub(crate) fn hash_builtin(&self) -> bool {
        self.ask(Ask::HashBuiltin)
    }

    /// D393 / D3932: the node is a comparison the home reads quiet past PostgreSQL's leakproof mark over quiet arguments
    /// (`Ask::QuietCmp`; visible/errorhold.rs `quiet_cmp_by`).
    #[inline]
    pub(crate) fn quiet_cmp(&self, bound: &dyn Fn(i32) -> bool) -> bool {
        self.ask(Ask::QuietCmp { bound })
    }

    /// D3126: the node may raise only inside a comparand that reads no column (`Ask::Comparand`; visible/pgrc.rs
    /// `raises_in_comparand`).
    #[inline]
    pub(crate) fn raises_in_comparand(&self, bound: &dyn Fn(i32) -> bool) -> bool {
        self.ask(Ask::Comparand { bound })
    }

    /// The node is a hash clause whose equality or hash procs may be a user's (`Ask::UserClause`).
    #[inline]
    pub(crate) fn user_clause(&self) -> bool {
        self.ask(Ask::UserClause)
    }

    /// The node is an operator clause whose operator and function are built in (`Ask::BuiltinOp`).
    #[inline]
    pub(crate) fn builtin_op(&self) -> bool {
        self.ask(Ask::BuiltinOp)
    }

    /// Whether everything PG evaluates before SubPlan `plan_id` inside the node (each argument left of it, at every level
    /// down to it) raises nothing (`quiet`); None when the node does not hold it (rerun/store_read.rs).
    pub(crate) fn quiet_before(&self, plan_id: i32) -> Option<bool> {
        before(self.gx, self.n, plan_id)
    }

    /// The node acts by decompose's plain reading (`effects::of_expr(.., PLAIN).acts`: a VOLATILE call or one the catalog
    /// cannot answer, a SubPlan, any node the walk does not know).
    #[inline]
    pub(crate) fn acts_plain(&self, cx: &gravity_decompose::effects::Cx<'_, 'n>) -> bool {
        self.effects(cx).acts
    }

    /// `window_fill_exact`: the node calm (raise-free, nothing VOLATILE or user-defined: `ordered_fed_acting`'s reading)
    /// outside the SubPlans `placed` names, which the fill's own step evaluates with their arguments; a node above one is
    /// read alone (decompose `of_node`), its other children whole.
    pub(crate) fn calm_outside(&self, arena: gravity_program::ArenaRef, placed: &dyn Fn(i32) -> bool) -> bool {
        outside(self.gx, arena, self.n, placed)
    }
}

impl<'a, 'g, 'p> Fx<'a, 'g, 'p, 'p> {
    /// D1907: a cast in the node to a domain whose CHECK acts by effects' reader (`effects::of_expr(..).acts`: a VOLATILE
    /// call or one the catalog cannot answer, a SubPlan, any node the walk does not know), a nested domain's CHECK with it:
    /// the inline forms run the check at every open, while effects reads a domain coercion as one that may raise, not one
    /// that acts. `volatile_check`'s walk with that reading of a CHECK (effects.rs `domain_checks`), failing closed the
    /// same way (effects.rs `check_acts`).
    pub(crate) fn check_acts(&self, cx: &gravity_decompose::effects::Cx<'_, 'p>) -> bool {
        casts(self.gx, cx, self.n, &mut mcx::PgVec::new_in(self.gx.mcx))
    }
}

/// `Ask::RangeUserCmp` (D2065). PostgreSQL's range functions compare bounds and elements through the type cache
/// (rangetypes.c range_cmp_bounds: `rng_cmp_proc_finfo`), so `r @> x` over such a range runs that user function, VOLATILE
/// or not, at each call: a call the side classes, which read a call's own function, do not see. A user type the catalog
/// cannot answer for counts; a built-in type is not asked.
fn range_cmp(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    let c = gx.catalog;
    let q = |s: Support| c.support(s).ok().filter(|&x| x != 0);
    // a built-in type (below FIRST_NORMAL_OID) is no domain over a user type, and a built-in range's opclass is built in
    let user = |t: u32| {
        if t < FIRST_NORMAL_OID {
            return false;
        }
        let Ok(b) = c.base_type(t, &mut -1) else { return true };
        let multi = q(Support::MultirangeRange(b)).is_some();
        (multi || q(Support::RangeSub(b)).is_some()) && !super::reach(c, Reach::Range(Via::Cmp, multi), b, 0)
    };
    let mut any = false;
    gravity_decompose::expr::walk(n, &mut |x| {
        let args = x.as_func_expr().map(|f| &f.args).or_else(|| x.as_op_expr().map(|o| &o.args)).or_else(|| x.as_scalar_array_op_expr().map(|o| &o.args)).or_else(|| x.as_distinct_expr().map(|o| &o.args)).or_else(|| x.as_null_if_expr().map(|o| &o.args));
        any |= args.is_some_and(|a| a.iter().any(|v| type_of(v).is_some_and(user)));
        !any
    });
    any
}

/// `Ask::UserTyped` (W4b-RowX, code review rounds 2 and 3; `n` a list: each member, as a gate's implicit AND): a type past
/// FirstNormalObjectId that `type_free` does not read free (AGC-17: an enum, a domain over a free type, an array of one
/// and a composite of them run initdb's enum_*, the base's, array_* and record_* procs over their members' own; a user
/// base or range type, a pseudo-type member and a runaway nesting reach user code, as does a type the effects facts hold
/// no row for (fail closed, before any catalog read); a domain's CHECK runs only where a CoerceToDomain does, its own
/// reader), a node the walk cannot see, or a builtin that runs a query of its own (`effects::runs_own_query`, STABLE: its
/// own query runs what the relations it names call, a view's nextval: table_, schema_, database_to_xml[_and_xmlschema]).
/// An aggregate's ORDER BY and DISTINCT clauses hold no value (the values they sort are its argument entries, walked): a
/// SortGroupClause reads as its sort and equality operators, builtins or a user's code (D1902: an ordered aggregate over a
/// rerun set's join, `string_agg(.. ORDER BY ..)`, no longer makes the statement loud).
fn typed(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    crate::stack_check();
    if let Some(l) = n.as_list() {
        return l.iter().any(|c| typed(gx, c));
    }
    let ty = type_of(n).or_else(|| n.as_row_expr().map(|r| r.row_typeid)).or_else(|| n.as_array_coerce_expr().map(|a| a.resulttype));
    let mut any = ty.is_some_and(|t| t >= FIRST_NORMAL_OID && !(gx.facts.type_row(t).is_some() && super::type_free(gx.catalog, t, 0))) || n.as_func_expr().is_some_and(|f| gravity_decompose::effects::runs_own_query(f.funcid));
    let known = match n.as_aggref() {
        Some(a) => {
            let clause = |c: Node<'_>| c.as_sort_group_clause().is_none_or(|s| s.sortop >= FIRST_NORMAL_OID || s.eqop >= FIRST_NORMAL_OID);
            any |= a.aggdirectargs.iter().chain(a.args.iter()).chain(a.aggfilter).any(|c| typed(gx, c)) || a.aggorder.iter().chain(a.aggdistinct.iter()).any(clause);
            true
        }
        None => kids(n, &mut |c| any |= typed(gx, c)),
    };
    any || !known
}

/// `Ask::QuietEach`: an expression can raise because of a user type only when it calls that type's functions or casts or
/// compares through it, so one that holds a value of a user type (`typed`: a row-reading xml builtin too, or a node the
/// walk cannot see) reads as trunk (`Ask::Quiet` without `arr`), and any other reads PG 18.6's quiet array reads whatever
/// the statement holds elsewhere (a generate_series over int4 arguments next to a user range in another expression reads
/// quiet).
fn each(gx: &Gx<'_, '_>, n: Node<'_>, bound: &dyn Fn(i32) -> bool) -> bool {
    crate::stack_check();
    match n.as_list() {
        Some(l) => l.iter().all(|m| each(gx, m, bound)),
        None => gx.fx(n).quiet_by(bound, !typed(gx, n)),
    }
}

/// `Fx::check_acts`' walk: each CoerceToDomain of `n` whose domain holds a CHECK that acts (`domain_checks`), a CHECK read
/// by effects' reader and by this walk again (a nested domain).
fn casts<'p>(gx: &Gx<'_, 'p>, cx: &gravity_decompose::effects::Cx<'_, 'p>, n: Node<'p>, doms: &mut mcx::PgVec<'_, u32>) -> bool {
    crate::stack_check();
    let mut any = false;
    gravity_decompose::expr::walk(n, &mut |x| {
        any |= x.as_coerce_to_domain().is_some_and(|d| super::domain_checks(gx, d.resulttype, doms, &mut |e, doms| gravity_decompose::effects::of_expr(cx, e, &gravity_decompose::effects::PLAIN).acts || casts(gx, cx, e, doms)));
        !any
    });
    any
}

/// `Fx::calm_outside`' walk.
fn outside(gx: &Gx<'_, '_>, arena: gravity_program::ArenaRef, n: Node<'_>, placed: &dyn Fn(i32) -> bool) -> bool {
    crate::stack_check();
    if n.as_sub_plan().is_some_and(|s| placed(s.plan_id)) {
        return true;
    }
    let mut holds = false;
    gravity_decompose::expr::walk(n, &mut |x| {
        holds |= x.as_sub_plan().is_some_and(|s| placed(s.plan_id));
        !holds
    });
    if !holds {
        let fx = gx.fx(n);
        return fx.quiet() && !fx.user_or_volatile() && !fx.user_typed();
    }
    let e = gravity_decompose::effects::of_node(&gravity_decompose::effects::Cx::without_stmt(gx.facts, arena), n);
    let mut ok = !e.acts && !e.may_raise;
    let known = kids(n, &mut |c| ok &= outside(gx, arena, c, placed));
    ok && known
}

/// `Fx::quiet_before`' walk: everything PG evaluates before SubPlan `plan_id` inside `n` raises nothing; None when `n`
/// does not hold it.
fn before(gx: &Gx<'_, '_>, n: Node<'_>, plan_id: i32) -> Option<bool> {
    crate::stack_check();
    if let Some(s) = n.as_sub_plan() {
        return (s.plan_id == plan_id).then_some(true);
    }
    let (mut at, mut quiet) = (None, true);
    let known = kids(n, &mut |c| {
        if at.is_none() {
            match before(gx, c, plan_id) {
                Some(ok) => at = Some(ok && quiet),
                None => quiet &= gx.fx(c).quiet(),
            }
        }
    });
    at.map(|ok| ok && known)
}

/// `Ask::Loud`'s walk: `heard_at` at each node, a list's members; an unknown node counts.
fn heard(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, n: Node<'_>) -> bool {
    crate::stack_check(); // D3832
    if let Some(l) = n.as_list() {
        return l.iter().any(|c| heard(gx, p, c));
    }
    let mut any = heard_at(gx, p, n);
    let known = kids(n, &mut |c| any = any || heard(gx, p, c));
    any || !known
}

/// `Ask::LoudHere`: a call or operator when `effects::of_call` says it acts or may raise (an unknown function counts; a
/// user one is never quiet, whatever its marks; D3954: nor one whose input collation is indeterminate, `collation_set`),
/// another SubPlan of pipeline `p` by its step's facts (`step_loud`), an InitPlan's parameter (PG runs it at the read),
/// any node but a column, a constant, a plain parameter or a structural one (conservative beside nest/subplan.rs `own`).
fn heard_at(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, n: Node<'_>) -> bool {
    use types_nodes::NodeTag as T;
    let call = |f: u32| {
        let e = gravity_decompose::effects::of_call(gx.facts, f);
        e.acts || e.may_raise
    };
    let opfn = |o: u32, c: u32| if c != 0 { c } else { gx.facts.oper(o).map_or(0, |x| x.code) };
    let op = |o: u32, c: u32| call(opfn(o, c));
    // D3954: a call over arguments of different implicit collations has none (its input collation is indeterminate), and a
    // collation-aware builtin then raises at its first input (varlena.c check_collation_set): the effects home's
    // `collation_set` rule, as its quiet_node reads it (a builtin that reads no collation needs none)
    let unset = |f: u32, c: u32, a: &types_nodes::list::NodeList<'_>| !gravity_decompose::effects::collation_set(f, c, a);
    n.as_func_expr().map(|e| call(e.funcid) || unset(e.funcid, e.inputcollid, &e.args))
        .or_else(|| n.as_op_expr().map(|o| op(o.opno, o.opfuncid) || unset(opfn(o.opno, o.opfuncid), o.inputcollid, &o.args)))
        .or_else(|| n.as_sub_plan().map(|s| !p.ops.iter().any(|o| matches!(o, Op::SubPlan(t) if t.plan_id == s.plan_id && !step_loud(t)))))
        .or_else(|| n.as_param().map(|q| q.paramkind == ParamKind::PARAM_EXEC && crate::rerun::params::producer(gx, q.paramid) == Some(KindId::NestInitplan)))
        .unwrap_or(!matches!(n.node_tag(), T::T_Var | T::T_Const | T::T_TargetEntry | T::T_RelabelType | T::T_BoolExpr | T::T_NullTest | T::T_BooleanTest | T::T_CaseExpr | T::T_CaseWhen | T::T_CaseTestExpr | T::T_CoalesceExpr | T::T_RowExpr | T::T_CollateExpr))
}

/// A SubPlan step whose plan, test or arguments act or may raise (`Effects::of_sub_plan` over its desc's F5 facts).
pub(in crate::effects) fn step_loud(d: &gravity_decompose::ops::shared::subplan::SubPlanDesc<'_>) -> bool {
    let e = gravity_decompose::effects::Effects::of_sub_plan(d);
    e.acts || e.may_raise
}

/// `Ask::QuietCmp`: an OpExpr under a set input collation (two arguments of different implicit collations leave a
/// collation-aware comparison none, and it raises at its first row: varlena.c check_collation_set) whose function the
/// home reads quiet (`effects::quiet_call`: a leakproof builtin or `types_core::quiet::call`, e.g. numeric_gt), each
/// argument raising nothing or such a comparison.
fn cmp(gx: &Gx<'_, '_>, n: Node<'_>, bound: &dyn Fn(i32) -> bool) -> bool {
    crate::stack_check();
    n.as_op_expr().is_some_and(|o| {
        gravity_decompose::effects::collation_set(o.opfuncid, o.inputcollid, &o.args)
            && gravity_decompose::effects::quiet_call(gx.facts, o.opfuncid)
            && o.args.iter().all(|a| gx.fx(a).quiet_by(bound, false) || cmp(gx, a, bound))
    })
}

/// `Ask::Comparand`: an OpExpr whose function the home reads quiet (`effects::quiet_call`), each argument raising nothing
/// or reading no column (a walk that meets no Var and knows every node).
fn comparand(gx: &Gx<'_, '_>, n: Node<'_>, bound: &dyn Fn(i32) -> bool) -> bool {
    let columnless = |a: Node<'_>| {
        let mut col = false;
        gravity_decompose::expr::walk(a, &mut |x| {
            col |= x.as_var().is_some();
            true
        }) && !col
    };
    n.as_op_expr().is_some_and(|o| gravity_decompose::effects::quiet_call(gx.facts, o.opfuncid) && o.args.iter().all(|a| gx.fx(a).quiet_by(bound, false) || columnless(a)))
}

/// `Ask::UserClause` (visible/drain.rs `user_clause` says what it reads): no operator clause, an operator or function past
/// the builtin catalog, an operator in no builtin hash family or not calling its oprcode, or a polymorphic / record
/// equality over an operand whose type `type_free` does not read free.
fn hash_reach(gx: &Gx<'_, '_>, c: Node<'_>) -> bool {
    let Some(o) = c.as_op_expr() else { return true };
    let props = gravity_decompose::effects::builtin_oper(o.opno);
    let free = |n: Node<'_>| type_of(n).is_some_and(|t| super::type_free(gx.catalog, t, 0));
    let poly_free = || o.args.len() == 2 && o.args.iter().all(free);
    o.opno >= FIRST_NORMAL_OID || o.opfuncid >= FIRST_NORMAL_OID || props.is_none_or(|p| p.hash_family == 0 || (p.poly && !poly_free()) || p.code != o.opfuncid)
}

/// Expression `n` of a PlannedStmt's tree, read in admit before any `Gx` exists (a Catalog; the statement's subplans
/// where an answer reads them): the home's second producer, beside `Fx` (`PlanFx::ask`).
#[derive(Clone, Copy)]
pub(crate) struct PlanFx<'c, 'a> {
    cat: &'c dyn Catalog<'a>,
    n: Node<'a>,
}

/// A question a plan-tree decider asks of one expression.
#[derive(Clone, Copy)]
enum PlanAsk<'s, 'a> {
    /// D2651: the expression makes no call a rewind's re-evaluation could show (`again`), a SubPlan's plan read through
    /// the statement's subplans.
    RewindQuiet(&'s types_nodes::list::OptNodeList<'a>),
    /// D3509: the expression gives the same value at a second evaluation over the same row (`steady`).
    Fixed,
}

impl<'c, 'a> PlanFx<'c, 'a> {
    /// Expression `n`'s plan-tree answers under catalog `cat`.
    #[inline]
    pub(crate) fn of(cat: &'c dyn Catalog<'a>, n: Node<'a>) -> Self {
        PlanFx { cat, n }
    }

    /// The one producer of plan-tree answers.
    #[inline(always)]
    fn ask(&self, q: PlanAsk<'_, 'a>) -> bool {
        match q {
            PlanAsk::RewindQuiet(sp) => again(sp, self.cat, self.n),
            PlanAsk::Fixed => steady(self.cat, self.n),
        }
    }

    /// D2651: the expression makes no call a rewind's re-evaluation could show (visible.rs `rewind_quiet`); `sp`: the
    /// statement's subplans.
    #[inline]
    pub(crate) fn rewind_quiet(&self, sp: &types_nodes::list::OptNodeList<'a>) -> bool {
        self.ask(PlanAsk::RewindQuiet(sp))
    }

    /// D3509: the expression gives the same value at a second evaluation over the same row (visible.rs `rewind_keeps`).
    #[inline]
    pub(crate) fn fixed(&self) -> bool {
        self.ask(PlanAsk::Fixed)
    }
}

/// `PlanAsk::Fixed`'s walk (visible.rs `rewind_keeps`): expression `n` gives the same value at a second evaluation over
/// the same row: whitelisted nodes and built-in IMMUTABLE functions and operators only (no STABLE call reading a setting,
/// no SubPlan, no aggregate).
fn steady<'a>(cat: &dyn Catalog<'a>, n: Node<'a>) -> bool {
    crate::stack_check();
    use types_nodes::NodeTag as T;
    let immutable = |f: u32| f != 0 && f < types_core::catalog::FirstNormalObjectId && cat.func(f).is_ok_and(|x| x.volatility == Some(b'i'));
    let own = match n.node_tag() {
        T::T_Var
        | T::T_Const
        | T::T_Param
        | T::T_CaseTestExpr
        | T::T_TargetEntry
        | T::T_BoolExpr
        | T::T_RelabelType
        | T::T_CaseExpr
        | T::T_CaseWhen
        | T::T_CoalesceExpr
        | T::T_NullTest
        | T::T_BooleanTest
        | T::T_RowExpr
        | T::T_ArrayExpr
        | T::T_FieldSelect => true,
        T::T_List => return n.as_list().is_some_and(|l| l.iter().all(|c| steady(cat, c))),
        T::T_FuncExpr => n.as_func_expr().is_some_and(|f| immutable(f.funcid)),
        T::T_OpExpr => n.as_op_expr().is_some_and(|o| immutable(o.opfuncid)),
        T::T_DistinctExpr => n.as_distinct_expr().is_some_and(|o| immutable(o.opfuncid)),
        T::T_NullIfExpr => n.as_null_if_expr().is_some_and(|o| immutable(o.opfuncid)),
        T::T_ScalarArrayOpExpr => n.as_scalar_array_op_expr().is_some_and(|s| immutable(s.opfuncid) && [s.hashfuncid, s.negfuncid].into_iter().all(|f| f == 0 || immutable(f))),
        _ => false,
    };
    if !own {
        return false;
    }
    let mut all = true;
    let known = kids(n, &mut |c| all = all && steady(cat, c));
    known && all
}

/// `PlanAsk::RewindQuiet`'s walk (visible.rs `rewind_quiet`): expression `n` makes no call a rewind's re-evaluation could
/// show.
fn again<'a>(sp: &types_nodes::list::OptNodeList<'a>, cat: &dyn Catalog<'a>, n: Node<'a>) -> bool {
    crate::stack_check();
    use types_nodes::NodeTag as T;
    let builtin = |f: u32| f != 0 && f < types_core::catalog::FirstNormalObjectId && cat.func(f).is_ok_and(|x| x.volatility.is_some_and(|v| v != b'v'));
    let own = match n.node_tag() {
        T::T_Var
        | T::T_Const
        | T::T_Param
        | T::T_CaseTestExpr
        | T::T_SQLValueFunction
        | T::T_TargetEntry
        | T::T_BoolExpr
        | T::T_RelabelType
        | T::T_CaseExpr
        | T::T_CaseWhen
        | T::T_CoalesceExpr
        | T::T_NullTest
        | T::T_BooleanTest
        | T::T_RowExpr
        | T::T_ArrayExpr
        | T::T_FieldSelect => true,
        T::T_List => return n.as_list().is_some_and(|l| l.iter().all(|c| again(sp, cat, c))),
        T::T_FuncExpr => n.as_func_expr().is_some_and(|f| builtin(f.funcid)),
        T::T_OpExpr => n.as_op_expr().is_some_and(|o| builtin(o.opfuncid)),
        T::T_DistinctExpr => n.as_distinct_expr().is_some_and(|o| builtin(o.opfuncid)),
        T::T_NullIfExpr => n.as_null_if_expr().is_some_and(|o| builtin(o.opfuncid)),
        T::T_ScalarArrayOpExpr => n.as_scalar_array_op_expr().is_some_and(|s| builtin(s.opfuncid) && [s.hashfuncid, s.negfuncid].into_iter().all(|f| f == 0 || builtin(f))),
        // a built-in aggregate (no user transition code) without ORDER BY / DISTINCT comparisons
        T::T_Aggref => n.as_aggref().is_some_and(|a| a.aggfnoid != 0 && a.aggfnoid < types_core::catalog::FirstNormalObjectId && a.aggorder.len() == 0 && a.aggdistinct.len() == 0),
        // an uncorrelated hashed SubPlan keeps its table at a rescan that changes no param (ExecHashSubPlan builds again
        // only under chgParam): only its test, walked below, runs again; any other runs its plan again per call
        T::T_SubPlan => n.as_sub_plan().is_some_and(|s| {
            (s.useHashTable && s.args.len() == 0 && s.parParam.len() == 0) || (s.plan_id >= 1 && super::visible::per_call_quiet(sp, cat, sp.nth((s.plan_id - 1) as usize)))
        }),
        _ => false,
    };
    if !own {
        return false;
    }
    let mut all = true;
    let known = kids(n, &mut |c| all = all && again(sp, cat, c));
    known && all
}
