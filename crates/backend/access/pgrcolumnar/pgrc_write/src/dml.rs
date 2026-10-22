//! # dml — the M5e trickle-DML producer faces (M5e-SCOUT scaffold; NO
//! execution path)
//!
//! **Posture (the scout law):** this module is the M5e build lane's
//! copy-and-wire target, landed BEFORE the build so its first day is not a
//! survey day. It defines the WRITE-SIDE contract faces of the closed §3.7
//! delta/DML representation — nothing here is reachable from any ingest,
//! seal, or publish path; no AM arm calls it; no census row exists. The
//! survey artifact is `docs/design/lanev4-m5e-scout.md` (producer map,
//! copy sources, open questions).
//!
//! Laws these faces are shaped by (every one is a SIGNED format-ledger row,
//! `docs/design/lanev4-format-ledger.md` §7, OD-3 RULED 2026-08-12):
//!
//! - **DM-1 (delta parts ARE parts):** trickle DML buffers in a WAL-logged
//!   rowstore (the `pgrc_delta` heap delta-relation lineage) and flushes to
//!   DELTA PARTS at threshold — same format, same encodings menu under a
//!   smallness-aware election ([`delta_smallness`]); a delta part is never
//!   stats-blind (§3.2 all-stats-at-birth holds at every size). One read
//!   path, no second decoder.
//! - **DM-2 (Dv deletion vectors):** per-part delete vectors in the Dv
//!   sidecar slot (`pgrc_format::sidecar::SidecarKind::Dv`), compressed,
//!   rowid-indexed, generation-tagged; the manifest triplet
//!   ([`DvTriplet`] = `dv_gen`/`dv_len`/`dv_crc` on the 64 B `PartRecord`)
//!   is the AUTHORITATIVE reference. Replace-not-stack: at most ONE DV per
//!   part (`pgrc_format::dml` module law); the consumer half is landed —
//!   `pgrc_scan::PartDeletes` + `pgrc_am::scan`'s triplet validation
//!   (S3b, PR #946) — [`DvCompile`] is its producer mirror.
//! - **DM-3 (compaction law):** merge at part granularity — base + delta
//!   chain + Dv rewrite into a fresh generation-bumped part with FRESH
//!   elections and re-sealed exact footer facts; the generation bump is the
//!   FooterFacts cache invalidation key (FT-8). Frozen bank families are
//!   generation-0 immutable.
//! - **DM-4 (WAL hygiene per CMP-G/SB-8):** no dead heap-twin WAL class is
//!   ever re-created. Flush/merge/Dv publishes ride the pgrc rmgr's
//!   `FILE_WRITE` + `PUBLISH` records (`publish::publish_parts`); zero
//!   Heap/Heap2 records.
//!
//! Publish-ordering law carried here for the build lane (spec §15/§16 via
//! `pgrc_format::dml`): a Dv sidecar file is logged under a tmp name and
//! renamed by the same `PUBLISH` as the manifest generation that references
//! its triplet, and that generation precedes the deleting transaction's
//! commit record — redo gives either the old triplet (old deletions,
//! consistent) or both, never a manifest pointing at missing/mismatched Dv
//! bytes (which the S3b reader refuses as CORRUPTION).

use std::collections::BTreeMap;

use pgrc_format::dml::{encode_dv, DvBlockKind, DvReader, DV_BITMAP_BYTES};
use pgrc_format::geom::{BAND_ROWS, GRANULE_ROWS};
use pgrc_format::wire::crc32c;
use pgrc_format::{FormatError, FormatResult};

/// DM-1 smallness floor (PROVISIONAL — open question Q3 of the scout doc;
/// RE-MEASURE before any default ships): below this many buffered rows a
/// delta-part flush elects the light arms only — no dict build, no sampled
/// candidate ladder; VERBATIM/BYTE_FOR-class offers stand. One band is the
/// natural first anchor: the smallest extent grain the seal already emits.
pub const DELTA_SMALLNESS_ROWS_FLOOR: u64 = BAND_ROWS as u64;

