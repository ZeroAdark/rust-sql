//! D1292 / D2849: PostgreSQL's 42P22 at a hash under no collation ("could not determine which collation to use for string
//! hashing"), as the kinds that hash call it: `m6_raise` stashes the error, `m6_raise_elems` stashes it only when an array
//! holds a non-NULL element (hash_array calls the element's hash per non-NULL element). Each returns the host status,
//! below zero once the error is stashed; the caller leaves through its raise block.
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_decompose::join::KeyRaise;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntCond, Type, Value};
use gravity_kernels::host_abi::v4::m6_raise as h6;

use crate::frame::env::Env;
use crate::frame::row::RowVals;
use crate::rep::CellRef;

/// `m6_raise` of PG's 42P22 at a hash: the status, always below zero.
pub(crate) fn at_hash(b: &mut FunctionBuilder<'_>, env: &Env) -> Value {
    let h = b.declare_host_once("m6_raise", &[Type::Ptr, Type::I64], Some(Type::I64), gravity_kernels::contract::entry1::<h6::M6Raise> as *const () as usize, HostFlags { may_error: true, ..Default::default() });
    let code = b.iconst(Type::I64, h6::RAISE_INDETERMINATE_HASH as i64);
    b.call_host(h, &[env.ctx, code], Some(Type::I64)).expect("m6_raise returns its status")
}

/// `m6_raise_elems` over the array datum `d` (not NULL): below zero (PG's 42P22, stashed) when it holds a non-NULL
/// element, else 0.
pub(crate) fn elems(b: &mut FunctionBuilder<'_>, env: &Env, d: Value) -> Value {
    let p = if b.value_type(d) == Type::Ptr { d } else { b.conv(ConvOp::IntToPtr, Type::Ptr, d) };
    let h = b.declare_host_once("m6_raise_elems", &[Type::Ptr, Type::Ptr], Some(Type::I64), gravity_kernels::contract::entry1::<h6::M6RaiseElems> as *const () as usize, HostFlags { may_error: true, ..Default::default() });
    b.call_host(h, &[env.ctx, p], Some(Type::I64)).expect("m6_raise_elems returns its status")
}

/// D2849 (decompose join.rs `KeyRaise`): a hash join key `c` of type facts `tf` where PG hashes it (its Hash node's insert,
/// the join's outer row): unless it is NULL, a `Value` key's hash raises 42P22 at once, an `Elems` key's when the array
/// holds a non-NULL element. The raise leaves through the row's raise block, the stashed error its payload.
pub(crate) fn at_key(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals, c: CellRef<'_>, tf: &TypeFacts, raise: KeyRaise) {
    let (l_hash, l_on) = (b.create_block(&[]), b.create_block(&[]));
    match c.null {
        Some(null) => b.cond_br(null, (l_on, &[]), (l_hash, &[])),
        None => b.br(l_hash, &[]),
    }
    b.switch_to(l_hash);
    let st = match raise {
        KeyRaise::Elems => {
            let mut cx = env.lx(b, node);
            let (d, _) = crate::rep::datum(&mut cx, CellRef { null: None, ..c }, tf);
            env.memo = cx.memo;
            elems(b, env, d)
        }
        _ => at_hash(b, env),
    };
    let failed = b.icmp(IntCond::Slt, st, env.zero);
    let host = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    let err = b.select(failed, host, env.zero);
    crate::step::expr::raise_if(b, env, row, err);
    b.br(l_on, &[]);
    b.switch_to(l_on);
}
