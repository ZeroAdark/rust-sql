use gravity_driver_rig as common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::{install_spill_factory, BorrowedBatch, Call, Claimed, Entry, Error, Flow, Grant, Open, Sink, Step, SHARE_FLOOR};

/// Counts rows without storing them: the sink's storage is not the driver's steady state.
struct CountSink(usize);

impl Sink for CountSink {
    fn push(&mut self, batch: BorrowedBatch<'_>) -> Result<Flow, Error> {
        self.0 += batch.rows;
        Ok(Flow::Continue)
    }
}
use gravity_abi::breaker::{HashAgg};
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

const KEY: PlanKey = PlanKey { handle: Handle(9), generation: Generation(1) };
const NULLS: [u8; 2] = [0, 0];
const BLOCKS: u64 = 400;
const PER_BLOCK: u64 = 50;
const CLAIM: u64 = 7;
const MIB: u64 = 1 << 20;

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The tests share the process-wide file counters: one at a time.
fn install() -> std::sync::MutexGuard<'static, ()> {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        install_spill_factory(&MEM_SPILL);
    });
    SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn round_robin(open: &Open<'_>, width: u16) -> Result<(), Error> {
    let cursor = AtomicU64::new(0);
    'claims: loop {
        for o in 0..width {
            let lo = cursor.fetch_add(CLAIM, Ordering::AcqRel);
            if lo >= BLOCKS {
                break 'claims;
            }
            if open.run_claim(o, MorselRange::new(lo, (lo + CLAIM).min(BLOCKS)))? == Claimed::Stopped {
                break 'claims;
            }
        }
    }
    Ok(())
}

/// One execution at `width` under `grant`: the sorted groups and the spill census.
fn groups(entry: &mut Entry, program: &Arc<Program>, derive: &mut FakeDerive, grant: Grant, width: u16, modulus: u64) -> Result<(Vec<Vec<u64>>, bool, u32, u64), Error> {
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let params = [modulus, 3];
    let mut b = binding(&params, &NULLS, &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), program, derive, &mut b, lease(grant))?;
    let r: Result<_, Error> = (|| {
        if width >= 2 {
            let open = entry.open_section(&mut exec, &b, derive, 0, width)?;
            let r = round_robin(&open, width);
            entry.close_section(open, &mut b, &GroupMerge, r.is_ok())?;
            r?;
        } else {
            assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(64))?, Step::Done);
        }
        let node = exec.node();
        let census = (node.spilled(), node.spill_parts(), node.spilled_bytes());
        assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64))?, Step::Done);
        Ok(census)
    })();
    let ok = r.is_ok();
    entry.end(exec, ok);
    let (spilled, parts, bytes) = r?;
    Ok((sink.sorted(), spilled, parts, bytes))
}

#[test]
fn a_four_mb_grant_forces_scatter_and_refill_and_the_groups_are_identical() {
    let _serial = install();
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_group_spill(64));
    let mut entry = Entry::new(2);
    let modulus = 6000u64;
    // unspilled: a 64 MB grant holds 6000 groups everywhere
    let (want, spilled, parts, bytes) = groups(&mut entry, &program, &mut derive, Grant::of(64 * MIB), 1, modulus).unwrap();
    assert!(!spilled && parts == 0 && bytes == 0, "64 MB: nothing spilled");
    assert_eq!(want.len() as u64, modulus);
    assert_eq!(want.iter().map(|r| r[1]).sum::<u64>(), BLOCKS * PER_BLOCK);
    assert_eq!(SPILL_FILES_LIVE.load(Ordering::Relaxed), 0);
    // a 1-byte grant floors every share at 64 kB: the 32 B records overflow a 1024-record table
    // at 896 groups, so the worker scatters into 8 partitions and the emit refills them one by one
    for w in [1u16, 4] {
        let (got, spilled, parts, bytes) = groups(&mut entry, &program, &mut derive, Grant::of(1), w, modulus).unwrap();
        assert_eq!(got, want, "width {w}: identical groups through scatter + refill");
        assert!(spilled && parts == 8, "width {w}: spilled into 8 partitions ({parts})");
        assert!(bytes >= 6000 * 32, "width {w}: at least every group's record hit the file ({bytes} B)");
        assert_eq!((SPILL_FILES_LIVE.load(Ordering::Relaxed), SPILL_SETS_LIVE.load(Ordering::Relaxed)), (0, 0), "width {w}: every temp file and the set removed at end");
    }
    let epochs = SPILL_EPOCHS.load(Ordering::Relaxed);
    assert!(epochs >= 2, "{epochs} epochs written");
    let _ = SHARE_FLOOR;
}

