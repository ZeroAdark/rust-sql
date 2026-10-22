use gravity_interp::{Interp, Trap};
use gravity_ir::text::parse_program;
use gravity_ir::*;

fn load(src: &str) -> Program {
    parse_program(src, &|name| match name {
        "add_i32" => Some(add_i32 as extern "C" fn(i32, i32) -> i32 as usize),
        "bump" => Some(bump as extern "C" fn(*mut i64) as usize),
        _ => None,
    })
    .unwrap_or_else(|e| panic!("{e}"))
}

extern "C" fn add_i32(a: i32, b: i32) -> i32 {
    a.wrapping_add(b)
}
extern "C" fn bump(p: *mut i64) {
    unsafe { *p += 10 }
}

#[test]
fn sum_positive_loop() {
    let p = load(
        r#"
(program
  (fn sum_positive ((ctx ptr) (n i64)) (i64)
    (block entry ((ctx ptr) (n i64))
      (zero (const i64 0))
      (br loop zero zero))
    (block loop ((acc i64) (i i64))
      (done (icmp sge i n))
      (condbr done (exit acc) (body acc i)))
    (block body ((acc2 i64) (i2 i64))
      (p (ptr_add ctx i2 4 0))
      (x32 (load i32 p 0))
      (x (sext i64 x32))
      (z (const i64 0))
      (keep (icmp sgt x z))
      (one (const i64 1))
      (next (iadd i2 one))
      (sum (iadd acc2 x))
      (condbr keep (loop sum next) (loop acc2 next)))
    (block exit ((r i64))
      (return r))))
"#,
    );
    let data: Vec<i32> = vec![5, -3, 7, 0, -100, 11];
    let r = Interp::new(&p).run(Func(0), &[data.as_ptr() as u64, data.len() as u64]).unwrap();
    assert_eq!(r, vec![23]);
}

#[test]
fn recursion_and_internal_call() {
    let p = load(
        r#"
(program
  (fn fact ((n i64)) (i64)
    (block entry ((n i64))
      (one (const i64 1))
      (base (icmp sle n one))
      (condbr base (ret one) (rec n)))
    (block rec ((m i64))
      (one2 (const i64 1))
      (m1 (isub m one2))
      (f (call fact m1))
      (r (imul m f))
      (br ret r))
    (block ret ((v i64))
      (return v))))
"#,
    );
    assert_eq!(Interp::new(&p).run(Func(0), &[10]).unwrap(), vec![3628800]);
}

#[test]
fn host_calls_and_stack_slots() {
    let p = load(
        r#"
(program
  (host add_i32 (i32 i32) i32 pure)
  (host bump (ptr) void touches_memory)
  (fn f ((a i32) (b i32)) (i64)
    (slots (8 8))
    (block entry ((a i32) (b i32))
      (s (call_host add_i32 a b))
      (w (sext i64 s))
      (p (stack_addr 0))
      ((store p 0 w))
      ((call_host bump p))
      (r (load i64 p 0))
      (return r))))
"#,
    );
    assert_eq!(Interp::new(&p).run(Func(0), &[-7i64 as u64, 4]).unwrap(), vec![7]);
}

