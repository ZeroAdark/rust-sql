//! # pgrc_format — the pgrcolumnar on-disk format + codec ABI (chunk M3-A)
//!
//! THE frozen M3 surface: every downstream pgrcolumnar chunk (M3-B..L,
//! `docs/design/lanev3-m3-chunks.md` §2/§4) builds against this crate's merged
//! tip. The byte-level contract is `docs/design/pgrc-format.md` (cited below
//! as `spec §N`); the design authority is `docs/design/pgrcolumnar-v2.md`.
//! Amending anything here is an A-lane PR against BOTH files — consumers never
//! side-edit the freeze (`lanev3-m3-chunks.md` §4 wave-1 law).
//!
//! ## Ruling provenance (binding; never re-litigated here)
//!
//! - **O-M3-2** (naming): the `pgrcolumnar` sibling family; the old
//!   `access/pgrcolumnar` crate is a design-only donor, untouched, deleted M7.
//! - **O-2 / O-M3-1(a)**: tombstones-first DV ladder — the complete
//!   tombstone/DV vocabulary is [`dml`] (spec §15); no deletion machinery at
//!   M3 (COPY + SELECT only; trickle DML gets typed refusals at M3-H).
//! - **O-3** (superseded by delta v2 R3, 2026-09-22): publish is WAL-first
//!   ([`manifest`], spec §13); the `pgrc` rmgr's records are [`wal`] (spec §14).
//! - **O-5 / O-M3-5**: eq-only enums in v1; the enumsortorder zone-key slot is
//!   reserved as [`meta::KeyKind::EnumRankReserved`] (spec §8.1).
//! - **O-7**: own-directory residency vocabulary in [`dirlayout`] (spec §12).
//! - **O-8 / O-M3-5**: packed columnar RowId in [`rowid`] (spec §10) with the
//!   10B-row bit-budget static assert; typed-TID-refusal posture is M3-H's.
//! - **O-9**: the ruled reloption vocabulary in [`relopt`] (spec §17).
//! - **O-10**: logical bank identity + required MANIFEST fields in [`bank`]
//!   (spec §18).
//! - **O-11 / O-12**: predicate-cache/memo/trgm sidecar slots RESERVED in
//!   [`sidecar`] (spec §16).
//! - **O-M3-3**: encoding id 12 (the retired FSST codec,
//!   [`enc::ENC_FSST_RESERVED`]) is RESERVED and refuses typed
//!   (ReservedEncoding); the unassigned band 13..=127 refuses typed
//!   (UnknownEncoding).
//! - **M0-S1/M0-S4** frozen geometry + encoding matrix: [`geom`], [`enc`]
//!   (spec §2/§4); the pow2-switch dispatch law and ctx-relative kernel law
//!   are ABI contracts in [`abi`] (spec §19).
//! - **StrView §7b** (`lanev3-strview.md`): every string payload region is
//!   varlena-shaped, 8-aligned (spec §1) — pinned in this crate's tests.
//!
//! ## Module map (spec section in parens)
//!
//! [`geom`] (§2) · [`class`] (§3) · [`enc`] (§4) · [`part`] (§5–§6) ·
//! [`dict`] (§7) · [`meta`] (§8) · [`sortkey`] (§9) · [`rowid`] (§10) ·
//! [`ident`] (§11 + §5.5) · [`dirlayout`] (§12) · [`manifest`] (§13) ·
//! [`wal`] (§14) · [`dml`] (§15) · [`sidecar`] (§16) ·
//! [`relopt`] (§17) · [`bank`] (§18) · [`abi`] (§19) · [`verbatim`] (§19.1,
//! the reference codec proving the ABI) · [`wire`] (LE + CRC + varlena
//! plumbing).
//!
//! ## Crate laws
//!
//! - Pure: no I/O, no clocks, no locks, no thread-locals, no env — encode is
//!   `&mut Vec<u8>`-out, decode is `&[u8]`-in. (`Vec<u8>`/`String` here are
//!   wire-encode staging buffers per the in-tree format-crate precedent —
//!   `pgrcolumnar/src/format.rs` — not engine collections; callers own
//!   allocation policy, and decode-side kernels are allocation-free.)
//! - Every failure is a typed [`FormatError`]; decode kernels are no-panic and
//!   bounds-validated even after CRC passes (spec §1).
//! - Every on-disk struct is layout-pinned (`src/tests/layout.rs`, the
//!   issue-#69 template) and every wire shape golden-pinned
//!   (`src/tests/golden.rs`); the reference codec round-trips the full
//!   six-face ABI (`src/tests/roundtrip.rs`); unknown/reserved-encoding
//!   refusals are born-RED-tested with seeded IDs (`src/tests/refusal.rs`).
//! - `unsafe` is confined to [`abi`]'s datum-image access kernel (reading
//!   pointer-class datums during encode/verify), each block `// SAFETY:`
//!   documented.

