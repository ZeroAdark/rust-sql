//! Every integer/float op at every width, checked against Rust semantics
//! over a fixed set of edge-case operands.

mod common;
use gravity_interp::{canon, Trap};
use gravity_ir::builder::ProgramBuilder;
use gravity_ir::*;

const INT_TYPES: [Type; 4] = [Type::I8, Type::I16, Type::I32, Type::I64];

fn bits(ty: Type) -> u32 {
    ty.bytes() * 8
}
fn min(ty: Type) -> i64 {
    canon(ty, 1u64 << (bits(ty) - 1)) as i64 // e.g. I8: 0x80 -> -128
}
fn max(ty: Type) -> i64 {
    (u64::MAX >> (65 - bits(ty))) as i64
}
/// Interesting operands for a width, as canonical sign-extended i64.
fn operands(ty: Type) -> Vec<i64> {
    let mut v = vec![0, 1, -1, 2, -2, 3, 7, -7, 10, 100, -100, 127, -128, 255, 256, 1000, -1000, 0x1234, 65535, 65536, 0x7fff_ffff, -0x8000_0000, 1 << 40, -(1 << 40), min(ty), max(ty), min(ty) + 1, max(ty) - 1, min(ty) / 2, max(ty) / 2];
    for x in &mut v {
        *x = canon(ty, *x as u64) as i64;
    }
    v.sort();
    v.dedup();
    v
}

/// Build `fn(a: ty, b: ty) -> res` with one binary op.
fn bin_fn(mk: impl Fn(&mut gravity_ir::builder::FunctionBuilder, Value, Value) -> Value, ty: Type, res: Type) -> Program {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[ty, ty], &[res]);
    let mut b = pb.define(f);
    let [x, y]: [Value; 2] = b.params().try_into().unwrap();
    let r = mk(&mut b, x, y);
    b.ret(&[r]);
    pb.finish()
}
fn un_fn(mk: impl Fn(&mut gravity_ir::builder::FunctionBuilder, Value) -> Value, ty: Type, res: Type) -> Program {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[ty], &[res]);
    let mut b = pb.define(f);
    let x = b.params()[0];
    let r = mk(&mut b, x);
    b.ret(&[r]);
    pb.finish()
}

fn run2(p: &Program, a: i64, b: i64) -> Result<u64, Trap> {
    common::both(p, Func(0), &[a as u64, b as u64]).map(|v| v[0])
}
fn run1(p: &Program, a: i64) -> Result<u64, Trap> {
    common::both(p, Func(0), &[a as u64]).map(|v| v[0])
}

/// Reference: exact i128 arithmetic wrapped to width.
fn wrap(ty: Type, v: i128) -> i64 {
    canon(ty, v as u64) as i64
}
fn fits(ty: Type, v: i128) -> bool {
    v >= min(ty) as i128 && v <= max(ty) as i128
}
fn unsigned(ty: Type, v: i64) -> u64 {
    (v as u64) & (u64::MAX >> (64 - bits(ty)))
}

