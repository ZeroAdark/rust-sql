//! D3698: the TPDE view build (`FnView::build_in`) stays linear in a function's cold blocks, its edge hints and its
//! loops. A join chain's probe pipeline holds a thousand loops, a cold detour per check and a likely hint per branch in
//! one function of twenty thousand blocks; the view looked each hint up by a scan of its list for every edge of every
//! frequency round, every successor and every cold block, and tested every loop per edge, per block and per value: a
//! 200-table chain spent 29 % of its compile there.
//!
//! Two shapes at a size where those scans cost seconds, each built under a bound the linear build meets with a wide
//! margin: one row loop with a cold detour and a likely hint per check (48,000 checks: 5 to 11 s a build before, 0.1 s
//! after), and a chain of sibling loops whose state passes from each loop's exit join into the next loop's header, with
//! the entry's values read in every loop (16,000 loops: 7.5 s before, 0.13 s after; Graviton, opt-level 1 or 3).
//! `D3698_N=<n>` resizes both for a by-hand run.

use std::time::{Duration, Instant};

use gravity_ir::builder::ProgramBuilder;
use gravity_ir::*;
use gravity_tpde::view::{FnView, NoImm};

/// The bound on one view build: over ten times the linear build of either shape (0.15 s at opt-level 1), under a third
/// of the quadratic one at these sizes. An unoptimized build (the dev profile's opt-level 0) runs the linear build ~20x
/// slower (2.7 s and 3.2 s measured on Graviton, 2026-10-20) and the quadratic one with it, so its bound is 10 s: about
/// three times the linear build, still a small fraction of the quadratic one.
const BOUND: Duration = Duration::from_millis(if cfg!(debug_assertions) { 10_000 } else { 2000 });

fn size(default: usize) -> usize {
    std::env::var("D3698_N").ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

/// One row loop over `n` checks: check `k` loads a cell and branches to a cold detour (a store) when it is zero, the
/// other edge marked likely; the detour rejoins the next check. The loop's latch sums the cells.
fn detour_loop(n: usize) -> Program {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::Ptr, Type::I64], &[Type::I64]);
    let mut fb = pb.define(f);
    let [p, len]: [Value; 2] = fb.params().try_into().unwrap();
    let header = fb.create_block(&[Type::I64, Type::I64]);
    let exit = fb.create_block(&[Type::I64]);
    let z = fb.iconst(Type::I64, 0);
    fb.br(header, &[z, z]);
    fb.switch_to(header);
    fb.mark_loop_header(header);
    let [i, acc]: [Value; 2] = fb.block_params(header).try_into().unwrap();
    let more = fb.icmp(IntCond::Slt, i, len);
    let first = fb.create_block(&[Type::I64]);
    fb.cond_br(more, (first, &[acc]), (exit, &[acc]));
    fb.set_likely(header, true);
    let mut check = first;
    for k in 0..n {
        fb.switch_to(check);
        let a = fb.block_params(check)[0];
        let x = fb.load(Type::I64, p, (8 * (k % 64)) as i32);
        let zero = fb.iconst(Type::I64, 0);
        let is_zero = fb.icmp(IntCond::Eq, x, zero);
        let cold = fb.create_block(&[]);
        let next = fb.create_block(&[Type::I64]);
        let a1 = fb.int_bin(IntBinOp::Add, a, x);
        fb.cond_br(is_zero, (cold, &[]), (next, &[a1]));
        fb.set_likely(check, false);
        fb.mark_cold(cold);
        fb.switch_to(cold);
        fb.store(p, 0, a);
        fb.br(next, &[a]);
        check = next;
    }
    fb.switch_to(check);
    let a = fb.block_params(check)[0];
    let one = fb.iconst(Type::I64, 1);
    let i1 = fb.int_bin(IntBinOp::Add, i, one);
    fb.br(header, &[i1, a]);
    fb.switch_to(exit);
    let r = fb.block_params(exit)[0];
    fb.ret(&[r]);
    pb.finish()
}

