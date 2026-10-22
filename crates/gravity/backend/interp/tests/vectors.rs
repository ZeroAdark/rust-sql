//! The vector ops (`docs/design/gravity-simple/simd.md`) through the text
//! form and the interpreter, checked against scalar Rust references lane
//! by lane. The TPDE oracle (`gravity_tpde/tests/ops_oracle.rs`) will
//! compare its NEON sequences against these semantics once item 2 lands.

use gravity_interp::Interp;
use gravity_ir::text::parse_program;
use gravity_ir::*;

fn load(src: &str) -> Program {
    parse_program(src, &|_| None).unwrap_or_else(|e| panic!("{e}"))
}

fn run(p: &Program, args: &[u64]) -> Vec<u64> {
    Interp::new(p).run(Func(0), args).unwrap()
}

/// `f(p, q, out)`: loads two `v2i64` from `p` and `q`, stores `OP` to `out`.
fn v2i64_bin(op: &str) -> Program {
    load(&format!(
        r#"
(program
  (fn f ((p ptr) (q ptr) (out ptr)) ()
    (block entry ((p ptr) (q ptr) (out ptr))
      (a (vload v2i64 p 0 unaligned))
      (b (vload v2i64 q 0 unaligned))
      (r ({op} a b))
      ((vstore out 0 r unaligned))
      (return))))
"#
    ))
}

#[test]
fn lane_wise_arithmetic_and_compare_v2i64() {
    let xs: [i64; 2] = [i64::MAX, -7];
    let ys: [i64; 2] = [1, 3];
    let mut out = [0i64; 2];
    for (op, want) in [
        ("vadd", [xs[0].wrapping_add(ys[0]), xs[1].wrapping_add(ys[1])]),
        ("vsub", [xs[0].wrapping_sub(ys[0]), xs[1].wrapping_sub(ys[1])]),
        ("vand", [xs[0] & ys[0], xs[1] & ys[1]]),
        ("vor", [xs[0] | ys[0], xs[1] | ys[1]]),
        ("vxor", [xs[0] ^ ys[0], xs[1] ^ ys[1]]),
        ("vcmp sgt", [-((xs[0] > ys[0]) as i64), -((xs[1] > ys[1]) as i64)]),
        ("vcmp ugt", [-(((xs[0] as u64) > (ys[0] as u64)) as i64), -(((xs[1] as u64) > (ys[1] as u64)) as i64)]),
        ("vcmp eq", [0, 0]),
        ("vcmp ne", [-1, -1]),
        ("vsmin", [xs[0].min(ys[0]), xs[1].min(ys[1])]),
        ("vsmax", [xs[0].max(ys[0]), xs[1].max(ys[1])]),
        ("vumin", [(xs[0] as u64).min(ys[0] as u64) as i64, (xs[1] as u64).min(ys[1] as u64) as i64]),
        ("vumax", [(xs[0] as u64).max(ys[0] as u64) as i64, (xs[1] as u64).max(ys[1] as u64) as i64]),
    ] {
        let p = v2i64_bin(op);
        run(&p, &[xs.as_ptr() as u64, ys.as_ptr() as u64, out.as_mut_ptr() as u64]);
        assert_eq!(out, want, "{op}");
    }
}

