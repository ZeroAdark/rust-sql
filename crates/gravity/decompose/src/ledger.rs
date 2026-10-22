use types_nodes::NodeTag;

macro_rules! kinds {
    ($($k:ident),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[repr(u8)]
        pub enum Kind { $($k),+ }

        impl Kind {
            pub const ALL: &'static [Kind] = &[$(Kind::$k),+];

            pub const fn name(self) -> &'static str {
                match self { $(Kind::$k => stringify!($k)),+ }
            }

            pub const fn tag(self) -> NodeTag {
                paste_tag!(self; $($k),+)
            }

            pub fn of(tag: NodeTag) -> Option<Kind> {
                Kind::ALL.iter().copied().find(|k| k.tag() == tag)
            }

            /// The plan node kind's name of `tag`, "?" for a tag the ledger has no kind for (lowerv4's gate_tags
            /// diagnosis; it lived in corpus.rs, the test-only plan fixture reader, until alloclint's UNREACHED file row).
            pub fn tag_name(tag: NodeTag) -> &'static str {
                Kind::of(tag).map_or("?", |k| k.name())
            }
        }
    };
}

macro_rules! paste_tag {
    ($s:expr; $($k:ident),+) => {
        match $s { $(Kind::$k => tag_of!($k)),+ }
    };
}

macro_rules! tag_of {
    (Result) => { NodeTag::T_Result };
    (ProjectSet) => { NodeTag::T_ProjectSet };
    (ModifyTable) => { NodeTag::T_ModifyTable };
    (Append) => { NodeTag::T_Append };
    (MergeAppend) => { NodeTag::T_MergeAppend };
    (RecursiveUnion) => { NodeTag::T_RecursiveUnion };
    (BitmapAnd) => { NodeTag::T_BitmapAnd };
    (BitmapOr) => { NodeTag::T_BitmapOr };
    (SeqScan) => { NodeTag::T_SeqScan };
    (SampleScan) => { NodeTag::T_SampleScan };
    (IndexScan) => { NodeTag::T_IndexScan };
    (IndexOnlyScan) => { NodeTag::T_IndexOnlyScan };
    (BitmapIndexScan) => { NodeTag::T_BitmapIndexScan };
    (BitmapHeapScan) => { NodeTag::T_BitmapHeapScan };
    (TidScan) => { NodeTag::T_TidScan };
    (TidRangeScan) => { NodeTag::T_TidRangeScan };
    (SubqueryScan) => { NodeTag::T_SubqueryScan };
    (FunctionScan) => { NodeTag::T_FunctionScan };
    (ValuesScan) => { NodeTag::T_ValuesScan };
    (TableFuncScan) => { NodeTag::T_TableFuncScan };
    (CteScan) => { NodeTag::T_CteScan };
    (NamedTuplestoreScan) => { NodeTag::T_NamedTuplestoreScan };
    (WorkTableScan) => { NodeTag::T_WorkTableScan };
    (ForeignScan) => { NodeTag::T_ForeignScan };
    (CustomScan) => { NodeTag::T_CustomScan };
    (NestLoop) => { NodeTag::T_NestLoop };
    (NestLoopParam) => { NodeTag::T_NestLoopParam };
    (MergeJoin) => { NodeTag::T_MergeJoin };
    (HashJoin) => { NodeTag::T_HashJoin };
    (Material) => { NodeTag::T_Material };
    (Memoize) => { NodeTag::T_Memoize };
    (Sort) => { NodeTag::T_Sort };
    (IncrementalSort) => { NodeTag::T_IncrementalSort };
    (Group) => { NodeTag::T_Group };
    (Agg) => { NodeTag::T_Agg };
    (WindowAgg) => { NodeTag::T_WindowAgg };
    (Unique) => { NodeTag::T_Unique };
    (Gather) => { NodeTag::T_Gather };
    (GatherMerge) => { NodeTag::T_GatherMerge };
    (Hash) => { NodeTag::T_Hash };
    (SetOp) => { NodeTag::T_SetOp };
    (LockRows) => { NodeTag::T_LockRows };
    (Limit) => { NodeTag::T_Limit };
    (PlanRowMark) => { NodeTag::T_PlanRowMark };
    (PartitionPruneInfo) => { NodeTag::T_PartitionPruneInfo };
    (PlanInvalItem) => { NodeTag::T_PlanInvalItem };
    (SubPlan) => { NodeTag::T_SubPlan };
    (AlternativeSubPlan) => { NodeTag::T_AlternativeSubPlan };
}