/// `n` sibling loops in sequence: loop `k`'s header takes (i, acc) from the join after loop `k - 1` (its exit edge's
/// state), its body reads the entry's pointer and length (live across every loop) with a likely-hinted check and a cold
/// detour, and its exit joins with the state for loop `k + 1`.
fn sibling_loops(n: usize) -> Program {
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::Ptr, Type::I64], &[Type::I64]);
    let mut fb = pb.define(f);
    let [p, len]: [Value; 2] = fb.params().try_into().unwrap();
    let z = fb.iconst(Type::I64, 0);
    let mut join = fb.create_block(&[Type::I64]);
    fb.br(join, &[z]);
    for k in 0..n {
        fb.switch_to(join);
        let acc0 = fb.block_params(join)[0];
        let header = fb.create_block(&[Type::I64, Type::I64]);
        let body = fb.create_block(&[]);
        let cold = fb.create_block(&[]);
        let latch = fb.create_block(&[]);
        let next = fb.create_block(&[Type::I64]);
        let z0 = fb.iconst(Type::I64, 0);
        fb.br(header, &[z0, acc0]);
        fb.switch_to(header);
        fb.mark_loop_header(header);
        let [i, acc]: [Value; 2] = fb.block_params(header).try_into().unwrap();
        let more = fb.icmp(IntCond::Slt, i, len);
        fb.cond_br(more, (body, &[]), (next, &[acc]));
        fb.set_likely(header, true);
        fb.switch_to(body);
        let x = fb.load(Type::I64, p, (8 * (k % 64)) as i32);
        let zero = fb.iconst(Type::I64, 0);
        let is_zero = fb.icmp(IntCond::Eq, x, zero);
        fb.cond_br(is_zero, (cold, &[]), (latch, &[]));
        fb.set_likely(body, false);
        fb.mark_cold(cold);
        fb.switch_to(cold);
        fb.store(p, 0, acc);
        fb.br(latch, &[]);
        fb.switch_to(latch);
        let one = fb.iconst(Type::I64, 1);
        let i1 = fb.int_bin(IntBinOp::Add, i, one);
        let acc1 = fb.int_bin(IntBinOp::Add, acc, x);
        fb.br(header, &[i1, acc1]);
        join = next;
    }
    fb.switch_to(join);
    let r = fb.block_params(join)[0];
    fb.ret(&[r]);
    pb.finish()
}

/// The fastest of three view builds of `p`'s function, and its loop count.
fn build(p: &Program) -> (Duration, usize) {
    gravity_ir::verify::verify(p).unwrap_or_else(|e| panic!("{} {}", e.func, e.message));
    let mut best = Duration::MAX;
    let mut loops = 0;
    let ctx = mcx::MemoryContext::new("view-build-linear");
    let mut view = FnView::new_in(ctx.mcx());
    for _ in 0..3 {
        let t = Instant::now();
        view.rebuild(&p.funcs[0], &NoImm);
        best = best.min(t.elapsed());
        loops = view.loops.len();
    }
    (best, loops)
}

#[test]
fn a_row_loop_of_many_cold_detours_builds_in_linear_time() {
    let n = size(48_000);
    let p = detour_loop(n);
    let (t, loops) = build(&p);
    eprintln!("detour_loop n {n}: {} blocks, {loops} loop(s), view build {t:?}", p.funcs[0].blocks.len());
    assert_eq!(loops, 1);
    assert!(t < BOUND, "the view build of {n} cold detours took {t:?} (bound {BOUND:?}): a hint lookup scans its list");
}

#[test]
fn a_chain_of_many_sibling_loops_builds_in_linear_time() {
    let n = size(16_000);
    let p = sibling_loops(n);
    let (t, loops) = build(&p);
    eprintln!("sibling_loops n {n}: {} blocks, {loops} loops, view build {t:?}", p.funcs[0].blocks.len());
    assert_eq!(loops, n);
    assert!(t < BOUND, "the view build of {n} sibling loops took {t:?} (bound {BOUND:?}): a test scans every loop");
}
