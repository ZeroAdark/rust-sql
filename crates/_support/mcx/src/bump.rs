// bump.c backend: per-alloc is MAXALIGN + one window bump; ALL accounting runs at block transitions (C's mem_allocated shape), never per chunk.

use core::alloc::Layout;
use core::ptr::NonNull;

use allocator_api2::alloc::AllocError;

use crate::aset::{
    recycle_blocks, take_recycled_blocks, Block, INIT_BLOCK_SIZE, MAX_BLOCK_SIZE, POOL_MAX_ALIGN,
};
use crate::Acct;

// C's allocChunkLimit policy (8 chunks per max block), kept although Rust blocks carry no header.
const BUMP_BLOCK_HDR_SZ: usize = 40;
#[cfg(test)]
pub(crate) const BUMP_CHUNK_LIMIT: usize = chunk_limit(MAX_BLOCK_SIZE);

fn fast_limit(chunk_limit: usize) -> u32 {
    if crate::bump_fast_on() {
        chunk_limit as u32
    } else {
        0
    }
}

const _: () = assert!(chunk_limit(MAX_BLOCK_SIZE) <= u32::MAX as usize);

const fn chunk_limit(max_block_size: usize) -> usize {
    let bound = (max_block_size - BUMP_BLOCK_HDR_SZ) / 8;
    let mut limit = max_block_size;
    while limit > bound {
        limit >>= 1;
    }
    limit
}

pub(crate) struct BumpArena {
    // blocks[0] is the keeper (retained over reset, uncharged until claimed).
    blocks: alloc::vec::Vec<Block>,
    // Dedicated oversize/overaligned chunks, with the allocator that served each; freed wholesale at reset.
    oversize: alloc::vec::Vec<(NonNull<u8>, Layout, crate::heap::Served)>,
    // Invariant: windows null until the keeper is claimed, else cur_ptr <= cur_end in the active block.
    cur_ptr: *mut u8,
    cur_end: *mut u8,
    // The keeper's base while the keeper is the arena's only memory and the window is open over it (no second block, no
    // oversize chunk): a reset is then `cur_ptr = keeper_base` and nothing else. Null otherwise.
    keeper_base: *mut u8,
    keeper_size: u32,
    retain_cap: u32,
    reuse: u32,
    // The chunk limit of the inline window hit (Mcx::alloc_fast via try_bump_fast): chunk_limit while the
    // PGRUST_MCX_BUMP_FAST switch is on (default), 0 while off (no hit inline: every allocation takes allocate_slow, the
    // old route). Copied once at the arena's creation so the switch costs the inline path nothing (it sits in the
    // u32 hole: the arena does not grow).
    fast_limit: u32,
    next_block_size: usize,
    max_block_size: usize,
    chunk_limit: usize,
    mem_allocated: usize,
}

const _: () = assert!(core::mem::size_of::<BumpArena>() <= 128);

impl BumpArena {
    pub(crate) fn new() -> BumpArena {
        Self::with_max_block_size(MAX_BLOCK_SIZE)
    }

    pub(crate) fn with_max_block_size(max_block_size: usize) -> BumpArena {
        assert!(max_block_size.is_power_of_two());
        assert!((INIT_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&max_block_size));
        Self::from_blocks(take_recycled_blocks(), max_block_size)
    }

    // Session-root retirement: the keeper goes too; the next alloc grows one.
    pub(crate) fn release_keeper(&mut self) {
        let fast_limit = self.fast_limit;
        *self = Self::from_blocks(alloc::vec::Vec::new(), self.max_block_size);
        self.fast_limit = fast_limit;
    }

