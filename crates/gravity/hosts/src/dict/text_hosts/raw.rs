//! `text_hosts`' unsafe steps (rule R6, the module's raw home): the decode arena's byte view and a decoded row's
//! payload, the regions the table builds write into, the `Fp128` header's reads and writes, the dictionary tables'
//! unchecked readers, a fmgr text result's bytes, and the pages advice of the build buffers and the reuse pool.
#![allow(unsafe_code)]

use gravity_abi::PtrWord;
use super::*;

use gravity_dict::table::Header;

// SAFETY: the table is written once at the build and read-only afterwards; `hdr` points into `buf`.
unsafe impl Send for FpTable {}
unsafe impl Sync for FpTable {}

/// A table build's buffer (`reuse`): the global allocator's `Vec<u64>`, or pagepool P3's (`PGRUST_POOL_GRAVITY=1`): a page
/// pool block or span (`Tag::BUILD`, session-lived, its old words as they are) with its own charge (`Door::Pool`), moved
/// with the buffer from owner to owner (`own`) and off at its drop. Its words read as a `[u64]`.
pub struct BuildBuf {
    mem: Mem,
}

enum Mem {
    Heap(Vec<u64>),
    /// `dead`: the idle release gave its whole runs to the pool (`give_dead`), their bytes off its charge.
    Pool { ptr: core::ptr::NonNull<u64>, words: usize, charge: pgsync::Mutex<memledger::Charge>, dead: bool },
}

// SAFETY: the buffer owns its words (a pool block is named by no one else); its charge is behind a lock.
unsafe impl Send for BuildBuf {}
unsafe impl Sync for BuildBuf {}

/// pagepool P3: the pool's dead-page grain (`pagepool::give_pages` takes whole runs of it).
const POOL_RUN: usize = 64 << 10;

/// pagepool P3: a build buffer from the page pool, session-lived (kept buffers outlive their statement), not zeroed (a
/// build reads only the words it wrote).
const POOL_HINT: pagepool::Hint = pagepool::Hint::new(pagepool::Class::Session, pagepool::Tag::BUILD);

impl BuildBuf {
    /// pagepool P3: `words` words or more from the page pool, on the calling thread's owner (where the allocator's bytes
    /// would be) until `own` moves them; `None` with the switch off or a refusal.
    pub(super) fn pool(words: usize) -> Option<BuildBuf> {
        if !(pagepool::on() && pagepool::switches().gravity) || words == 0 {
            return None;
        }
        let b = pagepool::take(words.checked_mul(8)?, 64, POOL_HINT)?;
        let owner = memledger::charge::context().0;
        let charge = memledger::Charge::open(owner, memledger::charge::release_of(owner), memledger::Kind::Region, memledger::charge::Door::Pool, b.len() as u64);
        Some(BuildBuf { mem: Mem::Pool { ptr: b.cast::<u64>(), words: b.len() / 8, charge: pgsync::Mutex::new(charge), dead: false } })
    }

    /// The words the buffer holds (a `Vec`'s capacity).
    pub fn capacity(&self) -> usize {
        match &self.mem {
            Mem::Heap(v) => v.capacity(),
            Mem::Pool { words, .. } => *words,
        }
    }

    /// The allocator's bytes it holds (a door-1 charge's); 0 for a pool buffer, whose bytes are on its own charge.
    pub(super) fn heap_bytes(&self) -> u64 {
        match &self.mem {
            Mem::Heap(v) => (v.capacity() * 8) as u64,
            Mem::Pool { .. } => 0,
        }
    }

    pub(super) fn is_pooled(&self) -> bool {
        matches!(self.mem, Mem::Pool { .. })
    }

    /// pagepool P3: a pool buffer's bytes on `owner` from now (nothing for the allocator's: its charges are its holder's).
    pub(super) fn own(&self, owner: memledger::Owner) {
        if let Mem::Pool { charge, .. } = &self.mem {
            let mut c = charge.lock().unwrap_or_else(|e| e.into_inner());
            if c.owner() != owner {
                c.transfer(owner, memledger::charge::release_of(owner));
            }
        }
    }

    /// pppress (B): a pool buffer tagged as the resident cache's (`cached`: `Tag::DICT`, which the present sweep's cache
    /// term counts) or as a build's (`Tag::BUILD`, its take's) from now on, its in-use bytes with it. True where the pool
    /// took it and for the allocator's buffer; false where it keeps its tag (a dead kept buffer; for the pool, a small
    /// block, a FIT_TAIL miss, a dead run).
    pub(super) fn tag_cached(&self, cached: bool) -> bool {
        match &self.mem {
            Mem::Heap(_) => true,
            Mem::Pool { dead: true, .. } => false,
            Mem::Pool { ptr, .. } => {
                let tag = if cached { pagepool::Tag::DICT } else { POOL_HINT.tag };
                // SAFETY: the live pool block this buffer owns; its holder has it (a give or `give_dead` needs it whole).
                unsafe { pagepool::retag(ptr.cast(), tag) }
            }
        }
    }

