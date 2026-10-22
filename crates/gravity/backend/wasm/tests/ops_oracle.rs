//! Every scalar op form at every width, compiled to wasm and run under
//! wasmtime against the interpreter over `gravity_interp/tests/ops_oracle.rs`'s
//! operand sets (the interpreter is itself checked against Rust there).

mod common;
use common::*;
use gravity_interp::canon;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::*;

const INT_TYPES: [Type; 4] = [Type::I8, Type::I16, Type::I32, Type::I64];

fn bits(ty: Type) -> u32 {
    ty.bytes() * 8
}
fn min(ty: Type) -> i64 {
    canon(ty, 1u64 << (bits(ty) - 1)) as i64
}
fn max(ty: Type) -> i64 {
    (u64::MAX >> (65 - bits(ty))) as i64
}
fn operands(ty: Type) -> Vec<i64> {
    let mut v = vec![0, 1, -1, 2, -2, 3, 7, -7, 10, 100, -100, 127, -128, 255, 256, 1000, -1000, 0x1234, 65535, 65536, 0x7fff_ffff, -0x8000_0000, 1 << 40, -(1 << 40), min(ty), max(ty), min(ty) + 1, max(ty) - 1, min(ty) / 2, max(ty) / 2];
    for x in &mut v {
        *x = canon(ty, *x as u64) as i64;
    }
    v.sort();
    v.dedup();
    v
}

fn bin_fn(mk: impl Fn(&mut FunctionBuilder, Value, Value) -> Value, ty: Type, res: Type) -> Program {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[ty, ty], &[res]);
    let mut b = pb.define(f);
    let [x, y]: [Value; 2] = b.params().try_into().unwrap();
    let r = mk(&mut b, x, y);
    b.ret(&[r]);
    pb.finish()
}
fn un_fn(mk: impl Fn(&mut FunctionBuilder, Value) -> Value, ty: Type, res: Type) -> Program {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[ty], &[res]);
    let mut b = pb.define(f);
    let x = b.params()[0];
    let r = mk(&mut b, x);
    b.ret(&[r]);
    pb.finish()
}

/// Every operand pair through both backends.
fn sweep2(rig: &mut Rig, p: &Program, xs: &[i64], ys: &[i64]) -> usize {
    let (inst, _) = rig.instantiate(p, Func(0));
    let mut n = 0;
    for &a in xs {
        for &c in ys {
            if both(rig, p, &inst, Func(0), &[a as u64, c as u64]).is_some() {
                n += 1;
            }
        }
    }
    n
}
fn sweep1(rig: &mut Rig, p: &Program, xs: &[i64]) -> usize {
    let (inst, _) = rig.instantiate(p, Func(0));
    for &a in xs {
        both(rig, p, &inst, Func(0), &[a as u64]);
    }
    xs.len()
}

#[test]
fn int_binary_ops() {
    let mut rig = Rig::new();
    let mut cases = 0;
    for ty in INT_TYPES {
        let ops = operands(ty);
        for op in [
            IntBinOp::Add, IntBinOp::Sub, IntBinOp::Mul, IntBinOp::Sdiv, IntBinOp::Udiv, IntBinOp::Srem, IntBinOp::Urem,
            IntBinOp::And, IntBinOp::Or, IntBinOp::Xor, IntBinOp::Shl, IntBinOp::Ushr, IntBinOp::Sshr,
            IntBinOp::Smin, IntBinOp::Smax, IntBinOp::Umin, IntBinOp::Umax,
        ] {
            let p = bin_fn(|b, x, y| b.int_bin(op, x, y), ty, ty);
            cases += sweep2(&mut rig, &p, &ops, &ops);
        }
    }
    eprintln!("int_binary_ops: {cases} cases agree");
}

#[test]
fn int_overflow_predicates() {
    let mut rig = Rig::new();
    for ty in INT_TYPES {
        let ops = operands(ty);
        for op in [IntOvfOp::SaddOvf, IntOvfOp::SsubOvf, IntOvfOp::SmulOvf] {
            let p = bin_fn(|b, x, y| b.int_ovf(op, x, y), ty, Type::Bool);
            sweep2(&mut rig, &p, &ops, &ops);
        }
    }
}