#[test]
fn int_binary_ops() {
    for ty in INT_TYPES {
        let ops = operands(ty);
        for op in [
            IntBinOp::Add, IntBinOp::Sub, IntBinOp::Mul, IntBinOp::Sdiv, IntBinOp::Udiv, IntBinOp::Srem, IntBinOp::Urem,
            IntBinOp::And, IntBinOp::Or, IntBinOp::Xor, IntBinOp::Shl, IntBinOp::Ushr, IntBinOp::Sshr,
            IntBinOp::Smin, IntBinOp::Smax, IntBinOp::Umin, IntBinOp::Umax,
        ] {
            let p = bin_fn(|b, x, y| b.int_bin(op, x, y), ty, ty);
            for &a in &ops {
                for &c in &ops {
                    let got = run2(&p, a, c);
                    let (ua, uc) = (unsigned(ty, a), unsigned(ty, c));
                    let sh = (uc as u32) % bits(ty);
                    let want: Result<i64, ()> = match op {
                        IntBinOp::Add => Ok(wrap(ty, a as i128 + c as i128)),
                        IntBinOp::Sub => Ok(wrap(ty, a as i128 - c as i128)),
                        IntBinOp::Mul => Ok(wrap(ty, a as i128 * c as i128)),
                        IntBinOp::Sdiv => if c == 0 || (a == min(ty) && c == -1) { Err(()) } else { Ok(wrap(ty, (a as i128) / (c as i128))) },
                        IntBinOp::Srem => if c == 0 || (a == min(ty) && c == -1) { Err(()) } else { Ok(wrap(ty, (a as i128) % (c as i128))) },
                        IntBinOp::Udiv => ua.checked_div(uc).map(|q| wrap(ty, q as i128)).ok_or(()),
                        IntBinOp::Urem => ua.checked_rem(uc).map(|r| wrap(ty, r as i128)).ok_or(()),
                        IntBinOp::And => Ok(a & c),
                        IntBinOp::Or => Ok(a | c),
                        IntBinOp::Xor => Ok(a ^ c),
                        IntBinOp::Shl => Ok(wrap(ty, (ua << sh) as i128)),
                        IntBinOp::Ushr => Ok(wrap(ty, (ua >> sh) as i128)),
                        IntBinOp::Sshr => Ok(wrap(ty, (a >> sh) as i128)),
                        IntBinOp::Smin => Ok(a.min(c)),
                        IntBinOp::Smax => Ok(a.max(c)),
                        IntBinOp::Umin => Ok(wrap(ty, ua.min(uc) as i128)),
                        IntBinOp::Umax => Ok(wrap(ty, ua.max(uc) as i128)),
                        IntBinOp::Smulh | IntBinOp::Umulh => unreachable!("smulh_and_addhi"),
                        IntBinOp::Rotl => unreachable!("rotl_i64"),
                    };
                    match (got, want) {
                        (Ok(g), Ok(w)) => assert_eq!(g as i64, w, "{ty:?} {op:?} {a} {c}"),
                        (Err(_), Err(())) => {}
                        (g, w) => panic!("{ty:?} {op:?} {a} {c}: got {g:?} want {w:?}"),
                    }
                }
            }
        }
    }
}

#[test]
fn int_overflow_predicates() {
    for ty in INT_TYPES {
        let ops = operands(ty);
        for op in [IntOvfOp::SaddOvf, IntOvfOp::SsubOvf, IntOvfOp::SmulOvf] {
            let p = bin_fn(|b, x, y| b.int_ovf(op, x, y), ty, Type::Bool);
            for &a in &ops {
                for &c in &ops {
                    let exact = match op {
                        IntOvfOp::SaddOvf => a as i128 + c as i128,
                        IntOvfOp::SsubOvf => a as i128 - c as i128,
                        IntOvfOp::SmulOvf => a as i128 * c as i128,
                    };
                    assert_eq!(run2(&p, a, c).unwrap(), (!fits(ty, exact)) as u64, "{ty:?} {op:?} {a} {c}");
                }
            }
        }
    }
}

#[test]
fn int_unary_ops() {
    for ty in INT_TYPES {
        for op in [IntUnOp::Neg, IntUnOp::Not, IntUnOp::Clz, IntUnOp::Ctz, IntUnOp::Popcnt, IntUnOp::Bswap, IntUnOp::Bitrev] {
            let p = un_fn(|b, x| b.int_un(op, x), ty, ty);
            for &a in &operands(ty) {
                let ua = unsigned(ty, a);
                let w = bits(ty);
                let want = match op {
                    IntUnOp::Neg => wrap(ty, -(a as i128)),
                    IntUnOp::Not => !a,
                    IntUnOp::Clz => (ua.leading_zeros() - (64 - w)) as i64,
                    IntUnOp::Ctz => ua.trailing_zeros().min(w) as i64,
                    IntUnOp::Popcnt => ua.count_ones() as i64,
                    IntUnOp::Bswap => wrap(ty, (ua.swap_bytes() >> (64 - w)) as i128),
                    IntUnOp::Bitrev => wrap(ty, (ua.reverse_bits() >> (64 - w)) as i128),
                };
                assert_eq!(run1(&p, a).unwrap() as i64, want, "{ty:?} {op:?} {a}");
            }
        }
    }
}

