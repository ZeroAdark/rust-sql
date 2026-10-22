//! A numeric result's image for num.arith (rep/varlena/num_arith.rs, M2-INVENTORY I48): PostgreSQL's canonical short form
//! (numeric.c make_result) of a mantissa word open-coded into a per-site slot, and the shim's arithmetic into the same
//! slot for what the word form declines (cold). Budget 80 lines. Owner M2-REP. Registers nothing.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, IntUnOp, Type, Value};

use crate::frame::env::Env;

/// The canonical image of `m × 10^-scale` (make_result's: short form, dscale `ds` or `scale`, zero groups stripped) at `slot + 8`
/// with a 4-byte header: `|m| × 10^pad` split into five base-10000 groups g4..g0, stored most significant first so the
/// top nonzero group lands at `slot + 14`; the headers are written over the zero groups above it.
pub(crate) fn image(b: &mut FunctionBuilder<'_>, env: &Env, m: Value, scale: i16, ds: Option<Value>, slot: Value) -> Value {
    let (c, bin) = (|b: &mut FunctionBuilder<'_>, v: i64| b.iconst(Type::I64, v), |b: &mut FunctionBuilder<'_>, op, x, y| b.int_bin(op, x, y));
    let pad = (4 - scale % 4) % 4;
    let (neg, nm) = (b.icmp(IntCond::Slt, m, env.zero), b.int_un(IntUnOp::Neg, m));
    let a = b.select(neg, nm, m);
    let x = if pad == 0 { a } else { let p = c(b, 10i64.pow(pad as u32)); bin(b, IntBinOp::Mul, a, p) };
    // the five groups by reciprocal multiplies, two levels deep instead of a chain of four divisions (x < 10^19), each
    // smulh by ceil(2^89 / 10^8): hi = x / 10^8 as smulh(x >> 8) >> 17 (the pre-shift took 8 of the 25), g4 = hi / 10^8 as
    // smulh(hi) >> 25 (x >> 8 < 2^56 and hi < 10^11 keep both exact); each 8-digit half / 10^4 as n * ceil(2^40 / 10^4)
    // >> 40 (exact below 10^8); a remainder is n - q d
    let q = |b: &mut FunctionBuilder<'_>, n: Value, s: i64| {
        let (big, m) = (s < 40, if s < 40 { 6_189_700_196_426_901_375 } else { 109_951_163 });
        let (m, s) = (c(b, m), c(b, s));
        let h = bin(b, if big { IntBinOp::Smulh } else { IntBinOp::Mul }, n, m);
        bin(b, IntBinOp::Ushr, h, s)
    };
    let rem = |b: &mut FunctionBuilder<'_>, n: Value, q: Value, d: i64| { let d = c(b, d); let t = bin(b, IntBinOp::Mul, q, d); bin(b, IntBinOp::Sub, n, t) };
    let x8 = { let s = c(b, 8); bin(b, IntBinOp::Ushr, x, s) };
    let hi = q(b, x8, 17);
    let (lo, g4) = (rem(b, x, hi, 100_000_000), q(b, hi, 25));
    let mid = rem(b, hi, g4, 100_000_000);
    let (g3, g1) = (q(b, mid, 40), q(b, lo, 40));
    let (g2, g0) = (rem(b, mid, g3, 10_000), rem(b, lo, g1, 10_000));
    let (g, mut mask) = ([g0, g1, g2, g3, g4], env.zero);
    for (i, &gi) in g.iter().enumerate() {
        let nz = { let nz = b.icmp(IntCond::Ne, gi, env.zero); b.conv(ConvOp::Zext, Type::I64, nz) };
        mask = { let sh = c(b, i as i64); let bit = bin(b, IntBinOp::Shl, nz, sh); bin(b, IntBinOp::Or, mask, bit) };
    }
    let zero = b.icmp(IntCond::Eq, mask, env.zero);
    let (c63, four, five) = (c(b, 63), c(b, 4), c(b, 5));
    let top = { let z = b.int_un(IntUnOp::Clz, mask); let t = bin(b, IntBinOp::Sub, c63, z); b.select(zero, four, t) };
    let low = { let t = b.int_un(IntUnOp::Ctz, mask); b.select(zero, five, t) };
    let n = { let d = bin(b, IntBinOp::Sub, top, low); bin(b, IntBinOp::Add, d, env.one) };
    // groups g4..g1 as one little-endian word, g0 after it, at slot + 14 - 2 (4 - top)
    let w = (1..5).fold(env.zero, |w, i| { let sh = c(b, 16 * (4 - i as i64)); let x = bin(b, IntBinOp::Shl, g[i], sh); bin(b, IntBinOp::Or, w, x) });
    let base = { let t2 = bin(b, IntBinOp::Add, top, top); let o = { let k = c(b, 6); bin(b, IntBinOp::Add, t2, k) }; b.ptr_add(slot, o, 1, 0) };
    b.store_unaligned(base, 0, w);
    let g0 = b.conv(ConvOp::Trunc, Type::I16, g[0]);
    b.store_unaligned(base, 8, g0);
    let len = { let n2 = bin(b, IntBinOp::Add, n, n); let six = c(b, 6); let l = bin(b, IntBinOp::Add, n2, six); let two = c(b, 2); bin(b, IntBinOp::Shl, l, two) };
    let weight = { let f = c(b, ((scale + pad) / 4) as i64); let w = bin(b, IntBinOp::Sub, top, f); let w = b.select(zero, env.zero, w); let m7 = c(b, 0x7F); bin(b, IntBinOp::And, w, m7) };
    let sign = { let tr = b.bconst(true); let nz = b.int_bin(IntBinOp::Xor, zero, tr); let s = b.int_bin(IntBinOp::And, neg, nz); let s = b.conv(ConvOp::Zext, Type::I64, s); let k = c(b, 13); bin(b, IntBinOp::Shl, s, k) };
    let h = { let k = c(b, 0x8000 | if ds.is_some() { 0 } else { (scale as i64) << 7 }); let h = bin(b, IntBinOp::Or, k, sign); bin(b, IntBinOp::Or, h, weight) };
    let h = match ds { Some(d) => { let s7 = c(b, 7); let d = bin(b, IntBinOp::Shl, d, s7); bin(b, IntBinOp::Or, h, d) } None => h };
    let (len, h) = (b.conv(ConvOp::Trunc, Type::I32, len), b.conv(ConvOp::Trunc, Type::I16, h));
    (b.store(slot, 8, len), b.store(slot, 12, h));
    let p = b.ptr_add(slot, env.zero, 1, 8);
    b.conv(ConvOp::PtrToInt, Type::I64, p)
}

