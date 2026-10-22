//! Larger programs in the text form: loops, memory, recursion, control flow.

mod common;
use gravity_ir::text::parse_program;
use gravity_ir::*;

extern "C" fn host_mul3(a: i64, b: i64, c: i64) -> i64 {
    a * b * c
}
extern "C" fn host_swap_i64(p: *mut i64, q: *mut i64) {
    unsafe { core::ptr::swap(p, q) }
}

extern "C" fn host_sum8(a: i64, b: i64, c: i64, d: i64, e: i64, f: i64, g: i64, h: i64) -> i64 {
    a + 2 * b + 3 * c + 4 * d + 5 * e + 6 * f + 7 * g + 8 * h
}
/// Narrow arguments and a narrow result: the upper bits of the returned
/// register are whatever the host leaves there.
extern "C" fn host_mix(a: i32, b: i8, c: i16, d: bool) -> i32 {
    a.wrapping_add(b as i32).wrapping_add(c as i32).wrapping_add(d as i32)
}

fn load(src: &str) -> Program {
    parse_program(src, &|name| match name {
        "mul3" => Some(host_mul3 as extern "C" fn(i64, i64, i64) -> i64 as usize),
        "swap_i64" => Some(host_swap_i64 as extern "C" fn(*mut i64, *mut i64) as usize),
        "sum8" => Some(host_sum8 as extern "C" fn(i64, i64, i64, i64, i64, i64, i64, i64) -> i64 as usize),
        "mix" => Some(host_mix as extern "C" fn(i32, i8, i16, bool) -> i32 as usize),
        _ => None,
    })
    .unwrap_or_else(|e| panic!("{e}"))
}
fn run(p: &Program, name: &str, args: &[u64]) -> Vec<u64> {
    let f = Func(p.funcs.iter().position(|f| f.name == name).unwrap() as u32);
    common::both(p, f, args).unwrap()
}

