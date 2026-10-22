//! Cold-call save/restore (docs/design/gravity-simple/tpde-phi-coalescing.md
//! section 6, row "cold CallHost"): loop state may live in caller-saved
//! registers when every call in its range is cold; the call sequence saves
//! and restores those registers around the call.
//!
//! The `trash` host writes every caller-saved register it may touch before
//! returning, so a value the call sequence failed to save comes back wrong
//! and the interpreter oracle catches it.

mod common;
use gravity_ir::text::parse_program;
use gravity_ir::*;
use gravity_tpde::view::FnView;
use tpde_core::ir_adaptor::BlockCalls;

/// Decrements `*p` and returns 1 while it stays positive, 0 once it is not
/// (a page fetch: "more pages" / "done"). Clobbers the caller-saved
/// registers on AArch64 (argument registers x1-x7, temporaries x9-x15, the
/// vector argument and temporary registers).
#[cfg(target_arch = "aarch64")]
extern "C" fn host_trash(p: *mut i64) -> i64 {
    unsafe {
        core::arch::asm!(
            "mov x1, #-1", "mov x2, #-1", "mov x3, #-1", "mov x4, #-1", "mov x5, #-1", "mov x6, #-1", "mov x7, #-1",
            "mov x9, #-1", "mov x10, #-1", "mov x11, #-1", "mov x12, #-1", "mov x13, #-1", "mov x14, #-1", "mov x15, #-1",
            "movi v0.2d, #0xffffffffffffffff", "movi v1.2d, #0xffffffffffffffff", "movi v2.2d, #0xffffffffffffffff", "movi v3.2d, #0xffffffffffffffff",
            "movi v4.2d, #0xffffffffffffffff", "movi v5.2d, #0xffffffffffffffff", "movi v6.2d, #0xffffffffffffffff", "movi v7.2d, #0xffffffffffffffff",
            "movi v16.2d, #0xffffffffffffffff", "movi v17.2d, #0xffffffffffffffff", "movi v18.2d, #0xffffffffffffffff", "movi v19.2d, #0xffffffffffffffff",
            out("x1") _, out("x2") _, out("x3") _, out("x4") _, out("x5") _, out("x6") _, out("x7") _,
            out("x9") _, out("x10") _, out("x11") _, out("x12") _, out("x13") _, out("x14") _, out("x15") _,
            out("v0") _, out("v1") _, out("v2") _, out("v3") _, out("v4") _, out("v5") _, out("v6") _, out("v7") _,
            out("v16") _, out("v17") _, out("v18") _, out("v19") _,
            options(nostack),
        );
        *p -= 1;
        (*p > 0) as i64
    }
}
#[cfg(not(target_arch = "aarch64"))]
extern "C" fn host_trash(p: *mut i64) -> i64 {
    unsafe {
        *p -= 1;
        (*p > 0) as i64
    }
}
/// A cold call without a result: adds 1000 to `*p`.
extern "C" fn host_touch(p: *mut i64) {
    unsafe { *p += 1000 }
}
/// Eight arguments: every AAPCS argument register (x0-x7) is taken by the
/// call, so a pinned value must sit elsewhere.
extern "C" fn host_sum8(a: i64, b: i64, c: i64, d: i64, e: i64, f: i64, g: i64, h: i64) -> i64 {
    [a, b, c, d, e, f, g, h].iter().enumerate().fold(0i64, |s, (i, v)| s.wrapping_add(v.wrapping_mul(i as i64 + 1)))
}

fn load(src: &str) -> Program {
    parse_program(src, &|name| match name {
        "trash" => Some(host_trash as extern "C" fn(*mut i64) -> i64 as usize),
        "touch" => Some(host_touch as extern "C" fn(*mut i64) as usize),
        "sum8" => Some(host_sum8 as extern "C" fn(i64, i64, i64, i64, i64, i64, i64, i64) -> i64 as usize),
        _ => None,
    })
    .unwrap_or_else(|e| panic!("{e}"))
}
fn run(p: &Program, name: &str, args: &[u64]) -> Vec<u64> {
    let f = Func(p.funcs.iter().position(|f| f.name == name).unwrap() as u32);
    common::both(p, f, args).unwrap()
}
/// The view's call classification of the block named `block` in the text
/// `src` (blocks are created in textual order, the entry first).
fn block_calls(p: &Program, src: &str, block: &str) -> BlockCalls {
    let f = &p.funcs[0];
    #[cfg(target_arch = "aarch64")]
    let rules: &'static dyn gravity_tpde::view::ImmRules = &gravity_tpde::frontend::A64_IMM;
    #[cfg(target_arch = "x86_64")]
    let rules: &'static dyn gravity_tpde::view::ImmRules = &gravity_tpde::frontend_x64::X64_IMM;
    let ctx = mcx::MemoryContext::new("coldcall-view");
    let view = FnView::build(ctx.mcx(), f, rules);
    let b = src.match_indices("(block ").map(|(i, _)| &src[i + 7..]).position(|rest| rest.starts_with(block) && rest[block.len()..].starts_with(' ')).unwrap_or_else(|| panic!("no block {block}"));
    view.block_calls[b]
}

