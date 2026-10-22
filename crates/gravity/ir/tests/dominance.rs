//! `verify_dominance` against the definition of dominance, on random functions (M0-GATE, main 09-24: should verify check
//! dominance, and is the check exact). A use in a block the entry reaches is dominated when its definition comes earlier
//! in the same block, or when the entry cannot reach the use's block without passing through the definition's block.
//! Blocks the entry does not reach are not checked: TPDE's analyzer lays out the reverse postorder from the entry and
//! drops them, and the interpreter never branches there. A use of a value nothing defines is refused by name, never a
//! panic. The brute force below walks the CFG once per use; the verifier's answer must match it on every program.

use gravity_ir::builder::ProgramBuilder;
use gravity_ir::verify::{verify_dominance, verify_structure};
use gravity_ir::*;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// A function `f(p: ptr)` of `2..=9` blocks, no edge into the entry: each block loads or adds a few values from any
/// value built so far and ends in a return, a branch or a conditional branch to random non-entry blocks, passing a
/// random value to a block that takes one. Then a few operands are replaced by any value of the function, sometimes one
/// nothing defines.
fn random_function(r: &mut Rng) -> Program {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::Ptr], &[]);
    {
        let mut b = pb.define(f);
        let p = b.params()[0];
        let n = 2 + r.below(8);
        let mut blocks = vec![b.entry()];
        for _ in 1..n {
            blocks.push(if r.below(3) == 0 { b.create_block(&[Type::I64]) } else { b.create_block(&[]) });
        }
        let mut pool: Vec<Value> = Vec::new();
        for &bk in &blocks {
            b.switch_to(bk);
            pool.extend(b.block_params(bk).iter().copied().filter(|&v| v != p));
            for _ in 0..r.below(4) {
                let v = if pool.is_empty() || r.below(3) == 0 {
                    b.load(Type::I64, p, 8 * r.below(4) as i32)
                } else {
                    let (x, y) = (pool[r.below(pool.len())], pool[r.below(pool.len())]);
                    b.int_bin(IntBinOp::Add, x, y)
                };
                pool.push(v);
            }
        }
        for &bk in &blocks {
            b.switch_to(bk);
            let target = |r: &mut Rng, b: &mut gravity_ir::builder::FunctionBuilder<'_>| {
                let t = blocks[1 + r.below(n - 1)];
                let arg: Vec<Value> = if b.block_params(t).is_empty() { vec![] } else { vec![if pool.is_empty() { b.load(Type::I64, p, 0) } else { pool[r.below(pool.len())] }] };
                (t, arg)
            };
            match r.below(4) {
                0 => b.ret(&[]),
                1 => {
                    let (t, a) = target(r, &mut b);
                    b.br(t, &a);
                }
                _ => {
                    let c = b.load(Type::I64, p, 32); // the condition's type is `verify`'s business, not dominance's
                    let (t, a) = target(r, &mut b);
                    let (e, ea) = target(r, &mut b);
                    b.cond_br(c, (t, &a), (e, &ea));
                }
            }
        }
    }
    let mut prog = pb.finish();
    let func = &mut prog.funcs[0];
    if r.below(4) == 0 {
        // a value nothing defines, read by one add (when the function has one)
        let u = Value(func.values.len() as u32);
        func.values.push(Type::I64);
        let adds: Vec<(usize, usize)> = func.blocks.iter().enumerate()
            .flat_map(|(bi, b)| b.insts.iter().enumerate().filter(|(_, i)| matches!(i.op, Op::IntBin { .. })).map(move |(ii, _)| (bi, ii)))
            .collect();
        if !adds.is_empty() {
            let (bi, ii) = adds[r.below(adds.len())];
            if let Op::IntBin { b, .. } = &mut func.blocks[bi].insts[ii].op {
                *b = u;
            }
        }
    }
    let nv = func.values.len();
    for _ in 0..r.below(4) {
        let bi = r.below(func.blocks.len());
        let blk = &mut func.blocks[bi];
        if blk.insts.is_empty() {
            continue;
        }
        let ii = r.below(blk.insts.len());
        if let Op::IntBin { a, .. } = &mut blk.insts[ii].op {
            *a = Value(r.below(nv) as u32);
        }
    }
    prog
}