kinds! {
    Result, ProjectSet, ModifyTable, Append, MergeAppend, RecursiveUnion, BitmapAnd, BitmapOr,
    SeqScan, SampleScan, IndexScan, IndexOnlyScan, BitmapIndexScan, BitmapHeapScan, TidScan,
    TidRangeScan, SubqueryScan, FunctionScan, ValuesScan, TableFuncScan, CteScan,
    NamedTuplestoreScan, WorkTableScan, ForeignScan, CustomScan, NestLoop, NestLoopParam,
    MergeJoin, HashJoin, Material, Memoize, Sort, IncrementalSort, Group, Agg, WindowAgg, Unique,
    Gather, GatherMerge, Hash, SetOp, LockRows, Limit, PlanRowMark, PartitionPruneInfo,
    PlanInvalItem, SubPlan, AlternativeSubPlan,
}

pub const PLAN_TAG_FIRST: u16 = NodeTag::T_Result as u16;
pub const PLAN_TAG_LAST: u16 = NodeTag::T_Limit as u16;
pub const ATTACHED_TAGS: &[NodeTag] = &[NodeTag::T_PlanRowMark, NodeTag::T_PartitionPruneInfo, NodeTag::T_PlanInvalItem, NodeTag::T_SubPlan, NodeTag::T_AlternativeSubPlan];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Milestone {
    D0,
    D1,
    D2,
    D3,
    D4,
    D5,
}

impl Milestone {
    pub const fn name(self) -> &'static str {
        match self {
            Milestone::D0 => "D0",
            Milestone::D1 => "D1",
            Milestone::D2 => "D2",
            Milestone::D3 => "D3",
            Milestone::D4 => "D4",
            Milestone::D5 => "D5",
        }
    }
}

pub const CURRENT: Milestone = Milestone::D5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Mapped(Milestone),
    Refused(Milestone, &'static str),
    Excluded(&'static str, &'static str),
}

use Milestone::*;
use State::*;

