// MemoryContext::new_unobserved: an unobserved root never reaches the root
// observer (no weak entry in its creating thread's context forest, the
// non-atomic handle another thread's walk would race), and roots made on one
// thread, then used and dropped on another, stay sound at W8 with the
// observer installed, with the global pools (the default build) and with
// the per-thread pools live (--features std: an integration build compiles
// the lib without cfg(test), so the pooling paths run).
//
// Run: cargo test -p mcx --test unobserved_root [--features std]

use std::sync::mpsc;
use std::sync::Mutex;

use mcx::{box_new_in, MemoryContext, PgVec, RootWeak};

static SEEN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

fn observe(w: RootWeak) {
    if let Some(t) = w.tree_stats() {
        SEEN.lock().unwrap().push(t.name);
    }
}

fn seen(name: &str) -> usize {
    SEEN.lock().unwrap().iter().filter(|n| **n == name).count()
}

/// A root handed between threads (the caller guarantees one thread at a time).
struct Handed(MemoryContext);
// SAFETY: an unobserved root's state is reached only through this handle, by one thread at a time (the channel orders
// the handoff); its pools' entries are caches over Global.
unsafe impl Send for Handed {}

#[test]
fn unobserved_roots_skip_the_observer_and_move_between_threads_at_w8() {
    mcx::set_root_observer(observe);
    let observed = MemoryContext::new("unobs-test observed");
    let unobserved = MemoryContext::new_unobserved("unobs-test unobserved");
    let kid = unobserved.new_child("unobs-test kid");
    assert_eq!(seen("unobs-test observed"), 1, "a plain root reaches the observer");
    assert_eq!(seen("unobs-test unobserved"), 0, "an unobserved root never does");
    assert_eq!(seen("unobs-test kid"), 0, "a child is never a root");
    drop((kid, unobserved, observed));

    const W: usize = 8;
    const ROUNDS: u64 = 200;
    let (txs, rxs): (Vec<_>, Vec<_>) = (0..W).map(|_| mpsc::channel::<Handed>()).unzip();
    std::thread::scope(|s| {
        for (i, rx) in rxs.into_iter().enumerate() {
            let tx = txs[(i + 1) % W].clone();
            s.spawn(move || {
                for r in 0..ROUNDS {
                    let root = MemoryContext::new_unobserved("unobs-test w8");
                    let kid = root.new_child("unobs-test w8 kid");
                    let mut v: PgVec<u64> = PgVec::new_in(kid.mcx());
                    v.extend(0..64u64.wrapping_add(r));
                    assert_eq!(v[63], 63);
                    drop(v);
                    drop(kid);
                    let b = box_new_in(root.mcx(), [i as u8; 256]);
                    assert_eq!(b[255], i as u8);
                    drop(b);
                    tx.send(Handed(root)).unwrap();
                    let Handed(theirs) = rx.recv().unwrap();
                    let kid = theirs.new_child("unobs-test w8 kid");
                    let b = box_new_in(kid.mcx(), [r as u8; 512]);
                    assert_eq!(b[511], r as u8);
                    drop(b);
                    drop(kid);
                    drop(theirs);
                    let own = MemoryContext::new("unobs-test w8 observed");
                    drop(own);
                }
            });
        }
        drop(txs);
    });
    assert_eq!(seen("unobs-test w8"), 0, "no unobserved root reached the observer");
    assert_eq!(seen("unobs-test w8 observed"), W * ROUNDS as usize);
}

#[test]
fn an_unobserved_wholesale_root_resets_live_chunks() {
    let mut ctx = MemoryContext::new_unobserved_wholesale("unobs-test wholesale");
    core::mem::forget(mcx::vec_with_capacity_in::<u64>(ctx.mcx(), 2).unwrap());
    assert_eq!(ctx.used(), 16);
    ctx.reset();
    assert_eq!(ctx.used(), 0);
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "reset with 16 bytes still charged")]
fn an_unobserved_exact_root_still_reports_a_live_chunk() {
    let mut ctx = MemoryContext::new_unobserved("unobs-test exact");
    core::mem::forget(mcx::vec_with_capacity_in::<u64>(ctx.mcx(), 2).unwrap());
    ctx.reset();
}