/// D165: a slot's spill stream dies with its breaker. Two scans fold into slot 0 one after the other (the second once the first's
/// emit freed the slot), both spilled at one partition count and record size: the second emit's groups are its own scan's, never
/// the first's records refilled with them (the stream kept the first's files and the second appended to them: every count doubled).
#[test]
fn a_slot_produced_again_after_its_spill_starts_a_stream_of_its_own() {
    let _serial = install();
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let one = Arc::new(p_group_spill(64));
    let two = Arc::new(p_two_group_spills(64));
    let mut entry = Entry::new(2);
    let modulus = 6000u64;
    let (want, spilled, _, _) = groups(&mut entry, &one, &mut derive, Grant::of(1), 1, modulus).unwrap();
    assert!(spilled, "the one scan spilled");
    let (got, spilled, _, _) = groups(&mut entry, &two, &mut derive, Grant::of(1), 1, modulus).unwrap();
    assert!(spilled, "both scans spilled");
    let mut twice: Vec<Vec<u64>> = want.iter().chain(want.iter()).cloned().collect();
    twice.sort();
    assert_eq!(got, twice, "each emit returns its own scan's groups");
    assert_eq!((SPILL_FILES_LIVE.load(Ordering::Relaxed), SPILL_SETS_LIVE.load(Ordering::Relaxed)), (0, 0), "every temp file and the set removed at end");
}

#[test]
fn temp_files_are_removed_on_error_and_on_cancel() {
    let _serial = install();
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_group_spill(64));
    let mut entry = Entry::new(2);
    // cancel: the abort byte set after the first claims scattered
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let params = [6000u64, 3];
    let mut b = binding(&params, &NULLS, &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(1))).unwrap();
    let open = entry.open_section(&mut exec, &b, &mut derive, 0, 2).unwrap();
    for lo in (0..BLOCKS / 2).step_by(CLAIM as usize) {
        open.run_claim((lo / CLAIM % 2) as u16, MorselRange::new(lo, (lo + CLAIM).min(BLOCKS))).unwrap();
    }
    assert!(SPILL_FILES_LIVE.load(Ordering::Relaxed) > 0, "the half scan spilled");
    flag.store(1, Ordering::Release);
    let r = open.run_claim(0, MorselRange::new(BLOCKS / 2, BLOCKS / 2 + CLAIM));
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    assert!(entry.close_section(open, &mut b, &GroupMerge, false).is_ok());
    entry.end(exec, false);
    assert_eq!((SPILL_FILES_LIVE.load(Ordering::Relaxed), SPILL_SETS_LIVE.load(Ordering::Relaxed)), (0, 0), "cancel: files and set removed");
    // error: a worker error (the fixture's fail block) after a spill
    table.fail_block.store(BLOCKS - 3, Ordering::Relaxed);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&params, &NULLS, &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(1))).unwrap();
    let r = entry.run_pipeline(&mut exec, &mut b, Call::batch(64));
    assert!(r.is_err(), "the fail block errors the scan: {r:?}");
    entry.end(exec, false);
    table.fail_block.store(u64::MAX, Ordering::Relaxed);
    assert_eq!((SPILL_FILES_LIVE.load(Ordering::Relaxed), SPILL_SETS_LIVE.load(Ordering::Relaxed)), (0, 0), "error: files and set removed");
    // fail-closed: a process without temp file access refuses the spill instead of overrunning
    SPILL_ALLOW.store(false, Ordering::Relaxed);
    let r = groups(&mut entry, &program, &mut derive, Grant::of(1), 1, 6000);
    SPILL_ALLOW.store(true, Ordering::Relaxed);
    assert!(matches!(r, Err(Error::Host(_))), "{r:?}");
    assert_eq!(SPILL_SETS_LIVE.load(Ordering::Relaxed), 0);
}

#[test]
fn a_spilled_execution_allocates_only_its_spill_buffers_in_steady_state() {
    let _serial = install();
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_group_spill(64));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let params = [6000u64, 3];
    // warm: the Node, its workers, the share regions
    for _ in 0..2 {
        groups(&mut entry, &program, &mut derive, Grant::of(1), 4, 6000).unwrap();
    }
    let mut sink = CountSink(0);
    TRACKED.store(token(), Ordering::Relaxed);
    let before = ALLOCS.load(Ordering::Relaxed);
    let mut b = binding(&params, &NULLS, &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(1))).unwrap();
    let open = entry.open_section(&mut exec, &b, &mut derive, 0, 4).unwrap();
    let after_open = ALLOCS.load(Ordering::Relaxed);
    round_robin(&open, 4).unwrap();
    entry.close_section(open, &mut b, &GroupMerge, true).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    entry.end(exec, true);
    let total = ALLOCS.load(Ordering::Relaxed) - before;
    TRACKED.store(0, Ordering::Relaxed);
    assert_eq!(sink.0, 6000, "every group reached the sink");
    assert_eq!(after_open - before, 0, "opening the section allocates nothing on a warm Node");
    // the set and the five files (each nine pre-sized buffers) are the spill buffers, created per
    // execution by design; the driver's own path (scatter's counting sort, refill's carry) adds none
    assert!(total <= 1 + 5 * 10, "{total} allocations in a spilled execution: only the spill buffers");
    let _ = HashAgg::cap_of;
}

