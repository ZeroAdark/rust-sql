//! LEDGER2 step 3: the operator arena's implementation (the module doc is `arena`'s): every raw read and write of a
//! chunk lives here.
#![allow(unsafe_code)]

use core::alloc::Layout;
use core::cell::{Cell, RefCell};
use core::hash::{BuildHasher, Hash};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::rc::Rc;

use allocator_api2::alloc::{AllocError, Allocator};
use gravity_abi::Ctx;
use memledger::charge::Door;
use memledger::{Charge, Owner, Release};

use gravity_abi::program::{ARENA_CHUNK_FIRST, ARENA_CHUNK_MAX};
pub use mcx::ForgetSafe;
pub use memledger::charge::Usage;
pub use memledger::Kind;

/// A chunk the host (or, with no host, the allocator) refused, or a size past what an arena holds: the bytes asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused {
    pub bytes: u64,
}

/// The chunks an arena's core holds inline; a draw past them goes to its overflow table (`Core::more`, on the allocator).
pub const CHUNKS: usize = 128;
/// The chunks one arena holds at most: as many as `ABytes` can name (its chunk index is a `u16`), not a tuned bound. Every
/// chunk past the 11th is at least `ARENA_CHUNK_MAX` (64 MiB), so the table is full only past 4 TiB held, beyond any
/// budget: a draw is refused by the host's road (the ladder / broker) or by the allocator, never by the table.
const MAX_CHUNKS: usize = u16::MAX as usize + 1;
/// A chunk the allocator gives with no host (a kernel test's rig) is aligned to this.
const RIG_ALIGN: usize = 64;
/// pagepool P3 (`PGRUST_POOL_GRAVITY=1`): an operator's chunk from the page pool, statement-lived, never zeroed.
const POOL_HINT: pagepool::Hint = pagepool::Hint::new(pagepool::Class::Task, pagepool::Tag::OP);

/// pagepool P3: the allocator road's chunks come from the page pool (the server's switch, read by the pool, with the pool
/// reserved).
#[inline]
fn pooled() -> bool {
    pagepool::on() && pagepool::switches().gravity
}

/// The arena id below a growable buffer.
const HEADER: usize = 8;
/// An `AVec`'s capacity word: its capacity below this bit, its arena's tag from it up. A `u64` on every target (wasm32's
/// `usize` holds no bit 48).
const TAG_SHIFT: u32 = 48;
const CAP_MASK: u64 = (1 << TAG_SHIFT) - 1;

/// Arena generations: an arena takes a new id at its creation and at each reset; the low 16 bits (its tag) are never 0.
static IDS: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    loop {
        let id = IDS.fetch_add(1, Relaxed);
        if id as u16 != 0 {
            return id;
        }
    }
}

#[derive(Clone, Copy)]
struct Chunk {
    base: usize,
    len: usize,
    host: bool,
}

const NO_CHUNK: Chunk = Chunk { base: 0, len: 0, host: false };

struct Core {
    ctx: *mut Ctx,
    owner: Owner,
    /// `op_begin` made the owner for this arena (its drop ends it); false for a worker arena or a begin past the table.
    began: bool,
    /// LEDGER2 step 5: a scratch arena (`OpArena::scratch`): its chunks are the allocator's whatever the host, so a buffer
    /// that took the allocator before keeps its road (never refused where it was not).
    heap_only: bool,
    /// CONVERT1: the chunks are charged to `owner` (false: `OpArena::scratch_of` with no owner: they stay on the allocating
    /// thread's account, as an allocator `Vec`'s bytes do, and `owner` is `UNCHARGED`).
    charged: bool,
    release: Release,
    kind: Kind,
    id: Cell<u64>,
    cur: Cell<usize>,
    end: Cell<usize>,
    next: Cell<usize>,
    n: Cell<usize>,
    chunks: [Cell<Chunk>; CHUNKS],
    /// The chunk bytes held now, the chunks drawn since the arena's creation.
    held: Cell<u64>,
    draws: Cell<u64>,
    /// With no host, the allocator's chunks as the owner's door-1 charge; and this core's own heap bytes (with `more`'s).
    rig: RefCell<Charge>,
    shell: RefCell<Charge>,
    /// pagepool P3: with the switch on (`pooled`: a charged arena), the pool's chunks as the owner's `Door::Pool` charge
    /// (`NONE` else: the allocator's road alone).
    pooled: bool,
    pool: RefCell<Charge>,
    /// The chunks past the inline `CHUNKS` (chunk `CHUNKS + i` at `i`); empty, no allocation, until a draw needs it. Its
    /// capacity stays across a reset and is freed after `shell` drops (the charge back on the thread before the free).
    more: RefCell<Vec<Chunk>>,
}

