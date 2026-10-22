use gravity_abi::breaker::{HashAgg, HashBuild, BUILD_PARTITION, BUILD_TABLE, HASHBUILD_ENTRY, HASHBUILD_HDR};
use gravity_abi::buildrows::{RowStore, ROWSTORE_HDR, SLICE_HDR};

/// A spool-encoded row without images: the length word, the values, one null word.
fn row(values: &[u64]) -> Vec<u8> {
    let words = 1 + values.len() + 1;
    let mut out = Vec::new();
    out.extend((words as u64).to_ne_bytes());
    for v in values {
        out.extend(v.to_ne_bytes());
    }
    out.extend(0u64.to_ne_bytes());
    out
}

fn region(bytes: usize) -> Vec<u64> {
    vec![0u64; bytes.div_ceil(8)]
}

#[test]
fn append_at_and_part_range_over_four_slices() {
    let mut buf = region(RowStore::bytes(64, 32, 4));
    let s = unsafe { RowStore::open(buf.as_mut_ptr() as *mut u8, buf.len() * 8, 4, 32) }.expect("room");
    assert_eq!((s.parts, s.row_hdr, s.len), (4, 32, 0));
    let a = s.append(1, &row(&[7, 8])).expect("room");
    let b = s.append(1, &row(&[9, 10])).expect("room");
    let c = s.append(3, &row(&[11])).expect("room");
    assert_eq!(s.len, 3);
    assert_eq!(s.at(a), &row(&[7, 8])[..]);
    assert_eq!(s.at(b), &row(&[9, 10])[..]);
    assert_eq!(s.at(c), &row(&[11])[..]);
    let (lo, hi) = s.part_range(1);
    assert_eq!((lo, hi - lo), (a - SLICE_HDR as u32, SLICE_HDR as u32 + 2 * 32));
    assert_eq!(u64::from_ne_bytes(s.part_bytes(1)[..8].try_into().unwrap()), lo as u64, "a slice image carries its origin");
    assert_eq!(s.part_range(0), (0, SLICE_HDR as u32));
    // a slice fills alone
    let mut n = 2;
    while s.append(1, &row(&[0, 0])).is_some() {
        n += 1;
    }
    assert!(n >= 8 && s.append(0, &row(&[1])).is_some(), "the other slices keep their room");
    s.clear();
    assert_eq!((s.len, s.part_range(1)), (0, (lo, lo + SLICE_HDR as u32)));
}

#[test]
fn a_carved_store_keeps_the_table_below_it() {
    let total = 64 << 10;
    let mut buf = region(total);
    let p = buf.as_mut_ptr() as *mut u8;
    unsafe { HashAgg::arm(p, total as u32) };
    let split = RowStore::split(total as u32, 48);
    let s = unsafe { RowStore::carve(p, 48, 8) }.expect("room");
    assert_eq!((s.parts, s.row_hdr), (8, 48));
    let b = unsafe { HashBuild::view(p) };
    assert_eq!(b.pad2, total as u32 - split);
    assert_eq!(unsafe { HashBuild::armed(p) }, total as u32 - split, "the arm word shrinks by the store");
    let t = unsafe { HashBuild::at_share_sized(p, BUILD_TABLE, 16) };
    assert!(HASHBUILD_HDR + t.max_cap() as usize * HASHBUILD_ENTRY <= t.pad2 as usize, "no growth reaches the store");
    while t.grow() {}
    let s = unsafe { RowStore::of_mut(p) }.expect("found through pad2");
    let off = s.append(s.part_for(5), &row(&[5, 55])).expect("room");
    assert!(t.insert(5, off as i64));
    let hit = t.probe(5).next().expect("the entry");
    assert_eq!(unsafe { RowStore::of(p) }.unwrap().at(hit.payload as u32), &row(&[5, 55])[..]);
    assert!(unsafe { RowStore::carve(p, 48, 8) }.is_some(), "carving twice finds the same store");
}

#[test]
fn a_laid_out_partition_over_its_whole_region_has_no_room() {
    let total = 8 << 10;
    let mut buf = region(total);
    let p = buf.as_mut_ptr() as *mut u8;
    unsafe { HashAgg::arm(p, total as u32) };
    unsafe { HashBuild::at_share(p, BUILD_PARTITION) };
    assert!(unsafe { RowStore::carve(p, 48, 4) }.is_none());
    assert!(unsafe { RowStore::of(p) }.is_none());
    // a table laid out small inside a region keeps room past its entries
    let total = 64 << 10;
    let mut buf = region(total);
    let p = buf.as_mut_ptr() as *mut u8;
    let t = unsafe { HashBuild::at_sized(p, total as u32, BUILD_TABLE, 16) };
    assert!(HashBuild::bytes(t.cap) <= total as u32 / 2, "the sized table leaves room");
    let s = unsafe { RowStore::carve(p, 16, 4) }.expect("room past the entries");
    assert!(s.cap as usize > ROWSTORE_HDR);
    let t = unsafe { HashBuild::view(p) };
    assert!(HashBuild::bytes(t.cap) <= t.region && t.region == t.pad2);
    assert_eq!(RowStore::split(total as u32, 0), 0, "no row bytes, no store");
}

