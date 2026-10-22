//! The program context (lowerv4 plan §2.1) every mechanism's make fn receives: expressions by `ExId` (shareable), sites,
//! the out row, scratch, the frame words, the program's v4 protocols and write plan, and one extension slot per face for that
//! face's compile-time state (keyed by face, never by milestone); the row path reads through `Env`.
use core::any::TypeId;
use std::rc::Rc;

use gravity_expr::emit::ParamNeed;
use gravity_expr::percode::CodeCol;
use gravity_expr::{Compiled, NullableDatum};
use gravity_lower::{Place, WordPlan};
use gravity_program::protos::Proto;
use gravity_program::{ArenaRef, ArenaVec, OutCol, SiteDesc, WritePlan};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};
use types_nodes::Node;

use crate::face::end::EndMech;
use crate::face::kinds::Face;
use crate::face::strategy::StrategySet;
use crate::face::source::SourceMech;
use crate::face::step::StepMech;
use crate::frame::stop::ResumeWord;
use crate::graph::Gx;
use crate::hir::Ex;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExId(pub u32);

/// One extension slot per face.
const FACES: usize = Face::RowOp as usize + 1;

pub use crate::frame::raw::ExtG;
pub(crate) use crate::frame::raw::ext_g;

/// Any value: the slot's box is one so dropping it drops the state.
pub(crate) trait Held {}
impl<T: ?Sized> Held for T {}

/// The faces' state slots (`Px::ext_mut`): each the `ExtG::Id` of the state it holds and the state, in the lowering's
/// planning context (`frame/raw.rs` reads them back).
#[derive(Default)]
pub(crate) struct Exts<'g> {
    pub(crate) slots: [Option<(TypeId, PgBox<'g, dyn Held + 'g>)>; FACES],
    /// `'g` invariant: a `&Exts<'g>` never shortens to `&Exts<'s>`, so a state is read back at the `'g` it was made at.
    _inv: core::marker::PhantomData<core::cell::Cell<&'g ()>>,
}

impl<'g> Exts<'g> {
    /// Face `f`'s state, made by `T::make` in `m` at first use; `None`: the slot holds another type.
    pub(crate) fn get_or_make<T: ExtG<'g>>(&mut self, f: Face, m: mcx::Mcx<'g>) -> Option<&mut T> {
        let slot = &mut self.slots[f as usize];
        if slot.is_none() {
            *slot = Some((TypeId::of::<T::Id>(), mcx::pg_box_dyn!(m, T::make(m) => dyn Held + 'g)));
        }
        self.get_mut(f)
    }
}

/// The rows one push of the out block carries (a plain aggregate's emit declares 1).
pub const OUT_ROWS: u32 = 64;

/// The out row the root's end declares: its columns, the rows a push carries, and the by-reference out region.
pub struct Out {
    pub cols: ArenaVec<OutCol>,
    pub rows: u32,
    pub region: u32,
}

/// A usize op position as a frame-word key `j` / `after` (u64 on every target): sign-extending, so `usize::MAX - c` (the
/// source / end / per-slot positions) is `u64::MAX - c` on wasm32 as on 64-bit, where it is the identity.
pub(crate) const fn key_j(j: usize) -> u64 {
    j as isize as i64 as u64
}

/// A frame word's owner: numbered once per program (`Px::frame_word`), 8 bytes each in the words site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordKey {
    /// Word `i` of face `face`'s mechanism at pipeline `k`, position `j` (a step's op index; `u64::MAX` for a source or end).
    /// `j` and `after` are u64 on every target: tags compose into their high bits (`<< 32`, `<< 40`, `<< 55`, `<< 56`), and
    /// wasm32's usize is 32 bits, so a key is built in u64 (a usize expression cannot reach a key without a cast).
    Open { face: Face, k: usize, j: u64, i: u8 },
    /// Word `i` of the union whose arm marker is op `after`, kept across the union's arms (D229).
    Union { union: u32, after: u64, i: u8 },
    /// The union's written flag: set by the first arm to open.
    UnionFlag { union: u32 },
    /// A resume word (`stop.rs`).
    Resume(ResumeWord),
    /// M8 L0-c: executor parameter `id`'s word `i` (0 its value, 1 its flags: params.rs SET / NULL), numbered first (params.rs `reserve`).
    Param { id: i32, i: u8 },
}

/// The Runs face's pipeline words (`WordKey::Open { face: Runs, k, j: u64::MAX, i }`): one index per role, so two
/// mechanisms of one pipeline (a run reader feeding a sort or top end) never pick the same number (D482; open.rs refuses
/// two roles on one word). A new role takes the next free index here; mergeappend keys by its op `j`, not these.
pub mod runs_word {
    pub const READER_MODE: u8 = 0;
    pub const READER_CURSOR: u8 = 1;
    pub const READER_TOTAL: u8 = 2;
    pub const END_RUN: u8 = 3;
    pub const TOP_N: u8 = 4;
    pub const TOP_SOFT: u8 = 5;
    pub const BOUND_ARMED: u8 = 6;
    pub const BOUND_FLAGS: u8 = 7;
    pub const BOUND_WORD: u8 = 8;
    pub const GROUPS_ARRIVALS: u8 = 9;
    pub const INC_START: u8 = 10;
    pub const INC_DISORDER: u8 = 11;
    /// V4R-179: a bounded incremental sort's stop counts the rows it has seen here; the row before's prefix pairs, two words
    /// per prefix key and any number of keys, are keyed by the Sort op's position (ops/sort/topk_rows.rs `Stop`). 13..28: free
    pub const TOP_SEEN: u8 = 12;
    /// D1634: a top's computed bounds `[value, flags]`, count then offset, which the Limit over its breaker reads too
    /// (params_bound.rs `top_words`)
    pub const TOP_BOUND: u8 = 29;
    /// jmerge-0012: a pulled slot run reader's run base, re-read at each open (a pull's growth moves the run), past
    /// TOP_BOUND's four words; keyed `j: usize::MAX - 1 - slot` (one per slot a pipeline's merges read)
    pub const SLOT_BASE: u8 = 33;
    /// group_core-0259: an unbounded incremental sort's stop (ops/sort/incremental.rs `Batches`, past SLOT_BASE): the open
    /// batch's rows, its load mode, its closed groups' distinct rows, the distinct rows handed on
    pub const INC_ROWS: u8 = 34;
    pub const INC_MODE: u8 = 35;
    pub const INC_HELD: u8 = 36;
    pub const INC_OUT: u8 = 37;
    /// lockrows INCSTREAM (ops/sort/inc_stream.rs, past INC_OUT): a streamed incremental sort's closed rows and end flag, and
    /// its reader's pull (the closed rows it wants) and the closed count it pulled at
    pub const INC_CLOSED: u8 = 38;
    pub const INC_DONE: u8 = 39;
    pub const INC_WANT: u8 = 40;
    pub const INC_BASE: u8 = 41;
    /// D2432 (past INCSTREAM's words): the open prefix group's parts put while it was still open (a group past a run's
    /// room): the most distinct rows of one part and their sum (a lower and an upper bound of the group's); the upper
    /// bounds of the rows handed on and of the batch's closed groups; a group closed by a put (its distinct rows plus one,
    /// 0 when none)
    pub const INC_SPLIT_LO: u8 = 42;
    pub const INC_SPLIT_HI: u8 = 43;
    pub const INC_OUT_HI: u8 = 44;
    pub const INC_HELD_HI: u8 = 45;
    pub const INC_PEND: u8 = 46;
    /// D2690 (past D2432's words): a top-K DISTINCT open group's last cut, `2 n + full` (its `n` ordered rows; 0: none)
    pub const INC_BOUND: u8 = 47;
    /// CAP-25 (past INC_BOUND): a fetch top's gathered rows `[from, to)` (ops/sort/strategies/fetch.rs), which a call resumed
    /// inside them reads from the fetch site's lanes again, never gathering them twice
    pub const FETCH_FROM: u8 = 48;
    pub const FETCH_TO: u8 = 49;
    /// NEW-43 (past FETCH_TO): an INCSTREAM producer's run filled, so the stream fell back to the breaker form (its runs put,
    /// its reader reading the host merge past the rows it handed on: ops/sort/inc_stream.rs `spilled`); 2 once the producer
    /// put the runs and stopped at the next prefix group's first row (inc_stream.rs `stopped`), 0 again once its reader
    /// re-armed the region past the merge
    pub const INC_SPILL: u8 = 50;
    /// NEW-43 (past INC_SPILL): the rows an INCSTREAM reader handed on before its last re-arm (the merges it read whole);
    /// each pull after one asks for its budget less them
    pub const INC_HANDED: u8 = 51;
    /// C1 part 2 (past INC_HANDED): the rows an INCSTREAM cursor reader's last pull asked for (ops/sort/inc_stream.rs
    /// `cursor_pull`)
    pub const INC_ASKED: u8 = 52;
}

/// A protocol's `site` naming its own pipeline's drafted site, which `Mechs::new` replaces with the site's number.
pub const DRAFT_SITE: i16 = -2;

/// D4203 (`Px::licensed_late`): whether a pipeline running a node mapping read ahead under a recheck licence may run
/// after a row left the statement, and by which road. Ordered by how much is known: `Ordered` is the most.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Late {
    /// no reader can pull it after a row left: program order puts it before every emitting pipeline
    No,
    /// a road the lowering cannot time (a rerun set, an on-demand set, a lazy build, another CTE's plan): it may run
    /// before or after the first row leaves (C2 debt)
    Unanswered,
    /// program order: it is pulled from a pipeline that runs after an emitting pipeline
    Ordered,
}

