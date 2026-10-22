use core::alloc::Layout;
use core::ptr::NonNull;

use allocator_api2::alloc::{AllocError, Allocator};

use crate::cost::runtime::ARENA_MAPPED_MIN as MAPPED_MIN;

pub type ArenaVec<T> = allocator_api2::vec::Vec<T, ArenaRef>;
pub type ArenaBox<T> = allocator_api2::boxed::Box<T, ArenaRef>;

/// The header words before the data: used, charged (G1-A: the words the memory ledger has on account, a mapped block's
/// touched prefix in `CHARGE_WORDS` steps; a boxed block is door 1's, all of it), capacity, and a mapped block's ledger
/// state (`*mut MappedLedger`: its charge, its owner's door-2 bytes, and its residency entry; null for a boxed block), which
/// also keeps the data 32-byte aligned.
const HDR: usize = 4;

/// A mapped block's ledger state, boxed behind header word 3. D4733: the charged prefix is on the residency registry
/// (`memledger::resident`, as a Gravity region is, D3872), so the pass reads which of its pages are in memory: the bump
/// charges a step as it passes it, and much of what a kept program's image lays out there (output and scratch space, a
/// source's room) is never written, so its pages stay the kernel's zero page. Without the entry the account counted them
/// as resident (`not_resident_mapped` 0) though the cgroup never held them.
struct MappedLedger {
    charge: memledger::Charge,
    res: u32,
}

/// G1-A door 2: a mapped arena's pages are charged to the ledger as its bump pointer first passes them, in steps of the
/// ledger's own fold size, so the hot path keeps its one compare (against the charged words, not the capacity).
const CHARGE_WORDS: usize = memledger::DOOR_FOLD_BYTES as usize / 8;

/// Header word 2's top bits above the capacity: whether the block is the page pool's (`pagepool::taken`), as `try_new`
/// knew it from the road that served it (`RECORDED`: known; `POOL`: the pool's). The global allocator serves from the
/// pool's range too (D4720), so the address alone asks the pool's run table.
const RECORDED: u64 = 1 << 63;
const POOL: u64 = 1 << 62;
const CAP_MASK: u64 = POOL - 1;

static POOLED_BIT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// An arena records whether its block is the pool's when it is made, and its resets read that instead of asking
/// `pagepool::taken` (on until set; exec glue sets it once per process from `PGRUST_GRAVITY_POOLED_BIT`). Off: nothing is
/// recorded or read, every reset asks.
pub fn set_pooled_bit(on: bool) {
    POOLED_BIT.store(on, core::sync::atomic::Ordering::Relaxed);
}

/// Header word 2's bits above the capacity for a block from the pool's road (`pool`) or not.
fn recorded(pool: bool) -> u64 {
    if !POOLED_BIT.load(core::sync::atomic::Ordering::Relaxed) {
        return 0;
    }
    RECORDED | if pool { POOL } else { 0 }
}

pub struct Arena {
    /// D4518: a boxed block's own charge, whole (a mapped block's lives in its header, word 3, where its handles grow it):
    /// on the arena's owner from the build, moved by `transfer_charge`, off the ledger at the drop. Held here, not boxed,
    /// so a boxed arena costs no allocation more. Declared first, so it drops before the block. `None` for a mapped block
    /// and on wasm.
    boxed: pgsync::Mutex<Option<memledger::Charge>>,
    block: Block,
}

/// The block behind an arena. Under a huge page it is the allocator's (zeroed by it). From `MAPPED_MIN`
/// up it is an anonymous private mapping: the kernel's pages are zero and are faulted in only where the
/// arena is used, so a 64 MiB per-backend arena costs one syscall, not 64 MiB of page faults and memset
/// (the allocator's calloc of a block carved from a region the postmaster already reserved touched every
/// page: +2.6 ms on a fresh backend's first statement, docs/gravity lowered notes §7.3).
enum Block {
    /// D1703: a boxed slice's words, held raw (the allocator's zeroed block, freed as the box): every write through `ptr`
    /// and the handles derives from that pointer, never from `as_ptr` of a shared borrow (read-only under Stacked / Tree Borrows).
    Boxed { ptr: NonNull<u64>, words: usize },
    Mapped { ptr: NonNull<u64>, bytes: usize },
    /// pagepool P3 (`PGRUST_POOL_GRAVITY=1`): a pool block, or a pool span (2 MiB up to `pool_lazy_min`), zeroed by the pool
    /// and charged whole on the arena's own charge (`Arena::boxed`, `Door::Pool`), as a boxed block is.
    Pooled { ptr: NonNull<u64>, bytes: usize },
    /// pagepool P3: a lazy span (`pool_lazy_min` and up), charged as its bump passes its pages, as a mapped block is: its
    /// ledger state in header word 3 (`MappedLedger`, its charge on `Door::Pool`, no residency entry), the pool told each
    /// step (`commit_to`).
    Lazy { ptr: NonNull<u64>, bytes: usize },
}

/// pagepool P3: the pool's dead-page grain (`pagepool::give_pages` releases whole runs of it).
const POOL_RUN: usize = 64 << 10;

