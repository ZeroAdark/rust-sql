use gravity_driver_rig as common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::ctx::hdr;
use gravity_abi::MorselRange;
use gravity_driver::{Claimed, Entry, Error, ExecState, NodeState, Open, Step};
use gravity_program::{Generation, Handle, PlanKey, Program};

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static TRACKED: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static TOKEN: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

fn token() -> usize {
    TOKEN.with(|t| t.as_ptr() as usize)
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if token() == TRACKED.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
}

#[global_allocator]
static A: Counting = Counting;

/// Every test that opens an `Entry` runs one at a time: they all cycle the process-wide `REGIONS` depot
/// (park.rs), whose stats the depot tests assert on, and `TRACKED` is one token.
static TRACK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const KEY: PlanKey = PlanKey { handle: Handle(1), generation: Generation(1) };
const NULLS: [u8; 1] = [0];
const BLOCKS: u64 = 24;
const PER_BLOCK: u64 = 10;
const CLAIM: u64 = 5;

fn drain(open: &Open<'_>, cursor: &AtomicU64, ordinal: u16) -> Result<u64, Error> {
    let mut n = 0;
    loop {
        let lo = cursor.fetch_add(CLAIM, Ordering::AcqRel);
        if lo >= BLOCKS {
            return Ok(n);
        }
        match open.run_claim(ordinal, MorselRange::new(lo, (lo + CLAIM).min(BLOCKS)))? {
            Claimed::Done => n += 1,
            Claimed::Stopped => return Ok(n),
        }
    }
}

fn count_cycle(entry: &mut Entry, program: &Arc<Program>, derive: &mut FakeDerive, width: u16, params: &[u64], flag: &AtomicU8, threaded: bool) -> Result<Vec<Vec<u64>>, Error> {
    let mut sink = LegSink::new();
    let mut b = binding(params, &NULLS, &mut sink, flag);
    let mut exec = entry.start(Some(KEY), program, derive, &mut b, lease(gravity_driver::Grant::of(8 << 20)))?;
    let r = (|| {
        if width >= 2 {
            let open = entry.open_section(&mut exec, &b, derive, 0, width)?;
            let cursor = AtomicU64::new(0);
            let claimed = if threaded {
                std::thread::scope(|s| {
                    let (open, cursor) = (&open, &cursor);
                    let hs: Vec<_> = (0..width).map(|o| s.spawn(move || drain(open, cursor, o))).collect();
                    hs.into_iter().map(|h| h.join().unwrap()).collect::<Result<Vec<u64>, Error>>()
                })
            } else {
                (0..width).map(|o| drain(&open, &cursor, o)).collect::<Result<Vec<u64>, Error>>()
            };
            let ok = claimed.is_ok();
            entry.close_section(open, &mut b, &CountMerge, ok)?;
            claimed?;
        }
        entry.run_to_end(&mut exec, &mut b)
    })();
    entry.end(exec, r.is_ok());
    r?;
    Ok(sink.sorted())
}

#[test]
fn a_two_pipeline_program_runs_serially_on_the_leader() {
    let _g = TRACK.lock().unwrap_or_else(|e| e.into_inner());
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_count(4));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let rows = count_cycle(&mut entry, &program, &mut derive, 1, &[100], &flag, false).unwrap();
    assert_eq!(rows, vec![vec![100, (0..100).sum::<u64>()]]);
    assert_eq!(table.pins.load(Ordering::Relaxed), 0);
    assert_eq!(table.binds.load(Ordering::Relaxed), 1);
}

#[test]
fn a_section_merges_partials_in_worker_order_and_the_leader_emits() {
    let _g = TRACK.lock().unwrap_or_else(|e| e.into_inner());
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_count(4));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    for width in [2u16, 3, 4] {
        let rows = count_cycle(&mut entry, &program, &mut derive, width, &[100], &flag, false).unwrap();
        assert_eq!(rows, vec![vec![100, (0..100).sum::<u64>()]], "width {width}");
        assert_eq!(table.pins.load(Ordering::Relaxed), 0);
    }
    assert_eq!(table.binds.load(Ordering::Relaxed), 3 + 2 + 3 + 4, "the leader per cycle plus one per slot per section");
}

