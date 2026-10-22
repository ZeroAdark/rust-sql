//! The SELECT face: a serial/parallel granule-at-a-time scan over the
//! frozen M3-F reader, feeding Virtual slots (the tableam
//! `TableScanDesc::Pgrcolumnar` arm drives [`PgrcScanDescData::
//! getnextslot`]).
//!
//! This is the DDL lane's CORRECTNESS scan (full decode of every column,
//! the FULL codec-registry binding — the M3-J election wiring writes real
//! elections, so this scan resolves the same vtables lx_source's
//! implementor does). Pruning, late materialization, dict lanes, and morsel-grain
//! parallel claims are M3-G's `lx_source` implementor; this scan exists so
//! CREATE→COPY→SELECT round-trips in a real backend and COPY TO works,
//! with parallel workers claiming at PART grain via the shared
//! `phs_nallocated` cursor (the old-AM parallelscan shape).
//!
//! Every part open is recorded in [`crate::inval`]'s relid ledger — the
//! ino-reuse hole closes at scan-begin recording + relcache-callback
//! invalidation.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use pgrc_format::abi::{ByteArena, DecodeOut, Selection, ValidityVerdict};
use pgrc_format::class::{ColSchema, CLASS_VARLENA};
use pgrc_format::enc::EncodingId;
use pgrc_format::geom::{GranuleGrain, GRANULE_ROWS};
use pgrc_format::ident::schema_fingerprint;
use pgrc_format::manifest::SegState;
use pgrc_format::part::StreamSectionHdr;
use pgrc_read::{
    resolve_effective, CodecBinding, DictHandle, OpenPart, PartExpect, PartPin, ReadError, Residency,
    SectionUnwrapper, StreamCursor, TableExpect, VfsPartIo, VfsTableDir,
};
use tableam_vocab::TableScanDescData;
use types_error::{PgError, PgResult};
use types_slot::SlotData;

use crate::probe::SnapshotCommitCheck;
use crate::read_error;
use pgrc_read::mem::Guard;
use pgrc_read::{CommitCheck, EffectiveManifest};