#[test]
fn int_unary_ops() {
    let mut rig = Rig::new();
    for ty in INT_TYPES {
        for op in [IntUnOp::Neg, IntUnOp::Not, IntUnOp::Clz, IntUnOp::Ctz, IntUnOp::Popcnt, IntUnOp::Bswap, IntUnOp::Bitrev] {
            let p = un_fn(|b, x| b.int_un(op, x), ty, ty);
            sweep1(&mut rig, &p, &operands(ty));
        }
    }
}

#[test]
fn int_compares() {
    let mut rig = Rig::new();
    for ty in INT_TYPES.iter().copied().chain([Type::Ptr]) {
        let ops = if ty == Type::Ptr { operands(Type::I64) } else { operands(ty) };
        for cond in [IntCond::Eq, IntCond::Ne, IntCond::Slt, IntCond::Sle, IntCond::Sgt, IntCond::Sge, IntCond::Ult, IntCond::Ule, IntCond::Ugt, IntCond::Uge] {
            let p = bin_fn(|b, x, y| b.icmp(cond, x, y), ty, Type::Bool);
            sweep2(&mut rig, &p, &ops, &ops);
        }
    }
}

#[test]
fn int_conversions() {
    let mut rig = Rig::new();
    for from in INT_TYPES {
        for to in INT_TYPES {
            let ops = operands(from);
            if to.bytes() > from.bytes() {
                sweep1(&mut rig, &un_fn(|b, x| b.conv(ConvOp::Sext, to, x), from, to), &ops);
                sweep1(&mut rig, &un_fn(|b, x| b.conv(ConvOp::Zext, to, x), from, to), &ops);
            }
            if to.bytes() < from.bytes() {
                sweep1(&mut rig, &un_fn(|b, x| b.conv(ConvOp::Trunc, to, x), from, to), &ops);
            }
        }
        for to in INT_TYPES {
            sweep1(&mut rig, &un_fn(|b, x| b.conv(ConvOp::Zext, to, x), Type::Bool, to), &[0, 1]);
            sweep1(&mut rig, &un_fn(|b, x| b.conv(ConvOp::Sext, to, x), Type::Bool, to), &[0, 1]);
        }
    }
    // Ptr <-> I64.
    sweep1(&mut rig, &un_fn(|b, x| b.conv(ConvOp::PtrToInt, Type::I64, x), Type::Ptr, Type::I64), &operands(Type::I64));
    sweep1(&mut rig, &un_fn(|b, x| b.conv(ConvOp::IntToPtr, Type::Ptr, x), Type::I64, Type::Ptr), &operands(Type::I64));
}

#[test]
fn int_float_conversions() {
    let mut rig = Rig::new();
    for from in INT_TYPES {
        for to in [Type::F32, Type::F64] {
            sweep1(&mut rig, &un_fn(|b, x| b.conv(ConvOp::SintToFloat, to, x), from, to), &operands(from));
            sweep1(&mut rig, &un_fn(|b, x| b.conv(ConvOp::UintToFloat, to, x), from, to), &operands(from));
        }
    }
    let vals: Vec<f64> = vec![0.0, -0.0, 1.0, -1.0, 1.5, -1.5, 2.999, -2.999, 100.25, 127.0, -128.0, 3.0e4, -3.0e4, 65535.0, 2147483647.0, -2147483648.0, 1e18, -1e18];
    for to in INT_TYPES {
        for from in [Type::F32, Type::F64] {
            let ps = un_fn(|b, x| b.conv(ConvOp::FloatToSint, to, x), from, to);
            let pu = un_fn(|b, x| b.conv(ConvOp::FloatToUint, to, x), from, to);
            let xs: Vec<i64> = vals.iter().map(|&v| if from == Type::F32 { (v as f32).to_bits() as i64 } else { v.to_bits() as i64 }).collect();
            sweep1(&mut rig, &ps, &xs);
            sweep1(&mut rig, &pu, &xs);
        }
    }
}

