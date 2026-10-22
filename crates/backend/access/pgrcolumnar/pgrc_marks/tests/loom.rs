//! The property every model checks, at the end AND inside each writer right
//! after its `written` returns (its own backend's next scan): an image
//! published with seq `s0` is either current (`seq() == s0`) and holds the
//! write, or `seq()` has moved past `s0` (no stale-and-clean outcome).

#![cfg(loom)]

use std::sync::Arc;

use loom::thread;
use pgrc_marks::{BlockMarks, GateOutcome, Locator, Registry, WriterCache};
use pgsync::atomic::{AtomicU32, AtomicUsize, Ordering::SeqCst};
use pgsync::{Mutex, RwLock};

const LOC: Locator = (1663, 5, 16384);
const NPAGES: usize = 3;

struct World {
    reg: Registry,
    pages: Vec<RwLock<u32>>,
    nblocks: AtomicU32,
    image: Mutex<Option<(u64, u32, Vec<u32>)>>, // (seq, nblocks, copies)
}

impl World {
    fn new(nblocks: u32) -> Arc<World> {
        Arc::new(World {
            reg: Registry::with_chunk_words(1),
            pages: (0..NPAGES).map(|_| RwLock::new(0)).collect(),
            nblocks: AtomicU32::new(nblocks),
            image: Mutex::new(None),
        })
    }

    fn write(&self, cache: &mut WriterCache, block: u32, version: u32) {
        *self.pages[block as usize].write().unwrap() = version;
        self.reg.written(cache, LOC, block);
    }

    fn check_own(&self, block: u32, version: u32) {
        let m = match self.reg.lookup(LOC) {
            Some(m) => m,
            None => return,
        };
        let img = self.image.lock().unwrap();
        if let Some((s, n, copies)) = img.as_ref() {
            if m.seq() == *s {
                assert!(block < *n, "current image does not cover block {block}");
                assert!(
                    copies[block as usize] >= version,
                    "stale-and-clean: block {block} copy {} < {version}",
                    copies[block as usize]
                );
            }
        }
    }

    fn refresh(&self, m: &BlockMarks, window: u32) -> u64 {
        let (prev_n, mut copies) = match self.image.lock().unwrap().as_ref() {
            Some((_, n, c)) => (*n, c.clone()),
            None => (0, vec![0; NPAGES]),
        };
        let s0 = m.begin_refresh(prev_n, window);
        let n = self.nblocks.load(SeqCst);
        let mut todo = Vec::new();
        m.dirty(prev_n.min(n), &mut todo);
        todo.extend(prev_n..n);
        for b in todo {
            m.arm(b);
            copies[b as usize] = *self.pages[b as usize].read().unwrap();
        }
        *self.image.lock().unwrap() = Some((s0, n, copies));
        s0
    }

    fn check_final(&self, m: &BlockMarks, want: &[(u32, u32)]) {
        let img = self.image.lock().unwrap();
        let (s, n, copies) = img.as_ref().expect("an image was published");
        if m.seq() == *s {
            for &(b, v) in want {
                assert!(b < *n && copies[b as usize] >= v, "stale-and-clean at end: block {b}");
            }
        }
    }
}

fn builder(bound: usize) -> loom::model::Builder {
    let mut b = loom::model::Builder::new();
    if std::env::var_os("LOOM_MAX_PREEMPTIONS").is_none() {
        b.preemption_bound = Some(bound);
    }
    b
}

#[test]
fn writer_vs_refresh() {
    builder(3).check(|| {
        let w = World::new(1);
        let m = w.reg.register(LOC);
        w.refresh(&m, 0);
        let w2 = Arc::clone(&w);
        let t = thread::spawn(move || {
            let mut c = WriterCache::new();
            w2.write(&mut c, 0, 1);
            w2.check_own(0, 1);
        });
        w.refresh(&m, 0);
        t.join().unwrap();
        w.check_final(&m, &[(0, 1)]);
    });
}

/// A writer that skips its mark MUST be caught.
#[test]
fn seeded_missing_mark_is_caught() {
    let caught = std::panic::catch_unwind(|| {
        loom::model(|| {
            let w = World::new(1);
            let m = w.reg.register(LOC);
            w.refresh(&m, 0);
            let w2 = Arc::clone(&w);
            let t = thread::spawn(move || {
                *w2.pages[0].write().unwrap() = 1; // no `written`
            });
            w.refresh(&m, 0);
            t.join().unwrap();
            w.check_final(&m, &[(0, 1)]);
        });
    });
    assert!(caught.is_err(), "the model must catch a missing mark");
}

