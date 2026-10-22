//! Requests of [`MMAP_THRESHOLD`] bytes or more get their own mapping, unmapped
//! at free and resized with mremap, as glibc serves a C backend's big chunks
//! (M_MMAP_THRESHOLD, dynamic ceiling 32 MB). The route depends on size alone.

use core::alloc::Layout;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering::Relaxed};

use allocator_api2::alloc::{AllocError, Allocator, Global};

pub const MMAP_THRESHOLD: usize = 32 << 20;

/// Where a context's blocks of `min` bytes and more come from once the binary installs it (`set_block_source`); every
/// block in `[lo, lo + len)` goes back through `give`, whichever thread frees it.
#[derive(Clone, Copy)]
pub struct BlockSource {
    /// null: not served, the block takes today's road
    pub take: unsafe fn(bytes: usize, align: usize, life: Lifetime, zeroed: bool) -> *mut u8,
    /// a block of `bytes` at `align` this source served; `life`: the class it was taken in when its context knows it (every
    /// pooled block of the context taken in one class), else None
    pub give: unsafe fn(p: *mut u8, bytes: usize, align: usize, life: Option<Lifetime>),
    /// in place or moved by the source; null: the caller copies
    pub resize: unsafe fn(p: *mut u8, old: usize, new: usize, align: usize) -> *mut u8,
    pub usable: fn(bytes: usize, align: usize) -> usize,
}

/// What a context's blocks live as long as: the source keeps the classes apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Lifetime {
    Task,
    Session,
    Cache,
    Process,
}

impl Lifetime {
    /// `self as u8` back; None for any other byte.
    #[inline]
    pub(crate) fn from_u8(b: u8) -> Option<Lifetime> {
        match b {
            0 => Some(Lifetime::Task),
            1 => Some(Lifetime::Session),
            2 => Some(Lifetime::Cache),
            3 => Some(Lifetime::Process),
            _ => None,
        }
    }
}

static SOURCE: AtomicPtr<BlockSource> = AtomicPtr::new(core::ptr::null_mut());
static LO: AtomicUsize = AtomicUsize::new(0);
static LEN: AtomicUsize = AtomicUsize::new(0);
static MIN: AtomicUsize = AtomicUsize::new(usize::MAX);
static KEEPERS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// Once, before any other thread starts and any context draws through it: blocks of `min` bytes and up come from `s`;
/// `keepers` false: keeper blocks come from it too and go back at their context's drop, never parked.
pub fn set_block_source(s: &'static BlockSource, lo: usize, len: usize, min: usize, keepers: bool) {
    LO.store(lo, Relaxed);
    LEN.store(len, Relaxed);
    MIN.store(min, Relaxed);
    KEEPERS.store(keepers, Relaxed);
    SOURCE.store(s as *const BlockSource as *mut BlockSource, core::sync::atomic::Ordering::Release);
}

#[inline(always)]
fn source() -> Option<&'static BlockSource> {
    // SAFETY: only set_block_source stores here, from a `&'static BlockSource`.
    unsafe { SOURCE.load(Relaxed).as_ref() }
}

#[inline(always)]
pub(crate) fn source_set() -> bool {
    !SOURCE.load(Relaxed).is_null()
}

static OWNER: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Once, before `set_block_source`, when another allocator serves from the source's range too (the page pool's global
/// allocator, D4720): a block in the range is the source's only when `owns` says so.
pub fn set_block_owner(owns: fn(*const u8) -> bool) {
    OWNER.store(owns as *mut (), Relaxed);
}

#[inline(always)]
pub(crate) fn pooled(p: *const u8) -> bool {
    // no source: one load and a branch
    let len = LEN.load(Relaxed);
    len != 0 && (p as usize).wrapping_sub(LO.load(Relaxed)) < len && owned(p)
}

/// `pooled` for a block that memoizes its answer: the source is installed, so a block's answer stands for its life (the
/// allocator that served it owns it until it is freed); `None`: no source yet, nothing to memoize.
#[inline(always)]
pub(crate) fn pooled_stable(p: *const u8) -> Option<bool> {
    (LEN.load(Relaxed) != 0 && POOLED_MEMO.load(Relaxed)).then(|| pooled(p))
}

