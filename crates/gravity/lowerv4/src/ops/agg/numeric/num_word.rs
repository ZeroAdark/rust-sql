//! Numeric mantissa words. Hooks / levers: -. Budget 200 lines (files-extra.tsv, +25 % gate). Owner M1-FOLD.
//!
//! Split from class_num.rs by responsibility (D267): a numeric cell's word at the aggregate's scale (a packed lane's word,
//! any other image through `num_mantissa`; with D256's dscale companion the image's own dscale, a smaller one widened),
//! PostgreSQL's specials algebra (NaN wins, opposite infinities make NaN), the exact i128 add with its sticky sentinel,
//! the raise of a word the fold cannot take, and the dscale finals (the sum divided down exactly to the largest dscale
//! summed, then num_sum / num_avg there). Used by class_num.rs, num_window.rs and memo_num.rs; registers nothing.
use gravity_fn::ErrSlot;
use gravity_fn::agg_final::{MANT_NAN, MANT_NINF, MANT_PINF, NO_MANTISSA, SENT_HI, SP_NAN, SP_NINF, SP_OVERFLOW, SP_PINF, SUM_HI_LIMIT};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_program::{Rep, Until};

use crate::face::fold::{NumRoad, NumShortImpl, NumShortMech};
use crate::face::strategy::NUM_MEMO;
use crate::frame::env::Env;
use crate::rep::{CellRef, RetainHome};
pub(crate) use crate::ops::agg::numeric::raw::word::{mantissa, num_avg_ds, num_sum_ds};

/// A row's term at the aggregate's scale: `lo` with `hi` (`None`: `lo`'s sign), the specials code (`None`: no leaf can be
/// special; 0 finite), entered where `ok`.
#[derive(Clone, Copy)]
pub(crate) struct Term {
    pub lo: Value,
    pub hi: Option<Value>,
    pub sp: Option<Value>,
    pub ok: Value,
    /// D256: the value's dscale (`None`: the aggregate's scale).
    pub ds: Option<Value>,
}

/// The raise of a mantissa the fold cannot take (a toasted image, or a sum past 128 bits): the error named, then `HOST`.
pub(crate) fn raise(b: &mut FunctionBuilder<'_>, env: &Env, bad: Value, what: i64) {
    let (l_bad, l_ok) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(bad, (l_bad, &[]), (l_ok, &[]));
    b.switch_to(l_bad);
    let h = b.declare_host_once("class_num::fold_error", &[Type::I64], Some(Type::I64), fold_error as *const () as usize, HostFlags { may_error: true, ..HostFlags::default() });
    let w = b.iconst(Type::I64, what);
    b.call_host(h, &[w], Some(Type::I64));
    let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    b.br(env.l_raise, &[e]);
    b.switch_to(l_ok);
}

gravity_kernels::contract::host! {
    #[cold]
    fn fold_error(what: i64) -> i64 {
        let msg = match what {
            0 => "gravity: a numeric value the mantissa fold cannot read (a toasted or unscaled image)",
            // NEW-numeric-1: a carried state (num_side.rs); the statement's recheck licence re-runs it in plan order
            2 => "gravity: a numeric value past the min / max order key (16 significant digits, dscale 15)",
            _ => "gravity: a numeric term past 128 bits",
        };
        gravity_fn::stash_error(Box::new(gravity_fn::PgError::error(msg))); // alloc: Cold
        0
    }

    #[cold]
    fn typmod_error(typmod: i64) -> i64 {
        gravity_fn::stash_error(gravity_fn::typesem::numeric::typmod_overflow(typmod as i32));
        0
    }
}

/// PS4-21: where `bad`, apply_typmod's numeric field overflow for `typmod` (a typmod cast's num.window node).
pub(crate) fn raise_typmod(b: &mut FunctionBuilder<'_>, env: &Env, bad: Value, typmod: i32) {
    let (l_bad, l_ok) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(bad, (l_bad, &[]), (l_ok, &[]));
    b.switch_to(l_bad);
    let h = b.declare_host_once("class_num::typmod_error", &[Type::I64], Some(Type::I64), typmod_error as *const () as usize, HostFlags { may_error: true, ..HostFlags::default() });
    let w = b.iconst(Type::I64, typmod as i64);
    b.call_host(h, &[w], Some(Type::I64));
    let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    b.br(env.l_raise, &[e]);
    b.switch_to(l_ok);
}

