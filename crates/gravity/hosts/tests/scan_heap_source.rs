use gravity_hosts_rig as common;

use std::rc::Rc;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering::Relaxed;

use common::*;
use gravity_abi::ScanPos;
use gravity_abi::ctx::lane;

/// The tests' scratch site lanes (a test bound, not the ABI's: sites size their lane arrays per program).
const MAX_LANES: usize = 32;
use gravity_abi::Lane;
use gravity_driver::{blank_site, BindingRef, CommandId, Error, Identity, PerTuple, Pins, Snapshot, Source, Window};
use gravity_program::LaneId;
use gravity_hosts::{HeapRangeSource, RangeRecipe};
use mcx::{MemoryContext, PgVec};
use tableam_vocab::{SO_ALLOW_PAGEMODE, SO_TYPE_SEQSCAN};
use types_rel::Relation;
use types_scan::sdir::ScanDirection::ForwardScanDirection;
use types_snapshot::SnapshotData;

static FLAG: AtomicU8 = AtomicU8::new(0);

fn binding_ref(snap: &Rc<SnapshotData<'static>>) -> BindingRef<'static> {
    BindingRef { snapshot: Snapshot(snap as *const _ as u64), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: None, ordinal: None }
}

fn recipe(mcx: mcx::Mcx<'_>) -> RangeRecipe {
    let heap = Relation::open(heap_relation_data(mcx), None);
    RangeRecipe::from_plan(&heap, &[1]).unwrap()
}

fn drain(src: &mut HeapRangeSource, lo: u64, hi: u64) -> (Vec<i32>, Vec<ScanPos>) {
    src.seek(&[lo, hi], &[0, 0]).unwrap();
    let (mut rows, mut poss) = (Vec::new(), Vec::new());
    let (mut v, mut n) = ([0u64; 1], [0u8; 1]);
    while src.next(&mut v, &mut n).unwrap() {
        assert_eq!(n[0], 0);
        rows.push(v[0] as u32 as i32);
        poss.push(src.position());
    }
    assert_eq!(src.pins(), 0, "a claim run to its end holds no pin");
    (rows, poss)
}

fn serial_scan(mcx: mcx::Mcx<'static>, snap: &Rc<SnapshotData<'static>>) -> Vec<i32> {
    let heap = Relation::open(heap_relation_data(mcx), None);
    let mut scan = heapam::heap_beginscan(mcx, &heap, Some(snap.clone()), 0, PgVec::new_in(mcx), None, SO_TYPE_SEQSCAN | SO_ALLOW_PAGEMODE).unwrap();
    let mut out = Vec::new();
    while let Some(t) = heapam::heap_getnext(&mut scan, ForwardScanDirection).unwrap() {
        let mut null = false;
        let d = unsafe { types_tuple::heap_getattr(t, 1, &heap.rd_att, &mut null) };
        out.push(d.as_u64() as u32 as i32);
    }
    heapam::heap_endscan(scan).unwrap();
    out
}

#[test]
fn range_claims_cover_the_relation_once_and_match_the_serial_scan() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("heap-source-test")));
    let mcx = ctx.mcx();
    let values: Vec<i32> = (0..1500).collect();
    load_rows(mcx, &values);
    let snap = static_mvcc_snapshot();
    let mut src = HeapRangeSource::new(recipe(mcx));
    src.bind(&binding_ref(&snap)).unwrap();
    let nblocks = src.nblocks() as u64;
    assert!(nblocks >= 4, "1500 int4 rows span several pages, got {nblocks}");
    let mut all = Vec::new();
    let mut fetched = 0;
    for lo in (0..nblocks).step_by(2) {
        let (rows, poss) = drain(&mut src, lo, lo + 2);
        assert!(poss.iter().all(|p| p.rel == HEAP_OID && (p.block as u64) >= lo && (p.block as u64) < lo + 2 && p.is_valid()));
        fetched += (lo + 2).min(nblocks) - lo;
        all.extend(rows);
    }
    assert_eq!(src.blocks_fetched(), fetched);
    all.sort_unstable();
    assert_eq!(all, values, "every row exactly once across the claims");
    let (tail, _) = drain(&mut src, nblocks + 5, u64::MAX);
    assert!(tail.is_empty(), "a claim past the relation is empty and pins nothing");
    src.unbind();
    assert_eq!(pins_held(), 0);
    let mut serial = serial_scan(mcx, &snap);
    serial.sort_unstable();
    assert_eq!(serial, values);
    quiesced();
}