static POOLED_MEMO: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// A context block keeps its `pooled` answer after the first ask (on until set; exec glue sets it once per process from
/// `PGRUST_GRAVITY_POOLED_MEMO`): a per-row context's reset asks it every row, and each ask is the source's range lookup.
pub fn set_pooled_memo(on: bool) {
    POOLED_MEMO.store(on, Relaxed);
}

static POOLED_BIT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// A context's block carries which allocator served it from its allocation (`Served`; on until set, exec glue sets it once
/// per process from `PGRUST_GRAVITY_POOLED_BIT`): its frees, resets and resizes read it instead of asking `pooled`. Off:
/// every such free asks, as before (and a block's `pooled` answer is the memo's, `set_pooled_memo`).
pub fn set_pooled_bit(on: bool) {
    POOLED_BIT.store(on, Relaxed);
}

/// Which allocator served a context's block, as its allocation knew it: the source's take served it or refused it, or the
/// block never asked the source. The allocator that served a block owns it until it is freed, and the address alone
/// cannot tell the source's blocks from the global allocator's that share its range (D4720), so the source's range lookup
/// asks the source's own table: a block that carries its answer asks nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum Served {
    /// not recorded: no source was installed at the allocation (nothing is known before it, as `pooled_stable`), or the
    /// bit is off
    Unknown,
    /// not the source's: the global allocator's or its own mapping
    Other,
    Source,
}

impl Served {
    /// A block allocated now, from the source (`source`) or not.
    #[inline(always)]
    fn now(source: bool) -> Served {
        if LEN.load(Relaxed) == 0 || !POOLED_BIT.load(Relaxed) {
            return Served::Unknown;
        }
        if source {
            Served::Source
        } else {
            Served::Other
        }
    }

    /// `pooled(p)` for the block at `p` this names, without the ask: None when not recorded or the bit is off.
    #[inline(always)]
    pub(crate) fn known(self, p: *const u8) -> Option<bool> {
        if self == Served::Unknown || !POOLED_BIT.load(Relaxed) {
            return None;
        }
        let s = self == Served::Source;
        debug_assert_eq!(s, pooled(p), "mcx: a block's served bit is not the source's answer for {p:p}");
        Some(s)
    }

    #[inline(always)]
    fn pooled(self, p: *const u8) -> bool {
        self.known(p).unwrap_or_else(|| pooled(p))
    }
}

#[inline(always)]
fn owned(p: *const u8) -> bool {
    let f = OWNER.load(Relaxed);
    // SAFETY: only set_block_owner stores here, a `fn(*const u8) -> bool`.
    f.is_null() || unsafe { core::mem::transmute::<*mut (), fn(*const u8) -> bool>(f) }(p)
}

/// Whether a dropped context's keeper parks for the next context (with no source: always).
#[inline]
pub(crate) fn keepers_park() -> bool {
    !source_set() || KEEPERS.load(Relaxed)
}

#[cold]
#[inline(never)]
fn take_for(s: &BlockSource, layout: Layout, led: &crate::owner::Led) -> Option<NonNull<[u8]>> {
    // SAFETY: the source's contract: a block of `bytes` at `align`, or null.
    let life = led.lifetime();
    let p = NonNull::new(unsafe { (s.take)(layout.size(), layout.align(), life, false) })?;
    debug_assert!(pooled(p.as_ptr()), "mcx: a block source served a block outside its range");
    led.pooled_in(layout.size(), life);
    Some(NonNull::slice_from_raw_parts(p, layout.size()))
}

/// # Safety
/// `ptr` is a live block of `layout` from the source (`pooled`); `life`, when some, the class it was taken in.
#[cold]
#[inline(never)]
unsafe fn give(ptr: NonNull<u8>, layout: Layout, life: Option<Lifetime>) {
    if let Some(s) = source() {
        (s.give)(ptr.as_ptr(), layout.size(), layout.align(), life);
    }
}

/// A block of a context whose reset or drop frees it Led-less: the caller resets its Led's numbers (`Led::reset_blocks`).
/// `life`: the context's `Led::pooled_life` (read before the reset), None when not known.
///
/// # Safety
/// As `Allocator::deallocate`, and a some `life` is the class every pooled block of the context was taken in.
#[inline]
pub(crate) unsafe fn free_block(ptr: NonNull<u8>, layout: Layout, life: Option<Lifetime>) {
    if pooled(ptr.as_ptr()) {
        return give(ptr, layout, life);
    }
    dealloc_unpooled(ptr, layout);
}

