//! The framed lazy dict handle (spec §7; charter §6): per-(part, attno,
//! path_ord) access to the byte-rank-sorted global dictionary with lazy
//! section faulting, a GENERATION-STABLE payload region (the StrView §7b
//! zero-copy dependency), and varlena-shaped entry presentation.
//!
//! ## Generation stability (the pin `src/tests/dict.rs` enforces)
//!
//! Once a dict section is faulted, its bytes NEVER move for the handle's
//! life: the buffer is an `Arc<[u8]>` held in a write-once cell inside the
//! handle (and the handle holds the part). A pointer returned by
//! [`DictHandle::entry`] therefore stays valid and constant across part-cache
//! churn, registry eviction, and concurrent faulting — dict materialization
//! can hand out zero-copy views (`lanev3-strview.md` §4).
//!
//! ## The lx_vec seam (M3-G)
//!
//! `lx_vec::DictSpace` is implemented over this type by the lx_source
//! implementor: `ncodes` ← [`DictHandle::ncodes`], `entry_datum` ← the
//! [`DictHandle::entry`] image pointer (a valid PG datum by the §7b law),
//! byte/char length faces ← [`DictHandle::lengths`] (index-only — stored
//! lengths make `length()` a table lookup with NO payload fault), sorted
//! order ← the byte-rank contract (spec §7: sorted order is contractual).
//! The u64 epoch currency is [`DictHandle::epoch64`]; the structural
//! identity is [`DictHandle::epoch_key`] (Law A: codes are meaningless
//! across epochs).

use std::sync::Arc;

use cost_profile::device::Device;
use pgrc_format::dict::{DictEntryRef, DICT_INDEX_ENTRY_LEN};
use pgrc_format::enc::Wrapper;
use pgrc_format::geom::DICT_FRAME_ENTRIES;
use pgrc_format::part::{StreamRole, StreamSectionHdr, STREAMF_DICT_EXEC, STREAMF_DICT_EXEC_NULLABLE, STREAM_SECTION_HDR_LEN};
use pgrc_format::FormatError;
use pgsync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use pgsync::Mutex;

use crate::cursor::{load_region_with, RegionState};
use crate::openpart::OpenPart;
use crate::registry::PartPin;
use crate::streams::ParsedStream;
use crate::{ReadError, ReadResult};

/// SB-7 payload fault mode. `FrameLazy` is the DESIGN DEFAULT (format-ledger
/// SB-7: frame-grain residency is format-default geometry); `WholeFault` is
/// the pre-L3 arm kept ONLY as the born-RED reproduction path — the
/// residency probe must show the difference (kill switch
/// `PGRUST_PGRC_FRAME_LAZY=0`, the #802-class census discipline).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictFaultMode {
    FrameLazy,
    WholeFault,
}

/// The frame-serve arm for frame-cut (SB-7 multi-extent, unwrapped)
/// payloads, stated explicitly — the env-independent probe face mirroring
/// [`DictFaultMode`]. `Direct` (production default) serves entries straight
/// from the part-cache `SegBuf`s; `Copy` is the frame-lazy memcpy arm that
/// preserves 8-aligned entry images (StrView §7b strict form; also the
/// `PGRUST_PGRC_FRAME_DIRECT=0` kill-switch arm).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictFrameServe {
    Direct,
    Copy,
}

impl DictFrameServe {
    fn from_env() -> DictFrameServe {
        if frame_direct() {
            DictFrameServe::Direct
        } else {
            DictFrameServe::Copy
        }
    }
}

/// [cold2→fd-arm] Frame-cut dict payloads serve DIRECT from the part cache
/// by default (no per-handle region, no frame memcpy) — armed default-ON
/// 2026-08-18 after the consumer audit found every production reader of
/// entry images byte-addressable (unaligned-safe header decodes; no
/// aligned casts over entry bytes — ADJUDICATION-20260818 §FRAME_DIRECT).
/// `PGRUST_PGRC_FRAME_DIRECT=0` is the kill switch back to the frame-lazy
/// copy arm, which preserves 8-aligned entry images (StrView §7b strict
/// form). Read per open, like the fault mode.
fn frame_direct() -> bool {
    !matches!(std::env::var("PGRUST_PGRC_FRAME_DIRECT").as_deref(), Ok("0"))
}

/// [stack] `PGRUST_PGRC_DICT_BLOCK_LAZY=1` OPTS IN to block-lazy serving
/// of WRAPPED single-extent dict payloads (the banks-of-record geometry:
/// one zstd block per 1024-entry dict frame under the §6.4 wrapper —
/// RESULTS-FMTLAND §B). Default OFF after the 100m A/B (RESULTS-STACK
/// §4.2): the decompressed blocks are HANDLE-lifetime, and the per-query-
/// run ruling reopens dict handles every rep/query — the whole arm
/// re-serves from the part-cache's process-lifetime unwrapped map, while
/// block-lazy re-preads + re-decompresses per handle (hot 10-50x on the
/// dense dict families; only sparse consumers win). Re-pose = move block
/// residency to part grain (the `unwrapped`-map analogue at block grain).
/// [densedict] Block-lazy grain election. `1`/`on` = the HANDLE-grain arm
/// (the measured-negative §4.2 configuration, kept for the A/B);
/// `part`/`2` = the RE-POSE: block residency at PART grain — the
/// decompressed block image lives in the part cache (`OpenPart::
/// dict_block_payload`, the `unwrapped`-map analogue at block grain), so
/// handle reopens under the per-query-run ruling Arc-clone it instead of
/// re-preading + re-decompressing (one decompress per (part, block) per
/// part residency). Default OFF (whole-region assembly) either way.
/// [sqe8blk] `part` additionally carries the per-handle DENSE/SPARSE
/// election ([`DictTouch`]): dense demands stream the whole payload
/// (uncached CRC'd read — no raw-extent double cache), sparse demands
/// fetch blocks; undeclared handles elect from bytes touched. `=1`
/// (handle grain) keeps the measured §4.2 behavior for the A/B.
/// `part` as the default was measured against `off` on one binary over wide-dictionary, cold and many-column reads
/// (coldab1.tsv): warm 0.96-1.04x, but cold 1.12-1.14x slower on three dense reads, because an undeclared handle under
/// `part` gives up card C's one-pread staging of the payload beside the index; so `off` stays the default.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockLazyGrain {
    Off,
    Handle,
    Part,
}

fn block_lazy() -> BlockLazyGrain {
    match std::env::var("PGRUST_PGRC_DICT_BLOCK_LAZY").as_deref() {
        Ok("1") | Ok("on") => BlockLazyGrain::Handle,
        Ok("part") | Ok("2") => BlockLazyGrain::Part,
        _ => BlockLazyGrain::Off,
    }
}

/// [sqe8blk] The per-handle touch class a consumer MAY declare after open
/// (the RESULTS-DENSEDICT §3.1 banked dense/sparse election). On the
/// PART-grain block-lazy arm the handle elects, per demand, between
/// fetch-blocks (sub-extent preads at 1024-entry block grain — the sparse
/// floor economics: one sorted top-N 336→81) and stream-whole (one sequential CRC'd
/// whole-extent read + decompress-all — the dense economics the gp2
/// measurement demands: its sqe-arm's 8,050 block preads / 171 MB LOST to
/// the orderly whole stream, +342 ms cold).
///
/// ELECTION INPUTS (per the house law — observable stats, never a
/// constant):
/// 1. The declared class, when the call site knows its touch pattern
///    (first declaration wins; byte-folding combines = Dense — those call
///    [`DictHandle::prewarm_payload`], which IS the dense election; a
///    zone-mask prewarm that PROVED sparsity declares Sparse so the
///    fallback below cannot un-decide it).
/// 2. Undeclared handles fall back to BYTES TOUCHED: the handle counts the
///    block faults its own demands caused and compares against the
///    payload's §6.4 block count (`nblocks`, from the block-offset table —
///    self-described geometry, not a tunable). Once this handle's block
///    reads have cost more than one stream of the whole payload, its fixed
///    cost included (`cost_profile::device`'s `faults_to_stream` of the
///    payload's mean block in the regime its extent is in, one cachestat at
///    the handle's first undeclared demand: after 2 faults cold, 11 warm for
///    20 blocks of 16 KiB on the reference box), the touch is dense by
///    observation and the REMAINDER is streamed whole (P5-18; the divisor
///    rule before it, nblocks / `stream_div`, left out the stream's fixed
///    read and streamed a 20-block payload after one fault).
/// 3. A handle that witnesses the shared part-grain image fully resident
///    (at open or after any ensure) short-circuits every later ensure to
///    one flag load — the §3.1 "skip the bit probe once all-resident"
///    hot refinement (+21 ms on two grouping queries).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictTouch {
    /// Byte-folding / whole-domain consumer: first demand streams the
    /// whole payload (one CRC'd sequential read + decompress-all).
    Dense,
    /// Point-probe consumer (top-K, gathers, zone-masked prewarm): serve
    /// at block grain, NEVER escalate to the whole stream.
    Sparse,
    /// P5-18: point probes whose count the caller cannot bound (a scan's
    /// renders, one per surviving row): served at block grain as `Sparse`,
    /// but the class stays undeclared, so the bytes-touched election
    /// (input 2) streams the rest once this handle's own block faults have
    /// cost more than one stream of the payload.
    Probe,
}

/// `DictHandle::touch` encoding (write-once via compare_exchange).
const TOUCH_UNDECLARED: u8 = 0;
const TOUCH_DENSE: u8 = 1;
const TOUCH_SPARSE: u8 = 2;

/// M2-ZONEREAD card C: whether a WRAPPED single-extent payload is served whole (one unwrap of the whole extent) under
/// this touch: then a caller that stages its raw bytes beside the index (`StreamCursor::stage_dict` / the values run's
/// tail) hands the handle's first fault its whole read. A block-lazy payload reads sub-ranges and never takes them.
pub fn wrapped_payload_whole(touch: Option<DictTouch>) -> bool {
    DictFaultMode::from_env() == DictFaultMode::WholeFault || (!matches!(touch, Some(DictTouch::Sparse | DictTouch::Probe)) && block_lazy() == BlockLazyGrain::Off)
}

/// P5-18: whether part `part`'s dictionary of `(attno, path_ord)` is cheaper streamed whole than served to `renders` point
/// probes at uniform codes at block grain: `Device::stream_pays` over its payload's blocks (one per `DICT_FRAME_ENTRIES`
/// entries, of the wrapped extent's mean size per block) in the regime of the extent's pages. Asked before the handle opens
/// (the stream directory only), so a caller that expects that many renders opens the dictionary undeclared and stages its
/// payload whole beside the index. `false` for a payload that is not one wrapped extent: only that one is served at block
/// grain, so the open mode changes nothing there.
pub fn wrapped_dense_pays(part: &OpenPart, attno: u32, path_ord: u32, renders: u64) -> ReadResult<bool> {
    let dir = part.stream_directory()?;
    let (Some(index), Some(payload)) = (dir.lookup(attno, path_ord, StreamRole::DictIndex), dir.lookup(attno, path_ord, StreamRole::DictPayload)) else {
        return Ok(false);
    };
    let [rec] = payload.extents.as_slice() else { return Ok(false) };
    if pgrc_format::enc::Wrapper::from_u8(payload.entry.wrapper)? == pgrc_format::enc::Wrapper::None || index.entry.values == 0 {
        return Ok(false);
    }
    let nb = index.entry.values.div_ceil(pgrc_format::geom::DICT_FRAME_ENTRIES as u64);
    Ok(Device::at_residency(part.page_resident(rec.file_off, rec.len)).stream_pays(renders, nb, rec.len / nb))
}

/// [sqe8blk] Kill switch for the §3.1 refinements (uncached ensure_all
/// whole-read + per-handle election + all-resident short-circuit):
/// `PGRUST_PGRC_BLOCKLAZY_REFINE=0` restores the densedict-measured
/// `=part` behavior — the part(old) A/B arm. Read per open/ensure_all
/// (rare, never hot).
fn blocklazy_refine() -> bool {
    !matches!(
        std::env::var("PGRUST_PGRC_BLOCKLAZY_REFINE").as_deref(),
        Ok("0")
    )
}

impl DictFaultMode {
    /// Resolve from the environment (read per open — the harness flips the
    /// arm between probe legs; opens are rare, the read is never hot).
    fn from_env() -> DictFaultMode {
        match std::env::var("PGRUST_PGRC_FRAME_LAZY") {
            Ok(v) if v == "0" || v.eq_ignore_ascii_case("off") => DictFaultMode::WholeFault,
            _ => DictFaultMode::FrameLazy,
        }
    }
}

// ---------------------------------------------------------------------------
// [cold2] Process-global dict payload fault census: bytes/frames the dict
// payload plane faulted (frame-lazy frame faults and whole-region
// assemblies), for the payload attribution witness (payload bytes
// faulted vs entries touched). Relaxed counters bumped only on FAULT
// events (rare — thousands per query), never on the entry() fast path.
// ---------------------------------------------------------------------------
pub static DICT_FRAME_FAULT_BYTES: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static DICT_FRAME_FAULTS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static DICT_WHOLE_FAULT_BYTES: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
/// [stack] Block-lazy census: COMPRESSED bytes preaded at wrapper-block
/// grain, and the number of block faults (RESULTS-FMTLAND §B.3 serving).
pub static DICT_BLOCK_FAULT_BYTES: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static DICT_BLOCK_FAULTS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Snapshot of the block-lazy census: (compressed_bytes_preaded, block_faults).
pub fn dict_block_census() -> (u64, u64) {
    use core::sync::atomic::Ordering::Relaxed;
    (
        DICT_BLOCK_FAULT_BYTES.load(Relaxed),
        DICT_BLOCK_FAULTS.load(Relaxed),
    )
}

/// Snapshot of the dict payload fault census:
/// (frame_fault_bytes, frame_faults, whole_region_bytes).
pub fn dict_fault_census() -> (u64, u64, u64) {
    use core::sync::atomic::Ordering::Relaxed;
    (
        DICT_FRAME_FAULT_BYTES.load(Relaxed),
        DICT_FRAME_FAULTS.load(Relaxed),
        DICT_WHOLE_FAULT_BYTES.load(Relaxed),
    )
}

/// The entry-access census (`crate::dict_census_on`; lane dictresident): random `entry()` calls,
/// entries the front-coded arm materialised (first touch per handle and code), entries walked
/// by a cursor, and the bytes the fc arm's materialisations hold.
pub static DICT_ENTRY_CALLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DICT_FC_MATERIALIZED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DICT_CURSOR_ENTRIES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DICT_FC_MATERIALIZED_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Snapshot: `(entry calls, fc materialised, cursor entries, fc materialised bytes)`.
pub fn dict_entry_census() -> (u64, u64, u64, u64) {
    use core::sync::atomic::Ordering::Relaxed;
    (DICT_ENTRY_CALLS.load(Relaxed), DICT_FC_MATERIALIZED.load(Relaxed), DICT_CURSOR_ENTRIES.load(Relaxed), DICT_FC_MATERIALIZED_BYTES.load(Relaxed))
}

/// The front-coded arm's materialised entries: an append-only chunk arena (entries never move)
/// behind the handle's memo — one decode per (handle, code), a pointer stable for the handle's life.
/// D4482 b: the chunks are the part cache's from their allocation (`charge`, opened at the first chunk; dropped before
/// the chunks free), as the payload images are (`ImageBuf`): the handle a list's warm cell keeps outlives the statement
/// whose read materialised them.
struct FcArena {
    charge: Option<memledger::Charge>,
    chunks: Vec<Box<[u8]>>,
    used: usize,
}

const FC_CHUNK: usize = 1 << 20;

impl FcArena {
    fn alloc(&mut self, len: usize) -> &mut [u8] {
        let need = len.next_multiple_of(8);
        if self.chunks.last().is_none_or(|c| self.used + need > c.len()) {
            let n = need.max(FC_CHUNK);
            self.chunks.push(vec![0u8; n].into_boxed_slice()); // alloc: Exec
            match &mut self.charge {
                Some(c) => c.commit(n as u64),
                None => self.charge = Some(crate::partcache::holder_charge(n as u64)),
            }
            self.used = 0;
        }
        let at = self.used;
        self.used += need;
        let c = self.chunks.last_mut().expect("just pushed");
        &mut c[at..at + len]
    }
}

/// The structural dict-epoch identity (spec §7): a dict epoch IS
/// (part identity, attno, path_ord). Layout-pinned (24 B). The definition
/// moved to the frozen ABI crate at M3-L3 (AB-2.2 names it the dict-code
/// lane's per-batch epoch tag identity); this re-export keeps the reader's
/// public face unchanged.
pub use pgrc_batch::DictEpochKey;

/// D4071: a lazily faulted region's cell, locked with the wait polled (`cancelpoll::lock_or_cancel`): its holder can sit in
/// the region's whole read and rebuild for hundreds of ms, which a waiter on `Mutex::lock` waited out past its statement's
/// cancel. A poisoned cell is taken as it is, as before.
fn lock_cell<T>(cell: &Mutex<T>) -> ReadResult<pgsync::MutexGuard<'_, T>> {
    let mut step = cancelpoll::step("pgrc_read dict: a region cell's lock waited on");
    Ok(cancelpoll::lock_or_cancel(&mut step, cell)?)
}

/// One lazily-faulted region publication: the write-once cell owns the
/// bytes; (ptr, len) are published AFTER the cell is written and never
/// change — the generation-stability mechanism.
struct LazyRegion {
    cell: Mutex<Option<RegionState>>,
    ptr: AtomicPtr<u8>,
    len: AtomicUsize,
}

impl LazyRegion {
    fn new() -> LazyRegion {
        LazyRegion {
            cell: Mutex::new(None),
            ptr: AtomicPtr::new(core::ptr::null_mut()),
            len: AtomicUsize::new(0),
        }
    }

