//! The P7 microbenchmarks (docs/gravity/expr.md §4), plain tests with fixed iterations: a shim
//! call from a kernel-shaped caller (through the address a kernel would hold) against the typed
//! body's own work; `FmgrCall::call` against the incumbent's per-call path (`execexpr::invoke`:
//! the frame's argument stores, `isnull = false`, `FmgrInfo::invoke`); zero allocations per
//! by-value call. Numbers are printed; the bars are asserted in release only.
//! `cargo test --release -p gravity_fn --test hostcall -- --nocapture`

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::time::Instant;

use datum::Datum;
use fmgr::LocalFcinfo;
use gravity_fn::{Arg, ErrSlot, Region, Ret, View};

struct Counting;

thread_local! {
    /// Allocations made by THIS thread. A process-wide counter charges any other thread's allocations during the loop
    /// (libtest's main thread collecting results, a test's setup before it takes SERIAL) to the benchmark in flight:
    /// pass3_hand_adapters counted 5 on jsonb_path_exists on loaded gate boxes three times (2026-09-23 / 09-24).
    /// It counts only the calling thread: a measured call that hands work to another thread is not covered.
    static ALLOCS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// This thread's allocation count so far (see ALLOCS).
fn allocs_here() -> usize {
    ALLOCS.with(|c| c.get())
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let _ = ALLOCS.try_with(|c| c.set(c.get() + 1)); // try_with: a thread tearing down its TLS still allocates
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
}

#[global_allocator]
static A: Counting = Counting;

const ITERS: u64 = 1 << 22;

/// The benchmarks run one at a time (their timings; the allocation counter is per thread).
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

type ByVal2 = extern "C-unwind" fn(*mut ErrSlot, i64, i64) -> Ret;
type ViewVal2 = extern "C-unwind" fn(*mut ErrSlot, u32, View, View) -> Ret;
type ByRefRef2 = extern "C-unwind" fn(*mut ErrSlot, *mut Region, u32, View, View) -> Ret;

fn ns(t: Instant, n: u64) -> f64 {
    t.elapsed().as_nanos() as f64 / n as f64
}

/// The bars are asserted on the best of three runs: a shared machine's load noise is one-sided.
fn best_of<T>(mut f: impl FnMut() -> (f64, T)) -> (f64, T) {
    let mut best = f();
    for _ in 0..2 {
        let r = f();
        if r.0 < best.0 {
            best = r;
        }
    }
    best
}

fn release() -> bool {
    !cfg!(debug_assertions)
}

#[test]
fn shim_call_over_the_typed_body() {
    let _serial = serial();
    common::install_seams();
    let shim = gravity_fn::shim_of(177).unwrap();
    let f: ByVal2 = unsafe { std::mem::transmute(shim.addr()) };
    let mut slot = ErrSlot::NONE;
    let xs: Vec<i64> = (0..1024).map(|i| (i * 7919) % 100_000).collect();
    let mut acc = 0i64;
    for _ in 0..ITERS / 1024 {
        for i in 0..1024 {
            acc = acc.wrapping_add(adt_int::int4pl(black_box(xs[i] as i32), black_box(acc as i32 & 0xff)).unwrap() as i64);
        }
    }
    let t = Instant::now();
    for _ in 0..ITERS / 1024 {
        for i in 0..1024 {
            acc = acc.wrapping_add(adt_int::int4pl(black_box(xs[i] as i32), black_box(acc as i32 & 0xff)).unwrap() as i64);
        }
    }
    let body = ns(t, ITERS);
    let t = Instant::now();
    for _ in 0..ITERS / 1024 {
        for i in 0..1024 {
            let r = f(&mut slot, black_box(xs[i]), black_box(acc & 0xff));
            acc = acc.wrapping_add(r.val + r.status);
        }
    }
    let shim_ns = ns(t, ITERS);
    let before = allocs_here();
    for i in 0..4096 {
        let r = f(&mut slot, xs[i & 1023], 1);
        acc = acc.wrapping_add(r.val);
    }
    let allocs = allocs_here() - before;
    eprintln!("int4pl: typed body {body:.2} ns, shim {shim_ns:.2} ns ({:+.2} ns), allocations per 4096 by-value calls: {allocs}; acc {acc}", shim_ns - body);
    assert_eq!(allocs, 0);
    if release() {
        assert!(shim_ns - body <= 2.0, "shim overhead {:.2} ns > 2 ns", shim_ns - body);
    }
}

#[test]
fn view_shim_call_over_the_typed_body() {
    let _serial = serial();
    common::install_seams();
    let shim = gravity_fn::shim_of(67).unwrap(); // texteq
    let f: ViewVal2 = unsafe { std::mem::transmute(shim.addr()) };
    let mut slot = ErrSlot::NONE;
    let a = b"hello world, this is a text".to_vec();
    let b = b"hello world, this is a texu".to_vec();
    let (va, vb) = (View::of(&a), View::of(&b));
    let mut acc = 0i64;
    let t = Instant::now();
    for _ in 0..ITERS {
        acc += varlena::texteq(black_box(va).bytes(), black_box(vb).bytes(), 950).unwrap() as i64;
    }
    let body = ns(t, ITERS);
    let t = Instant::now();
    for _ in 0..ITERS {
        acc += f(&mut slot, 950, black_box(va), black_box(vb)).val;
    }
    let shim_ns = ns(t, ITERS);
    let before = allocs_here();
    for _ in 0..4096 {
        acc += f(&mut slot, 950, va, vb).val;
    }
    let allocs = allocs_here() - before;
    eprintln!("texteq (C collation): typed body {body:.2} ns, shim {shim_ns:.2} ns ({:+.2} ns), allocations per 4096 view calls: {allocs}; acc {acc}", shim_ns - body);
    assert_eq!(allocs, 0);
    if release() {
        assert!(shim_ns - body <= 2.0, "shim overhead {:.2} ns > 2 ns", shim_ns - body);
    }
}

#[test]
fn region_result_shim() {
    let _serial = serial();
    common::install_seams();
    let shim = gravity_fn::shim_of(1258).unwrap(); // textcat
    let f: ByRefRef2 = unsafe { std::mem::transmute(shim.addr()) };
    let mut slot = ErrSlot::NONE;
    let a = b"hello ".to_vec();
    let b = b"world".to_vec();
    let (va, vb) = (View::of(&a), View::of(&b));
    let mut buf = vec![0u8; 1 << 20];
    let base = buf.as_mut_ptr();
    let mut region = Region::over(&mut buf);
    let mut acc = 0i64;
    let t = Instant::now();
    for i in 0..ITERS {
        if i & 4095 == 0 {
            region.cur = base;
        }
        acc += f(&mut slot, &mut region, 100, black_box(va), black_box(vb)).status;
    }
    let shim_ns = ns(t, ITERS);
    let mut cx = mcx::MemoryContext::new_bump("bench");
    let t = Instant::now();
    for i in 0..ITERS {
        if i & 4095 == 0 {
            cx.reset();
        }
        acc += varlena::text_catenate(cx.mcx(), black_box(va).bytes(), black_box(vb).bytes()).unwrap().data().len() as i64;
    }
    let body = ns(t, ITERS);
    region.cur = base;
    let before = allocs_here();
    for _ in 0..4096 {
        acc += f(&mut slot, &mut region, 100, va, vb).status;
    }
    let allocs = allocs_here() - before;
    eprintln!("textcat: typed body into a bump context {body:.2} ns, shim into the region {shim_ns:.2} ns, allocations per 4096 region calls: {allocs}; acc {acc}");
    assert_eq!(allocs, 0);
}

type ByRefVal2 = extern "C-unwind" fn(*mut ErrSlot, View, View) -> Ret;
type ByRefRef2NoColl = extern "C-unwind" fn(*mut ErrSlot, *mut Region, View, View) -> Ret;

/// Pass 2's image families: a fixed-size by-reference argument (uuid_eq), a varlena image
/// argument (inet_eq) and a fixed-size by-reference result copied into the region (macaddr_and).
#[test]
fn image_family_shims_over_the_typed_bodies() {
    let _serial = serial();
    common::install_seams();
    let mut slot = ErrSlot::NONE;
    let a: [u8; 16] = [0xa0, 0xee, 0xbc, 0x99, 0x9c, 0x0b, 0x4e, 0xf8, 0xbb, 0x6d, 0x6b, 0xb9, 0xbd, 0x38, 0x0a, 0x11];
    let b: [u8; 16] = [0xa0, 0xee, 0xbc, 0x99, 0x9c, 0x0b, 0x4e, 0xf8, 0xbb, 0x6d, 0x6b, 0xb9, 0xbd, 0x38, 0x0a, 0x12];
    let (va, vb) = (View::of(&a), View::of(&b));
    let uuid_eq: ByRefVal2 = unsafe { std::mem::transmute(gravity_fn::shim_of(2956).unwrap().addr()) };
    let mut acc = 0i64;
    let (body, _) = best_of(|| {
        let t = Instant::now();
        for _ in 0..ITERS {
            acc += adt_uuid::uuid_eq(black_box(&a), black_box(&b)) as i64;
        }
        (ns(t, ITERS), ())
    });
    let (shim_ns, _) = best_of(|| {
        let t = Instant::now();
        for _ in 0..ITERS {
            acc += uuid_eq(&mut slot, black_box(va), black_box(vb)).val;
        }
        (ns(t, ITERS), ())
    });
    let before = allocs_here();
    for _ in 0..4096 {
        acc += uuid_eq(&mut slot, va, vb).val;
    }
    let allocs = allocs_here() - before;
    eprintln!("uuid_eq (fixed by-ref args): typed body {body:.2} ns, shim {shim_ns:.2} ns ({:+.2} ns), allocations per 4096 calls: {allocs}; acc {acc}", shim_ns - body);
    assert_eq!(allocs, 0);
    if release() {
        assert!(shim_ns - body <= 2.0, "uuid_eq shim overhead {:.2} ns > 2 ns", shim_ns - body);
    }

    // inet: two 4-byte-header images of the same /24 address
    let mut img = vec![0u8; 12];
    img[..4].copy_from_slice(&datum::set_varsize_4b(12));
    img[4..].copy_from_slice(&[2, 24, 0, 4, 192, 168, 1, 5]);
    let mut img2 = img.clone();
    img2[11] = 6;
    let (ia, ib) = (View::of(&img), View::of(&img2));
    let inet_eq: ByRefVal2 = unsafe { std::mem::transmute(gravity_fn::shim_of(920).unwrap().addr()) };
    let (body, _) = best_of(|| {
        let t = Instant::now();
        for _ in 0..ITERS {
            let x = adt_network::InetRef::from_payload(black_box(&img[4..]));
            let y = adt_network::InetRef::from_payload(black_box(&img2[4..]));
            acc += (adt_network::network_cmp_internal(x, y) == 0) as i64;
        }
        (ns(t, ITERS), ())
    });
    let (shim_ns, _) = best_of(|| {
        let t = Instant::now();
        for _ in 0..ITERS {
            acc += inet_eq(&mut slot, black_box(ia), black_box(ib)).val;
        }
        (ns(t, ITERS), ())
    });
    let before = allocs_here();
    for _ in 0..4096 {
        acc += inet_eq(&mut slot, ia, ib).val;
    }
    let allocs = allocs_here() - before;
    eprintln!("inet_eq (varlena image args): typed body {body:.2} ns, shim {shim_ns:.2} ns ({:+.2} ns), allocations per 4096 calls: {allocs}", shim_ns - body);
    assert_eq!(allocs, 0);
    if release() {
        assert!(shim_ns - body <= 2.0, "inet_eq shim overhead {:.2} ns > 2 ns", shim_ns - body);
    }

    // macaddr_and: a 6-byte by-reference result written into the region
    let ma: [u8; 6] = [0x08, 0x00, 0x2b, 0x01, 0x02, 0x03];
    let mb: [u8; 6] = [0xff, 0xff, 0xff, 0x00, 0x00, 0xff];
    let (va, vb) = (View::of(&ma), View::of(&mb));
    let macaddr_and: ByRefRef2NoColl = unsafe { std::mem::transmute(gravity_fn::shim_of(3145).unwrap().addr()) };
    let mut buf = vec![0u8; 1 << 20];
    let base = buf.as_mut_ptr();
    let mut region = Region::over(&mut buf);
    let (shim_ns, _) = best_of(|| {
        let t = Instant::now();
        for i in 0..ITERS {
            if i & 4095 == 0 {
                region.cur = base;
            }
            acc += macaddr_and(&mut slot, &mut region, black_box(va), black_box(vb)).status;
        }
        (ns(t, ITERS), ())
    });
    let (body, _) = best_of(|| {
        let t = Instant::now();
        for _ in 0..ITERS {
            let x = adt_mac::MacAddr::from_bytes(black_box(ma));
            let y = adt_mac::MacAddr::from_bytes(black_box(mb));
            acc += adt_mac::macaddr_and(&x, &y).to_bytes()[0] as i64;
        }
        (ns(t, ITERS), ())
    });
    region.cur = base;
    let before = allocs_here();
    for _ in 0..4096 {
        acc += macaddr_and(&mut slot, &mut region, va, vb).status;
    }
    let allocs = allocs_here() - before;
    eprintln!("macaddr_and (fixed by-ref result): typed body {body:.2} ns, shim into the region {shim_ns:.2} ns ({:+.2} ns), allocations per 4096 calls: {allocs}; acc {acc}", shim_ns - body);
    assert_eq!(allocs, 0);
}

#[test]
fn fmgr_call_equals_the_incumbent() {
    let _serial = serial();
    common::install_seams();
    let mut call = gravity_fn::FmgrCall::bind(177, 2, 0).unwrap();
    let args = [Datum::from_i32(7), Datum::from_i32(0)];
    let nulls = [false, false];
    let mut acc = 0i64;
    let t = Instant::now();
    for i in 0..ITERS {
        let a = [args[0], Datum::from_i32((i & 0xff) as i32)];
        acc += call.call_site(black_box(&a), &nulls).unwrap().value.as_i64();
    }
    let ours = ns(t, ITERS);
    let mut flinfo = fmgr_core::fmgr_info(177).unwrap();
    let mut fc = Box::new(LocalFcinfo::<2>::new(0));
    fc.rearm(0);
    let t = Instant::now();
    for i in 0..ITERS {
        fc.set_arg(0, args[0]);
        fc.set_arg(1, Datum::from_i32((i & 0xff) as i32));
        fc.isnull = false;
        acc += flinfo.invoke(black_box(&mut *fc)).unwrap().as_i64();
    }
    let incumbent = ns(t, ITERS);
    let before = allocs_here();
    for _ in 0..4096 {
        acc += call.call_site(&args, &nulls).unwrap().value.as_i64();
    }
    let allocs = allocs_here() - before;
    let thin = fmgr_core::fmgr_thin_builtin(&flinfo, 2).is_some();
    eprintln!("FmgrCall::call int4pl {ours:.2} ns vs the incumbent's frame + invoke {incumbent:.2} ns (thin entry registered: {thin}); allocations per 4096 calls: {allocs}; acc {acc}");
    assert_eq!(allocs, 0);
    if release() {
        assert!(ours <= incumbent + 1.0, "FmgrCall::call {ours:.2} ns > incumbent {incumbent:.2} ns + 1");
    }
}

#[test]
fn dynamic_entry_agrees_with_the_typed_entry() {
    let _serial = serial();
    let shim = gravity_fn::shim_of(177).unwrap();
    let f: ByVal2 = unsafe { std::mem::transmute(shim.addr()) };
    let mut slot = ErrSlot::NONE;
    let mut r = Region { cur: core::ptr::null_mut(), end: core::ptr::null_mut() };
    assert_eq!(f(&mut slot, 40, 2), (shim.call)(&mut slot, &mut r, 0, &[], &[Arg::V(40), Arg::V(2)]));
    assert_eq!(f(&mut slot, 40, 2).val, 42);
    let r1 = f(&mut slot, i32::MAX as i64, 1);
    assert_eq!(r1.status, gravity_fn::ERROR);
    assert_eq!(slot.take().unwrap().message(), "integer out of range");
}

/// An input function's image for a literal (`len` for a fixed-size type; a varlena's own size otherwise).
fn input_image(name: &str, lit: &str, arg1: u32, len: Option<usize>) -> Vec<u8> {
    let oid = fmgr_core::fmgr_lookup_by_name(name).unwrap().foid;
    let mut call = gravity_fn::FmgrCall::bind(oid, 3, 0).unwrap();
    let c = std::ffi::CString::new(lit).unwrap();
    let r = call.call_site(&[Datum::from_usize(c.as_ptr() as usize), Datum::from_u32(arg1), Datum::from_i32(-1)], &[false; 3]).unwrap();
    let p = r.value.as_usize() as *const u8;
    let n = len.unwrap_or_else(|| unsafe { types_tuple::varatt::varsize_any(p) });
    unsafe { std::slice::from_raw_parts(p, n) }.to_vec()
}

/// Pass 3's hand adapters: zero allocations per call for every one; the ≤ 2 ns bar for the two
/// that only compare and copy (timetz_larger, network_larger); the scratch-context ones printed.
#[test]
fn pass3_hand_adapters() {
    let _serial = serial();
    common::install_seams();
    type Path4 = extern "C-unwind" fn(*mut ErrSlot, View, View, View, i64) -> Ret;
    type PathImg4 = extern "C-unwind" fn(*mut ErrSlot, *mut Region, View, View, View, i64) -> Ret;
    type ByRefRef1 = extern "C-unwind" fn(*mut ErrSlot, *mut Region, View) -> Ret;
    let mut slot = ErrSlot::NONE;
    let mut buf = vec![0u8; 1 << 20];
    let base = buf.as_mut_ptr();
    let mut region = Region::over(&mut buf);
    let jb = input_image("jsonb_in", "{\"a\": 1, \"b\": [1, 2, 3]}", 0, None);
    let jp = input_image("jsonpath_in", "$.b[*] ? (@ > 1)", 0, None);
    let vars = input_image("jsonb_in", "{}", 0, None);
    let mut cx = mcx::MemoryContext::new_bump("hostcall arrays");
    let arr = arrayfuncs::construct_array(cx.mcx(), &[Datum::from_i32(1), Datum::from_i32(2), Datum::from_i32(3)], 23, 4, true, arrayfuncs::foundation::TYPALIGN_INT).unwrap().to_vec();
    let tz_a = input_image("timetz_in", "12:34:56+05", 0, Some(12));
    let tz_b = input_image("timetz_in", "12:34:57+05", 0, Some(12));
    let in_a = input_image("inet_in", "192.168.1.5/24", 0, None);
    let in_b = input_image("inet_in", "192.168.1.6/24", 0, None);
    let name = b"pg_class\0".to_vec();
    let pat = b"PG\\_%".to_vec();
    let mut acc = 0i64;
    let mut count = |label: &str, mut f: &mut dyn FnMut() -> i64| {
        acc += f();
        let before = allocs_here();
        let t = Instant::now();
        for _ in 0..4096 {
            acc += f();
        }
        let per = ns(t, 4096);
        let allocs = allocs_here() - before;
        eprintln!("{label}: {per:.1} ns per call, allocations per 4096 calls: {allocs}");
        assert_eq!(allocs, 0, "{label}");
        per
    };
    let (vjb, vjp, vv, va, vtz_a, vtz_b, vin_a, vin_b) = (View::of(&jb), View::of(&jp), View::of(&vars), View::of(&arr), View::of(&tz_a), View::of(&tz_b), View::of(&in_a), View::of(&in_b));
    for (oid, label) in [(4005, "jsonb_path_exists"), (4009, "jsonb_path_match"), (1177, "jsonb_path_exists_tz"), (2030, "jsonb_path_match_tz")] {
        let f: Path4 = unsafe { std::mem::transmute(gravity_fn::shim_of(oid).unwrap().addr()) };
        count(label, &mut || f(&mut slot, black_box(vjb), black_box(vjp), black_box(vv), 1).status);
    }
    for (oid, label) in [(4010, "jsonb_path_exists_opr"), (4011, "jsonb_path_match_opr")] {
        let f: ByRefVal2 = unsafe { std::mem::transmute(gravity_fn::shim_of(oid).unwrap().addr()) };
        count(label, &mut || f(&mut slot, black_box(vjb), black_box(vjp)).status);
    }
    for (oid, label) in [(4007, "jsonb_path_query_array"), (4008, "jsonb_path_query_first"), (1180, "jsonb_path_query_array_tz"), (2023, "jsonb_path_query_first_tz")] {
        let f: PathImg4 = unsafe { std::mem::transmute(gravity_fn::shim_of(oid).unwrap().addr()) };
        count(label, &mut || {
            region.cur = base;
            f(&mut slot, &mut region, black_box(vjb), black_box(vjp), black_box(vv), 1).status
        });
    }
    let array_dims: ByRefRef1 = unsafe { std::mem::transmute(gravity_fn::shim_of(747).unwrap().addr()) };
    count("array_dims", &mut || {
        region.cur = base;
        array_dims(&mut slot, &mut region, black_box(va)).status
    });
    let (vn, vp) = (View::of(&name[..8]), View::of(&pat));
    for (oid, label) in [(1635, "nameiclike"), (1636, "nameicnlike")] {
        let f: ViewVal2 = unsafe { std::mem::transmute(gravity_fn::shim_of(oid).unwrap().addr()) };
        count(label, &mut || f(&mut slot, 950, black_box(vn), black_box(vp)).val);
    }
    let timetz_larger: ByRefRef2NoColl = unsafe { std::mem::transmute(gravity_fn::shim_of(1379).unwrap().addr()) };
    let network_larger: ByRefRef2NoColl = unsafe { std::mem::transmute(gravity_fn::shim_of(3562).unwrap().addr()) };
    for (f, label, a, b) in [(timetz_larger, "timetz_larger", vtz_a, vtz_b), (network_larger, "network_larger", vin_a, vin_b)] {
        count(label, &mut || {
            region.cur = base;
            f(&mut slot, &mut region, black_box(a), black_box(b)).status
        });
    }
    let (shim_ns, _) = best_of(|| {
        let t = Instant::now();
        for i in 0..ITERS {
            if i & 4095 == 0 {
                region.cur = base;
            }
            acc += timetz_larger(&mut slot, &mut region, black_box(vtz_a), black_box(vtz_b)).status;
        }
        (ns(t, ITERS), ())
    });
    let tt = |v: &[u8]| adt_date::TimeTzADT { time: i64::from_ne_bytes(v[..8].try_into().unwrap()), zone: i32::from_ne_bytes(v[8..12].try_into().unwrap()) };
    let mut out = [0u8; 12];
    let (body, _) = best_of(|| {
        let t = Instant::now();
        for _ in 0..ITERS {
            let pick = if adt_date::timetz_cmp_internal(&tt(black_box(&tz_a)), &tt(black_box(&tz_b))) > 0 { &tz_a } else { &tz_b };
            out.copy_from_slice(black_box(pick));
            acc += out[0] as i64;
        }
        (ns(t, ITERS), ())
    });
    eprintln!("timetz_larger: typed body + copy {body:.2} ns, shim into the region {shim_ns:.2} ns ({:+.2} ns); acc {acc}", shim_ns - body);
    if release() {
        assert!(shim_ns - body <= 2.0, "timetz_larger shim overhead {:.2} ns > 2 ns", shim_ns - body);
    }
    let (shim_ns, _) = best_of(|| {
        let t = Instant::now();
        for i in 0..ITERS {
            if i & 4095 == 0 {
                region.cur = base;
            }
            acc += network_larger(&mut slot, &mut region, black_box(vin_a), black_box(vin_b)).status;
        }
        (ns(t, ITERS), ())
    });
    let mut out = [0u8; 32];
    let (body, _) = best_of(|| {
        let t = Instant::now();
        for _ in 0..ITERS {
            let x = adt_network::InetRef::from_payload(black_box(&in_a[4..]));
            let y = adt_network::InetRef::from_payload(black_box(&in_b[4..]));
            let pick = if adt_network::network_cmp_internal(x, y) > 0 { &in_a } else { &in_b };
            out[..pick.len()].copy_from_slice(black_box(pick));
            acc += out[4] as i64;
        }
        (ns(t, ITERS), ())
    });
    eprintln!("network_larger: typed body + copy {body:.2} ns, shim into the region {shim_ns:.2} ns ({:+.2} ns); acc {acc}", shim_ns - body);
    if release() {
        assert!(shim_ns - body <= 2.0, "network_larger shim overhead {:.2} ns > 2 ns", shim_ns - body);
    }
}

/// The function-usage bracket (MH-work §B.3): a builtin's `fn_stats` is `TRACK_FUNC_ALL`, never tracked — under
/// `track_functions = all` it binds untracked and still takes the thin path, with the incumbent's answer; a tracked
/// site (a PL / SQL / C function under the GUC) needs the stats shmem: e2e/gravity/v2udf.sql's stats section counts it.
#[test]
fn a_builtin_is_never_bracketed_by_the_function_usage_counters() {
    let _serial = serial();
    common::install_seams();
    for track in [0, 1, 2] {
        pgstat::function::set_pgstat_track_functions(track);
        let mut call = gravity_fn::FmgrCall::bind(177, 2, 0).unwrap();
        assert!(!call.tracked(), "track_functions = {track}: int4pl is a builtin");
        let r = call.call_site(&[Datum::from_i32(black_box(40)), Datum::from_i32(black_box(2))], &[false, false]).unwrap();
        assert_eq!((r.value.as_i64() as i32, r.isnull), (42, false));
    }
    pgstat::function::set_pgstat_track_functions(0);
}