#[test]
fn int_compares() {
    for ty in INT_TYPES.iter().copied().chain([Type::Ptr]) {
        let ops = if ty == Type::Ptr { operands(Type::I64) } else { operands(ty) };
        for cond in [IntCond::Eq, IntCond::Ne, IntCond::Slt, IntCond::Sle, IntCond::Sgt, IntCond::Sge, IntCond::Ult, IntCond::Ule, IntCond::Ugt, IntCond::Uge] {
            let p = bin_fn(|b, x, y| b.icmp(cond, x, y), ty, Type::Bool);
            for &a in &ops {
                for &c in &ops {
                    let (ua, uc) = (a as u64, c as u64);
                    let want = match cond {
                        IntCond::Eq => a == c,
                        IntCond::Ne => a != c,
                        IntCond::Slt => a < c,
                        IntCond::Sle => a <= c,
                        IntCond::Sgt => a > c,
                        IntCond::Sge => a >= c,
                        IntCond::Ult => ua < uc,
                        IntCond::Ule => ua <= uc,
                        IntCond::Ugt => ua > uc,
                        IntCond::Uge => ua >= uc,
                    };
                    assert_eq!(run2(&p, a, c).unwrap(), want as u64, "{ty:?} {cond:?} {a} {c}");
                }
            }
        }
    }
}

#[test]
fn int_conversions() {
    for from in INT_TYPES {
        for to in INT_TYPES {
            let ops = operands(from);
            if to.bytes() > from.bytes() {
                let ps = un_fn(|b, x| b.conv(ConvOp::Sext, to, x), from, to);
                let pz = un_fn(|b, x| b.conv(ConvOp::Zext, to, x), from, to);
                for &a in &ops {
                    assert_eq!(run1(&ps, a).unwrap() as i64, a, "sext {from:?}->{to:?} {a}");
                    assert_eq!(run1(&pz, a).unwrap() as i64, wrap(to, unsigned(from, a) as i128), "zext {from:?}->{to:?} {a}");
                }
            }
            if to.bytes() < from.bytes() {
                let pt = un_fn(|b, x| b.conv(ConvOp::Trunc, to, x), from, to);
                for &a in &ops {
                    assert_eq!(run1(&pt, a).unwrap() as i64, wrap(to, a as i128), "trunc {from:?}->{to:?} {a}");
                }
            }
        }
        // Bool -> int.
        for to in INT_TYPES {
            let p = un_fn(|b, x| b.conv(ConvOp::Zext, to, x), Type::Bool, to);
            assert_eq!(run1(&p, 1).unwrap(), 1);
            assert_eq!(run1(&p, 0).unwrap(), 0);
        }
    }
}

#[test]
fn int_float_conversions() {
    for from in INT_TYPES {
        for to in [Type::F32, Type::F64] {
            let ps = un_fn(|b, x| b.conv(ConvOp::SintToFloat, to, x), from, to);
            let pu = un_fn(|b, x| b.conv(ConvOp::UintToFloat, to, x), from, to);
            for &a in &operands(from) {
                let gs = run1(&ps, a).unwrap();
                let gu = run1(&pu, a).unwrap();
                match to {
                    Type::F32 => {
                        assert_eq!(f32::from_bits(gs as u32), a as f32, "sitofp {from:?} {a}");
                        assert_eq!(f32::from_bits(gu as u32), unsigned(from, a) as f32, "uitofp {from:?} {a}");
                    }
                    _ => {
                        assert_eq!(f64::from_bits(gs), a as f64, "sitofp {from:?} {a}");
                        assert_eq!(f64::from_bits(gu), unsigned(from, a) as f64, "uitofp {from:?} {a}");
                    }
                }
            }
        }
    }
    // Float -> int on in-range values (out of range is undefined).
    for to in INT_TYPES {
        let p = un_fn(|b, x| b.conv(ConvOp::FloatToSint, to, x), Type::F64, to);
        for v in [0.0f64, 1.0, -1.0, 1.5, -1.5, 2.999, -2.999, 100.25, 127.0, -128.0, 3.0e4, -3.0e4] {
            if fits(to, v as i128) {
                assert_eq!(run1(&p, v.to_bits() as i64).unwrap() as i64, v as i64, "fptosi {to:?} {v}");
            }
        }
    }
}