/// Length-prefix trust: a granule's decode kernel writes exactly the
/// per-granule value-count (`written`, an untrusted gcount taken from the
/// part file) datums into the output buffer and returns that count, whereas
/// the AM sizes its buffers by — and `store_row` later publishes —
/// `expected` rows (the closed-form footer geometry, `rows_in_granule`).
/// When `written < expected` the slots `[written..expected)` were never
/// written for this granule and hold zero / stale / dangling datums from the
/// reused `ColBuf`; publishing them discloses uninitialized or prior-row
/// backend memory (and can NULL-deref pointer-class columns). Bind the served
/// row count to the written witness: refuse with a catchable
/// `ERRCODE_DATA_CORRUPTED` on ANY mismatch rather than publish unwritten
/// datums. Valid parts (gcount == closed-form geometry) pass unchanged.
fn check_decoded_count(attno: u32, g: u32, written: u32, expected: u32) -> PgResult<()> {
    if written != expected {
        return Err(Box::<PgError>::new( // alloc: Cold
            PgError::error(format!( // alloc: Cold
                "pgrcolumnar: column {} granule {} value-count mismatch \
                 (decoded {} of {} rows)",
                attno, g, written, expected
            ))
            .with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// the product codec binding (the lx_source `pgrc_codec_binding` shape,
// mirrored: M3-C's full kernel registry + the LZ4 section unwrapper).
// The M3-J election wiring makes COPY-written parts carry real elections,
// so the DDL lane's correctness scan must resolve the SAME vtables a
// conformant reader does — the reference binding reads only the
// VERBATIM/CONST election set and refuses everything else.
// ---------------------------------------------------------------------------

struct Lz4Unwrapper;

impl SectionUnwrapper for Lz4Unwrapper {
    fn wrapper(&self) -> pgrc_format::enc::Wrapper {
        pgrc_format::enc::Wrapper::Lz4
    }
    fn unwrap_section(
        &self,
        _hdr: &StreamSectionHdr,
        section: &[u8],
    ) -> pgrc_format::FormatResult<Vec<u8>> {
        let mut out = Vec::new();
        pgrc_codec::wrapper::unwrap_section(section, &mut out)?;
        Ok(out)
    }
    fn unwrap_section_until(&self, _hdr: &StreamSectionHdr, section: &[u8], stop: &mut dyn FnMut() -> bool) -> pgrc_format::FormatResult<Option<Vec<u8>>> {
        let mut out = Vec::new();
        Ok(pgrc_codec::wrapper::unwrap_section_until(section, &mut out, stop)?.then_some(out))
    }
    fn unwrap_dict_payload_section_until(&self, _hdr: &StreamSectionHdr, section: &[u8], _index: &[u8], stop: &mut dyn FnMut() -> bool) -> pgrc_format::FormatResult<Option<Vec<u8>>> {
        let mut out = Vec::new();
        Ok(pgrc_codec::wrapper::unwrap_section_until(section, &mut out, stop)?.then_some(out))
    }
}

/// Zstd section unwrap (spec §6.4 `wrapper = 2`, the CMP-A slot-fill) —
/// same codec entry point; the wrapper byte in the section header selects
/// the arm. One binding per plain-zstd arm (`Zstd`, `OnPairZstd`); only `Zstd` has a block face.
struct ZstdUnwrapper(pgrc_format::enc::Wrapper);

impl SectionUnwrapper for ZstdUnwrapper {
    fn wrapper(&self) -> pgrc_format::enc::Wrapper {
        self.0
    }
    fn unwrap_section(
        &self,
        _hdr: &StreamSectionHdr,
        section: &[u8],
    ) -> pgrc_format::FormatResult<Vec<u8>> {
        let mut out = Vec::new();
        pgrc_codec::wrapper::unwrap_section(section, &mut out)?;
        Ok(out)
    }
    fn unwrap_section_until(&self, _hdr: &StreamSectionHdr, section: &[u8], stop: &mut dyn FnMut() -> bool) -> pgrc_format::FormatResult<Option<Vec<u8>>> {
        let mut out = Vec::new();
        Ok(pgrc_codec::wrapper::unwrap_section_until(section, &mut out, stop)?.then_some(out))
    }
    fn unwrap_dict_payload_section_until(&self, _hdr: &StreamSectionHdr, section: &[u8], _index: &[u8], stop: &mut dyn FnMut() -> bool) -> pgrc_format::FormatResult<Option<Vec<u8>>> {
        let mut out = Vec::new();
        Ok(pgrc_codec::wrapper::unwrap_section_until(section, &mut out, stop)?.then_some(out))
    }
    fn unwrap_block(&self, src: &[u8], dst: &mut [u8]) -> pgrc_format::FormatResult<bool> {
        if self.0 != pgrc_format::enc::Wrapper::Zstd {
            return Ok(false);
        }
        pgrc_codec::wrapper::unwrap_block_into(pgrc_format::enc::Wrapper::Zstd, src, dst)?;
        Ok(true)
    }
}


/// pgrc format 2.2 dictionary layers (spec §6.4/§7): the front-coded DictPayload
/// (id 3) expands against the resident index, the packed DictIndex (id 4)
/// stands alone; both serve the whole-section and block faces.
/// One binding per front-coded arm (`FrontZstd`, `FrontOnPairZstd`): every face dispatches on the
/// section's wrapper byte; the OnPair arm's block face needs its decoded side block (the section's token table).
struct FrontZstdUnwrapper(pgrc_format::enc::Wrapper);

impl SectionUnwrapper for FrontZstdUnwrapper {
    fn wrapper(&self) -> pgrc_format::enc::Wrapper {
        self.0
    }
    fn unwrap_section(&self, _hdr: &StreamSectionHdr, section: &[u8]) -> pgrc_format::FormatResult<Vec<u8>> {
        let mut out = Vec::new();
        pgrc_codec::wrapper::unwrap_section(section, &mut out)?;
        Ok(out)
    }
    fn unwrap_section_until(&self, _hdr: &StreamSectionHdr, section: &[u8], stop: &mut dyn FnMut() -> bool) -> pgrc_format::FormatResult<Option<Vec<u8>>> {
        let mut out = Vec::new();
        Ok(pgrc_codec::wrapper::unwrap_section_until(section, &mut out, stop)?.then_some(out))
    }
    fn unwrap_dict_payload_section(&self, _hdr: &StreamSectionHdr, section: &[u8], index: &[u8]) -> pgrc_format::FormatResult<Vec<u8>> {
        let mut out = Vec::new();
        pgrc_codec::wrapper::unwrap_dict_payload_section(section, index, &mut out)?;
        Ok(out)
    }
    fn unwrap_dict_payload_section_until(&self, _hdr: &StreamSectionHdr, section: &[u8], index: &[u8], stop: &mut dyn FnMut() -> bool) -> pgrc_format::FormatResult<Option<Vec<u8>>> {
        let mut out = Vec::new();
        Ok(pgrc_codec::wrapper::unwrap_dict_payload_section_until(section, index, &mut out, stop)?.then_some(out))
    }
    fn decode_side(&self, side: &[u8]) -> pgrc_format::FormatResult<Option<pgrc_read::SideTable>> {
        if !self.0.side_block() {
            return Ok(None);
        }
        let t = pgrc_codec::onpairwrap::decode_side(side)?;
        let bytes = t.bytes();
        Ok(Some(pgrc_read::SideTable { table: Box::new(t), bytes }))
    }
    fn unwrap_dict_payload_block_side(&self, side: &pgrc_read::SideTable, src: &[u8], dst: &mut [u8], records: &[u8]) -> pgrc_format::FormatResult<bool> {
        let Some(t) = side.table.downcast_ref::<pgrc_codec::onpair::DecodeTable>() else { return Ok(false) };
        pgrc_codec::onpairwrap::unwrap_frame_block(t, src, dst, records)?;
        Ok(true)
    }
    fn unwrap_dict_payload_block(&self, src: &[u8], dst: &mut [u8], records: &[u8]) -> pgrc_format::FormatResult<bool> {
        if self.0 != pgrc_format::enc::Wrapper::FrontZstd {
            return Ok(false);
        }
        pgrc_codec::wrapper::unwrap_dict_payload_block(pgrc_format::enc::Wrapper::FrontZstd, src, dst, records)?;
        Ok(true)
    }
    fn unwrap_dict_payload_fc_section(&self, _hdr: &StreamSectionHdr, section: &[u8], index: &[u8]) -> pgrc_format::FormatResult<Vec<u8>> {
        let mut out = Vec::new();
        pgrc_codec::wrapper::unwrap_dict_payload_fc_section(section, index, &mut out)?;
        Ok(out)
    }
    fn unwrap_dict_payload_fc_section_until(&self, _hdr: &StreamSectionHdr, section: &[u8], index: &[u8], stop: &mut dyn FnMut() -> bool) -> pgrc_format::FormatResult<Option<Vec<u8>>> {
        let mut out = Vec::new();
        Ok(pgrc_codec::wrapper::unwrap_dict_payload_fc_section_until(section, index, &mut out, stop)?.then_some(out))
    }
}

struct PackedZstdUnwrapper;

impl SectionUnwrapper for PackedZstdUnwrapper {
    fn wrapper(&self) -> pgrc_format::enc::Wrapper {
        pgrc_format::enc::Wrapper::PackedZstd
    }
    fn unwrap_section(&self, _hdr: &StreamSectionHdr, section: &[u8]) -> pgrc_format::FormatResult<Vec<u8>> {
        let mut out = Vec::new();
        pgrc_codec::wrapper::unwrap_section(section, &mut out)?;
        Ok(out)
    }
    fn unwrap_section_until(&self, _hdr: &StreamSectionHdr, section: &[u8], stop: &mut dyn FnMut() -> bool) -> pgrc_format::FormatResult<Option<Vec<u8>>> {
        let mut out = Vec::new();
        Ok(pgrc_codec::wrapper::unwrap_section_until(section, &mut out, stop)?.then_some(out))
    }
    fn unwrap_block(&self, src: &[u8], dst: &mut [u8]) -> pgrc_format::FormatResult<bool> {
        pgrc_codec::wrapper::unwrap_block_into(pgrc_format::enc::Wrapper::PackedZstd, src, dst)?;
        Ok(true)
    }
}

/// The integer pre-pass wrapper (id 8): value streams only, the same codec entry point.
struct IntPreUnwrapper;

impl SectionUnwrapper for IntPreUnwrapper {
    fn wrapper(&self) -> pgrc_format::enc::Wrapper {
        pgrc_format::enc::Wrapper::IntPre
    }
    fn unwrap_section(&self, _hdr: &StreamSectionHdr, section: &[u8]) -> pgrc_format::FormatResult<Vec<u8>> {
        let mut out = Vec::new();
        pgrc_codec::wrapper::unwrap_section(section, &mut out)?;
        Ok(out)
    }
    fn unwrap_section_until(&self, _hdr: &StreamSectionHdr, section: &[u8], stop: &mut dyn FnMut() -> bool) -> pgrc_format::FormatResult<Option<Vec<u8>>> {
        let mut out = Vec::new();
        Ok(pgrc_codec::wrapper::unwrap_section_until(section, &mut out, stop)?.then_some(out))
    }
    fn unwrap_block(&self, src: &[u8], dst: &mut [u8]) -> pgrc_format::FormatResult<bool> {
        pgrc_codec::wrapper::unwrap_block_into(pgrc_format::enc::Wrapper::IntPre, src, dst)?;
        Ok(true)
    }
}

static LZ4_UNWRAPPER: Lz4Unwrapper = Lz4Unwrapper;
static ZSTD_UNWRAPPER: ZstdUnwrapper = ZstdUnwrapper(pgrc_format::enc::Wrapper::Zstd);
static ONPAIR_VALUES_UNWRAPPER: ZstdUnwrapper = ZstdUnwrapper(pgrc_format::enc::Wrapper::OnPairZstd);
static FRONT_UNWRAPPER: FrontZstdUnwrapper = FrontZstdUnwrapper(pgrc_format::enc::Wrapper::FrontZstd);
static FRONT_ONPAIR_UNWRAPPER: FrontZstdUnwrapper = FrontZstdUnwrapper(pgrc_format::enc::Wrapper::FrontOnPairZstd);
static PACKED_UNWRAPPER: PackedZstdUnwrapper = PackedZstdUnwrapper;
static INTPRE_UNWRAPPER: IntPreUnwrapper = IntPreUnwrapper;
static UNWRAPPERS: [&dyn SectionUnwrapper; 7] = [
    &LZ4_UNWRAPPER,
    &ZSTD_UNWRAPPER,
    &FRONT_UNWRAPPER,
    &PACKED_UNWRAPPER,
    &FRONT_ONPAIR_UNWRAPPER,
    &ONPAIR_VALUES_UNWRAPPER,
    &INTPRE_UNWRAPPER,
];

/// Init-once through `pgsync::OnceLock` (the single lock library; pure init
/// closure — the codec registry's own once-cell posture).
/// The production codec binding (registry + LZ4/ZSTD unwrappers); shared
/// with Gravity's granule source, which decodes the same parts on pool threads.
pub fn codec_binding() -> &'static CodecBinding<'static> {
    static BINDING: pgsync::OnceLock<CodecBinding<'static>> = pgsync::OnceLock::new();
    BINDING.get_or_init(|| CodecBinding {
        registry: pgrc_codec::registry(),
        unwrappers: &UNWRAPPERS,
    })
}

/// DM-2: load one part's delete vector when its manifest record references
/// one (`dv_gen != 0`). The Dv is AUTHORITATIVE sidecar state (spec
/// §15/§16): the manifest triplet (`dv_gen`, `dv_len`, `dv_crc`) validates
/// the payload, and ANY miss — absent file, stale envelope, length or crc
/// mismatch, internal inconsistency — is a typed CORRUPTION refusal, never
/// a silent Dv-free scan (which would resurrect deleted rows).
pub(crate) fn load_part_deletes(
    dir: &str,
    rec: &pgrc_format::manifest::PartRecord,
) -> PgResult<Option<Arc<pgrc_scan::PartDeletes>>> {
    if rec.dv_gen == 0 {
        return Ok(None);
    }
    let corrupt = |detail: String| {
        Box::<PgError>::new( // alloc: Cold
            PgError::error(format!( // alloc: Cold
                "pgrcolumnar: part {} delete vector (gen {}): {detail}",
                rec.part_no, rec.dv_gen
            ))
            .with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED),
        )
    };
    let consult = pgrc_read::sidecar::read_sidecar(
        &VfsTableDir::new(dir.to_string()),
        rec.part_no,
        pgrc_format::sidecar::SidecarKind::Dv,
        rec.dv_gen,
        None,
        None,
    )
    .map_err(read_error)?;
    let payload = match consult {
        pgrc_read::sidecar::SidecarConsult::Hit { payload, .. } => payload,
        pgrc_read::sidecar::SidecarConsult::Absent => {
            return Err(corrupt("sidecar file absent (manifest references it)".to_string())) // alloc: Cold
        }
        pgrc_read::sidecar::SidecarConsult::Stale(why) => {
            return Err(corrupt(format!("sidecar envelope stale: {why}"))) // alloc: Cold
        }
    };
    if payload.len() as u64 != rec.dv_len {
        return Err(corrupt(format!( // alloc: Cold
            "payload length {} vs manifest dv_len {}",
            payload.len(),
            rec.dv_len
        )));
    }
    if pgrc_format::wire::crc32c(&payload) != rec.dv_crc {
        return Err(corrupt("payload crc vs manifest dv_crc".to_string())); // alloc: Cold
    }
    let deletes = pgrc_scan::PartDeletes::from_dv_payload(
        &payload,
        rec.part_no,
        rec.dv_gen,
        rec.granule_count,
    )
    .map_err(|e| corrupt(format!("payload refused: {e}")))?; // alloc: Cold
    Ok(Some(Arc::new(deletes)))
}

/// The reader-side directory open: the strict scan-free walk. End-of-recovery cleanup ran before any reader, so
/// `ManifestMissing` is a true corruption tripwire (spec §13.3 walk law).
pub(crate) fn resolve_for_scan(
    dir: &str,
    check: &dyn CommitCheck,
    expect: &TableExpect,
) -> PgResult<Option<EffectiveManifest>> {
    resolve_effective(&VfsTableDir::new(dir.to_string()), check, expect).map_err(read_error)
}

/// The manifest-head probe (the sqe engine-registry currency check): the
/// head generation the catalog holds for `rel`, `Ok(None)` = no committed
/// publish. In memory; no file is touched. The caller pairs it with the
/// commit-visibility of the generation it cached.
pub fn probe_manifest_head(rel: &types_rel::Relation<'_>) -> PgResult<Option<u64>> {
    let locator = rel.rd_locator.get();
    let slot: crate::catalog::RelSlot = (locator.spcOid, locator.dbOid, locator.relNumber as u64);
    Ok(crate::catalog::head(slot).filter(|&h| h != 0))
}

const VALIDITY_WORDS: usize = (GRANULE_ROWS as usize).div_ceil(64);
/// Initial per-column arena; it doubles on an arena refusal while a granule's images do not fit, the data's size its only
/// bound (D2582: a 512 MiB ceiling refused a granule of 70 KB values where PostgreSQL returns the rows), each growth
/// reserved fallibly (`grow_arena`), so a machine that cannot hold the granule is a typed error, never an abort.
const ARENA_INITIAL: usize = 256 << 10;

/// `arena` replaced by a zeroed one of `words` words, fallibly: false (it is left as it was) when the machine cannot hold it.
fn grow_arena(arena: &mut memledger::ChargedVec<u64>, words: usize) -> bool {
    arena.with_vec(|old| {
        let mut v: Vec<u64> = Vec::new();
        if v.try_reserve_exact(words).is_err() { // alloc: Exec charged arena.owner()
            return false;
        }
        v.resize(words, 0); // alloc: Exec charged arena.owner()
        *old = v;
        true
    })
}

/// A scan's own buffers' owner: the statement or portal running it (read once, where the scan begins), `Kind::Region`.
fn stmt_owner_of(o: memledger::Owner) -> Option<(memledger::Owner, memledger::Kind)> {
    matches!(o, memledger::Owner::Statement { .. } | memledger::Owner::Portal { .. }).then_some((o, memledger::Kind::Region))
}

/// One live segment's rows, deformed into the parent's slot (F1; the heap scan is `pgrc_front::segscan`). No
/// lifetime parameter: a `dyn SegmentSource + 'mcx` keeps the scan descriptor covariant.
pub trait SegmentSource {
    fn next(&mut self, slot: &mut SlotData<'_>) -> PgResult<bool>;
    fn end(self: Box<Self>) -> PgResult<()>;
    /// SCANSTATS: the rows it has returned so far (the parent's `seq_tup_read`).
    fn returned(&self) -> u64 {
        0
    }
}

/// IX1 L4: a bitmap heap scan's rows (`pgrc_front::ixbitmap`, attached by tableam's `table_beginscan_bm`). A TID bitmap
/// page of a pgrc table is a virtual block of `IDS_PER_BLOCK` row IDs (IX0 choice 2): the source stages the rows of
/// one page the scan's snapshot sees, then stores them one at a time. No lifetime parameter, as [`SegmentSource`]:
/// the scan descriptor stays covariant.
pub trait BitmapSource {
    /// Stages the visible rows of virtual block `block`: the IDs of `offsets` (an exact page; offset `o` names ID
    /// `block * IDS_PER_BLOCK + o - 1`), or every ID of the block (`None`: a lossy page). Returns how many; the
    /// cursor of [`Self::next`] starts at the first.
    fn stage(&mut self, block: u32, offsets: Option<&[u16]>) -> PgResult<u32>;
    /// Staged row `i` (below [`Self::stage`]'s count) into `slot`; the cursor moves past it.
    fn store<'s>(&mut self, mcx: mcx::Mcx<'s>, i: u32, slot: &mut SlotData<'s>) -> PgResult<()>;
    /// The staged row at the cursor into `slot`; `false` once the page's rows are all stored.
    fn next<'s>(&mut self, mcx: mcx::Mcx<'s>, slot: &mut SlotData<'s>) -> PgResult<bool>;
    /// The executor's column projection (`table_scan_set_needed_attrs`), by `attno - 1`.
    fn set_needed(&mut self, needed: &[bool]);
    /// A rescan: nothing is staged.
    fn reset(&mut self);
    /// The scan's end: what the source opened is closed.
    fn end(self: Box<Self>) -> PgResult<()>;
}

/// A scan's tombstones (F2 L3; built by `pgrc_front::filter`, attached by tableam): the rows they name never surface.
pub trait DeletionFilter {
    /// ORs view part `part`'s granule `g` deletions into `mask`, allocating it when the granule has any.
    fn granule_into(&self, part: usize, g: u32, mask: &mut Option<Box<[u64; pgrc_scan::DV_GRANULE_WORDS]>>);
    /// Ring slot `slot`'s deleted segment positions, sorted.
    fn segment_positions(&self, slot: u16) -> &[u32];
    /// SnapshotDirty: the in-progress deleter of view part `part`'s row `row` of granule `g` (a row the mask keeps).
    fn busy_part(&self, _part: usize, _g: u32, _row: u32) -> Option<types_core::TransactionId> {
        None
    }
    /// SnapshotDirty: ring slot `slot`'s segment positions an in-progress delete holds, with the deleter, sorted.
    fn busy_segment(&self, _slot: u16) -> &[(u32, types_core::TransactionId)] {
        &[]
    }
}

#[cold]
#[inline(never)]
fn process_interrupts() -> PgResult<()> {
    postgres_seams::check_for_interrupts::call()
}

/// C's CHECK_FOR_INTERRUPTS (miscadmin.h): the inline flag test, the service
/// out of line. The row scan checks once per granule and once per decoded
/// column, as heapam checks once per page (heap_fetch_next_buffer): the
/// executor's per-tuple check cannot stop a scan that is still inside one
/// `getnextslot` (a run of granules whose rows are all deleted, a part's
/// first granule faulting every column's extents and dictionaries).
#[inline(always)]
fn check_for_interrupts() -> PgResult<()> {
    if init_small::globals::InterruptPending() {
        return process_interrupts();
    }
    Ok(())
}

/// One part of a relation as a scan sees it under a snapshot: where its file
/// is, what the open must find there, and its delete vector.
pub struct ScanPartSpec {
    pub path: std::ffi::CString,
    pub expect: PartExpect,
    pub rows: u64,
    pub granules: u32,
    pub deletes: Option<Arc<pgrc_scan::PartDeletes>>,
    /// FASTADD: which of the scan's columns the part holds (`None` = every one); an absent one reads its fill.
    pub present: Option<Arc<[bool]>>,
}

pub struct ResolvedScan {
    pub schemas: Vec<ColSchema>,
    pub parts: Vec<ScanPartSpec>,
    /// The live segments (F1): `(ring slot, relid, part_no)` of each active or sealing record, read after the parts
    /// (IX0: `part_no` names the segment's extents).
    pub segments: Vec<(u16, types_core::Oid, u32)>,
    /// The generation the parts are of (F2 L2: the statement's view, which positions name).
    pub gen: Option<Arc<crate::catalog::Generation>>,
}


/// The manifest head a resolve came from: what a caller that keeps the
/// resolved parts resident needs to re-check currency without re-resolving
/// (`probe_manifest_head(dir) == Some(gen)` and the publisher visible to the
/// statement's snapshot — the sqe engine-registry probe, now Gravity's part
/// residency too). `None` = no committed publish.
pub struct ScanHead {
    pub dir: String,
    pub gen: u64,
    pub publisher_fxid: u64,
}

/// `resolve_scan_parts` with the registry pins kept and the manifest head
/// reported (Gravity's resident part table: the pins hold the parts open
/// across statements, so the hit path opens nothing).
pub struct ResolvedScanPinned {
    pub scan: ResolvedScan,
    pub head: Option<ScanHead>,
    pub pins: Vec<PartPin>,
}

/// The effective part list of `rel` under `snapshot` (manifest order), each
/// part opened once through the registry so its key is recorded for
/// invalidation; the pins are dropped (a scan re-pins one part at a time).
/// Shared by `begin` and by Gravity's granule source.
pub fn resolve_scan_parts(
    rel: &types_rel::Relation<'_>,
    snapshot: Option<&types_snapshot::SnapshotData<'_>>,
) -> PgResult<ResolvedScan> {
    Ok(resolve_scan_parts_pinned(rel, snapshot)?.scan)
}

/// The pinned form: the same resolution, the pins returned instead of
/// dropped, plus the head (dir, generation, publisher) the parts came from.
pub fn resolve_scan_parts_pinned(
    rel: &types_rel::Relation<'_>,
    snapshot: Option<&types_snapshot::SnapshotData<'_>>,
) -> PgResult<ResolvedScanPinned> {
    resolve_scan_parts_as(rel, snapshot, crate::schema::col_schemas(rel)?)
}

/// The pinned resolution against `schemas` (ALTER's rewrite reads under the old descriptor's).
fn resolve_scan_parts_as(
    rel: &types_rel::Relation<'_>,
    snapshot: Option<&types_snapshot::SnapshotData<'_>>,
    schemas: Vec<ColSchema>,
) -> PgResult<ResolvedScanPinned> {
    crate::inval::ensure_inval_registered()?;
    let locator = rel.rd_locator.get();
    let slot: crate::catalog::RelSlot = (locator.spcOid, locator.dbOid, locator.relNumber as u64);
    let fp = schema_fingerprint(&schemas);
    let dir = crate::dirpath::table_dir_path(locator, rel.rd_backend);
    crate::catalog::record_relid(rel.rd_id, slot);
    let check = SnapshotCommitCheck::new(snapshot);
    let found = crate::catalog::resolve(rel, slot, &dir, &check)?;
    check.take_error()?;
    let gen = match found {
        crate::catalog::Lookup::Gen(g) => g,
        crate::catalog::Lookup::Empty => return Ok(ResolvedScanPinned { scan: ResolvedScan { schemas, parts: Vec::new(), segments: Vec::new(), gen: None }, head: None, pins: Vec::new() }),
        crate::catalog::Lookup::Unknown => return Err(crate::catalog::not_loaded(rel, &dir)),
    };
    // FASTADD: the generation's schema is the reader's, or one the parts written before an ADD/DROP COLUMN keep (an
    // older snapshot's generation); each part is read under its own, its absent columns filled
    // D2189: a column dropped with no ALTER publish (a dependency's cascade, an inheritance or partition
    // child, a temp type's cleanup) leaves parts under a stamp no generation describes; the schema book gets it from the
    // dropped attribute PG keeps, or the read fails closed (`schema::book_dropped_stamp`)
    let book = |part_fp: u64| if part_fp != fp { crate::schema::book_dropped_stamp(rel, part_fp) } else { Ok(()) };
    let mut presences = crate::fastadd::Presences::new(&schemas, fp);
    book(gen.schema_fp)?;
    presences.of(gen.schema_fp)?;
    let mut parts = crate::alloc::try_vec(gen.parts.len(), "scan")?;
    let mut pins = crate::alloc::try_vec(gen.parts.len(), "scan")?;
    for (p, rec) in gen.parts.iter().zip(&gen.manifest.parts) {
        let part_fp = p.expect.schema_fingerprint.unwrap_or(gen.schema_fp);
        book(part_fp)?;
        let present = presences.of(part_fp)?;
        let ident = p.part.ident();
        crate::inval::record_part_key(rel.rd_id, (ident.dev, ident.ino, ident.len));
        pins.push(PartPin::pin(&p.part));
        parts.push(ScanPartSpec { path: p.path.clone(), expect: p.expect, rows: rec.rows, granules: rec.granule_count, deletes: p.deletes.clone(), present });
    }
    // D839 (trunk 4934e25b379): a composed own view is not the generation its number names, so a resident table keyed
    // by (head, publisher) must not keep it (another session would take it for that generation after the commit)
    let head = (!gen.composed()).then(|| ScanHead { dir, gen: gen.gen, publisher_fxid: gen.publisher_fxid });
    let segments = gen.segments.iter().filter(|r| r.state != SegState::Sealed).map(|r| (r.slot, r.relid, r.part_no)).collect();
    Ok(ResolvedScanPinned { scan: ResolvedScan { schemas, parts, segments, gen: Some(gen) }, head, pins })
}

struct ScanPart {
    /// Held ONLY while this part is the scan's current part (the 100M
    /// lesson: pins are eviction-exempt, so a whole-table eager pin set
    /// drives resident bytes toward on-disk table size and the kernel
    /// OOM-kills the server). Identity was validated at `begin`; `expect` +
    /// `path` re-open on (re)claim through the shared registry (a cache hit
    /// when the entry survived, a fresh identity-checked open otherwise).
    pin: Option<PartPin>,
    path: std::ffi::CString,
    expect: PartExpect,
    rows: u64,
    granules: u32,
    /// DM-2: this part's decoded delete vector (`None` = dv_gen == 0).
    /// The correctness scan skips deleted rows at the slot boundary — the
    /// one read path serves SELECT/COPY TO/ANALYZE Dv-applied.
    deletes: Option<Arc<pgrc_scan::PartDeletes>>,
    /// FASTADD: which of the scan's columns the part holds (`None` = every one).
    present: Option<Arc<[bool]>>,
}

pub(crate) struct ColBuf {
    pub(crate) datums: memledger::ChargedVec<u64>,
    /// u64-backed so the byte view is always 8-aligned (the ByteArena law).
    arena: memledger::ChargedVec<u64>,
    validity: memledger::ChargedVec<u64>,
    all_valid: bool,
    /// C1 L5 (D777's dict region, generalized): what the column's datums alias — a dictionary-coded column's dict
    /// payload region (the pointer-class law; a cached image, or an SB-7 region assembled for the decoding cursor
    /// alone) — held until the column's next decode, so the datums depend neither on the cursor's lifetime
    /// (set_needed_attrs, release_part and reset_position drop the row scan's cursors, and `decode_granule` — the row
    /// cache's and compaction's decode — drops its own when it returns) nor on the cache's (a held image is pinned).
    guard: Guard,
    /// FASTADD: the by-reference fill image the datums address when the column was filled ([`ColBuf::fill_const`]),
    /// held as the guard holds a decoded column's region.
    fill: Option<Arc<[u64]>>,
    /// D3181: ANALYZE's sample of a dictionary-coded varlena column ([`decode_col_dict`]): the part's frame-lazy
    /// dictionary (with the part's index in the scan) its datums alias, held as the guard holds a decoded column's
    /// region; and the granule's codes.
    sample_dict: Option<(usize, DictHandle)>,
    codes: Vec<u32>,
}

impl ColBuf {
    /// Every buffer allocated up front, uncharged (compaction's and the row cache's buffers: R15 merge, trunk's
    /// ChargedVec fields with no owner, as `ColBuf::new` allocated before).
    pub(crate) fn new() -> ColBuf {
        let mut b = ColBuf::unallocated();
        b.ensure_allocated();
        b
    }

    /// No storage until the column is first decoded: a column the executor
    /// never reads costs the scan nothing.
    pub(crate) fn unallocated() -> ColBuf {
        ColBuf::charged(None)
    }

    /// `unallocated`, its buffers charged to `owner` as they are allocated (the scan's statement).
    fn charged(owner: Option<(memledger::Owner, memledger::Kind)>) -> ColBuf {
        let buf = || memledger::ChargedVec::new(owner.map(|o| o.0), owner.map_or(memledger::Kind::Region, |o| o.1));
        ColBuf {
            datums: buf(),
            arena: buf(),
            validity: buf(),
            all_valid: true,
            guard: Guard::new(),
            fill: None,
            sample_dict: None,
            codes: Vec::new(),
        }
    }

    fn ensure_allocated(&mut self) {
        if self.datums.is_empty() {
            self.datums.with_vec(|v| *v = vec![0u64; GRANULE_ROWS as usize]); // alloc: Exec charged self.datums.owner()
            self.arena.with_vec(|v| *v = vec![0u64; ARENA_INITIAL / 8]); // alloc: Exec charged self.arena.owner()
            self.validity.with_vec(|v| *v = vec![0u64; VALIDITY_WORDS]); // alloc: Exec charged self.validity.owner()
        }
    }

    /// Hold what the column's just-decoded datums alias (`StreamCursor::hold_datums`), letting the previous granule's go.
    fn hold_datums(&mut self, cursor: &StreamCursor<'_>) {
        self.guard.release();
        self.fill = None;
        cursor.hold_datums(&mut self.guard);
    }

    /// FASTADD: `rows` rows of a column the part lacks: its fill (the column's missing value, or NULL).
    pub(crate) fn fill_const(&mut self, rows: u32, fill: &crate::fastadd::Fill) {
        self.ensure_allocated();
        self.guard.release();
        self.fill = fill.image().cloned();
        match fill.word() {
            Some(w) => {
                self.datums[..rows as usize].fill(w);
                self.all_valid = true;
            }
            None => {
                self.validity.fill(0);
                self.all_valid = false;
            }
        }
    }

    pub(crate) fn is_valid(&self, row: usize) -> bool {
        self.all_valid || (self.validity[row / 64] >> (row % 64)) & 1 == 1
    }

    /// IX1 L0: the bytes the buffer holds (the column-selective row cache's cap counts them).
    pub(crate) fn mem_bytes(&self) -> usize {
        (self.datums.capacity() + self.arena.capacity() + self.validity.capacity()) * 8 + self.codes.capacity() * 4
    }
}

/// ROWMAP2 M0: whether a scan's part rows need their positions (`tts_tid`), and for which consumer
/// ([`PgrcScanDescData::set_tid_need`]). A part row's position is `tid_of` its row ID, read from the part's
/// `RowIdentMap`; a scan whose need is [`TidNeed::None`] loads no map and leaves its part rows' positions invalid (as
/// for a part without row IDs). Segment rows keep their positions either way (they cost no map).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TidNeed {
    /// The plan proves nothing reads the position: no system-column or whole-row reference of the relation, and the
    /// relation is neither a result relation nor row-marked.
    None = 0,
    /// A scan the executor does not classify (logical apply's search, FK validation, COPY TO, a rewrite): every scan
    /// until told otherwise.
    Direct = 1,
    /// A system-column or whole-row reference of the scanned relation (`SELECT ctid`, `RETURNING ctid`).
    Ctid = 2,
    /// A result relation of UPDATE / DELETE: the row identity the modification fetches by.
    Dml = 3,
    /// A result relation with row-level UPDATE / DELETE triggers or transition tables.
    Trigger = 4,
    /// A locking row mark (FOR UPDATE / NO KEY UPDATE / SHARE / KEY SHARE).
    Lock = 5,
    /// A reference row mark (a joined relation of UPDATE / DELETE): EvalPlanQual refetches the row by its position.
    Epq = 6,
}

mcx::forget_safe_nodrop!(TidNeed);

impl TidNeed {
    /// Whether a scan of this need stamps positions: every need but `None`, except the one class
    /// `ROWMAP2_FORGET_TID=<direct|ctid|dml|trigger|lock|epq>` names (a test knob: that consumer's tests must fail).
    pub fn stamps(self) -> bool {
        let f = forgotten_tid_need();
        self != TidNeed::None && self as u8 != f && !(f == CURSOR_POSITION && self == TidNeed::Ctid)
    }
}

/// `ROWMAP2_FORGET_TID`'s class (0 = none), read once per process: a scan reads it when its need is set, never per row.
fn forgotten_tid_need() -> u8 {
    static FORGET: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(u8::MAX);
    let f = FORGET.load(Ordering::Relaxed);
    if f != u8::MAX {
        return f;
    }
    let class = match std::env::var("ROWMAP2_FORGET_TID").as_deref() {
        Ok("direct") => TidNeed::Direct,
        Ok("ctid") => TidNeed::Ctid,
        Ok("dml") => TidNeed::Dml,
        Ok("trigger") => TidNeed::Trigger,
        Ok("lock") => TidNeed::Lock,
        Ok("epq") => TidNeed::Epq,
        Ok("cursor-position") => return store_forgotten(&FORGET, CURSOR_POSITION),
        _ => TidNeed::None,
    } as u8;
    store_forgotten(&FORGET, class)
}

/// IX1 L0 (OC-C c): `ROWMAP2_FORGET_TID=cursor-position`'s class: [`PgrcScanDescData::current_rowid`] answers nothing,
/// so `WHERE CURRENT OF` over a scan that stamps no positions fails as it did before IX1; a `Ctid` scan stamps none
/// either (PGRCSERVE M3: Gravity's cursor reads its rows' positions as its identity copy's ctid).
const CURSOR_POSITION: u8 = 0xfe;

fn store_forgotten(cell: &std::sync::atomic::AtomicU8, class: u8) -> u8 {
    cell.store(class, Ordering::Relaxed);
    class
}

fn forget_cursor_position() -> bool {
    forgotten_tid_need() == CURSOR_POSITION
}

pub struct PgrcScanDescData<'mcx> {
    pub rs_base: TableScanDescData<'mcx>,
    /// SO_TEMP_SNAPSHOT carrier (parallel worker scans register a restored
    /// snapshot; endscan unregisters it — the old-AM shape).
    pub rs_temp_snapshot: Option<std::rc::Rc<types_snapshot::SnapshotData<'static>>>,
    /// SCANSTATS: this scan's pg_stat counts and read telemetry, folded when it is dropped.
    pub stats: crate::scanstats::ScanTally,
    schemas: Vec<ColSchema>,
    /// The slot's width: past `schemas.len()` when columns were dropped.
    natts: usize,
    parts: Vec<ScanPart>,
    binding: &'static CodecBinding<'static>,
    /// Current position: `cur_part == parts.len()` = exhausted;
    /// `cur_part == usize::MAX` = before first advance.
    cur_part: usize,
    cur_granule: u32,
    rows_in_granule: u32,
    row_cursor: u32,
    /// DM-2: the CURRENT granule's deletion mask (copied out of the
    /// part's `PartDeletes` at decode; `None` = nothing deleted here).
    cur_dv_mask: Option<Box<[u64; pgrc_scan::DV_GRANULE_WORDS]>>,
    cols: Vec<ColBuf>,
    segments: Vec<(u16, types_core::Oid, u32)>,
    /// The view the parts are of (its parts' maps give the rows' IDs, IX0).
    gen: Option<Arc<crate::catalog::Generation>>,
    /// FASTADD: per column, what a part that lacks it reads (empty when every part holds every column).
    fills: Vec<crate::fastadd::Fill>,
    next_segment: usize,
    /// A parallel claim past the parts, kept for the segments so no segment is stranded.
    parked: Option<usize>,
    /// The segment being read, after the parts.
    pub segment: Option<Box<dyn SegmentSource + 'mcx>>,
    /// The snapshot's tombstones (F2), unioned with each granule's DV.
    pub filter: Option<Box<dyn DeletionFilter + 'mcx>>,
    /// PSLR: a SnapshotDirty scan: each part row writes the snapshot's out-fields ([`Self::report_dirty`]).
    dirty: bool,
    /// C1 L5: the cursors' residency ([`read_residency`] over the scan's part files).
    residency: Residency,
    /// D259: the columns (0-based, schema order) the executor reads — the
    /// plan's consumed set via `set_needed_attrs`; every column until told.
    /// Only these decode; the others read NULL in the slot.
    needed: Vec<usize>,
    needed_mask: Vec<bool>,
    /// The current part's decode cursors, one per needed column, opened at
    /// the part's first decoded granule under `residency` and
    /// dropped when the scan leaves the part: what the scan holds is one
    /// extent per needed stream plus the part's dictionaries, never the
    /// relation (the shared residency plane is not grown by a row scan).
    cursors: Vec<Option<StreamCursor<'static>>>,
    /// The current granule was decoded for a smaller column set than
    /// `needed` (the set grew mid-granule): re-decode before the next row.
    redecode: bool,
    /// The unneeded columns' NULLs must be (re)written into the slot: set at
    /// every granule decode and need-set change; `filled_slot` names the
    /// slot that holds them.
    fill_unneeded: bool,
    filled_slot: usize,
    /// IX0 L3: the table directory (the parts' `RowIdentMap`s are read from it), and the current part's map and
    /// grain, `None` for a part without row IDs (written before IX0: its rows' ctids are invalid).
    dir: String,
    map: Option<(usize, Option<Arc<crate::catalog::RowMap>>, u32)>,
    /// ROWMAP2: part rows get positions (and their parts' maps load) only while this holds ([`TidNeed::stamps`]).
    tid: bool,
    /// D4826: a consumer reads the positions it is given (a ctid, a row's identity for DML or a lock), so a part without
    /// row IDs is refused rather than given invalid ones.
    tid_strict: bool,
    /// ROWMAP2: the current map's group or run the last stamp read (`RowMap::ident_from`).
    map_hint: usize,
    /// ROWMAP2 M1: a granule's row IDs, decoded once beside its columns (`RowMap::decode_ids`), and the `(part,
    /// granule)` they are of; a stamp in that granule is then a buffer read.
    ids: Vec<u64>,
    ids_at: Option<(usize, u32)>,
    /// IX1 L0: a TID scan's row-ID bound (the counter's next ID when it began; `table_tuple_tid_valid`); 0 for any
    /// other scan.
    tid_limit: u64,
    /// ZONEPRUNE: the executor's zone quals ([`Self::push_zone_quals`]) on columns with a word zone key; a part
    /// granule whose [min, max] fails one of them is skipped before its decode (no row in it passes the plan's qual).
    zone: Vec<ZoneProbe>,
    /// IX1 L4: a bitmap heap scan's pages ([`Self::begin_bitmap`]); `None` for any other scan.
    pub bitmap: Option<Box<dyn BitmapSource + 'mcx>>,
    /// PGRCSERVE M0 (`table_scan_push_quals`): a reader's exact conjuncts on needed columns whose datum word is the order
    /// key ([`Self::push_quals`]). A part row failing one is never returned, and a granule's other needed columns decode
    /// only for the rows that pass ([`Self::decode_current_granule`]); the reader still evaluates its own qual.
    rowq: Vec<RowProbe>,
    /// The current granule was decoded under `rowq`: `pass` its passing live rows, ascending, `pass_at` the next one
    /// `getnextslot` returns (an empty `pass`: no row of the granule is returned and its other columns stay undecoded).
    probed: bool,
    pass: Vec<u16>,
    pass_at: usize,
    /// D3181: ANALYZE's two-pass sampler ([`Self::live_positions`], [`Self::fetch_sample`]).
    sample: Sample,
    /// D4735: a TABLESAMPLE scan's current virtual block; `None` before the first and after the last.
    tsm_block: Option<u32>,
}

/// D3181: a sample position names a part row as its granule's number in the scan (the parts' granules in part order)
/// times [`GRANULE_ROWS`] plus its row, and the `k`-th live segment row (the segments follow the parts) as
/// `SEGMENT_POSITION | k`.
pub const SEGMENT_POSITION: u64 = 1 << 63;

/// D3181: ANALYZE's two-pass sampler over the scan: the live rows counted from the masks alone, the kept stream
/// ordinals mapped to positions, then only those rows decoded and fetched.
#[derive(Default)]
struct Sample {
    /// The positions [`PgrcScanDescData::fetch_sample`] serves, ascending ([`PgrcScanDescData::set_sample_positions`]);
    /// empty: a granule decodes whole.
    positions: Vec<u64>,
    /// The current granule's share of `positions` (its rows).
    rows: Vec<u16>,
    /// Each part's first granule's number, built at the first need.
    first: Vec<u64>,
    /// Each part's live rows ([`PgrcScanDescData::live_part_rows`]).
    part_live: Vec<u64>,
    /// The live segment rows the count pass read, and the ordinal the fetch pass reads next.
    segment_rows: u64,
    segment_next: u64,
}

/// ZONEPRUNE: one pushed zone qual, resolved against the scan's columns and lowered to the column's key once.
struct ZoneProbe {
    /// The column's index in `schemas`.
    col: usize,
    attno: u32,
    profile: pgrc_meta::profile::MetaProfile,
    cmp: tableam_vocab::ZoneCmp,
    key: pgrc_meta::lower::LoweredConst<'static>,
}

/// PGRCSERVE M0: one pushed row qual, resolved against the scan's columns: `col op val` over the column's datum word read
/// as a signed word, which is the column's order key (`KeyDerivation::SignedWord`: sign-extended integers, date, time,
/// timestamp[tz], money), so the comparison is the type's own.
#[derive(Clone, Copy)]
struct RowProbe {
    /// The column's index in `schemas`.
    col: usize,
    cmp: tableam_vocab::ZoneCmp,
    val: i64,
}

impl RowProbe {
    #[inline]
    fn passes(&self, word: u64) -> bool {
        use tableam_vocab::ZoneCmp;
        let x = word as i64;
        match self.cmp {
            ZoneCmp::Eq => x == self.val,
            ZoneCmp::Ne => x != self.val,
            ZoneCmp::Lt => x < self.val,
            ZoneCmp::Le => x <= self.val,
            ZoneCmp::Gt => x > self.val,
            ZoneCmp::Ge => x >= self.val,
        }
    }
}

// TableScanDesc (and genam's SysScanDescData around it) needs this descriptor covariant in 'mcx.
#[allow(dead_code)]
fn covariant<'a, 'b: 'a>(d: PgrcScanDescData<'b>) -> PgrcScanDescData<'a> {
    d
}

