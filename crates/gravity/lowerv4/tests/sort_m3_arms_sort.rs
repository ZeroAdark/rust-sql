//! V4R-159 (M3-HOSTS): a union's arms continue a full sort's slot. A union under a Runs Sort (gsunion-004: CUBE ... ORDER
//! BY) and a sorted rollup no Sort re-orders (groupingsets-006 over a table; D467: decompose's synthetic sort over nodeAgg's order;
//! 041's VALUES input now folds as one tagged group, gsetsagg D3029)
//! stream their arms into the sort's end: every arm pipeline ends in the one Sort slot, arm 0 declares `Arms` over the
//! run and the slot's one `SortMerge` (exec v4/m3.rs `ArmSort` holds each arm's run and merges them at the last), and the
//! arms hold their runs resident with road sort.resident off too. G4: a union under a top-k keeps the Materialize form.
use gravity_lowerv4_it as common;

use gravity_abi::breaker::BreakerKind;
use gravity_lowerv4::face::strategy::{self, StrategySet};
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::End;

fn plan(rel: &str) -> String {
    std::fs::read_to_string(common::plans_dir().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// The Arms and SortMerge protocols of `rel` lowered under `off`, and each pipeline's sort slot.
fn lowered(rel: &str, off: &str) -> (Vec<Proto>, Vec<Proto>, Vec<Option<u8>>) {
    strategy::set_off(StrategySet::parse(off).unwrap_or_else(|e| panic!("{off}: {e}")));
    let l = match common::lower_fixture(&plan(rel)) {
        Some(Ok(l)) => l,
        Some(Err(e)) => panic!("{rel} lowers: {e:?}"),
        None => panic!("{rel}: decompose refused"),
    };
    strategy::set_off(StrategySet::EMPTY);
    let of = |k: ProtoKind| l.descs.v4.protos.iter().filter(|p| p.kind == k).copied().collect::<Vec<_>>();
    let sorts = l.descs.pipelines.iter().map(|p| match p.ends_in { End::Breaker(b) if b.kind == BreakerKind::Sort => Some(b.slot), _ => None }).collect();
    (of(ProtoKind::Arms), of(ProtoKind::SortMerge), sorts)
}

/// D3437 (C10 4a-2): where the arms read breakers their own children filled, each arm runs right after its child
/// (drain/arms.rs `arm_run`), so the `Arms` span runs from arm 0 to the last arm over the children between them
/// (frame/mechs.rs `arms_span`): `arms` of its pipelines end in the sort's slot, arm 0 and the last arm among them.
fn one_merged_run(rel: &str, arms: u64, off: &str) {
    let (a, m, sorts) = lowered(rel, off);
    let [a] = a[..] else { panic!("{rel}: one Arms protocol: {a:?}") };
    let [m] = m[..] else { panic!("{rel}: one SortMerge, declared at arm 0 only: {m:?}") };
    assert_eq!((a.slot, m.slot, m.pipeline), (m.slot, a.slot, a.pipeline), "{rel}: the arms over the merge's slot, both at arm 0");
    let span = a.pipeline as usize..(a.pipeline as u64 + a.arg) as usize;
    assert_eq!((sorts[span.start], sorts[span.end - 1]), (Some(a.slot), Some(a.slot)), "{rel}: the span {span:?} starts and ends at an arm");
    assert_eq!(sorts[span.clone()].iter().filter(|s| **s == Some(a.slot)).count() as u64, arms, "{rel}: {arms} arm pipelines in the span {span:?} end in the sort's slot");
    assert_eq!(sorts.iter().filter(|s| **s == Some(a.slot)).count() as u64, arms, "{rel}: only the arms produce the slot");
}

#[test]
fn a_union_under_a_full_sort_merges_its_arms_runs_once() {
    one_merged_run("gsunion-004.plan", 4, "");
}

#[test]
fn a_sorted_rollup_read_in_its_order_sorts_its_arms() {
    one_merged_run("regress-full/groupingsets-006.plan", 3, "");
}

#[test]
fn the_arms_hold_their_runs_with_sort_resident_off() {
    one_merged_run("gsunion-004.plan", 4, "sort.resident");
}

#[test]
fn a_union_under_a_top_k_keeps_the_materialize_form() {
    match common::lower_fixture(&plan("gsunion-014.plan")) {
        Some(Ok(_)) => {}
        other => panic!("gsunion-014 (ORDER BY .. LIMIT 5) serves its settled union in nest.spool's eager form (S2a): {:?}", other.map(|r| r.err().map(|e| format!("{e:?}")))),
    }
}
