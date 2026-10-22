//! A context's block source with no parked keepers (`BlockSource::keepers` false, PGRUST_POOL_KEEPERS=0): every block of
//! `min` bytes and up is the source's, a reset keeps a pooled keeper on its owner, a drop gives it back instead of
//! parking it, and a SendRoot dropped on another thread leaves its owner's account where it started.
#![cfg(all(feature = "std", unix))]

use std::alloc::Layout;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard, Once};

use mcx::heap::{set_block_source, BlockSource, Lifetime};
use mcx::{Allocator, MemoryContext, SendRoot};
use memledger::charge::owner_ctx;
use memledger::{Acct, Owner};

const KIB: usize = 1 << 10;
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
        set_block_source(&SOURCE, p as usize, ZONE, KIB, false);
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

fn acct_bytes(a: Acct) -> i64 {
    memledger::fold();
    memledger::account_row(a).account_bytes
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
fn a_reset_keeps_a_pooled_keeper_on_its_owner_and_a_drop_gives_it_back() {
    let _turn = serial();
    on(4050, |a| {
        let s = Owner::Session(a);
        let mut ctx = MemoryContext::new("pool-nokeep");
        ctx.set_owner_session();
        let (n0, acct0, live0) = (owner_ctx(s).0, acct_bytes(a), LIVE.load(Relaxed));
        let (takes0, hinted0) = (TAKES.load(Relaxed), HINTED.load(Relaxed));
        let mut held = Vec::with_capacity(64);
        fill(&ctx, &mut held);
        assert!(pooled(held[0].0), "the keeper is the source's");
        free(&ctx, &mut held);
        ctx.reset();
        assert_eq!(ctx.mem_allocated(), 8 * KIB);
        assert_eq!((owner_ctx(s).0 - n0, acct_bytes(a) - acct0, LIVE.load(Relaxed) - live0), (8 * KIB as i64, 8 * KIB as i64, 8 * KIB as i64));
        drop(ctx);
        assert_eq!((owner_ctx(s).0, acct_bytes(a), LIVE.load(Relaxed)), (n0, acct0, live0), "given back, not parked");
        // pagepool P2 (E3b): every block a session's, keeper included: each give named the class
        assert_eq!(HINTED.load(Relaxed) - hinted0, TAKES.load(Relaxed) - takes0);
    });
}

#[test]
fn a_send_root_dropped_on_another_thread_returns_its_owners_account() {
    let _turn = serial();
    on(4051, |a| {
        let s = Owner::Session(a);
        let (n0, acct0, live0) = (owner_ctx(s).0, acct_bytes(a), LIVE.load(Relaxed));
        let (takes0, hinted0) = (TAKES.load(Relaxed), HINTED.load(Relaxed));
        let mut r = SendRoot::new_owned("pool-send-root", s);
        let mut held = Vec::with_capacity(64);
        for _ in 0..40 {
            held.push(r.mcx().allocate(chunk(KIB)).unwrap().cast::<u8>());
        }
        let big = r.mcx().allocate(chunk(256 * KIB)).unwrap().cast::<u8>();
        for p in held.drain(..) {
            // SAFETY: each is this root's chunk for its layout
            unsafe { r.mcx().deallocate(p, chunk(KIB)) };
        }
        // SAFETY: as above
        unsafe { r.mcx().deallocate(big, chunk(256 * KIB)) };
        let footprint = r.allocated() as i64;
        assert_eq!((owner_ctx(s).0 - n0, acct_bytes(a) - acct0), (footprint, footprint));
        std::thread::spawn(move || drop(r)).join().unwrap();
        assert_eq!((owner_ctx(s).0, acct_bytes(a), LIVE.load(Relaxed)), (n0, acct0, live0));
        // pagepool P2 (E3b): the drop on another thread names the class too (the AllocSet's own copy, no Led there)
        assert_eq!(HINTED.load(Relaxed) - hinted0, TAKES.load(Relaxed) - takes0);
    });
}
