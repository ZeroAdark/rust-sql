//! SORTRUNS-E0 L0 (pgrc-plans/SORTRUNS-E0-STEP1.md D1, D2): the publish keeps `EXT_RUNS` true to the generation's
//! parts. A retired member's slot empties (or passes to its in-place rewrite) and a run left with no member goes,
//! before the record is logged (TEETH, plan.rs:246: unedited, the record names a part the generation lacks and every
//! reader refuses the table). A PUBLISH of a manifest with runs carries the kind bit and redoes byte for byte. The
//! end-of-recovery cleanup keeps a run's reverse file exactly while the effective or an in-progress generation names
//! its run, and removes the debris of a publish that did not commit.

use super::*;
use crate::cleanup::cleanup_dir;
use crate::plan::{apply_publish, manifest_of, redo_publish, ApplyMode, PublishPlan};
use crate::publish::TxnVerdict;
use crate::walsink::NoWal;
use pgrc_format::dirlayout::{manifest_file_name, runrev_file_name, CURRENT_FILE_NAME};
use pgrc_format::manifest::{range_entries, with_next_row_id, with_ranges, PartRecord, MANIFESTF_RUNS};
use pgrc_format::rowid::IDS_PER_BLOCK;
use pgrc_format::sortrun::{runs, with_runs, RankForm, RunRevForm, SortRun};
use pgrc_format::wal::{PublishKind, PublishRec, PUBLISH_KIND_RUNS};

const B: u64 = IDS_PER_BLOCK;

fn spec() -> crate::seal::PartSpec {
    *open_writer(vec![int8_col(1)], stamp(1, 1)).spec()
}

fn rec(part_no: u32, rows: u64) -> PartRecord {
    PartRecord { rows, file_len: 4096, footer_off: 2048, dv_gen: 0, dv_len: 0, part_no, flags: 0, granule_count: 1, band_count: 1, dv_crc: 0, granule_rows: 8192 }
}

fn homes(run_id: u64, members: &[u32], ranges: &[(u64, u64)]) -> SortRun {
    let span: u64 = ranges.iter().map(|r| r.1 - r.0).sum();
    SortRun { run_id, level: 1, rev: RunRevForm::Labels, rank: RankForm::Dense, rows: span, rev_len: 2 * 4096, rev_crc: 9, members: members.to_vec(), ranges: ranges.to_vec() }
}

/// Parts 1, 2, 3 (fxid 5): a homing run of 1 and 2 over `[B, 4B)`, part 3 homing `[20B, 21B)` itself, floor 40B.
fn base_with_run() -> Manifest {
    let mut b = PublishPlan::build(None, &spec(), 5, PublishKind::Bulk);
    for p in [1, 2, 3] {
        b.upsert(rec(p, 10));
    }
    let ext = with_next_row_id(&[], 40 * B).unwrap();
    let ext = with_ranges(&ext, &[(3, vec![(20 * B, 21 * B)])]).unwrap();
    b.ext(with_runs(&ext, &[homes(5, &[1, 2], &[(B, 4 * B)])]).unwrap());
    let m = b.finish().expect("plan").manifest;
    assert_ne!(m.header.flags & MANIFESTF_RUNS, 0);
    m
}

fn decoded(m: &Manifest) -> Manifest {
    Manifest::decode(&m.encode()).expect("the generation decodes")
}

/// TEETH (plan.rs:246): a compaction retiring one member of a homing run. Unedited, `EXT_RUNS` names the retired part
/// and the manifest does not decode; edited, the slot is empty and the run keeps homing its range.
#[test]
fn sortruns_teeth_a_retired_member_leaves_its_slot() {
    let base = base_with_run();
    let mut b = PublishPlan::build(Some(&base), &spec(), 6, PublishKind::Compact);
    b.retire(1);
    let plan = b.finish().expect("plan");
    let m = decoded(&plan.manifest);
    let rs = runs(&m).unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs[0].members, vec![0, 2], "slot 0 emptied");
    assert_eq!(rs[0].ranges, vec![(B, 4 * B)], "the run still homes its IDs");
    assert_eq!(manifest_of(Some(&base), &plan.rec), plan.manifest, "redo composes the same manifest");
    assert_eq!(range_entries(&m).unwrap(), vec![(3, vec![(20 * B, 21 * B)])]);
}

