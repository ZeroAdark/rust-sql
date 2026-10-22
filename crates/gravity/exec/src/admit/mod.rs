//! Admission from the graph (docs/gravity/exec.md "Admission from the graph"): the plan decomposed over the
//! relcache, the program cache probed by the graph's content key, the lowerer and the backend on a miss, the words
//! bound by node locators, the host recipes from the program's sites. A typed refusal is the statement's error (`hand_off`).

use std::sync::Arc;
use std::time::Instant;

use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::subplan::{SubForm, SubPlanDesc};
use gravity_decompose::{Am, Op, PipelineGraph, PipelineNode, Source};
use gravity_hosts::{GranuleRecipe, HostRegistry, RangeRecipe, SiteRecipe};
use gravity_hosts::facts::RelcacheFacts;
use gravity_lower::{Located, WordSrc};
use gravity_program::{limits, ArenaRef, ArenaVec, ContentKey, PipeEst, Program};
use mcx::Mcx;
use types_core::Oid;
use types_error::PgResult;
use types_nodes::parsenodes::WCOKind;
use types_nodes::plannodes::PlannedStmt;
use types_nodes::{Node, NodeTag};
use types_portal::ParamListHandle;
use types_storage::lock::NoLock;

use crate::shapes::{self, Admitted, KeyExprs, KeySrc, SHAPE_PARAMS};
use crate::Backend;

pub const SHAPE: &str = "GRAPH";
/// The per-backend admission arena: decompose and lower of one statement run in it and it is reset after each admission
/// (`graph`). Mapped, so only the pages one admission touches are faulted in (program/src/arena.rs). A capacity bound.
/// The graph arena is an address-space reservation (program arena: an anonymous mapping, pages committed where used and
/// released past `arena::KEEP_BYTES` on each reset), sized for the id spaces landing 20 opened: `limits::PIPELINES` arms at
/// 16 KiB each (a write arm over one leaf measured ~16 KiB of graph: 4100 leaves ran out 64 MiB).
/// D3174: wasm32 has no mmap, so its arena is a boxed block committed whole at the level's first admission (1 GiB of linear
/// memory, zeroed, per nested level): there it is `arena::KEEP_BYTES`, the size every ordinary statement's graph fits in.
pub const ARENA_BYTES: usize = if cfg!(target_family = "wasm") {
    gravity_program::arena::KEEP_BYTES
} else {
    gravity_program::limits::PIPELINES * (16 << 10)
};

pub use crate::inputs::knobs::lowerer::{explain_only, instrumented, is_explain_only, is_instrumented, is_nested, is_publishing, mode, nested, publishing, Mode, GUC};
pub use crate::inputs::knobs::seam::{offered, NOT_OFFERED};


/// A site's admission context (lowerv3 design §7.4): the statement, the site, its graph source and plan node.
pub struct AdmitCx<'a> {
    pub pstmt: &'static PlannedStmt<'static>,
    pub mcx: Mcx<'static>,
    pub site: &'a gravity_program::SiteDesc,
    pub source: &'a Source<'static>,
    pub node_id: i32,
    pub attnos: &'a [i16],
    /// The admitting statement's graph (a recipe that records at admission what its bind would read from it: D2754).
    pub graph: &'a PipelineGraph<'static>,
    /// D3295: the program drives this site, and only in pipelines that run on the leader (none `Parallel::Eligible`);
    /// `false` for a site no pipeline drives (a nest's or SubPlan's inner).
    pub leader: bool,
    /// D3670: every pipeline driving this site is a rerun set's or an InitPlan's member (gravity_program `lifetime`), so its
    /// scan counts at each of their runs (driver `Node::count_scan`), never at the bind: PG counts a sequential scan as it
    /// begins one and at every rescan, and none for an inner it never reaches.
    pub run_counted: bool,
    /// D3670: no pipeline drives this site (a nest's or a SubPlan's embedded inner), so each re-open's first fetch seeks it
    /// (driver `Site.next`): its scan counts at each such seek, as PG counts at its scan's begin and every rescan.
    pub probed: bool,
    /// memsig E-prog (C): the recipe set's root (`registry`'s), where a rung's template builds the lists it keeps
    /// (`TplLists::list`): the statement's at birth, the session's once the backend's cache files the set.
    pub lists: &'a TplLists,
}

/// memsig E-prog (C): a handle on a recipe set's root (`RecipeSet::root`) that a rung's template (`RecipeTpl::Rung`)
/// builds its kept lists in, in place, at admission (`list`). A template so built is held only by its set: a filed
/// `RecipeTpl` has no `Clone` (`SiteTpl`'s lists are the root's), every bind reads a rung by `downcast_ref`, and the set
/// moves and is shared only whole (`Arc<RecipeSet>`), so the lists drop with the set's list, before its root.
pub struct TplLists(mcx::RootA);

impl TplLists {
    /// A new list in the set's root with room for exactly `n` (`list`).
    pub(crate) fn list_with<T>(&self, n: usize) -> TplVec<T> {
        let mut v = self.list();
        tpl_room(&mut v, n, true, RECIPE_ROOT);
        v
    }

    /// `x` onto `v`, a list of this root, its room taken first (the root's out-of-memory ERROR, as `tpl_push`).
    pub(crate) fn push<T>(&self, v: &mut TplVec<T>, x: T) {
        tpl_push(v, x, RECIPE_ROOT)
    }
}

pub struct BindCx<'a> {
    pub graph: Option<&'a PipelineGraph<'static>>,
    pub mcx: Mcx<'static>,
    /// D2754: this execution's statement when the main registry binds on a front-key hit (no graph): a recipe that
    /// recorded at admission where its plan expressions sit reads them here. `None` with a graph, and for an InitPlan's
    /// or an Append arm's registry, which binds without either, as before.
    pub front: Option<&'static PlannedStmt<'static>>,
    /// This execution's statement for the main registry (with or without the graph); `None` for an InitPlan's or an Append
    /// arm's registry. A recipe that compiles its plan node's own qual reads it here (an index walk's qual pre-check, ops/scan/sites.rs).
    pub plan: Option<&'static PlannedStmt<'static>>,
    /// memsig E-prog (A): the owner a bind's working memory is charged to, with `Kind::Region`, as exec's `Prepare::owner`:
    /// `gravity_driver::stmt_context` read once by `registry_from` on the backend, so the portal it executes, else its
    /// running statement; `None` outside both. `mcx` is the admissions' root (the session's) and stays as it is.
    /// RULE: nothing charged to this owner is ever stored into a cached product (a filed template, a recipe set, the
    /// registry an admission keeps) or the admissions' root: a bind's scratch dies in its call.
    pub owner: Option<(memledger::Owner, memledger::Kind)>,
}

impl BindCx<'_> {
    /// An empty `ChargedVec` on this bind's owner (`owner`; uncharged when `None`). Its line's tag:
    /// `// alloc: Exec charged cx.owner`. See `owner`'s RULE.
    pub fn charged_vec<T>(&self) -> memledger::ChargedVec<T> {
        match self.owner {
            Some((o, k)) => memledger::ChargedVec::new(Some(o), k), // alloc: Exec charged o
            None => memledger::ChargedVec::none(),
        }
    }
}

/// The site's recipe when the rung owns its source (`None`: another rung's), or its refusal.
pub type AdmitResult = PgResult<Result<Option<AdmitTpl>, Refused>>;
pub type Admit = fn(&AdmitCx<'_>) -> AdmitResult;
/// `true` when the rung bound the recipe into this execution's registry.
pub type BindResult = PgResult<Result<bool, Refused>>;
pub type Bind = fn(&RecipeTpl, &mut HostRegistry, &BindCx<'_>, &mut KeyExprs) -> BindResult;

/// Each rung's source admission and recipe bind tables (§7.4): empty since lowerv3 was removed (its R1 / R1b / R7 arms are
/// `exec/src/admit/v3_r{1,1b,7}.rs` at tag `lowerv3-final`); lowerv4 appends one `(ADMIT, BIND)` line per rung.
const RUNGS: &[(&[Admit], &[Bind])] = crate::ops::RUNGS;
pub mod entry;
pub mod explain;
mod gates;
pub mod kept;
pub mod lean;
pub mod memo;
pub mod plan;
mod raw;
pub mod reentry;
mod site_tpl;
pub use site_tpl::{GranuleTpl, SiteTpl};
pub mod regen;
pub mod removed;

/// The one hand-off after an admission refused: the statement's error (0A000, the typed reason verbatim). An EXPLAIN
/// without ANALYZE runs nothing, so its refusal is a verdict line, never the statement's error (entry explain.rs).
pub fn hand_off(why: &str) -> PgResult<()> {
    if is_explain_only() {
        Ok(())
    } else {
        Err(crate::runner::refused(why))
    }
}

/// Why the graph path did not admit: the stage that refused and its reason.
#[derive(Clone, Debug)]
pub struct Refused {
    pub stage: &'static str,
    pub what: String,
}

impl core::fmt::Display for Refused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "graph: {}: {}", self.stage, self.what)
    }
}

pub(crate) fn refuse<T>(stage: &'static str, what: impl Into<String>) -> Result<T, Refused> {
    Err(Refused { stage, what: what.into() })
}

/// A word of a cached program, by the position of its node in the key's traversal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordTpl {
    Const(u32),
    /// Element `k` of the array literal at plan node `o` (an `IN` list's member).
    Elem(u32, u32),
    Extern(i32),
    Expr(u32),
    /// D899: an index key's expression at plan node `o` bound when its pipeline starts (`gravity_lower::WordSrc::Late`).
    Late(u32),
    /// D422: the `CurrentOfExpr` at plan node `o`: its address in this execution's plan (the cursor's name).
    Cursor(u32),
    /// V4R-122: the element set of the hashed `= ANY` constant array at plan node `o` for the program's owned object `d`,
    /// built once per execution here (lowerv4 expr.saop's `ExprExtMech::bind_set`), charged to the grant pool: its address.
    Set(u32, u32),
    Zero,
    /// D2834: the execution's parameter list, the word every deferred extern parameter's fetch reads (`gravity_lower::WordSrc::Deferred`).
    Deferred,
    /// A NULL literal's word: NULL in every execution its content key matches (lower key.rs keys `constisnull`), so it is
    /// bound without reading a node; a NULL decompose synthesized (demand/readers.rs `nulled`) binds alike and its front-key
    /// position stays a D644 stand-in (front.rs `synthesized`).
    Null,
}

/// A word bound for one admission: a literal's datum, an extern parameter (0-based), a key expression.
#[derive(Clone, Copy, Debug)]
pub enum GraphWord {
    Const { value: u64, null: u8 },
    Extern(usize),
    Expr(usize),
    /// D899: key expression `expr` (as `Expr`) that program pipeline `pipeline` alone reads, at its driving site: the
    /// runner binds it when that pipeline starts (`runner::plan` `late_words`), PostgreSQL's runtime key at its scan's
    /// first pull; `eval` leaves its word NULL until then.
    Late { expr: usize, pipeline: u16 },
    Zero,
    /// D2834: this execution's parameter list (its handle), which the program's deferred extern parameters are fetched
    /// from where they are evaluated (gravity_expr emit/host.rs `param_deferred`).
    Deferred,
}

pub struct GraphWords {
    pub words: Box<[GraphWord]>,
    pub key: ContentKey,
    /// The InitPlan programs of a composed statement (rung 12), run before the main program (`runner::run_inits`).
    pub inits: Vec<InitRun>,
    /// An Append's remaining arm programs (rung 12 need 3; the main is arm 1), run after it in order into the same receiver.
    pub arms: Vec<ArmRun>,
    /// Exec pruning steps over the InitPlans' words: the arms decided again after `run_inits` (`prune::exec_skip`).
    pub prune: Option<crate::ops::append::prune::ExecPrune>,
    /// The template the admission was bound from (`bind`; `None` for one bound with none): its MergeAppend keys and its
    /// estimate guards are read from it, shared with every admission of it, never copied (`merge`, `rels`).
    pub tpl: Option<Arc<GraphTpl>>,
    /// V4R-122: the hashed `= ANY` sets this execution's `Set` words point into (grant-pool regions, released with it):
    /// the Bind's context owner's, the session's once the plan memo files the admission (D4844, `adopt_sets`).
    pub sets: Vec<gravity_driver::RegionBuf>,
    /// D761: the template's walk of the relations ExecutorStart opens (`runner::held`), taken per run.
    pub held: Arc<crate::runner::held::HeldPlan>,
    /// W8-EQUIVCLASS-SORTOP: the template's `no_proc`: the plan memo never files this admission (`Backend::remember_served`).
    pub no_proc: bool,
    /// KEY-PREDICTED-ROWS road (b): this statement's planner estimates of the main program's pipelines (`predicted_rows`,
    /// `build_rows`, D4047 `end_rows`) by program pipeline, where they differ from the ones the program was lowered with (`estimates`);
    /// `None`: those. The execution's readers take them (`bound`, `bound_all`) while that program (its `Arc` address, the
    /// first field) is the admission's: an Append arm swapped in for its run (runner drive.rs `ArmSwap`) reads its own.
    pub est: Option<(usize, Box<[PipeEst]>)>,
    /// The program was admitted for a partial reader (a named cursor's FETCH: decompose `PipelineGraph::partial`, which its
    /// content key holds); the plan memo hands it only to a start of the same kind (entry keyed.rs `take`).
    pub partial: bool,
    /// PS1-21 / PS1-26 step 1b: the template's guards decide on a relation's pages (lower key.rs `rel_decide`): the plan
    /// memo serves this admission again only where they decide alike now (`rels`, `rels_hold`, `Backend::serve_stale`).
    pub rels_decide: bool,
    /// D3005: the extern parameters a LIMIT / OFFSET count took as this execution's constant (decompose
    /// `admit::limit_externs`), `(paramid, value)`: the plan memo serves this admission again only to an execution that
    /// binds the same values (`folded_hold`).
    pub folded: Box<[(i32, i64)]>,
    /// GENP-1 (D3004): a generic plan's scans re-estimated for the admission's parameter values (`genp`): the memo serves
    /// this admission again only where an execution's re-estimates fall in the same octaves (`folded_hold`).
    pub genp: crate::inputs::genp::Genp,
    /// D3177 (the main merge's audit): the domain constraint sets the admission read, which its words may point into (a
    /// by-reference CHECK constant): kept while it lives; the memo serves it again only while each is current
    /// (`Backend::serve_stale`).
    pub domains: gravity_expr::catalog::DomainPins,
    /// D2834: the extern parameters the admission's execution left to evaluation time (`deferred_ids`), which its program
    /// fetches where it evaluates them: the memo serves it again only to an execution whose deferred ones are among them
    /// (`folded_hold`; such a fetch serves a slot that has its value too).
    pub deferred: Box<[i32]>,
    /// C3 S4c: what the admission read from the catalog, its decompose's reads and its programs' (a hit replays its
    /// template's, `bind`; a front hit its variant's): the plan memo files it and serves it again only while they stand
    /// (`deps_hold`).
    pub reads: gravity_deps::Held,
}

impl GraphWords {
    /// A MergeAppend's keys (need 14), its template's: `drive` binds `armmerge::ArmMerge` as every program's sink and drains
    /// after the last arm.
    pub fn merge(&self) -> &[gravity_lower::mergearms::MergeKey] {
        self.tpl.as_deref().map_or(&[], |t| &t.merge[..])
    }

    /// The template's estimate guards when one decides on a relation's pages (`rels_decide`), else none.
    pub fn rels(&self) -> &[gravity_lower::inputs::key::EstGuard] {
        match self.tpl.as_deref() {
            Some(t) if self.rels_decide => &t.guards[..],
            _ => &[],
        }
    }

    /// D4844: the bound sets (`sets`) become the session's (the plan memo holds them past the portal whose Bind built them).
    /// Off a backend nothing moves.
    pub(crate) fn adopt_sets(&mut self) {
        let acct = memledger::current();
        if !acct.is_backend() {
            return;
        }
        for s in self.sets.iter_mut() {
            s.adopt(memledger::Owner::Session(acct));
        }
    }

    /// C3 S4c: every catalog read of the admission still reads the same (gravity_deps `Held::stands`).
    pub fn deps_hold(&self) -> bool {
        self.reads.stands()
    }

    /// D899: whether this statement's late words wait for their pipelines (the runner's `late_words`): a program with no
    /// InitPlan program and no Append arm, whose programs would read the same words (they bind at bind then, as `Expr`).
    pub fn late_ok(&self) -> bool {
        self.inits.is_empty() && self.arms.is_empty()
    }
}

/// One Append arm bound for an admission (the same out columns as the main program's); `ArmTpl` its cached form under the key.
pub struct ArmRun {
    pub program: Arc<Program>,
    pub registry: HostRegistry,
    /// Pruned for this execution (initial partition pruning at bind, `prune::initial_valid`): the arm does not run.
    pub skip: bool,
}

pub struct ArmTpl {
    pub recipes: Arc<RecipeSet>,
    pub program: Arc<Program>,
}

/// One InitPlan program bound for an admission: its cell goes into word `word` (true / a row's first column).
pub struct InitRun {
    pub program: Arc<Program>,
    pub registry: HostRegistry,
    pub word: usize,
    pub exists: bool,
    pub param: i32,
}

/// A cached InitPlan program: `program` and its recipes under the statement's key, with the word it fills.
pub struct InitTpl {
    pub recipes: Arc<RecipeSet>,
    pub program: Arc<Program>,
    pub word: u32,
    pub exists: bool,
    pub param: i32,
}

/// A program's template under the key: its words by node locator, and the functions its plan's
/// expressions call, rechecked for EXECUTE on every execution as C's `ExecInitExpr` checks them (lower.md §14.13 item 4),
/// each with the object its denial names.
/// planprod P6-b: its lists live in its own root (`root`, 'GravityGraphTpl', the last field, so they drop before it): owned
/// at birth by the lowering statement, by the session when the backend's cache files it (`adopt`, from `adopt_hosted`),
/// by the program caches once the process-wide tier holds it (`publish`), and never moved after that.
pub struct GraphTpl {
    pub words: TplVec<WordTpl>,
    pub funcs: TplVec<(Oid, shapes::AclObject)>,
    pub inits: TplVec<InitTpl>,
    pub arms: TplVec<ArmTpl>,
    /// A composed Append with initial pruning steps: its `partPruneInfos` index (the arms decided per execution).
    pub prune: Option<u32>,
    pub exec_prune: bool,
    pub merge: TplVec<gravity_lower::mergearms::MergeKey>,
    /// The bytes of the check options' names (`CheckTpl::relname` / `polname`, shared with every admission's
    /// `CheckSpec`): a door-1 charge of the root's owner, moved with the root (`adopt`, `publish`). Declared before
    /// `checks`, so its drop gives the bytes to the dropping thread before the names' free lands there.
    names: pgsync::Mutex<memledger::charge::Charge>,
    /// D4580 (`own_bytes`, from the publish): declared before `held` and `deps`, so it drops before their frees.
    own: pgsync::Mutex<memledger::charge::Charge>,
    /// The write's check options (members CheckRls / CheckView): each qual by its traversal index, rebound per admission.
    pub checks: TplVec<CheckTpl>,
    /// D761: the relations the plan's ExecutorStart opens, walked once here (`runner::held::plan`).
    pub held: Arc<crate::runner::held::HeldPlan>,
    /// W8-EQUIVCLASS-SORTOP: the program raises PG's "missing support function" for a Sort whose btree family has no order
    /// proc (decompose `sort::holds_no_proc`). PG looks the proc up at every execution and its plancache does not follow
    /// pg_amproc, so no admission of it is filed in the plan memo: each execution admits again (the front key follows it).
    pub no_proc: bool,
    /// KEY-PREDICTED-ROWS road (b): the lowering's decisions on planner estimates (lower inputs/key.rs `decide`); the
    /// content key holds no estimate, so a hit serves only a graph on whose estimates every guard decides the same.
    pub guards: TplVec<gravity_lower::inputs::key::EstGuard>,
    /// C3: what the lowering read from the catalog, each read's generation then (`gravity_deps`).
    pub deps: gravity_deps::Deps,
    /// D4579: the database it was lowered in (its relations' and its catalog's): the tier lets the program go when that
    /// database is dropped (`forget_database`), and asks `deps` against that database's generations (`sweep_superseded`).
    pub db: Oid,
    /// The program caches took the root's bytes (`publish`): no owner moves them after.
    published: core::sync::atomic::AtomicBool,
    root: pgsync::Mutex<mcx::SendRoot>,
}

/// planprod P6-b: a list of a template (`GraphTpl`), in its root: grown only while `install` builds it.
pub type TplVec<T> = allocator_api2::vec::Vec<T, mcx::RootA>;

/// The roots template lists live in, by name (an out-of-memory error names its context).
const GRAPH_ROOT: &str = "GravityGraphTpl";
const RECIPE_ROOT: &str = "GravityHostRecipes";

/// Room for `n` more elements of template list `v` (exactly `n` when `exact`), or the root's out-of-memory ERROR:
/// allocator-api2's own growth (`push`, `reserve`, `extend_from_slice`) aborts on a failed block.
fn tpl_room<T>(v: &mut TplVec<T>, n: usize, exact: bool, root: &'static str) {
    if v.capacity() - v.len() >= n {
        return;
    }
    let r = if exact { v.try_reserve_exact(n) } else { v.try_reserve(n) };
    if r.is_err() {
        mcx::raise(mcx::oom_boxed_named(root, n.saturating_mul(core::mem::size_of::<T>())));
    }
}

/// `v.push(x)` with its room taken first (`tpl_room`).
fn tpl_push<T>(v: &mut TplVec<T>, x: T, root: &'static str) {
    tpl_room(v, 1, false, root);
    v.push(x);
}

/// planprod P6-c1: a program's recipe templates (`registry`), a list in a root of its own: the statement's until the
/// backend's cache files it (`adopt_hosted`), then the session's. Never the process-wide tier's (a recipe holds this
/// backend's scan keys and function-manager frames). The list drops before the root (field order).
pub struct RecipeSet {
    list: TplVec<RecipeTpl>,
    root: core::cell::RefCell<mcx::SendRoot>,
    /// The shared block that holds the set (`SHARED_BLOCK` bytes, beside the root's): charged to the root's owner, moved
    /// with it (`adopt`), off it at the drop.
    block: core::cell::RefCell<memledger::charge::Charge>,
}

/// The bytes of a `RecipeSet`'s shared block: its two counts and the set.
const SHARED_BLOCK: u64 = (2 * core::mem::size_of::<usize>() + core::mem::size_of::<RecipeSet>()) as u64;

impl core::ops::Deref for RecipeSet {
    type Target = [RecipeTpl];

    fn deref(&self) -> &[RecipeTpl] {
        &self.list
    }
}

impl RecipeSet {
    /// The list in its root, shared: the shared block is charged to `o`, the root's owner.
    fn shared(list: TplVec<RecipeTpl>, root: mcx::SendRoot, o: memledger::Owner) -> Arc<RecipeSet> {
        use memledger::charge::{release_of, Charge, Door};
        let block = core::cell::RefCell::new(Charge::open(o, release_of(o), memledger::Kind::Region, Door::Heap, 0));
        let set = Arc::new(RecipeSet { list, root: core::cell::RefCell::new(root), block }); // alloc: Program charged o
        // the block the allocator just counted on this thread moves to the owner
        set.block.borrow_mut().commit(SHARED_BLOCK);
        set
    }

    fn adopt(&self, o: memledger::Owner) {
        self.root.borrow_mut().set_owner(o);
        self.block.borrow_mut().transfer(o, memledger::charge::release_of(o));
    }

    /// The bytes its root holds now (the witnesses').
    #[cfg(test)]
    fn root_bytes(&self) -> usize {
        self.root.borrow().allocated()
    }
}

/// The program caches' memory account (gravity_backend `cache::PROGRAM_CACHES`).
const PROGRAM_CACHES: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::ProgramCaches));

impl GraphTpl {
    /// The backend's cache filed it (`adopt_hosted`): the root is `o`'s, unless the program caches hold it.
    fn adopt(&self, o: memledger::Owner) {
        if !self.published.load(core::sync::atomic::Ordering::Acquire) {
            pgsync::lock(&self.root).set_owner(o);
            pgsync::lock(&self.names).transfer(o, memledger::charge::release_of(o));
        }
    }

