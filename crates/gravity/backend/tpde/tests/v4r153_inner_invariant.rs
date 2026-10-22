//! V4R-153: the invariant of an inner loop — a value the outer (window) loop defines once per trip and the inner (row)
//! loop reads on every row — is nominated for a fixed register like a function-level invariant. Before, a value defined
//! inside any loop was skipped, TPDE's own rule refused it too (`definitions_in_childs`), and the row loop reloaded it
//! from its slot every row (TH14-pgrc2 p1: the per-window plane pointers and bounds, 22 values per row loop).
#![cfg(target_arch = "aarch64")] // names frontend::A64_IMM (the AArch64 frontend); x86_64 has frontend_x64

mod common;
use gravity_ir::text::parse_program;
use gravity_ir::Func;
use gravity_tpde::view::FnView;

/// An outer loop b1 (window, `i < n`) whose trip b2 defines `base = i * i` and `mix = base + n`, then an inner loop b3
/// (row, `j < n`) whose body b4 reads both on every row. `late = base + 1` is defined in the same trip but read only
/// after the inner loop (b5, once per window): not an inner invariant, so it stays with TPDE's own rule.
const PROGRAM: &str = "(program
  (fn p0 ((v0 i64)) (i64)
    (hints (loop b1 b3))
    (block b0 ((v0 i64))
      (v10 (const i64 0))
      (br b1 v10 v10))
    (block b1 ((v1 i64) (v2 i64))
      (v11 (icmp slt v1 v0))
      (condbr v11 (b2) (b6 v2)))
    (block b2 ()
      (v20 (imul v1 v1))
      (v21 (iadd v20 v0))
      (v22 (const i64 1))
      (v23 (iadd v20 v22))
      (br b3 v10 v2))
    (block b3 ((v3 i64) (v4 i64))
      (v12 (icmp slt v3 v0))
      (condbr v12 (b4) (b5 v4)))
    (block b4 ()
      (v30 (iadd v4 v20))
      (v31 (ixor v30 v21))
      (v32 (const i64 1))
      (v33 (iadd v3 v32))
      (br b3 v33 v31))
    (block b5 ((v5 i64))
      (v40 (iadd v5 v23))
      (v41 (const i64 1))
      (v42 (iadd v1 v41))
      (br b1 v42 v40))
    (block b6 ((v6 i64))
      (return v6))))";

/// The value block `b`'s instruction `i` defines (the parser renumbers values; blocks keep their order).
fn def(p: &gravity_ir::Program, b: usize, i: usize) -> usize {
    p.funcs[0].blocks[b].insts[i].result.expect("a value-defining instruction").index()
}

#[test]
fn an_inner_loops_per_window_invariant_is_nominated() {
    let p = parse_program(PROGRAM, &|_| Some(0)).unwrap_or_else(|e| panic!("{e}"));
    let ctx = mcx::MemoryContext::new("v4r153-view");
    let view = FnView::build(ctx.mcx(), &p.funcs[0], &gravity_tpde::frontend::A64_IMM);
    assert!(view.force_fixed[def(&p, 2, 0)], "base (defined per window, read every row) is nominated");
    assert!(view.force_fixed[def(&p, 2, 1)], "mix (defined per window, read every row) is nominated");
}

/// G4 (does not apply): a value defined per window and read once per window, after the row loop, is not an inner
/// invariant; nor is a value the row loop defines and reads itself.
#[test]
fn a_per_window_value_read_once_per_window_is_not_nominated() {
    let p = parse_program(PROGRAM, &|_| Some(0)).unwrap_or_else(|e| panic!("{e}"));
    let ctx = mcx::MemoryContext::new("v4r153-view");
    let view = FnView::build(ctx.mcx(), &p.funcs[0], &gravity_tpde::frontend::A64_IMM);
    assert!(!view.force_fixed[def(&p, 2, 3)], "late (read once per window) is left to TPDE's rule");
    assert!(!view.force_fixed[def(&p, 4, 0)], "a row value is the row loop's own");
}

#[test]
fn the_nested_loops_compute_the_same_natively_and_interpreted() {
    let p = parse_program(PROGRAM, &|_| Some(0)).unwrap_or_else(|e| panic!("{e}"));
    for n in [0u64, 1, 3, 17] {
        common::both(&p, Func(0), &[n]).unwrap();
    }
}
