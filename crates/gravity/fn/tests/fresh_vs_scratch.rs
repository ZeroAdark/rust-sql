//! Michael's ruling 09-23 (restore PG's contract): an out-function's scratch buffer against a fresh allocation in the caller's
//! per-row context, in the row executor's shape (an armed frame, the per-tuple context reset every row). Run with --release.
mod common;

use std::alloc::Layout;
use std::ffi::CString;
use std::hint::black_box;
use std::time::Instant;

use datum::Datum;
use fmgr::{FmgrInfo, FunctionCallInfoBaseData as Fcinfo, LocalFcinfo, PGFunction};
use mcx::{Allocator, MemoryContext};
use types_error::PgResult;

const ROWS: usize = 10_000_000;

thread_local! {
    static OUT_SCRATCH: std::cell::UnsafeCell<[u8; 16]> = const { std::cell::UnsafeCell::new([0; 16]) };
}

/// trunk's int4out (a99fbce07a8): the thread-local scratch the slice removed, kept here as the reference arm.
fn scratch_int4out(_f: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let v = fcinfo.args_n::<1>()[0].value.as_i32();
    OUT_SCRATCH.with(|c| {
        // SAFETY: the sole live access on this thread.
        let buf = unsafe { &mut *c.get() };
        let len = adt_int::int4out(v, buf);
        buf[len] = 0;
        Ok(Datum::from_usize(buf.as_ptr() as usize))
    })
}

