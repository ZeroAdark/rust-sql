//! D314: an unwind out of a host (a source's window, a sink's push) never crosses the `extern "C"` frames between the
//! program and the host. The driver's trampoline parks it and fails the call, and `Code::call` resumes it once the
//! program returns: the caller gets the payload intact, and the Entry serves the next execution. Before D314 the unwind
//! met the `extern "C"` frame (here `advance_c` / `push_c`, the stand-ins of the JIT'd code and of kernels'
//! `host_abi::advance` / `push`) and Rust aborted the process: the whole server, a backend being a thread.
//! The worker side (a claim's raise reaching the leader with its SQLSTATE) is `gravity_pool`'s `tests/d314_worker_raise.rs`.
use gravity_driver_rig as common;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::ctx::{lane, site_ptr};
use gravity_abi::status::DONE;
use gravity_abi::{Ctx, Datum, Lane, ScanPos};
use gravity_driver::{box_source, ArenaBox, ArenaRef, BindingRef, BorrowedBatch, Cursor, Derive, Entry, Flow, Locker, Result, Sink, Source, SourceBox, Window, WindowBase, Writer};
use gravity_program::{Code, Layout, OutCol, Program, SiteDesc, WritePlan};

const ROWS: usize = 64;
const WINDOWS: u32 = 4;
static COL: [Datum; ROWS] = [7; ROWS];

/// What the host at `AT` does on its next call: nothing, raise an ERROR-shaped payload, or panic as a bug.
static MODE: AtomicU8 = AtomicU8::new(QUIET);
static AT: AtomicU8 = AtomicU8::new(IN_SOURCE);
const QUIET: u8 = 0;
const RAISE: u8 = 1;
const BUG: u8 = 2;
const IN_SOURCE: u8 = 0;
const IN_SINK: u8 = 1;