#[test]
fn a_claim_reads_its_run_as_combined_reads_to_the_claim_end() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("heap-source-test")));
    let mcx = ctx.mcx();
    let values: Vec<i32> = (0..1500).collect();
    load_rows(mcx, &values);
    let snap = static_mvcc_snapshot();
    let mut src = HeapRangeSource::new(recipe(mcx));
    src.bind(&binding_ref(&snap)).unwrap();
    let nblocks = src.nblocks() as u64;
    let calls0 = BATCHED_CALLS.load(Relaxed);
    BATCHED_HINT_MAX.store(0, Relaxed);
    BATCHED_FIRST.store(u64::MAX, Relaxed);
    let (rows, _) = drain(&mut src, 0, nblocks);
    assert_eq!(rows.len(), values.len());
    assert_eq!(src.blocks_fetched(), nblocks);
    // a block the guess or a shared hint does not pin is read with the run to the claim's end as its hint (the fake's
    // consecutive buffers answer the blocks after the first by guess; a hint left by an earlier test may answer the first)
    let batched = BATCHED_CALLS.load(Relaxed) - calls0;
    assert!(batched + src.recent_pins() >= nblocks - 1, "every block but the last is a combined read or a verified guess");
    if batched > 0 {
        assert_eq!(BATCHED_FIRST.load(Relaxed) + BATCHED_HINT_MAX.load(Relaxed), nblocks, "the hint is the blocks left in the claim");
    }
    let calls1 = BATCHED_CALLS.load(Relaxed);
    BATCHED_HINT_MAX.store(0, Relaxed);
    // a claim past the relation's end never asks for a run past it: the last block alone is a plain read
    let (rows, _) = drain(&mut src, nblocks - 1, u64::MAX);
    assert!(!rows.is_empty());
    assert_eq!((BATCHED_CALLS.load(Relaxed), BATCHED_HINT_MAX.load(Relaxed)), (calls1, 0), "the last block is a plain read");
    src.unbind();
    assert_eq!(pins_held(), 0);
    quiesced();
}

#[test]
fn a_pool_out_of_block_order_is_pinned_through_the_shared_hints() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("heap-source-test-hints")));
    let mcx = ctx.mcx();
    let values: Vec<i32> = (0..1500).collect();
    load_rows(mcx, &values);
    reverse_pool(HEAP_OID);
    let snap = static_mvcc_snapshot();
    let mut src = HeapRangeSource::new(recipe(mcx));
    src.bind(&binding_ref(&snap)).unwrap();
    let nblocks = src.nblocks() as u64;
    assert!(nblocks > 2);
    let (first, poss1) = drain(&mut src, 0, nblocks);
    assert_eq!(first, values);
    let filled = src.recent_pins();
    let (second, poss2) = drain(&mut src, 0, nblocks);
    assert_eq!((&second, &poss2), (&first, &poss1));
    assert_eq!(src.recent_pins() - filled, nblocks, "every page of the rescan is pinned where the first scan found it");
    src.unbind();
    // another source over the same relation file (another slot, another statement) starts from the same hints
    let mut other = HeapRangeSource::new(recipe(mcx));
    other.bind(&binding_ref(&snap)).unwrap();
    let (third, poss3) = drain(&mut other, 0, nblocks);
    assert_eq!((&third, &poss3), (&first, &poss1));
    assert_eq!(other.recent_pins(), nblocks);
    other.unbind();
    // the pages move again: every hint is wrong, each costs one failed check and the rows are the same
    reverse_pool(HEAP_OID);
    other.bind(&binding_ref(&snap)).unwrap();
    let (fourth, poss4) = drain(&mut other, 0, nblocks);
    assert_eq!((&fourth, &poss4), (&first, &poss1));
    assert!(other.recent_pins() <= 1, "a reversed pool leaves at most the middle block in place");
    assert_eq!(other.blocks_fetched(), nblocks);
    other.unbind();
    assert_eq!(pins_held(), 0);
    quiesced();
}