pub struct Px<'g, 'p> {
    pub arena: ArenaRef,
    pub gx: &'g Gx<'g, 'p>,
    pub plan: &'g mut dyn WordPlan<'p>,
    exs: Vec<Ex<'p>>,
    /// D3585: `hosted_unsafe`'s answer over the first `.0` expressions (`.1`: one of them has a call a pool thread cannot
    /// run); reset when the expressions are taken or changed.
    hosted_seen: core::cell::Cell<(usize, bool)>,
    pub sites: ArenaVec<SiteDesc>,
    /// Sites drafted at make time by pipeline (`set_site` / `site_mut`), numbered by `Mechs::new` after every make.
    drafts: Vec<(usize, SiteDesc)>,
    /// Each made pipeline's driving site, by graph index (`Mechs::new` records it).
    pipe_sites: PgVec<'g, Option<u16>>,
    /// Each made pipeline's nominal driving site (no drafted site, its source's `nominal_site`), by graph index
    /// (`Mechs::shaped` records it): `claims_site`'s answer where `pipeline_site` has none.
    nominal_sites: PgVec<'g, Option<u16>>,
    /// Sites numbered at emit after every source site (a row op's write and conflict sites: exec admit's host registry is
    /// positional over the source sites), each with the cell its number goes into (`add_late_site`).
    late: Vec<(SiteDesc, Rc<core::cell::Cell<usize>>)>,
    pub out: Option<Out>,
    pub scratch: u32,
    /// The code lanes the hosted emit sweeps per dictionary entry (`Hosted::new_over`).
    pub codes: Vec<CodeCol>,
    /// Read-only bytes the program owns (a long VALUES list's literal table), freed with it; a program holding any is never cached.
    pub data: Vec<Box<[u8]>>,
    /// Objects the program owns beside its IR (expr.saop's prepared sets, V4R-SAOP), freed with it; unlike `data` they keep
    /// the program cacheable: their contents follow literals the content key hashes by image.
    pub owned: Vec<Box<dyn core::any::Any>>,
    /// planprod P4: the program's hosted product root ('GravityHosted', `hir::hosted_root`), made at the first
    /// `program_cx` (whose contexts are its children, kept in `owned`) and handed to the hosted owner at the finish
    /// (frame/program.rs, `finish_keeping`), so one adoption (`hir::adopt`) moves the owner and those contexts together. A root of its own,
    /// never the planning context nor a child of it. None until a lowering asks.
    pub(crate) hosted_root: Option<mcx::PinnedContext>,
    /// The runtime protocols the program declares (`gravity_program::v4`), in declaration order.
    pub protos: Vec<Proto>,
    /// D3585: the places in `protos` of protocols declared at `DRAFT_SITE` that no pipeline's shaping has given its site yet
    /// (`declare_proto`, frame/mechs.rs `shaped`): shaping each pipeline searched every protocol (one per scan arm of an
    /// Append over N partitions), quadratic in the arms.
    pub(crate) draft_protos: Vec<u32>,
    /// The write plan a row-op end declares; `None`: a SELECT. C11 WriteOwner: private, the methods below are its only
    /// readers and writers.
    write: Option<WritePlan>,
    /// D2605: the graph pipeline whose row op claimed `write` (ops/modifytable/rowop.rs `claim`); `None` for a lock's plan.
    pub write_k: Option<usize>,
    /// D1687: the write plans beside `write`, each with its write site, ModifyTable node and graph pipeline
    /// (ops/modifytable/rowop.rs `claim`).
    pub writes: Vec<(crate::ops::modifytable::rowop::Late, i32, WritePlan, usize)>,
    /// The roads that apply to the statement (`applies::applicable`), set before the mechanisms are made.
    pub strategies: StrategySet,
    word_keys: PgVec<'g, WordKey>,
    /// `(graph pipeline whose makers run, the identity they declare under)` (`ident`, `remake`).
    pub(crate) making: (usize, usize),
    /// Passes added while their `after` is made, each with its virtual id, until `resolve` takes them (`add_pass`).
    pub(crate) passes: Vec<(usize, usize, ExtraPass<'g>)>,
    /// The graph pipeline each pass follows, by virtual id - graph length.
    pub(crate) pass_after: Vec<usize>,
    /// Breaker slots handed out past the graph's (`new_slot`).
    pub(crate) new_slots: usize,
    /// `(k, union, marker op index)` per arm marker (its make sets it); a pipeline under nested unions has several.
    pub(crate) unions: Vec<(usize, u32, usize)>,
    /// D824 (V4R-298): `(build, probe)` graph pipelines of each lazy build (PG's outer-first hash join over a user-function
    /// build side): emit sets the build's `Pipeline::lazy_for` to the probe's program index; the probe's pipeline runs on
    /// the leader (`Mechs::shaped`, D1005).
    pub(crate) lazy: Vec<(usize, usize)>,
    /// D3609 (rerun/params_key.rs `input_cells`): graph pipelines whose walk evaluates a computed key at its seek and
    /// probes a hash build; each probe's make refuses unless it takes D1902's `Order` (probe.rs).
    pub(crate) seek_keyed: Vec<usize>,
    /// The program's `Program.locks` entries (a lock that is a `Program.locks` entry of its own), laid by lockrows plan.rs
    /// `Plan` for the lock's make; empty: the program's one lock plan. D3379 FAMCLAIM HASH: their own field, as the
    /// RowOp face's extension slot also holds a union write's arm state (modifytable rowop.rs `Arms`) in one program.
    pub(crate) locks: Vec<gravity_program::program::LockSite>,
    /// The lock plan's state (lockrows plan.rs `PlanState`: D4154 INITMULTI's handshake), written by its `Plan` only.
    pub(crate) lock_plan: crate::ops::lockrows::plan::PlanState,
    /// D2033 step 2: the graph pipelines a recheck keeps in the statement run's order (lockrows rerun.rs `first_held`,
    /// the program's `recheck_hold`).
    pub(crate) recheck_hold: Vec<usize>,
    /// D2033 step 3: of those, the epq probe's own held build (lockrows rerun.rs `epq_held`; the program's
    /// `recheck_hold_epq`).
    pub(crate) recheck_hold_epq: Vec<usize>,
    /// D1236's held inner (ops/mergejoin/held_inner.rs): the graph pipelines whose full Sort's error waits for a merge
    /// join's first inner call, or a pull merge's first read of the run's end (D449, ops/append/merge_pull.rs; the
    /// program's `Pipeline::held_sort`).
    pub(crate) held_sort: Vec<usize>,
    /// D2316: the `held_sort` Incremental Sort producers whose run keeps its rows (ops/sort/inc_held.rs; the program's
    /// `Pipeline::held_at`).
    pub(crate) held_at: Vec<usize>,
    /// D3769: the sites exec admit pairs with a named source ahead of its walk order, `(site, the view index of the
    /// pipeline whose source it reads)` (rerun/spool/site.rs `walk_site`; the program's `V4Descs::site_pins`).
    pub(crate) site_pins: Vec<(usize, usize)>,
    /// v5 errorhold: graph pipelines that depart under a recheck licence (`licensed`): emit sets their `Pipeline::recheck`.
    pub(crate) recheck: Vec<usize>,
    /// D1922 (the portal-kind fact): graph pipelines served only to a reader that takes every row (`whole_read`): emit sets
    /// their `Pipeline::whole_read`.
    pub(crate) whole_read: Vec<usize>,
    /// `DECISIONS.md` 4 (D) b: graph pipelines that run code ahead of PostgreSQL's plan under a tagged licence
    /// (`ahead_unchecked`), each with the plan node it names: emit sets their `Pipeline::ahead`.
    pub(crate) ahead: Vec<(usize, gravity_program::AheadTag)>,
    /// D3788 (`DECISIONS.md` 4 (D) b, build_class `raise_only`): `(build, reader, x)` per pipeline `x` a lazy group of that
    /// build and reader waits past only because `x` raises and acts nowhere, recorded when `lazy_group_by` admits the
    /// group (it reads `Px` shared); emit tags each `x` of a group declared lazy (`lazy`) as `ahead_unchecked` tags.
    pub(crate) lazy_raises: core::cell::RefCell<Vec<(usize, usize, usize)>>,
    /// D1443: graph pipelines that run on the leader (join.drain's seeded arms over a resident keyed end, F-ARMS).
    pub(crate) serial: Vec<usize>,
    /// D3024 (b): graph pipelines whose keyed group emit runs wide (table/emit_group.rs `Emit::new`, made before the end): a
    /// row sort or top at their end takes section ends though the decomposer's descriptor is serial (ops/sort/lower.rs,
    /// topk_rows.rs). D3336 (A2 (iii)): and a CTE store's reader whose sort its record claims fill (ops/ctescan/lower.rs).
    /// On the statement's mcx (the program's home), as `runs`.
    pub(crate) wide: PgVec<'g, usize>,
    /// D3024 (b), sorted: the run tables of the group emits that sort in place and run wide (table/emit_runs.rs), filed by
    /// their emits for their tops and the tops' drains.
    pub(crate) runs: PgVec<'g, crate::table::emit_runs::SortedRuns<'g>>,
    /// M7-DRAIN (V4R-378): the tails made into a pipeline after its own steps (join.drain's fused form, `open_tail`).
    pub(crate) tails: Vec<crate::frame::tail::Tail<'g>>,
    /// Literal occurrences placed before their own compile reads them (`claim_const`), each taken once (`take_claim`).
    claims: ArenaVec<(Node<'p>, Place)>,
    /// NEW-72: by-reference literal occurrences placed for a reader that needs the word whether or not a compile reads the
    /// literal (`offer_ref_const`): a compile that does takes it as a claim (`take_claim`); one left untaken is no bug.
    offered: ArenaVec<(Node<'p>, Place)>,
    /// `(expression root, literal, IN list element or u32::MAX, place)` per literal occurrence placed (`lit_word`).
    lits: ArenaVec<(Node<'p>, Node<'p>, u32, Place)>,
    /// D3585: `lits` by (root, literal, element) address, and each whole literal's first place (`lit_place`): a linear
    /// search per literal made placing them quadratic in the statement's literals (each arm of an Append over N partitions
    /// brings its own).
    lit_at: std::collections::HashMap<(usize, usize, u32), Place>, // alloc: Program
    lit_first: std::collections::HashMap<usize, Place>, // alloc: Program
    /// D3642: the word of each whole literal's share identity (`lit_word`): its copies in the arms of a set read it.
    /// Read by key only (no walk), so a hash map in the lowering's context (its owner the statement's) holds what the
    /// ordered map held, with a handful of shares inline.
    shared: mcx::PgMemoMap<'g, gravity_lower::inputs::key::LitShare<'p>, Place>,
    ext: Exts<'g>,
    /// The all-blockers mode's record of this lowering (`frame/diag.rs`, V4R-109); `None`: the mode is off.
    pub(crate) diag: Option<&'g core::cell::RefCell<crate::frame::diag::Log>>,
    /// D3168: the admission's content key, the staged probe's memo key (`LowerOpts::shape`).
    pub(crate) shape: Option<u128>,
    /// The exact request's home (`frame/iface.rs`): every node's request, filled before any make, and the answers made.
    pub(crate) iface: crate::frame::iface::Iface,
    /// The lifetime home's runtime half (`crate::lifetime`): every word declared with the scope that resets it.
    pub lifetimes: crate::lifetime::Lifetimes,
    /// Construct C2 (b): the licences held for graph pipelines run ahead of PostgreSQL's demand, by kind (`hold`),
    /// filled only while the tracer is on (frame/program.rs `stamps_on`) and stamped on the program's
    /// `Pipeline::licence`.
    licences: Vec<(usize, gravity_program::LicenceStamp)>,
    /// memsig E-prog: the lowering's lists root ('GravityHostedLists', owned by the running statement from its birth),
    /// made at the first `lists_mcx`: each compiled expression builds its program's lists here in place (step/expr.rs
    /// `compile_in_at`), and the finish takes the root with the programs (frame/program.rs, `hir::finish_keeping`), which
    /// compacts each list into the holder's root (a template's or the owner's product root) and drops this one. Not taken,
    /// it drops with the `Px`. LAST: every field above (`exs` and what a face keeps of a program) drops before it.
    pub(crate) lists_root: Option<mcx::SendRoot>,
}

impl<'g, 'p> Px<'g, 'p> {
    /// PLANMEM: the lowering's planning context (`Gx::mcx`): a mechanism made here keeps it (`mcx: Mcx<'g>`) for the
    /// scratch its lowering allocates (`pg.rs`).
    #[inline]
    pub fn mcx(&self) -> mcx::Mcx<'g> {
        self.gx.mcx
    }

    pub fn new(arena: ArenaRef, gx: &'g Gx<'g, 'p>, plan: &'g mut dyn WordPlan<'p>) -> Px<'g, 'p> {
        Px {
            arena,
            gx,
            plan,
            exs: Vec::new(),
            hosted_seen: core::cell::Cell::new((0, false)),
            sites: ArenaVec::new_in(arena),
            drafts: Vec::new(),
            pipe_sites: PgVec::new_in(gx.mcx),
            nominal_sites: PgVec::new_in(gx.mcx),
            late: Vec::new(),
            out: None,
            scratch: 0,
            codes: Vec::new(),
            data: Vec::new(),
            owned: Vec::new(),
            hosted_root: None,
            protos: Vec::new(),
            draft_protos: Vec::new(),
            write: None,
            write_k: None,
            writes: Vec::new(),
            strategies: StrategySet::EMPTY,
            word_keys: PgVec::new_in(gx.mcx),
            making: (usize::MAX, usize::MAX),
            passes: Vec::new(),
            pass_after: Vec::new(),
            new_slots: 0,
            unions: Vec::new(),
            lazy: Vec::new(), seek_keyed: Vec::new(), locks: Vec::new(), lock_plan: Default::default(), recheck_hold: Vec::new(), recheck_hold_epq: Vec::new(), held_sort: Vec::new(), serial: Vec::new(), wide: PgVec::new_in(gx.mcx), runs: PgVec::new_in(gx.mcx),
            recheck: Vec::new(),
            held_at: Vec::new(),
            site_pins: Vec::new(),
            whole_read: Vec::new(),
            ahead: Vec::new(),
            lazy_raises: Default::default(),
            tails: Vec::new(),
            claims: ArenaVec::new_in(arena),
            offered: ArenaVec::new_in(arena),
            lits: ArenaVec::new_in(arena),
            lit_at: Default::default(),
            lit_first: Default::default(),
            shared: mcx::PgMemoMap::new_in(gx.mcx),
            ext: Default::default(),
            diag: None,
            shape: None,
            iface: crate::frame::iface::Iface::of(gx),
            lifetimes: Default::default(),
            licences: Vec::new(),
            lists_root: None,
        }
    }

    /// v5 errorhold (interfaces §2.3, `Batch::Statement`): graph pipeline `k` departs from PostgreSQL's order under `l`.
    /// A recheck licence marks its `Pipeline::recheck`, so an error the statement raises before any row left it is checked
    /// by the plan-order run (exec `runner::recheck`); only a licence makes the bit. Ask `l` with `batch_effects`.
    /// C11 WriteOwner: the program's one write plan (`None`: a SELECT so far).
    pub(crate) fn write_plan(&self) -> Option<&WritePlan> {
        self.write.as_ref()
    }

    /// C11 WriteOwner: a second claimant of the one write plan (a lock or a write beside the one that holds it) refuses
    /// NotBuilt nest.cte at its node.
    pub(crate) fn second_write(&self, node: i32) {
        if self.write.is_some() {
            crate::not_built!(node, crate::admit::refusal::Variant::Kind(crate::face::kinds::KindId::NestCte));
        }
    }

    /// C11 WriteOwner: this make keeps the write plan.
    pub(crate) fn keep_write(&mut self, plan: WritePlan) {
        self.write = Some(plan);
    }

    /// C11 WriteOwner: the make at `node` claims the one write plan, `plan` copied into the program's arena
    /// (`second_write`, then `keep_write`).
    pub(crate) fn claim_write(&mut self, node: i32, plan: &WritePlan) {
        self.second_write(node);
        self.write = Some(plan.copy_in(self.arena));
    }

    /// C11 WriteOwner: the program's write plan, taken at emit.
    pub(crate) fn take_write(&mut self) -> Option<WritePlan> {
        self.write.take()
    }

    pub fn licensed(&mut self, k: usize, l: &gravity_decompose::effects::Licence) {
        if l.recheck() && !self.recheck.contains(&k) {
            self.recheck.push(k); // alloc: Program
        }
    }

    /// Construct C2 (b): graph pipeline `k` runs ahead of PostgreSQL's demand under `l`: its kind goes on the program's
    /// `Pipeline::licence`, which the run-ahead tracer reads (driver ahead_trace.rs). Recorded only while the tracer is
    /// on (frame/program.rs `stamps_on`): the lowering and its program are the same either way. The first licence held
    /// for `k` stays, but an accepted one (`Px::accept`, the class asked where no other licence covers the run) gives way
    /// to any other held later, which says more of the run (a recheck, a proof, a held error).
    pub fn hold(&mut self, k: usize, l: &gravity_decompose::effects::Licence) {
        use gravity_program::LicenceStamp as S;
        if !crate::frame::program::stamps_on() {
            return;
        }
        let at = self.licences.iter().position(|&(q, _)| q == k);
        if at.is_some_and(|i| self.licences[i].1 != S::Accepted || l.accepted()) {
            return;
        }
        let kind = if let Some(p) = l.exact() {
            S::Exact { proof: proof_stamp(p.kind()), node: p.node(), check: self.proof_check(k, &p) }
        } else if l.accepted() {
            S::Accepted
        } else if l.held_error() {
            S::Held
        } else if l.whole_read() {
            S::Whole
        } else if l.tagged() {
            S::Tagged
        } else if l.recheck() {
            S::Recheck
        } else {
            S::Plain
        };
        match at {
            Some(i) => self.licences[i].1 = kind,
            None => self.licences.push((k, kind)), // alloc: Program
        }
    }

    /// D1: what the lowering checks of exact proof `p` held for graph pipeline `k`: a PgReadsWhole proof cites a node
    /// whose input `k` runs (one of `k`'s plan nodes, its end's among them); the others' evidence was checked where the
    /// proof was built, and their timing nothing here can tell (`Unchecked`).
    fn proof_check(&self, k: usize, p: &gravity_decompose::effects::Proof) -> gravity_program::ProofCheck {
        use gravity_program::ProofCheck as C;
        if p.kind() != gravity_decompose::effects::ProofKind::PgReadsWhole {
            return C::Unchecked;
        }
        let x = self.gx.pipelines().get(k);
        match x.is_some_and(|x| x.node_id == p.node() || x.node_ops.iter().any(|n| n.node_id == p.node())) {
            true => C::Checked,
            false => C::Failed,
        }
    }

    /// The licence kind held for graph pipeline `k` (`hold`); `None`: none, or the tracer is off.
    pub(crate) fn held_kind(&self, k: usize) -> gravity_program::LicenceStamp {
        self.licences.iter().find(|&&(q, _)| q == k).map_or(gravity_program::LicenceStamp::None, |&(_, s)| s)
    }

    /// D1922 (the portal-kind fact, shared with D2316 / D2647): graph pipeline `k` departs from PostgreSQL's order only in
    /// what a reader that stops early would see (its first row waits on a run PostgreSQL makes as it hands rows on), so
    /// the program is served to a reader that takes every row; the entry delegates it under a cursor FETCH, an SPI cursor
    /// or PL/pgSQL FOR loop, or a SQL function's lazy row (exec admit/entry `PortalKind::partial`). This form holds no
    /// licence: the code run ahead acts (a VOLATILE call, a write), and acting code never qualifies for the whole-read
    /// licence, so its callers stay AHEAD-DEBT rows; code that does not act asks `read_whole`.
    pub fn whole_read(&mut self, k: usize) {
        if !self.whole_read.contains(&k) {
            self.whole_read.push(k); // alloc: Program
        }
    }

    /// `whole_read` under the whole-read licence (`effects::whole_read`): graph pipeline `k` runs code with effects `own`
    /// ahead of PostgreSQL's demand only for a reader that takes every row. Granted when `own` does not act: the program
    /// is marked as `whole_read` marks it (the guard the run-ahead tracer checks at run time) and the licence handed
    /// back. Denied (acting code), `k` is marked with no licence (`whole_read`'s form) and `None` comes back.
    pub fn read_whole(&mut self, k: usize, own: gravity_decompose::effects::Effects) -> Option<gravity_decompose::effects::Licence> {
        let l = gravity_decompose::effects::whole_read(own).ok();
        debug_assert!(l.is_none_or(|l| l.whole_read()));
        self.whole_read(k);
        if let Some(l) = &l {
            self.hold(k, l);
        }
        l
    }

    /// `DECISIONS.md` 4 (D) b: graph pipeline `k` runs code with effects `own` ahead of PostgreSQL's plan where no
    /// plan-order run re-checks it (the family has none, `ahead_of_rows` fails, or this is the plan-order run, where the
    /// family has no PostgreSQL form and asks it before a refusal). Code with no effects departs from nothing. Otherwise
    /// the departure goes when `licence_unchecked` grants it, asked with the statement's acting set and
    /// whether `k` or what feeds it calls user code (build_class `side`: a user or VOLATILE call, a row lock), or holds
    /// what that class reads as Raises without looking inside (`effects::hides_user_code`, a WindowAgg read into: a SubPlan,
    /// a Sort, a nest, a group's aggregates), where a user function may RAISE NOTICE or write (D3566). For code
    /// that may raise, `k` and every pipeline feeding it (build_class `feed`) are tagged, each with the top plan node it
    /// runs as EXPLAIN names it (`PipelineGraph::explain_of`; denied when EXPLAIN has no name for one), so an error a run
    /// of the part ends in names the subtree it came from for the harness. Denied, the family emits its PostgreSQL form or
    /// refuses by its own name.
    pub fn ahead_unchecked(&mut self, k: usize, own: gravity_decompose::effects::Effects) -> Option<gravity_decompose::effects::Licence> {
        use crate::ops::hashjoin::build_class::{feed, side, Side};
        use gravity_decompose::effects::{licence_unchecked, unobservable, Effects};
        if own == Effects::NONE {
            return unobservable(own).inspect(|l| self.hold(k, l));
        }
        let gx = self.gx;
        let user_code = side(gx, k) == Side::UserFn || crate::effects::hides_user_code(gx, k);
        let l = licence_unchecked(own, user_code).ok()?;
        if !l.tagged() {
            self.hold(k, &l);
            return Some(l);
        }
        let ps = gx.pipelines();
        let src = if let gravity_decompose::Source::Breaker { slot } = ps[k].source { Some(slot) } else { None };
        // D2849: a hash of a key under no collation raises 42P22, which no unchecked licence covers
        if std::iter::once(k).chain(feed(gx, k, ps[k].desc.consumes.iter().copied().chain(src))).any(|q| crate::effects::hashes_raising_key(gx, q)) {
            return None;
        }
        let mut tags = PgVec::new_in(self.mcx());
        for q in std::iter::once(k).chain(feed(gx, k, ps[k].desc.consumes.iter().copied().chain(src))) {
            let t = ahead_tag(gx, q)?;
            tags.push_in((q, t));
        }
        for (q, t) in tags {
            if !self.ahead.iter().any(|&(x, _)| x == q) {
                self.ahead.push((q, t)); // alloc: Program
            }
        }
        self.hold(k, &l);
        Some(l)
    }

    /// v5 errorhold: the effects a departure asks its licence with: its own, and when it may raise (its licence then
    /// carries a recheck) the statement's acting set too (`PipelineGraph::acts`): the plan-order run repeats the whole
    /// statement, so an acting call anywhere in it denies the licence.
    pub fn batch_effects(&self, own: gravity_decompose::effects::Effects) -> gravity_decompose::effects::Effects {
        use gravity_decompose::effects::Effects;
        if own.may_raise { own.and(Effects { acts: self.gx.graph.acts, ..Effects::NONE }) } else { own }
    }

    /// v5 errorhold: graph pipeline `k` runs code with effects `own` ahead of PostgreSQL's order (an eager build or inner
    /// PostgreSQL may never run). Code with no effects departs from nothing. Otherwise the departure goes when the licence
    /// is granted (`batch_effects`: an acting statement denies it, and so does the plan-order run) and, for code that may
    /// raise, `k` runs whole before any row can leave the statement (`ahead_of_rows`: the plan-order run answers only an
    /// error raised before then) and neither it nor what feeds it calls a user or VOLATILE function; the pipeline is then
    /// marked for that run. Denied, the family emits its PostgreSQL form or refuses by its own name.
    pub fn run_ahead(&mut self, k: usize, own: gravity_decompose::effects::Effects) -> Option<gravity_decompose::effects::Licence> {
        use gravity_decompose::effects::{licence, unobservable, Departure, Effects};
        if own == Effects::NONE {
            return unobservable(own).inspect(|l| self.hold(k, l));
        }
        // v4's acting rule over `k` and all that feeds it (join/build_class.rs `side`): a user or VOLATILE call is never run
        // ahead, whatever the licence (a PL error inside its own SPI may not re-run: D2573)
        // D3587: nor where something in it hides a call from that class (`effects::hides_user_code`, as `ahead_unchecked`
        // asks) in a statement that calls user code (`effects::statement_user_code`): a user function there may RAISE
        // NOTICE before the plan-order run takes the statement; a statement of PostgreSQL's own code runs none
        let hides = || crate::effects::hides_user_code(self.gx, k) && crate::effects::statement_user_code(self.gx, self.arena);
        // D3816: a pinned non-acting builtin (a disk size, the lock status) is read as builtin code there (effects
        // visible/hashjoin.rs `acting_side`)
        if own.may_raise && (!self.ahead_of_rows(k) || crate::effects::visible::hashjoin::acting_side(self.gx, k) == crate::ops::hashjoin::build_class::Side::UserFn || hides()) {
            return None;
        }
        let l = licence(self.batch_effects(own), Departure::Ahead).ok()?;
        self.licensed(k, &l);
        self.hold(k, &l);
        // Construct C2 (b): what feeds `k` runs before it under the same licence. The acting rule above read `k` with its
        // feeders (`side`, `hides_user_code`), and the plan-order run that answers an error of `k` answers any error the
        // statement raises before a row left it, theirs too (exec `runner::rechecks`: one recheck pipeline in the program)
        if crate::frame::program::stamps_on() {
            let ps = self.gx.pipelines();
            let src = if let gravity_decompose::Source::Breaker { slot } = ps[k].source { Some(slot) } else { None };
            for q in crate::ops::hashjoin::build_class::feed(self.gx, k, ps[k].desc.consumes.iter().copied().chain(src)) {
                self.hold(q, &l);
            }
        }
        Some(l)
    }

    /// Construct C2, the accepted class: graph pipeline `k` runs code with effects `own` (`k` with what feeds it) ahead of
    /// PostgreSQL's demand where no other licence covers it. Code with no effects departs from nothing. Code that does not
    /// act holds `effects::accepted` (an error PostgreSQL never hits, from early work), asked with whether `k` or what
    /// feeds it calls user code (`effects::fed_user_code`, the class `ahead_unchecked` reads); code that acts holds
    /// `effects::volatile_free` over the whole statement (every act a call-local builtin's, nothing in the statement that
    /// could read one, no write, no row lock). The licence is held on `k` and on what feeds it (build_class `feed`) for
    /// the run-ahead tracer; codegen reads nothing of it. `None`: neither grants, and the run stays the tracer's key.
    pub fn accept(&mut self, k: usize, own: gravity_decompose::effects::Effects) -> Option<gravity_decompose::effects::Licence> {
        use gravity_decompose::effects::{accepted, unobservable, volatile_free, Cx, Effects};
        if own == Effects::NONE {
            return unobservable(own).inspect(|l| self.hold(k, l));
        }
        let gx = self.gx;
        let l = if own.acts {
            let s = gx.graph.stmt?;
            volatile_free(&Cx::new(s, gx.facts, self.arena), s.planTree.into_iter().chain(s.subplans.iter().flatten())).ok()?
        } else {
            accepted(own, crate::effects::fed_user_code(gx, k)).ok()?
        };
        self.hold(k, &l);
        if crate::frame::program::stamps_on() {
            let ps = gx.pipelines();
            let src = if let gravity_decompose::Source::Breaker { slot } = ps[k].source { Some(slot) } else { None };
            for q in crate::ops::hashjoin::build_class::feed(gx, k, ps[k].desc.consumes.iter().copied().chain(src)) {
                self.hold(q, &l);
            }
        }
        Some(l)
    }

    /// v5 errorhold: graph pipeline `k` runs whole, once, before any row can leave the statement, so an error it raises
    /// finds nothing sent (exec `runner::recheck` re-runs only then). It is a graph pipeline (not a nest inner's), in no
    /// rerun set (a set runs again per outer row, a recursive term per generation) but a set kept whole (`kept_ahead`),
    /// feeding no set run on demand (an InitPlan's cell or a hashed SubPlan's table may first be read after rows left), and
    /// no pipeline that hands rows to the statement's sink (an emit: the driver's `is_emit`) comes at or before it: the
    /// driver runs the others in index order. A CTE's set is pulled by its readers (exec ctescan/proto.rs): it runs ahead
    /// of rows when each reader is a graph pipeline that does by the rules above and no embedded inner reads it.
    pub fn ahead_of_rows(&self, k: usize) -> bool {
        crate::stack_check();
        use gravity_decompose::ops::shared::subplan::SubForm;
        let (gx, ps) = (self.gx, self.gx.pipelines());
        let init = |q: usize| gx.graph.init_plans.iter().any(|i| i.pipeline as usize == q);
        let emits = |q: usize| !matches!(gx.end_kind(q), crate::graph::EndKind::Breaker(_)) && !init(q);
        let hashed = (0..ps.len()).flat_map(move |q| ps[q].ops.iter().filter_map(move |op| match op {
            gravity_decompose::Op::SubPlan(d) => match d.form {
                SubForm::Hashed { slot } => gx.life_of(q, slot).map(|l| l.0 as usize),
                _ => None,
            },
            _ => None,
        }));
        // D3585: in the graph's lowering, the first emitting pipeline and the sets are found once (memo.rs), not by a walk
        // of every pipeline for each `k` (quadratic in an Append's arms)
        let key = crate::memo::key_of(&gx.graph.pipelines);
        let first = crate::memo::read(key, |m| m.first_emit).map(|x| {
            x.unwrap_or_else(|| {
                let first = (0..ps.len()).find(|&q| emits(q)).map(|q| q as u32);
                crate::memo::write(key, |m| m.first_emit = Some(first));
                first
            })
        });
        let emits_by = |r: usize| match first {
            Some(f) => f.is_some_and(|f| f as usize <= r),
            None => (0..=r).any(emits),
        };
        let sets: PgVec<'_, usize> = match crate::memo::read(key, |m| m.ahead_sets.clone()) {
            Some(Some(s)) => s.iter().map(|&q| q as usize).collect_in(self.mcx()),
            kept => {
                let s: PgVec<'_, usize> = gx.graph.init_plans.iter().map(|i| i.pipeline as usize).chain(hashed).collect_in(self.mcx());
                if kept.is_some() {
                    let s: std::rc::Rc<[u32]> = s.iter().map(|&q| q as u32).collect(); // alloc: Program
                    crate::memo::write(key, |m| m.ahead_sets = Some(s));
                }
                s
            }
        };
        let ahead = |r: usize| {
            r + 1 < ps.len() && !emits_by(r) && !gx.graph.reruns.iter().any(|x| x.pipelines.iter().any(|&p| p as usize == r)) && !sets.iter().any(|&q| feeds(gx, q, r))
        };
        let inner = |q: usize| ps.inners().iter().any(|(_, p)| p.desc.consumes_from.contains(&gravity_program::LifeId(q as u16)));
        let pulled = |q: usize| gx.cte_write(q) && !inner(q) && (0..ps.len()).filter(|&r| r != q && crate::ops::shared::cte_reads::reads(gx, r).contains(&q)).all(ahead);
        k + 1 < ps.len()
            && !emits_by(k)
            && (!gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&p| p as usize == k)) || self.kept_ahead(k))
            && !sets.iter().any(|&q| feeds(gx, q, k) && !pulled(q))
    }

    /// D3217 (`ahead_of_rows`): every rerun set holding graph pipeline `k` is kept whole (rerun/sets.rs `kept_whole`: it
    /// runs its members once, at its first run) and its owner, the graph pipeline whose nest opens it (`set_owner`), comes
    /// after `k` and runs ahead of rows itself: the set's one run then comes before any row leaves.
    fn kept_ahead(&self, k: usize) -> bool {
        let gx = self.gx;
        gx.graph.reruns.iter().enumerate().filter(|(_, r)| r.pipelines.contains(&(k as u16))).all(|(i, r)| {
            crate::rerun::sets::kept_whole(gx, r) && crate::rerun::sets::set_owner(gx, i).is_some_and(|o| o > k && self.ahead_of_rows(o))
        })
    }

    /// D4203 (C2 (b)'s "before any row can leave" half for mapping's own departures): graph pipeline `q` runs a node
    /// mapping read ahead of PostgreSQL's demand under a recheck licence (`PipelineGraph::licensed_nodes`: a sorted group
    /// folded whole before its first group is handed on, a window's fill read past a stopping Limit), and may run after a
    /// row left the statement, where that licence's plan-order run cannot answer its error (exec admit `recheck`:
    /// `sent == 0`). `run_ahead` asks `ahead_of_rows` of the lowering's own departures; mapping cannot see when its nodes
    /// run, so this asks it of them, and says which road (`Late`):
    /// - `q` in a CTE's plan (exec ctescan/proto.rs: the plan runs at a reader's pull): `Ordered` when some reader's
    ///   pulling graph pipeline (an embedded inner's owner, through every level, none in a rerun set) runs in program
    ///   order after an emitting pipeline; `Unanswered` when a reader (or an owner on the way) sits in a rerun set
    ///   (pulled again at a re-open), runs on demand (an InitPlan's, a hashed SubPlan's table, a lazy build) or is another
    ///   CTE's plan: when that pull comes is not known here;
    /// - else the same of `q` itself.
    /// Its one caller is ops/ctescan/lower.rs (the CTE pull road). An `Unanswered` road, and a late node on another road,
    /// is C2 debt (driver ahead_trace.rs's Recheck-after-send is the backstop).
    pub fn licensed_late(&self, q: usize) -> Late {
        crate::stack_check();
        use gravity_decompose::ops::shared::subplan::{SubForm, SubLink};
        let (gx, ps) = (self.gx, self.gx.pipelines());
        let licensed = q < ps.len() && gx.graph.licensed_nodes.iter().any(|&id| ps[q].node_id == id || ps[q].node_ops.iter().any(|o| o.node_id == id));
        if !licensed {
            return Late::No;
        }
        let init = |x: usize| gx.graph.init_plans.iter().any(|i| i.pipeline as usize == x);
        let first = (0..ps.len()).find(|&x| !matches!(gx.end_kind(x), crate::graph::EndKind::Breaker(_)) && !init(x));
        let rerun = |x: usize| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(x as u16)));
        let hashed = (0..ps.len()).flat_map(|x| {
            ps[x].ops.iter().filter_map(move |op| match op {
                gravity_decompose::Op::SubPlan(d) => match d.form {
                    SubForm::Hashed { slot } => gx.life_of(x, slot).map(|l| l.0 as usize),
                    _ => None,
                },
                _ => None,
            })
        });
        let cte = |i: &&gravity_decompose::graph::InitPlan<'_>| matches!(i.link, SubLink::Cte);
        let demand: PgVec<'_, usize> = gx.graph.init_plans.iter().filter(|i| !cte(i)).map(|i| i.pipeline as usize).chain(hashed).collect_in(self.mcx());
        let on = |x: usize| x >= ps.len() || rerun(x) || ps[x].desc.lazy_for >= 0 || demand.iter().any(|&d| feeds(gx, d, x));
        let after = |x: usize| first.is_some_and(|f| f < x);
        let road = |again: bool, x: usize| match (again || on(x) || crate::ops::ctescan::lower::is_cte(gx, x), after(x)) {
            (false, true) => Late::Ordered,
            (true, _) => Late::Unanswered,
            (false, false) => Late::No,
        };
        let ctes: PgVec<'_, usize> = gx.graph.init_plans.iter().filter(cte).map(|i| i.pipeline as usize).filter(|&k| feeds(gx, k, q)).collect_in(self.mcx());
        if ctes.is_empty() {
            return road(false, q);
        }
        let inners = ps.inners();
        let pulls = |p: usize| {
            let (mut r, mut again) = (p, rerun(p));
            while r >= ps.len() {
                r = inners[r - ps.len()].0 .0;
                again |= rerun(r);
            }
            road(again, r)
        };
        let readers = |k: usize| (0..ps.end()).filter(move |&p| p != k && crate::ops::shared::cte_reads::reads(gx, p).contains(&k));
        ctes.into_iter().flat_map(readers).map(pulls).max().unwrap_or(Late::No)
    }

    pub fn add_ex(&mut self, c: Compiled<'p>) -> ExId {
        self.add_placed(Ex { c, places: Vec::new(), needs: Vec::new(), ext: None, held: Vec::new(), volatile: false, late: None }) // alloc: Program
    }

    /// Places `e`, its `volatile` read here, where the catalog is (effects/visible/expr.rs `volatile_site`), joined to
    /// the bit its compiler read from the plan node (a claim operand's: step/expr.rs `compile_in`).
    pub fn add_placed(&mut self, mut e: Ex<'p>) -> ExId {
        e.volatile |= crate::effects::visible::expr::volatile_site(self.gx.facts, &e.c);
        e.late = crate::frame::hoist::late_roots(self.gx, &e);
        self.exs.push(e); // alloc: Program
        ExId(self.exs.len() as u32 - 1)
    }

    pub fn ex(&self, id: ExId) -> &Compiled<'p> {
        &self.exs[id.0 as usize].c
    }

    pub fn ex_mut(&mut self, id: ExId) -> &mut Compiled<'p> {
        self.hosted_seen.set((0, false));
        &mut self.exs[id.0 as usize].c
    }

    /// D3585: whether an expression placed so far is `unsafe_ex` (hir.rs `hosted_serial`: a hosted call a pool thread
    /// cannot run). Each heap source asks it of every expression of the program; the answer is kept over the expressions
    /// already read (they are only added until `take_exs`), so a program of N arms reads each once, not N times.
    pub fn hosted_unsafe(&self, unsafe_ex: &dyn Fn(&Compiled<'p>) -> bool) -> bool {
        let (seen, any) = self.hosted_seen.get();
        let (mut n, mut any) = if seen <= self.exs.len() { (seen, any) } else { (0, false) };
        while !any && n < self.exs.len() {
            any = unsafe_ex(&self.exs[n].c);
            n += 1;
        }
        self.hosted_seen.set((n, any));
        any
    }

    pub fn placed(&self, id: ExId) -> &Ex<'p> {
        &self.exs[id.0 as usize]
    }

    pub fn nexs(&self) -> usize {
        self.exs.len()
    }

    pub fn add_site(&mut self, s: SiteDesc) -> usize {
        self.sites.push(s);
        self.sites.len() - 1
    }

    /// Pipeline `k`'s site drafted by its source's make, which its steps and end then declare on (verdicts, answer lanes,
    /// per-code tables) in their makes; `Mechs::new` numbers it in place of `SourceMech::site_desc`.
    pub fn set_site(&mut self, k: usize, s: SiteDesc) {
        let k = self.ident(k);
        self.drafts.retain(|d| d.0 != k);
        self.drafts.push((k, s)); // alloc: Program
    }

    pub fn site_mut(&mut self, k: usize) -> Option<&mut SiteDesc> {
        let k = self.ident(k);
        self.drafts.iter_mut().find(|d| d.0 == k).map(|d| &mut d.1)
    }

    /// Pipeline `k`'s numbered driving site: `None` before its `Mechs::new` or without a site (a producer is always made
    /// before the pipelines that read its breaker).
    pub fn pipeline_site(&self, k: usize) -> Option<usize> {
        self.pipe_sites.get(k).copied().flatten().map(usize::from)
    }

    /// The view indices `pipeline_site` may name a site for: those below this.
    pub(crate) fn pipe_sites_len(&self) -> usize {
        self.pipe_sites.len()
    }

    pub fn set_pipeline_site(&mut self, k: usize, site: Option<usize>) {
        if self.pipe_sites.len() <= k {
            self.pipe_sites.resize_in(k + 1, None);
        }
        self.pipe_sites[k] = site.map(|s| s as u16);
    }

    /// The site pipeline `k`'s claims are cut over: its numbered driving site (`pipeline_site`), else the nominal one its
    /// source named (`SourceMech::nominal_site`: a wide group emit's, a co-split merge's, a spool reader's), which the
    /// runner reads no row through. `None` before its `Mechs::new` or for a pipeline with neither (it runs on the leader).
    pub fn claims_site(&self, k: usize) -> Option<usize> {
        self.pipeline_site(k).or_else(|| self.nominal_sites.get(k).copied().flatten().map(usize::from))
    }

    /// Pipeline `k`'s nominal driving site (`claims_site`), recorded where it has no drafted one.
    pub(crate) fn set_nominal_site(&mut self, k: usize, site: Option<usize>) {
        if self.nominal_sites.len() <= k {
            self.nominal_sites.resize_in(k + 1, None);
        }
        self.nominal_sites[k] = site.map(|s| s as u16);
    }

    pub fn take_site(&mut self, k: usize) -> Option<SiteDesc> {
        let k = self.ident(k);
        let i = self.drafts.iter().position(|d| d.0 == k)?;
        Some(self.drafts.swap_remove(i).1)
    }

    /// The frame word of `key`, numbered on first use.
    pub fn frame_word(&mut self, key: WordKey) -> u16 {
        // a step made for another program pipeline (a pass, a swap) keys by that pipeline and its own graph position, so it
        // never shares a word with that pipeline's own op j
        let moved = |px: &Self, k: usize, j: u64| if px.making.0 == k && px.making.0 != px.making.1 { (px.making.1, j ^ (k as u64 + 1) << 32) } else { (k, j) };
        let key = match key {
            WordKey::Open { face, k, j, i } => {
                let (k, j) = moved(self, k, j);
                WordKey::Open { face, k, j, i }
            }
            WordKey::Resume(ResumeWord::StepOffset { k, j }) => {
                let (k, j) = moved(self, k, j);
                WordKey::Resume(ResumeWord::StepOffset { k, j })
            }
            WordKey::Resume(ResumeWord::EpqRecheck { k }) => WordKey::Resume(ResumeWord::EpqRecheck { k: self.ident(k) }),
            WordKey::Union { .. } | WordKey::UnionFlag { .. } if self.making.0 != self.making.1 && !self.tailing() => self.bug("a union word declared by a remade pipeline"),
            key => key,
        };
        match self.word_keys.iter().position(|&w| w == key) {
            Some(i) => i as u16,
            None => {
                self.word_keys.push_in(key);
                self.word_keys.len() as u16 - 1
            }
        }
    }

    /// A site numbered at emit, after every source site and before the words site; its number is in the cell from then on.
    pub fn add_late_site(&mut self, s: SiteDesc) -> Rc<core::cell::Cell<usize>> {
        let at = Rc::new(core::cell::Cell::new(usize::MAX));
        self.late.push((s, at.clone())); // alloc: Program
        at
    }

    /// The late sites numbered, in request order (emit, before the words site); first the scans a recheck runs again whole
    /// named (D3497, marks.rs `place_rescans`).
    pub fn place_late_sites(&mut self) {
        crate::rerun::epq::marks::place_rescans(self);
        for (mut s, at) in core::mem::take(&mut self.late) {
            crate::rerun::epq::marks::place_rows(self, &mut s);
            at.set(self.add_site(s));
        }
    }

    /// The words site, after every other (a driving-site-less pipeline's site 0 stays a source's): `None` without words.
    pub fn add_words_site(&mut self) -> Option<usize> {
        let n = self.word_keys.len() as u16;
        (n > 0).then(|| self.add_site(SiteDesc::words(self.arena, n)))
    }

    pub fn set_union(&mut self, k: usize, union: u32, j: usize) {
        self.unions.push((self.ident(k), union, j)); // alloc: Program
    }

    /// The union pipeline `k` op `j` is under: the nearest arm marker before it (an inner union's marker comes first).
    pub fn union_of(&self, k: usize, j: usize) -> Option<(u32, usize)> {
        let k = self.ident(k);
        self.unions.iter().filter(|u| u.0 == k && u.2 < j).max_by_key(|u| u.2).map(|u| (u.1, u.2))
    }

    pub fn scratch_at_least(&mut self, bytes: u32) {
        self.scratch = self.scratch.max(bytes);
    }

    /// A runtime protocol the program declares (`gravity_program::v4::Proto`); the runner runs it per execution.
    pub fn declare_proto(&mut self, mut p: Proto) {
        p.pipeline = self.ident(p.pipeline as usize) as u16;
        if p.site == DRAFT_SITE {
            self.draft_protos.push(self.protos.len() as u32); // alloc: Program
        }
        self.protos.push(p); // alloc: Program
    }

    /// Face `f`'s compile-time state, made on first use. Only `f`'s own mechanisms name `T`.
    pub fn ext_mut<T: ExtG<'g>>(&mut self, f: Face) -> &mut T {
        let node = self.gx.root_node();
        match self.ext.get_or_make::<T>(f, self.gx.mcx) {
            Some(t) => t,
            None => crate::refuse!(node, crate::admit::refusal::Reason::Bug { payload: "a face's extension slot holds another type" }),
        }
    }

    pub fn ext<T: ExtG<'g>>(&self, f: Face) -> Option<&T> {
        self.ext.get(f)
    }

    /// The identity pipeline `k`'s makers declare sites, words and protocols under: `k` itself, or the program pipeline
    /// `remake` (or a swap) makes `k`'s shape for. `set_site` / `site_mut` / `frame_word` / `declare_proto` apply it.
    pub fn ident(&self, k: usize) -> usize {
        if self.making.0 == k { self.making.1 } else { k }
    }

    /// Runs `f` with graph pipeline `shape`'s makers declaring under `ident` (resolve, `remake`, `Mechs::new`).
    pub(crate) fn making<R>(&mut self, shape: usize, ident: usize, f: impl FnOnce(&mut Self) -> R) -> R {
        let saved = core::mem::replace(&mut self.making, (shape, ident));
        let r = f(self);
        self.making = saved;
        r
    }

    /// The base `proto_words` gives next (D4197: the `keys.user` word a program's functions read before it is declared).
    pub fn next_proto_word(&self) -> u16 {
        u16::try_from(self.plan.nwords()).unwrap_or(u16::MAX)
    }

    /// `n` contiguous parameter words no expression owns, zeroed at bind (`WordSrc::Kernel`): only a protocol writes them
    /// (`Proto.words` = `[base, base + n)`); the IR reads them through `env.params`.
    pub fn proto_words(&mut self, n: u16) -> u16 {
        let base = self.plan.nwords();
        self.plan.reserve(base + n as usize);
        if self.plan.nwords() != base + n as usize || base + n as usize > u16::MAX as usize {
            self.bug("the word plan did not reserve the protocol's words");
        }
        base as u16
    }

    pub(crate) fn bug(&self, payload: &'static str) -> ! {
        crate::refuse!(self.gx.root_node(), crate::admit::refusal::Reason::Bug { payload })
    }

    /// By-value literal `node`'s word for a reader ahead of its compile (the HAVING K's floor), taken once by that compile
    /// (step/consts.rs): one occurrence stays one word, as exec admit's per-occurrence template needs.
    pub fn claim_const(&mut self, node: Node<'p>) -> Option<Place> {
        let (nd, typ) = by_value_need(node)?;
        if let Some(&(_, p)) = self.claims.iter().find(|x| x.0.ptr_eq(node)) {
            return Some(p);
        }
        let p = self.plan.need_at(&ParamNeed::Const(nd), typ, true, node);
        self.claims.push((node, p));
        Some(p)
    }

    /// By-value literal `node`'s word for a reader ahead of the compile that files it under expression root `root`
    /// (step/consts.rs `place`: one word per (root, literal) occurrence, `lit_word`, D317): that compile's word, placed now
    /// or found. Unlike `claim_const` it leaves no claim to take, so a root whose reader and compile run more than once
    /// (the end each arm of a union makes over one aggregate, D2958) reads its one word every time.
    pub fn root_const(&mut self, root: Node<'p>, node: Node<'p>) -> Option<Place> {
        let (nd, typ) = by_value_need(node)?;
        Some(self.lit_word(root, node, u32::MAX, |px| px.plan.need_at(&ParamNeed::Const(nd), typ, true, node)))
    }

    /// `claim_const` for a by-reference literal (its word is its pointer, bound per execution): a reader ahead of its
    /// compile (lowerv4 ops/scan/pgrc/lower.rs `tid_bounds`, a ctid clause's tid), the word taken once by that compile.
    /// `None` for a NULL or by-value literal.
    pub fn claim_ref_const(&mut self, node: Node<'p>) -> Option<Place> {
        let c = node.as_const().filter(|c| !c.constbyval && !c.constisnull)?;
        if let Some(&(_, p)) = self.claims.iter().find(|x| x.0.ptr_eq(node)) {
            return Some(p);
        }
        let p = self.plan.need_at(&ParamNeed::Const(NullableDatum::value(c.constvalue)), c.consttype, false, node);
        self.claims.push((node, p));
        Some(p)
    }

    /// NEW-72: `claim_ref_const` for a reader that needs the word whatever the compiles do (an IN list's array, bound per
    /// execution for its zone predicates, lowerv4 ops/scan/pgrc/verdict_zone.rs `list_word`): a compile that places the
    /// literal itself (a long list's array as an operand, step/expr_saop.rs `Arr::Sub`) takes this word, so the occurrence
    /// stays one word (exec admit `template`); a compile that places only its elements' words or its set leaves it, and
    /// that is no bug. `None` for a NULL or by-value literal.
    pub fn offer_ref_const(&mut self, node: Node<'p>) -> Option<Place> {
        let c = node.as_const().filter(|c| !c.constbyval && !c.constisnull)?;
        if let Some(&(_, p)) = self.claims.iter().chain(self.offered.iter()).find(|x| x.0.ptr_eq(node)) {
            return Some(p);
        }
        let p = self.plan.need_at(&ParamNeed::Const(NullableDatum::value(c.constvalue)), c.consttype, false, node);
        self.offered.push((node, p));
        Some(p)
    }

    /// NEW-72: the place a compile already gave literal `at` as a whole (`lit_word`, never an IN list element's), the
    /// first such occurrence: a reader after the compile shares its word.
    pub fn lit_place(&self, at: Node<'p>) -> Option<Place> {
        self.lit_first.get(&(at.as_raw().as_ptr() as usize)).copied()
    }

    /// NEW-72: the word `offer_ref_const` offered literal `node`, while no compile took it (`take_claim`; one that did
    /// placed it at `lit_place`).
    pub fn offered_place(&self, node: Node<'p>) -> Option<Place> {
        self.offered.iter().find(|x| x.0.ptr_eq(node)).map(|x| x.1)
    }

    pub fn take_claim(&mut self, node: Node<'p>) -> Option<Place> {
        if let Some(i) = self.claims.iter().position(|x| x.0.ptr_eq(node)) {
            return Some(self.claims.remove(i).1);
        }
        let i = self.offered.iter().position(|x| x.0.ptr_eq(node))?;
        Some(self.offered.remove(i).1)
    }

    /// Literal `at` (element `k` of IN list `at`, else u32::MAX) under root `root`: its first compile's place, else
    /// `place`'s (step/consts.rs). A root compiled twice (a top's drain re-projecting its entries, D317; a claimed
    /// call) reads one word per traversal occurrence, as exec admit's template needs. D3642: a whole literal with a share
    /// identity (gravity_lower key.rs `lit_share`: the copies of one literal in a set's arms, one EC's Const under its
    /// clauses) reads its identity's word under every root, the keys holding its copies as one node (key.rs `alias`), so
    /// the program's words follow the statement, not its arm count; a literal a reader claimed ahead keeps that word.
    pub fn lit_word(&mut self, root: Node<'p>, at: Node<'p>, k: u32, place: impl FnOnce(&mut Self) -> Place) -> Place {
        let key = (root.as_raw().as_ptr() as usize, at.as_raw().as_ptr() as usize, k);
        if let Some(&p) = self.lit_at.get(&key) {
            return p;
        }
        let share = if k == u32::MAX { gravity_lower::inputs::key::lit_share(at) } else { None };
        let share = share.filter(|_| !self.claims.iter().chain(self.offered.iter()).any(|x| x.0.ptr_eq(at)));
        let p = match share.and_then(|s| self.shared.get(&s).copied()) {
            Some(p) => {
                self.plan.follow(p, at);
                p
            }
            None => place(self),
        };
        if let (Some(s), Place::Word(_)) = (share, p) {
            if self.shared.get(&s).is_none() {
                self.shared.insert(s, p);
            }
        }
        self.lits.push((root, at, k, p));
        self.lit_at.entry(key).or_insert(p); // alloc: Program (the first placed wins, as the search found it)
        if k == u32::MAX {
            self.lit_first.entry(key.1).or_insert(p); // alloc: Program
        }
        p
    }

    pub(crate) fn take_exs(&mut self) -> Vec<Ex<'p>> {
        if !self.claims.is_empty() {
            self.bug("an unconsumed literal claim");
        }
        self.hosted_seen.set((0, false));
        core::mem::take(&mut self.exs)
    }

    pub(crate) fn exs_mut(&mut self) -> &mut Vec<Ex<'p>> {
        self.hosted_seen.set((0, false));
        &mut self.exs
    }
}

