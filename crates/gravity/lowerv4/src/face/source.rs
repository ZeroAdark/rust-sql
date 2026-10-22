//! The source face (plan §3.1; H01 H02 H03 H06 H22 H28): a pipeline's rows in windows. The v3 frame trait carries over
//! (lowerv3-final emit/frame.rs), with the v3-r1-a4 block hook (`word_win`, H01) and the declared H-variants added: window
//! facts (H06), the verdicts a source answers (H02), its read plan (H03), its declared order (H22), its row locator (H28)
//! and its decode order (H01 e). Source kinds (kinds.rs, face Source) are made by `frame::SourceMaker`.
//!
//! Every face declares its hook families' variants with `hook_variants!` (below): one row per variant of the lever map
//! (`shared-hooks.md` H01–H35), naming the kind or road that builds it, so an unbuilt variant refuses
//! `NotBuilt { hook, variant, milestone }` by that name, with its declaration's hook and milestone. Closed at M1's exit.
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, Value};
use gravity_program::{ArenaRef, Pipeline, SiteDesc};

use crate::cell::LaneId;
use crate::face::strategy;
use crate::frame::env::Env;
use crate::frame::frame::{Win, WordWin};
use crate::frame::open::OpenWords;
use crate::frame::px::{ExId, Px};
use crate::frame::row::{Carry, Mask, RowVals};
use crate::admit::refusal::Variant;
use crate::ops as registry;

/// M6 D2 (L0-g, road `jsonb.lanes`): a drafted pgrc site's lanes before its quals compile (the path lanes the leading
/// filters read), pipeline `k`'s. source/pgrc.rs calls it when the road is registered and on.
pub struct SiteDraft(pub fn(&Px<'_, '_>, usize, &mut SiteDesc));

/// The `jsonb.lanes` draft (`SiteDraft`) of pipeline `k`'s pgrc `site`, when the road is built.
pub fn site_draft(px: &Px<'_, '_>, k: usize, site: &mut SiteDesc) {
    if let Some(d) = registry::strategy(strategy::JSONB_LANES).and_then(|m| m.downcast_ref::<SiteDraft>()) {
        (d.0)(px, k, site);
        // P1b: the int8 number paths after the text ones (their shred indices follow jsonb.lanes' `paths`)
        crate::ops::scan::pgrc::strategies::jsonb_nums::engage(px, k, site);
        // P1c: the paths a group reads over the column its projection forwards (strategies/jsonb_fwd.rs)
        crate::ops::scan::pgrc::strategies::jsonb_fwd::engage(px, k, site);
    }
    // D3038: a varlena column's `octet_length` as a per-row lane (strategies/octet_lane.rs; percode.length gates it)
    crate::ops::scan::pgrc::strategies::octet_lane::engage(px, k, site);
}

