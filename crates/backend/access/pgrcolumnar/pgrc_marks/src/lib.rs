//! One bit per companion heap block: **1 = clean** (armed by a reader before
//! it copies the block), 0 = dirty or never armed. Every companion write calls
//! [`written`] after its heap call returns (the page lock released). A reader
//! refreshing an image calls [`BlockMarks::begin_refresh`], arms each block it
//! re-decodes ([`BlockMarks::arm`]) and only then takes the share lock.
//! `seq` only grows. Its low bit is the overflow flag of the
//! current refresh cycle: [`BlockMarks::begin_refresh`] sets it (`fetch_or`),
//! and the first write that finds its block not clean clears it by adding 1
//! (one bump per cycle, however many such writes follow). A clean→dirty
//! transition adds 2, so it never touches the flag. An image built after
//! `s0 = begin_refresh(..)` is current while `seq() == s0`.
//! Registration: [`Registry::register`] inserts the marks and then bumps the
//! epoch, before the builder reads any page.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use pgsync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering::SeqCst};
use pgsync::{Condvar, Mutex};

/// `(spcOid, dbOid, relNumber)` of a companion heap.
pub type Locator = (u32, u32, u32);

/// The extension window armed past `nblocks` at every refresh (= `PRE_EXTEND_BLOCKS`, `pgrc_front::maint`).
pub const WINDOW: u32 = 1024;

pub const CHUNK_WORDS: usize = 1024;

/// Chunks per `BlockMarks`. A block past `TOP_CHUNKS × chunk bits` can never
/// be armed: its writes take the overflow bump and every refresh re-decodes
/// it (always correct; 256 × 64 Ki blocks = 128 GiB of companion heap).
const TOP_CHUNKS: usize = 256;

const GATE_WAIT: std::time::Duration = std::time::Duration::from_secs(2);


pub struct BlockMarks {
    words_per_chunk: usize,
    /// Each non-null entry is the first word of a leaked `Box<[AtomicU64]>` of
    /// `words_per_chunk` words, installed once by CAS, freed in `Drop`.
    chunks: Box<[AtomicPtr<AtomicU64>]>,
    seq: AtomicU64,
    armed_to: AtomicU32,
    poisoned: AtomicBool,
}

impl BlockMarks {
    pub fn new() -> BlockMarks {
        BlockMarks::with_chunk_words(CHUNK_WORDS)
    }

    pub fn with_chunk_words(words: usize) -> BlockMarks {
        assert!(words > 0);
        BlockMarks {
            words_per_chunk: words,
            chunks: (0..TOP_CHUNKS).map(|_| AtomicPtr::new(std::ptr::null_mut())).collect(),
            seq: AtomicU64::new(0),
            armed_to: AtomicU32::new(0),
            poisoned: AtomicBool::new(false),
        }
    }

    #[inline]
    fn split(&self, block: u32) -> (usize, usize, u64) {
        let b = block as usize;
        let bits = self.words_per_chunk * 64;
        (b / bits, (b % bits) / 64, 1u64 << (b % 64))
    }

    #[inline]
    fn word(&self, block: u32) -> Option<&AtomicU64> {
        let (c, w, _) = self.split(block);
        let p = self.chunks.get(c)?.load(SeqCst);
        if p.is_null() {
            return None;
        }
        // SAFETY: a non-null entry points at `words_per_chunk` words that live
        // until `self` drops (installed once, never replaced).
        Some(unsafe { &*p.add(w) })
    }

    fn word_mut(&self, block: u32) -> Option<&AtomicU64> {
        let (c, w, _) = self.split(block);
        let slot = self.chunks.get(c)?;
        let mut p = slot.load(SeqCst);
        if p.is_null() {
            let fresh: Box<[AtomicU64]> =
                (0..self.words_per_chunk).map(|_| AtomicU64::new(0)).collect();
            let fresh = Box::into_raw(fresh) as *mut AtomicU64;
            match slot.compare_exchange(std::ptr::null_mut(), fresh, SeqCst, SeqCst) {
                Ok(_) => p = fresh,
                Err(won) => {
                    // SAFETY: `fresh` was never published.
                    drop(unsafe {
                        Box::from_raw(std::ptr::slice_from_raw_parts_mut(fresh, self.words_per_chunk))
                    });
                    p = won;
                }
            }
        }
        // SAFETY: as in `word`.
        Some(unsafe { &*p.add(w) })
    }

