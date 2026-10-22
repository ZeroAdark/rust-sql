//! The join face (plan §3.1; H18 H25): the probe types, where a build comes from, the build side, co-sorted merges, chains
//! and drains. A probe is a step (join kinds, made by `frame::StepMaker`). It fuses scan, filter, probe and fold, stops
//! early, resumes mid-chain, and reads the payload by pointer with the inner row in registers. A build is an end
//! (`frame::EndMaker`) over a keyed table (keyed.rs `TableDecl`); a drain (H25, keyed.rs `PartEmit::Drain`) is its source.
//! Match semantics are PG's: NULL keys never match, and an outer row keeps its null-extended form. Duplicate build keys make
//! the probe an expanding step (step.rs `ExpandKind::Chain`).
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::Value;
use mcx::{PgBox, PgVec};

use crate::face::end::EndMech;
use crate::table::face::TableDecl;
use crate::table::keyimg::ImageWords;
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::px::Px;

crate::hook_variants! {
    /// The probe types (H18 a).
    "H18" pub enum Probe {
        Inner = Kind(JoinInner),
        Left = Kind(JoinLeft),
        /// Stops at the first match.
        Semi = Kind(JoinSemi),
        Anti = Kind(JoinAnti),
        /// RIGHT, RIGHT SEMI and RIGHT ANTI, by marks.
        Right = Kind(JoinRight),
        /// FULL: a left fill plus a right drain.
        Full = Kind(JoinFull),
    }
}

/// D4904: a SEMI / ANTI build its probe reads for keys alone keeps one record per key (strategies/dedup.rs `keys`); exec knobs
/// `PGRUST_GRAVITY_SEMI_KEYS` sets it once, where it registers the lowerer.
pub fn set_semi_keys(on: bool) {
    crate::ops::hashjoin::strategies::dedup::set_keys(on);
}

crate::strategies! {
    /// A keyed fold's output handed over as a build. No mechanism yet: its readers take the straight form.
    JOIN_EMIT_BUILD = 92, "join.emit_build", JoinChain;
    /// Key index over an invariant inner. No mechanism yet: its readers take the straight form.
    NEST_EQKEY = 94, "nest.eqkey", Nest;
    /// Adaptive bulk build of a hot inner. No mechanism yet: its readers take the straight form.
    NEST_BULK = 95, "nest.bulk", NestParam;
}

crate::strategies! {
    /// D4497 K1: a nest inner whose params are outputs of InitPlans fixed for the statement is kept, keyed or spooled
    /// (decompose self_spool.rs `fixed_only`). Decompose's mechanism, no lowering one: off, the nest keeps its rescans.
    NEST_KEEP_INITPLAN = 139, "nest.keep.initplan", Initplan;
    /// D4497 K2: a nest under a Subquery Scan is top level for the keep decision (decompose self_spool.rs `top_level`).
    /// Decompose's mechanism, no lowering one: off, the nest keeps its rescans.
    NEST_KEEP_SUBQUERY = 140, "nest.keep.subquery", Nest;
    /// D3227 K3: a SEMI or ANTI nest kept keyed, its probe stopping at a key's first record that passes the join qual
    /// (decompose self_spool.rs `keyed_kind`). Decompose's mechanism: off, the nest keeps its rescans.
    NEST_KEEP_SEMI = 143, "nest.keep.semi", Nest;
    /// D3227 K3: a keyed nest's inner read through a plan Material, and a nest under a Material filled once top level for
    /// the keep decision (decompose self_spool.rs `through_material`). Decompose's mechanism: off, the nest keeps its rescans.
    NEST_KEEP_MATERIAL = 144, "nest.keep.material", Nest;
}

crate::strategies! {
    /// D3227 (b): the keyed form's reach widened: an inner that may raise, keyed under the recheck licence where no row
    /// leaves the statement first (decompose self_spool.rs `raise_ahead`), and a nest under a hashed aggregate PostgreSQL
    /// fills once on another nest's inner side, top level for the keep decision (`agg_filled_once`). Decompose's
    /// mechanism: off, such a nest keeps the Nest form.
    NEST_KEY_EQ = 153, "nest.key.eq", Nest;
}