#[test]
fn gcd_euclid() {
    let p = load(
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
    for (a, b, g) in [(48, 18, 6), (17, 5, 1), (0, 9, 9), (100, 100, 100), (1071, 462, 21)] {
        assert_eq!(run(&p, "gcd", &[a, b]), vec![g]);
    }
}

#[test]
fn fibonacci_iterative_with_parallel_copy() {
    // (a, b) <- (b, a+b): the edge must be a parallel copy.
    let p = load(
        r#"
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
"#,
    );
    let want = [0u64, 1, 1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144];
    for (n, w) in want.iter().enumerate() {
        assert_eq!(run(&p, "fib", &[n as u64]), vec![*w]);
    }
    assert_eq!(run(&p, "fib", &[90]), vec![2880067194370816120]);
}

#[test]
fn swap_via_edge_args() {
    let p = load(
        r#"
(program
  (fn swap ((a i64) (b i64)) (i64 i64)
    (block entry ((a i64) (b i64))
      (br out b a))
    (block out ((x i64) (y i64))
      (return x y))))
"#,
    );
    assert_eq!(run(&p, "swap", &[1, 2]), vec![2, 1]);
}

#[test]
fn binary_search_i32_array() {
    let p = load(
        r#"
(program
  (fn bsearch ((arr ptr) (n i64) (key i32)) (i64)
    (block entry ((arr ptr) (n i64) (key i32))
      (zero (const i64 0))
      (br loop zero n))
    (block loop ((lo i64) (hi i64))
      (empty (icmp sge lo hi))
      (condbr empty (notfound) (probe lo hi)))
    (block probe ((lo2 i64) (hi2 i64))
      (sum (iadd lo2 hi2))
      (one (const i64 1))
      (mid (ushr sum one))
      (p (ptr_add arr mid 4 0))
      (v (load i32 p 0))
      (eq (icmp eq v key))
      (condbr eq (found mid) (narrow lo2 hi2 mid v)))
    (block narrow ((lo3 i64) (hi3 i64) (mid2 i64) (v2 i32))
      (less (icmp slt v2 key))
      (one2 (const i64 1))
      (mid1 (iadd mid2 one2))
      (condbr less (loop mid1 hi3) (loop lo3 mid2)))
    (block found ((i i64))
      (return i))
    (block notfound ()
      (m (const i64 -1))
      (return m))))
"#,
    );
    let arr: Vec<i32> = vec![-50, -3, 0, 4, 9, 15, 16, 100, 1000];
    for (i, &k) in arr.iter().enumerate() {
        assert_eq!(run(&p, "bsearch", &[arr.as_ptr() as u64, arr.len() as u64, k as i64 as u64]), vec![i as u64]);
    }
    for k in [-100, -1, 5, 17, 5000] {
        assert_eq!(run(&p, "bsearch", &[arr.as_ptr() as u64, arr.len() as u64, k as i64 as u64]), vec![u64::MAX]);
    }
}

#[test]
fn bubble_sort_in_place() {
    let p = load(
        r#"
(program
  (fn sort ((arr ptr) (n i64)) ()
    (block entry ((arr ptr) (n i64))
      (zero (const i64 0))
      (br outer zero))
    (block outer ((i i64))
      (one (const i64 1))
      (n1 (isub n one))
      (done (icmp sge i n1))
      (condbr done (exit) (inner_start i)))
    (block inner_start ((i2 i64))
      (zero2 (const i64 0))
      (br inner i2 zero2))
    (block inner ((i3 i64) (j i64))
      (one2 (const i64 1))
      (n1b (isub n one2))
      (lim (isub n1b i3))
      (jdone (icmp sge j lim))
      (i4 (iadd i3 one2))
      (condbr jdone (outer i4) (cmp i3 j)))
    (block cmp ((i5 i64) (j2 i64))
      (p (ptr_add arr j2 8 0))
      (a (load i64 p 0))
      (b (load i64 p 8))
      (gt (icmp sgt a b))
      (one3 (const i64 1))
      (j3 (iadd j2 one3))
      (condbr gt (swap i5 j3 p a b) (inner i5 j3)))
    (block swap ((i6 i64) (j4 i64) (q ptr) (a2 i64) (b2 i64))
      ((store q 0 b2))
      ((store q 8 a2))
      (br inner i6 j4))
    (block exit ()
      (return))))
"#,
    );
    let mut data: Vec<i64> = vec![5, -1, 9, 3, 3, 0, -100, 42, 7, 1];
    run(&p, "sort", &[data.as_mut_ptr() as u64, data.len() as u64]);
    let mut want = data.clone();
    want.sort();
    assert_eq!(data, want);
}

#[test]
fn fnv1a_hash_bytes() {
    let p = load(
        r#"
(program
  (fn fnv ((s ptr) (n i64)) (i64)
    (block entry ((s ptr) (n i64))
      (basis (const i64 0xcbf29ce484222325))
      (zero (const i64 0))
      (br loop basis zero))
    (block loop ((h i64) (i i64))
      (done (icmp sge i n))
      (condbr done (exit h) (step h i)))
    (block step ((h2 i64) (i2 i64))
      (p (ptr_add s i2 1 0))
      (b8 (load i8 p 0))
      (b (zext i64 b8))
      (x (ixor h2 b))
      (prime (const i64 0x100000001b3))
      (h3 (imul x prime))
      (one (const i64 1))
      (i3 (iadd i2 one))
      (br loop h3 i3))
    (block exit ((r i64))
      (return r))))
"#,
    );
    fn fnv(s: &[u8]) -> u64 {
        s.iter().fold(0xcbf29ce484222325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3))
    }
    for s in ["", "a", "hello", "the quick brown fox", "\u{ff}\u{fe}bytes"] {
        assert_eq!(run(&p, "fnv", &[s.as_ptr() as u64, s.len() as u64]), vec![fnv(s.as_bytes())]);
    }
}

#[test]
fn collatz_steps() {
    let p = load(
        r#"
(program
  (fn collatz ((n i64)) (i64)
    (block entry ((n i64))
      (zero (const i64 0))
      (br loop n zero))
    (block loop ((x i64) (steps i64))
      (one (const i64 1))
      (done (icmp eq x one))
      (condbr done (exit steps) (step x steps)))
    (block step ((x2 i64) (s2 i64))
      (one2 (const i64 1))
      (bit (iand x2 one2))
      (odd (icmp ne bit one2))
      (half (sshr x2 one2))
      (three (const i64 3))
      (t (imul x2 three))
      (t1 (iadd t one2))
      (next (select odd half t1))
      (s3 (iadd s2 one2))
      (br loop next s3))
    (block exit ((r i64))
      (return r))))
"#,
    );
    for (n, w) in [(1, 0), (2, 1), (3, 7), (6, 8), (7, 16), (27, 111), (97, 118)] {
        assert_eq!(run(&p, "collatz", &[n]), vec![w]);
    }
}

