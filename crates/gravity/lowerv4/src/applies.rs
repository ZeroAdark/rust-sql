//! Road applicability (plan §3.3, ruling 6): one predicate per road over the kinds a graph needs and the properties
//! `props.rs` computes, mirroring `docs/gravity/lowerv4/tools/v4plan.py` `pred()` term for term. Closed at M0 and frozen by
//! the parity test (the census through both, `tests/m0_faces.rs`); narrowing a predicate later is an extension reopen.
//! Admission (R-A) is here too: the one place a statement refuses because a kind or an applicable road is not built.
use crate::face::kinds::{KindId as K, KindSet};
use crate::face::strategy::{self, Strategy, StrategySet, STRATEGIES, WRITE_ROUTE};
use crate::graph::Gx;
use crate::props::Props;
use crate::admit::refusal::Variant;
use crate::ops as registry;

macro_rules! tokens {
    ($(#[$m:meta])* $ty:ident { $($v:ident $name:literal),* $(,)? }) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[repr(u8)]
        pub enum $ty { $($v),* }

        impl $ty {
            pub const ALL: &'static [$ty] = &[$($ty::$v),*];

            /// The name v4plan.py uses.
            pub const fn name(self) -> &'static str {
                match self { $($ty::$v => $name),* }
            }

            pub fn from_name(s: &str) -> Option<$ty> {
                $ty::ALL.iter().copied().find(|t| t.name() == s)
            }
        }
    };
}

tokens! {
    /// A graph property a predicate reads: v4plan.py `classify()`'s `P` tokens (`props.rs` computes them from the graph).
    Prop {
        Seq "seq", Pgrc "pgrc", Heap "heap", Filter "filter", Like "like", TextEq "text_eq", Datetime "datetime",
        Length "length", Having "having", Case "case", MinmaxExpr "minmax_expr",
        OrderExpr "order_expr", DistinctOn "distinct_on", TopkWide "topk_wide", CountOrder "count_order", Jsonb "jsonb",
        ExprKey "expr_key", ModKey "mod_key", Code "code", Packed "packed", Walk "walk", FpSortkey "fp_sortkey",
        Hosted "hosted", Numeric "numeric", Count "count", Truncated "truncated", ValuesOnly "values_only",
        MultiTarget "multi_target", CatalogNest "catalog_nest", LaneKey "lane_key", Route "route", RowTrigger "row_trigger",
    }
}

tokens! {
    /// A road's applicability predicate (`model.py` ROADS' predicate column).
    Applies {
        All "all", Append "append", CatalogNest "catalog_nest", Cte "cte", Code "code", CodeCaseKey "code_case_key", CodeExpr "code_expr",
        CodeFilter "code_filter", CodeFilterTopk "code_filter_topk", CodeKeyed "code_keyed", CodeKeyedTopk "code_keyed_topk", CodeKeyedPgrc "code_keyed_pgrc", CodeLength "code_length",
        CodeTopk "code_topk", CountTopk "count_topk", DatetimeExpr "datetime_expr", Distinct "distinct",
        DistinctOn "distinct_on", DistinctPlain "distinct_plain", Drain "drain", ExprKey "expr_key",
        ExprOrder "expr_order", Fold "fold", FoldScan "fold_scan", HavingKeyed "having_keyed", Heap "heap",
        HeapLike "heap_like", HeapTextFilter "heap_text_filter", Initplan "initplan", Insert "insert",
        InsertUpdate "insert_update", Join "join", JoinChain "join_chain",
        JoinFold "join_fold", JoinFoldVarlena "join_fold_varlena", Jsonb "jsonb", JsonbPgrc "jsonb_pgrc",
        Keyed "keyed", Loop "loop", Merge "merge", Nest "nest", NestFold "nest_fold",
        NestParam "nest_param", NumAny "num_any", NumHeap "num_heap", NumPacked "num_packed", Pgrc "pgrc",
        PgrcFilter "pgrc_filter", PgrcJoinOrBound "pgrc_join_or_bound", PgrcLike "pgrc_like",
        PgrcPlain "pgrc_plain", PgrcPlainFilter "pgrc_plain_filter", PgrcPlainNum "pgrc_plain_num",
        RightJoin "right_join", RowsOut "rows_out", Scan "scan", ScanFilter "scan_filter",
        SemiJoin "semi_join", Setop "setop", SetopAny "setop_any", Sort "sort", SortKeylane "sort_keylane", SpoolSort "spool_sort", Srf "srf",
        SrfLimit "srf_limit", Stats "stats", Subplan "subplan", Table "table", TextFilter "text_filter", TopkRows "topk_rows", TopkWide "topk_wide",
        Twopass "twopass", Update "update", VarlenaKey "varlena_key", VarlenaKeyCanon "varlena_key_canon", VarlenaKeyHeap "varlena_key_heap", Walkable "walkable",
        Window "window", Write "write",
    }
}