/// A hook family's variant list. Several variants may name one road. A road's registered value has its face's type, which
/// the caller names (`need::<T>`); a variant with no face type registers `&()` and is only `require`d. `HOOKS` lists the
/// family's H-ids (`-`: roads model.py gives no hook).
#[macro_export]
macro_rules! hook_variants {
    (@id Kind $id:ident) => { $crate::admit::refusal::Variant::Kind($crate::face::kinds::KindId::$id) };
    (@id Strategy $id:ident) => { $crate::admit::refusal::Variant::Strategy($crate::face::strategy::$id) };
    ($(#[$m:meta])* $hooks:literal pub enum $name:ident { $($(#[$vm:meta])* $v:ident = $what:ident($id:ident),)* }) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name { $($(#[$vm])* $v),* }

        impl $name {
            pub const HOOKS: &'static str = $hooks;
            pub const ALL: &'static [$name] = &[$($name::$v),*];

            /// The kind or road this variant is: the name its `NotBuilt` carries.
            pub const fn variant(self) -> $crate::admit::refusal::Variant {
                match self { $($name::$v => $crate::hook_variants!(@id $what $id)),* }
            }

            /// Some appended registry table provides it.
            pub fn built(self) -> bool { $crate::face::source::built(self.variant()) }

            /// Built, not dropped by its twin A/B and not switched off (`gravity_road_off`): the mechanism emits this form.
            pub fn on(self) -> bool { $crate::face::source::on(self.variant()) }

            /// The refusal path: `NotBuilt` by this variant's name unless a table provides it.
            pub fn require(self, cx: &impl $crate::admit::refusal::Site) {
                if !self.built() {
                    $crate::not_built!(cx, self.variant())
                }
            }

            /// The implementation registered for this variant, as its face's type `T` (`NotBuilt` when unbuilt).
            pub fn need<T: 'static>(self, cx: &impl $crate::admit::refusal::Site) -> &'static T {
                $crate::frame::iface::need_variant::<T>(cx, self.variant())
            }
        }
    };
}

pub fn built(v: Variant) -> bool {
    match v { Variant::Kind(k) => registry::kind(k).is_some(), Variant::Strategy(r) => registry::strategy(r).is_some() }
}

pub fn on(v: Variant) -> bool {
    match v { Variant::Kind(_) => built(v), Variant::Strategy(r) => built(v) && !strategy::off().has(r) && !registry::dropped_strategies().has(r) }
}

/// What a source knows of one window (H06 a / b). Each fact is a condition word (nonzero: holds), or `None` when unknown.
/// The frame versions the row loop on them (at most two bodies, `BodyForm`); an unknown fact takes the straight body.
#[derive(Clone, Copy, Debug, Default)]
pub struct WinConds {
    /// No lane the chain reads has a null in the window (pgrc all-valid validity, heap NONULLS).
    pub nonulls: Option<Value>,
    /// The window's rows are its lanes' rows in order (no selection vector).
    pub nosel: Option<Value>,
    /// The window's keys are dense (a direct-indexed table takes them).
    pub dense: Option<Value>,
    /// The runs word (H22, run collapse; M2 decision 7, M3 L0-b): a condition word. Nonzero: the window's rows come in runs
    /// of equal group keys (a key order the source proves: pgrc's declared sort prefix); 0: no runs, the straight road;
    /// `None`: unknown, the compare path.
    pub runs: Option<Value>,
    /// Heap: every line pointer of the page is a visible tuple (PAGE_FULL).
    pub page_full: Option<Value>,
    /// Code lanes are served as codes, not decoded values (H06 b).
    pub codes: Option<Value>,
    /// The host answered the window from its stats records (H02 f): no lane is loaded and no row may be read; the row
    /// count is still `end - cursor`. Set only on a site whose answer lanes the fold agreed to (fold.rs).
    pub answered: Option<Value>,
    /// Every row of the window (after SEL) passes the leading filters `SourceMech::covers` names (AllMatch, an answered
    /// window, a condition-cache hit); the frame versions the body with those steps elided.
    pub covered: Option<Value>,
    /// The filter lanes are ByteFor at width 1 or 2 in this window: body.form's packed body reads `SourceMech::packed`.
    pub packed: Option<Value>,
    /// The site's per-code tables are bound with no faulting entry (percode_bind.rs `clean`): no row re-evaluates.
    pub clean: Option<Value>,
    /// V4R-19 (A): `(cond, datum)`: when the Bool `cond` holds, the end's one group key is the non-NULL I64 `datum` (its
    /// image word) on every row of the window (exact zone stats); the run carry then folds the window's rows into its open
    /// run with no key compare (keyed/run_collapse.rs). `datum` means nothing when `cond` fails.
    pub const_key: Option<(Value, Value)>,
}

/// One lane's 64-row block as raw ByteFor deltas (value = reference + delta, unsigned; every row in the reference's frame).
#[derive(Clone, Copy, Debug)]
pub struct Packed {
    /// The block's first delta (row `base`); a NULL row's delta is garbage (validity still ANDs in).
    pub deltas: Value,
    /// The delta width in bytes (I64: 1 or 2, or 0 for a constant frame), elected per stream per part: known at run time.
    pub width: Value,
    /// The frame's reference word (I64): the lane type's datum word of delta 0.
    pub reference: Value,
    /// The frame's largest value (I64) when known: a range it misses clears the block unread, one it covers sets it whole.
    pub fmax: Option<Value>,
}

crate::hook_variants! {
    /// How the frame versions a window's body on its facts (H06).
    "H06" pub enum BodyForm {
        /// (a) the facts pick one of at most two bodies: no-null lanes, no selection, dense keys, the runs word, PAGE_FULL,
        /// per-code tables bound clean.
        Versioned = Strategy(BODY_VERSIONED),
        /// (b) validity and form per window: all-valid vs mixed, codes vs decoded.
        Form = Strategy(BODY_FORM),
    }
}

/// The facts a window's fast body runs under (H06): each `true` holds for every row of that body, so a source or step
/// reading `Env::body` skips the test it answers. Outside the fast body every field is `false`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Body {
    pub nonulls: bool,
    pub nosel: bool,
    pub dense: bool,
    pub codes: bool,
    pub page_full: bool,
    /// The packed body: the first step's masks read the block's ByteFor deltas (`Env::packed`) where a qual allows.
    pub packed: bool,
    /// The per-code tables are clean: a row reads no fault table (step/percode_len.rs `table_ref`); outside it, every row does.
    pub clean: bool,
}

