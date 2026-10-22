//! The fold face (plan §3.1; H01c H02f H14 H20): an aggregate's transition class over its arguments' Reps. A class reads
//! its argument cells only through each Rep's `datum` accessor (rep/mod.rs), so it never matches on a Rep. The fold ends
//! hold the accumulators: `fold.plain`, `fold.filter`, and the keyed and window ends. A class says how to init, step,
//! merge and finish one accumulator, either in registers (the carry) or in a record slot. PG's rules hold throughout: its
//! transition semantics (strictness, NULL skipping, float NaN / Inf, numeric scale) and its final functions.
//!
//! Each class kind (face Fold, `class.word` ... `class.other`) registers a `ClassImpl`. The roads below are the block fold
//! (H01 c), answered windows (H02 f), the numeric accessor (H14), multiplicity folding (H20) and moving frames.
use gravity_decompose::ops::shared::agg::AggDecl;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;
use gravity_program::SiteDesc;

use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::frame::frame::{Mask, WordWin};
use crate::frame::px::Px;
use crate::admit::refusal::Site;
use crate::rep::CellRef;

/// Where one accumulator lives: SSA values in the carry, which a step replaces, or a slot at `base + off` in a record, a
/// state slot or a scratch area, with its validity as the layout marks it (offsets relative to `base`, like `off`).
pub enum Acc<'a> {
    Regs(&'a mut [Value]),
    Slot { base: Value, off: i32, valid: gravity_expr::emit::CellValid },
}

/// An accumulator's shape. In registers it takes `words` values; in a slot it takes `bytes` at `align`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccShape {
    pub words: u8,
    pub bytes: u32,
    pub align: u8,
}

/// D3024: a packed numeric aggregate's lazily read final (`FoldClass::packed_num`): `sum` an i128 sum `[lo, hi]` at
/// `scale` rendered through `render` (`(region, lo, hi, scale, scale)`), else a min / max mantissa word at `scale`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackedNum {
    pub sum: bool,
    pub scale: u16,
    pub render: usize,
}

/// A transition class (registered as `ClassImpl` by the class kinds).
pub trait FoldClass: Sync {
    fn shape(&self, agg: &AggDecl<'_>) -> AccShape;
    /// The initial state (PG's initcond, or NULL / empty).
    fn init(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, agg: &AggDecl<'_>, acc: Acc<'_>);
    /// One input row: `args` are the aggregate's argument cells, read through their Rep's datum.
    fn step(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, agg: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>]);
    /// Combines two partial states (workers, parts, spilled partitions), as PG's combinefn does.
    fn merge(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, agg: &AggDecl<'_>, into: Acc<'_>, from: Acc<'_>);
    /// The final value and its null flag.
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, agg: &AggDecl<'_>, acc: Acc<'_>) -> (Value, Value);
    /// D1520: `finish` with an ordered-set aggregate's direct arguments given over the group's row (each its datum word and
    /// how it is given, kernels `DIRECT_*`); a keyed end hands them only to the hosted fold (keyed/stream.rs `finals_of`).
    fn finish_direct(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _agg: &AggDecl<'_>, _acc: Acc<'_>, _direct: &[(Value, Value)]) -> (Value, Value) {
        crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "direct arguments handed to a class with no hosted final" })
    }
    /// D1371: a host-sorted aggregate's sort and transitions over its group's run, ahead of the group's finals (effects
    /// `folds_ahead`); only the hosted fold keeps such a run.
    fn fold_ahead(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _agg: &AggDecl<'_>, _acc: Acc<'_>) {
        crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a fold ahead of the finals handed to a class with no hosted run" })
    }
    /// How this class's final value orders (a top-k over aggregate finals, `runs.topk_groups`); `None`: not orderable here.
    fn order(&self, _agg: &AggDecl<'_>) -> Option<FinalOrder> {
        None
    }
    /// D3024: the packed numeric form whose final a keyed emit reads lazily (`gravity_expr::emit::CellForm::PackedNum`): the
    /// slot's raw words and their scale, and the sum's render host; `None`: the eager `finish`.
    fn packed_num(&self, _agg: &AggDecl<'_>) -> Option<PackedNum> {
        None
    }
    /// H01 c: the form in which this class folds a whole 64-row mask, if any.
    fn block(&self, _agg: &AggDecl<'_>) -> Option<BlockFold> {
        None
    }
    /// H02 f: the answer record this class folds instead of an answered window's rows, if any.
    fn answer(&self, _agg: &AggDecl<'_>) -> Option<AnswerForm> {
        None
    }
    /// V4R-38: one row's exact numeric term, a num.window product the end folded (`WideTerm`), into `acc`: the class that
    /// takes one (class.num_wide) folds it as its step folds a datum; any other refuses (an end hands it only that class).
    fn fold_wide(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _agg: &AggDecl<'_>, _acc: Acc<'_>, _t: &WideTerm) {
        crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a wide numeric term handed to a class that takes none" })
    }
    /// V4R-215: whether `put` writes a fresh record's slot from a partial state held in registers.
    fn puts(&self, _agg: &AggDecl<'_>) -> bool {
        false
    }
    /// Whether this class folds in its record's slot only, never in a run carry's registers (ops/agg/hashagg.rs): a state
    /// whose merge at each key change costs more than a step into the record (the interval sum's eight words).
    fn record_only(&self, _agg: &AggDecl<'_>) -> bool {
        false
    }
    /// V4R-252 (D720): whether this is keyed/parted.rs's deferred minimum (`CodeSlot`), by type (a class's address is no name).
    fn deferred_code(&self) -> bool {
        false
    }
    /// V4R-549 (D720): whether this is fold/num_datum.rs's DATUM_DS, by type (a zero-sized class shares its address).
    fn datum_ds(&self) -> bool {
        false
    }
    /// D2916: the class kind this class is registered as, by type (a zero-sized class has no address of its own: class_float
    /// sat at class_word's in one link), for a reader that asks whether a slot's class is the one its kind names; `None`
    /// where no reader asks (num.partial's classes answer).
    fn class_kind(&self) -> Option<KindId> {
        None
    }
    /// V4R-215: a fresh record's slot written once from the registers `from`, as `init` then `merge` from them would leave it
    /// (PG's combinefn from the identity); an end calls it only when `puts`.
    fn put(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _agg: &AggDecl<'_>, _into: Acc<'_>, _from: &[Value]) {
        crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a fresh slot put to a class that puts none" })
    }
    /// D3224 A3: whether `avg`'s final may read `sum`'s slot for its sum, the two folding one transition state (table/rec.rs
    /// `GroupRec::share` asks the record's side: one argument and FILTER); the class answers its own (one scale, one form).
    /// Such an avg steps its count alone (`count`) and finishes over both slots (`finish_shared`).
    fn shares(&self, _sum: &AggDecl<'_>, _avg: &AggDecl<'_>) -> bool {
        false
    }
    /// D3224 A3: a sharing avg's step (`shares`): its count alone, as `step` counts the row.
    fn count(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _agg: &AggDecl<'_>, _acc: Acc<'_>, _args: &[CellRef<'_>]) {
        crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a count step to a class that shares no state" })
    }
    /// D3224 A3: a sharing avg's final (`shares`): its sum from the sum's slot `sum`, its count from its own `own`.
    fn finish_shared(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _agg: &AggDecl<'_>, _sum: Acc<'_>, _own: Acc<'_>) -> (Value, Value) {
        crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a shared final to a class that shares no state" })
    }
}

