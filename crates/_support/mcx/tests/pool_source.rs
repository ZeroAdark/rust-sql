//! The page pool as a context's block source (the server's PGRUST_POOL_MCX=1 adapter, keepers from the pool too): blocks
//! of `block_min` and up come from the pool and all go back, the owner holds the footprint, a SendRoot dropped on another
//! thread leaves its owner's account where it started, a 64 MiB block is a lazy span that grows, and before the source is
//! set a block takes today's road.
#![cfg(all(feature = "std", target_os = "linux"))]

use std::alloc::Layout;
use std::ptr::{null_mut, NonNull};
use std::sync::{Mutex, MutexGuard, Once};

use mcx::heap::{set_block_source, BlockSource, Lifetime};
use mcx::{Allocator, MemoryContext, SendRoot};
use memledger::charge::owner_ctx;
use memledger::{account_row, Acct, Owner};
use pagepool::{Class, Hint, Tag, Zero};

const KIB: usize = 1 << 10;
const MIB: usize = 1 << 20;
/// The least block these tests' source serves from the pool.
const BLOCK_MIN: usize = KIB;

fn class(life: Lifetime) -> Class {
    match life {
        Lifetime::Task => Class::Task,
        Lifetime::Session => Class::Session,
        Lifetime::Cache => Class::Cache,
        Lifetime::Process => Class::Process,
    }
}

unsafe fn take(bytes: usize, align: usize, life: Lifetime, zeroed: bool) -> *mut u8 {
    let hint = Hint::new(class(life), Tag::MCX).zero(if zeroed { Zero::Dense } else { Zero::No });
    pagepool::take(bytes, align, hint).map_or(null_mut(), |b| b.as_ptr().cast())
}

static HINTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// As the server's: a give that names the take's class is `give_hinted` (debug builds check the class against the run).
unsafe fn give(p: *mut u8, bytes: usize, align: usize, life: Option<Lifetime>) {
    // SAFETY: BlockSource's contract: a live block of `bytes` at `align` this source served, a some `life` its take's.
    unsafe {
        let p = NonNull::new_unchecked(p);
        match life {
            Some(l) => {
                HINTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                pagepool::give_hinted(p, bytes, align, Hint::new(class(l), Tag::MCX))
            }
            None => pagepool::give(p, bytes),
        }
    }
}

unsafe fn resize(p: *mut u8, old: usize, new: usize, align: usize) -> *mut u8 {
    // SAFETY: as `give`.
    unsafe { pagepool::resize(NonNull::new_unchecked(p), old, new, align) }.map_or(null_mut(), |b| b.as_ptr().cast())
}

static SOURCE: BlockSource = BlockSource { take, give, resize, usable: pagepool::usable };

fn chunk(n: usize) -> Layout {
    Layout::from_size_align(n, 8).unwrap()
}

/// The pool armed once (its first caller checks today's road before it), and this test alone on it.
fn serial() -> MutexGuard<'static, ()> {
    static ONCE: Once = Once::new();
    static TURN: Mutex<()> = Mutex::new(());
    ONCE.call_once(|| {
        let cfg = pagepool::Config::from_policy();
        assert!(pagepool::init(&cfg), "the pool reserves its zones");
        let ctx = MemoryContext::new("pool-before");
        let p = ctx.mcx().allocate(chunk(64 * KIB)).unwrap().cast::<u8>();
        assert!(!pagepool::owns(p.as_ptr()), "no source: today's road");
        // SAFETY: this context's chunk for its layout
        unsafe { ctx.mcx().deallocate(p, chunk(64 * KIB)) };
        drop(ctx);
        let (lo, len) = pagepool::range();
        // 1 KiB, not the policy's pool_block_min (64 KiB since E1b): the tests below take keeper-sized and 1 KiB-chunk
        // blocks from the pool to cover the whole road; the server's min is its own knob
        set_block_source(&SOURCE, lo, len, BLOCK_MIN, false);
    });
    TURN.lock().unwrap_or_else(|e| e.into_inner())
}

fn mcx_in_use() -> usize {
    pagepool::stats().in_use[Tag::MCX.0 as usize]
}

fn acct_bytes(a: Acct) -> i64 {
    memledger::fold();
    account_row(a).account_bytes
}

