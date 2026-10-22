//! D4679: mcx's growth shelf (aset.rs `growth`, behind PGRUST_MEMPOLICY keep_growth, default 0). A dropped context's growth
//! blocks (its blocks after the first) park on its thread's shelf, counted as parked keepers on the thread's own session
//! account and apart as growth (`memledger::growth`: bytes, blocks), at most 64 KiB; the thread's next context takes a
//! parked block of the size it needs before it mallocs. Off parks nothing; a thread that charges no session account of its
//! own parks nothing; the first park after the memory-short epoch moved (REACH) sheds the shelf; the session's unbind, a
//! thread-reuse park, idle passivation and the thread's exit empty it, each counted out on the account it parked on.
//!
//! An integration build compiles the lib WITHOUT cfg(test), so the shelf is live here. One #[test] in this binary: the
//! knob and the memory-short epoch are process-wide.
//!
//! Run: cargo test -p mcx --features std --test growth_shelf
#![cfg(feature = "std")]

use core::alloc::Layout;

use mcx::{Allocator, MemoryContext};
use memledger::Acct;
use mempolicy::consts::{set, K};

const KIB: usize = 1024;

/// A child of `root` grown by 1 KiB chunks until its blocks hold `bytes` (8 KiB first, then 8, 16, 32, 64 KiB ..).
fn grown(root: &MemoryContext, bytes: usize) -> MemoryContext {
    let c = root.new_child("grown");
    let one = Layout::from_size_align(KIB, 8).unwrap();
    while c.mem_allocated() < bytes {
        c.mcx().allocate(one).expect("a 1 KiB chunk");
    }
    assert_eq!(c.mem_allocated(), bytes, "blocks of 8, 8, 16, 32 .. KiB");
    c
}

/// On a new thread bound to backend `procno`: `f(its account, a root context)`; unbinds after unless `f` did.
fn bound(procno: u32, f: impl FnOnce(Acct, &MemoryContext) + Send + 'static) {
    std::thread::spawn(move || {
        let a = Acct::backend(procno);
        assert!(a.is_backend());
        memledger::bind_backend(procno, 9000 + procno);
        let root = MemoryContext::new("growth-shelf root");
        f(a, &root);
        drop(root);
        if memledger::bound() == a {
            memledger::unbind_backend();
        }
    })
    .join()
    .expect("the bound thread");
}

#[test]
fn a_dropped_contexts_growth_blocks_park_on_its_session_account_up_to_the_cap_and_go_at_the_session_end() {
    let g = |a| memledger::growth(a);
    let none = (0i64, 0i64);
    // 8 (first) + 8 + 16 + 32 KiB: three growth blocks, 56 KiB
    let (blocks, parked) = (64 * KIB, (56 * KIB as i64, 3i64));

    // off (the default): nothing parks
    bound(4060, move |a, root| {
        drop(grown(root, blocks));
        assert_eq!(g(a), none, "off: a dropped context frees its growth blocks");
    });

    set(K::KeepGrowth, 1);
    bound(4061, move |a, root| {
        assert_eq!(memledger::bound(), a);
        let k0 = memledger::keepers(a);
        drop(grown(root, blocks));
        assert_eq!(g(a), parked, "on: the growth blocks park on the session's account");
        assert!(memledger::keepers(a) - k0 >= parked.0, "counted in the account's keepers too");

        // the next context takes them back, block for block, before it mallocs
        let c = grown(root, blocks);
        assert_eq!(g(a), none, "taken: counted out");
        drop(c);
        assert_eq!(g(a), parked);

        // the cap: a context with a 64 KiB block too keeps the shelf at 64 KiB in at most 4 blocks
        drop(grown(root, 2 * blocks));
        let (b, n) = g(a);
        assert!(b > 0 && b <= 64 * KIB as i64 && n <= 4, "at most 64 KiB in 4 blocks: {b} in {n}");

        // a thread-reuse park empties the shelf; so does idle passivation
        mcx::run_thread_reuse_hooks();
        assert_eq!(g(a), none, "a thread-reuse park empties the shelf");
        drop(grown(root, blocks));
        assert_eq!(g(a), parked);
        let _ = mcx::passivate_trim();
        assert_eq!(g(a), none, "idle passivation empties the shelf");

        // REACH: the first park after the memory-short epoch moved sheds the shelf and parks nothing (a context with one
        // growth block: it takes the shelf's 8 KiB, its drop sheds the 16 and 32 KiB and frees the 8)
        drop(grown(root, blocks));
        assert_eq!(g(a), parked);
        mcx::memory_short_now();
        drop(grown(root, 16 * KIB));
        assert_eq!(g(a), none, "memory short: the shelf sheds");
        drop(grown(root, blocks));
        assert_eq!(g(a), parked, "after the shed the shelf parks again");

        // the session's unbind empties it while the thread still charges the account; after it nothing parks
        memledger::unbind_backend();
        assert_eq!(g(a), none, "the unbind empties the shelf, counted out on the session's account");
        assert_eq!(memledger::bound(), Acct::UNBOUND);
        let unbound = g(Acct::UNBOUND);
        drop(grown(root, blocks));
        assert_eq!((g(a), g(Acct::UNBOUND)), (none, unbound), "no session account of its own: nothing parks");
    });

    // the thread's exit empties it too
    let left = std::thread::spawn(move || {
        memledger::bind_backend(4063, 9063);
        let root = MemoryContext::new("exits bound");
        drop(grown(&root, blocks));
        let at = memledger::growth(Acct::backend(4063));
        drop(root);
        at
    })
    .join()
    .expect("the exiting thread");
    assert_eq!(left, parked);
    assert_eq!(g(Acct::backend(4063)), none, "the thread's exit frees its shelf, counted out");

    // a thread bound to no session parks nothing
    std::thread::spawn(move || {
        let (t, u) = (memledger::growth_total(), g(Acct::UNBOUND));
        let root = MemoryContext::new("unbound root");
        drop(grown(&root, blocks));
        assert_eq!((memledger::growth_total(), g(Acct::UNBOUND)), (t, u), "unbound: nothing parks");
    })
    .join()
    .expect("the unbound thread");
    set(K::KeepGrowth, 0);
}