#[test]
fn sortruns_teeth_the_last_member_takes_the_run() {
    let base = base_with_run();
    let mut b = PublishPlan::build(Some(&base), &spec(), 6, PublishKind::Compact);
    b.retire(1);
    b.retire(2);
    let plan = b.finish().expect("plan");
    let m = decoded(&plan.manifest);
    assert!(runs(&m).unwrap().is_empty());
    assert_eq!(m.header.flags & MANIFESTF_RUNS, 0, "no runs left: the bit clears, a pre-E0 binary reads the table again");
}

#[test]
fn sortruns_an_in_place_rewrite_keeps_the_slot() {
    let base = base_with_run();
    let mut b = PublishPlan::build(Some(&base), &spec(), 6, PublishKind::Compact);
    b.retire(2);
    b.upsert(rec(7, 9));
    b.repoint_member(2, 7);
    let plan = b.finish().expect("plan");
    let m = decoded(&plan.manifest);
    assert_eq!(runs(&m).unwrap()[0].members, vec![1, 7]);
    // retiring a part outside every run changes no run
    let mut b = PublishPlan::build(Some(&base), &spec(), 7, PublishKind::Compact);
    b.retire(3);
    let m = decoded(&b.finish().expect("plan").manifest);
    assert_eq!(runs(&m).unwrap()[0].members, vec![1, 2]);
    assert!(range_entries(&m).unwrap().is_empty(), "part 3's own range left with it");
}

/// Runs that do not match the generation's parts are refused at plan time, before anything is logged.
#[test]
fn sortruns_a_plan_naming_a_missing_member_is_refused() {
    let base = base_with_run();
    let mut b = PublishPlan::build(Some(&base), &spec(), 6, PublishKind::Compact);
    b.set_runs(&[homes(5, &[1, 9], &[(B, 4 * B)])]).unwrap();
    assert!(b.finish().is_err(), "part 9 is not in the generation");
    let mut b = PublishPlan::build(Some(&base), &spec(), 6, PublishKind::Compact);
    b.set_runs(&[homes(5, &[1, 3], &[(B, 4 * B)])]).unwrap();
    assert!(b.finish().is_err(), "part 3 homes its own range: it cannot be a homing run's member");
}

#[test]
fn sortruns_the_publish_record_carries_the_bit_and_redoes_byte_equal() {
    let mut vfs = mem_with_dir();
    let base = base_with_run();
    let mut b0 = PublishPlan::build(None, &spec(), 5, PublishKind::Bulk);
    for p in [1, 2, 3] {
        b0.upsert(rec(p, 10));
    }
    b0.ext(base.ext.clone());
    let p0 = b0.finish().unwrap();
    let m0 = apply_publish(&mut vfs, DIR, None, &p0.rec, ApplyMode::Primary).expect("apply");
    let mut b = PublishPlan::build(Some(&m0), &spec(), 6, PublishKind::Compact);
    b.retire(1);
    let plan = b.finish().unwrap();
    let mut bytes = Vec::new();
    plan.rec.encode(&mut bytes).unwrap();
    assert_eq!(bytes[12 + 8], PublishKind::Compact as u8 | PUBLISH_KIND_RUNS);
    let back = PublishRec::decode(&bytes).unwrap();
    assert_eq!(back, plan.rec);
    let m1 = apply_publish(&mut vfs, DIR, Some(&m0), &plan.rec, ApplyMode::Primary).expect("apply");
    let (mpath, cpath) = (format!("{DIR}/{}", manifest_file_name(m1.header.gen)), format!("{DIR}/{CURRENT_FILE_NAME}"));
    let (written, current) = (vfs.read_full(&mpath).unwrap(), vfs.read_full(&cpath).unwrap());
    vfs.unlink_path(&mpath).unwrap();
    vfs.unlink_path(&cpath).unwrap();
    let redone = redo_publish(&mut vfs, DIR, &back).expect("redo").expect("applied");
    assert_eq!(vfs.read_full(&mpath).unwrap(), written, "byte for byte");
    assert_eq!(vfs.read_full(&cpath).unwrap(), current);
    assert_eq!(redone, m1);
}

