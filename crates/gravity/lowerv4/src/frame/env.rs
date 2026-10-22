//! The environment one lowered function threads through its source, steps and end (lowerv4 plan §2.1): the context words,
//! the function's hosts, the expression tree, the exits, the per-claim state slot and frame words, and where in the chain
//! the frame is lowering (the pipeline, the step, the block path's masked rows).
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, HostFlags, HostFn, Type, Value};
use gravity_kernels::host_abi as abi;

use gravity_expr::emit::AggCell;
use gravity_expr::Compiled;

use crate::face::strategy::{Strategy, StrategySet};
use crate::face::source::Body;
use crate::face::step::Hoisted;
use crate::frame::px::ExId;
use crate::frame::stop::Exits;
use crate::hir::{Ex, Ir};
use crate::rep::Memo;

/// An aggregate row the chain's expressions read `Aggref { aggno }` from: a plain fold's finals at its drain, a keyed
/// emit's group record. `rec` is the row's base; `cells[aggno]` says where and how each aggregate's value sits there.
#[derive(Clone, Debug)]
pub struct AggRow<'e> {
    pub rec: Value,
    pub cells: mcx::PgVec<'e, AggCell>,
}

leader_tls::leader_tls! {
    thread_local! {
        static PIPE_SITES: core::cell::RefCell<Option<std::rc::Rc<[Option<u16>]>>> = const { core::cell::RefCell::new(None) };
        static SITE_RELS: core::cell::RefCell<Option<std::rc::Rc<[u32]>>> = const { core::cell::RefCell::new(None) };
        /// D4197: the program's `keys.user` handle word while its functions are made (`with_user_keys`), and whether a
        /// user key's site read it.
        static USER_KEYS: core::cell::Cell<Option<(u16, bool)>> = const { core::cell::Cell::new(None) };
    }
}

/// Runs `f` (the program's functions) with each pipeline's driving site seen by every `Env`, `rels` by `site_relation`.
pub fn with_pipe_sites<T>(sites: std::rc::Rc<[Option<u16>]>, rels: std::rc::Rc<[u32]>, f: impl FnOnce() -> T) -> T {
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            PIPE_SITES.with(|c| c.borrow_mut().take());
            SITE_RELS.with(|c| c.borrow_mut().take());
        }
    }
    PIPE_SITES.with(|c| *c.borrow_mut() = Some(sites));
    SITE_RELS.with(|c| *c.borrow_mut() = Some(rels));
    let _clear = Clear;
    f()
}

/// PGRCSERVE M0 (D2): the relation numbered site `site` reads (`with_pipe_sites`); 0 for none or outside the scope.
pub fn site_relation(site: Option<usize>) -> u32 {
    SITE_RELS.with(|c| c.borrow().as_ref().and_then(|r| site.and_then(|s| r.get(s).copied())).unwrap_or(0))
}

/// D4197: runs `f` (the program's functions) with `word` the program's `keys.user` handle word, which a user key's site
/// reads (`user_keys`), and reports whether one did: the program then declares the protocol over that word. The scope
/// before comes back after, a panic out of `f` included.
pub fn with_user_keys<T>(word: u16, f: impl FnOnce() -> T) -> (T, bool) {
    struct Back(Option<(u16, bool)>);
    impl Drop for Back {
        fn drop(&mut self) {
            USER_KEYS.with(|c| c.set(self.0));
        }
    }
    let back = Back(USER_KEYS.with(|c| c.replace(Some((word, false)))));
    let r = f();
    let read = USER_KEYS.with(|c| c.get()).is_some_and(|(_, read)| read);
    drop(back);
    (r, read)
}

/// D4197: a user key site outside a program's functions (`with_user_keys`): no run's classes to call.
pub const NO_USER_KEYS: &str = "a user key site with no keys.user word (outside the program's functions)";

