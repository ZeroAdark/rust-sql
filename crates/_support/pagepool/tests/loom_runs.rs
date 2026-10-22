//! The runs bit under loom: two classes take from each other's superpages while a third thread frees into both.
#![cfg(loom)]

use loom::sync::atomic::AtomicU32;
use loom::sync::Arc;
use pagepool::loom_api::{give_free, take_free, FreeRuns};
use std::sync::atomic::Ordering::*;

struct Bits(AtomicU32);

impl FreeRuns for Bits {
    fn get(&self) -> u32 {
        self.0.load(Relaxed)
    }
    fn clear(&self, mask: u32) {
        self.0.fetch_and(!mask, Relaxed);
    }
    fn add(&self, mask: u32) -> u32 {
        self.0.fetch_or(mask, Relaxed)
    }
}

struct Pool {
    class: [AtomicU32; 2],
    runs: AtomicU32,
    sp: [Bits; 2],
}

fn lock(l: &AtomicU32) {
    while l.compare_exchange_weak(0, 1, Acquire, Relaxed).is_err() {
        loom::thread::yield_now();
    }
}

fn unlock(l: &AtomicU32) {
    l.store(0, Release);
}

/// Class `c`'s take of one run: its own class bit, then the runs bit; the other class's superpage first.
fn take(p: &Pool, c: usize) -> Option<(usize, u32)> {
    lock(&p.class[c]);
    lock(&p.runs);
    let got = [1 - c, c].into_iter().find_map(|s| take_free(&p.sp[s], 1, 1).map(|m| (s, m)));
    unlock(&p.runs);
    unlock(&p.class[c]);
    got
}

fn free(p: &Pool, c: usize, s: usize, m: u32) {
    lock(&p.class[c]);
    lock(&p.runs);
    assert!(give_free(&p.sp[s], m), "a run freed twice");
    unlock(&p.runs);
    unlock(&p.class[c]);
}

#[test]
fn cross_class_takes_and_frees() {
    loom::model(|| {
        // superpage 0: run 0 free, run 1 held by the freer; superpage 1: run 0 free, run 1 held by the freer
        let p = Arc::new(Pool { class: [AtomicU32::new(0), AtomicU32::new(0)], runs: AtomicU32::new(0), sp: [Bits(AtomicU32::new(1)), Bits(AtomicU32::new(1))] });
        let ts: Vec<_> = (0..2)
            .map(|c| {
                let p = p.clone();
                loom::thread::spawn(move || take(&p, c))
            })
            .collect();
        let f = p.clone();
        let freer = loom::thread::spawn(move || {
            free(&f, 0, 1, 2);
            free(&f, 1, 0, 2);
        });
        let got: Vec<_> = ts.into_iter().map(|t| t.join().unwrap()).collect();
        freer.join().unwrap();
        let mut held = [0u32; 2];
        for (s, m) in got.into_iter().flatten() {
            assert_eq!(held[s] & m, 0, "a run handed out twice");
            held[s] |= m;
        }
        for s in 0..2 {
            assert_eq!(held[s] & p.sp[s].get(), 0, "a run both handed out and free");
            assert_eq!(held[s] | p.sp[s].get(), 3, "every run free or held once");
        }
    });
}