#[cold]
#[inline(never)]
fn wrong_arena() -> ! {
    panic!("gravity_fn::arena: a buffer used with an arena that does not own it (another arena, or this one after a reset)")
}

impl Core {
    #[inline]
    fn tag(&self) -> usize {
        self.id.get() as u16 as usize
    }

    /// Chunk `i` (below `n`): the inline table's, else the overflow table's.
    #[inline]
    fn chunk(&self, i: usize) -> Chunk {
        match self.chunks.get(i) {
            Some(c) => c.get(),
            None => self.more.borrow()[i - CHUNKS],
        }
    }

    /// Room for one more entry in the overflow table (doubling, at least `CHUNKS` entries), its bytes the owner's when the
    /// arena is charged; false only when the allocator refuses.
    #[cold]
    fn more_room(&self) -> bool {
        let mut m = self.more.borrow_mut();
        if m.len() < m.capacity() {
            return true;
        }
        let was = m.capacity();
        // the table's growth, charged to `self.owner` below (the fallible form: a refusal is the allocator's)
        if m.try_reserve(was.max(CHUNKS)).is_err() {
            return false;
        }
        if self.charged {
            self.shell.borrow_mut().commit(((m.capacity() - was) * core::mem::size_of::<Chunk>()) as u64);
        }
        true
    }

    /// `size` bytes at `align` (a power of two) from the newest chunk, a new chunk when it has no room.
    #[inline]
    fn carve(&self, size: usize, align: usize) -> Result<usize, Refused> {
        let p = self.cur.get().wrapping_add(align - 1) & !(align - 1);
        match p.checked_add(size) {
            Some(e) if p >= self.cur.get() && e <= self.end.get() => {
                self.cur.set(e);
                Ok(p)
            }
            _ => self.carve_new(size, align),
        }
    }

    #[cold]
    fn carve_new(&self, size: usize, align: usize) -> Result<usize, Refused> {
        let refused = Refused { bytes: size as u64 };
        let need = size.checked_add(align).ok_or(refused)?;
        let want = need.max(self.next.get()).checked_next_multiple_of(4096).ok_or(refused)?;
        let (base, len) = self.draw(want)?;
        self.next.set(self.next.get().saturating_mul(2).min(ARENA_CHUNK_MAX));
        let p = (base + align - 1) & !(align - 1);
        self.cur.set(p + size);
        self.end.set(base + len);
        Ok(p)
    }

    /// A chunk of `want` bytes: the host's (charged to the owner there), else the allocator's (charged here; pagepool P3: the
    /// pool's with the switch on, all of what it hands out, `want` or more). Its base and length.
    fn draw(&self, want: usize) -> Result<(usize, usize), Refused> {
        let refused = Refused { bytes: want as u64 };
        let i = self.n.get();
        // the entry's room first: a chunk is never taken that the table cannot hold
        if i == MAX_CHUNKS || (i >= CHUNKS && !self.more_room()) {
            return Err(refused);
        }
        // SAFETY: `ctx` is null or the ctx this arena was made on, live for the arena's life (the module doc).
        // SAFETY: as above.
        let asked = if self.heap_only { None } else { unsafe { gravity_abi::program::arena_chunk(self.ctx, want as u64, self.owner.words()) } };
        let pool = if asked.is_none() && self.pooled { pagepool::take(want, RIG_ALIGN, POOL_HINT) } else { None };
        let (base, len, host) = match (asked, pool) {
            (Some(p), _) if p.is_null() => return Err(refused),
            (Some(p), _) => (p as usize, want, true),
            (None, Some(b)) => {
                self.pool.borrow_mut().commit(b.len() as u64);
                (b.cast::<u8>().as_ptr() as usize, b.len(), false)
            }
            (None, None) => {
                let l = Layout::from_size_align(want, RIG_ALIGN).map_err(|_| refused)?;
                // SAFETY: a non-zero-size layout; a null return is the allocator's refusal.
                let p = unsafe { std::alloc::alloc(l) };
                if p.is_null() {
                    return Err(refused);
                }
                if self.charged {
                    self.rig.borrow_mut().commit(want as u64);
                }
                (p as usize, want, false)
            }
        };
        let c = Chunk { base, len, host };
        match self.chunks.get(i) {
            Some(slot) => slot.set(c),
            // within the room `more_room` made: no allocation
            None => self.more.borrow_mut().push(c),
        }
        self.n.set(i + 1);
        self.held.set(self.held.get() + len as u64);
        self.draws.set(self.draws.get() + 1);
        Ok((base, len))
    }

