//! The scan-shape gap lane (docs/design/gravity-simple/tpde-scangap.md):
//! a back-edge incoming computed into the phi's own register (`shadow_phi`,
//! no move on the latch), with one and with two latches and with a phi
//! read after the incoming's definition; and a loop-invariant pointer
//! loaded in the entry block that stays in a register through the loop
//! (`pick_fixed` nomination, no frame reload per iteration).

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

/// Disassembly of the compiled function as `(offset, text)` lines.
#[cfg(target_arch = "aarch64")]
fn disasm(p: &Program, name: &str) -> Vec<(i64, String)> {
    use mcx::MemoryContext;
    let f = Func(p.funcs.iter().position(|f| f.name == name).unwrap() as u32);
    let ctx = MemoryContext::new("scangap");
    let c = gravity_tpde::compile(ctx.mcx(), p).unwrap();
    let range = c.mapper().get_mapped_range().unwrap();
    let bytes = unsafe { range.bytes() };
    let off = c.entry(f) - range.address();
    let mut out = Vec::new();
    let mut i = off;
    while i + 4 <= bytes.len() {
        let w = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        let mut s = [0u8; 128];
        let ddi = tpde_disarm::decode(w);
        let line = tpde_disarm::da64_format(&ddi, &mut s).map(|n| String::from_utf8_lossy(&s[..n]).into_owned()).unwrap_or_else(|_| "??".into());
        out.push(((i - off) as i64, line));
        if w == 0xd65f03c0 {
            break;
        }
        i += 4;
    }
    out
}

/// Register-to-register moves (`orr xd, xzr, xs`) inside every backward
/// branch's range: the phi moves of the latches.
#[cfg(target_arch = "aarch64")]
fn latch_moves(lines: &[(i64, String)]) -> usize {
    let mut n = 0;
    for (off, line) in lines {
        let Some(rel) = line.strip_prefix("b #0x") else { continue };
        let rel = u64::from_str_radix(rel.trim(), 16).unwrap() as i64;
        if rel >= 0 {
            continue;
        }
        let target = off + rel;
        n += lines.iter().filter(|(o, l)| *o >= target && *o <= *off && l.starts_with("orr x") && l.contains(", xzr, x")).count();
    }
    n
}

/// Loads from the frame (`[x29, ...]`) anywhere in the function.
#[cfg(target_arch = "aarch64")]
fn frame_loads(lines: &[(i64, String)]) -> usize {
    lines.iter().filter(|(_, l)| l.starts_with("ld") && l.contains("[x29")).count()
}

/// `sum(0..n)`: both latch incomings (`i + 1`, `acc + i`) are defined in
/// the latch, used only there, and the phis are not read after them.
const SUM: &str = r#"
(program
  (fn sum ((n i64)) (i64)
    (block entry ((n i64))
      (zero (const i64 0))
      (br loop zero zero))
    (block loop ((i i64) (acc i64))
      (more (icmp slt i n))
      (condbr more (body) (exit)))
    (block body ()
      (acc2 (iadd acc i))
      (one (const i64 1))
      (i2 (iadd i one))
      (br loop i2 acc2))
    (block exit ()
      (return acc))))
"#;

/// Two latches: odd `i` adds, even `i` subtracts. Each latch's incomings
/// are computed into the phi registers; the second latch still finds the
/// phis fixed where the first left them.
const ODDEVEN: &str = r#"
(program
  (fn oddeven ((n i64)) (i64)
    (block entry ((n i64))
      (zero (const i64 0))
      (br loop zero zero))
    (block loop ((i i64) (acc i64))
      (more (icmp slt i n))
      (condbr more (body) (exit)))
    (block body ()
      (one (const i64 1))
      (bit (iand i one))
      (zero0 (const i64 0))
      (odd (icmp ne bit zero0))
      (condbr odd (add) (sub)))
    (block add ()
      (acc2 (iadd acc i))
      (one2 (const i64 1))
      (i2 (iadd i one2))
      (br loop i2 acc2))
    (block sub ()
      (acc3 (isub acc i))
      (one3 (const i64 1))
      (i3 (iadd i one3))
      (br loop i3 acc3))
    (block exit ()
      (return acc))))
"#;

