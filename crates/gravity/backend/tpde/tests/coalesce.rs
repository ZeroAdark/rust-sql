//! Phi coalescing corpus (`docs/design/gravity-simple/tpde-phi-coalescing.md`):
//! diamonds and loops whose phis have incoming values in registers, a value
//! passed to two phis, a phi whose incoming value stays live after the edge,
//! a nested loop, a switch into a block with parameters. Every program runs
//! natively and through the interpreter; on AArch64 the loops are also
//! checked for frame spills in the generated code.

mod common;
use gravity_ir::text::parse_program;
use gravity_ir::*;

fn load(src: &str) -> Program {
    parse_program(src, &|_| None).unwrap_or_else(|e| panic!("{e}"))
}
fn run(p: &Program, name: &str, args: &[u64]) -> Vec<u64> {
    let f = Func(p.funcs.iter().position(|f| f.name == name).unwrap() as u32);
    common::both(p, f, args).unwrap()
}

/// Frame stores and loads (`[x29, ...]` / `[sp, ...]` operands) in the
/// compiled function, and the disassembly.
#[cfg(target_arch = "aarch64")]
fn frame_traffic(p: &Program, name: &str) -> (usize, usize, String) {
    use mcx::MemoryContext;
    let f = Func(p.funcs.iter().position(|f| f.name == name).unwrap() as u32);
    let ctx = MemoryContext::new("coalesce");
    let c = gravity_tpde::compile(ctx.mcx(), p).unwrap();
    let range = c.mapper().get_mapped_range().unwrap();
    let bytes = unsafe { range.bytes() };
    let off = c.entry(f) - range.address();
    let (mut stores, mut loads, mut text) = (0, 0, String::new());
    let mut i = off;
    while i + 4 <= bytes.len() {
        let w = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        let mut s = [0u8; 128];
        let ddi = tpde_disarm::decode(w);
        let line = tpde_disarm::da64_format(&ddi, &mut s)
            .map(|n| String::from_utf8_lossy(&s[..n]).into_owned())
            .unwrap_or_else(|_| "??".into());
        let frame = (line.contains("[x29") || line.contains("[sp")) && !line.contains("x29, x30");
        if frame && line.starts_with("st") {
            stores += 1;
        }
        if frame && line.starts_with("ld") {
            loads += 1;
        }
        text.push_str(&format!("{:#06x}: {line}\n", i - off));
        if w == 0xd65f03c0 {
            break;
        }
        i += 4;
    }
    (stores, loads, text)
}

const GCD: &str = r#"
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
"#;

const FIB: &str = r#"
(program
  (fn fib ((n i64)) (i64)
    (block entry ((n i64))
      (zero (const i64 0))
      (one (const i64 1))
      (br loop zero one n))
    (block loop ((a i64) (b i64) (k i64))
      (z (const i64 0))
      (done (icmp eq k z))
      (condbr done (exit a) (step a b k)))
    (block step ((a2 i64) (b2 i64) (k2 i64))
      (s (iadd a2 b2))
      (one2 (const i64 1))
      (k3 (isub k2 one2))
      (br loop b2 s k3))
    (block exit ((r i64))
      (return r))))
"#;

/// A diamond whose arms pass values defined before the branch (both in
/// registers at the edges) and swap them; the join reads both.
const DIAMOND: &str = r#"
(program
  (fn dia ((a i64) (b i64)) (i64)
    (block entry ((a i64) (b i64))
      (x (iadd a b))
      (y (imul a b))
      (lt (icmp slt a b))
      (condbr lt (then x y) (else y x)))
    (block then ((t0 i64) (t1 i64))
      (br join t0 t1))
    (block else ((e0 i64) (e1 i64))
      (br join e0 e1))
    (block join ((p i64) (q i64))
      (d (isub p q))
      (return d))))
"#;

/// The same value passed to two phis of the loop header, and the incoming
/// values of the exit staying live after the edge (used by `tail`).
const TWICE: &str = r#"
(program
  (fn twice ((n i64)) (i64)
    (block entry ((n i64))
      (br loop n n n))
    (block loop ((i i64) (acc i64) (lim i64))
      (z (const i64 0))
      (done (icmp sle i z))
      (condbr done (exit acc lim i) (step i acc lim)))
    (block step ((i2 i64) (acc2 i64) (lim2 i64))
      (one (const i64 1))
      (i3 (isub i2 one))
      (acc3 (iadd acc2 i2))
      (br loop i3 acc3 lim2))
    (block exit ((r i64) (l i64) (last i64))
      (t (imul r l))
      (u (iadd t last))
      (return u))))
"#;

