//! The ONE seal implementation (chunk M3-D row): chunk stats → election →
//! encode → footer/meta assembly → part file.
//!
//! One part = one call to [`seal_part`]. The part image is assembled
//! deterministically (part bytes are a pure function of (schema, elections,
//! input row partition) — spec §1) and written to a `tmp-<fxid>-<seq>.pgrct`
//! file, HEADER FIRST (spec §5: a crashed partial file is identifiable as
//! pgrc and never readable — the tmp name additionally keeps it out of
//! every reader's namespace until publish renames it). `part_no` is 0 in the
//! sealed image and is patched at publish (spec §5.1: assigned at publish,
//! monotone).
//!
//! Laws enforced on this path, every seal, no opt-outs:
//!
//! - **`verify_roundtrip` per granule per value stream** (spec §19.6, the
//!   election quadruple's fixed leg): the just-encoded section is decoded
//!   through the SAME vtable a reader resolves and compared value-by-value.
//!   Failure is a typed [`WriteError::RoundTrip`] — the part never reaches
//!   a file.
//! - **Two-witness null law** (spec §6.6): every granule/band/part stats
//!   record's `nonnull` is cross-checked against the EMITTED validity
//!   bitmap's popcount (not the input's — the emitted bytes are the second
//!   witness). Skew is a typed refusal; the check count is witnessed in the
//!   [`SealReport`].
//! - **O(streams-touched) discipline**: sealing reads nothing back but the
//!   in-memory section images it just wrote; no whole-part re-reads, no
//!   file reads at all before publish.
//!
//! File section order: per-stream value extents (per band) with their
//! overflow extents, then validity streams, then per-stream Stats (+ any
//! builder aux sections), PathTable (when shredded), SortKey, StreamDir,
//! the section table, FooterFixed, PartTail. Sections start 8-aligned
//! (writer law, spec §1 — makes entry alignment absolute).

use pgrc_format::abi::{
    verify_roundtrip, ByteArena, CodecVtable, ColumnMetaBuilder, EncodeInput, GranuleEncoder,
    KernelCtx, KernelKey,
};
use pgrc_format::class::{ColSchema, StorageClass, CLASS_BYVAL};
use pgrc_format::enc::{EncodingId, Wrapper};
use pgrc_format::geom::{self, GranuleGrain, GRANULES_PER_BAND, GRANULE_ROWS};
use pgrc_format::rowid::MAX_GRANULES_PER_PART;
use pgrc_format::meta::StatsRecord;
use pgrc_format::part::{
    ExtentRecord, FooterFixed, OverflowSink, PartHeader, PartTail, SectionEntry, SectionKind,
    StreamCloseout, StreamEntry, StreamRole, StreamSectionHdr, StreamSectionWriter, FOOTER_MAGIC,
    SECTIONF_META_ZSTD, STREAMF_HAS_OVERFLOW, STREAMF_SIGNED,
};
use pgrc_format::sortkey::{NullsOrder, SortDir, SortKeyEntry, SortKeyRecord, SORT_KEY_KIND_TEXT_PATH};
use pgrc_format::verbatim::{encode_validity_bitmap, reference_vtables, VerbatimEncoder};
use pgrc_format::wire::{pad_to, varlena_entry_at};

use pgrc_format::dict::{DictCharLenForm, DictSections};
use pgrc_codec::wrapper::wrapper_available;

use pgrc_codec::arraydual::{
    assemble_array_datums, assembled_arena_bytes, elect_array_split, ArrayElemFacts, ArraySplit,
};

use crate::held::{cap_bytes, HeldCharge};
use memledger::{ChargedVec, Kind};
use crate::elect::{
    elect_stream, verbatim_baseline_len, wins_twenty_pct, CandidateSource, Elected, ElectPlan,
    ElectionWitness, ExtentShape, FullElectInput, FullElection,
};
use crate::ingest::ColBuffer;
use crate::meta_standin::StandinMetaBuilder;
use crate::shred::{encode_path_table, validate_lanes, ShredLane, ShredLaneSource};
use crate::structural::{ClusterKeyDecl, StructuralPolicy};
use crate::wvfs::{WriteVfs, WvFd};
use crate::{WriteError, WriteResult};
use pgrc_format::relopt::ShredOptions;
use pgrc_format::sidecar::ColDistribution;

/// Part-level identity facts (schema fingerprint per spec §5.5, computed by
/// the writer once per table via `pgrc_format::ident::schema_fingerprint`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartSpec {
    pub spc: u32,
    pub db: u32,
    pub relfilenumber: u64,
    pub schema_fingerprint: u64,
}

/// Resolves decode vtables for round-trip verification. M3-C's kernels join
/// through a composite resolver; the reference resolver serves the M3-D
/// election set (VERBATIM/CONST).
pub trait VerifyResolver {
    fn resolve(&self, key: KernelKey) -> WriteResult<&'static CodecVtable>;
}

/// Resolver over the frozen reference vtables.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReferenceResolver;

impl VerifyResolver for ReferenceResolver {
    fn resolve(&self, key: KernelKey) -> WriteResult<&'static CodecVtable> {
        for vt in reference_vtables() {
            if vt.key == key {
                return Ok(vt);
            }
        }
        Err(WriteError::Format(
            pgrc_format::FormatError::KernelMissing {
                encoding: key.encoding,
                class: key.class,
                width: key.width,
            },
        ))
    }
}

/// Resolver over the full codec-family registry (M3-C's kernels + the
/// reference vtables): the production resolver for full-registry seals —
/// verify resolves exactly the vtable a reader will.
#[derive(Debug, Default, Clone, Copy)]
pub struct CodecResolver;

impl VerifyResolver for CodecResolver {
    fn resolve(&self, key: KernelKey) -> WriteResult<&'static CodecVtable> {
        pgrc_codec::registry()
            .resolve(key)
            .map_err(WriteError::Format)
    }
}

/// A sealed (unpublished) part: the facts publish needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedPart {
    /// File name within the table directory (`tmp-<fxid>-<seq>.pgrct`).
    pub tmp_name: String,
    pub rows: u64,
    pub file_len: u64,
    pub footer_off: u64,
    pub granule_count: u32,
    pub band_count: u32,
    /// The sealed 96-B footer image (part_no still 0). Publish patches
    /// part_no through this copy — no file read-back, O(streams-touched)
    /// preserved.
    pub footer_image: Vec<u8>,
    /// ST-1 (OD-2): the encoded Stats-sidecar payload (per-stream
    /// distribution sketches, `pgrc_format::sidecar::encode_stats_payload`
    /// form). Empty = no stream computed one. Publish writes it as the
    /// part's `stats` sidecar companion — stats are a SEAL byproduct, so
    /// "banks ship stats-built" is structural, never a follow-up pass.
    pub stats_payload: Vec<u8>,
    /// [fmt-land] Bank-grain stats-plane slices, captured AS THE PART
    /// SEALS (the user 2026-08-16: "we should generate it as we copy data
    /// in"): per top-level column (path_ord 0), the UNWRAPPED §8.1 Stats
    /// body + the 24-B PartDigest record byte-for-byte as sealed into the
    /// part. Publish folds them into the bank's `bankstats-<gen>.pgrcbs`
    /// sidecar (`bankplane.rs`) — a derived artifact, byte-identical to an
    /// offline rebuild from the sealed parts by construction (same bodies,
    /// same attno order).
    pub plane_slices: Vec<PlaneSlice>,
    /// The facts fold's per-column inputs beyond the Stats body (`facts.rs`:
    /// the planner facts file the publisher writes beside the manifest),
    /// captured as the part seals; attno-ascending, top-level columns only.
    pub facts_slices: Vec<FactsSlice>,
    /// IX0: the row IDs the part homes (its `EXT_RANGES` entry; the publish's plan records it). A bulk part's one
    /// range (its rows are `lo + ordinal`), a sealed segment's extents, a compaction output's inputs' ranges. Empty
    /// until the writer's caller grants them ([`crate::writer::TableWriter::grant_ids`], `set_ids`).
    pub ids: pgrc_format::manifest::IdRanges,
}

/// One top-level column's facts-fold inputs (`facts.rs`): the UNWRAPPED
/// NdvRegisters body, the bloom witness, the Values stream's class /
/// signedness / width and dictionary election with its code count (from
/// the stream plans, exactly what a reader finds in the StreamDir), and the
/// column's data-section bytes (from the section table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactsSlice {
    pub attno: u32,
    pub class: u8,
    pub signed: bool,
    pub width: u8,
    pub ndv_body: Option<Vec<u8>>,
    pub bloom: bool,
    pub dict: bool,
    pub ncodes: u64,
    pub disk_bytes: u64,
}

/// [fmt-land] One (column, part) stats-plane slice — the exact bytes the
/// `pgrc_format::bankstats` column payload carries for this part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaneSlice {
    pub attno: u32,
    /// The UNWRAPPED §8.1 Stats section body (granule records ‖ band
    /// records ‖ part record) — captured BEFORE the CMP-F meta wrap.
    pub stats_body: Vec<u8>,
    /// The 24-B PartDigest record bytes; `None` when the digest section
    /// was not sealed (PGRUST_PGRC_PARTDIGEST=0 arm).
    pub digest: Option<[u8; pgrc_format::bankstats::BANKSTATS_DIGEST_LEN]>,
}

/// The seal witnesses (gates prove they RAN, not just that nothing fired).
#[derive(Debug, Default, Clone)]
pub struct SealReport {
    pub elections: Vec<ElectionWitness>,
    /// verify_roundtrip completions (== granules × value streams).
    pub granules_verified: u64,
    /// Two-witness nonnull cross-checks performed (granule+band+part grain).
    pub nonnull_crosschecks: u64,
    /// O-10 logical identity per stream: (attno, path_ord, (lo, hi, rows)).
    pub col_hashes: Vec<(u32, u32, (u64, u64, u64))>,
    /// FT-6 clustered witness (report leg): the declared cluster-key
    /// attnos this part was sorted-and-verified on (empty = undeclared,
    /// OD-8's legal-unlicensed posture). The on-disk leg is the spec-§9
    /// SortKey section with `nkeys > 0`.
    pub cluster_keys: Vec<u32>,
    /// SEAL-FUSION phase timers (measurement witnesses, never part bytes):
    /// the cluster-sort wall (IN-1; the EXT-SORT lane's domain) and the
    /// post-sort seal wall (election + encode + verify + meta + assembly —
    /// the fusion's own phase).
    pub sort_secs: f64,
    pub seal_secs: f64,
    /// Typed census notes minted by the elections (e.g. a
    /// Utf8Chars claim that failed per-part verification and ran
    /// BytesOnly). Measurement witnesses, never part bytes.
    pub notes: Vec<crate::elect::SealNote>,
    /// IXBULKSORT: the declared cluster key's sort permutation (`perm[s]` = the arrival position of the row sealed at
    /// position `s`); `None` when the seal kept arrival order. The writer takes it at the cut
    /// ([`crate::writer::Placed`]); never part bytes.
    pub cluster_perm: Option<Vec<u32>>,
}

pub fn band_count_u64(rows: u64) -> u64 {
    geom::band_count(rows) as u64
}

/// SEAL-SPEED-2 fold-fusion switch (default ON; `PGRUST_SEAL_FUSE_FOLD=0`
/// kills back to the classic encode-then-observe pair). A perf dial, never
/// a bytes dial: witness values are identical either way (the per-row fold
/// sequence is shared code — `observe_granule` delegates to the same
/// streaming face the fused encoders drive). Read once per process.
fn fuse_fold_enabled() -> bool {
    static ON: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("PGRUST_SEAL_FUSE_FOLD").map(|v| v.trim() != "0").unwrap_or(true)
    })
}

/// Wrapper election for a finished BYTE-RUN section (overflow / dict index /
/// dict payload) — CMP-B's wrapper-offer CLASS widening: the value-stream
/// offer (CMP-A, O-CMP-3(a)) extends to the byte-run stream class under the
/// SAME laws. SB-1 (v4 DEFAULT, not an offer): the arm is zstd-3
/// block-grain — the election table of `9b6ff701ae` adopted verbatim (zstd
/// wins every offered family; LZ4 dominated on BOTH axes — worse ratio
/// everywhere AND slower unwrap — so it is no longer priced; the enum id
/// stays reader vocabulary). The wrapped image ships iff it clears the
/// wrapper-layer ≥20% stream-grain law (O-CMP-4(a)). Byte-run sections are
/// single-granule, so the wrapped form is one block; readers rebuild the
/// byte-exact region at first region fault and it stays part-resident
/// (O-CMP-5(a) — the StrView §7b dict-payload region-stability law is
/// preserved by residency, not by refusing the wrapper). Returns `None`
/// when the gate refuses (the section ships unwrapped, exactly as before).
fn wrap_byte_run_section(
    section: &[u8],
    dict: Option<(StreamRole, &[u8], DictCharLenForm)>,
    owner: Option<memledger::Owner>,
) -> WriteResult<Option<(Wrapper, Vec<u8>)>> {
    // pgrc format 2.2 dictionary layers (spec §6.4/§7): DictPayload front-codes
    // each frame beneath zstd against the v1 index; a DictIndex in the
    // Absolute char-len form packs beneath zstd. Both replace plain zstd
    // for their role under the same ≥20% law; the probe-only Delta/Absent
    // index forms keep the role-agnostic zstd arm.
    let dict_arm: Option<(Wrapper, Vec<u8>)> = match dict {
        Some((StreamRole::DictPayload, index, _)) if wrapper_available(Wrapper::FrontZstd) => {
            let mut out = Vec::new();
            pgrc_codec::wrapper::wrap_dict_payload_section(section, index, &mut out)
                .map_err(WriteError::Format)?;
            // the OnPair arm is kept only when its exact image is the smaller one
            match pgrc_codec::onpairwrap::offer_dict_payload(section, index).map_err(WriteError::Format)? {
                Some(op) if op.len() < out.len() => Some((Wrapper::FrontOnPairZstd, op)),
                _ => Some((Wrapper::FrontZstd, out)),
            }
        }
        Some((StreamRole::DictIndex, _, DictCharLenForm::Absolute))
            if wrapper_available(Wrapper::PackedZstd) =>
        {
            let mut out = Vec::new();
            pgrc_codec::wrapper::wrap_dict_index_section(section, &mut out)
                .map_err(WriteError::Format)?;
            Some((Wrapper::PackedZstd, out))
        }
        _ => None,
    };
    if let Some((arm, out)) = dict_arm {
        return Ok(wins_twenty_pct(out.len() as u64, section.len() as u64).then_some((arm, out)));
    }
    let hdr = StreamSectionHdr::decode(section).map_err(WriteError::Format)?;
    let payload_end = if hdr.frame_table_off != 0 {
        hdr.frame_table_off as usize
    } else if hdr.gcount_table_off != 0 {
        hdr.gcount_table_off as usize
    } else {
        section.len()
    };
    let payload_len = payload_end
        .checked_sub(pgrc_format::part::STREAM_SECTION_HDR_LEN)
        .ok_or(WriteError::Contract {
            detail: "byte-run section shorter than its header",
        })?;
    // SB-7 under the wrapper: a FRAMED byte-run section (DictPayload — the
    // only byte-run stream carrying a frame table) wraps with ONE zstd
    // block PER DICT FRAME, so the wrapped image's block-offset table is
    // frame-addressable — the partial-decompress geometry the M3-L3
    // frame-grain residency consumes (the whole-section unwrap + image
    // cache stays TODAY's realized answer per P-6; the CMP-E cold cells
    // price the difference). Unframed byte runs (overflow, DictIndex) stay
    // one block.
    let mut ends: ChargedVec<u32> = ChargedVec::new(owner, Kind::Region);
    match hdr.frame_table(section).map_err(WriteError::Format)? {
        Some(offs) if !offs.is_empty() => ends.with_vec(|v| v.extend(offs.iter().skip(1).copied().chain(std::iter::once(payload_len as u32)))), // alloc: Exec charged ends.owner()
        _ => ends.with_vec(|v| v.push(payload_len as u32)), // alloc: Exec charged ends.owner()
    }
    let mut best: Option<(Wrapper, Vec<u8>)> = None;
    for arm in [Wrapper::Zstd] {
        if !wrapper_available(arm) {
            // A build without the codec never elects it (the wasm arm's
            // parity-honest degradation, mirrored from the value path).
            continue;
        }
        let mut out: Vec<u8> = Vec::new();
        pgrc_codec::wrapper::wrap_section(section, &ends, arm, &mut out)
            .map_err(WriteError::Format)?;
        if !wins_twenty_pct(out.len() as u64, section.len() as u64) {
            continue;
        }
        let better = match &best {
            None => true,
            Some((_, b)) => out.len() < b.len(),
        };
        if better {
            best = Some((arm, out));
        }
    }
    Ok(best)
}

/// One stream's directory facts, accumulated during emission.
struct StreamPlan {
    attno: u32,
    path_ord: u32,
    role: StreamRole,
    encoding: u16,
    flags: u16,
    class_id: u8,
    width: u8,
    fixed_len: u32,
    aux32: u32,
    wrapper: u8,
    values: u64,
    extents: Vec<ExtentRecord>,
}

/// Seal one part from accumulated column buffers.
///
/// `roots` are the table's columns in attno order; `lanes` are derived
/// shred substreams (validated row-aligned); `octet` the columns whose
/// octet-length lanes (D3038, `crate::octet`) this function derives after
/// its cluster sort and seals after every other stream; `builders` is
/// parallel to `roots ++ lanes ++ octet` (the §19.7 faces this lane drives —
/// M3-E's real builders replace the stand-ins through the same trait). `structural`
/// carries the caller-declared structural-election facts (TY-1 ArrayDual;
/// see [`StructuralPolicy`]).
///
/// SB-10: this function ELECTS the part's granule grain first — the largest
/// [`geom::GRAIN_LADDER`] grain at which every stream's exact whole-part
/// value bytes average under [`geom::GRANULE_BYTE_BOUND_PROVISIONAL`] per
/// granule (oversize varlenas price at their 16-B inline stub; the ladder
/// floor is taken when even 1024 misses the bound). All granule/band
/// geometry below is closed-form in (rows, grain); the grain lands in the
/// footer.
#[allow(clippy::too_many_arguments)]
pub fn seal_part(
    vfs: &mut dyn WriteVfs,
    table_dir: &str,
    spec: &PartSpec,
    roots: &[ColBuffer],
    lanes: &[ShredLane],
    octet: &[u32],
    builders: &mut [Box<dyn ColumnMetaBuilder>],
    sources: &[&dyn CandidateSource],
    resolver: &dyn VerifyResolver,
    structural: &StructuralPolicy,
    fxid: u64,
    seq: u32,
) -> WriteResult<(SealedPart, SealReport)> {
    seal_part_core(vfs, table_dir, spec, roots, lanes, octet, builders, structural, fxid, seq, None, |sc, builders, sink| {
        for (si, b) in builders.iter_mut().enumerate() {
            let out = seal_stream(sc, si, b.get(), sources, resolver)?;
            b.release();
            sink(out)?;
        }
        Ok(())
    })
}

/// [`seal_part`] with the streams sealed concurrently on `pool` (F1 L6): each stream writes its own image and the
/// part stitches them in stream order, so the bytes are the serial seal's.
#[allow(clippy::too_many_arguments)]
pub fn seal_part_pooled(
    vfs: &mut dyn WriteVfs,
    table_dir: &str,
    spec: &PartSpec,
    roots: &[ColBuffer],
    lanes: &[ShredLane],
    octet: &[u32],
    builders: &mut [Box<dyn ColumnMetaBuilder + Send>],
    sources: &[&(dyn CandidateSource + Sync)],
    resolver: &(dyn VerifyResolver + Sync),
    structural: &StructuralPolicy,
    fxid: u64,
    seq: u32,
    pool: &dyn ColumnPool,
) -> WriteResult<(SealedPart, SealReport)> {
    seal_part_core(vfs, table_dir, spec, roots, lanes, octet, builders, structural, fxid, seq, None, |sc, builders, sink| {
        for out in seal_streams_pooled(sc, builders, sources, resolver, pool)? {
            sink(out)?;
        }
        Ok(())
    })
}

/// FASTADD (pgrc-plans/FASTADD.done): seal `roots` (no shred lanes) into one part image in memory at `grain`, never a
/// file: a reader's stand-in for the columns a part written before an ADD COLUMN lacks, whose granules must be that
/// part's (its grain and row count). Elected and verified as any seal (`sources`, `resolver`).
pub fn seal_part_image(
    spec: &PartSpec,
    roots: &[ColBuffer],
    sources: &[&dyn CandidateSource],
    resolver: &dyn VerifyResolver,
    grain: GranuleGrain,
) -> WriteResult<Vec<u8>> {
    let mut vfs = crate::wvfs::MemVfs::new();
    vfs.mkdir_path("image")?;
    let mut builders: Vec<Box<dyn ColumnMetaBuilder>> = crate::meta_wire::builders_for_streams(roots, &[], &[]);
    let structural = StructuralPolicy::new();
    let (sealed, _) = seal_part_core(&mut vfs, "image", spec, roots, &[], &[], &mut builders, &structural, 0, 0, Some(grain), |sc, builders, sink| {
        for (si, b) in builders.iter_mut().enumerate() {
            let out = seal_stream(sc, si, b.get(), sources, resolver)?;
            b.release();
            sink(out)?;
        }
        Ok(())
    })?;
    vfs.read_full(&format!("image/{}", sealed.tmp_name))
}

pgsync::process_global! {
    /// C1-REG: word scratch (a granule's datums, a verify arena) kept for the next seal or verify task instead of
    /// allocated and freed per task: on an rtpool worker that churn is memory the allocator keeps (the d4c plateau).
    /// Two stacks (datums, arenas) of at most [`WORD_IDLE_MAX`] idle buffers of at most [`WORD_IDLE_MAX_WORDS`] words
    /// each (one set per concurrent lane is enough); a bigger or surplus buffer is freed as before. A buffer comes back
    /// zeroed at the asked length, as a fresh `vec![0; n]` would. This stack holds the datum buffers.
    static DATUM_IDLE: pgsync::Mutex<Vec<(Vec<u64>, memledger::Charge)>> = pgsync::Mutex::new(Vec::new());
    /// The verify arenas' idle stack (see [`DATUM_IDLE`]).
    static ARENA_IDLE: pgsync::Mutex<Vec<(Vec<u64>, memledger::Charge)>> = pgsync::Mutex::new(Vec::new());
}

/// R15-SEAL-IMAGE-CHARGE: an idle buffer's bytes sit on the parked holder while it waits in its stack, not on the account
/// of the thread that parked it; the taking thread gets them back before it uses (or frees) the buffer.
fn park_charge(bytes: u64) -> memledger::Charge {
    memledger::Charge::open(
        memledger::Owner::Cache(memledger::Holder::Parked),
        memledger::Release::Evict,
        Kind::Region,
        memledger::charge::Door::Heap,
        bytes,
    )
}
const WORD_IDLE_MAX: usize = 8;
const WORD_IDLE_MAX_WORDS: usize = 1 << 17;

/// A word buffer from one of the idle stacks; it goes back when dropped.
pub(crate) struct Words {
    /// R15-SEAL-IMAGE-CHARGE: the buffer on its user's owner while in use (declared first: it drops first).
    charge: HeldCharge,
    v: Vec<u64>,
    arena: bool,
}