    /// Every chunk back (the host's to it, the allocator's freed), newest first; the bump window empty. The overflow
    /// table keeps its capacity.
    fn give_all(&self) {
        // taken out while the host is called (no borrow held across it), put back empty with its capacity
        let mut more = core::mem::take(&mut *self.more.borrow_mut());
        while let Some(c) = more.pop() {
            self.give(c);
        }
        *self.more.borrow_mut() = more;
        for i in (0..self.n.get().min(CHUNKS)).rev() {
            let c = self.chunks[i].replace(NO_CHUNK);
            self.give(c);
        }
        self.n.set(0);
        self.cur.set(0);
        self.end.set(0);
        self.next.set(ARENA_CHUNK_FIRST);
        self.held.set(0);
    }

    /// One chunk back: the host's to it, the allocator's freed, then its charge off the owner (D4536: the bytes stay on
    /// their owner until they are gone; uncharged first, a teardown's in-flight chunks read as unattributed).
    fn give(&self, c: Chunk) {
        if c.host {
            // SAFETY: a chunk `arena_chunk` gave this arena on `ctx`; nothing reads it after the reset or drop.
            unsafe { gravity_abi::program::arena_give(self.ctx, c.base as *mut u8) };
        } else if pagepool::taken(c.base as *const u8) {
            // SAFETY: a chunk `draw` took from the pool, given back once (its entry cleared or popped by the caller).
            unsafe { pagepool::give(NonNull::new_unchecked(c.base as *mut u8), c.len) };
            self.pool.borrow_mut().give(c.len as u64);
        } else {
            // SAFETY: allocated in `draw` with this layout, freed once (its entry cleared or popped by the caller).
            unsafe { std::alloc::dealloc(c.base as *mut u8, Layout::from_size_align_unchecked(c.len, RIG_ALIGN)) };
            self.rig.borrow_mut().free(c.len as u64);
        }
    }

    /// A growable buffer of `size` bytes at `align`, the arena's id in the 8 bytes below it.
    fn alloc_tagged(&self, size: usize, align: usize) -> Result<NonNull<u8>, Refused> {
        let a = align.max(HEADER);
        let p = self.carve(size.checked_add(a).ok_or(Refused { bytes: size as u64 })?, a)? + a;
        // SAFETY: `[p - a, p + size)` was just carved from a live chunk; the header word is inside it, 8-aligned.
        unsafe { ((p - HEADER) as *mut u64).write(self.id.get()) };
        // SAFETY: inside a chunk, never 0.
        Ok(unsafe { NonNull::new_unchecked(p as *mut u8) })
    }

    /// Panics unless `p` is a growable buffer of this arena's generation (the header below it).
    #[inline]
    fn check(&self, p: NonNull<u8>) {
        // SAFETY: the caller's tag check passed (or `p` came from this arena's allocator handle): `p` is a buffer this
        // arena carved, inside a chunk it still holds, its header readable.
        if unsafe { ((p.as_ptr() as usize - HEADER) as *const u64).read() } != self.id.get() {
            wrong_arena();
        }
    }

    /// A growable buffer given back: the bump rewinds when it is the newest carve; its header cleared.
    fn free_tagged(&self, p: NonNull<u8>, size: usize, align: usize) {
        let a = align.max(HEADER);
        let at = p.as_ptr() as usize;
        // SAFETY: as `check`: the header is inside a chunk this arena holds.
        unsafe { ((at - HEADER) as *mut u64).write(0) };
        if at + size == self.cur.get() {
            self.cur.set(at - a);
        }
    }

    /// The buffer at `p` (`size` bytes, the newest carve) extended in place to `to` bytes when the chunk has the room.
    fn extend_top(&self, p: NonNull<u8>, size: usize, to: usize) -> bool {
        let at = p.as_ptr() as usize;
        match at.checked_add(to) {
            Some(e) if at + size == self.cur.get() && e <= self.end.get() => {
                self.cur.set(e);
                true
            }
            _ => false,
        }
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        self.give_all();
        if self.began {
            memledger::charge::op_end(self.owner);
        }
    }
}

