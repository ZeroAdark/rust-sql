//! The cache road's reading (`arena::road`): its live blocks (the global allocator's `road_live` at every cache-road take
//! and free, on any thread), what scopes that name no owner kept (`plain`: none can, every scope names its owner), and
//! the live bytes no cache holder's charge covers (`unheld`). Its own binary: the counters are process-wide. No global
//! allocator here: the test makes the allocator's calls itself (`door_heap`, `cache_took`, `road_live`), in the order the
//! server's wrapper makes them.

use memledger::arena::{cache_scope_for, cache_took, road, road_live, set_cache_heap_ready};
use memledger::charge::context;
use memledger::{bind_backend, door_heap, statement_begin, statement_end, unbind_backend, Consumer, Holder, Kind, Owner};

const MIB: isize = 1 << 20;
const H: Owner = Owner::Cache(Holder::Of(Consumer::PartCache));

/// A cache-road block of `n` bytes taken (+) or freed (-) on this thread inside a cache scope, as the wrapper books it.
fn block(own: bool, n: isize) {
    door_heap(n);
    cache_took(n);
    road_live(own, n);
}

#[test]
fn the_road_counts_live_blocks_and_what_no_cache_holder_covers() {
    std::thread::spawn(|| {
        set_cache_heap_ready();
        bind_backend(3961, 1);
        statement_begin();
        let r0 = road();
        assert_eq!(r0.committed, None, "no arenas installed here");
        assert_eq!(r0.retention(), None);

        // an image the part cache keeps: 3 MiB in the cache arena, 1 MiB of it a transient freed inside; charged as taken
        let s = cache_scope_for(H, Kind::Region);
        block(false, 3 * MIB);
        block(false, -MIB);
        let kept = s.finish().expect("a holder scope's charge");
        let r1 = road();
        assert_eq!(r1.arena_live - r0.arena_live, 2 << 20, "{r1:?}");
        assert_eq!(r1.holders_heap - r0.holders_heap, 2 << 20);
        assert_eq!(r1.unheld(), r0.unheld(), "the holder covers it: {r1:?}");

        // an image a reader keeps and no cache does (its statement's): 1 MiB in the own range, live and unheld while it
        // runs, so the check reads between statements
        let (o, _) = context();
        let s = cache_scope_for(o, Kind::Region);
        block(true, MIB);
        let read = s.finish().expect("a statement scope's charge");
        let r2 = road();
        assert_eq!(r2.own_live - r0.own_live, 1 << 20);
        assert_eq!(r2.holders_heap, r1.holders_heap, "no cache holds it");
        assert_eq!(r2.unheld() - r0.unheld(), 1 << 20, "{r2:?}");

        // the frees: each charge back on this thread, then its blocks freed outside any scope (an eviction, the reader's end)
        drop(read);
        door_heap(-MIB);
        road_live(true, -MIB);
        drop(kept);
        door_heap(-2 * MIB);
        road_live(false, -2 * MIB);
        let r3 = road();
        assert_eq!((r3.arena_live, r3.own_live, r3.holders_heap), (r0.arena_live, r0.own_live, r0.holders_heap), "{r3:?}");
        assert_eq!(r3.plain, r0.plain, "every scope named its owner");
        statement_end();
        unbind_backend();
    })
    .join()
    .unwrap();
}
