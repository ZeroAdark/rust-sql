//! PGRUST_GRAVITY_POOLED_BIT with the page pool as the block source and `PoolAlloc` as the global allocator, which
//! serves from the same range (D4720): a context's blocks and dedicated chunks record the allocator that served them,
//! a refused take records the global allocator's, a resize records its new chunk's across the source's min and between
//! the two allocators, and every block goes back to the allocator that served it. Debug builds check each recorded
//! answer against the source's lookup where it replaces one. A context reset with live chunks is a wholesale one (as a
//! per-row context).
#![cfg(all(feature = "std", target_os = "linux"))]

use std::alloc::Layout;
use std::ptr::{null_mut, NonNull};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard, Once};

use mcx::heap::{set_block_owner, set_block_source, set_pooled_bit, BlockSource, Lifetime};
use mcx::{Allocator, MemoryContext};
use pagepool::{Class, Hint, PoolAlloc, Tag, Zero};

#[global_allocator]
static A: PoolAlloc = PoolAlloc;

const KIB: usize = 1 << 10;
const MIB: usize = 1 << 20;
static TAKES: AtomicUsize = AtomicUsize::new(0);
static GIVES: AtomicUsize = AtomicUsize::new(0);
static REFUSED: AtomicUsize = AtomicUsize::new(0);
/// The source refuses every take and resize while set (a full pool).
static REFUSE: AtomicBool = AtomicBool::new(false);

fn class(life: Lifetime) -> Class {
    match life {
        Lifetime::Task => Class::Task,
        Lifetime::Session => Class::Session,
        Lifetime::Cache => Class::Cache,
        Lifetime::Process => Class::Process,
    }
}

unsafe fn take(bytes: usize, align: usize, life: Lifetime, zeroed: bool) -> *mut u8 {
    if REFUSE.load(Relaxed) {
        REFUSED.fetch_add(1, Relaxed);
        return null_mut();
    }
    let hint = Hint::new(class(life), Tag::MCX).zero(if zeroed { Zero::Dense } else { Zero::No });
    let p: *mut u8 = pagepool::take(bytes, align, hint).map_or(null_mut(), |b| b.as_ptr().cast());
    TAKES.fetch_add(!p.is_null() as usize, Relaxed);
    p
}

unsafe fn give(p: *mut u8, bytes: usize, align: usize, life: Option<Lifetime>) {
    assert!(pagepool::taken(p), "the source gets back only a block it served");
    GIVES.fetch_add(1, Relaxed);
    // SAFETY: BlockSource's contract: a live block of `bytes` at `align` this source served, a some `life` its take's.
    unsafe {
        let p = NonNull::new_unchecked(p);
        match life {
            Some(l) => pagepool::give_hinted(p, bytes, align, Hint::new(class(l), Tag::MCX)),
            None => pagepool::give(p, bytes),
        }
    }
}

unsafe fn resize(p: *mut u8, old: usize, new: usize, align: usize) -> *mut u8 {
    if REFUSE.load(Relaxed) {
        return null_mut();
    }
    // SAFETY: as `give`.
    unsafe { pagepool::resize(NonNull::new_unchecked(p), old, new, align) }.map_or(null_mut(), |b| b.as_ptr().cast())
}

static SOURCE: BlockSource = BlockSource { take, give, resize, usable: pagepool::usable };

fn chunk(n: usize) -> Layout {
    Layout::from_size_align(n, 8).unwrap()
}

/// The source installed once at the server's min (64 KiB), and this test alone on it (the counters and REFUSE are
/// process-wide).
fn serial() -> MutexGuard<'static, ()> {
    static ONCE: Once = Once::new();
    static TURN: Mutex<()> = Mutex::new(());
    ONCE.call_once(|| {
        let probe = vec![1u8; 8 * KIB];
        assert!(pagepool::global_on() && pagepool::owns(probe.as_ptr()), "the global allocator serves from the pool's range");
        drop(probe);
        let (lo, len) = pagepool::range();
        set_block_owner(pagepool::taken);
        set_block_source(&SOURCE, lo, len, 64 * KIB, true);
    });
    TURN.lock().unwrap_or_else(|e| e.into_inner())
}

