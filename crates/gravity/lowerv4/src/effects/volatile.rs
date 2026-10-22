//! The lowering's VOLATILE walk: whether evaluating an expression makes a call whose count or moment the user could see.
//! The recursion is shared (`expr::kids`); what differs between the questions that ask it is named in a `Reading`: which
//! nodes' functions are calls, where a call's volatility is read, whether a node the walk cannot see counts, how a cast to
//! a domain counts, and the further leaf classes of join.drain's spooled form. The readings differ both ways (a DISTINCT
//! over a VOLATILE function, a function the catalog cannot answer, a domain's CHECK), so none stands for another.
//!
//! The readings: `catalog` (effects.rs `volatile`), `visible` (effects/visible.rs `calls`' own call test), `check` (D1899's
//! domain CHECKs, effects.rs `volatile_check`), `spool` (join.drain's spooled form, effects/visible/drain.rs `acts`) and
//! `draw` (effects/visible.rs `call_order_blind`, negated); `any`, their fail-closed union, is what T2's collapse reads.
use gravity_decompose::effects::{call_acts, of_call, user_code, Volatility};
use types_nodes::{Node, NodeTag};

use crate::graph::Gx;

/// Which nodes' functions are calls.
#[derive(Clone, Copy)]
pub(crate) enum Kinds {
    /// No node is a call.
    None,
    /// A FuncExpr's function and an OpExpr's, as the node stores it.
    FuncOp,
    /// D1871 (effects.rs `calls`): a FuncExpr's, OpExpr's, DistinctExpr's and NullIfExpr's function, and a
    /// ScalarArrayOpExpr's operator, hash and negator functions.
    Calls,
    /// A FuncExpr's function, and an OpExpr's, DistinctExpr's, NullIfExpr's and ScalarArrayOpExpr's operator by its
    /// function: the node's, or pg_operator's oprcode when the node stores none (an operator that gives neither counts).
    Resolved,
    /// `Resolved`, plus a ScalarArrayOpExpr's hash and negator functions, each RowCompareExpr operator, an Aggref's and a
    /// WindowFunc's function, and a CoerceViaIO's output and input functions (a type the facts cannot answer counts).
    Visible,
}

/// Where a call's volatility is read.
#[derive(Clone, Copy)]
pub(crate) enum Call {
    /// pg_proc's provolatile 'v' as the catalog holds it; a function the catalog cannot answer counts as `unknown` says.
    Catalog { unknown: bool },
    /// The effects facts: VOLATILE, or a function the facts cannot answer.
    Facts,
    /// A visible effect: user code (decompose `effects::user_code`, D2044), a builtin that may raise a NOTICE (effects.rs
    /// `may_notice`), or one that acts by its own row (decompose `effects::of_call`: VOLATILE, or a function the catalog
    /// cannot answer).
    Effect,
    /// A call the catalog does not show non-VOLATILE that is no independent random draw (`DRAWS`).
    NotDraw,
    /// D4124 (effects/visible.rs `order_shown`): a call pg_proc marks VOLATILE (or cannot answer) whose place among the
    /// statement's calls shows in a value: user code (decompose `effects::user_code`) or a function the facts cannot
    /// answer, a sequence or clock call (types_core quiet `CALL_LOCAL_ACT` but its random draws, `DRAWS`), and a call that
    /// reads or sets their state (`CALL_LOCAL_STATE`). A builtin that reads server state (pg_lock_status,
    /// pg_relation_size) changes nothing another call reads, so its place shows in no value.
    Order,
    /// D3816: a call that acts by the effects home's one answer (decompose `effects::call_acts`: VOLATILE, or a function
    /// the facts cannot answer, but not a pinned non-acting builtin, a disk size or the lock status).
    Acting,
}

/// How a cast to a domain (CoerceToDomain) counts.
#[derive(Clone, Copy)]
pub(crate) enum Domain {
    /// As any other node: its argument is walked.
    None,
    /// D1899: when the domain chain's CHECK constraints make a call the `check(true)` reading counts (effects.rs
    /// `domain_checks`).
    Check,
    /// Always: a CHECK may call anything.
    Any,
}