    /// The whole pool runs inside the buffer, as a block offset and length.
    fn runs(ptr: core::ptr::NonNull<u64>, words: usize) -> (usize, usize) {
        let a = ptr.as_ptr() as usize;
        let (lo, hi) = (a.next_multiple_of(POOL_RUN), (a + words * 8) & !(POOL_RUN - 1));
        (lo - a, hi.saturating_sub(lo))
    }

    /// pagepool P3: the idle release of a kept pool buffer: its whole runs dead (`give_pages`: the purger releases them on
    /// pressure or age), their bytes off its charge. False for the allocator's buffer (`release`'s road).
    pub(super) fn give_dead(&mut self) -> bool {
        let Mem::Pool { ptr, words, charge, dead } = &mut self.mem else { return false };
        if !*dead {
            let (at, len) = Self::runs(*ptr, *words);
            // SAFETY: whole runs of this kept buffer, which nothing reads or writes until `take_dead`.
            unsafe { pagepool::give_pages(ptr.cast(), at, len) };
            charge.get_mut().unwrap_or_else(|e| e.into_inner()).give(len as u64);
            *dead = true;
        }
        true
    }

    /// pagepool P3: a kept pool buffer taken again: its dead runs live (their old words or zero), back on its charge.
    pub(super) fn take_dead(&mut self) {
        if let Mem::Pool { ptr, words, charge, dead } = &mut self.mem {
            if core::mem::replace(dead, false) {
                let (at, len) = Self::runs(*ptr, *words);
                // SAFETY: the runs `give_dead` gave, of this buffer, before any word of them is read or written.
                unsafe { pagepool::take_pages(ptr.cast(), at, len) };
                charge.get_mut().unwrap_or_else(|e| e.into_inner()).commit(len as u64);
            }
        }
    }
}

impl Default for BuildBuf {
    fn default() -> BuildBuf {
        BuildBuf { mem: Mem::Heap(Vec::new()) } // alloc: Node
    }
}

impl From<Vec<u64>> for BuildBuf {
    fn from(v: Vec<u64>) -> BuildBuf {
        BuildBuf { mem: Mem::Heap(v) }
    }
}

impl core::ops::Deref for BuildBuf {
    type Target = [u64];

    fn deref(&self) -> &[u64] {
        match &self.mem {
            Mem::Heap(v) => v,
            // SAFETY: the pool block of `words` words this buffer owns, live until its drop.
            Mem::Pool { ptr, words, .. } => unsafe { core::slice::from_raw_parts(ptr.as_ptr(), *words) },
        }
    }
}

impl core::ops::DerefMut for BuildBuf {
    fn deref_mut(&mut self) -> &mut [u64] {
        match &mut self.mem {
            Mem::Heap(v) => v,
            // SAFETY: as `deref`, borrowed mutably through the buffer.
            Mem::Pool { ptr, words, .. } => unsafe { core::slice::from_raw_parts_mut(ptr.as_ptr(), *words) },
        }
    }
}

impl Drop for BuildBuf {
    fn drop(&mut self) {
        if let Mem::Pool { ptr, words, charge, .. } = &mut self.mem {
            // its charge off its owner first, then the block back (dead runs live again in the pool's give)
            drop(core::mem::replace(charge.get_mut().unwrap_or_else(|e| e.into_inner()), memledger::Charge::NONE));
            // SAFETY: the pool block `pool` took, given back once.
            unsafe { pagepool::give(ptr.cast(), *words * 8) };
        }
    }
}

/// The u64 buffer `words` as bytes (the decode arena).
pub(super) fn bytes_of(words: &mut [u64]) -> &mut [u8] {
    let len = words.len() * 8;
    // SAFETY: an 8-aligned u64 buffer viewed as bytes; the decode alone writes it now.
    unsafe { core::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, len) }
}

/// D3288 (C): the first `end` bytes of the scratch arena `words` (a decoded granule's images, `row_spans`).
pub(super) fn arena_bytes(words: &[u64], end: usize) -> &[u8] {
    let end = end.min(words.len() * 8);
    // SAFETY: an 8-aligned u64 buffer viewed as its first `end` bytes, all inside it.
    unsafe { core::slice::from_raw_parts(words.as_ptr() as *const u8, end) }
}

/// The payload of the 4B-U varlena image at `d`: a live row's datum in the scratch arena.
#[inline(always)]
pub(super) fn payload<'a>(d: u64) -> &'a [u8] {
    let p = d as *const u8;
    // SAFETY: a live row's datum is a 4B-U image in the scratch arena, live until the next decode.
    unsafe {
        let hdr = u32::from_le_bytes([*p, *p.add(1), *p.add(2), *p.add(3)]);
        core::slice::from_raw_parts(p.add(4), (hdr >> 2) as usize - 4)
    }
}

