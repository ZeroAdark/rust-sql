//! IXCOPYPATH: a manifest decode and an install-only publish's rebuild of the row-ID records allocate the same at any
//! part count (IX0 L2+L3 allocated one ranges list per part in each, on every small COPY's publish). Its own test
//! binary: the counting allocator is process-wide, the count per thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use pgrc_format::geom::{band_count, granule_count};
use pgrc_format::manifest::{flat_ranges, rebuild_ranges, with_next_row_id, with_ranges, IdRanges, Manifest, ManifestHeader, PartRecord, MANIFEST_MAGIC};
use pgrc_format::rowid::{align_up, ROWID_FIRST};
use pgrc_format::FORMAT_VERSION;

struct Counting;

thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

fn count() {
    let _ = ALLOCS.try_with(|n| n.set(n.get() + 1));
}

// SAFETY: forwards to `System`; the counter is a const-initialized thread-local `Cell` (no allocation, no destructor).
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        count();
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static A: Counting = Counting;

/// The allocations (and reallocations) `f` makes on this thread.
fn allocs<T>(f: impl FnOnce() -> T) -> (u64, T) {
    let n0 = ALLOCS.with(Cell::get);
    let t = f();
    (ALLOCS.with(Cell::get) - n0, t)
}

const ROWS: u64 = 100_000;

fn part(part_no: u32) -> PartRecord {
    PartRecord { rows: ROWS, file_len: 4_000_000, footer_off: 3_999_888, dv_gen: 0, dv_len: 0, part_no, flags: 0, granule_count: granule_count(ROWS), band_count: band_count(ROWS), dv_crc: 0, granule_rows: 0 }
}

/// Part `no`'s bulk range: the table's parts hold consecutive grants.
fn ids(no: u32) -> IdRanges {
    let lo = ROWID_FIRST + u64::from(no) * align_up(ROWS);
    vec![(lo, lo + align_up(ROWS))]
}

/// A generation of `n` bulk parts (part numbers 0..n), each homing its own range, the floor at the last one's end.
fn generation(n: u32) -> Manifest {
    let table: Vec<(u32, IdRanges)> = (0..n).map(|no| (no, ids(no))).collect();
    let ext = with_next_row_id(&with_ranges(&[], &table).unwrap(), ids(n)[0].0).unwrap();
    let header = ManifestHeader {
        gen: 7,
        prev_gen: 6,
        publisher_fxid: 9,
        relfilenumber: 90210,
        schema_fingerprint: 1,
        magic: MANIFEST_MAGIC,
        format_version: FORMAT_VERSION,
        spc: 1663,
        db: 5,
        part_count: n,
        next_part_no: n,
        flags: 0,
        reserved: 0,
    };
    Manifest::new(header, (0..n).map(part).collect()).with_ext(ext)
}

#[test]
fn a_manifest_decode_allocates_the_same_at_any_part_count() {
    let at = |n: u32| {
        let bytes = generation(n).encode();
        let (k, m) = allocs(|| Manifest::decode(&bytes));
        assert_eq!(m.expect("decodes").parts.len(), n as usize);
        k
    };
    let (small, big) = (at(500), at(4_000));
    assert!(big <= small + 2, "decode allocations: {small} at 500 parts, {big} at 4,000");
}

#[test]
fn an_install_only_rebuild_allocates_the_same_at_any_part_count() {
    let at = |n: u32| {
        let m = generation(n);
        let mut parts = m.parts.clone();
        parts.push(part(n));
        let add = vec![(n, ids(n))];
        let (k, ext) = allocs(|| rebuild_ranges(&m.ext, &parts, &add));
        let mut header = m.header;
        header.part_count = n + 1;
        header.next_part_no = n + 1;
        let next = Manifest::new(header, parts).with_ext(ext.expect("rebuilds"));
        assert!(Manifest::decode(&next.encode()).is_ok(), "the new generation decodes");
        k
    };
    let (small, big) = (at(500), at(4_000));
    assert!(big <= small + 2, "rebuild allocations: {small} at 500 parts, {big} at 4,000");
}

/// The catalog's read of a generation's ranges (Generation::linked) is one list, not one per part.
#[test]
fn the_flat_ranges_of_a_generation_allocate_the_same_at_any_part_count() {
    let at = |n: u32| {
        let m = generation(n);
        let (k, f) = allocs(|| flat_ranges(&m));
        let f = f.expect("reads");
        assert_eq!((f.at.len(), f.of(n as usize - 1)), (n as usize + 1, &ids(n - 1)[..]));
        k
    };
    let (small, big) = (at(500), at(4_000));
    assert!(big <= small + 2, "flat_ranges allocations: {small} at 500 parts, {big} at 4,000");
}