/// By-value or NULL literal `node`'s need (its canonical word, never baked: a word bound per execution) and type, for
/// `Px::claim_const` / `Px::root_const`; `None` for a by-reference literal or another node.
fn by_value_need(node: Node<'_>) -> Option<(NullableDatum, u32)> {
    let c = node.as_const().filter(|c| c.constbyval || c.constisnull)?;
    let nd = if c.constisnull { NullableDatum::null() } else { NullableDatum::value(gravity_expr::nodes::konst::word_canon(c.consttype, c.constvalue)) };
    Some((nd, c.consttype))
}

/// `DECISIONS.md` 4 (D) b (`Px::ahead_unchecked`, D3788's `lazy_raises`): the tag of graph pipeline `q` running ahead, the
/// top plan node it runs as EXPLAIN names it (`PipelineGraph::explain_of`); `None` when EXPLAIN has no name for it.
pub(crate) fn ahead_tag(gx: &Gx<'_, '_>, q: usize) -> Option<gravity_program::AheadTag> {
    let p = &gx.pipelines()[q];
    let top = p.node_ops.iter().rev().find(|n| !n.producer_top).or(p.node_ops.last()).map_or(p.node_id, |n| n.node_id);
    gx.graph.explain_of(top)
}

/// An extra pass's mechanisms (`Px::add_pass`), made by the pipeline it follows.
pub struct ExtraPass<'g> {
    pub source: PgBox<'g, dyn SourceMech + 'g>,
    pub steps: PgVec<'g, PgBox<'g, dyn StepMech + 'g>>,
    pub end: PgBox<'g, dyn EndMech + 'g>,
}