impl Words {
    fn idle(arena: bool) -> pgsync::MutexGuard<'static, Vec<(Vec<u64>, memledger::Charge)>> {
        match arena {
            false => pgsync::lock(&DATUM_IDLE),
            true => pgsync::lock(&ARENA_IDLE),
        }
    }

    fn zeroed(arena: bool, n: usize, owner: Option<memledger::Owner>) -> Words {
        let parked = Words::idle(arena).pop();
        let mut v = match parked {
            Some((v, charge)) => {
                drop(charge);
                v
            }
            None => Vec::new(),
        };
        v.clear();
        v.resize(n, 0);
        let mut charge = HeldCharge::new(owner);
        charge.sync(v.capacity() as u64 * 8);
        Words { charge, v, arena }
    }

    /// A granule's datum scratch (`GRANULE_ROWS` words), on `owner` while in use.
    pub(crate) fn datums(owner: Option<memledger::Owner>) -> Words {
        Words::zeroed(false, GRANULE_ROWS as usize, owner)
    }

    /// A verify arena (8 words; [`VerifyBands::band`] grows it to each granule's bound), on `owner` while in use.
    pub(crate) fn arena(owner: Option<memledger::Owner>) -> Words {
        Words::zeroed(true, 8, owner)
    }

    /// The charge refitted to the buffer after a use grew it.
    fn sync(&mut self) {
        self.charge.sync(self.v.capacity() as u64 * 8);
    }
}

impl core::ops::Deref for Words {
    type Target = Vec<u64>;
    fn deref(&self) -> &Vec<u64> {
        &self.v
    }
}

impl core::ops::DerefMut for Words {
    fn deref_mut(&mut self) -> &mut Vec<u64> {
        &mut self.v
    }
}

impl Drop for Words {
    fn drop(&mut self) {
        self.charge = HeldCharge::none();
        let v = core::mem::take(&mut self.v);
        if v.capacity() <= WORD_IDLE_MAX_WORDS {
            let mut idle = Words::idle(self.arena);
            if idle.len() < WORD_IDLE_MAX {
                let charge = park_charge(v.capacity() as u64 * 8);
                idle.push((v, charge));
            }
        }
    }
}

/// Every stream of `sc` sealed on `pool`, each builder released once its stream is sealed; the images in stream order.
fn seal_streams_pooled(
    sc: &StreamCtx<'_>,
    builders: &mut [Box<dyn ColumnMetaBuilder + Send>],
    sources: &[&(dyn CandidateSource + Sync)],
    resolver: &(dyn VerifyResolver + Sync),
    pool: &dyn ColumnPool,
) -> WriteResult<Vec<StreamOut>> {
    type Slot<'b> = pgsync::Mutex<(&'b mut Box<dyn ColumnMetaBuilder + Send>, Option<WriteResult<StreamOut>>)>;
    let slots: Vec<Slot<'_>> = builders.iter_mut().map(|b| pgsync::Mutex::new((b, None))).collect();
    pool.run(slots.len(), &|si| {
        let srcs: Vec<&dyn CandidateSource> = sources.iter().map(|s| *s as &dyn CandidateSource).collect();
        let mut slot = pgsync::lock(&slots[si]);
        let out = seal_stream(sc, si, slot.0.get(), &srcs, resolver);
        slot.0.release();
        slot.1 = Some(out);
        drop(slot);
        // K1-MEM: a pool thread may sit idle after this stream, and the allocator returns a thread's freed pages to the
        // OS only when that thread allocates or collects again: give a big stream's working set back now.
        if seal_working_set(sc.streams[si].2) >= RELEASE_AFTER_BYTES {
            mcx::release_retained();
        }
    });
    slots
        .into_iter()
        .map(|slot| slot.into_inner().unwrap_or_else(|e| e.into_inner()).1.ok_or(WriteError::Contract { detail: "a pool skipped a stream" })?)
        .collect()
}

/// The builder behind a [`seal_part`] slot, `Send` or not.
pub trait MetaSlot {
    fn get(&mut self) -> &mut dyn ColumnMetaBuilder;
    /// K1-MEM: the stream is sealed and its distribution taken ([`StreamOut`]): drop the builder's part-grain
    /// structures now (the exact distinct set is the stream's size again), so no two streams' sets coexist. The slot
    /// keeps a stand-in nothing reads.
    fn release(&mut self);
    /// D3038: the builder moved out (an octet-length lane's, to the seal's second phase), the slot keeping a stand-in
    /// nothing reads.
    fn take(&mut self) -> Self
    where
        Self: Sized;
}

impl MetaSlot for Box<dyn ColumnMetaBuilder> {
    fn get(&mut self) -> &mut dyn ColumnMetaBuilder {
        &mut **self
    }
    fn release(&mut self) {
        *self = Box::new(StandinMetaBuilder::new());
    }
    fn take(&mut self) -> Self {
        std::mem::replace(self, Box::new(StandinMetaBuilder::new()))
    }
}

impl MetaSlot for Box<dyn ColumnMetaBuilder + Send> {
    fn get(&mut self) -> &mut dyn ColumnMetaBuilder {
        &mut **self
    }
    fn release(&mut self) {
        *self = Box::new(StandinMetaBuilder::new());
    }
    fn take(&mut self) -> Self {
        std::mem::replace(self, Box::new(StandinMetaBuilder::new()))
    }
}

/// D3038: whether column `attno`'s own stream sealed as dictionary codes (whose per-code length table answers a length)
/// or as one constant value, among the plans sealed so far: its octet-length lane is then not emitted.
fn octet_answered(plans: &[StreamPlan], attno: u32) -> bool {
    plans.iter().any(|p| {
        p.attno == attno
            && p.path_ord == 0
            && p.role == StreamRole::Values
            && (p.encoding == EncodingId::DictCodes.as_u16() || p.encoding == EncodingId::Const.as_u16())
    })
}

/// Runs `f(i)` for every `i < n`, on any threads, and returns once every call has returned.
pub trait ColumnPool: Send + Sync {
    fn run(&self, n: usize, f: &(dyn Fn(usize) + Sync));

    /// K1-MEM: [`ColumnPool::run`] while the caller runs `beside`; returns once both are done. The default runs them
    /// one after the other.
    fn run_beside(&self, n: usize, f: &(dyn Fn(usize) + Sync), beside: &mut dyn FnMut()) {
        self.run(n, f);
        beside();
    }
}

/// The caller's thread, in order.
pub struct SerialPool;

impl ColumnPool for SerialPool {
    fn run(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        (0..n).for_each(f)
    }
}

/// K1-MEM B: a stream's value bands, moved into its image as the election made them (the wrapped or the unwrapped
/// section), with the zeros between them.
#[derive(Default)]
struct Pieces {
    parts: Vec<Vec<u8>>,
    len: usize,
}

impl Pieces {
    /// Zeros up to the next 8-byte boundary; the offset there.
    fn pad8(&mut self) -> usize {
        let pad = (8 - self.len % 8) % 8;
        if pad != 0 {
            self.parts.push(vec![0; pad]);
            self.len += pad;
        }
        self.len
    }

    fn push(&mut self, image: Vec<u8>) {
        self.len += image.len();
        self.parts.push(image);
    }
}

/// One stream's sealed image, its offsets relative to its own first byte: `head` (the value bands, as the election
/// made them) then `buf`.
pub struct StreamOut {
    /// R15-SEAL-IMAGE-CHARGE: `head` and `buf` on the part's owner until the stream is written or stitched. Declared
    /// first: it drops before the image is freed.
    charge: HeldCharge,
    head: Pieces,
    buf: Vec<u8>,
    sections: Vec<SectionEntry>,
    plans: Vec<StreamPlan>,
    report: SealReport,
    plane_slices: Vec<PlaneSlice>,
    facts_slices: Vec<FactsSlice>,
    /// ST-1: the stream's distribution sketch, taken as the stream seals (K1-MEM; it was read at part end).
    dist: Option<(u32, u32, ColDistribution)>,
}

/// The part-wide facts every stream's seal reads.
pub struct StreamCtx<'a> {
    rows: u64,
    grain: GranuleGrain,
    granule_count: u32,
    band_count: u32,
    streams: Vec<(u32, u32, &'a ColBuffer, Option<i32>)>,
    /// The roots (attnos) that derived shred lanes in this part.
    shredded: Vec<u32>,
    /// jsonops (3): the part-global path_ords of the shred lanes whose every non-NULL document has an object root
    /// (their Values entries carry STREAMF_SHRED_OBJECT_ROOTS).
    object_root_lanes: Vec<u32>,
    structural: &'a StructuralPolicy,
}

/// The byte ranges of a sealed part that its publish patches (`plan::apply_part_install`: the header at 0, the fixed
/// footer at `footer_off`): the masks of its reference's crc (WR).
pub fn part_ref_masks(footer_off: u64) -> pgrc_format::wal::SmallMasks {
    [(0, pgrc_format::part::PART_HEADER_LEN as u32), (footer_off, pgrc_format::part::FOOTER_FIXED_LEN as u32)]
}

/// [`part_ref_masks`] before the footer's offset is known (a streamed seal, K1-MEM): the header alone. The footer's
/// mask joins it before the footer is written (`WriteVfs::note_ref_masks` widens a crc already running).
pub fn header_ref_masks() -> pgrc_format::wal::SmallMasks {
    [(0, pgrc_format::part::PART_HEADER_LEN as u32), (0, 0)]
}

/// A part image being assembled: the header, the streams in stream order, then the tail. Offsets are absolute:
/// `buf` begins at `flushed`, which is always 8-aligned — every piece appended after a flush begins by padding to 8
/// (each stream, the tail's first section), so padding before a flush writes the same bytes the next piece would
/// have. The whole-part seal never flushes and writes the image at the end, header first; the streamed seal
/// (K1-MEM) writes through after the header and after every stream.
struct PartAsm {
    /// R15-SEAL-IMAGE-CHARGE: `buf`'s block on the part's owner (declared first: it drops first).
    charge: HeldCharge,
    buf: Vec<u8>,
    flushed: u64,
    sections: Vec<SectionEntry>,
    plans: Vec<StreamPlan>,
    report: SealReport,
    plane_slices: Vec<PlaneSlice>,
    facts_slices: Vec<FactsSlice>,
    dist_cols: Vec<(u32, u32, ColDistribution)>,
}

impl PartAsm {
    fn new(spec: &PartSpec, owner: Option<memledger::Owner>) -> PartAsm {
        let mut buf: Vec<u8> = Vec::new();
        PartHeader::new(
            0, // part_no: assigned at publish (spec §5.1)
            spec.schema_fingerprint,
            spec.spc,
            spec.db,
            spec.relfilenumber,
        )
        .encode_into(&mut buf);
        let mut charge = HeldCharge::new(owner);
        charge.sync(buf.capacity() as u64);
        PartAsm {
            charge,
            buf,
            flushed: 0,
            sections: Vec::new(),
            plans: Vec::new(),
            report: SealReport::default(),
            plane_slices: Vec::new(),
            facts_slices: Vec::new(),
            dist_cols: Vec::new(),
        }
    }

    /// Stitch one stream's image in (stream order).
    fn push(&mut self, mut out: StreamOut) {
        pad_to(&mut self.buf, 8);
        let base = self.flushed + self.buf.len() as u64;
        let (head, img) = (std::mem::take(&mut out.head), std::mem::take(&mut out.buf));
        // charge first: the block that will hold the stream (`try_reserve`'s amortized growth) on the charge before
        // it is allocated
        let more: usize = head.parts.iter().map(|p| p.len()).sum::<usize>() + img.len();
        if self.buf.capacity() - self.buf.len() < more {
            self.charge.sync((self.buf.len() + more).max(self.buf.capacity() * 2) as u64);
            let _ = self.buf.try_reserve(more);
        }
        for p in head.parts.iter().chain(std::iter::once(&img)) {
            self.buf.extend_from_slice(p);
        }
        self.charge.sync(self.buf.capacity() as u64);
        drop(std::mem::take(&mut out.charge));
        drop((head, img));
        self.note(out, base);
    }

    /// The streamed seal: write one stream's image through at `flushed` (nothing is buffered, so it is 8-aligned),
    /// then the zeros to the next 8-byte boundary that the next piece's padding would write. No copy of the image.
    fn push_write(&mut self, mut out: StreamOut, vfs: &mut dyn WriteVfs, fd: &WvFd) -> WriteResult<()> {
        if !self.buf.is_empty() || self.flushed % 8 != 0 {
            return Err(WriteError::Contract { detail: "a streamed stream written over buffered bytes" });
        }
        let (base, head, img) = (self.flushed, std::mem::take(&mut out.head), std::mem::take(&mut out.buf));
        let mut at = base;
        for p in head.parts.iter().chain(std::iter::once(&img)) {
            if !p.is_empty() {
                vfs.pwrite_at(fd, at, p)?;
            }
            at += p.len() as u64;
        }
        let pad = (8 - at % 8) % 8;
        if pad != 0 {
            vfs.pwrite_at(fd, at, &[0u8; 8][..pad as usize])?;
        }
        self.flushed = at + pad;
        drop(std::mem::take(&mut out.charge));
        drop((head, img));
        self.note(out, base);
        Ok(())
    }

    /// A stitched stream's sections, plans, report and slices, its offsets moved to `base`.
    fn note(&mut self, out: StreamOut, base: u64) {
        self.sections.extend(out.sections.into_iter().map(|s| SectionEntry { off: s.off + base, ..s }));
        self.plans.extend(out.plans.into_iter().map(|mut p| {
            p.extents.iter_mut().for_each(|e| e.file_off += base);
            p
        }));
        self.report.elections.extend(out.report.elections);
        self.report.granules_verified += out.report.granules_verified;
        self.report.nonnull_crosschecks += out.report.nonnull_crosschecks;
        self.report.col_hashes.extend(out.report.col_hashes);
        self.report.notes.extend(out.report.notes);
        self.plane_slices.extend(out.plane_slices);
        self.facts_slices.extend(out.facts_slices);
        self.dist_cols.extend(out.dist);
    }

    /// The streamed seal: write what is buffered at `flushed`, padded to 8.
    fn flush(&mut self, vfs: &mut dyn WriteVfs, fd: &WvFd) -> WriteResult<()> {
        pad_to(&mut self.buf, 8);
        self.write_out(vfs, fd)
    }

    /// Write what is buffered at `flushed`, as it stands (the part's last bytes carry no padding).
    fn write_out(&mut self, vfs: &mut dyn WriteVfs, fd: &WvFd) -> WriteResult<()> {
        if !self.buf.is_empty() {
            vfs.pwrite_at(fd, self.flushed, &self.buf)?;
            self.flushed += self.buf.len() as u64;
            self.buf.clear();
        }
        Ok(())
    }

    /// The tail after the last stream: PathTable (shredded parts only), SortKey, StreamDir, the section table,
    /// FooterFixed and PartTail. Returns the absolute footer offset and the footer's image.
    #[allow(clippy::too_many_arguments)]
    fn tail(
        &mut self,
        spec: &PartSpec,
        lane_paths: &[&str],
        structural: &StructuralPolicy,
        schema: &[ColSchema],
        rows: u64,
        granule_count: u32,
        band_count: u32,
        grain: GranuleGrain,
    ) -> WriteResult<(u64, Vec<u8>)> {
        let base = self.flushed;
        let first = self.sections.len();
        // ---- PathTable (spec §6.5; shredded parts only) --------------------
        if !lane_paths.is_empty() {
            let body = encode_path_table(lane_paths)?;
            push_raw_section(&mut self.buf, &mut self.sections, SectionKind::PathTable, 0, 0, &body)?;
        }

        // ---- SortKey (spec §9): with a declared cluster key the v4 writer IS
        // a verified producer of the ordering (IN-1): it sorted — or verified —
        // this very part, so the section attests honestly (FT-6's
        // per-part clustered witness: key + sorted-within-part, both legs the
        // writer's own; an attested-but-unverified key would poison sort
        // elision, which is why nkeys stayed 0 until this producer existed).
        // Undeclared tables stay nkeys = 0 (OD-8: legal, unlicensed).
        let sort_body = if structural.cluster_key().is_empty() {
            SortKeyRecord::default().encode()
        } else {
            let keys = structural
                .cluster_key()
                .iter()
                .map(|k| {
                    let col = schema
                        .iter()
                        .find(|s| s.attno == k.attno)
                        .expect("the seal validated the key attnos");
                    // a path key's value is text compared bytewise: C order
                    let (collation_class, pad) = if k.path.is_empty() {
                        (col.collation_class.as_u8(), 0)
                    } else {
                        (pgrc_format::class::CollationClass::C.as_u8(), SORT_KEY_KIND_TEXT_PATH)
                    };
                    SortKeyEntry {
                        attno: k.attno,
                        dir: k.dir as u8,
                        nulls: k.nulls as u8,
                        collation_class,
                        pad,
                    }
                })
                .collect();
            self.report.cluster_keys = structural.cluster_key().iter().map(|k| k.attno).collect();
            let paths = structural.cluster_key().iter().map(|k| k.path.clone()).collect();
            SortKeyRecord { keys, paths }.encode()
        };
        push_raw_section(&mut self.buf, &mut self.sections, SectionKind::SortKey, 0, 0, &sort_body)?;

        // ---- StreamDir (spec §6.2/§6.3) -----------------------------------
        self.plans.sort_by_key(|p| (p.attno, p.path_ord, p.role.as_u8()));
        let dir_body = encode_stream_dir(&self.plans);
        let stream_count = self.plans.len() as u32;
        push_raw_section(&mut self.buf, &mut self.sections, SectionKind::StreamDir, 0, 0, &dir_body)?;
        for s in &mut self.sections[first..] {
            s.off += base;
        }

        // ---- section table + footer + tail (spec §5.2–§5.4) ----------------
        pad_to(&mut self.buf, 8);
        let section_table_off = base + self.buf.len() as u64;
        let mut table_bytes: Vec<u8> = Vec::with_capacity(self.sections.len() * 32);
        for s in &self.sections {
            s.encode_into(&mut table_bytes);
        }
        let section_table_crc = pgrc_format::wire::crc32c(&table_bytes);
        self.buf.extend_from_slice(&table_bytes);

        pad_to(&mut self.buf, 8);
        let footer_rel = self.buf.len();
        FooterFixed {
            magic: FOOTER_MAGIC,
            format_version: pgrc_format::FORMAT_VERSION,
            rows,
            granule_count,
            band_count,
            section_count: self.sections.len() as u32,
            flags: 0,
            section_table_off,
            section_table_crc,
            part_no: 0, // patched at publish
            schema_fingerprint: spec.schema_fingerprint,
            stream_count,
            granule_rows: grain.rows(), // SB-10: the part's elected grain
            reserved: [0; 28],
            footer_crc: 0, // computed by encode_into
        }
        .encode_into(&mut self.buf);
        let footer_image = self.buf[footer_rel..footer_rel + pgrc_format::part::FOOTER_FIXED_LEN].to_vec();
        let footer_off = base + footer_rel as u64;
        PartTail::new(footer_off).encode_into(&mut self.buf);
        self.charge.sync(self.buf.capacity() as u64);
        Ok((footer_off, footer_image))
    }

    /// The sealed part's facts once its bytes are on file (`file_len` of them).
    #[allow(clippy::too_many_arguments)]
    fn into_sealed(
        self,
        tmp_name: String,
        rows: u64,
        file_len: u64,
        footer_off: u64,
        footer_image: Vec<u8>,
        granule_count: u32,
        band_count: u32,
        schema: &[ColSchema],
        octet_ords: &[u32],
    ) -> (SealedPart, SealReport) {
        // ---- ST-1 (OD-2): the per-stream distribution sketches, in stream
        // order (each taken as its stream sealed). The payload is
        // DETERMINISTIC — sketches iterate canonical-byte order and streams
        // ride the (attno, path_ord) order of the stream list — so sealed
        // parts stay a pure function of their inputs, sidecar included.
        let stats_payload = if self.dist_cols.is_empty() {
            Vec::new()
        } else {
            pgrc_format::sidecar::encode_stats_payload(&self.dist_cols)
        };
        let (plans, sections) = (self.plans, self.sections);
        let sealed = SealedPart {
            ids: Vec::new(),
            tmp_name,
            rows,
            file_len,
            footer_off,
            granule_count,
            band_count,
            footer_image,
            stats_payload,
            plane_slices: {
                // Canonical attno order (the offline builder walks the
                // schema in attno order; emission order already is, the
                // sort is the invariant's belt).
                let mut ps = self.plane_slices;
                ps.sort_by_key(|s| s.attno);
                ps
            },
            facts_slices: {
                let mut fs = self.facts_slices;
                for f in fs.iter_mut() {
                    let plan = |role: StreamRole| plans.iter().find(|p| p.attno == f.attno && p.path_ord == 0 && p.role == role);
                    if let Some(v) = plan(StreamRole::Values) {
                        f.class = v.class_id;
                        f.signed = v.flags & STREAMF_SIGNED != 0;
                        f.dict = v.encoding == EncodingId::DictCodes.as_u16();
                    } else {
                        f.class = u8::MAX;
                    }
                    // The STORAGE width (the schema's), never the plan's encoding width.
                    f.width = schema.iter().find(|s| s.attno == f.attno).map_or(0, |s| s.class.width());
                    if f.dict {
                        f.ncodes = plan(StreamRole::DictPayload).map_or(0, |d| d.values);
                    }
                    // D3038: the octet-length lanes' sections are no scan's reads of their column (`facts` the same)
                    f.disk_bytes = sections
                        .iter()
                        .filter(|e| e.attno == f.attno && !octet_ords.contains(&e.path_ord) && matches!(SectionKind::from_u16(e.kind), Ok(SectionKind::StreamDir) | Ok(SectionKind::Stream) | Ok(SectionKind::PathTable)))
                        .map(|e| e.len)
                        .sum();
                }
                fs.sort_by_key(|s| s.attno);
                fs
            },
        };
        (sealed, self.report)
    }
}

/// jsonops (3): the part-global path_ords (1..) of the lanes with object roots.
fn object_root_lanes(lanes: &[ShredLane]) -> Vec<u32> {
    lanes.iter().enumerate().filter(|(_, l)| l.object_roots).map(|(i, _)| i as u32 + 1).collect()
}

/// The roots that derived shred lanes.
fn shredded_roots(lanes: &[ShredLane]) -> Vec<u32> {
    let mut v: Vec<u32> = lanes.iter().map(|l| l.parent_attno).collect();
    v.dedup();
    v
}

