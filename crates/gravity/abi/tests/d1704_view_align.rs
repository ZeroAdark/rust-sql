//! D1704: every typed view over region bytes (`HashAgg::at` and its siblings) asserts its base's alignment in debug builds.
//! The compiler's own misaligned-dereference check skips a borrow through a raw pointer, so a region at 4 mod 8 (a
//! `Vec<u8>` region under Miri) made a `&mut HashAgg` with no word. Each view here gets a base 2 past an 8-aligned word
//! (misaligned for its type, 8- or 4-aligned; `HashAgg::at` also 4 past, the Miri case) and panics by name before it
//! reads or writes a byte; the aligned base serves.
#![cfg(debug_assertions)]

use std::panic::{catch_unwind, AssertUnwindSafe};

use gravity_abi::breaker::{Candidates, DenseFront, HashAgg, HashBuild, Owned, OwnerTop, Rec, RecordLayout, SlotKind, SortRun, TopK, T1};
use gravity_abi::buildrows::RowStore;

fn panics_by_name(what: &str, f: impl FnOnce()) {
    let e = catch_unwind(AssertUnwindSafe(f)).expect_err(what);
    let m = e.downcast_ref::<String>().map(String::as_str).or_else(|| e.downcast_ref::<&str>().copied()).unwrap_or("");
    assert!(m.contains("D1704") && m.contains("aligned"), "{what}: {m}");
}

#[test]
fn every_typed_view_asserts_its_alignment() {
    let mut words = vec![0u64; 1024];
    let base = words.as_mut_ptr() as *mut u8;
    let layout = RecordLayout::new(&[], &[SlotKind::Count]);
    // SAFETY (every call below): each view panics at its alignment assert before it touches a byte; `words` outlives them.
    let (p4, p) = unsafe { (base.add(4), base.add(2)) };
    panics_by_name("HashAgg::at at 4 mod 8", || {
        let _ = unsafe { HashAgg::at(p4, &layout, 1, 4096) };
    });
    panics_by_name("HashAgg::at", || {
        let _ = unsafe { HashAgg::at(p, &layout, 1, 4096) };
    });
    panics_by_name("HashAgg::view", || {
        let _ = unsafe { HashAgg::view(p) };
    });
    panics_by_name("T1::at", || {
        let _ = unsafe { T1::at(p, &layout, 0, 4096, 4096) };
    });
    panics_by_name("T1::view", || {
        let _ = unsafe { T1::view(p) };
    });
    panics_by_name("Owned::view", || {
        let _ = unsafe { Owned::view(p) };
    });
    panics_by_name("OwnerTop::view", || {
        let _ = unsafe { OwnerTop::view(p) };
    });
    panics_by_name("SortRun::at_share", || {
        let _ = unsafe { SortRun::at_share(p, 8) };
    });
    panics_by_name("SortRun::at_rows", || {
        let _ = unsafe { SortRun::at_rows(p, 8, 1) };
    });
    panics_by_name("SortRun::region_bytes", || {
        let _ = unsafe { SortRun::region_bytes(p, 8) };
    });
    panics_by_name("DenseFront::at", || {
        let _ = unsafe { DenseFront::at(p, 0, 1) };
    });
    panics_by_name("HashBuild::at", || {
        let _ = unsafe { HashBuild::at(p, 1, 4096) };
    });
    panics_by_name("HashBuild::view", || {
        let _ = unsafe { HashBuild::view(p) };
    });
    panics_by_name("HashBuild::armed", || {
        let _ = unsafe { HashBuild::armed(p) };
    });
    panics_by_name("HashBuild::at_sized", || {
        let _ = unsafe { HashBuild::at_sized(p, 4096, 4096, 1) };
    });
    panics_by_name("TopK::at_cap", || {
        let _ = unsafe { TopK::at_cap(p, 1, 1, 1, 0) };
    });
    panics_by_name("TopK::view", || {
        let _ = unsafe { TopK::view(p) };
    });
    panics_by_name("Rec::at", || {
        let _ = unsafe { Rec::at(p, &layout) };
    });
    panics_by_name("Candidates::at", || {
        let _ = unsafe { Candidates::at(p, 1, 0, 0) };
    });
    panics_by_name("RowStore::open", || {
        let _ = unsafe { RowStore::open(p, 4096, 1, 0) };
    });
    panics_by_name("RowStore::view", || {
        let _ = unsafe { RowStore::view(p) };
    });
    // the aligned base: the same view serves (a zeroed region, laid out by `at`)
    let t = unsafe { HashAgg::at(base, &layout, 1, 4096) };
    t.single().add_count(0, 3);
    assert_eq!(t.single().count(0), 3);
}