#[test]
fn switch_state_machine_counts_words() {
    // States: 0 = outside word, 1 = inside word. Count transitions 0->1.
    let p = load(
        r#"
(program
  (fn words ((s ptr) (n i64)) (i64)
    (block entry ((s ptr) (n i64))
      (zero (const i64 0))
      (br loop zero zero zero))
    (block loop ((i i64) (state i64) (count i64))
      (done (icmp sge i n))
      (condbr done (exit count) (read i state count)))
    (block read ((i2 i64) (st i64) (c i64))
      (p (ptr_add s i2 1 0))
      (ch8 (load i8 p 0))
      (ch (zext i64 ch8))
      (space (const i64 32))
      (issp (icmp eq ch space))
      (one (const i64 1))
      (i3 (iadd i2 one))
      (switch st (case 0 (outside i3 c issp)) (default (inside i3 c issp))))
    (block outside ((i4 i64) (c2 i64) (sp bool))
      (one2 (const i64 1))
      (c3 (iadd c2 one2))
      (z (const i64 0))
      (condbr sp (loop i4 z c2) (loop i4 one2 c3)))
    (block inside ((i5 i64) (c4 i64) (sp2 bool))
      (z2 (const i64 0))
      (one3 (const i64 1))
      (condbr sp2 (loop i5 z2 c4) (loop i5 one3 c4)))
    (block exit ((r i64))
      (return r))))
"#,
    );
    for (s, w) in [("", 0), ("a", 1), ("hello world", 2), ("  leading  and   trailing  ", 3), ("one", 1), ("   ", 0)] {
        assert_eq!(run(&p, "words", &[s.as_ptr() as u64, s.len() as u64]), vec![w]);
    }
}

#[test]
fn mutual_recursion_even_odd() {
    let p = load(
        r#"
(program
  (fn is_even ((n i64)) (bool)
    (block entry ((n i64))
      (z (const i64 0))
      (base (icmp eq n z))
      (condbr base (yes) (rec n)))
    (block rec ((m i64))
      (one (const i64 1))
      (m1 (isub m one))
      (r (call is_odd m1))
      (return r))
    (block yes ()
      (t (const bool true))
      (return t)))
  (fn is_odd ((n i64)) (bool)
    (block entry ((n i64))
      (z (const i64 0))
      (base (icmp eq n z))
      (condbr base (no) (rec n)))
    (block rec ((m i64))
      (one (const i64 1))
      (m1 (isub m one))
      (r (call is_even m1))
      (return r))
    (block no ()
      (f (const bool false))
      (return f))))
"#,
    );
    for n in 0..20u64 {
        assert_eq!(run(&p, "is_even", &[n]), vec![(n % 2 == 0) as u64]);
        assert_eq!(run(&p, "is_odd", &[n]), vec![(n % 2 == 1) as u64]);
    }
}

#[test]
fn narrow_width_wraparound_counter() {
    // Count in i8 from 120 by steps of 5 until it goes negative.
    let p = load(
        r#"
(program
  (fn f () (i8 i64)
    (block entry ()
      (start (const i8 120))
      (zero (const i64 0))
      (br loop start zero))
    (block loop ((x i8) (n i64))
      (z (const i8 0))
      (neg (icmp slt x z))
      (condbr neg (exit x n) (step x n)))
    (block step ((x2 i8) (n2 i64))
      (five (const i8 5))
      (x3 (iadd x2 five))
      (one (const i64 1))
      (n3 (iadd n2 one))
      (br loop x3 n3))
    (block exit ((r i8) (c i64))
      (return r c))))
"#,
    );
    // 120,125,-126: two steps, final value -126.
    assert_eq!(run(&p, "f", &[]), vec![(-126i8) as i64 as u64, 2]);
}

