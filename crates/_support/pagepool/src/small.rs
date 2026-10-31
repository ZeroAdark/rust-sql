//! The small-object tier (8 B-1 KiB), mimalloc's page design on 64 KiB pool blocks (class General, tag SMALL): a local
//! free-list pop / push per thread and class, one CAS for a free from another thread. Emptied runs go back to the pool
//! and a dead thread's runs are adopted, so a new thread starts on warm pages (mimalloc's start on fresh or purged ones).

use crate::central::{Atom64, Nodes, Stack, NONE};
use crate::run::RUN;
use crate::{api, Class, Hint, Tag};
use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicIsize, AtomicPtr, AtomicU32, AtomicU64, Ordering::*};
use std::alloc::System;

pub(crate) const SMALL_MAX: usize = 1024;
/// The most alignment the tier serves: blocks start 128 B into a 64 KiB-aligned run, at multiples of their size.
pub(crate) const ALIGN_MAX: usize = 128;
const HDR: usize = 128;
pub(crate) const NCLS: usize = 24;
/// mimalloc's bins up to 1 KiB: 8 B steps to 64 B, then four per doubling.
pub(crate) const SIZES: [u32; NCLS] = [8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 640, 768, 896, 1024];

const CLASS_OF: [u8; SMALL_MAX / 8 + 1] = {
    let mut t = [0u8; SMALL_MAX / 8 + 1];
    let (mut i, mut c) = (0, 0);
    while i < t.len() {
        while (SIZES[c] as usize) < i * 8 {
            c += 1;
        }
        t[i] = c as u8;
        i += 1;
    }
    t
};

const RUN_HINT: Hint = Hint::new(Class::General, Tag::SMALL);
const CACHE_HINT: Hint = Hint::new(Class::General, Tag::CACHE);
const GENERAL: u8 = 0;
const CACHE: u8 = 1;
use crate::run::RUN_BUCKET;

const CUR: u8 = 0;
const PARTIAL: u8 = 1;
const FULL: u8 = 2;
const SPARE: u8 = 3;
const FLOAT: u8 = 4;
const NEVER: u32 = u32::MAX;
/// Heap states: no run yet; owning runs; past its thread's exit destructor (owns none again).
const NEW: u8 = 0;
const LIVE: u8 = 1;
const DEAD: u8 = 2;
const SCAN: usize = 4;
const ADOPT: usize = 4;
/// A fresh (zero, maybe unfaulted) run's free list grows by about this many bytes at a time.
const EXTEND: u32 = 4096;
/// Remote frees wait per thread (direct-mapped by run, flushed at exit and trim) and go as one chain: one CAS per RBATCH.
const RCACHE: usize = 16;
const RBATCH: u32 = 16;

/// A run's header (first 128 B): line 0 the owner's, line 1 other threads'. `alink` sits on the pool link's `next_batch`.
#[repr(C, align(64))]
pub(crate) struct Run {
    free: usize,
    alink: AtomicU32,
    used: u32,
    watch: u32,
    bump: u32,
    end: u32,
    size: u32,
    cap: u32,
    cls: u8,
    state: u8,
    lazy: bool,
    kind: u8,
    prev: usize,
    next: usize,
    _p1: usize,
    owner: AtomicU64,
    remote: AtomicU64,
    _p2: [u64; 6],
}

const _: () = assert!(core::mem::size_of::<Run>() == HDR && core::mem::offset_of!(Run, owner) == 64);
const _: () = assert!(core::mem::offset_of!(Run, alink) == 8);

#[repr(C, align(64))]
struct Empty([u64; 16]);
static EMPTY: Empty = Empty([0; 16]);

#[inline(always)]
fn empty() -> *mut Run {
    &raw const EMPTY as *mut Run
}

struct Heap {
    state: Cell<u8>,
    cur: [Cell<*mut Run>; NCLS],
    ccur: [Cell<*mut Run>; NCLS],
    /// A current cache run or a pending remote free since the last idle point.
    busy: Cell<bool>,
    partial: [Cell<*mut Run>; NCLS],
    full: [Cell<*mut Run>; NCLS],
    spare: Cell<*mut Run>,
    rc: [Pending; RCACHE],
    /// Class bytes this thread took less those it freed (any run's): written by this thread only, summed by `live_bytes`.
    live: AtomicIsize,
    /// Card 10: where an idle tail cut that stopped at its time budget goes on: (class, runs of its walk passed), the
    /// passes run again for tails left to the purger's split, and whether the pass the budget stopped left any before
    /// the stop (its resume decides `again` for the whole pass).
    tail_at: Cell<(u8, u32)>,
    tail_again: Cell<u8>,
    tail_deferred: Cell<bool>,
    rnext: Cell<*const Heap>,
    rprev: Cell<*const Heap>,
}

struct Pending {
    run: Cell<usize>,
    head: Cell<usize>,
    tail: Cell<usize>,
    n: Cell<u32>,
}

thread_local! {
    static HEAP: Heap = const {
        Heap {
            state: Cell::new(NEW),
            cur: [const { Cell::new(&raw const EMPTY as *mut Run) }; NCLS],
            ccur: [const { Cell::new(&raw const EMPTY as *mut Run) }; NCLS],
            busy: Cell::new(false),
            partial: [const { Cell::new(null_mut()) }; NCLS],
            full: [const { Cell::new(null_mut()) }; NCLS],
            spare: Cell::new(null_mut()),
            rc: [const { Pending { run: Cell::new(0), head: Cell::new(0), tail: Cell::new(0), n: Cell::new(0) } }; RCACHE],
            live: AtomicIsize::new(0),
            tail_at: Cell::new((0, 0)),
            tail_again: Cell::new(0),
            tail_deferred: Cell::new(false),
            rnext: Cell::new(core::ptr::null()),
            rprev: Cell::new(core::ptr::null()),
        }
    };
}

#[inline(always)]
fn count(h: &Heap, d: isize) {
    h.live.store(h.live.load(Relaxed).wrapping_add(d), Relaxed);
}

/// The live heaps, under a spin lock taken at thread start / exit and by `live_bytes` only; exited counts in DEAD_LIVE.
static REG_LOCK: crate::Line<AtomicBool> = crate::Line(AtomicBool::new(false));
static REG_HEAD: AtomicPtr<Heap> = AtomicPtr::new(null_mut());
static DEAD_LIVE: crate::Line<AtomicIsize> = crate::Line(AtomicIsize::new(0));
static CACHE_RUNS: AtomicIsize = AtomicIsize::new(0);

fn reg_lock() {
    while REG_LOCK.0.compare_exchange_weak(false, true, Acquire, Relaxed).is_err() {
        core::hint::spin_loop();
    }
}

fn reg_unlock() {
    REG_LOCK.0.store(false, Release);
}

fn register(h: &Heap) {
    reg_lock();
    let head = REG_HEAD.load(Relaxed);
    h.rnext.set(head);
    h.rprev.set(core::ptr::null());
    if !head.is_null() {
        // SAFETY: a registered heap: its thread unregisters it (under this lock) before its storage goes.
        unsafe { (*head).rprev.set(h) };
    }
    REG_HEAD.store(h as *const Heap as *mut Heap, Relaxed);
    reg_unlock();
}

