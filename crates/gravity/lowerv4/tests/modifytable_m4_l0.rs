//! M4 L0 (M4-plan §1.3, Decision 1; cards/M4.md L0-a / L0-b): the row op as a step (face/end.rs `RowOpMech`, made by
//! `write.delete`'s `RowOpMaker` row, write/rowop_step.rs), dormant until M4's write kinds call it. A one-pipeline
//! program (N rows, no site) whose step calls the row op runs through gravity_driver with a scripted writer: each status
//! arm (Done, Skipped, Conflict, Recheck, an error, a NULL junk ctid) takes the branch PG's ExecInsert / ExecUpdate /
//! ExecDelete does, the cells and the position reach the host by attno, and the conflict site answers the existing row.
//! M1's DELETE end is the same end (its 21 IR pins: tests/m1.rs `ir_pins`).
use gravity_lowerv4_it as common;

use std::sync::atomic::AtomicU8;
use std::sync::{Arc, Mutex};

use common::RigCatalog;
use gravity_abi::program::{WRITE_DELETE, WRITE_INSERT};
use gravity_decompose::{NoFacts, PipelineGraph, PipelineNode, Source};
use gravity_driver::{box_locker, box_writer, ArenaBox, BindingRef, Locker, Outcome, ScanPos, SourceBox, WriteOp, Writer};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, HostFlags, IntBinOp, Type, Value};
use gravity_lower::{assemble, compile, Backend, Sequential};
use gravity_lowerv4::face::end::{Arm, EndMech, LocFrom, RowOpDecl, RowOpMaker, RowOpMech, RowOpRow};
use gravity_lowerv4::face::kinds::KindId;
use gravity_lowerv4::face::source::{Locator, SourceMech};
use gravity_lowerv4::face::step::StepMech;
use gravity_lowerv4::frame::env::Env;
use gravity_lowerv4::frame::frame::{Mechs, Next, Win};
use gravity_lowerv4::frame::program;
use gravity_lowerv4::frame::px::Px;
use gravity_lowerv4::frame::row::{Carry, RowVals};
use gravity_lowerv4::frame::stop::Exits;
use gravity_lowerv4::graph::Gx;
use gravity_lowerv4::admit::refusal::Variant;
use gravity_lowerv4::{catch, Reason};
use gravity_program::{Arena, Cmd, End, Pipeline, SiteDesc, WritePlan};

const REL: u32 = 16_384;

/// The scripted host's record, process-wide (a static, not TLS: the session TLS census counts every thread_local!).
/// RUN serializes the tests that read it: each run() clears it, runs one program and takes it back under RUN.
static RUN: Mutex<()> = Mutex::new(());
/// What the IR reported after each call: ("done" | "conflict", row, the first cell the arm read back).
static SEEN: Mutex<Vec<(&'static str, u64, u64)>> = Mutex::new(Vec::new());
/// What the writer saw per call: (op, block, off, cells as (value, null)).
static CALLS: Mutex<Vec<(WriteOp, u32, u16, Vec<(u64, u8)>)>> = Mutex::new(Vec::new());

fn held<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

extern "C" fn seen(arm: u64, row: u64, v: u64) -> u64 {
    held(&SEEN).push((if arm == 0 { "done" } else { "conflict" }, row, v));
    0
}

/// `n` rows, no site: row `j`'s locator is `(j, 1)` for a driving row op.
struct Rows(i64);

impl SourceMech for Rows {
    fn once(&self) -> bool {
        true
    }
    fn open(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Value {
        b.iconst(Type::I64, 0)
    }
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _l_eof: Block) -> Win {
        Win { cursor: env.zero, end: b.iconst(Type::I64, self.0), facts: Default::default() }
    }
    fn row<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        RowVals { row: gravity_lowerv4::cell::Row::new_in(env.mcx), vals: mcx::pg_vec![in env.mcx; (j, None)], cols: mcx::PgVec::new_in(env.mcx), carry, next, at: j }
    }
    fn locator(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) -> Option<Locator> {
        let _ = b;
        Some(Locator { block: row.vals[0].0, offset: env.one, pinned: None, rel: None })
    }
}