/// V4R-38: a row's numeric term at the aggregate's scale as an i256 (four words of two's complement), its specials code
/// (`None`: no leaf can be special; else 0 for a finite term, else PG's NaN / Infinity code) and whether it is entered.
pub struct WideTerm {
    pub w: [Value; 4],
    pub sp: Option<Value>,
    pub ok: Value,
}

pub struct ClassImpl(pub &'static dyn FoldClass);

/// A final's order image: the slot's signed word; a rational `(sum i128, n)` compared exactly by cross-multiplying; an f64
/// with NaN largest and -0 == +0; a code or image compared as text under the aggregate's collation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinalOrder {
    Word,
    Rational,
    F64,
    Text,
}

/// Class kind `k`'s implementation, or `NotBuilt` by its name.
pub fn class(cx: &impl Site, k: KindId) -> &'static dyn FoldClass {
    crate::frame::iface::kind_impl::<ClassImpl>(cx, k).0
}

crate::hook_variants! {
    /// Block folds (H01 c): one pass over a whole 64-row mask.
    "H01c" pub enum BlockFold {
        /// A popcount of the mask.
        Count = Strategy(MASK_FOLD),
        /// A masked sum of one lane.
        Sum = Strategy(MASK_FOLD),
        Min = Strategy(MASK_FOLD),
        Max = Strategy(MASK_FOLD),
        /// FOLD-13: min / max of the lane's sign-flipped image (an unsigned word order).
        MinImage = Strategy(MASK_FOLD),
        MaxImage = Strategy(MASK_FOLD),
    }
}

