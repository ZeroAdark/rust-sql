//! pagepool P3 (`PGRUST_POOL_GRAVITY=1`): an operator arena's allocator-road chunks come from the page pool. The owner holds
//! them once each (`Door::Pool`: among its account's pool bytes, nothing moved off the drawing thread), all of what the pool
//! handed out; a reset or the drop gives them back to the pool and off the owner; an uncharged arena keeps the allocator's
//! road.
#![cfg(target_os = "linux")]

use std::alloc::Layout;
use std::sync::{Mutex, MutexGuard, Once};

use gravity_fn::arena::{AVec, Kind, OpArena};
use memledger::charge::usage;
use memledger::{account_row, Acct};
use pagepool::Tag;

/// The pool armed once with the Gravity switch, and this test alone on it.
fn serial() -> MutexGuard<'static, ()> {
    static ONCE: Once = Once::new();
    static TURN: Mutex<()> = Mutex::new(());
    ONCE.call_once(|| {
        std::env::set_var("PGRUST_POOL_GRAVITY", "1");
        assert!(pagepool::switches().gravity, "the switch, read at its first call");
        assert!(pagepool::init(&pagepool::Config::from_policy()), "the pool reserves its zones");
    });
    TURN.lock().unwrap_or_else(|e| e.into_inner())
}

/// On a new thread bound to backend `procno`, inside a statement: `f(account)`.
fn in_statement(procno: u32, f: impl FnOnce(Acct) + Send + 'static) {
    std::thread::spawn(move || {
        memledger::bind_backend(procno, 9000 + procno);
        memledger::statement_begin();
        f(Acct::backend(procno));
        let _ = memledger::statement_end();
        pagepool::flush_shelf();
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}

fn pool_bytes(a: Acct) -> i64 {
    memledger::fold();
    account_row(a).pagepool_bytes
}

fn op_in_use() -> usize {
    pagepool::stats().in_use[Tag::OP.0 as usize]
}

#[test]
fn an_operator_arenas_chunks_are_the_pools_on_its_owner_once_each() {
    let _t = serial();
    in_statement(4801, |a| {
        let (q0, u0) = (pool_bytes(a), op_in_use());
        let mut arena = OpArena::new(core::ptr::null_mut(), Kind::Sort, 1 << 20);
        let o = arena.owner();
        let shell = usage(o).held;
        let mut v: AVec<u64> = AVec::new();
        for i in 0..1_000_000u64 {
            v.push(&arena, i).unwrap();
        }
        let p = arena.alloc(Layout::from_size_align(24, 8).unwrap()).unwrap();
        assert!(pagepool::owns(p.as_ptr()), "the pool's chunk");
        let chunks = arena.chunk_bytes() as i64;
        assert!(chunks >= 8_000_000, "the vector's bytes are inside them");
        assert_eq!(usage(o).held, shell + chunks, "the chunks, each once, and the core");
        assert_eq!(pool_bytes(a) - q0, chunks, "on the account's pool bytes, all the pool handed out");
        assert_eq!(op_in_use() - u0, chunks as usize, "the pool's OP row holds what the owner holds");
        assert_eq!(v.as_slice(&arena)[999_999], 999_999);
        arena.reset();
        assert_eq!((usage(o).held, arena.chunk_bytes(), pool_bytes(a) - q0, op_in_use()), (shell, 0, 0, u0), "back at the reset");
        v.detach();
        v.push(&arena, 9).unwrap();
        assert!(pool_bytes(a) > q0);
        drop(arena);
        assert_eq!((usage(o).held, pool_bytes(a), op_in_use()), (0, q0, u0), "the owner leaves with its arena, the chunks with it");
    });
}

#[test]
fn an_uncharged_arena_keeps_the_allocators_road() {
    let _t = serial();
    in_statement(4802, |a| {
        let q0 = pool_bytes(a);
        let arena = OpArena::scratch_of(None, Kind::Region);
        let p = arena.alloc(Layout::from_size_align(4096, 8).unwrap()).unwrap();
        assert!(!pagepool::owns(p.as_ptr()), "no owner: the allocator's chunk, on the allocating thread as door 1's");
        assert_eq!(pool_bytes(a), q0);
    });
}