/// A body road's mechanism (`BodyImpl`, for `body.versioned` / `body.form`): the window's guard (`I1`) and the facts the
/// fast body runs under, or `None` when the window's facts give no fast body.
pub trait BodyMech: Sync {
    fn guard(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, facts: &WinConds) -> Option<(Value, Body)>;
}

pub struct BodyImpl(pub &'static dyn BodyMech);

/// src.prune's skip bit `(word, bit)` of arm `i` of `n` (pipeline `k`, plan node `node`) for another kind's arms (D509: a MergeAppend's).
pub struct PruneImpl(pub for<'a, 'b, 'c> fn(&mut Px<'a, 'b>, usize, u16, u16, &gravity_decompose::ops::shared::append::PruneDesc<'c>, i32) -> (u16, u32));

crate::hook_variants! {
    /// The verdicts a source answers per window before its rows are read (H02). (f), answered records, is fold.rs's
    /// `AnswerForm`: the fold reads the answer records the source publishes.
    "H02" pub enum VerdictKind {
        /// (a) min / max zones.
        Zone = Strategy(VERDICT_ZONE),
        /// (b) bloom `NoMatch` on equality.
        Bloom = Strategy(VERDICT_BLOOM),
        /// (c) the PSMA candidate row window.
        Psma = Strategy(VERDICT_PSMA),
        /// (d) `AllMatch` skips the steps the verdict covers.
        AllMatch = Strategy(VERDICT_ALLMATCH),
        /// (e) filter-only lanes stay undecoded.
        Elide = Strategy(VERDICT_ELIDE),
        /// (g) keyed on lane identity `(attno, path_ord)`: shredded path lanes get zone, bloom and PSMA.
        PathLane = Strategy(JSONB_LANES),
        /// (h) verdicts against runtime constants (consts.rs `RtConst`).
        RtConst = Strategy(VERDICT_RTCONST),
        /// (i) the condition cache: a qual's per-granule survivor words kept across statements under the qual's
        /// fingerprint (which includes its constants' types / typmods and its collations) and the granule's identity
        /// (`SourceMech::granule_ident`). A hit is `NoMatch`, `AllMatch` or the window mask; a miss evaluates and
        /// publishes. Only quals whose every function and operator is IMMUTABLE consult or fill it (main 09-24): never
        /// stable, volatile, param or subplan quals, so no `now()`, `current_setting()` or DateStyle / TimeZone /
        /// lc_time dependent cast. Its `VerdictMech::declare` returns `false` for any other qual. The cached mask is
        /// the qual's result alone; snapshot visibility and delete vectors are applied after it, never cached.
        CondCache = Strategy(VERDICT_COND),
        /// A code-range verdict against a per-code table (codes.rs `PerValue`).
        CodeRange = Strategy(VERDICT_CODERANGE),
        /// N1 (M3 decision 2): a code filter's survivors selected without a base pass, under a count-ordered top.
        CodeSparse = Strategy(CODE_SPARSE),
    }
}