    fn from_blocks(blocks: alloc::vec::Vec<Block>, max_block_size: usize) -> BumpArena {
        debug_assert!(blocks.is_empty() || (blocks.len() == 1 && blocks[0].used == 0));
        let mem_allocated = blocks.first().map_or(0, |b| b.size);
        crate::global_footprint::add(mem_allocated);
        BumpArena {
            blocks,
            oversize: alloc::vec::Vec::new(),
            cur_ptr: core::ptr::null_mut(),
            cur_end: core::ptr::null_mut(),
            keeper_base: core::ptr::null_mut(),
            keeper_size: INIT_BLOCK_SIZE as u32,
            retain_cap: 0,
            reuse: 1,
            fast_limit: fast_limit(chunk_limit(max_block_size)),
            next_block_size: INIT_BLOCK_SIZE,
            max_block_size,
            chunk_limit: chunk_limit(max_block_size),
            mem_allocated,
        }
    }

    /// A window arena: a `keeper_size` keeper (allocated at the first allocation, never from the recycle pool) and up to
    /// `retain_cap` bytes of extra blocks kept across reset and reused in order.
    pub(crate) fn window(keeper_size: u32, retain_cap: u32) -> BumpArena {
        BumpArena {
            blocks: alloc::vec::Vec::new(),
            oversize: alloc::vec::Vec::new(),
            cur_ptr: core::ptr::null_mut(),
            cur_end: core::ptr::null_mut(),
            keeper_base: core::ptr::null_mut(),
            keeper_size,
            retain_cap,
            reuse: 1,
            fast_limit: fast_limit(chunk_limit(MAX_BLOCK_SIZE)),
            next_block_size: INIT_BLOCK_SIZE,
            max_block_size: MAX_BLOCK_SIZE,
            chunk_limit: chunk_limit(MAX_BLOCK_SIZE),
            mem_allocated: 0,
        }
    }

    pub(crate) fn footprint(&self) -> usize {
        self.mem_allocated
    }

    pub(crate) fn nblocks(&self) -> usize {
        self.blocks.len() + self.oversize.len()
    }

    pub(crate) fn mem_allocated(&self) -> usize {
        self.mem_allocated + if self.blocks.is_empty() { INIT_BLOCK_SIZE } else { 0 }
    }

    pub(crate) fn window_tail(&self) -> usize {
        self.cur_end as usize - self.cur_ptr as usize
    }

    #[inline]
    pub(crate) fn alloc(
        &mut self,
        layout: Layout,
        acct: &Acct,
    ) -> Result<NonNull<[u8]>, AllocError> {
        if layout.align() > POOL_MAX_ALIGN {
            return self.alloc_dedicated(layout, acct);
        }
        let csize = (layout.size() + 7) & !7;
        // wrapping_sub folds the ZST case into the dedicated branch.
        if csize.wrapping_sub(1) >= self.chunk_limit {
            return self.alloc_dedicated(layout, acct);
        }
        let avail = self.cur_end as usize - self.cur_ptr as usize;
        if csize <= avail {
            let p = self.cur_ptr;
            // SAFETY: csize <= avail keeps the bump inside the active block; p non-null there.
            unsafe {
                self.cur_ptr = p.add(csize);
                return Ok(NonNull::slice_from_raw_parts(NonNull::new_unchecked(p), csize));
            }
        }
        self.alloc_from_new_block(csize, acct)
    }

    #[inline(always)]
    pub(crate) fn try_bump(&mut self, n: usize) -> Option<NonNull<u8>> {
        let csize = (n + 7) & !7;
        let avail = self.cur_end as usize - self.cur_ptr as usize;
        if csize.wrapping_sub(1) >= self.chunk_limit || csize > avail {
            return None;
        }
        let p = self.cur_ptr;
        // SAFETY: csize <= avail keeps the bump inside the active block; p is non-null there (avail > 0).
        unsafe {
            self.cur_ptr = p.add(csize);
            Some(NonNull::new_unchecked(p))
        }
    }