/// A row loop with a page-fetch-shaped cold call every 16 rows. `n`, `a`,
/// `b`, the counter slot address and the loop phis (`i`, `acc`) are live
/// across the call; the fetch's "done" answer leaves through a cold block
/// that reads the loop state (the raise/unwind shape).
const LOOP_FETCH: &str = r#"
(program
  (host trash (ptr) i64 touches_memory)
  (fn f ((n i64) (pages i64) (a i64) (b i64)) (i64)
    (slots (8 8))
    (hints (loop head) (freq fetch 0.01) (cold err))
    (block entry ((n i64) (pages i64) (a i64) (b i64))
      (p (stack_addr 0))
      ((store p 0 pages))
      (z (const i64 0))
      (br head z z))
    (block head ((i i64) (acc i64))
      (done (icmp sge i n))
      (condbr done (exit acc) (body i acc)))
    (block body ((i2 i64) (acc2 i64))
      (m (const i64 15))
      (lo (iand i2 m))
      (zz (const i64 0))
      (isf (icmp eq lo zz))
      (condbr isf (fetch i2 acc2) (step i2 acc2)))
    (block fetch ((i3 i64) (acc3 i64))
      (st (call_host trash p))
      (one (const i64 1))
      (ok (icmp eq st one))
      (condbr ok (step i3 acc3) (err i3 acc3)))
    (block step ((i4 i64) (acc4 i64))
      (t (imul i4 a))
      (u (iadd t b))
      (acc5 (iadd acc4 u))
      (one2 (const i64 1))
      (i5 (iadd i4 one2))
      (br head i5 acc5))
    (block err ((i6 i64) (acc6 i64))
      (v (isub acc6 i6))
      (w (imul v a))
      (x (iadd w n))
      (return x))
    (block exit ((r i64))
      (return r))))
"#;

#[test]
fn loop_state_across_a_cold_fetch_with_result() {
    let p = load(LOOP_FETCH);
    assert_eq!(block_calls(&p, LOOP_FETCH, "fetch"), BlockCalls { hot: false, cold: true });
    assert_eq!(block_calls(&p, LOOP_FETCH, "step"), BlockCalls::default());
    // Enough pages: the loop runs to `n`.
    for (n, a, b) in [(0i64, 3i64, 5i64), (1, 3, 5), (16, 7, -2), (100, 11, 13), (1000, -5, 9)] {
        let mut acc = 0i64;
        for i in 0..n {
            acc = acc.wrapping_add(i.wrapping_mul(a).wrapping_add(b));
        }
        assert_eq!(run(&p, "f", &[n as u64, 1_000_000, a as u64, b as u64]), vec![acc as u64], "n={n}");
    }
    // Few pages: the fetch answers "done" partway and the cold exit reads
    // the loop state.
    for (n, pages, a, b) in [(100i64, 1i64, 3i64, 5i64), (100, 3, 7, -2), (1000, 10, 11, 13)] {
        let mut acc = 0i64;
        let mut left = pages;
        let mut out = None;
        for i in 0..n {
            if i & 15 == 0 {
                left -= 1;
                if left <= 0 {
                    out = Some(acc.wrapping_sub(i).wrapping_mul(a).wrapping_add(n));
                    break;
                }
            }
            acc = acc.wrapping_add(i.wrapping_mul(a).wrapping_add(b));
        }
        assert_eq!(run(&p, "f", &[n as u64, pages as u64, a as u64, b as u64]), vec![out.unwrap() as u64], "n={n} pages={pages}");
    }
}