/// One execution under `grant` with `bits` generation-0 partitions: the sorted groups and the generations reached.
fn groups_bits(entry: &mut Entry, program: &Arc<Program>, derive: &mut FakeDerive, grant: Grant, width: u16, modulus: u64, bits: u64) -> Result<(Vec<Vec<u64>>, u32, u8), Error> {
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let params = [modulus, bits];
    let mut b = binding(&params, &NULLS, &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), program, derive, &mut b, lease(grant))?;
    let r: Result<_, Error> = (|| {
        if width >= 2 {
            let open = entry.open_section(&mut exec, &b, derive, 0, width)?;
            let r = round_robin(&open, width);
            entry.close_section(open, &mut b, &GroupMerge, r.is_ok())?;
            r?;
        } else {
            assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(64))?, Step::Done);
        }
        assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64))?, Step::Done);
        let node = exec.node();
        Ok((node.spill_parts(), node.spill_generations()))
    })();
    let ok = r.is_ok();
    entry.end(exec, ok);
    let (parts, gens) = r?;
    Ok((sink.sorted(), parts, gens))
}

#[test]
fn a_partition_too_big_for_the_table_splits_into_a_second_generation_on_refill() {
    let _serial = install();
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_group_spill(64));
    let mut entry = Entry::new(2);
    let (want, parts, gens) = groups_bits(&mut entry, &program, &mut derive, Grant::of(64 * MIB), 1, 6000, 1).unwrap();
    assert_eq!((want.len(), parts, gens), (6000, 0, 0));
    // two generation-0 partitions of ~3000 groups each never fit a 1024-record table: the refill
    // of each splits it into 8 sub-partitions (generation 2) that do; the queue grows to 2 + 16
    for w in [1u16, 4] {
        let (got, parts, gens) = groups_bits(&mut entry, &program, &mut derive, Grant::of(1), w, 6000, 1).unwrap();
        assert_eq!(got, want, "width {w}: identical groups through a split refill");
        assert_eq!((parts, gens), (2 + 16, 2), "width {w}: both partitions split once");
        assert_eq!((SPILL_FILES_LIVE.load(Ordering::Relaxed), SPILL_SETS_LIVE.load(Ordering::Relaxed)), (0, 0), "width {w}: every file removed");
    }
    // eight partitions of 750 fit: one generation
    let (got, parts, gens) = groups_bits(&mut entry, &program, &mut derive, Grant::of(1), 2, 6000, 3).unwrap();
    assert_eq!((got == want, parts, gens), (true, 8, 1));
}

/// The example's Entry path (`examples/scale.rs`, `Entry::start(None, …)` → `Entry::end` →
/// `Node::teardown`): a Node torn down right after a spilled execution drops every file and the
/// set — its `SpillTable` must drop before the arena that holds it (a field-order use-after-free
/// segfaulted the example on Linux/arm64 in `Node::teardown`).
#[test]
fn teardown_after_a_spilled_execution_drops_the_files_and_the_set() {
    let _serial = install();
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_group_spill(64));
    let mut entry = Entry::new(2);
    for w in [1u16, 4] {
        let flag = AtomicU8::new(0);
        let mut sink = LegSink::new();
        let params = [6000u64, 3];
        let mut b = binding(&params, &NULLS, &mut sink, &flag);
        // no plan key: `end` tears the Node down instead of parking it (the example's path)
        let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(Grant::of(1))).unwrap();
        if w >= 2 {
            let open = entry.open_section(&mut exec, &b, &mut derive, 0, w).unwrap();
            round_robin(&open, w).unwrap();
            entry.close_section(open, &mut b, &GroupMerge, true).unwrap();
        } else {
            assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
        }
        assert!(exec.node().spilled() && SPILL_FILES_LIVE.load(Ordering::Relaxed) > 0, "width {w}: files live before the teardown");
        assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
        entry.end(exec, true);
        assert_eq!((SPILL_FILES_LIVE.load(Ordering::Relaxed), SPILL_SETS_LIVE.load(Ordering::Relaxed)), (0, 0), "width {w}: the teardown dropped every file and the set");
        assert_eq!(sink.sorted().len(), 6000, "width {w}");
    }
    assert_eq!(entry.stats().built, 2, "a Node per execution: none parked");
}