    // Mcx::alloc_fast's window hit: alloc()'s plain bump (a non-ZST, non-dedicated size that fits the open window), which
    // touches no accounting (bump accounting runs at block transitions only), so a hit here leaves exactly the state
    // alloc() would. None = nothing touched; the caller then takes alloc(). The caller checks align <= POOL_MAX_ALIGN.
    #[inline(always)]
    pub(crate) fn try_bump_fast(&mut self, size: usize) -> Option<NonNull<[u8]>> {
        let csize = (size + 7) & !7;
        let avail = self.cur_end as usize - self.cur_ptr as usize;
        if csize.wrapping_sub(1) >= self.fast_limit as usize || csize > avail {
            return None;
        }
        let p = self.cur_ptr;
        // SAFETY: csize <= avail keeps the bump inside the active block; p is non-null there (avail > 0).
        unsafe {
            self.cur_ptr = p.add(csize);
            Some(NonNull::slice_from_raw_parts(NonNull::new_unchecked(p), csize))
        }
    }

    #[cfg(test)]
    pub(crate) fn set_fast(&mut self, on: bool) {
        self.fast_limit = if on { self.chunk_limit as u32 } else { 0 };
    }

    #[inline(always)]
    pub(crate) fn rewind_keeper(&mut self) -> bool {
        if self.keeper_base.is_null() {
            return false;
        }
        self.cur_ptr = self.keeper_base;
        true
    }

    #[cold]
    #[inline(never)]
    fn alloc_from_new_block(
        &mut self,
        csize: usize,
        acct: &Acct,
    ) -> Result<NonNull<[u8]>, AllocError> {
        // Retained keeper (fresh arena or post-reset): claim and charge it now.
        if self.cur_ptr.is_null() {
            if let Some(k) = self.blocks.first() {
                if csize <= k.size {
                    acct.check_limit(k.size)?;
                    acct.commit_block(k.size, self.mem_allocated, self.nblocks());
                    let base = k.ptr.as_ptr();
                    // SAFETY: csize <= k.size; both offsets stay in the keeper.
                    unsafe {
                        self.cur_ptr = base.add(csize);
                        self.cur_end = base.add(k.size);
                    }
                    acct.window_tail.set(k.size - csize);
                    self.keeper_base = base;
                    return Ok(NonNull::slice_from_raw_parts(k.ptr, csize));
                }
            }
        }
        let is_keeper = self.blocks.is_empty();
        let reuse = self.reuse as usize;
        if !is_keeper && reuse < self.blocks.len() && csize <= self.blocks[reuse].size {
            let b = &self.blocks[reuse];
            let base = b.ptr.as_ptr();
            // SAFETY: csize <= b.size; both offsets stay in the kept block.
            unsafe {
                self.cur_ptr = base.add(csize);
                self.cur_end = base.add(b.size);
            }
            acct.window_tail.set(b.size - csize);
            self.keeper_base = core::ptr::null_mut();
            self.reuse += 1;
            return Ok(NonNull::slice_from_raw_parts(b.ptr, csize));
        }
        let mut blksize = if is_keeper {
            self.keeper_size as usize
        } else {
            let b = self.next_block_size;
            self.next_block_size = (b * 2).min(self.max_block_size);
            b
        };
        if blksize < csize {
            blksize = csize.next_power_of_two();
        }
        acct.check_limit(blksize)?;
        acct.admit_block(blksize);
        self.blocks.try_reserve(1).map_err(|_| AllocError)?;
        let block = Block::alloc_for(blksize, &acct.led, is_keeper || self.retains(blksize))?;
        self.mem_allocated += blksize;
        crate::global_footprint::add(blksize);
        acct.commit_block(blksize, self.mem_allocated, self.nblocks() + 1);
        let p = block.ptr;
        // SAFETY: csize <= blksize; both offsets stay within the new block.
        unsafe {
            self.cur_ptr = p.as_ptr().add(csize);
            self.cur_end = p.as_ptr().add(blksize);
        }
        acct.window_tail.set(blksize - csize);
        self.keeper_base = if is_keeper && self.oversize.is_empty() { p.as_ptr() } else { core::ptr::null_mut() };
        self.blocks.push(block);
        self.reuse = self.blocks.len() as u32;
        Ok(NonNull::slice_from_raw_parts(p, csize))
    }

