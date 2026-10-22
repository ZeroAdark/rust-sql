//! Direct interpreter for Gravity IR.
//!
//! Every value is a `u64` slot. Integers are stored sign-extended to 64 bits
//! (so an `I32` holding -1 is `0xFFFF_FFFF_FFFF_FFFF`), `Bool` is 0 or 1,
//! floats are their IEEE bit pattern, pointers are their address. Arguments
//! and results use the same encoding. A 128-bit vector value keeps its low
//! 64 bits in the slot and its high 64 bits in a side table (`Frame::hi`);
//! lane `k` of a `w`-bit-lane vector is bits `k*w..(k+1)*w` of the 128
//! (lanes 0 and 1 of `V2I64` are the low and the high word). The vector
//! ops run lane by lane (`exec_vec`): this is the reference the backends'
//! NEON sequences are checked against (`gravity_tpde/tests/ops_oracle.rs`).
//!
//! This is the reference semantics for the TPDE backend and the tool for
//! running the text corpus. It is not tuned; it walks the IR as-is.
//!
//! Host calls go through fixed `extern "C"` trampolines selected by argument
//! count. Every argument is passed in an integer register class as `u64`
//! and the result is read as `u64`. That matches the System V and AAPCS64
//! ABIs for integer, pointer and bool parameters. Host functions that take or
//! return `f32`/`f64` directly are **not** supported by the interpreter; pass
//! floats as bitcast integers (which is what `Datum` does anyway).
#![deny(unsafe_code)]

use gravity_abi::host::MAX_ARGS as MAX_HOST_ARGS;
use gravity_abi::limits::INTERP_STACK_CHUNK;
use gravity_ir::*;

mod raw;
use raw::call_host;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trap {
    /// `Terminator::Unreachable` was executed.
    Unreachable { func: Func, block: Block },
    DivideByZero { func: Func, block: Block },
    /// Signed `MIN / -1` or `MIN % -1`.
    DivideOverflow { func: Func, block: Block },
    /// A value was read before any definition wrote it (a dominance bug in
    /// the producer of the IR).
    UndefinedValue { func: Func, value: Value },
    /// Host function signature the trampolines cannot express.
    UnsupportedHostSignature { host: HostFn },
    /// Recursion deeper than `MAX_DEPTH`.
    StackOverflow,
}

pub const MAX_DEPTH: usize = gravity_abi::limits::INTERP_MAX_DEPTH;

pub struct Interp<'p> {
    program: &'p Program,
}

impl<'p> Interp<'p> {
    pub fn new(program: &'p Program) -> Self {
        Self { program }
    }

    /// Run `func` with `args` (encoded as described in the crate docs) and
    /// return its results.
    pub fn run(&self, func: Func, args: &[u64]) -> Result<Vec<u64>, Trap> {
        self.run_depth(func, args, 0)
    }

    fn run_depth(&self, func: Func, args: &[u64], depth: usize) -> Result<Vec<u64>, Trap> {
        if depth > MAX_DEPTH {
            return Err(Trap::StackOverflow);
        }
        let f = self.program.func(func);
        assert_eq!(args.len(), f.params.len(), "{}: wrong argument count", f.name);
        let mut frame = Frame::new(f);
        let mut block = f.entry;
        for (&p, &a) in f.block(block).params.iter().zip(args) {
            frame.set(p, a);
        }
        loop {
            let b = f.block(block);
            for inst in &b.insts {
                self.exec(f, func, block, &mut frame, inst, depth)?;
            }
            let edge = match &b.term {
                Terminator::Br(e) => e,
                Terminator::CondBr { cond, then_edge, else_edge } => {
                    if frame.get(f, func, *cond)? != 0 {
                        then_edge
                    } else {
                        else_edge
                    }
                }
                Terminator::Switch { value, cases, default } => {
                    let ty = f.value_type(*value);
                    let v = mask(ty, frame.get(f, func, *value)?);
                    cases.iter().find(|c| mask(ty, c.value) == v).map(|c| &c.edge).unwrap_or(default)
                }
                Terminator::Return(vals) => {
                    return vals.iter().map(|&v| frame.get(f, func, v)).collect();
                }
                Terminator::Unreachable => return Err(Trap::Unreachable { func, block }),
            };
            // Parallel copy: read all args before writing any param. A
            // vector argument carries its high word too.
            let target = f.block(edge.target);
            let mut tmp = [(0u64, 0u64); 16];
            let mut heap;
            let vals: &mut [(u64, u64)] = if edge.args.len() <= tmp.len() {
                &mut tmp[..edge.args.len()]
            } else {
                heap = vec![(0u64, 0u64); edge.args.len()];
                &mut heap
            };
            for (slot, &a) in vals.iter_mut().zip(&edge.args) {
                *slot = frame.get2(f, func, a)?;
            }
            for (&p, &v) in target.params.iter().zip(vals.iter()) {
                frame.set2(p, v);
            }
            block = edge.target;
        }
    }