#[test]
fn f32_accumulation_rounds_like_f32() {
    let p = load(
        r#"
(program
  (fn sum ((n i64)) (f32)
    (block entry ((n i64))
      (zero (const f32 0.0))
      (i0 (const i64 0))
      (br loop zero i0))
    (block loop ((acc f32) (i i64))
      (done (icmp sge i n))
      (condbr done (exit acc) (step acc i)))
    (block step ((acc2 f32) (i2 i64))
      (tenth (const f32 0.1))
      (acc3 (fadd acc2 tenth))
      (one (const i64 1))
      (i3 (iadd i2 one))
      (br loop acc3 i3))
    (block exit ((r f32))
      (return r))))
"#,
    );
    let mut want = 0.0f32;
    for _ in 0..1000 {
        want += 0.1f32;
    }
    let got = f32::from_bits(run(&p, "sum", &[1000])[0] as u32);
    assert_eq!(got.to_bits(), want.to_bits());
}

#[test]
fn host_call_with_three_args_and_pointer_out_params() {
    let p = load(
        r#"
(program
  (host mul3 (i64 i64 i64) i64 pure)
  (host swap_i64 (ptr ptr) void touches_memory)
  (fn f ((a i64) (b i64) (c i64)) (i64 i64)
    (slots (8 8) (8 8))
    (block entry ((a i64) (b i64) (c i64))
      (m (call_host mul3 a b c))
      (p (stack_addr 0))
      (q (stack_addr 1))
      ((store p 0 m))
      ((store q 0 c))
      ((call_host swap_i64 p q))
      (x (load i64 p 0))
      (y (load i64 q 0))
      (return x y))))
"#,
    );
    assert_eq!(run(&p, "f", &[2, 3, 7]), vec![7, 42]);
}

#[test]
fn strlen_pointer_walk() {
    let p = load(
        r#"
(program
  (fn strlen ((s ptr)) (i64)
    (block entry ((s ptr))
      (zero (const i64 0))
      (br loop zero))
    (block loop ((i i64))
      (p (ptr_add s i 1 0))
      (c (load i8 p 0))
      (z (const i8 0))
      (end (icmp eq c z))
      (one (const i64 1))
      (i2 (iadd i one))
      (condbr end (exit i) (loop i2)))
    (block exit ((n i64))
      (return n))))
"#,
    );
    for s in ["\0", "a\0", "hello\0", "with spaces and stuff\0"] {
        assert_eq!(run(&p, "strlen", &[s.as_ptr() as u64]), vec![s.len() as u64 - 1]);
    }
}

#[test]
fn nested_loops_matrix_multiply() {
    // C[i][j] = sum_k A[i][k] * B[k][j], square n x n, row-major i64.
    let p = load(
        r#"
(program
  (fn matmul ((a ptr) (b ptr) (c ptr) (n i64)) ()
    (block entry ((a ptr) (b ptr) (c ptr) (n i64))
      (zero (const i64 0))
      (br i_loop zero))
    (block i_loop ((i i64))
      (idone (icmp sge i n))
      (zero2 (const i64 0))
      (condbr idone (exit) (j_loop i zero2)))
    (block j_loop ((i2 i64) (j i64))
      (jdone (icmp sge j n))
      (one (const i64 1))
      (i3 (iadd i2 one))
      (zero3 (const i64 0))
      (condbr jdone (i_loop i3) (k_loop i2 j zero3 zero3)))
    (block k_loop ((i4 i64) (j2 i64) (k i64) (acc i64))
      (kdone (icmp sge k n))
      (condbr kdone (store_c i4 j2 acc) (k_step i4 j2 k acc)))
    (block k_step ((i5 i64) (j3 i64) (k2 i64) (acc2 i64))
      (in_ (imul i5 n))
      (ik (iadd in_ k2))
      (pa (ptr_add a ik 8 0))
      (av (load i64 pa 0))
      (kn (imul k2 n))
      (kj (iadd kn j3))
      (pb (ptr_add b kj 8 0))
      (bv (load i64 pb 0))
      (prod (imul av bv))
      (acc3 (iadd acc2 prod))
      (one2 (const i64 1))
      (k3 (iadd k2 one2))
      (br k_loop i5 j3 k3 acc3))
    (block store_c ((i6 i64) (j4 i64) (v i64))
      (in2 (imul i6 n))
      (ij (iadd in2 j4))
      (pc (ptr_add c ij 8 0))
      ((store pc 0 v))
      (one3 (const i64 1))
      (j5 (iadd j4 one3))
      (br j_loop i6 j5))
    (block exit ()
      (return))))
"#,
    );
    let n = 3usize;
    let a: Vec<i64> = vec![1, 2, 3, 4, 5, 6, 7, 8, 9];
    let b: Vec<i64> = vec![9, 8, 7, 6, 5, 4, 3, 2, 1];
    let mut c = vec![0i64; 9];
    run(&p, "matmul", &[a.as_ptr() as u64, b.as_ptr() as u64, c.as_mut_ptr() as u64, n as u64]);
    let mut want = vec![0i64; 9];
    for i in 0..n {
        for j in 0..n {
            for k in 0..n {
                want[i * n + j] += a[i * n + k] * b[k * n + j];
            }
        }
    }
    assert_eq!(c, want);
}

