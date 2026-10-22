//! The verifier's edge checks: a branch must pass exactly the target
//! block's parameters (count and types). The TPDE view turns block
//! parameters into phis with one incoming value per predecessor edge, and
//! the interpreter binds them by position, so an edge with too few or too
//! many arguments is a structural inconsistency no backend can execute
//! faithfully (the `array_more` re-entry of the fused recursion's consumer
//! walk once branched to a one-parameter block with no argument:
//! `tpde-phi-assert`).

use gravity_ir::builder::ProgramBuilder;
use gravity_ir::verify::verify;
use gravity_ir::*;

/// `f(p: ptr) -> i64`: a loop header `walk(w: i64)` re-entered from an
/// exit-side block through the edge `shape` builds.
fn walk_program(shape: impl Fn(&mut gravity_ir::builder::FunctionBuilder<'_>, Value, Value, Block, Block)) -> Program {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let p = b.params()[0];
    let w0 = b.conv(ConvOp::PtrToInt, Type::I64, p);
    let walk = b.create_block(&[Type::I64]);
    let take = b.create_block(&[]);
    let ended = b.create_block(&[]);
    let fin = b.create_block(&[]);
    b.br(walk, &[w0]);
    b.switch_to(walk);
    let w = b.block_params(walk)[0];
    let arr = b.conv(ConvOp::IntToPtr, Type::Ptr, w);
    let pos = b.load(Type::I64, arr, 0);
    let lim = b.load(Type::I64, arr, 8);
    let have = b.icmp(IntCond::Slt, pos, lim);
    b.cond_br(have, (take, &[]), (ended, &[]));
    b.switch_to(take);
    let one = b.iconst(Type::I64, 1);
    let pos1 = b.int_bin(IntBinOp::Add, pos, one);
    b.store(arr, 0, pos1);
    b.br(walk, &[w]);
    b.switch_to(ended);
    shape(&mut b, w, arr, walk, fin);
    b.switch_to(fin);
    let r = b.load(Type::I64, arr, 16);
    b.ret(&[r]);
    pb.finish()
}

#[test]
fn an_edge_passing_the_blocks_parameters_verifies() {
    let p = walk_program(|b, w, arr, walk, fin| {
        let z = b.iconst(Type::I64, 0);
        let more = b.load(Type::I64, arr, 24);
        let again = b.icmp(IntCond::Ne, more, z);
        b.cond_br(again, (walk, &[w]), (fin, &[]));
    });
    verify(&p).unwrap();
}

#[test]
fn an_edge_with_too_few_arguments_is_rejected() {
    let p = walk_program(|b, _w, arr, walk, fin| {
        let z = b.iconst(Type::I64, 0);
        let more = b.load(Type::I64, arr, 24);
        let again = b.icmp(IntCond::Ne, more, z);
        // The Q90 shape: the walk re-entered with no header argument.
        b.cond_br(again, (walk, &[]), (fin, &[]));
    });
    let e = verify(&p).unwrap_err();
    assert_eq!(e.func, "f");
    assert!(e.message.contains("has 0 args, block has 1 params"), "{}", e.message);
}

#[test]
fn an_edge_with_too_many_arguments_is_rejected() {
    let p = walk_program(|b, w, _arr, walk, _fin| b.br(walk, &[w, w]));
    let e = verify(&p).unwrap_err();
    assert!(e.message.contains("has 2 args, block has 1 params"), "{}", e.message);
}

#[test]
fn an_unconditional_edge_with_too_few_arguments_is_rejected() {
    let p = walk_program(|b, _w, _arr, walk, _fin| b.br(walk, &[]));
    let e = verify(&p).unwrap_err();
    assert!(e.message.contains("has 0 args, block has 1 params"), "{}", e.message);
}

#[test]
fn an_edge_argument_of_the_wrong_type_is_rejected() {
    let p = walk_program(|b, _w, _arr, walk, _fin| {
        let p8 = b.iconst(Type::I32, 7);
        b.br(walk, &[p8]);
    });
    let e = verify(&p).unwrap_err();
    assert!(e.message.contains("is I32 but"), "{}", e.message);
}

/// A use its definition does not dominate: `x` defined in one arm of a
/// diamond, read after the join (the register-state lane's Q85.2 shape,
/// a pointer computed on the hash path and read on the join). The
/// interpreter reads an unset slot, native code whatever the register
/// holds: the structural checks pass it, `verify_dominance` and (since
/// main 09-24) `verify` reject it.
#[test]
fn a_use_not_dominated_by_its_definition_is_rejected() {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let p = b.params()[0];
    let c0 = b.load(Type::I64, p, 0);
    let z = b.iconst(Type::I64, 0);
    let c = b.icmp(IntCond::Ne, c0, z);
    let arm = b.create_block(&[]);
    let join = b.create_block(&[]);
    b.cond_br(c, (arm, &[]), (join, &[]));
    b.switch_to(arm);
    let x = b.load(Type::I64, p, 8);
    b.br(join, &[]);
    b.switch_to(join);
    let y = b.int_bin(IntBinOp::Add, x, z);
    b.ret(&[y]);
    let prog = pb.finish();
    gravity_ir::verify::verify_structure(&prog).unwrap();
    let e = gravity_ir::verify::verify_dominance(&prog).unwrap_err();
    assert!(e.message.contains("does not dominate the use"), "{}", e.message);
    assert_eq!(verify(&prog).unwrap_err(), e);
}

/// The same diamond with the definition in the entry block verifies: the
/// entry dominates the join.
#[test]
fn a_dominated_use_across_a_diamond_verifies() {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let p = b.params()[0];
    let c0 = b.load(Type::I64, p, 0);
    let x = b.load(Type::I64, p, 8);
    let z = b.iconst(Type::I64, 0);
    let c = b.icmp(IntCond::Ne, c0, z);
    let arm = b.create_block(&[]);
    let join = b.create_block(&[Type::I64]);
    b.cond_br(c, (arm, &[]), (join, &[z]));
    b.switch_to(arm);
    let w = b.load(Type::I64, p, 16);
    b.br(join, &[w]);
    b.switch_to(join);
    let r = b.block_params(join)[0];
    let y = b.int_bin(IntBinOp::Add, x, r);
    b.ret(&[y]);
    let prog = pb.finish();
    verify(&prog).unwrap();
    gravity_ir::verify::verify_dominance(&prog).unwrap();
}

/// D3990: `store_zero_at` puts a zero and its store at a block's cursor taken earlier, before what the block gained
/// since, after what it held; the program verifies (the zero is defined before the store that reads it).
#[test]
fn a_zero_stored_at_an_earlier_cursor_lands_there() {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let p = b.params()[0];
    let first = b.load(Type::I64, p, 0);
    let at = b.cursor();
    let later = b.load(Type::I64, p, 8);
    b.store_zero_at(at, p, 16);
    let sum = b.int_bin(IntBinOp::Add, first, later);
    b.ret(&[sum]);
    let prog = pb.finish();
    verify(&prog).unwrap();
    let insts = &prog.funcs[0].blocks[at.0.index()].insts;
    assert!(matches!(insts[0].op, Op::Load { offset: 0, .. }), "{:?}", insts[0].op);
    assert!(matches!(insts[1].op, Op::Const { ty: Type::I64, bits: 0 }), "{:?}", insts[1].op);
    let z = insts[1].result.unwrap();
    assert!(matches!(insts[2].op, Op::Store { offset: 16, value, .. } if value == z), "{:?}", insts[2].op);
    assert!(matches!(insts[3].op, Op::Load { offset: 8, .. }), "{:?}", insts[3].op);
}