impl<'mcx> PgrcScanDescData<'mcx> {
    /// Open the scan: resolve the snapshot-effective manifest, validate +
    /// record every listed part through the shared registry (identity
    /// checked at open; keys recorded for invalidation), then release the
    /// pins — the scan holds at most ONE part pinned while decoding it.
    pub fn begin(rs_base: TableScanDescData<'mcx>) -> PgResult<Box<PgrcScanDescData<'mcx>>> {
        Self::begin_as(rs_base, None)
    }

    /// [`Self::begin`] with the rows read under `desc` instead of the relation's (ALTER's rewrite: the old descriptor).
    pub fn begin_as(rs_base: TableScanDescData<'mcx>, desc: Option<&types_tuple::tupdesc::TupleDescData<'_>>) -> PgResult<Box<PgrcScanDescData<'mcx>>> {
        let (name, d) = (rs_base.rs_rd.name(), &rs_base.rs_rd.rd_att);
        let (schemas, natts) = match desc {
            Some(d) => (crate::schema::col_schemas_of(name, &d.attrs)?, d.natts as usize),
            None => (crate::schema::col_schemas_of(name, &d.attrs)?, d.natts as usize),
        };
        let resolved = resolve_scan_parts_as(&rs_base.rs_rd, rs_base.rs_snapshot.as_deref(), schemas)?.scan;
        let (schemas, segments, gen) = (resolved.schemas, resolved.segments, resolved.gen);
        let fills = match resolved.parts.iter().any(|p| p.present.is_some()) {
            true => match desc {
                Some(old) => crate::fastadd::fills(old, &schemas)?,
                None => crate::fastadd::fills(d, &schemas)?,
            },
            false => Vec::new(),
        };
        let parts: Vec<ScanPart> = resolved
            .parts
            .into_iter()
            .map(|s| ScanPart { pin: None, path: s.path, expect: s.expect, rows: s.rows, granules: s.granules, deletes: s.deletes, present: s.present })
            .collect();

        let ncols = schemas.len();
        let (rs_base_locator, rs_base_backend) = (rs_base.rs_rd.rd_locator.get(), rs_base.rs_rd.rd_backend);
        let residency = read_residency(parts.iter().map(|p| p.expect.file_len.unwrap_or(0)).sum());
        if residency == Residency::Private {
            crate::rowcache::note_bulk(rs_base.rs_rd.rd_id);
        }
        let mut stats = crate::scanstats::ScanTally::default();
        stats.start(rs_base.rs_flags);
        let dirty = rs_base.rs_snapshot.as_deref().is_some_and(|s| s.snapshot_type == types_snapshot::SnapshotType::SNAPSHOT_DIRTY);
        Ok(Box::new(PgrcScanDescData {
            rs_base,
            rs_temp_snapshot: None,
            stats,
            schemas,
            natts,
            parts,
            binding: codec_binding(),
            cur_part: usize::MAX,
            cur_granule: 0,
            rows_in_granule: 0,
            row_cursor: 0,
            cur_dv_mask: None,
            cols: (0..ncols).map(|_| ColBuf::charged(stmt_owner_of(memledger::charge::context().0))).collect(),
            segments,
            gen,
            fills,
            next_segment: 0,
            parked: None,
            segment: None,
            filter: None,
            dirty,
            residency,
            needed: (0..ncols).collect(),
            needed_mask: crate::alloc::try_filled(ncols, true, "scan")?,
            cursors: (0..ncols).map(|_| None).collect(),
            redecode: false,
            fill_unneeded: true,
            filled_slot: 0,
            dir: crate::dirpath::table_dir_path(rs_base_locator, rs_base_backend),
            map: None,
            tid: TidNeed::Direct.stamps(),
            tid_strict: false,
            map_hint: 0,
            ids: Vec::new(),
            ids_at: None,
            tid_limit: 0,
            zone: Vec::new(),
            bitmap: None,
            rowq: Vec::new(),
            probed: false,
            pass: Vec::new(),
            pass_at: 0,
            sample: Sample::default(),
            tsm_block: None,
        }))
    }

    /// IX1 L0: a TID scan's descriptor: no parts resolved (each TID is fetched by its ID, `ixfetch`), the IDs below
    /// `limit` (the counter's next ID) valid.
    pub fn begin_tid(rs_base: TableScanDescData<'mcx>, limit: u64) -> Box<PgrcScanDescData<'mcx>> {
        Self::by_id(rs_base, limit)
    }

    /// IX1 L4: a bitmap heap scan's descriptor: no parts resolved; `source` stages each bitmap page's rows by their IDs
    /// and stores them (`table_scan_bitmap_next_tuple` and the page-batch pair).
    pub fn begin_bitmap(rs_base: TableScanDescData<'mcx>, source: Box<dyn BitmapSource + 'mcx>) -> Box<PgrcScanDescData<'mcx>> {
        let mut d = Self::by_id(rs_base, 0);
        d.bitmap = Some(source);
        d
    }

