//! Direct-indexed table from observed min / max, road `join.dense` (H23a, R5-L02: a one-word key over a
//! narrow observed range). Budget 160 lines. Owner M1-JOIN.
//!
//! A one-word key image whose observed range spans at most the cost home's span for the records (rt.rs `dense_span_max`:
//! the chain heads within reach of the KeyHash directory) is indexed by `key - lo` (the range the build measured, never a
//! declared one): `DENSE` when the keys prove unique at lay-out (an occupancy bitmap, and each record's
//! link and payload in its key's slot), `DCHAIN` when a duplicate shows up or the slots do not fit (u32 chain heads by
//! key over the records, the chains as the KeyHash form's). A probe key outside `[lo, hi]` misses without a load.
use core::sync::atomic::Ordering::Relaxed;
use core::sync::atomic::{AtomicU64, AtomicU8};

use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};

use crate::frame::env::Env;

use gravity_driver::State;
use mcx::GrowIn;

use crate::ops::hashjoin::build::{ALLOW, A_DENSE, A_MULT, DBITS, DCHAIN, DENSE, DIR, FLAGS, FULL, F_COMPACT, F_DUP, F_RANGE, HDR, HI, LO, LOW, N, PHASE, SLOTS};
use crate::ops::hashjoin::rt::{grow_to, rd, wr, Rt};
use crate::ops::hashjoin::build_side::Shape;
use crate::ops::hashjoin::probe::{rec_at, Hv};

mod raw;
pub(crate) use raw::{compact_out, lay, put, span_of};

/// The road's value: the dense forms are laid (`lay`) and probed (`first`) by this file's functions.
pub(crate) struct Dense;

crate::strategies! {
    /// Direct-indexed table from observed min / max.
    JOIN_DENSE = 50, "join.dense", Join;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(crate::face::strategy::JOIN_DENSE, &Dense)], ..crate::ops::NONE };

/// The occupancy bitmap's bytes over `span` keys (the slots follow it, a cache line up).
pub(crate) fn bitmap(span: usize) -> usize {
    (span.div_ceil(64) * 8).next_multiple_of(64)
}

/// A single-row hit (V4R-156) as values: `p` non-zero when the key is present, its `n` payload words (`I64`), then the
/// Fp128 words of the record's live code columns (join.payload_fp, `Shape::fp_live`: `[lo, hi]` each), and its NULL bits.
pub(crate) struct Hit<'e> {
    pub p: Value,
    pub pay: mcx::PgVec<'e, Value>,
    pub null: mcx::PgVec<'e, Value>,
}

