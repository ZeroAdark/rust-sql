//! D664 (main 09-26, found reviewing V4R-211 against D641): a Node's `spill_epoch` started at 0, so a Node re-created at
//! a dropped Node's address showed the epoch a lane recorded on the old one, and the lane (lowerv4 hir.rs `current`: same
//! ctx, same epoch) kept the chunk records of spills the old Node's settle had already freed (a latent use-after-free).
//! A lane forgets its records only when the (ctx, epoch) it recorded changes, and the allocator may hand a new Node the
//! old one's address, so the epoch alone must never repeat. Deterministic, never by address luck: each statement runs on
//! its own Entry (a Node built fresh, as a re-created Node is) after the last one drew a spill region and settled; at the
//! parent every fresh Node shows 0, so the second statement repeats the first's epoch on every run.
use gravity_lowerv4_it as common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use gravity_abi::Ctx;
use gravity_driver::{ArenaBox, Binding, BindingParts, CommandId, Entry, Grant, Identity, Locker, PerTuple, Pins, Reservation, Snapshot, SourceBox, Writer};
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


/// The spill epoch a hosted function entering the statement's ctx would record, for each of `n` statements, each on a
/// fresh Entry (its Node built anew) and each drawing one spill region before it ends (the settle drops it).
fn epochs(n: usize) -> Vec<u64> {
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
    let mut seen = Vec::new();
    for _ in 0..n {
        let mut entry = Entry::new(1);
        let mut sink = NoRows;
        let parts = BindingParts { snapshot: Snapshot(0), command_id: CommandId(0), params: &[0], param_nulls: &[0], abort_flag: &flag, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity { proc_number: 0, resource_owner: 0, latch: 0 }, resolved: None };
        let mut b = Binding::new(parts, &mut sink);
        let lease = std::rc::Rc::new(Reservation::unadmitted(Grant::of(1 << 30)));
        let exec = entry.start(None, &program, &mut Derive, &mut b, lease).unwrap_or_else(|e| panic!("start: {e:?}"));
        let ctx: *mut Ctx = exec.node().ctx();
        // SAFETY: `ctx` is the started statement's bound ctx.
        unsafe {
            seen.push(gravity_driver::hostimpl::arena::spill_epoch(ctx));
            assert!(!gravity_driver::hostimpl::arena::spill_region(ctx, 1 << 20).is_null(), "the spill draw");
        }
        entry.end(exec, true);
    }
    seen
}

/// No statement shows an epoch an earlier one showed: a lane that recorded chunks on a dropped Node forgets them on a Node
/// re-created at its address.
#[test]
fn a_recreated_node_never_shows_a_recorded_epoch() {
    let seen = epochs(4);
    for (i, e) in seen.iter().enumerate() {
        assert!(!seen[..i].contains(e), "statement {i} shows epoch {e} again: {seen:?}");
    }
}
