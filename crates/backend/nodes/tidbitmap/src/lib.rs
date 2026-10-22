// tidbitmap.c. Pagetable is simplehash (murmurhash32, robin-hood insert,
// backward-shift delete, backward iteration) bucket for bucket, because
// tbm_lossify's page choice depends on bucket order. Bucket arrays are
// allocated through the huge (MCXT_ALLOC_HUGE) entry point. The shared
// iteration lane is thread-native: the frozen arrays + cursor C parks in DSA
// live in an Arc (std containers: cross-thread memory, outside mcx
// accounting like ParallelTableScanDescShared).

extern crate alloc;

use std::sync::{Arc, Mutex};

use allocator_api2::alloc::{AllocError, Allocator, Layout};
use core::ptr::NonNull;
use mcx::{Mcx, PgVec};
use types_core::{BlockNumber, OffsetNumber, BLCKSZ};
use types_error::{PgError, PgResult};
use types_nodes::{bitmapword, BITS_PER_BITMAPWORD};
use types_storage::bufpage::MaxHeapTuplesPerPage;
use types_tuple::itemptr::{
    ItemPointerData, ItemPointerGetBlockNumber, ItemPointerGetOffsetNumber,
};

pub const TBM_MAX_TUPLES_PER_PAGE: usize = MaxHeapTuplesPerPage;
pub const PAGES_PER_CHUNK: usize = BLCKSZ / 32;

const WORDS_PER_PAGE: usize = (TBM_MAX_TUPLES_PER_PAGE - 1) / BITS_PER_BITMAPWORD + 1;
const WORDS_PER_CHUNK: usize = (PAGES_PER_CHUNK - 1) / BITS_PER_BITMAPWORD + 1;
pub const TBM_WORDS: usize = if WORDS_PER_PAGE > WORDS_PER_CHUNK {
    WORDS_PER_PAGE
} else {
    WORDS_PER_CHUNK
};

#[inline(always)]
const fn wordnum(x: usize) -> usize {
    x / BITS_PER_BITMAPWORD
}

#[inline(always)]
const fn bitnum(x: usize) -> usize {
    x % BITS_PER_BITMAPWORD
}

#[derive(Clone, Copy)]
pub struct PagetableEntry {
    pub blockno: BlockNumber,
    pub ischunk: bool,
    pub recheck: bool,
    pub words: [bitmapword; TBM_WORDS],
}

const _: () = assert!(core::mem::size_of::<PagetableEntry>() <= 48);

impl PagetableEntry {
    #[inline]
    fn zeroed(blockno: BlockNumber) -> Self {
        PagetableEntry { blockno, ischunk: false, recheck: false, words: [0; TBM_WORDS] }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TbmStatus {
    Empty,
    OnePage,
    Hash,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TbmIterating {
    Not,
    Private,
    Shared,
}

/// C TBMSharedIteratorState + PTEntryArray/PTIterationArray: arrays frozen
/// at prepare, only the cursor mutates, under the mutex (C: istate->lock).
pub struct TbmSharedIterState {
    npages: i32,
    nchunks: i32,
    ptbase: Vec<PagetableEntry>,
    ptpages: Vec<i32>,
    ptchunks: Vec<i32>,
    cursor: Mutex<TbmSharedCursor>,
}

impl TbmSharedIterState {
    /// bitmap-morsels: the frozen entry counts (exact pages, lossy chunks).
    /// The morsel granule space is SEGMENTED over the frozen arrays:
    /// granules [0, npages) are exact-page entries in `ptpages` (block-number
    /// order), granules [npages, npages+nchunks) are chunk entries in
    /// `ptchunks` (block-number order). See [`TbmRangeIterator`].
    pub fn entry_counts(&self) -> (u64, u64) {
        (self.npages.max(0) as u64, self.nchunks.max(0) as u64)
    }

    /// The heap pages the entries expand to (an exact page one, a lossy chunk its set bits).
    pub fn expanded_pages(&self) -> u64 {
        let lossy: u64 = self.ptchunks.iter().map(|&i| self.ptbase[i as usize].words[..WORDS_PER_CHUNK].iter().map(|w| w.count_ones() as u64).sum::<u64>()).sum();
        self.npages.max(0) as u64 + lossy
    }
}

struct TbmSharedCursor {
    spageptr: i32,
    schunkptr: i32,
    schunkbit: i32,
}

#[derive(Clone, Copy)]
struct McxHuge<'mcx>(Mcx<'mcx>);

// SAFETY (trait contract): delegates to the underlying `Mcx` allocator; a
// block handed out by `allocate` is released to the same context by
// `deallocate` with the layout it was allocated with.
unsafe impl Allocator for McxHuge<'_> {
    #[inline]
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let p = self.0.alloc_uninit_bytes_huge(layout)?;
        Ok(NonNull::slice_from_raw_parts(p, layout.size()))
    }

    #[inline]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: per the trait contract, `ptr`/`layout` came from `allocate`
        // above, which is the same `Mcx` allocation.
        unsafe { self.0.deallocate(ptr, layout) }
    }
}

const SH_MAX_SIZE: u64 = u32::MAX as u64 + 1;
/// `tbm_create_pagetable`'s initial element count.
const PAGETABLE_INITIAL: u32 = 128;
const SH_FILLFACTOR: f64 = 0.9;
const SH_MAX_FILLFACTOR: f64 = 0.98;
const SH_GROW_MAX_DIB: u32 = 25;
const SH_GROW_MAX_MOVE: u32 = 150;
const SH_GROW_MIN_FILLFACTOR: f64 = 0.1;

type Slots<'mcx, T> = allocator_api2::vec::Vec<T, McxHuge<'mcx>>;

#[inline(always)]
fn pagetable_hash(blockno: BlockNumber) -> u32 {
    hashfn::murmurhash32(blockno)
}

#[cold]
fn sh_error(msg: &str) -> alloc::boxed::Box<PgError> {
    PgError::error(alloc::string::String::from(msg)).into()
}

fn sh_compute_size(newsize: u64) -> PgResult<u64> {
    let size = newsize.max(2).next_power_of_two();
    debug_assert!(size <= SH_MAX_SIZE);
    if (core::mem::size_of::<PagetableEntry>() as u64).saturating_mul(size) >= usize::MAX as u64 / 2
    {
        return Err(sh_error("hash table too large"));
    }
    Ok(size)
}