/// A set of properties: one bit per `Prop`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PropSet(pub u64);

impl PropSet {
    pub const fn has(self, p: Prop) -> bool {
        self.0 & (1u64 << p as u32) != 0
    }

    pub const fn any(self, ps: &[Prop]) -> bool {
        let mut i = 0;
        while i < ps.len() {
            if self.has(ps[i]) {
                return true;
            }
            i += 1;
        }
        false
    }

    pub fn add(&mut self, p: Prop) {
        self.0 |= 1u64 << p as u32;
    }

    pub fn iter(self) -> impl Iterator<Item = Prop> {
        Prop::ALL.iter().copied().filter(move |p| self.has(*p))
    }
}

const KEYED: KindSet = KindSet::of(&[K::KeyedHashagg, K::KeyedDistinctGrouped, K::KeyedDistinctPlain, K::KeyedGsets, K::KeyedTwopass]);
const NEST: KindSet = KindSet::of(&[K::NestLoop, K::NestSpool, K::NestParam, K::NestMemo, K::NestSubplan, K::NestInitplan, K::NestRerun, K::NestCte]);
/// The nest kinds that may run a pipeline more than once per execution (every `nest.*` but an initplan and a CTE).
const NEST_RESCAN: KindSet = KindSet::of(&[K::NestLoop, K::NestSpool, K::NestParam, K::NestMemo, K::NestSubplan, K::NestRerun]);
/// Every `src.*` kind but `src.result` and `src.values`.
const SRCS: KindSet = KindSet::of(&[K::SrcPgrc, K::SrcIndex, K::SrcHeap, K::SrcIndexOnly, K::SrcIndexArray, K::SrcIndexAm, K::SrcBitmap, K::SrcBitmapTree, K::SrcTid, K::SrcSample, K::SrcAppend, K::SrcPrune]);
const WRITE: KindSet = KindSet::of(&[K::WriteDelete, K::WriteInsert, K::WriteUpdate, K::WriteMerge, K::WriteReturning, K::WriteOnconflict, K::WriteLock, K::WriteTransition]);
const RUNS_WINDOW: KindSet = KindSet::of(&[K::RunsTopkGroups, K::RunsSort, K::RunsTopkRows, K::RunsUnique, K::RunsGroup, K::RunsIncremental, K::RunsMergeappend, K::WindowAgg]);
const DISTINCT: KindSet = KindSet::of(&[K::KeyedDistinctPlain, K::KeyedDistinctGrouped]);
/// The kinds that sort rows (V4R-127: a Group / Unique / sorted aggregate reads an order and sorts nothing; its input's
/// Sort, when it has one, is runs.sort's own kind).
const SORT: KindSet = KindSet::of(&[K::RunsSort, K::RunsTopkRows, K::RunsIncremental]);
const VARLENA_KEY: KindSet = KindSet::of(&[K::RepVarlenaHash, K::RepVarlenaOrder]);
const RIGHT_JOIN: KindSet = KindSet::of(&[K::JoinRight, K::JoinFull]);
const SEMI_JOIN: KindSet = KindSet::of(&[K::JoinSemi, K::JoinAnti]);
const SRF: KindSet = KindSet::of(&[K::SrfFunction, K::SrfPl, K::SrfProjectset]);
const INSERT_UPDATE: KindSet = KindSet::of(&[K::WriteInsert, K::WriteUpdate]);

