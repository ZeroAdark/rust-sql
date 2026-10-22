//! class.num_wide's words (V4R-38, B-lite: the user 09-24). Hooks / levers: -. Budget 180 lines (files-extra.tsv, +25 %
//! gate). Owner M3-FOLD (built by V4R38-WIDE).
//!
//! Split from class_num_wide.rs by responsibility: a numeric datum read exactly as an i256 term at the aggregate's scale
//! (`term`: numeric.c's short and long images, the specials codes, a digit below the scale unreadable by name, join.mult's
//! weight; `scaled` for a tree term), the specials codes' sum (`codes`), a slot's validity, and the finals through
//! gravity_fn's `num_sum256` / `num_avg256` with their errors kept by name. Registers nothing.
use gravity_abi::breaker::NumWide;
use gravity_expr::emit::CellValid;
use gravity_fn::agg_final::{wide_divrem, wide_mul_add, SENT_HI, SP_OVERFLOW};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, StateMask, Type, Value};
use mcx::{CollectIn, PgVec};

use crate::frame::env::Env;
pub(crate) use crate::ops::agg::numeric::raw::wide::{avg_final, scaled, sum_final, term};

/// The term's code past a readable image: a toasted datum, or digits below the aggregate's scale.
pub(crate) const UNREADABLE: i64 = -1;

/// Base-10000 `digits` at `weight` × 10^scale × `mult`, signed: the i256 words, or the code (`term`). The digits are read
/// where they lie (`term`: the image's own), never copied out.
pub(crate) fn mantissa(digits: impl ExactSizeIterator<Item = u64>, weight: i32, scale: i32, mult: u64, neg: bool) -> Result<NumWide, i64> {
    let (mut m, n) = ([0u64; 4], digits.len());
    for d in digits {
        wide_mul_add(&mut m, 10_000, d).then_some(()).ok_or(SP_OVERFLOW)?;
    }
    let mut e = 4 * (weight - n as i32 + 1) + scale;
    while e > 0 && m != [0; 4] {
        let k = e.min(19);
        wide_mul_add(&mut m, 10u64.pow(k as u32), 0).then_some(()).ok_or(SP_OVERFLOW)?;
        e -= k;
    }
    while e < 0 && m != [0; 4] {
        let k = (-e).min(19);
        if wide_divrem(&mut m, 10u64.pow(k as u32)) != 0 {
            return Err(UNREADABLE);
        }
        e += k;
    }
    if !wide_mul_add(&mut m, mult, 0) || m[3] >= 1 << 62 {
        return Err(SP_OVERFLOW);
    }
    Ok(if neg { neg256(m) } else { m })
}

gravity_kernels::contract::host! {
    #[cold]
    fn unreadable() -> i64 {
        gravity_fn::stash_error(Box::new(gravity_fn::PgError::error("gravity: a numeric term the wide fold cannot read (a toasted image, or digits below the aggregate's scale)"))); // alloc: Cold
        0
    }

    /// Two specials codes added as PostgreSQL adds them (abi's `num_add` over two sentinels).
    pub(crate) fn codes(x: i64, y: i64) -> i64 {
        let s = |c: i64| ((SENT_HI as i128) << 64) | c as u64 as i128;
        gravity_abi::breaker::num_add(s(x), s(y)) as u64 as i64
    }
}

pub(crate) fn neg256(m: NumWide) -> NumWide {
    let mut out = m;
    let mut c = 1u128;
    for w in out.iter_mut() {
        let v = (!*w) as u128 + c;
        (*w, c) = (v as u64, v >> 64);
    }
    out
}

/// A slot's validity as a `Bool` (fold/slots.rs's rule over abi's `Rec::valid`).
pub(crate) fn slot_ok(b: &mut FunctionBuilder<'_>, env: &Env, base: Value, v: CellValid) -> Value {
    match v {
        CellValid::Always => b.bconst(true),
        CellValid::NonZeroAt(o) => {
            let w = b.load(Type::I64, base, o as i32);
            b.icmp(IntCond::Ne, w, env.zero)
        }
        CellValid::Bit { off, bit } => {
            let byte = b.load_zext(Type::I8, base, off as i32);
            let m = b.iconst(Type::I64, 1 << bit);
            let x = b.int_bin(IntBinOp::And, byte, m);
            b.icmp(IntCond::Ne, x, env.zero)
        }
        CellValid::Opened(o) => {
            let w = b.load(Type::I64, base, o as i32);
            let m = b.const_raw(Type::I64, !gravity_abi::breaker::MOVING);
            let n = b.int_bin(IntBinOp::And, w, m);
            let two = b.iconst(Type::I64, 2);
            b.icmp(IntCond::Uge, n, two)
        }
    }
}