    /// The process-wide tier holds it: the root is the program caches' from now until its last drop, once.
    fn publish(self: &Arc<Self>) {
        self.publish_to(PROGRAM_CACHES);
    }

    fn publish_to(self: &Arc<Self>, caches: memledger::Owner) {
        use memledger::charge::{release_of, Charge, Door};
        if !self.published.swap(true, core::sync::atomic::Ordering::AcqRel) {
            pgsync::lock(&self.root).set_owner(caches);
            pgsync::lock(&self.names).transfer(caches, release_of(caches));
            *pgsync::lock(&self.own) = Charge::open(caches, release_of(caches), memledger::Kind::Region, Door::Heap, self.own_bytes());
        }
    }

    fn own_bytes(&self) -> u64 {
        memledger::arc_bytes::<GraphTpl>() + mcx::SendRoot::HEADER_BYTES + memledger::arc_bytes::<crate::runner::held::HeldPlan>() + self.held.heap_bytes() + self.deps.heap_bytes()
    }

    /// The bytes its root holds now (the witnesses').
    #[cfg(test)]
    fn root_bytes(&self) -> usize {
        pgsync::lock(&self.root).allocated()
    }
}

pub struct CheckTpl {
    pub kind: WCOKind,
    pub relname: Arc<str>,
    pub polname: Option<Arc<str>>,
    pub node: u32,
    pub target: u16,
    /// D1687: the write whose list holds it, by its relation (a route's root) and whether it is a writing CTE's
    /// (exec ops/modifytable/beside.rs gives a writer beside the program's `write` its own list).
    pub rel: u32,
    pub cte: bool,
}

/// The write op's check options located in the key's traversal (`key.rs` visits each qual once, in desc order).
/// With the bytes of the names it made (`shared_str_bytes` each), for the template's `names` charge.
fn check_template(graph: &PipelineGraph<'_>, nodes: &[Node<'_>], mut out: TplVec<CheckTpl>) -> Result<(TplVec<CheckTpl>, u64), Refused> {
    let mut names = 0u64;
    for (k, d) in graph.pipelines.iter().enumerate().flat_map(|(k, p)| p.ops.iter().filter_map(move |op| if let Op::Write(d) = op { Some((k, d)) } else { None })) {
        for c in d.checks.iter() {
            let Some(i) = nodes.iter().position(|k| k.ptr_eq(c.qual)) else { return refuse("words", "a check option qual outside the key's traversal") };
            // several result relations: every arm's write carries the same lists, each qual once
            if out.iter().any(|t: &CheckTpl| t.node == i as u32) {
                continue;
            }
            let (relname, polname) = (c.relname.unwrap_or(""), c.polname);
            names += shared_str_bytes(relname) + polname.map_or(0, shared_str_bytes);
            let (relname, polname): (Arc<str>, Option<Arc<str>>) = (Arc::from(relname), polname.map(Arc::from)); // alloc: Program charged the template root's owner (`names`)
            tpl_push(&mut out, CheckTpl { kind: c.kind, relname, polname, node: i as u32, target: c.target, rel: crate::ops::modifytable::beside::desc_rel(d), cte: crate::ops::modifytable::beside::cte_write(graph, k) }, GRAPH_ROOT);
        }
    }
    Ok((out, names))
}

/// The bytes of an `Arc<str>` of `s`: its two counts and the text, rounded to the counts' alignment.
fn shared_str_bytes(s: &str) -> u64 {
    (2 * core::mem::size_of::<usize>() + s.len()).next_multiple_of(core::mem::align_of::<usize>()) as u64
}

/// This admission's check quals from the template and this plan's nodes (hit or miss: the host binds the current plan's).
fn bind_checks(tpl: &[CheckTpl], nodes: &[Node<'static>], inits: std::rc::Rc<[gravity_hosts::ops::modifytable::write_check::InitWord]>) -> Result<Vec<gravity_hosts::ops::modifytable::write_check::CheckSpec>, Refused> {
    let mut out = Vec::with_capacity(tpl.len()); // alloc: Program
    for c in tpl {
        let Some(n) = nodes.get(c.node as usize) else { return refuse("words", "a check option locator outside the plan") };
        out.push(gravity_hosts::ops::modifytable::write_check::CheckSpec { kind: c.kind, relname: Arc::clone(&c.relname), polname: c.polname.clone(), node: *n, target: c.target, inits: std::rc::Rc::clone(&inits) }); // alloc: Program
    }
    Ok(out)
}

/// M8-WRITE W2: the program's InitPlans for the checks' lazy params: an Init proto's `arg` = its pair count, its first
/// exec param in the high half (lowerv4 nest/initplan.rs; exec v4/m8_init.rs `lay`), its cell in word `words.base`.
fn check_inits<A: gravity_program::Allocator + Clone>(v4: Option<&gravity_program::protos::V4Descs<A>>) -> std::rc::Rc<[gravity_hosts::ops::modifytable::write_check::InitWord]> {
    let protos = v4.map_or(&[][..], |d| &d.protos[..]);
    let init = |p: &gravity_program::protos::Proto| gravity_hosts::ops::modifytable::write_check::InitWord { first: gravity_kernels::ops::shared::object::entries::init_first(p.arg), n: gravity_kernels::ops::shared::object::entries::init_pairs(p.arg), word: p.words.base };
    protos.iter().filter(|p| p.kind == gravity_program::protos::ProtoKind::Init).map(init).collect() // alloc: Program
}

/// The functions C gates when it initializes the plan's expressions — an operator's or a function's proc, a
/// scalar-array operator's proc and hash proc, an aggregate, a window function (`execExpr.c`, `nodeAgg.c`,
/// `nodeWindowAgg.c`) — from the key's traversal (every expression of the plan) plus the index clauses' operators (the
/// traversal holds a key's value, not its operator); not what the lowering bound, since it open-codes probes, filters and aggregates.
/// An Aggref's aggfnoid is nodeAgg.c's OBJECT_AGGREGATE gate, every other one (a window function's too) OBJECT_FUNCTION;
/// an aggregate gated both ways is named by its gate C initializes first (`first_gates`), not by the pipelines' order.
/// Where C gates outside an expression's own init, and the expressions it compiles elsewhere or never: `gates.rs`.
fn program_funcs(graph: &PipelineGraph<'_>, nodes: &[Node<'_>], pstmt: &PlannedStmt<'_>, out: TplVec<(Oid, shapes::AclObject)>) -> PgResult<TplVec<(Oid, shapes::AclObject)>> {
    let mut funcs = Vec::new(); // alloc: Program
    fn keys(ks: &[gravity_decompose::IndexKey<'_>], funcs: &mut Vec<Oid>) -> PgResult<()> {
        // D2785: C gates an index clause through the indexqualorig / indexorderbyorig expression; a null test has no
        // operator, and ExecInitExprRec compiles a row compare through its btree procs with no gate
        let gated = ks.iter().filter(|k| !matches!(k.form, gravity_decompose::graph::KeyForm::Null | gravity_decompose::graph::KeyForm::NotNull | gravity_decompose::graph::KeyForm::Row));
        for k in gated {
            funcs.push(lsyscache::get_opcode(k.opno)?); // alloc: Program
        }
        Ok(())
    }
    fn bitmap(p: &gravity_decompose::ops::shared::scan::BitmapPlan<'_>, funcs: &mut Vec<Oid>) -> PgResult<()> {
        stack_depth_core::check_stack_depth()?;
        match p {
            gravity_decompose::ops::shared::scan::BitmapPlan::Index { keys: ks, .. } => keys(ks, funcs),
            gravity_decompose::ops::shared::scan::BitmapPlan::And { plans, .. } | gravity_decompose::ops::shared::scan::BitmapPlan::Or { plans, .. } => plans.iter().try_for_each(|q| bitmap(q, funcs)),
        }
    }
    fn sources(p: &PipelineNode<'_>, funcs: &mut Vec<Oid>) -> PgResult<()> {
        stack_depth_core::check_stack_depth()?;
        match &p.source {
            Source::Index { keys: ks, .. } => keys(ks, funcs)?,
            Source::Bitmap { plan, .. } => bitmap(plan, funcs)?,
            _ => {}
        }
        for op in p.ops.iter() {
            if let Op::Join(JoinDesc::Nest { inner, .. }) | Op::SubPlan(SubPlanDesc { form: SubForm::Nested { inner }, .. }) = op {
                sources(inner, funcs)?;
            }
        }
        Ok(())
    }
    for p in graph.pipelines.iter() {
        sources(p, &mut funcs)?;
    }
    let (mut aggs, mut wins, mut node_ops) = (Vec::new(), Vec::new(), Vec::new()); // alloc: Program
    let sites = gates::Sites::of(graph);
    // D2915: only a gate-carrying node is asked where its gate runs (a streamed VALUES list's cells are most of a long plan)
    for n in nodes.iter().filter(|n| gates::gated(n.node_tag())) {
        let to = match sites.at(*n) {
            gates::At::Start => &mut funcs,
            // D2775: a VALUES row's call is gated by the program at the row's visit (lowerv4 ops/values `gate_row`)
            gates::At::Visit | gates::At::Never => continue,
        };
        match n.node_tag() {
            NodeTag::T_OpExpr => to.push(n.as_op_expr().unwrap().opfuncid),
            NodeTag::T_DistinctExpr => to.push(n.as_distinct_expr().unwrap().opfuncid),
            NodeTag::T_NullIfExpr => to.push(n.as_null_if_expr().unwrap().opfuncid),
            NodeTag::T_FuncExpr => to.push(n.as_func_expr().unwrap().funcid),
            NodeTag::T_ScalarArrayOpExpr => {
                let s = n.as_scalar_array_op_expr().unwrap();
                to.push(s.opfuncid);
                if s.hashfuncid != 0 {
                    to.push(s.hashfuncid);
                }
            }
            NodeTag::T_Aggref => {
                aggs.push(n.as_aggref().unwrap().aggfnoid); // alloc: Program
                gravity_decompose::admit::distinct_eq_ops(*n, &mut node_ops);
            }
            NodeTag::T_WindowFunc => wins.push(n.as_window_func().unwrap().winfnoid), // alloc: Program
            _ => {}
        }
    }
    gravity_decompose::admit::node_gate_ops(pstmt, &mut node_ops);
    gates::op_gates(&node_ops, &mut funcs)?;
    let both: Vec<Oid> = aggs.iter().copied().filter(|f| wins.contains(f)).collect(); // alloc: Program
    let first = if both.is_empty() { Vec::new() } else { first_gates(pstmt.subplans.iter().flatten().chain(pstmt.planTree), &both) }; // alloc: Program
    funcs.extend_from_slice(&wins); // alloc: Program
    Ok(gates(&funcs, &aggs, &first, out))
}

/// For each oid of `both`, the object of its first gate in C's init order: InitPlan initializes each subplan in list
/// order, then the plan tree (execMain.c); ExecInitNode a node's children (outer before inner, arms in order) before its
/// own gates, ExecInitAgg's on each Aggref and ExecInitWindowAgg's on each window function.
fn first_gates<'p>(trees: impl Iterator<Item = Node<'p>>, both: &[Oid]) -> Vec<(Oid, shapes::AclObject)> {
    fn node(n: Node<'_>, both: &[Oid], first: &mut Vec<(Oid, shapes::AclObject)>) {
        stack_depth_core::check_stack_depth_or_panic();
        let Some(p) = n.as_plan() else { return };
        let arms = n.as_append().map(|a| &a.appendplans).or(n.as_merge_append().map(|a| &a.mergeplans)).or(n.as_bitmap_and().map(|a| &a.bitmapplans)).or(n.as_bitmap_or().map(|a| &a.bitmapplans));
        for c in [p.lefttree, p.righttree].into_iter().flatten().chain(arms.into_iter().flat_map(|l| l.iter())).chain(n.as_subquery_scan().and_then(|s| s.subplan)) {
            node(c, both, first);
        }
        let mut gate = |f: Oid, object| {
            if both.contains(&f) && !first.iter().any(|g| g.0 == f) {
                first.push((f, object)); // alloc: Program
            }
        };
        for e in p.targetlist.iter().chain(p.qual.iter()) {
            if n.as_agg().is_some() {
                gravity_decompose::expr::run_aggrefs(e, &mut |a| gate(a.as_aggref().unwrap().aggfnoid, shapes::AclObject::Aggregate));
            } else if n.as_window_agg().is_some() {
                gravity_decompose::expr::walk(e, &mut |w| match w.as_window_func() {
                    Some(w) => {
                        gate(w.winfnoid, shapes::AclObject::Function);
                        false
                    }
                    None => true,
                });
            }
        }
    }
    let mut first = Vec::new(); // alloc: Program
    for t in trees {
        node(t, both, &mut first);
    }
    first
}

/// `funcs` (OBJECT_FUNCTION gates) and `aggs` (OBJECT_AGGREGATE gates) sorted by oid, once each per object; of an oid
/// gated both ways, the gate `first` names leads (neither named: the function's).
fn gates(funcs: &[Oid], aggs: &[Oid], first: &[(Oid, shapes::AclObject)], mut all: TplVec<(Oid, shapes::AclObject)>) -> TplVec<(Oid, shapes::AclObject)> {
    tpl_room(&mut all, funcs.len() + aggs.len(), false, GRAPH_ROOT);
    for g in funcs.iter().map(|&f| (f, shapes::AclObject::Function)).chain(aggs.iter().map(|&f| (f, shapes::AclObject::Aggregate))) {
        tpl_push(&mut all, g, GRAPH_ROOT);
    }
    all.sort_unstable_by_key(|&g| (g.0, !first.contains(&g), g.1 == shapes::AclObject::Aggregate));
    all.dedup();
    all
}

/// D3585: the occurrences of `n` in the plan's nodes, in traversal order: its one place in `one` (D4449: each node's place
/// by address, where no address repeats), else from `by_addr` (every node's (address, place), sorted).
fn occurrences<'a>(one: Option<&'a gravity_lower::inputs::key::PosTable>, by_addr: &'a [(usize, u32)], n: Node<'_>) -> impl Iterator<Item = usize> + 'a {
    let a = n.as_raw().as_ptr() as usize;
    let at = one.and_then(|t| t.get(a as u64)).map(|i| i as usize);
    at.into_iter().chain(by_addr[by_addr.partition_point(|x| x.0 < a)..].iter().take_while(move |x| x.0 == a).map(|x| x.1 as usize))
}

/// D4449: the plan's nodes' places by address, where no address repeats (`occurrences`' `one`).
fn places(nodes: &[Node<'_>]) -> Option<gravity_lower::inputs::key::PosTable> {
    let mut t = gravity_lower::inputs::key::PosTable::with_capacity(nodes.len()); // alloc: Program
    nodes.iter().enumerate().all(|(i, k)| t.put(k.as_raw().as_ptr() as usize as u64, i as u32).is_none()).then_some(t)
}

fn template(plan: &Located<'_>, nodes: &[Node<'_>], mut out: TplVec<WordTpl>) -> Result<TplVec<WordTpl>, Refused> {
    tpl_room(&mut out, plan.seq.words.len(), false, GRAPH_ROOT);
    // D3585: each word finds its node by address (a linear search per word was quadratic in the plan's literals: an
    // Append over 4000 partitions brings each arm's). D4449: by a table where no address repeats, else by the sorted list;
    // a plan of no word and no follower reads neither (a literal table's 100k key-walk nodes were sorted for no word)
    let locates = !(plan.seq.words.is_empty() && plan.followers.is_empty());
    let one = if locates { places(nodes) } else { None };
    let mut by_addr: Vec<(usize, u32)> = if !locates || one.is_some() { Default::default() } else { nodes.iter().enumerate().map(|(i, k)| (k.as_raw().as_ptr() as usize, i as u32)).collect() }; // alloc: Program
    by_addr.sort_unstable();
    let first = |n: Node<'_>| occurrences(one.as_ref(), &by_addr, n).next().map(|i| i as u32);
    // a plan may hold one node at two places (two equal index keys share their constant); each word takes the next
    // unclaimed occurrence in traversal order, so a hit's plan binds each occurrence's own value
    let mut taken = vec![false; if locates { nodes.len() } else { 0 }]; // alloc: Program
    let mut locate = |n: Node<'_>| -> Option<u32> {
        let i = occurrences(one.as_ref(), &by_addr, n).find(|&i| !taken[i])?;
        taken[i] = true;
        Some(i as u32)
    };
    for (w, at) in plan.seq.words.iter().zip(plan.nodes.iter()) {
        let t = match (w, at) {
            (WordSrc::Const { .. }, Some(n)) if n.as_const().is_some_and(|c| c.constisnull) => WordTpl::Null,
            (WordSrc::Const { .. }, Some(n)) => WordTpl::Const(locate(*n).ok_or_else(|| Refused { stage: "words", what: "a literal outside the key's traversal".into() })?),
            (WordSrc::Const { .. }, None) => return refuse("words", "a literal word without a node"),
            (WordSrc::Elem { index, .. }, Some(n)) => WordTpl::Elem(first(*n).ok_or_else(|| Refused { stage: "words", what: "an array literal outside the key's traversal".into() })?, *index),
            (WordSrc::Elem { .. }, None) => return refuse("words", "an element word without its array literal"),
            (WordSrc::Extern { id, .. }, _) => WordTpl::Extern(*id),
            (WordSrc::Expr(n), _) => WordTpl::Expr(locate(*n).ok_or_else(|| Refused { stage: "words", what: "a key expression outside the key's traversal".into() })?),
            (WordSrc::Late(n), _) => WordTpl::Late(locate(*n).ok_or_else(|| Refused { stage: "words", what: "a key expression outside the key's traversal".into() })?),
            (WordSrc::Cursor(n), _) => WordTpl::Cursor(locate(*n).ok_or_else(|| Refused { stage: "words", what: "a CURRENT OF outside the key's traversal".into() })?),
            (WordSrc::Set { node, desc }, _) => WordTpl::Set(first(*node).ok_or_else(|| Refused { stage: "words", what: "a hashed array outside the key's traversal".into() })?, *desc),
            (WordSrc::Kernel | WordSrc::Exec { .. }, _) => WordTpl::Zero,
            (WordSrc::Deferred { .. }, _) => WordTpl::Deferred,
        };
        tpl_push(&mut out, t, GRAPH_ROOT);
    }
    // D3642: a literal reading another copy's word is bound through that copy, so the keys must hold it (key.rs `alias`)
    if plan.followers.iter().any(|f| occurrences(one.as_ref(), &by_addr, *f).next().is_none()) {
        return refuse("words", "a literal's copy outside the key's traversal");
    }
    Ok(out)
}