/// `v4plan.py pred(name, K, P)`: whether the road predicate `a` holds for kinds `k` and properties `p`.
pub fn holds(a: Applies, k: KindSet, p: PropSet) -> bool {
    use Prop as P;
    let pg = k.has(K::SrcPgrc);
    let seq = p.has(P::Seq);
    let keyed = k.any(KEYED);
    let fold = k.has(K::FoldPlain);
    let join = k.has(K::JoinInner);
    let nest = k.any(NEST);
    let srcs = k.any(SRCS);
    let code_keyed = k.has(K::RepCodeHash);
    // D2980: an initplan or a CTE runs its plan once per execution, so a plain fold inside it (a scalar subquery's
    // count) takes the answer roads as at the top; their own per-pipeline tests (answer_site.rs, answer_counted.rs) still
    // decide each fold. A rescanned nest (a correlated subplan, a parameterized loop) keeps them off.
    let plain = pg && fold && !join && !k.any(NEST_RESCAN);
    match a {
        Applies::ScanFilter => seq && p.has(P::Filter),
        Applies::FoldScan => fold && seq,
        Applies::Pgrc => pg,
        Applies::Scan => srcs,
        Applies::Loop | Applies::All => true,
        Applies::Fold => fold || keyed,
        Applies::PgrcFilter => pg && p.has(P::Filter),
        Applies::PgrcLike => pg && p.has(P::Like),
        Applies::CodeFilter => pg && p.has(P::Code) && p.has(P::Filter),
        Applies::PgrcJoinOrBound => pg && (join || k.has(K::RunsTopkRows)),
        Applies::PgrcPlain => plain,
        Applies::PgrcPlainFilter => plain && p.has(P::Filter),
        Applies::PgrcPlainNum => plain && k.has(K::ClassNumPacked),
        Applies::CodeExpr => p.has(P::Code) && p.any(&[P::Like, P::Hosted, P::ExprKey, P::Case]),
        Applies::CodeLength => p.has(P::Code) && p.has(P::Length),
        Applies::CodeKeyed => code_keyed,
        Applies::Code => p.has(P::Code),
        Applies::NumPacked => k.has(K::ClassNumPacked) || p.has(P::Packed),
        Applies::NumAny => p.has(P::Numeric),
        Applies::NumHeap => p.has(P::Heap) && p.has(P::Numeric),
        Applies::Write => k.any(WRITE),
        Applies::RowsOut => seq && !(fold || keyed || join || nest || k.any(RUNS_WINDOW)),
        Applies::Keyed => keyed,
        Applies::Table => keyed || join,
        Applies::Distinct => k.any(DISTINCT),
        Applies::DistinctPlain => k.has(K::KeyedDistinctPlain),
        Applies::Twopass => k.has(K::KeyedTwopass) || (k.has(K::KeyedHashagg) && p.has(P::Having)),
        Applies::HavingKeyed => k.has(K::KeyedHashagg) && p.has(P::Having),
        Applies::CodeKeyedPgrc => code_keyed && pg,
        Applies::Join => join,
        Applies::JoinFold => join && (fold || keyed),
        Applies::JoinFoldVarlena => join && (fold || keyed) && k.any(VARLENA_KEY),
        Applies::Heap => k.has(K::SrcHeap),
        Applies::HeapLike => k.has(K::SrcHeap) && p.has(P::Like),
        Applies::HeapTextFilter => k.has(K::SrcHeap) && p.has(P::TextEq),
        // textkeys step 2 (road text.set): a text equality / IN list over any source (a byte set's site)
        Applies::TextFilter => p.has(P::TextEq),
        Applies::DatetimeExpr => p.has(P::Datetime),
        Applies::Append => k.has(K::SrcAppend),
        Applies::CountTopk => k.has(K::RunsTopkGroups) && p.has(P::CountOrder),
        Applies::CodeFilterTopk => pg && p.has(P::Code) && p.has(P::Filter) && k.has(K::RunsTopkGroups) && p.has(P::CountOrder),
        Applies::CodeKeyedTopk => code_keyed && k.has(K::RunsTopkGroups) && p.has(P::CountOrder),
        Applies::TopkRows => k.has(K::RunsTopkRows),
        Applies::Walkable => p.has(P::Walk),
        Applies::TopkWide => k.has(K::RunsTopkRows) && p.has(P::TopkWide),
        Applies::Sort => k.any(SORT),
        Applies::SortKeylane => k.any(KindSet::of(&[K::RunsSort, K::RunsIncremental])) && p.has(P::LaneKey),
        Applies::DistinctOn => p.has(P::DistinctOn),
        Applies::ExprKey => p.has(P::ModKey),
        Applies::ExprOrder => p.any(&[P::MinmaxExpr, P::OrderExpr]),
        Applies::CodeCaseKey => p.has(P::Code) && p.has(P::ExprKey) && p.has(P::Case),
        // D3007: code.cut's group form cuts a value-set group's entries under a top over its key (runs.topk_groups)
        Applies::CodeTopk => p.has(P::Code) && p.has(P::FpSortkey) && (k.has(K::RunsTopkRows) || k.has(K::RunsTopkGroups)),
        Applies::VarlenaKey => k.any(VARLENA_KEY),
        // key.tagged engages only where no key meets a dictionary's Fp128 (no pgrc source, no code key)
        Applies::VarlenaKeyHeap => k.any(VARLENA_KEY) && !pg && !code_keyed,
        // textkeys step 3 (key.canon): a varlena key, unless a group key is an expression where code lanes are read (the
        // per-code key forms, percode_key / key.codeif / countwalk, image an expression's results by their own tables)
        Applies::VarlenaKeyCanon => k.any(VARLENA_KEY) && !(p.has(P::Code) && p.has(P::ExprKey)),
        Applies::JsonbPgrc => pg && p.has(P::Jsonb),
        Applies::Jsonb => p.has(P::Jsonb) && srcs,
        Applies::Drain => k.has(K::JoinDrain),
        Applies::RightJoin => k.any(RIGHT_JOIN),
        Applies::SemiJoin => k.any(SEMI_JOIN),
        Applies::JoinChain => k.has(K::JoinChain),
        Applies::Nest => k.has(K::NestLoop) && !p.has(P::CatalogNest), // psql's lookup loop: nest.catalog_left's (M3 D16)
        Applies::CatalogNest => p.has(P::CatalogNest),
        Applies::Cte => k.has(K::NestCte),
        Applies::NestParam => k.has(K::NestParam),
        Applies::Subplan => k.has(K::NestSubplan),
        Applies::Initplan => k.has(K::NestInitplan),
        Applies::NestFold => k.has(K::NestLoop) && (fold || keyed),
        Applies::SpoolSort => k.has(K::NestSpool) && k.has(K::RunsSort),
        Applies::Srf => k.any(SRF),
        Applies::SrfLimit => k.any(SRF) && k.has(K::StepLimit),
        // D4565: agg.stat_reg, a var / stddev call (class.stats)
        Applies::Stats => k.has(K::ClassStats),
        Applies::Setop => k.has(K::SetopSetop) && p.has(P::Count),
        Applies::SetopAny => k.has(K::SetopSetop),
        Applies::Window => k.has(K::WindowAgg),
        Applies::Merge => k.has(K::MergeJoin),
        Applies::Update => k.has(K::WriteUpdate),
        Applies::InsertUpdate => k.any(INSERT_UPDATE),
        Applies::Insert => k.has(K::WriteInsert),
    }
}

