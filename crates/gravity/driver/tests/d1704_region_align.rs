//! D1704 (found by D1702's Miri run): the driver's regions were `Vec<u8>` (alignment 1) holding 8-aligned tables, and a
//! table's typed view over one (`HashAgg::at`, a `&mut HashAgg`) needs the 8. Natively every block came 16-aligned (the
//! server's mimalloc, glibc's malloc), so plain `cargo test` passes either way; Miri places a block at exactly the alignment
//! asked, and the defect shows under
//! `MIRIFLAGS=-Zmiri-tree-borrows cargo +nightly miri test -p gravity_driver --test d1704_region_align`.
//! A region is now one allocation at `node::REGION_ALIGN`, freed with the same layout (Miri checks the free's layout too).
//! The test names only what the parent has too (its proof runs it there).
use gravity_abi::breaker::{HashAgg, RecordLayout, SlotKind, HASHAGG_HDR};
use gravity_driver::{try_region, RegionBuf, REGIONS};

/// The widest word a table lays (`u64`): `node::REGION_ALIGN`.
const ALIGN: usize = 8;

#[test]
fn every_region_is_8_aligned_under_its_tables_view() {
    let layout = RecordLayout::new(&[], &[SlotKind::Count]);
    let mut held: Vec<RegionBuf> = Vec::new();
    for i in 0..24usize {
        let bytes = 4096 + 8 * i + i % 5;
        let mut r = if i % 2 == 0 { try_region(bytes) } else { REGIONS.take(bytes, None) }.expect("the unarmed pool draws");
        assert!(r.len() >= bytes && r.capacity() == r.len(), "region {i}: {} B for {bytes}", r.len());
        let p = r.as_mut_ptr();
        assert_eq!(p as usize % ALIGN, 0, "region {i} ({bytes} B) at {p:p}");
        // SAFETY: `r` holds at least `bytes` (> the table) writable bytes, 8-aligned (asserted); its header zeroed here, as
        // the driver zeroes a region's prefix at every open.
        unsafe {
            core::ptr::write_bytes(p, 0, HASHAGG_HDR);
            HashAgg::at(p, &layout, 1, bytes as u32).single().add_count(0, i as i64 + 1);
            assert_eq!(HashAgg::at(p, &layout, 1, bytes as u32).single().count(0), i as i64 + 1);
        }
        held.push(r);
    }
    let empty = try_region(0).expect("an empty region");
    assert!(empty.is_empty() && RegionBuf::empty().is_empty() && RegionBuf::default().is_empty());
}
