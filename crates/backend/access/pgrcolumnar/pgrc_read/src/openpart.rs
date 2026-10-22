//! Part open + CRC-validated section faulting (spec §5–§6.2, §11).
//!
//! The open path faults exactly four fixed regions — tail, footer, section
//! table, header — then everything else is lazy at section grain through the
//! per-part segment cache. Every fault is recorded in the part's fault log:
//! the O(streams-touched) born-RED gate reads that log as its witness
//! (spec §6.2; the 2.9 ms/MiB cold-open law is the reason).
//!
//! A part is READABLE iff its tail magic + footer CRC validate AND it is
//! listed in an effective manifest generation (spec §5). This module owns
//! the first half; the manifest gate composes in [`crate::registry`] +
//! [`crate::manifest_walk`] via [`PartExpect`] validation facts.

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

use pgrc_format::ident::{part_uuid, PartIdent};
use pgrc_format::manifest::PartRecord;
use pgrc_format::meta::{StatsRecord, STATS_RECORD_LEN};
use pgrc_format::part::{
    ExtentRecord, FooterFixed, PartHeader, SectionEntry, SectionKind, StreamEntry,
    FOOTER_FIXED_LEN, PART_HEADER_LEN, PART_TAIL_LEN, SECTION_ENTRY_LEN, TAIL_MAGIC,
};
use pgrc_format::wire::{crc32c, Cur};
use pgrc_format::FormatError;
use pgsync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use pgsync::Mutex;

use crate::cursor::Residency;
use crate::io::PartIo;
use crate::streams::StreamDirectory;
use crate::{ReadError, ReadResult};

pgsync::process_global! {
    /// Process-global dict-epoch clock (crate doc "Dict epochs"): u64 epochs
    /// are minted once per (open-part instance, dict stream), so equality
    /// certifies code-space identity structurally. Starts at 1 (0 is never a
    /// minted epoch).
    static EPOCH_CLOCK: pgsync::atomic::AtomicU64 = pgsync::atomic::AtomicU64::new(1);
}

// ---------------------------------------------------------------------------
// stream-fault observer (claim-horizon lane)
// ---------------------------------------------------------------------------

/// One demand access to a stream extent through the cursor faces
/// ([`OpenPart::extent_bytes`] / [`OpenPart::unwrapped_extent_bytes`]),
/// resident or not. `part` is an opaque identity key (`&OpenPart as usize`)
/// — observers correlate it against the `Arc<OpenPart>`s they already hold.
#[derive(Debug, Clone, Copy)]
pub struct StreamFaultEvent {
    pub part: usize,
    pub attno: u32,
    pub path_ord: u32,
    pub role: u8,
    pub extent_idx: u32,
    pub granule_start: u32,
    pub granule_count: u32,
    pub file_off: u64,
    pub len: u64,
    pub resident: bool,
    pub wrapped: bool,
}

/// Observer contract: called on the FAULTING thread, inside the measured
/// window, for every stream-extent access (hit or miss) while enabled —
/// it must be cheap and MUST NOT call back into the faulting faces
/// synchronously (hand work to another thread). Observation is a witness
/// face: it never changes what a read returns.
pub type StreamFaultObserver = dyn Fn(&StreamFaultEvent) + Send + Sync;

pgsync::process_global! {
    static STREAM_FAULT_OBS_ENABLED: pgsync::atomic::AtomicU32 =
        pgsync::atomic::AtomicU32::new(0);
    static STREAM_FAULT_OBS: pgsync::OnceLock<Box<StreamFaultObserver>> =
        pgsync::OnceLock::new();
}

/// Install the process-wide stream-fault observer (once; later calls are
/// refused with `false`). Enable/disable with
/// [`set_stream_fault_observer_enabled`] — installed-but-disabled costs one
/// relaxed atomic load per access.
pub fn set_stream_fault_observer(cb: Box<StreamFaultObserver>) -> bool {
    STREAM_FAULT_OBS.set(cb).is_ok()
}

pub fn set_stream_fault_observer_enabled(on: bool) {
    STREAM_FAULT_OBS_ENABLED.store(on as u32, Ordering::Relaxed);
}

#[inline]
fn observe_stream_fault(ev: &StreamFaultEvent) {
    if STREAM_FAULT_OBS_ENABLED.load(Ordering::Relaxed) != 0 {
        if let Some(cb) = STREAM_FAULT_OBS.get() {
            cb(ev);
        }
    }
}

// ---------------------------------------------------------------------------
// SegBuf — 8-aligned immutable section bytes
// ---------------------------------------------------------------------------

/// One section image. The bytes live in an 8-aligned `u64` buffer, so the byte base is
/// ALWAYS 8-aligned: sections start 8-aligned in the file (spec §1), so
/// section-relative entry alignment (varlena slots, dict payload entries)
/// becomes absolute pointer alignment — the ≥8-align law and the StrView §7b
/// zero-copy dependency. Clones share the buffer; bytes never move. The buffer
/// is a [`crate::mem`] chunk: charged as held at build, credited in its class on drop.
#[derive(Clone)]
pub struct SegBuf {
    chunk: Arc<Chunk>,
    ptr: *const u8,
    len: usize,
}

// SAFETY: `ptr` points into `chunk._img`, which `chunk` keeps alive and nothing writes once the SegBuf exists.
unsafe impl Send for SegBuf {}
unsafe impl Sync for SegBuf {}

/// The buffer behind a [`SegBuf`] and its memory class (`crate::mem`), with trunk's CACHE-CHARGE slot (its `Img`).
struct Chunk {
    /// CACHE-CHARGE (trunk's `Img::taken`): until the part cache's own charge takes them over ([`OpenPart::charge_seg`]),
    /// the charge its bytes took on the part cache's holder as they were allocated (a held build: `in_cache_held`), or a
    /// reader's charge it carries ([`SegBuf::carry`]). Declared first, so it drops first (after `Chunk::drop`'s census
    /// credit): an image never cached gives its bytes back to the thread its free lands on.
    taken: Mutex<Option<memledger::Charge>>,
    /// Owned here (trunk D4071: built or adopted as it is), read through `SegBuf::ptr` (no second hop on `bytes`).
    _img: Image,
    len: usize,
    class: core::sync::atomic::AtomicU8,
    refbit: core::sync::atomic::AtomicBool,
    /// D3113: a part map let go of this image while another holder kept it ([`SegBuf::owe_if_shared`]): the part cache's
    /// census (and its account, the memory governor's) still counts it, and its last holder's drop releases it.
    owed: core::sync::atomic::AtomicBool,
}

impl Drop for Chunk {
    fn drop(&mut self) {
        crate::mem::credit(self.class.load(Ordering::Relaxed), self.len);
        if self.owed.load(Ordering::Acquire) {
            crate::partcache::repay(self.len as u64);
        }
    }
}

/// D4487 t3: an image's `Arc<Chunk>` block (trunk's `Arc<Img>`; the two counts beside it), allocated with the image on
/// the thread that builds it: a reader's image carries it in its charge ([`SegBuf::carry`]).
pub(crate) const IMG_BLOCK: u64 = (core::mem::size_of::<Chunk>() + 2 * core::mem::size_of::<usize>()) as u64;

/// D4487 t2: what a cached image holds past its bytes, on the part cache's charge with them: its `Arc<Chunk>` block and its
/// map entry (key and `SegBuf`).
pub(crate) const IMAGE_FIXED: u64 = IMG_BLOCK + (core::mem::size_of::<(u64, u64)>() + core::mem::size_of::<SegBuf>()) as u64;

/// D4487 t2: a passed extent's map entry (`PartState::passed`), on the part cache's charge while it is listed.
pub(crate) const PASSED_FIXED: u64 = (core::mem::size_of::<(u64, u64)>() + core::mem::size_of::<Passed>()) as u64;

/// D4321 step 1 (LADDER-DESIGN section 6, user-approved into class 2): an image is made with this thread's allocations
/// on the cache arena's heap (a cache scope; the global allocator's road while the heap is not installed): a
/// `SegBuf::build`'s buffer and an unwrap's rebuilt image (adopted as the image, `SegBuf::adopt`), so an evicted image
/// frees pages holding cache images only and the ladder's purge returns them whole. An unwrap's transient buffers come
/// from the same heap and go back at once. Every such scope names its owner (`memledger::arena::cache_scope_for`): the
/// part cache's holder for an image the cache keeps (`in_cache_held`), else the reader's owner or the thread's context
/// (`charged_for`).
///
/// CACHE-CHARGE: the scope for an image the part cache keeps: its cache-road blocks charged to the part cache's holder
/// as they are taken, the charge returned for the image to carry ([`SegBuf::adopt_held`], [`SegBuf::build_held`]) until
/// the cache's charge takes it over.
#[inline]
fn in_cache_held<T>(f: impl FnOnce() -> T) -> (T, Option<memledger::Charge>) {
    let scope = memledger::arena::cache_scope_for(crate::partcache::OWNER, memledger::Kind::Region);
    let r = f();
    (r, scope.finish())
}

/// The owner a reader's own buffers are charged to and its kind lane: the breaker its scan feeds, else its statement
/// (`StreamCursor::read_owner`); `None`: uncharged, on the allocating thread's account.
pub type ReadOwner = Option<(memledger::Owner, memledger::Kind)>;

pub(crate) fn release_of(o: memledger::Owner) -> memledger::Release {
    match o {
        memledger::Owner::Portal { .. } => memledger::Release::Portal,
        memledger::Owner::Session(_) => memledger::Release::Session,
        memledger::Owner::Cache(_) => memledger::Release::Evict,
        memledger::Owner::Statement { .. } | memledger::Owner::WorkerSet(_) | memledger::Owner::Op { .. } => memledger::Release::Statement,
    }
}

/// `n` bytes of a block this thread just allocated, onto the reader's owner until the charge drops (before the block).
pub(crate) fn read_charge(owner: ReadOwner, n: u64) -> memledger::Charge {
    match owner {
        Some((o, k)) => memledger::Charge::open(o, release_of(o), k, memledger::charge::Door::Heap, n),
        None => memledger::Charge::NONE,
    }
}

/// `f` in a cache scope for an image a reader keeps and no cache does (a build, an unwrap's rebuild): its cache-road
/// blocks charged as they are taken to `owner`, else to the thread's context ([`context_owner`]: no cache scope is
/// plain), the charge returned for the image to carry ([`SegBuf::carry`]).
pub(crate) fn charged_for<T>(owner: ReadOwner, f: impl FnOnce() -> T) -> (T, Option<memledger::Charge>) {
    let (o, k) = owner.unwrap_or_else(|| (memledger::charge::context().0, memledger::Kind::Region));
    let scope = memledger::arena::cache_scope_for(o, k);
    let r = f();
    (r, scope.finish())
}

pub(crate) fn read_vec<T>(owner: ReadOwner) -> memledger::ChargedVec<T> {
    memledger::ChargedVec::new(owner.map(|o| o.0), owner.map_or(memledger::Kind::Region, |o| o.1))
}

/// The owner of a read no cursor names: the calling thread's context (`memledger::charge::context`: the portal its
/// backend executes, else its running statement, else its session; a holder off a backend), on `Kind::Region`.
pub(crate) fn context_owner() -> ReadOwner {
    Some((memledger::charge::context().0, memledger::Kind::Region))
}

/// `charged_for`'s charge raised to `cap` bytes: a block the general road served (no cache heap installed, or a full
/// one) took no cache-road charge in the scope, so its bytes, counted on this thread, are committed now. Never lowered:
/// a cache-road block's own size stays charged.
fn at_least(taken: Option<memledger::Charge>, cap: u64) -> Option<memledger::Charge> {
    taken.map(|mut c| {
        if c.committed() < cap {
            c.commit(cap - c.committed());
        }
        c
    })
}

/// A [`SegBuf`]'s storage, shared as it was built (D4071: an `Arc<[u64]>` from the built `Vec` copied the whole image
/// once more, outside any poll).
enum Image {
    /// Built here: zeroed, then filled.
    Words(Vec<u64>),
    /// A rebuilt image adopted as it is ([`SegBuf::adopt`]): its base was 8-aligned.
    Bytes(Vec<u8>),
}

impl Image {
    fn base(&self) -> *const u8 {
        match self {
            Image::Words(w) => w.as_ptr() as *const u8,
            Image::Bytes(b) => b.as_ptr(),
        }
    }

    fn cap_bytes(&self) -> u64 {
        match self {
            Image::Words(w) => (w.capacity() * 8) as u64,
            Image::Bytes(b) => b.capacity() as u64,
        }
    }
}

impl core::fmt::Debug for SegBuf {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SegBuf").field("len", &self.len).finish()
    }
}

impl SegBuf {
    /// Allocate `len` zeroed bytes in an 8-aligned buffer and let `fill`
    /// populate them before the buffer is shared. `polled`: the zero fill
    /// goes a `cancelpoll::PIECE` at a time, the statement's cancel asked
    /// before each piece past the first (D4071: the fill faults in every page
    /// of the image, hundreds of ms for a large one under memory pressure).
    /// The buffer is allocated in the cache scope the caller opened for its owner ([`Self::build_for`]).
    fn build(
        len: usize,
        polled: bool,
        fill: impl FnOnce(&mut [u8]) -> ReadResult<()>,
    ) -> ReadResult<SegBuf> {
        SegBuf::build_in(len, polled, false, fill)
    }

    /// CACHE-CHARGE: `build` for an image the part cache keeps: its block charged to the part cache's holder from its
    /// allocation (`in_cache_held`), carried until [`OpenPart::charge_seg`].
    fn build_held(
        len: usize,
        polled: bool,
        fill: impl FnOnce(&mut [u8]) -> ReadResult<()>,
    ) -> ReadResult<SegBuf> {
        SegBuf::build_in(len, polled, true, fill)
    }

    fn build_in(
        len: usize,
        polled: bool,
        held: bool,
        fill: impl FnOnce(&mut [u8]) -> ReadResult<()>,
    ) -> ReadResult<SegBuf> {
        // `len` is an on-file extent length (up to u32::MAX) or an unwrapped
        // extent's image, which the data sizes: a granule of 140 KB values
        // unwraps past 1 GiB (D2582: C's MaxAllocSize as a cap here refused it
        // where PostgreSQL returns the rows). Reserved fallibly, so a hostile
        // part or a full machine is a typed read error, never an allocator
        // abort.
        let nwords = len.div_ceil(8);
        let mut step = polled.then(|| cancelpoll::step("pgrc_read SegBuf::build: an image zeroed"));
        let mut words: Vec<u64> = Vec::new();
        // D4321 step 1 (LADDER-DESIGN 6): the image's one allocation on the cache arena: in the part cache's scope
        // (`in_cache_held`), else in the scope its caller opened for its owner (`charged_for`: `build_for`, `copied_for`)
        // one reservation, run in whichever scope charges it
        let mut reserve = || words.try_reserve_exact(nwords);
        let (reserved, taken) = if held { in_cache_held(&mut reserve) } else { (reserve(), None) };
        reserved.map_err(|_| ReadError::Io { at: "extent image allocation", errno: libc::ENOMEM })?;
        while words.len() < nwords {
            words.resize(nwords.min(words.len() + cancelpoll::PIECE / 8), 0);
            match step.as_mut() {
                Some(step) if words.len() < nwords => step.tick()?,
                _ => {}
            }
        }
        drop(step);
        // SAFETY: u64 → u8 reinterpret of an exclusively owned buffer;
        // alignment only loosens and `len <= words.len() * 8`.
        let bytes =
            unsafe { core::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, len) };
        fill(bytes)?;
        Ok(SegBuf::chunked(Image::Words(words), len, taken))
    }

    /// A copy of a byte image (a run's raw extents), its fill unpolled, charged to the thread's context from its
    /// allocation ([`Self::from_bytes_for`]). An allocation the machine refuses is `ReadError::Io` ENOMEM (D4322: an
    /// `expect` here aborted the server).
    pub fn from_bytes(v: &[u8]) -> ReadResult<SegBuf> {
        SegBuf::from_bytes_for(None, v)
    }

    /// CACHE-CHARGE: `from_bytes` for an image the part cache keeps (`build_held`).
    fn from_bytes_held(v: &[u8]) -> ReadResult<SegBuf> {
        SegBuf::build_held(v.len(), false, |b| {
            b.copy_from_slice(v);
            Ok(())
        })
    }

    /// `from_bytes` with the allocation's failure a typed read error (the
    /// unwrapped images, which the data sizes), and the zero fill and the copy
    /// polled a `cancelpoll::PIECE` at a time ([`Self::copied_for`] the thread's context).
    pub fn try_from_bytes(v: &[u8]) -> ReadResult<SegBuf> {
        SegBuf::copied_for(None, v)
    }

    /// [`Self::try_from_bytes`] charged to `owner` (else the thread's context) from its allocation ([`Self::build_for`]).
    pub(crate) fn copied_for(owner: ReadOwner, v: &[u8]) -> ReadResult<SegBuf> {
        SegBuf::build_for(owner, v.len(), true, |b| {
            let mut step = cancelpoll::step("pgrc_read SegBuf::try_from_bytes: an image copied");
            Ok(step.pieces(v.len(), cancelpoll::PIECE, |r| {
                b[r.start..r.end].copy_from_slice(&v[r]);
                Ok::<(), cancelpoll::Canceled>(())
            })?)
        })
    }

    /// D4071 / D4183: a rebuilt image (an unwrap's) as the resident image itself, neither zeroed nor copied again: the
    /// build and the copy into the shared buffer that followed a rebuild's last poll were two more passes over the whole
    /// image. An image whose base is not 8-aligned (the allocator's choice) is copied, polled, as [`Self::try_from_bytes`].
    pub fn adopt(v: Vec<u8>) -> ReadResult<SegBuf> {
        if v.as_ptr() as usize % 8 != 0 {
            return SegBuf::try_from_bytes(&v);
        }
        let len = v.len();
        Ok(SegBuf::chunked(Image::Bytes(v), len, None))
    }

    /// The image as a [`crate::mem`] chunk: charged as held now, credited in its class at the chunk's drop. Moving the
    /// storage into the chunk leaves its heap buffer where it is, so `ptr` is its base. `taken`: the charge its bytes
    /// took as they were allocated (CACHE-CHARGE), carried in the chunk.
    fn chunked(img: Image, len: usize, taken: Option<memledger::Charge>) -> SegBuf {
        let ptr = img.base();
        crate::mem::charge(len);
        let class = core::sync::atomic::AtomicU8::new(crate::mem::HELD);
        let refbit = core::sync::atomic::AtomicBool::new(false);
        let owed = core::sync::atomic::AtomicBool::new(false);
        SegBuf { chunk: Arc::new(Chunk { taken: Mutex::new(taken), _img: img, len, class, refbit, owed }), ptr, len }
    }

    /// CACHE-CHARGE: `adopt` of a rebuild made in `in_cache_held`, with the charge its blocks took (`taken`). An image
    /// copied instead (its base not 8-aligned) drops `taken` first: the rebuild's bytes go back to this thread, where
    /// `v` is freed, and the copy takes its own (`build_held`).
    fn adopt_held(v: Vec<u8>, taken: Option<memledger::Charge>) -> ReadResult<SegBuf> {
        if v.as_ptr() as usize % 8 != 0 {
            drop(taken);
            let copy = SegBuf::build_held(v.len(), true, |b| {
                let mut step = cancelpoll::step("pgrc_read SegBuf::try_from_bytes: an image copied");
                Ok(step.pieces(v.len(), cancelpoll::PIECE, |r| {
                    b[r.clone()].copy_from_slice(&v[r]);
                    Ok::<(), cancelpoll::Canceled>(())
                })?)
            });
            return copy;
        }
        let len = v.len();
        Ok(SegBuf::chunked(Image::Bytes(v), len, taken))
    }

    /// CACHE-CHARGE: the charge the image's bytes took on the part cache's holder (a held build), once: the cache's
    /// charge takes it over (`OpenPart::charge_seg`).
    fn take_charge(&self) -> Option<memledger::Charge> {
        lock(&self.chunk.taken).take()
    }

    /// Assemble an owned region from a fallible fill (the SB-7 multi-extent
    /// byte-run concatenation in `cursor::load_region`), `owner`'s from its allocation ([`Self::build_for`]).
    pub(crate) fn assemble_for(
        owner: ReadOwner,
        len: usize,
        fill: impl FnOnce(&mut [u8]) -> ReadResult<()>,
    ) -> ReadResult<SegBuf> {
        SegBuf::build_for(owner, len, true, fill)
    }

    /// Wave 3 (memlead Q3): a reader's door-1 charge of this image's bytes, committed on the thread that built it, carried
    /// in the image (dropped before its block frees, as a held build's). Only an image no clone or slice shares yet and
    /// that carries no charge takes it: else the charge comes back. (`&mut self` and one strong count, no weak ones exist:
    /// no other holder can clone it meanwhile.)
    pub(crate) fn carry_charge(&mut self, c: memledger::Charge) -> Result<(), memledger::Charge> {
        if self.is_shared() {
            return Err(c);
        }
        let mut slot = lock(&self.chunk.taken);
        if slot.is_some() {
            return Err(c);
        }
        *slot = Some(c);
        Ok(())
    }

    /// D4487 t3 (tests): the bytes the image's carried charge holds.
    #[cfg(test)]
    pub(crate) fn carried_bytes(&self) -> u64 {
        lock(&self.chunk.taken).as_ref().map_or(0, memledger::Charge::committed)
    }

    /// The image with the reader's charge `c` (`charged_for`) in it; a charge it cannot carry drops here, its bytes back
    /// on this thread with the image's (uncharged, as before). D4487 t3: the charge takes the image's `Arc<Chunk>` block too
    /// (`IMG_BLOCK`, on this thread's account since the image was made here or handed here by the cache's debit), so a
    /// cursor that keeps a reader's image past its statement keeps all of it on the image's owner.
    pub(crate) fn carry(mut self, c: Option<memledger::Charge>) -> SegBuf {
        if let Some(mut c) = c {
            c.commit(IMG_BLOCK);
            drop(self.carry_charge(c));
        }
        self
    }

    /// [`Self::build`] whose image is `owner`'s from its allocation (`charged_for`), the charge at least its capacity
    /// (`at_least`) and carried until it drops; no owner: the thread's context's (`charged_for`).
    pub(crate) fn build_for(
        owner: ReadOwner,
        len: usize,
        polled: bool,
        fill: impl FnOnce(&mut [u8]) -> ReadResult<()>,
    ) -> ReadResult<SegBuf> {
        let (built, taken) = charged_for(owner, || SegBuf::build(len, polled, fill));
        let seg = built?;
        let taken = at_least(taken, seg.cap_bytes());
        Ok(seg.carry(taken))
    }

    /// [`Self::from_bytes`] charged to `owner` (else the thread's context) from its allocation ([`Self::build_for`]).
    pub(crate) fn from_bytes_for(owner: ReadOwner, v: &[u8]) -> ReadResult<SegBuf> {
        SegBuf::build_for(owner, v.len(), false, |b| {
            b.copy_from_slice(v);
            Ok(())
        })
    }

    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `ptr` is the base of `chunk._words` (immutable, kept alive
        // by `chunk`); `len <= words.len() * 8`.
        unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// A map image handed out, under its part's mutex, by a face of residency `res`: Shared makes it sticky, Cached
    /// sets its CLOCK ref bit, Private changes nothing.
    pub(crate) fn served(&self, res: Residency) -> SegBuf {
        match res {
            Residency::Shared if self.class() != crate::mem::STICKY => self.set_class(crate::mem::STICKY),
            Residency::Cached if !self.chunk.refbit.load(Ordering::Relaxed) => {
                self.chunk.refbit.store(true, Ordering::Relaxed)
            }
            _ => {}
        }
        self.clone()
    }

    pub(crate) fn class(&self) -> u8 {
        self.chunk.class.load(Ordering::Relaxed)
    }

    fn set_class(&self, to: u8) {
        let from = self.chunk.class.swap(to, Ordering::Relaxed);
        if from != to {
            crate::mem::shift(from, to, self.len);
        }
    }

    /// Same chunk (a [`crate::mem::Guard`] holds each once).
    pub(crate) fn same(&self, other: &SegBuf) -> bool {
        Arc::ptr_eq(&self.chunk, &other.chunk)
    }

    /// The image's heap block in bytes (its capacity), as the allocator counted it.
    pub(crate) fn cap_bytes(&self) -> u64 {
        self.chunk._img.cap_bytes()
    }

    /// D3113: another clone keeps the image alive, so dropping the cache's copy frees nothing.
    pub(crate) fn is_shared(&self) -> bool {
        Arc::strong_count(&self.chunk) > 1
    }

    /// D3113 (the line's eviction class): this image, just taken out of its part's map (under the part's mutex), stays
    /// charged to the part cache until its last holder drops it when another holder keeps it (true: the drop releases
    /// it, once); false: this copy is the last, and the caller releases it now. No clone can appear meanwhile (a new one
    /// comes from the map, which no longer has it, or from a holder, whose count is already above 1). Until then the
    /// part cache's owed bytes count it ([`crate::partcache::owed`]): only its holder can free it, so a trim still past
    /// its target runs the evicted hook for it.
    fn owe_if_shared(&self) -> bool {
        let shared = self.is_shared();
        if shared && !self.chunk.owed.swap(true, Ordering::AcqRel) {
            crate::partcache::owe(self.chunk.len as u64);
        }
        shared
    }

    /// D4834: the bytes and block of an image another holder keeps, split off its part's charge as the part lets go of it
    /// (an eviction, the part's drop): its last holder's drop gives them back (`taken` drops first), where its free lands.
    fn carry_owed(&self, c: memledger::Charge) {
        let mut slot = lock(&self.chunk.taken);
        match slot.as_mut() {
            Some(t) => {
                let _ = t.absorb(c);
            }
            None => *slot = Some(c),
        }
    }

    /// D4834 (tests): the bytes the image's carried charge holds.
    #[cfg(test)]
    pub(crate) fn owed_bytes(&self) -> u64 {
        lock(&self.chunk.taken).as_ref().map_or(0, memledger::Charge::committed)
    }

    fn addr(&self) -> usize {
        Arc::as_ptr(&self.chunk) as usize
    }

    /// D3823 (debug): fill an image no other clone holds with 0xDB bytes before it frees; a shared one is left as is. The
    /// bytes poisoned.
    fn poison(&mut self) -> u64 {
        match Arc::get_mut(&mut self.chunk).map(|c| &mut c._img) {
            Some(Image::Words(w)) => {
                w.fill(0xDBDB_DBDB_DBDB_DBDB);
                self.len as u64
            }
            Some(Image::Bytes(b)) => {
                b.fill(0xDB);
                self.len as u64
            }
            None => 0,
        }
    }
}