#[test]
fn v4i32_mul_shift_select_reduce_movmask() {
    let p = load(
        r#"
(program
  (fn f ((p ptr) (q ptr) (out ptr)) (i64 i64 i64)
    (block entry ((p ptr) (q ptr) (out ptr))
      (a (vload v4i32 p 0 unaligned))
      (b (vload v4i32 q 0 unaligned))
      (m (vmul a b))
      (s (vshl m 1))
      (u (vushr s 3))
      (t (vsshr s 3))
      (k (vcmp slt a b))
      (sel (vselect k a b))
      ((vstore out 0 u unaligned))
      ((vstore out 16 t unaligned))
      ((vstore out 32 sel unaligned))
      (sum (vreduce_add sel))
      (bits (vmovmask k))
      (l3 (vextract sel 3))
      (l3w (sext i64 l3))
      (return sum bits l3w))))
"#,
    );
    let xs: [i32; 4] = [-5, 100_000, 7, i32::MIN];
    let ys: [i32; 4] = [3, 70_000, -2, 1];
    let mut out = [0i32; 12];
    let r = run(&p, &[xs.as_ptr() as u64, ys.as_ptr() as u64, out.as_mut_ptr() as u64]);
    let m: Vec<i32> = (0..4).map(|k| xs[k].wrapping_mul(ys[k])).collect();
    let s: Vec<i32> = m.iter().map(|v| v.wrapping_shl(1)).collect();
    let u: Vec<i32> = s.iter().map(|v| ((*v as u32) >> 3) as i32).collect();
    let t: Vec<i32> = s.iter().map(|v| v >> 3).collect();
    let sel: Vec<i32> = (0..4).map(|k| if xs[k] < ys[k] { xs[k] } else { ys[k] }).collect();
    assert_eq!(&out[0..4], &u[..]);
    assert_eq!(&out[4..8], &t[..]);
    assert_eq!(&out[8..12], &sel[..]);
    let sum: i64 = sel.iter().map(|&v| v as i64).sum();
    let bits: u64 = (0..4).map(|k| ((xs[k] < ys[k]) as u64) << k).sum();
    assert_eq!(r, vec![sum as u64, bits, sel[3] as i64 as u64]);
}

#[test]
fn splat_insert_widen_narrow_bitcast() {
    let p = load(
        r#"
(program
  (fn f ((x i64) (out ptr)) (i64 i64)
    (block entry ((x i64) (out ptr))
      (v (vsplat v2i64 x))
      (one (const i64 1))
      (v1 (vinsert v 1 one))
      ((vstore out 0 v1 unaligned))
      (bytes (bitcast v16i8 v1))
      (h (vwiden u lo bytes))
      (hs (vwiden s lo bytes))
      ((vstore out 16 h unaligned))
      ((vstore out 32 hs unaligned))
      (w (vwiden s hi h))
      (n (vnarrow w w))
      ((vstore out 48 n unaligned))
      (c (vreduce_add bytes))
      (hi (vextract v1 1))
      (return c hi))))
"#,
    );
    let mut out = [0u8; 64];
    let x: u64 = 0x80ff_0102_0304_05fe;
    let r = run(&p, &[x, out.as_mut_ptr() as u64]);
    // v1 = [x, 1]
    assert_eq!(u64::from_le_bytes(out[0..8].try_into().unwrap()), x);
    assert_eq!(u64::from_le_bytes(out[8..16].try_into().unwrap()), 1);
    // The low 8 bytes of v1 widened unsigned / signed to 8 halfwords.
    let lo = x.to_le_bytes();
    for k in 0..8 {
        let got_u = u16::from_le_bytes(out[16 + 2 * k..18 + 2 * k].try_into().unwrap());
        let got_s = i16::from_le_bytes(out[32 + 2 * k..34 + 2 * k].try_into().unwrap());
        assert_eq!(got_u, lo[k] as u16, "uxtl lane {k}");
        assert_eq!(got_s, lo[k] as i8 as i16, "sxtl lane {k}");
    }
    // w = the high 4 halfwords of h sign-extended to 32 bits (they are
    // unsigned bytes, so positive); narrowing w twice gives [w, w] as 8
    // halfwords.
    for k in 0..4 {
        let want = lo[4 + k] as u16;
        assert_eq!(u16::from_le_bytes(out[48 + 2 * k..50 + 2 * k].try_into().unwrap()), want);
        assert_eq!(u16::from_le_bytes(out[56 + 2 * k..58 + 2 * k].try_into().unwrap()), want);
    }
    // vreduce_add of 16 signed bytes: x's bytes plus the byte 1 and seven 0s.
    let c: i64 = lo.iter().map(|&b| b as i8 as i64).sum::<i64>() + 1;
    assert_eq!(r, vec![c as u64, 1]);
}