crate::hook_variants! {
    /// Where a build's rows come from (H18 b).
    "H18" pub enum BuildFrom {
        /// Any pipeline's rows.
        Pipeline = Kind(JoinInner),
        /// A keyed fold's output handed over as a build (D148).
        Breaker = Strategy(JOIN_EMIT_BUILD),
        /// A spooled param-free inner with an equality conjunct (the equality PG factors out of OR arms), indexed by key.
        SpooledInner = Strategy(NEST_EQKEY),
        /// psql's lookup nested loop (M3 decision 16): the pg_namespace inner read once into a keyed array, NULL-extended.
        CatalogLeft = Strategy(NEST_CATALOG_LEFT),
        /// An adaptive bulk build of a hot inner from an index-order scan.
        Bulk = Strategy(NEST_BULK),
        /// A kept inner whose params are fixed InitPlan outputs (D4497 K1).
        SpooledFixedParams = Strategy(NEST_KEEP_INITPLAN),
        /// A kept inner of a nest under a Subquery Scan (D4497 K2).
        SpooledUnderSubquery = Strategy(NEST_KEEP_SUBQUERY),
        /// A kept SEMI or ANTI nest's inner (D3227 K3).
        SpooledSemi = Strategy(NEST_KEEP_SEMI),
        /// A kept inner read through a plan Material (D3227 K3).
        SpooledThroughMaterial = Strategy(NEST_KEEP_MATERIAL),
        /// A raising inner keyed under its licence, or a nest's under a hashed aggregate filled once (D3227 (b)).
        SpooledUnderAgg = Strategy(NEST_KEY_EQ),
    }
}

crate::strategies! {
    /// Run-collapsed semi / anti on a sorted key. No mechanism yet: its readers take the straight form.
    SEMIJOIN_RUNS = 91, "semijoin.runs", SemiJoin;
}

crate::hook_variants! {
    /// Join roads over a probe (H18 a / c / d, and H06 for sorted keys).
    "H18 H06" pub enum JoinRoad {
        /// (a) several probes and nests chained on one outer pipeline, with mid-chain resume.
        Chain = Kind(JoinChain),
        /// (c) the smaller side is built, chosen from exact bind facts. Ruling 1: the row order may change; the multiset
        /// must match.
        BuildSide = Strategy(JOIN_BUILDSIDE),
        /// (d) a co-sorted merge when both sides declare the key order (H22).
        CoSorted = Kind(MergeJoin),
        /// The unmatched-row drain of a build.
        Drain = Kind(JoinDrain),
        /// H06: a semi / anti probe collapses runs of equal keys on a sorted key (the runs word, source.rs `WinConds`).
        SemiRuns = Strategy(SEMIJOIN_RUNS),
        /// D4821: copies of a build record laid once when the probe's end keeps one copy of a row.
        Dedup = Strategy(JOIN_DEDUP),
        /// join.payload_fp: a build's records carry a text payload column's Fp128 words for the group past the probe.
        PayloadFp = Strategy(JOIN_PAYLOAD_FP),
        /// join.drain.fp: a draining join's drain links each record's carried words, as its probe does.
        DrainFp = Strategy(JOIN_DRAIN_FP),
    }
}

/// A build as its probes see it: its table, the payload bytes each record carries after the image, and the breaker slot
/// that holds it.
#[derive(Clone, Copy, Debug)]
pub struct BuildRef {
    pub table: TableDecl,
    pub payload: u32,
    pub slot: u8,
}

/// A probe's match walk. It is registered as `ProbeImpl` beside each probe kind's step maker (one kind row per face type).
pub trait ProbeMech: Sync {
    /// Walks the records of `build` whose image equals `img` (hash `h`), handing each record's address to `each`. Returns the
    /// matched flag (`Bool`) that the outer types' null fill and the anti / semi exits read.
    fn walk(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, build: &BuildRef, img: &ImageWords, h: Value, each: &mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env, Value)) -> Value;
}

pub struct ProbeImpl(pub &'static dyn ProbeMech);

/// H18 c (join.buildside): an INNER hash join whose sides swap, the smaller side built from exact bind facts. Graph
/// pipeline `build` then makes graph `probe`'s source and its ops before `at` (the probe op) into the build; graph
/// `probe` makes `build`'s source and ops, the swapped probe, then its own ops after `at` and its end. Slots and lives
/// keep their graph order (`build` still produces before `probe` consumes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Swap {
    pub build: usize,
    pub probe: usize,
    pub at: usize,
}

/// The swaps a statement takes and their two made halves (`frame::resolve`); the makers of the moved source and ops run
/// under `Px::ident` of the program pipeline they now belong to, so their sites, words and protocols move with them.
pub trait SideMech: Sync {
    fn swaps<'g>(&self, px: &Px<'g, '_>) -> PgVec<'g, Swap>;
    fn make_build<'g>(&self, px: &mut Px<'g, '_>, s: &Swap) -> PgBox<'g, dyn EndMech + 'g>;
    fn make_probe<'g>(&self, px: &mut Px<'g, '_>, s: &Swap) -> PgBox<'g, dyn StepMech + 'g>;
}

pub struct SideImpl(pub &'static dyn SideMech);
