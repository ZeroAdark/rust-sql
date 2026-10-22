//! The decode-kernel invocation layer (spec §19, read side): vtable
//! resolution ONCE at stream open (never in or near the hot loop — the S4
//! pow2-switch law), per-extent parsed state (header, frame table, gcount
//! prefix), per-face section faulting (crate-doc policy table), and the six
//! face calls over CRC-validated bytes.
//!
//! Contract notes set by this layer (cited by the M3-F report; binding on
//! M3-C's kernels because the reader builds every ctx):
//!
//! - **`validity` face ctx carries `bytes = &[]`.** The validity bitmap
//!   layout is encoding-independent (spec §6.6) and the canonical kernel
//!   reads only `ctx.validity_bytes`; faulting the values extent for a
//!   bitmap question would violate O(streams-touched). A validity kernel
//!   must not consult section bytes.
//! - **Wrapped sections**: `entry.wrapper != 0` resolves an unwrapper from
//!   the [`CodecBinding`] at cursor open — absent unwrapper is a typed
//!   `WrapperUnsupported` refusal BEFORE any payload fault. The unwrapper
//!   returns a rebuilt UNWRAPPED section image (header `wrapper = 0`,
//!   uncompressed payload, tables), so kernels stay wrapper-oblivious
//!   (wrappers are orthogonal to encodings, spec §4).
//! - **Single-slice ABI limits**: `KernelCtx.overflow` and `DictSections`
//!   are single slices, so overflow/dict streams split across extents refuse
//!   typed (`Unsupported`) — an A-lane report item, not a silent skip.
//! - **`ctx.rows` carries the stream's PER-ENTRY dimension** (== `values`).
//!   The frozen reference kernels size the validity bitmap and mask output
//!   by `ctx.rows` (`validity_from_ctx`, `nonnull_count`), while a child
//!   stream's bitmap covers its VALUES (spec §6.6) — for root streams the
//!   two are the same number (the §2 closed form), for child streams only
//!   the value count is coherent. The rows-vs-values asymmetry in the
//!   reference kernels is an A-lane ABI report item; until ruled, this
//!   layer feeds every kernel the entry count in both fields.
//! - **The `validity` face self-counts from the validity stream** (its own
//!   gcount table for child streams, the §2 closed form for root streams) —
//!   consulting the values extent for a bitmap question would violate
//!   O(streams-touched). The values-vs-validity count cross-witness lives
//!   on the decode faces, where the values extent is faulted anyway.

use std::sync::Arc;

use pgrc_format::abi::{
    CodecRegistry, CodecVtable, DecodeOut, KernelCtx, KernelKey, MetaProbeFn, NarrowOut, Selection,
    ValidityVerdict,
};
use pgrc_format::dict::{DictLayout, DictSections};
use pgrc_format::enc::{EncodingId, Wrapper};
use pgrc_format::geom;
use pgrc_format::meta::{ProbeAnswer, MetaProbe};
use pgrc_format::part::{
    StreamRole, StreamSectionHdr, STREAMF_HAS_OVERFLOW, STREAM_SECTION_HDR_LEN,
};
use pgrc_format::{FormatError, FormatResult};

use crate::openpart::{read_vec, OpenPart, PassKey, ReadOwner, SegBuf};
use crate::streams::ParsedStream;
use crate::{ReadError, ReadResult};

// ---------------------------------------------------------------------------
// the binding seam (kernels + unwrappers arrive together, this crate frozen)
// ---------------------------------------------------------------------------

/// A wrapper's decoded side block (`Wrapper::side_block`), kept with a block-lazy dictionary payload for its
/// block faults; `bytes` is what it holds (charged to the part cache with the block tables).
pub struct SideTable {
    pub table: Box<dyn core::any::Any + Send + Sync>,
    pub bytes: usize,
}

/// Decode-side section unwrap (spec §6.4 `wrapper != 0`). Implemented by the
/// codec crate next to its kernels; registered in the [`CodecBinding`].
pub trait SectionUnwrapper: Sync {
    fn wrapper(&self) -> Wrapper;
    /// Decode a side block (`Wrapper::side_block` arms); `None` = this binding has no block grain for the arm.
    fn decode_side(&self, _side: &[u8]) -> FormatResult<Option<SideTable>> {
        Ok(None)
    }
    /// [`Self::unwrap_dict_payload_block`] for a side-block arm; `Ok(false)` = no block grain.
    fn unwrap_dict_payload_block_side(&self, _side: &SideTable, _src: &[u8], _dst: &mut [u8], _records: &[u8]) -> FormatResult<bool> {
        Ok(false)
    }
    /// Rebuild the unwrapped section image: `StreamSectionHdr` with
    /// `wrapper = 0`, uncompressed payload, frame/gcount tables addressing
    /// it. Bounds-validated, typed errors, no panic.
    fn unwrap_section(&self, hdr: &StreamSectionHdr, section: &[u8]) -> FormatResult<Vec<u8>>;
    /// D4216 / D4228: [`Self::unwrap_section`] asking `stop` between its pieces of work (the image's zero fill, each
    /// block); `Ok(None)` when it said stop. A binding without the arm builds whole (the default).
    fn unwrap_section_until(
        &self,
        hdr: &StreamSectionHdr,
        section: &[u8],
        _stop: &mut dyn FnMut() -> bool,
    ) -> FormatResult<Option<Vec<u8>>> {
        self.unwrap_section(hdr, section).map(Some)
    }
    /// [stack] Decompress ONE §6.4 wrapper block into exactly `dst`
    /// (compressed span from the block-offset table, uncompressed span from
    /// the frame table). `Ok(false)` = this binding cannot serve block
    /// grain — callers fall back to the whole-section unwrap (the default,
    /// so existing bindings keep today's behavior verbatim).
    fn unwrap_block(&self, _src: &[u8], _dst: &mut [u8]) -> FormatResult<bool> {
        Ok(false)
    }
    /// The DictPayload twins (pgrc format 2.2, spec §6.4/§7): `index` is the
    /// dictionary's resident v1 index image (12 B records; for the block
    /// face, the block's dict frame's records). A `FrontZstd` binding
    /// needs them for its expand; every other arm delegates to the
    /// role-agnostic faces above, so existing bindings are unchanged.
    fn unwrap_dict_payload_section(
        &self,
        hdr: &StreamSectionHdr,
        section: &[u8],
        _index: &[u8],
    ) -> FormatResult<Vec<u8>> {
        self.unwrap_section(hdr, section)
    }
    fn unwrap_dict_payload_block(
        &self,
        src: &[u8],
        dst: &mut [u8],
        _records: &[u8],
    ) -> FormatResult<bool> {
        self.unwrap_block(src, dst)
    }
    /// D3887: [`Self::unwrap_dict_payload_section`] asking `stop` before each block; `Ok(None)` when it said stop. A
    /// binding without the arm builds whole (the default).
    fn unwrap_dict_payload_section_until(
        &self,
        hdr: &StreamSectionHdr,
        section: &[u8],
        index: &[u8],
        _stop: &mut dyn FnMut() -> bool,
    ) -> FormatResult<Option<Vec<u8>>> {
        self.unwrap_dict_payload_section(hdr, section, index).map(Some)
    }
    /// The resident front-coded form of a FrontZstd section (`crate::dict_resident_frontcoded`):
    /// the inflated frames as `pgrc_format::dict::FcResident` under an unwrapped twin header.
    /// A binding without the arm refuses typed.
    fn unwrap_dict_payload_fc_section(
        &self,
        _hdr: &StreamSectionHdr,
        _section: &[u8],
        _index: &[u8],
    ) -> FormatResult<Vec<u8>> {
        Err(FormatError::Corrupt { at: "front-coded resident form without a binding" })
    }
    /// D3973: [`Self::unwrap_dict_payload_fc_section`] asking `stop` before each block; `Ok(None)` when it said stop.
    fn unwrap_dict_payload_fc_section_until(
        &self,
        hdr: &StreamSectionHdr,
        section: &[u8],
        index: &[u8],
        _stop: &mut dyn FnMut() -> bool,
    ) -> FormatResult<Option<Vec<u8>>> {
        self.unwrap_dict_payload_fc_section(hdr, section, index).map(Some)
    }
}

/// Everything the reader dispatches through: the kernel registry (spec
/// §19.5) plus the section unwrappers. M3-C ships a static binding with its
/// kernel lists and LZ4/Zstd unwrappers; this crate never changes to admit
/// them.
pub struct CodecBinding<'a> {
    pub registry: &'a CodecRegistry,
    pub unwrappers: &'a [&'a dyn SectionUnwrapper],
}

impl<'a> CodecBinding<'a> {
    fn unwrapper_for(&self, wrapper: Wrapper) -> Option<&'a dyn SectionUnwrapper> {
        self.unwrappers
            .iter()
            .copied()
            .find(|u| u.wrapper() == wrapper)
    }
}

/// A leaked binding over the reference Verbatim/Const vtables with no
/// unwrappers — test/tool scaffolding (leaks three small allocations per
/// call; product bindings are statics assembled by the codec crate).
pub fn reference_binding_leaked() -> &'static CodecBinding<'static> {
    let vts: &'static [&'static CodecVtable] =
        Box::leak(Box::new(pgrc_format::verbatim::reference_vtables()));
    let reg: &'static CodecRegistry = Box::leak(Box::new(CodecRegistry::new(vts)));
    Box::leak(Box::new(CodecBinding {
        registry: reg,
        unwrappers: &[],
    }))
}

// ---------------------------------------------------------------------------
// per-extent parsed state
// ---------------------------------------------------------------------------

/// One loaded, validated extent of a granule-organized stream.
struct ExtentState {
    bytes: SegBuf,
    /// Parsed once (kernels are allocation-free and never parse it).
    frame_table: Option<Vec<u32>>,
    /// Child streams: per-granule value counts from the gcount table;
    /// `None` = root stream (closed-form from part rows).
    gcounts: Option<Vec<u32>>,
    /// Validity streams: byte offset of each granule's bitmap inside the
    /// payload region (`prefix[i+1] - prefix[i]` = that granule's bytes).
    vbyte_prefix: Option<Vec<u32>>,
    payload_start: u32,
    payload_end: u32,
}

/// The bytes of an extent's parsed tables (frame table, gcounts, validity prefix), as their blocks hold them.
fn tables_bytes(t: [&Option<Vec<u32>>; 3]) -> u64 {
    t.iter().map(|v| v.as_ref().map_or(0, |v| v.capacity() as u64 * 4)).sum()
}

impl ExtentState {
    fn table_bytes(&self) -> u64 {
        tables_bytes([&self.frame_table, &self.gcounts, &self.vbyte_prefix])
    }

    fn payload(&self) -> &[u8] {
        &self.bytes.bytes()[self.payload_start as usize..self.payload_end as usize]
    }

    /// D3994 (B.1 W): this state while its cursor is parked: the image let go, the parsed tables kept. D4573: `passed` when a
    /// pass (D4315) parked it, not the cursor's park.
    fn park(self, passed: bool) -> ParkedExtent {
        let ExtentState { bytes, frame_table, gcounts, vbyte_prefix, payload_start, payload_end } = self;
        drop(bytes);
        ParkedExtent { frame_table, gcounts, vbyte_prefix, payload_start, payload_end, passed }
    }
}

/// D3994 (B.1 W): a loaded extent's state while its cursor is parked (`StreamCursor::park_images`): the image let go (the
/// cursor holds no clone, so the part cache may evict it and its memory goes with the eviction), the parsed tables kept:
/// they describe the same immutable, CRC-checked bytes whoever faults them again. The cursor's next use of the extent
/// takes the image from the part cache (`extent_image`: a lookup while it is cached, else the cold fault) without
/// parsing it again.
struct ParkedExtent {
    frame_table: Option<Vec<u32>>,
    gcounts: Option<Vec<u32>>,
    vbyte_prefix: Option<Vec<u32>>,
    payload_start: u32,
    payload_end: u32,
    /// D4573: a pass (D4315) parked it while its statement ran ([`ExtSlot::pass`]): the cursor's next park drops it whole
    /// (E2). One the park parked stays parked with its tables at any later park.
    passed: bool,
}

impl ParkedExtent {
    fn unpark(self, bytes: SegBuf) -> ExtentState {
        let ParkedExtent { frame_table, gcounts, vbyte_prefix, payload_start, payload_end, passed: _ } = self;
        ExtentState { bytes, frame_table, gcounts, vbyte_prefix, payload_start, payload_end }
    }
}

/// One extent of a cursor's stream: not loaded, loaded (its image held), or (D3994 B.1 W) parked: its image let go at the
/// cursor's park, its parsed tables kept for the next use.
enum ExtSlot {
    Empty,
    Loaded(ExtentState),
    Parked(ParkedExtent),
}

impl ExtSlot {
    fn loaded(&self) -> Option<&ExtentState> {
        match self {
            ExtSlot::Loaded(st) => Some(st),
            _ => None,
        }
    }

    fn is_loaded(&self) -> bool {
        matches!(self, ExtSlot::Loaded(_))
    }

    fn table_bytes(&self) -> u64 {
        match self {
            ExtSlot::Empty => 0,
            ExtSlot::Loaded(st) => st.table_bytes(),
            ExtSlot::Parked(p) => tables_bytes([&p.frame_table, &p.gcounts, &p.vbyte_prefix]),
        }
    }

    /// D3994 (B.1 W): a loaded extent lets its image go (its tables stay). E2: an extent a pass parked (D4315) goes whole,
    /// its tables with it; the bytes of the tables dropped, for the cursor's `kept` (0 for a loaded, an empty or a parked
    /// slot). It dropped them silently before: the cursor's charge kept them, and the slot depot's trim of a parked set
    /// handed the trimming backend's account bytes no free took back (+17 MiB on a 10 GB join suite at W8). D4573: an
    /// extent a park parked stays as it is, its tables kept: a worker set's cursor is parked twice at its statement's
    /// end (its source's unbind, then the move of its use to the slot depot), and the second park dropped every table
    /// the first one kept.
    fn park(&mut self) -> u64 {
        match core::mem::replace(self, ExtSlot::Empty) {
            ExtSlot::Loaded(st) => {
                *self = ExtSlot::Parked(st.park(false));
                0
            }
            ExtSlot::Parked(p) if !p.passed => {
                *self = ExtSlot::Parked(p);
                0
            }
            gone => gone.table_bytes(),
        }
    }

    /// D4315: a pass: a loaded extent lets its image go, its tables kept and the slot marked passed (D4573: the cursor's
    /// next park drops it whole, E2). Anything else stays as it is.
    fn pass(&mut self) {
        match core::mem::replace(self, ExtSlot::Empty) {
            ExtSlot::Loaded(st) => *self = ExtSlot::Parked(st.park(true)),
            other => *self = other,
        }
    }
}

/// A granule's framed values as `StreamCursor::framed` resolves them (the fields of pgrc_codec bytefor `Framed`).
#[derive(Clone, Copy, Debug)]
pub struct FramedGranule<'a> {
    /// The extent's payload region (past the section header, before its tables): frame offsets index it.
    pub payload: &'a [u8],
    /// The extent's frame table (payload offset of each frame).
    pub frame_table: &'a [u32],
    /// The granule's first frame.
    pub fbase: u32,
    /// The granule's value count.
    pub values: u32,
}

/// A single-extent byte-run stream (overflow / dict index / dict payload):
/// the payload region inside its one loaded section. Shared with the lazy
/// dict handle (`crate::dicthandle`).
pub(crate) struct RegionState {
    pub(crate) bytes: SegBuf,
    pub(crate) payload_start: u32,
    pub(crate) payload_end: u32,
}

