//! Diagnostic only (the bar for wasm speed is a later stage): the corpus
//! loops under wasmtime against the interpreter, and the compiler's own
//! latency per program; printed with `--nocapture`.

mod common;
use common::*;
use gravity_interp::Interp;
use gravity_ir::text::parse_program;
use gravity_ir::*;
use std::time::Instant;

const COLLATZ: &str = r#"
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
"#;

const FNV: &str = r#"
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

fn median(v: &mut [u128]) -> u128 {
    v.sort();
    v[v.len() / 2]
}

#[test]
fn corpus_loops_wasmtime_vs_interpreter() {
    let mut rig = Rig::new();
    let mut lines = Vec::new();
    for (name, src, args_native, args_guest, setup) in [
        ("collatz(837799)", COLLATZ, vec![837_799u64], vec![837_799u64], None),
        ("fnv(64 KiB)", FNV, vec![0, 65_536], vec![DATA as u64, 65_536], Some(65_536usize)),
    ] {
        let p = parse_program(src, &|_| None).unwrap();
        let buf: Vec<u8> = (0..setup.unwrap_or(0)).map(|k| (k * 31 % 251) as u8).collect();
        let mut args_native = args_native;
        if setup.is_some() {
            args_native[0] = buf.as_ptr() as u64;
            rig.write(DATA, &buf);
        }
        let mut t_compile = Vec::new();
        let mut bytes = 0;
        for _ in 0..50 {
            let t0 = Instant::now();
            bytes = gravity_wasm::compile(&p, Func(0)).unwrap().len();
            t_compile.push(t0.elapsed().as_nanos());
        }
        let t0 = Instant::now();
        let (inst, _) = rig.instantiate(&p, Func(0));
        let t_inst = t0.elapsed().as_nanos();
        let mut t_wasm = Vec::new();
        let mut t_interp = Vec::new();
        let mut got = Vec::new();
        let mut want = Vec::new();
        for _ in 0..5 {
            let t0 = Instant::now();
            got = rig.call(&inst, &p, Func(0), &args_guest).unwrap();
            t_wasm.push(t0.elapsed().as_nanos());
            let t0 = Instant::now();
            want = Interp::new(&p).run(Func(0), &args_native).unwrap();
            t_interp.push(t0.elapsed().as_nanos());
        }
        assert_eq!(got, want);
        lines.push(format!(
            "{name}: compile {} us (median of 50), module {bytes} bytes, wasmtime compile+instantiate {} us, run wasm {} us vs interpreter {} us (median of 5)",
            median(&mut t_compile) / 1000,
            t_inst / 1000,
            median(&mut t_wasm) / 1000,
            median(&mut t_interp) / 1000
        ));
    }
    for l in lines {
        println!("{l}");
    }
}