#[test]
fn extension_vs_refresh() {
    for window in [0u32, 1, 2] {
        builder(3).check(move || {
            let w = World::new(1);
            let m = w.reg.register(LOC);
            w.refresh(&m, window);
            let w2 = Arc::clone(&w);
            let t = thread::spawn(move || {
                let mut c = WriterCache::new();
                w2.nblocks.store(2, SeqCst); // extend
                w2.write(&mut c, 1, 1);
                w2.check_own(1, 1);
            });
            w.refresh(&m, window);
            t.join().unwrap();
            w.check_final(&m, &[(1, 1)]);
        });
    }
}

#[test]
fn stale_handle_vs_register() {
    builder(3).check(|| {
        let w = World::new(1);
        let mut c = WriterCache::new();
        w.reg.written(&mut c, LOC, 0);
        let w2 = Arc::clone(&w);
        let t = thread::spawn(move || {
            let mut c = c;
            w2.write(&mut c, 0, 1);
            w2.check_own(0, 1);
        });
        let m = w.reg.register(LOC);
        w.refresh(&m, 1);
        t.join().unwrap();
        w.check_final(&m, &[(0, 1)]);
    });
}

#[test]
fn two_refreshes_one_writer_gate() {
    builder(2).check(|| {
        let w = World::new(1);
        let m = w.reg.register(LOC);
        w.refresh(&m, 0);
        let owners = Arc::new(AtomicUsize::new(0));
        let refresher = |w: Arc<World>, m: Arc<BlockMarks>, owners: Arc<AtomicUsize>| {
            move || {
                for _ in 0..2 {
                    let s = m.seq();
                    let current = w.image.lock().unwrap().as_ref().is_some_and(|i| i.0 == s);
                    if current {
                        return;
                    }
                    match w.reg.gate(LOC) {
                        GateOutcome::Owner(g) => {
                            owners.fetch_add(1, SeqCst);
                            w.refresh(&m, 0);
                            drop(g);
                            return;
                        }
                        GateOutcome::Built => {}
                        GateOutcome::TimedOut => panic!("no timeout without a lock cycle"),
                    }
                }
            }
        };
        let w2 = Arc::clone(&w);
        let tw = thread::spawn(move || {
            let mut c = WriterCache::new();
            w2.write(&mut c, 0, 1);
            w2.check_own(0, 1);
        });
        let t1 = thread::spawn(refresher(Arc::clone(&w), Arc::clone(&m), Arc::clone(&owners)));
        refresher(Arc::clone(&w), Arc::clone(&m), Arc::clone(&owners))();
        t1.join().unwrap();
        tw.join().unwrap();
        if m.seq() != w.image.lock().unwrap().as_ref().unwrap().0 {
            w.refresh(&m, 0);
        }
        w.check_final(&m, &[(0, 1)]);
    });
}

#[test]
fn gate_single_owner() {
    loom::model(|| {
        let reg = Arc::new(Registry::with_chunk_words(1));
        let owners = Arc::new(AtomicUsize::new(0));
        let live = Arc::new(AtomicUsize::new(0));
        let f = |reg: Arc<Registry>, owners: Arc<AtomicUsize>, live: Arc<AtomicUsize>| {
            move || match reg.gate(LOC) {
                GateOutcome::Owner(g) => {
                    owners.fetch_add(1, SeqCst);
                    assert_eq!(live.fetch_add(1, SeqCst), 0, "two live owners");
                    live.fetch_sub(1, SeqCst);
                    drop(g);
                }
                GateOutcome::Built => {}
                GateOutcome::TimedOut => panic!("unexpected timeout"),
            }
        };
        let t = thread::spawn(f(Arc::clone(&reg), Arc::clone(&owners), Arc::clone(&live)));
        f(Arc::clone(&reg), Arc::clone(&owners), Arc::clone(&live))();
        t.join().unwrap();
        assert!(owners.load(SeqCst) >= 1);
    });
}

#[test]
fn overflow_flag() {
    builder(3).check(|| {
        let w = World::new(1);
        let m = w.reg.register(LOC);
        w.refresh(&m, 0);
        w.nblocks.store(3, SeqCst);
        let w1 = Arc::clone(&w);
        let t1 = thread::spawn(move || {
            let mut c = WriterCache::new();
            w1.write(&mut c, 1, 1);
            w1.check_own(1, 1);
        });
        let w2 = Arc::clone(&w);
        let t2 = thread::spawn(move || {
            let mut c = WriterCache::new();
            w2.write(&mut c, 2, 1);
            w2.check_own(2, 1);
        });
        w.refresh(&m, 0);
        t1.join().unwrap();
        t2.join().unwrap();
        w.check_final(&m, &[(1, 1), (2, 1)]);
    });
}