    /// Fast path: the published region, if faulted.
    fn get(&self) -> Option<(*const u8, usize)> {
        let p = self.ptr.load(Ordering::Acquire);
        if p.is_null() {
            return None;
        }
        Some((p as *const u8, self.len.load(Ordering::Acquire)))
    }

    /// Fault-and-publish (once). Racing callers serialize on the cell lock;
    /// exactly one stores, all see the same region afterwards.
    fn ensure(
        &self,
        part: &Arc<OpenPart>,
        unwrappers: &[&dyn crate::SectionUnwrapper],
        ps: &ParsedStream,
        what: &'static str,
        dict_index: Option<&[u8]>,
    ) -> ReadResult<(*const u8, usize)> {
        let census_payload = what == "dict payload stream";
        if let Some(r) = self.get() {
            return Ok(r);
        }
        let mut cell = lock_cell(&self.cell)?;
        if cell.is_none() {
            let region = load_region_with(part, unwrappers, ps, what, dict_index)?;
            let payload = region.payload();
            let ptr = payload.as_ptr() as *mut u8;
            let len = payload.len();
            *cell = Some(region);
            // Publish AFTER the owning cell is written (Release pairs with
            // the Acquire in `get`): a non-null ptr proves the Arc'd buffer
            // is rooted in the cell for this handle's lifetime.
            self.len.store(len, Ordering::Release);
            self.ptr.store(ptr, Ordering::Release);
            if census_payload {
                DICT_WHOLE_FAULT_BYTES
                    .fetch_add(len as u64, core::sync::atomic::Ordering::Relaxed);
            }
        }
        let region = cell.as_ref().expect("just stored");
        let payload = region.payload();
        Ok((payload.as_ptr(), payload.len()))
    }
}

/// Frame-lazy interior state: the write-once whole-region reservation the
/// per-frame faults fill in place.
struct FrameLazyState {
    /// The stable backing store, sized to the WHOLE assembled region and
    /// allocated ZEROED (`reserve_zeroed` ⇒ untouched pages stay uncommitted
    /// — reserving 3 GB costs address space, faulted frames cost RSS).
    /// NEVER reallocated: base pointer stability for the handle's life is
    /// the StrView §7b dependency, exactly as for the whole-fault arm.
    buf: ImageBuf,
    len: usize,
    /// Region-relative start offset of each extent (frame), from the
    /// contiguity-validated extent table.
    offsets: Vec<usize>,
}

/// SB-7 frame-lazy payload publication (M3-L3): the contractual dict fault
/// grain (the frame) made REAL on the v4 writer's frame-boundary extent
/// geometry. Extent i carries dict frame i (per-frame CRC; extent 0 also
/// carries the section header, the last extent also the frame-table tail).
/// `ensure_extent(f)` faults exactly extent f — a first entry touch no
/// longer faults the 3.01/2.62/2.27 GB payload class (a wide-text top-N's
/// memory ingredient).
struct FrameLazyPayload {
    cell: Mutex<Option<FrameLazyState>>,
    /// Published whole-region base + len + payload bounds (Release after
    /// the cell owns the buffer; non-null ptr proves rooting — the same
    /// publication idiom as [`LazyRegion`]).
    ptr: AtomicPtr<u8>,
    len: AtomicUsize,
    payload_start: AtomicUsize,
    payload_end: AtomicUsize,
    /// Per-frame publication bits (Release on store; Acquire on the fast
    /// path) — bit i set ⇔ extent i's bytes are resident and immutable.
    frame_bits: Vec<AtomicU64>,
    /// [idx251] Region-relative start offset of each extent (frame),
    /// published once (write-once) alongside the region ptr so the read
    /// path can bound an entry to its SPECIFIC published frame extent in
    /// O(1) without taking the cell lock. `offsets[i]` = start of extent i;
    /// extent i's byte span is `[offsets[i], offsets[i+1])` (the last frame
    /// runs to `len`).
    frame_offsets: pgsync::OnceLock<Vec<usize>>,
    /// Faulted payload bytes — the residency census term
    /// (`pgrc_dict_frames_resident` class; the born-RED whole-fault arm
    /// shows the whole region here).
    resident: AtomicUsize,
    frames: u32,
}

impl FrameLazyPayload {
    fn new(nextents: usize) -> FrameLazyPayload {
        let words = nextents.div_ceil(64);
        FrameLazyPayload {
            cell: Mutex::new(None),
            ptr: AtomicPtr::new(core::ptr::null_mut()),
            len: AtomicUsize::new(0),
            payload_start: AtomicUsize::new(0),
            payload_end: AtomicUsize::new(0),
            frame_bits: (0..words).map(|_| AtomicU64::new(0)).collect(),
            frame_offsets: pgsync::OnceLock::new(),
            resident: AtomicUsize::new(0),
            frames: nextents as u32,
        }
    }

    /// [idx251] Region-relative `[lo, hi)` byte span of frame `f` (extent
    /// `f`), for per-frame bounding of the lazy read. `None` until the
    /// region is published (`frame_offsets` set alongside the ptr).
    #[inline]
    fn frame_span(&self, f: u32) -> Option<(usize, usize)> {
        let offs = self.frame_offsets.get()?;
        let i = f as usize;
        let lo = *offs.get(i)?;
        let hi = match offs.get(i + 1) {
            Some(&h) => h,
            None => self.len.load(Ordering::Acquire),
        };
        Some((lo, hi))
    }

    #[inline]
    fn frame_resident(&self, f: u32) -> bool {
        let w = (f / 64) as usize;
        self.frame_bits[w].load(Ordering::Acquire) & (1u64 << (f % 64)) != 0
    }

    /// The published region, if extent 0 (header) has been faulted.
    fn get(&self) -> Option<(*const u8, usize, usize, usize)> {
        let p = self.ptr.load(Ordering::Acquire);
        if p.is_null() {
            return None;
        }
        Some((
            p as *const u8,
            self.len.load(Ordering::Acquire),
            self.payload_start.load(Ordering::Acquire),
            self.payload_end.load(Ordering::Acquire),
        ))
    }

    /// Fault extent (= dict frame) `f` into the reserved region; always
    /// faults extent 0 first (header decode publishes the region). Racing
    /// callers serialize on the cell lock; frame bytes are written exactly
    /// once and published with Release — once a frame bit is set, its
    /// bytes never change (generation stability at frame grain).
    fn ensure_extent(
        &self,
        part: &Arc<OpenPart>,
        ps: &ParsedStream,
        f: u32,
        what: &'static str,
    ) -> ReadResult<()> {
        if f >= self.frames {
            return Err(ReadError::Format(FormatError::Bounds { at: "dict frame" }));
        }
        if self.frame_resident(f) && !self.ptr.load(Ordering::Acquire).is_null() {
            return Ok(());
        }
        let mut cell = lock_cell(&self.cell)?;
        if cell.is_none() {
            // First fault on this handle: validate the extent geometry the
            // whole-fault path validates (contiguity; the SB-7 cut), size
            // and reserve the region.
            let mut step = cancelpoll::step("pgrc_read dict ensure_extent: a frame-lazy region's geometry");
            for (i, w) in ps.extents.windows(2).enumerate() {
                step.batch(i)?;
                if w[0].file_off + w[0].len != w[1].file_off {
                    return Err(ReadError::Format(FormatError::Corrupt {
                        at: "byte-run extent contiguity",
                    }));
                }
            }
            // [idx200] Bound the CLAIMED whole-region total against the
            // absolute cap BEFORE reserving — the extent lengths are
            // file-controlled and no read has yet witnessed them (the
            // whole-fault path reads every extent, each file-bounds-checked in
            // `fault_range`, before it assembles `total`; the lazy arm reserves
            // up front, so it MUST refuse hostile geometry here). Typed,
            // catchable (ERRCODE_DATA_CORRUPTED) — never an unbounded reservation.
            let total =
                dict_region_total_checked(ps.extents.iter().map(|e| e.len as usize).sum())?;
            let mut offsets = pgrc_format::try_vec(ps.extents.len(), "dicthandle")?;
            let mut off = 0usize;
            for (i, e) in ps.extents.iter().enumerate() {
                step.batch(i)?;
                offsets.push(off);
                off += e.len as usize;
            }
            // [idx200] Fallible reservation: even under the cap, an allocation
            // failure must be a typed refusal, not `handle_alloc_error`'s
            // process abort (which the infallible `vec!` would take, bypassing
            // statement-thread panic containment).
            let buf = ImageBuf::zeroed(total.div_ceil(8)).ok_or_else(|| {
                ReadError::Format(FormatError::Corrupt {
                    at: "dict region reservation",
                })
            })?;
            *cell = Some(FrameLazyState {
                buf,
                len: total,
                offsets,
            });
        }
        let state = cell.as_mut().expect("just stored");
        let base = state.buf.as_mut_ptr() as *mut u8;
        let state_len = state.len;
        let offsets = &state.offsets;
        let fault_one = |i: u32| -> ReadResult<()> {
            if self.frame_resident(i) {
                return Ok(());
            }
            let rec = &ps.extents[i as usize];
            let seg = part.extent_bytes(&ps.entry, rec, i)?;
            let off = offsets[i as usize];
            debug_assert!(off + seg.len() <= state_len, "extent beyond region");
            // SAFETY: raw-pointer write into the never-reallocated,
            // cell-owned buffer; the target frame range is UNPUBLISHED
            // (its bit is clear), no shared reference reads unpublished
            // bytes (readers gate every dereference on ensure_code →
            // frame bit, Acquire), and all writes serialize on the cell
            // lock. Never a `&mut` over the buffer — published frames are
            // concurrently read.
            unsafe {
                core::ptr::copy_nonoverlapping(seg.bytes().as_ptr(), base.add(off), seg.len());
            }
            self.resident.fetch_add(seg.len(), Ordering::Relaxed);
            DICT_FRAME_FAULT_BYTES
                .fetch_add(seg.len() as u64, core::sync::atomic::Ordering::Relaxed);
            DICT_FRAME_FAULTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let w = (i / 64) as usize;
            self.frame_bits[w].fetch_or(1u64 << (i % 64), Ordering::Release);
            Ok(())
        };
        // Extent 0 first: the header lives there, and publishing the
        // region requires its decode.
        fault_one(0)?;
        if self.ptr.load(Ordering::Acquire).is_null() {
            let bytes = unsafe {
                core::slice::from_raw_parts(state.buf.as_ptr() as *const u8, state.len)
            };
            let hdr = StreamSectionHdr::decode(bytes)?;
            if hdr.encoding != ps.entry.encoding || hdr.wrapper != ps.entry.wrapper {
                return Err(ReadError::Format(FormatError::Corrupt { at: what }));
            }
            let (pstart, pend) = crate::cursor::payload_bounds(&hdr, state.len)?;
            // [idx251] Publish the extent offset table BEFORE the ptr so a
            // reader that sees a non-null ptr (Acquire) also sees the
            // offsets — the read path bounds each entry to its published
            // frame extent via `frame_span`.
            let _ = self.frame_offsets.set(state.offsets.clone());
            self.payload_start.store(pstart as usize, Ordering::Release);
            self.payload_end.store(pend as usize, Ordering::Release);
            self.len.store(state.len, Ordering::Release);
            self.ptr
                .store(state.buf.as_ptr() as *mut u8, Ordering::Release);
        }
        fault_one(f)
    }
}

/// [cold2] FRAME-DIRECT payload serving: entries are resolved straight
/// from the part-cache `SegBuf`s (one per frame-cut extent) — no
/// per-handle whole-region reservation and NO second memcpy per frame.
/// A frame the claim-horizon prefetcher already landed is an `Arc` clone
/// here (zero copies past the pread). Generation stability holds: the
/// `SegBuf`s are write-once in the per-extent `OnceLock`s and live for
/// the handle's life. THE TRADE: entry images can lose the §7b 8-align
/// datum guarantee (frame cuts land at the UNPADDED end of the previous
/// frame, so a frame's first entry sits at a non-8 offset inside its
/// SegBuf — the 8-align pin fails on this arm by measurement). The
/// 2026-08-18 consumer audit verified every production reader of entry
/// images — sqe kernels (`entry().bytes` compares), the scan-side datum
/// publishers (`ScanDictSpace::entry_datum`, the checked-gather lane) and
/// their downstream varlena readers (`varsize_any` family, StrView cells,
/// hashfn, tuple materialization) — reads entry bytes through
/// unaligned-safe byte accesses, so the arm is the production DEFAULT;
/// `PGRUST_PGRC_FRAME_DIRECT=0` (kill switch) restores the copy arm and
/// with it the strict 8-align form.
struct FrameDirectPayload {
    /// Per-extent resident frames (write-once).
    segs: Vec<pgsync::OnceLock<crate::openpart::SegBuf>>,
    /// Region-relative start offset of each extent; published with the
    /// geometry after extent 0's header decode.
    geom: pgsync::OnceLock<FrameDirectGeom>,
    resident: AtomicUsize,
    frames: u32,
}

struct FrameDirectGeom {
    offsets: Vec<usize>,
    payload_start: usize,
    payload_end: usize,
}

impl FrameDirectPayload {
    fn new(nextents: usize) -> FrameDirectPayload {
        FrameDirectPayload {
            segs: (0..nextents).map(|_| pgsync::OnceLock::new()).collect(),
            geom: pgsync::OnceLock::new(),
            resident: AtomicUsize::new(0),
            frames: nextents as u32,
        }
    }

    /// Fault extent `f` (and extent 0 for the header, once); publishes the
    /// geometry after extent 0. Same contiguity validation as frame-lazy.
    fn ensure_extent(
        &self,
        part: &Arc<OpenPart>,
        ps: &ParsedStream,
        f: u32,
        what: &'static str,
    ) -> ReadResult<()> {
        if f >= self.frames {
            return Err(ReadError::Format(FormatError::Bounds { at: "dict frame" }));
        }
        if self.geom.get().is_none() {
            let mut step = cancelpoll::step("pgrc_read dict ensure_extent: a frame-direct region's geometry");
            for (i, w) in ps.extents.windows(2).enumerate() {
                step.batch(i)?;
                if w[0].file_off + w[0].len != w[1].file_off {
                    return Err(ReadError::Format(FormatError::Corrupt {
                        at: "byte-run extent contiguity",
                    }));
                }
            }
            let total: usize = ps.extents.iter().map(|e| e.len as usize).sum();
            let mut offsets = Vec::with_capacity(ps.extents.len());
            let mut off = 0usize;
            for (i, e) in ps.extents.iter().enumerate() {
                step.batch(i)?;
                offsets.push(off);
                off += e.len as usize;
            }
            // Extent 0 carries the section header: decode it to bound the
            // payload window, then publish the geometry once.
            let seg0 = self.fault_seg(part, ps, 0)?;
            let hdr = StreamSectionHdr::decode(seg0.bytes())?;
            if hdr.encoding != ps.entry.encoding || hdr.wrapper != ps.entry.wrapper {
                return Err(ReadError::Format(FormatError::Corrupt { at: what }));
            }
            let (pstart, pend) = crate::cursor::payload_bounds(&hdr, total)?;
            let _ = self.geom.set(FrameDirectGeom {
                offsets,
                payload_start: pstart as usize,
                payload_end: pend as usize,
            });
        }
        self.fault_seg(part, ps, f)?;
        Ok(())
    }

