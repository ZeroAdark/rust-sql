use core::sync::atomic::Ordering;
use std::collections::HashSet;

use lwlock::{LWLockAcquire, LWLockRelease, LW_SHARED};
use types_core::{Buffer, BLCKSZ};
use types_storage::buf::{BufferAccessStrategyType, BM_DIRTY, BM_TAG_VALID, BUF_REFCOUNT_MASK};

use super::*;
use crate::extent::{self, ACTIVE, DRAINING, EXTENT_BUFFERS, RETIRED, UNBORN};
use crate::resize::{BufferPoolResize, ResizeOutcome};

const POOL_FLOOR_TARGET: i32 = 3072;

fn resize_to(buffers: i32, timeout_ms: i32) -> ResizeOutcome {
    BufferPoolResize(buffers as i64 * BLCKSZ as i64, timeout_ms).unwrap()
}

fn restore_boot_pool() {
    TEST_RELOCATE.store(true, Ordering::Relaxed);
    TEST_DRAIN_RATE.store(0, Ordering::Relaxed);
    let out = resize_to(TEST_NBUFFERS, 10_000);
    assert_eq!(out.active_bytes, TEST_NBUFFERS as i64 * BLCKSZ as i64);
    assert_eq!(out.shortfall_bytes, 0);
    assert_pool_consistent();
}

fn lookup(tag: &types_storage::buf::buftag) -> i32 {
    let h = BufTableHashCode(tag);
    let lock = BufMappingPartitionLock(h);
    LWLockAcquire(lock, LW_SHARED, globals::MyProcNumber()).unwrap();
    let id = BufTableLookup(tag, h).unwrap();
    LWLockRelease(lock).unwrap();
    id
}

/// Every valid buffer lies in a live extent and is what the mapping names
/// for its tag; retired extents hold no valid or pinned buffer; the active
/// count and the sweep span agree with the extent states.
fn assert_pool_consistent() {
    let mut live = 0;
    for e in 0..extent::n_extents() {
        let st = extent::state(e);
        let (lo, hi) = extent::extent_range(e);
        if st == ACTIVE || st == DRAINING {
            live += hi - lo;
        }
        assert_ne!(st, DRAINING, "no extent stays draining after a resize returns");
    }
    assert_eq!(extent::active_buffers(), live);
    assert_eq!(extent::span(), extent::computed_span());
    assert_eq!(bufmgr_seams::active_buffers_or(0), live);
    for id in 0..NBuffersInited() {
        let desc = GetBufferDescriptor(id);
        let st = LockBufHdr(desc);
        let tag = desc.tag();
        UnlockBufHdr(desc, st);
        let e = id >> crate::extent::EXTENT_SHIFT;
        if extent::state(e) == RETIRED {
            assert_eq!(st & BM_TAG_VALID, 0, "buffer {id} valid in retired extent {e}");
            assert_eq!(st & BUF_REFCOUNT_MASK, 0, "buffer {id} pinned in retired extent {e}");
        }
        if st & BM_TAG_VALID != 0 {
            assert_eq!(lookup(&tag), id, "mapping for buffer {id} ({tag:?})");
        }
    }
}

fn fill(rel: u32, nblocks: u32) {
    for blk in 0..nblocks {
        let b = read_blk(rel, blk);
        ReleaseBuffer(b).unwrap();
    }
}

fn resident_blocks(rel: u32) -> Vec<(u32, i32)> {
    let mut out = Vec::new();
    for id in 0..NBuffersInited() {
        let desc = GetBufferDescriptor(id);
        let st = LockBufHdr(desc);
        let tag = desc.tag();
        UnlockBufHdr(desc, st);
        if st & BM_TAG_VALID != 0 && tag.relNumber == rel {
            out.push((tag.blockNum, id));
        }
    }
    out
}

fn page_block_stamp(b: Buffer) -> u32 {
    let p = BufferGetBlockPtr(b);
    // SAFETY: pinned page image, BLCKSZ bytes; valid_page_into stamps the
    // block number at offset 24.
    unsafe { u32::from_ne_bytes(core::slice::from_raw_parts(p.add(24), 4).try_into().unwrap()) }
}

fn pin_id(id: i32) {
    crate::privref::ReservePrivateRefCountEntry();
    crate::pin::resowner_enlarge_for_pin().unwrap();
    crate::pin::PinBuffer(GetBufferDescriptor(id), &None);
}

fn unpin_id(id: i32) {
    crate::pin::UnpinBuffer(GetBufferDescriptor(id));
}

#[test]
fn bp1_geometry_plan() {
    let inert = extent::plan_geometry(1000, 0, false).unwrap();
    assert_eq!((inert.max_buffers, inert.boot_buffers, inert.n_extents), (1000, 1000, 4));
    let same = extent::plan_geometry(1000, 1000, false).unwrap();
    assert_eq!(same, inert, "max == shared_buffers is the inert shape");
    let engaged = extent::plan_geometry(1000, 4096, false).unwrap();
    assert_eq!((engaged.max_buffers, engaged.boot_buffers, engaged.n_extents), (4096, 1024, 16));
    let tight = extent::plan_geometry(1000, 1010, false).unwrap();
    assert_eq!((tight.boot_buffers, tight.n_extents), (1010, 4), "boot rounds up, clamped to max");
    let err = extent::plan_geometry(1000, 999, false).unwrap_err();
    assert!(err.message.contains("must be 0 or at least \"shared_buffers\""), "{}", err.message);
    assert_eq!(err.sqlstate, types_error::ERRCODE_CONFIG_FILE_ERROR);
    let thp = extent::plan_geometry(1000, 0, true);
    if cfg!(target_os = "linux") {
        assert!(thp.unwrap().thp);
    } else {
        let e = thp.unwrap_err();
        assert!(e.message.contains("not supported on this platform"), "{}", e.message);
    }
}

#[test]
fn bp1_boot_shape_is_the_resizable_harness_pool() {
    let _g = setup();
    assert_eq!(extent::max_buffers(), TEST_POOL_MAX);
    assert_eq!(extent::boot_buffers(), TEST_NBUFFERS);
    assert_eq!(extent::n_extents(), TEST_POOL_MAX / EXTENT_BUFFERS);
    for e in 0..extent::n_extents() {
        let st = extent::state(e);
        if e < TEST_NBUFFERS / EXTENT_BUFFERS {
            assert!(st == ACTIVE || st == RETIRED || st == UNBORN);
        }
    }
    assert!(NBuffersInited() >= TEST_NBUFFERS);
    let blocks = BufferGetBlockPtr(1) as usize;
    assert_eq!(blocks % crate::extent::EXTENT_BYTES, 0, "blocks are 2 MiB aligned");
}

