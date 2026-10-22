//! D1458: the owner regions' pointer accessors under Tree Borrows. `Owned::owner` and `OwnerTop::records` hand out `*mut u8`
//! into the region behind their header, derived from `&self`; the owner phase writes its records through them while the
//! header's `&` view is live. A pointer derived from a shared reference to a `Freeze` type is read-only (Tree Borrows; and to
//! LLVM, a `&` argument is `readonly noalias`, D1358's store-drop): the headers carry an `UnsafeCell` marker so a shared view
//! no longer freezes the region behind it. Plain `cargo test` passes either way; the defect shows only under
//! `MIRIFLAGS=-Zmiri-tree-borrows cargo +nightly miri test -p gravity_abi --test d1458_owner_region`.
//! The headers are laid through raw pointers (no struct literal), so the test compiles on either side of the fix.

use gravity_abi::breaker::{Owned, OwnerTop, OWNED_HDR, OWNERTOP_HDR};

#[test]
fn d1458_an_owner_top_view_hands_out_writable_records() {
    let mut buf = vec![0u64; (OWNERTOP_HDR + 64) / 8];
    let p = buf.as_mut_ptr() as *mut u8;
    unsafe { (*(p as *mut OwnerTop)).n = 2 };
    let top = unsafe { OwnerTop::view(p) };
    let r = top.records() as *mut u64;
    unsafe {
        *r = 11;
        *r.add(1) = 22;
    }
    assert_eq!(top.n, 2);
    assert_eq!(unsafe { (*r, *r.add(1)) }, (11, 22));
}

#[test]
fn d1458_an_owned_view_hands_out_writable_owner_regions() {
    let owner_bytes = 32usize;
    let mut buf = vec![0u64; (OWNED_HDR + 2 * owner_bytes) / 8];
    let p = buf.as_mut_ptr() as *mut u8;
    unsafe {
        (*(p as *mut Owned)).nowners = 2;
        (*(p as *mut Owned)).owner_bytes = owner_bytes as u32;
    }
    let o = unsafe { Owned::view(p) };
    for i in 0..2 {
        unsafe { *(o.owner(i) as *mut u64) = 100 + i as u64 };
    }
    assert_eq!(o.nowners, 2);
    assert_eq!(unsafe { (*(o.owner(0) as *const u64), *(o.owner(1) as *const u64)) }, (100, 101));
}