#[test]
fn vector_phis_carry_both_words() {
    // acc <- acc + a over a loop: the block parameter is a v2i64.
    let p = load(
        r#"
(program
  (fn f ((p ptr) (n i64)) (i64)
    (block entry ((p ptr) (n i64))
      (z (const i64 0))
      (acc0 (vsplat v2i64 z))
      (br loop acc0 z))
    (block loop ((acc v2i64) (i i64))
      (done (icmp sge i n))
      (condbr done (exit acc) (body acc i)))
    (block body ((acc2 v2i64) (i2 i64))
      (cell (ptr_add p i2 16 0))
      (v (vload v2i64 cell 0 unaligned))
      (acc3 (vadd acc2 v))
      (one (const i64 1))
      (i3 (iadd i2 one))
      (br loop acc3 i3))
    (block exit ((r v2i64))
      (s (vreduce_add r))
      (return s))))
"#,
    );
    let data: Vec<i64> = (1..=10).map(|v| v * 1_000_000_007).collect();
    let r = run(&p, &[data.as_ptr() as u64, 5]);
    assert_eq!(r, vec![data.iter().sum::<i64>() as u64]);
}

#[test]
fn verifier_rejects_vector_boundaries_and_scalar_ops_on_vectors() {
    let bad = [
        "(program (fn f ((v v2i64)) () (block entry ((v v2i64)) (return))))",
        "(program (fn f ((p ptr)) () (block entry ((p ptr)) (a (load v2i64 p 0)) (return))))",
        "(program (fn f ((p ptr)) () (block entry ((p ptr)) (a (vload v2i64 p 0)) (b (vmul a a)) (return))))",
        "(program (fn f ((p ptr)) () (block entry ((p ptr)) (a (vload v4i32 p 0)) (b (vshl a 32)) (return))))",
        "(program (fn f ((p ptr)) () (block entry ((p ptr)) (a (vload v2f64 p 0)) (b (vcmp eq a a)) (return))))",
        "(program (fn f ((p ptr)) () (block entry ((p ptr)) (a (vload v2i64 p 0)) (b (vextract a 2)) (return))))",
    ];
    for src in bad {
        assert!(parse_program(src, &|_| None).is_err(), "{src}");
    }
}

/// `vsmin` / `vsmax` / `vumin` / `vumax` at every integer lane width against scalar references.
#[test]
fn lane_wise_min_max_at_every_width() {
    for (ty, w) in [("v16i8", 8u32), ("v8i16", 16), ("v4i32", 32), ("v2i64", 64)] {
        let n = (128 / w) as usize;
        let m: u64 = if w == 64 { u64::MAX } else { (1u64 << w) - 1 };
        let pats: [i64; 8] = [0, 1, -1, 2, i64::MIN, i64::MAX, 0x7f, -0x80];
        let mut xs = vec![0u8; 16];
        let mut ys = vec![0u8; 16];
        for i in 0..n {
            xs[i * (w as usize / 8)..(i + 1) * (w as usize / 8)].copy_from_slice(&pats[i % 8].to_le_bytes()[..w as usize / 8]);
            ys[i * (w as usize / 8)..(i + 1) * (w as usize / 8)].copy_from_slice(&pats[(i + 3) % 8].to_le_bytes()[..w as usize / 8]);
        }
        let lane = |v: &[u8], i: usize| -> u64 {
            let mut b = [0u8; 8];
            b[..w as usize / 8].copy_from_slice(&v[i * (w as usize / 8)..(i + 1) * (w as usize / 8)]);
            u64::from_le_bytes(b) & m
        };
        let sext = |u: u64| ((u << (64 - w)) as i64) >> (64 - w);
        for (op, signed, is_min) in [("vsmin", true, true), ("vsmax", true, false), ("vumin", false, true), ("vumax", false, false)] {
            let p = load(&format!(
                r#"
(program
  (fn f ((p ptr) (q ptr) (out ptr)) ()
    (block entry ((p ptr) (q ptr) (out ptr))
      (a (vload {ty} p 0 unaligned))
      (b (vload {ty} q 0 unaligned))
      (r ({op} a b))
      ((vstore out 0 r unaligned))
      (return))))
"#
            ));
            let mut out = [0u8; 16];
            run(&p, &[xs.as_ptr() as u64, ys.as_ptr() as u64, out.as_mut_ptr() as u64]);
            for i in 0..n {
                let (x, y) = (lane(&xs, i), lane(&ys, i));
                let want = if signed {
                    (if is_min { sext(x).min(sext(y)) } else { sext(x).max(sext(y)) }) as u64 & m
                } else if is_min {
                    x.min(y)
                } else {
                    x.max(y)
                };
                assert_eq!(lane(&out, i), want, "{op} {ty} lane {i}");
            }
        }
    }
}
