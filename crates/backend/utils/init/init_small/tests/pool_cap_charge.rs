//! P6 (`pool-global`, D4718): PoolAlloc's claims past the pool's claim cap stay in the ledger's sight, by construction:
//! the bytes are resident (the read the ladder judges) and door 1 put them on the allocating backend's account, not on
//! the unattributed row. Its own binary: the global allocator.
#![cfg(all(feature = "pool-global", target_os = "linux"))]

use init_small::oom::Rescue;
use memledger::{account_row, bind_backend, fold, unbind_backend, Acct};

#[global_allocator]
static A: Rescue<pagepool::PoolAlloc> = Rescue(pagepool::PoolAlloc);

const MIB: usize = 1 << 20;

fn rss_anon() -> usize {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    let kb = s.lines().find_map(|l| l.strip_prefix("RssAnon:")).unwrap().trim().trim_end_matches(" kB").trim();
    kb.parse::<usize>().unwrap() * 1024
}

#[test]
fn past_cap_claims_are_resident_and_on_the_allocating_backends_account() {
    init_small::oom::charge_usable_layout(true, pagepool::good_size);
    std::thread::spawn(|| {
        bind_backend(3992, 8200 + 3992);
        let a = Acct::backend(3992);
        drop(vec![1u8; MIB]);
        pagepool::return_warm_now(usize::MAX);
        let mut big: Vec<Vec<u8>> = Vec::with_capacity(64);
        fold();
        let (a0, r0) = (account_row(a).account_bytes, rss_anon());
        let cap = pagepool::stats().claimed + 2 * MIB;
        pagepool::set_cap(cap);
        for i in 0..64 {
            big.push(vec![i as u8 | 1; MIB]);
        }
        let small: Vec<Box<[u8; 100]>> = (0..100_000).map(|_| Box::new([7u8; 100])).collect();
        let bytes = (64 * MIB + small.capacity() * 8 + small.len() * 100) as i64;
        let claimed = pagepool::stats().claimed;
        fold();
        let moved = account_row(a).account_bytes - a0;
        let grew = rss_anon() - r0;
        pagepool::set_cap(0);
        assert!(claimed > cap, "claimed past the cap: {claimed} <= {cap}");
        assert!(moved >= bytes && moved < bytes + 2 * MIB as i64, "the allocating backend's account moved by the blocks: {moved} vs {bytes}");
        assert!(grew >= 64 * MIB, "the past-cap blocks are resident, in the ladder's read: RssAnon +{grew}");
        drop(big);
        drop(small);
        unbind_backend();
    })
    .join()
    .unwrap();
}