#[test]
fn float_ops() {
    let mut rig = Rig::new();
    let vals64: Vec<f64> = vec![0.0, -0.0, 1.0, -1.0, 0.5, 2.5, -2.5, 3.5, 1e300, -1e300, 1e-300, f64::INFINITY, f64::NEG_INFINITY, f64::NAN, f64::MAX, f64::MIN_POSITIVE, 123456.789];
    let b64: Vec<i64> = vals64.iter().map(|v| v.to_bits() as i64).collect();
    let b32: Vec<i64> = vals64.iter().map(|&v| (v as f32).to_bits() as i64).collect();
    for op in [FloatBinOp::Add, FloatBinOp::Sub, FloatBinOp::Mul, FloatBinOp::Div, FloatBinOp::Min, FloatBinOp::Max, FloatBinOp::Copysign] {
        sweep2(&mut rig, &bin_fn(|b, x, y| b.float_bin(op, x, y), Type::F64, Type::F64), &b64, &b64);
        sweep2(&mut rig, &bin_fn(|b, x, y| b.float_bin(op, x, y), Type::F32, Type::F32), &b32, &b32);
    }
    for op in [FloatUnOp::Neg, FloatUnOp::Abs, FloatUnOp::Sqrt, FloatUnOp::Ceil, FloatUnOp::Floor, FloatUnOp::Trunc, FloatUnOp::Nearest] {
        sweep1(&mut rig, &un_fn(|b, x| b.float_un(op, x), Type::F64, Type::F64), &b64);
        sweep1(&mut rig, &un_fn(|b, x| b.float_un(op, x), Type::F32, Type::F32), &b32);
    }
    for cond in [FloatCond::Eq, FloatCond::Ne, FloatCond::Lt, FloatCond::Le, FloatCond::Gt, FloatCond::Ge, FloatCond::Ordered, FloatCond::Unordered] {
        sweep2(&mut rig, &bin_fn(|b, x, y| b.fcmp(cond, x, y), Type::F64, Type::Bool), &b64, &b64);
        sweep2(&mut rig, &bin_fn(|b, x, y| b.fcmp(cond, x, y), Type::F32, Type::Bool), &b32, &b32);
    }
    sweep1(&mut rig, &un_fn(|b, x| b.conv(ConvOp::Fpromote, Type::F64, x), Type::F32, Type::F64), &b32);
    sweep1(&mut rig, &un_fn(|b, x| b.conv(ConvOp::Fdemote, Type::F32, x), Type::F64, Type::F32), &b64);
    sweep1(&mut rig, &un_fn(|b, x| b.conv(ConvOp::Bitcast, Type::I64, x), Type::F64, Type::I64), &b64);
    sweep1(&mut rig, &un_fn(|b, x| b.conv(ConvOp::Bitcast, Type::F64, x), Type::I64, Type::F64), &b64);
    sweep1(&mut rig, &un_fn(|b, x| b.conv(ConvOp::Bitcast, Type::I32, x), Type::F32, Type::I32), &b32);
    sweep1(&mut rig, &un_fn(|b, x| b.conv(ConvOp::Bitcast, Type::F32, x), Type::I32, Type::F32), &b32.iter().map(|&x| x as i32 as i64).collect::<Vec<_>>());
}

#[test]
fn select_and_bool_logic() {
    let mut rig = Rig::new();
    for ty in [Type::I64, Type::I32, Type::I8, Type::F64, Type::Bool] {
        let p = bin_fn(
            |b, x, y| {
                let c = if ty.is_float() { b.fcmp(FloatCond::Lt, x, y) } else { b.icmp(IntCond::Slt, x, y) };
                let s = b.select(c, x, y);
                if ty == Type::Bool {
                    let a = b.int_bin(IntBinOp::And, x, y);
                    let o = b.int_bin(IntBinOp::Or, s, a);
                    b.int_bin(IntBinOp::Xor, o, c)
                } else {
                    s
                }
            },
            ty,
            ty,
        );
        let xs: Vec<i64> = match ty {
            Type::Bool => vec![0, 1],
            Type::F64 => vec![1.5f64.to_bits() as i64, (-2.0f64).to_bits() as i64, f64::NAN.to_bits() as i64],
            _ => operands(ty),
        };
        sweep2(&mut rig, &p, &xs, &xs);
    }
}

