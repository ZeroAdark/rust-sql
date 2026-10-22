//! The lowering seam (the rung ladder, lowerv2 and lowerv3 were removed 2026-09-23: tags `ladder-final`, `lowerv2-final`,
//! `lowerv3-final`): what the entry hands a lowerer and gets back (`Lowered`, `Descs`, the word plan, `assemble`), the content
//! key, the spool and SubPlan-set runtime faces. Until lowerv4 installs itself every graph is one typed refusal: no fallback here, ever.
#![allow(clippy::too_many_arguments)]
#![deny(unsafe_code)]

mod meeting;

use core::fmt;

use gravity_decompose::{PipelineGraph, Source};
use gravity_expr::Catalog;
use gravity_ir::Func;
use gravity_program::{ArenaRef, ArenaVec, Code, Layout, OutCol, Pipeline, Program, SiteDesc, WritePlan};
use types_error::{PgError, PgResult};
use types_nodes::primnodes::INDEX_VAR;

pub use crate::rerun::words::{reads_deferred, Located, Place, Sequential, WordPlan, WordSrc, DEFERRED_AHEAD};

/// PostgreSQL's check_stack_depth at a level of one of the seam's recursions over the plan and its expressions (the
/// content key's walks): decompose's guard, 54001 past max_stack_depth, never the thread's stack end.
#[inline]
pub(crate) fn stack_check() {
    gravity_decompose::walk::stack_check();
}

/// A graph no milestone lowers: `module` names the ladder file, `what` the op, `node` the plan node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub module: &'static str,
    pub what: &'static str,
    pub node: i32,
    pub detail: &'static str,
    pub oid: u32,
    pub native_rel: u32, // PGRCSERVE M0 (D2): a pgrc relation whose rows road serves this shape; else 0
}

impl Refusal {
    pub const fn not_lowered(module: &'static str, what: &'static str, node: i32) -> Refusal {
        Refusal { module, what, node, detail: "", oid: 0, native_rel: 0 }
    }

    /// The lowerer stopped for a pending interrupt (D231): the admission raises it, never logs or remembers the refusal.
    pub fn is_interrupted(&self) -> bool {
        self.what == INTERRUPTED
    }

    pub fn naming(self, c: &gravity_expr::Compiled<'_>) -> Refusal {
        match gravity_expr::nodes::hostcall::first(&c.prog) {
            Some(h) => Refusal { detail: if h.name.is_empty() { "user-defined" } else { h.name }, oid: h.oid, ..self },
            None => self,
        }
    }
}

/// The refusal text of a lowering stopped by a pending interrupt (D231; the facade's `cancelpoll::interrupts::pending`).
pub const INTERRUPTED: &str = "Interrupted";

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.oid == 0 {
            return write!(f, "{}: {} (node {})", self.module, self.what, self.node);
        }
        write!(f, "{}: {} [{} {}] (node {})", self.module, self.what, self.oid, self.detail, self.node)
    }
}

impl From<Refusal> for Box<PgError> {
    fn from(r: Refusal) -> Box<PgError> {
        Box::new(PgError::error(r.to_string()).with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED)) // alloc: Cold
    }
}

pub type Result<T> = core::result::Result<T, Refusal>;

/// The descriptors `Program::with_pipelines` takes, one `Pipeline` per lowered function.
pub struct Descs {
    pub layout: Layout,
    pub sites: ArenaVec<SiteDesc>,
    pub out: ArenaVec<OutCol>,
    pub write: WritePlan,
    pub pipelines: ArenaVec<Pipeline>,
    pub out_region: u32,
    /// The hosted emit's state, shared with every `Program` `assemble` builds from these descriptors.
    pub owner: Owner,
    /// The program owns statement-specific bytes (a literal table): the executor never files it in the program cache.
    pub no_cache: bool,
    /// Why `no_cache` (the admit report's `tier=miss:no_cache.<why>`): `data` a literal table's bytes in the program,
    /// `list` a read of a list the content key keys by its class (V4R-110), `errorhold` the plan-order program; "" filed.
    pub no_cache_why: &'static str,
    pub instr: Vec<(i32, u8)>, // EXPLAIN ANALYZE's counter cells (plan node, counter) of an instrumented lowering; else empty
    pub v4: gravity_program::protos::V4Descs, // the program's runtime protocols (lowerv4); empty for none
    pub used: u128, // the kinds that made the program (lowerv4 mechs_used.rs, KindSet bits): the report's mechanisms line, never keyed
    pub locks: Vec<gravity_program::program::LockSite>, // a LockRows' several locks (`Program.locks`); empty for any other
    pub writes: Vec<gravity_program::program::WriteAt>, // D1687: the write plans beside `write` (`Program.writes`); empty for one
    pub write_end: u16, // D2605: where `write`'s ModifyTable ends (`Program.write_end`); `END_STATEMENT` for most
}