/// A build region over the 8-aligned buffer `buf`, which the caller keeps (unmoved) while the region and what it built
/// are read: the table's own buffer for its lifetime, or a scratch buffer live through the build.
pub(super) fn region(buf: &mut [u64]) -> gravity_dict::Region {
    // SAFETY: an 8-aligned buffer the caller owns for the region's use.
    unsafe { gravity_dict::Region::new(buf.as_mut_ptr() as *mut u8, buf.len() * 8) }
}

/// Every code's value of the `I64` table `t` (a test's read of a built table).
#[cfg(test)]
pub(super) fn i64_values(t: &gravity_dict::I64) -> Vec<i64> {
    // SAFETY: each `c < ncodes`.
    (0..t.ncodes()).map(|c| unsafe { t.get(c) }).collect()
}

/// The bytes of the entry view `v` (a dictionary entry, a derived value, a slice of a resident payload), live through
/// the sweep or the build that handed it out.
#[inline(always)]
pub(super) fn view_bytes<'a>(v: gravity_dict::View) -> &'a [u8] {
    // SAFETY: the entry's bytes are live through the sweep.
    unsafe { v.bytes() }
}

/// The `Fp128` table at `hdr`, the header the build wrote into its table's buffer.
pub(super) fn fp128_at(hdr: *const Header) -> gravity_dict::Fp128 {
    // SAFETY: `hdr` is the `Fp128` header the build wrote into `buf`.
    unsafe { gravity_dict::Fp128::at(hdr) }
}

/// H08: the `Bits` table at `hdr` (a condition table's header the build wrote into its buffer); `None` for a null header
/// or a table of another element.
pub(super) fn bits_at(hdr: *const Header) -> Option<gravity_dict::table::Bits> {
    // SAFETY: a non-null `hdr` is the header the build wrote into its table's buffer; `Bits::at` only after its element says so.
    unsafe { (!hdr.is_null() && (*hdr).elem == gravity_dict::lane::ELEM_BITS).then(|| gravity_dict::table::Bits::at(hdr)) }
}

/// The `Fp128` entries of `t` (its header not null), 16 B per code: `None` for a table of another element or one with
/// faulted entries.
pub(super) fn fp128_entries(t: &FpTable) -> Option<&[u8]> {
    // SAFETY: `hdr` is the header the build wrote into `buf`; an `Fp128` table's `data` holds `ncodes` 16-byte entries.
    unsafe {
        let h = &*t.hdr;
        (h.elem == gravity_dict::lane::ELEM_FP128 && h.faults.ptr().is_null()).then(|| core::slice::from_raw_parts(h.data.ptr(), h.ncodes as usize * 16))
    }
}

/// The header a build just wrote into its region, not yet published, without its fault bitmap.
pub(super) fn no_faults(hdr: *mut Header) -> *const Header {
    // SAFETY: the header this build just wrote into `region`, not yet published.
    unsafe {
        (*hdr).faults = gravity_abi::word(core::ptr::null());
        (*hdr).nfaults = 0;
        (*hdr).flags &= !gravity_dict::table::FLAG_FAULTS;
    }
    hdr as *const Header
}

/// The fresh zeroed header `hdr` inside a region, filled as a program table's: `elem`, `ncodes` codes, its `data` and its
/// fault bitmap `faults` (reserved after it), the region's `base`.
pub(super) fn program_header(hdr: *mut Header, elem: u8, ncodes: u32, data: *const u8, faults: *const u64, base: *const u8) {
    // SAFETY: a fresh zeroed header inside the region; the data and the bitmap were just reserved after it.
    unsafe {
        (*hdr).elem = elem;
        (*hdr).flags = gravity_dict::table::FLAG_FAULTS;
        (*hdr).ncodes = ncodes;
        (*hdr).data = gravity_abi::word(data);
        (*hdr).faults = gravity_abi::word(faults);
        (*hdr).base = gravity_abi::word(base);
    }
}

/// The length table over a part dictionary's resident `index` (`INDEX_ENTRY` bytes an entry): the lane reads each entry's
/// `field` in place.
pub(super) fn index_len_header(region: &mut gravity_dict::Region, index: &[u8], field: gravity_dict::LenField) -> Result<*const Header, gravity_dict::BuildError> {
    // SAFETY: the index is live and immutable for the handle's life, which outlives the part's lanes.
    unsafe { gravity_dict::index_len_header(region, index.as_ptr(), (index.len() / INDEX_ENTRY) as u32, field) }
}

/// The `Derived` table at `hdr`, the header the inner build wrote into its table's buffer.
pub(super) fn derived_at(hdr: *const Header) -> gravity_dict::Derived {
    // SAFETY: the header the inner build wrote into `t`'s buffer, a `Derived` table.
    unsafe { gravity_dict::Derived::at(hdr) }
}