/// The write kind's step as M4's kinds will write it: cells `(100 + j, j)` with the second NULL on odd rows, a junk ctid
/// `(j + 7, j + 1)` NULL on row `null_tid`, the row op, then the arms reporting what they read.
struct WriteStep<'g> {
    op: mcx::PgBox<'g, dyn RowOpMech + 'g>,
    natts: u16,
    junk: bool,
    driving: bool,
    conflict: bool,
    null_tid: i64,
    /// `call_op`'s per-call host op (MERGE's arm), and the cells the call passes (a DELETE may pass none).
    arm: Option<u32>,
    ncells: usize,
}

fn report(b: &mut FunctionBuilder<'_>, env: &Env, arm: i64, row: Value, v: Value) {
    let h = b.declare_host_once("m4_l0_seen", &[Type::I64, Type::I64, Type::I64], Some(Type::I64), seen as *const () as usize, HostFlags::default());
    let a = b.iconst(Type::I64, arm);
    let _ = b.call_host(h, &[a, row, v], Some(Type::I64));
    let _ = env;
}

impl StepMech for WriteStep<'_> {
    /// L0-f: a step that reads the locator gets it under an `end.sink`-style end (here `Count`).
    fn wants_locator(&self) -> bool {
        self.driving
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let j = row.vals[0].0;
        let hundred = b.iconst(Type::I64, 100);
        let v0 = b.int_bin(IntBinOp::Add, j, hundred);
        let t = b.bconst(true);
        let one = b.iconst(Type::I64, 1);
        let odd = b.int_bin(IntBinOp::And, j, one);
        let even = b.icmp(gravity_ir::IntCond::Eq, odd, env.zero);
        let cells = [(v0, t), (j, even)];
        let tid = (self.junk && self.arm != Some(WRITE_INSERT)).then(|| {
            let slot = b.declare_stack_slot(8, 8);
            let img = b.stack_addr(slot);
            let seven = b.iconst(Type::I64, 7);
            let blk = b.int_bin(IntBinOp::Add, j, seven);
            let lo = b.conv(ConvOp::Trunc, Type::I16, blk);
            let z16 = b.iconst(Type::I16, 0);
            b.store(img, 0, z16);
            b.store(img, 2, lo);
            let off = b.int_bin(IntBinOp::Add, j, one);
            let off = b.conv(ConvOp::Trunc, Type::I16, off);
            b.store(img, 4, off);
            let nt = b.iconst(Type::I64, self.null_tid);
            let valid = b.icmp(gravity_ir::IntCond::Ne, j, nt);
            (b.conv(ConvOp::PtrToInt, Type::I64, img), valid)
        });
        let r = RowOpRow { cells: &cells[..self.ncells], tid, rel: None };
        let op = &*self.op;
        let mut done = |b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, rv: &RowVals| {
            let c = op.cells(b, env, false);
            let v = c.first().map_or(env.zero, |c| c.0);
            report(b, env, 0, rv.vals[0].0, v);
            next(b, env, rv)
        };
        if let Some(arm) = self.arm {
            return op.call_op(b, env, row, arm, r, &mut done, Arm::None);
        }
        if !self.conflict {
            return op.call(b, env, row, r, &mut done, None);
        }
        let mut on_conflict = |b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, rv: &RowVals| {
            let c = op.cells(b, env, true);
            report(b, env, 1, rv.vals[0].0, c[0].0);
            rv.carry.copy_in(env.mcx)
        };
        op.call(b, env, row, r, &mut done, Some(&mut on_conflict))
    }
}

struct Count;

impl EndMech for Count {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        Carry { out: b.int_bin(IntBinOp::Add, row.carry.out, env.one), accs: mcx::to_vec_in(&row.carry.accs[..], env.mcx) }
    }
}

/// Answers each call from `script` in order; `Err` for a host error. The conflict row's first cell is 777.
struct Scripted(Vec<Result<Outcome, ()>>, usize);

impl Writer for Scripted {
    fn bind(&mut self, _b: &BindingRef<'_>) -> gravity_driver::Result<()> {
        Ok(())
    }
    fn write(&mut self, op: WriteOp, pos: ScanPos, values: &[u64], isnull: &[u8]) -> gravity_driver::Result<Outcome> {
        held(&CALLS).push((op, pos.block, pos.off, values.iter().copied().zip(isnull.iter().copied()).collect()));
        self.1 += 1;
        self.0.get(self.1 - 1).cloned().unwrap_or(Ok(Outcome::Done)).map_err(|()| gravity_driver::Error::Host("XX000\tscripted".into()))
    }
    fn existing(&mut self, values: &mut [u64], isnull: &mut [u8]) -> gravity_driver::Result<ScanPos> {
        values[0] = 777;
        isnull[0] = 0;
        Ok(ScanPos::NONE)
    }
    fn unbind(&mut self) {}
}