/// A window's verdict as the `I64` word a verdict road yields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i64)]
pub enum Verdict {
    NoMatch = 0,
    Maybe = 1,
    AllMatch = 2,
}

/// A verdict road's mechanism (registered as `VerdictImpl`).
pub trait VerdictMech: Sync {
    /// Declares the verdict of filter `qual` on the source's site (zone predicates, filter and answer lanes), which the host
    /// answers per granule. Returns `false` when `qual` takes no verdict of this kind.
    fn declare(&self, px: &mut Px<'_, '_>, site: &mut SiteDesc, qual: ExId) -> bool;
    /// The window's verdict computed in IR at the window's open (runtime constants), as a `Verdict` word. `None` means the
    /// host's answer stands.
    fn window(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _win: &Win) -> Option<Value> { None }
}

pub struct VerdictImpl(pub &'static dyn VerdictMech);

crate::strategies! {
    /// Batched ordered index probes, descent reuse, TID prefetch. No mechanism yet: its readers take the straight form.
    NEST_BATCH = 99, "nest.batch", NestParam;
}

crate::hook_variants! {
    /// The I/O a source asks of the host (H03). These are cross-crate: hosts / bufmgr build them, v4 declares them.
    "H03" pub enum ReadForm {
        /// (a) heap read-stream look-ahead.
        Stream = Strategy(HEAP_STREAM),
        /// (b) verdict-aware coalesced granule reads (hosts: granule_source).
        Claim = Strategy(READ_CLAIM),
        /// (c) TID-driven heap prefetch of an index inner's next K rows (nest.rs `ProbeForm::Batch`). (d), no checksum
        /// re-verify on hot re-reads, is not built: a read that did not block proves no earlier page verification.
        TidPrefetch = Strategy(NEST_BATCH),
    }
}

/// A source's read plan: the set of `ReadForm`s it asks for (none: plain reads).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadPlan(pub u8);

impl ReadPlan {
    pub const PLAIN: ReadPlan = ReadPlan(0);

    pub const fn with(self, f: ReadForm) -> ReadPlan {
        ReadPlan(self.0 | 1 << f as u8)
    }

    pub fn forms(self) -> impl Iterator<Item = ReadForm> {
        ReadForm::ALL.iter().copied().filter(move |&f| self.0 >> f as u8 & 1 == 1)
    }
}

crate::hook_variants! {
    /// How a source lays out and decodes its lanes (H01 e, and the heap page road of H01 / H03).
    "H01 H03" pub enum LaneForm {
        /// (e) the key and filter lanes decode first, the other lanes only for the rows that survive.
        LateDecode = Strategy(DECODE_LATE),
        /// Raw pinned heap pages: fixed-offset deform, the word driver, varlena pointers.
        HeapPages = Strategy(HEAP_PAGES),
    }
}

/// When a source decodes its lanes (H01 e): all at the window's open, or the lanes of `first` first (bit i: `WordWin::lanes[i]`;
/// lanes past 64 decode eagerly) and the rest for the rows that survive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DecodeOrder {
    #[default]
    Eager,
    Late { first: u64 },
}

/// A window's granule identity in the condition cache (H02i, `VerdictKind::CondCache`), as `I64` words the source computes
/// at the window. The qual's fingerprint completes the key. Only pgrc parts have one (a heap has no condition
/// cache).
#[derive(Clone, Copy, Debug)]
pub enum GranuleIdent {
    /// A pgrc part: its uuid (two words) and the granule's index in the part.
    Part { uuid: [Value; 2], granule: Value },
}

