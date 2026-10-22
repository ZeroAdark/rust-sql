//! An eviction inside a conditional edge's phi moves must not leak to the
//! other edge (`tpde-phi-coalescing.md` "Open defect", found by columnar6):
//! the moves of the edge that is not the fall-through are emitted inside
//! the branch region, and a register they take from a live value is
//! spilled on that edge only; the compiler's linear state then says "in
//! the stack slot" on the fall-through path too, where the store never
//! ran. The shape is `keyfold.rs`'s first word probe: a diamond
//! `condbr go (ld) (join 0)` under register pressure, `ld` the
//! fall-through, `join`'s phi fed a constant on the other edge, and values
//! defined before the branch read after the join (and some read only in
//! `ld`). Native and interpreter must agree for every pressure.

mod common;
use gravity_ir::text::parse_program;
use gravity_ir::*;

/// `holders` values live past the join, `fallthrough_only` values read
/// only in `ld`; `incoming` is the join phi's argument on the edge from
/// the loop block (a constant, or a value defined in the loop block and
/// used nowhere else); `swap` puts the join edge first in the condbr.
fn program(holders: usize, fallthrough_only: usize, incoming_value: bool, swap: bool) -> Program {
    let mut s = String::from(
        r#"
(program
  (fn f ((p ptr) (q ptr) (n i64)) (i64)
    (block entry ((p ptr) (q ptr) (n i64))
      (z (const i64 0))
      (br loop z))
    (block loop ((i i64))
      (row (load i64 p 0))
"#,
    );
    for k in 1..=holders {
        s.push_str(&format!("      (h{k} (load i64 p {}))\n", 8 * k));
    }
    for k in 1..=fallthrough_only {
        s.push_str(&format!("      (m{k} (load i64 p {}))\n", 512 + 8 * k));
    }
    s.push_str("      (mv (load i64 p 1024))\n      (one (const i64 1))\n      (bit (iand i one))\n      (c (icmp eq bit one))\n");
    let inc = if incoming_value { "mv" } else { "z" };
    if swap {
        s.push_str(&format!("      (condbr c (join {inc}) (ld)))\n"));
    } else {
        s.push_str(&format!("      (condbr c (ld) (join {inc})))\n"));
    }
    s.push_str("    (block ld ()\n");
    let mut prev = String::from("z");
    for k in 1..=fallthrough_only {
        s.push_str(&format!("      (s{k} (iadd {prev} m{k}))\n"));
        prev = format!("s{k}");
    }
    s.push_str(&format!("      (recl (iadd {prev} one))\n      (br join recl))\n"));
    s.push_str("    (block join ((rec i64))\n      (t0 (iadd row rec))\n");
    let mut prev = String::from("t0");
    for k in 1..=holders {
        s.push_str(&format!("      (t{k} (iadd {prev} h{k}))\n"));
        prev = format!("t{k}");
    }
    s.push_str(&format!(
        r#"      (a (load i64 q 0))
      (a1 (iadd a {prev}))
      ((store q 0 a1))
      (i1 (iadd i one))
      (more (icmp slt i1 n))
      (condbr more (loop i1) (exit)))
    (block exit ()
      (r (load i64 q 0))
      (return r))))
"#
    ));
    parse_program(&s, &|_| None).unwrap_or_else(|e| panic!("{e}\n{s}"))
}

fn check(holders: usize, fallthrough_only: usize, incoming_value: bool, swap: bool) {
    let p = program(holders, fallthrough_only, incoming_value, swap);
    let cells: Vec<i64> = (0..200).map(|i| 0x1000_0000 + 7919 * i as i64).collect();
    let n = 6i64;
    let mut acc = 0i64;
    let want = {
        let row = cells[0];
        let hs: i64 = (1..=holders).map(|k| cells[k]).sum();
        let ms: i64 = (1..=fallthrough_only).map(|k| cells[64 + k]).sum();
        let mv = cells[128];
        let mut total = 0i64;
        for i in 0..n {
            let go = i & 1 == 1;
            let rec = if (go && !swap) || (!go && swap) { ms + 1 } else if incoming_value { mv } else { 0 };
            total += row + rec + hs;
        }
        total
    };
    // The accumulator is in memory: each backend gets a fresh one.
    let interp = gravity_interp::Interp::new(&p).run(Func(0), &[cells.as_ptr() as u64, &mut acc as *mut i64 as u64, n as u64]).unwrap();
    assert_eq!(interp, vec![want as u64], "interpreter: holders {holders} fallthrough_only {fallthrough_only} incoming_value {incoming_value} swap {swap}");
    let mut acc2 = 0i64;
    let native = common::native(&p, Func(0), &[cells.as_ptr() as u64, &mut acc2 as *mut i64 as u64, n as u64]);
    assert_eq!(native, interp, "native vs interpreter: holders {holders} fallthrough_only {fallthrough_only} incoming_value {incoming_value} swap {swap}");
}