pub const LEDGER: &[(Kind, State)] = &[
    (Kind::Result, Mapped(D1)),
    (Kind::SeqScan, Mapped(D1)),
    (Kind::IndexScan, Mapped(D1)),
    (Kind::IndexOnlyScan, Mapped(D1)),
    (Kind::ValuesScan, Mapped(D1)),
    (Kind::Limit, Mapped(D1)),
    (Kind::Agg, Mapped(D2)),
    (Kind::Group, Mapped(D2)),
    (Kind::Unique, Mapped(D2)),
    (Kind::NestLoop, Mapped(D3)),
    (Kind::NestLoopParam, Mapped(D3)),
    (Kind::MergeJoin, Mapped(D3)),
    (Kind::HashJoin, Mapped(D3)),
    (Kind::Hash, Mapped(D3)),
    (Kind::Material, Mapped(D3)),
    (Kind::Memoize, Mapped(D3)),
    (Kind::Sort, Mapped(D3)),
    (Kind::IncrementalSort, Mapped(D3)),
    (Kind::WindowAgg, Mapped(D3)),
    (Kind::SetOp, Mapped(D3)),
    (Kind::RecursiveUnion, Mapped(D3)),
    (Kind::WorkTableScan, Mapped(D3)),
    (Kind::SubPlan, Mapped(D4)),
    (Kind::AlternativeSubPlan, Mapped(D4)),
    (Kind::ProjectSet, Mapped(D4)),
    (Kind::FunctionScan, Mapped(D4)),
    (Kind::CteScan, Mapped(D4)),
    (Kind::SubqueryScan, Mapped(D4)),
    (Kind::Append, Mapped(D4)),
    (Kind::MergeAppend, Mapped(D4)),
    (Kind::PartitionPruneInfo, Mapped(D4)),
    (Kind::ModifyTable, Mapped(D4)),
    (Kind::LockRows, Mapped(D4)),
    (Kind::PlanRowMark, Mapped(D4)),
    (Kind::NamedTuplestoreScan, Mapped(D4)),
    (Kind::BitmapAnd, Mapped(D5)),
    (Kind::BitmapOr, Mapped(D5)),
    (Kind::BitmapIndexScan, Mapped(D5)),
    (Kind::BitmapHeapScan, Mapped(D5)),
    (Kind::TidScan, Mapped(D5)),
    (Kind::TidRangeScan, Mapped(D5)),
    (Kind::SampleScan, Mapped(D5)),
    (Kind::TableFuncScan, Mapped(D5)),
    (Kind::ForeignScan, Excluded("a post-milestone-2 node family", "milestone-2.md §5 'node families'")),
    (Kind::CustomScan, Excluded("no Rust node type; the planner never builds one", "milestone-2.md §5 'node families'")),
    (Kind::Gather, Excluded("never planned under Gravity: the driver owns parallelism; one in a captured plan is a planner defect", "decompose.md §0 ruling 8 (2026-09-17)")),
    (Kind::GatherMerge, Excluded("never planned under Gravity: the driver owns parallelism; one in a captured plan is a planner defect", "decompose.md §0 ruling 8 (2026-09-17)")),
    (Kind::PlanInvalItem, Excluded("a plancache dependency, never executed", "plan.md §1: the plancache stays with the incumbent")),
];

pub fn state_of(k: Kind) -> State {
    LEDGER.iter().find(|(x, _)| *x == k).map(|(_, s)| *s).expect("every kind has a ledger row")
}

pub fn is_mapped(k: Kind) -> bool {
    matches!(state_of(k), Mapped(_))
}

/// A mapped kind reachable only through a kind of a later milestone (its served golden waits for that kind).
pub fn gated_by(k: Kind) -> Option<Kind> {
    match k {
        Kind::RecursiveUnion | Kind::WorkTableScan => Some(Kind::CteScan),
        Kind::BitmapIndexScan | Kind::BitmapAnd | Kind::BitmapOr => Some(Kind::BitmapHeapScan),
        _ => None,
    }
}

/// A mapped kind no plan tree carries: setrefs resolves an AlternativeSubPlan to one of its SubPlans before
/// execution (`fix_alternative_subplan`), so the cost choice `subplan::choose` makes appears in no golden.
pub fn unplanned(k: Kind) -> bool {
    k == Kind::AlternativeSubPlan
}

pub fn refused_now() -> impl Iterator<Item = (Kind, State)> {
    LEDGER.iter().copied().filter(|(k, _)| !is_mapped(*k) && !matches!(state_of(*k), Excluded(..)))
}

/// The corpus census's ownership (decompose.md §5.6) of every shape refusal a corpus still carries: a reason with a
/// row here is refused by a named ruling, a contract or a lane's written need; one without is unowned and the census
/// lists it by text for its owner-to-be. A row's text is a prefix of the refusal's (the write family's excluded
/// reasons carry their citation after the colon).
pub const OWNERS: &[(&str, &str)] = &[
    ("table access method", "by ruling: a relation neither heap nor pgrc2 (Rick 2026-09-17: foreign AMs may refuse; the create_am test's heap2, a sequence read as a table)"),
];

/// The owner of a refusal reason, or None when the census must list it as unowned.
pub fn owner(reason: &str) -> Option<&'static str> {
    OWNERS.iter().find(|(prefix, _)| reason.starts_with(prefix)).map(|(_, o)| *o)
}

pub fn reason(k: Kind) -> &'static str {
    match state_of(k) {
        Mapped(_) => "mapped",
        Refused(_, r) => r,
        Excluded(r, _) => r,
    }
}