fn unregister(h: &Heap) {
    reg_lock();
    let (n, p) = (h.rnext.get(), h.rprev.get());
    // SAFETY: registered neighbours, as in `register`.
    unsafe {
        if !n.is_null() {
            (*n).rprev.set(p);
        }
        if p.is_null() {
            REG_HEAD.store(n as *mut Heap, Relaxed);
        } else {
            (*p).rnext.set(n);
        }
    }
    DEAD_LIVE.0.fetch_add(h.live.swap(0, Relaxed), Relaxed);
    reg_unlock();
}

/// Class bytes the tier has handed out and not taken back, every thread's: the live heaps' counts and the exited ones'.
pub(crate) fn live_bytes() -> isize {
    reg_lock();
    let mut sum = DEAD_LIVE.0.load(Relaxed);
    let mut h = REG_HEAD.load(Relaxed) as *const Heap;
    while !h.is_null() {
        // SAFETY: registered, as in `register`.
        unsafe {
            sum = sum.wrapping_add((*h).live.load(Relaxed));
            h = (*h).rnext.get();
        }
    }
    reg_unlock();
    sum
}

pub(crate) fn cache_run_bytes() -> isize {
    CACHE_RUNS.load(Relaxed) * RUN as isize
}

#[inline(always)]
fn heap() -> &'static Heap {
    // SAFETY: the heap lives as long as its thread, and the reference never leaves the call on that thread that took it.
    unsafe { &*HEAP.with(|h| h as *const Heap) }
}

