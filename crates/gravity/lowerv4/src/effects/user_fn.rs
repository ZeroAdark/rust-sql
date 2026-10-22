//! The lowering's USER-FN walk: whether evaluating an expression may run user code. The recursion is shared
//! (`expr::kids`), and so is the rule for a called function (decompose `effects::runs_user_code`, the one home: user code
//! unless PostgreSQL's own, a builtin the PROCS table holds or a SQL function initdb wrote whose catalog row is still
//! initdb's, read through `Facts`, and no builtin that runs code named at run time); what
//! differs between the questions that ask it is named in a `Reading`: which nodes are read, whether a node the walk cannot
//! see counts, and the node kinds that run no code of their own. The readings differ both ways (a user function in a
//! DISTINCT, a domain cast), so none stands for another.
//!
//! The readings: `calls` (effects.rs `user_fn`), `hidden` (D1906, effects/visible/hashjoin.rs `hidden`) and `params_key`
//! (D2071, rerun/params_key.rs `user_code`).
use gravity_decompose::catalog::typefacts::FIRST_NORMAL_OID;
use gravity_decompose::catalog::Facts;
use types_nodes::{Node, NodeTag};

/// Which nodes are read.
#[derive(Clone, Copy)]
pub(crate) enum Kinds {
    /// A FuncExpr's function and an OpExpr's, as the node stores it.
    FuncOp,
    /// D1871 (effects.rs `calls`): a FuncExpr's, OpExpr's, DistinctExpr's and NullIfExpr's function, and a
    /// ScalarArrayOpExpr's operator, hash and negator functions.
    Calls,
    /// D1906: the calls a hash join's side class does not read (it reads a FuncExpr's and an OpExpr's function): a
    /// ScalarArrayOpExpr's, DistinctExpr's or NullIfExpr's operator (a user's past FIRST_NORMAL_OID) or functions, a
    /// RowCompareExpr's operators, a CoerceViaIO to or from a user type (or from a type the walk cannot name), a row-reading
    /// xml builtin (its own query runs what the relations it names call: a view's nextval), and every domain cast,
    /// NextValueExpr, SubPlan and AlternativeSubPlan.
    Hidden,
}

/// One USER-FN reading: what `walk` counts.
#[derive(Clone, Copy)]
pub(crate) struct Reading {
    pub(crate) kinds: Kinds,
    /// A node `expr::kids` cannot see through counts (fail closed); otherwise it passes, its kids unread.
    pub(crate) unknown: bool,
    /// The node kinds that run no code of their own: a node `kinds` does not read and this list does not name counts.
    /// None: such a node passes.
    pub(crate) allow: Option<&'static [NodeTag]>,
    /// `Hidden`: a SubPlan node counts (fail closed); false leaves it to the caller, who reads the SubPlan's own parts
    /// (D3566, effects.rs `hides_calls`). An AlternativeSubPlan always counts.
    pub(crate) subplans: bool,
    /// D3587: a user function LANGUAGE internal over a builtin C function (decompose `effects::body`, KeySem) runs that
    /// builtin, PostgreSQL's own code, so it counts as the builtin does.
    pub(crate) body: bool,
}

impl Reading {
    /// effects.rs `user_fn`: a call D1871's `calls` reads whose function is user code (decompose `effects::runs_user_code`);
    /// a node the walk cannot see passes.
    pub(crate) const fn calls() -> Reading {
        Reading { kinds: Kinds::Calls, unknown: false, allow: None, subplans: true, body: false }
    }

    /// D3587 (effects.rs `hides_calls`): `calls`, a user function LANGUAGE internal over a builtin read as that builtin.
    pub(crate) const fn calls_body() -> Reading {
        Reading { kinds: Kinds::Calls, unknown: false, allow: None, subplans: true, body: true }
    }

    /// D1906 (effects/visible/hashjoin.rs `hidden`): a call the side class does not read, or a node the walk cannot see.
    pub(crate) const fn hidden() -> Reading {
        Reading { kinds: Kinds::Hidden, unknown: true, allow: None, subplans: true, body: false }
    }

    /// D3566 (effects.rs `hides_calls`): `hidden` with SubPlan nodes left to the caller, who reads each SubPlan's parts.
    pub(crate) const fn hidden_parts() -> Reading {
        Reading { kinds: Kinds::Hidden, unknown: true, allow: None, subplans: false, body: false }
    }

    /// D2071 (rerun/params_key.rs `user_code`): a FuncExpr's or OpExpr's function that is user code
    /// (`effects::runs_user_code`), a node of a kind `PLAIN` does not name, or a node the walk cannot see.
    pub(crate) const fn params_key() -> Reading {
        Reading { kinds: Kinds::FuncOp, unknown: true, allow: Some(PLAIN), subplans: true, body: false }
    }
}

/// D2071's node kinds that run no code of their own.
const PLAIN: &[NodeTag] = &[
    NodeTag::T_Var,
    NodeTag::T_Const,
    NodeTag::T_Param,
    NodeTag::T_RelabelType,
    NodeTag::T_CollateExpr,
    NodeTag::T_CaseTestExpr,
    NodeTag::T_BoolExpr,
    NodeTag::T_NullTest,
    NodeTag::T_BooleanTest,
    NodeTag::T_CaseExpr,
    NodeTag::T_CaseWhen,
    NodeTag::T_CoalesceExpr,
    NodeTag::T_RowExpr,
];

