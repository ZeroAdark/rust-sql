//! D314, the worker side: a source that raises by unwinding in a pool worker's claim, under an `extern "C"` frame
//! between the program and the host (the JIT'd code's). The driver's trampoline parks the unwind, `Code::call` resumes
//! it on the worker once the program returns, and the pool's claim catch turns a PostgreSQL ERROR into the claim's
//! error with its SQLSTATE, as a PostgreSQL worker's error reaches the leader; a bug's panic, or a PgError above ERROR,
//! stays "worker panicked"; D2933: a bug's panic is C's abort() on the worker (it dies on PanicExitThread, the crash
//! restart in a server). A raise on the leader (W1) unwinds to the caller with its payload. Before D314 the unwind
//! met the `extern "C"` frame and Rust aborted the process.
#[path = "../../hosts/tests/common/mod.rs"]
mod common;

use std::cell::Cell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind};
use gravity_abi::ctx::lane;
use gravity_abi::{Ctx, Datum, Lane, ScanPos};
use gravity_driver::{box_source, ArenaBox, ArenaRef, Binding, BindingParts, BindingRef, BorrowedBatch, CommandId, Cursor, Derive, Entry, Error, Flow, Identity, LegRef, Locker, Merge, PerTuple, Pins, Result, Sink, SinkLeg, Snapshot, Source, SourceBox, Window, WindowBase, Writer};
use gravity_kernels::par_scan_count;
use gravity_parallel::{run_program, BlockRange, ClaimSize, MorselSource, Plan, PlainAggPartial};
use gravity_pool::{Ambient, RuntimePool};
use gravity_program::{Arena, Code, End, Layout, OutCol, Pipeline, Program, Reason, SiteDesc, WritePlan};
use runtime::{Runtime, RuntimeConfig, WorkerPool};
use types_error::{PgError, ERRCODE_DIVISION_BY_ZERO, ERROR, FATAL};

const REL: u32 = 91314;
const BLOCKS: u64 = 64;
const ROWS: usize = 128;
/// The block whose window raises.
const RAISE_BLOCK: u64 = 37;
static COL: [Datum; ROWS] = [1; ROWS];
static ABORT: AtomicU8 = AtomicU8::new(0);

/// What the window of `RAISE_BLOCK` does.
static MODE: AtomicU8 = AtomicU8::new(QUIET);
const QUIET: u8 = 0;
const RAISE_BOXED: u8 = 1;
const RAISE_PLAIN: u8 = 2;
const RAISE_FATAL: u8 = 3;
const BUG: u8 = 4;
/// Whether the raise ran on a pool worker (not the leader's thread).
static RAISED_ON_WORKER: AtomicBool = AtomicBool::new(false);
/// D2933: how each pool thread that died ended (true = on PanicExitThread, C's abort()).
static DEATHS: Mutex<Vec<bool>> = Mutex::new(Vec::new());