/// D256: a (lo, hi) sum at `scale` divided down exactly to `d`, the largest dscale summed (its digits below `d` are
/// zero; a specials sentinel stays), then numeric sum / avg's final at `d`, their result dscale.
pub(crate) fn down(lo: u64, hi: u64, scale: u64, d: u64) -> (u64, u64, u64) {
    let d = d.min(scale);
    // a special, or no places to drop (the division by 1 was an i128 __divti3 call per rendered group)
    if hi as i64 == SENT_HI || d == scale {
        return (lo, hi, d);
    }
    let v = (((hi as i64 as i128) << 64) | lo as i128) / 10i128.pow((scale - d) as u32);
    (v as u64, (v >> 64) as u64, d)
}

pub(crate) fn kept(r: gravity_fn::Ret, e: &mut ErrSlot) -> u64 {
    if r.status == gravity_fn::OK {
        return r.val as u64;
    }
    if let Some(err) = e.take() {
        gravity_fn::stash_error(err);
    }
    0
}

/// Numeric cell `c`'s mantissa word at `scale` (a sentinel for a special), its NULL flag and, with `ds` (D256), its
/// dscale: a packed lane's word (a verbatim window's image read at the column's scale), any other datum's image through
/// `num_mantissa`.
pub(crate) fn word(b: &mut FunctionBuilder<'_>, env: &mut Env, c: CellRef<'_>, scale: i16, ds: bool) -> (Value, Value, Option<Value>) {
    let f = b.bconst(false);
    let null = c.null.unwrap_or(f);
    let mut cx = env.lx(b, env.node);
    if matches!(c.cell.rep, Rep::PackedNum { .. }) && crate::rep::num_of(c.cell.rep).scale == scale {
        let k = crate::rep::retain(&mut cx, c, &crate::step::expr::tf_datum(1700), Until::Statement, RetainHome::Frame);
        env.memo = cx.memo;
        return (k.val, null, ds.then(|| b.iconst(Type::I64, scale as i64)));
    }
    let (p, none) = crate::rep::datum(&mut cx, c, &crate::step::expr::tf_datum(1700));
    env.memo = cx.memo;
    // Road num.memo: one decode per (datum, scale) per row; the row memo's branch rule keeps the words
    // dominating
    let (k, memo) = ((p, none, scale, ds), env.applies(NUM_MEMO));
    if let Some((m, d)) = env.memo.num(k).filter(|_| memo) {
        return (m, none, ds.then_some(d));
    }
    let short = NumRoad::Short.on().then(|| NumRoad::Short.need::<NumShortImpl>(&env.node).0);
    let (m, d) = mantissa_at(b, env, (p, none), scale, ds, short);
    if memo {
        env.memo.put_num(k, (m, d));
    }
    (m, none, ds.then_some(d))
}

/// Datum `p`'s `(mantissa, dscale)` through `num_mantissa` (`(0, 0)` when `none`; dscale 0 without `ds`); road num.short's
/// `short` reads the image first, and the host call runs, cold, only for one it declines.
fn mantissa_at(b: &mut FunctionBuilder<'_>, env: &mut Env, (p, none): (Value, Value), scale: i16, ds: bool, short: Option<&dyn NumShortMech>) -> (Value, Value) {
    let (l_call, l_out) = (b.create_block(&[]), b.create_block(&[Type::I64, Type::I64]));
    b.cond_br(none, (l_out, &[env.zero, env.zero]), (l_call, &[]));
    b.switch_to(l_call);
    if let Some(s) = short {
        let (m, ok, d) = s.decode(b, env, p, scale, ds);
        let l_host = b.create_block(&[]);
        b.mark_cold(l_host);
        b.cond_br(ok, (l_out, &[m, d.unwrap_or(env.zero)]), (l_host, &[]));
        b.switch_to(l_host);
    }
    let (name, pure) = if ds { ("class_num::mantissa_ds", false) } else { ("class_num::mantissa", true) };
    let h = b.declare_host_once(name, &[Type::I64, Type::I64, Type::Ptr], Some(Type::I64), mantissa as *const () as usize, HostFlags { touches_memory: true, pure, ..HostFlags::default() });
    let s = b.iconst(Type::I64, scale as i64);
    let slot = ds.then(|| b.declare_stack_slot(8, 8));
    let dp = match slot {
        Some(sl) => b.stack_addr(sl),
        None => b.iconst(Type::Ptr, 0),
    };
    let m = b.call_host(h, &[p, s, dp], Some(Type::I64)).expect("returns the mantissa");
    let d = if ds { b.load(Type::I64, dp, 0) } else { env.zero };
    b.br(l_out, &[m, d]);
    b.switch_to(l_out);
    let v = b.block_params(l_out);
    (v[0], v[1])
}