fn put(vfs: &mut MemVfs, name: &str, bytes: &[u8]) {
    let p = format!("{DIR}/{name}");
    let fd = vfs.create_rw(&p).expect("create");
    vfs.pwrite_at(&fd, 0, bytes).expect("write");
    vfs.close_file(fd).expect("close");
}

/// D2 lifecycle, crash debris: the effective generation names run 7, an in-progress one runs 7 and 12; reverse 9 is
/// named by none. Cleanup removes 9 only; once the in-progress publisher aborted, 12 goes too; 7 stays.
#[test]
fn sortruns_cleanup_keeps_exactly_the_named_reverses() {
    let mut vfs = mem_with_dir();
    let mut b = PublishPlan::build(None, &spec(), 50, PublishKind::Bulk);
    for p in [1, 2] {
        b.upsert(rec(p, 10));
    }
    b.ext(with_runs(&with_next_row_id(&[], 40 * B).unwrap(), &[homes(7, &[1, 2], &[(B, 4 * B)])]).unwrap());
    let p1 = b.finish().unwrap();
    let m1 = apply_publish(&mut vfs, DIR, None, &p1.rec, ApplyMode::Primary).unwrap();
    let mut b = PublishPlan::build(Some(&m1), &spec(), 60, PublishKind::Compact);
    b.upsert(rec(3, 5));
    b.set_runs(&[homes(7, &[1, 2], &[(B, 4 * B)]), homes(12, &[3], &[(5 * B, 6 * B)])]).unwrap();
    let p2 = b.finish().unwrap();
    apply_publish(&mut vfs, DIR, Some(&m1), &p2.rec, ApplyMode::Primary).unwrap();
    for r in [7, 9, 12] {
        put(&mut vfs, &runrev_file_name(r), &[0u8; 4096]);
    }
    let live = Probe::new(TxnVerdict::Committed).set(60, TxnVerdict::InProgress);
    let rep = cleanup_dir(&mut vfs, &mut NoWal, DIR, &live).expect("cleanup");
    let rr: Vec<&String> = rep.removed.iter().filter(|n| n.starts_with("runrev-")).collect();
    assert_eq!(rr, vec![&runrev_file_name(9)], "only the unnamed reverse");
    let dead = Probe::new(TxnVerdict::Committed).set(60, TxnVerdict::Aborted);
    let rep = cleanup_dir(&mut vfs, &mut NoWal, DIR, &dead).expect("cleanup");
    assert!(rep.removed.contains(&runrev_file_name(12)), "the aborted publish's reverse: {:?}", rep.removed);
    assert!(!rep.removed.contains(&runrev_file_name(7)));
    assert!(vfs.exists_path(&format!("{DIR}/{}", runrev_file_name(7))).unwrap(), "the effective run's reverse stays");
}

