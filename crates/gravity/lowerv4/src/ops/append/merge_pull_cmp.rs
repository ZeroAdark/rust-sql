//! The pull-form merge's text keys (SETOP R2, V4R-458, D449; a helper of runs/merge_pull.rs): a text / varchar merge key's
//! pair (runs/keylane.rs `pairs`) is an inexact prefix word, so two held rows whose pairs tie are ordered by the key's
//! comparator over their held copies (runs/merge_pull_copy.rs: PostgreSQL's detoasted payload behind a 4-byte header),
//! `varstr_cmp` under the key's collation with PostgreSQL's memcmp tie-break (kernels `text_cmp_coll`), negated for a
//! DESC key. A comparator-ordered key (runs/keylane.rs `hosted`: a numeric, an inet, an array; `Key::proc` its btree
//! support function's resolution id) is ordered the same way by that comparator under its collation (raw.rs
//! `PullKeyHosted`: `keysem::call::order`). The pull form then reads PostgreSQL's rows exactly (a row per arm ahead,
//! the emitting arm's next), so an arm row that can raise (union#163: `a || b`) raises where PostgreSQL's does and D449's
//! rule does not apply. A bpchar key stays by name out of the form (props `merge_pull`). Registers nothing. Owner
//! M9-SETOP (files-extra, V4R-458).
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, HostFlags, IntBinOp, IntCond, Type, Value};

use crate::frame::env::Env;

use crate::rep::keylane::Key;

/// Whether the held row `(pairs, vals, nulls)` orders strictly before the best row so far `(bp, bv)`, key by key: each
/// key's pair words (unsigned, lexicographic), then, for a key with a tail whose pairs tie over two non-NULL values, the
/// comparator over the two held copies (called only then, never over the empty best `none`). A compare that raised leaves
/// through `env.l_raise` (HOST: its error stashed).
#[allow(clippy::too_many_arguments)]
pub(crate) fn less(b: &mut FunctionBuilder<'_>, env: &mut Env, keys: &[Key], pairs: &[Value], bp: &[Value], vals: &[Value], nulls: &[Value], bv: &[Value], none: Value) -> Value {
    let h = b.declare_host_once("pull_key_cmp", &[Type::I64, Type::I64, Type::I64], Some(Type::I64), crate::ops::append::raw::pull_key_cmp as *const () as usize, HostFlags { may_error: true, ..HostFlags::default() });
    let hh = b.declare_host_once("pull_key_hosted", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Some(Type::I64), gravity_kernels::contract::entry3::<crate::ops::append::raw::PullKeyHosted> as *const () as usize, HostFlags { may_error: true, ..HostFlags::default() });
    let (mut less, mut eq) = (b.bconst(false), b.bconst(true));
    for (i, key) in keys.iter().enumerate() {
        let mut lt_i = b.bconst(false);
        let mut eq_i = b.bconst(true);
        for (w, p) in pairs[2 * i..2 * i + 2].iter().zip(bp[2 * i..2 * i + 2].iter()) {
            let lt = b.icmp(IntCond::Ult, *w, *p);
            let e = b.icmp(IntCond::Eq, *w, *p);
            let both = b.int_bin(IntBinOp::And, eq_i, lt);
            lt_i = b.int_bin(IntBinOp::Or, lt_i, both);
            eq_i = b.int_bin(IntBinOp::And, eq_i, e);
        }
        if key.tail != 0 {
            let c = key.col as usize;
            let some = b.icmp(IntCond::Eq, nulls[c], env.zero);
            let t = b.bconst(true);
            let some_best = b.int_bin(IntBinOp::Xor, none, t);
            let tie = b.int_bin(IntBinOp::And, eq, eq_i);
            let tie = b.int_bin(IntBinOp::And, tie, some);
            let tie = b.int_bin(IntBinOp::And, tie, some_best);
            let cmp = if key.proc != 0 {
                let pc = b.const_raw(Type::I64, key.proc as u64 | (key.coll.oid as u64) << 32);
                call_on(b, env, hh, tie, &[env.ctx, vals[c], bv[c], pc], env.l_raise)
            } else {
                let coll = b.iconst(Type::I64, key.coll.oid as i64);
                call_on(b, env, h, tie, &[vals[c], bv[c], coll], env.l_raise)
            };
            let cmp = if key.desc { b.int_bin(IntBinOp::Sub, env.zero, cmp) } else { cmp };
            let lt_t = b.icmp(IntCond::Slt, cmp, env.zero);
            let eq_t = b.icmp(IntCond::Eq, cmp, env.zero);
            let tail_lt = b.int_bin(IntBinOp::And, eq_i, lt_t);
            lt_i = b.int_bin(IntBinOp::Or, lt_i, tail_lt);
            eq_i = b.int_bin(IntBinOp::And, eq_i, eq_t);
        }
        let now = b.int_bin(IntBinOp::And, eq, lt_i);
        less = b.int_bin(IntBinOp::Or, less, now);
        eq = b.int_bin(IntBinOp::And, eq, eq_i);
    }
    less
}

/// The comparator's −1 / 0 / 1 of the two copies `args` carry (with its collation) when `when` holds, else 0; its `2`
/// (raised) leaves through `l_raise`.
fn call_on(b: &mut FunctionBuilder<'_>, env: &mut Env, h: gravity_ir::HostFn, when: Value, args: &[Value], l_raise: Block) -> Value {
    let (l_call, l_join) = (b.create_block(&[]), b.create_block(&[Type::I64]));
    b.cond_br(when, (l_call, &[]), (l_join, &[env.zero]));
    b.switch_to(l_call);
    let r = b.call_host(h, args, Some(Type::I64)).expect("returns the order");
    let two = b.iconst(Type::I64, 2);
    let (raised, l_ok) = (b.icmp(IntCond::Eq, r, two), b.create_block(&[]));
    let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    b.cond_br(raised, (l_raise, &[e]), (l_ok, &[]));
    b.switch_to(l_ok);
    b.br(l_join, &[r]);
    b.switch_to(l_join);
    b.block_params(l_join)[0]
}