/// A run's `owner`: the thread pointer (no dead thread's: an exiting thread gives up every run, and owns none after).
#[inline(always)]
fn tid() -> u64 {
    #[cfg(all(target_os = "linux", target_arch = "aarch64", not(miri)))]
    {
        let t: u64;
        // SAFETY: reads the thread pointer register.
        unsafe { core::arch::asm!("mrs {}, tpidr_el0", out(reg) t, options(pure, nomem, nostack, preserves_flags)) };
        t
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64", not(miri)))]
    {
        let t: u64;
        // SAFETY: reads the thread control block's self pointer.
        unsafe { core::arch::asm!("mov {}, qword ptr fs:[0]", out(reg) t, options(pure, readonly, nostack, preserves_flags)) };
        t
    }
    #[cfg(not(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64"), not(miri))))]
    {
        HEAP.with(|h| h as *const Heap as u64)
    }
}

#[inline(always)]
fn owner<'a>(r: *mut Run) -> &'a AtomicU64 {
    // SAFETY: `r` is a run header in the pool zone, mapped for the process's life; only the atomic field is referenced.
    unsafe { &*core::ptr::addr_of!((*r).owner) }
}

#[inline(always)]
fn remote<'a>(r: *mut Run) -> &'a AtomicU64 {
    // SAFETY: as `owner`.
    unsafe { &*core::ptr::addr_of!((*r).remote) }
}

#[inline(always)]
fn alink<'a>(r: *mut Run) -> &'a AtomicU32 {
    // SAFETY: as `owner`.
    unsafe { &*core::ptr::addr_of!((*r).alink) }
}

#[inline(always)]
pub(crate) fn class_of(size: usize, align: usize) -> usize {
    debug_assert!(size <= SMALL_MAX && align <= ALIGN_MAX);
    // SAFETY: size <= SMALL_MAX, the caller's.
    let c = unsafe { *CLASS_OF.get_unchecked((size + 7) >> 3) } as usize;
    if align <= 8 { c } else { aligned_class(c, align) }
}

#[inline]
fn aligned_class(mut c: usize, align: usize) -> usize {
    while SIZES[c] as usize % align != 0 {
        c += 1;
    }
    c
}

#[inline(always)]
pub(crate) fn usable(size: usize, align: usize) -> usize {
    SIZES[class_of(size, align)] as usize
}

#[inline(always)]
fn run_of(p: *const u8) -> *mut Run {
    (p as usize & !(RUN - 1)) as *mut Run
}

/// # Safety
/// `p` is a live block of the tier.
#[inline]
pub(crate) unsafe fn block_size(p: *const u8) -> usize {
    // SAFETY: the caller's: `p`'s run is a formatted run.
    unsafe { (*run_of(p)).size as usize }
}

/// Whether `p` (in the pool) is a block of the tier: inside a 64 KiB block tagged SMALL or CACHE, never at its start
/// (the run's header is there; a big cache-class block of that bucket carries the same tag and starts its block).
#[inline]
pub(crate) fn is_small(p: *const u8) -> bool {
    let base = p as usize & !(RUN - 1);
    base != p as usize && matches!(crate::run::lookup(base as *const u8), Some(i) if i.kind == crate::run::Kind::Blocks { bucket: RUN_BUCKET } && (i.tag == Tag::SMALL || i.tag == Tag::CACHE))
}

#[inline(always)]
pub(crate) fn alloc(size: usize, align: usize) -> *mut u8 {
    let c = class_of(size, align);
    let h = heap();
    let r = h.cur[c].get();
    // SAFETY: `r` is this thread's run of the class (or EMPTY, whose free list is empty); free blocks link the next.
    unsafe {
        let b = (*r).free;
        if b != 0 {
            (*r).free = *(b as *const usize);
            (*r).used += 1;
            count(h, (*r).size as isize);
            return b as *mut u8;
        }
    }
    alloc_slow(h, c, size, align)
}

/// # Safety
/// `p` came from `alloc` and is not used again.
#[inline(always)]
pub(crate) unsafe fn free(p: *mut u8) {
    let r = run_of(p);
    // SAFETY: the caller's: `r` is the block's run; its owner-only fields are ours when its owner is us.
    unsafe {
        if owner(r).load(Relaxed) == tid() {
            *(p as *mut usize) = (*r).free;
            (*r).free = p as usize;
            let u = (*r).used - 1;
            (*r).used = u;
            count(heap(), -((*r).size as isize));
            if u == (*r).watch {
                free_local_slow(r);
            }
        } else {
            free_remote(r, p);
        }
    }
}

/// Whether a live block of the tier is the cache class's.
/// # Safety
/// `p` is a live block of the tier.
#[inline]
pub(crate) unsafe fn is_cache_block(p: *const u8) -> bool {
    // SAFETY: the caller's: `p`'s run is a formatted run; `kind` is written only before its blocks go out.
    unsafe { (*run_of(p)).kind == CACHE }
}

/// A block of the cache class: from this thread's current cache run of the class (no general block shares its run).
#[inline]
pub(crate) fn cache_alloc(size: usize, align: usize) -> *mut u8 {
    let c = class_of(size, align);
    let h = heap();
    let r = h.ccur[c].get();
    // SAFETY: as `alloc`'s, on the cache class's current run.
    unsafe {
        if (*r).free != 0 {
            count(h, (*r).size as isize);
            return pop(r);
        }
    }
    cache_slow(h, c)
}

#[inline(never)]
fn cache_slow(h: &Heap, c: usize) -> *mut u8 {
    match h.state.get() {
        // pphatch: no cache block under the hatch (null: the caller's system path)
        NEW if !start(h) => return null_mut(),
        DEAD => return dead_alloc(c, CACHE),
        _ => {}
    }
    // SAFETY: the current cache run is this heap's; a found one is ours once taken.
    unsafe {
        let r = h.ccur[c].replace(empty());
        if r != empty() {
            if take_remote_into_empty(r) || extend(r) {
                h.ccur[c].set(r);
                count(h, (*r).size as isize);
                return pop(r);
            }
            release(r, c);
        }
        let r = find_cache(c);
        if r.is_null() {
            return null_mut();
        }
        (*r).state = CUR;
        (*r).watch = NEVER;
        h.ccur[c].set(r);
        h.busy.set(true);
        count(h, (*r).size as isize);
        pop(r)
    }
}

/// An abandoned cache run with a free block (the full ones go back after the look), else a new one.
unsafe fn find_cache(c: usize) -> *mut Run {
    // SAFETY: a popped abandoned run is ours until it is listed again.
    unsafe {
        let mut back = [0u32; ADOPT];
        let mut nb = 0;
        let mut got = null_mut();
        for _ in 0..ADOPT {
            let Some(i) = ab_pop(&ABANDONED_CACHE[c], ab_lock(CACHE, c), &Abandoned) else { break };
            let r = at(i);
            owner(r).store(tid(), Relaxed);
            collect(r);
            if (*r).free != 0 || extend(r) {
                COUNTS.adopted.fetch_add(1, Relaxed);
                got = r;
                break;
            }
            owner(r).store(0, Relaxed);
            back[nb] = i;
            nb += 1;
        }
        for &i in &back[..nb] {
            ABANDONED_CACHE[c].push(&Abandoned, i);
        }
        if got.is_null() { fresh(c, tid(), CACHE) } else { got }
    }
}

#[inline(always)]
unsafe fn pop(r: *mut Run) -> *mut u8 {
    // SAFETY: the caller's: our run with a free block.
    unsafe {
        let b = (*r).free;
        debug_assert!(b != 0);
        (*r).free = *(b as *const usize);
        (*r).used += 1;
        b as *mut u8
    }
}

#[inline(never)]
fn alloc_slow(h: &Heap, c: usize, size: usize, align: usize) -> *mut u8 {
    match h.state.get() {
        NEW if !start(h) => return crate::global::hatch_alloc(size, align),
        DEAD => return dead_alloc(c, GENERAL),
        _ => {}
    }
    // SAFETY: every run reached here is this heap's.
    unsafe {
        let r = h.cur[c].get();
        if r != empty() {
            if take_remote_into_empty(r) || extend(r) {
                count(h, (*r).size as isize);
                return pop(r);
            }
            h.cur[c].set(empty());
            to_full(h, c, r);
        }
        let r = find(h, c);
        if r.is_null() {
            return null_mut();
        }
        (*r).state = CUR;
        (*r).watch = NEVER;
        h.cur[c].set(r);
        count(h, (*r).size as isize);
        pop(r)
    }
}

/// The thread's heap goes live. pphatch: false under the hatch (mimalloc serving), the heap left NEW: every take of the
/// thread then misses its empty runs and comes back here, to be mimalloc's (no run, registration or exit hook is made).
fn start(h: &Heap) -> bool {
    if crate::global::hatch() {
        return false;
    }
    h.state.set(LIVE);
    register(h);
    if !crate::GLOBAL_ON.load(Relaxed) {
        crate::GLOBAL_ON.store(true, Relaxed);
    }
    exit::arm();
    true
}

/// pphatch: a free of the tier's block under the hatch (none exists: PoolAlloc's free tests the mode
/// first). An allocator may not unwind: a line to stderr (any block it needs comes from mimalloc), then abort.
#[cold]
#[inline(never)]
fn hatch_reached() -> ! {
    let m = b"pagepool: the small tier reached while mimalloc serves (the hatch)\n";
    let _ = std::io::Write::write_all(&mut std::io::stderr(), m);
    std::process::abort()
}

/// A take after this thread's exit destructor: from a run that goes straight back onto the abandoned list.
#[cold]
fn dead_alloc(c: usize, kind: u8) -> *mut u8 {
    let stack = abandoned(kind, c);
    // SAFETY: a popped abandoned run or a fresh one is ours alone until `release` lists it again.
    unsafe {
        let r = match ab_pop(stack, ab_lock(kind, c), &Abandoned) {
            Some(i) => {
                let r = at(i);
                collect(r);
                if (*r).free != 0 || extend(r) {
                    r
                } else {
                    stack.push(&Abandoned, i);
                    fresh(c, 0, kind)
                }
            }
            None => fresh(c, 0, kind),
        };
        if r.is_null() {
            return null_mut();
        }
        DEAD_LIVE.0.fetch_add((*r).size as isize, Relaxed);
        let b = pop(r);
        release(r, c);
        b
    }
}

/// A run with a free block: partial, full with remote frees, adopted, the spare, then a new one. Null: refused.
unsafe fn find(h: &Heap, c: usize) -> *mut Run {
    // SAFETY: the lists hold this heap's runs; an adopted run is ours once popped.
    unsafe {
        if let Some(r) = list_pop(&h.partial[c]) {
            return r;
        }
        let first = h.full[c].get();
        for k in 0..SCAN {
            let r = h.full[c].get();
            if r.is_null() || (k > 0 && r == first) {
                break;
            }
            if remote(r).load(Relaxed) != 0 {
                list_unlink(&h.full[c], r);
                take_remote_into_empty(r);
                return r;
            }
            h.full[c].set((*r).next as *mut Run);
        }
        for _ in 0..ADOPT {
            let Some(i) = ab_pop(&ABANDONED[c], ab_lock(GENERAL, c), &Abandoned) else { break };
            let r = at(i);
            owner(r).store(tid(), Relaxed);
            COUNTS.adopted.fetch_add(1, Relaxed);
            collect(r);
            if (*r).free != 0 || extend(r) {
                return r;
            }
            to_full(h, c, r);
        }
        let s = h.spare.replace(null_mut());
        if !s.is_null() {
            COUNTS.spare.fetch_add(1, Relaxed);
            if (*s).cls as usize != c {
                format(s, c, tid(), false, GENERAL);
            }
            if (*s).free != 0 || extend(s) {
                return s;
            }
            give_run(s);
        }
        let _ = h;
        fresh(c, tid(), GENERAL)
    }
}

const RUN_LAYOUT: Layout = match Layout::from_size_align(RUN, RUN) {
    Ok(l) => l,
    Err(_) => panic!(),
};

/// A new run of `kind` for `owner` (0: none): the pool's, or the system's while it is off or starting. Null: refused.
#[cold]
#[inline(never)]
unsafe fn fresh(c: usize, owner: u64, kind: u8) -> *mut Run {
    let (p, lazy) = if crate::global::ready() {
        let _u = crate::global::Uncapped::new();
        match api::take_block_zero(RUN_BUCKET, hint(kind)) {
            Some(t) => t,
            None => return null_mut(),
        }
    } else {
        // SAFETY: a valid layout; `give_run` frees a run outside the pool through `System`.
        let p = unsafe { System.alloc(RUN_LAYOUT) } as usize;
        if p == 0 || p >> 48 != 0 {
            return null_mut();
        }
        (p, true)
    };
    COUNTS.fresh.fetch_add(1, Relaxed);
    if kind == CACHE && crate::owns(p as *const u8) {
        CACHE_RUNS.fetch_add(1, Relaxed);
    }
    let r = p as *mut Run;
    // SAFETY: a 64 KiB block just taken: ours.
    unsafe {
        format(r, c, owner, lazy, kind);
        extend(r);
    }
    r
}

#[inline]
fn hint(kind: u8) -> Hint {
    if kind == CACHE { CACHE_HINT } else { RUN_HINT }
}

unsafe fn format(r: *mut Run, c: usize, id: u64, lazy: bool, kind: u8) {
    let size = SIZES[c];
    let cap = ((RUN - HDR) / size as usize) as u32;
    // SAFETY: the caller's: a run we hold that no other thread reaches (no block of it is out).
    unsafe {
        (*r).free = 0;
        alink(r).store(NONE, Relaxed);
        (*r).used = 0;
        (*r).watch = NEVER;
        (*r).bump = HDR as u32;
        (*r).end = HDR as u32 + cap * size;
        (*r).size = size;
        (*r).cap = cap;
        (*r).cls = c as u8;
        (*r).state = CUR;
        (*r).lazy = lazy;
        (*r).kind = kind;
        (*r).prev = 0;
        (*r).next = 0;
        remote(r).store(0, Relaxed);
        owner(r).store(id, Relaxed);
    }
}

/// Link never-used blocks: a page at a time on a fresh (maybe unfaulted) run, all on a warm one. False: none left.
unsafe fn extend(r: *mut Run) -> bool {
    // SAFETY: our run; the blocks past `bump` are unused.
    unsafe {
        let (bump, end, size) = ((*r).bump, (*r).end, (*r).size);
        if bump >= end {
            return false;
        }
        let left = (end - bump) / size;
        let n = if (*r).lazy { (EXTEND / size).max(4).min(left) } else { left };
        let base = r as usize;
        let first = base + bump as usize;
        let mut p = first;
        for _ in 1..n {
            let q = p + size as usize;
            *(p as *mut usize) = q;
            p = q;
        }
        *(p as *mut usize) = (*r).free;
        (*r).free = first;
        (*r).bump = bump + n * size;
        true
    }
}

/// The run's remote list as its free list (which is empty); whether it had any.
#[inline]
unsafe fn take_remote_into_empty(r: *mut Run) -> bool {
    let (head, n) = remote_take(remote(r));
    if n == 0 {
        return false;
    }
    // SAFETY: our run, its free list empty; the remote list's links were published by the pushers' Release.
    unsafe {
        debug_assert_eq!((*r).free, 0);
        (*r).free = r as usize + head as usize;
        (*r).used -= n;
    }
    true
}

/// The run's remote list spliced onto its free list.
unsafe fn collect(r: *mut Run) {
    let (head, n) = remote_take(remote(r));
    if n == 0 {
        return;
    }
    // SAFETY: a run we hold; `n` blocks chain from `head`, the last linking 0.
    unsafe {
        let first = r as usize + head as usize;
        let mut last = first;
        for _ in 1..n {
            last = *(last as *const usize);
        }
        *(last as *mut usize) = (*r).free;
        (*r).free = first;
        (*r).used -= n;
    }
}

unsafe fn to_full(h: &Heap, c: usize, r: *mut Run) {
    // SAFETY: our run, off every list.
    unsafe {
        debug_assert!((*r).used > 0);
        (*r).state = FULL;
        (*r).watch = (*r).used - 1;
        list_push(&h.full[c], r);
    }
}

#[inline(never)]
unsafe fn free_local_slow(r: *mut Run) {
    let h = heap();
    // SAFETY: our run, on the list its state names.
    unsafe {
        let c = (*r).cls as usize;
        match (*r).state {
            FULL => {
                list_unlink(&h.full[c], r);
                (*r).state = PARTIAL;
                (*r).watch = 0;
                list_push(&h.partial[c], r);
            }
            PARTIAL => {
                list_unlink(&h.partial[c], r);
                retire(h, r);
            }
            _ => {}
        }
    }
}

/// A run of ours with every block home: the heap's spare; the spare it replaces goes back to the pool.
unsafe fn retire(h: &Heap, r: *mut Run) {
    // SAFETY: our run, off every list, no block out (so no free can reach it).
    unsafe {
        (*r).state = SPARE;
        (*r).watch = NEVER;
    }
    let old = h.spare.replace(r);
    if !old.is_null() {
        // SAFETY: the old spare: ours, nothing out.
        unsafe { give_run(old) };
    }
}

unsafe fn give_run(r: *mut Run) {
    // SAFETY: the caller's: a run we hold with no block out.
    unsafe {
        debug_assert_eq!((*r).used, 0);
        (*r).state = FLOAT;
        owner(r).store(0, Relaxed);
        COUNTS.retired.fetch_add(1, Relaxed);
        if crate::owns(r as *const u8) {
            if (*r).kind == CACHE {
                CACHE_RUNS.fetch_sub(1, Relaxed);
            }
            api::give_run_home(r as usize, hint((*r).kind));
        } else {
            System.dealloc(r as *mut u8, RUN_LAYOUT);
        }
    }
}

#[inline(never)]
unsafe fn free_remote(r: *mut Run, p: *mut u8) {
    let h = heap();
    let base = r as usize;
    // SAFETY: the caller's: `r` is a formatted run; `kind` and `size` are written only before its blocks go out.
    let (direct, size) = unsafe { ((*r).kind == CACHE, (*r).size as isize) };
    if h.state.get() == NEW && !start(h) {
        hatch_reached();
    }
    if h.state.get() == LIVE {
        count(h, -size);
    } else {
        DEAD_LIVE.0.fetch_sub(size, Relaxed);
    }
    match h.state.get() {
        LIVE if !direct => {}
        _ => {
            // a dead heap's free, or a cache block's: at once, so the run never waits on this thread
            let off = (p as usize - base) as u32;
            remote_push(remote(r), &RunLinks(base), off);
            return;
        }
    }
    let e = &h.rc[(base >> 16) & (RCACHE - 1)];
    // SAFETY: `p` is the caller's block, ours until the chain is pushed.
    unsafe {
        if e.run.get() == base {
            *(p as *mut usize) = e.head.get();
            e.head.set(p as usize);
            let n = e.n.get() + 1;
            e.n.set(n);
            if n == RBATCH {
                flush_pending(e);
            }
        } else {
            if e.run.get() != 0 {
                flush_pending(e);
            }
            e.run.set(base);
            h.busy.set(true);
            e.head.set(p as usize);
            e.tail.set(p as usize);
            e.n.set(1);
        }
    }
}

fn flush_pending(e: &Pending) {
    let base = e.run.replace(0);
    let (head, tail) = ((e.head.get() - base) as u32, (e.tail.get() - base) as u32);
    remote_push_chain(remote(base as *mut Run), &RunLinks(base), head, tail, e.n.get());
}

fn flush_all_pending(h: &Heap) {
    for e in h.rc.iter() {
        if e.run.get() != 0 {
            flush_pending(e);
        }
    }
}

struct RunLinks(usize);

impl Links for RunLinks {
    #[inline(always)]
    fn set_next(&self, off: u32, next: u32) {
        let v = if next == 0 { 0 } else { self.0 + next as usize };
        // SAFETY: `off` is a block of this run being freed by the caller, ours until the push publishes it.
        unsafe { *((self.0 + off as usize) as *mut usize) = v };
    }
}

/// The block links a remote list writes (a model's cells under loom, a run's blocks in the product).
pub trait Links {
    /// Point the block at `off` to the block at `next` (0: the end).
    fn set_next(&self, off: u32, next: u32);
}

#[inline]
pub fn remote_push<A: Atom64>(w: &A, links: &impl Links, off: u32) -> u64 {
    remote_push_chain(w, links, off, off, 1)
}

/// Push a chain of `n` blocks, `head` to `tail` (already linked), onto a run's remote list (one CAS); the word before.
#[inline]
pub fn remote_push_chain<A: Atom64>(w: &A, links: &impl Links, head: u32, tail: u32, n: u32) -> u64 {
    debug_assert!(head != 0 && head < 1 << 16 && tail != 0 && tail < 1 << 16 && n > 0);
    let mut cur = w.load(Relaxed);
    loop {
        links.set_next(tail, (cur & 0xffff) as u32);
        let new = ((cur & !0xffff) + ((n as u64) << 16)) | head as u64;
        match w.cas(cur, new, Release, Relaxed) {
            Ok(_) => return cur,
            Err(now) => cur = now,
        }
    }
}

/// Take a run's whole remote list: (head offset, blocks); (0, 0) when empty.
#[inline]
pub fn remote_take<A: Atom64>(w: &A) -> (u32, u32) {
    if w.load(Relaxed) == 0 {
        return (0, 0);
    }
    let v = w.swap(0, Acquire);
    ((v & 0xffff) as u32, ((v >> 16) & 0xffff) as u32)
}

unsafe fn list_push(head: &Cell<*mut Run>, r: *mut Run) {
    let h = head.get();
    // SAFETY: the caller's: our runs, on this list or `r` on none.
    unsafe {
        if h.is_null() {
            (*r).next = r as usize;
            (*r).prev = r as usize;
            head.set(r);
        } else {
            let t = (*h).prev as *mut Run;
            (*r).next = h as usize;
            (*r).prev = t as usize;
            (*t).next = r as usize;
            (*h).prev = r as usize;
        }
    }
}

unsafe fn list_unlink(head: &Cell<*mut Run>, r: *mut Run) {
    // SAFETY: the caller's: `r` is on this list.
    unsafe {
        let n = (*r).next as *mut Run;
        if n == r {
            head.set(null_mut());
            return;
        }
        let p = (*r).prev as *mut Run;
        (*p).next = n as usize;
        (*n).prev = p as usize;
        if head.get() == r {
            head.set(n);
        }
    }
}

unsafe fn list_pop(head: &Cell<*mut Run>) -> Option<*mut Run> {
    let h = head.get();
    if h.is_null() {
        return None;
    }
    // SAFETY: the caller's.
    unsafe { list_unlink(head, h) };
    Some(h)
}

/// Runs whose thread exited with blocks still out, per class; any thread adopts them.
static ABANDONED: [Stack; NCLS] = [const { Stack::new() }; NCLS];
/// The cache class's: its replaced current runs and dead threads' (cache runs never wait on an owner).
static ABANDONED_CACHE: [Stack; NCLS] = [const { Stack::new() }; NCLS];
/// Each abandoned list's remover lock (`ab_pop`, `ab_take_all`), the general lists' then the cache lists'.
static AB_LOCK: [AtomicU32; 2 * NCLS] = [const { AtomicU32::new(0) }; 2 * NCLS];

#[inline]
fn abandoned(kind: u8, c: usize) -> &'static Stack {
    if kind == CACHE { &ABANDONED_CACHE[c] } else { &ABANDONED[c] }
}

