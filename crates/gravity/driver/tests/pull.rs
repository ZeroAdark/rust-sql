use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::Datum;
use gravity_driver::{BorrowedBatch, Entry, MorselRange, PullAdapter, PullBuffer, Sink};
use gravity_program::{Generation, Handle, PlanKey};

const KEY: PlanKey = PlanKey { handle: Handle(3), generation: Generation(1) };
const NULLS: [u8; 4] = [0; 4];
const PARAMS: [Datum; 4] = [1, 2, 3, 4];

fn collect(adapter: &mut PullAdapter<'_>, entry: &mut Entry, n: usize) -> Vec<Vec<Datum>> {
    let mut got = Vec::new();
    let served = adapter.fetch(entry, n, &mut |v, _| {
        got.push(v.to_vec());
        Ok(())
    });
    assert_eq!(served.unwrap(), got.len());
    got
}

#[test]
fn fetch_three_twice_then_all_enters_the_pipeline_ceil_rows_over_batch_times() {
    let mut entry = Entry::new(4);
    let program = Arc::new(p_lookup4(4));
    let mut derive = FakeDerive::new([ten_rows, no_rows, no_rows, no_rows]);
    let flag = AtomicU8::new(0);
    let mut adapter = PullAdapter::new(2, 4);
    for _ in 0..3 {
        adapter.open(&mut entry, Some(KEY), &program, &mut derive, parts(&PARAMS, &NULLS, &flag), lease(gravity_driver::Grant::of(8 << 20))).unwrap();
        assert_eq!(collect(&mut adapter, &mut entry, 3), vec![vec![40, 200], vec![41, 201], vec![42, 202]]);
        assert_eq!(adapter.buffered(), 1);
        assert_eq!(collect(&mut adapter, &mut entry, 3), vec![vec![43, 203], vec![44, 204], vec![45, 205]]);
        assert_eq!(adapter.buffered(), 2);
        assert_eq!(collect(&mut adapter, &mut entry, usize::MAX), vec![vec![46, 206], vec![47, 207], vec![48, 208], vec![49, 209]]);
        assert_eq!(adapter.entries(), 3);
        assert!(collect(&mut adapter, &mut entry, 5).is_empty());
        assert_eq!(adapter.entries(), 3);
        adapter.close(&mut entry, true);
    }
    assert_eq!((entry.stats().built, entry.stats().reused), (1, 2));
}