/// D4351: live reader `id`'s granule (one short scan: a part's live readers).
fn set_low(readers: &mut [LiveReader], id: u32, low: u32) {
    // bounded("other: one part's live readers, a cursor each (sessions x read columns)")
    for r in readers.iter_mut() {
        if r.id == id {
            r.low = low;
            return;
        }
    }
}

/// A part's image map (`segs` / `unwrapped` / `staged`): the `BTreeMap` it was, plus the chunk class law (`crate::mem`).
/// `insert` is the Shared faces' (sticky), `insert_cached` a Cached admission's, and an image the map lets go of
/// (`remove`, or the part dropping) is held by whoever still has it.
#[derive(Default)]
struct ImageMap(BTreeMap<(u64, u64), SegBuf>);

impl ImageMap {
    fn get(&self, key: &(u64, u64)) -> Option<&SegBuf> {
        self.0.get(key)
    }

    fn contains_key(&self, key: &(u64, u64)) -> bool {
        self.0.contains_key(key)
    }

    fn insert(&mut self, key: (u64, u64), b: SegBuf) {
        b.set_class(crate::mem::STICKY);
        self.0.insert(key, b);
    }

    fn insert_cached(&mut self, key: (u64, u64), b: SegBuf) {
        b.set_class(crate::mem::CACHED);
        self.0.insert(key, b);
    }

    fn remove(&mut self, key: &(u64, u64)) -> Option<SegBuf> {
        let b = self.0.remove(key)?;
        b.set_class(crate::mem::HELD);
        Some(b)
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    /// The images (D3003's `evict_contents` sums a map it took; dropping the map then moves each to held).
    fn values_mut(&mut self) -> std::collections::btree_map::ValuesMut<'_, (u64, u64), SegBuf> {
        self.0.values_mut()
    }

    fn values(&self) -> std::collections::btree_map::Values<'_, (u64, u64), SegBuf> {
        self.0.values()
    }

    /// D3113: move out the images no other clone holds; a shared image stays cached and counted. The images taken (held
    /// once the taken map drops) and the bytes kept.
    fn take_sole(&mut self) -> (ImageMap, u64) {
        let mut kept = 0u64;
        let mut sole = ImageMap::default();
        // C4 RQ row 14: trunk's take_sole form (one retain, no loop over the map), into the line's ImageMap
        self.0.retain(|k, b| {
            if b.is_shared() {
                kept += b.len() as u64;
                return true;
            }
            sole.0.insert(*k, b.clone()); // alloc: Cold
            false
        });
        (sole, kept)
    }
}

impl Drop for ImageMap {
    fn drop(&mut self) {
        self.0.values().for_each(|b| b.set_class(crate::mem::HELD));
    }
}

/// D4552 (a) `drain_nodes`: the map's nodes go one at a time, each image leaving as held (the map's drop's law).
impl IntoIterator for ImageMap {
    type Item = ((u64, u64), SegBuf);
    type IntoIter = std::collections::btree_map::IntoIter<(u64, u64), SegBuf>;
    fn into_iter(mut self) -> Self::IntoIter {
        self.0.values().for_each(|b| b.set_class(crate::mem::HELD));
        core::mem::take(&mut self.0).into_iter()
    }
}

/// A Cached admission's ring slot (`crate::mem`). The part is weak, so a dropped part only leaves a dead slot.
struct PartSlot {
    part: Weak<OpenPart>,
    unwrapped: bool,
    key: (u64, u64),
    addr: usize,
}

impl crate::mem::Victim for PartSlot {
    fn visit(&self) -> crate::mem::Sweep {
        use crate::mem::Sweep;
        let Some(part) = self.part.upgrade() else { return Sweep::Dead };
        let mut st = lock(&part.state);
        let map = if self.unwrapped { &mut st.unwrapped } else { &mut st.segs };
        let Some(b) = map.get(&self.key).filter(|b| b.addr() == self.addr) else { return Sweep::Dead };
        if b.class() != crate::mem::CACHED {
            return Sweep::Sticky;
        }
        if b.chunk.refbit.swap(false, Ordering::Relaxed) {
            return Sweep::Referenced;
        }
        if Arc::strong_count(&b.chunk) > 1 {
            return Sweep::Pinned;
        }
        let b = map.remove(&self.key).expect("present under the lock");
        st.evicted = true;
        part.debit(b.len() as u64, 1);
        drop(st);
        Sweep::Evicted(b.len() as u64)
    }

    fn dead(&self) -> bool {
        let Some(part) = self.part.upgrade() else { return true };
        let st = lock(&part.state);
        let map = if self.unwrapped { &st.unwrapped } else { &st.segs };
        !map.get(&self.key).is_some_and(|b| b.addr() == self.addr && b.class() == crate::mem::CACHED)
    }
}

// ---------------------------------------------------------------------------
// fault witness
// ---------------------------------------------------------------------------

/// What a recorded fault touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultTag {
    /// PartTail (16 B, fixed open set).
    Tail,
    /// FooterFixed (96 B, fixed open set).
    Footer,
    /// The section table (fixed open set).
    SectionTable,
    /// PartHeader (64 B, fixed open set).
    Header,
    /// A footer-listed section faulted by table index.
    ListedSection {
        kind: u16,
        attno: u32,
        path_ord: u32,
    },
    /// A stream extent faulted through its ExtentRecord.
    StreamExtent {
        attno: u32,
        path_ord: u32,
        role: u8,
        extent: u32,
    },
}

/// One recorded fault: the region and why it was read (once per resident region; a cache hit records nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultEntry {
    pub tag: FaultTag,
    pub off: u64,
    pub len: u64,
}

fn stream_tag(entry: &StreamEntry, extent: u32) -> FaultTag {
    FaultTag::StreamExtent { attno: entry.attno, path_ord: entry.path_ord, role: entry.role, extent }
}

/// D4487: an open part's fault log, the tests' witness of what it read (`OpenPart::faults`). It keeps its entries only
/// where a test reads them (cfg(test), loom, or the `fault-log` feature the dependent crates' tests turn on): nothing in
/// production reads it, and it grew by one entry per re-fault for the part's life (a long-lived OpenPart in the part
/// cache, evicted and faulted again statement after statement, on no one's account).
#[derive(Default)]
struct FaultLog(#[cfg(any(test, loom, feature = "fault-log"))] Vec<FaultEntry>);

// production keeps no entry: the log is a zero-sized field there
#[cfg(not(any(test, loom, feature = "fault-log")))]
const _: () = assert!(core::mem::size_of::<FaultLog>() == 0);

impl FaultLog {
    #[inline]
    fn push(&mut self, _entry: FaultEntry) {
        #[cfg(any(test, loom, feature = "fault-log"))]
        self.0.push(_entry);
    }

    /// The entries kept (the line's C1 L5 cap, `PartState::log_fault`): 0 where none is kept (production).
    #[inline]
    fn len(&self) -> usize {
        #[cfg(any(test, loom, feature = "fault-log"))]
        let n = self.0.len();
        #[cfg(not(any(test, loom, feature = "fault-log")))]
        let n = 0;
        n
    }
}

// ---------------------------------------------------------------------------
// open-time validation facts
// ---------------------------------------------------------------------------

/// Validation facts the caller knows before open (manifest record + catalog derivation). Every present fact is
/// checked; a mismatch is a typed [`ReadError::OpenMismatch`] (spec §13.1/§5.5).
#[derive(Debug, Clone, Copy, Default)]
pub struct PartExpect {
    pub part_no: Option<u32>,
    pub rows: Option<u64>,
    pub file_len: Option<u64>,
    pub footer_off: Option<u64>,
    pub schema_fingerprint: Option<u64>,
    pub relfilenumber: Option<u64>,
}

impl PartExpect {
    /// No expectations (tests/tools); product opens use [`PartExpect::from_manifest`].
    pub fn none() -> PartExpect {
        PartExpect::default()
    }

    /// The full expectation set for a manifest-listed part; the footer's creating `(spc, db)` is not one (W0-8): a
    /// database copy (CREATE DATABASE .. TEMPLATE, ALTER DATABASE SET TABLESPACE) carries the part files to another
    /// `(spc, db)` unchanged.
    pub fn from_manifest(rec: &PartRecord, schema_fingerprint: u64, relfilenumber: u64) -> PartExpect {
        PartExpect {
            part_no: Some(rec.part_no),
            rows: Some(rec.rows),
            file_len: Some(rec.file_len),
            footer_off: Some(rec.footer_off),
            schema_fingerprint: Some(schema_fingerprint),
            relfilenumber: Some(relfilenumber),
        }
    }
}

// ---------------------------------------------------------------------------
// OpenPart
// ---------------------------------------------------------------------------

/// A run record's role flag (`prefetch_extent_run_holes_roles`): the record is a WRAPPED extent of another stream than
/// the run's entry, read in the run's pread and held raw (`PartState::staged`) until its own unwrap takes it.
pub const ROLE_STAGE_RAW: u8 = 0x80;
/// A run record's role flag: the record is footer-listed section `idx` (its `file_off`/`len`/`crc` the section entry's),
/// read in the run's pread into the segment cache exactly as `section_bytes` puts it (M2-ZONEREAD card A: a zone lane's
/// Stats / Bloom / PSMA cluster sits right after the lane's streams, so the claim run brings it along).
pub const ROLE_SECTION: u8 = 0x40;

/// Card C's census: wrapped extents staged raw by a run, and staged extents an unwrap took (no pread of its own).
pub static STAGED_INSERTED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static STAGED_TAKEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// A wait on another reader's in-flight read goes in slices of this, a tick of the waiter's step between two: a cancel
/// ends the wait within a slice (the read waited on retires its keys either way).
const INFLIGHT_SLICE: core::time::Duration = core::time::Duration::from_millis(10);

struct PartState {
    /// The segment cache: (file_off, len) → CRC-validated bytes. Write-once
    /// per key; buffers never move after insert (Arc), which is what makes
    /// dict payload regions generation-stable (spec §7, StrView §7b).
    segs: ImageMap,
    /// The UNWRAPPED-image cache for wrapped extents (O-CMP-5(a), ruled
    /// 2026-08-10): (file_off, len) of the WRAPPED extent → the rebuilt
    /// unwrapped (still-encoded) section image. Decompress once per part
    /// residency; scans hit encoded bytes. The wrapped file bytes are read,
    /// CRC-checked (the wrapper CRC law) and DROPPED — they never enter
    /// `segs` — so `resident` prices the image readers actually use.
    unwrapped: ImageMap,
    /// Lazily parsed stream directory (spec §6.2).
    stream_dir: Option<Arc<StreamDirectory>>,
    /// D4804: [`OpenPart::shred_digest`], read once.
    shred_digest: Option<u64>,
    /// Minted u64 dict epochs per (attno, path_ord) — one per open-part
    /// instance (crate doc "Dict epochs").
    dict_epochs: BTreeMap<(u32, u32), u64>,
    /// D3109: the statement epoch each (attno, path_ord) was last opened in (a cursor or a dictionary): the streams the
    /// part's last statement did not open are stale, and a content trim evicts them before whole parts.
    stream_epochs: BTreeMap<(u32, u32), u64>,
    /// Memory step 6a: the broker second each (attno, path_ord) was last opened in (only while the broker runs): the
    /// page-cache floor's file bytes ([`OpenPart::stream_file_bytes_since`]).
    stream_secs: BTreeMap<(u32, u32), u32>,
    /// The fault log (a test witness, `faults`; grows by one entry per resident region, a re-fault's too).
    faults: FaultLog,
    /// Cached faults past the log's cap (`2 × sections + 64`): counted, not logged, since an evicted region can
    /// be faulted again and again (C1 L5).
    unlogged: u64,
    /// An image of this part was evicted (the C1 L5 ring, a D3003 content trim, an extent eviction): from then on a
    /// region can be faulted again and again, so every fault site logs under the cap (`PartState::log_fault`).
    evicted: bool,
    /// Extents a prefetch run is currently reading (claim-horizon lane):
    /// a demand fault that finds its key here WAITS on `inflight_cv`
    /// instead of re-issuing the read (duplicate reads are what helped
    /// kill the WILLNEED arm — RESULTS-OFFICIAL §6).
    inflight: std::collections::BTreeSet<(u64, u64)>,
    /// [densedict] PART-GRAIN block-lazy dict payload images (the
    /// RESULTS-STACK §4.2 re-pose): (file_off, len) of the WRAPPED
    /// single-extent dict payload → the shared block-image state. The
    /// `unwrapped`-map analogue at BLOCK grain — one pread + one-block
    /// decompress per (part, block) per part residency; handle reopens
    /// Arc-clone the image instead of re-decompressing (the +49% hot tax
    /// of the handle-grain arm). Only faulted blocks commit RSS
    /// (`alloc_zeroed` reservation), so cost ≤ the `unwrapped` image.
    dict_blocks: BTreeMap<(u64, u64), Arc<crate::dicthandle::BlockLazyPayload>>,
    /// M2-ZONEREAD card C: WRAPPED extents read raw by a run that also brought a neighbouring stream's extents (a code
    /// lane's claim run carrying its dictionary index and payload, or the index and payload in one read), CRC-checked
    /// and held until their first unwrap takes them (`unwrapped_extent_bytes*`): the rebuild needs the index, which
    /// the run cannot hand it, so the bytes wait here instead of costing a pread each later. Taken once, then gone.
    staged: ImageMap,
    /// D4315: the segment / unwrapped images a scan's cursor read past (`OpenPart::pass_extents`) and their bytes: no
    /// reader of the passing statement reads them again, so a content trim may evict them while the part stays in use.
    passed: BTreeMap<(u64, u64), Passed>,
    /// D4351: the live readers that pass behind them (`StreamCursor::set_pass_behind`): each one's stream and the
    /// granule it reads (it reads nothing below it again). A passed extent no live reader of its stream is still behind
    /// goes first (`evict_passed`); one another reader has yet to reach goes only in the trim's second tier.
    readers: Vec<LiveReader>,
    next_reader: u32,
}

impl PartState {
    /// Log a fault (the witness). Once an image of the part was evicted, a region can be faulted again and again (the
    /// D3003 part cache trims whole parts' contents), so past `cap` (`OpenPart::fault_cap`) a fault is counted in
    /// `unlogged` instead (the C1 L5 law the Cached faces always keep): the log stays bounded for the part's life.
    fn log_fault(&mut self, cap: usize, e: FaultEntry) {
        if !self.evicted || self.faults.len() < cap {
            self.faults.push(e);
        } else {
            self.unlogged += 1;
        }
    }
}

/// D4351: a passed extent: its cached bytes, the stream it belongs to and the granule its extent ends at (exclusive).
#[derive(Clone, Copy, Debug)]
struct Passed {
    len: u64,
    stream: StreamTag,
    g_end: u32,
}

/// D4351: a reader's stream: (attno, path_ord, role) of the cursor (its values and validity streams both).
pub type StreamTag = (u32, u32, u8);

/// D4351: a live reader of one stream and the granule it reads (everything at or above it is still ahead of it).
#[derive(Clone, Copy, Debug)]
struct LiveReader {
    id: u32,
    stream: StreamTag,
    low: u32,
}

/// D4351: an extent a cursor read past: its image key and the granule its extent ends at (exclusive).
#[derive(Clone, Copy, Debug, Default)]
pub struct PassKey {
    pub key: (u64, u64),
    pub g_end: u32,
}

/// One open part: identity facts, validated fixed structures, and the lazy
/// segment cache. Shared process-wide as `Arc<OpenPart>` through
/// [`crate::registry::PartRegistry`]; all methods take `&self`.
pub struct OpenPart {
    /// D4552 (a): the part's own heap bytes (its io, section table, known kinds, fault log, the `Arc` block) on the part
    /// cache's holder from the open ([`PartRegistry::open_pinned`](crate::PartRegistry::open_pinned)'s cache scope), so
    /// whatever thread drops the last `Arc` (an eviction, a statement's dictionary handle) takes them back before they
    /// free. Declared first, so it drops first. Unset for a part opened outside the registry (its opener frees it).
    own: pgsync::OnceLock<memledger::Charge>,
    /// C8 move 1: the segment cache's bytes on the part cache's ledger account (a fault commits, an eviction frees);
    /// dropped before `state` frees the images.
    held: Mutex<memledger::Charge>,
    io: Box<dyn PartIo>,
    ident: PartIdent,
    uuid: [u8; 16],
    header: PartHeader,
    footer: FooterFixed,
    /// The part's elected granule grain (SB-10) — validated at open from
    /// the footer; every granule/band closed form this crate computes keys
    /// off it.
    grain: pgrc_format::geom::GranuleGrain,
    sections: Vec<SectionEntry>,
    /// Adjudicated kind per section: `None` = unknown-but-optional (skipped
    /// by readers, still CRC-faultable).
    known: Vec<Option<SectionKind>>,
    state: Mutex<PartState>,
    /// Signalled when a prefetch run retires in-flight keys.
    inflight_cv: pgsync::Condvar,
    /// Pin count (registry law: pinned parts are never evicted).
    pub(crate) pin_count: AtomicU32,
    /// Logical LRU stamp (registry clock — never wall time).
    pub(crate) last_used: AtomicU64,
    /// Bytes resident in the segment cache.
    resident: AtomicU64,
    /// D4795 (2a): the statement `resident`'s latest census bytes were charged to (a leaf lock: only `count` / `uncount`
    /// run under it).
    gov: Mutex<crate::partcache::Charged>,
    /// D3003: statements (and kept cursors) using the part now ([`PartUse`]): a part in use is never content-evicted.
    users: AtomicU32,
    /// D3994 (B.1 W): of `users`, the parked executors' uses (a kept cursor past its statement: the session's, the depot's,
    /// [`PartUse::transfer`] marks them): they read nothing until a statement binds them again, so they pin nothing; a part
    /// whose every use is parked is evictable under the memory policy's gate (`partcache::parked_evictable`).
    parked: AtomicU32,
    /// D4315: the owners (memory ledger account, owner key) whose statement deferred the part (D3823's gate: every claim
    /// left it, held for the statement's next run while the cache has room), with their deferred uses ([`PartUse::defer`]),
    /// and how many entries: their uses read nothing more, so they are no part of the cache's floor (`live_claimed`).
    deferred: Mutex<Vec<(u32, u64, u32)>>,
    ndeferred: AtomicU32,
    /// D3999 + C8: the users' owners, as (memory ledger account, owner key: `memledger::Owner::key`), and their uses: the
    /// bytes in use by account, and by owner (the per-owner rank).
    user_accts: Mutex<Vec<(u32, u64, u32)>>,
    /// D4554 (code.heads' kept counts, the `heads_cache` switch): rows of a code in this part, as (column, code, rows),
    /// the counts its statements' emits asked (`keep_code_counts`), kept with the part's cached contents: the part cache's
    /// bytes (its census and holder's charge, by the list's capacity), dropped at their eviction (`evict`).
    code_counts: Mutex<Vec<(i16, u32, u32)>>,
    /// D3003: the part's use stamp (`partcache::stamp`: the statement epoch of its last statement, then its first touch
    /// in it, latest first); content eviction runs in its order.
    cache_used: AtomicU64,
    /// Memory step 6a: the re-use token of the part's latest statement ([`OpenPart::use_part_in`]).
    reuse_stmt: AtomicU64,
    /// Memory step 6a: the broker seconds of the part's latest two statements' uses: the latest in the low half, the one
    /// before in the high half (0 = none). Re-used inside a window = the one before is inside it.
    reuse_secs: AtomicU64,
    /// Memory step 6a: the most a content eviction freed since the latest statement's first use: the part's NEED is
    /// max(cached bytes, this), so a cache that evicts what it re-uses reports its working set, not its size.
    last_evicted: AtomicU64,
    /// ZONEPRUNE F2: per column (by attno) its Stats section's records, decoded on a scan's first ask and kept
    /// ([`Self::zone_records`]); unallocated until then.
    zone: pgsync::OnceLock<ZoneMemo<ZoneRecordsCell>>,
    /// The bytes content evictions freed of this part that no fault has brought back yet: a fault takes its bytes off,
    /// as the part cache's re-miss (memledger::value).
    owed: AtomicU64,
    /// Broker (b): the part cache's byte clock at this part's last content eviction (0: none since its last fault), its
    /// ghost (`memledger::value::ghost`: the refault distance is the bytes evicted since).
    evicted_at: AtomicU64,
    /// D4315: the bytes of `PartState::passed` (one relaxed load answers "none passed" on the hot paths).
    passed_bytes: AtomicU64,
    /// D4504: the bytes of this part's metadata faces its holders keep ([`FaceHold`]: Gravity's part lists' unwrapped
    /// Stats walk, bloom and PSMA bodies), in the part cache's census beside its contents; the content trim's last tier
    /// lets them go ([`OpenPart::evict_faces`]), after every data tier.
    face_bytes: AtomicU64,
    /// D4504: bumped by each [`OpenPart::evict_faces`]: a [`FaceHold`] taken before it is stale, and its holder lets it go.
    face_gen: AtomicU64,
    /// D4570: the part's file left its relation ([`crate::partcache::drop_parts`]).
    dropped: AtomicBool,
}

/// D4504: a holder's metadata bytes of one part (a part list's faces of it): charged to the part cache's holder, and,
/// `counted`, in the part cache's census and the part's `face_bytes` from [`FaceHold::add`] to the drop, so the part
/// cache's budget bounds them and its content trim lets them go with their part ([`OpenPart::evict_faces`], after its
/// data). It names its part without keeping it (a part gone takes its counter with it; the census still forgets the
/// bytes at the drop). Declare it before the bodies it counts: its drop moves their bytes back to the freeing thread
/// before they free.
pub struct FaceHold {
    charge: memledger::Charge,
    part: std::sync::Weak<OpenPart>,
    bytes: u64,
    /// Of `bytes`, those in the census and the part's `face_bytes` (`counted` holds of a live part).
    census: u64,
    /// D4795 (2a): the statement `census` was charged to.
    gov: crate::partcache::Charged,
    gen: u64,
    counted: bool,
}

impl FaceHold {
    /// A hold of `part`'s faces: `counted` false (the off switch) charges them only, as before D4504.
    pub fn new(part: &Arc<OpenPart>, counted: bool) -> FaceHold {
        FaceHold { charge: crate::partcache::holder_charge(0), part: Arc::downgrade(part), bytes: 0, census: 0, gov: Default::default(), gen: part.face_gen.load(Ordering::Acquire), counted }
    }

    /// A hold of no part's faces (a list over parts it did not open): charged only, never stale.
    pub fn unpinned() -> FaceHold {
        FaceHold { charge: crate::partcache::holder_charge(0), part: std::sync::Weak::new(), bytes: 0, census: 0, gov: Default::default(), gen: 0, counted: false }
    }

    /// `n` more bytes, just allocated on this thread, held.
    pub fn add(&mut self, n: u64) {
        if n == 0 {
            return;
        }
        self.charge.commit(n);
        self.bytes += n;
        if !self.counted {
            return;
        }
        if let Some(p) = self.part.upgrade() {
            p.face_bytes.fetch_add(n, Ordering::Relaxed);
            crate::partcache::count(n, &mut self.gov);
            self.census += n;
        }
    }

    /// The bytes held.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Whether a content trim let this part's faces go since the hold was taken, or the part is gone: its holder drops it
    /// (a hold the census does not count is never stale).
    pub fn stale(&self) -> bool {
        self.counted && self.part.upgrade().is_none_or(|p| p.face_gen.load(Ordering::Acquire) != self.gen)
    }
}

impl Drop for FaceHold {
    fn drop(&mut self) {
        if self.census > 0 {
            if let Some(p) = self.part.upgrade() {
                p.face_bytes.fetch_sub(self.census, Ordering::Relaxed);
            }
            crate::partcache::uncount(self.census, &mut self.gov);
        }
    }
}

/// One column's [`OpenPart::zone_records`] (`None`: the part has no Stats section for it).
type ZoneRecordsCell = pgsync::OnceLock<Option<ZoneMemo<Option<StatsRecord>>>>;

/// ZONEPRUNE F2: a slice of a part's zone memo (the per-attno cell table, or one column's records), its bytes charged
/// from its build to its drop: sticky in the residency plane ([`crate::mem::zone_bytes`]), and the part's
/// [`OpenPart::resident`] (so the registry's byte budget and its janitor see them) once the part keeps it. It lives
/// inside its OpenPart, so it goes when the part goes (a registry eviction or a newer generation's forget drops the
/// entry; the last holder's drop drops the part) and never outlives it.
struct ZoneMemo<T>(Box<[T]>);

impl<T> ZoneMemo<T> {
    fn new(v: Box<[T]>) -> ZoneMemo<T> {
        crate::mem::zone_charge(core::mem::size_of_val(&*v));
        ZoneMemo(v)
    }

    fn bytes(&self) -> u64 {
        core::mem::size_of_val(&*self.0) as u64
    }
}

/// The bytes a zone memo cell's value counts.
trait ZoneBytes {
    fn zone_bytes(&self) -> u64;
}

impl<T> ZoneBytes for ZoneMemo<T> {
    fn zone_bytes(&self) -> u64 {
        self.bytes()
    }
}

impl<T> ZoneBytes for Option<ZoneMemo<T>> {
    fn zone_bytes(&self) -> u64 {
        self.as_ref().map_or(0, ZoneMemo::bytes)
    }
}

impl<T> Drop for ZoneMemo<T> {
    fn drop(&mut self) {
        crate::mem::zone_credit(core::mem::size_of_val(&*self.0));
    }
}

/// Memory step 6a: a use without a statement token counts as a new statement's at its first use in an epoch at least this
/// many broker seconds after the latest.
const REUSE_FALLBACK_S: u32 = 2;

/// D3003: a use of a part's cached contents (a stream cursor, a statement's part cell): while one lives the part's
/// segment and dictionary images are never evicted (datums may alias them for the user's life — the region-stability
/// law the eviction must keep). Creating one stamps the part's use clock.
pub struct PartUse {
    /// C8: the use as its owner's charge (`memledger::charge::context` at the take: the portal, statement or session of the
    /// taking thread's backend, a claim's leader's; a cache's holder elsewhere); `transfer` moves it.
    charge: memledger::Charge,
    part: Arc<OpenPart>,
    /// D3994 (B.1 W): a parked executor's use (counted in the part's `parked`): set by a transfer to a session or the
    /// depot, cleared by one back to a running owner. A take is never parked, whatever its context: a use taken outside a
    /// statement stays live.
    parked: bool,
    /// D4315: the owner key this use marked deferred on its part ([`PartUse::defer`]), cleared by `undefer` or its drop.
    deferred: Option<(u32, u64)>,
}

/// D4554: one part's use moves (`PartUse::transfer_in`): its user list locked at the first move that changes a user, its
/// parked count moved at the end. The count lags the group's moves until then: a concurrent trim may see a rejoining
/// part (parked -> running) as parked-only for that window and evict its contents, which is safe (a running cursor's
/// images are its own clones, and a later read faults the part again); the window is one source's bind.
pub struct UseMoves<'p> {
    part: &'p OpenPart,
    users: Option<pgsync::MutexGuard<'p, Vec<(u32, u64, u32)>>>,
    parked: i64,
}