    /// D4735: a TABLESAMPLE scan's descriptor, heap's page sampling over the virtual blocks of the IDs below `bound`;
    /// `source` stages each offset the method picks by its ID.
    pub fn begin_sample(rs_base: TableScanDescData<'mcx>, source: Box<dyn BitmapSource + 'mcx>, bound: u64) -> Box<PgrcScanDescData<'mcx>> {
        let mut d = Self::by_id(rs_base, bound);
        d.bitmap = Some(source);
        d
    }

    /// D4735: heapam's `heap_scan_sample_next_block`: the method's next block, or every block in order; false at the end.
    pub fn sample_next_block(&mut self, driver: &mut dyn tableam_vocab::SampleScanDriver, donetuples: i64) -> PgResult<bool> {
        let invalid = types_core::InvalidBlockNumber;
        let nblocks = self.tid_limit.div_ceil(pgrc_format::rowid::IDS_PER_BLOCK).min(invalid as u64 - 1) as u32;
        if nblocks == 0 {
            return Ok(false);
        }
        let block = if driver.has_next_sample_block() {
            driver.next_sample_block(nblocks, donetuples)?
        } else {
            match self.tsm_block {
                None => 0,
                Some(b) if b + 1 < nblocks => b + 1,
                Some(_) => invalid,
            }
        };
        self.tsm_block = (block != invalid).then_some(block);
        check_for_interrupts()?;
        Ok(block != invalid)
    }

    /// D4735: heapam's `heap_scan_sample_next_tuple`: the next row of the current block the method keeps; an offset whose
    /// ID holds no row this snapshot sees is skipped. False when the block has no more.
    pub fn sample_next_tuple<'s>(
        &mut self,
        mcx: mcx::Mcx<'s>,
        driver: &mut dyn tableam_vocab::SampleScanDriver,
        donetuples: i64,
        slot: &mut SlotData<'s>,
    ) -> PgResult<bool> {
        let (Some(block), Some(source)) = (self.tsm_block, self.bitmap.as_deref_mut()) else {
            slot.base_mut().mark_empty();
            return Ok(false);
        };
        let per = pgrc_format::rowid::IDS_PER_BLOCK;
        let maxoffset = self.tid_limit.saturating_sub(block as u64 * per).min(per) as u16;
        loop {
            check_for_interrupts()?;
            let offset = driver.next_sample_tuple(block, maxoffset, donetuples);
            if offset == types_tuple::itemptr::InvalidOffsetNumber {
                slot.base_mut().mark_empty();
                return Ok(false);
            }
            if source.stage(block, Some(&[offset]))? > 0 {
                source.store(mcx, 0, slot)?;
                return Ok(true);
            }
        }
    }

    /// A descriptor that reads rows by ID only (a TID scan's, a bitmap scan's): no parts, no segments, no tally.
    fn by_id(rs_base: TableScanDescData<'mcx>, limit: u64) -> Box<PgrcScanDescData<'mcx>> {
        let natts = rs_base.rs_rd.rd_att.natts as usize;
        let dir = crate::dirpath::table_dir_path(rs_base.rs_rd.rd_locator.get(), rs_base.rs_rd.rd_backend);
        Box::new(PgrcScanDescData {
            rs_base,
            rs_temp_snapshot: None,
            // SCANSTATS: a TID or bitmap scan is no seq_scan (heapam counts SO_TYPE_SEQSCAN scans only), so its tally never
            // starts
            stats: crate::scanstats::ScanTally::default(),
            schemas: Vec::new(),
            natts,
            parts: Vec::new(),
            binding: codec_binding(),
            cur_part: usize::MAX,
            cur_granule: 0,
            rows_in_granule: 0,
            row_cursor: 0,
            cur_dv_mask: None,
            cols: Vec::new(),
            segments: Vec::new(),
            gen: None,
            fills: Vec::new(),
            next_segment: 0,
            parked: None,
            segment: None,
            filter: None,
            dirty: false,
            residency: Residency::Cached,
            needed: Vec::new(),
            needed_mask: Vec::new(),
            cursors: Vec::new(),
            redecode: false,
            fill_unneeded: true,
            filled_slot: 0,
            dir,
            map: None,
            tid: true,
            tid_strict: false,
            map_hint: 0,
            ids: Vec::new(),
            ids_at: None,
            tid_limit: limit,
            zone: Vec::new(),
            bitmap: None,
            rowq: Vec::new(),
            probed: false,
            pass: Vec::new(),
            pass_at: 0,
            sample: Sample::default(),
            tsm_block: None,
        })
    }

    /// IX1 L0: `table_tuple_tid_valid` of a TID scan: a TID that names an ID ([`pgrc_format::rowid::id_of`]) the
    /// counter handed out before the scan began (heap's `ItemPointerIsValid` and block below its block count).
    pub fn tid_valid(&self, block: u32, off: u16) -> bool {
        pgrc_format::rowid::id_of(block, off).is_some_and(|id| id >= pgrc_format::rowid::ROWID_FIRST && id < self.tid_limit)
    }

    /// IX1 L0 (OC-C c, main 2026-10-06): the ID of the part row this scan last returned (`getnextslot` stores row
    /// `row_cursor - 1` of the current granule and returns), through its part's map: `WHERE CURRENT OF` over a scan
    /// that stamps no positions ([`TidNeed::None`]) resolves that one row instead of every row loading its part's
    /// map. The caller asks only for a slot the scan filled whose row carries no position (a segment row always
    /// does). `None`: no part row is current (before the first, past the parts), the part has no row IDs, or
    /// `ROWMAP2_FORGET_TID=cursor-position` (the test knob).
    pub fn current_rowid(&self) -> PgResult<Option<u64>> {
        if forget_cursor_position() || self.row_cursor == 0 || self.cur_part >= self.parts.len() {
            return Ok(None);
        }
        let Some(gen) = self.gen.as_ref() else { return Ok(None) };
        let i = self.cur_part;
        let rec = &gen.manifest.parts[i];
        if gen.ids_of(i).is_empty() && rec.flags & pgrc_format::manifest::PART_FLAG_ROWMAP == 0 {
            return Ok(None);
        }
        let grain = if rec.granule_rows == 0 { pgrc_format::geom::GRANULE_ROWS } else { rec.granule_rows };
        Ok(Some(gen.row_map(i, &self.dir)?.ident(self.cur_granule * grain + self.row_cursor - 1)))
    }

    /// The generation this scan reads (the view its positions name); `None` = no committed publish.
    pub fn generation(&self) -> Option<&Arc<crate::catalog::Generation>> {
        self.gen.as_ref()
    }

    /// Live segments read after the view's (design §4.8: newer ones, which may hold the transaction's own writes).
    pub fn add_segments(&mut self, segs: impl IntoIterator<Item = (u16, types_core::Oid, u32)>) {
        self.segments.extend(segs);
    }

    /// The executor's column projection (`table_scan_set_needed_attrs`): only
    /// the attributes flagged here decode; the rest read NULL. `needed` is
    /// indexed by attribute number - 1 (a column's `attno`, past dropped ones); a short or long slice is read as
    /// "every attribute past its end is needed".
    pub fn set_needed_attrs(&mut self, needed: &[bool]) {
        let ncols = self.schemas.len();
        let mask: Vec<bool> = (0..ncols).map(|c| needed.get(self.schemas[c].attno as usize - 1).copied().unwrap_or(true)).collect();
        if mask == self.needed_mask {
            return;
        }
        // A column now needed that the current granule did not decode: the
        // granule is decoded again before its next row is stored.
        let grew = mask.iter().zip(&self.needed_mask).any(|(&now, &was)| now && !was);
        if grew && self.cur_part < self.parts.len() && self.rows_in_granule > 0 {
            self.redecode = true;
        }
        for (c, &n) in mask.iter().enumerate() {
            if !n {
                self.cursors[c] = None;
            }
        }
        // PGRCSERVE M0: a pushed row qual reads a decoded column only
        self.rowq.retain(|p| mask[p.col]);
        self.needed = (0..ncols).filter(|&c| mask[c]).collect();
        self.needed_mask = mask;
        self.fill_unneeded = true;
    }

    /// ROWMAP2 M0: the executor's verdict on who reads this scan's positions (`table_scan_set_tid_need`); a scan
    /// never told stamps them ([`TidNeed::Direct`]).
    pub fn set_tid_need(&mut self, need: TidNeed) {
        self.tid = need.stamps();
        self.tid_strict = self.tid && need != TidNeed::Direct;
    }

    /// ZONEPRUNE (`table_scan_push_zone_quals`): the executor's zone quals for the scan's next pass, replacing any
    /// earlier ones (a rescan pushes its Params' new values). Each is a conjunct of the plan's qual, `attnum op val`
    /// with `val` the constant's datum word (pgrc_zonequal's key); kept only on a column whose zone key is the datum
    /// word itself (signed or small unsigned word, bool) and only when the word lowers exactly. Advisory: dropping
    /// one only loses a skip, never a row.
    pub fn push_zone_quals(&mut self, quals: &[tableam_vocab::ZoneQual]) {
        use pgrc_meta::key::KeyDerivation;
        use tableam_vocab::ZoneCmp;
        self.zone.clear();
        for q in quals {
            let Some(col) = self.schemas.iter().position(|s| s.attno == u32::from(q.attnum)) else { continue };
            let s = &self.schemas[col];
            let Ok(profile) = pgrc_meta::profile::MetaProfile::derive(s.class, s.collation_class, s.semantics) else { continue };
            let word = match profile.key {
                KeyDerivation::SignedWord | KeyDerivation::Bool => q.val as u64,
                // an oid's word: the small unsigned key takes only what its width holds
                KeyDerivation::UnsignedWordSmall if (0..=i64::from(u32::MAX)).contains(&q.val) => q.val as u64,
                _ => continue,
            };
            if q.op == ZoneCmp::Ne {
                continue;
            }
            let Some(key) = pgrc_meta::lower::lower_const(&profile, pgrc_meta::lower::ConstInput::Word(word)).lowered() else { continue };
            self.zone.push(ZoneProbe { col, attno: s.attno, profile, cmp: q.op, key });
        }
    }

    /// PGRCSERVE M0 (`table_scan_push_quals`): a reader's exact conjuncts for the scan's next pass, replacing any earlier
    /// ones (a rescan drops them, as it drops the zone quals). Each is `attnum op val` with `val` the constant's datum word
    /// (the zone quals' vocabulary); kept only on a needed column whose datum word is its order key (a signed word), where
    /// the word comparison is the type's. A part row failing one is skipped before its slot is filled, and a granule's
    /// other needed columns decode only for the rows that pass. The segments' rows are returned whole: the reader
    /// filters every row it gets.
    pub fn push_quals(&mut self, quals: &[tableam_vocab::ZoneQual]) {
        use pgrc_meta::key::KeyDerivation;
        self.rowq.clear();
        for q in quals {
            let Some(col) = self.schemas.iter().position(|s| s.attno == u32::from(q.attnum)) else { continue };
            if !self.needed_mask.get(col).copied().unwrap_or(false) {
                continue;
            }
            let s = &self.schemas[col];
            let Ok(profile) = pgrc_meta::profile::MetaProfile::derive(s.class, s.collation_class, s.semantics) else { continue };
            if profile.key != KeyDerivation::SignedWord {
                continue;
            }
            self.rowq.push(RowProbe { col, cmp: q.op, val: q.val }); // alloc: Cold (once per pass, one per qual)
        }
    }

    /// ZONEPRUNE: whether the current part granule's zone fails a pushed qual. A column's zone records are the part's
    /// own, decoded once per part and shared by every scan of it (F2: `pgrc_scan::meta::zone_records`), so the check
    /// is a lookup: no per-scan section load, lock or copy. A part without the column's stats, or without the column
    /// (FASTADD), decides nothing; an unreadable section decides nothing either (the skip is advisory).
    #[inline(never)]
    fn zone_prunes(&mut self) -> PgResult<bool> {
        use pgrc_meta::verdict::{evaluate, GrainFacts, ZonePredicate};
        use tableam_vocab::ZoneCmp;
        let (i, g) = (self.cur_part, self.cur_granule);
        self.pin(i)?;
        let p = &self.parts[i];
        let part = p.pin.as_ref().expect("pinned").part();
        let present = p.present.as_deref();
        let rows = u64::from(pgrc_format::geom::rows_in_granule_at(part.rows(), part.grain(), g));
        for z in &self.zone {
            if present.is_some_and(|m| !m[z.col]) {
                continue;
            }
            let Ok(Some(recs)) = pgrc_scan::meta::zone_records(part, z.attno) else { continue };
            let Some(Some(rec)) = recs.get(g as usize) else { continue };
            let probe = match z.cmp {
                ZoneCmp::Eq => ZonePredicate::Eq(z.key),
                ZoneCmp::Lt => ZonePredicate::Lt(z.key),
                ZoneCmp::Le => ZonePredicate::Le(z.key),
                ZoneCmp::Gt => ZonePredicate::Gt(z.key),
                ZoneCmp::Ge => ZonePredicate::Ge(z.key),
                ZoneCmp::Ne => continue,
            };
            if matches!(evaluate(&z.profile, GrainFacts { rows }, rec, &probe, None), pgrc_format::meta::Verdict::AllFail) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Restart (rescan). Parallel claim state is reset by
    /// `parallelscan_reinitialize`, not here. Pins release here and
    /// re-acquire lazily on the next claim.
    pub fn reset_position(&mut self) {
        self.stats_stop();
        self.stats.start(self.rs_base.rs_flags);
        let mut released = false;
        for p in &mut self.parts {
            released |= p.pin.take().is_some();
        }
        if released {
            crate::inval::registry().maintain();
        }
        self.cursors.iter_mut().for_each(|c| *c = None);
        self.cur_part = usize::MAX;
        self.cur_granule = 0;
        self.rows_in_granule = 0;
        self.row_cursor = 0;
        self.cur_dv_mask = None;
        self.redecode = false;
        self.next_segment = 0;
        self.parked = None;
        // ZONEPRUNE: a rescan's quals are the executor's to push again (its Params may have changed): none survive
        self.zone.clear();
        // PGRCSERVE M0: nor do the pushed row quals
        self.rowq.clear();
        (self.probed, self.pass_at) = (false, 0);
        self.pass.clear();
        self.tsm_block = None;
    }

    /// The next segment to read once the parts are done: in a parallel scan, the claims past the parts.
    pub fn claim_segment(&mut self) -> Option<(u16, types_core::Oid, u32)> {
        let idx = match self.rs_base.rs_parallel {
            Some(pscan) => match self.parked.take() {
                Some(i) => i,
                // SAFETY: as in `claim_next_part`.
                None => unsafe { pscan.as_ref().phs_nallocated.fetch_add(1, Ordering::SeqCst) as usize - self.parts.len() },
            },
            None => {
                self.next_segment += 1;
                self.next_segment - 1
            }
        };
        self.segments.get(idx).copied()
    }

    /// Pin part `idx` (cache hit when the entry survived the budget; a
    /// fresh identity-checked open otherwise) and hand back its OpenPart.
    fn ensure_pinned(&mut self, idx: usize) -> PgResult<Arc<OpenPart>> {
        self.pin(idx)?;
        Ok(Arc::clone(
            self.parts[idx].pin.as_ref().expect("just pinned").part(),
        ))
    }

    /// Pin part `idx` (its first use in the scan); [`Self::ensure_pinned`] less the `Arc` (ZONEPRUNE F2: the zone
    /// check borrows the pinned part instead, no refcount traffic on a part every session's scan shares).
    fn pin(&mut self, idx: usize) -> PgResult<()> {
        if self.parts[idx].pin.is_none() {
            let p = &self.parts[idx];
            let open_io = || -> pgrc_read::ReadResult<Box<dyn pgrc_read::PartIo>> { Ok(Box::new(VfsPartIo::open(&p.path)?)) };
            let pin = crate::inval::registry().open_pinned(&p.expect, open_io).map_err(read_error)?;
            self.parts[idx].pin = Some(pin);
        }
        Ok(())
    }

    /// Release part `idx`'s hold and let the janitor reclaim finished
    /// residents down to the budget (the scan is forward-only per claim).
    /// The part's cursors go, and with them what only they held.
    fn release_part(&mut self, idx: usize) {
        self.cursors.iter_mut().for_each(|c| *c = None);
        if idx < self.parts.len() && self.parts[idx].pin.take().is_some() {
            crate::inval::registry().maintain();
        }
    }

    fn claim_next_part(&mut self) -> Option<usize> {
        if let Some(pscan) = self.rs_base.rs_parallel {
            // Part-grain parallel claims over the shared cursor.
            // SAFETY: the parallel descriptor outlives every worker scan
            // (the tableam parallel contract); we only touch the atomic.
            let idx = unsafe {
                pscan
                    .as_ref()
                    .phs_nallocated
                    .fetch_add(1, Ordering::SeqCst)
            } as usize;
            if idx < self.parts.len() {
                Some(idx)
            } else {
                self.parked = Some(idx - self.parts.len());
                None
            }
        } else {
            let next = if self.cur_part == usize::MAX {
                0
            } else {
                self.cur_part + 1
            };
            if next < self.parts.len() {
                Some(next)
            } else {
                None
            }
        }
    }

    /// Advance to the next granule (possibly claiming the next part);
    /// false = scan exhausted. Never inlined: it runs once per granule, and inlined into the per-row `getnextslot` its
    /// setup was hoisted into every call (SCANSTATS: small-R -2.2% instructions out of line).
    #[inline(never)]
    fn next_granule(&mut self) -> PgResult<bool> {
        self.offer_current();
        loop {
            check_for_interrupts()?;
            if self.cur_part != usize::MAX
                && self.cur_part < self.parts.len()
                && self.cur_granule + 1 < self.parts[self.cur_part].granules
            {
                self.cur_granule += 1;
            } else {
                match self.claim_next_part() {
                    Some(idx) => {
                        let prev = self.cur_part;
                        if prev != usize::MAX && prev < self.parts.len() && prev != idx {
                            self.release_part(prev);
                        }
                        self.cur_part = idx;
                        self.cur_granule = 0;
                        if self.parts[idx].granules == 0 {
                            continue;
                        }
                    }
                    None => {
                        let prev = self.cur_part;
                        if prev != usize::MAX && prev < self.parts.len() {
                            self.release_part(prev);
                        }
                        self.cur_part = self.parts.len();
                        return Ok(false);
                    }
                }
            }
            // ZONEPRUNE: a granule no row of which passes the plan's qual is skipped undecoded
            if !self.zone.is_empty() && self.zone_prunes()? {
                self.stats.pruned();
                self.rows_in_granule = 0;
                self.row_cursor = 0;
                self.cur_dv_mask = None;
                continue;
            }
            self.decode_current_granule()?;
            self.stats.granule(self.rows_in_granule, self.cur_dv_mask.as_deref());
            // PGRCSERVE M0: a granule decoded under pushed row quals with no passing row is read and left
            if self.rows_in_granule > 0 && !(self.probed && self.pass.is_empty()) {
                (self.row_cursor, self.pass_at) = (0, 0);
                return Ok(true);
            }
        }
    }

    /// F2-2: an armed relation's decoded granule goes to the row cache as the scan leaves it (a buffer swap).
    fn offer_current(&mut self) {
        if self.rows_in_granule == 0 || self.cur_part >= self.parts.len() {
            return;
        }
        // D3181: a sample's granule decoded only its sampled rows
        if !self.sample.positions.is_empty() {
            return;
        }
        // D259: a projected granule (not every column decoded) is not offered: the cache serves whole rows, and a
        // refetch decodes its granule itself; PGRCSERVE M0: nor one decoded under pushed row quals (its other columns
        // decoded for the passing rows alone)
        if self.needed.len() != self.schemas.len() || self.probed {
            return;
        }
        let Some(gen) = &self.gen else { return };
        let key = crate::rowcache::GranuleKey { gen: gen.view_key(), part: self.cur_part as u32, granule: self.cur_granule };
        crate::rowcache::offer(self.rs_base.rs_rd.rd_id, key, self.rows_in_granule, &mut self.cols);
        self.rows_in_granule = 0;
    }

    /// Decode the current granule's NEEDED columns through the part's
    /// cursors (opened on the part's first granule). The row count is
    /// the part's closed form, so a scan that needs no column (`count(*)`)
    /// reads nothing but the delete vector.
    fn decode_current_granule(&mut self) -> PgResult<()> {
        if self.tid {
            self.load_map()?;
        }
        let part: Arc<OpenPart> = self.ensure_pinned(self.cur_part)?;
        let g = self.cur_granule;
        let rows = pgrc_format::geom::rows_in_granule_at(part.rows(), part.grain(), g);
        let present = self.parts[self.cur_part].present.clone();
        // D3181: ANALYZE's sample decodes only the granule's sampled rows
        let sel = !self.sample.positions.is_empty();
        if sel {
            let first = sample_position(self.sample.first[self.cur_part] + g as u64, 0);
            let sp = &self.sample.positions;
            let (lo, hi) = (sp.partition_point(|&q| q < first), sp.partition_point(|&q| q < first + rows as u64));
            self.sample.rows.clear();
            self.sample.rows.extend(sp[lo..hi].iter().map(|&q| (q - first) as u16));
        }
        // PGRCSERVE M0: under pushed row quals the probes' columns decode first (phase 0); the granule's passing live rows
        // are found on them, and the other needed columns decode for those rows alone (phase 1: through the survivor-only
        // face while they are under half the granule), none when no row passes
        let probing = !sel && !self.rowq.is_empty();
        (self.probed, self.pass_at) = (probing, 0);
        self.pass.clear();
        if probing {
            self.cur_dv_mask = self.granule_deletions(self.cur_part, g);
        }
        for phase in 0..1 + usize::from(probing) {
            if phase == 1 {
                self.find_passing(rows);
                if self.pass.is_empty() {
                    break;
                }
            }
            let only = phase == 1 && self.pass.len() * 2 < rows as usize;
            for k in 0..self.needed.len() {
                check_for_interrupts()?;
                let c = self.needed[k];
                if probing && (phase == 0) != self.rowq.iter().any(|p| p.col == c) {
                    continue;
                }
                let attno = self.schemas[c].attno;
                if present.as_ref().is_some_and(|p| !p[c]) {
                    self.cols[c].fill_const(rows, &self.fills[c]);
                    continue;
                }
                if self.cursors[c].is_none() {
                    let cursor = StreamCursor::open(Arc::clone(&part), self.binding, attno, 0)
                        .map_err(read_error)?
                        .with_residency(self.residency);
                    self.cursors[c] = Some(cursor);
                }
                let cursor = self.cursors[c].as_mut().expect("just opened");
                if only {
                    decode_sample_rows(cursor, &mut self.cols[c], g, &self.pass, attno)?;
                    continue;
                }
                if sel {
                    // a dictionary-coded varlena column faults only the dictionary frames its sampled codes name
                    let key = cursor.key();
                    if key.encoding == EncodingId::DictCodes.as_u16()
                        && key.class == CLASS_VARLENA
                        && decode_col_dict(cursor, &mut self.cols[c], (self.cur_part, &part), self.binding, g, rows, &self.sample.rows, attno)?
                    {
                        continue;
                    }
                    decode_sample_rows(cursor, &mut self.cols[c], g, &self.sample.rows, attno)?;
                    continue;
                }
                let buf = &mut self.cols[c];
                buf.ensure_allocated();
                // decode_full with arena growth on typed exhaustion.
                loop {
                    let arena_words = buf.arena.len();
                    let (datums, arena) = (&mut buf.datums, &mut buf.arena);
                    let arena_bytes = unsafe {
                        // SAFETY: see ColBuf::arena_bytes (split borrows force
                        // the inline form here).
                        core::slice::from_raw_parts_mut(
                            arena.as_mut_ptr() as *mut u8,
                            arena.len() * 8,
                        )
                    };
                    let mut out = DecodeOut {
                        datums: &mut datums[..rows as usize],
                        arena: ByteArena::new(arena_bytes),
                    };
                    match cursor.decode_full(g, &mut out) {
                        // Length-prefix trust: `decode_full` writes exactly the
                        // granule's value-count (an untrusted per-granule gcount
                        // from the part file) datums and returns that count, but we
                        // sized `out.datums` by — and `store_row` later publishes —
                        // `rows` (the closed-form footer geometry). If the kernel
                        // wrote fewer than `rows`, slots [n..rows) were never
                        // written this granule and hold zero/stale/dangling datums
                        // from the reused ColBuf. Bind the served row count to the
                        // written witness and refuse typed on any undercount rather
                        // than publish unwritten memory as valid rows.
                        Ok(n) => {
                            check_decoded_count(attno, g, n, rows)?;
                            break;
                        }
                        // D2582: the arena grows while the granule's images do not fit, fallibly
                        Err(ReadError::Format(pgrc_format::FormatError::ArenaExhausted { .. })) => {
                            if !grow_arena(&mut buf.arena, arena_words * 2) {
                                return Err(read_error(ReadError::Io { at: "granule arena", errno: libc::ENOMEM }));
                            }
                        }
                        Err(e) => return Err(read_error(e)),
                    }
                }
                buf.hold_datums(cursor);
                match cursor.validity(g, &mut buf.validity).map_err(read_error)? {
                    ValidityVerdict::AllValid => buf.all_valid = true,
                    ValidityVerdict::Mixed { .. } => buf.all_valid = false,
                }
            }
        }
        self.rows_in_granule = rows;
        self.redecode = false;
        self.fill_unneeded = true;
        // ROWMAP2 M1: the granule's row IDs, one pass over the map's encoding (runs or delta frames; an implicit
        // map's stamp stays an add)
        self.ids_at = None;
        if self.tid {
            if let Some((i, Some(map), grain)) = &self.map {
                if *i == self.cur_part && !matches!(**map, crate::catalog::RowMap::Implicit(_)) {
                    let n = rows as usize;
                    if self.ids.len() < n {
                        crate::alloc::try_resize(&mut self.ids, n, 0, "scan")?; // alloc: Cold (once per scan, to the grain)
                    }
                    map.decode_ids(g * grain, &mut self.ids[..n]);
                    self.ids_at = Some((self.cur_part, g));
                }
            }
        }
        // DM-2: stage the granule's deletion mask beside the decoded
        // columns (a 1 KiB copy at granule grain — never per row); a probed granule staged it first.
        if !probing {
            self.cur_dv_mask = self.granule_deletions(self.cur_part, g);
        }
        Ok(())
    }

    /// PGRCSERVE M0: the current granule's live rows that pass every pushed row qual, read on the probes' decoded columns
    /// (a NULL passes no comparison), into `pass`.
    fn find_passing(&mut self, rows: u32) {
        self.pass.clear();
        let (rowq, cols, mask) = (&self.rowq, &self.cols, self.cur_dv_mask.as_deref());
        'row: for r in 0..rows as usize {
            if mask.is_some_and(|m| pgrc_scan::PartDeletes::is_deleted(m, r as u32)) {
                continue;
            }
            for p in rowq {
                let b = &cols[p.col];
                if !b.is_valid(r) || !p.passes(b.datums[r]) {
                    continue 'row;
                }
            }
            self.pass.push(r as u16); // alloc: Cold (to the grain once, then reused)
        }
    }

    /// Granule `g` of part `p`'s deletions as the scan applies them: its delete vector, OR'd with the snapshot's
    /// tombstones (F2); `None` when it has neither.
    fn granule_deletions(&self, p: usize, g: u32) -> Option<Box<[u64; pgrc_scan::DV_GRANULE_WORDS]>> {
        let mut mask = self.parts[p].deletes.as_ref().and_then(|d| d.granule_mask(g)).map(|m| Box::new(*m));
        if let Some(f) = &self.filter {
            f.granule_into(p, g, &mut mask);
        }
        mask
    }

    /// Fill `slot` with the next row; false = exhausted (slot marked
    /// empty). DM-2: deleted rows never surface (the one read path — the
    /// slot boundary is this scan's verdict layer).
    pub fn getnextslot(&mut self, slot: &mut SlotData<'_>) -> PgResult<bool> {
        if self.redecode {
            self.decode_current_granule()?;
            // PGRCSERVE M0: the passing rows past the ones already returned
            let at = self.row_cursor;
            self.pass_at = self.pass.partition_point(|&r| u32::from(r) < at);
        }
        loop {
            // PGRCSERVE M0: a granule decoded under pushed row quals returns its passing rows alone
            if self.probed {
                if let Some(&r) = self.pass.get(self.pass_at) {
                    self.pass_at += 1;
                    self.row_cursor = u32::from(r) + 1;
                    self.store_row(r as usize, slot);
                    if self.dirty {
                        self.report_dirty(r as u32);
                    }
                    return Ok(true);
                }
                self.row_cursor = self.rows_in_granule;
            }
            if self.row_cursor < self.rows_in_granule {
                let row = self.row_cursor as usize;
                self.row_cursor += 1;
                if let Some(mask) = &self.cur_dv_mask {
                    if pgrc_scan::PartDeletes::is_deleted(mask, row as u32) {
                        continue;
                    }
                }
                self.store_row(row, slot);
                if self.dirty {
                    self.report_dirty(row as u32);
                }
                return Ok(true);
            }
            if self.cur_part == self.parts.len() && self.cur_part != usize::MAX {
                slot.base_mut().mark_empty();
                return Ok(false);
            }
            if !self.next_granule()? {
                slot.base_mut().mark_empty();
                return Ok(false);
            }
        }
    }

    /// SnapshotDirty's out-fields for part row `row` of the current granule, as `HeapTupleSatisfiesDirty` writes them
    /// for each tuple: no inserter in progress (the parts are the newest committed generation's, or the reader's own),
    /// and as the deleter the in-progress tombstone the filter kept the row visible for.
    #[cold]
    fn report_dirty(&self, row: u32) {
        let Some(s) = self.rs_base.rs_snapshot.as_deref() else { return };
        let busy = self.filter.as_ref().and_then(|f| f.busy_part(self.cur_part, self.cur_granule, row));
        s.dirty_xmin.set(types_core::InvalidTransactionId);
        s.dirty_xmax.set(busy.unwrap_or(types_core::InvalidTransactionId));
        s.dirty_speculative_token.set(0);
    }

    /// IX0 L3: the current part's `RowIdentMap` (the scan stamps each row's ID), loaded once per part (ROWMAP2: only by a
    /// scan that stamps positions). A part
    /// without row IDs (no `EXT_RANGES` entry and no authoritative map: written before IX0) has none.
    fn load_map(&mut self) -> PgResult<()> {
        let i = self.cur_part;
        if self.map.as_ref().is_some_and(|m| m.0 == i) {
            return Ok(());
        }
        let Some(gen) = self.gen.as_ref() else { return Ok(()) };
        let rec = &gen.manifest.parts[i];
        let grain = if rec.granule_rows == 0 { pgrc_format::geom::GRANULE_ROWS } else { rec.granule_rows };
        let ided = !gen.ids_of(i).is_empty() || rec.flags & pgrc_format::manifest::PART_FLAG_ROWMAP != 0;
        let map = if ided {
            Some(gen.row_map(i, &self.dir)?)
        } else if self.tid_strict {
            return Err(crate::no_row_ids(&self.rs_base.rs_rd.name()));
        } else {
            None
        };
        self.map = Some((i, map, grain));
        self.map_hint = 0;
        Ok(())
    }

    /// The row and its position (IX0 L3): `tid_of` its row ID, from the part's map (an add, an array read, a run
    /// search, a lane read); invalid for a part without IDs, and (ROWMAP2) for a scan that stamps no positions. D259: only the needed columns are stored; the unneeded
    /// ones (and the dropped ones past `schemas`) are NULL, written once per granule and slot.
    fn store_row(&mut self, row: usize, slot: &mut SlotData<'_>) {
        let base = slot.base_mut();
        let slot_id = base as *const _ as usize;
        if self.fill_unneeded || slot_id != self.filled_slot {
            if self.natts != self.schemas.len() {
                base.tts_isnull[..self.natts].fill(true);
                base.tts_values[..self.natts].fill(datum::Datum::null());
            }
            for (c, &n) in self.needed_mask.iter().enumerate() {
                if !n {
                    let a = self.schemas[c].attno as usize - 1;
                    base.tts_isnull[a] = true;
                    base.tts_values[a] = datum::Datum::null();
                }
            }
            self.fill_unneeded = false;
            self.filled_slot = slot_id;
        }
        for &c in &self.needed {
            let buf = &self.cols[c];
            let a = self.schemas[c].attno as usize - 1;
            if buf.is_valid(row) {
                base.tts_isnull[a] = false;
                base.tts_values[a] = datum::Datum::from_u64(buf.datums[row]);
            } else {
                base.tts_isnull[a] = true;
                base.tts_values[a] = datum::Datum::null();
            }
        }
        base.tts_nvalid = self.natts as types_core::AttrNumber;
        base.mark_not_empty();
        match &self.map {
            Some((i, Some(map), grain)) if self.tid && *i == self.cur_part => {
                // ROWMAP2 M1: the granule's decoded IDs when they are buffered (one test; a scan without positions
                // pays none of it), else the map
                let id = if self.ids_at == Some((self.cur_part, self.cur_granule)) {
                    self.ids[row]
                } else {
                    map.ident_from(self.cur_granule * grain + row as u32, &mut self.map_hint)
                };
                let (block, off) = pgrc_format::rowid::tid_of(id);
                types_tuple::ItemPointerSet(&mut base.tts_tid, block, off);
            }
            _ => types_tuple::ItemPointerSetInvalid(&mut base.tts_tid),
        }
        base.tts_tableOid = self.rs_base.rs_rd.rd_id;
    }

    /// SCANSTATS: a stop inside a part granule gives back the live rows it credited and never returned.
    fn stats_stop(&mut self) {
        if self.cur_part < self.parts.len() {
            self.stats.stop(self.rows_in_granule, self.row_cursor, self.cur_dv_mask.as_deref());
        }
    }

    /// Total LIVE rows across the scan's parts (COPY TO progress) — manifest facts minus Dv deletions, no pin needed.
    /// ANALYZE counts with the tombstones and the segments too ([`Self::live_part_rows`], `pgrc_front::sample`).
    pub fn total_rows(&self) -> u64 {
        self.parts
            .iter()
            .map(|p| {
                p.rows.saturating_sub(
                    p.deletes.as_ref().map(|d| d.deleted_rows).unwrap_or(0),
                )
            })
            .sum()
    }

    /// D3181: part `p`'s rows and granule grain, the view's open part's (no pin: the generation holds it open).
    fn part_geometry(&self, p: usize) -> (u64, GranuleGrain) {
        let part = &self.gen.as_ref().expect("a scan with parts has its view").parts[p].part;
        (part.rows(), part.grain())
    }

    /// D3181: each part's first granule's number in the scan (the sample positions' granule numbering).
    fn sample_first(&mut self) {
        if self.sample.first.len() != self.parts.len() {
            let mut at = 0u64;
            self.sample.first = self
                .parts
                .iter()
                .map(|p| {
                    let f = at;
                    at += p.granules as u64;
                    f
                })
                .collect();
        }
    }

    /// D3181: ANALYZE's live part rows, the sampler's totalrows less the segments' (`pgrc_front::sample` counts those):
    /// each part's rows less its deletions as the scan applies them, its delete vector and, with tombstones, every
    /// granule's union with them, counted from the masks alone (nothing decoded). Kept per part for
    /// [`Self::live_positions`].
    pub fn live_part_rows(&mut self) -> PgResult<u64> {
        let mut live = crate::alloc::try_vec(self.parts.len(), "scan")?;
        for p in 0..self.parts.len() {
            check_for_interrupts()?;
            let n = match self.filter {
                None => self.parts[p].rows.saturating_sub(self.parts[p].deletes.as_ref().map_or(0, |d| d.deleted_rows)),
                Some(_) => {
                    let (rows, grain) = self.part_geometry(p);
                    (0..self.parts[p].granules)
                        .map(|g| {
                            let n = pgrc_format::geom::rows_in_granule_at(rows, grain, g);
                            n as u64 - self.granule_deletions(p, g).map_or(0, |m| masked_count(&m, n, true))
                        })
                        .sum()
                }
            };
            live.push(n);
        }
        let total = live.iter().sum();
        self.sample.part_live = live;
        Ok(total)
    }

    /// D3181: the live segment rows `pgrc_front::sample` counted (they follow the part rows in the sampler's stream,
    /// in claim order), and the segment claims rewound for its fetch pass.
    pub fn set_sample_segments(&mut self, rows: u64) {
        self.sample.segment_rows = rows;
        self.sample.segment_next = 0;
        self.next_segment = 0;
        self.parked = None;
    }

    /// D3181: the ordinal of the live segment row the fetch pass reads next.
    pub fn sample_segment_next(&self) -> u64 {
        self.sample.segment_next
    }

    /// D3181: the fetch pass read one live segment row.
    pub fn sample_segment_step(&mut self) {
        self.sample.segment_next += 1;
    }

    /// D3181: ANALYZE's two-pass sampler: the positions (ascending) [`Self::fetch_sample`] is asked for next. Each granule
    /// then decodes only those rows (`decode_sel`, equal to `decode_full` then select), and any other position reads as
    /// not live.
    pub fn set_sample_positions(&mut self, positions: Vec<u64>) {
        debug_assert!(positions.windows(2).all(|w| w[0] < w[1]));
        self.sample_first();
        self.sample.positions = positions;
        self.rows_in_granule = 0;
    }

    /// D3181: ANALYZE's sampler: the sample positions of the live rows at stream ordinals `ords` (ascending, in
    /// `getnextslot` order: the parts' rows, then the segments'), from the part geometry and the deletion masks (delete
    /// vectors and tombstones) alone: no column decodes. Ordinals past the live rows get no position.
    pub fn live_positions(&mut self, ords: &[u64], out: &mut Vec<u64>) -> PgResult<()> {
        debug_assert!(ords.windows(2).all(|w| w[0] < w[1]));
        if self.sample.part_live.len() != self.parts.len() {
            self.live_part_rows()?;
        }
        self.sample_first();
        let (mut i, mut base) = (0usize, 0u64);
        for p in 0..self.parts.len() {
            if i == ords.len() {
                break;
            }
            let live = self.sample.part_live[p];
            let end = i + ords[i..].partition_point(|&o| o < base + live);
            if end > i {
                check_for_interrupts()?;
                let (rows, grain) = self.part_geometry(p);
                let rel: Vec<u64> = ords[i..end].iter().map(|&o| o - base).collect();
                part_live_positions(self.sample.first[p], rows, grain, self.parts[p].granules, |g| self.granule_deletions(p, g), &rel, out);
                i = end;
            }
            base += live;
        }
        for &o in &ords[i..] {
            let k = o - base;
            if k >= self.sample.segment_rows {
                break;
            }
            out.push(SEGMENT_POSITION | k);
        }
        Ok(())
    }

    /// D3181: the part row at sample position `pos` ([`Self::live_positions`]) into `slot`, its granule decoded (only
    /// its sampled rows) when the scan is not on it; false, the slot empty, for a position that is not a sampled live
    /// part row (a segment row is `pgrc_front::sample`'s). Never mixed with `getnextslot`.
    pub fn fetch_sample(&mut self, pos: u64, slot: &mut SlotData<'_>) -> PgResult<bool> {
        if pos & SEGMENT_POSITION != 0 || (!self.sample.positions.is_empty() && self.sample.positions.binary_search(&pos).is_err()) {
            return not_live(slot);
        }
        self.sample_first();
        let (granule, row) = (pos / GRANULE_ROWS as u64, (pos % GRANULE_ROWS as u64) as u32);
        let p = self.sample.first.partition_point(|&f| f <= granule);
        if p == 0 {
            return not_live(slot);
        }
        let p = p - 1;
        let Ok(g) = u32::try_from(granule - self.sample.first[p]) else { return not_live(slot) };
        if g >= self.parts[p].granules {
            return not_live(slot);
        }
        if self.cur_part != p || self.cur_granule != g || self.rows_in_granule == 0 {
            check_for_interrupts()?;
            let prev = self.cur_part;
            if prev != usize::MAX && prev < self.parts.len() && prev != p {
                self.release_part(prev);
            }
            self.cur_part = p;
            self.cur_granule = g;
            self.decode_current_granule()?;
        }
        if row >= self.rows_in_granule || self.cur_dv_mask.as_deref().is_some_and(|m| pgrc_scan::PartDeletes::is_deleted(m, row)) {
            return not_live(slot);
        }
        self.store_row(row as usize, slot);
        Ok(true)
    }

    /// Spec §8.9 (loadstats D): each part's live rows in `getnextslot` order, and whether its stored Sample section may
    /// stand for it (a part with deletions may not: its stored rows need not be live).
    pub fn sample_part_rows(&self) -> PgResult<Vec<(u64, bool)>> {
        let mut out = sample_vec(self.parts.len())?;
        for p in &self.parts {
            let deleted = p.deletes.as_ref().map_or(0, |d| d.deleted_rows);
            out.push((p.rows.saturating_sub(deleted), deleted == 0));
        }
        Ok(out)
    }

    /// Spec §8.9 (loadstats D): part `p`'s stored sample rows that `pick` chooses (given the stored row count:
    /// ascending stored-row indices, or `None` to decline), each handed to `emit` with the values `fetch_ordinal` would
    /// serve for that row (an unneeded column reads NULL): a by-value column's stored datum word, a by-reference one's
    /// stored datum image. `Ok(false)`, nothing emitted: the part has no Sample section (sealed before the amendment or
    /// with PGRUST_PGRC_SAMPLE=0), `pick` declined, or the section lacks a needed column.
    pub fn stored_sample(
        &mut self,
        p: usize,
        pick: &mut dyn FnMut(usize) -> Option<Vec<usize>>,
        emit: &mut dyn FnMut(&[datum::Datum], &[bool]) -> PgResult<()>,
    ) -> PgResult<bool> {
        check_for_interrupts()?;
        let part = self.ensure_pinned(p)?;
        let served = stored_sample_rows(&part, &self.schemas, &self.needed, pick, emit);
        drop(part);
        if p != self.cur_part && self.parts[p].pin.take().is_some() {
            crate::inval::registry().maintain();
        }
        served
    }

    /// D3742: ANALYZE's sampled rows at `positions` (ascending, `live_positions`'), decoded by `dop` threads (this one
    /// among them) and handed to `emit` in position order with the values `fetch_ordinal` serves (an unneeded column
    /// reads NULL). The `fetch_ordinal` walk decodes each sampled granule, every column, on this thread: a uniform
    /// sample of a large table lands in most of its granules, so that walk reads and decodes the whole table one
    /// extent at a time. Here one task is one needed column of one part over the part's sampled granules, through the
    /// faces and selections the walk uses (`decode_col_dict`, else `decode_col` over the granule's sampled rows), so
    /// every value is the same bytes. A task copies its rows' by-reference values out of its granule buffers before
    /// its next decode, and advises (WILLNEED) its column's extents a stretch ahead of its reads, so a cold table's
    /// reads overlap in the device's queue. Workers run no backend-thread code (no interrupts, no ereport): this
    /// thread services interrupts between its own tasks, and raises a worker's error or panic after the join. Parts
    /// go in windows of `2 * dop`, pinned and released by this thread, so at most a window is pinned at once.
    /// `Declined` (nothing read): fewer than `min_granules` sampled granules, or `dop` < 2.
    pub fn sample_rows_parallel(
        &mut self,
        positions: &[u64],
        dop: usize,
        min_granules: usize,
        emit: &mut dyn FnMut(&[datum::Datum], &[bool]) -> PgResult<()>,
    ) -> PgResult<SampleFetch> {
        debug_assert!(positions.windows(2).all(|w| w[0] < w[1]));
        // part by part, its sampled granules: (granule, positions[lo..hi])
        let mut plan: Vec<(usize, Vec<(u32, usize, usize)>)> = Vec::new();
        let (mut i, mut ngranules) = (0usize, 0usize);
        // V5SYNC r14: the line's sample-position map (D3181 `sample.first`) stands for trunk's `part_first`
        self.sample_first();
        while i < positions.len() {
            let granule = positions[i] / GRANULE_ROWS as u64;
            let p = self.sample.first.partition_point(|&f| f <= granule);
            if p == 0 || granule - self.sample.first[p - 1] >= self.parts[p - 1].granules as u64 {
                return Ok(SampleFetch::NotLive(positions[i]));
            }
            let (p, g) = (p - 1, (granule - self.sample.first[p - 1]) as u32);
            let end = i + positions[i..].partition_point(|&q| q / GRANULE_ROWS as u64 == granule);
            match plan.last_mut() {
                Some((q, gs)) if *q == p => gs.push((g, i, end)),
                _ => plan.push((p, vec![(g, i, end)])),
            }
            ngranules += 1;
            i = end;
        }
        if dop < 2 || ngranules < min_granules.max(1) {
            return Ok(SampleFetch::Declined);
        }
        let ncols = self.schemas.len();
        let needed = self.needed.clone();
        let (mut values, mut isnull) = (sample_filled(ncols, datum::Datum::null())?, sample_filled(ncols, true)?);
        // D4372: a window the ladder's gate sent to this thread alone, so the next past the line goes there without a wait
        let mut serial = false;
        for win in plan.chunks(2 * dop) {
            let mut parts: Vec<SamplePart> = sample_vec(win.len())?;
            let mut bad = None;
            'pin: for (p, gs) in win {
                check_for_interrupts()?;
                let part = self.ensure_pinned(*p)?;
                let mut granules = sample_vec(gs.len())?;
                for &(g, lo, hi) in gs {
                    let rows = pgrc_format::geom::rows_in_granule_at(part.rows(), part.grain(), g);
                    let mask = self.parts[*p].deletes.as_deref().and_then(|d| d.granule_mask(g));
                    let mut sel = sample_vec(hi - lo)?;
                    for &q in &positions[lo..hi] {
                        let row = (q % GRANULE_ROWS as u64) as u32;
                        if row >= rows || mask.is_some_and(|m| pgrc_scan::PartDeletes::is_deleted(m, row)) {
                            bad = Some(q);
                            break 'pin;
                        }
                        sel.push(row as u16);
                    }
                    granules.push(SampleGranule { g, rows, sel });
                }
                parts.push(SamplePart { idx: *p, part, granules });
            }
            let cols = match bad {
                Some(_) => Ok(Vec::new()),
                None => sample_window_dop(dop, &mut serial, || sample_window_ask(&parts, &self.schemas, &needed, dop))
                    .and_then(|dop| sample_tasks(&parts, &self.schemas, &needed, self.binding, dop)),
            };
            drop(parts);
            let mut released = false;
            for (p, _) in win {
                if *p != self.cur_part {
                    released |= self.parts[*p].pin.take().is_some();
                }
            }
            if released {
                crate::inval::registry().maintain();
            }
            if let Some(q) = bad {
                return Ok(SampleFetch::NotLive(q));
            }
            let cols = cols?;
            for (w, (_, gs)) in win.iter().enumerate() {
                let row_cols = &cols[w * needed.len()..(w + 1) * needed.len()];
                for k in 0..gs.iter().map(|&(_, lo, hi)| hi - lo).sum() {
                    for (ci, &c) in needed.iter().enumerate() {
                        (values[c], isnull[c]) = match row_cols[ci].value(k) {
                            Some(d) => (d, false),
                            None => (datum::Datum::null(), true),
                        };
                    }
                    emit(&values, &isnull)?;
                }
            }
        }
        Ok(SampleFetch::Done)
    }
}

