//! `Fold::run`'s loop for one layout, its constants inline. Every merge equals `Fold::fold`'s: a table folds byte for byte
//! as the counted loops fold it. No sub-split filter (kernels `owner_fold` sends a sub-split to the loops).
use gravity_abi::breaker::{Candidates, FOLD_OVERFLOW, FOLD_PF, MAX_RECORD_KEYS, MOVING};
use core::mem::offset_of;
use gravity_expr::emit::hash::{emit_hash_mix, emit_home_at, emit_key_hash_with, Bits};
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{ConvOp, FloatBinOp, Func, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_kernels::contract::entry3;
use gravity_kernels::table::fold_jit::{FoldCall, SlotHost};
use gravity_kernels::table::owner_fold::{Fold, Op, SIGN};

/// `(ptr FoldCall) -> i64`, `FoldFn`'s value.
///
/// Perf (baseline code generator): the loops carry the record pointer and the hash multiplier as block parameters (a
/// 64-bit constant is rematerialized at each use); no branch in a record's block has both edges carrying its values (each
/// such value is spilled on every record's path).
pub fn emit(pb: &mut ProgramBuilder, f: &Fold) -> Option<Func> {
    let (rec, sent, nw, _) = f.shape();
    if rec % 8 != 0 || nw == 0 || nw > MAX_RECORD_KEYS || FOLD_PF * rec > i32::MAX as usize {
        return None;
    }
    let host = pb.declare_host("owner_fold_slot", &[Type::Ptr, Type::I64, Type::Ptr, Type::Ptr], Some(Type::I64), entry3::<SlotHost> as *const () as usize, HostFlags { touches_memory: true, worker: true, ..HostFlags::default() });
    let func = pb.declare_func("owner_fold", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(func);
    let ctx = b.params()[0];
    let at = |o: usize| o as i32;
    let base = b.load(Type::Ptr, ctx, at(offset_of!(FoldCall, base)));
    let mask = b.load(Type::I64, ctx, at(offset_of!(FoldCall, mask)));
    let used = b.load(Type::I64, ctx, at(offset_of!(FoldCall, used)));
    let shr = b.load(Type::I64, ctx, at(offset_of!(FoldCall, shr)));
    let len0 = b.load(Type::I64, ctx, at(offset_of!(FoldCall, len)));
    let recs = b.load(Type::Ptr, ctx, at(offset_of!(FoldCall, recs)));
    let nrecs = b.load(Type::I64, ctx, at(offset_of!(FoldCall, nrecs)));
    let cand = b.load(Type::Ptr, ctx, at(offset_of!(FoldCall, cand)));
    let coff = b.load(Type::I64, cand, at(offset_of!(Candidates, off)));
    let cthr = b.load(Type::I64, cand, at(offset_of!(Candidates, thr)));
    let recs_at = b.conv(ConvOp::PtrToInt, Type::I64, recs);
    let rec_k = b.iconst(Type::I64, rec as i64);
    let n_rec = b.int_bin(IntBinOp::Mul, nrecs, rec_k);
    let end_at = b.int_bin(IntBinOp::Add, recs_at, n_rec);
    let last = b.int_bin(IntBinOp::Sub, end_at, rec_k);
    let one = b.iconst(Type::I64, 1);
    let zero = b.iconst(Type::I64, 0);
    let rec_c = b.iconst(Type::I64, rec as i64);
    let mix0 = emit_hash_mix(&mut b);
    let words = |b: &mut FunctionBuilder<'_>, p: Value| {
        let mut w = [zero; MAX_RECORD_KEYS];
        for (k, v) in w.iter_mut().enumerate().take(nw) {
            *v = b.load(Type::I64, p, (8 * k) as i32);
        }
        w
    };
    let slot = |b: &mut FunctionBuilder<'_>, j: Value| {
        let o = if rec.is_power_of_two() {
            let s = b.iconst(Type::I64, rec.trailing_zeros() as i64);
            b.int_bin(IntBinOp::Shl, j, s)
        } else {
            let r = b.iconst(Type::I64, rec as i64);
            b.int_bin(IntBinOp::Mul, j, r)
        };
        b.ptr_add(base, o, 1, 0)
    };
    // a one-slot table's shift is 64, taken modulo: its mask is 0
    let home = |b: &mut FunctionBuilder<'_>, h: Value| {
        let y = emit_home_at(b, h, shr, Bits::Dyn(used));
        b.int_bin(IntBinOp::And, y, mask)
    };
    let pf = b.iconst(Type::I64, FOLD_PF as i64);
    let pre_n = b.int_bin(IntBinOp::Umin, nrecs, pf);
    let pro = b.create_block(&[Type::I64, Type::Ptr, Type::I64]);
    let pro_body = b.create_block(&[]);
    let head = b.create_block(&[Type::I64, Type::I64, Type::I64, Type::Ptr, Type::I64]);
    b.br(pro, &[zero, recs, mix0]);
    b.switch_to(pro);
    b.mark_loop_header(pro);
    let [pi, pp, pmix]: [Value; 3] = b.block_params(pro).try_into().ok()?;
    let more = b.icmp(IntCond::Ult, pi, pre_n);
    b.cond_br(more, (pro_body, &[]), (head, &[zero, zero, len0, recs, pmix]));
    b.switch_to(pro_body);
    let kw = words(&mut b, pp);
    let h = emit_key_hash_with(&mut b, &kw[..nw], pmix);
    let rp = b.ptr_add(ctx, pi, 8, at(offset_of!(FoldCall, ring)));
    b.store(rp, 0, h);
    let j = home(&mut b, h);
    let d = slot(&mut b, j);
    b.prefetch(d, 0);
    let pi1 = b.int_bin(IntBinOp::Add, pi, one);
    let pp1 = b.ptr_add(pp, rec_c, 1, 0);
    b.br(pro, &[pi1, pp1, pmix]);
    b.switch_to(head);
    b.mark_loop_header(head);
    let [i, folded, len, src, mix]: [Value; 5] = b.block_params(head).try_into().ok()?;
    let body = b.create_block(&[]);
    let exit = b.create_block(&[]);
    let live = b.icmp(IntCond::Ult, i, nrecs);
    b.set_likely(head, true);
    b.cond_br(live, (body, &[]), (exit, &[]));
    b.switch_to(exit);
    b.store(ctx, at(offset_of!(FoldCall, len)), len);
    b.ret(&[folded]);
    b.switch_to(body);
    let pmask = b.iconst(Type::I64, FOLD_PF as i64 - 1);
    let ri = b.int_bin(IntBinOp::And, i, pmask);
    let rp = b.ptr_add(ctx, ri, 8, at(offset_of!(FoldCall, ring)));
    let h = b.load(Type::I64, rp, 0);
    // clamped to the run's last record: no read past the run (the ring slot is never read again)
    let ahead_rec = b.ptr_add(src, zero, 1, (FOLD_PF * rec) as i32);
    let ahead_at = b.conv(ConvOp::PtrToInt, Type::I64, ahead_rec);
    let at_last = b.int_bin(IntBinOp::Umin, ahead_at, last);
    let p2 = b.conv(ConvOp::IntToPtr, Type::Ptr, at_last);
    let kw2 = words(&mut b, p2);
    let h2 = emit_key_hash_with(&mut b, &kw2[..nw], mix);
    b.store(rp, 0, h2);
    let j2 = home(&mut b, h2);
    let d2 = slot(&mut b, j2);
    b.prefetch(d2, 0);
    let i1 = b.int_bin(IntBinOp::Add, i, one);
    let sk = words(&mut b, src);
    let probe = b.create_block(&[Type::I64]);
    let j0 = home(&mut b, h);
    b.br(probe, &[j0]);
    b.switch_to(probe);
    b.mark_loop_header(probe);
    let j = b.block_params(probe)[0];
    let d = slot(&mut b, j);
    let sw = b.load(Type::I64, d, at(sent));
    let empty = b.icmp(IntCond::Eq, sw, zero);
    let ins = b.create_block(&[]);
    let cmp = b.create_block(&[]);
    b.cond_br(empty, (ins, &[]), (cmp, &[]));
    b.switch_to(cmp);
    let mut diff = zero;
    for (k, &s) in sk[..nw].iter().enumerate() {
        let w = b.load(Type::I64, d, 8 * k as i32);
        let x = b.int_bin(IntBinOp::Xor, w, s);
        diff = b.int_bin(IntBinOp::Or, diff, x);
    }
    let same = b.icmp(IntCond::Eq, diff, zero);
    let merge = b.create_block(&[]);
    let step = b.create_block(&[]);
    b.cond_br(same, (merge, &[]), (step, &[]));
    b.switch_to(step);
    let jn = b.int_bin(IntBinOp::Add, j, one);
    let jm = b.int_bin(IntBinOp::And, jn, mask);
    b.br(probe, &[jm]);
    let note = b.create_block(&[Type::I64]);
    b.switch_to(ins);
    let full_at = b.load(Type::I64, ctx, at(offset_of!(FoldCall, full_at)));
    let full = b.icmp(IntCond::Uge, len, full_at);
    let filled = b.create_block(&[]);
    let copy = b.create_block(&[]);
    b.cond_br(full, (filled, &[]), (copy, &[]));
    b.switch_to(filled);
    b.mark_cold(filled);
    b.store(ctx, at(offset_of!(FoldCall, len)), len);
    let m1 = b.iconst(Type::I64, -1);
    let r = b.int_bin(IntBinOp::Sub, m1, folded);
    b.ret(&[r]);
    b.switch_to(copy);
    for o in (0..rec / 16).map(|k| 16 * k as i32) {
        let v = b.vload_unaligned(Type::V2I64, src, o);
        b.vstore_unaligned(d, o, v);
    }
    if rec % 16 != 0 {
        let o = (rec - 8) as i32;
        let w = b.load(Type::I64, src, o);
        b.store(d, o, w);
    }
    let len1 = b.int_bin(IntBinOp::Add, len, one);
    b.br(note, &[len1]);
    b.switch_to(merge);
    let refuse = b.create_block(&[]);
    Merges { ctx, d, src, valid: f.valid(), host, refuse }.emit(&mut b, f);
    b.br(note, &[len]);
    b.switch_to(refuse);
    b.mark_cold(refuse);
    b.store(ctx, at(offset_of!(FoldCall, len)), len);
    let ovf = b.iconst(Type::I64, FOLD_OVERFLOW);
    b.ret(&[ovf]);
    b.switch_to(note);
    let len2 = b.block_params(note)[0];
    let folded1 = b.int_bin(IntBinOp::Add, folded, one);
    let next = b.create_block(&[]);
    let try_note = b.create_block(&[]);
    let ov = b.load(Type::I8, cand, at(offset_of!(Candidates, overflow)));
    let z8 = b.iconst(Type::I8, 0);
    let open = b.icmp(IntCond::Eq, ov, z8);
    b.cond_br(open, (try_note, &[]), (next, &[]));
    b.switch_to(try_note);
    let cw = b.ptr_add(d, coff, 1, 0);
    let c = b.load(Type::I64, cw, 0);
    let moving = b.iconst(Type::I64, MOVING as i64);
    let mv = b.int_bin(IntBinOp::And, c, moving);
    let still = b.icmp(IntCond::Eq, mv, zero);
    let reached = b.icmp(IntCond::Sge, c, cthr);
    let both = b.int_bin(IntBinOp::And, still, reached);
    let list = b.create_block(&[]);
    b.cond_br(both, (list, &[]), (next, &[]));
    b.switch_to(list);
    let cl = b.load(Type::I64, cand, at(offset_of!(Candidates, len)));
    let cap = b.load(Type::I64, cand, at(offset_of!(Candidates, cap)));
    let at_cap = b.icmp(IntCond::Eq, cl, cap);
    let over = b.create_block(&[]);
    let push = b.create_block(&[]);
    b.cond_br(at_cap, (over, &[]), (push, &[]));
    b.switch_to(over);
    let t8 = b.iconst(Type::I8, 1);
    b.store(cand, at(offset_of!(Candidates, overflow)), t8);
    b.br(next, &[]);
    b.switch_to(push);
    let cm = b.int_bin(IntBinOp::Or, c, moving);
    b.store(cw, 0, cm);
    let cptr = b.load(Type::Ptr, cand, at(offset_of!(Candidates, ptr)));
    let slot_at = b.ptr_add(cptr, cl, 4, 0);
    let j32 = b.conv(ConvOp::Trunc, Type::I32, j);
    b.store(slot_at, 0, j32);
    let cl1 = b.int_bin(IntBinOp::Add, cl, one);
    b.store(cand, at(offset_of!(Candidates, len)), cl1);
    b.br(next, &[]);
    b.switch_to(next);
    let src1 = b.ptr_add(src, rec_c, 1, 0);
    b.br(head, &[i1, folded1, len2, src1, mix]);
    drop(b);
    Some(func)
}

