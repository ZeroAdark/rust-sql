//! Wave 2 R1: `ChargedVec` charges its block's capacity to its owner as it grows and gives it back with the data, also
//! when one thread fills it and another frees it. Its own binary: the global allocator here is door 1 (every allocation
//! and free counted on the calling thread's account, `door_heap`), as the server's allocator is.

use std::alloc::{GlobalAlloc, Layout, System};

use memledger::charge::{context, enter_as, owner_row};
use memledger::{account_row, bind_backend, door_heap, fold, statement_begin, statement_end, unbind_backend, Acct, ChargedVec, Kind, Owner};

struct Door1;

// SAFETY: forwards to the system allocator; `door_heap` is a const thread-local add (no allocation, no destructor).
unsafe impl GlobalAlloc for Door1 {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller's layout, forwarded.
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            door_heap(l.size() as isize);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: `p` came from `alloc` with `l`.
        unsafe { System.dealloc(p, l) };
        door_heap(-(l.size() as isize));
    }
}

#[global_allocator]
static A: Door1 = Door1;

/// What the runtime around a spawn / join moves between two threads' accounts (a thread handle allocated on one and freed
/// on the other): far below the blocks these tests charge.
const SLACK: i64 = 4096;

/// Each test on its own thread and backend slot (3960..3964: no other test's), its statement running.
fn on(procno: u32, f: impl FnOnce(Acct, Owner) + Send + 'static) {
    std::thread::spawn(move || {
        bind_backend(procno, 8100 + procno);
        statement_begin();
        let (o, _) = context();
        f(Acct::backend(procno), o);
        statement_end();
        unbind_backend();
    })
    .join()
    .unwrap();
}

fn account(a: Acct) -> i64 {
    fold();
    account_row(a).account_bytes
}

fn committed(o: Owner) -> i64 {
    owner_row(o).committed
}

#[test]
fn the_charge_follows_the_capacity_and_leaves_with_the_data() {
    on(3960, |_, o| {
        let mut v = ChargedVec::<u64>::new(Some(o), Kind::Region);
        assert_eq!((v.owner(), committed(o)), (Some(o), 0));
        for i in 0..1000 {
            v.push(i).unwrap();
            assert_eq!(committed(o), 8 * v.capacity() as i64, "push {i}");
        }
        v.extend_from_slice(&[7; 5000]).unwrap();
        assert_eq!(committed(o), 8 * v.capacity() as i64);
        v.resize(100_000, 3).unwrap();
        assert_eq!((v.len(), v[99_999], committed(o)), (100_000, 3, 8 * v.capacity() as i64));
        v.resize(10, 0).unwrap();
        let cap = v.capacity();
        v.clear();
        assert_eq!((v.len(), v.capacity(), committed(o)), (0, cap, 8 * cap as i64), "clear keeps the block and its charge");
        v.with_vec(|w| w.reserve_exact(1 << 20));
        assert_eq!(committed(o), 8 * v.capacity() as i64);
        v.with_vec(|w| w.shrink_to_fit());
        assert_eq!((v.capacity(), committed(o)), (0, 0), "a shrink frees its share");
        v.extend_from_slice(&[1, 2, 3]).unwrap();
        v.as_mut_slice()[1] = 9;
        assert_eq!(v.as_slice(), [1, 9, 3]);
        assert_eq!(v.charged_bytes() as i64, committed(o));
        drop(v);
        assert_eq!(committed(o), 0);
    });
}

/// A worker serving its leader (on the leader's account, as a claim is: hosts worker_arm's `enter_as`) fills the vec; the
/// leader frees it. The owner holds the block while it lives and nothing after; the account is back where it was.
#[test]
fn filled_on_a_worker_freed_on_the_leader() {
    on(3961, |a, o| {
        let before = account(a);
        let v = std::thread::spawn(move || {
            let _serving = enter_as(o);
            let mut v = ChargedVec::<u8>::new(Some(o), Kind::Region);
            for _ in 0..64 {
                v.extend_from_slice(&[1; 4096]).unwrap();
            }
            v
        })
        .join()
        .unwrap();
        let cap = v.capacity() as i64;
        assert_eq!(committed(o), cap);
        let during = account(a) - before;
        assert!((during - cap).abs() < SLACK, "the block is on the leader's account: {during} for {cap}");
        drop(v);
        assert_eq!(committed(o), 0);
        let after = account(a) - before;
        assert!(after.abs() < SLACK, "the account is back: {after}");
    });
}