/// `free_block` for a block whose allocation named its server (`served`): no ask when it is recorded.
///
/// # Safety
/// As `free_block`, and `served` is the block's.
#[inline]
pub(crate) unsafe fn free_served_block(ptr: NonNull<u8>, layout: Layout, life: Option<Lifetime>, served: Served) {
    if served.pooled(ptr.as_ptr()) {
        return give(ptr, layout, life);
    }
    dealloc_unpooled(ptr, layout);
}

/// # Safety
/// As `Allocator::deallocate`, for a block not from the source.
#[inline(always)]
unsafe fn dealloc_unpooled(ptr: NonNull<u8>, layout: Layout) {
    if !mapped(layout) {
        return Global.deallocate(ptr, layout);
    }
    door(ptr.as_ptr() as usize, layout.size(), false, None);
    #[cfg(unix)]
    os::unmap(ptr, layout.size());
}

/// A source's block freed or resized through `Heap` with no context's Led: it goes home all the same.
///
/// # Safety
/// As `Allocator::deallocate`.
#[cold]
#[inline(never)]
unsafe fn stray(ptr: NonNull<u8>, layout: Layout) {
    debug_assert!(false, "mcx: a pooled block of {} bytes freed without its context's Led", layout.size());
    give(ptr, layout, None);
}

#[derive(Clone, Copy, Default)]
pub struct Heap;

/// Whether a block of `layout` gets its own mapping (door 2): a context posts such a block at its map, not with its door-1 bytes.
#[inline(always)]
pub(crate) fn mapped(layout: Layout) -> bool {
    #[cfg(unix)]
    {
        layout.size() >= MMAP_THRESHOLD && layout.align() <= os::page_size()
    }
    #[cfg(not(unix))]
    {
        let _ = layout;
        false
    }
}

#[cfg(unix)]
mod os {
    use core::ptr::NonNull;

    pub fn page_size() -> usize {
        use core::sync::atomic::{AtomicUsize, Ordering};
        static PAGE: AtomicUsize = AtomicUsize::new(0);
        let p = PAGE.load(Ordering::Relaxed);
        if p != 0 {
            return p;
        }
        // SAFETY: sysconf is a pure query.
        let p = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as usize;
        PAGE.store(p, Ordering::Relaxed);
        p
    }

    pub fn round(size: usize) -> usize {
        let p = page_size();
        (size + p - 1) & !(p - 1)
    }

    pub fn map(size: usize) -> Option<NonNull<u8>> {
        // SAFETY: fresh private anonymous mapping; no existing memory is named.
        let p = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                round(size),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            None
        } else {
            #[cfg(feature = "std")]
            mempolicy::maps::name(p.cast(), round(size), c"pgrust:mcx");
            NonNull::new(p.cast())
        }
    }

    /// # Safety
    /// `p`/`size` name a live mapping from [`map`]/[`remap`].
    pub unsafe fn unmap(p: NonNull<u8>, size: usize) {
        libc::munmap(p.as_ptr().cast(), round(size));
    }

    /// # Safety
    /// As [`unmap`].
    pub unsafe fn remap(p: NonNull<u8>, old: usize, new: usize) -> Option<NonNull<u8>> {
        let (old_r, new_r) = (round(old), round(new));
        if old_r == new_r {
            return Some(p);
        }
        #[cfg(target_os = "linux")]
        {
            let q = libc::mremap(p.as_ptr().cast(), old_r, new_r, libc::MREMAP_MAYMOVE);
            if q == libc::MAP_FAILED {
                None
            } else {
                NonNull::new(q.cast())
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            if new_r < old_r {
                libc::munmap(p.as_ptr().add(new_r).cast(), old_r - new_r);
                return Some(p);
            }
            let q = map(new_r)?;
            core::ptr::copy_nonoverlapping(p.as_ptr(), q.as_ptr(), old);
            libc::munmap(p.as_ptr().cast(), old_r);
            Some(q)
        }
    }
}

