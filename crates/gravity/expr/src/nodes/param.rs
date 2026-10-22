//! `Param`: PARAM_EXTERN through the bound list or the callback hook (the hook's argument is
//! armed per evaluation), PARAM_EXEC through the executor's cells. The binding is resolved at
//! evaluation with the incumbent's errors (`42704` missing, `42804` type mismatch).
//! D2834: an extern parameter the execution leaves to evaluation time (`gravity_program::externs::deferred`: a PL list's
//! slot with no value, or one whose type is no longer the plan's) is `DEFERRED`: the emitted code fetches it where it is
//! evaluated (emit/host.rs `param_deferred`), as ExecEvalParamExtern does, never ahead of it.

use gravity_fn::NullableDatum;
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_nodes::primnodes::ParamKind;

use crate::eval::Cx;
use crate::nodes::{unsupported, Builder, ExprNode, Kind, NodeId};
use crate::CompileResult;

#[derive(Clone, Copy, Debug)]
pub struct Param {
    pub kind: ParamKind,
    pub id: i32,
    pub typ: Oid,
}

/// D2834: the kind of a deferred extern parameter's node (`compile`), one no plan carries (a SubLink's parameter never
/// reaches the executor): every reader that takes a PARAM_EXTERN word passes it by.
pub const DEFERRED: ParamKind = ParamKind::PARAM_SUBLINK;

pub fn compile(b: &mut Builder<'_, '_>, p: &types_nodes::primnodes::Param) -> CompileResult<NodeId> {
    let kind = match p.paramkind {
        ParamKind::PARAM_EXTERN if gravity_program::externs::deferred(p.paramid) => DEFERRED,
        ParamKind::PARAM_EXTERN | ParamKind::PARAM_EXEC => p.paramkind,
        _ => return unsupported(Kind::Param),
    };
    Ok(b.push(ExprNode::Param(Param { kind, id: p.paramid, typ: p.paramtype })))
}

/// D2834: whether `n` is a deferred extern parameter (`DEFERRED`).
pub fn is_deferred(n: &Param) -> bool {
    n.kind == DEFERRED
}

pub fn eval(n: &Param, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    match n.kind {
        ParamKind::PARAM_EXEC => {
            let base = cx.params.exec_vals.filter(|_| n.id >= 0 && (n.id as u32) < cx.params.n_exec).ok_or_else(|| no_value(n.id))?;
            // SAFETY: `id` is within the once-sized array the binding names.
            let prm = unsafe { &*base.as_ptr().add(n.id as usize) };
            Ok(NullableDatum { value: prm.value, isnull: prm.isnull })
        }
        _ => {
            if let Some(func) = cx.params.param_callback {
                // SAFETY: the hook argument is the one the owner armed for this evaluation.
                return unsafe { func(cx.callback_arg, n.id, n.typ) };
            }
            let list = cx.params.extern_params.unwrap_or(&[]);
            if n.id <= 0 || n.id as usize > list.len() {
                return Err(no_value(n.id));
            }
            let prm = &list[(n.id - 1) as usize];
            if prm.ptype == 0 {
                return Err(no_value(n.id));
            }
            if prm.ptype != n.typ {
                return Err(type_mismatch(n.id, prm.ptype, n.typ));
            }
            Ok(NullableDatum { value: prm.value, isnull: prm.isnull })
        }
    }
}

/// D2834: deferred extern parameter `id` of type `typ` (the plan's) where the expression evaluates it, from the execution's
/// list `list` as C's ExecEvalParamExtern takes it: a slot the list was built without is the owner's fetch's now (PL/pgSQL's
/// non-speculative fetch, which raises a record's "not assigned yet" / "has no field" here), and a slot whose type is not
/// the plan's raises the type mismatch. A list no longer registered is an internal error, never a read.
pub fn fetch_deferred(list: types_portal::ParamListHandle, id: i32, typ: Oid) -> PgResult<NullableDatum> {
    let Some(slot) = types_portal::params::slot(list, id) else {
        return Err(Box::new(PgError::error(format!("gravity: deferred parameter {id} read past its parameter list")).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR))); // alloc: Cold
    };
    let mut prm = slot;
    if prm.ptype == 0 {
        if let Some(h) = types_portal::params::fetch_hook(list) {
            // SAFETY: the list's owner keeps the hook's argument live while its list is registered (`slot` checked it is).
            prm = unsafe { (h.func)(h.arg, id) }?;
        }
    }
    if prm.ptype == 0 {
        return Err(no_value(id));
    }
    if prm.ptype != typ {
        return Err(type_mismatch(id, prm.ptype, typ));
    }
    Ok(NullableDatum { value: prm.value, isnull: prm.isnull })
}

#[cold]
#[inline(never)]
fn no_value(id: i32) -> Box<PgError> {
    Box::new(PgError::error(format!("no value found for parameter {id}")).with_sqlstate(types_error::ERRCODE_UNDEFINED_OBJECT)) // alloc: Cold
}

#[cold]
#[inline(never)]
fn type_mismatch(id: i32, ptype: Oid, paramtype: Oid) -> Box<PgError> {
    let f = |o: Oid| format_type::format_type_be(o).unwrap_or_else(|_| format!("{o}")); // alloc: Cold
    Box::new( // alloc: Cold
        PgError::error(format!("type of parameter {id} ({}) does not match that when preparing the plan ({})", f(ptype), f(paramtype))) // alloc: Cold
            .with_sqlstate(types_error::ERRCODE_DATATYPE_MISMATCH),
    )
}