thread_local! {
    static OUT_SCRATCH8: std::cell::UnsafeCell<[u8; 24]> = const { std::cell::UnsafeCell::new([0; 24]) };
    static NUM_SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// trunk's int8out (a99fbce07a8): thread-local scratch, the reference arm.
fn scratch_int8out(_f: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let v = fcinfo.args_n::<1>()[0].value.as_i64();
    OUT_SCRATCH8.with(|c| {
        // SAFETY: the sole live access on this thread.
        let buf = unsafe { &mut *c.get() };
        let len = adt_int8::int8out(v, buf);
        buf[len] = 0;
        Ok(Datum::from_usize(buf.as_ptr() as usize))
    })
}

/// trunk's numeric_out (a99fbce07a8): a retained Vec (there in fn_extra), the reference arm.
fn scratch_numeric_out(_f: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    // SAFETY: arg 0 is a non-null numeric varlena.
    let num = unsafe { adt_numeric::builtins::num_arg(fcinfo, 0) }?;
    NUM_SCRATCH.with(|b| {
        let mut buf = b.borrow_mut();
        buf.clear();
        adt_numeric::io::numeric_out_into(num, &mut buf);
        buf.push(0);
        Ok(Datum::from_usize(buf.as_ptr() as usize))
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// the per-tuple context holds only the result: its reset does the whole rewind
    Clean,
    /// the per-tuple context already holds other bytes of the row (the executor's shape)
    Dirty,
    /// Gravity's shape: the call site's own context, reset right after its result is consumed (the row context stays dirty)
    Site,
}

/// ns per row: the frame armed with a per-tuple context, reset after every row.
fn run(func: PGFunction, oid: u32, args: &[Datum], mode: Mode) -> f64 {
    run_pad(func, oid, args, mode, 0)
}

/// `run` with `pad` bytes allocated in the result context before each call (the result then lands at that offset).
fn run_pad(func: PGFunction, oid: u32, args: &[Datum], mode: Mode, pad: usize) -> f64 {
    let mut cx = MemoryContext::new_bump("per-tuple");
    let mut site = MemoryContext::new_bump("site");
    let mut flinfo = FmgrInfo::new(func, oid, 1, true, false);
    let mut fc = Box::new(LocalFcinfo::<1>::new(0));
    let mut acc = 0usize;
    let t = Instant::now();
    for i in 0..ROWS {
        fc.rearm(0);
        // SAFETY: the context outlives the call; reset below after the result is consumed.
        unsafe { fc.set_result_mcx(if mode == Mode::Site { site.mcx() } else { cx.mcx() }) };
        if mode != Mode::Clean {
            black_box(cx.mcx().allocate(Layout::from_size_align(8, 8).unwrap()).unwrap());
        }
        if pad > 0 {
            let rcx = if mode == Mode::Site { &site } else { &cx };
            black_box(rcx.mcx().alloc_bytes(pad).unwrap());
        }
        fc.set_arg(0, args[i % args.len()]);
        let r = flinfo.invoke(&mut **fc).unwrap();
        // SAFETY: a NUL-terminated cstring result.
        acc += unsafe { std::ffi::CStr::from_ptr(r.as_usize() as *const core::ffi::c_char) }.to_bytes().len();
        black_box(acc);
        if mode == Mode::Site {
            site.reset();
        }
        cx.reset();
    }
    t.elapsed().as_nanos() as f64 / ROWS as f64
}

/// ns per row: the into-entry rendering at the end of a reused buffer (printtup's DataRow shape: reserve, render, advance).
fn run_into(e: &fmgr::OutInto, args: &[Datum]) -> f64 {
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 16);
    let mut acc = 0usize;
    let t = Instant::now();
    for i in 0..ROWS {
        if buf.len() > (1 << 15) {
            buf.clear();
        }
        let v = args[i % args.len()];
        let bound = (e.bound)(v).unwrap();
        buf.reserve(bound);
        let len = buf.len();
        // SAFETY: bound bytes reserved past len.
        let n = unsafe { (e.render)(v, buf.as_mut_ptr().add(len)) }.unwrap();
        unsafe { buf.set_len(len + n) };
        acc += n;
        black_box(acc);
    }
    t.elapsed().as_nanos() as f64 / ROWS as f64
}

fn numeric_images() -> Vec<Vec<u8>> {
    let mut call = gravity_fn::FmgrCall::bind(1701, 3, 0).unwrap();
    ["1234567.89", "-0.000123", "98765432109876543210.5", "42", "3.14159265358979"]
        .iter()
        .map(|s| {
            let c = CString::new(*s).unwrap();
            let r = call.call_site(&[Datum::from_usize(c.as_ptr() as usize), Datum::from_u32(1700), Datum::from_i32(-1)], &[false; 3]).unwrap();
            let p = r.value.as_usize() as *const u8;
            // SAFETY: a live numeric varlena.
            unsafe { std::slice::from_raw_parts(p, types_tuple::varatt::varsize_any(p)) }.to_vec()
        })
        .collect()
}

#[test]
#[ignore = "bench: cargo test --release --test fresh_vs_scratch -- --ignored --nocapture"]
fn fresh_vs_scratch() {
    common::install_seams();
    let ints: Vec<Datum> = (0..1024).map(|i| Datum::from_i32(i * 7919 - 4_000_000)).collect();
    let int8s: Vec<Datum> = (0..1024).map(|i| Datum::from_i64(i as i64 * 1_000_000_007 - 9_000_000_000)).collect();
    let nums = numeric_images();
    let numd: Vec<Datum> = nums.iter().map(|v| Datum::from_usize(v.as_ptr() as usize)).collect();
    let builtin = |oid: u32| fmgr_core::fmgr_isbuiltin(oid).unwrap().func;
    // (name, trunk's scratch body, the real builtin (fresh in the result context), oid, args)
    let arms: [(&str, PGFunction, PGFunction, u32, &[Datum]); 3] = [
        ("int4out", scratch_int4out, builtin(43), 43, &ints),
        ("int8out", scratch_int8out, builtin(461), 461, &int8s),
        ("numeric_out", scratch_numeric_out, builtin(1702), 1702, &numd),
    ];
    let med = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (v[v.len() / 2], v[0], v[v.len() - 1])
    };
    let into_of = |oid: u32| {
        [adt_int::builtins::INT_OUT_INTO, adt_int8::builtins::INT8_OUT_INTO, adt_numeric::builtins::NUMERIC_OUT_INTO]
            .into_iter()
            .flatten()
            .find(|e| e.foid == oid)
            .unwrap()
    };
    for (name, _, _, oid, args) in arms {
        let (mut s, mut n) = (Vec::new(), Vec::new());
        for _ in 0..7 {
            s.push(run(arms.iter().find(|a| a.3 == oid).unwrap().1, oid, args, Mode::Dirty));
            n.push(run_into(into_of(oid), args));
        }
        let ((sm, s0, s1), (nm, n0, n1)) = (med(s), med(n));
        println!("{name} [into, caller's buffer]: scratch {sm:.2} ns/row [{s0:.2}..{s1:.2}]  into {nm:.2} [{n0:.2}..{n1:.2}]  delta {:+.2} ns ({:+.1}%)", nm - sm, 100.0 * (nm - sm) / sm);
    }
    for (name, scratch, fresh, oid, args) in arms {
        for (label, fmode) in [("fresh, row ctx clean", Mode::Clean), ("fresh, row ctx dirty", Mode::Dirty), ("fresh, per-site ctx reset per call", Mode::Site)] {
            let (mut s, mut f) = (Vec::new(), Vec::new());
            for _ in 0..7 {
                s.push(run(scratch, oid, args, if fmode == Mode::Clean { Mode::Clean } else { Mode::Dirty }));
                f.push(run(fresh, oid, args, fmode));
            }
            let ((sm, s0, s1), (fm, f0, f1)) = (med(s), med(f));
            println!("{name} [{label}]: scratch {sm:.2} ns/row [{s0:.2}..{s1:.2}]  fresh {fm:.2} [{f0:.2}..{f1:.2}]  delta {:+.2} ns ({:+.1}%)", fm - sm, 100.0 * (fm - sm) / sm);
        }
    }
}

/// One arm for perf: FMGR_ARM=int4out|int8out|numeric_out, FMGR_MODE=clean|dirty|site, FMGR_FN=scratch|fresh; 5 runs.
#[test]
#[ignore = "perf target: FMGR_ARM/FMGR_MODE/FMGR_FN select one arm"]
fn one_arm() {
    common::install_seams();
    let ints: Vec<Datum> = (0..1024).map(|i| Datum::from_i32(i * 7919 - 4_000_000)).collect();
    let int8s: Vec<Datum> = (0..1024).map(|i| Datum::from_i64(i as i64 * 1_000_000_007 - 9_000_000_000)).collect();
    let nums = numeric_images();
    let numd: Vec<Datum> = nums.iter().map(|v| Datum::from_usize(v.as_ptr() as usize)).collect();
    let builtin = |oid: u32| fmgr_core::fmgr_isbuiltin(oid).unwrap().func;
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let (scratch, fresh, oid, args): (PGFunction, PGFunction, u32, &[Datum]) = match env("FMGR_ARM").as_str() {
        "int8out" => (scratch_int8out, builtin(461), 461, &int8s),
        "numeric_out" => (scratch_numeric_out, builtin(1702), 1702, &numd),
        _ => (scratch_int4out, builtin(43), 43, &ints),
    };
    let mode = match env("FMGR_MODE").as_str() {
        "clean" => Mode::Clean,
        "site" => Mode::Site,
        _ => Mode::Dirty,
    };
    let f = if env("FMGR_FN") == "scratch" { scratch } else { fresh };
    let pad = env("FMGR_PAD").parse().unwrap_or(0);
    for _ in 0..5 {
        println!("{:.2} ns/row", run_pad(f, oid, args, mode, pad));
    }
}