#[allow(clippy::too_many_arguments)]
fn seal_part_core<B: MetaSlot>(
    vfs: &mut dyn WriteVfs,
    table_dir: &str,
    spec: &PartSpec,
    roots: &[ColBuffer],
    lanes: &[ShredLane],
    octet: &[u32],
    builders: &mut [B],
    structural: &StructuralPolicy,
    fxid: u64,
    seq: u32,
    grain: Option<GranuleGrain>,
    mut emit: impl FnMut(&StreamCtx<'_>, &mut [B], &mut dyn FnMut(StreamOut) -> WriteResult<()>) -> WriteResult<()>,
) -> WriteResult<(SealedPart, SealReport)> {
    // ---- contracts ---------------------------------------------------------
    let Some(first) = roots.first() else {
        return Err(WriteError::Contract {
            detail: "seal with zero columns",
        });
    };
    let rows = first.rows();
    if rows == 0 {
        return Err(WriteError::Contract {
            detail: "seal of an empty part",
        });
    }
    for c in roots {
        if c.rows() != rows {
            return Err(WriteError::Contract {
                detail: "column row counts diverge",
            });
        }
    }
    for l in lanes {
        if l.col.rows() != rows {
            return Err(WriteError::Contract {
                detail: "shred lane not row-aligned",
            });
        }
    }

    let t_sort0 = std::time::Instant::now();

    // ---- IN-1/FT-6/OD-8: declared-cluster-key sorted ingest ----------------
    // Applied BEFORE stream assembly so every stream — roots AND derived
    // lanes — carries the same row order. One implementation for both
    // ingest paths (serial `cut_part` and the parallel `seal_one` both land
    // here), so the sort preserves the byte-identical-parts law by
    // construction. Already-ordered input takes the verify-only fast path
    // (identity permutation is skipped — the rebuilt state would be
    // bit-identical anyway). Order is a pure function of the part's rows
    // (STABLE sort: equal keys keep input order), so the sealed bytes are
    // a pure function of the input MULTISET + declared key — a shuffled
    // feed and a presorted feed seal byte-identical parts.
    let cluster_hold;
    let mut cluster_perm = None;
    let (roots, lanes): (&[ColBuffer], &[ShredLane]) = if structural.cluster_key().is_empty()
    {
        (roots, lanes)
    } else {
        match cluster_sort(roots, lanes, structural.cluster_key())? {
            None => (roots, lanes), // verified already ordered
            Some((sorted_roots, sorted_lanes, perm)) => {
                cluster_hold = (sorted_roots, sorted_lanes);
                cluster_perm = Some(perm);
                (&cluster_hold.0, &cluster_hold.1)
            }
        }
    };

    // D3038: the octet-length lanes, from the rows in the order they seal (never permuted)
    let octs = crate::octet::octet_lanes(roots, octet)?;

    let t_seal0 = std::time::Instant::now();

    // Streams: roots (path_ord 0) then lanes (part-global path_ord 1..).
    // The fourth slot is the NumericFs lane scale (RULED 2026-08-14:
    // persisted in the Values entry's aux32 under STREAMF_LANE_SCALE;
    // None on roots and non-numeric lanes).
    let streams: Vec<(u32, u32, &ColBuffer, Option<i32>)> = roots
        .iter()
        .map(|c| (c.schema.attno, 0u32, c, None))
        .chain(
            lanes
                .iter()
                .enumerate()
                .map(|(i, l)| (l.parent_attno, i as u32 + 1, &l.col, l.scale)),
        )
        .collect();
    if builders.len() != streams.len() + octs.len() {
        return Err(WriteError::Contract {
            detail: "one meta builder per stream",
        });
    }

    // ---- SB-10 grain election (closed-form over exact chunk stats) ---------
    // (FASTADD: a stand-in part's grain is the part it stands beside, `seal_part_image`; D3038: the octet-length
    // lanes elect beside the other streams)
    let grain = grain.unwrap_or_else(|| {
        let all: Vec<(u32, u32, &ColBuffer, Option<i32>)> = streams.iter().copied().chain(octs.iter().map(|l| (l.parent_attno, 0, &l.col, None))).collect();
        elect_part_grain(rows, &all)
    });
    // The closed forms are untruncated u64 (geom §, idx 253). Bound the
    // granule count in u64 first; a legitimate part is well within u32, so
    // only after the MAX_GRANULES_PER_PART guard do we narrow to the u32 the
    // footer/manifest fields (and the downstream loops) carry.
    let granule_count = geom::granule_count_at(rows, grain);
    let band_count = geom::band_count_at(rows, grain);
    if granule_count > MAX_GRANULES_PER_PART as u64 {
        return Err(WriteError::Contract {
            detail: "part exceeds the rowid granule budget (cut policy breach)",
        });
    }
    let granule_count = granule_count as u32;
    let band_count = band_count as u32;

    // ---- per-stream emission, stitched in stream order ----------------------
    let mut asm = PartAsm::new(spec, roots.first().and_then(ColBuffer::owner));
    let n1 = streams.len();
    let sc = StreamCtx {
        rows,
        grain,
        granule_count,
        band_count,
        streams,
        shredded: shredded_roots(lanes),
        object_root_lanes: object_root_lanes(lanes),
        structural,
    };
    emit(&sc, &mut builders[..n1], &mut |out: StreamOut| {
        asm.push(out);
        Ok(())
    })?;
    // ---- D3038: the octet-length lanes, once every other stream sealed ------
    // A lane whose column sealed as dictionary codes or one constant is not emitted; the others take the path
    // ordinals after the shred lanes, in column order, and seal as the other streams did (the same images on a pool).
    let keep: Vec<usize> = (0..octs.len()).filter(|&i| !octet_answered(&asm.plans, octs[i].parent_attno)).collect();
    let octet_ords: Vec<u32> = (0..keep.len()).map(|j| lanes.len() as u32 + 1 + j as u32).collect();
    if !keep.is_empty() {
        let mut kept: Vec<B> = keep.iter().map(|&i| builders[n1 + i].take()).collect();
        let sc = StreamCtx {
            rows,
            grain,
            granule_count,
            band_count,
            streams: keep.iter().zip(&octet_ords).map(|(&i, &ord)| (octs[i].parent_attno, ord, &octs[i].col, None)).collect(),
            shredded: shredded_roots(lanes),
            object_root_lanes: object_root_lanes(lanes),
            structural,
        };
        emit(&sc, &mut kept, &mut |out: StreamOut| {
            asm.push(out);
            Ok(())
        })?;
    }
    let lane_paths: Vec<&str> = lanes.iter().map(|l| l.path.as_str()).chain(keep.iter().map(|_| pgrc_format::shredlane::OCTET_LENGTH_PATH)).collect();
    let schema: Vec<ColSchema> = roots.iter().map(|c| c.schema).collect();
    let (footer_off, footer_image) = asm.tail(spec, &lane_paths, structural, &schema, rows, granule_count, band_count, grain)?;

    // ---- write the tmp file, header first (spec §5) ------------------------
    let tmp_name = pgrc_format::dirlayout::temp_file_name(fxid, seq);
    let path = format!("{table_dir}/{tmp_name}");
    let fd = vfs.create_rw(&path)?;
    // The publish patches these two ranges through any name of the file (`plan::apply_part_install`), so a
    // reference's crc skips them (WR).
    vfs.note_ref_masks(&fd, part_ref_masks(footer_off))?;
    let buf = &asm.buf;
    vfs.pwrite_at(&fd, 0, &buf[..pgrc_format::part::PART_HEADER_LEN])?;
    vfs.pwrite_at(
        &fd,
        pgrc_format::part::PART_HEADER_LEN as u64,
        &buf[pgrc_format::part::PART_HEADER_LEN..],
    )?;
    vfs.close_file(fd)?;
    let file_len = buf.len() as u64;

    asm.report.sort_secs = (t_seal0 - t_sort0).as_secs_f64();
    asm.report.seal_secs = t_seal0.elapsed().as_secs_f64();
    asm.report.cluster_perm = cluster_perm;
    Ok(asm.into_sealed(tmp_name, rows, file_len, footer_off, footer_image, granule_count, band_count, &schema, &octet_ords))
}

/// K1-MEM: what [`seal_part_streamed`] did.
pub enum Streamed {
    Sealed(SealedPart, SealReport),
    /// A stream's values average past the default grain's granule byte bound: the part needs the finer grain the
    /// whole-part seal elects (SB-10). The caller seals it that way; a tmp file already created is the caller's to
    /// reclaim.
    FinerGrain,
}

/// K1-MEM: the floor of a pooled streamed seal's batch, in estimated seal working set ([`seal_working_set`]). A batch's
/// streams seal concurrently while their estimates sum to at most the larger of this and the largest single stream
/// the part has sealed so far: a batch never holds more than the part's widest stream already did, so the seal's peak
/// stays that stream's (a stream past the bound seals alone; without a pool every stream seals alone).
pub const STREAMED_BATCH_BYTES: u64 = 64 << 20;

/// K1-MEM: a pooled stream whose estimated working set ([`seal_working_set`]) is at least this collects its thread's
/// freed memory once sealed.
const RELEASE_AFTER_BYTES: u64 = 32 << 20;

/// K1-MEM: a bound on what sealing `col` holds: the column, its exact distinct set (ST-1; about 80 B per distinct
/// int8, 200 B per distinct 128-B text), its encoded images (unwrapped, wrapped) and the stream image. Measured at
/// 800,000 distinct values: 123 MB for an int8 column, 460 MB for a 132-B text column; this is 160 and 712 MB. (Since
/// K1-MEM B the distinct set keys into the column, about 60 B per distinct value, and the stream image is the chosen
/// band images themselves: the bound is looser, not wrong.)
fn seal_working_set(col: &ColBuffer) -> u64 {
    4 * col.approx_bytes() + 160 * col.rows()
}

/// A streamed seal's pooled batching ([`STREAMED_BATCH_BYTES`]): the working set of the batch being gathered and the
/// bound, raised to the widest stream seen. Without a pool the bound stays 0: every stream seals alone.
pub(crate) struct BatchBound {
    held: u64,
    bound: u64,
    pooled: bool,
}

impl BatchBound {
    pub(crate) fn new(pooled: bool, floor: u64) -> BatchBound {
        BatchBound { held: 0, bound: if pooled { floor } else { 0 }, pooled }
    }

    /// A stream of estimated working set `ws` joins: true when the batch gathered so far (if any) must seal first.
    pub(crate) fn admit(&mut self, ws: u64, gathered: bool) -> bool {
        if self.pooled {
            self.bound = self.bound.max(ws);
        }
        let cut = gathered && self.held + ws > self.bound;
        self.held = if cut || !gathered { ws } else { self.held + ws };
        cut
    }
}

/// `streams` in order, cut into pooled batches by `b`.
fn stream_batches<'c>(streams: Vec<(u32, u32, &'c ColBuffer, Option<i32>)>, b: &mut BatchBound) -> Vec<Vec<(u32, u32, &'c ColBuffer, Option<i32>)>> {
    let mut out: Vec<Vec<(u32, u32, &'c ColBuffer, Option<i32>)>> = Vec::new();
    for s in streams {
        let gathered = !out.is_empty();
        match b.admit(seal_working_set(s.2), gathered) || !gathered {
            true => out.push(vec![s]),
            false => out.last_mut().expect("gathered").push(s),
        }
    }
    b.held = 0;
    out
}

/// The seal pool's half of a streamed seal ([`crate::writer::SealPool`] borrowed).
pub struct StreamedPool<'p> {
    pub pool: &'p dyn ColumnPool,
    pub sources: Vec<&'p (dyn CandidateSource + Sync)>,
    pub resolver: &'p (dyn VerifyResolver + Sync),
}

/// One batch of a streamed seal: its streams sealed and written through in stream order, each builder dropped with its
/// stream. With a pool, a batch of several streams seals them concurrently, and a lone stream spreads its band work
/// (verify, wrapper) over the pool while this thread folds its meta ([`seal_stream_beside`]).
#[allow(clippy::too_many_arguments)]
fn seal_streamed_batch(
    asm: &mut PartAsm,
    vfs: &mut dyn WriteVfs,
    fd: &WvFd,
    (rows, grain, granule_count, band_count): (u64, GranuleGrain, u32, u32),
    batch: Vec<(u32, u32, &ColBuffer, Option<i32>)>,
    (shredded, object_roots): (&[u32], &[u32]),
    octet: bool,
    structural: &StructuralPolicy,
    pool: Option<&StreamedPool<'_>>,
    sources: &[&dyn CandidateSource],
    resolver: &dyn VerifyResolver,
) -> WriteResult<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let n = batch.len();
    let sc = StreamCtx {
        rows,
        grain,
        granule_count,
        band_count,
        streams: batch,
        shredded: pgrc_format::try_to_vec(shredded, "seal")?,
        object_root_lanes: pgrc_format::try_to_vec(object_roots, "seal")?,
        structural,
    };
    match pool {
        Some(p) if n > 1 => {
            // D3038: an octet-length lane's builder is the octet profile's (`meta_wire::octet_builder`)
            let mut builders: Vec<Box<dyn ColumnMetaBuilder + Send>> = sc
                .streams
                .iter()
                .map(|s| match octet {
                    true => crate::meta_wire::send_octet_builder(s.0),
                    false => crate::meta_wire::send_builder_for(&s.2.schema),
                })
                .collect();
            for out in seal_streams_pooled(&sc, &mut builders, &p.sources, p.resolver, p.pool)? {
                asm.push_write(out, vfs, fd)?;
            }
        }
        _ => {
            for si in 0..n {
                let mut builder = match octet {
                    true => crate::meta_wire::octet_builder(sc.streams[si].0, sc.streams[si].2.owner()),
                    false => crate::meta_wire::builder_for(&sc.streams[si].2.schema, sc.streams[si].2.owner()),
                };
                // A stream sealed alone spreads its band work over the pool, when there is one.
                let out = match pool {
                    Some(p) => seal_stream_beside(&sc, si, &mut *builder, p.pool, sources, resolver)?,
                    None => seal_stream(&sc, si, &mut *builder, sources, resolver)?,
                };
                drop(builder);
                asm.push_write(out, vfs, fd)?;
            }
        }
    }
    Ok(())
}

/// K1-MEM: seal one part stream by stream at the default grain, fetching each root's column from `feed` as it seals
/// and writing each stream's image through at once; the bytes are [`seal_part`]'s for the same rows. Memory is the
/// streams in flight — with a pool, one batch of narrow columns (their estimated working sets up to `batch_bytes`,
/// sealed concurrently) or one wide column (its bands verified and wrapped on the pool); without, one column — never
/// the part.
///
/// - `feed(c, out)` fills `out` (empty; `schema[c]`) with column `c`'s `rows` rows in part order. It is called once
///   per root, in schema order, except that a declared cluster key's columns are fetched first and held to their turn
///   (the feed must already be in key order: the seal verifies, it never sorts a streamed part), and a root that
///   derived shred lanes is fetched again after the roots, when its lanes (re-derived) seal in part-global order.
/// - The grain is the default (SB-10 elects it whenever every stream's values average under the granule byte bound);
///   the first stream past the bound returns [`Streamed::FinerGrain`].
/// - `created` is set once the tmp file exists: on an error or [`Streamed::FinerGrain`] the caller reclaims it.
#[allow(clippy::too_many_arguments)]
pub fn seal_part_streamed(
    vfs: &mut dyn WriteVfs,
    table_dir: &str,
    spec: &PartSpec,
    schema: &[ColSchema],
    rows: u64,
    feed: &mut dyn FnMut(usize, &mut ColBuffer) -> WriteResult<()>,
    shred: &mut dyn ShredLaneSource,
    shred_opts: &ShredOptions,
    sources: &[&dyn CandidateSource],
    resolver: &dyn VerifyResolver,
    pool: Option<&StreamedPool<'_>>,
    batch_bytes: u64,
    structural: &StructuralPolicy,
    fxid: u64,
    seq: u32,
    created: &mut bool,
) -> WriteResult<Streamed> {
    if schema.is_empty() {
        return Err(WriteError::Contract { detail: "seal with zero columns" });
    }
    if rows == 0 {
        return Err(WriteError::Contract { detail: "seal of an empty part" });
    }
    // No phase timers (`SealReport::seal_secs` stays 0): nothing reads them, and a raw clock read here would grow the
    // file's determinism-lint budget.
    let grain = GranuleGrain::DEFAULT;
    let gc = geom::granule_count_at(rows, grain);
    if gc > MAX_GRANULES_PER_PART as u64 {
        return Err(WriteError::Contract { detail: "part exceeds the rowid granule budget (cut policy breach)" });
    }
    let (granule_count, band_count) = (gc as u32, geom::band_count_at(rows, grain) as u32);
    // `elect_part_grain`'s test at its first (the default) rung.
    let fits = |c: &ColBuffer| c.granule_pricing_bytes().div_ceil(gc.max(1)) <= geom::GRANULE_BYTE_BOUND_PROVISIONAL;
    let mut fetch = |c: usize| -> WriteResult<ColBuffer> {
        let mut col = ColBuffer::new(schema[c]);
        feed(c, &mut col)?;
        if col.rows() != rows {
            return Err(WriteError::Contract { detail: "column row counts diverge" });
        }
        Ok(col)
    };

    // The cluster key's columns first: verified in order (`cluster_sort`'s fast path), then held to their turn.
    let mut held: Vec<Option<ColBuffer>> = (0..schema.len()).map(|_| None).collect();
    if !structural.cluster_key().is_empty() {
        for d in structural.cluster_key() {
            let k = schema.iter().position(|s| s.attno == d.attno).ok_or(WriteError::Contract {
                detail: "declared cluster-key attno is not a table column",
            })?;
            if held[k].is_none() {
                held[k] = Some(fetch(k)?);
            }
        }
        let keys: Vec<&ColBuffer> = held.iter().flatten().collect();
        if !cluster_ordered(&keys, structural.cluster_key())? {
            return Err(WriteError::Contract { detail: "a streamed seal's feed is not in cluster-key order" });
        }
    }

    let tmp_name = pgrc_format::dirlayout::temp_file_name(fxid, seq);
    let fd = vfs.create_rw(&format!("{table_dir}/{tmp_name}"))?;
    *created = true;
    let body = (|| -> WriteResult<Option<(SealedPart, SealReport)>> {
        vfs.note_ref_masks(&fd, header_ref_masks())?;
        let mut asm = PartAsm::new(spec, None);
        asm.flush(&mut *vfs, &fd)?;
        let geo = (rows, grain, granule_count, band_count);
        let seal = |asm: &mut PartAsm,
                    vfs: &mut dyn WriteVfs,
                    batch: Vec<(u32, u32, &ColBuffer, Option<i32>)>,
                    shredded: &[u32],
                    object_roots: &[u32],
                    octet: bool| {
            seal_streamed_batch(asm, vfs, &fd, geo, batch, (shredded, object_roots), octet, structural, pool, sources, resolver)
        };

        // ---- roots, in schema order ------------------------------------
        // D3038: each eligible root's octet-length lane, derived at the root's turn and held (an int4 word a row) to
        // its turn after the shred lanes; dropped once its root seals as dictionary codes or one constant
        let mut octs: Vec<ShredLane> = Vec::new();
        let mut shredded: Vec<u32> = Vec::new();
        let mut lane_paths: Vec<(usize, Vec<String>)> = Vec::new();
        let mut bound = BatchBound::new(pool.is_some(), batch_bytes);
        let mut batch: Vec<ColBuffer> = Vec::new();
        for c in 0..schema.len() {
            // A column is held while the batch before it seals only when its size is known to be small (a fixed-width
            // column beside a pooled batch): without a pool, or ahead of a varlena column, the batch seals first.
            if !batch.is_empty() && (pool.is_none() || schema[c].class == StorageClass::VarlenaVerbatim) {
                seal(&mut asm, &mut *vfs, batch.iter().map(|b| (b.schema.attno, 0u32, b, None)).collect(), &shredded, &[], false)?;
                batch.clear();
                bound.held = 0;
                octs.retain(|l| !octet_answered(&asm.plans, l.parent_attno));
            }
            let col = match held[c].take() {
                Some(col) => col,
                None => fetch(c)?,
            };
            if !fits(&col) {
                return Ok(None);
            }
            if crate::octet::eligible(&col, structural, &*shred) {
                let lane = crate::octet::octet_lanes(std::slice::from_ref(&col), &[col.schema.attno])?;
                if lane.iter().any(|l| !fits(&l.col)) {
                    return Ok(None);
                }
                octs.extend(lane);
            }
            // Shred lanes (dual-store, O-4), as `cut_part` derives them: checked against the grain now, sealed after
            // the roots (derived again then) so that no lane is held across the roots.
            if col.schema.class == StorageClass::VarlenaVerbatim {
                let derived = shred.shred(&col, shred_opts)?;
                validate_lanes(&col, &derived, shred_opts)?;
                if !derived.is_empty() {
                    for l in &derived {
                        if l.col.rows() != rows {
                            return Err(WriteError::Contract { detail: "shred lane not row-aligned" });
                        }
                        if !fits(&l.col) {
                            return Ok(None);
                        }
                    }
                    shredded.push(col.schema.attno);
                    lane_paths.push((c, derived.iter().map(|l| l.path.clone()).collect()));
                }
            }
            if bound.admit(seal_working_set(&col), !batch.is_empty()) {
                seal(&mut asm, &mut *vfs, batch.iter().map(|b| (b.schema.attno, 0u32, b, None)).collect(), &shredded, &[], false)?;
                batch.clear();
                octs.retain(|l| !octet_answered(&asm.plans, l.parent_attno));
            }
            batch.push(col);
        }
        seal(&mut asm, &mut *vfs, batch.iter().map(|b| (b.schema.attno, 0u32, b, None)).collect(), &shredded, &[], false)?;
        drop(batch);
        bound.held = 0;

        // ---- lanes, part-global path order (derived again) ----------------
        let mut ord = 1u32;
        let mut object_roots: Vec<u32> = Vec::new();
        for (c, paths) in &lane_paths {
            let col = fetch(*c)?;
            let derived = shred.shred(&col, shred_opts)?;
            validate_lanes(&col, &derived, shred_opts)?;
            drop(col);
            if derived.len() != paths.len() || derived.iter().zip(paths).any(|(l, p)| l.path != *p) {
                return Err(WriteError::Contract { detail: "shred lanes moved between a streamed seal's passes" });
            }
            object_roots.extend(derived.iter().enumerate().filter(|(_, l)| l.object_roots).map(|(i, _)| ord + i as u32));
            let lanes = derived.iter().enumerate().map(|(i, l)| (l.parent_attno, ord + i as u32, &l.col, l.scale)).collect();
            for b in stream_batches(lanes, &mut bound) {
                seal(&mut asm, &mut *vfs, b, &shredded, &object_roots, false)?;
            }
            ord += derived.len() as u32;
        }

        // ---- D3038: the octet-length lanes, as `seal_part_core` emits them --------------------------------------
        octs.retain(|l| !octet_answered(&asm.plans, l.parent_attno));
        let octet_ords: Vec<u32> = (0..octs.len() as u32).map(|j| ord + j).collect();
        let streams = octs.iter().zip(&octet_ords).map(|(l, &o)| (l.parent_attno, o, &l.col, None)).collect();
        for b in stream_batches(streams, &mut bound) {
            seal(&mut asm, &mut *vfs, b, &shredded, &object_roots, true)?;
        }
        drop(octs);

        // ---- the tail, and the footer's crc mask before it lands ---------
        let paths: Vec<&str> = lane_paths
            .iter()
            .flat_map(|(_, p)| p.iter().map(|s| s.as_str()))
            .chain(octet_ords.iter().map(|_| pgrc_format::shredlane::OCTET_LENGTH_PATH))
            .collect();
        let (footer_off, footer_image) = asm.tail(spec, &paths, structural, schema, rows, granule_count, band_count, grain)?;
        vfs.note_ref_masks(&fd, part_ref_masks(footer_off))?;
        asm.write_out(&mut *vfs, &fd)?;
        let file_len = asm.flushed;
        Ok(Some(asm.into_sealed(tmp_name.clone(), rows, file_len, footer_off, footer_image, granule_count, band_count, schema, &octet_ords)))
    })();
    let closed = vfs.close_file(fd);
    match body? {
        Some(sealed) => {
            closed?;
            Ok(Streamed::Sealed(sealed.0, sealed.1))
        }
        None => Ok(Streamed::FinerGrain),
    }
}

/// One stream of a part (roots at `path_ord` 0, then lanes): its election, extents, validity and stats.
fn seal_stream(
    sc: &StreamCtx<'_>,
    si: usize,
    builder: &mut dyn ColumnMetaBuilder,
    sources: &[&dyn CandidateSource],
    resolver: &dyn VerifyResolver,
) -> WriteResult<StreamOut> {
    seal_stream_with(sc, si, builder, None, sources, resolver)
}

