//! The host-call microbenchmark at driver level (docs/gravity/abi-v2.md §6): the real call
//! targets of this tree over a resident fixture source, fixed iterations, ns per call.
use gravity_driver_rig as common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;
use std::time::Instant;

use common::*;
use gravity_abi::ctx::{arena_bump, cancel_pending, lane, site_ptr};
use gravity_abi::status::{DONE, ERROR};
use gravity_abi::{Ctx, Datum, Lane, ScanPos};
use gravity_driver::{box_source, ArenaBox, ArenaRef, BindingRef, BorrowedBatch, Cursor, Derive, Entry, Locker, Result, Sink, Source, SourceBox, Window, WindowBase, Writer};
use gravity_program::{Code, Layout, OutCol, Program, SiteDesc, WritePlan};

const ROWS: usize = 8192;
const ITERS: u64 = 1 << 20;

static COL0: [Datum; ROWS] = [7; ROWS];
static COL1: [Datum; ROWS] = [9; ROWS];

/// A resident part whose window is already decoded: every `window()` republishes the same two lanes.
struct Resident {
    cur: Cursor,
}

impl Source for Resident {
    fn bind(&mut self, _b: &BindingRef<'_>) -> Result<()> {
        Ok(())
    }

    fn seek(&mut self, _keys: &[Datum], _keys_null: &[u8]) -> Result<()> {
        Ok(())
    }

    fn window(&mut self, w: &mut Window<'_>) -> Result<u32> {
        w.set(0, Lane::byval(COL0.as_ptr() as *const u8, core::ptr::null(), 8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, 1)));
        w.set(1, Lane::byval(COL1.as_ptr() as *const u8, core::ptr::null(), 8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, 2)));
        w.rows = ROWS as u32;
        w.set_base(WindowBase::at(ScanPos::new(77, 1, 0)));
        Ok(ROWS as u32)
    }

    fn lanes_hint(&self) -> usize {
        2
    }

    fn cursor(&self) -> &Cursor {
        &self.cur
    }

    fn cursor_mut(&mut self) -> &mut Cursor {
        &mut self.cur
    }

    fn refetch(&mut self, _values: &mut [Datum], _isnull: &mut [u8]) -> Result<bool> {
        Ok(false)
    }

    fn substitute(&mut self, _pos: ScanPos) {}

    fn unbind(&mut self) {}
}

struct ResidentDerive;

impl Derive for ResidentDerive {
    fn size_hint(&self, _desc: &SiteDesc) -> usize {
        core::mem::size_of::<Resident>()
    }

    fn derive_source(&mut self, _desc: &SiteDesc, a: ArenaRef) -> SourceBox {
        box_source(Resident { cur: Cursor::new() }, a)
    }

    fn writer_size_hint(&self) -> usize {
        0
    }

    fn derive_writer(&mut self, _plan: &WritePlan, _a: ArenaRef) -> ArenaBox<dyn Writer> {
        unreachable!()
    }

    fn locker_size_hint(&self) -> usize {
        0
    }

    fn derive_locker(&mut self, _plan: &WritePlan, _a: ArenaRef) -> ArenaBox<dyn Locker> {
        unreachable!()
    }
}

unsafe extern "C-unwind" fn k_next(ctx: *mut Ctx) -> i64 {
    let n = *(*ctx).params;
    let s = site_ptr(ctx, 0);
    gravity_abi::site::seek(s);
    let mut acc = 0i64;
    for _ in 0..n {
        let r = gravity_abi::site::next(s);
        if r <= 0 {
            return ERROR;
        }
        acc += r + (*s).rows as i64;
    }
    core::hint::black_box(acc);
    DONE
}

unsafe extern "C-unwind" fn k_push(ctx: *mut Ctx) -> i64 {
    let n = *(*ctx).params;
    for _ in 0..n {
        (*(*ctx).out).count = 0;
        if gravity_abi::program::push(ctx) < 0 {
            return ERROR;
        }
    }
    DONE
}

unsafe extern "C-unwind" fn k_cancel(ctx: *mut Ctx) -> i64 {
    let n = *(*ctx).params;
    let mut hits = 0u64;
    for _ in 0..n {
        hits += cancel_pending(core::hint::black_box(ctx)) as u64;
    }
    core::hint::black_box(hits);
    DONE
}

unsafe extern "C-unwind" fn k_bump(ctx: *mut Ctx) -> i64 {
    let n = *(*ctx).params;
    let mut acc = 0usize;
    let base = (*ctx).arena_cur;
    for i in 0..n {
        if i % 4096 == 0 {
            (*ctx).arena_cur = base;
        }
        acc ^= arena_bump(core::hint::black_box(ctx), 8) as usize;
    }
    core::hint::black_box(acc);
    DONE
}

fn program(k: unsafe extern "C-unwind" fn(*mut Ctx) -> i64) -> Program {
    let st = gravity_program::Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), 77, &[1, 2])];
    let out = [OutCol::new("c0", 20, -1, 8, true)];
    Program::new(Layout { nsites: 1, out_natts: 1, out_rows: 64, scratch_bytes: 65536, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, Code::hand(k))
}

fn time(name: &str, k: unsafe extern "C-unwind" fn(*mut Ctx) -> i64) -> f64 {
    let program = Arc::new(program(k));
    let mut derive = ResidentDerive;
    let mut entry = Entry::new(1);
    let params = [ITERS];
    let nulls = [0u8];
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sref = &sink;
    let mut b = binding(&params, &nulls, &mut sref, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    entry.run_to_end(&mut exec, &mut b).unwrap();
    let mut exec2 = entry.start(None, &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    let t = Instant::now();
    entry.run_to_end(&mut exec2, &mut b).unwrap();
    let ns = t.elapsed().as_nanos() as f64 / ITERS as f64;
    entry.end(exec2, true);
    entry.end(exec, true);
    println!("hostcall {name}: {ns:.2} ns/call ({ITERS} calls)");
    ns
}

/// The sink's own work on an empty block: `FakeSink::push` called directly, the same iterations.
fn sink_alone() -> f64 {
    let sink = FakeSink::new();
    let mut sref = &sink;
    let t = Instant::now();
    for _ in 0..ITERS {
        let empty = BorrowedBatch { natts: 1, rows: 0, values: &[], isnull: &[], region: &[] };
        Sink::push(core::hint::black_box(&mut sref), empty).unwrap();
    }
    let ns = t.elapsed().as_nanos() as f64 / ITERS as f64;
    println!("hostcall sink alone (FakeSink::push, empty batch): {ns:.2} ns/call");
    ns
}

#[test]
fn hostcall_table() {
    time("Site.next (resident part, decoded window)", k_next);
    let push = time("Program.push (empty block)", k_push);
    let alone = sink_alone();
    println!("hostcall Program.push over the sink's own work: {:.2} ns", push - alone);
    time("cancel test (one load)", k_cancel);
    time("arena bump (cur += n)", k_bump);
}
