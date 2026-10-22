//! `Scratch` (docs/design/gravity-simple/compile-latency.md §2, lever 1d):
//! one scratch across compiles of different programs yields the same
//! code as fresh compiles, and the code runs.

mod common;

use gravity_ir::text::parse_program;
use gravity_ir::{Func, Program};
use mcx::MemoryContext;

fn load(src: &str) -> Program {
    parse_program(src, &|_| None).unwrap_or_else(|e| panic!("{e}"))
}

fn image(c: &gravity_tpde::Compiled<'_>) -> Vec<u8> {
    let r = c.mapper().get_mapped_range().unwrap();
    // SAFETY: nothing runs or writes the mapping while it is copied.
    unsafe { r.bytes() }.to_vec()
}

#[test]
fn scratch_reused_across_programs_matches_fresh_compiles() {
    let gcd = load(
        r#"
(program
  (fn gcd ((a i64) (b i64)) (i64)
    (block entry ((a i64) (b i64))
      (br loop a b))
    (block loop ((x i64) (y i64))
      (z (const i64 0))
      (done (icmp eq y z))
      (condbr done (exit x) (step x y)))
    (block step ((x2 i64) (y2 i64))
      (r (srem x2 y2))
      (br loop y2 r))
    (block exit ((g i64))
      (return g))))
"#,
    );
    let sum = load(
        r#"
(program
  (fn sum3 ((a i64) (b i64) (c i64)) (i64)
    (block entry ((a i64) (b i64) (c i64))
      (s (iadd a b))
      (t (iadd s c))
      (return t))))
"#,
    );
    let mut scratch = gravity_tpde::Scratch::new();
    for round in 0..3 {
        for (p, args, want) in [(&gcd, [48u64, 18, 0], 6u64), (&sum, [1, 2, 3], 6), (&gcd, [1071, 462, 0], 21)] {
            let ctx = MemoryContext::new("scratch-test");
            let with = gravity_tpde::compile_with(ctx.mcx(), p, &mut scratch).unwrap();
            let fresh_ctx = MemoryContext::new("scratch-test-fresh");
            let fresh = gravity_tpde::compile(fresh_ctx.mcx(), p).unwrap();
            assert_eq!(image(&with), image(&fresh), "round {round}: the scratch changes the code");
            assert_eq!(with.code_bytes(), fresh.code_bytes());
            let nargs = p.func(Func(0)).params.len();
            // SAFETY: the entry is `extern "C"` over 64-bit integer args.
            let got = unsafe {
                if nargs == 2 {
                    let f: unsafe extern "C" fn(u64, u64) -> u64 = core::mem::transmute(with.entry(Func(0)));
                    f(args[0], args[1])
                } else {
                    let f: unsafe extern "C" fn(u64, u64, u64) -> u64 = core::mem::transmute(with.entry(Func(0)));
                    f(args[0], args[1], args[2])
                }
            };
            assert_eq!(got, want, "round {round}");
            // The timing is filled only with a clock installed: zero here.
            assert_eq!(with.timing().total_ns, 0);
        }
    }
}
