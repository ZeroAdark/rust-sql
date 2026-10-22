//! D4762: the tier-0 compile pool builds, files and frees its modules in the program caches' account (process-wide state:
//! the ledger's homes and accounts, the code cache, the pool). Before, the pool's thread (no backend: UNBOUND) made each
//! entry's own blocks (its Arc block, its root's header, the compile's headers) and the session that later evicted the entry
//! freed them: UNBOUND only rose, and that session's account fell by the same bytes. Door 1 counts every block on the
//! calling thread here, as the server's allocator does.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::Ordering;

use gravity_backend::{cache, mapped_cached_all, promote, Lookup};
use gravity_ir::text::parse_program;
use gravity_ir::Func;

struct Door1;

unsafe impl GlobalAlloc for Door1 {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            memledger::door_heap(l.size() as isize);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l);
        memledger::door_heap(-(l.size() as isize));
    }
}

#[global_allocator]
static A: Door1 = Door1;

fn konst(k: i64) -> (gravity_ir::Program, Func) {
    let src = format!("(program (fn e ((c ptr)) (i64) (block entry ((c ptr)) (k (const i64 {k})) (return k))))");
    let p = parse_program(&src, &|_| None).unwrap_or_else(|e| panic!("{e}"));
    let f = Func(p.funcs.iter().position(|f| f.name == "e").unwrap() as u32);
    (p, f)
}

/// Modules the pool compiles and files: enough that the pool thread's door-1 counts fold onto its account many times over
/// (one fold step is `DOOR_FOLD_BYTES`, 256 KiB) had they stayed on UNBOUND.
const N: i64 = 1024;
/// What a thread's pending door-1 counts may leave off its account.
const SLACK: i64 = 128 << 10;

#[test]
fn the_pool_builds_files_and_frees_its_modules_in_the_program_caches_account() {
    memledger::set_home(true);
    std::env::set_var("PGRUST_GRAVITY_TIER0", "force");
    cache::set_budget_bytes(1 << 30);
    let pc = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::ProgramCaches)).acct();
    let unbound = memledger::Acct::UNBOUND;
    let at = |a: memledger::Acct| memledger::account_row(a).account_bytes;
    // the statements' side is one backend, on its own account; the pool's thread is the only UNBOUND one that runs
    std::thread::spawn(move || {
        memledger::bind_backend(4078, 9000 + 4078);
        memledger::statement_begin();
        let b = memledger::Acct::backend(4078);
        assert!(b.is_backend(), "a backend slot");
        let compile = |k: i64| {
            let (p, f) = konst(k);
            let (codes, o) = mapped_cached_all(&p, &[f]).unwrap();
            assert_eq!((codes[0].kind(), o.lookup), ("tier0", Lookup::Miss), "a forced miss goes to the pool");
            codes
        };
        // the pool's thread started, its first job done
        let warm = compile(4_762_000);
        promote::quiesce();
        memledger::fold();
        let (u0, p0, b0) = (at(unbound), at(pc), at(b));
        let (filed, dropped) = (promote::FILED.load(Ordering::Relaxed), promote::DROPPED.load(Ordering::Relaxed));
        let mut held = Vec::new();
        for i in 0..N {
            held.push(compile(4_762_001 + i));
            if i % 8 == 7 {
                promote::quiesce();
            }
        }
        promote::quiesce();
        assert_eq!(promote::DROPPED.load(Ordering::Relaxed), dropped, "no job dropped");
        assert_eq!(promote::FILED.load(Ordering::Relaxed), filed + N as u64, "the pool filed every module");
        memledger::fold();
        let (u1, p1, b1) = (at(unbound), at(pc), at(b));
        // the entries leave on this backend: its statements' codes, then every module the cache holds
        drop((held, warm));
        cache::reset();
        memledger::fold();
        let (u2, p2, b2) = (at(unbound), at(pc), at(b));
        memledger::statement_end();
        memledger::unbind_backend();
        eprintln!(
            "D4762 witness: {N} modules the pool filed: UNBOUND {:+}, program_caches {:+}, the backend {:+}; then the backend \
             dropped them: UNBOUND {:+}, program_caches {:+}, the backend {:+} (from the start)",
            u1 - u0,
            p1 - p0,
            b1 - b0,
            u2 - u0,
            p2 - p0,
            b2 - b0
        );
        assert!((u1 - u0).abs() <= SLACK, "the pool's builds are the program caches', not UNBOUND's: {:+}", u1 - u0);
        assert!(p1 - p0 >= N * mcx::SendRoot::HEADER_BYTES as i64, "the modules' blocks on the program caches: {:+}", p1 - p0);
        assert!((u2 - u0).abs() <= SLACK, "nothing left on UNBOUND: {:+}", u2 - u0);
        // what stays: the program caches' map tables (a reset keeps their buckets) and the pool thread's recycled context
        // nodes; the backend's own residue (its thread's pools) is the same with the modules made anywhere
        assert!(p2 - p0 >= -SLACK && p2 - p0 <= (p1 - p0) / 50, "the program caches gave the modules' bytes back: {:+} of {:+}", p2 - p0, p1 - p0);
        assert!(b2 - b0 >= -SLACK, "the backend that dropped them gave back only its own bytes: {:+}", b2 - b0);
    })
    .join()
    .unwrap();
}
