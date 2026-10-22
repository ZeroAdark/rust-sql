//! QA-side bridges between the merged wave-2 crates. The product wiring of
//! these seams belongs to M3-G/H (unmerged, live lanes); this crate builds
//! its own fixture-grade adapters so the batteries compose C's real codecs
//! under D's writer and F's reader TODAY, with zero product edits.
//!
//! - [`QaResolver`] — `pgrc_codec::registry()` as the writer's round-trip
//!   [`VerifyResolver`] (every seal verifies through the SAME vtables the
//!   reader dispatches).
//! - [`full_binding`] — the reader [`CodecBinding`] over the full codec
//!   registry + the LZ4 unwrapper (C's `wrapper::unwrap_section` behind
//!   F's `SectionUnwrapper` seam).
//! - [`ForcedElection`] — a fixture `CandidateSource` that makes C's real
//!   encoders win the writer's election (candidate priced at 1 byte: the
//!   ≥10%-win gate always accepts). Legitimate for QA by the metamorphic
//!   law: ANY election must yield identical logical answers, and the seal
//!   path still `verify_roundtrip`s every granule through the real
//!   vtables. NOTE (seam fact, reported in the lane report): the frozen
//!   `CandidateSource::propose(stats)` face carries no column identity and
//!   no values, so data-priced candidates cannot plug through it —
//!   fixtures force AT MOST ONE plan per storage class per table.
//! - [`Probe`] — one scripted clog serving both the writer's [`TxnProbe`]
//!   and the reader's [`CommitCheck`] (the two sides must agree on
//!   effectiveness — several legs assert exactly that).

use pgrc_codec::registry;
use pgrc_format::abi::{CodecVtable, GranuleEncoder, KernelKey};
use pgrc_format::class::StorageClass;
use pgrc_format::enc::Wrapper;
use pgrc_format::part::StreamSectionHdr;
use pgrc_format::FormatResult;
use pgrc_read::io::MemTableDir;
use pgrc_read::manifest_walk::CommitCheck;
use pgrc_read::cursor::{CodecBinding, SectionUnwrapper};
use pgrc_write::elect::{Candidate, CandidateSource, EncoderFactory, FullElectInput};
use pgrc_write::publish::{TxnProbe, TxnVerdict};
use pgrc_write::seal::VerifyResolver;
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// resolver + binding over the real registry
// ---------------------------------------------------------------------------

/// The full-registry round-trip resolver for `seal_part`.
#[derive(Debug, Default, Clone, Copy)]
pub struct QaResolver;

impl VerifyResolver for QaResolver {
    fn resolve(&self, key: KernelKey) -> pgrc_write::WriteResult<&'static CodecVtable> {
        registry().resolve(key).map_err(Into::into)
    }
}

/// C's LZ4 unwrap behind F's decode-side seam.
struct Lz4Unwrapper;

impl SectionUnwrapper for Lz4Unwrapper {
    fn wrapper(&self) -> Wrapper {
        Wrapper::Lz4
    }

    fn unwrap_section(&self, _hdr: &StreamSectionHdr, section: &[u8]) -> FormatResult<Vec<u8>> {
        let mut out = Vec::new();
        pgrc_codec::wrapper::unwrap_section(section, &mut out)?;
        Ok(out)
    }
}

/// C's Zstd unwrap behind the same seam (spec §6.4 `wrapper = 2`, the
/// CMP-A slot-fill).
struct ZstdUnwrapper(Wrapper);

impl SectionUnwrapper for ZstdUnwrapper {
    fn wrapper(&self) -> Wrapper {
        self.0
    }

    fn unwrap_section(&self, _hdr: &StreamSectionHdr, section: &[u8]) -> FormatResult<Vec<u8>> {
        let mut out = Vec::new();
        pgrc_codec::wrapper::unwrap_section(section, &mut out)?;
        Ok(out)
    }
}


/// pgrc format 2.2 dictionary layers (spec §6.4/§7): the front-coded DictPayload
/// (id 3) expands against the resident index, the packed DictIndex (id 4)
/// stands alone; both serve the whole-section and block faces.
struct FrontZstdUnwrapper(Wrapper);