/// The words of this admission from the template and this plan's nodes; key expressions compiled once (Program lifetime) over the statement's parameter image.
fn bind_words(tpl: &[WordTpl], nodes: &[Node<'static>], mcx: Mcx<'static>, exprs: &mut KeyExprs, key: ContentKey, program: &Program, owner: Option<&dyn core::any::Any>) -> PgResult<Result<(GraphWords, bool), Refused>> {
    let mut words = Vec::with_capacity(tpl.len()); // alloc: Program
    let (mut volatile, mut sets) = (false, Vec::new()); // alloc: Program
    for (w, t) in tpl.iter().enumerate() {
        words.push(match *t {
            WordTpl::Const(o) => {
                let Some(c) = nodes.get(o as usize).and_then(|n| n.as_const()) else { return Ok(refuse("words", "a literal locator that is not a constant")) };
                // D858: the literal's word in its type's convention (a folded relabel keeps its argument's)
                let d = c.constvalue;
                let value = if c.constisnull { 0 } else if c.constbyval { gravity_expr::nodes::konst::word_canon(c.consttype, d).as_u64() } else { d.as_u64() };
                GraphWord::Const { value, null: c.constisnull as u8 }
            }
            WordTpl::Elem(o, k) => {
                let Some(c) = nodes.get(o as usize).and_then(|n| n.as_const()) else { return Ok(refuse("words", "an element locator that is not an array literal")) };
                if c.constisnull {
                    GraphWord::Const { value: 0, null: 1 }
                } else {
                    let Ok(ef) = gravity_expr::nodes::array::elem_facts(&gravity_expr::PgCatalog, c.consttype) else { return Ok(refuse("words", "an array literal without element facts")) };
                    let mut scratch = [0u8; 256];
                    let mut region = gravity_expr::Region::over(&mut scratch);
                    let Ok(mut walk) = gravity_expr::nodes::array::Walk::over(c.constvalue, ef, &mut region) else { return Ok(refuse("words", "an array literal the walk did not open")) };
                    let mut e = None;
                    for _ in 0..=k {
                        e = walk.next();
                    }
                    let Some(e) = e else { return Ok(refuse("words", "an element past the array literal's end")) };
                    GraphWord::Const { value: if e.isnull { 0 } else { e.value.as_u64() }, null: e.isnull as u8 }
                }
            }
            WordTpl::Extern(id) => {
                if id < 1 {
                    return Ok(refuse("words", "an extern parameter below 1"));
                }
                GraphWord::Extern(id as usize - 1)
            }
            WordTpl::Expr(o) | WordTpl::Late(o) => {
                let Some(n) = nodes.get(o as usize) else { return Ok(refuse("words", "a key expression locator outside the plan")) };
                volatile |= crate::effects::contains_volatile(*n);
                let Some(st) = execexpr::exec_init_expr(mcx, Some(*n), shapes::param_bind(exprs))? else { return Ok(refuse("words", "a key expression the executor did not initialize")) };
                exprs.exprs.push(st); // alloc: Program
                let expr = exprs.exprs.len() - 1;
                match (*t, late_owner(program, w)) {
                    (WordTpl::Late(_), Some(pipeline)) => GraphWord::Late { expr, pipeline },
                    _ => GraphWord::Expr(expr),
                }
            }
            WordTpl::Cursor(o) => {
                let Some(c) = nodes.get(o as usize).and_then(|n| n.as_current_of_expr()) else { return Ok(refuse("words", "a cursor locator that is not a CURRENT OF")) };
                // the host reads this execution's cursor name through it (hosts v4/m2_tid_sample.rs `cursor_of`)
                GraphWord::Const { value: c as *const types_nodes::primnodes::CurrentOfExpr<'_> as u64, null: 0 }
            }
            WordTpl::Set(o, d) => {
                let Some(c) = nodes.get(o as usize).and_then(|n| n.as_const()) else { return Ok(refuse("words", "a set locator that is not an array literal")) };
                if c.constisnull {
                    GraphWord::Const { value: 0, null: 1 }
                } else {
                    // lowerv4's expr.saop builds it (step/expr_saop_set.rs `bound`), through its face
                    let built = gravity_lowerv4::ops::kind_as::<gravity_lowerv4::face::ext::ExprExtImpl>(gravity_lowerv4::face::kinds::KindId::ExprSaop).and_then(|x| x.0.bind_set(owner, d, c.constvalue.as_u64()));
                    let Some(set) = built else { return Ok(refuse("words", "a hashed IN list without its set builder")) };
                    let set = set?;
                    let value = set.as_ptr() as u64;
                    sets.push(set); // alloc: Program
                    GraphWord::Const { value, null: 0 }
                }
            }
            WordTpl::Zero => GraphWord::Zero,
            WordTpl::Deferred => GraphWord::Deferred,
            WordTpl::Null => GraphWord::Const { value: 0, null: 1 },
        });
    }
    Ok(Ok((GraphWords { words: words.into_boxed_slice(), key, inits: Vec::new(), arms: Vec::new(), prune: None, tpl: None, sets, held: Default::default(), no_proc: false, est: None, partial: gravity_decompose::admit::partial_root(), rels_decide: false, folded: Box::new([]), genp: Default::default(), domains: Default::default(), deferred: Box::new([]), reads: gravity_deps::Held::of(gravity_deps::Deps::default()) }, volatile))) // alloc: Program
}

/// D899: the program pipeline that alone reads word `w`, at its driving site's key words, when it is not the emit (the
/// emit's scan is always pulled, and first): a late word's owner. `None` (another site reads it too, or none does) binds
/// the word at bind, as an `Expr`.
fn late_owner(program: &Program, w: usize) -> Option<u16> {
    let reads = |s: &gravity_program::SiteDesc| s.param_slots.iter().any(|&x| x >= 0 && x as usize == w);
    let mut owner = None;
    for (i, site) in program.sites.iter().enumerate().filter(|(_, s)| reads(s)) {
        let p = program.pipelines.iter().position(|p| p.driving_site >= 0 && p.driving_site as usize == i)?;
        if owner.is_some_and(|o| o != p) {
            return None;
        }
        owner = Some(p);
    }
    owner.filter(|&p| p + 1 < program.pipelines.len()).map(|p| p as u16)
}

/// Every source of the graph in pipeline order (a nest's or nested SubPlan's inner after its outer's ops), with its plan node id.
pub(crate) fn sources<'a, 'p>(p: &'a PipelineNode<'p>, out: &mut Vec<(&'a Source<'p>, i32)>) {
    stack_depth_core::check_stack_depth_or_panic();
    out.push((&p.source, p.node_id));
    for op in p.ops.iter() {
        if let Op::Join(JoinDesc::Nest { inner, .. }) | Op::SubPlan(SubPlanDesc { form: SubForm::Nested { inner }, .. }) = op {
            sources(inner, out);
        }
    }
}

/// Whether graph source `s` can serve program site `site`: its relation, and its index (a graph source serves one program
/// site each; a lowered program may declare a second site over the same relation (rung 6's count site beside the entries
/// window, lower.md §15.18), which takes the used source's recipe base; a bitmap source's site is the decomposer's range
/// site: the program's site carries the probe's index, except the morsel form's, which keeps the range site (no index)).
fn site_fits(site: &gravity_program::SiteDesc, s: &Source<'_>) -> bool {
    let index = match s {
        Source::Bitmap { plan: gravity_decompose::ops::shared::scan::BitmapPlan::Index { indexid, .. }, morsel, .. } if !*morsel || !site.is_range() => *indexid,
        _ => s.relation().map_or(0, |(_, _, sd)| sd.index),
    };
    s.relation().is_some_and(|(_, relid, _)| relid == site.relation) && index == site.index
}

/// One host recipe per source site of the program, in site order: the site's own relation,
/// index and attributes (a write widens its site), the graph's source for the access method and the plan's index clauses.
fn registry(program: &Program, graph: &PipelineGraph<'static>, pstmt: &'static PlannedStmt<'static>, mcx: Mcx<'static>) -> PgResult<Result<Arc<RecipeSet>, Refused>> {
    let mut all: Vec<(&Source<'static>, i32)> = Vec::new(); // alloc: Program
    let mut start = Vec::with_capacity(graph.pipelines.len()); // alloc: Program
    let mut top = Vec::new(); // alloc: Program
    for (k, p) in graph.pipelines.iter().enumerate() {
        start.push(all.len()); // alloc: Program
        sources(p, &mut all);
        top.resize(all.len(), k); // alloc: Program
    }
    // D3588: the program numbers its sites in its pipelines' order, which moves an Append's arms ahead of a later child's
    // pipelines (lowerv4 `arm_order`, D1752): each graph pipeline's sources (its own, then its nest inners') are searched
    // at the first site a pipeline reading its source drives, so two sources over one relation and index (a bitmap arm and
    // a nest inner's index probe) meet their own sites; a graph pipeline whose source drives no site keeps its place
    // after its graph predecessor
    let mut first = vec![usize::MAX; graph.pipelines.len()]; // alloc: Program
    for p in program.pipelines.iter() {
        if let (Ok(si), Some(f)) = (usize::try_from(p.driving_site), first.get_mut(p.source_of as usize)) {
            *f = (*f).min(si);
        }
    }
    for k in 0..first.len() {
        if first[k] == usize::MAX {
            first[k] = if k == 0 { 0 } else { first[k - 1] };
        }
    }
    let mut order: Vec<usize> = (0..all.len()).collect(); // alloc: Program
    order.sort_by_key(|&i| (first[top[i]], i));
    // D3615 (a): each site's first driving pipeline and whether any of them is eligible, read in one walk (each site walked
    // every pipeline twice: 32 M steps at 4000 partitions, on every admission, a shared-tier hit's too)
    let mut drive: Vec<(Option<usize>, bool)> = vec![(None, false); program.sites.len()]; // alloc: Program
    // D3670: and whether every one of them is a rerun set's or an InitPlan's member (its runs count the site's scan)
    let lifetimes = program.v4.as_deref().map(|d| &d.lifetimes);
    let run_member = |k: usize| u16::try_from(k).ok().and_then(|k| lifetimes.and_then(|l| l.member(k))).is_some_and(|m| matches!(m.runner, gravity_program::lifetime::Runner::Set(_) | gravity_program::lifetime::Runner::Init(_)));
    let mut run_counted = vec![true; program.sites.len()]; // alloc: Program
    for (k, p) in program.pipelines.iter().enumerate() {
        if let Some(d) = usize::try_from(p.driving_site).ok().filter(|&d| d < drive.len()) {
            drive[d].0.get_or_insert(k);
            drive[d].1 |= p.is_eligible();
            run_counted[d] &= run_member(k);
        }
    }
    let mut used = vec![false; all.len()]; // alloc: Program
    // D3769: a pinned site (V4Descs `site_pins`: a plan Material's pulled child, and the sites of its relation and index
    // numbered before it) takes its named source first, so no site the walk order pairs can take it; fail closed when no
    // source of that node fits the site
    let mut pinned: Vec<Option<usize>> = vec![None; program.sites.len()]; // alloc: Program
    for &(si, node) in program.v4.as_deref().map_or(&[][..], |d| &d.site_pins[..]) {
        let Some(site) = program.sites.get(si as usize).filter(|s| s.is_source()) else {
            return Ok(refuse("hosts", format!("a pinned site {si} that is no source site"))); // alloc: Cold
        };
        // a second site over that source (a make run twice) takes the used source's recipe base, as the walk order's does
        let of_node = |unused: bool| (0..all.len()).find(|&i| (!unused || !used[i]) && all[i].1 == node && site_fits(site, all[i].0));
        let Some(i) = of_node(true).or_else(|| of_node(false)) else {
            return Ok(refuse("hosts", format!("no graph source of node {node} for pinned site relation {}", site.relation))); // alloc: Cold
        };
        used[i] = true;
        pinned[si as usize] = Some(i);
    }
    // declared before the list made in it, so an early return drops the list first
    let owner = memledger::charge::statement_owner();
    let mut root = mcx::SendRoot::new_owned_small(RECIPE_ROOT, owner);
    let mut reg: TplVec<RecipeTpl> = raw::tpl_list(&mut root);
    let lists = raw::tpl_lists(&mut root);
    // one block of the list's final size: the root keeps no smaller blocks a growth would leave behind
    tpl_room(&mut reg, program.sites.iter().filter(|s| s.is_source()).count(), true, RECIPE_ROOT);
    for (si, site) in program.sites.iter().enumerate().filter(|(_, s)| s.is_source()) {
        let fits = |i: usize| site_fits(site, all[i].0);
        let find = |unused: bool| order.iter().copied().find(|&i| (!unused || !used[i]) && fits(i));
        // a driving site binds the source its pipeline read (`Pipeline::source_of`), else the first fitting one
        let own = drive[si].0.and_then(|k| start.get(program.pipelines[k].source_of as usize)).copied();
        let Some(i) = pinned[si].or_else(|| own.filter(|&i| i < all.len() && !used[i] && fits(i))).or_else(|| find(true)).or_else(|| find(false)) else {
            return Ok(refuse("hosts", format!("no graph source for site relation {}", site.relation))); // alloc: Cold
        };
        used[i] = true;
        let (source, node_id) = all[i];
        let attnos: &[i16] = &site.out_attnos[..site.natts()];
        if let Err(r) = attnos_admitted(source, attnos) {
            return Ok(Err(r));
        }
        if !site.tables.is_empty() && !matches!(source, Source::Range { am: Am::Pgrcolumnar, .. } | Source::Sample { am: Am::Pgrcolumnar, .. }) {
            return Ok(refuse("hosts", format!("site relation {} declares {} table lanes over a source without dictionaries", site.relation, site.tables.len()))); // alloc: Cold
        }
        let leader = drive[si].0.is_some() && !drive[si].1;
        let run_counted = drive[si].0.is_some() && run_counted[si];
        let cx = AdmitCx { pstmt, mcx, site, source, node_id, attnos, graph, leader, run_counted, probed: drive[si].0.is_none(), lists: &lists };
        let mut got = None;
        for f in RUNGS.iter().flat_map(|r| r.0.iter()) {
            match f(&cx)? {
                Ok(Some(t)) => {
                    got = Some(t);
                    break;
                }
                Ok(None) => {}
                Err(e) => return Ok(Err(e)),
            }
        }
        match got {
            Some(t) => {
                let t = t.file(&mut root);
                tpl_push(&mut reg, t, RECIPE_ROOT)
            }
            None => return Ok(refuse("hosts", "a source without a host recipe (bitmap / tid / sample / table function / named)")),
        }
    }
    Ok(Ok(RecipeSet::shared(reg, root, owner)))
}

/// A range site over no attribute (count(*) over no column) reaches the rungs over a heap only, M2's count-only recipe
/// (V4R-37 a); over any other access method the driving lane is the shape table's first by-value column (lower.md §14.4).
fn attnos_admitted(source: &Source<'_>, attnos: &[i16]) -> Result<(), Refused> {
    match source {
        Source::Range { am, .. } if attnos.is_empty() && *am != Am::Heap => refuse("hosts", "a range source without an attribute (count(*) over no column: the driving lane is the shape table's first by-value column; lower.md §14.4)"),
        _ => Ok(()),
    }
}

/// V4R-37 b: a v4 program that declares `src.prune` reads this execution's extern parameters through `GraphWords.prune`'s
/// handle, which `runner::eval_keys` sets at every execution. A composed Append's own exec pruning keeps its row; any other
/// program with the protocol gets the handle alone (`nsubplans` 0: `prune::exec_skip` leaves its arms to the protocol).
/// D3663: so does a program with a relation set its Append prunes at the executor start (`sets`: `SiteDesc::set_prune`),
/// whose members exec `Initial` decides under the execution's parameters.
fn v4_prune_handle<A: gravity_program::Allocator + Clone>(v4: Option<&gravity_program::protos::V4Descs<A>>, sets: bool, prune: Option<crate::ops::append::prune::ExecPrune>, params: ParamListHandle) -> Option<crate::ops::append::prune::ExecPrune> {
    let declares = sets || v4.is_some_and(|d| d.protos.iter().any(|p| p.kind == gravity_program::protos::ProtoKind::Prune));
    match prune {
        None if declares => Some(crate::ops::append::prune::ExecPrune { index: 0, nsubplans: 0, params }),
        p => p,
    }
}

/// A site's recipe as filed under the key: no plan pointer, no lock, so a memo hit rebuilds the registry without opening a relation.
/// planprod P6-c2: an index walk's recipe (`Index`, `Bitmap`) and a granule walk's (`Granule`) as the set holds them, their
/// lists in the set's root (`SiteTpl`, `GranuleTpl`); a rung admits one with its own (`AdmitTpl`), which `registry` files.
#[derive(Clone)]
pub enum RecipeTpl<S = SiteTpl, G = GranuleTpl> {
    Index(S),
    /// The bitmap walk's recipe and the plan's `bitmapqualorig` (compiled per execution: its params are the statement's).
    /// The recheck qual is this execution's plan's (`bitmapqualorig`): found in the graph at `registry_from` by relation and
    /// the BitmapIndexScan's plan node id (D2207: two scans over one index each recheck with their own qual). The fields:
    /// the recipe, the relation, the index, the BitmapIndexScan's node id, and (D2754) the BitmapHeapScan's node id when a
    /// front-key hit, bound without the graph, reads the recheck from that node of its own plan (ops/scan/sites_heap.rs
    /// `plan_recheck`; `None`: the recheck binds from the graph alone and the front never files the statement), and (D2754
    /// part 4) when that recheck reads executor parameters, the key words and the parameters it reads as the graph's bind
    /// found them (ops/scan/bitmap_param.rs `front_words`): a front-key hit compiles the hit's own recheck over them.
    Bitmap(S, u32, u32, i32, Option<i32>, Option<Arc<crate::ops::scan::bitmap_param::ParamWords>>),
    Range(RangeRecipe),
    Granule(G),
    Fetch(gravity_hosts::ops::scan::pgrc::fetch_site::FetchRecipe),
    /// A heap fetch site's (exec ops/sort/fetch.rs), bound as a rung recipe.
    HeapFetch(gravity_hosts::ops::sort::heap_fetch::HeapFetchRecipe),
    /// A later rung's recipe kind, bound by its own `BIND` table.
    Rung(Arc<dyn core::any::Any + Send + Sync>),
}

/// A site's recipe as a rung admits it (`AdmitResult`): an index or granule walk's recipe with its own lists, until
/// `registry` files it.
pub type AdmitTpl = RecipeTpl<SiteRecipe, GranuleRecipe>;

impl AdmitTpl {
    /// The recipe filed in `root`, its set's (`registry`): an index or granule walk's lists moved there (`SiteTpl::file`,
    /// `GranuleTpl::file`).
    fn file(self, root: &mut mcx::SendRoot) -> RecipeTpl {
        match self {
            RecipeTpl::Index(s) => RecipeTpl::Index(SiteTpl::file(s, root)),
            RecipeTpl::Bitmap(s, relid, indexid, node, heap, words) => RecipeTpl::Bitmap(SiteTpl::file(s, root), relid, indexid, node, heap, words),
            RecipeTpl::Range(r) => RecipeTpl::Range(r),
            RecipeTpl::Granule(r) => RecipeTpl::Granule(GranuleTpl::file(r, root)),
            RecipeTpl::Fetch(r) => RecipeTpl::Fetch(r),
            RecipeTpl::HeapFetch(r) => RecipeTpl::HeapFetch(r),
            RecipeTpl::Rung(r) => RecipeTpl::Rung(r),
        }
    }
}

/// The result relation's `insertedCols` / `updatedCols` (`GetResultRTEPermissionInfo`: the first result relation RTE that
/// carries a `perminfoindex` — a pruned partition's carries none, its root's (last in `resultRelations`) does).
pub fn modified_cols(pstmt: &'static PlannedStmt<'static>) -> Option<gravity_hosts::ops::modifytable::write_constraints::ModifiedCols> {
    let pi = pstmt.resultRelations.iter().find_map(|rti| {
        let rte = shapes::rte(pstmt, rti as u32)?;
        pstmt.permInfos.nth(rte.perminfoindex.checked_sub(1)? as usize).as_rte_permission_info()
    })?;
    Some((&pi.insertedCols, &pi.updatedCols))
}

/// D411: whether recipe template `t` binds from the admitting statement's graph (`BindCx.graph`), so a front-key hit,
/// bound without it, could not bind it and its statement is never filed there: scan/front_kind.rs's kinds, except a nest
/// inner's computed-key walk whose keys are its plan node's own expressions (D2754, ops/scan/param_key.rs
/// `plan_keyed`), which a front hit binds from that execution's plan.
fn graph_bound(t: &RecipeTpl) -> bool {
    crate::ops::scan::front_kind::binds_from_graph(t) && !crate::ops::scan::param_key::plan_keyed(t)
}

/// THIS execution's registry from the cached template: a bitmap recipe's recheck qual is compiled here (its params are
/// this execution's, `shapes::compile_filter`), a volatile recheck refused by name.
fn registry_from(recipes: &[RecipeTpl], graph: Option<&PipelineGraph<'static>>, front: Option<&'static PlannedStmt<'static>>, plan: Option<&'static PlannedStmt<'static>>, mcx: Mcx<'static>, exprs: &mut KeyExprs) -> PgResult<Result<HostRegistry, Refused>> {
    let mut reg = shapes::empty_registry();
    let cx = BindCx { graph, mcx, front, plan, owner: gravity_driver::stmt_context().map(|(o, _)| (o, memledger::Kind::Region)) };
    // memsig E-pub t4: what the registry grows at bind (the sample seeds) on the binding statement's session (charge_to)
    reg.charge_to(cx.owner);
    for r in recipes {
        let mut done = false;
        for f in RUNGS.iter().flat_map(|r| r.1.iter()) {
            match f(r, &mut reg, &cx, exprs)? {
                Ok(true) => {
                    done = true;
                    break;
                }
                Ok(false) => {}
                Err(e) => return Ok(Err(e)),
            }
        }
        if !done {
            return Ok(refuse("hosts", "a recipe no rung binds"));
        }
    }
    Ok(Ok(reg))
}

/// A worker's non-range site traps (`driver::bind::sites`), so a pipeline an index site drives runs on the leader
/// (`Reason::IndexSource`). D2984: only that pipeline; the program's others stay eligible. A pipeline reads a site it is
/// not driven by only through a step the lowerer keeps serial already (a nest inner, a scan sub-select, a merge join, a
/// lazy build's probe, a pulled set: their `serial()`), as opr_sanity's catalog nest loop once reached the trap
/// (`unsupported execution (kind 4)`) from a worker. An index-only site the pool reads by leaf pages
/// (`SiteDesc::leaves`) or an ordered site it reads in key ranges (`SiteDesc::ranges`) keeps its pipeline eligible: every
/// participant binds its leaf or range source.
fn serial_over_index_sites(program: &mut Program) {
    let sites = &program.sites;
    for p in program.pipelines.iter_mut() {
        let d = p.driving_site;
        if p.is_eligible() && d >= 0 && sites.get(d as usize).is_some_and(|s| s.is_source() && !s.is_range() && !s.leaves() && !s.ranges()) {
            if let Some(reason) = gravity_program::source_serial(gravity_program::SourceShape::Index) {
                p.parallel = gravity_program::Parallel::Ineligible { site: d, reason };
            }
        }
    }
}

/// A range site over a temporary relation reads the backend's local buffers, which are thread-local: every eligible
/// pipeline of such a program runs on the leader (`Reason::TempSource`; item 11 of r1-index — 480 pg_regress statements
/// map-wide, `char` and `varchar` whole).
fn serial_over_temp_sources(program: &mut Program, mcx: Mcx<'static>) -> PgResult<()> {
    let mut temp = None;
    for (i, s) in program.sites.iter().enumerate() {
        if !(s.is_source() && s.is_range()) {
            continue;
        }
        let rel = table::table_open(mcx, s.relation as Oid, NoLock)?;
        let local = rel.uses_local_buffers();
        rel.close(NoLock)?;
        if local {
            temp = Some(i);
            break;
        }
    }
    let Some(site) = temp else { return Ok(()) };
    let Some(reason) = gravity_program::source_serial(gravity_program::SourceShape::Temp) else { return Ok(()) };
    for p in program.pipelines.iter_mut() {
        if p.is_eligible() {
            p.parallel = gravity_program::Parallel::Ineligible { site: site as i16, reason };
        }
    }
    Ok(())
}

/// `ExecCheckXactReadOnly` for every served statement, whatever its command (D733), run as standard_ExecutorStart runs
/// it: only in a read-only transaction (its parallel-mode arm is unreachable here: the entry refuses in parallel mode). A
/// relation whose permission info asks more than ACL_SELECT (a write's target; a SELECT's FOR UPDATE / NO KEY UPDATE /
/// SHARE / KEY SHARE, which transformLockingClause adds ACL_UPDATE for) refuses the statement unless it is temporary
/// (PostgreSQL exempts the temp namespaces: `transactions.sql`'s READ ONLY block writes temptest). A plain SELECT's infos
/// ask ACL_SELECT alone: the walk is a mask test per relation, and nothing is opened.
pub fn prevent_write_if_read_only(pstmt: &PlannedStmt<'_>, cmd: gravity_program::Cmd, mcx: Mcx<'_>) -> PgResult<()> {
    if !xact::XactReadOnly() {
        return Ok(());
    }
    for pi in pstmt.permInfos.iter().filter_map(|n| n.as_rte_permission_info()) {
        if pi.requiredPerms & !types_nodes::parsenodes::ACL_SELECT == 0 {
            continue;
        }
        let rel = table::table_open(mcx, pi.relid, NoLock)?;
        let temp = rel.uses_local_buffers();
        rel.close(NoLock)?;
        if !temp {
            return xact::PreventCommandIfReadOnly(command_name(pstmt, cmd));
        }
    }
    Ok(())
}

/// `CreateCommandName(plannedstmt)` as ExecCheckXactReadOnly names the statement (D733): a SELECT's tag is its first row
/// mark's strength (CreateCommandTag's "not 100% but probably close enough"; no mark, or a first mark that locks nothing:
/// SELECT); a write's is its command's.
fn command_name(pstmt: &PlannedStmt<'_>, cmd: gravity_program::Cmd) -> &'static str {
    use types_nodes::nodes_enums::{CmdType, LockClauseStrength::*};
    if pstmt.commandType != CmdType::CMD_SELECT {
        return cmd.tag();
    }
    match pstmt.rowMarks.iter().next().and_then(|n| n.as_plan_row_mark()).map(|r| r.strength) {
        Some(LCS_FORKEYSHARE) => "SELECT FOR KEY SHARE",
        Some(LCS_FORSHARE) => "SELECT FOR SHARE",
        Some(LCS_FORNOKEYUPDATE) => "SELECT FOR NO KEY UPDATE",
        Some(LCS_FORUPDATE) => "SELECT FOR UPDATE",
        _ => "SELECT",
    }
}

/// This transaction has made combo CIDs (it inserted and then deleted or updated a tuple), so a
/// `HEAP_COMBOCID` tuple may be met and `combocid`'s map — the leader's thread-local — is needed to
/// read it: a Gravity pool worker's is empty and aborts in `HeapTupleSatisfiesMVCCPage` on a frame
/// that cannot unwind. The runner then runs every pipeline on the leader (`run_parallel`, width 0).
/// A read-only transaction makes none, so the width rule is untouched by any benchmark statement.
pub fn leader_only() -> bool {
    if !combocid_seams::combo_cid_count::is_installed() {
        return false;
    }
    combocid_seams::combo_cid_count::call() > 0
}

/// D2798: a publishing lowering's program (`LowerOpts::publish`: one store per row the plain program lacks) is filed and
/// found apart from the same graph's plain one, in this backend's cache alone (no front variant, no shared tier).
/// D3019: content key `base` apart for an execution whose text parameter is a loud LIKE pattern
/// (`gravity_decompose::admit::externs_salt`; 0 for every other, which keeps `base`): the lowering hoists a parameter's
/// LIKE only where its pattern is quiet, so such an execution never takes that program by its key.
fn salted(base: ContentKey) -> ContentKey {
    let salt = gravity_decompose::admit::externs_salt();
    if salt == 0 {
        return base;
    }
    let mut h = gravity_abi::hash::Mix128::new();
    for w in [base.0 as u64, (base.0 >> 64) as u64, salt, 0x4433_3031_3953] {
        h.word(w);
    }
    ContentKey(h.key128())
}

fn published(key: ContentKey) -> ContentKey {
    if is_publishing() { ContentKey(key.0 ^ PUBLISH_SALT) } else { key }
}

const PUBLISH_SALT: u128 = 0x9e37_79b9_7f4a_7c15_f39c_c060_5ced_c835;

/// EXPLAIN ANALYZE's program (counter cells, `LowerOpts::instrument`) is filed beside the plain program under its own
/// key, in this backend's cache and the process-wide tier: an EXPLAIN ANALYZE never takes the plain program and a plain
/// execution never takes the instrumented one.
fn instrumented_key(key: ContentKey) -> ContentKey {
    if is_instrumented() { ContentKey(key.0 ^ INSTR_SALT) } else { key }
}

const INSTR_SALT: u128 = 0x5bd1_e995_c2b2_ae35_27d4_eb2f_1656_67c5;

/// C3 S4b: the program tiers' one key (this backend's cache and memos, the level tier, the process-wide tier): the graph
/// content key (literal-free: words bind per execution), what the plan names (`plan_names`: its objects without their
/// generations, so a dropped and recreated function or relation is another key: D3900 / D3901), the database and the
/// road-off text (`residency::tier_salt`, D311: a lowering input no content key carries). No catalog epoch: a filed
/// program is served while its lowering's recorded dependencies are current (`Deps::current`).
fn tier_key(base: ContentKey, names: u128) -> ContentKey {
    let mut h = gravity_abi::hash::Mix128::new();
    for w in [base.0 as u64, (base.0 >> 64) as u64, names as u64, (names >> 64) as u64, gravity_hosts::ops::scan::pgrc::residency::tier_salt()] {
        h.word(w);
    }
    ContentKey(h.key128())
}

/// C1 part 2: front walk `f` with the statement's reader kind in its key when the reader is partial (a named cursor's
/// FETCH: decompose `admit::partial_root`), as the content key holds it (decompose `PipelineGraph::partial`): a front hit
/// hands each reader kind its own program, and every other front key is unchanged.
/// D4600 (`lean`): `unsalted`, the admission binds no extern fact, deferred slot or re-estimated row, where the salt is 0.
fn reader_front(mut f: crate::inputs::front::Front<'_>, unsalted: bool) -> crate::inputs::front::Front<'_> {
    // D3019: an execution whose text parameter is a loud LIKE pattern is keyed apart (`salted`)
    let salt = if unsalted { 0 } else { gravity_decompose::admit::externs_salt() };
    if salt != 0 {
        let mut h = gravity_abi::hash::Mix128::new();
        for w in [f.main as u64, (f.main >> 64) as u64, salt] {
            h.word(w);
        }
        f.main = h.key128();
    }
    if gravity_decompose::admit::partial_root() {
        let mut h = gravity_abi::hash::Mix128::new();
        for w in [f.main as u64, (f.main >> 64) as u64, 0x4355_5253_4f52] {
            h.word(w);
        }
        f.main = h.key128();
    }
    // D3585: a relation-set admission's programs are filed apart from the per-arm ones
    if gravity_decompose::admit::relsets_on() {
        let mut h = gravity_abi::hash::Mix128::new();
        for w in [f.main as u64, (f.main >> 64) as u64, 0x5245_4c53_4554] {
            h.word(w);
        }
        f.main = h.key128();
    }
    f
}

struct Timing {
    decompose: u64,
    key: u64,
    lower: u64,
    backend: u64,
    /// The program, its template and its recipes came from the key's memo: no lowering, no relation opened.
    hit: bool,
    /// ... and the key from the front key's memo (front.rs): no decompose either.
    front: bool,
    /// The lowered IR's hash when this admission lowered (V4R-18: the front variant's, for the shadow check).
    ir: Option<u128>,
    /// ... and the hit came from the process-wide tier (V4R-35): another backend (or this one, before its own evicted it) lowered it.
    shared: bool,
    /// What the process-wide tier did with this admission (the report line's `tier=`): off (no stamp), hit, published,
    /// private:<why> (a program with host state or a composed statement: this backend's; `private_tier`), raced (the stamp
    /// moved while it lowered), own-ddl, miss:no_cache.<why> (never filed: `Descs::no_cache_why`), miss.
    tier: &'static str,
    /// D644: why this admission's shape was not filed under the front key (the report's `front not filed (<why>)` line);
    /// `None`: filed, a front hit, or no report asked.
    unfiled: Option<&'static str>,
    /// The admission's planning context's peak and its plan row's bytes after the lowering (`plan::PlanScope::stats`).
    plan: (usize, i64),
    /// The lowered IR's bytes in its programs' roots (main, InitPlans, arms: the statement's), when this admission lowered.
    ir_bytes: usize,
    /// D4786: the program is filed (or was found) under the key salted with its class-keyed lists' bytes (`list_salted`).
    salted: bool,
}

/// The per-backend memos by content key (templates, recipes, refusals) are cleared when full (C3 S4b: a filing whose recorded dependencies moved stays until its key is filed again or the memo is cleared).
pub const MEMO_ENTRIES: usize = 4096;

fn us(t: Instant) -> u64 {
    t.elapsed().as_nanos() as u64 / 1000
}

/// Under `PGRUST_GRAVITY_FACTS` (read once per backend): one `gravity facts:` LOG line per relation the plan
/// names, `facts::render`'s text — what `scripts/gravity-facts-check.sh` diffs against the capture's header.
fn dump_facts(facts: &RelcacheFacts<'_>, pstmt: &PlannedStmt<'_>) {
    if !crate::inputs::knobs::seam::facts_on() {
        return;
    }
    let mut seen: Vec<u32> = Vec::new(); // alloc: Cold
    for rte in pstmt.rtable.iter().filter_map(|n| n.as_range_tbl_entry()) {
        if rte.relid == 0 || seen.contains(&rte.relid) {
            continue;
        }
        seen.push(rte.relid);
        let mut text = String::new(); // alloc: Cold
        let _ = gravity_decompose::catalog::render(facts, rte.relid, &mut text);
        for line in text.lines() {
            let _ = elog::ereport(types_error::LOG).errmsg(format!("gravity facts: {line}")).errhidestmt(true).errhidecontext(true).finish(types_error::ErrorLocation::new("gravity", 0, "facts")); // alloc: Cold
        }
    }
}

/// The program, template and recipes filed under `key` (the backend's program cache and both memos); D3881: else what a
/// static level of this thread filed (`LEVEL_TIER`), filed here too. C3 S4b: only while the lowering's recorded
/// dependencies are current (the entry's mark: no read while this thread's views stand since its last check); a stale
/// filing is a miss, which the next lowering's filing replaces.
fn lookup(b: &mut Backend, key: ContentKey) -> Option<Filed> {
    if let Some((t, mark)) = b.graph_words.get(&key) {
        if !t.deps.current_since(mark) {
            note_stale("backend", &t.deps);
            return None;
        }
        let t = t.clone();
        let here = b.cache.l2(key).and_then(|p| b.graph_registry.get(&key).cloned().map(|(r, o)| (p, t, r, o)));
        if here.is_some() {
            return here;
        }
    }
    let h = level_take(key)?;
    file_here(b, key, &h);
    Some(h)
}

/// D644: every key-walk position a template binds from (its words' and its check options' locators): a front variant
/// may hold a stand-in for a synthesized node only where none reads (front.rs `fill_reading`).
fn template_reads(g: &GraphTpl) -> Vec<u32> {
    let mut r = word_reads(&g.words);
    r.extend(g.checks.iter().map(|c| c.node)); // alloc: Cold
    r
}

/// The key-walk positions words `w` bind from (`template_reads`' word half).
fn word_reads(w: &[WordTpl]) -> Vec<u32> {
    w.iter().filter_map(|w| match *w { WordTpl::Const(o) | WordTpl::Elem(o, _) | WordTpl::Expr(o) | WordTpl::Late(o) | WordTpl::Cursor(o) | WordTpl::Set(o, _) => Some(o), WordTpl::Extern(_) | WordTpl::Zero | WordTpl::Null | WordTpl::Deferred => None }).collect() // alloc: Cold
}

/// The front key's shadow check (`pgrust.gravity_front_check`): a hit decomposes and keys again, and a different content
/// key or key-walk node is a front-key bug, the statement's ERROR whatever the fallback posture. V4R-18: a variant filed
/// by a lowering (`ir`) serves by-reference words whose images differ, so its content key may differ; the check then
/// lowers this statement again and needs the variant's IR (a literal the lowering baked without logging it differs).
/// KEY-PREDICTED-ROWS road (b): `filed` is the variant's key and its program's estimate guards: a guarded program's key is
/// its variant key (`file_local`), and the walk's exact rows decide as its lowering did.
#[allow(clippy::too_many_arguments)]
fn front_check(pcx: &plan::PlanScope<'_>, arena: ArenaRef, mcx: Mcx<'static>, pstmt: &'static PlannedStmt<'static>, filed: (ContentKey, &[gravity_lower::inputs::key::EstGuard]), nodes: &[Node<'static>], ir: Option<u128>, stand_ins: (&[u32], &[u32]), deps: Option<&gravity_deps::Deps>) -> PgResult<()> {
    let (key, guards) = filed;
    let facts_ctx = gravity_driver::mem::root::Root::new("GravityGraphFacts");
    let facts = RelcacheFacts::new(facts_ctx.mcx());
    shape_as_filed(pcx, arena, mcx, pstmt, &facts, key, guards)?;
    // C3 S4d: what decompose and the key walk read here, which the variant's recorded reads must cover (a read they miss
    // is a change that would not retire it)
    let reads = gravity_deps::Recording::start();
    let why = match gravity_decompose::decompose_planned(arena, pstmt, &facts, mcx, pcx.mcx()) {
        Err(r) => Some(format!("decompose refuses a filed shape ({r})")), // alloc: Cold
        Ok(graph) => {
            let mut fresh: ArenaVec<Node<'static>> = ArenaVec::new_in(arena);
            // C3 S4b: the key holds no catalog epoch, so an invalidation this check's own relation opens read (D646) keys alike
            let base = salted(gravity_lower::inputs::key::key_and_nodes(&graph, &mut |n| fresh.push(n)));
            let k = ContentKey(gravity_lower::inputs::key::variant(tier_key(base, plan_names(pstmt, &graph).identity()).0, guards, None));
            let read = reads.finish();
            let missed: Vec<gravity_deps::Dep> = deps.map(|d| read.iter().filter(|&r| !d.covers(r)).take(6).collect()).unwrap_or_default(); // alloc: Cold
            let same = k == key;
            if !missed.is_empty() {
                Some(format!("gravity deps: decompose here read {missed:?}, which the variant's recorded dependencies miss ({} here, {} filed)", read.len(), deps.map_or(0, |d| d.len()))) // alloc: Cold
            } else if same && !gravity_lower::inputs::key::guards_hold(guards, &graph, &facts) {
                Some(format!("content key {:032x}: the filed program's estimate guards flip on this statement's estimates (the front key holds the plan's rows exactly)", k.0)) // alloc: Cold
            } else if !same && ir.is_some() {
                let mut plan = Located::new(arena);
                let lowered = {
                    let _defer = gravity_expr::nodes::func::acl_defer();
                    gravity_lower::lower_composed(arena, pcx.mcx(), &graph, &facts, &gravity_expr::PgCatalog, &mut plan, gravity_lower::LowerOpts { instrument: false, publish: false, shape: None })
                };
                match lowered {
                    Ok(c) if ir.is_some_and(|filed| crate::inputs::front::variant_ir_ok(filed, || crate::inputs::front::ir_hash(&c.main))) => None,
                    Ok(_) => Some(format!("content key {:032x}, filed {:032x}: the variant's IR differs (a baked literal the bake log missed)", k.0, key.0)), // alloc: Cold
                    Err(r) if r.is_interrupted() => {
                        cancelpoll::interrupts::check()?;
                        None
                    }
                    Err(r) => Some(format!("content key {:032x}, filed {:032x}: the lowering refuses ({r})", k.0, key.0)), // alloc: Cold
                }
            } else if !same {
                Some(format!("content key {:032x}, filed {:032x}", k.0, key.0)) // alloc: Cold
            } else if fresh.len() != nodes.len() || fresh.iter().zip(nodes).enumerate().any(|(i, (a, b))| !crate::inputs::front::node_matches(*a, *b, stand_ins.0.contains(&(i as u32)), stand_ins.1.contains(&(i as u32)))) {
                Some(format!("the key walk's nodes differ ({} against {} filed)", fresh.len(), nodes.len())) // alloc: Cold
            } else {
                None
            }
        }
    };
    match why {
        None => Ok(()),
        Some(w) => Err(Box::new(types_error::PgError::error(format!("gravity front key: Reason::Bug: {w}")).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR))), // alloc: Cold
    }
}

/// While the content key is not the filed one, the named native shape refusal's relation moves.
fn shape_as_filed<'p>(pcx: &plan::PlanScope<'_>, arena: ArenaRef, mcx: Mcx<'p>, pstmt: &'p PlannedStmt<'p>, facts: &RelcacheFacts, key: ContentKey, guards: &[gravity_lower::inputs::key::EstGuard]) -> PgResult<()> {
    loop {
        let Ok(graph) = gravity_decompose::decompose_planned(arena, pstmt, facts, mcx, pcx.mcx()) else { return Ok(()) };
        let base = salted(gravity_lower::inputs::key::key_and_nodes(&graph, &mut |_| {}));
        if ContentKey(gravity_lower::inputs::key::variant(tier_key(base, plan_names(pstmt, &graph).identity()).0, guards, None)) == key {
            return Ok(());
        }
        let mut plan = Located::new(arena);
        let lowered = {
            let _defer = gravity_expr::nodes::func::acl_defer();
            gravity_lower::lower_composed(arena, pcx.mcx(), &graph, facts, &gravity_expr::PgCatalog, &mut plan, gravity_lower::LowerOpts { instrument: false, publish: false, shape: None })
        };
        match lowered {
            Err(r) if r.is_interrupted() => return cancelpoll::interrupts::check(),
            Err(r) if r.native_rel != 0 && gravity_hosts::ops::scan::am_rows::shape_rows(r.native_rel) => {
                gravity_hosts::ops::scan::am_rows::take_reshaped();
            }
            _ => return Ok(()),
        }
    }
}

/// D4600 (`lean`): `unsalted`, the admission's scopes hold no extern fact, deferred slot or re-estimated row, so its key
/// salt (`gravity_decompose::admit::externs_salt`, a fold over those three) is 0 and is not computed.
#[allow(clippy::too_many_arguments)]
fn admit_in(b: &mut Backend, pcx: &plan::PlanScope<'_>, arena: ArenaRef, mcx: Mcx<'static>, pstmt: &'static PlannedStmt<'static>, params: ParamListHandle, unsalted: bool, out: &mut Option<Admitted>) -> PgResult<Result<Timing, Refused>> {
    // the front key (front.rs): a filed shape's program and words without decompose_in or key_and_nodes (no timer of its
    // own: the report line's total is the walk, the lookup and the bind)
    // v5 errorhold: the plan-order program (every licence denied, `runner::recheck`) is the statement's own: never looked
    // up, shared or filed (a filed program under its key is the licensed one)
    let plan_order = gravity_decompose::effects::denying();
    // D2798: a publishing lowering takes no front variant (a front hit is the plain program's). C3 S4d: the walk runs under
    // no recording of its own (a memo-filed admission's records it, `graph`): a hit, a simple-protocol statement's steady
    // state, records nothing; a miss walks again under `reads` below.
    let front = if is_instrumented() || is_publishing() || crate::inputs::knobs::seam::facts_on() || plan_order { None } else { crate::inputs::front::walk(arena, pstmt, None).map(|f| reader_front(f, unsalted)) };
    if let Some((key, nodes, est, shift)) = front.as_ref().and_then(|f| crate::inputs::front::hit_est(f, arena, Some(mcx))) {
        // a program's literal guards (lower key.rs `lit_decide`) decide on this statement's literals here: the front key
        // holds a by-value literal as a word, so a statement whose literal decides otherwise takes the content-key path.
        // PS1-05: its cost guards (`cost_decide`) decide on this plan's charges, which the walk read (the key holds none).
        // PS1-21 step 1b: a relation guard decides on its relation's pages now (no plan or key follows them)
        let costs = |g: &[gravity_lower::inputs::key::EstGuard]| front.as_ref().is_some_and(|f| gravity_lower::inputs::key::costs_hold(g, &f.scans, &f.inits));
        // D3104: a variant's index morsel sites bind this plan's rows where they differ, and its estimate guards decide on them
        // (fire record: `front.morsel_rows.guard` when a guard decides otherwise on them: the graph path admits it)
        let moves = |g: &[gravity_lower::inputs::key::EstGuard]| shift.as_ref().is_none_or(|m| crate::inputs::front::moved_hold(g, m, pstmt) || (fires::fire("front.morsel_rows.guard"), false).1);
        if let Some(h) = lookup(b, key).filter(|h| gravity_lower::inputs::key::lits_hold(&h.1.guards, &nodes) && costs(&h.1.guards) && rels_hold(&h.1.guards) && moves(&h.1.guards)) {
            let est = match &shift {
                Some(m) => {
                    fires::fire("front.morsel_rows"); // D3104: a front hit bound an index morsel site's rows from the plan
                    moved_est(&h.0, est.as_deref(), m)
                }
                None => est,
            };
            if crate::inputs::front::check_on() {
                let synth = front.as_ref().map(crate::inputs::front::filed_synth).unwrap_or_default();
                let derived = front.as_ref().map(crate::inputs::front::filed_derived).unwrap_or_default();
                let deps = front.as_ref().and_then(crate::inputs::front::filed_deps);
                front_check(pcx, arena, mcx, pstmt, (key, &h.1.guards), &nodes, front.as_ref().and_then(crate::inputs::front::filed_ir), (&synth, &derived), deps.as_ref())?;
            }
            return bind(mcx, pstmt, params, key, &nodes, h, None, est, Timing { decompose: 0, key: 0, lower: 0, backend: 0, hit: true, front: true, ir: None, shared: false, tier: "-", unfiled: None, plan: (0, 0), ir_bytes: 0, salted: false }, out);
        }
    }
    // D4600 (`lean`): the plan's removed families (a Foreign Scan, a Custom Scan, a foreign result relation) raise their
    // 0A000 here, ahead of every other read (runner admission.rs `admit_either` no longer checks first). A front hit
    // needs no check: its variant was filed by an admission that passed it, and the front key holds every range-table
    // entry's relkind and the result relations, and refuses a Foreign Scan or a Custom Scan node (front.rs `plan` has no
    // arm for either), so a statement under the same front key passes it too.
    if lean::on() {
        removed::check(pstmt)?;
    }
    // C3 S4d: what the admission reads from the walk to the fill (the walk again, decompose, the key walk, the program
    // lookup or its lowering): the front variant's dependencies (`front::Entry::deps`). Unfinished (a refusal), its reads
    // are the admission's recording's, if any.
    let reads = gravity_deps::Recording::start();
    if front.is_some() {
        let _ = crate::inputs::front::walk(arena, pstmt, None);
    }
    let facts_ctx = gravity_driver::mem::root::Root::new("GravityGraphFacts");
    let facts = RelcacheFacts::new(facts_ctx.mcx());
    dump_facts(&facts, pstmt);
    let t0 = Instant::now();
    // MERGE carries the target's old row as synthesized junk Vars, which need the node allocator (lower.md §8.9 need 14): `decompose_in` takes it, plain `decompose` refuses MERGE by name.
    let graph = match gravity_decompose::decompose_planned(arena, pstmt, &facts, mcx, pcx.mcx()) {
        Ok(g) => g,
        Err(r) => return Ok(refuse("decompose", r.to_string())), // alloc: Cold
    };
    let t_dec = us(t0);
    cancelpoll::interrupts::check()?;
    let t0 = Instant::now();
    let mut nodes: ArenaVec<Node<'static>> = ArenaVec::new_in(arena);
    let base = gravity_lower::inputs::key::key_and_nodes(&graph, &mut |n| nodes.push(n));
    let base = if unsalted { base } else { salted(base) };
    let key = instrumented_key(published(tier_key(base, plan_names(pstmt, &graph).identity())));
    // KEY-PREDICTED-ROWS road (b): the filed program whose estimate decisions this graph's estimates make alike (`find`)
    let mut hit = if plan_order { None } else { find(b, key, &graph, &facts) };
    // D4786 (`PGRUST_GRAVITY_LIST_SALT`): a program that read a class-keyed list's image is filed under the key salted with
    // the graph's class-keyed lists' bytes (`list_salted`), here and process-wide; a miss under the key looks there
    let salt = if hit.is_none() && !plan_order && crate::inputs::knobs::lowerer::list_salt_on() { list_salted(key, &graph) } else { None };
    let mut salted_hit = false;
    if let Some(sk) = salt {
        hit = find(b, sk, &graph, &facts);
        salted_hit = hit.is_some();
    }
    // C3: a miss lowers with the plan's own dependency lists as its seed (`plan_deps`); V4R-35: it asks the process-wide
    // tier first, under the same key (None: this admission may not share)
    let plan_deps = if hit.is_none() && !plan_order { Some(plan_deps(pstmt, &graph)) } else { None };
    let shared = if plan_deps.is_some() && !is_publishing() { shared_key(key, false) } else { None };
    // the recipes are this backend's (function-manager frames, scan keys): built here from the graph, as `install` does
    // D3438: a program filed without its host state takes this backend's, rebuilt from its template (`None`: a miss)
    // D4786: a miss under the key looks under the list-salted key there too (`install` publishes a list program under it)
    let mut shared_salted = false;
    let found = shared.and_then(|sk| {
        shared_find(sk, &graph, &facts).or_else(|| {
            let f = shared_find(salt?.0, &graph, &facts)?;
            shared_salted = true;
            Some(f)
        })
    });
    let from_shared = match found {
        Some((program, tpl)) => match program.hosted.as_ref().map(gravity_lowerv4::hir::rebuild) {
            Some(None) => None,
            owner => match registry(&program, &graph, pstmt, mcx)? {
                Ok(r) => Some((program, tpl.0, r, owner.flatten())),
                Err(_) => None,
            },
        },
        None => None,
    };
    let t_key = us(t0);
    let mut t = Timing { decompose: t_dec, key: t_key, lower: 0, backend: 0, hit: hit.is_some() || from_shared.is_some(), front: false, ir: None, shared: from_shared.is_some(), tier: if from_shared.is_some() { "hit" } else if shared.is_some() { "miss" } else { "off" }, unfiled: None, plan: (0, 0), ir_bytes: 0, salted: false };
    if salted_hit {
        (t.salted, t.tier) = (true, "local:list");
        // the check the process-wide tier's hits take: a program that kept a plan image's address would differ here
        if let Some(h) = hit.as_ref().filter(|_| progcache_check_on()) {
            progcache_check(b, pcx, arena, &graph, &facts, pstmt, mcx, key, &nodes, h)?;
        }
    }
    if let Some(h) = from_shared {
        if progcache_check_on() {
            progcache_check(b, pcx, arena, &graph, &facts, pstmt, mcx, key, &nodes, &h)?;
        }
        match salt.filter(|_| shared_salted) {
            Some(sk) => {
                file_local(b, sk, &h);
                (t.salted, t.tier) = (true, "hit:list");
            }
            None => file_local(b, key, &h),
        }
        hit = Some(h);
    }
    let (program, tpl, recipes, owner) = match hit {
        Some(h) => h,
        None => match install(b, pcx, arena, &graph, &facts, pstmt, mcx, key, &nodes, &mut t, plan_deps.as_ref(), shared, salt)? {
            Ok(h) => h,
            Err(r) => return Ok(Err(r)),
        },
    };
    // KEY-PREDICTED-ROWS road (b): the key the program is filed under (its variant, `file_local`), which the front variant
    // names; this statement's estimates where the program was lowered with others (filed with the front variant too: a
    // front hit binds without the graph)
    let key = if t.salted { salt.unwrap_or(key) } else { key };
    let key = ContentKey(gravity_lower::inputs::key::variant(key.0, &tpl.guards, None));
    let est = estimates(&program, &graph);
    // D3104: where a front hit may bind this shape's index morsel rows from its plan (`None`: equal rows only)
    let sites = front.as_ref().and_then(|f| crate::inputs::front::morsel_sites(f, &graph, &tpl.guards, pstmt));
    // filed under the key and bound without the graph (a bitmap walk's or tree's recheck qual is the only reader of
    // BindCx.graph: D411, a tree's rung recipe was missed and its front hit refused); D644: every shape not filed says why
    t.unfiled = match front.as_ref() {
        None if is_instrumented() || crate::inputs::knobs::seam::facts_on() => None,
        None if plan_order => Some("the plan-order run's program (v5 errorhold) is never filed"),
        None => Some("no front walk (a node or field the walk does not key)"),
        // `key` is the program's variant key (road (b) above): a guarded program (a key::decide user) is filed under the
        // front too, as `front_check` keys it, and its front hit binds the plan's exact rows (the front key holds them), on
        // which every guard decides as it did
        // a front hit decides a literal guard on the node it rebuilds at its literal's position: a front-walk node, or a
        // constant decompose built, which `file` keeps only as a `Built` rebuilt from the hit's own plan constant
        // D4786: the front key keys a constant list by its class too: a front hit would bind this program for other bytes
        Some(_) if t.salted => Some("a program filed under its class-keyed lists' bytes is never filed under the front key"),
        Some(f) if !gravity_lower::inputs::key::lits_walked(&tpl.guards, &nodes, &f.nodes) => Some("a literal guard's literal is no node a front hit rebuilds"),
        Some(f) => match b.graph_words.get(&key).map(|e| &e.0) {
            None => Some("no words template under the key"),
            Some(_) if recipes.iter().any(graph_bound) => Some("a recipe binds from the graph (D411)"),
            Some(g) => {
                // V4R-18: a lowering here logged what it baked; a content-key hit did not lower, so every image is taken as baked
                let locators = template_reads(g);
                let needs = crate::inputs::front::needs(f, pstmt, &facts);
                let deps = reads.finish();
                match t.ir {
                    Some(ir) => crate::inputs::front::fill_lowered_reading_est(f, key, &nodes, needs, ir, Some(&locators), est.as_deref(), sites, &deps),
                    None => crate::inputs::front::fill_reading_est(f, key, &nodes, needs, &locators, est.as_deref(), sites, &deps),
                }
                .err()
            }
        },
    };
    bind(mcx, pstmt, params, key, &nodes, (program, tpl, recipes, owner), Some(&graph), est, t, out)
}

/// KEY-PREDICTED-ROWS road (b): the program filed under content key `key` whose estimate decisions `graph`'s estimates
/// make alike (and whose relation guards its relations' pages now, PS1-21 step 1b): the one last filed under `key` when its
/// guards hold, else the one filed for the outcomes those guards decide on this graph (`key::variant`, `file_local`), if any.
fn find(b: &mut Backend, key: ContentKey, graph: &PipelineGraph<'_>, facts: &RelcacheFacts) -> Option<Filed> {
    use gravity_lower::inputs::key::{guards_hold, variant};
    let h = lookup(b, key)?;
    if guards_hold(&h.1.guards, graph, facts) {
        return Some(h);
    }
    lookup(b, ContentKey(variant(key.0, &h.1.guards, Some((graph, facts))))).filter(|v| guards_hold(&v.1.guards, graph, facts))
}

/// `find` in the process-wide tier under its key `sk` (`install` publishes a guarded program under its variant key too):
/// an entry whose recorded dependencies are all current (C3 S3; under this backend's own uncommitted DDL, none of them
/// touched by it: `Deps::current`).
fn shared_find(sk: u128, graph: &PipelineGraph<'_>, facts: &RelcacheFacts) -> Option<SharedProgram> {
    use gravity_lower::inputs::key::{guards_hold, variant};
    let valid = |t: &SharedTpl| t.0.deps.current() || note_stale("shared", &t.0.deps);
    let (program, tpl) = shared_cache().get(sk)?;
    if guards_hold(&tpl.0.guards, graph, facts) {
        return valid(&tpl).then_some((program, tpl));
    }
    shared_cache().get(variant(sk, &tpl.0.guards, Some((graph, facts)))).filter(|(_, t)| guards_hold(&t.0.guards, graph, facts) && valid(t))
}

/// PS1-21 / PS1-26 step 1b: whether `guards`' relation guards (lower key.rs `rel_decide`) decide as they did on their
/// relations' pages now: one page count each, and no context made for guards without one.
pub(crate) fn rels_hold(guards: &[gravity_lower::inputs::key::EstGuard]) -> bool {
    if !gravity_lower::inputs::key::has_rels(guards) {
        return true;
    }
    let ctx = gravity_driver::mem::root::Root::new("GravityRelGuards");
    gravity_lower::inputs::key::rels_hold(guards, &RelcacheFacts::new(ctx.mcx()))
}

/// KEY-PREDICTED-ROWS road (b): `graph`'s planner estimates of `program`'s pipelines (each pipeline's `rows_of` / `build_of` /
/// `end_of`, lowerv4 frame/mechs.rs; `gravity_program::bind_estimates`), `None` when every one equals what the program
/// carries (the admission that lowered it, or a statement of the same estimates).
fn estimates(program: &Program, graph: &PipelineGraph<'_>) -> Option<Box<[PipeEst]>> {
    gravity_program::bind_estimates(&program.pipelines, |j| graph.pipelines.get(j as usize).map(|q| &q.desc))
}

/// D3104: a front hit's estimates of `program`'s pipelines: its variant's (`filed`, else the program's) with each moved
/// index morsel site's rows (`Moved::rows`) on the pipelines that carry its graph pipeline's (`rows_of`), whose rows at
/// their end (D4047) the variant's statement gave and so are dropped; `None` when they come out as the program's (`estimates`).
fn moved_est(program: &Program, filed: Option<&[PipeEst]>, m: &crate::inputs::front::Moved) -> Option<Box<[PipeEst]>> {
    let est: Vec<PipeEst> = program // alloc: Cold
        .pipelines
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let e = filed.and_then(|e| e.get(i).copied()).unwrap_or(p.est());
            m.rows.iter().find(|(j, _)| *j == p.rows_of).map_or(e, |&(_, r)| PipeEst { rows: r, end: 0, ..e })
        })
        .collect();
    let same = program.pipelines.iter().zip(est.iter()).all(|(p, e)| p.est() == *e);
    (!same).then(|| est.into_boxed_slice())
}

/// `a`'s estimates (`GraphWords::est`) when its program is the one they were bound for.
fn bound_est(a: &Admitted) -> Option<&[PipeEst]> {
    let (of, est) = a.graph.as_ref()?.est.as_ref()?;
    (*of == Arc::as_ptr(&a.program) as usize && est.len() == a.program.pipelines.len()).then_some(&est[..])
}

/// Program pipeline `i` of `a`'s program with this statement's estimates (`GraphWords::est`).
pub(crate) fn bound(a: &Admitted, i: usize) -> Option<gravity_program::Pipeline> {
    let p = *a.program.pipelines.get(i)?;
    Some(match bound_est(a).and_then(|e| e.get(i)) {
        Some(&e) => p.with_est(e),
        None => p,
    })
}

/// `a`'s program's pipelines with this statement's estimates (`bound`): its own slice when they are the program's.
pub(crate) fn bound_all(a: &Admitted) -> std::borrow::Cow<'_, [gravity_program::Pipeline]> {
    match bound_est(a) {
        None => std::borrow::Cow::Borrowed(&*a.program.pipelines),
        Some(_) => std::borrow::Cow::Owned((0..a.program.pipelines.len()).filter_map(|i| bound(a, i)).collect()), // alloc: Cold
    }
}

/// An admission's binding from its filed template, the front key's hit and today's path alike: the words, the InitPlan
/// and arm registries, initial pruning, the registry (`graph` for a recipe's plan quals; none on a front hit) and checks.
#[allow(clippy::too_many_arguments)]
fn bind(mcx: Mcx<'static>, pstmt: &'static PlannedStmt<'static>, params: ParamListHandle, key: ContentKey, nodes: &[Node<'static>], (program, tpl, recipes, owner): Filed, graph: Option<&PipelineGraph<'static>>, est: Option<Box<[PipeEst]>>, t: Timing, out: &mut Option<Admitted>) -> PgResult<Result<Timing, Refused>> {
    // C3 S4c: the admission's recording holds its program's reads too (a hit lowered nothing to record them)
    gravity_deps::replay(&tpl.deps);
    let nparams = if params.is_null() { 0 } else { types_portal::params::num_params(params) };
    let mut exprs = shapes::key_exprs_for(nparams);
    if nparams > 0 {
        types_portal::params::with(params, |ps| exprs.stable[..nparams.min(ps.len())].copy_from_slice(&ps[..nparams.min(ps.len())]));
    }
    let window = execexpr::execute_acl_window();
    let bound = bind_words(&tpl.words, nodes, mcx, &mut exprs, key, &program, owner.as_deref().or(program.owner.as_deref()));
    exprs.acl_funcs = window.finish().into_iter().map(|f| (f, shapes::AclObject::Function)).collect(); // alloc: Program
    // the program's own calls, rechecked by `eval_keys` at every execution, hit or miss (item 4)
    exprs.acl_funcs.extend_from_slice(&tpl.funcs); // alloc: Program
    let (mut words, volatile) = match bound? {
        Ok(w) => w,
        Err(r) => return Ok(Err(r)),
    };
    // V4R-205: the declared M4 protocols over this plan, at every admission (a cached program meets a new plan here)
    if let Err(r) = crate::ops::modifytable::statement::admit(program.v4.as_deref(), pstmt) {
        return Ok(Err(r));
    }
    for i in tpl.inits.iter() {
        let registry = match registry_from(&i.recipes, None, None, None, mcx, &mut exprs)? {
            Ok(r) => r,
            Err(r) => return Ok(Err(r)),
        };
        words.inits.push(InitRun { program: i.program.clone(), registry, word: i.word as usize, exists: i.exists, param: i.param }); // alloc: Program
    }
    for x in tpl.arms.iter() {
        let registry = match registry_from(&x.recipes, None, None, None, mcx, &mut exprs)? {
            Ok(r) => r,
            Err(r) => return Ok(Err(r)),
        };
        words.arms.push(ArmRun { program: x.program.clone(), registry, skip: false }); // alloc: Program
    }
    // the template's merge keys and guards are read through it (`GraphWords::tpl`), shared, not copied
    words.rels_decide = gravity_lower::inputs::key::has_rels(&tpl.guards);
    words.held = tpl.held.clone();
    words.no_proc = tpl.no_proc;
    words.est = est.map(|e| (Arc::as_ptr(&program) as usize, e));
    if let Some(index) = tpl.prune {
        // initial partition pruning: the arms ruled out under this execution's parameters are skipped; the main (subplan 0) always runs (its quals hold)
        let valid = crate::ops::append::prune::initial_valid(pstmt, index as usize, params)?;
        for (i, arm) in words.arms.iter_mut().enumerate() {
            arm.skip = !crate::ops::append::prune::member(&valid, i + 1);
        }
        if tpl.exec_prune {
            words.prune = Some(crate::ops::append::prune::ExecPrune { index, nsubplans: (words.arms.len() + 1) as u16, params });
        }
    }
    // D3663: a relation set pruned at the executor start (the statement's, an arm's or an InitPlan's) reads the parameters
    let set_prune = |p: &Arc<Program>| p.sites.iter().any(|d| d.set_prune.is_some());
    let sets = set_prune(&program) || words.arms.iter().any(|x| set_prune(&x.program)) || words.inits.iter().any(|x| set_prune(&x.program));
    words.prune = v4_prune_handle(program.v4.as_deref(), sets, words.prune, params);
    let mut registry = match registry_from(&recipes, graph, graph.is_none().then_some(pstmt), Some(pstmt), mcx, &mut exprs)? {
        Ok(r) => r,
        Err(r) => return Ok(Err(r)),
    };
    registry.set_owner(owner);
    let checks = match tpl.checks.is_empty() {
        true => None,
        false => match bind_checks(&tpl.checks, nodes, check_inits(program.v4.as_deref())) {
            Ok(c) => Some(c),
            Err(r) => return Ok(Err(r)),
        },
    };
    // D1687: the program write's checks, and a writer beside it its own checks and MERGE subcommands
    if let Err(r) = crate::ops::modifytable::beside::set(&mut registry, &program, pstmt, &tpl.checks, checks) {
        return Ok(Err(r));
    }
    // the admission holds the template from here (its last use above): `GraphWords::merge` / `rels` read it
    words.tpl = Some(tpl);
    let nk = words.words.len();
    let cmd = program.write.cmd;
    let out_natts = program.out.len();
    // D3516: a write's key words run when it runs, under its own snapshot (PortalRunMulti takes it at Execute), not at Bind
    // under an older one: a STABLE key over a table another session changed in between named the old row
    let eval_at_execute = volatile || cmd != gravity_program::Cmd::Select;
    // D4454: written once into the caller's slot (the admission layers and the server read it there; by value, each `?` and
    // closure return above copied it)
    *out = Some(Admitted { shape: SHAPE, program, registry, key_exprs: exprs, nkparams: nk, keys: [KeySrc::Const(0); SHAPE_PARAMS], cmd, out_natts, eval_at_execute, sort_keys: [0; 4], nsort: 0, walk: None, pstmt: Some(pstmt), graph: Some(words), params: ParamListHandle::NULL, query_env: types_portal::QueryEnvHandle::NULL, timezone: None, memo: Default::default(), keep: None, ident: None, roads: Default::default() });
    Ok(Ok(t))
}