struct NoLock;

impl Locker for NoLock {
    fn bind(&mut self, _b: &BindingRef<'_>) -> gravity_driver::Result<()> {
        Ok(())
    }
    fn lock(&mut self, _pos: ScanPos) -> gravity_driver::Result<Outcome> {
        Ok(Outcome::Done)
    }
    fn unbind(&mut self) {}
}

struct Derive(Option<Vec<Result<Outcome, ()>>>);

impl gravity_driver::Derive for Derive {
    fn size_hint(&self, _: &SiteDesc) -> usize {
        0
    }
    fn derive_source(&mut self, _d: &SiteDesc, _a: gravity_program::ArenaRef) -> SourceBox {
        panic!("the row-op test program has no source site")
    }
    fn writer_size_hint(&self) -> usize {
        core::mem::size_of::<Scripted>()
    }
    fn derive_writer(&mut self, _plan: &WritePlan, a: gravity_program::ArenaRef) -> ArenaBox<dyn Writer> {
        box_writer(Scripted(self.0.take().unwrap_or_default(), 0), a)
    }
    fn locker_size_hint(&self) -> usize {
        core::mem::size_of::<NoLock>()
    }
    fn derive_locker(&mut self, _plan: &WritePlan, a: gravity_program::ArenaRef) -> ArenaBox<dyn Locker> {
        box_locker(NoLock, a)
    }
}

struct NoRows;

impl gravity_driver::Sink for NoRows {
    fn push(&mut self, _b: gravity_driver::BorrowedBatch<'_>) -> gravity_driver::Result<gravity_driver::Flow> {
        Ok(gravity_driver::Flow::Continue)
    }
}

struct Case {
    rows: i64,
    op: u32,
    natts: u16,
    loc: LocFrom,
    conflict: bool,
    null_tid: i64,
    script: Vec<Result<Outcome, ()>>,
}

/// The step calls it through the registry, as M4's kinds do.
fn maker() -> &'static RowOpMaker {
    gravity_lowerv4::ops::kind_as::<RowOpMaker>(KindId::WriteDelete).expect("write.delete registers the row-op maker")
}

