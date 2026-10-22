//! D638 (main 09-26, from the v3 report "gravity_fn: the arena cannot hold the set's row", 53200): `arena_alloc` grew
//! the window by the raw request while `arena_bump` takes it rounded up to 8, so a request past the 64 kB chunk that
//! is not a multiple of 8 got a chunk of exactly its size, 4 bytes short of the bump. This drives the real library
//! entry (`string_to_table`, whose row is copied into the ctx arena at `step`: fn/src/srf.rs `arena_text`) through a
//! real driver ctx: rows of 69,996 / 70,000 B and 1,048,572 / 1,048,576 B (images of a multiple of 8 and not), and
//! 100 MB. Each is its value, never 53200.
use gravity_lowerv4_it as common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use gravity_abi::Ctx;
use gravity_driver::{ArenaBox, Locker, SourceBox, Writer};
use gravity_fn::srf::{entry_of, OUT_WORDS, STATE_BYTES};
use gravity_fn::ErrSlot;
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

/// `string_to_table(text, text)`; an empty delimiter yields the input as one row (no collation read).
const STRING_TO_TABLE: u32 = 6160;

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

/// A 4-byte-header varlena image of `n` bytes of `c`.
fn text(n: usize, c: u8) -> Vec<u8> {
    let mut v = Vec::with_capacity(n + 4);
    v.extend_from_slice(&datum::set_varsize_4b(n + 4));
    v.resize(n + 4, c);
    v
}

/// The varlena's payload at `d` (4-byte header).
unsafe fn payload<'a>(d: i64) -> &'a [u8] {
    let p = d as usize as *const u8;
    let n = (p.cast::<u32>().read_unaligned() >> 2) as usize;
    std::slice::from_raw_parts(p.add(4), n - 4)
}

/// `string_to_table(img, '')` opened and stepped on `ctx`: the row's datum, or the error's (sqlstate, message).
unsafe fn one_row(ctx: *mut Ctx, img: &[u8]) -> Result<i64, (String, String)> {
    let e = entry_of(STRING_TO_TABLE).expect("string_to_table is a library entry");
    let empty = text(0, 0);
    let av = [img.as_ptr() as i64, empty.as_ptr() as i64];
    let an = [0u8; 2];
    let mut state = vec![0u8; STATE_BYTES];
    let mut err = ErrSlot::NONE;
    let fail = |err: &mut ErrSlot| err.take().map_or(("?".into(), "?".into()), |x| (format!("{:?}", x.sqlstate()), x.message().to_owned()));
    if (e.open)(&mut err, ctx, state.as_mut_ptr(), av.as_ptr(), an.as_ptr(), 2, 100) < 0 {
        return Err(fail(&mut err));
    }
    let mut out = [0i64; OUT_WORDS];
    match (e.step)(&mut err, ctx, state.as_mut_ptr(), out.as_mut_ptr()) {
        1 => Ok(out[0]),
        r if r < 0 => Err(fail(&mut err)),
        r => panic!("step {r}: no row"),
    }
}

/// `f` over a bound driver ctx (one Call's arena window, as the program's steps would see it).
fn with_ctx<T>(f: impl FnOnce(*mut Ctx) -> T) -> T {
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
    use gravity_driver::{Binding, BindingParts, CommandId, Entry, Grant, Identity, PerTuple, Pins, Reservation, Snapshot};
    let parts = BindingParts { snapshot: Snapshot(0), command_id: CommandId(0), params: &[0], param_nulls: &[0], abort_flag: &flag, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity { proc_number: 0, resource_owner: 0, latch: 0 }, resolved: None };
    let mut b = Binding::new(parts, &mut sink);
    let mut entry = Entry::new(1);
    let lease = std::rc::Rc::new(Reservation::unadmitted(Grant::of(1 << 30)));
    let exec = entry.start(None, &program, &mut Derive, &mut b, lease).unwrap_or_else(|e| panic!("start: {e:?}"));
    let r = f(exec.node().ctx());
    entry.end(exec, true);
    r
}

/// Rows whose images are and are not a multiple of 8, past one 64 kB chunk, and 100 MB: each is PostgreSQL's value.
#[test]
fn a_large_row_is_its_value() {
    for n in [69_996usize, 70_000, (1 << 20) - 4, 1 << 20, 100 << 20] {
        let img = text(n, b'a');
        let got = with_ctx(|ctx| unsafe { one_row(ctx, &img).map(|d| payload(d).len()) });
        assert_eq!(got, Ok(n), "a {n} B row");
    }
}