    fn fault_seg(
        &self,
        part: &Arc<OpenPart>,
        ps: &ParsedStream,
        i: u32,
    ) -> ReadResult<&crate::openpart::SegBuf> {
        if let Some(seg) = self.segs[i as usize].get() {
            return Ok(seg);
        }
        let rec = &ps.extents[i as usize];
        let seg = part.extent_bytes(&ps.entry, rec, i)?;
        let len = seg.bytes().len();
        let cell = &self.segs[i as usize];
        if cell.set(seg).is_ok() {
            self.resident.fetch_add(len, Ordering::Relaxed);
            DICT_FRAME_FAULT_BYTES.fetch_add(len as u64, core::sync::atomic::Ordering::Relaxed);
            DICT_FRAME_FAULTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        Ok(cell.get().expect("just set"))
    }

    /// Resolve entry bytes for `code` frame-locally (index rec already
    /// decoded by the caller): `(image, bytes)` out of frame
    /// `code / DICT_FRAME_ENTRIES`'s SegBuf.
    fn entry_slices(
        &self,
        code: u32,
        payload_off: usize,
    ) -> ReadResult<(&[u8], &[u8])> {
        let f = (code / DICT_FRAME_ENTRIES) as usize;
        let geom = self.geom.get().ok_or(ReadError::Format(FormatError::Corrupt {
            at: "dict payload geometry unpublished after ensure",
        }))?;
        let seg = self.segs[f]
            .get()
            .ok_or(ReadError::Format(FormatError::Corrupt {
                at: "dict frame unresident after ensure",
            }))?;
        let region_off = geom.payload_start + payload_off;
        if region_off >= geom.payload_end {
            return Err(ReadError::Format(FormatError::Bounds { at: "dict payload" }));
        }
        let lo = region_off
            .checked_sub(geom.offsets[f])
            .ok_or(ReadError::Format(FormatError::Bounds { at: "dict payload" }))?;
        pgrc_format::wire::varlena_entry_at(seg.bytes(), lo, "dict payload")
            .map_err(Into::into)
    }
}

/// [stack] BLOCK-LAZY serving of WRAPPED single-extent dict payloads — the
/// reader-only partial unwrap RESULTS-FMTLAND §B proved viable on the banks
/// of record (every wrapped dict payload is block-cut at 1024-entry dict-
/// frame grain with a §6.4 block-offset table at the head of the payload
/// region and the frame table at the raw tail). Serving chain: entry code →
/// frame = code/1024 → block (1:1 when the geometry is valid) → sub-extent
/// pread of just that compressed block (~40-300 KB) → one-block decompress
/// into a write-once reserved region at the block's uncompressed span.
/// Generation stability and §7b alignment hold exactly as on the whole-
/// fault arm: the reserved buffer never moves, and the uncompressed image
/// bytes land at the same payload-relative offsets the whole unwrap would
/// put them at. Integrity: sub-extent preads cannot be checked against the
/// extent's whole-range CRC — per-block zstd decode failure is the typed
/// witness (the bulk `ensure_all` path reads the whole extent CRC-checked).
/// Any geometry the probe would skip (no frame table, block/frame count
/// skew, binding without block decode) falls back to the whole-region
/// unwrap — never wrong, only whole.
/// [idx136, D302] Anti-amplification law on a block-lazy dict payload's declared
/// uncompressed image. `hdr.uncompressed_len` is file-controlled (a u32, up to
/// ~4 GiB), arrives by an UNCHECKED sub-extent pread (no CRC covers the head),
/// and sizes BOTH the write-once reservation (`BlockLazyState::buf`) and the
/// per-block decompress target (`fault_block`'s last-block span reaches it).
///
/// The law is the writer's: the image ends 0..=7 bytes past the last entry the
/// dict INDEX records ([`pgrc_format::dict::dict_payload_image_len_range`]),
/// and the index is the CRC-validated resident image. A header that disagrees
/// is refused typed before any reservation, so a crafted near-4-GiB claim over
/// a small extent stops here unless the file's own index says the same.
///
/// D302: this replaces a 1024× uncompressed:compressed ratio, which refused
/// VALID writer output — front coding stores a shared-prefix entry as a
/// varint, and zstd then expands a run ~30000:1 per block, so a FrontZstd
/// image legitimately reaches 1177:1 (800 near-identical 2,100-byte values)
/// and far beyond; no ratio is sound. The Whole arm applies the same law
/// (`pgrc_codec::wrapper::unwrap_dict_payload_section`), so the two arms
/// agree on which payloads are readable.
///
/// Why that is enough: a file whose index and header agree is
/// indistinguishable from valid writer output, whose image the writer may
/// seal up to u32::MAX. Its worst case is a FALLIBLE reservation (typed
/// refusal on allocation failure, never `handle_alloc_error`'s abort) of the
/// size the index records — what a valid dict of that size costs.
/// [`DICT_PAYLOAD_MAX_UNC_BYTES`] stays as the block-lazy reservation ceiling:
/// above it the handle serves through the whole arm (`Ok(None)`), never a
/// refusal the whole arm would not also give.
const DICT_PAYLOAD_MAX_UNC_BYTES: usize = 1 << 30; // 1 GiB (PG MaxAllocSize class)

/// Validate a dict payload's declared uncompressed length against the
/// writer's law over the resident v1 `index`. Returns the validated `unc_len`
/// or a typed `Corrupt` refusal — never a reservation sized from a header the
/// index contradicts.
fn dict_payload_unc_checked(unc_len: usize, index: &[u8]) -> ReadResult<usize> {
    let (lo, hi) = pgrc_format::dict::dict_payload_image_len_range(index)?;
    if (unc_len as u64) < lo || unc_len as u64 > hi {
        return Err(ReadError::Format(FormatError::Corrupt {
            at: "dict payload uncompressed_len vs dict index",
        }));
    }
    Ok(unc_len)
}

/// [idx200] Anti-amplification bound on a FRAME-LAZY / whole-region dict
/// region's declared total byte size. The frame-lazy arm sums the CLAIMED
/// per-extent lengths (`ExtentRecord::len` — file-controlled u64s) and reserves
/// ONE whole-region buffer of that total BEFORE any extent read witnesses the
/// claim against the file, so a crafted part with hostile extent geometry could
/// size a multi-GiB (or larger) reservation over a few-hundred-byte file
/// (unbounded allocation → `handle_alloc_error` cluster abort). The reader MUST
/// refuse any claimed total exceeding the absolute cap ([`DICT_PAYLOAD_MAX_UNC_BYTES`],
/// the PG MaxAllocSize class) — typed and catchable (`ERRCODE_DATA_CORRUPTED`
/// via [`FormatError::Corrupt`]), never an unbounded reservation. The
/// whole-fault path (`cursor::load_region`) is naturally witnessed — it reads
/// every extent (each file-bounds-checked in `OpenPart::fault_range`) before it
/// assembles the total — so this bound closes only the lazy arm's up-front
/// reservation gap. Valid dicts (region well under the cap) pass untouched.
fn dict_region_total_checked(total: usize) -> ReadResult<usize> {
    if total > DICT_PAYLOAD_MAX_UNC_BYTES {
        return Err(ReadError::Format(FormatError::Corrupt {
            at: "dict region total",
        }));
    }
    Ok(total)
}

struct BlockLazyState {
    /// D4552 (a): `btab` and `ftab` are the part cache's from their allocation, as `buf` is: dropped before them (field
    /// order), on whichever thread drops the state (a trim's evicted hook, another backend).
    tabs_charge: Option<memledger::Charge>,
    /// D4084: the whole fallback's region on the part cache's account; dropped before `whole` frees (field order).
    whole_charge: Option<memledger::Charge>,
    /// Reserved zeroed uncompressed-payload image (`reserve_zeroed` ⇒
    /// untouched blocks stay uncommitted). NEVER reallocated. Empty on the
    /// whole-fallback path (the region below roots the bytes instead).
    buf: ImageBuf,
    /// Uncompressed payload length (`hdr.uncompressed_len`).
    len: usize,
    /// Compressed block offsets, payload-relative, `nb + 1` entries
    /// (§6.4 block-offset table; entry 0 = the table's own length).
    btab: Vec<u32>,
    /// Uncompressed frame starts, payload-relative, `nb` entries.
    ftab: Vec<u32>,
    /// A side-block arm's decoded side block (`btab` then has one entry past the frames' blocks).
    side: Option<crate::SideTable>,
    /// Whole-fallback rooting: the assembled region (wrapped geometry the
    /// block path refused), published instead of `buf`.
    whole: Option<RegionState>,
}

/// D2985 + D4084 (C8 move 1): a lazily faulted dictionary payload's reservation of `words` u64s, all zero with none of
/// its whole pages committed (`reserve_zeroed`, D4135: they commit as faults write them; the fill it replaced wrote every
/// page of the payload up front) and the part cache's from its allocation: its charge moves the block from the
/// opening thread's ledger account to the cache's at once and back to the freeing thread's at its drop, and the residency
/// registry lists it as not resident until faults write its pages. Before, door 1 left the whole reservation on the
/// opener's account (past its session's end, on 'unbound') and the image's last holder went that much negative at the
/// free.
struct ImageBuf {
    /// Its residency registry entry (`resident::NONE`: empty, or the registry could not grow).
    reg: u32,
    /// Dropped before `words` frees (field order).
    charge: Option<memledger::Charge>,
    words: Box<[u64]>,
}

impl ImageBuf {
    fn empty() -> ImageBuf {
        ImageBuf { reg: memledger::resident::NONE, charge: None, words: Box::new([]) }
    }

    /// `None` when the allocation fails (the callers' typed refusal, never `handle_alloc_error`'s abort).
    fn zeroed(words: usize) -> Option<ImageBuf> {
        if words == 0 {
            return Some(ImageBuf::empty());
        }
        let layout = core::alloc::Layout::array::<u64>(words).ok()?;
        // SAFETY: `layout` has a non-zero size; the allocation is checked for null, is `words` zeroed u64s (a valid value
        // for every element) laid out as `[u64; words]`, and the global allocator frees it as that Box's layout.
        let words = unsafe {
            let p = reserve_zeroed(layout)?;
            Box::from_raw(core::ptr::slice_from_raw_parts_mut(p, words))
        };
        let bytes = layout.size();
        let charge = Some(crate::partcache::holder_charge(bytes as u64));
        let reg = memledger::resident::register(words.as_ptr() as usize, bytes, bytes, crate::partcache::LEDGER, false, true);
        Some(ImageBuf { reg, charge, words })
    }
}

/// D4135: `layout`'s bytes from the global allocator, all zero, with no whole page of them committed; `None` when the
/// allocation fails. `alloc_zeroed` gave that only over fresh OS pages: over memory the allocator reuses it zeroes with a
/// write, which commits every page of a reservation no block was faulted into (a hot scan suite: 1.8-1.9 GB of the part
/// cache's 4.1 GB of dictionary reservations resident, 2.4x the cache's census). Here the whole pages go back to the
/// kernel first (`pagegive::give_back`: they read zero after and commit at their first write, base pages, so one block's
/// fault commits that block) and only the unaligned head and tail are written; where the kernel refuses, the write zeroes
/// it all, as before. C.23: under PoolAlloc a superpage and up comes from the pool's base pages, with no give-back.
///
/// # Safety
/// `layout` has a non-zero size.
#[cfg(target_os = "linux")]
unsafe fn reserve_zeroed(layout: core::alloc::Layout) -> Option<*mut u64> {
    const PAGE: usize = 4096;
    // SAFETY: the caller's contract (a non-zero size)
    if let Some(p) = pagepool::base_pages(layout.size(), || unsafe { std::alloc::alloc_zeroed(layout) }) {
        return (!p.is_null()).then_some(p as *mut u64);
    }
    // SAFETY: the caller's contract (a non-zero size)
    let p = unsafe { std::alloc::alloc(layout) };
    if p.is_null() {
        return None;
    }
    let (at, bytes) = (p as usize, layout.size());
    let (lo, hi) = (at.next_multiple_of(PAGE), (at + bytes) / PAGE * PAGE);
    // SAFETY: `[lo, hi)` is whole pages inside the block just allocated, which nothing else can reach yet and whose bytes
    // are dead (never initialized); the writes stay inside the block.
    unsafe {
        // P6: under PoolAlloc (`pagepool::GLOBAL_BUILD`) the block may be the pool's, named so by give_back_any; in the
        // other build it is mimalloc's: the allocator's mapping, as before (the constant folds the branch away)
        if hi > lo
            && (if pagepool::GLOBAL_BUILD {
                pagepool::give_back_any(lo, hi - lo, false)
            } else {
                pagegive::give_back(lo, hi - lo, pagegive::Mapping::Allocator { huge: false })
            }) == 0
        {
            core::ptr::write_bytes(p, 0, lo - at);
            core::ptr::write_bytes(hi as *mut u8, 0, at + bytes - hi);
        } else {
            core::ptr::write_bytes(p, 0, bytes);
        }
    }
    Some(p as *mut u64)
}

/// # Safety
/// `layout` has a non-zero size.
#[cfg(not(target_os = "linux"))]
unsafe fn reserve_zeroed(layout: core::alloc::Layout) -> Option<*mut u64> {
    // SAFETY: the caller's contract (a non-zero size)
    let p = unsafe { std::alloc::alloc_zeroed(layout) };
    (!p.is_null()).then_some(p as *mut u64)
}

impl core::ops::Deref for ImageBuf {
    type Target = [u64];

    fn deref(&self) -> &[u64] {
        &self.words
    }
}

impl core::ops::DerefMut for ImageBuf {
    fn deref_mut(&mut self) -> &mut [u64] {
        &mut self.words
    }
}

impl Drop for ImageBuf {
    fn drop(&mut self) {
        memledger::resident::unregister(self.reg);
    }
}

pub(crate) struct BlockLazyPayload {
    /// D4552 (a): the payload's own `Arc` block on the part cache's holder from its making (`OpenPart::dict_block_payload`),
    /// so whichever handle or part drops it last takes the bytes back before the free. First, so it drops first.
    held: pgsync::OnceLock<memledger::Charge>,
    cell: Mutex<Option<BlockLazyState>>,
    /// D4552 (a): `bits` and `frame_starts` are the part cache's from their allocation (`charge_tables`), as the image
    /// is: dropped before them (field order), on whichever thread drops the handle's last holder.
    tables_charge: pgsync::OnceLock<memledger::Charge>,
    /// Per-block publication bits (sized at first fault; Release on store,
    /// Acquire on the fast path) — bit b set ⇔ block b's uncompressed
    /// bytes are resident and immutable. Whole-fallback publishes ONE
    /// all-set bit.
    bits: pgsync::OnceLock<Vec<AtomicU64>>,
    /// [idx251] Uncompressed frame starts (`ftab`, payload-relative),
    /// published once (write-once) on the BLOCK path so the read path can
    /// bound an entry to its SPECIFIC published block in O(1) without the
    /// cell lock. Block `b` covers payload bytes `[ftab[b], ftab[b+1])`
    /// (the last block runs to `len`). UNSET on the whole-fallback path
    /// (that publishes the entire payload atomically — the whole window IS
    /// the published window, so no per-block bound is needed).
    frame_starts: pgsync::OnceLock<Vec<u32>>,
    nblocks: AtomicUsize,
    /// Published payload window (base, len): Release after the cell owns
    /// the backing store — the same idiom as [`LazyRegion`].
    ptr: AtomicPtr<u8>,
    len: AtomicUsize,
    /// Faulted UNCOMPRESSED payload bytes (the residency census term).
    resident: AtomicUsize,
    /// D4795 (2a): the statement `resident`'s latest census bytes were charged to (a leaf lock).
    gov: Mutex<crate::partcache::Charged>,
    /// [sqe8blk] Published block count (Release AFTER each block's bit —
    /// under the cell lock, so `== nblocks` with Acquire proves every
    /// block's bytes are visible). Feeds [`BlockLazyPayload::
    /// fully_resident`], the handle's all-resident short-circuit.
    blocks_resident: AtomicUsize,
}

impl Drop for BlockLazyPayload {
    fn drop(&mut self) {
        // D3003: the part cache census forgets the image's faulted bytes as they free (D4084: their memory leaves the
        // cache's account with the state's charges)
        crate::partcache::uncount(self.resident.load(Ordering::Relaxed) as u64, &mut pgsync::lock(&self.gov));
    }
}

impl BlockLazyPayload {
    /// `n` faulted bytes into the census; their memory is on the cache's account already (D4084: the reservation's or the
    /// whole region's charge).
    #[inline]
    fn charge(&self, part: &OpenPart, ps: &ParsedStream, n: usize) {
        self.resident.fetch_add(n, Ordering::Relaxed);
        crate::partcache::count(n as u64, &mut pgsync::lock(&self.gov));
        // a dictionary block faulted into the part cache: its miss and bytes (untimed)
        memledger::value::missed(memledger::Consumer::PartCache, n as u64);
        // cache plan A6: the decoded bytes read back, at the key the payload's door looks it up by (its one extent)
        if let Some(rec) = ps.extents.first() {
            memledger::value::reloaded(memledger::Consumer::PartCache, part.value_key() ^ rec.file_off.rotate_left(32), n as u64);
        }
    }

    /// D4552 (a): the charge of its own `Arc` block (`held`); a second is dropped here.
    pub(crate) fn hold(&self, c: memledger::Charge) {
        let _ = self.held.set(c);
    }

    /// D3003: faulted image bytes.
    pub(crate) fn resident_bytes(&self) -> u64 {
        self.resident.load(Ordering::Relaxed) as u64
    }

    /// D4552 (a): `bits` and `frame_starts`, just set on this thread (under the cell lock, at the state's first init), onto the
    /// part cache's account; a later init finds them set and its own copies drop here, on this thread.
    fn charge_tables(&self) {
        if self.tables_charge.get().is_some() {
            return;
        }
        let bits = self.bits.get().map_or(0, |b| b.capacity() * core::mem::size_of::<AtomicU64>());
        let starts = self.frame_starts.get().map_or(0, |f| f.capacity() * core::mem::size_of::<u32>());
        if bits + starts > 0 {
            let _ = self.tables_charge.set(crate::partcache::holder_charge((bits + starts) as u64));
        }
    }

    pub(crate) fn new() -> BlockLazyPayload {
        BlockLazyPayload {
            held: pgsync::OnceLock::new(),
            cell: Mutex::new(None),
            tables_charge: pgsync::OnceLock::new(),
            bits: pgsync::OnceLock::new(),
            frame_starts: pgsync::OnceLock::new(),
            nblocks: AtomicUsize::new(0),
            ptr: AtomicPtr::new(core::ptr::null_mut()),
            len: AtomicUsize::new(0),
            resident: AtomicUsize::new(0),
            gov: Mutex::new(crate::partcache::Charged { slot: 0, epoch: 0, bytes: 0 }),
            blocks_resident: AtomicUsize::new(0),
        }
    }

    /// [sqe8blk] Every block published (whole-fallback counts as its one
    /// block). All publications serialize on the cell lock and bump
    /// `blocks_resident` with Release AFTER the bit + bytes, so a true
    /// answer here proves every block's bytes are visible to the caller.
    /// D2985's use-after-release check: fill the image's owned backing with 0xDB (bytes poisoned; 0 when the image is
    /// rooted by a region rather than owned, or not faulted).
    fn poison(&self) -> u64 {
        let mut g = self.cell.lock().unwrap_or_else(|e| e.into_inner());
        match g.as_mut() {
            Some(st) if !st.buf.is_empty() => {
                st.buf.fill(0xDBDB_DBDB_DBDB_DBDB);
                (st.buf.len() * 8) as u64
            }
            _ => 0,
        }
    }

    /// Some block is resident: a handle opened over it re-serves the shared image.
    pub(crate) fn any_resident(&self) -> bool {
        self.blocks_resident.load(Ordering::Acquire) > 0
    }

    pub(crate) fn fully_resident(&self) -> bool {
        let nb = self.nblocks.load(Ordering::Acquire);
        nb > 0 && self.blocks_resident.load(Ordering::Acquire) == nb
    }