#[test]
fn concurrent_claims_from_std_threads_give_the_serial_answer() {
    let _g = TRACK.lock().unwrap_or_else(|e| e.into_inner());
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_count(4));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    for _ in 0..20 {
        let rows = count_cycle(&mut entry, &program, &mut derive, 4, &[1000], &flag, true).unwrap();
        assert_eq!(rows, vec![vec![BLOCKS * PER_BLOCK, (0..BLOCKS * PER_BLOCK).sum::<u64>()]]);
        assert_eq!(table.pins.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn a_window_kernel_gives_the_row_kernels_answer_at_every_width_up_to_the_slot_capacity() {
    let _g = TRACK.lock().unwrap_or_else(|e| e.into_inner());
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let rows = Arc::new(p_count(4));
    let windows = Arc::new(p_count_w(4));
    let mut entry = Entry::new(4);
    let flag = AtomicU8::new(0);
    for width in [1u16, 2, 3, 4, 8, 16, 63, 64, 96, 128, 191, gravity_driver::MAX_WIDTH] {
        let a = count_cycle(&mut entry, &rows, &mut derive, width, &[137], &flag, width > 1).unwrap();
        let b = count_cycle(&mut entry, &windows, &mut derive, width, &[137], &flag, width > 1).unwrap();
        assert_eq!(a, b, "width {width}");
        assert_eq!(b, vec![vec![137, (0..137).sum::<u64>()]]);
        assert_eq!(table.pins.load(Ordering::Relaxed), 0);
    }
    flag.store(1, Ordering::Release);
    assert_eq!(count_cycle(&mut entry, &windows, &mut derive, 2, &[137], &flag, false), Err(Error::Cancelled), "the abort byte is read once per window");
    flag.store(0, Ordering::Release);
    assert_eq!(table.pins.load(Ordering::Relaxed), 0);
}

#[test]
fn a_window_survives_a_suspend_on_the_leader() {
    let _g = TRACK.lock().unwrap_or_else(|e| e.into_inner());
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_rows(3));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    sink.stop_after.set(Some(1));
    let mut sink_ref = &sink;
    let mut b = binding(&[0], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    let mut call = gravity_driver::Call::batch(3);
    let mut suspends = 0;
    loop {
        match entry.run(&mut exec, &mut b, call).unwrap() {
            Step::Done => break,
            Step::Suspended { .. } => {
                suspends += 1;
                sink.stop_after.set(Some(sink.rows() + 1));
                assert_eq!(table.pins.load(Ordering::Relaxed), 1, "the staged block stays pinned across the suspend");
                call = gravity_driver::Call::resume(3);
            }
            Step::Recheck { .. } | Step::BuildWait { .. } => unreachable!(),
        }
    }
    entry.end(exec, true);
    assert_eq!(sink.rows() as u64, BLOCKS * PER_BLOCK, "every row once across {suspends} suspends");
    assert!(suspends as u64 >= BLOCKS * PER_BLOCK / 3 - 1);
    assert_eq!(sink.last()[0], BLOCKS * PER_BLOCK - 1, "rows arrive in order to the end");
    assert_eq!(table.pins.load(Ordering::Relaxed), 0);
}

#[test]
fn slots_are_provisioned_once_and_sections_allocate_nothing_after() {
    for program in [p_count(4), p_count_w(4)] {
        slots_cycle(Arc::new(program));
    }
}

/// The slot depot (§4.5): a program with a salt takes its worker slots from the process-wide
/// depot at `open_section` and returns them at `end`; the next Node — a fresh one, as a fresh
/// backend's would be — finds them, and the cycle allocates nothing after the first.
#[test]
fn slots_park_in_the_depot_across_nodes_and_allocate_nothing() {
    let _g = TRACK.lock().unwrap_or_else(|e| e.into_inner());
    let program = Arc::new(p_count(4));
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table);
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    const SALT: u64 = 0x5107_de90;
    let stats0 = gravity_driver::DEPOT.stats();
    let cycle = |entry: &mut Entry, derive: &mut FakeDerive, key: Option<PlanKey>| -> usize {
        let mut sink = LegSink::new();
        let mut b = binding(&[100], &NULLS, &mut sink, &flag);
        let before = ALLOCS.load(Ordering::Relaxed);
        let mut exec = entry.start(key, &program, derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
        exec.set_slot_salt(Some(SALT));
        let open = entry.open_section(&mut exec, &b, derive, 0, 3).unwrap();
        let cursor = AtomicU64::new(0);
        for o in 0..3 {
            drain(&open, &cursor, o).unwrap();
        }
        entry.close_section(open, &mut b, &CountMerge, true).unwrap();
        assert_eq!(exec.node().worker_capacity(), 3, "the slots stay with the Node until end");
        assert_eq!(entry.run(&mut exec, &mut b, gravity_driver::Call::batch(4)).unwrap(), Step::Done);
        entry.end(exec, true);
        ALLOCS.load(Ordering::Relaxed) - before
    };
    // unkeyed: the Node is torn down at end, the slots are not — the depot has them
    cycle(&mut entry, &mut derive, None);
    let s1 = gravity_driver::DEPOT.stats();
    assert_eq!((s1.puts - stats0.puts, s1.takes - stats0.takes), (1, 0), "the first cycle built its slots and parked them");
    cycle(&mut entry, &mut derive, None);
    let s2 = gravity_driver::DEPOT.stats();
    assert_eq!((s2.puts - stats0.puts, s2.takes - stats0.takes), (2, 1), "the second Node took the parked slots");
    TRACKED.store(token(), Ordering::Relaxed);
    let control = {
        let mut sink = LegSink::new();
        let before = ALLOCS.load(Ordering::Relaxed);
        let v = [100u64, 4950];
        gravity_driver::Sink::push(&mut sink, gravity_driver::BorrowedBatch { natts: 2, rows: 1, values: &v, isnull: &[0, 0], region: &[] }).unwrap();
        ALLOCS.load(Ordering::Relaxed) - before
    };
    // keyed: the leader Node parks per backend, the slots in the depot; neither allocates per statement
    cycle(&mut entry, &mut derive, Some(KEY));
    for i in 0..100 {
        let allocs = cycle(&mut entry, &mut derive, Some(KEY));
        assert_eq!(allocs, control, "cycle {i}: only the test's own sink row");
    }
    TRACKED.store(0, Ordering::Relaxed);
    let s3 = gravity_driver::DEPOT.stats();
    assert_eq!(s3.takes - stats0.takes, s3.puts - stats0.puts - 1, "every cycle after the first took what the previous parked");
    assert!(s3.bytes > 0 && s3.evictions == stats0.evictions);
}

fn slots_cycle(program: Arc<Program>) {
    let _g = TRACK.lock().unwrap_or_else(|e| e.into_inner());
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table);
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    count_cycle(&mut entry, &program, &mut derive, 4, &[100], &flag, false).unwrap();
    TRACKED.store(token(), Ordering::Relaxed);
    let control = {
        let mut sink = LegSink::new();
        let before = ALLOCS.load(Ordering::Relaxed);
        let v = [100u64, 4950];
        gravity_driver::Sink::push(&mut sink, gravity_driver::BorrowedBatch { natts: 2, rows: 1, values: &v, isnull: &[0, 0], region: &[] }).unwrap();
        ALLOCS.load(Ordering::Relaxed) - before
    };
    for i in 0..200 {
        let mut sink = LegSink::new();
        let mut b = binding(&[100], &NULLS, &mut sink, &flag);
        let before = ALLOCS.load(Ordering::Relaxed);
        let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
        assert_eq!(exec.node_mut().worker_capacity(), 4);
        let open = entry.open_section(&mut exec, &b, &mut derive, 0, 3).unwrap();
        let cursor = AtomicU64::new(0);
        for o in 0..3 {
            drain(&open, &cursor, o).unwrap();
        }
        entry.close_section(open, &mut b, &CountMerge, true).unwrap();
        assert_eq!(exec.next_pipeline(), 1);
        assert_eq!(unsafe { count_sum(exec.node_mut().slot_region(0).as_mut_ptr()) }.count(0), 100);
        assert_eq!(entry.run(&mut exec, &mut b, gravity_driver::Call::batch(4)).unwrap(), Step::Done);
        assert_eq!(b.rows_processed, 1, "the leader counts the emitted row; worker partials add none");
        entry.end(exec, true);
        assert_eq!(ALLOCS.load(Ordering::Relaxed) - before, control, "cycle {i}: only the test's own sink row");
    }
    TRACKED.store(0, Ordering::Relaxed);
}

#[test]
fn legs_take_worker_rows_and_counts_fold_into_the_leader() {
    let _g = TRACK.lock().unwrap_or_else(|e| e.into_inner());
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_rows(7));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    let open = entry.open_section(&mut exec, &b, &mut derive, 0, 4).unwrap();
    let cursor = AtomicU64::new(0);
    std::thread::scope(|s| {
        for o in 0..4 {
            let open = &open;
            let cursor = &cursor;
            s.spawn(move || drain(open, cursor, o).unwrap());
        }
    });
    entry.close_section(open, &mut b, &NoMerge, true).unwrap();
    assert_eq!(exec.state(), ExecState::Done);
    assert_eq!(b.rows_processed, BLOCKS * PER_BLOCK);
    entry.end(exec, true);
    let rows = sink.sorted();
    assert_eq!(rows.len() as u64, BLOCKS * PER_BLOCK);
    assert_eq!(rows[17], vec![17, 1]);
    assert!(sink.leg_pushes.load(Ordering::Relaxed) >= (BLOCKS * PER_BLOCK / 7));
    assert_eq!(table.pins.load(Ordering::Relaxed), 0);
}