/// GV-D2: an integer to f32 rounds once, to nearest-even (C's `(float4) int64`). Each operand's f64 rounding lands exactly
/// halfway between two f32s, so rounding through f64 first ties to the even one below.
#[test]
fn int_to_f32_rounds_once() {
    let ps = un_fn(|b, x| b.conv(ConvOp::SintToFloat, Type::F32, x), Type::I64, Type::F32);
    let pu = un_fn(|b, x| b.conv(ConvOp::UintToFloat, Type::F32, x), Type::I64, Type::F32);
    let f = |bits: u64| f32::from_bits(bits as u32);
    let s: [i64; 4] = [(1 << 59) + (1 << 35) + 1, -((1 << 59) + (1 << 35) + 1), (1 << 62) + (1 << 38) + 1, i64::MAX];
    for a in s {
        assert_eq!(f(run1(&ps, a).unwrap()), a as f32, "sitofp {a}");
    }
    assert_eq!(f(run1(&ps, s[0]).unwrap()), 576460821022900224.0f32, "2^59 + 2^35 + 1 is 2^59 + 2^36");
    assert_eq!(f(run1(&ps, s[2]).unwrap()), 4611686568183201792.0f32, "2^62 + 2^38 + 1 is 2^62 + 2^39");
    let u: [u64; 3] = [(1 << 63) + (1 << 39) + 1, (1 << 59) + (1 << 35) + 1, u64::MAX];
    for a in u {
        assert_eq!(f(run1(&pu, a as i64).unwrap()), a as f32, "uitofp {a}");
    }
    assert_eq!(f(run1(&pu, u[0] as i64).unwrap()), 9223373136366403584.0f32, "2^63 + 2^39 + 1 is 2^63 + 2^40");
}