/// One VOLATILE reading: what `walk` counts.
#[derive(Clone, Copy)]
pub(crate) struct Reading<'r> {
    pub(crate) kinds: Kinds,
    pub(crate) call: Call,
    /// A node `expr::kids` cannot see through counts (fail closed); otherwise it passes, its kids unread.
    pub(crate) unknown: bool,
    pub(crate) domain: Domain,
    /// A List reads as its members (any of them counts); otherwise it is a node the walk cannot see.
    pub(crate) list: bool,
    /// join.drain's spooled form's further leaves: GREATEST / LEAST over a type whose compare is not `cmp_free`, a SubPlan
    /// no op of the pipeline evaluates (these are the ids its ops evaluate), a sequence's NextValueExpr, the XML / JSON
    /// nodes, and RETURNING's OLD / NEW image of a view column (PG 18's ReturningExpr) read as its expression.
    pub(crate) spool: Option<&'r [i32]>,
}

impl<'r> Reading<'r> {
    /// effects.rs `volatile` (D1005 / D1871): a call node's function pg_proc marks VOLATILE; a function the catalog cannot
    /// answer and a node the walk cannot see do not count.
    pub(crate) const fn catalog() -> Reading<'static> {
        Reading { kinds: Kinds::Calls, call: Call::Catalog { unknown: false }, unknown: false, domain: Domain::None, list: false, spool: None }
    }

    /// The fail-closed union of `catalog`, `visible` and `check(false)` (T2): a call a FuncExpr, OpExpr, DistinctExpr,
    /// NullIfExpr or ScalarArrayOpExpr (its hash and negator functions with it) makes whose function pg_proc marks VOLATILE
    /// or cannot answer, and a cast to a domain whose CHECKs make one; a node the walk cannot see passes, as in every
    /// reading outside a CHECK.
    pub(crate) const fn any() -> Reading<'static> {
        Reading { kinds: Kinds::Calls, call: Call::Catalog { unknown: true }, unknown: false, domain: Domain::Check, list: false, spool: None }
    }

    /// effects/visible.rs `calls`' own call test: a FuncExpr's or OpExpr's function pg_proc marks VOLATILE, or one the
    /// catalog cannot answer.
    pub(crate) const fn visible() -> Reading<'static> {
        Reading { kinds: Kinds::FuncOp, call: Call::Catalog { unknown: true }, unknown: false, domain: Domain::None, list: false, spool: None }
    }

    /// D1899 (effects.rs `volatile_check`): outside a CHECK only a cast to a domain whose CHECKs make a VOLATILE call
    /// counts; `inside` one, every call the facts read VOLATILE or cannot answer, and every node the walk cannot see.
    pub(crate) const fn check(inside: bool) -> Reading<'static> {
        Reading { kinds: if inside { Kinds::Resolved } else { Kinds::None }, call: Call::Facts, unknown: inside, domain: Domain::Check, list: false, spool: None }
    }

    /// join.drain's spooled form (effects/visible/drain.rs `acts`, F5's coverage): a visible effect of any kind, every
    /// domain cast and every node the walk cannot see. `subs`: the SubPlans the expression's pipeline's ops evaluate.
    pub(crate) const fn spool(subs: &'r [i32]) -> Reading<'r> {
        Reading { kinds: Kinds::Visible, call: Call::Effect, unknown: true, domain: Domain::Any, list: true, spool: Some(subs) }
    }

    /// D3816 (visible/hashjoin.rs `side_acts`): `any` with a pinned non-acting builtin's call not counted (`Call::Acting`).
    pub(crate) const fn acting() -> Reading<'static> {
        Reading { kinds: Kinds::Calls, call: Call::Acting, unknown: false, domain: Domain::Check, list: false, spool: None }
    }

    /// effects/visible.rs `call_order_blind`, negated, over the union's call nodes and domain casts: a call D1871's
    /// nodes make whose function is not shown non-VOLATILE and is no random draw, or a cast to a domain whose CHECKs make a
    /// VOLATILE call.
    pub(crate) const fn draw() -> Reading<'static> {
        Reading { kinds: Kinds::Calls, call: Call::NotDraw, unknown: false, domain: Domain::Check, list: false, spool: None }
    }

    /// effects/visible.rs `order_shown` (D4124): a call D1871's nodes make whose place among the statement's calls shows
    /// (`Call::Order`). A domain CHECK's calls are not read: a shape that hides its sequence call there stays D4132's.
    pub(crate) const fn order() -> Reading<'static> {
        Reading { kinds: Kinds::Calls, call: Call::Order, unknown: false, domain: Domain::None, list: false, spool: None }
    }
}

