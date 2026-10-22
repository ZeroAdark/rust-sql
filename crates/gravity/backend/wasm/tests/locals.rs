//! D3228: a function with more IR values than an engine compiles locals (V8 and wasmparser: 50 000 per function) still
//! compiles, because values whose live ranges never overlap share a local; and sharing never lets a later value clobber
//! one still live (a value carried around a loop and past the chain, a result nothing reads, a parameter nothing reads,
//! both float and integer chains), checked against the interpreter.

mod common;
use common::*;
use gravity_ir::text::parse_program;
use gravity_ir::*;

/// `(program ...)` text: a loop whose body is a chain of `n` integer values and `n / 4` float values, three trips.
fn wide(n: usize) -> String {
    let mut s = String::from(
        "(program\n  (fn wide ((a i64) (b i64)) (i64)\n    (block entry ((a i64) (b i64))\n      (zero (const i64 0))\n      (br loop zero zero))\n    (block loop ((acc i64) (i i64))\n      (x0 (iadd acc a))\n      (dead (imul a b))\n      (f0 (sitofp f64 i))\n      (fb (sitofp f64 b))\n",
    );
    for k in 1..n {
        let op = ["iadd x{p} a", "ixor x{p} b", "isub x{p} i"][k % 3].replace("{p}", &(k - 1).to_string());
        s += &format!("      (x{k} ({op}))\n");
    }
    for k in 1..n / 4 {
        // bounded (no infinity for fptosi): + f0 (the trip count) and - fb (b) by turns
        s += &format!("      (f{k} ({} f{} {}))\n", if k % 2 == 0 { "fadd" } else { "fsub" }, k - 1, if k % 2 == 0 { "f0" } else { "fb" });
    }
    let (xl, fl) = (n - 1, n / 4 - 1);
    s += &format!(
        "      (fi (fptosi i64 f{fl}))\n      (y (iadd x{xl} fi))\n      (one (const i64 1))\n      (next (iadd i one))\n      (three (const i64 3))\n      (done (icmp sge next three))\n      (condbr done (exit y b next) (loop y next)))\n    (block exit ((r i64) (bb i64) (unused i64))\n      (s (isub r bb))\n      (return s))))\n"
    );
    s
}

#[test]
fn a_function_wider_than_the_engines_locals_limit_compiles_and_agrees() {
    let n = 60_000;
    let p = parse_program(&wide(n), &|_| None).unwrap_or_else(|e| panic!("{e}"));
    assert!(p.func(Func(0)).values.len() > 70_000, "the function has more values than the locals limit");
    let mut rig = Rig::new();
    let (inst, _) = rig.instantiate(&p, Func(0));
    for args in [[3u64, 5], [u64::MAX, 1 << 40], [0, 0], [12345, 678]] {
        both(&mut rig, &p, &inst, Func(0), &args).expect("no trap");
    }
}

#[test]
fn shared_locals_keep_every_live_value() {
    // small enough to read: values live across the loop (a, b, i), a dead result, a dead parameter, float + int chains
    let p = parse_program(&wide(40), &|_| None).unwrap_or_else(|e| panic!("{e}"));
    let mut rig = Rig::new();
    let (inst, _) = rig.instantiate(&p, Func(0));
    for args in [[1u64, 2], [7, 9], [u64::MAX, 3]] {
        both(&mut rig, &p, &inst, Func(0), &args).expect("no trap");
    }
}
