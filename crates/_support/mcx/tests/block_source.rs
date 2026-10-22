//! A context's block source (the page pool's seam), over a stand-in source: blocks of `min` bytes and up come from it and
//! all go back, a keeper parks off it, a block it does not serve takes today's road, and the source's bytes post to the
//! context's owner exactly.
#![cfg(all(feature = "std", unix))]

use std::alloc::Layout;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard, Once};

use mcx::heap::{set_block_source, BlockSource, Lifetime};
use mcx::{Allocator, MemoryContext};
use memledger::charge::{owner_ctx, owner_row};
use memledger::{Acct, Owner};

const KIB: usize = 1 << 10;
const MIB: usize = 1 << 20;
const ZONE: usize = 4 << 30;

static BASE: AtomicUsize = AtomicUsize::new(0);
static NEXT: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicI64 = AtomicI64::new(0);
static TAKES: AtomicUsize = AtomicUsize::new(0);
static LIVES: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];
static FREE: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());
// live blocks and the class each was taken in; gives that named a class (pagepool P2 E3b)
static TAKEN: Mutex<Vec<(usize, Lifetime)>> = Mutex::new(Vec::new());
static HINTED: AtomicUsize = AtomicUsize::new(0);

unsafe fn take(bytes: usize, align: usize, life: Lifetime, _zeroed: bool) -> *mut u8 {
    if align > 4096 {
        return std::ptr::null_mut();
    }
    TAKES.fetch_add(1, Relaxed);
    LIVES[life as usize].fetch_add(1, Relaxed);
    LIVE.fetch_add(bytes as i64, Relaxed);
    let mut free = FREE.lock().unwrap();
    let p = if let Some(i) = free.iter().position(|&(_, b)| b == bytes) {
        free.swap_remove(i).0
    } else {
        drop(free);
        let size = (bytes + 4095) & !4095;
        let off = NEXT.fetch_add(size, Relaxed);
        assert!(off + size <= ZONE, "the stand-in zone is full");
        BASE.load(Relaxed) + off
    };
    TAKEN.lock().unwrap().push((p, life));
    p as *mut u8
}

/// A give that names a class names its take's.
unsafe fn give(p: *mut u8, bytes: usize, _align: usize, life: Option<Lifetime>) {
    let mut taken = TAKEN.lock().unwrap();
    let i = taken.iter().position(|&(a, _)| a == p as usize).expect("a give of a block the stand-in served");
    let (_, took) = taken.swap_remove(i);
    drop(taken);
    if let Some(l) = life {
        assert_eq!(l, took, "a hinted give names the class its block was taken in");
        HINTED.fetch_add(1, Relaxed);
    }
    LIVE.fetch_sub(bytes as i64, Relaxed);
    FREE.lock().unwrap().push((p as usize, bytes));
}

unsafe fn resize(_: *mut u8, _: usize, _: usize, _: usize) -> *mut u8 {
    std::ptr::null_mut()
}

fn usable(bytes: usize, _: usize) -> usize {
    bytes
}

static SOURCE: BlockSource = BlockSource { take, give, resize, usable };

/// The source installed once, and this test alone on it (the stand-in's counters are process-wide).
fn serial() -> MutexGuard<'static, ()> {
    static ONCE: Once = Once::new();
    static TURN: Mutex<()> = Mutex::new(());
    ONCE.call_once(|| {
        // SAFETY: a fresh private anonymous reservation that nothing else names.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                ZONE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        assert_ne!(p, libc::MAP_FAILED);
        BASE.store(p as usize, Relaxed);
        set_block_source(&SOURCE, p as usize, ZONE, KIB, true);
    });
    TURN.lock().unwrap_or_else(|e| e.into_inner())
}

fn pooled(p: NonNull<u8>) -> bool {
    (p.as_ptr() as usize).wrapping_sub(BASE.load(Relaxed)) < ZONE
}

fn chunk(n: usize) -> Layout {
    Layout::from_size_align(n, 8).unwrap()
}