/// Filled on a thread of ANOTHER account (thread A: its own backend), freed on the owner's (thread B): the commit moves
/// the block from A's account to the owner's, the drop leaves it on B's where the free lands. No row ends below where it
/// started, and the owner ends at 0.
#[test]
fn filled_on_thread_a_freed_on_thread_b() {
    on(3962, |b, o| {
        let b0 = account(b);
        let v = std::thread::spawn(move || {
            bind_backend(3963, 8163);
            let a = Acct::backend(3963);
            let a0 = account(a);
            let mut v = ChargedVec::<u64>::new(Some(o), Kind::Region);
            v.resize(1 << 17, 5).unwrap();
            let moved = account(a) - a0;
            assert!(moved.abs() < SLACK, "thread A's account keeps nothing of the block: {moved}");
            unbind_backend();
            v
        })
        .join()
        .unwrap();
        let cap = 8 * v.capacity() as i64;
        assert_eq!(committed(o), cap);
        assert!((account(b) - b0 - cap).abs() < SLACK, "the block is on the owner's account");
        drop(v);
        assert_eq!(committed(o), 0);
        let after = account(b) - b0;
        assert!(after.abs() < SLACK, "thread B's account is back, not below: {after}");
    });
}

#[test]
fn no_owner_is_a_plain_vec() {
    on(3964, |a, o| {
        let before = account(a);
        let mut v = ChargedVec::<u32>::new(None, Kind::Region);
        v.extend_from_slice(&[1; 50_000]).unwrap();
        assert_eq!((v.owner(), v.charged_bytes(), committed(o)), (None, 0, 0));
        let during = account(a) - before;
        assert!(during >= 4 * 50_000, "the block stays on the allocating thread's account: {during}");
        let d: ChargedVec<u8> = ChargedVec::default();
        assert_eq!((d.owner(), d.len(), d.capacity()), (None, 0, 0));
    });
}

#[test]
fn a_refused_reserve_changes_nothing() {
    on(3960 + 5, |_, o| {
        let mut v = ChargedVec::<u64>::new(Some(o), Kind::Region);
        v.push(1).unwrap();
        let held = committed(o);
        assert!(v.try_reserve(usize::MAX).is_err());
        assert!(v.extend_from_slice(&[0; 3]).is_ok());
        assert_eq!((v.len(), committed(o)), (4, 8 * v.capacity() as i64));
        assert!(held > 0);
    });
}

#[test]
fn the_value_floor_is_the_doors_fold_step() {
    assert_eq!(memledger::VALUE_FLOOR as i64, memledger::DOOR_FOLD_BYTES);
    assert_eq!(memledger::VALUE_FLOOR, 256 << 10);
}

#[test]
fn it_is_send_and_sync_as_a_vec_is() {
    fn shared<X: Send + Sync>() {}
    shared::<ChargedVec<u64>>();
    shared::<ChargedVec<(u64, u32)>>();
}

#[test]
fn the_guard_grows_in_place_and_syncs_at_sync_and_drop() {
    on(3966, |b, o| {
        let b0 = account(b);
        let mut v = std::thread::spawn(move || {
            // Grown through the guard on thread A (its own backend) past the door's fold step.
            bind_backend(3967, 8167);
            let a = Acct::backend(3967);
            let a0 = account(a);
            let mut v = ChargedVec::<u8>::new(Some(o), Kind::Region);
            {
                let mut g = v.vec_mut();
                assert_eq!(g.owner(), Some(o));
                g.extend_from_slice(&vec![1; 8 * memledger::VALUE_FLOOR]);
                assert_eq!(committed(o), 0, "growth through the guard is uncharged until a sync");
                g.sync();
                assert_eq!(committed(o), g.capacity() as i64, "sync charges the capacity");
                let w: &mut Vec<u8> = &mut g;
                w.reserve_exact(1 << 20);
            }
            assert_eq!(committed(o), v.capacity() as i64, "the guard's drop syncs (capacity, not length)");
            assert!(v.capacity() > v.len());
            let moved = account(a) - a0;
            assert!(moved.abs() < SLACK, "thread A's account keeps nothing of the block: {moved}");
            unbind_backend();
            v
        })
        .join()
        .unwrap();
        let cap = v.capacity() as i64;
        assert_eq!(committed(o), cap);
        assert!((account(b) - b0 - cap).abs() < SLACK, "the block is on the owner's account");
        v.vec_mut().shrink_to_fit();
        assert_eq!(committed(o), v.capacity() as i64, "a shrink frees its share at the guard's drop");
        drop(v);
        assert_eq!(committed(o), 0);
        let after = account(b) - b0;
        assert!(after.abs() < SLACK, "thread B's account nets to 0: {after}");
    });
}