/// K1-MEM: [`seal_stream`] with the stream's band work on `pool`: its verify and wrapper run band by band there while
/// the caller folds its meta (the builder stays on the caller's thread); the same image. For a stream sealed alone on
/// the caller's thread, never from a pool thread.
fn seal_stream_beside(
    sc: &StreamCtx<'_>,
    si: usize,
    builder: &mut dyn ColumnMetaBuilder,
    pool: &dyn ColumnPool,
    sources: &[&dyn CandidateSource],
    resolver: &dyn VerifyResolver,
) -> WriteResult<StreamOut> {
    seal_stream_with(sc, si, builder, Some(pool), sources, resolver)
}

/// The stream's seal with `builder` pinned to its inputs (K1-MEM B): every `EncodeInput` the seal hands the builder is
/// a slice of this stream's column (`sc.streams[si]`, borrowed for this call) or of buffers local to the call, and the
/// builder is observed and read only inside it; so the builder may key its distinct set into those bytes instead of
/// copying them, and lets go of them before this returns, whatever it returns.
fn seal_stream_with(
    sc: &StreamCtx<'_>,
    si: usize,
    builder: &mut dyn ColumnMetaBuilder,
    band_pool: Option<&dyn ColumnPool>,
    sources: &[&dyn CandidateSource],
    resolver: &dyn VerifyResolver,
) -> WriteResult<StreamOut> {
    // SAFETY: the inputs outlive the builder's use of them (above): `unpin_inputs` runs before the column's borrow ends.
    unsafe { builder.pin_inputs() };
    let out = seal_stream_pinned(sc, si, builder, band_pool, sources, resolver);
    builder.unpin_inputs();
    out
}

fn seal_stream_pinned(
    sc: &StreamCtx<'_>,
    si: usize,
    builder: &mut dyn ColumnMetaBuilder,
    band_pool: Option<&dyn ColumnPool>,
    sources: &[&dyn CandidateSource],
    resolver: &dyn VerifyResolver,
) -> WriteResult<StreamOut> {
    let (rows, grain, granule_count, band_count, structural) = (sc.rows, sc.grain, sc.granule_count, sc.band_count, sc.structural);
    let (attno, path_ord, col, lane_scale) = sc.streams[si];
    let mut buf: Vec<u8> = Vec::new();
    let mut sections: Vec<SectionEntry> = Vec::new();
    let mut plans: Vec<StreamPlan> = Vec::new();
    let mut report = SealReport::default();
    let mut plane_slices: Vec<PlaneSlice> = Vec::new();
    let mut facts_slices: Vec<FactsSlice> = Vec::new();
    // Scratch reused across streams. The verify arena is u64-BACKED and
    // byte-viewed ([`words_as_bytes`]): ByteArena bases must be 8-aligned
    // (the abi.rs §19.4 base-alignment law, debug-asserted). The v3-verbatim
    // ordinary path used a bare `Vec<u8>` here and survived only by malloc
    // alignment accident — the structural arm below always carried the
    // sanctioned idiom; now both arms do (L2 A-lane hardening).
    // (C1-REG: taken from the idle word buffers at the first in-stream verify, not allocated per stream.)
    let mut verify_scratch: Option<(Words, Words)> = None;
    // R15-SEAL-IMAGE-CHARGE: the stream's image (its band sections, the wrapper's images, `head`, `buf`) on the part's
    // owner from its allocation until the stream is written, synced as each block grows (declared after them: drops
    // first). The overflow region is a `ChargedVec` of its own.
    let mut img = HeldCharge::new(col.owner());
    let class = col.schema.class;
    let stats = col.stream_stats();
    let shape = ExtentShape {
        extent_count: band_count as u64,
        frame_count: rows.div_ceil(geom::FRAME_VALUES as u64),
    };

    // Materialize the whole stream's row-dense datum currency ONCE
    // (SEAL-FUSION): byval classes BORROW the staged words (zero copy);
    // byref classes materialize heap pointers in one pass. Granule
    // inputs at the elected grain — and, below, the election's
    // default-grain inputs — are SLICES of this one array: the M3-C
    // analyzers are exact whole-part pure functions (analyze-then-
    // elect, charter §1), and the seal reuses the same inputs for
    // encode + verify + meta observation.
    let flat = col.part_ptrs();
    // CONVERT W3-2a: a byref column's materialized pointers on the part's owner for the stream (declared after
    // them: drops first; a byval column borrows its words, nothing to charge)
    let mut ptrs_charge = HeldCharge::new(col.owner());
    if let std::borrow::Cow::Owned(v) = &flat {
        ptrs_charge.sync(v.capacity() as u64 * 8);
    }
    let ginputs: Vec<pgrc_format::abi::EncodeInput<'_>> = (0..granule_count)
        .map(|g| {
            let start = g as usize * grain.rows() as usize;
            let rows_g = geom::rows_in_granule_at(rows, grain, g);
            col.encode_input(g, grain, &flat[start..start + rows_g as usize], rows_g)
        })
        .collect();

    // ---- TY-1 structural arm: ArrayDual (ENC 10) ----------------------
    // A declared array column routes through the codec's whole-part
    // accept/refuse: acceptance on EVERY granule seals sizes+elements
    // dual substreams (per-part permanence); any refusal (null
    // elements, multidim, byref/text payloads, exotic shapes) falls
    // through to the ordinary election, which demotes to VERBATIM.
    if path_ord == 0 && class == StorageClass::VarlenaVerbatim {
        if let Some(facts) = structural.array_facts(attno) {
            if let Some(splits) = split_all_granules(&ginputs, facts)? {
                // CONVERT W3-2a: the per-granule array splits on the part's owner while the arm seals (declared
                // after them: drops first)
                let mut splits_charge = HeldCharge::new(col.owner());
                splits_charge.sync(
                    (splits.capacity() * core::mem::size_of::<ArraySplit>()
                        + splits.iter().map(|s| (s.sizes.capacity() + s.elems.capacity()) * 8).sum::<usize>())
                        as u64,
                );
                let baseline = verbatim_baseline_len(&stats, &shape);
                seal_array_dual(
                    &mut buf,
                    &mut sections,
                    &mut plans,
                    &mut report,
                    builder,
                    resolver,
                    col,
                    &ginputs,
                    &splits,
                    facts,
                    attno,
                    rows,
                    grain,
                    granule_count,
                    band_count,
                    baseline,
                    &mut plane_slices,
                    &mut facts_slices,
                )?;
                let dist = builder.distribution().map(|d| (attno, path_ord, d));
                img.sync(buf.capacity() as u64);
                return Ok(StreamOut { charge: img, head: Pieces::default(), buf, sections, plans, report, plane_slices, facts_slices, dist });
            }
        }
    }

    // Election inputs: the FROZEN election analyzers and the dict arm
    // slice at the DEFAULT grain (their intra-granule closed forms are
    // capacity-keyed at 8192). The part grain is uniform, so both
    // slicings cover the identical row-dense value sequence — the
    // election is judged on the same values, and dict code arrays are
    // re-sliced to the part grain below. SEAL-FUSION: the default-grain
    // inputs are SLICES of the one materialized array above — the old
    // second whole-column materialization (#1b) is gone.
    let e_ginputs: Vec<pgrc_format::abi::EncodeInput<'_>> = if grain.is_default() {
        Vec::new()
    } else {
        let dgc = geom::granule_count(rows);
        (0..dgc)
            .map(|g| {
                let start = g as usize * GranuleGrain::DEFAULT.rows() as usize;
                let rows_g = geom::rows_in_granule(rows, g);
                col.encode_input(
                    g,
                    GranuleGrain::DEFAULT,
                    &flat[start..start + rows_g as usize],
                    rows_g,
                )
            })
            .collect()
    };
    let elect_granules: &[pgrc_format::abi::EncodeInput<'_>] = if grain.is_default() {
        &ginputs
    } else {
        &e_ginputs
    };

    // ---- election: full-registry arm first (source order), stats-only
    // fallback (A-lane amendment M3-A2) ---------------------------------
    let fin = FullElectInput {
        attno,
        path_ord,
        stats: &stats,
        shape: &shape,
        granules: elect_granules,
        col: Some(col),
    };
    let mut full: Option<FullElection> = None;
    for src in sources {
        if let Some(r) = src.elect_full(&fin) {
            full = Some(r?);
            break;
        }
    }
    let mut full = match full {
        Some(f) => f,
        None => {
            let (elected, witness) = elect_stream(&fin, sources);
            match elected {
                Elected::Verbatim => FullElection {
                    encoding: EncodingId::Verbatim.as_u16(),
                    width: class.width(),
                    aux32: 0,
                    extra_flags: 0,
                    offer_wrapper: false,
                    plan: ElectPlan::Verbatim,
                    witness,
                    note: None,
                },
                Elected::Candidate(f) => FullElection {
                    encoding: f.key().encoding,
                    width: f.key().width,
                    aux32: 0,
                    extra_flags: 0,
                    offer_wrapper: false,
                    plan: ElectPlan::Encoder(f),
                    witness,
                    note: None,
                },
            }
        }
    };
    let encoding = full.encoding;
    let width = full.width;
    let mut flags: u16 = full.extra_flags;
    if class.signed() {
        flags |= STREAMF_SIGNED;
    }
    // The NumericFs lane-scale slot (JSON routing, RULED 2026-08-14):
    // a shredded numeric lane's chunk-shared decimal scale lands in
    // the Values entry's aux32 under STREAMF_LANE_SCALE (the
    // ArrayDual-aux32 precedent) — readers adjudicate scale soundness
    // from the part alone; flag-absent (pre-ruling) numeric lanes
    // stay reader-refused.
    if let Some(scale) = lane_scale {
        full.aux32 = scale as u32;
        flags |= pgrc_format::part::STREAMF_LANE_SCALE;
    }
    // jsonops (3): a shred lane of a part whose every non-NULL document has an object root
    if path_ord >= 1 && sc.object_root_lanes.contains(&path_ord) {
        flags |= pgrc_format::part::STREAMF_SHRED_OBJECT_ROOTS;
    }
    // Resolve the verify vtable through the ONE normalization (spec
    // §6.3/§19.5): the entry width byte is per-encoding vocabulary
    // (DICT_CODES: max code width — a stats fact), never the dispatch
    // axis. The verify path resolves exactly as a reader will.
    let key = pgrc_format::enc::stream_kernel_key(encoding, class.id(), width)
        .map_err(WriteError::Format)?;
    let vt = resolver.resolve(key)?;

    // Validity bytes for the WHOLE stream, built up front: the verify
    // ctx and the two-witness check both read the emitted bytes (the
    // per-granule popcounts ride the build — SEAL-FUSION #15/#17).
    let (val_bytes, val_pops) = build_validity_bytes(col, granule_count, grain);

    // DICT-DEDUP: the dict election already maintained this part's
    // exact counted distinct set (the byte-rank dict build). Hand it to
    // the meta builder BEFORE the folds below, so `distribution()`
    // serves from it and the builder's own accumulator never runs on
    // this stream — the distinct set is maintained ONCE, two consumers.
    // Non-dict streams, demoted arms, the count-free D2 inherit path,
    // and the `PGRUST_PGRC_DICT_DIST_FEED=0` control arm take the
    // `None` branch and keep the accumulator (#971) path; sidecar
    // bytes are identical either way (pure functions of the data — the
    // sketch equivalence battery + the rig dirshas pin it).
    //
    // PGRC-WRITER-FAST `bank`: a varlena column's append deferred the
    // non-null part of its O-10 logical hash. Fold it here once per
    // DISTINCT value off the same counted set (Σ count·digest is exact
    // under the wrapping multiset sum; taken BEFORE the feed moves into
    // the builder, trusted only when the counts cover every row), else
    // inside the meta builder's hash lane (one pass, four chains; see
    // after the band encode), else by one walk.
    let mut bank_hash = *col.eager_hash();
    let mut bank_pending = col.bank_deferred();
    if bank_pending {
        if let ElectPlan::Dict(d) = &full.plan {
            if let Some(feed) = &d.dist_feed {
                let mut h = bank_hash;
                for (b, c) in feed {
                    let (lo, hi) = pgrc_format::bank::row_digests(b);
                    h.observe_repeated(lo, hi, *c);
                }
                if h.digest().2 == stats.rows {
                    bank_hash = h;
                    bank_pending = false;
                }
            }
        }
    }
    let bank_fused = bank_pending && builder.bank_fold_begin();
    if let ElectPlan::Dict(d) = &mut full.plan {
        if let Some(feed) = d.dist_feed.take() {
            // CONVERT W3-2a: the feed leaves the plan's charge for the meta builder
            d.sync_charge();
            builder.set_distribution_feed(feed);
        }
    }
    let dict_plan = match &full.plan {
        ElectPlan::Dict(d) => Some(d),
        _ => None,
    };
    // Dict codes were built by the FROZEN election arm at the DEFAULT
    // grain; at a smaller part grain, re-slice the row-dense whole-part
    // code stream at the elected grain (default-grain granule
    // concatenation IS row-dense part order; null slots carry 0 either
    // way).
    let codes_regrain: Option<Vec<Vec<u64>>> = match (dict_plan, grain.is_default()) {
        (Some(d), false) => {
            let flat: Vec<u64> = d.codes.iter().flat_map(|v| v.iter().copied()).collect(); // alloc: Exec charged flat_charge
            // CONVERT W3-2a: the flattened codes on the part's owner while the re-slice copies them (declared after
            // them: drops first)
            let mut flat_charge = HeldCharge::new(col.owner());
            flat_charge.sync(flat.capacity() as u64 * 8);
            let mut out = pgrc_format::try_vec(granule_count as usize, "seal")?; // alloc: Exec charged regrain_charge
            for g in 0..granule_count {
                let start = g as usize * grain.rows() as usize;
                let rows_g = geom::rows_in_granule_at(rows, grain, g) as usize;
                out.push(pgrc_format::try_to_vec(&flat[start..start + rows_g], "seal")?); // alloc: Exec charged regrain_charge
            }
            Some(out)
        }
        _ => None,
    };
    // CONVERT W3-2a: the re-sliced codes on the part's owner for the stream (declared after them: drops first)
    let mut regrain_charge = HeldCharge::new(col.owner());
    regrain_charge.sync(codes_regrain.as_ref().map_or(0, |o| {
        cap_bytes(o.capacity(), o) + o.iter().map(|v| cap_bytes(v.capacity(), v)).sum::<u64>()
    }));
    // Value-stream encode currency: global codes for dict streams
    // (their datum currency IS the code, spec §7), datum pointers/words
    // otherwise.
    let enc_inputs: Vec<pgrc_format::abi::EncodeInput<'_>> = (0..granule_count)
        .map(|g| {
            let rows_g = geom::rows_in_granule_at(rows, grain, g);
            let datums: &[u64] = match (&codes_regrain, dict_plan) {
                (Some(c), _) => &c[g as usize],
                (None, Some(d)) => &d.codes[g as usize],
                (None, None) => {
                    let start = g as usize * grain.rows() as usize;
                    &flat[start..start + rows_g as usize]
                }
            };
            pgrc_format::abi::EncodeInput {
                class,
                rows: rows_g,
                datums,
                validity: col.granule_validity(g, grain),
            }
        })
        .collect();

    // ---- band emission into scratch sections (ONE overflow region per
    // stream, the v1 single-extent law, spec §6.8), the MANDATORY
    // round-trip verify ([`VerifyBands`]) and the wrapper's band images.
    // K1-MEM (`band_pool`, a streamed seal's lone stream): the bands are
    // encoded first, then verified and wrapped on the pool while this
    // thread folds the meta (the classic pair's fold sequence, whose
    // witness bytes are the fused walk's); the bytes are the serial
    // seal's.
    let dict_payloads = match dict_plan {
        Some(d) => Some((
            crate::dict::section_payload(&d.images.index_section)?,
            crate::dict::section_payload(&d.images.payload_section)?,
            d.entry_count,
            d.images.charlen_form,
        )),
        None => None,
    };
    let observe_codes = dict_plan.is_some() && builder.dict_code_observe_supported();
    let bands = EncodeBands {
        plan: &full.plan,
        class,
        encoding,
        width,
        grain,
        granule_count,
        band_count,
        enc_inputs: &enc_inputs,
        ginputs: &ginputs,
        observe_codes,
    };
    let verify = VerifyBands { vt, key, flags, col, rows, grain, granule_count, ginputs: &ginputs, val_bytes: &val_bytes, dict: dict_payloads, attno, path_ord };
    // ---- wrapper decision (stream grain, exact bytes, the wrapper-
    // layer ≥20% law — O-CMP-4(a); the v4 DEFAULT on every full
    // election — SB-1: zstd-3 block-grain per data family, the
    // election table of 9b6ff701ae adopted verbatim; LZ4 dominated on
    // both axes and is no longer priced. Disk-only semantics: readers
    // rebuild the ENCODED image at extent open, kernels never see a
    // wrapper (O-CMP-5(a) unwrapped-image residency). A build without
    // the codec never elects it (the wasm arm's parity-honest
    // degradation).
    // The integer pre-pass (`PGRUST_PGRC_INTPRE`) is offered on value streams whose encoding has an integer view; its
    // blocks fall back to the plain zstd block, and a stream where none pre-passed settles to Zstd below.
    let wrap_arm = match pgrc_codec::intpre::on() && pgrc_codec::intpre::eligible(encoding, width) && wrapper_available(Wrapper::IntPre) {
        true => Wrapper::IntPre,
        false => Wrapper::Zstd,
    };
    let want_wrap = full.offer_wrapper && wrapper_available(wrap_arm);
    let (band_secs, ovf_buf, ovf_entries, verified, images, band_bytes): BandsOut = match band_pool {
        None => {
            let (band_secs, ovf_buf, ovf_entries, band_bytes) = bands.encode(Some(&mut *builder), &mut img)?;
            let mut verified = 0u64;
            let (datum_scratch, arena_words) = verify_scratch.get_or_insert_with(|| (Words::datums(col.owner()), Words::arena(col.owner())));
            for (bi, (sec, _, _)) in band_secs.iter().enumerate() {
                verified += verify.band(bi as u32, sec, ovf_buf.as_slice(), datum_scratch, arena_words)?;
                arena_words.sync();
            }
            let mut held = band_bytes;
            let images = match want_wrap {
                true => Some(
                    band_secs
                        .iter()
                        .map(|s| {
                            let mut out = Vec::new();
                            wrap_band(s, wrap_arm, &mut out)?;
                            held += out.capacity() as u64;
                            img.sync(held);
                            Ok(out)
                        })
                        .collect::<WriteResult<Vec<_>>>()?,
                ),
                false => None,
            };
            (band_secs, ovf_buf, ovf_entries, verified, images, band_bytes)
        }
        Some(pool) => {
            let (band_secs, ovf_buf, ovf_entries, band_bytes) = bands.encode(None, &mut img)?;
            let wrap = want_wrap.then_some(wrap_arm);
            let (verified, images) = verify.pooled_beside(pool, &band_secs, ovf_buf.as_slice(), wrap, (&mut img, band_bytes), &mut || {
                for g in 0..granule_count {
                    match observe_codes {
                        true => builder.observe_granule_dict_codes(&enc_inputs[g as usize], g),
                        false => builder.observe_granule(&ginputs[g as usize], g),
                    }
                }
            })?;
            (band_secs, ovf_buf, ovf_entries, verified, images, band_bytes)
        }
    };
    report.granules_verified += verified;
    // `bank`: the builder's fold when it walked every non-null value (the meta fold above), else the walk.
    if bank_pending {
        let fused = if bank_fused { builder.bank_fold_take() } else { None };
        match fused {
            Some(f) if f.digest().2 == stats.nonnull => bank_hash.merge(&f),
            _ => bank_hash = col.logical_hash()?,
        }
    }
    let mut wrapped_secs: Option<(Wrapper, Vec<Vec<u8>>)> = None;
    let mut held = band_bytes;
    if let Some(mut images) = images {
        let arm = pgrc_codec::intpre::settle(wrap_arm, &mut images).map_err(WriteError::Format)?;
        let unwrapped_total: usize = band_secs.iter().map(|(sec, _, _)| sec.len()).sum();
        let wrapped_total: usize = images.iter().map(|i| i.len()).sum();
        // the OnPair verbatim arm: every band's exact image, kept only when the stream's total is the smaller one
        let onpair = match class == StorageClass::VarlenaVerbatim && encoding == EncodingId::Verbatim.as_u16() && pgrc_codec::onpairwrap::onpair_verbatim_on() {
            true => {
                // the competing images at their lengths, not their worst-case bounds, while the OnPair images are built
                for i in images.iter_mut() {
                    i.shrink_to_fit(); // alloc: Exec charged img
                }
                let with_images = held + images.iter().map(|i| i.capacity() as u64).sum::<u64>();
                img.sync(with_images);
                onpair_value_images(&band_secs, &mut img, with_images)?
            }
            false => None,
        };
        let (arm, images) = match onpair {
            Some(op) if op.iter().map(|i| i.len()).sum::<usize>() < wrapped_total => (Wrapper::OnPairZstd, op),
            _ => (arm, images),
        };
        let total: usize = images.iter().map(|i| i.len()).sum();
        if wins_twenty_pct(total as u64, unwrapped_total as u64) {
            held += images.iter().map(|i| i.capacity() as u64).sum::<u64>();
            wrapped_secs = Some((arm, images));
        }
        img.sync(held);
    }
    let wrapper_byte: u8 = match &wrapped_secs {
        Some((arm, _)) => arm.as_u8(),
        None => Wrapper::None.as_u8(),
    };

    // ---- append value extents -----------------------------------------
    // K1-MEM B: the value bands are the stream's first bytes. Each band's image (the wrapped one when the wrapper won,
    // else the unwrapped section) moves into the stream's `head` as it is, 8-aligned — no copy into `buf`; an image the
    // decision above left unused goes with its band. Everything after the bands is built in `buf` at stream offset
    // `base` (rebased below).
    let mut value_extents: Vec<ExtentRecord> = Vec::new();
    let mut head = Pieces::default();
    let mut wrapped_images = wrapped_secs.map(|(_, images)| images.into_iter());
    for (bi, (sec, closeout, ends)) in band_secs.into_iter().enumerate() {
        drop_charged(&mut img, &mut held, ends);
        let b = bi as u32;
        let g0 = b * GRANULES_PER_BAND;
        let g1 = (g0 + GRANULES_PER_BAND).min(granule_count);
        // The wrapper CRC law: CRC over the WRAPPED bytes — exactly the
        // bytes the extent record addresses on disk.
        let (image, crc) = match wrapped_images.as_mut() {
            Some(images) => {
                drop_charged(&mut img, &mut held, sec);
                let image = images.next().ok_or(WriteError::Contract { detail: "a band without its wrapped image" })?;
                let crc = pgrc_format::wire::crc32c(&image);
                (image, crc)
            }
            None => (sec, closeout.crc),
        };
        let sec_start = head.pad8();
        let len = image.len();
        head.push(image);
        value_extents.push(ExtentRecord {
            file_off: sec_start as u64,
            len: len as u64,
            values: closeout.values,
            granule_start: g0,
            granule_count: g1 - g0,
            crc,
            flags: 0,
        });
        sections.push(SectionEntry {
            off: sec_start as u64,
            len: len as u64,
            kind: SectionKind::Stream.as_u16(),
            flags: 0,
            attno,
            path_ord,
            crc,
        });
    }
    // Zeros to 8 (every section after pads first; the bands are never the stream's last bytes).
    let base = head.pad8() as u64;
    img.sync(image_bytes(&head, &buf));
    let rest_from = sections.len();

    // ---- the ONE overflow extent (spec §6.8 v1 single-extent law).
    // Built in scratch so the byte-run wrapper offer (CMP-B class
    // widening) can price the finished section; gated on the same
    // `offer_wrapper` the value stream carries (the stats-only fallback
    // never wraps — reference-grade bindings keep reading its parts). --
    let mut overflow_extents: Vec<ExtentRecord> = Vec::new();
    let mut overflow_entries_total: u64 = 0;
    let mut overflow_wrapper: u8 = Wrapper::None.as_u8();
    if !ovf_buf.is_empty() {
        // SEAL-FUSION (#22): the count was accumulated at append time;
        // the header re-walk survives only as a debug self-check.
        let entries = ovf_entries;
        debug_assert_eq!(
            entries,
            count_overflow_entries(ovf_buf.as_slice())?,
            "overflow sink count drifted from the region census"
        );
        overflow_entries_total = entries;
        // CONVERT W3-2a (cvformat A2): the overflow section on the part's owner
        let mut osec: ChargedVec<u8> = ChargedVec::new(col.owner(), Kind::Region);
        let closeout = {
            let mut w = StreamSectionWriter::begin_charged(
                &mut osec,
                EncodingId::Verbatim.as_u16(),
                0,
                Wrapper::None,
            )?;
            w.payload().extend_from_slice(ovf_buf.as_slice());
            w.end_granule(entries as u32);
            w.finish(false)?
        };
        let wrapped = if full.offer_wrapper {
            wrap_byte_run_section(&osec, None, col.owner())?
        } else {
            None
        };
        // The wrapper CRC law: CRC over the bytes the extent record
        // addresses on disk.
        let (bytes, crc): (&[u8], u32) = match &wrapped {
            Some((arm, image)) => {
                overflow_wrapper = arm.as_u8();
                (image.as_slice(), pgrc_format::wire::crc32c(image))
            }
            None => (osec.as_slice(), closeout.crc),
        };
        pad_to(&mut buf, 8);
        let osec_start = buf.len();
        buf.extend_from_slice(bytes);
        img.sync(image_bytes(&head, &buf));
        overflow_extents.push(ExtentRecord {
            file_off: osec_start as u64,
            len: bytes.len() as u64,
            values: entries,
            granule_start: 0,
            granule_count,
            crc,
            flags: 0,
        });
        sections.push(SectionEntry {
            off: osec_start as u64,
            len: bytes.len() as u64,
            kind: SectionKind::Stream.as_u16(),
            flags: 0,
            attno,
            path_ord,
            crc,
        });
        flags |= STREAMF_HAS_OVERFLOW;
    }

    // ---- dict sections (single extents each, spec §7; section headers
    // stamp DICT_CODES — the family id; layout is role-driven) ----------
    let mut dict_stream_plans: Vec<StreamPlan> = Vec::new();
    if let Some(d) = dict_plan {
        let pairs: [(StreamRole, &Vec<u8>, &pgrc_format::part::StreamCloseout); 2] = [
            (
                StreamRole::DictIndex,
                &d.images.index_section,
                &d.images.index_closeout,
            ),
            (
                StreamRole::DictPayload,
                &d.images.payload_section,
                &d.images.payload_closeout,
            ),
        ];
        for (role, image, closeout) in pairs {
            // CMP-B wrapper-offer class widening: the byte-run dict
            // sections take the same per-stream offer as the value
            // stream (dict PAYLOAD is the bank's single largest byte
            // sink — §9.2 of the compression doc). Verify ran over the
            // UNWRAPPED images above (`verify_sections`); wrapping is
            // strictly downstream of verify, like the value path.
            let wrapped = if full.offer_wrapper {
                let index = crate::dict::section_payload(&d.images.index_section)?;
                wrap_byte_run_section(image, Some((role, index, d.images.charlen_form)), col.owner())?
            } else {
                None
            };
            let (dict_wrapper, bytes, crc): (u8, &[u8], u32) = match &wrapped {
                Some((arm, w_image)) => (
                    arm.as_u8(),
                    w_image.as_slice(),
                    pgrc_format::wire::crc32c(w_image),
                ),
                None => (Wrapper::None.as_u8(), image.as_slice(), closeout.crc),
            };
            image_room(&mut buf, bytes.len() + 8)?;
            pad_to(&mut buf, 8);
            let s = buf.len();
            buf.extend_from_slice(bytes);
            sections.push(SectionEntry {
                off: s as u64,
                len: bytes.len() as u64,
                kind: SectionKind::Stream.as_u16(),
                flags: 0,
                attno,
                path_ord,
                crc,
            });
            // SB-7 (frame-boundary dict extents): the UNWRAPPED
            // DictPayload stream's extent table is cut at dict-frame
            // boundaries — extent i covers frame i's byte run (extent 0
            // additionally carries the section header; the last extent
            // carries the frame-table tail), each with its own CRC, so
            // `ensure_frame` has a REAL per-frame fault-and-validate
            // grain from birth (PC-3.2: a claimed span faults frames,
            // never whole payloads). A WRAPPED payload stays
            // single-extent by design: O-CMP-5(a) unwrap is
            // whole-section + part-resident image cache (P-6), so the
            // frame grain there is the unwrapped image's, not the
            // disk's. DictIndex stays single-extent (12-B closed-form
            // stride; it is the small always-resident half).
            let extents: Vec<ExtentRecord> = if role == StreamRole::DictPayload
                && wrapped.is_none()
            {
                dict_frame_extents(image, s as u64, d.entry_count, col.owner())?
            } else {
                vec![ExtentRecord {
                    file_off: s as u64,
                    len: bytes.len() as u64,
                    values: d.entry_count as u64,
                    granule_start: 0,
                    granule_count,
                    crc,
                    flags: 0,
                }]
            };
            dict_stream_plans.push(StreamPlan {
                attno,
                path_ord,
                role,
                // M5d char-len record: the DictIndex stream self-
                // describes its `char_field` form (spec §6.3) — readers
                // consult the flags, never a writer posture. Absolute
                // (the disarmed default) stamps 0, byte-for-byte the
                // pre-M5d dir entry.
                flags: if role == StreamRole::DictIndex {
                    d.images.charlen_form.flag_bits()
                } else {
                    0
                },
                encoding: EncodingId::DictCodes.as_u16(),
                class_id: class.id(),
                width: 0,
                fixed_len: 0,
                aux32: 0,
                wrapper: dict_wrapper,
                values: d.entry_count as u64,
                extents,
            });
        }
    }

    // Validity stream: present iff ≥1 NULL in this part (spec §6.1).
    let validity_extent = emit_validity_stream(
        &mut buf,
        &mut sections,
        col,
        &val_bytes,
        attno,
        path_ord,
        rows,
        grain,
        granule_count,
    )?;

    // D3070: each granule's lane as stored (its rows' share of its band's
    // value extent, plus its rows' share of the overflow extent), the
    // bytes a probe reads without a bloom: the builder prices the part's
    // equality blooms against them.
    {
        let overflow: u64 = overflow_extents.iter().map(|e| e.len).sum();
        let mut stored: ChargedVec<u64> = ChargedVec::new(col.owner(), Kind::Region);
        stored.with_vec(|v| v.try_reserve_exact(granule_count as usize)).map_err(|_| WriteError::Format(pgrc_format::FormatError::OutOfMemory { bytes: granule_count as usize * core::mem::size_of::<u64>() }))?; // alloc: Exec charged stored.owner()
        stored.with_vec(|v| v.resize(granule_count as usize, 0)); // alloc: Exec charged stored.owner()
        for e in &value_extents {
            let g1 = (e.granule_start + e.granule_count).min(granule_count);
            let band_rows: u64 = (e.granule_start..g1).map(|g| geom::rows_in_granule_at(rows, grain, g) as u64).sum();
            for g in e.granule_start..g1 {
                let r = geom::rows_in_granule_at(rows, grain, g) as u64;
                stored[g as usize] += e.len * r / band_rows.max(1);
            }
        }
        for (g, st) in stored.iter_mut().enumerate() {
            let r = geom::rows_in_granule_at(rows, grain, g as u32) as u64;
            *st += overflow * r / rows.max(1);
        }
        builder.set_lane_stored_bytes(&stored);
    }
    // ---- two-witness null law + stats section (spec §6.6/§8.1) --------
    emit_stats_sections(
        &mut buf,
        &mut sections,
        &mut report,
        builder,
        col,
        &val_pops,
        attno,
        path_ord,
        rows,
        grain,
        granule_count,
        band_count,
        // pgrc format 2.1 §2.2: the dict entry count IS the exact per-part NDV
        // when this stream elected DICT_CODES.
        dict_plan.map(|d| d.entry_count as u64),
        &mut plane_slices,
        &mut facts_slices,
    )?;
    // sortpath (code-keyed zones): a DICT_CODES stream's per-granule [min, max] code over its valid rows + the part
    // rollup, OPTIONAL and raw; the codes are byte-rank ordered, so a reader decides a text compare exactly where the
    // Coarse prefix keys tie.
    if dict_plan.is_some() {
        let body = pgrc_format::meta::codezones_encode(&code_zones(&enc_inputs))?;
        push_optional_raw_section(&mut buf, &mut sections, SectionKind::CodeZones, attno, path_ord, &body)?;
    }

    // K1-MEM B: what was built in `buf` sits after the bands.
    for sec in &mut sections[rest_from..] {
        sec.off += base;
    }
    let rebase = |e: &mut ExtentRecord| e.file_off += base;
    overflow_extents.iter_mut().for_each(rebase);
    dict_stream_plans.iter_mut().flat_map(|p| p.extents.iter_mut()).for_each(rebase);
    let validity_extent = validity_extent.map(|mut e| {
        rebase(&mut e);
        e
    });

    // ---- stream plans (directory rows) ---------------------------------
    plans.push(StreamPlan {
        attno,
        path_ord,
        role: StreamRole::Values,
        encoding,
        flags,
        class_id: class.id(),
        width,
        fixed_len: class.fixed_len(),
        aux32: full.aux32,
        wrapper: wrapper_byte,
        values: rows,
        extents: value_extents,
    });
    if let Some(ve) = validity_extent {
        plans.push(StreamPlan {
            attno,
            path_ord,
            role: StreamRole::Validity,
            encoding: EncodingId::BoolBitmap.as_u16(),
            flags: 0,
            class_id: class.id(),
            width: 0,
            fixed_len: 0,
            aux32: 0,
            wrapper: 0,
            values: rows,
            extents: vec![ve],
        });
    }
    if !overflow_extents.is_empty() {
        plans.push(StreamPlan {
            attno,
            path_ord,
            role: StreamRole::Overflow,
            encoding: EncodingId::Verbatim.as_u16(),
            flags: 0,
            class_id: class.id(),
            width: 0,
            fixed_len: 0,
            aux32: 0,
            wrapper: overflow_wrapper,
            values: overflow_entries_total,
            extents: overflow_extents,
        });
    }
    plans.append(&mut dict_stream_plans);

    // TY-3 (OD-4 two-arm election): a jsonb root that DERIVED typed
    // shred lanes this part records the STRUCTURAL JsonbShred election
    // as its witness — honestly: the lanes were actually emitted (they
    // ride this very stream list at path_ord ≥ 1), the image lane
    // remains the read truth (dual-store law, O-4), and the on-disk
    // parent StreamEntry keeps its text-plane encoding. baseline/chosen
    // stay the image-lane election's exact bytes.
    let mut witness = full.witness;
    if path_ord == 0 && sc.shredded.contains(&attno) {
        witness.encoding = EncodingId::JsonbShred.as_u16();
    }
    report.elections.push(witness);
    if let Some(n) = full.note {
        report.notes.push(n);
    }
    report
        .col_hashes
        .push((attno, path_ord, bank_hash.digest()));
    // ST-1: the stream's distribution sketch now (K1-MEM), so the caller can drop its builder with the stream.
    let dist = builder.distribution().map(|d| (attno, path_ord, d));
    img.sync(image_bytes(&head, &buf));
    Ok(StreamOut { charge: img, head, buf, sections, plans, report, plane_slices, facts_slices, dist })
}

