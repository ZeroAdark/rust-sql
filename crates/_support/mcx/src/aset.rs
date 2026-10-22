// aset.c block-pooling backend, minus the per-chunk header (the Allocator gets context + Layout); is_dedicated is layout-pure.

use core::alloc::Layout;
use core::ptr::NonNull;

use allocator_api2::alloc::AllocError;

use crate::Acct;

// Guard-page debug allocator: overruns fault at the writing site.
#[cfg(feature = "aset-guard")]
mod guard {
    use core::alloc::Layout;
    use core::ptr::NonNull;

    use allocator_api2::alloc::AllocError;

    fn page_size() -> usize {
        // SAFETY: sysconf is always callable.
        unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
    }

    // [meta page][data pages...][PROT_NONE guard]; Meta records base+len for munmap.
    #[repr(C)]
    struct Meta {
        base: *mut u8,
        len: usize,
    }

    pub(super) fn alloc(layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let ps = page_size();
        let size = layout.size().max(1);
        let align = layout.align().max(8);
        let data_pages = (size + ps - 1) / ps;
        let total = ps + data_pages * ps + ps;
        // SAFETY: plain anonymous private mapping; failure checked below.
        let base = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                total,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(AllocError);
        }
        let base = base as *mut u8;
        // SAFETY: guard page lies inside the mapping just created.
        let guard = unsafe { base.add(ps + data_pages * ps) };
        unsafe {
            if libc::mprotect(guard as *mut libc::c_void, ps, libc::PROT_NONE) != 0 {
                libc::munmap(base as *mut libc::c_void, total);
                return Err(AllocError);
            }
        }
        let guard_addr = guard as usize;
        let mut start = guard_addr - size;
        start &= !(align - 1);
        let ptr = start as *mut u8;
        // SAFETY: `base` is the writable mapping start; Meta fits a page.
        unsafe {
            core::ptr::write(base as *mut Meta, Meta { base, len: total });
        }
        // G1-A door 2 (debug feature): a guarded chunk is its own mapping, which door 1 never sees
        crate::heap::door(base as usize, total, true, None);
        debug_assert!(ptr >= unsafe { base.add(ps) });
        // SAFETY: `start` is inside the data pages, hence non-null.
        let nn = unsafe { NonNull::new_unchecked(ptr) };
        Ok(NonNull::slice_from_raw_parts(nn, guard_addr - start))
    }

    pub(super) unsafe fn dealloc(ptr: NonNull<u8>, _layout: Layout) {
        let ps = page_size();
        // Meta page immediately precedes the data page containing `ptr`.
        let p = ptr.as_ptr() as usize;
        let data_page_start = p & !(ps - 1);
        let base = (data_page_start - ps) as *mut u8;
        let meta = core::ptr::read(base as *const Meta);
        crate::heap::door(meta.base as usize, meta.len, false, None);
        libc::munmap(meta.base as *mut libc::c_void, meta.len);
    }
}

const ALLOC_MINBITS: u32 = 3;
const NUM_FREELISTS: usize = 11;
const ALLOC_CHUNK_LIMIT: usize = 1 << (NUM_FREELISTS - 1 + ALLOC_MINBITS as usize);
pub(crate) const POOL_MAX_ALIGN: usize = 8;

pub(crate) const INIT_BLOCK_SIZE: usize = 8 * 1024;
pub(crate) const MAX_BLOCK_SIZE: usize = 8 * 1024 * 1024;

#[cfg(feature = "aset-stats")]
pub(crate) mod stats {
    use core::sync::atomic::AtomicU64;
    pub(crate) static BLOCK_MALLOCS: AtomicU64 = AtomicU64::new(0);
    pub(crate) static KEEPER_REUSES: AtomicU64 = AtomicU64::new(0);
}

pub fn alloc_stats() -> (u64, u64) {
    #[cfg(feature = "aset-stats")]
    {
        use core::sync::atomic::Ordering;
        (
            stats::BLOCK_MALLOCS.load(Ordering::Relaxed),
            stats::KEEPER_REUSES.load(Ordering::Relaxed),
        )
    }
    #[cfg(not(feature = "aset-stats"))]
    {
        (0, 0)
    }
}

const MAX_FREE_CONTEXTS: usize = 100;

pub(crate) struct Block {
    pub(crate) ptr: NonNull<u8>,
    pub(crate) size: usize,
    pub(crate) used: usize,
    /// `pooled`'s answer once asked (`heap::pooled_stable`): 0 not asked, 1 no, 2 yes
    pooled: core::cell::Cell<u8>,
    /// the allocator that served it, as its allocation knew it
    served: crate::heap::Served,
}

impl Block {
    fn layout(size: usize) -> Layout {
        Layout::from_size_align(size, POOL_MAX_ALIGN).unwrap()
    }

    /// A new block of a context whose owner is `led`'s (a mapped one maps for it); a `kept` one (a keeper a reset keeps
    /// or a drop parks) never comes from the block source.
    pub(crate) fn alloc_for(size: usize, led: &crate::owner::Led, kept: bool) -> Result<Block, AllocError> {
        #[cfg(feature = "aset-stats")]
        stats::BLOCK_MALLOCS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let (ptr, served) = if kept {
            crate::heap::allocate_kept_served_for(Block::layout(size), led)?
        } else {
            crate::heap::allocate_served_for(Block::layout(size), led)?
        };
        Ok(Block { ptr: ptr.cast(), size, used: 0, pooled: core::cell::Cell::new(0), served })
    }

    /// # Safety: at most once per block, with no chunk in it still in use; its context's reset or drop resets the
    /// context's Led. `life`: its context's `Led::pooled_life` (`heap::free_block`).
    pub(crate) unsafe fn free(self, life: Option<crate::heap::Lifetime>) {
        crate::heap::free_served_block(self.ptr, Block::layout(self.size), life, self.served);
    }

    fn pooled(&self) -> bool {
        if let Some(p) = self.served.known(self.ptr.as_ptr()) {
            return p;
        }
        match self.pooled.get() {
            1 => false,
            2 => true,
            _ => match crate::heap::pooled_stable(self.ptr.as_ptr()) {
                Some(p) => {
                    self.pooled.set(1 + u8::from(p));
                    p
                }
                None => crate::heap::pooled(self.ptr.as_ptr()),
            },
        }
    }
}

/// A recycle list's entry: a dropped context's keeper (its block list, the keeper alone) and the account it parked on,
/// which door 1 counted the block on (the calling thread's, `memledger::current`). Its bytes are counted parked on that
/// account until a context takes it or the list lets it go, whichever thread or account does it.
pub(crate) struct Parked {
    blocks: alloc::vec::Vec<Block>,
    acct: u32,
}

impl Parked {
    /// `blocks` parking now on the calling thread's account: counted parked.
    fn park(blocks: alloc::vec::Vec<Block>) -> Parked {
        #[cfg(feature = "std")]
        let acct = memledger::current().0;
        #[cfg(not(feature = "std"))]
        let acct = 0;
        let p = Parked { blocks, acct };
        p.count(true);
        p
    }

    /// The entry leaving its list (taken, or to be freed): counted out on the account it parked on.
    fn leave(self) -> alloc::vec::Vec<Block> {
        self.count(false);
        self.blocks
    }