// Three integer results: AAPCS64 returns them in x0-x2, but System V has
// only rax:rdx, so TPDE's x86-64 return builder refuses this shape and the
// test is AArch64-only.
#[cfg(target_arch = "aarch64")]
#[test]
fn sql_style_nullable_int4_add_with_overflow_error() {
    // The lowering shape for `a + b` on nullable int4: null flags threaded as
    // i8, overflow branches to an error block that returns a status code.
    // Returns (status, is_null, value). status: 0 ok, 1 overflow.
    let p = load(
        r#"
(program
  (fn int4pl ((a i32) (a_null i8) (b i32) (b_null i8)) (i32 i8 i32)
    (block entry ((a i32) (a_null i8) (b i32) (b_null i8))
      (nn (ior a_null b_null))
      (z8 (const i8 0))
      (any_null (icmp ne nn z8))
      (condbr any_null (null_result) (compute)))
    (block null_result ()
      (ok (const i32 0))
      (one (const i8 1))
      (junk (const i32 0))
      (return ok one junk))
    (block compute ()
      (ovf (sadd_ovf a b))
      (s (iadd a b))
      (condbr ovf (overflow) (done s)))
    (block overflow ()
      (err (const i32 1))
      (z (const i8 0))
      (junk2 (const i32 0))
      (return err z junk2))
    (block done ((v i32))
      (ok2 (const i32 0))
      (z2 (const i8 0))
      (return ok2 z2 v))))
"#,
    );
    let r = |a: i32, an: bool, b: i32, bn: bool| run(&p, "int4pl", &[a as i64 as u64, an as u64, b as i64 as u64, bn as u64]);
    assert_eq!(r(2, false, 3, false), vec![0, 0, 5]);
    assert_eq!(r(2, true, 3, false), vec![0, 1, 0]);
    assert_eq!(r(2, false, 3, true), vec![0, 1, 0]);
    assert_eq!(r(i32::MAX, false, 1, false), vec![1, 0, 0]);
    assert_eq!(r(i32::MIN, false, -1, false), vec![1, 0, 0]);
    assert_eq!(r(-5, false, -6, false), vec![0, 0, (-11i32) as i64 as u64]);
}

/// A 50-case dense switch (jump table) both with one block per case and
/// with every case feeding one join block through edge arguments (split
/// edges), plus a sparse switch that must not become a table.
fn switch50_program() -> Program {
    let mut src = String::from("(program\n  (fn sw ((x i64)) (i64)\n    (block entry ((x i64))\n      (switch x");
    for k in 100..150 {
        src.push_str(&format!(" (case {k} (c{k}))"));
    }
    src.push_str(" (default (dflt))))\n");
    for k in 100..150 {
        src.push_str(&format!("    (block c{k} () (v{k} (const i64 {})) (return v{k}))\n", k * 3 + 1));
    }
    src.push_str("    (block dflt () (m (const i64 -1)) (return m)))\n");
    // All cases into one block with an argument: every edge needs a split.
    src.push_str("  (fn sw2 ((x i32)) (i64)\n    (block entry ((x i32))\n");
    for k in 0..50 {
        src.push_str(&format!("      (k{k} (const i64 {}))\n", k * k + 7));
    }
    src.push_str("      (m1 (const i64 -1))\n      (switch x");
    for k in 0..50 {
        src.push_str(&format!(" (case {k} (join k{k}))"));
    }
    src.push_str(" (default (join m1))))\n");
    src.push_str("    (block join ((v i64)) (return v)))\n");
    // Sparse cases on an i8 scrutinee.
    src.push_str("  (fn sparse ((x i8)) (i64)\n    (block entry ((x i8))\n      (switch x (case 0 (z)) (case 255 (neg)) (case 100 (hund)) (default (d))))\n");
    src.push_str("    (block z () (a (const i64 10)) (return a))\n    (block neg () (b (const i64 20)) (return b))\n    (block hund () (c (const i64 30)) (return c))\n    (block d () (e (const i64 40)) (return e))))\n");
    load(&src)
}

