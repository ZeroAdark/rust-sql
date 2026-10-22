//! D642 (main 09-26, from the v3 report's "a cap of 64 grow chunks per call"): `arena_grow` refused the 65th chunk of a
//! Call (`Inner.chunk_cap` = `MAX_CHUNKS`) whatever the statement's grant had left, so a served set-returning step (or any
//! kernel) whose rows grow the arena past 64 chunks would fail where PostgreSQL returns them. The refusal is now the
//! chunk's bytes (`chunk_region`: W, else G, else the past-G road), and a statement's end parks only `PARK_CHUNKS` of its
//! growth (the user's free-memory preference: the rest return their pages and drop). Through a real driver ctx (d638's
//! rig): 100 grows of 64 KB in one Call all succeed, are writable and are held in the grant pool; a grow past the pool's
//! capacity is refused; 100 grows of 1 MiB leave at most 64 MiB of them parked after the statement; a statement's pins
//! (never under a chunk count: a `min(<text>)` aggregate's images) stay parked as before, 100 pins of 1 MiB beside 100 grows park
//! the 100 pins and 64 of the grows.
use gravity_lowerv4_it as common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use gravity_abi::Ctx;
use gravity_driver::{ArenaBox, Binding, BindingParts, CommandId, Entry, Grant, Identity, Locker, PerTuple, Pins, Reservation, Snapshot, SourceBox, Writer, REGIONS};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntBinOp, Type, Value};
use gravity_lower::{assemble, compile, Backend, Sequential};
use gravity_lowerv4::face::end::EndMech;
use gravity_lowerv4::face::source::SourceMech;
use gravity_lowerv4::frame::env::Env;
use gravity_lowerv4::frame::frame::{Mechs, Win};
use gravity_lowerv4::frame::program;
use gravity_lowerv4::frame::px::Px;
use gravity_lowerv4::frame::row::{Carry, RowVals};
use gravity_lowerv4::frame::stop::Exits;
use gravity_lowerv4::graph::Gx;
use gravity_program::{Arena, SiteDesc, WritePlan};

struct OneRow;

impl SourceMech for OneRow {
    fn once(&self) -> bool {
        true
    }
    fn open(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Value {
        b.iconst(Type::I64, 0)
    }
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _l_eof: Block) -> Win {
        Win { cursor: env.zero, end: b.iconst(Type::I64, 1), facts: Default::default() }
    }
    fn row<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        RowVals { row: gravity_lowerv4::cell::Row::new_in(env.mcx), vals: mcx::PgVec::new_in(env.mcx), cols: mcx::PgVec::new_in(env.mcx), carry, next, at: j }
    }
}

struct Count;

impl EndMech for Count {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        Carry { out: b.int_bin(IntBinOp::Add, row.carry.out, env.one), accs: mcx::to_vec_in(&row.carry.accs[..], env.mcx) }
    }
}

struct Derive;

impl gravity_driver::Derive for Derive {
    fn size_hint(&self, _: &SiteDesc) -> usize {
        0
    }
    fn derive_source(&mut self, _d: &SiteDesc, _a: gravity_program::ArenaRef) -> SourceBox {
        panic!("no source site")
    }
    fn writer_size_hint(&self) -> usize {
        0
    }
    fn derive_writer(&mut self, _plan: &WritePlan, _a: gravity_program::ArenaRef) -> ArenaBox<dyn Writer> {
        panic!("no writer")
    }
    fn locker_size_hint(&self) -> usize {
        0
    }
    fn derive_locker(&mut self, _plan: &WritePlan, _a: gravity_program::ArenaRef) -> ArenaBox<dyn Locker> {
        panic!("no locker")
    }
}

struct NoRows;

impl gravity_driver::Sink for NoRows {
    fn push(&mut self, _b: gravity_driver::BorrowedBatch<'_>) -> gravity_driver::Result<gravity_driver::Flow> {
        Ok(gravity_driver::Flow::Continue)
    }
}


const KB: u64 = 1 << 10;
const MB: u64 = 1 << 20;