/// The statement census facts several families read (C11: the lock plan's, ops/lockrows/plan.rs `Census`, and the
/// hash drain's and row op's admissions), on the graph context so no family names another's module.
impl Gx<'_, '_> {
    /// D3379 LOCKPOST: the pipelines that run after the statement's emit and never at a recheck: `drained`'s, and the post
    /// pipelines of an unread data-modifying CTE without RETURNING (ops/modifytable/post.rs `set`).
    pub(crate) fn after_emit(&self) -> Vec<usize> {
        let mut v = crate::rerun::epq::marks::drained(self);
        let post: PgVec<'_, usize> = crate::ops::modifytable::post::set(self).into_iter().flatten().filter(|q| !v.contains(q)).collect_in(self.mcx);
        v.extend(post); // alloc: Program
        v
    }

    /// The data-modifying CTEs' UPDATE / DELETE pipelines `qs` beside a CTE claim and the statement's own write, at least
    /// one, each rechecking in its own IR (modifytable rowop_epq.rs `cte_in_ir`): no writer but the statement's stops for
    /// the runner's EvalPlanQual, and none reruns the claim (each reads its row from its COPY mark).
    pub(crate) fn ctes_beside(&self, qs: &[usize]) -> bool {
        !qs.is_empty() && qs.iter().all(|&q| crate::ops::modifytable::rowop_epq::cte_in_ir(self, q))
    }