#[test]
fn float_ops() {
    let vals64: Vec<f64> = vec![0.0, -0.0, 1.0, -1.0, 0.5, 2.5, -2.5, 3.5, 1e300, -1e300, 1e-300, f64::INFINITY, f64::NEG_INFINITY, f64::NAN, f64::MAX, f64::MIN_POSITIVE, 123456.789];
    let vals32: Vec<f32> = vals64.iter().map(|&v| v as f32).collect();
    for op in [FloatBinOp::Add, FloatBinOp::Sub, FloatBinOp::Mul, FloatBinOp::Div, FloatBinOp::Min, FloatBinOp::Max, FloatBinOp::Copysign] {
        let p64 = bin_fn(|b, x, y| b.float_bin(op, x, y), Type::F64, Type::F64);
        let p32 = bin_fn(|b, x, y| b.float_bin(op, x, y), Type::F32, Type::F32);
        for &a in &vals64 {
            for &c in &vals64 {
                let want = match op {
                    FloatBinOp::Add => a + c,
                    FloatBinOp::Sub => a - c,
                    FloatBinOp::Mul => a * c,
                    FloatBinOp::Div => a / c,
                    FloatBinOp::Min => a.min(c),
                    FloatBinOp::Max => a.max(c),
                    FloatBinOp::Copysign => a.copysign(c),
                };
                let got = f64::from_bits(run2(&p64, a.to_bits() as i64, c.to_bits() as i64).unwrap());
                assert!(got.to_bits() == want.to_bits() || (got.is_nan() && want.is_nan()), "f64 {op:?} {a} {c}: {got} vs {want}");
            }
        }
        for &a in &vals32 {
            for &c in &vals32 {
                let want = match op {
                    FloatBinOp::Add => a + c,
                    FloatBinOp::Sub => a - c,
                    FloatBinOp::Mul => a * c,
                    FloatBinOp::Div => a / c,
                    FloatBinOp::Min => a.min(c),
                    FloatBinOp::Max => a.max(c),
                    FloatBinOp::Copysign => a.copysign(c),
                };
                let got = f32::from_bits(run2(&p32, a.to_bits() as i64, c.to_bits() as i64).unwrap() as u32);
                assert!(got.to_bits() == want.to_bits() || (got.is_nan() && want.is_nan()), "f32 {op:?} {a} {c}: {got} vs {want}");
            }
        }
    }
    for op in [FloatUnOp::Neg, FloatUnOp::Abs, FloatUnOp::Sqrt, FloatUnOp::Ceil, FloatUnOp::Floor, FloatUnOp::Trunc, FloatUnOp::Nearest] {
        let p64 = un_fn(|b, x| b.float_un(op, x), Type::F64, Type::F64);
        for &a in &vals64 {
            let want = match op {
                FloatUnOp::Neg => -a,
                FloatUnOp::Abs => a.abs(),
                FloatUnOp::Sqrt => a.sqrt(),
                FloatUnOp::Ceil => a.ceil(),
                FloatUnOp::Floor => a.floor(),
                FloatUnOp::Trunc => a.trunc(),
                FloatUnOp::Nearest => a.round_ties_even(),
            };
            let got = f64::from_bits(run1(&p64, a.to_bits() as i64).unwrap());
            assert!(got.to_bits() == want.to_bits() || (got.is_nan() && want.is_nan()), "f64 {op:?} {a}: {got} vs {want}");
        }
    }
    for cond in [FloatCond::Eq, FloatCond::Ne, FloatCond::Lt, FloatCond::Le, FloatCond::Gt, FloatCond::Ge, FloatCond::Ordered, FloatCond::Unordered] {
        let p = bin_fn(|b, x, y| b.fcmp(cond, x, y), Type::F64, Type::Bool);
        for &a in &vals64 {
            for &c in &vals64 {
                let want = match cond {
                    FloatCond::Eq => a == c,
                    FloatCond::Ne => a != c,
                    FloatCond::Lt => a < c,
                    FloatCond::Le => a <= c,
                    FloatCond::Gt => a > c,
                    FloatCond::Ge => a >= c,
                    FloatCond::Ordered => !a.is_nan() && !c.is_nan(),
                    FloatCond::Unordered => a.is_nan() || c.is_nan(),
                };
                assert_eq!(run2(&p, a.to_bits() as i64, c.to_bits() as i64).unwrap(), want as u64, "fcmp {cond:?} {a} {c}");
            }
        }
    }
    // Promote / demote / bitcast.
    let pp = un_fn(|b, x| b.conv(ConvOp::Fpromote, Type::F64, x), Type::F32, Type::F64);
    let pd = un_fn(|b, x| b.conv(ConvOp::Fdemote, Type::F32, x), Type::F64, Type::F32);
    let pb64 = un_fn(|b, x| b.conv(ConvOp::Bitcast, Type::I64, x), Type::F64, Type::I64);
    let pb32 = un_fn(|b, x| b.conv(ConvOp::Bitcast, Type::F32, x), Type::I32, Type::F32);
    for &a in &vals64 {
        let d = f32::from_bits(run1(&pd, a.to_bits() as i64).unwrap() as u32);
        assert!(d.to_bits() == (a as f32).to_bits() || (d.is_nan() && a.is_nan()), "fdemote {a}");
        assert_eq!(run1(&pb64, a.to_bits() as i64).unwrap(), a.to_bits(), "bitcast f64->i64 {a}");
    }
    for &a in &vals32 {
        let u = f64::from_bits(run1(&pp, a.to_bits() as i64).unwrap());
        assert!(u.to_bits() == (a as f64).to_bits() || (u.is_nan() && a.is_nan()), "fpromote {a}");
        let bc = f32::from_bits(run1(&pb32, a.to_bits() as i32 as i64).unwrap() as u32);
        assert!(bc.to_bits() == a.to_bits(), "bitcast i32->f32 {a}");
    }
}

#[test]
fn select_and_bool_logic() {
    let p = bin_fn(
        |b, x, y| {
            let c = b.icmp(IntCond::Slt, x, y);
            b.select(c, x, y) // min
        },
        Type::I64,
        Type::I64,
    );
    for &a in &operands(Type::I64) {
        for &c in &operands(Type::I64) {
            assert_eq!(run2(&p, a, c).unwrap() as i64, a.min(c));
        }
    }
}

