//! The hosted aggregate call face: the transfn / combinefn / finalfn / serialfn call (the `AggStateNode` bracket)
//! shared by class.collect, class.user, class.ordered and class.num_varlena (L2; a face file, no rows of its own; M5's
//! face/srf.rs pattern). Decision 4, declared by registry/m6.d/face_aggcall.path (cards/M6.md, M6-changes.md §1). Owner
//! M6-HOSTS.
//!
//! A class that folds an aggregate PG runs through fmgr declares the Agg's arena once (`arena`: kind `agg.arena`,
//! V4R-269, exec v4/m6.rs), keeps one state slot per group (`SHAPE`: a zeroed slot is a group not yet initialized, the
//! host applies the initcond at its first call), and calls `step` per input row and `finish` per group. The host keeps
//! PG's rules (strictness, the first input as a strict transfn's state, the by-reference copy into the aggregate
//! context); an error raises `HOST` with PG's error stashed. The arena is the leader's: a program that declares it runs
//! width 1 (parallel hosted aggregates wait on mcx new_unobserved (core)), so the combine and serial calls land with
//! the parallel row.
// Read by the M6 fold classes (M6-AGG / M6-FOLD); until they land, nothing names it.
#![allow(dead_code)]
use gravity_abi::status::raise;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntCond, Type, Value};
use gravity_kernels::ops::agg::trans::hosted::{m6_agg_final, m6_agg_final_direct, m6_agg_fold, m6_agg_trans, AGG_STATE_BYTES, AGG_WORDS, AGG_W_HEAD};
use gravity_program::protos::{Proto, ProtoKind};

use crate::face::fold::AccShape;
use crate::frame::env::Env;
use crate::frame::px::Px;

/// A hosted aggregate's accumulator: one slot in the group's record, never registers.
pub(crate) const SHAPE: AccShape = AccShape { words: 0, bytes: AGG_STATE_BYTES, align: 8 };

/// Agg `node`'s arena for pipeline `k`: `agg.arena` declared (once per Agg node, after the program's width protocols),
/// the index of the parameter word holding its handle returned.
pub(crate) fn arena(px: &mut Px<'_, '_>, k: usize, node: i32) -> u16 {
    let base = px.proto_words(AGG_WORDS);
    px.declare_proto(Proto::new(ProtoKind::AggArena, k as u16).words(base, AGG_WORDS).arg(node as u32 as u64));
    base + AGG_W_HEAD as u16
}

/// One transition of aggregate `aggno` (the Aggref's) over the state slot at `state`: `args` its transition inputs in
/// the Aggref's order, each (datum, NULL flag).
pub(crate) fn step(b: &mut FunctionBuilder<'_>, env: &mut Env, word: u16, aggno: i32, state: Value, args: &[(Value, Value)]) {
    let n = args.len().max(1) as u32;
    let (vs, ns) = (b.declare_stack_slot(8 * n, 8), b.declare_stack_slot(n, 8));
    let (vs, ns) = (b.stack_addr(vs), b.stack_addr(ns));
    for (i, &(v, null)) in args.iter().enumerate() {
        b.store(vs, 8 * i as i32, v);
        let nb = b.conv(ConvOp::Zext, Type::I8, null);
        b.store(ns, i as i32, nb);
    }
    let h = b.load(Type::I64, env.params, 8 * word as i32);
    let a = b.iconst(Type::I64, aggno as i64);
    let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
    let f = b.declare_host_once("m6_agg_trans", &[Type::Ptr, Type::I64, Type::I64, Type::Ptr, Type::Ptr, Type::Ptr], Some(Type::I64), m6_agg_trans as *const () as usize, flags);
    let st = b.call_host(f, &[env.ctx, h, a, state, vs, ns], Some(Type::I64)).expect("m6_agg_trans returns its status");
    raise_on(b, env, st);
}

/// D1371: host-sorted aggregate `aggno`'s sort and transitions over its group's run at `state`, ahead of the group's
/// finals (kernels `m6_agg_fold`); its final then reads the folded state.
pub(crate) fn fold(b: &mut FunctionBuilder<'_>, env: &mut Env, word: u16, aggno: i32, state: Value) {
    let h = b.load(Type::I64, env.params, 8 * word as i32);
    let a = b.iconst(Type::I64, aggno as i64);
    let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
    let f = b.declare_host_once("m6_agg_fold", &[Type::Ptr, Type::I64, Type::I64, Type::Ptr], Some(Type::I64), m6_agg_fold as *const () as usize, flags);
    let st = b.call_host(f, &[env.ctx, h, a, state], Some(Type::I64)).expect("m6_agg_fold returns its status");
    raise_on(b, env, st);
}

/// Aggregate `aggno`'s final value over the state slot at `state`: (datum, NULL flag); a by-reference datum lives in the
/// Call's scratch (the sink copies it).
pub(crate) fn finish(b: &mut FunctionBuilder<'_>, env: &mut Env, word: u16, aggno: i32, state: Value) -> (Value, Value) {
    finish_direct(b, env, word, aggno, state, &[])
}

/// D1520: `finish` with an ordered-set aggregate's direct arguments given, each (datum word, how it is given: kernels
/// `DIRECT_*`, an i64) in the Aggref's order (`m6_agg_final_direct`; finalize_aggregate's per-group aggdirectargs).
pub(crate) fn finish_direct(b: &mut FunctionBuilder<'_>, env: &mut Env, word: u16, aggno: i32, state: Value, direct: &[(Value, Value)]) -> (Value, Value) {
    let out = b.declare_stack_slot(16, 8);
    let out = b.stack_addr(out);
    let h = b.load(Type::I64, env.params, 8 * word as i32);
    let a = b.iconst(Type::I64, aggno as i64);
    let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
    let st = if direct.is_empty() {
        let f = b.declare_host_once("m6_agg_final", &[Type::Ptr, Type::I64, Type::I64, Type::Ptr, Type::Ptr], Some(Type::I64), m6_agg_final as *const () as usize, flags);
        b.call_host(f, &[env.ctx, h, a, state, out], Some(Type::I64)).expect("m6_agg_final returns its status")
    } else {
        let n = direct.len() as u32;
        let (vs, gs) = (b.declare_stack_slot(8 * n, 8), b.declare_stack_slot(n, 8));
        let (vs, gs) = (b.stack_addr(vs), b.stack_addr(gs));
        for (i, &(v, g)) in direct.iter().enumerate() {
            b.store(vs, 8 * i as i32, v);
            let g = b.conv(ConvOp::Trunc, Type::I8, g);
            b.store(gs, i as i32, g);
        }
        let f = b.declare_host_once("m6_agg_final_direct", &[Type::Ptr, Type::I64, Type::I64, Type::Ptr, Type::Ptr, Type::Ptr, Type::Ptr], Some(Type::I64), m6_agg_final_direct as *const () as usize, flags);
        b.call_host(f, &[env.ctx, h, a, state, out, vs, gs], Some(Type::I64)).expect("m6_agg_final_direct returns its status")
    };
    raise_on(b, env, st);
    let (v, nb) = (b.load(Type::I64, out, 0), b.load(Type::I64, out, 8));
    (v, b.icmp(IntCond::Ne, nb, env.zero))
}

/// A failed call (its error stashed) leaves through `HOST`.
fn raise_on(b: &mut FunctionBuilder<'_>, env: &mut Env, st: Value) {
    let bad = b.icmp(IntCond::Ne, st, env.zero);
    let ok = b.create_block(&[]);
    let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
    b.cond_br(bad, (env.l_raise, &[e]), (ok, &[]));
    b.switch_to(ok);
}