/// D3229: a stack slot sits at the same address call after call along the same call path, as compiled code's does (the
/// sort kernels name a merge's reader by its emit table, a stack slot: a recursive term's pipeline, called again each
/// generation, must find its own merge), a frame nested inside another gets its own bytes, and every call's slots start
/// zeroed whatever the call before left there.
#[test]
fn stack_slots_keep_their_address_call_after_call() {
    let p = load(
        r#"
(program
  (fn leaf ((x i64)) (i64)
    (slots (16 8))
    (block entry ((x i64))
      (p (stack_addr 0))
      ((store p 8 x))
      (a (ptrtoint p))
      (return a)))
  (fn outer ((x i64)) (i64 i64)
    (slots (32 16))
    (block entry ((x i64))
      (q (stack_addr 0))
      ((store q 0 x))
      (r (call leaf x))
      (b (ptrtoint q))
      (return b r)))
  (fn peek ((x i64)) (i64)
    (slots (16 8))
    (block entry ((x i64))
      (p (stack_addr 0))
      (old (load i64 p 8))
      ((store p 8 x))
      (return old))))
"#,
    );
    // between calls the heap is taken in the sizes a frame's bytes had (a frame's own allocation came back at the same
    // address only while the allocator handed the freed block out again: the run-to-run variation of D3229)
    let mut hold: Vec<Vec<u8>> = Vec::new();
    let churn = |hold: &mut Vec<Vec<u8>>| (16..=96).step_by(8).for_each(|n| hold.push(vec![1u8; n]));
    let i = Interp::new(&p);
    let leaf = i.run(Func(0), &[7]).unwrap();
    for k in 0..3 {
        churn(&mut hold);
        assert_eq!(i.run(Func(0), &[8 + k]).unwrap(), leaf, "leaf's slot moved");
    }
    let nested = Interp::new(&p).run(Func(1), &[9]).unwrap();
    assert_ne!(nested[1], nested[0], "a nested frame gets its own bytes");
    assert_eq!((nested[0] % 16, nested[1] % 8), (0, 0));
    for k in 0..3 {
        churn(&mut hold);
        assert_eq!(Interp::new(&p).run(Func(1), &[10 + k]).unwrap(), nested, "the nested call path moved");
    }
    for k in 0..3 {
        assert_eq!(i.run(Func(2), &[20 + k]).unwrap(), vec![0], "a call's slots start zeroed");
    }
}

#[test]
fn overflow_flags_and_widths() {
    let p = load(
        r#"
(program
  (fn f ((a i32) (b i32)) (i32 bool bool)
    (block entry ((a i32) (b i32))
      (s (iadd a b))
      (o (sadd_ovf a b))
      (m (smul_ovf a b))
      (return s o m))))
"#,
    );
    let i = Interp::new(&p);
    let max = i32::MAX as i64 as u64;
    assert_eq!(i.run(Func(0), &[max, 1]).unwrap(), vec![i32::MIN as i64 as u64, 1, 0]);
    assert_eq!(i.run(Func(0), &[3, 4]).unwrap(), vec![7, 0, 0]);
    assert_eq!(i.run(Func(0), &[65536, 65536]).unwrap(), vec![0x20000, 0, 1]);
}

#[test]
fn switch_and_traps() {
    let p = load(
        r#"
(program
  (fn f ((k i64) (d i64)) (i64)
    (block entry ((k i64) (d i64))
      (switch k (case 1 (one)) (case 2 (two d)) (default (dead))))
    (block one ()
      (c (const i64 100))
      (return c))
    (block two ((x i64))
      (n (const i64 1000))
      (q (sdiv n x))
      (return q))
    (block dead ()
      (unreachable))))
"#,
    );
    let i = Interp::new(&p);
    assert_eq!(i.run(Func(0), &[1, 0]).unwrap(), vec![100]);
    assert_eq!(i.run(Func(0), &[2, 8]).unwrap(), vec![125]);
    assert!(matches!(i.run(Func(0), &[2, 0]), Err(Trap::DivideByZero { .. })));
    assert!(matches!(i.run(Func(0), &[9, 0]), Err(Trap::Unreachable { .. })));
}

#[test]
fn floats() {
    let p = load(
        r#"
(program
  (fn f ((a f64) (b f64)) (f64 bool i32)
    (block entry ((a f64) (b f64))
      (s (fadd a b))
      (r (sqrt s))
      (nan (fcmp uno a b))
      (t (fptosi i32 r))
      (return r nan t))))
"#,
    );
    let r = Interp::new(&p).run(Func(0), &[9.0f64.to_bits(), 7.0f64.to_bits()]).unwrap();
    assert_eq!(f64::from_bits(r[0]), 4.0);
    assert_eq!(r[1], 0);
    assert_eq!(r[2], 4);
}