#[test]
fn bp1_refusals_are_typed_and_loud() {
    let _g = setup();
    let above = BufferPoolResize((TEST_POOL_MAX as i64 + 1) * BLCKSZ as i64, 0).unwrap_err();
    assert_eq!(above.sqlstate, types_error::ERRCODE_INVALID_PARAMETER_VALUE);
    assert!(above.message.contains("exceeds \"pgrust.buffer_pool_max\""), "{}", above.message);
    let floor = extent::pin_floor_buffers();
    let below = BufferPoolResize((floor as i64 - 1) * BLCKSZ as i64, 0).unwrap_err();
    assert_eq!(below.sqlstate, types_error::ERRCODE_INVALID_PARAMETER_VALUE);
    assert!(below.message.contains("below the pin floor"), "{}", below.message);
    let neg = BufferPoolResize(TEST_NBUFFERS as i64 * BLCKSZ as i64, -1).unwrap_err();
    assert!(neg.message.contains("timeout must not be negative"), "{}", neg.message);
    TEST_IN_RECOVERY.store(true, Ordering::Relaxed);
    let rec = BufferPoolResize(TEST_NBUFFERS as i64 * BLCKSZ as i64, 0).unwrap_err();
    TEST_IN_RECOVERY.store(false, Ordering::Relaxed);
    assert_eq!(rec.sqlstate, types_error::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE);
    crate::resize::testing::set_resizing(true);
    let busy = BufferPoolResize(TEST_NBUFFERS as i64 * BLCKSZ as i64, 0).unwrap_err();
    crate::resize::testing::set_resizing(false);
    assert_eq!(busy.sqlstate, types_error::ERRCODE_OBJECT_IN_USE);
    assert_eq!(extent::active_buffers(), TEST_NBUFFERS, "refusals change nothing");
}

#[test]
fn bp1_shrink_relocates_hot_pages_then_grow_restores() {
    let _g = setup();
    restore_boot_pool();
    let rel = 9601;
    // D4890: start from an empty pool (as giveback_relocation_never_writes_a_victim): after a test that left the pool
    // full of hot pages, the fill's clock sweep evicted the relation's own fresh blocks (giveback-02 under load: under
    // the precondition's 5000, right after the relocation test). The write seams first: a dirty page left behind is
    // flushed (installed once per process; other tests install them in no fixed order)
    setup_write_seams();
    crate::EvictAllUnpinnedBuffers().unwrap();
    fill(rel, 6000);
    // Hits raise usage without a sweep: the pages are hot wherever the clock
    // hand stood while they were read (a sweep pass may have zeroed them).
    for (blk, _) in resident_blocks(rel) {
        ReleaseBuffer(read_blk(rel, blk)).unwrap();
    }
    let before = resident_blocks(rel);
    assert!(before.len() >= 5000);
    let relocated0 = crate::resize::BufferPoolStats().relocated;

    let out = resize_to(4096, 10_000);
    assert_eq!(out.active_bytes, 4096 * BLCKSZ as i64);
    assert_eq!(out.target_bytes, 4096 * BLCKSZ as i64);
    assert_eq!(out.shortfall_bytes, 0);
    assert_pool_consistent();
    let stats = crate::resize::BufferPoolStats();
    let active = (0..extent::n_extents()).filter(|&e| extent::state(e) == ACTIVE).count();
    assert_eq!(active as i32, 4096 / EXTENT_BUFFERS);
    assert!(stats.retired_extents >= (TEST_NBUFFERS - 4096) as i64 / EXTENT_BUFFERS as i64);
    assert!(stats.relocated > relocated0, "hot pages moved into survivors");

    let after = resident_blocks(rel);
    assert!(after.len() <= 4096);
    assert!(after.len() >= 3000, "the shrunk pool keeps what fits: {}", after.len());
    let reads0 = SMGR_READS.load(Ordering::Relaxed);
    for &(blk, _) in after.iter().take(500) {
        let b = read_blk(rel, blk);
        assert_eq!(page_block_stamp(b), blk, "relocated page content intact");
        ReleaseBuffer(b).unwrap();
    }
    assert_eq!(SMGR_READS.load(Ordering::Relaxed), reads0, "resident pages hit after relocation");

    let out = resize_to(TEST_NBUFFERS, 1000);
    assert_eq!(out.active_bytes, TEST_NBUFFERS as i64 * BLCKSZ as i64);
    fill(rel + 1, 6000);
    assert_pool_consistent();
}

#[test]
fn bp1_no_relocation_kill_switch_drops_and_refetches() {
    let _g = setup();
    restore_boot_pool();
    let rel = 9611;
    fill(rel, 7000);
    TEST_RELOCATE.store(false, Ordering::Relaxed);
    let s0 = crate::resize::BufferPoolStats();
    resize_to(4096, 10_000);
    TEST_RELOCATE.store(true, Ordering::Relaxed);
    let s1 = crate::resize::BufferPoolStats();
    assert_eq!(s1.relocated, s0.relocated, "kill switch: nothing relocated");
    assert!(s1.dropped > s0.dropped);
    assert_pool_consistent();
    restore_boot_pool();
}

#[test]
fn bp1_shrink_writes_dirty_pages() {
    let _g = setup();
    setup_write_seams();
    restore_boot_pool();
    let rel = 9621;
    fill(rel, 7500);
    let mut dirtied = Vec::new();
    for (blk, id) in resident_blocks(rel) {
        if id >= 4096 && dirtied.len() < 64 {
            dirty_block(rel, blk);
            dirtied.push(blk);
        }
    }
    assert!(!dirtied.is_empty());
    let w0 = crate::resize::BufferPoolStats().written;
    let writes0 = WRITES.lock().unwrap().len();
    resize_to(4096, 10_000);
    let s = crate::resize::BufferPoolStats();
    assert!(s.written >= w0 + dirtied.len() as i64, "every dirty page in a drained extent written");
    let written: HashSet<u32> = WRITES.lock().unwrap()[writes0..]
        .iter()
        .filter(|w| w.2 == rel)
        .map(|w| w.4)
        .collect();
    for blk in &dirtied {
        assert!(written.contains(blk), "block {blk} written before its extent retired");
    }
    for id in 0..NBuffersInited() {
        let st = GetBufferDescriptor(id).state.load(Ordering::Relaxed);
        if extent::state(id >> crate::extent::EXTENT_SHIFT) == RETIRED {
            assert_eq!(st & BM_DIRTY, 0);
        }
    }
    assert_pool_consistent();
    restore_boot_pool();
}