/// C `pagetable_hash`. `data[i]` is initialized iff `used[i]` (C's
/// `status == SH_STATUS_IN_USE`).
struct Pagetable<'mcx> {
    used: Slots<'mcx, bool>,
    data: Slots<'mcx, core::mem::MaybeUninit<PagetableEntry>>,
    size: u64,
    sizemask: u32,
    members: u32,
    grow_threshold: u32,
}

struct PagetableIter {
    cur: u32,
    end: u32,
    done: bool,
}

impl<'mcx> Pagetable<'mcx> {
    fn alloc_slots(
        mcx: Mcx<'mcx>,
        size: u64,
    ) -> PgResult<(Slots<'mcx, bool>, Slots<'mcx, core::mem::MaybeUninit<PagetableEntry>>)> {
        let n = size as usize;
        let oom = || mcx.oom(n.saturating_mul(core::mem::size_of::<PagetableEntry>() + 1));
        let mut used = Slots::new_in(McxHuge(mcx));
        used.try_reserve_exact(n).map_err(|_| oom())?;
        used.resize(n, false);
        let mut data = Slots::new_in(McxHuge(mcx));
        data.try_reserve_exact(n).map_err(|_| oom())?;
        // SAFETY: capacity >= n was reserved above, and MaybeUninit elements
        // need no initialization.
        unsafe { data.set_len(n) };
        Ok((used, data))
    }

    fn create(mcx: Mcx<'mcx>, nelements: u32) -> PgResult<Self> {
        let size = (nelements as f64 / SH_FILLFACTOR).min(SH_MAX_SIZE as f64) as u64;
        let size = sh_compute_size(size)?;
        let (used, data) = Self::alloc_slots(mcx, size)?;
        let mut tb = Pagetable { used, data, size: 0, sizemask: 0, members: 0, grow_threshold: 0 };
        tb.update_parameters(size)?;
        Ok(tb)
    }

    /// Empties the table in place (every bucket unused), keeping its size.
    fn clear(&mut self) {
        self.used.iter_mut().for_each(|u| *u = false);
        self.members = 0;
    }

    fn update_parameters(&mut self, newsize: u64) -> PgResult<()> {
        let size = sh_compute_size(newsize)?;
        self.size = size;
        self.sizemask = (size - 1) as u32;
        let fill = if size == SH_MAX_SIZE { SH_MAX_FILLFACTOR } else { SH_FILLFACTOR };
        self.grow_threshold = (size as f64 * fill) as u32;
        Ok(())
    }

    #[inline(always)]
    fn entry(&self, i: u32) -> &PagetableEntry {
        debug_assert!(self.used[i as usize]);
        // SAFETY: callers pass only in-use buckets, whose data is initialized.
        unsafe { self.data[i as usize].assume_init_ref() }
    }

    #[inline(always)]
    fn entry_mut(&mut self, i: u32) -> &mut PagetableEntry {
        debug_assert!(self.used[i as usize]);
        // SAFETY: callers pass only in-use buckets, whose data is initialized.
        unsafe { self.data[i as usize].assume_init_mut() }
    }

    #[inline(always)]
    fn distance_from_optimal(&self, optimal: u32, bucket: u32) -> u32 {
        if optimal <= bucket {
            bucket - optimal
        } else {
            (self.size + bucket as u64 - optimal as u64) as u32
        }
    }

    #[cold]
    #[inline(never)]
    fn grow(&mut self, mcx: Mcx<'mcx>, newsize: u64) -> PgResult<()> {
        let oldsize = self.size as usize;
        debug_assert!((oldsize as u64) < newsize);
        let newsize = sh_compute_size(newsize)?;
        let (newused, newdata) = Self::alloc_slots(mcx, newsize)?;
        self.update_parameters(newsize)?;
        let oldused = core::mem::replace(&mut self.used, newused);
        let olddata = core::mem::replace(&mut self.data, newdata);

        // The optimal-bucket probe uses the NEW sizemask, as C's does.
        let mut startelem = 0usize;
        for i in 0..oldsize {
            if !oldused[i] {
                startelem = i;
                break;
            }
            // SAFETY: oldused[i] marks an initialized bucket.
            let blockno = unsafe { olddata[i].assume_init_ref() }.blockno;
            if (pagetable_hash(blockno) & self.sizemask) as usize == i {
                startelem = i;
                break;
            }
        }

        let mut copyelem = startelem;
        for _ in 0..oldsize {
            if oldused[copyelem] {
                // SAFETY: oldused[copyelem] marks an initialized bucket.
                let blockno = unsafe { olddata[copyelem].assume_init_ref() }.blockno;
                let mut curelem = pagetable_hash(blockno) & self.sizemask;
                while self.used[curelem as usize] {
                    curelem = curelem.wrapping_add(1) & self.sizemask;
                }
                self.used[curelem as usize] = true;
                self.data[curelem as usize] = olddata[copyelem];
            }
            copyelem += 1;
            if copyelem >= oldsize {
                copyelem = 0;
            }
        }
        Ok(())
    }