/// A spool-encoded row without images: the length word, the values, one null word.
fn stored_row(values: &[u64]) -> Vec<u8> {
    let mut out = ((values.len() as u64) + 2).to_ne_bytes().to_vec();
    for v in values {
        out.extend(v.to_ne_bytes());
    }
    out.extend(0u64.to_ne_bytes());
    out
}

#[test]
fn a_build_with_a_row_store_spills_and_refills_its_rows_with_the_partition() {
    use gravity_abi::breaker::{HashBuild, BUILD_PARTITION, BUILD_TABLE};
    use gravity_abi::buildrows::RowStore;
    use gravity_driver::{refill_build, scatter_build, SpillFactory, SpillSet};
    let _g = install();
    let mut set = MemSpillFactory.create().unwrap();
    const BITS: u8 = 2;
    let total = 64usize << 10;
    let mut region = vec![0u64; total / 8];
    let p = region.as_mut_ptr() as *mut u8;
    unsafe { HashAgg::arm(p, total as u32) };
    let store = unsafe { RowStore::carve(p, 32, 1 << BITS) }.expect("room");
    let build = unsafe { HashBuild::at_share(p, BUILD_PARTITION) };
    let mut rows = Vec::new();
    for k in 0..200i32 {
        let r = stored_row(&[k as u64, k as u64 * 10]);
        let off = store.append(store.part_for(k), &r).expect("room");
        assert!(build.insert(k, off as i64));
        rows.push((k, r));
    }
    let mut file = set.open("agg", 0, 0, 2 << BITS).unwrap();
    assert_eq!(scatter_build(build, 0, BITS, file.as_mut(), None).unwrap(), 200);
    assert_eq!((build.len, unsafe { RowStore::of(p) }.unwrap().len), (0, 0), "both cleared");
    let mut refilled = 0u64;
    // the region re-armed at the entries' share (the store stays where `pad2` says) and laid out as the refill's table
    let store_off = unsafe { HashBuild::view(p) }.pad2;
    unsafe {
        let b = &mut *(p as *mut HashBuild);
        b.cap = 0;
        b.table = store_off;
    }
    for part in 0..1u32 << BITS {
        let table = unsafe { HashBuild::at_share(p, BUILD_TABLE) };
        assert_eq!((table.pad2, table.region), (store_off, store_off));
        let mut overflow = None;
        let n = refill_build(table, part, file.as_ref(), 1 << BITS, &mut |_| panic!("no overflow"), &mut overflow).unwrap();
        refilled += n;
        let s = unsafe { RowStore::of(p) }.unwrap();
        assert_eq!(s.len, 0, "a refilled slice counts no rows; its entries carry the offsets");
        for (k, r) in rows.iter().filter(|(k, _)| HashBuild::partition_of(*k, BITS) == part) {
            let e = table.probe(*k).next().expect("the key refilled");
            assert_eq!(s.at(e.payload as u32), &r[..], "row {k} of partition {part}");
        }
        unsafe { gravity_driver::clear_region(p) };
        assert_eq!(unsafe { RowStore::of(p) }.unwrap().part_range(part as u16).1 - unsafe { RowStore::of(p) }.unwrap().part_range(part as u16).0, 8, "clear_region clears the store's slices too");
    }
    assert_eq!(refilled, 200);
}