#[test]
fn bp1_pinned_extents_give_shortfall_then_complete() {
    let _g = setup();
    let _k = GivebackKnob::off();
    restore_boot_pool();
    let rel = 9631;
    fill(rel, 9000);
    let keep = POOL_FLOOR_TARGET / EXTENT_BUFFERS;
    let active: Vec<i32> = (0..extent::n_extents()).filter(|&e| extent::state(e) == ACTIVE).collect();
    let total = active.len() as i32;
    assert_eq!(total, TEST_NBUFFERS / EXTENT_BUFFERS);
    let unpinned = 3;
    let mut pinned = Vec::new();
    for &e in &active[unpinned as usize..] {
        let (lo, hi) = extent::extent_range(e);
        let id = (lo..hi)
            .find(|&id| GetBufferDescriptor(id).state.load(Ordering::Relaxed) & BM_TAG_VALID != 0)
            .expect("filled extent has a valid buffer");
        pin_id(id);
        pinned.push(id);
    }
    let out = resize_to(POOL_FLOOR_TARGET, 300);
    let need = (total - keep) as i64;
    let retired_now = (TEST_NBUFFERS as i64 * BLCKSZ as i64 - out.active_bytes) / (EXTENT_BUFFERS as i64 * BLCKSZ as i64);
    assert_eq!(retired_now, unpinned as i64, "the unpinned extents go first");
    assert_eq!(out.target_bytes, POOL_FLOOR_TARGET as i64 * BLCKSZ as i64);
    assert_eq!(
        out.shortfall_bytes,
        (need - unpinned as i64) * EXTENT_BUFFERS as i64 * BLCKSZ as i64,
        "pinned extents are the shortfall"
    );
    assert_pool_consistent();
    for id in &pinned {
        let e = id >> crate::extent::EXTENT_SHIFT;
        assert_eq!(extent::state(e), ACTIVE, "a pinned extent is never retired");
        assert!(GetBufferDescriptor(*id).state.load(Ordering::Relaxed) & BM_TAG_VALID != 0);
    }
    for id in pinned {
        unpin_id(id);
    }
    let out = resize_to(POOL_FLOOR_TARGET, 10_000);
    assert_eq!(out.shortfall_bytes, 0, "completes once the pins are gone");
    assert_eq!(out.active_bytes, POOL_FLOOR_TARGET as i64 * BLCKSZ as i64);
    assert_pool_consistent();
    restore_boot_pool();
}

#[test]
fn bp1_single_pin_falls_back_to_a_lower_extent() {
    let _g = setup();
    restore_boot_pool();
    fill(9641, 8000);
    let top = extent::computed_span() / EXTENT_BUFFERS - 1;
    let (lo, hi) = extent::extent_range(top);
    let id = (lo..hi)
        .find(|&id| GetBufferDescriptor(id).state.load(Ordering::Relaxed) & BM_TAG_VALID != 0)
        .unwrap();
    pin_id(id);
    let out = resize_to(TEST_NBUFFERS - 4 * EXTENT_BUFFERS, 2000);
    assert_eq!(out.shortfall_bytes, 0, "a lower extent retires in the pinned one's place");
    assert_eq!(extent::state(top), ACTIVE);
    assert_pool_consistent();
    unpin_id(id);
    restore_boot_pool();
}

#[test]
fn bp1_victim_paths_never_use_retired_buffers() {
    let _g = setup();
    setup_write_seams();
    if !transam_xlog_seams::xlog_needs_flush::is_installed() {
        transam_xlog_seams::xlog_needs_flush::set(|_| false);
    }
    restore_boot_pool();
    resize_to(POOL_FLOOR_TARGET + 5 * EXTENT_BUFFERS, 10_000);
    let strategy = GetAccessStrategy(BufferAccessStrategyType::BasBulkread);
    for blk in 0..6000u32 {
        let b = ReadBufferWithoutRelcache(
            rloc(9651),
            ForkNumber::MAIN_FORKNUM,
            blk,
            ReadBufferMode::Normal,
            strategy.clone(),
            true,
        )
        .unwrap();
        let e = (b - 1) >> crate::extent::EXTENT_SHIFT;
        assert_eq!(extent::state(e), ACTIVE, "victim {b} from a non-active extent");
        ReleaseBuffer(b).unwrap();
    }
    resize_to(POOL_FLOOR_TARGET, 10_000);
    for blk in 0..6000u32 {
        let b = ReadBufferWithoutRelcache(
            rloc(9652),
            ForkNumber::MAIN_FORKNUM,
            blk,
            ReadBufferMode::Normal,
            strategy.clone(),
            true,
        )
        .unwrap();
        let e = (b - 1) >> crate::extent::EXTENT_SHIFT;
        assert_eq!(extent::state(e), ACTIVE, "ring handed out retired buffer {b}");
        ReleaseBuffer(b).unwrap();
    }
    FreeAccessStrategy(strategy);
    fill(9653, 5000);
    assert_pool_consistent();
    restore_boot_pool();
}

#[test]
fn bp1_grow_past_boot_initializes_new_extents() {
    let _g = setup();
    restore_boot_pool();
    let out = resize_to(TEST_POOL_MAX, 0);
    assert_eq!(out.active_bytes, TEST_POOL_MAX as i64 * BLCKSZ as i64);
    assert_eq!(NBuffersInited(), TEST_POOL_MAX);
    assert_eq!(extent::span(), TEST_POOL_MAX);
    fill(9661, 15000);
    let top = resident_blocks(9661).iter().filter(|&&(_, id)| id >= TEST_NBUFFERS).count();
    assert!(top > 5000, "grown extents take pages: {top}");
    assert_pool_consistent();
    restore_boot_pool();
    assert_eq!(NBuffersInited(), TEST_POOL_MAX, "descriptors stay initialized (BP2 releases them)");
}

#[test]
fn bp1_mapping_table_halves_after_shrink_and_regrows() {
    let _g = setup();
    restore_boot_pool();
    fill(9671, 8000);
    let before = crate::buf_table::total_capacity();
    resize_to(POOL_FLOOR_TARGET, 10_000);
    let after = crate::buf_table::total_capacity();
    assert!(after < before, "sparse partitions halved: {before} -> {after}");
    assert_pool_consistent();
    restore_boot_pool();
    fill(9672, 8000);
    assert_pool_consistent();
}

#[test]
fn bp1_released_extents_leave_residency() {
    let _g = setup();
    restore_boot_pool();
    fill(9681, 8100);
    let r0 = extent::resident_block_bytes(NBuffersInited());
    resize_to(POOL_FLOOR_TARGET, 10_000);
    let r1 = extent::resident_block_bytes(NBuffersInited());
    let released = (TEST_NBUFFERS - POOL_FLOOR_TARGET) as u64 * BLCKSZ as u64;
    assert!(
        r0 - r1 >= released * 9 / 10,
        "resident {r0} -> {r1}, expected ~{released} fewer bytes"
    );
    let retired = (0..extent::n_extents())
        .find(|&e| extent::state(e) == RETIRED)
        .expect("a shrink leaves retired extents");
    let (lo, _) = extent::extent_range(retired);
    let p = BufferGetBlockPtr(lo + 1);
    // SAFETY: inside the reservation; the retired extent is never read by
    // the pool, and a released range reads as zeroes on every platform.
    let zero = unsafe { core::slice::from_raw_parts(p, BLCKSZ).iter().all(|&b| b == 0) };
    assert!(zero, "released block memory is zero-fill");
    restore_boot_pool();
}

