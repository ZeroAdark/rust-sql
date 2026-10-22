//! The phi scratch register and a materialized constant (`phi_fuzz` seeds
//! 16729 and 1191, backend.md §6.7). An edge into a slot phi (a join
//! outside every loop: vetoed from pinning) whose incoming value is not in
//! a register reloads it through `PhiScratchReg`; with every register in
//! use the scratch takes the first non-fixed one. When that register
//! holds a constant spilled before the branch -- stack-valid, but a
//! constant is never stored (`spill_part`) so it has no slot -- the
//! scratch's restore must rematerialize it, not load from frame offset 0.
//!
//! The program: 26 holders loaded from `p`, then `c = 1` materialized
//! for a `mul` into x0 (freed when `p` died), so 27 registers are in use
//! at the diamond; the join's phi is a slot phi (a loop follows it, so the
//! join is outside every loop) fed a constant on one edge -- the edge
//! materializes it through the scratch, which takes x0. Everything is
//! read after the loop, so nothing is fixed (vetoed as live across the
//! loop) and nothing dies before the edge.
#![cfg(target_arch = "aarch64")]

mod common;
use gravity_ir::builder::ProgramBuilder;
use gravity_ir::*;

fn program() -> Program {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::Ptr], &[Type::I64]);
    let mut fb = pb.define(f);
    let [p]: [Value; 1] = fb.params().try_into().unwrap();
    let holders: Vec<Value> = (0..26).map(|k| fb.load(Type::I64, p, 8 * k)).collect();
    // `c` is materialized during the encode, after the product's result
    // took a register: each product reuses its operand's (the last use),
    // `p`'s x0 is released an instruction after its last load, and `c`
    // takes the lowest free register, x0.
    let m2 = fb.int_bin(IntBinOp::Mul, holders[1], holders[2]);
    let c = fb.iconst(Type::I64, 1);
    let m = fb.int_bin(IntBinOp::Mul, holders[0], c);
    let cond = fb.icmp(IntCond::Slt, m2, holders[2]);
    let join = fb.create_block(&[Type::I64]);
    let then_b = fb.create_block(&[]);
    let else_b = fb.create_block(&[]);
    fb.cond_br(cond, (then_b, &[]), (else_b, &[]));
    fb.switch_to(then_b);
    let k = fb.iconst(Type::I64, 5);
    fb.br(join, &[k]);
    fb.switch_to(else_b);
    fb.br(join, &[holders[2]]);
    fb.switch_to(join);
    let x = fb.block_params(join)[0];
    let header = fb.create_block(&[Type::I64]);
    let exit = fb.create_block(&[]);
    let z = fb.iconst(Type::I64, 0);
    fb.br(header, &[z]);
    fb.switch_to(header);
    fb.mark_loop_header(header);
    let i = fb.block_params(header)[0];
    let one = fb.iconst(Type::I64, 1);
    let i1 = fb.int_bin(IntBinOp::Add, i, one);
    let two = fb.iconst(Type::I64, 2);
    let more = fb.icmp(IntCond::Slt, i1, two);
    fb.cond_br(more, (header, &[i1]), (exit, &[]));
    fb.switch_to(exit);
    let mut a = fb.int_bin(IntBinOp::Add, x, c);
    a = fb.int_bin(IntBinOp::Add, a, m);
    a = fb.int_bin(IntBinOp::Add, a, m2);
    for &h in &holders[2..] {
        let t = fb.int_bin(IntBinOp::Mul, h, h);
        a = fb.int_bin(IntBinOp::Add, a, t);
        a = fb.int_bin(IntBinOp::Xor, a, h);
    }
    fb.ret(&[a]);
    pb.finish()
}

#[test]
fn scratch_over_a_spilled_constant_rematerializes() {
    let p = program();
    gravity_ir::verify::verify(&p).and_then(|()| gravity_ir::verify::verify_dominance(&p)).unwrap_or_else(|e| panic!("{} {}", e.func, e.message));
    let cells: Vec<i64> = (0..64).map(|k| 0x0102_0304_0506_0708u64.wrapping_mul(k as u64 + 1).rotate_left(k as u32 * 3) as i64).collect();
    let interp = gravity_interp::Interp::new(&p).run(Func(0), &[cells.as_ptr() as u64]).unwrap();
    let native = common::native(&p, Func(0), &[cells.as_ptr() as u64]);
    assert_eq!(native, interp, "native vs interpreter");
}