impl<'p> UseMoves<'p> {
    /// The group of `part`'s use moves (its caller holds the part).
    pub fn new(part: &'p OpenPart) -> UseMoves<'p> {
        UseMoves { part, users: None, parked: 0 }
    }

    /// Whether `part` is this group's.
    pub fn is_of(&self, part: &OpenPart) -> bool {
        core::ptr::eq(self.part, part)
    }
}

impl Drop for UseMoves<'_> {
    fn drop(&mut self) {
        self.users = None;
        match self.parked {
            0 => {}
            n if n > 0 => {
                // SeqCst with the dropped mark's (`running_use_ended`), as one use's park (`PartUse::transfer`)
                self.part.parked.fetch_add(n as u32, Ordering::SeqCst);
                // D4570: the group may have parked a dropped part's last running uses: its contents go now
                self.part.running_use_ended();
            }
            n => {
                self.part.parked.fetch_sub(n.unsigned_abs() as u32, Ordering::Relaxed);
            }
        }
    }
}

/// D3994 (B.1 W): the owners whose uses are parked executors' (a kept cursor between statements): a backend's session,
/// the slot depot.
fn parks(o: memledger::Owner) -> bool {
    matches!(o, memledger::Owner::Session(_) | memledger::Owner::Cache(memledger::Holder::Parked))
}

/// A use moves from user `from` to user `into` on a part's user list (`OpenPart::user_accts`: an entry per user with its
/// count): false, nothing moved, when `from` has no entry or the list cannot grow by `into`'s.
fn move_user(u: &mut Vec<(u32, u64, u32)>, from: (u32, u64), into: (u32, u64)) -> bool {
    let Some(i) = u.iter().position(|e| (e.0, e.1) == from) else { return false };
    match u.iter().position(|e| (e.0, e.1) == into) {
        Some(j) => u[j].2 += 1,
        None if u.try_reserve(1).is_ok() => u.push((into.0, into.1, 1)),
        None => return false,
    }
    u[i].2 -= 1;
    if u[i].2 == 0 {
        u.swap_remove(i);
    }
    true
}

/// A user-list entry's key: the owner's account and its key.
fn user_key(o: memledger::Owner) -> (u32, u64) {
    (o.acct().0, o.key())
}

impl PartUse {
    pub fn part(&self) -> &Arc<OpenPart> {
        &self.part
    }

    /// C8: the use's owner now.
    pub fn owner(&self) -> memledger::Owner {
        self.charge.owner()
    }

    /// C8: where the use ends (its taker's context's, or by row: `use_part_by_row`).
    pub fn release(&self) -> memledger::Release {
        self.charge.release()
    }

    /// C8: the use moves to owner `to`, released at `release` (a kept cursor past its statement goes to its session, a
    /// parked worker set's to the depot): the part's user list and the owners' cells, once. Nothing when the list cannot
    /// take the new owner's entry (no memory): the use stays with its owner.
    pub fn transfer(&mut self, to: memledger::Owner, release: memledger::Release) {
        // D4554: a move to where it is moves nothing (a kept cursor's use already parked at a worker set's park)
        if self.charge.owner() == to && self.charge.release() == release {
            self.undefer();
            return;
        }
        let (from, into) = (user_key(self.charge.owner()), user_key(to));
        // D4552 (a): the user list grows in the part cache's account (`OpenPart::homed`)
        if from != into && !{
            let _pc = self.part.homed();
            move_user(&mut lock(&self.part.user_accts), from, into)
        } {
            return;
        }
        self.charge.transfer(to, release);
        // D4315: a deferral marks the owner the use had; a moved use is its new owner's, which deferred nothing
        self.undefer();
        // D3994 (B.1 W): a move to a parked executor's owner parks the use, one back to a running owner un-parks it
        let park = parks(to);
        if park != self.parked {
            if park {
                // SeqCst with the dropped mark's (`running_use_ended`)
                self.part.parked.fetch_add(1, Ordering::SeqCst);
            } else {
                self.part.parked.fetch_sub(1, Ordering::Relaxed);
            }
            self.parked = park;
            if park {
                self.part.running_use_ended();
            }
        }
    }

    /// D4554: `transfer` for a run of uses over ONE part (`group`'s), the part's user list locked and its parked count
    /// moved once for the run: a backend moves every kept cursor's use at each statement's bind and end, several cursors
    /// (a source's columns) a part, and each lock and count is a cache line another core touched last.
    pub fn transfer_in(&mut self, group: &mut UseMoves<'_>, to: memledger::Owner, release: memledger::Release) {
        debug_assert!(core::ptr::eq(&*self.part, group.part), "a use moved in another part's group");
        if self.charge.owner() == to && self.charge.release() == release {
            self.undefer();
            return;
        }
        let (from, into) = (user_key(self.charge.owner()), user_key(to));
        if from != into && !{
            let _pc = group.part.homed();
            move_user(group.users.get_or_insert_with(|| lock(&group.part.user_accts)), from, into)
        } {
            return;
        }
        self.charge.transfer(to, release);
        self.undefer();
        let park = parks(to);
        if park != self.parked {
            group.parked += if park { 1 } else { -1 };
            self.parked = park;
        }
    }

    /// D4554 (`kept_parked`): a statement's or portal's use parks where it is, its charge left on that owner for one later
    /// move (the slot depot's park, else the statement's end to the session); the part sees what `transfer_in` to the
    /// session shows.
    pub fn park_pending_in(&mut self, group: &mut UseMoves<'_>) {
        debug_assert!(core::ptr::eq(&*self.part, group.part), "a use moved in another part's group");
        if !matches!(self.charge.owner(), memledger::Owner::Statement { .. } | memledger::Owner::Portal { .. }) {
            return;
        }
        self.undefer();
        if !self.parked {
            group.parked += 1;
            self.parked = true;
        }
    }

    /// D3994 (B.1 W): whether this use is a parked executor's (it pins nothing).
    pub fn is_parked(&self) -> bool {
        self.parked
    }

    /// D4315: the use's statement deferred its part (D3823's gate: every claim of the statement left the part and the
    /// cache had room, so the statement keeps it for its next run): from now every use of this owner on the part is out of
    /// the cache's floor (`OpenPart::live_claimed`), until `undefer` or the use drops. No memory for the mark: the part stays
    /// in the floor, as before.
    pub fn defer(&mut self) {
        if self.deferred.is_some() {
            return;
        }
        let key = user_key(self.charge.owner());
        let mut d = lock(&self.part.deferred);
        let _pc = self.part.homed();
        match d.iter().position(|e| (e.0, e.1) == key) {
            Some(i) => d[i].2 += 1,
            None if d.try_reserve(1).is_ok() => {
                d.push((key.0, key.1, 1));
                self.part.ndeferred.fetch_add(1, Ordering::Release);
            }
            None => return,
        }
        self.deferred = Some(key);
    }

    /// D4315: the statement reads the part again (a second pass enters it) or gives it back: its uses count in the floor.
    pub fn undefer(&mut self) {
        let Some(key) = self.deferred.take() else { return };
        let mut d = lock(&self.part.deferred);
        if let Some(i) = d.iter().position(|e| (e.0, e.1) == key) {
            d[i].2 -= 1;
            if d[i].2 == 0 {
                d.swap_remove(i);
                self.part.ndeferred.fetch_sub(1, Ordering::Release);
            }
        }
    }

    /// D4315: whether this use's statement deferred its part.
    pub fn is_deferred(&self) -> bool {
        self.deferred.is_some()
    }
}

impl Drop for PartUse {
    /// The part's count and user list; the charge (the owner's cell) discharges with the field.
    fn drop(&mut self) {
        self.undefer();
        if self.parked {
            self.part.parked.fetch_sub(1, Ordering::Relaxed);
        }
        // SeqCst with the dropped mark's (`running_use_ended`): a drop and this use's end never both miss the other
        self.part.users.fetch_sub(1, Ordering::SeqCst);
        let key = user_key(self.charge.owner());
        {
            let mut u = lock(&self.part.user_accts);
            if let Some(i) = u.iter().position(|e| (e.0, e.1) == key) {
                u[i].2 -= 1;
                if u[i].2 == 0 {
                    u.swap_remove(i);
                }
            }
        }
        if !self.parked {
            self.part.running_use_ended();
        }
    }
}

impl Drop for OpenPart {
    fn drop(&mut self) {
        // D4552 (a): a registry-opened part's state maps go here, their nodes where they were allocated (`homed`), their
        // values on this thread after the segment cache's charge gave this thread the images' bytes (as `held` dropping
        // before `state` did)
        if self.own.get().is_some() {
            // D4834: an image another holder keeps takes its bytes and block off `held` first
            {
                let st = lock(&self.state);
                let mut held = lock(&self.held);
                // bounded("other: one part's state maps, a split per image another holder keeps (a Drop cannot stop)")
                for b in [&st.segs, &st.unwrapped, &st.staged].into_iter().flat_map(|m| m.values()).filter(|b| b.is_shared()) {
                    b.carry_owed(held.split_off(b.len() as u64 + IMG_BLOCK));
                }
            }
            drop(core::mem::replace(&mut *lock(&self.held), memledger::Charge::NONE));
            let mut st = lock(&self.state);
            let (segs, unwrapped, staged, passed, inflight) = (core::mem::take(&mut st.segs), core::mem::take(&mut st.unwrapped), core::mem::take(&mut st.staged), core::mem::take(&mut st.passed), core::mem::take(&mut st.inflight));
            let (dict_epochs, stream_epochs, stream_secs, dict_blocks) = (core::mem::take(&mut st.dict_epochs), core::mem::take(&mut st.stream_epochs), core::mem::take(&mut st.stream_secs), core::mem::take(&mut st.dict_blocks));
            let faults = core::mem::take(&mut st.faults);
            drop(st);
            self.drain_nodes(segs);
            self.drain_nodes(unwrapped);
            self.drain_nodes(staged);
            self.drain_nodes(passed);
            self.drain_nodes(inflight);
            self.drain_nodes(dict_epochs);
            self.drain_nodes(stream_epochs);
            self.drain_nodes(stream_secs);
            self.drain_nodes(dict_blocks);
            let _pc = self.homed();
            drop(faults);
            // the use lists grew there too (`use_by`, a use's move or deferral)
            drop(core::mem::take(&mut *lock(&self.deferred)));
            drop(core::mem::take(&mut *lock(&self.user_accts)));
        }
        // the census forgets what this part still held (its maps free with it; their memory leaves the cache's account
        // with `held`); D3113: but an image another holder keeps stays counted until that holder drops it
        let owed: u64 = {
            let st = lock(&self.state);
            let mut held = lock(&self.held);
            // D4834: and its bytes and block go into it before `held` drops
            let mut owe = |b: &SegBuf| {
                b.carry_owed(held.split_off(b.len() as u64 + IMG_BLOCK));
                b.len() as u64
            };
            [&st.segs, &st.unwrapped, &st.staged].into_iter().flat_map(|m| m.values()).filter(|b| b.owe_if_shared()).map(|b| owe(b)).sum()
        };
        crate::partcache::uncount(self.resident.load(Ordering::Relaxed).saturating_sub(owed), &mut lock(&self.gov));
        // D4607: counted after its last strong reference went, so a holder that reads the count and then finds this part's
        // weak references dead sees every close the count names
        note_part_closed();
    }
}

/// D4607: how many open parts have closed in this process (an [`OpenPart`] dropped). A holder keyed by open parts (the
/// condition cache: its entries name their part weakly) compares it with the count at its last sweep, so it walks its
/// entries for closed parts' only when some part closed since.
static CLOSED_PARTS: AtomicU64 = AtomicU64::new(0);

/// D4607: an open part closed (its drop; a test stands one in).
pub fn note_part_closed() {
    CLOSED_PARTS.fetch_add(1, Ordering::Release);
}

/// D4607: the count of closed parts ([`CLOSED_PARTS`]).
pub fn closed_parts() -> u64 {
    CLOSED_PARTS.load(Ordering::Acquire)
}

impl core::fmt::Debug for OpenPart {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OpenPart")
            .field("ident", &self.ident)
            .field("uuid", &self.uuid)
            .finish_non_exhaustive()
    }
}

impl OpenPart {
    /// Open and validate a part (spec §5): tail → footer → section table →
    /// header, all CRC-checked, geometry echoes verified, expectation facts
    /// enforced. Exactly four reads; nothing else is faulted.
    /// D4552 (a): the open's charge (the registry's cache scope) for the part to carry ([`OpenPart::own`]); a second is
    /// dropped (its bytes go back to this thread, where they were taken).
    /// D4552 (a): the part cache's account entered around a change of a registry-opened part's state maps (`PartState`):
    /// their NODES are allocated and freed there, whichever thread faults, evicts or drops the part, so no thread's
    /// account carries another's. Only the map operation: a value goes in already charged (an image's own charge) and
    /// comes out to drop outside (`drain_nodes`). `None` for a part opened outside the registry, or with the memory
    /// policy's `ledger_home` off.
    #[inline]
    fn homed(&self) -> Option<memledger::Entered> {
        if self.own.get().is_some() {
            crate::partcache::home().enter()
        } else {
            None
        }
    }

    /// D4552 (a): a state map let go: each node freed where it was allocated ([`Self::homed`]), each value dropped outside,
    /// on this thread, where its own charge gives its bytes back.
    fn drain_nodes<I: IntoIterator>(&self, m: I) {
        let mut it = {
            let _pc = self.homed();
            m.into_iter()
        };
        // bounded("other: one part's state map, a node a step; the part's drop or eviction runs it to its end (a Drop cannot stop)")
        loop {
            let next = {
                let _pc = self.homed();
                it.next()
            };
            match next {
                Some(v) => drop(v),
                None => break,
            }
        }
        let _pc = self.homed();
        drop(it);
    }

    pub(crate) fn carry_own(&self, c: Option<memledger::Charge>) {
        // a block the general road served (no cache heap, or a full one) took no cache-road charge in the scope: its
        // lists' and its `Arc` block's bytes, counted on this thread, are committed now (`at_least`)
        let lists = self.sections.capacity() * core::mem::size_of::<SectionEntry>() + self.known.capacity() * core::mem::size_of::<Option<SectionKind>>();
        if let Some(c) = at_least(c, (core::mem::size_of::<OpenPart>() + 2 * core::mem::size_of::<usize>() + lists) as u64) {
            let _ = self.own.set(c);
        }
    }