/// DM-1: true when a part of `rows` rows takes the smallness-aware election
/// posture (the elect-side consumer maps this onto `CodecCandidates`
/// posture supply — dict arm withheld, stats computed regardless).
#[inline]
pub fn delta_smallness(rows: u64) -> bool {
    rows < DELTA_SMALLNESS_ROWS_FLOOR
}

/// The manifest Dv triplet (spec §13.1/§15) — the exact three fields the
/// S3b read side validates before decoding a payload
/// (`pgrc_am::scan`: sidecar named `(part_no, dv, dv_gen)`, payload length
/// == `dv_len`, `crc32c(payload)` == `dv_crc`; then
/// `pgrc_scan::PartDeletes::from_dv_payload` re-binds header identity).
/// The producer mints it from the encoded payload image, nowhere else — a
/// hand-assembled triplet is a contract defect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DvTriplet {
    /// Generation tag; `0` is RESERVED (== "no delete vector" on the
    /// `PartRecord`), so a real triplet always carries `dv_gen >= 1`.
    pub dv_gen: u64,
    /// Whole payload image length (header + blocks + trailing crc32c).
    pub dv_len: u64,
    /// crc32c over the whole payload image.
    pub dv_crc: u32,
}

impl DvTriplet {
    /// Mint the triplet for an `encode_dv` payload image at `dv_gen`.
    pub fn for_payload(dv_gen: u64, payload: &[u8]) -> DvTriplet {
        DvTriplet {
            dv_gen,
            dv_len: payload.len() as u64,
            dv_crc: crc32c(payload),
        }
    }
}

/// Wire-minimal DV block-kind law (the producer's election; the reader
/// accepts both): a List block's payload is `2 × count` bytes, a Bitmap
/// block's is [`DV_BITMAP_BYTES`] — List wins strictly below the crossover,
/// Bitmap at or above it. Pure, total, deterministic.
#[inline]
pub const fn dv_block_kind_for(count: u32) -> DvBlockKind {
    if (count as usize) * 2 < DV_BITMAP_BYTES {
        DvBlockKind::List
    } else {
        DvBlockKind::Bitmap
    }
}

/// The Dv compile face (DM-2 producer half; the compaction rung's writer):
/// accumulates deleted granule-row ordinals for ONE part and encodes the
/// COMPLETE replacement payload (replace-not-stack — the caller feeds every
/// horizon-passed deletion for the part, old Dv contents included, never a
/// delta over the previous Dv).
///
/// Determinism: granules in `BTreeMap` order and ascending bitmaps keep identical deletion sets byte-identical.
/// Bitmaps, not a per-row set: a compile seeds each touched part's whole DV (F2 S6: the set was its top cost).
#[derive(Debug, Default)]
pub struct DvCompile {
    granules: BTreeMap<u32, GranuleBits>,
}

const GRANULE_WORDS: usize = GRANULE_ROWS as usize / 64;

#[derive(Debug)]
struct GranuleBits {
    words: Box<[u64; GRANULE_WORDS]>,
    count: u32,
}

impl GranuleBits {
    fn new() -> GranuleBits {
        GranuleBits { words: Box::new([0; GRANULE_WORDS]), count: 0 }
    }

    fn set(&mut self, row: u16) {
        let (w, b) = (row as usize / 64, 1u64 << (row % 64));
        if self.words[w] & b == 0 {
            self.words[w] |= b;
            self.count += 1;
        }
    }

    fn rows(&self) -> FormatResult<Vec<u16>> {
        let mut out = pgrc_format::try_vec(self.count as usize, "dml")?;
        for (w, &word) in self.words.iter().enumerate() {
            let mut bits = word;
            while bits != 0 {
                out.push((w * 64) as u16 + bits.trailing_zeros() as u16);
                bits &= bits - 1;
            }
        }
        Ok(out)
    }
}

