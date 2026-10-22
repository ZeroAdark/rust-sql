//! CACHE-CHARGE: a cache scope that names its holder (`arena::cache_scope_for`) charges the holder the cache-road blocks
//! taken while it is open, as they are taken (`arena::cache_took`, folded at `DOOR_FOLD_BYTES`), so a cache build in
//! flight is on the holder's row, not the backend's unattributed bytes. The image takes the scope's charge over
//! (`Charge::absorb`) and gives it back where it is freed. Its own binary: the global allocator here is door 1 and, inside
//! a cache scope, the cache road (every block of a scope is a cache-road block here), as the server's wrapper is.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;

use memledger::arena::{cache_scope_for, cache_took, in_cache_scope, scope_foreign_frees, set_cache_heap_ready};
use memledger::charge::{owner_row, Door};
use memledger::{account_row, bind_backend, door_heap, fold, unbind_backend, Acct, Charge, Consumer, Holder, Kind, Owner, Release, DOOR_FOLD_BYTES};

struct Door1;

// SAFETY: forwards to the system allocator; `door_heap` / `cache_took` are const thread-local adds (no allocation, no
// destructor), a fold atomics only.
unsafe impl GlobalAlloc for Door1 {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller's layout, forwarded.
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            door_heap(l.size() as isize);
            if in_cache_scope() {
                cache_took(l.size() as isize);
            }
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: `p` came from `alloc` with `l`.
        unsafe { System.dealloc(p, l) };
        door_heap(-(l.size() as isize));
        if in_cache_scope() {
            cache_took(-(l.size() as isize));
        }
    }
}

#[global_allocator]
static A: Door1 = Door1;

/// What the runtime around a spawn / join moves between two threads' accounts: far below the blocks charged here.
const SLACK: i64 = 4096;
const MIB: i64 = 1 << 20;

/// The holders here: no other test binary's (own binary), one test at a time (`ONE`).
const H1: Owner = Owner::Cache(Holder::Of(Consumer::ZoneStore));
const H2: Owner = Owner::Cache(Holder::Of(Consumer::CondCache));
static ONE: Mutex<()> = Mutex::new(());

/// Each test on its own thread and backend slot (3970..3979: no other test's).
fn on(procno: u32, f: impl FnOnce(Acct) + Send + 'static) {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    set_cache_heap_ready();
    std::thread::spawn(move || {
        bind_backend(procno, 8200 + procno);
        f(Acct::backend(procno));
        unbind_backend();
    })
    .join()
    .unwrap();
}

fn account(a: Acct) -> i64 {
    fold();
    account_row(a).account_bytes
}

fn committed(o: Owner) -> i64 {
    owner_row(o).committed
}

#[test]
fn a_build_in_flight_is_on_its_holder_and_the_image_gives_it_back_where_it_is_freed() {
    on(3970, |a| {
        let (a0, h0) = (account(a), committed(H1));
        let scope = cache_scope_for(H1, Kind::Region);
        let mut img: Vec<u8> = Vec::new();
        for _ in 0..(3 * MIB as usize / 4096) {
            img.extend_from_slice(&[7; 4096]);
        }
        let cap = img.capacity() as i64;
        let during = committed(H1) - h0;
        assert!((during - cap).abs() <= DOOR_FOLD_BYTES, "the build in flight is the holder's: {during} for {cap}");
        assert!((account(a) - a0 - (cap - during)).abs() < SLACK, "and off the backend's account but the unfolded rest");
        let taken = scope.finish().expect("a holder scope's charge");
        assert_eq!(taken.committed() as i64, cap, "the growth's transient blocks netted: the image's block alone");
        assert_eq!(committed(H1) - h0, cap);
        assert!((account(a) - a0).abs() < SLACK, "the backend's account holds none of it: {}", account(a) - a0);
        // the holder's insert takes the charge over: nothing committed twice
        let mut held = Charge::open(H1, Release::Evict, Kind::Region, Door::Heap, 0);
        assert_eq!(held.absorb(taken) as i64, cap);
        assert_eq!(committed(H1) - h0, cap);
        // evicted on another backend's thread: the charge's drop there, then the free there
        let b = std::thread::spawn(move || {
            bind_backend(3971, 8200 + 3971);
            let b = Acct::backend(3971);
            let b0 = account(b);
            drop(held);
            drop(img);
            let moved = account(b) - b0;
            unbind_backend();
            moved
        })
        .join()
        .unwrap();
        assert!(b.abs() < SLACK, "the evicting thread's account nets to 0: {b}");
        assert_eq!(committed(H1), h0, "the holder holds nothing after");
        assert!((account(a) - a0).abs() < SLACK, "the building thread's account is back: {}", account(a) - a0);
    });
}

#[test]
fn a_scope_dropped_unfinished_gives_its_bytes_back_to_the_thread_that_frees_them() {
    on(3972, |a| {
        let (a0, h0) = (account(a), committed(H1));
        let v: Vec<u8> = {
            let _scope = cache_scope_for(H1, Kind::Region);
            let v = vec![1u8; 2 * MIB as usize];
            assert_eq!(committed(H1) - h0, 2 * MIB, "taken: the holder's");
            v
        };
        assert_eq!(committed(H1), h0, "a scope ended without finish holds nothing");
        assert!((account(a) - a0 - 2 * MIB).abs() < SLACK, "the block is the thread's again");
        drop(v);
        assert!((account(a) - a0).abs() < SLACK);
    });
}

