//! The vector ops through the x86-64 SSE4.2 lowering (backend.md §6.10) —
//! the cases `ops_vector.rs` does not reach, on both architectures: every
//! shift count at every width (the byte and 64-bit forms are emulated on
//! x86-64), the accumulator loop with vector phis (the PCOUNT window of
//! §6.9.3: four accumulators, a compare-subtract per row, the combine and
//! the reduce), the 16-byte spill path under register pressure, the
//! movmask / extract / splat forms on float lanes, and the two-address
//! aliasing corners
//! (`x op x`, a result reusing an operand).

mod common;
use gravity_interp::Trap;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::*;

const INT_VECS: [Type; 4] = [Type::V16I8, Type::V8I16, Type::V4I32, Type::V2I64];

fn vec_bin(ty: Type, mk: impl Fn(&mut FunctionBuilder, Value, Value) -> Value) -> Program {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::Ptr, Type::Ptr, Type::Ptr], &[]);
    let mut b = pb.define(f);
    let [p, q, out]: [Value; 3] = b.params().try_into().unwrap();
    let a = b.vload_unaligned(ty, p, 0);
    let c = b.vload_unaligned(ty, q, 0);
    let r = mk(&mut b, a, c);
    b.vstore_unaligned(out, 0, r);
    b.ret(&[]);
    pb.finish()
}

fn lanes(ty: Type) -> Vec<Vec<u8>> {
    let w = ty.lane().bytes() as usize;
    let n = ty.lanes() as usize;
    let pats: [i64; 10] = [0, 1, -1, 2, i64::MIN, i64::MAX, 0x7f, -0x80, 0x5a5a_5a5a_5a5a_5a5a, -0x1234_5678_9abc_def1];
    let mut out = Vec::new();
    for k in 0..8 {
        let mut v = vec![0u8; 16];
        for i in 0..n {
            let x = pats[(i * 3 + k) % pats.len()];
            v[i * w..(i + 1) * w].copy_from_slice(&x.to_le_bytes()[..w]);
        }
        out.push(v);
    }
    out
}

fn lane_get(ty: Type, v: &[u8], i: usize) -> i64 {
    let w = ty.lane().bytes() as usize;
    let mut b = [0u8; 8];
    b[..w].copy_from_slice(&v[i * w..(i + 1) * w]);
    let sh = 64 - 8 * w as u32;
    ((u64::from_le_bytes(b) << sh) as i64) >> sh
}

fn stored_both(p: &Program, args_mk: impl Fn(*mut u8) -> Vec<u64>) -> ([u8; 16], [u8; 16]) {
    let mut o1 = [0u8; 16];
    let mut o2 = [0u8; 16];
    gravity_interp::Interp::new(p).run(Func(0), &args_mk(o1.as_mut_ptr())).unwrap();
    common::native(p, Func(0), &args_mk(o2.as_mut_ptr()));
    (o1, o2)
}

fn run(p: &Program, args: &[u64]) -> Result<Vec<u64>, Trap> {
    common::both(p, Func(0), args)
}

/// Every shift op at every count below the lane width, lane by lane against the interpreter and a
/// scalar reference.
#[test]
fn vector_shifts_at_every_count() {
    for ty in INT_VECS {
        let w = ty.lane().bytes() as u32 * 8;
        for k in 0..w as u8 {
            for (name, op) in [("vshl", VecShiftOp::Shl), ("vushr", VecShiftOp::Ushr), ("vsshr", VecShiftOp::Sshr)] {
                let p = vec_bin(ty, |b, x, _y| b.vshift(op, x, k));
                for a in lanes(ty) {
                    let (o1, o2) = stored_both(&p, |out| vec![a.as_ptr() as u64, a.as_ptr() as u64, out as u64]);
                    assert_eq!(o1, o2, "{name} {ty:?} by {k} a={a:?}");
                    for i in 0..ty.lanes() as usize {
                        let x = lane_get(ty, &a, i);
                        let m = if w == 64 { u64::MAX } else { (1u64 << w) - 1 };
                        let want = match op {
                            VecShiftOp::Shl => ((x as u64) << k) & m,
                            VecShiftOp::Ushr => ((x as u64) & m) >> k,
                            VecShiftOp::Sshr => (x >> k) as u64 & m,
                        };
                        let want = ((want << (64 - w)) as i64) >> (64 - w);
                        assert_eq!(lane_get(ty, &o2, i), want, "{name} {ty:?} by {k} lane {i} of a={a:?}");
                    }
                }
            }
        }
    }
}

