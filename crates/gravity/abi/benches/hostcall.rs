//! The ABI microbenchmark (docs/gravity/abi-v2.md §6), a plain test with fixed iterations:
//! `cargo test -p gravity_abi --bench hostcall -- --nocapture`. Each class is measured on the v2
//! mechanism (a C-unwind pointer call, the window written in place, one load, one bump) and on the
//! v3-milestone-1 mechanism it replaced (`catch_unwind` guard + `Result` + `dyn` dispatch + the
//! twelve-store marshal), written out here so the comparison stays reproducible after the port.
use std::borrow::Cow;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Instant;

use gravity_abi::ctx::{arena_bump, cancel_pending, lane, Ctx, Lane, Out, ScanPos, HEADER_SIZE};
use gravity_abi::program::ProgramBlock;
use gravity_abi::site::{self, Site, SITE_SIZE};
use gravity_abi::status;

const ROWS: usize = 8192;
const ITERS: u64 = 1 << 21;
static COL0: [u64; ROWS] = [7; ROWS];
static COL1: [u64; ROWS] = [9; ROWS];

// ---- the source's own work, shared by both arms: two lanes, rows, the base position ----

struct Resident;

trait DynSource {
    fn window(&mut self, lanes: &mut [Lane], rows: &mut u32, pos: &mut ScanPos) -> Result<u32, Cow<'static, str>>;
}

impl DynSource for Resident {
    #[inline(never)]
    fn window(&mut self, lanes: &mut [Lane], rows: &mut u32, pos: &mut ScanPos) -> Result<u32, Cow<'static, str>> {
        lanes[0] = Lane::byval(COL0.as_ptr() as *const u8, core::ptr::null(), 8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, 1));
        lanes[1] = Lane::byval(COL1.as_ptr() as *const u8, core::ptr::null(), 8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, 2));
        *rows = ROWS as u32;
        *pos = ScanPos::new(77, 1, 0);
        Ok(ROWS as u32)
    }
}

// ---- the v3-milestone-1 mechanism: env → Inner, guard, dyn, marshal ----

struct OldInner {
    source: Box<dyn DynSource>,
    err: Option<Cow<'static, str>>,
    site: *mut Site,
    lanes: [Lane; 4],
    pushes: u64,
}

impl OldInner {
    fn guard(&mut self, f: impl FnOnce(&mut OldInner) -> Result<i64, Cow<'static, str>>) -> i64 {
        // unwind-ok: the mechanism under measurement
        match catch_unwind(AssertUnwindSafe(|| f(self))) {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                self.err = Some(e);
                -1
            }
            Err(_) => -1,
        }
    }
}

unsafe extern "C" fn old_window(env: *mut u8, _site: i64) -> i64 {
    let n = &mut *(env as *mut OldInner);
    n.guard(|n| {
        let mut rows = 0u32;
        let mut pos = ScanPos::NONE;
        let mut lanes = n.lanes;
        let got = n.source.window(&mut lanes, &mut rows, &mut pos)?;
        n.lanes = lanes;
        let s = &mut *n.site;
        core::ptr::copy_nonoverlapping(n.lanes.as_ptr(), s.lanes, 2);
        s.rows = rows;
        s.cursor = 0;
        s.sel = core::ptr::null();
        s.sel_n = 0;
        s.nlanes = 2;
        s.pos = pos;
        s.part_id = 0;
        s.dict_epoch = 0;
        s.granule = 0;
        s.claim_id = 0;
        s.answered = 0;
        s.answer = core::ptr::null();
        s.rows_in += rows as u64;
        Ok(got as i64)
    })
}

unsafe extern "C" fn old_push(env: *mut u8) -> i64 {
    let n = &mut *(env as *mut OldInner);
    n.guard(|n| {
        n.pushes += 1;
        Ok(0)
    })
}

unsafe extern "C" fn old_check(env: *mut u8) -> i64 {
    let n = &mut *(env as *mut OldInner);
    n.guard(|_| Ok(0))
}