/// The gates' knob's edit (PGRUST_PGRC_TEST_SELF_RUNS): a compaction output becomes a one-member self-homed run; the
/// output's next compaction drops it.
#[test]
fn sortruns_the_test_knob_writes_a_self_homed_run() {
    let mut b = PublishPlan::build(None, &spec(), 5, PublishKind::Bulk);
    for p in [1, 2] {
        b.upsert(rec(p, 10));
    }
    let base = b.finish().unwrap().manifest;
    let mut b = PublishPlan::build(Some(&base), &spec(), 6, PublishKind::Compact);
    b.retire(1);
    b.retire(2);
    b.upsert(rec(4, 20));
    crate::publish::add_self_run(Some(&base), &mut b, 4, 20).unwrap();
    let m = decoded(&b.finish().unwrap().manifest);
    let rs = runs(&m).unwrap();
    assert_eq!((rs.len(), rs[0].rev, rs[0].members.clone(), rs[0].run_id), (1, RunRevForm::None, vec![4], m.header.gen));
    assert_ne!(m.header.flags & MANIFESTF_RUNS, 0);
    let mut b = PublishPlan::build(Some(&m), &spec(), 7, PublishKind::Compact);
    b.retire(4);
    b.upsert(rec(5, 20));
    let m2 = decoded(&b.finish().unwrap().manifest);
    assert!(runs(&m2).unwrap().is_empty());
    assert_eq!(m2.header.flags & MANIFESTF_RUNS, 0);
}

/// SORTRUNS-E0 L1, the test-only writer (`testruns::build`): its reverse answers every ID of its member as the member's
/// map holds it, through a group and the member's fast path, an exception, or a frame; an ID of the run's ranges no row
/// holds answers no row.
#[test]
fn sortruns_l1_the_test_writer_builds_a_reverse_that_answers_every_id() {
    use pgrc_format::rowmap::GranMap;
    use pgrc_format::runrev::{BytePages, Hit, RunRev};
    let ranges = vec![(10 * B, 30 * B)];
    let mut seed = 7u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    // key order: blocks of ascending IDs, the blocks shuffled; every 6th ID dropped
    let mut blocks: Vec<Vec<u64>> = (10 * B..30 * B).filter(|id| id % 6 != 0).collect::<Vec<_>>().chunks(700).map(|c| c.to_vec()).collect();
    for i in (1..blocks.len()).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        blocks.swap(i, j);
    }
    let ids: Vec<u64> = blocks.concat();
    for form in [RunRevForm::FrameIx, RunRevForm::Labels] {
        for rank in [None, Some(RankForm::Set)] {
            let h = crate::testruns::build(form, rank, 31, 4, &ids, &ranges).expect("built");
            assert_eq!((h.run.rev, h.run.members.clone(), h.run.ranges.clone(), h.run.rows), (form, vec![4], ranges.clone(), ids.len() as u64));
            assert_eq!(h.file.len() as u64, h.run.rev_len);
            let m = GranMap::decode(&h.member_map, ids.len() as u64).expect("member map");
            let src = BytePages(&h.file);
            let rev = RunRev::open(&h.run, &src).expect("opens against its EXT_RUNS entry");
            for (o, &id) in ids.iter().enumerate() {
                let got = match rev.lookup(id, &src).unwrap() {
                    Hit::Group { slot: 0, va, vb } => m.run_find(va, vb, id).unwrap(),
                    Hit::Frame { slot: 0, vframe } => m.run_find_in(vframe, id).unwrap(),
                    other => panic!("{form:?} {rank:?}: {id} -> {other:?}"),
                };
                assert_eq!(got, Some(o as u32), "{form:?} {rank:?}: {id}");
            }
            for id in (10 * B..30 * B).filter(|id| id % 6 == 0).step_by(5) {
                let got = match rev.lookup(id, &src).unwrap() {
                    Hit::Absent => None,
                    Hit::Group { va, vb, .. } => m.run_find(va, vb, id).unwrap(),
                    Hit::Frame { vframe, .. } => m.run_find_in(vframe, id).unwrap(),
                };
                assert_eq!(got, None, "{form:?} {rank:?}: dropped {id}");
            }
        }
    }
    assert!(crate::testruns::build(RunRevForm::FrameIx, None, 31, 4, &[5 * B], &ranges).is_err(), "an ID outside the run's ranges");
    assert!(crate::testruns::build(RunRevForm::None, None, 31, 4, &ids, &ranges).is_err(), "NONE has no reverse");
}