/// V4R-5: roads a revision widened past their closed roads.rs predicate: (road, the extra token that also makes it
/// apply). write.route's column stays `insert`, which write.literals shares.
pub const WIDENED: &[(Strategy, Prop)] = &[(WRITE_ROUTE, Prop::MultiTarget)];

/// V4R-204 (M4-plan Decision 2, amended by main 09-25): roads a revision narrowed past their closed roads.rs predicate:
/// (road, the tokens that must also hold, the tokens that must not). write.route: a routed INSERT / MERGE (WIDENED adds a
/// multi-target write), not every INSERT.
pub const NARROWED: &[(Strategy, &[Prop], &[Prop])] = &[(WRITE_ROUTE, &[Prop::Route], &[])];

fn narrowed(r: Strategy, p: PropSet) -> bool {
    NARROWED.iter().filter(|n| n.0 == r).all(|&(_, need, not)| need.iter().all(|&t| p.has(t)) && !not.iter().any(|&t| p.has(t)))
}

/// `v4plan.py roads_for(K, P)`: every road whose predicate holds (within NARROWED), or that WIDENED widens.
pub fn strategies_for(k: KindSet, p: PropSet) -> StrategySet {
    let mut out = StrategySet::EMPTY;
    for d in STRATEGIES.iter().filter(|d| holds(d.premise(), k, p) && narrowed(**d, p) || WIDENED.iter().any(|&(r, t)| r == **d && p.has(t))) {
        out.add(*d);
    }
    out
}