impl RegionState {
    pub(crate) fn payload(&self) -> &[u8] {
        &self.bytes.bytes()[self.payload_start as usize..self.payload_end as usize]
    }
}

/// Payload bounds of a section (spec §6.4): past the header, before the
/// first table. Mirrors the reference codec's region math. `pub(crate)`
/// since M3-L3: the frame-lazy dict payload region (`crate::dicthandle`)
/// computes the same bounds from its extent-0 fault.
pub(crate) fn payload_bounds(hdr: &StreamSectionHdr, section_len: usize) -> FormatResult<(u32, u32)> {
    let end = if hdr.frame_table_off != 0 {
        hdr.frame_table_off as usize
    } else if hdr.gcount_table_off != 0 {
        hdr.gcount_table_off as usize
    } else {
        section_len
    };
    if end < STREAM_SECTION_HDR_LEN || end > section_len {
        return Err(FormatError::Bounds {
            at: "payload region",
        });
    }
    // Width-safe narrowing (spec §6.4, idx-196): the section-relative table
    // offsets are u32-typed fields, but an ASSEMBLED multi-extent byte-run
    // region (`load_region`) sums per-extent lengths — each capped at
    // u32::MAX, the SUM is not — so the `section_len` fallback can exceed
    // 2^32. A bare `end as u32` would truncate to a value BELOW payload_start
    // (an inverted window that panics `payload()` or underflows downstream
    // pointer math). Reject any payload end that does not fit the returned
    // u32 with a typed, catchable error; valid (<=4 GiB) regions are
    // unaffected. `end >= STREAM_SECTION_HDR_LEN` is already proven, so the
    // returned pair is ordered by construction.
    let end = u32::try_from(end).map_err(|_| FormatError::Bounds {
        at: "payload region exceeds u32",
    })?;
    Ok((STREAM_SECTION_HDR_LEN as u32, end))
}

// ---------------------------------------------------------------------------
// StreamCursor
// ---------------------------------------------------------------------------

/// Where a cursor's faulted images live (D259).
///
/// `Shared` (the default): every extent / region a face faults is admitted
/// to the part's residency plane (`OpenPart`'s segment and unwrapped-image
/// caches) and every loaded extent state stays with the cursor — the hot
/// posture of the Gravity / SQE scans, which re-read the same columns.
///
/// `Private`: the bulk-read posture of a one-pass forward scan (the row
/// executor's table scan — PostgreSQL's BAS_BULKREAD ring). A resident image
/// is still served from the plane, but a miss is read into a buffer only this
/// cursor holds (never inserted, never counted in `resident`), and the cursor
/// keeps ONE values extent and ONE validity extent: loading the next extent
/// drops the previous one. What a private cursor pins is therefore bounded
/// by one extent per stream plus the column's dictionary / overflow region,
/// whatever the relation's size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Residency {
    Shared,
    Private,
    /// C1 L5, the storage readers' posture under the cap (`crate::mem`): a hit
    /// is served and sets the image's CLOCK ref bit; a miss is read, then
    /// admitted cold if the cap has room (otherwise only this cursor holds it,
    /// as Private). Nothing but its holders keeps an admitted image: the ring
    /// evicts it when they are gone. Loads keep one extent per stream, as
    /// Private does, so what a cursor pins stays bounded.
    Cached,
}

/// The per-(column, path) decode cursor: vtable + companion streams resolved
/// at open, extents faulted lazily per face call. Worker-confined (`&mut
/// self` faces, R1–R6); the underlying part is shared.
/// D4315: the passed extents' keys a cursor hands its part in one call.
const PASS_CHUNK: usize = 16;

/// The most footer-listed sections one values run carries (`prefetch_granule_runs_tail`): a lane's Stats, Bloom and PSMA.
pub const TAIL_SECTIONS: usize = 3;

/// Which WRAPPED dictionary regions a values run carries (`StreamCursor::prefetch_granule_runs_tail`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DictTail {
    pub index: bool,
    pub payload: bool,
}

impl DictTail {
    pub const NONE: DictTail = DictTail { index: false, payload: false };
}

/// D4554: the own tables (`kept`) of the cursors one reader keeps open across statements (a granule source's), on one
/// charge the reader moves once for all of them ([`KeptPool::home`], [`KeptPool::transfer`], [`KeptPool::park`]): each kept
/// cursor's own charge moved at every statement's bind, window, end, park and take, thousands of moves a statement on a
/// wide worker set. A pooled cursor ([`StreamCursor::set_pool`]) commits and frees its tables' bytes on the pool as they
/// change, and its drop takes its bytes off.
pub struct KeptPool {
    inner: pgsync::Mutex<PoolCharge>,
}

struct PoolCharge {
    /// On `at`'s owner and kind while `at` names one (else `Charge::NONE`, and the pooled bytes are no owner's).
    charge: memledger::Charge,
    at: ReadOwner,
    /// The pooled cursors' bytes (what `charge` holds while `at` names an owner).
    bytes: u64,
}

impl Default for KeptPool {
    fn default() -> KeptPool {
        KeptPool::new()
    }
}

impl KeptPool {
    pub const fn new() -> KeptPool {
        KeptPool { inner: pgsync::Mutex::new(PoolCharge { charge: memledger::Charge::NONE, at: None, bytes: 0 }) }
    }

    /// The bytes of a shared pool's own block (`Arc`'s two counts and the pool).
    pub const SHARED_BYTES: u64 = (2 * core::mem::size_of::<usize>() + core::mem::size_of::<KeptPool>()) as u64;

    /// D4554: a pool its reader's cursors share, its own block counted in its bytes ([`Self::SHARED_BYTES`]): charged where
    /// the pool is homed, from its first home to its drop, like its cursors' tables.
    pub fn shared() -> Arc<KeptPool> {
        let p = Arc::new(KeptPool::new()); // alloc: Node charged p (its own charge counts the block: SHARED_BYTES)
        p.lock().bytes = Self::SHARED_BYTES;
        p
    }

    fn lock(&self) -> pgsync::MutexGuard<'_, PoolCharge> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    /// On owner `to` from now on (a cursor's `kept_home` rule: the breaker a source names, its statement at the bind, its
    /// session at the end), when it was on another owner or kind: the bytes move there in one step. `None` gives them back
    /// to this thread (no owner to name).
    pub fn home(&self, to: ReadOwner) {
        let mut g = self.lock();
        if g.at == to {
            return;
        }
        match to {
            Some((o, k)) => {
                let mut c = memledger::Charge::open(o, crate::openpart::release_of(o), k, memledger::charge::Door::Heap, 0);
                let old = core::mem::replace(&mut g.charge, memledger::Charge::NONE);
                if g.at.is_some() {
                    c.absorb(old);
                } else {
                    // the bytes sat on no owner: on this thread, where the open takes them (a cursor's `rehome_kept`)
                    drop(old);
                    c.commit(g.bytes);
                }
                g.charge = c;
            }
            None => g.charge = memledger::Charge::NONE,
        }
        g.at = to;
    }

    /// To owner `to`, released at `release` (the slot depot's moves: its holder at a worker set's park, the taking statement
    /// at its take), the kind kept; from no owner, homed there (`Kind::Region`).
    pub fn transfer(&self, to: memledger::Owner, release: memledger::Release) {
        let mut g = self.lock();
        match g.at {
            Some((o, k)) if o != to => {
                g.charge.transfer(to, release);
                g.at = Some((to, k));
            }
            Some(_) => {}
            None => {
                drop(g);
                self.home(Some((to, memledger::Kind::Region)));
            }
        }
    }

    /// The reader's statement ended and it keeps its cursors (C8, a cursor's `keep_past_statement`): bytes on a statement,
    /// a portal or a breaker go to that account's session.
    pub fn park(&self) {
        let at = self.lock().at;
        if let Some((o, _)) = at.filter(|(o, _)| !matches!(o, memledger::Owner::Session(_) | memledger::Owner::Cache(_))) {
            self.home(Some((memledger::Owner::Session(o.acct()), memledger::Kind::Region)));
        }
    }

    /// The pool's owner now.
    pub fn owner(&self) -> Option<memledger::Owner> {
        self.lock().at.map(|(o, _)| o)
    }

    /// The pooled cursors' bytes.
    pub fn bytes(&self) -> u64 {
        self.lock().bytes
    }

    fn commit(&self, n: u64) {
        let mut g = self.lock();
        g.bytes += n;
        if g.at.is_some() {
            g.charge.commit(n);
        }
    }

    fn free(&self, n: u64) {
        let mut g = self.lock();
        let n = n.min(g.bytes);
        g.bytes -= n;
        if g.at.is_some() {
            g.charge.free(n);
        }
    }
}

pub struct StreamCursor<'b> {
    /// The cursor's own tables (its extent slots, its stream copies, the loaded extents' parsed tables): `kept_bytes` of
    /// them, charged to [`Self::kept_home`] (`None`: no owner to name), or (D4554) to its reader's [`KeptPool`]. Declared
    /// first, so it drops before them.
    kept: Option<memledger::Charge>,
    kept_bytes: u64,
    /// D4554: the reader's pool the tables are charged to instead ([`Self::set_pool`]); its owner moves are the reader's.
    pool: Option<Arc<KeptPool>>,
    /// RESIDUE (the slot depot): where `transfer_kept` put `kept` apart from the part use, until the next owner change.
    kept_to: Option<memledger::Owner>,
    part: Arc<OpenPart>,
    binding: &'b CodecBinding<'b>,
    vt: &'b CodecVtable,
    /// The NORMALIZED dispatch key (`enc::stream_kernel_key` over the entry
    /// fields) — every ctx this cursor builds carries it, never raw fields.
    key: KernelKey,
    attno: u32,
    path_ord: u32,
    /// Cloned stream facts (entry + extents) for the values stream and
    /// companions — parsed-directory content, small.
    values: ParsedStream,
    validity: Option<ParsedStream>,
    overflow: Option<ParsedStream>,
    dict_index: Option<ParsedStream>,
    dict_payload: Option<ParsedStream>,
    vstate: Vec<ExtSlot>,
    valstate: Vec<ExtSlot>,
    ovf_state: Option<RegionState>,
    dict_state: Option<(RegionState, RegionState, u32, pgrc_format::dict::DictCharLenForm)>,
    /// D3994 (B.1 W): the dictionary regions were loaded before the park: loading them again is the same cursor's, not a
    /// new whole-payload assembly for the census.
    parked_dict: bool,
    /// D4554: something loaded or passed since the last park (an extent's image, a slot a pass parked, the overflow or
    /// dictionary regions): a park with neither has nothing to let go, and a backend parks every kept cursor at each
    /// statement's end.
    loaded: bool,
    residency: Residency,
    /// D4315 (`set_pass_behind`): loading a values or validity extent passes the loaded ones before it.
    pass_on_load: bool,
    /// D4315: the end (exclusive granule) of the furthest values extent this cursor loaded since [`Self::set_pass_behind`].
    read_hi: u32,
    /// D4347 (`hold_for_walks`): a whole-column walk of this cursor's column reads it again in the same claim; the cursor
    /// passes by band (`pass_behind`) instead of on load.
    walked: bool,
    /// D4351: this cursor's id as a live reader of its part (`OpenPart::register_reader`; 0: not one), and the granule
    /// it last published as its position (it reads nothing below it again).
    reader: u32,
    low: u32,
    /// D3003: the part's contents stay resident while this cursor lives (its loaded regions alias them). C8: the use is its
    /// opener's (`transfer_use` moves a kept cursor's).
    part_use: crate::openpart::PartUse,
    /// The breaker the reading scan feeds (`set_owner`); `None`: the cursor's statement ([`Self::read_owner`]).
    owner: ReadOwner,
}

impl core::fmt::Debug for StreamCursor<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StreamCursor")
            .field("attno", &self.attno)
            .field("path_ord", &self.path_ord)
            .finish_non_exhaustive()
    }
}

/// D4351: a dropped cursor reads no more: it leaves its part's live readers. D4554: a pooled one's tables leave its pool
/// (the bytes onto this thread, where they free).
impl Drop for StreamCursor<'_> {
    fn drop(&mut self) {
        if self.reader != 0 {
            self.part.deregister_reader(self.reader);
        }
        if let Some(p) = self.pool.take() {
            p.free(self.kept_bytes);
        }
    }
}