/// A mantissa word's finite value (0 for a sentinel) and whether it is one where `ok`: `NO_MANTISSA`, -Inf, +Inf and NaN
/// are i64::MIN, MIN + 1, MAX - 1 and MAX, the words `m + 2` (wrapping) puts below MIN + 4 (`specials` codes them).
pub(crate) fn sentinel(b: &mut FunctionBuilder<'_>, env: &Env, m: Value, ok: Value) -> (Value, Value) {
    let (two, lim) = (b.iconst(Type::I64, 2), b.iconst(Type::I64, i64::MIN + 4));
    let m2 = b.int_bin(IntBinOp::Add, m, two);
    let s = b.icmp(IntCond::Slt, m2, lim);
    (b.select(s, env.zero, m), b.int_bin(IntBinOp::And, s, ok))
}

/// A mantissa word's finite value (0 for a special) and specials code; `NO_MANTISSA` raises where `ok`.
pub(crate) fn specials(b: &mut FunctionBuilder<'_>, env: &Env, m: Value, ok: Value) -> (Value, Value) {
    let none = b.iconst(Type::I64, NO_MANTISSA);
    let bad = b.icmp(IntCond::Eq, m, none);
    let bad = b.int_bin(IntBinOp::And, bad, ok);
    raise(b, env, bad, 0);
    let mut sp = env.zero;
    for (w, code) in [(MANT_NINF, SP_NINF), (MANT_PINF, SP_PINF), (MANT_NAN, SP_NAN)] {
        let (wv, cv) = (b.iconst(Type::I64, w), b.iconst(Type::I64, code));
        let is = b.icmp(IntCond::Eq, m, wv);
        sp = b.select(is, cv, sp);
    }
    let special = b.icmp(IntCond::Ne, sp, env.zero);
    (b.select(special, env.zero, m), sp)
}

/// PostgreSQL's `x + y` over specials codes (0 finite): NaN wins, opposite infinities make NaN, an infinity carries;
/// D299's OVERFLOW is a finite sum to them (an infinity or NaN wins over it, it wins over a finite).
pub(crate) fn add_codes(b: &mut FunctionBuilder<'_>, env: &Env, x: Value, y: Value) -> Value {
    let (nan, pinf, two) = (b.iconst(Type::I64, SP_NAN), b.iconst(Type::I64, SP_PINF), b.iconst(Type::I64, 2));
    let inf = |b: &mut FunctionBuilder<'_>, c: Value| {
        let d = b.int_bin(IntBinOp::Sub, c, pinf);
        b.icmp(IntCond::Ult, d, two)
    };
    let (xi, yi) = (inf(b, x), inf(b, y));
    let xz = b.icmp(IntCond::Eq, x, env.zero);
    let (xn, yn) = (b.icmp(IntCond::Eq, x, nan), b.icmp(IntCond::Eq, y, nan));
    let differ = b.icmp(IntCond::Ne, x, y);
    let both = b.int_bin(IntBinOp::And, xi, yi);
    let clash = b.int_bin(IntBinOp::And, both, differ);
    let any_nan = b.int_bin(IntBinOp::Or, xn, yn);
    let bad = b.int_bin(IntBinOp::Or, any_nan, clash);
    let r = b.select(xz, y, x);
    let r = b.select(yi, y, r);
    let r = b.select(xi, x, r);
    b.select(bad, nan, r)
}

/// D299: a finite sum whose hi word left [-SUM_HI_LIMIT, SUM_HI_LIMIT) is the sticky OVERFLOW code (every final raises it).
pub(crate) fn capped(b: &mut FunctionBuilder<'_>, lo: Value, hi: Value) -> (Value, Value) {
    let (lim, span) = (b.iconst(Type::I64, SUM_HI_LIMIT), b.iconst(Type::I64, i64::MIN));
    let h = b.int_bin(IntBinOp::Add, hi, lim);
    let out = b.icmp(IntCond::Uge, h, span);
    let (code, sent) = (b.iconst(Type::I64, SP_OVERFLOW), b.iconst(Type::I64, SENT_HI));
    (b.select(out, code, lo), b.select(out, sent, hi))
}