#[test]
fn a_leg_that_stops_ends_the_claim_as_stopped() {
    let _g = TRACK.lock().unwrap_or_else(|e| e.into_inner());
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_rows(4));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    sink.stop_after = Some(8);
    let mut b = binding(&[0], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    let open = entry.open_section(&mut exec, &b, &mut derive, 0, 2).unwrap();
    assert_eq!(open.run_claim(0, MorselRange::new(0, 2)).unwrap(), Claimed::Stopped);
    assert_eq!(open.run_claim(1, MorselRange::new(2, 4)).unwrap(), Claimed::Stopped);
    entry.close_section(open, &mut b, &NoMerge, true).unwrap();
    entry.end(exec, true);
    assert_eq!(sink.rows.lock().unwrap().len(), 12, "each worker flushed one full batch and got Stop");
    assert_eq!(table.pins.load(Ordering::Relaxed), 0, "a stopped claim released its page");
}

#[test]
fn a_pin_held_at_settle_is_the_claims_error_and_the_node_stays_parkable() {
    let _g = TRACK.lock().unwrap_or_else(|e| e.into_inner());
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_count(4));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[100], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    let open = entry.open_section(&mut exec, &b, &mut derive, 0, 2).unwrap();
    table.leak.store(true, Ordering::Relaxed);
    assert_eq!(open.run_claim(0, MorselRange::new(0, 2)), Err(Error::host("pins held at claim settle")));
    table.leak.store(false, Ordering::Relaxed);
    table.fail_block.store(3, Ordering::Relaxed);
    assert_eq!(open.run_claim(1, MorselRange::new(2, 4)), Err(Error::host("range read failed")));
    table.fail_block.store(u64::MAX, Ordering::Relaxed);
    // D1408: the pin claim 0 left behind fails the close by name (the claims' own errors come first in the section's run)
    assert!(matches!(entry.close_section(open, &mut b, &CountMerge, false), Err(e) if format!("{e:?}").contains("D1408")));
    assert_eq!(exec.state(), ExecState::Failed);
    entry.end(exec, false);
    assert_eq!(entry.stats().ended_failed, 1);
    table.pins.store(0, Ordering::Relaxed);
    let rows = count_cycle(&mut entry, &program, &mut derive, 2, &[100], &flag, false).unwrap();
    assert_eq!(rows, vec![vec![100, (0..100).sum::<u64>()]]);
    assert_eq!(entry.stats().reused, 1);
}