#[test]
fn a_rescan_pins_its_pages_through_the_recent_buffer_guess() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("heap-source-test-recent")));
    let mcx = ctx.mcx();
    let values: Vec<i32> = (0..1500).collect();
    load_rows(mcx, &values);
    let snap = static_mvcc_snapshot();
    let mut src = HeapRangeSource::new(recipe(mcx));
    src.bind(&binding_ref(&snap)).unwrap();
    let nblocks = src.nblocks() as u64;
    let (first, poss1) = drain(&mut src, 0, nblocks);
    let after_first = src.recent_pins();
    let (second, poss2) = drain(&mut src, 0, nblocks);
    assert_eq!((second, poss2), (first, poss1), "the guessed buffers are verified by tag: the same rows at the same positions");
    assert!(src.recent_pins() > after_first, "a table read in block order sits in consecutive buffers: the rescan guesses right");
    assert!(src.recent_pins() <= src.blocks_fetched());
    assert_eq!(src.blocks_fetched(), 2 * nblocks);
    src.unbind();
    assert_eq!(pins_held(), 0);
    let mut serial = serial_scan(mcx, &snap);
    serial.sort_unstable();
    assert_eq!(serial, values);
    quiesced();
}

#[test]
fn a_stopped_claim_releases_its_page_and_the_source_rebinds() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("heap-source-test-2")));
    let mcx = ctx.mcx();
    load_rows(mcx, &(0..600).collect::<Vec<i32>>());
    let snap = static_mvcc_snapshot();
    let mut src = HeapRangeSource::new(recipe(mcx));
    src.bind(&binding_ref(&snap)).unwrap();
    src.seek(&[0, u64::MAX], &[0, 0]).unwrap();
    let (mut v, mut n) = ([0u64; 1], [0u8; 1]);
    assert!(src.next(&mut v, &mut n).unwrap());
    assert_eq!((v[0], src.pins(), pins_held()), (0, 1, 1), "mid-claim the staged page is pinned");
    src.release();
    assert_eq!((src.pins(), pins_held()), (0, 0));
    assert!(src.next(&mut v, &mut n).unwrap(), "after a release the claim continues from the next page");
    assert!(v[0] > 0);
    src.release();
    src.seek(&[0, 1], &[1, 0]).unwrap();
    assert!(!src.next(&mut v, &mut n).unwrap(), "a NULL claim cell is an empty claim");
    assert_eq!(src.refetch(&mut v, &mut n), Err(gravity_hosts::unsupported()), "no refetch on a worker");
    src.unbind();
    assert_eq!(pins_held(), 0);
    src.bind(&binding_ref(&snap)).unwrap();
    let (rows, _) = drain(&mut src, 0, u64::MAX);
    assert_eq!(rows.len(), 600);
    assert_eq!(src.seek(&[0, 1], &[0, 0]), Ok(()));
    src.unbind();
    assert!(matches!(src.seek(&[0, 1], &[0, 0]), Err(Error::Host(_))), "seek on an unbound source is refused");
    quiesced();
}

/// EPQ (lower.md §8.6 need 3): the driver substitutes the newest version's position on every
/// site and re-runs the program — the range source serves that row alone, at its own position,
/// and the re-seek does not clear the substitution.
#[test]
fn a_substituted_position_serves_that_row_alone() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("heap-source-test-epq")));
    let mcx = ctx.mcx();
    load_rows(mcx, &(0..600).collect::<Vec<i32>>());
    let snap = static_mvcc_snapshot();
    let mut src = HeapRangeSource::new(recipe(mcx));
    src.bind(&binding_ref(&snap)).unwrap();
    src.seek(&[0, u64::MAX], &[0, 0]).unwrap();
    let (mut v, mut n) = ([0u64; 1], [0u8; 1]);
    assert!(src.next(&mut v, &mut n).unwrap());
    assert!(src.next(&mut v, &mut n).unwrap(), "the second row of the claim");
    let (want, pos) = (v[0], src.position());
    src.release();
    src.substitute(pos);
    src.seek(&[0, u64::MAX], &[0, 0]).unwrap();
    assert!(src.next(&mut v, &mut n).unwrap(), "the substituted row");
    assert_eq!((v[0], n[0]), (want, 0), "the version at the substituted position");
    assert_eq!(src.position(), pos, "served at its own position");
    assert!(!src.next(&mut v, &mut n).unwrap(), "the scan ends after the substituted row");
    assert_eq!((src.pins(), pins_held()), (0, 0));
    for bad in [ScanPos::new(HEAP_OID, 0, 0), ScanPos::new(HEAP_OID, 1 << 20, 1), ScanPos::new(HEAP_OID + 1, 0, 1), ScanPos::NONE] {
        src.substitute(bad);
        src.seek(&[0, u64::MAX], &[0, 0]).unwrap();
        assert!(!src.next(&mut v, &mut n).unwrap(), "no row at {bad:?}");
    }
    let (rows, _) = drain(&mut src, 0, u64::MAX);
    assert_eq!(rows.len(), 600, "the source scans normally after a substitution");
    src.unbind();
    assert_eq!(pins_held(), 0);
    quiesced();
}