    pub fn seq(&self) -> u64 {
        self.seq.load(SeqCst)
    }

    pub fn armed_to(&self) -> u32 {
        self.armed_to.load(SeqCst)
    }

    fn overflow_bump(&self) {
        let mut s = self.seq.load(SeqCst);
        while s & 1 == 1 {
            match self.seq.compare_exchange(s, s + 1, SeqCst, SeqCst) {
                Ok(_) => return,
                Err(now) => s = now,
            }
        }
    }

    /// Not clean: the once-per-cycle overflow bump, so this writer never returns while an image that misses its
    /// change still looks current.
    #[inline]
    pub fn written(&self, block: u32) {
        let (_, _, bit) = self.split(block);
        if let Some(word) = self.word(block) {
            if word.load(SeqCst) & bit != 0 && word.fetch_and(!bit, SeqCst) & bit != 0 {
                self.seq.fetch_add(2, SeqCst);
                return;
            }
        }
        self.overflow_bump();
    }

    fn arm_range(&self, from: u32, to: u32) {
        let mut b = from;
        while b < to {
            let (_, _, bit) = self.split(b);
            let in_word = (64 - b % 64).min(to - b);
            let mask = if in_word == 64 { !0u64 } else { ((1u64 << in_word) - 1) << (b % 64) };
            debug_assert_eq!(mask & bit, bit);
            match self.word_mut(b) {
                Some(w) => {
                    w.fetch_or(mask, SeqCst);
                }
                None => return,
            }
            b += in_word;
        }
    }

    /// Reader, before any page read of a refresh. The returned `s0` is the seq the refreshed image carries.
    pub fn begin_refresh(&self, nblocks_hint: u32, window: u32) -> u64 {
        let s0 = self.seq.fetch_or(1, SeqCst) | 1;
        let from = self.armed_to.load(SeqCst);
        let to = nblocks_hint.saturating_add(window);
        if to > from {
            self.arm_range(from, to);
            self.armed_to.fetch_max(to, SeqCst);
        }
        s0
    }

    pub fn arm(&self, block: u32) {
        let (_, _, bit) = self.split(block);
        if let Some(w) = self.word_mut(block) {
            w.fetch_or(bit, SeqCst);
            self.armed_to.fetch_max(block.saturating_add(1), SeqCst);
        }
    }

    pub fn is_clean(&self, block: u32) -> bool {
        let (_, _, bit) = self.split(block);
        self.word(block).is_some_and(|w| w.load(SeqCst) & bit != 0)
    }

    pub fn dirty(&self, upto: u32, out: &mut Vec<u32>) {
        let bits = (self.words_per_chunk * 64) as u32;
        let mut b = 0u32;
        while b < upto {
            let (c, _, _) = self.split(b);
            let p = self.chunks.get(c).map_or(std::ptr::null_mut(), |s| s.load(SeqCst));
            let chunk_end = (c as u64 + 1) * bits as u64;
            let end = (chunk_end.min(upto as u64)) as u32;
            if p.is_null() {
                out.extend(b..end);
                b = end;
                continue;
            }
            while b < end {
                let w = self.word(b).expect("chunk present").load(SeqCst);
                let base = b - b % 64;
                let hi = (base as u64 + 64).min(end as u64) as u32;
                let mut zeros = !w;
                if b > base {
                    zeros &= !((1u64 << (b - base)) - 1);
                }
                while zeros != 0 {
                    let i = zeros.trailing_zeros();
                    let blk = base + i;
                    if blk >= hi {
                        break;
                    }
                    out.push(blk);
                    zeros &= zeros - 1;
                }
                b = hi;
            }
        }
    }

    /// Same-locator truncate: the image is dropped, not refreshed.
    pub fn poison(&self) {
        self.poisoned.store(true, SeqCst);
        self.seq.fetch_add(2, SeqCst);
    }

