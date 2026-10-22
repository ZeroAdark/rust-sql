//! Encoding registry (spec §4; the M0-S4 frozen matrix). IDs are assigned
//! only here + spec §4, by A-lane PRs. Resolution distinguishes UNKNOWN
//! (never assigned, a typed refusal, never a fallback) from assigned IDs;
//! the RESERVED class holds id 12 (the retired FSST codec): it is never
//! reused and refuses typed (`ReservedEncoding`).

use crate::{FormatError, FormatResult};

/// The assigned encoding IDs (spec §4 table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum EncodingId {
    /// Class-shaped byte-exact fallback (spec §6.7) — the universal demotion
    /// target: refusal demotes, never normalizes.
    Verbatim = 0,
    /// One value per extent (spec §6.9).
    Const = 1,
    /// Byte-aligned FOR, per-frame i64 refs, widths 1/2/4/8 (spec §6.10) —
    /// the int-family primary (M0-S4 frozen).
    ByteFor = 2,
    /// FastLanes-style transposed FFOR, interleave layout — the elected
    /// narrow-width tier for FUSED consumers only (S4 verdict (a)).
    FforInterleave = 3,
    /// Cold/size election only (S4 verdict (b) struck it from hot tiers).
    DeltaFor = 4,
    /// Float primary-by-election, RawF(=Verbatim) fallback (S4 verdict (c)).
    Alp = 5,
    /// ALP-RD for full-mantissa columns (weak trade — expect Verbatim to win
    /// the ≥10% election often).
    AlpRd = 6,
    /// 1-bit bitmap (bool primary; also the validity substream layout,
    /// spec §6.6).
    BoolBitmap = 7,
    /// Bit-packed GLOBAL codes, per-granule base+width (spec §6.11).
    DictCodes = 8,
    /// A6b fixed-scale packed i64 (elected scale in `aux32`).
    PackedNumeric = 9,
    /// A9 sizes+elements dual substreams (children per spec §6.5).
    ArrayDual = 10,
    /// Shredded jsonb path lanes (vendored `jsonb_shred` vocabulary).
    JsonbShred = 11,
}

/// Encoding id 12: the retired FSST codec. Never reused; resolution refuses it
/// typed (`ReservedEncoding`), so a part carrying it is refused, never misread.
pub const ENC_FSST_RESERVED: u16 = 12;

/// IDs 13..=127 are reserved for A-lane assignment; resolution refuses them
/// as UNKNOWN (they were never assigned).
pub const ENC_RESERVED_MAX: u16 = 127;

impl EncodingId {
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// Adjudicate a wire encoding id (spec §4). Born-RED-tested with seeded
    /// unknown IDs in 13..=127 (`tests/refusal.rs`).
    pub fn resolve(id: u16) -> FormatResult<EncodingId> {
        match id {
            0 => Ok(EncodingId::Verbatim),
            1 => Ok(EncodingId::Const),
            2 => Ok(EncodingId::ByteFor),
            3 => Ok(EncodingId::FforInterleave),
            4 => Ok(EncodingId::DeltaFor),
            5 => Ok(EncodingId::Alp),
            6 => Ok(EncodingId::AlpRd),
            7 => Ok(EncodingId::BoolBitmap),
            8 => Ok(EncodingId::DictCodes),
            9 => Ok(EncodingId::PackedNumeric),
            10 => Ok(EncodingId::ArrayDual),
            11 => Ok(EncodingId::JsonbShred),
            ENC_FSST_RESERVED => Err(FormatError::ReservedEncoding { id }),
            _ => Err(FormatError::UnknownEncoding { id }),
        }
    }
}