#[inline]
fn ab_lock(kind: u8, c: usize) -> &'static AtomicU32 {
    &AB_LOCK[(kind == CACHE) as usize * NCLS + c]
}

/// A spin lock word (core's; loom's in its model).
pub trait Spin {
    fn lock(&self);
    fn unlock(&self);
}

impl Spin for AtomicU32 {
    #[inline]
    fn lock(&self) {
        crate::superpage::spin(self)
    }
    #[inline]
    fn unlock(&self) {
        crate::superpage::unspin(self)
    }
}

/// An abandoned list's pop, under the list's remover lock: a pop reads the top run's `alink`, and only a remover takes
/// a run off, so with removers one at a time that run is still listed, never one another thread adopted and gave back
/// (a system run's memory goes to `System`). Pushers stay lock-free.
pub fn ab_pop<A: Atom64>(s: &Stack<A>, lock: &impl Spin, nodes: &impl Nodes) -> Option<u32> {
    lock.lock();
    let got = s.pop(nodes);
    lock.unlock();
    got
}

/// An abandoned list emptied under its remover lock; its old top (the runs chain through `alink`).
pub fn ab_take_all<A: Atom64>(s: &Stack<A>, lock: &impl Spin) -> Option<u32> {
    lock.lock();
    let got = s.take_all();
    lock.unlock();
    got
}