#[test]
fn memory_widths_and_alignment() {
    let mut rig = Rig::new();
    for ty in [Type::I8, Type::I16, Type::I32, Type::I64, Type::F32, Type::F64, Type::Bool, Type::Ptr] {
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("f", &[Type::Ptr, ty], &[ty, ty, Type::I64]);
        let mut b = pb.define(f);
        let [p, v]: [Value; 2] = b.params().try_into().unwrap();
        b.store(p, 16, v);
        b.store_unaligned(p, 33, v);
        b.store_unaligned(p, -7, v);
        let r1 = b.load(ty, p, 16);
        let r2 = b.load_unaligned(ty, p, 33);
        let r3 = if ty.is_int() && ty != Type::I64 { b.load_zext_unaligned(ty, p, -7) } else { b.iconst(Type::I64, 0) };
        b.ret(&[r1, r2, r3]);
        let prog = pb.finish();
        let (inst, _) = rig.instantiate(&prog, Func(0));
        let vals: Vec<u64> = match ty {
            Type::Bool => vec![0, 1],
            Type::F32 => vec![1.5f32.to_bits() as u64, f32::NAN.to_bits() as u64],
            Type::F64 => vec![(-2.25f64).to_bits(), f64::INFINITY.to_bits()],
            Type::Ptr => vec![0, 0x1234_5678_9abc_def0],
            _ => operands(ty).iter().map(|&x| x as u64).collect(),
        };
        #[repr(align(16))]
        struct Buf([u8; 80]);
        let base = DATA + 64;
        for v in vals {
            let mut buf = Buf([0xAA; 80]);
            rig.write(base - 16, &[0xAA; 80]);
            let want = gravity_interp::Interp::new(&prog).run(Func(0), &[buf.0.as_mut_ptr() as u64 + 16, v]).unwrap();
            let got = rig.call(&inst, &prog, Func(0), &[base as u64, v]).unwrap();
            assert_eq!(got, want, "{ty:?} {v:#x}");
            assert_eq!(rig.read(base - 16, 80), buf.0.to_vec(), "{ty:?} {v:#x}: the guest bytes equal the native bytes");
        }
    }
}

#[test]
fn crc32c_matches_the_reference() {
    let mut rig = Rig::new();
    let p = bin_fn(|b, x, y| b.crc32c(x, y), Type::I64, Type::I64);
    let (inst, _) = rig.instantiate(&p, Func(0));
    for acc in [0i64, 1, -1, 0xdead_beef, 0x1234_5678] {
        for data in [0i64, 1, -1, 0x0102_0304_0506_0708, i64::MIN, i64::MAX, 0x7f7f_7f7f_7f7f_7f7f] {
            let got = both(&mut rig, &p, &inst, Func(0), &[acc as u64, data as u64]).unwrap();
            assert_eq!(got[0], gravity_interp::crc32c_u64(acc as u32, data as u64) as u64);
        }
    }
}

#[test]
fn stack_slots_and_ptr_add() {
    let mut rig = Rig::new();
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::I64, Type::I64], &[Type::I64, Type::I64, Type::I64]);
    let mut b = pb.define(f);
    let s0 = b.declare_stack_slot(8, 8);
    let s1 = b.declare_stack_slot(64, 16);
    let s2 = b.declare_stack_slot(1, 1);
    let [x, y]: [Value; 2] = b.params().try_into().unwrap();
    let p0 = b.stack_addr(s0);
    let p1 = b.stack_addr(s1);
    let p2 = b.stack_addr(s2);
    b.store(p0, 0, x);
    let q = b.ptr_add(p1, y, 8, 16);
    b.store(q, 0, y);
    let q2 = b.ptr_add(p1, y, 8, 0);
    let back = b.load(Type::I64, q2, 16);
    let t = b.conv(ConvOp::Trunc, Type::I8, x);
    b.store(p2, 0, t);
    let t2 = b.load_zext(Type::I8, p2, 0);
    let a = b.load(Type::I64, p0, 0);
    let s = b.int_bin(IntBinOp::Add, a, back);
    let d = b.conv(ConvOp::PtrToInt, Type::I64, p1);
    let e = b.conv(ConvOp::PtrToInt, Type::I64, p0);
    let diff = b.int_bin(IntBinOp::Sub, d, e);
    b.ret(&[s, t2, diff]);
    let p = pb.finish();
    let (inst, _) = rig.instantiate(&p, Func(0));
    let sp0 = rig.sp_value();
    for (x, y) in [(5i64, 0i64), (-300, 3), (1 << 40, 5)] {
        let got = rig.call(&inst, &p, Func(0), &[x as u64, y as u64]).unwrap();
        assert_eq!(got[0], (x + y) as u64);
        assert_eq!(got[1], (x as u8) as u64);
        assert_eq!(got[2], 16, "slot 1 is 16-aligned after an 8-byte slot");
        assert_eq!(rig.sp_value(), sp0, "the shadow stack pointer is restored");
    }
}

