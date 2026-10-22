//! D2074: a bit or word per-code `PROGRAM` table filled by the rows that read it (DK1: a key table too), not swept (percode_bind.rs `bind`
//! publishes it with every entry faulting; step/expr.rs's cold path fills the row's entry, then reads it again). A sweep
//! evaluated the body for every code of the part where PostgreSQL evaluates it only for the rows it reads. STEP-04 (C05):
//! a table every row of the part reads is swept (`every_row`, `unconditional`). Owner M1-STEP.
use std::rc::Rc;

use gravity_abi::ctx::site_off;
use gravity_abi::site::site;
use gravity_dict::lane::{ELEM_BITS, ELEM_FP128};
use gravity_decompose::graph::Source;
use gravity_decompose::Op;
use gravity_expr::emit::{ColsRef, RowRefs};
use gravity_expr::{Compiled, ExprNode, Kids, NodeId, Program};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_kernels::host_abi::{self as abi, BIND_OUT_WORDS};

use crate::admit::refusal::Reason;
use crate::dict::percode::{Class, Sweep};
use crate::dict::percode_bind::{exit_neg, mem, set_bit};
use crate::frame::cancel;
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px};
use crate::frame::row::RowVals;

/// A table the rows fill (`length(repeat(t, 6000))` over a part of 2000 distinct codes for the 5 rows a filter keeps was
/// 2000 bodies of 384 KB swept; DK1, a key table: `md5(u)` over 2M entries for the 10k rows `id % 1000 = 0` keeps); a key
/// table countwalk reads over every code is swept, and so is a table every row of the part reads (STEP-04, `every_row`):
/// the sweep pays each entry's body once in one loop, where the rows' fill pays a fault, a render and a re-evaluation per
/// entry (`upper(model) = 'IPAD'` over a 100M-row table, W0 216 -> 85 ms, W8 28 -> 11).
pub(crate) fn lazy(sw: &Sweep) -> bool {
    !sw.walked && !sw.every
}

/// STEP-04 (C05): every row of each part reaches step `j` of pipeline `k`: the pipeline scans its relation whole, only
/// projections run before the step (no filter, gate, join or limit drops a row first), no limit or consumer bound stops the pipeline early,
/// and the step is a filter, a projection or the end. An embedded inner (a nest inner, a SubPlan's plan) runs for some
/// outer rows only, so it never counts.
pub(crate) fn every_row(px: &Px<'_, '_>, k: usize, j: usize) -> bool {
    let Some(p) = px.gx.pipelines().get(k).filter(|p| matches!(p.source, Source::Range { .. })) else { return false };
    let at = match p.ops.get(j) {
        Some(o) => matches!(o, Op::Filter(_) | Op::Project(_)),
        None => j == p.ops.len(),
    };
    // a Limit, or a consumer's bound on any node the pipeline runs (EXISTS, a semi join, a cursor's fetch: frame::iface)
    let stops = p.ops.iter().any(|o| crate::frame::iface::demand(o).is_some())
        || std::iter::once(p.node_id).chain(p.node_ops.iter().map(|n| n.node_id)).any(|n| !matches!(px.request(n).bound, crate::frame::iface::Bound::All));
    at && p.ops[..j].iter().all(|o| matches!(o, Op::Project(_))) && !stops
}

/// STEP-04 (C05): per-code root `at` of `c` (compiled at step `j` of pipeline `k`) is evaluated for every row of the part
/// (`every_row`): on the path from `c`'s root through nodes that evaluate each child (a call, an operator, a cast, a test,
/// GREATEST / LEAST) or the first child of AND / OR / COALESCE, and at an end no aggregate has a FILTER or a CASE in its
/// call (its arguments, a wide sum's WHEN arms, run for some rows only). Any other root keeps the rows' fill.
pub(crate) fn unconditional(px: &Px<'_, '_>, k: usize, j: usize, c: &Compiled<'_>, at: NodeId) -> bool {
    let plain = |n: types_nodes::Node<'_>| {
        let mut case = false;
        gravity_decompose::expr::walk(n, &mut |x| {
            case |= x.as_case_expr().is_some();
            !case
        });
        !case
    };
    let aggs = || j < px.gx.pipelines()[k].ops.len() || px.gx.group_at(k).is_none_or(|g| g.aggs.iter().all(|a| a.filter.is_none() && plain(a.node)));
    every_row(px, k, j) && aggs() && reaches(&c.prog, c.root, at)
}