#[test]
fn bp1_crash_reset_restores_boot_extents() {
    let _g = setup();
    restore_boot_pool();
    resize_to(12288, 0);
    fill(9691, 11000);
    BufferManagerShmemResetAfterCrash();
    assert_eq!(extent::active_buffers(), TEST_NBUFFERS);
    assert_eq!(extent::span(), TEST_NBUFFERS);
    for e in 0..extent::n_extents() {
        let st = extent::state(e);
        if e < TEST_NBUFFERS / EXTENT_BUFFERS {
            assert_eq!(st, ACTIVE);
        } else {
            assert!(st == RETIRED || st == UNBORN);
        }
    }
    assert!(!crate::resize::BufferPoolStats().resizing);
    let b = read_blk(9692, 0);
    assert!(b <= TEST_NBUFFERS);
    ReleaseBuffer(b).unwrap();
    fill(9693, 9000);
    assert_pool_consistent();
    restore_boot_pool();
}

#[test]
fn bp1_thrash_under_concurrent_readers() {
    let _g = setup();
    let _k = GivebackKnob::off();
    setup_write_seams();
    restore_boot_pool();
    const UNIVERSE: u32 = 12_000;
    for rel in [9700u32, 9701] {
        fake_rel_fd(rel, 0, UNIVERSE);
    }
    let stop = std::sync::Arc::new(core::sync::atomic::AtomicBool::new(false));
    let done = std::sync::Arc::new(core::sync::atomic::AtomicU64::new(0));
    let mut readers = Vec::new();
    for t in 0..4u32 {
        let stop = stop.clone();
        let done = done.clone();
        readers.push(std::thread::spawn(move || {
            become_backend();
            let owner = resowner::ResourceOwnerCreate(
                types_resowner::ResourceOwner::NULL,
                "bufmgr-bp1-thrash",
            )
            .unwrap();
            resowner::SetCurrentResourceOwner(owner);
            let mut x: u64 = 0x9E37_79B9_7F4A_7C15 ^ t as u64;
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let blk = (x % UNIVERSE as u64) as u32;
                let b = read_blk(9700 + t % 2, blk);
                assert_eq!(page_block_stamp(b), blk, "page content under thrash");
                if x % 7 == 0 {
                    LockBuffer(b, BUFFER_LOCK_EXCLUSIVE).unwrap();
                    MarkBufferDirty(b).unwrap();
                    LockBuffer(b, BUFFER_LOCK_UNLOCK).unwrap();
                }
                ReleaseBuffer(b).unwrap();
                n += 1;
                done.fetch_add(1, Ordering::Relaxed);
            }
            AtEOXact_Buffers(true);
            n
        }));
    }
    let targets = [POOL_FLOOR_TARGET, TEST_POOL_MAX, 5000, 12000, POOL_FLOOR_TARGET, 9000];
    let mut shortfalls = 0;
    for round in 0..18 {
        let out = resize_to(targets[round % targets.len()], 150);
        if out.shortfall_bytes > 0 {
            shortfalls += 1;
        }
    }
    // On a loaded host the resizes can finish before the readers have done much: they keep reading (bounded) until they
    // have made the progress checked below, so a stall still fails and a slow box does not.
    let t0 = std::time::Instant::now();
    while done.load(Ordering::Relaxed) <= 1000 && t0.elapsed() < std::time::Duration::from_secs(120) {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    stop.store(true, Ordering::Relaxed);
    let reads: u64 = readers.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(reads > 1000, "readers made progress: {reads}");
    assert_pool_consistent();
    let _ = shortfalls;
    restore_boot_pool();
}

fn zipf_blocks(universe: u32, draws: usize, s: f64, seed: u64) -> Vec<u32> {
    let mut cdf = Vec::with_capacity(universe as usize);
    let mut acc = 0.0f64;
    for k in 1..=universe {
        acc += 1.0 / (k as f64).powf(s);
        cdf.push(acc);
    }
    let mut x = seed | 1;
    let mut out = Vec::with_capacity(draws);
    for _ in 0..draws {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let u = (x >> 11) as f64 / (1u64 << 53) as f64 * acc;
        let k = cdf.partition_point(|&c| c < u) as u32;
        out.push(k.min(universe - 1));
    }
    out
}

#[test]
fn bp1_wss_zipf_estimate_within_five_percent() {
    let _g = setup();
    let now = 2_000_000_000u32;
    let mut worst = 0.0f64;
    for (universe, draws, seed) in [(200_000u32, 400_000usize, 7u64), (1_000_000, 2_000_000, 11), (2_000_000, 2_500_000, 13)] {
        crate::wss::reset();
        let mut exact = HashSet::new();
        for blk in zipf_blocks(universe, draws, 1.05, seed) {
            let rel = 9800 + (blk % 3);
            let tag = crate::read::init_buffer_tag(rloc(rel), ForkNumber::MAIN_FORKNUM, blk / 3);
            crate::wss::testing::feed_at(BufTableHashCode(&tag), now);
            exact.insert((rel, blk / 3));
        }
        let est = crate::wss::testing::estimate(now, 60);
        let err = (est - exact.len() as f64).abs() / exact.len() as f64;
        worst = worst.max(err);
        assert!(err < 0.05, "universe {universe}: exact {} est {est:.0} err {:.2}%", exact.len(), err * 100.0);
    }
    eprintln!("bp1 wss worst relative error {:.2}%", worst * 100.0);
    crate::wss::reset();
}

#[test]
fn bp1_wss_windows_forget_old_pages() {
    let _g = setup();
    crate::wss::reset();
    let t0 = 2_100_000_000u32;
    for blk in 0..200_000u32 {
        let tag = crate::read::init_buffer_tag(rloc(9811), ForkNumber::MAIN_FORKNUM, blk);
        crate::wss::testing::feed_at(BufTableHashCode(&tag), t0);
    }
    for blk in 0..50_000u32 {
        let tag = crate::read::init_buffer_tag(rloc(9812), ForkNumber::MAIN_FORKNUM, blk);
        crate::wss::testing::feed_at(BufTableHashCode(&tag), t0 + 240);
    }
    let one = crate::wss::testing::estimate(t0 + 250, 60);
    let five = crate::wss::testing::estimate(t0 + 250, 300);
    assert!((one - 50_000.0).abs() / 50_000.0 < 0.05, "1-minute window {one:.0}");
    assert!((five - 250_000.0).abs() / 250_000.0 < 0.05, "5-minute window {five:.0}");
    crate::wss::reset();
}

#[test]
fn bp1_wss_sampling_rate_is_one_in_sixteen() {
    let mut sampled = 0u32;
    for blk in 0..160_000u32 {
        let tag = crate::read::init_buffer_tag(rloc(9821), ForkNumber::MAIN_FORKNUM, blk);
        if crate::wss::testing::sampled(BufTableHashCode(&tag)) {
            sampled += 1;
        }
    }
    assert!((9_300..10_700).contains(&sampled), "sampled {sampled} of 160000");
}