#[test]
fn const_incoming_under_pressure() {
    for (h, m) in [(12, 20), (12, 30), (16, 30), (4, 30), (0, 30), (24, 12)] {
        check(h, m, false, false);
    }
}

#[test]
fn value_incoming_under_pressure() {
    for (h, m) in [(12, 20), (12, 30), (16, 30), (4, 30), (0, 30), (24, 12)] {
        check(h, m, true, false);
    }
}

#[test]
fn join_edge_first_under_pressure() {
    for (h, m) in [(12, 20), (12, 30), (16, 30), (4, 30), (0, 30), (24, 12)] {
        check(h, m, false, true);
        check(h, m, true, true);
    }
}

extern "C" fn host_touch(p: *mut i64) {
    unsafe { *p += 1000 }
}

/// The columnar e2e's failing allocation (`tpde-phi-coalescing.md` "Open
/// defect"; the twin `count(*) FROM (SELECT DISTINCT k FROM h)`), in
/// `demote.rs`'s budget-filling shape: the loop header defines eight
/// values read after the join's call (callee-saved fixed holders) and
/// eight read only in the arm (caller-saved fixed holders), so the fixed
/// budget is full, and carries seven counters besides `i` and `acc` so
/// the pinned budget is full too (the join's phi cannot be anchored to a
/// pinned register: it takes `init_assignment`'s budget-full path); the
/// join's phi `r` is heavy (read many times after the join). At the edge `loop -> join` the phi's assignment is created
/// inside the branch region, and with the budget full the weighted
/// allocator demotes the lightest holder — a spill emitted inside the
/// edge's code, on that edge only — unless the region forbids it.
/// `set_branch_region` set the region flag inside a `tpde_assert_ne!`,
/// compiled out of release builds: the release compiler demoted inside
/// the edge, the arm's path reached the join with the holder's slot never
/// written, and the reload after the join read garbage (the e2e's fault
/// at address 0). Native and interpreter must agree; the debug compiler
/// (the region honored) always did.
fn demotion_program(nk: usize, nm: usize, phi_uses: usize) -> Program {
    demotion_program_nc(nk, nm, phi_uses, 0)
}