#[test]
fn cancel_during_a_claim_is_cancelled_and_the_worker_ctx_carries_the_leaders_byte() {
    let _g = TRACK.lock().unwrap_or_else(|e| e.into_inner());
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_count(4));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[100], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    let open = entry.open_section(&mut exec, &b, &mut derive, 0, 2).unwrap();
    assert_eq!(open.run_claim(1, MorselRange::new(0, 1)).unwrap(), Claimed::Done);
    flag.store(1, Ordering::Release);
    assert_eq!(open.run_claim(0, MorselRange::new(1, 2)), Err(Error::Cancelled));
    flag.store(0, Ordering::Release);
    entry.close_section(open, &mut b, &CountMerge, false).unwrap();
    {
        let node = exec.node_mut();
        assert_eq!(node.state(), NodeState::Bound);
        let w = node.worker(1);
        assert_eq!(unsafe { *gravity_abi::ctx::cell(w.ctx(), hdr::CANCEL) }, 0, "detached slots drop the byte");
    }
    entry.end(exec, false);
    assert_eq!(table.pins.load(Ordering::Relaxed), 0);
}

/// D223: a take pops its own class or the one above (≤ 4× the ask), never a larger parked region: a 64 kB ask used to
/// take a parked 1 MB region (the D157 fallback to any larger class) and the 1 MB statement after it mapped another.
#[test]
fn a_small_ask_takes_at_most_one_class_up_from_the_depot() {
    let _g = TRACK.lock().unwrap_or_else(|e| e.into_inner());
    use gravity_driver::REGIONS;
    const ASK: usize = 64 << 10;
    // the ask's class and the one above emptied (held until the end), so only the 1 MB region is parked near it
    let mut held = Vec::new();
    for bytes in [ASK, 2 * ASK] {
        while REGIONS.take_parked(bytes, None, None).map(|r| held.push(r)).is_some() {}
    }
    let mut big = REGIONS.take(1 << 20, None).expect("unarmed pool");
    let p = big.as_mut_ptr();
    REGIONS.put(big);
    let small = REGIONS.take(ASK, None).expect("unarmed pool");
    assert!(small.len() < 4 * ASK, "a 64 kB ask took a {} byte region", small.len());
    let again = REGIONS.take(1 << 20, None).expect("unarmed pool");
    assert_eq!(again.as_ptr(), p as *const u8, "the 1 MB region stayed parked for a 1 MB ask");
    REGIONS.put(again);
    REGIONS.put(small);
    held.into_iter().for_each(|r| REGIONS.put(r));
}