struct GivebackKnob;

impl GivebackKnob {
    fn on() -> GivebackKnob {
        mempolicy::consts::set(mempolicy::consts::K::BufGiveback, 1);
        GivebackKnob
    }

    fn off() -> GivebackKnob {
        mempolicy::consts::set(mempolicy::consts::K::BufGiveback, 0);
        GivebackKnob
    }

    fn at_default() -> GivebackKnob {
        let k = mempolicy::consts::K::BufGiveback;
        mempolicy::consts::set(k, mempolicy::consts::KNOBS[k as usize].default);
        GivebackKnob
    }
}

impl Drop for GivebackKnob {
    fn drop(&mut self) {
        mempolicy::consts::set(mempolicy::consts::K::BufGiveback, 0);
    }
}

fn draining_extents() -> usize {
    (0..extent::n_extents()).filter(|&e| extent::state(e) == DRAINING).count()
}

/// Runs the background writer's slices until the give-back is done; returns the slices. Every slice returns
/// promptly: a pinned buffer is skipped, never waited on.
fn run_background(max_slices: usize) -> usize {
    for i in 0..max_slices {
        let t = std::time::Instant::now();
        let step = crate::giveback_step().unwrap();
        assert!(t.elapsed() < std::time::Duration::from_millis(500), "slice {i} took {:?}", t.elapsed());
        match step {
            crate::GivebackStep::Idle => return i,
            crate::GivebackStep::Again(ms) => std::thread::sleep(std::time::Duration::from_millis(ms as u64)),
        }
    }
    panic!("the give-back did not finish in {max_slices} slices");
}

fn bytes_of(buffers: i32) -> i64 {
    buffers as i64 * BLCKSZ as i64
}

#[test]
fn giveback_off_keeps_the_timeout_and_revert() {
    let _g = setup();
    let _k = GivebackKnob::off();
    restore_boot_pool();
    assert_eq!(crate::giveback_request(1 << 30), 0);
    assert_eq!(crate::giveback_reclaimable(), 0);
    assert_eq!(crate::giveback_step().unwrap(), crate::GivebackStep::Idle);
    fill(9701, 8000);
    TEST_DRAIN_RATE.store(2000, Ordering::Relaxed);
    let out = resize_to(POOL_FLOOR_TARGET, 20);
    TEST_DRAIN_RATE.store(0, Ordering::Relaxed);
    assert!(out.shortfall_bytes > 0, "a paced shrink stops at its timeout");
    assert_eq!(draining_extents(), 0, "and reverts what it had in flight");
    assert!(!crate::resize::BufferPoolStats().resizing);
    assert_eq!(crate::giveback_stats().goal_bytes, 0);
    assert_pool_consistent();
    restore_boot_pool();
}

/// D4649 flip (the user's, 2026-10-16T17:37Z): at the knob's default a shrink that reaches its timeout keeps its
/// draining extents and the background writer finishes it. Born red on the old default 0 (the shrink reverted).
#[test]
fn giveback_at_the_default_a_timed_out_shrink_hands_off_and_the_background_finishes() {
    let _g = setup();
    restore_boot_pool();
    let _k = GivebackKnob::at_default();
    fill(9831, 7500);
    let out = resize_to(4096, 0);
    assert_eq!(out.active_bytes, bytes_of(TEST_NBUFFERS), "timeout 0: the whole shrink is the background's");
    assert!(draining_extents() > 0, "the default keeps the draining extents");
    run_background(10_000);
    assert_eq!(extent::active_buffers(), 4096);
    assert_pool_consistent();
    restore_boot_pool();
}

#[test]
fn giveback_timeout_hands_off_and_the_background_finishes() {
    let _g = setup();
    restore_boot_pool();
    let _k = GivebackKnob::on();
    let rel = 9711;
    // D4890: start from an empty pool, so what fill leaves resident does not depend on the test before it (the write
    // seams first: a dirty page left behind is flushed)
    setup_write_seams();
    crate::EvictAllUnpinnedBuffers().unwrap();
    fill(rel, 7500);
    for (blk, _) in resident_blocks(rel) {
        ReleaseBuffer(read_blk(rel, blk)).unwrap();
    }
    TEST_DRAIN_RATE.store(2000, Ordering::Relaxed);
    let out = resize_to(4096, 20);
    TEST_DRAIN_RATE.store(0, Ordering::Relaxed);
    assert!(out.shortfall_bytes > 0);
    assert!(draining_extents() > 0, "the timeout keeps its draining extents");
    assert!(crate::resize::BufferPoolStats().resizing, "a pending give-back reads as resizing");
    let g = crate::giveback_stats();
    assert!(g.on);
    assert_eq!(g.goal_bytes, bytes_of(4096));
    assert_eq!(g.landing_bytes, out.active_bytes - bytes_of(4096));

    run_background(10_000);
    assert_eq!(extent::active_buffers(), 4096);
    assert_eq!(draining_extents(), 0);
    let g = crate::giveback_stats();
    assert_eq!((g.goal_bytes, g.landing_bytes, g.dirty_pending_bytes), (0, 0, 0));
    assert!(g.slices > 0);
    assert!(!crate::resize::BufferPoolStats().resizing);
    assert_eq!(crate::resize::BufferPoolStats().target_bytes, bytes_of(4096));
    assert_pool_consistent();
    let after = resident_blocks(rel);
    assert!(after.len() >= 3000, "the shrunk pool keeps what fits: {}", after.len());
    let reads0 = SMGR_READS.load(Ordering::Relaxed);
    for &(blk, _) in after.iter().take(500) {
        let b = read_blk(rel, blk);
        assert_eq!(page_block_stamp(b), blk, "relocated page content intact");
        ReleaseBuffer(b).unwrap();
    }
    assert_eq!(SMGR_READS.load(Ordering::Relaxed), reads0, "resident pages hit after the background drain");
    restore_boot_pool();
}

