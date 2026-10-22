// D3639 (D), main 2026-10-10T12:44:30Z: aset.c's context_freelists for the small size class (ALLOCSET_SMALL_SIZES, a 1 kB
// first block) beside the default one. A dropped small context parks its first block on the small list; the next small
// context starts with it, empty, as aset.c reuses a freelist context as if new. The list holds MAX_FREE_CONTEXTS (100);
// a drop onto a full list discards the whole list first, then parks (aset.c AllocSetDelete). A default context never
// takes a small block, nor a small context a default one.
//
// An integration build compiles the lib WITHOUT cfg(test), so the keeper lists are live here (the in-crate unit tests
// bypass them). One #[test] in this binary, run on its own thread: with `std` the lists are per thread, without it they
// are per process, and either way nothing else touches them.
//
// Run: cargo test -p mcx --test small_keeper_freelist [--features std]

use mcx::{box_new_in, MemoryContext, ALLOCSET_SMALL_INITSIZE};

const MAX_FREE_CONTEXTS: usize = 100; // aset.c

fn used_small(root: &MemoryContext) -> MemoryContext {
    let c = root.new_child_small("small");
    let b = box_new_in(c.mcx(), [7u8; 16]);
    assert_eq!(b[15], 7);
    drop(b);
    c
}

#[test]
fn small_contexts_reuse_their_first_block_like_aset_c() {
    std::thread::spawn(|| {
        let root = MemoryContext::new("small-keeper-root");

        let c = root.new_child_small("fresh");
        assert_eq!(c.mem_allocated(), 0);
        let b = box_new_in(c.mcx(), [1u8; 16]);
        assert_eq!(c.mem_allocated(), ALLOCSET_SMALL_INITSIZE);
        drop(b);
        drop(c);

        let d = root.new_child_small("reused");
        assert_eq!(d.mem_allocated(), ALLOCSET_SMALL_INITSIZE);
        assert_eq!(d.stats().used, 0);
        let b = box_new_in(d.mcx(), [2u8; 64]);
        assert_eq!(b[63], 2);
        assert_eq!(d.mem_allocated(), ALLOCSET_SMALL_INITSIZE);
        drop(b);
        drop(d);

        let e = root.new_child("default");
        assert_ne!(e.mem_allocated(), ALLOCSET_SMALL_INITSIZE);
        let f = root.new_child_small("still-parked");
        assert_eq!(f.mem_allocated(), ALLOCSET_SMALL_INITSIZE);
        drop(f);
        drop(e);

        let g = root.new_child_small("drain");
        assert_eq!(g.mem_allocated(), ALLOCSET_SMALL_INITSIZE);
        let h = root.new_child_small("empty");
        assert_eq!(h.mem_allocated(), 0);
        let live: Vec<MemoryContext> = (0..150).map(|_| used_small(&root)).collect();
        drop(live);
        let again: Vec<MemoryContext> = (0..150).map(|_| root.new_child_small("again")).collect();
        let reused = again.iter().filter(|c| c.mem_allocated() == ALLOCSET_SMALL_INITSIZE).count();
        assert_eq!(reused, 150 - MAX_FREE_CONTEXTS);
        assert!(again.iter().all(|c| c.stats().used == 0));
        drop(again);
        drop(h);
        drop(g);
        drop(root);
    })
    .join()
    .unwrap();
}