/// Node `at` is `id` or under it through children evaluated on every evaluation of `id` (`unconditional`).
fn reaches(p: &Program<'_>, id: NodeId, at: NodeId) -> bool {
    crate::stack_check();
    if id == at {
        return true;
    }
    if p.islands.iter().any(|i| i.root == id) {
        return false;
    }
    let range = |k: Kids| &p.kids[k.start as usize..(k.start + k.len) as usize];
    let kids: &[NodeId] = match &p.nodes[id.0 as usize] {
        ExprNode::Func(n) | ExprNode::Op(n) | ExprNode::Distinct(n) | ExprNode::NullIf(n) => range(p.sites[n.site.0 as usize].args),
        ExprNode::ScalarArrayOp(n) => range(p.sites[n.site.0 as usize].args),
        ExprNode::MinMax(n) => range(n.args),
        ExprNode::Bool(n) => &range(n.args)[..n.args.len.min(1) as usize],
        ExprNode::Coalesce(n) => &range(n.args)[..n.args.len.min(1) as usize],
        ExprNode::NullTest(n) => std::slice::from_ref(&n.arg),
        ExprNode::BooleanTest(n) => std::slice::from_ref(&n.arg),
        ExprNode::Relabel(n) => std::slice::from_ref(&n.arg),
        ExprNode::CoerceViaIO(n) => std::slice::from_ref(&n.arg),
        ExprNode::Collate(n) => std::slice::from_ref(&n.arg),
        _ => &[],
    };
    kids.iter().any(|&x| reaches(p, x, at))
}

/// D2074: lazy table `sw` (site lane `lane`, bind slot `sw.t` of `out`) published for the part with every entry faulting
/// (`agg_table_done` keeps the bitmap): a row reading an entry takes the cold re-evaluation, which fills it (`fill`).
pub(crate) fn faulting(b: &mut FunctionBuilder<'_>, env: &mut Env, site_v: Value, out: Value, lane: u8, sw: &Sweep, l_exit: Block) {
    let h_n = b.declare_host_once("agg_table_ncodes", &[Type::Ptr, Type::I64, Type::I64], Some(Type::I64), abi::agg_table_ncodes as *const () as usize, mem());
    let h_done = b.declare_host_once("agg_table_done", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Some(Type::I64), abi::agg_table_done as *const () as usize, mem());
    let lane_v = b.iconst(Type::I64, lane as i64);
    let n = b.call_host(h_n, &[env.ctx, site_v, lane_v], Some(Type::I64)).unwrap_or(env.zero);
    exit_neg(b, env, n, l_exit);
    if sw.elem == ELEM_FP128 {
        // a key table has no bitmap: every entry `[0, 0]` (the source zeroed it) sends its row to the re-evaluation, which
        // stores the entry (percode_key.rs `cell`)
        let st = b.call_host(h_done, &[env.ctx, site_v, lane_v, env.zero], Some(Type::I64)).unwrap_or(env.zero);
        exit_neg(b, env, st, l_exit);
        return;
    }
    let faults = b.load(Type::Ptr, out, (BIND_OUT_WORDS * sw.t * 8) as i32 + 8);
    let s63 = b.iconst(Type::I64, 63);
    let n63 = b.int_bin(IntBinOp::Add, n, s63);
    let six = b.iconst(Type::I64, 6);
    let nw = b.int_bin(IntBinOp::Ushr, n63, six);
    let ones = b.iconst(Type::I64, -1);
    let (l_loop, l_body) = cancel::header(b, env.ctx, env.x.l_cancel, &[Type::I64], env.mcx);
    let l_done = b.create_block(&[]);
    b.br(l_loop, &[env.zero]);
    b.switch_to(l_body);
    let w = b.block_params(l_body)[0];
    let more = b.icmp(IntCond::Slt, w, nw);
    let l_set = b.create_block(&[]);
    b.cond_br(more, (l_set, &[]), (l_done, &[]));
    b.switch_to(l_set);
    let at = b.ptr_add(faults, w, 8, 0);
    b.store(at, 0, ones);
    let w1 = b.int_bin(IntBinOp::Add, w, env.one);
    b.br(l_loop, &[w1]);
    b.switch_to(l_done);
    let st = b.call_host(h_done, &[env.ctx, site_v, lane_v, n], Some(Type::I64)).unwrap_or(env.zero);
    exit_neg(b, env, st, l_exit);
}