/// Code `c`'s canonical code in `d` (`c < ncodes`; a faulting code's is `u32::MAX`, the fault bit carries over).
pub(super) fn derived_canonical(d: &gravity_dict::Derived, c: u32) -> u32 {
    // SAFETY: `code < ncodes`.
    unsafe { d.canonical(c) }
}

/// Code `c`'s value in `d` (`c < ncodes`): a view into the table's buffer or the part's resident payload.
pub(super) fn derived_view(d: &gravity_dict::Derived, c: u32) -> gravity_dict::View {
    // SAFETY: `code < ncodes`; the views lie in `t`'s buffer or the part's resident payload, live through the build.
    unsafe { d.view(c) }
}

/// The payload of the non-null text result at `p`, live until the call's next call (`ResultLife::UntilNextCall`).
pub(super) fn result_bytes<'a>(p: *const u8) -> &'a [u8] {
    // SAFETY: a non-null text result is a live varlena image until the call's next call: the caller copies it first.
    unsafe { datum::VarlenaRef::from_ptr(p) }.data()
}

/// Physical memory in bytes (`sysconf`); 0 when unknown.
#[cfg(target_os = "linux")]
pub(super) fn phys_bytes() -> u64 {
    extern "C" {
        fn sysconf(name: core::ffi::c_int) -> core::ffi::c_long;
    }
    const SC_PAGESIZE: core::ffi::c_int = 30;
    const SC_PHYS_PAGES: core::ffi::c_int = 85;
    // SAFETY: sysconf reads two process-independent constants.
    let (pages, page) = unsafe { (sysconf(SC_PHYS_PAGES), sysconf(SC_PAGESIZE)) };
    if pages > 0 && page > 0 { pages as u64 * page as u64 } else { 0 }
}

/// A table buffer's pages advised huge and pre-faulted at the build, so no statement's loop takes the faults (pagepool P3:
/// a pool buffer's pages pre-faulted, `prefault`; the pool's zone is advised huge already).
#[cfg(target_os = "linux")]
pub(super) fn advise_resident(buf: &mut [u64]) {
    if pagepool::taken(buf.as_ptr() as *const u8) {
        if let Some(p) = core::ptr::NonNull::new(buf.as_mut_ptr() as *mut u8) {
            // SAFETY: the buffer's own words, written by no other thread during the build.
            unsafe { pagepool::prefault(p, buf.len() * 8) };
        }
        return;
    }
    extern "C" {
        fn madvise(addr: *mut core::ffi::c_void, len: usize, advice: core::ffi::c_int) -> core::ffi::c_int;
    }
    const MADV_HUGEPAGE: core::ffi::c_int = 14;
    const MADV_POPULATE_WRITE: core::ffi::c_int = 23;
    const PAGE: usize = 4096;
    let (start, end) = (buf.as_mut_ptr() as usize, buf.as_mut_ptr() as usize + buf.len() * 8);
    let (lo, hi) = ((start + PAGE - 1) & !(PAGE - 1), end & !(PAGE - 1));
    if hi <= lo {
        return;
    }
    // SAFETY: `[lo, hi)` is page-aligned and inside the buffer; the advice never fails the build (a refusal is ignored).
    unsafe {
        madvise(lo as *mut core::ffi::c_void, hi - lo, MADV_HUGEPAGE);
        madvise(lo as *mut core::ffi::c_void, hi - lo, MADV_POPULATE_WRITE);
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) fn advise_resident(_buf: &mut [u64]) {}

/// The whole pages inside `[ptr, ptr + bytes)` given back to the kernel (`pagegive`, as MADV_DONTNEED without its
/// CVE-2026-74674 call shape): they read zero after, which a kept buffer may (its words are never read before written, as
/// over a fresh zeroed one). The buffer is global-allocator memory advised huge at its build (`advise_resident`).
/// The bytes given back (0 when none).
#[cfg(target_os = "linux")]
pub(super) fn release(ptr: *mut u8, bytes: usize) -> u64 {
    const PAGE: usize = 4096;
    let (lo, hi) = ((ptr as usize).next_multiple_of(PAGE), (ptr as usize + bytes) / PAGE * PAGE);
    if hi > lo {
        // SAFETY: whole pages inside a buffer the pool owns (its lock held); no mapping is freed. Named the page pool's
        // when it owns the range (the pool as the global allocator), else the global allocator's, as before.
        if unsafe { pagepool::give_back_any(lo, hi - lo, true) } == 0 {
            return (hi - lo) as u64;
        }
    }
    0
}

#[cfg(not(target_os = "linux"))]
pub(super) fn release(_ptr: *mut u8, _bytes: usize) -> u64 {
    0
}