/// [`PgrcScanDescData::stored_sample`] on a pinned part: the section body is copied into 8-aligned words, so every
/// stored image starts where its datum may be read in place (the spec §8.9 blocks and images are 8-aligned).
fn stored_sample_rows(
    part: &OpenPart,
    schemas: &[ColSchema],
    needed: &[usize],
    pick: &mut dyn FnMut(usize) -> Option<Vec<usize>>,
    emit: &mut dyn FnMut(&[datum::Datum], &[bool]) -> PgResult<()>,
) -> PgResult<bool> {
    use pgrc_format::sample::SampleRef;
    let Some(idx) = part.find_section(pgrc_format::part::SectionKind::Sample, 0, 0) else {
        return Ok(false);
    };
    let body = crate::footer::meta_section_body(part, idx)?;
    let mut words: Vec<u64> = sample_filled(body.len().div_ceil(8), 0u64)?;
    // SAFETY: `words` holds at least `body.len()` bytes; u64 has no invalid bit patterns and u8 no alignment.
    let bytes = unsafe { core::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), body.len()) };
    bytes.copy_from_slice(&body);
    drop(body);
    let s = SampleRef::parse(bytes).map_err(|e| read_error(ReadError::Format(e)))?;
    let mut cols = sample_vec(needed.len())?;
    for &c in needed {
        let sc = &schemas[c];
        match s.col(sc.attno).map_err(|e| read_error(ReadError::Format(e)))? {
            // a cstring column (typlen -2) is read by the granule path: the section stores varlena and fixed shapes
            Some(col) if col.byref() == !sc.typbyval && sc.typlen != -2 => cols.push(col),
            _ => return Ok(false),
        }
    }
    let Some(choice) = pick(s.n()) else { return Ok(false) };
    let (mut values, mut isnull) = (sample_filled(schemas.len(), datum::Datum::null())?, sample_filled(schemas.len(), true)?);
    for &i in &choice {
        if i >= s.n() {
            return Err(read_error(ReadError::Format(pgrc_format::FormatError::Bounds { at: "sample pick" })));
        }
        for (ci, &c) in needed.iter().enumerate() {
            let col = &cols[ci];
            (values[c], isnull[c]) = if !col.valid(i) {
                (datum::Datum::null(), true)
            } else if col.byref() {
                let img = col.image(i).map_err(|e| read_error(ReadError::Format(e)))?;
                (datum::Datum::from_u64(img.as_ptr() as u64), false)
            } else {
                (datum::Datum::from_u64(col.word(i).map_err(|e| read_error(ReadError::Format(e)))?), false)
            };
        }
        emit(&values, &isnull)?;
    }
    Ok(true)
}

/// D3742: what [`PgrcScanDescData::sample_rows_parallel`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum SampleFetch {
    /// Every position's row went to `emit`, in position order.
    Done,
    /// No live row stands at this position (the rows emitted before it are the caller's to drop).
    NotLive(u64),
    /// Nothing was read: the caller walks `fetch_ordinal`.
    Declined,
}

/// One sampled granule of a [`SamplePart`]: its rows and the sampled ones (ascending).
#[derive(Clone)]
struct SampleGranule {
    g: u32,
    rows: u32,
    sel: Vec<u16>,
}

/// A part of a sample window, pinned by the backend thread: the scan's part index and its sampled granules.
#[derive(Clone)]
struct SamplePart {
    idx: usize,
    part: Arc<OpenPart>,
    granules: Vec<SampleGranule>,
}

/// One column's values of a part's sampled rows, in row order: a by-value word, or (by reference) the value's byte
/// offset in `bytes`, each value starting 8-aligned (the ByteArena law); `None` is NULL.
struct SampleCol {
    byref: bool,
    vals: Vec<Option<u64>>,
    bytes: Vec<u64>,
}

impl SampleCol {
    fn value(&self, k: usize) -> Option<datum::Datum> {
        self.vals[k].map(|v| {
            if self.byref {
                // the image lives in `bytes` (never resized after the task), read by the caller's `emit`
                datum::Datum::from_u64(self.bytes.as_ptr() as u64 + v)
            } else {
                datum::Datum::from_u64(v)
            }
        })
    }

    /// Copy the by-reference image at `ptr` (`typlen` -1 varlena, -2 cstring, else fixed) and record its offset.
    ///
    /// # Safety
    /// `ptr` is a decoded datum of a column of `typlen`: a valid image of that type's length.
    unsafe fn push_ref(&mut self, ptr: *const u8, typlen: i16) -> PgResult<()> {
        let len = match typlen {
            -1 => unsafe { types_tuple::varatt::varsize_any(ptr) },
            -2 => unsafe { std::ffi::CStr::from_ptr(ptr.cast()) }.to_bytes_with_nul().len(),
            n => n as usize,
        };
        let (at, words) = (self.bytes.len(), len.div_ceil(8));
        self.bytes.try_reserve(words).map_err(|_| sample_worker_oom(len))?;
        self.bytes.resize(at + words, 0);
        // SAFETY: `bytes` was just grown by `len` rounded up to words past `at`; `ptr` backs `len` bytes (caller).
        unsafe { core::ptr::copy_nonoverlapping(ptr, self.bytes.as_mut_ptr().add(at).cast::<u8>(), len) };
        self.vals.push(Some(at as u64 * 8));
        Ok(())
    }
}

/// D4372: the gate's poll while it waits for the ladder (R5), as the load's (pgrc_write's `MEMORY_GATE_POLL`).
const SAMPLE_GATE_POLL: core::time::Duration = core::time::Duration::from_millis(20);

/// D4372: a sample window's bytes, an estimate: each needed column's kept values of every sampled row (the `Option` word,
/// and by reference the image, 32 bytes for a varlena), and per thread one granule's decode buffer of the widest needed
/// column (datums, codes and its arena) and its cursor's reads (1 MiB).
fn sample_window_ask(parts: &[SamplePart], schemas: &[ColSchema], needed: &[usize], dop: usize) -> u64 {
    let width = |s: &ColSchema| -> u64 {
        match (s.typbyval, s.typlen) {
            (true, _) => 0,
            (false, n) if n > 0 => (n as u64).div_ceil(8) * 8,
            _ => 32,
        }
    };
    let rows: u64 = parts.iter().flat_map(|p| &p.granules).map(|g| g.sel.len() as u64).sum();
    let kept: u64 = needed.iter().map(|&c| rows.saturating_mul(size_of::<Option<u64>>() as u64 + width(&schemas[c]))).sum();
    let widest = needed.iter().map(|&c| width(&schemas[c])).max().unwrap_or(0);
    let buf = (GRANULE_ROWS as u64).saturating_mul(8 + 4 + widest).saturating_add(1 << 20);
    kept.saturating_add(buf.saturating_mul(dop as u64))
}

/// D4372: the threads of a sample window under the memory ladder (`pgrust.memory_ladder = on` only; off: `dop`, one
/// relaxed load, `ask` never called). Under the ladder's line: `dop`. Past it, D4365's gate shape on the backend thread: a
/// climb, the wait (R5, its budget, interrupts serviced), one hand-off of R2b / R6 to the ladder thread, then a second
/// wait; still past the line, the window runs on this thread alone (1: one decode buffer, nothing else in flight), and
/// `serial` sends the next window past the line there without a wait. Never a refusal: ANALYZE has the serial road.
/// Public for its test (tests/d4372_sample_gate.rs: the ladder is process state, so the test runs in its own binary).
pub fn sample_window_dop(dop: usize, serial: &mut bool, ask: impl FnOnce() -> u64) -> PgResult<usize> {
    if !mcx::ladder::on() {
        return Ok(dop);
    }
    let ask = ask();
    if mcx::ladder::need(ask) == 0 {
        *serial = false;
        return Ok(dop);
    }
    if *serial {
        return Ok(1);
    }
    let ep = mcx::ladder_stats::Wait::begin(mcx::ladder_stats::Site::Sample, ask);
    let w = mcx::ladder::wait_begin(&mcx::ladder::climb(ask));
    let budget = mempolicy::admit::bytes_wait();
    let waited = |polls: u32, from: u32| SAMPLE_GATE_POLL.saturating_mul(polls - from);
    let (mut polls, mut since, mut seen, mut handed) = (0u32, 0u32, mcx::ladder::epoch(), None::<u32>);
    let out = loop {
        if let Err(e) = check_for_interrupts() {
            ep.canceled();
            break Err(e);
        }
        if mcx::ladder::need(ask) == 0 {
            break Ok(dop);
        }
        let e = mcx::ladder::epoch();
        if e != seen {
            (seen, since) = (e, polls);
        }
        match handed {
            None if mempolicy::ladder::wait_spent(waited(polls, since), waited(polls, 0), budget) => {
                if !mcx::ladder::hand_off(ask) {
                    break Ok(dop);
                }
                handed = Some(polls);
            }
            Some(at) if mempolicy::ladder::wait_spent(waited(polls, at), waited(polls, at), budget) => {
                ep.refused();
                *serial = true;
                break Ok(1);
            }
            _ => {}
        }
        polls = polls.saturating_add(1);
        waiter::sleep(SAMPLE_GATE_POLL);
    };
    mcx::ladder::wait_end(w);
    out
}

/// (c) SAMPLE ON THE POOL: what a sample window asks of the runtime's worker pool. pgrc_am sits below the runtime
/// crate (runtime -> parallel -> ... -> pgrc_am), so the process that starts the pool installs it
/// (launch_backend, beside `runtime::install_global`).
pub trait SamplePool: Send + Sync {
    /// The pool may take work now (the runtime master switch).
    fn available(&self) -> bool;
    /// Submit `ntasks` morsels, one task each, as ONE utility resource group (`Runtime::submit_utility`) whose width
    /// is capped at `width` (`WidthRequest::ceiling`; CPU only, no cache bytes: memory is the ledger's). A pool
    /// worker runs task `t` as `run(t)`.
    fn submit(&self, ntasks: usize, width: usize, run: Arc<dyn Fn(usize) + Send + Sync>) -> Box<dyn SampleRg>;
}

/// A submitted sample window: poll it, or abort it.
pub trait SampleRg {
    /// None while it runs; Some(true) completed, Some(false) aborted.
    fn try_done(&self) -> Option<bool>;
    /// Abort: no new morsel starts; in-flight ones finish; the RG then completes as aborted.
    fn abort(&self);
}

static SAMPLE_POOL: pgsync::OnceLock<Box<dyn SamplePool>> = pgsync::OnceLock::new();

/// Install the process's sample pool (once; later calls are ignored).
pub fn install_sample_pool(pool: Box<dyn SamplePool>) {
    let _ = SAMPLE_POOL.set(pool);
}

/// The tasks of one sample window: column `needed[ci]` of `parts[w]` is task `w * needed.len() + ci`, its values at
/// that index.
///
/// (c) SAMPLE ON THE POOL (loadstats): with the pool installed and available and `dop` >= 2, the window is ONE
/// utility resource group on the runtime pool (the first plain `Runtime::submit_utility` consumer: served at the
/// utility weight, capped to B_util while foreground work is admitted, the full fair share on an idle pool). One
/// morsel is one task; `dop` (the GUC `pgrust.pgrc_analyze_threads`, unset = 1 + max_parallel_maintenance_workers,
/// after the ladder's answer for this window) is the RG's width CAP. The backend thread waits, servicing interrupts
/// (an interrupt aborts the RG first). Without the pool, or at `dop` 1, the tasks run here in order. Either road
/// fills the same table, so the emit order and every sampled value are the same.
fn sample_tasks(
    parts: &[SamplePart],
    schemas: &[ColSchema],
    needed: &[usize],
    binding: &'static CodecBinding<'static>,
    dop: usize,
) -> PgResult<Vec<SampleCol>> {
    let pool = SAMPLE_POOL.get().map(|p| &**p).filter(|p| p.available());
    sample_tasks_on(pool, parts, schemas, needed, binding, dop, SAMPLE_MORSEL_TASKS).map(|(cols, _)| cols)
}

/// One sample morsel's decode state: every buffer a task decodes into, made with the morsel and dropped with it,
/// while the morsel runs on the statement's ledger account. (The accounting prototype's worker arenas replace the
/// account entry around this struct: one site.)
struct SampleMorsel {
    buf: ColBuf,
}

impl SampleMorsel {
    fn new(owner: memledger::Owner) -> SampleMorsel {
        SampleMorsel { buf: ColBuf::charged(stmt_owner_of(owner)) }
    }
}

/// One sample task (no interrupt check: the caller's).
fn sample_task(
    parts: &[SamplePart],
    schemas: &[ColSchema],
    needed: &[usize],
    binding: &'static CodecBinding<'static>,
    t: usize,
    m: &mut SampleMorsel,
) -> PgResult<SampleCol> {
    let (sp, s) = (&parts[t / needed.len()], &schemas[needed[t % needed.len()]]);
    sample_column(sp, s, binding, &mut m.buf)
}

/// [`sample_tasks`] on an explicit pool (None: on this thread) with `per_morsel` tasks a morsel, and the most morsels it
/// ever had in flight at once (the width the pool gave it; 1 on this thread). Separate for the tests (a pool of their
/// own, small morsels).
fn sample_tasks_on(
    pool: Option<&dyn SamplePool>,
    parts: &[SamplePart],
    schemas: &[ColSchema],
    needed: &[usize],
    binding: &'static CodecBinding<'static>,
    dop: usize,
    per_morsel: usize,
) -> PgResult<(Vec<SampleCol>, usize)> {
    let ntasks = parts.len() * needed.len();
    let Some(pool) = pool.filter(|_| dop >= 2 && ntasks > per_morsel.max(1)) else {
        // this thread, in task order, its errors raised as they are, the interrupt check between tasks
        let mut out = sample_vec(ntasks)?;
        let mut m = SampleMorsel::new(memledger::charge::context().0);
        for t in 0..ntasks {
            check_for_interrupts()?;
            out.push(sample_task(parts, schemas, needed, binding, t, &mut m)?);
        }
        return Ok((out, 1));
    };
    // D4372: the statement's owner, read on the backend thread; every morsel runs on it while it decodes, so its
    // buffers and the columns it keeps count where the ledger, R6 and the ladder see them (drawn as they grow)
    // the window's own copies for the pool (fallible: ERROR 53200 in ANALYZE's context); the parts are Arc'd
    let (mut wparts, mut wschemas, mut wneeded) = (sample_vec(parts.len())?, sample_vec(schemas.len())?, sample_vec(needed.len())?);
    wparts.extend(parts.iter().cloned());
    wschemas.extend(schemas.iter().cloned());
    wneeded.extend_from_slice(needed);
    let work = Arc::new(SampleWork {
        parts: wparts,
        schemas: wschemas,
        needed: wneeded,
        binding,
        owner: memledger::charge::context().0,
        done: pgsync::Mutex::new((0..ntasks).map(|_| None).collect()),
        failed: pgsync::Mutex::new(None),
        stop: std::sync::atomic::AtomicBool::new(false),
        in_flight: std::sync::atomic::AtomicUsize::new(0),
        peak_in_flight: std::sync::atomic::AtomicUsize::new(0),
    });
    // the pool holds the work only weakly: this thread keeps the one strong handle, so the window's tables (allocated
    // here, on the statement's account) are freed here too, whichever thread finishes the RG
    let w = Arc::downgrade(&work);
    let per_morsel = per_morsel.max(1);
    let nmorsels = ntasks.div_ceil(per_morsel);
    let rg = pool.submit(
        nmorsels,
        dop.min(nmorsels),
        Arc::new(move |i| {
            if let Some(w) = w.upgrade() {
                w.run_morsel(i * per_morsel..((i + 1) * per_morsel).min(ntasks))
            }
        }),
    );
    let mut interrupted = None;
    let completed = loop {
        if let Some(c) = rg.try_done() {
            break c;
        }
        if interrupted.is_none() {
            if let Err(e) = check_for_interrupts() {
                work.stop.store(true, Ordering::Relaxed);
                rg.abort();
                interrupted = Some(e);
            }
        }
        waiter::sleep(SAMPLE_POOL_POLL);
    };
    drop(rg);
    if let Some(e) = interrupted {
        return Err(e);
    }
    if let Some(e) = work.failed.lock().unwrap().take() {
        return Err(e);
    }
    if !completed {
        return Err(Box::new(PgError::error("pgrcolumnar: an ANALYZE sample resource group was aborted"))); // alloc: Cold
    }
    let done = core::mem::take(&mut *work.done.lock().unwrap());
    let peak = work.peak_in_flight.load(Ordering::Relaxed);
    Ok((done.into_iter().map(|c| c.expect("every task ran")).collect(), peak))
}

/// (c) tasks per morsel: consecutive tasks (columns of one part, mostly) share the morsel's decode buffers, as a
/// thread's tasks shared its buffer before; a morsel stays a few tens of ms (the pool's yield grain).
const SAMPLE_MORSEL_TASKS: usize = 32;

/// (c) the backend thread's poll while a sample window's RG runs (its interrupt check between polls).
const SAMPLE_POOL_POLL: core::time::Duration = core::time::Duration::from_millis(1);

/// (c) one sample window's work on the pool: its parts (pinned by the backend thread, which keeps the pins until the
/// RG has completed), the result table and the first error.
struct SampleWork {
    parts: Vec<SamplePart>,
    schemas: Vec<ColSchema>,
    needed: Vec<usize>,
    binding: &'static CodecBinding<'static>,
    owner: memledger::charge::Owner,
    done: pgsync::Mutex<Vec<Option<SampleCol>>>,
    failed: pgsync::Mutex<Option<Box<PgError>>>,
    stop: std::sync::atomic::AtomicBool,
    in_flight: std::sync::atomic::AtomicUsize,
    peak_in_flight: std::sync::atomic::AtomicUsize,
}

impl SampleWork {
    /// One morsel = the tasks `ts`, on a pool worker, on the statement's account, sharing one [`SampleMorsel`]; a
    /// panic is contained, the first error stops the tasks not yet started.
    fn run_morsel(&self, ts: core::ops::Range<usize>) {
        let _acct = memledger::charge::enter_as(self.owner);
        let now = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak_in_flight.fetch_max(now, Ordering::Relaxed);
        let mut m = SampleMorsel::new(self.owner);
        for t in ts {
            if self.stop.load(Ordering::Relaxed) {
                break;
            }
            // unwind-ok: worker-containment
            let got = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                sample_task(&self.parts, &self.schemas, &self.needed, self.binding, t, &mut m)
            }));
            let e = match got {
                Ok(Ok(col)) => {
                    self.done.lock().unwrap()[t] = Some(col);
                    continue;
                }
                Ok(Err(e)) => e,
                Err(_) => Box::new(PgError::error("pgrcolumnar: an ANALYZE sample worker panicked")), // alloc: Cold
            };
            self.stop.store(true, Ordering::Relaxed);
            self.failed.lock().unwrap().get_or_insert(e);
            break;
        }
        drop(m);
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// D3742: an empty vector with room for `n` items, reserved fallibly (ERROR 53200 in ANALYZE's context on failure).
fn sample_vec<T>(n: usize) -> PgResult<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| mcx::oom_named("Analyze", n * size_of::<T>()))?;
    Ok(v)
}

/// D3742: `n` copies of `x`, reserved fallibly like [`sample_vec`].
fn sample_filled<T: Clone>(n: usize, x: T) -> PgResult<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| mcx::oom_named("Analyze", n * size_of::<T>()))?;
    v.resize(n, x);
    Ok(v)
}

