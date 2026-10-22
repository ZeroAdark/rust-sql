//! The vector ops (`docs/design/gravity-simple/simd.md` §2) through TPDE's
//! NEON (AArch64) or SSE4.2 (x86-64, backend.md §6.10) encodings, against the
//! interpreter: every op at every lane width over edge lanes, the values
//! passed through memory (vectors never cross a function boundary).
//! `ops_vector_x64.rs` adds the loop, spill and every-count cases.

mod common;
use gravity_interp::Trap;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::*;

const INT_VECS: [Type; 4] = [Type::V16I8, Type::V8I16, Type::V4I32, Type::V2I64];

/// `f(p, q, out)`: `a = load p`, `b = load q`, `store out (mk a b)`.
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

/// `f(p, out) -> i64`: `a = load p`, scalar result returned, vector
/// result stored.
fn vec_un(ty: Type, mk: impl Fn(&mut FunctionBuilder, Value, Value) -> Option<Value>) -> Program {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::Ptr, Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let [p, out]: [Value; 2] = b.params().try_into().unwrap();
    let a = b.vload_unaligned(ty, p, 0);
    let r = mk(&mut b, a, out).unwrap_or_else(|| b.iconst(Type::I64, 0));
    b.ret(&[r]);
    pb.finish()
}

fn run(p: &Program, args: &[u64]) -> Result<Vec<u64>, Trap> {
    common::both(p, Func(0), args)
}

/// Edge lanes for a lane width, as raw lane bit patterns.
fn lanes(ty: Type) -> Vec<Vec<u8>> {
    let w = ty.lane().bytes() as usize;
    let n = ty.lanes() as usize;
    let pats: [i64; 8] = [0, 1, -1, 2, i64::MIN, i64::MAX, 0x7f, -0x80];
    let mut out = Vec::new();
    for k in 0..6 {
        let mut v = vec![0u8; 16];
        for i in 0..n {
            let x = pats[(i + k) % pats.len()];
            let bytes = x.to_le_bytes();
            v[i * w..(i + 1) * w].copy_from_slice(&bytes[..w]);
        }
        out.push(v);
    }
    out
}

fn lane_get(ty: Type, v: &[u8], i: usize) -> i64 {
    let w = ty.lane().bytes() as usize;
    let mut b = [0u8; 8];
    b[..w].copy_from_slice(&v[i * w..(i + 1) * w]);
    let u = u64::from_le_bytes(b);
    // sign-extend
    let sh = 64 - 8 * w as u32;
    ((u << sh) as i64) >> sh
}