/// V4R-27: key `k`'s one record in a build laid `DBITS` or `DENSE` (the probe's single-row body), branch-free within a
/// form: the bits themselves under `DBITS`, the slot's words under `DENSE` (a key out of range reads slot 0 and is absent).
pub(crate) fn first_unique<'e>(b: &mut FunctionBuilder<'_>, env: &Env<'e, '_>, hv: &Hv, k: Value, sh: &Shape, n: usize) -> Hit<'e> {
    let f = sh.fp_n();
    let l_f = b.create_block(&mcx::pg_vec![in env.mcx; Type::I64; 1 + 2 * n + 2 * f][..]);
    if crate::ops::hashjoin::strategies::dense_bits::may(sh) {
        let (db, l_b, l_d) = (b.iconst(Type::I64, DBITS as i64), b.create_block(&[]), b.create_block(&[]));
        let is_b = b.icmp(IntCond::Eq, hv.form, db);
        b.cond_br(is_b, (l_b, &[]), (l_d, &[]));
        b.switch_to(l_b);
        let [set, v, nul] = crate::ops::hashjoin::strategies::dense_bits::hit(b, env, hv, k, sh);
        // a compact record's build carries no Fp128 words (build.rs `live`: `f` is 0 where this form runs)
        b.br(l_f, &mcx::concat_in(&[&mcx::pg_vec![in env.mcx; set][..], &mcx::pg_vec![in env.mcx; v; n + 2 * f][..], &mcx::pg_vec![in env.mcx; nul; n][..]], env.mcx)[..]);
        b.switch_to(l_d);
    }
    let off = b.int_bin(IntBinOp::Sub, k, hv.lo);
    let inr = b.icmp(IntCond::Ult, off, hv.span);
    let (o, six, sixty3) = (b.select(inr, off, env.zero), b.iconst(Type::I64, 6), b.iconst(Type::I64, 63));
    let (wi, bi) = (b.int_bin(IntBinOp::Ushr, o, six), b.int_bin(IntBinOp::And, o, sixty3));
    let wp = b.ptr_add(hv.dir, wi, 8, 0);
    let w = b.load(Type::I64, wp, 0);
    let (x, i1) = (b.int_bin(IntBinOp::Ushr, w, bi), b.conv(ConvOp::Zext, Type::I64, inr));
    let e = b.int_bin(IntBinOp::And, x, env.one);
    let set = b.int_bin(IntBinOp::And, e, i1);
    let srec = b.iconst(Type::I64, sh.rec.get() as i64 - 8 * sh.words.get() as i64);
    let so = b.int_bin(IntBinOp::Mul, o, srec);
    let view = b.ptr_add(hv.slots, so, 1, -8 * sh.words.get() as i32);
    let link = b.load(Type::I64, view, sh.link_off());
    let mut ws = mcx::pg_vec![in env.mcx; set];
    ws.extend_in((0..n).map(|i| sh.pay_off(i)).chain((0..2 * f).map(|h| sh.fp_off(h / 2) + 8 * (h % 2) as i32)).map(|o| b.load(Type::I64, view, o)));
    for i in 0..n {
        let s = b.iconst(Type::I64, 32 + i as i64);
        let x = b.int_bin(IntBinOp::Ushr, link, s);
        ws.push_in(b.int_bin(IntBinOp::And, x, env.one));
    }
    b.br(l_f, &ws[..]);
    b.switch_to(l_f);
    let p = b.block_params(l_f);
    Hit { p: p[0], pay: mcx::to_vec_in(&p[1..1 + n + 2 * f], env.mcx), null: mcx::to_vec_in(&p[1 + n + 2 * f..], env.mcx) }
}

