//! The per-window bind of a site's per-code tables and the once-per-part `PROGRAM` sweep (road `percode.table`; lowerv2
//! text/codes.rs `bind_swept` / `sweep_table`). The source calls `bind` at each window once the window's lanes are
//! loaded: `agg_bind` fills the site's table lanes for the window's part (LIKE verdicts and lengths resident from the
//! hosts, fingerprints), and a `PROGRAM` table the part has not filled yet is swept here over its dictionary's codes,
//! then bound again. An entry whose body raised or came out NULL is a fault: the table never answers it, the row
//! re-evaluates the expression over the rendered text (step/expr.rs). step/percode.rs keeps the analysis and the mask.
//! Budget 160 lines. Owner M1-STEP.
use std::cell::RefCell;
use std::rc::Rc;

use gravity_abi::ctx::site_off;
use gravity_abi::site::site;
use gravity_dict::lane::{ELEM_BITS, ELEM_FP128};
use gravity_expr::emit::{ColsRef, RowRefs};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_kernels::host_abi::{self as abi, BIND_BITS, BIND_FP, BIND_LEN, BIND_OUT_WORDS, BIND_WORDS};
use mcx::{CollectIn, PgVec};

use crate::dict::percode::{Class, Sweep, Tables};
use crate::frame::cancel;
use crate::frame::env::Env;
use crate::admit::refusal::Reason;

pub(super) fn mem() -> HostFlags {
    HostFlags { touches_memory: true, ..HostFlags::default() }
}

fn class_word(c: Class) -> u64 {
    (match c {
        Class::Bits | Class::Set => BIND_BITS,
        Class::Len => BIND_LEN,
        Class::Fp => BIND_FP,
        Class::Words => BIND_WORDS,
    }) as u64
}

/// A negative status `st` leaves through `l_exit` with it; the builder continues in a new block.
pub(super) fn exit_neg(b: &mut FunctionBuilder<'_>, env: &Env, st: Value, l_exit: Block) {
    let neg = b.icmp(IntCond::Slt, st, env.zero);
    let l_ok = b.create_block(&[]);
    b.cond_br(neg, (l_exit, &[st]), (l_ok, &[]));
    b.switch_to(l_ok);
}

/// X5: each parameter-valued table's values into the site's key cells, before `agg_bind` fills its lanes (the source
/// reads them there, hosts granule_source `bound_spec`): a statement parameter's bind word and its null byte, at every
/// window, so a value is read at the scan's first window and never by a scan that has none. A parameter with no bind word
/// is a NULL: every entry faults and the row evaluates the body.
fn bound(b: &mut FunctionBuilder<'_>, env: &Env, site_k: usize, bound: &[crate::dict::percode::Bound]) {
    if bound.is_empty() {
        return;
    }
    let s = site_off(site_k) as i32;
    let keys = b.load(Type::Ptr, env.ctx, s + site::KEYS as i32);
    let keys_null = b.load(Type::Ptr, env.ctx, s + site::KEYS_NULL as i32);
    for bd in bound {
        let e = env.placed(bd.ex);
        let params = bd.params.iter().map(|&(id, _)| {
            e.needs.iter().zip(e.places.iter()).find_map(|(need, place)| match (need, place) {
                (gravity_expr::emit::ParamNeed::Extern { id: x, .. }, gravity_lower::Place::Word(w)) if *x == id => Some(*w),
                _ => None,
            })
        });
        // NEW-72 entry C: a bound text constant's word, where the expression's word plan placed it
        let consts = bd.consts.iter().map(|&i| match e.places.get(i as usize) {
            Some(gravity_lower::Place::Word(w)) => Some(*w),
            _ => None,
        });
        for (&cell, word) in bd.cells.iter().zip(params.chain(consts)) {
            let (v, isnull) = match word {
                Some(w) => (b.load(Type::I64, env.params, w as i32 * 8), b.load(Type::I8, env.params_null, w as i32)),
                None => (env.zero, b.iconst(Type::I8, 1)),
            };
            b.store(keys, cell as i32 * 8, v);
            b.store(keys_null, cell as i32, isnull);
        }
    }
}

/// NEW-72 entry C: the bound tables' key cells (`bound`) at the scan's open too, before its seek: the source's sparse code
/// select (hosts granule_source/fetch.rs `sparse_table`) resolves a bound table at the first window's fetch, before that
/// window's bind writes them, and must read this execution's values.
pub(crate) fn bound_keys(b: &mut FunctionBuilder<'_>, env: &Env, site_k: usize, t: &Rc<RefCell<Tables>>) {
    bound(b, env, site_k, &t.borrow().bound);
}