    /// C `pagetable_insert`; a new entry comes back zeroed with its key set.
    /// The grow check runs even when the key is present, as C's does.
    fn insert(&mut self, mcx: Mcx<'mcx>, key: BlockNumber) -> PgResult<(u32, bool)> {
        let hash = pagetable_hash(key);
        'restart: loop {
            let mut insertdist = 0u32;
            if self.members >= self.grow_threshold {
                if self.size == SH_MAX_SIZE {
                    return Err(sh_error("hash table size exceeded"));
                }
                self.grow(mcx, self.size * 2)?;
            }
            let startelem = hash & self.sizemask;
            let mut curelem = startelem;
            loop {
                if !self.used[curelem as usize] {
                    self.members += 1;
                    self.used[curelem as usize] = true;
                    self.data[curelem as usize] =
                        core::mem::MaybeUninit::new(PagetableEntry::zeroed(key));
                    return Ok((curelem, false));
                }
                let curblock = self.entry(curelem).blockno;
                if curblock == key {
                    return Ok((curelem, true));
                }
                let curoptimal = pagetable_hash(curblock) & self.sizemask;
                let curdist = self.distance_from_optimal(curoptimal, curelem);

                if insertdist > curdist {
                    let mut emptyelem = curelem;
                    let mut emptydist = 0u32;
                    loop {
                        emptyelem = emptyelem.wrapping_add(1) & self.sizemask;
                        if !self.used[emptyelem as usize] {
                            break;
                        }
                        emptydist += 1;
                        if emptydist > SH_GROW_MAX_MOVE
                            && self.members as f64 / self.size as f64 >= SH_GROW_MIN_FILLFACTOR
                        {
                            self.grow_threshold = 0;
                            continue 'restart;
                        }
                    }
                    let mut lastelem = emptyelem;
                    let mut moveelem = emptyelem;
                    while moveelem != curelem {
                        moveelem = moveelem.wrapping_sub(1) & self.sizemask;
                        self.data[lastelem as usize] = self.data[moveelem as usize];
                        lastelem = moveelem;
                    }
                    self.used[emptyelem as usize] = true;
                    self.members += 1;
                    self.data[curelem as usize] =
                        core::mem::MaybeUninit::new(PagetableEntry::zeroed(key));
                    return Ok((curelem, false));
                }

                curelem = curelem.wrapping_add(1) & self.sizemask;
                insertdist += 1;
                if insertdist > SH_GROW_MAX_DIB
                    && self.members as f64 / self.size as f64 >= SH_GROW_MIN_FILLFACTOR
                {
                    self.grow_threshold = 0;
                    continue 'restart;
                }
            }
        }
    }

    fn lookup(&self, key: BlockNumber) -> Option<&PagetableEntry> {
        let mut curelem = pagetable_hash(key) & self.sizemask;
        loop {
            if !self.used[curelem as usize] {
                return None;
            }
            let entry = self.entry(curelem);
            if entry.blockno == key {
                return Some(entry);
            }
            curelem = curelem.wrapping_add(1) & self.sizemask;
        }
    }

    fn delete(&mut self, key: BlockNumber) -> bool {
        let mut curelem = pagetable_hash(key) & self.sizemask;
        loop {
            if !self.used[curelem as usize] {
                return false;
            }
            if self.entry(curelem).blockno == key {
                let mut lastelem = curelem;
                self.members -= 1;
                loop {
                    curelem = curelem.wrapping_add(1) & self.sizemask;
                    if !self.used[curelem as usize] {
                        break;
                    }
                    let curoptimal = pagetable_hash(self.entry(curelem).blockno) & self.sizemask;
                    if curoptimal == curelem {
                        break;
                    }
                    self.data[lastelem as usize] = self.data[curelem as usize];
                    lastelem = curelem;
                }
                self.used[lastelem as usize] = false;
                return true;
            }
            curelem = curelem.wrapping_add(1) & self.sizemask;
        }
    }

    fn start_iterate(&self) -> PagetableIter {
        let startelem = self.used.iter().position(|&u| !u).expect("pagetable has no empty bucket");
        PagetableIter { cur: startelem as u32, end: startelem as u32, done: false }
    }

    fn start_iterate_at(&self, at: u32) -> PagetableIter {
        let cur = at & self.sizemask;
        PagetableIter { cur, end: cur, done: false }
    }

    /// C `pagetable_iterate`: backwards, over the live sizemask (the table
    /// may grow under a lossify walk).
    fn iterate(&self, iter: &mut PagetableIter) -> Option<u32> {
        while !iter.done {
            let elem = iter.cur;
            iter.cur = iter.cur.wrapping_sub(1) & self.sizemask;
            if (iter.cur & self.sizemask) == (iter.end & self.sizemask) {
                iter.done = true;
            }
            if self.used[elem as usize] {
                return Some(elem);
            }
        }
        None
    }

    fn for_each(&self, mut f: impl FnMut(&PagetableEntry)) {
        let mut it = self.start_iterate();
        while let Some(i) = self.iterate(&mut it) {
            f(self.entry(i));
        }
    }
}

// SAFETY: the slot arrays are arena bytes of the owning context (huge entry
// point of the same `Mcx`), and bool/PagetableEntry have no drop glue;
// forgetting the table at reset leaks nothing outside the arena.
unsafe impl mcx::ForgetSafe for Pagetable<'_> {}

pub struct TIDBitmap<'mcx> {
    mcx: Mcx<'mcx>,
    status: TbmStatus,
    pagetable: Option<Pagetable<'mcx>>,
    nentries: i32,
    maxentries: i32,
    npages: i32,
    nchunks: i32,
    iterating: TbmIterating,
    lossify_start: u32,
    entry1: PagetableEntry,
    // Sorted readout arrays of pagetable bucket indices (C keeps
    // PagetableEntry pointers, tidbitmap.c:713; the bitmap is read-only once
    // iterating, so a bucket index is as stable as C's pointer, and the arrays
    // stay pointer-sized rather than entry-sized).
    spages: Option<PgVec<'mcx, usize>>,
    schunks: Option<PgVec<'mcx, usize>>,
}

pub struct TbmPrivateIterator {
    spageptr: usize,
    schunkptr: usize,
    schunkbit: usize,
}

pub struct TbmIterateResult<'a> {
    pub blockno: BlockNumber,
    pub lossy: bool,
    pub recheck: bool,
    page: Option<&'a PagetableEntry>,
}

impl TbmIterateResult<'_> {
    /// An exact page's tuple bits (bit `off - 1` per offset), copied out of the frozen entry; `None` lossy.
    pub fn tuple_bits(&self) -> Option<[bitmapword; TBM_WORDS]> {
        self.page.filter(|_| !self.lossy).map(|p| p.words)
    }

    /// `tbm_extract_page_tuple`: fills as many offsets as fit, returns the
    /// total number of offsets present on the page.
    pub fn extract_page_tuples(&self, offsets: &mut [OffsetNumber]) -> usize {
        let page = self.page.expect("extract_page_tuples on a lossy page");
        extract_words(&page.words[..WORDS_PER_PAGE], offsets)
    }

    /// Owned copy for a read stream's per-buffer data (C keeps the
    /// TBMIterateResult itself there; its page pointer outlives the call).
    pub fn to_page_result(&self) -> TbmPageResult {
        let mut words = [0; WORDS_PER_PAGE];
        if let Some(page) = self.page {
            words.copy_from_slice(&page.words[..WORDS_PER_PAGE]);
        }
        TbmPageResult { blockno: self.blockno, lossy: self.lossy, recheck: self.recheck, words }
    }
}