struct Abandoned;

impl Nodes for Abandoned {
    #[inline]
    fn next_batch(&self, i: u32) -> u32 {
        alink(at(i)).load(Relaxed)
    }
    #[inline]
    fn set_next_batch(&self, i: u32, v: u32) {
        alink(at(i)).store(v, Relaxed)
    }
}

/// A run's list index: its address over 64 KiB (runs lie below 2^48: the pool's, and the system's taken only there).
#[inline]
fn at(i: u32) -> *mut Run {
    ((i as usize) << 16) as *mut Run
}

#[inline]
fn index(r: *mut Run) -> u32 {
    debug_assert!((r as usize) >> 48 == 0);
    ((r as usize) >> 16) as u32
}

/// A run leaving this heap: to the pool when every block is home, else (and a cache run always) onto an abandoned list.
unsafe fn release(r: *mut Run, c: usize) {
    // SAFETY: preserve the caller's detached-run ownership contract.
    unsafe { release_run(r, c, false) };
}

/// An idle handoff keeps empty runs formatted for the next taker; a reclamation sweep can still return them.
unsafe fn release_run(r: *mut Run, c: usize, keep_empty: bool) {
    // SAFETY: the caller's: a run of ours, off every list.
    unsafe {
        collect(r);
        // A failed reservation has no purger to reclaim empty system-backed cache runs.
        if (*r).used == 0 && ((!keep_empty && (*r).kind != CACHE) || !crate::owns(r.cast())) {
            give_run(r);
        } else {
            (*r).state = FLOAT;
            (*r).watch = NEVER;
            owner(r).store(0, Relaxed);
            COUNTS.abandoned.fetch_add(1, Relaxed);
            abandoned((*r).kind, c).push(&Abandoned, index(r));
        }
    }
}

/// Thread exit: every run leaves (to the pool, or abandoned for adoption); the heap owns none after.
fn abandon() {
    let h = heap();
    if h.state.get() != LIVE {
        return;
    }
    flush_all_pending(h);
    // SAFETY: this heap's runs.
    unsafe {
        for c in 0..NCLS {
            for r in [h.cur[c].replace(empty()), h.ccur[c].replace(empty())] {
                if r != empty() {
                    release(r, c);
                }
            }
            while let Some(r) = list_pop(&h.partial[c]) {
                release(r, c);
            }
            while let Some(r) = list_pop(&h.full[c]) {
                release(r, c);
            }
        }
        let s = h.spare.replace(null_mut());
        if !s.is_null() {
            give_run(s);
        }
    }
    h.state.set(DEAD);
    unregister(h);
    crate::shelf::flush_shelf();
}