impl<'b> StreamCursor<'b> {
    /// Open the cursor for (attno, path_ord) over the `Values` role.
    pub fn open(
        part: Arc<OpenPart>,
        binding: &'b CodecBinding<'b>,
        attno: u32,
        path_ord: u32,
    ) -> ReadResult<StreamCursor<'b>> {
        StreamCursor::open_role(part, binding, attno, path_ord, StreamRole::Values)
    }

    /// Open the cursor over a value-bearing role (`Values`, `ChildValues`,
    /// `Sizes` — the nesting currency, spec §6.5): resolve the vtable from
    /// the registry (unknown/reserved encoding IDs refuse first, spec
    /// §19.5), resolve companion streams, resolve the unwrapper if the
    /// stream is wrapped. Faults at most the StreamDir section.
    pub fn open_role(
        part: Arc<OpenPart>,
        binding: &'b CodecBinding<'b>,
        attno: u32,
        path_ord: u32,
        role: StreamRole,
    ) -> ReadResult<StreamCursor<'b>> {
        part.note_stream_use(attno, path_ord);
        if !matches!(
            role,
            StreamRole::Values | StreamRole::ChildValues | StreamRole::Sizes
        ) {
            return Err(ReadError::Unsupported {
                what: "cursor over a non-value role",
            });
        }
        let dir = part.stream_directory()?;
        let values = dir
            .lookup(attno, path_ord, role)
            .ok_or(ReadError::StreamMissing {
                attno,
                path_ord,
                role: role.as_u8(),
            })?
            .clone(); // alloc: Node charged kept_home
        // Adjudicate the encoding ID before any kernel-table walk (typed
        // unknown-vs-reserved split, born-RED-tested upstream).
        EncodingId::resolve(values.entry.encoding)?;
        // A STRUCTURAL parent entry (ArrayDual — TY-1) is an election
        // marker, not a decodable stream: readers compose through the
        // `Sizes`/`ChildValues` substream cursors + the codec's
        // `assemble_array_datums`. Refuse typed BEFORE the kernel-key
        // normalization (which would report it as corruption).
        if values.entry.encoding == EncodingId::ArrayDual.as_u16() {
            return Err(ReadError::Unsupported {
                what: "structural parent stream (compose via Sizes/ChildValues)",
            });
        }
        // Wrapper resolution at open: a wrapped stream without a registered
        // unwrapper refuses before any payload fault.
        let wrapper = Wrapper::from_u8(values.entry.wrapper)?;
        if wrapper != Wrapper::None && binding.unwrapper_for(wrapper).is_none() {
            return Err(ReadError::Format(FormatError::WrapperUnsupported {
                wrapper: wrapper.as_u8(),
            }));
        }
        // Resolve through the ONE normalization (spec §6.3/§19.5, A-lane
        // amendment M3-A2): the entry width byte is per-encoding vocabulary
        // (DICT_CODES: max code width — a stats fact), the dispatch width is
        // normalized. A raw-field key cannot resolve a §6.3-conformant dict
        // stream (the M3-G gap-1 seam, closed here).
        let key = pgrc_format::enc::stream_kernel_key(
            values.entry.encoding,
            values.entry.class,
            values.entry.width,
        )?;
        let vt = binding.registry.resolve(key)?;
        // Validity binding: a child stream's OWN bitmap is VALUE-aligned
        // (spec §6.6 — "a child stream's bitmap covers its VALUES") and
        // binds exactly as in v3. The ONE exception is a ChildValues stream
        // under an ArrayDual STRUCTURAL parent (TY-1): there the validity
        // stream at this (attno, path_ord) is the PARENT'S row-aligned
        // bitmap — elements carry no validity by construction (null
        // elements refuse the split at seal) — and slicing a row-aligned
        // bitmap against an element count would refuse typed. Discriminate
        // on the directory fact itself: the structural parent entry.
        let under_array_dual_parent = role == StreamRole::ChildValues
            && dir
                .lookup(attno, path_ord, StreamRole::Values)
                .is_some_and(|p| p.entry.encoding == EncodingId::ArrayDual.as_u16());
        let validity = if under_array_dual_parent {
            None
        } else {
            dir.lookup(attno, path_ord, StreamRole::Validity).cloned()
        };
        let overflow = if values.entry.flags & STREAMF_HAS_OVERFLOW != 0 {
            Some(
                dir.lookup(attno, path_ord, StreamRole::Overflow)
                    .ok_or(ReadError::Format(FormatError::Corrupt {
                        at: "HAS_OVERFLOW without Overflow stream",
                    }))?
                    .clone(), // alloc: Node charged kept_home
            )
        } else {
            None
        };
        let is_dict = values.entry.encoding == EncodingId::DictCodes.as_u16();
        let (dict_index, dict_payload) = if is_dict {
            let di = dir
                .lookup(attno, path_ord, StreamRole::DictIndex)
                .ok_or(ReadError::Format(FormatError::Corrupt {
                    at: "dict column without DictIndex stream",
                }))?
                .clone(); // alloc: Node charged kept_home
            let dp = dir
                .lookup(attno, path_ord, StreamRole::DictPayload)
                .ok_or(ReadError::Format(FormatError::Corrupt {
                    at: "dict column without DictPayload stream",
                }))?
                .clone(); // alloc: Node charged kept_home
            (Some(di), Some(dp))
        } else {
            (None, None)
        };
        // Companion streams share the refusal-before-fault pin (CMP-B: the
        // byte-run stream class can be wrapped now — a wrapped dict/overflow
        // stream without its unwrapper refuses TYPED at open, before any
        // payload fault; validity is checked under the same law).
        // bounded("const-domain: the four companion streams")
        for companion in [
            validity.as_ref(),
            overflow.as_ref(),
            dict_index.as_ref(),
            dict_payload.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            let w = Wrapper::from_u8(companion.entry.wrapper)?;
            if w != Wrapper::None && binding.unwrapper_for(w).is_none() {
                return Err(ReadError::Format(FormatError::WrapperUnsupported {
                    wrapper: w.as_u8(),
                }));
            }
        }
        let n_v = values.extents.len();
        let n_val = validity.as_ref().map(|s| s.extents.len()).unwrap_or(0);
        let mut cur = StreamCursor {
            kept: None,
            kept_bytes: 0,
            pool: None,
            kept_to: None,
            part_use: part.use_part(),
            part,
            binding,
            vt,
            key,
            attno,
            path_ord,
            values,
            validity,
            overflow,
            dict_index,
            dict_payload,
            vstate: (0..n_v).map(|_| ExtSlot::Empty).collect(), // alloc: Node charged kept_home
            valstate: (0..n_val).map(|_| ExtSlot::Empty).collect(), // alloc: Node charged kept_home
            ovf_state: None,
            dict_state: None,
            parked_dict: false,
            loaded: false,
            pass_on_load: false,
            read_hi: 0,
            walked: false,
            reader: 0,
            low: 0,
            residency: Residency::Shared,
            owner: None,
        };
        let slots = |v: &Vec<ExtSlot>| (v.capacity() * core::mem::size_of::<ExtSlot>()) as u64;
        let streams = [Some(&cur.values), cur.validity.as_ref(), cur.overflow.as_ref(), cur.dict_index.as_ref(), cur.dict_payload.as_ref()];
        let copies: u64 = streams.into_iter().flatten().map(|s| (s.extents.capacity() * core::mem::size_of::<pgrc_format::part::ExtentRecord>()) as u64).sum();
        cur.kept_bytes = slots(&cur.vstate) + slots(&cur.valstate) + copies;
        cur.rehome_kept();
        Ok(cur)
    }

    /// Switch this cursor's residency posture (see [`Residency`]); call it
    /// before the first face call.
    pub fn with_residency(mut self, residency: Residency) -> StreamCursor<'b> {
        self.residency = residency;
        self
    }

    /// Loaded (values, validity) extent states — the residency tests' witness.
    #[cfg(test)]
    pub(crate) fn loaded_extents(&self) -> (usize, usize) {
        (
            self.vstate.iter().filter(|s| s.is_loaded()).count(),
            self.valstate.iter().filter(|s| s.is_loaded()).count(),
        )
    }

    /// The values stream entry (encoding, flags, width — the resolved key).
    pub fn entry(&self) -> &pgrc_format::part::StreamEntry {
        &self.values.entry
    }

    pub fn attno(&self) -> u32 {
        self.attno
    }

    /// C8: this cursor's part use moves to owner `to` (released at `release`): a cursor a parked Node keeps past its
    /// statement (D1507) goes to the backend's session.
    pub fn transfer_use(&mut self, to: memledger::Owner, release: memledger::Release) {
        self.part_use.transfer(to, release);
        self.kept_to = None;
        // a use moved off a running statement reads for no breaker of it: the cursor's tables follow the use (the session,
        // the slot depot), and the next breaker names itself again (set_owner)
        if !matches!(to, memledger::Owner::Statement { .. } | memledger::Owner::Portal { .. }) {
            self.owner = None;
        }
        // D3994 (B.1 W): a parked worker set's cursor lets its images go, as an entry park's does
        if self.part_use.is_parked() {
            self.park_images();
        }
        self.rehome_kept();
    }

    /// D4554: `transfer_use` with the use moved in `group` (its part's, `PartUse::transfer_in`: one lock and one parked
    /// count a part for a source's cursors over it).
    pub fn transfer_use_in(&mut self, group: &mut crate::openpart::UseMoves<'_>, to: memledger::Owner, release: memledger::Release) {
        self.part_use.transfer_in(group, to, release);
        self.kept_to = None;
        if !matches!(to, memledger::Owner::Statement { .. } | memledger::Owner::Portal { .. }) {
            self.owner = None;
        }
        if self.part_use.is_parked() {
            self.park_images();
        }
        self.rehome_kept();
    }

    /// D4554: the part this cursor reads (its source groups its use moves by it).
    pub fn part(&self) -> &Arc<OpenPart> {
        &self.part
    }

    /// C8: the owner of this cursor's part use now.
    pub fn use_owner(&self) -> memledger::Owner {
        self.part_use.owner()
    }

    /// The breaker the scan reading this cursor feeds, and its kind lane (its source's `Window::op_owner`); `None`: the
    /// cursor's statement.
    pub fn set_owner(&mut self, owner: ReadOwner) {
        self.owner = owner;
        self.rehome_kept();
    }

    /// RESIDUE (memlead's slot depot, wave2-base f3d401d970a): the cursor's own tables (`kept`) move to `to`, released at
    /// `release`; its part use stays where it is. For the source's `Source::transfer_held`: at a worker set's park `to` =
    /// `Cache(SlotDepot)`, `Evict` (the use went to `Cache(Parked)` first, through `kept_uses_to`), at its take the taking
    /// statement's `charge::context()`. The next owner change (`set_owner`, `transfer_use`, `keep_past_statement`, a
    /// `rejoin` that moves the use) homes them by [`Self::kept_home`] again.
    pub fn transfer_kept(&mut self, to: memledger::Owner, release: memledger::Release) {
        // D4554: a pooled cursor's tables move with its pool
        if self.pool.is_some() {
            return;
        }
        self.kept_to = Some(to);
        match self.kept.as_mut() {
            Some(c) => c.transfer(to, release),
            None => {
                self.kept = Some(memledger::Charge::open(
                    to,
                    release,
                    memledger::Kind::Region,
                    memledger::charge::Door::Heap,
                    self.kept_bytes,
                ))
            }
        }
    }

    /// Where the cursor's own tables are charged: the breaker its source named, else where `transfer_kept` put them, else
    /// its part use's owner (its statement or portal; its session or a depot while parked: `transfer_use` moves them with
    /// the use).
    fn kept_home(&self) -> ReadOwner {
        self.owner.or(self.kept_to.map(|o| (o, memledger::Kind::Region))).or(match self.part_use.owner() {
            memledger::Owner::Cache(memledger::Holder::Unbound) => None,
            o => Some((o, memledger::Kind::Region)),
        })
    }

    /// D4554: this cursor's tables onto `pool` from now on (its own charge's bytes there in one step, on this thread); the
    /// owner moves are the pool's from then on. A cursor joins one pool once.
    pub fn set_pool(&mut self, pool: Arc<KeptPool>) {
        if self.pool.is_some() {
            return;
        }
        // the own charge's bytes back on this thread first, where the pool takes them
        self.kept = None;
        self.kept_to = None;
        pool.commit(self.kept_bytes);
        self.pool = Some(pool);
    }

    fn rehome_kept(&mut self) {
        if self.pool.is_some() {
            return;
        }
        let home = self.kept_home();
        if self.kept.as_ref().map(|c| (c.owner(), c.kind())) == home {
            return;
        }
        match (&mut self.kept, home) {
            (Some(c), Some((o, k))) if c.kind() == k => c.transfer(o, crate::openpart::release_of(o)),
            _ => {
                // the old charge's bytes back on this thread first, where the new one takes them
                self.kept = None;
                let n = self.kept_bytes;
                self.kept = home.map(|h| crate::openpart::read_charge(Some(h), n));
            }
        }
    }

    /// `n` more bytes of the cursor's own tables (`kept`, `pool`, `kept_bytes`: the fields, so a stream borrowed beside
    /// stays).
    fn kept_add(kept: &mut Option<memledger::Charge>, pool: &Option<Arc<KeptPool>>, bytes: &mut u64, n: u64) {
        *bytes += n;
        if let Some(p) = pool {
            p.commit(n);
        } else if let Some(c) = kept.as_mut() {
            c.commit(n);
        }
    }

    fn kept_sub(kept: &mut Option<memledger::Charge>, pool: &Option<Arc<KeptPool>>, bytes: &mut u64, n: u64) {
        let n = n.min(*bytes);
        *bytes -= n;
        if let Some(p) = pool {
            p.free(n);
        } else if let Some(c) = kept.as_mut() {
            c.free(n);
        }
    }

    /// E2's witness (tests): the bytes `kept_bytes` should hold, counted from the fields now: the extent slots and stream
    /// copies the open counted, and every loaded or parked extent's tables.
    #[cfg(test)]
    fn tables_now(&self) -> u64 {
        let slots = |v: &Vec<ExtSlot>| (v.capacity() * core::mem::size_of::<ExtSlot>()) as u64;
        let streams = [Some(&self.values), self.validity.as_ref(), self.overflow.as_ref(), self.dict_index.as_ref(), self.dict_payload.as_ref()];
        let copies: u64 = streams.into_iter().flatten().map(|s| (s.extents.capacity() * core::mem::size_of::<pgrc_format::part::ExtentRecord>()) as u64).sum();
        slots(&self.vstate) + slots(&self.valstate) + copies + self.vstate.iter().chain(self.valstate.iter()).map(ExtSlot::table_bytes).sum::<u64>()
    }

    /// The bytes of the cursor's own tables and their owner now.
    pub fn kept(&self) -> (u64, Option<memledger::Owner>) {
        match &self.pool {
            Some(p) => (self.kept_bytes, p.owner()),
            None => (self.kept_bytes, self.kept.as_ref().map(|c| c.owner())),
        }
    }

    /// The owner of the buffers this cursor's reads allocate: the breaker its source named, else the statement or portal
    /// its part use names (`Kind::Region`); `None` while parked.
    pub fn read_owner(&self) -> ReadOwner {
        self.owner.or_else(|| match self.part_use.owner() {
            o @ (memledger::Owner::Statement { .. } | memledger::Owner::Portal { .. }) => Some((o, memledger::Kind::Region)),
            _ => None,
        })
    }

    /// C8: the cursor outlives its statement in a parked Node (D1507: its source unbinds and keeps it): a use its statement
    /// or portal owns passes to that account's session, which owns the backend's estate between statements.
    pub fn keep_past_statement(&mut self) {
        let o = self.part_use.owner();
        if matches!(o, memledger::Owner::Statement { .. } | memledger::Owner::Portal { .. }) {
            self.part_use.transfer(memledger::Owner::Session(o.acct()), memledger::Release::Session);
        }
        self.owner = None;
        self.kept_to = None;
        self.rehome_kept();
        // D3994 (B.1 W): and it lets its images go: a parked executor holds no part bytes
        self.park_images();
    }

    /// D4554: `keep_past_statement` with the use moved in `group` (`transfer_use_in`'s).
    pub fn keep_past_statement_in(&mut self, group: &mut crate::openpart::UseMoves<'_>) {
        let o = self.part_use.owner();
        if matches!(o, memledger::Owner::Statement { .. } | memledger::Owner::Portal { .. }) {
            self.part_use.transfer_in(group, memledger::Owner::Session(o.acct()), memledger::Release::Session);
        }
        self.owner = None;
        self.kept_to = None;
        self.rehome_kept();
        self.park_images();
    }

    /// D4554 (`kept_parked`): `keep_past_statement_in` with the use parked on its statement or portal
    /// (`PartUse::park_pending_in`): the slot depot's park moves it once (`transfer_use_in`), else the statement's end to
    /// the session (`keep_past_statement_in`).
    pub fn keep_pending_park_in(&mut self, group: &mut crate::openpart::UseMoves<'_>) {
        self.part_use.park_pending_in(group);
        self.owner = None;
        self.kept_to = None;
        self.rehome_kept();
        self.park_images();
    }

    /// D3994 (B.1 W): the cursor lets go what it loaded (its statement ended; it reads nothing until a statement binds it
    /// again): every extent's and region's image clone drops, so the part cache may evict the part (when only parked
    /// executors use it and the memory policy's gate opens) and its memory goes then. The parsed extent tables stay
    /// ([`ParkedExtent`]): the next use takes the image back from the part cache, a lookup while it is cached, the cold
    /// fault after an eviction. No datum of the ended statement points into the images any more. E2: an extent a pass
    /// parked during the statement (D4315: its reader went past it) goes whole here, and its tables leave `kept`. D4573: a
    /// second park (a worker set's cursor parks at its source's unbind and again when its use moves to the slot depot)
    /// keeps every table the first one kept.
    pub fn park_images(&mut self) {
        if !core::mem::take(&mut self.loaded) {
            return;
        }
        let mut gone = 0u64;
        // bounded("other: one cursor's values and validity extents, the stream's directory extents")
        for s in self.vstate.iter_mut().chain(self.valstate.iter_mut()) {
            gone += s.park();
        }
        Self::kept_sub(&mut self.kept, &self.pool, &mut self.kept_bytes, gone);
        self.ovf_state = None;
        if self.dict_state.take().is_some() {
            self.parked_dict = true;
        }
    }

    /// D4315: the scan is past every granule below `g` (its window over them was consumed whole): the values and validity
    /// extents that end at or below `g` let their images go (their parsed tables stay, as [`Self::park_images`] keeps
    /// them) and the part marks them passed (`OpenPart::pass_extents`), so a content trim may evict them while the part
    /// stays in use; a later read of one takes it back from the cache, or faults it again. `Shared` cursors only (a
    /// private one keeps one extent already). Returns the bytes newly marked.
    pub fn pass_behind(&mut self, g: u32) -> u64 {
        if self.residency != Residency::Shared {
            return 0;
        }
        let tag = self.stream_tag();
        // D4351: the reader's position (the band it enters) goes with its first pass
        let mut at = self.position(g);
        let mut keys = [PassKey::default(); PASS_CHUNK];
        let mut n = 0usize;
        let mut added = 0u64;
        // bounded("other: one cursor's values and validity extents, each parked once")
        for (ps, slots) in [(Some(&self.values), &mut self.vstate), (self.validity.as_ref(), &mut self.valstate)] {
            let Some(ps) = ps else { continue };
            // bounded("other: one stream's extents of one part, each parked once")
            for (i, slot) in slots.iter_mut().enumerate() {
                let Some(rec) = ps.extents.get(i) else { break };
                let g_end = rec.granule_start.saturating_add(rec.granule_count);
                if g_end > g {
                    break;
                }
                if slot.is_loaded() {
                    slot.pass();
                    // the next park drops the passed slot whole (E2): it parks (`loaded`), loaded since or not
                    self.loaded = true;
                    keys[n] = PassKey { key: (rec.file_off, rec.len), g_end };
                    n += 1;
                    if n == PASS_CHUNK {
                        added += self.part.pass_extents(tag, at.take(), &keys[..n]);
                        n = 0;
                    }
                }
            }
        }
        if n > 0 || at.is_some() {
            added += self.part.pass_extents(tag, at, &keys[..n]);
        }
        added
    }

    /// D4351: this cursor's stream as its part's passed extents and live readers name it.
    fn stream_tag(&self) -> crate::openpart::StreamTag {
        (self.attno, self.path_ord, self.values.role as u8)
    }

    /// D4351: the position to publish for a live reader now reading granule `g` (`None`: not a live reader, or no move).
    #[inline]
    fn position(&mut self, g: u32) -> Option<(u32, u32)> {
        if self.reader == 0 || self.low == g {
            return None;
        }
        self.low = g;
        Some((self.reader, g))
    }

    /// D4315: from now on, loading a values or validity extent passes the extents before it that this cursor holds
    /// ([`Self::pass_behind`]'s rule): its reader reads forward and keeps no view into a granule's images past its next
    /// granule's read (a scan whose windows are consumed whole, a whole-column walk that copies). `Shared` only.
    /// D4351: such a cursor is a live reader of its part from granule 0 (`OpenPart::register_reader`) until it drops or
    /// stops passing: the trim's first tier keeps the passed extents it has yet to reach.
    pub fn set_pass_behind(&mut self, on: bool) {
        self.pass_on_load = on && self.residency == Residency::Shared;
        self.read_hi = 0;
        self.walked = false;
        if self.pass_on_load {
            if self.reader == 0 {
                self.reader = self.part.register_reader(self.stream_tag());
            } else if self.low != 0 {
                self.part.set_reader_low(self.reader, 0);
            }
            self.low = 0;
        } else if self.reader != 0 {
            self.part.deregister_reader(self.reader);
            self.reader = 0;
            self.low = 0;
        }
    }

    /// D4347: a whole-column walk (a verbatim part's ranks, `text_hosts::row_codes`) is about to read this cursor's column,
    /// and another walk or the scan reads it again in the same claim (the LIKE table by granule scans, the rows' render):
    /// the cursor stops passing on load, so no extent the first walk read is evicted before the next read, and its reader
    /// passes the extents by band instead (`pass_behind` at each band the claim enters: the user's rule, the whole column
    /// pinned while the claim walks it whole, then only what is ahead of the scan). Only a cursor that passes behind.
    pub fn hold_for_walks(&mut self) {
        if self.pass_on_load {
            self.pass_on_load = false;
            self.walked = true;
            // D4351: the walk reads the column from its start
            if self.reader != 0 && self.low != 0 {
                self.low = 0;
                self.part.set_reader_low(self.reader, 0);
            }
        }
    }

    /// D4347: the cursor's column was walked whole in this claim ([`Self::hold_for_walks`]): its reader passes by band.
    pub fn walked(&self) -> bool {
        self.walked
    }

    /// D4315: the end (exclusive granule) of the furthest values extent this cursor loaded since [`Self::set_pass_behind`]
    /// (0: none). A whole-column walk (a verbatim part's ranks, a LIKE table by granule scans) reads to the part's end at
    /// its first band: the scan's read-ahead starts past it, so it reads no extent the walk already read and passed.
    pub fn read_hi(&self) -> u32 {
        self.read_hi
    }

    /// D4315: the loaded extents below index `ei` of the values (`!validity`) or validity stream go, passed. D4351: the
    /// cursor reads granule `g` now: its live-reader position, published with the pass (one lock per extent load).
    fn pass_below(&mut self, validity: bool, ei: usize, g: u32) {
        let tag = self.stream_tag();
        let mut at = self.position(g);
        let (ps, slots) = if validity { (self.validity.as_ref(), &mut self.valstate) } else { (Some(&self.values), &mut self.vstate) };
        let Some(ps) = ps else { return };
        let mut keys = [PassKey::default(); PASS_CHUNK];
        let mut n = 0usize;
        // bounded("other: one stream's extents of one part below the one loaded, each parked once")
        for (slot, rec) in slots.iter_mut().zip(ps.extents.iter()).take(ei) {
            if slot.is_loaded() {
                slot.pass();
                self.loaded = true;
                keys[n] = PassKey { key: (rec.file_off, rec.len), g_end: rec.granule_start.saturating_add(rec.granule_count) };
                n += 1;
                if n == PASS_CHUNK {
                    self.part.pass_extents(tag, at.take(), &keys[..n]);
                    n = 0;
                }
            }
        }
        if n > 0 || at.is_some() {
            self.part.pass_extents(tag, at, &keys[..n]);
        }
    }

    /// D4315: the end (exclusive granule) of this cursor's read-ahead from granule `g`: its values extents from the one
    /// covering `g` on, until their file bytes reach `bytes` (one extent at least).
    pub fn read_ahead_end(&self, g: u32, bytes: u64) -> u32 {
        let exts = &self.values.extents;
        let i0 = exts.partition_point(|r| r.granule_start + r.granule_count <= g);
        let mut sum = 0u64;
        let mut end = g.saturating_add(1);
        // bounded("other: one stream's extents of one part, a sum each")
        for r in &exts[i0.min(exts.len())..] {
            sum += r.len;
            end = r.granule_start + r.granule_count;
            if sum >= bytes {
                break;
            }
        }
        end.max(g.saturating_add(1))
    }

    /// D3994 / D3204 (B.1 R): a kept cursor's use rejoins the statement or portal `to` that binds its Node again (the
    /// binding thread's `memledger::charge::context`): while that statement runs the part is its in-use part, ranked as its
    /// by the watchdog and split as its, not a parked one; the unbind gives it back (`keep_past_statement`; a worker set's
    /// put, to the depot). Only a parked use moves (the session's, the depot's): a running owner's never.
    pub fn rejoin(&mut self, to: memledger::Owner, release: memledger::Release) {
        let parked = matches!(self.part_use.owner(), memledger::Owner::Session(_) | memledger::Owner::Cache(memledger::Holder::Parked));
        if parked && matches!(to, memledger::Owner::Statement { .. } | memledger::Owner::Portal { .. }) {
            self.part_use.transfer(to, release);
            self.kept_to = None;
            self.rehome_kept();
        }
    }

    /// D4554: `rejoin` with the use moved in `group` (`transfer_use_in`'s).
    pub fn rejoin_in(&mut self, group: &mut crate::openpart::UseMoves<'_>, to: memledger::Owner, release: memledger::Release) {
        let parked = matches!(self.part_use.owner(), memledger::Owner::Session(_) | memledger::Owner::Cache(memledger::Holder::Parked));
        if parked && matches!(to, memledger::Owner::Statement { .. } | memledger::Owner::Portal { .. }) {
            self.part_use.transfer_in(group, to, release);
            self.kept_to = None;
            self.rehome_kept();
        }
    }

    pub fn path_ord(&self) -> u32 {
        self.path_ord
    }

    /// Logical rows in part-granule `g` (closed form in (rows, grain) —
    /// spec §2 + SB-10; the grain is the part footer's).
    /// M2-ZONEREAD card C: the column's WRAPPED single-extent dictionary index, and its payload when `payload`, read in
    /// one pread (they sit one after the other) and staged raw for `DictHandle`'s unwraps, before the handle faults the
    /// index: a dictionary-filtered scan read each part's index and then its payload, two preads. Already resident or
    /// staged regions are not read again; unwrapped regions are the values run's (`prefetch_granule_runs`). Returns the
    /// run face's census.
    pub fn stage_dict(&self, payload: bool, max_run: u64, max_hole: u64) -> [u64; 5] {
        let (Some(di), Some(dp)) = (self.dict_index.as_ref(), self.dict_payload.as_ref()) else { return [0; 5] };
        let raw = crate::openpart::ROLE_STAGE_RAW;
        let Some(&first) = di.extents.first() else { return [0; 5] };
        let mut recs = [(0u32, first, di.role as u8 | raw); 2];
        let mut n = 0;
        for (ds, on) in [(di, true), (dp, payload)] {
            if on && ds.entry.wrapper != Wrapper::None.as_u8() && ds.extents.len() == 1 {
                recs[n] = (0, ds.extents[0], ds.role as u8 | raw);
                n += 1;
            }
        }
        if n < 2 {
            // one region alone is the handle's own read, as before
            return [0; 5];
        }
        // bounded("small-fixed: the two dictionary regions")
        recs[..n].sort_unstable_by_key(|r| r.1.file_off);
        let r = self.part.prefetch_extent_run_holes_roles_for(self.read_owner(), &di.entry, &recs[..n], max_run, max_hole, None, None);
        [r.0, r.1, r.2, r.3, r.4]
    }

    pub fn rows_in_granule(&self, g: u32) -> u32 {
        geom::rows_in_granule_at(self.part.rows(), self.part.grain(), g)
    }

    pub fn granule_count(&self) -> u32 {
        self.part.footer().granule_count
    }

    /// Whether the column stores a validity stream in this part (spec §6.1:
    /// present iff ≥ 1 NULL). The O-6 zero-null proof input for dict lanes.
    pub fn has_validity_stream(&self) -> bool {
        self.validity.is_some()
    }

    /// The file extents `(off, len)` holding granules `[g_lo, g_hi)` of the values and validity streams, each once, in
    /// stream then file order (M3's walk residency, hosts `extent_resident`); nothing is read.
    pub fn granule_extents(&self, g_lo: u32, g_hi: u32, f: &mut dyn FnMut(u64, u64)) {
        if g_lo >= g_hi {
            return;
        }
        for ps in [Some(&self.values), self.validity.as_ref()].into_iter().flatten() {
            let i0 = ps.extents.partition_point(|r| r.granule_start + r.granule_count <= g_lo);
            let i1 = ps.extents.partition_point(|r| r.granule_start < g_hi).max(i0);
            ps.extents[i0..i1].iter().for_each(|r| f(r.file_off, r.len));
        }
    }

    /// The part file's page-cache residency over `[off, off+len)`: `(pages cached, pages)`, `None` unknown.
    pub fn page_resident(&self, off: u64, len: u64) -> Option<(u64, u64)> {
        self.part.page_resident(off, len)
    }

    /// Read the values and validity extents covering part-granules
    /// `[g_lo, g_hi)` into the part's residency plane ahead of the decode
    /// faces — `OpenPart::prefetch_extent_run_holes_roles` coalesces them into
    /// preads of at most `max_run` bytes, reading and discarding holes of at
    /// most `max_hole` bytes between extents; wrapped extents rebuild
    /// through this cursor's unwrapper exactly as `load_extent` does. Never
    /// fails: an extent skipped here is read by the decode face as before.
    /// Returns the run face's `[inserted, inserted_bytes, already_resident,
    /// io_bytes, io_runs]` summed over the two streams.
    ///
    /// An UNWRAPPED values stream's last chunk also carries the column's
    /// unwrapped dictionary regions (DictIndex / DictPayload, at most
    /// `DICT_SLOTS` extents): the writer lays them right after the values,
    /// so the one pread that brings the claim's codes brings its dictionary
    /// too (measured on a wide table: the dictionary regions were a separate demand
    /// fault per part, adjacent to the run, 139–392 bytes). A wrapped region
    /// stays `DictHandle`'s to rebuild (its unwrap needs the index; a WILLNEED
    /// hint beside the run was measured: no change in the cold cell).
    pub fn prefetch_granules(&self, g_lo: u32, g_hi: u32, max_run: u64, max_hole: u64) -> [u64; 5] {
        self.prefetch_granule_runs(&[(g_lo, g_hi)], max_run, max_hole)
    }

    /// PERF-COLD: advise (WILLNEED, [`OpenPart::advise_extents_in_pieces`]) the values and validity extents covering the
    /// granule runs `[lo, hi)` (ascending, disjoint) that are not resident, before a claim's lanes read them: the lanes'
    /// preads then overlap in the device's queue instead of going one at a time. `max_hole` / `piece` in bytes.
    /// At most `limit` bytes (the caller's in-flight budget). Returns `(calls, bytes)` advised; a hot claim (everything
    /// resident) makes no syscall.
    pub fn advise_granule_runs(&self, runs: &[(u32, u32)], max_hole: u64, piece: u64, limit: u64) -> (u64, u64) {
        let mut acc = (0u64, 0u64);
        for ps in [Some(&self.values), self.validity.as_ref()].into_iter().flatten() {
            let wrapped = ps.entry.wrapper != Wrapper::None.as_u8();
            let exts = &ps.extents;
            let recs = runs
                .iter()
                .scan(0usize, |next, &(g_lo, g_hi)| {
                    let i0 = exts.partition_point(|r| r.granule_start + r.granule_count <= g_lo).max(*next);
                    let i1 = exts.partition_point(|r| r.granule_start < g_hi).max(i0);
                    *next = i1;
                    Some(&exts[i0..i1])
                })
                .flatten();
            let (c, b) = self.part.advise_extents_in_pieces(recs, wrapped, max_hole, piece, limit.saturating_sub(acc.1));
            acc = (acc.0 + c, acc.1 + b);
        }
        acc
    }

    /// [`Self::prefetch_granules`] over several granule runs `[lo, hi)` (ascending, disjoint): the
    /// extents covering any run, each once and in file order, go through ONE run call, so extents
    /// of neighbouring runs coalesce and an extent between two runs that no run covers is a hole
    /// under `max_hole` (a cold point filter: a part's verdict-admitted runs in adjacent bands were a pread
    /// each, split at 1–7 B of padding). Up to `RUN_CHUNK` extents the records sit on the stack;
    /// past it in a Vec reserved fallibly, and when that reserve fails the runs go one by one in
    /// stack chunks as they did before this call existed. One run of at most `RUN_CHUNK` extents
    /// reads exactly as before.
    pub fn prefetch_granule_runs(&self, runs: &[(u32, u32)], max_run: u64, max_hole: u64) -> [u64; 5] {
        self.prefetch_granule_runs_tail(runs, max_run, max_hole, DictTail::NONE, &[])
    }

    /// M2-ZONEREAD card C: [`Self::prefetch_granule_runs`] whose values run also carries the column's WRAPPED
    /// single-extent dictionary regions `tail` names, read in the run's pread and staged raw on the part
    /// (`OpenPart::staged`) for `DictHandle`'s unwrap: a text-filter scan read a part's codes, its dictionary index and
    /// its payload in three preads, though the writer lays them one after the other. The caller names only what the
    /// statement reads (the index whenever it opens the lane's dictionary, the payload only for a whole-payload walk).
    /// Holes and the run cap apply as to any extent.
    ///
    /// M2-ZONEREAD card A: `sections` (footer-listed section indexes, at most `TAIL_SECTIONS`) ride the same values run
    /// into the segment cache (`OpenPart::section_run_record`): a zone lane's Stats / Bloom / PSMA cluster sits right
    /// after its streams, so the granule verdicts' meta comes in the codes' pread, not one of its own. A section past
    /// the hole or the run cap is a pread of its own, as it was.
    pub fn prefetch_granule_runs_tail(&self, runs: &[(u32, u32)], max_run: u64, max_hole: u64, tail: DictTail, sections: &[usize]) -> [u64; 5] {
        type Rec = (u32, pgrc_format::part::ExtentRecord, u8);
        const RUN_CHUNK: usize = 32;
        const DICT_SLOTS: usize = 4 + TAIL_SECTIONS;
        let mut acc = [0u64; 5];
        let owner = self.read_owner();
        // a prefetch: a cancel ends it with what it read (`prefetch_extent_run_holes_roles` stops on its own step too)
        let mut step = cancelpoll::step("pgrc_read prefetch_granule_runs_tail: a claim's runs in chunks");
        for ps in [Some(&self.values), self.validity.as_ref()].into_iter().flatten() {
            let Ok(wrapper) = Wrapper::from_u8(ps.entry.wrapper) else { continue };
            let uw = if wrapper == Wrapper::None {
                None
            } else {
                match self.binding.unwrapper_for(wrapper) {
                    Some(u) => Some(u),
                    None => continue,
                }
            };
            let span = |g_lo: u32, g_hi: u32| {
                let i0 = ps.extents.partition_point(|r| r.granule_start + r.granule_count <= g_lo);
                (i0, ps.extents.partition_point(|r| r.granule_start < g_hi).max(i0))
            };
            // the runs' extents in file order, each once (an extent a previous run covers is skipped)
            let extents_of = || {
                runs.iter()
                    .scan(0usize, |next, &(g_lo, g_hi)| {
                        let (i0, i1) = span(g_lo, g_hi);
                        let i0 = i0.max(*next);
                        *next = i1.max(i0);
                        Some(i0..i1.max(i0))
                    })
                    .flatten()
            };
            let total = extents_of().count();
            if total == 0 {
                continue;
            }
            let entry = &ps.entry;
            let mut rebuild = |raw: &[u8]| -> ReadResult<Vec<u8>> {
                let hdr = StreamSectionHdr::decode(raw)?;
                if hdr.encoding != entry.encoding || hdr.width != entry.width || hdr.wrapper != entry.wrapper {
                    return Err(ReadError::Format(FormatError::Corrupt { at: "section header vs stream entry" }));
                }
                // D4216: an extent's rebuild is its whole image's fill and inflate: per piece, the statement's cancel
                let mut step = cancelpoll::step("pgrc_read prefetch_granule_runs_tail: a wrapped extent's rebuild");
                let uw = uw.expect("wrapped stream has its unwrapper");
                let rebuilt = uw.unwrap_section_until(&hdr, raw, &mut || step.tick().is_err())?.ok_or(ReadError::Canceled)?;
                let twin = StreamSectionHdr::decode(&rebuilt)?;
                if twin.wrapper != Wrapper::None.as_u8() {
                    return Err(ReadError::Format(FormatError::Corrupt { at: "unwrapped section still wrapped" }));
                }
                Ok(rebuilt)
            };
            let role = ps.role as u8;
            // An UNWRAPPED values stream's last chunk of `n` records also carries the column's unwrapped
            // dictionary regions while the chunk's slots hold them (a wrapped region is the handle's to
            // rebuild: its unwrap needs the index).
            let dict_tail = |n: usize, push: &mut dyn FnMut(Rec)| {
                if ps.role != StreamRole::Values {
                    return;
                }
                let mut n = n;
                for (ds, staged) in [(self.dict_index.as_ref(), tail.index), (self.dict_payload.as_ref(), tail.payload)] {
                    let Some(ds) = ds else { continue };
                    if n + ds.extents.len() > RUN_CHUNK + DICT_SLOTS {
                        continue;
                    }
                    if ds.entry.wrapper == Wrapper::None.as_u8() {
                        // an unwrapped region rides an unwrapped run into the segment cache
                        if wrapper != Wrapper::None {
                            continue;
                        }
                        // bounded("small-fixed: at most RUN_CHUNK + DICT_SLOTS records, the check above")
                        for (ei, rec) in ds.extents.iter().enumerate() {
                            push((ei as u32, *rec, ds.role as u8));
                        }
                    } else {
                        // a wrapped region only when named, staged raw for its unwrap (one extent: its unwrap is whole)
                        if !staged || ds.extents.len() != 1 {
                            continue;
                        }
                        push((0, ds.extents[0], ds.role as u8 | crate::openpart::ROLE_STAGE_RAW));
                    }
                    n += ds.extents.len();
                }
                // the named sections, in file order, after the run's extents
                let mut secs = [(0u32, ps.extents[0], 0u8); TAIL_SECTIONS];
                let mut m = 0;
                for &idx in sections.iter().take(TAIL_SECTIONS) {
                    if let Some(r) = self.part.section_run_record(idx) {
                        secs[m] = r;
                        m += 1;
                    }
                }
                // bounded("small-fixed: at most TAIL_SECTIONS sections")
                secs[..m].sort_unstable_by_key(|r| r.1.file_off);
                // bounded("small-fixed: at most TAIL_SECTIONS sections")
                for r in &secs[..m] {
                    if n < RUN_CHUNK + DICT_SLOTS {
                        push(*r);
                        n += 1;
                    }
                }
            };
            // past one stack chunk: every record in one call, the dictionary tail under the slot rule of the last chunk
            let mut all: memledger::ChargedVec<Rec> = if total > RUN_CHUNK { read_vec(owner) } else { memledger::ChargedVec::none() };
            if total > RUN_CHUNK && all.with_vec(|v| v.try_reserve_exact(total + RUN_CHUNK + DICT_SLOTS)).is_ok() { // alloc: Exec charged owner
                all.with_vec(|v| v.extend(extents_of().map(|i| (i as u32, ps.extents[i], role)))); // alloc: Exec charged owner
                dict_tail((total - 1) % RUN_CHUNK + 1, &mut |r| all.with_vec(|v| v.push(r))); // alloc: Exec charged owner
                let rb: Option<&mut dyn FnMut(&[u8]) -> ReadResult<Vec<u8>>> = if uw.is_some() { Some(&mut rebuild) } else { None };
                let r = self.part.prefetch_extent_run_holes_roles_for(owner, entry, &all, max_run, max_hole, rb, None);
                // bounded("const-domain: the run census's five counts")
                for (a, v) in acc.iter_mut().zip([r.0, r.1, r.2, r.3, r.4]) {
                    *a += v;
                }
                continue;
            }
            // The extent list in stack chunks: a resident pass allocates nothing.
            let mut recs: [Rec; RUN_CHUNK + DICT_SLOTS] = [(0u32, ps.extents[0], role); RUN_CHUNK + DICT_SLOTS];
            let mut chunked = |step: &mut cancelpoll::Step, it: &mut dyn Iterator<Item = usize>, acc: &mut [u64; 5]| {
                let mut it = it.peekable();
                while it.peek().is_some() {
                    if step.tick().is_err() {
                        return;
                    }
                    let mut n = 0;
                    // bounded("small-fixed: at most RUN_CHUNK records a chunk")
                    while n < RUN_CHUNK {
                        let Some(i) = it.next() else { break };
                        recs[n] = (i as u32, ps.extents[i], role);
                        n += 1;
                    }
                    if it.peek().is_none() {
                        let mut m = n;
                        dict_tail(n, &mut |r| {
                            recs[m] = r;
                            m += 1;
                        });
                        n = m;
                    }
                    let rb: Option<&mut dyn FnMut(&[u8]) -> ReadResult<Vec<u8>>> = if uw.is_some() { Some(&mut rebuild) } else { None };
                    let r = self.part.prefetch_extent_run_holes_roles_for(owner, entry, &recs[..n], max_run, max_hole, rb, None);
                    // bounded("const-domain: the run census's five counts")
                    for (a, v) in acc.iter_mut().zip([r.0, r.1, r.2, r.3, r.4]) {
                        *a += v;
                    }
                }
            };
            if total <= RUN_CHUNK {
                chunked(&mut step, &mut extents_of(), &mut acc);
            } else {
                // the reserve failed: run by run, each as its own call
                for &(g_lo, g_hi) in runs {
                    if step.tick().is_err() {
                        break;
                    }
                    let (i0, i1) = span(g_lo, g_hi);
                    chunked(&mut step, &mut (i0..i1), &mut acc);
                }
            }
        }
        acc
    }

    // -- the six faces ----------------------------------------------------

    /// `decode_full` (spec §19.3): all rows of granule `g` into `out`.
    pub fn decode_full(&mut self, g: u32, out: &mut DecodeOut<'_>) -> ReadResult<u32> {
        let vt = self.vt;
        let (ei, gie, values_g, _rows_g) = self.ensure_values(g)?;
        let vslice = self.ensure_validity_slice(g, values_g)?;
        self.ensure_overflow()?;
        self.ensure_dict()?;
        let ctx = self.build_ctx(g, ei, gie, values_g, vslice, true, true)?;
        (vt.decode_full)(&ctx, out).map_err(Into::into)
    }

    /// `decode_lengths` (D3038): granule `g`'s varlena payload octet lengths,
    /// one per value slot, exactly the slots `decode_full` writes (a NULL row's
    /// slot is an empty entry; validity says which rows are live), with no
    /// payload copied. Only a
    /// VERBATIM varlena stream answers; any other encoding or class refuses
    /// (`ReadError::Unsupported`) and the caller decodes in full.
    pub fn decode_lengths(&mut self, g: u32, out: &mut [u32]) -> ReadResult<u32> {
        if self.key.encoding != EncodingId::Verbatim.as_u16()
            || self.key.class != pgrc_format::class::CLASS_VARLENA
        {
            return Err(ReadError::Unsupported {
                what: "length-only decode of a lane that is not verbatim varlena",
            });
        }
        let (ei, gie, values_g, _rows_g) = self.ensure_values(g)?;
        let vslice = self.ensure_validity_slice(g, values_g)?;
        self.ensure_overflow()?;
        let ctx = self.build_ctx(g, ei, gie, values_g, vslice, true, false)?;
        pgrc_format::verbatim::varlena_lengths(&ctx, out).map_err(Into::into)
    }

    /// `decode_i32` / `decode_i16`: `decode_full` for byval int lanes of
    /// width 4 / 2 (Gravity's narrow lanes): the kernel's `decode_narrow`
    /// face unpacks the granule straight into `out` at that width — no
    /// datum words, no arena (byval kernels never touch it). Returns the
    /// rows written (`rows_in_granule(g)`). A lane that is not a byval word
    /// class refuses (`ReadError::Unsupported`); the caller asks the
    /// column's logical width (the stream entry's `width` is the elected
    /// encoding's, 0 for the FOR family).
    pub fn decode_i32(&mut self, g: u32, out: &mut [i32]) -> ReadResult<u32> {
        let n = out.len();
        self.decode_narrow(g, n, NarrowOut::I32(out))
    }

    pub fn decode_i16(&mut self, g: u32, out: &mut [i16]) -> ReadResult<u32> {
        let n = out.len();
        self.decode_narrow(g, n, NarrowOut::I16(out))
    }

    fn decode_narrow(&mut self, g: u32, cap: usize, out: NarrowOut<'_>) -> ReadResult<u32> {
        if self.values.entry.class != pgrc_format::class::CLASS_BYVAL {
            return Err(ReadError::Unsupported {
                what: "typed decode of a lane that is not a byval word",
            });
        }
        if cap < self.rows_in_granule(g) as usize {
            return Err(ReadError::Format(FormatError::Bounds {
                at: "typed decode output",
            }));
        }
        let vt = self.vt;
        let (ei, gie, values_g, _rows_g) = self.ensure_values(g)?;
        let vslice = self.ensure_validity_slice(g, values_g)?;
        let ctx = self.build_ctx(g, ei, gie, values_g, vslice, false, false)?;
        (vt.decode_narrow)(&ctx, out).map_err(Into::into)
    }

    /// The stream's normalized dispatch key (encoding, class, width).
    pub fn key(&self) -> KernelKey {
        self.key
    }

    /// Granule `g`'s values ctx (validity attached, no overflow or dictionary) handed to
    /// `f`: a consumer's own kernel over the stream's frames (gravity's fixed lane runs
    /// `pgrc_codec::packednum::mantissa_full` here — this crate stays codec-agnostic).
    pub fn granule_ctx<R>(&mut self, g: u32, f: impl FnOnce(&KernelCtx<'_>) -> FormatResult<R>) -> ReadResult<R> {
        let (ei, gie, values_g, _rows_g) = self.ensure_values(g)?;
        let vslice = self.ensure_validity_slice(g, values_g)?;
        let ctx = self.build_ctx(g, ei, gie, values_g, vslice, false, false)?;
        f(&ctx).map_err(Into::into)
    }

    /// Granule `g`'s framed values straight from the extent state this cursor keeps (TH14's source lever, main 09-24): the
    /// payload region and the frame table parsed once when the extent loaded (its section header checked against the
    /// stream entry then), the granule's first frame and its value count. No ctx, no second header parse, no validity,
    /// overflow or dictionary fault: a codec's frame kernel (pgrc_codec bytefor `Framed`) reads it with the bounds it
    /// proves per frame. `None` unless the values stream is `encoding` (BYTE_FOR or PACKED_NUMERIC), a root stream (no
    /// gcount table: a granule's frames are `FRAMES_PER_GRANULE` apart) with a frame table; the caller then takes the ctx
    /// face. The view borrows the cursor: it lives for one call, never past the extent state it reads.
    pub fn framed(&mut self, g: u32, encoding: u16) -> ReadResult<Option<FramedGranule<'_>>> {
        if self.values.entry.encoding != encoding {
            return Ok(None);
        }
        let (ei, gie, values_g, _rows_g) = self.ensure_values(g)?;
        let st = self.vstate[ei].loaded().expect("values extent loaded");
        let (None, Some(ft)) = (&st.gcounts, st.frame_table.as_deref()) else {
            return Ok(None);
        };
        // `section::frame_base`'s closed form and its range check against the frame table's exact entry count
        let fbase = gie as u64 * geom::FRAMES_PER_GRANULE as u64;
        if fbase > ft.len() as u64 {
            return Err(ReadError::Format(FormatError::Corrupt {
                at: "frame_base out of range",
            }));
        }
        Ok(Some(FramedGranule {
            payload: st.payload(),
            frame_table: ft,
            fbase: fbase as u32,
            values: values_g,
        }))
    }

    /// Granule `g`'s values of a root VERBATIM stream in place: the granule's contiguous `values × width` body inside the
    /// loaded extent image — the bytes `decode_full` / `decode_narrow` copy unchanged when the lane is the stored width.
    /// `None` unless the values stream is VERBATIM, a root stream (one value per row), of nonzero width, and at least
    /// `slack` bytes of the extent image follow the body (a consumer's vector tail may read a little past the last row;
    /// those reads stay inside the image). The slice borrows the cursor's extent state: the image is an immutable
    /// `SegBuf`, and it stays loaded until this cursor loads another values extent (PGRC-DECODE lever 2, main 09-24).
    pub fn verbatim_body(&mut self, g: u32, slack: usize) -> ReadResult<Option<&[u8]>> {
        if self.values.entry.encoding != EncodingId::Verbatim as u16 {
            return Ok(None);
        }
        let (ei, gie, values_g, rows_g) = self.ensure_values(g)?;
        let w = self.key.width as usize;
        let st = self.vstate[ei].loaded().expect("values extent loaded");
        if st.gcounts.is_some() || values_g != rows_g || w == 0 {
            return Ok(None);
        }
        let start = st.payload_start as usize + gie as usize * geom::GRANULE_ROWS as usize * w;
        let end = start + values_g as usize * w;
        if end > st.payload_end as usize {
            return Err(ReadError::Format(FormatError::Bounds {
                at: "verbatim body in place",
            }));
        }
        let image = st.bytes.bytes();
        Ok((end + slack <= image.len()).then(|| &image[start..end]))
    }

    /// `decode_sel` (spec §19.3): survivor-only completion, dense outputs in
    /// selection order. Must equal `decode_full ∘ select` — this layer
    /// builds bit-identical ctxs for both faces, so the kernel property
    /// survives composition (pinned in `src/tests/decode_props.rs`).
    pub fn decode_sel(
        &mut self,
        g: u32,
        sel: &Selection<'_>,
        out: &mut DecodeOut<'_>,
    ) -> ReadResult<u32> {
        let vt = self.vt;
        let (ei, gie, values_g, _rows_g) = self.ensure_values(g)?;
        sel.validate(values_g)?;
        let vslice = self.ensure_validity_slice(g, values_g)?;
        self.ensure_overflow()?;
        self.ensure_dict()?;
        let ctx = self.build_ctx(g, ei, gie, values_g, vslice, true, true)?;
        (vt.decode_sel)(&ctx, sel, out).map_err(Into::into)
    }

    /// `decode_codes` (spec §19.3): global codes without dict bytes.
    pub fn decode_codes(&mut self, g: u32, out: &mut [u32]) -> ReadResult<u32> {
        let vt = self.vt;
        let (ei, gie, values_g, _rows_g) = self.ensure_values(g)?;
        let vslice = self.ensure_validity_slice(g, values_g)?;
        let ctx = self.build_ctx(g, ei, gie, values_g, vslice, false, false)?;
        (vt.decode_codes)(&ctx, out).map_err(Into::into)
    }

    /// `validity` (spec §19.3): mask words for granule `g`. Faults ONLY the
    /// validity extent (ctx carries empty section bytes — module doc); the
    /// per-entry count comes from the validity stream ITSELF (its own
    /// gcount table for child streams, the §2 closed form for root
    /// streams) — never from the values extent.
    pub fn validity(&mut self, g: u32, out: &mut [u64]) -> ReadResult<ValidityVerdict> {
        let vt = self.vt;
        if g >= self.granule_count() {
            return Err(ReadError::Format(FormatError::Bounds {
                at: "granule ordinal",
            }));
        }
        let (values_g, vslice) = match self.load_validity_extent(g)? {
            // No validity stream: all-valid (spec §6.1). The kernel
            // short-circuits on `validity_bytes = None` before any count
            // geometry, so the closed form serves both root and child
            // streams here.
            None => (self.rows_in_granule(g), None),
            Some((ei, gie)) => {
                let st = self.valstate[ei].loaded().expect("just loaded");
                let values_g = match &st.gcounts {
                    Some(gc) => *gc.get(gie as usize).ok_or(ReadError::Format(
                        FormatError::Bounds { at: "gcount index" },
                    ))?,
                    None => self.rows_in_granule(g),
                };
                (values_g, self.validity_slice_at(ei, gie, values_g)?)
            }
        };
        let ctx = self.build_empty_ctx(g, values_g, vslice);
        (vt.validity)(&ctx, out).map_err(Into::into)
    }

    /// `meta_probe` (spec §19.3): encoding-local stat answers.
    pub fn meta_probe(&mut self, g: u32, probe: &MetaProbe) -> ReadResult<ProbeAnswer> {
        let vt = self.vt;
        let (ei, gie, values_g, _rows_g) = self.ensure_values(g)?;
        let vslice = self.ensure_validity_slice(g, values_g)?;
        let ctx = self.build_ctx(g, ei, gie, values_g, vslice, false, false)?;
        let f: MetaProbeFn = vt.meta_probe;
        f(&ctx, probe).map_err(Into::into)
    }

    /// `dict_handle` (spec §19.3): the ABI face — dict geometry facts from
    /// the kernel. (The reader-level lazy handle is
    /// [`crate::dicthandle::DictHandle`].)
    pub fn dict_handle_face(&mut self, g: u32) -> ReadResult<DictLayout> {
        let vt = self.vt;
        let (ei, gie, values_g, _rows_g) = self.ensure_values(g)?;
        let vslice = self.ensure_validity_slice(g, values_g)?;
        self.ensure_dict()?;
        let ctx = self.build_ctx(g, ei, gie, values_g, vslice, false, true)?;
        (vt.dict_handle)(&ctx).map_err(Into::into)
    }

    /// Values in part-granule `g` of THIS stream (child streams: from the
    /// gcount table; root: closed form).
    pub fn values_in_granule(&mut self, g: u32) -> ReadResult<u32> {
        Ok(self.ensure_values(g)?.2)
    }

    /// The resident dict PAYLOAD region as (base address, byte length);
    /// `None` for non-dict streams. This is the zero-copy aliasing target of
    /// dict-varlena `decode_full`/`decode_sel` datums (the `DecodeOut`
    /// pointer-class output law): the region is a part-cached `SegBuf`
    /// (8-aligned, insert-only — resident for the `OpenPart`'s life), so
    /// datums into it outlive every granule-claim consumption window.
    /// Containment gates (pgrc_qa) use these bounds.
    pub fn dict_payload_bounds(&mut self) -> ReadResult<Option<(usize, usize)>> {
        if self.dict_payload.is_none() {
            return Ok(None);
        }
        self.ensure_dict()?;
        Ok(self.dict_state.as_ref().map(|(_, p, _, _)| {
            let pl = p.payload();
            (pl.as_ptr() as usize, pl.len())
        }))
    }

    /// The dict PAYLOAD region this cursor loaded, `None` before a dict face ran or for a non-dict stream: what
    /// dict-varlena `decode_full` / `decode_sel` datums alias. A single-extent payload is the part-cached image; a
    /// multi-extent one (SB-7) is assembled for this cursor alone, so a caller whose datums outlive the cursor holds
    /// this clone for as long as it reads them (D777).
    pub fn dict_payload_region(&self) -> Option<SegBuf> {
        self.dict_state.as_ref().map(|(_, p, _, _)| p.bytes.clone())
    }

    /// Hold, in `g`, every region this cursor's decode datums can alias (today the dictionary payload region:
    /// dict-varlena datums point into it, the pointer-class law), so they stay valid after the cursor drops or
    /// moves on, until `g.release()` (C1 L5). One `Arc` clone per region: call it per granule or morsel, never per row.
    pub fn hold_datums(&self, g: &mut crate::mem::Guard) {
        if let Some((_, payload, _, _)) = &self.dict_state {
            g.hold(&payload.bytes);
        }
    }

    // -- extent state -----------------------------------------------------

    /// Ensure the values extent covering `g` is loaded; returns
    /// (extent idx, granule-in-extent, values_g, rows_g).
    fn ensure_values(&mut self, g: u32) -> ReadResult<(usize, u32, u32, u32)> {
        if g >= self.granule_count() {
            return Err(ReadError::Format(FormatError::Bounds {
                at: "granule ordinal",
            }));
        }
        let (ei, rec) = self.values.extent_for_granule(g)?;
        let ei = ei as usize;
        let gie = g - self.values.extents[ei].granule_start;
        if !self.vstate[ei].is_loaded() {
            // D3994 (B.1 W): a parked extent takes its image back with the tables it parsed before
            let parked = core::mem::replace(&mut self.vstate[ei], ExtSlot::Empty);
            let mut gone = parked.table_bytes();
            // the line's C1 L5: a Private or a Cached cursor keeps one extent of a stream (trunk E2: their tables off the kept charge)
            if self.residency != Residency::Shared {
                self.vstate.iter_mut().for_each(|s| {
                    gone += s.table_bytes();
                    *s = ExtSlot::Empty;
                });
            }
            Self::kept_sub(&mut self.kept, &self.pool, &mut self.kept_bytes, gone);
            let st = match parked {
                ExtSlot::Parked(p) => p.unpark(extent_image(&self.part, self.binding, &self.values, ei as u32, rec, self.residency, self.read_owner())?),
                _ => load_extent(&self.part, self.binding, &self.values, ei as u32, rec, false, self.residency, self.read_owner())?,
            };
            self.part.unpass((rec.file_off, rec.len));
            self.read_hi = self.read_hi.max(rec.granule_start.saturating_add(rec.granule_count));
            Self::kept_add(&mut self.kept, &self.pool, &mut self.kept_bytes, st.table_bytes());
            self.vstate[ei] = ExtSlot::Loaded(st);
            self.loaded = true;
            if self.pass_on_load {
                self.pass_below(false, ei, g);
            }
        }
        let rows_g = self.rows_in_granule(g);
        let st = self.vstate[ei].loaded().expect("just loaded");
        let values_g = match &st.gcounts {
            Some(gc) => *gc.get(gie as usize).ok_or(ReadError::Format(FormatError::Bounds {
                at: "gcount index",
            }))?,
            None => rows_g,
        };
        Ok((ei, gie, values_g, rows_g))
    }

    /// Load the validity extent covering `g` (iff the stream exists);
    /// returns (extent idx, granule-in-extent).
    fn load_validity_extent(&mut self, g: u32) -> ReadResult<Option<(usize, u32)>> {
        let Some(vs) = &self.validity else {
            return Ok(None);
        };
        let (ei, rec) = vs.extent_for_granule(g)?;
        let (ei, rec) = (ei as usize, *rec);
        let gie = g - vs.extents[ei].granule_start;
        if !self.valstate[ei].is_loaded() {
            // D3994 (B.1 W): as `ensure_values`. The stream is borrowed, not cloned: a rebound parked cursor takes its
            // validity extents back with no allocation (the steady-state rebind law)
            let parked = core::mem::replace(&mut self.valstate[ei], ExtSlot::Empty);
            let mut gone = parked.table_bytes();
            // the line's C1 L5: a Private or a Cached cursor keeps one extent of a stream (trunk E2: their tables off the kept charge)
            if self.residency != Residency::Shared {
                self.valstate.iter_mut().for_each(|s| {
                    gone += s.table_bytes();
                    *s = ExtSlot::Empty;
                });
            }
            Self::kept_sub(&mut self.kept, &self.pool, &mut self.kept_bytes, gone);
            let st = match parked {
                ExtSlot::Parked(p) => p.unpark(extent_image(&self.part, self.binding, vs, ei as u32, &rec, self.residency, self.read_owner())?),
                _ => load_extent(&self.part, self.binding, vs, ei as u32, &rec, true, self.residency, self.read_owner())?,
            };
            self.part.unpass((rec.file_off, rec.len));
            Self::kept_add(&mut self.kept, &self.pool, &mut self.kept_bytes, st.table_bytes());
            self.valstate[ei] = ExtSlot::Loaded(st);
            self.loaded = true;
            if self.pass_on_load {
                self.pass_below(true, ei, g);
            }
        }
        Ok(Some((ei, gie)))
    }

    /// The granule's bitmap slice coordinates inside a loaded validity
    /// extent, cross-checked against the caller's value count.
    fn validity_slice_at(
        &self,
        ei: usize,
        gie: u32,
        values_g: u32,
    ) -> ReadResult<Option<(usize, u32, u32)>> {
        let st = self.valstate[ei].loaded().expect("validity extent loaded");
        let prefix = st.vbyte_prefix.as_ref().expect("validity prefix built");
        let (b0, b1) = (
            *prefix
                .get(gie as usize)
                .ok_or(ReadError::Format(FormatError::Bounds { at: "validity prefix" }))?,
            *prefix
                .get(gie as usize + 1)
                .ok_or(ReadError::Format(FormatError::Bounds { at: "validity prefix" }))?,
        );
        // Two-witness discipline at the slice level: the bitmap must be
        // sized for exactly this granule's values (spec §6.6). Real on the
        // decode faces (values_g comes from the values extent); structural
        // on the validity face (same-source counts).
        if (b1 - b0) as usize != (values_g as usize).div_ceil(8) {
            return Err(ReadError::Format(FormatError::Corrupt {
                at: "validity bitmap size vs values",
            }));
        }
        Ok(Some((ei, b0, b1)))
    }

    /// Ensure the validity extent covering `g` (iff the stream exists) and
    /// return the granule's bitmap slice coordinates.
    fn ensure_validity_slice(
        &mut self,
        g: u32,
        values_g: u32,
    ) -> ReadResult<Option<(usize, u32, u32)>> {
        match self.load_validity_extent(g)? {
            None => Ok(None),
            Some((ei, gie)) => self.validity_slice_at(ei, gie, values_g),
        }
    }

    fn ensure_overflow(&mut self) -> ReadResult<()> {
        let Some(os) = &self.overflow else {
            return Ok(());
        };
        if self.ovf_state.is_some() {
            return Ok(());
        }
        self.loaded = true;
        self.ovf_state = Some(load_region_res(
            &self.part,
            self.binding.unwrappers,
            os,
            "overflow stream",
            None,
            self.residency,
            self.read_owner(),
        )?);
        Ok(())
    }

    fn ensure_dict(&mut self) -> ReadResult<()> {
        let (Some(di), Some(dp)) = (&self.dict_index, &self.dict_payload) else {
            return Ok(());
        };
        if self.dict_state.is_some() {
            return Ok(());
        }
        let idx = load_region_res(&self.part, self.binding.unwrappers, di, "dict index stream", None, self.residency, self.read_owner())?;
        let pay = load_region_res(
            &self.part,
            self.binding.unwrappers,
            dp,
            "dict payload stream",
            Some(idx.payload()),
            self.residency,
            self.read_owner(),
        )?;
        // [cold2] census: a cursor-side whole-payload assembly (decode-path
        // dict_state / is_dict-class probes) — the attribution witness. D3994
        // (B.1 W): a parked cursor taking its dictionary back is not a new one.
        if !core::mem::take(&mut self.parked_dict) {
            crate::dicthandle::DICT_WHOLE_FAULT_BYTES
                .fetch_add(pay.payload().len() as u64, core::sync::atomic::Ordering::Relaxed);
        }
        let entry_count =
            u32::try_from(di.entry.values).map_err(|_| ReadError::Format(FormatError::Corrupt {
                at: "dict entry count",
            }))?;
        if idx.payload().len() != entry_count as usize * pgrc_format::dict::DICT_INDEX_ENTRY_LEN {
            return Err(ReadError::Format(FormatError::Corrupt {
                at: "dict index size vs entry count",
            }));
        }
        // M5d char-len record: the DictIndex dir entry names the
        // `char_field` form; the ctx carries it so `dict_entry` stays pure.
        let charlen_form =
            pgrc_format::dict::DictCharLenForm::from_flags(di.entry.flags)
                .map_err(ReadError::Format)?;
        self.dict_state = Some((idx, pay, entry_count, charlen_form));
        self.loaded = true;
        Ok(())
    }

    // -- ctx assembly -----------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn build_ctx(
        &self,
        g: u32,
        ei: usize,
        gie: u32,
        values_g: u32,
        vslice: Option<(usize, u32, u32)>,
        with_overflow: bool,
        with_dict: bool,
    ) -> ReadResult<KernelCtx<'_>> {
        let st = self.vstate[ei].loaded().expect("values extent loaded");
        let validity_bytes = vslice.map(|(vei, b0, b1)| {
            let vst = self.valstate[vei].loaded().expect("validity extent loaded");
            &vst.payload()[b0 as usize..b1 as usize]
        });
        let overflow = if with_overflow {
            self.ovf_state.as_ref().map(|r| r.payload())
        } else {
            None
        };
        let dict = if with_dict {
            let fc = self.dict_payload.as_ref().is_some_and(|dp| dp.entry.wrapper == Wrapper::FrontZstd.as_u8()) && crate::dict_resident_frontcoded();
            self.dict_state.as_ref().map(|(i, p, n, form)| DictSections {
                index: i.payload(),
                payload: p.payload(),
                entry_count: *n,
                charlen_form: *form,
                form: if fc { pgrc_format::dict::DictPayloadForm::FrontCoded } else { pgrc_format::dict::DictPayloadForm::Expanded },
            })
        } else {
            None
        };
        Ok(KernelCtx {
            key: self.key,
            flags: self.values.entry.flags,
            fixed_len: self.values.entry.fixed_len,
            bytes: st.bytes.bytes(),
            frame_table: st.frame_table.as_deref(),
            granule: g,
            granule_in_extent: gie,
            // The per-entry dimension in BOTH fields (module doc: the
            // reference kernels size validity geometry by `rows`; for root
            // streams this IS the §2 closed form).
            rows: values_g,
            values: values_g,
            validity_bytes,
            overflow,
            dict,
        })
    }

    /// Ctx for the validity face: empty section bytes (module doc).
    fn build_empty_ctx(
        &self,
        g: u32,
        values_g: u32,
        vslice: Option<(usize, u32, u32)>,
    ) -> KernelCtx<'_> {
        let validity_bytes = vslice.map(|(vei, b0, b1)| {
            let vst = self.valstate[vei].loaded().expect("validity extent loaded");
            &vst.payload()[b0 as usize..b1 as usize]
        });
        KernelCtx {
            key: self.key,
            flags: self.values.entry.flags,
            fixed_len: self.values.entry.fixed_len,
            bytes: &[],
            frame_table: None,
            granule: g,
            granule_in_extent: 0,
            rows: values_g,
            values: values_g,
            validity_bytes,
            overflow: None,
            dict: None,
        }
    }
}

