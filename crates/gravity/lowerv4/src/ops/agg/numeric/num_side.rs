//! NEW-numeric-1 N13: class.num_packed's keyed extreme (`MinNumDyn` / `MaxNumDyn`: min / max of an unconstrained numeric
//! column) with its side companion. Hooks / levers: -. Owner M1-FOLD.
//!
//! The state is `[key, side, n]`. In a record: the key slot (`MinI64` / `MaxI64`, gravity_fn typesem numeric
//! `order_key`), the side companion right after it (abi `NumSideMin` / `NumSideMax`: 0, or the address of the record's
//! copy of its extreme over the values its rows could not key, abi `NUM_SIDE_T` in its low bit; its validity the next
//! bit) and the key's validity as `n`. A row whose image keys (inline, else detoasted: raw.rs `num_key_slow`) folds its
//! key; a NULL folds nothing; any other row folds into the side copy where it arrives (`num_side_step`, cold): nothing
//! raises and nothing re-runs. A key take clears the tie bit and a side take sets it, so on equal values the bit names
//! the form of the last row at that value, PostgreSQL's member (numeric_smaller / numeric_larger keep the later of equal
//! values); merges compose it (abi `side_fold`). A carried state (registers: a window's or a run's carry) holds no copy,
//! so a value past the key there raises and the statement's recheck licence re-runs it in plan order (decompose agg
//! `held_whole`): the plain end keeps these extremes in its record (trans/plain.rs) and the hashed end takes no run carry
//! over them (hashagg.rs). N14: a pgrcolumnar lane of packed codes (`Rep::PackedNumDyn`, the column's only readers
//! these calls) keys a packed window's code at the window's scale with no image (typesem `order_key_of`); a verbatim
//! window's element is its image, read as above. Registers nothing; class_num.rs's `Num` calls it.
use gravity_expr::emit::CellValid;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, IntUnOp, Type, Value};

use crate::face::fold::Acc;
use crate::frame::env::Env;
use crate::rep::CellRef;

use crate::ops::agg::numeric::num_word::raise;
use crate::ops::agg::numeric::raw::word::{num_dyn_key, num_dyn_side_final, num_dyn_word, num_key_slow, num_pack_key, num_side_merge, num_side_step, KEY_TOAST};
use crate::rep::packednum::datum::NUM_IMAGE_BYTES;
use crate::rep::packednum::raw::datum::num_image;

/// The state's words in registers: `[key, side, n]`.
pub(crate) const WORDS: u8 = 3;

/// The side companion's validity: the bit after the key slot's, else its word's.
fn side_valid(off: i32, valid: CellValid) -> CellValid {
    match valid {
        CellValid::Bit { off, bit: 7 } => CellValid::Bit { off: off + 1, bit: 0 },
        CellValid::Bit { off, bit } => CellValid::Bit { off, bit: bit + 1 },
        _ => CellValid::NonZeroAt((off + 8) as u32),
    }
}

/// The key's identity: above (min) / below (max) every key.
fn key_ident(b: &mut FunctionBuilder<'_>, min: bool) -> Value {
    b.iconst(Type::I64, if min { i64::MAX } else { i64::MIN })
}

/// The state (an invalid key the identity, an invalid side 0).
fn load(b: &mut FunctionBuilder<'_>, env: &Env, min: bool, acc: &Acc<'_>) -> [Value; 3] {
    let Acc::Slot { base, off, valid } = *acc else {
        let Acc::Regs(w) = acc else { unreachable!() };
        return [w[0], w[1], w[2]];
    };
    let ok = crate::ops::shared::slots::slot_ok(b, env, base, valid);
    let (k, id) = (b.load(Type::I64, base, off), key_ident(b, min));
    let k = b.select(ok, k, id);
    let sok = crate::ops::shared::slots::slot_ok(b, env, base, side_valid(off, valid));
    let s = b.load(Type::I64, base, off + 8);
    let s = b.select(sok, s, env.zero);
    [k, s, b.conv(ConvOp::Zext, Type::I64, ok)]
}

