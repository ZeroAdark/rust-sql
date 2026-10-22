//! # batchreg — the batch-function registry (design doc §3a; formerly the
//! `lanereg` crate, folded into execexpr when the lane stack was deleted)
//!
//! ONE place that answers, for a function/operator OID, **which lane batch
//! tiers can execute it and under what admission constraints**. Before this
//! crate, that knowledge was scattered: `execexpr::CmpOp::for_fn_oid` (the AOT
//! qual-bitmap comparator set), `execexpr::jit`'s `inline_op` (the JIT
//! arithmetic set), `lanefold::classify_trans` (the aggregate-transition fold
//! set) — plus, on side branches, `lanestitch`'s stencil vocabulary — each kept
//! its **own** OID table, so coverage drifted: a stencil could exist with no
//! matching AOT census, a fold with no matching qual-tier comparator, an
//! arithmetic OID inlined by the JIT but unknown to the fold's affine
//! admission. This registry is the single source those consumers query, and
//! the origin of the coverage-drift report (`coverage_report`).
//!
//! ## What this is (and is NOT)
//!
//! It is a **static, const census** of admission — no lazy init, no
//! thread-locals (the repo bans lazy-init `thread_local!` on hot paths). Every
//! query is a scan of a `&'static [BatchFn]`, done at **plan / arm time**
//! (classify / ready), never per batch — so consumers' hot paths stay
//! zero-cost. It does NOT own the concrete kernel *implementations* or the
//! consumer *enums* (`execexpr::CmpOp` is a shared comparator vocabulary used
//! well beyond the lane — nbtree, hashjoin, grouping — and stays in execexpr).
//! The registry owns the **admission mapping**: OID → { tiers, shape,
//! constraints }. A consumer decodes the registry's neutral `Shape` into its
//! own kernel selector (`CmpOp`, `InlineOp`, `LaneKind`).
//!
//! ## The safety contract carried per tier (design §3a)
//!
//! Immutable / side-effect-free only; error discipline is one of
//! [`GuardTier`]; strict-NULL per-row masks honored by the consumer; collation
//! baked per call site ([`CollGate`]). The registry records the *coarse* tier
//! and its guard/collation class; the consumer still does the shape-specific
//! work (affine decomposition, guard-interval derivation, per-batch re-proof).
//!
//! ## Migration recipe — how a pending tier's table slots in
//!
//! Side branches each add coverage; their tables are represented here as
//! [`Avail::Pending`] rows (with the branch name) until they land. Landing
//! one is mechanical (`lane-v2-foldcov`, `lane-v2-textfold`,
//! `lane-v2-stitchwire` flipped to `InTree` on the third merge train,
//! 2026-07-12; `lane-v2-k2probe`/`lane-v2-projstitch` carried no pending
//! rows — their coverage is consumer-side):
//!   1. flip the affected [`TierCov`] rows from `Pending { branch }` to
//!      `InTree` (delete the `branch`);
//!   2. point the consumer at the registry (e.g. the new fold OIDs' consumer
//!      calls `covers(oid, Tier::Fold)` / decodes [`fold_desc`]);
//!   3. the conformance test in that consumer's crate (see
//!      `execexpr`/`lanefold` tests) now binds the live table to the registry —
//!      it fails if they disagree, so drift cannot re-open.
//! Adding a brand-new OID a branch introduces = add a [`BatchFn`] row here in
//! the same edit that adds the consumer arm.

#![allow(clippy::manual_range_contains)]

use ::types_core::Oid;

/// A batch-execution tier — a consumer that can run a function/operator over a
/// batch instead of the scalar fmgr.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    AotQualCmp,
    JitArith,
    Fold,
    FoldAffine,
    StitchCmp,
    StitchArith,
    StitchSaop,
    RowOp,
}

pub const ALL_TIERS: &[Tier] = &[
    Tier::AotQualCmp,
    Tier::JitArith,
    Tier::Fold,
    Tier::FoldAffine,
    Tier::StitchCmp,
    Tier::StitchArith,
    Tier::StitchSaop,
    Tier::RowOp,
];