/// An operator's arena (see the module doc). Not `Send`: a worker draws from its own (`for_worker`).
pub struct OpArena {
    core: Rc<Core>,
}

/// What a worker thread needs to open an arena charged to the same owner (`OpArena::for_worker`).
#[derive(Clone, Copy, Debug)]
pub struct ArenaSeed {
    owner: Owner,
    release: Release,
    kind: Kind,
    heap_only: bool,
    charged: bool,
}

impl ArenaSeed {
    /// The worker's arena on its own `ctx`: per-thread chunks, each charged to the seed's owner once, no per-allocation
    /// atomics; its chunks go back at its reset or drop (the owner stays its maker's). CONVERT1: a scratch arena's seed
    /// keeps the allocator's road (and an uncharged one charges no one), as its maker's did.
    pub fn arena(self, ctx: *mut Ctx) -> OpArena {
        OpArena::make(ctx, self.owner, false, self.release, self.kind, self.heap_only, self.charged)
    }
}

/// CONVERT1: an uncharged arena's `owner` (`OpArena::scratch_of(None, ..)`): no charge of it is ever opened.
pub const UNCHARGED: Owner = Owner::Cache(memledger::Holder::Unbound);

impl OpArena {
    /// An operator's arena on `ctx` (null: no host, the allocator's chunks): a new ledger owner of kind `kind` with `budget`
    /// bytes promised (its share), under the owner the calling thread charges now (its portal, statement or session).
    pub fn new(ctx: *mut Ctx, kind: Kind, budget: u64) -> OpArena {
        let (parent, release) = memledger::charge::context();
        let owner = memledger::charge::op_begin(parent, kind as u32, budget);
        OpArena::make(ctx, owner, owner != parent, release, kind, false, true)
    }

    /// LEDGER2 step 5: a kernel call's scratch arena (a sort's copy, index or bitmap that took the allocator): a new owner of
    /// kind `kind` under the breaker slot the calling program block's pipeline runs for (the host's answer,
    /// `gravity_abi::program::arena_slot_owner`), its chunks the allocator's, each charged to the owner once, never the
    /// host's (no road changes, no refusal past the allocator's own). `None` with no host or no slot owner: the caller keeps
    /// its allocator buffer, as before (its bytes stay on the account's unattributed row).
    pub fn scratch(ctx: *mut Ctx, kind: Kind) -> Option<OpArena> {
        let parent = OpArena::slot_owner(ctx)?;
        let owner = memledger::charge::op_begin(parent, kind as u32, 0);
        // the owner's own release: a portal's when `op_begin` gave the parent back (its table full) and the parent is one
        Some(OpArena::make(ctx, owner, owner != parent, memledger::charge::release_of(owner), kind, true, true))
    }

    /// CONVERT1: the ledger owner of the breaker slot `ctx`'s pipeline runs for (the host's answer,
    /// `gravity_abi::program::arena_slot_owner`); `None` with no host or no slot owner. CONVERT wave 2 (R3): the slot the
    /// pipeline produces into, else the statement's (or the portal's) owner.
    pub fn slot_owner(ctx: *mut Ctx) -> Option<Owner> {
        // SAFETY: `ctx` is null or the caller's bound ctx, its program block live for the call.
        Owner::from_words(unsafe { gravity_abi::program::arena_slot_owner(ctx) }?)
    }

    /// CONVERT wave 2: `slot_owner` and the ledger kind its bytes count under (the host's reply word 0,
    /// `gravity_abi::program::arena_slot_owner_kind`): the produced slot's breaker kind, `Kind::Region` for the statement's
    /// owner; as `Window::op_owner` answers a source. `None` as `slot_owner`, or a kind no `Kind` names.
    pub fn slot_owner_kind(ctx: *mut Ctx) -> Option<(Owner, Kind)> {
        // SAFETY: as `slot_owner`.
        let (w, k) = unsafe { gravity_abi::program::arena_slot_owner_kind(ctx) }?;
        Some((Owner::from_words(w)?, *Kind::ALL.get(usize::try_from(k).ok()?)?))
    }