fn store(b: &mut FunctionBuilder<'_>, env: &Env, acc: Acc<'_>, [k, s, n]: [Value; 3]) {
    match acc {
        Acc::Regs(r) => r[..3].copy_from_slice(&[k, s, n]),
        Acc::Slot { base, off, valid } => {
            b.store(base, off, k);
            let any = b.icmp(IntCond::Ne, n, env.zero);
            crate::ops::shared::slots::mark(b, env, base, valid, any);
            b.store(base, off + 8, s);
            let some = b.icmp(IntCond::Ne, s, env.zero);
            crate::ops::shared::slots::mark(b, env, base, side_valid(off, valid), some);
        }
    }
}

/// `s` with the tie bit cleared where `take` holds (a key take is the later member).
fn clear_where(b: &mut FunctionBuilder<'_>, take: Value, s: Value) -> Value {
    let m = b.iconst(Type::I64, !(gravity_abi::breaker::NUM_SIDE_T as i64));
    let cleared = b.int_bin(IntBinOp::And, s, m);
    b.select(take, cleared, s)
}

/// The empty state (a fresh record's side word 0: a stale address behind an unset validity bit is never read).
pub(crate) fn init(b: &mut FunctionBuilder<'_>, env: &Env, min: bool, acc: Acc<'_>) {
    let id = key_ident(b, min);
    store(b, env, acc, [id, env.zero, env.zero]);
}