/// G1-A door 2: the mapped block at `ptr` (its mapping's start) on (`on`) its owner or off it (C8: the ledger home's
/// `door_mapped`, the one way an mcx mapped block is charged: the map's context owner, its cell and its account, which the
/// home remembers by address and the unmap gives back to; the server's std build; the blocks under the threshold are door
/// 1's, through `Global`). LEDGER2 step 2: a context's block maps for the context's owner (`led`), not the calling
/// thread's (the home's `door_mapped_owned`); its unmap finds that owner by address as any. Out of line: `deallocate`
/// inlines into every free.
#[cold]
#[inline(never)]
pub(crate) fn door(ptr: usize, bytes: usize, on: bool, led: Option<&crate::owner::Led>) {
    #[cfg(all(feature = "std", unix))]
    match led {
        Some(led) if on => {
            let mut o = led.owner();
            memledger::charge::door_mapped_owned(ptr, os::round(bytes) as u64, &mut o);
            led.set_owner(o);
        }
        _ => memledger::charge::door_mapped(ptr, os::round(bytes) as u64, on),
    }
    #[cfg(not(all(feature = "std", unix)))]
    let _ = (ptr, bytes, on, led);
}

/// LEDGER2 step 2: `Heap::allocate` for a context's arena: a mapped block maps for the context's owner (`led`) and counts
/// in its mapped bytes; the arena's footprint update then posts nothing for it.
#[inline(always)]
pub(crate) fn allocate_for(layout: Layout, led: &crate::owner::Led) -> Result<NonNull<[u8]>, AllocError> {
    allocate_from(layout, led).map(|(p, _)| p)
}

/// `allocate_for` with the allocator that served the block.
#[inline(always)]
pub(crate) fn allocate_served_for(layout: Layout, led: &crate::owner::Led) -> Result<(NonNull<[u8]>, Served), AllocError> {
    allocate_from(layout, led).map(|(p, source)| (p, Served::now(source)))
}

/// `allocate_kept_for` with the allocator that served the block (never the source).
#[inline(always)]
pub(crate) fn allocate_kept_served_for(layout: Layout, led: &crate::owner::Led) -> Result<(NonNull<[u8]>, Served), AllocError> {
    allocate_kept_for(layout, led).map(|p| (p, Served::now(false)))
}

/// The block and whether the source's take served it (else the take was not asked or refused it).
#[inline(always)]
fn allocate_from(layout: Layout, led: &crate::owner::Led) -> Result<(NonNull<[u8]>, bool), AllocError> {
    if let Some(s) = source() {
        if layout.size() >= MIN.load(Relaxed) {
            if let Some(p) = take_for(s, layout, led) {
                return Ok((p, true));
            }
        }
    }
    allocate_kept_for(layout, led).map(|p| (p, false))
}

/// `allocate_for` for a block a reset keeps (a keeper): never the source's, so a reset leaves no pooled byte behind.
#[inline(always)]
pub(crate) fn allocate_kept_for(layout: Layout, led: &crate::owner::Led) -> Result<NonNull<[u8]>, AllocError> {
    if !mapped(layout) {
        return Global.allocate(layout);
    }
    let out = Heap.map_for(layout, led)?;
    led.mapped_in(layout);
    Ok(out)
}

/// LEDGER2 step 2: `Heap::deallocate` for a context's arena (a mapped block leaves its mapped bytes, a source's its
/// pooled ones).
///
/// # Safety
/// As `Allocator::deallocate`.
#[inline(always)]
pub(crate) unsafe fn deallocate_for(ptr: NonNull<u8>, layout: Layout, led: &crate::owner::Led) {
    if pooled(ptr.as_ptr()) {
        return give_for(ptr, layout, led);
    }
    led.unmapped(layout);
    dealloc_unpooled(ptr, layout);
}

/// `deallocate_for` for a block whose allocation named its server (`served`): no ask when it is recorded.
///
/// # Safety
/// As `deallocate_for`, and `served` is the block's.
#[inline(always)]
pub(crate) unsafe fn deallocate_served_for(ptr: NonNull<u8>, layout: Layout, led: &crate::owner::Led, served: Served) {
    if served.pooled(ptr.as_ptr()) {
        return give_for(ptr, layout, led);
    }
    led.unmapped(layout);
    dealloc_unpooled(ptr, layout);
}

/// # Safety
/// As `deallocate_for`, for a source's block.
#[cold]
#[inline(never)]
unsafe fn give_for(ptr: NonNull<u8>, layout: Layout, led: &crate::owner::Led) {
    let life = led.pooled_life();
    led.pooled_out(layout.size());
    give(ptr, layout, life);
}