#[derive(Clone, Copy, Default)]
pub struct TbmPageResult {
    pub blockno: BlockNumber,
    pub lossy: bool,
    pub recheck: bool,
    words: [bitmapword; WORDS_PER_PAGE],
}

impl TbmPageResult {
    pub fn extract_page_tuples(&self, offsets: &mut [OffsetNumber]) -> usize {
        debug_assert!(!self.lossy);
        extract_words(&self.words, offsets)
    }
}

fn extract_words(words: &[bitmapword], offsets: &mut [OffsetNumber]) -> usize {
    let mut ntuples = 0usize;
    for (wn, &word) in words.iter().enumerate() {
        let mut w = word;
        let mut off = wn * BITS_PER_BITMAPWORD + 1;
        while w != 0 {
            if w & 1 != 0 {
                if ntuples < offsets.len() {
                    offsets[ntuples] = off as OffsetNumber;
                }
                ntuples += 1;
            }
            off += 1;
            w >>= 1;
        }
    }
    ntuples
}

/// C `tbm_calculate_entries` (tidbitmap.c:1545-1561): estimate the number
/// of hashtable entries that fit in maxbytes, counting sizeof(PagetableEntry)
/// plus two Pointers per entry for the iteration readout arrays; clamped to
/// [16, INT_MAX - 1] only. There is no MaxAllocSize ceiling: the pagetable is
/// allocated MCXT_ALLOC_HUGE (see [`McxHuge`]).
pub fn tbm_calculate_entries(maxbytes: usize) -> i32 {
    let nbuckets = maxbytes
        / (core::mem::size_of::<PagetableEntry>() + 2 * core::mem::size_of::<*const u8>());
    let nbuckets = nbuckets.min((i32::MAX - 1) as usize); /* safety limit */
    let nbuckets = nbuckets.max(16); /* sanity limit */
    nbuckets as i32
}

#[cold]
fn offset_out_of_range(off: OffsetNumber) -> alloc::boxed::Box<PgError> {
    PgError::error(alloc::format!("tuple offset out of range: {off}")).into()
}