/// Whether reading `r` counts any node of `n`; `f` answers for the catalog rows of the functions it calls.
pub(in crate::effects) fn walk(f: &dyn Facts, n: Node<'_>, r: &Reading) -> bool {
    walk_by(f, n, r, &|_| false)
}

/// `walk`, with `free` the user types whose output runs no user code (effects.rs `type_free`: a composite, array, enum
/// or domain of built-in types, whose record_out / array_out / enum_out call only built-in output functions); `Kinds::Hidden`
/// reads a CoerceViaIO from such a type as built-in code.
pub(in crate::effects) fn walk_by(f: &dyn Facts, n: Node<'_>, r: &Reading, free: &dyn Fn(u32) -> bool) -> bool {
    crate::stack_check();
    let mut any = own(f, n, r, free);
    let known = gravity_decompose::expr::kids(n, &mut |c| any = any || walk_by(f, c, r, free));
    any || r.unknown && !known
}

/// Whether node `n` itself counts.
fn own(f: &dyn Facts, n: Node<'_>, r: &Reading, free: &dyn Fn(u32) -> bool) -> bool {
    // D3923: a builtin body that runs code named at run time is not PostgreSQL's own code alone (`effects::runs_user_code`)
    let own_body = |fid: u32| gravity_decompose::effects::body(f, fid).is_some_and(|b| !gravity_decompose::effects::runs_named_code(b));
    let call = |fid: u32| gravity_decompose::effects::runs_user_code(f, fid) && !(r.body && own_body(fid));
    let read = match r.kinds {
        Kinds::FuncOp => n.as_func_expr().map(|x| x.funcid).or_else(|| n.as_op_expr().map(|o| o.opfuncid)).map(call),
        Kinds::Calls => {
            let c = super::calls(n);
            c[0].is_some().then(|| c.into_iter().flatten().any(call))
        }
        Kinds::Hidden => Some(hidden(n, &call, free) || r.subplans && n.node_tag() == NodeTag::T_SubPlan),
    };
    read.unwrap_or_else(|| r.allow.is_some_and(|a| !a.contains(&n.node_tag())))
}

/// `Kinds::Hidden`'s leaf: node `n` makes a call the side class does not read; `call` says when a function is user code.
/// A CoerceViaIO runs its source type's output function and its result type's input function: a user type's counts,
/// unless the source is one `free` names (record output over built-in columns: a whole-row cast to text, a recursive
/// query's path rows). The result side stays as it was: a composite's input runs its columns' (a domain's CHECK too).
fn hidden(n: Node<'_>, call: &dyn Fn(u32) -> bool, free: &dyn Fn(u32) -> bool) -> bool {
    let user = |o: u32| o >= FIRST_NORMAL_OID;
    let out = |t: u32| user(t) && !free(t);
    n.as_scalar_array_op_expr().is_some_and(|o| user(o.opno) || call(o.opfuncid) || [o.hashfuncid, o.negfuncid].into_iter().any(|x| x != 0 && call(x)))
        || n.as_distinct_expr().is_some_and(|o| user(o.opno) || call(o.opfuncid))
        || n.as_null_if_expr().is_some_and(|o| user(o.opno) || call(o.opfuncid))
        || n.as_row_compare_expr().is_some_and(|r| r.opnos.iter().any(user))
        || n.as_coerce_via_io().is_some_and(|c| user(c.resulttype) || gravity_decompose::catalog::expr_type::type_of(c.arg).is_none_or(out))
        || n.as_func_expr().is_some_and(|f| gravity_decompose::effects::runs_own_query(f.funcid))
        || matches!(n.node_tag(), NodeTag::T_CoerceToDomain | NodeTag::T_NextValueExpr | NodeTag::T_AlternativeSubPlan)
}

/// hashjoin/probe.rs `nest_held`'s and mergejoin/held_inner.rs's node test (a fill whose error op 14 holds): builtin code only, as far as the walk sees (a user function or operator by
/// `effects::user_fn` and `hidden`'s kinds: a user operator a ScalarArrayOpExpr, DistinctExpr, NullIfExpr or RowCompareExpr
/// runs, a user type's I/O, a row-reading xml builtin), a domain only an initdb one (information_schema's: its CHECK is
/// initdb code), no nextval node and no SubPlan.
pub(crate) fn held_node(gx: &crate::graph::Gx<'_, '_>, n: types_nodes::Node<'_>) -> bool {
    use gravity_decompose::effects::user_code as user;
    use types_nodes::NodeTag;
    let mut bad = crate::effects::user_fn(gx, n);
    let seen = gravity_decompose::expr::walk(n, &mut |x| {
        bad |= x.as_scalar_array_op_expr().is_some_and(|o| [o.opno, o.opfuncid, o.hashfuncid, o.negfuncid].into_iter().any(user))
            || x.as_distinct_expr().is_some_and(|o| user(o.opno) || user(o.opfuncid))
            || x.as_null_if_expr().is_some_and(|o| user(o.opno) || user(o.opfuncid))
            || x.as_row_compare_expr().is_some_and(|r| r.opnos.iter().any(user))
            || x.as_coerce_via_io().is_some_and(|c| user(c.resulttype) || gravity_decompose::catalog::expr_type::type_of(c.arg).is_none_or(user))
            || x.as_coerce_to_domain().is_some_and(|d| user(d.resulttype))
            || x.as_func_expr().is_some_and(|f| gravity_decompose::effects::runs_own_query(f.funcid))
            || matches!(x.node_tag(), NodeTag::T_NextValueExpr | NodeTag::T_SubPlan | NodeTag::T_AlternativeSubPlan);
        !bad
    });
    !bad && seen
}