#[test]
fn memory_widths_and_alignment() {
    // Store each width at an offset, load it back both aligned and unaligned.
    for ty in [Type::I8, Type::I16, Type::I32, Type::I64, Type::F32, Type::F64, Type::Bool] {
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("f", &[Type::Ptr, ty], &[ty, ty]);
        let mut b = pb.define(f);
        let [p, v]: [Value; 2] = b.params().try_into().unwrap();
        b.store(p, 16, v);
        b.store_unaligned(p, 33, v);
        let r1 = b.load(ty, p, 16);
        let r2 = b.load_unaligned(ty, p, 33);
        b.ret(&[r1, r2]);
        let prog = pb.finish();
        #[repr(align(16))]
        struct Buf([u8; 64]);
        let mut buf = Buf([0xAA; 64]);
        let vals: Vec<u64> = match ty {
            Type::Bool => vec![0, 1],
            Type::F32 => vec![1.5f32.to_bits() as u64, f32::NAN.to_bits() as u64],
            Type::F64 => vec![(-2.25f64).to_bits(), f64::INFINITY.to_bits()],
            _ => operands(ty).iter().map(|&x| x as u64).collect(),
        };
        for v in vals {
            let r = common::both(&prog, Func(0), &[buf.0.as_mut_ptr() as u64, v]).unwrap();
            assert_eq!(r, vec![v, v], "{ty:?} {v:#x}");
        }
        // Bytes outside the stored ranges are untouched.
        assert!(buf.0[..16].iter().all(|&x| x == 0xAA));
    }
}

/// `crc32c` (one `crc32cx` / `crc32 r64` on the backends) against the
/// interpreter's bitwise reference over the edge-case operands, and the
/// reference against a known CRC-32C vector.
#[test]
fn crc32c_matches_reference() {
    // CRC-32C("123456789") = 0xE3069283 with the standard ~0 init / final
    // xor, computed as one 8-byte word then one byte (a 1-byte word with
    // seven zero bytes appended changes the CRC, so the last byte goes
    // through the byte-at-a-time reference).
    let word = u64::from_le_bytes(*b"12345678");
    let mut crc = gravity_interp::crc32c_u64(0xFFFF_FFFF, word);
    crc ^= b'9' as u32;
    for _ in 0..8 {
        crc = if crc & 1 != 0 { (crc >> 1) ^ 0x82F6_3B78 } else { crc >> 1 };
    }
    assert_eq!(crc ^ 0xFFFF_FFFF, 0xE306_9283);
    let p = bin_fn(|b, x, y| b.crc32c(x, y), Type::I64, Type::I64);
    let ops = operands(Type::I64);
    for &a in &ops {
        for &c in &ops {
            let got = run2(&p, a, c).unwrap();
            assert_eq!(got, gravity_interp::crc32c_u64(a as u32, c as u64) as u64, "crc32c({a:#x}, {c:#x})");
        }
    }
}