    // ZST, over-aligned or over-chunk-limit request (C's BumpAllocLarge
    // lane); a ZST of any alignment is dangling and uncharged.
    #[cold]
    #[inline(never)]
    fn alloc_dedicated(
        &mut self,
        layout: Layout,
        acct: &Acct,
    ) -> Result<NonNull<[u8]>, AllocError> {
        let size = layout.size();
        if size == 0 {
            let dangling = layout.align() as *mut u8;
            // SAFETY: align is a non-zero power of two, hence non-null.
            let nn = unsafe { NonNull::new_unchecked(dangling) };
            return Ok(NonNull::slice_from_raw_parts(nn, 0));
        }
        self.oversize.try_reserve(1).map_err(|_| AllocError)?;
        acct.check_limit(size)?;
        acct.admit_block(size);
        let (p, served) = crate::heap::allocate_served_for(layout, &acct.led)?;
        self.mem_allocated += size;
        crate::global_footprint::add(size);
        self.oversize.push((p.cast(), layout, served));
        self.keeper_base = core::ptr::null_mut();
        acct.commit_block(size, self.mem_allocated, self.nblocks());
        acct.window_tail.set(self.window_tail());
        Ok(p)
    }

    /// # Safety
    /// `ptr`: live from this arena's alloc/grow with `old_layout`; `new_layout.size() >= old_layout.size()`.
    pub(crate) unsafe fn grow(
        &mut self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
        acct: &Acct,
    ) -> Result<NonNull<[u8]>, AllocError> {
        if old_layout.align() <= POOL_MAX_ALIGN && new_layout.align() <= POOL_MAX_ALIGN {
            let old_csize = (old_layout.size() + 7) & !7;
            let new_csize = (new_layout.size() + 7) & !7;
            // Last-chunk in-place extension (what a growing PgVec hits).
            if ptr.as_ptr().add(old_csize) == self.cur_ptr
                && new_csize - old_csize <= self.cur_end as usize - self.cur_ptr as usize
                && new_csize < self.chunk_limit
            {
                self.cur_ptr = ptr.as_ptr().add(new_csize);
                return Ok(NonNull::slice_from_raw_parts(ptr, new_csize));
            }
        }
        self.grow_moved(ptr, old_layout, new_layout, acct)
    }

    // Off the tail-extension fast path. A dedicated (oversize/overaligned)
    // chunk reallocs through the heap (mremap when mapped) like C's AllocSet repalloc of a
    // dedicated block: the old block is released by the system allocator,
    // so the peak is old+new only while it copies (string_agg's buffer).
    // Anything else — a block chunk, including one whose new size crosses
    // chunk_limit and so promotes to a dedicated chunk — is alloc+copy and
    // the old chunk stays dead in its block until reset (bump.c has no
    // per-chunk free; C's AllocSet would have freed that small chunk into
    // a freelist, a bounded difference of at most chunk_limit bytes).
    #[cold]
    #[inline(never)]
    unsafe fn grow_moved(
        &mut self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
        acct: &Acct,
    ) -> Result<NonNull<[u8]>, AllocError> {
        if let Some(i) = self.oversize.iter().position(|(p, _, _)| *p == ptr) {
            let (_, cur_layout, served) = self.oversize[i];
            let new_size = new_layout.size().max(cur_layout.size());
            let new_layout =
                Layout::from_size_align(new_size, new_layout.align()).map_err(|_| AllocError)?;
            let delta = new_size - cur_layout.size();
            acct.check_limit(delta)?;
            acct.admit_block(delta);
            // SAFETY: (ptr, cur_layout) is the live heap allocation recorded
            // for this chunk; new_size >= cur_layout.size().
            let (new, served) = unsafe { crate::heap::resize_served_for(ptr, cur_layout, new_layout, &acct.led, served)? };
            self.oversize[i] = (new.cast(), new_layout, served);
            self.mem_allocated += delta;
            crate::global_footprint::add(delta);
            acct.commit_block(delta, self.mem_allocated, self.nblocks());
            return Ok(new);
        }
        let new = self.alloc(new_layout, acct)?;
        core::ptr::copy_nonoverlapping(ptr.as_ptr(), new.cast::<u8>().as_ptr(), old_layout.size());
        Ok(new)
    }

