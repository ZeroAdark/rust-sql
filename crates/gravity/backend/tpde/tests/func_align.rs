//! Every function of a compiled program starts at a 64-byte boundary of its mapping, whatever the host table holds (one
//! 16-byte PLT slot a symbol precedes the text): one more host declaration moved every function 16 bytes before, and an
//! IR-identical walk read 4 % slower from the placement alone. The functions' code and the distance between them do not
//! change with the host count.

use gravity_ir::text::parse_program;
use gravity_ir::{Func, Program};
use mcx::MemoryContext;

extern "C" fn host_add(a: i64, b: i64) -> i64 {
    a.wrapping_add(b)
}

fn program(hosts: usize) -> Program {
    let mut s = String::from("(program\n");
    for h in 0..hosts {
        s.push_str(&format!("  (host h{h} (i64 i64) i64 pure)\n"));
    }
    s.push_str(
        r#"  (fn f0 ((a i64) (b i64)) (i64)
    (block entry ((a i64) (b i64))
      (s (call_host h0 a b))
      (t (iadd s b))
      (return t)))
  (fn f1 ((a i64) (b i64) (c i64)) (i64)
    (block entry ((a i64) (b i64) (c i64))
      (s (iadd a b))
      (t (imul s c))
      (return t))))
"#,
    );
    parse_program(&s, &|_| Some(host_add as extern "C" fn(i64, i64) -> i64 as usize)).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn each_function_starts_on_a_cache_line_whatever_the_host_count() {
    let mut gaps = Vec::new();
    for hosts in 1..=6 {
        let p = program(hosts);
        let ctx = MemoryContext::new("func-align-test");
        let c = gravity_tpde::compile(ctx.mcx(), &p).unwrap_or_else(|e| panic!("compile: {e:?}"));
        let base = c.mapper().get_mapped_range().unwrap().address();
        let (e0, e1) = (c.entry(Func(0)), c.entry(Func(1)));
        assert_eq!((e0 - base) % 64, 0, "{hosts} hosts: f0 at {:#x}", e0 - base);
        assert_eq!((e1 - base) % 64, 0, "{hosts} hosts: f1 at {:#x}", e1 - base);
        assert_eq!(e0 % 64, 0, "{hosts} hosts: f0's address");
        gaps.push(e1 - e0);
        // SAFETY: both entries are `extern "C"` over 64-bit integer args; f0 calls host_add.
        let (r0, r1) = unsafe {
            let f0: unsafe extern "C" fn(i64, i64) -> i64 = core::mem::transmute(e0);
            let f1: unsafe extern "C" fn(i64, i64, i64) -> i64 = core::mem::transmute(e1);
            (f0(2, 3), f1(2, 3, 4))
        };
        assert_eq!((r0, r1), (8, 20), "{hosts} hosts");
    }
    assert!(gaps.windows(2).all(|w| w[0] == w[1]), "f1's distance from f0 moved with the host count: {gaps:?}");
}
