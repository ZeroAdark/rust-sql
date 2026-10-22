use gravity_driver_rig as common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use common::*;
use gravity_abi::ScanPos;
use gravity_driver::{Call, Entry, Recheck, RowBuf, Step};
use gravity_program::{Generation, Handle, PlanKey};

const KEY: PlanKey = PlanKey { handle: Handle(5), generation: Generation(1) };
const NULLS: [u8; 4] = [0; 4];

#[test]
fn recheck_runs_a_second_node_on_the_substituted_row_and_leaves_the_exec_alone() {
    let mut entry = Entry::new(4);
    let program = Arc::new(p_lookup3(64));
    let (mut derive, stats) = FakeDerive::new([customer, no_rows, no_rows, no_rows]).with_stats();
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[1, 2, 3], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    let (mut v, mut n) = ([0u64; 3], [0u8; 3]);
    let pos = ScanPos::new(501, 0xF2, 0x1A58);
    let newer = [0xF2u64, 0x1A58, 501];
    let r = entry.recheck(Some(KEY), &program, &mut derive, &b, &mut |_, s| s.substitute(pos), RowBuf { values: &mut v, isnull: &mut n, region: &mut Vec::new(), byref: &[] }, None, None, &[]).unwrap();
    assert_eq!((r, v, n), ((Recheck::Pass, 1), newer, [0, 0, 0]), "the recheck node pushed the substituted row once");
    assert_eq!((entry.stats().built, entry.stats().rechecks, entry.park().len()), (2, 1, 1));
    assert_eq!(stats.binds.get(), 2);
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    assert_eq!(sink.last()[..3], [0xF1, 0x1A57, 500], "the executing node saw its own row");
    entry.end(exec, true);
    assert_eq!(entry.park().len(), 2);
}

#[test]
fn recheck_without_a_row_is_skip_and_reuses_the_parked_node() {
    let mut entry = Entry::new(4);
    let program = Arc::new(p_lookup3(64));
    let mut derive = FakeDerive::new([no_rows; 4]);
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let b = binding(&[9, 9, 9], &NULLS, &mut sink_ref, &flag);
    let (mut v, mut n) = ([0u64; 3], [0u8; 3]);
    for _ in 0..3 {
        let r = entry.recheck(Some(KEY), &program, &mut derive, &b, &mut |_, _| {}, RowBuf { values: &mut v, isnull: &mut n, region: &mut Vec::new(), byref: &[] }, None, None, &[]).unwrap();
        assert_eq!(r, (Recheck::Skip, 0));
    }
    assert_eq!((entry.stats().built, entry.stats().reused), (1, 2));
}

/// D610: a recheck from a pipeline that is not the program's last (an Append arm's locking write, a producer) stops the
/// run as `Step::Recheck` and the recheck node re-runs that pipeline, not pipeline 0. Before: `Error::Recheck`, and a
/// recheck ran pipeline 0 (here a no-op arm: `Skip`, the newest version dropped).
#[test]
fn a_recheck_from_a_middle_pipeline_reruns_that_pipeline() {
    use gravity_driver::Outcome;
    use gravity_program::{Arena, Code, End, Layout, LockKind, OutCol, Pipeline, Program, Reason, SiteDesc, WaitPolicy, WritePlan};
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::index_source(st.handle(), 1005, 1105, &[1, 2], &[0, 1], &[3])];
    let pipes = [
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_noop)),
        (Pipeline::serial(0, End::Sink, Reason::Lock), Code::hand(k_lock)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_noop)),
    ];
    let layout = Layout { nsites: 1, out_natts: 1, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 };
    let program = Arc::new(Program::with_pipelines(layout, &sites, &[OutCol::new("c0", 20, -1, 8, true)], WritePlan::locking(1005, LockKind::Exclusive, WaitPolicy::Block), pipes));
    let mut entry = Entry::new(4);
    let mut derive = FakeDerive::new([five_stock, no_rows, no_rows, no_rows]);
    derive.lock_outcomes = [Outcome::Done, Outcome::Skipped, Outcome::Done, Outcome::Recheck];
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[1, 2], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Recheck { site: 0 }, "pipeline 1 stops on the recheck");
    assert_eq!(exec.next_pipeline(), 1);
    let (mut v, mut n) = ([0u64; 1], [1u8; 1]);
    let pos = ScanPos::new(1005, 0xF2, 0x1A58);
    let r = entry.recheck(Some(KEY), &program, &mut derive, &b, &mut |_, s| s.substitute(pos), RowBuf { values: &mut v, isnull: &mut n, region: &mut Vec::new(), byref: &[] }, None, None, &[]).unwrap();
    assert_eq!((r.0, n), (Recheck::Pass, [0]), "the recheck node ran the locking pipeline on the substituted row");
    entry.end(exec, false);
}