    /// D3276 (DRAINED LOCK): the statement's row locks (outside every CTE, no write of its own) beside writing CTEs that are
    /// all rerun/epq/marks.rs `drained` (nothing reads them: they run after the statement's emit, and the lock's recheck
    /// never runs them, as EvalPlanQual's tree holds no CteScan of them), each an INSERT or a write that rechecks in its own
    /// IR (`cte_write_alone`), and no lock inside a CTE. The CTE's write is then a write plan beside the lock's.
    /// D3379 INITCTE: or writing CTEs only the statement's non-CTE InitPlans read, each write an INSERT (lockrows lock_init.rs
    /// `init_ctes`): PostgreSQL runs such a CTE when the InitPlan first reads it, and the lock's EvalPlanQual runs neither
    /// again (EvalPlanQualStart copies their values; the InitPlan's members are `recheck_skip`).
    pub(crate) fn lock_beside_drained(&self) -> bool {
        let gx = self;
        let ps = gx.pipelines();
        let lock = |k: usize| ps[k].ops.iter().any(|o| matches!(o, gravity_decompose::Op::Lock(_)));
        if crate::ops::modifytable::rowop::main_writes(gx) || !(0..ps.len()).any(|k| !gx.cte_write(k) && lock(k)) || (0..ps.len()).any(|k| gx.cte_write(k) && lock(k)) {
            return false;
        }
        // D3379 LOCKPOST: or the post CTE's (post.rs: one without RETURNING, run after the emit, bound at the turn)
        let drained = gx.after_emit();
        let inits = crate::rerun::epq::inits::init_ctes(gx).unwrap_or_default();
        let writes: PgVec<'_, usize> = (0..ps.len()).filter(|&k| ps[k].ops.iter().any(|o| matches!(o, gravity_decompose::Op::Write(w) if w.cmd != gravity_program::Cmd::Select))).collect_in(gx.mcx);
        !writes.is_empty() && writes.iter().all(|&k| (drained.contains(&k) || inits.contains(&k)) && ps[k].ops.iter().all(|o| !matches!(o, gravity_decompose::Op::Write(w) if w.cmd != gravity_program::Cmd::Select && !crate::ops::modifytable::rowop::cte_write_alone(gx, k, w))))
    }
}