/// The cold call has no result and takes the counter; two cold calls in
/// one block (`touch` then `trash`); the eight-argument host in a second
/// cold block takes every argument register.
#[test]
fn cold_calls_without_result_and_with_all_argument_registers() {
    let src = r#"
(program
  (host trash (ptr) i64 touches_memory)
  (host touch (ptr) void touches_memory)
  (host sum8 (i64 i64 i64 i64 i64 i64 i64 i64) i64 pure)
  (fn f ((n i64) (a i64) (b i64) (c i64)) (i64 i64)
    (slots (8 8))
    (hints (loop head) (freq fetch 0.01) (freq wide 0.02))
    (block entry ((n i64) (a i64) (b i64) (c i64))
      (p (stack_addr 0))
      (big (const i64 1000000))
      ((store p 0 big))
      (z (const i64 0))
      (br head z z z))
    (block head ((i i64) (acc i64) (acc2 i64))
      (done (icmp sge i n))
      (condbr done (exit acc acc2) (body i acc acc2)))
    (block body ((i2 i64) (acc3 i64) (acc4 i64))
      (m (const i64 31))
      (lo (iand i2 m))
      (zz (const i64 0))
      (isf (icmp eq lo zz))
      (condbr isf (fetch i2 acc3 acc4) (body2 i2 acc3 acc4)))
    (block body2 ((i6 i64) (acc9 i64) (acc10 i64))
      (m2 (const i64 31))
      (lo2 (iand i6 m2))
      (seven (const i64 7))
      (isw (icmp eq lo2 seven))
      (condbr isw (wide i6 acc9 acc10) (step i6 acc9 acc10)))
    (block fetch ((i3 i64) (acc5 i64) (acc6 i64))
      ((call_host touch p))
      (st (call_host trash p))
      (acc7 (iadd acc5 st))
      (br step i3 acc7 acc6))
    (block wide ((i7 i64) (acc11 i64) (acc12 i64))
      (s (call_host sum8 i7 acc11 a b c n acc12 i7))
      (acc13 (iadd acc12 s))
      (br step i7 acc11 acc13))
    (block step ((i4 i64) (acc8 i64) (acc14 i64))
      (t (imul i4 a))
      (u (iadd t b))
      (v (iadd acc8 u))
      (w (imul i4 c))
      (x (iadd acc14 w))
      (one (const i64 1))
      (i5 (iadd i4 one))
      (br head i5 v x))
    (block exit ((r i64) (r2 i64))
      (cnt (load i64 p 0))
      (r3 (iadd r2 cnt))
      (return r r3))))
"#;
    let p = load(src);
    assert_eq!(block_calls(&p, src, "fetch"), BlockCalls { hot: false, cold: true });
    assert_eq!(block_calls(&p, src, "wide"), BlockCalls { hot: false, cold: true });
    for (n, a, b, c) in [(0i64, 3i64, 5i64, 2i64), (7, 3, 5, 2), (8, 3, 5, 2), (100, 7, -2, 3), (1000, 11, 13, -4)] {
        let (mut acc, mut acc2) = (0i64, 0i64);
        let mut cnt = 1_000_000i64;
        for i in 0..n {
            if i & 31 == 0 {
                cnt += 1000;
                cnt -= 1;
                acc = acc.wrapping_add((cnt > 0) as i64);
            } else if i & 31 == 7 {
                acc2 = acc2.wrapping_add(host_sum8(i, acc, a, b, c, n, acc2, i));
            }
            acc = acc.wrapping_add(i.wrapping_mul(a).wrapping_add(b));
            acc2 = acc2.wrapping_add(acc2.wrapping_mul(0)).wrapping_add(i.wrapping_mul(c));
        }
        assert_eq!(run(&p, "f", &[n as u64, a as u64, b as u64, c as u64]), vec![acc as u64, acc2.wrapping_add(cnt) as u64], "n={n}");
    }
}