/// pagepool P3: an arena's block from the page pool: the server's switch (`PGRUST_POOL_GRAVITY=1`, read by the pool) with
/// the pool reserved.
#[cfg(not(target_family = "wasm"))]
#[inline]
fn pooled() -> bool {
    pagepool::on() && pagepool::switches().gravity
}

/// A program arena's pool memory: session-lived (`arena_owner`), zeroed (the arena's words past its bump read zero).
/// D4787: its length is address space (a graph arena asks 1 GiB and a short statement touches ~260 KB), so it expects to
/// touch one superpage: with `PGRUST_POOL_SPAN_TRIM=1` a warm span it takes again keeps that much committed and gives the
/// rest back rather than zero it (without the switch the pool reads no expectation under an ask's length).
#[cfg(not(target_family = "wasm"))]
const POOL_HINT: pagepool::Hint =
    pagepool::Hint::new(pagepool::Class::Session, pagepool::Tag::PROGRAM).zero(pagepool::Zero::Dense).expect_kib(2 << 10);

/// A reset keeps this much of an arena's prefix resident and hands the pages above it back to the kernel: the size every
/// ordinary statement's graph fits in (it was the whole graph arena before landing 20 made the arena an address-space
/// reservation), so the common reset is a memset, and only a statement that used more pays the page faults again.
pub const KEEP_BYTES: usize = 64 << 20;

/// The page `madvise` works in (4 KiB on every target; on a larger kernel page the call fails on a misaligned start and the
/// reset zeroes everything in place).
const PAGE: usize = 4096;

/// Hands the whole pages of `[from, to)` at or above `base + KEEP_BYTES` back to the kernel (Linux `pagegive::give_back`,
/// as `MADV_DONTNEED`: an anonymous private page reads as zero on its next touch) and zeroes the tail past the last whole page; returns where the
/// caller's in-place zeroing from `from` stops (`to` when nothing was released).
fn release_above_keep(base: usize, from: usize, to: usize) -> usize {
    let start = (base + KEEP_BYTES).max(from).next_multiple_of(PAGE);
    let end = to & !(PAGE - 1);
    if !cfg!(target_os = "linux") || end <= start {
        return to;
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: whole pages of this arena's used span, past the kept prefix; nothing references them after the reset. The
        // arena is its own read-write mapping without MAP_NORESERVE (`try_new`) and never advised huge. `pagegive`: the
        // range reads zero after, as after MADV_DONTNEED, without that call's CVE-2026-74674 shape.
        if unsafe { pagegive::give_back(start, end - start, pagegive::Mapping::Accounted { huge: false }) } != 0 {
            return to;
        }
        unsafe { core::ptr::write_bytes(end as *mut u8, 0, to - end) };
        start
    }
    #[cfg(not(target_os = "linux"))]
    unreachable!()
}

impl Block {
    fn ptr(&self) -> *mut u64 {
        match self {
            Block::Boxed { ptr, .. } | Block::Mapped { ptr, .. } | Block::Pooled { ptr, .. } | Block::Lazy { ptr, .. } => ptr.as_ptr(),
        }
    }
}

// SAFETY: a block is plain memory owned by one `Arena` (the boxed form's auto traits, kept for the mapped one).
unsafe impl Send for Block {}
unsafe impl Sync for Block {}