unsafe extern "C" fn old_scratch(env: *mut u8, bytes: i64) -> i64 {
    let n = &mut *(env as *mut OldInner);
    core::hint::black_box(n);
    bytes
}

// ---- the v2 mechanism: the site pointer is the environment ----

unsafe extern "C" fn v2_next(s: *mut Site) -> i64 {
    let src = &mut *((*s).src as *mut Resident);
    let s = &mut *s;
    let lanes = core::slice::from_raw_parts_mut(s.lanes, 2);
    match src.window(lanes, &mut s.rows, &mut s.pos) {
        Ok(n) => {
            s.cursor = 0;
            s.nlanes = 2;
            s.rows_in += n as u64;
            n as i64
        }
        Err(_) => status::ERROR,
    }
}

unsafe extern "C" fn v2_push(ctx: *mut Ctx) -> i64 {
    let n = &mut *((*(*ctx).program).env as *mut OldInner);
    n.pushes += 1;
    (*(*ctx).out).count = 0;
    status::OK
}

#[inline(never)]
unsafe extern "C" fn v2_grow(ctx: *mut Ctx, bytes: u64) -> i64 {
    // the cold call: the arena is rewound to its start (the bench never runs out)
    let n = &mut *((*(*ctx).program).env as *mut OldInner);
    core::hint::black_box(n);
    (*ctx).arena_cur = (*ctx).arena_end.sub(1 << 20);
    bytes as i64
}

/// The v3 host table shape: the old arms are called through it (never a direct call).
struct OldTable {
    window: unsafe extern "C" fn(*mut u8, i64) -> i64,
    push: unsafe extern "C" fn(*mut u8) -> i64,
    check: unsafe extern "C" fn(*mut u8) -> i64,
    scratch: unsafe extern "C" fn(*mut u8, i64) -> i64,
}

static OLD: OldTable = OldTable { window: old_window, push: old_push, check: old_check, scratch: old_scratch };

struct Rig {
    buf: Vec<u64>,
    lanes: Vec<Lane>,
    out: Box<Out>,
    prog: Box<ProgramBlock>,
    inner: Box<OldInner>,
    arena: Vec<u64>,
    byte: core::sync::atomic::AtomicU8,
}

impl Rig {
    fn new() -> Rig {
        let mut buf = vec![0u64; (HEADER_SIZE + SITE_SIZE) / 8];
        let ctx = buf.as_mut_ptr() as *mut Ctx;
        let mut lanes = vec![Lane::NONE; 4];
        let mut out = Box::new(Out { values: core::ptr::null_mut(), isnull: core::ptr::null_mut(), count: 0, cap: 64, region: core::ptr::null_mut(), region_cap: 0, region_len: 0 });
        let mut prog = Box::new(ProgramBlock::NONE);
        let site = unsafe { (ctx as *mut u8).add(HEADER_SIZE) as *mut Site };
        let mut inner = Box::new(OldInner { source: Box::new(Resident), err: None, site, lanes: [Lane::NONE; 4], pushes: 0 });
        let mut arena = vec![0u64; (1 << 20) / 8];
        let rig_byte = core::sync::atomic::AtomicU8::new(0);
        prog.push = Some(v2_push);
        prog.arena_grow = Some(v2_grow);
        prog.env = &mut *inner as *mut OldInner as *mut u8;
        unsafe {
            (*ctx).program = &mut *prog;
            (*ctx).sites = site;
            (*ctx).out = &mut *out;
            (*ctx).nsites = 1;
            (*ctx).arena_cur = arena.as_mut_ptr() as *mut u8;
            (*ctx).arena_end = (arena.as_mut_ptr() as *mut u8).add(1 << 20);
            (*site).lanes = lanes.as_mut_ptr();
            (*site).next = Some(v2_next);
            (*site).src = core::ptr::addr_of_mut!(*inner.source) as *mut u8;
            (*site).env = prog.env;
            (*site).state = site::site::STATE_OPEN;
        }
        Rig { buf, lanes, out, prog, inner, arena, byte: rig_byte }
    }