impl<'mcx> TIDBitmap<'mcx> {
    pub fn new(mcx: Mcx<'mcx>, maxbytes: usize) -> Self {
        TIDBitmap {
            mcx,
            status: TbmStatus::Empty,
            pagetable: None,
            nentries: 0,
            maxentries: tbm_calculate_entries(maxbytes),
            npages: 0,
            nchunks: 0,
            iterating: TbmIterating::Not,
            lossify_start: 0,
            entry1: PagetableEntry::zeroed(0),
            spages: None,
            schunks: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.nentries == 0
    }

    /// Empties the bitmap for reuse by the next build of the same scan node
    /// (`tbm_free` + `tbm_create` with the same `maxbytes`), keeping its
    /// storage: the executor's query context does not reuse freed memory, so
    /// a bitmap built per rescan would otherwise grow the query's memory by
    /// a pagetable per rescan (D2181). A pagetable that grew past its initial
    /// size is dropped, so the reused bitmap's bucket layout — and with it
    /// the iteration and lossify order — is a fresh bitmap's.
    pub fn recycle(&mut self, maxbytes: usize) {
        let keep = self
            .pagetable
            .as_ref()
            .is_some_and(|t| sh_compute_size((PAGETABLE_INITIAL as f64 / SH_FILLFACTOR) as u64).is_ok_and(|s| s == t.size));
        if keep {
            self.pagetable.as_mut().expect("just checked").clear();
        } else {
            self.pagetable = None;
        }
        self.status = TbmStatus::Empty;
        self.nentries = 0;
        // tbm_lossify may have raised it; a new bitmap starts from maxbytes.
        self.maxentries = tbm_calculate_entries(maxbytes);
        self.npages = 0;
        self.nchunks = 0;
        self.iterating = TbmIterating::Not;
        self.lossify_start = 0;
        self.entry1 = PagetableEntry::zeroed(0);
        if let Some(v) = self.spages.as_mut() {
            v.clear();
        }
        if let Some(v) = self.schunks.as_mut() {
            v.clear();
        }
    }

    fn create_pagetable(&mut self) -> PgResult<()> {
        debug_assert!(self.status != TbmStatus::Hash);
        // A recycled bitmap keeps its cleared initial-size pagetable.
        let mut table = match self.pagetable.take() {
            Some(t) => t,
            None => Pagetable::create(self.mcx, PAGETABLE_INITIAL)?,
        };
        debug_assert!(table.members == 0);
        if self.status == TbmStatus::OnePage {
            let (idx, found) = table.insert(self.mcx, self.entry1.blockno)?;
            debug_assert!(!found);
            *table.entry_mut(idx) = self.entry1;
        }
        self.pagetable = Some(table);
        self.status = TbmStatus::Hash;
        Ok(())
    }

    /// `tbm_add_tuples`; tids need not be sorted, but runs of equal block are
    /// the fast path (one pagetable probe per run, as C's currblk cache).
    pub fn add_tuples(&mut self, tids: &[ItemPointerData], recheck: bool) -> PgResult<()> {
        debug_assert!(self.iterating == TbmIterating::Not);
        let mut i = 0usize;
        while i < tids.len() {
            let blk = ItemPointerGetBlockNumber(&tids[i]);
            let mut run_end = i + 1;
            while run_end < tids.len() && ItemPointerGetBlockNumber(&tids[run_end]) == blk {
                run_end += 1;
            }
            for tid in &tids[i..run_end] {
                let off = ItemPointerGetOffsetNumber(tid);
                if off < 1 || off as usize > TBM_MAX_TUPLES_PER_PAGE {
                    return Err(offset_out_of_range(off));
                }
            }
            if !self.page_is_lossy(blk) {
                let mut new_entry = false;
                let page = self.get_pageentry(blk, &mut new_entry)?;
                if page.ischunk {
                    // Chunk-header page: one bit stands for the page itself.
                    page.words[0] |= 1 as bitmapword;
                } else {
                    for tid in &tids[i..run_end] {
                        let off = ItemPointerGetOffsetNumber(tid) as usize;
                        page.words[wordnum(off - 1)] |= (1 as bitmapword) << bitnum(off - 1);
                    }
                }
                page.recheck |= recheck;
                if self.nentries > self.maxentries {
                    self.lossify()?;
                }
            }
            i = run_end;
        }
        Ok(())
    }

    /// `tbm_add_page` — whole page (always rechecked when reported).
    pub fn add_page(&mut self, pageno: BlockNumber) -> PgResult<()> {
        self.mark_page_lossy(pageno)?;
        if self.nentries > self.maxentries {
            self.lossify()?;
        }
        Ok(())
    }

    /// `tbm_union`: self |= b.
    pub fn union(&mut self, b: &TIDBitmap<'mcx>) -> PgResult<()> {
        debug_assert!(self.iterating == TbmIterating::Not);
        if b.nentries == 0 {
            return Ok(());
        }
        if b.status == TbmStatus::OnePage {
            self.union_page(&b.entry1)?;
        } else {
            debug_assert!(b.status == TbmStatus::Hash);
            let btable = b.pagetable.as_ref().expect("TBM_HASH without pagetable");
            let mut it = btable.start_iterate();
            while let Some(i) = btable.iterate(&mut it) {
                self.union_page(btable.entry(i))?;
            }
        }
        Ok(())
    }

    /// bitmap-morsels mode C: `self |= frozen` — union a FROZEN readout (the
    /// cross-thread partial a build worker produced with
    /// `prepare_shared_iterate`; `TIDBitmap` itself is `!Send`, the frozen
    /// state is the sanctioned handoff) into this live bitmap. Per-entry
    /// semantics are exactly `tbm_union`'s — the same `union_page` body, in
    /// arbitrary entry order (C's tbm_union iterates hash order), including
    /// the exact/lossy fold and the maxentries lossify guard.
    pub fn union_frozen(&mut self, f: &TbmSharedIterState) -> PgResult<()> {
        debug_assert!(self.iterating == TbmIterating::Not);
        for e in f.ptbase.iter() {
            self.union_page(e)?;
        }
        Ok(())
    }

    fn union_page(&mut self, bpage: &PagetableEntry) -> PgResult<()> {
        if bpage.ischunk {
            for wn in 0..WORDS_PER_CHUNK {
                let mut w = bpage.words[wn];
                if w != 0 {
                    let mut pg = bpage.blockno + (wn * BITS_PER_BITMAPWORD) as BlockNumber;
                    while w != 0 {
                        if w & 1 != 0 {
                            self.mark_page_lossy(pg)?;
                        }
                        pg += 1;
                        w >>= 1;
                    }
                }
            }
        } else if self.page_is_lossy(bpage.blockno) {
            return Ok(());
        } else {
            let mut new_entry = false;
            let apage = self.get_pageentry(bpage.blockno, &mut new_entry)?;
            if apage.ischunk {
                apage.words[0] |= 1 as bitmapword;
            } else {
                for wn in 0..WORDS_PER_PAGE {
                    apage.words[wn] |= bpage.words[wn];
                }
                apage.recheck |= bpage.recheck;
            }
        }
        if self.nentries > self.maxentries {
            self.lossify()?;
        }
        Ok(())
    }

    /// `tbm_intersect`: self &= b.
    pub fn intersect(&mut self, b: &TIDBitmap<'mcx>) {
        debug_assert!(self.iterating == TbmIterating::Not);
        if self.nentries == 0 {
            return;
        }
        if self.status == TbmStatus::OnePage {
            if intersect_page(&mut self.entry1, b) {
                debug_assert!(!self.entry1.ischunk);
                self.npages -= 1;
                self.nentries -= 1;
                debug_assert!(self.nentries == 0);
                self.status = TbmStatus::Empty;
            }
        } else {
            debug_assert!(self.status == TbmStatus::Hash);
            let table = self.pagetable.as_mut().expect("TBM_HASH without pagetable");
            let (mut npages, mut nchunks, mut nentries) =
                (self.npages, self.nchunks, self.nentries);
            let mut it = table.start_iterate();
            while let Some(i) = table.iterate(&mut it) {
                let apage = table.entry_mut(i);
                if intersect_page(apage, b) {
                    if apage.ischunk {
                        nchunks -= 1;
                    } else {
                        npages -= 1;
                    }
                    nentries -= 1;
                    let blockno = apage.blockno;
                    assert!(table.delete(blockno), "hash table corrupted");
                }
            }
            self.npages = npages;
            self.nchunks = nchunks;
            self.nentries = nentries;
        }
    }

    fn find_pageentry(&self, pageno: BlockNumber) -> Option<&PagetableEntry> {
        if self.nentries == 0 {
            return None;
        }
        if self.status == TbmStatus::OnePage {
            if self.entry1.blockno != pageno {
                return None;
            }
            debug_assert!(!self.entry1.ischunk);
            return Some(&self.entry1);
        }
        let page = self.pagetable.as_ref().expect("TBM_HASH without pagetable").lookup(pageno)?;
        if page.ischunk {
            return None;
        }
        Some(page)
    }

    fn get_pageentry(
        &mut self,
        pageno: BlockNumber,
        new_entry: &mut bool,
    ) -> PgResult<&mut PagetableEntry> {
        if self.status == TbmStatus::Empty {
            self.entry1 = PagetableEntry::zeroed(pageno);
            self.status = TbmStatus::OnePage;
            self.nentries += 1;
            self.npages += 1;
            *new_entry = true;
            return Ok(&mut self.entry1);
        }
        if self.status == TbmStatus::OnePage {
            if self.entry1.blockno == pageno {
                return Ok(&mut self.entry1);
            }
            self.create_pagetable()?;
        }
        let mcx = self.mcx;
        let table = self.pagetable.as_mut().expect("TBM_HASH without pagetable");
        let (idx, found) = table.insert(mcx, pageno)?;
        if !found {
            self.nentries += 1;
            self.npages += 1;
            *new_entry = true;
        }
        Ok(table.entry_mut(idx))
    }

    fn page_is_lossy(&self, pageno: BlockNumber) -> bool {
        if self.nchunks == 0 {
            return false;
        }
        debug_assert!(self.status == TbmStatus::Hash);
        let bitno = pageno as usize % PAGES_PER_CHUNK;
        let chunk_pageno = pageno - bitno as BlockNumber;
        match self.pagetable.as_ref().expect("TBM_HASH without pagetable").lookup(chunk_pageno) {
            Some(page) if page.ischunk => {
                page.words[wordnum(bitno)] & ((1 as bitmapword) << bitnum(bitno)) != 0
            }
            _ => false,
        }
    }

    fn mark_page_lossy(&mut self, pageno: BlockNumber) -> PgResult<()> {
        if self.status != TbmStatus::Hash {
            self.create_pagetable()?;
        }
        let bitno = pageno as usize % PAGES_PER_CHUNK;
        let chunk_pageno = pageno - bitno as BlockNumber;
        let mcx = self.mcx;
        let table = self.pagetable.as_mut().expect("TBM_HASH without pagetable");
        if bitno != 0 && table.delete(pageno) {
            self.nentries -= 1;
            self.npages -= 1;
        }
        let (idx, found) = table.insert(mcx, chunk_pageno)?;
        let page = table.entry_mut(idx);
        if !found {
            page.ischunk = true;
            self.nentries += 1;
            self.nchunks += 1;
        } else if !page.ischunk {
            // Chunk-header page was exact: it had tuple bits, so its own
            // page-bit is set lossy.
            *page = PagetableEntry::zeroed(chunk_pageno);
            page.ischunk = true;
            page.words[0] = 1 as bitmapword;
            self.nchunks += 1;
            self.npages -= 1;
        }
        page.words[wordnum(bitno)] |= (1 as bitmapword) << bitnum(bitno);
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn lossify(&mut self) -> PgResult<()> {
        debug_assert!(self.iterating == TbmIterating::Not);
        debug_assert!(self.status == TbmStatus::Hash);
        // mark_page_lossy inserts/deletes (and may grow the table) under the
        // walk; C's walk has the identical race and tolerates it the same way.
        let mut it = self
            .pagetable
            .as_ref()
            .expect("TBM_HASH without pagetable")
            .start_iterate_at(self.lossify_start);
        loop {
            let table = self.pagetable.as_ref().expect("TBM_HASH without pagetable");
            let Some(i) = table.iterate(&mut it) else { break };
            let page = table.entry(i);
            if page.ischunk || page.blockno as usize % PAGES_PER_CHUNK == 0 {
                continue;
            }
            let blockno = page.blockno;
            self.mark_page_lossy(blockno)?;
            if self.nentries <= self.maxentries / 2 {
                self.lossify_start = it.cur;
                break;
            }
        }
        if self.nentries > self.maxentries / 2 {
            self.maxentries = self.nentries.min((i32::MAX - 1) / 2) * 2;
        }
        Ok(())
    }

    /// bitmap-morsels: entry counts (exact pages, lossy chunks) before any
    /// freeze — the runtime bitmap arm's admission floor input.
    pub fn entry_counts(&self) -> (u64, u64) {
        match self.status {
            TbmStatus::Empty => (0, 0),
            TbmStatus::OnePage => (1, 0),
            TbmStatus::Hash => (self.npages.max(0) as u64, self.nchunks.max(0) as u64),
        }
    }

    /// The entry behind a readout bucket index (spages/schunks).
    #[inline]
    fn readout_entry(&self, idx: usize) -> &PagetableEntry {
        self.pagetable.as_ref().expect("TBM_HASH without pagetable").entry(idx as u32)
    }

    /// `tbm_begin_private_iterate`; the bitmap is read-only afterwards.
    pub fn begin_private_iterate(&mut self) -> PgResult<TbmPrivateIterator> {
        if self.status == TbmStatus::Hash && self.iterating == TbmIterating::Not {
            let table = self.pagetable.as_ref().expect("TBM_HASH without pagetable");
            // A recycled bitmap's emptied readout vectors are reused when
            // they hold enough (no reallocation in the query context).
            let mut spages: PgVec<'mcx, usize> = match self.spages.take() {
                Some(v) if v.is_empty() && v.capacity() >= self.npages as usize => v,
                _ => mcx::vec_with_capacity_in(self.mcx, self.npages as usize)?,
            };
            let mut schunks: PgVec<'mcx, usize> = match self.schunks.take() {
                Some(v) if v.is_empty() && v.capacity() >= self.nchunks as usize => v,
                _ => mcx::vec_with_capacity_in(self.mcx, self.nchunks as usize)?,
            };
            // tidbitmap.c:713-731 pushes PagetableEntry pointers; a bucket
            // index is this table's pointer.
            let mut it = table.start_iterate();
            while let Some(i) = table.iterate(&mut it) {
                if table.entry(i).ischunk {
                    schunks.push(i as usize);
                } else {
                    spages.push(i as usize);
                }
            }
            debug_assert!(spages.len() == self.npages as usize);
            debug_assert!(schunks.len() == self.nchunks as usize);
            spages.sort_unstable_by_key(|&i| table.entry(i as u32).blockno);
            schunks.sort_unstable_by_key(|&i| table.entry(i as u32).blockno);
            self.spages = Some(spages);
            self.schunks = Some(schunks);
        }
        self.iterating = TbmIterating::Private;
        Ok(TbmPrivateIterator { spageptr: 0, schunkptr: 0, schunkbit: 0 })
    }