/// The first record of one-word key `k` under `DENSE` (its slot's record view: the slot less the image words, so the
/// link and payload offsets are the records') or `DCHAIN` (the chain's head): 0 when none.
pub(crate) fn first(b: &mut FunctionBuilder<'_>, env: &Env, hv: &Hv, k: Value, words: i64, rec: u32) -> Value {
    let off = b.int_bin(IntBinOp::Sub, k, hv.lo);
    let inr = b.icmp(IntCond::Ult, off, hv.span);
    let (l_in, l_out) = (b.create_block(&[]), b.create_block(&[Type::I64]));
    b.cond_br(inr, (l_in, &[]), (l_out, &[env.zero]));
    b.switch_to(l_in);
    let dense = b.iconst(Type::I64, DENSE as i64);
    let is_dense = b.icmp(IntCond::Eq, hv.form, dense);
    let (l_d, l_c) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(is_dense, (l_d, &[]), (l_c, &[]));
    b.switch_to(l_d);
    let (six, sixty3, one) = (b.iconst(Type::I64, 6), b.iconst(Type::I64, 63), env.one);
    let wi = b.int_bin(IntBinOp::Ushr, off, six);
    let wp = b.ptr_add(hv.dir, wi, 8, 0);
    let w = b.load(Type::I64, wp, 0);
    let bi = b.int_bin(IntBinOp::And, off, sixty3);
    let sh = b.int_bin(IntBinOp::Ushr, w, bi);
    let bit = b.int_bin(IntBinOp::And, sh, one);
    let set = b.icmp(IntCond::Ne, bit, env.zero);
    let srec = b.iconst(Type::I64, rec as i64 - 8 * words);
    let so = b.int_bin(IntBinOp::Mul, off, srec);
    let si = b.conv(ConvOp::PtrToInt, Type::I64, hv.slots);
    let sp = b.int_bin(IntBinOp::Add, si, so);
    let img = b.iconst(Type::I64, 8 * words);
    let view = b.int_bin(IntBinOp::Sub, sp, img);
    let p = b.select(set, view, env.zero);
    b.br(l_out, &[p]);
    b.switch_to(l_c);
    let hp = b.ptr_add(hv.dir, off, 4, 0);
    let h1 = b.load_zext(Type::I32, hp, 0);
    let rp = rec_at(b, env, hv, h1, rec);
    let hz = b.icmp(IntCond::Eq, h1, env.zero);
    let p = b.select(hz, env.zero, rp);
    b.br(l_out, &[p]);
    b.switch_to(l_out);
    b.block_params(l_out)[0]
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use core::cell::{Cell as VCell, RefCell};

    use gravity_interp::Interp;
    use gravity_ir::builder::ProgramBuilder;
    use gravity_ir::{ConvOp, IntBinOp, IntCond, Type};

    use crate::ops::hashjoin::build::{ALLOW, A_DENSE, BITS, DBITS, DENSE, DIR, FLAGS, FORM, FULL, F_COMPACT, F_RANGE, HDR, HI, LO, N, PHASE, REC, SLOTS, TOP, VBITS, WORDS};
    use crate::ops::hashjoin::rt::{rd, wr, Rt};
    use crate::ops::hashjoin::build_side::Shape;
    use crate::ops::hashjoin::probe::Hv;
    use crate::frame::{env, env::Env, stop};

    /// Key `k`'s payload word under `vbits`: a bool's (key odd), else a byte's word, sign-extended for an even key and
    /// zero-extended for an odd one (JOIN-02: a source may extend either way).
    fn val(k: i64, vbits: u8) -> u64 {
        let byte = (k * 37) as u8;
        match (vbits, k & 1) {
            (1, b) => b as u64,
            (_, 0) => byte as i8 as i64 as u64,
            _ => byte as u64,
        }
    }

    /// A compact region of `keys` (value `val`, NULL: key % 5 == 0), room for its bitmaps, its expansion and its slots.
    fn region(keys: &[i64], vbits: u8) -> Vec<u64> {
        let mut v = vec![0u64; (HDR + 24 * keys.len() + 16384) / 8];
        let d = v.as_mut_ptr() as *mut u8;
        // SAFETY: offsets inside `v`.
        unsafe {
            (wr::<u64>(d, N, keys.len() as u64), wr::<i64>(d, LO, *keys.iter().min().unwrap()), wr::<i64>(d, HI, *keys.iter().max().unwrap()));
            (wr::<u8>(d, FLAGS, F_RANGE | F_COMPACT), wr::<u8>(d, ALLOW, A_DENSE), crate::ops::hashjoin::layout::set_pay(d, 1), wr::<u8>(d, WORDS, 1), wr::<u8>(d, PHASE, 1));
            (wr::<u32>(d, REC, 8), wr::<u32>(d, FULL, 24));
            (wr::<u64>(d, TOP, 8 * v.len() as u64), wr::<u8>(d, VBITS, vbits));
            let mask = if vbits == 1 { 1 } else { u16::MAX as u64 };
            for (i, &k) in keys.iter().enumerate() {
                wr::<u64>(d.add(HDR + 8 * i), 0, (k as u64 & u32::MAX as u64) | (val(k, vbits) & mask) << 32 | ((k % 5 == 0) as u64) << (32 + vbits));
            }
        }
        v
    }

    /// V4R-27: `first_unique` finds each key's one record, its payload and NULL bit, under both unique forms, and misses
    /// an absent key and one outside the range; JOIN-02: a bool's value bit and a one-byte word's 16 bits alike.
    #[test]
    fn first_unique_reads_the_one_record_under_dbits_and_dense() {
        for vbits in [1, crate::ops::hashjoin::strategies::dense_bits::WIDE] {
            first_unique_under(vbits);
        }
    }

    fn first_unique_under(vbits: u8) {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let keys: Vec<i64> = (-40..160).filter(|k| k % 3 != 0).collect();
        let sh = Shape {
            slot: 0, build: 0, probe: None, payload: vec![1], allow: A_DENSE, words: VCell::new(1), rec: VCell::new(24),
            range: VCell::new(true), vbits: VCell::new(vbits), kept: RefCell::new(Vec::new()), rel: VCell::new([0; crate::ops::hashjoin::layout::REL_WORDS]), mult: VCell::new(false), dedup: VCell::new(0),
            swapped: false, hoist: VCell::new(None), form: VCell::new(None), valued: VCell::new([0; crate::ops::hashjoin::layout::REL_WORDS]),
            bits: VCell::new(None), fp: crate::ops::hashjoin::strategies::payload_fp::FpCols::NONE, fp_live: Default::default(),
        };
        let mut pb = ProgramBuilder::new();
        let core = env::declare(&mut pb);
        let f = pb.declare_func("fu", &[Type::Ptr, Type::Ptr, Type::I64], &[Type::I64, Type::I64, Type::I64]);
        {
            let mut b = pb.define(f);
            let x = stop::exits(&mut b);
            let env = Env::new(&mut b, core, crate::hir::Ir::none(), &[], x, rig.mcx());
            let (r, k) = (b.params()[1], b.params()[2]);
            let at = |b: &mut gravity_ir::builder::FunctionBuilder<'_>, o: usize| {
                let off = b.load(Type::I64, r, o as i32);
                b.ptr_add(r, off, 1, 0)
            };
            let (form, dir, slots) = (b.load_zext(Type::I8, r, FORM as i32), at(&mut b, DIR), at(&mut b, SLOTS));
            let (bits, lo, hi) = (b.load_zext(Type::I32, r, BITS as i32), b.load(Type::I64, r, LO as i32), b.load(Type::I64, r, HI as i32));
            let d = b.int_bin(IntBinOp::Sub, hi, lo);
            let span = b.int_bin(IntBinOp::Add, d, env.one);
            let (ri, hdr) = (b.conv(ConvOp::PtrToInt, Type::I64, r), b.iconst(Type::I64, HDR as i64));
            let (recs, n) = (b.int_bin(IntBinOp::Add, ri, hdr), b.load(Type::I64, r, N as i32));
            let planes = Some(crate::ops::hashjoin::strategies::dense_bits::planes(&mut b, dir, slots, &sh));
            let hv = Hv { form, dir, bits, lo, span, slots, recs, base: ri, n, planes };
            let h = super::first_unique(&mut b, &env, &hv, k, &sh, 1);
            let (l_hit, l_miss) = (b.create_block(&[]), b.create_block(&[]));
            let hit = b.icmp(IntCond::Ne, h.p, env.zero);
            b.cond_br(hit, (l_hit, &[]), (l_miss, &[]));
            b.switch_to(l_hit);
            let (val, null) = (h.pay[0], h.null[0]);
            b.ret(&[env.one, val, null]);
            b.switch_to(l_miss);
            b.ret(&[env.zero, env.zero, env.zero]);
            for l in [x.l_finish, x.l_stop_in, x.l_full, x.l_status, x.l_done, x.l_raise, x.l_cancel, x.l_exit] {
                b.switch_to(l);
                b.ret(&[env.zero, env.zero, env.zero]);
            }
        }
        let p = pb.finish();
        let mut ctx = vec![0u64; gravity_abi::ctx::ctx_size(1) / 8];
        for want in [DBITS, DENSE] {
            let mut v = region(&keys, vbits);
            let d = v.as_mut_ptr() as *mut u8;
            // SAFETY: `v` holds the header, the records and room for either form past them.
            unsafe {
                if want == DBITS {
                    let t = Rt::of(d, v.len() * 8);
                    assert!(crate::ops::hashjoin::strategies::dense_bits::need(d) > 0 && crate::ops::hashjoin::strategies::dense_bits::lay(&t));
                } else {
                    crate::ops::hashjoin::strategies::dense_bits::expand(d);
                    let (form, _) = super::lay(&Rt::of(d, v.len() * 8)).unwrap().expect("a dense form");
                    wr::<u8>(d, FORM, form);
                }
                assert_eq!(rd::<u8>(d, FORM), want);
            }
            for k in -45..165 {
                let got = Interp::new(&p).run(f, &[ctx.as_mut_ptr() as u64, d as u64, k as u64]).expect("runs");
                let on = (-40..160).contains(&k) && k % 3 != 0;
                let exp = if on { vec![1, val(k, vbits), (k % 5 == 0) as u64] } else { vec![0, 0, 0] };
                assert_eq!(got, exp, "vbits {vbits}, form {want}, key {k}");
            }
        }
    }
}