pub(crate) fn kept(r: gravity_fn::Ret, e: &mut gravity_fn::ErrSlot) -> u64 {
    if r.status == gravity_fn::OK {
        return r.val as u64;
    }
    if let Some(err) = e.take() {
        gravity_fn::stash_error(err);
    }
    0
}

/// A final through a gravity_fn `agg_final` host over `(region, args…)`, entered only where the state is not NULL, into a
/// per-call region the sink copies from; a failed render raises the host's error (fold/slots.rs's `host_final`).
pub(crate) fn host_final(b: &mut FunctionBuilder<'_>, env: &Env, name: &'static str, host: usize, args: &[Value], null: Value) -> Value {
    let flags = HostFlags { may_error: true, pure: false, touches_memory: true, worker: false, state: StateMask::NONE };
    let params: PgVec<'_, Type> = [Type::Ptr, Type::Ptr].into_iter().chain(args[1..].iter().map(|_| Type::I64)).collect_in(env.mcx);
    let h = b.declare_host_once(name, &params[..], Some(Type::I64), host, flags);
    let (rs, buf) = (b.declare_stack_slot(16, 8), b.declare_stack_slot(128, 8));
    let (l_call, l_join) = (b.create_block(&[]), b.create_block(&[Type::I64]));
    b.cond_br(null, (l_join, &[env.zero]), (l_call, &[]));
    b.switch_to(l_call);
    let (r, p) = (b.stack_addr(rs), b.stack_addr(buf));
    let end = b.ptr_add(p, env.zero, 1, 128);
    b.store(r, 0, p);
    b.store(r, 8, end);
    let all: PgVec<'_, Value> = core::iter::once(r).chain(args.iter().copied()).collect_in(env.mcx);
    let v = b.call_host(h, &all[..], Some(Type::I64)).unwrap();
    let failed = b.icmp(IntCond::Eq, v, env.zero);
    let l_ok = b.create_block(&[]);
    let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    b.cond_br(failed, (env.l_raise, &[e]), (l_ok, &[]));
    b.switch_to(l_ok);
    b.br(l_join, &[v]);
    b.switch_to(l_join);
    b.block_params(l_join)[0]
}