pub mod abi;
pub mod answers;
pub mod bank;
pub mod bankstats;
pub mod class;
pub mod dict;
pub mod dirlayout;
pub mod dml;
pub mod enc;
pub mod facts;
pub mod geom;
pub mod ident;
pub mod manifest;
pub mod meta;
pub mod part;
pub mod relopt;
pub mod rowid;
pub mod rowmap;
pub mod runrev;
pub mod sample;
pub mod sections;
pub mod shredlane;
pub mod sidecar;
pub mod sortkey;
pub mod sortrun;
pub mod spool;
pub mod statsrun;
pub mod verbatim;
pub mod wal;
pub mod wire;
pub mod zonemap;

#[cfg(test)]
mod tests;

/// On-disk format version (spec §5.1). Bumped only by A-lane PRs.
pub const FORMAT_VERSION: u32 = 1;

/// The codec/meta-builder ABI version (spec §19). Downstream crates may pin
/// against it; it moves only with an A-lane ABI amendment.
pub const ABI_VERSION: u32 = 1;

/// The typed error vocabulary for the whole format surface (spec §1: every
/// failure is typed — no panic, no fallback, no silent skip).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    /// Input ended before the structure did. `at` names the structure.
    Truncated { at: &'static str },
    /// A magic word did not match.
    BadMagic { at: &'static str },
    /// A version field is outside what this crate reads.
    BadVersion { at: &'static str, got: u32 },
    /// A crc32c check failed (spec §5.2 per-section CRCs).
    CrcMismatch { at: &'static str },
    /// Structurally invalid contents (ordering, counts, ranges). Typed
    /// refusal, never UB — the #66/#340 incident-class law.
    Corrupt { at: &'static str },
    /// Encoding ID never assigned (spec §4). Born-RED-tested.
    UnknownEncoding { id: u16 },
    /// Encoding ID reserved, never reused (12, the retired FSST codec).
    ReservedEncoding { id: u16 },
    /// Footer section table names a kind this reader does not know and the
    /// entry is not flagged `SECTION_OPTIONAL` (spec §5.2).
    UnknownSectionKind { kind: u16 },
    /// Stream directory names an unknown role (spec §6.1).
    UnknownStreamRole { role: u8 },
    /// Stream entry names an unknown storage class (spec §3).
    UnknownStorageClass { class: u8 },
    /// Catalog properties and the refinement hint disagree (spec §3).
    BadClassHint { detail: &'static str },
    /// The face is not served by this encoding (spec §19.3).
    FaceUnsupported { face: abi::Face, encoding: u16 },
    /// No kernel registered for the key (spec §19.5).
    KernelMissing { encoding: u16, class: u8, width: u8 },
    /// Decode arena exhausted (typed, never a resize — spec §19.4).
    ArenaExhausted { needed: usize },
    /// An offset/length reached outside its section (spec §1).
    Bounds { at: &'static str },
    /// Encode-side contract violation (e.g. CONST input not constant).
    EncodeContract { detail: &'static str },
    /// Wrapper block assembly is the codec crate's (M3-C); the format-crate
    /// section writer emits unwrapped sections only (spec §6.4).
    WrapperUnsupported { wrapper: u8 },
    /// A fallible allocation failed; the AM raises it as ERROR 53200.
    OutOfMemory { bytes: usize },
}

impl core::fmt::Display for FormatError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FormatError::Truncated { at } => write!(f, "pgrc format: truncated at {at}"),
            FormatError::BadMagic { at } => write!(f, "pgrc format: bad magic at {at}"),
            FormatError::BadVersion { at, got } => {
                write!(f, "pgrc format: unreadable version {got} at {at}")
            }
            FormatError::CrcMismatch { at } => write!(f, "pgrc format: crc mismatch at {at}"),
            FormatError::Corrupt { at } => write!(f, "pgrc format: corrupt structure at {at}"),
            FormatError::UnknownEncoding { id } => {
                write!(f, "pgrc format: unknown encoding id {id}")
            }
            FormatError::ReservedEncoding { id } => {
                write!(
                    f,
                    "pgrc format: reserved encoding id {id} (not implemented)"
                )
            }
            FormatError::UnknownSectionKind { kind } => {
                write!(f, "pgrc format: unknown required section kind {kind}")
            }
            FormatError::UnknownStreamRole { role } => {
                write!(f, "pgrc format: unknown stream role {role}")
            }
            FormatError::UnknownStorageClass { class } => {
                write!(f, "pgrc format: unknown storage class {class}")
            }
            FormatError::BadClassHint { detail } => {
                write!(f, "pgrc format: class/hint mismatch: {detail}")
            }
            FormatError::FaceUnsupported { face, encoding } => {
                write!(
                    f,
                    "pgrc abi: face {face:?} unsupported by encoding {encoding}"
                )
            }
            FormatError::KernelMissing {
                encoding,
                class,
                width,
            } => write!(
                f,
                "pgrc abi: no kernel for (encoding {encoding}, class {class}, width {width})"
            ),
            FormatError::ArenaExhausted { needed } => {
                write!(
                    f,
                    "pgrc abi: decode arena exhausted ({needed} bytes needed)"
                )
            }
            FormatError::Bounds { at } => write!(f, "pgrc format: out-of-bounds at {at}"),
            FormatError::EncodeContract { detail } => {
                write!(f, "pgrc encode: contract violation: {detail}")
            }
            FormatError::WrapperUnsupported { wrapper } => {
                write!(
                    f,
                    "pgrc encode: wrapper {wrapper} assembly is the codec crate's"
                )
            }
            FormatError::OutOfMemory { bytes } => {
                write!(f, "pgrc format: out of memory ({bytes} bytes)")
            }
        }
    }
}

impl std::error::Error for FormatError {}

/// Result alias for the whole crate.
pub type FormatResult<T> = Result<T, FormatError>;

/// `Vec::with_capacity(n)` whose failure is `FormatError::OutOfMemory`.
pub fn try_vec<T>(n: usize, _at: &'static str) -> FormatResult<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n)
        .map_err(|_| FormatError::OutOfMemory { bytes: n.saturating_mul(core::mem::size_of::<T>()) })?;
    Ok(v)
}

/// `vec![val; n]` whose failure is `FormatError::OutOfMemory`.
pub fn try_filled<T: Clone>(n: usize, val: T, _at: &'static str) -> FormatResult<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n)
        .map_err(|_| FormatError::OutOfMemory { bytes: n.saturating_mul(core::mem::size_of::<T>()) })?;
    v.resize(n, val);
    Ok(v)
}

/// `slice.to_vec()` whose failure is `FormatError::OutOfMemory`.
pub fn try_to_vec<T: Clone>(s: &[T], at: &'static str) -> FormatResult<Vec<T>> {
    let mut v = try_vec(s.len(), at)?;
    v.extend_from_slice(s);
    Ok(v)
}

/// `v.resize(n, val)` whose failure is `FormatError::OutOfMemory`.
pub fn try_resize<T: Clone>(v: &mut Vec<T>, n: usize, val: T, _at: &'static str) -> FormatResult<()> {
    if n > v.len() {
        let extra = n - v.len();
        v.try_reserve(extra)
            .map_err(|_| FormatError::OutOfMemory { bytes: extra.saturating_mul(core::mem::size_of::<T>()) })?;
    }
    v.resize(n, val);
    Ok(())
}

/// `v.reserve(extra)` whose failure is `FormatError::OutOfMemory`.
pub fn try_reserve_more<T>(v: &mut Vec<T>, extra: usize, _at: &'static str) -> FormatResult<()> {
    v.try_reserve(extra)
        .map_err(|_| FormatError::OutOfMemory { bytes: extra.saturating_mul(core::mem::size_of::<T>()) })
}

/// Result-neutral accelerators (bloom blocks, sort-order shortcuts, the writer's stored-lane prices) that were skipped
/// because their allocation failed. A skipped structure is exactly "not built": the reader falls back and results do
/// not change. The count and bytes are cumulative; the access method drains them into a LOG line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accel {
    Bloom,
    SortOrder,
    LanePrice,
    BankPlane,
    Answers,
    /// Part-grain distinct-count statistics, left missing (never partial) so the next ANALYZE recomputes them.
    DistStats,
}

const ACCEL_NAMES: [&str; 6] =
    ["bloom block", "sketch sort order", "stored-lane price", "bank plane records", "answers", "distinct-count stats"];
static ACCEL_COUNT: [core::sync::atomic::AtomicU64; 6] = [const { core::sync::atomic::AtomicU64::new(0) }; 6];
static ACCEL_BYTES: [core::sync::atomic::AtomicU64; 6] = [const { core::sync::atomic::AtomicU64::new(0) }; 6];
static ACCEL_TOTAL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Count one skipped accelerator of `bytes` bytes.
pub fn note_accel_skipped(what: Accel, bytes: usize) {
    use core::sync::atomic::Ordering::Relaxed;
    ACCEL_COUNT[what as usize].fetch_add(1, Relaxed);
    ACCEL_BYTES[what as usize].fetch_add(bytes as u64, Relaxed);
    ACCEL_TOTAL.fetch_add(1, Relaxed);
}

/// The cumulative number of skipped accelerators.
pub fn accel_skips_total() -> u64 {
    ACCEL_TOTAL.load(core::sync::atomic::Ordering::Relaxed)
}

/// Drain the per-structure counts since the last drain: (name, count, bytes) for every non-zero structure.
pub fn take_accel_skips() -> Vec<(&'static str, u64, u64)> {
    use core::sync::atomic::Ordering::Relaxed;
    let mut out = Vec::new();
    for i in 0..ACCEL_NAMES.len() {
        let c = ACCEL_COUNT[i].swap(0, Relaxed);
        if c != 0 {
            out.push((ACCEL_NAMES[i], c, ACCEL_BYTES[i].swap(0, Relaxed)));
        }
    }
    out
}
