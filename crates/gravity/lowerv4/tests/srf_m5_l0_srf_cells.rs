//! M5 L0-c (1) (M5-plan §1.3, §9 Wave 1a row 1a-1): each set-returning entry of a ProjectSet's projection reads its own
//! function's value. The projection compiles every target entry as its own gravity_expr program, and `compile_srf`
//! numbers a call's cell from that program's aggregate count, so every entry's call is cell 0: two functions in one select
//! list (golden srf-003 `SELECT generate_series(1, 3), generate_series(1, 2)`, rows (1,1) (2,2) (3,NULL)) both read the
//! first one's value. The Srf step (srf/project_set.rs, not built) hands its functions' cells on in `Env::srfs`, function
//! `i` at `cells[i]`; step/project.rs gives each entry the cell of the function at its resno. A stub Srf step stands in
//! for the unbuilt kind: one row, function 0 = 11 (valid), function 1 = NULL (exhausted, the third row of srf-003), and
//! the real projection of srf-003's graph over it, run by the driver.
use gravity_lowerv4_it as common;

use std::sync::atomic::AtomicU8;
use std::sync::{Arc, Mutex};

use common::RigCatalog;
use gravity_decompose::Op;
use gravity_driver::{ArenaBox, Locker, SourceBox, Writer};
use gravity_expr::emit::agg::{AggCell, CellValid};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, HostFlags, IntBinOp, Type, Value};
use gravity_lower::{assemble, compile, Backend, Sequential};
use gravity_lowerv4::face::end::EndMech;
use gravity_lowerv4::face::kinds::KindId;
use gravity_lowerv4::face::source::SourceMech;
use gravity_lowerv4::face::step::StepMech;
use gravity_lowerv4::frame::env::{AggRow, Env};
use gravity_lowerv4::frame::frame::{Mechs, Next, StepMaker, Win};
use gravity_lowerv4::frame::program;
use gravity_lowerv4::frame::px::Px;
use gravity_lowerv4::frame::row::{Carry, RowVals};
use gravity_lowerv4::frame::stop::Exits;
use gravity_lowerv4::graph::Gx;
use gravity_program::{Arena, SiteDesc, WritePlan};

/// RUN serializes the runs that read SEEN (a static, not TLS: the session TLS census counts every thread_local!).
static RUN: Mutex<()> = Mutex::new(());
/// What the end saw per row: (entry 1's value, its null, entry 2's value, its null).
static SEEN: Mutex<Vec<(u64, u64, u64, u64)>> = Mutex::new(Vec::new());

fn held<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

extern "C" fn seen(v0: u64, n0: u64, v1: u64, n1: u64) -> u64 {
    held(&SEEN).push((v0, n0, v1, n1));
    0
}

/// One row, no site.
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

/// The Srf step's contract as project_set.rs will keep it: the functions' current values in a record, `Env::srfs`'
/// `cells[i]` = function `i`, set around the steps after it. Function 0 is 11; function 1 is exhausted (NULL, its word 22).
struct StubSrf;

impl StepMech for StubSrf {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let slot = b.declare_stack_slot(24, 8);
        let rec = b.stack_addr(slot);
        let (v0, v1, open1) = (b.iconst(Type::I64, 11), b.iconst(Type::I64, 22), b.iconst(Type::I64, 0));
        b.store(rec, 0, v0);
        b.store(rec, 8, v1);
        b.store(rec, 16, open1);
        env.srfs = Some(AggRow { rec, cells: mcx::pg_vec![in env.mcx; AggCell::word(0, CellValid::Always), AggCell::word(8, CellValid::NonZeroAt(16))] });
        let c = next(b, env, row);
        env.srfs = None;
        c
    }
}

/// Reports the projection's two cells, counts the row.
struct Report;

impl EndMech for Report {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        let h = b.declare_host_once("m5_l0_seen", &[Type::I64; 4], Some(Type::I64), seen as *const () as usize, HostFlags::default());
        let mut args = Vec::new();
        for &(v, null) in row.vals.iter().take(2) {
            let n = null.map_or(env.zero, |n| b.conv(gravity_ir::ConvOp::Zext, Type::I64, n));
            args.extend([v, n]);
        }
        assert_eq!(args.len(), 4, "the projection hands on two cells");
        let _ = b.call_host(h, &args, Some(Type::I64));
        Carry { out: b.int_bin(IntBinOp::Add, row.carry.out, env.one), accs: mcx::to_vec_in(&row.carry.accs[..], env.mcx) }
    }
}

struct Derive;

