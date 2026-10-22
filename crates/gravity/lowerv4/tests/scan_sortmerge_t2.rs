//! sortmerge T2 (D4469 road 2): a SeqScan the plan relies on for its pgrcolumnar `sorted_by` order (`SeqScan.pgrc_merge`)
//! lowers to a serial merged range site: its key the mark, no code lane, and no verdict that changes a window's rows or
//! leaves a lane undecoded (hosts `MergedRangeSource` refuses those at its bind). A plain aggregate over a marked scan reads
//! no order: decompose's D3002 rule makes it an ordinary eligible scan.
use gravity_abi::ctx::lane;
use gravity_lowerv4_it as common;
use gravity_program::protos::{ProtoKind, VERDICT_CODE_SPARSE, VERDICT_CODE_TABLE, VERDICT_COND, VERDICT_CONST_KEY, VERDICT_COUNT_ONLY, VERDICT_COUNT_SELECT, VERDICT_ELIDE, VERDICT_ELIDE_HIT, VERDICT_GUARD_SEL, VERDICT_LATE, VERDICT_PACKED, VERDICT_PACKED_ZONE, VERDICT_UNREAD};

const SHAPED: u64 = VERDICT_LATE | VERDICT_PACKED | VERDICT_UNREAD | VERDICT_PACKED_ZONE | VERDICT_ELIDE | VERDICT_ELIDE_HIT | VERDICT_CONST_KEY | VERDICT_GUARD_SEL | VERDICT_COUNT_ONLY | VERDICT_COUNT_SELECT | VERDICT_CODE_TABLE | VERDICT_CODE_SPARSE | VERDICT_COND;

fn lowered(name: &str) -> common::LoweredIn {
    let text = std::fs::read_to_string(common::plans_dir().join(format!("{name}.plan"))).expect("plan fixture");
    common::lower_fixture(&text).expect("decomposes").unwrap_or_else(|e| panic!("{name}: {e:?}"))
}

#[test]
fn a_marked_seqscan_lowers_to_a_serial_merged_site() {
    // (fixture, the merged sites' keys): ORDER BY a, b; a WITH TIES limit on a; a merge join of two marked scans (fs's
    // text column t read beside its key); a scan marked on a text column
    let cases: [(&str, &[&[i16]]); 4] = [("sortmerge-001", &[&[1, 2]]), ("sortmerge-002", &[&[1]]), ("sortmerge-003", &[&[1], &[1]]), ("sortmerge-004", &[&[3]])];
    for (name, keys) in cases {
        let l = lowered(name);
        let d = &l.descs;
        let merged: Vec<Vec<i16>> = d.sites.iter().filter(|s| s.is_merged()).map(|s| s.merge.to_vec()).collect();
        assert_eq!(merged, keys.iter().map(|k| k.to_vec()).collect::<Vec<_>>(), "{name}: the merged sites' keys");
        for (i, s) in d.sites.iter().enumerate().filter(|(_, s)| s.is_merged()) {
            assert!(s.lanes().iter().all(|l| l.kind != lane::KIND_CODES), "{name}: site {i} has a code lane: {s:?}");
            let drivers: Vec<usize> = (0..d.pipelines.len()).filter(|&k| d.pipelines[k].driving_site == i as i16).collect();
            assert!(!drivers.is_empty(), "{name}: site {i} is driven");
            for &k in &drivers {
                assert!(!d.pipelines[k].is_eligible(), "{name}: pipeline {k} over merged site {i} is serial");
                for p in d.v4.protos.iter().filter(|p| p.kind == ProtoKind::Verdicts && p.pipeline as usize == k && p.slot == 0) {
                    assert_eq!(p.arg & SHAPED, 0, "{name}: pipeline {k}'s verdicts {:#x} include a shaped one", p.arg);
                }
            }
        }
    }
}

#[test]
fn a_plain_aggregate_over_a_marked_scan_takes_no_merge() {
    let l = lowered("sortmerge-005");
    let d = &l.descs;
    assert!(d.sites.iter().all(|s| !s.is_merged()), "count(*) reads no order: no merged site");
    let k = (0..d.pipelines.len()).find(|&k| d.pipelines[k].driving_site == 0).expect("a pipeline drives the scan");
    assert!(d.pipelines[k].is_eligible(), "the unmerged scan claims morsels");
}
