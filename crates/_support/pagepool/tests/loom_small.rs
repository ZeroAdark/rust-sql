//! The small tier's remote-free list under loom: two threads push blocks (one a pre-linked chain, as a thread's pending
//! cache flushes) while the owner takes the list; every block is taken exactly once, the counts add up, and the links
//! the pushers wrote are visible to the taker (loom's cells would report a race). Runs only with RUSTFLAGS="--cfg loom" and --features global.
#![cfg(all(loom, feature = "global"))]

use loom::cell::UnsafeCell;
use loom::sync::atomic::AtomicU64;
use loom::sync::Arc;
use pagepool::loom_api::{remote_push, remote_push_chain, remote_take, Atom64, Links};
use std::sync::atomic::Ordering;

struct L(AtomicU64);

impl Atom64 for L {
    fn load(&self, o: Ordering) -> u64 {
        self.0.load(o)
    }
    fn cas(&self, cur: u64, new: u64, ok: Ordering, err: Ordering) -> Result<u64, u64> {
        self.0.compare_exchange(cur, new, ok, err)
    }
}

/// Blocks at offsets 8, 16, 24, 32 (offset / 8 indexes the cell).
struct Blocks(Vec<UnsafeCell<u32>>);

// SAFETY: each cell is written by the one thread freeing that block, and read by the taker after its Acquire.
unsafe impl Sync for Blocks {}

impl Links for Blocks {
    fn set_next(&self, off: u32, next: u32) {
        self.0[off as usize / 8].with_mut(|p| unsafe { *p = next });
    }
}

impl Blocks {
    fn next(&self, off: u32) -> u32 {
        self.0[off as usize / 8].with(|p| unsafe { *p })
    }
}

fn walk(b: &Blocks, head: u32, n: u32, seen: &mut Vec<u32>) {
    let mut c = head;
    for _ in 0..n {
        assert!(c != 0, "list shorter than its count");
        seen.push(c);
        c = b.next(c);
    }
    assert_eq!(c, 0, "list longer than its count");
}

#[test]
fn pushes_race_the_owners_take() {
    loom::model(|| {
        let b = Arc::new(Blocks((0..5).map(|_| UnsafeCell::new(u32::MAX)).collect()));
        let w = Arc::new(L(AtomicU64::new(0)));
        let (b1, w1) = (b.clone(), w.clone());
        let t1 = loom::thread::spawn(move || {
            remote_push(&*w1, &*b1, 8);
            remote_push(&*w1, &*b1, 16);
        });
        let (b2, w2) = (b.clone(), w.clone());
        let t2 = loom::thread::spawn(move || {
            remote_push(&*w2, &*b2, 24);
        });
        let mut seen = Vec::new();
        let (h, n) = remote_take(&*w);
        walk(&b, h, n, &mut seen);
        t1.join().unwrap();
        t2.join().unwrap();
        let (h, n) = remote_take(&*w);
        walk(&b, h, n, &mut seen);
        seen.sort();
        assert_eq!(seen, vec![8, 16, 24], "every block taken exactly once");
    });
}

#[test]
fn a_chain_races_a_push_and_the_take() {
    loom::model(|| {
        let b = Arc::new(Blocks((0..5).map(|_| UnsafeCell::new(u32::MAX)).collect()));
        let w = Arc::new(L(AtomicU64::new(0)));
        let (b1, w1) = (b.clone(), w.clone());
        let t1 = loom::thread::spawn(move || {
            b1.set_next(16, 8);
            b1.set_next(32, 16);
            remote_push_chain(&*w1, &*b1, 32, 8, 3);
        });
        let (b2, w2) = (b.clone(), w.clone());
        let t2 = loom::thread::spawn(move || {
            remote_push(&*w2, &*b2, 24);
        });
        let mut seen = Vec::new();
        let (h, n) = remote_take(&*w);
        walk(&b, h, n, &mut seen);
        t1.join().unwrap();
        t2.join().unwrap();
        let (h, n) = remote_take(&*w);
        walk(&b, h, n, &mut seen);
        seen.sort();
        assert_eq!(seen, vec![8, 16, 24, 32], "every block taken exactly once");
    });
}