/// Whether reading `r` counts any node of `n`.
pub(in crate::effects) fn walk(gx: &Gx<'_, '_>, n: Node<'_>, r: &Reading<'_>) -> bool {
    walk_in(gx, n, r, &mut mcx::PgVec::new_in(gx.mcx))
}

/// `walk` with the domains whose CHECKs are being read (a domain met again inside its own checks counts).
fn walk_in(gx: &Gx<'_, '_>, n: Node<'_>, r: &Reading<'_>, doms: &mut mcx::PgVec<'_, u32>) -> bool {
    crate::stack_check();
    if r.list {
        if let Some(l) = n.as_list() {
            return l.iter().any(|x| walk_in(gx, x, r, doms));
        }
    }
    if r.spool.is_some() {
        if let Some(e) = n.as_returning_expr() {
            return walk_in(gx, e.retexpr, r, doms);
        }
    }
    let mut any = own_call(gx, n, r) || own_domain(gx, n, r, doms) || r.spool.is_some_and(|subs| own_spool(gx, n, subs));
    let agg = n.as_aggref();
    let known = gravity_decompose::expr::kids(n, &mut |c| {
        any = any || match (agg, c.as_sort_group_clause()) {
            (Some(a), Some(s)) => sort_item(gx, a, s, r),
            _ => walk_in(gx, c, r, doms),
        }
    });
    any || r.unknown && !known
}

/// D3770 (SQLAlchemy's enum load: `array_agg(enumlabel::text ORDER BY enumsortorder)` read past a hash join's probe): an
/// Aggref's ORDER BY or DISTINCT item, a SortGroupClause (no expression: `expr::kids` gives it, `walk_in` cannot see it),
/// is its sort and equality operators, run as the aggregate sorts or de-duplicates its input. Where the reading counts
/// operators by their function (`Resolved`, `Visible`) each counts as an operator call does, and a polymorphic one (array,
/// record, range: its procs run the element, field or subtype's, as `user_clause`'s hash clauses) unless its item's type
/// reads free (`type_free`); an operator outside the builtin catalog counts. Any other reading keeps it unseen.
fn sort_item(gx: &Gx<'_, '_>, a: &types_nodes::primnodes::Aggref<'_>, s: &types_nodes::parsenodes::SortGroupClause, r: &Reading<'_>) -> bool {
    if !matches!(r.kinds, Kinds::Resolved | Kinds::Visible) {
        return r.unknown;
    }
    let item = a.args.iter().filter_map(|t| t.as_target_entry()).find(|t| t.ressortgroupref == s.tleSortGroupRef);
    let free = item.and_then(|t| gravity_decompose::catalog::expr_type::type_of(t.expr)).is_some_and(|t| super::type_free(gx.catalog, t, 0));
    [s.sortop, s.eqop].into_iter().filter(|&o| o != 0).any(|o| {
        let poly = gravity_decompose::effects::builtin_oper(o).is_none_or(|p| p.poly && !free);
        poly || resolved(gx, o, 0).is_none_or(|f| counts(gx, r.call, f))
    })
}

