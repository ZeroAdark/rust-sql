//! pagepool P3 under `PoolAlloc` as the global allocator (the pagepool-global build, the Gravity switch unset): the global
//! allocator's blocks share the page pool's range, so an operator arena's allocator-road chunks (`std::alloc`) lie inside
//! it. Their give must free them to the global allocator and off the rig's charge, never hand them to `pagepool::give` and
//! the pool door's charge (the range test `owns` would: the shape of D4720, and the owner kept the chunks' rig bytes after
//! the arena was gone). `pagepool::taken` tells the pool's own blocks. Its own binary: it sets the global allocator.
#![cfg(target_os = "linux")]

use gravity_fn::arena::{AVec, Kind, OpArena};
use memledger::charge::usage;
use memledger::{account_row, Acct};
use pagepool::PoolAlloc;

#[global_allocator]
static A: PoolAlloc = PoolAlloc;

#[test]
fn a_charged_arenas_allocator_chunks_inside_the_pools_range_leave_with_the_arena() {
    if pagepool::switches().gravity {
        // the premise is the switch unset: with it on (a run forcing PGRUST_POOL_GRAVITY), a charged arena takes the
        // pool's chunks (`taken`, the pool's door), which other tests witness
        eprintln!("SKIP: PGRUST_POOL_GRAVITY is set; this witness is the allocator road's");
        return;
    }
    std::thread::spawn(|| {
        memledger::bind_backend(4811, 9811);
        memledger::statement_begin();
        assert!(pagepool::global_on(), "PoolAlloc serves this process");
        let a = Acct::backend(4811);
        memledger::fold();
        let q0 = account_row(a).pagepool_bytes;
        let mut arena = OpArena::new(core::ptr::null_mut(), Kind::Sort, 1 << 20);
        let o = arena.owner();
        let shell = usage(o).held;
        let mut v: AVec<u64> = AVec::new();
        for i in 0..200_000u64 {
            v.push(&arena, i).unwrap();
        }
        let p = v.as_slice(&arena).as_ptr() as *const u8;
        assert!(pagepool::owns(p), "the global allocator's chunk is inside the pool's range");
        assert!(!pagepool::taken(p), "and is not a block the pool's take handed out");
        let chunks = arena.chunk_bytes() as i64;
        assert!(chunks >= 1_600_000, "the vector's bytes are in the chunks: {chunks}");
        assert_eq!(usage(o).held, shell + chunks, "the chunks on their owner, through the rig's road");
        arena.reset();
        memledger::fold();
        assert_eq!(usage(o).held, shell, "back at the reset: the chunks freed off the rig's charge");
        assert_eq!(account_row(a).pagepool_bytes, q0, "and none of them through the pool's door");
        v.detach();
        v.push(&arena, 9).unwrap();
        drop(arena);
        memledger::fold();
        assert_eq!(usage(o).held, 0, "the owner leaves with its arena, the chunks with it");
        assert_eq!(account_row(a).pagepool_bytes, q0, "nothing went through the pool's door");
        let _ = memledger::statement_end();
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}