/// Hard ceiling on one extent's granule count, independent of any
/// file-supplied footer (a corrupt footer could itself over-report). Kept
/// well above any realistic part geometry; its role is only to stop a crafted
/// 4-byte count from committing gigabytes before the payload-size witnesses
/// (gcount-table bounds, validity `payload_len`) run.
const MAX_EXTENT_GRANULES: u32 = 1 << 28;

/// Validate a file-supplied per-extent granule count against an independent
/// budget BEFORE it drives any allocation or count-driven loop (idx-198).
///
/// The class-level budget: an extent covers a SUBSET of the part's granules,
/// so its `granule_count` can never exceed the part footer's `granule_count`
/// (itself cross-checked against `rows`/`grain` at part open — the independent
/// witness), and never the hard [`MAX_EXTENT_GRANULES`] cap. A 4-byte lie
/// thus refuses catchably (`ERRCODE_DATA_CORRUPTED` via `FormatError::Corrupt`)
/// here, instead of sizing a multi-gigabyte `Vec` or spinning a
/// multi-billion-iteration loop. Returns the validated count as `usize` for
/// direct use as an allocation/loop bound.
fn checked_extent_granules(
    part: &OpenPart,
    rec: &pgrc_format::part::ExtentRecord,
) -> ReadResult<usize> {
    if rec.granule_count > part.footer().granule_count
        || rec.granule_count > MAX_EXTENT_GRANULES
    {
        return Err(ReadError::Format(FormatError::Corrupt {
            at: "extent granule_count exceeds part budget",
        }));
    }
    Ok(rec.granule_count as usize)
}

