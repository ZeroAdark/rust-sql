//! `gravity_interp/tests/programs.rs`'s text corpus (loops, parallel
//! copies, memory, recursion, `switch`, narrow widths, f32, hosts with
//! stack slots) through the wasm backend under wasmtime, against the
//! interpreter; pointer arguments live in the guest memory on the wasm
//! side and in native memory on the interpreter's.

mod common;
use common::*;
use gravity_interp::Interp;
use gravity_ir::text::parse_program;
use gravity_ir::*;

extern "C" fn host_mul3(a: i64, b: i64, c: i64) -> i64 {
    a * b * c
}
extern "C" fn host_swap_i64(p: u64, q: u64) {
    unsafe { core::ptr::swap(p as *mut i64, q as *mut i64) }
}
extern "C" fn host_sum8(a: i64, b: i64, c: i64, d: i64, e: i64, f: i64, g: i64, h: i64) -> i64 {
    a + 2 * b + 3 * c + 4 * d + 5 * e + 6 * f + 7 * g + 8 * h
}
extern "C" fn host_mix(a: i32, b: i8, c: i16, d: bool) -> i32 {
    a.wrapping_add(b as i32).wrapping_add(c as i32).wrapping_add(d as i32)
}

fn load(src: &str) -> Program {
    parse_program(src, &|name| match name {
        "mul3" => Some(host_mul3 as extern "C" fn(i64, i64, i64) -> i64 as usize),
        "swap_i64" => Some(host_swap_i64 as extern "C" fn(u64, u64) as usize),
        "sum8" => Some(host_sum8 as extern "C" fn(i64, i64, i64, i64, i64, i64, i64, i64) -> i64 as usize),
        "mix" => Some(host_mix as extern "C" fn(i32, i8, i16, bool) -> i32 as usize),
        _ => None,
    })
    .unwrap_or_else(|e| panic!("{e}"))
}

fn func(p: &Program, name: &str) -> Func {
    Func(p.funcs.iter().position(|f| f.name == name).unwrap() as u32)
}

/// The guest-side host table for the corpus's hosts.
fn hosts() -> Vec<(&'static str, HostImpl)> {
    vec![
        ("mul3", Box::new(|_: &mut [u8], a: &[u64]| (a[0] as i64 * a[1] as i64 * a[2] as i64) as u64)),
        (
            "swap_i64",
            Box::new(|m: &mut [u8], a: &[u64]| {
                let (p, q) = (a[0] as usize, a[1] as usize);
                let x: [u8; 8] = m[p..p + 8].try_into().unwrap();
                let y: [u8; 8] = m[q..q + 8].try_into().unwrap();
                m[p..p + 8].copy_from_slice(&y);
                m[q..q + 8].copy_from_slice(&x);
                0
            }),
        ),
        ("sum8", Box::new(|_: &mut [u8], a: &[u64]| (host_sum8(a[0] as i64, a[1] as i64, a[2] as i64, a[3] as i64, a[4] as i64, a[5] as i64, a[6] as i64, a[7] as i64)) as u64)),
        ("mix", Box::new(|_: &mut [u8], a: &[u64]| host_mix(a[0] as i32, a[1] as i8, a[2] as i16, a[3] != 0) as i64 as u64)),
    ]
}