/// `a + t` over two i128 sums either of which may be a sentinel: finite + finite adds, past the range OVERFLOW (D299: an
/// add of two in-range sums cannot wrap); a sentinel is sticky.
pub(crate) fn add_sum(b: &mut FunctionBuilder<'_>, env: &Env, alo: Value, ahi: Value, tlo: Value, thi: Value) -> (Value, Value) {
    let sent = b.iconst(Type::I64, SENT_HI);
    let m = b.int_bin(IntBinOp::Smin, ahi, thi);
    let slow = b.icmp(IntCond::Eq, m, sent);
    let (l_slow, l_fast, l_join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64, Type::I64]));
    b.cond_br(slow, (l_slow, &[]), (l_fast, &[]));
    b.switch_to(l_fast);
    let (lo, hi) = crate::ops::agg::trans::class_word::add128(b, alo, ahi, tlo, thi);
    let (lo, hi) = capped(b, lo, hi);
    b.br(l_join, &[lo, hi]);
    b.switch_to(l_slow);
    let (a_s, t_s) = (b.icmp(IntCond::Eq, ahi, sent), b.icmp(IntCond::Eq, thi, sent));
    let (ca, ct) = (b.select(a_s, alo, env.zero), b.select(t_s, tlo, env.zero));
    let code = add_codes(b, env, ca, ct);
    b.br(l_join, &[code, sent]);
    b.switch_to(l_join);
    (b.block_params(l_join)[0], b.block_params(l_join)[1])
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use gravity_fn::Region;

    /// D299: an overflowed sum's final (sum, avg, a dscale final) returns 0 with its 22003 stashed by name for the driver;
    /// through gravity_expr's host the message was dropped ("an error without a message").
    #[test]
    fn an_overflowed_sum_final_keeps_its_error() {
        let sent = SENT_HI as u64;
        for (ds, n) in [(2, 0), (0, 0), (2, 3)] {
            let mut bytes = [0u8; 64];
            let mut r = Region::over(&mut bytes);
            let rp = &mut r as *mut Region as u64;
            // SAFETY: `r` is a live region over `bytes`.
            let v = unsafe { if n == 0 { num_sum_ds(rp, SP_OVERFLOW as u64, sent, 4, ds) } else { num_avg_ds(rp, SP_OVERFLOW as u64, sent, n, 4, ds) } };
            assert_eq!(v, 0);
            let err = gravity_fn::take_error().expect("the stashed error");
            assert_eq!(err.sqlstate(), types_error::ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE);
        }
    }

    /// A short numeric image's dscale read from its header and its mantissa widened to the scale (`dsp` set), exact otherwise;
    /// D4567: a dscale past the scale reads the value at the scale while no digit lies below it (a quotient's image).
    #[test]
    fn datum_dscales_are_read_and_widened() {
        // 4-byte varlena header, then numeric.c's short header (0x8000 | dscale << 7 | weight) and base-10000 digits
        let img = |h: u16, digits: &[u16]| -> Vec<u8> {
            let mut v = (((4 + 2 + 2 * digits.len()) as u32) << 2).to_le_bytes().to_vec();
            v.extend(h.to_le_bytes());
            digits.iter().for_each(|d| v.extend(d.to_le_bytes()));
            v
        };
        let run = |v: &[u8], ds: bool| {
            let mut d = -1i64;
            let m = unsafe { mantissa(v.as_ptr() as u64, 4, if ds { &mut d as *mut i64 as u64 } else { 0 }) };
            (m, d)
        };
        assert_eq!(run(&img(0x8000 | 2 << 7, &[1, 5000]), true), (15_000, 2));
        assert_eq!(run(&img(0x8000, &[]), true), (0, 0));
        assert_eq!(run(&img(0xC000, &[]), true), (MANT_NAN, 0));
        assert_eq!(run(&img(0x8000 | 6 << 7, &[1, 5000]), true), (15_000, 6));
        assert_eq!(run(&img(0x8000 | 6 << 7, &[1, 5000, 1200]), true), (NO_MANTISSA, 6));
        assert_eq!(run(&img(0x8000 | 2 << 7, &[1, 5000]), false), (NO_MANTISSA, -1));
    }

    /// D299: `add_sum` past |2^126| is the sticky OVERFLOW code, never a wrapped value (nor one on the sentinel's hi word,
    /// read as NaN); NaN and an infinity win over it; a sum inside the range adds exactly.
    #[test]
    fn a_sum_past_the_range_is_overflow() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        use gravity_ir::builder::ProgramBuilder;
        let run = |a: i128, t: i128| -> i128 {
            let mut pb = ProgramBuilder::new();
            let core = crate::frame::env::declare(&mut pb);
            let f = pb.declare_func("t", &[Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64], &[Type::I64, Type::I64]);
            let mut b = pb.define(f);
            let p = b.params().to_vec();
            let x = crate::frame::stop::exits(&mut b);
            let env = Env::new(&mut b, core, crate::hir::Ir::none(), &[], x, rig.mcx());
            let (lo, hi) = add_sum(&mut b, &env, p[1], p[2], p[3], p[4]);
            b.ret(&[lo, hi]);
            for blk in [x.l_finish, x.l_stop_in, x.l_full, x.l_status, x.l_done, x.l_raise, x.l_cancel, x.l_exit] {
                b.switch_to(blk);
                let z = b.iconst(Type::I64, -1);
                b.ret(&[z, z]);
            }
            let prog = pb.finish();
            let mut ctx = vec![0u64; gravity_abi::ctx::ctx_size(1) / 8];
            let args = [ctx.as_mut_ptr() as u64, a as u64, (a >> 64) as u64, t as u64, (t >> 64) as u64];
            let got = gravity_interp::Interp::new(&prog).run(f, &args).expect("runs");
            (got[0] as u128 | (got[1] as u128) << 64) as i128
        };
        let sent = |c: i64| ((SENT_HI as i128) << 64) | c as u64 as i128;
        let (nan, pinf, ovf) = (sent(SP_NAN), sent(SP_PINF), sent(SP_OVERFLOW));
        let big = (1i128 << 126) - 1;
        assert_eq!(run(big - 5, 5), big);
        assert_eq!(run(big, 1), ovf);
        assert_eq!(run(-big, -2), ovf);
        assert_eq!(run(-(1i128 << 125), -(1i128 << 125)), -(1i128 << 126));
        assert_eq!(run(-(1i128 << 126), -(1i128 << 122)), ovf);
        assert_eq!(run(ovf, 12_345), ovf);
        assert_eq!(run(-7, ovf), ovf);
        assert_eq!(run(ovf, nan), nan);
        assert_eq!(run(pinf, ovf), pinf);
        assert_eq!(run(123, -124), -1);
    }

    /// A sum at scale 4 printed at the largest dscale summed: exact division, a sentinel kept, a dscale past the scale capped.
    #[test]
    fn sums_divide_down_to_the_dscale() {
        let v = |x: i128| (x as u64, (x >> 64) as u64);
        let (lo, hi) = v(-1_234_500);
        assert_eq!(down(lo, hi, 4, 2), (v(-12_345).0, v(-12_345).1, 2));
        let (lo, hi) = v(i128::MAX / 100_000 * 10_000);
        assert_eq!(down(lo, hi, 4, 0), (v(i128::MAX / 100_000).0, v(i128::MAX / 100_000).1, 0));
        assert_eq!(down(SP_NAN as u64, SENT_HI as u64, 4, 0), (SP_NAN as u64, SENT_HI as u64, 0));
        assert_eq!(down(7, 0, 4, 9), (7, 0, 4));
    }

    /// Reads the image in place when its pointer is odd (the mantissa `10 p`, the dscale 2), and declines otherwise.
    struct Short;

    static DECODES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    impl NumShortMech for Short {
        fn decode(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, ptr: Value, scale: i16, ds: bool) -> (Value, Value, Option<Value>) {
            assert_eq!((scale, ds), (2, true));
            DECODES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let ten = b.iconst(Type::I64, 10);
            let odd = b.int_bin(IntBinOp::And, ptr, env.one);
            (b.int_bin(IntBinOp::Mul, ptr, ten), b.icmp(IntCond::Ne, odd, env.zero), Some(b.iconst(Type::I64, 2)))
        }
    }

    /// L0-d: road num.short's mechanism reads a datum's image before the host call: a fitting image is its word and dscale
    /// (the host never runs), a NULL datum is (0, 0) without either.
    #[test]
    fn num_short_decodes_before_the_host() {
        let run = |p: i64, none: bool| {
            crate::frame::stop::tests::run1(&[], &mut |b, env| {
                let (p, none) = (b.iconst(Type::I64, p), b.bconst(none));
                let (m, d) = mantissa_at(b, env, (p, none), 2, true, Some(&Short));
                let k = b.iconst(Type::I64, 1000);
                let m = b.int_bin(IntBinOp::Mul, m, k);
                b.int_bin(IntBinOp::Add, m, d)
            })
        };
        assert_eq!(run(5, false), 50_002);
        assert_eq!(run(5, true), 0);
        assert_eq!(DECODES.load(std::sync::atomic::Ordering::Relaxed), 2, "decoded once per lowering, never at run time");
    }
}