#[test]
fn a_free_the_scope_did_not_take_is_clamped_and_counted() {
    on(3973, |a| {
        let (a0, h0, f0) = (account(a), committed(H1), scope_foreign_frees());
        let older = vec![3u8; MIB as usize];
        let scope = cache_scope_for(H1, Kind::Region);
        drop(older);
        let taken = scope.finish().expect("a charge");
        assert_eq!((taken.committed(), committed(H1) - h0), (0, 0), "never below what it took");
        assert_eq!(scope_foreign_frees() - f0, MIB as u64, "the foreign free counted");
        drop(taken);
        assert!((account(a) - a0).abs() < SLACK);
    });
}

#[test]
fn nested_scopes_charge_the_innermost_holder() {
    on(3974, |a| {
        let (a0, h1, h2) = (account(a), committed(H1), committed(H2));
        let outer = cache_scope_for(H1, Kind::Region);
        let x = vec![1u8; MIB as usize];
        let inner = cache_scope_for(H2, Kind::Region);
        let y = vec![2u8; 2 * MIB as usize];
        let t2 = inner.finish().expect("inner");
        let z = vec![3u8; MIB as usize];
        let t1 = outer.finish().expect("outer");
        assert_eq!((t1.committed() as i64, t2.committed() as i64), (2 * MIB, 2 * MIB), "x + z on the outer, y on the inner");
        assert_eq!((committed(H1) - h1, committed(H2) - h2), (2 * MIB, 2 * MIB));
        drop((t1, t2));
        drop((x, y, z));
        assert_eq!((committed(H1), committed(H2)), (h1, h2));
        assert!((account(a) - a0).abs() < SLACK);
    });
}

#[test]
fn absorb_moves_another_owners_charge_and_fit_brings_it_to_the_image() {
    on(3975, |a| {
        let (a0, h1, h2) = (account(a), committed(H1), committed(H2));
        let block = vec![0u8; 3 * MIB as usize];
        let t = Charge::open(H2, Release::Evict, Kind::Region, Door::Heap, 3 * MIB as u64);
        let mut held = Charge::open(H1, Release::Evict, Kind::Sort, Door::Heap, 0);
        assert_eq!(held.absorb(t), 3 * MIB as u64);
        assert_eq!((committed(H1) - h1, committed(H2) - h2), (3 * MIB, 0), "moved to the absorbing owner");
        held.fit_committed(2 * MIB as u64);
        assert_eq!(committed(H1) - h1, 2 * MIB, "a fit down frees the rest to this thread");
        held.fit_committed(3 * MIB as u64);
        assert_eq!(committed(H1) - h1, 3 * MIB);
        drop(held);
        drop(block);
        assert_eq!(committed(H1), h1);
        assert!((account(a) - a0).abs() < SLACK);
    });
}

/// D4834: a split moves bytes between two claims of one owner, never its totals; each part gives its bytes back at its drop.
#[test]
fn split_off_moves_a_claim_and_each_part_gives_its_bytes_back() {
    on(3977, |a| {
        let (a0, h1) = (account(a), committed(H1));
        let block = vec![0u8; 3 * MIB as usize];
        let mut held = Charge::open(H1, Release::Evict, Kind::Sort, Door::Heap, 3 * MIB as u64);
        let a1 = account(a);
        let part = held.split_off(MIB as u64);
        assert_eq!((held.committed() as i64, part.committed() as i64), (2 * MIB, MIB));
        assert_eq!((part.owner(), part.release(), part.kind(), part.acct()), (held.owner(), held.release(), held.kind(), held.acct()));
        assert_eq!(committed(H1) - h1, 3 * MIB, "the owner's total does not move");
        assert_eq!(account(a), a1, "nor the account's");
        assert_eq!(held.absorb(part), MIB as u64);
        assert_eq!(held.committed() as i64, 3 * MIB, "absorbed back: the original claim");
        let part = held.split_off(MIB as u64);
        drop(part);
        assert_eq!(committed(H1) - h1, 2 * MIB, "the split part's drop gives its bytes back");
        drop(held);
        drop(block);
        assert_eq!(committed(H1), h1);
        assert!((account(a) - a0).abs() < SLACK);
    });
}

/// CONVERSION WAVE 3 (cvpread Q4): a scope may name a statement's operator, not only a holder: an uncached image built on
/// the cache road is that owner's as it is allocated, with the owner's release (`Statement`), and its bytes never leave
/// the backend's account (the operator's cell is on it): they move from its unattributed bytes to the operator's row.
#[test]
fn a_scope_naming_an_operator_charges_it_with_the_statements_release() {
    on(3976, |a| {
        memledger::statement_begin();
        let op = memledger::charge::op_begin(memledger::charge::context().0, Kind::Sort as u32, 0);
        let a0 = account(a);
        let scope = cache_scope_for(op, Kind::Region);
        let img = vec![3u8; 2 * MIB as usize];
        assert_eq!(committed(op), 2 * MIB, "taken: the operator's as it was allocated");
        let taken = scope.finish().expect("a scope naming an owner has its charge");
        assert_eq!((taken.owner(), taken.release(), taken.committed() as i64), (op, Release::Statement, 2 * MIB));
        assert!((account(a) - a0 - 2 * MIB).abs() < SLACK, "the bytes stay on the backend's account, now owned");
        // the reader's image drops its charge first, then its block (Img's field order)
        drop(taken);
        drop(img);
        assert_eq!(committed(op), 0);
        assert!((account(a) - a0).abs() < SLACK);
        memledger::charge::op_end(op);
        let _ = memledger::statement_end();
    });
}