/// `f(p, n, thr, out) -> i64`: the PCOUNT window (backend.md §6.9.3) at `ty`'s width — four
/// accumulators as vector phis, per row `acc -= (row < splat thr)`, the accumulators combined
/// after the loop and reduced; `n` rows of `ty.lanes()` lanes per vector, 4 vectors per iteration.
fn count_loop(ty: Type) -> Program {
    let lane = ty.lane();
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::Ptr, Type::I64, Type::I64, Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let [p, n, thr, _out]: [Value; 4] = b.params().try_into().unwrap();
    let t = if lane == Type::I64 { thr } else { b.conv(ConvOp::Trunc, lane, thr) };
    let ts = b.vsplat(ty, t);
    let z = b.iconst(lane, 0);
    let zs = b.vsplat(ty, z);
    let head = b.create_block(&[Type::I64, ty, ty, ty, ty]);
    let body = b.create_block(&[]);
    let exit = b.create_block(&[Type::I64, ty, ty, ty, ty]);
    let i0 = b.iconst(Type::I64, 0);
    b.br(head, &[i0, zs, zs, zs, zs]);
    b.switch_to(head);
    b.mark_loop_header(head);
    let [i, a0, a1, a2, a3]: [Value; 5] = b.block_params(head).try_into().unwrap();
    let step = b.iconst(Type::I64, 4 * ty.lanes() as i64);
    let next = b.int_bin(IntBinOp::Add, i, step);
    let more = b.icmp(IntCond::Ule, next, n);
    b.cond_br(more, (body, &[]), (exit, &[i, a0, a1, a2, a3]));
    b.switch_to(body);
    let base = b.ptr_add(p, i, lane.bytes() as u8, 0);
    let mut accs = [a0, a1, a2, a3];
    for (k, acc) in accs.iter_mut().enumerate() {
        let v = b.vload_unaligned(ty, base, 16 * k as i32);
        let m = b.vcmp(IntCond::Slt, v, ts);
        *acc = b.vbin(VecBinOp::Sub, *acc, m);
    }
    b.br(head, &[next, accs[0], accs[1], accs[2], accs[3]]);
    b.switch_to(exit);
    let [_, e0, e1, e2, e3]: [Value; 5] = b.block_params(exit).try_into().unwrap();
    let s = b.vbin(VecBinOp::Add, e0, e1);
    let s = b.vbin(VecBinOp::Add, s, e2);
    let s = b.vbin(VecBinOp::Add, s, e3);
    let r = b.vreduce_add(s);
    b.ret(&[r]);
    pb.finish()
}

#[test]
fn accumulator_loop_with_vector_phis_counts_like_the_scalar_fold() {
    for ty in INT_VECS {
        let p = count_loop(ty);
        let w = ty.lane().bytes() as usize;
        let per = 4 * ty.lanes() as usize;
        for rows in [0usize, per, 2 * per, 5 * per, 5 * per + 3] {
            // rows as the lane's canonical values: a spread around the threshold, incl. the extremes
            let vals: Vec<i64> = (0..rows).map(|i| [i as i64 - 7, 3, -128, 127, i64::MIN >> (64 - 8 * w as u32), i64::MAX >> (64 - 8 * w as u32), 0, -1][i % 8]).collect();
            let mut buf = vec![0u8; rows * w + 64];
            for (i, v) in vals.iter().enumerate() {
                buf[i * w..(i + 1) * w].copy_from_slice(&v.to_le_bytes()[..w]);
            }
            for thr in [0i64, 1, -3, 100, -100] {
                let mut out = [0u8; 16];
                let got = run(&p, &[buf.as_ptr() as u64, rows as u64, thr as u64, out.as_mut_ptr() as u64]).unwrap()[0] as i64;
                let counted = rows / per * per;
                let mut want = 0i64;
                for v in &vals[..counted] {
                    let x = ((*v as u64) << (64 - 8 * w as u32)) as i64 >> (64 - 8 * w as u32);
                    want += (x < thr) as i64;
                }
                assert_eq!(got, want, "count loop {ty:?} rows={rows} thr={thr}");
            }
        }
    }
}