/// D3994 (B.1 W): an extent's image (the part cache's, or this cursor's own under `Residency::Private`), its section header
/// checked against the stream entry: `load_extent`'s fault, and a parked cursor's taking its image back (`ParkedExtent`).
fn extent_image(
    part: &Arc<OpenPart>,
    binding: &CodecBinding<'_>,
    ps: &ParsedStream,
    extent_idx: u32,
    rec: &pgrc_format::part::ExtentRecord,
    residency: Residency,
    owner: ReadOwner,
) -> ReadResult<SegBuf> {
    let entry_wrapper = Wrapper::from_u8(ps.entry.wrapper)?;
    let bytes = if entry_wrapper == Wrapper::None {
        let bytes = match residency {
            Residency::Shared => part.extent_bytes(&ps.entry, rec, extent_idx)?,
            Residency::Private => part.extent_bytes_uncached_for(owner, &ps.entry, rec, extent_idx)?,
            Residency::Cached => part.extent_bytes_cached_for(owner, &ps.entry, rec, extent_idx)?,
        };
        let hdr = StreamSectionHdr::decode(bytes.bytes())?;
        if hdr.encoding != ps.entry.encoding
            || hdr.width != ps.entry.width
            || hdr.wrapper != ps.entry.wrapper
        {
            return Err(ReadError::Format(FormatError::Corrupt {
                at: "section header vs stream entry",
            }));
        }
        bytes
    } else {
        // Typed refusal BEFORE any payload fault (the cursor-open pin,
        // repeated here so the predicate is correct standalone).
        let uw = binding.unwrapper_for(entry_wrapper).ok_or(ReadError::Format(
            FormatError::WrapperUnsupported {
                wrapper: entry_wrapper.as_u8(),
            },
        ))?;
        let mut rebuild = |raw: &[u8]| -> ReadResult<Vec<u8>> {
            let hdr = StreamSectionHdr::decode(raw)?;
            if hdr.encoding != ps.entry.encoding
                || hdr.width != ps.entry.width
                || hdr.wrapper != ps.entry.wrapper
            {
                return Err(ReadError::Format(FormatError::Corrupt {
                    at: "section header vs stream entry",
                }));
            }
            // D4228: an extent's rebuild is its whole image's fill and inflate: per piece, the statement's cancel
            let mut step = cancelpoll::step("pgrc_read load_extent: a wrapped extent's rebuild");
            let rebuilt = uw.unwrap_section_until(&hdr, raw, &mut || step.tick().is_err())?.ok_or(ReadError::Canceled)?;
            let twin = StreamSectionHdr::decode(&rebuilt)?;
            if twin.wrapper != Wrapper::None.as_u8() {
                return Err(ReadError::Format(FormatError::Corrupt {
                    at: "unwrapped section still wrapped",
                }));
            }
            Ok(rebuilt)
        };
        match residency {
            Residency::Shared => part.unwrapped_extent_bytes_for(owner, &ps.entry, rec, extent_idx, &mut rebuild)?,
            Residency::Private => part.unwrapped_extent_bytes_uncached_for(owner, &ps.entry, rec, extent_idx, &mut rebuild)?,
            Residency::Cached => part.unwrapped_extent_bytes_cached_for(owner, &ps.entry, rec, extent_idx, &mut rebuild)?,
        }
    };
    Ok(bytes)
}