/// D4197: the run's `keys.user` handle (kernels typesem/userfp.rs `RunKeys`), every user key host call's first word past
/// the context: the program's word, loaded through `ctx`'s parameter words. `None` outside `with_user_keys`; the site
/// refuses by `NO_USER_KEYS` then.
pub fn user_keys(b: &mut FunctionBuilder<'_>, ctx: Value) -> Option<Value> {
    let (word, _) = USER_KEYS.with(|c| c.get())?;
    USER_KEYS.with(|c| c.set(Some((word, true))));
    let params = b.load(Type::Ptr, ctx, gravity_abi::ctx::hdr::PARAMS as i32);
    Some(b.load(Type::I64, params, 8 * word as i32))
}

/// Every function's hosts: `done`, `stopped`, the out block's `push`, the claim's `scratch` (operator state only, as LIMIT's
/// counters: never a builtin's or fmgr result, which is fresh in the caller's context or one of its inputs).
#[derive(Clone, Copy, Debug)]
pub struct Core {
    pub done: HostFn,
    pub stopped: HostFn,
    pub push: HostFn,
    pub scratch: HostFn,
    /// A dictionary code word `part << 32 | code` of a site's code lane rendered to its image (the `Code` datum).
    pub render: HostFn,
    /// The roads that apply to the statement (`applies::applicable`, set by `program::emit`): a mechanism emits a road's
    /// form exactly where `Env::applies` holds.
    pub strategies: StrategySet,
}

pub fn declare(pb: &mut ProgramBuilder) -> Core {
    let mem = HostFlags { touches_memory: true, ..HostFlags::default() };
    Core {
        done: pb.declare_host("done", &[Type::Ptr], Some(Type::I64), gravity_kernels::contract::entry0::<abi::Done> as *const () as usize, mem),
        stopped: pb.declare_host("stopped", &[Type::Ptr], Some(Type::I64), gravity_kernels::contract::entry0::<abi::Stopped> as *const () as usize, mem),
        push: pb.declare_host("push", &[Type::Ptr], Some(Type::I64), gravity_kernels::contract::entry0::<abi::Push> as *const () as usize, mem),
        scratch: pb.declare_host("scratch", &[Type::Ptr, Type::I64], Some(Type::Ptr), gravity_kernels::contract::entry1::<abi::Scratch> as *const () as usize, mem),
        render: pb.declare_host("agg_render", &[Type::Ptr, Type::I64, Type::I64, Type::I64], Some(Type::Ptr), abi::agg_render as *const () as usize, mem),
        strategies: StrategySet::EMPTY,
    }
}

/// A nest's inner row as nest.loop joins it (`Env::redo_nests`): its cells, their values and its column map; producerchain
/// C: and whether it is a LEFT nest's NULL-extended row (no inner row matched: EvalPlanQual's test tuple is none).
pub type NestRow<'e> = (mcx::PgVec<'e, crate::cell::Cell>, mcx::PgVec<'e, (Value, Option<Value>)>, mcx::PgVec<'e, Option<u16>>, Filled);

/// Whether a held inner row (`NestRow`) is a LEFT join's NULL-extended row. D3818: a LEFT nest or probe that lowers the
/// steps after it once for its matched and NULL-extended rows (`Gx::fills_share`) knows it only at run time: `At` is the
/// shared block's miss tag, true for the NULL-extended row (epq_ir.rs `run_on` branches on it).
#[derive(Clone, Copy)]
pub enum Filled {
    No,
    Yes,
    At(Value),
}