/// `prefetch` is a hint: the program's results are unchanged and the
/// address may be anything (an offset past the buffer, a null base).
#[test]
fn prefetch_is_a_no_op() {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::Ptr, Type::I64], &[Type::I64]);
    let mut b = pb.define(f);
    let [p, v]: [Value; 2] = b.params().try_into().unwrap();
    b.prefetch(p, 0);
    b.prefetch(p, 4096);
    b.prefetch(p, -64);
    let np = b.iconst(Type::Ptr, 0);
    b.prefetch(np, 0);
    b.store(p, 8, v);
    let r = b.load(Type::I64, p, 8);
    b.ret(&[r]);
    let prog = pb.finish();
    let mut buf = [0u64; 8];
    for v in operands(Type::I64) {
        let r = common::both(&prog, Func(0), &[buf.as_mut_ptr() as u64, v as u64]).unwrap();
        assert_eq!(r, vec![v as u64]);
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

/// `rotl` (I64 only): a run-time count (taken modulo 64) and constant counts in and out of range (the immediate form for
/// 0..64, the register form else) against `u64::rotate_left`.
#[test]
fn rotl_i64() {
    let p = bin_fn(|b, x, y| b.int_bin(IntBinOp::Rotl, x, y), Type::I64, Type::I64);
    let ops = operands(Type::I64);
    for &a in &ops {
        for &c in &ops {
            assert_eq!(run2(&p, a, c).unwrap(), (a as u64).rotate_left((c as u64 % 64) as u32), "rotl {a} {c}");
        }
    }
    for k in [0i64, 1, 13, 16, 17, 21, 32, 63, 64, 65, 127, -1, -13] {
        let p = un_fn(
            |b, x| {
                let kc = b.iconst(Type::I64, k);
                b.int_bin(IntBinOp::Rotl, x, kc)
            },
            Type::I64,
            Type::I64,
        );
        for &a in &ops {
            assert_eq!(run1(&p, a).unwrap(), (a as u64).rotate_left((k as u64 % 64) as u32), "rotl {a} by {k}");
        }
    }
}

/// `smulh` / `umulh` (the signed / unsigned high product word, I64 only) and `addhi` (the 128-bit sum's high word) against i128.
#[test]
fn smulh_and_addhi() {
    let p = bin_fn(|b, x, y| b.int_bin(IntBinOp::Smulh, x, y), Type::I64, Type::I64);
    let ops = operands(Type::I64);
    for &a in &ops {
        for &c in &ops {
            assert_eq!(run2(&p, a, c).unwrap(), ((a as i128 * c as i128) >> 64) as u64, "smulh {a} {c}");
        }
    }
    let p = bin_fn(|b, x, y| b.int_bin(IntBinOp::Umulh, x, y), Type::I64, Type::I64);
    for &a in &ops {
        for &c in &ops {
            assert_eq!(run2(&p, a, c).unwrap(), ((a as u64 as u128 * c as u64 as u128) >> 64) as u64, "umulh {a} {c}");
        }
    }
    let p = addhi_fn();
    for &w in &ADDHI_WORDS { for &x in &ADDHI_WORDS { for &y in &ADDHI_WORDS { for &z in &ADDHI_WORDS {
        let got = common::both(&p, Func(0), &[w as u64, x as u64, y as u64, z as u64]).unwrap()[0];
        assert_eq!(got, addhi_want([w, x, y, z]), "addhi {w} {x} {y} {z}");
    } } } }
}

/// Six i128 accumulators of signed products over a loop (`mul` + `smulh` into `iadd` + `addhi`): enough live words that the
/// allocator spills and reloads between `cmn`/`add` and `adc`, which must not clobber the carry.
#[test]
fn addhi_under_register_pressure() {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::I64, Type::I64], &[Type::I64]);
    let mut b = pb.define(f);
    let [n, seed]: [Value; 2] = b.params().try_into().unwrap();
    let tys = [Type::I64; 14];
    let head = b.create_block(&tys);
    let body = b.create_block(&tys);
    let exit = b.create_block(&tys);
    let zero = b.iconst(Type::I64, 0);
    let mut init = vec![zero, seed];
    init.extend(std::iter::repeat(zero).take(12));
    b.br(head, &init);
    b.switch_to(head);
    let p = b.block_params(head).to_vec();
    let more = b.icmp(IntCond::Slt, p[0], n);
    b.cond_br(more, (body, &p), (exit, &p));
    b.switch_to(body);
    let p = b.block_params(body).to_vec();
    let k = b.iconst(Type::I64, 0x9e37_79b9_7f4a_7c15u64 as i64);
    let x = b.int_bin(IntBinOp::Mul, p[1], k);
    let s = b.iconst(Type::I64, 17);
    let xs = b.int_bin(IntBinOp::Sshr, x, s);
    let x = b.int_bin(IntBinOp::Xor, x, xs);
    let one = b.iconst(Type::I64, 1);
    let mut next = vec![b.int_bin(IntBinOp::Add, p[0], one), x];
    for j in 0..6 {
        let m = b.iconst(Type::I64, (j as i64 + 3).wrapping_mul(0x1234_5678_9abc_def1u64 as i64) >> j);
        let y = b.int_bin(IntBinOp::Xor, x, m);
        let lo = b.int_bin(IntBinOp::Mul, x, y);
        let hi = b.int_bin(IntBinOp::Smulh, x, y);
        let (alo, ahi) = (p[2 + 2 * j], p[3 + 2 * j]);
        let nlo = b.int_bin(IntBinOp::Add, alo, lo);
        let nhi = b.add_hi(alo, lo, ahi, hi);
        next.push(nlo);
        next.push(nhi);
    }
    b.br(head, &next);
    b.switch_to(exit);
    let p = b.block_params(exit).to_vec();
    let mut acc = p[2];
    for &v in &p[3..] {
        let r = b.iconst(Type::I64, 7);
        let t = b.int_bin(IntBinOp::Shl, acc, r);
        acc = b.int_bin(IntBinOp::Xor, t, v);
    }
    b.ret(&[acc]);
    let prog = pb.finish();
    for (n, seed) in [(0i64, 1i64), (1, 5), (7, -3), (1000, 0x0123_4567_89ab_cdef)] {
        common::both(&prog, Func(0), &[n as u64, seed as u64]).unwrap();
    }
}