/// A lowered program through the backend, with its recipes (an InitPlan program of a composed statement).
fn compile_program(l: &gravity_lower::Lowered, graph: &PipelineGraph<'static>, pstmt: &'static PlannedStmt<'static>, mcx: Mcx<'static>) -> PgResult<Result<(Arc<Program>, Arc<RecipeSet>), Refused>> {
    let codes = match backend(&l.ir, &l.funcs, l.key, &crate::inputs::knobs::tier0::site_rows(pstmt, &l.tagged))? {
        Ok(c) => c,
        Err(r) => return Ok(r),
    };
    let mut program = gravity_lower::assemble(l, codes); // alloc: Program
    serial_over_index_sites(&mut program);
    serial_over_temp_sources(&mut program, mcx)?;
    let recipes: Arc<RecipeSet> = match registry(&program, graph, pstmt, mcx)? {
        Ok(r) => r,
        Err(r) => return Ok(Err(r)),
    };
    Ok(Ok((Arc::new(program), recipes))) // alloc: Program
}

/// The backend compile of a lowered program's functions (D2909): its stop points read the lowerer's interrupt test
/// (the facade's `cancelpoll::interrupts::pending`), so a cancel, a statement_timeout or a terminate stops a long compile within one block's
/// or one pass's work, and the interrupt is PostgreSQL's own error here. D3811: a wait for another thread's compile of the
/// same module (the compile pool's, another backend's) reads the same test every 10 ms and ends the same way. One that raised nothing (a barrier, a catchup)
/// compiles again, as the lowering does (`install`); a failure while an interrupt is pending takes it first; any other
/// failure refuses "backend".
/// `sites`: the program's tagged lines with their nodes' planned rows (knobs/tier0.rs `site_rows`), tier 0's weighing.
fn backend<T>(ir: &gravity_backend::gravity_ir::Program, funcs: &[gravity_backend::gravity_ir::Func], key: Option<u128>, sites: &[(u32, u64)]) -> PgResult<Result<Vec<gravity_program::Code>, Result<T, Refused>>> {
    loop {
        cancelpoll::interrupts::check()?;
        match gravity_backend::mapped_cached_keyed(ir, funcs, key.map(gravity_backend::cache::CodeKey), sites) {
            Ok((c, _)) => return Ok(Ok(c)),
            Err(e) if e.message() == gravity_backend::cache::INTERRUPTED => {}
            Err(e) => {
                cancelpoll::interrupts::check()?;
                return Ok(Err(refuse("backend", e.message())));
            }
        }
    }
}