    fn count(&self, on: bool) {
        let n = self.blocks.iter().map(|b| b.size).sum::<usize>() + self.blocks.capacity() * core::mem::size_of::<Block>();
        let d = if on { n as i64 } else { -(n as i64) };
        #[cfg(feature = "std")]
        memledger::keepers_moved(memledger::Acct(self.acct), d);
        #[cfg(not(feature = "std"))]
        {
            let _ = self.acct;
            PARKED.fetch_add(d, core::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Without `std` (no memory ledger): the parked bytes, process-wide.
#[cfg(not(feature = "std"))]
static PARKED: core::sync::atomic::AtomicI64 = core::sync::atomic::AtomicI64::new(0);

/// The keeper blocks parked in the recycle lists now, in bytes (every thread's and the shared pool's): free context memory
/// kept for the next context's first block, on its account (door 1) and no owner's. The owners view and the unattributed
/// check count it apart, per account (`memledger::keepers`), not as unattributed.
pub fn parked_keeper_bytes() -> usize {
    #[cfg(feature = "std")]
    let n = memledger::keepers_total();
    #[cfg(not(feature = "std"))]
    let n = PARKED.load(core::sync::atomic::Ordering::Relaxed);
    n.max(0) as usize
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum KeeperClass {
    Default,
    Small,
}

impl KeeperClass {
    pub(crate) fn keeper_size(self) -> usize {
        match self {
            KeeperClass::Default => INIT_BLOCK_SIZE,
            KeeperClass::Small => crate::ALLOCSET_SMALL_INITSIZE,
        }
    }
}

// PoolMutex, not bare UnsafeCell: dependent crates' libtest threads share the
// pool (notes/mcx-acct-pool-test-race.md); create/drop only, so the lock is cold.
struct KeeperPool {
    stack: crate::PoolMutex<alloc::vec::Vec<Parked>>,
}

static KEEPER_POOL: KeeperPool = KeeperPool {
    stack: crate::PoolMutex::new(alloc::vec::Vec::new()),
};

static SMALL_KEEPER_POOL: KeeperPool = KeeperPool {
    stack: crate::PoolMutex::new(alloc::vec::Vec::new()),
};

fn keeper_pool(class: KeeperClass) -> &'static KeeperPool {
    match class {
        KeeperClass::Default => &KEEPER_POOL,
        KeeperClass::Small => &SMALL_KEEPER_POOL,
    }
}

impl KeeperPool {
    // Single CAS try, never spin: a contended pool is bypassed (fresh alloc /
    // direct free) — see PoolMutex::try_with.
    #[cfg(test)]
    fn with<R>(&self, f: impl FnOnce(&mut alloc::vec::Vec<Parked>) -> R) -> R {
        self.stack.try_with(f).expect("uncontended in single-threaded test")
    }

    fn take(&self) -> alloc::vec::Vec<Block> {
        match self.stack.try_with(|stack| stack.pop().map(Parked::leave)) {
            Some(Some(blocks)) => {
                #[cfg(feature = "aset-stats")]
                stats::KEEPER_REUSES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                blocks
            }
            _ => alloc::vec::Vec::new(),
        }
    }

    // `blocks` is exactly [keeper] with used == 0; a full list drains wholesale first.
    fn recycle(&self, blocks: alloc::vec::Vec<Block>) {
        let mut give = Some(blocks);
        let overflow = self.stack.try_with(|stack| {
            let drained = if stack.len() >= MAX_FREE_CONTEXTS {
                core::mem::take(stack)
            } else {
                alloc::vec::Vec::new()
            };
            stack.push(Parked::park(give.take().expect("closure runs at most once")));
            drained
        });
        match overflow {
            Some(drained) => drained.into_iter().for_each(|p| free_blocks(p.leave(), None)),
            // a contended pool: the entry never parked, so it was never counted
            None => free_blocks(give.take().expect("closure did not run"), None),
        }
    }
}

/// `life`: the blocks' context's `Led::pooled_life`; None for parked keepers (never the source's).
fn free_blocks(blocks: alloc::vec::Vec<Block>, life: Option<crate::heap::Lifetime>) {
    for b in blocks {
        // SAFETY: a keeper list's blocks, no chunk in use (Drop / &mut); this is the sole free.
        unsafe { b.free(life) };
    }
}

// Per-thread keeper list (see lib.rs `local_pool_on`): C-parity residency —
// context_freelists is per-process = per-backend, so per-thread cap
// MAX_FREE_CONTEXTS matches C's per-backend footprint exactly.
/// A parked entry leaving its thread's list to be freed (a park onto the full list, a trim, the thread's exit).
#[cfg(all(feature = "std", not(test)))]
fn dispose_keepers(p: Parked) {
    free_blocks(p.leave(), None);
}

#[cfg(all(feature = "std", not(test)))]
std::thread_local! {
    static KEEPER_TLS: crate::LocalStack<Parked> =
        const { crate::LocalStack::new(MAX_FREE_CONTEXTS, dispose_keepers) };
    static SMALL_KEEPER_TLS: crate::LocalStack<Parked> =
        const { crate::LocalStack::new(MAX_FREE_CONTEXTS, dispose_keepers) };
}

#[cfg(all(feature = "std", not(test)))]
fn keeper_tls(class: KeeperClass) -> &'static std::thread::LocalKey<crate::LocalStack<Parked>> {
    match class {
        KeeperClass::Default => &KEEPER_TLS,
        KeeperClass::Small => &SMALL_KEEPER_TLS,
    }
}

#[inline]
pub(crate) fn take_recycled_blocks() -> alloc::vec::Vec<Block> {
    take_recycled_keeper(KeeperClass::Default)
}

#[inline]
pub(crate) fn take_recycled_keeper(class: KeeperClass) -> alloc::vec::Vec<Block> {
    #[cfg(test)]
    {
        let _ = class;
        alloc::vec::Vec::new()
    }
    #[cfg(not(test))]
    {
        #[cfg(feature = "std")]
        if crate::local_pool_on() {
            if let Ok(Some(p)) = keeper_tls(class).try_with(|s| s.take()) {
                let blocks = p.leave();
                #[cfg(feature = "aset-stats")]
                stats::KEEPER_REUSES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                return blocks;
            }
            // TLS empty or gone (thread exiting): fresh blocks.
            return alloc::vec::Vec::new();
        }
        keeper_pool(class).take()
    }
}

/// D4679: the growth shelf. A context dropped at a statement's end (simple_query's per-parsetree context, made and dropped
/// for each statement of a multi-statement message, and the planner's lazy contexts under it) frees its growth blocks (its
/// blocks after the first: 8 KiB, 16 KiB, ..) and a first block no keeper list takes, which the next statement's contexts
/// malloc again (2-3 mallocs and frees a statement). Under `PGRUST_MEMPOLICY=keep_growth=1` (default 0: off, today's road)
/// such a block parks here instead, at most `mempolicy::keep::GROWTH_SHELF_BYTES` (64 KiB) per thread (the oldest freed to
/// make room), and the thread's next context block of exactly that size is taken from here before it mallocs. Only while
/// the thread charges the session account it is bound to (`memledger::bound`: a backend's own thread, not a pool worker in
/// a session's home), so every block on the shelf parked on that account, and a take is on it too. A parked block is
/// counted as a parked keeper on that account, apart as growth (`memledger::growth_moved`), until a context takes it or
/// the shelf frees it: at the session's unbind (memledger's unbind hook, while the thread still charges the account), a
/// thread-reuse park (`run_thread_reuse_hooks`), idle passivation (`trim_recycled_blocks`), the first park after the
/// memory-short epoch moved (REACH, the ladder's shed path: that park parks nothing either) and the thread's exit.
#[cfg(all(feature = "std", not(test)))]
mod growth {
    use super::Block;
    use core::cell::RefCell;

    const SLOTS: usize = 4;

    /// Parked blocks with their park order (the oldest goes first when a new block needs the room), every one parked on
    /// account `acct`; `short`: the memory-short epoch the shelf last read.
    struct Shelf {
        slots: [Option<(Block, u64)>; SLOTS],
        bytes: usize,
        seq: u64,
        acct: u32,
        short: u64,
    }

    impl Shelf {
        fn free_slot(&mut self, i: usize) {
            if let Some((b, _)) = self.slots[i].take() {
                self.bytes -= b.size;
                memledger::growth_moved(memledger::Acct(self.acct), -(b.size as i64), -1);
                // SAFETY: a parked growth block, no chunk in use; the shelf held it alone. Never a pooled block (`park`'s
                // callers keep those out), so no class to name.
                unsafe { b.free(None) };
            }
        }

        fn free_all(&mut self) {
            for i in 0..SLOTS {
                self.free_slot(i);
            }
        }

        /// The oldest parked block's slot.
        fn oldest(&self) -> Option<usize> {
            (0..SLOTS).filter(|&i| self.slots[i].is_some()).min_by_key(|&i| self.slots[i].as_ref().map_or(u64::MAX, |x| x.1))
        }
    }

    impl Drop for Shelf {
        fn drop(&mut self) {
            self.free_all();
        }
    }

    std::thread_local! {
        static SHELF: RefCell<Shelf> =
            const { RefCell::new(Shelf { slots: [None, None, None, None], bytes: 0, seq: 0, acct: 0, short: 0 }) };
    }

    /// Whether the shelf is on (one knob load): the callers' only cost when off, the shelf's own code out of line.
    #[inline(always)]
    pub(super) fn on() -> bool {
        mempolicy::keep::growth_shelf_bytes() != 0
    }

    /// A parked block of exactly `size` bytes parked on the calling thread's account now, counted out; `None` when empty
    /// or none fits.
    #[inline(never)]
    pub(super) fn take(size: usize) -> Option<Block> {
        SHELF
            .try_with(|s| {
                let mut s = s.try_borrow_mut().ok()?;
                if s.bytes == 0 || s.acct != memledger::current().0 {
                    return None;
                }
                let i = s.slots.iter().position(|x| x.as_ref().is_some_and(|(b, _)| b.size == size))?;
                let (b, _) = s.slots[i].take()?;
                s.bytes -= b.size;
                memledger::growth_moved(memledger::Acct(s.acct), -(b.size as i64), -1);
                Some(b)
            })
            .ok()
            .flatten()
    }

    /// Parks growth block `b` (its context dropped: no chunk in use) when the switch is on, it is at most the cap and the
    /// thread charges its own session account, freeing the oldest parked blocks first while it would not fit (the shelf
    /// follows the current statements' sizes); else hands it back to be freed. The first park after the memory-short epoch
    /// moved frees a shelf that holds blocks and parks nothing.
    #[inline(never)]
    pub(super) fn park(b: Block) -> Option<Block> {
        let cap = mempolicy::keep::growth_shelf_bytes();
        if cap == 0 || b.size > cap {
            return Some(b);
        }
        let acct = memledger::current();
        if !acct.is_backend() || acct != memledger::bound() {
            return Some(b);
        }
        unbind_hook();
        let mut give = Some(b);
        let _ = SHELF.try_with(|s| {
            let Ok(mut s) = s.try_borrow_mut() else { return };
            let short = crate::memory_short_epoch();
            if s.short != short || s.acct != acct.0 {
                // REACH: memory went short since the shelf last looked, so a shelf that holds blocks sheds them (and this
                // block goes too); blocks parked on another account (a missed unbind) go as well
                let shed = s.short != short && s.bytes != 0;
                s.free_all();
                s.short = short;
                s.acct = acct.0;
                if shed {
                    return;
                }
            }
            let size = give.as_ref().map_or(0, |b| b.size);
            while s.bytes + size > cap || s.slots.iter().all(Option::is_some) {
                let Some(i) = s.oldest() else { return };
                s.free_slot(i);
            }
            let Some(i) = s.slots.iter().position(Option::is_none) else { return };
            let mut b = give.take().expect("parked once");
            b.used = 0;
            memledger::growth_moved(acct, size as i64, 1);
            s.seq += 1;
            let seq = s.seq;
            s.slots[i] = Some((b, seq));
            s.bytes += size;
        });
        give
    }

    /// Frees this thread's parked growth blocks (the session's unbind, a thread-reuse park, idle passivation).
    pub(super) fn empty_shelf() {
        let _ = SHELF.try_with(|s| {
            if let Ok(mut s) = s.try_borrow_mut() {
                s.free_all();
            }
        });
    }

    /// The session's unbind empties the unbinding thread's shelf while the thread still charges the account (set at the
    /// first park: off, nothing is set).
    fn unbind_hook() {
        static SET: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
        if !SET.load(core::sync::atomic::Ordering::Relaxed) {
            SET.store(true, core::sync::atomic::Ordering::Relaxed);
            memledger::set_unbind_hook(empty_shelf);
        }
    }
}

/// D4679: empties this thread's growth shelf (`growth`; a thread-reuse park).
pub(crate) fn empty_growth_shelf() {
    #[cfg(all(feature = "std", not(test)))]
    growth::empty_shelf();
}

/// D3.4 idle passivation: free this thread's parked keeper blocks (context
/// churn parks up to MAX_FREE_CONTEXTS × INIT_BLOCK_SIZE here — retained
/// allocator memory an idle backend should not hold). Global-pool arm
/// (PGRUST_MCX_POOL_STRIPE=0) drains the shared pool instead: coarser blast
/// radius, but this is a passivation-only cold call.
pub(crate) fn trim_recycled_blocks() {
    #[cfg(not(test))]
    {
        #[cfg(feature = "std")]
        growth::empty_shelf();
        for class in [KeeperClass::Default, KeeperClass::Small] {
            #[cfg(feature = "std")]
            if crate::local_pool_on() {
                let _ = keeper_tls(class).try_with(|s| s.drain_all());
                continue;
            }
            if let Some(all) = keeper_pool(class).stack.try_with(core::mem::take) {
                all.into_iter().for_each(|p| free_blocks(p.leave(), None));
            }
        }
    }
}

#[inline]
pub(crate) fn recycle_blocks(blocks: alloc::vec::Vec<Block>) {
    recycle_keeper(KeeperClass::Default, blocks, None);
}

/// `life`: the keeper's context's `Led::pooled_life` (a pooled keeper goes back to the source, never parks).
#[inline]
pub(crate) fn recycle_keeper(class: KeeperClass, blocks: alloc::vec::Vec<Block>, life: Option<crate::heap::Lifetime>) {
    #[cfg(test)]
    {
        let _ = class;
        for b in blocks {
            // SAFETY: keeper-only list, blocks no longer in use (Drop/&mut).
            unsafe { b.free(life) };
        }
    }
    #[cfg(not(test))]
    {
        if crate::heap::source_set() && (!crate::heap::keepers_park() || blocks.iter().any(Block::pooled)) {
            free_blocks(blocks, life);
            return;
        }
        #[cfg(feature = "std")]
        if crate::local_pool_on() {
            // Ownership parks OUTSIDE the closure: on TLS-gone the closure
            // never runs (Err) and the blocks are freed here — `Block` has no
            // Drop, so letting the Vec fall out of a dead closure would leak
            // the raw allocations.
            let mut give = Some(blocks);
            let done = keeper_tls(class)
                .try_with(|s| s.give_wholesale(Parked::park(give.take().expect("closure runs at most once"))));
            if done.is_err() {
                // never parked, so never counted
                free_blocks(give.take().expect("closure did not run"), None);
            }
            return;
        }
        keeper_pool(class).recycle(blocks);
    }
}

#[inline]
fn is_dedicated(layout: Layout) -> bool {
    layout.align() > POOL_MAX_ALIGN || layout.size().max(1) > ALLOC_CHUNK_LIMIT
}

#[inline]
fn free_list_index(size: usize) -> usize {
    let chunk = size.max(1 << ALLOC_MINBITS).next_power_of_two();
    (chunk.trailing_zeros() - ALLOC_MINBITS) as usize
}

#[inline]
fn idx_and_chunk(size: usize) -> (usize, usize) {
    if size <= (1 << ALLOC_MINBITS) {
        (0, 1 << ALLOC_MINBITS)
    } else {
        let t = (size - 1) >> ALLOC_MINBITS;
        let idx = (usize::BITS - t.leading_zeros()) as usize;
        (idx, 1usize << (idx + ALLOC_MINBITS as usize))
    }
}

// C-parity inverse of size_class (aset.c GetChunkSizeFromFreeListIdx); awaits chunk-space consumers.
#[allow(dead_code)]
#[inline]
fn chunk_size(idx: usize) -> usize {
    1usize << (idx as u32 + ALLOC_MINBITS)
}

pub(crate) struct AllocSet {
    blocks: alloc::vec::Vec<Block>,
    // Freelist heads; the next pointer lives in the chunk's own first 8 bytes.
    freelist: [Option<NonNull<u8>>; NUM_FREELISTS],
    next_block_size: usize,
    // C's AllocSetContextCreate initBlockSize / maxBlockSize.
    init_block_size: usize,
    max_block_size: usize,
    keeper_class: Option<KeeperClass>,
    mem_allocated: usize,
    // Cached bump window over the active block (C's freeptr/endptr); null until a block exists.
    cur_ptr: *mut u8,
    cur_end: *mut u8,
    // Outstanding dedicated (> ALLOC_CHUNK_LIMIT / over-aligned) allocations —
    // C's single-chunk blocks (aset.c AllocSetAllocLarge links them into the
    // block list). They live until pfree, reset, or context delete; without
    // this list a reset/Drop leaked every outstanding dedicated chunk
    // permanently and invisibly (P1: create_gather_merge_path's path_arena
    // final buffer, ~13KB leaked per DISTINCT planning under ArenaForget).
    // with the allocator that served each (`heap::Served`), which a resize may change
    dedicated: alloc::vec::Vec<(NonNull<u8>, Layout, crate::heap::Served)>,
    // pagepool P2 (E3b): the context's `Led::pooled_life` as of its last block take, for the drop's gives (Drop has no Led)
    pool_life: Option<crate::heap::Lifetime>,
    // upstream 3f3eefc28892 (18.4): Detect pfree or repalloc of a previously-freed memory chunk.
    // Pool chunks currently on a freelist, by address (C marks the chunk header;
    // pool chunks here have none). Debug builds only.
    #[cfg(debug_assertions)]
    freed: crate::freed::FreedSet,
}

impl AllocSet {
    pub(crate) fn new() -> AllocSet {
        Self::from_blocks(take_recycled_blocks(), INIT_BLOCK_SIZE, MAX_BLOCK_SIZE, Some(KeeperClass::Default))
    }

    // No recycled keeper: the first block is allocated by the first alloc,
    // so a context that is never used costs no block.
    pub(crate) fn new_sized(init_block_size: usize, max_block_size: usize) -> AllocSet {
        debug_assert!(init_block_size <= max_block_size);
        Self::from_blocks(alloc::vec::Vec::new(), init_block_size, max_block_size, None)
    }

    pub(crate) fn new_small(max_block_size: usize) -> AllocSet {
        let init = crate::ALLOCSET_SMALL_INITSIZE;
        debug_assert!(init <= max_block_size);
        Self::from_blocks(take_recycled_keeper(KeeperClass::Small), init, max_block_size, Some(KeeperClass::Small))
    }

    // Session-root retirement: the keeper goes too; the next alloc grows one.
    pub(crate) fn release_keeper(&mut self) {
        let (init, max, class) = (self.init_block_size, self.max_block_size, self.keeper_class);
        *self = Self::from_blocks(alloc::vec::Vec::new(), init, max, class);
    }

    /// The recycle list a dropped context's keeper of `size` bytes parks on, if any.
    fn parks(&self, size: usize) -> Option<KeeperClass> {
        match self.keeper_class {
            Some(c) if size == c.keeper_size() => Some(c),
            _ if size == INIT_BLOCK_SIZE => Some(KeeperClass::Default),
            _ => None,
        }
    }

    fn from_blocks(
        blocks: alloc::vec::Vec<Block>,
        init_block_size: usize,
        max_block_size: usize,
        keeper_class: Option<KeeperClass>,
    ) -> AllocSet {
        let mem_allocated = blocks.first().map_or(0, |b| b.size);
        crate::global_footprint::add(mem_allocated);
        debug_assert!(blocks.is_empty() || (blocks.len() == 1 && blocks[0].used == 0));
        let (cur_ptr, cur_end) = match blocks.first() {
            Some(b) => {
                let base = b.ptr.as_ptr();
                // SAFETY: used <= size, both offsets stay within the block.
                (unsafe { base.add(b.used) }, unsafe { base.add(b.size) })
            }
            None => (core::ptr::null_mut(), core::ptr::null_mut()),
        };
        AllocSet {
            blocks,
            freelist: [None; NUM_FREELISTS],
            next_block_size: init_block_size,
            init_block_size,
            max_block_size,
            keeper_class,
            mem_allocated,
            cur_ptr,
            cur_end,
            dedicated: alloc::vec::Vec::new(),
            pool_life: None,
            #[cfg(debug_assertions)]
            freed: crate::freed::FreedSet::new(),
        }
    }

    #[inline]
    fn sync_active(&mut self) {
        if !self.cur_ptr.is_null() {
            if let Some(active) = self.blocks.last_mut() {
                active.used = self.cur_ptr as usize - active.ptr.as_ptr() as usize;
            }
        }
    }

    // C-parity accessor (MemoryContextMemAllocated); awaits memory-accounting consumers.
    #[allow(dead_code)]
    pub(crate) fn mem_allocated(&self) -> usize {
        self.mem_allocated
    }

    // aset.c:1545 AllocSetStats at block grain: the bytes held in blocks and
    // the block count — every block on set->blocks, the single-chunk blocks
    // of AllocSetAllocLarge included.
    pub(crate) fn footprint(&self) -> usize {
        self.mem_allocated
    }

    pub(crate) fn nblocks(&self) -> usize {
        self.blocks.len() + self.dedicated.len()
    }

    // Mirror the block-grain figures to the accounting node; cold (block
    // transitions only). The chunk-grain figures (live chunk bytes, parked
    // free chunks) move on the Acct itself at each alloc/free.
    #[inline]
    fn sync_blocks(&self, acct: &Acct) {
        acct.set_blocks(self.mem_allocated, self.nblocks());
    }

    // The pool hit, the ONE definition of alloc's two hit branches: a freelist pop or a bump inside the active block.
    // Mcx::alloc_fast inlines it at every allocation site (D4483); alloc calls it after its ZST / guard / dedicated arms.
    // None = no state touched (a zero size, the aset-guard build, a dedicated size, no room): the caller goes on to the
    // full path.
    #[inline(always)]
    pub(crate) fn alloc_hit(&mut self, layout: Layout, acct: &Acct) -> Option<NonNull<[u8]>> {
        if cfg!(feature = "aset-guard") || layout.size() == 0 || is_dedicated(layout) {
            return None;
        }
        let (idx, csize) = idx_and_chunk(layout.size());

        if let Some(head) = self.freelist[idx] {
            // SAFETY: a parked chunk's first 8 bytes hold the next-free pointer (>= 8B, 8-aligned).
            let next = unsafe { core::ptr::read(head.as_ptr() as *const Option<NonNull<u8>>) };
            self.freelist[idx] = next;
            #[cfg(debug_assertions)]
            self.freed.note_alloc(head.as_ptr().addr());
            acct.free_chunks.set(acct.free_chunks.get() - 1);
            acct.live_chunk_bytes.set(acct.live_chunk_bytes.get() + csize);
            return Some(NonNull::slice_from_raw_parts(head, csize));
        }

        let avail = self.cur_end as usize - self.cur_ptr as usize;
        if csize <= avail {
            let p = self.cur_ptr;
            acct.live_chunk_bytes.set(acct.live_chunk_bytes.get() + csize);
            // SAFETY: csize <= avail keeps the bump within the active block; p non-null there.
            unsafe {
                self.cur_ptr = p.add(csize);
                return Some(NonNull::slice_from_raw_parts(NonNull::new_unchecked(p), csize));
            }
        }
        None
    }

    // Reached from Mcx's out-of-line slow path only (D4483): the pool hit itself inlines at the sites through
    // Mcx::alloc_fast.
    #[inline(always)]
    pub(crate) fn alloc(&mut self, layout: Layout, acct: &Acct) -> Result<NonNull<[u8]>, AllocError> {
        // ZST: dangling aligned pointer, no pool state touched (as allocator_api2 expects).
        if layout.size() == 0 {
            let dangling = layout.align() as *mut u8;
            // SAFETY: align is a non-zero power of two, hence non-null.
            let nn = unsafe { NonNull::new_unchecked(dangling) };
            return Ok(NonNull::slice_from_raw_parts(nn, 0));
        }
        #[cfg(feature = "aset-guard")]
        {
            return guard::alloc(layout);
        }
        #[cfg(not(feature = "aset-guard"))]
        if is_dedicated(layout) {
            return self.alloc_dedicated(layout, acct);
        }
        if let Some(hit) = self.alloc_hit(layout, acct) {
            return Ok(hit);
        }
        let (_, csize) = idx_and_chunk(layout.size());
        self.alloc_new_block(csize, acct)
    }

    // C's AllocSetAllocLarge: a dedicated chunk is a single-chunk block —
    // tracked, counted in mem_allocated, and freed at pfree/reset/delete.
    #[cold]
    #[inline(never)]
    fn alloc_dedicated(&mut self, layout: Layout, acct: &Acct) -> Result<NonNull<[u8]>, AllocError> {
        acct.admit_block(layout.size());
        let (out, served) = crate::heap::allocate_served_for(layout, &acct.led)?;
        self.pool_life = acct.led.pooled_life();
        self.dedicated.push((out.cast::<u8>(), layout, served));
        self.mem_allocated += layout.size();
        crate::global_footprint::add(layout.size());
        // A single-chunk block has no free space (freeptr == endptr).
        acct.live_chunk_bytes.set(acct.live_chunk_bytes.get() + layout.size());
        self.sync_blocks(acct);
        Ok(out)
    }

    #[cold]
    #[inline(never)]
    fn dealloc_dedicated(&mut self, ptr: NonNull<u8>, layout: Layout, acct: &Acct) {
        let idx = self
            .dedicated
            .iter()
            .position(|&(p, _, _)| p == ptr)
            .expect("dedicated dealloc of an untracked chunk");
        let (_, _, served) = self.dedicated.swap_remove(idx);
        self.mem_allocated -= layout.size();
        crate::global_footprint::sub(layout.size());
        acct.live_chunk_bytes.set(acct.live_chunk_bytes.get() - layout.size());
        // SAFETY: tracked live dedicated allocation; caller guarantees layout. Freed before the sync: the Led's pooled and
        // mapped bytes leave with the block, so the door-1 part never reads the block as its own.
        unsafe { crate::heap::deallocate_served_for(ptr, layout, &acct.led, served) };
        self.sync_blocks(acct);
    }

    // aset.c AllocSetRealloc of an external chunk: realloc() (mremap if mapped).
    #[cold]
    #[inline(never)]
    unsafe fn realloc_dedicated(
        &mut self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
        acct: &Acct,
    ) -> Result<NonNull<[u8]>, AllocError> {
        let idx = self
            .dedicated
            .iter()
            .position(|&(p, _, _)| p == ptr)
            .expect("dedicated realloc of an untracked chunk");
        acct.admit_block(new_layout.size().saturating_sub(old_layout.size()));
        let (out, served) = crate::heap::resize_served_for(ptr, old_layout, new_layout, &acct.led, self.dedicated[idx].2)?;
        self.pool_life = acct.led.pooled_life();
        self.dedicated[idx] = (out.cast::<u8>(), new_layout, served);
        self.mem_allocated = self.mem_allocated - old_layout.size() + new_layout.size();
        crate::global_footprint::sub(old_layout.size());
        crate::global_footprint::add(new_layout.size());
        acct.live_chunk_bytes
            .set(acct.live_chunk_bytes.get() - old_layout.size() + new_layout.size());
        self.sync_blocks(acct);
        Ok(out)
    }

    /// An unpooled root's drop: every block freed now, its keeper too (no recycle list, no growth shelf), so the arena's
    /// own drop finds none. Pooled blocks (pagepool) go back to the block source in the context's class (`pool_life`).
    pub(crate) fn release_unpooled(&mut self) {
        let life = self.pool_life;
        self.free_all_dedicated(life);
        crate::global_footprint::sub(self.mem_allocated);
        self.mem_allocated = 0;
        for b in self.blocks.drain(..) {
            // SAFETY: the context's drop proves no chunk in these blocks is in use; this is the sole free.
            unsafe { b.free(life) };
        }
        self.freelist = [None; NUM_FREELISTS];
        self.cur_ptr = core::ptr::null_mut();
        self.cur_end = core::ptr::null_mut();
    }

    pub(crate) fn dedicated_served(&self, p: NonNull<u8>) -> Option<bool> {
        self.dedicated.iter().find(|e| e.0 == p).and_then(|e| e.2.known(p.as_ptr()))
    }

    fn free_all_dedicated(&mut self, life: Option<crate::heap::Lifetime>) {
        for (ptr, layout, served) in self.dedicated.drain(..) {
            self.mem_allocated -= layout.size();
            crate::global_footprint::sub(layout.size());
            // SAFETY: reset/Drop's &mut proves no dedicated chunk is in use; reset/Drop resets the Led.
            unsafe { crate::heap::free_served_block(ptr, layout, life, served) };
        }
    }

    #[cold]
    #[inline(never)]
    fn alloc_new_block(&mut self, csize: usize, acct: &Acct) -> Result<NonNull<[u8]>, AllocError> {
        self.sync_active();
        let is_keeper = self.blocks.is_empty();
        let blksize = if is_keeper {
            self.init_block_size.max(csize)
        } else {
            self.next_block_size.max(csize)
        };
        acct.admit_block(blksize);
        #[cfg(all(feature = "std", not(test)))]
        let parked = if growth::on() { growth::take(blksize) } else { None };
        #[cfg(not(all(feature = "std", not(test))))]
        let parked: Option<Block> = None;
        // a growth-shelf block is never the source's (a pooled block never parks there), so it may be a kept one too
        let kept = is_keeper && self.parks(blksize).is_some() && crate::heap::keepers_park();
        let mut block = match parked {
            Some(b) => b,
            None => Block::alloc_for(blksize, &acct.led, kept)?,
        };
        self.pool_life = acct.led.pooled_life();
        self.mem_allocated += blksize;
        crate::global_footprint::add(blksize);
        if !is_keeper {
            self.next_block_size = (self.next_block_size * 2).min(self.max_block_size);
        }
        let p = block.ptr;
        block.used = csize;
        // SAFETY: csize <= blksize; both offsets stay within the new block.
        self.cur_ptr = unsafe { p.as_ptr().add(csize) };
        self.cur_end = unsafe { p.as_ptr().add(blksize) };
        self.blocks.push(block);
        acct.live_chunk_bytes.set(acct.live_chunk_bytes.get() + csize);
        self.sync_blocks(acct);
        Ok(NonNull::slice_from_raw_parts(p, csize))
    }

    /// # Safety
    /// Live allocation from [`alloc`](Self::alloc) with the same layout.
    pub(crate) unsafe fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout, acct: &Acct) {
        // ZST: dangling sentinel; writing a freelist head into it corrupts the heap.
        if layout.size() == 0 {
            return;
        }
        #[cfg(feature = "aset-guard")]
        {
            guard::dealloc(ptr, layout);
            return;
        }
        #[cfg(not(feature = "aset-guard"))]
        if is_dedicated(layout) {
            self.dealloc_dedicated(ptr, layout, acct);
            return;
        }
        let idx = free_list_index(layout.size());
        #[cfg(debug_assertions)]
        self.freed.note_free(ptr);
        core::ptr::write(ptr.as_ptr() as *mut Option<NonNull<u8>>, self.freelist[idx]);
        self.freelist[idx] = Some(ptr);
        // aset.c AllocSetFree: the chunk parks on its freelist (AllocSetStats
        // counts it as a free chunk of its class size).
        acct.live_chunk_bytes.set(acct.live_chunk_bytes.get() - chunk_size(idx));
        acct.free_chunks.set(acct.free_chunks.get() + 1);
    }

    // upstream 3f3eefc28892 (18.4): Detect pfree or repalloc of a previously-freed memory chunk.
    // C AllocSetFree/AllocSetRealloc's `requested_size == InvalidAllocSize` test.
    // Dedicated chunks leave the context at free, so "untracked" is "freed".
    #[cfg(debug_assertions)]
    pub(crate) fn is_freed(&self, ptr: NonNull<u8>, layout: Layout) -> bool {
        #[cfg(feature = "aset-guard")]
        {
            let _ = (ptr, layout);
            false
        }
        #[cfg(not(feature = "aset-guard"))]
        {
            if is_dedicated(layout) {
                return !self.dedicated.iter().any(|&(p, _, _)| p == ptr);
            }
            self.freed.contains(ptr)
        }
    }

    // A pooled chunk stays put when its size class does not change (C's AllocSetRealloc keeps
    // a chunk that already fits; here the next layout must name the same class, as `dealloc`
    // reads the class from it), and when it is the active block's last chunk and the new class
    // fits before the block's end (the bump window moves with it). Every other realloc moves
    // (C's in-place lone-block growth is not ported).
    /// # Safety: as [`dealloc`](Self::dealloc); `new_layout` must be valid.
    pub(crate) unsafe fn realloc(
        &mut self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
        acct: &Acct,
    ) -> Result<NonNull<[u8]>, AllocError> {
        #[cfg(not(feature = "aset-guard"))]
        if is_dedicated(old_layout) && is_dedicated(new_layout) && old_layout.size() != 0 {
            return self.realloc_dedicated(ptr, old_layout, new_layout, acct);
        }
        #[cfg(not(feature = "aset-guard"))]
        if old_layout.size() != 0
            && new_layout.size() != 0
            && !is_dedicated(old_layout)
            && !is_dedicated(new_layout)
        {
            let (old_idx, old_c) = idx_and_chunk(old_layout.size());
            let (new_idx, new_c) = idx_and_chunk(new_layout.size());
            if new_idx == old_idx {
                return Ok(NonNull::slice_from_raw_parts(ptr, old_c));
            }
            let start = ptr.as_ptr();
            // SAFETY: a live pooled chunk of old_c bytes; its end is at most its block's end.
            if start.add(old_c) == self.cur_ptr && new_c <= self.cur_end as usize - start as usize {
                // SAFETY: new_c fits between the chunk's start and the active block's end.
                self.cur_ptr = start.add(new_c);
                acct.live_chunk_bytes.set(acct.live_chunk_bytes.get() + new_c - old_c);
                return Ok(NonNull::slice_from_raw_parts(ptr, new_c));
            }
        }
        let new = self.alloc(new_layout, acct)?;
        let copy = old_layout.size().min(new_layout.size());
        core::ptr::copy_nonoverlapping(ptr.as_ptr(), new.cast::<u8>().as_ptr(), copy);
        self.dealloc(ptr, old_layout, acct);
        Ok(new)
    }

    // aset.c:537-597 AllocSetReset: every chunk goes (freelists emptied, the
    // keeper block alone stays, fully free).
    pub(crate) fn reset(&mut self, acct: &Acct) {
        let life = acct.led.pooled_life();
        self.free_all_dedicated(life);
        #[cfg(debug_assertions)]
        self.freed.clear();
        acct.live_chunk_bytes.set(0);
        acct.free_chunks.set(0);
        if self.blocks.is_empty() {
            self.freelist = [None; NUM_FREELISTS];
            self.next_block_size = self.init_block_size;
            acct.led.reset_mapped(self.mem_allocated);
            self.sync_blocks(acct);
            return;
        }
        for b in self.blocks.drain(1..) {
            self.mem_allocated -= b.size;
            crate::global_footprint::sub(b.size);
            // SAFETY: reset's &mut proves no chunk in these blocks is in use.
            unsafe { b.free(life) };
        }
        let keeper = &mut self.blocks[0];
        keeper.used = 0;
        let base = keeper.ptr.as_ptr();
        self.cur_ptr = base;
        // SAFETY: offset stays within the keeper block.
        self.cur_end = unsafe { base.add(keeper.size) };
        self.freelist = [None; NUM_FREELISTS];
        self.next_block_size = self.init_block_size;
        debug_assert_eq!(self.mem_allocated, self.blocks[0].size);
        let pooled = if self.blocks[0].pooled() { self.mem_allocated } else { 0 };
        acct.led.reset_blocks(self.mem_allocated, pooled);
        self.sync_blocks(acct);
    }
}

impl Drop for AllocSet {
    fn drop(&mut self) {
        let life = self.pool_life;
        self.free_all_dedicated(life);
        // Remaining block bytes (keeper included) leave this context now,
        // whether freed or parked in the recycle pool.
        crate::global_footprint::sub(self.mem_allocated);
        self.mem_allocated = 0;
        if self.blocks.is_empty() {
            return;
        }
        let mut blocks = core::mem::take(&mut self.blocks);
        // D4679: with the growth shelf on, a growth block parks for the next context's same block when it fits; a pooled
        // block (pagepool) never parks there: it goes back to the block source in the context's class, as a pooled keeper
        // never parks on a recycle list (`recycle_keeper`)
        #[cfg(all(feature = "std", not(test)))]
        let shelf = growth::on();
        for b in blocks.drain(1..) {
            #[cfg(all(feature = "std", not(test)))]
            let Some(b) = (if shelf && !b.pooled() { growth::park(b) } else { Some(b) }) else { continue };
            // SAFETY: Drop proves no chunk in these blocks is in use.
            unsafe { b.free(life) };
        }
        let keeper = &mut blocks[0];
        keeper.used = 0;
        if let Some(class) = self.parks(keeper.size) {
            recycle_keeper(class, blocks, life);
        } else {
            for b in blocks {
                // D4679: a keeper no recycle class takes (a lazy context's first block sized by its first alloc) parks
                // on the growth shelf too
                #[cfg(all(feature = "std", not(test)))]
                let Some(b) = (if shelf && !b.pooled() { growth::park(b) } else { Some(b) }) else { continue };
                // SAFETY: as above; sole free of a non-default keeper.
                unsafe { b.free(life) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::{Cell, RefCell};

    fn acct() -> Acct {
        Acct {
            name: Cell::new("aset-test"),
            ident: RefCell::new(None),
            self_used: Cell::new(0),
            self_peak: Cell::new(0),
            limit: Cell::new(usize::MAX),
            limited_path: Cell::new(false),
            arena_footprint: Cell::new(0),
            arena_nblocks: Cell::new(0),
            window_tail: Cell::new(0),
            live_chunk_bytes: Cell::new(0),
            free_chunks: Cell::new(0),
            empty_blocks: Cell::new(0),
            is_bump: false,
            wholesale_reset: Cell::new(false),
            send_root: Cell::new(false),
            unpooled: false,
            kind: "AllocSet",
            parent: None,
            children: RefCell::new(alloc::vec::Vec::new()),
            led: crate::owner::Led::NEW,
        }
    }

    #[test]
    fn free_list_index_matches_power_of_two_classes() {
        assert_eq!(free_list_index(1), 0);
        assert_eq!(free_list_index(8), 0);
        assert_eq!(free_list_index(9), 1);
        assert_eq!(free_list_index(16), 1);
        assert_eq!(free_list_index(17), 2);
        assert_eq!(free_list_index(4096), 9);
        assert_eq!(free_list_index(8192), 10);
        assert_eq!(chunk_size(0), 8);
        assert_eq!(chunk_size(10), 8192);
    }

    #[test]
    fn idx_and_chunk_matches_free_list_index_and_chunk_size() {
        for size in 1..=ALLOC_CHUNK_LIMIT {
            let (idx, csize) = idx_and_chunk(size);
            assert_eq!(idx, free_list_index(size), "idx mismatch at size {size}");
            assert_eq!(csize, chunk_size(idx), "csize mismatch at size {size}");
            assert!(csize >= size && idx < NUM_FREELISTS);
        }
    }

    #[test]
    fn cached_bump_carves_contiguously_across_a_block_boundary() {
        let mut a = AllocSet::new();
        let acct = acct();
        let l = Layout::from_size_align(64, 8).unwrap();
        let per_block = INIT_BLOCK_SIZE / 64;
        let mut ptrs = alloc::vec::Vec::new();
        for _ in 0..(per_block * 3 + 5) {
            let p = a.alloc(l, &acct).unwrap();
            let raw = p.cast::<u8>().as_ptr();
            assert_eq!(raw as usize % 8, 0);
            for &q in &ptrs {
                let q: *mut u8 = q;
                assert!(
                    (raw as usize) + 64 <= q as usize || (q as usize) + 64 <= raw as usize,
                    "overlap detected"
                );
            }
            ptrs.push(raw);
        }
        assert!(a.blocks.len() >= 3);
        let active = a.blocks.last().unwrap();
        let abase = active.ptr.as_ptr() as usize;
        assert!(a.cur_ptr as usize >= abase && a.cur_end as usize == abase + active.size);
        assert!(a.cur_ptr as usize <= a.cur_end as usize);
    }

    #[test]
    fn cached_bump_survives_reset_and_interleaved_free() {
        let mut a = AllocSet::new();
        let acct = acct();
        let l = Layout::from_size_align(48, 8).unwrap();
        let mut held = alloc::vec::Vec::new();
        for i in 0..500 {
            let p = a.alloc(l, &acct).unwrap().cast::<u8>();
            if i % 3 == 0 {
                unsafe { a.dealloc(p, l, &acct) };
            } else {
                held.push(p);
            }
        }
        a.reset(&acct);
        assert_eq!(a.blocks.len(), 1);
        assert_eq!(a.blocks[0].used, 0);
        let keeper_base = a.blocks[0].ptr.as_ptr();
        assert_eq!(a.cur_ptr, keeper_base);
        let first = a.alloc(l, &acct).unwrap().cast::<u8>().as_ptr();
        assert_eq!(first, keeper_base, "first post-reset chunk must start the keeper");
        for _ in 0..(INIT_BLOCK_SIZE / 64 + 10) {
            let _ = a.alloc(l, &acct).unwrap();
        }
        a.sync_active();
        let total_used: usize = a.blocks.iter().map(|b| b.used).sum();
        assert!(total_used > INIT_BLOCK_SIZE);
    }

    #[test]
    fn recycled_keeper_primes_cached_window() {
        let mut a = AllocSet::new();
        let acct = acct();
        assert!(a.cur_ptr.is_null() && a.cur_end.is_null());
        let l = Layout::from_size_align(32, 8).unwrap();
        let p = a.alloc(l, &acct).unwrap().cast::<u8>().as_ptr();
        assert!(!a.cur_ptr.is_null());
        let base = a.blocks[0].ptr.as_ptr();
        assert_eq!(p, base);
        assert_eq!(a.cur_end, unsafe { base.add(a.blocks[0].size) });
    }

    #[test]
    fn dedicated_routing_is_layout_pure() {
        assert!(is_dedicated(Layout::from_size_align(8193, 8).unwrap()));
        assert!(!is_dedicated(Layout::from_size_align(8192, 8).unwrap()));
        assert!(is_dedicated(Layout::from_size_align(16, 16).unwrap()));
        assert!(!is_dedicated(Layout::from_size_align(16, 8).unwrap()));
    }

    #[test]
    fn alloc_dealloc_reuses_same_size_class() {
        let mut a = AllocSet::new();
        let acct = acct();
        let l = Layout::from_size_align(24, 8).unwrap();
        let p1 = a.alloc(l, &acct).unwrap();
        assert!(p1.len() >= 24);
        assert_eq!(p1.cast::<u8>().as_ptr() as usize % 8, 0);
        unsafe { a.dealloc(p1.cast(), l, &acct) };
        let p2 = a.alloc(l, &acct).unwrap();
        assert_eq!(p1.cast::<u8>().as_ptr(), p2.cast::<u8>().as_ptr());
    }

    #[test]
    fn carving_many_chunks_grows_blocks_and_stays_aligned() {
        let mut a = AllocSet::new();
        let acct = acct();
        let l = Layout::from_size_align(64, 8).unwrap();
        let mut ptrs = alloc::vec::Vec::new();
        for _ in 0..1000 {
            let p = a.alloc(l, &acct).unwrap();
            assert_eq!(p.cast::<u8>().as_ptr() as usize % 8, 0);
            ptrs.push(p);
        }
        assert!(a.mem_allocated() > INIT_BLOCK_SIZE);
        assert!(a.blocks.len() > 1);
        for p in ptrs {
            unsafe { a.dealloc(p.cast(), l, &acct) };
        }
    }

    #[test]
    fn reset_keeps_keeper_frees_rest() {
        let mut a = AllocSet::new();
        let acct = acct();
        let l = Layout::from_size_align(4096, 8).unwrap();
        for _ in 0..10 {
            let _ = a.alloc(l, &acct).unwrap();
        }
        assert!(a.blocks.len() > 1);
        a.reset(&acct);
        assert_eq!(a.blocks.len(), 1);
        assert_eq!(a.mem_allocated(), INIT_BLOCK_SIZE);
        assert_eq!(a.blocks[0].used, 0);
        let _ = a.alloc(l, &acct).unwrap();
    }

    #[test]
    fn zero_sized_alloc_dealloc_is_a_noop_on_dangling_ptr() {
        let mut a = AllocSet::new();
        let acct = acct();
        for align in [1usize, 2, 4, 8] {
            let l = Layout::from_size_align(0, align).unwrap();
            let p = a.alloc(l, &acct).unwrap();
            assert_eq!(p.len(), 0);
            assert_eq!(p.cast::<u8>().as_ptr() as usize % align, 0);
            assert_eq!(a.mem_allocated(), 0);
            unsafe { a.dealloc(p.cast(), l, &acct) };
        }
        let dangling = unsafe { NonNull::new_unchecked(1usize as *mut u8) };
        unsafe { a.dealloc(dangling, Layout::from_size_align(0, 1).unwrap(), &acct) };
        assert!(a.freelist.iter().all(|h| h.is_none()));
    }

    #[test]
    fn keeper_pool_caps_and_recycles() {
        let pool = KeeperPool { stack: crate::PoolMutex::new(alloc::vec::Vec::new()) };
        pool.recycle(alloc::vec::Vec::new());
        assert_eq!(pool.with(|s| s.len()), 1);
        let _ = pool.take();
        assert_eq!(pool.with(|s| s.len()), 0);
        for _ in 0..MAX_FREE_CONTEXTS {
            pool.recycle(alloc::vec::Vec::new());
        }
        assert_eq!(pool.with(|s| s.len()), MAX_FREE_CONTEXTS);
        pool.recycle(alloc::vec::Vec::new());
        assert_eq!(pool.with(|s| s.len()), 1);
    }

    #[test]
    fn dedicated_large_chunk_roundtrips() {
        let mut a = AllocSet::new();
        let acct = acct();
        let l = Layout::from_size_align(100_000, 8).unwrap();
        let p = a.alloc(l, &acct).unwrap();
        assert!(p.len() >= 100_000);
        // C counts single-chunk blocks in mem_allocated (aset.c AllocSetAllocLarge).
        assert_eq!(a.mem_allocated(), 100_000);
        unsafe { a.dealloc(p.cast(), l, &acct) };
        assert_eq!(a.mem_allocated(), 0);
    }

    #[test]
    fn reset_frees_outstanding_dedicated_chunks() {
        let mut a = AllocSet::new();
        let acct = acct();
        let l = Layout::from_size_align(100_000, 8).unwrap();
        let _p1 = a.alloc(l, &acct).unwrap();
        let _p2 = a.alloc(l, &acct).unwrap();
        assert_eq!(a.mem_allocated(), 200_000);
        assert_eq!(a.dedicated.len(), 2);
        a.reset(&acct);
        assert_eq!(a.dedicated.len(), 0);
        // Only the keeper block (if any) remains charged.
        assert_eq!(a.mem_allocated(), a.blocks.first().map_or(0, |b| b.size));
    }

    #[test]
    fn realloc_moves_dedicated_chunks_and_keeps_accounting() {
        let mut a = AllocSet::new();
        let acct = acct();
        let small = Layout::from_size_align(64, 8).unwrap();
        let big = Layout::from_size_align(50_000, 8).unwrap();
        let bigger = Layout::from_size_align(120_000, 8).unwrap();
        let p = a.alloc(small, &acct).unwrap();
        let base = a.mem_allocated();
        let p2 = unsafe { a.realloc(p.cast(), small, big, &acct).unwrap() };
        assert_eq!(a.mem_allocated(), base + 50_000);
        let p3 = unsafe { a.realloc(p2.cast(), big, bigger, &acct).unwrap() };
        assert_eq!(a.mem_allocated(), base + 120_000);
        unsafe { a.dealloc(p3.cast(), bigger, &acct) };
        assert_eq!(a.mem_allocated(), base);
        assert!(a.dedicated.is_empty());
    }

    fn lay(n: usize) -> Layout {
        Layout::from_size_align(n, 8).unwrap()
    }

    fn fill(p: NonNull<[u8]>, n: usize, seed: u8) {
        for i in 0..n {
            unsafe { *p.cast::<u8>().as_ptr().add(i) = seed.wrapping_add(i as u8) };
        }
    }

    fn holds(p: NonNull<[u8]>, n: usize, seed: u8) -> bool {
        (0..n).all(|i| unsafe { *p.cast::<u8>().as_ptr().add(i) } == seed.wrapping_add(i as u8))
    }

    #[test]
    fn realloc_within_the_size_class_keeps_the_chunk() {
        let mut a = AllocSet::new_sized(INIT_BLOCK_SIZE, MAX_BLOCK_SIZE);
        let acct = acct();
        let p = a.alloc(lay(20), &acct).unwrap();
        let _q = a.alloc(lay(8), &acct).unwrap();
        fill(p, 20, 3);
        let live = acct.live_chunk_bytes.get();
        let p2 = unsafe { a.realloc(p.cast(), lay(20), lay(32), &acct).unwrap() };
        assert_eq!(p2.cast::<u8>(), p.cast::<u8>());
        assert_eq!(p2.len(), 32);
        assert!(holds(p2, 20, 3));
        let p3 = unsafe { a.realloc(p2.cast(), lay(32), lay(17), &acct).unwrap() };
        assert_eq!(p3.cast::<u8>(), p.cast::<u8>());
        assert_eq!(acct.live_chunk_bytes.get(), live);
        assert!(a.freelist.iter().all(Option::is_none));
        unsafe { a.dealloc(p3.cast(), lay(17), &acct) };
        assert_eq!(acct.live_chunk_bytes.get(), live - 32);
        assert!(a.freelist[2].is_some());
    }

    #[test]
    fn realloc_grows_and_shrinks_the_active_blocks_last_chunk_in_place() {
        let mut a = AllocSet::new_sized(INIT_BLOCK_SIZE, MAX_BLOCK_SIZE);
        let acct = acct();
        let _q = a.alloc(lay(16), &acct).unwrap();
        let p = a.alloc(lay(16), &acct).unwrap();
        fill(p, 16, 7);
        let live = acct.live_chunk_bytes.get();
        let p2 = unsafe { a.realloc(p.cast(), lay(16), lay(100), &acct).unwrap() };
        assert_eq!(p2.cast::<u8>(), p.cast::<u8>());
        assert_eq!(p2.len(), 128);
        assert!(holds(p2, 16, 7));
        assert_eq!(a.cur_ptr, unsafe { p.cast::<u8>().as_ptr().add(128) });
        assert_eq!(acct.live_chunk_bytes.get(), live + 112);
        let p3 = unsafe { a.realloc(p2.cast(), lay(100), lay(10), &acct).unwrap() };
        assert_eq!(p3.cast::<u8>(), p.cast::<u8>());
        assert_eq!(a.cur_ptr, unsafe { p.cast::<u8>().as_ptr().add(16) });
        assert_eq!(acct.live_chunk_bytes.get(), live);
        assert!(holds(p3, 10, 7));
        let r = a.alloc(lay(8), &acct).unwrap();
        assert_eq!(r.cast::<u8>().as_ptr(), unsafe { p.cast::<u8>().as_ptr().add(16) });
        assert!(a.freelist.iter().all(Option::is_none));
    }

    #[test]
    fn realloc_moves_a_chunk_that_is_not_last_or_does_not_fit() {
        let mut a = AllocSet::new_sized(INIT_BLOCK_SIZE, MAX_BLOCK_SIZE);
        let acct = acct();
        let p = a.alloc(lay(16), &acct).unwrap();
        let _q = a.alloc(lay(16), &acct).unwrap();
        fill(p, 16, 11);
        let live = acct.live_chunk_bytes.get();
        let p2 = unsafe { a.realloc(p.cast(), lay(16), lay(64), &acct).unwrap() };
        assert_ne!(p2.cast::<u8>(), p.cast::<u8>());
        assert!(holds(p2, 16, 11));
        assert_eq!(acct.live_chunk_bytes.get(), live + 64 - 16);
        assert_eq!(a.freelist[1], Some(p.cast::<u8>()));
        let room = a.cur_end as usize - p2.cast::<u8>().as_ptr() as usize;
        let want = (room + 1).next_power_of_two();
        assert!(want <= ALLOC_CHUNK_LIMIT && want > room);
        let blocks = a.blocks.len();
        let p3 = unsafe { a.realloc(p2.cast(), lay(64), lay(want), &acct).unwrap() };
        assert_ne!(p3.cast::<u8>(), p2.cast::<u8>());
        assert!(holds(p3, 16, 11));
        assert_eq!(a.blocks.len(), blocks + 1);
        assert_eq!(acct.live_chunk_bytes.get(), live - 16 + want);
    }
}
