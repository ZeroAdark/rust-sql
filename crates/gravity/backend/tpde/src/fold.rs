//! Constant folding before compilation (PERF-HEAPSCAN): an integer op, compare, conversion or select whose
//! operands (a select: its condition and the operand it picks) are constants is replaced by the constant it computes, in
//! a copy of the program that only the compiler sees (the lowerer's program, its IR pins and the interpreter keep the
//! original). A lowering's constant arithmetic (a rescale by `10^0`, a CASE arm's `0 x 10^4`, a NULL flag's `true xor
//! true`) then costs nothing at run time, its uses may take an immediate and its operands die. Semantics are the IR's
//! (`gravity_ir::IntBinOp`): arithmetic wraps at the type's width and a shift count is taken modulo it; a division is left
//! to run (a zero divisor is the lowerer's to guard). To a fixpoint, so chains fold.
use gravity_ir::{ConvOp, Function, IntBinOp, IntCond, Op, Program, Type};
use mcx::{pg_vec, Mcx, PgVec};

use crate::view::const_canon;

/// The program with its constant instructions folded, `None` when nothing folds (the caller compiles the original); the
/// constant tables in `m` (the compile's scratch).
pub fn program(p: &Program, m: Mcx<'_>) -> Option<Program> {
    let hit = |f: &Function| {
        let c = consts(f, m);
        f.blocks.iter().flat_map(|b| &b.insts).any(|i| i.result.is_some_and(|r| c[r.index()].is_none()) && eval(f, &c, &i.op, i.result.map_or(Type::I64, |r| f.values[r.index()])).is_some())
    };
    if !p.funcs.iter().any(hit) {
        return None;
    }
    let mut q = p.clone();
    q.funcs.iter_mut().for_each(|f| function(f, m));
    Some(q)
}

fn consts<'m>(f: &Function, m: Mcx<'m>) -> PgVec<'m, Option<(Type, u64)>> {
    let mut c = pg_vec![in m; None; f.values.len()];
    for i in f.blocks.iter().flat_map(|b| &b.insts) {
        if let (Some(r), Op::Const { ty, bits }) = (i.result, &i.op) {
            c[r.index()] = Some((*ty, *bits));
        }
    }
    c
}

/// `f`'s foldable instructions rewritten as `Op::Const` in place.
fn function(f: &mut Function, m: Mcx<'_>) {
    let mut c = consts(f, m);
    let mut changed = true;
    while changed {
        changed = false;
        for bi in 0..f.blocks.len() {
            for ii in 0..f.blocks[bi].insts.len() {
                let Some(r) = f.blocks[bi].insts[ii].result else { continue };
                let ty = f.values[r.index()];
                if c[r.index()].is_some() {
                    continue;
                }
                let Some(bits) = eval(f, &c, &f.blocks[bi].insts[ii].op, ty) else { continue };
                let bits = const_canon(ty, bits) as u64;
                f.blocks[bi].insts[ii].op = Op::Const { ty, bits };
                c[r.index()] = Some((ty, bits));
                changed = true;
            }
        }
    }
}

/// The constant `op` computes at result type `ty` when its operands are constants, else `None`.
fn eval(_f: &Function, c: &[Option<(Type, u64)>], op: &Op, ty: Type) -> Option<u64> {
    if !(ty == Type::Bool || ty.is_int()) {
        return None;
    }
    let k = |v: gravity_ir::Value| c[v.index()];
    match *op {
        Op::IntBin { op, a, b } => k(a).zip(k(b)).and_then(|(x, y)| bin(op, ty, x.1, y.1)),
        Op::Icmp { cond, a, b } => k(a).zip(k(b)).and_then(|(x, y)| cmp(cond, x.0, x.1, y.1)),
        Op::Select { cond, a, b } => k(cond).and_then(|s| k(if s.1 & 1 != 0 { a } else { b })).map(|x| x.1),
        Op::Conv { op: ConvOp::Zext, a, .. } => k(a).filter(|x| x.0 == Type::Bool || x.0.is_int()).map(|(t, x)| const_canon(t, x) as u64 & mask(t)),
        Op::Conv { op: ConvOp::Sext | ConvOp::Trunc, a, .. } => k(a).filter(|x| x.0 == Type::Bool || x.0.is_int()).map(|(t, x)| const_canon(t, x) as u64),
        _ => None,
    }
}

/// The low `ty`-width bits (all 64 for `I64`).
fn mask(ty: Type) -> u64 {
    match ty {
        Type::Bool => 1,
        Type::I8 => 0xff,
        Type::I16 => 0xffff,
        Type::I32 => 0xffff_ffff,
        _ => u64::MAX,
    }
}

