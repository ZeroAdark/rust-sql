//! Demotion of a fixed-register holder (`CompilerBase::init_assignment`,
//! the weighted fixed budget): a heavier value defined while the budget is
//! full spills the lightest classic holder and takes its fixed register.
//! The spill is a store emitted where the demotion happens, and the
//! compiler's linear register state ("on the stack") flows into every block
//! compiled afterwards; so the holder may only be demoted in its own
//! definition block, which dominates every use. Demoting it in one arm of a
//! diamond left the other arm's path without the store: the code after the
//! demoting arm reloaded an unwritten slot (the tpde arm's garbage group
//! keys of `partition_aggregate:79/81`, `mismatch-tail-handoff.md`).
//!
//! The shape: a loop header defines ten values read after a hot call in the
//! join (they cross the call: callee-saved fixed registers, with the two
//! pointer parameters all ten) and eight values read in the second arm
//! only (caller-saved fixed registers), so the fixed budget of sixteen is
//! full. The first arm defines a value that lives past an inner diamond
//! (five uses: heavier than any of the eight), so its `init_assignment`
//! runs the budget-full path and demotes the lightest holder — one of the
//! eight, read by the second arm. With the demotion in the first arm, the
//! second arm read the slot that only the first arm had stored.

mod common;
use gravity_ir::text::parse_program;
use gravity_ir::*;

extern "C" fn host_touch(p: *mut i64) {
    unsafe { *p += 1000 }
}

const NK: usize = 8;
const NM: usize = 8;

fn program() -> Program {
    let mut src = String::from(
        r#"
(program
  (host touch (ptr) void touches_memory)
  (fn f ((p ptr) (q ptr)) (i64)
    (block entry ((p ptr) (q ptr))
      (z (const i64 0))
      (br loop z z))
    (block loop ((i i64) (acc i64))
      (h (load i64 p 0))
      (n (load i64 p 248))
"#,
    );
    for k in 1..=NK {
        src.push_str(&format!("      (k{k} (load i64 p {}))\n", 8 * k));
    }
    for m in 1..=NM {
        src.push_str(&format!("      (m{m} (load i64 p {}))\n", 128 + 8 * m));
    }
    src.push_str(
        r#"
      (one (const i64 1))
      (bit (iand i one))
      (c (icmp eq bit one))
      (condbr c (arma) (armb)))
    (block arma ()
      (x0 (ineg h))
      (two (const i64 2))
      (bit2 (iand i two))
      (c2 (icmp eq bit2 two))
      (condbr c2 (a1) (a2)))
    (block a1 ()
      (br ajoin))
    (block a2 ()
      (br ajoin))
    (block ajoin ()
      (x1 (iadd x0 x0))
      (x2 (iadd x1 x0))
      (x3 (iadd x2 x0))
      (x4 (iadd x3 x0))
      (x5 (iadd x4 x0))
      (br join x5))
    (block armb ()
      (y0 (iadd i n))
"#,
    );
    let mut prev = "y0".to_string();
    for m in 1..=NM {
        src.push_str(&format!("      (y{m} (iadd {prev} m{m}))\n"));
        prev = format!("y{m}");
    }
    src.push_str(&format!(
        r#"
      (br join {prev}))
    (block join ((r i64))
      ((call_host touch q))
      (s0 (iadd h r))
"#
    ));
    let mut prev = "s0".to_string();
    for k in 1..=NK {
        src.push_str(&format!("      (t{k} (iadd k{k} k{k}))\n      (s{k} (iadd {prev} t{k}))\n"));
        prev = format!("s{k}");
    }
    src.push_str(&format!(
        r#"
      (acc1 (iadd acc {prev}))
      (one2 (const i64 1))
      (i1 (iadd i one2))
      (more (icmp slt i1 n))
      (condbr more (loop i1 acc1) (exit acc1)))
    (block exit ((res i64))
      (return res))))
"#
    ));
    parse_program(&src, &|name| match name {
        "touch" => Some(host_touch as extern "C" fn(*mut i64) as usize),
        _ => None,
    })
    .unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn fixed_holder_is_not_demoted_in_a_diamond_arm() {
    let p = program();
    // Cells 0..=8: h, k1..k8; cells 17..=24: m1..m8; cell 31: the loop bound.
    let mut cells: Vec<i64> = (0..31).map(|i| 0x5eed_0000_1000 + 7919 * i as i64).collect();
    let bound = 6i64;
    cells.push(bound);
    let mut touched = 0i64;
    let f = Func(0);
    let want = common::both(&p, f, &[cells.as_ptr() as u64, &mut touched as *mut i64 as u64]).unwrap();
    // The interpreter's answer, as a sanity check of the program itself.
    let mut acc = 0i64;
    for i in 0..bound {
        let r = if i & 1 == 1 { -6 * cells[0] } else { i + bound + cells[17..17 + NM].iter().sum::<i64>() };
        acc += cells[0] + r + cells[1..=NK].iter().map(|k| 2 * k).sum::<i64>();
    }
    assert_eq!(want, vec![acc as u64]);
    assert_eq!(touched, 2 * 1000 * bound, "the join's call ran once per iteration on both arms");
}