    pub fn open(io: Box<dyn PartIo>, expect: &PartExpect) -> ReadResult<OpenPart> {
        io.advise_random();
        let file_len = io.len();
        let min = (PART_HEADER_LEN + FOOTER_FIXED_LEN + PART_TAIL_LEN) as u64;
        if file_len < min {
            return Err(ReadError::Format(FormatError::Truncated {
                at: "part file",
            }));
        }
        if let Some(want) = expect.file_len {
            if want != file_len {
                return Err(ReadError::OpenMismatch { field: "file_len" });
            }
        }
        let mut faults = FaultLog::default();

        // 1. Tail (spec §5.4).
        let tail_off = file_len - PART_TAIL_LEN as u64;
        let mut tail_buf = [0u8; PART_TAIL_LEN];
        io.pread_exact(tail_off, &mut tail_buf, "PartTail")?;
        faults.push(FaultEntry {
            tag: FaultTag::Tail,
            off: tail_off,
            len: PART_TAIL_LEN as u64,
        });
        let footer_off = decode_tail(&tail_buf, file_len)?;
        if let Some(want) = expect.footer_off {
            if want != footer_off {
                return Err(ReadError::OpenMismatch {
                    field: "footer_off",
                });
            }
        }
        let (dev, ino) = io.dev_ino();
        let ident = PartIdent {
            dev,
            ino,
            len: file_len,
            footer_off,
        };

        // 2. Footer (spec §5.3; CRC + geometry echoes inside decode).
        let mut footer_buf = [0u8; FOOTER_FIXED_LEN];
        io.pread_exact(footer_off, &mut footer_buf, "FooterFixed")?;
        faults.push(FaultEntry {
            tag: FaultTag::Footer,
            off: footer_off,
            len: FOOTER_FIXED_LEN as u64,
        });
        let footer = FooterFixed::decode(&footer_buf)?;
        // SB-10: the grain is ladder-validated inside decode; bind the
        // typed form once for every closed form downstream.
        let grain = footer.grain()?;
        if let Some(want) = expect.rows {
            if want != footer.rows {
                return Err(ReadError::OpenMismatch { field: "rows" });
            }
        }
        if let Some(want) = expect.part_no {
            if want != footer.part_no {
                return Err(ReadError::OpenMismatch { field: "part_no" });
            }
        }
        if let Some(want) = expect.schema_fingerprint {
            if want != footer.schema_fingerprint {
                return Err(ReadError::OpenMismatch {
                    field: "schema_fingerprint",
                });
            }
        }

        // 3. Section table (spec §5.2).
        let st_len = footer.section_count as u64 * SECTION_ENTRY_LEN as u64;
        let st_off = footer.section_table_off;
        if st_off
            .checked_add(st_len)
            .map(|end| end > file_len)
            .unwrap_or(true)
        {
            return Err(ReadError::Format(FormatError::Bounds {
                at: "section table",
            }));
        }
        let mut st_buf = pgrc_format::try_filled(st_len as usize, 0u8, "openpart")?;
        io.pread_exact(st_off, &mut st_buf, "section table")?;
        faults.push(FaultEntry {
            tag: FaultTag::SectionTable,
            off: st_off,
            len: st_len,
        });
        if crc32c(&st_buf) != footer.section_table_crc {
            return Err(ReadError::Format(FormatError::CrcMismatch {
                at: "section table",
            }));
        }
        let mut sections = pgrc_format::try_vec(footer.section_count as usize, "openpart")?;
        let mut known = pgrc_format::try_vec(footer.section_count as usize, "openpart")?;
        let mut c = Cur::new(&st_buf);
        // bounded("catalog-sized: one decode per section entry, the part's streams and their meta sections (its columns times their roles); the section table's bytes, read above, bound the count")
        for _ in 0..footer.section_count {
            let e = SectionEntry::decode(&mut c)?;
            // Unknown non-optional kind: typed refusal at open (spec §5.2).
            known.push(e.known_kind()?);
            sections.push(e);
        }
        validate_section_layout(&sections, file_len, footer_off, st_off, st_len)?;

        // 4. Header (spec §5.1) + echo checks.
        let mut hdr_buf = [0u8; PART_HEADER_LEN];
        io.pread_exact(0, &mut hdr_buf, "PartHeader")?;
        faults.push(FaultEntry {
            tag: FaultTag::Header,
            off: 0,
            len: PART_HEADER_LEN as u64,
        });
        let header = PartHeader::decode(&hdr_buf)?;
        if header.part_no != footer.part_no {
            return Err(ReadError::Format(FormatError::Corrupt {
                at: "header/footer part_no echo",
            }));
        }
        if header.schema_fingerprint != footer.schema_fingerprint {
            return Err(ReadError::Format(FormatError::Corrupt {
                at: "header/footer fingerprint echo",
            }));
        }
        if let Some(want) = expect.relfilenumber {
            if want != header.relfilenumber {
                return Err(ReadError::OpenMismatch {
                    field: "relfilenumber",
                });
            }
        }

        let uuid = part_uuid(&ident);
        Ok(OpenPart {
            own: pgsync::OnceLock::new(),
            io,
            ident,
            uuid,
            header,
            footer,
            grain,
            sections,
            known,
            held: Mutex::new(crate::partcache::holder_charge(0)),
            state: Mutex::new(PartState {
                segs: ImageMap::default(),
                unwrapped: ImageMap::default(),
                stream_dir: None,
                shred_digest: None,
                dict_epochs: BTreeMap::new(),
                stream_epochs: BTreeMap::new(),
                stream_secs: BTreeMap::new(),
                faults,
                unlogged: 0,
                evicted: false,
                inflight: std::collections::BTreeSet::new(),
                dict_blocks: BTreeMap::new(),
                staged: ImageMap::default(),
                passed: BTreeMap::new(),
                readers: Vec::new(),
                next_reader: 0,
            }),
            pin_count: AtomicU32::new(0),
            last_used: AtomicU64::new(0),
            resident: AtomicU64::new(0),
            gov: Mutex::new(crate::partcache::Charged { slot: 0, epoch: 0, bytes: 0 }),
            users: AtomicU32::new(0),
            parked: AtomicU32::new(0),
            deferred: Mutex::new(Vec::new()),
            ndeferred: AtomicU32::new(0),
            user_accts: Mutex::new(Vec::new()),
            code_counts: Mutex::new(Vec::new()),
            cache_used: AtomicU64::new(0),
            reuse_stmt: AtomicU64::new(0),
            reuse_secs: AtomicU64::new(0),
            last_evicted: AtomicU64::new(0),
            owed: AtomicU64::new(0),
            evicted_at: AtomicU64::new(0),
            passed_bytes: AtomicU64::new(0),
            face_bytes: AtomicU64::new(0),
            face_gen: AtomicU64::new(0),
            dropped: AtomicBool::new(false),
            inflight_cv: pgsync::Condvar::new(),
            zone: pgsync::OnceLock::new(),
        })
    }

    pub fn ident(&self) -> PartIdent {
        self.ident
    }

    /// The part file's page-cache residency over `[off, off+len)` (`PartIo::page_resident`).
    pub fn page_resident(&self, off: u64, len: u64) -> Option<(u64, u64)> {
        self.io.page_resident(off, len)
    }

    pub fn uuid(&self) -> [u8; 16] {
        self.uuid
    }

    pub fn header(&self) -> &PartHeader {
        &self.header
    }

    pub fn footer(&self) -> &FooterFixed {
        &self.footer
    }

    pub fn rows(&self) -> u64 {
        self.footer.rows
    }

    /// The part's elected granule grain (SB-10; footer-validated at open).
    pub fn grain(&self) -> pgrc_format::geom::GranuleGrain {
        self.grain
    }

    pub fn sections(&self) -> &[SectionEntry] {
        &self.sections
    }

    /// Adjudicated kind of section `idx` (`None` = unknown-but-optional).
    pub fn section_kind(&self, idx: usize) -> Option<SectionKind> {
        self.known.get(idx).copied().flatten()
    }

    /// Find the first section of `kind` for (attno, path_ord).
    pub fn find_section(&self, kind: SectionKind, attno: u32, path_ord: u32) -> Option<usize> {
        self.sections.iter().enumerate().position(|(i, e)| {
            self.known[i] == Some(kind) && e.attno == attno && e.path_ord == path_ord
        })
    }

    /// ZONEPRUNE F2: column `attno`'s Stats section (path 0) as its records: record `g` is the one at
    /// `g * STATS_RECORD_LEN` (`None` where it does not decode, as `pgrc_scan::ColumnMeta::granule_record` reads it).
    /// Decoded on the first ask and kept on the part, which never changes, so every later scan's per-granule zone
    /// check is a lookup (no section fault, lock, copy or decode). `body` loads the section's unwrapped body
    /// (`Ok(None)`: the part has none, kept as such); a failed load is not kept, so the next ask retries. The load
    /// runs outside both cells (a racing asker decodes too; the first `set` wins). The kept bytes (the cell table and
    /// each column's records) count in [`Self::resident`] and as sticky in [`crate::mem`] ([`Self::zone_resident`]),
    /// and go with the part.
    pub fn zone_records<E>(
        &self,
        attno: u32,
        body: impl FnOnce() -> Result<Option<Vec<u8>>, E>,
    ) -> Result<Option<&[Option<StatsRecord>]>, E> {
        let table = match self.zone.get() {
            Some(t) => t,
            None => {
                let n = self.sections.iter().zip(&self.known)
                    // a column's attno (MaxTupleAttributeNumber bounds it; a stray larger one is never a column)
                    .filter(|(e, k)| **k == Some(SectionKind::Stats) && e.path_ord == 0 && e.attno <= 1664)
                    .map(|(e, _)| e.attno as usize + 1)
                    .max()
                    .unwrap_or(0);
                self.keep_zone(&self.zone, ZoneMemo::new((0..n).map(|_| ZoneRecordsCell::new()).collect()));
                self.zone.get().expect("zone table just set")
            }
        };
        let Some(cell) = table.0.get(attno as usize) else { return Ok(None) };
        if let Some(recs) = cell.get() {
            return Ok(recs.as_ref().map(|r| &*r.0));
        }
        let recs: Option<ZoneMemo<Option<StatsRecord>>> = body()?.map(|b| {
            ZoneMemo::new(b.chunks_exact(STATS_RECORD_LEN).map(|r| StatsRecord::decode(&mut Cur::new(r)).ok()).collect())
        });
        self.keep_zone(cell, recs);
        Ok(cell.get().expect("zone records just set").as_ref().map(|r| &*r.0))
    }

    /// ZONEPRUNE F2: set a zone memo cell; the first set's bytes join the part's [`Self::resident`] through
    /// `Self::charge`, so the part cache's census counts what the part's drop debits (a racing loser's value drops
    /// here, credited by its own drop).
    fn keep_zone<V: ZoneBytes>(&self, cell: &pgsync::OnceLock<V>, v: V) {
        let bytes = v.zone_bytes();
        if cell.set(v).is_ok() {
            self.charge(bytes);
        }
    }

    /// ZONEPRUNE F2: the bytes of this part's zone memo (of [`Self::resident`]).
    pub fn zone_resident(&self) -> u64 {
        let Some(t) = self.zone.get() else { return 0 };
        t.bytes() + t.0.iter().filter_map(|c| c.get()).map(|v| v.zone_bytes()).sum::<u64>()
    }

    /// Bytes resident in the segment cache.
    pub fn resident(&self) -> u64 {
        self.resident.load(Ordering::Relaxed)
    }

    /// D4487 t2 (tests): the part cache's charge of this part's segment cache, and its cached images.
    #[cfg(test)]
    pub(crate) fn held_and_images(&self) -> (u64, u64) {
        let st = lock(&self.state);
        (lock(&self.held).committed(), (st.segs.len() + st.unwrapped.len() + st.staged.len()) as u64)
    }

    /// D4487 t2 (tests): a cached segment's key.
    #[cfg(test)]
    pub(crate) fn a_cached_key(&self) -> Option<(u64, u64)> {
        lock(&self.state).segs.0.keys().next().copied()
    }

    /// CACHE-CHARGE: the cache's charge of image `seg` (`n` of its bytes), whose held build's charge (`SegBuf::take_charge`) the
    /// cache's takes over: the bytes are the holder's since their allocation, nothing is committed twice.
    #[inline]
    fn charge_seg(&self, off: u64, n: u64, seg: &SegBuf) {
        self.charge_taken(off, n, seg.take_charge());
    }

    /// `n` bytes of an image this thread just made into the cache: its census, its charge (the held build's `taken` joins
    /// it), its value counters.
    fn charge_taken(&self, off: u64, n: u64, taken: Option<memledger::Charge>) {
        self.charge_fixed(off, n, taken, IMAGE_FIXED);
    }

    /// ZONEPRUNE F2 (the line's zone memo, `keep_zone`): `n` bytes the part keeps that are no cached image (no block or
    /// map entry past them), charged as the BASE's `charge` did.
    fn charge(&self, n: u64) {
        self.charge_fixed(0, n, None, 0);
    }