/// RD-key.codeif: a constant-arm key's image rows of tables `t` with no site table (no agg_bind) into `Env::tables`, at
/// each window: a site's with no table of its own, and D3876 a spill pair pipeline's (hashjoin/spill/pair.rs), which runs
/// the probe's steps over its kept rows with no site. Nothing when `t` declares no row.
pub(crate) fn bind_rows_only(b: &mut FunctionBuilder<'_>, env: &mut Env, t: &Rc<RefCell<Tables>>) {
    let tb = t.borrow();
    if !tb.lanes.is_empty() || tb.rows == 0 {
        return;
    }
    let out_slot = b.declare_stack_slot((crate::dict::strategies::percode_key::out_words(0, tb.rows) * 8) as u32, 8);
    let out = b.stack_addr(out_slot);
    b.store(out, 0, env.zero); // the flags slot: no table unfilled
    crate::dict::strategies::percode_key::bind_rows(b, env, out, 0, tb.rows);
    env.tables = Some(out);
}

/// Site `site_k`'s tables `t` bound for the window's part into `Env::tables` (agg_bind's out array: table `i` at byte
/// `24 i`, the flags at word `3 n`), each length table's row read built here once (`Tables::lens`), not per row. A
/// failing bind leaves through `l_exit` with its status.
pub(crate) fn bind(b: &mut FunctionBuilder<'_>, env: &mut Env, site_k: usize, t: &Rc<RefCell<Tables>>, l_exit: Block) {
    let tb = t.borrow();
    let n = tb.lanes.len();
    if n == 0 {
        drop(tb);
        bind_rows_only(b, env, t);
        return;
    }
    let spec_slot = b.declare_stack_slot((n * 8) as u32, 8);
    let out_slot = b.declare_stack_slot((crate::dict::strategies::percode_key::out_words(n, tb.rows) * 8) as u32, 8);
    let (spec, out) = (b.stack_addr(spec_slot), b.stack_addr(out_slot));
    for (i, &(lane, class)) in tb.lanes.iter().enumerate() {
        let w = b.iconst(Type::I64, (lane as u64 | class_word(class) << 16) as i64);
        b.store(spec, i as i32 * 8, w);
    }
    bound(b, env, site_k, &tb.bound);
    let site_v = b.iconst(Type::I64, site_k as i64);
    let n_v = b.iconst(Type::I64, n as i64);
    let h = b.declare_host_once("agg_bind", &[Type::Ptr, Type::I64, Type::Ptr, Type::I64, Type::Ptr], Some(Type::I64), abi::agg_bind as *const () as usize, mem());
    let r = b.call_host(h, &[env.ctx, site_v, spec, n_v, out], Some(Type::I64)).unwrap_or(env.zero);
    exit_neg(b, env, r, l_exit);
    crate::dict::strategies::percode_key::bind_rows(b, env, out, n, tb.rows);
    if !tb.sweeps.is_empty() {
        let all = b.load(Type::I64, out, (BIND_OUT_WORDS * n * 8) as i32);
        // D1747: table i's unfilled bit is bit 1 + i of the flags words (`abi::bind_flag_words`): word 0 shifted to table 0
        let words: PgVec<'_, Value> = std::iter::once(b.int_bin(IntBinOp::Ushr, all, env.one)).chain((1..abi::bind_flag_words(n)).map(|w| b.load(Type::I64, out, ((BIND_OUT_WORDS * n + w) * 8) as i32))).collect_in(env.mcx);
        let unfilled = words[1..].iter().fold(words[0], |a, &w| b.int_bin(IntBinOp::Or, a, w));
        let any = b.icmp(IntCond::Ne, unfilled, env.zero);
        let (l_sweep, l_ready) = (b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_sweep);
        b.cond_br(any, (l_sweep, &[]), (l_ready, &[]));
        b.switch_to(l_sweep);
        let pid = b.load_zext(Type::I32, env.ctx, site_off(site_k) as i32 + site::PART_ID as i32);
        let s32 = b.iconst(Type::I64, 32);
        let part = b.int_bin(IntBinOp::Shl, pid, s32);
        for sw in tb.sweeps.iter() {
            let sh = b.iconst(Type::I64, if sw.t < 63 { sw.t } else { (1 + sw.t) % 64 } as i64);
            let x = b.int_bin(IntBinOp::Ushr, words[(1 + sw.t) / 64], sh);
            let bit = b.int_bin(IntBinOp::And, x, env.one);
            let set = b.icmp(IntCond::Ne, bit, env.zero);
            let (l_do, l_next) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(set, (l_do, &[]), (l_next, &[]));
            b.switch_to(l_do);
            if crate::dict::percode_fill::lazy(sw) {
                crate::dict::percode_fill::faulting(b, env, site_v, out, tb.lanes[sw.t].0, sw, l_exit);
            } else {
                sweep(b, env, site_v, part, out, tb.lanes[sw.t].0, sw, l_exit);
            }
            b.br(l_next, &[]);
            b.switch_to(l_next);
        }
        let r = b.call_host(h, &[env.ctx, site_v, spec, n_v, out], Some(Type::I64)).unwrap_or(env.zero);
        exit_neg(b, env, r, l_exit);
        b.br(l_ready, &[]);
        b.switch_to(l_ready);
    }
    env.tables = Some(out);
    let lens = (0..n as u16).filter(|&i| tb.lanes[i as usize].1 == Class::Len).map(|i| (out, i, crate::dict::percode_len::table_ref(b, out, i, Class::Len, true))).collect(); // alloc: Program
    drop(tb);
    let mut tm = t.borrow_mut();
    tm.lens = lens;
    tm.site = Some(site_k);
}