    /// `tbm_prepare_shared_iterate`: freezes the pagetable into a flat entry
    /// array plus sorted page/chunk index arrays, copied out of the arena
    /// into the workers' Arc (C: dsa). Iteration order matches C exactly.
    pub fn prepare_shared_iterate(&mut self) -> PgResult<Arc<TbmSharedIterState>> {
        debug_assert!(self.iterating != TbmIterating::Private);
        let oom = |n: usize| self.mcx.oom(n * core::mem::size_of::<PagetableEntry>());
        let mut ptbase: Vec<PagetableEntry> = Vec::new();
        ptbase.try_reserve_exact(self.nentries.max(0) as usize).map_err(|_| oom(self.nentries.max(0) as usize))?;
        let mut ptpages: Vec<i32> = Vec::new();
        ptpages.try_reserve_exact(self.npages.max(0) as usize).map_err(|_| oom(1))?;
        let mut ptchunks: Vec<i32> = Vec::new();
        ptchunks.try_reserve_exact(self.nchunks.max(0) as usize).map_err(|_| oom(1))?;
        match self.status {
            TbmStatus::Hash => {
                let table = self.pagetable.as_ref().expect("TBM_HASH without pagetable");
                table.for_each(|page| {
                    let idx = ptbase.len() as i32;
                    ptbase.push(*page);
                    if page.ischunk {
                        ptchunks.push(idx);
                    } else {
                        ptpages.push(idx);
                    }
                });
            }
            TbmStatus::OnePage => {
                ptbase.push(self.entry1);
                ptpages.push(0);
            }
            TbmStatus::Empty => {}
        }
        debug_assert!(ptpages.len() == self.npages as usize);
        debug_assert!(ptchunks.len() == self.nchunks as usize);
        ptpages.sort_unstable_by_key(|&i| ptbase[i as usize].blockno);
        ptchunks.sort_unstable_by_key(|&i| ptbase[i as usize].blockno);
        self.iterating = TbmIterating::Shared;
        Ok(Arc::new(TbmSharedIterState {
            npages: self.npages,
            nchunks: self.nchunks,
            ptbase,
            ptpages,
            ptchunks,
            cursor: Mutex::new(TbmSharedCursor { spageptr: 0, schunkptr: 0, schunkbit: 0 }),
        }))
    }
}

