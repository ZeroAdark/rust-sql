//! P6 (`pool-global`): under `Rescue<pagepool::PoolAlloc>`, a cache scope's blocks are the pool allocator's cache class
//! and are charged as the cache arena's were (D4321 / CACHE-CHARGE): built on thread A inside a holder scope, the image
//! is the holder's while it lives (none of it on A's account), and evicted on thread B (the holder's charge dropped,
//! then the free) it leaves the holder at 0 and neither backend's account moved. Its own binary: the global allocator.
#![cfg(all(feature = "pool-global", target_os = "linux"))]

use init_small::oom::Rescue;
use memledger::arena::{cache_scope_for, set_cache_heap_ready};
use memledger::charge::{owner_row, Door};
use memledger::{account_row, bind_backend, fold, unbind_backend, Acct, Charge, Consumer, Holder, Kind, Owner, Release};

#[global_allocator]
static A: Rescue<pagepool::PoolAlloc> = Rescue(pagepool::PoolAlloc);

const SLACK: i64 = 4096;
const MIB: usize = 1 << 20;
const H: Owner = Owner::Cache(Holder::Of(Consumer::ZoneStore));

fn account(a: Acct) -> i64 {
    fold();
    account_row(a).account_bytes
}

#[test]
fn a_cache_image_on_the_pool_is_its_holders_and_nets_out_where_it_is_evicted() {
    init_small::oom::charge_usable_layout(true, pagepool::good_size);
    init_small::oom::set_cache_alloc(pagepool::cache_alloc, pagepool::cache_realloc);
    init_small::oom::set_cache_test(pagepool::is_cache);
    set_cache_heap_ready();
    std::thread::spawn(|| {
        bind_backend(3990, 8200 + 3990);
        let a = Acct::backend(3990);
        let (a0, h0) = (account(a), owner_row(H).committed);
        let scope = cache_scope_for(H, Kind::Region);
        let mut img: Vec<u8> = Vec::new();
        for _ in 0..(3 * MIB / 4096) {
            img.extend_from_slice(&[7; 4096]);
        }
        let small: Vec<Box<[u8; 48]>> = (0..2000).map(|_| Box::new([5u8; 48])).collect();
        let cap = img.capacity() + small.capacity() * 8 + small.len() * 48;
        assert!(pagepool::is_cache(img.as_ptr(), img.capacity(), 1), "the image is the cache class's");
        assert!(small.iter().all(|b| pagepool::is_cache(b.as_ptr(), 48, 1)), "small images too, on cache-only runs");
        let taken = scope.finish().expect("a holder scope's charge");
        assert_eq!(taken.committed() as usize, cap, "the image's blocks alone (the growth's transient blocks netted)");
        assert_eq!((owner_row(H).committed - h0) as usize, cap, "the holder holds the image while it lives");
        assert!((account(a) - a0).abs() < SLACK, "none of it on the building backend: {}", account(a) - a0);
        let mut held = Charge::open(H, Release::Evict, Kind::Region, Door::Heap, 0);
        assert_eq!(held.absorb(taken) as usize, cap);
        let moved = std::thread::spawn(move || {
            bind_backend(3991, 8200 + 3991);
            let b = Acct::backend(3991);
            let b0 = account(b);
            drop(held);
            drop(img);
            drop(small);
            pagepool::flush_remote();
            let moved = account(b) - b0;
            unbind_backend();
            moved
        })
        .join()
        .unwrap();
        assert!(moved.abs() < SLACK, "the evicting backend's account nets to 0: {moved}");
        assert_eq!(owner_row(H).committed, h0, "the holder holds nothing after");
        assert!((account(a) - a0).abs() < SLACK, "the building backend's account is back: {}", account(a) - a0);
        unbind_backend();
    })
    .join()
    .unwrap();
}