/// Normalize stream-entry fields to the [`KernelKey`] a codec registered
/// (spec §6.3 + §19.5, A-lane amendment M3-A2): the entry's `width` byte is
/// **per-encoding vocabulary** (ByvalWord width, BYTE_FOR delta width,
/// DICT_CODES max code width — for DICT_CODES a stats fact, never a dispatch
/// axis), while `KernelKey.width` is the **normalized dispatch width**. This
/// is the SINGLE mapping every resolver consults — the read side
/// (`pgrc_read` cursor open + every ctx it builds) and the write side's
/// round-trip verify both come through here; building a `KernelKey` from raw
/// entry fields is a contract defect (the M3-G gap-1 seam: a spec-conformant
/// dict stream whose width byte carries the max code width cannot resolve
/// through a raw-field key).
///
/// Lives in the format crate because both `pgrc_codec` (which cannot be a
/// `pgrc_read` dependency) and `pgrc_read` (which cannot depend on the
/// codec crate) must agree on it; `pgrc_codec::dispatch::stream_kernel_key`
/// delegates here.
pub fn stream_kernel_key(
    encoding: u16,
    class: u8,
    stream_width: u8,
) -> FormatResult<crate::abi::KernelKey> {
    use crate::class::{CLASS_BOOL, CLASS_BYVAL, CLASS_F32, CLASS_F64};
    let id = EncodingId::resolve(encoding)?;
    let width = match id {
        // Verbatim/Const key on the class width the reference registered.
        EncodingId::Verbatim | EncodingId::Const => match class {
            CLASS_BYVAL => match stream_width {
                1 | 2 | 4 | 8 => stream_width,
                _ => {
                    return Err(FormatError::Corrupt {
                        at: "stream width byte",
                    })
                }
            },
            CLASS_F32 => 4,
            CLASS_F64 => 8,
            CLASS_BOOL => 1,
            _ => 0,
        },
        // BYTE_FOR / PACKED_NUMERIC key on the elected delta/mantissa
        // width — the full byte-aligned ladder {1..=8} (SB-3 widened the
        // v3 pow2 set {1,2,4,8}).
        EncodingId::ByteFor | EncodingId::PackedNumeric => match stream_width {
            1..=8 => stream_width,
            _ => {
                return Err(FormatError::Corrupt {
                    at: "stream width byte",
                })
            }
        },
        EncodingId::BoolBitmap => 1,
        // Granule/section-self-describing families key on width 0.
        // DICT_CODES' entry width byte carries the stream's max code width
        // (spec §6.3 — a stats fact; 0 = unrecorded); it never keys
        // dispatch.
        EncodingId::FforInterleave
        | EncodingId::DeltaFor
        | EncodingId::Alp
        | EncodingId::AlpRd
        | EncodingId::DictCodes => 0,
        // Structural elections never appear on disk (spec §4 ruling): a
        // stream entry or section header carrying them is corrupt —
        // ARRAY_DUAL/JSONB_SHRED are election-grain markers whose
        // substreams carry real IDs.
        EncodingId::ArrayDual | EncodingId::JsonbShred => {
            return Err(FormatError::Corrupt {
                at: "structural encoding in stream entry",
            })
        }
    };
    Ok(crate::abi::KernelKey {
        encoding,
        class,
        width,
    })
}

/// Section-payload wrapper (spec §4/§6.4): disk-only layers the reader
/// rebuilds at extent open. Ids 3/4 (pgrc format 2.2, spec §6.4/§7) are the
/// dictionary layers: a front-coded `DictPayload` beneath zstd and the
/// packed `DictIndex` beneath zstd — both expand to the v1 images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Wrapper {
    None = 0,
    Lz4 = 1,
    Zstd = 2,
    /// `DictPayload` only: per-frame front coding (spec §7, restart every
    /// `DICT_FC_RESTART` entries) then zstd, one block per dict frame.
    FrontZstd = 3,
    /// `DictIndex` only: the packed index image (spec §7: offset anchors,
    /// frame-FOR byte lengths, char deltas) then zstd, one block.
    PackedZstd = 4,
    /// `DictPayload` only: `FrontZstd`'s front coding with the string bytes OnPair-tokenized against one
    /// table per section, then zstd; one block per dict frame, then the table's block.
    FrontOnPairZstd = 5,
    /// Verbatim varlena `Values` only: the slot tables and entry headers as varints, the entries' bytes
    /// OnPair-tokenized against one table per section, then zstd; one block, then the table's block.
    OnPairZstd = 6,
    /// Value streams: per granule block, an integer pre-pass (RLE, linear runs, patched base, delta + scale) of the
    /// encoded image then zstd, or the plain zstd block behind a mode byte. Id 7 is reserved.
    IntPre = 8,
}

impl Wrapper {
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
    /// Adjudicate a wire wrapper id: an id this build does not know is a
    /// typed `WrapperUnsupported` (the old-binary posture, born-RED-tested)
    /// — never a corruption verdict, never a fallback.
    pub fn from_u8(v: u8) -> FormatResult<Wrapper> {
        match v {
            0 => Ok(Wrapper::None),
            1 => Ok(Wrapper::Lz4),
            2 => Ok(Wrapper::Zstd),
            3 => Ok(Wrapper::FrontZstd),
            4 => Ok(Wrapper::PackedZstd),
            5 => Ok(Wrapper::FrontOnPairZstd),
            6 => Ok(Wrapper::OnPairZstd),
            8 => Ok(Wrapper::IntPre),
            _ => Err(FormatError::WrapperUnsupported { wrapper: v }),
        }
    }
    /// The section's last block is a side block every frame block decodes against (`FrontOnPairZstd`'s
    /// token table): blocks = frames + 1.
    pub const fn side_block(self) -> bool {
        matches!(self, Wrapper::FrontOnPairZstd)
    }
    /// The one stream role a dictionary wrapper may stamp (`None` for the
    /// role-agnostic layers).
    pub const fn dict_role(self) -> Option<crate::part::StreamRole> {
        match self {
            Wrapper::FrontZstd | Wrapper::FrontOnPairZstd => Some(crate::part::StreamRole::DictPayload),
            Wrapper::PackedZstd => Some(crate::part::StreamRole::DictIndex),
            Wrapper::None | Wrapper::Lz4 | Wrapper::Zstd | Wrapper::OnPairZstd | Wrapper::IntPre => None,
        }
    }
}