/// D219: two scatters of a stored-row build into one owner's file (the build filled, spilled, filled again, spilled — a
/// build past its grant twice), refilled per partition into a store of the same shape: every entry reads its own row. Before
/// the fix the refill read one origin word per file partition, so the second scatter's origin word landed among the rows
/// and its entries were rebased against the first scatter's: the join read another key's row (wrong rows, no error).
#[test]
fn d219_a_stored_build_scattered_twice_into_one_file_refills_every_entry_onto_its_own_row() {
    use gravity_abi::breaker::{HashBuild, BUILD_PARTITION, BUILD_TABLE};
    use gravity_abi::buildrows::RowStore;
    use gravity_driver::{refill_build, scatter_build, SpillFactory, SpillSet};
    let _g = install();
    let mut set = MemSpillFactory.create().unwrap();
    const BITS: u8 = 2;
    let total = 64usize << 10;
    let mut region = vec![0u64; total / 8];
    let p = region.as_mut_ptr() as *mut u8;
    unsafe { HashAgg::arm(p, total as u32) };
    let store = unsafe { RowStore::carve(p, 32, 1 << BITS) }.expect("room");
    let build = unsafe { HashBuild::at_share(p, BUILD_PARTITION) };
    let mut file = set.open("agg", 0, 0, 2 << BITS).unwrap();
    let mut rows = Vec::new();
    for scatter in 0..2i32 {
        for k in scatter * 150..scatter * 150 + 150 {
            // the second scatter's rows are wider: a row read at a stale base is another key's, or straddles two
            let r = if scatter == 0 { stored_row(&[k as u64]) } else { stored_row(&[k as u64, 7, k as u64 * 3]) };
            let off = store.append(store.part_for(k), &r).expect("room");
            assert!(build.insert(k, off as i64));
            rows.push((k, r));
        }
        assert_eq!(scatter_build(build, 0, BITS, file.as_mut(), None).unwrap(), 150);
    }
    let mut region2 = vec![0u64; total / 8];
    let q = region2.as_mut_ptr() as *mut u8;
    let mut wrong = Vec::new();
    for part in 0..1u32 << BITS {
        region2.iter_mut().for_each(|w| *w = 0);
        unsafe { HashAgg::arm(q, total as u32) };
        assert!(unsafe { RowStore::carve(q, 32, 1 << BITS) }.is_some(), "the build's own store shape");
        let table = unsafe { HashBuild::at_share(q, BUILD_TABLE) };
        let mut overflow = None;
        refill_build(table, part, file.as_ref(), 1 << BITS, &mut |_| panic!("no overflow"), &mut overflow).unwrap();
        let s = unsafe { RowStore::of(q) }.unwrap();
        for (k, r) in rows.iter().filter(|(k, _)| HashBuild::partition_of(*k, BITS) == part) {
            let e = table.probe(*k).next().expect("the key refilled");
            let (lo, hi) = s.part_range(part as u16);
            let off = e.payload as u32;
            // read only inside the partition's slice: a stale offset may name no row at all
            let fits = off >= lo && off + 8 <= hi && { off as u64 + unsafe { (s.at_ptr(off) as *const u64).read_unaligned() } * 8 <= hi as u64 };
            let got = if fits { s.at(off).to_vec() } else { Vec::new() };
            if got != *r {
                wrong.push(*k);
            }
        }
    }
    assert!(wrong.is_empty(), "{} of 300 entries read another row (first keys {:?})", wrong.len(), &wrong[..wrong.len().min(8)]);
}

/// Two epochs in one owner's file (the build filled, scattered, filled again, scattered) and a refill into the part
/// phase's one-slice store: every entry finds its own row. Before the marker entries the refill read one origin word
/// per file partition, so the second epoch's origin landed among the rows and its entries pointed at the first's.
#[test]
fn a_stored_build_scattered_twice_refills_every_row_into_a_one_slice_store() {
    use gravity_abi::breaker::{HashBuild, BUILD_PARTITION, BUILD_TABLE};
    use gravity_abi::buildrows::RowStore;
    use gravity_driver::{refill_build, scatter_build, SpillFactory, SpillSet};
    let _g = install();
    let mut set = MemSpillFactory.create().unwrap();
    const BITS: u8 = 2;
    let total = 64usize << 10;
    let mut region = vec![0u64; total / 8];
    let p = region.as_mut_ptr() as *mut u8;
    unsafe { HashAgg::arm(p, total as u32) };
    let store = unsafe { RowStore::carve(p, 32, 1 << BITS) }.expect("room");
    let build = unsafe { HashBuild::at_share(p, BUILD_PARTITION) };
    let mut file = set.open("agg", 0, 0, 2 << BITS).unwrap();
    let mut rows = Vec::new();
    for epoch in 0..2i32 {
        for k in epoch * 150..epoch * 150 + 150 {
            // the second epoch's rows are wider: a stale origin or base would misread them
            let r = if epoch == 0 { stored_row(&[k as u64]) } else { stored_row(&[k as u64, 7, k as u64 * 3]) };
            let off = store.append(store.part_for(k), &r).expect("room");
            assert!(build.insert(k, off as i64));
            rows.push((k, r));
        }
        assert_eq!(scatter_build(build, 0, BITS, file.as_mut(), None).unwrap(), 150);
    }
    let mut region2 = vec![0u64; total / 8];
    let q = region2.as_mut_ptr() as *mut u8;
    let mut refilled = 0u64;
    for part in 0..1u32 << BITS {
        region2.iter_mut().for_each(|w| *w = 0);
        unsafe { HashAgg::arm(q, total as u32) };
        assert!(unsafe { RowStore::carve(q, 32, 1) }.is_some(), "a one-slice store");
        let table = unsafe { HashBuild::at_share(q, BUILD_TABLE) };
        let mut overflow = None;
        refilled += refill_build(table, part, file.as_ref(), 1 << BITS, &mut |_| panic!("no overflow"), &mut overflow).unwrap();
        let s = unsafe { RowStore::of(q) }.unwrap();
        for (k, r) in rows.iter().filter(|(k, _)| HashBuild::partition_of(*k, BITS) == part) {
            let e = table.probe(*k).next().expect("the key refilled");
            assert_eq!(s.at(e.payload as u32), &r[..], "row {k} of partition {part}");
        }
    }
    assert_eq!(refilled, 300, "the markers are not entries");
}

