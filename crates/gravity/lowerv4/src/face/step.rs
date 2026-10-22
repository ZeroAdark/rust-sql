//! The step face (plan §3.1; H01 H17 H29 H32 H35): one op over a row, which hands the row on through `Next`. The v3 trait
//! carries over (lowerv3-final emit/frame.rs), with these added:
//! - the v3-r1-a4 block form (H01): `block_open` / `block` over the window's 64-row blocks, first step only;
//! - the expanding step (H32) and its offset word (H17 c);
//! - the rescan protocol (H29);
//! - loop-invariant hoisting (H35).
//!
//! Per-open words (H17 a), the EPQ resume word (H17 b, end.rs) and cancel (H17 d) belong to the frame (frame/open.rs,
//! stop.rs, cancel.rs). Step kinds (face Step, and the op kinds of the other faces) are made by `frame::StepMaker`. Closed at
//! M1's exit.
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_expr::Compiled;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, Type, Value};
use mcx::GrowIn;
use types_core::Oid;
use types_nodes::list::NodeList;
use types_nodes::Node;

use crate::face::end::EndMech;
use crate::frame::env::Env;
use crate::frame::frame::{Mask, Next, WordWin};
use crate::frame::open::OpenWords;
use crate::frame::px::{ExId, Px};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::graph::Gx;

crate::hook_variants! {
    /// The 64-row block forms (H01).
    "H01" pub enum BlockForm {
        /// A 64-row scalar word mask whose rows are walked by ctz.
        Word = Strategy(MASK_WORD),
        /// (a) the arm64 `VecCmp` chunk form with gathered sign bits (the scalar word elsewhere).
        Vec = Strategy(MASK_VEC),
        /// (b) an all-ones block runs straight when the steps are row-order-free.
        Run = Strategy(MASK_RUN),
        /// (c) a block consumer folding the whole mask (fold.rs `BlockFold`).
        Fold = Strategy(MASK_FOLD),
        /// (d) an operand that is a runtime constant (consts.rs `RtConst`). model.py gives (d) no road of its own; it is built
        /// with the runtime-constant verdict road.
        RtOperand = Strategy(VERDICT_RTCONST),
        /// (e) the key column decodes first and the other lanes only for survivors (source.rs `DecodeOrder`).
        LateDecode = Strategy(DECODE_LATE),
        /// (f) a source without a word window (a heap scan's pages): the first filter's mask made row by row, its passing
        /// rows walked by ctz (frame/mask_rows.rs, lane brfix).
        Rows = Strategy(MASK_ROWS),
    }
}

/// A block form's mechanism (registered as `BlockImpl` for the mask roads): the rows of `[base, base + 64)` that pass
/// `qual` within `m`.
pub trait BlockMech: Sync {
    fn mask(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, w: &WordWin, base: Value, qual: ExId, m: Mask) -> Mask;
}

pub struct BlockImpl(pub &'static dyn BlockMech);

/// Row `i` of a block through the chain with the carry before it: the carry after.
pub type RowWalk<'a, 'e> = &'a mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env<'e, '_>, Value, Carry<'e>) -> Carry<'e>;

/// Road `mask.run`'s mechanism (`MaskRunImpl`, H01 b): block `[base, base + 64)`'s passing rows `m` walked through `row` in
/// ascending order with the carry threaded, then `l_next(next, carry…)`. An all-ones block runs as a straight count, in the
/// same row order as the ctz walk.
pub trait MaskRunMech: Sync {
    fn walk<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, base: Value, m: Mask, carry: &Carry, next: (Block, Value), row: RowWalk<'_, 'e>);
}

