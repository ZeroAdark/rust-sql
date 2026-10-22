//! Irreducible control flow through the dispatch transform (agreement
//! with the interpreter), and the typed refusal of a host whose address
//! is not a table index.

mod common;
use common::*;
use gravity_ir::builder::ProgramBuilder;
use gravity_ir::*;
use gravity_wasm::Refusal;

/// Two loop headers entering each other from the entry: neither dominates the other.
#[test]
fn irreducible_graph_is_dispatched() {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::Bool, Type::I64], &[Type::I64]);
    let mut b = pb.define(f);
    let [c, n]: [Value; 2] = b.params().try_into().unwrap();
    let a = b.create_block(&[Type::I64]);
    let d = b.create_block(&[Type::I64]);
    let exit = b.create_block(&[Type::I64]);
    b.cond_br(c, (a, &[n]), (d, &[n]));
    b.switch_to(a);
    let x = b.block_params(a)[0];
    let one = b.iconst(Type::I64, 1);
    let x1 = b.int_bin(IntBinOp::Sub, x, one);
    let z = b.iconst(Type::I64, 0);
    let done = b.icmp(IntCond::Sle, x1, z);
    b.cond_br(done, (exit, &[x1]), (d, &[x1]));
    b.switch_to(d);
    let y = b.block_params(d)[0];
    let two = b.iconst(Type::I64, 2);
    let y1 = b.int_bin(IntBinOp::Sub, y, two);
    let z2 = b.iconst(Type::I64, 0);
    let done2 = b.icmp(IntCond::Sle, y1, z2);
    b.cond_br(done2, (exit, &[y1]), (a, &[y1]));
    b.switch_to(exit);
    let r = b.block_params(exit)[0];
    b.ret(&[r]);
    let p = pb.finish();
    gravity_ir::verify::verify(&p).unwrap();
    let f = &p.funcs[0];
    let fixed = gravity_wasm::irreduce::make_reducible(f).ok().flatten().expect("the graph needs the transform");
    assert_eq!(fixed.funcs[0].blocks.len(), f.blocks.len() + 1, "one dispatch block");
    let mut rig = Rig::new();
    let (inst, _) = rig.instantiate(&p, Func(0));
    for c in [0u64, 1] {
        for n in [0i64, 1, 2, 3, 5, 10, 11] {
            both(&mut rig, &p, &inst, Func(0), &[c, n as u64]);
        }
    }
}

#[test]
fn native_host_address_is_refused() {
    extern "C" fn h(a: u64) -> u64 {
        a
    }
    let mut pb = ProgramBuilder::new();
    let host = pb.declare_host("h", &[Type::I64], Some(Type::I64), h as *const () as usize, HostFlags::default());
    let f = pb.declare_func("f", &[Type::I64], &[Type::I64]);
    let mut b = pb.define(f);
    let x = b.params()[0];
    let r = b.call_host(host, &[x], Some(Type::I64)).unwrap();
    b.ret(&[r]);
    let p = pb.finish();
    let got = gravity_wasm::compile(&p, Func(0));
    if (h as *const () as usize) > u32::MAX as usize {
        assert!(matches!(got, Err(Refusal::HostIndex { host: HostFn(0), .. })), "{got:?}");
    } else {
        assert!(got.is_ok());
    }
}