/// The phi `acc` is read again after `t = acc + i` (by `acc2 = acc + t`),
/// so `t` must not take `acc`'s register; `acc2` may.
const READ_AFTER: &str = r#"
(program
  (fn readafter ((n i64)) (i64)
    (block entry ((n i64))
      (zero (const i64 0))
      (br loop zero zero))
    (block loop ((i i64) (acc i64))
      (more (icmp slt i n))
      (condbr more (body) (exit)))
    (block body ()
      (t (iadd acc i))
      (acc2 (iadd acc t))
      (one (const i64 1))
      (i2 (iadd i one))
      (br loop i2 acc2))
    (block exit ()
      (return acc))))
"#;

/// Fibonacci: the latch passes `(b, a + b)`, a parallel copy. `s = a + b`
/// must not take `b`'s register: the edge still reads `b` for the phi `a`.
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
      (condbr done (exit) (step)))
    (block step ()
      (s (iadd a b))
      (one2 (const i64 1))
      (k3 (isub k one2))
      (br loop b s k3))
    (block exit ()
      (return a))))
"#;

/// `ctx = [array pointer, n]`: the array pointer is loaded once in the
/// entry block and indexed in the loop.
const INVARIANT: &str = r#"
(program
  (fn dot ((ctx ptr)) (i64)
    (block entry ((ctx ptr))
      (arr (load ptr ctx 0))
      (n (load i64 ctx 8))
      (zero (const i64 0))
      (br loop zero zero))
    (block loop ((i i64) (acc i64))
      (more (icmp slt i n))
      (condbr more (body) (exit)))
    (block body ()
      (p (ptr_add arr i 8 0))
      (x (load i64 p 0))
      (acc2 (iadd acc x))
      (one (const i64 1))
      (i2 (iadd i one))
      (br loop i2 acc2))
    (block exit ()
      (return acc))))
"#;

#[test]
fn back_edge_incomings_computed_into_phi_registers() {
    let p = load(SUM);
    assert_eq!(run(&p, "sum", &[10])[0], 45);
    assert_eq!(run(&p, "sum", &[0])[0], 0);
    assert_eq!(run(&p, "sum", &[1000])[0], 499500);
    #[cfg(target_arch = "aarch64")]
    {
        let lines = disasm(&p, "sum");
        let text: String = lines.iter().map(|(o, l)| format!("{o:5}: {l}\n")).collect();
        assert_eq!(latch_moves(&lines), 0, "latch moves remain:\n{text}");
    }
}

#[test]
fn two_latches_both_compute_into_phi_registers() {
    let p = load(ODDEVEN);
    let expect = |n: i64| (0..n).fold(0i64, |a, i| if i & 1 == 1 { a + i } else { a - i }) as u64;
    for n in [0, 1, 2, 7, 100] {
        assert_eq!(run(&p, "oddeven", &[n as u64])[0], expect(n));
    }
    #[cfg(target_arch = "aarch64")]
    {
        let lines = disasm(&p, "oddeven");
        let text: String = lines.iter().map(|(o, l)| format!("{o:5}: {l}\n")).collect();
        assert_eq!(latch_moves(&lines), 0, "latch moves remain:\n{text}");
    }
}

#[test]
fn phi_read_after_the_incoming_keeps_its_register() {
    let p = load(READ_AFTER);
    let expect = |n: i64| (0..n).fold(0i64, |a, i| a + (a + i)) as u64;
    for n in [0, 1, 2, 7, 30] {
        assert_eq!(run(&p, "readafter", &[n as u64])[0], expect(n));
    }
}

#[test]
fn parallel_copy_keeps_the_phi_read_at_the_edge() {
    let p = load(FIB);
    let fib = |n: u64| (0..n).fold((0u64, 1u64), |(a, b), _| (b, a + b)).0;
    for n in [0, 1, 2, 12, 40] {
        assert_eq!(run(&p, "fib", &[n])[0], fib(n));
    }
    #[cfg(target_arch = "aarch64")]
    {
        // `k - 1` is computed into `k`'s register; the parallel copy keeps
        // its two moves.
        let lines = disasm(&p, "fib");
        let text: String = lines.iter().map(|(o, l)| format!("{o:5}: {l}\n")).collect();
        assert_eq!(latch_moves(&lines), 2, "latch moves:\n{text}");
    }
}