fn intersect_page(apage: &mut PagetableEntry, b: &TIDBitmap<'_>) -> bool {
    if apage.ischunk {
        let mut candelete = true;
        for wn in 0..WORDS_PER_CHUNK {
            let w = apage.words[wn];
            if w != 0 {
                let mut neww = w;
                let mut pg = apage.blockno + (wn * BITS_PER_BITMAPWORD) as BlockNumber;
                let mut bit = 0usize;
                let mut rest = w;
                while rest != 0 {
                    if rest & 1 != 0 && !b.page_is_lossy(pg) && b.find_pageentry(pg).is_none() {
                        neww &= !((1 as bitmapword) << bit);
                    }
                    pg += 1;
                    bit += 1;
                    rest >>= 1;
                }
                apage.words[wn] = neww;
                if neww != 0 {
                    candelete = false;
                }
            }
        }
        candelete
    } else if b.page_is_lossy(apage.blockno) {
        apage.recheck = true;
        false
    } else {
        let mut candelete = true;
        if let Some(bpage) = b.find_pageentry(apage.blockno) {
            debug_assert!(!bpage.ischunk);
            for wn in 0..WORDS_PER_PAGE {
                apage.words[wn] &= bpage.words[wn];
                if apage.words[wn] != 0 {
                    candelete = false;
                }
            }
            apage.recheck |= bpage.recheck;
        }
        candelete
    }
}

#[inline]
fn advance_schunkbit(chunk: &PagetableEntry, schunkbit: &mut usize) {
    let mut bit = *schunkbit;
    while bit < PAGES_PER_CHUNK {
        if chunk.words[wordnum(bit)] & ((1 as bitmapword) << bitnum(bit)) != 0 {
            break;
        }
        bit += 1;
    }
    *schunkbit = bit;
}

impl TbmPrivateIterator {
    /// `tbm_private_iterate`: pages come out in block-number order;
    /// `None` = exhausted.
    pub fn next<'a>(&mut self, tbm: &'a TIDBitmap<'_>) -> Option<TbmIterateResult<'a>> {
        debug_assert!(tbm.iterating == TbmIterating::Private);
        let nchunks = tbm.nchunks as usize;
        let npages = tbm.npages as usize;
        let schunks: &[usize] = tbm.schunks.as_deref().unwrap_or(&[]);
        while self.schunkptr < nchunks {
            let chunk = tbm.readout_entry(schunks[self.schunkptr]);
            let mut schunkbit = self.schunkbit;
            advance_schunkbit(chunk, &mut schunkbit);
            if schunkbit < PAGES_PER_CHUNK {
                self.schunkbit = schunkbit;
                break;
            }
            self.schunkptr += 1;
            self.schunkbit = 0;
        }
        // Emit the numerically earlier of the next lossy page and the next
        // exact page.
        if self.schunkptr < nchunks {
            let chunk = tbm.readout_entry(schunks[self.schunkptr]);
            let chunk_blockno = chunk.blockno + self.schunkbit as BlockNumber;
            let next_exact_blockno = if tbm.status == TbmStatus::OnePage {
                Some(tbm.entry1.blockno)
            } else {
                tbm.spages
                    .as_deref()
                    .and_then(|s| s.get(self.spageptr))
                    .map(|&i| tbm.readout_entry(i).blockno)
            };
            if self.spageptr >= npages || next_exact_blockno.is_none_or(|b| chunk_blockno < b) {
                self.schunkbit += 1;
                return Some(TbmIterateResult {
                    blockno: chunk_blockno,
                    lossy: true,
                    recheck: true,
                    page: None,
                });
            }
        }
        if self.spageptr < npages {
            let page = if tbm.status == TbmStatus::OnePage {
                &tbm.entry1
            } else {
                tbm.readout_entry(tbm.spages.as_deref().expect("iterating without spages")[self.spageptr])
            };
            self.spageptr += 1;
            return Some(TbmIterateResult {
                blockno: page.blockno,
                lossy: false,
                recheck: page.recheck,
                page: Some(page),
            });
        }
        None
    }
}

/// C TBMSharedIterator: attach handle pinning the Arc (no bitmap needed).
pub struct TbmSharedIterator {
    state: Arc<TbmSharedIterState>,
}

impl TbmSharedIterator {
    /// `tbm_attach_shared_iterate`; C takes (dsa, dsa_pointer).
    pub fn attach(state: Arc<TbmSharedIterState>) -> Self {
        TbmSharedIterator { state }
    }