/// Blocks the entry reaches without entering `avoid` (none when `avoid` is the entry).
fn reach(f: &Function, avoid: Option<usize>) -> Vec<bool> {
    let mut seen = vec![false; f.blocks.len()];
    if avoid == Some(f.entry.index()) {
        return seen;
    }
    let mut stack = vec![f.entry.index()];
    seen[f.entry.index()] = true;
    while let Some(b) = stack.pop() {
        for e in f.blocks[b].term.edges() {
            let t = e.target.index();
            if !seen[t] && Some(t) != avoid {
                seen[t] = true;
                stack.push(t);
            }
        }
    }
    seen
}

/// Every use in a reachable block dominated by its definition, by the definition.
fn dominated_by_definition(f: &Function) -> bool {
    let mut def = vec![None; f.values.len()];
    for (bi, b) in f.blocks.iter().enumerate() {
        for &v in &b.params {
            def[v.index()] = Some((bi, 0));
        }
        for (ii, inst) in b.insts.iter().enumerate() {
            if let Some(v) = inst.result {
                def[v.index()] = Some((bi, ii + 1));
            }
        }
    }
    let reachable = reach(f, None);
    let ok = |v: Value, ub: usize, up: usize| match def.get(v.index()).copied().flatten() {
        None => false,
        Some((db, dp)) if db == ub => dp < up,
        Some((db, _)) => !reach(f, Some(db))[ub],
    };
    f.blocks.iter().enumerate().filter(|&(bi, _)| reachable[bi]).all(|(bi, b)| {
        let tp = b.insts.len() + 1;
        b.insts.iter().enumerate().all(|(ii, inst)| {
            let mut all = true;
            inst.op.for_each_operand(|v| all &= ok(v, bi, ii + 1));
            all
        }) && {
            let mut all = true;
            b.term.for_each_operand(|v| all &= ok(v, bi, tp));
            all
        }
            && b.term.edges().all(|e| e.args.iter().all(|&a| ok(a, bi, tp)))
    })
}

#[test]
fn verify_dominance_is_the_definition_on_random_functions() {
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut clean, mut broken, mut undefined) = (0, 0, 0);
    for i in 0..20_000 {
        let p = random_function(&mut r);
        verify_structure(&p).unwrap_or_else(|e| panic!("program {i}: the generator broke structure: {e:?}"));
        let want = dominated_by_definition(&p.funcs[0]);
        let got = verify_dominance(&p);
        assert_eq!(got.is_ok(), want, "program {i}: verify_dominance {got:?}, the definition says {want}\n{}", gravity_ir::text::print_program(&p));
        if let Err(e) = got {
            broken += 1;
            undefined += e.message.contains("is never defined") as usize;
        } else {
            clean += 1;
        }
    }
    // the generator reaches all three outcomes, so the comparison above is not vacuous
    assert!(clean > 1_000 && broken > 1_000 && undefined > 100, "clean {clean}, broken {broken}, never defined {undefined}");
}

/// `verify` runs the dominance check (main 09-24: lowerv4's every lowering calls `verify`, so an undominated use is a
/// `Reason::Bug` refusal there, never a program handed to TPDE): a well-typed function whose join reads a value one arm
/// of the diamond defines is refused by `verify` with the dominance message, and the same function reading it in the arm
/// verifies.
#[test]
fn verify_refuses_an_undominated_use() {
    let build = |read_in_join: bool| {
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("f", &[Type::Ptr], &[Type::I64]);
        {
            let mut b = pb.define(f);
            let p = b.params()[0];
            let c0 = b.load(Type::I64, p, 0);
            let c1 = b.load(Type::I64, p, 8);
            let c = b.icmp(IntCond::Ne, c0, c1);
            let arm = b.create_block(&[]);
            let join = b.create_block(&[Type::I64]);
            b.cond_br(c, (arm, &[]), (join, &[c0]));
            b.switch_to(arm);
            let x = b.load(Type::I64, p, 16);
            b.br(join, &[x]);
            b.switch_to(join);
            let r = if read_in_join { x } else { b.block_params(join)[0] };
            b.ret(&[r]);
        }
        pb.finish()
    };
    let e = gravity_ir::verify::verify(&build(true)).expect_err("the join reads the arm's value");
    assert_eq!(format!("fn {}: {}", e.func, e.message), "fn f: b2 terminator: v5 is defined in b1, which does not dominate the use");
    gravity_ir::verify::verify(&build(false)).unwrap();
}