    /// CONVERT wave 2 (wave2-base commit 7): the owner and ledger kind of the breaker slot `ctx`'s pipeline OWNS (the host's
    /// answer, `gravity_abi::program::arena_own_owner_kind`): the slot it produces into, else the first slot it consumes (an
    /// emit's: the breaker it reads), the owner the pipeline's grant-road blocks charge (the driver's `Inner::op_owner`).
    /// For a kernel's allocator-road scratch that sits beside those blocks (a sort's or an emit's `Work`). `None` with no
    /// host, no such slot, no owner of it, or a kind no `Kind` names; never the statement's.
    pub fn own_owner_kind(ctx: *mut Ctx) -> Option<(Owner, Kind)> {
        // SAFETY: as `slot_owner`.
        let (w, k) = unsafe { gravity_abi::program::arena_own_owner_kind(ctx) }?;
        Some((Owner::from_words(w)?, *Kind::ALL.get(usize::try_from(k).ok()?)?))
    }

    /// CONVERT1: an operator call's working memory on the allocator's road, charged to `owner` ITSELF (the operator's own
    /// row and kind, no child owner begun: nothing to end, no operator cell taken per call). Its chunks are the
    /// allocator's, each charged to the owner once at its draw and given back at the arena's drop: no road changes, no
    /// refusal past the allocator's own. `None` (the slot has no owner: the operator table was full, or no host, a rig):
    /// the chunks charge no one and stay on the allocating thread's account, where an allocator `Vec`'s bytes sit (its
    /// unattributed row); nothing is inferred from the running thread.
    pub fn scratch_of(owner: Option<Owner>, kind: Kind) -> OpArena {
        let (o, charged) = owner.map_or((UNCHARGED, false), |o| (o, true));
        // the owner's own release (a cursor's portal holds its scratch past the statement that made it)
        let release = if charged { memledger::charge::release_of(o) } else { Release::Statement };
        OpArena::make(core::ptr::null_mut(), o, false, release, kind, true, charged)
    }

    fn make(ctx: *mut Ctx, owner: Owner, began: bool, release: Release, kind: Kind, heap_only: bool, charged: bool) -> OpArena {
        let core = Rc::new(Core {
            ctx,
            owner,
            began,
            heap_only,
            charged,
            release,
            kind,
            id: Cell::new(next_id()),
            cur: Cell::new(0),
            end: Cell::new(0),
            next: Cell::new(ARENA_CHUNK_FIRST),
            n: Cell::new(0),
            chunks: [const { Cell::new(NO_CHUNK) }; CHUNKS],
            held: Cell::new(0),
            draws: Cell::new(0),
            rig: RefCell::new(if charged { Charge::open(owner, release, kind, Door::Heap, 0) } else { Charge::NONE }),
            shell: RefCell::new(Charge::NONE),
            // pagepool P3: an uncharged arena keeps the allocator's road (its bytes on the allocating thread, as door 1's)
            pooled: charged && pooled(),
            pool: RefCell::new(if charged && pooled() { Charge::open(owner, release, kind, Door::Pool, 0) } else { Charge::NONE }),
            more: RefCell::new(Vec::new()), // alloc: Exec charged owner
        });
        // the core's own heap bytes (the Rc's block) are the owner's too
        if charged {
            let shell = core::mem::size_of::<Core>() + 2 * core::mem::size_of::<usize>();
            *core.shell.borrow_mut() = Charge::open(owner, release, kind, Door::Heap, shell as u64);
        }
        OpArena { core }
    }

    /// A child arena on the same ctx: a new owner under this arena's (a build's partition, a merge's run).
    pub fn child(&self, kind: Kind, budget: u64) -> OpArena {
        let c = &*self.core;
        let owner = memledger::charge::op_begin(c.owner, kind as u32, budget);
        OpArena::make(c.ctx, owner, owner != c.owner, c.release, kind, c.heap_only, c.charged)
    }

    /// The seed of a worker's arena charged to this arena's owner (`ArenaSeed::arena` on the worker's thread and ctx).
    pub fn for_worker(&self) -> ArenaSeed {
        ArenaSeed { owner: self.core.owner, release: self.core.release, kind: self.core.kind, heap_only: self.core.heap_only, charged: self.core.charged }
    }

    /// The arena's ledger owner and its numbers now (memledger `usage`: held, budget, spilled, its subtree's).
    pub fn owner(&self) -> Owner {
        self.core.owner
    }

    pub fn usage(&self) -> Usage {
        memledger::charge::usage(self.core.owner)
    }