/// A row's locator (H28): its heap `(block, offset)` as `I64` words, and the pinned buffer holding it when the source knows.
/// D3681: `rel`, a relation-set source's row's own relation (an `I32`, the window's position's: the set's member the walk
/// is in); `None`: the source's one relation.
#[derive(Clone, Copy, Debug)]
pub struct Locator {
    pub block: Value,
    pub offset: Value,
    pub pinned: Option<Value>,
    pub rel: Option<Value>,
}

crate::hook_variants! {
    /// What reads a row's locator (H28).
    "H28" pub enum LocatorUse {
        /// The fetch form of a wide top-k: `(image, locator)` runs, the winners fetched once.
        Fetch = Strategy(TOPK_FETCH),
        /// A row op's target row (end.rs, H27).
        RowOp = Kind(WriteDelete),
        /// LockRows and its EvalPlanQual recheck.
        Lock = Kind(WriteLock),
    }
}

pub trait SourceMech {
    /// The JIT map's name of this mechanism (frame/jit.rs): its type's.
    fn mech_name(&self) -> &'static str {
        core::any::type_name::<Self>()
    }
    fn declare(&mut self, _pb: &mut ProgramBuilder) {}
    fn site_desc(&self, _arena: ArenaRef) -> Option<SiteDesc> { None }
    /// After every make of pipeline `k`, on its drafted site just before it is numbered: lanes that follow every table lane
    /// (the condition cache's verdict lane, M1-SOURCE).
    fn seal(&mut self, _px: &mut Px<'_, '_>, _k: usize, _site: &mut SiteDesc) {}
    /// D824 (V4R-298): the pipeline's row order is observable (a lazy build runs at the first row to reach its probe): a
    /// source that may walk a window out of order (heap.pages' reversed walk under a plain aggregate) walks it forward.
    fn in_order(&mut self) {}
    /// D2561: before the code, on graph pipeline `k`'s source when a later call reads the array it gathers (a group sort's
    /// emit, form G, read by the top's drain) in a program that runs a nested pipeline: keep that array past its call (pinned).
    /// `false`: the source cannot, and the frame refuses the program.
    fn keep_past_call(&mut self, _px: &mut Px<'_, '_>, _k: usize) -> bool { false }
    /// D2561: the source re-reads, in its own call, the record array its top pipeline's group emit gathered (a top's drain,
    /// form G: topn/lower.rs `make_drain`).
    fn rereads_top(&self) -> bool { false }
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline { px.gx.pipelines()[k].desc }
    /// A once source (a Result, a breaker's single emit) has no window loop.
    fn once(&self) -> bool { false }
    /// H17 a: every word the source reads across rows or calls; the frame writes them at the open.
    fn open_words(&self, _w: &mut OpenWords) {}
    /// H03 a–d.
    fn read_plan(&self) -> ReadPlan { ReadPlan::PLAIN }
    /// H02 a–i: the verdicts declared on this source's site.
    fn verdicts(&self) -> &'static [VerdictKind] { &[] }
    /// H01 e.
    fn decode_order(&self) -> DecodeOrder { DecodeOrder::Eager }
    /// H01 e (decode.late): the late lanes decoded before anything reads them. `Some((base, m))`: block `[base, base + 64)`'s
    /// rows in `m`, after step 0's mask and before the end's block fold or the ctz walk; `None`: a row-loop body's start, the
    /// whole window.
    fn fill_late(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _win: &Win, _at: Option<(Value, Mask)>) {}
    /// D3194: the late lanes of columns `attnos` alone decoded for block `at`'s rows, ahead of `fill_late` (a filtered probe
    /// tests its keys first); whether those columns are readable there now. A source that cannot says no and emits nothing.
    fn fill_late_cols(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _win: &Win, _at: (Value, Mask), _attnos: &[i16]) -> bool {
        matches!(self.decode_order(), DecodeOrder::Eager)
    }
    /// D2625: at a nest inner's re-open, before its rerun set runs or any gate is read (rerun/inner.rs `open`): the reset PG's
    /// rescan makes whatever the call then does (a kept table's match flags: hashjoin/drain/source.rs).
    fn reopen(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env) {}
    /// D4062: a source that lowers steps of its own (a nest inner union's arms, rerun/inner_arms.rs) runs their prologues
    /// with its inner's (rerun/inner.rs `prologue`), as a pipeline's frame runs its steps' before the source.
    fn prologue(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _x: &crate::frame::stop::Exits) {}
    /// D4062: and their epilogues with its inner's (rerun/inner.rs `epilogue`).
    fn epilogue(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _x: &crate::frame::stop::Exits) {}
    /// The open: a negative status leaves the function with it.
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value;
    /// The next window with its facts (H06), or `l_eof(status)` at the end (0) or on a stop.
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win;
    /// A1 (D3224): the window's packed numeric lanes' kind tests, once per window (TPDE has no LICM): `(site, lane,
    /// verbatim)` for each `Window` / dynamic-scale PackedNum lane, the Bool rep/packednum/datum.rs `element` reads in place
    /// of its kind byte's load in the window's rows (frame.rs sets them for the row loops alone). Empty: read per row.
    fn num_kinds(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Vec<(usize, u16, Value)> { Vec::new() }
    /// H01: the window's lanes for the 64-row block path; `None` means only the straight row loop runs.
    fn word_win<'e>(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env<'e, '_>, _win: &Win) -> Option<WordWin<'e>> { None }
    /// H06 b: lane `attno`'s block at `base` as raw ByteFor deltas, only under `WinConds::packed`; `None` masks it decoded.
    fn packed(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _win: &Win, _attno: i16, _base: Value) -> Option<Packed> { None }
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e>;
    /// decode.late in the straight rows (H01 e): the leading steps (the pipeline's leading filters) that read only the cells
    /// `row_early` makes; the frame's chain asks `row_late` for the rest once a row passed them. 0: `row` alone.
    fn late_at(&self) -> usize { 0 }
    /// `row` with only the cells the first `late_at` steps read; the other cells' values are placeholders.
    fn row_early<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> { self.row(b, env, win, j, carry, next) }
    /// A `row_early` row with its placeholder cells made, where it passed the first `late_at` steps.
    fn row_late<'x, 'e: 'x>(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env<'e, '_>, _win: &Win, row: RowVals<'x>) -> RowVals<'x> { row }
    /// Road `mask.rows` (frame/mask_rows.rs), right after the `row_early` (`early`) or `row` it asks of: whether that row's
    /// cells stay readable to the window's end (words and pointers into the pinned page, nothing made per row), so the mask
    /// keeps them per row and a passing row's walk reads them back; inside, the value `row_late` resumes from (`None`: a
    /// constant). `None`: not kept (the walk makes the row again).
    fn keep_row(&self, _early: bool) -> Option<Option<Value>> { None }
    /// Road `mask.rows`: a kept row's resume value (`keep_row`) handed back before its `row_late`.
    fn resume_late(&self, _at: Value) {}
    /// H02i: the window's granule identity for the condition cache (pgrc parts only); `None`: the windows are not cached.
    fn granule_ident(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _win: &Win) -> Option<GranuleIdent> { None }
    /// Bit j: pipeline op j is a leading Filter whose qual this source's verdicts cover when `WinConds::covered` holds.
    fn covers(&self) -> u64 { 0 }
    /// In the uncovered body, right after the last covered step passed `row` (only when `covers() != 0`): a condition-cache
    /// fill records the row from what the statement evaluates anyway (no evaluation ahead: laziness, D237).
    fn passed(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _win: &Win, _row: &RowVals) {}
    /// A lowering-time fact that must agree with `passed` / `passed_block`: false, they emit nothing for this source (no
    /// condition-cache lane), so the frame builds no fill (no per-row `passed`, and the block path folds).
    fn fills(&self) -> bool { true }
    /// Whether the fill also takes whole blocks (`passed_block`); false keeps the block path from folding while it fills.
    fn fills_blocks(&self) -> bool { false }
    /// `passed` for a block (only when `fills_blocks`): bit i of `m` = row `base + i` passed every covered step.
    fn passed_block(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _win: &Win, _base: Value, _m: Mask) {}
    /// H28.
    fn locator(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _row: &RowVals) -> Option<Locator> { None }
    /// M7-SPILL (V4R-359): a site-less source whose claims are not a site's (a spill pair's partitions) names the program
    /// site its pipeline declares as its nominal driving site (the runner reads no row through it).
    fn nominal_site(&self, _px: &Px<'_, '_>) -> Option<usize> { None }
    /// The source's own position write-back when it has no site (a breaker's emit).
    fn write_back(&self, _b: &mut FunctionBuilder<'_>, _env: &mut Env, _cur: Value, _out: Value) {}
    /// D732-rest (V4R-290): a COPY-marked subquery's entry (`copy_row::wrap`): the frame's call is its stored row first.
    fn copy_entry(&self) -> Option<&crate::rerun::epq::copy_row::CopyEntry> { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::face::strategy::{Strategy, STRATEGIES};
    use crate::dict::face as codes;
    use crate::face::{consts, end, policy, rewrite, runs, spill, step};
    use crate::face::fold as fold;
    use crate::ops::hashjoin::face as join;
    use crate::table::{face as keyed, keyimg};
    use crate::rerun::face as nest;
    use crate::admit::refusal::{Reason, Refusal};

    /// Every face's variant lists: (HOOKS, variants), each variant's refusal checked on the way.
    macro_rules! lists {
        ($($t:ty),* $(,)?) => {{
            let mut out: Vec<(&'static str, Vec<Variant>)> = Vec::new(); // alloc: Program
            $(
                for &v in <$t>::ALL {
                    let want = Reason::NotBuilt { hook: v.variant().hook(), variant: v.variant(), milestone: v.variant().milestone() };
                    assert!(v.built() || crate::catch(0, || v.require(&0)) == Err(Refusal { reason: want, node: 0, native_rel: 0 }), "{v:?}");
                }
                out.push((<$t>::HOOKS, <$t>::ALL.iter().map(|v| v.variant()).collect())); // alloc: Program
            )*
            out
        }};
    }

    #[test]
    fn every_strategy_is_a_face_variant_and_refuses_by_name() {
        let lists = lists!(
            BodyForm, VerdictKind, ReadForm, LaneForm, LocatorUse, step::BlockForm, step::LoopRoad, step::ExprRoad,
            step::Rescan, step::ExpandKind, end::SinkKind, end::Parallel, end::RowOpRoad, end::Finals, fold::BlockFold,
            fold::AnswerForm, fold::NumRoad, fold::Multiplicity, fold::Moving, fold::StatRoad, keyed::TablePolicy, keyed::Prefetch,
            keyed::CrossPass, keyed::OwnerTop, keyed::PartEmit, runs::RunStore, runs::RunForm, runs::OrderUse, join::Probe,
            join::BuildFrom, join::JoinRoad, nest::ParamOwner, nest::CacheUse, nest::ProbeForm, spill::SpillForm,
            codes::PerValue, codes::CodeRep, codes::DictPolicy, keyimg::ImageForm, consts::RtConst, policy::Width,
            policy::Floor, rewrite::Rewrite,
        );
        let mut seen = [false; Strategy::COUNT];
        for (_, vs) in &lists {
            for v in vs {
                if let Variant::Strategy(r) = *v {
                    seen[r.bit()] = true;
                }
            }
        }
        let missing: Vec<&str> = STRATEGIES.iter().filter(|d| !seen[d.bit()]).map(|d| d.name()).collect(); // alloc: Program
        assert!(missing.is_empty(), "strategies no face declares: {missing:?}");
    }
}