/// Numeric cell `c` folded into `acc`: its key, else (cold: `num_dyn_key` below `KEY_TOAST`) a toasted image's key read
/// detoasted, else its value into the side copy, which a record's slot holds (`images`: the end's image cursor, `None`
/// for a pinned region per copy, the plain end's one record); a carried state raises instead.
pub(crate) fn step(b: &mut FunctionBuilder<'_>, env: &mut Env, min: bool, acc: Acc<'_>, c: CellRef<'_>, images: Option<Value>) {
    use gravity_fn::typesem::numeric::{KEY_NAN, NO_KEY};
    // the row's image, or (N14) a pgrcolumnar lane of packed codes (`Rep::PackedNumDyn`), read below per window
    let packed = c.cell.rep == gravity_program::Rep::PackedNumDyn;
    let (p, none) = if packed {
        (None, c.null.unwrap_or_else(|| b.bconst(false)))
    } else {
        let mut cx = env.lx(b, env.node);
        let (p, none) = crate::rep::datum(&mut cx, c, &crate::step::expr::tf_datum(types_core::catalog::NUMERICOID));
        env.memo = cx.memo;
        (Some(p), none)
    };
    let st = load(b, env, min, &acc);
    let slot = matches!(acc, Acc::Slot { .. });
    // the image `p` rides each block's parameter: the datum's, a verbatim window's element, or a packed code's render
    let (l_call, l_cold, l_slow, l_side) = (b.create_block(&[Type::I64]), b.create_block(&[Type::I64]), b.create_block(&[Type::I64]), b.create_block(&[Type::I64]));
    let (l_key, l_join) = (b.create_block(&[Type::I64]), b.create_block(&[Type::I64; 3]));
    for l in [l_cold, l_slow, l_side] {
        b.mark_cold(l);
    }
    let (nan, no) = (b.iconst(Type::I64, KEY_NAN), b.iconst(Type::I64, NO_KEY));
    match p {
        Some(p) => b.cond_br(none, (l_key, &[nan]), (l_call, &[p])),
        None => {
            // N14: a verbatim window's element is its image (the datum path below); a packed window's code keys at the
            // window's scale with no image (typesem `order_key_of`); a code past the key renders to its image (cold) for the
            // side copy. NaN and +-Infinity never sit in a packed chunk (pgrc PACKED_NUMERIC refuses them).
            let (l_read, l_code, l_img) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
            b.mark_cold(l_img);
            b.cond_br(none, (l_key, &[nan]), (l_read, &[]));
            b.switch_to(l_read);
            let node = env.node;
            let mut cx = env.lx(b, node);
            let read = crate::rep::packednum::datum::dyn_word(&mut cx, c);
            env.memo = cx.memo;
            let Some((verbatim, w, s)) = read else {
                crate::refuse!(node, crate::admit::refusal::Reason::Bug { payload: "a dynamic-scale packed numeric cell that is not its lane" })
            };
            b.cond_br(verbatim, (l_call, &[w]), (l_code, &[]));
            b.switch_to(l_code);
            let (fast, k) = pack_key(b, env, w, s);
            let l_host = b.create_block(&[]);
            b.mark_cold(l_host);
            b.cond_br(fast, (l_key, &[k]), (l_host, &[]));
            b.switch_to(l_host);
            let h = b.declare_host_once("class_num::num_pack_key", &[Type::I64; 2], Some(Type::I64), num_pack_key as *const () as usize, HostFlags { pure: true, ..HostFlags::default() });
            let k = b.call_host(h, &[w, s], Some(Type::I64)).expect("returns the order key");
            let past = b.icmp(IntCond::Eq, k, no);
            b.cond_br(past, (l_img, &[]), (l_key, &[k]));
            b.switch_to(l_img);
            let at = b.declare_stack_slot(NUM_IMAGE_BYTES, 8);
            let out = b.stack_addr(at);
            let out = b.conv(ConvOp::PtrToInt, Type::I64, out);
            let h = b.declare_host_once("rep_num_image", &[Type::I64; 3], Some(Type::I64), num_image as *const () as usize, HostFlags { touches_memory: true, ..HostFlags::default() });
            let pi = b.call_host(h, &[w, s, out], Some(Type::I64)).expect("returns the image");
            let failed = b.icmp(IntCond::Eq, pi, env.zero);
            let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
            let l_ok = b.create_block(&[]);
            b.cond_br(failed, (env.l_raise, &[e]), (l_ok, &[]));
            b.switch_to(l_ok);
            b.br(l_side, &[pi]);
        }
    }
    b.switch_to(l_call);
    let p = b.block_params(l_call)[0];
    let h = b.declare_host_once("class_num::num_dyn_key", &[Type::I64], Some(Type::I64), num_dyn_key as *const () as usize, HostFlags { touches_memory: true, pure: true, ..HostFlags::default() });
    let k = b.call_host(h, &[p], Some(Type::I64)).expect("returns the order key");
    let toast = b.iconst(Type::I64, KEY_TOAST);
    let cold = b.icmp(IntCond::Sle, k, toast);
    b.cond_br(cold, (l_cold, &[p]), (l_key, &[k]));
    // a toasted image's key, detoasted (`num_key_slow`); a value past the key
    b.switch_to(l_cold);
    let p = b.block_params(l_cold)[0];
    let toasted = b.icmp(IntCond::Eq, k, toast);
    b.cond_br(toasted, (l_slow, &[p]), (l_side, &[p]));
    b.switch_to(l_slow);
    let p = b.block_params(l_slow)[0];
    let h = b.declare_host_once("class_num::num_key_slow", &[Type::I64], Some(Type::I64), num_key_slow as *const () as usize, HostFlags { touches_memory: true, ..HostFlags::default() });
    let k = b.call_host(h, &[p], Some(Type::I64)).expect("returns the order key");
    let past = b.icmp(IntCond::Eq, k, no);
    b.cond_br(past, (l_side, &[p]), (l_key, &[k]));
    b.switch_to(l_side);
    let p = b.block_params(l_side)[0];
    if slot {
        let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
        let h = b.declare_host_once("class_num::num_side_step", &[Type::Ptr, Type::I64, Type::Ptr, Type::I64, Type::I64], Some(Type::I64), num_side_step as *const () as usize, flags);
        let cur = match images {
            Some(v) => v,
            None => b.iconst(Type::Ptr, 0),
        };
        let mv = b.iconst(Type::I64, i64::from(min));
        let s = b.call_host(h, &[env.ctx, st[1], cur, p, mv], Some(Type::I64)).expect("returns the side word");
        let failed = b.icmp(IntCond::Eq, s, env.zero);
        let l_ok = b.create_block(&[]);
        let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
        b.cond_br(failed, (env.l_raise, &[e]), (l_ok, &[]));
        b.switch_to(l_ok);
        b.br(l_join, &[st[0], s, st[2]]);
    } else {
        let t = b.bconst(true);
        raise(b, env, t, 2);
        b.br(l_key, &[no]); // unreachable: `raise` left
    }
    b.switch_to(l_key);
    let k = b.block_params(l_key)[0];
    let t1 = b.bconst(true);
    let ok = b.int_bin(IntBinOp::Xor, none, t1);
    let four = b.iconst(Type::I64, 4);
    let (vr, vs) = (b.int_bin(IntBinOp::Sshr, k, four), b.int_bin(IntBinOp::Sshr, st[0], four));
    // numeric_smaller / numeric_larger keep the later of equal values: a tie on `key >> 4` takes the row
    let better = b.icmp(if min { IntCond::Sle } else { IntCond::Sge }, vr, vs);
    let empty = b.icmp(IntCond::Eq, st[2], env.zero);
    let take = b.int_bin(IntBinOp::Or, better, empty);
    let take = b.int_bin(IntBinOp::And, take, ok);
    let w = b.select(take, k, st[0]);
    let one = b.conv(ConvOp::Zext, Type::I64, ok);
    let n = b.int_bin(IntBinOp::Add, st[2], one);
    let s = clear_where(b, take, st[1]);
    b.br(l_join, &[w, s, n]);
    b.switch_to(l_join);
    let v = b.block_params(l_join);
    let v = [v[0], v[1], v[2]];
    store(b, env, acc, v);
}