/// This thread's idle runs (every block home, remote frees taken) and its spare back to the pool; how many.
pub(crate) fn trim() -> usize {
    let h = heap();
    if h.state.get() != LIVE {
        return 0;
    }
    flush_all_pending(h);
    let before = COUNTS.retired.load(Relaxed);
    // SAFETY: this heap's runs.
    unsafe {
        for c in 0..NCLS {
            let r = h.ccur[c].replace(empty());
            if r != empty() {
                release(r, c);
            }
            let r = h.cur[c].get();
            if r != empty() {
                collect(r);
                if (*r).used == 0 {
                    h.cur[c].set(empty());
                    give_run(r);
                }
            }
            let mut keep = Cell::new(null_mut());
            while let Some(r) = list_pop(&h.partial[c]) {
                collect(r);
                if (*r).used == 0 {
                    give_run(r);
                } else {
                    list_push(&keep, r);
                }
            }
            h.partial[c].set(keep.get());
            keep = Cell::new(null_mut());
            while let Some(r) = list_pop(&h.full[c]) {
                collect(r);
                if (*r).used == 0 {
                    give_run(r);
                } else if (*r).free != 0 {
                    (*r).state = PARTIAL;
                    (*r).watch = 0;
                    list_push(&h.partial[c], r);
                } else {
                    list_push(&keep, r);
                }
            }
            h.full[c].set(keep.get());
        }
        let s = h.spare.replace(null_mut());
        if !s.is_null() {
            give_run(s);
        }
    }
    (COUNTS.retired.load(Relaxed) - before) as usize
}

/// pppress card 10: `PGRUST_POOL_IDLE_TAIL` (0 unread, 1 off, 2 on), the runs cut and the resident bytes they gave back.
static TAIL_SWITCH: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
static TAIL_RUNS: AtomicU64 = AtomicU64::new(0);
static TAIL_BYTES: AtomicU64 = AtomicU64::new(0);
/// Card 10: idle cuts, their ns (total, longest, by power of two from 1 us), and the ones the budget stopped.
static TAIL_CALLS: AtomicU64 = AtomicU64::new(0);
static TAIL_NS: AtomicU64 = AtomicU64::new(0);
static TAIL_MAX_NS: AtomicU64 = AtomicU64::new(0);
static TAIL_HIST: [AtomicU64; TAIL_BINS] = [const { AtomicU64::new(0) }; TAIL_BINS];
static TAIL_STOPS: AtomicU64 = AtomicU64::new(0);
/// Card 10: tails left for a later slice while the purger splits their huge folio.
static TAIL_DEFERS: AtomicU64 = AtomicU64::new(0);
/// Card 10: passes a cut runs again for its deferred tails before it leaves them to the next idle period.
const TAIL_AGAIN: u8 = 4;
pub(crate) const TAIL_BINS: usize = 16;
/// An idle cut stops between runs past this (it runs on the session's thread, ahead of its next command's read): the
/// next one, a timer slice later, goes on from there. Half of the 1 ms a cut may take, since one run's cut can split a
/// superpage's folio first.
static TAIL_BUDGET_NS: AtomicU64 = AtomicU64::new(500_000);

pub(crate) fn tail_on() -> bool {
    match TAIL_SWITCH.load(Relaxed) {
        0 => {
            let v = if crate::os::env_is_one(c"PGRUST_POOL_IDLE_TAIL") { 2 } else { 1 };
            TAIL_SWITCH.store(v, Relaxed);
            v == 2
        }
        v => v == 2,
    }
}

pub(crate) fn force_tail(on: bool) {
    TAIL_SWITCH.store(if on { 2 } else { 1 }, Relaxed);
}

/// Card 10 test seam: the idle cut's time budget (0: one run a cut).
pub(crate) fn force_tail_budget(ns: u64) {
    TAIL_BUDGET_NS.store(ns, Relaxed);
}

pub(crate) fn tail_counts() -> (u64, u64, u64) {
    (TAIL_RUNS.load(Relaxed), TAIL_BYTES.load(Relaxed), TAIL_DEFERS.load(Relaxed))
}

/// (cuts, their total ns, the longest, the ones the budget stopped, cuts by ns: bin 0 under 1 us, bin k under 2^k us).
pub(crate) fn tail_times() -> (u64, u64, u64, u64, [u64; TAIL_BINS]) {
    (TAIL_CALLS.load(Relaxed), TAIL_NS.load(Relaxed), TAIL_MAX_NS.load(Relaxed), TAIL_STOPS.load(Relaxed), core::array::from_fn(|i| TAIL_HIST[i].load(Relaxed)))
}

/// Card 10, a session's idle release: each run this thread owns with blocks out (current and partial; a full run has
/// no free block) gives its pages past its last used block back to the OS, class by class. Before each run after the
/// first, past `TAIL_BUDGET_NS` the cut stops; `resume` goes on from the run it stopped at, else a cut starts at the first
/// class. A tail in a superpage still PMD-mapped waits for the purger to split its folio (`purger::ask_split`): the pass
/// then runs again from the first class, up to `TAIL_AGAIN` times. The resident bytes given back, and whether runs are
/// left (the caller's idle timer comes back).
pub(crate) fn give_idle_tails(resume: bool) -> (usize, bool) {
    let h = heap();
    if h.state.get() != LIVE {
        return (0, false);
    }
    let t0 = pg_clock::mono_ns();
    let budget = TAIL_BUDGET_NS.load(Relaxed);
    flush_all_pending(h);
    let (c0, skip) = if resume { h.tail_at.get() } else { (0, 0) };
    if !resume {
        h.tail_again.set(0);
    }
    let (mut bytes, mut stop, mut cuts, mut deferred) = (0, None, 0u32, resume && h.tail_deferred.get());
    // SAFETY: this heap's runs, on their owner thread; the partial lists are circular.
    unsafe {
        'classes: for c in c0 as usize..NCLS {
            let (cur, first) = (h.cur[c].get(), h.partial[c].get());
            // the class's current run, then its partial ring; `i` counts the runs of the walk (those of the first class
            // the last cut passed are not cut again)
            let (mut r, mut ring) = if cur != empty() { (cur, false) } else { (first, true) };
            let mut i = 0u32;
            while !r.is_null() {
                if c != c0 as usize || i >= skip {
                    if cuts > 0 && pg_clock::mono_ns().saturating_sub(t0) > budget {
                        stop = Some((c as u8, i));
                        break 'classes;
                    }
                    let (b, d) = cut_tail(r);
                    (bytes, deferred, cuts) = (bytes + b, deferred | d, cuts + 1);
                }
                i += 1;
                if !ring {
                    (r, ring) = (first, true);
                    continue;
                }
                r = (*r).next as *mut Run;
                if r == first {
                    break;
                }
            }
        }
    }
    h.tail_at.set(stop.unwrap_or((0, 0)));
    h.tail_deferred.set(stop.is_some() && deferred);
    // a whole pass that left tails to the purger's split runs again (a timer slice on), a bounded number of times
    let again = stop.is_none() && deferred && h.tail_again.get() < TAIL_AGAIN;
    if again {
        h.tail_again.set(h.tail_again.get() + 1);
    }
    let ns = pg_clock::mono_ns().saturating_sub(t0);
    TAIL_CALLS.fetch_add(1, Relaxed);
    TAIL_NS.fetch_add(ns, Relaxed);
    TAIL_MAX_NS.fetch_max(ns, Relaxed);
    TAIL_HIST[((u64::BITS - (ns >> 10).leading_zeros()) as usize).min(TAIL_BINS - 1)].fetch_add(1, Relaxed);
    if stop.is_some() {
        TAIL_STOPS.fetch_add(1, Relaxed);
    }
    (bytes, stop.is_some() || again)
}