/// S5: a spill stream is its breaker life's. A new life of a slot starts with an empty stream (the D165 class: an earlier
/// breaker's files are never the new one's), a life's end drops its files and keeps only the census, and a consumer's read of
/// a stream no live life owns is an error, never another life's records.
#[test]
fn a_stream_is_owned_by_its_life_and_a_new_life_starts_empty() {
    use gravity_driver::SpillTable;
    let _g = install();
    let mut t = SpillTable::with_slots(2);
    assert!(t.live_stream(0).is_err(), "no life yet: no stream to read");
    let first = t.begin_life(0);
    t.file(0, 0, 3).unwrap().write_epoch(&[(1, &[7u8; 64])]).unwrap();
    assert!(t.stream(0).spilled() && t.live_stream(0).is_ok());
    assert_eq!(t.census_of(0), (8, 1));
    t.end_life(0);
    assert!(!t.stream(0).spilled(), "the ended life's files are dropped with it");
    assert!(t.live_stream(0).is_err(), "a read after the life ended is an error");
    assert!(t.spilled() && t.census_of(0) == (8, 1), "the census keeps what the ended life reached");
    assert_eq!(t.live_parts(0), 0, "D546: an ended life's parts never answer for the slot's next life");
    let second = t.begin_life(0);
    assert_eq!(t.live_parts(0), 0, "D546: a new life that has not spilled has no parts, whatever the census holds");
    assert_ne!(first, second);
    assert!(!t.stream(0).spilled() && t.stream(0).life == second, "the new life's stream is empty");
    // a new life begun while the old one still spilled (the D165 order) drops the old files too
    t.file(0, 0, 3).unwrap().write_epoch(&[(2, &[9u8; 64])]).unwrap();
    let third = t.begin_life(0);
    assert!(!t.stream(0).spilled() && t.stream(0).life == third);
    assert!(t.live_stream(1).is_err(), "the other slot has no life");
    t.clear();
    assert!(!t.spilled() && t.live_stream(0).is_err());
}

/// D126: a stored partition too big for the part phase's one-slice table splits on refill. The overflow scatters the table
/// and its rows at the next hash window (the rows copied out per sub-partition, every payload rebased), the rest of the
/// partition refills behind it and splits the same way; every sub-partition then refills alone and finds each of its rows.
#[test]
fn a_stored_partition_past_the_part_table_splits_on_refill_and_every_row_comes_back() {
    use gravity_abi::breaker::{HashBuild, BUILD_PARTITION, BUILD_TABLE};
    use gravity_abi::buildrows::RowStore;
    use gravity_driver::{refill_build, scatter_build, SpillFactory, SpillSet, SPLIT_BITS};
    let _g = install();
    let mut set = MemSpillFactory.create().unwrap();
    const BITS: u8 = 1;
    let total = 256usize << 10;
    let mut region = vec![0u64; total / 8];
    let p = region.as_mut_ptr() as *mut u8;
    unsafe { HashAgg::arm(p, total as u32) };
    let store = unsafe { RowStore::carve(p, 48, 1 << BITS) }.expect("room");
    let build = unsafe { HashBuild::at_share(p, BUILD_PARTITION) };
    let mut file = set.open("agg", 0, 0, 2 << BITS).unwrap();
    let mut rows = Vec::new();
    for k in 0..1500i32 {
        let r = if k % 3 == 0 { stored_row(&[k as u64, 7, k as u64 * 3, 9]) } else { stored_row(&[k as u64]) };
        let off = store.append(store.part_for(k), &r).expect("room");
        assert!(build.insert(k, off as i64));
        rows.push((k, r));
    }
    assert_eq!(scatter_build(build, 0, BITS, file.as_mut(), None).unwrap(), 1500);
    // the part table: 16 KiB, a quarter of a partition's rows
    let small = 16usize << 10;
    let mut region2 = vec![0u64; small / 8];
    let q = region2.as_mut_ptr() as *mut u8;
    let lay = |q: *mut u8| unsafe {
        core::ptr::write_bytes(q, 0, small);
        HashAgg::arm(q, small as u32);
        assert!(RowStore::carve(q, 48, 1).is_some(), "a one-slice store");
        HashBuild::at_share(q, BUILD_TABLE)
    };
    let (mut seen, mut splits) = (0usize, 0usize);
    for part in 0..1u32 << BITS {
        let mut split = set.open("agg", 1, part as usize, (1 << SPLIT_BITS) << 1).unwrap();
        let table = lay(q);
        let mut overflow = None;
        let n = refill_build(table, part, file.as_ref(), 1 << BITS, &mut |r| { scatter_build(unsafe { &mut *(r as *mut HashBuild) }, BITS, SPLIT_BITS, split.as_mut(), None).map(|_| ()) }, &mut overflow).unwrap();
        assert!(overflow.is_some(), "partition {part} overflows the part table");
        splits += 1;
        assert!(n > 0);
        scatter_build(table, BITS, SPLIT_BITS, split.as_mut(), None).unwrap();
        for sub in 0..1u32 << SPLIT_BITS {
            let table = lay(q);
            refill_build(table, sub, split.as_ref(), 1 << SPLIT_BITS, &mut |_| panic!("a sub-partition fits"), &mut None).unwrap();
            let s = unsafe { RowStore::of(q) }.unwrap();
            for (k, r) in rows.iter().filter(|(k, _)| HashBuild::partition_of(*k, BITS) == part && (HashBuild::key_hash(*k) >> BITS) & ((1 << SPLIT_BITS) - 1) == sub) {
                let e = table.probe(*k).next().unwrap_or_else(|| panic!("key {k} in sub-partition {sub} of {part}"));
                assert_eq!(s.at(e.payload as u32), &r[..], "row {k}");
                seen += 1;
            }
            assert_eq!(table.len as usize, rows.iter().filter(|(k, _)| HashBuild::partition_of(*k, BITS) == part && (HashBuild::key_hash(*k) >> BITS) & ((1 << SPLIT_BITS) - 1) == sub).count());
        }
    }
    assert_eq!((seen, splits), (1500, 2));
}

