use gravity_ir::builder::ProgramBuilder;
use gravity_ir::text::{parse_program, print_program};
use gravity_ir::verify::verify;
use gravity_ir::*;

/// sum_positive(ctx, n): sums the i32s at ctx[0..n] that are > 0, with an
/// overflow guard that calls a host error function.
fn build() -> Program {
    let mut pb = ProgramBuilder::new();
    let raise = pb.declare_host("raise_overflow", &[], None, 0, HostFlags { may_error: true, ..Default::default() });
    let f = pb.declare_func("sum_positive", &[Type::Ptr, Type::I64], &[Type::I64]);
    let mut b = pb.define(f);
    let [ctx, n]: [Value; 2] = b.params().try_into().unwrap();
    let zero = b.iconst(Type::I64, 0);
    let one = b.iconst(Type::I64, 1);
    let loop_ = b.create_block(&[Type::I64, Type::I64]); // (acc, i)
    let body = b.create_block(&[Type::I64, Type::I64]);
    let add = b.create_block(&[Type::I64, Type::I64, Type::I64]); // (acc, i, x)
    let ovf = b.create_block(&[]);
    let exit = b.create_block(&[Type::I64]);
    b.br(loop_, &[zero, zero]);

    b.switch_to(loop_);
    let [acc, i]: [Value; 2] = b.block_params(loop_).to_vec().try_into().unwrap();
    let done = b.icmp(IntCond::Sge, i, n);
    b.cond_br(done, (exit, &[acc]), (body, &[acc, i]));

    b.switch_to(body);
    let [acc, i]: [Value; 2] = b.block_params(body).to_vec().try_into().unwrap();
    let p = b.ptr_add(ctx, i, 4, 0);
    let x32 = b.load(Type::I32, p, 0);
    let x = b.conv(ConvOp::Sext, Type::I64, x32);
    let z = b.iconst(Type::I64, 0);
    let keep = b.icmp(IntCond::Sgt, x, z);
    let next = b.int_bin(IntBinOp::Add, i, one);
    b.cond_br(keep, (add, &[acc, next, x]), (loop_, &[acc, next]));

    b.switch_to(add);
    let [acc, i, x]: [Value; 3] = b.block_params(add).to_vec().try_into().unwrap();
    let o = b.int_ovf(IntOvfOp::SaddOvf, acc, x);
    let sum = b.int_bin(IntBinOp::Add, acc, x);
    b.cond_br(o, (ovf, &[]), (loop_, &[sum, i]));

    b.switch_to(ovf);
    b.call_host(raise, &[], None);
    b.unreachable();

    b.switch_to(exit);
    let [r]: [Value; 1] = b.block_params(exit).to_vec().try_into().unwrap();
    b.ret(&[r]);
    pb.finish()
}

#[test]
fn builder_program_verifies_and_round_trips() {
    let p = build();
    verify(&p).unwrap();
    let text = print_program(&p);
    // The loader numbers values in a different order (all block params
    // first), so compare after one canonicalizing pass.
    let p2 = parse_program(&text, &|_| Some(0)).unwrap_or_else(|e| panic!("{e}\n{text}"));
    verify(&p2).unwrap();
    let text2 = print_program(&p2);
    let p3 = parse_program(&text2, &|_| Some(0)).unwrap();
    assert_eq!(p2, p3, "{text2}");
    assert_eq!(print_program(&p3), text2);
    assert_eq!(p2.funcs[0].blocks.len(), p.funcs[0].blocks.len());
    assert_eq!(p2.funcs[0].values.len(), p.funcs[0].values.len());
}

#[test]
fn hand_written_text_loads() {
    let src = r#"
(program
  (host int4_out (i32) ptr may_error touches_memory)
  (fn abs_or_call ((x i32)) (i32)
    (block entry ((x i32))
      (zero (const i32 0))
      (neg (icmp slt x zero))
      (condbr neg (flip x) (keep x)))
    (block flip ((a i32))
      (r (ineg a))
      ((call_host int4_out r))       ; void use of a value-returning call is a type error, so:
      (br keep r))
    (block keep ((b i32))
      (return b))))
"#;
    // The comment above is wrong on purpose for the reader: a value-returning
    // host call needs a result name. Expect a load failure that says so.
    let err = parse_program(src, &|_| None).unwrap_err();
    assert!(err.contains("produces a value"), "{err}");

    let src = src.replace("((call_host int4_out r))", "(s (call_host int4_out r))");
    let p = parse_program(&src, &|_| None).unwrap();
    assert_eq!(p.funcs[0].blocks.len(), 3);
    assert!(p.host_fns[0].flags.may_error);
}

#[test]
fn verifier_rejects_edge_arity() {
    let src = r#"
(program
  (fn f ((x i64)) (i64)
    (block entry ((x i64))
      (br next))
    (block next ((y i64))
      (return y))))
"#;
    let err = parse_program(src, &|_| None).unwrap_err();
    assert!(err.contains("args"), "{err}");
}