/// Nested loops: the inner header's initial value is the outer phi, and
/// the outer phi is live across the whole inner loop.
const NESTED: &str = r#"
(program
  (fn nest ((n i64) (m i64)) (i64)
    (block entry ((n i64) (m i64))
      (zero (const i64 0))
      (br outer zero zero))
    (block outer ((i i64) (acc i64))
      (done (icmp sge i n))
      (j0 (const i64 0))
      (condbr done (exit acc) (inner_head i acc j0)))
    (block inner_head ((i2 i64) (acc2 i64) (j i64))
      (jdone (icmp sge j m))
      (condbr jdone (next i2 acc2) (inner i2 acc2 j)))
    (block inner ((i3 i64) (acc3 i64) (j2 i64))
      (prod (imul i3 j2))
      (acc4 (iadd acc3 prod))
      (one (const i64 1))
      (j3 (iadd j2 one))
      (br inner_head i3 acc4 j3))
    (block next ((i4 i64) (acc5 i64))
      (one2 (const i64 1))
      (i5 (iadd i4 one2))
      (br outer i5 acc5))
    (block exit ((r i64))
      (return r))))
"#;

/// A switch whose cases target one block with parameters (split edges).
const SWITCH: &str = r#"
(program
  (fn sw ((k i64) (v i64)) (i64)
    (block entry ((k i64) (v i64))
      (one (const i64 1))
      (two (const i64 2))
      (w (iadd v one))
      (switch k (case 0 (out v one)) (case 1 (out w two)) (case 2 (out one w)) (default (out w w))))
    (block out ((p i64) (q i64))
      (r (imul p q))
      (return r))))
"#;

#[test]
fn diamond_phis_from_registers() {
    let p = load(DIAMOND);
    for (a, b) in [(1i64, 2i64), (5, 3), (0, 0), (-4, 7), (100, -100)] {
        let want = if a < b { (a + b) - a * b } else { a * b - (a + b) };
        assert_eq!(run(&p, "dia", &[a as u64, b as u64]), vec![want as u64]);
    }
}

#[test]
fn one_value_to_two_phis_and_live_after_edge() {
    let p = load(TWICE);
    for n in [0i64, 1, 2, 5, 10] {
        let mut i = n;
        let mut acc = n;
        while i > 0 {
            acc += i;
            i -= 1;
        }
        let want = acc * n + i;
        assert_eq!(run(&p, "twice", &[n as u64]), vec![want as u64]);
    }
}

#[test]
fn nested_loops() {
    let p = load(NESTED);
    for (n, m) in [(0i64, 0i64), (1, 1), (3, 4), (5, 2), (7, 7)] {
        let mut acc = 0;
        for i in 0..n {
            for j in 0..m {
                acc += i * j;
            }
        }
        assert_eq!(run(&p, "nest", &[n as u64, m as u64]), vec![acc as u64]);
    }
}

#[test]
fn switch_into_block_with_params() {
    let p = load(SWITCH);
    for (k, v) in [(0i64, 5i64), (1, 5), (2, 5), (3, 5), (-1, 9)] {
        let w = v + 1;
        let want = match k {
            0 => v * 1,
            1 => w * 2,
            2 => 1 * w,
            _ => w * w,
        };
        assert_eq!(run(&p, "sw", &[k as u64, v as u64]), vec![want as u64]);
    }
}

#[test]
fn gcd_and_fib_still_correct() {
    let g = load(GCD);
    assert_eq!(run(&g, "gcd", &[48, 18]), vec![6]);
    let f = load(FIB);
    assert_eq!(run(&f, "fib", &[12]), vec![144]);
}

/// The loops keep their phis in registers: no frame traffic at all. (The
/// nested loop's `n`/`m` arguments are not phis; TPDE's fixed-assignment
/// policy leaves the budget to values defined inside loops, so they are
/// spilled once and reloaded per iteration — two stores, two loads per
/// call of the function, none for a phi.)
#[cfg(target_arch = "aarch64")]
#[test]
fn loops_have_no_frame_spills() {
    for (src, name) in [(GCD, "gcd"), (FIB, "fib"), (TWICE, "twice"), (DIAMOND, "dia")] {
        let p = load(src);
        let (stores, loads, text) = frame_traffic(&p, name);
        eprintln!("--- {name} ---\n{text}");
        assert_eq!((stores, loads), (0, 0), "{name} spills to the frame:\n{text}");
    }
    let p = load(NESTED);
    let (stores, loads, text) = frame_traffic(&p, "nest");
    eprintln!("--- nest ---\n{text}");
    assert!(stores <= 2 && loads <= 2, "nest spills more than its two arguments:\n{text}");
}