    fn get(&self) -> Option<(*const u8, usize)> {
        let p = self.ptr.load(Ordering::Acquire);
        if p.is_null() {
            return None;
        }
        Some((p as *const u8, self.len.load(Ordering::Acquire)))
    }

    #[inline]
    fn block_resident(&self, b: usize) -> bool {
        match self.bits.get() {
            Some(bits) => bits[b / 64].load(Ordering::Acquire) & (1u64 << (b % 64)) != 0,
            None => false,
        }
    }

    /// Whether dict `frame`'s block is published (a resident block needs no
    /// fault, so it never counts toward the bytes-touched election).
    #[inline]
    fn frame_resident(&self, frame: u32) -> bool {
        !self.ptr.load(Ordering::Acquire).is_null() && self.block_resident(self.block_of_frame(frame))
    }

    /// Map a dict frame to its block ordinal (1:1 on valid geometry; the
    /// whole-fallback path has one all-set block).
    #[inline]
    fn block_of_frame(&self, frame: u32) -> usize {
        let nb = self.nblocks.load(Ordering::Acquire);
        (frame as usize).min(nb.saturating_sub(1))
    }

    /// [idx251] Payload-relative `[lo, hi)` byte span of the block backing
    /// dict `frame`, for per-block bounding of the lazy read. `plen` is the
    /// published payload window length (the last block runs to it). `None`
    /// on the whole-fallback path (`frame_starts` unset) — there the whole
    /// window IS the published window, so no per-block bound applies.
    #[inline]
    fn block_span(&self, frame: u32, plen: usize) -> Option<(usize, usize)> {
        let ftab = self.frame_starts.get()?;
        if ftab.is_empty() {
            return None;
        }
        let b = self.block_of_frame(frame);
        let lo = *ftab.get(b)? as usize;
        let hi = match ftab.get(b + 1) {
            Some(&h) => h as usize,
            None => plen,
        };
        Some((lo, hi))
    }

    /// Initialize the state under the cell lock: head pread (header +
    /// block-offset table), tail pread (frame table), geometry validation,
    /// region reservation. Falls back to the whole-region unwrap on any
    /// geometry the block path cannot prove.
    fn init_state(
        &self,
        part: &Arc<OpenPart>,
        unwrappers: &[&dyn crate::SectionUnwrapper],
        ps: &ParsedStream,
        what: &'static str,
        index: &[u8],
    ) -> ReadResult<BlockLazyState> {
        let rec = &ps.extents[0];
        let try_blocks = || -> ReadResult<Option<BlockLazyState>> {
            let head_len = rec.len.min(4096);
            if (head_len as usize) < STREAM_SECTION_HDR_LEN + 8 {
                return Ok(None);
            }
            let head = part.subrange_bytes(rec, 0, head_len, what)?;
            let hdr = StreamSectionHdr::decode(head.bytes())?;
            if hdr.encoding != ps.entry.encoding || hdr.wrapper != ps.entry.wrapper {
                return Err(ReadError::Format(FormatError::Corrupt { at: what }));
            }
            let unc_len = hdr.uncompressed_len as usize;
            if hdr.frame_table_off == 0 || unc_len == 0 {
                return Ok(None);
            }
            let payload_end = hdr.frame_table_off as usize;
            if payload_end < STREAM_SECTION_HDR_LEN + 8 || payload_end as u64 > rec.len {
                return Ok(None);
            }
            let first = u32::from_le_bytes(
                head.bytes()[STREAM_SECTION_HDR_LEN..STREAM_SECTION_HDR_LEN + 4]
                    .try_into()
                    .expect("len 4"),
            ) as usize;
            if first < 8 || first % 4 != 0 || first > payload_end - STREAM_SECTION_HDR_LEN {
                return Ok(None);
            }
            let wrapper = pgrc_format::enc::Wrapper::from_u8(hdr.wrapper)?;
            let nbt = first / 4 - 1;
            let nb = if wrapper.side_block() { nbt.saturating_sub(1) } else { nbt };
            if nb < 1 || hdr.frame_count as usize != nb {
                // Block/frame skew (or a single block): the partial win is
                // zero — whole assembly is strictly simpler.
                return Ok(None);
            }
            // Block-offset table: re-read if the fixed head missed it.
            let table = if STREAM_SECTION_HDR_LEN + first <= head.bytes().len() {
                head
            } else {
                part.subrange_bytes(
                    rec,
                    0,
                    (STREAM_SECTION_HDR_LEN + first) as u64,
                    what,
                )?
            };
            let tb = &table.bytes()[STREAM_SECTION_HDR_LEN..STREAM_SECTION_HDR_LEN + first];
            let btab: Vec<u32> = (0..=nbt)
                .map(|i| u32::from_le_bytes(tb[i * 4..i * 4 + 4].try_into().expect("len 4")))
                .collect();
            let comp_len = (payload_end - STREAM_SECTION_HDR_LEN) as u32;
            if btab.windows(2).any(|w| w[0] > w[1]) || *btab.last().expect("nonempty") > comp_len
            {
                return Ok(None);
            }
            // [idx136, D302] Refuse a declared uncompressed image the dict
            // index contradicts — before any image-sized reservation or
            // decompress. Typed, catchable; a crafted ~4 GiB unc_len over a
            // small extent stops here instead of reserving ~4 GiB. An image
            // past the block-lazy ceiling serves through the whole arm, which
            // applies the same law and reserves fallibly.
            let unc_len = dict_payload_unc_checked(unc_len, index)?;
            if unc_len > DICT_PAYLOAD_MAX_UNC_BYTES {
                return Ok(None);
            }
            // Frame table at the raw tail: uncompressed frame starts.
            let tail = part.subrange_bytes(
                rec,
                hdr.frame_table_off as u64,
                (nb * 4) as u64,
                what,
            )?;
            let ftab: Vec<u32> = (0..nb)
                .map(|i| {
                    u32::from_le_bytes(
                        tail.bytes()[i * 4..i * 4 + 4].try_into().expect("len 4"),
                    )
                })
                .collect();
            if ftab[0] != 0
                || ftab.windows(2).any(|w| w[0] > w[1])
                || *ftab.last().expect("nonempty") as usize > unc_len
            {
                return Ok(None);
            }
            let side = match wrapper.side_block() {
                false => None,
                true => {
                    let Some(u) = unwrappers.iter().find(|u| u.wrapper() == wrapper) else { return Ok(None) };
                    let (s0, s1) = (btab[nb] as u64, btab[nb + 1] as u64);
                    let sb = part.subrange_bytes(rec, STREAM_SECTION_HDR_LEN as u64 + s0, s1 - s0, what)?;
                    match u.decode_side(sb.bytes())? {
                        Some(t) => Some(t),
                        None => return Ok(None),
                    }
                }
            };
            // [idx136] Fallible reservation: even under the cap, an allocation
            // failure must be a typed refusal, not `handle_alloc_error`'s abort
            // (which the infallible `vec!` would take, bypassing statement-
            // thread panic containment).
            let buf = ImageBuf::zeroed(unc_len.div_ceil(8)).ok_or_else(|| {
                ReadError::Format(FormatError::Bounds {
                    at: "dict payload reservation",
                })
            })?;
            let tabs = ((btab.capacity() + ftab.capacity()) * core::mem::size_of::<u32>() + side.as_ref().map_or(0, |t| t.bytes)) as u64;
            Ok(Some(BlockLazyState {
                buf,
                len: unc_len,
                tabs_charge: Some(crate::partcache::holder_charge(tabs)),
                btab,
                ftab,
                side,
                whole_charge: None,
                whole: None,
            }))
        };
        if let Some(st) = try_blocks()? {
            let nb = st.ftab.len();
            let _ = self.bits.set((0..nb.div_ceil(64)).map(|_| AtomicU64::new(0)).collect());
            // [idx251] Publish the frame-start table BEFORE the ptr so a
            // reader that sees a non-null ptr (Acquire) also sees it — the
            // read path bounds each entry to its published block via
            // `block_span`.
            let _ = self.frame_starts.set(st.ftab.clone());
            self.charge_tables();
            self.nblocks.store(nb, Ordering::Release);
            self.len.store(st.len, Ordering::Release);
            self.ptr.store(st.buf.as_ptr() as *mut u8, Ordering::Release);
            return Ok(st);
        }
        // Whole-region fallback: exactly today's assembly (CRC-validated
        // whole extent + whole unwrap), published as one all-set block.
        let region = load_region_with(part, unwrappers, ps, what, Some(index))?;
        let payload = region.payload();
        let (rptr, rlen) = (payload.as_ptr() as *mut u8, payload.len());
        DICT_WHOLE_FAULT_BYTES.fetch_add(rlen as u64, core::sync::atomic::Ordering::Relaxed);
        self.charge(part, ps, rlen);
        let st = BlockLazyState {
            buf: ImageBuf::empty(),
            len: rlen,
            tabs_charge: None,
            btab: Vec::new(),
            ftab: Vec::new(),
            side: None,
            whole_charge: Some(crate::partcache::holder_charge(rlen as u64)),
            whole: Some(region),
        };
        let _ = self.bits.set(vec![AtomicU64::new(1)]);
        self.charge_tables();
        self.nblocks.store(1, Ordering::Release);
        self.blocks_resident.store(1, Ordering::Release);
        self.len.store(rlen, Ordering::Release);
        self.ptr.store(rptr, Ordering::Release);
        Ok(st)
    }

    /// Ensure the block backing dict frame `frame` is resident: one
    /// sub-extent pread of the compressed block + a one-block decompress
    /// into the reserved region. Returns `true` iff THIS call faulted a
    /// block at block grain (a sub-extent pread happened) — the [sqe8blk]
    /// bytes-touched election input.
    fn ensure_frame_block(
        &self,
        part: &Arc<OpenPart>,
        unwrappers: &[&dyn crate::SectionUnwrapper],
        ps: &ParsedStream,
        frame: u32,
        what: &'static str,
        index: &[u8],
    ) -> ReadResult<bool> {
        if self.frame_resident(frame) {
            return Ok(false);
        }
        let mut cell = lock_cell(&self.cell)?;
        if cell.is_none() {
            *cell = Some(self.init_state(part, unwrappers, ps, what, index)?);
        }
        let state = cell.as_mut().expect("just stored");
        if state.whole.is_some() {
            return Ok(false); // whole-fallback: everything is resident
        }
        let nb = state.ftab.len();
        let b = (frame as usize).min(nb - 1);
        if self.block_resident(b) {
            return Ok(false);
        }
        self.fault_block(part, unwrappers, ps, state, b, what, index)
    }

    /// Fault ONE block (cell lock held, bit b clear): sub-extent pread +
    /// one-block decompress into the unpublished span, then publish bit b.
    /// Returns `true` on a block-grain fault, `false` when the binding
    /// forced the whole-fallback (everything resident afterwards).
    fn fault_block(
        &self,
        part: &Arc<OpenPart>,
        unwrappers: &[&dyn crate::SectionUnwrapper],
        ps: &ParsedStream,
        state: &mut BlockLazyState,
        b: usize,
        what: &'static str,
        index: &[u8],
    ) -> ReadResult<bool> {
        let nb = state.ftab.len();
        let (c0, c1) = (state.btab[b] as usize, state.btab[b + 1] as usize);
        let f0 = state.ftab[b] as usize;
        let f1 = if b + 1 < nb { state.ftab[b + 1] as usize } else { state.len };
        let rec = &ps.extents[0];
        let seg = part.subrange_bytes(
            rec,
            (STREAM_SECTION_HDR_LEN + c0) as u64,
            (c1 - c0) as u64,
            what,
        )?;
        let wrapper = pgrc_format::enc::Wrapper::from_u8(ps.entry.wrapper)?;
        let u = unwrappers
            .iter()
            .find(|u| u.wrapper() == wrapper)
            .ok_or(ReadError::Format(FormatError::WrapperUnsupported {
                wrapper: wrapper.as_u8(),
            }))?;
        // SAFETY: raw-pointer slice into the never-reallocated, cell-owned
        // buffer; the target span is UNPUBLISHED (bit b clear), no shared
        // reference reads unpublished bytes (readers gate on the bit,
        // Acquire), and all writes serialize on the cell lock.
        let dst = unsafe {
            core::slice::from_raw_parts_mut(
                (state.buf.as_mut_ptr() as *mut u8).add(f0),
                f1 - f0,
            )
        };
        let recs = pgrc_format::dict::dict_frame_records(index, b as u32)?;
        let grain = match &state.side {
            Some(t) => u.unwrap_dict_payload_block_side(t, seg.bytes(), dst, recs)?,
            None => u.unwrap_dict_payload_block(seg.bytes(), dst, recs)?,
        };
        if !grain {
            // Binding cannot serve block grain: whole-fallback ON TOP of
            // the reservation — decompress everything via the whole path
            // and copy the payload into the reserved buffer (bits all set).
            let region = load_region_with(part, unwrappers, ps, what, Some(index))?;
            let payload = region.payload();
            if payload.len() != state.len {
                return Err(ReadError::Format(FormatError::Corrupt { at: what }));
            }
            let base = state.buf.as_mut_ptr() as *mut u8;
            // SAFETY: same discipline as above; unpublished spans only —
            // published block bytes are IDENTICAL by determinism (spec
            // §11), so overwriting them with equal bytes is benign, but we
            // still only touch unpublished spans to keep the law simple:
            // copy the whole image before any bit beyond b is published.
            // bounded("other: one copy of the region the whole-fallback just rebuilt, at most DICT_PAYLOAD_MAX_UNC_BYTES (1 GiB): a memcpy with first-touch faults, about 0.2 s at the cap; its bits publish only after the whole copy, so a stop between blocks would leave blocks charged and unpublished")
            for blk in 0..nb {
                if self.block_resident(blk) {
                    continue;
                }
                let s0 = state.ftab[blk] as usize;
                let s1 = if blk + 1 < nb { state.ftab[blk + 1] as usize } else { state.len };
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        payload.as_ptr().add(s0),
                        base.add(s0),
                        s1 - s0,
                    );
                }
                self.charge(part, ps, s1 - s0);
            }
            DICT_WHOLE_FAULT_BYTES
                .fetch_add(payload.len() as u64, core::sync::atomic::Ordering::Relaxed);
            let bits = self.bits.get().expect("published at init");
            // bounded("other: the region's block bits, one store per 64 blocks of a region of at most 1 GiB (DICT_PAYLOAD_MAX_UNC_BYTES); all of them publish together")
            for w in bits {
                w.store(u64::MAX, Ordering::Release);
            }
            self.blocks_resident.store(nb, Ordering::Release);
            return Ok(false);
        }
        self.charge(part, ps, f1 - f0);
        DICT_BLOCK_FAULT_BYTES
            .fetch_add((c1 - c0) as u64, core::sync::atomic::Ordering::Relaxed);
        DICT_BLOCK_FAULTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let bits = self.bits.get().expect("published at init");
        bits[b / 64].fetch_or(1u64 << (b % 64), Ordering::Release);
        self.blocks_resident.fetch_add(1, Ordering::Release);
        Ok(true)
    }

    /// Bulk publication (the C7 prewarm / dense-consumer path): ONE
    /// CRC-validated whole-extent read, then decompress every unresident
    /// block — the orderly whole-region economics of today's arm, into the
    /// same block-grain publication.
    fn ensure_all(
        &self,
        part: &Arc<OpenPart>,
        unwrappers: &[&dyn crate::SectionUnwrapper],
        ps: &ParsedStream,
        what: &'static str,
        index: &[u8],
    ) -> ReadResult<()> {
        let mut cell = lock_cell(&self.cell)?;
        if cell.is_none() {
            *cell = Some(self.init_state(part, unwrappers, ps, what, index)?);
        }
        let state = cell.as_mut().expect("just stored");
        if state.whole.is_some() {
            return Ok(());
        }
        let nb = state.ftab.len();
        if (0..nb).all(|b| self.block_resident(b)) {
            return Ok(());
        }
        // Whole raw extent: CRC-validated, in-flight-registered (plays the
        // C6 game with the claim-horizon issuers) but NOT retained in the
        // part cache — the bytes immediately decompress into the block
        // image below, and caching the raw extent alongside doubled the
        // dense first-touch memory traffic (+361 ms on a dense dictionary
        // scan). A raw image some other
        // consumer already cached (or has in flight) is still served from
        // the cache — hit costs nothing extra. PGRUST_PGRC_BLOCKLAZY_
        // REFINE=0 = the densedict-measured cached arm (part-old A/B).
        let raw = if blocklazy_refine() {
            part.extent_bytes_uncached(&ps.entry, &ps.extents[0], 0)?
        } else {
            part.extent_bytes(&ps.entry, &ps.extents[0], 0)?
        };
        let wrapper = pgrc_format::enc::Wrapper::from_u8(ps.entry.wrapper)?;
        let u = unwrappers
            .iter()
            .find(|u| u.wrapper() == wrapper)
            .ok_or(ReadError::Format(FormatError::WrapperUnsupported {
                wrapper: wrapper.as_u8(),
            }))?;
        let base = state.buf.as_mut_ptr() as *mut u8;
        let bits = self.bits.get().expect("published at init");
        let mut block_grain = true;
        let mut step = cancelpoll::step("pgrc_read dict ensure_all: a dictionary's blocks");
        for b in 0..nb {
            if self.block_resident(b) {
                continue;
            }
            // D3973: a block at a time, the statement's abort polled before each (a whole prewarm held a build 419 ms);
            // the blocks published stay resident
            step.tick()?;
            let (c0, c1) = (state.btab[b] as usize, state.btab[b + 1] as usize);
            let f0 = state.ftab[b] as usize;
            let f1 = if b + 1 < nb { state.ftab[b + 1] as usize } else { state.len };
            let src = &raw.bytes()[STREAM_SECTION_HDR_LEN + c0..STREAM_SECTION_HDR_LEN + c1];
            // SAFETY: unpublished span under the cell lock (same law as
            // fault_block).
            let dst = unsafe { core::slice::from_raw_parts_mut(base.add(f0), f1 - f0) };
            let recs = pgrc_format::dict::dict_frame_records(index, b as u32)?;
            let grain = match &state.side {
                Some(t) => u.unwrap_dict_payload_block_side(t, src, dst, recs)?,
                None => u.unwrap_dict_payload_block(src, dst, recs)?,
            };
            if !grain {
                block_grain = false;
                break;
            }
            self.charge(part, ps, f1 - f0);
            bits[b / 64].fetch_or(1u64 << (b % 64), Ordering::Release);
            self.blocks_resident.fetch_add(1, Ordering::Release);
        }
        if !block_grain {
            // Binding without block decode: whole unwrap + copy (bits set).
            let region = load_region_with(part, unwrappers, ps, what, Some(index))?;
            let payload = region.payload();
            if payload.len() != state.len {
                return Err(ReadError::Format(FormatError::Corrupt { at: what }));
            }
            for b in 0..nb {
                if self.block_resident(b) {
                    continue;
                }
                // a block a tick: each block publishes as it is copied, so the blocks copied stay resident
                step.tick()?;
                let f0 = state.ftab[b] as usize;
                let f1 = if b + 1 < nb { state.ftab[b + 1] as usize } else { state.len };
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        payload.as_ptr().add(f0),
                        base.add(f0),
                        f1 - f0,
                    );
                }
                self.charge(part, ps, f1 - f0);
                bits[b / 64].fetch_or(1u64 << (b % 64), Ordering::Release);
                self.blocks_resident.fetch_add(1, Ordering::Release);
            }
        }
        DICT_WHOLE_FAULT_BYTES
            .fetch_add(state.len as u64, core::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

/// The payload region arm elected at open: frame-lazy on the SB-7
/// unwrapped multi-extent geometry, whole-fault otherwise (single-extent
/// legacy/small dicts, wrapped payloads — O-CMP-5(a) unwrap is
/// whole-section by design — and the kill-switch arm).
enum PayloadRegion {
    Whole(LazyRegion),
    FrameLazy(FrameLazyPayload),
    FrameDirect(FrameDirectPayload),
    /// Arc'd so the image can be PART-grain shared (the §4.2 re-pose);
    /// the handle-grain A/B arm holds a private Arc.
    BlockLazy(Arc<BlockLazyPayload>),
}

/// The reader-level lazy dict handle. `&self` faces throughout (dict lanes
/// are consumed concurrently under claim scope); internal publication is
/// write-once.
pub struct DictHandle {
    /// Option-C (`STREAMF_CHARLEN_ABSENT`): the load-time one-pass
    /// per-code char-length table, built on the FIRST `lengths()` call
    /// (one UTF-8 lead-count walk over every entry — the index-only/
    /// no-payload-fault contract is deviated under this form BY DESIGN;
    /// the M5d length-lanes consumer shares this pass). Empty vec until
    /// built; write-once.
    /// D4482 b: the table's bytes on the part cache's account from its build (dropped before it frees).
    char_table_charge: pgsync::OnceLock<memledger::Charge>,
    /// D4482 b: the memo's bytes on the part cache's account from its allocation (dropped before it frees).
    fc_memo_charge: pgsync::OnceLock<memledger::Charge>,
    part: Arc<OpenPart>,
    /// Registry-mediated opens keep the part pinned for the handle's life
    /// (the lane that hands out codes must keep its payload region
    /// resident).
    _pin: Option<PartPin>,
    /// Section unwrappers for wrapped dict streams (CMP-B: the byte-run
    /// stream class takes the wrapper offer). `'static` like every
    /// production binding; adjudicated at open (refusal-before-fault).
    unwrappers: &'static [&'static dyn crate::SectionUnwrapper],
    attno: u32,
    path_ord: u32,
    entry_count: u32,
    exec_publishable: bool,
    exec_nullable: bool,
    column_all_valid: bool,
    /// The DictIndex `char_field` form (spec §6.3 flags; M5d char-len
    /// record): Absolute / Delta (reconstructed, typed corruption on
    /// underflow) / Absent (Option-C: recomputed — see `char_table`).
    /// Self-described per stream — never a process posture.
    charlen_form: pgrc_format::dict::DictCharLenForm,
    char_table: pgsync::OnceLock<Vec<u32>>,
    epoch64: u64,
    index_stream: ParsedStream,
    payload_stream: ParsedStream,
    index: LazyRegion,
    payload: PayloadRegion,
    /// [sqe8blk] Dense/sparse election is LIVE on this handle — true only
    /// on the PART-grain block-lazy arm (`=part`). The `=handle` A/B arm
    /// and every non-block arm keep their measured behavior unchanged.
    elect: bool,
    /// [sqe8blk] Declared touch class (TOUCH_*; write-once, first wins).
    touch: AtomicU8,
    /// [sqe8blk] The all-resident short-circuit: once true, `ensure_frame`
    /// on the block-lazy arm is one Acquire load (set with the shared
    /// image's full publication proven — see `fully_resident`).
    payload_all_resident: AtomicBool,
    /// [sqe8blk] Block faults THIS handle's demands caused (the
    /// bytes-touched election input; shared-image hits don't count).
    demand_block_faults: AtomicU32,
    /// The bytes-touched election's fault count (`stream_after`), derived at
    /// the handle's first undeclared demand; 0 = not yet.
    stream_after: AtomicUsize,
    /// Lane dictresident: the payload region holds the front-coded form (a FrontZstd stream
    /// under `crate::dict_resident_frontcoded`); `entry` materialises through `fc_memo` (a
    /// pointer per code, 0 = not yet) into `fc_arena`, `entries` walks in place.
    fc: bool,
    fc_memo: pgsync::OnceLock<Box<[AtomicU64]>>,
    fc_arena: Mutex<FcArena>,
}