pub struct Env<'e, 'p> {
    /// The lowering's planning context (`Px::mcx`): emission scratch.
    pub mcx: mcx::Mcx<'e>,
    pub ctx: Value,
    pub zero: Value,
    pub one: Value,
    pub params: Value,
    pub params_null: Value,
    pub core: Core,
    pub hir: Ir,
    pub memo: Memo<'e>,
    pub x: Exits,
    pub l_raise: Block,
    pub l_raise_row: Option<Block>,
    pub l_cancel: Block,
    /// The end's per-claim state slot (`EndMech::state_bytes`).
    pub state: Option<Value>,
    /// The program's frame words (the words site's key cells): per-open words, union flags, resume words.
    pub words: Option<Value>,
    /// Loop headers whose every back edge is a bounded repeat inside a checked loop (a row loop in its window; D1028: never one a retry shares).
    pub bounded: mcx::PgVec<'e, Block>,
    pub site: Option<usize>,
    /// The pipeline being lowered, its plan node, and the step whose `lower` runs (`None`: the source or the end).
    pub k: usize,
    pub node: i32,
    pub step: Option<usize>,
    /// The row runs inside a 64-row block whose first step's mask already passed it: that step skips its per-row test.
    pub masked: bool,
    /// The pipeline's expanding steps (step, offset word): a stop inside a row leaves an offset set, and `stop::held`
    /// reads a row whose later expanding step has one as re-entered.
    pub expands: mcx::PgVec<'e, (usize, i32)>,
    /// The pipeline's steps that may stop a call before taking its row (step, offset word: `StepMech::wait_offset`).
    pub waits: mcx::PgVec<'e, (usize, i32)>,
    /// D2056: the outer checks' stop, set by the step whose prologue runs first (`StepMech::outer`): a later prologue's
    /// one-time evaluation (a gate's quals) runs only while it is false (PG never pulls below a stopped node).
    pub stopped: Option<Value>,
    /// M8 B1b: a nest inner's own position word (nest/inner.rs), which a siteless source keeps its resume position in
    /// (source/result.rs `resume_pos`) instead of the pipeline's resume word; `None` outside an inner.
    pub pos: Option<i32>,
    /// The expanding step whose single-row body is being lowered (`StepMech::single_guard`, V4R-27): it hands on at most
    /// one row per row, no offset word, the cursor past the row.
    pub single: Option<usize>,
    /// D3148: the body `EndMech::every_guard` versions, where the end's admission admits every row (no test lowered).
    pub every: bool,
    /// The JIT map's tags (frame/jit.rs): the body form being lowered and the pipeline end's mechanism.
    pub jit_body: &'static str,
    pub jit_end: &'static str,
    /// D3194: the body `StepMech::rowfilter_guard` versions, where step j tests its build's Bloom words (join.rowfilter).
    pub rowfilter: Option<usize>,
    /// D3194: the block path's walked rows passed that step's block test (`StepMech::block_rowfilter`): no per-row test.
    pub prefiltered: bool,
    /// D3168: the body `StepMech::batch_guard` versions, where step j's rows run its stages over a block.
    pub batch: Option<usize>,
    /// D3168: the row being walked is row `t` of a block whose stages ran (`StepMech::block_batch`).
    pub batch_row: Option<BatchRow>,
    /// D3168 S0: a pipeline's plain function, beside its staged one (frame.rs `pipeline_fns_in`): no probe stages here.
    pub unstaged: bool,
    /// D3168 S0: a probe of this function ran its rows in stages over a block (probe_stage.rs `block`).
    pub staged: bool,
    /// The block span (H01) while its masks are built: this step's place in it and its length ((0, 1) elsewhere).
    pub span: (usize, usize),
    /// The aggregate row in scope, set by a drain or emit source in its `row()`; step expressions read `Aggref` from it.
    pub aggs: Option<AggRow<'e>>,
    /// A ProjectSet's current set values (M5 L0-c (1)), set by its Srf step: `cells[i]` is function `i` of the `SrfDesc`.
    pub srfs: Option<AggRow<'e>>,
    pub subplans: mcx::PgVec<'e, (i32, i32, i32, usize)>, // M8 nest.subplan: the SubPlans the row reads, (plan_id, value word, flags word, lent: lazy)
    /// D3374: the lazy SubPlan evaluated next holds its second row (an interpreter island's read: step/expr_ext_lazy.rs
    /// `emit_subplan_held`); its evaluation takes the flag, so nothing it lowers inherits it.
    pub hold_rows: bool,
    /// D652-S1 (modifytable): each eager SubPlan step's body for a write's EvalPlanQual redo in the IR, (plan_id, lent entry
    /// with no row): the redo evaluates it again over the newest version (subplan site.rs `redo`).
    pub redo_subs: mcx::PgVec<'e, (i32, usize)>,
    /// D3506 SETLOCK (rerun/epq/set_lock.rs): a lock's EvalPlanQual redo is lowering: a hashed probe of a set whose
    /// plan locks reads the recheck's own set (subplan hashed.rs `Probe::twin`).
    pub epq_twin: bool,
    /// D3506 EXISTS SORT (rerun/epq/sub_lock.rs `SortClaim`): the SubPlan steps of a claim's Sort producer made again
    /// for its recheck, by plan id, each a `&dyn StepMech` lent while the claim's lock lowers (epq_ir.rs `Redo::Sub`).
    pub redo_own: mcx::PgVec<'e, (i32, usize)>,
    /// D1933 (c) (admit): each nest's inner row handed on, by the nest's node (the row's cells, values and column map),
    /// for a write's EvalPlanQual redo in the IR over the newest version joined to it again (epq_ir.rs `Redo::Nest`).
    pub redo_nests: mcx::PgVec<'e, (i32, NestRow<'e>)>,
    /// modifytable D2739 NEST-INNER (subq-0323): each INNER nest's outer row while it hands a match on, by the nest's node, for
    /// a write whose target is that nest's inner (epq_ir.rs `Redo::InnerOf`: the newest version joined to it again).
    pub redo_outers: mcx::PgVec<'e, (i32, NestRow<'e>)>,
    /// producerchain D: each INNER hash probe's key image of the row it hands a record on for, by the probe's node (the
    /// matched record's own image; its payload row is on `redo_nests` under that node), for a write's EvalPlanQual redo in
    /// the IR (epq_ir.rs `Redo::Probe`: the newest version's image against it).
    pub redo_probes: mcx::PgVec<'e, (i32, mcx::PgVec<'e, Value>)>,
    /// The row's locator (H28), set by the frame after the source's `row()` when the end is a row op.
    pub loc: Option<crate::face::source::Locator>,
    /// The joined row's INNER_VAR columns (attno `a` is cell `inner[a - 1]` of the row handed on), set by a probe for the
    /// steps after it and cleared by the projection that collapses the row (`row::inner_col`).
    pub inner: Option<mcx::PgVec<'e, Option<u16>>>,
    /// The I64 multiplicity (>= 1) of the row handed on (join.mult, H20): set by a probe, only when the end's `takes_mult()`
    /// holds and no later step counts rows, restored after its `next()`; a second probe multiplies. A collapsing projection
    /// keeps it (the collapsed row still stands for that many rows); only the end consumes it. `None` means 1.
    pub mult: Option<Value>,
    /// The block's packed views by attno (`SourceMech::packed`), set per block in the packed body; empty elsewhere.
    pub packed: mcx::PgVec<'e, (i16, crate::face::source::Packed)>,
    /// The window's bound per-code tables of the site, as the kernels' `agg_bind` lays them out: table `t` at byte `24 t`
    /// (its data, its fault words or the zero page, its length form `BIND_FORM_*`), flags at word `3 n`. The source binds
    /// them after publishing each window's lanes (`step_percode::bind`); the code hash arm and per-code steps read them.
    pub tables: Option<Value>,
    /// The window's code lanes (lane, empty code, part word, site lane ordinal), set by the source at `next_window`.
    pub codes: mcx::PgVec<'e, gravity_expr::emit::CodeRef>,
    /// The window being walked (after `next_window`), for an end reading its facts per row (the runs word).
    pub win: Option<crate::frame::frame::Win>,
    /// A1 (D3224): the window's packed numeric lane kind tests (`SourceMech::num_kinds`), set by the frame around its row
    /// loops only (their values dominate nothing else); `None` elsewhere, where `element` reads the kind byte per row.
    pub num_kinds: Option<std::rc::Rc<[(usize, u16, Value)]>>,
    /// A1 (D3224): the fold steps a group record of a keyed table (ops/agg/hashagg.rs `row_in`): such a record is opened
    /// over zeroed space (abi `Rec::open`) or initialised by its class (`open_rec`), and a step marks a slot valid in the
    /// store that first writes it, so an invalid sum / avg slot holds zero words: the lean adds read them untested.
    pub rec_zeroed: bool,
    /// D3224 A2: inside hashagg `row_in`, whether the row's record is a code front's (dict/codefront.rs `upsert_at`, a
    /// Bool: true in the front's arm). A front record holds one window's rows alone (flushed into the table and zeroed at
    /// every window end), so a sum there is bounded by the window's rows (`SourceMech::num_kinds` sends a window past
    /// `WINDOW_ROWS` to the generic step).
    pub rec_window: Option<Value>,
    /// True while hashagg's lazy probe's cold block upserts (ops/agg/hashagg.rs `row_lazy`): the probe already compared
    /// the row's code with code.front_part's run cache and missed, so `upsert_at` refills the cache without reading it.
    pub front_probed: bool,
    /// Pipeline k's driving site, by graph index (`with_pipe_sites`): a record cell's dictionaries are its producer's.
    pub pipe_sites: Option<std::rc::Rc<[Option<u16>]>>,
    /// The facts the window's fast body runs under (H06); every field `false` outside it.
    pub body: Body,
    /// V4R-371: the general body's entry `l_row(j, carry…)` while the fast body is lowered (`None` elsewhere): a source row
    /// the window's facts do not cover (a heap row whose header word fails) re-enters the window there, at its own row.
    pub escape: Option<Block>,
    /// The expressions hoisted out of the row loop (H35), each computed once at its scope.
    pub hoisted: mcx::PgVec<'e, Hoisted<'e>>,
    /// The window's early exits that store the end's carried accumulators first (frame.carry), when the end carries.
    pub carry_exits: Option<crate::face::step::CarryExits>,
    /// The parameter word of the statement's aggregate arena handle (`agg.arena`, V4R-269), set by a fold end around its
    /// hosted classes' calls (face/aggcall.rs `step` / `finish` read it; V4R-277).
    pub agg_arena: Option<u16>,
    /// D2580: the count-reached exit's record of which step met it, in a pipeline with a step that holds rows back (frame.rs
    /// `pipeline_fn`); `None` elsewhere, where the count reached is `done` as it always was.
    pub full_from: Option<FullFrom>,
    /// join.payload_fp (ops/hashjoin/strategies/payload_fp.rs): a hash probe's record code words and the Fp128 words their
    /// records carry (`[lo, hi]`, defined beside the word, so they dominate its every read); kept for the function, as a
    /// value is defined once (never cleared with the memo).
    pub fps: crate::lx::FpLinks,
    exs: &'e [Ex<'p>],
}