    /// [`Self::charge_taken`] with `fixed` bytes past `n` on the part cache's charge (an image's block and map entry).
    fn charge_fixed(&self, off: u64, n: u64, taken: Option<memledger::Charge>, fixed: u64) {
        self.resident.fetch_add(n, Ordering::Relaxed);
        crate::partcache::count(n, &mut lock(&self.gov));
        // the image this thread just allocated: the cache's from now (a held build's bytes are already: its charge joins),
        // with its block and map entry (D4487 t2)
        {
            let want = n + fixed;
            let mut held = lock(&self.held);
            let got = taken.map_or(0, |t| held.absorb(t));
            if want > got {
                held.commit(want - got);
            } else if got > want {
                // the block's slack past the image (its capacity rounding) back to this thread, as before
                held.free(got - want);
            }
        }
        // every image faulted into the cache: the part cache's miss and its bytes (timed where the read `arm`ed it)
        memledger::value::fault(memledger::Consumer::PartCache, self.value_key(), n);
        // cache plan A6: and the bytes it read back, at the key the image's door looks it up by (file offset `off`)
        memledger::value::reloaded(memledger::Consumer::PartCache, self.value_key() ^ off.rotate_left(32), n);
        if self.owed.load(Ordering::Relaxed) != 0 {
            if let Ok(owed) = self.owed.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |o| (o != 0).then(|| o.saturating_sub(n))) {
                memledger::value::re_miss(memledger::Consumer::PartCache, owed.min(n));
            }
        }
        if self.evicted_at.load(Ordering::Relaxed) != 0 {
            let at = self.evicted_at.swap(0, Ordering::Relaxed);
            if at != 0 {
                memledger::value::ghost(memledger::Consumer::PartCache, at, 1);
            }
        }
    }

    /// Broker (b): `bytes` of this part's contents evicted: the part cache's byte clock moves and the part keeps the clock
    /// as its ghost (switched on by `memledger::value::ext`).
    #[inline]
    fn ghost_mark(&self, bytes: u64) {
        if bytes != 0 && memledger::value::ext() {
            self.evicted_at.store(memledger::value::evicted(memledger::Consumer::PartCache, bytes), Ordering::Relaxed);
        }
    }

    /// The word this part's faults are timed by (memledger::value): its address.
    #[inline]
    pub(crate) fn value_key(&self) -> u64 {
        self as *const OpenPart as usize as u64
    }

    /// A lookup the part's cache served.
    #[inline]
    fn served() {
        memledger::value::hit(memledger::Consumer::PartCache);
    }

    /// A lookup the part's cache did not serve, its read and rebuild about to run: timed when sampled, to its `charge`.
    #[inline]
    fn arm(&self) {
        memledger::value::arm(memledger::Consumer::PartCache, self.value_key());
    }

    /// `bytes` a content eviction freed: owed until faults bring them back.
    #[inline]
    fn owe(&self, bytes: u64) {
        if bytes != 0 {
            self.owed.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    /// The line's D3113 eviction class on the census: `bytes` of `images` cached images left the part's maps, `owed` of
    /// them another holder keeps (counted until its last drop repays them, `Chunk::drop`); the rest leave the census and
    /// the part's charge now, with every image's block and map entry (trunk D4487 t2, as [`Self::debit`]). D4834: an owed
    /// image's bytes and block go into the image instead (`SegBuf::carry_owed`); its map entry leaves now.
    fn debit_owed(&self, bytes: u64, owed: u64, images: u64, freed: &[(SegBuf, bool)]) {
        self.resident.fetch_sub(bytes, Ordering::Relaxed);
        let sole = bytes.saturating_sub(owed);
        if sole > 0 {
            crate::partcache::uncount(sole, &mut lock(&self.gov));
        }
        let mut held = lock(&self.held);
        let mut kept = 0u64;
        // bounded("other: the images one eviction took, their walk polled in `evict_images`")
        for (b, _) in freed.iter().filter(|f| f.1) {
            b.carry_owed(held.split_off(b.len() as u64 + IMG_BLOCK));
            kept += 1;
        }
        held.free(sole + images * IMAGE_FIXED - kept * IMG_BLOCK);
    }

    /// `n` bytes of `images` cached images out of the cache.
    #[inline]
    fn debit(&self, n: u64, images: u64) {
        self.resident.fetch_sub(n, Ordering::Relaxed);
        crate::partcache::uncount(n, &mut lock(&self.gov));
        // the images this thread frees now (an eviction's sole copies) or holds from now (a staged image taken), their
        // blocks and map entries with them (D4487 t2)
        lock(&self.held).free(n + images * IMAGE_FIXED);
    }

    /// D4487 t2: `n` passed-extent entries listed (`add`) or gone: their bytes onto or off the part cache's charge.
    fn passed_entries(&self, n: u64, add: bool) {
        if n == 0 {
            return;
        }
        let mut held = lock(&self.held);
        if add {
            held.commit(n * PASSED_FIXED);
        } else {
            held.free(n * PASSED_FIXED);
        }
    }

    /// D3003: a use of this part (see [`PartUse`]) by a statement without a re-use token.
    pub fn use_part(self: &Arc<Self>) -> PartUse {
        self.use_part_in(0)
    }

    /// D3003 + memory step 6a: a use of this part by the statement whose re-use token is `token`
    /// (`partcache::next_stmt_token`; 0 = unknown). While the broker runs, a use under a new token shifts the part's
    /// latest statement second into the one before (`reuse_secs`); under off this costs one relaxed load.
    pub fn use_part_in(self: &Arc<Self>, token: u64) -> PartUse {
        self.take_use(token, None)
    }

    /// C8: [`OpenPart::use_part_in`] by a taker that gives the part back as its claims leave it (a scan's per-part
    /// release): its context's owner still, released by row.
    pub fn use_part_by_row(self: &Arc<Self>, token: u64) -> PartUse {
        self.take_use(token, Some(memledger::Release::Row))
    }

    #[inline]
    fn take_use(self: &Arc<Self>, token: u64, by: Option<memledger::Release>) -> PartUse {
        self.users.fetch_add(1, Ordering::Relaxed);
        if memledger::value::ext() {
            // broker (b)'s loss signal: a use of a resident part in the cache's coldest tenth (its stamp before this use)
            let (was, tail) = (self.cache_used.load(Ordering::Relaxed), crate::partcache::tail_stamp());
            if tail != 0 && was != 0 && was <= tail && self.resident() != 0 {
                memledger::value::tail_hit(memledger::Consumer::PartCache);
            }
        }
        // C8: the use is its taker's (the portal, statement or session the calling thread works for)
        let (owner, release) = memledger::charge::context();
        let release = by.unwrap_or(release);
        let key = user_key(owner);
        {
            let mut u = lock(&self.user_accts);
            let _pc = self.homed();
            match u.iter_mut().find(|e| (e.0, e.1) == key) {
                Some(e) => e.2 += 1,
                None => u.push((key.0, key.1, 1)),
            }
        }
        let first = crate::partcache::stamp(&self.cache_used);
        let now = crate::partcache::now_sec();
        if now != 0 {
            self.note_reuse(token, first, now);
        }
        PartUse { part: Arc::clone(self), charge: memledger::Charge::use_of(owner, release), parked: false, deferred: None }
    }

    pub(crate) fn note_reuse(&self, token: u64, first_in_epoch: bool, now: u32) {
        let secs = self.reuse_secs.load(Ordering::Relaxed);
        let last = secs as u32;
        let new_statement = if token != 0 {
            let had = self.reuse_stmt.load(Ordering::Relaxed);
            had != token && self.reuse_stmt.compare_exchange(had, token, Ordering::Relaxed, Ordering::Relaxed).is_ok()
        } else {
            first_in_epoch && now.saturating_sub(last) >= REUSE_FALLBACK_S
        };
        if new_statement {
            let shifted = (u64::from(last) << 32) | u64::from(now);
            if self.reuse_secs.compare_exchange(secs, shifted, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                self.last_evicted.store(0, Ordering::Relaxed);
            }
        }
    }

    /// Memory step 6a: the broker seconds of the part's latest statement use and the one before (0 = none).
    pub fn reuse_secs(&self) -> (u32, u32) {
        let s = self.reuse_secs.load(Ordering::Relaxed);
        (s as u32, (s >> 32) as u32)
    }

    /// Memory step 6a: the part's NEED: max(its cached bytes, the most an eviction freed since its latest statement).
    pub fn need_bytes(&self) -> u64 {
        self.cache_bytes().max(self.last_evicted.load(Ordering::Relaxed))
    }

    /// D3109: (attno, path_ord) is opened in the current statement epoch (a cursor or a dictionary, never per row).
    pub(crate) fn note_stream_use(&self, attno: u32, path_ord: u32) {
        let e = crate::partcache::epoch();
        let now = crate::partcache::now_sec();
        let mut st = lock(&self.state);
        let _pc = self.homed();
        st.stream_epochs.insert((attno, path_ord), e);
        if now != 0 {
            st.stream_secs.insert((attno, path_ord), now);
        }
    }

    /// Memory step 6a: the file bytes of the streams opened at or after broker second `since` (the page-cache floor's
    /// signal: what the OS must keep cached for the part's re-reads to stay off the disk).
    pub fn stream_file_bytes_since(&self, since: u32) -> u64 {
        let st = lock(&self.state);
        let Some(dir) = st.stream_dir.as_ref() else { return 0 };
        dir.streams()
            .iter()
            .filter(|s| st.stream_secs.get(&(s.entry.attno, s.entry.path_ord)).is_some_and(|&t| t >= since))
            .flat_map(|s| s.extents.iter().map(|r| r.len))
            .sum()
    }

    /// Memory step 6a: an eviction freed `freed` bytes of this part (while the broker runs): its NEED keeps them, and a part
    /// re-used inside the widest window stamps the cache's re-used eviction second (the policy's no-shrink signal).
    fn note_evicted(&self, freed: u64) {
        let now = crate::partcache::now_sec();
        if now == 0 || freed == 0 {
            return;
        }
        self.last_evicted.fetch_max(freed, Ordering::Relaxed);
        let (_, prev) = self.reuse_secs();
        if prev != 0 && now.saturating_sub(prev) <= crate::partcache::REUSE_WINDOW_S {
            crate::partcache::note_reused_evict(now);
        }
    }

    /// D3109: evict the cached extents (segments, unwrapped, staged; never the part-grain dictionary images, so no evicted
    /// hook) of the streams the part's last statement did not open: other statements' columns, which whole-part eviction
    /// keeps for as long as the part itself stays in use. A stream never opened through a cursor or a dictionary is not
    /// stale. Cache-only ([`Self::evict_extent_images`]). Returns the bytes freed: an image another holder keeps leaves
    /// the part but stays charged until that holder drops it (D3113, the line's class), so it frees nothing here.
    pub fn evict_stale_streams(&self) -> u64 {
        let last = crate::partcache::epoch_of(self.cache_used());
        let keys: Vec<(u64, u64)> = {
            // D4398: on the memory ladder's pass, nothing while another thread holds the part
            let Some(st) = memledger::ladder_pass::lock_or_skip(&self.state) else { return 0 };
            let Some(dir) = st.stream_dir.clone() else { return 0 };
            dir.streams()
                .iter()
                .filter(|s| st.stream_epochs.get(&(s.entry.attno, s.entry.path_ord)).is_some_and(|&e| e < last))
                .flat_map(|s| s.extents.iter().map(|r| (r.file_off, r.len)))
                .filter(|k| st.segs.contains_key(k) || st.unwrapped.contains_key(k) || st.staged.contains_key(k))
                .collect()
        };
        if keys.is_empty() {
            return 0;
        }
        let (_, bytes, owed) = self.evict_images(&keys);
        bytes - owed
    }

    /// D4315: a scan's cursor read past these extents (segment or unwrapped image keys) of its stream `stream`: the cached
    /// images stay, and a content trim may evict them while the part is in use ([`Self::evict_passed`]); what a reader
    /// keeps a clone of stays (the [`take_sole`] rule). Keys not cached are skipped. D4351: `at` is the passing reader's
    /// position (its id and the granule it reads), published under the same lock. Returns the bytes newly marked.
    pub fn pass_extents(&self, stream: StreamTag, at: Option<(u32, u32)>, keys: &[PassKey]) -> u64 {
        let mut added = 0u64;
        let mut listed = 0u64;
        {
            let mut g = lock(&self.state);
            let st = &mut *g;
            let _pc = self.homed();
            if let Some((id, low)) = at {
                set_low(&mut st.readers, id, low);
            }
            // bounded("other: the extents one cursor read past, at most one stream's extents of one part")
            for k in keys {
                let Some(len) = st.unwrapped.get(&k.key).or_else(|| st.segs.get(&k.key)).map(|b| b.len() as u64) else { continue };
                if let std::collections::btree_map::Entry::Vacant(e) = st.passed.entry(k.key) {
                    e.insert(Passed { len, stream, g_end: k.g_end }); // alloc: Exec
                    added += len;
                    listed += 1;
                }
            }
        }
        self.passed_entries(listed, true);
        if added > 0 {
            self.passed_bytes.fetch_add(added, Ordering::Relaxed);
            crate::partcache::note_passed(added);
        }
        added
    }

    /// D4351: a cursor that passes behind it reads stream `stream` from granule 0: it registers as a live reader, so the
    /// trim's first tier keeps the passed extents it has yet to reach. Returns its id (never 0).
    pub fn register_reader(&self, stream: StreamTag) -> u32 {
        let mut g = lock(&self.state);
        let st = &mut *g;
        st.next_reader = st.next_reader.wrapping_add(1).max(1);
        let id = st.next_reader;
        let cap = st.readers.capacity();
        st.readers.push(LiveReader { id, stream, low: 0 }); // alloc: Exec
        // D4487 t2: the list keeps its most readers' room for the part's life (a deregister keeps the capacity): its
        // growth is the part cache's with the part's other bytes
        let grown = (st.readers.capacity() - cap) * core::mem::size_of::<LiveReader>();
        drop(g);
        if grown > 0 {
            lock(&self.held).commit(grown as u64);
        }
        id
    }

    /// D4351: live reader `id` now reads granule `low` (it reads nothing below it again).
    pub fn set_reader_low(&self, id: u32, low: u32) {
        set_low(&mut lock(&self.state).readers, id, low);
    }

    /// D4351: live reader `id` reads no more (its cursor dropped, or stopped passing behind).
    pub fn deregister_reader(&self, id: u32) {
        let mut g = lock(&self.state);
        if let Some(i) = g.readers.iter().position(|r| r.id == id) {
            g.readers.swap_remove(i);
        }
    }

    /// D4351: the live readers of the part (a witness).
    pub fn live_readers(&self) -> usize {
        lock(&self.state).readers.len()
    }

    /// D4315: a reader loads extent `key` again: it is read, not passed (one relaxed load while nothing is passed).
    #[inline]
    pub fn unpass(&self, key: (u64, u64)) {
        if self.passed_bytes.load(Ordering::Relaxed) == 0 {
            return;
        }
        let gone = {
            let _pc = self.homed();
            lock(&self.state).passed.remove(&key)
        };
        if let Some(p) = gone {
            self.passed_bytes.fetch_sub(p.len, Ordering::Relaxed);
            self.passed_entries(1, false);
        }
    }

    /// D4407: the bytes a content trim frees of this part now, by the trim's own tests: as an unused part (`whole`,
    /// [`Self::evict_contents`]) the segment / unwrapped / staged images no other clone holds ([`take_sole`]) and the
    /// part-grain dictionary images no handle shares, none while a read is in flight; as a part in use
    /// ([`Self::evict_passed`]'s tiers) its passed extents' images no other clone holds and no read has in flight. An image a
    /// holder clones frees nothing when the cache drops its copy (D3113). Under the memory ladder's pass, none while another
    /// thread holds the part (as [`Self::cache_bytes`]).
    pub fn trimmable_now(&self, whole: bool) -> u64 {
        let Some(st) = memledger::ladder_pass::lock_or_skip(&self.state) else { return 0 };
        let sole = |b: &SegBuf| if b.is_shared() { 0 } else { b.len() as u64 };
        if !whole {
            let PartState { passed, segs, unwrapped, inflight, .. } = &*st;
            return passed.keys().filter(|k| !inflight.contains(k)).filter_map(|k| unwrapped.get(k).or_else(|| segs.get(k))).map(sole).sum();
        }
        if !st.inflight.is_empty() {
            return 0;
        }
        let images: u64 = st.segs.values().chain(st.unwrapped.values()).chain(st.staged.values()).map(sole).sum();
        let dicts: u64 = st.dict_blocks.values().filter(|d| Arc::strong_count(d) == 1).map(|d| d.resident_bytes()).sum();
        images + dicts
    }

    /// D4315: the cached bytes of the extents a scan read past ([`Self::pass_extents`]).
    pub fn passed_bytes(&self) -> u64 {
        self.passed_bytes.load(Ordering::Relaxed)
    }

    /// D4315: the cached bytes a use of the part pins: [`Self::cache_bytes`] less the passed extents', which a trim may take.
    pub fn pinned_bytes(&self) -> u64 {
        self.cache_bytes().saturating_sub(self.passed_bytes())
    }

    /// D4315: a content trim's pass over a part in use: evict the passed images no clone shares (cache-only, a later read
    /// re-faults CRC-checked as a first read: the [`Self::evict_extent_images`] law), poisoned first under `poison`
    /// (D3823's check), so a view still pointing into one reads 0xDB bytes. A key in flight stays. D4351: the trim's first
    /// tier: only the passed extents no live reader of their stream is still behind (each live reader of the stream reads
    /// at or past the extent's end); [`Self::evict_passed_ahead`] is the second. The bytes freed.
    pub(crate) fn evict_passed(&self, poison: bool) -> u64 {
        self.evict_passed_tier(poison, false)
    }

    /// D4351: the trim's second tier, when the first left the cache over its target: every passed image no clone shares,
    /// those another live reader has yet to reach too (that reader faults them again; give-back is never blocked).
    pub(crate) fn evict_passed_ahead(&self, poison: bool) -> u64 {
        self.evict_passed_tier(poison, true)
    }

    fn evict_passed_tier(&self, poison: bool, ahead: bool) -> u64 {
        if self.passed_bytes() == 0 {
            return 0;
        }
        let mut freed = memledger::ChargedVec::new(Some(crate::partcache::OWNER), memledger::Kind::Region);
        let mut unlisted = 0u64;
        {
            let mut step = cancelpoll::step("pgrc_read evict_passed: a passed image's eviction");
            // D4398: on the memory ladder's pass, nothing while another thread holds the part
            let Some(mut g) = memledger::ladder_pass::lock_or_skip(&self.state) else { return 0 };
            let _pc = self.homed();
            let PartState { passed, segs, unwrapped, inflight, readers, .. } = &mut *g;
            // D4351: each stream's lowest live reader, sorted by stream (one search per passed extent)
            let mut lows: memledger::ChargedVec<(StreamTag, u32)> = memledger::ChargedVec::new(Some(crate::partcache::OWNER), memledger::Kind::Region);
            if !ahead && !readers.is_empty() {
                lows.with_vec(|v| v.extend(readers.iter().map(|r| (r.stream, r.low)))); // alloc: Cold charged crate::partcache::OWNER
                // bounded("other: one part's live readers, a cursor each (sessions x read columns)")
                lows.sort_unstable();
                lows.with_vec(|v| v.dedup_by_key(|e| e.0));
            }
            let mut i = 0usize;
            let listed = passed.len();
            passed.retain(|k, p| {
                i += 1;
                if step.batch(i).is_err() || inflight.contains(k) {
                    return true;
                }
                if !ahead {
                    if let Ok(j) = lows.binary_search_by_key(&p.stream, |e| e.0) {
                        if p.g_end > lows[j].1 {
                            return true;
                        }
                    }
                }
                for m in [&mut *unwrapped, &mut *segs] {
                    let Some(b) = m.get(k) else { continue };
                    if b.is_shared() || freed.try_reserve(1).is_err() {
                        return true;
                    }
                    if let Some(b) = m.remove(k) {
                        let _ = freed.push(b); // alloc: Cold charged crate::partcache::OWNER (reserved above: no growth)
                    }
                    return false;
                }
                false
            });
            self.passed_bytes.store(passed.values().map(|p| p.len).sum(), Ordering::Relaxed);
            unlisted = (listed - passed.len()) as u64;
        }
        self.passed_entries(unlisted, false);
        let bytes: u64 = freed.iter().map(|b| b.len() as u64).sum();
        if bytes > 0 || !freed.is_empty() {
            if poison {
                crate::partcache::note_park_poisoned(freed.iter_mut().map(SegBuf::poison).sum());
            }
            self.debit(bytes, freed.len() as u64);
            self.owe(bytes);
            self.ghost_mark(bytes);
            self.note_evicted(bytes);
        }
        drop(freed);
        bytes
    }

    /// D4554: the kept rows of each of `codes` in column `attno` (`keep_code_counts`) into `out`, when the part keeps every
    /// one; false (and `out` partly written) when it misses one.
    pub fn code_counts(&self, attno: i16, codes: &[u32], out: &mut [u32]) -> bool {
        let c = lock(&self.code_counts);
        // bounded("other: one lookup per asked code over the part's kept counts, at most keep::CODE_COUNTS_KEPT")
        for (i, &code) in codes.iter().enumerate() {
            match c.iter().find(|e| e.0 == attno && e.1 == code) {
                Some(e) => out[i] = e.2,
                None => return false,
            }
        }
        true
    }

    /// D4554: `pairs` (code, rows) of column `attno` kept (`code_counts` answers them from now) while the part's contents
    /// stay cached, up to `mempolicy::keep::CODE_COUNTS_KEPT` entries a part (a code kept already keeps its count: the part never changes).
    /// The list's growth is the part cache's bytes from now (this thread allocated it), charged under the list's lock so
    /// an eviction (`evict`) frees only what was charged.
    pub fn keep_code_counts(&self, attno: i16, pairs: &[(u32, u32)]) {
        let mut c = lock(&self.code_counts);
        let before = c.capacity();
        // bounded("other: the codes one statement's emit asked of this part, at most keep::CODE_COUNTS_KEPT kept")
        for &(code, rows) in pairs {
            if c.len() >= mempolicy::keep::CODE_COUNTS_KEPT {
                break;
            }
            if !c.iter().any(|e| e.0 == attno && e.1 == code) && c.try_reserve(1).is_ok() { // alloc: Cold charged crate::partcache::OWNER (the part's held charge, below)
                c.push((attno, code, rows)); // alloc: Cold charged crate::partcache::OWNER (reserved above: no growth)
            }
        }
        let grown = ((c.capacity() - before) * core::mem::size_of::<(i16, u32, u32)>()) as u64;
        if grown > 0 {
            self.resident.fetch_add(grown, Ordering::Relaxed);
            crate::partcache::count(grown, &mut lock(&self.gov));
            lock(&self.held).commit(grown);
        }
    }

    /// D3003: live uses of the part.
    pub fn users(&self) -> u32 {
        self.users.load(Ordering::Relaxed)
    }

    /// D3994 (B.1 W): the uses a running statement (or a portal, a worker set, a cache) holds: `users` less the parked
    /// executors' ([`PartUse::transfer`] to a session or the depot). A part with none is in nobody's read: the trim may
    /// evict it when every use is parked and the memory policy's gate allows.
    pub fn live_users(&self) -> u32 {
        self.users().saturating_sub(self.parked.load(Ordering::Relaxed))
    }

    /// D4315: the part's floor test: some live claim holds it. Of the live uses (`live_users`), those of the owners whose
    /// statement deferred the part (every claim left it; `PartUse::defer`) read nothing more: such a part is reclaimable
    /// (its statement gives it back at its next due part leave, or at its end), so it no longer lifts the cache's floor and,
    /// under the broker, the target the deferral itself waits on. One load when nothing deferred it.
    pub fn live_claimed(&self) -> bool {
        let live = self.live_users();
        if live == 0 || self.ndeferred.load(Ordering::Acquire) == 0 {
            return live != 0;
        }
        let d = lock(&self.deferred);
        let held: u64 = {
            let u = lock(&self.user_accts);
            u.iter().filter(|e| d.iter().any(|k| (k.0, k.1) == (e.0, e.1))).map(|e| e.2 as u64).sum()
        };
        live as u64 > held
    }

    /// D3999 + C8: each user's (memory ledger account, owner key) and its uses now.
    pub fn user_accounts(&self) -> Vec<(u32, u64, u32)> {
        lock(&self.user_accts).clone()
    }

    /// D4029 (d): `f` over each user's (account, owner key) and its uses, under the part's lock (no copy).
    pub fn with_user_accounts<R>(&self, f: impl FnOnce(&[(u32, u64, u32)]) -> R) -> R {
        f(&lock(&self.user_accts))
    }

    pub(crate) fn cache_used(&self) -> u64 {
        self.cache_used.load(Ordering::Relaxed)
    }

    /// D3003: the part's cached bytes: the segment cache plus its part-grain dictionary images.
    pub fn cache_bytes(&self) -> u64 {
        // D4398: on the memory ladder's pass, none while another thread holds the part (the pass leaves the cache out)
        let Some(st) = memledger::ladder_pass::lock_or_skip(&self.state) else { return 0 };
        self.resident() + st.dict_blocks.values().map(|d| d.resident_bytes()).sum::<u64>()
    }

    /// D4504: the bytes of this part's metadata faces its holders keep ([`FaceHold`]).
    pub fn face_bytes(&self) -> u64 {
        self.face_bytes.load(Ordering::Relaxed)
    }

    /// D4504: the content trim's last tier: this part's faces go (every [`FaceHold`] of it is stale from now, and the faces
    /// hook's owner drops its holds); their bytes leave the census when the last holder (a running statement's) drops.
    /// Returns the bytes let go.
    pub fn evict_faces(&self) -> u64 {
        let b = self.face_bytes.load(Ordering::Relaxed);
        if b > 0 {
            self.face_gen.fetch_add(1, Ordering::AcqRel);
        }
        b
    }

    /// D3003: drop every cached image (segments, unwrapped, staged, part-grain dictionary images): cache-only, a later
    /// read re-faults CRC-checked exactly as a first read (the [`Self::evict_extent_images`] law); holders' `SegBuf` /
    /// `Arc` clones keep their bytes. A part with a read in flight is skipped. The caller checks [`Self::users`] first.
    /// D3113: a segment / unwrapped / staged image another clone holds (a part list's dictionary handle) stays cached
    /// and counted, since dropping the cache's copy would free nothing. Returns `(segment bytes freed, dictionary image
    /// bytes, segment bytes kept shared)` (dictionary images free when their last handle drops).
    pub fn evict_contents(&self) -> (u64, u64, u64) {
        self.evict(false, false).0
    }

    /// D3823 (debug builds and `PGRUST_GRAVITY_RELEASE_POISON=1`, a statement's uses-only release): evict as
    /// [`Self::evict_contents`] does, the segment / unwrapped / staged images no holder shares poisoned with 0xDB bytes
    /// before they free, so a datum still pointing into one (a by-reference value a breaker kept past its window, where
    /// the copy at the retain was missed) reads 0xDB bytes, not the column's. The caller checks [`Self::users`] first.
    /// The bytes poisoned.
    pub fn debug_poison_contents(&self) -> u64 {
        self.evict(true, false).1
    }

    /// D3994 (B.1 W) under D3823's check: [`Self::evict_contents`] of a part only parked executors use, the images it frees
    /// poisoned first ([`Self::debug_poison_contents`]'s fill): a pointer a parked executor kept into one reads 0xDB bytes.
    pub(crate) fn evict_contents_poisoned(&self) -> (u64, u64, u64) {
        let ((s, d, k), poisoned) = self.evict(true, false);
        crate::partcache::note_park_poisoned(poisoned);
        (s, d, k)
    }

    /// D4570: whether the part's file left its relation ([`crate::partcache::drop_parts`]).
    pub fn is_dropped(&self) -> bool {
        self.dropped.load(Ordering::SeqCst)
    }

    /// D4570: mark the part dropped; `false` when it already was (a part several generations carry comes once).
    pub(crate) fn mark_dropped(&self) -> bool {
        !self.dropped.swap(true, Ordering::SeqCst)
    }

    /// D4570: a dropped part's contents and faces go when no running statement uses it: a parked executor's kept cursor
    /// holds none of its images (D3994), so its uses do not keep them, and the images free poisoned under D3823's check.
    /// Cache-only, as the content trim's eviction, but no re-use history: nothing reads the part again. Returns `(bytes
    /// freed, face bytes let go)`; nothing while a running statement uses it (its last running use's end gives them back,
    /// [`Self::running_use_ended`]) or a read is in flight; an image another holder shares stays counted until it frees.
    pub(crate) fn evict_dropped(&self) -> (u64, u64) {
        // the mark, the running uses' ends and these reads are SeqCst: a use that ends as the part drops sees the mark
        // (and gives the contents back itself), or this read sees it gone
        if self.users.load(Ordering::SeqCst).saturating_sub(self.parked.load(Ordering::SeqCst)) != 0 {
            return (0, 0);
        }
        let poison = self.users() != 0 && crate::partcache::park_poison();
        let ((segs, dicts, _), poisoned) = self.evict(poison, true);
        if poisoned > 0 {
            crate::partcache::note_park_poisoned(poisoned);
        }
        (segs + dicts, self.evict_faces())
    }

    /// D4570: a running use of the part ended (its drop, or its move to a parked executor): a dropped part that no running
    /// statement uses any more gives its contents back now. One load while the part is not dropped.
    #[inline]
    fn running_use_ended(&self) {
        if self.is_dropped() && self.users.load(Ordering::SeqCst).saturating_sub(self.parked.load(Ordering::SeqCst)) == 0 {
            let (bytes, _) = self.evict_dropped();
            crate::partcache::note_dropped(0, bytes);
        }
    }

    fn evict(&self, poison: bool, dropped: bool) -> ((u64, u64, u64), u64) {
        let mut unlisted = 0u64;
        let (mut segs, mut unwrapped, mut staged, dicts, kept) = {
            // D4398: on the memory ladder's pass, nothing while another thread holds the part (as a read in flight)
            let Some(mut st) = memledger::ladder_pass::lock_or_skip(&self.state) else { return ((0, 0, 0), 0) };
            if !st.inflight.is_empty() {
                return ((0, 0, 0), 0);
            }
            st.evicted = true;
            let _pc = self.homed();
            let (segs, k1) = st.segs.take_sole();
            let (unwrapped, k2) = st.unwrapped.take_sole();
            let (staged, k3) = st.staged.take_sole();
            if !st.passed.is_empty() {
                let PartState { passed, segs: s, unwrapped: u, .. } = &mut *st;
                let listed = passed.len();
                passed.retain(|k, _| s.contains_key(k) || u.contains_key(k));
                unlisted = (listed - passed.len()) as u64;
                self.passed_bytes.store(passed.values().map(|p| p.len).sum(), Ordering::Relaxed);
            }
            (segs, unwrapped, staged, core::mem::take(&mut st.dict_blocks), k1 + k2 + k3)
        };
        self.passed_entries(unlisted, false);
        let seg_bytes: u64 = segs.values().chain(unwrapped.values()).chain(staged.values()).map(|b| b.len() as u64).sum();
        let images = (segs.len() + unwrapped.len() + staged.len()) as u64;
        if seg_bytes > 0 || images > 0 {
            self.debit(seg_bytes, images);
            if !dropped {
                self.owe(seg_bytes);
            }
        }
        let dict_bytes: u64 = dicts.values().map(|d| d.resident_bytes()).sum();
        // D4554: the kept code counts go with the contents (the census and the holder's charge they took, `debit`)
        {
            let mut c = lock(&self.code_counts);
            let count_bytes = (c.capacity() * core::mem::size_of::<(i16, u32, u32)>()) as u64;
            *c = Vec::new();
            if count_bytes > 0 {
                self.debit(count_bytes, 0);
            }
        }
        if !dropped {
            self.ghost_mark(seg_bytes + dict_bytes);
        }
        let poisoned = if poison { segs.values_mut().chain(unwrapped.values_mut()).chain(staged.values_mut()).map(SegBuf::poison).sum() } else { 0 };
        drop((segs, unwrapped, staged));
        self.drain_nodes(dicts);
        if !dropped {
            self.note_evicted(seg_bytes + dict_bytes);
        }
        ((seg_bytes, dict_bytes, kept), poisoned)
    }

    /// Snapshot of the fault log (the O(streams-touched) witness). D4487: the tests' only (`FaultLog`).
    #[cfg(any(test, loom, feature = "fault-log"))]
    pub fn faults(&self) -> Vec<FaultEntry> {
        lock(&self.state).faults.0.clone()
    }

    /// Cached faults past the fault log's cap (counted, not logged).
    pub fn faults_unlogged(&self) -> u64 {
        lock(&self.state).unlogged
    }

    /// The fault log's cap for refaults (`2 × sections + 64`, C1 L5; `PartState::log_fault`).
    fn fault_cap(&self) -> usize {
        2 * self.sections.len() + 64
    }

    /// The part file's modification time as its io recorded it at open (`None` for in-memory parts): the registry's
    /// check on a hit, now that the fd pool (D2873) may close the part's fd (GENID's rule, crate doc).
    pub(crate) fn io_mtime(&self) -> Option<(i64, i64)> {
        self.io.mtime()
    }

    /// Fault one footer-listed section by table index: CRC-validated against
    /// the section entry, cached, logged once.
    pub fn section_bytes(&self, idx: usize) -> ReadResult<SegBuf> {
        let e = *self
            .sections
            .get(idx)
            .ok_or(ReadError::Format(FormatError::Bounds {
                at: "section index",
            }))?;
        self.fault_range(
            e.off,
            e.len,
            e.crc,
            "listed section",
            FaultTag::ListedSection {
                kind: e.kind,
                attno: e.attno,
                path_ord: e.path_ord,
            },
        )
    }

    /// [`Self::section_bytes`] under [`Residency::Cached`] (C1 L5, `crate::mem`): a hit sets the image's ref bit; a
    /// miss is read, CRC-checked and admitted cold if the cap has room (otherwise only the caller holds it).
    pub fn section_bytes_cached(self: &Arc<Self>, idx: usize) -> ReadResult<SegBuf> {
        let e = *self.sections.get(idx).ok_or(ReadError::Format(FormatError::Bounds { at: "section index" }))?;
        let key = (e.off, e.len);
        if let Some(b) = lock(&self.state).segs.get(&key).map(|b| b.served(Residency::Cached)) {
            return Ok(b);
        }
        let buf = self.section_bytes_uncached(idx)?;
        let tag = FaultTag::ListedSection { kind: e.kind, attno: e.attno, path_ord: e.path_ord };
        Ok(self.admit(false, key, buf, tag))
    }

    /// [`Self::section_bytes`] under [`Residency::Private`] (C1 L5b): a resident image is served unmarked; a miss is
    /// read and CRC-checked for the caller alone (nothing inserted, logged or counted). For metadata a caller copies
    /// at once: `pgrc_am`'s `meta_section_body` (the ANALYZE fold, the planner answers).
    pub fn section_bytes_uncached(&self, idx: usize) -> ReadResult<SegBuf> {
        let e = *self.sections.get(idx).ok_or(ReadError::Format(FormatError::Bounds { at: "section index" }))?;
        if let Some(b) = lock(&self.state).segs.get(&(e.off, e.len)) {
            return Ok(b.clone());
        }
        let at: &'static str = "listed section";
        if e.off.checked_add(e.len).map(|end| end > self.io.len()).unwrap_or(true) {
            return Err(ReadError::Format(FormatError::Bounds { at }));
        }
        let buf = SegBuf::build_for(None, e.len as usize, true, |b| self.io.pread_exact(e.off, b, at))?;
        if crc32c(buf.bytes()) != e.crc {
            return Err(ReadError::Format(FormatError::CrcMismatch { at }));
        }
        Ok(buf)
    }

    /// [`Self::extent_bytes`] under [`Residency::Cached`]: a hit sets the image's ref bit; a miss is read as
    /// [`Self::extent_bytes_uncached`] reads it, then admitted cold if the cap has room. An admitted image is
    /// evicted once nothing but this part holds it.
    pub fn extent_bytes_cached(
        self: &Arc<Self>,
        entry: &StreamEntry,
        rec: &ExtentRecord,
        extent_idx: u32,
    ) -> ReadResult<SegBuf> {
        self.extent_bytes_cached_for(None, entry, rec, extent_idx)
    }

    /// [`Self::extent_bytes_cached`] whose miss is read as `owner`'s ([`Self::extent_bytes_uncached_for`]); an image
    /// admitted becomes the cache's (`admit`), one refused stays the reader's.
    pub(crate) fn extent_bytes_cached_for(
        self: &Arc<Self>,
        owner: ReadOwner,
        entry: &StreamEntry,
        rec: &ExtentRecord,
        extent_idx: u32,
    ) -> ReadResult<SegBuf> {
        if let Some(b) = self.cached_hit(false, entry, rec, extent_idx) {
            return Ok(b);
        }
        let buf = self.extent_bytes_uncached_for(owner, entry, rec, extent_idx)?;
        Ok(self.admit(false, (rec.file_off, rec.len), buf, stream_tag(entry, extent_idx)))
    }

    /// [`Self::unwrapped_extent_bytes`] under [`Residency::Cached`]; the miss is
    /// [`Self::unwrapped_extent_bytes_uncached`]'s read and rebuild.
    pub fn unwrapped_extent_bytes_cached(
        self: &Arc<Self>,
        entry: &StreamEntry,
        rec: &ExtentRecord,
        extent_idx: u32,
        rebuild: &mut dyn FnMut(&[u8]) -> ReadResult<Vec<u8>>,
    ) -> ReadResult<SegBuf> {
        self.unwrapped_extent_bytes_cached_for(None, entry, rec, extent_idx, rebuild)
    }

    /// [`Self::unwrapped_extent_bytes_cached`] whose miss is read and rebuilt as `owner`'s
    /// ([`Self::unwrapped_extent_bytes_uncached_for`]).
    pub(crate) fn unwrapped_extent_bytes_cached_for(
        self: &Arc<Self>,
        owner: ReadOwner,
        entry: &StreamEntry,
        rec: &ExtentRecord,
        extent_idx: u32,
        rebuild: &mut dyn FnMut(&[u8]) -> ReadResult<Vec<u8>>,
    ) -> ReadResult<SegBuf> {
        if let Some(b) = self.cached_hit(true, entry, rec, extent_idx) {
            return Ok(b);
        }
        let buf = self.unwrapped_extent_bytes_uncached_for(owner, entry, rec, extent_idx, rebuild)?;
        Ok(self.admit(true, (rec.file_off, rec.len), buf, stream_tag(entry, extent_idx)))
    }

    fn cached_hit(&self, unwrapped: bool, entry: &StreamEntry, rec: &ExtentRecord, extent_idx: u32) -> Option<SegBuf> {
        let key = (rec.file_off, rec.len);
        let st = lock(&self.state);
        let b = if unwrapped { st.unwrapped.get(&key) } else { st.segs.get(&key) }?.served(Residency::Cached);
        drop(st);
        if STREAM_FAULT_OBS_ENABLED.load(Ordering::Relaxed) != 0 {
            observe_stream_fault(&StreamFaultEvent {
                part: self as *const OpenPart as usize,
                attno: entry.attno,
                path_ord: entry.path_ord,
                role: entry.role,
                extent_idx,
                granule_start: rec.granule_start,
                granule_count: rec.granule_count,
                file_off: rec.file_off,
                len: rec.len,
                resident: true,
                wrapped: unwrapped,
            });
        }
        Some(b)
    }

    /// Admission (`crate::mem`) of an image a Cached face just read, charged as held: make room with no part lock
    /// held, then insert-if-absent (the first insert wins) and register the ring slot. Over the cap the image is
    /// returned held and uninserted; a later reader faults it again.
    fn admit(self: &Arc<Self>, unwrapped: bool, key: (u64, u64), buf: SegBuf, tag: FaultTag) -> SegBuf {
        let room = crate::mem::make_room();
        let mut guard = lock(&self.state);
        let st = &mut *guard;
        let map = if unwrapped { &mut st.unwrapped } else { &mut st.segs };
        if let Some(b) = map.get(&key) {
            return b.served(Residency::Cached);
        }
        if !room {
            return buf;
        }
        map.insert_cached(key, buf.clone());
        if st.faults.len() < self.fault_cap() {
            st.faults.push(FaultEntry { tag, off: key.0, len: key.1 });
        } else {
            st.unlogged += 1;
        }
        // CACHE-CHARGE: the reader's charge the image carried (`build_for`) goes back to this thread; the cache's takes
        // its bytes, block and map entry from now (as `section_bytes_batch_for` drops a kept image's reader charge)
        drop(buf.take_charge());
        self.charge_taken(key.0, buf.len() as u64, None);
        drop(guard);
        let part = Arc::downgrade(self);
        crate::mem::register(Box::new(PartSlot { part, unwrapped, key, addr: buf.addr() }));
        buf
    }

    /// Fault one stream extent (spec §6.4): CRC-validated against the
    /// ExtentRecord, cached, logged once. `entry` supplies the log identity.
    pub fn extent_bytes(
        &self,
        entry: &StreamEntry,
        rec: &ExtentRecord,
        extent_idx: u32,
    ) -> ReadResult<SegBuf> {
        if STREAM_FAULT_OBS_ENABLED.load(Ordering::Relaxed) != 0 {
            let resident = lock(&self.state).segs.contains_key(&(rec.file_off, rec.len));
            observe_stream_fault(&StreamFaultEvent {
                part: self as *const OpenPart as usize,
                attno: entry.attno,
                path_ord: entry.path_ord,
                role: entry.role,
                extent_idx,
                granule_start: rec.granule_start,
                granule_count: rec.granule_count,
                file_off: rec.file_off,
                len: rec.len,
                resident,
                wrapped: false,
            });
        }
        self.fault_range(
            rec.file_off,
            rec.len,
            rec.crc,
            "stream extent",
            FaultTag::StreamExtent {
                attno: entry.attno,
                path_ord: entry.path_ord,
                role: entry.role,
                extent: extent_idx,
            },
        )
    }

    /// [sqe8blk] One WHOLE stream extent, CRC-validated, WITHOUT part-cache
    /// retention — the block-lazy `ensure_all` whole-read (RESULTS-
    /// DENSEDICT §3.1 banked refinement 1). The caller immediately
    /// decompresses the wrapped payload into its own write-once block
    /// image; retaining the raw extent alongside (what `extent_bytes`
    /// does) doubled the dense first-touch memory traffic AND resident
    /// bytes. Cache hit: the cached image is served (already resident —
    /// costs nothing extra, is never evicted here). Miss: registers
    /// in-flight first (the C6 courtesy — a racing prefetch run drops the
    /// extent instead of double-reading; a racing demand faulter waits),
    /// preads + CRC-checks, retires the key WITHOUT inserting. A waiter
    /// that wakes to a still-missing image issues its own read (bounded;
    /// only across distinct handles racing the same extent).
    pub fn extent_bytes_uncached(
        &self,
        entry: &StreamEntry,
        rec: &ExtentRecord,
        extent_idx: u32,
    ) -> ReadResult<SegBuf> {
        self.extent_bytes_uncached_for(None, entry, rec, extent_idx)
    }

    /// [`Self::extent_bytes_uncached`] whose image read here is `owner`'s from its allocation (a cached hit stays the
    /// cache's).
    pub(crate) fn extent_bytes_uncached_for(
        &self,
        owner: ReadOwner,
        entry: &StreamEntry,
        rec: &ExtentRecord,
        extent_idx: u32,
    ) -> ReadResult<SegBuf> {
        let at: &'static str = "stream extent";
        let key = (rec.file_off, rec.len);
        // cache plan C2 / C5: a private read serves a resident image as any other read does: the same door
        memledger::value::lookup(memledger::Consumer::PartCache, self.value_key() ^ rec.file_off.rotate_left(32), rec.len);
        if STREAM_FAULT_OBS_ENABLED.load(Ordering::Relaxed) != 0 {
            let resident = lock(&self.state).segs.contains_key(&key);
            observe_stream_fault(&StreamFaultEvent {
                part: self as *const OpenPart as usize,
                attno: entry.attno,
                path_ord: entry.path_ord,
                role: entry.role,
                extent_idx,
                granule_start: rec.granule_start,
                granule_count: rec.granule_count,
                file_off: rec.file_off,
                len: rec.len,
                resident,
                wrapped: false,
            });
        }
        let mut step = cancelpoll::step("pgrc_read extent_bytes_uncached: a whole extent's read");
        loop {
            let mut st = lock(&self.state);
            if let Some(b) = st.segs.get(&key) {
                return Ok(b.clone());
            }
            if st.inflight.contains(&key) {
                while st.inflight.contains(&key) {
                    st = self.wait_slice(st);
                    step.tick()?;
                }
                continue; // re-check the cache under the same lock
            }
            let _pc = self.homed();
            st.inflight.insert(key);
            break;
        }
        let retire = |this: &OpenPart| {
            let mut st = lock(&this.state);
            let _pc = this.homed();
            st.inflight.remove(&key);
            drop(st);
            this.inflight_cv.notify_all();
        };
        if rec
            .file_off
            .checked_add(rec.len)
            .map(|end| end > self.io.len())
            .unwrap_or(true)
        {
            retire(self);
            return Err(ReadError::Format(FormatError::Bounds { at }));
        }
        let buf = match SegBuf::build_for(owner, rec.len as usize, true, |b| pread_polled(&mut step, &*self.io, rec.file_off, b, at)) {
            Ok(b) => b,
            Err(e) => {
                retire(self);
                return Err(e);
            }
        };
        if crc32c(buf.bytes()) != rec.crc {
            retire(self);
            return Err(ReadError::Format(FormatError::CrcMismatch { at }));
        }
        retire(self);
        Ok(buf)
    }

    /// Fault one WRAPPED stream extent and return its UNWRAPPED (encoded)
    /// section image — the O-CMP-5(a) residency home. Cache hit: the
    /// rebuilt image, no I/O, no decompress (once per part residency). Miss:
    /// bounds-check, read, CRC-validate the WRAPPED bytes against the
    /// ExtentRecord (the wrapper CRC law — CRC over the wrapped bytes),
    /// hand them to `rebuild` (the cursor's unwrapper adjudication +
    /// header cross-witness + unwrap), cache ONLY the rebuilt image, and
    /// account `resident` at the image's real size. The transient wrapped
    /// buffer drops — RAM cost of a wrapped extent = its encoded image.
    /// Racing faulters both rebuild; the first insert wins (equal identity
    /// ⇒ identical bytes, spec §11) and is the one logged.
    pub fn unwrapped_extent_bytes(
        &self,
        entry: &StreamEntry,
        rec: &ExtentRecord,
        extent_idx: u32,
        rebuild: &mut dyn FnMut(&[u8]) -> ReadResult<Vec<u8>>,
    ) -> ReadResult<SegBuf> {
        self.unwrapped_extent_bytes_for(None, entry, rec, extent_idx, rebuild)
    }

    /// [`Self::unwrapped_extent_bytes`] whose transient wrapped bytes are `owner`'s while the image is rebuilt.
    pub(crate) fn unwrapped_extent_bytes_for(
        &self,
        owner: ReadOwner,
        entry: &StreamEntry,
        rec: &ExtentRecord,
        extent_idx: u32,
        rebuild: &mut dyn FnMut(&[u8]) -> ReadResult<Vec<u8>>,
    ) -> ReadResult<SegBuf> {
        let key = (rec.file_off, rec.len);
        memledger::value::lookup(memledger::Consumer::PartCache, self.value_key() ^ rec.file_off.rotate_left(32), rec.len);
        let hit = lock(&self.state).unwrapped.get(&key).map(|b| b.served(Residency::Shared));
        if STREAM_FAULT_OBS_ENABLED.load(Ordering::Relaxed) != 0 {
            observe_stream_fault(&StreamFaultEvent {
                part: self as *const OpenPart as usize,
                attno: entry.attno,
                path_ord: entry.path_ord,
                role: entry.role,
                extent_idx,
                granule_start: rec.granule_start,
                granule_count: rec.granule_count,
                file_off: rec.file_off,
                len: rec.len,
                resident: hit.is_some(),
                wrapped: true,
            });
        }
        if let Some(b) = hit {
            Self::served();
            return Ok(b);
        }
        if self.wait_inflight(key)? {
            if let Some(b) = lock(&self.state).unwrapped.get(&key) {
                Self::served();
                return Ok(b.served(Residency::Shared));
            }
        }
        self.arm();
        // the raw image is `owner`'s from its allocation (`wrapped_raw`), its charge dropped with it
        let raw = self.wrapped_raw(owner, rec)?;
        let (rebuilt, taken) = in_cache_held(|| rebuild(raw.bytes()));
        let rebuilt = rebuilt?;
        drop(raw);
        // the part cache's LOAD GATE: an unwrapped image's size is known only once it is rebuilt; room before it is kept
        crate::partcache::load_gate(rebuilt.len() as u64, crate::partcache::Load::Demand);
        let buf = SegBuf::adopt_held(rebuilt, taken)?;
        let mut st = lock(&self.state);
        if let Some(b) = st.unwrapped.get(&key) {
            return Ok(b.served(Residency::Shared));
        }
        {
            let _pc = self.homed();
            st.unwrapped.insert(key, buf.clone());
            st.log_fault(self.fault_cap(), FaultEntry {
                tag: FaultTag::StreamExtent {
                    attno: entry.attno,
                    path_ord: entry.path_ord,
                    role: entry.role,
                    extent: extent_idx,
                },
                off: rec.file_off,
                len: rec.len,
            });
        }
        self.charge_seg(rec.file_off, buf.len() as u64, &buf);
        Ok(buf)
    }

    /// [`Self::unwrapped_extent_bytes`] WITHOUT residency admission — the
    /// bulk-read face of the row scan (D259; PostgreSQL's BAS_BULKREAD ring:
    /// a sequential scan never grows the shared cache by the relation it
    /// reads). Cache hit (the image is already resident, e.g. a Gravity scan
    /// warmed it): the resident image is served, nothing is read. Miss:
    /// bounds-check, read, CRC-validate the wrapped bytes, `rebuild`, and
    /// hand the image to the caller ONLY — nothing is inserted, logged or
    /// counted in `resident`, so the image lives exactly as long as the
    /// caller holds it. A racing prefetch run of the same key is waited on
    /// first (its image, once inserted, is served).
    pub fn unwrapped_extent_bytes_uncached(
        &self,
        entry: &StreamEntry,
        rec: &ExtentRecord,
        extent_idx: u32,
        rebuild: &mut dyn FnMut(&[u8]) -> ReadResult<Vec<u8>>,
    ) -> ReadResult<SegBuf> {
        self.unwrapped_extent_bytes_uncached_for(None, entry, rec, extent_idx, rebuild)
    }

    pub(crate) fn unwrapped_extent_bytes_uncached_for(
        &self,
        owner: ReadOwner,
        entry: &StreamEntry,
        rec: &ExtentRecord,
        extent_idx: u32,
        rebuild: &mut dyn FnMut(&[u8]) -> ReadResult<Vec<u8>>,
    ) -> ReadResult<SegBuf> {
        let key = (rec.file_off, rec.len);
        // cache plan C2 / C5: as `extent_bytes_uncached_for`
        memledger::value::lookup(memledger::Consumer::PartCache, self.value_key() ^ rec.file_off.rotate_left(32), rec.len);
        let hit = lock(&self.state).unwrapped.get(&key).cloned();
        if STREAM_FAULT_OBS_ENABLED.load(Ordering::Relaxed) != 0 {
            observe_stream_fault(&StreamFaultEvent {
                part: self as *const OpenPart as usize,
                attno: entry.attno,
                path_ord: entry.path_ord,
                role: entry.role,
                extent_idx,
                granule_start: rec.granule_start,
                granule_count: rec.granule_count,
                file_off: rec.file_off,
                len: rec.len,
                resident: hit.is_some(),
                wrapped: true,
            });
        }
        if let Some(b) = hit {
            return Ok(b);
        }
        if self.wait_inflight(key)? {
            if let Some(b) = lock(&self.state).unwrapped.get(&key) {
                return Ok(b.clone());
            }
        }
        let raw = self.wrapped_raw(owner, rec)?;
        let (rebuilt, taken) = charged_for(owner, || rebuild(raw.bytes()));
        let rebuilt = rebuilt?;
        drop(raw);
        if rebuilt.as_ptr() as usize % 8 != 0 {
            // adopt copies it: the rebuild's bytes go back to this thread, where it frees, and the copy takes its own
            drop(taken);
            return SegBuf::copied_for(owner, &rebuilt);
        }
        let taken = at_least(taken, rebuilt.capacity() as u64);
        Ok(SegBuf::adopt(rebuilt)?.carry(taken))
    }

    /// [stack] Raw sub-range pread INSIDE a listed stream extent — the
    /// block-lazy wrapped-payload serving primitive (RESULTS-FMTLAND §B.3).
    /// Bounds-checked against the extent and the file; NOT extent-CRC
    /// validated (a strict sub-range cannot be checked against the extent's
    /// whole-range CRC — integrity rides the per-block codec decode); NOT
    /// cached (the bytes immediately decompress into the caller's
    /// write-once region and drop). When the WHOLE extent is already
    /// resident (a claim-horizon prefetch or an earlier whole fault), the
    /// cached image is sliced instead of re-read; an in-flight whole-extent
    /// read is waited on first (the C6 courtesy, not a second read).
    /// The bytes (the slice's copy or the read's) are the reader's from their allocation: its context
    /// ([`context_owner`]: its portal, else its statement).
    pub fn subrange_bytes(
        &self,
        rec: &ExtentRecord,
        off_in_extent: u64,
        len: u64,
        at: &'static str,
    ) -> ReadResult<SegBuf> {
        self.subrange_bytes_for(context_owner(), rec, off_in_extent, len, at)
    }

    /// [`Self::subrange_bytes`] whose bytes are `owner`'s from their allocation ([`SegBuf::build_for`]).
    pub(crate) fn subrange_bytes_for(
        &self,
        owner: ReadOwner,
        rec: &ExtentRecord,
        off_in_extent: u64,
        len: u64,
        at: &'static str,
    ) -> ReadResult<SegBuf> {
        if off_in_extent
            .checked_add(len)
            .map(|end| end > rec.len)
            .unwrap_or(true)
        {
            return Err(ReadError::Format(FormatError::Bounds { at }));
        }
        let whole_key = (rec.file_off, rec.len);
        memledger::value::lookup(memledger::Consumer::PartCache, self.value_key() ^ rec.file_off.rotate_left(32), rec.len);
        let slice_of = |seg: &SegBuf| -> ReadResult<SegBuf> {
            SegBuf::from_bytes_for(
                owner,
                &seg.bytes()[off_in_extent as usize..(off_in_extent + len) as usize],
            )
        };
        if let Some(b) = lock(&self.state).segs.get(&whole_key).cloned() {
            Self::served();
            return slice_of(&b);
        }
        if self.wait_inflight(whole_key)? {
            if let Some(b) = lock(&self.state).segs.get(&whole_key).cloned() {
                Self::served();
                return slice_of(&b);
            }
        }
        let off = rec.file_off + off_in_extent;
        if off.checked_add(len).map(|end| end > self.io.len()).unwrap_or(true) {
            return Err(ReadError::Format(FormatError::Bounds { at }));
        }
        SegBuf::build_for(owner, len as usize, true, |b| self.io.pread_exact(off, b, at))
    }

    /// [densedict] The PART-GRAIN shared block-lazy image for a wrapped
    /// dict payload extent (the RESULTS-STACK §4.2 re-pose). Get-or-create;
    /// the returned Arc keeps the image (and every published block pointer)
    /// alive for the caller's life even across part-cache churn — the same
    /// generation-stability rooting the handle-grain arm had, now shared by
    /// every handle that reopens the same extent in this part residency.
    pub(crate) fn dict_block_payload(
        &self,
        key: (u64, u64),
    ) -> Arc<crate::dicthandle::BlockLazyPayload> {
        let mut st = lock(&self.state);
        // the payload's block charged inside it (a handle may hold it last), made outside the part cache's account; its
        // node inside it
        let make = || {
            let (p, held) = in_cache_held(|| Arc::new(crate::dicthandle::BlockLazyPayload::new()));
            if let Some(c) = at_least(held, memledger::arc_bytes::<crate::dicthandle::BlockLazyPayload>()) {
                p.hold(c);
            }
            p
        };
        let made = if st.dict_blocks.contains_key(&key) { None } else { Some(make()) };
        let _pc = self.homed();
        // (absent here only when it was absent above: the same state lock)
        let p = st.dict_blocks.entry(key).or_insert_with(|| made.unwrap_or_else(make));
        p.clone()
    }

    /// Advisory WILLNEED over the StreamDir section (the catalog warms every
    /// part's directory at open: hint all, then parse all, so the faults land
    /// on the page cache). False when the directory is already parsed or the
    /// section is missing; never faults, never errors.
    pub fn advise_stream_directory(&self) -> bool {
        if lock(&self.state).stream_dir.is_some() {
            return false;
        }
        let Some(idx) = self.find_section(SectionKind::StreamDir, 0, 0) else { return false };
        let e = self.sections[idx];
        self.io.advise_willneed(e.off, e.len);
        true
    }

    /// The lazily-parsed stream directory (spec §6.2). First call faults the
    /// StreamDir section; later calls are cache hits.
    pub fn stream_directory(&self) -> ReadResult<Arc<StreamDirectory>> {
        if let Some(d) = lock(&self.state).stream_dir.clone() {
            return Ok(d);
        }
        let idx = self
            .find_section(SectionKind::StreamDir, 0, 0)
            .ok_or(ReadError::Format(FormatError::Corrupt {
                at: "missing StreamDir section",
            }))?;
        let bytes = self.section_bytes(idx)?;
        // D4552 (a): the directory is the part's (the part cache's holder), whoever parses it, whoever drops it last: its
        // lists and its `Arc` block charged as they are allocated, the charge inside it
        let (dir, held) = in_cache_held(|| StreamDirectory::parse(bytes.bytes(), self.footer.stream_count).map(Arc::new));
        let dir = dir?;
        if let Some(c) = at_least(held, dir.heap_bytes()) {
            let _ = dir.held.set(c);
        }
        let mut st = lock(&self.state);
        // First parse wins (racing parses produce identical directories —
        // equal identity ⇒ identical bytes).
        if let Some(d) = st.stream_dir.clone() {
            return Ok(d);
        }
        st.stream_dir = Some(dir.clone());
        Ok(dir)
    }

    /// D4804: the part's shred election digest (`streams::shred_digest`), read once from its path table and stream
    /// directory (0 without a path table).
    pub fn shred_digest(&self) -> ReadResult<u64> {
        if let Some(d) = lock(&self.state).shred_digest {
            return Ok(d);
        }
        let d = match crate::streams::read_path_table(self)? {
            Some(paths) => {
                let dir = self.stream_directory()?;
                crate::streams::shred_digest(&paths, &dir)
            }
            None => 0,
        };
        lock(&self.state).shred_digest = Some(d);
        Ok(d)
    }

    /// Eagerly fault EVERY footer-listed section (cache warming / integrity
    /// sweep — also the born-RED gate's "whole-part fault" tooth: the
    /// O(streams-touched) witness must detect this).
    pub fn prefault_all_sections(&self) -> ReadResult<u32> {
        let mut n = 0u32;
        for idx in 0..self.sections.len() {
            self.section_bytes(idx)?;
            n += 1;
        }
        Ok(n)
    }

    /// Fault several footer-listed sections with the fewest positioned
    /// reads: the entries not already resident are sorted by offset and
    /// runs whose gaps are at most `gap` bytes are read once and sliced
    /// (the footer fold's meta sections sit one cluster per column between
    /// that column's streams, so a part's 210 single reads become ~105).
    /// Every slice is CRC-checked against its entry; a slice whose
    /// `keep(idx)` holds enters the segment cache exactly as
    /// [`Self::section_bytes`] would have put it (later readers hit), the
    /// others are returned caller-scoped and never become resident.
    /// Result order = `idxs` order.
    pub fn section_bytes_batch(
        &self,
        idxs: &[usize],
        gap: u64,
        keep: &dyn Fn(usize) -> bool,
    ) -> ReadResult<Vec<SegBuf>> {
        self.section_bytes_batch_for(None, idxs, gap, keep)
    }

    /// [`Self::section_bytes_batch`] whose run reads are `owner`'s (the reading scan's breaker, else its statement).
    pub fn section_bytes_batch_for(
        &self,
        owner: ReadOwner,
        idxs: &[usize],
        gap: u64,
        keep: &dyn Fn(usize) -> bool,
    ) -> ReadResult<Vec<SegBuf>> {
        let bounds = ReadError::Format(FormatError::Bounds {
            at: "section index",
        });
        let mut out: memledger::ChargedVec<Option<SegBuf>> = read_vec(owner);
        out.with_vec(|v| v.try_reserve_exact(idxs.len())).map_err(|_| ReadError::Io { at: "section index reads", errno: libc::ENOMEM })?; // alloc: Exec charged owner
        out.with_vec(|v| v.resize(idxs.len(), None)); // alloc: Exec charged owner
        let mut want: memledger::ChargedVec<(u64, u64, usize)> = read_vec(owner);
        want.try_reserve(idxs.len()).map_err(|_| ReadError::Io { at: "section index reads", errno: libc::ENOMEM })?; // alloc: Exec charged owner
        {
            let st = lock(&self.state);
            // bounded("catalog-sized: the listed sections, a few meta sections per scanned column; one cache lookup each")
            for (pos, &idx) in idxs.iter().enumerate() {
                let e = self.sections.get(idx).ok_or(bounds)?;
                memledger::value::lookup(memledger::Consumer::PartCache, self.value_key() ^ e.off.rotate_left(32), e.len);
                match st.segs.get(&(e.off, e.len)) {
                    Some(b) => {
                        Self::served();
                        out[pos] = Some(b.served(Residency::Shared));
                    }
                    None => want.with_vec(|v| v.push((e.off, e.len, pos))), // alloc: Exec charged owner
                }
            }
        }
        // bounded("catalog-sized: the listed sections not resident, a few meta sections per scanned column")
        want.sort_unstable();
        let file_len = self.io.len();
        let mut step = cancelpoll::step("pgrc_read section_bytes_batch: a run of listed sections");
        let mut i = 0;
        while i < want.len() {
            step.tick()?;
            let start = want[i].0;
            let mut end = start.saturating_add(want[i].1);
            let mut j = i + 1;
            // bounded("catalog-sized: one comparison per listed section, each joins one run once")
            while j < want.len() && want[j].0 <= end.saturating_add(gap) {
                end = end.max(want[j].0.saturating_add(want[j].1));
                j += 1;
            }
            if end > file_len {
                return Err(bounds);
            }
            // the part cache's LOAD GATE: a run of listed sections is read before its need (no wait)
            crate::partcache::load_gate(end - start, crate::partcache::Load::Ahead);
            let mut run: memledger::ChargedVec<u8> = read_vec(owner);
            run.with_vec(|v| *v = vec![0u8; (end - start) as usize]); // alloc: Exec charged owner
            pread_polled(&mut step, &*self.io, start, &mut run, "listed section run")?;
            for &(off, len, pos) in &want[i..j] {
                step.tick()?;
                let idx = idxs[pos];
                let e = self.sections[idx];
                let lo = (off - start) as usize;
                let bytes = &run[lo..lo + len as usize];
                if crc32c(bytes) != e.crc {
                    return Err(ReadError::Format(FormatError::CrcMismatch {
                        at: "listed section",
                    }));
                }
                // CACHE-CHARGE: a kept section's image is the part cache's from its allocation
                let kept = keep(idx);
                let buf = if kept { SegBuf::from_bytes_held(bytes)? } else { SegBuf::from_bytes_for(owner, bytes)? };
                if kept {
                    let mut st = lock(&self.state);
                    if !st.segs.contains_key(&(off, len)) {
                        {
                            let _pc = self.homed();
                            st.segs.insert((off, len), buf.clone());
                            st.log_fault(self.fault_cap(), FaultEntry {
                                tag: FaultTag::ListedSection {
                                    kind: e.kind,
                                    attno: e.attno,
                                    path_ord: e.path_ord,
                                },
                                off,
                                len,
                            });
                        }
                        self.charge_seg(off, len, &buf);
                    } else {
                        // another reader's image stays: this one is the caller's alone, its bytes back on this thread
                        drop(buf.take_charge());
                    }
                }
                out[pos] = Some(buf);
            }
            i = j;
        }
        Ok(out.iter_mut().map(|b| b.take().expect("every position is filled")).collect())
    }

    /// The minted u64 dict epoch for (attno, path_ord) — one per open-part
    /// instance, from the process-global epoch clock (crate doc).
    pub fn dict_epoch(&self, attno: u32, path_ord: u32) -> u64 {
        let mut st = lock(&self.state);
        if let Some(&e) = st.dict_epochs.get(&(attno, path_ord)) {
            return e;
        }
        let e = EPOCH_CLOCK.fetch_add(1, Ordering::Relaxed);
        let _pc = self.homed();
        st.dict_epochs.insert((attno, path_ord), e);
        e
    }

    /// Advisory WILLNEED over a run of stream extents (the cold-readahead
    /// claim-hook face, C3 inherit). Contract:
    ///
    /// - ADVISORY ONLY: never faults, never errors, never changes what a
    ///   later `extent_bytes` returns — a refused hint costs speed, not rows.
    /// - Residency-skipping: extents already in the segment cache (plain or
    ///   unwrapped) are skipped, so hot repetitions issue ~zero syscalls.
    /// - Run-merging: file-contiguous non-resident extents coalesce into one
    ///   hint (extents of one stream ascend by construction, so a granule
    ///   run usually costs one syscall per stream).
    /// - Bounds-clamped against the real file length (a lying record is
    ///   skipped here and reported typed by the real read).
    ///
    /// Returns `(hints_issued, bytes_hinted)` — the caller's witness input
    /// (the no-silent-no-op law).
    pub fn advise_extent_run(&self, recs: &[ExtentRecord]) -> (u64, u64) {
        let file_len = self.io.len();
        let mut hints = 0u64;
        let mut bytes = 0u64;
        let mut run_off = 0u64;
        let mut run_len = 0u64;
        // advisory: a cancel ends the hints, the reads that follow stop on their own steps
        let mut step = cancelpoll::step("pgrc_read advise_extent_run: an extent's hint");
        for r in recs {
            if step.tick().is_err() {
                break;
            }
            let in_bounds = r.len > 0
                && r.file_off
                    .checked_add(r.len)
                    .map(|end| end <= file_len)
                    .unwrap_or(false);
            let resident = in_bounds && {
                let key = (r.file_off, r.len);
                let st = lock(&self.state);
                st.segs.contains_key(&key) || st.unwrapped.contains_key(&key)
            };
            if !in_bounds || resident {
                if run_len > 0 {
                    self.io.advise_willneed(run_off, run_len);
                    hints += 1;
                    bytes += run_len;
                    run_len = 0;
                }
                continue;
            }
            if run_len > 0 && run_off + run_len == r.file_off {
                run_len += r.len;
            } else {
                if run_len > 0 {
                    self.io.advise_willneed(run_off, run_len);
                    hints += 1;
                    bytes += run_len;
                }
                run_off = r.file_off;
                run_len = r.len;
            }
        }
        if run_len > 0 {
            self.io.advise_willneed(run_off, run_len);
            hints += 1;
            bytes += run_len;
        }
        (hints, bytes)
    }

    /// Advisory WILLNEED over the NON-resident extents of `recs` (ascending file order), extents at most `max_hole` apart
    /// coalesced into one range, each range advised in calls of at most `piece` bytes (PERF-COLD): Linux caps one
    /// WILLNEED call at the larger of the device's max I/O and its read_ahead_kb (`force_page_cache_ra`), so one call over a
    /// multi-megabyte run advises only its first piece; per-piece calls put the whole run in the device's queue at once,
    /// and the demand preads that follow find it in the page cache or in flight. Advisory only, like
    /// [`Self::advise_extent_run`]: never faults, never errors, never changes what a later read returns. `wrapped`: the
    /// stream's extents are resident as unwrapped images (else as plain segments). Hot (every extent resident): no syscall.
    /// At most `limit` bytes are advised (the caller's in-flight budget; the ranges past it are left to the demand reads).
    /// Returns `(calls, bytes)` advised.
    pub fn advise_extents_in_pieces<'r>(&self, recs: impl Iterator<Item = &'r ExtentRecord>, wrapped: bool, max_hole: u64, piece: u64, limit: u64) -> (u64, u64) {
        let file_len = self.io.len();
        let piece = piece.max(4096);
        let (mut calls, mut bytes) = (0u64, 0u64);
        // advisory: a cancel ends the hints (the bytes not advised are left out of `bytes`), the reads stop on their own
        let mut step = cancelpoll::step("pgrc_read advise_extents_in_pieces: a run's hints");
        // one coalesced range `[off, end)`, clipped to what is left of `limit`, in `piece`-byte calls; false on a cancel
        let advise = |step: &mut cancelpoll::Step, off: u64, end: u64, calls: &mut u64, bytes: &mut u64| {
            let end = end.min(off.saturating_add(limit.saturating_sub(*bytes)));
            let mut o = off;
            while o < end {
                if step.tick().is_err() {
                    *bytes += o - off;
                    return false;
                }
                let n = (end - o).min(piece);
                self.io.advise_willneed(o, n);
                *calls += 1;
                o += n;
            }
            *bytes += end.saturating_sub(off);
            true
        };
        let (mut lo, mut hi) = (0u64, 0u64);
        for r in recs {
            if bytes >= limit || step.tick().is_err() {
                return (calls, bytes);
            }
            let Some(end) = r.file_off.checked_add(r.len).filter(|&e| r.len > 0 && e <= file_len) else { continue };
            let resident = {
                let key = (r.file_off, r.len);
                let st = lock(&self.state);
                if wrapped { st.unwrapped.contains_key(&key) || st.staged.contains_key(&key) } else { st.segs.contains_key(&key) }
            };
            if resident {
                continue;
            }
            if hi > lo && r.file_off >= hi && r.file_off - hi <= max_hole {
                hi = hi.max(end);
                continue;
            }
            if hi > lo && !advise(&mut step, lo, hi, &mut calls, &mut bytes) {
                return (calls, bytes);
            }
            (lo, hi) = (r.file_off, end);
        }
        if hi > lo && bytes < limit {
            advise(&mut step, lo, hi, &mut calls, &mut bytes);
        }
        (calls, bytes)
    }

    /// Prefetch a run of stream extents with REAL reads into the residency
    /// plane (claim-horizon lane — the advisory WILLNEED face above was
    /// measured NEGATIVE on gp2; this face reads). Contract:
    ///
    /// - Warms EXACTLY the caches the cursor faces consult: unwrapped
    ///   entries (`entry.wrapper != None`, via `rebuild` — the caller
    ///   supplies the same unwrapper adjudication `cursor::load_extent`
    ///   uses) land in the unwrapped-image cache; plain extents land in
    ///   `segs`. A later demand access is a cache hit — byte-identical by
    ///   the insert-if-absent law (equal identity ⇒ identical bytes).
    /// - Coalescing: file-contiguous non-resident extents are read with ONE
    ///   pread of up to `max_run` bytes, then split, CRC-validated and
    ///   inserted per extent.
    /// - NEVER fails the scan: bounds/CRC/IO/rebuild problems skip the
    ///   extent — the demand path surfaces them typed exactly as today.
    /// - Resident/fault accounting matches what the demand faults would
    ///   have recorded.
    ///
    /// Returns `(inserted_extents, inserted_bytes, already_resident,
    /// io_bytes, io_runs)`.
    pub fn prefetch_extent_run(
        &self,
        entry: &StreamEntry,
        recs: &[(u32, ExtentRecord)],
        max_run: u64,
        rebuild: Option<&mut dyn FnMut(&[u8]) -> ReadResult<Vec<u8>>>,
    ) -> (u64, u64, u64, u64, u64) {
        self.prefetch_extent_run_holes(entry, recs, max_run, 0, rebuild, None)
    }

    /// `prefetch_extent_run_holes_roles` over records of `entry`'s own role
    /// (the SQE claim-horizon prefetcher's face; its batch is already a Vec).
    pub fn prefetch_extent_run_holes(
        &self,
        entry: &StreamEntry,
        recs: &[(u32, ExtentRecord)],
        max_run: u64,
        max_hole: u64,
        rebuild: Option<&mut dyn FnMut(&[u8]) -> ReadResult<Vec<u8>>>,
        inserted_log: Option<&mut Vec<(u64, u64)>>,
    ) -> (u64, u64, u64, u64, u64) {
        let roled: Vec<(u32, ExtentRecord, u8)> = recs.iter().map(|(i, r)| (*i, *r, entry.role)).collect(); // alloc: Exec
        self.prefetch_extent_run_holes_roles(entry, &roled, max_run, max_hole, rebuild, inserted_log)
    }

    /// `prefetch_extent_run` with a HOLE LIMIT (the Arrow/DuckDB
    /// bandwidth×latency rule): extents separated by ≤ `max_hole` bytes of
    /// unneeded file are read in ONE pread (the hole bytes are discarded)
    /// when the whole run stays ≤ `max_run`. `max_hole = 0` = strictly
    /// contiguous. Extents in the run are marked IN-FLIGHT for the
    /// duration of the read; a demand fault on one of them waits instead
    /// of re-issuing. `inserted_log`, when given, receives the (file_off,
    /// len) key of every extent THIS call inserted — the caller's
    /// provenance witness ([claimh-cooling]: only prefetch-inserted images
    /// are probationary; already-resident drop-outs must never be evicted).
    /// Each record carries its stream ROLE for the fault tag: a values run
    /// may bring the column's dictionary regions (`StreamCursor::prefetch_granules`).
    pub fn prefetch_extent_run_holes_roles(
        &self,
        entry: &StreamEntry,
        recs: &[(u32, ExtentRecord, u8)],
        max_run: u64,
        max_hole: u64,
        rebuild: Option<&mut dyn FnMut(&[u8]) -> ReadResult<Vec<u8>>>,
        inserted_log: Option<&mut Vec<(u64, u64)>>,
    ) -> (u64, u64, u64, u64, u64) {
        self.prefetch_extent_run_holes_roles_for(None, entry, recs, max_run, max_hole, rebuild, inserted_log)
    }

    /// [`Self::prefetch_extent_run_holes_roles`] whose run reads are `owner`'s (the reading cursor's).
    #[allow(clippy::too_many_arguments, clippy::type_complexity)] // the pub face's parameters, plus the owner
    pub(crate) fn prefetch_extent_run_holes_roles_for(
        &self,
        owner: ReadOwner,
        entry: &StreamEntry,
        recs: &[(u32, ExtentRecord, u8)],
        max_run: u64,
        max_hole: u64,
        mut rebuild: Option<&mut dyn FnMut(&[u8]) -> ReadResult<Vec<u8>>>,
        mut inserted_log: Option<&mut Vec<(u64, u64)>>,
    ) -> (u64, u64, u64, u64, u64) {
        let wrapped = entry.wrapper != 0;
        // a record marked ROLE_STAGE_RAW is another stream's wrapped extent, held raw (`staged`) for its own unwrap
        let raw_of = |role: u8| role & ROLE_STAGE_RAW != 0;
        let section_of = |role: u8| role & ROLE_SECTION != 0;
        if wrapped && rebuild.is_none() && !recs.iter().any(|r| raw_of(r.2) || section_of(r.2)) {
            return (0, 0, 0, 0, 0);
        }
        let file_len = self.io.len();
        let (mut inserted, mut inserted_bytes, mut already, mut io_bytes, mut io_runs) =
            (0u64, 0u64, 0u64, 0u64, 0u64);
        let mut run: memledger::ChargedVec<(u32, ExtentRecord, u8)> = read_vec(owner);
        let mut run_end = 0u64;
        // D4016 / D4065 (C9): a prefetch is advisory, so a cancel ends it with what it read (true from `flush`): the run in
        // hand retires its keys, the demand reads after it stop on their own steps
        let mut step = cancelpoll::step("pgrc_read prefetch_extent_run_holes_roles: a claim run's read");
        let flush = |step: &mut cancelpoll::Step,
                         run: &mut memledger::ChargedVec<(u32, ExtentRecord, u8)>,
                         inserted: &mut u64,
                         inserted_bytes: &mut u64,
                         io_bytes: &mut u64,
                         io_runs: &mut u64,
                         rebuild: &mut Option<&mut dyn FnMut(&[u8]) -> ReadResult<Vec<u8>>>,
                         inserted_log: &mut Option<&mut Vec<(u64, u64)>>| -> bool {
            if run.is_empty() {
                return false;
            }
            let off0 = run[0].1.file_off;
            let last = &run[run.len() - 1].1;
            let total = (last.file_off + last.len - off0) as usize;
            // Register the run in flight (skip keys a demand fault or a
            // racing run already resident/in-flight — those drop out).
            {
                let mut st = lock(&self.state);
                run.with_vec(|v| {
                    v.retain(|(_, r, role)| {
                        let k = (r.file_off, r.len);
                        let present = if raw_of(*role) {
                            st.unwrapped.contains_key(&k) || st.staged.contains_key(&k)
                        } else if wrapped && !section_of(*role) {
                            st.unwrapped.contains_key(&k)
                        } else {
                            st.segs.contains_key(&k)
                        };
                        !present && !st.inflight.contains(&k)
                    })
                });
                let _pc = self.homed();
                // bounded("other: one key per record of the run, which max_run bytes of extents bound; a set insert each")
                for (_, r, _) in run.iter() {
                    st.inflight.insert((r.file_off, r.len));
                }
            }
            if run.is_empty() {
                return false;
            }
            // the part cache's LOAD GATE: a prefetch run asks before it reads (a read ahead never waits)
            crate::partcache::load_gate(total as u64, crate::partcache::Load::Ahead);
            let mut buf: memledger::ChargedVec<u8> = read_vec(owner);
            buf.with_vec(|v| *v = vec![0u8; total]); // alloc: Exec charged owner
            let read = pread_polled(step, &*self.io, off0, &mut buf, "prefetch extent run");
            let retire = |this: &OpenPart, run: &mut memledger::ChargedVec<(u32, ExtentRecord, u8)>| {
                let mut st = lock(&this.state);
                let _pc = this.homed();
                // bounded("other: one key per record of the run, which max_run bytes of extents bound; a set removal each, all of them, so no key stays in flight")
                for (_, r, _) in run.iter() {
                    st.inflight.remove(&(r.file_off, r.len));
                }
                drop(st);
                this.inflight_cv.notify_all();
                run.clear();
            };
            if matches!(read, Err(ReadError::Canceled)) {
                retire(self, run);
                return true;
            }
            let mut stopped = false;
            if read.is_ok() {
                *io_bytes += total as u64;
                *io_runs += 1;
                for (idx, rec, role) in run.iter() {
                    if step.tick().is_err() {
                        stopped = true;
                        break;
                    }
                    let s = (rec.file_off - off0) as usize;
                    let raw = &buf[s..s + rec.len as usize];
                    if crc32c(raw) != rec.crc {
                        continue; // demand read reports this typed
                    }
                    let key = (rec.file_off, rec.len);
                    if raw_of(*role) {
                        let mut st = lock(&self.state);
                        if st.unwrapped.contains_key(&key) || st.staged.contains_key(&key) {
                            continue;
                        }
                        // a prefetch: an image the machine cannot hold now is left to the demand read, which reports it typed
                        let Ok(img) = SegBuf::from_bytes_held(raw) else { continue };
                        let taken = img.take_charge();
                        {
                            let _pc = self.homed();
                            st.staged.insert(key, img);
                        }
                        drop(st);
                        self.charge_taken(rec.file_off, rec.len, taken);
                        STAGED_INSERTED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                        *inserted += 1;
                        *inserted_bytes += rec.len;
                        continue;
                    }
                    if section_of(*role) {
                        let Some(e) = self.sections.get(*idx as usize).copied() else { continue };
                        if (e.off, e.len) != key {
                            continue;
                        }
                        let mut st = lock(&self.state);
                        if st.segs.contains_key(&key) {
                            continue;
                        }
                        let Ok(img) = SegBuf::from_bytes_held(raw) else { continue };
                        let taken = img.take_charge();
                        {
                            let _pc = self.homed();
                            st.segs.insert(key, img);
                            st.log_fault(self.fault_cap(), FaultEntry { tag: FaultTag::ListedSection { kind: e.kind, attno: e.attno, path_ord: e.path_ord }, off: e.off, len: e.len });
                        }
                        drop(st);
                        self.charge_taken(rec.file_off, rec.len, taken);
                        *inserted += 1;
                        *inserted_bytes += rec.len;
                        continue;
                    }
                    let tag = FaultTag::StreamExtent {
                        attno: entry.attno,
                        path_ord: entry.path_ord,
                        role: *role,
                        extent: *idx,
                    };
                    if wrapped {
                        let (rebuilt, taken) = in_cache_held(|| rebuild.as_mut().unwrap()(raw));
                        let rebuilt = match rebuilt {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        // a prefetch: an image the machine cannot hold now is left to the demand read, which reports it typed
                        crate::partcache::load_gate(rebuilt.len() as u64, crate::partcache::Load::Ahead);
                        let Ok(img) = SegBuf::adopt_held(rebuilt, taken) else { continue };
                        let img_len = img.len() as u64;
                        let mut st = lock(&self.state);
                        if st.unwrapped.contains_key(&key) {
                            continue;
                        }
                        let taken = img.take_charge();
                        {
                            let _pc = self.homed();
                            st.unwrapped.insert(key, img);
                            st.log_fault(self.fault_cap(), FaultEntry { tag, off: rec.file_off, len: rec.len });
                        }
                        drop(st);
                        self.charge_taken(rec.file_off, img_len, taken);
                        if let Some(log) = inserted_log.as_mut() {
                            log.push(key);
                        }
                        *inserted += 1;
                        *inserted_bytes += rec.len;
                    } else {
                        let Ok(img) = SegBuf::from_bytes_held(raw) else { continue };
                        let mut st = lock(&self.state);
                        if st.segs.contains_key(&key) {
                            continue;
                        }
                        let taken = img.take_charge();
                        {
                            let _pc = self.homed();
                            st.segs.insert(key, img);
                            st.log_fault(self.fault_cap(), FaultEntry { tag, off: rec.file_off, len: rec.len });
                        }
                        drop(st);
                        self.charge_taken(rec.file_off, rec.len, taken);
                        if let Some(log) = inserted_log.as_mut() {
                            log.push(key);
                        }
                        *inserted += 1;
                        *inserted_bytes += rec.len;
                    }
                }
            }
            retire(self, run);
            stopped
        };
        for (idx, rec, role) in recs {
            if step.tick().is_err() {
                return (inserted, inserted_bytes, already, io_bytes, io_runs);
            }
            let in_bounds = rec.len > 0
                && rec
                    .file_off
                    .checked_add(rec.len)
                    .map(|end| end <= file_len)
                    .unwrap_or(false);
            if !in_bounds {
                if flush(&mut step, &mut run, &mut inserted, &mut inserted_bytes, &mut io_bytes, &mut io_runs, &mut rebuild, &mut inserted_log) {
                    return (inserted, inserted_bytes, already, io_bytes, io_runs);
                }
                run_end = 0;
                continue;
            }
            let key = (rec.file_off, rec.len);
            if !raw_of(*role) && !section_of(*role) && wrapped && rebuild.is_none() {
                continue;
            }
            let resident = {
                let st = lock(&self.state);
                if raw_of(*role) {
                    st.unwrapped.contains_key(&key) || st.staged.contains_key(&key)
                } else if wrapped && !section_of(*role) {
                    st.unwrapped.contains_key(&key)
                } else {
                    st.segs.contains_key(&key)
                }
            };
            if resident {
                already += 1;
                if flush(&mut step, &mut run, &mut inserted, &mut inserted_bytes, &mut io_bytes, &mut io_runs, &mut rebuild, &mut inserted_log) {
                    return (inserted, inserted_bytes, already, io_bytes, io_runs);
                }
                run_end = 0;
                continue;
            }
            let near = !run.is_empty()
                && rec.file_off >= run_end
                && rec.file_off - run_end <= max_hole;
            let fits = run.is_empty()
                || (near && rec.file_off + rec.len - run[0].1.file_off <= max_run);
            if !fits && flush(&mut step, &mut run, &mut inserted, &mut inserted_bytes, &mut io_bytes, &mut io_runs, &mut rebuild, &mut inserted_log) {
                return (inserted, inserted_bytes, already, io_bytes, io_runs);
            }
            run_end = rec.file_off + rec.len;
            run.with_vec(|v| v.push((*idx, *rec, *role))); // alloc: Exec charged owner
        }
        flush(&mut step, &mut run, &mut inserted, &mut inserted_bytes, &mut io_bytes, &mut io_runs, &mut rebuild, &mut inserted_log);
        (inserted, inserted_bytes, already, io_bytes, io_runs)
    }

    /// [claimh-cooling] Evict a set of extent images from the residency
    /// plane (both `segs` and `unwrapped`). Scan-resistant admission
    /// (LeanStore-style cooling mapped onto this plane): prefetch-admitted
    /// images that were never consumed by a demand access are probationary;
    /// the caller (the claim-horizon prefetcher's rep gate) evicts them at
    /// the cold→hot rep boundary so hot reps walk exactly the demand-built
    /// residency set. Eviction is cache-only and can never change what a
    /// read returns — a later demand access simply re-faults, CRC-checked,
    /// exactly as a first access would. Keys currently IN FLIGHT are
    /// skipped (a racing run's insert would resurrect them anyway).
    /// Returns `(evicted_extents, evicted_bytes)`; `resident` is debited
    /// by the evicted image sizes. D3113: an image another holder keeps
    /// goes to held (the chunk census) and stays charged to the part
    /// cache's census and account until its last holder drops it.
    pub fn evict_extent_images(&self, keys: &[(u64, u64)]) -> (u64, u64) {
        let (n, bytes, _) = self.evict_images(keys);
        (n, bytes)
    }

    /// [`Self::evict_extent_images`], and of its bytes the ones still charged (`owed`: another holder keeps them).
    fn evict_images(&self, keys: &[(u64, u64)]) -> (u64, u64, u64) {
        let mut n = 0u64;
        let mut bytes = 0u64;
        // D3113: of `bytes`, the images another holder keeps: they leave this part's resident now, and the part cache's
        // census (its account, the memory governor's) at their last holder's drop
        let mut owed = 0u64;
        let mut unlisted = 0u64;
        // D4595: the evicted images drop after the debit, as `evict_passed_tier`'s do: the cache's charge hands their bytes
        // to this thread first, then their frees land on it. Freed in place before the debit, a large image's free folded
        // this thread's row below zero on its own (the ledger floor credited UNBOUND, and the debit then left the bytes on
        // this thread's row for its session).
        let mut freed = memledger::ChargedVec::new(Some(crate::partcache::OWNER), memledger::Kind::Region);
        {
            // cache-only: a cancel leaves the rest resident, as a later pass would find them
            let mut step = cancelpoll::step("pgrc_read evict_extent_images: a probationary image's eviction");
            // D4398: on the memory ladder's pass, nothing while another thread holds the part
            let Some(mut g) = memledger::ladder_pass::lock_or_skip(&self.state) else { return (0, 0, 0) };
            let _pc = self.homed();
            let st = &mut *g;
            for (i, k) in keys.iter().enumerate() {
                if step.batch(i).is_err() {
                    break;
                }
                if st.inflight.contains(k) || freed.try_reserve(1).is_err() {
                    continue;
                }
                let b = if let Some(b) = st.segs.remove(k) {
                    b
                } else if let Some(b) = st.unwrapped.remove(k) {
                    b
                } else if let Some(b) = st.staged.remove(k) {
                    b
                } else {
                    continue;
                };
                if let Some(p) = st.passed.remove(k) {
                    self.passed_bytes.fetch_sub(p.len, Ordering::Relaxed);
                    unlisted += 1;
                }
                n += 1;
                bytes += b.len() as u64;
                let shared = b.owe_if_shared();
                if shared {
                    owed += b.len() as u64;
                }
                let _ = freed.push((b, shared)); // alloc: Cold charged crate::partcache::OWNER (reserved above: no growth)
            }
            if n > 0 {
                st.evicted = true;
            }
        }
        self.passed_entries(unlisted, false);
        if bytes > 0 || n > 0 {
            self.debit_owed(bytes, owed, n, &freed);
            self.owe(bytes);
            self.ghost_mark(bytes);
        }
        drop(freed);
        (n, bytes, owed)
    }

    /// A wrapped extent's raw bytes: the ones a run staged (taken: an image is rebuilt from them once), else one read,
    /// CRC-checked against the record (the wrapper CRC law) either way. The raw image is a transient (it drops once its
    /// image is rebuilt) and `owner`'s while it lives: a read's from its allocation ([`SegBuf::build_for`]), a staged one's
    /// from the cache's debit that hands it to this thread. A read no owner names (a cursor no statement owns: a part's
    /// shared dict handle) charges this thread's context ([`context_owner`]): the transient never outlives the call.
    fn wrapped_raw(&self, owner: ReadOwner, rec: &ExtentRecord) -> ReadResult<SegBuf> {
        let owner = owner.or_else(context_owner);
        let at: &'static str = "wrapped stream extent";
        let key = (rec.file_off, rec.len);
        let staged = {
            let _pc = self.homed();
            lock(&self.state).staged.remove(&key)
        };
        if let Some(raw) = staged {
            self.debit(rec.len, 1);
            STAGED_TAKEN.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if crc32c(raw.bytes()) == rec.crc {
                let c = read_charge(owner, raw.cap_bytes());
                return Ok(raw.carry(Some(c)));
            }
        }
        if rec
            .file_off
            .checked_add(rec.len)
            .map(|end| end > self.io.len())
            .unwrap_or(true)
        {
            return Err(ReadError::Format(FormatError::Bounds { at }));
        }
        let mut step = cancelpoll::step("pgrc_read wrapped_raw: a wrapped extent's read");
        let raw = SegBuf::build_for(owner, rec.len as usize, true, |b| pread_polled(&mut step, &*self.io, rec.file_off, b, at))?;
        if crc32c(raw.bytes()) != rec.crc {
            return Err(ReadError::Format(FormatError::CrcMismatch { at }));
        }
        Ok(raw)
    }

    /// Footer-listed section `idx` as a run record (`ROLE_SECTION`): its offset, length and CRC, `role` its flag.
    pub fn section_run_record(&self, idx: usize) -> Option<(u32, ExtentRecord, u8)> {
        let e = self.sections.get(idx)?;
        let rec = ExtentRecord { file_off: e.off, len: e.len, values: 0, granule_start: 0, granule_count: 0, crc: e.crc, flags: 0 };
        Some((idx as u32, rec, ROLE_SECTION))
    }

    /// Extents staged raw and not taken yet (M2-ZONEREAD card C's witness).
    pub fn staged_extents(&self) -> usize {
        lock(&self.state).staged.len()
    }

    /// [claim-horizon] Block while a prefetch run holds `key` in flight
    /// (returns immediately when it does not). Called by the demand
    /// faces before issuing their own read; the cache lookup that follows
    /// finds the prefetched image. Bounded: an in-flight run always
    /// retires its keys (success or skip). C9: the wait goes in slices, a
    /// tick between two, so a cancel ends it (`Err`).
    fn wait_inflight(&self, key: (u64, u64)) -> ReadResult<bool> {
        let mut st = lock(&self.state);
        if !st.inflight.contains(&key) {
            return Ok(false);
        }
        let mut step = cancelpoll::step("pgrc_read wait_inflight: a demand read's wait on a run in flight");
        while st.inflight.contains(&key) {
            st = self.wait_slice(st);
            step.tick()?;
        }
        Ok(true)
    }

    /// One slice of a wait on `inflight_cv` ([`INFLIGHT_SLICE`]): the waiter ticks its step between slices.
    fn wait_slice<'a>(&self, st: pgsync::MutexGuard<'a, PartState>) -> pgsync::MutexGuard<'a, PartState> {
        self.inflight_cv.wait_timeout(st, INFLIGHT_SLICE).map(|(g, _)| g).unwrap_or_else(|e| e.into_inner().0)
    }

    /// The shared fault engine: cache lookup → (outside the lock) bounded
    /// read + CRC check → insert-if-absent. Racing faulters both read; the
    /// first insert wins and is the one logged — sound because equal
    /// identity ⇒ identical bytes (spec §11); the loser's buffer drops.
    fn fault_range(
        &self,
        off: u64,
        len: u64,
        crc: u32,
        at: &'static str,
        tag: FaultTag,
    ) -> ReadResult<SegBuf> {
        let key = (off, len);
        memledger::value::lookup(memledger::Consumer::PartCache, self.value_key() ^ off.rotate_left(32), len);
        let mut step = cancelpoll::step("pgrc_read fault_range: a section or region's read");
        // [cold2] Demand faults REGISTER IN FLIGHT before reading (the
        // prefetch runs always did): a racing prefetch run now drops the
        // extent from its run (`retain(!inflight)`) instead of reading it a
        // second time, and a racing demand faulter waits instead of
        // re-issuing. Measured on the 100m bank: single-extent dict payload
        // regions consumed part-parallel (VerdictWords, hash combines) were
        // read TWICE — once by the worker, once by the chained issuer —
        // pf_wasted ≈ the payload size and R_MBps at half the ceiling.
        if !demand_inflight() {
            // Kill-switch arm (PGRUST_PGRC_DEMAND_INFLIGHT=0): the pre-fix
            // race — read without registering; first insert wins.
            if let Some(b) = lock(&self.state).segs.get(&key) {
                Self::served();
                return Ok(b.served(Residency::Shared));
            }
            if self.wait_inflight(key)? {
                if let Some(b) = lock(&self.state).segs.get(&key) {
                    Self::served();
                    return Ok(b.served(Residency::Shared));
                }
            }
            self.arm();
            if off.checked_add(len).map(|end| end > self.io.len()).unwrap_or(true) {
                return Err(ReadError::Format(FormatError::Bounds { at }));
            }
            crate::partcache::load_gate(len, crate::partcache::Load::Demand);
            let buf = SegBuf::build_held(len as usize, true, |b| pread_polled(&mut step, &*self.io, off, b, at))?;
            if crc32c(buf.bytes()) != crc {
                return Err(ReadError::Format(FormatError::CrcMismatch { at }));
            }
            let mut st = lock(&self.state);
            if let Some(b) = st.segs.get(&key) {
                return Ok(b.served(Residency::Shared));
            }
            {
                let _pc = self.homed();
                st.segs.insert(key, buf.clone());
                st.log_fault(self.fault_cap(), FaultEntry { tag, off, len });
            }
            self.charge_seg(off, len, &buf);
            return Ok(buf);
        }
        loop {
            let mut st = lock(&self.state);
            if let Some(b) = st.segs.get(&key) {
                Self::served();
                return Ok(b.served(Residency::Shared));
            }
            if st.inflight.contains(&key) {
                while st.inflight.contains(&key) {
                    st = self.wait_slice(st);
                    step.tick()?;
                }
                continue; // re-check the cache under the same lock
            }
            let _pc = self.homed();
            st.inflight.insert(key);
            break;
        }
        let retire = |this: &OpenPart| {
            let mut st = lock(&this.state);
            let _pc = this.homed();
            st.inflight.remove(&key);
            drop(st);
            this.inflight_cv.notify_all();
        };
        self.arm();
        if off
            .checked_add(len)
            .map(|end| end > self.io.len())
            .unwrap_or(true)
        {
            retire(self);
            return Err(ReadError::Format(FormatError::Bounds { at }));
        }
        // the part cache's LOAD GATE: room for the image before it is read (the key stays in flight across a wait)
        crate::partcache::load_gate(len, crate::partcache::Load::Demand);
        let buf = match SegBuf::build_held(len as usize, true, |b| pread_polled(&mut step, &*self.io, off, b, at)) {
            Ok(b) => b,
            Err(e) => {
                retire(self);
                return Err(e);
            }
        };
        if crc32c(buf.bytes()) != crc {
            retire(self);
            return Err(ReadError::Format(FormatError::CrcMismatch { at }));
        }
        let mut st = lock(&self.state);
        if let Some(b) = st.segs.get(&key).map(|b| b.served(Residency::Shared)) {
            {
                let _pc = self.homed();
                st.inflight.remove(&key);
            }
            drop(st);
            self.inflight_cv.notify_all();
            return Ok(b);
        }
        {
            let _pc = self.homed();
            st.segs.insert(key, buf.clone());
            st.log_fault(self.fault_cap(), FaultEntry { tag, off, len });
            st.inflight.remove(&key);
        }
        drop(st);
        self.inflight_cv.notify_all();
        self.charge_seg(off, len, &buf);
        Ok(buf)
    }
}