#[test]
fn the_recipe_refuses_what_a_worker_cannot_deform() {
    let _g = serial();
    install();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("heap-source-test-3")));
    let mcx = ctx.mcx();
    let heap = Relation::open(heap_relation_data(mcx), None);
    assert!(RangeRecipe::from_plan(&heap, &[2]).is_err(), "attno beyond the relation");
    assert!(RangeRecipe::from_plan(&heap, &[0]).is_err());
    assert_eq!(RangeRecipe::from_plan(&heap, &[]).map(|r| r.natts).ok(), Some(0), "no attribute: the count-only recipe (M2 H3)");
    let index = Relation::open(index_relation_data(mcx, false), None);
    assert!(RangeRecipe::from_plan(&index, &[1]).is_err(), "not a heap table");
    let r = RangeRecipe::from_plan(&heap, &[1, 1]).unwrap();
    assert_eq!((r.heap, r.natts, &r.attnos[..2]), (HEAP_OID, 2, &[1i16, 1][..]));
}

#[test]
fn windows_match_the_row_protocol_with_the_offset_lane_and_hold_one_pin_each() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("heap-source-test-4")));
    let mcx = ctx.mcx();
    let values: Vec<i32> = (0..900).collect();
    load_rows(mcx, &values);
    let snap = static_mvcc_snapshot();
    let mut src = HeapRangeSource::new(recipe(mcx));
    src.bind(&binding_ref(&snap)).unwrap();
    let nblocks = src.nblocks() as u64;
    let (rows, poss) = drain(&mut src, 0, nblocks);
    src.seek(&[0, nblocks], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let (mut wrows, mut wposs, mut windows) = (Vec::new(), Vec::new(), 0u64);
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        assert_eq!((src.pins(), pins_held()), (1, 1), "the page stays pinned while its window is current");
        assert_eq!(w.nlanes, 2, "one attribute lane and the offset lane");
        let (a, off) = (&w.lanes()[0], &w.lanes()[1]);
        assert_eq!((a.width, a.region, a.id, a.all_valid()), (8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, 1), true));
        assert_eq!((off.width, off.id), (2, LaneId::Offset.encode()));
        assert_eq!((w.base().pos.rel, w.base().pos.block as u64), (HEAP_OID, windows));
        for i in 0..n as usize {
            wrows.push(unsafe { a.datum(i) } as u32 as i32);
            let o = unsafe { *(off.ptr as *const u16).add(i) };
            wposs.push(ScanPos::new(HEAP_OID, w.base().pos.block, o));
        }
        windows += 1;
    }
    assert_eq!((src.pins(), pins_held()), (0, 0));
    assert_eq!(windows, nblocks, "one window per page");
    assert_eq!(wrows, rows, "the window protocol and the row protocol read the same rows");
    assert_eq!(wposs, poss, "and the offset lane gives the row protocol's positions");
    src.seek(&[0, nblocks], &[0, 0]).unwrap();
    let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
    assert!(src.window(&mut w).unwrap() > 0);
    src.release();
    assert_eq!((src.pins(), pins_held()), (0, 0), "release ends the window and drops its pin");
    src.unbind();
    assert_eq!(pins_held(), 0);
    quiesced();
}