/// Whether node `n` itself makes a call `r` counts.
fn own_call(gx: &Gx<'_, '_>, n: Node<'_>, r: &Reading<'_>) -> bool {
    let call = |f: u32| counts(gx, r.call, f);
    let op = |opno: u32, f: u32| resolved(gx, opno, f).is_none_or(call);
    match r.kinds {
        Kinds::None => false,
        Kinds::FuncOp => n.as_func_expr().map(|f| f.funcid).or_else(|| n.as_op_expr().map(|o| o.opfuncid)).is_some_and(call),
        Kinds::Calls => super::calls(n).into_iter().flatten().any(call),
        Kinds::Resolved | Kinds::Visible => {
            let visible = matches!(r.kinds, Kinds::Visible);
            if let Some(f) = n.as_func_expr() {
                call(f.funcid)
            } else if let Some(o) = n.as_op_expr() {
                op(o.opno, o.opfuncid)
            } else if let Some(o) = n.as_distinct_expr() {
                op(o.opno, o.opfuncid)
            } else if let Some(o) = n.as_null_if_expr() {
                op(o.opno, o.opfuncid)
            } else if let Some(s) = n.as_scalar_array_op_expr() {
                op(s.opno, s.opfuncid) || visible && [s.hashfuncid, s.negfuncid].into_iter().any(|f| f != 0 && call(f))
            } else if !visible {
                false
            } else if let Some(c) = n.as_row_compare_expr() {
                c.opnos.iter().any(|o| op(o, 0))
            } else if let Some(a) = n.as_aggref() {
                call(a.aggfnoid)
            } else if let Some(w) = n.as_window_func() {
                call(w.winfnoid)
            } else if let Some(c) = n.as_coerce_via_io() {
                let out = gravity_decompose::catalog::expr_type::type_of(c.arg).and_then(|t| gx.facts.type_io(t)).map(|t| t.1);
                [out, gx.facts.type_io(c.resulttype).map(|t| t.0)].into_iter().any(|f| f.is_none_or(call))
            } else {
                false
            }
        }
    }
}

/// Whether node `n` is a cast to a domain `r` counts.
fn own_domain(gx: &Gx<'_, '_>, n: Node<'_>, r: &Reading<'_>, doms: &mut mcx::PgVec<'_, u32>) -> bool {
    match r.domain {
        Domain::None => false,
        Domain::Check => n.as_coerce_to_domain().is_some_and(|d| super::domain_checks(gx, d.resulttype, doms, &mut |e, doms| walk_in(gx, e, &Reading::check(true), doms))),
        Domain::Any => n.as_coerce_to_domain().is_some(),
    }
}

/// Whether node `n` is one of the spooled form's further leaves (`Reading::spool`); `subs` as there.
fn own_spool(gx: &Gx<'_, '_>, n: Node<'_>, subs: &[i32]) -> bool {
    if let Some(m) = n.as_min_max_expr() {
        !super::cmp_free(gx.catalog, m.minmaxtype)
    } else if let Some(s) = n.as_sub_plan() {
        !subs.contains(&s.plan_id)
    } else {
        matches!(
            n.node_tag(),
            NodeTag::T_NextValueExpr | NodeTag::T_XmlExpr | NodeTag::T_JsonExpr | NodeTag::T_JsonConstructorExpr | NodeTag::T_JsonIsPredicate | NodeTag::T_JsonValueExpr | NodeTag::T_JsonBehavior
        )
    }
}

/// Whether call `f` counts by `call`.
fn counts(gx: &Gx<'_, '_>, call: Call, f: u32) -> bool {
    match call {
        Call::Catalog { unknown } => gx.catalog.func(f).map_or(unknown, |x| x.volatility == Some(b'v')),
        Call::Facts => gx.facts.func(f).is_none_or(|p| p.volatility == Volatility::Volatile),
        Call::Effect => user_code(f) || super::may_notice(f) || of_call(gx.facts, f).acts,
        Call::NotDraw => !(gx.catalog.func(f).is_ok_and(|x| x.volatility.is_some_and(|v| v != b'v')) || draw(f)),
        Call::Order => {
            use types_core::quiet::{call_local_act, call_local_state};
            gx.catalog.func(f).map_or(true, |x| x.volatility == Some(b'v')) && (user_code(f) || gx.facts.func(f).is_none() || call_local_act(f) && !draw(f) || call_local_state(f))
        }
        Call::Acting => call_acts(gx.facts, f),
    }
}

/// An operator's function: the node's, else pg_operator's oprcode (none: neither gives one).
fn resolved(gx: &Gx<'_, '_>, opno: u32, f: u32) -> Option<u32> {
    if f != 0 {
        Some(f)
    } else {
        gx.facts.oper(opno).map(|o| o.code).filter(|&c| c != 0)
    }
}

/// The builtins whose every VOLATILE call is an independent random draw (by the builtin they run): random(),
/// random(min, max), random_normal(), gen_random_uuid() / uuidv4().
const DRAWS: [&str; 6] = ["drandom", "drandom_normal", "int4random", "int8random", "numeric_random", "gen_random_uuid"];

/// Function `f` runs one of `DRAWS`.
fn draw(f: u32) -> bool {
    gravity_fn::row_of(f).is_some_and(|r| DRAWS.contains(&r.name))
}