pub type Owner = core::cell::Cell<Option<(std::rc::Rc<dyn core::any::Any>, Vec<u32>)>>;

pub fn no_owner() -> Owner {
    core::cell::Cell::new(None)
}

/// `ArenaRef` borrows nothing, so a `Descs` can drop after its arena (a `match` tail temporary): forget the vectors — their drop glue walks `SiteDesc`s in the unmapped block and an arena frees nothing.
impl Drop for Descs {
    fn drop(&mut self) {
        core::mem::forget(core::mem::replace(&mut self.sites, ArenaVec::new_in(ArenaRef::EMPTY)));
        core::mem::forget(core::mem::replace(&mut self.out, ArenaVec::new_in(ArenaRef::EMPTY)));
        core::mem::forget(core::mem::replace(&mut self.write, WritePlan::SELECT));
        core::mem::forget(core::mem::replace(&mut self.pipelines, ArenaVec::new_in(ArenaRef::EMPTY)));
    }
}

/// The graph lowered to IR: descriptors + one function per pipeline; `assemble` takes the codes.
pub struct Lowered {
    pub ir: gravity_ir::Program,
    pub funcs: ArenaVec<Func>,
    pub descs: Descs,
    /// `ir`'s code key (`code_key`) when the lowerer took it over the finished IR (lowerv4's verify once keys by it too);
    /// `None`: the backend takes it. D3886: the IR was hashed again in the backend's lookup, its tier-0 codes and its compile.
    /// Whatever changes `ir` after the lowering must clear it.
    pub key: Option<u128>,
    /// D4005: the IR lines the lowerer attributed to a plan node whose rows run them (`FunctionBuilder::tag_lines`: a
    /// projection's entries, once per row its node emits), `(plan_node_id, lines)`: tier 0's break-even weighs them by
    /// that node's planned rows (exec knobs/tier0.rs `site_rows`, gravity_backend `mapped_cached_keyed`).
    pub tagged: Vec<(i32, u32)>,
    /// D3168 S0: the pipelines whose plain function hands its call to a staged one (`status::STAGED`): the plain function,
    /// the staged one in a module of its own, made the first time it is wanted (`assemble`; none: the shape's memo skipped
    /// its lowering, gravity_program `want`), and its memo tag (0: none); charged to the running statement, as the lowering
    /// is (lowerv4 `emit`).
    pub staged: gravity_fn::arena::ChargedVec<(Func, Option<gravity_ir::Program>, u64)>,
}

/// The code-cache key of an IR program (gravity_backend `cache::key_of`), for `Lowered::key`.
pub fn code_key(ir: &gravity_ir::Program) -> u128 {
    gravity_backend::cache::key_of(ir).0
}

/// The JIT perf map's note of a lowering (gravity_backend `perfmap::note`), filed only while the map is on: the plan
/// node of each pipeline function (`funcs[i]` runs pipeline `i`, `nodes[i]` its plan node) and the IR's tags (mode
/// `tags`: `jit_tags_on`), under the program's code key.
pub fn jit_note(key: Option<u128>, funcs: &[Func], nodes: &[i32], tags: Option<gravity_ir::JitTags>) {
    let mode = gravity_backend::perfmap::mode();
    let Some(key) = key.filter(|_| mode != gravity_backend::perfmap::OFF) else { return };
    gravity_backend::perfmap::note(key, mode, funcs, nodes, tags);
}

/// Whether this lowering records the JIT map's tags (`gravity_ir::builder::ProgramBuilder::jit_tags`): the perf map's
/// mode `tags` on this thread.
pub fn jit_tags_on() -> bool {
    gravity_backend::perfmap::mode() == gravity_backend::perfmap::TAGS
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Tpde,
    Interp,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct LowerOpts { // what the executor asks of a lowering beyond the graph (ENTRY E1)
    pub instrument: bool, // EXPLAIN ANALYZE: counted per plan node into the program block's counter region; never filed in L2
    pub publish: bool, // D2798: the row the sink's pipeline is on, stored before its quals (a cursor whose user code may say WHERE CURRENT OF it)
    pub shape: Option<u128>, // D3168: the admission's content key, the staged probe's memo key (gravity_program `stage_memo`); None: no memo
}

pub type LowerFn = for<'p> fn(ArenaRef, mcx::Mcx<'_>, &PipelineGraph<'p>, &dyn gravity_decompose::Facts, &dyn Catalog<'p>, &mut dyn WordPlan<'p>, LowerOpts) -> Result<Lowered>;

pub struct LowererHook {
    pub lower: LowerFn,
}

static LOWERER: pgsync::OnceLock<LowererHook> = pgsync::OnceLock::new();

/// The lowerer, registered once by the executor (`exec/src/knobs/lowerer.rs`; it depends on this crate, this crate cannot
/// depend on it). None is registered until lowerv4's M0 (lowerv3 removed 2026-09-23, tags `lowerv3-final`, `lowerv3-landed`).
pub fn set_lowerer(h: LowererHook) {
    let _ = LOWERER.set(h);
}

/// The stub's one named refusal: every graph meets it until a lowerer is registered (`pgrust.gravity_lowerer` `auto`).
pub const NOT_BUILT: &str = "lowerer v4 not built yet; lowerv3 removed (tags lowerv3-final, lowerv3-landed)";

/// The lowerer that serves a graph: the registered one, else the stub's refusal (`NOT_BUILT`); `pgrust.gravity_lowerer`
/// `v1` / `v2` / `v3` are refused at SET (tags `ladder-final`, `lowerv2-final`, `lowerv3-final`).
pub fn lower_graph<'p>(arena: ArenaRef, mcx: mcx::Mcx<'_>, graph: &PipelineGraph<'p>, facts: &dyn gravity_decompose::Facts, catalog: &dyn Catalog<'p>, plan: &mut dyn WordPlan<'p>, opts: LowerOpts) -> Result<Lowered> {
    match LOWERER.get() {
        Some(h) => (h.lower)(arena, mcx, graph, facts, catalog, plan, opts),
        None => Err(Refusal::not_lowered("lowerv4", NOT_BUILT, graph.root().node_id)),
    }
}