/// SORTRUNS-E0 L1: a compaction published as a run that homes its output's IDs (the knob's edit): the output has no
/// `EXT_RANGES` entry, the run homes the inputs' ranges, and the generation decodes (one home per ID). The output's
/// own compaction brings the run's ranges to its output and drops the run.
#[test]
fn sortruns_l1_a_compaction_published_as_a_homing_run() {
    let mut b = PublishPlan::build(None, &spec(), 5, PublishKind::Bulk);
    for p in [1, 2] {
        b.upsert(rec(p, 5));
    }
    b.ext(with_ranges(&with_next_row_id(&[], 40 * B).unwrap(), &[(1, vec![(B, 2 * B)]), (2, vec![(2 * B, 3 * B)])]).unwrap());
    let base = b.finish().unwrap().manifest;
    let mut b = PublishPlan::build(Some(&base), &spec(), 6, PublishKind::Compact);
    b.retire(1);
    b.retire(2);
    b.upsert(rec(4, 10));
    let ids: Vec<u64> = (0..5).map(|i| B + 3 * i).chain((0..5).map(|i| 2 * B + 7 * i)).collect();
    let h = crate::testruns::build(RunRevForm::FrameIx, None, b.gen(), 4, &ids, &vec![(B, 3 * B)]).unwrap();
    b.set_ranges(4, Vec::new());
    crate::publish::add_run(Some(&base), &mut b, h.run.clone()).unwrap();
    let m = decoded(&b.finish().unwrap().manifest);
    assert!(range_entries(&m).unwrap().is_empty(), "the member has no entry: the run homes its IDs");
    let rs = runs(&m).unwrap();
    assert_eq!((rs.len(), rs[0].homes(), rs[0].ranges.clone(), rs[0].rank), (1, true, vec![(B, 3 * B)], RankForm::Set));
    // the member's own compaction: the run goes, its ranges with the output
    let mut b = PublishPlan::build(Some(&m), &spec(), 7, PublishKind::Compact);
    b.retire(4);
    b.upsert(rec(5, 10));
    b.set_ranges(5, vec![(B, 3 * B)]);
    let m2 = decoded(&b.finish().unwrap().manifest);
    assert!(runs(&m2).unwrap().is_empty());
    assert_eq!(range_entries(&m2).unwrap(), vec![(5, vec![(B, 3 * B)])]);
    // a run under another generation than the publish's is refused
    let mut b = PublishPlan::build(Some(&m), &spec(), 7, PublishKind::Compact);
    assert!(crate::publish::add_run(Some(&m), &mut b, h.run.clone()).is_err());
}

/// SORTRUNS-COPYRACE (a) (main 10-08; pgrc-plans/SORTRUNS-E0-STEP1.md §9.6): a COPY's publish lands between a run
/// job's reservation of its run ID and the run's publish, numbered above the reservation (GENID): the run still
/// publishes over it, its ID below the base's generation (L3a refused it: the run pass starved under steady ingest).
#[test]
fn sortruns_copyrace_a_run_publishes_over_a_base_that_passed_its_id() {
    let mut b = PublishPlan::build(None, &spec(), 5, PublishKind::Bulk);
    b.upsert(rec(1, 10));
    b.ext(with_ranges(&with_next_row_id(&[], 40 * B).unwrap(), &[(1, vec![(B, 2 * B)])]).unwrap());
    let read = b.finish().unwrap().manifest;
    let (g0, id) = (read.header.gen, read.header.gen + 1);
    // the COPY, numbered above the reserved ID (next_gen over the used numbers)
    let mut b = PublishPlan::build(Some(&read), &spec(), 6, PublishKind::Bulk);
    b.number_above((id, 0));
    b.upsert(rec(2, 10));
    b.set_ranges(2, vec![(2 * B, 3 * B)]);
    let copy = b.finish().unwrap().manifest;
    assert_eq!(copy.header.gen, id + 1, "the COPY's generation passed the reserved ID");
    // the run's publish: part 1 merged into member 4 of a run homing its IDs
    let mut b = PublishPlan::build(Some(&copy), &spec(), 7, PublishKind::Compact);
    b.retire(1);
    b.upsert(rec(4, 10));
    b.set_ranges(4, Vec::new());
    crate::publish::add_run_from(Some(&copy), &mut b, homes(id, &[4], &[(B, 2 * B)]), g0).expect("the run publishes over the COPY's generation");
    let m = decoded(&b.finish().unwrap().manifest);
    let rs = runs(&m).unwrap();
    assert_eq!((rs.len(), rs[0].run_id, rs[0].members.clone()), (1, id, vec![4]));
    assert!(m.header.gen > id + 1);
    assert_eq!(range_entries(&m).unwrap(), vec![(2, vec![(2 * B, 3 * B)])], "the COPY's part keeps its own IDs");
}