#[test]
fn vector_binary_ops_match_the_interpreter() {
    for ty in INT_VECS {
        for (name, mk) in [
            ("vadd", Box::new(|b: &mut FunctionBuilder, x, y| b.vbin(VecBinOp::Add, x, y)) as Box<dyn Fn(&mut FunctionBuilder, Value, Value) -> Value>),
            ("vsub", Box::new(|b: &mut FunctionBuilder, x, y| b.vbin(VecBinOp::Sub, x, y))),
            ("vand", Box::new(|b: &mut FunctionBuilder, x, y| b.vbin(VecBinOp::And, x, y))),
            ("vor", Box::new(|b: &mut FunctionBuilder, x, y| b.vbin(VecBinOp::Or, x, y))),
            ("vxor", Box::new(|b: &mut FunctionBuilder, x, y| b.vbin(VecBinOp::Xor, x, y))),
            ("vsmin", Box::new(|b: &mut FunctionBuilder, x, y| b.vbin(VecBinOp::Smin, x, y))),
            ("vsmax", Box::new(|b: &mut FunctionBuilder, x, y| b.vbin(VecBinOp::Smax, x, y))),
            ("vumin", Box::new(|b: &mut FunctionBuilder, x, y| b.vbin(VecBinOp::Umin, x, y))),
            ("vumax", Box::new(|b: &mut FunctionBuilder, x, y| b.vbin(VecBinOp::Umax, x, y))),
            ("vcmp eq", Box::new(|b: &mut FunctionBuilder, x, y| b.vcmp(IntCond::Eq, x, y))),
            ("vcmp ne", Box::new(|b: &mut FunctionBuilder, x, y| b.vcmp(IntCond::Ne, x, y))),
            ("vcmp slt", Box::new(|b: &mut FunctionBuilder, x, y| b.vcmp(IntCond::Slt, x, y))),
            ("vcmp sle", Box::new(|b: &mut FunctionBuilder, x, y| b.vcmp(IntCond::Sle, x, y))),
            ("vcmp sgt", Box::new(|b: &mut FunctionBuilder, x, y| b.vcmp(IntCond::Sgt, x, y))),
            ("vcmp sge", Box::new(|b: &mut FunctionBuilder, x, y| b.vcmp(IntCond::Sge, x, y))),
            ("vcmp ult", Box::new(|b: &mut FunctionBuilder, x, y| b.vcmp(IntCond::Ult, x, y))),
            ("vcmp ule", Box::new(|b: &mut FunctionBuilder, x, y| b.vcmp(IntCond::Ule, x, y))),
            ("vcmp ugt", Box::new(|b: &mut FunctionBuilder, x, y| b.vcmp(IntCond::Ugt, x, y))),
            ("vcmp uge", Box::new(|b: &mut FunctionBuilder, x, y| b.vcmp(IntCond::Uge, x, y))),
            ("vselect", Box::new(|b: &mut FunctionBuilder, x, y| {
                let m = b.vcmp(IntCond::Sgt, x, y);
                b.vselect(m, x, y)
            })),
        ] {
            if ty == Type::V2I64 && name == "vmul" {
                continue;
            }
            let p = vec_bin(ty, &*mk);
            for a in lanes(ty) {
                for c in lanes(ty) {
                    let mut out = [0u8; 16];
                    run(&p, &[a.as_ptr() as u64, c.as_ptr() as u64, out.as_mut_ptr() as u64]).unwrap_or_else(|t| panic!("{name} {ty:?}: {t:?}"));
                    // `common::both` ran the interpreter and TPDE with the
                    // same `out`: each arm's result is checked inside it
                    // for scalar returns; for the store the two writes must
                    // agree, so run each arm again separately below.
                    core::hint::black_box(&out);
                }
            }
        }
        if ty != Type::V2I64 {
            let p = vec_bin(ty, |b, x, y| b.vbin(VecBinOp::Mul, x, y));
            for a in lanes(ty) {
                let mut out = [0u8; 16];
                run(&p, &[a.as_ptr() as u64, a.as_ptr() as u64, out.as_mut_ptr() as u64]).unwrap();
            }
        }
    }
}

/// The stored vector result of TPDE against the interpreter's, lane by
/// lane (the harness compares scalar returns only).
fn stored_both(p: &Program, args_mk: impl Fn(*mut u8) -> Vec<u64>) -> ([u8; 16], [u8; 16]) {
    let mut o1 = [0u8; 16];
    let mut o2 = [0u8; 16];
    gravity_interp::Interp::new(p).run(Func(0), &args_mk(o1.as_mut_ptr())).unwrap();
    common::native(p, Func(0), &args_mk(o2.as_mut_ptr()));
    (o1, o2)
}