#[test]
fn switch_fifty_cases_jump_table() {
    let p = switch50_program();
    for x in [99i64, 100, 101, 124, 149, 150, 151, -1, 1000, i64::MIN, i64::MAX] {
        let want = if (100..150).contains(&x) { x * 3 + 1 } else { -1 };
        assert_eq!(run(&p, "sw", &[x as u64]), vec![want as u64], "sw {x}");
    }
    for x in [-1i32, 0, 1, 7, 25, 48, 49, 50, 51, i32::MAX, i32::MIN] {
        let want = if (0..50).contains(&x) { (x as i64) * (x as i64) + 7 } else { -1 };
        assert_eq!(run(&p, "sw2", &[x as i64 as u64]), vec![want as u64], "sw2 {x}");
    }
    for (x, want) in [(0i8, 10), (-1, 20), (100, 30), (1, 40), (-128, 40), (127, 40)] {
        assert_eq!(run(&p, "sparse", &[x as i64 as u64]), vec![want], "sparse {x}");
    }
}

#[test]
fn host_call_with_eight_args_and_narrow_result() {
    // Eight arguments exercise the stack-argument path of the call builder
    // (System V passes six in registers); the narrow host result must be
    // re-canonicalized before it is used as an i64.
    let p = load(
        r#"
(program
  (host sum8 (i64 i64 i64 i64 i64 i64 i64 i64) i64 pure)
  (host mix (i32 i8 i16 bool) i32 pure)
  (fn f ((a i64) (b i64) (c i64) (d i64) (e i64) (f i64) (g i64) (h i64)) (i64)
    (block entry ((a i64) (b i64) (c i64) (d i64) (e i64) (f i64) (g i64) (h i64))
      (s (call_host sum8 a b c d e f g h))
      (t (call_host sum8 h g f e d c b a))
      (r (isub s t))
      (return r)))
  (fn m ((a i32) (b i8) (c i16) (d bool)) (i64)
    (block entry ((a i32) (b i8) (c i16) (d bool))
      (r (call_host mix a b c d))
      (w (sext i64 r))
      (one (const i64 1))
      (v (iadd w one))
      (return v))))
"#,
    );
    let s = |v: &[i64]| v[0].wrapping_add(2 * v[1]).wrapping_add(3 * v[2]).wrapping_add(4 * v[3]).wrapping_add(5 * v[4]).wrapping_add(6 * v[5]).wrapping_add(7 * v[6]).wrapping_add(8 * v[7]);
    for args in [[1i64, 2, 3, 4, 5, 6, 7, 8], [-1, 100, -1000, 7, 0, 3, -9, 11], [1 << 40, 1, 1, 1, 1, 1, 1, -(1 << 40)]] {
        let mut rev = args;
        rev.reverse();
        let want = s(&args).wrapping_sub(s(&rev));
        assert_eq!(run(&p, "f", &args.map(|v| v as u64)), vec![want as u64]);
    }
    for (a, b, c, d) in [(1i32, 2i8, 3i16, true), (-5, -128, 32767, false), (i32::MAX, 1, 0, false), (i32::MIN, -1, -1, true)] {
        let want = a.wrapping_add(b as i32).wrapping_add(c as i32).wrapping_add(d as i32) as i64 + 1;
        assert_eq!(run(&p, "m", &[a as i64 as u64, b as i64 as u64, c as i64 as u64, d as u64]), vec![want as u64]);
    }
}