/// D3742: a sample worker's failed allocation: `mcx::oom_named`'s ERROR 53200 without its observer hook, which is the
/// backend thread's to run (the error reaches it after the join).
fn sample_worker_oom(request: usize) -> Box<PgError> {
    Box::<PgError>::new( // alloc: Cold
        PgError::error("out of memory")
            .with_sqlstate(types_error::ERRCODE_OUT_OF_MEMORY)
            .with_detail(format!("Failed on request of size {request} in memory context \"Analyze\".")), // alloc: Cold
    )
}

/// Granules a sample task advises at a time (D3742): at each stretch's first granule, the non-resident extents of that
/// stretch and the next go to the device's queue, so the next stretch's reads are in flight while this one decodes.
const SAMPLE_ADVISE_GRANULES: usize = 32;

/// One sample task: column `s` of part `sp` over its sampled granules, each decoded as `decode_current_granule`
/// decodes it for the sample (its own private cursor; `buf` is the worker's, its part state reset here).
fn sample_column(sp: &SamplePart, s: &ColSchema, binding: &'static CodecBinding<'static>, buf: &mut ColBuf) -> PgResult<SampleCol> {
    buf.sample_dict = None;
    let mut cursor =
        StreamCursor::open(Arc::clone(&sp.part), binding, s.attno, 0).map_err(read_error)?.with_residency(Residency::Private);
    let key = cursor.key();
    let dict = key.encoding == EncodingId::DictCodes.as_u16() && key.class == CLASS_VARLENA;
    let n = sp.granules.iter().map(|sg| sg.sel.len()).sum();
    let byref = !s.typbyval;
    let mut out = SampleCol { byref, vals: Vec::new(), bytes: Vec::new() };
    out.vals.try_reserve_exact(n).map_err(|_| sample_worker_oom(n * size_of::<Option<u64>>()))?;
    let mut runs: Vec<(u32, u32)> = Vec::with_capacity(SAMPLE_ADVISE_GRANULES);
    for (i, sg) in sp.granules.iter().enumerate() {
        if i % SAMPLE_ADVISE_GRANULES == 0 {
            runs.clear();
            for next in &sp.granules[i..(i + 2 * SAMPLE_ADVISE_GRANULES).min(sp.granules.len())] {
                match runs.last_mut() {
                    Some(r) if r.1 == next.g => r.1 = next.g + 1,
                    _ => runs.push((next.g, next.g + 1)),
                }
            }
            let _ = cursor.advise_granule_runs(&runs, 256 << 10, 1 << 20, u64::MAX);
        }
        if !(dict && decode_col_dict(&mut cursor, buf, (sp.idx, &sp.part), binding, sg.g, sg.rows, &sg.sel, s.attno)?) {
            decode_sample_rows(&mut cursor, buf, sg.g, &sg.sel[..], s.attno)?;
        }
        for &r in &sg.sel {
            let r = r as usize;
            if !buf.is_valid(r) {
                out.vals.push(None);
            } else if byref {
                // SAFETY: a valid decoded datum of this column: its type's image (the slot `store_row` would fill).
                unsafe { out.push_ref(buf.datums[r] as *const u8, s.typlen)? };
            } else {
                out.vals.push(Some(buf.datums[r]));
            }
        }
    }
    Ok(out)
}

/// D3181: an empty slot and no row.
fn not_live(slot: &mut SlotData<'_>) -> PgResult<bool> {
    slot.base_mut().mark_empty();
    Ok(false)
}

/// D3181: the sample position of row `row` of the scan's granule `granule` ([`SEGMENT_POSITION`]'s doc).
fn sample_position(granule: u64, row: u32) -> u64 {
    granule * GRANULE_ROWS as u64 + row as u64
}

/// D3181: `live_positions` within one part: `ords` are part-relative live ordinals, ascending, each below the part's
/// live rows; `mask_of(g)` is granule `g`'s deletions.
fn part_live_positions(
    first_granule: u64,
    rows: u64,
    grain: GranuleGrain,
    granules: u32,
    mask_of: impl Fn(u32) -> Option<Box<[u64; pgrc_scan::DV_GRANULE_WORDS]>>,
    ords: &[u64],
    out: &mut Vec<u64>,
) {
    let mut i = 0usize;
    let mut g_base = 0u64;
    for g in 0..granules {
        if i == ords.len() {
            return;
        }
        let n = pgrc_format::geom::rows_in_granule_at(rows, grain, g);
        let mask = mask_of(g);
        let live_g = n as u64 - mask.as_deref().map_or(0, |m| masked_count(m, n, true));
        while i < ords.len() && ords[i] < g_base + live_g {
            let k = ords[i] - g_base;
            let row = match mask.as_deref() {
                None => k as u32,
                Some(m) => nth_live(m, n, k),
            };
            out.push(sample_position(first_granule + g as u64, row));
            i += 1;
        }
        g_base += live_g;
    }
}

/// The set (`deleted`) or clear bits among a mask's first `rows` bits.
fn masked_count(mask: &[u64; pgrc_scan::DV_GRANULE_WORDS], rows: u32, deleted: bool) -> u64 {
    let mut c = 0u64;
    for (w, &bits) in mask.iter().enumerate() {
        let lo = w as u32 * 64;
        if lo >= rows {
            break;
        }
        let valid = if rows - lo >= 64 { !0u64 } else { (1u64 << (rows - lo)) - 1 };
        c += ((if deleted { bits } else { !bits }) & valid).count_ones() as u64;
    }
    c
}

/// The row of a granule's `k`-th live (clear) bit among its first `rows`.
fn nth_live(mask: &[u64; pgrc_scan::DV_GRANULE_WORDS], rows: u32, mut k: u64) -> u32 {
    for (w, &bits) in mask.iter().enumerate() {
        let lo = w as u32 * 64;
        if lo >= rows {
            break;
        }
        let valid = if rows - lo >= 64 { !0u64 } else { (1u64 << (rows - lo)) - 1 };
        let mut live = !bits & valid;
        let c = live.count_ones() as u64;
        if k < c {
            for _ in 0..k {
                live &= live - 1;
            }
            return lo + live.trailing_zeros();
        }
        k -= c;
    }
    rows
}

/// D3181: rows `sel` (ascending) of granule `g` of `cursor`'s column into `buf`, each at its row's slot (the others not
/// written), through the reader's survivor-only face (`decode_sel`: equal to `decode_full` then the rows); the arena
/// grown on typed exhaustion as the whole granule's decode grows it, the validity the granule's.
fn decode_sample_rows(cursor: &mut StreamCursor<'static>, buf: &mut ColBuf, g: u32, sel: &[u16], attno: u32) -> PgResult<()> {
    buf.ensure_allocated();
    let want = sel.len() as u32;
    loop {
        let arena_words = buf.arena.len();
        let (datums, arena) = (&mut buf.datums, &mut buf.arena);
        // SAFETY: see ColBuf::arena_bytes (split borrows force the inline form here).
        let arena_bytes = unsafe { core::slice::from_raw_parts_mut(arena.as_mut_ptr() as *mut u8, arena.len() * 8) };
        let mut out = DecodeOut { datums: &mut datums[..want as usize], arena: ByteArena::new(arena_bytes) };
        match cursor.decode_sel(g, &Selection { rows: sel }, &mut out) {
            // the length-prefix trust law of the whole granule's decode, over the selection
            Ok(n) => {
                check_decoded_count(attno, g, n, want)?;
                break;
            }
            Err(ReadError::Format(pgrc_format::FormatError::ArenaExhausted { .. })) => {
                if !grow_arena(&mut buf.arena, arena_words * 2) {
                    return Err(read_error(ReadError::Io { at: "granule arena", errno: libc::ENOMEM }));
                }
            }
            Err(e) => return Err(read_error(e)),
        }
    }
    // dense in selection order -> each row's slot; descending, as row sel[k] >= k
    for (k, &r) in sel.iter().enumerate().rev() {
        buf.datums[r as usize] = buf.datums[k];
    }
    buf.hold_datums(cursor);
    match cursor.validity(g, &mut buf.validity).map_err(read_error)? {
        ValidityVerdict::AllValid => buf.all_valid = true,
        ValidityVerdict::Mixed { .. } => buf.all_valid = false,
    }
    Ok(())
}

/// D3181: the sampled rows `sel` of a dictionary-coded varlena column without its whole dictionary: the granule's codes
/// (`decode_codes`), then each sampled valid row's entry from the part's frame-lazy `DictHandle`, which faults only the
/// frames those codes name. A datum is the entry's varlena image, the bytes `decode_sel` points at. False when the
/// reader declines the dictionary handle: the caller decodes through `decode_sel`.
#[allow(clippy::too_many_arguments)]
fn decode_col_dict(
    cursor: &mut StreamCursor<'static>,
    buf: &mut ColBuf,
    (part_idx, part): (usize, &Arc<OpenPart>),
    binding: &'static CodecBinding<'static>,
    g: u32,
    rows: u32,
    sel: &[u16],
    attno: u32,
) -> PgResult<bool> {
    buf.ensure_allocated();
    if buf.sample_dict.as_ref().is_none_or(|(p, _)| *p != part_idx) {
        buf.sample_dict = None;
        match DictHandle::open(Arc::clone(part), None, binding.unwrappers, attno, 0) {
            Ok(h) => buf.sample_dict = Some((part_idx, h)),
            Err(ReadError::Unsupported { .. }) => return Ok(false),
            Err(e) => return Err(read_error(e)),
        }
    }
    crate::alloc::try_resize(&mut buf.codes, rows as usize, 0, "scan")?;
    let n = cursor.decode_codes(g, &mut buf.codes[..rows as usize]).map_err(read_error)?;
    check_decoded_count(attno, g, n, rows)?;
    buf.all_valid = match cursor.validity(g, &mut buf.validity).map_err(read_error)? {
        ValidityVerdict::AllValid => true,
        ValidityVerdict::Mixed { .. } => false,
    };
    let dict = &buf.sample_dict.as_ref().expect("opened above").1;
    for &r in sel {
        let r = r as usize;
        buf.datums[r] = if buf.all_valid || (buf.validity[r / 64] >> (r % 64)) & 1 == 1 {
            match dict.entry(buf.codes[r]) {
                Ok(e) => e.image.as_ptr() as u64,
                Err(ReadError::Unsupported { .. }) => return Ok(false),
                Err(e) => return Err(read_error(e)),
            }
        } else {
            0
        };
    }
    // the datums alias the handle's entries (`sample_dict`), not the previous decode's region
    buf.guard.release();
    buf.fill = None;
    Ok(true)
}

/// SCANSTATS: the scan's counts land when it is dropped (table_endscan, and an erroring statement's teardown: heapam's
/// drain posture), the open segment's rows with them.
impl Drop for PgrcScanDescData<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        self.stats_stop();
        if let Some(seg) = self.segment.as_ref() {
            let n = seg.returned();
            self.stats.segment_rows(n);
        }
        self.stats.finish(&self.rs_base.rs_rd);
    }
}

/// C1 L5: the residency of a read over `part_bytes` of part files, PostgreSQL's BAS_BULKREAD rule over the
/// decoded-column cap. Past a quarter of it (a bulk read: a large scan, a refetch in a statement whose scan of the
/// relation is bulk, a compaction job over large inputs) `Private`: its images live while its cursors hold them, one
/// extent per stream, and never enter the cache, so a bulk operation never evicts everyone's hot data. Otherwise
/// `Cached`, bounded by the cap.
pub(crate) fn read_residency(part_bytes: u64) -> Residency {
    if part_bytes > crate::inval::cache_cap() / 4 {
        Residency::Private
    } else {
        Residency::Cached
    }
}

/// C1 L5: one part's decode cursors under one residency, kept across the granules a caller decodes (the row cache's
/// refetches, a compaction job's inputs), as the row scan keeps its part-lived cursors: a cursor re-reads only when
/// it leaves its extent, where a fresh one per granule would re-read the extent each time (PostgreSQL's ring posture).
pub(crate) struct PartCursors {
    part: Arc<OpenPart>,
    residency: Residency,
    cursors: Vec<Option<StreamCursor<'static>>>,
}

impl PartCursors {
    pub(crate) fn new(part: &Arc<OpenPart>, residency: Residency) -> PartCursors {
        PartCursors { part: Arc::clone(part), residency, cursors: Vec::new() }
    }

    /// Whether these cursors read `part` under `residency`.
    pub(crate) fn reads(&self, part: &Arc<OpenPart>, residency: Residency) -> bool {
        Arc::ptr_eq(&self.part, part) && self.residency == residency
    }
}

/// FASTADD: the columns a part lacks (`present[c]` false) and what each reads ([`crate::fastadd::Fill`], by column).
#[derive(Clone, Copy)]
pub(crate) struct Lacks<'a> {
    pub(crate) present: &'a [bool],
    pub(crate) fills: &'a [crate::fastadd::Fill],
}

