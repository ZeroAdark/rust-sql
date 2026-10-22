//! GENID (pgrc-plans/GENID.done): a publish numbers its generation and the parts it installs or reserves above the
//! numbers the process already used in the table directory ([`TxnProbe::used_numbers`]) as well as above its base, so
//! two contents never share a (directory, generation) or a part file name while a cache of the process can hold the
//! first. The compile and the compaction precompute their generation (and the compaction its output number) before
//! the plan: the same rule.

use super::*;
use crate::plan::PublishPlan;
use crate::publish::{effective_manifest, next_gen, publish_compile};
use crate::walmodel::{RecordingSink, WalLog};
use pgrc_format::manifest::range_entries;
use pgrc_format::wal::PublishKind;

#[test]
fn next_gen_is_above_the_base_and_every_used_number() {
    assert_eq!(next_gen(0, (0, 0)), 1, "a first generation");
    assert_eq!(next_gen(5, (0, 0)), 6, "nothing used: the base's successor, as before");
    assert_eq!(next_gen(5, (3, 9)), 6, "used numbers below the base change nothing");
    assert_eq!(next_gen(2, (7, 0)), 8, "a rolled-back publish or a same-transaction TRUNCATE used 7");
}

/// The plan: `number_above` before any install or reservation lifts both counters; the manifest's generation is
/// [`crate::plan::PlanBuilder::gen`], its `prev_gen` still the base's.
#[test]
fn a_plan_numbers_above_the_used_numbers() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let probe = Probe::new(TxnVerdict::Committed);
    let mut w = open_writer(vec![int8_col(1)], stamp(100, 1));
    append_int8_rows(&mut w, &mut vfs, &mut kit, 100, |i| Some(i as i64));
    let out = finish_and_publish(&mut w, &mut vfs, &mut kit, &probe);
    assert_eq!((out.gen, out.manifest.header.next_part_no), (1, 1));
    let spec = *w.spec();
    let base = out.manifest;
    let mut b = PublishPlan::build(Some(&base), &spec, 110, PublishKind::Bulk);
    assert_eq!(b.gen(), 2);
    b.number_above((6, 4));
    assert_eq!(b.gen(), 7);
    assert_eq!(b.reserve(), 4, "a reservation takes the first unused part number");
    b.number_above((3, 2));
    assert_eq!(b.gen(), 7, "lower used numbers never lower the counters");
    let plan = b.finish().expect("finish");
    let h = plan.manifest.header;
    assert_eq!((h.gen, h.prev_gen, h.next_part_no), (7, 1, 5));
}

/// A publish on gen 1 in a directory that used generation 7 and part numbers below 5 (a rolled-back publish, or the
/// directory before a same-transaction TRUNCATE): generation 8, built on 1, its part numbered 5.
#[test]
fn a_publish_numbers_its_generation_and_parts_above_the_used_numbers() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let mut w1 = open_writer(vec![int8_col(1)], stamp(100, 1));
    append_int8_rows(&mut w1, &mut vfs, &mut kit, 100, |i| Some(i as i64));
    finish_and_publish(&mut w1, &mut vfs, &mut kit, &Probe::new(TxnVerdict::Committed));
    let probe = Probe::new(TxnVerdict::Committed).used(7, 5);
    let mut w2 = open_writer(vec![int8_col(1)], stamp(110, 1));
    append_int8_rows(&mut w2, &mut vfs, &mut kit, 100, |i| Some(i as i64 * 2));
    let out = finish_and_publish(&mut w2, &mut vfs, &mut kit, &probe);
    let h = out.manifest.header;
    assert_eq!((out.gen, h.prev_gen, out.part_nos.clone(), h.next_part_no), (8, 1, vec![5], 6));
    let eff = effective_manifest(&mut vfs, DIR, &probe, None).expect("walk").expect("effective");
    assert_eq!(eff.header.gen, 8);
    assert_eq!(eff.parts.iter().map(|p| p.part_no).collect::<Vec<_>>(), vec![0, 5], "part 0 carried, the new part named 5");
    let names = vfs.list_dir(DIR).expect("list");
    assert!(names.iter().any(|n| n == "part-5.pgrc") && names.iter().any(|n| n == "manifest-8.pgrcm"), "{names:?}");
    assert!(!names.iter().any(|n| n == "part-1.pgrc" || n == "manifest-2.pgrcm"), "no used number named again: {names:?}");
}

/// The compile precomputes its generation (its DVs are named by it) before the plan: above the used numbers too, and
/// the plan's generation must be the precomputed one.
#[test]
fn a_compile_s_precomputed_generation_is_above_the_used_numbers() {
    let mut vfs = mem_with_dir();
    let log = WalLog::shared();
    let mut kit = Kit::new();
    let mut w1 = logged_writer(vec![int8_col(1)], stamp(100, 1), &log);
    append_int8_rows(&mut w1, &mut vfs, &mut kit, 1000, |i| Some(i as i64));
    finish_and_publish(&mut w1, &mut vfs, &mut kit, &Probe::new(TxnVerdict::Committed));
    let spec = *w1.spec();
    let probe = Probe::new(TxnVerdict::Committed).used(6, 3);
    let mut sink = RecordingSink::new(DIR, log.clone());
    let rows: Vec<(u32, u16)> = (1..=10).map(|r| (0, r)).collect();
    let out = publish_compile(&mut vfs, &mut sink, DIR, &spec, &[(0, &rows[..])], 150, &probe, (None, None)).expect("compile").expect("publishes");
    assert_eq!((out.gen, out.manifest.header.prev_gen), (7, 1));
    let eff = effective_manifest(&mut vfs, DIR, &probe, None).expect("walk").expect("effective");
    assert_eq!((eff.header.gen, eff.parts[0].dv_gen), (7, 7));
    let names = vfs.list_dir(DIR).expect("list");
    assert!(names.iter().any(|n| n == "part-0-dv-7.pgrcs") && !names.iter().any(|n| n == "part-0-dv-2.pgrcs"), "{names:?}");
}