pub mod mergearms {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct MergeKey {
        pub col: u16,
        pub desc: bool,
        pub nulls_first: bool,
    }
}

pub use meeting::{lower_composed, Composed, InitDesc};
pub mod admit;
pub mod inputs;
pub mod ops;
pub mod rerun;

/// A graph whose root writes or locks rows (the entry's write gates read this before lowering).
pub fn is_write_graph(graph: &PipelineGraph<'_>) -> bool {
    graph.root().ops.iter().any(|o| matches!(o, gravity_decompose::Op::Write(_) | gravity_decompose::Op::Lock(_)))
}

/// The range table index a pipeline's `Var`s carry: a scan's `scanrelid`, `INDEX_VAR` for the rest.
pub fn varno_of(source: &Source<'_>) -> i32 {
    match source {
        Source::Range { scanrelid, .. } | Source::Index { scanrelid, .. } | Source::Bitmap { scanrelid, .. } | Source::Values { scanrelid, .. } => *scanrelid as i32,
        _ => INDEX_VAR,
    }
}

/// The backend over every pipeline function.
pub fn compile(l: &Lowered, backend: Backend) -> PgResult<Vec<Code>> {
    if let Backend::Tpde = backend {
        return Ok(gravity_backend::mapped_cached_keyed(&l.ir, &l.funcs, l.key.map(gravity_backend::cache::CodeKey), &[])?.0);
    }
    let mut codes = Vec::with_capacity(l.funcs.len()); // alloc: Program
    for &f in l.funcs.iter() {
        codes.push(gravity_backend::interp(l.ir.clone(), f).map_err(|e| Box::new(PgError::error(e)))?); // alloc: Program
    }
    Ok(codes)
}

pub fn assemble(l: &Lowered, codes: Vec<Code>) -> Program {
    let codes = codes.into_iter().enumerate().map(|(i, c)| match l.funcs.get(i).and_then(|f| l.staged.as_slice().iter().find(|s| s.0 == *f)) {
        Some((_, Some(side), tag)) => {
            let interp = c.is_interp();
            c.with_staged(gravity_backend::staged(side, interp, *tag))
        }
        Some((_, None, tag)) => c.with_want(*tag),
        None => c,
    });
    let p = Program::with_declared_pipelines(l.descs.layout, &l.descs.sites, &l.descs.out, l.descs.write.copy_in(*l.descs.sites.allocator()), l.descs.pipelines.iter().copied().zip(codes));
    let p = if l.descs.out_region == 0 { p } else { p.with_out_region(l.descs.out_region) };
    let p = if l.descs.instr.is_empty() { p } else { p.with_instr(&l.descs.instr) };
    let p = p.with_v4(&l.descs.v4).with_used(l.descs.used).as_lowered().with_locks(&l.descs.locks); // alloc: Program
    // the list `with_writes` copies into the program's own arena: built in the lowering's, beside the plans' copies
    let a = *l.descs.sites.allocator();
    let mut writes = ArenaVec::with_capacity_in(l.descs.writes.len(), a);
    for w in l.descs.writes.iter() {
        writes.push(gravity_program::program::WriteAt { site: w.site, plan: w.plan.copy_in(a), end: w.end, node: w.node });
    }
    let p = p.with_writes(&writes).with_write_end(l.descs.write_end);
    // the owner stays with the descriptors: a second `assemble` (the rig's, under the interpreter) shares it
    let owner = l.descs.owner.take();
    let p = match &owner {
        Some((o, sig)) => p.with_owner(o.clone(), sig),
        None => p,
    };
    l.descs.owner.set(owner);
    p
}