/// `f` on a bound backend thread, then every block the source served is back.
fn on(procno: u32, f: impl FnOnce() + Send + 'static) {
    std::thread::spawn(move || {
        memledger::bind_backend(procno, 9000 + procno);
        let (takes0, gives0) = (TAKES.load(Relaxed), GIVES.load(Relaxed));
        let use0 = pagepool::stats().in_use[Tag::MCX.0 as usize];
        f();
        REFUSE.store(false, Relaxed);
        pagepool::flush_shelf();
        assert_eq!(GIVES.load(Relaxed) - gives0, TAKES.load(Relaxed) - takes0, "the source got back what it took, nothing else");
        assert_eq!(pagepool::stats().in_use[Tag::MCX.0 as usize], use0, "every taken block went back");
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}

/// The global allocator's block in the source's range: the address alone names the source.
fn global_in_range(p: NonNull<u8>) -> bool {
    pagepool::owns(p.as_ptr()) && !pagepool::taken(p.as_ptr())
}

#[test]
fn a_refused_take_records_the_global_allocators_block_and_frees_it_there() {
    let _turn = serial();
    on(4080, || {
        let big = chunk(256 * KIB);
        for round in 0..20 {
            let mut ctx = MemoryContext::new_wholesale("bit-refused");
            let p = ctx.mcx().allocate(big).unwrap().cast::<u8>();
            assert_eq!(ctx.dedicated_served(p), Some(true), "the source's take served it");
            let refused0 = REFUSED.load(Relaxed);
            REFUSE.store(true, Relaxed);
            let q = ctx.mcx().allocate(big).unwrap().cast::<u8>();
            // blocks of 8 KiB doubling: the ones of 64 KiB and up are asked of the source and refused
            let mut held = Vec::with_capacity(512);
            for _ in 0..400 {
                held.push(ctx.mcx().allocate(chunk(KIB)).unwrap());
            }
            REFUSE.store(false, Relaxed);
            assert!(REFUSED.load(Relaxed) >= refused0 + 3, "the dedicated chunk and the big blocks were refused");
            assert!(global_in_range(q), "the global allocator served the refused chunk from the source's range");
            assert_eq!(ctx.dedicated_served(q), Some(false), "a refused take records the global allocator");
            // and the blocks after: the source's again
            for _ in 0..600 {
                held.push(ctx.mcx().allocate(chunk(KIB)).unwrap());
            }
            let r = ctx.mcx().allocate(big).unwrap().cast::<u8>();
            assert_eq!(ctx.dedicated_served(r), Some(true));
            if round % 2 == 0 {
                // SAFETY: `q` is this context's chunk for `big`, freed once
                unsafe { ctx.mcx().deallocate(q, big) };
            }
            held.clear();
            ctx.reset();
            if round % 3 == 0 {
                for _ in 0..1000 {
                    let _ = ctx.mcx().allocate(chunk(KIB)).unwrap();
                }
            }
            drop(ctx);
        }
    });
}

#[test]
fn a_dedicated_chunk_records_its_server_across_resizes() {
    let _turn = serial();
    on(4081, || {
        let ctx = MemoryContext::new("bit-resize");
        let (l32, l128, l256, l512, l40m) = (chunk(32 * KIB), chunk(128 * KIB), chunk(256 * KIB), chunk(512 * KIB), chunk(40 * MIB));
        let p = ctx.mcx().allocate(l128).unwrap().cast::<u8>();
        // SAFETY: inside the chunk
        unsafe { p.as_ptr().write(7) };
        assert_eq!(ctx.dedicated_served(p), Some(true));
        // SAFETY: each resize takes this context's live chunk at its layout, the next one names the chunk it returned
        unsafe {
            let q = ctx.mcx().shrink(p, l128, l32).unwrap().cast::<u8>();
            assert!(pagepool::taken(q.as_ptr()) && ctx.dedicated_served(q) == Some(true), "under the source's min, still its");
            let r = ctx.mcx().grow(q, l32, l128).unwrap().cast::<u8>();
            assert_eq!(ctx.dedicated_served(r), Some(true));
            REFUSE.store(true, Relaxed);
            let s = ctx.mcx().grow(r, l128, l512).unwrap().cast::<u8>();
            REFUSE.store(false, Relaxed);
            assert!(global_in_range(s) && ctx.dedicated_served(s) == Some(false), "refused: moved to the global allocator");
            let t = ctx.mcx().shrink(s, l512, l32).unwrap().cast::<u8>();
            assert_eq!(ctx.dedicated_served(t), Some(false));
            let u = ctx.mcx().grow(t, l32, l256).unwrap().cast::<u8>();
            assert!(!pagepool::taken(u.as_ptr()) && ctx.dedicated_served(u) == Some(false), "a resize keeps the global allocator's");
            let v = ctx.mcx().grow(u, l256, l40m).unwrap().cast::<u8>();
            assert!(!pagepool::owns(v.as_ptr()) && ctx.dedicated_served(v) == Some(false), "its own mapping");
            assert_eq!(v.as_ptr().read(), 7);
            let w = ctx.mcx().shrink(v, l40m, l128).unwrap().cast::<u8>();
            assert_eq!(ctx.dedicated_served(w), Some(false));
            ctx.mcx().deallocate(w, l128);
            let x = ctx.mcx().allocate(l512).unwrap().cast::<u8>();
            let y = ctx.mcx().grow(x, l512, l40m).unwrap().cast::<u8>();
            assert!(pagepool::taken(y.as_ptr()) && ctx.dedicated_served(y) == Some(true), "the source's past the map threshold");
            let z = ctx.mcx().shrink(y, l40m, l32).unwrap().cast::<u8>();
            assert_eq!(ctx.dedicated_served(z), Some(pagepool::taken(z.as_ptr())));
        }
        drop(ctx);
    });
}

#[test]
fn a_bump_arenas_blocks_and_oversize_chunks_record_their_server() {
    let _turn = serial();
    on(4082, || {
        let mut ctx = MemoryContext::new_bump("bit-bump");
        let (l1m, l4m, l8m) = (chunk(MIB), chunk(4 * MIB), chunk(8 * MIB));
        for _ in 0..10 {
            let p = ctx.mcx().allocate(l1m).unwrap().cast::<u8>();
            assert_eq!(ctx.dedicated_served(p), Some(true));
            REFUSE.store(true, Relaxed);
            // SAFETY: this context's chunk at its layout, then the grown one at its
            let q = unsafe { ctx.mcx().grow(p, l1m, l4m) }.unwrap().cast::<u8>();
            for _ in 0..300 {
                let _ = ctx.mcx().allocate(chunk(KIB)).unwrap();
            }
            REFUSE.store(false, Relaxed);
            assert!(global_in_range(q) && ctx.dedicated_served(q) == Some(false));
            // SAFETY: as above
            let r = unsafe { ctx.mcx().grow(q, l4m, l8m) }.unwrap().cast::<u8>();
            assert_eq!(ctx.dedicated_served(r), Some(false));
            for _ in 0..600 {
                let _ = ctx.mcx().allocate(chunk(KIB)).unwrap();
            }
            ctx.reset();
        }
        drop(ctx);
    });
}

#[test]
fn with_the_bit_off_nothing_is_recorded_and_every_block_goes_home() {
    let _turn = serial();
    on(4083, || {
        set_pooled_bit(false);
        let big = chunk(256 * KIB);
        let mut ctx = MemoryContext::new_wholesale("bit-off");
        let p = ctx.mcx().allocate(big).unwrap().cast::<u8>();
        REFUSE.store(true, Relaxed);
        let q = ctx.mcx().allocate(big).unwrap().cast::<u8>();
        REFUSE.store(false, Relaxed);
        assert_eq!((ctx.dedicated_served(p), ctx.dedicated_served(q)), (None, None), "off: not read");
        assert!(pagepool::taken(p.as_ptr()) && global_in_range(q));
        for _ in 0..1000 {
            let _ = ctx.mcx().allocate(chunk(KIB)).unwrap();
        }
        // a chunk made while on records its server, which is not read while off
        set_pooled_bit(true);
        let r = ctx.mcx().allocate(big).unwrap().cast::<u8>();
        assert_eq!(ctx.dedicated_served(r), Some(true));
        set_pooled_bit(false);
        assert_eq!(ctx.dedicated_served(r), None);
        ctx.reset();
        drop(ctx);
        set_pooled_bit(true);
    });
}
