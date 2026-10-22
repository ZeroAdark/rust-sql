//! Differential harness: run a Gravity function natively (TPDE) and through
//! the interpreter, and compare.
#![allow(dead_code)]

use gravity_interp::{Interp, Trap};
use gravity_ir::builder::ProgramBuilder;
use gravity_ir::*;
use mcx::MemoryContext;

/// Append a wrapper `w` to `p` whose params and returns are all `I64`, so the
/// native entry can be called through a fixed `extern "C"` signature
/// regardless of the target function's float/narrow types.
fn with_wrapper(p: &Program, f: Func) -> (Program, Func) {
    let target = p.func(f);
    let mut pb = ProgramBuilder::new();
    for h in &p.host_fns {
        pb.declare_host(&h.name, &h.params, h.ret, h.addr, h.flags);
    }
    let mut funcs = Vec::new();
    for fun in &p.funcs {
        funcs.push(pb.declare_func(&fun.name, &fun.params, &fun.returns));
    }
    let nparams = target.params.len();
    let nret = target.returns.len();
    let w = pb.declare_func("w", &vec![Type::I64; nparams], &vec![Type::I64; nret]);
    {
        let mut b = pb.define(w);
        let params = b.params().to_vec();
        let mut args = Vec::new();
        for (i, &ty) in target.params.iter().enumerate() {
            let v = params[i];
            args.push(match ty {
                Type::I64 => v,
                Type::I8 | Type::I16 | Type::I32 => b.conv(ConvOp::Trunc, ty, v),
                Type::Bool => {
                    let z = b.iconst(Type::I64, 0);
                    b.icmp(IntCond::Ne, v, z)
                }
                Type::F64 => b.conv(ConvOp::Bitcast, Type::F64, v),
                Type::F32 => {
                    let t = b.conv(ConvOp::Trunc, Type::I32, v);
                    b.conv(ConvOp::Bitcast, Type::F32, t)
                }
                Type::Ptr => b.conv(ConvOp::IntToPtr, Type::Ptr, v),
                _ => unreachable!("vectors never cross a function boundary"),
            });
        }
        let mut outs = Vec::new();
        if nret <= 1 {
            let r = b.call(funcs[f.index()], &args, target.returns.first().copied());
            if let Some(r) = r {
                outs.push(to_i64(&mut b, target.returns[0], r));
            }
        } else {
            // Multi-value returns: call through a stack slot is not expressible;
            // instead inline by copying the callee body. Tests with two
            // returns use functions small enough to inline by hand, so route
            // them through `call2`.
            unreachable!("multi-return wrappers are built by with_wrapper2");
        }
        b.ret(&outs);
        let mut out = pb.finish();
        let m = out.mint();
        for (i, fun) in p.funcs.iter().enumerate() {
            out.funcs[i] = fun.clone_in(&m);
        }
        (out, w)
    }
}

fn to_i64(b: &mut gravity_ir::builder::FunctionBuilder, ty: Type, r: Value) -> Value {
    match ty {
        Type::I64 => r,
        Type::I8 | Type::I16 | Type::I32 | Type::Bool => b.conv(ConvOp::Sext, Type::I64, r),
        Type::F64 => b.conv(ConvOp::Bitcast, Type::I64, r),
        Type::F32 => {
            // The interpreter holds F32 as zero-extended bits.
            let t = b.conv(ConvOp::Bitcast, Type::I32, r);
            b.conv(ConvOp::Zext, Type::I64, t)
        }
        Type::Ptr => b.conv(ConvOp::PtrToInt, Type::I64, r),
        _ => unreachable!("vectors never cross a function boundary"),
    }
}