#[test]
fn hints_roundtrip_through_text() {
    use gravity_ir::builder::ProgramBuilder;
    use gravity_ir::text::{parse_program, print_program};
    use gravity_ir::*;
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_func("f", &[Type::I64], &[Type::I64]);
    let mut b = pb.define(f);
    let x = b.params()[0];
    let hot = b.create_block(&[]);
    let cold = b.create_block(&[]);
    let one = b.iconst(Type::I64, 1);
    let y = b.int_bin(IntBinOp::Add, x, one);
    let c = b.icmp(IntCond::Sgt, y, one);
    let entry = b.entry();
    b.cond_br(c, (hot, &[]), (cold, &[]));
    b.set_likely(entry, true);
    b.mark_cold(cold);
    b.set_freq(cold, 0.001);
    b.mark_loop_header(hot);
    b.mark_remat(y);
    b.mark_nowrap(y);
    b.switch_to(hot);
    b.ret(&[y]);
    b.switch_to(cold);
    b.ret(&[x]);
    let p = pb.finish();
    let text = print_program(&p);
    assert!(text.contains("(hints"), "{text}");
    let q = parse_program(&text, &|_| None).unwrap_or_else(|e| panic!("{e}\n{text}"));
    assert_eq!(q.funcs[0].hints, p.funcs[0].hints, "{text}");
}

/// The scalar and lane-wise min / max ops (milestone-2.md §2.6 item 10) print, parse and verify
/// at every width; the verifier refuses them on `Bool` and on float vectors.
#[test]
fn min_max_ops_round_trip_and_verify() {
    let src = r#"
(program
  (fn f ((x i64) (y i64) (p ptr)) (i64)
    (block entry ((x i64) (y i64) (p ptr))
      (a (smin x y)) (b (smax x y)) (c (umin x y)) (d (umax x y))
      (x8 (itrunc i8 x)) (y8 (itrunc i8 y)) (e (umin x8 y8)) (e2 (smax x8 y8))
      (x16 (itrunc i16 x)) (y16 (itrunc i16 y)) (g (smin x16 y16))
      (x32 (itrunc i32 x)) (y32 (itrunc i32 y)) (h (umax x32 y32))
      (v (vload v4i32 p 0 unaligned)) (w (vload v4i32 p 16 unaligned))
      (m1 (vsmin v w)) (m2 (vsmax v w)) (m3 (vumin v w)) (m4 (vumax v w))
      (v2 (vload v2i64 p 32 unaligned)) (m5 (vumax v2 v2))
      (v8 (vload v8i16 p 0 unaligned)) (m6 (vsmin v8 v8))
      (v16 (vload v16i8 p 0 unaligned)) (m7 (vumin v16 v16))
      ((vstore p 0 m1 unaligned)) ((vstore p 16 m2 unaligned)) ((vstore p 32 m3 unaligned)) ((vstore p 48 m4 unaligned))
      ((vstore p 64 m5 unaligned)) ((vstore p 80 m6 unaligned)) ((vstore p 96 m7 unaligned))
      (s1 (iadd a b)) (s2 (iadd c d)) (s3 (iadd s1 s2))
      (return s3))))
"#;
    let p = parse_program(src, &|_| None).unwrap_or_else(|e| panic!("{e}"));
    verify(&p).unwrap_or_else(|e| panic!("{e:?}"));
    let text = print_program(&p);
    for op in ["smin", "smax", "umin", "umax", "vsmin", "vsmax", "vumin", "vumax"] {
        assert!(text.contains(&format!("({op} ")), "{op} missing from\n{text}");
    }
    let again = parse_program(&text, &|_| None).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(print_program(&again), text);
    for bad in [
        "(program (fn f ((x i64) (y i64)) () (block entry ((x i64) (y i64)) (c (icmp eq x y)) (d (icmp ne x y)) (e (smin c d)) (return))))",
        "(program (fn f ((p ptr)) () (block entry ((p ptr)) (a (vload v2f64 p 0)) (b (vsmax a a)) (return))))",
        "(program (fn f ((p ptr)) () (block entry ((p ptr)) (a (vload v4f32 p 0)) (b (vumin a a)) (return))))",
    ] {
        // the parser verifies what it built: either step refuses
        assert!(parse_program(bad, &|_| None).map(|p| verify(&p).is_err()).unwrap_or(true), "{bad}");
    }
}

/// C6 (b): the s-expression reader recurses once per list; nesting past MAX_SEXP_DEPTH is an error,
/// where it used to recurse as deep as the input (200000 open lists ran the thread off its stack).
#[test]
fn deeply_nested_text_is_an_error_not_a_stack_overflow() {
    use gravity_ir::text::{read_sexp, MAX_SEXP_DEPTH};
    let nest = |n: usize| "(".repeat(n) + "x" + &")".repeat(n);
    assert!(read_sexp(&nest(MAX_SEXP_DEPTH as usize)).is_ok());
    let e = read_sexp(&nest(MAX_SEXP_DEPTH as usize + 1)).unwrap_err();
    assert!(e.contains("nested deeper than"), "{e}");
    assert!(read_sexp(&nest(200_000)).unwrap_err().contains("nested deeper than"));
}
