use core::sync::atomic::Ordering;

use types_core::BLCKSZ;
use types_storage::buf::BM_TAG_VALID;

use super::*;
use crate::extent::{self, EXTENT_BUFFERS, EXTENT_BYTES, RETIRED};
use crate::resize::BufferPoolResize;

fn fill(rel: u32, nblocks: u32) {
    for blk in 0..nblocks {
        ReleaseBuffer(read_blk(rel, blk)).unwrap();
    }
}

/// The VmFlags of the mapping holding `addr` (/proc/self/smaps).
fn vm_flags(addr: usize) -> Vec<String> {
    let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap();
    let mut inside = false;
    for line in smaps.lines() {
        if let Some((range, _)) = line.split_once(' ') {
            if let Some((lo, hi)) = range.split_once('-') {
                if let (Ok(lo), Ok(hi)) = (usize::from_str_radix(lo, 16), usize::from_str_radix(hi, 16)) {
                    inside = lo <= addr && addr < hi;
                    continue;
                }
            }
        }
        if inside {
            if let Some(f) = line.strip_prefix("VmFlags:") {
                return f.split_whitespace().map(str::to_string).collect();
            }
        }
    }
    panic!("no mapping holds {addr:#x}");
}

/// D4685 (CVE-2026-74674): a retired extent's blocks (a whole 2 MiB-aligned range) are mapped over afresh, never
/// MADV_DONTNEED'd; the range reads zero after, and the fresh mapping keeps the reservation's MAP_NORESERVE and its core
/// exclusion.
#[test]
fn d4685_release_maps_over_and_keeps_the_reservation_flags() {
    let _g = setup();
    TEST_DRAIN_RATE.store(0, Ordering::Relaxed);
    let out = BufferPoolResize(TEST_NBUFFERS as i64 * BLCKSZ as i64, 10_000).unwrap();
    assert_eq!(out.shortfall_bytes, 0);
    fill(9801, 8000);
    let top = TEST_NBUFFERS / EXTENT_BUFFERS - 1;
    let lo = top * EXTENT_BUFFERS;
    assert!((lo..lo + EXTENT_BUFFERS).any(|id| GetBufferDescriptor(id).state.load(Ordering::Relaxed) & BM_TAG_VALID != 0));
    let p = BufferGetBlockPtr(lo + 1) as usize;
    assert_eq!(p % EXTENT_BYTES, 0, "an extent's blocks are one whole 2 MiB-aligned range");
    let before = vm_flags(p);
    assert!(before.iter().any(|f| f == "nr"), "the reservation is MAP_NORESERVE: {before:?}");
    if std::env::var_os("PGRUST_CORE_DUMP_BUFFERS").is_none() {
        assert!(before.iter().any(|f| f == "dd"), "the reservation is out of the core: {before:?}");
    }

    let out = BufferPoolResize((TEST_NBUFFERS - EXTENT_BUFFERS) as i64 * BLCKSZ as i64, 10_000).unwrap();
    assert_eq!(out.shortfall_bytes, 0);
    assert_eq!(extent::state(top), RETIRED);
    // SAFETY: the retired extent's range stays mapped (read-write, inside the reservation); nothing writes it.
    let bytes = unsafe { core::slice::from_raw_parts(p as *const u8, EXTENT_BYTES) };
    assert!(bytes.iter().all(|&b| b == 0), "a released extent reads zero");
    let after = vm_flags(p);
    assert!(after.iter().any(|f| f == "nr"), "MAP_NORESERVE kept: {after:?}");
    assert!(after.iter().any(|f| f == "rd") && after.iter().any(|f| f == "wr"), "read-write kept: {after:?}");
    assert_eq!(
        after.iter().any(|f| f == "dd"),
        before.iter().any(|f| f == "dd"),
        "core exclusion as at reserve: {before:?} -> {after:?}"
    );

    let out = BufferPoolResize(TEST_NBUFFERS as i64 * BLCKSZ as i64, 10_000).unwrap();
    assert_eq!(out.shortfall_bytes, 0);
    fill(9802, 8000);
}