    fn ctx(&mut self) -> *mut Ctx {
        self.buf.as_mut_ptr() as *mut Ctx
    }

    fn site(&mut self) -> *mut Site {
        unsafe { (self.ctx() as *mut u8).add(HEADER_SIZE) as *mut Site }
    }

    fn env(&mut self) -> *mut u8 {
        &mut *self.inner as *mut OldInner as *mut u8
    }
}

fn ns(name: &str, iters: u64, f: impl FnOnce()) -> f64 {
    let t = Instant::now();
    f();
    let ns = t.elapsed().as_nanos() as f64 / iters as f64;
    println!("hostcall {name}: {ns:.2} ns");
    ns
}

#[test]
fn hostcall_table() {
    let mut rig = Rig::new();
    let (ctx, site, env) = (rig.ctx(), rig.site(), rig.env());
    // Site.next
    let src_only = ns("source work alone (2 lanes + rows + pos, dyn call)", ITERS, || {
        let s = unsafe { &mut *site };
        let mut acc = 0u64;
        for _ in 0..ITERS {
            let lanes = unsafe { core::slice::from_raw_parts_mut(s.lanes, 2) };
            acc += rig.inner.source.window(lanes, &mut s.rows, &mut s.pos).unwrap() as u64;
        }
        core::hint::black_box(acc);
    });
    let old_next = ns("Site.next, v3 guard+table+dyn+marshal", ITERS, || {
        let mut acc = 0i64;
        for _ in 0..ITERS {
            acc += unsafe { (core::hint::black_box(&OLD).window)(env, 0) };
        }
        core::hint::black_box(acc);
    });
    let new_next = ns("Site.next, v2 pointer call in place", ITERS, || {
        let mut acc = 0i64;
        for _ in 0..ITERS {
            acc += unsafe { site::next(site) };
        }
        core::hint::black_box(acc);
    });
    // Program.push
    let old_push_ns = ns("Program.push empty, v3 guard+dyn", ITERS, || {
        let mut acc = 0i64;
        for _ in 0..ITERS {
            acc += unsafe { (core::hint::black_box(&OLD).push)(env) };
        }
        core::hint::black_box(acc);
    });
    let new_push_ns = ns("Program.push empty, v2 pointer call", ITERS, || {
        let mut acc = 0i64;
        for _ in 0..ITERS {
            acc += unsafe { gravity_abi::program::push(ctx) };
        }
        core::hint::black_box(acc);
    });
    // cancellation
    let old_check_ns = ns("cancel test, v3 host call", ITERS, || {
        let mut acc = 0i64;
        for _ in 0..ITERS {
            acc += unsafe { (core::hint::black_box(&OLD).check)(env) };
        }
        core::hint::black_box(acc);
    });
    unsafe { (*ctx).cancel = rig.byte.as_ptr() };
    let new_check_ns = ns("cancel test, v2 one load", ITERS, || {
        let mut acc = 0u64;
        for _ in 0..ITERS {
            acc += unsafe { cancel_pending(core::hint::black_box(ctx)) } as u64;
        }
        core::hint::black_box(acc);
    });
    // arena
    let old_scratch_ns = ns("arena bump, v3 scratch host call", ITERS, || {
        let mut acc = 0i64;
        for _ in 0..ITERS {
            acc ^= unsafe { (core::hint::black_box(&OLD).scratch)(env, 16) };
        }
        core::hint::black_box(acc);
    });
    let new_bump_ns = ns("arena bump, v2 cur += n", ITERS, || {
        let mut acc = 0usize;
        for i in 0..ITERS {
            if i % (1 << 16) == 0 {
                unsafe { (*ctx).arena_cur = rig.arena.as_mut_ptr() as *mut u8 };
            }
            acc ^= unsafe { arena_bump(core::hint::black_box(ctx), 8) } as usize;
        }
        core::hint::black_box(acc);
    });
    // the cold call's cost to the surrounding loop: a bump loop that grows every 4096 bumps
    // against the same loop with the window rewound in place
    let warm_loop = ns("bump loop, no cold call", ITERS, || {
        let mut acc = 0usize;
        let (mut a, mut b, mut c, mut d) = (1usize, 2usize, 3usize, 4usize);
        for i in 0..ITERS {
            if i & 4095 == 0 {
                unsafe { (*ctx).arena_cur = rig.arena.as_mut_ptr() as *mut u8 };
            }
            let p = unsafe { arena_bump(ctx, 8) } as usize;
            a = a.wrapping_mul(31).wrapping_add(p);
            b ^= a.rotate_left(7);
            c = c.wrapping_add(b >> 3);
            d = d.wrapping_mul(17) ^ c;
            acc ^= d;
        }
        core::hint::black_box((acc, a, b, c, d));
    });
    let cold_loop = ns("bump loop, cold arena_grow every 4096", ITERS, || {
        let mut acc = 0usize;
        let (mut a, mut b, mut c, mut d) = (1usize, 2usize, 3usize, 4usize);
        for i in 0..ITERS {
            if i & 4095 == 0 {
                unsafe { gravity_abi::program::arena_grow(ctx, 8) };
            }
            let p = unsafe { arena_bump(ctx, 8) } as usize;
            a = a.wrapping_mul(31).wrapping_add(p);
            b ^= a.rotate_left(7);
            c = c.wrapping_add(b >> 3);
            d = d.wrapping_mul(17) ^ c;
            acc ^= d;
        }
        core::hint::black_box((acc, a, b, c, d));
    });
    let end = unsafe { (*ctx).arena_end } as usize;
    let base = end - (1 << 20);
    let warm_reg = ns("bump loop, cursor in a register, rewound in place", ITERS, || {
        let mut acc = 0usize;
        let (mut a, mut b, mut c, mut d) = (1usize, 2usize, 3usize, 4usize);
        let mut cur = base;
        for _ in 0..ITERS {
            if cur + 8 > end {
                cur = core::hint::black_box(base);
            }
            let p = cur;
            cur += 8;
            a = a.wrapping_mul(31).wrapping_add(p);
            b ^= a.rotate_left(7);
            c = c.wrapping_add(b >> 3);
            d = d.wrapping_mul(17) ^ c;
            acc ^= d;
        }
        core::hint::black_box((acc, a, b, c, d, cur));
    });
    let cold_reg = ns("bump loop, cursor in a register, cold arena_grow at exhaustion", ITERS, || {
        let mut acc = 0usize;
        let (mut a, mut b, mut c, mut d) = (1usize, 2usize, 3usize, 4usize);
        let mut cur = base;
        for _ in 0..ITERS {
            if cur + 8 > end {
                unsafe { gravity_abi::program::arena_grow(ctx, 8) };
                cur = unsafe { (*ctx).arena_cur } as usize;
            }
            let p = cur;
            cur += 8;
            a = a.wrapping_mul(31).wrapping_add(p);
            b ^= a.rotate_left(7);
            c = c.wrapping_add(b >> 3);
            d = d.wrapping_mul(17) ^ c;
            acc ^= d;
        }
        core::hint::black_box((acc, a, b, c, d, cur));
    });
    println!("hostcall cold call, register cursor: loop cost {:.3} ns/iter (one call per 131072 bumps)", cold_reg - warm_reg);
    println!("hostcall table (ns): next v3 {old_next:.2} v2 {new_next:.2} (source alone {src_only:.2}); push v3 {old_push_ns:.2} v2 {new_push_ns:.2}; cancel v3 {old_check_ns:.2} v2 {new_check_ns:.2}; arena v3 {old_scratch_ns:.2} v2 {new_bump_ns:.2}; cold-call loop cost {:.3} ns/iter", cold_loop - warm_loop);
    let _ = (&rig.lanes, &rig.out, &rig.prog);
    assert!(new_next - src_only <= 8.0 || cfg!(debug_assertions), "Site.next over the source's own work: {:.2} ns > 8", new_next - src_only);
    assert!(new_push_ns <= 5.0 || cfg!(debug_assertions), "Program.push empty: {new_push_ns:.2} ns > 5");
}