/// The word plan's named refusal: a statement past `limits::PARAM_WORDS` words (a sanity bound; its bindings are sized by
/// the statement's own words).
const PAST_WORDS: &str = "more than 32768 parameter words (the word plan's bound, `limits::PARAM_WORDS`)";
/// D4197: an InitPlan or arm program keyed by a user key: its road lays no `keys.user` run (`runner::run_inits`, an arm's open).
const SIDE_USER_KEYS: &str = "a user key in an InitPlan or arm program (its road lays no keys.user run)";
const _: () = assert!(limits::PARAM_WORDS == 32768, "PAST_WORDS names the bound");

/// The miss path: the lowering, the backend, the template, the program filed under the key with its recipes (one relation open per site here, none on a hit).
#[allow(clippy::too_many_arguments)]
fn install(b: &mut Backend, pcx: &plan::PlanScope<'_>, arena: ArenaRef, graph: &PipelineGraph<'static>, facts: &RelcacheFacts, pstmt: &'static PlannedStmt<'static>, mcx: Mcx<'static>, key: ContentKey, nodes: &[Node<'static>], t: &mut Timing, plan_deps: Option<&gravity_deps::Deps>, shared: Option<u128>, salt: Option<ContentKey>) -> PgResult<Result<Filed, Refused>> {
    let t0 = Instant::now();
    // C3: what this lowering reads from the catalog (gravity_deps: the caches note each read while it is armed)
    let recording = gravity_deps::Recording::start();
    let (plan, composed) = loop {
        let mut plan = Located::new(arena);
        // the compiler's EXECUTE gates are deferred: the check is every execution's (`eval_keys`)
        let (composed, guards) = {
            let _defer = gravity_expr::nodes::func::acl_defer();
            gravity_lower::inputs::key::guarded(|| gravity_lower::lower_composed(arena, pcx.mcx(), graph, facts, &gravity_expr::PgCatalog, &mut plan, gravity_lower::LowerOpts { instrument: is_instrumented(), publish: is_publishing(), shape: Some(key.0) }))
        };
        match composed {
            // a pending interrupt stopped the lowerer (D231): a cancel is the statement's error; one that raised nothing, lower again
            Err(r) if r.is_interrupted() => cancelpoll::interrupts::check()?,
            c => break (plan, (c, guards)),
        }
    };
    t.plan = pcx.stats();
    let (composed, mut guards) = composed;
    // the literals the lowering decided on, by their key-walk positions (a hit's statement decides on its own there)
    gravity_lower::inputs::key::place_lits(&mut guards, nodes);
    // planprod P6-b: the template's root, the lowering statement's until the backend's cache files it (`adopt_hosted`);
    // declared before every list made in it, so an early return drops the lists first
    let towner = memledger::charge::statement_owner();
    let mut troot = mcx::SendRoot::new_owned_small(GRAPH_ROOT, towner);
    let funcs = program_funcs(graph, nodes, pstmt, raw::tpl_list(&mut troot))?;
    let composed = match composed {
        Ok(c) => c,
        // past the word plan's bound whatever step met it first (a by-reference literal's fold, a protocol's words) refuses by it
        Err(_) if plan.seq.overflow => return Ok(refuse("lower", PAST_WORDS)),
        Err(r) => {
            if r.native_rel != 0 {
                gravity_hosts::ops::scan::am_rows::shape_rows(r.native_rel);
            }
            return Ok(refuse("lower", r.to_string())); // alloc: Cold
        }
    };
    let lowered = &composed.main;
    // the IR hash feeds only the shadow check (`front_check`): taken under it, else the variant is filed unhashed (PERF-COLD
    // 09-25: ~55 us of every first lowering, its derived Hash through the byte-wise Fnv)
    t.ir = Some(if crate::inputs::front::check_on() { crate::inputs::front::ir_hash(lowered) } else { crate::inputs::front::IR_UNHASHED });
    if plan.seq.overflow {
        return Ok(refuse("lower", PAST_WORDS));
    }
    // D2834: a deferred extern parameter's value taken ahead of its evaluation (a key, a count, a table) refuses by name
    if plan.seq.ahead {
        return Ok(refuse("lower", gravity_lower::DEFERRED_AHEAD));
    }
    t.lower = us(t0);
    t.ir_bytes = lowered.ir.bytes() + composed.inits.iter().map(|i| i.lowered.ir.bytes()).sum::<usize>() + composed.arms.iter().map(|a| a.ir.bytes()).sum::<usize>();
    let t0 = Instant::now();
    // the InitPlan programs first (rung 12): each its own program and recipes, the word it fills from the plan
    let mut inits: TplVec<InitTpl> = raw::tpl_list(&mut troot);
    tpl_room(&mut inits, composed.inits.len(), false, GRAPH_ROOT);
    if let Some(why) = composed.inits.iter().map(|i| &i.lowered).chain(composed.arms.iter()).find_map(|l| crate::output_cid::side_program_refusal(&l.descs.write)) {
        return Ok(refuse("lower", why));
    }
    // D4197: a side program runs over the main program's key words with no protocol of its own laid (`runner::run_inits`,
    // an arm's open): a user key's classes (`keys.user`) would have no run behind their word
    if composed.inits.iter().map(|i| &i.lowered).chain(composed.arms.iter()).any(|l| l.descs.v4.protos.iter().any(|p| p.kind == gravity_program::protos::ProtoKind::UserKeys)) {
        return Ok(refuse("lower", SIDE_USER_KEYS));
    }
    for i in composed.inits.iter() {
        let Some(word) = plan.seq.words.iter().position(|w| matches!(w, WordSrc::Exec { id, .. } if *id == i.param)) else { return Ok(refuse("lower", "an InitPlan without its exec word")) };
        let exists = matches!(i.link, gravity_decompose::ops::shared::subplan::SubLink::Exists);
        if !exists && !i.lowered.descs.out.first().is_some_and(|c| c.typbyval) {
            return Ok(refuse("lower", "a by-reference InitPlan cell (its copy out of the init program's region)"));
        }
        let (program, recipes) = match compile_program(&i.lowered, graph, pstmt, mcx)? {
            Ok(p) => p,
            Err(r) => return Ok(Err(r)),
        };
        tpl_push(&mut inits, InitTpl { program, recipes, word: word as u32, exists, param: i.param }, GRAPH_ROOT);
    }
    let mut arms: TplVec<ArmTpl> = raw::tpl_list(&mut troot);
    tpl_room(&mut arms, composed.arms.len(), false, GRAPH_ROOT);
    for a in composed.arms.iter() {
        let (program, recipes) = match compile_program(a, graph, pstmt, mcx)? {
            Ok(p) => p,
            Err(r) => return Ok(Err(r)),
        };
        tpl_push(&mut arms, ArmTpl { program, recipes }, GRAPH_ROOT);
    }
    let codes = match backend(&lowered.ir, &lowered.funcs, lowered.key, &crate::inputs::knobs::tier0::site_rows(pstmt, &lowered.tagged))? {
        Ok(c) => c,
        Err(r) => return Ok(r),
    };
    t.backend = us(t0);
    let (checks, names) = match check_template(graph, nodes, raw::tpl_list(&mut troot)) {
        Ok(c) => c,
        Err(r) => return Ok(Err(r)),
    };
    // the names' blocks the allocator just counted on this thread move to the root's owner
    let names = {
        use memledger::charge::{release_of, Charge, Door};
        let mut c = Charge::open(towner, release_of(towner), memledger::Kind::Region, Door::Heap, 0);
        c.commit(names);
        pgsync::Mutex::new(c)
    };
    let mut tpl = match template(&plan, nodes, raw::tpl_list(&mut troot)) {
        Ok(words) => {
            let mut merge = raw::tpl_list(&mut troot);
            tpl_room(&mut merge, composed.merge.len(), false, GRAPH_ROOT);
            merge.extend_from_slice(&composed.merge);
            let mut kept = raw::tpl_list(&mut troot);
            tpl_room(&mut kept, guards.len(), false, GRAPH_ROOT);
            kept.extend_from_slice(&guards);
            GraphTpl { words, funcs, inits, arms, prune: None, exec_prune: false, merge, names, own: pgsync::Mutex::new(memledger::charge::Charge::NONE), checks, held: Arc::new(crate::runner::held::plan(pstmt)), no_proc: gravity_decompose::ops::sort::holds_no_proc(graph), guards: kept, deps: gravity_deps::Deps::default(), db: init_small::globals::MyDatabaseId(), published: core::sync::atomic::AtomicBool::new(false), root: pgsync::Mutex::new(troot) } // alloc: Program
        }
        Err(r) => return Ok(Err(r)),
    };
    let mut program = gravity_lower::assemble(lowered, codes); // alloc: Program
    serial_over_index_sites(&mut program);
    serial_over_temp_sources(&mut program, mcx)?;
    let recipes: Arc<RecipeSet> = match registry(&program, graph, pstmt, mcx)? {
        Ok(r) => r,
        Err(r) => return Ok(Err(r)),
    };
    tpl.deps = recording.finish();
    deps_cover(graph, &tpl.deps)?;
    // the plan's own lists beside what the lowering read (PostgreSQL's plancache retires the plan by them)
    if let Some(plan) = plan_deps {
        tpl.deps.absorb(plan);
    }
    let tpl = Arc::new(tpl); // alloc: Program
    let no_cache = lowered.descs.no_cache || composed.inits.iter().map(|i| &i.lowered).chain(composed.arms.iter()).any(|l| l.descs.no_cache);
    let no_cache_why = if lowered.descs.no_cache { lowered.descs.no_cache_why } else { composed.inits.iter().map(|i| &i.lowered).chain(composed.arms.iter()).find(|l| l.descs.no_cache).map_or("", |l| l.descs.no_cache_why) };
    let list_only = core::iter::once(&lowered.descs).chain(composed.inits.iter().map(|i| &i.lowered.descs)).chain(composed.arms.iter().map(|a| &a.descs)).all(|d| !d.no_cache || d.no_cache_why == "list");
    // D4786: filed under its lists' bytes (below), as a program that is not `no_cache` is under its key
    let filed = !no_cache || (salt.is_some() && list_only);
    // D3438: a program whose host state a template stands for (lowerv4 `hir::template`) is filed without it where the
    // process-wide tier may take it; the owner is this backend's, filed beside its recipes
    let owner = match program.owner.as_deref().and_then(gravity_lowerv4::hir::template) {
        Some(host) if shared.is_some() && filed && tpl.inits.is_empty() && tpl.arms.is_empty() && !private_set(&program) => {
            program.hosted = Some(host);
            program.owner.take()
        }
        _ => None,
    };
    let program = Arc::new(program); // alloc: Program
    // v5 identity (day 0): with the dump on, every install's programs, descriptors and word template (knobs/dump.rs)
    if let Some(dir) = gravity_backend::dump::dir() {
        let finals: Vec<&[gravity_program::Pipeline]> = core::iter::once(&program.pipelines[..]).chain(tpl.inits.iter().map(|i| &i.program.pipelines[..])).chain(tpl.arms.iter().map(|a| &a.program.pipelines[..])).collect(); // alloc: Cold
        crate::inputs::knobs::dump::write_install(&dir, &composed, &finals, &tpl.words, recipes.len());
    }
    if no_cache {
        NO_CACHE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if shared.is_some() {
            t.tier = match no_cache_why {
                "data" => "miss:no_cache.data",
                "list" => "miss:no_cache.list",
                "errorhold" => "miss:no_cache.errorhold",
                _ => "miss:no_cache",
            };
        }
    }
    let h = (program, tpl, recipes, owner);
    // D4786: a program every never-filed part of which read a class-keyed list (nothing else statement-specific) is filed
    // under the key salted with the lists' bytes (`list_salted`): in this backend and, as any other, process-wide
    let (key, shared) = match salt.filter(|_| no_cache && list_only) {
        Some(sk) => {
            (t.salted, t.tier) = (true, "local:list-filed");
            (sk, shared.map(|_| sk.0))
        }
        None if no_cache => return Ok(Ok(h)),
        None => (key, shared),
    };
    file_local(b, key, &h);
    // V4R-35: published when no program of it carries host state and the stamp did not move while it lowered
    if shared.is_some() {
        // D3866 (step 2): a read under this backend's own uncommitted DDL files nothing (`shared_key`'s publish stamp)
        t.tier = match private_tier(&h) {
            Some(why) => why,
            None if gravity_hosts::ops::scan::pgrc::residency::private_build() => "own-ddl",
            None => "raced",
        };
    }
    // C3 S3: published when every dependency the lowering and its plan hold still has the generation read (an
    // invalidation while it lowered retires it here, as it would at the next hit)
    if let Some(sk) = shared.filter(|_| shareable(&h) && gravity_hosts::ops::scan::pgrc::residency::shared_tier(true) && h.1.deps.current()) {
        t.tier = if t.salted { "published:list" } else { "published" };
        sweep_superseded();
        let bytes = h.0.code_bytes() + h.0.arena_used() + h.0.hosted.as_deref().map_or(0, gravity_lowerv4::hir::template_bytes);
        // KEY-PREDICTED-ROWS road (b): a guarded program is published under its variant key (`shared_find`), and under `sk`
        // while that holds none (the guards a later admission decides again; the first filed stays)
        let at = gravity_lower::inputs::key::variant(sk, &h.1.guards, None);
        let live = |v: &SharedProgram| v.1 .0.deps.current();
        let under_sk = at != sk && shared_cache().insert_over(sk, (h.0.clone(), SharedTpl(h.1.clone())), bytes, live).1;
        let ((program, tpl), filed) = shared_cache().insert_over(at, (h.0.clone(), SharedTpl(h.1.clone())), bytes, live);
        // C2: a program the tier holds is the program caches' memory, on their ledger account until its last drop; so
        // is its host state's template (planprod P6: lowerv4 `hir::publish`)
        if under_sk || filed {
            h.0.file_on_ledger();
            if let Some(t) = h.0.hosted.as_deref() {
                gravity_lowerv4::hir::publish(t);
            }
            // planprod P6-b: and the program's template (`GraphTpl`'s root)
            h.1.publish();
        }
        // the tier keeps the entry another backend filed first; one whose estimate decisions differ on this graph is not
        // taken: this backend runs, and keeps, its own (D3438: so is one of host state, whose owner is not this one's)
        if !Arc::ptr_eq(&program, &h.0) && (!gravity_lower::inputs::key::guards_hold(&tpl.0.guards, graph, facts) || program.hosted.is_some() || h.0.hosted.is_some()) {
            return Ok(Ok(h));
        }
        return Ok(Ok((program, tpl.0, h.2, h.3)));
    }
    Ok(Ok(h))
}