    fn exec(&self, f: &Function, func: Func, block: Block, fr: &mut Frame, inst: &Instruction, depth: usize) -> Result<(), Trap> {
        if is_vec_op(&inst.op) {
            let r = exec_vec(f, func, fr, &inst.op)?;
            match (inst.result, r) {
                (Some(res), Some(v)) => fr.set2(res, v),
                (None, None) => {}
                _ => unreachable!("verified: vector op result shape"),
            }
            return Ok(());
        }
        let get = |v: Value| fr.get(f, func, v);
        let result: Option<u64> = match &inst.op {
            Op::Const { ty, bits } => Some(canon(*ty, *bits)),
            Op::IntBin { op, a, b } => {
                let ty = f.value_type(*a);
                let (x, y) = (get(*a)?, get(*b)?);
                let (xs, ys) = (x as i64, y as i64);
                let (xu, yu) = (mask(ty, x), mask(ty, y));
                let bits = ty.bytes() * 8;
                let min = (1u64 << (bits - 1)) as i64;
                let min = (min << (64 - bits)) >> (64 - bits); // sign-extended MIN
                let r: u64 = match op {
                    IntBinOp::Add => xs.wrapping_add(ys) as u64,
                    IntBinOp::Sub => xs.wrapping_sub(ys) as u64,
                    IntBinOp::Mul => xs.wrapping_mul(ys) as u64,
                    IntBinOp::Sdiv | IntBinOp::Srem => {
                        if ys == 0 {
                            return Err(Trap::DivideByZero { func, block });
                        }
                        if xs == min && ys == -1 {
                            return Err(Trap::DivideOverflow { func, block });
                        }
                        (if *op == IntBinOp::Sdiv { xs / ys } else { xs % ys }) as u64
                    }
                    IntBinOp::Udiv | IntBinOp::Urem => {
                        if yu == 0 {
                            return Err(Trap::DivideByZero { func, block });
                        }
                        if *op == IntBinOp::Udiv {
                            xu / yu
                        } else {
                            xu % yu
                        }
                    }
                    IntBinOp::And => x & y,
                    IntBinOp::Or => x | y,
                    IntBinOp::Xor => x ^ y,
                    IntBinOp::Shl => xu.wrapping_shl(yu as u32 % bits),
                    IntBinOp::Ushr => xu.wrapping_shr(yu as u32 % bits),
                    IntBinOp::Sshr => (xs >> (yu as u32 % bits)) as u64,
                    IntBinOp::Smin => xs.min(ys) as u64,
                    IntBinOp::Smax => xs.max(ys) as u64,
                    IntBinOp::Umin => xu.min(yu),
                    IntBinOp::Umax => xu.max(yu),
                    IntBinOp::Smulh => ((xs as i128 * ys as i128) >> 64) as u64,
                    IntBinOp::Umulh => ((xu as u128 * yu as u128) >> 64) as u64,
                    IntBinOp::Rotl => xu.rotate_left(yu as u32 % bits),
                };
                Some(canon(ty, r))
            }
            Op::IntOvf { op, a, b } => {
                let ty = f.value_type(*a);
                let (x, y) = (get(*a)? as i64, get(*b)? as i64);
                let r = match op {
                    IntOvfOp::SaddOvf => x.wrapping_add(y),
                    IntOvfOp::SsubOvf => x.wrapping_sub(y),
                    IntOvfOp::SmulOvf => {
                        // Exact check in i128 to be correct for I64 too.
                        let p = (x as i128) * (y as i128);
                        let fits = p == (canon(ty, p as u64) as i64) as i128;
                        fr.set(inst.result.unwrap(), (!fits) as u64);
                        return Ok(());
                    }
                };
                // Fits iff canonicalizing to the width preserves the value,
                // and for I64 iff the i64 op did not overflow.
                let fits = match ty {
                    Type::I64 => match op {
                        IntOvfOp::SaddOvf => x.checked_add(y).is_some(),
                        IntOvfOp::SsubOvf => x.checked_sub(y).is_some(),
                        IntOvfOp::SmulOvf => unreachable!(),
                    },
                    _ => canon(ty, r as u64) as i64 == r,
                };
                Some((!fits) as u64)
            }
            Op::IntUn { op, a } => {
                let ty = f.value_type(*a);
                let x = get(*a)?;
                let bits = ty.bytes() * 8;
                let r = match op {
                    IntUnOp::Neg => (x as i64).wrapping_neg() as u64,
                    IntUnOp::Not => !x,
                    IntUnOp::Clz => (mask(ty, x).leading_zeros() - (64 - bits)) as u64,
                    IntUnOp::Ctz => mask(ty, x).trailing_zeros().min(bits) as u64,
                    IntUnOp::Popcnt => mask(ty, x).count_ones() as u64,
                    IntUnOp::Bswap => x.swap_bytes() >> (64 - bits),
                    IntUnOp::Bitrev => mask(ty, x).reverse_bits() >> (64 - bits),
                };
                Some(canon(ty, r))
            }
            Op::Icmp { cond, a, b } => {
                let (x, y) = (get(*a)?, get(*b)?);
                let (xs, ys) = (x as i64, y as i64);
                let r = match cond {
                    IntCond::Eq => x == y,
                    IntCond::Ne => x != y,
                    IntCond::Slt => xs < ys,
                    IntCond::Sle => xs <= ys,
                    IntCond::Sgt => xs > ys,
                    IntCond::Sge => xs >= ys,
                    IntCond::Ult => x < y,
                    IntCond::Ule => x <= y,
                    IntCond::Ugt => x > y,
                    IntCond::Uge => x >= y,
                };
                Some(r as u64)
            }
            Op::FloatBin { op, a, b } => {
                let ty = f.value_type(*a);
                Some(float_bin(ty, *op, get(*a)?, get(*b)?))
            }
            Op::FloatUn { op, a } => {
                let ty = f.value_type(*a);
                Some(float_un(ty, *op, get(*a)?))
            }
            Op::Fcmp { cond, a, b } => {
                let ty = f.value_type(*a);
                let (x, y) = (to_f64(ty, get(*a)?), to_f64(ty, get(*b)?));
                let r = match cond {
                    FloatCond::Eq => x == y,
                    FloatCond::Ne => x != y,
                    FloatCond::Lt => x < y,
                    FloatCond::Le => x <= y,
                    FloatCond::Gt => x > y,
                    FloatCond::Ge => x >= y,
                    FloatCond::Ordered => !x.is_nan() && !y.is_nan(),
                    FloatCond::Unordered => x.is_nan() || y.is_nan(),
                };
                Some(r as u64)
            }
            Op::Conv { op, to, a } => {
                let from = f.value_type(*a);
                if from.is_vector() {
                    unreachable!("vector bitcast is a vector op");
                }
                let x = get(*a)?;
                let r = match op {
                    ConvOp::Sext => canon(*to, x),          // already sign-extended
                    ConvOp::Zext => canon(*to, mask(from, x)),
                    ConvOp::Trunc => canon(*to, x),
                    // one rounding to the target width (C's `(float4) int64`); through f64 first rounds twice
                    ConvOp::SintToFloat => match to {
                        Type::F32 => (x as i64 as f32).to_bits() as u64,
                        _ => (x as i64 as f64).to_bits(),
                    },
                    ConvOp::UintToFloat => match to {
                        Type::F32 => (mask(from, x) as f32).to_bits() as u64,
                        _ => (mask(from, x) as f64).to_bits(),
                    },
                    ConvOp::FloatToSint => canon(*to, to_f64(from, x) as i64 as u64),
                    ConvOp::FloatToUint => canon(*to, to_f64(from, x) as u64),
                    ConvOp::Fpromote => (f32::from_bits(x as u32) as f64).to_bits(),
                    ConvOp::Fdemote => (f64::from_bits(x) as f32).to_bits() as u64,
                    ConvOp::Bitcast => canon(*to, mask(from, x)),
                    ConvOp::PtrToInt | ConvOp::IntToPtr => x,
                };
                Some(r)
            }
            Op::Select { cond, a, b } => Some(if get(*cond)? != 0 { get(*a)? } else { get(*b)? }),
            Op::LoadZext { ty, ptr, offset, unaligned } => {
                let p = (get(*ptr)? as i64 + *offset as i64) as usize as *const u8;
                Some(raw::load_zext(p, ty.bytes(), *unaligned))
            }
            Op::Load { ty, ptr, offset, unaligned } => {
                let p = (get(*ptr)? as i64 + *offset as i64) as usize as *const u8;
                Some(canon(*ty, raw::load(p, ty.bytes(), *unaligned)))
            }
            Op::Store { ptr, offset, value, unaligned } => {
                let p = (get(*ptr)? as i64 + *offset as i64) as usize as *mut u8;
                let v = get(*value)?;
                let ty = f.value_type(*value);
                raw::store(p, ty.bytes(), v, *unaligned);
                None
            }
            Op::Prefetch { ptr, .. } => {
                // A hint: evaluated for the dominance check, never dereferenced.
                get(*ptr)?;
                None
            }
            Op::Crc32c { acc, data } => Some(crc32c_u64(get(*acc)? as u32, get(*data)?) as u64),
            Op::AddHi { alo, blo, ahi, bhi } => {
                let carry = get(*alo)?.overflowing_add(get(*blo)?).1 as u64;
                Some(get(*ahi)?.wrapping_add(get(*bhi)?).wrapping_add(carry))
            }
            Op::StackAddr { slot } => Some(fr.slot_addr(*slot) as u64),
            Op::PtrAdd { base, index, scale, offset } => {
                let b = get(*base)? as i64;
                let i = get(*index)? as i64;
                Some(b.wrapping_add(i.wrapping_mul(*scale as i64)).wrapping_add(*offset as i64) as u64)
            }
            Op::Call { callee, args } => {
                let mut av = Vec::with_capacity(args.len());
                for &a in args.iter() {
                    av.push(get(a)?);
                }
                let r = self.run_depth(*callee, &av, depth + 1)?;
                r.first().copied()
            }
            Op::CallHost { callee, args } => {
                let decl = self.program.host_fn(*callee);
                let mut av = [0u64; MAX_HOST_ARGS];
                if args.len() > MAX_HOST_ARGS || decl.params.iter().any(|t| t.is_float()) || decl.ret.is_some_and(|t| t.is_float()) {
                    return Err(Trap::UnsupportedHostSignature { host: *callee });
                }
                for (slot, &a) in av.iter_mut().zip(args.iter()) {
                    *slot = get(a)?;
                }
                let raw = call_host(decl.addr, &av[..args.len()], host_narrow(&decl.params), host_ret(decl.ret));
                decl.ret.map(|t| canon(t, raw))
            }
            Op::VecLoad { .. } | Op::VecStore { .. } | Op::VecBin { .. } | Op::VecShift { .. } | Op::VecCmp { .. } | Op::VecSelect { .. } | Op::VecReduceAdd { .. } | Op::VecMovmask { .. } | Op::VecSplat { .. } | Op::VecExtract { .. } | Op::VecInsert { .. } | Op::VecWiden { .. } | Op::VecNarrow { .. } => {
                unreachable!("vector ops run in exec_vec")
            }
        };
        if let Some(r) = inst.result {
            fr.set(r, result.expect("verified: op has result"));
        }
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// Vectors (lane by lane; docs/design/gravity-simple/simd.md)
// ----------------------------------------------------------------------------

fn is_vec_op(op: &Op) -> bool {
    match op {
        Op::VecLoad { .. } | Op::VecStore { .. } | Op::VecBin { .. } | Op::VecShift { .. } | Op::VecCmp { .. } | Op::VecSelect { .. } | Op::VecReduceAdd { .. } | Op::VecMovmask { .. } | Op::VecSplat { .. } | Op::VecExtract { .. } | Op::VecInsert { .. } | Op::VecWiden { .. } | Op::VecNarrow { .. } => true,
        Op::Conv { op: ConvOp::Bitcast, to, .. } => to.is_vector(),
        _ => false,
    }
}

/// A 128-bit vector as its lanes, zero-extended to `u64` each (lane 0
/// first). `n` lanes of `w` bits.
fn unpack(ty: Type, (lo, hi): (u64, u64)) -> [u64; 16] {
    let n = ty.lanes() as usize;
    let w = 128 / n as u32;
    let mut out = [0u64; 16];
    let v = (lo as u128) | ((hi as u128) << 64);
    let m: u128 = if w == 64 { u64::MAX as u128 } else { (1u128 << w) - 1 };
    for (k, o) in out.iter_mut().enumerate().take(n) {
        *o = ((v >> (k as u32 * w)) & m) as u64;
    }
    out
}

/// Inverse of `unpack`: each lane's low `w` bits, packed.
fn pack(ty: Type, lanes: &[u64; 16]) -> (u64, u64) {
    let n = ty.lanes() as usize;
    let w = 128 / n as u32;
    let m: u128 = if w == 64 { u64::MAX as u128 } else { (1u128 << w) - 1 };
    let mut v: u128 = 0;
    for (k, &l) in lanes.iter().enumerate().take(n) {
        v |= ((l as u128) & m) << (k as u32 * w);
    }
    (v as u64, (v >> 64) as u64)
}

/// Sign-extend a `w`-bit lane to `i64`.
fn sext_lane(w: u32, x: u64) -> i64 {
    ((x << (64 - w)) as i64) >> (64 - w)
}

fn exec_vec(f: &Function, func: Func, fr: &mut Frame, op: &Op) -> Result<Option<(u64, u64)>, Trap> {
    let get = |v: Value| fr.get(f, func, v);
    let get2 = |v: Value| fr.get2(f, func, v);
    let lanes_of = |v: Value| -> Result<(Type, [u64; 16]), Trap> {
        let t = f.value_type(v);
        Ok((t, unpack(t, get2(v)?)))
    };
    Ok(match op {
        Op::Conv { op: ConvOp::Bitcast, a, .. } => Some(get2(*a)?),
        Op::VecLoad { ptr, offset, unaligned, .. } => {
            let p = (get(*ptr)? as i64 + *offset as i64) as usize as *const u8;
            let v: [u64; 2] = raw::load16(p, *unaligned);
            Some((v[0], v[1]))
        }
        Op::VecStore { ptr, offset, value, unaligned } => {
            let p = (get(*ptr)? as i64 + *offset as i64) as usize as *mut u8;
            let (lo, hi) = get2(*value)?;
            raw::store16(p, [lo, hi], *unaligned);
            None
        }
        Op::VecBin { op, a, b } => {
            let ty = f.value_type(*a);
            let (x, y) = (get2(*a)?, get2(*b)?);
            match op {
                VecBinOp::And => Some((x.0 & y.0, x.1 & y.1)),
                VecBinOp::Or => Some((x.0 | y.0, x.1 | y.1)),
                VecBinOp::Xor => Some((x.0 ^ y.0, x.1 ^ y.1)),
                VecBinOp::Add | VecBinOp::Sub | VecBinOp::Mul | VecBinOp::Smin | VecBinOp::Smax | VecBinOp::Umin | VecBinOp::Umax => {
                    let (xl, yl) = (unpack(ty, x), unpack(ty, y));
                    let w = 128 / ty.lanes();
                    let mut r = [0u64; 16];
                    for k in 0..ty.lanes() as usize {
                        r[k] = match ty {
                            Type::V2F64 => {
                                let (p, q) = (f64::from_bits(xl[k]), f64::from_bits(yl[k]));
                                (match op {
                                    VecBinOp::Add => p + q,
                                    VecBinOp::Sub => p - q,
                                    _ => p * q,
                                })
                                .to_bits()
                            }
                            Type::V4F32 => {
                                let (p, q) = (f32::from_bits(xl[k] as u32), f32::from_bits(yl[k] as u32));
                                (match op {
                                    VecBinOp::Add => p + q,
                                    VecBinOp::Sub => p - q,
                                    _ => p * q,
                                })
                                .to_bits() as u64
                            }
                            _ => match op {
                                VecBinOp::Add => xl[k].wrapping_add(yl[k]),
                                VecBinOp::Sub => xl[k].wrapping_sub(yl[k]),
                                VecBinOp::Smin => sext_lane(w, xl[k]).min(sext_lane(w, yl[k])) as u64,
                                VecBinOp::Smax => sext_lane(w, xl[k]).max(sext_lane(w, yl[k])) as u64,
                                VecBinOp::Umin => xl[k].min(yl[k]),
                                VecBinOp::Umax => xl[k].max(yl[k]),
                                _ => xl[k].wrapping_mul(yl[k]),
                            },
                        };
                    }
                    Some(pack(ty, &r))
                }
            }
        }
        Op::VecShift { op, a, count } => {
            let (ty, xl) = lanes_of(*a)?;
            let w = 128 / ty.lanes();
            let c = *count as u32;
            let mut r = [0u64; 16];
            for k in 0..ty.lanes() as usize {
                r[k] = match op {
                    VecShiftOp::Shl => xl[k] << c,
                    VecShiftOp::Ushr => xl[k] >> c,
                    VecShiftOp::Sshr => (sext_lane(w, xl[k]) >> c) as u64,
                };
            }
            Some(pack(ty, &r))
        }
        Op::VecCmp { cond, a, b } => {
            let (ty, xl) = lanes_of(*a)?;
            let yl = unpack(ty, get2(*b)?);
            let w = 128 / ty.lanes();
            let mut r = [0u64; 16];
            for k in 0..ty.lanes() as usize {
                let (xu, yu) = (xl[k], yl[k]);
                let (xs, ys) = (sext_lane(w, xu), sext_lane(w, yu));
                let t = match cond {
                    IntCond::Eq => xu == yu,
                    IntCond::Ne => xu != yu,
                    IntCond::Slt => xs < ys,
                    IntCond::Sle => xs <= ys,
                    IntCond::Sgt => xs > ys,
                    IntCond::Sge => xs >= ys,
                    IntCond::Ult => xu < yu,
                    IntCond::Ule => xu <= yu,
                    IntCond::Ugt => xu > yu,
                    IntCond::Uge => xu >= yu,
                };
                r[k] = if t { u64::MAX } else { 0 };
            }
            Some(pack(ty, &r))
        }
        Op::VecSelect { mask, a, b } => {
            let (m, x, y) = (get2(*mask)?, get2(*a)?, get2(*b)?);
            Some(((m.0 & x.0) | (!m.0 & y.0), (m.1 & x.1) | (!m.1 & y.1)))
        }
        Op::VecReduceAdd { a } => {
            let (ty, xl) = lanes_of(*a)?;
            let r = match ty {
                Type::V2F64 => (f64::from_bits(xl[0]) + f64::from_bits(xl[1])).to_bits(),
                Type::V4F32 => {
                    let l = |k: usize| f32::from_bits(xl[k] as u32);
                    ((l(0) + l(1)) + (l(2) + l(3))).to_bits() as u64
                }
                _ => {
                    let w = 128 / ty.lanes();
                    let mut s: i64 = 0;
                    for &x in xl.iter().take(ty.lanes() as usize) {
                        s = s.wrapping_add(sext_lane(w, x));
                    }
                    s as u64
                }
            };
            Some((r, 0))
        }
        Op::VecMovmask { a } => {
            let (ty, xl) = lanes_of(*a)?;
            let w = 128 / ty.lanes();
            let mut m = 0u64;
            for (k, &x) in xl.iter().enumerate().take(ty.lanes() as usize) {
                m |= ((x >> (w - 1)) & 1) << k;
            }
            Some((m, 0))
        }
        Op::VecSplat { ty, a } => {
            let x = get(*a)?;
            let mut r = [0u64; 16];
            for l in r.iter_mut().take(ty.lanes() as usize) {
                *l = x;
            }
            Some(pack(*ty, &r))
        }
        Op::VecExtract { a, lane } => {
            let (ty, xl) = lanes_of(*a)?;
            Some((canon(ty.lane(), xl[*lane as usize]), 0))
        }
        Op::VecInsert { a, lane, value } => {
            let (ty, mut xl) = lanes_of(*a)?;
            xl[*lane as usize] = get(*value)?;
            Some(pack(ty, &xl))
        }
        Op::VecWiden { a, signed, high } => {
            let (ty, xl) = lanes_of(*a)?;
            let to = ty.widened().expect("verified");
            let w = 128 / ty.lanes();
            let n = to.lanes() as usize;
            let base = if *high { n } else { 0 };
            let mut r = [0u64; 16];
            for k in 0..n {
                let x = xl[base + k];
                r[k] = if *signed { sext_lane(w, x) as u64 } else { x };
            }
            Some(pack(to, &r))
        }
        Op::VecNarrow { a, b } => {
            let (ty, xl) = lanes_of(*a)?;
            let yl = unpack(ty, get2(*b)?);
            let to = ty.narrowed().expect("verified");
            let n = ty.lanes() as usize;
            let mut r = [0u64; 16];
            r[..n].copy_from_slice(&xl[..n]);
            r[n..2 * n].copy_from_slice(&yl[..n]);
            // `pack` truncates each lane to the narrower width.
            Some(pack(to, &r))
        }
        _ => unreachable!("not a vector op"),
    })
}

// ----------------------------------------------------------------------------
// Frame
// ----------------------------------------------------------------------------

struct Frame {
    slots: Vec<u64>,
    /// High 64 bits of vector values (zero for scalars).
    hi: Vec<u64>,
    defined: Vec<bool>,
    /// The frame's stack slots: `base + slot_offsets[s]`, in the thread's interpreter stack (`Stack`).
    base: *mut u8,
    slot_offsets: Vec<usize>,
    /// Where the thread's stack stood before this frame (`Stack::pop` at drop); `None`: the function has no stack slots.
    mark: Option<(usize, usize)>,
}

impl Frame {
    fn new(f: &Function) -> Self {
        let mut slot_offsets = Vec::with_capacity(f.stack_slots.len());
        let mut size = 0usize;
        let mut max_align = 16usize;
        for s in &f.stack_slots {
            let align = (s.align as usize).max(1);
            max_align = max_align.max(align);
            size = (size + align - 1) & !(align - 1);
            slot_offsets.push(size);
            size += s.bytes as usize;
        }
        let (base, mark) = if f.stack_slots.is_empty() {
            (core::ptr::null_mut(), None)
        } else {
            let (base, mark) = STACK.with(|s| s.borrow_mut().push(size, max_align));
            (base, Some(mark))
        };
        Self { slots: vec![0; f.values.len()], hi: vec![0; f.values.len()], defined: vec![false; f.values.len()], base, slot_offsets, mark }
    }
    #[inline]
    fn get(&self, f: &Function, func: Func, v: Value) -> Result<u64, Trap> {
        debug_assert!(v.index() < f.values.len());
        if !self.defined[v.index()] {
            return Err(Trap::UndefinedValue { func, value: v });
        }
        Ok(self.slots[v.index()])
    }
    #[inline]
    fn set(&mut self, v: Value, x: u64) {
        self.slots[v.index()] = x;
        self.hi[v.index()] = 0;
        self.defined[v.index()] = true;
    }
    /// Both words of a value (the high word is 0 for a scalar).
    #[inline]
    fn get2(&self, f: &Function, func: Func, v: Value) -> Result<(u64, u64), Trap> {
        let lo = self.get(f, func, v)?;
        Ok((lo, self.hi[v.index()]))
    }
    #[inline]
    fn set2(&mut self, v: Value, (lo, hi): (u64, u64)) {
        self.slots[v.index()] = lo;
        self.hi[v.index()] = hi;
        self.defined[v.index()] = true;
    }
    fn slot_addr(&mut self, s: StackSlot) -> *mut u8 {
        // offsets were computed within the frame's region of the thread's stack
        self.base.wrapping_add(self.slot_offsets[s.index()])
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        if let Some(mark) = self.mark {
            // `try_with`: a frame dropped while the thread's locals are torn down has nothing to give back
            let _ = STACK.try_with(|s| s.borrow_mut().pop(mark));
        }
    }
}

// ----------------------------------------------------------------------------
// The interpreter's stack
// ----------------------------------------------------------------------------

/// D3229: the frames' stack slots live on one stack per thread, as compiled code's do, so a stack slot sits at the same
/// address call after call along the same call path. Kernels key state on such addresses: the sort kernels name a merge's
/// reader by its emit table, a stack slot of the reading pipeline (kernels ops/sort `reader_of`), and a recursive term's
/// pipeline, called again each generation, must find the merge it laid as its own. A heap allocation per frame moved the
/// table at every call, so the interpreter raised D1061's Bug where the compiled code reread its runs. Chunks are never
/// freed or moved while the thread lives (a chunk too small for a frame past the top is replaced, nothing live in it).
struct Stack {
    chunks: Vec<Box<[u8]>>,
    /// The top: chunk `chunk`, `off` bytes used (`chunks` empty: `(0, 0)`).
    chunk: usize,
    off: usize,
}

thread_local! {
    static STACK: core::cell::RefCell<Stack> = const { core::cell::RefCell::new(Stack { chunks: Vec::new(), chunk: 0, off: 0 }) };
}

impl Stack {
    /// `bytes` zeroed bytes aligned to `align` (a power of two) at the top; returns their address and the mark to pop to.
    fn push(&mut self, bytes: usize, align: usize) -> (*mut u8, (usize, usize)) {
        let mark = (self.chunk, self.off);
        let fits = |c: &[u8], off: usize| {
            let base = c.as_ptr() as usize;
            let at = ((base + off + align - 1) & !(align - 1)) - base;
            (at + bytes <= c.len()).then_some(at)
        };
        let (chunk, at) = match self.chunks.get(self.chunk).and_then(|c| fits(c, self.off)) {
            Some(at) => (self.chunk, at),
            None => {
                let next = if self.chunks.is_empty() { 0 } else { self.chunk + 1 };
                if self.chunks.get(next).and_then(|c| fits(c, 0)).is_none() {
                    let chunk = vec![0u8; (bytes + align).max(INTERP_STACK_CHUNK)].into_boxed_slice();
                    if next < self.chunks.len() {
                        self.chunks[next] = chunk;
                    } else {
                        self.chunks.push(chunk);
                    }
                }
                (next, fits(&self.chunks[next], 0).expect("a fresh chunk holds the frame"))
            }
        };
        let c = &mut self.chunks[chunk];
        c[at..at + bytes].fill(0);
        (self.chunk, self.off) = (chunk, at + bytes);
        (c.as_mut_ptr().wrapping_add(at), mark)
    }

    fn pop(&mut self, (chunk, off): (usize, usize)) {
        (self.chunk, self.off) = (chunk, off);
    }
}

// ----------------------------------------------------------------------------
// Value encoding helpers
// ----------------------------------------------------------------------------

/// Canonical slot encoding: sign-extend integers from their width, mask
/// floats and bool to their width.
#[inline]
/// CRC-32C of the eight little-endian bytes of `data` folded into `acc`
/// (bit by bit, reflected polynomial 0x82F63B78): the reference for the
/// backends' `crc32cx` / `crc32 r64`, which compute exactly this.
pub fn crc32c_u64(acc: u32, data: u64) -> u32 {
    let mut crc = acc;
    for i in 0..8 {
        crc ^= ((data >> (8 * i)) & 0xff) as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x82F6_3B78 } else { crc >> 1 };
        }
    }
    crc
}