/// Load + validate one granule-organized extent: fault (CRC under the part's
/// segment cache), header cross-witness against the stream entry, unwrap if
/// wrapped, parse frame table, parse the gcount table (child streams), build
/// the validity byte prefix (validity streams).
///
/// Wrapped extents (O-CMP-5(a), ruled 2026-08-10) resolve their unwrapper
/// from the stream ENTRY before any payload fault (the refusal-before-fault
/// pin, mirrored from cursor open) and go through the part's UNWRAPPED-image
/// cache: decompress once per part residency, every later cursor over this
/// extent hits the rebuilt encoded image.
#[allow(clippy::too_many_arguments)]
fn load_extent(
    part: &Arc<OpenPart>,
    binding: &CodecBinding<'_>,
    ps: &ParsedStream,
    extent_idx: u32,
    rec: &pgrc_format::part::ExtentRecord,
    is_validity: bool,
    residency: Residency,
    owner: ReadOwner,
) -> ReadResult<ExtentState> {
    let mut step = cancelpoll::step("pgrc_read load_extent: an extent's granule tables");
    let bytes = extent_image(part, binding, ps, extent_idx, rec, residency, owner)?;
    let hdr = StreamSectionHdr::decode(bytes.bytes())?;
    let frame_table = hdr.frame_table(bytes.bytes())?;
    let (payload_start, payload_end) = payload_bounds(&hdr, bytes.len())?;
    let gcounts = if hdr.gcount_table_off != 0 {
        // Class-level budget check BEFORE the table-bytes math or the
        // allocation/loop (idx-198): a file-supplied count can never exceed
        // the part footer / hard cap.
        let ng = checked_extent_granules(part, rec)?;
        let off = hdr.gcount_table_off as usize;
        let need = ng * 4;
        let end = off.checked_add(need).ok_or(ReadError::Format(FormatError::Bounds {
            at: "gcount table",
        }))?;
        if off < STREAM_SECTION_HDR_LEN || end > bytes.len() {
            return Err(ReadError::Format(FormatError::Bounds {
                at: "gcount table",
            }));
        }
        let mut v: Vec<u32> = Vec::new();
        v.try_reserve(ng).map_err(|_| { // alloc: Node charged kept_home
            ReadError::Format(FormatError::Corrupt {
                at: "gcount table allocation",
            })
        })?;
        let mut sum: u64 = 0;
        for i in 0..ng {
            step.batch(i)?;
            let b = &bytes.bytes()[off + i * 4..off + i * 4 + 4];
            let c = u32::from_le_bytes(b.try_into().expect("len 4"));
            sum += c as u64;
            v.push(c); // alloc: Node charged kept_home
        }
        if sum != rec.values {
            return Err(ReadError::Format(FormatError::Corrupt {
                at: "gcount sum vs extent values",
            }));
        }
        Some(v)
    } else {
        None
    };
    let vbyte_prefix = if is_validity {
        // Class-level budget check BEFORE the allocation/loop (idx-198): the
        // validity prefix is sized and built from the extent's file-supplied
        // granule_count with no gcount-table byte witness to cap it (root
        // streams have no gcount table), so a 4-byte lie would otherwise
        // commit gigabytes / spin for billions of iterations before the
        // `acc > payload_len` witness below ever runs.
        let ng = checked_extent_granules(part, rec)?;
        let payload_len = (payload_end - payload_start) as usize;
        let mut prefix: Vec<u32> = Vec::new();
        prefix.try_reserve(ng + 1).map_err(|_| { // alloc: Node charged kept_home
            ReadError::Format(FormatError::Corrupt {
                at: "validity prefix allocation",
            })
        })?;
        prefix.push(0u32); // alloc: Node charged kept_home
        let mut acc: u64 = 0;
        for i in 0..ng as u32 {
            step.batch(i as usize)?;
            let vals = match &gcounts {
                Some(gc) => gc[i as usize],
                None => geom::rows_in_granule_at(part.rows(), part.grain(), rec.granule_start + i),
            };
            acc += (vals as u64).div_ceil(8);
            let acc32 = u32::try_from(acc).map_err(|_| {
                ReadError::Format(FormatError::Corrupt {
                    at: "validity prefix overflow",
                })
            })?;
            prefix.push(acc32); // alloc: Node charged kept_home
        }
        if acc as usize > payload_len {
            return Err(ReadError::Format(FormatError::Corrupt {
                at: "validity payload short",
            }));
        }
        Some(prefix)
    } else {
        None
    };
    Ok(ExtentState {
        bytes,
        frame_table,
        gcounts,
        vbyte_prefix,
        payload_start,
        payload_end,
    })
}