impl DictHandle {
    /// cache plan C2 / C5: a reader took this handle from a cache that keeps it (a part list's warm cell) and reads its
    /// images with no part cache lookup: one sampled lookup of each of its index and payload extents at the part cache's
    /// door, keyed as `OpenPart`'s own doors key them (`memledger::value::lookup`: thread-local, sampled). The payload is
    /// held decoded, so its lookups carry what the cache holds for it, its resident uncompressed bytes
    /// ([`Self::resident_payload_bytes`], the census term), not its stored bytes ([`payload_door_bytes`]). Off
    /// (`cache_track` 0): one relaxed load.
    pub fn note_reads(&self) {
        if memledger::value::track_of(memledger::Consumer::PartCache) == 0 {
            return;
        }
        let k = self.part.value_key();
        // bounded("catalog-sized: a dictionary's index extents, a few per handle")
        for rec in &self.index_stream.extents {
            memledger::value::lookup(memledger::Consumer::PartCache, k ^ rec.file_off.rotate_left(32), rec.len);
        }
        let stored: u64 = self.payload_stream.extents.iter().map(|r| r.len).sum();
        let held = self.resident_payload_bytes();
        // bounded("catalog-sized: a dictionary's payload extents, a few per handle")
        for rec in &self.payload_stream.extents {
            memledger::value::lookup(memledger::Consumer::PartCache, k ^ rec.file_off.rotate_left(32), payload_door_bytes(rec.len, stored, held));
        }
    }

    /// The bytes-touched election's fault count (see [`DictTouch`], input 2):
    /// `Device::faults_to_stream` of the payload's mean block, `nb` blocks
    /// over its one extent, in the device regime of that extent's bytes.
    /// Derived once per handle (one cachestat), then one load.
    fn stream_after(&self, nb: usize) -> usize {
        let d = self.stream_after.load(Ordering::Relaxed);
        if d != 0 {
            return d;
        }
        let d = match self.payload_stream.extents.first() {
            Some(rec) if nb > 0 => {
                let n = Device::at_residency(self.part.page_resident(rec.file_off, rec.len)).faults_to_stream(nb as u64, rec.len / nb as u64);
                usize::try_from(n).unwrap_or(usize::MAX).max(1)
            }
            // open builds a block-lazy payload over exactly one extent and the caller holds nb > 0, so this arm is never
            // taken; it streams at the first block read, the whole-region behaviour of every other payload
            _ => 1,
        };
        self.stream_after.store(d, Ordering::Relaxed);
        d
    }
}

/// cache plan C2 / C5: the bytes a dictionary payload extent's lookup carries at the part cache's door: `len` stored bytes of
/// the payload's `stored`, scaled up to the payload's `held` resident uncompressed bytes once they exceed the stored ones
/// (a decoded payload the cache holds is larger than its compressed image), never below `len`. Lane cachepolicy's census
/// (10-16): a scan suite's warm dictionaries hold 9.0 GiB decoded, every hot pass reads all of them.
pub(crate) fn payload_door_bytes(len: u64, stored: u64, held: u64) -> u64 {
    if stored == 0 || held <= stored {
        return len;
    }
    u64::try_from(u128::from(len) * u128::from(held) / u128::from(stored)).unwrap_or(u64::MAX)
}

/// [idx251] Resolve an entry from a slice bounded to its PUBLISHED
/// frame/block (the fix for the lazy-window data race): `seg` is exactly the
/// bytes of the published window (frame extent or block), `seg_base` is that
/// window's start in the same coordinate space as `payload_start`, and
/// `payload_end` is the window's exclusive upper bound. `payload_off` (from
/// untrusted index bytes) is validated to land at/above the window start and
/// below its end — so the varlena read cannot reference bytes outside the
/// published window (which other threads may still be writing). Typed,
/// catchable refusals (`ERRCODE_DATA_CORRUPTED`) on any violation.
fn present_bounded_entry<'a>(
    seg: &'a [u8],
    seg_base: usize,
    payload_start: usize,
    payload_end: usize,
    payload_off: usize,
    byte_len: u32,
    char_field: u32,
    charlen_form: pgrc_format::dict::DictCharLenForm,
) -> ReadResult<DictEntryRef<'a>> {
    let region_off = payload_start + payload_off;
    if region_off >= payload_end {
        return Err(ReadError::Format(FormatError::Bounds { at: "dict payload" }));
    }
    // Below the window start ⇒ the entry claims another (possibly
    // unpublished) frame/block: refuse before any read.
    let lo = region_off
        .checked_sub(seg_base)
        .ok_or(ReadError::Format(FormatError::Bounds { at: "dict payload" }))?;
    let (image, bytes) = pgrc_format::wire::varlena_entry_at(seg, lo, "dict payload")?;
    if bytes.len() != byte_len as usize {
        return Err(ReadError::Format(FormatError::Corrupt {
            at: "dict entry byte_len",
        }));
    }
    use pgrc_format::dict::DictCharLenForm;
    let char_len = match charlen_form {
        DictCharLenForm::Absolute => char_field,
        DictCharLenForm::Delta => byte_len.checked_sub(char_field).ok_or(
            ReadError::Format(FormatError::Corrupt {
                at: "dict entry char_len delta",
            }),
        )?,
        DictCharLenForm::Absent => pgrc_format::dict::utf8_char_count(bytes),
    };
    Ok(DictEntryRef {
        image,
        bytes,
        byte_len,
        char_len,
    })
}