/// # Safety
/// As `resize_for`, for a source's block.
#[cold]
#[inline(never)]
unsafe fn resize_pooled(ptr: NonNull<u8>, old: Layout, new: Layout, led: &crate::owner::Led) -> Result<NonNull<[u8]>, AllocError> {
    resize_pooled_served(ptr, old, new, led).map(|(q, _)| q)
}

/// `resize_pooled` with the allocator that served the new block: the source's resize keeps it the source's, a refused
/// resize moves it to whichever allocator serves `new` (the global allocator's under the source's min, or after a refused
/// take).
///
/// # Safety
/// As `resize_pooled`.
#[cold]
#[inline(never)]
unsafe fn resize_pooled_served(
    ptr: NonNull<u8>,
    old: Layout,
    new: Layout,
    led: &crate::owner::Led,
) -> Result<(NonNull<[u8]>, Served), AllocError> {
    if let Some(s) = source() {
        if let Some(q) = NonNull::new((s.resize)(ptr.as_ptr(), old.size(), new.size(), new.align())) {
            debug_assert!(pooled(q.as_ptr()), "mcx: a block source resized a block outside its range");
            led.pooled_resized(old.size(), new.size());
            return Ok((NonNull::slice_from_raw_parts(q, new.size()), Served::now(true)));
        }
    }
    let (q, served) = allocate_served_for(new, led)?;
    core::ptr::copy_nonoverlapping(ptr.as_ptr(), q.cast::<u8>().as_ptr(), old.size().min(new.size()));
    give_for(ptr, old, led);
    Ok((q, served))
}

/// LEDGER2 step 2: `Heap::grow` / `shrink` for a context's arena (a block that becomes mapped maps for `led`'s owner).
///
/// # Safety
/// As `Allocator::grow`/`shrink` (either direction).
pub(crate) unsafe fn resize_for(ptr: NonNull<u8>, old: Layout, new: Layout, led: &crate::owner::Led) -> Result<NonNull<[u8]>, AllocError> {
    if pooled(ptr.as_ptr()) {
        return resize_pooled(ptr, old, new, led);
    }
    let out = if mapped(new) && !mapped(old) {
        let q = Heap.map_for(new, led)?;
        core::ptr::copy_nonoverlapping(ptr.as_ptr(), q.cast::<u8>().as_ptr(), old.size().min(new.size()));
        dealloc_unpooled(ptr, old);
        q
    } else {
        Heap.resize(ptr, old, new)?
    };
    led.unmapped(old);
    led.mapped_in(new);
    Ok(out)
}

/// `resize_for` for a block whose allocation named its server (`served`), with the new block's: no ask when it is
/// recorded; an unrecorded one takes `resize_for` and its new block stays unrecorded.
///
/// # Safety
/// As `resize_for`, and `served` is the block's.
pub(crate) unsafe fn resize_served_for(
    ptr: NonNull<u8>,
    old: Layout,
    new: Layout,
    led: &crate::owner::Led,
    served: Served,
) -> Result<(NonNull<[u8]>, Served), AllocError> {
    let Some(pooled) = served.known(ptr.as_ptr()) else {
        return resize_for(ptr, old, new, led).map(|q| (q, Served::Unknown));
    };
    if pooled {
        return resize_pooled_served(ptr, old, new, led);
    }
    let out = if mapped(new) && !mapped(old) {
        let q = Heap.map_for(new, led)?;
        core::ptr::copy_nonoverlapping(ptr.as_ptr(), q.cast::<u8>().as_ptr(), old.size().min(new.size()));
        dealloc_unpooled(ptr, old);
        q
    } else {
        Heap.resize_unpooled(ptr, old, new)?
    };
    led.unmapped(old);
    led.mapped_in(new);
    Ok((out, Served::now(false)))
}

/// The mapped block at `ptr` about to be remapped (`on`) or left where it was (`!on`: the remap failed), so a block mapped
/// at its old address before `door_remap` is never taken for it (C8: the home's `door_moving`).
#[cold]
#[inline(never)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn door_move(ptr: usize, on: bool) {
    #[cfg(all(feature = "std", unix))]
    memledger::charge::door_moving(ptr, on);
    #[cfg(not(all(feature = "std", unix)))]
    let _ = (ptr, on);
}