/// `demotion_program` with `nc` loop-carried counters (register-state's
/// shape: many loop-carried phis per block, beyond the pinned budget).
fn demotion_program_nc(nk: usize, nm: usize, phi_uses: usize, NC: usize) -> Program {
    let mut s = String::from(
        r#"
(program
  (host touch (ptr) void touches_memory)
  (fn f ((p ptr) (q ptr)) (i64)
    (block entry ((p ptr) (q ptr))
      (z (const i64 0))
"#,
    );
    s.push_str(&format!("      (br loop z z{}))
", " z".repeat(NC)));
    s.push_str(&format!("    (block loop ((i i64) (acc i64){})
", (1..=NC).map(|c| format!(" (c{c} i64)")).collect::<String>()));
    s.push_str("      (h (load i64 p 0))
      (n (load i64 p 248))
");
    for k in 1..=nk {
        s.push_str(&format!("      (k{k} (load i64 p {}))\n", 8 * k));
    }
    for m in 1..=nm {
        s.push_str(&format!("      (m{m} (load i64 p {}))\n", 128 + 8 * m));
    }
    s.push_str("      (one (const i64 1))\n      (bit (iand i one))\n      (c (icmp eq bit one))\n      (condbr c (arma) (join z)))\n    (block arma ()\n      (y0 (iadd i n))\n");
    let mut prev = String::from("y0");
    for m in 1..=nm {
        s.push_str(&format!("      (y{m} (iadd {prev} m{m}))\n"));
        prev = format!("y{m}");
    }
    s.push_str(&format!("      (br join {prev}))\n    (block join ((r i64))\n      ((call_host touch q))\n      (s0 (iadd h r))\n"));
    let mut prev = String::from("s0");
    for k in 1..=nk {
        s.push_str(&format!("      (t{k} (iadd k{k} k{k}))\n      (s{k} (iadd {prev} t{k}))\n"));
        prev = format!("s{k}");
    }
    for u in 1..=phi_uses {
        s.push_str(&format!("      (u{u} (iadd {prev} r))\n"));
        prev = format!("u{u}");
    }
    // The counters advance by one per iteration and are summed into the
    // result at the end.
    for c in 1..=NC {
        s.push_str(&format!("      (c{c}n (iadd c{c} one))
"));
    }
    s.push_str(&format!("      (acc1 (iadd acc {prev}))
      (i1 (iadd i one))
      (more (icmp slt i1 n))
      (condbr more (loop i1 acc1{}) (exit acc1{})))
", (1..=NC).map(|c| format!(" c{c}n")).collect::<String>(), (1..=NC).map(|c| format!(" c{c}n")).collect::<String>()));
    s.push_str(&format!("    (block exit ((res i64){})
", (1..=NC).map(|c| format!(" (e{c} i64)")).collect::<String>()));
    let mut prev = String::from("res");
    for c in 1..=NC {
        s.push_str(&format!("      (r{c} (iadd {prev} e{c}))
"));
        prev = format!("r{c}");
    }
    s.push_str(&format!("      (return {prev}))))
"));
    // `GRAVITY_EDGE_DUMP=<dir>` writes the program text (for `asm_debug`).
    if let Ok(dir) = std::env::var("GRAVITY_EDGE_DUMP") {
        std::fs::write(format!("{dir}/demotion-{nk}-{nm}-{phi_uses}.txt"), &s).unwrap();
    }
    parse_program(&s, &|name| match name {
        "touch" => Some(host_touch as extern "C" fn(*mut i64) as usize),
        _ => None,
    })
    .unwrap_or_else(|e| panic!("{e}\n{s}"))
}

/// The same demotion without a loop: no pinned loop phis, so the
/// callee-saved and caller-saved fixed pools fill completely (the fixed
/// budget of 16) with the entry block's holders; `h` (one use, after the
/// join's call) is the lightest holder, the join's phi `r` the heaviest
/// value. The arm is chosen by the flag at `p + 256`.
fn demotion_program_straight(nk: usize, nm: usize, phi_uses: usize) -> Program {
    let mut s = String::from(
        r#"
(program
  (host touch (ptr) void touches_memory)
  (fn f ((p ptr) (q ptr)) (i64)
    (block entry ((p ptr) (q ptr))
      (z (const i64 0))
      (h (load i64 p 0))
      (flag (load i64 p 256))
"#,
    );
    for k in 1..=nk {
        s.push_str(&format!("      (k{k} (load i64 p {}))\n", 8 * k));
    }
    for m in 1..=nm {
        s.push_str(&format!("      (m{m} (load i64 p {}))\n", 128 + 8 * m));
    }
    s.push_str("      (c (icmp ne flag z))\n      (condbr c (arma) (join z)))\n    (block arma ()\n      (y0 (iadd flag z))\n");
    let mut prev = String::from("y0");
    for m in 1..=nm {
        s.push_str(&format!("      (w{m} (iadd m{m} m{m}))\n      (y{m} (iadd {prev} w{m}))\n"));
        prev = format!("y{m}");
    }
    s.push_str(&format!("      (br join {prev}))\n    (block join ((r i64))\n      ((call_host touch q))\n      (s0 (iadd h r))\n"));
    let mut prev = String::from("s0");
    for k in 1..=nk {
        s.push_str(&format!("      (t{k} (iadd k{k} k{k}))\n      (s{k} (iadd {prev} t{k}))\n"));
        prev = format!("s{k}");
    }
    for u in 1..=phi_uses {
        s.push_str(&format!("      (u{u} (iadd {prev} r))\n"));
        prev = format!("u{u}");
    }
    s.push_str(&format!("      (return {prev}))))\n"));
    if let Ok(dir) = std::env::var("GRAVITY_EDGE_DUMP") {
        std::fs::write(format!("{dir}/straight-{nk}-{nm}-{phi_uses}.txt"), &s).unwrap();
    }
    parse_program(&s, &|name| match name {
        "touch" => Some(host_touch as extern "C" fn(*mut i64) as usize),
        _ => None,
    })
    .unwrap_or_else(|e| panic!("{e}\n{s}"))
}

#[test]
fn a_heavy_phi_must_not_demote_a_holder_inside_the_edge_straight() {
    let mut cells: Vec<i64> = (0..32).map(|i| 0x3000_0000 + 7919 * i as i64).collect();
    cells.push(0); // cell 32 = p+256: the arm flag
    for (nk, nm, phi_uses) in [(9, 8, 12), (9, 10, 24), (10, 8, 8), (9, 12, 4), (11, 8, 16)] {
        let p = demotion_program_straight(nk, nm, phi_uses);
        for flag in [1i64, 0, 1] {
            cells[32] = flag;
            let h = cells[0];
            let ks: i64 = (1..=nk).map(|k| 2 * cells[k]).sum();
            let ms: i64 = (1..=nm).map(|m| 2 * cells[16 + m]).sum();
            let r = if flag != 0 { flag + ms } else { 0 };
            let want = h + r + ks + r * phi_uses as i64;
            let mut touched = 0i64;
            let interp = gravity_interp::Interp::new(&p).run(Func(0), &[cells.as_ptr() as u64, &mut touched as *mut i64 as u64]).unwrap();
            assert_eq!(interp, vec![want as u64], "interpreter: nk {nk} nm {nm} phi_uses {phi_uses} flag {flag}");
            let mut touched = 0i64;
            let native = common::native(&p, Func(0), &[cells.as_ptr() as u64, &mut touched as *mut i64 as u64]);
            assert_eq!(native, interp, "native vs interpreter: nk {nk} nm {nm} phi_uses {phi_uses} flag {flag}");
            assert_eq!(touched, 1000);
        }
    }
}

#[test]
fn a_heavy_phi_must_not_demote_a_holder_inside_the_edge() {
    let mut cells: Vec<i64> = (0..31).map(|i| 0x2000_0000 + 7919 * i as i64).collect();
    let n = 6i64;
    cells.push(n); // cell 31 = p+248: the loop bound
    for (nk, nm, phi_uses) in [(8, 9, 12), (8, 10, 24), (6, 12, 16), (8, 12, 4), (8, 8, 12)] {
        let p = demotion_program(nk, nm, phi_uses);
        let want = {
            let h = cells[0];
            let ks: i64 = (1..=nk).map(|k| 2 * cells[k]).sum();
            let ms: i64 = (1..=nm).map(|m| cells[16 + m]).sum();
            (0..n).map(|i| {
                let r = if i & 1 == 1 { i + n + ms } else { 0 };
                h + r + ks + r * phi_uses as i64
            }).sum::<i64>()
        };
        let mut touched = 0i64;
        let interp = gravity_interp::Interp::new(&p).run(Func(0), &[cells.as_ptr() as u64, &mut touched as *mut i64 as u64]).unwrap();
        assert_eq!(interp, vec![want as u64], "interpreter: nk {nk} nm {nm} phi_uses {phi_uses}");
        assert_eq!(touched, 1000 * n);
        let mut touched = 0i64;
        let native = common::native(&p, Func(0), &[cells.as_ptr() as u64, &mut touched as *mut i64 as u64]);
        assert_eq!(native, interp, "native vs interpreter: nk {nk} nm {nm} phi_uses {phi_uses}");
        assert_eq!(touched, 1000 * n, "native: the join's call ran once per iteration");
    }
}

/// Register state's shape (`PGRUST_GRAVITY_REGSTATE=all`, e2e Q85.2): a
/// loop header carrying many values beyond the pinned budget, a diamond
/// and a call in the body. Native and interpreter must agree and the loop
/// must end.
#[test]
fn many_loop_carried_phis_agree_with_the_interpreter() {
    let mut cells: Vec<i64> = (0..31).map(|i| 0x4000_0000 + 7919 * i as i64).collect();
    let n = 6i64;
    cells.push(n);
    for (nk, nm, phi_uses, nc) in [(8, 8, 12, 9), (8, 8, 12, 13), (6, 6, 8, 15), (8, 8, 4, 20), (4, 4, 4, 26)] {
        let p = demotion_program_nc(nk, nm, phi_uses, nc);
        let want = {
            let h = cells[0];
            let ks: i64 = (1..=nk).map(|k| 2 * cells[k]).sum();
            let ms: i64 = (1..=nm).map(|m| cells[16 + m]).sum();
            (0..n).map(|i| {
                let r = if i & 1 == 1 { i + n + ms } else { 0 };
                h + r + ks + r * phi_uses as i64
            }).sum::<i64>() + nc as i64 * n
        };
        let mut touched = 0i64;
        let interp = gravity_interp::Interp::new(&p).run(Func(0), &[cells.as_ptr() as u64, &mut touched as *mut i64 as u64]).unwrap();
        assert_eq!(interp, vec![want as u64], "interpreter: nk {nk} nm {nm} phi_uses {phi_uses} nc {nc}");
        let mut touched = 0i64;
        let native = common::native(&p, Func(0), &[cells.as_ptr() as u64, &mut touched as *mut i64 as u64]);
        assert_eq!(native, interp, "native vs interpreter: nk {nk} nm {nm} phi_uses {phi_uses} nc {nc}");
    }
}

/// Register state's shape with narrow loop-carried values: `i8` flags
/// carried through the loop header next to the `i64` counters (the Q85.2
/// program's header carries nine `i64` and six `i8` parameters), toggled
/// per iteration and folded into the result; the loop bound is read from
/// one of them so a lost update hangs or ends early. Native vs interpreter.
fn narrow_phis_program(nc: usize, nf: usize) -> Program {
    let mut s = String::from(
        r#"
(program
  (host touch (ptr) void touches_memory)
  (fn f ((p ptr) (q ptr)) (i64)
    (block entry ((p ptr) (q ptr))
      (z (const i64 0))
      (zb (const i8 0))
"#,
    );
    s.push_str(&format!("      (br loop z z{}{}))\n", " z".repeat(nc), " zb".repeat(nf)));
    s.push_str(&format!("    (block loop ((i i64) (acc i64){}{})\n", (1..=nc).map(|c| format!(" (c{c} i64)")).collect::<String>(), (1..=nf).map(|f| format!(" (f{f} i8)")).collect::<String>()));
    s.push_str("      (h (load i64 p 0))\n      (n (load i64 p 248))\n      (one (const i64 1))\n      (oneb (const i8 1))\n      (bit (iand i one))\n      (c (icmp eq bit one))\n      (condbr c (arma) (join z)))\n    (block arma ()\n      (y0 (iadd i n))\n      (br join y0))\n    (block join ((r i64))\n      ((call_host touch q))\n      (s0 (iadd h r))\n");
    let mut prev = String::from("s0");
    for c in 1..=nc {
        s.push_str(&format!("      (c{c}n (iadd c{c} one))\n      (s{c} (iadd {prev} c{c}n))\n"));
        prev = format!("s{c}");
    }
    for f in 1..=nf {
        s.push_str(&format!("      (f{f}n (ixor f{f} oneb))\n      (f{f}w (zext i64 f{f}n))\n      (t{f} (iadd {prev} f{f}w))\n"));
        prev = format!("t{f}");
    }
    s.push_str(&format!("      (acc1 (iadd acc {prev}))\n      (i1 (iadd i one))\n      (more (icmp slt i1 n))\n      (condbr more (loop i1 acc1{}{}) (exit acc1)))\n    (block exit ((res i64))\n      (return res))))\n", (1..=nc).map(|c| format!(" c{c}n")).collect::<String>(), (1..=nf).map(|f| format!(" f{f}n")).collect::<String>()));
    if let Ok(dir) = std::env::var("GRAVITY_EDGE_DUMP") {
        std::fs::write(format!("{dir}/narrow-{nc}-{nf}.txt"), &s).unwrap();
    }
    parse_program(&s, &|name| match name {
        "touch" => Some(host_touch as extern "C" fn(*mut i64) as usize),
        _ => None,
    })
    .unwrap_or_else(|e| panic!("{e}\n{s}"))
}

#[test]
fn narrow_loop_carried_phis_agree_with_the_interpreter() {
    let mut cells: Vec<i64> = (0..31).map(|i| 0x5000_0000 + 7919 * i as i64).collect();
    let n = 6i64;
    cells.push(n);
    for (nc, nf) in [(3, 3), (9, 6), (12, 8), (4, 12), (20, 6)] {
        let p = narrow_phis_program(nc, nf);
        let want = {
            let h = cells[0];
            (0..n).map(|i| {
                let r = if i & 1 == 1 { i + n } else { 0 };
                // counters: c_k after i iterations = i, and c_kn = i + 1; flags toggle: f after i iterations = i % 2, f_n = (i + 1) % 2
                h + r + nc as i64 * (i + 1) + nf as i64 * ((i + 1) % 2)
            }).sum::<i64>()
        };
        let mut touched = 0i64;
        let interp = gravity_interp::Interp::new(&p).run(Func(0), &[cells.as_ptr() as u64, &mut touched as *mut i64 as u64]).unwrap();
        assert_eq!(interp, vec![want as u64], "interpreter: nc {nc} nf {nf}");
        let mut touched = 0i64;
        let native = common::native(&p, Func(0), &[cells.as_ptr() as u64, &mut touched as *mut i64 as u64]);
        assert_eq!(native, interp, "native vs interpreter: nc {nc} nf {nf}");
    }
}