/// The term the fold cannot read raises by name, then `HOST`.
pub(crate) fn raise(b: &mut FunctionBuilder<'_>, env: &Env, bad: Value) {
    let (l_bad, l_ok) = (b.create_block(&[]), b.create_block(&[]));
    b.mark_cold(l_bad);
    b.cond_br(bad, (l_bad, &[]), (l_ok, &[]));
    b.switch_to(l_bad);
    let h = b.declare_host_once("class_num_wide::unreadable", &[], Some(Type::I64), unreadable as *const () as usize, HostFlags { may_error: true, ..HostFlags::default() });
    b.call_host(h, &[], Some(Type::I64));
    let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    b.br(env.l_raise, &[e]);
    b.switch_to(l_ok);
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use gravity_fn::agg_final::{SP_NAN, SP_NINF, SP_PINF};

    /// numeric.c's long image (4-byte varlena header, sign / dscale word, weight, base-10000 digits).
    fn long(neg: bool, dscale: u16, weight: i16, digits: &[u16]) -> Vec<u8> {
        let mut v = (((4 + 4 + 2 * digits.len()) as u32) << 2).to_le_bytes().to_vec();
        v.extend((if neg { 0x4000 } else { 0 } | dscale).to_le_bytes());
        v.extend(weight.to_le_bytes());
        digits.iter().for_each(|d| v.extend(d.to_le_bytes()));
        v
    }

    /// numeric.c's short image behind a 1-byte varlena header (an odd-aligned payload).
    fn short(neg: bool, dscale: u16, weight: i16, digits: &[u16]) -> Vec<u8> {
        let mut v = vec![0u8, (((1 + 2 + 2 * digits.len()) as u8) << 1) | 1];
        let h = 0x8000 | if neg { 0x2000 } else { 0 } | dscale << 7 | (weight as u16 & 0x7F);
        v.extend(h.to_le_bytes());
        digits.iter().for_each(|d| v.extend(d.to_le_bytes()));
        v
    }

    fn run(img: &[u8], at: usize, scale: i64, mult: i64) -> Result<i128, i64> {
        let mut out = [0u64; 4];
        // SAFETY: `img[at..]` is a numeric image; `out` four words.
        let code = unsafe { term(img[at..].as_ptr() as u64, scale, mult, out.as_mut_ptr() as u64) };
        let top = ((out[3] as u128) << 64 | out[2] as u128) as i128;
        match code {
            0 => {
                assert!(top == 0 || top == -1, "a test value within i128");
                Ok(((out[1] as u128) << 64 | out[0] as u128) as i128)
            }
            c => Err(c),
        }
    }

    /// A datum's term: exact at the scale (a smaller dscale widened), signed, times join.mult's weight; both headers; the
    /// specials their codes; a digit below the scale unreadable by name; past |2^254| the OVERFLOW code.
    #[test]
    fn a_datum_reads_as_an_exact_i256_term() {
        // 12345.678900 at scale 6: digits 1 2345 6789 at weight 1
        let v = long(false, 6, 1, &[1, 2345, 6789]);
        assert_eq!(run(&v, 0, 6, 1), Ok(12_345_678_900));
        assert_eq!(run(&v, 0, 8, 1), Ok(1_234_567_890_000));
        assert_eq!(run(&v, 0, 6, 3), Ok(37_037_036_700));
        assert_eq!(run(&v, 0, 3, 1), Err(UNREADABLE), "6789 has digits below 10^-3");
        assert_eq!(run(&long(true, 2, 0, &[7, 2500]), 0, 6, 1), Ok(-7_250_000));
        assert_eq!(run(&short(true, 2, 0, &[7, 2500]), 1, 6, 1), Ok(-7_250_000));
        assert_eq!(run(&short(false, 0, 0, &[]), 1, 6, 1), Ok(0));
        // a 55-digit product: 10^54 (digit 100 at weight 13) at scale 0 and 6
        let big = long(false, 0, 13, &[100]);
        let mut out = [0u64; 4];
        assert_eq!(unsafe { term(big.as_ptr() as u64, 6, 1, out.as_mut_ptr() as u64) }, 0);
        let mut m = [1u64, 0, 0, 0];
        (0..60).for_each(|_| assert!(wide_mul_add(&mut m, 10, 0)));
        assert_eq!(out, m, "10^60 exactly");
        // past 2^254 (about 2.9e76): 10^77 is OVERFLOW
        assert_eq!(unsafe { term(long(false, 0, 19, &[10]).as_ptr() as u64, 0, 1, out.as_mut_ptr() as u64) }, SP_OVERFLOW);
        for (h, code) in [(0xC000u16, SP_NAN), (0xD000, SP_PINF), (0xF000, SP_NINF)] {
            let mut s = 24u32.to_le_bytes().to_vec();
            s.extend(h.to_le_bytes());
            s.extend([0u8; 18]);
            assert_eq!(run(&s, 0, 6, 1), Err(code));
        }
        assert_eq!(run(&[1u8, 18, 0, 0], 0, 6, 1), Err(UNREADABLE), "a toast pointer");
    }

    /// Two specials codes add as PostgreSQL adds them (NaN wins, opposite infinities NaN, OVERFLOW loses to either).
    #[test]
    fn codes_add_as_pg() {
        assert_eq!(codes(SP_PINF, SP_NINF), SP_NAN);
        assert_eq!(codes(SP_OVERFLOW, SP_PINF), SP_PINF);
        assert_eq!(codes(SP_NINF, SP_NINF), SP_NINF);
        assert_eq!(codes(SP_NAN, SP_OVERFLOW), SP_NAN);
    }
}