impl DvCompile {
    pub fn new() -> DvCompile {
        DvCompile::default()
    }

    /// Record one deleted row. `row` is the granule-row ordinal
    /// (`< GRANULE_ROWS`; the caller extracts it from the packed rowid via
    /// `pgrc_format::rowid`). Idempotent — re-marking is a no-op, so a
    /// tombstone replay cannot skew counts.
    pub fn mark(&mut self, granule: u32, row: u16) {
        debug_assert!((row as u32) < GRANULE_ROWS);
        self.granules.entry(granule).or_insert_with(GranuleBits::new).set(row);
    }

    /// Total deleted rows accumulated (the header fact the reader
    /// re-verifies against block payloads).
    pub fn deleted_rows(&self) -> u64 {
        self.granules.values().map(|g| u64::from(g.count)).sum()
    }

    /// True when nothing is marked — the caller publishes NO Dv (a zero
    /// triplet on the record), never an empty payload.
    pub fn is_empty(&self) -> bool {
        self.granules.is_empty()
    }

    /// Encode the complete payload image for `part_no` at generation `gen`
    /// (header + per-granule blocks in ascending granule order + crc32c),
    /// electing each block's kind by [`dv_block_kind_for`]. The caller
    /// mints the manifest triplet from the returned image via
    /// [`DvTriplet::for_payload`].
    pub fn encode(&self, part_no: u32, gen: u64) -> FormatResult<Vec<u8>> {
        let ordinals: Vec<(u32, Vec<u16>)> = self.granules.iter().map(|(g, bits)| -> FormatResult<(u32, Vec<u16>)> { Ok((*g, bits.rows()?)) }).collect::<FormatResult<_>>()?;
        let blocks: Vec<(u32, DvBlockKind, &[u16])> = ordinals
            .iter()
            .map(|(g, rows)| (*g, dv_block_kind_for(rows.len() as u32), rows.as_slice()))
            .collect();
        encode_dv(part_no, gen, &blocks)
    }
}

impl DvCompile {
    /// A compile seeded with an existing DV payload's rows (replace-not-stack, lcud `compact.rs`
    /// `seed_from_existing_dv`): what it encodes is then the part's complete deletion set. The caller validated the
    /// payload against the manifest triplet; its header must name `part_no`.
    pub fn seeded(payload: &[u8], part_no: u32) -> FormatResult<DvCompile> {
        let mut c = DvCompile::new();
        let mut rd = DvReader::open(payload)?;
        if rd.header.part_no != part_no {
            return Err(FormatError::Corrupt { at: "DV header part_no vs its part at compile" });
        }
        while let Some(block) = rd.next_block()? {
            match block.kind {
                DvBlockKind::List => {
                    for pair in block.payload.chunks_exact(2) {
                        let row = u16::from_le_bytes([pair[0], pair[1]]);
                        if u32::from(row) >= GRANULE_ROWS {
                            return Err(FormatError::Corrupt { at: "DV list row past the granule" });
                        }
                        c.mark(block.granule, row);
                    }
                }
                DvBlockKind::Bitmap => {
                    let g = c.granules.entry(block.granule).or_insert_with(GranuleBits::new);
                    for (w, chunk) in g.words.iter_mut().zip(block.payload.chunks_exact(8)) {
                        *w |= u64::from_le_bytes(chunk.try_into().expect("8 bytes"));
                    }
                    g.count = g.words.iter().map(|w| w.count_ones()).sum();
                }
            }
        }
        Ok(c)
    }
}

