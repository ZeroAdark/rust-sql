//! D2869 (ITEM 1, bar's TH13 W8 in the gate's order): over `REGION_DEPOT_BYTES` a put dropped the largest classes first, so a
//! statement whose regions were the largest lost them to the cap and mapped them fresh at its next run (TH13 W8 after TH10
//! W8: ~1 GiB of fresh huge pages a run). Now the cap drops the least recently put regions (D405's rule), never the one just
//! put. One test: the depot is process-wide.

use gravity_driver::{try_region, REGIONS, REGION_DEPOT_BYTES};

fn put_new(bytes: usize) -> usize {
    let buf = try_region(bytes).expect("unarmed pool");
    let p = buf.as_ptr() as usize;
    REGIONS.put(buf);
    p
}

#[test]
fn the_cap_drops_the_least_recently_put_regions() {
    REGIONS.trim();
    assert_eq!(REGIONS.stats().bytes, 0);

    // two older smaller regions, then two newer larger ones past the cap: the older ones go, not the largest class
    let quarter = REGION_DEPOT_BYTES / 16;
    let half = REGION_DEPOT_BYTES / 2;
    let before = REGIONS.stats().evictions;
    let small = [put_new(quarter), put_new(quarter)];
    let large = [put_new(half), put_new(half)];
    let s = REGIONS.stats();
    assert_eq!((s.entries, s.bytes), (2, REGION_DEPOT_BYTES), "the two least recently put regions dropped, down to the cap");
    assert_eq!(s.evictions, before + 2);
    let kept: Vec<usize> = std::iter::from_fn(|| REGIONS.take_parked(half, None, None)).map(|b| b.as_ptr() as usize).collect();
    assert!(large.iter().all(|p| kept.contains(p)), "the larger regions put last stay parked");
    assert!(REGIONS.take_parked(quarter, None, None).is_none(), "the smaller regions put first dropped: {small:x?}");
    assert_eq!(REGIONS.stats().bytes, 0);

    REGIONS.trim();
}