/// N14: typesem numeric `order_key_of(w, s)` in line, for a packed code of at most 16 digits at a scale of at most 15
/// (`fast`): the digit count `n` from the bit length (`t` = floor(bits x log10 2), plus one where |w| >= 10^t), the
/// coefficient |w| x 10^(16 - n), the key ((n - s + 15) x 10^16 + coefficient) x 16 + s, negated about `s` for w < 0, and
/// `s` for 0 (typesem `key_at`). The rest (17 digits, a scale past 15) take the host's (cold).
fn pack_key(b: &mut FunctionBuilder<'_>, env: &Env, w: Value, s: Value) -> (Value, Value) {
    let c = |b: &mut FunctionBuilder<'_>, v: i64| b.iconst(Type::I64, v);
    let neg = b.icmp(IntCond::Slt, w, env.zero);
    let a = { let nw = b.int_un(IntUnOp::Neg, w); b.select(neg, nw, w) };
    // the bit length of |w| | 1 (clz of 0 left to no backend): 0 then counts 0 digits below
    let bits = { let a1 = b.int_bin(IntBinOp::Or, a, env.one); let z = b.int_un(IntUnOp::Clz, a1); let k = c(b, 64); b.int_bin(IntBinOp::Sub, k, z) };
    let t = { let k = c(b, 1233); let x = b.int_bin(IntBinOp::Mul, bits, k); let sh = c(b, 12); b.int_bin(IntBinOp::Ushr, x, sh) };
    let tab = b.iconst(Type::Ptr, gravity_fn::typesem::numeric::POW10.as_ptr() as i64);
    let pt = { let p = b.ptr_add(tab, t, 8, 0); b.load(Type::I64, p, 0) };
    let n = { let ge = b.icmp(IntCond::Uge, a, pt); let ge = b.conv(ConvOp::Zext, Type::I64, ge); b.int_bin(IntBinOp::Add, t, ge) };
    let (sixteen, fifteen) = (c(b, 16), c(b, 15));
    let fast = { let x = b.icmp(IntCond::Ule, n, sixteen); let y = b.icmp(IntCond::Ule, s, fifteen); b.int_bin(IntBinOp::And, x, y) };
    let at = { let d = b.int_bin(IntBinOp::Sub, sixteen, n); b.select(fast, d, env.zero) };
    let coef = { let p = b.ptr_add(tab, at, 8, 0); let pw = b.load(Type::I64, p, 0); b.int_bin(IntBinOp::Mul, a, pw) };
    let k = {
        let e = { let d = b.int_bin(IntBinOp::Sub, n, s); b.int_bin(IntBinOp::Add, d, fifteen) };
        let x = { let p16 = c(b, 10i64.pow(16)); let x = b.int_bin(IntBinOp::Mul, e, p16); b.int_bin(IntBinOp::Add, x, coef) };
        let four = c(b, 4);
        b.int_bin(IntBinOp::Shl, x, four)
    };
    let pos = b.icmp(IntCond::Sgt, w, env.zero);
    let key = { let kp = b.int_bin(IntBinOp::Add, k, s); let kn = b.int_bin(IntBinOp::Sub, s, k); b.select(pos, kp, kn) };
    let zero = b.icmp(IntCond::Eq, w, env.zero);
    (fast, b.select(zero, s, key))
}