/// Scalar-only programs: the same arguments to both backends.
fn scalar(src: &str, name: &str, cases: &[&[u64]]) {
    let mut rig = Rig::new();
    let p0 = load(src);
    let p = rig.bind(&p0, hosts());
    let f = func(&p, name);
    let (inst, _) = rig.instantiate(&p, f);
    for args in cases {
        let want = Interp::new(&p0).run(f, args).unwrap();
        let got = rig.call(&inst, &p, f, args).unwrap();
        assert_eq!(got, want, "{name}{args:?}");
    }
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

#[test]
fn gcd_euclid() {
    scalar(GCD, "gcd", &[&[48, 18], &[17, 5], &[0, 9], &[100, 100], &[1071, 462]]);
}

#[test]
fn fibonacci_iterative_with_parallel_copy() {
    let src = r#"
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
    let cases: Vec<Vec<u64>> = (0..13u64).map(|n| vec![n]).chain([vec![90]]).collect();
    let refs: Vec<&[u64]> = cases.iter().map(|c| c.as_slice()).collect();
    scalar(src, "fib", &refs);
}

#[test]
fn swap_via_edge_args() {
    scalar(
        r#"
(program
  (fn swap ((a i64) (b i64)) (i64 i64)
    (block entry ((a i64) (b i64))
      (br out b a))
    (block out ((x i64) (y i64))
      (return x y))))
"#,
        "swap",
        &[&[1, 2], &[u64::MAX, 0]],
    );
}

#[test]
fn binary_search_i32_array() {
    let src = r#"
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
"#;
    let mut rig = Rig::new();
    let p = load(src);
    let (inst, _) = rig.instantiate(&p, Func(0));
    let arr: Vec<i32> = vec![-50, -3, 0, 4, 9, 15, 16, 100, 1000];
    rig.write(DATA, &arr.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>());
    for k in arr.iter().copied().chain([-100, -1, 5, 17, 5000]) {
        let want = Interp::new(&p).run(Func(0), &[arr.as_ptr() as u64, arr.len() as u64, k as i64 as u64]).unwrap();
        let got = rig.call(&inst, &p, Func(0), &[DATA as u64, arr.len() as u64, k as i64 as u64]).unwrap();
        assert_eq!(got, want, "key {k}");
    }
}

#[test]
fn bubble_sort_in_place() {
    let src = r#"
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
"#;
    let mut rig = Rig::new();
    let p = load(src);
    let (inst, _) = rig.instantiate(&p, Func(0));
    let data: Vec<i64> = vec![5, -1, 9, 3, 3, 0, -100, 42, 7, 1];
    rig.write(DATA, &data.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>());
    rig.call(&inst, &p, Func(0), &[DATA as u64, data.len() as u64]).unwrap();
    let mut want = data.clone();
    want.sort();
    let got: Vec<i64> = rig.read(DATA, 80).chunks(8).map(|c| i64::from_le_bytes(c.try_into().unwrap())).collect();
    assert_eq!(got, want);
}

#[test]
fn fnv1a_hash_bytes() {
    let src = r#"
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
"#;
    let mut rig = Rig::new();
    let p = load(src);
    let (inst, _) = rig.instantiate(&p, Func(0));
    for s in ["", "a", "hello", "the quick brown fox", "\u{ff}\u{fe}bytes"] {
        rig.write(DATA, s.as_bytes());
        let want = Interp::new(&p).run(Func(0), &[s.as_ptr() as u64, s.len() as u64]).unwrap();
        let got = rig.call(&inst, &p, Func(0), &[DATA as u64, s.len() as u64]).unwrap();
        assert_eq!(got, want, "{s:?}");
    }
}

#[test]
fn collatz_steps() {
    scalar(
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
        "collatz",
        &[&[1], &[2], &[3], &[6], &[7], &[27], &[97]],
    );
}

#[test]
fn switch_state_machine_counts_words() {
    let src = r#"
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
"#;
    let mut rig = Rig::new();
    let p = load(src);
    let (inst, _) = rig.instantiate(&p, Func(0));
    for s in ["", "a", "hello world", "  leading  and   trailing  ", "one", "   "] {
        rig.write(DATA, s.as_bytes());
        let want = Interp::new(&p).run(Func(0), &[s.as_ptr() as u64, s.len() as u64]).unwrap();
        let got = rig.call(&inst, &p, Func(0), &[DATA as u64, s.len() as u64]).unwrap();
        assert_eq!(got, want, "{s:?}");
    }
}

#[test]
fn mutual_recursion_even_odd() {
    let src = r#"
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
"#;
    let cases: Vec<Vec<u64>> = (0..20u64).map(|n| vec![n]).collect();
    let refs: Vec<&[u64]> = cases.iter().map(|c| c.as_slice()).collect();
    scalar(src, "is_even", &refs);
    scalar(src, "is_odd", &refs);
}

#[test]
fn narrow_width_wraparound_counter() {
    scalar(
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
        "f",
        &[&[]],
    );
}

#[test]
fn f32_accumulation_rounds_like_f32() {
    scalar(
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
        "sum",
        &[&[1000], &[0], &[7]],
    );
}

#[test]
fn host_calls_with_stack_slots_and_narrow_signatures() {
    let src = r#"
(program
  (host mul3 (i64 i64 i64) i64 pure)
  (host swap_i64 (ptr ptr) void touches_memory)
  (host sum8 (i64 i64 i64 i64 i64 i64 i64 i64) i64 pure)
  (host mix (i32 i8 i16 bool) i32 pure)
  (fn f ((a i64) (b i64) (c i64)) (i64 i64 i64 i32)
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
      (s (call_host sum8 a b c x y m a b))
      (a32 (itrunc i32 a))
      (b8 (itrunc i8 b))
      (c16 (itrunc i16 c))
      (z (const i64 0))
      (t (icmp ne c z))
      (mx (call_host mix a32 b8 c16 t))
      (return x y s mx))))
"#;
    scalar(src, "f", &[&[2, 3, 4], &[-1i64 as u64, 200, 70000], &[1 << 40, -128i64 as u64, 0]]);
}

/// A merge node inside a loop inside a diamond: nested `block`/`loop`
/// labels resolve at every depth.
#[test]
fn nested_loops_and_diamonds() {
    scalar(
        r#"
(program
  (fn f ((n i64) (m i64)) (i64)
    (block entry ((n i64) (m i64))
      (z (const i64 0))
      (neg (icmp slt n z))
      (condbr neg (exit z) (outer z z)))
    (block outer ((i i64) (acc i64))
      (done (icmp sge i n))
      (condbr done (exit acc) (inner i acc z)))
    (block inner ((i2 i64) (acc2 i64) (j i64))
      (jd (icmp sge j m))
      (condbr jd (next i2 acc2) (body i2 acc2 j)))
    (block body ((i3 i64) (acc3 i64) (j2 i64))
      (one (const i64 1))
      (odd (iand j2 one))
      (isodd (icmp ne odd z))
      (condbr isodd (addj i3 acc3 j2) (addi i3 acc3 j2)))
    (block addj ((i4 i64) (acc4 i64) (j3 i64))
      (a (iadd acc4 j3))
      (br join i4 a j3))
    (block addi ((i5 i64) (acc5 i64) (j4 i64))
      (a2 (iadd acc5 i5))
      (br join i5 a2 j4))
    (block join ((i6 i64) (acc6 i64) (j5 i64))
      (one2 (const i64 1))
      (j6 (iadd j5 one2))
      (br inner i6 acc6 j6))
    (block next ((i7 i64) (acc7 i64))
      (one3 (const i64 1))
      (i8 (iadd i7 one3))
      (br outer i8 acc7))
    (block exit ((r i64))
      (return r))))
"#,
        "f",
        &[&[0, 0], &[3, 4], &[10, 7], &[-1i64 as u64, 5], &[5, 0]],
    );
}