/// C3 S1 (check mode, `progcache_check_on`): every relation the graph scans, reads by index or writes is in its lowering's
/// recorded dependencies: the recorder sees each relation the lowering opens; and no catalog read happened inside the code
/// generator (`gravity_deps::catalog_free`, any thread) since the last check.
fn deps_cover(graph: &PipelineGraph<'_>, deps: &gravity_deps::Deps) -> PgResult<()> {
    if !progcache_check_on() {
        return Ok(());
    }
    if let Some((d, n)) = gravity_deps::take_free_read() {
        return Err(Box::new(types_error::PgError::error(format!("gravity deps: Reason::Bug: {n} catalog read(s) inside the code generator, the first {d:?}")).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR))); // alloc: Cold
    }
    let missed = graph
        .pipelines
        .iter()
        .flat_map(|p| {
            let index = if let Source::Index { indexid, .. } = &p.source { Some(*indexid) } else { None };
            let writes = p.ops.iter().filter_map(|o| if let Op::Write(w) = o { Some(w.site.relation) } else { None });
            p.source.relation().map(|(_, r, _)| r).into_iter().chain(index).chain(writes)
        })
        .find(|&r| r != 0 && !deps.contains(gravity_deps::Dep::Rel(r)));
    match missed {
        None => Ok(()),
        Some(r) => Err(Box::new(types_error::PgError::error(format!("gravity deps: Reason::Bug: the lowering's recorded dependencies miss relation {r}")).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR))), // alloc: Cold
    }
}

/// Files a program, its template and recipes under this backend's key (its own cache and both memos); KEY-PREDICTED-ROWS
/// road (b): a guarded program also under its variant key, beside the programs of the key's other outcomes (`find`).
fn file_local(b: &mut Backend, key: ContentKey, h: &Filed) {
    file_at(b, key, h);
    let at = ContentKey(gravity_lower::inputs::key::variant(key.0, &h.1.guards, None));
    if at != key {
        file_at(b, at, h);
    }
}

fn file_at(b: &mut Backend, key: ContentKey, h: &Filed) {
    // KEY-PREDICTED-ROWS road (b): front variants filed for an earlier program under the key never hit this one
    crate::inputs::front::forget(key);
    file_here(b, key, h);
    level_file(key, h);
}

fn file_here(b: &mut Backend, key: ContentKey, h: &Filed) {
    adopt_hosted(h);
    b.cache.install(key, h.0.clone());
    if b.graph_words.len() >= MEMO_ENTRIES {
        b.graph_words.clear();
        b.graph_registry.clear();
    }
    b.graph_words.insert(key, (h.1.clone(), core::cell::Cell::new(gravity_deps::UNCHECKED))); // alloc: Program
    b.graph_registry.insert(key, (h.2.clone(), h.3.clone())); // alloc: Program
}

/// planprod P4: the backend's caches hold what `h` files from here, so its hosted owners (the program's, its InitPlans' and
/// arms', and the one filed beside the recipes) are the session's: each owner's product root and the contexts under it
/// (lowerv4 `hir::adopt`). planprod P6-b: so is the template's root (`GraphTpl::adopt`), unless the program caches hold it.
/// Off a backend nothing moves.
fn adopt_hosted(h: &Filed) {
    let acct = memledger::current();
    if !acct.is_backend() {
        return;
    }
    let session = memledger::Owner::Session(acct);
    let programs = core::iter::once(&h.0).chain(h.1.inits.iter().map(|i| &i.program)).chain(h.1.arms.iter().map(|a| &a.program));
    for o in programs.filter_map(|p| p.owner.as_deref()).chain(h.3.as_deref()) {
        gravity_lowerv4::hir::adopt(o, session);
    }
    // planprod P6-b: the template's root too (a published one stays the program caches'); P6-c1: and the recipe lists'
    h.1.adopt(session);
    for r in core::iter::once(&h.2).chain(h.1.inits.iter().map(|i| &i.recipes)).chain(h.1.arms.iter().map(|a| &a.recipes)) {
        r.adopt(session);
    }
}

/// D3881: what the statement levels of one thread share. Each level files what it lowers in its own caches (the program
/// cache, `graph_words`, `graph_registry`), so a level past the first started empty, and a body nested N deep was decomposed
/// and keyed once per depth (a deep level is built afresh after each run of its caller, reentry `Above`). A static
/// level's filing is kept here too, and a miss at any level takes it: the program and its templates as filed, the host state
/// rebuilt for the taking level from the program's template (D3438 `hir::rebuild`), never shared. A deep level files nothing
/// here: its recipes were built in its own context, freed with the level. Dropped whole at a flush (as every level's caches)
/// and when full; an entry whose recorded reads moved is not taken (`level_take`).
struct LevelTier {
    flush: u64,
    filed: std::collections::HashMap<ContentKey, (Arc<Program>, Arc<GraphTpl>, Arc<RecipeSet>)>,
}

thread_local! {
    // ManuallyDrop: never dropped by the thread's TLS destructor, after the contexts its recipes reach (`level_drop`)
    static LEVEL_TIER: core::cell::RefCell<core::mem::ManuallyDrop<Option<LevelTier>>> = const { core::cell::RefCell::new(core::mem::ManuallyDrop::new(None)) }; // alloc: Program
}

/// A filing other levels may take: no program of it holds host state of its own (a hosted one is filed without it, its
/// template standing for it) and no recipe holds a compiled qual.
fn levels_share(h: &Filed) -> bool {
    let unowned = |p: &Program| p.owner.is_none();
    let plain = |r: &[RecipeTpl]| r.iter().all(|t| match t {
        RecipeTpl::Index(s) | RecipeTpl::Bitmap(s, ..) => s.unfiltered(),
        _ => true,
    });
    unowned(&h.0) && !private_set(&h.0) && (h.3.is_none() || h.0.hosted.is_some()) && plain(&h.2) && h.1.inits.iter().all(|i| unowned(&i.program) && plain(&i.recipes)) && h.1.arms.iter().all(|a| unowned(&a.program) && plain(&a.recipes))
}

fn level_tier<R>(f: impl FnOnce(&mut LevelTier) -> R) -> R {
    LEVEL_TIER.with(|t| {
        let mut t = t.borrow_mut();
        let flush = crate::admit::reentry::flush_gen();
        let tier = t.get_or_insert_with(|| LevelTier { flush, filed: std::collections::HashMap::new() });
        if tier.flush != flush {
            tier.filed.clear();
            tier.flush = flush;
        }
        f(tier)
    })
}

fn level_file(key: ContentKey, h: &Filed) {
    if !crate::admit::reentry::current().is_some_and(|d| d < crate::admit::reentry::MAX_DEPTH) || !levels_share(h) {
        return;
    }
    level_tier(|t| {
        if t.filed.len() >= MEMO_ENTRIES {
            t.filed.clear();
        }
        t.filed.insert(key, (h.0.clone(), h.1.clone(), h.2.clone())); // alloc: Program
    });
}

fn level_take(key: ContentKey) -> Option<Filed> {
    // C3 S4b: while its lowering's recorded dependencies are current, as a level's own filing (`lookup`)
    let (program, tpl, recipes) = level_tier(|t| t.filed.get(&key).filter(|f| f.1.deps.current() || note_stale("level", &f.1.deps)).cloned())?;
    let owner = match program.hosted.as_ref() {
        Some(host) => Some(gravity_lowerv4::hir::rebuild(host)?),
        None => None,
    };
    Some((program, tpl, recipes, owner))
}

leader_tls::leader_tls! {
    thread_local! {
        /// C3: why this admission's program lookup found its filing retired (`note_stale`), for the report's `stale=`.
        static STALE: core::cell::RefCell<Option<String>> = const { core::cell::RefCell::new(None) };
    }
}

/// C3: a filing under the admission's key whose recorded dependencies moved (tier `tier`): the first cause
/// (gravity_deps `Deps::moved`), kept for the report (the retire rate's attribution). False, for the caller's filter.
#[cold]
fn note_stale(tier: &str, deps: &gravity_deps::Deps) -> bool {
    STALE.with(|s| {
        let mut s = s.borrow_mut();
        if s.is_none() {
            *s = deps.moved().map(|w| format!("{tier}: {w}")); // alloc: Cold
        }
    });
    false
}

/// D3881: a plan cache reset: every level's filings go (`Backend::drop_plans`), these with them.
pub(crate) fn level_forget() {
    let _ = LEVEL_TIER.try_with(|t| {
        if let Ok(mut t) = t.try_borrow_mut() {
            if let Some(t) = t.as_mut() {
                t.filed.clear();
            }
        }
    });
}

/// D3881: the session's end, before the static levels go (their contexts hold the recipes filed here).
pub(crate) fn level_drop() {
    let _ = LEVEL_TIER.try_with(|t| {
        if let Ok(mut t) = t.try_borrow_mut() {
            **t = None;
        }
    });
}

/// The process-wide program tier (PROGCACHE, V4R-35): programs, templates and recipes by a catalog-stamped
/// graph key, shared read-only by every backend behind its own cache. `SHARED_ENTRIES` / `SHARED_BYTES` bound it (LRU).
/// The recipes are not in it: a `SiteRecipe` holds this backend's scan keys and function-manager frames (`fn_extra`).
pub type SharedProgram = (Arc<Program>, SharedTpl);

/// A filed program, its template and recipes, and (D3438) this backend's host state for a program filed without one
/// (`Program::hosted`): `None` for any other, whose owner (if any) is its own `Program::owner`.
type Filed = (Arc<Program>, Arc<GraphTpl>, Arc<RecipeSet>, Option<std::rc::Rc<dyn core::any::Any>>);

/// A template the tier may hold: no InitPlan or arm (theirs carry recipes); its words, functions, merge keys and check
/// options are plain data (`_TPL_PARTS`).
#[derive(Clone)]
pub struct SharedTpl(Arc<GraphTpl>);

// Its `Send + Sync` claim and the contract behind it: raw.rs.

const _TPL_PARTS: () = {
    const fn ok<T: Send + Sync>() {}
    ok::<WordTpl>();
    ok::<(Oid, shapes::AclObject)>();
    ok::<Option<u32>>();
    ok::<gravity_lower::mergearms::MergeKey>();
    ok::<CheckTpl>();
    ok::<gravity_lower::inputs::key::EstGuard>();
    ok::<gravity_deps::Deps>();
    ok::<pgsync::Mutex<mcx::SendRoot>>();
};
/// The process-wide program tier's bounds (entries and code + arena bytes), split over its shards; past either, the least
/// recently used entry is evicted and a later admission compiles again. Capacity bounds, not thresholds.
pub const SHARED_ENTRIES: usize = 4096;
pub const SHARED_BYTES: usize = 256 << 20;
static SHARED: pgsync::OnceLock<gravity_program::cache::SharedCache<SharedProgram>> = pgsync::OnceLock::new();

pub fn shared_cache() -> &'static gravity_program::cache::SharedCache<SharedProgram> {
    SHARED.get_or_init(|| gravity_program::cache::SharedCache::new(SHARED_ENTRIES, SHARED_BYTES))
}

/// D4579: DROP DATABASE (l2cache's forget-database hook, `init_seams`): the tier lets go every program lowered in `db`.
/// No relcache message names a dropped database's relations, so nothing else would: a later database runs the same
/// statements under other keys. The programs' bytes leave at their last holder's drop (a gang executor's, retired with
/// the database); the code cache's modules only they ran go once unheld (`drop_shared_where`).
pub fn forget_database(db: Oid) {
    // D4632: and the memory history's keys of the dropped database
    gravity_driver::mem::feedback::forget_db(db);
    if SHARED.get().is_some() {
        drop_shared_where(|v| v.1 .0.db == db);
        gravity_backend::cache::evict_doomed();
    }
}

/// D4579: the earliest `sweep_superseded` runs again (`pg_clock::mono_ns`).
static SWEEP_AT_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// D4579: `l2cache::committed_moves` as the last sweep read it before judging the tier: while the count stands, every
/// program the sweep kept still stands, and the next filing skips the sweep.
static SWEPT_MOVES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// D4579: at most one superseded sweep a second, at a filing.
const SWEEP_EVERY_NS: u64 = 1_000_000_000;

/// D4579: before a filing, at most once a second and only when a committed generation moved since the last sweep
/// (`l2cache::committed_moves`): the tier lets go every program whose recorded reads are superseded in
/// the database it was lowered in (a dropped or rewritten relation, a truncated one's storage, a changed catalog row).
/// None of them can be served again (`Deps::superseded_in`): its statement files a new program under its own key
/// (another relation's, after a DROP and CREATE) or replaces it under the same. Then the code cache's doomed modules
/// nothing holds any more go (a program a gang executor or a running statement held at its removal).
fn sweep_superseded() {
    use core::sync::atomic::Ordering::Relaxed;
    let now = pg_clock::mono_ns();
    let at = SWEEP_AT_NS.load(Relaxed);
    if now < at || SWEEP_AT_NS.compare_exchange(at, now + SWEEP_EVERY_NS, Relaxed, Relaxed).is_err() {
        return;
    }
    // nothing committed moved since the last sweep judged the tier: every program it kept still stands. The count is
    // read before the judging: a move during it moves the count past this, and a later filing sweeps again.
    let moves = l2cache::committed_moves();
    if moves != SWEPT_MOVES.swap(moves, Relaxed) {
        drop_shared_where(|v| v.1 .0.deps.superseded_in(v.1 .0.db));
    }
    // the doomed modules a holder let go since (nothing doomed: one load)
    gravity_backend::cache::evict_doomed();
}

/// D4579: the tier lets go the programs `doomed` names; each module a removed program ran from is marked doomed in the
/// code cache, for the caller's `evict_doomed` (it goes then when the tier's was the last hold). The programs removed.
fn drop_shared_where(doomed: impl Fn(&SharedProgram) -> bool) -> usize {
    // one pass per shard: a pass takes a whole shard's entries (the tier's bound over its shards)
    shared_cache().remove_where::<{ SHARED_ENTRIES / gravity_program::cache::SHARDS }>(doomed, |(program, tpl)| {
        gravity_backend::cache::doom_mappings(program.code_mappings());
        // D4762: and the modules its tier-0 codes promote from, which a code that never promoted names no mapping of
        gravity_backend::cache::doom_modules(program.code_modules());
        drop((program, tpl));
    })
}

/// The process-wide tier's key (C3 S4b): the admission's own key `key` (`tier_key`: one key for both tiers), with
/// `pgrust.gravity_shared_programs` on. An entry is served while its recorded dependencies are current (`shared_find`).
/// `None`: this admission neither reads nor publishes shared entries. D3866 (step 2): `publish` false keys a READ, which
/// a backend under its own uncommitted DDL may make; the entry it finds is served only when that DDL touches none of its
/// dependencies.
fn shared_key(key: ContentKey, publish: bool) -> Option<u128> {
    if gravity_hosts::inputs::custom_guc_text("pgrust.gravity_shared_programs").is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "off" | "false" | "0" | "no")) {
        return None;
    }
    if !gravity_hosts::ops::scan::pgrc::residency::shared_tier(publish) {
        return None;
    }
    Some(key.0)
}

/// C3 S3: the plan's own dependency lists, as PostgreSQL's plancache keeps them (setrefs): the range table's relations
/// (D2684: the statement holds them locked), the index sources the graph reads, and the user functions and domains the
/// plan names (`invalItems`, D3901). Not the rest of `relationOids`, the regclass constants: a constant's relation is a
/// word the program binds per execution, never read by the lowering (a read of it would be recorded), so each literal's
/// relation split the key and retired the program at that relation's DDL (C3 S4b, the realapp retire rate: Rails's
/// `WHERE i.indrelid = '"t"'::regclass` introspection lowered once per table, 1760 more lowerings than the epoch scheme).
fn plan_objects(pstmt: &PlannedStmt<'_>, graph: &PipelineGraph<'_>, mut each: impl FnMut(gravity_deps::Dep)) {
    let tables = pstmt.rtable.iter().filter_map(|n| n.as_range_tbl_entry()).map(|r| r.relid);
    let indexes = graph.pipelines.iter().filter_map(|p| if let Source::Index { indexid, .. } = &p.source { Some(*indexid) } else { None });
    for r in tables.chain(indexes).filter(|&r| r != 0) {
        each(gravity_deps::Dep::Rel(r));
    }
    for i in pstmt.invalItems.iter().filter_map(|n| n.as_plan_inval_item()) {
        each(gravity_deps::Dep::Cat { cache: i.cacheId as i16, hash: i.hashValue });
    }
}

/// `plan_objects`, each at its generation now: the seed of a lowering's recorded dependencies.
fn plan_deps(pstmt: &PlannedStmt<'_>, graph: &PipelineGraph<'_>) -> gravity_deps::Deps {
    let mut d = gravity_deps::Deps::default();
    plan_objects(pstmt, graph, |o| d.add(o));
    d
}

/// `plan_objects` without their generations: their identity is in the program tiers' key (`tier_key`; D3900: two plans
/// over different relations whose graphs hash alike are two keys).
fn plan_names(pstmt: &PlannedStmt<'_>, graph: &PipelineGraph<'_>) -> gravity_deps::Names {
    let mut n = gravity_deps::Names::default();
    plan_objects(pstmt, graph, |o| n.add(o));
    n
}

/// A program with host state (`Program::owner`: the hosted call cells, function-manager frames and island states the
/// code writes through `hosted_enter`, behind an `Rc`) is this backend's, unless a template stands for it (D3438: filed
/// without it, `Program::hosted`); so is a composed statement (its InitPlans' and arms' recipes), and one whose set is a
/// PL, function-manager or table-function call (M5 Decision 7, V4R-258: `setfn_private`).
fn shareable(h: &Filed) -> bool {
    private_tier(h).is_none()
}

/// Why a filed program is this backend's (`shareable` false), the report's `tier=private:<why>`: a composed statement
/// (`initplan`, `arms`), a PL / function-manager / table-function set (`setfn`), or host state no template stands for
/// (`host.<why>`: lowerv4 `hir::private_why`); `None`: shareable.
fn private_tier(h: &Filed) -> Option<&'static str> {
    if !h.1.inits.is_empty() {
        Some("private:initplan")
    } else if !h.1.arms.is_empty() {
        Some("private:arms")
    } else if private_set(&h.0) {
        Some("private:setfn")
    } else {
        h.0.owner.as_deref().map(|o| match gravity_lowerv4::hir::private_why(o) {
            "island" => "private:host.island",
            "json" => "private:host.json",
            "descs" => "private:host.descs",
            "calls" => "private:host.calls",
            "data" => "private:host.data",
            "owned" => "private:host.owned",
            "owned.mergejoin" => "private:host.owned.mergejoin",
            "owned.wholerow" => "private:host.owned.wholerow",
            "keep" => "private:host.keep",
            "kind" => "private:host.kind",
            _ => "private:host",
        })
    }
}