/// Block `blk` of the fixture heap, pinned for `f` (the page's header and line pointers are the test's to set: a VACUUM's
/// PD_ALL_VISIBLE, a pruned line pointer).
fn with_page(blk: u32, f: impl FnOnce(&mut types_storage::bufpage::PageMut<'_>)) {
    let loc = types_storage::RelFileLocator::new(1663, 5, HEAP_OID);
    let buf = bufmgr_seams::read_buffer_without_relcache::call(loc, types_core::ForkNumber::MAIN_FORKNUM, blk, types_storage::ReadBufferMode::Normal, None, true).unwrap();
    let mut page = unsafe { types_storage::bufpage::PageMut::from_raw(bufmgr_seams::buffer_get_page::call(buf)) };
    f(&mut page);
    bufmgr_seams::release_buffer::call(buf).unwrap();
}

fn kill_line(blk: u32, off: u16) {
    with_page(blk, |p| {
        let mut id = p.as_ref().item_id(off);
        id.mark_dead();
        p.set_item_id(off, id);
    });
}

/// heapam's page scan: a page VACUUM marked PD_ALL_VISIBLE is answered from its line pointers under a snapshot not taken
/// during recovery (no tuple through the MVCC seam); any other page tests every normal line pointer; a dead line pointer is
/// skipped on both.
#[test]
fn an_all_visible_page_is_answered_from_its_line_pointers() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("heap-source-test-allvis")));
    let mcx = ctx.mcx();
    let values: Vec<i32> = (0..600).collect();
    load_rows(mcx, &values);
    let snap = static_mvcc_snapshot();
    let mut src = HeapRangeSource::new(recipe(mcx));
    src.bind(&binding_ref(&snap)).unwrap();
    let nblocks = src.nblocks() as u64;
    assert!(nblocks >= 2, "600 int4 rows span several pages, got {nblocks}");
    let (rows, poss) = drain(&mut src, 0, nblocks);
    assert_eq!(rows, values);
    assert_eq!(src.page_paths(), (0, nblocks), "no page is marked all-visible before a VACUUM: every page tested");
    with_page(0, |p| p.set_all_visible());
    let (rows, _) = drain(&mut src, 0, nblocks);
    assert_eq!(rows, values, "the all-visible page serves the same rows");
    assert_eq!(src.page_paths(), (1, 2 * nblocks - 1), "page 0 answered from its line pointers, the rest tested");
    let dead0 = rows[poss.iter().position(|p| p.block == 0 && p.off == 2).unwrap()];
    let dead1 = rows[poss.iter().position(|p| p.block == 1 && p.off == 1).unwrap()];
    kill_line(0, 2);
    kill_line(1, 1);
    let (rows, _) = drain(&mut src, 0, nblocks);
    let want: Vec<i32> = values.iter().copied().filter(|&v| v != dead0 && v != dead1).collect();
    assert_eq!(rows, want, "a dead line pointer is skipped on the all-visible page and on a tested page");
    assert_eq!(src.page_paths(), (2, 3 * nblocks - 2));
    src.unbind();
    // a snapshot taken during recovery tests every tuple, all-visible page or not (heap_prepare_pagescan's rule)
    let mut recovery = SnapshotData::sentinel(mcx, types_snapshot::SnapshotType::SNAPSHOT_MVCC);
    recovery.takenDuringRecovery = true;
    let recovery = Rc::new(recovery);
    src.bind(&binding_ref(&recovery)).unwrap();
    let (rows, _) = drain(&mut src, 0, nblocks);
    assert_eq!(rows, want);
    assert_eq!(src.page_paths(), (0, nblocks), "the bind resets the census; every page tested under a recovery snapshot");
    src.unbind();
    assert_eq!(pins_held(), 0);
    quiesced();
}