#[test]
fn giveback_background_writes_dirty_pages() {
    let _g = setup();
    setup_write_seams();
    restore_boot_pool();
    let _k = GivebackKnob::on();
    let rel = 9721;
    fill(rel, 7500);
    let mut dirtied = Vec::new();
    for (blk, id) in resident_blocks(rel) {
        if id >= 4096 && dirtied.len() < 64 {
            dirty_block(rel, blk);
            dirtied.push(blk);
        }
    }
    assert!(!dirtied.is_empty());
    let writes0 = WRITES.lock().unwrap().len();
    let out = resize_to(4096, 0);
    assert_eq!(out.active_bytes, bytes_of(TEST_NBUFFERS), "timeout 0: the whole shrink is the background's");
    let g = crate::giveback_stats();
    assert!(g.dirty_pending_bytes >= bytes_of(dirtied.len() as i32), "dirty pending {}", g.dirty_pending_bytes);

    run_background(10_000);
    assert_eq!(extent::active_buffers(), 4096);
    let g = crate::giveback_stats();
    assert!(g.written_bytes >= bytes_of(dirtied.len() as i32));
    assert!(g.write_bytes_per_s > 0);
    let written: HashSet<u32> = WRITES.lock().unwrap()[writes0..]
        .iter()
        .filter(|w| w.2 == rel)
        .map(|w| w.4)
        .collect();
    for blk in &dirtied {
        assert!(written.contains(blk), "block {blk} written before its extent retired");
    }
    for id in 0..NBuffersInited() {
        if extent::state(id >> crate::extent::EXTENT_SHIFT) == RETIRED {
            assert_eq!(GetBufferDescriptor(id).state.load(Ordering::Relaxed) & BM_DIRTY, 0);
        }
    }
    assert_pool_consistent();
    restore_boot_pool();
}

/// D4890: the buffers a test pinned by hand, unpinned when it ends on a failed assertion too. A test thread's private
/// refcounts die with it, its shared pins do not: box 30's run failed giveback_skips_pins_and_finishes_after_unpin before
/// its unpin loop, and the next give-back test then could not retire those extents ('did not finish in 10000 slices').
struct HeldPins(Vec<i32>);

impl Drop for HeldPins {
    fn drop(&mut self) {
        for id in self.0.drain(..) {
            unpin_id(id);
        }
    }
}

#[test]
fn giveback_skips_pins_and_finishes_after_unpin() {
    let _g = setup();
    restore_boot_pool();
    let _k = GivebackKnob::on();
    fill(9731, 9000);
    let active: Vec<i32> = (0..extent::n_extents()).filter(|&e| extent::state(e) == ACTIVE).collect();
    let unpinned = 3;
    let mut pinned = HeldPins(Vec::new());
    for &e in &active[unpinned..] {
        let (lo, hi) = extent::extent_range(e);
        let id = (lo..hi)
            .find(|&id| GetBufferDescriptor(id).state.load(Ordering::Relaxed) & BM_TAG_VALID != 0)
            .expect("filled extent has a valid buffer");
        pin_id(id);
        pinned.0.push(id);
    }
    resize_to(POOL_FLOOR_TARGET, 0);
    // D4890: slices until the give-back has retired what the unpinned extents hold (the state the next assertion
    // reads), at least 20 of them, instead of exactly 20: a slice is GIVEBACK_SLICE (10 ms) of work, so on a loaded box
    // 20 slices covered one unpinned extent of three (7936 active, 7424 wanted). Every slice still backs off and never
    // finishes with pins held; a drain that never gets there leaves the loop at 10_000 slices and fails the assertion.
    let unpinned_retired = TEST_NBUFFERS - unpinned as i32 * EXTENT_BUFFERS;
    for step in 0..10_000 {
        if step >= 20 && extent::active_buffers() <= unpinned_retired {
            break;
        }
        match crate::giveback_step().unwrap() {
            crate::GivebackStep::Again(ms) => {
                assert!(ms as i64 <= 50, "backs off, never waits on a pin");
                std::thread::sleep(std::time::Duration::from_millis(ms as u64));
            }
            crate::GivebackStep::Idle => panic!("finished with pins held"),
        }
    }
    assert_eq!(
        extent::active_buffers(),
        TEST_NBUFFERS - unpinned as i32 * EXTENT_BUFFERS,
        "the unpinned extents go first"
    );
    for id in &pinned.0 {
        assert!(GetBufferDescriptor(*id).state.load(Ordering::Relaxed) & BM_TAG_VALID != 0);
        assert_ne!(extent::state(id >> crate::extent::EXTENT_SHIFT), RETIRED, "a pinned extent is never retired");
    }
    for id in pinned.0.drain(..) {
        unpin_id(id);
    }
    run_background(10_000);
    assert_eq!(extent::active_buffers(), POOL_FLOOR_TARGET);
    assert_pool_consistent();
    restore_boot_pool();
}

#[test]
fn giveback_call_supersedes_and_knob_off_reverts() {
    let _g = setup();
    restore_boot_pool();
    let k = GivebackKnob::on();
    fill(9741, 8000);
    resize_to(4096, 0);
    assert!(draining_extents() > 0);
    let out = resize_to(TEST_NBUFFERS, 1000);
    assert_eq!(out.active_bytes, bytes_of(TEST_NBUFFERS));
    assert_eq!(draining_extents(), 0, "a resize call drops the pending give-back");
    assert_eq!(crate::giveback_stats().goal_bytes, 0);
    assert_eq!(crate::giveback_step().unwrap(), crate::GivebackStep::Idle);
    assert_pool_consistent();

    resize_to(4096, 0);
    assert!(draining_extents() > 0);
    drop(k);
    assert_eq!(crate::giveback_step().unwrap(), crate::GivebackStep::Idle, "knob off: the drain stops");
    assert_eq!(draining_extents(), 0, "and its extents go back to ACTIVE");
    assert_eq!(extent::active_buffers(), TEST_NBUFFERS);
    assert_pool_consistent();
    fill(9742, 8000);
    restore_boot_pool();
}

#[test]
fn giveback_request_frees_only_above_the_configured_size() {
    let _g = setup();
    restore_boot_pool();
    resize_to(TEST_POOL_MAX, 0);
    let _k = GivebackKnob::on();
    fill(9751, 15000);
    let above = TEST_POOL_MAX - TEST_NBUFFERS;
    assert_eq!(crate::giveback_reclaimable(), bytes_of(above) as u64);
    assert_eq!(crate::giveback_request(100), 0, "under one extent schedules nothing");
    let first = 8 * EXTENT_BUFFERS;
    assert_eq!(crate::giveback_request(bytes_of(first) as u64), bytes_of(first) as u64);
    assert_eq!(extent::active_buffers(), TEST_POOL_MAX, "a request never drains inline");
    assert_eq!(crate::giveback_landing(), bytes_of(first) as u64);
    assert_eq!(crate::giveback_reclaimable(), bytes_of(above - first) as u64);
    assert_eq!(crate::giveback_request(u64::MAX / 2), bytes_of(above - first) as u64, "capped at the configured size");
    assert_eq!(crate::giveback_request(1 << 30), 0, "nothing left above the configured size");
    assert_eq!(crate::giveback_reclaimable(), 0);

    run_background(10_000);
    assert_eq!(extent::active_buffers(), TEST_NBUFFERS);
    assert_eq!(crate::giveback_landing(), 0);
    assert_eq!(crate::giveback_stats().minimum_bytes, bytes_of(TEST_NBUFFERS));
    assert_pool_consistent();
    restore_boot_pool();
}