/// D2580: `Env::full`'s record of the step whose count was reached (a LIMIT met, a merge join whose inner ran out): a stack
/// word `at` the count-reached exit reads, so the steps after that one which hold rows back (`StepMech::flushes`, the last
/// at `last`) hand them on before `done`, as their input's end does (PG: the node above sees its child's end). Only the
/// pipeline's own steps (`k`, `node`) record one; anything else records `FULL_NONE`.
#[derive(Clone, Copy, Debug)]
pub struct FullFrom {
    pub at: Value,
    pub k: usize,
    pub node: i32,
    pub last: usize,
}

/// `FullFrom`'s word when no step after the one met holds rows back (the end, a step past the last holding one).
pub const FULL_NONE: i64 = i64::MAX;

/// D3168: the rows of a block a probe's stages run over: `nw` mask words at `masks` (`Ptr`; bit t of word w is row
/// `j + 64 w + t`), and whether every one of them walks after (`all`: a condition-cache fill sees each row the span passes).
#[derive(Clone, Copy)]
pub struct BatchRows {
    pub masks: Value,
    pub nw: usize,
    pub all: bool,
}

/// D3168: a block's stages as the frame walks its rows (`StepMech::block_batch`): the rows that walk (`nw` mask words at
/// `walk`, `Ptr`), the probe (its offset word) and the block's first records (`Ptr`, one word a row, 0: none).
#[derive(Clone, Copy)]
pub struct Batched {
    pub walk: Value,
    pub offset: i32,
    pub firsts: Value,
}