#[test]
fn three_partials_merge_with_their_offsets_rebased() {
    let mut leader = region(RowStore::bytes(96, 32, 4));
    let l = unsafe { RowStore::open(leader.as_mut_ptr() as *mut u8, leader.len() * 8, 4, 32) }.unwrap();
    let mut all: Vec<(u16, u32, Vec<u8>)> = Vec::new();
    let mut bufs = Vec::new();
    for i in 0..3u64 {
        bufs.push(region(RowStore::bytes(16, 32, 4)));
        let b = bufs.last_mut().unwrap();
        let p = unsafe { RowStore::open(b.as_mut_ptr() as *mut u8, b.len() * 8, 4, 32) }.unwrap();
        let mut offs = Vec::new();
        for j in 0..4u64 {
            let part = ((i + j) % 4) as u16;
            let r = row(&[i, j]);
            offs.push((part, p.append(part, &r).unwrap(), r));
        }
        let mut rebase = [0u32; 4];
        assert!(l.room_for(p) && l.merge_from(p, &mut rebase));
        for (part, off, r) in offs {
            all.push((part, off.wrapping_add(rebase[part as usize]), r));
        }
    }
    assert_eq!(l.len, 12);
    for (part, off, r) in &all {
        assert_eq!(l.at(*off), &r[..]);
        let (lo, hi) = l.part_range(*part);
        assert!(lo <= *off && *off < hi);
    }
    let mut mismatch = region(RowStore::bytes(16, 32, 8));
    let m = unsafe { RowStore::open(mismatch.as_mut_ptr() as *mut u8, mismatch.len() * 8, 8, 32) }.unwrap();
    assert!(!l.merge_from(m, &mut [0u32; 8]), "the partition counts must agree");
}

#[test]
fn a_serial_partition_hashes_in_place_at_its_rows_size_and_grows_to_its_region() {
    let total = 1 << 20;
    let mut buf = region(total);
    let p = buf.as_mut_ptr() as *mut u8;
    unsafe { HashAgg::arm(p, total as u32) };
    let b = unsafe { HashBuild::at_share(p, BUILD_PARTITION) };
    assert!(b.cap > 60_000, "a 1 MiB partition holds tens of thousands of entries");
    for k in 0..12i32 {
        assert!(b.insert(k * 7, k as i64));
    }
    assert!(b.hash_in_place());
    assert_eq!((b.table, b.cap), (BUILD_TABLE, 1024), "the table is sized for its 12 rows, not the region");
    assert_eq!(HashBuild::table_cap_of(HashBuild::bytes(b.max_cap())), b.max_cap());
    for k in 0..12i32 {
        assert_eq!(b.probe(k * 7).next().map(|e| e.payload), Some(k as i64));
    }
    let mut doublings = 0;
    while b.grow() {
        doublings += 1;
    }
    assert!(doublings >= 5 && b.cap >= 32768, "grows to the region ({} doublings, cap {})", doublings, b.cap);
    for k in 0..12i32 {
        assert_eq!(b.probe(k * 7).next().map(|e| e.payload), Some(k as i64));
    }
    let total = 4096;
    let mut buf = region(total);
    let p = buf.as_mut_ptr() as *mut u8;
    unsafe { HashAgg::arm(p, total as u32) };
    let b = unsafe { HashBuild::at_share(p, BUILD_PARTITION) };
    let n = b.cap;
    for k in 0..n as i32 {
        assert!(b.insert(k, 0));
    }
    assert!(!b.hash_in_place(), "past 7/8 of the region's largest table the caller spills");
}

#[test]
fn a_small_build_hashes_at_one_eighth_load_and_a_tight_region_keeps_the_seven_eighths_table() {
    let open = |total: usize, n: i32| {
        let mut buf = region(total);
        let p = buf.as_mut_ptr() as *mut u8;
        unsafe { HashAgg::arm(p, total as u32) };
        let b = unsafe { HashBuild::at_share(p, BUILD_PARTITION) };
        for k in 0..n {
            assert!(b.insert(k * 7, k as i64));
        }
        let hashed = b.hash_in_place();
        let cap = b.cap;
        let found = (0..n).all(|k| b.probe(k * 7).next().map(|e| e.payload) == Some(k as i64));
        (hashed, cap, found)
    };
    assert_eq!(open(1 << 20, 1000), (true, 8192, true), "1000 rows at 1/8 load");
    assert_eq!(open(1 << 20, 2048), (true, 16384, true), "the largest sparse build");
    assert_eq!(open(1 << 20, 2049), (true, 4096, true), "past it: 7/8 load");
    let tight = HashBuild::bytes(8191) as usize;
    let (hashed, cap, found) = open(tight, 2048);
    assert_eq!((hashed, cap, found), (true, 4096, true), "a region short of 1/8 degrades to the 7/8 table it holds");
    assert_eq!(HashBuild::table_cap_for(12), 1024);
}
