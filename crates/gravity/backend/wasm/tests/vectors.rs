//! The vector ops through SIMD128 against the interpreter's lane-by-lane
//! reference (`gravity_tpde/tests/ops_vector.rs`'s shapes): every op at
//! every lane width over edge lanes, operands through memory.

mod common;
use common::*;
use gravity_interp::Interp;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::*;

const INT_VECS: [Type; 4] = [Type::V16I8, Type::V8I16, Type::V4I32, Type::V2I64];
const ALL_VECS: [Type; 6] = [Type::V16I8, Type::V8I16, Type::V4I32, Type::V2I64, Type::V4F32, Type::V2F64];

/// `f(p, q, out) -> i64`: `a = load p`, `b = load q`; a vector result is stored to `out`, a scalar one returned.
fn vec_fn(ty: Type, mk: impl Fn(&mut FunctionBuilder, Value, Value) -> Value) -> Program {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::Ptr, Type::Ptr, Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let [p, q, out]: [Value; 3] = b.params().try_into().unwrap();
    let a = b.vload_unaligned(ty, p, 0);
    let c = b.vload_unaligned(ty, q, 0);
    let r = mk(&mut b, a, c);
    let ret = if b.value_type(r).is_vector() {
        b.vstore_unaligned(out, 0, r);
        b.iconst(Type::I64, 0)
    } else if b.value_type(r) == Type::I64 {
        r
    } else {
        let t = b.value_type(r);
        if t.is_float() {
            let bits = b.conv(ConvOp::Bitcast, if t == Type::F32 { Type::I32 } else { Type::I64 }, r);
            if t == Type::F32 {
                b.conv(ConvOp::Zext, Type::I64, bits)
            } else {
                bits
            }
        } else {
            b.conv(ConvOp::Sext, Type::I64, r)
        }
    };
    b.ret(&[ret]);
    pb.finish()
}

/// Edge lanes for a lane width, as 16-byte patterns.
fn lanes(ty: Type) -> Vec<[u8; 16]> {
    let w = ty.lane().bytes() as usize;
    let n = ty.lanes() as usize;
    let pats: [u64; 10] = [0, 1, u64::MAX, 2, 1 << 63, i64::MAX as u64, 0x7f, 0xff80, 0x8000_0000, 0x1234_5678_9abc_def0];
    let mut out = Vec::new();
    for k in 0..pats.len() {
        let mut v = [0u8; 16];
        for i in 0..n {
            let bytes = pats[(i + k) % pats.len()].to_le_bytes();
            v[i * w..(i + 1) * w].copy_from_slice(&bytes[..w]);
        }
        out.push(v);
    }
    if ty.is_float_vector() {
        let fs: [f64; 8] = [0.0, -0.0, 1.5, -2.25, f64::INFINITY, f64::NAN, 1e30, -1e-30];
        for k in 0..fs.len() {
            let mut v = [0u8; 16];
            for i in 0..n {
                let x = fs[(i + k) % fs.len()];
                if w == 4 {
                    v[i * 4..i * 4 + 4].copy_from_slice(&(x as f32).to_le_bytes());
                } else {
                    v[i * 8..i * 8 + 8].copy_from_slice(&x.to_le_bytes());
                }
            }
            out.push(v);
        }
    }
    out
}

/// Both backends over every operand pair; the stored vector and the scalar agree.
fn check(rig: &mut Rig, name: &str, ty: Type, p: &Program) {
    let (inst, _) = rig.instantiate(p, Func(0));
    let (gp, gq, gout) = (DATA, DATA + 16, DATA + 32);
    for a in lanes(ty) {
        for b in lanes(ty) {
            #[repr(align(16))]
            struct Buf([u8; 48]);
            let mut buf = Buf([0; 48]);
            buf.0[..16].copy_from_slice(&a);
            buf.0[16..32].copy_from_slice(&b);
            let base = buf.0.as_mut_ptr() as u64;
            let want = Interp::new(p).run(Func(0), &[base, base + 16, base + 32]).unwrap();
            rig.write(gp, &a);
            rig.write(gq, &b);
            rig.write(gout, &[0; 16]);
            let got = rig.call(&inst, p, Func(0), &[gp as u64, gq as u64, gout as u64]).unwrap();
            let want_vec = buf.0[32..48].to_vec();
            let got_vec = rig.read(gout, 16);
            let nan_ok = ty.is_float_vector() && same_lanes_nan(ty, &want_vec, &got_vec);
            assert!(got_vec == want_vec || nan_ok, "{name} {ty:?}: stored vector differs: wasm {got_vec:02x?} vs interpreter {want_vec:02x?}\na={a:02x?} b={b:02x?}");
            let scalar_ok = got == want || (ty.is_float_vector() && f64::from_bits(got[0]).is_nan() && f64::from_bits(want[0]).is_nan()) || (ty == Type::V4F32 && f32::from_bits(got[0] as u32).is_nan() && f32::from_bits(want[0] as u32).is_nan());
            assert!(scalar_ok, "{name} {ty:?}: scalar differs: wasm {got:?} vs interpreter {want:?}\na={a:02x?} b={b:02x?}");
        }
    }
}