#[test]
fn switch_terminator() {
    let mut rig = Rig::new();
    for ty in [Type::I64, Type::I32, Type::I8] {
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("f", &[ty], &[Type::I64]);
        let mut b = pb.define(f);
        let x = b.params()[0];
        let out = b.create_block(&[Type::I64]);
        let c1 = b.create_block(&[]);
        let c2 = b.create_block(&[Type::I64]);
        let k7 = b.iconst(Type::I64, 7);
        let kd = b.iconst(Type::I64, -1);
        b.switch(x, [(0, c1, &[][..]), (canon(ty, -3i64 as u64), c2, &[k7][..]), (100, out, &[kd][..])], (c2, &[kd]));
        b.switch_to(c1);
        let k1 = b.iconst(Type::I64, 1);
        b.br(out, &[k1]);
        b.switch_to(c2);
        let v = b.block_params(c2)[0];
        let two = b.iconst(Type::I64, 2);
        let m = b.int_bin(IntBinOp::Mul, v, two);
        b.br(out, &[m]);
        b.switch_to(out);
        let r = b.block_params(out)[0];
        b.ret(&[r]);
        let p = pb.finish();
        let (inst, _) = rig.instantiate(&p, Func(0));
        for x in [0i64, -3, 100, 5, 1, -1, 127, -128] {
            both(&mut rig, &p, &inst, Func(0), &[canon(ty, x as u64)]);
        }
    }
}

/// `fn(alo, blo, ahi, bhi) -> addhi` and the 12 words its sweep crosses (carry in / out at both ends).
fn addhi_fn() -> Program {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::I64; 4], &[Type::I64]);
    let mut b = pb.define(f);
    let [w, x, y, z]: [Value; 4] = b.params().try_into().unwrap();
    let r = b.add_hi(w, x, y, z);
    b.ret(&[r]);
    pb.finish()
}
const ADDHI_WORDS: [i64; 12] = [0, 1, -1, 2, -2, i64::MIN, i64::MAX, i64::MIN + 1, i64::MAX - 1, 1 << 40, -(1 << 40), 0x5555_5555_5555_5555];
fn addhi_want(a: [i64; 4]) -> u64 {
    let x = ((a[2] as i128) << 64) | (a[0] as u64 as i128);
    let y = ((a[3] as i128) << 64) | (a[1] as u64 as i128);
    (x.wrapping_add(y) >> 64) as u64
}

/// `rotl` (I64 only), a run-time count and constant counts in and out of range, against the interpreter and `u64::rotate_left`.
#[test]
fn rotl_i64() {
    let mut rig = Rig::new();
    let ops = operands(Type::I64);
    let p = bin_fn(|b, x, y| b.int_bin(IntBinOp::Rotl, x, y), Type::I64, Type::I64);
    let (inst, _) = rig.instantiate(&p, Func(0));
    for &a in &ops {
        for &c in &ops {
            let got = both(&mut rig, &p, &inst, Func(0), &[a as u64, c as u64]);
            assert_eq!(got.map(|v| v[0]), Some((a as u64).rotate_left((c as u64 % 64) as u32)), "rotl {a} {c}");
        }
    }
    for k in [0i64, 1, 13, 32, 63, 64, 65, -1] {
        let p = un_fn(
            |b, x| {
                let kc = b.iconst(Type::I64, k);
                b.int_bin(IntBinOp::Rotl, x, kc)
            },
            Type::I64,
            Type::I64,
        );
        assert_eq!(sweep1(&mut rig, &p, &ops), ops.len(), "rotl by {k}");
    }
}

/// `smulh` / `umulh` through the helpers and `addhi` inline, against the interpreter.
#[test]
fn smulh_and_addhi() {
    let mut rig = Rig::new();
    let ops = operands(Type::I64);
    let p = bin_fn(|b, x, y| b.int_bin(IntBinOp::Smulh, x, y), Type::I64, Type::I64);
    sweep2(&mut rig, &p, &ops, &ops);
    let p = bin_fn(|b, x, y| b.int_bin(IntBinOp::Umulh, x, y), Type::I64, Type::I64);
    sweep2(&mut rig, &p, &ops, &ops);
    let p = addhi_fn();
    let (inst, _) = rig.instantiate(&p, Func(0));
    for &w in &ADDHI_WORDS { for &x in &ADDHI_WORDS { for &y in &ADDHI_WORDS { for &z in &ADDHI_WORDS {
        let got = both(&mut rig, &p, &inst, Func(0), &[w as u64, x as u64, y as u64, z as u64]);
        assert_eq!(got.map(|v| v[0]), Some(addhi_want([w, x, y, z])), "addhi {w} {x} {y} {z}");
    } } } }
}