    pub fn poisoned(&self) -> bool {
        self.poisoned.load(SeqCst)
    }
}

impl Default for BlockMarks {
    fn default() -> Self {
        BlockMarks::new()
    }
}

impl Drop for BlockMarks {
    fn drop(&mut self) {
        for slot in self.chunks.iter() {
            let p = slot.load(SeqCst);
            if !p.is_null() {
                // SAFETY: installed by `word_mut` from a Box of this length;
                // `&mut self` means no reader remains.
                drop(unsafe {
                    Box::from_raw(std::ptr::slice_from_raw_parts_mut(p, self.words_per_chunk))
                });
            }
        }
    }
}


struct GateCell {
    owner: pgsync::thread::ThreadId,
    done: Mutex<bool>,
    cv: Condvar,
}

pub struct Registry {
    epoch: AtomicU64,
    map: Mutex<BTreeMap<Locator, Arc<BlockMarks>>>,
    gates: Mutex<BTreeMap<Locator, Arc<GateCell>>>,
    chunk_words: usize,
}

pub enum GateOutcome<'r> {
    /// This thread builds; publish, then drop the guard (wakes waiters).
    Owner(GateGuard<'r>),
    /// The owner finished: re-check the published image.
    Built,
    /// The bounded wait expired, or this thread already owns the gate: build privately and do not publish.
    TimedOut,
}

pub struct GateGuard<'r> {
    reg: &'r Registry,
    key: Locator,
}

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        let cell = pgsync::lock(&self.reg.gates).remove(&self.key);
        if let Some(cell) = cell {
            *pgsync::lock(&cell.done) = true;
            cell.cv.notify_all();
        }
    }
}

pub struct WriterCache {
    epoch: u64,
    empty: bool,
    ents: Vec<(Locator, Option<Arc<BlockMarks>>)>,
}

const CACHE_ENTS: usize = 16;

impl WriterCache {
    pub const fn new() -> WriterCache {
        WriterCache { epoch: u64::MAX, empty: false, ents: Vec::new() }
    }
}

impl Default for WriterCache {
    fn default() -> Self {
        WriterCache::new()
    }
}

impl Registry {
    pub fn new() -> Registry {
        Registry::with_chunk_words(CHUNK_WORDS)
    }

    pub fn with_chunk_words(words: usize) -> Registry {
        Registry {
            epoch: AtomicU64::new(0),
            map: Mutex::new(BTreeMap::new()),
            gates: Mutex::new(BTreeMap::new()),
            chunk_words: words,
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch.load(SeqCst)
    }

    pub fn register(&self, loc: Locator) -> Arc<BlockMarks> {
        let mut map = pgsync::lock(&self.map);
        if let Some(m) = map.get(&loc) {
            if !m.poisoned() {
                return Arc::clone(m);
            }
        }
        let m = Arc::new(BlockMarks::with_chunk_words(self.chunk_words));
        map.insert(loc, Arc::clone(&m));
        drop(map);
        self.epoch.fetch_add(1, SeqCst);
        m
    }

    pub fn forget(&self, loc: Locator) {
        let gone = pgsync::lock(&self.map).remove(&loc);
        if gone.is_some() {
            self.epoch.fetch_add(1, SeqCst);
        }
    }

    pub fn lookup(&self, loc: Locator) -> Option<Arc<BlockMarks>> {
        pgsync::lock(&self.map).get(&loc).cloned()
    }

    pub fn registered(&self, loc: Locator) -> bool {
        pgsync::lock(&self.map).contains_key(&loc)
    }

    pub fn len(&self) -> usize {
        pgsync::lock(&self.map).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn truncated(&self, loc: Locator) {
        if let Some(m) = self.lookup(loc) {
            m.poison();
        }
    }

    /// One epoch load when nothing changed; a lookup under the map lock once per epoch per locator otherwise.
    #[inline]
    pub fn written(&self, cache: &mut WriterCache, loc: Locator, block: u32) {
        let e = self.epoch.load(SeqCst);
        if e != cache.epoch {
            cache.ents.clear();
            cache.epoch = e;
            cache.empty = pgsync::lock(&self.map).is_empty();
        }
        if cache.empty {
            return;
        }
        let m = match cache.ents.iter().find(|(l, _)| *l == loc) {
            Some((_, m)) => m.clone(),
            None => {
                let m = self.lookup(loc);
                if cache.ents.len() >= CACHE_ENTS {
                    cache.ents.clear();
                }
                cache.ents.push((loc, m.clone()));
                m
            }
        };
        if let Some(m) = m {
            m.written(block);
        }
    }

    /// One refresher per locator at a time (a refresh computes its dirty list
    /// against the published image, so two concurrent refreshes of one marks
    /// could publish a stale base as clean).
    pub fn gate(&self, loc: Locator) -> GateOutcome<'_> {
        let me = pgsync::thread::current().id();
        let key = loc;
        let cell = {
            let mut g = pgsync::lock(&self.gates);
            match g.get(&key) {
                Some(c) if c.owner == me => return GateOutcome::TimedOut,
                Some(c) => Arc::clone(c),
                None => {
                    g.insert(
                        key,
                        Arc::new(GateCell { owner: me, done: Mutex::new(false), cv: Condvar::new() }),
                    );
                    return GateOutcome::Owner(GateGuard { reg: self, key });
                }
            }
        };
        let deadline = std::time::Instant::now() + GATE_WAIT;
        let mut done = pgsync::lock(&cell.done);
        while !*done {
            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }
            done = match cell.cv.wait_timeout(done, deadline - now) {
                Ok((g, _)) => g,
                Err(e) => e.into_inner().0,
            };
        }
        if *done {
            GateOutcome::Built
        } else {
            GateOutcome::TimedOut
        }
    }
}