/// Bit `code` of the word array at `words` set.
pub(super) fn set_bit(b: &mut FunctionBuilder<'_>, words: Value, code: Value) {
    let (six, m63) = (b.iconst(Type::I64, 6), b.iconst(Type::I64, 63));
    let wi = b.int_bin(IntBinOp::Ushr, code, six);
    let wp = b.ptr_add(words, wi, 8, 0);
    let w = b.load(Type::I64, wp, 0);
    let sh = b.int_bin(IntBinOp::And, code, m63);
    let one = b.iconst(Type::I64, 1);
    let bit = b.int_bin(IntBinOp::Shl, one, sh);
    let w2 = b.int_bin(IntBinOp::Or, w, bit);
    b.store(wp, 0, w2);
}

/// `PROGRAM` table `sw` (site lane `lane`, bind slot `sw.t` of `out`) swept for the part: every code rendered (a code
/// with no image keeps a zero entry; a failed dictionary read leaves through `l_exit` with its status, D344), the body evaluated over a one-column row holding the image, its bit,
/// word or Fp128 stored at the code, a raise or a NULL a fault bit (an Fp128's a zero entry); `agg_table_done` publishes.
/// D3115: the statement's sources that reach the part share one sweep: each sweeps the `SWEEP_CHUNK`-code chunks
/// `agg_table_claim` hands it until none is left, and `agg_table_done` returns once every chunk is swept.
#[allow(clippy::too_many_arguments)]
fn sweep(b: &mut FunctionBuilder<'_>, env: &mut Env, site_v: Value, part: Value, out: Value, lane: u8, sw: &Sweep, l_exit: Block) {
    let h_n = b.declare_host_once("agg_table_ncodes", &[Type::Ptr, Type::I64, Type::I64], Some(Type::I64), abi::agg_table_ncodes as *const () as usize, mem());
    let h_claim = b.declare_host_once("agg_table_claim", &[Type::Ptr, Type::I64, Type::I64], Some(Type::I64), abi::agg_table_claim as *const () as usize, mem());
    let h_done = b.declare_host_once("agg_table_done", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Some(Type::I64), abi::agg_table_done as *const () as usize, mem());
    let (lane_v, over_v) = (b.iconst(Type::I64, lane as i64), b.iconst(Type::I64, sw.over as i64));
    let n = b.call_host(h_n, &[env.ctx, site_v, lane_v], Some(Type::I64)).unwrap_or(env.zero);
    exit_neg(b, env, n, l_exit);
    let at = (BIND_OUT_WORDS * sw.t * 8) as i32;
    let data = b.load(Type::Ptr, out, at);
    let faults = b.load(Type::Ptr, out, at + 8);
    let (cv, cn) = crate::frame::hoist::words(b, env, sw.node, sw.ex);
    let (l_claim, l_claim_body) = cancel::header(b, env.ctx, env.x.l_cancel, &[Type::I64], env.mcx);
    let (l_loop, l_body) = cancel::header(b, env.ctx, env.x.l_cancel, &[Type::I64, Type::I64, Type::I64], env.mcx);
    let (l_eval, l_fault) = (b.create_block(&[]), b.create_block(&[]));
    let (l_next, l_done) = (b.create_block(&[Type::I64]), b.create_block(&[Type::I64]));
    b.br(l_claim, &[env.zero]);
    b.switch_to(l_claim_body);
    let nf = b.block_params(l_claim_body)[0];
    let start = b.call_host(h_claim, &[env.ctx, site_v, lane_v], Some(Type::I64)).unwrap_or(n);
    exit_neg(b, env, start, l_exit);
    let left = b.icmp(IntCond::Slt, start, n);
    let l_chunk = b.create_block(&[]);
    b.cond_br(left, (l_chunk, &[]), (l_done, &[nf]));
    b.switch_to(l_chunk);
    let chunk = b.iconst(Type::I64, gravity_abi::site::SWEEP_CHUNK as i64);
    let stop = b.int_bin(IntBinOp::Add, start, chunk);
    let inside = b.icmp(IntCond::Slt, stop, n);
    let end = b.select(inside, stop, n);
    b.br(l_loop, &[start, end, nf]);
    b.switch_to(l_body);
    let ps = b.block_params(l_body);
    let (code, end, nf) = (ps[0], ps[1], ps[2]);
    let more = b.icmp(IntCond::Slt, code, end);
    let l_code = b.create_block(&[]);
    b.cond_br(more, (l_code, &[]), (l_claim, &[nf]));
    b.switch_to(l_code);
    let word = b.int_bin(IntBinOp::Or, part, code);
    let p = b.call_host(env.core.render, &[env.ctx, site_v, over_v, word], Some(Type::Ptr)).unwrap_or(env.zero);
    let pi = b.conv(ConvOp::PtrToInt, Type::I64, p);
    exit_neg(b, env, pi, l_exit);
    let none = b.icmp(IntCond::Eq, pi, env.zero);
    b.cond_br(none, (l_next, &[nf]), (l_eval, &[]));
    b.switch_to(l_eval);
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
    b.cond_br(bad, (l_fault, &[]), (l_ok, &[]));
    b.switch_to(l_ok);
    if sw.elem == ELEM_BITS {
        let nz = b.icmp(IntCond::Ne, l.value, env.zero);
        let l_set = b.create_block(&[]);
        b.cond_br(nz, (l_set, &[]), (l_next, &[nf]));
        b.switch_to(l_set);
        set_bit(b, data, code);
    } else if sw.elem == ELEM_FP128 {
        crate::dict::strategies::percode_key::store(b, data, code, l.value);
    } else {
        if b.value_type(l.value) != Type::I64 {
            crate::refuse!(sw.node, Reason::Shape { why: crate::admit::refusal::ShapeWhy("a per-code word body whose value is not a word") });
        }
        let at = b.ptr_add(data, code, 8, 0);
        b.store(at, 0, l.value);
    }
    b.br(l_next, &[nf]);
    b.switch_to(l_fault);
    // an Fp128 entry that faulted stays [0, 0], which sends its rows to the re-evaluation (percode_key.rs `cell`): no bit
    let nf1 = if sw.elem == ELEM_FP128 { nf } else { set_bit(b, faults, code); b.int_bin(IntBinOp::Add, nf, env.one) };
    b.br(l_next, &[nf1]);
    b.switch_to(l_next);
    let nf = b.block_params(l_next)[0];
    env.hir.rewind(b); // the entry's results are consumed: the region holds one entry's, never a dictionary's
    let c1 = b.int_bin(IntBinOp::Add, code, env.one);
    b.br(l_loop, &[c1, end, nf]);
    b.switch_to(l_done);
    let nf = b.block_params(l_done)[0];
    let st = b.call_host(h_done, &[env.ctx, site_v, lane_v, nf], Some(Type::I64)).unwrap_or(env.zero);
    exit_neg(b, env, st, l_exit);
}

/// The site's tables bound clean for the window (`agg_bind`'s flags word, bit 0: no table has faults), as a `Bool`.
pub(crate) fn clean(b: &mut FunctionBuilder<'_>, env: &Env, t: &Rc<RefCell<Tables>>) -> Value {
    let n = t.borrow().lanes.len();
    let Some(out) = env.tables.filter(|_| n > 0) else { return b.bconst(n == 0) };
    let all = b.load(Type::I64, out, (BIND_OUT_WORDS * n * 8) as i32);
    let bit = b.int_bin(IntBinOp::And, all, env.one);
    b.icmp(IntCond::Ne, bit, env.zero)
}