/// Two states, `from` the later rows' (a plain end's window into its carry, a record into another, as `step` holds
/// them): a key tie keeps `from`'s member; a side copy `from` holds folds by value (abi `side_fold`, cold), else the tie
/// bit clears where the key took `from`'s member. Partials merged in another order (W8) keep either member, both equal
/// by numeric_cmp, exact, and an input row's text.
pub(crate) fn merge(b: &mut FunctionBuilder<'_>, env: &mut Env, min: bool, into: Acc<'_>, from: Acc<'_>) {
    let (x, y) = (load(b, env, min, &into), load(b, env, min, &from));
    let four = b.iconst(Type::I64, 4);
    let (vx, vy) = (b.int_bin(IntBinOp::Sshr, x[0], four), b.int_bin(IntBinOp::Sshr, y[0], four));
    let better = b.icmp(if min { IntCond::Sle } else { IntCond::Sge }, vy, vx);
    let empty = b.icmp(IntCond::Eq, x[2], env.zero);
    let some = b.icmp(IntCond::Ne, y[2], env.zero);
    let take = b.int_bin(IntBinOp::Or, better, empty);
    let take = b.int_bin(IntBinOp::And, take, some);
    let w = b.select(take, y[0], x[0]);
    let n = b.int_bin(IntBinOp::Add, x[2], y[2]);
    let s0 = clear_where(b, take, x[1]);
    let (l_side, l_join) = (b.create_block(&[]), b.create_block(&[Type::I64]));
    let has = b.icmp(IntCond::Ne, y[1], env.zero);
    b.cond_br(has, (l_side, &[]), (l_join, &[s0]));
    b.switch_to(l_side);
    let h = b.declare_host_once("class_num::num_side_merge", &[Type::I64; 4], Some(Type::I64), num_side_merge as *const () as usize, HostFlags { touches_memory: true, ..HostFlags::default() });
    let (kt, mv) = (b.conv(ConvOp::Zext, Type::I64, take), b.iconst(Type::I64, i64::from(min)));
    let s = b.call_host(h, &[x[1], y[1], kt, mv], Some(Type::I64)).expect("returns the side word");
    b.br(l_join, &[s]);
    b.switch_to(l_join);
    let s = b.block_params(l_join)[0];
    store(b, env, into, [w, s, n]);
}

