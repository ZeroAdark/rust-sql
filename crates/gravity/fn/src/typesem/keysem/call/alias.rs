//! D3402: a LANGUAGE sql comparator whose body is one built-in call over its two arguments (`SELECT btint8cmp($2, $1)`,
//! a reverse order class's support 1) is bound as that built-in, its arguments permuted (`Alias`): the bound-call table
//! calls the built-in, and no statement runs per compared pair. PostgreSQL runs the body as a statement at every call
//! (functions.c `fmgr_sql`); its answer is the built-in's over the same two values, and these gates keep every other
//! thing that statement does out of reach of an observer:
//! - the comparator is LANGUAGE sql, kind `f`, not SECURITY DEFINER, with no SET clause, not set-returning, with two
//!   arguments, returning int4;
//! - its body, as the planner inlines it (`clauses_seams::inline_sql_function`, the reading D3082's `sql_inlined` makes),
//!   is a FuncExpr or OpExpr of a built-in (LANGUAGE internal, below FirstNormalObjectId) whose two arguments are `$1`
//!   and `$2` (at most relabelled), under no input collation, returning the comparator's type; the binding has no
//!   collation either (`call.rs` asks only then);
//! - the effects home's verdict (`AliasVerdict`, installed by gravity_hosts over gravity_decompose's effects): the
//!   built-in raises on no input (no error whose CONTEXT line would name the SQL function's statement) and the comparator
//!   is not VOLATILE (a VOLATILE one's statement advances the command counter);
//! - the current user may EXECUTE the built-in (ExecInitFunc's check at the body's start); a user change reads the alias
//!   again;
//! - no executor hook module acts on this backend's statements (pg_stat_statements and auto_explain would see each
//!   statement of a body), asked at every call;
//! - the search path is the one the body was read under (`namespace_seams::active_path_generation`): its names resolve
//!   through it, and a `SET search_path` between two statements reads the body again.
//!
//! An equality (`Use::Equal`) keeps its statement: track_functions counts the SQL `=`'s calls.
use types_core::Oid;
use types_nodes::primnodes::{Param, ParamKind};
use types_nodes::Node;

/// A comparator bound as a built-in: the permutation, and the user and search path it was read under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Alias {
    /// The built-in's argument `i` is the comparator's argument `perm[i]`.
    pub(super) perm: [u8; 2],
    user: Oid,
    path: u64,
}

impl Alias {
    /// Whether the user and the search path are the ones the alias was read under.
    #[inline]
    pub(super) fn current(&self) -> bool {
        self.user == miscinit_seams::get_user_id::call() && namespace_seams::active_path_generation::call().is_ok_and(|g| g == self.path) && !hooked()
    }

    /// Tests: an alias of `perm` that is always current (`current` is never asked of it).
    #[cfg(test)]
    pub(super) fn for_test(perm: [u8; 2]) -> Alias {
        Alias { perm, user: 0, path: 0 }
    }
}

/// The effects home's verdict on an alias: SQL comparator `cmp`'s statement calling built-in `b` over two non-NULL values
/// answers with `b`'s result alone (`b` raises on no input, `cmp` is not VOLATILE). Installed once at startup
/// (`set_alias_verdict`); with none installed no comparator is an alias.
pub type AliasVerdict = fn(cmp: Oid, b: Oid) -> bool;

/// Installs the effects home's `AliasVerdict` (gravity_exec's startup, over gravity_hosts' facts).
pub fn set_alias_verdict(v: AliasVerdict) {
    super::raw::set_verdict(v);
}

const SQL_LANGUAGE: Oid = 14;
const INTERNAL_LANGUAGE: Oid = 12;
const INT4: Oid = 23;
const ACL_EXECUTE: u64 = 1 << 7;
const ACLCHECK_OK: i32 = 0;

