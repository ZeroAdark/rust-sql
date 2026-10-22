//! DC-04: a top over groups (runs.topk_groups, ops/sort/topn/lower.rs `TopEnd`) keeps its records in place at the front of
//! the group emit's array, so its own slot holds none. Decompose sized that slot for `count + offset` run rows
//! (`topk_budget`: 64 + n x (16 + 8 natts + the null bitmap)), which the leader's footprint counted for nothing. Pinned
//! over every plan fixture that lowers: a lowered TopK breaker (in a v4 program, only a group top's: a row top's end
//! rewrites its breaker to a Sort) asks no bytes.
#[allow(dead_code)]
use gravity_lowerv4_it as common;

use gravity_abi::breaker::{Breaker, BreakerKind, Budget};
use gravity_program::End;

#[test]
fn a_group_top_asks_no_region_of_its_own() {
    let dir = common::plans_dir();
    let all = [common::files(&dir, "plan"), common::files(&dir.join("regress-full"), "plan")].concat();
    let (mut bad, mut seen) = (Vec::new(), 0);
    for (name, text) in all.into_iter().filter(|(_, t)| t.contains("{LIMIT") && t.contains("{SORT") && t.contains("{AGG")) {
        let Some(Ok(l)) = common::lower_fixture(&text) else { continue };
        for (k, p) in l.descs.pipelines.iter().enumerate() {
            if let End::Breaker(b @ Breaker { kind: BreakerKind::TopK, .. }) = p.ends_in {
                seen += 1;
                if b.bytes != 0 || b.budget != Budget::Fixed(0) {
                    bad.push(format!("{name}: p{k} TopK asks {} bytes ({:?})", b.bytes, b.budget));
                }
            }
        }
    }
    assert!(seen > 0, "no fixture lowers a top over groups");
    assert!(bad.is_empty(), "{} group top(s) asking a region:\n{}", bad.len(), bad.join("\n"));
}