/// The region depot (§4.5 as landed): a torn-down Node's breaker region goes back to the process-wide
/// depot and the next Node takes it — capacity only: a parked region's
/// bytes are whatever the last owner left (scribbled here), the open zeroes the header prefix
/// and the statement's result is the same; the byte cap holds against the least recently put regions.
#[test]
fn regions_park_in_the_depot_as_capacity_only() {
    let _g = TRACK.lock().unwrap_or_else(|e| e.into_inner());
    use gravity_driver::{REGIONS, REGION_DEPOT_BYTES};
    let program = Arc::new(p_count(4));
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table);
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let bytes = program.slots.bytes[0] as usize;
    assert!(bytes > 0, "p_count's plain-agg region");
    let cycle = |entry: &mut Entry, derive: &mut FakeDerive| -> (Vec<Vec<u64>>, Vec<u8>) {
        let mut sink = LegSink::new();
        let mut b = binding(&[100], &NULLS, &mut sink, &flag);
        let mut exec = entry.start(None, &program, derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
        let open = entry.open_section(&mut exec, &b, derive, 0, 2).unwrap();
        let cursor = AtomicU64::new(0);
        for o in 0..2 {
            drain(&open, &cursor, o).unwrap();
        }
        entry.close_section(open, &mut b, &CountMerge, true).unwrap();
        let region = exec.node().slot_region(0);
        let prefix = region[..gravity_driver::node::ZERO_PREFIX.min(region.len())].to_vec();
        assert_eq!(entry.run(&mut exec, &mut b, gravity_driver::Call::batch(4)).unwrap(), Step::Done);
        assert!(exec.node().slot_region(0).is_empty(), "the life ended with its last consumer: its region is in the free list");
        entry.end(exec, true);
        (sink.sorted(), prefix)
    };
    let s0 = REGIONS.stats();
    let (rows, prefix1) = cycle(&mut entry, &mut derive);
    assert_eq!(rows, vec![vec![100, 4950]]);
    let s1 = REGIONS.stats();
    assert!(s1.puts > s0.puts, "the torn-down Node parked its region");
    // scribble every parked region of the class: what a previous statement left is never read
    let mut taken = Vec::new();
    // (parked ones only: another test's regions of other classes left in the depot made the old loop map fresh ones and count them)
    while let Some(mut r) = REGIONS.take_parked(bytes, None, None) {
        r.fill(0xAB);
        taken.push(r);
        if taken.len() > 64 {
            break;
        }
    }
    let n = taken.len();
    taken.into_iter().for_each(|r| REGIONS.put(r));
    let (rows, prefix2) = cycle(&mut entry, &mut derive);
    assert_eq!(rows, vec![vec![100, 4950]], "a scribbled region gives the same answer");
    assert_eq!(prefix2, prefix1, "the header prefix is re-derived from zero, not the scribble");
    let s2 = REGIONS.stats();
    assert!(s2.takes > s1.takes + n as u64, "the second Node took a parked region");
    let big = 3 << 20;
    let mut r = REGIONS.take(big, None).expect("unarmed pool");
    assert_eq!(r.len(), big, "a region is its bytes page-rounded, never its power-of-two class (D157)");
    let p = r.as_ptr();
    r[..64].fill(0xCD);
    REGIONS.put(r);
    let again = REGIONS.take(big, None).expect("unarmed pool");
    assert_eq!(again.as_ptr(), p, "the class's parked region is the one taken");
    assert_eq!(again[0], 0xCD, "capacity only: the depot does not zero");
    // the cap: the least recently put regions drop first, never the region just put (three regions past half the cap: a
    // region is its bytes, D157); the small region put after them stays (D2869: no longer the largest first)
    let s3 = REGIONS.stats();
    let large: Vec<gravity_driver::RegionBuf> = (0..3).map(|_| REGIONS.take(REGION_DEPOT_BYTES / 2 + 1, None).expect("unarmed pool")).collect();
    large.into_iter().for_each(|r| REGIONS.put(r));
    REGIONS.put(again);
    let s4 = REGIONS.stats();
    assert!(s4.bytes <= REGION_DEPOT_BYTES, "over the cap: {} bytes parked", s4.bytes);
    assert!(s4.evictions > s3.evictions);
    let small = REGIONS.take(big, None).expect("unarmed pool");
    assert_eq!(small.as_ptr(), p, "the region put last survived the eviction of the large ones put before it");
    REGIONS.put(small);
}