/// One band's scratch section: its image, closeout and granule payload ends.
type BandSec = (Vec<u8>, StreamCloseout, Vec<u32>);

/// R15-SEAL-IMAGE-CHARGE: `v`'s bytes out of the stream's image charge (`held` of them), then `v` freed: the charge gives
/// them back to this thread before the free lands on it.
fn drop_charged<T>(img: &mut HeldCharge, held: &mut u64, v: Vec<T>) {
    *held = held.saturating_sub(cap_bytes(v.capacity(), &v));
    img.sync(*held);
    drop(v);
}

/// The bytes a stream's image holds: its value bands (`head`) and the rest (`buf`).
fn image_bytes(head: &Pieces, buf: &Vec<u8>) -> u64 {
    let parts: usize = head.parts.iter().map(|p| p.capacity()).sum();
    (parts + head.parts.capacity() * core::mem::size_of::<Vec<u8>>() + buf.capacity()) as u64
}
/// A stream's bands: sections, the overflow region and its entry count, the granules verified and the wrapper's band
/// images (when the election offers a wrapper).
/// R15-SEAL-IMAGE-CHARGE: and the bytes of the bands the stream's image charge holds.
type BandsOut = (Vec<BandSec>, ChargedVec<u8>, u64, u64, Option<Vec<Vec<u8>>>, u64);

/// The band emission of a stream's election ([`seal_stream`]).
struct EncodeBands<'a> {
    plan: &'a ElectPlan,
    class: StorageClass,
    encoding: u16,
    width: u8,
    grain: GranuleGrain,
    granule_count: u32,
    band_count: u32,
    enc_inputs: &'a [pgrc_format::abi::EncodeInput<'a>],
    ginputs: &'a [pgrc_format::abi::EncodeInput<'a>],
    /// A dict stream whose builder folds from the codes (D-STATS).
    observe_codes: bool,
}

impl EncodeBands<'_> {
    /// Every band's section and the overflow region, the meta folds fed to `observer` as they go (`None`: the caller
    /// folds them itself, [`seal_stream_beside`]).
    /// R15-SEAL-IMAGE-CHARGE: the bands' bytes go onto `img` as each granule is written (the overflow region onto its own
    /// `ChargedVec`, the same owner); returns them with the bands.
    fn encode(&self, mut observer: Option<&mut dyn ColumnMetaBuilder>, img: &mut HeldCharge) -> WriteResult<(Vec<BandSec>, ChargedVec<u8>, u64, u64)> {
        let (class, encoding, width, grain, granule_count) = (self.class, self.encoding, self.width, self.grain, self.granule_count);
        let (enc_inputs, ginputs) = (self.enc_inputs, self.ginputs);
        let dict = matches!(self.plan, ElectPlan::Dict(_));
        let mut ovf_buf: ChargedVec<u8> = ChargedVec::new(img.owner(), Kind::Region);
        let mut band_secs: Vec<BandSec> = Vec::new();
        let mut held = 0u64;
        let ovf_entries: u64;
        {
            let mut ovf = OverflowSink::new_charged(&mut ovf_buf);
            for b in 0..self.band_count {
                let g0 = b * GRANULES_PER_BAND;
                let g1 = (g0 + GRANULES_PER_BAND).min(granule_count);
                let mut sec: Vec<u8> = Vec::new();
                let mut w = StreamSectionWriter::begin(&mut sec, encoding, width, Wrapper::None)?;
                let mut enc: Box<dyn GranuleEncoder> = match self.plan {
                    ElectPlan::Verbatim => Box::new(VerbatimEncoder { class }),
                    // SEAL-FUSION: the POSITIONED face — carry-aware
                    // factories hand the encoder its cursor into the
                    // election's carried facts (per-frame refs/widths, ALP
                    // frames); carry-free factories fall
                    // through to `make`.
                    ElectPlan::Encoder(f) => f.make_at(g0, grain.rows()),
                    ElectPlan::Dict(d) => Box::new(pgrc_codec::dictcodes::DictCodesEncoder {
                        class: class.id(),
                        max_width: d.max_width,
                        byte_align: d.byte_align,
                    }),
                };
                let mut ends: Vec<u32> = Vec::with_capacity((g1 - g0) as usize);
                for g in g0..g1 {
                    // SEAL-SPEED-2 fold fusion (ruled 2026-08-12): the meta
                    // folds RIDE the encode walk for value-currency streams
                    // — one iteration produces encoded bytes AND feeds the
                    // builder (fused overrides in verbatim/bytefor/deltafor;
                    // families whose emit is a carried memcpy fall through
                    // to the classic pair inside the default face). Dict
                    // streams observe the VALUE currency — via the D-STATS
                    // fold-from-codes face when the builder offers it (the
                    // codes ARE the values through the byte-rank order
                    // certificate + the DICT-DEDUP feed's entry table;
                    // seal-fusion charter §4 cut 3), via the classic
                    // hydrated-value pair otherwise (D2 inherit, feed-off
                    // control, `PGRUST_SEAL_BATCH_FOLD=0`, stand-ins).
                    // Witness bytes are switch-invisible on every arm (the
                    // fold facts are pure functions of the data);
                    // `PGRUST_SEAL_FUSE_FOLD=0` kills back to the classic
                    // pair.
                    match observer.as_deref_mut() {
                        Some(builder) if !dict && fuse_fold_enabled() => {
                            enc.encode_granule_observed(&enc_inputs[g as usize], &mut w, &mut ovf, builder, g)?;
                        }
                        Some(builder) => {
                            enc.encode_granule(&enc_inputs[g as usize], &mut w, &mut ovf)?;
                            if self.observe_codes {
                                // enc_inputs for a dict stream IS the codes
                                // currency (rows + validity + global codes).
                                builder.observe_granule_dict_codes(&enc_inputs[g as usize], g);
                            } else {
                                builder.observe_granule(&ginputs[g as usize], g);
                            }
                        }
                        None => enc.encode_granule(&enc_inputs[g as usize], &mut w, &mut ovf)?,
                    }
                    ends.push(w.payload_off());
                    img.sync(held + (w.payload().capacity() + ends.capacity() * 4) as u64);
                }
                enc.finish_stream(&mut w)?;
                if let Some(last) = ends.last_mut() {
                    // CONST closes out in finish_stream; fold trailing bytes
                    // into the final granule's block for wrapper purposes.
                    *last = w.payload_off();
                }
                // SB-10: at a NON-DEFAULT grain, multi-frame-per-granule
                // families get the gcount table on ROOT sections too — the
                // frozen kernels' `value_base`/`frame_base` closed forms are
                // capacity-keyed at 8192, while their gcount arm is
                // grain-exact. Granule-framed families (CONST/ALP/ALP_RD/
                // BOOL_BITMAP) address by granule ordinal —
                // grain-proof without (and `granule_frame_base` refuses) a
                // table. At the default grain nothing changes: v3 bytes
                // exactly.
                let emit_gcounts = !grain.is_default() && root_gcounts_required(encoding);
                let closeout = w.finish(emit_gcounts)?;
                held += (sec.capacity() + ends.capacity() * 4) as u64;
                band_secs.push((sec, closeout, ends));
                img.sync(held + cap_bytes(band_secs.capacity(), &band_secs));
            }
            ovf_entries = ovf.entries();
        }
        held += cap_bytes(band_secs.capacity(), &band_secs);
        Ok((band_secs, ovf_buf, ovf_entries, held))
    }
}

/// The MANDATORY round-trip verify, per granule, over a stream's UNWRAPPED band sections (the exact image kernels see
/// post-unwrap). Dict streams verify FULL MATERIALIZATION against the original varlena inputs (ctx carries the emitted
/// dict sections).
struct VerifyBands<'a> {
    vt: &'static CodecVtable,
    key: KernelKey,
    flags: u16,
    col: &'a ColBuffer,
    rows: u64,
    grain: GranuleGrain,
    granule_count: u32,
    ginputs: &'a [pgrc_format::abi::EncodeInput<'a>],
    val_bytes: &'a [u8],
    dict: Option<(&'a [u8], &'a [u8], u32, DictCharLenForm)>,
    attno: u32,
    path_ord: u32,
}

impl VerifyBands<'_> {
    /// Band `b` (section `sec`, the stream's overflow region `ovf`): the granules verified.
    fn band(&self, b: u32, sec: &[u8], ovf: &[u8], datum_scratch: &mut [u64], arena_words: &mut Vec<u64>) -> WriteResult<u64> {
        let (col, grain, class) = (self.col, self.grain, self.col.schema.class);
        let g0 = b * GRANULES_PER_BAND;
        let g1 = (g0 + GRANULES_PER_BAND).min(self.granule_count);
        let hdr = StreamSectionHdr::decode(sec)?;
        let ft = hdr.frame_table(sec)?;
        for g in g0..g1 {
            let rows_g = geom::rows_in_granule_at(self.rows, grain, g);
            let input = &self.ginputs[g as usize];
            let bound = (col.granule_arena_bound(g, grain) + 64) as usize;
            if arena_words.len() * 8 < bound {
                arena_words.resize(bound.div_ceil(8), 0);
            }
            let arena_buf: &mut [u8] = words_as_bytes(arena_words);
            let ctx = KernelCtx {
                key: self.key,
                flags: self.flags,
                fixed_len: class.fixed_len(),
                bytes: sec,
                frame_table: ft.as_deref(),
                granule: g,
                granule_in_extent: g - g0,
                rows: rows_g,
                values: rows_g,
                validity_bytes: validity_slice(col, self.val_bytes, g, rows_g, grain),
                overflow: if ovf.is_empty() { None } else { Some(ovf) },
                dict: self.dict.map(|(index, payload, entry_count, charlen_form)| DictSections {
                    index,
                    payload,
                    entry_count,
                    charlen_form,
                    form: pgrc_format::dict::DictPayloadForm::Expanded,
                }),
            };
            verify_roundtrip(self.vt, &ctx, input, datum_scratch, arena_buf).map_err(|cause| WriteError::RoundTrip {
                attno: self.attno,
                path_ord: self.path_ord,
                granule: g,
                cause,
            })?;
            // SEAL-FUSION (walks #13/#14 dropped): the dict
            // `decode_codes` cross-witness re-decoded every granule and
            // compared decoded codes against the elected code arrays.
            // The full-materialization verify above already proves more:
            // through the reader's own kernel it resolves every STORED
            // code to its value and compares canonical bytes against the
            // original varlena inputs — and dict entries are strictly
            // distinct (byte-rank sorted, `verify_sections`-proven), so
            // value identity IMPLIES code identity. The reader-kernel
            // decode_full+compare leg is untouched — that structure is
            // the bank-integrity tooth.
        }
        Ok((g1 - g0) as u64)
    }

    /// Every band of `secs` verified, and wrapped under `wrap` when that is `Some`, on `pool` while the caller runs
    /// `beside`: the granules verified and the band images, or the first error in band order, verify before wrap (the
    /// serial seal's).
    fn pooled_beside(
        &self,
        pool: &dyn ColumnPool,
        secs: &[BandSec],
        ovf: &[u8],
        wrap: Option<Wrapper>,
        (img, held): (&mut HeldCharge, u64),
        beside: &mut dyn FnMut(),
    ) -> WriteResult<(u64, Option<Vec<Vec<u8>>>)> {
        type Slot<T> = pgsync::Mutex<Option<WriteResult<T>>>;
        let verified: Vec<Slot<u64>> = secs.iter().map(|_| pgsync::Mutex::new(None)).collect();
        // C1-REG: each band's wrapped image is allocated here, at its worst-case size, and a worker only fills it: the
        // image is freed later by this thread, so no worker keeps its pages (an idle rtpool worker returns a block freed
        // by another thread only when it next allocates).
        let wrapped: Vec<Slot<Vec<u8>>> = secs.iter().map(|_| pgsync::Mutex::new(None)).collect();
        // R15-SEAL-IMAGE-CHARGE: the images' bound on the stream's image charge before they are allocated
        if let Some(arm) = wrap {
            img.sync(held + secs.iter().map(|(sec, _, ends)| pgrc_codec::wrapper::wrap_section_bound(sec, ends, arm) as u64).sum::<u64>());
        }
        let outs: Vec<pgsync::Mutex<Vec<u8>>> = match wrap {
            Some(arm) => secs.iter().map(|(sec, _, ends)| pgsync::Mutex::new(Vec::with_capacity(pgrc_codec::wrapper::wrap_section_bound(sec, ends, arm)))).collect(),
            None => Vec::new(),
        };
        let tasks = if wrap.is_some() { 2 * secs.len() } else { secs.len() };
        pool.run_beside(
            tasks,
            &|i| match (i < secs.len(), wrap) {
                (true, _) => {
                    let (mut scratch, mut arena) = (Words::datums(self.col.owner()), Words::arena(self.col.owner()));
                    let out = self.band(i as u32, &secs[i].0, ovf, &mut scratch, &mut arena);
                    *pgsync::lock(&verified[i]) = Some(out);
                }
                (false, Some(arm)) => {
                    let b = i - secs.len();
                    let mut out = std::mem::take(&mut *pgsync::lock(&outs[b]));
                    let cap = out.capacity();
                    let r = wrap_band(&secs[b], arm, &mut out);
                    debug_assert!(r.is_err() || out.capacity() == cap, "a band's wrap outgrew wrap_section_bound");
                    *pgsync::lock(&wrapped[b]) = Some(r.map(|()| out));
                }
                (false, None) => {}
            },
            beside,
        );
        fn take<T>(slot: Slot<T>, what: &'static str) -> WriteResult<T> {
            slot.into_inner().unwrap_or_else(|e| e.into_inner()).ok_or(WriteError::Contract { detail: what })?
        }
        let mut n = 0u64;
        for slot in verified {
            n += take(slot, "a pool skipped a band's verify")?;
        }
        let images = match wrap {
            Some(_) => Some(wrapped.into_iter().map(|slot| take(slot, "a pool skipped a band's wrap")).collect::<WriteResult<Vec<_>>>()?),
            None => None,
        };
        Ok((n, images))
    }
}