#[test]
fn invariant_pointer_stays_in_a_register() {
    let p = load(INVARIANT);
    let arr: Vec<i64> = (0..100).map(|i| i * 3).collect();
    let ctx: [i64; 2] = [arr.as_ptr() as i64, arr.len() as i64];
    assert_eq!(run(&p, "dot", &[ctx.as_ptr() as u64])[0], arr.iter().sum::<i64>() as u64);
    #[cfg(target_arch = "aarch64")]
    {
        let lines = disasm(&p, "dot");
        let text: String = lines.iter().map(|(o, l)| format!("{o:5}: {l}\n")).collect();
        assert_eq!(frame_loads(&lines), 0, "frame loads remain:\n{text}");
        assert_eq!(latch_moves(&lines), 0, "latch moves remain:\n{text}");
    }
}

/// Scan-gap pass 3, item 2: a branch on the `iand` / `ior` of two compares,
/// or of a compare and a `Bool` value, is one compare, one conditional
/// compare (`ccmp`) and one branch (`FnView::fuse_compares`, `ImmRules::ccmp`).
/// `both_cmps`: `a < b && c != 0`; `either`: `a < b || c == 7`; `with_flag`:
/// `(a + b) overflows-i32 && flag` (the predicated filter's raise guard:
/// the self-extension compare first, the flag as the `ccmp`); `flag_or`:
/// `flag || a == b`.
const CCMP: &str = r#"
(program
  (fn both_cmps ((a i64) (b i64) (c i64)) (i64)
    (block entry ((a i64) (b i64) (c i64))
      (lt (icmp slt a b))
      (z (const i64 0))
      (nz (icmp ne c z))
      (both (iand lt nz))
      (condbr both (yes) (no)))
    (block yes ()
      (one (const i64 1))
      (return one))
    (block no ()
      (zero (const i64 0))
      (return zero)))
  (fn either ((a i64) (b i64) (c i64)) (i64)
    (block entry ((a i64) (b i64) (c i64))
      (lt (icmp slt a b))
      (seven (const i64 7))
      (is7 (icmp eq c seven))
      (any (ior lt is7))
      (condbr any (yes) (no)))
    (block yes ()
      (one (const i64 1))
      (return one))
    (block no ()
      (zero (const i64 0))
      (return zero)))
  (fn with_flag ((a i64) (b i64) (f i64)) (i64)
    (block entry ((a i64) (b i64) (f i64))
      (z (const i64 0))
      (flag (icmp ne f z))
      (s (iadd a b))
      (n (itrunc i32 s))
      (w (sext i64 n))
      (ovf (icmp ne s w))
      (bad (iand ovf flag))
      (condbr bad (raise) (ok)))
    (block raise ()
      (m1 (const i64 -1))
      (return m1))
    (block ok ()
      (return s)))
  (fn flag_or ((a i64) (b i64) (f i64)) (i64)
    (block entry ((a i64) (b i64) (f i64))
      (z (const i64 0))
      (flag (icmp ne f z))
      (eq (icmp eq a b))
      (any (ior flag eq))
      (condbr any (yes) (no)))
    (block yes ()
      (one (const i64 1))
      (return one))
    (block no ()
      (zero (const i64 0))
      (return zero))))
"#;