/// `f` over the bound ctx of one statement under an unadmitted grant of `grant` bytes (one Call's arena window, as the
/// program's steps would see it); the statement ends (its settle) before this returns.
fn statement<T>(grant: u64, f: impl FnOnce(*mut Ctx) -> T) -> T {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    common::RigCatalog::install_seams();
    // the server's grant pool (postmaster arms it from RAM / 4); the rig never arms it
    static POOL: std::sync::Once = std::sync::Once::new();
    POOL.call_once(|| gravity_driver::POOL.arm(2 << 30));
    let arena = Arena::new(1 << 20);
    let a = arena.handle();
    let mut g = gravity_decompose::PipelineGraph::new(a);
    g.pipelines.push(gravity_decompose::PipelineNode::new(a, 0, gravity_decompose::Source::Const, gravity_program::Pipeline::serial(-1, gravity_program::End::Sink, gravity_program::Reason::NoDrivingSite)));
    let gx = Gx { graph: &g, facts: &gravity_decompose::NoFacts, catalog: common::RigCatalog::get(), over: None, mcx: rig.mcx() };
    let mut words = Sequential::new(a);
    let mut px = Px::new(a, &gx, &mut words);
    let m = Mechs::new(&mut px, 0, mcx::pg_box_dyn!(rig.mcx(), OneRow => dyn SourceMech), mcx::PgVec::new_in(rig.mcx()), mcx::pg_box_dyn!(rig.mcx(), Count => dyn EndMech));
    let l = program::emit(&mut px, vec![m]);
    drop(px);
    let codes = compile(&l, Backend::Interp).unwrap_or_else(|e| panic!("backend: {}", e.message()));
    let program = Arc::new(assemble(&l, codes));
    let flag = AtomicU8::new(0);
    let mut sink = NoRows;
    let parts = BindingParts { snapshot: Snapshot(0), command_id: CommandId(0), params: &[0], param_nulls: &[0], abort_flag: &flag, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity { proc_number: 0, resource_owner: 0, latch: 0 }, resolved: None };
    let mut b = Binding::new(parts, &mut sink);
    let mut entry = Entry::new(1);
    let lease = std::rc::Rc::new(Reservation::unadmitted(Grant::of(grant)));
    let exec = entry.start(None, &program, &mut Derive, &mut b, lease).unwrap_or_else(|e| panic!("start: {e:?}"));
    let r = f(exec.node().ctx());
    entry.end(exec, true);
    r
}

/// `n` grows of `bytes` on `ctx`, each window written at both ends: the count that succeeded before the first refusal.
unsafe fn grows(ctx: *mut Ctx, n: usize, bytes: u64) -> usize {
    for i in 0..n {
        if gravity_abi::program::arena_grow(ctx, bytes) <= 0 {
            return i;
        }
        let (lo, hi) = ((*ctx).arena_cur, (*ctx).arena_end);
        assert!(hi as usize - lo as usize >= bytes as usize, "grow {i}: a window of {} B for {bytes} B", hi as usize - lo as usize);
        lo.write(i as u8);
        hi.sub(1).write(i as u8);
    }
    n
}

/// One test (the depot is process-wide): the three cases in order.
#[test]
fn growth_is_bounded_by_bytes_and_returned_at_the_statement_end() {
    // past the old 64-chunk cap in one Call: every grow a writable window, its bytes held in the grant pool
    let (got, held) = statement(1 << 30, |ctx| unsafe {
        let before = gravity_driver::POOL.held();
        let got = grows(ctx, 100, 64 * KB);
        (got, gravity_driver::POOL.held().saturating_sub(before))
    });
    assert_eq!(got, 100, "100 grows of 64 KB under a 1 GiB grant");
    assert!(held >= 100 * 64 * KB, "the 100 chunks held {held} B of the pool, not their 6.25 MiB");
    // the refusal is by bytes: one grow past the pool's capacity (2 GiB here) is refused, at its first chunk
    let got = statement(1 << 30, |ctx| unsafe { grows(ctx, 1, 3 << 30) });
    assert_eq!(got, 0, "a 3 GiB grow past a 2 GiB pool");
    // the statement's end parks at most PARK_CHUNKS of its growth: 100 MiB grown, at most 64 MiB (+ its slots) parked
    REGIONS.clear();
    let before = REGIONS.stats().bytes;
    let got = statement(1 << 30, |ctx| unsafe { grows(ctx, 100, MB) });
    assert_eq!(got, 100, "100 grows of 1 MiB under a 1 GiB grant");
    let parked = REGIONS.stats().bytes.saturating_sub(before) as u64;
    assert!(parked < 80 * MB, "{} MiB of the statement's 100 MiB growth still parked", parked / MB);
    // G4, what the return leaves alone: the pins (the old cap never counted them) park whole, the grows past the pins
    // park PARK_CHUNKS, so a rerun re-takes its pins from the depot instead of faulting them in again
    REGIONS.clear();
    let before = REGIONS.stats().bytes;
    let (pins, got) = statement(1 << 30, |ctx| unsafe {
        let pins = (0..100).filter(|_| !gravity_abi::program::pin(ctx, MB).is_null()).count();
        (pins, grows(ctx, 100, MB))
    });
    assert_eq!((pins, got), (100, 100), "100 pins and 100 grows of 1 MiB under a 1 GiB grant");
    let parked = REGIONS.stats().bytes.saturating_sub(before) as u64;
    assert!((164 * MB..180 * MB).contains(&parked), "{} MiB parked, not the 100 MiB of pins + 64 MiB of grows", parked / MB);
}