/// v5 errorhold (`Px::ahead_of_rows`): pipeline `k` is graph pipeline `q` or feeds it, transitively: through the slots each
/// consumes and its breaker source, and a union's feeds and arms (as join/build_class.rs `feed` walks them).
/// D3824: also drain/spool_admit.rs `free_write` (APART) and `still_cte`.
pub(crate) fn feeds(gx: &Gx<'_, '_>, q: usize, k: usize) -> bool {
    let ps = gx.pipelines();
    let mut todo = mcx::pg_vec![in gx.mcx; q];
    let mut seen: PgVec<'_, usize> = PgVec::new_in(gx.mcx);
    while let Some(p) = todo.pop() {
        if p == k {
            return true;
        }
        if p >= ps.len() || seen.contains(&p) {
            continue;
        }
        seen.push_in(p);
        let src = if let gravity_decompose::Source::Breaker { slot } = ps[p].source { Some(slot) } else { None };
        todo.extend_in(ps[p].desc.consumes.iter().copied().chain(src).filter_map(|s| gx.life_of(p, s)).map(|l| l.0 as usize));
        todo.extend_in(gx.union_feeds(p).into_iter().chain(gx.run_arms(p)));
    }
    false
}

/// D1: the program's stamp of a structural proof's kind.
fn proof_stamp(k: gravity_decompose::effects::ProofKind) -> gravity_program::ProofStamp {
    use gravity_decompose::effects::ProofKind as K;
    use gravity_program::ProofStamp as S;
    match k {
        K::FirstFetch => S::FirstFetch,
        K::PgReadsWhole => S::PgReadsWhole,
        K::PgFailsToo => S::PgFailsToo,
        K::PgRunsFirst => S::PgRunsFirst,
    }
}
