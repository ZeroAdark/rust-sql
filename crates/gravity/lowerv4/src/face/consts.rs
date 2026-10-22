//! The runtime-constant face (plan §3.1; H07 a–f). A runtime constant is a filter or zone operand whose value is set during
//! execution. The forms are listed in `RtConst` below.
//!
//! A constant lives in a frame word that its producer writes; a consumer reads it together with its armed flag. Filters are
//! armed and disarmed by measurement, never by planner selectivity (v1 negative result 4). A disarmed filter passes every
//! row, so arming changes cost, never rows.
//!
//! Consumers: verdicts (source.rs `VerdictKind::RtConst`), the block mask's operand (step.rs `BlockForm::RtOperand`, H01 d)
//! and probe-side filters.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;

use gravity_decompose::ops::shared::agg::GroupDecl;
use gravity_program::Pipeline;

use crate::frame::env::Env;
use crate::face::strategy::TOPK_WALK;
use crate::frame::px::Px;

crate::strategies! {
    /// Eager-safe InitPlan values feed verdicts. No mechanism yet: its readers take the straight form.
    NEST_EAGERSAFE = 97, "nest.eagersafe", Initplan;
    /// Skip-ahead on a sorted side. No mechanism yet: its readers take the straight form.
    MERGE_SKIP = 115, "merge.skip", Merge;
}

crate::hook_variants! {
    /// The runtime constants (H07 a–f).
    "H07" pub enum RtConst {
        /// (a) the top-k bound, as the run's first filter.
        Bound = Strategy(TOPK_BOUNDFIRST),
        /// (b) a group walk's n-th key.
        WalkKey = Strategy(TOPK_WALK),
        /// (c) a finished build's key set, range or bloom.
        BuildSet = Strategy(JOIN_RTFILTER),
        /// (c, D3245) every probe's build filter on one scan site, a granule skipped when any one rules it out; keys past
        /// the set cap as runs.
        BuildSets = Strategy(JOIN_RTFILTER_MULTI),
        /// (c, D4569) the build's filter on the scan of every UNION ALL arm the probe was copied into (an arm's marker only
        /// hands its rows on).
        BuildArms = Strategy(JOIN_RTFILTER_ARMS),
        /// (d) the driver's ordered claims, stopped at the proven bound.
        Claims = Strategy(TOPK_WALK),
        /// (e) PARAM_EXEC / InitPlan values. An InitPlan runs lazily at first read, as in PG (D237); only an eager-safe one
        /// feeds open-time verdicts.
        InitPlan = Strategy(NEST_EAGERSAFE),
        /// (f) hashed IN / NOT IN key sets (a hashed SubPlan), with PG's NULL rules for NOT IN.
        InSet = Kind(NestSubplan),
        /// A sorted side skips ahead to the other side's key (merge join).
        SkipAhead = Strategy(MERGE_SKIP),
    }
}

/// A runtime constant's shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtForm {
    /// One word, compared by the consumer's operator.
    Scalar,
    /// A `[lo, hi]` pair of words.
    Range,
    /// A key set, probed through the one KeyHash (keyed.rs `TableMech::find`).
    KeySet,
    /// A bloom over the key set's KeyHash words: `NoMatch` only.
    Bloom,
}

/// A runtime constant's words: the producer's value word and armed-flag word, each a byte offset into the frame words
/// (`Px::frame_word`, under its producer's `WordKey`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtSlot {
    pub what: RtConst,
    pub form: RtForm,
    pub value: i32,
    pub armed: i32,
}

impl RtSlot {
    /// `(armed, value)`. A consumer tests `value` only when `armed` is nonzero.
    pub fn read(&self, b: &mut FunctionBuilder<'_>, env: &Env) -> (Value, Value) {
        (env.load_word(b, self.armed), env.load_word(b, self.value))
    }
}

/// Road `topk.walk`'s mechanism (H07 b / d; runs/walk.rs, V4R-148): the claims of a walked pipeline in zone-minimum order,
/// stopped at the proven bound (`RtConst::Claims`). frame/program.rs `walk` hands it every made pipeline while the road
/// applies and is on; it declares the walk on `desc` and the program's protocols, or leaves both as they are.
pub trait WalkMech: Sync {
    /// Program pipeline `k`, its site `site` (the program's numbering) and its made descriptor `desc`.
    fn declare(&self, px: &mut Px<'_, '_>, k: usize, site: Option<usize>, desc: &mut Pipeline);
    /// The group walk `(n, attno)` that `declare` declares on program pipeline `k`, which builds group `g`, or `None`.
    fn group(&self, _px: &Px<'_, '_>, _k: usize, _g: &GroupDecl<'_>) -> Option<(u32, i16)> {
        None
    }
}

/// `topk.walk`'s row in a registry table.
pub struct WalkImpl(pub &'static dyn WalkMech);

/// Road topk.walk (H07 b / d, V4R-148): frame/program.rs hands every made pipeline over a driving site to the walk's
/// mechanism (runs/walk.rs) while the road applies and is on; it walks what a walk hint names, or nothing.
pub fn walk(px: &mut Px<'_, '_>, k: usize, site: Option<usize>, desc: &mut Pipeline) {
    if desc.driving_site < 0 || !(px.strategies.has(TOPK_WALK) && RtConst::Claims.on()) {
        return;
    }
    let node = px.gx.pipelines()[px.after_of(k)].node_id;
    RtConst::Claims.need::<WalkImpl>(&node).0.declare(px, k, site, desc);
}

/// V4R-177: the group walk `(n, attno)` that pipeline `k`'s keyed end folds under, gated as in `walk`. The end is made
/// before `walk` runs. The end's row skip (keyed/walk_skip.rs) is sound only under the cut this walk declares.
pub fn walk_group(px: &Px<'_, '_>, k: usize, g: &GroupDecl<'_>) -> Option<(u32, i16)> {
    if !(px.strategies.has(TOPK_WALK) && RtConst::Claims.on()) {
        return None;
    }
    RtConst::Claims.need::<WalkImpl>(&g.node_id).0.group(px, k, g)
}