pub struct MaskRunImpl(pub &'static dyn MaskRunMech);

/// A window's exits for an end that carries accumulators in registers (`CarryMech::exits`): `l_stop(out, cursor, status,
/// accs…)` and `l_full(out, cursor, accs…)` store the accumulators (`EndMech::window_end`) and leave by `l_stop_in` /
/// `l_full`.
#[derive(Clone, Copy, Debug)]
pub struct CarryExits {
    pub l_stop: Block,
    pub l_full: Block,
}

/// Road `frame.carry`'s mechanism (`CarryImpl`, H17): an end's accumulators ride the row loop's carry for a window, and an
/// exit that leaves the window early (a stop, the count reached) stores them first, so a resumed call reloads them and a
/// finished one folds them. The frame sets `Env::carry_exits` per window; a step leaving mid-row goes through them with its
/// row's carry (`Env::stop` / `Env::full`). `exits` makes the blocks before the window's bodies lower; `seal` writes their
/// bodies after them, where the window's own end is written (D326: an end's `window_end` reads what its `lower` decided,
/// the hash build's record shape).
pub trait CarryMech: Sync {
    fn exits(&self, b: &mut FunctionBuilder<'_>, accs: &[Type], mcx: mcx::Mcx<'_>) -> CarryExits;
    fn seal(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, end: &dyn EndMech, e: CarryExits, x: &Exits);
}

pub struct CarryImpl(pub &'static dyn CarryMech);

crate::strategies! {
    /// Running frames O(1) per row, state in registers. No mechanism yet: its readers take the straight form.
    WIN_FRAMECLASS = 110, "win.frameclass", Window;
    /// Parallel prefix for running aggregates / row numbers: per-morsel prefixes, then a second parallel pass. No
    /// mechanism yet: its readers take the straight form.
    WIN_PREFIX = 112, "win.prefix", Window;
}

crate::hook_variants! {
    /// The frame's loop roads (H17, H35).
    "H17 H35" pub enum LoopRoad {
        /// H35: loop-invariant words hoisted out of inner, generation and chain loops (TPDE has no LICM).
        Hoist = Strategy(LOOP_HOIST),
        /// H17: fold accumulators and the open run carried in registers per window.
        Carry = Strategy(FRAME_CARRY),
        /// H17: running window frames cost O(1) per row, with their state in registers.
        FrameClass = Strategy(WIN_FRAMECLASS),
        /// H17: a parallel prefix for running aggregates and row numbers: per-morsel prefixes, then a second parallel pass.
        Prefix = Strategy(WIN_PREFIX),
    }
}

crate::hook_variants! {
    /// Expression roads a step's compile takes.
    "- H14 H35" pub enum ExprRoad {
        /// date_trunc / extract as integer arithmetic (Umulh), strict key words.
        Fusion = Strategy(EXPR_FUSION),
        /// The `->>` / `->` shims read the jsonb image in place, under the fmgr result contract.
        JsonbInplace = Strategy(JSONB_INPLACE),
        /// textkeys step 1: text.eq's cheap per-row form (the constant decoded once per call, the column's header once per
        /// row, bpchar's trim open-coded).
        TextLean = Strategy(TEXT_LEAN),
        /// textkeys step 2: expr.saop's byte set probed in line over its elements' images, its operand a column or a view.
        TextSet = Strategy(TEXT_SET),
        /// D4794: the byte set over bpchar lists too, elements and operand without their trailing blanks.
        TextSetBpchar = Strategy(TEXT_SET_BPCHAR),
        /// D4822: a hosted substring's result by bytes over ASCII text.
        TextSubstrAscii = Strategy(TEXT_SUBSTR_ASCII),
    }
}

pub use crate::face::ext::{expr_exts, Claim, ExprExtImpl, ExprExtMech, ExtPlan, SubEmit, EXT_AT};
pub(crate) use crate::face::ext::exts_in;

/// Kind `expr.volatile`'s mechanism (`VolatileImpl`): whether expression `ex`'s volatile calls are held, and in how many
/// words, on a row a stopped call re-enters.
pub trait VolatileMech: Sync {
    fn plan(&self, px: &mut Px<'_, '_>, at: (usize, usize), c: &Compiled<'_>, ex: ExId) -> Held;
}

pub struct VolatileImpl(pub &'static dyn VolatileMech);

/// `words > 0`: the frame holds the expression's result (value, NULL flag) across a stop inside its row (frame/stop.rs
/// `held`) in `max(words, 2)` frame words (`Ex::held`); the words past the first two are the extension's own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Held {
    pub words: u8,
}

crate::hook_variants! {
    /// How a step's range re-runs (H29): per outer row or per generation, with its parameter words written by their owner
    /// (nest.rs `ParamOwner`).
    "H29 H19" pub enum Rescan {
        /// A re-open. The frame writes only the declared per-open words (no grant draw, no section open, no pool wake). Its
        /// rerun sets come from chgParam.
        Reopen = Kind(NestRerun),
        /// A rescan-invariant inner is spooled once and re-read by position.
        SelfSpool = Strategy(NEST_SELFSPOOL),
        /// An empty left input short-circuits the right (SetOp).
        SkipEmpty = Strategy(SETOP_EMPTYLEFT),
    }
}

crate::hook_variants! {
    /// What an expanding step expands (H32): one input row yields k >= 0 rows.
    "H32" pub enum ExpandKind {
        /// A set-returning function's rows (FunctionScan, ProjectSet).
        Srf = Kind(SrfFunction),
        /// A probe's match chain over duplicate build keys.
        Chain = Kind(JoinInner),
        /// A nest's inner rows per outer row.
        Nest = Kind(NestLoop),
        /// A LIMIT stop reaches the set loop.
        LimitCut = Strategy(SRF_LIMITCUT),
    }
}

/// An expanding step (H32).
/// - `offset` is the byte offset of its offset word (`stop::ResumeWord::StepOffset`), which it declares as a per-open word
///   with init 0. The word holds the row's next output.
/// - A stop inside the row stores the offset and leaves by `env.x.l_stop_in` with the cursor at the row (`RowVals::at`). The
///   resumed call continues from the offset.
/// - Every stop's cursor is one of two (D357): past the row (`RowVals::next`: the row is through, as the sink leaves after
///   storing it) or at the row (`RowVals::at`: re-entered, not through). A step above that keeps per-row state (a LIMIT's
///   count) stores it at a stop only in the first case (frame.carry's `through`, which checks the cursor is one of them).
/// - `held` counts the frame words that hold the row's volatile arguments, so a resume never re-evaluates them.
#[derive(Clone, Copy, Debug)]
pub struct Expand {
    pub kind: ExpandKind,
    pub offset: i32,
    pub held: u8,
}

/// Where a hoisted expression is computed (H35), instead of once per row: at each window's start, the one scope a step
/// adds (step/filter.rs). A scope at the call's entry, before any window, would compute a value PostgreSQL computes only
/// for a row that reaches the expression: it holds a `Licence` if it is ever added (lint/rules/rh_ahead.py).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoistAt {
    Window,
}

/// The loop-invariant expressions a step reads (H35). The frame computes each once at its scope (road `hoist`), and the step
/// reads that value instead of re-emitting it in the inner loop.
#[derive(Debug)]
pub struct Hoist<'m> {
    pub words: mcx::PgVec<'m, (HoistAt, ExId)>,
}

impl<'m> Hoist<'m> {
    pub fn new_in(mcx: mcx::Mcx<'m>) -> Hoist<'m> {
        Hoist { words: mcx::PgVec::new_in(mcx) }
    }

    pub fn add(&mut self, at: HoistAt, e: ExId) {
        self.words.push_in((at, e));
    }
}

/// One hoisted expression's words (`Env::hoisted`): its constants as the row would load them (placed words from the
/// bind image, folded literals as immediates), computed once at the expression's scope.
#[derive(Clone, Debug)]
pub struct Hoisted<'e> {
    pub ex: ExId,
    pub words: mcx::PgVec<'e, Value>,
    pub nulls: mcx::PgVec<'e, Value>,
    /// D3033: its late operands (`Ex::late`) computed at the scope over these words, `[value, valid, err]` by node; the
    /// row reads them (`RowRefs::late`), a held raise only where the row reaches the operand.
    pub late: mcx::PgVec<'e, (gravity_expr::NodeId, [Value; 3])>,
}

/// Road `hoist`'s mechanism (`HoistImpl`): emits `h`'s entries of scope `at` into `Env::hoisted`. Only what cannot raise
/// is hoisted (a load of a bind word, an immediate), so a hoisted value never raises before a row reaches it (PG's
/// laziness): anything that could raise stays in the row.
pub trait HoistMech: Sync {
    fn emit(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, at: HoistAt, h: &Hoist);
}

pub struct HoistImpl(pub &'static dyn HoistMech);

pub trait StepMech {
    /// The JIT map's name of this mechanism (frame/jit.rs): its type's.
    fn mech_name(&self) -> &'static str {
        core::any::type_name::<Self>()
    }
    /// At make time, once the pipeline's end is made (resolve): a step whose form depends on the end reads it here
    /// (join.mult: the probe takes its multiplicity form only when `end.takes_mult()`).
    fn end_is(&mut self, _end: &dyn EndMech) {}
    /// H17 a: every word the step reads across rows or calls (LIMIT counters, walk state, an expanding step's offset).
    fn open_words(&self, _w: &mut OpenWords) {}
    fn prologue(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _x: &Exits) {}
    /// D2056: the step's prologue makes the checks of nodes above every other step of the pipeline (a Limit's zero count,
    /// a one-time gate: result/gate.rs's feeder), so the frame runs it first, as PG makes them before it pulls.
    fn outer(&self) -> bool {
        false
    }
    /// D2877: the prologue makes a check PG makes at its node's first call whatever the node's outer holds (a one-time
    /// gate's quals, a feeder's checks): the runner then calls the kernel once even when no claim reaches the pipeline
    /// (`Pipeline::first_call`).
    fn first_call(&self) -> bool {
        false
    }
    /// D2228: at a nest inner's re-open, before the gates of the Results below the step: the reads PG makes before it pulls
    /// (a Limit's computed bounds, raising there); `Some(stop)` when the step then pulls nothing (a zero count).
    fn reopen(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Option<Value> {
        None
    }
    /// At every window's start.
    fn window(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _x: &Exits) {}
    /// H01 a / b / e: the first step's block form over the window's lanes, as the guard under which whole blocks run by mask.
    fn block_open(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _w: &WordWin) -> Option<Mask> {
        None
    }
    /// The rows of block `[base, base + 64)` that pass, within `m`. The chain then runs with `env.masked` set (a masked row
    /// skips this step's per-row test).
    fn block(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _w: &WordWin, _base: Value, m: Mask) -> Mask {
        m
    }
    /// H01 c (count form): how many rows of the whole block `[base, base + 64)` pass (`I64`), counted without the mask;
    /// asked of the first step only when the end counts (`EndMech::counts`). `None`: the step has no count form.
    fn block_count(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _w: &WordWin, _base: Value) -> Option<Value> {
        None
    }
    /// H01 (the block span): the window lanes (bit `a - 1`, `a <= 64`) this step's block form reads from their buffers that
    /// the first step's block form does not (a lane step 0 reads from its buffer is decoded before the block's masks; raw
    /// deltas are no buffer read). Asked of a spanned step past the first: one that reads a lane the source decodes late
    /// (`DecodeOrder::Late`) runs after the frame's `fill_late` of the block's rows so far. `u64::MAX`: not known.
    fn block_reads(&self, _env: &Env, _w: &WordWin) -> u64 {
        u64::MAX
    }
    /// Road `mask.rows` (frame/mask_rows.rs): the first step's counters (its frame words, read by its `prologue` where the
    /// road applies to it), `None` when the step has no row mask.
    fn row_stats(&self) -> Option<crate::frame::mask_rows::Stats> {
        None
    }
    /// Road `mask.rows`: a leading step past the first whose test joins the first's row mask (`row_pass` gives a value).
    fn row_maskable(&self) -> bool {
        false
    }
    /// Road `mask.rows`: the step's test of `row` as one value, every qual evaluated and ANDed with no branch on a qual's
    /// outcome: `(pass, err)`, `err` the raise word the row path would raise (not raised here: the frame walks that row
    /// again on the row path). Asked of the first step when its `row_stats` is `Some`, and of each `row_maskable` step after.
    fn row_pass(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _row: &RowVals) -> Option<(Value, Value)> {
        None
    }
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e>;
    fn epilogue(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _x: &Exits) {}
    /// H32 and its offset word (H17 c).
    fn expand(&self) -> Option<Expand> {
        None
    }
    /// EXPLAIN ANALYZE: the offset word a row finds tagged when it re-enters this step after the step stopped the call
    /// before taking it (a lazy build still pending, D824): the frame counts no boundary up to this step for it again,
    /// in every body (a non-expanding one included). `None`: the step never stops so.
    fn wait_offset(&self) -> Option<i32> {
        None
    }
    /// D3289: a step that is not expanding but may stop the call with the cursor AT its row, to take that row again on the
    /// resumed call (a row lock's recheck behind a receiver's stop at its push: lockrows marks.rs `recheck_exit`), and its
    /// word, set while the row waits to be handed on again. The frame lists it with the expanding steps' offsets
    /// (`Env::expands`), so every expanding step before it hands the same row on again, as after a later expanding step
    /// that stopped inside (D688). `None`: the step never stops so.
    fn reentry(&self) -> Option<i32> {
        None
    }
    /// V4R-27: after `window`, the `Bool` under which this expanding step hands on at most one row per row all window; the
    /// frame then lowers a second body with `Env::single` naming it, where it is a non-expanding step. `None`: none.
    fn single_guard(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Option<Value> {
        None
    }
    /// D3194 (join.rowfilter): after `window`, the `Bool` under which this probe tests its build's Bloom words this window;
    /// the frame then lowers a second body with `Env::rowfilter` naming it (the body without the test runs otherwise).
    /// `None`: the step never tests one.
    fn rowfilter_guard(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Option<Value> {
        None
    }
    /// D3194: in that body's block path, the block `[j, j + 64)` rows of `mask` that pass the filter, tested from `row` (the
    /// source's row `j`) before the late lanes decode; `fill` decodes the key columns' late lanes first (false: it cannot,
    /// so no test). `None`: no block test (each row tests on its own).
    #[allow(clippy::type_complexity)]
    fn block_rowfilter(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _row: &RowVals, _j: Value, _mask: Mask, _fill: &mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env, &[i16]) -> bool) -> Option<Value> {
        None
    }
    /// D3168 (join.probe.staged): after `window`, the `Bool` under which this probe runs its rows in stages over a block
    /// (its build's KeyHash directory and records past L2); the frame then lowers a body with `Env::batch` naming it.
    /// `None`: never.
    fn batch_guard(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Option<Value> {
        None
    }
    /// D3168 S0: the step stages its rows in some window (`batch_guard`): its pipeline lowers a staged function and a plain
    /// one (frame.rs `pipeline_fns_in`).
    fn stages(&self) -> bool {
        false
    }
    /// D3168 S0: at the plain function's entry, before anything it runs, the branch to `ls.0` (the call runs the staged
    /// function instead) or `ls.1` (it runs on). The default runs on.
    fn stage_entry(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env, ls: (Block, Block)) {
        b.br(ls.1, &[]);
    }
    /// D3168: in that body, the probe's stages over the rows `rows` of the block at row `j` (`row.0` the source's row
    /// `j`; `row.1` maps a column of the probe's row to the source row's through the plain projections between,
    /// `forwards`), `fill` decoding the key columns' late lanes first. `None`: no stages (each row runs on its own).
    #[allow(clippy::type_complexity)]
    fn block_batch(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _row: (&RowVals, &dyn Fn(i16) -> Option<i16>), _rows: crate::frame::env::BatchRows, _fill: &mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env, &[i16]) -> bool) -> Option<crate::frame::env::Batched> {
        None
    }
    /// D3168: a step that hands on every row it takes and computes nothing a row could raise on or act through (a plain
    /// projection): `Some` of the column of its own row that column `a` of its output passes on bare (`None`: computed).
    fn forwards(&self, _a: i16) -> Option<Option<i16>> {
        None
    }
    /// H29.
    fn rescan(&self) -> Rescan {
        Rescan::Reopen
    }
    /// H35.
    fn hoist(&self, _h: &mut Hoist) {}
    /// Every row the step gets goes on once and it computes nothing the end reads (a projection the end does not read, a
    /// constant): the block path may then fold a whole block at the end (`EndMech::block`, mask.fold).
    fn keeps_rows(&self) -> bool {
        false
    }
    /// D1071 (V4R-n): the step holds rows back that the input's end hands on (`flush`). The frame lowers an end-of-input
    /// path only for a pipeline with such a step, so every other program is unchanged.
    fn flushes(&self) -> bool {
        false
    }
    /// D3810: an expanding step that lowers its continuation at one place (a nest whose matched and NULL-extended rows share
    /// one block, nestloop/lower.rs), so an owner nest whose inner holds it still walks once (rerun/inner.rs `walks_once`).
    fn hands_on_once(&self) -> bool {
        false
    }
    /// D3274: the step acts at an embedded inner's input end (`ended`). The inner's walk lowers that path only for an inner
    /// with such a step, so every other program is unchanged.
    fn ends(&self) -> bool {
        false
    }
    /// D3274: at an embedded inner's input end, each re-open's walk reaching it (its source's end, or a gate that failed at
    /// the re-open: the node above PostgreSQL's scan gets no row), before the walk leaves: a lock's EvalPlanQualEnd
    /// (ExecLockRows at its input's end).
    fn ended(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env) {}
    /// D2152: after the prologue, the `Bool` that holds when the step passes no row this call (a stopped feeder, a false
    /// one-time gate, a zero LIMIT): the frame then never opens the source and runs no window, as PG never pulls below the
    /// node, so nothing there (an index key over an InitPlan, a late bound) is evaluated. `None`: the step never closes it.
    fn closes(&self, _b: &mut FunctionBuilder<'_>, _env: &Env) -> Option<Value> {
        None
    }
    /// EXPLAIN ANALYZE only (made in the instrumented program, never read by the plain one): after the prologue, the `Bool`
    /// that holds when the step's first window will pass no row although `closes` does not close the call (a LIMIT already
    /// met by an earlier call that shares its count: a union's later arm): PostgreSQL never calls the nodes below it, so
    /// they count no loop (`instr::count_open`). `None`: no such state.
    fn met_at_open(&self, _b: &mut FunctionBuilder<'_>, _env: &Env) -> Option<Value> {
        None
    }
    /// D1071: at the input's end, what the step holds back handed on through `next` from the end's fresh `carry`; the carry
    /// after it. A stop inside leaves by the frame's flush exits, which keep the source's position: the resumed call reaches
    /// the end again and the step's own words say what it still holds.
    fn flush<'e>(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env<'e, '_>, carry: Carry<'e>, _next: Next<'_, 'e>) -> Carry<'e> {
        carry
    }
    /// H28 (M4-plan §1.3 L0-f): the step reads the row's locator (`Env::loc`), a lock step (`LocatorUse::Lock`) whose
    /// pipeline ends in `end.sink`: the frame sets it as for a row-op end.
    fn wants_locator(&self) -> bool {
        false
    }
    /// M8 (nest.loop, M8-plan §3.0): the hosts of the source it drives inside its row (an embedded inner), declared with the
    /// pipeline's own source (frame.rs `pipeline_fn`).
    fn declare(&mut self, _pb: &mut ProgramBuilder) {}
    /// M8: the step holds per-program state no worker may share (an embedded inner's site): its pipeline runs serial.
    fn serial(&self) -> bool {
        false
    }
}

/// V4R-240: expr.core's compile / type / emit for the kinds of later milestones that evaluate expressions outside their
/// pipeline's Filter / Project ops (ON CONFLICT DO UPDATE's SET / WHERE, MERGE's actions): `need_kind::<ExprFace>(px,
/// KindId::ExprCore)`. `outer`: every `Var(varno, n)` is entry `n` of the list; `at` = `(k, j)`, op `j` of pipeline `k`.
pub trait ExprMech: Sync {
    fn compile<'p>(&self, px: &mut Px<'_, 'p>, node: Node<'p>, outer: Option<(i32, &NodeList<'p>)>, id: i32) -> ExId;
    fn compile_at<'p>(&self, px: &mut Px<'_, 'p>, at: (usize, usize), node: Node<'p>, outer: Option<(i32, &NodeList<'p>)>, id: i32) -> ExId;
    /// `(value, valid)`, raising at the row on an error.
    fn emit(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, id: i32, row: &RowVals, ex: ExId) -> (Value, Value);
    /// true and not NULL.
    fn pred(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, id: i32, row: &RowVals, ex: ExId) -> Value;
    fn out_type<'p>(&self, gx: &Gx<'_, 'p>, node: Node<'p>, id: i32) -> Oid;
    fn tf_of(&self, gx: &Gx<'_, '_>, typid: u32) -> TypeFacts;
}

/// expr.core's row for `ExprMech`.
pub struct ExprFace(pub &'static dyn ExprMech);
