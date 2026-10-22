//! F1 / F4 (D296's census): a module the code generator cannot compile past its asserts is the compile's error through the
//! code cache (`mapped_cached_all`: the admission refuses "backend" and the fallback takes the statement), never a panic out
//! of it. Two such modules, D296's two backend items: an edge into a function's entry block, and a use its definition does
//! not dominate. `gravity_ir::verify` refuses both since F2 (76bc6f92047, 3d92e2d62a1), so a lowering that runs it
//! (lowerv4's every lowering) never reaches the backend with them; the backend itself does not verify, and this test hands
//! it the modules directly, as a caller that skipped verify would: the containment is the second line behind the verifier.
use std::panic::{catch_unwind, AssertUnwindSafe};

use gravity_ir::builder::ProgramBuilder;
use gravity_ir::{Func, IntCond, Program, Type};

/// `f(ctx) -> i64` whose loop block branches back into the entry block (its one parameter passed, so the edge's arity
/// and types verify).
fn entry_edge() -> (Program, Func) {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let ctx = b.params()[0];
    let entry = b.entry();
    let (body, done) = (b.create_block(&[]), b.create_block(&[]));
    b.br(body, &[]);
    b.switch_to(body);
    let x = b.load(Type::I64, ctx, 0);
    let z = b.iconst(Type::I64, 0);
    let c = b.icmp(IntCond::Eq, x, z);
    b.cond_br(c, (done, &[]), (entry, &[ctx]));
    b.switch_to(done);
    b.ret(&[z]);
    drop(b);
    (pb.finish(), f)
}

/// `f(ctx) -> i64` returning a value defined on one arm of a branch only (no dominance).
fn use_before_def() -> (Program, Func) {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let ctx = b.params()[0];
    let (one, other, join) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
    let x = b.load(Type::I64, ctx, 0);
    let z = b.iconst(Type::I64, 0);
    let c = b.icmp(IntCond::Eq, x, z);
    b.cond_br(c, (one, &[]), (other, &[]));
    b.switch_to(one);
    let v = b.iconst(Type::I64, 7);
    b.br(join, &[]);
    b.switch_to(other);
    b.br(join, &[]);
    b.switch_to(join);
    b.ret(&[v]);
    drop(b);
    (pb.finish(), f)
}

#[test]
fn a_code_generator_panic_is_the_compiles_error() {
    let mut panicked = Vec::new();
    for (name, (p, f)) in [("entry edge", entry_edge()), ("use before def", use_before_def())] {
        // the first line: the verifier names it (F2) ...
        let refused = gravity_ir::verify::verify(&p).expect_err("the IR verifier refuses the module");
        let why = if name == "entry edge" { "the entry block" } else { "does not dominate the use" };
        assert!(refused.message.contains(why), "{name}: verify says {:?}", refused.message);
        // ... the second: the backend, handed it past the verifier, contains its own panic
        let ctx = mcx::MemoryContext::new("contain raw");
        let raw = catch_unwind(AssertUnwindSafe(|| gravity_tpde::compile(ctx.mcx(), &p).map(|_| ())));
        drop(ctx);
        let cached = catch_unwind(AssertUnwindSafe(|| gravity_backend::mapped_cached_all(&p, &[f]).map(|_| ())));
        let cached = cached.unwrap_or_else(|_| panic!("{name}: a panic came out of mapped_cached_all"));
        if raw.is_err() {
            let e = cached.expect_err("a panicking compile is an error");
            assert!(e.message().contains("the code generator panicked"), "{name}: {}", e.message());
            panicked.push(name);
        }
    }
    eprintln!("--stats the code generator panicked on: {panicked:?}");
    assert!(!panicked.is_empty(), "neither module reaches a code-generator assert: the test would prove nothing");
    // the cache still compiles and hits after a contained panic
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("ok", &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let k = b.iconst(Type::I64, 3);
    b.ret(&[k]);
    drop(b);
    let p = pb.finish();
    assert!(gravity_backend::mapped_cached_all(&p, &[f]).is_ok());
    assert!(gravity_backend::mapped_cached_all(&p, &[f]).is_ok_and(|(_, o)| o.hit()));
}