impl SectionUnwrapper for FrontZstdUnwrapper {
    fn wrapper(&self) -> Wrapper {
        self.0
    }
    fn unwrap_section(&self, _hdr: &StreamSectionHdr, section: &[u8]) -> FormatResult<Vec<u8>> {
        let mut out = Vec::new();
        pgrc_codec::wrapper::unwrap_section(section, &mut out)?;
        Ok(out)
    }
    fn unwrap_dict_payload_section(&self, _hdr: &StreamSectionHdr, section: &[u8], index: &[u8]) -> FormatResult<Vec<u8>> {
        let mut out = Vec::new();
        pgrc_codec::wrapper::unwrap_dict_payload_section(section, index, &mut out)?;
        Ok(out)
    }
    fn decode_side(&self, side: &[u8]) -> FormatResult<Option<pgrc_read::SideTable>> {
        if !self.0.side_block() {
            return Ok(None);
        }
        let t = pgrc_codec::onpairwrap::decode_side(side)?;
        let bytes = t.bytes();
        Ok(Some(pgrc_read::SideTable { table: Box::new(t), bytes }))
    }
    fn unwrap_dict_payload_block_side(&self, side: &pgrc_read::SideTable, src: &[u8], dst: &mut [u8], records: &[u8]) -> FormatResult<bool> {
        let Some(t) = side.table.downcast_ref::<pgrc_codec::onpair::DecodeTable>() else { return Ok(false) };
        pgrc_codec::onpairwrap::unwrap_frame_block(t, src, dst, records)?;
        Ok(true)
    }
    fn unwrap_dict_payload_block(&self, src: &[u8], dst: &mut [u8], records: &[u8]) -> FormatResult<bool> {
        if self.0 != Wrapper::FrontZstd {
            return Ok(false);
        }
        pgrc_codec::wrapper::unwrap_dict_payload_block(Wrapper::FrontZstd, src, dst, records)?;
        Ok(true)
    }
}

struct PackedZstdUnwrapper;

impl SectionUnwrapper for PackedZstdUnwrapper {
    fn wrapper(&self) -> Wrapper {
        Wrapper::PackedZstd
    }
    fn unwrap_section(&self, _hdr: &StreamSectionHdr, section: &[u8]) -> FormatResult<Vec<u8>> {
        let mut out = Vec::new();
        pgrc_codec::wrapper::unwrap_section(section, &mut out)?;
        Ok(out)
    }
    fn unwrap_block(&self, src: &[u8], dst: &mut [u8]) -> FormatResult<bool> {
        pgrc_codec::wrapper::unwrap_block_into(Wrapper::PackedZstd, src, dst)?;
        Ok(true)
    }
}

/// The integer pre-pass unwrap (id 8) behind the same seam.
struct IntPreUnwrapper;

impl SectionUnwrapper for IntPreUnwrapper {
    fn wrapper(&self) -> Wrapper {
        Wrapper::IntPre
    }
    fn unwrap_section(&self, _hdr: &StreamSectionHdr, section: &[u8]) -> FormatResult<Vec<u8>> {
        let mut out = Vec::new();
        pgrc_codec::wrapper::unwrap_section(section, &mut out)?;
        Ok(out)
    }
}

/// The full reader binding: every shipped kernel + the LZ4/Zstd unwrappers.
/// Leaked once per process (test scaffolding; the product binding is
/// M3-G/H's static).
pub fn full_binding() -> &'static CodecBinding<'static> {
    static LZ4: Lz4Unwrapper = Lz4Unwrapper;
    static ZSTD: ZstdUnwrapper = ZstdUnwrapper(Wrapper::Zstd);
    static ONPAIR_VALUES: ZstdUnwrapper = ZstdUnwrapper(Wrapper::OnPairZstd);
    static FRONT: FrontZstdUnwrapper = FrontZstdUnwrapper(Wrapper::FrontZstd);
    static FRONT_ONPAIR: FrontZstdUnwrapper = FrontZstdUnwrapper(Wrapper::FrontOnPairZstd);
    static PACKED: PackedZstdUnwrapper = PackedZstdUnwrapper;
    static INTPRE: IntPreUnwrapper = IntPreUnwrapper;
    let unwrappers: &'static [&'static dyn SectionUnwrapper] = Box::leak(Box::new([
        &LZ4 as &dyn SectionUnwrapper,
        &ZSTD as &dyn SectionUnwrapper,
        &FRONT as &dyn SectionUnwrapper,
        &PACKED as &dyn SectionUnwrapper,
        &FRONT_ONPAIR as &dyn SectionUnwrapper,
        &ONPAIR_VALUES as &dyn SectionUnwrapper,
        &INTPRE as &dyn SectionUnwrapper,
    ]));
    Box::leak(Box::new(CodecBinding {
        registry: registry(),
        unwrappers,
    }))
}