/// Every band's `Wrapper::OnPairZstd` image (charged on `img` above `held` as each is built), or `None` when a
/// band does not offer the arm (switch off, small, or a layout the arm does not carry).
fn onpair_value_images(secs: &[BandSec], img: &mut HeldCharge, mut held: u64) -> WriteResult<Option<Vec<Vec<u8>>>> {
    let mut out = Vec::new();
    for (sec, _, _) in secs {
        match pgrc_codec::onpairvals::offer_values(sec).map_err(WriteError::Format)? {
            Some(op) => {
                held += op.capacity() as u64;
                img.sync(held);
                out.push(op); // alloc: Exec charged img
            }
            None => return Ok(None),
        }
    }
    Ok(Some(out))
}

/// One band's wrapper image under `arm`, appended to `out`.
fn wrap_band((sec, _, ends): &BandSec, arm: Wrapper, out: &mut Vec<u8>) -> WriteResult<()> {
    pgrc_codec::wrapper::wrap_section(sec, ends, arm, out).map_err(WriteError::Format)
}

/// ST-1 (OD-2) + D4400: a sealed stream's distribution sketch (`ord` = its part-global path_ord, `None` = the stream was not
/// sealed) onto `out`, its builder releasing the distinct set it read it from.
#[allow(dead_code)] // V5SYNC r14: trunk's seal helper (loadstats D Sample / D4400 / D4409), not yet wired into the line's seal
fn take_distribution(
    builder: &mut Box<dyn ColumnMetaBuilder>,
    attno: u32,
    ord: Option<u32>,
    out: &mut Vec<(u32, u32, pgrc_format::sidecar::ColDistribution)>,
) {
    let Some(path_ord) = ord else { return };
    if let Some(d) = builder.distribution() {
        out.push((attno, path_ord, d));
    }
}

/// Append one META-PLANE section (Stats / Psma / Bloom / NdvRegisters —
/// the CMP-F wrapped meta-section class, SB-6): the body ships in the
/// zstd meta envelope iff the wrap clears the SB-2 ≥20% gate at this
/// section's own grain; `SECTIONF_META_ZSTD` marks the stored form and the
/// CRC stays over the STORED bytes (the wrapper CRC law). The structural
/// plane (PathTable / SortKey / StreamDir / section table / footer) stays
/// raw — CMP-F's claw is the census's measured meta-section class
/// (~2.1GB at 100m), not the struct-arithmetic residual; the two byte
/// classes are deliberately not conflated. A build without zstd ships raw
/// (the wasm parity-honest degradation, exactly the value path's).
fn push_meta_section(
    buf: &mut Vec<u8>,
    sections: &mut Vec<SectionEntry>,
    kind: SectionKind,
    attno: u32,
    path_ord: u32,
    body: &[u8],
) -> WriteResult<()> {
    if let Some(env) = pgrc_codec::wrapper::meta_wrap_body(body) {
        if wins_twenty_pct(env.len() as u64, body.len() as u64) {
            image_room(buf, env.len() + 8)?;
            pad_to(buf, 8);
            let off = buf.len() as u64;
            buf.extend_from_slice(&env);
            sections.push(SectionEntry {
                off,
                len: env.len() as u64,
                kind: kind.as_u16(),
                flags: SECTIONF_META_ZSTD,
                attno,
                path_ord,
                crc: pgrc_format::wire::crc32c(&env),
            });
            return Ok(());
        }
    }
    push_raw_section(buf, sections, kind, attno, path_ord, body)
}

/// [`push_meta_section`] for an OPTIONAL kind (spec §8.9 `Sample`): the zstd meta envelope when it wins the SB-2 gate,
/// else raw; `SECTION_OPTIONAL` either way, so a reader without the kind skips it.
/// Test apparatus: [`set_sample_section_off`]'s process-wide switch (the `PGRUST_PGRC_SAMPLE=0` arm without the
/// environment, which a test process cannot set safely while other threads read it).
static SAMPLE_SECTION_OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Seal this process's parts without the spec §8.9 Sample section (`off`), or with it again. For tests whose premise
/// is two parts of the same byte length from different values: the section's zstd-wrapped body depends on the values.
pub fn set_sample_section_off(off: bool) {
    SAMPLE_SECTION_OFF.store(off, std::sync::atomic::Ordering::Relaxed);
}

/// A seal-time A/B kill switch: `name` set to `0` / `off` seals without its section (default ON; production never
/// sets these). The populate switches (OD-10/OD-11) and the Sample switch (spec §8.9) read through this one site.
fn seal_switch_off(name: &str) -> bool {
    matches!(std::env::var(name).as_deref(), Ok("0") | Ok("off"))
}

#[allow(dead_code)] // V5SYNC r14: trunk's seal helper (loadstats D Sample / D4400 / D4409), not yet wired into the line's seal
fn push_optional_meta_section(
    buf: &mut Vec<u8>,
    sections: &mut Vec<SectionEntry>,
    kind: SectionKind,
    attno: u32,
    path_ord: u32,
    body: &[u8],
) -> WriteResult<()> {
    let at = sections.len();
    push_meta_section(buf, sections, kind, attno, path_ord, body)?;
    sections[at].flags |= pgrc_format::part::SECTION_OPTIONAL;
    Ok(())
}

/// One root value's datum image for the Sample section and its seed digest: a by-value word's 8 LE bytes, a Fixed(N)
/// image, a varlena's 4-B header + payload (the shape the scan decodes, spec §1); `None` = NULL.
#[allow(dead_code)] // V5SYNC r14: trunk's seal helper (loadstats D Sample / D4400 / D4409), not yet wired into the line's seal
fn sample_image(col: &ColBuffer, row: u64) -> WriteResult<Option<Vec<u8>>> {
    if !col.valid_at(row) {
        return Ok(None);
    }
    Ok(Some(match col.schema.class {
        StorageClass::Fixed { .. } => {
            pgrc_format::try_to_vec(col.fixed_at(row)?.unwrap_or(&[]), "sample image").map_err(WriteError::Format)?
        }
        StorageClass::VarlenaVerbatim => {
            let p = col.varlena_payload(row)?.unwrap_or(&[]);
            let mut v = pgrc_format::try_vec(4 + p.len(), "sample image").map_err(WriteError::Format)?;
            v.extend_from_slice(&pgrc_format::wire::varlena_header_4b_u(p.len() as u32).to_le_bytes());
            v.extend_from_slice(p);
            v
        }
        _ => pgrc_format::try_to_vec(&col.word_at(row).to_le_bytes(), "sample image").map_err(WriteError::Format)?,
    }))
}

/// The Sample section body (spec §8.9): `min(256, rows)` rows drawn by a seed from the schema fingerprint, the row
/// count and the first root's values at its first, middle and last rows; every root column, ascending attno.
#[allow(dead_code)] // V5SYNC r14: trunk's seal helper (loadstats D Sample / D4400 / D4409), not yet wired into the line's seal
fn sample_body(roots: &[ColBuffer], fp: u64, rows: u64) -> WriteResult<Vec<u8>> {
    use pgrc_format::sample::{sample_digest, sample_ordinals, sample_seed, SampleValues, SampleWriter};
    let mut digest = 0u64;
    for r in [0, rows / 2, rows - 1] {
        digest = sample_digest(digest, sample_image(&roots[0], r)?.as_deref());
    }
    let ords = sample_ordinals(rows, sample_seed(fp, rows, digest)).map_err(WriteError::Format)?;
    let mut order: ChargedVec<usize> = ChargedVec::new(roots.first().and_then(ColBuffer::owner), Kind::Region);
    order.with_vec(|v| v.try_reserve_exact(roots.len())).map_err(|_| WriteError::Format(pgrc_format::FormatError::OutOfMemory { bytes: roots.len() * core::mem::size_of::<usize>() }))?; // alloc: Exec charged order.owner()
    order.with_vec(|v| v.extend(0..roots.len())); // alloc: Exec charged order.owner()
    order.with_vec(|v| v.sort_by_key(|&i| roots[i].schema.attno)); // alloc: Exec charged order.owner()
    let mut w = SampleWriter::new(pgrc_format::try_to_vec(&ords, "sample ordinals").map_err(WriteError::Format)?);
    for &i in order.iter() {
        let col = &roots[i];
        match col.schema.class {
            StorageClass::Fixed { .. } | StorageClass::VarlenaVerbatim => {
                let mut own: Vec<Option<Vec<u8>>> =
                    pgrc_format::try_vec(ords.len(), "sample images").map_err(WriteError::Format)?; // alloc: Exec charged sample_charge
                for &o in &ords {
                    own.push(sample_image(col, o as u64)?); // alloc: Exec charged sample_charge
                }
                let mut refs: Vec<Option<&[u8]>> =
                    pgrc_format::try_vec(ords.len(), "sample images").map_err(WriteError::Format)?; // alloc: Exec charged sample_charge
                refs.extend(own.iter().map(|v| v.as_deref())); // alloc: Exec charged sample_charge
                // CONVERT W3-2a: one column's sampled value images on the part's owner while the writer copies them
                // (declared after them: drops first)
                let mut sample_charge = HeldCharge::new(col.owner());
                sample_charge.sync(
                    cap_bytes(own.capacity(), &own)
                        + own.iter().map(|v| v.as_ref().map_or(0, |b| b.capacity() as u64)).sum::<u64>()
                        + cap_bytes(refs.capacity(), &refs),
                );
                w.add(col.schema.attno, SampleValues::Images(&refs)).map_err(WriteError::Format)?;
            }
            _ => {
                let mut words: ChargedVec<Option<u64>> = ChargedVec::new(col.owner(), Kind::Region);
                words.with_vec(|v| v.try_reserve_exact(ords.len())).map_err(|_| WriteError::Format(pgrc_format::FormatError::OutOfMemory { bytes: ords.len() * core::mem::size_of::<Option<u64>>() }))?; // alloc: Exec charged words.owner()
                words.with_vec(|v| v.extend(ords.iter().map(|&o| col.valid_at(o as u64).then(|| col.word_at(o as u64))))); // alloc: Exec charged words.owner()
                w.add(col.schema.attno, SampleValues::Words(&words)).map_err(WriteError::Format)?;
            }
        }
    }
    w.finish().map_err(WriteError::Format)
}

/// Append one raw OPTIONAL section (pgrc format 2.1 new kinds — FlatStats/…): the
/// `SECTION_OPTIONAL` flag makes pre-2.1 readers skip it typed instead of
/// refusing the part (spec §5.2 forward-compat law); the body stays raw so
/// mmap-cast consumers see the arrays directly (never `SECTIONF_META_ZSTD`).
fn push_optional_raw_section(
    buf: &mut Vec<u8>,
    sections: &mut Vec<SectionEntry>,
    kind: SectionKind,
    attno: u32,
    path_ord: u32,
    body: &[u8],
) -> WriteResult<()> {
    image_room(buf, body.len() + 8)?;
    pad_to(buf, 8);
    let off = buf.len() as u64;
    buf.extend_from_slice(body);
    sections.push(SectionEntry {
        off,
        len: body.len() as u64,
        kind: kind.as_u16(),
        flags: pgrc_format::part::SECTION_OPTIONAL,
        attno,
        path_ord,
        crc: pgrc_format::wire::crc32c(body),
    });
    Ok(())
}

/// D4409 (b): one band of a stream offered a wrapper, kept in the form it wins in alone until the stream-level decision.
#[allow(dead_code)] // V5SYNC r14: trunk's seal helper (loadstats D Sample / D4400 / D4409), not yet wired into the line's seal
enum BandForm {
    /// Its wrapped image cleared the 20% law by itself.
    Wrapped(Vec<u8>),
    /// Its unwrapped section, exactly as its writer closed it out.
    Plain(Vec<u8>),
}

/// D4409 (b): a kept band with what its extent and a later re-wrap or unwrap need.
#[allow(dead_code)] // V5SYNC r14: trunk's seal helper (loadstats D Sample / D4400 / D4409), not yet wired into the line's seal
struct KeptBand {
    form: BandForm,
    closeout: StreamCloseout,
    /// The band's granule payload ends (`wrap_section`'s block geometry).
    ends: Vec<u32>,
}

/// D4409 (b): appends one band's stored bytes to the image (8-aligned, room taken by `image_room`) and records its
/// extent and its section entry (`granules` = its [start, end) granules, `at` = the stream's (attno, path_ord)).
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)] // V5SYNC r14: trunk's seal helper (loadstats D Sample / D4400 / D4409), not yet wired into the line's seal
fn append_band(
    buf: &mut Vec<u8>,
    extents: &mut Vec<ExtentRecord>,
    sections: &mut Vec<SectionEntry>,
    bytes: &[u8],
    crc: u32,
    values: u64,
    granules: (u32, u32),
    at: (u32, u32),
) -> WriteResult<()> {
    image_room(buf, bytes.len() + 8)?;
    pad_to(buf, 8);
    let sec_start = buf.len();
    buf.extend_from_slice(bytes);
    extents.push(ExtentRecord {
        file_off: sec_start as u64,
        len: bytes.len() as u64,
        values,
        granule_start: granules.0,
        granule_count: granules.1 - granules.0,
        crc,
        flags: 0,
    });
    sections.push(SectionEntry {
        off: sec_start as u64,
        len: bytes.len() as u64,
        kind: SectionKind::Stream.as_u16(),
        flags: 0,
        attno: at.0,
        path_ord: at.1,
        crc,
    });
    Ok(())
}

/// D4409 (b): a band kept wrapped, in a stream that does not wrap, back to the unwrapped section its writer closed out.
/// `unwrap_section` rebuilds the writer's exact image (the reader's residency relies on it); the rebuild is checked
/// against the writer's CRC (`crc`), and a mismatch is a typed refusal, never a stored band.
#[allow(dead_code)] // V5SYNC r14: trunk's seal helper (loadstats D Sample / D4400 / D4409), not yet wired into the line's seal
pub(crate) fn unwrap_kept(image: &[u8], crc: u32) -> WriteResult<Vec<u8>> {
    let mut plain: Vec<u8> = Vec::new();
    pgrc_codec::wrapper::unwrap_section(image, &mut plain).map_err(WriteError::Format)?;
    if pgrc_format::wire::crc32c(&plain) != crc {
        return Err(WriteError::Refused { what: "a band kept wrapped did not unwrap to the section its writer closed out" });
    }
    Ok(plain)
}

/// D4409 (a): room for `extra` more bytes of the part's image (and its padding), grown as the memory policy says
/// (`mempolicy::load::image_grow`: what it adds plus an eighth, never Vec's doubling, which on a part that does not pack
/// took the seal past its charge); capacity only, the image's bytes the same. A failed reservation is a typed
/// out-of-memory refusal.
fn image_room(buf: &mut Vec<u8>, extra: usize) -> WriteResult<()> {
    if buf.capacity() - buf.len() < extra {
        let grow = mempolicy::load::image_grow(buf.len() as u64, extra as u64) as usize;
        buf.try_reserve_exact(grow).map_err(|_| WriteError::Format(pgrc_format::FormatError::OutOfMemory { bytes: grow }))?;
    }
    Ok(())
}

/// Append one raw-body section (Stats/PathTable/SortKey/StreamDir…): 8-align,
/// write body, record the SectionEntry with its crc.
fn push_raw_section(
    buf: &mut Vec<u8>,
    sections: &mut Vec<SectionEntry>,
    kind: SectionKind,
    attno: u32,
    path_ord: u32,
    body: &[u8],
) -> WriteResult<()> {
    image_room(buf, body.len() + 8)?;
    pad_to(buf, 8);
    let off = buf.len() as u64;
    buf.extend_from_slice(body);
    sections.push(SectionEntry {
        off,
        len: body.len() as u64,
        kind: kind.as_u16(),
        flags: 0,
        attno,
        path_ord,
        crc: pgrc_format::wire::crc32c(body),
    });
    Ok(())
}

/// StreamDir body: StreamEntry × n, then the extent tables each entry's
/// `extent_table_off` points into (section-relative offsets, spec §6.2).
fn encode_stream_dir(plans: &[StreamPlan]) -> Vec<u8> {
    let entries_len = plans.len() * pgrc_format::part::STREAM_ENTRY_LEN;
    let mut tables: Vec<u8> = Vec::new();
    let mut entry_offs: Vec<u64> = Vec::with_capacity(plans.len());
    for p in plans {
        entry_offs.push((entries_len + tables.len()) as u64);
        for e in &p.extents {
            e.encode_into(&mut tables);
        }
    }
    let mut out = Vec::with_capacity(entries_len + tables.len());
    for (i, p) in plans.iter().enumerate() {
        StreamEntry {
            extent_table_off: entry_offs[i],
            values: p.values,
            attno: p.attno,
            path_ord: p.path_ord,
            fixed_len: p.fixed_len,
            aux32: p.aux32,
            extent_count: p.extents.len() as u32,
            encoding: p.encoding,
            flags: p.flags,
            role: p.role.as_u8(),
            class: p.class_id,
            width: p.width,
            wrapper: p.wrapper,
            reserved: 0,
        }
        .encode_into(&mut out);
    }
    out.extend_from_slice(&tables);
    out
}

/// SB-10 grain election (OD-13 RULED "let's byte bound granules"): the
/// LARGEST ladder grain at which EVERY stream's exact whole-part value
/// bytes, averaged per granule (ceiling division — conservative), stay at
/// or under the provisional byte bound. Oversize varlenas already price at
/// their 16-B inline stub ([`ColBuffer::granule_pricing_bytes`] — their
/// payloads route to the overflow stream, not the granule). When even the
/// ladder floor misses the bound, the floor is taken: the bound is a
/// geometry target, never a refusal surface. Pure function of the exact
/// chunk stats — serial and parallel seals elect identically by
/// construction.
fn elect_part_grain(rows: u64, streams: &[(u32, u32, &ColBuffer, Option<i32>)]) -> GranuleGrain {
    for &g in &geom::GRAIN_LADDER {
        let grain = GranuleGrain::from_rows(g).expect("ladder grain");
        let gc = geom::granule_count_at(rows, grain).max(1);
        let fits = streams.iter().all(|&(_, _, col, _)| {
            col.granule_pricing_bytes().div_ceil(gc) <= geom::GRANULE_BYTE_BOUND_PROVISIONAL
        });
        if fits {
            return grain;
        }
    }
    let floor = *geom::GRAIN_LADDER.last().expect("nonempty ladder");
    GranuleGrain::from_rows(floor).expect("ladder grain")
}

/// Which encoding families need the per-granule gcount table on ROOT
/// sections at a NON-DEFAULT grain (SB-10): the multi-frame-per-granule
/// families address intra-extent values/frames through `value_base`/
/// `frame_base`, whose no-table closed forms are capacity-keyed at the
/// default 8192 grain; their gcount arm is grain-exact. The granule-framed
/// families (CONST/ALP/ALP_RD/BOOL_BITMAP) address by granule ordinal
/// and stay table-free (`granule_frame_base` refuses a table by contract).
fn root_gcounts_required(encoding: u16) -> bool {
    matches!(
        EncodingId::resolve(encoding),
        Ok(EncodingId::Verbatim
            | EncodingId::ByteFor
            | EncodingId::FforInterleave
            | EncodingId::DeltaFor
            | EncodingId::DictCodes
            | EncodingId::PackedNumeric)
    )
}