    /// `tbm_shared_iterate`; cursor advance under the mutex (C: istate->lock).
    pub fn next(&mut self) -> Option<TbmIterateResult<'_>> {
        let st = &*self.state;
        let ptbase: &[PagetableEntry] = &st.ptbase;
        let idxpages: &[i32] = &st.ptpages;
        let idxchunks: &[i32] = &st.ptchunks;
        let mut cur = st.cursor.lock().unwrap_or_else(|e| e.into_inner());
        while cur.schunkptr < st.nchunks {
            let chunk = &ptbase[idxchunks[cur.schunkptr as usize] as usize];
            let mut schunkbit = cur.schunkbit as usize;
            advance_schunkbit(chunk, &mut schunkbit);
            if schunkbit < PAGES_PER_CHUNK {
                cur.schunkbit = schunkbit as i32;
                break;
            }
            cur.schunkptr += 1;
            cur.schunkbit = 0;
        }
        if cur.schunkptr < st.nchunks {
            let chunk = &ptbase[idxchunks[cur.schunkptr as usize] as usize];
            let chunk_blockno = chunk.blockno + cur.schunkbit as BlockNumber;
            if cur.spageptr >= st.npages
                || chunk_blockno < ptbase[idxpages[cur.spageptr as usize] as usize].blockno
            {
                cur.schunkbit += 1;
                return Some(TbmIterateResult {
                    blockno: chunk_blockno,
                    lossy: true,
                    recheck: true,
                    page: None,
                });
            }
        }
        if cur.spageptr < st.npages {
            let idx = idxpages[cur.spageptr as usize] as usize;
            cur.spageptr += 1;
            drop(cur);
            let page = &ptbase[idx];
            return Some(TbmIterateResult {
                blockno: page.blockno,
                lossy: false,
                recheck: page.recheck,
                page: Some(page),
            });
        }
        None
    }
}

/// bitmap-morsels RANGE iterator: iterates one CLAIMED WINDOW of the frozen
/// shared arrays with NO shared cursor — each morsel worker owns a disjoint
/// entry-index window, so no lock and no cross-worker handoff. The granule
/// space is SEGMENTED (not C's block-order page/chunk merge): entries
/// [0, npages) are the exact pages in `ptpages` order (ascending blockno),
/// entries [npages, npages+nchunks) are the lossy chunks in `ptchunks` order.
/// Each chunk entry expands to its set bits exactly like the shared/private
/// iterators (advance_schunkbit), yielding lossy per-page results with
/// recheck=true. Cross-window row ORDER therefore differs from the serial
/// merged order — sound only for order-insensitive consumers (the runtime
/// bitmap arm admits order-insensitive-exact agg partials only); per-page
/// contents, recheck flags, and visibility semantics are identical.
pub struct TbmRangeIterator {
    state: Arc<TbmSharedIterState>,
    /// Next entry index in the segmented space, in [pos, end).
    pos: u64,
    end: u64,
    /// Bit cursor within the current chunk entry (pos >= npages only).
    schunkbit: usize,
}

impl TbmRangeIterator {
    /// Iterate segmented entries [start, end); the caller guarantees
    /// end <= npages + nchunks (the morsel source's granule count).
    pub fn new(state: Arc<TbmSharedIterState>, start: u64, end: u64) -> Self {
        debug_assert!(
            end <= (state.npages.max(0) as u64 + state.nchunks.max(0) as u64)
                && start <= end
        );
        TbmRangeIterator { state, pos: start, end, schunkbit: 0 }
    }

    pub fn next(&mut self) -> Option<TbmIterateResult<'_>> {
        let st = &*self.state;
        let npages = st.npages.max(0) as u64;
        loop {
            if self.pos >= self.end {
                return None;
            }
            if self.pos < npages {
                let idx = st.ptpages[self.pos as usize] as usize;
                self.pos += 1;
                let page = &st.ptbase[idx];
                return Some(TbmIterateResult {
                    blockno: page.blockno,
                    lossy: false,
                    recheck: page.recheck,
                    page: Some(page),
                });
            }
            let ci = (self.pos - npages) as usize;
            let chunk = &st.ptbase[st.ptchunks[ci] as usize];
            let mut bit = self.schunkbit;
            advance_schunkbit(chunk, &mut bit);
            if bit >= PAGES_PER_CHUNK {
                self.pos += 1;
                self.schunkbit = 0;
                continue;
            }
            self.schunkbit = bit + 1;
            return Some(TbmIterateResult {
                blockno: chunk.blockno + bit as BlockNumber,
                lossy: true,
                recheck: true,
                page: None,
            });
        }
    }
}

/// Unified iterator (`TBMIterator`).
pub struct TbmIterator {
    private: Option<TbmPrivateIterator>,
    shared: Option<TbmSharedIterator>,
    range: Option<TbmRangeIterator>,
}

impl TbmIterator {
    pub fn empty() -> Self {
        TbmIterator { private: None, shared: None, range: None }
    }

    pub fn private(iter: TbmPrivateIterator) -> Self {
        TbmIterator { private: Some(iter), shared: None, range: None }
    }

    pub fn shared(iter: TbmSharedIterator) -> Self {
        TbmIterator { private: None, shared: Some(iter), range: None }
    }

    /// bitmap-morsels: a claimed-window iterator (self-contained, lock-free).
    pub fn range(iter: TbmRangeIterator) -> Self {
        TbmIterator { private: None, shared: None, range: Some(iter) }
    }

    pub fn exhausted(&self) -> bool {
        self.private.is_none() && self.shared.is_none() && self.range.is_none()
    }

    pub fn end_iterate(&mut self) {
        self.private = None;
        self.shared = None;
        self.range = None;
    }

    /// `tbm_iterate`; the private lane needs the owning bitmap, the shared
    /// and range lanes are self-contained.
    pub fn next<'a>(&'a mut self, tbm: Option<&'a TIDBitmap<'_>>) -> Option<TbmIterateResult<'a>> {
        if let Some(p) = self.private.as_mut() {
            return p.next(tbm.expect("private tbm_iterate without the owning bitmap"));
        }
        if let Some(r) = self.range.as_mut() {
            return r.next();
        }
        self.shared.as_mut().expect("tbm_iterate on an exhausted TBMIterator").next()
    }
}

mcx::forget_safe_nodrop!(TbmStatus, TbmIterating, PagetableEntry, TbmPrivateIterator);

// Exempt (droppy Arc handles): released by TbmIterator::end_iterate /
// ExecEndBitmapHeapScan before the owning arena resets.
unsafe impl mcx::ForgetSafe for TbmSharedIterator {}
unsafe impl mcx::ForgetSafe for TbmRangeIterator {}
unsafe impl mcx::ForgetSafe for TbmIterator {}

mcx::forget_safe_struct!(
    TIDBitmap<'_> { mcx, status, pagetable, nentries, maxentries, npages,
        nchunks, iterating, lossify_start, entry1, spages, schunks },
);

#[cfg(test)]
mod tests;