    /// The owner's budget (its share) is `bytes` now.
    pub fn set_budget(&self, bytes: u64) {
        memledger::charge::op_budget(self.core.owner, bytes);
    }

    /// The chunk bytes this arena holds now, and the chunks it drew since its creation.
    pub fn chunk_bytes(&self) -> u64 {
        self.core.held.get()
    }

    pub fn chunk_draws(&self) -> u64 {
        self.core.draws.get()
    }

    /// `layout`'s bytes, uninitialized: a raw carve (no header; it never grows).
    pub fn alloc(&self, layout: Layout) -> Result<NonNull<u8>, Refused> {
        let p = self.core.carve(layout.size().max(1), layout.align())?;
        // SAFETY: inside a chunk, never 0.
        Ok(unsafe { NonNull::new_unchecked(p as *mut u8) })
    }

    /// `alloc`, zeroed.
    pub fn alloc_zeroed(&self, layout: Layout) -> Result<NonNull<u8>, Refused> {
        let p = self.alloc(layout)?;
        // SAFETY: `layout.size()` bytes just carved at `p`.
        unsafe { p.as_ptr().write_bytes(0, layout.size()) };
        Ok(p)
    }

    /// `src` copied into the arena: a Copy handle read back through it (`read`).
    pub fn bytes(&self, src: &[u8]) -> Result<ABytes, Refused> {
        let len = u32::try_from(src.len()).map_err(|_| Refused { bytes: src.len() as u64 })?;
        let c = &*self.core;
        let p = c.carve(src.len().max(1), 1)?;
        // SAFETY: `src.len()` bytes just carved at `p`, disjoint from `src` (a fresh carve).
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), p as *mut u8, src.len()) };
        // a carve comes from the newest chunk
        let ix = c.n.get() - 1;
        let off = (p - c.chunk(ix).base) as u64;
        if off > u32::MAX as u64 {
            return Err(Refused { bytes: src.len() as u64 });
        }
        Ok(ABytes { at: (ix as u64) << 48 | (c.tag() as u64) << 32 | off, len })
    }

    /// The bytes `b` names (panics for a handle of another arena or of this one before a reset).
    pub fn read(&self, b: ABytes) -> &[u8] {
        let c = &*self.core;
        let (ix, tag, off) = ((b.at >> 48) as usize, (b.at >> 32) as u16 as usize, (b.at & 0xffff_ffff) as usize);
        if tag != c.tag() || ix >= c.n.get() {
            wrong_arena();
        }
        let ch = c.chunk(ix);
        if off + b.len as usize > ch.len {
            wrong_arena();
        }
        // SAFETY: inside chunk `ix`, which this arena holds while `self` is borrowed (a reset needs `&mut`).
        unsafe { core::slice::from_raw_parts((ch.base + off) as *const u8, b.len as usize) }
    }

    /// Every chunk back and a new generation: every `AVec` / `ABytes` of before panics if used with this arena again.
    /// Panics while an `AMap` of it is live.
    pub fn reset(&mut self) {
        if Rc::strong_count(&self.core) != 1 {
            panic!("gravity_fn::arena: reset with a live AMap (drop the maps first)");
        }
        self.core.give_all();
        self.core.id.set(next_id());
    }

    /// The allocator handle a map over this arena holds (a refcount: `reset` panics while one is live).
    fn handle(&self) -> ArenaAlloc {
        ArenaAlloc { core: Rc::clone(&self.core) }
    }

    #[inline]
    fn tag(&self) -> usize {
        self.core.tag()
    }
}

/// A Copy handle to bytes inside an arena (`OpArena::bytes`), read through it (`OpArena::read`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ABytes {
    /// chunk index (16 bits) | arena tag (16 bits) | offset in the chunk (32 bits).
    at: u64,
    len: u32,
}

impl ABytes {
    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// A thin vector in an arena (see the module doc): `ptr, len, cap` with the arena's tag in `cap`'s high bits.
pub struct AVec<T: ForgetSafe> {
    ptr: NonNull<T>,
    len: usize,
    cap: u64,
}

// SAFETY: no Drop; its buffer is arena bytes, its elements ForgetSafe.
unsafe impl<T: ForgetSafe> ForgetSafe for AVec<T> {}

impl<T: ForgetSafe> Default for AVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: ForgetSafe> AVec<T> {
    const SIZE: usize = {
        assert!(core::mem::size_of::<T>() != 0, "an AVec of a zero-sized type");
        core::mem::size_of::<T>()
    };

