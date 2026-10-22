//! nest.subplan's per-lane result objects (M8-plan §3.6): the SubResult protocol that holds a nested SubPlan's value beyond
//! the inner's window. An ARRAY link accumulates its rows in the calling lane's accumulator (RT-2b `m8_array_*`, PG's
//! accumArrayResultAny: each element copied as it comes, an array element building the N+1-D result); a by-reference
//! EXPR value is copied per call into the lane's slot (RT-2a `m8_sub_copy`, PG's curTuple), valid until that lane's next
//! call. A helper of nest/subplan.rs (owner M8-SUB): it registers nothing. Refused by name: an ARRAY of cstrings; a
//! cstring EXPR result.
use gravity_abi::status::raise;
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_expr::byref;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntCond, Type, Value};
use gravity_kernels::ops::shared::object::entries as m8;
use gravity_program::protos::{Proto, ProtoKind};
use mcx::GrowIn;

use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::admit::refusal::{Reason, ShapeWhy};

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

/// ARRAY (RT-2b, V4R-428): the element type `elem`'s accumulator, a SubResult object of pipeline `k` whose arg is the type
/// (the runtime's `elem_shape`). Its handle word.
pub(crate) fn array_acc(px: &mut Px<'_, '_>, k: usize, node: i32, elem: u32) -> u16 {
    if elem == 0 {
        bug(node, "an ARRAY SubPlan without its element type");
    }
    let Some(tf) = gravity_decompose::catalog::typefacts::facts_of(px.gx.facts, elem) else { bug(node, "an ARRAY SubPlan's element type the catalog refuses") };
    if tf.typlen != -1 && tf.typlen <= 0 {
        shape(node, "an ARRAY SubPlan of cstrings");
    }
    let h = px.proto_words(1);
    px.declare_proto(Proto::new(ProtoKind::SubResult, k as u16).words(h, 1).arg(elem as u64));
    h
}

/// An EXPR or MULTIEXPR link's by-reference result column (`tf`): its SubResult slot's handle word and the type's typlen;
/// `None` for a by-value one (ANY / ALL / ROWCOMPARE read their columns inside the walk and take none).
pub(crate) fn copy_slot(px: &mut Px<'_, '_>, k: usize, node: i32, tf: &TypeFacts) -> Option<(u16, i16)> {
    if tf.byval {
        return None;
    }
    if tf.typlen != -1 && tf.typlen <= 0 {
        shape(node, "a SubPlan's cstring result");
    }
    let h = px.proto_words(1);
    px.declare_proto(Proto::new(ProtoKind::SubResult, k as u16).words(h, 1));
    Some((h, tf.typlen))
}

#[derive(Clone, Copy)]
pub(crate) enum AccOp {
    Begin,
    Push,
    End,
}

/// `m8_array_begin` / `_push(value, null)` / `_end` on the lane's accumulator `h` (of the step at `node`). The call's result
/// (`_end`: the array datum, valid until this lane's next begin).
pub(crate) fn acc_call(b: &mut FunctionBuilder<'_>, env: &Env, (h, node): (Option<u16>, i32), op: AccOp, extra: &[Value]) -> Value {
    let Some(h) = h else { bug(node, "an ARRAY entry without its accumulator") };
    let (name, addr) = match op {
        AccOp::Begin => ("m8_array_begin", gravity_abi::host_words!(m8::m8_array_begin(ctx, h))),
        AccOp::Push => ("m8_array_push", gravity_abi::host_words!(m8::m8_array_push(ctx, h, value, null))),
        AccOp::End => ("m8_array_end", gravity_abi::host_words!(m8::m8_array_end(ctx, h))),
    };
    let mut tys = mcx::pg_vec![in env.mcx; Type::Ptr, Type::I64];
    tys.extend_in(extra.iter().map(|_| Type::I64));
    let flags = HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() };
    let f = b.declare_host_once(name, &tys[..], Some(Type::I64), addr, flags);
    let w = b.load(Type::I64, env.params, h as i32 * 8);
    let mut args = mcx::pg_vec![in env.mcx; env.ctx, w];
    args.extend_from_slice_in(extra);
    let r = b.call_host(f, &args[..], Some(Type::I64)).unwrap_or(env.zero);
    raise_below_zero(b, env, r);
    r
}

/// A by-reference EXPR value `d` copied into the lane's slot (`copy`: its handle word and typlen); 0 for a NULL.
pub(crate) fn keep(b: &mut FunctionBuilder<'_>, env: &Env, copy: (u16, i16), d: Value, null: Value) -> Value {
    let (h, typlen) = copy;
    let (l_copy, l_join) = (b.create_block(&[]), b.create_block(&[Type::I64]));
    b.cond_br(null, (l_join, &[env.zero]), (l_copy, &[]));
    b.switch_to(l_copy);
    let p = b.conv(ConvOp::IntToPtr, Type::Ptr, d);
    let len = byref::image_len(b, p, typlen);
    let w = b.load(Type::I64, env.params, h as i32 * 8);
    let o = b.conv(ConvOp::IntToPtr, Type::Ptr, w);
    let flags = HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() };
    let f = b.declare_host_once("m8_sub_copy", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::I64], Some(Type::I64), m8::m8_sub_copy as *const () as usize, flags);
    let r = b.call_host(f, &[env.ctx, o, p, len], Some(Type::I64)).unwrap_or(env.zero);
    raise_below_zero(b, env, r);
    b.br(l_join, &[r]);
    b.switch_to(l_join);
    b.block_params(l_join)[0]
}

/// A host status below zero leaves by the raise exit with the host's stashed error.
fn raise_below_zero(b: &mut FunctionBuilder<'_>, env: &Env, r: Value) {
    let bad = b.icmp(IntCond::Slt, r, env.zero);
    let (l_bad, l_ok) = (b.create_block(&[]), b.create_block(&[]));
    b.mark_cold(l_bad);
    b.cond_br(bad, (l_bad, &[]), (l_ok, &[]));
    b.switch_to(l_bad);
    let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
    b.br(env.l_raise, &[e]);
    b.switch_to(l_ok);
}

/// D1478: the SubPlan's own scans carry no PlanRowMark (the top-level query's relations alone do), so PG's EvalPlanQual runs
/// them again under the statement's snapshot: the inner's site, and the pipelines its slots come from (a rerun set's, one
/// kept across calls), each with its own producers (exec epq.rs), are named to the recheck, which leaves them unsubstituted.
/// D3278 (SUBPLAN): a pulled Material's fill pipeline drives no site; its child scans at its own walk site inside the
/// reader (material/lower.rs `make_reader`, the fill's `pipeline_site`), named too: the recheck's fresh spool (exec epq.rs
/// `fresh_spools`) then pulls the child from its start, as EvalPlanQual's fresh Material runs its child again.
pub(crate) fn rescan(px: &mut Px<'_, '_>, k: usize, ik: usize, site: Option<usize>) {
    if let Some(s) = site {
        px.declare_proto(Proto::new(ProtoKind::Rescan, k as u16).site(s as i16));
    }
    let ps = px.gx.pipelines();
    for &s in ps[ik].desc.consumes.iter() {
        if let Some(q) = (0..k).rev().find(|&q| ps[q].desc.produces() == Some(s)) {
            px.declare_proto(Proto::new(ProtoKind::Rescan, q as u16));
            if let (Ok((_, true)), Some(w)) = (crate::lifetime::material::spool_fill(px.gx, q), px.pipeline_site(q)) {
                px.declare_proto(Proto::new(ProtoKind::Rescan, q as u16).site(w as i16));
            }
        }
    }
}