pub fn canon(ty: Type, bits: u64) -> u64 {
    match ty {
        Type::Bool => bits & 1,
        Type::I8 => bits as u8 as i8 as i64 as u64,
        Type::I16 => bits as u16 as i16 as i64 as u64,
        Type::I32 => bits as u32 as i32 as i64 as u64,
        Type::F32 => bits & 0xFFFF_FFFF,
        Type::I64 | Type::F64 | Type::Ptr => bits,
        // A vector's low word is stored as is (`Frame::hi` holds the rest).
        _ => bits,
    }
}

/// Zero-extended view of an integer slot.
#[inline]
fn mask(ty: Type, bits: u64) -> u64 {
    match ty.bytes() {
        1 => bits & 0xFF,
        2 => bits & 0xFFFF,
        4 => bits & 0xFFFF_FFFF,
        _ => bits,
    }
}

#[inline]
fn to_f64(ty: Type, bits: u64) -> f64 {
    match ty {
        Type::F32 => f32::from_bits(bits as u32) as f64,
        _ => f64::from_bits(bits),
    }
}
#[inline]
fn from_f64(ty: Type, v: f64) -> u64 {
    match ty {
        Type::F32 => (v as f32).to_bits() as u64,
        _ => v.to_bits(),
    }
}

fn float_bin(ty: Type, op: FloatBinOp, a: u64, b: u64) -> u64 {
    // Compute at the operand width so f32 rounding is exact.
    macro_rules! go {
        ($t:ty, $from:expr, $to:expr) => {{
            let (x, y): ($t, $t) = ($from(a), $from(b));
            let r: $t = match op {
                FloatBinOp::Add => x + y,
                FloatBinOp::Sub => x - y,
                FloatBinOp::Mul => x * y,
                FloatBinOp::Div => x / y,
                FloatBinOp::Min => x.min(y),
                FloatBinOp::Max => x.max(y),
                FloatBinOp::Copysign => x.copysign(y),
            };
            $to(r)
        }};
    }
    match ty {
        Type::F32 => go!(f32, |v: u64| f32::from_bits(v as u32), |r: f32| r.to_bits() as u64),
        _ => go!(f64, f64::from_bits, |r: f64| r.to_bits()),
    }
}