impl Drop for Block {
    fn drop(&mut self) {
        match *self {
            // SAFETY: the block `try_new` allocated with a `[u64]` box's layout of `words`, taken back once.
            Block::Boxed { ptr, words } => drop(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(ptr.as_ptr(), words)) }),
            // SAFETY: the mapping was made by `Arena::new` with exactly these bounds and is unmapped once.
            #[cfg(not(target_family = "wasm"))]
            Block::Mapped { ptr, bytes } => {
                // C8: the charged prefix leaves its owner's account with the mapping (D4733: its residency entry first,
                // so no pass reads the range once it is gone)
                let l = unsafe { *ptr.as_ptr().add(3) } as *mut MappedLedger;
                if !l.is_null() {
                    // SAFETY: `try_new` boxed the ledger state and put it in the header; taken back once, as the mapping goes.
                    let l = unsafe { Box::from_raw(l) };
                    memledger::resident::unregister(l.res);
                    drop(l);
                }
                unsafe { libc::munmap(ptr.as_ptr() as *mut libc::c_void, bytes) };
            }
            // wasm32-wasip1 has no mmap: `try_new` never builds a mapped block there.
            #[cfg(target_family = "wasm")]
            Block::Mapped { .. } => unreachable!("arena: no mapped blocks on wasm"),
            // SAFETY: the pool block or span `pool_block` took, given back once; its charge (`Arena::boxed`) dropped first.
            Block::Pooled { ptr, bytes } => unsafe { pagepool::give(ptr.cast(), bytes) },
            Block::Lazy { ptr, bytes } => {
                // the charged prefix leaves its owner's account before the pages go back, as a mapping's does (its ledger
                // state is a mapped block's, D4733, with no residency entry: `resident::NONE`, so the unregister is a no-op)
                let l = unsafe { *ptr.as_ptr().add(3) } as *mut MappedLedger;
                if !l.is_null() {
                    // SAFETY: `pool_block` boxed the ledger state and put it in the header; taken back once, as the span goes.
                    let l = unsafe { Box::from_raw(l) };
                    memledger::resident::unregister(l.res);
                    drop(l);
                }
                // SAFETY: the lazy span `pool_block` took, given back once.
                unsafe { pagepool::give(ptr.cast(), bytes) };
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArenaRef(NonNull<u64>);

static EMPTY_BLOCK: [u64; HDR] = [0; HDR];

impl ArenaRef {
    /// The arena of capacity zero: the allocator of a constant empty `ArenaVec` (`WritePlan::SELECT`,
    /// `SiteDesc::write`), which never allocates; a push into one is a refused allocation.
    pub const EMPTY: ArenaRef = ArenaRef(unsafe { NonNull::new_unchecked(EMPTY_BLOCK.as_ptr() as *mut u64) });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark(usize);

/// `ArenaRef::scope`: resets its arena to the mark taken when it is dropped.
pub struct Scope(ArenaRef, Mark);

impl Drop for Scope {
    fn drop(&mut self) {
        self.0.reset_to(self.1);
    }
}

const fn words(bytes: usize) -> usize {
    bytes.div_ceil(8)
}

impl ArenaRef {
    fn used_words(&self) -> usize {
        unsafe { *self.0.as_ptr() as usize }
    }

    fn set_used_words(&self, w: usize) {
        unsafe { *self.0.as_ptr() = w as u64 }
    }

    fn charged_words(&self) -> usize {
        unsafe { *self.0.as_ptr().add(1) as usize }
    }

    fn cap_words(&self) -> usize {
        unsafe { (*self.0.as_ptr().add(2) & CAP_MASK) as usize }
    }

    /// Whether the block is the pool's, as its arena recorded it (`recorded`), else `pagepool::taken`'s answer.
    #[inline]
    fn is_pool_block(&self) -> bool {
        match self.recorded_pool() {
            Some(p) => {
                debug_assert_eq!(p, pagepool::taken(self.0.as_ptr() as *const u8), "arena: a recorded pool bit is not the pool's answer");
                p
            }
            None => pagepool::taken(self.0.as_ptr() as *const u8),
        }
    }

    /// Tests: whether the block is the pool's as its arena recorded it; None when not recorded or the bit is off.
    #[doc(hidden)]
    pub fn recorded_pool(&self) -> Option<bool> {
        let w = unsafe { *self.0.as_ptr().add(2) };
        (w & RECORDED != 0 && POOLED_BIT.load(core::sync::atomic::Ordering::Relaxed)).then_some(w & POOL != 0)
    }

    /// C8: a mapped block's ledger state (`None` for a boxed one). Its user is the arena's: the header's other words are
    /// written the same way.
    #[allow(clippy::mut_from_ref)]
    fn ledger(&self) -> Option<&mut MappedLedger> {
        // SAFETY: header word 3 is null or the ledger state `try_new` boxed for this block, live until the block drops.
        unsafe { (*self.0.as_ptr().add(3) as *mut MappedLedger).as_mut() }
    }

    /// C8: a mapped block's charge (`None` for a boxed one).
    #[allow(clippy::mut_from_ref)]
    fn charge(&self) -> Option<&mut memledger::Charge> {
        self.ledger().map(|l| &mut l.charge)
    }

    /// D4733: a mapped block's residency entry set to its charge now: the charged prefix (the header's page with it), on
    /// the charge's account and owner. The bytes it grew by count as not resident until the next pass reads them.
    fn listed(&self) {
        if let Some(l) = self.ledger() {
            let charged = self.charged_words() * 8;
            memledger::resident::update_at(l.res, self.0.as_ptr() as usize, HDR * 8 + charged, charged, l.charge.acct(), l.charge.nr_home());
        }
    }

    /// G1-A door 2: room up to word `end` of a mapped block, its pages past the charged words committed on the block's
    /// charge (in `CHARGE_WORDS` steps, never past the capacity); false past the capacity, as the bump's check was.
    #[cold]
    #[inline(never)]
    fn charge_room(&self, end: usize) -> bool {
        let (was, cap) = (self.charged_words(), self.cap_words());
        if end > cap {
            return false;
        }
        let to = end.next_multiple_of(CHARGE_WORDS).min(cap);
        if let Some(c) = self.charge() {
            if self.is_pool_block() {
                self.pool_commit(was, to);
            }
            c.commit(((to - was) * 8) as u64);
        }
        unsafe { *self.0.as_ptr().add(1) = to as u64 };
        self.listed();
        true
    }

    /// pagepool P3: a lazy span's words `[was, to)` open for the bump: the dead runs there live again (`take_pages`; zeroed
    /// before `pool_reset` gave them, so they read zero whatever the purger did), and the pool told the pages are touched.
    #[cold]
    fn pool_commit(&self, was: usize, to: usize) {
        let block = self.0.cast::<u8>();
        // the span starts on a superpage, so block offsets are as aligned as addresses; never past the arena's own runs
        let (lo, hi) = (((HDR + was) * 8) & !(POOL_RUN - 1), (HDR + to) * 8);
        let end = hi.next_multiple_of(POOL_RUN).min(((HDR + self.cap_words()) * 8).next_multiple_of(POOL_RUN));
        // SAFETY: runs of this arena's own span, past its charged words: the bump writes none of them before this returns.
        unsafe { pagepool::take_pages(block, lo, end - lo) };
        // SAFETY: the arena's own live span, starting at the block.
        if let Some(mut s) = unsafe { pagepool::span_at(block) } {
            unsafe { pagepool::commit_to(&mut s, hi) };
        }
    }

    /// pagepool P3: `reset_to` of a pool block. What was used past the mark is zeroed in place (the pool keeps its pages), and
    /// a lazy span's whole runs past the kept prefix's charged step are given dead (`give_pages`, already zero): the purger
    /// may release them, and their charge leaves the owner as `release_above_keep`'s pages do.
    fn pool_reset(&self, m: Mark, from: usize, to: usize) {
        // SAFETY: `[from, to)` is the arena's used span past the mark; nothing references it after the reset.
        unsafe { core::ptr::write_bytes(from as *mut u8, 0, to - from) };
        let data = self.data() as usize;
        let start = (data + KEEP_BYTES).max(from).next_multiple_of(PAGE);
        if to > start {
            let (was, keep) = (self.charged_words(), ((start - data) / 8).next_multiple_of(CHARGE_WORDS));
            if let Some(c) = self.charge().filter(|_| keep < was) {
                // SAFETY: the arena's own span, past its charged words from here on: the bump writes there again only after
                // `pool_commit` took the runs back.
                unsafe { pagepool::give_pages(self.0.cast(), (HDR + keep) * 8, (was - keep) * 8) };
                c.give(((was - keep) * 8) as u64);
                unsafe { *self.0.as_ptr().add(1) = keep as u64 };
            }
        }
        self.set_used_words(m.0);
    }

    fn data(&self) -> *mut u64 {
        unsafe { self.0.as_ptr().add(HDR) }
    }

    fn alloc_layout(&self, layout: Layout) -> Option<NonNull<[u8]>> {
        let start = self.used_words().next_multiple_of(words(layout.align()).max(1));
        let n = words(layout.size());
        if start + n > self.charged_words() && !self.charge_room(start + n) {
            return None;
        }
        if n == 0 {
            return NonNull::new(core::ptr::slice_from_raw_parts_mut(unsafe { self.data().add(start) } as *mut u8, 0));
        }
        self.set_used_words(start + n);
        let p = unsafe { self.data().add(start) as *mut u8 };
        NonNull::new(core::ptr::slice_from_raw_parts_mut(p, n * 8))
    }

    pub fn alloc(&self, bytes: usize) -> *mut u8 {
        match self.alloc_layout(Layout::from_size_align(bytes, 8).expect("layout")) {
            Some(p) => p.as_ptr() as *mut u8,
            None => exhausted(self.used(), bytes, self.capacity()),
        }
    }

    pub fn try_alloc(&self, bytes: usize) -> Option<*mut u8> {
        self.alloc_layout(Layout::from_size_align(bytes, 8).ok()?).map(|p| p.as_ptr() as *mut u8)
    }

    pub fn offset_of(&self, p: *const u8) -> usize {
        let off = p as usize - self.data() as usize;
        assert!(off <= self.capacity(), "pointer outside the arena");
        off
    }

    pub fn zero(&self, off: usize, bytes: usize) {
        assert!(off.is_multiple_of(8) && off + bytes <= self.used(), "zero {off}+{bytes} outside the used arena");
        unsafe { core::ptr::write_bytes(self.data().add(off / 8), 0, words(bytes)) };
    }

    pub fn mark(&self) -> Mark {
        Mark(self.used_words())
    }

    /// A mark that resets the arena when dropped: on return and on an unwind alike (D2487: a statement that ran the
    /// arena out unwinds, and the next statement finds the arena as it was before it).
    pub fn scope(&self) -> Scope {
        Scope(*self, self.mark())
    }

    /// Clears what was used past `m`. Whole pages from `KEEP_BYTES` up go back to the kernel (`MADV_DONTNEED`: an
    /// anonymous private page reads as zero again on its next touch), so one large statement does not keep its arena
    /// resident for the backend's life; the rest is zeroed in place.
    pub fn reset_to(&self, m: Mark) {
        assert!(m.0 <= self.used_words(), "reset above the used arena");
        let (from, to) = (self.data().wrapping_add(m.0) as usize, self.data().wrapping_add(self.used_words()) as usize);
        if self.is_pool_block() {
            return self.pool_reset(m, from, to);
        }
        let zero_to = release_above_keep(self.data() as usize, from, to);
        unsafe { core::ptr::write_bytes(from as *mut u8, 0, zero_to - from) };
        if zero_to < to {
            // G1-A door 2: the pages handed back leave the owner's account (to the step past what stays resident)
            let (was, keep) = (self.charged_words(), ((zero_to - self.data() as usize) / 8).next_multiple_of(CHARGE_WORDS));
            if keep < was {
                if let Some(c) = self.charge() {
                    c.give(((was - keep) * 8) as u64);
                }
                unsafe { *self.0.as_ptr().add(1) = keep as u64 };
                self.listed();
            }
        }
        self.set_used_words(m.0);
    }

    pub fn capacity(&self) -> usize {
        self.cap_words() * 8
    }

    pub fn used(&self) -> usize {
        self.used_words() * 8
    }
}

/// D2487 / D3933: an exhausted arena ends the statement with an ERROR (54000), never the backend. The payload is a
/// PgError: since D2933 a plain panic is C's abort() (tcop main_loop `pg_error_from_panic`), so D2487's string panic took
/// the backend down (signal 6, a crash restart) where it meant to unwind; a PgError unwinds to the statement boundary
/// as ereport(ERROR) does, through the same catches a host's error takes.
#[cold]
#[inline(never)]
fn exhausted(used: usize, bytes: usize, capacity: usize) -> ! {
    gravity_abi::limits::exceeded(format_args!("a program arena is exhausted: {used} + {bytes} bytes of {capacity}"))
}

/// D3957: C's palloc failure (`MemoryContextAllocationFailure`): 53200 "out of memory", the request's size in the detail.
#[cold]
#[inline(never)]
fn refused(bytes: usize) -> ! {
    let total = words(bytes).saturating_add(HDR).saturating_mul(8);
    let e = types_error::PgError::error("out of memory").with_sqlstate(types_error::ERRCODE_OUT_OF_MEMORY);
    std::panic::panic_any(e.with_detail(format!("Failed on request of size {total} in a Gravity program arena.")))
}

/// C8: an arena outlives the statement that maps it (a backend's graph, node and program arenas, a worker slot's), so a
/// mapped block's charge is its session's on a backend (a holder's elsewhere), never the running statement's or portal's.
#[cfg(not(target_family = "wasm"))]
fn arena_owner() -> (memledger::Owner, memledger::Release) {
    match memledger::charge::context() {
        (memledger::Owner::Statement { acct, .. } | memledger::Owner::Portal { acct, .. }, _) => (memledger::Owner::Session(acct), memledger::Release::Session),
        o => o,
    }
}

/// pagepool P3 (`PGRUST_POOL_GRAVITY=1`): the block from the page pool, zeroed (a warm one's old pages too), with its
/// charge on the arena's owner (`Door::Pool`): a pool block or a pool span whole, on the arena's own charge; a lazy span as
/// its bump passes its pages, the charge in its header as a mapped block's, from the pages a warm one holds already. The
/// block, its whole charge and the words charged; `None` with the switch off or a refusal (the allocator's or a mapping's
/// road then).
#[cfg(not(target_family = "wasm"))]
fn pool_block(total: usize, cap: usize) -> Option<(Block, Option<memledger::Charge>, usize)> {
    if !pooled() {
        return None;
    }
    let (owner, release) = arena_owner();
    let open = |bytes: usize| memledger::Charge::open(owner, release, memledger::Kind::Arena, memledger::charge::Door::Pool, bytes as u64);
    if total < MAPPED_MIN {
        let b = pagepool::take(total, 64, POOL_HINT)?;
        let bytes = b.len();
        return Some((Block::Pooled { ptr: b.cast::<u64>(), bytes }, Some(open(bytes)), cap));
    }
    let s = pagepool::span(total, POOL_HINT)?;
    let (ptr, bytes) = (NonNull::new(s.as_ptr().cast::<u64>()).expect("pool span"), s.len());
    if !s.is_lazy() {
        return Some((Block::Pooled { ptr, bytes }, Some(open(bytes)), cap));
    }
    // a warm span's committed pages are resident (the pool zeroed them): on the charge from the start, all of them. D4784:
    // a warm span another user grew can be longer than this arena (a region's, measured 1.64-1.76 GB committed under a
    // 1 GiB graph arena): its committed pages past the capacity stay resident in the span until it drops, so they are on
    // the charge too (they were on no account); the bump's charged words stop at the capacity
    let words = (s.committed() / 8).saturating_sub(HDR).min(cap);
    let held = s.committed().saturating_sub(HDR * 8).max(words * 8);
    // header word 3 holds a mapped block's ledger state (D4733's `MappedLedger`), here with no residency entry
    // (`resident::NONE`: `listed` and the drop's unregister are no-ops): the pool's door counts its bytes, as before
    let ledger = unsafe { std::alloc::alloc(Layout::new::<MappedLedger>()) } as *mut MappedLedger; // alloc: Cold
    if ledger.is_null() {
        // SAFETY: the span just taken, unused.
        unsafe { pagepool::give_span(s) };
        return None;
    }
    // SAFETY: a fresh allocation of a `MappedLedger`'s layout, taken back by `Block::drop`; header word 3 is inside the span.
    unsafe {
        ledger.write(MappedLedger { charge: open(held), res: memledger::resident::NONE });
        *ptr.as_ptr().add(3) = ledger as u64;
    }
    Some((Block::Lazy { ptr, bytes }, None, words))
}

impl Arena {
    /// D3957: a refused block (the allocator's or the kernel's) is the statement's ERROR 53200, as C's palloc failure is,
    /// never the backend's abort (`exhausted`'s road). A caller with state to unwind first takes `try_new`'s `None`.
    pub fn new(bytes: usize) -> Arena {
        Arena::try_new(bytes).unwrap_or_else(|| refused(bytes))
    }

    /// `new`, with a refused block as `None`, never a panic: a refused mapping (an address-space rail: `RLIMIT_AS`,
    /// `vm.overcommit_memory = 2`), a boxed block the global allocator refused, or a size past the address space. The caller
    /// refuses the work by name (D2845: exec's graph arena, mapped before the statement's grant).
    pub fn try_new(bytes: usize) -> Option<Arena> {
        Arena::try_new_in(bytes, true)
    }

    /// Tests: `try_new` with the pool's take refused: the allocator's or a mapping's road, as when the pool is full.
    #[doc(hidden)]
    pub fn try_new_off_pool(bytes: usize) -> Option<Arena> {
        Arena::try_new_in(bytes, false)
    }

    fn try_new_in(bytes: usize, pool: bool) -> Option<Arena> {
        #[cfg(target_family = "wasm")]
        let _ = pool;
        let cap = words(bytes);
        let total = HDR.checked_add(cap)?.checked_mul(8)?;
        // wasm32-wasip1 has no mmap: every arena is a boxed block, its allocation fallible (a refused grow of linear
        // memory is the address-space rail's `None`).
        #[cfg(target_family = "wasm")]
        let (block, charged) = {
            let mut v: Vec<u64> = Vec::new();
            v.try_reserve_exact(HDR + cap).ok()?;
            v.resize(HDR + cap, 0);
            let b = Box::into_raw(v.into_boxed_slice());
            (Block::Boxed { ptr: NonNull::new(b.cast::<u64>()).expect("box"), words: HDR + cap }, cap)
        };
        #[cfg(target_family = "wasm")]
        let boxed = None;
        #[cfg(not(target_family = "wasm"))]
        let mut boxed = None;
        #[cfg(not(target_family = "wasm"))]
        let pool = if pool { pool_block(total, cap) } else { None };
        #[cfg(not(target_family = "wasm"))]
        let (block, charged) = if let Some((block, whole, charged)) = pool {
            boxed = whole;
            (block, charged)
        } else if total < MAPPED_MIN {
            // SAFETY: a nonzero layout (HDR words at least), a `[u64]` box's: `Block::drop` frees it as that box.
            let b = unsafe { std::alloc::alloc_zeroed(Layout::array::<u64>(HDR + cap).ok()?) };
            let block = Block::Boxed { ptr: NonNull::new(b.cast::<u64>())?, words: HDR + cap };
            // D4518: door 1 counted the block on this thread's account; its charge names the owner, whole, and moves with
            // the arena as a mapping's pages do (a parked worker set's to the depot, a filed program's to the program
            // caches). With none, a backend's parked Node and the depot's worker sets kept their boxed images on no owner.
            let (owner, release) = arena_owner();
            boxed = Some(memledger::Charge::open(owner, release, memledger::Kind::Arena, memledger::charge::Door::Heap, total as u64));
            (block, cap)
        } else {
            // SAFETY: an anonymous private mapping of `total` bytes, zero-filled by the kernel on first touch.
            let p = unsafe { libc::mmap(core::ptr::null_mut(), total, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0) };
            if p == libc::MAP_FAILED {
                return None;
            }
            mempolicy::maps::name(p as *mut u8, total, c"pgrust:arena");
            let ledger = unsafe { std::alloc::alloc(Layout::new::<MappedLedger>()) } as *mut MappedLedger; // alloc: Cold
            if ledger.is_null() {
                unsafe { libc::munmap(p, total) };
                return None;
            }
            let (owner, release) = arena_owner();
            let charge = memledger::Charge::open(owner, release, memledger::Kind::Arena, memledger::charge::Door::Mapped, 0);
            // D4733: listed with nothing charged yet (`listed` follows each step of the charge)
            let res = memledger::resident::register_at(p as usize, HDR * 8, 0, charge.acct(), true, false, charge.nr_home());
            // SAFETY: a fresh allocation of a `MappedLedger`'s layout; `Block::drop` takes it back as a box.
            unsafe { ledger.write(MappedLedger { charge, res }) };
            unsafe { *(p as *mut u64).add(3) = ledger as u64 };
            (Block::Mapped { ptr: NonNull::new(p as *mut u64)?, bytes: total }, 0)
        };
        // SAFETY: the block holds at least HDR words. A boxed or pooled block is charged whole (`boxed`: door 1, the pool's
        // door), a mapped one from nothing, a lazy span from the pages a warm one holds.
        let from_pool = matches!(block, Block::Pooled { .. } | Block::Lazy { .. });
        unsafe {
            *block.ptr().add(2) = cap as u64 | recorded(from_pool);
            *block.ptr().add(1) = charged as u64;
        }
        Some(Arena { boxed: pgsync::Mutex::new(boxed), block })
    }

    /// C8: a mapped block's pages move to owner `to`, released at `release` (a filed program's arenas go to the program
    /// caches, a parked worker slot's to the depot and back); D4518: a boxed block's charge, whole, the same way (nothing
    /// on wasm, where door 1 has it).
    pub fn transfer_charge(&self, to: memledger::Owner, release: memledger::Release) {
        let h = self.handle();
        if let Some(c) = h.charge() {
            c.transfer(to, release);
            // D4733: its residency entry (its not-resident bytes) goes with the charge
            h.listed();
        } else if let Some(c) = pgsync::lock(&self.boxed).as_mut() {
            c.transfer(to, release);
        }
    }

    /// C8: a mapped block's pages move to the calling context's owner, as a mapping made here would be charged (a worker
    /// slot set's arena taken from the depot by the next statement's backend).
    #[cfg(not(target_family = "wasm"))]
    pub fn charge_here(&self) {
        let (to, release) = arena_owner();
        self.transfer_charge(to, release);
    }

    pub fn handle(&self) -> ArenaRef {
        ArenaRef(NonNull::new(self.block.ptr()).expect("arena block"))
    }

    /// G1-A: what the memory ledger has on account for the block, and whether the arena's own charge has it (it moves
    /// with `transfer_charge`): a mapped block's charged prefix (kind `Arena`, its drop's discharge), a boxed block whole
    /// (the allocation's size; D4518: on the arena's charge, door 1's alone on wasm). The program caches move a filed
    /// program's arenas to their account by it (`Program::file_on_ledger`).
    pub fn ledger_bytes(&self) -> (u64, bool) {
        match self.block {
            Block::Boxed { words, .. } => ((words * 8) as u64, pgsync::lock(&self.boxed).is_some()),
            Block::Pooled { bytes, .. } => (bytes as u64, true),
            Block::Mapped { .. } => ((self.handle().charged_words() * 8) as u64, true),
            // D4784: a lazy span's charge also holds a warm span's committed pages past the capacity (`pool_block`)
            Block::Lazy { .. } => (self.handle().charge().map_or((self.handle().charged_words() * 8) as u64, |c| c.committed()), true),
        }
    }

    pub fn bytes(&self, upto: usize) -> &[u8] {
        assert!(upto <= self.handle().used());
        unsafe { core::slice::from_raw_parts(self.block.ptr().add(HDR) as *const u8, upto) }
    }
}

unsafe impl Allocator for ArenaRef {
    /// D2487: an exhausted arena unwinds (`exhausted`: the statement ends as an ERROR); an `AllocError` here reached
    /// `handle_alloc_error` in the arena collections, which aborts the server.
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        match self.alloc_layout(layout) {
            Some(p) => Ok(p),
            None => exhausted(self.used(), layout.size(), self.capacity()),
        }
    }

    unsafe fn deallocate(&self, _ptr: NonNull<u8>, _layout: Layout) {}

    /// Landing 20: the arena's last block grows in place (a vector pushed past its capacity while nothing was
    /// allocated after it takes only the added words); any other block moves, as the default does. Without this every
    /// doubling left the old buffer behind, so a vector cost twice its final size of the fixed arena.
    unsafe fn grow(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let off = ptr.as_ptr() as usize - self.data() as usize;
        let top = off.is_multiple_of(8) && off / 8 + words(old.size()) == self.used_words();
        if old.size() > 0 && top && (ptr.as_ptr() as usize).is_multiple_of(new.align()) && (off / 8 + words(new.size()) <= self.charged_words() || self.charge_room(off / 8 + words(new.size()))) {
            // the words past the old block are unused, so zero (`reset_to` zeroes what it frees)
            self.set_used_words(off / 8 + words(new.size()));
            return Ok(NonNull::slice_from_raw_parts(ptr, words(new.size()) * 8));
        }
        let p = self.allocate(new)?;
        // SAFETY: the old block holds `old.size()` initialized bytes and the new one is a fresh, disjoint block
        unsafe { core::ptr::copy_nonoverlapping(ptr.as_ptr(), p.as_ptr() as *mut u8, old.size()) };
        Ok(p)
    }
}

#[cfg(test)]
mod door_tests {
    use super::*;

    /// G1-A door 2: a mapped arena charges the current account as its bump passes each step, a reset below the kept
    /// prefix keeps the charge (the pages stay resident), and the drop takes it all off.
    #[test]
    fn mapped_arena_charges_its_bumped_prefix() {
        let a = memledger::Acct::backend(4090);
        let _in = memledger::charge::enter_as(memledger::Owner::Session(a));
        let base = memledger::account_row(a).mapped_bytes;
        {
            let arena = Arena::new(16 << 20);
            let h = arena.handle();
            assert_eq!(memledger::account_row(a).mapped_bytes, base, "a fresh mapping has nothing on account");
            let m = h.mark();
            h.alloc((3 << 20) - 4096);
            let charged = memledger::account_row(a).mapped_bytes - base;
            assert_eq!(charged, 3 << 20, "charged to the step past the bump");
            h.alloc(100);
            assert_eq!(memledger::account_row(a).mapped_bytes - base, charged, "inside the charged step: no new charge");
            h.reset_to(m);
            assert_eq!(memledger::account_row(a).mapped_bytes - base, charged, "below KEEP_BYTES the pages stay, and so does the charge");
            assert!(h.try_alloc((16 << 20) + 8).is_none(), "past the capacity: refused, nothing charged");
            assert_eq!(memledger::account_row(a).mapped_bytes - base, charged);
        }
        assert_eq!(memledger::account_row(a).mapped_bytes, base, "the drop discharges the charged prefix");
        // a boxed (small) arena is the allocator's: door 2 never charges it
        let small = Arena::new(4096);
        small.handle().alloc(4000);
        assert_eq!(memledger::account_row(a).mapped_bytes, base);
        assert!(ArenaRef::EMPTY.try_alloc(8).is_none());
    }

    /// C8: a mapped arena's pages are its owner's. Stepped under one backend's account and then reset past the kept
    /// prefix and dropped under another's (a parked worker slot's arena that the next backend takes), its pages leave the
    /// account they were charged to, and the account that frees them moves by nothing.
    #[test]
    fn an_arena_stepped_under_one_account_leaves_it_when_another_resets_and_drops_it() {
        std::thread::spawn(|| {
            let (a, b) = (memledger::Acct::backend(4070), memledger::Acct::backend(4071));
            assert!(a.is_backend() && b.is_backend());
            memledger::bind_backend(4070, 9070);
            let mapped = |x: memledger::Acct| {
                memledger::fold();
                memledger::account_row(x).mapped_bytes
            };
            let (ba, bb) = (mapped(a), mapped(b));
            let arena = Arena::new(80 << 20);
            let h = arena.handle();
            let m = h.mark();
            h.alloc((70 << 20) - 4096);
            assert_eq!((mapped(a) - ba, mapped(b) - bb), (70 << 20, 0), "stepped under a");
            {
                let _in = memledger::charge::enter_as(memledger::Owner::Session(b));
                h.reset_to(m);
            }
            // what stays charged: the kept prefix, to the step past it (Linux hands the rest back; elsewhere none)
            let kept = (h.charged_words() * 8) as i64;
            assert!(kept < 70 << 20 || !cfg!(target_os = "linux"), "the reset handed pages back ({kept} bytes kept)");
            assert_eq!((mapped(a) - ba, mapped(b) - bb), (kept, 0), "reset under b: the pages handed back leave a");
            {
                let _in = memledger::charge::enter_as(memledger::Owner::Session(b));
                drop(arena);
            }
            assert_eq!((mapped(a) - ba, mapped(b) - bb), (0, 0), "dropped under b: the rest leaves a, b never held it");
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }

    /// D4733: a mapped arena's charged prefix is on the residency registry. The bump passes 8 MiB and only the first MiB
    /// is written: a pass reads the rest as not resident on the arena's account, so the account's resident door-2 bytes
    /// (`mapped - not_resident_mapped`) are the written MiB (within a huge page), not the 8 MiB charged. Its charge's move
    /// takes the not-resident bytes along, and the drop takes them off. Before, the arena was on no entry: the account
    /// called all 8 MiB resident, which a kept program's image (laid out whole, mostly never written) made a drift.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_mapped_arenas_unwritten_pages_are_not_resident_on_its_account() {
        std::thread::spawn(|| {
            // b: a cache's account (no other test of this crate charges it), where a kept arena's charge can move
            let (a, b) = (memledger::Acct::backend(4074), memledger::Acct::of(memledger::Consumer::CondCache));
            memledger::bind_backend(4074, 9074);
            let row = |x: memledger::Acct| {
                memledger::fold();
                let r = memledger::account_row(x);
                (r.mapped_bytes, r.not_resident_mapped_bytes)
            };
            let pass = || assert!(memledger::resident::pass(pagegive::resident_bytes, usize::MAX).is_some());
            let (ma, na) = row(a);
            let (mb, nb) = row(b);
            let arena = Arena::new(16 << 20);
            let h = arena.handle();
            let p = h.alloc(8 << 20);
            // SAFETY: the first MiB of the 8 MiB just allocated
            unsafe { core::ptr::write_bytes(p, 1, 1 << 20) };
            pass();
            let (m, n) = row(a);
            let (charged, resident) = (m - ma, (m - ma) - (n - na));
            assert_eq!(charged, 8 << 20, "charged to the step past the bump");
            assert!((1 << 20..=(1 << 20) + (2 << 20)).contains(&resident), "resident {resident} B of {charged} charged: the written MiB");
            arena.transfer_charge(memledger::Owner::Cache(memledger::Holder::of_acct(b)), memledger::Release::Evict);
            pass();
            let ((m2, n2), (m3, n3)) = (row(a), row(b));
            assert_eq!((m2 - ma, n2 - na), (0, 0), "moved off a whole");
            assert_eq!((m3 - mb, (m3 - mb) - (n3 - nb)), (charged, resident), "on b with its residency");
            drop(arena);
            assert_eq!((row(a), row(b)), ((ma, na), (mb, nb)), "the drop takes it off");
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }
}