#[test]
fn and_or_of_compares_fuse_to_ccmp() {
    let p = load(CCMP);
    let vals: [i64; 7] = [i64::MIN, -3, -1, 0, 1, 7, i64::MAX];
    for &a in &vals {
        for &b in &vals {
            for &c in &vals {
                let args = [a as u64, b as u64, c as u64];
                assert_eq!(run(&p, "both_cmps", &args)[0], (a < b && c != 0) as u64, "both_cmps {a} {b} {c}");
                assert_eq!(run(&p, "either", &args)[0], (a < b || c == 7) as u64, "either {a} {b} {c}");
                let s = a.wrapping_add(b);
                let ovf = s != s as i32 as i64;
                assert_eq!(run(&p, "with_flag", &args)[0], if ovf && c != 0 { -1i64 as u64 } else { s as u64 }, "with_flag {a} {b} {c}");
                assert_eq!(run(&p, "flag_or", &args)[0], (c != 0 || a == b) as u64, "flag_or {a} {b} {c}");
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    for (name, want_and) in [("both_cmps", true), ("either", true), ("with_flag", true), ("flag_or", true)] {
        let lines = disasm(&p, name);
        let text: String = lines.iter().map(|(o, l)| format!("{o:5}: {l}\n")).collect();
        assert_eq!(lines.iter().filter(|(_, l)| l.starts_with("ccmp ")).count(), 1, "{name}: one ccmp expected:\n{text}");
        // No materialized predicate: no `csinc` (cset) and no `and`/`orr`
        // of two flags feeding a `cbnz`.
        if want_and {
            assert!(!lines.iter().any(|(_, l)| l.starts_with("csinc ")), "{name}: predicate materialized:\n{text}");
        }
        assert!(!lines.iter().any(|(_, l)| l.starts_with("cbnz ") || l.starts_with("cbz ")), "{name}: register-test branch remains:\n{text}");
    }
}

/// Scan-gap pass 3, item 3: two 8-byte stores to consecutive cells of one
/// base are one `stp`; runs of zero byte stores widen to one zero store
/// (`FnView::fuse_stores`, `ImmRules::store_pairs`). The shape of the last
/// projection's output stores (values, then the null bytes).
const STORES: &str = r#"
(program
  (fn pairs ((p ptr) (q ptr) (a i64) (b i64)) (i64)
    (block entry ((p ptr) (q ptr) (a i64) (b i64))
      ((store p 0 a))
      ((store p 8 b))
      (z (const i8 0))
      ((store q 0 z))
      ((store q 1 z))
      ((store q 2 z))
      ((store q 3 z))
      (r (iadd a b))
      (return r)))
  (fn three ((p ptr) (a i64) (b i64) (c i64)) (i64)
    (block entry ((p ptr) (a i64) (b i64) (c i64))
      ((store p 16 a))
      (zero (const i64 0))
      ((store p 24 zero))
      ((store p 32 c))
      (z (const i8 0))
      ((store p 40 z))
      ((store p 41 z))
      (r (iadd a b))
      (return r))))
"#;

#[test]
fn consecutive_stores_pair_and_zero_stores_widen() {
    let p = load(STORES);
    let mut vals = [0i64; 2];
    let mut nulls = [0xffu8; 8];
    let r = run(&p, "pairs", &[vals.as_mut_ptr() as u64, nulls.as_mut_ptr() as u64, 5u64, (-9i64) as u64]);
    assert_eq!(r[0], (-4i64) as u64);
    assert_eq!(vals, [5, -9]);
    assert_eq!(nulls, [0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);
    let mut buf = [0x11u8; 48];
    let r = run(&p, "three", &[buf.as_mut_ptr() as u64, 7u64, 8u64, 9u64]);
    assert_eq!(r[0], 15);
    assert_eq!(i64::from_le_bytes(buf[16..24].try_into().unwrap()), 7);
    assert_eq!(i64::from_le_bytes(buf[24..32].try_into().unwrap()), 0);
    assert_eq!(i64::from_le_bytes(buf[32..40].try_into().unwrap()), 9);
    assert_eq!(&buf[40..42], &[0, 0]);
    assert_eq!(&buf[42..48], &[0x11; 6]);
    assert_eq!(&buf[..16], &[0x11; 16]);
    #[cfg(target_arch = "aarch64")]
    {
        let lines = disasm(&p, "pairs");
        let text: String = lines.iter().map(|(o, l)| format!("{o:5}: {l}\n")).collect();
        assert_eq!(lines.iter().filter(|(_, l)| l.starts_with("stp x")).count(), 1, "one stp expected:\n{text}");
        assert_eq!(lines.iter().filter(|(_, l)| l.starts_with("strb ") || l.starts_with("strh ")).count(), 0, "narrow zero stores remain:\n{text}");
        assert_eq!(lines.iter().filter(|(_, l)| l.starts_with("str wzr")).count(), 1, "one 4-byte zero store expected:\n{text}");
        let lines = disasm(&p, "three");
        let text: String = lines.iter().map(|(o, l)| format!("{o:5}: {l}\n")).collect();
        // `a, 0` pair (xzr as the second register), `c` alone, one `strh wzr`.
        assert_eq!(lines.iter().filter(|(_, l)| l.starts_with("stp x")).count(), 1, "one stp expected:\n{text}");
        assert!(lines.iter().any(|(_, l)| l.starts_with("stp x") && l.contains("xzr")), "xzr pair expected:\n{text}");
        assert_eq!(lines.iter().filter(|(_, l)| l.starts_with("strh wzr")).count(), 1, "one strh wzr expected:\n{text}");
    }
}