/// [cold2] kill switch for demand-fault in-flight registration
/// (`PGRUST_PGRC_DEMAND_INFLIGHT=0` = the pre-fix racing arm). Read once.
fn demand_inflight() -> bool {
    static F: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *F.get_or_init(|| !matches!(std::env::var("PGRUST_PGRC_DEMAND_INFLIGHT").as_deref(), Ok("0")))
}

/// Poison recovery: a panicked holder cannot corrupt this state (buffers are
/// write-once CRC-validated bytes; maps are insert-only), so continue.
fn lock<T>(m: &Mutex<T>) -> pgsync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// Decode the 16 tail bytes (spec §5.4) against the real file length —
/// mirrors `PartTail::decode_at_eof`, which wants the whole file image this
/// crate must never read.
fn decode_tail(buf: &[u8; PART_TAIL_LEN], file_len: u64) -> ReadResult<u64> {
    let mut c = Cur::new(buf);
    let footer_off = c.u64("PartTail")?;
    let footer_len = c.u32("PartTail")?;
    let magic = c.u32("PartTail")?;
    if magic != TAIL_MAGIC {
        return Err(ReadError::Format(FormatError::BadMagic { at: "PartTail" }));
    }
    if footer_len != FOOTER_FIXED_LEN as u32 {
        return Err(ReadError::Format(FormatError::Corrupt {
            at: "PartTail footer_len",
        }));
    }
    if footer_off
        .checked_add((FOOTER_FIXED_LEN + PART_TAIL_LEN) as u64)
        .map(|end| end > file_len)
        .unwrap_or(true)
    {
        return Err(ReadError::Format(FormatError::Bounds {
            at: "PartTail footer_off",
        }));
    }
    Ok(footer_off)
}