#[test]
fn early_close_parks_the_node_with_rows_left() {
    let mut entry = Entry::new(4);
    let program = Arc::new(p_lookup4(4));
    let mut derive = FakeDerive::new([ten_rows, no_rows, no_rows, no_rows]);
    let flag = AtomicU8::new(0);
    let mut adapter = PullAdapter::new(2, 4);
    adapter.open(&mut entry, Some(KEY), &program, &mut derive, parts(&PARAMS, &NULLS, &flag), lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(collect(&mut adapter, &mut entry, 1).len(), 1);
    adapter.close(&mut entry, true);
    assert_eq!((adapter.buffered(), entry.park().len()), (0, 1));
    adapter.open(&mut entry, Some(KEY), &program, &mut derive, parts(&PARAMS, &NULLS, &flag), lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(collect(&mut adapter, &mut entry, 100).len(), 10);
    adapter.close(&mut entry, true);
}

#[test]
fn fetch_five_then_fifteen_suspends_once_and_completes_on_the_second_call() {
    // The bound-Execute contract (exec.md): max_rows = 5 serves 5 rows and
    // leaves the execution open (over-produced rows buffered, the resume word
    // kept); the next call serves the remaining 15 and completes.
    let mut entry = Entry::new(4);
    let program = Arc::new(p_lookup4(8));
    let mut derive = FakeDerive::new([twenty_rows, no_rows, no_rows, no_rows]);
    let flag = AtomicU8::new(0);
    let mut adapter = PullAdapter::new(2, 8);
    adapter.open(&mut entry, Some(KEY), &program, &mut derive, parts(&PARAMS, &NULLS, &flag), lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    let first = collect(&mut adapter, &mut entry, 5);
    assert_eq!(first.len(), 5);
    assert_eq!(first[0], vec![400, 300]);
    assert!(!adapter.is_done(), "suspended with rows left");
    assert_eq!(adapter.buffered(), 3, "the batch's over-produced rows wait in the buffer");
    let second = collect(&mut adapter, &mut entry, 15);
    assert_eq!(second.len(), 15);
    assert_eq!(second[14], vec![419, 319]);
    assert!(adapter.is_done(), "complete after the second call");
    assert!(collect(&mut adapter, &mut entry, 15).is_empty());
    adapter.close(&mut entry, true);
    assert_eq!(entry.stats().ended_ok, 1);
}

/// D439: a program that declares `expr.volatile` (a VOLATILE call per row) is asked for the rows each fetch still owes, so
/// no row is produced (its calls run) ahead of the count: nothing is left buffered. Only such a program: the batch contract
/// above (over-produced rows buffered) holds for every other.
#[test]
fn a_volatile_program_produces_no_row_ahead_of_the_fetch_count() {
    let mut entry = Entry::new(4);
    let mut p = p_lookup4(8);
    let mut d = gravity_program::protos::V4Descs::new(vec![gravity_program::protos::Proto::new(gravity_program::protos::ProtoKind::Volatile, 0)]);
    d.pull_exact = true; // what lowerv4 effects/visible.rs `pull_exact` declares for a Volatile protocol
    p.set_v4(&d);
    let program = Arc::new(p);
    let mut derive = FakeDerive::new([twenty_rows, no_rows, no_rows, no_rows]);
    let flag = AtomicU8::new(0);
    let mut adapter = PullAdapter::new(2, 8);
    adapter.open(&mut entry, Some(KEY), &program, &mut derive, parts(&PARAMS, &NULLS, &flag), lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    let first = collect(&mut adapter, &mut entry, 5);
    assert_eq!((first.len(), first[0].clone()), (5, vec![400, 300]));
    assert_eq!(adapter.buffered(), 0, "no row produced past the fetch's five");
    assert_eq!(collect(&mut adapter, &mut entry, 2), vec![vec![405, 305], vec![406, 306]]);
    assert_eq!(adapter.buffered(), 0, "nor past the next fetch's two");
    let rest = collect(&mut adapter, &mut entry, usize::MAX);
    assert_eq!((rest.len(), rest[12].clone()), (13, vec![419, 319]));
    assert!(adapter.is_done());
    adapter.close(&mut entry, true);
    assert_eq!(entry.stats().ended_ok, 1);
}

/// entry.md §3 (chore C1): a two-pipeline program on the adapter — the fold pipeline runs to completion through
/// `with_open` (the runner's serial arm: one `ALL` claim, then `end_claims`), the pull then takes the emit as a
/// one-pipeline program's first batch; a reopen runs the producer again on the parked node.
#[test]
fn a_two_pipeline_program_runs_its_producer_then_pulls_the_emit() {
    let mut entry = Entry::new(4);
    let program = Arc::new(p_count(4));
    let table = RangeTable::new(8, 16);
    let mut derive = FakeDerive::ranges(table);
    let flag = AtomicU8::new(0);
    let params: [Datum; 4] = [100, 0, 0, 0];
    let mut adapter = PullAdapter::new(2, 4);
    for _ in 0..2 {
        adapter.open(&mut entry, Some(KEY), &program, &mut derive, parts(&params, &NULLS, &flag), lease(gravity_driver::Grant::of(8 << 20))).unwrap();
        adapter
            .with_open(|exec, binding| -> gravity_driver::Result<()> {
                assert_eq!(exec.next_pipeline(), 0);
                entry.run_claim(exec, binding, MorselRange::ALL)?;
                entry.end_claims(exec)?;
                assert_eq!(exec.next_pipeline(), 1);
                Ok(())
            })
            .unwrap();
        assert_eq!(adapter.buffered(), 0, "the producer pushed nothing");
        assert_eq!(collect(&mut adapter, &mut entry, 10), vec![vec![100, 4950]]);
        assert!(adapter.is_done());
        assert_eq!(adapter.entries(), 1);
        adapter.close(&mut entry, true);
    }
    assert_eq!((entry.stats().built, entry.stats().reused, entry.stats().ended_ok), (1, 1, 2));
}

/// lower.md §15.13: a by-reference datum inside the pushed region moves with the copy by its offset; a
/// by-value word that happens to fall in the range, a NULL cell and a pointer outside the region do not.
#[test]
fn relocate_moves_only_the_by_reference_datums_inside_the_region() {
    let from = [0u8; 64];
    let to = [0u8; 64];
    let lo = from.as_ptr() as u64;
    let mut v = [lo + 8, lo + 8, lo + 16, lo + 1000, 5];
    let n = [0u8, 0, 1, 0, 0];
    gravity_driver::relocate(&mut v, &n, &[true, false, true, true, true], (lo, 64), to.as_ptr());
    let t = to.as_ptr() as u64;
    assert_eq!(v, [t + 8, lo + 8, lo + 16, lo + 1000, 5]);
}

/// Multi-emit (main 09-23): every Sink pipeline is an emit. Two UNION ALL arms come out in pipeline order, each suspending
/// at the batch size, and a FETCH count spans the arms (the ladder's `open_next_arm`); the second arm's first call is a batch.
#[test]
fn two_sink_arms_pull_in_order_and_a_fetch_spans_them() {
    let mut entry = Entry::new(4);
    let table = RangeTable::new(8, 16);
    let mut derive = FakeDerive::ranges(table.clone());
    let flag = AtomicU8::new(0);
    let params: [Datum; 4] = [100, 0, 0, 0];
    let one = Arc::new(p_rows(16));
    let mut adapter = PullAdapter::new(2, 16);
    adapter.open(&mut entry, None, &one, &mut derive, parts(&params, &NULLS, &flag), lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    let arm = collect(&mut adapter, &mut entry, usize::MAX);
    adapter.close(&mut entry, true);
    assert_eq!(arm.len() as u64, table.rows());
    let program = Arc::new(p_two_arms(16));
    let mut derive = FakeDerive::ranges(table.clone());
    adapter.open(&mut entry, None, &program, &mut derive, parts(&params, &NULLS, &flag), lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    let mut got = Vec::new();
    for want in [100, 100, 100] {
        let rows = collect(&mut adapter, &mut entry, want);
        assert_eq!(rows.len(), want.min(2 * arm.len() - got.len()), "a FETCH count spans the arms");
        got.extend(rows);
    }
    assert!(adapter.is_done());
    let both: Vec<Vec<Datum>> = arm.iter().chain(arm.iter()).cloned().collect();
    assert_eq!(got, both, "arm 0's rows, then arm 1's");
    adapter.close(&mut entry, true);
}

/// D2584: one pull of `p_two_arms` with `mask` marked to run empty at the open: the rows it sent and whether it ended.
fn pull_masked<'a>(adapter: &mut PullAdapter<'a>, entry: &mut Entry, table: &Arc<RangeTable>, params: &'a [Datum; 4], flag: &'a AtomicU8, mask: u64) -> gravity_driver::Result<(usize, bool)> {
    let program = Arc::new(p_two_arms(16));
    let mut derive = FakeDerive::ranges(table.clone());
    adapter.open(entry, None, &program, &mut derive, parts(params, &NULLS, flag), lease(gravity_driver::Grant::of(8 << 20)))?;
    adapter.with_open(|exec, _| -> gravity_driver::Result<()> {
        exec.run_empty(mask);
        Ok(())
    })?;
    let mut n = 0usize;
    let r = adapter.fetch(entry, usize::MAX, &mut |_, _| {
        n += 1;
        Ok(())
    });
    let done = adapter.is_done();
    adapter.close(entry, r.is_ok());
    r.map(|_| (n, done))
}

/// D2584: an arm marked to run empty before it opens (`Exec::run_empty`: the runner's mark of an Append arm its pruning
/// removed) is never opened nor called: the pull goes on to the next arm (the last one masked ends it), and a masked arm
/// never reads (a failing read raises while an arm walks, never once both are masked).
#[test]
fn a_masked_arm_is_never_opened_and_the_pull_goes_on() {
    let mut entry = Entry::new(4);
    let table = RangeTable::new(8, 16);
    let flag = AtomicU8::new(0);
    let params: [Datum; 4] = [100, 0, 0, 0];
    let mut adapter = PullAdapter::new(2, 16);
    let rows = table.rows() as usize;
    for (mask, want) in [(0b00u64, 2 * rows), (0b01, rows), (0b10, rows), (0b11, 0)] {
        assert_eq!(pull_masked(&mut adapter, &mut entry, &table, &params, &flag, mask).expect("no read fails"), (want, true), "mask {mask:#b}");
    }
    table.fail_block.store(0, Ordering::Relaxed);
    assert!(pull_masked(&mut adapter, &mut entry, &table, &params, &flag, 0b10).is_err(), "arm 0 walks and its read fails");
    assert_eq!(pull_masked(&mut adapter, &mut entry, &table, &params, &flag, 0b11).expect("neither arm reads"), (0, true));
    table.fail_block.store(u64::MAX, Ordering::Relaxed);
}

/// Multi-emit with a producer between the arms: with the producers the caller's (`set_external_producers`, the runner's
/// `Producers`), the FETCH stops before the fold (`producer_next`), the caller runs it, and the pull goes on to the second
/// arm. Inline (the serial driver's road) the adapter runs the fold's calls itself.
#[test]
fn a_producer_between_arms_runs_before_the_second_arm() {
    let mut entry = Entry::new(4);
    let table = RangeTable::new(8, 16);
    let flag = AtomicU8::new(0);
    let params: [Datum; 4] = [100, 0, 0, 0];
    let program = Arc::new(p_arm_fold_arm(16));
    let mut adapter = PullAdapter::new(2, 16);
    let mut derive = FakeDerive::ranges(table.clone());
    adapter.open(&mut entry, None, &program, &mut derive, parts(&params, &NULLS, &flag), lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    adapter.set_external_producers(true);
    assert!(!adapter.producer_next(), "the first pipeline is an arm");
    let first = collect(&mut adapter, &mut entry, usize::MAX);
    assert_eq!(first.len() as u64, table.rows(), "the FETCH stops before the fold");
    assert!(adapter.producer_next());
    adapter
        .with_open(|exec, binding| -> gravity_driver::Result<()> {
            assert_eq!(exec.next_pipeline(), 1);
            entry.run_claim(exec, binding, MorselRange::ALL)?;
            entry.end_claims(exec)?;
            Ok(())
        })
        .unwrap();
    assert!(!adapter.producer_next());
    assert_eq!(collect(&mut adapter, &mut entry, 10), vec![vec![100, 4950]]);
    assert!(adapter.is_done());
    adapter.close(&mut entry, true);
    let mut derive = FakeDerive::ranges(table.clone());
    adapter.open(&mut entry, None, &program, &mut derive, parts(&params, &NULLS, &flag), lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert!(!adapter.producer_next(), "inline: the adapter's own");
    let all = collect(&mut adapter, &mut entry, usize::MAX);
    assert_eq!(all.len() as u64, table.rows() + 1);
    assert_eq!(all.last(), Some(&vec![100, 4950]));
    adapter.close(&mut entry, true);
}

/// D4480: a run's by-reference region grows to its largest block; the run's end (`end_run`, which `PullAdapter::close`
/// calls) frees a region over `REGION_KEEP`, so a portal slot's adapter keeps at most `REGION_KEEP` between runs. A run
/// within it keeps its region, and the next runs reuse it.
#[test]
fn a_run_over_region_keep_leaves_no_region_behind() {
    let mut buf = PullBuffer::new(1, 4);
    let push = |buf: &mut PullBuffer, region: &[u8]| {
        let values = [region.as_ptr() as Datum];
        buf.push(BorrowedBatch { natts: 1, rows: 1, values: &values, isnull: &[0], region }).unwrap();
    };
    let big = vec![b'x'; 1 << 20];
    push(&mut buf, &big);
    assert!(buf.region_capacity() >= big.len(), "the block's region is copied into the buffer");
    buf.end_run();
    assert_eq!(buf.buffered(), 0);
    assert!(buf.region_capacity() <= gravity_driver::pull::REGION_KEEP, "a {} B region kept past the run", buf.region_capacity());
    let small = vec![b'y'; 100];
    push(&mut buf, &small);
    buf.end_run();
    let kept = buf.region_capacity();
    assert!(kept >= small.len() && kept <= gravity_driver::pull::REGION_KEEP, "a run within REGION_KEEP keeps its region ({kept} B)");
    for _ in 0..3 {
        push(&mut buf, &small);
        buf.end_run();
        assert_eq!(buf.region_capacity(), kept, "the next runs reuse the kept region");
    }
}