/// A PostgreSQL ERROR's stand-in: the driver never names `PgError`, a host's `panic_any(PgError)` is one opaque payload.
#[derive(Debug, PartialEq)]
struct Raised(&'static str);

fn raise_at(at: u8) {
    if AT.load(Ordering::Relaxed) != at {
        return;
    }
    match MODE.load(Ordering::Relaxed) {
        RAISE => std::panic::panic_any(Raised("division by zero")),
        BUG => panic!("d314 fixture: a host's bug"),
        _ => {}
    }
}

/// `WINDOWS` windows of `ROWS` rows, the second one raising when asked.
struct Windows {
    cur: Cursor,
    left: u32,
}

impl Source for Windows {
    fn bind(&mut self, _b: &BindingRef<'_>) -> Result<()> {
        Ok(())
    }

    fn seek(&mut self, _keys: &[Datum], _keys_null: &[u8]) -> Result<()> {
        self.left = WINDOWS;
        Ok(())
    }

    fn window(&mut self, w: &mut Window<'_>) -> Result<u32> {
        if self.left == 0 {
            return Ok(0);
        }
        if self.left == WINDOWS - 1 {
            raise_at(IN_SOURCE);
        }
        self.left -= 1;
        w.set(0, Lane::byval(COL.as_ptr() as *const u8, core::ptr::null(), 8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, 1)));
        w.rows = ROWS as u32;
        w.set_base(WindowBase::at(ScanPos::new(77, WINDOWS - self.left, 0)));
        Ok(ROWS as u32)
    }

    fn lanes_hint(&self) -> usize {
        1
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

struct WindowsDerive;

impl Derive for WindowsDerive {
    fn size_hint(&self, _desc: &SiteDesc) -> usize {
        core::mem::size_of::<Windows>()
    }

    fn derive_source(&mut self, _desc: &SiteDesc, a: ArenaRef) -> SourceBox {
        box_source(Windows { cur: Cursor::new(), left: 0 }, a)
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

/// Counts the rows pushed, raising on the second push when asked.
struct Rows {
    rows: u64,
    pushes: u64,
}

impl Sink for Rows {
    fn push(&mut self, batch: BorrowedBatch<'_>) -> Result<Flow> {
        self.pushes += 1;
        if self.pushes == 2 {
            raise_at(IN_SINK);
        }
        for (values, _) in batch.iter() {
            self.rows += values[0];
        }
        Ok(Flow::Continue)
    }
}

/// The JIT'd code's frame: an `extern "C"` function between the program and the host, as kernels' `host_abi::advance`.
extern "C" fn advance_c(ctx: *mut Ctx) -> i64 {
    unsafe { gravity_abi::site::next(site_ptr(ctx, 0)) }
}

/// As `advance_c`, for `Program.push` (kernels' `host_abi::push`).
extern "C" fn push_c(ctx: *mut Ctx) -> i64 {
    unsafe { gravity_abi::program::push(ctx) }
}

/// One output row per window, its row count, pushed at once.
unsafe extern "C-unwind" fn k_windows(ctx: *mut Ctx) -> i64 {
    gravity_abi::site::seek(site_ptr(ctx, 0));
    loop {
        let r = advance_c(ctx);
        if r <= 0 {
            return if r == 0 { DONE } else { r };
        }
        let o = &mut *(*ctx).out;
        *o.values.add(o.count as usize) = r as Datum;
        *o.isnull.add(o.count as usize) = 0;
        o.count += 1;
        let p = push_c(ctx);
        if p < 0 {
            return p;
        }
    }
}

fn program() -> Program {
    let st = gravity_program::Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), 77, &[1])];
    let out = [OutCol::new("n", 20, -1, 8, true)];
    Program::new(Layout { nsites: 1, out_natts: 1, out_rows: 64, scratch_bytes: 65536, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, Code::hand(k_windows))
}

/// One execution to its end: the rows the sink took, or the unwind that left it.
fn execute(entry: &mut Entry, program: &Arc<Program>) -> std::thread::Result<Result<u64>> {
    let params = [0u64];
    let nulls = [0u8];
    let flag = AtomicU8::new(0);
    let mut sink = Rows { rows: 0, pushes: 0 };
    catch_unwind(AssertUnwindSafe(|| {
        let mut b = binding(&params, &nulls, &mut sink, &flag);
        let mut exec = entry.start(None, program, &mut WindowsDerive, &mut b, lease(gravity_driver::Grant::of(8 << 20)))?;
        let r = entry.run_to_end(&mut exec, &mut b);
        entry.end(exec, r.is_ok());
        r?;
        Ok(())
    }))
    .map(|r| r.map(|()| sink.rows))
}

#[test]
fn a_host_unwind_is_parked_past_the_extern_c_frames_and_resumed_to_the_caller() {
    let program = Arc::new(program());
    let mut entry = Entry::new(1);
    let all = (WINDOWS as u64) * ROWS as u64;
    assert_eq!(execute(&mut entry, &program).expect("no unwind").expect("served"), all);
    for at in [IN_SOURCE, IN_SINK] {
        AT.store(at, Ordering::Relaxed);
        // a PostgreSQL ERROR raised by unwinding: the same payload reaches the caller (the statement's catch reads it)
        MODE.store(RAISE, Ordering::Relaxed);
        let payload = execute(&mut entry, &program).expect_err("the raise unwinds to the caller");
        assert_eq!(payload.downcast_ref::<Raised>(), Some(&Raised("division by zero")), "at {at}: the payload goes on intact");
        // a bug's panic: the same, its message intact
        MODE.store(BUG, Ordering::Relaxed);
        let payload = execute(&mut entry, &program).expect_err("the panic unwinds to the caller");
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"d314 fixture: a host's bug"), "at {at}");
        // nothing parked is left behind: the next execution serves, whole
        MODE.store(QUIET, Ordering::Relaxed);
        assert_eq!(execute(&mut entry, &program).expect("no unwind").expect("served"), all, "at {at}: the entry serves again");
    }
}