impl gravity_driver::Derive for Derive {
    fn size_hint(&self, _: &SiteDesc) -> usize {
        0
    }
    fn derive_source(&mut self, _d: &SiteDesc, _a: gravity_program::ArenaRef) -> SourceBox {
        panic!("the test program has no source site")
    }
    fn writer_size_hint(&self) -> usize {
        0
    }
    fn derive_writer(&mut self, _plan: &WritePlan, _a: gravity_program::ArenaRef) -> ArenaBox<dyn Writer> {
        panic!("the test program writes nothing")
    }
    fn locker_size_hint(&self) -> usize {
        0
    }
    fn derive_locker(&mut self, _plan: &WritePlan, _a: gravity_program::ArenaRef) -> ArenaBox<dyn Locker> {
        panic!("the test program locks nothing")
    }
}

struct NoRows;

impl gravity_driver::Sink for NoRows {
    fn push(&mut self, _b: gravity_driver::BorrowedBatch<'_>) -> gravity_driver::Result<gravity_driver::Flow> {
        Ok(gravity_driver::Flow::Continue)
    }
}

/// srf-003's projection (step.project's maker over its decomposed graph) behind the stub, run to its end: what it saw.
fn run(rel: &str) -> Vec<(u64, u64, u64, u64)> {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    let _run = held(&RUN);
    held(&SEEN).clear();
    RigCatalog::install_seams();
    let text = std::fs::read_to_string(common::plans_dir().join(rel)).expect("plan fixture");
    let ctx = mcx::MemoryContext::new("m5 l0 cells");
    let fx = gravity_decompose::corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let arena = Arena::new(1 << 20);
    let a = arena.handle();
    let g = gravity_decompose::decompose_in(a, fx.stmt, &fx.facts, fx.mcx).expect("decomposes");
    let ops = &g.pipelines[0].ops;
    let j = (1..ops.len()).find(|&j| matches!(ops[j - 1], Op::Srf(_)) && matches!(ops[j], Op::Project(_))).expect("a projection right after the Srf op");
    let gx = Gx { graph: &g, facts: &fx.facts, catalog: RigCatalog::get(), over: None, mcx: rig.mcx() };
    let mut words = Sequential::new(a);
    let mut px = Px::new(a, &gx, &mut words);
    gravity_expr::nodes::func::acl_defer_begin();
    let maker = gravity_lowerv4::ops::kind_as::<StepMaker>(KindId::StepProject).expect("step.project registers its maker");
    let project = (maker.0)(&mut px, 0, j);
    let m = Mechs::new(&mut px, 0, mcx::pg_box_dyn!(rig.mcx(), OneRow => dyn SourceMech), mcx::pg_vec![in rig.mcx(); mcx::pg_box_dyn!(rig.mcx(), StubSrf => dyn StepMech), project], mcx::pg_box_dyn!(rig.mcx(), Report => dyn EndMech));
    let l = program::emit(&mut px, vec![m]);
    gravity_expr::nodes::func::acl_defer_end();
    drop(px);
    gravity_ir::verify::verify(&l.ir).expect("the IR verifies");
    let codes = compile(&l, Backend::Interp).unwrap_or_else(|e| panic!("backend: {}", e.message()));
    let program = Arc::new(assemble(&l, codes));
    let n = words.words.len().max(1);
    let mut params = vec![0u64; n];
    for (i, w) in words.words.iter().enumerate() {
        if let gravity_lower::WordSrc::Const { value, .. } = *w {
            params[i] = value.value.as_usize() as u64;
        }
    }
    let nulls = vec![0u8; n];
    let flag = AtomicU8::new(0);
    let mut sink = NoRows;
    use gravity_driver::{Binding, BindingParts, Call, CommandId, Entry, Grant, Identity, PerTuple, Pins, Reservation, Snapshot, Step};
    let parts = BindingParts { snapshot: Snapshot(0), command_id: CommandId(0), params: &params, param_nulls: &nulls, abort_flag: &flag, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity { proc_number: 0, resource_owner: 0, latch: 0 }, resolved: None };
    let mut b = Binding::new(parts, &mut sink);
    let mut entry = Entry::new(1);
    let lease = std::rc::Rc::new(Reservation::unadmitted(Grant::of(64 << 20)));
    let mut exec = entry.start(None, &program, &mut Derive, &mut b, lease).unwrap_or_else(|e| panic!("start: {e:?}"));
    let r = entry.run(&mut exec, &mut b, Call::batch(1024));
    entry.end(exec, matches!(r, Ok(Step::Done)));
    assert!(matches!(r, Ok(Step::Done)), "{r:?}");
    drop(b);
    held(&SEEN).clone()
}

/// srf-003: the second entry reads function 1's cell (NULL: the row PG emits third, `(3, NULL)`), never function 0's 11.
#[test]
fn two_set_returning_entries_read_their_own_cells() {
    let seen = run("srf-003.plan");
    assert_eq!(seen.len(), 1, "one row: {seen:?}");
    let (v0, n0, _, n1) = seen[0];
    assert_eq!((v0, n0), (11, 0), "entry 1 reads function 0: {seen:?}");
    assert_eq!(n1, 1, "entry 2 reads function 1 (exhausted: NULL), not function 0's 11: {seen:?}");
}