fn on(procno: u32, f: impl FnOnce(Acct) + Send + 'static) {
    std::thread::spawn(move || {
        memledger::bind_backend(procno, 9000 + procno);
        f(Acct::backend(procno));
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}

/// 40 KiB of 1 KiB chunks (the keeper and two more blocks) and a 64 KiB dedicated chunk.
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
fn create_reset_delete_cycles_give_every_block_back() {
    let _turn = serial();
    on(4040, |_| {
        let (live0, takes0) = (LIVE.load(Relaxed), TAKES.load(Relaxed));
        let mut held = Vec::with_capacity(64);
        for _ in 0..200_000 {
            let mut ctx = MemoryContext::new("pool-cycle");
            fill(&ctx, &mut held);
            free(&ctx, &mut held);
            ctx.reset();
            fill(&ctx, &mut held);
            free(&ctx, &mut held);
            drop(ctx);
        }
        assert!(TAKES.load(Relaxed) >= takes0 + 200_000 * 3, "the non-keeper blocks came from the source");
        assert_eq!(LIVE.load(Relaxed), live0, "every block went back");
    });
}

#[test]
fn the_owners_bytes_are_the_footprint_and_a_pooled_span_is_no_mapping() {
    let _turn = serial();
    on(4041, |a| {
        let ctx = MemoryContext::new("pool-owner");
        ctx.set_owner_session();
        let s = Owner::Session(a);
        let (n0, c0, live0) = (owner_ctx(s).0, owner_row(s).committed, LIVE.load(Relaxed));
        let mut held = Vec::with_capacity(64);
        fill(&ctx, &mut held);
        let big = chunk(64 * MIB);
        let p = ctx.mcx().allocate(big).unwrap().cast::<u8>();
        assert!(pooled(p), "a block past the map threshold is the source's too");
        assert_eq!(owner_ctx(s).0 - n0, ctx.mem_allocated() as i64, "the owner holds the footprint");
        assert_eq!(owner_row(s).committed, c0, "no door-2 mapping for a source's block");
        assert_eq!(LIVE.load(Relaxed) - live0, (ctx.mem_allocated() - 8 * KIB) as i64, "all but the keeper");
        // SAFETY: `p` is this context's chunk for `big`
        unsafe { ctx.mcx().deallocate(p, big) };
        free(&ctx, &mut held);
        drop(ctx);
        assert_eq!((owner_ctx(s).0, LIVE.load(Relaxed)), (n0, live0));
    });
}

#[test]
fn a_keeper_parks_off_the_source_and_an_unserved_block_takes_todays_road() {
    let _turn = serial();
    on(4042, |_| {
        let ctx = MemoryContext::new("pool-keeper");
        let first = ctx.mcx().allocate(chunk(64)).unwrap().cast::<u8>();
        assert!(!pooled(first), "the keeper is today's");
        let wide = Layout::from_size_align(64 * KIB, 8192).unwrap();
        let live0 = LIVE.load(Relaxed);
        let w = ctx.mcx().allocate(wide).unwrap().cast::<u8>();
        assert!(!pooled(w) && LIVE.load(Relaxed) == live0, "not served: today's road");
        // SAFETY: each is this context's chunk for its layout
        unsafe {
            ctx.mcx().deallocate(w, wide);
            ctx.mcx().deallocate(first, chunk(64));
        }
    });
}

#[test]
fn a_64_mib_block_grows_and_shrinks_through_the_source_with_its_bytes() {
    let _turn = serial();
    on(4043, |a| {
        let ctx = MemoryContext::new("pool-grow");
        ctx.set_owner_session();
        let s = Owner::Session(a);
        let (n0, live0) = (owner_ctx(s).0, LIVE.load(Relaxed));
        let (l64, l128, l32) = (chunk(64 * MIB), chunk(128 * MIB), chunk(32 * MIB));
        let p = ctx.mcx().allocate(l64).unwrap().cast::<u8>();
        // SAFETY: inside the 64 MiB block
        unsafe {
            p.as_ptr().write(7);
            p.as_ptr().add(64 * MIB - 1).write(9);
        }
        // SAFETY: `p` is this context's block for `l64`; l128 is larger
        let q = unsafe { ctx.mcx().grow(p, l64, l128) }.unwrap().cast::<u8>();
        assert!(pooled(q));
        // SAFETY: the grown block holds the old bytes
        assert_eq!(unsafe { (q.as_ptr().read(), q.as_ptr().add(64 * MIB - 1).read()) }, (7, 9));
        assert_eq!(owner_ctx(s).0 - n0, ctx.mem_allocated() as i64);
        assert_eq!(LIVE.load(Relaxed) - live0, 128 * MIB as i64);
        // SAFETY: `q` is this context's block for `l128`; l32 is smaller
        let r = unsafe { ctx.mcx().shrink(q, l128, l32) }.unwrap().cast::<u8>();
        assert_eq!(unsafe { r.as_ptr().read() }, 7);
        assert_eq!(LIVE.load(Relaxed) - live0, 32 * MIB as i64);
        // SAFETY: `r` is this context's block for `l32`
        unsafe { ctx.mcx().deallocate(r, l32) };
        drop(ctx);
        assert_eq!((owner_ctx(s).0, LIVE.load(Relaxed)), (n0, live0));
    });
}

#[test]
fn a_contexts_lifetime_is_its_owners() {
    let _turn = serial();
    on(4044, |_| {
        memledger::statement_begin();
        let task0 = LIVES[Lifetime::Task as usize].load(Relaxed);
        let ctx = MemoryContext::new("pool-life");
        let p = ctx.mcx().allocate(chunk(64 * KIB)).unwrap();
        assert_eq!(LIVES[Lifetime::Task as usize].load(Relaxed), task0 + 1, "an unnamed root in a statement: a task's");
        ctx.set_owner_session();
        let sess0 = LIVES[Lifetime::Session as usize].load(Relaxed);
        let child = ctx.new_child("pool-life-child");
        let q = child.mcx().allocate(chunk(64 * KIB)).unwrap();
        assert_eq!(LIVES[Lifetime::Session as usize].load(Relaxed), sess0 + 1, "re-owned to its session, and its child");
        // SAFETY: each is its context's chunk for its layout
        unsafe {
            child.mcx().deallocate(q.cast(), chunk(64 * KIB));
            ctx.mcx().deallocate(p.cast(), chunk(64 * KIB));
        }
        drop(child);
        drop(ctx);
        let _ = memledger::statement_end();
    });
}

/// pagepool P2 (E3b): while one class holds a context's pooled blocks, every give of them names it (a free, a reset, a
/// drop); once the context took blocks in two classes, its gives name none.
#[test]
fn a_give_names_the_class_while_one_class_holds_the_contexts_blocks() {
    let _turn = serial();
    on(4045, |_| {
        memledger::statement_begin();
        let (live0, takes0, hinted0) = (LIVE.load(Relaxed), TAKES.load(Relaxed), HINTED.load(Relaxed));
        let mut ctx = MemoryContext::new("pool-hint");
        let mut held = Vec::with_capacity(64);
        fill(&ctx, &mut held);
        free(&ctx, &mut held);
        ctx.reset();
        fill(&ctx, &mut held);
        held.clear();
        drop(ctx);
        let takes = TAKES.load(Relaxed) - takes0;
        assert!(takes >= 6, "blocks and dedicated chunks from the source: {takes}");
        assert_eq!(HINTED.load(Relaxed) - hinted0, takes, "one class (a task's): every give named it");
        assert_eq!(LIVE.load(Relaxed), live0);

        let (takes0, hinted0) = (TAKES.load(Relaxed), HINTED.load(Relaxed));
        let ctx = MemoryContext::new("pool-mixed");
        let p = ctx.mcx().allocate(chunk(64 * KIB)).unwrap();
        ctx.set_owner_session();
        let q = ctx.mcx().allocate(chunk(64 * KIB)).unwrap();
        assert_eq!(TAKES.load(Relaxed) - takes0, 2);
        // SAFETY: `p` is this context's chunk for its layout
        unsafe { ctx.mcx().deallocate(p.cast(), chunk(64 * KIB)) };
        let _ = q;
        drop(ctx);
        assert_eq!(HINTED.load(Relaxed), hinted0, "a task's block and a session's: no give names a class");
        assert_eq!(LIVE.load(Relaxed), live0);
        let _ = memledger::statement_end();
    });
}