/// `c`'s program run to its end: the driver's result (the recheck step as `Err(Recheck)`), the completion count, the arms'
/// reports, the writer's calls.
#[allow(clippy::type_complexity)]
fn run(c: Case) -> (Result<(), gravity_driver::Error>, u64, Vec<(&'static str, u64, u64)>, Vec<(WriteOp, u32, u16, Vec<(u64, u8)>)>) {
    let n = c.natts as usize;
    run_arm(c, None, n)
}

/// `run` with `call_op`'s per-call op `arm` and `ncells` cells per call.
#[allow(clippy::type_complexity)]
fn run_arm(c: Case, arm: Option<u32>, ncells: usize) -> (Result<(), gravity_driver::Error>, u64, Vec<(&'static str, u64, u64)>, Vec<(WriteOp, u32, u16, Vec<(u64, u8)>)>) {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    let _run = held(&RUN);
    held(&SEEN).clear();
    held(&CALLS).clear();
    RigCatalog::install_seams();
    let arena = Arena::new(1 << 20);
    let a = arena.handle();
    let mut g = PipelineGraph::new(a);
    g.pipelines.push(PipelineNode::new(a, 3, Source::Const, Pipeline::serial(-1, End::Sink, gravity_program::Reason::Write)));
    let gx = Gx { graph: &g, facts: &NoFacts, catalog: RigCatalog::get(), over: None, mcx: rig.mcx() };
    let mut words = Sequential::new(a);
    let mut px = Px::new(a, &gx, &mut words);
    let cmd = if c.op == WRITE_INSERT { Cmd::Insert } else { Cmd::Delete };
    let decl = RowOpDecl { op: c.op, relid: REL, natts: c.natts, plan: WritePlan::write(a, cmd, REL, &[]), loc: c.loc, conflict: c.conflict };
    let op = (maker().0)(&mut px, 0, decl);
    let nsites = 1 + usize::from(c.conflict);
    let step = WriteStep { op, natts: c.natts, junk: matches!(c.loc, LocFrom::JunkCtid { .. }), driving: c.loc == LocFrom::Driving, conflict: c.conflict, null_tid: c.null_tid, arm, ncells };
    let m = Mechs::new(&mut px, 0, mcx::pg_box_dyn!(rig.mcx(), Rows(c.rows) => dyn SourceMech), mcx::pg_vec![in rig.mcx(); mcx::pg_box_dyn!(rig.mcx(), step => dyn StepMech + '_)], mcx::pg_box_dyn!(rig.mcx(), Count => dyn EndMech));
    let l = program::emit(&mut px, vec![m]);
    assert_eq!(l.descs.layout.nsites as usize, nsites, "the write site, and the conflict site right after it");
    gravity_ir::verify::verify(&l.ir).expect("the row op's IR verifies");
    let codes = compile(&l, Backend::Interp).unwrap_or_else(|e| panic!("backend: {}", e.message()));
    let program = Arc::new(assemble(&l, codes));
    let flag = AtomicU8::new(0);
    let mut sink = NoRows;
    use gravity_driver::{Binding, BindingParts, Call, CommandId, Entry, Grant, Identity, PerTuple, Pins, Reservation, Snapshot, Step};
    let parts = BindingParts { snapshot: Snapshot(0), command_id: CommandId(0), params: &[0], param_nulls: &[0], abort_flag: &flag, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity { proc_number: 0, resource_owner: 0, latch: 0 }, resolved: None };
    let mut b = Binding::new(parts, &mut sink);
    let mut entry = Entry::new(1);
    let lease = std::rc::Rc::new(Reservation::unadmitted(Grant::of(64 << 20)));
    let mut derive = Derive(Some(c.script));
    let r = match entry.start(None, &program, &mut derive, &mut b, lease) {
        Err(e) => Err(e),
        Ok(mut exec) => {
            let r = match entry.run(&mut exec, &mut b, Call::batch(1024)) {
                Ok(Step::Done) => Ok(()),
                Ok(Step::Recheck { site }) => Err(gravity_driver::Error::Recheck(site)),
                Ok(s) => panic!("{s:?}"),
                Err(e) => Err(e),
            };
            entry.end(exec, r.is_ok());
            r
        }
    };
    let tag = b.rows_processed;
    drop(b);
    (r, tag, held(&SEEN).clone(), held(&CALLS).clone())
}

/// INSERT: the cells by attno (NULL as a zero word with its null byte), no position; Done goes on with the written row,
/// Skipped goes nowhere, Conflict takes the arm with the existing row in the conflict site; only Done counts.
#[test]
fn insert_arms_done_skipped_conflict() {
    let script = vec![Ok(Outcome::Done), Ok(Outcome::Skipped), Ok(Outcome::Conflict), Ok(Outcome::Done)];
    let (r, tag, seen, calls) = run(Case { rows: 4, op: WRITE_INSERT, natts: 2, loc: LocFrom::None, conflict: true, null_tid: -1, script });
    assert_eq!(r.map_err(|e| format!("{e:?}")), Ok(()));
    assert_eq!(tag, 2, "the command tag counts the rows written");
    assert_eq!(seen, vec![("done", 0, 100), ("conflict", 2, 777), ("done", 3, 103)]);
    let cells: Vec<_> = calls.iter().map(|c| (c.0, c.1, c.2, c.3.clone())).collect();
    assert_eq!(cells, (0..4u64).map(|j| (WriteOp::Insert, 0, 0, vec![(100 + j, 0), if j % 2 == 0 { (j, 0) } else { (0, 1) }])).collect::<Vec<_>>());
}

/// A host error stops at the row: nothing after it is written or reported.
#[test]
fn a_host_error_stops_at_its_row() {
    let script = vec![Ok(Outcome::Done), Err(())];
    let (r, _, seen, calls) = run(Case { rows: 4, op: WRITE_INSERT, natts: 2, loc: LocFrom::None, conflict: false, null_tid: -1, script });
    assert!(matches!(r, Err(gravity_driver::Error::Host(_))), "{r:?}");
    assert_eq!((seen, calls.len()), (vec![("done", 0, 100)], 2));
}

/// Without a conflict site, CONFLICT is not an answer this op takes: the driver fails the call (no conflict site to fill,
/// dml.rs), and the IR's unsupported arm stands behind it; never the Skipped branch, never a row after.
#[test]
fn conflict_without_a_conflict_site_is_an_error() {
    let (r, _, seen, _) = run(Case { rows: 2, op: WRITE_INSERT, natts: 2, loc: LocFrom::None, conflict: false, null_tid: -1, script: vec![Ok(Outcome::Conflict)] });
    assert!(matches!(r, Err(gravity_driver::Error::BadReturn(1) | gravity_driver::Error::Unsupported(_))), "{r:?}");
    assert!(seen.is_empty());
}

/// A junk ctid: its (block, offset) is the write's position; a recheck stops after the row naming the declared site.
#[test]
fn junk_ctid_position_and_recheck_site() {
    let script = vec![Ok(Outcome::Done), Ok(Outcome::Recheck)];
    let (r, _, seen, calls) = run(Case { rows: 4, op: WRITE_DELETE, natts: 0, loc: LocFrom::JunkCtid { site: 0 }, conflict: false, null_tid: -1, script });
    assert!(matches!(r, Err(gravity_driver::Error::Recheck(0))), "{r:?}");
    assert_eq!(seen, vec![("done", 0, 0)]);
    assert_eq!(calls.iter().map(|c| (c.0, c.1, c.2)).collect::<Vec<_>>(), vec![(WriteOp::Delete, 7, 1), (WriteOp::Delete, 8, 2)]);
}

/// A NULL junk ctid (PG: "ctid is NULL") never reaches the host: unsupported at the row.
#[test]
fn a_null_junk_ctid_is_unsupported() {
    let (r, _, seen, calls) = run(Case { rows: 3, op: WRITE_DELETE, natts: 0, loc: LocFrom::JunkCtid { site: 0 }, conflict: false, null_tid: 1, script: Vec::new() });
    assert!(matches!(r, Err(gravity_driver::Error::Unsupported(_))), "{r:?}");
    assert_eq!((seen.len(), calls.len()), (1, 1));
}

/// A DELETE declared with the relation's cells (RETURNING reads the deleted row the host puts there) passes none per call.
#[test]
fn a_delete_passes_no_cells() {
    let c = Case { rows: 2, op: WRITE_DELETE, natts: 2, loc: LocFrom::JunkCtid { site: 0 }, conflict: false, null_tid: -1, script: Vec::new() };
    let (r, tag, _, calls) = run_arm(c, None, 0);
    assert_eq!(r.map_err(|e| format!("{e:?}")), Ok(()));
    assert_eq!(tag, 2);
    assert_eq!(calls.iter().map(|c| (c.0, c.1, c.2)).collect::<Vec<_>>(), vec![(WriteOp::Delete, 7, 1), (WriteOp::Delete, 8, 2)]);
}

/// `call_op` (MERGE's arms, M4-TRIG T2): each call names its host op over the one write site. A DELETE arm passes the
/// junk ctid and no cells; an INSERT arm passes the cells and no tid, and writes with an empty position.
#[test]
fn call_op_names_the_host_op_per_call() {
    let upd = gravity_abi::program::WRITE_UPDATE;
    let c = || Case { rows: 2, op: upd, natts: 2, loc: LocFrom::JunkCtid { site: 0 }, conflict: false, null_tid: -1, script: Vec::new() };
    let (r, _, _, calls) = run_arm(c(), Some(WRITE_DELETE), 0);
    assert_eq!(r.map_err(|e| format!("{e:?}")), Ok(()));
    assert_eq!(calls.iter().map(|c| (c.0, c.1, c.2)).collect::<Vec<_>>(), vec![(WriteOp::Delete, 7, 1), (WriteOp::Delete, 8, 2)]);
    let (r, tag, _, calls) = run_arm(c(), Some(WRITE_INSERT), 2);
    assert_eq!(r.map_err(|e| format!("{e:?}")), Ok(()));
    assert_eq!(tag, 2);
    assert_eq!(calls.iter().map(|c| (c.0, c.3[0].0)).collect::<Vec<_>>(), vec![(WriteOp::Insert, 100), (WriteOp::Insert, 101)]);
}

/// G4: a second row op in one program refuses by the kind that serves a write feeding its statement, `nest.cte`.
#[test]
fn a_second_row_op_refuses_as_nest_cte() {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    RigCatalog::install_seams();
    let arena = Arena::new(1 << 16);
    let a = arena.handle();
    let mut g = PipelineGraph::new(a);
    g.pipelines.push(PipelineNode::new(a, 3, Source::Const, Pipeline::serial(-1, End::Sink, gravity_program::Reason::Write)));
    let gx = Gx { graph: &g, facts: &NoFacts, catalog: RigCatalog::get(), over: None, mcx: rig.mcx() };
    let mut words = Sequential::new(a);
    let mut px = Px::new(a, &gx, &mut words);
    let decl = || RowOpDecl { op: WRITE_INSERT, relid: REL, natts: 1, plan: WritePlan::write(a, Cmd::Insert, REL, &[]), loc: LocFrom::None, conflict: false };
    let _first = (maker().0)(&mut px, 0, decl());
    let r = catch(3, || drop((maker().0)(&mut px, 0, decl())));
    let e = r.err().expect("a second row op refuses");
    assert!(matches!(e.reason, Reason::NotBuilt { variant: Variant::Kind(KindId::NestCte), .. }), "{e:?}");
}

/// L0-f + the driving locator: a step declaring `wants_locator` gets the source's locator under a plain end, and the row
/// op writes at it; Skipped goes nowhere.
#[test]
fn driving_locator_reaches_a_step_under_a_plain_end() {
    let script = vec![Ok(Outcome::Done), Ok(Outcome::Skipped), Ok(Outcome::Done)];
    let (r, tag, seen, calls) = run(Case { rows: 3, op: WRITE_DELETE, natts: 0, loc: LocFrom::Driving, conflict: false, null_tid: -1, script });
    assert_eq!(r.map_err(|e| format!("{e:?}")), Ok(()));
    assert_eq!((tag, seen), (2, vec![("done", 0, 0), ("done", 2, 0)]));
    assert_eq!(calls.iter().map(|c| (c.0, c.1, c.2)).collect::<Vec<_>>(), vec![(WriteOp::Delete, 0, 1), (WriteOp::Delete, 1, 1), (WriteOp::Delete, 2, 1)]);
}

/// The write site (and the conflict site after it) is numbered at emit after every source site, whatever the order of the
/// makes: exec admit's host registry is positional over the source sites (M4-INSERT's INSERT ... SELECT over a join, whose
/// build pipeline's source is made after the write op).
#[test]
fn write_sites_follow_every_source_site() {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    RigCatalog::install_seams();
    let arena = Arena::new(1 << 16);
    let a = arena.handle();
    let mut g = PipelineGraph::new(a);
    g.pipelines.push(PipelineNode::new(a, 3, Source::Const, Pipeline::serial(-1, End::Sink, gravity_program::Reason::Write)));
    let gx = Gx { graph: &g, facts: &NoFacts, catalog: RigCatalog::get(), over: None, mcx: rig.mcx() };
    let mut words = Sequential::new(a);
    let mut px = Px::new(a, &gx, &mut words);
    let decl = RowOpDecl { op: WRITE_INSERT, relid: REL, natts: 2, plan: WritePlan::write(a, Cmd::Insert, REL, &[]), loc: LocFrom::None, conflict: true };
    let op = (maker().0)(&mut px, 0, decl);
    let src = px.add_site(SiteDesc::range_source(a, REL + 7, &[1]));
    px.place_late_sites();
    assert_eq!((src, op.site()), (0, 1), "a source made after the row op keeps its number; the write site follows it");
    assert!(matches!(px.sites[1].kind, gravity_program::SiteKind::Write { natts: 2 }));
    assert!(matches!(px.sites[2].kind, gravity_program::SiteKind::Write { natts: 2 }), "the conflict site right after");
}

/// L0-i (V4R-204, M4-plan Decision 2): write.route applies to a routed INSERT / MERGE or a multi-target write, not to
/// every INSERT. G4: a plain INSERT takes no route.
#[test]
fn write_route_applies_by_fact() {
    use gravity_lowerv4::applies::{strategies_for, Prop, PropSet};
    use gravity_lowerv4::face::kinds::KindSet;
    use gravity_lowerv4::face::strategy::{Strategy, WRITE_CHECKS, WRITE_LITERALS, WRITE_ROUTE};
    let props = |ps: &[Prop]| {
        let mut s = PropSet::default();
        ps.iter().for_each(|&p| s.add(p));
        s
    };
    let ins = KindSet::of(&[KindId::WriteInsert]);
    let on = |p: &[Prop], r: Strategy| strategies_for(ins, props(p)).has(r);
    assert!(!on(&[Prop::ValuesOnly], WRITE_ROUTE), "a plain INSERT ... VALUES is not routed");
    assert!(on(&[Prop::ValuesOnly], WRITE_LITERALS) && on(&[Prop::ValuesOnly], WRITE_CHECKS), "literals / checks stay on every INSERT");
    assert!(on(&[Prop::Route], WRITE_ROUTE) && on(&[Prop::MultiTarget], WRITE_ROUTE));
    assert!(!on(&[], WRITE_ROUTE) && !on(&[Prop::RowTrigger], WRITE_ROUTE), "INSERT ... SELECT, a row trigger: no route");
}

/// D652 (D610's rest): an UPDATE / DELETE / MERGE row op beside another pipeline that is not an arm of its Append (the
/// leader's recheck runs the stopped pipeline alone) refuses by name; an INSERT there (it never rechecks) makes.
#[test]
fn a_rechecking_write_over_several_pipelines_refuses() {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    RigCatalog::install_seams();
    let arena = Arena::new(1 << 16);
    let a = arena.handle();
    let mut g = PipelineGraph::new(a);
    for _ in 0..2 {
        g.pipelines.push(PipelineNode::new(a, 3, Source::Const, Pipeline::serial(-1, End::Sink, gravity_program::Reason::Write)));
    }
    let gx = Gx { graph: &g, facts: &NoFacts, catalog: RigCatalog::get(), over: None, mcx: rig.mcx() };
    for (op, cmd, refuses) in [(WRITE_INSERT, Cmd::Insert, false), (gravity_abi::program::WRITE_UPDATE, Cmd::Update, true), (WRITE_DELETE, Cmd::Delete, true)] {
        let mut words = Sequential::new(a);
        let mut px = Px::new(a, &gx, &mut words);
        let loc = if cmd == Cmd::Insert { LocFrom::None } else { LocFrom::Driving };
        let decl = RowOpDecl { op, relid: REL, natts: 1, plan: WritePlan::write(a, cmd, REL, &[]), loc, conflict: false };
        let r = catch(3, || drop((maker().0)(&mut px, 0, decl)));
        match r {
            Err(e) => assert!(refuses && matches!(e.reason, Reason::Shape { why } if why.0.contains("D652")), "{cmd:?}: {e:?}"),
            Ok(()) => assert!(!refuses, "{cmd:?} over two pipelines made"),
        }
    }
}

/// D655 (V4R-256): a rechecking UPDATE / DELETE in a pipeline that produces for another refuses by name (the leader's recheck
/// reruns the producers, then the emit: a producer's own stop has no plan to rerun by); the emit beside that producer, whose
/// marks name none of it, keeps D652.
#[test]
fn a_rechecking_write_in_a_producer_refuses_by_name() {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    use gravity_abi::breaker::{Breaker, BreakerKind};
    RigCatalog::install_seams();
    let arena = Arena::new(1 << 16);
    let a = arena.handle();
    let mut g = PipelineGraph::new(a);
    g.pipelines.push(PipelineNode::new(a, 3, Source::Const, Pipeline::serial(-1, End::Breaker(Breaker::sized(BreakerKind::HashBuild, 64).in_slot(0)), gravity_program::Reason::Write)));
    g.pipelines.push(PipelineNode::new(a, 3, Source::Const, Pipeline::serial(-1, End::Sink, gravity_program::Reason::Write)));
    let gx = Gx { graph: &g, facts: &NoFacts, catalog: RigCatalog::get(), over: None, mcx: rig.mcx() };
    for (k, cmd, why) in [(0, Cmd::Update, "produces for another"), (0, Cmd::Delete, "produces for another"), (1, Cmd::Update, "D652")] {
        let mut words = Sequential::new(a);
        let mut px = Px::new(a, &gx, &mut words);
        let op = if cmd == Cmd::Delete { WRITE_DELETE } else { gravity_abi::program::WRITE_UPDATE };
        let decl = RowOpDecl { op, relid: REL, natts: 1, plan: WritePlan::write(a, cmd, REL, &[]), loc: LocFrom::Driving, conflict: false };
        match catch(3, || drop((maker().0)(&mut px, k, decl))) {
            Err(e) => assert!(matches!(e.reason, Reason::Shape { why: w } if w.0.contains(why)), "{k} {cmd:?}: {e:?}"),
            Ok(()) => panic!("{k} {cmd:?} made"),
        }
    }
}

/// L0-e (M4-TRIG, main 09-25): write.delete's end hands a firing trigger set (oltp-033 `DELETE FROM ol_del WHERE k = $1`
/// with an AFTER DELETE ROW trigger) and a view target (triggers-018, its INSTEAD OF trigger the write) to write.transition,
/// NotBuilt by name until M4-TRIG registers the kind. The all-blockers mode goes past admission: the dry run's 115 such
/// DELETEs met a Bug at the end; a trigger-free DELETE (G4) still makes its end.
#[test]
fn a_delete_hands_triggers_and_a_view_to_write_transition() {
    use gravity_lowerv4::frame::diag;
    let lower = |text: &str| {
        RigCatalog::install_seams();
        let (ctx, arena) = (mcx::MemoryContext::new("v4 m4_l0 fixture"), Arena::new(1 << 20));
        let fx = gravity_decompose::corpus::load(ctx.mcx(), text).unwrap_or_else(|e| panic!("fixture: {e}"));
        // the server's member set (exec admit.rs: decompose_in records every member, TargetView among them)
        let graph = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).expect("decomposes");
        gravity_expr::nodes::func::acl_defer_begin();
        let (_, report) = diag::lower_graph_diag(arena.handle(), &graph, &fx.facts, RigCatalog::get(), &mut Sequential::new(arena.handle()), gravity_lower::LowerOpts::default());
        gravity_expr::nodes::func::acl_defer_end();
        report.blockers.iter().filter(|b| b.pipe.is_some()).map(|b| (b.refusal.reason, b.cascade)).collect::<Vec<_>>()
    };
    let text = |rel: &str| std::fs::read_to_string(common::plans_dir().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
    let transition = |(r, _): &(Reason, bool)| matches!(r, Reason::NotBuilt { variant: Variant::Kind(KindId::WriteTransition), .. });
    // a Bug that is not a cascade of an earlier blocker (the mode's placeholder artefacts are cascades, frame/diag.rs) nor
    // the unit catalog's gap (no typcache seam for the view's wholerow type: `seam not installed`, a rig limit, not a make)
    let bug = |(r, cascade): &(Reason, bool)| matches!(r, Reason::Bug { payload } if !payload.starts_with("seam not installed")) && !cascade;
    let plain = text("oltp-033.plan");
    assert!(!lower(&plain).iter().any(|b| transition(b) || bug(b)), "oltp-033 without a trigger");
    let fired = plain.replace("-- trigext 16534 before=- after=-", "-- trigext 16534 before=- after=d");
    assert!(fired != plain);
    let got = lower(&fired);
    // M4-TRIG T1 built write.transition: the hand-off lowers and serves (no refusal of any kind) where it refused by name
    assert!(got.is_empty(), "an AFTER DELETE trigger: {got:?}");
    // the view target: only the rig catalog's gap and its cascades; the server runner serves it (e2e m4_views). Since D1191
    // the rig answers the typcache seam from pg_type.dat, so the gap is the view's own row type (a relation the test
    // creates: no pg_type.dat row), the expression's catalog refusal by name, or (exprType naming that row type) the output
    // type's
    let got = lower(&text("regress-full/triggers-018.plan"));
    let gap = |(r, cascade): &(Reason, bool)| *cascade || matches!(r, Reason::Shape { why } if why.0 == "an expression the catalog refused" || why.0 == "an output type the catalog does not know");
    assert!(got.iter().all(gap), "a view target: {got:?}");
}
