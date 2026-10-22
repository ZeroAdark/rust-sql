//! verify's messages with the site labels built only when a message is: every structural, type and dominance check
//! that names a site (`b<i> params`, `b<i> inst <j>`, `b<i> terminator`, `b<i> terminator: edge to b<k>`) broken once,
//! the whole text pinned. The expected strings are the messages of the eager labels (one `format!` per instruction and
//! definition before any error existed: 25 of verify's 46 µs per lowering, M1-WRITE's compile profile 09-24).

use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::verify::{verify, verify_dominance, verify_structure, VerifyError};
use gravity_ir::*;

/// `f(p: ptr) -> i64` built by `body`.
fn one(body: impl FnOnce(&mut FunctionBuilder<'_>, Value)) -> Program {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let p = b.params()[0];
    body(&mut b, p);
    pb.finish()
}

/// b0 loads two words and branches on their compare to b1(x: i64) with the first, or to b2, which passes the second.
fn diamond() -> Program {
    one(|b, p| {
        let x = b.load(Type::I64, p, 0);
        let y = b.load(Type::I64, p, 8);
        let lt = b.icmp(IntCond::Slt, x, y);
        let tail = b.create_block(&[Type::I64]);
        let other = b.create_block(&[]);
        b.cond_br(lt, (tail, &[x]), (other, &[]));
        b.switch_to(other);
        b.br(tail, &[y]);
        b.switch_to(tail);
        let r = b.block_params(tail)[0];
        b.ret(&[r]);
    })
}

/// A value defined in one arm (b1) of a diamond and read in the join (b2) by `read`.
fn undominated(read: impl FnOnce(&mut FunctionBuilder<'_>, Value, Value)) -> Program {
    one(|b, p| {
        let c0 = b.load(Type::I64, p, 0);
        let c1 = b.load(Type::I64, p, 8);
        let c = b.icmp(IntCond::Ne, c0, c1);
        let arm = b.create_block(&[]);
        let join = b.create_block(&[]);
        b.cond_br(c, (arm, &[]), (join, &[]));
        b.switch_to(arm);
        let x = b.load(Type::I64, p, 16);
        b.br(join, &[]);
        b.switch_to(join);
        read(b, x, c0);
    })
}

fn text(r: Result<(), VerifyError>) -> String {
    let e = r.expect_err("the program is broken");
    format!("fn {}: {}", e.func, e.message)
}

#[test]
fn every_site_label_reads_as_before() {
    let mut got = Vec::new();
    // verify_structure: block parameters and instruction results defined twice / out of range
    let mut p = diamond();
    let m = p.mint();
    p.funcs[0].blocks[2].params = m.slice_from(&p.funcs[0].blocks[1].params);
    got.push(text(verify_structure(&p)));
    let mut p = diamond();
    p.funcs[0].blocks[0].insts[1].result = p.funcs[0].blocks[0].insts[0].result;
    got.push(text(verify_structure(&p)));
    let mut p = diamond();
    p.funcs[0].blocks[0].insts[2].result = Some(Value(999));
    got.push(text(verify(&p)));
    // verify: an instruction's result type, operand types, call arguments
    let mut p = diamond();
    let v = p.funcs[0].blocks[0].insts[0].result.unwrap();
    p.funcs[0].values[v.index()] = Type::I32;
    got.push(text(verify(&p)));
    got.push(text(verify(&one(|b, p| {
        let x = b.load(Type::I64, p, 0);
        let s = b.int_bin(IntBinOp::Add, x, p);
        b.ret(&[s]);
    }))));
    let mut pb = ProgramBuilder::new();
    let g = pb.declare_func("g", &[Type::I64], &[Type::I64]);
    let f = pb.declare_func("f", &[Type::Ptr], &[Type::I64]);
    {
        let mut b = pb.define(g);
        let a = b.params()[0];
        b.ret(&[a]);
    }
    {
        let mut b = pb.define(f);
        let p = b.params()[0];
        let x = b.load(Type::I64, p, 0);
        let r = b.call(g, &[x, x], Some(Type::I64)).unwrap();
        b.ret(&[r]);
    }
    got.push(text(verify(&pb.finish())));
    // verify: terminators (a non-Bool condition, an edge argument's type, the return count)
    got.push(text(verify(&one(|b, p| {
        let x = b.load(Type::I64, p, 0);
        let t = b.create_block(&[]);
        b.cond_br(x, (t, &[]), (t, &[]));
        b.switch_to(t);
        b.ret(&[x]);
    }))));
    got.push(text(verify(&one(|b, p| {
        let t = b.create_block(&[Type::I64]);
        b.br(t, &[p]);
        b.switch_to(t);
        let r = b.block_params(t)[0];
        b.ret(&[r]);
    }))));
    got.push(text(verify(&one(|b, p| {
        let x = b.load(Type::I64, p, 0);
        b.ret(&[x, x]);
    }))));
    // verify_dominance: an instruction operand, a terminator operand, an edge argument
    got.push(text(verify_dominance(&undominated(|b, x, c0| {
        let y = b.int_bin(IntBinOp::Add, x, c0);
        b.ret(&[y]);
    }))));
    got.push(text(verify_dominance(&undominated(|b, x, _| b.ret(&[x])))));
    got.push(text(verify_dominance(&undominated(|b, x, _| {
        let fin = b.create_block(&[Type::I64]);
        b.br(fin, &[x]);
        b.switch_to(fin);
        let r = b.block_params(fin)[0];
        b.ret(&[r]);
    }))));
    let want = [
        "fn f: b2 params: value v4 defined twice",
        "fn f: b0 inst 1: value v1 defined twice",
        "fn f: b0 inst 2: value v999 out of range",
        "fn f: b0 inst 0: result type Some(I32), expected Some(I64)",
        "fn f: b0 inst 1: v1 is I64 but v0 is Ptr",
        "fn f: b0 inst 1: 2 args, callee has 1 params",
        "fn f: b0 terminator: v1 is I64, expected Bool",
        "fn f: b0 terminator: v0 is Ptr but v1 is I64",
        "fn f: b0 terminator: returns 2 values, function declares 1",
        "fn f: b2 inst 0: v4 is defined in b1, which does not dominate the use",
        "fn f: b2 terminator: v4 is defined in b1, which does not dominate the use",
        "fn f: b2 terminator: edge to b3: v4 is defined in b1, which does not dominate the use",
    ];
    assert_eq!(got, want);
}

/// No edge enters the entry block (D296's census, main 09-24): its parameters are the function's arguments, not phis,
/// and the TPDE view gives it none, so an edge into it reached the backend and panicked there. `verify_structure` names
/// it, for every terminator form and with or without entry parameters, and so do `verify` and the text loader.
#[test]
fn an_edge_into_the_entry_block_is_named() {
    let mut got = Vec::new();
    // a loop back to the entry: `br` passing the entry's parameter
    got.push(text(verify_structure(&one(|b, p| {
        let t = b.create_block(&[]);
        b.br(t, &[]);
        b.switch_to(t);
        let e = b.entry();
        b.br(e, &[p]);
    }))));
    // one arm of a `cond_br`, the other to an ordinary block; through `verify`
    got.push(text(verify(&one(|b, p| {
        let x = b.load(Type::I64, p, 0);
        let t = b.create_block(&[]);
        b.br(t, &[]);
        b.switch_to(t);
        let z = b.iconst(Type::I64, 0);
        let c = b.icmp(IntCond::Eq, x, z);
        let out = b.create_block(&[]);
        let e = b.entry();
        b.cond_br(c, (out, &[]), (e, &[p]));
        b.switch_to(out);
        b.ret(&[x]);
    }))));
    // an entry without parameters: the edge is still refused (a loop starts at a block of its own)
    let mut pb = ProgramBuilder::new();
    let g = pb.declare_func("g", &[], &[]);
    {
        let mut b = pb.define(g);
        let t = b.create_block(&[]);
        b.br(t, &[]);
        b.switch_to(t);
        let e = b.entry();
        b.br(e, &[]);
    }
    got.push(text(verify_structure(&pb.finish())));
    // the text loader verifies what it loads
    let src = "(program (fn h () () (block top () (br again)) (block again () (br top))))";
    got.push(gravity_ir::text::parse_program(src, &|_| Some(0)).map(|_| ()).expect_err("an edge into the entry"));
    let want = [
        "fn f: b1 terminator: edge to b0, the entry block (its parameters are the function's arguments)",
        "fn f: b1 terminator: edge to b0, the entry block (its parameters are the function's arguments)",
        "fn g: b1 terminator: edge to b0, the entry block (its parameters are the function's arguments)",
        "fn h: b1 terminator: edge to b0, the entry block (its parameters are the function's arguments)",
    ];
    assert_eq!(got, want);
    // the same loop through a block of its own verifies
    verify(&one(|b, p| {
        let head = b.create_block(&[Type::Ptr]);
        b.br(head, &[p]);
        b.switch_to(head);
        let q = b.block_params(head)[0];
        let x = b.load(Type::I64, q, 0);
        let z = b.iconst(Type::I64, 0);
        let c = b.icmp(IntCond::Eq, x, z);
        let out = b.create_block(&[]);
        b.cond_br(c, (out, &[]), (head, &[q]));
        b.switch_to(out);
        b.ret(&[x]);
    }))
    .unwrap();
}