/// M7-SPILL B4c: a stored file holds a drain part (`2 << bits ..`, past the kept probe rows) at the largest bits too,
/// reads it back whole, and `clear` drops it with the file.
#[test]
fn a_stored_file_holds_its_drain_parts_and_clear_drops_them() {
    use gravity_driver::SpillTable;
    let _g = install();
    let live = SPILL_FILES_LIVE.load(Ordering::Relaxed);
    // 5: the build's bits; 6: the most a `SPILLED` mask (one u64 bit per partition) allows
    for bits in [5u8, 6] {
        let mut t = SpillTable::with_slots(1);
        t.begin_life(0);
        let (kept, drain) = ((1u32 << bits) + 3, (2u32 << bits) + 3);
        t.stage_put(0, 0, bits, kept, &[1u8; 24]).unwrap();
        t.stage_put(0, 0, bits, drain, &[2u8; 40]).unwrap();
        t.stage_put(0, 0, bits, (3u32 << bits) - 1, &[3u8; 8]).unwrap();
        t.stage_flush(0, 0, bits).unwrap();
        assert_eq!(SPILL_FILES_LIVE.load(Ordering::Relaxed), live + 1, "bits {bits}: one stored file");
        let read = |t: &SpillTable, part: u32| {
            let mut r = t.open_stored(0, 0, part).unwrap().expect("the part holds bytes");
            let (mut out, mut buf) = (Vec::new(), [0u8; 16]);
            loop {
                let n = r.read(&mut buf).unwrap();
                if n == 0 {
                    break out;
                }
                out.extend_from_slice(&buf[..n]);
            }
        };
        assert_eq!(read(&t, drain), vec![2u8; 40], "bits {bits}: the drain part reads back whole");
        assert_eq!(read(&t, kept), vec![1u8; 24], "bits {bits}: the kept rows stay their own part");
        assert!(t.open_stored(0, 0, drain + 1).unwrap().is_none(), "bits {bits}: an unwritten drain part is empty");
        t.clear();
        assert_eq!(SPILL_FILES_LIVE.load(Ordering::Relaxed), live, "bits {bits}: clear dropped the file and its drain part");
        assert!(!t.spilled() && t.open_stored(0, 0, drain).unwrap().is_none());
    }
}