/// One part's replacement DV for a compile publishing generation `gen` (F2 L7): the part's `existing` DV payload,
/// if it has one, plus the `(granule, row)` deletions in `rows`, encoded with `dv_gen = gen`. `None` when `rows`
/// adds no row, so an empty (or already folded) set publishes nothing.
pub fn compile_part_dv(
    existing: Option<&[u8]>,
    part_no: u32,
    rows: &[(u32, u16)],
    gen: u64,
) -> FormatResult<Option<(Vec<u8>, DvTriplet)>> {
    let mut c = match existing {
        Some(payload) => DvCompile::seeded(payload, part_no)?,
        None => DvCompile::new(),
    };
    let before = c.deleted_rows();
    rows.iter().for_each(|&(g, r)| c.mark(g, r));
    if c.deleted_rows() == before {
        return Ok(None);
    }
    let payload = c.encode(part_no, gen)?;
    let triplet = DvTriplet::for_payload(gen, &payload);
    Ok(Some((payload, triplet)))
}

/// Why a rowstore→delta-part flush fires (DM-1 vocabulary; append-only —
/// the census names these causes when the build lane lands its witness
/// rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushTrigger {
    /// The buffered-row threshold (the DM-1 "flushes at threshold" clause;
    /// the constant is the build lane's, floor-mint discipline).
    RowThreshold,
    /// An explicit maintenance flush (VACUUM-class or the compaction rung
    /// draining the rowstore before a merge).
    Maintenance,
}

/// The publish-step vocabulary a single M5e write transaction may compose
/// (each variant rides the ONE existing §13.3 publish path —
/// `publish::publish_parts` for new generations; none of these is a new
/// commit protocol):
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DmlPublishStep {
    /// DM-1 flush: newly sealed delta parts join the manifest as ordinary
    /// `PartRecord`s in a new generation (part_no from `next_part_no`,
    /// rowid §10 order preserved by publish order).
    AppendDeltaParts {
        /// Sealed-part count entering the generation.
        part_count: u32,
    },
    /// DM-2 bump: one part's Dv triplet replaces its predecessor in a new
    /// manifest generation; the sidecar file for (part_no, dv, dv_gen) is
    /// durable BEFORE this generation is written (module-law ordering).
    DvBump {
        part_no: u32,
        triplet: DvTriplet,
    },
    /// DM-3 merge: base + delta chain + Dv retire; ONE fresh part (fresh
    /// elections, re-sealed exact facts, zero triplet) replaces them in a
    /// new generation. The FooterFacts session cache invalidates by the
    /// generation key (FT-8), never flush-the-world.
    MergeReplace {
        /// Parts leaving the manifest (base + its deltas).
        retired_parts: Vec<u32>,
        /// The replacement part's number.
        merged_part_no: u32,
    },
}

#[cfg(test)]
mod dml_scaffold_tests {
    use super::*;

    #[test]
    fn block_kind_crossover_is_wire_minimal() {
        // List payload = 2*count bytes; Bitmap = 1024. Strictly below 512
        // ordinals the list image is smaller, at 512 they tie and Bitmap
        // wins (constant-time application on the read side).
        assert_eq!(dv_block_kind_for(0), DvBlockKind::List);
        assert_eq!(dv_block_kind_for(511), DvBlockKind::List);
        assert_eq!(dv_block_kind_for(512), DvBlockKind::Bitmap);
        assert_eq!(dv_block_kind_for(GRANULE_ROWS), DvBlockKind::Bitmap);
    }

    #[test]
    fn compile_round_trips_at_format_grain() {
        // Producer → frozen-format reader round trip (the DvReader half;
        // the PartDeletes-grain twin — the true S3b consumer face — lives
        // in `pgrc_am::dml`'s scaffold tests, where both crates are
        // normal dependencies). Sparse granule elects List, dense elects
        // Bitmap; idempotent re-marks never skew the header fact.
        let mut c = DvCompile::new();
        c.mark(3, 7);
        c.mark(3, 7); // idempotent re-mark
        c.mark(3, 4090);
        for r in 0..600u16 {
            c.mark(11, r);
        }
        assert_eq!(c.deleted_rows(), 602);
        let payload = c.encode(42, 5).expect("encode");
        let triplet = DvTriplet::for_payload(5, &payload);
        assert_eq!(triplet.dv_len, payload.len() as u64);
        assert_eq!(triplet.dv_gen, 5);
        let mut rd = DvReader::open(&payload).expect("open");
        assert_eq!(rd.header.part_no, 42);
        assert_eq!(rd.header.gen, 5);
        assert_eq!(rd.header.deleted_rows, 602);
        let b0 = rd.next_block().expect("block").expect("some");
        assert_eq!(b0.granule, 3);
        assert_eq!(b0.kind, DvBlockKind::List);
        assert_eq!(b0.count, 2);
        let b1 = rd.next_block().expect("block").expect("some");
        assert_eq!(b1.granule, 11);
        assert_eq!(b1.kind, DvBlockKind::Bitmap);
        assert_eq!(b1.count, 600);
        assert!(rd.next_block().expect("end").is_none());
    }

