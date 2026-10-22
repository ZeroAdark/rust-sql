//! D4762: the code cache's balance (process-wide state: the ledger's homes and accounts, the code cache, the compile pool,
//! mcx's per-thread block pools). Modules the compile pool built (in the program caches' home) and modules a backend built
//! (its statement's, adopted at the filing), all doomed and evicted on that backend, which then makes and drops contexts of
//! its own (taking what its keeper list holds) and trims its pools: the program caches' account is back to exactly its start
//! (and so are its owner bytes and its parked keepers), UNBOUND is unchanged, and the accounts' heap bytes moved by exactly
//! what the allocator holds more. Before, an entry the backend dropped in the program caches' home parked its root's keeper
//! on the backend's keeper list as the program caches': the backend took it for its own contexts and freed it on its own
//! account, and the program caches never got its bytes back. Door 1 counts every block on the calling thread here, as the
//! server's allocator does.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicI64, Ordering};

use gravity_backend::{cache, mapped_cached_all, promote, Lookup};
use gravity_ir::text::parse_program;
use gravity_ir::Func;
use memledger::{Acct, Consumer, Holder, Owner};

static LIVE: AtomicI64 = AtomicI64::new(0);

struct Door1;

unsafe impl GlobalAlloc for Door1 {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            memledger::door_heap(l.size() as isize);
            LIVE.fetch_add(l.size() as i64, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l);
        memledger::door_heap(-(l.size() as isize));
        LIVE.fetch_sub(l.size() as i64, Ordering::Relaxed);
    }
}

#[global_allocator]
static A: Door1 = Door1;

const PC: Owner = Owner::Cache(Holder::Of(Consumer::ProgramCaches));
/// Modules of each kind per round.
const N: i64 = 256;
/// This test's backend slot (no other test binary's: the ledger is per process, but a slot apart keeps the rows apart).
const PROCNO: u32 = 4079;

fn konst(k: i64) -> (gravity_ir::Program, Func) {
    let src = format!("(program (fn e ((c ptr)) (i64) (block entry ((c ptr)) (k (const i64 {k})) (return k))))");
    let p = parse_program(&src, &|_| None).unwrap_or_else(|e| panic!("{e}"));
    let f = Func(p.funcs.iter().position(|f| f.name == "e").unwrap() as u32);
    (p, f)
}

#[derive(Clone, Copy, Debug)]
struct Snap {
    pc: i64,
    pc_owner: i64,
    pc_keepers: i64,
    unbound: i64,
    backend: i64,
    heap_sum: i64,
    live: i64,
}

fn snap() -> Snap {
    memledger::fold();
    let pc = PC.acct();
    let heap_sum = (0..memledger::ACCOUNT_SLOTS as u32)
        .map(|i| {
            let r = memledger::account_row(Acct(i));
            r.account_bytes - r.mapped_bytes
        })
        .sum();
    Snap {
        pc: memledger::account_row(pc).account_bytes,
        pc_owner: memledger::charge::owner_row(PC).committed + memledger::charge::owner_ctx(PC).0,
        pc_keepers: memledger::keepers(pc),
        unbound: memledger::account_row(Acct::UNBOUND).account_bytes,
        backend: memledger::account_row(Acct::backend(PROCNO)).account_bytes,
        heap_sum,
        live: LIVE.load(Ordering::Relaxed),
    }
}

fn delta(a: Snap, b: Snap) -> String {
    format!(
        "program_caches {:+} (owner {:+}, keepers {:+}), UNBOUND {:+}, the backend {:+}, accounts' heap {:+}, allocator live {:+}",
        b.pc - a.pc,
        b.pc_owner - a.pc_owner,
        b.pc_keepers - a.pc_keepers,
        b.unbound - a.unbound,
        b.backend - a.backend,
        b.heap_sum - a.heap_sum,
        b.live - a.live
    )
}

/// One round on the backend: `N` modules the pool files (forced tier-0 misses) and `N` the backend compiles and files
/// itself, keys from `base`; their codes dropped, every module doomed by its key and evicted here; then the backend's own
/// contexts (taking what its keeper list holds) and its pools trimmed. The snapshots: filed, evicted, trimmed.
fn round(base: i64) -> [Snap; 3] {
    let mut held = Vec::new();
    let mut keys = Vec::new();
    for i in 0..N {
        let (p, f) = konst(base + i);
        keys.push(cache::key_of(&p).0);
        let (codes, o) = mapped_cached_all(&p, &[f]).unwrap();
        assert_eq!((codes[0].kind(), o.lookup), ("tier0", Lookup::Miss), "a forced miss goes to the pool");
        held.push(codes);
        if i % 8 == 7 {
            promote::quiesce();
        }
    }
    promote::quiesce();
    for i in 0..N {
        let (p, f) = konst(base + N + i);
        keys.push(cache::key_of(&p).0);
        let owner = memledger::charge::statement_owner();
        let (codes, o) = cache::get_or_compile_all(&p, &[f], |ctx| gravity_tpde::compile_owned(ctx.mcx(), &p, |c| c.set_owner(owner))).unwrap();
        assert_eq!(o.lookup, Lookup::Miss, "the backend compiles and files it");
        held.push(codes);
    }
    let filed = snap();
    drop(held);
    assert_eq!(cache::doom_modules(keys.iter().copied()), 2 * N as usize, "every module filed, doomed by its key");
    cache::evict_doomed();
    assert_eq!(cache::stats().entries, 0, "every module evicted");
    let evicted = snap();
    // the backend's own work after: contexts that take its parked keepers, then an idle backend's trim
    for _ in 0..300 {
        let c = mcx::MemoryContext::new("D4762 balance churn");
        let v = mcx::PgVec::<u64>::with_capacity_in(64, c.mcx());
        drop(v);
        drop(c);
    }
    mcx::passivate_trim();
    [filed, evicted, snap()]
}

#[test]
fn the_program_caches_come_back_exactly_once_every_module_is_doomed_and_evicted() {
    memledger::set_home(true);
    std::env::set_var("PGRUST_GRAVITY_TIER0", "force");
    cache::set_budget_bytes(1 << 30);
    std::thread::spawn(move || {
        memledger::bind_backend(PROCNO, 9000 + PROCNO);
        memledger::statement_begin();
        // the first round grows what stays (the cache's tables, the pool's thread and its pools, this thread's pools)
        round(4_762_100_000);
        let s0 = snap();
        let [s1, s2, s3] = round(4_762_200_000);
        memledger::statement_end();
        memledger::unbind_backend();
        eprintln!("D4762 balance, {N} pool-built + {N} backend-built modules:");
        eprintln!("  filed:   {}", delta(s0, s1));
        eprintln!("  evicted: {}", delta(s0, s2));
        eprintln!("  trimmed: {}", delta(s0, s3));
        assert_eq!(s3.pc - s0.pc, 0, "the program caches' account back to exactly its start");
        assert_eq!(s3.pc_owner - s0.pc_owner, 0, "and its owner bytes");
        assert_eq!(s3.pc_keepers - s0.pc_keepers, 0, "and its parked keepers");
        assert_eq!(s3.unbound - s0.unbound, 0, "UNBOUND unchanged");
        assert_eq!(s3.heap_sum - s0.heap_sum, s3.live - s0.live, "the accounts' heap moved by what the allocator holds more");
    })
    .join()
    .unwrap();
}
