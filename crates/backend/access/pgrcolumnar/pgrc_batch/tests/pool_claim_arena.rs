//! pagepool P3 (`PGRUST_POOL_GRAVITY=1`): a claim arena's chunks come from the page pool, its laws unchanged (8-aligned,
//! values unmoved until a reset, chunks kept across resets), their bytes on the thread's account among its pool bytes
//! (as the allocator's would be on it), all off at the arena's drop.
#![cfg(target_os = "linux")]

use memledger::{account_row, Acct};
use pgrc_batch::arena::ClaimArena;

fn pool_bytes(a: Acct) -> i64 {
    memledger::fold();
    account_row(a).pagepool_bytes
}

#[test]
fn a_claim_arenas_chunks_are_the_pools_on_the_threads_account() {
    std::env::set_var("PGRUST_POOL_GRAVITY", "1");
    assert!(pagepool::switches().gravity, "the switch, read at its first call");
    assert!(pagepool::init(&pagepool::Config::from_policy()), "the pool reserves its zones");
    std::thread::spawn(|| {
        memledger::bind_backend(4801, 9000 + 4801);
        let a = Acct::backend(4801);
        let q0 = pool_bytes(a);
        let mut arena = ClaimArena::new();
        let p = arena.alloc_copy(&[1, 2, 3]);
        assert!(pagepool::owns(p.as_ptr()), "the pool's chunk");
        assert_eq!(p.as_ptr() as usize % 8, 0, "the alignment law");
        let big = arena.alloc(3 << 20);
        assert!(pagepool::owns(big.as_ptr()));
        assert_eq!(unsafe { core::slice::from_raw_parts(p.as_ptr(), 3) }, &[1, 2, 3], "values unmoved by growth");
        let held = pool_bytes(a) - q0;
        assert!(held >= arena.retained_capacity() as i64, "on the thread's account: {held}");
        let kept = arena.retained_capacity();
        arena.reset();
        assert_eq!((arena.retained_capacity(), pool_bytes(a) - q0), (kept, held), "chunks kept across a reset");
        drop(arena);
        assert_eq!(pool_bytes(a), q0, "off at the drop");
        pagepool::flush_shelf();
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}