fn float_un(ty: Type, op: FloatUnOp, a: u64) -> u64 {
    macro_rules! go {
        ($t:ty, $from:expr, $to:expr) => {{
            let x: $t = $from(a);
            let r: $t = match op {
                FloatUnOp::Neg => -x,
                FloatUnOp::Abs => x.abs(),
                FloatUnOp::Sqrt => x.sqrt(),
                FloatUnOp::Ceil => x.ceil(),
                FloatUnOp::Floor => x.floor(),
                FloatUnOp::Trunc => x.trunc(),
                FloatUnOp::Nearest => x.round_ties_even(),
            };
            $to(r)
        }};
    }
    match ty {
        Type::F32 => go!(f32, |v: u64| f32::from_bits(v as u32), |r: f32| r.to_bits() as u64),
        _ => go!(f64, f64::from_bits, |r: f64| r.to_bits()),
    }
}

/// A host declaration's narrow parameters (`gravity_abi::host::call_typed`'s mask): integers of 32 bits or fewer, which
/// a wasm32 host takes as `i32`. A `Ptr` is a 64-bit word like an `I64`: every host takes it as one (the host ABI is
/// i64 words, gravity_abi::host; gravity_lint's host_decls test holds each declared host to it).
fn host_narrow(params: &[Type]) -> u16 {
    params.iter().enumerate().fold(0, |m, (k, &t)| if matches!(t, Type::I32 | Type::I16 | Type::I8 | Type::Bool) { m | 1 << k } else { m })
}

/// A host declaration's result as `call_typed` reads it.
fn host_ret(ret: Option<Type>) -> gravity_abi::host::HostRet {
    use gravity_abi::host::HostRet;
    match ret {
        None => HostRet::Void,
        Some(Type::I64 | Type::Ptr) => HostRet::Word,
        Some(_) => HostRet::Narrow,
    }
}