impl Default for Registry {
    fn default() -> Self {
        Registry::new()
    }
}


pgsync::process_global! {
    static REGISTRY: Registry = Registry {
        epoch: AtomicU64::new(0),
        map: Mutex::new(BTreeMap::new()),
        gates: Mutex::new(BTreeMap::new()),
        chunk_words: CHUNK_WORDS,
    };
}

thread_local! {
    static WRITER: RefCell<WriterCache> = const { RefCell::new(WriterCache::new()) };
}

pub fn registry() -> &'static Registry {
    &REGISTRY
}

pub fn register(loc: Locator) -> Arc<BlockMarks> {
    registry().register(loc)
}

pub fn forget(loc: Locator) {
    registry().forget(loc)
}

#[inline]
pub fn written(loc: Locator, block: u32) {
    WRITER.with(|c| registry().written(&mut c.borrow_mut(), loc, block));
}

pub fn truncated(loc: Locator) {
    registry().truncated(loc)
}

pub fn registered(loc: Locator) -> bool {
    registry().registered(loc)
}

pub fn gate(loc: Locator) -> GateOutcome<'static> {
    registry().gate(loc)
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn transitions_and_seq() {
        let m = BlockMarks::with_chunk_words(2);
        assert_eq!(m.seq(), 0);
        m.written(3);
        assert_eq!(m.seq(), 0);
        let s0 = m.begin_refresh(4, 4);
        assert_eq!(s0, 1);
        assert_eq!(m.armed_to(), 8);
        assert!((0..8).all(|b| m.is_clean(b)));
        assert!(!m.is_clean(8));
        m.written(2);
        assert_eq!(m.seq(), 3);
        assert!(!m.is_clean(2));
        m.written(2);
        assert_eq!(m.seq(), 4);
        m.written(2);
        assert_eq!(m.seq(), 4);
        m.written(20);
        m.written(21);
        assert_eq!(m.seq(), 4);
        let s1 = m.begin_refresh(8, 4);
        assert_eq!(s1, 5);
        m.written(40);
        assert_eq!(m.seq(), 6);
        m.arm(40);
        assert!(m.armed_to() >= 41);
        assert!(m.is_clean(40));
    }

    #[test]
    fn dirty_list() {
        let m = BlockMarks::with_chunk_words(1);
        m.begin_refresh(0, 200);
        for b in [0u32, 5, 63, 64, 65, 127, 128, 150] {
            m.written(b);
        }
        let mut out = Vec::new();
        m.dirty(151, &mut out);
        assert_eq!(out, vec![0, 5, 63, 64, 65, 127, 128, 150]);
        out.clear();
        m.dirty(100, &mut out);
        assert_eq!(out, vec![0, 5, 63, 64, 65]);
        out.clear();
        m.dirty(260, &mut out);
        assert_eq!(&out[..8], &[0, 5, 63, 64, 65, 127, 128, 150]);
        assert_eq!(&out[8..], &(200..260).collect::<Vec<_>>()[..]);
        m.arm(5);
        out.clear();
        m.dirty(10, &mut out);
        assert_eq!(out, vec![0]);
    }

    #[test]
    fn chunk_growth_past_64ki_blocks() {
        let m = BlockMarks::new();
        let far = 3 * 65_536 + 17;
        m.begin_refresh(far, WINDOW);
        assert!(m.is_clean(far));
        assert!(m.is_clean(far + WINDOW - 1));
        assert!(!m.is_clean(far + WINDOW));
        let s = m.seq();
        m.written(far);
        assert_eq!(m.seq(), s + 2);
        let mut out = Vec::new();
        m.dirty(far + 1, &mut out);
        assert_eq!(out, vec![far]);
        let cap = (TOP_CHUNKS * CHUNK_WORDS * 64) as u64;
        if cap < u32::MAX as u64 {
            let b = cap as u32 + 5;
            m.arm(b);
            assert!(!m.is_clean(b));
            let s = m.begin_refresh(0, 0);
            m.written(b);
            assert_eq!(m.seq(), s + 1);
        }
    }

    #[test]
    fn truncated_poisons() {
        let r = Registry::with_chunk_words(1);
        let loc = (1, 2, 3);
        let m = r.register(loc);
        let e = r.epoch();
        let s = m.begin_refresh(10, 0);
        r.truncated(loc);
        assert!(m.poisoned());
        assert_ne!(m.seq(), s);
        let m2 = r.register(loc);
        assert!(!Arc::ptr_eq(&m, &m2));
        assert!(!m2.poisoned());
        assert!(r.epoch() > e);
    }

    #[test]
    fn registry_and_writer_cache() {
        let r = Registry::with_chunk_words(1);
        let mut c = WriterCache::new();
        let loc = (1, 2, 3);
        r.written(&mut c, loc, 0); // empty registry: no-op
        assert!(!r.registered(loc));
        let m = r.register(loc);
        assert!(r.registered(loc));
        assert!(Arc::ptr_eq(&m, &r.register(loc)));
        let s0 = m.begin_refresh(4, 0);
        r.written(&mut c, loc, 1); // stale cache sees the new epoch
        assert_eq!(m.seq(), s0 + 2);
        r.written(&mut c, (9, 9, 9), 1); // other locator: cached None
        r.forget(loc);
        assert!(!r.registered(loc));
        let before = m.seq();
        r.written(&mut c, loc, 2);
        assert_eq!(m.seq(), before, "forgotten marks are not marked");
    }

    #[test]
    fn gate_one_owner() {
        let r = Registry::new();
        let loc = (1, 1, 1);
        let g = r.gate(loc);
        assert!(matches!(g, GateOutcome::Owner(_)));
        assert!(matches!(r.gate(loc), GateOutcome::TimedOut));
        assert!(matches!(r.gate((2, 2, 2)), GateOutcome::Owner(_)));
        std::thread::scope(|s| {
            let h = s.spawn(|| matches!(r.gate(loc), GateOutcome::Built));
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(g);
            assert!(h.join().unwrap());
        });
    }

    #[test]
    fn global_faces() {
        let loc = (77, 77, 77);
        assert!(!registered(loc));
        written(loc, 3);
        let m = register(loc);
        let s0 = m.begin_refresh(8, 0);
        written(loc, 3);
        assert_eq!(m.seq(), s0 + 2);
        truncated(loc);
        assert!(m.poisoned());
        forget(loc);
        assert!(!registered(loc));
    }
}