#[test]
fn load_zext_and_fused_immediate_compares() {
    // `loadz` of every narrow width from bytes with the high bit set, plus
    // compares against constants that fold into branch/compare immediates
    // (0, a small positive, a negative, and one outside imm12 range).
    let p = load(
        r#"
(program
  (fn lz ((p ptr)) (i64)
    (block entry ((p ptr))
      (a (loadz i8 p 0))
      (b (loadz i16 p 2 unaligned))
      (c (loadz i32 p 4))
      (s (load i8 p 0))
      (t (iadd a b))
      (u (iadd t c))
      (w (sext i64 s))
      (r (isub u w))
      (return r)))
  (fn cls ((x i64)) (i64)
    (block entry ((x i64))
      (k0 (const i64 0))
      (z (icmp eq x k0))
      (condbr z (ret0) (pos)))
    (block ret0 ()
      (a (const i64 100))
      (return a))
    (block pos ()
      (k1 (const i64 10))
      (lt (icmp slt x k1))
      (condbr lt (ret1) (neg)))
    (block ret1 ()
      (b (const i64 200))
      (return b))
    (block neg ()
      (k2 (const i64 -5000))
      (ge (icmp sge x k2))
      (k3 (const i64 70000))
      (big (icmp sgt x k3))
      (both (iand ge big))
      (condbr both (ret2) (ret3)))
    (block ret2 ()
      (c (const i64 300))
      (return c))
    (block ret3 ()
      (k4 (const i64 4095))
      (d (iadd x k4))
      (k5 (const i64 255))
      (e (iand d k5))
      (k6 (const i64 3))
      (f (shl e k6))
      (return f))))
"#,
    );
    let buf: [u8; 8] = [0x80, 0xff, 0xfe, 0xff, 0x00, 0x00, 0x00, 0x80];
    let expect = (0x80i64 + 0xfffe + 0x8000_0000) - (-128i64);
    assert_eq!(run(&p, "lz", &[buf.as_ptr() as u64]), vec![expect as u64]);
    for x in [0i64, 5, 9, 10, 11, -1, -4999, 70000, 70001, 1 << 40, -6000, i64::MIN] {
        let want = if x == 0 {
            100
        } else if x < 10 {
            200
        } else if x >= -5000 && x > 70000 {
            300
        } else {
            (((x.wrapping_add(4095)) & 255) << 3) as i64
        };
        assert_eq!(run(&p, "cls", &[x as u64]), vec![want as u64], "cls {x}");
    }
}

#[test]
fn int8_sub_overflow_branch() {
    // ssub_ovf feeding the branch directly (fused to subs + b.vs natively).
    let p = load(
        r#"
(program
  (fn f ((a i64) (b i64)) (i64)
    (block entry ((a i64) (b i64))
      (o (ssub_ovf a b))
      (condbr o (bad) (ok)))
    (block ok ()
      (d (isub a b))
      (return d))
    (block bad ()
      (m (const i64 -1))
      (return m))))
"#,
    );
    assert_eq!(run(&p, "f", &[10, 3]), vec![7]);
    assert_eq!(run(&p, "f", &[i64::MIN as u64, 1]), vec![u64::MAX]);
    assert_eq!(run(&p, "f", &[i64::MAX as u64, (-1i64) as u64]), vec![u64::MAX]);
    assert_eq!(run(&p, "f", &[(-5i64) as u64, (-7i64) as u64]), vec![2]);
}

#[test]
fn int4_add_overflow_by_self_sign_extension() {
    // The lowerer's narrow overflow test: 64-bit sum, overflow iff it is not
    // its own sign extension from 32 bits (fused natively to cmp x, w, sxtw).
    let p = load(
        r#"
(program
  (fn f ((a i64) (b i64)) (i64)
    (block entry ((a i64) (b i64))
      (r (iadd a b))
      (n (itrunc i32 r))
      (k (sext i64 n))
      (o (icmp ne r k))
      (condbr o (bad) (ok)))
    (block ok ()
      (return r))
    (block bad ()
      (m (const i64 -1))
      (return m))))
"#,
    );
    assert_eq!(run(&p, "f", &[10, 3]), vec![13]);
    assert_eq!(run(&p, "f", &[i32::MAX as u64, 1]), vec![u64::MAX]);
    assert_eq!(run(&p, "f", &[(i32::MIN as i64) as u64, (-1i64) as u64]), vec![u64::MAX]);
    assert_eq!(run(&p, "f", &[(-5i64) as u64, (-7i64) as u64]), vec![(-12i64) as u64]);
    assert_eq!(run(&p, "f", &[i32::MAX as u64, (-1i64) as u64]), vec![(i32::MAX - 1) as u64]);
}