/// The page protocol's window carries the flags of the visible tuples from `collect_visible`'s one walk: the fixture's
/// int4 rows have no null bitmap and every attribute on a tested page; the all-visible page is answered from its line
/// pointers, its headers unread (`PAGE_UNREAD`, V4R-371).
#[test]
fn a_raw_window_carries_the_page_and_its_flags_from_the_one_walk() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("heap-source-test-raw")));
    let mcx = ctx.mcx();
    let values: Vec<i32> = (0..500).collect();
    load_rows(mcx, &values);
    let snap = static_mvcc_snapshot();
    let mut src = HeapRangeSource::new(recipe(mcx)).raw(true);
    src.bind(&binding_ref(&snap)).unwrap();
    let nblocks = src.nblocks() as u64;
    assert!(nblocks >= 2);
    with_page(0, |p| p.set_all_visible());
    src.seek(&[0, nblocks], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let (mut n_rows, mut windows) = (0u64, 0u64);
    let census0 = gravity_hosts::ops::scan::heap_source::page_census();
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        let off = &w.lanes()[1];
        assert_eq!((off.width, off.id), (2, LaneId::Offset.encode()));
        assert!(w.lanes()[0].ptr.is_null(), "the attribute lane is published empty on the page protocol");
        drop(w);
        assert!(!site.page.is_null(), "the pinned page");
        use gravity_abi::site::site::{PAGE_FULL, PAGE_IOTA, PAGE_NONULLS, PAGE_UNREAD};
        let flags = PAGE_IOTA | if windows == 0 { PAGE_UNREAD | gravity_hosts::ops::scan::heap_window::PAGE_ALL_VISIBLE } else { PAGE_NONULLS | PAGE_FULL };
        assert_eq!(site.page_flags, flags, "window {windows}: unread when all-visible, else no nulls and full rows");
        let census = gravity_hosts::ops::scan::heap_source::page_census();
        assert_eq!((census.0 - census0.0, census.1 - census0.1), (windows + 1, windows), "window {windows}: a raw window deforms nothing until a lane is asked");
        let mut a = Lane::NONE;
        assert_eq!(src.lane(0, &[], &mut a).unwrap(), n, "the lane deformed on demand");
        for i in 0..n as usize {
            assert_eq!(unsafe { a.datum(i) } as u32, n_rows as u32 + i as u32);
        }
        n_rows += n as u64;
        windows += 1;
    }
    assert_eq!((n_rows, windows), (500, nblocks));
    assert_eq!(src.page_paths(), (1, nblocks - 1));
    src.unbind();
    assert_eq!(pins_held(), 0);
    quiesced();
}

/// C4 RQ: the rows road's window walk (`AmRowsSource::window_pg`) ticks a Step per row it fetches, so a cancel stops the
/// walk at its next row; with the byte clear it reads every row.
#[test]
fn a_canceled_rows_road_window_stops_at_its_next_row() {
    use gravity_hosts::ops::scan::am_rows::{AmRowsRecipe, AmRowsSource};
    fn window(src: &mut AmRowsSource, byte: &AtomicU8) -> Result<u32, Error> {
        let mut lanes = [Lane::NONE; MAX_LANES];
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        cancelpoll::scoped(byte, || src.window(&mut w))
    }
    fn walk(src: &mut AmRowsSource, byte: &AtomicU8) -> (usize, Result<u32, Error>) {
        src.seek(&[], &[]).unwrap();
        let mut got = 0usize;
        loop {
            match window(src, byte) {
                Ok(0) => break (got, Ok(0)),
                Ok(n) => got += n as usize,
                e => break (got, e),
            }
        }
    }
    let _g = serial();
    install();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("heap-source-test-rows-cancel")));
    let mcx = ctx.mcx();
    load_rows(mcx, &(0..600).collect::<Vec<i32>>());
    let snap = static_mvcc_snapshot();
    let heap = Relation::open(heap_relation_data(mcx), None);
    let mut src = AmRowsSource::new(AmRowsRecipe::from_plan(&heap, &[1], &[0]).unwrap());
    src.bind(&binding_ref(&snap)).unwrap();
    let byte = AtomicU8::new(0);
    assert_eq!(walk(&mut src, &byte), (600, Ok(0)), "the byte clear: every row");
    src.seek(&[], &[]).unwrap();
    let first = window(&mut src, &byte).unwrap();
    assert!(first > 0 && (first as usize) < 600, "the walk spans pages: {first} rows in its first window");
    byte.store(cancelpoll::ABORT_BIT, Relaxed);
    assert_eq!(window(&mut src, &byte), Err(Error::Cancelled), "a set byte stops the walk at its next row");
    byte.store(0, Relaxed);
    assert_eq!(walk(&mut src, &byte), (600, Ok(0)), "a rescan after the stop reads every row");
    src.unbind();
    quiesced();
}
