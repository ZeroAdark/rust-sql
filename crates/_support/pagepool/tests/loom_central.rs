//! The central stack under loom: push / pop / take_all with 3 threads, ABA included (a pop's stale next is caught by
//! the tag). Runs only with RUSTFLAGS="--cfg loom".
#![cfg(loom)]

use loom::sync::atomic::{AtomicU32, AtomicU64};
use loom::sync::Arc;
use pagepool::loom_api::{Atom64, Nodes, Stack, NONE};
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

struct Arr(Vec<AtomicU32>);

impl Nodes for Arr {
    fn next_batch(&self, i: u32) -> u32 {
        self.0[i as usize].load(Ordering::Relaxed)
    }
    fn set_next_batch(&self, i: u32, v: u32) {
        self.0[i as usize].store(v, Ordering::Relaxed)
    }
}

fn drain(s: &Stack<L>, n: &Arr) -> Vec<u32> {
    let mut v = Vec::new();
    while let Some(i) = s.pop(n) {
        v.push(i);
        assert!(v.len() <= 3, "a node twice: {v:?}");
    }
    v.sort();
    v
}

#[test]
fn push_pop_aba() {
    loom::model(|| {
        let n = Arc::new(Arr((0..3).map(|_| AtomicU32::new(NONE)).collect()));
        let s = Arc::new(Stack::with(L(AtomicU64::new(NONE as u64))));
        for i in 0..3 {
            s.push(&*n, i);
        }
        let (s1, n1) = (s.clone(), n.clone());
        let a = loom::thread::spawn(move || {
            if let Some(i) = s1.pop(&*n1) {
                s1.push(&*n1, i);
            }
        });
        let (s2, n2) = (s.clone(), n.clone());
        // pop 2 then 1, push 2 back, then 1: a's pop that read (2, next 1) before must not set the head to 1 (held here)
        let b = loom::thread::spawn(move || {
            let x = s2.pop(&*n2);
            let y = s2.pop(&*n2);
            for i in [x, y].into_iter().flatten() {
                s2.push(&*n2, i);
            }
        });
        a.join().unwrap();
        b.join().unwrap();
        assert_eq!(drain(&s, &n), vec![0, 1, 2]);
    });
}

#[test]
fn take_all_races_pop() {
    loom::model(|| {
        let n = Arc::new(Arr((0..3).map(|_| AtomicU32::new(NONE)).collect()));
        let s = Arc::new(Stack::with(L(AtomicU64::new(NONE as u64))));
        for i in 0..3 {
            s.push(&*n, i);
        }
        let (s1, n1) = (s.clone(), n.clone());
        let a = loom::thread::spawn(move || s1.pop(&*n1));
        let (s2, n2) = (s.clone(), n.clone());
        let b = loom::thread::spawn(move || {
            let mut got = Vec::new();
            let mut c = s2.take_all().unwrap_or(NONE);
            while c != NONE {
                got.push(c);
                c = n2.next_batch(c);
            }
            got
        });
        let x = a.join().unwrap();
        let mut all = b.join().unwrap();
        all.extend(x);
        all.extend(drain(&s, &n));
        all.sort();
        assert_eq!(all, vec![0, 1, 2], "every node exactly once");
    });
}

/// Three threads each pop a batch and push it back (an ABA chance at every pop: the top it read may be popped, others
/// moved and it pushed again before its CAS); every node stays on the stack exactly once.
#[test]
fn three_threads_pop_and_push_back() {
    loom::model(|| {
        let n = Arc::new(Arr((0..3).map(|_| AtomicU32::new(NONE)).collect()));
        let s = Arc::new(Stack::with(L(AtomicU64::new(NONE as u64))));
        for i in 0..3 {
            s.push(&*n, i);
        }
        let ts: Vec<_> = (0..3)
            .map(|_| {
                let (s, n) = (s.clone(), n.clone());
                loom::thread::spawn(move || {
                    if let Some(i) = s.pop(&*n) {
                        s.push(&*n, i);
                    }
                })
            })
            .collect();
        for t in ts {
            t.join().unwrap();
        }
        assert_eq!(drain(&s, &n), vec![0, 1, 2]);
    });
}