fn deaths() -> Vec<bool> {
    DEATHS.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// `WorkerPool::spawn_std`, each thread's death recorded: what reaches the top of a pool thread.
fn spawn_recording(rt: &Arc<Runtime>) -> WorkerPool {
    WorkerPool::spawn_with(Arc::clone(rt), |ordinal, body| {
        std::thread::Builder::new().name(format!("d314-rtworker-{ordinal}")).spawn(move || {
            if let Err(payload) = catch_unwind(AssertUnwindSafe(body)) {
                DEATHS.lock().unwrap_or_else(|p| p.into_inner()).push(payload.is::<types_error::PanicExitThread>());
            }
        })
    })
    .expect("pool threads")
}

thread_local! {
    static LEADER: Cell<bool> = const { Cell::new(false) };
}

fn raise() {
    RAISED_ON_WORKER.store(!LEADER.with(|l| l.get()), Ordering::Relaxed);
    // the destinations errstart sets on a pool thread, which has no client
    let err = || {
        let mut e = PgError::error("division by zero").with_sqlstate(ERRCODE_DIVISION_BY_ZERO);
        e.output_to_server = Some(true);
        e.output_to_client = Some(false);
        e
    };
    match MODE.load(Ordering::Relaxed) {
        RAISE_BOXED => std::panic::panic_any(Box::new(err())),
        RAISE_PLAIN => std::panic::panic_any(err()),
        RAISE_FATAL => std::panic::panic_any(Box::new(PgError::new(FATAL, "terminating connection (d314 fixture)".to_string()))),
        BUG => panic!("d314 fixture: a host's bug"),
        _ => {}
    }
}

/// One window of `ROWS` rows per block of the claim `[lo, hi)`.
struct Blocks {
    cur: Cursor,
    g: u64,
    hi: u64,
}

impl Source for Blocks {
    fn bind(&mut self, _b: &BindingRef<'_>) -> Result<()> {
        Ok(())
    }

    fn seek(&mut self, keys: &[Datum], _keys_null: &[u8]) -> Result<()> {
        (self.g, self.hi) = (keys[0], keys[1]);
        Ok(())
    }

    fn window(&mut self, w: &mut Window<'_>) -> Result<u32> {
        if self.g >= self.hi {
            return Ok(0);
        }
        if self.g == RAISE_BLOCK {
            raise();
        }
        w.set(0, Lane::byval(COL.as_ptr() as *const u8, core::ptr::null(), 8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, 1)));
        w.rows = ROWS as u32;
        w.set_base(WindowBase::at(ScanPos::new(REL, self.g as u32, 0)));
        self.g += 1;
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

struct BlocksDerive;

impl Derive for BlocksDerive {
    fn size_hint(&self, _desc: &SiteDesc) -> usize {
        core::mem::size_of::<Blocks>()
    }

    fn derive_source(&mut self, _desc: &SiteDesc, a: ArenaRef) -> SourceBox {
        box_source(Blocks { cur: Cursor::new(), g: 0, hi: 0 }, a)
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

struct CountSink {
    rows: Mutex<Vec<Vec<Datum>>>,
    pushes: AtomicU64,
}

impl CountSink {
    fn take(&self, b: BorrowedBatch<'_>) -> Result<Flow> {
        self.pushes.fetch_add(1, Ordering::Relaxed);
        let mut rows = self.rows.lock().unwrap();
        for (v, _) in b.iter() {
            rows.push(v.to_vec());
        }
        Ok(Flow::Continue)
    }
}

impl Sink for CountSink {
    fn push(&mut self, b: BorrowedBatch<'_>) -> Result<Flow> {
        self.take(b)
    }
    fn leg(&self, _ordinal: u16) -> Option<LegRef<'_>> {
        Some(LegRef::of(self))
    }
}

impl SinkLeg for CountSink {
    fn push(&self, b: BorrowedBatch<'_>) -> Result<Flow> {
        self.take(b)
    }
}

struct RangePlan {
    width: u16,
    blocks: [BlockRange; 2],
}

impl Plan for RangePlan {
    fn width_of(&self, _p: usize) -> u16 {
        self.width
    }
    fn claims_for(&self, p: usize) -> &dyn MorselSource {
        &self.blocks[p]
    }
    fn merge_for(&self, _p: usize) -> &dyn Merge {
        &PlainAggPartial
    }
}

/// The JIT'd code's frame: an `extern "C"` function between the program and the host.
extern "C" fn count_c(ctx: *mut Ctx) -> i64 {
    unsafe { par_scan_count::par_scan_count(ctx) }
}

unsafe extern "C-unwind" fn k_count(ctx: *mut Ctx) -> i64 {
    count_c(ctx)
}

/// `hand::par_scan_count_with` with its claim pipeline behind `count_c`.
fn program() -> Program {
    let st = Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), REL, &[1])];
    let out = [OutCol::new("count", 20, -1, 8, true)];
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, par_scan_count::BYTES)), BLOCKS * ROWS as u64), Code::hand(k_count)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(par_scan_count::par_scan_count_emit)),
    ];
    Program::with_pipelines(Layout { nsites: 1, out_natts: 1, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, pipes)
}

fn run(entry: &mut Entry, program: &Arc<Program>, pool: &RuntimePool, width: u16) -> std::thread::Result<Result<Vec<Vec<Datum>>>> {
    let params = [2u64];
    let nulls = [0u8];
    let mut sink = CountSink { rows: Mutex::new(Vec::new()), pushes: AtomicU64::new(0) };
    let r = catch_unwind(AssertUnwindSafe(|| {
        let parts = BindingParts { snapshot: Snapshot(0), command_id: CommandId(0), params: &params, param_nulls: &nulls, abort_flag: &ABORT, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: None };
        let mut b = Binding::new(parts, &mut sink);
        let mut exec = entry.start(None, program, &mut BlocksDerive, &mut b, std::rc::Rc::new(gravity_driver::Reservation::unadmitted(gravity_driver::Grant::of(8 << 20))))?;
        let plan = RangePlan { width, blocks: [BlockRange::new(BLOCKS, ClaimSize::Fixed(1)), BlockRange::new(0, ClaimSize::Fixed(1))] };
        let r = run_program(entry, &mut exec, &mut b, &mut BlocksDerive, pool, &plan);
        entry.end(exec, r.is_ok());
        r.map(|_| ())
    }));
    r.map(|r| r.map(|()| sink.rows.into_inner().unwrap()))
}