/// The roads that apply to a graph (R-A): every road whose predicate holds over its properties, less the roads a milestone
/// dropped and the ones `gravity_road_off` takes off (those serve on their straight form).
pub fn applicable(p: &Props) -> StrategySet {
    let on = strategies_for(p.kinds, p.props);
    on.minus(registry::dropped_strategies()).minus(strategy::off())
}

/// The exit report: the applicable strategies no table provides. An unbuilt strategy never refuses: the statement serves
/// on its straight form (`face/source.rs` `on()` is false for it); the diag mode logs a line each.
pub fn unbuilt_later(p: &Props) -> StrategySet {
    applicable(p).minus(registry::built_strategies())
}

/// Admission's pick (plan §3.1) over the registry: `pick_in`.
pub fn pick(p: &Props) -> Option<(Variant, Option<i32>)> {
    pick_in(p, registry::built_kinds())
}

/// The unbuilt kind (not in `kinds`) with the latest milestone (ties: graph order), with its node; `None`: admitted (an
/// unbuilt strategy never refuses).
pub fn pick_in(p: &Props, kinds: KindSet) -> Option<(Variant, Option<i32>)> {
    let kind = p.need.iter().enumerate().filter(|(_, (k, _))| !kinds.has(*k)).max_by_key(|(i, (k, _))| (k.decl().milestone, core::cmp::Reverse(*i))).map(|(_, e)| e);
    kind.map(|&(k, node)| (Variant::Kind(k), Some(node)))
}

/// Admission (plan §3.1): `pick` refuses `NotBuilt`, so a statement's refusal names the milestone that first serves it.
pub fn admit(gx: &Gx<'_, '_>, p: &Props) {
    if let Some((v, node)) = pick(p) {
        crate::not_built!(node.unwrap_or_else(|| gx.root_node()), v)
    }
}

/// Every kind the graph needs that no table provides, with its node (the all-refusals diagnostic reads it).
pub fn unbuilt<'a>(p: &'a Props<'_>) -> impl Iterator<Item = (K, i32)> + 'a {
    let built = registry::built_kinds();
    p.need.iter().copied().filter(move |(k, _)| !built.has(*k))
}

const _: () = assert!(Prop::ALL.len() <= 64 && Applies::ALL.len() == 79);