/// Load a single-extent byte-run stream (overflow / dict): the payload
/// region of its one section. Multi-extent runs cannot flow through the
/// frozen single-slice ctx — typed `Unsupported` (crate doc).
///
/// `dict_index`: the dictionary's resident v1 index image, in hand for the
/// DictPayload load (a `FrontZstd` payload expands against it).
pub(crate) fn load_region_with(
    part: &Arc<OpenPart>,
    unwrappers: &[&dyn crate::SectionUnwrapper],
    ps: &ParsedStream,
    what: &'static str,
    dict_index: Option<&[u8]>,
) -> ReadResult<RegionState> {
    load_region_res(part, unwrappers, ps, what, dict_index, Residency::Shared, None)
}

/// [`load_region_with`] under a cursor's [`Residency`]: `Private` reads a
/// region the plane does not hold into a buffer only the caller keeps.
fn load_region_res(
    part: &Arc<OpenPart>,
    unwrappers: &[&dyn crate::SectionUnwrapper],
    ps: &ParsedStream,
    what: &'static str,
    dict_index: Option<&[u8]>,
    residency: Residency,
    owner: ReadOwner,
) -> ReadResult<RegionState> {
    let extent = |rec: &pgrc_format::part::ExtentRecord, i: u32| match residency {
        Residency::Shared => part.extent_bytes(&ps.entry, rec, i),
        Residency::Private => part.extent_bytes_uncached_for(owner, &ps.entry, rec, i),
        Residency::Cached => part.extent_bytes_cached_for(owner, &ps.entry, rec, i),
    };
    let entry_wrapper = Wrapper::from_u8(ps.entry.wrapper)?;
    if ps.extents.len() != 1 {
        // SB-7 frame-boundary dict extents: an UNWRAPPED byte-run stream
        // whose extent table partitions one contiguous section (the writer
        // cuts the DictPayload extents at dict-frame boundaries, each with
        // its own CRC) assembles back into the single region the frozen
        // ctx shape needs. Realized fault grain HERE is still the whole
        // region (every extent faults, CRC-validated individually); the
        // per-frame lazy fault rides the M3-L3 reader rebuild on exactly
        // this geometry — the format is ready, the laziness is the
        // consumer's. Wrapped multi-extent byte runs stay refused (the
        // O-CMP-5(a) unwrap is whole-section by design).
        if entry_wrapper != Wrapper::None {
            return Err(ReadError::Unsupported {
                what: "multi-extent wrapped byte-run stream",
            });
        }
        let mut step = cancelpoll::step("pgrc_read load_region_res: a byte-run region's extents");
        for (i, w) in ps.extents.windows(2).enumerate() {
            step.batch(i)?;
            if w[0].file_off + w[0].len != w[1].file_off {
                return Err(ReadError::Format(FormatError::Corrupt {
                    at: "byte-run extent contiguity",
                }));
            }
        }
        let total: usize = ps.extents.iter().map(|e| e.len as usize).sum();
        let mut off = 0usize;
        let mut segs: memledger::ChargedVec<SegBuf> = read_vec(owner);
        segs.try_reserve(ps.extents.len()).map_err(|_| ReadError::Io { at: "byte-run region extents", errno: libc::ENOMEM })?; // alloc: Exec charged owner
        for (i, rec) in ps.extents.iter().enumerate() {
            step.tick()?;
            let seg = extent(rec, i as u32)?;
            segs.with_vec(|v| v.push(seg)); // alloc: Exec charged owner
        }
        let bytes = SegBuf::assemble_for(owner, total, |b| {
            for seg in segs.iter() {
                step.tick()?;
                b[off..off + seg.len()].copy_from_slice(seg.bytes());
                off += seg.len();
            }
            Ok(())
        })?;
        let hdr = StreamSectionHdr::decode(bytes.bytes())?;
        if hdr.encoding != ps.entry.encoding || hdr.wrapper != ps.entry.wrapper {
            return Err(ReadError::Format(FormatError::Corrupt { at: what }));
        }
        let (payload_start, payload_end) = payload_bounds(&hdr, bytes.len())?;
        return Ok(RegionState {
            bytes,
            payload_start,
            payload_end,
        });
    }
    let rec = ps.extents[0];
    let bytes = if entry_wrapper == Wrapper::None {
        let bytes = extent(&rec, 0)?;
        let hdr = StreamSectionHdr::decode(bytes.bytes())?;
        if hdr.encoding != ps.entry.encoding || hdr.wrapper != ps.entry.wrapper {
            return Err(ReadError::Format(FormatError::Corrupt { at: what }));
        }
        bytes
    } else {
        // CMP-B (wrapper-offer class widening): byte-run streams unwrap
        // through the SAME part-resident unwrapped-image cache as value
        // extents (O-CMP-5(a)) — decompress once per part residency; the
        // rebuilt region is a part-lifetime `SegBuf`, so the StrView §7b
        // dict-payload region-stability law holds for wrapped payloads
        // exactly as for unwrapped ones. Typed refusal BEFORE any payload
        // fault when this binding lacks the arm (the old-binary posture —
        // this refusal replaces the v1 "byte-run streams are never wrapped"
        // refusal, which now applies only through it).
        let uw = unwrappers
            .iter()
            .find(|u| u.wrapper() == entry_wrapper)
            .ok_or(ReadError::Format(FormatError::WrapperUnsupported {
                wrapper: entry_wrapper.as_u8(),
            }))?;
        let mut rebuild = |raw: &[u8]| -> ReadResult<Vec<u8>> {
            let mut step = cancelpoll::step("pgrc_read load_region_res: a wrapped region's rebuild");
            let hdr = StreamSectionHdr::decode(raw)?;
            if hdr.encoding != ps.entry.encoding || hdr.wrapper != ps.entry.wrapper {
                return Err(ReadError::Format(FormatError::Corrupt { at: what }));
            }
            let rebuilt = match dict_index {
                Some(index) if entry_wrapper == Wrapper::FrontZstd && crate::dict_resident_frontcoded() => {
                    uw.unwrap_dict_payload_fc_section_until(&hdr, raw, index, &mut || step.tick().is_err())?.ok_or(ReadError::Canceled)?
                }
                // D3887: a large dictionary's payload is seconds of inflate and expand: per block, the statement's cancel
                Some(index) => uw.unwrap_dict_payload_section_until(&hdr, raw, index, &mut || step.tick().is_err())?.ok_or(ReadError::Canceled)?,
                None => uw.unwrap_section_until(&hdr, raw, &mut || step.tick().is_err())?.ok_or(ReadError::Canceled)?,
            };
            let twin = StreamSectionHdr::decode(&rebuilt)?;
            if twin.wrapper != Wrapper::None.as_u8() {
                return Err(ReadError::Format(FormatError::Corrupt {
                    at: "unwrapped section still wrapped",
                }));
            }
            Ok(rebuilt)
        };
        match residency {
            Residency::Shared => part.unwrapped_extent_bytes_for(owner, &ps.entry, &rec, 0, &mut rebuild)?,
            Residency::Private => part.unwrapped_extent_bytes_uncached_for(owner, &ps.entry, &rec, 0, &mut rebuild)?,
            Residency::Cached => part.unwrapped_extent_bytes_cached_for(owner, &ps.entry, &rec, 0, &mut rebuild)?,
        }
    };
    let hdr = StreamSectionHdr::decode(bytes.bytes())?;
    let (payload_start, payload_end) = payload_bounds(&hdr, bytes.len())?;
    Ok(RegionState {
        bytes,
        payload_start,
        payload_end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wave 3 Q2: a cursor's own tables are its statement's from its open, grow with an extent's load, follow the breaker
    /// its source names, its part use to the session at the park and back at the rejoin, and leave with the cursor.
    #[test]
    fn a_cursors_kept_charge_follows_its_owner_and_leaves_with_it() {
        use memledger::charge::{self, Owner};
        let rows = 4 * u64::from(pgrc_format::geom::GRANULE_ROWS);
        let mut col = crate::testpart::text_col(1, rows, Some(7));
        col.extent_cuts = vec![1, 2, 3];
        let b = crate::testpart::build_part(&crate::testpart::PartSpec::new(rows, vec![col]));
        let part = crate::tests::open_built(&b, 213, 1);
        memledger::bind_backend(213, 9213);
        memledger::statement_begin();
        let (stmt, release) = charge::context();
        let sess = Owner::Session(stmt.acct());
        let row = |o: Owner| charge::owner_row(o).committed;
        let (s0, e0) = (row(stmt), row(sess));
        let mut cur = StreamCursor::open(part.clone(), reference_binding_leaked(), 1, 0).expect("open");
        let (n, at) = cur.kept();
        assert!(n > 0, "the slots and stream copies");
        assert_eq!((at, row(stmt) - s0), (Some(stmt), n as i64), "the statement's from the open");
        let mut vw = vec![0u64; 1 << 12];
        cur.validity(0, &mut vw).expect("validity");
        let (n2, _) = cur.kept();
        assert!(n2 > n, "a validity extent's prefix table");
        assert_eq!(row(stmt) - s0, n2 as i64, "the load's tables charged");
        let op = charge::op_begin(stmt, 1, 0);
        cur.set_owner(Some((op, memledger::Kind::HashAgg)));
        assert_eq!((cur.kept().1, row(op), row(stmt) - s0), (Some(op), n2 as i64, 0), "on the breaker");
        assert_eq!(cur.read_owner(), Some((op, memledger::Kind::HashAgg)));
        cur.keep_past_statement();
        assert_eq!((cur.kept().1, row(op), row(sess) - e0), (Some(sess), 0, n2 as i64), "the park: the session's");
        assert_eq!(cur.read_owner(), None, "a parked cursor reads for no one");
        cur.rejoin(stmt, release);
        assert_eq!((cur.kept().1, row(sess) - e0, row(stmt) - s0), (Some(stmt), 0, n2 as i64), "the rejoin: the statement's");
        // a worker set's park (the slot depot): kept_uses_to parks the use and the bytes follow it, then the source's
        // transfer_held moves the bytes alone to the depot (the holders' rows are process-wide: only owners are compared)
        let (parked, depot) = (Owner::Cache(memledger::Holder::Parked), Owner::Cache(memledger::Holder::SlotDepot));
        cur.transfer_use(parked, memledger::Release::Evict);
        assert_eq!((cur.kept().1, row(stmt) - s0), (Some(parked), 0), "the bytes follow the use to Parked");
        // D4573: this second park keeps the extent the first one parked, its table with it (it dropped the table before,
        // and the next statement parsed it again)
        assert_eq!((cur.kept().0, cur.tables_now()), (n2, n2), "the depot park: the parked extent's table kept, charged");
        assert_eq!(cur.valstate.iter().filter(|s| matches!(s, ExtSlot::Parked(_))).count(), 1, "the extent parked, not gone");
        cur.transfer_kept(depot, memledger::Release::Evict);
        assert_eq!((cur.kept().1, cur.use_owner()), (Some(depot), parked), "the depot holds the bytes, Parked the use");
        // the take: the taking statement's bytes at once (transfer_held), its use at the bind's rejoin
        cur.transfer_kept(stmt, release);
        assert_eq!((cur.kept().1, cur.use_owner(), row(stmt) - s0), (Some(stmt), parked, n2 as i64), "the take");
        cur.rejoin(stmt, release);
        assert_eq!((cur.kept().1, cur.use_owner(), row(stmt) - s0), (Some(stmt), stmt, n2 as i64), "the take's rejoin");
        // the next read takes the image back and parses nothing again: the kept table serves it
        cur.validity(0, &mut vw).expect("validity after the take");
        assert_eq!((cur.kept().0, cur.tables_now(), row(stmt) - s0), (n2, n2, n2 as i64), "the reread: no table built again");
        drop(cur);
        assert_eq!((row(stmt) - s0, row(sess) - e0), (0, 0), "nothing left at the drop");
        charge::op_end(op);
        memledger::statement_end();
        memledger::unbind_backend();
    }

    /// D4554: pooled cursors charge their tables to their reader's pool, which moves them all in one step; a load commits
    /// on it, a park's drop frees on it, the drop takes a cursor's bytes off, and the cursors' own owner moves move nothing.
    #[test]
    fn pooled_cursors_charge_their_pool_which_moves_them_at_once() {
        use memledger::charge::{self, Owner};
        let rows = 4 * u64::from(pgrc_format::geom::GRANULE_ROWS);
        let mut col = crate::testpart::text_col(1, rows, Some(7));
        col.extent_cuts = vec![1, 2, 3];
        let b = crate::testpart::build_part(&crate::testpart::PartSpec::new(rows, vec![col]));
        let part = crate::tests::open_built(&b, 216, 1);
        memledger::bind_backend(216, 9216);
        memledger::statement_begin();
        let (stmt, release) = charge::context();
        let sess = Owner::Session(stmt.acct());
        let row = |o: Owner| charge::owner_row(o).committed;
        let (s0, e0) = (row(stmt), row(sess));
        let pool = Arc::new(KeptPool::new());
        pool.home(Some((stmt, memledger::Kind::Region)));
        let mut a = StreamCursor::open(part.clone(), reference_binding_leaked(), 1, 0).expect("open");
        let mut c = StreamCursor::open(part.clone(), reference_binding_leaked(), 1, 0).expect("open");
        let (na, nc) = (a.kept().0, c.kept().0);
        a.set_pool(Arc::clone(&pool));
        c.set_pool(Arc::clone(&pool));
        assert_eq!((pool.bytes(), row(stmt) - s0, a.kept().1), (na + nc, (na + nc) as i64, Some(stmt)), "both on the pool, the statement's");
        let mut vw = vec![0u64; 1 << 12];
        a.validity(0, &mut vw).expect("validity");
        let na2 = a.kept().0;
        assert!(na2 > na, "a load's tables");
        assert_eq!((pool.bytes(), row(stmt) - s0), (na2 + nc, (na2 + nc) as i64), "the load commits on the pool");
        // a cursor's own moves move nothing; the pool's one move moves them all
        let depot = Owner::Cache(memledger::Holder::SlotDepot);
        a.transfer_kept(depot, memledger::Release::Evict);
        a.keep_past_statement();
        c.keep_past_statement();
        assert_eq!(row(stmt) - s0, (na2 + nc) as i64, "the cursors' own moves left the pool where it was");
        pool.park();
        assert_eq!((pool.owner(), row(stmt) - s0, row(sess) - e0), (Some(sess), 0, (na2 + nc) as i64), "the park: the session's, at once");
        a.rejoin(stmt, release);
        c.rejoin(stmt, release);
        pool.home(Some((stmt, memledger::Kind::Region)));
        assert_eq!((row(sess) - e0, row(stmt) - s0), (0, (na2 + nc) as i64), "the rejoin: the statement's again");
        drop(c);
        assert_eq!((pool.bytes(), row(stmt) - s0), (na2, na2 as i64), "a drop takes its bytes off the pool");
        drop(a);
        assert_eq!((pool.bytes(), row(stmt) - s0, row(sess) - e0), (0, 0, 0), "nothing left");
        drop(pool);
        memledger::statement_end();
        memledger::unbind_backend();
    }

    /// E2: an extent a pass parked keeps its tables until the cursor's park, which drops them, and the cursor's kept bytes
    /// (its charge) follow at every step: before, the park dropped them and the charge kept them, so the slot depot's
    /// trim of a parked set handed the trimming backend's account bytes no free took back (a 10 GB join suite at W8:
    /// the cursors' charges 48.1 MB, their frees 23.6 MB).
    #[test]
    fn a_park_after_a_pass_takes_the_passed_extents_tables_off_the_cursors_charge() {
        use memledger::charge;
        let rows = 4 * u64::from(pgrc_format::geom::GRANULE_ROWS);
        let mut col = crate::testpart::text_col(1, rows, Some(7));
        col.extent_cuts = vec![1, 2, 3];
        let b = crate::testpart::build_part(&crate::testpart::PartSpec::new(rows, vec![col]));
        let part = crate::tests::open_built(&b, 214, 1);
        memledger::bind_backend(214, 9214);
        memledger::statement_begin();
        let (stmt, _) = charge::context();
        let s0 = charge::owner_row(stmt).committed;
        let mut cur = StreamCursor::open(part.clone(), reference_binding_leaked(), 1, 0).expect("open");
        cur.set_pass_behind(true);
        let mut vw = vec![0u64; 1 << 12];
        let mut most = 0;
        for g in 0..4 {
            cur.validity(g, &mut vw).expect("validity");
            assert_eq!(cur.kept().0, cur.tables_now(), "granule {g}: the loads and the passes are counted");
            most = most.max(cur.kept().0);
        }
        let passed = cur.valstate.iter().filter(|s| matches!(s, ExtSlot::Parked(_))).count();
        assert!(passed >= 2, "the reads passed the extents behind them ({passed} parked)");
        cur.park_images();
        assert!(cur.valstate.iter().all(|s| !matches!(s, ExtSlot::Loaded(_))), "no image held past the park");
        assert_eq!(cur.kept().0, cur.tables_now(), "the park: the passed extents' tables left with their bytes (trunk: kept the bytes)");
        assert!(cur.kept().0 < most, "the park dropped tables");
        assert_eq!(charge::owner_row(stmt).committed - s0, cur.kept().0 as i64, "the charge holds what the cursor holds");
        // D4573: a second park (a worker set's: its source's unbind, then its use's move to the slot depot) keeps what the
        // first one kept
        let (kept, parked) = (cur.kept().0, cur.valstate.iter().filter(|s| matches!(s, ExtSlot::Parked(_))).count());
        assert!(parked >= 1, "the park kept the extent it parked itself ({parked} parked)");
        cur.park_images();
        assert_eq!((cur.kept().0, cur.tables_now()), (kept, kept), "the second park: the tables kept");
        assert_eq!(cur.valstate.iter().filter(|s| matches!(s, ExtSlot::Parked(_))).count(), parked, "the second park: nothing dropped");
        // a later read unparks the kept extent and loads the dropped ones again: still counted
        cur.validity(0, &mut vw).expect("validity again");
        assert_eq!(cur.kept().0, cur.tables_now(), "the reload");
        drop(cur);
        assert_eq!(charge::owner_row(stmt).committed - s0, 0, "nothing left at the drop");
        memledger::statement_end();
        memledger::unbind_backend();
    }

    fn plain_hdr() -> StreamSectionHdr {
        // Closed-form/root stream with no tables: the `end = section_len`
        // fallback arm of `payload_bounds` (idx-196 truncation path).
        StreamSectionHdr {
            magic: 0,
            encoding: 0,
            width: 0,
            wrapper: 0,
            frame_count: 0,
            frame_table_off: 0,
            gcount_table_off: 0,
            uncompressed_len: 0,
            value_count: 0,
            reserved: 0,
        }
    }

    #[test]
    fn payload_bounds_ok_for_in_range_region() {
        let hdr = plain_hdr();
        let (start, end) = payload_bounds(&hdr, STREAM_SECTION_HDR_LEN + 100).expect("valid");
        assert_eq!(start, STREAM_SECTION_HDR_LEN as u32);
        assert_eq!(end, (STREAM_SECTION_HDR_LEN + 100) as u32);
        assert!(start <= end, "returned window must be ordered");
    }

    #[test]
    fn payload_bounds_rejects_over_u32_assembled_region() {
        // An assembled multi-extent byte-run region of 2^32 + 16 bytes: the
        // old `end as u32` truncated to 16 (< payload_start = 32), an
        // inverted window. Must now surface a typed, catchable error rather
        // than a truncated (32, 16) pair.
        let hdr = plain_hdr();
        let big = (u32::MAX as usize) + 1 + 16; // 2^32 + 16
        let err = payload_bounds(&hdr, big).err().unwrap();
        assert!(matches!(err, FormatError::Bounds { .. }));
    }

    #[test]
    fn payload_bounds_never_inverts_across_u32_boundary() {
        // Any accepted result is ordered; the boundary case (exactly u32::MAX
        // total) either accepts an ordered pair or refuses — never inverts.
        let hdr = plain_hdr();
        match payload_bounds(&hdr, u32::MAX as usize) {
            Ok((start, end)) => assert!(start <= end),
            Err(FormatError::Bounds { .. }) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
}