fn same_lanes_nan(ty: Type, a: &[u8], b: &[u8]) -> bool {
    let w = ty.lane().bytes() as usize;
    (0..16 / w).all(|i| {
        let (x, y) = (&a[i * w..(i + 1) * w], &b[i * w..(i + 1) * w]);
        x == y
            || if w == 4 {
                f32::from_le_bytes(x.try_into().unwrap()).is_nan() && f32::from_le_bytes(y.try_into().unwrap()).is_nan()
            } else {
                f64::from_le_bytes(x.try_into().unwrap()).is_nan() && f64::from_le_bytes(y.try_into().unwrap()).is_nan()
            }
    })
}

#[test]
fn vector_binary_ops() {
    let mut rig = Rig::new();
    for ty in ALL_VECS {
        for op in [VecBinOp::Add, VecBinOp::Sub, VecBinOp::Mul, VecBinOp::And, VecBinOp::Or, VecBinOp::Xor, VecBinOp::Smin, VecBinOp::Smax, VecBinOp::Umin, VecBinOp::Umax] {
            if matches!(op, VecBinOp::Smin | VecBinOp::Smax | VecBinOp::Umin | VecBinOp::Umax) && !ty.is_int_vector() {
                continue;
            }
            if op == VecBinOp::Mul && ty == Type::V2I64 {
                continue;
            }
            check(&mut rig, &format!("{op:?}"), ty, &vec_fn(ty, |b, x, y| b.vbin(op, x, y)));
        }
    }
}

#[test]
fn vector_shifts() {
    let mut rig = Rig::new();
    for ty in INT_VECS {
        let w = ty.lane().bytes() * 8;
        for op in [VecShiftOp::Shl, VecShiftOp::Ushr, VecShiftOp::Sshr] {
            for count in [0u8, 1, 3, (w - 1) as u8] {
                check(&mut rig, &format!("{op:?} {count}"), ty, &vec_fn(ty, |b, x, _| b.vshift(op, x, count)));
            }
        }
    }
}

#[test]
fn vector_compares_and_select() {
    let mut rig = Rig::new();
    for ty in INT_VECS {
        for cond in [IntCond::Eq, IntCond::Ne, IntCond::Slt, IntCond::Sle, IntCond::Sgt, IntCond::Sge, IntCond::Ult, IntCond::Ule, IntCond::Ugt, IntCond::Uge] {
            check(&mut rig, &format!("vcmp {cond:?}"), ty, &vec_fn(ty, |b, x, y| b.vcmp(cond, x, y)));
            check(
                &mut rig,
                &format!("vselect {cond:?}"),
                ty,
                &vec_fn(ty, |b, x, y| {
                    let m = b.vcmp(cond, x, y);
                    b.vselect(m, x, y)
                }),
            );
        }
    }
}

#[test]
fn vector_reductions_and_movmask() {
    let mut rig = Rig::new();
    for ty in ALL_VECS {
        check(&mut rig, "vreduce_add", ty, &vec_fn(ty, |b, x, _| b.vreduce_add(x)));
        check(
            &mut rig,
            "vreduce_add sum",
            ty,
            &vec_fn(ty, |b, x, y| {
                let s = b.vbin(VecBinOp::Add, x, y);
                b.vreduce_add(s)
            }),
        );
        check(&mut rig, "vmovmask", ty, &vec_fn(ty, |b, x, _| b.vmovmask(x)));
    }
}