/// An integer op on two constants of `ty`.
fn bin(op: IntBinOp, ty: Type, x: u64, y: u64) -> Option<u64> {
    let (xs, ys, m) = (const_canon(ty, x), const_canon(ty, y), mask(ty));
    let (xu, yu, w) = (xs as u64 & m, ys as u64 & m, m.count_ones() as u64);
    let int = ty != Type::Bool;
    Some(match op {
        IntBinOp::And => xu & yu,
        IntBinOp::Or => xu | yu,
        IntBinOp::Xor => xu ^ yu,
        IntBinOp::Add if int => xs.wrapping_add(ys) as u64,
        IntBinOp::Sub if int => xs.wrapping_sub(ys) as u64,
        IntBinOp::Mul if int => xs.wrapping_mul(ys) as u64,
        IntBinOp::Shl if int => xu << (yu % w),
        IntBinOp::Ushr if int => xu >> (yu % w),
        IntBinOp::Sshr if int => (xs >> (yu % w)) as u64,
        IntBinOp::Smin if int => xs.min(ys) as u64,
        IntBinOp::Smax if int => xs.max(ys) as u64,
        IntBinOp::Umin if int => xu.min(yu),
        IntBinOp::Umax if int => xu.max(yu),
        IntBinOp::Smulh if ty == Type::I64 => ((xs as i128 * ys as i128) >> 64) as u64,
        IntBinOp::Umulh if ty == Type::I64 => ((xu as u128 * yu as u128) >> 64) as u64,
        IntBinOp::Rotl if ty == Type::I64 => xu.rotate_left((yu % w) as u32),
        _ => return None,
    })
}

/// A compare of two constants of `ty` (integers, `Bool`, pointers).
fn cmp(cond: IntCond, ty: Type, x: u64, y: u64) -> Option<u64> {
    if !(ty == Type::Bool || ty.is_int() || ty == Type::Ptr) {
        return None;
    }
    let (xs, ys, m) = (const_canon(ty, x), const_canon(ty, y), mask(ty));
    let (xu, yu) = (xs as u64 & m, ys as u64 & m);
    Some(match cond {
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
    } as u64)
}

#[cfg(test)]
mod tests {
    use gravity_ir::builder::ProgramBuilder;
    use gravity_ir::{IntBinOp, IntCond, Op, Type};

    /// A chain of constant arithmetic folds to one constant; an op with a live operand, a division and the program the
    /// lowerer built stay as they were.
    #[test]
    fn constant_chains_fold_in_a_copy_and_live_or_dividing_ops_stay() {
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("f", &[Type::I64], &[Type::I64]);
        {
            let mut b = pb.define(f);
            let x = b.params()[0];
            let (one, hundred, zero, tenk) = (b.iconst(Type::I64, 1), b.iconst(Type::I64, 100), b.iconst(Type::I64, 0), b.iconst(Type::I64, 10000));
            let h = b.int_bin(IntBinOp::Mul, one, hundred); // 100
            let z = b.int_bin(IntBinOp::Smulh, zero, tenk); // 0
            let lt = b.icmp(IntCond::Slt, z, h); // true
            let s = b.select(lt, h, z); // the builder folds a constant condition only when it sees one: not here
            let d = b.int_bin(IntBinOp::Sdiv, h, one); // a division: left alone
            let n8 = b.iconst(Type::I8, -3);
            let w = b.conv(gravity_ir::ConvOp::Zext, Type::I64, n8); // 253 at build time already
            let live = b.int_bin(IntBinOp::Add, x, s);
            let t = b.int_bin(IntBinOp::Add, live, d);
            let t = b.int_bin(IntBinOp::Add, t, w);
            b.ret(&[t]);
        }
        let p = pb.finish();
        let ctx = mcx::MemoryContext::new("fold-test");
        let q = super::program(&p, ctx.mcx()).expect("something folds");
        let g = &q.funcs[0];
        let insts: alloc::vec::Vec<_> = g.blocks.iter().flat_map(|b| &b.insts).collect();
        let n_const = insts.iter().filter(|i| matches!(i.op, Op::Const { .. })).count();
        assert!(n_const > p.funcs[0].blocks.iter().flat_map(|b| &b.insts).filter(|i| matches!(i.op, Op::Const { .. })).count());
        assert!(insts.iter().any(|i| matches!(i.op, Op::IntBin { op: IntBinOp::Sdiv, .. })), "a division stays");
        assert_eq!(insts.iter().filter(|i| matches!(i.op, Op::IntBin { op: IntBinOp::Add, .. })).count(), 3, "live adds stay");
        let consts: alloc::vec::Vec<i64> = insts.iter().filter_map(|i| if let Op::Const { ty, bits } = i.op { Some(super::const_canon(ty, bits)) } else { None }).collect();
        assert!(consts.contains(&100) && consts.contains(&0), "1 x 100 and 0 x 10^4 folded: {consts:?}");
        assert_eq!(p.funcs[0].blocks.iter().flat_map(|b| &b.insts).filter(|i| matches!(i.op, Op::IntBin { op: IntBinOp::Mul, .. })).count(), 1, "the original keeps its ops");
        assert!(super::program(&q, ctx.mcx()).is_none(), "a folded program folds no further");
    }
}