/// Run `f` natively. Functions with two return values must have integer-class
/// params and returns (called directly as `fn(...) -> Pair`).
pub fn native(p: &Program, f: Func, args: &[u64]) -> Vec<u64> {
    let nret = p.func(f).returns.len();
    let sig = (p.func(f).params.to_vec(), p.func(f).returns.to_vec());
    // Two float returns: the memory test's `(ptr, f) -> (f, f)` shape.
    if nret == 2 && sig.1[0].is_float() {
        let ctx = MemoryContext::new("gravity-tpde-test");
        let compiled = gravity_tpde::compile(ctx.mcx(), p).unwrap_or_else(|e| panic!("compile: {e:?}"));
        let addr = compiled.entry(f);
        assert_eq!(sig.0.len(), 2);
        return unsafe {
            if sig.1[0] == Type::F64 {
                // AAPCS64: HFA in d0/d1. System V: two SSE eightbytes in xmm0/xmm1.
                #[repr(C)]
                struct P(f64, f64);
                let fp: unsafe extern "C" fn(u64, f64) -> P = core::mem::transmute(addr);
                let r = fp(args[0], f64::from_bits(args[1]));
                vec![r.0.to_bits(), r.1.to_bits()]
            } else {
                #[cfg(target_arch = "aarch64")]
                {
                    // HFA of two floats: s0, s1.
                    #[repr(C)]
                    struct P(f32, f32);
                    let fp: unsafe extern "C" fn(u64, f32) -> P = core::mem::transmute(addr);
                    let r = fp(args[0], f32::from_bits(args[1] as u32));
                    vec![r.0.to_bits() as u64, r.1.to_bits() as u64]
                }
                #[cfg(target_arch = "x86_64")]
                {
                    // TPDE returns the second F32 in xmm1, but System V packs
                    // `(f32, f32)` into xmm0 alone. `(f32, f64)` classifies as
                    // two SSE eightbytes (xmm0, xmm1), and the low 32 bits of
                    // xmm1 are the second float.
                    #[repr(C)]
                    struct P(f32, f64);
                    let fp: unsafe extern "C" fn(u64, f32) -> P = core::mem::transmute(addr);
                    let r = fp(args[0], f32::from_bits(args[1] as u32));
                    vec![r.0.to_bits() as u64, r.1.to_bits() as u32 as u64]
                }
            }
        };
    }
    let (prog, entry) = if nret >= 2 {
        assert!(sig.0.iter().all(|t| !t.is_float()), "multi-return functions must take integer-class args");
        (p.clone(), f)
    } else {
        with_wrapper(p, f)
    };
    let ctx = MemoryContext::new("gravity-tpde-test");
    let compiled = gravity_tpde::compile(ctx.mcx(), &prog).unwrap_or_else(|e| panic!("compile: {e:?}"));
    let addr = compiled.entry(entry);
    assert!(args.len() <= 8, "unsupported arity {}", args.len());
    let mut a = [0u64; 8];
    a[..args.len()].copy_from_slice(args);
    let nret = prog.func(entry).returns.len();
    #[cfg(target_arch = "aarch64")]
    {
        // Integer-class AAPCS call with up to eight arguments and four results.
        let (r0, r1, r2, r3): (u64, u64, u64, u64);
        unsafe {
            core::arch::asm!(
                "blr {f}",
                f = in(reg) addr,
                inout("x0") a[0] => r0,
                inout("x1") a[1] => r1,
                inout("x2") a[2] => r2,
                inout("x3") a[3] => r3,
                in("x4") a[4],
                in("x5") a[5],
                in("x6") a[6],
                in("x7") a[7],
                clobber_abi("C"),
            );
        }
        let all = [r0, r1, r2, r3];
        assert!(nret <= 4);
        all[..nret].to_vec()
    }
    #[cfg(target_arch = "x86_64")]
    {
        // System V: six integer arguments in registers, the rest on the
        // stack (the compiler pushes them for us), results in rax:rdx. Three
        // or more integer results are not expressible in System V and TPDE's
        // x86-64 return builder refuses them.
        assert!(nret <= 2, "x86-64: at most two integer results");
        #[repr(C)]
        struct Pair(u64, u64);
        type F8 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64) -> Pair;
        let fp: F8 = unsafe { core::mem::transmute(addr) };
        let r = unsafe { fp(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7]) };
        let all = [r.0, r.1];
        all[..nret].to_vec()
    }
}

/// Interpreter result; when it succeeds, the native result must match.
pub fn both(p: &Program, f: Func, args: &[u64]) -> Result<Vec<u64>, Trap> {
    let interp = Interp::new(p).run(f, args);
    if let Ok(want) = &interp {
        let got = native(p, f, args);
        assert_eq!(&got, want, "native vs interpreter mismatch for {} args={args:?}", p.func(f).name);
    }
    interp
}