/// A parked slot's program block names the hosted owner of the program INSTANCE that built it (`prog::OWNER`, the
/// lowerer's host state, dropped with the instance): the next Node — a fresh instance of the same program, as a new
/// session's is — re-points every depot slot at its own owner at `provision`, and a released slot names none (q28 on
/// hits: `h_enter` over the previous session's freed owner, SIGSEGV on both ISAs).
#[test]
fn depot_slots_take_the_current_programs_owner_at_provision() {
    let _g = TRACK.lock().unwrap_or_else(|e| e.into_inner());
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table);
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    const SALT: u64 = 0x0000_04e0_0000_0001;
    let owner_of = |p: &Program| p.owner.as_deref().map_or(core::ptr::null_mut(), |o| o as *const dyn core::any::Any as *const u8 as *mut u8);
    let mut cycle = |program: &Arc<Program>| {
        let mut sink = LegSink::new();
        let mut b = binding(&[100], &NULLS, &mut sink, &flag);
        let mut exec = entry.start(None, program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
        exec.set_slot_salt(Some(SALT));
        let open = entry.open_section(&mut exec, &b, &mut derive, 0, 3).unwrap();
        let cursor = AtomicU64::new(0);
        for o in 0..3 {
            drain(&open, &cursor, o).unwrap();
        }
        entry.close_section(open, &mut b, &CountMerge, true).unwrap();
        let want = owner_of(program);
        assert!(!want.is_null());
        for o in 0..3 {
            assert_eq!(exec.node().worker(o).owner(), want, "worker {o} names the current instance's owner");
        }
        assert_eq!(entry.run(&mut exec, &mut b, gravity_driver::Call::batch(4)).unwrap(), Step::Done);
        entry.end(exec, true);
    };
    let mut p1 = p_count(4);
    p1.owner = Some(std::rc::Rc::new(1u32));
    let p1 = Arc::new(p1);
    cycle(&p1);
    drop(p1);
    let mut p2 = p_count(4);
    p2.owner = Some(std::rc::Rc::new(2u32));
    let p2 = Arc::new(p2);
    cycle(&p2);
    cycle(&p2);
}
