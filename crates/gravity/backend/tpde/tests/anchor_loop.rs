//! The anchor steal and a loop around the anchor edge (`phi_fuzz` seed
//! 17648, backend.md §6.7). A phi's first compiled edge may take its
//! incoming value's register when the value is already on the stack,
//! and that edge then emits no move. When the edge lies inside a loop
//! the incoming is defined outside of, the edge runs again after the
//! phi's other edges (the inner loop's back edge) wrote the register,
//! and the phi starts its second round with the wrong value: seed 17648
//! called its host once too often; here the inner loop's parameter `w`
//! is summed, so the second outer round would add `u` twice instead of
//! `v + u`.
//!
//! The program: `v` loaded in the entry (live across both loops, read on
//! the inner loop's entry edge only, so nothing fixes it), an outer loop
//! over `n` whose body enters an inner loop of two rounds with `w = v`,
//! the inner back edge passing `w = u`; the result is the sum of every
//! `w`.
#![cfg(target_arch = "aarch64")]

mod common;
use gravity_ir::builder::ProgramBuilder;
use gravity_ir::*;

fn program() -> Program {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::Ptr, Type::I64], &[Type::I64]);
    let mut fb = pb.define(f);
    let [p, n]: [Value; 2] = fb.params().try_into().unwrap();
    let v = fb.load(Type::I64, p, 0);
    let u = fb.load(Type::I64, p, 8);
    let outer = fb.create_block(&[Type::I64, Type::I64]);
    let inner = fb.create_block(&[Type::I64, Type::I64, Type::I64]);
    let exit = fb.create_block(&[Type::I64]);
    let done = fb.create_block(&[Type::I64]);
    let z = fb.iconst(Type::I64, 0);
    fb.br(outer, &[z, z]);
    fb.switch_to(outer);
    fb.mark_loop_header(outer);
    let [i, acc]: [Value; 2] = fb.block_params(outer).try_into().unwrap();
    let z2 = fb.iconst(Type::I64, 0);
    fb.br(inner, &[z2, v, acc]);
    fb.switch_to(inner);
    fb.mark_loop_header(inner);
    let [j, w, acc2]: [Value; 3] = fb.block_params(inner).try_into().unwrap();
    let acc3 = fb.int_bin(IntBinOp::Add, acc2, w);
    let one = fb.iconst(Type::I64, 1);
    let j1 = fb.int_bin(IntBinOp::Add, j, one);
    let two = fb.iconst(Type::I64, 2);
    let more = fb.icmp(IntCond::Slt, j1, two);
    fb.cond_br(more, (inner, &[j1, u, acc3]), (exit, &[acc3]));
    fb.switch_to(exit);
    let acc4 = fb.block_params(exit)[0];
    let one2 = fb.iconst(Type::I64, 1);
    let i1 = fb.int_bin(IntBinOp::Add, i, one2);
    let again = fb.icmp(IntCond::Slt, i1, n);
    fb.cond_br(again, (outer, &[i1, acc4]), (done, &[acc4]));
    fb.switch_to(done);
    let r = fb.block_params(done)[0];
    fb.ret(&[r]);
    pb.finish()
}

#[test]
fn anchor_edge_inside_a_loop_keeps_its_move() {
    let p = program();
    gravity_ir::verify::verify(&p).and_then(|()| gravity_ir::verify::verify_dominance(&p)).unwrap_or_else(|e| panic!("{} {}", e.func, e.message));
    let cells: [i64; 2] = [1000, 1];
    for n in [1i64, 2, 3] {
        let interp = gravity_interp::Interp::new(&p).run(Func(0), &[cells.as_ptr() as u64, n as u64]).unwrap();
        let native = common::native(&p, Func(0), &[cells.as_ptr() as u64, n as u64]);
        assert_eq!(native, vec![(1001 * n) as u64], "n {n}: every outer round adds v + u");
        assert_eq!(native, interp, "n {n}: native vs interpreter");
    }
}