/// D4786: the key a program that read a class-keyed list's image is filed under (`PGRUST_GRAVITY_LIST_SALT`):
/// `key` salted with the bytes of every constant list the content key keys by its class, not its bytes (V4R-110,
/// gravity_lower `class_keyed_lists`; a lowering that read one is `Descs::no_cache`, "list"), so only a statement whose
/// every such list holds the same bytes finds it. `None`: the graph keys no list by its class.
fn list_salted(key: ContentKey, graph: &PipelineGraph<'_>) -> Option<ContentKey> {
    let lists = gravity_lower::inputs::key::class_keyed_lists(graph);
    if lists.is_empty() {
        return None;
    }
    let mut h = gravity_abi::hash::Mix128::new();
    for w in [key.0 as u64, (key.0 >> 64) as u64, lists.len() as u64, 0x4434_3738_364c] {
        h.word(w);
    }
    for &image in lists.iter() {
        let bytes = raw::list_image(image);
        h.word(bytes.len() as u64);
        for c in bytes.chunks(8) {
            let mut w = [0u8; 8];
            w[..c.len()].copy_from_slice(c);
            h.word(u64::from_le_bytes(w));
        }
    }
    Some(ContentKey(h.key128()))
}

/// M5 Decision 7, V4R-258: the program's set is a PL, function-manager or table-function call (`setfn_private`).
fn private_set(p: &Program) -> bool {
    let set = |p: &gravity_program::protos::Proto| p.kind == gravity_program::protos::ProtoKind::SetFn && gravity_kernels::ops::srf::entries::setfn_private(p.arg);
    p.v4.as_deref().is_some_and(|d| d.protos.iter().any(set))
}

/// `pgrust.gravity_progcache_check` (default off; on in the harnesses): a shared hit lowers again here and raises when the
/// program's content key (its code's identity: any backend-local address baked into the IR moves it) or template differs.
fn progcache_check_on() -> bool {
    gravity_hosts::inputs::custom_guc_text("pgrust.gravity_progcache_check").is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "on" | "true" | "1" | "yes"))
}

fn progcache_check(b: &mut Backend, pcx: &plan::PlanScope<'_>, arena: ArenaRef, graph: &PipelineGraph<'static>, facts: &RelcacheFacts, pstmt: &'static PlannedStmt<'static>, mcx: Mcx<'static>, key: ContentKey, nodes: &[Node<'static>], h: &Filed) -> PgResult<()> {
    let mut t = Timing { decompose: 0, key: 0, lower: 0, backend: 0, hit: false, front: false, ir: None, shared: false, tier: "-", unfiled: None, plan: (0, 0), ir_bytes: 0, salted: false };
    let why = match install(b, pcx, arena, graph, facts, pstmt, mcx, key, nodes, &mut t, None, None, None)? {
        Err(r) => Some(format!("the lowering here refuses a shared shape ({r})")), // alloc: Cold
        // the shared program as this statement runs it: its estimates bound (`estimates`), so compared by `bind_key`
        Ok(mine) if mine.0.bind_key != h.0.bind_key => Some(format!("program key {:032x} here, {:032x} shared: {} (here / shared)", mine.0.key.0, h.0.key.0, mine.0.bind_diff(&h.0))), // alloc: Cold
        Ok(mine) if mine.1.words != h.1.words || mine.1.funcs != h.1.funcs || mine.1.inits.len() != h.1.inits.len() || mine.1.arms.len() != h.1.arms.len() => Some("the template's words differ".to_string()), // alloc: Cold
        Ok(mine) if mine.1.inits.iter().zip(h.1.inits.iter()).any(|(a, c)| a.program.key != c.program.key) || mine.1.arms.iter().zip(h.1.arms.iter()).any(|(a, c)| a.program.key != c.program.key) => Some("an InitPlan or arm program's key differs".to_string()), // alloc: Cold
        // C3: a read the shared program's recording lacks is a change that would not retire it
        Ok(mine) if mine.1.deps.iter().any(|d| !h.1.deps.covers(d)) => {
            let missed: Vec<_> = mine.1.deps.iter().filter(|&d| !h.1.deps.covers(d)).take(6).collect(); // alloc: Cold
            let only: Vec<_> = h.1.deps.iter().filter(|&d| !mine.1.deps.covers(d)).take(6).collect(); // alloc: Cold
            Some(format!("gravity deps: the lowering here read {missed:?}, which the shared program's recorded dependencies miss ({} here, {} shared; shared only: {only:?})", mine.1.deps.len(), h.1.deps.len())) // alloc: Cold
        }
        Ok(_) => None,
    };
    match why {
        None => Ok(()),
        Some(w) => Err(Box::new(types_error::PgError::error(format!("gravity progcache check: Reason::Bug: {w}")).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR))), // alloc: Cold
    }
}

/// Programs lowered with statement-specific bytes (`Descs::no_cache`), run once and never filed: `no_cache_count()`.
static NO_CACHE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn no_cache_count() -> u64 {
    NO_CACHE.load(core::sync::atomic::Ordering::Relaxed)
}

/// D4024: `f` with each interrupt a catalog read that answers a failed read as unknown dropped raised again at every later
/// CHECK_FOR_INTERRUPTS in it, and at its end.
pub(crate) fn redelivering<T>(f: impl FnOnce() -> PgResult<T>) -> PgResult<T> {
    use init_small::globals as g;
    let mark = g::InterruptsRaised();
    let depth = g::SetRedeliverInterrupts(g::RedeliverInterrupts() + 1);
    let r = f();
    g::SetRedeliverInterrupts(depth);
    if g::InterruptsRaised() == mark {
        return r;
    }
    g::TakeInterruptRaised().map_or(r, Err)
}

/// C3 S4c: `f`, an admission no memo files, served or refused (a plan without a plan key: runner `simple_at`, the entry's
/// and EXPLAIN's unkeyed starts, the identity copy, the plan-order program): `graph` does not record what it reads. Any
/// other admission records (a missed `unfiled` costs its recording only).
pub(crate) fn unfiled<T>(f: impl FnOnce() -> T) -> T {
    UNFILED.with(|c| c.set(c.get() | UNFILED_MARK));
    let _end = UnfiledEnd;
    f()
}

/// `unfiled`'s end on any exit (an unwind too): the mark a `graph` did not take goes.
struct UnfiledEnd;

impl Drop for UnfiledEnd {
    fn drop(&mut self) {
        UNFILED.with(|c| c.set(c.get() & !UNFILED_MARK));
    }
}

/// `UNFILED`'s bits: `unfiled`'s mark, and (D4600, `lean` on) a `graph` running on this thread.
const UNFILED_MARK: u8 = 1;
const IN_GRAPH: u8 = 2;

/// D4600: the end of a `graph` on any exit (an unwind too): the outer admission's `IN_GRAPH` bit back (`0` at the
/// thread's outermost admission), the `unfiled` mark as it is.
struct InGraphEnd(u8);

impl Drop for InGraphEnd {
    fn drop(&mut self) {
        UNFILED.with(|c| c.set((c.get() & !IN_GRAPH) | self.0));
    }
}

leader_tls::leader_tls! {
    thread_local! {
        /// `unfiled`: the next `graph` on this thread does not record (taken there, so a nested admission does). D4600:
        /// and whether a `graph` runs on this thread (`IN_GRAPH`, `lean` on only).
        static UNFILED: core::cell::Cell<u8> = const { core::cell::Cell::new(0) };
    }
}

/// The graph admission of one statement (bind or simple): `Ok(Err(_))` names the refusing step. D4454: the admission is
/// written once into the caller's slot `out` (`bind`), `Some` exactly when this answers `Ok(Ok(()))`: no layer moves it
/// (by value, each `?` and closure return between `bind` and the server copied its kilobytes, 2.1-2.9k instructions per
/// statement). Its one caller empties `out` on a refusal or an error (runner admission.rs `admit_either`), the admission
/// dropped there as the by-value one was here.
pub fn graph(b: &mut Backend, mcx: Mcx<'static>, pstmt: &'static PlannedStmt<'static>, params: ParamListHandle, out: &mut Option<Admitted>) -> PgResult<Result<(), Refused>> {
    let stack0 = stack_depth_core::exceeded_count();
    gravity_hosts::ops::scan::pgrc::residency::ensure_registered()?;
    // C3 S4c: what the admission reads from the catalog, from its first read (decompose, the key walks, the lowering; a hit
    // replays its template's reads, `bind`): the plan memo's test of it (`GraphWords::reads`), and of a refusal it files
    // (`note_refused_reads`). Not recorded for an admission nothing files (`unfiled`: a plan without a plan key, the
    // simple protocol's), which no memo serves again: its front hit records nothing (catbench's per-transaction cost). An
    // early return gives the reads to an outer recording, if any.
    // D4600 (`lean`): the admission marks itself running (`IN_GRAPH`); at the thread's outermost one (`top`) every scope
    // below holds its default value, as `graph` is their only setter and each puts its previous value back on any exit
    let lean = lean::on();
    let prev = UNFILED.with(|c| c.replace(if lean { IN_GRAPH } else { 0 }));
    let _in_graph = lean.then(|| InGraphEnd(prev & IN_GRAPH));
    let top = lean && prev & IN_GRAPH == 0;
    let recording = (prev & UNFILED_MARK == 0).then(gravity_deps::Recording::start);
    // PGRCSERVE G5: the statement's road answers, asked at each admission under its snapshot: decompose maps by them (hosts
    // facts.rs), the front key keys them, and the admission keeps them (`Admitted::roads`, asked again at each reuse)
    let roads = gravity_hosts::ops::scan::am_rows::Roads::choose(mcx, pstmt)?;
    let _roads = gravity_hosts::ops::scan::am_rows::RoadScope::enter(roads.clone());
    // B2-31: a timestamptz constant converted in the session TimeZone during the admission moves the count
    let zone_reads = gravity_decompose::normalize::zone_reads();
    // D2845: mapped at the level's first admission; an address-space rail's refusal is this admission's, by name
    let arena = match b.graph_arena() {
        Ok(arena) => arena,
        Err(r) => return Ok(Err(r)),
    };
    let scope = arena.scope(); // D2487: reset on an unwind too
    let plan_root = b.admit_root();
    let pcx = plan::PlanScope::new(&plan_root);
    // D4600 (`lean`): the admission's clock is its report lines' only reader (0 us where it was not read)
    let t0 = (!lean || b.reporting()).then(Instant::now);
    // the root reader's kind (a named cursor's portal: each FETCH takes a counted part; D2926: the unnamed portal's counted
    // start), for the families' stop roads
    let partial = entry::partial_reader(pstmt);
    // D4600 (`lean`): an execution without parameters (the simple protocol's) has no extern facts, no deferred slots and
    // no re-estimated rows, and its admission's folded / genp / deferred stay the empty ones `bind` wrote
    let bare = lean && params.is_null();
    // D3005: a LIMIT / OFFSET over an integer extern parameter takes this execution's value (`folded` keeps them)
    let facts = if bare { Vec::new() } else { extern_ints(params) }; // alloc: Program
    // D3019: every text parameter's pattern fact (the key's salt reads them all; a program hit lowers nothing to record them)
    let texts: Vec<(i32, i64)> = if bare { Default::default() } else { facts.iter().copied().filter(|f| f.1 < gravity_decompose::admit::QUIET_FACT + 4).collect() }; // alloc: Program
    // GENP-1 (D3004): a generic plan's parameter-reading scans estimated again for these values (none: one plan walk)
    // its cost read off the admission's own clock (`t0`: no second time site)
    let tg = t0.map_or(0, us);
    let mut gp = crate::inputs::genp::Genp::default();
    if !params.is_null() && !types_portal::params::defers_to_evaluation(params) {
        gp.sites = crate::inputs::genp::sites(pstmt);
        if !gp.sites.is_empty() {
            gp.octaves = gp.now(params).unwrap_or_else(|_| vec![0; gp.sites.len()].into_boxed_slice()); // alloc: Program
        }
    }
    let rows = if bare { Vec::new() } else { crate::inputs::genp::rows(&gp.sites, &gp.octaves) }; // alloc: Program
    if b.reporting() && !gp.sites.is_empty() {
        let us_est = t0.map_or(0, us).saturating_sub(tg);
        // the seam's rows beside the plan's (the e2e check compares them with a custom plan's EXPLAIN)
        let now = crate::inputs::genp::estimate_rows(&gp.sites, params).unwrap_or_default();
        let each: Vec<String> = gp.sites.iter().zip(now.iter().chain(core::iter::repeat(&None))).map(|(s, r)| format!("{}:{}->{}", s.node, s.rows, r.map_or(-1.0, |r| r))).collect(); // alloc: Cold
        b.report_line(&format!("gravity: genp sites={} rows=[{}] octaves={:?} rescaled={} us={}", gp.sites.len(), each.join(","), gp.octaves, rows.len(), us_est)); // alloc: Cold
    }
    // D3177: the key walks, the lowering and the word template read one constraint set per domain (`domains_pinned`); the
    // admission keeps the sets, which its words may point into (`GraphWords::domains`)
    let mut pins = gravity_expr::catalog::DomainPins::default();
    // D2834: the slots a PL list leaves to evaluation time are fetched where the program evaluates them (the key's salt
    // reads them: such a program is filed apart)
    let deferred = if bare { Vec::new() } else { deferred_ids(params) }; // alloc: Program
    // D3585: an Append's runs of twin heap scans read as relation sets (`pgrust.gravity_relset`); an instrumented or a
    // publishing lowering keeps every arm (each arm's node counts its own rows)
    let relsets = crate::inputs::knobs::lowerer::relset_on() && !is_instrumented() && !is_publishing();
    // D4569: a SELECT's quiet union nest copied into the arms (road append.nest)
    let union_nests = gravity_lowerv4::ops::append::strategies::nest::on();
    // D4600 (`lean`): at the outermost admission (`top`) a scope whose value is its default is not entered (the value it
    // would set is the one it holds); a nested admission enters each, as before
    // D4600 (`lean`): nothing in the key's salt (`admit_in`)
    let unsalted = lean && facts.is_empty() && rows.is_empty() && deferred.is_empty();
    let mut inner = || {
        // C3's recursive check: under the harness's front check, every held memo hit this admission takes fills again
        // (gravity_deps `Held::check`, raised below)
        let check = crate::inputs::front::check_on();
        let checking = (!top || check).then(|| gravity_deps::Checking::set(check));
        // a native pgrc shape refusal naming a relation is decided at compile: lower again with it on the rows road
        // (`am_rows::shape_rows`), one relation per pass
        let (r, p) = gravity_expr::catalog::domains_pinned(|| {
            let mut r = admit_in(b, &pcx, arena, mcx, pstmt, params, unsalted, &mut *out);
            while matches!(r, Ok(Err(_))) && gravity_hosts::ops::scan::am_rows::take_reshaped() {
                r = admit_in(b, &pcx, arena, mcx, pstmt, params, unsalted, &mut *out);
            }
            r
        });
        drop(checking);
        pins = p;
        r
    };
    let inner = move || if top && !partial { inner() } else { gravity_decompose::admit::reading_partial(partial, inner) };
    let inner = move || gravity_decompose::admit::grouping_relsets(relsets, inner);
    let inner = move || if top && rows.is_empty() { inner() } else { gravity_decompose::admit::rescaling(rows, inner) };
    let inner = move || if top && facts.is_empty() { (inner(), Vec::new()) } else { gravity_decompose::admit::binding_externs(facts, inner) }; // alloc: Program
    let ids = &deferred;
    let inner = move || if top && ids.is_empty() { inner() } else { gravity_program::externs::deferring(ids.clone(), inner) };
    let (r, mut folded) = if top && !union_nests { inner() } else { gravity_decompose::admit::copying_union_nests(union_nests, inner) };
    // D4600 (`lean`): an unrecorded admission's reads are the empty ones `bind` wrote
    let recorded = !lean || recording.is_some();
    let deps = recording.map(gravity_deps::Recording::finish).unwrap_or_default();
    let stale = STALE.with(|s| s.borrow_mut().take());
    // C3: a memo hit whose fill here read what its entry misses (or answers otherwise) is a cache keyed short of its
    // producer, the statement's ERROR whatever the fallback posture, as the front and progcache checks raise
    if let Some(why) = gravity_deps::take_miss() {
        return Err(Box::new(types_error::PgError::error(format!("gravity deps check: Reason::Bug: {why}")).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR))); // alloc: Cold
    }
    if matches!(r, Ok(Err(_))) {
        b.note_refused_reads(&deps);
    }
    for f in texts {
        if !folded.contains(&f) {
            folded.push(f); // alloc: Program
        }
    }
    let r = r.map(|r| {
        r.map(|t| {
            if let Some(g) = out.as_mut().and_then(|a| a.graph.as_mut()) {
                if !bare {
                    g.folded = folded.into_boxed_slice();
                    g.genp = gp;
                    g.deferred = deferred.into_boxed_slice();
                }
                g.domains = pins;
                if recorded {
                    g.reads = gravity_deps::Held::of(deps);
                }
            }
            t
        })
    });
    drop(scope);
    let r = r?;
    stack_met(stack0, r.is_ok())?;
    if b.reporting() {
        match &r {
            Ok(t) => {
                let a = out.as_ref().expect("bind wrote the admission");
                match t.unfiled {
                    Some(why) if t.front => b.report_line(&format!("gravity: front check ({why})")), // alloc: Cold
                    Some(why) => {
                        // PM3a: a statement the walk refuses names the construct (`front::refusal`, as the census does)
                        let m = arena.mark();
                        let at = crate::inputs::front::refusal(arena, pstmt).map(|(tag, what)| format!(": {tag:?}.{what}")); // alloc: Cold
                        arena.reset_to(m);
                        b.report_line(&format!("gravity: front not filed ({why}{})", at.unwrap_or_default())) // alloc: Cold
                    }
                    None => {}
                }
                b.report_line(&format!("gravity: graph admit {} key={:032x} decompose={}us key+lookup={}us lower={}us backend={}us total={}us words={} sites={} pipelines={} tier={} plan_peak={} plan_held={} ir_bytes={}{}", if t.front { "hit front" } else if t.shared { "hit shared" } else if t.hit { "hit" } else { "lowered" }, a.graph.as_ref().map_or(0, |g| g.key.0), t.decompose, t.key, t.lower, t.backend, t0.map_or(0, us), a.nkparams, a.program.sites.len(), a.program.pipelines.len(), t.tier, t.plan.0, t.plan.1, t.ir_bytes, stale.as_deref().map(|w| format!(" stale=[{w}]")).unwrap_or_default())) // alloc: Cold
            }
            Err(r) => crate::runner::refusal_report(b, format!("gravity: graph refused ({r}) total={}us", t0.map_or(0, us))), // alloc: Cold
        }
    }
    if let (Ok(_), Some(a)) = (&r, out.as_mut()) {
        a.timezone = gravity_decompose::normalize::zone_held(zone_reads);
        a.roads = roads;
    }
    Ok(r.map(|_| ()))
}

/// D3360: an admission that did not serve and met PostgreSQL's stack depth limit beneath it (a walk's check_stack_depth the
/// lowering turned into a refusal) ends in that 54001, as PostgreSQL raises it; nothing of the refusal is remembered. D4073
/// ruling: `graph` applies it before its verdict line (no `graph refused`, labelled as at run); the entry, over its admission.
pub fn stack_met(before: u64, served: bool) -> PgResult<()> {
    if !served && stack_depth_core::exceeded_count() != before {
        return Err(stack_depth_core::stack_depth_error());
    }
    Ok(())
}

/// D2834: the extern parameters `params` leaves to evaluation time, 1-based ids: a PL list's referenced slots that are no
/// plan constant (`PARAM_FLAG_CONST` clear: a slot the list was built without, or one whose type is no longer the plan's;
/// types_portal `defers_to_evaluation`). None for any other list.
pub fn deferred_ids(params: ParamListHandle) -> Vec<i32> {
    if params.is_null() || !types_portal::params::defers_to_evaluation(params) {
        return Vec::new(); // alloc: Program
    }
    types_portal::params::with(params, |ps| ps.iter().enumerate().filter(|(_, p)| p.pflags & types_portal::params::PARAM_FLAG_CONST == 0).map(|(i, _)| i as i32 + 1).collect()) // alloc: Program
}

/// `deferred_ids(params)` all in `held`, without collecting them (a revival asks it per nested call).
fn deferred_held(params: ParamListHandle, held: &[i32]) -> bool {
    types_portal::params::with(params, |ps| ps.iter().enumerate().filter(|(_, p)| p.pflags & types_portal::params::PARAM_FLAG_CONST == 0).all(|(i, _)| held.contains(&(i as i32 + 1))))
}

/// D3005: this execution's non-NULL int2 / int4 / int8 extern parameters, `(paramid, value)` (1-based ids, as `Param`
/// reads them); D3019: and each text / varchar / bpchar one's LIKE-pattern fact (`gravity_decompose::admit::QUIET_FACT`,
/// + 1 when quiet; a NULL one is not). None without a list or under a list whose slots wait for evaluation (the
/// admission refuses those).
pub fn extern_ints(params: ParamListHandle) -> Vec<(i32, i64)> {
    if params.is_null() || types_portal::params::defers_to_evaluation(params) {
        return Vec::new(); // alloc: Program
    }
    types_portal::params::with(params, |ps| {
        ps.iter()
            .enumerate()
            .filter_map(|(i, p)| {
                use gravity_decompose::admit::QUIET_FACT;
                if matches!(p.ptype, gravity_decompose::catalog::TEXTOID | gravity_decompose::catalog::BPCHAROID | gravity_decompose::catalog::VARCHAROID) {
                    use gravity_decompose::admit::QUIET_BIT;
                    let quiet = !p.isnull && gravity_decompose::effects::like_image_quiet(p.value.as_usize() as *const u8);
                    return Some((i as i32 + 1, QUIET_FACT + QUIET_BIT * quiet as i64));
                }
                if p.isnull {
                    return None;
                }
                let x = p.value.as_u64();
                let v = match p.ptype {
                    types_core::INT8OID => x as i64,
                    types_core::INT4OID => x as u32 as i32 as i64,
                    types_core::INT2OID => x as u16 as i16 as i64,
                    _ => return None,
                };
                Some((i as i32 + 1, v))
            })
            .collect() // alloc: Program
    })
}

/// D3005: whether admission `a`'s LIMIT / OFFSET constants taken from extern parameters (`GraphWords::folded`) are this
/// execution's values: else the plan memo's admission is not this execution's program, and it admits again.
/// GENP-1: and whether its scans' re-estimates for this execution fall in the octaves it lowered under (`crate::inputs::genp::Genp::hold`).
pub fn folded_hold(a: &Admitted, params: ParamListHandle) -> bool {
    let Some(g) = a.graph.as_ref() else { return !types_portal::params::defers_to_evaluation(params) };
    // D2834: every slot this execution leaves to evaluation time is one the program fetches where it evaluates it
    if types_portal::params::defers_to_evaluation(params) && !deferred_held(params, &g.deferred) {
        return false;
    }
    if !g.folded.is_empty() {
        let now = extern_ints(params);
        if !g.folded.iter().all(|f| now.contains(f)) {
            return false;
        }
    }
    g.genp.hold(params)
}

/// Key expression `e` of this execution (a `GraphWord::Expr` at bind, a `GraphWord::Late` when its pipeline starts, D899):
/// its word and null byte.
pub fn eval_late(exprs: &mut KeyExprs, e: usize, mcx: Mcx<'static>) -> PgResult<(u64, u8)> {
    let st = &mut exprs.exprs[e];
    st.arm_result_mcx(mcx);
    let nd = execexpr::exec_eval_expr(st, &mut execexpr::EvalSlots { scan: None, inner: None, outer: None })?;
    Ok((nd.value.as_u64(), nd.isnull as u8))
}