/// Run the codec's ArrayDual accept/refuse over EVERY granule of the part
/// (per-part permanence): `Some(splits)` iff every granule accepts; ONE
/// refusal demotes the whole column (`None`).
fn split_all_granules(
    ginputs: &[EncodeInput<'_>],
    facts: ArrayElemFacts,
) -> WriteResult<Option<Vec<ArraySplit>>> {
    let mut out = Vec::with_capacity(ginputs.len());
    for gi in ginputs {
        match elect_array_split(gi, facts).map_err(WriteError::Format)? {
            Some(s) => out.push(s),
            None => return Ok(None),
        }
    }
    Ok(Some(out))
}

/// Emit the validity stream for one (attno, path_ord) — present iff ≥1 NULL
/// in this part (spec §6.1). Shared by the ordinary and structural arms.
#[allow(clippy::too_many_arguments)]
fn emit_validity_stream(
    buf: &mut Vec<u8>,
    sections: &mut Vec<SectionEntry>,
    col: &ColBuffer,
    val_bytes: &[u8],
    attno: u32,
    path_ord: u32,
    rows: u64,
    grain: GranuleGrain,
    granule_count: u32,
) -> WriteResult<Option<ExtentRecord>> {
    if !col.has_null() {
        return Ok(None);
    }
    image_room(buf, val_bytes.len() + 8)?;
    pad_to(buf, 8);
    let vsec_start = buf.len();
    let mut w =
        StreamSectionWriter::begin(buf, EncodingId::BoolBitmap.as_u16(), 0, Wrapper::None)?;
    w.payload().extend_from_slice(val_bytes);
    for g in 0..granule_count {
        w.end_granule(geom::rows_in_granule_at(rows, grain, g));
    }
    let closeout = w.finish(false)?;
    sections.push(SectionEntry {
        off: vsec_start as u64,
        len: closeout.len,
        kind: SectionKind::Stream.as_u16(),
        flags: 0,
        attno,
        path_ord,
        crc: closeout.crc,
    });
    Ok(Some(ExtentRecord {
        file_off: vsec_start as u64,
        len: closeout.len,
        values: rows,
        granule_start: 0,
        granule_count,
        crc: closeout.crc,
        flags: 0,
    }))
}

/// sortpath (code-keyed zones): each granule's [min, max] code over its valid rows (`CODEZONE_EMPTY` for none), then
/// the part rollup over every granule.
fn code_zones(inputs: &[pgrc_format::abi::EncodeInput<'_>]) -> Vec<(u32, u32)> {
    let empty = pgrc_format::meta::CODEZONE_EMPTY;
    let mut out: Vec<(u32, u32)> = Vec::with_capacity(inputs.len() + 1);
    let mut part = empty;
    for input in inputs {
        let mut r = empty;
        for (row, &c) in input.datums.iter().enumerate().take(input.rows as usize) {
            if input.valid(row as u32) {
                let c = c as u32;
                r = (r.0.min(c), r.1.max(c));
            }
        }
        part = (part.0.min(r.0), part.1.max(r.1));
        out.push(r);
    }
    out.push(part);
    out
}

/// The two-witness null law + Stats section + builder aux sections for one
/// (attno, path_ord) (spec §6.6/§8.1). Shared by the ordinary and
/// structural arms — the builder observed the VALUE currency either way.
#[allow(clippy::too_many_arguments)]
fn emit_stats_sections(
    buf: &mut Vec<u8>,
    sections: &mut Vec<SectionEntry>,
    report: &mut SealReport,
    builder: &mut dyn ColumnMetaBuilder,
    col: &ColBuffer,
    popcounts: &[u32],
    attno: u32,
    path_ord: u32,
    rows: u64,
    grain: GranuleGrain,
    granule_count: u32,
    band_count: u32,
    dict_ndv: Option<u64>,
    plane: &mut Vec<PlaneSlice>,
    facts: &mut Vec<FactsSlice>,
) -> WriteResult<()> {
    let mut stats_body: Vec<u8> = Vec::new();
    let mut band_sums: ChargedVec<u64> = ChargedVec::new(col.owner(), Kind::Region);
    band_sums.with_vec(|v| v.try_reserve_exact(band_count as usize)).map_err(|_| WriteError::Format(pgrc_format::FormatError::OutOfMemory { bytes: band_count as usize * core::mem::size_of::<u64>() }))?; // alloc: Exec charged band_sums.owner()
    band_sums.with_vec(|v| v.resize(band_count as usize, 0)); // alloc: Exec charged band_sums.owner()
    let mut part_sum: u64 = 0;
    // Collected once, encoded twice: the §8.1 record stream AND (pgrc format 2.1
    // §2.1) its SoA transposition — one builder walk, two section bodies,
    // facts byte-identical by construction.
    let mut granule_recs: ChargedVec<StatsRecord> = ChargedVec::new(col.owner(), Kind::Region);
    granule_recs.with_vec(|v| v.try_reserve_exact(granule_count as usize)).map_err(|_| WriteError::Format(pgrc_format::FormatError::OutOfMemory { bytes: granule_count as usize * core::mem::size_of::<StatsRecord>() }))?; // alloc: Exec charged granule_recs.owner()
    for g in 0..granule_count {
        let rows_g = geom::rows_in_granule_at(rows, grain, g);
        let rec = builder.seal_granule(g);
        let bitmap_nonnull = bitmap_popcount(popcounts, g, rows_g, col.has_null());
        if rec.nonnull != bitmap_nonnull {
            return Err(WriteError::TwoWitnessSkew {
                attno,
                granule: g,
                stats_nonnull: rec.nonnull,
                bitmap_nonnull,
            });
        }
        report.nonnull_crosschecks += 1;
        band_sums[(g / GRANULES_PER_BAND) as usize] += bitmap_nonnull as u64;
        part_sum += bitmap_nonnull as u64;
        rec.encode_into(&mut stats_body);
        granule_recs.with_vec(|v| v.push(rec)); // alloc: Exec charged granule_recs.owner()
    }
    let mut band_recs: ChargedVec<StatsRecord> = ChargedVec::new(col.owner(), Kind::Region);
    band_recs.with_vec(|v| v.try_reserve_exact(band_count as usize)).map_err(|_| WriteError::Format(pgrc_format::FormatError::OutOfMemory { bytes: band_count as usize * core::mem::size_of::<StatsRecord>() }))?; // alloc: Exec charged band_recs.owner()
    for b in 0..band_count {
        let rec = builder.seal_band(b);
        if rec.nonnull as u64 != band_sums[b as usize] {
            return Err(WriteError::TwoWitnessSkew {
                attno,
                granule: b * GRANULES_PER_BAND, // band grain, first granule cited
                stats_nonnull: rec.nonnull,
                bitmap_nonnull: band_sums[b as usize] as u32,
            });
        }
        report.nonnull_crosschecks += 1;
        band_recs.with_vec(|v| v.push(rec)); // alloc: Exec charged band_recs.owner()
    }
    for rec in band_recs.iter() {
        rec.encode_into(&mut stats_body);
    }
    let part_rec = builder.seal_part();
    if part_rec.nonnull as u64 != part_sum {
        return Err(WriteError::TwoWitnessSkew {
            attno,
            granule: 0,
            stats_nonnull: part_rec.nonnull,
            bitmap_nonnull: part_sum as u32,
        });
    }
    report.nonnull_crosschecks += 1;
    part_rec.encode_into(&mut stats_body);

    // CMP-F (SB-6): the meta plane rides the wrapper machinery from the
    // start — Stats + every builder aux section (Psma/Bloom/NdvRegisters).
    push_meta_section(buf, sections, SectionKind::Stats, attno, path_ord, &stats_body)?;
    // pgrc format 2.1 §2.1: the FlatStats SoA transposition of the SAME records —
    // OPTIONAL (old readers skip typed), always RAW (mmap-cast law, never
    // zstd-wrapped), granule ordinals + one part rollup, ~44B/granule.
    // Kill switch PGRUST_PGRC_FLATSTATS=0 seals without it (the A/B
    // byte-attribution arm + the revert hatch); default ON — the section
    // is additive and §8.1 stays authoritative for band grain.
    if !matches!(
        std::env::var("PGRUST_PGRC_FLATSTATS").as_deref(),
        Ok("0") | Ok("off")
    ) {
        let flat = pgrc_format::meta::flatstats_encode(&granule_recs, &part_rec);
        push_optional_raw_section(buf, sections, SectionKind::FlatStats, attno, path_ord, &flat)?;
    }
    // pgrc format 2.1 §2.2: the per-(column, part) exact digest — exact NDV (the
    // dict entry count, free when this stream elected DICT_CODES) + the
    // part-grain zero tally under its computed witness. 24 B, OPTIONAL,
    // raw. Kill switch PGRUST_PGRC_PARTDIGEST=0.
    let mut plane_digest: Option<[u8; pgrc_format::bankstats::BANKSTATS_DIGEST_LEN]> = None;
    if !matches!(
        std::env::var("PGRUST_PGRC_PARTDIGEST").as_deref(),
        Ok("0") | Ok("off")
    ) {
        use pgrc_format::meta::{
            PartDigest, PARTDIGESTF_NDV_EXACT, PARTDIGESTF_ZERO_COMPUTED, STATSF_COMPUTED,
        };
        // Exact NDV source ladder: the dict entry count (the dictionary IS
        // the distinct set) → the meta builder's complete counted distinct
        // set (`distinct_exact` — O(1), None on long-value residue or
        // ndv-unsound profiles) → absent (readers fall back to ndv_est).
        let ndv_exact = dict_ndv.or_else(|| builder.distinct_exact());
        let mut flags = 0u16;
        if ndv_exact.is_some() {
            flags |= PARTDIGESTF_NDV_EXACT;
        }
        if part_rec.flags & STATSF_COMPUTED != 0 {
            flags |= PARTDIGESTF_ZERO_COMPUTED;
        }
        let digest = PartDigest {
            flags,
            ndv: ndv_exact.unwrap_or(0),
            zero_count: part_rec.zero_count,
        };
        let digest_bytes = digest.encode();
        push_optional_raw_section(
            buf,
            sections,
            SectionKind::PartDigest,
            attno,
            path_ord,
            &digest_bytes,
        )?;
        plane_digest = <[u8; pgrc_format::bankstats::BANKSTATS_DIGEST_LEN]>::try_from(
            digest_bytes.as_slice(),
        )
        .ok();
    }
    // OD-10/OD-11 populate kill switches (the M3.psma-ab populate half's
    // A/B apparatus): an OFF arm seals WITHOUT the aux section — the byte
    // attribution + seal-time delta the FT-9/FT-10 decline triggers price.
    // Default ON; production never sets these. SEAL-FUSION measurement-
    // honesty fix: the COMPUTE half (per-value bloom inserts, PSMA key
    // staging + block build) is skipped by the real builder under the same
    // switches (`pgrc_meta::builder::ColumnMeta::new`), so the OFF arm now
    // prices populate CPU honestly, not just section bytes; this section
    // skip stays as the byte half and as the belt for stand-in builders.
    let populate_off = seal_switch_off;
    let psma_off = populate_off("PGRUST_PGRC_PSMA_POPULATE");
    let bloom_off = populate_off("PGRUST_PGRC_BLOOM_POPULATE");
    let mut ndv_body: Option<Vec<u8>> = None;
    let mut bloom = false;
    let aux = builder.aux_sections();
    // CONVERT W3-2a (cvformat Q3, my side): the builder's finished aux bodies on the part's owner until each is
    // appended (declared after them: drops first)
    let mut aux_left: u64 = aux.iter().map(|(_, b)| b.capacity() as u64).sum();
    let mut aux_charge = HeldCharge::new(col.owner());
    aux_charge.sync(aux_left);
    for (kind, body) in aux {
        let held = body.capacity() as u64;
        if !((kind == SectionKind::Psma && psma_off) || (kind == SectionKind::Bloom && bloom_off)) {
            match kind {
                SectionKind::NdvRegisters => ndv_body = Some(body.clone()),
                SectionKind::Bloom => bloom = true,
                _ => {}
            }
            push_meta_section(buf, sections, kind, attno, path_ord, &body)?;
        }
        aux_left -= held;
        aux_charge.sync(aux_left);
    }
    if path_ord == 0 {
        facts.push(FactsSlice { attno, class: 0, signed: false, width: 0, ndv_body, bloom, dict: false, ncodes: 0, disk_bytes: 0 });
    }
    // [fmt-land] Stats-plane capture — the seal byproduct the bankstats
    // sidecar is assembled from at publish (top-level columns only; the
    // plane's column payloads carry exactly the path_ord-0 §8.1 bodies +
    // PartDigest records the offline builder would re-read from the part).
    if path_ord == 0 {
        plane.push(PlaneSlice {
            attno,
            stats_body,
            digest: plane_digest,
        });
    }
    Ok(())
}

/// Byte view over a u64-backed buffer — the sanctioned way to hand
/// [`ByteArena`] an 8-ALIGNED base (abi.rs §19.4 debug-asserts it; a
/// `Vec<u8>`'s base carries no alignment guarantee). Mirrors the
/// `pgrc_read` test-battery / `pgrc_qa::corpus` idiom.
fn words_as_bytes(words: &mut [u64]) -> &mut [u8] {
    // SAFETY: u64 → u8 reinterpret of an exclusively borrowed buffer;
    // alignment only loosens and the length is exact.
    unsafe {
        core::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, words.len() * 8)
    }
}

/// One emitted dual-substream's per-band facts.
struct DualBandSec {
    sec: ChargedVec<u8>,
    closeout: StreamCloseout,
}

/// Emit one word-currency dual substream (sizes or elements) per band in
/// the frozen §6.7 verbatim byval layout (dense LE words at `width`
/// stride), with the gcount table ALWAYS present — these are structural
/// children (spec §6.5: child streams carry their own value counts), which
/// also makes their addressing grain-proof by construction. Emitted by
/// hand rather than through [`VerbatimEncoder`] because a granule of
/// ELEMENTS may hold far more than 8192 values (ragged arrays) — the
/// reference encoder's capacity check is row-granule-shaped; the decode
/// side has no such cap (it is bounded by the per-granule value count).
fn emit_dual_word_sections(
    values_per_granule: impl Fn(u32) -> u32,
    word_at: impl Fn(u32, usize) -> u64,
    width: u8,
    granule_count: u32,
    band_count: u32,
    owner: Option<memledger::Owner>,
) -> WriteResult<Vec<DualBandSec>> {
    let mut out = Vec::with_capacity(band_count as usize);
    for b in 0..band_count {
        let g0 = b * GRANULES_PER_BAND;
        let g1 = (g0 + GRANULES_PER_BAND).min(granule_count);
        // CONVERT W3-2a (cvformat A2): each band's scratch section on the part's owner
        let mut sec: ChargedVec<u8> = ChargedVec::new(owner, Kind::Region);
        let mut w = StreamSectionWriter::begin_charged(
            &mut sec,
            EncodingId::Verbatim.as_u16(),
            width,
            Wrapper::None,
        )?;
        for g in g0..g1 {
            let n = values_per_granule(g);
            let buf = w.payload();
            for i in 0..n as usize {
                let d = word_at(g, i);
                buf.extend_from_slice(&d.to_le_bytes()[..width as usize]);
            }
            w.end_granule(n);
        }
        let closeout = w.finish(true)?;
        out.push(DualBandSec { sec, closeout });
    }
    Ok(out)
}

/// Append emitted dual-substream sections to the part image; returns the
/// extent records.
fn append_dual_sections(
    buf: &mut Vec<u8>,
    sections: &mut Vec<SectionEntry>,
    band_secs: &[DualBandSec],
    attno: u32,
    granule_count: u32,
) -> WriteResult<Vec<ExtentRecord>> {
    let mut extents = Vec::with_capacity(band_secs.len());
    for (bi, bs) in band_secs.iter().enumerate() {
        let b = bi as u32;
        let g0 = b * GRANULES_PER_BAND;
        let g1 = (g0 + GRANULES_PER_BAND).min(granule_count);
        image_room(buf, bs.sec.len() + 8)?;
        pad_to(buf, 8);
        let start = buf.len();
        buf.extend_from_slice(&bs.sec);
        extents.push(ExtentRecord {
            file_off: start as u64,
            len: bs.sec.len() as u64,
            values: bs.closeout.values,
            granule_start: g0,
            granule_count: g1 - g0,
            crc: bs.closeout.crc,
            flags: 0,
        });
        sections.push(SectionEntry {
            off: start as u64,
            len: bs.sec.len() as u64,
            kind: SectionKind::Stream.as_u16(),
            flags: 0,
            attno,
            path_ord: 0,
            crc: bs.closeout.crc,
        });
    }
    Ok(extents)
}

/// Seal one accepted array column as the TY-1 ArrayDual structural election
/// (ENC 10): a row-aligned `Sizes` substream (u32 counts; null rows 0 — the
/// split's placeholder law) plus a dense `ChildValues` substream (elements
/// as zero-extended words at the catalog element width), each in the frozen
/// VERBATIM byval layout with its own gcount table; the parent
/// `StreamEntry` records `encoding = ArrayDual` with ZERO extents (the
/// structural marker — spec §4: no §6.4 section header ever carries id 10;
/// `width`/`aux32` echo the catalog facts: element width / elemtype oid).
///
/// Verification is two-legged and mandatory: (1) each substream round-trips
/// per granule through the SAME vtable a reader resolves; (2) the split
/// COMPOSES back — `assemble_array_datums` over the split arrays must be
/// byte-identical to every stored array image (the honesty leg: a wrong
/// acceptance cannot reach a file).
#[allow(clippy::too_many_arguments)]
fn seal_array_dual(
    buf: &mut Vec<u8>,
    sections: &mut Vec<SectionEntry>,
    plans: &mut Vec<StreamPlan>,
    report: &mut SealReport,
    builder: &mut dyn ColumnMetaBuilder,
    resolver: &dyn VerifyResolver,
    col: &ColBuffer,
    ginputs: &[EncodeInput<'_>],
    splits: &[ArraySplit],
    facts: ArrayElemFacts,
    attno: u32,
    rows: u64,
    grain: GranuleGrain,
    granule_count: u32,
    band_count: u32,
    baseline: u64,
    plane: &mut Vec<PlaneSlice>,
    facts_slices: &mut Vec<FactsSlice>,
) -> WriteResult<()> {
    let sizes_class = StorageClass::ByvalWord {
        width: 4,
        signed: false,
    };
    let elem_class = StorageClass::ByvalWord {
        width: facts.elem_len,
        signed: false,
    };
    let sizes_key = pgrc_format::enc::stream_kernel_key(
        EncodingId::Verbatim.as_u16(),
        CLASS_BYVAL,
        4,
    )
    .map_err(WriteError::Format)?;
    let elems_key = pgrc_format::enc::stream_kernel_key(
        EncodingId::Verbatim.as_u16(),
        CLASS_BYVAL,
        facts.elem_len,
    )
    .map_err(WriteError::Format)?;
    let sizes_vt = resolver.resolve(sizes_key)?;
    let elems_vt = resolver.resolve(elems_key)?;

    // ---- emit both substreams (scratch sections, per band) -----------------
    let sizes_secs = emit_dual_word_sections(
        |g| splits[g as usize].sizes.len() as u32,
        |g, i| splits[g as usize].sizes[i],
        4,
        granule_count,
        band_count,
        col.owner(),
    )?;
    let elems_secs = emit_dual_word_sections(
        |g| splits[g as usize].elems.len() as u32,
        |g, i| splits[g as usize].elems[i],
        facts.elem_len,
        granule_count,
        band_count,
        col.owner(),
    )?;

    // ---- mandatory verify: substream round-trips + composition -------------
    let mut datum_scratch: ChargedVec<u64> = ChargedVec::new(col.owner(), Kind::Region);
    // ByteArena backings must be 8-aligned (the abi.rs §19.4 base-alignment
    // law, debug-asserted) — u64-backed buffers, byte-viewed (the
    // pgrc_read test-battery / qa corpus idiom). A `Vec<u8>` base carries
    // no alignment guarantee.
    let mut verify_arena_words: ChargedVec<u64> = ChargedVec::new(col.owner(), Kind::Region);
    verify_arena_words.with_vec(|v| v.resize(8, 0)); // alloc: Exec charged verify_arena_words.owner()
    let arena_scratch: &mut [u8] = words_as_bytes(&mut verify_arena_words);
    let mut compose_out: ChargedVec<u64> = ChargedVec::new(col.owner(), Kind::Region);
    for b in 0..band_count {
        let g0 = b * GRANULES_PER_BAND;
        let g1 = (g0 + GRANULES_PER_BAND).min(granule_count);
        for g in g0..g1 {
            let s = &splits[g as usize];
            let legs: [(&DualBandSec, KernelKey, &CodecVtable, &[u64], StorageClass); 2] = [
                (&sizes_secs[b as usize], sizes_key, sizes_vt, &s.sizes, sizes_class),
                (&elems_secs[b as usize], elems_key, elems_vt, &s.elems, elem_class),
            ];
            for (bs, key, vt, datums, class) in legs {
                let n = datums.len() as u32;
                if datum_scratch.len() < datums.len() {
                    datum_scratch.with_vec(|v| v.try_reserve_exact(datums.len() - v.len())).map_err(|_| WriteError::Format(pgrc_format::FormatError::OutOfMemory { bytes: datums.len() * core::mem::size_of::<u64>() }))?; // alloc: Exec charged datum_scratch.owner()
                    datum_scratch.with_vec(|v| v.resize(datums.len(), 0)); // alloc: Exec charged datum_scratch.owner()
                }
                let ctx = KernelCtx {
                    key,
                    flags: 0,
                    fixed_len: 0,
                    bytes: &bs.sec,
                    frame_table: None,
                    granule: g,
                    granule_in_extent: g - g0,
                    rows: n,
                    values: n,
                    validity_bytes: None,
                    overflow: None,
                    dict: None,
                };
                let input = EncodeInput {
                    class,
                    rows: n,
                    datums,
                    validity: None,
                };
                verify_roundtrip(vt, &ctx, &input, &mut datum_scratch, &mut *arena_scratch)
                    .map_err(|cause| WriteError::RoundTrip {
                        attno,
                        path_ord: 0,
                        granule: g,
                        cause,
                    })?;
                report.granules_verified += 1;
            }
            // Composition leg: split arrays must rebuild every stored image
            // byte-identically (empty arrays included). u64-backed arena
            // (the 8-aligned-base law again).
            let input = &ginputs[g as usize];
            let need = assembled_arena_bytes(facts, &s.sizes) + 64;
            let mut compose_arena_words: ChargedVec<u64> = ChargedVec::new(col.owner(), Kind::Region);
            compose_arena_words.with_vec(|v| v.try_reserve_exact(need.div_ceil(8))).map_err(|_| WriteError::Format(pgrc_format::FormatError::OutOfMemory { bytes: need.div_ceil(8) * core::mem::size_of::<u64>() }))?; // alloc: Exec charged compose_arena_words.owner()
            compose_arena_words.with_vec(|v| v.resize(need.div_ceil(8), 0)); // alloc: Exec charged compose_arena_words.owner()
            let mut arena = ByteArena::new(words_as_bytes(&mut compose_arena_words));
            if compose_out.len() < s.sizes.len() {
                compose_out.with_vec(|v| v.try_reserve_exact(s.sizes.len() - v.len())).map_err(|_| WriteError::Format(pgrc_format::FormatError::OutOfMemory { bytes: s.sizes.len() * core::mem::size_of::<u64>() }))?; // alloc: Exec charged compose_out.owner()
                compose_out.with_vec(|v| v.resize(s.sizes.len(), 0)); // alloc: Exec charged compose_out.owner()
            }
            assemble_array_datums(
                facts,
                &s.sizes,
                &s.elems,
                |r| input.valid(r),
                &mut compose_out.vec_mut(),
                &mut arena,
            )
            .map_err(|cause| WriteError::RoundTrip {
                attno,
                path_ord: 0,
                granule: g,
                cause,
            })?;
            for r in 0..input.rows {
                if !input.valid(r) {
                    continue;
                }
                let mut s_in = [0u8; 8];
                let mut s_out = [0u8; 8];
                // SAFETY: input datums obey the EncodeInput pointer-class
                // contract (writer-built); compose_out datums point into
                // the live `arena_bytes`.
                let a = unsafe {
                    pgrc_format::abi::datum_canonical_bytes(
                        StorageClass::VarlenaVerbatim,
                        input.datums[r as usize],
                        &mut s_in,
                    )
                    .map_err(WriteError::Format)?
                };
                let bcanon = unsafe {
                    pgrc_format::abi::datum_canonical_bytes(
                        StorageClass::VarlenaVerbatim,
                        compose_out[r as usize],
                        &mut s_out,
                    )
                    .map_err(WriteError::Format)?
                };
                if a != bcanon {
                    return Err(WriteError::Contract {
                        detail: "ArrayDual composition is not byte-identical",
                    });
                }
            }
            // Meta builders observe the VALUE currency (the arrays).
            builder.observe_granule(input, g);
        }
    }

    // ---- append substream extents ------------------------------------------
    let sizes_extents = append_dual_sections(buf, sections, &sizes_secs, attno, granule_count)?;
    let elems_extents = append_dual_sections(buf, sections, &elems_secs, attno, granule_count)?;
    let chosen_len: u64 = sizes_extents.iter().map(|e| e.len).sum::<u64>()
        + elems_extents.iter().map(|e| e.len).sum::<u64>();
    let elems_total: u64 = elems_extents.iter().map(|e| e.values).sum();

    // ---- validity + stats (shared emission) --------------------------------
    let (val_bytes, val_pops) = build_validity_bytes(col, granule_count, grain);
    let validity_extent = emit_validity_stream(
        buf,
        sections,
        col,
        &val_bytes,
        attno,
        0,
        rows,
        grain,
        granule_count,
    )?;
    emit_stats_sections(
        buf,
        sections,
        report,
        builder,
        col,
        &val_pops,
        attno,
        0,
        rows,
        grain,
        granule_count,
        band_count,
        None,
        plane,
        facts_slices,
    )?;

    // ---- directory rows -----------------------------------------------------
    // Parent: the structural marker — encoding ArrayDual, ZERO extents;
    // `width` echoes the element width and `aux32` the elemtype oid (the
    // caller-declared catalog facts, recorded so a reader rebuilds
    // `ArrayElemFacts` from the entry alone).
    plans.push(StreamPlan {
        attno,
        path_ord: 0,
        role: StreamRole::Values,
        encoding: EncodingId::ArrayDual.as_u16(),
        flags: 0,
        class_id: col.schema.class.id(),
        width: facts.elem_len,
        fixed_len: 0,
        aux32: facts.elemtype,
        wrapper: Wrapper::None.as_u8(),
        values: rows,
        extents: Vec::new(),
    });
    plans.push(StreamPlan {
        attno,
        path_ord: 0,
        role: StreamRole::Sizes,
        encoding: EncodingId::Verbatim.as_u16(),
        flags: 0,
        class_id: CLASS_BYVAL,
        width: 4,
        fixed_len: 0,
        aux32: 0,
        wrapper: Wrapper::None.as_u8(),
        values: rows,
        extents: sizes_extents,
    });
    plans.push(StreamPlan {
        attno,
        path_ord: 0,
        role: StreamRole::ChildValues,
        encoding: EncodingId::Verbatim.as_u16(),
        flags: 0,
        class_id: CLASS_BYVAL,
        width: facts.elem_len,
        fixed_len: 0,
        aux32: 0,
        wrapper: Wrapper::None.as_u8(),
        values: elems_total,
        extents: elems_extents,
    });
    if let Some(ve) = validity_extent {
        plans.push(StreamPlan {
            attno,
            path_ord: 0,
            role: StreamRole::Validity,
            encoding: EncodingId::BoolBitmap.as_u16(),
            flags: 0,
            class_id: col.schema.class.id(),
            width: 0,
            fixed_len: 0,
            aux32: 0,
            wrapper: 0,
            values: rows,
            extents: vec![ve],
        });
    }

    // The structural witness: encoding ArrayDual, priced honestly (exact
    // emitted substream bytes vs the verbatim baseline).
    report.elections.push(ElectionWitness {
        attno,
        path_ord: 0,
        encoding: EncodingId::ArrayDual.as_u16(),
        baseline_len: baseline,
        chosen_len,
    });
    report
        .col_hashes
        .push((attno, 0, col.logical_hash()?.digest()));
    Ok(())
}

/// The stream's full validity bitmap in the §6.6 layout (per-granule
/// contiguous, byte-padded per granule) PLUS the per-granule popcounts of
/// the just-emitted bytes. Empty when the column is all-valid.
///
/// SEAL-FUSION (walks #15/#17): the second-witness popcount is accumulated
/// per granule right after that granule's bytes are ENCODED — cache-warm,
/// one pass instead of a build pass plus a later popcount pass. The
/// two-witness law is intact: the popcount still reads the EMITTED bytes
/// (this very buffer is what `emit_validity_stream` writes to the file),
/// never the input bitset.
fn build_validity_bytes(
    col: &ColBuffer,
    granule_count: u32,
    grain: GranuleGrain,
) -> (Vec<u8>, Vec<u32>) {
    if !col.has_null() {
        return (Vec::new(), Vec::new());
    }
    let mut out = Vec::new();
    let mut pops = Vec::with_capacity(granule_count as usize);
    for g in 0..granule_count {
        let rows_g = geom::rows_in_granule_at(col.rows(), grain, g);
        let at = out.len();
        encode_validity_bitmap(col.granule_validity(g, grain), rows_g, &mut out);
        pops.push(out[at..].iter().map(|b| b.count_ones()).sum());
    }
    (out, pops)
}

/// Granule `g`'s slice of the emitted validity bytes (full granules before
/// `g` each occupy exactly `grain / 8` bytes — every ladder grain is a
/// multiple of 8).
fn validity_slice<'a>(
    col: &ColBuffer,
    val_bytes: &'a [u8],
    g: u32,
    rows_g: u32,
    grain: GranuleGrain,
) -> Option<&'a [u8]> {
    if !col.has_null() {
        return None;
    }
    let start = g as usize * (grain.rows() / 8) as usize;
    let len = (rows_g as usize).div_ceil(8);
    Some(&val_bytes[start..start + len])
}

