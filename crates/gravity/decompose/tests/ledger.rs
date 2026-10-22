use gravity_decompose::ledger::{self, Kind, Milestone, State, ATTACHED_TAGS, CURRENT, LEDGER, PLAN_TAG_FIRST, PLAN_TAG_LAST};
use types_nodes::NodeTag;

fn tag_set() -> Vec<NodeTag> {
    let mut v: Vec<NodeTag> = Kind::ALL.iter().map(|k| k.tag()).collect();
    v.sort_by_key(|t| *t as u16);
    v
}

#[test]
fn every_plan_node_kind_has_exactly_one_row() {
    let tags = tag_set();
    for k in Kind::ALL {
        assert_eq!(LEDGER.iter().filter(|(x, _)| x == k).count(), 1, "{} has one ledger row", k.name());
        assert_eq!(Kind::of(k.tag()), Some(*k));
    }
    assert_eq!(LEDGER.len(), Kind::ALL.len(), "no ledger row without a kind");
    let mut expected: Vec<u16> = (PLAN_TAG_FIRST..=PLAN_TAG_LAST).collect();
    expected.extend(ATTACHED_TAGS.iter().map(|t| *t as u16));
    expected.sort();
    let got: Vec<u16> = tags.iter().map(|t| *t as u16).collect();
    assert_eq!(got, expected, "the kind set is the plan-node tag range plus the attached kinds");
    assert_eq!(tags.len(), 48);
    assert!(tags.len() <= 64, "Kind fits the graph's mapped bitset");
}

#[test]
fn milestone_budget() {
    assert_eq!(CURRENT, Milestone::D5);
    let mapped: Vec<&str> = LEDGER.iter().filter(|(k, _)| ledger::is_mapped(*k)).map(|(k, _)| k.name()).collect();
    assert_eq!(mapped, ["Result", "SeqScan", "IndexScan", "IndexOnlyScan", "ValuesScan", "Limit", "Agg", "Group", "Unique", "NestLoop", "NestLoopParam", "MergeJoin", "HashJoin", "Hash", "Material", "Memoize", "Sort", "IncrementalSort", "WindowAgg", "SetOp", "RecursiveUnion", "WorkTableScan", "SubPlan", "AlternativeSubPlan", "ProjectSet", "FunctionScan", "CteScan", "SubqueryScan", "Append", "MergeAppend", "PartitionPruneInfo", "ModifyTable", "LockRows", "PlanRowMark", "NamedTuplestoreScan", "BitmapAnd", "BitmapOr", "BitmapIndexScan", "BitmapHeapScan", "TidScan", "TidRangeScan", "SampleScan", "TableFuncScan"]);
    let refused: Vec<(&str, &str)> = ledger::refused_now().map(|(k, s)| (k.name(), match s { State::Refused(m, _) => m.name(), State::Mapped(m) => m.name(), State::Excluded(..) => "x" })).collect();
    let budget: [(&str, &str); 0] = [];
    assert_eq!(refused, budget, "the refused budget is zero after D5: every kind is mapped or excluded with a ruling");
    assert!(ledger::gated_by(Kind::RecursiveUnion) == Some(Kind::CteScan) && ledger::gated_by(Kind::WorkTableScan) == Some(Kind::CteScan));
    assert!([Kind::BitmapIndexScan, Kind::BitmapAnd, Kind::BitmapOr].iter().all(|k| ledger::gated_by(*k) == Some(Kind::BitmapHeapScan)));
    assert_eq!(mapped.len(), 43, "43 mapped of 48");
    assert!(ledger::unplanned(Kind::AlternativeSubPlan) && ledger::is_mapped(Kind::AlternativeSubPlan), "the cost choice is made, never seen in a plan tree");
    let excluded: Vec<&str> = LEDGER.iter().filter(|(_, s)| matches!(s, State::Excluded(..))).map(|(k, _)| k.name()).collect();
    assert_eq!(excluded, ["ForeignScan", "CustomScan", "Gather", "GatherMerge", "PlanInvalItem"]);
    assert_eq!(mapped.len() + refused.len() + excluded.len(), Kind::ALL.len());
}

#[test]
fn every_refusal_lands_by_d5() {
    for (k, s) in LEDGER {
        match s {
            State::Refused(m, reason) => {
                assert!(*m >= Milestone::D2 && *m <= Milestone::D5, "{}: refused at {:?}", k.name(), m);
                assert!(!reason.is_empty());
            }
            State::Mapped(m) => assert!(*m <= Milestone::D5, "{}: mapped at {:?}", k.name(), m),
            State::Excluded(reason, ruling) => assert!(!reason.is_empty() && !ruling.is_empty(), "{}: an exclusion names its reason and ruling", k.name()),
        }
    }
}

/// D3: every member of the source family is mapped, or excluded by a ruling; every system lane has one bit.
#[test]
fn source_family_is_mapped_or_excluded() {
    use gravity_decompose::ops::scan::{FAMILY, SYS_LANES};
    for m in FAMILY.iter() {
        let row = gravity_decompose::LEDGER.iter().find(|r| r.0 == m.kind).unwrap_or_else(|| panic!("{:?} has no ledger row", m.kind));
        match row.1 {
            gravity_decompose::State::Mapped(_) => assert_ne!(m.source, "excluded", "{:?}", m.kind),
            gravity_decompose::State::Excluded(..) => assert_eq!(m.source, "excluded", "{:?}", m.kind),
            _ => panic!("{:?} is refused by kind", m.kind),
        }
    }
    assert!(SYS_LANES.len() <= 8 && SYS_LANES.iter().all(|l| l.0 <= 0 && l.0 != -1));
}