fn dirty_above(rel: u32, from_id: i32, n: usize) -> usize {
    let mut dirtied = 0;
    for (blk, id) in resident_blocks(rel) {
        if id >= from_id && dirtied < n {
            dirty_block(rel, blk);
            dirtied += 1;
        }
    }
    dirtied
}

#[test]
fn giveback_paces_dirty_writes_to_the_write_budget() {
    let _g = setup();
    setup_write_seams();
    restore_boot_pool();
    let _k = GivebackKnob::on();
    let k = mempolicy::consts::K::BufGivebackWriteBw;
    let rate = 8u64 << 20;
    mempolicy::consts::set(k, rate);
    fill(9761, 7500);
    let dirtied = dirty_above(9761, 4096, 512);
    assert!(dirtied >= 256, "dirtied {dirtied}");
    let t = std::time::Instant::now();
    resize_to(4096, 0);
    run_background(100_000);
    let secs = t.elapsed().as_secs_f64();
    let g = crate::giveback_stats();
    mempolicy::consts::set(k, mempolicy::consts::KNOBS[k as usize].default);
    assert_eq!(extent::active_buffers(), 4096);
    let bytes = (dirtied * BLCKSZ) as f64;
    let floor = (bytes - (rate / 10) as f64) / rate as f64;
    assert!(secs >= floor * 0.9, "{dirtied} dirty pages at {rate} B/s took {secs:.3} s, under {floor:.3} s");
    assert!(g.written_bytes >= bytes as i64);
    assert_pool_consistent();
    restore_boot_pool();
}

#[test]
fn giveback_leaves_unflushed_wal_pages_then_flushes_after_the_wait() {
    let _g = setup();
    setup_write_seams();
    restore_boot_pool();
    let _k = GivebackKnob::on();
    fill(9771, 7500);
    let dirtied = dirty_above(9771, 4096, 32);
    assert!(dirtied > 0);
    TEST_WAL_UNFLUSHED.store(true, Ordering::Relaxed);
    let t = std::time::Instant::now();
    resize_to(4096, 0);
    let mut later = 0;
    while t.elapsed() < std::time::Duration::from_millis(100) {
        if let crate::GivebackStep::Again(ms) = crate::giveback_step().unwrap() {
            later += 1;
            std::thread::sleep(std::time::Duration::from_millis(ms as u64));
        }
    }
    assert!(later > 0);
    assert!(extent::active_buffers() > 4096, "pages whose WAL is not flushed are not written in the first 100 ms");
    run_background(100_000);
    TEST_WAL_UNFLUSHED.store(false, Ordering::Relaxed);
    assert!(t.elapsed() >= std::time::Duration::from_millis(200), "the drain flushes WAL itself only after its wait");
    assert_eq!(extent::active_buffers(), 4096);
    assert_pool_consistent();
    restore_boot_pool();
}

#[test]
fn giveback_retires_extents_top_down_in_step_with_paced_writes() {
    let _g = setup();
    setup_write_seams();
    restore_boot_pool();
    let _k = GivebackKnob::on();
    let k = mempolicy::consts::K::BufGivebackWriteBw;
    mempolicy::consts::set(k, 4 << 20);
    resize_to(TEST_POOL_MAX, 0);
    let rel = 9781;
    fill(rel, 15000);
    let target = 4096;
    let mut per_extent = std::collections::HashMap::new();
    for (blk, id) in resident_blocks(rel) {
        let n = per_extent.entry(id >> crate::extent::EXTENT_SHIFT).or_insert(0);
        if id >= target && *n < 4 {
            dirty_block(rel, blk);
            *n += 1;
        }
    }
    let cut: Vec<i32> = (target >> crate::extent::EXTENT_SHIFT..extent::n_extents())
        .filter(|&e| extent::state(e) == ACTIVE)
        .collect();
    assert!(cut.len() > 4 * crate::resize::GIVEBACK_WINDOW, "cut {} extents", cut.len());
    resize_to(target, 0);
    let mut retired = HashSet::new();
    for slice in 0.. {
        assert!(slice < 100_000, "the give-back did not finish");
        let step = crate::giveback_step().unwrap();
        for &e in &cut {
            if extent::state(e) == RETIRED && retired.insert(e) {
                let above = cut.iter().filter(|&&a| a > e && extent::state(a) == DRAINING).count();
                assert!(
                    above < crate::resize::GIVEBACK_WINDOW,
                    "extent {e} retired with {above} extents above it still draining (slice {slice})"
                );
            }
        }
        match step {
            crate::GivebackStep::Idle => break,
            crate::GivebackStep::Again(ms) => std::thread::sleep(std::time::Duration::from_millis(ms as u64)),
        }
    }
    mempolicy::consts::set(k, mempolicy::consts::KNOBS[k as usize].default);
    assert_eq!(retired.len(), cut.len());
    assert_eq!(extent::active_buffers(), target);
    assert_pool_consistent();
    restore_boot_pool();
}

#[test]
fn giveback_relocation_never_writes_a_victim() {
    let _g = setup();
    setup_write_seams();
    restore_boot_pool();
    let _k = GivebackKnob::on();
    let rel = 9791;
    // D4890: start from an empty pool. Tests share it, serialized on TEST_LOCK in no fixed order, so which buffers fill
    // reuses depended on what the test before left resident (and its usage counts): box 16 got 1772 of the relation's
    // blocks below the cut, under the precondition's 2049
    crate::EvictAllUnpinnedBuffers().unwrap();
    fill(rel, 8000);
    let target = 4096;
    let mut below = Vec::new();
    for (blk, id) in resident_blocks(rel) {
        if id < target {
            dirty_block(rel, blk);
            below.push(blk);
        }
    }
    assert!(below.len() * 2 > target as usize, "most of the pool below the cut is dirty: {}", below.len());
    let writes0 = WRITES.lock().unwrap().len();
    let written0 = crate::resize::written();
    resize_to(target, 0);
    run_background(10_000);
    assert_eq!(extent::active_buffers(), target);
    assert_eq!(crate::resize::written(), written0, "nothing above the cut was dirty");
    assert_eq!(
        WRITES.lock().unwrap().len(),
        writes0,
        "a relocation never writes the dirty victim it would evict: the page above the cut is dropped instead"
    );
    let still_dirty = (0..target)
        .filter(|&id| GetBufferDescriptor(id).state.load(Ordering::Relaxed) & BM_DIRTY != 0)
        .count();
    assert!(still_dirty >= below.len(), "dirty below the cut: {still_dirty} of {}", below.len());
    assert_pool_consistent();
    restore_boot_pool();
}

/// L6: the memory ladder's mode for one test, back to off after it (a failed assert too).
struct LadderOn;

impl LadderOn {
    fn at(line: u64) -> LadderOn {
        mcx::ladder::publish(mcx::ladder::ON, line);
        LadderOn
    }
}

impl Drop for LadderOn {
    fn drop(&mut self) {
        mcx::ladder::publish(mcx::ladder::OFF, 0);
    }
}