    #[test]
    fn a_compile_replaces_the_existing_dv_never_stacks_on_it() {
        let mut old = DvCompile::new();
        old.mark(0, 5);
        for r in 0..600u16 {
            old.mark(2, r);
        }
        let old_payload = old.encode(7, 3).expect("encode");
        let (payload, t) = compile_part_dv(Some(&old_payload), 7, &[(0, 6), (2, 5), (4, 1)], 9).expect("compile").expect("new rows");
        assert_eq!((t.dv_gen, t.dv_len), (9, payload.len() as u64));
        let mut want = DvCompile::new();
        [(0, 5), (0, 6), (4, 1)].iter().for_each(|&(g, r)| want.mark(g, r));
        (0..600u16).for_each(|r| want.mark(2, r));
        assert_eq!(payload, want.encode(7, 9).expect("encode"), "the new DV is the whole set, at the new generation");
        let rd = DvReader::open(&payload).expect("open");
        assert_eq!((rd.header.gen, rd.header.deleted_rows), (9, 603));
        assert!(DvCompile::seeded(&old_payload, 8).is_err(), "a DV of another part");
    }

    #[test]
    fn a_list_row_past_the_granule_is_corrupt_not_a_panic() {
        let mut payload = encode_dv(7, 3, &[(0, DvBlockKind::List, &[1, 2])]).expect("encode");
        let (end, crc_at) = (payload.len() - 4, payload.len() - 6);
        payload[crc_at..end].copy_from_slice(&(GRANULE_ROWS as u16).to_le_bytes());
        let crc = crc32c(&payload[..end]);
        payload[end..].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(DvCompile::seeded(&payload, 7), Err(FormatError::Corrupt { at: "DV list row past the granule" })));
    }

    #[test]
    fn a_seed_re_encodes_byte_identical() {
        let mut old = DvCompile::new();
        [(0, 0), (0, (GRANULE_ROWS - 1) as u16), (9, 63), (9, 64)].iter().for_each(|&(g, r)| old.mark(g, r));
        (0..GRANULE_ROWS as u16).step_by(3).for_each(|r| old.mark(4, r));
        old.mark(4, (GRANULE_ROWS - 1) as u16);
        let payload = old.encode(7, 3).expect("encode");
        let seeded = DvCompile::seeded(&payload, 7).expect("seed");
        assert_eq!(seeded.deleted_rows(), old.deleted_rows());
        assert_eq!(seeded.encode(7, 3).expect("encode"), payload);
    }

    #[test]
    fn a_compile_that_adds_no_row_publishes_nothing() {
        assert!(compile_part_dv(None, 7, &[], 2).expect("compile").is_none());
        let mut old = DvCompile::new();
        old.mark(1, 1);
        let old_payload = old.encode(7, 3).expect("encode");
        assert!(compile_part_dv(Some(&old_payload), 7, &[], 4).expect("compile").is_none());
        assert!(compile_part_dv(Some(&old_payload), 7, &[(1, 1)], 4).expect("compile").is_none(), "already folded");
    }

    #[test]
    fn empty_compile_publishes_no_dv() {
        let c = DvCompile::new();
        assert!(c.is_empty());
        assert_eq!(c.deleted_rows(), 0);
    }
}