/// D3168: a staged block's row as its probe reads it: the block's stages and the row's place in the block (`I64`).
#[derive(Clone, Copy)]
pub struct BatchRow {
    pub offset: i32,
    pub firsts: Value,
    pub t: Value,
}

impl<'e, 'p> Env<'e, 'p> {
    /// `f` emitting into a branch of the row: a datum read there does not dominate the join (nor a sibling branch), so
    /// the row's memo is set aside around it and restored after (fold/filter.rs `step_if`'s rule).
    pub(crate) fn aside<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let memo = self.memo.set_aside();
        let r = f(self);
        self.memo.restore(memo);
        r
    }

    pub fn new(b: &mut FunctionBuilder<'_>, core: Core, hir: Ir, exs: &'e [Ex<'p>], x: Exits, mcx: mcx::Mcx<'e>) -> Env<'e, 'p> {
        Self::new_in(b, core, hir, exs, x, mcx)
    }

    pub fn new_in(b: &mut FunctionBuilder<'_>, core: Core, hir: Ir, exs: &'e [Ex<'p>], x: Exits, mcx: mcx::Mcx<'e>) -> Env<'e, 'p> {
        let ctx = b.params()[0];
        let zero = b.iconst(Type::I64, 0);
        let one = b.iconst(Type::I64, 1);
        let params = b.load(Type::Ptr, ctx, gravity_abi::ctx::hdr::PARAMS as i32);
        let params_null = b.load(Type::Ptr, ctx, gravity_abi::ctx::hdr::PARAMS_NULL as i32);
        Env {
            mcx,
            ctx,
            zero,
            one,
            params,
            params_null,
            core,
            hir,
            memo: Memo::root(mcx),
            x,
            l_raise: x.l_raise,
            l_raise_row: None,
            l_cancel: x.l_cancel,
            state: None,
            words: None,
            bounded: mcx::PgVec::new_in(mcx),
            site: None,
            k: 0,
            node: 0,
            step: None,
            masked: false,
            expands: mcx::PgVec::new_in(mcx),
            waits: mcx::PgVec::new_in(mcx),
            stopped: None,
            pos: None,
            single: None,
            every: false,
            jit_body: "fn",
            jit_end: "",
            rowfilter: None,
            prefiltered: false,
            batch: None,
            batch_row: None,
            unstaged: false,
            staged: false,
            span: (0, 1),
            aggs: None,
            srfs: None,
            subplans: mcx::PgVec::new_in(mcx),
            hold_rows: false,
            redo_subs: mcx::PgVec::new_in(mcx),
            epq_twin: false,
            redo_own: mcx::PgVec::new_in(mcx),
            redo_nests: mcx::PgVec::new_in(mcx),
            redo_outers: mcx::PgVec::new_in(mcx),
            redo_probes: mcx::PgVec::new_in(mcx),
            loc: None,
            inner: None,
            mult: None,
            packed: mcx::PgVec::new_in(mcx),
            tables: None,
            codes: mcx::PgVec::new_in(mcx),
            win: None,
            num_kinds: None,
            rec_zeroed: false,
            rec_window: None,
            front_probed: false,
            pipe_sites: PIPE_SITES.with(|c| c.borrow().clone()),
            body: Body::default(),
            escape: None,
            hoisted: mcx::PgVec::new_in(mcx),
            carry_exits: None,
            agg_arena: None,
            full_from: None,
            fps: crate::lx::FpLinks::NONE,
            exs,
        }
    }

    /// join.payload_fp: whether record code word `v` carries its Fp128 words (`Env::fps`).
    pub fn linked(&self, v: Value) -> bool {
        self.fps.get(v).is_some()
    }

    /// Road `r` applies to the statement and is on (built, not dropped, not switched off).
    pub fn applies(&self, r: Strategy) -> bool {
        self.core.strategies.has(r) && crate::face::source::on(crate::admit::refusal::Variant::Strategy(r))
    }

    /// `e`'s hoisted words, when road `hoist` computed them outside the row.
    pub fn hoisted(&self, e: ExId) -> Option<&Hoisted> {
        self.hoisted.iter().find(|h| h.ex == e)
    }

    pub fn ex(&self, id: ExId) -> &'e Compiled<'p> {
        &self.exs[id.0 as usize].c
    }

    pub fn placed(&self, id: ExId) -> &'e Ex<'p> {
        &self.exs[id.0 as usize]
    }

    pub fn lx<'a, 'f>(&mut self, b: &'a mut FunctionBuilder<'f>, node: i32) -> crate::rep::Lx<'a, 'f, 'e>
    where
        'e: 'a,
    {
        let at = crate::rep::Frame { ctx: self.ctx, site: self.site, hosts: crate::rep::RepHosts { render: self.core.render }, tables: self.tables };
        crate::rep::Lx { b, mcx: self.mcx, node, memo: self.memo.lend(), at: Some(at), space: None, pipe_sites: self.pipe_sites.clone(), raise: Some(self.l_raise), strategies: self.core.strategies, key_word: crate::rep::KeyWord::Sort, num_kinds: self.num_kinds.clone(), fps: self.fps }
    }

    /// The site a cell's lanes and dictionaries belong to (`rep::Lx::site_of`).
    pub fn site_of(&self, c: &crate::cell::Cell) -> Option<usize> {
        match (c.life.home, &self.pipe_sites) {
            (crate::cell::CellHome::Record(l), Some(p)) => p.get(l.0 as usize).copied().flatten().map(usize::from),
            (crate::cell::CellHome::Record(_), None) => None,
            _ => self.site,
        }
    }

    /// Frame word `off` (a byte offset from `stop::resume_word` or `Px::frame_word`) loaded as an `I64`.
    pub fn load_word(&self, b: &mut FunctionBuilder<'_>, off: i32) -> Value {
        match self.words {
            Some(w) => b.load(Type::I64, w, off),
            None => crate::refuse!(self.node, crate::admit::refusal::Reason::Bug { payload: "a frame word without the words site" }),
        }
    }

    pub fn store_word(&self, b: &mut FunctionBuilder<'_>, off: i32, v: Value) {
        match self.words {
            Some(w) => b.store(w, off, v),
            None => crate::refuse!(self.node, crate::admit::refusal::Reason::Bug { payload: "a frame word without the words site" }),
        }
    }

    /// Leaves the row with `status`, the cursor at `cur` and the rows counted in `c.out` (a stop inside a step). When the end
    /// carries accumulators in registers (frame.carry), `c.accs` are stored first through the window's `carry_exits`.
    pub fn stop(&self, b: &mut FunctionBuilder<'_>, c: &crate::frame::row::Carry, cur: Value, status: Value) {
        match self.carry_exits {
            Some(e) => b.br(e.l_stop, &mcx::concat_in(&[&[c.out, cur, status][..], &c.accs[..]], self.mcx)),
            None => b.br(self.x.l_stop_in, &[c.out, cur, status]),
        }
    }

    /// Leaves the row with the count reached (a LIMIT met), the accumulators stored first as `stop` does. D2580: the step
    /// met is recorded first (`FullFrom`), so the steps after it that hold rows back hand them on.
    pub fn full(&self, b: &mut FunctionBuilder<'_>, c: &crate::frame::row::Carry, cur: Value) {
        if let Some(f) = self.full_from {
            let from = match self.step {
                Some(j) if j < f.last && (self.k, self.node) == (f.k, f.node) => j as i64,
                _ => FULL_NONE,
            };
            let v = b.iconst(Type::I64, from);
            b.store(f.at, 0, v);
        }
        match self.carry_exits {
            Some(e) => b.br(e.l_full, &mcx::concat_in(&[&[c.out, cur][..], &c.accs[..]], self.mcx)),
            None => b.br(self.x.l_full, &[c.out, cur]),
        }
    }
}