/// SORTRUNS-COPYRACE (b): a second run with the ID of a run the base holds is refused: with steps and concurrent
/// publishes, uniqueness no longer follows from "above the base's generation", so it is checked.
#[test]
fn sortruns_copyrace_a_second_run_with_a_base_runs_id_is_refused() {
    let base = base_with_run();
    let held = runs(&base).unwrap();
    assert_eq!(held.iter().map(|r| r.run_id).collect::<Vec<_>>(), vec![5]);
    let g = base.header.gen;
    assert!(!crate::publish::run_id_ok(5, g, 9, &held), "run 5 again");
    assert!(crate::publish::run_id_ok(6, g, 9, &held));
    let mut b = PublishPlan::build(Some(&base), &spec(), 6, PublishKind::Compact);
    b.number_above((8, 0));
    b.retire(3);
    b.upsert(rec(4, 10));
    b.set_ranges(4, Vec::new());
    assert!(crate::publish::add_run_from(Some(&base), &mut b, homes(5, &[4], &[(20 * B, 21 * B)]), g).is_err(), "a publish adding run 5 again");
}

/// SORTRUNS-COPYRACE (c): an ID at or below the generation the run's job started from is refused (such a number may
/// be a generation's the job read, and so a run's); so is one above the publish's generation.
#[test]
fn sortruns_copyrace_an_id_at_or_below_the_jobs_start_is_refused() {
    for (id, ok) in [(0, false), (3, false), (4, false), (5, true), (9, true), (10, false)] {
        assert_eq!(crate::publish::run_id_ok(id, 4, 9, &[]), ok, "run ID {id}, job start 4, publish 9");
    }
}

/// SORTRUNS-E0 L2: a run's ID is its publish's generation (the gates' knob) or one its job reserved between the
/// base's generation and the publish's (`pgrc_am::catalog::reserve_gen`); the base's own number or an older one (a
/// generation's, so maybe a run's of the base) and any above the publish are refused.
#[test]
fn sortruns_l2_a_run_id_lies_above_the_base_and_at_most_the_publish() {
    let mut b = PublishPlan::build(None, &spec(), 5, PublishKind::Bulk);
    b.upsert(rec(1, 10));
    let base = b.finish().unwrap().manifest;
    let g = base.header.gen;
    for (id, ok) in [(0, false), (g, false), (g + 1, true), (g + 2, true), (g + 3, true), (g + 4, false)] {
        let mut b = PublishPlan::build(Some(&base), &spec(), 6, PublishKind::Compact);
        b.number_above((g + 2, 0));
        assert_eq!(b.gen(), g + 3, "two numbers reserved above the base");
        b.retire(1);
        b.upsert(rec(4, 10));
        assert_eq!(crate::publish::add_run(Some(&base), &mut b, homes(id, &[4], &[(B, 2 * B)])).is_ok(), ok, "run ID {id} over base {g}");
    }
}