/// Twenty live vectors (more than the sixteen XMM registers) combined after every one is loaded:
/// the 16-byte spill and reload path, lane by lane against the interpreter.
#[test]
fn vector_register_pressure_spills_sixteen_bytes() {
    const N: usize = 20;
    for ty in [Type::V4I32, Type::V2I64, Type::V16I8] {
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("f", &[Type::Ptr, Type::Ptr, Type::Ptr], &[]);
        let mut b = pb.define(f);
        let [p, q, out]: [Value; 3] = b.params().try_into().unwrap();
        let vs: Vec<Value> = (0..N).map(|i| b.vload_unaligned(ty, p, 16 * i as i32)).collect();
        let qv = b.vload_unaligned(ty, q, 0);
        // every vector combined in reverse (so the first-loaded are the longest-lived)
        let mut acc = qv;
        for v in vs.iter().rev() {
            let m = b.vcmp(IntCond::Sgt, *v, qv);
            let x = b.vselect(m, *v, acc);
            acc = b.vbin(VecBinOp::Add, acc, x);
        }
        for (i, v) in vs.iter().enumerate() {
            let x = b.vbin(VecBinOp::Xor, *v, acc);
            b.vstore_unaligned(out, 16 * i as i32, x);
        }
        b.ret(&[]);
        let p = pb.finish();
        let mut buf = vec![0u8; 16 * N];
        for (i, x) in buf.iter_mut().enumerate() {
            *x = (i as u32).wrapping_mul(2654435761) as u8;
        }
        let q = lanes(ty).remove(2);
        let mut o1 = vec![0u8; 16 * N];
        let mut o2 = vec![0u8; 16 * N];
        gravity_interp::Interp::new(&p).run(Func(0), &[buf.as_ptr() as u64, q.as_ptr() as u64, o1.as_mut_ptr() as u64]).unwrap();
        common::native(&p, Func(0), &[buf.as_ptr() as u64, q.as_ptr() as u64, o2.as_mut_ptr() as u64]);
        assert_eq!(o1, o2, "spill {ty:?}");
        assert!(o1.iter().any(|&x| x != 0));
    }
}

/// An aliasing `x op x` and a result reusing its operand, at every width.
#[test]
fn vector_aliasing_forms() {
    for ty in INT_VECS {
        for (name, mk) in [
            ("x-x", Box::new(|b: &mut FunctionBuilder, x, _y| b.vbin(VecBinOp::Sub, x, x)) as Box<dyn Fn(&mut FunctionBuilder, Value, Value) -> Value>),
            ("x<x", Box::new(|b: &mut FunctionBuilder, x, _y| b.vcmp(IntCond::Ult, x, x))),
            ("x>=x", Box::new(|b: &mut FunctionBuilder, x, _y| b.vcmp(IntCond::Sge, x, x))),
            ("umax x x", Box::new(|b: &mut FunctionBuilder, x, _y| b.vbin(VecBinOp::Umax, x, x))),
            ("sel x x x", Box::new(|b: &mut FunctionBuilder, x, _y| b.vselect(x, x, x))),
            ("chain", Box::new(|b: &mut FunctionBuilder, x, y| {
                let s = b.vbin(VecBinOp::Sub, x, y);
                let t = b.vbin(VecBinOp::Sub, s, y);
                let u = b.vcmp(IntCond::Ugt, t, x);
                b.vbin(VecBinOp::Smax, u, s)
            })),
        ] {
            let mut pb = ProgramBuilder::new();
            let f = pb.declare_func("f", &[Type::Ptr, Type::Ptr, Type::Ptr], &[]);
            let mut b = pb.define(f);
            let [p, q, out]: [Value; 3] = b.params().try_into().unwrap();
            let a = b.vload_unaligned(ty, p, 0);
            let c = b.vload_unaligned(ty, q, 0);
            let r = mk(&mut b, a, c);
            b.vstore_unaligned(out, 0, r);
            b.ret(&[]);
            let p = pb.finish();
            for a in lanes(ty) {
                for c in lanes(ty) {
                    let (o1, o2) = stored_both(&p, |out| vec![a.as_ptr() as u64, c.as_ptr() as u64, out as u64]);
                    assert_eq!(o1, o2, "{name} {ty:?} a={a:?} c={c:?}");
                }
            }
        }
    }
}