#[test]
fn a_workers_unwinding_raise_reaches_the_leader_with_its_sqlstate() {
    let _g = serial();
    install();
    LEADER.with(|l| l.set(true));
    let rt = Runtime::new(RuntimeConfig::new(8));
    let threads = spawn_recording(&rt);
    let pool = RuntimePool::new(Arc::clone(&rt), Ambient::HARNESS);
    let program = Arc::new(program());
    let mut entry = Entry::new(2);
    let all = vec![vec![BLOCKS * ROWS as u64]];
    for width in [4u16, 8] {
        MODE.store(QUIET, Ordering::Relaxed);
        assert_eq!(run(&mut entry, &program, &pool, width).expect("no unwind"), Ok(all.clone()), "W{width}");
        // an ERROR raised by unwinding on a worker, boxed or not: the statement's error, its SQLSTATE and message kept
        for mode in [RAISE_BOXED, RAISE_PLAIN] {
            MODE.store(mode, Ordering::Relaxed);
            RAISED_ON_WORKER.store(false, Ordering::Relaxed);
            let r = run(&mut entry, &program, &pool, width).expect("a worker's raise never unwinds the leader");
            assert!(RAISED_ON_WORKER.load(Ordering::Relaxed), "W{width} mode {mode}: the raising claim ran on a pool worker");
            assert_eq!(r, Err(Error::host("pg")), "W{width} mode {mode}");
            let pg = gravity_hosts::take_error().expect("the worker's error, restashed on the leader");
            assert_eq!((pg.message(), pg.sqlstate()), ("division by zero", ERRCODE_DIVISION_BY_ZERO), "W{width} mode {mode}");
            assert_eq!(pg.level(), ERROR);
            // the leader's report: its emit decides the destinations (the worker's "no client" never reaches it)
            assert_eq!((pg.output_to_server, pg.output_to_client), (None, None), "W{width} mode {mode}");
        }
        // a bug's panic and a PgError above ERROR are not a typed ERROR: the claim's host error, as before D314
        for mode in [BUG, RAISE_FATAL] {
            MODE.store(mode, Ordering::Relaxed);
            let r = run(&mut entry, &program, &pool, width);
            if mode == BUG {
                // D2933: a bug's panic is C's abort(): the section completes
                // (failed), then the leader PANICs (PanicExitThread: the crash restart in a server, whose fence retires
                // every pool thread)
                let payload = r.expect_err("a claim's bug panic PANICs the leader");
                assert!(payload.is::<types_error::PanicExitThread>(), "W{width}: the leader's PANIC, not XX000");
            } else {
                let r = r.expect("a worker's FATAL payload never unwinds the leader");
                assert_eq!(r, Err(Error::host("gravity: worker panicked in a claim")), "W{width} mode {mode}");
            }
            let _ = gravity_hosts::take_error();
        }
        // the pool and the entry serve again
        MODE.store(QUIET, Ordering::Relaxed);
        assert_eq!(run(&mut entry, &program, &pool, width).expect("no unwind"), Ok(all.clone()), "W{width}: served after the raises");
    }
    // W1: the claims run on the leader, and its raise unwinds to the caller with the payload (the statement's catch)
    MODE.store(RAISE_BOXED, Ordering::Relaxed);
    let payload = run(&mut entry, &program, &pool, 1).expect_err("the leader's raise unwinds to its caller");
    let pg = payload.downcast_ref::<Box<PgError>>().expect("the PgError payload, intact");
    assert_eq!(pg.sqlstate(), ERRCODE_DIVISION_BY_ZERO);
    MODE.store(QUIET, Ordering::Relaxed);
    assert_eq!(run(&mut entry, &program, &pool, 1).expect("no unwind"), Ok(all), "W1: served after the raise");
    quiesced();
    threads.shutdown();
    // D2933: no pool thread died: the claim's panic settled its morsel and the leader took the crash path
    assert_eq!(deaths(), Vec::<bool>::new(), "pool thread deaths");
}