impl DictHandle {
    /// Open the handle for (attno, path_ord). Faults at most the StreamDir
    /// section (directory lookup); dict sections stay unfaulted until
    /// [`DictHandle::ensure_frame`] / [`DictHandle::entry`] /
    /// [`DictHandle::lengths`] ask for them — the lazy contract the fault
    /// tests pin.
    pub fn open(
        part: Arc<OpenPart>,
        pin: Option<PartPin>,
        unwrappers: &'static [&'static dyn crate::SectionUnwrapper],
        attno: u32,
        path_ord: u32,
    ) -> ReadResult<DictHandle> {
        DictHandle::open_with_mode(part, pin, unwrappers, attno, path_ord, DictFaultMode::from_env())
    }

    /// [`DictHandle::open`] with the SB-7 fault mode stated explicitly —
    /// the probe face (unit tests and the harness born-RED arm select
    /// `WholeFault` without touching process env).
    pub fn open_with_mode(
        part: Arc<OpenPart>,
        pin: Option<PartPin>,
        unwrappers: &'static [&'static dyn crate::SectionUnwrapper],
        attno: u32,
        path_ord: u32,
        mode: DictFaultMode,
    ) -> ReadResult<DictHandle> {
        DictHandle::open_with_arms(part, pin, unwrappers, attno, path_ord, mode, DictFrameServe::from_env())
    }

    /// [`DictHandle::open_with_mode`] with the frame-serve arm ALSO stated
    /// explicitly (env-independent — the arm-specific pins in
    /// `tests/dict.rs` select `Copy`/`Direct` without touching process env).
    pub fn open_with_arms(
        part: Arc<OpenPart>,
        pin: Option<PartPin>,
        unwrappers: &'static [&'static dyn crate::SectionUnwrapper],
        attno: u32,
        path_ord: u32,
        mode: DictFaultMode,
        serve: DictFrameServe,
    ) -> ReadResult<DictHandle> {
        DictHandle::open_elected(part, pin, unwrappers, attno, path_ord, mode, serve, None)
    }

    /// [`DictHandle::open`] with the consumer's touch class declared AT OPEN
    /// (see [`DictTouch`]). `Sparse` elects the PART-grain block-lazy arm for
    /// a wrapped single-extent payload whatever the process-wide
    /// `PGRUST_PGRC_DICT_BLOCK_LAZY` arm says: a point-probe consumer (the
    /// dictionary heads of a top-k) faults the blocks its entries live in
    /// and never streams the section whole. `Dense` is `open` with the
    /// declaration made (the whole stream, as every undeclared open).
    pub fn open_touch(
        part: Arc<OpenPart>,
        pin: Option<PartPin>,
        unwrappers: &'static [&'static dyn crate::SectionUnwrapper],
        attno: u32,
        path_ord: u32,
        touch: DictTouch,
    ) -> ReadResult<DictHandle> {
        DictHandle::open_elected(part, pin, unwrappers, attno, path_ord, DictFaultMode::from_env(), DictFrameServe::from_env(), Some(touch))
    }

    #[allow(clippy::too_many_arguments)]
    fn open_elected(
        part: Arc<OpenPart>,
        pin: Option<PartPin>,
        unwrappers: &'static [&'static dyn crate::SectionUnwrapper],
        attno: u32,
        path_ord: u32,
        mode: DictFaultMode,
        serve: DictFrameServe,
        touch: Option<DictTouch>,
    ) -> ReadResult<DictHandle> {
        part.note_stream_use(attno, path_ord);
        let sparse = matches!(touch, Some(DictTouch::Sparse | DictTouch::Probe));
        let dir = part.stream_directory()?;
        let index_stream = dir
            .lookup(attno, path_ord, StreamRole::DictIndex)
            .ok_or(ReadError::StreamMissing {
                attno,
                path_ord,
                role: StreamRole::DictIndex.as_u8(),
            })?
            .clone();
        let payload_stream = dir
            .lookup(attno, path_ord, StreamRole::DictPayload)
            .ok_or(ReadError::StreamMissing {
                attno,
                path_ord,
                role: StreamRole::DictPayload.as_u8(),
            })?
            .clone();
        // Refusal-before-fault (CMP-B, the cursor-open pin mirrored): a
        // wrapped dict stream whose arm this binding lacks refuses TYPED at
        // open — the lazy contract (StreamDir only) is preserved, and no
        // payload fault can precede the refusal.
        for ps in [&index_stream, &payload_stream] {
            let w = pgrc_format::enc::Wrapper::from_u8(ps.entry.wrapper)?;
            if w != pgrc_format::enc::Wrapper::None
                && !unwrappers.iter().any(|u| u.wrapper() == w)
            {
                return Err(ReadError::Format(FormatError::WrapperUnsupported {
                    wrapper: w.as_u8(),
                }));
            }
        }
        let entry_count = u32::try_from(index_stream.entry.values).map_err(|_| {
            ReadError::Format(FormatError::Corrupt {
                at: "dict entry count",
            })
        })?;
        // Publishability facts (spec §7): the lattice flag lives on the
        // values stream; the O-6 zero-null proof is "no Validity stream".
        let values = dir.lookup(attno, path_ord, StreamRole::Values);
        let exec_publishable = values
            .map(|v| v.entry.flags & STREAMF_DICT_EXEC != 0)
            .unwrap_or(false);
        let exec_nullable = values
            .map(|v| v.entry.flags & STREAMF_DICT_EXEC_NULLABLE != 0)
            .unwrap_or(false);
        let column_all_valid = dir.lookup(attno, path_ord, StreamRole::Validity).is_none();
        let epoch64 = part.dict_epoch(attno, path_ord);
        // SB-7 arm election (M3-L3): frame-lazy on the v4 writer's
        // unwrapped multi-extent frame geometry; whole-fault for
        // single-extent payloads (legacy grain / sub-frame dicts), wrapped
        // payloads (O-CMP-5(a): unwrap is whole-section by design), and
        // the PGRUST_PGRC_FRAME_LAZY=0 kill-switch arm (the born-RED
        // reproduction path the residency probe consumes).
        let payload_wrapper = pgrc_format::enc::Wrapper::from_u8(payload_stream.entry.wrapper)?;
        let fc = payload_wrapper == Wrapper::FrontZstd && crate::dict_resident_frontcoded();
        let payload = if fc {
            // the front-coded form is served whole (the block-lazy arms expand per block)
            PayloadRegion::Whole(LazyRegion::new())
        } else if payload_stream.extents.len() > 1
            && payload_wrapper == Wrapper::None
            && mode == DictFaultMode::FrameLazy
        {
            if serve == DictFrameServe::Direct {
                PayloadRegion::FrameDirect(FrameDirectPayload::new(payload_stream.extents.len()))
            } else {
                PayloadRegion::FrameLazy(FrameLazyPayload::new(payload_stream.extents.len()))
            }
        } else if payload_stream.extents.len() == 1
            && payload_wrapper != Wrapper::None
            && mode == DictFaultMode::FrameLazy
            && (sparse || block_lazy() != BlockLazyGrain::Off)
        {
            // [stack] Wrapped single-extent geometry (the banks of record):
            // block-lazy partial unwrap (RESULTS-FMTLAND §B.3). Invalid
            // block geometry falls back to whole assembly at first fault.
            // [densedict] Part grain shares the image through the part
            // cache (reopens re-serve, never re-decompress); handle grain
            // is the born-negative A/B arm.
            match block_lazy() {
                BlockLazyGrain::Part | BlockLazyGrain::Off => {
                    let rec = &payload_stream.extents[0];
                    let bl = part.dict_block_payload((rec.file_off, rec.len));
                    // cache plan C2 / C5: a reopen re-serves the part's shared payload image with no part cache lookup
                    // (its blocks fault through the doors once); count one, keyed as the extent's door keys it, of the
                    // decoded bytes the image holds
                    if bl.any_resident() {
                        let bytes = payload_door_bytes(rec.len, rec.len, bl.resident_bytes());
                        memledger::value::lookup(memledger::Consumer::PartCache, part.value_key() ^ rec.file_off.rotate_left(32), bytes);
                    }
                    PayloadRegion::BlockLazy(bl)
                }
                BlockLazyGrain::Handle => PayloadRegion::BlockLazy(Arc::new(BlockLazyPayload::new())),
            }
        } else {
            PayloadRegion::Whole(LazyRegion::new())
        };
        // [sqe8blk] Election is live only on the part-grain arm; a reopen
        // that finds the shared image fully published starts on the
        // all-resident fast path (the §3.1 hot-probe refinement).
        let (elect, all_resident) = match &payload {
            PayloadRegion::BlockLazy(bl)
                if (sparse || block_lazy() == BlockLazyGrain::Part) && blocklazy_refine() =>
            {
                (true, bl.fully_resident())
            }
            _ => (false, false),
        };
        let charlen_form =
            pgrc_format::dict::DictCharLenForm::from_flags(index_stream.entry.flags)?;
        Ok(DictHandle {
            part,
            _pin: pin,
            unwrappers,
            attno,
            path_ord,
            entry_count,
            exec_publishable,
            exec_nullable,
            column_all_valid,
            charlen_form,
            char_table_charge: pgsync::OnceLock::new(),
            char_table: pgsync::OnceLock::new(),
            epoch64,
            index_stream,
            payload_stream,
            index: LazyRegion::new(),
            payload,
            elect,
            touch: AtomicU8::new(match touch {
                None | Some(DictTouch::Probe) => TOUCH_UNDECLARED,
                Some(DictTouch::Dense) => TOUCH_DENSE,
                Some(DictTouch::Sparse) => TOUCH_SPARSE,
            }),
            payload_all_resident: AtomicBool::new(all_resident),
            demand_block_faults: AtomicU32::new(0),
            stream_after: AtomicUsize::new(0),
            fc,
            fc_memo_charge: pgsync::OnceLock::new(),
            fc_memo: pgsync::OnceLock::new(),
            fc_arena: Mutex::new(FcArena { charge: None, chunks: Vec::new(), used: 0 }),
        })
    }

    /// [sqe8blk] Declare this handle's touch class (RESULTS-DENSEDICT
    /// §3.1 refinement 2 — see [`DictTouch`] for the election inputs).
    /// First declaration wins; undeclared handles use the bytes-touched
    /// fallback. A no-op on every arm but part-grain block-lazy.
    pub fn declare_touch(&self, t: DictTouch) {
        let v = match t {
            DictTouch::Dense => TOUCH_DENSE,
            DictTouch::Sparse => TOUCH_SPARSE,
            DictTouch::Probe => return,
        };
        let _ = self.touch.compare_exchange(
            TOUCH_UNDECLARED,
            v,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    /// Number of global codes (dict entries).
    /// D2985 (debug builds, a statement's released dictionary): poison the payload image when this handle is the last
    /// holder of it, so a datum still pointing into it reads 0xDB bytes. The part's cached images are evicted first when
    /// no statement uses the part (D3003 `users`). Only the part-grain / handle-grain block-lazy image is poisoned (the arm
    /// whose image the handle can own alone); bytes poisoned.
    pub fn debug_poison(&self) -> u64 {
        let PayloadRegion::BlockLazy(b) = &self.payload else { return 0 };
        if self.part.users() == 0 {
            self.part.evict_contents();
        }
        if Arc::strong_count(b) != 1 {
            return 0;
        }
        b.poison()
    }

    /// D4552 (a): the heap bytes its open allocated beside its images (the two streams' extent lists, a frame-grain
    /// payload's per-frame tables): what a holder that keeps the handle past its opener's statement charges (the warm
    /// cell's entry), so whichever thread drops it last takes them back. The images, the part-grain payload and the lazy
    /// tables carry their own charges.
    pub fn loose_bytes(&self) -> u64 {
        let ext = core::mem::size_of::<pgrc_format::part::ExtentRecord>();
        let streams = (self.index_stream.extents.capacity() + self.payload_stream.extents.capacity()) * ext;
        let payload = match &self.payload {
            PayloadRegion::FrameLazy(f) => f.frame_bits.capacity() * core::mem::size_of::<AtomicU64>(),
            PayloadRegion::FrameDirect(f) => f.segs.capacity() * core::mem::size_of::<pgsync::OnceLock<crate::openpart::SegBuf>>(),
            PayloadRegion::Whole(_) | PayloadRegion::BlockLazy(_) => 0,
        };
        (streams + payload) as u64
    }

    pub fn ncodes(&self) -> u32 {
        self.entry_count
    }

    /// The u64 epoch currency (minted per open-part instance — crate doc
    /// "Dict epochs"; equality certifies code-space identity).
    pub fn epoch64(&self) -> u64 {
        self.epoch64
    }

    /// The structural epoch identity (spec §7, Law A).
    pub fn epoch_key(&self) -> DictEpochKey {
        DictEpochKey {
            part_uuid: self.part.uuid(),
            attno: self.attno,
            path_ord: self.path_ord,
        }
    }

    /// `STREAMF_DICT_EXEC`: code-eq == value-eq holds; execution lanes may
    /// consume codes (spec §7 publishability lattice).
    pub fn exec_publishable(&self) -> bool {
        self.exec_publishable
    }

    /// `STREAMF_DICT_EXEC_NULLABLE` (P-a, the J1 nullable twin): code-eq == value-eq holds on every VALID row and the
    /// stream may carry NULL rows, so execution lanes may consume codes only beside the Validity plane (a NULL row's
    /// on-disk filler code is never a value). Parts sealed before the flag carry neither flag on a NULL-bearing stream.
    pub fn exec_nullable(&self) -> bool {
        self.exec_nullable
    }

    /// The exec form of this dictionary's codes: `Some(false)` = the zero-null `DICT_EXEC` form (no Validity stream),
    /// `Some(true)` = the nullable form (`DICT_EXEC_NULLABLE`; codes pair with the Validity plane), `None` = codes are
    /// storage-only (no exec flag, or `DICT_EXEC` beside a Validity stream: a flag / stream contradiction never serves).
    pub fn exec_form(&self) -> Option<bool> {
        if self.exec_publishable && self.column_all_valid {
            Some(false)
        } else if self.exec_nullable {
            Some(!self.column_all_valid)
        } else {
            None
        }
    }

    /// The O-6 zero-null proof input: the column stores no Validity stream
    /// in this part (spec §6.1 — absent = all-valid).
    pub fn column_all_valid(&self) -> bool {
        self.column_all_valid
    }

    /// Byte-rank sorted order is contractual for every stored dictionary
    /// (spec §7): global code order == byte order of the stored images.
    pub fn byte_rank_sorted(&self) -> bool {
        true
    }

    /// The dict frame a code lives in (spec §7 lazy-fault grain).
    pub fn frame_of_code(&self, code: u32) -> u32 {
        code / DICT_FRAME_ENTRIES
    }

    /// Ensure the sections backing dict frame `frame` are resident. The
    /// contractual grain is the dict frame, and since M3-L3 the REALIZED
    /// grain matches it on SB-7 geometry: the v4 writer cuts the UNWRAPPED
    /// DictPayload extent table at frame boundaries (per-frame CRCs), and
    /// the frame-lazy region reservation faults EXACTLY extent `frame`
    /// (plus extent 0, the header, once). Single-extent and wrapped
    /// payloads — and the `PGRUST_PGRC_FRAME_LAZY=0` born-RED arm — keep
    /// the v1 whole-region assembly. The index stream stays whole-resident
    /// by design (12-B stride, the small always-resident half).
    /// The resident v1 index image (faulted on first call; index-only).
    fn index_slice(&self) -> ReadResult<&[u8]> {
        let (iptr, ilen) = self.index.ensure(
            &self.part,
            self.unwrappers,
            &self.index_stream,
            "dict index stream",
            None,
        )?;
        // SAFETY: published from a payload slice of an `Arc<[u8]>` rooted in
        // `self.index.cell` for `self`'s lifetime; write-once, immutable.
        Ok(unsafe { core::slice::from_raw_parts(iptr, ilen) })
    }

    pub fn ensure_frame(&self, frame: u32) -> ReadResult<()> {
        let index = self.index_slice()?;
        match &self.payload {
            PayloadRegion::Whole(r) => {
                r.ensure(
                    &self.part,
                    self.unwrappers,
                    &self.payload_stream,
                    "dict payload stream",
                    Some(index),
                )?;
            }
            PayloadRegion::FrameLazy(fl) => {
                // On frame-cut geometry the frame ordinal IS the extent
                // ordinal (seal.rs dict_frame_extents: one extent per
                // frame, `granule_start` carries the frame ordinal).
                fl.ensure_extent(&self.part, &self.payload_stream, frame, "dict payload stream")?;
            }
            PayloadRegion::FrameDirect(fd) => {
                fd.ensure_extent(&self.part, &self.payload_stream, frame, "dict payload stream")?;
            }
            PayloadRegion::BlockLazy(bl) => {
                // [sqe8blk] Part-grain dense/sparse election (see
                // [`DictTouch`] for the inputs). Handle-grain (`=1`) keeps
                // the measured A/B behavior: always block grain.
                if !self.elect {
                    bl.ensure_frame_block(
                        &self.part,
                        self.unwrappers,
                        &self.payload_stream,
                        frame,
                        "dict payload stream",
                        index,
                    )?;
                    return Ok(());
                }
                // P5-18: a render of a block already resident faults nothing, so the election weighs only a demand
                // that would fault (else a handle at its fault count streams on a re-render of a block it holds)
                if self.payload_all_resident.load(Ordering::Acquire) || bl.frame_resident(frame) {
                    return Ok(());
                }
                let touch = self.touch.load(Ordering::Relaxed);
                let elected = touch == TOUCH_UNDECLARED && {
                    // Bytes-touched fallback: this handle's own demand
                    // faults vs the §6.4 block count (nblocks == 0
                    // before the first fault ⇒ stays block-grain).
                    let nb = bl.nblocks.load(Ordering::Acquire);
                    nb > 0 && self.demand_block_faults.load(Ordering::Relaxed) as usize >= self.stream_after(nb)
                };
                if elected {
                    // shapecheck fire signal (REGISTRY P5-08): the election streamed the rest
                    fires::fire_shared("dict.stream_rest");
                }
                if touch == TOUCH_DENSE || elected {
                    bl.ensure_all(
                        &self.part,
                        self.unwrappers,
                        &self.payload_stream,
                        "dict payload stream",
                        index,
                    )?;
                    self.payload_all_resident.store(true, Ordering::Release);
                    return Ok(());
                }
                let faulted = bl.ensure_frame_block(
                    &self.part,
                    self.unwrappers,
                    &self.payload_stream,
                    frame,
                    "dict payload stream",
                    index,
                )?;
                if faulted {
                    self.demand_block_faults.fetch_add(1, Ordering::Relaxed);
                }
                if bl.fully_resident() {
                    self.payload_all_resident.store(true, Ordering::Release);
                }
            }
        }
        Ok(())
    }

    /// SB-7 residency witnesses (the census terms the M3 residency probe
    /// and the memwatchdog line consume): faulted payload bytes on this
    /// handle. Whole-fault arms report the full region once assembled.
    pub fn resident_payload_bytes(&self) -> u64 {
        match &self.payload {
            PayloadRegion::Whole(r) => r.get().map(|(_, len)| len as u64).unwrap_or(0),
            PayloadRegion::FrameLazy(fl) => fl.resident.load(Ordering::Relaxed) as u64,
            PayloadRegion::FrameDirect(fd) => fd.resident.load(Ordering::Relaxed) as u64,
            PayloadRegion::BlockLazy(bl) => bl.resident.load(Ordering::Relaxed) as u64,
        }
    }

    /// Payload frames this handle serves at frame grain (0 = whole-fault
    /// arm; the witness the probe uses to prove the arm is live).
    pub fn frame_lazy_frames(&self) -> u32 {
        match &self.payload {
            PayloadRegion::Whole(_) => 0,
            PayloadRegion::FrameLazy(fl) => fl.frames,
            PayloadRegion::FrameDirect(fd) => fd.frames,
            // Block-lazy: block count once known (0 before the first fault).
            PayloadRegion::BlockLazy(bl) => bl.nblocks.load(Ordering::Acquire) as u32,
        }
    }

    /// Ensure the sections backing `code` are resident.
    pub fn ensure_code(&self, code: u32) -> ReadResult<()> {
        if code >= self.entry_count {
            return Err(ReadError::Format(FormatError::Bounds { at: "dict code" }));
        }
        self.ensure_frame(self.frame_of_code(code))
    }

    /// [stack] Orderly WHOLE-payload publication — the C7 prewarm / dense-
    /// consumer face. On the block-lazy arm this is ONE CRC-validated
    /// whole-extent read + decompress-all (today's whole-region economics);
    /// on every other arm it is `ensure_code(0)` (whole-fault assembles the
    /// region; frame arms keep their lazy grain — the prefetcher owns their
    /// bulk story). Bulk byte-folding consumers call THIS, never a fake
    /// first-entry touch.
    pub fn prewarm_payload(&self) -> ReadResult<()> {
        if self.entry_count == 0 {
            return Ok(());
        }
        match &self.payload {
            PayloadRegion::BlockLazy(bl) => {
                let index = self.index_slice()?;
                bl.ensure_all(
                    &self.part,
                    self.unwrappers,
                    &self.payload_stream,
                    "dict payload stream",
                    index,
                )?;
                // [sqe8blk] prewarm IS the dense election: everything is
                // published now — arm the all-resident short-circuit.
                self.payload_all_resident.store(true, Ordering::Release);
                Ok(())
            }
            _ => self.ensure_code(0),
        }
    }

    /// Stored byte + char lengths of `code` — INDEX-ONLY (no payload fault):
    /// the stored-lengths law makes `length()` a table lookup (spec §7).
    pub fn lengths(&self, code: u32) -> ReadResult<(u32, u32)> {
        if code >= self.entry_count {
            return Err(ReadError::Format(FormatError::Bounds { at: "dict code" }));
        }
        let (iptr, ilen) = self
            .index
            .ensure(&self.part, self.unwrappers, &self.index_stream, "dict index stream", None)?;
        // SAFETY: (iptr, ilen) were published from a payload slice of an
        // `Arc<[u8]>` rooted in `self.index.cell` for `self`'s lifetime;
        // the cell is write-once, so the region is live and immutable.
        let index = unsafe { core::slice::from_raw_parts(iptr, ilen) };
        let off = code as usize * DICT_INDEX_ENTRY_LEN;
        let rec = index
            .get(off..off + DICT_INDEX_ENTRY_LEN)
            .ok_or(ReadError::Format(FormatError::Bounds { at: "dict index" }))?;
        let byte_len = u32::from_le_bytes(rec[4..8].try_into().expect("len 4"));
        let char_field = u32::from_le_bytes(rec[8..12].try_into().expect("len 4"));
        // M5d char-len record: absolute char_len under the stream's
        // declared form (typed corruption on a delta > byte_len — UTF-8
        // chars are never longer than their bytes).
        use pgrc_format::dict::DictCharLenForm;
        let char_len = match self.charlen_form {
            DictCharLenForm::Absolute => char_field,
            DictCharLenForm::Delta => byte_len
                .checked_sub(char_field)
                .ok_or(ReadError::Format(FormatError::Corrupt {
                    at: "dict entry char_len delta",
                }))?,
            // Option-C: nothing stored — serve from the load-time
            // one-pass table (built on first call; the index-only/
            // no-payload-fault contract is DEVIATED under this form by
            // design — the build faults the payload once, then every
            // call is a table read).
            DictCharLenForm::Absent => {
                if self.char_table.get().is_none() {
                    let t = self.build_char_table()?;
                    let n = (t.capacity() * core::mem::size_of::<u32>()) as u64;
                    if self.char_table.set(t).is_ok() {
                        let _ = self.char_table_charge.set(crate::partcache::holder_charge(n));
                    }
                }
                let t = self.char_table.get().expect("just set");
                *t.get(code as usize).ok_or(ReadError::Format(
                    FormatError::Bounds { at: "dict char table" },
                ))?
            }
        };
        Ok((byte_len, char_len))
    }

    /// The stream's char-len form (what the index's third field carries).
    pub fn charlen_form(&self) -> pgrc_format::dict::DictCharLenForm {
        self.charlen_form
    }

    /// Whether the index is faulted: `index_bytes` and every index-only reader read nothing.
    pub fn index_resident(&self) -> bool {
        self.index.get().is_some()
    }

    /// The whole index (`DICT_INDEX_ENTRY_LEN` bytes per code: payload_off, byte_len, the
    /// char field) — INDEX-ONLY, no payload fault; live and immutable for the handle's life.
    pub fn index_bytes(&self) -> ReadResult<&[u8]> {
        let (iptr, ilen) = self
            .index
            .ensure(&self.part, self.unwrappers, &self.index_stream, "dict index stream", None)?;
        // SAFETY: a write-once published region, live for `self`'s lifetime.
        let index = unsafe { core::slice::from_raw_parts(iptr, ilen) };
        index
            .get(..self.entry_count as usize * DICT_INDEX_ENTRY_LEN)
            .ok_or(ReadError::Format(FormatError::Bounds { at: "dict index" }))
    }

    /// The whole payload as one slice that every entry's `payload_off` indexes directly, when
    /// the arm holds it so (`Whole`, or `BlockLazy` after `prewarm_payload`); `None` on the
    /// frame-served arms, where entries are reached through `entries` alone.
    pub fn payload_bytes(&self) -> ReadResult<Option<&[u8]>> {
        if self.entry_count == 0 {
            return Ok(Some(&[]));
        }
        if self.fc {
            return Ok(None);
        }
        let (p, l) = match &self.payload {
            PayloadRegion::Whole(r) => {
                self.ensure_code(0)?;
                r.get().ok_or(ReadError::Format(FormatError::Corrupt {
                    at: "dict payload region unpublished after ensure",
                }))?
            }
            PayloadRegion::BlockLazy(bl) => {
                self.prewarm_payload()?;
                bl.get().ok_or(ReadError::Format(FormatError::Corrupt {
                    at: "dict payload region unpublished after ensure",
                }))?
            }
            _ => return Ok(None),
        };
        // SAFETY: a write-once published region, live for `self`'s lifetime.
        Ok(Some(unsafe { core::slice::from_raw_parts(p, l) }))
    }

    /// Stored byte length of `code` — INDEX-ONLY under EVERY char-len
    /// form (`byte_len` lives in the index's second field regardless of
    /// what the third carries), so byte-only consumers (the per-row
    /// StrView flip's cell sizing — the hot caller) never touch the
    /// Option-C char table. The §9 guardrail pair caught the coupling:
    /// routing byte_len through `lengths()` made every dict-text query
    /// pay the load-time char walk it never asked for.
    pub fn byte_len_only(&self, code: u32) -> ReadResult<u32> {
        if code >= self.entry_count {
            return Err(ReadError::Format(FormatError::Bounds { at: "dict code" }));
        }
        let (iptr, ilen) = self
            .index
            .ensure(&self.part, self.unwrappers, &self.index_stream, "dict index stream", None)?;
        // SAFETY: write-once published region, live for `self`'s lifetime.
        let index = unsafe { core::slice::from_raw_parts(iptr, ilen) };
        let off = code as usize * DICT_INDEX_ENTRY_LEN;
        let rec = index
            .get(off..off + DICT_INDEX_ENTRY_LEN)
            .ok_or(ReadError::Format(FormatError::Bounds { at: "dict index" }))?;
        Ok(u32::from_le_bytes(rec[4..8].try_into().expect("len 4")))
    }

    /// Option-C (`STREAMF_CHARLEN_ABSENT`): the load-time one-pass —
    /// resolve every entry once and count UTF-8 lead bytes (the
    /// char-length fact's one definition). The M5d length-lanes consumer
    /// builds its per-code table from this same pass.
    fn build_char_table(&self) -> ReadResult<Vec<u32>> {
        let mut t = Vec::with_capacity(self.entry_count as usize);
        let mut cur = self.entries(0, self.entry_count)?;
        while let Some((_, e)) = cur.next_entry()? {
            // Under the Absent form the cursor's `char_len` IS the
            // `utf8_char_count` walk (the fact's one definition).
            t.push(e.char_len);
        }
        Ok(t)
    }

    /// Resolve entry `code`: the varlena-shaped image (a valid PG datum by
    /// the §7b law), its payload bytes, and the stored lengths. The returned
    /// references live as long as the handle and their addresses NEVER
    /// change (generation stability — module doc).
    pub fn entry(&self, code: u32) -> ReadResult<DictEntryRef<'_>> {
        self.ensure_code(code)?;
        if crate::dict_census_on() {
            DICT_ENTRY_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        let (iptr, ilen) = self
            .index
            .ensure(&self.part, self.unwrappers, &self.index_stream, "dict index stream", None)?;
        if self.fc {
            // SAFETY: the write-once published index region, live for `self`'s lifetime.
            let index = unsafe { core::slice::from_raw_parts(iptr, ilen) };
            return self.fc_entry(index, code);
        }
        // [cold2] frame-direct: resolve out of the frame's own SegBuf —
        // no whole-payload slice exists on this arm.
        if let PayloadRegion::FrameDirect(fd) = &self.payload {
            // SAFETY: write-once published index region, live for `self`'s
            // lifetime (same justification as the other arms below).
            let index = unsafe { core::slice::from_raw_parts(iptr, ilen) };
            let off = code as usize * DICT_INDEX_ENTRY_LEN;
            let rec = index
                .get(off..off + DICT_INDEX_ENTRY_LEN)
                .ok_or(ReadError::Format(FormatError::Bounds { at: "dict index" }))?;
            let payload_off = u32::from_le_bytes(rec[0..4].try_into().expect("len 4"));
            let byte_len = u32::from_le_bytes(rec[4..8].try_into().expect("len 4"));
            let char_field = u32::from_le_bytes(rec[8..12].try_into().expect("len 4"));
            let (image, bytes) = fd.entry_slices(code, payload_off as usize)?;
            if bytes.len() != byte_len as usize {
                return Err(ReadError::Format(FormatError::Corrupt {
                    at: "dict entry byte_len",
                }));
            }
            use pgrc_format::dict::DictCharLenForm;
            let char_len = match self.charlen_form {
                DictCharLenForm::Absolute => char_field,
                DictCharLenForm::Delta => byte_len.checked_sub(char_field).ok_or(
                    ReadError::Format(FormatError::Corrupt {
                        at: "dict entry char_len delta",
                    }),
                )?,
                DictCharLenForm::Absent => pgrc_format::dict::utf8_char_count(bytes),
            };
            return Ok(DictEntryRef { image, bytes, byte_len, char_len });
        }
        // Read the index rec once. `payload_off` is untrusted file bytes;
        // the FrameLazy/BlockLazy arms below bound it to `code`'s PUBLISHED
        // frame/block (idx251) rather than the whole payload window.
        let index = unsafe { core::slice::from_raw_parts(iptr, ilen) };
        let ioff = code as usize * DICT_INDEX_ENTRY_LEN;
        let rec = index
            .get(ioff..ioff + DICT_INDEX_ENTRY_LEN)
            .ok_or(ReadError::Format(FormatError::Bounds { at: "dict index" }))?;
        let payload_off = u32::from_le_bytes(rec[0..4].try_into().expect("len 4")) as usize;
        let byte_len = u32::from_le_bytes(rec[4..8].try_into().expect("len 4"));
        let char_field = u32::from_le_bytes(rec[8..12].try_into().expect("len 4"));
        match &self.payload {
            PayloadRegion::FrameDirect(_) => unreachable!("handled above"),
            PayloadRegion::Whole(r) => {
                let (pptr, plen) = r.ensure(
                    &self.part,
                    self.unwrappers,
                    &self.payload_stream,
                    "dict payload stream",
                    Some(index),
                )?;
                // Whole-fault publishes the ENTIRE region atomically (every
                // byte resident before the ptr is published), so the whole
                // window IS the published window — no per-frame race.
                // SAFETY: published from a payload slice of an `Arc<[u8]>`
                // rooted in this handle's write-once cell; live and immovable
                // for `self`'s lifetime.
                let payload = unsafe { core::slice::from_raw_parts(pptr, plen) };
                present_bounded_entry(
                    payload, 0, 0, plen, payload_off, byte_len, char_field, self.charlen_form,
                )
            }
            PayloadRegion::FrameLazy(fl) => {
                let (base, region_len, pstart, pend) =
                    fl.get().ok_or(ReadError::Format(FormatError::Corrupt {
                        at: "dict payload region unpublished after ensure",
                    }))?;
                // [idx251] Bound the read to `code`'s PUBLISHED frame extent
                // — not the whole payload window. `ensure_code` above
                // published frame `f` (its frame bit is set, Acquire); other
                // extents may be unpublished / concurrently written, so an
                // entry that points outside frame `f` is refused before any
                // dereference.
                let f = self.frame_of_code(code);
                let (flo, fhi) =
                    fl.frame_span(f).ok_or(ReadError::Format(FormatError::Corrupt {
                        at: "dict payload geometry unpublished after ensure",
                    }))?;
                let hi = fhi.min(pend);
                // SAFETY: `base` is the published, never-moving region
                // reservation; `[flo, hi)` lies within frame `f`'s extent,
                // whose bytes the frame bit proves resident and immutable.
                let region = unsafe { core::slice::from_raw_parts(base, region_len) };
                let seg = region
                    .get(flo..hi)
                    .ok_or(ReadError::Format(FormatError::Corrupt { at: "dict frame span" }))?;
                present_bounded_entry(
                    seg, flo, pstart, hi, payload_off, byte_len, char_field, self.charlen_form,
                )
            }
            PayloadRegion::BlockLazy(bl) => {
                let (pptr, plen) = bl.get().ok_or(ReadError::Format(FormatError::Corrupt {
                    at: "dict payload region unpublished after ensure",
                }))?;
                // SAFETY: published write-once payload window (reserved
                // buffer or the whole-fallback region), live and immovable
                // for `self`'s lifetime.
                let payload = unsafe { core::slice::from_raw_parts(pptr, plen) };
                // [idx251] On the block path bound the read to `code`'s
                // PUBLISHED block; the whole-fallback publishes every byte at
                // once, so its whole window IS the published window.
                match bl.block_span(self.frame_of_code(code), plen) {
                    Some((blo, bhi)) => {
                        let seg = payload.get(blo..bhi).ok_or(ReadError::Format(
                            FormatError::Corrupt { at: "dict block span" },
                        ))?;
                        present_bounded_entry(
                            seg, blo, 0, bhi, payload_off, byte_len, char_field,
                            self.charlen_form,
                        )
                    }
                    None => present_bounded_entry(
                        payload, 0, 0, plen, payload_off, byte_len, char_field,
                        self.charlen_form,
                    ),
                }
            }
        }
    }

    /// The front-coded arm's `entry`: the memo's image, else decoded now into the arena.
    fn fc_entry(&self, index: &[u8], code: u32) -> ReadResult<DictEntryRef<'_>> {
        let ioff = code as usize * DICT_INDEX_ENTRY_LEN;
        let rec = index
            .get(ioff..ioff + DICT_INDEX_ENTRY_LEN)
            .ok_or(ReadError::Format(FormatError::Bounds { at: "dict index" }))?;
        let byte_len = u32::from_le_bytes(rec[4..8].try_into().expect("len 4"));
        let char_field = u32::from_le_bytes(rec[8..12].try_into().expect("len 4"));
        let memo = self.fc_memo.get_or_init(|| {
            let _ = self.fc_memo_charge.set(crate::partcache::holder_charge((self.entry_count as usize * core::mem::size_of::<AtomicU64>()) as u64));
            (0..self.entry_count).map(|_| AtomicU64::new(0)).collect() // alloc: Exec
        });
        let mut p = memo[code as usize].load(Ordering::Acquire);
        if p == 0 {
            let PayloadRegion::Whole(r) = &self.payload else { unreachable!("fc elects the whole arm") };
            let (pptr, plen) = r.get().ok_or(ReadError::Format(FormatError::Corrupt {
                at: "dict payload region unpublished after ensure",
            }))?;
            // SAFETY: the published write-once region, live for `self`'s lifetime.
            let payload = unsafe { core::slice::from_raw_parts(pptr, plen) };
            let fcr = pgrc_format::dict::FcResident::parse(payload)?;
            let mut arena = pgsync::lock(&self.fc_arena);
            p = memo[code as usize].load(Ordering::Acquire);
            if p == 0 {
                let slot = arena.alloc(4 + byte_len as usize);
                pgrc_format::dict::fc_entry_into(&fcr, index, code, slot)?;
                p = slot.as_ptr() as u64;
                memo[code as usize].store(p, Ordering::Release);
                DICT_FC_MATERIALIZED.fetch_add(1, Ordering::Relaxed);
                DICT_FC_MATERIALIZED_BYTES.fetch_add(4 + byte_len as u64, Ordering::Relaxed);
            }
        }
        // SAFETY: `p` is a materialised image inside an arena chunk that never moves or frees
        // before `self` drops; `4 + byte_len` bytes were written there.
        let image = unsafe { core::slice::from_raw_parts(p as *const u8, 4 + byte_len as usize) };
        let bytes = &image[4..];
        use pgrc_format::dict::DictCharLenForm;
        let char_len = match self.charlen_form {
            DictCharLenForm::Absolute => char_field,
            DictCharLenForm::Delta => byte_len.checked_sub(char_field).ok_or(ReadError::Format(FormatError::Corrupt {
                at: "dict entry char_len delta",
            }))?,
            DictCharLenForm::Absent => pgrc_format::dict::utf8_char_count(bytes),
        };
        Ok(DictEntryRef { image, bytes, byte_len, char_len })
    }

    /// The bytes the front-coded arm materialised on this handle (0 on the expanded arm).
    pub fn fc_materialized_bytes(&self) -> u64 {
        if !self.fc {
            return 0;
        }
        let a = pgsync::lock(&self.fc_arena);
        (a.chunks.len().saturating_sub(1) * FC_CHUNK + a.used) as u64
    }

    /// The owning part.
    pub fn part(&self) -> &Arc<OpenPart> {
        &self.part
    }

    /// [sqe P5-0 lever 4] Bulk sequential entry access: a cursor over codes
    /// `[start, end)` that amortizes the per-entry `entry()` overhead
    /// (bounds + ensure + arm dispatch + region republication, ~44ns/entry
    /// measured on the fp build) down to one frame-grain refill per
    /// `DICT_FRAME_ENTRIES` codes. Yields the SAME `DictEntryRef`s as
    /// [`DictHandle::entry`] — byte-for-byte identical output is pinned by
    /// `tests/dict.rs` — with the same generation-stability law: the
    /// returned slices borrow the handle, never the cursor, and no
    /// per-entry allocation occurs.
    pub fn entries(&self, start: u32, end: u32) -> ReadResult<DictEntryCursor<'_>> {
        if start > end || end > self.entry_count {
            return Err(ReadError::Format(FormatError::Bounds { at: "dict code" }));
        }
        let index: &[u8] = if start == end {
            // Empty range: fault nothing (the lazy contract holds).
            &[]
        } else {
            let (iptr, ilen) = self.index.ensure(
                &self.part,
                self.unwrappers,
                &self.index_stream,
                "dict index stream",
                None,
            )?;
            // SAFETY: (iptr, ilen) were published from a payload slice of an
            // `Arc<[u8]>` rooted in `self.index.cell` for `self`'s lifetime;
            // the cell is write-once, so the region is live and immutable.
            unsafe { core::slice::from_raw_parts(iptr, ilen) }
        };
        let fc = if self.fc && start < end {
            self.ensure_frame(self.frame_of_code(start))?;
            let PayloadRegion::Whole(r) = &self.payload else { unreachable!("fc elects the whole arm") };
            let (pptr, plen) = r.get().ok_or(ReadError::Format(FormatError::Corrupt {
                at: "dict payload region unpublished after ensure",
            }))?;
            // SAFETY: the published write-once region, live for `'_` (the handle's lifetime).
            let payload = unsafe { core::slice::from_raw_parts(pptr, plen) };
            let fcr = pgrc_format::dict::FcResident::parse(payload)?;
            Some((pgrc_format::dict::FcWalk::new(fcr, index, start, end)?, Vec::new()))
        } else {
            None
        };
        Ok(DictEntryCursor {
            dh: self,
            index,
            win: BulkWin::Contig(&[]),
            code: start,
            end,
            // Forces the first `next_entry` to ensure + bind frame(start).
            frame_end: start,
            fc,
            walked: 0,
        })
    }
}