pub(crate) use super::raw::num_image::num_arith_cold;

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use gravity_ir::HostFlags;

    use super::*;
    use crate::frame::stop::tests::run1;

    /// make_result's image of `m × 10^-scale` (agg_final::num_sum) as bytes past its 4-byte header.
    fn want(m: i64, scale: i16) -> Vec<u8> {
        let mut buf = vec![0u64; 8]; // alloc: Program
        // SAFETY: `buf` is 64 bytes, live and unaliased while `r` lives.
        let mut r = gravity_fn::Region::over(unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), 64) });
        let ret = gravity_fn::agg_final::num_sum(core::ptr::null_mut(), &mut r, m, m >> 63, scale as i64);
        assert_eq!(ret.status, gravity_fn::OK);
        // SAFETY: `ret.val` is the live image num_sum wrote into `buf`.
        unsafe { gravity_fn::with_payload(ret.val as usize as *const u8, |x| x.to_vec()) }.expect("payload")
    }

    /// Every group boundary and magnitude up to 10^19 after the pad (the reciprocal split's range), both signs, each
    /// pad: the image is make_result's byte for byte.
    #[test]
    fn image_splits_every_group() {
        // the image comes back through a pointer the test hands the program (`out`), not a thread-local
        unsafe extern "C" fn keep(p: u64, out: u64) -> i64 {
            // SAFETY: `p` is a live varlena; `out` is the test's `&mut Vec<u8>`, unaliased for the call.
            unsafe { *(out as usize as *mut Vec<u8>) = gravity_fn::with_payload(p as usize as *const u8, |x| x.to_vec()).expect("payload") };
            0
        }
        let mut out: Vec<u8> = Vec::new();
        let at = &mut out as *mut Vec<u8> as i64;
        let mut n = 0;
        for scale in 0..=7i16 {
            let lim = 10u64.pow(19 - ((4 - scale % 4) % 4) as u32);
            let mut ms: Vec<u64> = vec![0, 1, 9, 10, 9_999, 10_000, 10_001, 99_999_999, 100_000_000, 100_000_001, 10u64.pow(12), 10u64.pow(16) - 1, 10u64.pow(16), 10u64.pow(16) + 1];
            let mut x = 0x0139_408D_CBBF_7A44u64; // Marsaglia xorshift seed (not a hash constant)
            for _ in 0..200 {
                x ^= x << 13; x ^= x >> 7; x ^= x << 17;
                ms.push(x % lim);
            }
            ms.extend([lim - 1, lim / 10, lim / 10 - 1]);
            for &a in ms.iter().filter(|&&a| a < lim && a <= i64::MAX as u64) {
                for m in [a as i64, -(a as i64)] {
                    run1(&[], &mut |b, env| {
                        let slot = { let sl = b.declare_stack_slot(64, 8); b.stack_addr(sl) };
                        let mv = b.iconst(Type::I64, m);
                        let p = image(b, env, mv, scale, None, slot);
                        let o = b.iconst(Type::I64, at);
                        let h = b.declare_host_once("keep", &[Type::I64, Type::I64], Some(Type::I64), keep as *const () as usize, HostFlags { touches_memory: true, ..HostFlags::default() });
                        b.call_host(h, &[p, o], Some(Type::I64)).expect("returns")
                    });
                    assert_eq!(out, want(m, scale), "{m}e-{scale}");
                    n += 1;
                }
            }
        }
        assert!(n > 3000);
    }
}
