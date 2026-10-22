//! tpde-fixedregs corpus (`docs/design/gravity-simple/tpde-fixedregs.md`):
//! the anchor side of phi coalescing (`FunctionSession::feed_phi`: a value
//! that only feeds a phi at its block's edge is defined into the phi's pin
//! mask, so the anchor steals it), values that stay live across a join
//! without being phis, and the immediate folds (a constant left operand,
//! a constant `ptr_add` index, identity ops). Every program runs natively
//! and through the interpreter; the allocator's event ledger
//! (`Compiled::regalloc`) is the witness for what the code does.

mod common;
use gravity_ir::text::parse_program;
use gravity_ir::*;
use tpde_compiler::RegallocEvent;

fn load(src: &str) -> Program {
    parse_program(src, &|_| None).unwrap_or_else(|e| panic!("{e}"))
}
fn run(p: &Program, name: &str, args: &[u64]) -> Vec<u64> {
    let f = Func(p.funcs.iter().position(|f| f.name == name).unwrap() as u32);
    common::both(p, f, args).unwrap()
}

/// The ledger of function 0, and its disassembly on AArch64 (empty
/// elsewhere).
fn ledger(p: &Program) -> (Vec<gravity_tpde::RegEvent>, String) {
    use mcx::MemoryContext;
    let ctx = MemoryContext::new("fixedregs");
    let c = gravity_tpde::compile_recording(ctx.mcx(), p).unwrap();
    let events = c.regalloc().to_vec();
    let mut text = c.regalloc_report(Func(0));
    #[cfg(target_arch = "aarch64")]
    {
        let range = c.mapper().get_mapped_range().unwrap();
        let bytes = unsafe { range.bytes() };
        let off = c.entry(Func(0)) - range.address();
        let mut i = off;
        while i + 4 <= bytes.len() {
            let w = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
            let mut s = [0u8; 128];
            let d = tpde_disarm::decode(w);
            let line = tpde_disarm::da64_format(&d, &mut s).map(|n| String::from_utf8_lossy(&s[..n]).into_owned()).unwrap_or_else(|_| "??".into());
            text.push_str(&format!("{:#06x}: {line}\n", i - off));
            if w == 0xd65f03c0 {
                break;
            }
            i += 4;
        }
    }
    (events, text)
}

fn count(events: &[gravity_tpde::RegEvent], kind: RegallocEvent, blocks: &[u32]) -> usize {
    events.iter().filter(|e| e.event == kind && e.block.is_some_and(|b| blocks.contains(&b))).count()
}

/// A loop whose body is a diamond; each arm defines the value it passes
/// to the join and reads it nowhere else. The first arm compiled is the
/// join's anchor: with `feed_phi` its value is defined into a register the
/// phi may take, so the anchor edge emits no move; the other arm moves.
/// Blocks: 0 entry, 1 loop, 2 odd, 3 even, 4 join, 5 exit.
const FEED: &str = r#"
(program
  (fn feed ((n i64)) (i64)
    (block entry ((n i64))
      (zero (const i64 0))
      (br loop n zero))
    (block loop ((i i64) (acc i64))
      (z (const i64 0))
      (done (icmp sle i z))
      (one (const i64 1))
      (low (iand i one))
      (odd (icmp ne low z))
      (condbr done (exit acc) (body i acc odd)))
    (block body ((i1 i64) (acc1 i64) (odd1 bool))
      (condbr odd1 (arm_odd i1 acc1) (arm_even i1 acc1)))
    (block arm_odd ((i2 i64) (acc2 i64))
      (three (const i64 3))
      (x (imul i2 three))
      (br join i2 acc2 x))
    (block arm_even ((i3 i64) (acc3 i64))
      (seven (const i64 7))
      (y (iadd i3 seven))
      (br join i3 acc3 y))
    (block join ((i4 i64) (acc4 i64) (p i64))
      (acc5 (iadd acc4 p))
      (one2 (const i64 1))
      (i5 (isub i4 one2))
      (br loop i5 acc5))
    (block exit ((r i64))
      (return r))))
"#;

fn feed_model(n: i64) -> i64 {
    let (mut i, mut acc) = (n, 0i64);
    while i > 0 {
        acc += if i & 1 != 0 { i * 3 } else { i + 7 };
        i -= 1;
    }
    acc
}

#[test]
fn feed_phi_parity() {
    let p = load(FEED);
    for n in [0i64, 1, 2, 3, 10, 17, 100] {
        assert_eq!(run(&p, "feed", &[n as u64]), vec![feed_model(n) as u64]);
    }
}