fn on(procno: u32, f: impl FnOnce(Acct) + Send + 'static) {
    std::thread::spawn(move || {
        memledger::bind_backend(procno, 9000 + procno);
        f(Acct::backend(procno));
        pagepool::flush_shelf();
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}

/// 40 KiB of 1 KiB chunks (the keeper and more blocks) and a 64 KiB dedicated chunk.
fn fill(ctx: &MemoryContext, out: &mut Vec<(NonNull<u8>, Layout)>) {
    for _ in 0..40 {
        out.push((ctx.mcx().allocate(chunk(KIB)).unwrap().cast(), chunk(KIB)));
    }
    out.push((ctx.mcx().allocate(chunk(64 * KIB)).unwrap().cast(), chunk(64 * KIB)));
}

fn free(ctx: &MemoryContext, out: &mut Vec<(NonNull<u8>, Layout)>) {
    for (p, l) in out.drain(..) {
        // SAFETY: each chunk is the one this context handed out for its layout, freed once
        unsafe { ctx.mcx().deallocate(p, l) };
    }
}

#[test]
fn create_reset_delete_cycles_give_every_block_back_to_the_pool() {
    let _turn = serial();
    on(4060, |a| {
        let (use0, acct0) = (mcx_in_use(), acct_bytes(a));
        let mut held = Vec::with_capacity(64);
        for _ in 0..20_000 {
            let mut ctx = MemoryContext::new("pool-cycle");
            fill(&ctx, &mut held);
            assert!(pagepool::owns(held[0].0.as_ptr()) && pagepool::owns(held[40].0.as_ptr()), "the pool's blocks");
            free(&ctx, &mut held);
            ctx.reset();
            fill(&ctx, &mut held);
            free(&ctx, &mut held);
            drop(ctx);
        }
        assert_eq!((mcx_in_use(), acct_bytes(a)), (use0, acct0), "every block went back, every byte off the account");
        assert!(HINTED.load(std::sync::atomic::Ordering::Relaxed) > 0, "one class per context: the gives named it");
    });
}

#[test]
fn the_owners_bytes_are_the_footprint() {
    let _turn = serial();
    on(4061, |a| {
        let ctx = MemoryContext::new("pool-owner");
        ctx.set_owner_session();
        let s = Owner::Session(a);
        let (n0, acct0, use0) = (owner_ctx(s).0, acct_bytes(a), mcx_in_use());
        let mut held = Vec::with_capacity(64);
        fill(&ctx, &mut held);
        held.push((ctx.mcx().allocate(chunk(MIB)).unwrap().cast(), chunk(MIB)));
        let footprint = ctx.mem_allocated() as i64;
        assert_eq!(owner_ctx(s).0 - n0, footprint, "the owner holds the footprint");
        assert_eq!(acct_bytes(a) - acct0, footprint, "and so does its account");
        assert_eq!(account_row(a).pagepool_bytes, footprint, "all of it the pool's");
        assert!(mcx_in_use() - use0 >= footprint as usize, "the pool's in-use count holds at least it");
        free(&ctx, &mut held);
        drop(ctx);
        assert_eq!((owner_ctx(s).0, acct_bytes(a), mcx_in_use()), (n0, acct0, use0));
    });
}

#[test]
fn a_send_root_dropped_on_another_thread_returns_its_owners_account() {
    let _turn = serial();
    on(4062, |a| {
        let s = Owner::Session(a);
        let (n0, acct0, use0) = (owner_ctx(s).0, acct_bytes(a), mcx_in_use());
        let mut r = SendRoot::new_owned("pool-send-root", s);
        let mut held = Vec::with_capacity(64);
        for _ in 0..40 {
            held.push(r.mcx().allocate(chunk(KIB)).unwrap().cast::<u8>());
        }
        let big = r.mcx().allocate(chunk(256 * KIB)).unwrap().cast::<u8>();
        assert!(pagepool::owns(big.as_ptr()));
        for p in held.drain(..) {
            // SAFETY: each is this root's chunk for its layout
            unsafe { r.mcx().deallocate(p, chunk(KIB)) };
        }
        // SAFETY: as above
        unsafe { r.mcx().deallocate(big, chunk(256 * KIB)) };
        let footprint = r.allocated() as i64;
        assert_eq!((owner_ctx(s).0 - n0, acct_bytes(a) - acct0), (footprint, footprint));
        std::thread::spawn(move || {
            drop(r);
            pagepool::flush_shelf();
        })
        .join()
        .unwrap();
        assert_eq!((owner_ctx(s).0, acct_bytes(a), mcx_in_use()), (n0, acct0, use0));
    });
}

#[test]
fn a_64_mib_block_is_a_lazy_span_that_grows() {
    let _turn = serial();
    on(4063, |a| {
        let ctx = MemoryContext::new("pool-grow");
        ctx.set_owner_session();
        let s = Owner::Session(a);
        let (n0, lazy0) = (owner_ctx(s).0, pagepool::stats().lazy_committed);
        let (l64, l128) = (chunk(64 * MIB), chunk(128 * MIB));
        let p = ctx.mcx().allocate(l64).unwrap().cast::<u8>();
        assert!(pagepool::owns(p.as_ptr()), "the pool's");
        assert!(pagepool::stats().lazy_committed >= lazy0 + 64 * MIB, "a lazy span: {:?}", pagepool::stats());
        // SAFETY: inside the 64 MiB block
        unsafe {
            p.as_ptr().write(7);
            p.as_ptr().add(64 * MIB - 1).write(9);
        }
        // SAFETY: `p` is this context's block for `l64`; l128 is larger
        let q = unsafe { ctx.mcx().grow(p, l64, l128) }.unwrap().cast::<u8>();
        assert!(pagepool::owns(q.as_ptr()));
        // SAFETY: the grown block holds the old bytes
        assert_eq!(unsafe { (q.as_ptr().read(), q.as_ptr().add(64 * MIB - 1).read()) }, (7, 9));
        assert_eq!(owner_ctx(s).0 - n0, ctx.mem_allocated() as i64);
        // SAFETY: `q` is this context's block for `l128`
        unsafe { ctx.mcx().deallocate(q, l128) };
        drop(ctx);
        assert_eq!(owner_ctx(s).0, n0);
    });
}
