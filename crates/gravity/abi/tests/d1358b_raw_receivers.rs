//! D1358b: the phase-1 table's raw-receiver functions and the dense front's writers under Tree Borrows. Before the fix,
//! `T1::probe` / `evict` / `drain` held a `&T1` taken from the raw table pointer across their own header writes (`len`,
//! `inserts`, `next_block`), and the next use of that reference reborrowed a location the write had disabled; `DenseFront::set`
//! / `clear` wrote through a pointer derived from `&self`. Plain `cargo test` passes either way; the defect shows only under
//! `MIRIFLAGS=-Zmiri-tree-borrows cargo +nightly miri test -p gravity_abi --test d1358b_raw_receivers`.

use gravity_abi::breaker::{DenseFront, KeyKind, Rec, RecordLayout, SlotKind, T1};
use gravity_abi::hash::KeyHash;

#[test]
fn d1358b_the_phase1_table_probes_evicts_and_drains_through_its_raw_pointer() {
    let l = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]);
    let limit = 64usize << 10;
    let mut buf = vec![0u64; limit / 8];
    let t = unsafe { T1::at(buf.as_mut_ptr() as *mut u8, &l, 2, 1 << 10, limit as u32) } as *mut T1;
    let cap = unsafe { (*t).cap } as u64;
    // every key 0..2·cap offered twice: probes open and find, collisions evict into the partition buffers
    let n = 2 * cap;
    for _ in 0..2 {
        for k in 0..n {
            let keys = [k];
            let h = KeyHash::of(&keys);
            let r = match unsafe { T1::probe(t, &keys, h) } {
                Some(r) => r,
                None => {
                    let home = unsafe { (*t).home(h) };
                    assert!(unsafe { T1::evict(t, home) }, "the pool has room");
                    unsafe { T1::open_at(t, home, &keys) }
                }
            };
            unsafe { Rec::at(r, &l) }.add_count(0, 1);
        }
    }
    let held = unsafe { (*t).held() };
    assert!(held >= n, "held {held} of {n} keys");
    assert!(unsafe { T1::drain(t) }, "the drain fits the pool");
    assert_eq!(unsafe { ((*t).len, (*t).held()) }, (0, held));
}

#[test]
fn d1358b_the_dense_front_writes_its_slots_through_its_own_borrow() {
    let mut buf = vec![0u64; (DenseFront::bytes(16) as usize).div_ceil(8)];
    let f = unsafe { DenseFront::at(buf.as_mut_ptr() as *mut u8, 0, 16) };
    f.set(3, 7);
    f.set(15, 0);
    assert_eq!((f.get(3), f.get(15), f.get(0)), (Some(7), Some(0), None));
    f.clear();
    assert_eq!((f.get(3), f.get(15)), (None, None));
}