/// The anchor arm (the first of the two compiled: `arm_odd`, block 3 in
/// text order after `body`) emits no phi move for `x`; only the other arm
/// moves, and only into the one phi it cannot steal (its `i`/`acc`
/// pass-throughs are renamed by copy propagation). No frame traffic in
/// the loop.
#[cfg(target_arch = "aarch64")]
#[test]
fn feed_phi_anchor_emits_no_move() {
    let p = load(FEED);
    let (events, text) = ledger(&p);
    eprintln!("{text}");
    let loop_blocks = [1u32, 2, 3, 4, 5];
    assert_eq!(count(&events, RegallocEvent::Spill, &loop_blocks), 0, "spill in the loop:\n{text}");
    assert_eq!(count(&events, RegallocEvent::Reload, &loop_blocks), 0, "reload in the loop:\n{text}");
    assert_eq!(count(&events, RegallocEvent::PhiStore, &loop_blocks), 0, "stack phi in the loop:\n{text}");
    // Arm 3 (`arm_odd`) is the anchor: no move. Arm 4 moves `y` into `p`.
    assert_eq!(count(&events, RegallocEvent::PhiMove, &[3]), 0, "the anchor arm moves:\n{text}");
    assert!(count(&events, RegallocEvent::PhiMove, &[4]) <= 1, "the second arm moves more than `p`:\n{text}");
}

/// Values computed before a diamond and read after its join, without
/// being phis: they must survive the join. In this port they do so as
/// fixed assignments (exempt from the join's spill-all); the ledger shows
/// no spill.
const ACROSS: &str = r#"
(program
  (fn across ((a i64) (b i64)) (i64)
    (block entry ((a i64) (b i64))
      (s (iadd a b))
      (m (imul a b))
      (d (isub a b))
      (lt (icmp slt a b))
      (condbr lt (then) (else)))
    (block then ()
      (two (const i64 2))
      (t (imul s two))
      (br join t))
    (block else ()
      (three (const i64 3))
      (e (imul m three))
      (br join e))
    (block join ((p i64))
      (q (iadd p s))
      (r (isub q m))
      (w (iadd r d))
      (return w))))
"#;

#[test]
fn values_live_across_join() {
    let p = load(ACROSS);
    for (a, b) in [(1i64, 2i64), (5, 3), (0, 0), (-4, 7), (100, -100)] {
        let (s, m, d) = (a + b, a * b, a - b);
        let pp = if a < b { s * 2 } else { m * 3 };
        let want = pp + s - m + d;
        assert_eq!(run(&p, "across", &[a as u64, b as u64]), vec![want as u64]);
    }
    #[cfg(target_arch = "aarch64")]
    {
        let (events, text) = ledger(&p);
        eprintln!("{text}");
        assert_eq!(count(&events, RegallocEvent::Spill, &[0, 1, 2, 3]), 0, "a value live across the join spilled:\n{text}");
        assert_eq!(count(&events, RegallocEvent::Reload, &[0, 1, 2, 3]), 0, "a value live across the join reloaded:\n{text}");
    }
}

/// The immediate folds: a constant left operand of a commutative op, a
/// constant `ptr_add` index (into the `add` and into a load's offset), and
/// identity ops (`0 + x`, `x | 0`, `x << 0`).
const FOLDS: &str = r#"
(program
  (fn folds ((x i64) (p ptr)) (i64)
    (block entry ((x i64) (p ptr))
      (k (const i64 48))
      (a (iadd k x))
      (m (const i64 255))
      (b (iand m a))
      (z (const i64 0))
      (q (ptr_add p z 1 16))
      (v (load i64 q 0))
      (q2 (ptr_add p z 8 8))
      (r (ptrtoint q2))
      (c (iadd z b))
      (d (ior c z))
      (e (shl d z))
      (f (iadd e v))
      (g (isub f r))
      (h (isub g z))
      (return h))))
"#;

#[test]
fn immediate_folds() {
    let p = load(FOLDS);
    let mem: [i64; 4] = [11, 22, 33, 44];
    let base = mem.as_ptr() as u64;
    for x in [0i64, 1, 7, 200, -48, 1000] {
        let a = 48 + x;
        let b = 255 & a;
        let v = mem[2];
        let want = (b + v) - (base as i64 + 8);
        assert_eq!(run(&p, "folds", &[x as u64, base]), vec![want as u64]);
    }
    #[cfg(target_arch = "aarch64")]
    {
        let (_, text) = ledger(&p);
        eprintln!("{text}");
        // No constant is materialized: no `movz`; the load has the
        // folded offset; the identity ops are gone.
        assert!(!text.contains("movz"), "a folded constant was materialized:\n{text}");
        assert!(text.contains("[x1, #0x10]") || text.contains("[x1, #16]"), "the load did not take the folded offset:\n{text}");
        let body = text.lines().filter(|l| l.starts_with("0x")).count();
        assert!(body <= 9, "more instructions than the folds allow ({body}):\n{text}");
    }
}