/// Decode granule `g` of `pc`'s part, every column of `schemas`, into `cols` through `pc`'s cursors (the row cache's
/// and compaction's decode); the granule's row count. FASTADD: a column the part lacks (`lacks`) reads its fill.
pub(crate) fn decode_granule(pc: &mut PartCursors, binding: &'static CodecBinding<'static>, schemas: &[ColSchema], g: u32, cols: &mut [ColBuf], lacks: Option<Lacks<'_>>) -> PgResult<u32> {
    let mut rows: Option<u32> = None;
    let more = schemas.len().saturating_sub(pc.cursors.len());
    pc.cursors.try_reserve(more).map_err(|_| mcx::oom_named("pgrcolumnar", more * size_of::<Option<StreamCursor<'static>>>()))?;
    pc.cursors.resize_with(schemas.len(), || None);
    for (c, schema) in schemas.iter().enumerate() {
        if lacks.is_some_and(|l| !l.present[c]) {
            continue;
        }
        if pc.cursors[c].is_none() {
            pc.cursors[c] = Some(StreamCursor::open(Arc::clone(&pc.part), binding, schema.attno, 0).map_err(read_error)?.with_residency(pc.residency));
        }
        let cursor = pc.cursors[c].as_mut().expect("just opened");
        let g_rows = cursor.rows_in_granule(g);
        match rows {
            None => rows = Some(g_rows),
            Some(r) if r != g_rows => {
                return Err(Box::new(PgError::error(format!(
                    "pgrcolumnar: column {} granule {} row skew ({} vs {})",
                    schema.attno, g, g_rows, r
                ))
                .with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED)));
            }
            Some(_) => {}
        }
        decode_column_granule(cursor, schema, g, &mut cols[c])?;
    }
    let Some(l) = lacks else { return Ok(rows.unwrap_or(0)) };
    let rows = rows.unwrap_or_else(|| pgrc_format::geom::rows_in_granule_at(pc.part.rows(), pc.part.grain(), g));
    for (c, buf) in cols.iter_mut().enumerate().filter(|(c, _)| !l.present[*c]) {
        buf.fill_const(rows, &l.fills[c]);
    }
    Ok(rows)
}

/// IX1 L0 (choice 6): decode granule `g` of column `c` (`schema`, one of `ncols`) of `pc`'s part into `buf`, through
/// `pc`'s cursor for that column (opened on first use): the column-selective row cache decodes only the columns a
/// fetch projects. The granule's row count.
pub(crate) fn decode_one_column(pc: &mut PartCursors, binding: &'static CodecBinding<'static>, ncols: usize, c: usize, schema: &ColSchema, g: u32, buf: &mut ColBuf) -> PgResult<u32> {
    if pc.cursors.len() < ncols {
        let more = ncols - pc.cursors.len();
        pc.cursors.try_reserve(more).map_err(|_| mcx::oom_named("pgrcolumnar", more * size_of::<Option<StreamCursor<'static>>>()))?;
        pc.cursors.resize_with(ncols, || None);
    }
    if pc.cursors[c].is_none() {
        pc.cursors[c] = Some(StreamCursor::open(Arc::clone(&pc.part), binding, schema.attno, 0).map_err(read_error)?.with_residency(pc.residency));
    }
    buf.ensure_allocated();
    decode_column_granule(pc.cursors[c].as_mut().expect("just opened"), schema, g, buf)
}

/// Decode granule `g` of `cursor`'s column (`schema`) into `buf`, holding what its datums alias (`hold_datums`); the
/// granule's row count (K1-MEM: a compaction's column pass keeps one cursor per input).
pub(crate) fn decode_column_granule(cursor: &mut StreamCursor<'_>, schema: &ColSchema, g: u32, buf: &mut ColBuf) -> PgResult<u32> {
    let g_rows = cursor.rows_in_granule(g);
    // decode_full with arena growth on typed exhaustion.
    loop {
        let arena_words = buf.arena.len();
        let (datums, arena) = (&mut buf.datums, &mut buf.arena);
        let arena_bytes = unsafe {
            // SAFETY: see ColBuf::arena_bytes (split borrows force
            // the inline form here).
            core::slice::from_raw_parts_mut(
                arena.as_mut_ptr() as *mut u8,
                arena.len() * 8,
            )
        };
        let mut out = DecodeOut {
            datums: &mut datums[..g_rows as usize],
            arena: ByteArena::new(arena_bytes),
        };
        match cursor.decode_full(g, &mut out) {
            // Length-prefix trust: `decode_full` writes exactly the
            // granule's value-count (an untrusted per-granule gcount
            // from the part file) datums and returns that count, but we
            // sized `out.datums` by — and `store_row` later publishes —
            // `g_rows` (the closed-form footer geometry). If the kernel
            // wrote fewer than `g_rows`, slots [n..g_rows) were never
            // written this granule and hold zero/stale/dangling datums
            // from the reused ColBuf. Bind the served row count to the
            // written witness and refuse typed on any undercount rather
            // than publish unwritten memory as valid rows.
            Ok(n) => {
                check_decoded_count(schema.attno, g, n, g_rows)?;
                break;
            }
            // D2582: the arena grows while the granule's images do not fit, fallibly
            Err(ReadError::Format(pgrc_format::FormatError::ArenaExhausted { .. })) => {
                if !grow_arena(&mut buf.arena, arena_words * 2) {
                    return Err(read_error(ReadError::Io { at: "granule arena", errno: libc::ENOMEM }));
                }
            }
            Err(e) => return Err(read_error(e)),
        }
    }
    buf.hold_datums(cursor);
    match cursor.validity(g, &mut buf.validity).map_err(read_error)? {
        ValidityVerdict::AllValid => buf.all_valid = true,
        ValidityVerdict::Mixed { .. } => buf.all_valid = false,
    }
    Ok(g_rows)
}

/// IXPERF F1: what one row of one column decodes into ([`decode_one_row`]): the arena a by-reference value is built
/// in and the guard over what the datum aliases (a dictionary's payload region), both kept until the column's next
/// slice, so the datum outlives the slot's materialize.
pub(crate) struct SliceBuf {
    arena: memledger::ChargedVec<u64>,
    guard: Guard,
}

impl SliceBuf {
    pub(crate) fn new() -> SliceBuf {
        SliceBuf { arena: memledger::ChargedVec::default(), guard: Guard::new() }
    }

    /// Let go of what the last slice's datum aliased (the arena stays for the next).
    pub(crate) fn release(&mut self) {
        self.guard.release();
    }
}

/// A slice's first arena (grows as the granule path's does, fallibly: [`grow_arena`]).
const SLICE_ARENA_INITIAL: usize = 8 << 10;

/// IXPERF F1: row `row` of granule `g` of column `c` (`schema`, one of `ncols`) of `pc`'s part, through `pc`'s cursor
/// for that column (opened on first use): its datum word, `None` for NULL. The validity bit is read first, so a NULL
/// row decodes no value; the value comes from the reader's survivor-only face (`decode_sel` of the one row, spec §19.3:
/// equal to `decode_full` then the row) instead of the granule's 8,192. The datum may alias `buf` until the column's
/// next slice.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_one_row(pc: &mut PartCursors, binding: &'static CodecBinding<'static>, ncols: usize, c: usize, schema: &ColSchema, g: u32, row: u32, buf: &mut SliceBuf) -> PgResult<Option<u64>> {
    if pc.cursors.len() < ncols {
        let more = ncols - pc.cursors.len();
        pc.cursors.try_reserve(more).map_err(|_| mcx::oom_named("pgrcolumnar", more * size_of::<Option<StreamCursor<'static>>>()))?;
        pc.cursors.resize_with(ncols, || None);
    }
    if pc.cursors[c].is_none() {
        pc.cursors[c] = Some(StreamCursor::open(Arc::clone(&pc.part), binding, schema.attno, 0).map_err(read_error)?.with_residency(pc.residency));
    }
    let cursor = pc.cursors[c].as_mut().expect("just opened");
    let Ok(at) = u16::try_from(row) else {
        return Err(Box::new(
            PgError::error(format!("pgrcolumnar: row {row} of column {} granule {g} is past a granule", schema.attno))
                .with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED),
        ));
    };
    let mut validity = [0u64; VALIDITY_WORDS];
    if let ValidityVerdict::Mixed { .. } = cursor.validity(g, &mut validity).map_err(read_error)? {
        if (validity[row as usize / 64] >> (row % 64)) & 1 == 0 {
            return Ok(None);
        }
    }
    let rows = [at];
    let sel = pgrc_format::abi::Selection { rows: &rows };
    if buf.arena.is_empty() {
        buf.arena.with_vec(|v| *v = vec![0u64; SLICE_ARENA_INITIAL / 8]); // alloc: Exec charged buf.arena.owner()
    }
    loop {
        let arena_words = buf.arena.len();
        // SAFETY: `buf.arena` is a live, uniquely borrowed u64 buffer of `arena_words` words: `arena_words * 8` initialized
        // bytes, 8-aligned (the ByteArena law), borrowed only for this one decode call.
        let arena_bytes = unsafe { core::slice::from_raw_parts_mut(buf.arena.as_mut_ptr() as *mut u8, arena_words * 8) };
        let mut one = [0u64; 1];
        let mut out = DecodeOut { datums: &mut one, arena: ByteArena::new(arena_bytes) };
        match cursor.decode_sel(g, &sel, &mut out) {
            Ok(n) => {
                check_decoded_count(schema.attno, g, n, 1)?;
                buf.guard.release();
                cursor.hold_datums(&mut buf.guard);
                return Ok(Some(one[0]));
            }
            // the arena grows while the row's images do not fit, fallibly, as the granule path's does
            Err(ReadError::Format(pgrc_format::FormatError::ArenaExhausted { .. })) => {
                if !grow_arena(&mut buf.arena, arena_words * 2) {
                    return Err(read_error(ReadError::Io { at: "granule arena", errno: libc::ENOMEM }));
                }
            }
            Err(e) => return Err(read_error(e)),
        }
    }
}

/// Store row `row` of decoded `cols` into `slot` (`natts` wide: dropped columns are NULL).
pub(crate) fn store_cols(cols: &[ColBuf], schemas: &[ColSchema], natts: usize, row: usize, slot: &mut SlotData<'_>) {
    let base = slot.base_mut();
    if natts != cols.len() {
        base.tts_isnull[..natts].fill(true);
        base.tts_values[..natts].fill(datum::Datum::null());
    }
    for (buf, schema) in cols.iter().zip(schemas) {
        let c = schema.attno as usize - 1;
        if buf.is_valid(row) {
            base.tts_isnull[c] = false;
            base.tts_values[c] = datum::Datum::from_u64(buf.datums[row]);
        } else {
            base.tts_isnull[c] = true;
            base.tts_values[c] = datum::Datum::null();
        }
    }
    base.tts_nvalid = natts as types_core::AttrNumber;
    base.mark_not_empty();
}

/// Row `row` of decoded column `c`: its datum word, `None` for NULL (K1's rewrite reads rows without a slot).
pub(crate) fn col_cell(cols: &[ColBuf], c: usize, row: usize) -> Option<u64> {
    cols[c].is_valid(row).then(|| cols[c].datums[row])
}

// ---------------------------------------------------------------------------
// The sqe engine-binding bridge (M4-S3 landing 2): open this relation's
// snapshot-effective part set as a `pgrc_scan::TableScan` — the SAME
// resolve/validate/record walk `PgrcScanDescData::begin` runs (identity
// checked through the shared registry, part keys recorded for
// invalidation), holding UNPINNED `Arc<OpenPart>`s (alive via Arc,
// evictable from the registry cache — never the 100M eager-pin posture;
// the pipeline's claim drive takes its own claim-scoped pins, PC-2.4).
// ---------------------------------------------------------------------------

/// The bridged scan + the facts the engine seam consults at lowering.
pub struct EngineBridgedScan {
    pub scan: pgrc_scan::TableScan,
    /// Manifest row total (exact at seal).
    pub total_rows: u64,
    /// DM-2 witness input: any part carries a delete vector (`dv_gen != 0`).
    pub deletion_bearing: bool,
    /// The staged columns' schemas in staged order (profile derivation).
    pub schemas: Vec<ColSchema>,
    /// The snapshot-effective manifest (P2-1: `sqe::bank::Bank::from_bridge`
    /// consumes it — generation identity + part geometry).
    pub manifest: pgrc_format::manifest::Manifest,
    /// The table directory path the parts were opened under.
    pub dir: String,
}

/// One staged column for the lane-capable bridge face: the root stream of
/// a catalog column, or a shredded-jsonb typed lane of it (TY-3 read
/// supply). Lane columns declare (dotted path, expected lane kind); the
/// scan resolves them per part and refuses typed where a part cannot
/// serve them (`ReadError::ShredLaneRefused` — see `pgrc_scan`).
#[derive(Debug, Clone)]
pub struct EngineScanCol {
    /// 1-based catalog attno (the parent jsonb column for lane specs).
    pub attno: u32,
    /// `None` = the root column; `Some((path, kind))` = its shred lane.
    pub shred: Option<(String, pgrc_format::shredlane::ShredLaneKind)>,
}

/// The generation the engine bridge serves of a plan-time lookup: `Ok(None)` for the empty table, `Err(())` for a
/// relation the catalog does not hold. F1's CREATE publishes a generation without parts (the segment records'
/// rotation), so a visible generation without parts is the empty table too and answers as a never-ingested one: the
/// engine reads parts only (a table with segment rows never reaches the bridge, sqeshell's `find_columnar` declines
/// it first), so no row is hidden.
pub(crate) fn bridge_generation(found: crate::catalog::Lookup) -> Result<Option<Arc<crate::catalog::Generation>>, ()> {
    match found {
        crate::catalog::Lookup::Gen(g) if !g.manifest.parts.is_empty() => Ok(Some(g)),
        crate::catalog::Lookup::Gen(_) | crate::catalog::Lookup::Empty => Ok(None),
        crate::catalog::Lookup::Unknown => Err(()),
    }
}

/// Open the engine TableScan over `rel` for the 1-based catalog attno set
/// `attnos` (staged order preserved; the ColSchema.attno domain).
/// `Ok(None)` = no part visible: an empty table, never ingested or (F1) holding
/// only the part-less generation its CREATE publishes —
/// the caller declines. Non-word storage classes are the CALLER's to
/// refuse; this face stages whatever schema the catalog carries.
pub fn open_engine_table_scan(
    rel: &types_rel::Relation<'_>,
    snapshot: Option<&types_snapshot::SnapshotData<'_>>,
    attnos: &[u32],
) -> PgResult<Option<EngineBridgedScan>> {
    let cols: Vec<EngineScanCol> = attnos
        .iter()
        .map(|&attno| EngineScanCol { attno, shred: None })
        .collect();
    open_engine_table_scan_cols(rel, snapshot, &cols)
}

/// The lane-capable bridge face: root columns AND shredded-jsonb lane
/// columns in one staged order (a lane column's schema is minted from its
/// declared kind — `pgrc_format::shredlane`, the one authority — since
/// lanes are derived streams the catalog never carries). The parent of a
/// lane spec must be a varlena catalog column (jsonb); everything past
/// declaration is the scan's per-part resolution + typed-refusal law.
pub fn open_engine_table_scan_cols(
    rel: &types_rel::Relation<'_>,
    snapshot: Option<&types_snapshot::SnapshotData<'_>>,
    cols: &[EngineScanCol],
) -> PgResult<Option<EngineBridgedScan>> {
    crate::inval::ensure_inval_registered()?;
    let locator = rel.rd_locator.get();
    let slot: crate::catalog::RelSlot = (locator.spcOid, locator.dbOid, locator.relNumber as u64);
    let all_schemas = crate::schema::col_schemas(rel)?;
    let fp = schema_fingerprint(&all_schemas);
    let dir = crate::dirpath::table_dir_path(locator, rel.rd_backend);
    crate::catalog::record_relid(rel.rd_id, slot);
    let check = SnapshotCommitCheck::new(snapshot);
    let found = crate::catalog::resolve(rel, slot, &dir, &check)?;
    check.take_error()?;
    let Some(gen) = bridge_generation(found).map_err(|()| crate::catalog::not_loaded(rel, &dir))? else {
        return Ok(None);
    };
    // FASTADD: the engine reads stored streams only; a generation holding parts written before an ADD/DROP COLUMN
    // (their absent columns are filled by the row scan) is not its to read (SQE declines it first: `served_lock`)
    if gen.schema_fp != fp || gen.lagged() {
        return Err(read_error(ReadError::OpenMismatch { field: "manifest schema_fingerprint" }));
    }
    // The parts are the catalog's open handles (held since server start or
    // since their publish): no file is opened or read here.
    let nparts = gen.parts.len();
    let mut parts: Vec<Arc<OpenPart>> = crate::alloc::try_vec(nparts, "scan")?;
    let mut deletes: Vec<Option<Arc<pgrc_scan::PartDeletes>>> = crate::alloc::try_vec(nparts, "scan")?;
    let mut total_rows = 0u64;
    let mut deletion_bearing = false;
    for (p, rec) in gen.parts.iter().zip(&gen.manifest.parts) {
        let ident = p.part.ident();
        crate::inval::record_part_key(rel.rd_id, (ident.dev, ident.ino, ident.len));
        total_rows += rec.rows;
        deletion_bearing |= rec.dv_gen != 0;
        deletes.push(p.deletes.clone());
        parts.push(Arc::clone(&p.part));
    }
    if !deletion_bearing {
        deletes.clear();
    }

    let mut schemas: Vec<ColSchema> = crate::alloc::try_vec(cols.len(), "scan")?;
    let mut columns: Vec<pgrc_scan::ScanColumn> = crate::alloc::try_vec(cols.len(), "scan")?;
    for c in cols {
        let a = c.attno;
        let Some(s) = all_schemas.iter().find(|s| s.attno == a) else {
            return Err(Box::new(PgError::error(format!( // alloc: Cold
                "pgrcolumnar engine bridge: staged attno {a} outside the schema"
            ))));
        };
        match &c.shred {
            None => {
                schemas.push(*s);
                columns.push(pgrc_scan::ScanColumn::root(*s, s.collation_class == pgrc_format::class::CollationClass::C));
            }
            Some((path, kind)) => {
                // Lane parents are varlena catalog columns (jsonb images);
                // any other class cannot carry shred lanes — declaration
                // error at the bridge, before any part is consulted.
                if s.class != pgrc_format::class::StorageClass::VarlenaVerbatim {
                    return Err(Box::new(PgError::error(format!( // alloc: Cold
                        "pgrcolumnar engine bridge: shred lane declared on non-varlena attno {a}"
                    ))));
                }
                let col = pgrc_scan::ScanColumn::shred_lane(a, path, *kind);
                schemas.push(col.schema);
                columns.push(col);
            }
        }
    }

    let manifest = (*gen.manifest).clone();
    let binding = codec_binding();
    Ok(Some(EngineBridgedScan {
        scan: pgrc_scan::TableScan {
            parts,
            binding,
            unwrappers: binding.unwrappers,
            columns,
            predicate: None,
            strview_cols: vec![],
            code_key_cols: vec![],
            deletes,
            opts: pgrc_scan::ScanOptions::default(),
        },
        total_rows,
        deletion_bearing,
        schemas,
        manifest,
        dir,
    }))
}

#[cfg(test)]
mod value_count_tests {
    use super::check_decoded_count;

    #[test]
    fn exact_written_count_is_accepted() {
        // The valid scan: the kernel wrote exactly the closed-form geometry.
        check_decoded_count(1, 0, 4096, 4096).expect("exact match must pass");
        check_decoded_count(3, 7, 0, 0).expect("empty granule must pass");
    }

    #[test]
    fn undercount_raises_data_corrupted() {
        // The finding: a crafted gcount undercounts the granule, so the kernel
        // wrote fewer datums than the AM publishes; slots [written..expected)
        // would otherwise leak zero/stale/dangling datums.
        let err = check_decoded_count(2, 5, 3, 4096).err().unwrap();
        assert_eq!(err.sqlstate(), types_error::ERRCODE_DATA_CORRUPTED);
    }

    #[test]
    fn overcount_also_refuses() {
        // Symmetric guard: an over-write past the sized buffer geometry is
        // equally a corruption signal (the buffer is sized by `expected`).
        let err = check_decoded_count(4, 9, 5000, 4096).err().unwrap();
        assert_eq!(err.sqlstate(), types_error::ERRCODE_DATA_CORRUPTED);
    }
}

/// D777 / the UAF fix (2b9d5be56d5): the row scan's dictionary datums are zero-copy pointers into the dict payload
/// region (the pointer-class law, `pgrc_format::abi::DecodeOut`). An unwrapped DictPayload of more than one dict frame
/// is cut into one extent per frame (SB-7), which the cursor assembles into a region of its own (`pgrc_read`
/// `cursor.rs` `load_region_with`). The datums a decode leaves in its `ColBuf`s must stay readable after the cursor
/// drops: `decode_granule`'s (the row cache and compaction) and the row scan's part-lived cursors'.
#[cfg(test)]
mod dict_region_tests {
    use super::{check_decoded_count, codec_binding, col_cell, decode_granule, grow_arena, read_error, ColBuf, PartCursors};
    use pgrc_format::abi::{ByteArena, DecodeOut, ValidityVerdict};
    use pgrc_read::{ReadError, Residency, StreamCursor};
    use types_error::PgResult;
    use pgrc_format::class::{ColSchema, CollationClass, StorageClass, TypeSemantics};
    use pgrc_format::enc::EncodingId;
    use pgrc_format::part::StreamRole;
    use pgrc_read::{MemPartIo, OpenPart, PartExpect};
    use pgrc_write::dict::TextSemantics;
    use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, DictPolicy};
    use pgrc_write::ingest::{NoExternalDetoast, RawDatum};
    use pgrc_write::publish::{TxnProbe, TxnVerdict};
    use pgrc_write::seal::CodecResolver;
    use pgrc_write::shred::NoShred;
    use pgrc_write::writer::{PartCutPolicy, SealEnv, SubxactEvidence, TableWriter, TxnStamp};
    use pgrc_write::wvfs::{MemVfs, WriteVfs};
    use std::sync::Arc;

    /// Distinct values: past `DICT_FRAME_ENTRIES` (1,024), so the payload has two frames and two extents; `SMALL_NDV`
    /// keeps it to one frame, one extent (the part's cached image itself under `Residency::Cached`).
    const NDV: usize = 1500;
    const SMALL_NDV: usize = 600;
    /// Payload bytes per value: with the 4-byte header an entry is 64 bytes, so no entry padding.
    const PAYLOAD: usize = 60;
    const ROWS: usize = 12_000;

    /// The per-column body of `PgrcScanDescData::decode_current_granule` (scan.rs), verbatim but for `self.` ->
    /// parameters, so the test runs its cursor and `ColBuf` without a relation. Each column's cursor is dropped when
    /// this returns: what `set_needed_attrs` (a column no longer read), `release_part` and `reset_position` do to the
    /// part-lived cursors while the `ColBuf`s still hold the granule's datums (D777).
    fn scan_decode_granule(part: &Arc<OpenPart>, binding: &'static pgrc_read::CodecBinding<'static>, schemas: &[ColSchema], g: u32, cols: &mut [ColBuf], residency: Residency) -> PgResult<u32> {
        let rows = pgrc_format::geom::rows_in_granule_at(part.rows(), part.grain(), g);
        let mut cursors: Vec<Option<StreamCursor<'static>>> = (0..schemas.len()).map(|_| None).collect();
        for c in 0..schemas.len() {
            let attno = schemas[c].attno;
            if cursors[c].is_none() {
                let cursor = StreamCursor::open(Arc::clone(part), binding, attno, 0)
                    .map_err(read_error)?
                    .with_residency(residency);
                cursors[c] = Some(cursor);
            }
            let cursor = cursors[c].as_mut().expect("just opened");
            let buf = &mut cols[c];
            buf.ensure_allocated();
            // decode_full with arena growth on typed exhaustion.
            loop {
                let arena_words = buf.arena.len();
                let (datums, arena) = (&mut buf.datums, &mut buf.arena);
                // SAFETY: as decode_current_granule's (u64-backed, 8-aligned, exclusively borrowed).
                let arena_bytes = unsafe { core::slice::from_raw_parts_mut(arena.as_mut_ptr() as *mut u8, arena.len() * 8) };
                let mut out = DecodeOut { datums: &mut datums[..rows as usize], arena: ByteArena::new(arena_bytes) };
                match cursor.decode_full(g, &mut out) {
                    Ok(n) => {
                        check_decoded_count(attno, g, n, rows)?;
                        break;
                    }
                    Err(ReadError::Format(pgrc_format::FormatError::ArenaExhausted { .. })) => {
                        if !grow_arena(&mut buf.arena, arena_words * 2) {
                            return Err(read_error(ReadError::Io { at: "granule arena", errno: libc::ENOMEM }));
                        }
                    }
                    Err(e) => return Err(read_error(e)),
                }
            }
            buf.hold_datums(cursor);
            match cursor.validity(g, &mut buf.validity).map_err(read_error)? {
                ValidityVerdict::AllValid => buf.all_valid = true,
                ValidityVerdict::Mixed { .. } => buf.all_valid = false,
            }
        }
        drop(cursors);
        Ok(rows)
    }

    struct AllCommitted;
    impl TxnProbe for AllCommitted {
        fn verdict(&self, _fxid: u64) -> TxnVerdict {
            TxnVerdict::Committed
        }
    }

    fn schema() -> ColSchema {
        ColSchema {
            attno: 1,
            class: StorageClass::VarlenaVerbatim,
            typlen: -1,
            typbyval: false,
            typalign: b'i',
            collation_class: CollationClass::C,
            semantics: TypeSemantics::TextCollated,
        }
    }

    /// `ndv` distinct high-entropy payloads (xorshift bytes): zstd cannot save the 20 % the wrapper needs, so the
    /// payload stays unwrapped.
    fn values(ndv: usize) -> Vec<Vec<u8>> {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        (0..ndv)
            .map(|_| {
                (0..PAYLOAD)
                    .map(|_| {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        (x >> 24) as u8
                    })
                    .collect()
            })
            .collect()
    }

    fn image(v: &[u8]) -> Vec<u8> {
        let mut img = pgrc_format::wire::varlena_header_4b_u(v.len() as u32).to_le_bytes().to_vec();
        img.extend_from_slice(v);
        img
    }

    /// Seal one part of `ROWS` rows, row `r` holding `vals[r % vals.len()]`, with the dictionary arm offered.
    fn part_bytes(vals: &[Vec<u8>]) -> Vec<u8> {
        let mut vfs = MemVfs::new();
        vfs.mkdir_path("t").expect("mkdir");
        let mut w = TableWriter::open(
            "t".to_string(),
            vec![schema()],
            1663,
            5,
            778,
            TxnStamp { fxid: 42, cid: 1 },
            &SubxactEvidence::default(),
            PartCutPolicy::default(),
        )
        .expect("open");
        let cands = CodecCandidates::new(ColumnPosture {
            cold: true,
            dict: Some(DictPolicy { ndv_cap: 1 << 24, exec_ok: true, sem: TextSemantics::BytesOnly }),
            ..Default::default()
        });
        let sources: [&dyn CandidateSource; 1] = [&cands];
        let resolver = CodecResolver;
        let mut shred = NoShred;
        let opts = pgrc_format::relopt::ShredOptions::default();
        let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &resolver, shred: &mut shred, shred_opts: &opts };
        for r in 0..ROWS {
            let img = image(&vals[r % vals.len()]);
            w.append_row(&[RawDatum::Bytes(&img)], &mut NoExternalDetoast, &mut env).expect("append");
        }
        w.finish(&mut env).expect("finish");
        let out = w.publish(&mut vfs, &AllCommitted).expect("publish");
        assert_eq!(out.part_nos.len(), 1, "one part");
        vfs.read_full(&format!("t/{}", pgrc_format::dirlayout::part_file_name(out.part_nos[0]))).expect("part bytes")
    }

    type Decode = fn(&Arc<OpenPart>, &'static pgrc_read::CodecBinding<'static>, &[ColSchema], u32, &mut [ColBuf], Residency) -> PgResult<u32>;

    /// `decode_granule` through cursors dropped when it returns (a refetch's part or a job's input left behind).
    fn decode_granule_once(part: &Arc<OpenPart>, binding: &'static pgrc_read::CodecBinding<'static>, schemas: &[ColSchema], g: u32, cols: &mut [ColBuf], residency: Residency) -> PgResult<u32> {
        decode_granule(&mut PartCursors::new(part, residency), binding, schemas, g, cols, None)
    }

    /// `decode_granule` (the row cache's and compaction's decode, its cursors dropped when it returns), in its bulk
    /// (`Private`) and default (`Cached`) postures.
    /// Spec §8.9 (loadstats D): a sealed part carries one Sample section, flagged `SECTION_OPTIONAL` (an older reader
    /// skips it, spec §5.2: kind 14 is unknown there), and the rows it serves are the part's own rows at the stored
    /// ordinals, each datum image byte-equal to the value the row holds. A `pick` that declines serves nothing.
    #[ignore = "V5SYNC r14 FOLLOWUP: kind-14 Sample section not written by the line's streamed seal (PGRCSERVE M4 / post-switch)"]
    #[test]
    fn stored_sample_serves_the_rows_at_its_ordinals() {
        use pgrc_format::part::{SectionKind, SECTION_OPTIONAL};
        use pgrc_format::sample::{SampleRef, SAMPLE_ROWS};
        let vals = values(NDV);
        let bytes = part_bytes(&vals);
        let part = Arc::new(OpenPart::open(Box::new(MemPartIo::new(bytes, 0x4D33_4801_0000_0003, 4344)), &PartExpect::none()).expect("open part"));
        let idx = part.find_section(SectionKind::Sample, 0, 0).expect("the part has a Sample section");
        let entry = part.sections()[idx];
        assert_ne!(entry.flags & SECTION_OPTIONAL, 0, "Sample is OPTIONAL");
        let older = pgrc_format::part::SectionEntry { kind: 63, ..entry };
        assert_eq!(older.known_kind(), Ok(None), "a reader that does not know the kind skips it");

        let body = crate::footer::meta_section_body(&part, idx).expect("body");
        let mut words = vec![0u64; body.len().div_ceil(8)];
        // SAFETY: `words` holds at least `body.len()` bytes.
        let b = unsafe { core::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), body.len()) };
        b.copy_from_slice(&body);
        let sref = SampleRef::parse(b).expect("parse");
        assert_eq!(sref.n(), SAMPLE_ROWS as usize);
        let ords: Vec<u32> = (0..sref.n()).map(|i| sref.ordinal(i)).collect();
        assert!(ords.windows(2).all(|w| w[0] < w[1]) && (*ords.last().unwrap() as usize) < ROWS);

        let pick_every = |n: usize| Some((0..n).collect::<Vec<usize>>());
        let mut got: Vec<Vec<u8>> = Vec::new();
        let served = super::stored_sample_rows(&part, &[schema()], &[0], &mut |n| pick_every(n), &mut |v, n| {
            assert!(!n[0]);
            let p = v[0].as_u64() as *const u8;
            // SAFETY: a by-reference datum of the stored image: the 4-B header then the payload.
            let head = unsafe { core::ptr::read_unaligned(p as *const [u8; 4]) };
            let len = (u32::from_le_bytes(head) >> 2) as usize;
            got.push(unsafe { core::slice::from_raw_parts(p, len) }.to_vec());
            Ok(())
        })
        .expect("serve");
        assert!(served);
        assert_eq!(got.len(), ords.len());
        for (i, &o) in ords.iter().enumerate() {
            assert_eq!(got[i], image(&vals[o as usize % NDV]), "stored row {i} (part row {o})");
        }
        let declined = super::stored_sample_rows(&part, &[schema()], &[0], &mut |_| None, &mut |_, _| panic!("emitted"));
        assert!(!declined.expect("decline"));
    }

    /// D4372 (born red): ANALYZE's parallel sample workers decode and copy on the statement's memory ledger account, so
    /// the bytes of every sampled column a worker keeps count where the ledger, R6's victim size and the ladder see them.
    /// Before, a worker's thread had no account: what it kept for the backend reached no statement, and its later free
    /// on the backend thread drove the statement's account below what it held. Several windows, so the workers take tasks.
    #[test]
    fn d4372_the_sample_workers_bytes_count_on_the_statements_account() {
        let vals = values(NDV);
        let part = Arc::new(OpenPart::open(Box::new(MemPartIo::new(part_bytes(&vals), 0x4D33_4801_0000_0004, 4345)), &PartExpect::none()).expect("open part"));
        let window = |n: usize| -> Vec<super::SamplePart> {
            (0..n)
                .map(|idx| super::SamplePart {
                    idx,
                    part: Arc::clone(&part),
                    granules: (0..2u32)
                        .map(|g| {
                            let rows = pgrc_format::geom::rows_in_granule_at(part.rows(), part.grain(), g);
                            super::SampleGranule { g, rows, sel: (0..rows as u16).collect() }
                        })
                        .collect(),
                })
                .collect()
        };
        let _serial = crate::session::tests::bind_serial();
        memledger::bind_backend(1, 4372);
        memledger::statement_begin();
        let a = memledger::current();
        let mut short = Vec::new();
        for _ in 0..6 {
            let parts = window(16);
            memledger::fold();
            let before = memledger::account_row(a).account_bytes;
            let cols = super::sample_tasks(&parts, &[schema()], &[0], codec_binding(), 4).expect("sample");
            memledger::fold();
            let after = memledger::account_row(a).account_bytes;
            let kept: i64 = cols.iter().map(|c| (c.vals.capacity() * size_of::<Option<u64>>() + c.bytes.capacity() * 8) as i64).sum();
            assert_eq!(cols.iter().map(|c| c.vals.len()).sum::<usize>(), 16 * ROWS, "every sampled row");
            if after - before < kept {
                short.push((kept, after - before));
            }
            drop(cols);
        }
        memledger::statement_end();
        memledger::unbind_backend();
        assert!(short.is_empty(), "(kept by the tasks, counted on the statement) per short window: {short:?}");
    }

    /// (c) a test pool: `width` std threads claim the window's tasks in a SHUFFLED order (a different claim order from
    /// this thread's), at most `width` at once; it records the width it was asked for.
    struct TestPool {
        asked: pgsync::Mutex<Vec<usize>>,
    }

    struct TestRg(Arc<std::sync::atomic::AtomicU8>);

    impl super::SampleRg for TestRg {
        fn try_done(&self) -> Option<bool> {
            match self.0.load(std::sync::atomic::Ordering::Acquire) {
                0 => None,
                v => Some(v == 1),
            }
        }
        fn abort(&self) {}
    }

    impl super::SamplePool for TestPool {
        fn available(&self) -> bool {
            true
        }
        fn submit(&self, ntasks: usize, width: usize, run: Arc<dyn Fn(usize) + Send + Sync>) -> Box<dyn super::SampleRg> {
            self.asked.lock().unwrap().push(width);
            let state = Arc::new(std::sync::atomic::AtomicU8::new(0));
            let mut order: Vec<usize> = (0..ntasks).collect();
            let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ ntasks as u64;
            for i in (1..order.len()).rev() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                order.swap(i, (x % (i as u64 + 1)) as usize);
            }
            let order = Arc::new(order);
            let next = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let st = Arc::clone(&state);
            std::thread::spawn(move || {
                std::thread::scope(|s| {
                    for _ in 0..width {
                        let (order, next, run) = (Arc::clone(&order), Arc::clone(&next), Arc::clone(&run));
                        s.spawn(move || loop {
                            let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if k >= order.len() {
                                break;
                            }
                            run(order[k]);
                        });
                    }
                });
                st.store(1, std::sync::atomic::Ordering::Release);
            });
            Box::new(TestRg(state))
        }
    }

    /// (c) the rig: a part of the module's values and `n` window parts over its first two granules (every row
    /// sampled).
    fn pool_parts(n: usize) -> Vec<super::SamplePart> {
        let vals = values(NDV);
        let part = Arc::new(OpenPart::open(Box::new(MemPartIo::new(part_bytes(&vals), 0x4D33_4801_0000_0005, 4346)), &PartExpect::none()).expect("open part"));
        (0..n)
            .map(|idx| super::SamplePart {
                idx,
                part: Arc::clone(&part),
                granules: (0..2u32)
                    .map(|g| {
                        let rows = pgrc_format::geom::rows_in_granule_at(part.rows(), part.grain(), g);
                        super::SampleGranule { g, rows, sel: (0..rows as u16).collect() }
                    })
                    .collect(),
            })
            .collect()
    }

    /// Every sampled value of a window, as bytes (by-reference images copied out), task by task.
    fn sampled(cols: &[super::SampleCol]) -> Vec<Vec<Option<Vec<u8>>>> {
        cols.iter()
            .map(|c| {
                (0..c.vals.len())
                    .map(|k| {
                        c.value(k).map(|d| {
                            if c.byref {
                                // SAFETY: the image lives in `c.bytes` (a 4B-U varlena of the module's text column).
                                let p = d.as_u64() as *const u8;
                                let len = unsafe { types_tuple::varatt::varsize_any(p) };
                                unsafe { core::slice::from_raw_parts(p, len) }.to_vec()
                            } else {
                                d.as_u64().to_le_bytes().to_vec()
                            }
                        })
                    })
                    .collect()
            })
            .collect()
    }

    /// (c) SAMPLE ON THE POOL: the window's tasks as morsels on a pool (claimed in a shuffled order, several at once)
    /// give the same table, task for task and value for value, as the tasks run in order on this thread; the pool is
    /// asked for exactly min(dop, tasks) (the GUC / the ladder's answer for the window is the RG's width cap: a forced
    /// low answer of 2 asks for 2, and the test pool, like the runtime's width cap, never runs more at once).
    #[test]
    fn sample_pool_road_matches_this_thread_and_asks_the_width_cap() {
        let parts = pool_parts(24);
        let (want, one) = super::sample_tasks_on(None, &parts, &[schema()], &[0], codec_binding(), 8, 1).expect("serial");
        assert_eq!(one, 1);
        assert_eq!(want.iter().map(|c| c.vals.len()).sum::<usize>(), 24 * ROWS);
        let want = sampled(&want);
        let pool = TestPool { asked: pgsync::Mutex::new(Vec::new()) };
        // (dop, tasks a morsel): one task a morsel and several (shared buffers), 24 tasks
        for (dop, per) in [(2usize, 1usize), (3, 1), (8, 1), (64, 1), (3, 5), (8, 4)] {
            let (got, peak) = super::sample_tasks_on(Some(&pool), &parts, &[schema()], &[0], codec_binding(), dop, per).expect("pool");
            assert!(peak >= 1 && peak <= dop.min(24usize.div_ceil(per)), "dop {dop} per {per}: {peak} morsels in flight");
            assert!(sampled(&got) == want, "dop {dop} per {per}: the pool road's table differs from this thread's");
        }
        assert_eq!(*pool.asked.lock().unwrap(), vec![2, 3, 8, 24, 3, 6], "the width each window asked the pool for");
        // dop 1 (the ladder's serial answer): no pool at all
        let (got, one) = super::sample_tasks_on(Some(&pool), &parts, &[schema()], &[0], codec_binding(), 1, 1).expect("dop 1");
        assert_eq!((one, pool.asked.lock().unwrap().len()), (1, 6));
        assert!(sampled(&got) == want);
    }

    /// (c) with D4372 kept: the pool's morsels decode and copy on the statement's ledger account (what the sampled
    /// columns keep counts there), and once they are dropped the account is back where it started.
    #[test]
    fn sample_pool_morsels_count_on_the_statement_and_return_to_start() {
        let parts = pool_parts(16);
        let pool = TestPool { asked: pgsync::Mutex::new(Vec::new()) };
        let _serial = crate::session::tests::bind_serial();
        memledger::bind_backend(2, 4373); // its own account: the D4372 test above binds procno 1 and the tests run at once
        memledger::statement_begin();
        let a = memledger::current();
        // one window first: what the part keeps resident once read is the part's, not a window's
        drop(super::sample_tasks_on(Some(&pool), &parts, &[schema()], &[0], codec_binding(), 4, 2).expect("warm"));
        memledger::fold();
        let start = memledger::account_row(a).account_bytes;
        let mut short = Vec::new();
        for _ in 0..4 {
            memledger::fold();
            let before = memledger::account_row(a).account_bytes;
            let (cols, _) = super::sample_tasks_on(Some(&pool), &parts, &[schema()], &[0], codec_binding(), 4, 2).expect("pool");
            memledger::fold();
            let after = memledger::account_row(a).account_bytes;
            let kept: i64 = cols.iter().map(|c| (c.vals.capacity() * size_of::<Option<u64>>() + c.bytes.capacity() * 8) as i64).sum();
            if after - before < kept {
                short.push((kept, after - before));
            }
            drop(cols);
        }
        memledger::fold();
        let end = memledger::account_row(a).account_bytes;
        memledger::statement_end();
        memledger::unbind_backend();
        assert!(short.is_empty(), "(kept by the morsels, counted on the statement) per short window: {short:?}");
        // back to the start but for the pool's own bookkeeping (the test pool's thread handles and claim order, the
        // task closure: small, made here and freed on its threads); one window's columns are ~20 MB
        assert!((end - start).abs() < 16 << 10, "the statement's account after the windows' columns are dropped: {start} -> {end}");
    }

    #[test]
    fn dict_datums_outlive_decode_granule_on_a_multi_extent_payload() {
        dict_datums_outlive(NDV, decode_granule_once, Residency::Private, ColBuf::new(), "decode_granule");
        dict_datums_outlive(NDV, decode_granule_once, Residency::Cached, ColBuf::new(), "decode_granule");
    }

    /// The row scan's part-lived cursors (D777), dropped while the `ColBuf`s still hold the granule.
    #[test]
    fn dict_datums_outlive_the_row_scans_cursors_on_a_multi_extent_payload() {
        dict_datums_outlive(NDV, scan_decode_granule, Residency::Private, ColBuf::unallocated(), "the row scan's decode");
        dict_datums_outlive(NDV, scan_decode_granule, Residency::Cached, ColBuf::unallocated(), "the row scan's decode");
    }

    /// C1 L5b: a one-extent payload under `Residency::Cached` is the part's cached image itself. Only the `ColBuf`'s
    /// guard keeps it once the cursor is gone, so the sweep (as if the cap were 0) must not take it; once the guard
    /// lets go, it does.
    #[test]
    fn dict_datums_outlive_a_cache_sweep_on_a_one_extent_payload() {
        dict_datums_outlive(SMALL_NDV, decode_granule_once, Residency::Cached, ColBuf::new(), "decode_granule");
        dict_datums_outlive(SMALL_NDV, scan_decode_granule, Residency::Cached, ColBuf::unallocated(), "the row scan's decode");
    }

    fn dict_datums_outlive(ndv: usize, decode: Decode, residency: Residency, fresh: ColBuf, what: &str) {
        let vals = values(ndv);
        let bytes = part_bytes(&vals);
        let part = Arc::new(OpenPart::open(Box::new(MemPartIo::new(bytes, 0x4D33_4801_0000_0002, 4343)), &PartExpect::none()).expect("open part"));

        // The shape under test: dictionary codes over an unwrapped payload, cut into one extent per frame.
        let sd = part.stream_directory().expect("stream directory");
        let shape: Vec<String> = sd
            .streams()
            .iter()
            .map(|s| format!("role={} enc={} wrapper={} values={} extents={}", s.role as u8, s.entry.encoding, s.entry.wrapper, s.entry.values, s.extents.len()))
            .collect();
        let values_stream = sd.lookup(1, 0, StreamRole::Values).expect("values stream");
        assert_eq!(values_stream.entry.encoding, EncodingId::DictCodes.as_u16(), "the dictionary arm won: {shape:?}");
        let payload = sd.lookup(1, 0, StreamRole::DictPayload).expect("dict payload stream");
        assert_eq!(payload.entry.wrapper, 0, "the payload stays unwrapped: {shape:?}");
        assert_eq!(payload.extents.len() > 1, ndv > 1024, "the payload is cut into one extent per frame: {shape:?}");
        let region_words = (payload.extents.iter().map(|e| e.len).sum::<u64>() as usize).div_ceil(8);

        let mut cols = vec![fresh];
        let rows = decode(&part, codec_binding(), &[schema()], 0, &mut cols, residency).expect("decode granule 0");
        assert_eq!(rows, pgrc_format::geom::GRANULE_ROWS);
        // C1 L5: everything nothing else holds leaves the cache, as if the cap were 0.
        pgrc_read::mem::sweep_all_for_tests();
        let held = part.resident();

        // Reuse whatever the decode freed: same-sized allocations (an exact fit takes the freed chunk back first)
        // filled with a pattern no 4B-U header can have (its low two bits are set).
        let reuse: Vec<Vec<u64>> = (0..16).map(|_| vec![0x5A5A_5A5A_5A5A_5A5Bu64; region_words]).collect();

        let mut bad = 0usize;
        let mut first: Option<String> = None;
        for r in 0..rows as usize {
            let want = image(&vals[r % ndv]);
            let Some(ptr) = col_cell(&cols, 0, r) else {
                bad += 1;
                first.get_or_insert_with(|| format!("row {r}: NULL"));
                continue;
            };
            // SAFETY: the datum is the row scan's by-reference word; reading its header is what `store_cols`'s
            // consumer does. A dangling word reads freed (still mapped) heap here; the header check stops before
            // any payload read past it.
            let head = unsafe { core::ptr::read_unaligned(ptr as *const [u8; 4]) };
            if head[..] != want[..4] {
                bad += 1;
                first.get_or_insert_with(|| format!("row {r}: header {head:02x?}, want {:02x?}", &want[..4]));
                continue;
            }
            // SAFETY: as above; the header matched, so `want.len()` bytes are the entry.
            let got = unsafe { core::slice::from_raw_parts(ptr as *const u8, want.len()) };
            if got != &want[..] {
                bad += 1;
                first.get_or_insert_with(|| format!("row {r}: payload differs"));
            }
        }
        drop(reuse);
        assert_eq!(bad, 0, "{bad} of {rows} dictionary datums no longer read their value after {what} ({residency:?}) returned; first: {}", first.unwrap_or_default());
        // Let go by the guard, a cached payload goes at the next sweep.
        drop(cols);
        pgrc_read::mem::sweep_all_for_tests();
        let cached_payload = ndv <= 1024 && residency == Residency::Cached;
        assert_eq!(part.resident() < held, cached_payload, "{what} ({residency:?}): resident {} after the guard let go, {held} before", part.resident());
    }
}

