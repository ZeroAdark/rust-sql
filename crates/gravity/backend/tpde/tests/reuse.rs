//! `FnView::reuse_values`: a stack store read straight back and a duplicate load are the earlier value (rename map), and the program still runs.
#![cfg(target_arch = "aarch64")] // names frontend::A64_IMM (the AArch64 frontend); x86_64 has frontend_x64
mod common;
use gravity_ir::text::parse_program;
use gravity_ir::*;
use gravity_tpde::view::FnView;

const FWD: &str = r#"
(program
  (fn fwd ((a i64) (b i64)) (i64)
    (slots (32 8))
    (block entry ((a i64) (b i64))
      (p (stack_addr 0))
      ((store p 0 a))
      ((store p 8 b))
      (x (load i64 p 0))
      (y (load i64 p 8))
      (q (ptr_add p a 8 0))
      (l1 (load i64 q 0))
      (l2 (load i64 q 0))
      (s (iadd x y))
      (t (iadd l1 l2))
      (u (iadd s t))
      (return u))))
"#;

#[test]
fn a_stack_store_read_back_and_a_duplicate_load_are_renamed() {
    let p = parse_program(FWD, &|_| None).unwrap_or_else(|e| panic!("{e}"));
    let f = &p.funcs[0];
    let ctx = mcx::MemoryContext::new("reuse-view");
    let view = FnView::build(ctx.mcx(), f, &gravity_tpde::frontend::A64_IMM);
    let renamed: Vec<(u32, u32)> = (0..f.values.len() as u32).filter(|v| view.rename[*v as usize] != *v).map(|v| (v, view.rename[v as usize])).collect();
    eprintln!("values {} renamed {renamed:?}", f.values.len());
    let ntype = |v: u32| f.values[v as usize];
    let _ = ntype;
    // x -> a, y -> b, l2 -> l1: three renames.
    assert!(renamed.len() >= 3, "renames: {renamed:?}");
    let out = common::both(&p, Func(0), &[0, 5]).unwrap();
    assert_eq!(out, vec![5]);
}

const IMG: &str = r#"
(program
  (fn img ((a i64) (b i64) (c ptr)) (i64)
    (slots (32 8) (8 1))
    (block entry ((a i64) (b i64) (c ptr))
      (p (stack_addr 0))
      (q (stack_addr 1))
      (z (const i64 0))
      (t (icmp ne a z))
      ((store p 0 a))
      ((store q 0 t))
      ((store p 8 b))
      ((store q 1 t))
      (h (load ptr c 0))
      (k (ptr_add h a 16 0))
      ((prefetch k 0))
      (x (load i64 p 0))
      (y (load i64 p 8))
      (f (loadz i8 q 1))
      (s (iadd x y))
      (u (iadd s f))
      (return u))))
"#;

#[test]
fn the_group_image_staged_on_the_frame_is_read_back_from_registers() {
    let p = parse_program(IMG, &|_| None).unwrap_or_else(|e| panic!("{e}"));
    let f = &p.funcs[0];
    let ctx = mcx::MemoryContext::new("reuse-view");
    let view = FnView::build(ctx.mcx(), f, &gravity_tpde::frontend::A64_IMM);
    let renamed: Vec<(u32, u32)> = (0..f.values.len() as u32).filter(|v| view.rename[*v as usize] != *v).map(|v| (v, view.rename[v as usize])).collect();
    eprintln!("values {} renamed {renamed:?}", f.values.len());
    assert!(renamed.len() >= 2, "x -> a, y -> b: {renamed:?}");
    let cell = [0u64; 4];
    let out = common::both(&p, Func(0), &[3, 5, cell.as_ptr() as u64]).unwrap();
    assert_eq!(out, vec![9]);
}