/// Run `r`'s whole pages past its last used block back (`os::age_release`: a range with no page mapped goes with no
/// call). The free blocks there leave the free list and `bump` comes back to the first block past the kept pages, the
/// run lazy from here, so `extend` links them again a page at a time. A partial run keeps a free block (`find` takes one
/// from it). With D4760's split on, a range still PMD-mapped is left for a later call once the purger has split its folio
/// (MADV_COLD, ~0.1-1 ms, not on a session's thread), and one with no PMD mapping goes with no split call. The resident
/// bytes given back, and whether the range was left for the purger's split.
/// # Safety
/// A run of this thread's heap, on its owner thread.
unsafe fn cut_tail(r: *mut Run) -> (usize, bool) {
    const PAGE: usize = 4096;
    // SAFETY: the caller's: our run; its free blocks and the bytes past `bump` are no one's.
    unsafe {
        collect(r);
        if (*r).used == 0 || !crate::owns(r as *const u8) {
            return (0, false);
        }
        let (base, size) = (r as usize, (*r).size as usize);
        let linked = ((*r).bump as usize - HDR) / size;
        let mut free = [0u64; (RUN - HDR) / 8 / 64 + 1];
        let mut b = (*r).free;
        while b != 0 {
            let i = (b - base - HDR) / size;
            free[i / 64] |= 1 << (i % 64);
            b = *(b as *const usize);
        }
        let mut last = linked;
        while last > 0 && free[(last - 1) / 64] & (1 << ((last - 1) % 64)) != 0 {
            last -= 1;
        }
        let keep = (HDR + last * size).next_multiple_of(PAGE);
        if keep >= RUN {
            return (0, false);
        }
        let out = (keep - HDR).div_ceil(size);
        let mut head = 0usize;
        let mut tail: *mut usize = &raw mut head;
        let mut b = (*r).free;
        while b != 0 {
            let next = *(b as *const usize);
            if (b - base - HDR) / size < out {
                *tail = b;
                tail = b as *mut usize;
            }
            b = next;
        }
        *tail = 0;
        (*r).free = head;
        (*r).bump = ((HDR + out * size) as u32).min((*r).bump);
        (*r).lazy = true;
        // D4760's split, where on: by the purger for a range still PMD-mapped, none where no page of it is
        let huge = if crate::thp_partial() >= crate::THP_SPLIT { crate::present::huge_in(base + keep, RUN - keep) } else { Some(false) };
        let deferred = huge == Some(true) && crate::purger::ask_split(base + keep);
        let gone = if deferred { 0 } else { crate::os::age_release_with(base + keep, RUN - keep, huge != Some(false)).unwrap_or(0) };
        if deferred {
            TAIL_DEFERS.fetch_add(1, Relaxed);
        }
        if (*r).free == 0 && (*r).state == PARTIAL {
            extend(r);
        }
        if gone > 0 {
            TAIL_RUNS.fetch_add(1, Relaxed);
            TAIL_BYTES.fetch_add(gone as u64, Relaxed);
            crate::stats::RELEASES.fetch_add(1, Relaxed);
            crate::stats::RELEASED_BYTES.fetch_add(gone as u64, Relaxed);
            // pppress (present's overshoot fix): a tail of a run this thread owns, not free in the present sweep
            crate::stats::RELEASED_LIVE.fetch_add(gone as u64, Relaxed);
        }
        (gone, deferred)
    }
}

/// This thread's runs' touched bytes less their used blocks (remote frees not yet collected stay used). Walks the runs.
pub(crate) fn thread_retained() -> usize {
    let h = heap();
    if h.state.get() != LIVE {
        return 0;
    }
    // SAFETY: this heap's runs, read on their owner thread.
    let one = |r: *mut Run| unsafe {
        let touched = if (*r).lazy { ((*r).bump as usize).next_multiple_of(4096).min(RUN) } else { RUN };
        touched.saturating_sub((*r).used as usize * (*r).size as usize)
    };
    let list = |head: &Cell<*mut Run>| {
        let (h, mut sum) = (head.get(), 0);
        let mut r = h;
        while !r.is_null() {
            sum += one(r);
            // SAFETY: as above; the list is circular.
            r = unsafe { (*r).next as *mut Run };
            if r == h {
                break;
            }
        }
        sum
    };
    let mut sum = 0;
    for c in 0..NCLS {
        for r in [h.cur[c].get(), h.ccur[c].get()] {
            if r != empty() {
                sum += one(r);
            }
        }
        sum += list(&h.partial[c]) + list(&h.full[c]);
    }
    let s = h.spare.get();
    if !s.is_null() {
        sum += one(s);
    }
    sum
}

/// Hand current runs to the adoption lists before this thread parks. Live blocks stay valid, while another thread
/// can reuse the remaining slots. Flush pending remote frees too. The scan is bounded by the fixed size classes;
/// partial and full lists stay owned until a later trim or thread exit. No allocation, no syscall.
pub(crate) fn idle_point() {
    let h = heap();
    if h.state.get() != LIVE {
        return;
    }
    // General runs have no per-acquisition idle bookkeeping: scan their fixed slots here instead.
    if h.busy.replace(false) {
        flush_all_pending(h);
    }
    for c in 0..NCLS {
        let r = h.cur[c].get();
        // System fallbacks have no guaranteed purger. Keep them local so thread exit can free empty runs.
        if r != empty() && crate::owns(r.cast()) {
            h.cur[c].set(empty());
            // SAFETY: this heap's current general run; detach before publishing it to other threads.
            unsafe { release_run(r, c, true) };
        }
        let r = h.ccur[c].get();
        if r != empty() && crate::owns(r.cast()) {
            h.ccur[c].set(empty());
            // SAFETY: this heap's current cache run.
            unsafe { release(r, c) };
        }
    }
}

/// This thread's pending remote frees out, then the abandoned runs with every block home back to the pool.
pub(crate) fn flush_remote() -> usize {
    let h = heap();
    if h.state.get() == LIVE {
        flush_all_pending(h);
    }
    sweep_abandoned()
}

/// Abandoned runs whose blocks have all come home (by remote frees) back to the pool; the rest stay for adoption.
pub(crate) fn sweep_abandoned() -> usize {
    let mut n = 0;
    for (kind, c) in [GENERAL, CACHE].into_iter().flat_map(|k| (0..NCLS).map(move |c| (k, c))) {
        let stack = abandoned(kind, c);
        let mut cur = ab_take_all(stack, ab_lock(kind, c)).unwrap_or(NONE);
        while cur != NONE {
            let r = at(cur);
            let next = alink(r).load(Relaxed);
            // SAFETY: take_all made every run on the chain ours (no owner, no list).
            unsafe {
                collect(r);
                if (*r).used == 0 {
                    give_run(r);
                    n += 1;
                } else {
                    stack.push(&Abandoned, cur);
                }
            }
            cur = next;
        }
    }
    n
}

/// Runs taken from the pool, adopted from idle or dead threads, reused as a spare, given back, and handed off.
#[derive(Clone, Copy, Debug, Default)]
pub struct SmallCounts {
    pub fresh: u64,
    pub adopted: u64,
    pub spare: u64,
    pub retired: u64,
    pub abandoned: u64,
}