    pub const fn new() -> AVec<T> {
        AVec { ptr: NonNull::dangling(), len: 0, cap: 0 }
    }

    pub fn with_capacity(a: &OpArena, n: usize) -> Result<AVec<T>, Refused> {
        let mut v = Self::new();
        v.reserve(a, n)?;
        Ok(v)
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        (self.cap & CAP_MASK) as usize
    }

    /// Panics unless `a` is this vector's arena now (its tag; a buffer at all: its header too when `deep`).
    #[inline]
    fn owned(&self, a: &OpArena, deep: bool) {
        if self.cap == 0 {
            return;
        }
        if self.cap >> TAG_SHIFT != a.tag() as u64 {
            wrong_arena();
        }
        if deep {
            a.core.check(self.ptr.cast());
        }
    }

    /// Room for `extra` more elements (growth: the arena's ownership checked, header included).
    pub fn reserve(&mut self, a: &OpArena, extra: usize) -> Result<(), Refused> {
        let need = self.len.checked_add(extra).ok_or(Refused { bytes: u64::MAX })?;
        if need <= self.capacity() {
            self.owned(a, false);
            return Ok(());
        }
        self.grow(a, need)
    }

    #[cold]
    fn grow(&mut self, a: &OpArena, need: usize) -> Result<(), Refused> {
        self.owned(a, true);
        let old = self.capacity();
        let new = need.max(old.saturating_mul(2)).max(4);
        let refused = Refused { bytes: (new as u64).saturating_mul(Self::SIZE as u64) };
        if new as u64 > CAP_MASK {
            return Err(refused);
        }
        let bytes = new.checked_mul(Self::SIZE).ok_or(refused)?;
        let c = &*a.core;
        if old != 0 && c.extend_top(self.ptr.cast(), old * Self::SIZE, bytes) {
            self.cap = new as u64 | (a.tag() as u64) << TAG_SHIFT;
            return Ok(());
        }
        let p = c.alloc_tagged(bytes, core::mem::align_of::<T>())?.cast::<T>();
        if old != 0 {
            // SAFETY: `len` initialized elements at the old buffer, moved to the new one (disjoint: a fresh carve).
            unsafe { core::ptr::copy_nonoverlapping(self.ptr.as_ptr(), p.as_ptr(), self.len) };
            c.free_tagged(self.ptr.cast(), old * Self::SIZE, core::mem::align_of::<T>());
        }
        self.ptr = p;
        self.cap = new as u64 | (a.tag() as u64) << TAG_SHIFT;
        Ok(())
    }

    #[inline]
    pub fn push(&mut self, a: &OpArena, v: T) -> Result<(), Refused> {
        if self.len == self.capacity() {
            self.grow(a, self.len + 1)?;
        } else {
            self.owned(a, false);
        }
        // SAFETY: `len < capacity`, the buffer this arena holds (checked above).
        unsafe { self.ptr.as_ptr().add(self.len).write(v) };
        self.len += 1;
        Ok(())
    }

    /// CONVERT1: `n` elements, the first `len` kept and the rest `v` (`Vec::resize`; growth as `reserve`'s).
    pub fn resize(&mut self, a: &OpArena, n: usize, v: T) -> Result<(), Refused>
    where
        T: Copy,
    {
        if n <= self.len {
            self.owned(a, false);
            self.len = n;
            return Ok(());
        }
        self.reserve(a, n - self.len)?;
        for i in self.len..n {
            // SAFETY: room for `n` elements (reserved above), the buffer this arena holds; written, never read uninitialized
            unsafe { self.ptr.as_ptr().add(i).write(v) };
        }
        self.len = n;
        Ok(())
    }