/// The float-lane forms: movmask, extract of every lane, splat, and the reductions' pairing.
#[test]
fn float_lane_movmask_extract_splat_and_reduce() {
    let fa = |ty: Type| -> Vec<[u8; 16]> {
        let mut out = Vec::new();
        if ty == Type::V2F64 {
            for pair in [[1.5f64, -2.25], [-0.0, 0.0], [f64::MAX, -f64::MIN_POSITIVE], [1e300, 1e-300], [-3.0, 1e16]] {
                let mut v = [0u8; 16];
                v[..8].copy_from_slice(&pair[0].to_le_bytes());
                v[8..].copy_from_slice(&pair[1].to_le_bytes());
                out.push(v);
            }
        } else {
            for quad in [[1.5f32, -2.25, 3.0, 1e-3], [-0.0, 0.0, -1.0, 1.0], [f32::MAX, -f32::MAX, 1e30, -1e-30], [1.0, 1e8, -1e8, 0.5]] {
                let mut v = [0u8; 16];
                for (i, f) in quad.iter().enumerate() {
                    v[i * 4..i * 4 + 4].copy_from_slice(&f.to_le_bytes());
                }
                out.push(v);
            }
        }
        out
    };
    for ty in [Type::V2F64, Type::V4F32] {
        let w = ty.lane().bytes() as usize;
        // movmask: one bit per lane, the sign
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("f", &[Type::Ptr], &[Type::I64]);
        let mut b = pb.define(f);
        let p = b.params()[0];
        let a = b.vload_unaligned(ty, p, 0);
        let m = b.vmovmask(a);
        b.ret(&[m]);
        let mm = pb.finish();
        for a in fa(ty) {
            let got = run(&mm, &[a.as_ptr() as u64]).unwrap()[0];
            let want: u64 = (0..ty.lanes() as usize).map(|i| ((a[i * w + w - 1] >> 7) as u64) << i).sum();
            assert_eq!(got, want, "vmovmask {ty:?} {a:?}");
        }
        // extract every lane, returned as its bits; splat of that lane stored
        for i in 0..ty.lanes() as u8 {
            let mut pb = ProgramBuilder::new();
            let f = pb.declare_func("f", &[Type::Ptr, Type::Ptr], &[Type::I64]);
            let mut b = pb.define(f);
            let [p, out]: [Value; 2] = b.params().try_into().unwrap();
            let a = b.vload_unaligned(ty, p, 0);
            let x = b.vextract(a, i);
            let s = b.vsplat(ty, x);
            b.vstore_unaligned(out, 0, s);
            let r = if ty == Type::V2F64 {
                b.conv(ConvOp::Bitcast, Type::I64, x)
            } else {
                let t = b.conv(ConvOp::Bitcast, Type::I32, x);
                b.conv(ConvOp::Zext, Type::I64, t)
            };
            b.ret(&[r]);
            let p = pb.finish();
            for a in fa(ty) {
                let mut o = [0u8; 16];
                let got = run(&p, &[a.as_ptr() as u64, o.as_mut_ptr() as u64]).unwrap()[0];
                let mut want = [0u8; 8];
                want[..w].copy_from_slice(&a[i as usize * w..(i as usize + 1) * w]);
                assert_eq!(got, u64::from_le_bytes(want), "vextract {ty:?} lane {i}");
                let (o1, o2) = stored_both(&p, |out| vec![a.as_ptr() as u64, out as u64]);
                assert_eq!(o1, o2, "vsplat {ty:?} lane {i}");
                for j in 0..ty.lanes() as usize {
                    assert_eq!(&o2[j * w..(j + 1) * w], &a[i as usize * w..(i as usize + 1) * w], "vsplat {ty:?} lane {i} -> {j}");
                }
            }
        }
        // the reduction's pairing: (l0 + l1) + (l2 + l3) / l0 + l1, against the scalar sum in that order
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("f", &[Type::Ptr], &[Type::I64]);
        let mut b = pb.define(f);
        let p = b.params()[0];
        let a = b.vload_unaligned(ty, p, 0);
        let r = b.vreduce_add(a);
        let r = if ty == Type::V2F64 {
            b.conv(ConvOp::Bitcast, Type::I64, r)
        } else {
            let t = b.conv(ConvOp::Bitcast, Type::I32, r);
            b.conv(ConvOp::Zext, Type::I64, t)
        };
        b.ret(&[r]);
        let red = pb.finish();
        for a in fa(ty) {
            let got = run(&red, &[a.as_ptr() as u64]).unwrap()[0];
            if ty == Type::V2F64 {
                let l = |i: usize| f64::from_le_bytes(a[i * 8..i * 8 + 8].try_into().unwrap());
                assert_eq!(f64::from_bits(got).to_bits(), (l(0) + l(1)).to_bits(), "vreduce_add v2f64 {a:?}");
            } else {
                let l = |i: usize| f32::from_le_bytes(a[i * 4..i * 4 + 4].try_into().unwrap());
                assert_eq!(f32::from_bits(got as u32).to_bits(), ((l(0) + l(1)) + (l(2) + l(3))).to_bits(), "vreduce_add v4f32 {a:?}");
            }
        }
    }
}

/// The integer reductions and movmask over the wider lane set (the sums leave the lane width).
#[test]
fn integer_reductions_widen_before_summing() {
    for ty in INT_VECS {
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("f", &[Type::Ptr], &[Type::I64, Type::I64]);
        let mut b = pb.define(f);
        let p = b.params()[0];
        let a = b.vload_unaligned(ty, p, 0);
        let s = b.vreduce_add(a);
        let m = b.vmovmask(a);
        b.ret(&[s, m]);
        let prog = pb.finish();
        for a in lanes(ty) {
            let got = run(&prog, &[a.as_ptr() as u64]).unwrap();
            let want: i64 = (0..ty.lanes() as usize).map(|i| lane_get(ty, &a, i)).fold(0i64, |s, x| s.wrapping_add(x));
            let want_m: u64 = (0..ty.lanes() as usize).map(|i| ((lane_get(ty, &a, i) < 0) as u64) << i).sum();
            assert_eq!(got[0] as i64, want, "vreduce_add {ty:?} {a:?}");
            assert_eq!(got[1], want_m, "vmovmask {ty:?} {a:?}");
        }
    }
}