/// The block-fold road's mechanism (registered as `BlockFoldImpl`): the mask's rows of lane `lane` in block
/// `[base, base + 64)` folded into `acc`.
pub trait BlockFoldMech: Sync {
    fn fold(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, form: BlockFold, w: &WordWin, lane: usize, base: Value, m: Mask, acc: Acc<'_>);
}

pub struct BlockFoldImpl(pub &'static dyn BlockFoldMech);

crate::hook_variants! {
    /// Window and part answers from the source's stats records (H02 f): count / sum / min / max / counted value.
    "H02f" pub enum AnswerForm {
        /// Answers read from stats records.
        Stats = Strategy(ANSWER_STATS),
        /// `count = nonnull - count_v` answers.
        Counted = Strategy(ANSWER_COUNTED),
        /// Numeric SUM answers over packed lanes.
        Numeric = Strategy(ANSWER_NUMERIC),
    }
}

/// An answer road's mechanism (registered as `AnswerImpl`).
pub trait AnswerMech: Sync {
    /// Declares the answer `agg` takes on the source's site (`answer_lanes`). Returns `false` when this answer does not
    /// serve `agg`.
    fn declare(&self, px: &mut Px<'_, '_>, site: &mut SiteDesc, agg: &AggDecl<'_>) -> bool;
    /// Folds an answered window's record at `rec` into `acc` instead of its rows.
    fn fold(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, agg: &AggDecl<'_>, acc: Acc<'_>, rec: Value);
}

pub struct AnswerImpl(pub &'static dyn AnswerMech);

crate::hook_variants! {
    /// The numeric Rep accessor and the per-row memo (H14).
    "H14" pub enum NumRoad {
        /// The window-word mantissa fold (Smulh + AddHi, sentinels).
        Window = Strategy(NUM_WINDOW),
        /// A per-(row, cell / ExId) memo of mantissas and subtrees.
        Memo = Strategy(NUM_MEMO),
        /// In-place decode of a short numeric image (at most 3 digit groups).
        Short = Strategy(NUM_SHORT),
        /// The lean exact add of a finite heap / verbatim word into a sum / avg slot, cold generic fallback.
        Lean = Strategy(NUM_LEAN),
        /// A hashed group's per-claim i64 partials, closed into the table each window.
        Partial = Strategy(NUM_PARTIAL),
        /// num.partial's block of a one-byte key pair, placed by its bytes, tried before the image's hash.
        Pair = Strategy(NUM_PAIR),
        /// num.partial's terms: a shared subtree and a column's gate once per row, typmod-bounded nodes unchecked.
        Share = Strategy(NUM_SHARE),
        /// num.partial's terms: a short heap image at a typmod scale of 0..4 decoded by one select, no weight switch.
        Narrow = Strategy(NUM_NARROW),
        /// A packed numeric aggregate's final compared with a constant over its raw words (D3024).
        AggCmp = Strategy(NUM_AGGCMP),
        /// D3233: a CASE as the rows it selects: a CASE aggregate with a NULL arm folds only its taken rows (num_case.rs
        /// `select`), and an expression's CASE arm that computes anything runs only where taken (gravity_expr `CaseSelect`).
        CaseSelect = Strategy(CASE_SELECT),
        /// A sum / avg argument's decided scale: a mixed-scale CASE beside a larger fixed scale (D4567).
        Scale = Strategy(NUM_SCALE),
        /// A sum / avg of a division by a constant at a decided scale, its display scale by row (D4567 (a)).
        Div = Strategy(NUM_DIV),
        /// A min / max of a CTE's one-scale numeric column over its order keys (D4723).
        Cte = Strategy(NUM_CTE),
    }
}

/// Road `num.short`'s mechanism (`NumShortImpl`): the numeric image at `ptr` read in place at `scale` (a varlena datum):
/// its mantissa word (a sentinel for a special), whether the image fit (else the caller's host call decodes it) and, with
/// `ds`, its dscale.
pub trait NumShortMech: Sync {
    fn decode(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, ptr: Value, scale: i16, ds: bool) -> (Value, Value, Option<Value>);
}

pub struct NumShortImpl(pub &'static dyn NumShortMech);

crate::strategies! {
    /// Accumulators by inner position. No mechanism yet: its readers take the straight form.
    NEST_GROUPJOIN = 100, "nest.groupjoin", NestFold;
    /// Run-length products for count-only consumers. No mechanism yet: its readers take the straight form.
    MERGE_MULT = 114, "merge.mult", Merge;
}

crate::hook_variants! {
    /// Multiplicity folding (H20). A fold over a join or merge whose arguments are counts, or one side's columns, runs as
    /// per-entry counters or run-length products.
    "H20" pub enum Multiplicity {
        /// Per-entry counters for a fold over a hash join.
        JoinCounters = Strategy(JOIN_MULT),
        /// Accumulators by inner position (a group join).
        GroupJoin = Strategy(NEST_GROUPJOIN),
        /// Run-length products for count-only consumers of a merge join.
        MergeRuns = Strategy(MERGE_MULT),
    }
}

crate::strategies! {
    /// Moving frames by inverse transition (minvfunc) or a sliding structure. No mechanism yet: its readers take the
    /// straight form.
    WIN_INVERSE = 111, "win.inverse", Window;
}

crate::hook_variants! {
    /// Moving-frame folds: a window frame whose head moves. Its rows leave the accumulator by PG's inverse transition
    /// (`aggminvtransfn`) or through a sliding structure, never by a re-fold unless PG re-folds.
    "H19 -" pub enum Moving {
        Inverse = Strategy(WIN_INVERSE),
    }
}

crate::hook_variants! {
    /// A var / stddev call's register form: PostgreSQL's own transition state in record slots (class.stats).
    "-" pub enum StatRoad {
        Register = Strategy(STAT_REG),
    }
}