    // Post-reset the window re-opens over the keeper (kept charged by the
    // caller): the next alloc takes the plain bump path, as C's BumpReset
    // leaves the keeper in mem_allocated with freeptr rewound. The common
    // reset (everything fit in the keeper) frees nothing — window rewind only.
    #[inline]
    pub(crate) fn reset(&mut self) {
        if !self.oversize.is_empty() || self.blocks.len() > 1 {
            self.free_extra_blocks();
        }
        match self.blocks.first_mut() {
            Some(k) => {
                k.used = 0;
                let base = k.ptr.as_ptr();
                self.cur_ptr = base;
                // SAFETY: base + k.size is the keeper's one-past-the-end.
                self.cur_end = unsafe { base.add(k.size) };
                self.keeper_base = base;
            }
            None => {
                self.cur_ptr = core::ptr::null_mut();
                self.cur_end = core::ptr::null_mut();
                self.keeper_base = core::ptr::null_mut();
            }
        }
        self.next_block_size = INIT_BLOCK_SIZE;
        self.reuse = 1;
        debug_assert_eq!(self.mem_allocated, self.blocks.iter().map(|b| b.size).sum::<usize>());
        debug_assert!(!self.blocks.iter().any(|b| crate::heap::pooled(b.ptr.as_ptr())), "mcx: a block source's block kept over a reset");
    }

    pub(crate) fn oversize_served(&self, p: NonNull<u8>) -> Option<bool> {
        self.oversize.iter().find(|e| e.0 == p).and_then(|e| e.2.known(p.as_ptr()))
    }

    /// Whether a new block of `size` bytes is one a reset keeps (`retain_cap`): never the block source's.
    fn retains(&self, size: usize) -> bool {
        self.retain_cap != 0 && self.blocks.iter().skip(1).map(|b| b.size).sum::<usize>() + size <= self.retain_cap as usize
    }

    #[cold]
    #[inline(never)]
    fn free_extra_blocks(&mut self) {
        for (p, layout, served) in self.oversize.drain(..) {
            self.mem_allocated -= layout.size();
            crate::global_footprint::sub(layout.size());
            // SAFETY: reset's &mut proves no oversize chunk is still in use; the reset's caller resets the Led.
            unsafe { crate::heap::free_served_block(p, layout, None, served) };
        }
        if self.blocks.len() > 1 {
            let mut keep = 1;
            let mut kept = 0usize;
            while keep < self.blocks.len() && kept + self.blocks[keep].size <= self.retain_cap as usize {
                kept += self.blocks[keep].size;
                keep += 1;
            }
            for b in self.blocks.drain(keep..) {
                self.mem_allocated -= b.size;
                crate::global_footprint::sub(b.size);
                // SAFETY: reset's &mut proves no chunk in these blocks is in use.
                unsafe { b.free(None) };
            }
        }
    }
}