/// D3576: extern parameter `p`'s word. A by-reference varlena that is an on-disk toast pointer (a PL/pgSQL argument read
/// from another table's row: `pl_matches(docs.body)` over `lines`) is fetched here, on the leader, into `mcx` (still
/// compressed when it was: a pool thread decompresses in memory), so a pool claim never fetches it: a pool thread reads
/// only the statement's own toast relations (D151), where PostgreSQL's worker opens any. Every other word as passed.
pub(crate) fn extern_word(p: &types_portal::params::ParamExternData, mcx: Mcx<'_>) -> PgResult<u64> {
    let v = p.value.as_u64();
    // the common by-value types skip the catalog (bool, int2, int4, int8, oid, float4, float8, date, time, timestamp, timestamptz)
    if p.isnull || v == 0 || p.ptype == 0 || matches!(p.ptype, 16 | 20 | 21 | 23 | 26 | 700 | 701 | 1082 | 1083 | 1114 | 1184) {
        return Ok(v);
    }
    let (typlen, byval) = gravity_fn::typesem::storage::len_byval(p.ptype)?;
    if byval || typlen != -1 {
        return Ok(v);
    }
    match raw::ondisk_image(v as usize) {
        Some(image) => Ok(heaptoast::toast_fetch_datum(mcx, image)?.leak().as_ptr() as u64),
        None => Ok(v),
    }
}

/// ExecEvalParamExtern's error for extern parameter `i` (0-based) that the execution's ParamList does not hold: a plan whose
/// parameter list outgrew the list it runs with (a PL/pgSQL statement re-analyzed while its function ran, P493).
#[cold]
pub fn no_param(i: usize) -> Box<types_error::PgError> {
    Box::new(types_error::PgError::error(format!("no value found for parameter {}", i + 1)).with_sqlstate(types_error::ERRCODE_UNDEFINED_OBJECT)) // alloc: Cold
}

/// The words of a graph admission into the binding's parameter image (its `g.words.len()` words: `runner::key_words`), the
/// extern parameters read in place from the ParamList's array (`params`' own); `false` when the binding is short. An
/// extern parameter the list does not hold is ExecEvalParamExtern's error ([`no_param`]).
pub fn eval(a: &mut Admitted, params: ParamListHandle, ext: &[types_portal::params::ParamExternData], kparams: &mut [u64], knulls: &mut [u8], mcx: Mcx<'static>) -> PgResult<bool> {
    let Some(g) = a.graph.as_ref() else { return Ok(false) };
    let n = g.words.len();
    if kparams.len() < n || knulls.len() < n {
        return Ok(false);
    }
    for (w, word) in g.words.iter().enumerate() {
        match *word {
            GraphWord::Const { value, null } => {
                kparams[w] = value;
                knulls[w] = null;
            }
            GraphWord::Extern(i) => {
                let Some(p) = ext.get(i) else { return Err(no_param(i)) };
                kparams[w] = extern_word(p, mcx)?;
                knulls[w] = p.isnull as u8;
            }
            // D899: a late word waits for its pipeline's start (the runner's `late_words`) where the runner places it: a
            // statement with no InitPlan program and no Append arm (their programs read the same words)
            GraphWord::Late { .. } if g.late_ok() => {
                kparams[w] = 0;
                knulls[w] = 1;
            }
            GraphWord::Expr(e) | GraphWord::Late { expr: e, .. } => {
                (kparams[w], knulls[w]) = eval_late(&mut a.key_exprs, e, mcx)?;
            }
            GraphWord::Zero => {
                kparams[w] = 0;
                knulls[w] = 0;
            }
            // D2834: the list the deferred parameters are fetched from where they are evaluated (none: no program that reads one)
            GraphWord::Deferred => {
                kparams[w] = params.0;
                knulls[w] = 0;
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcx::MemoryContext;
    use gravity_lower::WordPlan;

    /// privileges#562: an Aggref's gate is OBJECT_AGGREGATE, a window function's OBJECT_FUNCTION even for an aggregate;
    /// sorted by oid, once each per object; of an aggregate gated both ways, the gate `first` names leads.
    #[test]
    fn gates_lead_with_the_first_gate() {
        use shapes::AclObject::{Aggregate as A, Function as F};
        let mut root = mcx::SendRoot::new("gates test");
        let mut l = || super::raw::tpl_list(&mut root);
        assert_eq!(&*gates(&[66], &[66], &[(66, A)], l()), &[(66, A), (66, F)]);
        assert_eq!(&*gates(&[66], &[66], &[(66, F)], l()), &[(66, F), (66, A)]);
        // repeats collapse per object; other oids sort around them
        assert_eq!(&*gates(&[70, 65, 66, 65, 66], &[66, 67, 66], &[(66, F)], l()), &[(65, F), (66, F), (66, A), (67, A), (70, F)]);
        assert!(gates(&[], &[], &[], l()).is_empty());
    }

    /// privileges#562 review r1 S1: C's init order, not the pipelines'. A hash join initializes its outer (probe) child
    /// before its inner (build), InitPlan each subplan before the plan tree, ExecInitNode a node's children before its gates.
    #[test]
    fn first_gates_follow_c_init_order() {
        use shapes::AclObject::{Aggregate as A, Function as F};
        use types_nodes::plannodes::{Agg, Append, Hash, HashJoin, SeqScan, SubqueryScan, WindowAgg};
        use types_nodes::primnodes::{Aggref, OpExpr, WindowFunc};
        fn scan(m: Mcx<'_>) -> Node<'_> {
            Node::build::<SeqScan>(m).unwrap().seal()
        }
        // HAVING agg(x) > 0 over `below`
        fn agg<'m>(m: Mcx<'m>, f: Oid, below: Node<'m>) -> Node<'m> {
            let mut a = Node::build::<Aggref>(m).unwrap();
            a.aggfnoid = f;
            let mut op = Node::build::<OpExpr>(m).unwrap();
            op.args.lappend(m, a.seal()).unwrap();
            let mut n = Node::build::<Agg>(m).unwrap();
            n.plan.qual.lappend(m, op.seal()).unwrap();
            n.plan.lefttree = Some(below);
            n.seal()
        }
        fn window<'m>(m: Mcx<'m>, f: Oid, below: Node<'m>) -> Node<'m> {
            let mut w = Node::build::<WindowFunc>(m).unwrap();
            w.winfnoid = f;
            let mut n = Node::build::<WindowAgg>(m).unwrap();
            n.plan.targetlist.lappend(m, w.seal()).unwrap();
            n.plan.lefttree = Some(below);
            n.seal()
        }
        fn hash_join<'m>(m: Mcx<'m>, outer: Node<'m>, inner: Node<'m>) -> Node<'m> {
            let mut s = Node::build::<SubqueryScan>(m).unwrap();
            s.subplan = Some(inner);
            let mut h = Node::build::<Hash>(m).unwrap();
            h.plan.lefttree = Some(s.seal());
            let mut j = Node::build::<HashJoin>(m).unwrap();
            j.join.plan.lefttree = Some(outer);
            j.join.plan.righttree = Some(h.seal());
            j.seal()
        }
        let ctx = MemoryContext::new("first_gates");
        let m = ctx.mcx();
        // J1: the window on the probe side, the Aggref on the build; J4 the reverse
        assert_eq!(first_gates([hash_join(m, window(m, 66, scan(m)), agg(m, 66, scan(m)))].into_iter(), &[66]), [(66, F)]);
        assert_eq!(first_gates([hash_join(m, agg(m, 66, scan(m)), window(m, 66, scan(m)))].into_iter(), &[66]), [(66, A)]);
        // the lower node's gate: a window over the Aggref's node, an Aggref over the window's
        assert_eq!(first_gates([window(m, 66, agg(m, 66, scan(m)))].into_iter(), &[66]), [(66, A)]);
        assert_eq!(first_gates([agg(m, 66, window(m, 66, scan(m)))].into_iter(), &[66]), [(66, F)]);
        // J6: a subplan's window before the plan tree's Aggref, even the probe side's
        assert_eq!(first_gates([window(m, 66, scan(m)), hash_join(m, agg(m, 66, scan(m)), scan(m))].into_iter(), &[66]), [(66, F)]);
        // an Append's arms in order
        let mut app = Node::build::<Append>(m).unwrap();
        app.appendplans.lappend(m, agg(m, 66, scan(m))).unwrap();
        app.appendplans.lappend(m, window(m, 66, scan(m))).unwrap();
        assert_eq!(first_gates([app.seal()].into_iter(), &[66]), [(66, A)]);
        // each oid its own first gate; an oid outside `both` none
        assert_eq!(first_gates([hash_join(m, window(m, 67, scan(m)), agg(m, 66, window(m, 66, scan(m))))].into_iter(), &[66, 67]), [(67, F), (66, F)]);
        assert!(first_gates([agg(m, 68, window(m, 68, scan(m)))].into_iter(), &[66]).is_empty());
    }

    /// PERF-COLD 09-25 (main's condition on the lazy IR hash): a variant a lowering filed while the shadow check was off
    /// carries `IR_UNHASHED`; the check turned on later accepts it without hashing (never a false mismatch), and a hashed
    /// variant is still compared.
    #[test]
    fn unhashed_variant_never_mismatches() {
        assert!(crate::inputs::front::variant_ir_ok(crate::inputs::front::IR_UNHASHED, || panic!("an unhashed variant is not hashed again")));
        assert!(crate::inputs::front::variant_ir_ok(0x1234_5679, || 0x1234_5679));
        assert!(!crate::inputs::front::variant_ir_ok(0x1234_5679, || 0x1234_567b));
    }

    /// D411: every recipe kind's answer to the front guard (a graph-bound recipe is never filed under the front key), and
    /// a bitmap tree's / an unknown rung recipe's template classified as such.
    #[test]
    fn every_recipe_kind_answers_the_front_guard() {
        use crate::ops::scan::front_kind::{graph_use, tpl_kind, TplKind};
        let want = [
            (TplKind::Index, false),
            (TplKind::Bitmap, true),
            (TplKind::Range, false),
            (TplKind::Granule, false),
            (TplKind::Fetch, false),
            (TplKind::RungTree, true),
            (TplKind::RungTid, false),
            (TplKind::RungSample, false),
            (TplKind::RungMorsel, false),
            (TplKind::RungOther, true),
        ];
        for (k, w) in want {
            assert_eq!(graph_use(k), w, "{k:?}");
        }
        let (tree, other) = crate::ops::scan::front_kind::fixture_tpls();
        assert_eq!(tpl_kind(&tree), TplKind::RungTree);
        assert!(crate::ops::scan::front_kind::binds_from_graph(&tree), "a bitmap tree binds its recheck from the graph");
        assert_eq!(tpl_kind(&other), TplKind::RungOther);
        assert!(crate::ops::scan::front_kind::binds_from_graph(&other), "an unknown rung recipe stays out of the front key");
        assert!(graph_bound(&tree) && graph_bound(&other), "neither is a plan-keyed computed-key walk (D2754)");
    }

    /// K4's repro (`WHERE tid = 1 AND id = 1`): both index keys hold ONE Const node; the key's traversal visits it
    /// twice. Each word must take its own occurrence — a memo hit on `(5, 0)` then binds `(5, 0)`, not `(5, 5)`.
    #[test]
    fn two_words_sharing_one_node_take_successive_occurrences() {
        let ctx = MemoryContext::new("template");
        let mcx = ctx.mcx();
        let arena = gravity_program::Arena::new(1 << 16);
        let shared = Node::mk_const(mcx, types_core::INT4OID, -1, 0, 4, datum::Datum::from_i32(1), false, true).unwrap();
        let other = Node::mk_const(mcx, types_core::INT4OID, -1, 0, 4, datum::Datum::from_i32(7), false, true).unwrap();
        let mut plan = Located::new(arena.handle());
        plan.key(shared);
        plan.key(other);
        plan.key(shared);
        let mut root = mcx::SendRoot::new("template test");
        let tpl = template(&plan, &[shared, other, shared], super::raw::tpl_list(&mut root)).ok().expect("located");
        assert_eq!(&*tpl, &[WordTpl::Const(0), WordTpl::Const(1), WordTpl::Const(2)]);
        // a word whose node has no unclaimed occurrence left is outside the traversal
        assert!(template(&plan, &[shared, other], super::raw::tpl_list(&mut root)).is_err());
    }

    /// memoize 10-03 item 2: a NULL literal's word binds NULL without a locator (its content key keys the nullness), even
    /// where its node is outside the key's traversal (a NULL decompose synthesized, a D644 stand-in in the front key).
    #[test]
    fn a_null_literal_word_reads_no_node() {
        let ctx = MemoryContext::new("template");
        let mcx = ctx.mcx();
        let arena = gravity_program::Arena::new(1 << 16);
        let null = Node::mk_const(mcx, types_core::INT4OID, -1, 0, 4, datum::Datum::null(), true, true).unwrap();
        let one = Node::mk_const(mcx, types_core::INT4OID, -1, 0, 4, datum::Datum::from_i32(1), false, true).unwrap();
        let mut plan = Located::new(arena.handle());
        plan.key(null);
        plan.key(one);
        let mut root = mcx::SendRoot::new("template test");
        let tpl = template(&plan, &[one], super::raw::tpl_list(&mut root)).ok().expect("located");
        assert_eq!(&*tpl, &[WordTpl::Null, WordTpl::Const(0)]);
        assert_eq!(word_reads(&tpl), [0u32]);
    }

    /// V4R-37 a: an empty-attnos range over a heap reaches the rungs (M2's count-only recipe); over pgrc it is still
    /// refused before any rung (does not apply), and a range with an attribute passes either way.
    #[test]
    fn an_empty_attnos_heap_range_reaches_the_rungs() {
        let arena = gravity_program::Arena::new(1 << 16);
        let range = |am| Source::Range { site: gravity_program::SiteDesc::range_source(arena.handle(), 7, &[1]), am, scanrelid: 1, relid: 7 };
        assert!(attnos_admitted(&range(Am::Heap), &[]).is_ok());
        assert!(attnos_admitted(&range(Am::Pgrcolumnar), &[]).is_err_and(|r| r.what.contains("without an attribute")));
        assert!(attnos_admitted(&range(Am::Other), &[]).is_err());
        for am in [Am::Heap, Am::Pgrcolumnar] {
            assert!(attnos_admitted(&range(am), &[1]).is_ok());
        }
    }

    /// V4R-37 b: a v4 program declaring `src.prune` carries the execution's parameter handle in `GraphWords.prune` (which
    /// `eval_keys` refreshes); a program without the protocol carries none (does not apply), and a composed Append's own
    /// exec pruning row is kept as it was.
    #[test]
    fn a_prune_program_carries_the_param_handle() {
        use gravity_program::protos::{Proto, ProtoKind, V4Descs};
        let params = ParamListHandle::NULL;
        assert!(v4_prune_handle(None::<&V4Descs>, false, None, params).is_none());
        let arms = V4Descs::new(vec![Proto::new(ProtoKind::Arms, 0)]); // alloc: Cold
        assert!(v4_prune_handle(Some(&arms), false, None, params).is_none(), "append.arms alone reads no parameter");
        let prune = V4Descs::new(vec![Proto::new(ProtoKind::Arms, 0), Proto::new(ProtoKind::Prune, 0)]); // alloc: Cold
        let got = v4_prune_handle(Some(&prune), false, None, params).expect("src.prune declared: the handle is carried");
        assert_eq!(got.nsubplans, 0, "the handle alone: exec_skip leaves the arms to the protocol");
        let own = crate::ops::append::prune::ExecPrune { index: 3, nsubplans: 4, params };
        assert!(v4_prune_handle(Some(&prune), false, Some(own), params).is_some_and(|p| p.index == 3 && p.nsubplans == 4));
        // D3663: a relation set pruned at the executor start carries the handle alone, with no protocol
        assert!(v4_prune_handle(None::<&V4Descs>, true, None, params).is_some_and(|p| p.nsubplans == 0), "a pruned set reads the parameters");
    }

    /// planprod P6-b: a template (`GraphTpl`) over `words` words, its root owned by the running statement as `install` makes it.
    fn graph_tpl(words: usize) -> GraphTpl {
        let mut root = mcx::SendRoot::new_owned_small("GravityGraphTpl", memledger::charge::statement_owner());
        let mut w: TplVec<WordTpl> = super::raw::tpl_list(&mut root);
        w.resize(words, WordTpl::Zero);
        let (funcs, inits, arms, merge, checks, guards) = (super::raw::tpl_list(&mut root), super::raw::tpl_list(&mut root), super::raw::tpl_list(&mut root), super::raw::tpl_list(&mut root), super::raw::tpl_list(&mut root), super::raw::tpl_list(&mut root));
        let held = Arc::new(crate::runner::held::plan(&PlannedStmt::default()));
        GraphTpl { words: w, funcs, inits, arms, prune: None, exec_prune: false, merge, names: pgsync::Mutex::new(memledger::charge::Charge::NONE), own: pgsync::Mutex::new(memledger::charge::Charge::NONE), checks, held, no_proc: false, guards, deps: gravity_deps::Deps::default(), db: 0, published: core::sync::atomic::AtomicBool::new(false), root: pgsync::Mutex::new(root) }
    }

    /// A backend session bound on a thread of its own until the returned sender sends (the tier's stand-in: an exact count
    /// under concurrent tests; `publish` passes `PROGRAM_CACHES`).
    fn other_session(procno: u32) -> (memledger::Owner, std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let (done, wait) = std::sync::mpsc::channel::<()>();
        let (bound, is_bound) = std::sync::mpsc::channel::<()>();
        let t = std::thread::spawn(move || {
            memledger::bind_backend(procno, 9000 + procno);
            bound.send(()).unwrap();
            let _ = wait.recv();
            memledger::unbind_backend();
        });
        is_bound.recv().unwrap();
        (memledger::Owner::Session(memledger::Acct::backend(procno)), done, t)
    }

    /// planprod P6-b: a template's root is the lowering statement's at birth, the session's when the backend's cache files
    /// it (`adopt_hosted`), the tier's at the publish (a stand-in session here), once: no adoption moves it after. It
    /// leaves that owner when the last holder drops the template, on another thread.
    #[test]
    fn a_graph_templates_root_is_the_statements_then_the_sessions_then_the_tiers_until_its_last_drop() {
        std::thread::spawn(|| {
            let (tier, done, other) = other_session(4066);
            let a = memledger::Acct::backend(4067);
            memledger::bind_backend(4067, 9000 + 4067);
            memledger::statement_begin();
            let stmt = memledger::Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
            let session = memledger::Owner::Session(a);
            let (s0, c0) = (memledger::charge::owner_ctx(session).0, memledger::charge::owner_ctx(tier).0);
            // shared as the tier shares it (`SharedTpl`: a template's lists are its root's, so its `Send` is the holder's)
            let tpl = SharedTpl(Arc::new(graph_tpl(16 << 10)));
            // the check option names' charge (`names`), as `install` opens it on the root's owner
            let committed = |o| memledger::charge::owner_row(o).committed;
            let (n0, nt0) = (committed(stmt), committed(tier));
            const NAMES: u64 = 3 * 24;
            {
                use memledger::charge::{release_of, Charge, Door};
                let mut c = pgsync::lock(&tpl.0.names);
                *c = Charge::open(stmt, release_of(stmt), memledger::Kind::Region, Door::Heap, 0);
                c.commit(NAMES);
            }
            assert_eq!(committed(stmt) - n0, NAMES as i64, "the names are the statement's, as the root is");
            let bytes = tpl.0.root_bytes() as i64;
            let born = memledger::charge::owner_ctx(stmt).0;
            assert!(born >= bytes && bytes >= 16 << 10, "the root holds the lists, the statement's: {born} {bytes}");
            tpl.0.adopt(session);
            let filed = memledger::charge::owner_ctx(session).0;
            assert_eq!(memledger::charge::owner_ctx(stmt).0, born - bytes, "the adoption takes the root off the statement");
            assert_eq!(filed - s0, bytes, "onto the session");
            assert_eq!((committed(stmt) - n0, memledger::charge::owner_row(session).committed >= NAMES as i64), (0, true), "and the names with it");
            tpl.0.publish_to(tier);
            eprintln!("P6-b witness: a template's root of {bytes} bytes: statement {born} -> session {s0} -> {filed} -> published {}, tier {c0} -> {}", memledger::charge::owner_ctx(session).0, memledger::charge::owner_ctx(tier).0);
            assert_eq!(memledger::charge::owner_ctx(session).0, s0, "the publish takes it off the session");
            assert_eq!(memledger::charge::owner_ctx(tier).0 - c0, bytes, "onto the tier");
            let own = tpl.0.own_bytes();
            assert!(own >= memledger::arc_bytes::<GraphTpl>() + mcx::SendRoot::HEADER_BYTES + memledger::arc_bytes::<crate::runner::held::HeldPlan>(), "D4580: its own blocks: {own}");
            assert_eq!(committed(tier) - nt0, (NAMES + own) as i64, "the names with it, and (D4580) the template's own blocks");
            tpl.0.adopt(session);
            assert_eq!(memledger::charge::owner_ctx(session).0, s0, "never moves after the publish");
            memledger::statement_end();
            std::thread::spawn(move || drop(tpl)).join().unwrap();
            assert_eq!(memledger::charge::owner_ctx(tier).0, c0, "the last drop, on another thread, takes it off");
            assert_eq!(committed(tier), nt0, "the names and its own blocks too");
            memledger::unbind_backend();
            done.send(()).unwrap();
            other.join().unwrap();
        })
        .join()
        .unwrap();
    }

    /// planprod P6-b: a template the tier never takes stays on the session and leaves with the backend cache's holder.
    #[test]
    fn a_graph_template_the_tier_refuses_stays_the_sessions_and_leaves_with_it() {
        std::thread::spawn(|| {
            let a = memledger::Acct::backend(4065);
            memledger::bind_backend(4065, 9000 + 4065);
            memledger::statement_begin();
            let session = memledger::Owner::Session(a);
            let s0 = memledger::charge::owner_ctx(session).0;
            let tpl = Arc::new(graph_tpl(16 << 10));
            let bytes = tpl.root_bytes() as i64;
            tpl.adopt(session);
            let filed = memledger::charge::owner_ctx(session).0;
            memledger::statement_end();
            assert_eq!(memledger::charge::owner_ctx(session).0, filed, "it outlives the statement on the session");
            drop(tpl);
            eprintln!("P6-b witness: a refused template of {bytes} bytes: session {s0} -> {filed} -> {}", memledger::charge::owner_ctx(session).0);
            assert_eq!(filed - s0, bytes, "the session held it");
            assert_eq!(memledger::charge::owner_ctx(session).0, s0, "gone with it");
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }

    /// planprod P6-c1: a program's recipe list (`registry`'s `RecipeSet`) is the lowering statement's at birth, the session's
    /// once the backend's cache files it (`adopt_hosted`), and leaves the session with its last holder: its root and the
    /// shared block that holds it, both.
    #[test]
    fn a_recipe_lists_root_is_the_statements_then_the_sessions_until_its_drop() {
        std::thread::spawn(|| {
            let held = |o| memledger::charge::usage(o).held;
            let a = memledger::Acct::backend(4068);
            memledger::bind_backend(4068, 9000 + 4068);
            memledger::statement_begin();
            let stmt = memledger::Owner::Statement { acct: a, stmt: memledger::running_statement(a) };
            let session = memledger::Owner::Session(a);
            let (t0, s0) = (held(stmt), held(session));
            let owner = memledger::charge::statement_owner();
            assert_eq!(owner, stmt);
            let mut root = mcx::SendRoot::new_owned_small("GravityHostRecipes", owner);
            let mut list: TplVec<RecipeTpl> = super::raw::tpl_list(&mut root);
            list.reserve_exact(16);
            for k in 0..16u8 {
                list.push(RecipeTpl::Rung(Arc::new(k)));
            }
            let set = RecipeSet::shared(list, root, owner);
            let bytes = set.root_bytes() as i64 + SHARED_BLOCK as i64;
            let born = held(stmt);
            assert_eq!(born - t0, bytes, "the root holds the list and the shared block is charged, both the statement's");
            set.adopt(session);
            let filed = held(session);
            eprintln!("P6-c1 witness: a list of {} recipes ({} B each), root + shared block {bytes} B ({SHARED_BLOCK} B block): statement {t0} -> {born} -> {}, session {s0} -> {filed}", set.len(), core::mem::size_of::<RecipeTpl>(), held(stmt));
            assert_eq!(held(stmt), t0, "the filing takes both off the statement");
            assert_eq!(filed - s0, bytes, "onto the session");
            memledger::statement_end();
            assert_eq!(held(session), filed, "they outlive the statement on the session");
            drop(set);
            assert_eq!(held(session), s0, "gone with the last holder");
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }
}
