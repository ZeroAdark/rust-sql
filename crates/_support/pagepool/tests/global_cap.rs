//! PoolAlloc's claims pass the pool's claim cap (a global allocation has no refusal path); other takes keep it.
#![cfg(all(feature = "global", not(target_family = "wasm")))]

use pagepool::{Class, Hint, PoolAlloc, Tag};

#[global_allocator]
static A: PoolAlloc = PoolAlloc;

#[test]
fn a_global_allocation_is_never_refused_by_the_claim_cap() {
    drop(vec![1u8; 1 << 20]);
    pagepool::return_warm_now(usize::MAX);
    let mut held = Vec::with_capacity(4096);
    let cap = pagepool::stats().claimed + (2 << 20);
    pagepool::set_cap(cap);
    let big: Vec<Vec<u8>> = (0..96).map(|i| vec![i as u8; 1 << 20]).collect();
    let small: Vec<Box<[u8; 100]>> = (0..200_000).map(|_| Box::new([7u8; 100])).collect();
    let s = pagepool::stats();
    assert!(s.claimed > cap, "the global path claimed past the cap: {s:?}");
    while let Some(b) = pagepool::take(2 << 20, 64, Hint::new(Class::Task, Tag::OTHER)) {
        held.push(b);
        assert!(held.len() < 4096);
    }
    let t = pagepool::stats();
    assert!(t.claimed <= s.claimed, "a take outside PoolAlloc claims nothing past the cap ({} warm ones): {t:?}", held.len());
    assert_eq!(big.iter().map(|v| v[0] as usize).sum::<usize>(), (0..96).sum::<usize>());
    assert!(small.iter().all(|b| b[99] == 7));
    pagepool::set_cap(0);
}