/// The second witness: popcount of the EMITTED bitmap bytes (all-valid
/// streams count rows — the absent-bitmap arm of spec §6.1). SEAL-FUSION:
/// the counts were accumulated while the bitmap bytes were built
/// ([`build_validity_bytes`]) — same emitted bytes, one pass.
fn bitmap_popcount(popcounts: &[u32], g: u32, rows_g: u32, has_null: bool) -> u32 {
    if has_null {
        popcounts[g as usize]
    } else {
        rows_g
    }
}

/// SB-7: cut the UNWRAPPED DictPayload section's extent table at dict-frame
/// boundaries. The section image is untouched (the frame table was already
/// written every `DICT_FRAME_ENTRIES` entries — ONLY extent geometry
/// changes, the ledger row's own scoping): extent i covers frame i's byte
/// run of the STORED section (extent 0 includes the 32-B header; the last
/// extent includes the frame-table tail), `values` = entries in the frame,
/// `granule_start` = frame ordinal, per-extent CRC over exactly the
/// addressed bytes — the fault-and-validate grain `ensure_frame` needs.
fn dict_frame_extents(
    image: &[u8],
    sec_start: u64,
    entry_count: u32,
    owner: Option<memledger::Owner>,
) -> WriteResult<Vec<ExtentRecord>> {
    let hdr = StreamSectionHdr::decode(image).map_err(WriteError::Format)?;
    let ft = hdr
        .frame_table(image)
        .map_err(WriteError::Format)?
        .ok_or(WriteError::Contract {
            detail: "dict payload section without a frame table",
        })?;
    if ft.is_empty() {
        return Err(WriteError::Contract {
            detail: "dict payload frame table empty",
        });
    }
    // Frame starts are payload-relative; absolute (section-relative) start
    // of frame i is header + off. Extent 0 starts at 0 (header rides with
    // frame 0); the last extent runs to the section end (frame table tail).
    let nf = ft.len();
    let mut bounds: ChargedVec<usize> = ChargedVec::new(owner, Kind::Region);
    bounds.with_vec(|v| v.try_reserve_exact(nf + 1)).map_err(|_| WriteError::Format(pgrc_format::FormatError::OutOfMemory { bytes: (nf + 1) * core::mem::size_of::<usize>() }))?; // alloc: Exec charged bounds.owner()
    bounds.with_vec(|v| v.push(0)); // alloc: Exec charged bounds.owner()
    for &off in ft.iter().skip(1) {
        bounds.with_vec(|v| v.push(pgrc_format::part::STREAM_SECTION_HDR_LEN + off as usize)); // alloc: Exec charged bounds.owner()
    }
    bounds.with_vec(|v| v.push(image.len())); // alloc: Exec charged bounds.owner()
    let mut out = Vec::with_capacity(nf);
    for f in 0..nf {
        let (lo, hi) = (bounds[f], bounds[f + 1]);
        if hi <= lo || hi > image.len() {
            return Err(WriteError::Contract {
                detail: "dict frame extent bounds",
            });
        }
        let entries_in_frame = if f + 1 < nf {
            pgrc_format::geom::DICT_FRAME_ENTRIES as u64
        } else {
            entry_count as u64 - (nf as u64 - 1) * pgrc_format::geom::DICT_FRAME_ENTRIES as u64
        };
        out.push(ExtentRecord {
            file_off: sec_start + lo as u64,
            len: (hi - lo) as u64,
            values: entries_in_frame,
            granule_start: f as u32,
            granule_count: 1,
            crc: pgrc_format::wire::crc32c(&image[lo..hi]),
            flags: 0,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Cluster-key sorted ingest (IN-1 / FT-6 / OD-8)
// ---------------------------------------------------------------------------

/// The cluster sort's comparator, for a caller that orders rows the way the seal does (the merged scan reads
/// sorted parts in key order through it; K1's sorted merge feeds a presorted job, so the seal's sort takes its
/// verify-only path and cannot disagree with the caller's order).
pub mod cmp {
    pub use super::{decl_slots, key_cmp, key_slot_at, key_slots, path_values, slot_cmp, KeySlot};
}

/// One prepared key cell (comparator currency — extracted with typed error
/// propagation BEFORE sorting, so the comparator itself is infallible).
#[derive(Debug, Clone, Copy)]
pub enum KeySlot<'a> {
    Null,
    I(i64),
    U(u64),
    F(f64),
    B(&'a [u8]),
}

/// PG float ordering: NaN == NaN, NaN greater than everything else
/// (float8_cmp), -0 == +0.
fn pg_float_cmp(a: f64, b: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Equal,
        (true, false) => Greater,
        (false, true) => Less,
        (false, false) => a.partial_cmp(&b).expect("no NaN"),
    }
}

/// Is `schema` cluster-sortable from its STORED form? The declared key must
/// sort in the order the stored canonical form exposes: byval word classes
/// (sign per the class), floats (PG NaN law), memcmp-ordered byref, and
/// C-collated text (strcmp IS the collation). Everything else — non-C
/// collations, packed/unpacked numeric via varlena images, opaque
/// semantics — refuses typed: a wrong order silently poisons every
/// consumer that gates on the clustered witness.
pub fn cluster_sortable(schema: &pgrc_format::class::ColSchema) -> bool {
    use pgrc_format::class::{CollationClass, TypeSemantics};
    match schema.class {
        StorageClass::ByvalWord { .. }
        | StorageClass::Bool
        | StorageClass::F32
        | StorageClass::F64 => true,
        StorageClass::Fixed { .. } | StorageClass::VarlenaVerbatim => {
            match schema.semantics {
                TypeSemantics::MemcmpOrdered => true,
                TypeSemantics::TextCollated => {
                    schema.collation_class == CollationClass::C
                }
                _ => false,
            }
        }
    }
}

/// Extract one key column's comparator cells.
pub fn key_slots<'a>(col: &'a ColBuffer) -> WriteResult<Vec<KeySlot<'a>>> {
    let rows = col.rows();
    let mut out = Vec::with_capacity(rows as usize);
    for r in 0..rows {
        out.push(key_slot_at(col, r)?);
    }
    Ok(out)
}

/// One row's value of a jsonb text path (PostgreSQL's `->` / `->>` chain
/// with object keys): None = SQL NULL (a step meets no object or no key, or
/// the final value is JSON null). `doc` is the stored container.
pub fn jsonb_path_text(doc: &[u8], path: &[Vec<u8>]) -> WriteResult<Option<Vec<u8>>> {
    use adt_jsonb::container::{container_is_object, get_key_value, JsonbItem};
    let mut c = doc;
    for (i, key) in path.iter().enumerate() {
        if !container_is_object(c) {
            return Ok(None);
        }
        let Some(v) = get_key_value(c, key) else { return Ok(None) };
        if i + 1 < path.len() {
            match v {
                JsonbItem::Binary(sub) => c = sub,
                _ => return Ok(None),
            }
            continue;
        }
        return Ok(match v {
            JsonbItem::Null => None,
            JsonbItem::String(s) => Some(pgrc_format::try_to_vec(s, "seal")?),
            other => {
                // numbers, booleans and containers: JsonbValueAsText
                let ctx = mcx::MemoryContext::new("pgrc sort path");
                let text = adt_jsonb::getfield::value_as_text(ctx.mcx(), &other)
                    .map_err(|_| WriteError::Contract { detail: "jsonb sort path: value_as_text failed" })?;
                match text {
                    Some(t) => Some(pgrc_format::try_to_vec(t.data(), "seal")?),
                    None => None,
                }
            }
        });
    }
    Ok(None)
}

/// A path key's per-row values (owned; the slots borrow them).
pub fn path_values(col: &ColBuffer, path: &[Vec<u8>]) -> WriteResult<Vec<Option<Vec<u8>>>> {
    (0..col.rows())
        .map(|r| match col.varlena_payload(r)? {
            Some(doc) => jsonb_path_text(doc, path),
            None => Ok(None),
        })
        .collect()
}

/// One key's comparator cells: the column's stored form, or a path key's
/// values out of `hold` (filled here).
pub fn decl_slots<'a>(
    col: &'a ColBuffer,
    d: &ClusterKeyDecl,
    hold: &'a Option<Vec<Option<Vec<u8>>>>,
) -> WriteResult<Vec<KeySlot<'a>>> {
    match hold {
        Some(vals) if !d.path.is_empty() => {
            Ok(vals.iter().map(|v| v.as_deref().map_or(KeySlot::Null, KeySlot::B)).collect())
        }
        _ => key_slots(col),
    }
}

/// Row `r`'s comparator cell of one key column (SORTRUNS-E0 L2: the run merge compares rows of different inputs'
/// columns, one cell at a time).
#[inline]
pub fn key_slot_at(col: &ColBuffer, r: u64) -> WriteResult<KeySlot<'_>> {
    if !col.valid_at(r) {
        return Ok(KeySlot::Null);
    }
    Ok(match col.schema.class {
        StorageClass::ByvalWord { signed: true, .. } => KeySlot::I(col.word_at(r) as i64),
        StorageClass::ByvalWord { signed: false, .. } | StorageClass::Bool => KeySlot::U(col.word_at(r)),
        StorageClass::F32 => KeySlot::F(f32::from_bits(col.word_at(r) as u32) as f64),
        StorageClass::F64 => KeySlot::F(f64::from_bits(col.word_at(r))),
        StorageClass::Fixed { .. } => KeySlot::B(col.fixed_at(r)?.expect("valid row")),
        StorageClass::VarlenaVerbatim => KeySlot::B(col.varlena_payload(r)?.expect("valid row")),
    })
}

/// Compare rows `a` and `b` under one key declaration. NULLS FIRST/LAST is
/// absolute placement (PG semantics: independent of ASC/DESC); direction
/// inverts value comparisons only.
#[inline]
pub fn key_cmp(
    slots: &[KeySlot<'_>],
    decl: &ClusterKeyDecl,
    a: usize,
    b: usize,
) -> std::cmp::Ordering {
    slot_cmp(&slots[a], &slots[b], decl)
}

/// Two cells of one key column under its declaration ([`key_cmp`]'s law), wherever each came from.
#[inline]
pub fn slot_cmp(a: &KeySlot<'_>, b: &KeySlot<'_>, decl: &ClusterKeyDecl) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let ord = match (a, b) {
        (KeySlot::Null, KeySlot::Null) => return Equal,
        (KeySlot::Null, _) => {
            return if decl.nulls == NullsOrder::First { Less } else { Greater }
        }
        (_, KeySlot::Null) => {
            return if decl.nulls == NullsOrder::First { Greater } else { Less }
        }
        (KeySlot::I(x), KeySlot::I(y)) => x.cmp(y),
        (KeySlot::U(x), KeySlot::U(y)) => x.cmp(y),
        (KeySlot::F(x), KeySlot::F(y)) => pg_float_cmp(*x, *y),
        (KeySlot::B(x), KeySlot::B(y)) => x.cmp(y),
        _ => unreachable!("one column, one slot shape"),
    };
    if decl.dir == SortDir::Desc {
        ord.reverse()
    } else {
        ord
    }
}

/// Sort the part by the declared cluster key (IN-1): validate the key,
/// extract comparator cells, VERIFY-ONLY when the input is already
/// ordered (`Ok(None)`), else apply the STABLE permutation to every root
/// and lane buffer and re-verify the produced order (the attestation's
/// second leg — a broken sort cannot reach a file). IXBULKSORT: the
/// permutation comes back too (`perm[s]` = the arrival position of sorted
/// row `s`), for the rows' IDs ([`SealReport::cluster_perm`]).
/// Resolve and validate the declared cluster key's columns among `cols` (roots only: a lane is a derived stream,
/// never a declared key), with their comparison slots, in declaration order.
/// (sortpath: a path key also carries its per-row values, which [`key_slot_cols`] borrows.)
#[allow(clippy::type_complexity)]
fn cluster_key_cols<'c>(
    cols: &[&'c ColBuffer],
    decls: &'c [ClusterKeyDecl],
) -> WriteResult<Vec<(&'c ColBuffer, &'c ClusterKeyDecl, Option<Vec<Option<Vec<u8>>>>)>> {
    let mut keys: Vec<(&ColBuffer, &ClusterKeyDecl)> = Vec::with_capacity(decls.len());
    for d in decls {
        let col = *cols
            .iter()
            .find(|c| c.schema.attno == d.attno)
            .ok_or(WriteError::Contract {
                detail: "declared cluster-key attno is not a table column",
            })?;
        if d.path.is_empty() && !cluster_sortable(&col.schema) {
            return Err(WriteError::Refused {
                what: "cluster key over a column whose stored form is not its sort order",
            });
        }
        if !d.path.is_empty() && col.schema.class != StorageClass::VarlenaVerbatim {
            return Err(WriteError::Refused {
                what: "cluster path key over a column that is not a stored jsonb document",
            });
        }
        keys.push((col, d));
    }
    keys.into_iter()
        .map(|(col, d)| Ok((col, d, (!d.path.is_empty()).then(|| path_values(col, &d.path)).transpose()?)))
        .collect()
}

/// The comparison slots of [`cluster_key_cols`]' keys, in declaration order.
#[allow(clippy::type_complexity)]
fn key_slot_cols<'a, 'c: 'a>(
    keys: &'a [(&'c ColBuffer, &'c ClusterKeyDecl, Option<Vec<Option<Vec<u8>>>>)],
) -> WriteResult<Vec<(Vec<KeySlot<'a>>, &'a ClusterKeyDecl)>> {
    keys.iter().map(|(col, d, hold)| Ok((decl_slots(col, d, hold)?, *d))).collect()
}

/// Rows `r - 1` and `r` of the key slots, in key order.
fn key_rows_cmp(slot_cols: &[(Vec<KeySlot<'_>>, &ClusterKeyDecl)], a: usize, b: usize) -> std::cmp::Ordering {
    for (slots, d) in slot_cols {
        let ord = key_cmp(slots, d, a, b);
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

/// K1-MEM: the rows of `cols` (which hold the declared key's columns) are already in key order — [`cluster_sort`]'s
/// fast path, whose verification IS the attestation; a streamed seal never sorts.
fn cluster_ordered(cols: &[&ColBuffer], decls: &[ClusterKeyDecl]) -> WriteResult<bool> {
    let rows = cols.first().map(|c| c.rows()).unwrap_or(0);
    let keys = cluster_key_cols(cols, decls)?;
    let slot_cols = key_slot_cols(&keys)?;
    Ok((1..rows as usize).all(|r| key_rows_cmp(&slot_cols, r - 1, r) != std::cmp::Ordering::Greater))
}

fn cluster_sort(
    roots: &[ColBuffer],
    lanes: &[ShredLane],
    decls: &[ClusterKeyDecl],
) -> WriteResult<Option<(Vec<ColBuffer>, Vec<ShredLane>, Vec<u32>)>> {
    let rows = roots.first().map(|c| c.rows()).unwrap_or(0);
    let refs: Vec<&ColBuffer> = roots.iter().collect();
    let keys = cluster_key_cols(&refs, decls)?;
    let slot_cols = key_slot_cols(&keys)?;
    // CONVERT W3-2a: the sort's key slots and permutation on the part's owner while it sorts (declared after them:
    // drops first)
    let owner = roots.first().and_then(ColBuffer::owner);
    let slots_held: u64 = slot_cols.iter().map(|(v, _)| cap_bytes(v.capacity(), v)).sum();
    let mut sort_charge = HeldCharge::new(owner);
    sort_charge.sync(slots_held);
    let cmp_rows = |a: usize, b: usize| key_rows_cmp(&slot_cols, a, b);
    // Fast path: already ordered ⇒ the verification IS the attestation.
    let already = (1..rows as usize).all(|r| cmp_rows(r - 1, r) != std::cmp::Ordering::Greater);
    if already {
        return Ok(None);
    }
    // STABLE sort (equal keys keep input order — the deterministic
    // tiebreak; sealed bytes become a pure function of multiset + key).
    let mut perm: Vec<u32> = (0..rows as u32).collect(); // alloc: Exec charged sort_charge
    sort_charge.sync(slots_held + perm.capacity() as u64 * 4);
    perm.sort_by(|&a, &b| cmp_rows(a as usize, b as usize));
    let sorted_roots: Vec<ColBuffer> = roots
        .iter()
        .map(|c| c.permuted(&perm))
        .collect::<WriteResult<_>>()?;
    let sorted_lanes: Vec<ShredLane> = lanes
        .iter()
        .map(|l| {
            Ok(ShredLane {
                scale: l.scale,
                object_roots: l.object_roots,
                parent_attno: l.parent_attno,
                path: l.path.clone(),
                col: l.col.permuted(&perm)?,
            })
        })
        .collect::<WriteResult<_>>()?;
    // Post-sort verification (belt): the produced order must hold on the
    // PERMUTED buffers — the attestation never rides the sort's own
    // correctness alone.
    {
        let mut vcols: ChargedVec<&ColBuffer> = ChargedVec::new(roots.first().and_then(ColBuffer::owner), Kind::Region);
        vcols.with_vec(|v| v.extend(decls.iter().map(|d| sorted_roots.iter().find(|c| c.schema.attno == d.attno).expect("validated above")))); // alloc: Exec charged vcols.owner()
        let vholds: Vec<Option<Vec<Option<Vec<u8>>>>> = vcols
            .iter()
            .zip(decls)
            .map(|(col, d)| Ok((!d.path.is_empty()).then(|| path_values(col, &d.path)).transpose()?))
            .collect::<WriteResult<_>>()?;
        let mut vkeys: Vec<(Vec<KeySlot<'_>>, &ClusterKeyDecl)> = Vec::new();
        for ((col, d), hold) in vcols.iter().zip(decls).zip(&vholds) {
            vkeys.push((decl_slots(col, d, hold)?, d)); // alloc: Exec charged vcharge
        }
        // CONVERT W3-2a: the verification's slots on the part's owner (declared after them: drops first)
        let mut vcharge = HeldCharge::new(owner);
        vcharge.sync(vkeys.iter().map(|(v, _)| cap_bytes(v.capacity(), v)).sum());
        let vcmp = |a: usize, b: usize| {
            for (slots, d) in &vkeys {
                let ord = key_cmp(slots, d, a, b);
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        };
        for r in 1..rows as usize {
            if vcmp(r - 1, r) == std::cmp::Ordering::Greater {
                return Err(WriteError::Contract {
                    detail: "cluster sort postcondition violated",
                });
            }
        }
    }
    drop(sort_charge);
    Ok(Some((sorted_roots, sorted_lanes, perm)))
}

/// Count varlena entries in an overflow payload region (sequential
/// 8-aligned varlena images — spec §6.8).
fn count_overflow_entries(ovf: &[u8]) -> WriteResult<u64> {
    let mut off = 0usize;
    let mut n = 0u64;
    while off < ovf.len() {
        off = off.div_ceil(8) * 8;
        if off >= ovf.len() {
            break;
        }
        let (image, _) = varlena_entry_at(ovf, off, "overflow census")?;
        off += image.len();
        n += 1;
    }
    Ok(n)
}
