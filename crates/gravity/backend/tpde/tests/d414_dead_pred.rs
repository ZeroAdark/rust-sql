//! D414: a block the entry never reaches (the lowerer leaves such blocks: 58-120 in the live distinct-count, avg-by-key and TH17 row
//! bodies) that branches into a loop body must not hide the loop. Its dominator set is only itself, and intersected into
//! the live block it joins, it erased the header from every block past the join: the back edge vanished and the row loop
//! compiled as straight-line code at frequency 1 (every phi and invariant through a stack slot, its calls counted cold).
#![cfg(target_arch = "aarch64")] // names frontend::A64_IMM (the AArch64 frontend); x86_64 has frontend_x64

use gravity_ir::text::parse_program;
use gravity_tpde::view::FnView;

/// A counted loop b1 (header) -> b2 -> b3 (latch) -> b1, and a dead block b4 (no predecessor) that jumps to b3, the
/// latch's join. Without the fix b3's dominators lose b1, so b3 -> b1 is not a back edge.
const PROGRAM: &str = "(program
  (fn p0 ((v0 ptr)) (i64)
    (hints (loop b1))
    (block b0 ((v0 ptr))
      (v10 (const i64 0))
      (br b1 v10 v10))
    (block b1 ((v1 i64) (v2 i64))
      (v11 (load i64 v0 8))
      (v12 (icmp slt v1 v11))
      (condbr v12 (b2) (b5 v2)))
    (block b2 ()
      (v13 (load i64 v0 16))
      (v14 (iadd v2 v13))
      (br b3 v14))
    (block b3 ((v3 i64))
      (v15 (const i64 1))
      (v16 (iadd v1 v15))
      (br b1 v16 v3))
    (block b4 ()
      (v17 (const i64 7))
      (br b3 v17))
    (block b5 ((v4 i64))
      (return v4))))";

#[test]
fn a_dead_predecessor_does_not_hide_the_loop() {
    let p = parse_program(PROGRAM, &|_| Some(0)).unwrap_or_else(|e| panic!("{e}"));
    let f = &p.funcs[0];
    let ctx = mcx::MemoryContext::new("d414-view");
    let view = FnView::build(ctx.mcx(), f, &gravity_tpde::frontend::A64_IMM);
    let body = view.loops.iter().find(|(h, _)| *h == 1).map(|(_, b)| b).expect("header b1 has a loop entry");
    assert!(body[2] && body[3], "b2 and the latch b3 are in b1's loop: {body:?}");
    assert!(!body[4], "the dead block is not part of the loop");
    assert!(view.block_freq[3] > 1.0, "the latch runs per iteration, not once: {}", view.block_freq[3]);
    assert!(view.block_freq[2] > 1.0, "the body runs per iteration, not once: {}", view.block_freq[2]);
}