/// Structural layout validation of the section table (spec §5.2/§1): every
/// section inside the file, past the header, and no two sections (or the
/// footer/tail/table regions) overlap.
fn validate_section_layout(
    sections: &[SectionEntry],
    file_len: u64,
    footer_off: u64,
    st_off: u64,
    st_len: u64,
) -> ReadResult<()> {
    let mut ranges: Vec<(u64, u64)> = Vec::with_capacity(sections.len() + 2);
    // bounded("catalog-sized: one range per section entry, the part's streams and their meta sections")
    for e in sections {
        let end = e
            .off
            .checked_add(e.len)
            .ok_or(ReadError::Format(FormatError::Bounds {
                at: "section range",
            }))?;
        if e.off < PART_HEADER_LEN as u64 || end > file_len {
            return Err(ReadError::Format(FormatError::Bounds {
                at: "section range",
            }));
        }
        ranges.push((e.off, end));
    }
    ranges.push((st_off, st_off + st_len));
    ranges.push((
        footer_off,
        footer_off + (FOOTER_FIXED_LEN + PART_TAIL_LEN) as u64,
    ));
    // bounded("catalog-sized: the section ranges above plus two, one per section entry")
    ranges.sort_unstable();
    // bounded("catalog-sized: one comparison per section range")
    for w in ranges.windows(2) {
        // Zero-length sections may share offsets; real ranges must not
        // overlap.
        if w[1].0 < w[0].1 {
            return Err(ReadError::Format(FormatError::Corrupt {
                at: "section overlap",
            }));
        }
    }
    Ok(())
}

/// D3973 (C9): `buf` from `off` as `step`'s units, a tick before each piece read (a whole wrapped extent's read held a
/// dictionary prewarm 530 ms); outside a kernel call it never stops.
pub(crate) fn pread_polled(step: &mut cancelpoll::Step, io: &dyn PartIo, off: u64, buf: &mut [u8], at: &'static str) -> ReadResult<()> {
    pread_pieces(step, io, off, buf, at, EXTENT_PIECE)
}

pub(crate) fn pread_pieces(step: &mut cancelpoll::Step, io: &dyn PartIo, off: u64, buf: &mut [u8], at: &'static str, piece: usize) -> ReadResult<()> {
    step.pieces(buf.len(), piece, |r| io.pread_exact(off + r.start as u64, &mut buf[r], at))
}

/// An extent read's piece: 8 MiB, not `cancelpoll::PIECE`, until D4016 pairs a 1 MiB piece with the extent's WILLNEED, so
/// a cold read keeps its request size until then. A claim run (`read_run_bytes`, 2 MiB by
/// default) and a listed-section run stay one read under it.
const EXTENT_PIECE: usize = 8 << 20;