/// `Fold::fold`'s order: the validity word is ORed after every merge read it.
struct Merges {
    ctx: Value,
    d: Value,
    src: Value,
    valid: Option<usize>,
    host: gravity_ir::HostFn,
    refuse: gravity_ir::Block,
}

impl Merges {
    fn emit(&self, b: &mut FunctionBuilder<'_>, f: &Fold) {
        let (d, src) = (self.d, self.src);
        let vw = self.valid.map(|vo| (b.load(Type::I64, d, vo as i32), b.load(Type::I64, src, vo as i32)));
        let one = b.iconst(Type::I64, 1);
        let zero = b.iconst(Type::I64, 0);
        let bit = |b: &mut FunctionBuilder<'_>, v: Value, j: u32| {
            let s = b.iconst(Type::I64, j as i64);
            let x = b.int_bin(IntBinOp::Ushr, v, s);
            b.int_bin(IntBinOp::And, x, one)
        };
        let smask = |b: &mut FunctionBuilder<'_>, o: &Op| {
            vw.map(|(_, sv)| {
                let x = bit(b, sv, o.j);
                b.int_bin(IntBinOp::Sub, zero, x)
            })
        };
        let word = |b: &mut FunctionBuilder<'_>, p: Value, o: &Op, k: i32| b.load(Type::I64, p, o.off as i32 + k);
        for o in f.adds() {
            let mut s = word(b, src, o, 0);
            if o.k != !0 {
                let k = b.iconst(Type::I64, o.k as i64);
                s = b.int_bin(IntBinOp::And, s, k);
            }
            if let Some(m) = smask(b, o) {
                s = b.int_bin(IntBinOp::And, s, m);
            }
            let dv = word(b, d, o, 0);
            let r = b.int_bin(IntBinOp::Add, dv, s);
            b.store(d, o.off as i32, r);
        }
        for o in f.wides() {
            let (mut s0, mut s1) = (word(b, src, o, 0), word(b, src, o, 8));
            if let Some(m) = smask(b, o) {
                s0 = b.int_bin(IntBinOp::And, s0, m);
                s1 = b.int_bin(IntBinOp::And, s1, m);
            }
            let (d0, d1) = (word(b, d, o, 0), word(b, d, o, 8));
            let lo = b.int_bin(IntBinOp::Add, d0, s0);
            let hi = b.add_hi(d0, s0, d1, s1);
            b.store(d, o.off as i32, lo);
            b.store(d, o.off as i32 + 8, hi);
        }
        for o in f.extremes() {
            let (s, dv) = (word(b, src, o, 0), word(b, d, o, 0));
            let op = match o.k {
                0 => Some(IntBinOp::Umin),
                k if k == !0 => Some(IntBinOp::Umax),
                k if k == SIGN => Some(IntBinOp::Smin),
                k if k == !SIGN => Some(IntBinOp::Smax),
                _ => None,
            };
            if let (None, Some(op)) = (vw, op) {
                let r = b.int_bin(op, s, dv);
                b.store(d, o.off as i32, r);
                continue;
            }
            let (sk, dk) = if o.k == 0 {
                (s, dv)
            } else {
                let k = b.iconst(Type::I64, o.k as i64);
                (b.int_bin(IntBinOp::Xor, s, k), b.int_bin(IntBinOp::Xor, dv, k))
            };
            let mut take = b.icmp(IntCond::Ult, sk, dk);
            if let Some((dw, sw)) = vw {
                let db = bit(b, dw, o.j);
                let dnone = b.icmp(IntCond::Eq, db, zero);
                take = b.int_bin(IntBinOp::Or, take, dnone);
                let sb = bit(b, sw, o.j);
                let shas = b.icmp(IntCond::Ne, sb, zero);
                take = b.int_bin(IntBinOp::And, take, shas);
            }
            let r = b.select(take, s, dv);
            b.store(d, o.off as i32, r);
        }
        if let Some(o) = f.implicit() {
            let s = word(b, src, &o, 0);
            let nm = b.iconst(Type::I64, !MOVING as i64);
            let c = b.int_bin(IntBinOp::And, s, nm);
            let c1 = b.int_bin(IntBinOp::Sub, c, one);
            let mut v = b.int_bin(IntBinOp::Smax, c1, zero);
            if let Some(m) = smask(b, &o) {
                v = b.int_bin(IntBinOp::And, v, m);
            }
            let dv = word(b, d, &o, 0);
            let r = b.int_bin(IntBinOp::Add, dv, v);
            b.store(d, o.off as i32, r);
        }
        for o in f.rest() {
            let done = b.create_block(&[]);
            if let Some((_, sw)) = vw {
                let sb = bit(b, sw, o.j);
                let shas = b.icmp(IntCond::Ne, sb, zero);
                let go = b.create_block(&[]);
                b.cond_br(shas, (go, &[]), (done, &[]));
                b.switch_to(go);
            }
            if o.k == 0 {
                let j = b.iconst(Type::I64, o.j as i64);
                let ok = b.call_host(self.host, &[self.ctx, j, d, src], Some(Type::I64)).expect("the slot host's word");
                let yes = b.icmp(IntCond::Ne, ok, zero);
                b.cond_br(yes, (done, &[]), (self.refuse, &[]));
            } else {
                self.float_sum(b, o, vw.map(|v| v.0), done);
            }
            b.switch_to(done);
        }
        if let Some((dw, sw)) = vw {
            let r = b.int_bin(IntBinOp::Or, dw, sw);
            b.store(d, self.valid.unwrap_or_default() as i32, r);
        }
    }

    /// `float8_pl`: an infinite sum of finite operands refuses (`FOLD_OVERFLOW`).
    fn float_sum(&self, b: &mut FunctionBuilder<'_>, o: &Op, dvalid: Option<Value>, done: gravity_ir::Block) {
        let (d, src) = (self.d, self.src);
        let s = b.load(Type::I64, src, o.off as i32);
        if let Some(dw) = dvalid {
            let one = b.iconst(Type::I64, 1);
            let sh = b.iconst(Type::I64, o.j as i64);
            let x = b.int_bin(IntBinOp::Ushr, dw, sh);
            let db = b.int_bin(IntBinOp::And, x, one);
            let zero = b.iconst(Type::I64, 0);
            let dnone = b.icmp(IntCond::Eq, db, zero);
            let put = b.create_block(&[]);
            let add = b.create_block(&[]);
            b.cond_br(dnone, (put, &[]), (add, &[]));
            b.switch_to(put);
            b.store(d, o.off as i32, s);
            b.br(done, &[]);
            b.switch_to(add);
        }
        let dv = b.load(Type::I64, d, o.off as i32);
        let fd = b.conv(ConvOp::Bitcast, Type::F64, dv);
        let fs = b.conv(ConvOp::Bitcast, Type::F64, s);
        let fr = b.float_bin(FloatBinOp::Add, fd, fs);
        let r = b.conv(ConvOp::Bitcast, Type::I64, fr);
        // infinite: past the sign bit, the all-ones exponent over a zero fraction
        let one = b.iconst(Type::I64, 1);
        let inf = b.iconst(Type::I64, (0x7ff0_0000_0000_0000u64 << 1) as i64);
        let is_inf = |b: &mut FunctionBuilder<'_>, v: Value| {
            let x = b.int_bin(IntBinOp::Shl, v, one);
            b.icmp(IntCond::Eq, x, inf)
        };
        let (ri, di, si) = (is_inf(b, r), is_inf(b, dv), is_inf(b, s));
        let fin = b.int_bin(IntBinOp::Or, di, si);
        let t = b.bconst(true);
        let both_fin = b.int_bin(IntBinOp::Xor, fin, t);
        let over = b.int_bin(IntBinOp::And, ri, both_fin);
        let keep = b.create_block(&[]);
        b.cond_br(over, (self.refuse, &[]), (keep, &[]));
        b.switch_to(keep);
        b.store(d, o.off as i32, r);
        b.br(done, &[]);
    }
}