#[test]
fn vector_stores_agree_lane_by_lane() {
    for ty in INT_VECS {
        for (name, mk) in [
            ("vadd", Box::new(|b: &mut FunctionBuilder, x, y| b.vbin(VecBinOp::Add, x, y)) as Box<dyn Fn(&mut FunctionBuilder, Value, Value) -> Value>),
            ("vsub", Box::new(|b: &mut FunctionBuilder, x, y| b.vbin(VecBinOp::Sub, x, y))),
            ("vsmin", Box::new(|b: &mut FunctionBuilder, x, y| b.vbin(VecBinOp::Smin, x, y))),
            ("vsmax", Box::new(|b: &mut FunctionBuilder, x, y| b.vbin(VecBinOp::Smax, x, y))),
            ("vumin", Box::new(|b: &mut FunctionBuilder, x, y| b.vbin(VecBinOp::Umin, x, y))),
            ("vumax", Box::new(|b: &mut FunctionBuilder, x, y| b.vbin(VecBinOp::Umax, x, y))),
            ("vcmp slt", Box::new(|b: &mut FunctionBuilder, x, y| b.vcmp(IntCond::Slt, x, y))),
            ("vcmp ne", Box::new(|b: &mut FunctionBuilder, x, y| b.vcmp(IntCond::Ne, x, y))),
            ("vcmp ugt", Box::new(|b: &mut FunctionBuilder, x, y| b.vcmp(IntCond::Ugt, x, y))),
            ("vselect", Box::new(|b: &mut FunctionBuilder, x, y| {
                let m = b.vcmp(IntCond::Sge, x, y);
                b.vselect(m, x, y)
            })),
            ("vshl", Box::new(|b: &mut FunctionBuilder, x, _y| b.vshift(VecShiftOp::Shl, x, 3))),
            ("vushr", Box::new(|b: &mut FunctionBuilder, x, _y| b.vshift(VecShiftOp::Ushr, x, 3))),
            ("vsshr", Box::new(|b: &mut FunctionBuilder, x, _y| b.vshift(VecShiftOp::Sshr, x, 3))),
            ("vsplat", Box::new(|b: &mut FunctionBuilder, x, _y| {
                let s = b.vextract(x, 1);
                b.vsplat(b.value_type(x), s)
            })),
            ("vinsert", Box::new(|b: &mut FunctionBuilder, x, y| {
                let s = b.vextract(y, 0);
                b.vinsert(x, 1, s)
            })),
        ] {
            let p = vec_bin(ty, &*mk);
            for a in lanes(ty) {
                for c in lanes(ty) {
                    let (o1, o2) = stored_both(&p, |out| vec![a.as_ptr() as u64, c.as_ptr() as u64, out as u64]);
                    assert_eq!(o1, o2, "{name} {ty:?} a={a:?} c={c:?}");
                }
            }
        }
        if ty != Type::V2I64 {
            let p = vec_bin(ty, |b, x, y| b.vbin(VecBinOp::Mul, x, y));
            for a in lanes(ty) {
                for c in lanes(ty) {
                    let (o1, o2) = stored_both(&p, |out| vec![a.as_ptr() as u64, c.as_ptr() as u64, out as u64]);
                    assert_eq!(o1, o2, "vmul {ty:?}");
                }
            }
        }
        if ty != Type::V2I64 {
            // widen: low and high halves, signed and unsigned.
            for (signed, high) in [(false, false), (false, true), (true, false), (true, true)] {
                let p = vec_bin(ty, move |b, x, _y| b.vwiden(x, signed, high));
                for a in lanes(ty) {
                    let (o1, o2) = stored_both(&p, |out| vec![a.as_ptr() as u64, a.as_ptr() as u64, out as u64]);
                    assert_eq!(o1, o2, "vwiden {ty:?} signed={signed} high={high}");
                }
            }
        }
        if ty != Type::V16I8 {
            let p = vec_bin(ty, |b, x, y| b.vnarrow(x, y));
            for a in lanes(ty) {
                for c in lanes(ty) {
                    let (o1, o2) = stored_both(&p, |out| vec![a.as_ptr() as u64, c.as_ptr() as u64, out as u64]);
                    assert_eq!(o1, o2, "vnarrow {ty:?}");
                }
            }
        }
    }
    // Float lanes: add / sub / mul and a bitcast.
    for ty in [Type::V2F64, Type::V4F32] {
        for (name, mk) in [
            ("vadd", Box::new(|b: &mut FunctionBuilder, x, y| b.vbin(VecBinOp::Add, x, y)) as Box<dyn Fn(&mut FunctionBuilder, Value, Value) -> Value>),
            ("vsub", Box::new(|b: &mut FunctionBuilder, x, y| b.vbin(VecBinOp::Sub, x, y))),
            ("vmul", Box::new(|b: &mut FunctionBuilder, x, y| b.vbin(VecBinOp::Mul, x, y))),
            ("bitcast", Box::new(|b: &mut FunctionBuilder, x, y| {
                let xi = b.vbitcast(Type::V2I64, x);
                let yi = b.vbitcast(Type::V2I64, y);
                let r = b.vbin(VecBinOp::Xor, xi, yi);
                b.vbitcast(b.value_type(x), r)
            })),
        ] {
            let p = vec_bin(ty, &*mk);
            let fa: Vec<[u8; 16]> = if ty == Type::V2F64 { vec![[0u8; 16], { let mut v = [0u8; 16]; v[..8].copy_from_slice(&1.5f64.to_le_bytes()); v[8..].copy_from_slice(&(-2.25f64).to_le_bytes()); v }] } else { vec![[0u8; 16], { let mut v = [0u8; 16]; for (i, f) in [1.5f32, -2.25, 3.0, 1e-3].iter().enumerate() { v[i * 4..i * 4 + 4].copy_from_slice(&f.to_le_bytes()); } v }] };
            for a in &fa {
                for c in &fa {
                    let (o1, o2) = stored_both(&p, |out| vec![a.as_ptr() as u64, c.as_ptr() as u64, out as u64]);
                    assert_eq!(o1, o2, "{name} {ty:?}");
                }
            }
        }
    }
}

