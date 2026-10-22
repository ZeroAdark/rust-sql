//! The small tier's abandoned lists under loom: a remover (an adopter's pop, a sweep's take_all) races another that
//! gives back a run it took (a system run's memory goes back to `System`). A run's memory is a loom cell, its give-back
//! a write, a pop's read of its `alink` a read, so loom names any read of a run freed meanwhile. Runs only with
//! RUSTFLAGS="--cfg loom" and --features global.
#![cfg(all(loom, feature = "global"))]

use loom::cell::UnsafeCell;
use loom::sync::atomic::{AtomicU32, AtomicU64};
use loom::sync::Arc;
use pagepool::loom_api::{ab_pop, ab_take_all, Atom64, Nodes, Spin, Stack, NONE};
use std::sync::atomic::Ordering::{self, *};

struct L(AtomicU64);

impl Atom64 for L {
    fn load(&self, o: Ordering) -> u64 {
        self.0.load(o)
    }
    fn cas(&self, cur: u64, new: u64, ok: Ordering, err: Ordering) -> Result<u64, u64> {
        self.0.compare_exchange(cur, new, ok, err)
    }
}

struct Lock(AtomicU32);

impl Spin for Lock {
    fn lock(&self) {
        while self.0.compare_exchange_weak(0, 1, Acquire, Relaxed).is_err() {
            loom::thread::yield_now();
        }
    }
    fn unlock(&self) {
        self.0.store(0, Release);
    }
}

/// Two runs: each its header's `alink` and the rest of its memory (0 live, 1 given back).
struct Runs {
    link: [AtomicU32; 2],
    mem: [UnsafeCell<u32>; 2],
}

// SAFETY: `mem` is reached only through loom's cell, which checks every access pair.
unsafe impl Sync for Runs {}

impl Nodes for Runs {
    fn next_batch(&self, i: u32) -> u32 {
        // the link lies in the run's memory: reading it reads the run
        // SAFETY: loom's cell; a concurrent give-back is reported, not raced.
        let gone = self.mem[i as usize].with(|p| unsafe { *p });
        assert_eq!(gone, 0, "a remover read the alink of run {i} after it was given back");
        self.link[i as usize].load(Relaxed)
    }
    fn set_next_batch(&self, i: u32, v: u32) {
        self.link[i as usize].store(v, Relaxed)
    }
}

impl Runs {
    fn give_back(&self, i: u32) {
        // SAFETY: loom's cell.
        self.mem[i as usize].with_mut(|p| unsafe { *p = 1 });
    }
    fn gone(&self, i: u32) -> bool {
        // SAFETY: loom's cell, after every thread joined.
        self.mem[i as usize].with(|p| unsafe { *p }) != 0
    }
}

struct Ab {
    stack: Stack<L>,
    lock: Lock,
    runs: Runs,
}

fn two_runs() -> Arc<Ab> {
    let ab = Arc::new(Ab {
        stack: Stack::with(L(AtomicU64::new(NONE as u64))),
        lock: Lock(AtomicU32::new(0)),
        runs: Runs { link: [AtomicU32::new(NONE), AtomicU32::new(NONE)], mem: [UnsafeCell::new(0), UnsafeCell::new(0)] },
    });
    ab.stack.push(&ab.runs, 0);
    ab.stack.push(&ab.runs, 1);
    ab
}

/// Every run is given back once or still listed, never both.
fn settle(ab: &Ab) {
    let mut listed = [false; 2];
    let mut c = ab.stack.take_all().unwrap_or(NONE);
    while c != NONE {
        assert!(!listed[c as usize], "run {c} listed twice");
        listed[c as usize] = true;
        c = ab.runs.link[c as usize].load(Relaxed);
    }
    for i in 0..2 {
        assert!(listed[i as usize] != ab.runs.gone(i), "run {i}: listed {} given back {}", listed[i as usize], ab.runs.gone(i));
    }
}

/// `find_cache`'s look (pop, no free block, push back) against `sweep_abandoned` (take all, give back the empty runs).
#[test]
fn a_pop_races_a_sweep() {
    loom::model(|| {
        let ab = two_runs();
        let a = ab.clone();
        let adopter = loom::thread::spawn(move || {
            if let Some(i) = ab_pop(&a.stack, &a.lock, &a.runs) {
                a.stack.push(&a.runs, i);
            }
        });
        let s = ab.clone();
        let sweeper = loom::thread::spawn(move || {
            let mut c = ab_take_all(&s.stack, &s.lock).unwrap_or(NONE);
            while c != NONE {
                let next = s.runs.link[c as usize].load(Relaxed);
                s.runs.give_back(c);
                c = next;
            }
        });
        adopter.join().unwrap();
        sweeper.join().unwrap();
        settle(&ab);
    });
}

/// Two adopters: one keeps the run it popped and gives it back once its blocks are home (`give_run`).
#[test]
fn a_pop_races_a_pop_that_gives_back() {
    loom::model(|| {
        let ab = two_runs();
        let a = ab.clone();
        let first = loom::thread::spawn(move || {
            if let Some(i) = ab_pop(&a.stack, &a.lock, &a.runs) {
                a.runs.give_back(i);
            }
        });
        let b = ab.clone();
        let second = loom::thread::spawn(move || {
            if let Some(i) = ab_pop(&b.stack, &b.lock, &b.runs) {
                b.stack.push(&b.runs, i);
            }
        });
        first.join().unwrap();
        second.join().unwrap();
        settle(&ab);
    });
}