/// The final: the key's image (gravity_fn `num_word` at the key's dscale) when no side copy is held, else the better of
/// the two by value, a tie the member the tie bit names (raw.rs `num_dyn_side_final`); NULL over no row.
pub(crate) fn finish(b: &mut FunctionBuilder<'_>, env: &mut Env, min: bool, acc: Acc<'_>) -> (Value, Value) {
    use crate::ops::agg::trans::plain::host_final as fin;
    let [k, s, n] = load(b, env, min, &acc);
    let (nk, ns) = (b.icmp(IntCond::Eq, n, env.zero), b.icmp(IntCond::Eq, s, env.zero));
    let none = b.int_bin(IntBinOp::And, nk, ns);
    let (l_key, l_side, l_join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64]));
    b.cond_br(ns, (l_key, &[]), (l_side, &[]));
    b.switch_to(l_key);
    let v = fin(b, env, "class_num::num_dyn_word", num_dyn_word as *const () as usize, &[k], nk);
    b.br(l_join, &[v]);
    b.switch_to(l_side);
    let (never, mv) = (b.bconst(false), b.iconst(Type::I64, i64::from(min)));
    let v = fin(b, env, "class_num::num_dyn_side_final", num_dyn_side_final as *const () as usize, &[k, n, s, mv], never);
    b.br(l_join, &[v]);
    b.switch_to(l_join);
    (b.block_params(l_join)[0], none)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind as S};
    use gravity_fn::typesem::numeric::{order_key_of, NO_KEY};

    /// The lowered key of a packed code is typesem's `order_key_of` wherever it takes the code (at most 16 digits, a
    /// scale of at most 15): every digit-count boundary, both signs, zero, scales 0 ..= 17; the rest go to the host.
    #[test]
    fn a_lowered_packed_key_is_order_key_of() {
        let mut ms: Vec<i64> = vec![0, 1, 7, 9, 36_028_797_018_963_967]; // alloc: Program
        for d in 1..=17u32 {
            let p = 10i64.pow(d);
            ms.extend([p / 10, p - 1, p, p + 1, p / 7, p / 3 * 2]);
        }
        let mut x = 0x2545_F491_4F6C_DD1Du64; // Marsaglia xorshift seed (not a hash constant)
        for _ in 0..300 {
            x ^= x << 13; x ^= x >> 7; x ^= x << 17;
            ms.push((x % (1u64 << 55)) as i64 >> (x % 50));
        }
        let mut n = 0;
        for s in 0..=17i64 {
            for &a in &ms {
                for m in [a, -a] {
                    let got = crate::frame::stop::tests::run1(&[], &mut |b, env| {
                        let (w, sv) = (b.iconst(Type::I64, m), b.iconst(Type::I64, s));
                        let (fast, k) = pack_key(b, env, w, sv);
                        let none = b.iconst(Type::I64, NO_KEY);
                        b.select(fast, k, none)
                    });
                    let fast = s <= 15 && a.unsigned_abs() < 10u64.pow(16);
                    assert_eq!(got, if fast { order_key_of(m, s) } else { NO_KEY }, "{m} at {s}");
                    n += usize::from(fast);
                }
            }
        }
        assert!(n > 5000, "{n}");
    }

    /// The side companion is the slot after its key's (decompose agg `cell`), so its word and validity bit are
    /// `off + 8` and `side_valid`'s (abi's layout rule), the inline tier's last slot included.
    #[test]
    fn the_side_companion_sits_after_its_key() {
        for keys in [&[][..], &[KeyKind::I32][..], &[KeyKind::I64, KeyKind::I64][..]] {
            for (pad, pairs) in (0..=3).map(|p| (p, 2)).chain((0..=5).map(|p| (p, 1))) {
                let mut slots = vec![S::Count];
                slots.extend(core::iter::repeat_n(S::SumI64, pad));
                slots.extend([S::MinI64, S::NumSideMin, S::MaxI64, S::NumSideMax].into_iter().take(2 * pairs));
                let l = RecordLayout::new(keys, &slots);
                for j in [pad + 1, pad + 3].into_iter().take(pairs) {
                    let (o, bit) = l.valid_bit(j);
                    let v = side_valid(l.slot_off(j) as i32, CellValid::Bit { off: o as u32, bit });
                    let (o2, bit2) = l.valid_bit(j + 1);
                    assert_eq!((l.slot_off(j) + 8, v), (l.slot_off(j + 1), CellValid::Bit { off: o2 as u32, bit: bit2 }), "pad {pad} slot {j}");
                }
            }
        }
    }
}