struct Counts {
    fresh: AtomicU64,
    adopted: AtomicU64,
    spare: AtomicU64,
    retired: AtomicU64,
    abandoned: AtomicU64,
}

static COUNTS: crate::Line<Counts> = crate::Line(Counts {
    fresh: AtomicU64::new(0),
    adopted: AtomicU64::new(0),
    spare: AtomicU64::new(0),
    retired: AtomicU64::new(0),
    abandoned: AtomicU64::new(0),
});

pub(crate) fn counts() -> SmallCounts {
    SmallCounts {
        fresh: COUNTS.fresh.load(Relaxed),
        adopted: COUNTS.adopted.load(Relaxed),
        spare: COUNTS.spare.load(Relaxed),
        retired: COUNTS.retired.load(Relaxed),
        abandoned: COUNTS.abandoned.load(Relaxed),
    }
}

#[cfg(unix)]
mod exit {
    use core::sync::atomic::{AtomicU64, Ordering::*};

    static KEY: AtomicU64 = AtomicU64::new(0);

    extern "C" fn at_exit(_: *mut libc::c_void) {
        super::abandon();
    }

    fn key() -> u64 {
        let k = KEY.load(Acquire);
        if k != 0 {
            return k;
        }
        let mut key: libc::pthread_key_t = 0;
        // SAFETY: `key` is a valid out pointer; `at_exit` stays valid for the process's life.
        if unsafe { libc::pthread_key_create(&mut key, Some(at_exit)) } != 0 {
            return 0;
        }
        match KEY.compare_exchange(0, key as u64 + 1, AcqRel, Acquire) {
            Ok(_) => key as u64 + 1,
            Err(won) => {
                // SAFETY: our own key, never set.
                unsafe { libc::pthread_key_delete(key) };
                won
            }
        }
    }

    pub(super) fn arm() {
        let k = key();
        if k != 0 {
            // SAFETY: a live key; any non-null value arms its destructor for this thread.
            unsafe { libc::pthread_setspecific((k - 1) as libc::pthread_key_t, core::ptr::NonNull::<u8>::dangling().as_ptr().cast()) };
        }
    }
}

#[cfg(not(unix))]
mod exit {
    pub(super) fn arm() {}
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn classes_cover_every_size_and_alignment() {
        for size in 1..=SMALL_MAX {
            for align in [1, 2, 4, 8, 16, 32, 64, 128] {
                let c = class_of(size, align);
                let s = SIZES[c] as usize;
                assert!(s >= size && s % align == 0, "{size} @ {align} -> {s}");
                if align <= 8 && c > 0 {
                    assert!((SIZES[c - 1] as usize) < size, "{size}: not the least class");
                }
                assert_eq!(HDR % align, 0);
            }
        }
        for w in SIZES.windows(2) {
            assert!(w[1] > w[0] && w[1] as f64 / w[0] as f64 <= 2.0);
        }
    }

    #[test]
    fn remote_words_round_trip() {
        let w = AtomicU64::new(0);
        struct Arr(Vec<Cell<u32>>);
        impl Links for Arr {
            fn set_next(&self, off: u32, next: u32) {
                self.0[off as usize / 8].set(next);
            }
        }
        let a = Arr((0..64).map(|_| Cell::new(u32::MAX)).collect());
        assert_eq!(remote_take(&w), (0, 0));
        for off in [128u32, 136, 200, 504] {
            remote_push(&w, &a, off);
        }
        let (head, n) = remote_take(&w);
        assert_eq!((head, n), (504, 4));
        let mut seen = vec![];
        let mut c = head;
        while c != 0 {
            seen.push(c);
            c = a.0[c as usize / 8].get();
        }
        assert_eq!(seen, vec![504, 200, 136, 128]);
        assert_eq!(remote_take(&w), (0, 0));
    }

    /// Run by `crate::tests::pool_kernels` (and Miri): distinct aligned blocks, remote frees, abandonment, all back.
    pub(crate) fn small_kernels() {
        let n = if cfg!(miri) { 300 } else { 20_000 };
        let mut held: Vec<(usize, usize)> = Vec::new();
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for i in 0..n {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let size = 1 + (x as usize % SMALL_MAX);
            let align = [1usize, 8, 16, 64][i % 4];
            let p = alloc(size, align) as usize;
            assert!(p != 0 && p % align == 0 && crate::owns(p as *const u8));
            assert!(usable(size, align) >= size);
            // SAFETY: a fresh block of `size` bytes.
            unsafe { core::ptr::write_bytes(p as *mut u8, (i & 0xff) as u8, size) };
            held.push((p, size));
            if i % 3 == 0 {
                let (q, _) = held.swap_remove((x >> 20) as usize % held.len());
                // SAFETY: ours, not used again.
                unsafe { free(q as *mut u8) };
            }
        }
        held.sort();
        for w in held.windows(2) {
            assert!(w[0].0 + w[0].1 <= w[1].0, "blocks overlap: {:?}", w);
        }
        assert!(thread_retained() > 0);
        let (mine, theirs) = held.split_at(held.len() / 2);
        let theirs = theirs.to_vec();
        std::thread::spawn(move || {
            for (p, _) in theirs {
                // SAFETY: handed over, not used again.
                unsafe { free(p as *mut u8) };
            }
        })
        .join()
        .unwrap();
        for &(p, _) in mine {
            // SAFETY: ours, not used again.
            unsafe { free(p as *mut u8) };
        }
        trim();
        let h = heap();
        for c in 0..NCLS {
            assert!(h.cur[c].get() == empty() && h.partial[c].get().is_null() && h.full[c].get().is_null(), "class {c} kept a run");
        }
        let before = counts();
        let out: Vec<usize> = std::thread::spawn(|| (0..64).map(|i| alloc(16 + i * 8, 8) as usize).collect()).join().unwrap();
        assert!(counts().abandoned > before.abandoned, "the dead thread's runs were abandoned");
        for p in out {
            // SAFETY: the dead thread's blocks, handed over.
            unsafe { free(p as *mut u8) };
        }
        let q = alloc(16, 8);
        // SAFETY: ours.
        unsafe { free(q) };
        trim();
        sweep_abandoned();
        for s in ABANDONED.iter() {
            assert!(s.pop(&Abandoned).is_none(), "an abandoned run with every block home was not given back");
        }
        // the cache class: its own runs, left at the idle point, freed by another thread, home through flush_remote
        let live = live_bytes();
        let blocks: Vec<usize> = (0..if cfg!(miri) { 60 } else { 3000 }).map(|i| cache_alloc(8 + (i % 128) * 8, 8) as usize).collect();
        // SAFETY: live blocks of the tier.
        assert!(blocks.iter().all(|&p| p != 0 && unsafe { is_cache_block(p as *const u8) }));
        assert!(live_bytes() > live);
        idle_point();
        std::thread::spawn(move || {
            for p in blocks {
                // SAFETY: handed over, not used again.
                unsafe { free(p as *mut u8) };
            }
        })
        .join()
        .unwrap();
        flush_remote();
        assert_eq!(live_bytes(), live);
        for s in ABANDONED_CACHE.iter() {
            assert!(s.pop(&Abandoned).is_none(), "a cache run with every block home stayed out of the pool");
        }
        crate::flush_shelf();
    }
}