/// Nested loops: the inner loop fetches (cold) every 8 rows and the outer
/// loop calls once per outer iteration (cold, 0.05); the outer state (`i`,
/// `total`) and the entry values are live across both.
#[test]
fn nested_loops_with_cold_calls_at_both_levels() {
    let src = r#"
(program
  (host trash (ptr) i64 touches_memory)
  (host touch (ptr) void touches_memory)
  (fn f ((n i64) (m i64) (a i64)) (i64)
    (slots (8 8))
    (hints (loop outer) (loop inner) (freq fetch 0.01) (freq per_outer 0.05) (cold err))
    (block entry ((n i64) (m i64) (a i64))
      (p (stack_addr 0))
      (big (const i64 1000000))
      ((store p 0 big))
      (z (const i64 0))
      (br outer z z))
    (block outer ((i i64) (total i64))
      (odone (icmp sge i n))
      (condbr odone (exit total) (per_outer i total)))
    (block per_outer ((i1 i64) (total1 i64))
      ((call_host touch p))
      (z2 (const i64 0))
      (br inner i1 total1 z2 z2))
    (block inner ((i2 i64) (total2 i64) (j i64) (acc i64))
      (idone (icmp sge j m))
      (condbr idone (onext i2 total2 acc) (ibody i2 total2 j acc)))
    (block ibody ((i3 i64) (total3 i64) (j2 i64) (acc2 i64))
      (msk (const i64 7))
      (lo (iand j2 msk))
      (zz (const i64 0))
      (isf (icmp eq lo zz))
      (condbr isf (fetch i3 total3 j2 acc2) (istep i3 total3 j2 acc2)))
    (block fetch ((i4 i64) (total4 i64) (j3 i64) (acc3 i64))
      (st (call_host trash p))
      (one (const i64 1))
      (ok (icmp eq st one))
      (condbr ok (istep i4 total4 j3 acc3) (err i4 total4 j3 acc3)))
    (block istep ((i5 i64) (total5 i64) (j4 i64) (acc4 i64))
      (t (imul i5 j4))
      (u (iadd t a))
      (acc5 (iadd acc4 u))
      (one2 (const i64 1))
      (j5 (iadd j4 one2))
      (br inner i5 total5 j5 acc5))
    (block onext ((i6 i64) (total6 i64) (acc6 i64))
      (total7 (iadd total6 acc6))
      (one3 (const i64 1))
      (i7 (iadd i6 one3))
      (br outer i7 total7))
    (block err ((i8 i64) (total8 i64) (j8 i64) (acc8 i64))
      (v (iadd total8 acc8))
      (w (isub v j8))
      (x (imul w i8))
      (y (isub x n))
      (return y))
    (block exit ((r i64))
      (return r))))
"#;
    let p = load(src);
    assert_eq!(block_calls(&p, src, "fetch"), BlockCalls { hot: false, cold: true });
    assert_eq!(block_calls(&p, src, "per_outer"), BlockCalls { hot: false, cold: true });
    for (n, m, a) in [(0i64, 5i64, 1i64), (3, 0, 1), (3, 5, 2), (10, 40, -3), (50, 33, 7)] {
        let mut total = 0i64;
        for i in 0..n {
            let mut acc = 0i64;
            for j in 0..m {
                acc = acc.wrapping_add(i.wrapping_mul(j).wrapping_add(a));
            }
            total = total.wrapping_add(acc);
        }
        assert_eq!(run(&p, "f", &[n as u64, m as u64, a as u64]), vec![total as u64], "n={n} m={m}");
    }
}

/// A per-row host call stays hot: a value live across it goes to a
/// callee-saved register (the classification, checked on the view) and
/// the program still matches.
#[test]
fn per_row_call_stays_hot() {
    let src = r#"
(program
  (host trash (ptr) i64 touches_memory)
  (fn f ((n i64) (a i64)) (i64)
    (slots (8 8))
    (hints (loop head))
    (block entry ((n i64) (a i64))
      (p (stack_addr 0))
      (big (const i64 1000000))
      ((store p 0 big))
      (z (const i64 0))
      (br head z z))
    (block head ((i i64) (acc i64))
      (done (icmp sge i n))
      (condbr done (exit acc) (body i acc)))
    (block body ((i2 i64) (acc2 i64))
      (st (call_host trash p))
      (t (imul i2 a))
      (u (iadd t st))
      (acc3 (iadd acc2 u))
      (one (const i64 1))
      (i3 (iadd i2 one))
      (br head i3 acc3))
    (block exit ((r i64))
      (return r))))
"#;
    let p = load(src);
    assert_eq!(block_calls(&p, src, "body"), BlockCalls { hot: true, cold: false });
    for (n, a) in [(0i64, 3i64), (1, 3), (100, 7), (1000, -5)] {
        let mut acc = 0i64;
        for i in 0..n {
            acc = acc.wrapping_add(i.wrapping_mul(a).wrapping_add(1));
        }
        assert_eq!(run(&p, "f", &[n as u64, a as u64]), vec![acc as u64], "n={n}");
    }
}

/// `GRAVITY_COLDCALL_DUMP=1`: print the AArch64 disassembly of `LOOP_FETCH`
/// (debug aid: the fetch block's `bl` should be bracketed by the stores and
/// loads of the pinned caller-saved registers, not preceded by per-row
/// spills).
#[cfg(target_arch = "aarch64")]
#[test]
fn dump_loop_fetch() {
    if std::env::var("GRAVITY_COLDCALL_DUMP").is_err() {
        return;
    }
    let p = load(LOOP_FETCH);
    let ctx = mcx::MemoryContext::new("coldcall-dump");
    let c = gravity_tpde::compile(ctx.mcx(), &p).unwrap();
    let range = c.mapper().get_mapped_range().unwrap();
    let bytes = unsafe { range.bytes() };
    let entry = c.entry(Func(0));
    let off = entry - range.address();
    let mut i = off;
    while i + 4 <= bytes.len() && i < off + 1024 {
        let w = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        let mut s = [0u8; 128];
        let ddi = tpde_disarm::decode(w);
        let text = tpde_disarm::da64_format(&ddi, &mut s).map(|n| String::from_utf8_lossy(&s[..n]).into_owned()).unwrap_or_else(|_| "??".into());
        eprintln!("{:#06x}: {w:08x}  {text}", i - off);
        i += 4;
    }
}