/// The bulk cursor's per-frame payload binding — resolved ONCE per frame
/// at refill instead of once per entry.
#[derive(Clone, Copy)]
enum BulkWin<'a> {
    /// Whole-payload window (Whole / FrameLazy / BlockLazy arms): entry
    /// `payload_off`s index directly into this slice. Only bytes of
    /// ENSURED frames are dereferenced through it (the refill ensured the
    /// current frame; entries of frame f live wholly inside extent f).
    Contig(&'a [u8]),
    /// [cold2] frame-direct: the current frame's SegBuf; `payload_off` maps
    /// via `payload_start + payload_off - base` (the `entry_slices` law).
    Direct {
        seg: &'a [u8],
        base: usize,
        payload_start: usize,
        payload_end: usize,
    },
}

/// Sequential bulk entry cursor over a [`DictHandle`] — see
/// [`DictHandle::entries`]. Entry slices borrow the HANDLE (`'a`), so they
/// remain valid after the cursor advances or drops (generation stability).
pub struct DictEntryCursor<'a> {
    dh: &'a DictHandle,
    index: &'a [u8],
    win: BulkWin<'a>,
    code: u32,
    end: u32,
    /// First code NOT served by the currently-bound frame window.
    frame_end: u32,
    /// The front-coded arm: the walk and the buffer the current entry is decoded into in place.
    fc: Option<(pgrc_format::dict::FcWalk<'a>, Vec<u8>)>,
    walked: u64,
}

impl Drop for DictEntryCursor<'_> {
    fn drop(&mut self) {
        if self.walked != 0 {
            DICT_CURSOR_ENTRIES.fetch_add(self.walked, Ordering::Relaxed);
        }
    }
}