/// Comparator `proc` (a resolution id) as its built-in alias: the built-in's resolution id and the `Alias`; `None` when
/// a gate fails, and when reading the catalog or the body raises (the comparator's own call then raises where
/// PostgreSQL's does).
pub(super) fn read(proc: u32) -> Option<(u32, Alias)> {
    let installed = syscache_seams::lookup_pg_proc_shape::is_installed()
        && syscache_seams::lookup_pg_proc_signature::is_installed()
        && clauses_seams::inline_sql_function::is_installed()
        && namespace_seams::active_path_generation::is_installed()
        && miscinit_seams::get_user_id::is_installed()
        && aclchk_seams::object_aclcheck::is_installed();
    if !installed || hooked() || super::raw::verdict().is_none() {
        return None;
    }
    let f = crate::resolved::info(proc)?.fn_oid;
    if f == 0 || super::super::builtin(f) {
        return None;
    }
    let path = namespace_seams::active_path_generation::call().ok()?;
    let user = miscinit_seams::get_user_id::call();
    let cx = mcx::MemoryContext::new_bump("gravity_fn keysem alias");
    let body = body_call(cx.mcx(), f);
    drop(cx);
    let (b, perm) = body.ok().flatten()?;
    let s = syscache_seams::lookup_pg_proc_shape::call(b).ok().flatten()?;
    if !super::super::builtin(b) || s.prolang != INTERNAL_LANGUAGE || s.proretset || s.pronargs != 2 || s.prosecdef || !s.proconfig_isnull {
        return None;
    }
    if !super::raw::verdict().is_some_and(|v| v(f, b)) {
        return None;
    }
    if aclchk_seams::object_aclcheck::call(types_core::catalog::PROCEDURE_RELATION_ID, b, user, ACL_EXECUTE).ok()? != ACLCHECK_OK {
        return None;
    }
    let id = crate::resolved::resolve(b).ok()?.ok()?;
    Some((id, Alias { perm, user, path }))
}

/// Whether an executor hook module acts on this backend's statements now (contrib exec_hooks: pg_stat_statements
/// tracking at the running statement's nesting level, auto_explain loaded with a duration threshold): a statement of the
/// comparator's body would be seen, so it runs.
fn hooked() -> bool {
    execmain_seams::tap_executor_hooks_active::call_if_or(false, |f| f())
}

/// SQL comparator `f`'s body as the planner inlines it, read as one built-in call (`one_call`).
fn body_call(mcx: mcx::Mcx<'_>, f: Oid) -> types_error::PgResult<Option<(Oid, [u8; 2])>> {
    let Some(p) = syscache_seams::lookup_pg_proc_shape::call(f)? else { return Ok(None) };
    if p.prolang != SQL_LANGUAGE || p.prokind != b'f' as i8 || p.prosecdef || p.proretset || !p.proconfig_isnull || p.pronargs != 2 || p.prorettype != INT4 {
        return Ok(None);
    }
    let Some((ret, argtypes)) = syscache_seams::lookup_pg_proc_signature::call(mcx, f)? else { return Ok(None) };
    if argtypes.len() != 2 {
        return Ok(None);
    }
    let mut params = Vec::with_capacity(2); // alloc: Cold
    for (i, &t) in argtypes.iter().enumerate() {
        let param = Param { paramkind: ParamKind::PARAM_EXTERN, paramid: i as i32 + 1, paramtype: t, paramtypmod: -1, location: -1, ..Default::default() };
        params.push(Node::mk(mcx, param)?);
    }
    let args = types_nodes::NodeList::from_slice(mcx, &params)?;
    let Some(body) = clauses_seams::inline_sql_function::call(mcx, f, ret, 0, 0, &args)? else { return Ok(None) };
    Ok(one_call(body, ret))
}

/// `body`'s function and the permutation of its arguments, when it is one call returning `ret`, under no input
/// collation, whose two arguments are `$1` and `$2` in some order (each at most relabelled).
pub(super) fn one_call(body: Node<'_>, ret: Oid) -> Option<(Oid, [u8; 2])> {
    let (fid, rt, coll, args) = if let Some(e) = body.as_func_expr() {
        if e.funcretset || e.funcvariadic {
            return None;
        }
        (e.funcid, e.funcresulttype, e.inputcollid, &e.args)
    } else if let Some(e) = body.as_op_expr() {
        if e.opretset {
            return None;
        }
        (e.opfuncid, e.opresulttype, e.inputcollid, &e.args)
    } else {
        return None;
    };
    if fid == 0 || rt != ret || coll != 0 || args.len() != 2 {
        return None;
    }
    let mut perm = [0u8; 2];
    for (i, a) in args.iter().enumerate() {
        let mut n = a;
        while let Some(r) = n.as_relabel_type() {
            n = r.arg;
        }
        let p = n.as_param()?;
        if p.paramkind != ParamKind::PARAM_EXTERN || !(1..=2).contains(&p.paramid) {
            return None;
        }
        perm[i] = (p.paramid - 1) as u8;
    }
    (perm[0] != perm[1]).then_some((fid, perm))
}