impl Tier {
    pub const fn short(self) -> &'static str {
        match self {
            Tier::AotQualCmp => "aot-cmp",
            Tier::JitArith => "jit-arith",
            Tier::Fold => "fold",
            Tier::FoldAffine => "fold-affine",
            Tier::StitchCmp => "stitch-cmp",
            Tier::StitchArith => "stitch-arith",
            Tier::StitchSaop => "stitch-saop",
            Tier::RowOp => "rowop",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Avail {
    InTree,
    Pending { branch: &'static str },
    /// Coverage was evaluated and deliberately REFUSED: the tier cannot
    /// execute this OID with byte-identical C semantics under its current
    /// framework. `why` documents the blocking reason so the refusal is a
    /// recorded decision, not silent drift (censusgaps methodology).
    Refused { why: &'static str },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardTier {
    NonErroring,
    TypeProof,
    DataGuard,
    /// Batch runs unchecked; on any detected trap the rows 0..k replay per-row
    /// via fmgr so the error fires on C's row (JIT overflow branch).
    ReplayOnErr,
    /// The fold IS C's checked per-row op, applied one row at a time in row
    /// order (the fold-trans float tier): errors fire inline on C's exact
    /// row — no guard interval, no demote, no replay.
    ExactOp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollGate {
    NotApplicable,
    Deterministic,
}

#[derive(Clone, Copy, Debug)]
pub struct TierCov {
    pub tier: Tier,
    pub avail: Avail,
    pub guard: GuardTier,
    pub coll: CollGate,
}

impl TierCov {
    const fn intree(tier: Tier, guard: GuardTier, coll: CollGate) -> TierCov {
        TierCov { tier, avail: Avail::InTree, guard, coll }
    }
    #[allow(dead_code)]
    const fn pending(tier: Tier, branch: &'static str, guard: GuardTier, coll: CollGate) -> TierCov {
        TierCov { tier, avail: Avail::Pending { branch }, guard, coll }
    }
    const fn refused(tier: Tier, why: &'static str) -> TierCov {
        TierCov {
            tier,
            avail: Avail::Refused { why },
            guard: GuardTier::NonErroring, // vacuous: a refused tier never runs
            coll: CollGate::NotApplicable,
        }
    }
    pub fn is_intree(&self) -> bool {
        matches!(self.avail, Avail::InTree)
    }
    pub fn is_refused(&self) -> bool {
        matches!(self.avail, Avail::Refused { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpPred {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpWidth {
    I2,
    I4,
    I8,
    I24,
    I42,
    I84,
    I48,
    Oid,
    F4,
    F8,
    F48,
    F84,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CmpShape {
    pub width: CmpWidth,
    pub pred: CmpPred,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithKind {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithWidth {
    W24,
    W4,
    W8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArithShape {
    pub width: ArithWidth,
    pub op: ArithKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FoldKind {
    CountStar,
    CountAny,
    Sum,
    AvgAccum,
    Min,
    Max,
    Int128AvgAccum,
    FMin,
    FMax,
    BoolAnd,
    BoolOr,
    BitAnd,
    BitOr,
    FSum,
    FAccum,
    NumAccum,
    FRegrAccum,
    Count2,
}

#[derive(Clone, Copy, Debug)]
pub enum Shape {
    Cmp(CmpShape),
    Arith(ArithShape),
    Fold(FoldKind),
}

#[derive(Clone, Copy, Debug)]
pub struct BatchFn {
    pub oid: Oid,
    pub name: &'static str,
    pub shape: Shape,
    pub cov: &'static [TierCov],
}

impl BatchFn {
    pub fn tier(&self, tier: Tier) -> Option<&TierCov> {
        self.cov.iter().find(|c| c.tier == tier)
    }
}


use CmpPred::*;
use CmpWidth::*;

// The distinct coverage patterns, as named 'static slices (const promotion of
// an inline `&[..]` inside a const fn is not allowed, and naming them dedups).
const COV_AOT_CMP: &[TierCov] = &[
    TierCov::intree(Tier::AotQualCmp, GuardTier::NonErroring, CollGate::NotApplicable),
    TierCov::intree(Tier::StitchCmp, GuardTier::NonErroring, CollGate::NotApplicable),
];
const COV_AOT_CMP_SAOP: &[TierCov] = &[
    TierCov::intree(Tier::AotQualCmp, GuardTier::NonErroring, CollGate::NotApplicable),
    TierCov::intree(Tier::StitchCmp, GuardTier::NonErroring, CollGate::NotApplicable),
    TierCov::intree(Tier::StitchSaop, GuardTier::NonErroring, CollGate::NotApplicable),
];
const COV_ARITH_INT4: &[TierCov] = &[
    TierCov::intree(Tier::JitArith, GuardTier::ReplayOnErr, CollGate::NotApplicable),
    TierCov::intree(Tier::FoldAffine, GuardTier::DataGuard, CollGate::NotApplicable),
    TierCov::intree(Tier::StitchArith, GuardTier::ReplayOnErr, CollGate::NotApplicable),
];
// int8 pl/mi/mul: JIT inlines them (adds/subs/mul+smulh overflow checks with
// per-call replay), but the fold's affine admission is REFUSED (censusgaps):
// lanefold's proof machinery is int4-result based — coefficients are i32,
// `safe_interval` computes `i32::MIN/MAX - addend` exactly in i64, and the
// guard/zone intervals live in i64. An int8-result affine transform's exact
// safe interval (`i64::MIN - k`..`i64::MAX - k`) overflows i64 and needs i128
// interval arithmetic plus i64 coefficients (LaneTrans is size-bounded); no
// Int128 proof machinery exists in-tree. Without an exact interval the fold
// cannot promise C's overflow ereport fires at C's row, so int8 affine args
// stay on the checked per-row program.
const FOLDAFFINE_INT8_REFUSAL: &str =
    "int8 affine needs i128 interval proofs (safe_interval/guards are i64, coefficients i32); \
     without an exact interval the fold cannot reproduce C's int8 overflow ereport";
const COV_ARITH_INT8_JIT: &[TierCov] = &[
    TierCov::intree(Tier::JitArith, GuardTier::ReplayOnErr, CollGate::NotApplicable),
    TierCov::refused(Tier::FoldAffine, FOLDAFFINE_INT8_REFUSAL),
];
const COV_ARITH_MIX24: &[TierCov] = &[
    TierCov::intree(Tier::JitArith, GuardTier::ReplayOnErr, CollGate::NotApplicable),
    TierCov::intree(Tier::FoldAffine, GuardTier::DataGuard, CollGate::NotApplicable),
];
const COV_FOLD_IT: &[TierCov] =
    &[TierCov::intree(Tier::Fold, GuardTier::TypeProof, CollGate::NotApplicable)];

const fn cmp_aot(oid: Oid, name: &'static str, width: CmpWidth, pred: CmpPred) -> BatchFn {
    BatchFn { oid, name, shape: Shape::Cmp(CmpShape { width, pred }), cov: COV_AOT_CMP }
}

const fn cmp_aot_saop(oid: Oid, name: &'static str, width: CmpWidth, pred: CmpPred) -> BatchFn {
    BatchFn { oid, name, shape: Shape::Cmp(CmpShape { width, pred }), cov: COV_AOT_CMP_SAOP }
}


pub static ENTRIES: &[BatchFn] = &[
    cmp_aot_saop(65, "int4eq", I4, Eq),
    cmp_aot_saop(144, "int4ne", I4, Ne),
    cmp_aot_saop(66, "int4lt", I4, Lt),
    cmp_aot_saop(149, "int4le", I4, Le),
    cmp_aot_saop(147, "int4gt", I4, Gt),
    cmp_aot_saop(150, "int4ge", I4, Ge),
    cmp_aot_saop(467, "int8eq", I8, Eq),
    cmp_aot_saop(468, "int8ne", I8, Ne),
    cmp_aot_saop(469, "int8lt", I8, Lt),
    cmp_aot_saop(471, "int8le", I8, Le),
    cmp_aot_saop(470, "int8gt", I8, Gt),
    cmp_aot_saop(472, "int8ge", I8, Ge),
    cmp_aot_saop(63, "int2eq", I2, Eq),
    cmp_aot_saop(145, "int2ne", I2, Ne),
    cmp_aot_saop(64, "int2lt", I2, Lt),
    cmp_aot_saop(148, "int2le", I2, Le),
    cmp_aot_saop(146, "int2gt", I2, Gt),
    cmp_aot_saop(151, "int2ge", I2, Ge),
    cmp_aot_saop(474, "int84eq", I84, Eq),
    cmp_aot_saop(475, "int84ne", I84, Ne),
    cmp_aot_saop(476, "int84lt", I84, Lt),
    cmp_aot_saop(478, "int84le", I84, Le),
    cmp_aot_saop(477, "int84gt", I84, Gt),
    cmp_aot_saop(479, "int84ge", I84, Ge),
    cmp_aot_saop(852, "int48eq", I48, Eq),
    cmp_aot_saop(853, "int48ne", I48, Ne),
    cmp_aot_saop(854, "int48lt", I48, Lt),
    cmp_aot_saop(856, "int48le", I48, Le),
    cmp_aot_saop(855, "int48gt", I48, Gt),
    cmp_aot_saop(857, "int48ge", I48, Ge),
    cmp_aot_saop(158, "int24eq", I24, Eq),
    cmp_aot_saop(164, "int24ne", I24, Ne),
    cmp_aot_saop(160, "int24lt", I24, Lt),
    cmp_aot_saop(166, "int24le", I24, Le),
    cmp_aot_saop(162, "int24gt", I24, Gt),
    cmp_aot_saop(168, "int24ge", I24, Ge),
    cmp_aot_saop(159, "int42eq", I42, Eq),
    cmp_aot_saop(165, "int42ne", I42, Ne),
    cmp_aot_saop(161, "int42lt", I42, Lt),
    cmp_aot_saop(167, "int42le", I42, Le),
    cmp_aot_saop(163, "int42gt", I42, Gt),
    cmp_aot_saop(169, "int42ge", I42, Ge),
    cmp_aot_saop(184, "oideq", Oid, Eq),
    cmp_aot_saop(185, "oidne", Oid, Ne),
    cmp_aot_saop(716, "oidlt", Oid, Lt),
    cmp_aot_saop(717, "oidle", Oid, Le),
    cmp_aot_saop(1638, "oidgt", Oid, Gt),
    cmp_aot_saop(1639, "oidge", Oid, Ge),
    cmp_aot(287, "float4eq", F4, Eq),
    cmp_aot(288, "float4ne", F4, Ne),
    cmp_aot(289, "float4lt", F4, Lt),
    cmp_aot(290, "float4le", F4, Le),
    cmp_aot(291, "float4gt", F4, Gt),
    cmp_aot(292, "float4ge", F4, Ge),
    cmp_aot(293, "float8eq", F8, Eq),
    cmp_aot(294, "float8ne", F8, Ne),
    cmp_aot(295, "float8lt", F8, Lt),
    cmp_aot(296, "float8le", F8, Le),
    cmp_aot(297, "float8gt", F8, Gt),
    cmp_aot(298, "float8ge", F8, Ge),
    cmp_aot(299, "float48eq", F48, Eq),
    cmp_aot(300, "float48ne", F48, Ne),
    cmp_aot(301, "float48lt", F48, Lt),
    cmp_aot(302, "float48le", F48, Le),
    cmp_aot(303, "float48gt", F48, Gt),
    cmp_aot(304, "float48ge", F48, Ge),
    cmp_aot(305, "float84eq", F84, Eq),
    cmp_aot(306, "float84ne", F84, Ne),
    cmp_aot(307, "float84lt", F84, Lt),
    cmp_aot(308, "float84le", F84, Le),
    cmp_aot(309, "float84gt", F84, Gt),
    cmp_aot(310, "float84ge", F84, Ge),
    cmp_aot_saop(1086, "date_eq", I4, Eq),
    cmp_aot_saop(1091, "date_ne", I4, Ne),
    cmp_aot_saop(1087, "date_lt", I4, Lt),
    cmp_aot_saop(1088, "date_le", I4, Le),
    cmp_aot_saop(1089, "date_gt", I4, Gt),
    cmp_aot_saop(1090, "date_ge", I4, Ge),
    cmp_aot_saop(2052, "timestamp_eq", I8, Eq),
    cmp_aot_saop(2053, "timestamp_ne", I8, Ne),
    cmp_aot_saop(2054, "timestamp_lt", I8, Lt),
    cmp_aot_saop(2055, "timestamp_le", I8, Le),
    cmp_aot_saop(2057, "timestamp_gt", I8, Gt),
    cmp_aot_saop(2056, "timestamp_ge", I8, Ge),
    cmp_aot_saop(1152, "timestamptz_eq", I8, Eq),
    cmp_aot_saop(1153, "timestamptz_ne", I8, Ne),
    cmp_aot_saop(1154, "timestamptz_lt", I8, Lt),
    cmp_aot_saop(1155, "timestamptz_le", I8, Le),
    cmp_aot_saop(1157, "timestamptz_gt", I8, Gt),
    cmp_aot_saop(1156, "timestamptz_ge", I8, Ge),
    arith(177, "int4pl", ArithWidth::W4, ArithKind::Add, COV_ARITH_INT4),
    arith(181, "int4mi", ArithWidth::W4, ArithKind::Sub, COV_ARITH_INT4),
    arith(141, "int4mul", ArithWidth::W4, ArithKind::Mul, COV_ARITH_INT4),
    arith(463, "int8pl", ArithWidth::W8, ArithKind::Add, COV_ARITH_INT8_JIT),
    arith(464, "int8mi", ArithWidth::W8, ArithKind::Sub, COV_ARITH_INT8_JIT),
    arith(465, "int8mul", ArithWidth::W8, ArithKind::Mul, COV_ARITH_INT8_JIT),
    fold_affine(178, "int24pl", ArithKind::Add),
    fold_affine(179, "int42pl", ArithKind::Add),
    fold_affine(182, "int24mi", ArithKind::Sub),
    fold_affine(183, "int42mi", ArithKind::Sub),
    fold_affine(170, "int24mul", ArithKind::Mul),
    fold_affine(171, "int42mul", ArithKind::Mul),
    fold_affine(172, "int24div", ArithKind::Div),
    fold_it(1219, "int8inc", FoldKind::CountStar),
    fold_it(2804, "int8inc_any", FoldKind::CountAny),
    fold_it(1840, "int2_sum", FoldKind::Sum),
    fold_it(1841, "int4_sum", FoldKind::Sum),
    fold_it(1962, "int2_avg_accum", FoldKind::AvgAccum),
    fold_it(1963, "int4_avg_accum", FoldKind::AvgAccum),
    fold_it(2746, "int8_avg_accum", FoldKind::Int128AvgAccum),
    fold_it(768, "int4larger", FoldKind::Max),
    fold_it(769, "int4smaller", FoldKind::Min),
    fold_it(770, "int2larger", FoldKind::Max),
    fold_it(771, "int2smaller", FoldKind::Min),
    fold_it(1236, "int8larger", FoldKind::Max),
    fold_it(1237, "int8smaller", FoldKind::Min),
    fold_it(1138, "date_larger", FoldKind::Max),
    fold_it(1139, "date_smaller", FoldKind::Min),
    fold_it(2036, "timestamp_larger", FoldKind::Max),
    fold_it(2035, "timestamp_smaller", FoldKind::Min),
    fold_it(1196, "timestamptz_larger", FoldKind::Max),
    fold_it(1195, "timestamptz_smaller", FoldKind::Min),
    fold_cov2(209, "float4larger", FoldKind::FMax),
    fold_cov2(211, "float4smaller", FoldKind::FMin),
    fold_cov2(223, "float8larger", FoldKind::FMax),
    fold_cov2(224, "float8smaller", FoldKind::FMin),
    fold_cov2(2515, "booland_statefunc", FoldKind::BoolAnd),
    fold_cov2(2516, "boolor_statefunc", FoldKind::BoolOr),
    fold_cov2(1892, "int2and", FoldKind::BitAnd),
    fold_cov2(1893, "int2or", FoldKind::BitOr),
    fold_cov2(1898, "int4and", FoldKind::BitAnd),
    fold_cov2(1899, "int4or", FoldKind::BitOr),
    fold_cov2(1904, "int8and", FoldKind::BitAnd),
    fold_cov2(1905, "int8or", FoldKind::BitOr),
    fold_str(458, "text_larger", FoldKind::Max),
    fold_str(459, "text_smaller", FoldKind::Min),
    fold_str(1063, "bpchar_larger", FoldKind::Max),
    fold_str(1064, "bpchar_smaller", FoldKind::Min),
    fold_trans(204, "float4pl", FoldKind::FSum),
    fold_trans(218, "float8pl", FoldKind::FSum),
    fold_trans(208, "float4_accum", FoldKind::FAccum),
    fold_trans(222, "float8_accum", FoldKind::FAccum),
    fold_trans(2858, "numeric_avg_accum", FoldKind::NumAccum),
    fold_trans(2806, "float8_regr_accum", FoldKind::FRegrAccum),
    fold_trans(2805, "int8inc_float8_float8", FoldKind::Count2),
];

const fn arith(oid: Oid, name: &'static str, width: ArithWidth, op: ArithKind, cov: &'static [TierCov]) -> BatchFn {
    BatchFn { oid, name, shape: Shape::Arith(ArithShape { width, op }), cov }
}

const fn fold_affine(oid: Oid, name: &'static str, op: ArithKind) -> BatchFn {
    BatchFn { oid, name, shape: Shape::Arith(ArithShape { width: ArithWidth::W24, op }), cov: COV_ARITH_MIX24 }
}

const fn fold_it(oid: Oid, name: &'static str, kind: FoldKind) -> BatchFn {
    BatchFn { oid, name, shape: Shape::Fold(kind), cov: COV_FOLD_IT }
}

const COV_FOLD_FOLDCOV: &[TierCov] =
    &[TierCov::intree(Tier::Fold, GuardTier::TypeProof, CollGate::NotApplicable)];
const COV_FOLD_TEXTFOLD: &[TierCov] =
    &[TierCov::intree(Tier::Fold, GuardTier::NonErroring, CollGate::Deterministic)];

const fn fold_cov2(oid: Oid, name: &'static str, kind: FoldKind) -> BatchFn {
    BatchFn { oid, name, shape: Shape::Fold(kind), cov: COV_FOLD_FOLDCOV }
}

const fn fold_str(oid: Oid, name: &'static str, kind: FoldKind) -> BatchFn {
    BatchFn { oid, name, shape: Shape::Fold(kind), cov: COV_FOLD_TEXTFOLD }
}

// fold-trans tier (lane-v2-lanefold-trans, knob-gated default OFF):
// ORDER-PRESERVING float folds. The kernel applies C's exact checked per-row
// op in row order (no reassociation), so overflow ereports fire inline on
// C's row — no guard interval, no demote, no replay.
const COV_FOLD_TRANS: &[TierCov] =
    &[TierCov::intree(Tier::Fold, GuardTier::ExactOp, CollGate::NotApplicable)];

const fn fold_trans(oid: Oid, name: &'static str, kind: FoldKind) -> BatchFn {
    BatchFn { oid, name, shape: Shape::Fold(kind), cov: COV_FOLD_TRANS }
}

// ---------------------------------------------------------------------------
// Query API — all plan/arm-time; linear scans of a small static table.
// ---------------------------------------------------------------------------

pub fn entry(oid: Oid) -> Option<&'static BatchFn> {
    ENTRIES.iter().find(|e| e.oid == oid)
}

pub fn covers(oid: Oid, tier: Tier) -> bool {
    entry(oid).and_then(|e| e.tier(tier)).is_some_and(|c| c.is_intree())
}

pub fn covers_pending(oid: Oid, tier: Tier) -> bool {
    entry(oid).and_then(|e| e.tier(tier)).is_some_and(|c| !c.is_refused())
}

pub fn aot_qual_cmp(oid: Oid) -> Option<CmpShape> {
    let e = entry(oid)?;
    e.tier(Tier::AotQualCmp).filter(|c| c.is_intree())?;
    match e.shape {
        Shape::Cmp(s) => Some(s),
        _ => None,
    }
}

pub fn jit_arith(oid: Oid) -> Option<ArithShape> {
    let e = entry(oid)?;
    e.tier(Tier::JitArith).filter(|c| c.is_intree())?;
    match e.shape {
        Shape::Arith(s) => Some(s),
        _ => None,
    }
}

pub fn fold_desc(oid: Oid) -> Option<FoldKind> {
    let e = entry(oid)?;
    e.tier(Tier::Fold).filter(|c| c.is_intree())?;
    match e.shape {
        Shape::Fold(k) => Some(k),
        _ => None,
    }
}


#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drift {
    StencilNoCensus,
    FoldAffineNoJit,
    JitNoFoldAffine,
    PendingOnly,
}

impl Drift {
    pub fn label(self) -> &'static str {
        match self {
            Drift::StencilNoCensus => "stencil-but-no-census",
            Drift::FoldAffineNoJit => "fold-affine-but-no-jit",
            Drift::JitNoFoldAffine => "jit-but-no-fold-affine",
            Drift::PendingOnly => "pending-only",
        }
    }
}

pub fn drift_of(e: &BatchFn) -> Vec<Drift> {
    let mut out = Vec::new();
    let has = |t: Tier| e.tier(t).is_some_and(|c| !c.is_refused());
    let documented = |t: Tier| e.tier(t).is_some_and(|c| c.is_refused());
    let has_it = |t: Tier| e.tier(t).is_some_and(|c| c.is_intree());
    if has(Tier::StitchCmp) && !has_it(Tier::AotQualCmp) {
        out.push(Drift::StencilNoCensus);
    }
    if has_it(Tier::FoldAffine) && !has_it(Tier::JitArith) && !documented(Tier::JitArith) {
        out.push(Drift::FoldAffineNoJit);
    }
    if has_it(Tier::JitArith) && !has_it(Tier::FoldAffine) && !documented(Tier::FoldAffine) {
        out.push(Drift::JitNoFoldAffine);
    }
    if !e.cov.iter().any(|c| c.is_intree()) {
        out.push(Drift::PendingOnly);
    }
    out
}

fn cell(e: &BatchFn, t: Tier) -> &'static str {
    match e.tier(t) {
        None => "-",
        Some(c) if c.is_intree() => "IN",
        Some(c) if c.is_refused() => "RF",
        Some(_) => "..",
    }
}

pub fn coverage_report() -> String {
    let mut s = String::new();
    s.push_str("# Lane batch-function registry — coverage-drift report\n\n");
    s.push_str("Generated from `lanereg::ENTRIES` (`lanereg::coverage_report`).\n");
    s.push_str("`IN` = in-tree, `..` = pending on a side branch, `RF` = documented refusal\n");
    s.push_str("(see the refusals section), `-` = not covered.\n\n");
    s.push_str("| OID | name | aot-cmp | jit-arith | fold | fold-affine | stitch-cmp | stitch-arith | stitch-saop | drift |\n");
    s.push_str("|----:|------|:-------:|:---------:|:----:|:-----------:|:----------:|:------------:|:-----------:|-------|\n");
    for e in ENTRIES {
        let mut drift = String::new();
        for (i, d) in drift_of(e).iter().enumerate() {
            if i > 0 {
                drift.push_str(", ");
            }
            drift.push_str(d.label());
        }
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            e.oid,
            e.name,
            cell(e, Tier::AotQualCmp),
            cell(e, Tier::JitArith),
            cell(e, Tier::Fold),
            cell(e, Tier::FoldAffine),
            cell(e, Tier::StitchCmp),
            cell(e, Tier::StitchArith),
            cell(e, Tier::StitchSaop),
            drift,
        ));
    }
    s.push_str("\n## Summary\n\n");
    let total = ENTRIES.len();
    let count = |d: Drift| ENTRIES.iter().filter(|e| drift_of(e).contains(&d)).count();
    s.push_str(&format!("- registered OIDs: {total}\n"));
    s.push_str(&format!(
        "- {}: {} (stitch comparator stencils with no AOT qual census)\n",
        Drift::StencilNoCensus.label(),
        count(Drift::StencilNoCensus)
    ));
    s.push_str(&format!(
        "- {}: {} (fold affine ops the JIT does not inline)\n",
        Drift::FoldAffineNoJit.label(),
        count(Drift::FoldAffineNoJit)
    ));
    s.push_str(&format!(
        "- {}: {} (JIT-inlined arith unknown to the fold affine admission)\n",
        Drift::JitNoFoldAffine.label(),
        count(Drift::JitNoFoldAffine)
    ));
    s.push_str(&format!(
        "- {}: {} (coverage only on a side branch)\n",
        Drift::PendingOnly.label(),
        count(Drift::PendingOnly)
    ));
    let refusals: Vec<(&BatchFn, Tier, &'static str)> = ENTRIES
        .iter()
        .flat_map(|e| {
            e.cov.iter().filter_map(move |c| match c.avail {
                Avail::Refused { why } => Some((e, c.tier, why)),
                _ => None,
            })
        })
        .collect();
    if !refusals.is_empty() {
        s.push_str("\n## Documented refusals\n\n");
        s.push_str(
            "Tier admissions evaluated and deliberately refused — the tier cannot\n\
             reproduce byte-identical C semantics under its current framework.\n\n",
        );
        for (e, t, why) in refusals {
            s.push_str(&format!("- {} `{}` × {}: {}\n", e.oid, e.name, t.short(), why));
        }
    }
    s
}

#[cfg(test)]
mod tests;