impl<'a> DictEntryCursor<'a> {
    /// Bind the frame containing `self.code`: one `ensure_frame` + one arm
    /// dispatch per `DICT_FRAME_ENTRIES` codes.
    #[cold]
    fn refill(&mut self) -> ReadResult<()> {
        let dh = self.dh;
        let frame = dh.frame_of_code(self.code);
        dh.ensure_frame(frame)?;
        self.win = match &dh.payload {
            PayloadRegion::Whole(r) => {
                let (p, l) = r.get().ok_or(ReadError::Format(FormatError::Corrupt {
                    at: "dict payload region unpublished after ensure",
                }))?;
                // SAFETY: published from a payload slice of an `Arc<[u8]>`
                // rooted in the handle's write-once cell; live and immutable
                // for `'a` (the same justification as `entry()`).
                BulkWin::Contig(unsafe { core::slice::from_raw_parts(p, l) })
            }
            PayloadRegion::FrameLazy(fl) => {
                let (base, region_len, pstart, pend) =
                    fl.get().ok_or(ReadError::Format(FormatError::Corrupt {
                        at: "dict payload region unpublished after ensure",
                    }))?;
                // [idx251] Bind a per-frame window (not the whole payload):
                // `ensure_frame` above published exactly frame `frame`; other
                // extents may be unpublished / concurrently written. The
                // `Direct` win bounds each entry to `[flo, hi)`, so a crafted
                // `payload_off` cannot reach outside the published frame.
                let (flo, fhi) =
                    fl.frame_span(frame).ok_or(ReadError::Format(FormatError::Corrupt {
                        at: "dict payload geometry unpublished after ensure",
                    }))?;
                let hi = fhi.min(pend);
                // SAFETY: `base` is the published, never-moving region
                // reservation; `[flo, hi)` lies within frame `frame`'s
                // extent, whose bytes the frame bit proves resident and
                // immutable for `'a`.
                let region = unsafe { core::slice::from_raw_parts(base, region_len) };
                let seg = region
                    .get(flo..hi)
                    .ok_or(ReadError::Format(FormatError::Corrupt { at: "dict frame span" }))?;
                BulkWin::Direct {
                    seg,
                    base: flo,
                    payload_start: pstart,
                    payload_end: hi,
                }
            }
            PayloadRegion::BlockLazy(bl) => {
                let (p, l) = bl.get().ok_or(ReadError::Format(FormatError::Corrupt {
                    at: "dict payload region unpublished after ensure",
                }))?;
                // SAFETY: published write-once payload window (reserved
                // buffer or the whole-fallback region), live and immovable
                // for `'a`.
                let payload = unsafe { core::slice::from_raw_parts(p, l) };
                // [idx251] On the block path bind a per-block window; the
                // whole-fallback publishes every byte at once, so its whole
                // window IS the published window.
                match bl.block_span(frame, l) {
                    Some((blo, bhi)) => {
                        let seg = payload.get(blo..bhi).ok_or(ReadError::Format(
                            FormatError::Corrupt { at: "dict block span" },
                        ))?;
                        BulkWin::Direct {
                            seg,
                            base: blo,
                            payload_start: 0,
                            payload_end: bhi,
                        }
                    }
                    None => BulkWin::Contig(payload),
                }
            }
            PayloadRegion::FrameDirect(fd) => {
                let geom = fd
                    .geom
                    .get()
                    .ok_or(ReadError::Format(FormatError::Corrupt {
                        at: "dict payload geometry unpublished after ensure",
                    }))?;
                let seg = fd.segs[frame as usize].get().ok_or(ReadError::Format(
                    FormatError::Corrupt {
                        at: "dict frame unresident after ensure",
                    },
                ))?;
                BulkWin::Direct {
                    seg: seg.bytes(),
                    base: geom.offsets[frame as usize],
                    payload_start: geom.payload_start,
                    payload_end: geom.payload_end,
                }
            }
        };
        self.frame_end = (((frame as u64) + 1) * DICT_FRAME_ENTRIES as u64)
            .min(self.end as u64) as u32;
        Ok(())
    }

    /// The front-coded arm's `next_entry`: the walk decodes in place into the cursor's buffer.
    fn next_fc(&mut self) -> ReadResult<Option<(u32, DictEntryRef<'_>)>> {
        let (walk, buf) = self.fc.as_mut().expect("fc arm");
        let Some((code, len)) = walk.next(buf)? else { return Ok(None) };
        self.walked += 1;
        let off = code as usize * DICT_INDEX_ENTRY_LEN;
        let rec = self.index.get(off..off + DICT_INDEX_ENTRY_LEN).ok_or(ReadError::Format(FormatError::Bounds { at: "dict index" }))?;
        let char_field = u32::from_le_bytes(rec[8..12].try_into().expect("len 4"));
        let image = &buf[..4 + len];
        use pgrc_format::dict::DictCharLenForm;
        let char_len = match self.dh.charlen_form {
            DictCharLenForm::Absolute => char_field,
            DictCharLenForm::Delta => (len as u32).checked_sub(char_field).ok_or(ReadError::Format(FormatError::Corrupt {
                at: "dict entry char_len delta",
            }))?,
            DictCharLenForm::Absent => pgrc_format::dict::utf8_char_count(&image[4..]),
        };
        Ok(Some((code, DictEntryRef { image, bytes: &image[4..], byte_len: len as u32, char_len })))
    }

    /// The next `(code, entry)` in the range, or `Ok(None)` at the end.
    /// Identical validation + presentation to [`DictHandle::entry`]. The entry borrows the
    /// cursor: on the front-coded arm it lives in the cursor's buffer until the next call.
    #[inline]
    pub fn next_entry(&mut self) -> ReadResult<Option<(u32, DictEntryRef<'_>)>> {
        if self.fc.is_some() {
            return self.next_fc();
        }
        if self.code >= self.frame_end {
            if self.code >= self.end {
                return Ok(None);
            }
            self.refill()?;
        }
        let code = self.code;
        let off = code as usize * DICT_INDEX_ENTRY_LEN;
        let rec = self
            .index
            .get(off..off + DICT_INDEX_ENTRY_LEN)
            .ok_or(ReadError::Format(FormatError::Bounds { at: "dict index" }))?;
        let payload_off = u32::from_le_bytes(rec[0..4].try_into().expect("len 4"));
        let byte_len = u32::from_le_bytes(rec[4..8].try_into().expect("len 4"));
        let char_field = u32::from_le_bytes(rec[8..12].try_into().expect("len 4"));
        let (image, bytes) = match self.win {
            BulkWin::Contig(p) => {
                pgrc_format::wire::varlena_entry_at(p, payload_off as usize, "dict payload")?
            }
            BulkWin::Direct {
                seg,
                base,
                payload_start,
                payload_end,
            } => {
                let region_off = payload_start + payload_off as usize;
                if region_off >= payload_end {
                    return Err(ReadError::Format(FormatError::Bounds {
                        at: "dict payload",
                    }));
                }
                let lo = region_off
                    .checked_sub(base)
                    .ok_or(ReadError::Format(FormatError::Bounds { at: "dict payload" }))?;
                pgrc_format::wire::varlena_entry_at(seg, lo, "dict payload")?
            }
        };
        if bytes.len() != byte_len as usize {
            return Err(ReadError::Format(FormatError::Corrupt {
                at: "dict entry byte_len",
            }));
        }
        use pgrc_format::dict::DictCharLenForm;
        let char_len = match self.dh.charlen_form {
            DictCharLenForm::Absolute => char_field,
            DictCharLenForm::Delta => {
                byte_len
                    .checked_sub(char_field)
                    .ok_or(ReadError::Format(FormatError::Corrupt {
                        at: "dict entry char_len delta",
                    }))?
            }
            DictCharLenForm::Absent => pgrc_format::dict::utf8_char_count(bytes),
        };
        self.code = code + 1;
        self.walked += 1;
        Ok(Some((
            code,
            DictEntryRef {
                image,
                bytes,
                byte_len,
                char_len,
            },
        )))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod d4135_tests {
    use super::*;

    const PAGE: usize = 4096;

    /// The bytes of `b`'s whole pages the kernel has in core (mincore).
    fn resident(b: &ImageBuf) -> usize {
        let (at, bytes) = (b.as_ptr() as usize, core::mem::size_of_val(&**b));
        let (lo, hi) = (at.next_multiple_of(PAGE), (at + bytes) / PAGE * PAGE);
        let mut vec = vec![0u8; (hi - lo) / PAGE];
        // SAFETY: `[lo, hi)` is whole pages inside the live reservation; `vec` holds one byte a page.
        let rc = unsafe { libc::mincore(lo as *mut libc::c_void, hi - lo, vec.as_mut_ptr()) };
        assert_eq!(rc, 0, "mincore");
        vec.iter().filter(|&&v| v & 1 != 0).count() * PAGE
    }

    /// D4135: a reservation commits no whole page until a fault writes it, over memory the allocator reuses as over fresh
    /// OS pages, and one block's fault commits that block only. Before, `alloc_zeroed` zeroed a reused block with a write,
    /// so a reservation nothing was faulted into was resident whole.
    #[test]
    fn a_reservation_commits_only_the_pages_a_fault_writes_over_reused_memory_too() {
        const WORDS: usize = (8 << 20) / 8;
        const BLOCK: usize = 64 << 10;
        let thp_always = std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled").is_ok_and(|t| t.contains("[always]"));
        for round in 0..6 {
            let mut b = ImageBuf::zeroed(WORDS).expect("the reservation");
            assert_eq!(resident(&b), 0, "round {round}: nothing faulted, so no whole page is resident");
            // one block's fault, in the reservation's middle
            let at = WORDS / 2;
            b[at..at + BLOCK / 8].fill(7);
            let now = resident(&b);
            assert!(now >= BLOCK - PAGE, "round {round}: the faulted block is resident ({now})");
            assert!(thp_always || now <= BLOCK + PAGE, "round {round}: at most one page past the faulted block ({now})");
            assert!(b[..at].iter().all(|&w| w == 0) && b[at + BLOCK / 8..].iter().all(|&w| w == 0), "the rest reads zero");
            // every page dirtied, so the allocator's next block of this size is memory it reuses
            b.fill(u64::MAX);
        }
    }
}

#[cfg(test)]
mod idx136_tests {
    use super::*;
    use crate::ReadError;

    /// A v1 index image over entries of `lens` bytes, laid out by the writer's
    /// law: each varlena image at the next 8-aligned offset.
    fn index_of(lens: &[u32]) -> (Vec<u8>, usize) {
        let (mut out, mut off) = (Vec::new(), 0usize);
        let mut end = 0usize;
        for &l in lens {
            out.extend_from_slice(&(off as u32).to_le_bytes());
            out.extend_from_slice(&l.to_le_bytes());
            out.extend_from_slice(&l.to_le_bytes());
            end = off + 4 + l as usize;
            off = end.next_multiple_of(8);
        }
        (out, end)
    }

    fn refused(r: ReadResult<usize>) -> bool {
        matches!(
            r,
            Err(ReadError::Format(FormatError::Corrupt {
                at: "dict payload uncompressed_len vs dict index"
            }))
        )
    }

    // [idx136] A block-lazy dict payload declaring a near-4-GiB uncompressed
    // image its index does not record is REFUSED typed before any
    // image-sized reservation — never a 4 GiB alloc/abort.
    #[test]
    fn crafted_uncompressed_len_refused() {
        let (index, _) = index_of(&[10, 2100, 7]);
        assert!(refused(dict_payload_unc_checked((u32::MAX - 7) as usize, &index)));
    }

    // [idx136, D302] The law is exact: one byte short of the last entry, or
    // more than 7 bytes of pad past it, disagrees with the index.
    #[test]
    fn a_header_the_index_contradicts_is_refused() {
        let (index, end) = index_of(&[10, 2100, 7]);
        assert!(refused(dict_payload_unc_checked(end - 1, &index)));
        assert!(refused(dict_payload_unc_checked(end + 8, &index)));
        let (empty, _) = index_of(&[]);
        assert!(refused(dict_payload_unc_checked(8, &empty)));
        assert!(dict_payload_unc_checked(0, &empty).is_ok());
    }

    // [D302] Valid writer output passes whatever its compression ratio: 800
    // front-coded 2,104-byte entries seal to a 1,435-byte zstd span (1177:1)
    // on the repro's part, and the old 1024× ratio refused them.
    #[test]
    fn valid_payload_accepted_at_any_ratio() {
        let lens: Vec<u32> = (0..800).map(|i| 2100 + (i % 6) as u32).collect();
        let (index, end) = index_of(&lens);
        for unc in end..=end + 7 {
            assert_eq!(dict_payload_unc_checked(unc, &index).ok(), Some(unc));
        }
        assert!(end > 1024 * 1435, "the repro's image is past the old ratio bound");
    }
}

#[cfg(test)]
mod idx251_tests {
    use super::*;
    use pgrc_format::dict::DictCharLenForm;

    // A published-window frame at region offset 100..106; the payload origin
    // (payload_off == 0) maps to region offset 10. A single 2-byte entry
    // ("hi") lives at the window start.
    fn frame_window() -> (Vec<u8>, usize, usize, usize) {
        let mut seg = Vec::new();
        seg.extend_from_slice(&pgrc_format::wire::varlena_header_4b_u(2).to_le_bytes());
        seg.extend_from_slice(b"hi");
        let seg_base = 100usize; // region offset of the published frame start
        let payload_start = 10usize; // region offset where payload_off==0 maps
        let payload_end = seg_base + seg.len(); // window upper bound
        (seg, seg_base, payload_start, payload_end)
    }

    // [idx251] An entry whose bytes fall inside the published window resolves.
    #[test]
    fn entry_within_published_window_ok() {
        let (seg, seg_base, payload_start, payload_end) = frame_window();
        // region_off == seg_base ⇒ payload_off == seg_base - payload_start.
        let payload_off = seg_base - payload_start;
        let e = present_bounded_entry(
            &seg,
            seg_base,
            payload_start,
            payload_end,
            payload_off,
            2,
            2,
            DictCharLenForm::Absolute,
        )
        .expect("entry inside the published window resolves");
        assert_eq!(e.bytes, b"hi");
    }

    // [idx251] The core fix: an entry whose `payload_off` points BELOW the
    // published window (into an adjacent / possibly-unpublished frame) is
    // refused with a typed Bounds error — the old whole-window bound would
    // have read those unpublished bytes (the data race).
    #[test]
    fn entry_below_published_window_refused() {
        let (seg, seg_base, payload_start, payload_end) = frame_window();
        // One byte below the window start ⇒ region_off < seg_base.
        let payload_off = seg_base - payload_start - 1;
        let err = present_bounded_entry(
            &seg,
            seg_base,
            payload_start,
            payload_end,
            payload_off,
            2,
            2,
            DictCharLenForm::Absolute,
        )
        .err()
        .expect("below-window entry must be refused");
        assert!(
            matches!(err, ReadError::Format(FormatError::Bounds { at: "dict payload" })),
            "expected typed Bounds refusal, got {err:?}"
        );
    }

    // [idx251] An entry at/above the window's exclusive upper bound (into a
    // later, possibly-unpublished frame) is also refused.
    #[test]
    fn entry_above_published_window_refused() {
        let (seg, seg_base, payload_start, payload_end) = frame_window();
        // region_off == payload_end (exclusive) ⇒ refused.
        let payload_off = payload_end - payload_start;
        assert!(present_bounded_entry(
            &seg,
            seg_base,
            payload_start,
            payload_end,
            payload_off,
            2,
            2,
            DictCharLenForm::Absolute,
        )
        .is_err());
    }
}

#[cfg(test)]
mod d4482_fc_arena_tests {
    use super::*;

    /// D4482 b: a front-coded handle's materialised chunks are the part cache's from their allocation: the arena's charge
    /// (on the part cache's holder) holds every chunk's bytes, a second chunk's too, and nothing before the first.
    #[test]
    fn an_fc_arenas_chunks_are_the_part_caches_from_their_allocation() {
        let mut a = FcArena { charge: None, chunks: Vec::new(), used: 0 };
        assert!(a.charge.is_none(), "an arena that materialised nothing holds no charge");
        a.alloc(10);
        a.alloc(100);
        let held = |a: &FcArena| a.charge.as_ref().map_or(0, |c| c.committed());
        assert_eq!(held(&a), FC_CHUNK as u64, "the first chunk is on the charge");
        assert_eq!(a.charge.as_ref().expect("charge").owner(), crate::partcache::OWNER, "the part cache's holder");
        a.alloc(FC_CHUNK);
        let bytes: usize = a.chunks.iter().map(|c| c.len()).sum();
        assert_eq!((a.chunks.len(), held(&a)), (2, bytes as u64), "a second chunk's bytes join the charge");
    }
}

#[cfg(test)]
mod cachetrack_door_tests {
    use super::payload_door_bytes;

    #[test]
    fn a_decoded_payload_door_carries_its_resident_bytes() {
        // not resident yet, or decoded no larger than stored: the stored bytes
        assert_eq!(payload_door_bytes(4096, 4096, 0), 4096);
        assert_eq!(payload_door_bytes(4096, 4096, 4096), 4096);
        assert_eq!(payload_door_bytes(4096, 4096, 1024), 4096);
        // one extent of a 3x payload: its decoded bytes
        assert_eq!(payload_door_bytes(4096, 4096, 12288), 12288);
        // two extents (1 + 3 of 4 stored KiB) of a 16 KiB decoded payload: each its share
        assert_eq!(payload_door_bytes(1024, 4096, 16384), 4096);
        assert_eq!(payload_door_bytes(3072, 4096, 16384), 12288);
        assert_eq!(payload_door_bytes(0, 0, 16384), 0);
    }
}