    pub fn extend_from_slice(&mut self, a: &OpArena, s: &[T]) -> Result<(), Refused>
    where
        T: Copy,
    {
        self.reserve(a, s.len())?;
        // SAFETY: room for `s.len()` more (reserved above); `s` is not this buffer (a shared borrow of it would hold `a`'s
        // slice, which needs `&self`, while this takes `&mut self`).
        unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), self.ptr.as_ptr().add(self.len), s.len()) };
        self.len += s.len();
        Ok(())
    }

    pub fn pop(&mut self, a: &OpArena) -> Option<T> {
        self.owned(a, false);
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        // SAFETY: element `len` was initialized and is now outside the vector.
        Some(unsafe { self.ptr.as_ptr().add(self.len).read() })
    }

    /// The elements, read through their arena.
    #[inline]
    pub fn as_slice<'s>(&'s self, a: &'s OpArena) -> &'s [T] {
        self.owned(a, false);
        // SAFETY: `len` initialized elements of a buffer the arena holds while `a` is borrowed.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    #[inline]
    pub fn as_mut_slice<'s>(&'s mut self, a: &'s OpArena) -> &'s mut [T] {
        self.owned(a, false);
        // SAFETY: as `as_slice`, borrowed mutably through `self`.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    /// No elements; the buffer kept (they are ForgetSafe: nothing to drop).
    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn truncate(&mut self, n: usize) {
        self.len = self.len.min(n);
    }

    /// The buffer back to its arena (the newest carve rewinds), the vector empty: ownership checked, header included.
    pub fn release(&mut self, a: &OpArena) {
        if self.cap == 0 {
            return;
        }
        self.owned(a, true);
        a.core.free_tagged(self.ptr.cast(), self.capacity() * Self::SIZE, core::mem::align_of::<T>());
        *self = Self::new();
    }

    /// Empty with no buffer, its old one left to the arena (after a reset, before the vector is used again).
    pub fn detach(&mut self) {
        *self = Self::new();
    }
}

/// The allocator a map over an arena holds: a refcounted handle (`OpArena::reset` panics while one is live).
#[derive(Clone)]
pub struct ArenaAlloc {
    core: Rc<Core>,
}

// SAFETY: a block stays valid until it is deallocated or the arena resets, which the refcount forbids while this handle
// lives; every block carries the arena's header and is checked at its deallocation.
unsafe impl Allocator for ArenaAlloc {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if layout.size() == 0 {
            // SAFETY: a non-zero alignment is a non-null address.
            let p = unsafe { NonNull::new_unchecked(layout.align() as *mut u8) };
            return Ok(NonNull::slice_from_raw_parts(p, 0));
        }
        let p = self.core.alloc_tagged(layout.size(), layout.align()).map_err(|_| AllocError)?;
        Ok(NonNull::slice_from_raw_parts(p, layout.size()))
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        if layout.size() == 0 {
            return;
        }
        self.core.check(ptr);
        self.core.free_tagged(ptr, layout.size(), layout.align());
    }
}

/// A hash map in an arena (hashbrown over `ArenaAlloc`): its entries are ForgetSafe. Insertion is fallible only: a table
/// growth the arena refuses is `Refused`, never the allocator's abort.
pub struct AMap<K: ForgetSafe, V: ForgetSafe, S = hashbrown::hash_map::DefaultHashBuilder> {
    map: hashbrown::HashMap<K, V, S, ArenaAlloc>,
}

impl<K: ForgetSafe + Eq + Hash, V: ForgetSafe, S: BuildHasher + Default> AMap<K, V, S> {
    pub fn new(a: &OpArena) -> Self {
        AMap { map: hashbrown::HashMap::with_hasher_in(S::default(), a.handle()) }
    }

    pub fn try_reserve(&mut self, n: usize) -> Result<(), Refused> {
        self.map.try_reserve(n).map_err(|_| Refused { bytes: n as u64 })
    }

    /// `v` at `k` (the old value back), with room taken first: a refused growth inserts nothing.
    pub fn try_insert(&mut self, k: K, v: V) -> Result<Option<V>, Refused> {
        self.try_reserve(1)?;
        Ok(self.map.insert(k, v))
    }

    pub fn get<Q: ?Sized + Hash + hashbrown::Equivalent<K>>(&self, k: &Q) -> Option<&V> {
        self.map.get(k)
    }

    pub fn get_mut<Q: ?Sized + Hash + hashbrown::Equivalent<K>>(&mut self, k: &Q) -> Option<&mut V> {
        self.map.get_mut(k)
    }

    pub fn contains_key<Q: ?Sized + Hash + hashbrown::Equivalent<K>>(&self, k: &Q) -> bool {
        self.map.contains_key(k)
    }

    pub fn remove<Q: ?Sized + Hash + hashbrown::Equivalent<K>>(&mut self, k: &Q) -> Option<V> {
        self.map.remove(k)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    pub fn iter(&self) -> hashbrown::hash_map::Iter<'_, K, V> {
        self.map.iter()
    }

    pub fn iter_mut(&mut self) -> hashbrown::hash_map::IterMut<'_, K, V> {
        self.map.iter_mut()
    }
}
