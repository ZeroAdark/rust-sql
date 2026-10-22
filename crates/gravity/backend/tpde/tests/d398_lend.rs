//! D398 (TPDE-REGS, 09-24): v4's TH14-pgrc2 fold (`tpch-sf10-014.plan`, p1 on M1 8016394653f) ran out of scratch
//! registers at `addhi` (four operands and a result) with every GP register locked: FIXED 14 + PINNED 9 held, the four
//! operands locked, none left for the result. `select_reg` failed ("ran out of registers for scratch registers") and the
//! statement could not compile. It now lends a register held only by a fixed assignment the instruction neither reads nor
//! defines: saved to a fresh slot, used as the scratch, reloaded when the instruction ends. The fixture is that function
//! as the lowerer emitted it.
#![cfg(target_arch = "aarch64")]

mod common;
use gravity_ir::text::parse_program;
use mcx::MemoryContext;

/// The lends of a compile; none may sit inside a branch region (edge code: a terminator's phi moves), where the reload
/// at the instruction's end would run on one edge only.
/// Withhold x18 on every platform: Linux then has the 27 GP registers of macOS, where the starvation was found (on
/// Linux's 28 the stock budgets leave exactly the five an `addhi` needs).
fn as_on_macos() {
    tpde_compiler::TEST_WITHHELD.store(1 << 18, std::sync::atomic::Ordering::Relaxed);
}

fn lends_outside_regions(c: &gravity_tpde::Compiled<'_>) -> usize {
    use tpde_compiler::RegallocEvent;
    let (mut depth, mut lends) = (0i32, 0usize);
    for e in c.regalloc() {
        match e.event {
            RegallocEvent::RegionBegin => depth += 1,
            RegallocEvent::RegionEnd => depth -= 1,
            RegallocEvent::Lend => {
                assert_eq!(depth, 0, "a register lent inside a branch region (block {:?})", e.block);
                lends += 1;
            }
            _ => {}
        }
    }
    lends
}

#[test]
fn a_starved_instruction_borrows_a_fixed_register() {
    as_on_macos();
    let p = parse_program(include_str!("fixtures/th14_addhi_exhaustion.txt"), &|_| Some(0x1000)).unwrap_or_else(|e| panic!("{e}"));
    let ctx = MemoryContext::new("gravity-tpde-test");
    let lends = gravity_tpde::compile_recording(ctx.mcx(), &p).map(|c| lends_outside_regions(&c)).unwrap_or_else(|e| panic!("compile: {}", e.message()));
    assert!(lends > 0, "no register was lent: the fixture no longer starves the allocator");
}

/// Fourteen loop invariants read in every block of the body (the fixed budget), nine loop-carried phis (the pinned
/// budget) and an `addhi` over four fresh values: the result finds every GP register locked and borrows one. The lent
/// register's holder (an invariant or a phi) must read back unchanged after the instruction, on every trip: native
/// vs interpreter.
#[test]
fn a_lent_register_keeps_its_value() {
    use gravity_ir::builder::ProgramBuilder;
    use gravity_ir::*;
    as_on_macos();
    const NINV: usize = 20;
    const NACC: usize = 12;
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("lend", &[Type::Ptr, Type::I64], &[Type::I64]);
    let mut b = pb.define(f);
    let [p, n]: [Value; 2] = b.params().try_into().unwrap();
    let inv: Vec<Value> = (0..NINV).map(|k| b.load(Type::I64, p, 8 * k as i32)).collect();
    let header = b.create_block(&[Type::I64; NACC + 1]);
    b.mark_loop_header(header);
    let bodies: Vec<Block> = (0..4).map(|_| b.create_block(&[])).collect();
    let exit = b.create_block(&[]);
    let zero = b.iconst(Type::I64, 0);
    let mut init = vec![zero];
    init.extend((0..NACC).map(|k| inv[k]));
    b.br(header, &init);
    b.switch_to(header);
    let hp: Vec<Value> = b.block_params(header).to_vec();
    let (i, mut acc) = (hp[0], hp[1..].to_vec());
    let go = b.icmp(IntCond::Slt, i, n);
    b.cond_br(go, (bodies[0], &[]), (exit, &[]));
    for (s, &blk) in bodies.iter().enumerate() {
        b.switch_to(blk);
        for (k, &v) in inv.iter().enumerate() {
            let a = &mut acc[(k + s) % NACC];
            let x = b.int_bin(IntBinOp::Xor, *a, v);
            *a = b.int_bin(IntBinOp::Add, x, i);
        }
        if s == 1 {
            let x: Vec<Value> = (0..4).map(|k| b.load(Type::I64, p, 8 * (NINV + k) as i32)).collect();
            let x: Vec<Value> = x.iter().enumerate().map(|(k, &v)| b.int_bin(IntBinOp::Add, v, acc[k])).collect();
            let h = b.add_hi(x[0], x[1], x[2], x[3]);
            acc[0] = b.int_bin(IntBinOp::Add, acc[0], h);
            // The four operands stay live past the `addhi`, so its result cannot take one of their registers.
            for k in 0..4 {
                acc[k + 1] = b.int_bin(IntBinOp::Sub, acc[k + 1], x[k]);
            }
        }
        if s + 1 < bodies.len() {
            b.br(bodies[s + 1], &[]);
        } else {
            let one = b.iconst(Type::I64, 1);
            let i2 = b.int_bin(IntBinOp::Add, i, one);
            let mut next = vec![i2];
            next.extend(acc.iter().copied());
            b.br(header, &next);
        }
    }
    b.switch_to(exit);
    let mut r = hp[1];
    for &a in &hp[2..] {
        r = b.int_bin(IntBinOp::Xor, r, a);
    }
    for &v in &inv {
        r = b.int_bin(IntBinOp::Add, r, v);
    }
    b.ret(&[r]);
    drop(b);
    let prog = pb.finish();
    gravity_ir::verify::verify(&prog).unwrap_or_else(|e| panic!("{e:?}"));
    gravity_ir::verify::verify_dominance(&prog).unwrap_or_else(|e| panic!("{e:?}"));
    let ctx = MemoryContext::new("gravity-tpde-test");
    let lends = {
        let c = gravity_tpde::compile_recording(ctx.mcx(), &prog).unwrap_or_else(|e| panic!("compile: {}", e.message()));
        lends_outside_regions(&c)
    };
    assert!(lends > 0, "no register was lent: the shape no longer starves the allocator");
    let mut data: Vec<i64> = (0..(NINV + 4) as i64).map(|k| k.wrapping_mul(0x9E37_79B9_7F4A_7C15u64 as i64) ^ (k << 40)).collect();
    for trips in [0u64, 1, 2, 7, 100] {
        common::both(&prog, f, &[data.as_mut_ptr() as u64, trips]).unwrap_or_else(|t| panic!("interp trapped: {t:?}"));
    }
}