/// L6 witness: under ladder pressure (C1's background trigger with the headroom under LOW, a 1 GiB limit: HIGH 102.4
/// MiB) a pool above its configured size gets a give-back request through the hooks `init_seams` installs, for what
/// the caches' elastic bytes cannot cover, after them; with buf_giveback = 0 none, and the pool never moves.
#[test]
fn ladder_pressure_asks_the_pool_above_its_configured_size_only_with_buf_giveback() {
    let _g = setup();
    let _off = GivebackKnob::off();
    restore_boot_pool();
    resize_to(TEST_POOL_MAX, 0);
    let (gib, mib) = (1u64 << 30, 1u64 << 20);
    let (limit, line) = (gib, gib - 64 * mib);
    let _l = LadderOn::at(line);
    let high = mempolicy::ladder::marks(limit).1;
    let elastic = core::cell::Cell::new(0u64);
    let reclaim = |n: u64| {
        let got = n.min(elastic.get());
        elastic.set(elastic.get() - got);
        got
    };
    // the read at the line: no headroom, so the drain aims at HIGH
    let squeeze = |cache: u64| {
        elastic.set(cache);
        mcx::ladder::background_pass_with(&reclaim, &|| elastic.get(), limit, line, line, false, None)
    };
    squeeze(0);
    assert_eq!(crate::giveback_landing(), 0, "buf_giveback = 0: no request");
    assert_eq!(crate::giveback_stats().goal_bytes, 0);
    {
        let _k = GivebackKnob::on();
        squeeze(2 * high);
        assert_eq!(crate::giveback_landing(), 0, "the caches' elastic bytes cover the drain: the pool stays");
        squeeze(80 * mib);
        let want = (high - 80 * mib).div_ceil(2 * mib) * 2 * mib;
        assert_eq!(crate::giveback_landing(), want, "the pool is asked for what the caches cannot cover, in whole extents");
        assert_eq!(extent::active_buffers(), TEST_POOL_MAX, "a request never drains inline");
        run_background(10_000);
        assert_eq!(extent::active_buffers() as u64, TEST_POOL_MAX as u64 - want / BLCKSZ as u64);
        assert!(extent::active_buffers() >= TEST_NBUFFERS, "never under the configured size");
        assert_pool_consistent();
    }
    restore_boot_pool();
}

/// buf_grow (user 10-18: the buffer pool grows into free memory): with the ladder on at the start and no explicit
/// pgrust.buffer_pool_max, the reservation is buf_grow_max_pct (75) of physical memory in whole extents; the ladder off,
/// either knob off, or a reservation not past shared_buffers: shared_buffers exactly (0, the inert shape).
#[test]
fn buf_grow_reservation_is_three_quarters_of_memory_with_the_ladder_on_at_the_start() {
    use mempolicy::consts::{set, K};
    let _g = setup();
    let _k = GivebackKnob::on();
    let gib = 1u64 << 30;
    assert_eq!(extent::grow_max_buffers(16384, false, 32 * gib), 0, "the ladder off at the start: shared_buffers exactly");
    assert_eq!(extent::grow_max_buffers(16384, true, 32 * gib), (24 * gib / BLCKSZ as u64) as i32, "75 % of 32 GiB");
    assert_eq!(extent::grow_max_buffers(4_000_000, true, 32 * gib), 0, "not past shared_buffers: inert");
    set(K::BufGrow, 0);
    assert_eq!(extent::grow_max_buffers(16384, true, 32 * gib), 0, "buf_grow 0");
    set(K::BufGrow, 1);
    set(K::BufGiveback, 0);
    assert_eq!(extent::grow_max_buffers(16384, true, 32 * gib), 0, "buf_giveback 0: nothing could give it back");
    set(K::BufGiveback, 1);
    let engaged = extent::plan_geometry(16384, extent::grow_max_buffers(16384, true, 32 * gib), false).unwrap();
    assert_eq!((engaged.boot_buffers, engaged.max_buffers), (16384, 3_145_728), "boot at shared_buffers, room to grow");
}

/// buf_grow: the ladder's grow request raises a goal in whole extents and wakes the background writer, whose next
/// cycle activates the extents (counted); capped at the reservation; nothing while a give-back drains or with the knob
/// off; the grown extents are the give-back's to take back. The pool's evicted bytes (the ladder's demand) count the
/// valid pages a miss reused.
#[test]
fn buf_grow_a_request_is_applied_by_the_background_writer_within_the_reservation() {
    use mempolicy::consts::{set, K};
    let _g = setup();
    let _k = GivebackKnob::on();
    set(K::BufGrow, 1);
    restore_boot_pool();
    let ext = bytes_of(EXTENT_BUFFERS) as u64;
    let evicted0 = crate::evicted_bytes();
    fill(9811, TEST_NBUFFERS as u32 + 2000);
    assert!(crate::evicted_bytes() >= evicted0 + 1000 * BLCKSZ as u64, "a full pool's misses evict valid pages, counted");
    let (grows0, grown0) = crate::grow_counts();
    assert_eq!(crate::grow_request(ext / 2), 0, "under one extent: nothing");
    assert_eq!(crate::grow_request(4 * ext), 4 * ext, "four extents scheduled");
    assert_eq!(extent::active_buffers(), TEST_NBUFFERS, "not applied until the background writer's cycle");
    assert_eq!(crate::giveback_step().unwrap(), crate::GivebackStep::Idle);
    assert_eq!(extent::active_buffers(), TEST_NBUFFERS + 4 * EXTENT_BUFFERS, "applied");
    assert_eq!(crate::grow_counts(), (grows0 + 1, grown0 + 4 * ext));
    assert_pool_consistent();
    fill(9812, 4 * EXTENT_BUFFERS as u32);
    assert!(resident_blocks(9812).iter().any(|&(_, id)| id >= TEST_NBUFFERS), "the grown extents take pages");
    let top = bytes_of(TEST_POOL_MAX - extent::active_buffers()) as u64;
    assert_eq!(crate::grow_request(u64::MAX / 4), top, "capped at the reservation");
    assert_eq!(crate::giveback_step().unwrap(), crate::GivebackStep::Idle);
    assert_eq!(extent::active_buffers(), TEST_POOL_MAX);
    assert_eq!(crate::grow_request(ext), 0, "at the reservation's top: nothing");
    assert_eq!(crate::giveback_reclaimable(), bytes_of(TEST_POOL_MAX - TEST_NBUFFERS) as u64, "grown extents: the give-back's");
    assert!(crate::giveback_request(4 * ext) > 0, "the ladder takes some back");
    assert_eq!(crate::grow_request(ext), 0, "a give-back draining: no grow");
    run_background(10_000);
    set(K::BufGrow, 0);
    assert_eq!(crate::grow_request(ext), 0, "buf_grow 0: nothing");
    set(K::BufGrow, 1);
    restore_boot_pool();
}