/// CONVERSION WAVE 2: a stored build's scatter (`scatter_stored`) charges its entry stream (16 B an entry, the scatter's
/// working memory) to the build slot's ledger owner it is given, by construction: the owner's held, read from inside the
/// scatter's own epoch writes, covers the stream; after the scatter the owner holds nothing of it. Without an owner
/// nothing is charged.
#[test]
fn wave2_a_stored_builds_scatter_charges_its_entry_stream_to_the_slot_owner() {
    use gravity_abi::breaker::{HashBuild, BUILD_PARTITION};
    use gravity_abi::buildrows::RowStore;
    use gravity_driver::{scatter_build, Result, RunReader, SpillFile};
    const PROC: u32 = 4071;
    const BITS: u8 = 2;
    const ROWS: i32 = 200;
    struct Seen {
        o: Option<memledger::Owner>,
        max: i64,
    }
    impl SpillFile for Seen {
        fn write_epoch(&mut self, _parts: &[(u32, &[u8])]) -> Result<()> {
            if let Some(o) = self.o {
                self.max = self.max.max(memledger::charge::usage(o).held);
            }
            Ok(())
        }
        fn read_part(&self, _part: u32, _sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
            Ok(())
        }
        fn open_part(&self, _part: u32) -> Result<Option<Box<dyn RunReader>>> {
            Ok(None)
        }
        fn spilled_bytes(&self) -> u64 {
            0
        }
    }
    let scatter = |o: Option<memledger::Owner>| -> i64 {
        let total = 64usize << 10;
        let mut region = vec![0u64; total / 8];
        let p = region.as_mut_ptr() as *mut u8;
        unsafe { HashAgg::arm(p, total as u32) };
        let store = unsafe { RowStore::carve(p, 32, 1 << BITS) }.expect("room");
        let build = unsafe { HashBuild::at_share(p, BUILD_PARTITION) };
        for k in 0..ROWS {
            let off = store.append(store.part_for(k), &stored_row(&[k as u64, k as u64 * 10])).expect("room");
            assert!(build.insert(k, off as i64));
        }
        let mut f = Seen { o, max: 0 };
        assert_eq!(scatter_build(build, 0, BITS, &mut f, o).unwrap(), ROWS as u64);
        f.max
    };
    let (during, after, unowned) = std::thread::spawn(move || {
        memledger::bind_backend(PROC, 9000 + PROC);
        memledger::statement_begin();
        let stmt = memledger::charge::context().0;
        let o = memledger::charge::op_begin(stmt, memledger::Kind::HashBuild as u32, 0);
        assert_ne!(o, stmt, "an operator owner");
        let during = scatter(Some(o));
        let after = memledger::charge::usage(o).held;
        let unowned = scatter(None);
        memledger::charge::op_end(o);
        let _ = memledger::statement_end();
        memledger::unbind_backend();
        (during, after, unowned)
    })
    .join()
    .unwrap();
    let stream = (ROWS as i64 + (1 << BITS)) * gravity_abi::breaker::HASHBUILD_ENTRY as i64;
    assert!(during >= stream, "the owner holds the entry stream during the scatter: {during} < {stream}");
    assert_eq!(after, 0, "and nothing of it after");
    assert_eq!(unowned, 0, "no owner: nothing charged");
}

/// CONVERSION WAVE 2: a stream's stage buffers are charged to the slot's owner as they grow (`StageClaim`); a stage a whole
/// part put grew past 2 x STAGE is dropped after its epoch write (its bytes leave the owner with it) instead of staying at
/// that capacity to the life's end; the life's end gives the rest back.
#[test]
fn wave2_a_streams_stage_is_charged_to_its_slot_owner_and_a_whole_part_put_does_not_stay() {
    use gravity_driver::SpillTable;
    const PROC: u32 = 4070;
    const BITS: u8 = 5;
    const STAGE: i64 = 64 << 10;
    let _g = install();
    let (small, after_whole, flushed, after_drop) = std::thread::spawn(|| {
        memledger::bind_backend(PROC, 9000 + PROC);
        memledger::statement_begin();
        let stmt = memledger::charge::context().0;
        let o = memledger::charge::op_begin(stmt, memledger::Kind::HashBuild as u32, 0);
        let held = || memledger::charge::usage(o).held;
        let mut t = SpillTable::with_slots(1);
        t.begin_life(0);
        t.set_owner(0, Some(o));
        t.stage_put(0, 0, BITS, 3, &[1u8; 100]).unwrap();
        let small = held();
        t.stage_put(0, 0, BITS, 4, &vec![2u8; 1 << 20]).unwrap();
        let after_whole = held();
        t.stage_flush(0, 0, BITS).unwrap();
        let flushed = held();
        t.set_owner(0, None);
        drop(t);
        let after_drop = held();
        memledger::charge::op_end(o);
        let _ = memledger::statement_end();
        memledger::unbind_backend();
        (small, after_whole, flushed, after_drop)
    })
    .join()
    .unwrap();
    assert!(small >= 100, "a staged put's buffer is the owner's: {small}");
    assert!(after_whole < 2 * STAGE, "a whole 1 MiB part put does not stay staged: {after_whole}");
    assert!(after_whole >= small, "the small stage stays: {after_whole} < {small}");
    assert_eq!(flushed, after_whole, "a flush keeps the small stages at their capacity");
    assert_eq!(after_drop, 0, "the life's end gives the stage bytes back");
}