/// `door` for a mapped block remapped from `old` to `new` (`old_bytes` to `new_bytes`), after its `door_move`: it stays
/// its owner's, the difference on that owner (C8: the home's `door_remapped`).
#[cold]
#[inline(never)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn door_remap(old: usize, new: usize, old_bytes: usize, new_bytes: usize) {
    #[cfg(all(feature = "std", unix))]
    memledger::charge::door_remapped(old, new, os::round(old_bytes) as u64, os::round(new_bytes) as u64);
    #[cfg(not(all(feature = "std", unix)))]
    let _ = (old, new, old_bytes, new_bytes);
}

// SAFETY (trait contract): one side serves and frees each block, by size;
// mappings are page-aligned, covering every alignment `mapped` admits.
unsafe impl Allocator for Heap {
    #[inline(always)]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if !mapped(layout) {
            return Global.allocate(layout);
        }
        self.map(layout)
    }

    #[inline(always)]
    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if !mapped(layout) {
            return Global.allocate_zeroed(layout);
        }
        self.map(layout)
    }

    #[inline(always)]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        if pooled(ptr.as_ptr()) {
            return stray(ptr, layout);
        }
        dealloc_unpooled(ptr, layout);
    }

    unsafe fn grow(
        &self,
        ptr: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        self.resize(ptr, old, new)
    }

    unsafe fn shrink(
        &self,
        ptr: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        self.resize(ptr, old, new)
    }
}

impl Heap {
    #[cold]
    fn map_for(&self, layout: Layout, led: &crate::owner::Led) -> Result<NonNull<[u8]>, AllocError> {
        #[cfg(unix)]
        {
            let p = os::map(layout.size()).ok_or(AllocError)?;
            door(p.as_ptr() as usize, layout.size(), true, Some(led));
            Ok(NonNull::slice_from_raw_parts(p, layout.size()))
        }
        #[cfg(not(unix))]
        {
            let _ = (layout, led);
            Err(AllocError)
        }
    }

    #[cold]
    fn map(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        #[cfg(unix)]
        {
            let p = os::map(layout.size()).ok_or(AllocError)?;
            door(p.as_ptr() as usize, layout.size(), true, None);
            Ok(NonNull::slice_from_raw_parts(p, layout.size()))
        }
        #[cfg(not(unix))]
        {
            let _ = layout;
            Err(AllocError)
        }
    }

    /// # Safety
    /// As `Allocator::grow`/`shrink` (either direction).
    unsafe fn resize(
        &self,
        ptr: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        if pooled(ptr.as_ptr()) {
            let q = self.allocate(new)?;
            core::ptr::copy_nonoverlapping(ptr.as_ptr(), q.cast::<u8>().as_ptr(), old.size().min(new.size()));
            stray(ptr, old);
            return Ok(q);
        }
        self.resize_unpooled(ptr, old, new)
    }

    /// `resize` of a block not from the source.
    ///
    /// # Safety
    /// As `resize`, for a block not from the source.
    unsafe fn resize_unpooled(
        &self,
        ptr: NonNull<u8>,
        old: Layout,
        new: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        match (mapped(old), mapped(new)) {
            (false, false) => {
                if new.size() >= old.size() {
                    Global.grow(ptr, old, new)
                } else {
                    Global.shrink(ptr, old, new)
                }
            }
            #[cfg(unix)]
            (true, true) => {
                door_move(ptr.as_ptr() as usize, true);
                let Some(p) = os::remap(ptr, old.size(), new.size()) else {
                    door_move(ptr.as_ptr() as usize, false);
                    return Err(AllocError);
                };
                door_remap(ptr.as_ptr() as usize, p.as_ptr() as usize, old.size(), new.size());
                Ok(NonNull::slice_from_raw_parts(p, new.size()))
            }
            _ => {
                let q = self.allocate(new)?;
                core::ptr::copy_nonoverlapping(
                    ptr.as_ptr(),
                    q.cast::<u8>().as_ptr(),
                    old.size().min(new.size()),
                );
                dealloc_unpooled(ptr, old);
                Ok(q)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_no_source_every_block_takes_todays_road() {
        assert!(!source_set() && keepers_park());
        let led = crate::owner::Led::NEW;
        let l = Layout::from_size_align(64 << 10, 8).unwrap();
        let p = allocate_for(l, &led).unwrap().cast::<u8>();
        assert!(!pooled(p.as_ptr()));
        // SAFETY: `p` is the block allocate_for handed out for `l`
        unsafe { deallocate_for(p, l, &led) };
    }
}