// ---------------------------------------------------------------------------
// forced elections (C's encoders under D's seal)
// ---------------------------------------------------------------------------

/// Which real codec a fixture forces for a storage class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForcedPlan {
    /// BYTE_FOR at a fixed delta width (fixture data must fit it — the
    /// seal's mandatory round-trip verify catches a mis-sized fixture).
    ByteFor { delta_width: u8, signed: bool },
    DeltaFor,
    Alp,
    AlpRd,
    Bool,
}

struct ForcedFactory {
    plan: ForcedPlan,
    byval_width: u8,
}

impl ForcedFactory {
    fn build(&self) -> Box<dyn GranuleEncoder> {
        match self.plan {
            ForcedPlan::ByteFor {
                delta_width,
                signed,
            } => Box::new(pgrc_codec::bytefor::ByteForEncoder::new_bytefor(
                self.byval_width,
                delta_width,
                signed,
            )),
            ForcedPlan::DeltaFor => Box::new(pgrc_codec::deltafor::DeltaForEncoder::default()),
            ForcedPlan::Alp => Box::new(pgrc_codec::alpc::AlpEncoder {
                encoding: pgrc_format::enc::EncodingId::Alp,
                carry: None,
            }),
            ForcedPlan::AlpRd => Box::new(pgrc_codec::alpc::AlpEncoder {
                encoding: pgrc_format::enc::EncodingId::AlpRd,
                carry: None,
            }),
            ForcedPlan::Bool => Box::new(pgrc_codec::boolbm::BoolBitmapEncoder),
        }
    }
}

impl EncoderFactory for ForcedFactory {
    fn key(&self) -> KernelKey {
        // Delegate to the encoder's own key — cannot drift.
        self.build().key()
    }

    fn make(&self) -> Box<dyn GranuleEncoder> {
        self.build()
    }
}

/// The fixture candidate source: proposes `plan` (priced 1 — always wins the
/// integer gate) for every stream whose storage class it fits, nothing else.
#[derive(Debug, Clone, Copy)]
pub struct ForcedElection {
    pub plan: ForcedPlan,
}

impl ForcedElection {
    pub fn new(plan: ForcedPlan) -> ForcedElection {
        ForcedElection { plan }
    }
}

impl CandidateSource for ForcedElection {
    fn propose(&self, input: &FullElectInput<'_>) -> Vec<Candidate> {
        let stats = input.stats;
        let byval_width = match (self.plan, stats.class) {
            (ForcedPlan::ByteFor { .. }, StorageClass::ByvalWord { width, .. }) => width,
            (ForcedPlan::DeltaFor, StorageClass::ByvalWord { width, .. }) => width,
            (ForcedPlan::Alp | ForcedPlan::AlpRd, StorageClass::F64) => 8,
            (ForcedPlan::Bool, StorageClass::Bool) => 1,
            _ => return Vec::new(),
        };
        vec![Candidate {
            factory: Box::new(ForcedFactory {
                plan: self.plan,
                byval_width,
            }),
            encoded_len: 1,
        }]
    }
}

// ---------------------------------------------------------------------------
// the scripted clog (one truth, both faces)
// ---------------------------------------------------------------------------

/// Scripted transaction verdicts serving writer (`TxnProbe`) and reader
/// (`CommitCheck`) alike.
#[derive(Debug, Clone)]
pub struct Probe {
    verdicts: BTreeMap<u64, TxnVerdict>,
    default: TxnVerdict,
}

impl Probe {
    pub fn new(default: TxnVerdict) -> Probe {
        Probe {
            verdicts: BTreeMap::new(),
            default,
        }
    }

    pub fn set(mut self, fxid: u64, v: TxnVerdict) -> Probe {
        self.verdicts.insert(fxid, v);
        self
    }

    pub fn mark(&mut self, fxid: u64, v: TxnVerdict) {
        self.verdicts.insert(fxid, v);
    }
}

impl TxnProbe for Probe {
    fn verdict(&self, fxid: u64) -> TxnVerdict {
        self.verdicts.get(&fxid).copied().unwrap_or(self.default)
    }
}

impl CommitCheck for Probe {
    fn committed(&self, fxid: u64) -> bool {
        TxnProbe::verdict(self, fxid) == TxnVerdict::Committed
    }
}

/// A directory snapshot (name → bytes) as the reader's `TableDirIo`.
pub fn memdir_of(files: &BTreeMap<String, Vec<u8>>) -> MemTableDir {
    let mut d = MemTableDir::new();
    for (name, bytes) in files {
        d.put(name, bytes.clone());
    }
    d
}