/// The compaction precomputes its output's part number and its generation: above the used numbers too.
#[test]
fn a_compaction_s_output_number_and_generation_are_above_the_used_numbers() {
    let mut vfs = mem_with_dir();
    let log = WalLog::shared();
    let mut kit = Kit::new();
    let mut probe = Probe::new(TxnVerdict::InProgress);
    for (fxid, rows) in [(100, 1000), (110, 500)] {
        probe = probe.set(fxid, TxnVerdict::Committed);
        let mut w = logged_writer(vec![int8_col(1)], stamp(fxid, 1), &log);
        append_int8_rows(&mut w, &mut vfs, &mut kit, rows, |i| Some(i as i64));
        finish_and_publish(&mut w, &mut vfs, &mut kit, &probe);
    }
    let probe = probe.used(9, 12);
    let mut w = logged_writer(vec![int8_col(1)], stamp(200, 1), &log);
    append_int8_rows(&mut w, &mut vfs, &mut kit, 1400, |i| Some(i as i64 * 7));
    let map = pgrc_format::rowmap::encode_runs(&[(391, 900), (2_910_000, 500)], pgrc_format::rowmap::Opts::default()).expect("the map");
    let ids = [(291, 291 + 1164), (2_910_000, 2_910_000 + 582)];
    let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
    let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
    w.finish(&mut env).expect("finish");
    let (out, no) = w.publish_compact(&mut vfs, &probe, (None, None), &[(0, 0), (1, 0)], (pgrc_format::manifest::PART_ROWMAP_GRANULES, &map), &ids).expect("compact");
    assert_eq!((out.gen, out.manifest.header.prev_gen, no, out.manifest.header.next_part_no), (10, 2, Some(12), 13));
    assert_eq!(out.manifest.parts.iter().map(|p| p.part_no).collect::<Vec<_>>(), vec![12]);
    assert_eq!(range_entries(&out.manifest).expect("ranges"), vec![(12, ids.to_vec())]);
    let names = vfs.list_dir(DIR).expect("list");
    assert!(names.iter().any(|n| n == "part-12.pgrc") && names.iter().any(|n| n == "part-12-rowmap-1.pgrcs"), "{names:?}");
    assert!(!names.iter().any(|n| n == "part-2.pgrc"), "{names:?}");
}

/// IX0 L3 (DV-3): a writer whose cut policy bounds a part asks for `align_up(max_rows)` IDs before a part's first row,
/// numbers its rows from the grant, and each sealed part homes `[lo, lo + align_up(rows))`; the publish lists every
/// part's range and raises the floor to the last end. A grant over a held one, or one smaller than the buffered rows,
/// is refused.
#[test]
fn a_granted_writer_homes_each_part_at_its_grant() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let policy = || PartCutPolicy { max_rows: 100, max_bytes: u64::MAX, cut_granule_rows: 100 };
    let mut w = open_writer_policy(vec![int8_col(1)], stamp(100, 1), policy());
    let mut next = 291u64;
    let mut grants = Vec::new();
    for i in 0..250u64 {
        if let Some(n) = w.needs_ids() {
            assert_eq!(n, 291, "align_up(max_rows)");
            w.grant_ids(next, next + n).expect("grant");
            grants.push(next);
            next += n;
        }
        assert_eq!(w.next_id(), Some(grants.last().unwrap() + i % 100), "row {i} numbered from its part's grant");
        append_int8_rows(&mut w, &mut vfs, &mut kit, 1, |_| Some(i as i64));
    }
    assert_eq!(w.needs_ids(), None, "a grant is held");
    assert!(w.grant_ids(next, next + 291).is_err(), "a grant over a held one");
    let out = finish_and_publish(&mut w, &mut vfs, &mut kit, &Probe::new(TxnVerdict::Committed));
    assert_eq!(out.part_nos, vec![0, 1, 2]);
    assert_eq!(range_entries(&out.manifest).expect("ranges"), vec![(0, vec![(291, 582)]), (1, vec![(582, 873)]), (2, vec![(873, 1164)])]);
    assert_eq!(pgrc_format::manifest::next_row_id(&out.manifest).expect("floor"), 1164);

    let mut w = open_writer_policy(vec![int8_col(1)], stamp(110, 1), policy());
    append_int8_rows(&mut w, &mut vfs, &mut kit, 10, |i| Some(i as i64));
    assert!(w.grant_ids(1164, 1170).is_err(), "a grant smaller than the buffered rows");
}