#[test]
fn vector_lanes_splat_extract_insert() {
    let mut rig = Rig::new();
    for ty in ALL_VECS {
        for lane in 0..ty.lanes() as u8 {
            check(&mut rig, &format!("vextract {lane}"), ty, &vec_fn(ty, |b, x, _| b.vextract(x, lane)));
            check(
                &mut rig,
                &format!("vinsert {lane}"),
                ty,
                &vec_fn(ty, |b, x, y| {
                    let s = b.vextract(y, 0);
                    b.vinsert(x, lane, s)
                }),
            );
        }
        check(
            &mut rig,
            "vsplat",
            ty,
            &vec_fn(ty, |b, x, _| {
                let s = b.vextract(x, 1);
                b.vsplat(ty, s)
            }),
        );
    }
}

#[test]
fn vector_widen_narrow_bitcast() {
    let mut rig = Rig::new();
    for ty in [Type::V16I8, Type::V8I16, Type::V4I32] {
        for signed in [false, true] {
            for high in [false, true] {
                check(&mut rig, &format!("vwiden {signed} {high}"), ty, &vec_fn(ty, |b, x, _| b.vwiden(x, signed, high)));
            }
        }
    }
    for ty in [Type::V8I16, Type::V4I32, Type::V2I64] {
        check(&mut rig, "vnarrow", ty, &vec_fn(ty, |b, x, y| b.vnarrow(x, y)));
    }
    for ty in ALL_VECS {
        for to in ALL_VECS {
            check(
                &mut rig,
                &format!("vbitcast {to:?}"),
                ty,
                &vec_fn(ty, |b, x, y| {
                    let c = b.vbitcast(to, x);
                    let d = b.vbitcast(to, y);
                    b.vbin(VecBinOp::Xor, c, d)
                }),
            );
        }
    }
}

/// Vectors as block parameters across a loop: the edge copies carry 128 bits.
#[test]
fn vector_loop_carried_parameters() {
    let mut rig = Rig::new();
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::Ptr, Type::I64], &[Type::I64]);
    let mut b = pb.define(f);
    let [p, n]: [Value; 2] = b.params().try_into().unwrap();
    let head = b.create_block(&[Type::V4I32, Type::V4I32, Type::I64]);
    let body = b.create_block(&[]);
    let exit = b.create_block(&[]);
    let zero = b.iconst(Type::I32, 0);
    let one = b.iconst(Type::I32, 1);
    let z = b.vsplat(Type::V4I32, zero);
    let o = b.vsplat(Type::V4I32, one);
    let i0 = b.iconst(Type::I64, 0);
    b.br(head, &[z, o, i0]);
    b.switch_to(head);
    b.mark_loop_header(head);
    let [acc, step, i]: [Value; 3] = b.block_params(head).try_into().unwrap();
    let done = b.icmp(IntCond::Sge, i, n);
    b.cond_br(done, (exit, &[]), (body, &[]));
    b.switch_to(body);
    let q = b.ptr_add(p, i, 16, 0);
    let v = b.vload_unaligned(Type::V4I32, q, 0);
    let m = b.vbin(VecBinOp::Mul, v, step);
    let acc2 = b.vbin(VecBinOp::Add, acc, m);
    let step2 = b.vbin(VecBinOp::Add, step, o);
    let i1 = b.iconst(Type::I64, 1);
    let i2 = b.int_bin(IntBinOp::Add, i, i1);
    b.br(head, &[acc2, step2, i2]);
    b.switch_to(exit);
    let r = b.vreduce_add(acc);
    b.ret(&[r]);
    let p = pb.finish();
    let (inst, _) = rig.instantiate(&p, Func(0));
    let data: Vec<i32> = (0..64).map(|k| (k * 7919 - 1000) as i32).collect();
    let bytes: Vec<u8> = data.iter().flat_map(|x| x.to_le_bytes()).collect();
    rig.write(DATA, &bytes);
    for n in [0i64, 1, 5, 16] {
        let want = Interp::new(&p).run(Func(0), &[data.as_ptr() as u64, n as u64]).unwrap();
        let got = rig.call(&inst, &p, Func(0), &[DATA as u64, n as u64]).unwrap();
        assert_eq!(got, want, "n = {n}");
    }
}