/// D2074: expression `id`'s lazy table entries the row reads, filled where they still fault (`faulting`): the row's code
/// rendered and the table's root evaluated over the image, its bit or word stored and its fault bit cleared (the table's
/// last fault cleared drops its bitmap: `agg_table_filled`); a code with
/// no image, a raise or a NULL leaves the fault (the row's whole re-evaluation answers it, as a swept fault). The region
/// is given back after each entry. `false`: the expression reads no lazy table (nothing emitted).
pub(crate) fn fill(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals, id: ExId) -> bool {
    let (Some(pl), Some(out)) = (crate::step::expr::plan_of(env, id), env.tables) else { return false };
    let tabs = Rc::clone(&pl.tabs);
    let tb = tabs.borrow();
    let Some(site_k) = tb.site else { return false };
    let mut fills = pl.tables.iter().filter(|x| matches!(x.1, Class::Bits | Class::Words)).filter_map(|x| tb.sweeps.iter().find(|s| s.t == x.0 as usize && lazy(s))).peekable();
    if fills.peek().is_none() {
        return false;
    }
    fires::fire("percode.lazy");
    let (codes, _) = crate::dict::percode_len::refs(b, env, node, row, pl);
    let site_v = b.iconst(Type::I64, site_k as i64);
    let pid = b.load_zext(Type::I32, env.ctx, site_off(site_k) as i32 + site::PART_ID as i32);
    let s32 = b.iconst(Type::I64, 32);
    let part = b.int_bin(IntBinOp::Shl, pid, s32);
    for sw in fills {
        let Some(cr) = codes.iter().find(|c| c.attno == sw.attno) else { continue };
        let code = gravity_expr::emit::percode::code_at(b, &cr.lane, env.zero);
        let at = (BIND_OUT_WORDS * sw.t * 8) as i32;
        let (data, faults) = (b.load(Type::Ptr, out, at), b.load(Type::Ptr, out, at + 8));
        let fb = gravity_expr::emit::percode::bit_of(b, faults, code);
        let faulted = b.icmp(IntCond::Ne, fb, env.zero);
        let go = match cr.lane.valid_null { Some(ok) => b.int_bin(IntBinOp::And, ok, faulted), None => faulted };
        let (l_do, l_eval, l_back, l_next) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(go, (l_do, &[]), (l_next, &[]));
        b.switch_to(l_do);
        let mark = env.hir.mark(b);
        let over_v = b.iconst(Type::I64, sw.over as i64);
        let word = b.int_bin(IntBinOp::Or, part, code);
        let p = b.call_host(env.core.render, &[env.ctx, site_v, over_v, word], Some(Type::Ptr)).unwrap_or(env.zero);
        let pi = b.conv(ConvOp::PtrToInt, Type::I64, p);
        exit_neg(b, env, pi, env.x.l_exit);
        let none = b.icmp(IntCond::Eq, pi, env.zero);
        b.cond_br(none, (l_back, &[]), (l_eval, &[]));
        b.switch_to(l_eval);
        let (cv, cn) = crate::frame::hoist::words(b, env, sw.node, sw.ex);
        let t = b.bconst(true);
        let width = sw.attno.max(1) as usize;
        let (values, valids) = (mcx::pg_vec![in env.mcx; pi; width], mcx::pg_vec![in env.mcx; t; width]);
        let e = env.placed(sw.ex);
        let refs = env.hir.with_ex(b, e, RowRefs::scan(ColsRef::Values { values: &values[..], valids: &valids[..] }, &cv, &cn, env.mcx));
        let Ok(l) = e.c.emit_at(b, &refs, sw.root) else { crate::refuse!(sw.node, Reason::Bug { payload: "a per-code body with no emit over its image" }) };
        let raised = b.icmp(IntCond::Ne, l.err, env.zero);
        let null = b.int_bin(IntBinOp::Xor, l.valid, t);
        let bad = b.int_bin(IntBinOp::Or, raised, null);
        let l_ok = b.create_block(&[]);
        b.cond_br(bad, (l_back, &[]), (l_ok, &[]));
        b.switch_to(l_ok);
        if sw.elem == ELEM_BITS {
            let nz = b.icmp(IntCond::Ne, l.value, env.zero);
            let (l_set, l_clear) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(nz, (l_set, &[]), (l_clear, &[]));
            b.switch_to(l_set);
            set_bit(b, data, code);
            b.br(l_clear, &[]);
            b.switch_to(l_clear);
        } else {
            if b.value_type(l.value) != Type::I64 {
                crate::refuse!(sw.node, Reason::Shape { why: crate::admit::refusal::ShapeWhy("a per-code word body whose value is not a word") });
            }
            let at = b.ptr_add(data, code, 8, 0);
            b.store(at, 0, l.value);
        }
        clear_bit(b, faults, code);
        let h_filled = b.declare_host_once("agg_table_filled", &[Type::Ptr, Type::I64, Type::I64], Some(Type::I64), gravity_kernels::contract::entry2::<abi::TableFilled> as *const () as usize, mem());
        let lane_v = b.iconst(Type::I64, tb.lanes[sw.t].0 as i64);
        let st = b.call_host(h_filled, &[env.ctx, site_v, lane_v], Some(Type::I64)).unwrap_or(env.zero);
        exit_neg(b, env, st, env.x.l_exit);
        b.br(l_back, &[]);
        b.switch_to(l_back);
        if let Some(m) = mark {
            env.hir.give_back(b, m);
        }
        b.br(l_next, &[]);
        b.switch_to(l_next);
    }
    true
}

/// Bit `code` of the word array at `words` cleared.
fn clear_bit(b: &mut FunctionBuilder<'_>, words: Value, code: Value) {
    let (six, m63) = (b.iconst(Type::I64, 6), b.iconst(Type::I64, 63));
    let wi = b.int_bin(IntBinOp::Ushr, code, six);
    let wp = b.ptr_add(words, wi, 8, 0);
    let w = b.load(Type::I64, wp, 0);
    let sh = b.int_bin(IntBinOp::And, code, m63);
    let one = b.iconst(Type::I64, 1);
    let bit = b.int_bin(IntBinOp::Shl, one, sh);
    let ones = b.iconst(Type::I64, -1);
    let keep = b.int_bin(IntBinOp::Xor, bit, ones);
    let w2 = b.int_bin(IntBinOp::And, w, keep);
    b.store(wp, 0, w2);
}