/// A row's slice ([`decode_one_row`]) whose by-reference value decodes past 512 MiB: the slice arena grows by the
/// granule path's rule (reserved fallibly by [`grow_arena`], no cap) and the row reads its whole value, as do the rows
/// around it through the same cursor and arena. A verbatim (never dictionary-coded) varlena is built in the arena
/// (the arena law); a dictionary-coded one would alias its payload instead, so the test checks the part has none.
/// About 2-2.5 GiB peak (the arena doubling to 1 GiB, the 512 MiB it grew from, the value's image): #[ignore]d out of
/// the parallel unit set and run alone by the heavy-unit leg.
#[cfg(test)]
mod slice_arena_tests {
    use super::{codec_binding, decode_one_row, PartCursors, SliceBuf};
    use pgrc_format::class::{ColSchema, CollationClass, StorageClass, TypeSemantics};
    use pgrc_format::part::StreamRole;
    use pgrc_read::{MemPartIo, OpenPart, PartExpect, Residency};
    use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture};
    use pgrc_write::ingest::{NoExternalDetoast, RawDatum};
    use pgrc_write::publish::{TxnProbe, TxnVerdict};
    use pgrc_write::seal::CodecResolver;
    use pgrc_write::shred::NoShred;
    use pgrc_write::writer::{PartCutPolicy, SealEnv, SubxactEvidence, TableWriter, TxnStamp};
    use pgrc_write::wvfs::{MemVfs, WriteVfs};
    use std::sync::Arc;

    /// The big value's payload: past 512 MiB by a block and a partial block.
    const BIG: usize = (512 << 20) + 4096 + 77;
    const BLOCK: usize = 4096;

    struct AllCommitted;
    impl TxnProbe for AllCommitted {
        fn verdict(&self, _fxid: u64) -> TxnVerdict {
            TxnVerdict::Committed
        }
    }

    fn schema() -> ColSchema {
        ColSchema {
            attno: 1,
            class: StorageClass::VarlenaVerbatim,
            typlen: -1,
            typbyval: false,
            typalign: b'i',
            collation_class: CollationClass::C,
            semantics: TypeSemantics::TextCollated,
        }
    }

    /// One block of xorshift bytes; the big payload repeats it, its last copy cut short.
    fn block() -> Vec<u8> {
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        (0..BLOCK)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    fn image(payload: &[u8]) -> Vec<u8> {
        let mut img = pgrc_format::wire::varlena_header_4b_u(payload.len() as u32).to_le_bytes().to_vec();
        img.extend_from_slice(payload);
        img
    }

    fn big_image(b: &[u8]) -> Vec<u8> {
        let mut img = Vec::with_capacity(4 + BIG);
        img.extend_from_slice(&pgrc_format::wire::varlena_header_4b_u(BIG as u32).to_le_bytes());
        while img.len() - 4 + BLOCK <= BIG {
            img.extend_from_slice(b);
        }
        let rest = BIG - (img.len() - 4);
        img.extend_from_slice(&b[..rest]);
        img
    }

    /// Seal one part of the rows `small`, the big value, `tail` (no dictionary offered).
    fn part_bytes(b: &[u8]) -> Vec<u8> {
        let mut vfs = MemVfs::new();
        vfs.mkdir_path("t").expect("mkdir");
        let mut w = TableWriter::open(
            "t".to_string(),
            vec![schema()],
            1663,
            5,
            779,
            TxnStamp { fxid: 42, cid: 1 },
            &SubxactEvidence::default(),
            PartCutPolicy::default(),
        )
        .expect("open");
        let cands = CodecCandidates::new(ColumnPosture::default());
        let sources: [&dyn CandidateSource; 1] = [&cands];
        let resolver = CodecResolver;
        let mut shred = NoShred;
        let opts = pgrc_format::relopt::ShredOptions::default();
        let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &resolver, shred: &mut shred, shred_opts: &opts };
        w.append_row(&[RawDatum::Bytes(&image(b"small"))], &mut NoExternalDetoast, &mut env).expect("append small");
        {
            let big = big_image(b);
            w.append_row(&[RawDatum::Bytes(&big)], &mut NoExternalDetoast, &mut env).expect("append big");
        }
        w.append_row(&[RawDatum::Bytes(&image(b"tail"))], &mut NoExternalDetoast, &mut env).expect("append tail");
        w.finish(&mut env).expect("finish");
        let out = w.publish(&mut vfs, &AllCommitted).expect("publish");
        assert_eq!(out.part_nos.len(), 1, "one part");
        vfs.read_full(&format!("t/{}", pgrc_format::dirlayout::part_file_name(out.part_nos[0]))).expect("part bytes")
    }

    /// The varlena a by-reference datum word points at: its header, then `len` payload bytes.
    ///
    /// SAFETY (callers): `w` is a datum `decode_one_row` returned for a 4B-U value of `len` payload bytes, read before
    /// the column's next slice.
    unsafe fn varlena<'a>(w: u64, len: usize) -> ([u8; 4], &'a [u8]) {
        let p = w as usize as *const u8;
        (core::ptr::read_unaligned(p as *const [u8; 4]), core::slice::from_raw_parts(p.add(4), len))
    }

    #[test]
    #[ignore = "2 GB peak; run by the heavy-unit leg"]
    fn a_slice_past_512_mib_grows_its_arena_and_reads_its_value() {
        let b = block();
        let bytes = part_bytes(&b);
        let part = Arc::new(OpenPart::open(Box::new(MemPartIo::new(bytes, 0x4D33_4801_0000_0003, 4344)), &PartExpect::none()).expect("open part"));
        let sd = part.stream_directory().expect("stream directory");
        assert!(sd.lookup(1, 0, StreamRole::DictPayload).is_none(), "verbatim: no dictionary payload the datum could alias");
        let s = schema();
        let mut pc = PartCursors::new(&part, Residency::Cached);
        let mut buf = SliceBuf::new();

        let w = decode_one_row(&mut pc, codec_binding(), 1, 0, &s, 0, 1, &mut buf).expect("the big row decodes").expect("not NULL");
        assert!(buf.arena.len() * 8 > 512 << 20, "the slice arena grew past 512 MiB: {} bytes", buf.arena.len() * 8);
        // SAFETY: the big row's datum, read before the next slice.
        let (head, got) = unsafe { varlena(w, BIG) };
        assert_eq!(head, pgrc_format::wire::varlena_header_4b_u(BIG as u32).to_le_bytes(), "the big value's header");
        let bad = got.chunks(BLOCK).position(|c| c != &b[..c.len()]);
        assert_eq!(bad, None, "the big value's payload, block by block");

        for (row, want) in [(0u32, &b"small"[..]), (2, &b"tail"[..])] {
            buf.release();
            let w = decode_one_row(&mut pc, codec_binding(), 1, 0, &s, 0, row, &mut buf).expect("decodes").expect("not NULL");
            // SAFETY: this row's datum, read before the next slice.
            let (head, got) = unsafe { varlena(w, want.len()) };
            assert_eq!(head, pgrc_format::wire::varlena_header_4b_u(want.len() as u32).to_le_bytes(), "row {row}'s header");
            assert_eq!(got, want, "row {row}");
        }
    }
}

#[cfg(test)]
mod live_position_tests {
    use super::{masked_count, nth_live, part_live_positions, sample_position, SEGMENT_POSITION};
    use pgrc_format::geom::{rows_in_granule_at, GranuleGrain, GRANULE_ROWS};
    use pgrc_scan::DV_GRANULE_WORDS;

    type Mask = [u64; DV_GRANULE_WORDS];

    fn lcg(s: &mut u64) -> u64 {
        *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *s >> 33
    }

    fn random_mask(s: &mut u64, n: u32, every: u64) -> Box<Mask> {
        let mut m = Box::new([0u64; DV_GRANULE_WORDS]);
        for r in 0..n {
            if lcg(s) % every == 0 {
                m[(r / 64) as usize] |= 1 << (r % 64);
            }
        }
        m
    }

    /// Every live row's position in `getnextslot` order.
    fn brute(first: u64, rows: u64, grain: GranuleGrain, granules: u32, masks: &[Option<Box<Mask>>]) -> Vec<u64> {
        let mut out = Vec::new();
        for g in 0..granules {
            for r in 0..rows_in_granule_at(rows, grain, g) {
                let deleted = masks[g as usize].as_ref().is_some_and(|m| (m[(r / 64) as usize] >> (r % 64)) & 1 == 1);
                if !deleted {
                    out.push(sample_position(first + g as u64, r));
                }
            }
        }
        out
    }

    /// D3181: the walker maps live ordinals to the positions a walk of the rows reaches, over granules with no
    /// deletions, with a delete vector, and with a delete vector OR'd with the snapshot's tombstones (the scan's
    /// `granule_deletions`), at the grains a part may elect.
    #[test]
    fn walker_matches_the_row_stream_under_delete_vectors_and_tombstones() {
        let mut s = 7u64;
        for &(rows, grain_rows, first) in &[(1u64, 8192u32, 0u64), (8192, 8192, 3), (20_000, 8192, 5), (9_000, 1024, 0), (4_097, 4096, 11)] {
            let grain = GranuleGrain::from_rows(grain_rows).unwrap();
            let granules = rows.div_ceil(grain_rows as u64) as u32;
            for (dv, tombs) in [(false, false), (true, false), (false, true), (true, true)] {
                let masks: Vec<Option<Box<Mask>>> = (0..granules)
                    .map(|g| {
                        let n = rows_in_granule_at(rows, grain, g);
                        let dv = (dv && g % 2 == 0).then(|| random_mask(&mut s, n, 3));
                        let tomb = (tombs && g % 3 != 1).then(|| random_mask(&mut s, n, 5));
                        match (dv, tomb) {
                            (None, None) => None,
                            (Some(m), None) | (None, Some(m)) => Some(m),
                            (Some(mut a), Some(b)) => {
                                a.iter_mut().zip(b.iter()).for_each(|(x, y)| *x |= y);
                                Some(a)
                            }
                        }
                    })
                    .collect();
                let all = brute(first, rows, grain, granules, &masks);
                let ords: Vec<u64> = (0..all.len() as u64).filter(|_| lcg(&mut s) % 4 == 0).collect();
                let mut got = Vec::new();
                part_live_positions(first, rows, grain, granules, |g| masks[g as usize].clone(), &ords, &mut got);
                let want: Vec<u64> = ords.iter().map(|&o| all[o as usize]).collect();
                assert_eq!(got, want, "rows {rows} grain {grain_rows} dv {dv} tombstones {tombs}");
                let live: u64 = (0..granules)
                    .map(|g| {
                        let n = rows_in_granule_at(rows, grain, g);
                        n as u64 - masks[g as usize].as_deref().map_or(0, |m| masked_count(m, n, true))
                    })
                    .sum();
                assert_eq!(live, all.len() as u64, "the live count is the walk's");
            }
        }
    }

    #[test]
    fn mask_counts_read_only_the_granule_rows() {
        let mut m = [0u64; DV_GRANULE_WORDS];
        m[0] = 0b1011;
        m[DV_GRANULE_WORDS - 1] = 1 << 63;
        assert_eq!(masked_count(&m, 70, true), 3);
        assert_eq!(masked_count(&m, 70, false), 67);
        assert_eq!(masked_count(&m, GRANULE_ROWS, true), 4);
        assert_eq!(nth_live(&m, 70, 0), 2);
        assert_eq!(nth_live(&m, 70, 1), 4);
        assert_eq!(nth_live(&m, 70, 66), 69);
    }

    /// D3181: a part row's position never reaches the segment rows' (a part granule's number is far below 2^63 /
    /// GRANULE_ROWS), so the positions of a stream (parts, then segments) ascend.
    #[test]
    fn part_positions_stay_below_the_segment_positions() {
        assert!(sample_position(u32::MAX as u64 * 1024, GRANULE_ROWS - 1) < SEGMENT_POSITION);
        assert!(SEGMENT_POSITION < (SEGMENT_POSITION | 1));
    }
}
