//! The page pool as a context's block source while `PoolAlloc` is the global allocator (the server's pagepool-global
//! build with PGRUST_POOL_MCX=1): the global allocator's blocks share the source's range, so a context's smaller blocks
//! (the global allocator's) must go back to it, and the source gets back exactly the blocks it took (D4720: they reached
//! the source's give, which shelved a small-tier address as a 64 KiB block, and the server faulted at start).
#![cfg(all(feature = "std", target_os = "linux"))]

use std::alloc::Layout;
use std::ptr::{null_mut, NonNull};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use mcx::heap::{set_block_owner, set_block_source, BlockSource, Lifetime};
use mcx::{Allocator, MemoryContext};
use pagepool::{Class, Hint, PoolAlloc, Tag, Zero};

#[global_allocator]
static A: PoolAlloc = PoolAlloc;

const KIB: usize = 1 << 10;
static TAKES: AtomicUsize = AtomicUsize::new(0);
static GIVES: AtomicUsize = AtomicUsize::new(0);

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
    let p: *mut u8 = pagepool::take(bytes, align, hint).map_or(null_mut(), |b| b.as_ptr().cast());
    TAKES.fetch_add(!p.is_null() as usize, Relaxed);
    p
}

unsafe fn give(p: *mut u8, bytes: usize, align: usize, life: Option<Lifetime>) {
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
    // SAFETY: as `give`.
    unsafe { pagepool::resize(NonNull::new_unchecked(p), old, new, align) }.map_or(null_mut(), |b| b.as_ptr().cast())
}

static SOURCE: BlockSource = BlockSource { take, give, resize, usable: pagepool::usable };

fn chunk(n: usize) -> Layout {
    Layout::from_size_align(n, 8).unwrap()
}

#[test]
fn the_source_gets_back_its_own_blocks_and_none_of_the_global_allocators() {
    let probe = vec![1u8; 8 * KIB];
    assert!(pagepool::global_on() && pagepool::owns(probe.as_ptr()), "the global allocator serves from the pool's range");
    let (lo, len) = pagepool::range();
    set_block_owner(pagepool::taken);
    set_block_source(&SOURCE, lo, len, 64 * KIB, true);
    std::thread::spawn(|| {
        memledger::bind_backend(4070, 9070);
        let use0 = pagepool::stats().in_use[Tag::MCX.0 as usize];
        let mut held = Vec::with_capacity(512);
        for _ in 0..200 {
            let mut ctx = MemoryContext::new("pool-global");
            for round in 0..2 {
                for _ in 0..300 {
                    held.push((ctx.mcx().allocate(chunk(KIB)).unwrap().cast::<u8>(), chunk(KIB)));
                }
                held.push((ctx.mcx().allocate(chunk(256 * KIB)).unwrap().cast::<u8>(), chunk(256 * KIB)));
                for (p, l) in held.drain(..) {
                    // SAFETY: each chunk is this context's for its layout, freed once
                    unsafe { ctx.mcx().deallocate(p, l) };
                }
                if round == 0 {
                    ctx.reset();
                }
            }
            drop(ctx);
        }
        pagepool::flush_shelf();
        assert!(TAKES.load(Relaxed) > 0, "the source served the bigger blocks");
        assert_eq!(GIVES.load(Relaxed), TAKES.load(Relaxed), "the source got back what it took, nothing of the global allocator's");
        assert_eq!(pagepool::stats().in_use[Tag::MCX.0 as usize], use0, "every taken block went back");
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
    drop(probe);
}