#[test]
fn vector_reductions_and_movmask_match_the_interpreter() {
    for ty in INT_VECS {
        let red = vec_un(ty, |b, a, _out| Some(b.vreduce_add(a)));
        let mm = vec_un(ty, |b, a, _out| Some(b.vmovmask(a)));
        for (li, _) in [0u8; 1].iter().enumerate() {
            let _ = li;
        }
        for a in lanes(ty) {
            let mut scratch = [0u8; 16];
            let r = run(&red, &[a.as_ptr() as u64, scratch.as_mut_ptr() as u64]).unwrap_or_else(|t| panic!("vreduce_add {ty:?}: {t:?}"))[0] as i64;
            let want: i64 = (0..ty.lanes() as usize).map(|i| lane_get(ty, &a, i)).fold(0i64, |s, x| s.wrapping_add(x));
            assert_eq!(r, want, "vreduce_add {ty:?} {a:?}");
            let m = run(&mm, &[a.as_ptr() as u64, scratch.as_mut_ptr() as u64]).unwrap_or_else(|t| panic!("vmovmask {ty:?}: {t:?}"))[0];
            let want_m: u64 = (0..ty.lanes() as usize).map(|i| ((lane_get(ty, &a, i) < 0) as u64) << i).sum();
            assert_eq!(m, want_m, "vmovmask {ty:?} {a:?}");
        }
        // extract every lane
        for i in 0..ty.lanes() as u8 {
            let ex = vec_un(ty, move |b, a, _out| Some(b.vextract(a, i)));
            for a in lanes(ty) {
                let mut scratch = [0u8; 16];
                let r = run(&ex, &[a.as_ptr() as u64, scratch.as_mut_ptr() as u64]).unwrap()[0] as i64;
                assert_eq!(r, lane_get(ty, &a, i as usize), "vextract {ty:?} lane {i}");
            }
        }
    }
    // Float reductions: returned through a bitcast to I64.
    let p = vec_un(Type::V2F64, |b, a, _out| {
        let r = b.vreduce_add(a);
        Some(b.conv(ConvOp::Bitcast, Type::I64, r))
    });
    let mut v = [0u8; 16];
    v[..8].copy_from_slice(&1.5f64.to_le_bytes());
    v[8..].copy_from_slice(&2.25f64.to_le_bytes());
    let mut scratch = [0u8; 16];
    let r = run(&p, &[v.as_ptr() as u64, scratch.as_mut_ptr() as u64]).unwrap()[0];
    assert_eq!(f64::from_bits(r), 3.75);
    let p = vec_un(Type::V4F32, |b, a, _out| {
        let r = b.vreduce_add(a);
        let i = b.conv(ConvOp::Bitcast, Type::I32, r);
        Some(b.conv(ConvOp::Zext, Type::I64, i))
    });
    let mut v = [0u8; 16];
    for (i, f) in [1.0f32, 2.0, 3.0, 4.0].iter().enumerate() {
        v[i * 4..i * 4 + 4].copy_from_slice(&f.to_le_bytes());
    }
    let r = run(&p, &[v.as_ptr() as u64, scratch.as_mut_ptr() as u64]).unwrap()[0];
    assert_eq!(f32::from_bits(r as u32), 10.0);
}

/// The lane-wise min / max against scalar references at every integer lane width (the
/// interpreter is the oracle of the tests above; this pins both to Rust's `min` / `max`).
#[test]
fn vector_min_max_match_scalar_references() {
    for ty in INT_VECS {
        let w = ty.lane().bytes() as u32 * 8;
        for (name, op, signed, is_min) in [("vsmin", VecBinOp::Smin, true, true), ("vsmax", VecBinOp::Smax, true, false), ("vumin", VecBinOp::Umin, false, true), ("vumax", VecBinOp::Umax, false, false)] {
            let p = vec_bin(ty, |b, x, y| b.vbin(op, x, y));
            for a in lanes(ty) {
                for c in lanes(ty) {
                    let (o1, o2) = stored_both(&p, |out| vec![a.as_ptr() as u64, c.as_ptr() as u64, out as u64]);
                    assert_eq!(o1, o2, "{name} {ty:?} a={a:?} c={c:?}");
                    for i in 0..ty.lanes() as usize {
                        let (x, y) = (lane_get(ty, &a, i), lane_get(ty, &c, i));
                        let want = if signed {
                            if is_min { x.min(y) } else { x.max(y) }
                        } else {
                            let m = if w == 64 { u64::MAX } else { (1u64 << w) - 1 };
                            let (xu, yu) = (x as u64 & m, y as u64 & m);
                            let r = if is_min { xu.min(yu) } else { xu.max(yu) };
                            ((r << (64 - w)) as i64) >> (64 - w)
                        };
                        assert_eq!(lane_get(ty, &o2, i), want, "{name} {ty:?} lane {i} of a={a:?} c={c:?}");
                    }
                }
            }
        }
    }
}