impl Drop for BumpArena {
    fn drop(&mut self) {
        self.retain_cap = 0;
        self.reset();
        // The keeper's bytes leave this context now, whether freed or parked
        // in the recycle pool.
        crate::global_footprint::sub(self.mem_allocated);
        self.mem_allocated = 0;
        if self.blocks.is_empty() {
            return;
        }
        let blocks = core::mem::take(&mut self.blocks);
        if blocks[0].size == INIT_BLOCK_SIZE {
            recycle_blocks(blocks);
        } else {
            for b in blocks {
                // SAFETY: Drop proves no chunk is in use; sole free.
                unsafe { b.free(None) };
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
            name: Cell::new("bump-test"),
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
            is_bump: true,
            wholesale_reset: Cell::new(false),
            send_root: Cell::new(false),
            unpooled: false,
            kind: "Bump",
            parent: None,
            children: RefCell::new(alloc::vec::Vec::new()),
            led: crate::owner::Led::NEW,
        }
    }

    #[test]
    fn capped_blocks_survive_reset_and_grow_crosses_chunk_limit() {
        let mut a = BumpArena::with_max_block_size(INIT_BLOCK_SIZE);
        let acct = acct();
        let layout = Layout::from_size_align(200, 8).unwrap();
        for _ in 0..2000 {
            a.alloc(layout, &acct).unwrap();
        }
        assert!(a.blocks.len() > 2);
        assert!(a.blocks.iter().all(|block| block.size <= INIT_BLOCK_SIZE));
        assert!(a.oversize.is_empty());
        a.reset();
        acct.self_used.set(a.footprint());
        let old = Layout::from_size_align(64, 8).unwrap();
        let large = Layout::from_size_align(1024, 8).unwrap();
        let p = a.alloc(old, &acct).unwrap().cast::<u8>();
        // SAFETY: p owns 64 live writable bytes; grow preserves that prefix.
        unsafe {
            p.as_ptr().write_bytes(0x5a, old.size());
            let q = a.grow(p, old, large, &acct).unwrap().cast::<u8>();
            assert_eq!(core::slice::from_raw_parts(q.as_ptr(), old.size()), &[0x5a; 64]);
        }
        assert_eq!(a.oversize.len(), 1);
        assert_eq!(a.oversize[0].1.size(), 1024);
        for _ in 0..200 {
            a.alloc(layout, &acct).unwrap();
        }
        assert!(a.blocks.iter().all(|block| block.size <= INIT_BLOCK_SIZE));
    }

    #[test]
    fn bump_carves_contiguously_and_stays_aligned() {
        let mut a = BumpArena::new();
        let acct = acct();
        let l = Layout::from_size_align(24, 8).unwrap();
        let mut last: Option<*mut u8> = None;
        for _ in 0..1000 {
            let p = a.alloc(l, &acct).unwrap().cast::<u8>().as_ptr();
            assert_eq!(p as usize % 8, 0);
            if let Some(q) = last {
                assert_ne!(p, q);
            }
            last = Some(p);
        }
        assert!(a.footprint() >= 1000 * 24);
        assert!(acct.self_used.get() >= 1000 * 24, "block charges cover the chunks");
        assert_eq!(acct.self_used.get() % 8, 0);
    }

    #[test]
    fn charge_happens_at_block_transitions_only() {
        let mut a = BumpArena::new();
        let acct = acct();
        let l = Layout::from_size_align(64, 8).unwrap();
        let _ = a.alloc(l, &acct).unwrap();
        let after_first = acct.self_used.get();
        assert_eq!(after_first, INIT_BLOCK_SIZE, "first alloc charges the whole keeper");
        for _ in 0..(INIT_BLOCK_SIZE / 64 - 1) {
            let _ = a.alloc(l, &acct).unwrap();
        }
        assert_eq!(acct.self_used.get(), after_first, "in-block allocs charge nothing");
        let _ = a.alloc(l, &acct).unwrap();
        assert!(acct.self_used.get() > after_first, "block transition charges");
        assert_eq!(acct.arena_footprint.get(), a.footprint());
        assert_eq!(acct.arena_nblocks.get(), a.nblocks());
    }

    #[test]
    fn reset_keeps_keeper_with_window_open() {
        let mut a = BumpArena::new();
        let acct = acct();
        let l = Layout::from_size_align(4096, 8).unwrap();
        for _ in 0..10 {
            let _ = a.alloc(l, &acct).unwrap();
        }
        assert!(a.blocks.len() > 1);
        a.reset();
        // Post-reset accounting is the caller's (MemoryContext::reset keeps the
        // keeper charged); the arena's window must already be open on it.
        acct.self_used.set(a.blocks[0].size);
        assert_eq!(a.blocks.len(), 1);
        assert_eq!(a.footprint(), a.blocks[0].size);
        let keeper_base = a.blocks[0].ptr.as_ptr();
        assert_eq!(a.cur_ptr, keeper_base, "window re-opens on the keeper at reset");
        let charged = acct.self_used.get();
        let p = a.alloc(l, &acct).unwrap().cast::<u8>().as_ptr();
        assert_eq!(p, keeper_base, "first post-reset chunk starts the keeper");
        assert_eq!(acct.self_used.get(), charged, "no re-charge on the fast path");
    }

    #[test]
    fn zero_sized_alloc_is_dangling_and_uncharged() {
        let mut a = BumpArena::new();
        let acct = acct();
        for align in [1usize, 2, 4, 8, 16, 32, 4096] {
            let l = Layout::from_size_align(0, align).unwrap();
            let p = a.alloc(l, &acct).unwrap();
            assert_eq!(p.len(), 0);
            assert_eq!(p.cast::<u8>().as_ptr() as usize % align, 0);
            assert_eq!((a.footprint(), a.nblocks()), (0, 0), "align={align}");
        }
        assert_eq!(acct.self_used.get(), 0);
        assert_eq!(a.footprint(), 0);
    }

    #[test]
    fn oversize_and_overaligned_get_dedicated_chunks() {
        let mut a = BumpArena::new();
        let acct = acct();
        let big = Layout::from_size_align(BUMP_CHUNK_LIMIT + 1, 8).unwrap();
        let p = a.alloc(big, &acct).unwrap();
        assert!(p.len() >= BUMP_CHUNK_LIMIT + 1);
        let al = Layout::from_size_align(64, 64).unwrap();
        let q = a.alloc(al, &acct).unwrap();
        assert_eq!(q.cast::<u8>().as_ptr() as usize % 64, 0);
        assert_eq!(a.oversize.len(), 2);
        assert!(acct.self_used.get() > BUMP_CHUNK_LIMIT);
        a.reset();
        assert!(a.oversize.is_empty());
    }

    #[test]
    fn grow_extends_last_chunk_in_place() {
        let mut a = BumpArena::new();
        let acct = acct();
        let old = Layout::from_size_align(32, 8).unwrap();
        let new = Layout::from_size_align(64, 8).unwrap();
        let p = a.alloc(old, &acct).unwrap().cast::<u8>();
        // SAFETY: p live from alloc with `old`.
        let q = unsafe { a.grow(p, old, new, &acct) }.unwrap();
        assert_eq!(q.cast::<u8>().as_ptr(), p.as_ptr(), "in-place extension");
        let _hole = a.alloc(old, &acct).unwrap();
        let bigger = Layout::from_size_align(128, 8).unwrap();
        // SAFETY: q live from grow with `new`.
        let r = unsafe { a.grow(q.cast(), new, bigger, &acct) }.unwrap();
        assert_ne!(r.cast::<u8>().as_ptr(), p.as_ptr(), "non-tail grow moves");
    }

    #[test]
    fn grow_dedicated_reallocs_and_charges_delta_only() {
        let mut a = BumpArena::new();
        let acct = acct();
        let old = Layout::from_size_align(BUMP_CHUNK_LIMIT + 8, 8).unwrap();
        let new = Layout::from_size_align(4 * BUMP_CHUNK_LIMIT, 8).unwrap();
        let p = a.alloc(old, &acct).unwrap().cast::<u8>();
        assert_eq!(a.oversize.len(), 1);
        assert_eq!(a.footprint(), old.size());
        let used0 = acct.self_used.get();
        // SAFETY: p owns old.size() live bytes; grow preserves that prefix.
        let q = unsafe {
            p.as_ptr().write_bytes(0xa5, old.size());
            let q = a.grow(p, old, new, &acct).unwrap().cast::<u8>();
            assert!(core::slice::from_raw_parts(q.as_ptr(), old.size()).iter().all(|&b| b == 0xa5));
            q
        };
        assert_eq!(a.oversize.len(), 1, "realloc replaces the entry, never appends");
        assert_eq!(a.oversize[0].0, q);
        assert_eq!(a.oversize[0].1.size(), new.size());
        assert_eq!(a.blocks.len(), 0, "no block was opened for the copy");
        assert_eq!(a.footprint(), new.size(), "old dedicated bytes are released, not retained");
        assert_eq!(acct.self_used.get(), used0 + (new.size() - old.size()));
        assert_eq!(acct.arena_footprint.get(), a.footprint());
        assert_eq!(acct.arena_nblocks.get(), 1);
        // Second grow on the moved pointer keeps one entry.
        let bigger = Layout::from_size_align(8 * BUMP_CHUNK_LIMIT, 8).unwrap();
        // SAFETY: q live from grow with `new`.
        let r = unsafe { a.grow(q, new, bigger, &acct) }.unwrap().cast::<u8>();
        assert_eq!(a.oversize.len(), 1);
        assert_eq!(a.oversize[0].0, r);
        assert_eq!(a.footprint(), bigger.size());
        a.reset();
        assert!(a.oversize.is_empty());
        assert_eq!(a.footprint(), 0, "reset frees the grown dedicated chunk");
    }

    #[test]
    fn grow_promotes_block_chunk_to_dedicated_then_reallocs() {
        let mut a = BumpArena::new();
        let acct = acct();
        let small = Layout::from_size_align(64, 8).unwrap();
        let p = a.alloc(small, &acct).unwrap().cast::<u8>();
        let _hole = a.alloc(small, &acct).unwrap();
        let big = Layout::from_size_align(BUMP_CHUNK_LIMIT + 8, 8).unwrap();
        // SAFETY: p owns 64 live bytes; grow preserves that prefix.
        let q = unsafe {
            p.as_ptr().write_bytes(0x3c, small.size());
            let q = a.grow(p, small, big, &acct).unwrap().cast::<u8>();
            assert_eq!(core::slice::from_raw_parts(q.as_ptr(), 64), &[0x3c; 64]);
            q
        };
        assert_eq!(a.oversize.len(), 1, "crossing chunk_limit promotes to a dedicated chunk");
        assert_eq!(a.blocks.len(), 1, "the dead 64-byte chunk stays in the keeper");
        assert_eq!(a.footprint(), INIT_BLOCK_SIZE + big.size());
        let bigger = Layout::from_size_align(2 * BUMP_CHUNK_LIMIT, 8).unwrap();
        // SAFETY: q live from grow with `big`.
        let r = unsafe { a.grow(q, big, bigger, &acct) }.unwrap().cast::<u8>();
        assert_eq!(a.oversize.len(), 1);
        assert_eq!(a.oversize[0].0, r);
        assert_eq!(a.footprint(), INIT_BLOCK_SIZE + bigger.size());
        // Overaligned dedicated chunks realloc the same way.
        let al = Layout::from_size_align(64, 64).unwrap();
        let al2 = Layout::from_size_align(4096, 64).unwrap();
        let s = a.alloc(al, &acct).unwrap().cast::<u8>();
        // SAFETY: s live from alloc with `al`.
        let t = unsafe { a.grow(s, al, al2, &acct) }.unwrap().cast::<u8>();
        assert_eq!(t.as_ptr() as usize % 64, 0);
        assert_eq!(a.oversize.len(), 2);
        assert_eq!(a.footprint(), INIT_BLOCK_SIZE + bigger.size() + al2.size());
        a.reset();
        assert!(a.oversize.is_empty());
        assert_eq!(a.footprint(), INIT_BLOCK_SIZE);
    }
}
