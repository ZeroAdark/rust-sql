//! The footer fold as a mergeable accumulator, its per-part inputs from
//! either side (a part being sealed, a part on disk), the served values,
//! and the `facts-<gen>.pgrcf` file the publisher writes beside every
//! manifest generation (`pgrc_format::facts`; the user's ruling
//! 2026-09-13, gravity 1c pass 3 item 1).
//!
//! One fold, three callers. The PUBLISHER (`publish::publish_parts`) reads
//! the base generation's file, folds the sealed parts' captured sections
//! into it and writes the new generation's file before `CURRENT` moves —
//! visible iff the manifest is, retired with it. The READER
//! (`pgrc_am::footer`) decodes that file and serves it; a generation
//! without one (every bank published before this file existed) is walked
//! — the same fold over the parts' sections — and the walk's product is
//! written back by a writer context (the startup prime, ANALYZE), never at
//! plan time. A publish that RETIRES parts (the M5e `MergeReplace` step,
//! not yet a writer) cannot subtract register unions: it folds the
//! surviving parts from disk (`FactsAcc::fold_open_parts`) — the cost of
//! one walk over what it already rewrites. A delete-vector-only generation
//! re-keys the previous image unchanged (facts are ingest-time).
//!
//! Semantics are the footer fold's, unchanged: NDV = the union of every
//! part's registers, unknown once any part lacks the section; width and
//! max length from the part-grain Stats records under the
//! `STATSF_COMPUTED` witness, unknown once any part lacks it; a qual class
//! is served when the parts carrying its witness hold at least
//! [`QUAL_CLASS_VOTE_ROW_PERCENT`] of the rows; the codec is `dict` iff
//! every part elected `DictCodes`, else the storage class's; zone ranges
//! per granule in manifest part order for every byval-word or bool column
//! whose every part carries the exact key witness — a superset of the
//! word-keyed columns the planner is served (`ServedCol::zones` under the
//! reader's rule: the catalog's `MetaProfile::derive`, or
//! `pgrc_meta::profile::zone_word_class` when the storage width is known).
//! A part carries a stream's ENCODING width, not its storage width, so a
//! walk cannot tell an unsigned 4-byte word (`UnsignedWordSmall`, zoned)
//! from an unsigned 8-byte one (`UnsignedWordFlip`, not zoned): `width` in
//! the image is the storage width when the publisher wrote it (it has the
//! schema), 0 when a walk did.

use std::collections::BTreeMap;

use cost_profile::device::Device;
use pgrc_format::class::{CLASS_BOOL, CLASS_BYVAL, CLASS_F32, CLASS_F64, CLASS_FIXED};
use pgrc_format::enc::EncodingId;
use pgrc_format::facts::{ColFacts, FactsHeader, FactsImage, COLF_DICT_ALL, COLF_NDV_ABSENT, COLF_WIDTH_POISONED, COLF_ZONES};
use pgrc_format::manifest::Manifest;
use pgrc_format::meta::{stats_section_len, KeyKind, StatsRecord, STATSF_COMPUTED, STATS_RECORD_LEN};
use pgrc_format::part::{SectionKind, StreamRole, SECTIONF_META_ZSTD, STREAMF_SIGNED};
use pgrc_format::wire::Cur;
use pgrc_format::zonemap::ZoneMap;
use pgrc_format::{FormatError, FormatResult};
use pgrc_meta::ndv::Hll;
use pgrc_meta::profile::zone_word_class;
use pgrc_read::{OpenPart, ReadError, ReadResult};

use crate::seal::SealedPart;

/// Qual-class bits (gravity §3 #7): the part Stats record carries an
/// `Exact` key; an equality bloom section exists; the Values stream is
/// dictionary-coded.
pub const QUAL_CLASS_ZONE: u8 = 1;
pub const QUAL_CLASS_BLOOM: u8 = 2;
pub const QUAL_CLASS_DICT: u8 = 4;
/// A column has a qual class when the parts carrying it hold at least this
/// many per cent of the relation's rows (ruling 2026-09-13: a row-fraction
/// vote, so a load's tail part cannot veto the relation's class).
/// Keep (P5-27): a ruled share, not a size fitted to a corpus. The class only
/// orders the planner's quals (`costsize::pgrc_qual_cost_multiplier`); every
/// part evaluates a qual by its own sections whatever the vote says, so the
/// share moves an estimate, never an answer, and nothing about the data but
/// the carried row fraction decides it (`qual_class_vote_reads_only_the_row_fraction`).
pub const QUAL_CLASS_VOTE_ROW_PERCENT: u64 = 90;

/// Codec kinds (`ColCostFacts::codec`; mirrored as `types_pathnodes::
/// PGRC_CODEC_*`): unknown, a byval word stream, dictionary codes, a
/// by-reference varlena stream, a by-reference fixed-length stream.
pub const CODEC_UNKNOWN: u8 = 0;
pub const CODEC_WORD: u8 = 1;
pub const CODEC_DICT: u8 = 2;
pub const CODEC_TEXT: u8 = 3;
pub const CODEC_FIXED: u8 = 4;

/// The planner width convention's varlena header allowance (`VARHDRSZ`).
pub const VARLENA_HEADER_BYTES: u64 = 4;

/// The gap one part's meta sections coalesce across
/// (`OpenPart::section_bytes_batch`): the bytes a sequential read covers in
/// one read's fixed cost, in the device regime the sections' bytes are in
/// (`cost_profile::device`: one cachestat of their span, blended by the
/// cached share; cold when the part cannot tell). On the reference box a
/// cold walk coalesces across 334 KB and a warm one across 17 KB. The slack
/// read between two sections is never resident.
pub fn meta_coalesce_gap(part: &OpenPart, idxs: &[usize]) -> u64 {
    let sections = part.sections();
    let (lo, hi) = idxs
        .iter()
        .filter_map(|&i| sections.get(i))
        .fold((u64::MAX, 0u64), |(lo, hi), e| (lo.min(e.off), hi.max(e.off.saturating_add(e.len))));
    let resident = if hi > lo { part.page_resident(lo, hi - lo) } else { None };
    Device::at_residency(resident).coalesce_gap()
}

/// The classes whose zone ranges the image carries: the byval words (all
/// widths and signs — see the module doc on widths) and bool.
pub fn zone_candidate(class: u8) -> bool {
    class == CLASS_BYVAL || class == CLASS_BOOL
}

/// The prime's serving rule (no catalog): [`zone_word_class`] when the
/// storage width is known or not needed; `None` = ambiguous (an unsigned
/// byval word of unknown width), so a catalog holder must decide.
pub fn zone_rule_without_catalog(c: &ColFacts) -> Option<bool> {
    if c.class == CLASS_BYVAL && !c.signed && c.width == 0 {
        return None;
    }
    Some(zone_word_class(c.class, c.signed, c.width))
}

/// The codec kind a storage class implies before the election is known.
pub fn class_codec(class: u8) -> u8 {
    match class {
        CLASS_BYVAL | CLASS_F32 | CLASS_F64 | CLASS_BOOL => CODEC_WORD,
        CLASS_FIXED => CODEC_FIXED,
        _ => CODEC_TEXT,
    }
}

/// One part's fold inputs for one top-level column (path_ord 0).
#[derive(Debug, Clone, Copy)]
pub struct PartColFacts<'a> {
    pub attno: u32,
    pub class: u8,
    pub signed: bool,
    /// The storage word width (a publisher knows it from the schema); 0 =
    /// unknown (a walk: parts carry only the encoding width).
    pub width: u8,
    /// The UNWRAPPED Stats section body; `None` when the part has none.
    pub stats_body: Option<&'a [u8]>,
    /// The UNWRAPPED NdvRegisters section body; `None` when absent.
    pub ndv_body: Option<&'a [u8]>,
    pub bloom: bool,
    pub dict: bool,
    /// The DictPayload stream's value count when `dict`.
    pub ncodes: u64,
    /// Σ data-section bytes (Stream/PathTable) of the column in this part.
    pub disk_bytes: u64,
}

/// One part's fold inputs; `cols` attno-ascending.
#[derive(Debug, Clone)]
pub struct PartFacts<'a> {
    pub rows: u64,
    pub file_len: u64,
    pub granules: u32,
    pub bands: u32,
    pub cols: Vec<PartColFacts<'a>>,
}

/// The owned form read from a part on disk ([`part_facts_from_open`]).
#[derive(Debug, Clone)]
pub struct OwnedPartFacts {
    pub rows: u64,
    pub file_len: u64,
    pub granules: u32,
    pub bands: u32,
    pub cols: Vec<OwnedColFacts>,
}

#[derive(Debug, Clone)]
pub struct OwnedColFacts {
    pub attno: u32,
    pub class: u8,
    pub signed: bool,
    pub width: u8,
    pub stats_body: Option<Vec<u8>>,
    pub ndv_body: Option<Vec<u8>>,
    pub bloom: bool,
    pub dict: bool,
    pub ncodes: u64,
    pub disk_bytes: u64,
}

impl OwnedPartFacts {
    pub fn as_facts(&self) -> PartFacts<'_> {
        PartFacts {
            rows: self.rows,
            file_len: self.file_len,
            granules: self.granules,
            bands: self.bands,
            cols: self
                .cols
                .iter()
                .map(|c| PartColFacts {
                    attno: c.attno,
                    class: c.class,
                    signed: c.signed,
                    width: c.width,
                    stats_body: c.stats_body.as_deref(),
                    ndv_body: c.ndv_body.as_deref(),
                    bloom: c.bloom,
                    dict: c.dict,
                    ncodes: c.ncodes,
                    disk_bytes: c.disk_bytes,
                })
                .collect(),
        }
    }
}

fn unwrap_meta(flags: u16, raw: &[u8]) -> FormatResult<Vec<u8>> {
    if flags & SECTIONF_META_ZSTD != 0 {
        pgrc_codec::wrapper::meta_unwrap_body(raw)
    } else {
        Ok(pgrc_format::try_to_vec(raw, "facts")?)
    }
}

/// The walk's per-part inputs from an open part: every column the section
/// table names, its Stats and NdvRegisters bodies read in one coalesced
/// batch (Stats and the StreamDir stay resident for the scans that follow;
/// the registers are dropped with the result), the Values stream's class
/// and election from the stream directory, the bloom witness and the data
/// bytes from the section table.
pub fn part_facts_from_open(part: &OpenPart, file_len: u64) -> ReadResult<OwnedPartFacts> {
    let sections = part.sections();
    let mut attnos: Vec<u32> = sections.iter().filter(|e| e.attno != 0).map(|e| e.attno).collect();
    attnos.sort_unstable();
    attnos.dedup();
    let mut idxs: Vec<usize> = pgrc_format::try_vec(2 * attnos.len() + 1, "facts")?;
    idxs.extend(part.find_section(SectionKind::StreamDir, 0, 0));
    let mut stats_idx: Vec<Option<usize>> = pgrc_format::try_vec(attnos.len(), "facts")?;
    let mut ndv_idx: Vec<Option<usize>> = pgrc_format::try_vec(attnos.len(), "facts")?;
    for &attno in &attnos {
        let s = part.find_section(SectionKind::Stats, attno, 0);
        let n = part.find_section(SectionKind::NdvRegisters, attno, 0);
        idxs.extend(s);
        idxs.extend(n);
        stats_idx.push(s);
        ndv_idx.push(n);
    }
    let keep = |i: usize| sections[i].kind != SectionKind::NdvRegisters.as_u16();
    let raw = part.section_bytes_batch(&idxs, meta_coalesce_gap(part, &idxs), &keep)?;
    let mut body_of: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
    for (&i, seg) in idxs.iter().zip(&raw) {
        body_of.insert(i, unwrap_meta(sections[i].flags, seg.bytes()).map_err(ReadError::Format)?);
    }
    let dir = part.stream_directory()?;
    // D3038: the octet-length lanes' sections are no scan's reads of their column (`seal_part`'s disk bytes the same)
    let octet_ords: Vec<u32> = pgrc_read::read_path_table(part)?
        .map(|p| p.iter().enumerate().filter(|(_, s)| s.as_str() == pgrc_format::shredlane::OCTET_LENGTH_PATH).map(|(i, _)| i as u32 + 1).collect())
        .unwrap_or_default();
    let mut cols = pgrc_format::try_vec(attnos.len(), "facts")?;
    for (k, &attno) in attnos.iter().enumerate() {
        let values = dir.lookup(attno, 0, StreamRole::Values).map(|v| v.entry);
        let dict = values.is_some_and(|e| e.encoding == EncodingId::DictCodes.as_u16());
        let disk_bytes = sections
            .iter()
            .filter(|e| e.attno == attno && !octet_ords.contains(&e.path_ord) && matches!(SectionKind::from_u16(e.kind), Ok(SectionKind::StreamDir) | Ok(SectionKind::Stream) | Ok(SectionKind::PathTable)))
            .map(|e| e.len)
            .sum();
        cols.push(OwnedColFacts {
            attno,
            class: values.map_or(u8::MAX, |e| e.class),
            signed: values.is_some_and(|e| e.flags & STREAMF_SIGNED != 0),
            width: 0,
            stats_body: stats_idx[k].and_then(|i| body_of.remove(&i)),
            ndv_body: ndv_idx[k].and_then(|i| body_of.remove(&i)),
            bloom: part.find_section(SectionKind::Bloom, attno, 0).is_some(),
            dict,
            ncodes: if dict { dir.lookup(attno, 0, StreamRole::DictPayload).map_or(0, |d| d.entry.values) } else { 0 },
            disk_bytes,
        });
    }
    Ok(OwnedPartFacts { rows: part.rows(), file_len, granules: part.footer().granule_count, bands: part.footer().band_count, cols })
}

/// K1 L7: one column of a part as compaction's size estimate ([`cut_bytes`]) reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColSize {
    pub attno: u32,
    /// The Values stream's class (`class::CLASS_*`; `u8::MAX` without one) and, for `CLASS_FIXED`, its length.
    pub class: u8,
    pub fixed_len: u32,
    /// The part-grain Stats record (computed), for the heap classes (varlena, fixed).
    pub stats: Option<StatsRecord>,
}

impl ColSize {
    /// The writer buffers this column's values in its heap (`ColBuffer::heap`).
    pub fn heap(&self) -> bool {
        self.class == CLASS_FIXED || self.class == pgrc_format::class::CLASS_VARLENA
    }
}

/// K1 L7: the bulk writer's part-cut measure (`ColBuffer::approx_bytes`, which `PartCutPolicy::max_bytes` bounds)
/// estimated for a part of `rows` rows from its columns: per column a datum word and a validity bit a row; in the heap,
/// a varlena value's payload (`byte_len_sum`, exact) plus its 4-B header padded to 8 (3.5 B on average), a fixed-size
/// value its length padded to 8. A heap column without a Stats record counts no heap.
pub fn cut_bytes(rows: u64, cols: &[ColSize]) -> u64 {
    let n = cols.len() as u64;
    let words = rows * n * 8 + rows.div_ceil(64) * 8 * n;
    let heap: u64 = cols
        .iter()
        .map(|c| match c.stats {
            Some(r) if c.class == pgrc_format::class::CLASS_VARLENA => r.byte_len_sum + u64::from(r.nonnull) * 15 / 2,
            Some(r) if c.class == CLASS_FIXED => u64::from(r.nonnull) * u64::from(c.fixed_len).next_multiple_of(8),
            _ => 0,
        })
        .sum();
    words + heap
}

/// K1 L7: an open part's columns (attno-ascending, from its section table) with each one's Values class and fixed
/// length from the stream directory, without statistics.
pub fn part_columns(part: &OpenPart) -> ReadResult<Vec<ColSize>> {
    let mut attnos: Vec<u32> = part.sections().iter().filter(|e| e.attno != 0).map(|e| e.attno).collect();
    attnos.sort_unstable();
    attnos.dedup();
    let dir = part.stream_directory()?;
    Ok(attnos
        .into_iter()
        .map(|attno| {
            let v = dir.lookup(attno, 0, StreamRole::Values).map(|v| v.entry);
            ColSize { attno, class: v.map_or(u8::MAX, |e| e.class), fixed_len: v.map_or(0, |e| e.fixed_len), stats: None }
        })
        .collect())
}

/// K1 L7: [`part_columns`] with the heap columns' part-grain Stats records from the part's own sections, read in one
/// coalesced batch and kept resident (no NdvRegisters, unlike [`part_facts_from_open`]). Compaction's part-size
/// estimate when the bank stats plane does not cover the part.
pub fn part_heap_stats(part: &OpenPart) -> ReadResult<Vec<ColSize>> {
    let mut cols = part_columns(part)?;
    let at: Vec<(usize, usize)> =
        cols.iter().enumerate().filter(|(_, c)| c.heap()).filter_map(|(k, c)| part.find_section(SectionKind::Stats, c.attno, 0).map(|i| (k, i))).collect();
    let idxs: Vec<usize> = at.iter().map(|&(_, i)| i).collect();
    let raw = part.section_bytes_batch(&idxs, meta_coalesce_gap(part, &idxs), &|_| true)?;
    let sections = part.sections();
    let (granules, bands) = (part.footer().granule_count, part.footer().band_count);
    for (&(k, i), seg) in at.iter().zip(&raw) {
        let body = unwrap_meta(sections[i].flags, seg.bytes()).map_err(ReadError::Format)?;
        let rec = part_stats_record(&body, granules, bands, cols[k].attno).map_err(ReadError::Format)?;
        cols[k].stats = Some(rec).filter(|r| r.flags & STATSF_COMPUTED != 0);
    }
    Ok(cols)
}

/// The publisher's per-part inputs from a part being sealed: the seal's
/// captured slices (`SealedPart::{plane_slices, facts_slices}`, both
/// attno-ascending over the top-level columns).
pub fn part_facts_from_sealed(s: &SealedPart) -> FormatResult<PartFacts<'_>> {
    let mut cols = pgrc_format::try_vec(s.facts_slices.len(), "facts")?;
    for f in &s.facts_slices {
        let stats_body = s.plane_slices.iter().find(|p| p.attno == f.attno).map(|p| p.stats_body.as_slice());
        cols.push(PartColFacts {
            attno: f.attno,
            class: f.class,
            signed: f.signed,
            width: f.width,
            stats_body,
            ndv_body: f.ndv_body.as_deref(),
            bloom: f.bloom,
            dict: f.dict,
            ncodes: f.ncodes,
            disk_bytes: f.disk_bytes,
        });
    }
    Ok(PartFacts { rows: s.rows, file_len: s.file_len, granules: s.granule_count, bands: s.band_count, cols })
}

/// Slice + decode the ONE part-grain record at the tail of a Stats section
/// body (spec §8.1: granule records, then band records, then one part
/// record), checking the body's length against the geometry first.
pub fn part_stats_record(body: &[u8], granules: u32, bands: u32, attno: u32) -> FormatResult<StatsRecord> {
    let expect = stats_section_len(granules, bands);
    if body.len() != expect {
        let _ = attno;
        return Err(FormatError::Corrupt { at: "Stats section length vs geometry" });
    }
    let off = (granules as usize + bands as usize) * STATS_RECORD_LEN;
    let mut c = Cur::new(&body[off..]);
    StatsRecord::decode(&mut c)
}

/// One granule's served range: empty for an all-null granule, exact for an
/// `Exact` key, the full domain otherwise (never optimistic).
fn granule_range(r: &StatsRecord) -> (i64, i64) {
    if r.nonnull == 0 {
        (i64::MAX, i64::MIN)
    } else if KeyKind::from_u8(r.key_kind).ok() == Some(KeyKind::Exact) {
        (r.min_key, r.max_key)
    } else {
        (i64::MIN, i64::MAX)
    }
}

/// One column's running state.
#[derive(Debug, Clone)]
struct ColAcc {
    attno: u32,
    class: u8,
    signed: bool,
    width: u8,
    hll: Option<Hll>,
    ndv_absent: bool,
    byte_len_sum: u64,
    nonnull: u64,
    char_len_max: u32,
    width_poisoned: bool,
    disk_bytes: u64,
    rows_zone: u64,
    rows_bloom: u64,
    rows_dict: u64,
    dict_all: bool,
    ncodes: u64,
    /// `None` = dropped (not a word class, or a part lacked the witness).
    zones: Option<Vec<(i64, i64)>>,
}

impl ColAcc {
    /// A column first seen now, after `parts_before` parts that did not
    /// carry it: every "any part lacks it" law already applies.
    fn new(c: &PartColFacts<'_>, parts_before: u64) -> ColAcc {
        let seen_before = parts_before > 0;
        ColAcc {
            attno: c.attno,
            class: c.class,
            signed: c.signed,
            width: c.width,
            hll: None,
            ndv_absent: seen_before,
            byte_len_sum: 0,
            nonnull: 0,
            char_len_max: 0,
            width_poisoned: seen_before,
            disk_bytes: 0,
            rows_zone: 0,
            rows_bloom: 0,
            rows_dict: 0,
            dict_all: !seen_before,
            ncodes: 0,
            zones: if !seen_before && zone_candidate(c.class) { Some(Vec::new()) } else { None },
        }
    }

    /// The part does not carry the column at all.
    fn fold_missing(&mut self) {
        self.ndv_absent = true;
        self.hll = None;
        self.width_poisoned = true;
        self.byte_len_sum = 0;
        self.nonnull = 0;
        self.dict_all = false;
        self.zones = None;
    }

    fn fold(&mut self, c: &PartColFacts<'_>, rows: u64, granules: u32, bands: u32) -> FormatResult<()> {
        if (self.class, self.signed) != (c.class, c.signed) {
            return Err(FormatError::Corrupt { at: "facts: column class differs across parts" });
        }
        if self.width != 0 && c.width != 0 && self.width != c.width {
            return Err(FormatError::Corrupt { at: "facts: column width differs across parts" });
        }
        if self.width == 0 {
            self.width = c.width;
        }
        let mut exact = false;
        match c.stats_body {
            Some(body) => {
                let rec = part_stats_record(body, granules, bands, c.attno)?;
                exact = KeyKind::from_u8(rec.key_kind).ok() == Some(KeyKind::Exact);
                if rec.flags & STATSF_COMPUTED == 0 {
                    self.width_poisoned = true;
                    self.byte_len_sum = 0;
                    self.nonnull = 0;
                } else if !self.width_poisoned {
                    self.byte_len_sum += rec.byte_len_sum;
                    self.nonnull += u64::from(rec.nonnull);
                    self.char_len_max = self.char_len_max.max(rec.char_len_max);
                }
                if let Some(z) = self.zones.as_mut() {
                    if exact {
                        pgrc_format::try_reserve_more(z, granules as usize, "facts")?;
                        for g in 0..granules as usize {
                            let mut cur = Cur::new(&body[g * STATS_RECORD_LEN..(g + 1) * STATS_RECORD_LEN]);
                            z.push(granule_range(&StatsRecord::decode(&mut cur)?));
                        }
                    } else {
                        self.zones = None;
                    }
                }
            }
            None => {
                self.width_poisoned = true;
                self.byte_len_sum = 0;
                self.nonnull = 0;
                self.zones = None;
            }
        }
        match (c.ndv_body, self.ndv_absent) {
            (Some(body), false) => {
                let (_hdr, hll) = Hll::decode_section(body)?;
                match &mut self.hll {
                    Some(u) => {
                        if u.regs_len() != hll.regs_len() {
                            return Err(FormatError::Corrupt { at: "NdvRegisters register count differs across parts" });
                        }
                        u.merge(&hll);
                    }
                    None => self.hll = Some(hll),
                }
            }
            (None, _) => {
                self.ndv_absent = true;
                self.hll = None;
            }
            (Some(_), true) => {}
        }
        if exact {
            self.rows_zone = self.rows_zone.saturating_add(rows);
        }
        if c.bloom {
            self.rows_bloom = self.rows_bloom.saturating_add(rows);
        }
        if c.dict {
            self.rows_dict = self.rows_dict.saturating_add(rows);
            self.ncodes += c.ncodes;
        }
        self.dict_all &= c.dict;
        self.disk_bytes += c.disk_bytes;
        Ok(())
    }
}

impl ColAcc {
    /// The column as a fold that started after `parts_before` parts lacking it would hold it: the "any part lacks it"
    /// laws applied (`ColAcc::new` with parts before, then the folds).
    fn seen_late(mut self) -> ColAcc {
        self.ndv_absent = true;
        self.hll = None;
        self.width_poisoned = true;
        self.byte_len_sum = 0;
        self.nonnull = 0;
        self.dict_all = false;
        self.zones = None;
        self
    }

    /// Fold `o` (a later set of parts, all carrying the column) into `self`. Zones concatenate; the caller orders them.
    fn merge(&mut self, o: &ColAcc) -> FormatResult<()> {
        if (self.class, self.signed) != (o.class, o.signed) {
            return Err(FormatError::Corrupt { at: "facts: column class differs across parts" });
        }
        if self.width != 0 && o.width != 0 && self.width != o.width {
            return Err(FormatError::Corrupt { at: "facts: column width differs across parts" });
        }
        if self.width == 0 {
            self.width = o.width;
        }
        if self.ndv_absent || o.ndv_absent {
            self.ndv_absent = true;
            self.hll = None;
        } else if let (Some(u), Some(h)) = (self.hll.as_mut(), o.hll.as_ref()) {
            if u.regs_len() != h.regs_len() {
                return Err(FormatError::Corrupt { at: "NdvRegisters register count differs across parts" });
            }
            u.merge(h);
        } else if self.hll.is_none() {
            self.hll = o.hll.clone();
        }
        self.width_poisoned |= o.width_poisoned;
        if self.width_poisoned {
            self.byte_len_sum = 0;
            self.nonnull = 0;
        } else {
            self.byte_len_sum += o.byte_len_sum;
            self.nonnull += o.nonnull;
        }
        // Order-dependent in the sequential fold once poisoned; `serve` hides it (max_len 0 when poisoned).
        self.char_len_max = self.char_len_max.max(o.char_len_max);
        self.disk_bytes += o.disk_bytes;
        self.rows_zone = self.rows_zone.saturating_add(o.rows_zone);
        self.rows_bloom = self.rows_bloom.saturating_add(o.rows_bloom);
        self.rows_dict = self.rows_dict.saturating_add(o.rows_dict);
        self.dict_all &= o.dict_all;
        self.ncodes += o.ncodes;
        self.zones = match (self.zones.take(), &o.zones) {
            (Some(mut a), Some(b)) => {
                a.extend_from_slice(b);
                Some(a)
            }
            _ => None,
        };
        Ok(())
    }
}

/// The mergeable fold over a generation's parts.
#[derive(Debug, Clone, Default)]
pub struct FactsAcc {
    pub rows: u64,
    pub bytes: u64,
    pub granules: u64,
    pub parts: u64,
    cols: Vec<ColAcc>,
}

impl FactsAcc {
    pub fn new() -> FactsAcc {
        FactsAcc::default()
    }

    /// Resume from a persisted image (the previous generation's).
    pub fn from_image(img: &FactsImage) -> FormatResult<FactsAcc> {
        let mut cols = pgrc_format::try_vec(img.cols.len(), "facts")?;
        for c in &img.cols {
            let ndv_absent = c.flags & COLF_NDV_ABSENT != 0;
            let hll = if ndv_absent || c.ndv_body.is_empty() { None } else { Some(Hll::decode_section(&c.ndv_body)?.1) };
            cols.push(ColAcc {
                attno: c.attno,
                class: c.class,
                signed: c.signed,
                width: c.width,
                hll,
                ndv_absent,
                byte_len_sum: c.byte_len_sum,
                nonnull: c.nonnull,
                char_len_max: c.char_len_max,
                width_poisoned: c.flags & COLF_WIDTH_POISONED != 0,
                disk_bytes: c.disk_bytes,
                rows_zone: c.rows_zone,
                rows_bloom: c.rows_bloom,
                rows_dict: c.rows_dict,
                dict_all: c.flags & COLF_DICT_ALL != 0,
                ncodes: c.ncodes,
                zones: if c.flags & COLF_ZONES != 0 { Some(c.zones.decode()?) } else { None },
            });
        }
        Ok(FactsAcc { rows: img.header.rows, bytes: img.header.bytes, granules: img.header.granules, parts: img.header.parts, cols })
    }

    /// Fold one more part (manifest order).
    pub fn fold_part(&mut self, p: &PartFacts<'_>) -> FormatResult<()> {
        let parts_before = self.parts;
        let mut merged: Vec<ColAcc> = pgrc_format::try_vec(self.cols.len().max(p.cols.len()), "facts")?;
        let (mut i, mut j) = (0, 0);
        while i < self.cols.len() || j < p.cols.len() {
            let a = self.cols.get(i).map(|c| c.attno);
            let b = p.cols.get(j).map(|c| c.attno);
            match (a, b) {
                (Some(x), Some(y)) if x == y => {
                    let mut c = self.cols[i].clone();
                    c.fold(&p.cols[j], p.rows, p.granules, p.bands)?;
                    merged.push(c);
                    i += 1;
                    j += 1;
                }
                (Some(x), Some(y)) if x < y => {
                    let mut c = self.cols[i].clone();
                    c.fold_missing();
                    merged.push(c);
                    i += 1;
                }
                (Some(_), None) => {
                    let mut c = self.cols[i].clone();
                    c.fold_missing();
                    merged.push(c);
                    i += 1;
                }
                (_, Some(_)) => {
                    let mut c = ColAcc::new(&p.cols[j], parts_before);
                    c.fold(&p.cols[j], p.rows, p.granules, p.bands)?;
                    merged.push(c);
                    j += 1;
                }
                (None, None) => unreachable!(),
            }
        }
        self.cols = merged;
        self.rows += p.rows;
        self.bytes += p.file_len;
        self.granules += u64::from(p.granules);
        self.parts += 1;
        Ok(())
    }

    /// Fold parts on disk in the given order (the walk; a retiring publish).
    pub fn fold_open_parts<'a>(&mut self, parts: impl IntoIterator<Item = (&'a OpenPart, u64)>) -> ReadResult<()> {
        for (part, file_len) in parts {
            let owned = part_facts_from_open(part, file_len)?;
            self.fold_part(&owned.as_facts()).map_err(ReadError::Format)?;
        }
        Ok(())
    }

    /// Merge `o`, the accumulator of another disjoint set of parts, into `self`: the served values equal the
    /// sequential fold of both sets (`serve`-equal; the raw `char_len_max` of a poisoned column may differ). HLL
    /// registers merge by max, sums add, witnesses AND; a column one side lacks takes the "a part lacks it" laws. Zones
    /// are concatenated `self` then `o`: a caller merging parts that interleave in manifest order re-assembles them
    /// ([`FactsAcc::assemble_zones`]).
    pub fn merge(&mut self, o: &FactsAcc) -> FormatResult<()> {
        if o.parts == 0 {
            return Ok(());
        }
        if self.parts == 0 {
            *self = o.clone();
            return Ok(());
        }
        let mut merged: Vec<ColAcc> = pgrc_format::try_vec(self.cols.len().max(o.cols.len()), "facts")?;
        let (mut i, mut j) = (0, 0);
        while i < self.cols.len() || j < o.cols.len() {
            match (self.cols.get(i).map(|c| c.attno), o.cols.get(j).map(|c| c.attno)) {
                (Some(x), Some(y)) if x == y => {
                    let mut c = self.cols[i].clone();
                    c.merge(&o.cols[j])?;
                    merged.push(c);
                    i += 1;
                    j += 1;
                }
                (Some(x), Some(y)) if x < y => {
                    merged.push(self.cols[i].clone().seen_late());
                    i += 1;
                }
                (Some(_), None) => {
                    merged.push(self.cols[i].clone().seen_late());
                    i += 1;
                }
                (_, Some(_)) => {
                    merged.push(o.cols[j].clone().seen_late());
                    j += 1;
                }
                (None, None) => unreachable!(),
            }
        }
        self.cols = merged;
        self.rows += o.rows;
        self.bytes += o.bytes;
        self.granules += o.granules;
        self.parts += o.parts;
        Ok(())
    }

    /// Re-assemble every zoned column's per-granule ranges in manifest order: `slices` lists, for each part in that
    /// order, `(image index, first granule, granules)` in `images[image index]`'s own zone vector. Every image named
    /// must carry the column's zones (the merged accumulator only keeps zones every part had).
    pub fn assemble_zones(&mut self, images: &[&FactsImage], slices: &[(u32, u32, u32)]) -> FormatResult<()> {
        let total: u64 = slices.iter().map(|s| u64::from(s.2)).sum();
        for c in self.cols.iter_mut().filter(|c| c.zones.is_some()) {
            let mut decoded: Vec<Option<Vec<(i64, i64)>>> = pgrc_format::try_filled(images.len(), None, "facts")?;
            let mut out: Vec<(i64, i64)> = pgrc_format::try_vec(total as usize, "facts")?;
            for &(ix, first, n) in slices {
                let ix = ix as usize;
                if decoded[ix].is_none() {
                    let col = images[ix].cols.iter().find(|x| x.attno == c.attno && x.flags & COLF_ZONES != 0).ok_or(FormatError::Corrupt { at: "facts: zones absent from an assembled source" })?;
                    decoded[ix] = Some(col.zones.decode()?);
                }
                let z = decoded[ix].as_ref().expect("decoded");
                let range = z.get(first as usize..first as usize + n as usize).ok_or(FormatError::Corrupt { at: "facts: zone slice past its source" })?;
                out.extend_from_slice(range);
            }
            if out.len() as u64 != self.granules {
                return Err(FormatError::Corrupt { at: "facts: assembled zones vs granules" });
            }
            c.zones = Some(out);
        }
        Ok(())
    }

    /// The persisted form under a generation's identity.
    pub fn into_image(self, gen: u64, publisher_fxid: u64, schema_fingerprint: u64) -> FormatResult<FactsImage> {
        let cols = self
            .cols
            .into_iter()
            .map(|c| -> FormatResult<ColFacts> {
                let mut flags = 0u8;
                if c.ndv_absent {
                    flags |= COLF_NDV_ABSENT;
                }
                if c.width_poisoned {
                    flags |= COLF_WIDTH_POISONED;
                }
                if c.dict_all {
                    flags |= COLF_DICT_ALL;
                }
                let mut ndv_body = Vec::new();
                if let (Some(h), false) = (&c.hll, c.ndv_absent) {
                    h.encode_section(&mut ndv_body);
                }
                let zones = match c.zones {
                    Some(z) => {
                        flags |= COLF_ZONES;
                        ZoneMap::encode(&z)?
                    }
                    None => ZoneMap::default(),
                };
                Ok(ColFacts {
                    attno: c.attno,
                    class: c.class,
                    signed: c.signed,
                    width: c.width,
                    flags,
                    byte_len_sum: c.byte_len_sum,
                    nonnull: c.nonnull,
                    char_len_max: c.char_len_max,
                    disk_bytes: c.disk_bytes,
                    rows_zone: c.rows_zone,
                    rows_bloom: c.rows_bloom,
                    rows_dict: c.rows_dict,
                    ncodes: c.ncodes,
                    ndv_body,
                    zones,
                })
            })
            .collect::<FormatResult<Vec<ColFacts>>>()?;
        Ok(FactsImage {
            header: FactsHeader { gen, publisher_fxid, schema_fingerprint, rows: self.rows, bytes: self.bytes, granules: self.granules, parts: self.parts },
            cols,
        })
    }
}

/// One column as the planner sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedCol {
    pub attno: u32,
    pub ndv: u64,
    pub avg_width: i32,
    pub max_len: i32,
    pub disk_bytes: u64,
    pub qual_class: u8,
    pub codec: u8,
    pub ncodes: u64,
    /// NEW-stats-2: the column's non-null values summed over every part's
    /// part-grain Stats record (ingest-time counts, never sampled); `None`
    /// when a part lacks the column, its Stats section or the
    /// `STATSF_COMPUTED` witness (poisoned). A cost input only: it may lag
    /// the snapshot (deleted or in-flight rows).
    pub nonnull: Option<u64>,
    /// The compact per-granule map (`ZoneMap::decode` expands it).
    pub zones: Option<ZoneMap>,
}

/// The served values of an image (the fold's `finish`); `zone_rule` says
/// which columns' zone ranges the planner gets (the word-keyed ones).
pub fn serve(img: &FactsImage, zone_rule: &dyn Fn(&ColFacts) -> bool) -> FormatResult<Vec<ServedCol>> {
    let total = img.header.rows;
    let carried = |rows: u64| total > 0 && rows.saturating_mul(100) >= total.saturating_mul(QUAL_CLASS_VOTE_ROW_PERCENT);
    let mut out = pgrc_format::try_vec(img.cols.len(), "facts")?;
    for c in &img.cols {
        let ndv = if c.flags & COLF_NDV_ABSENT != 0 || c.ndv_body.is_empty() { 0 } else { u64::from(Hll::decode_section(&c.ndv_body)?.1.estimate()) };
        let poisoned = c.flags & COLF_WIDTH_POISONED != 0;
        let avg_width = if poisoned || c.nonnull == 0 || c.byte_len_sum == 0 {
            0
        } else {
            ((c.byte_len_sum + c.nonnull / 2) / c.nonnull + VARLENA_HEADER_BYTES).min(i32::MAX as u64) as i32
        };
        let max_len = if poisoned { 0 } else { i32::try_from(c.char_len_max).unwrap_or(i32::MAX) };
        let mut qual_class = 0u8;
        for (rows, bit) in [(c.rows_zone, QUAL_CLASS_ZONE), (c.rows_bloom, QUAL_CLASS_BLOOM), (c.rows_dict, QUAL_CLASS_DICT)] {
            if carried(rows) {
                qual_class |= bit;
            }
        }
        let dict = c.flags & COLF_DICT_ALL != 0;
        out.push(ServedCol {
            attno: c.attno,
            ndv,
            avg_width,
            max_len,
            disk_bytes: c.disk_bytes,
            qual_class,
            codec: if dict { CODEC_DICT } else { class_codec(c.class) },
            ncodes: if dict { c.ncodes } else { 0 },
            nonnull: (!poisoned).then_some(c.nonnull),
            zones: (c.flags & COLF_ZONES != 0 && zone_rule(c)).then(|| c.zones.clone()),
        });
    }
    Ok(out)
}

/// A base image carried to a generation with the same parts (a DV-only
/// generation): the header re-stamped, the accumulator unchanged.
pub fn restamp(img: &FactsImage, manifest: &Manifest) -> FactsImage {
    let mut out = img.clone();
    out.header.gen = manifest.header.gen;
    out.header.publisher_fxid = manifest.header.publisher_fxid;
    out.header.schema_fingerprint = manifest.header.schema_fingerprint;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgrc_format::class::{CLASS_VARLENA, CLASS_F64};
    use pgrc_format::facts::FactsImage;

    fn rec(min: i64, max: i64, nonnull: u32, kind: KeyKind, computed: bool, byte_len_sum: u64, char_len_max: u32) -> StatsRecord {
        let mut r = StatsRecord::absent();
        r.min_key = min;
        r.max_key = max;
        r.nonnull = nonnull;
        r.key_kind = kind.as_u8();
        r.byte_len_sum = byte_len_sum;
        r.char_len_max = char_len_max;
        if computed {
            r.flags |= STATSF_COMPUTED;
        }
        r
    }

    /// A Stats body: `granules` granule records, one band record, one part record.
    fn stats_body(granules: &[StatsRecord], part: &StatsRecord) -> Vec<u8> {
        let mut out = Vec::new();
        for g in granules {
            g.encode_into(&mut out);
        }
        part.encode_into(&mut out);
        part.encode_into(&mut out);
        assert_eq!(out.len(), stats_section_len(granules.len() as u32, 1));
        out
    }

    fn hll_body(values: impl Iterator<Item = u64>) -> Vec<u8> {
        let mut h = Hll::default();
        for v in values {
            let mut z = v.wrapping_add(0x9E37_79B9_7F4A_7C15);
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            h.observe_hash(z ^ (z >> 31));
        }
        let mut out = Vec::new();
        h.encode_section(&mut out);
        out
    }

    struct P {
        rows: u64,
        stats: Vec<u8>,
        ndv: Option<Vec<u8>>,
        bloom: bool,
        dict: bool,
        ncodes: u64,
        disk: u64,
    }

    fn word_part(p: &P, attno: u32) -> PartFacts<'_> {
        PartFacts {
            rows: p.rows,
            file_len: p.disk + 100,
            granules: 2,
            bands: 1,
            cols: vec![PartColFacts {
                attno,
                class: CLASS_BYVAL,
                signed: true,
                width: 8,
                stats_body: Some(&p.stats),
                ndv_body: p.ndv.as_deref(),
                bloom: p.bloom,
                dict: p.dict,
                ncodes: p.ncodes,
                disk_bytes: p.disk,
            }],
        }
    }

    fn two_granule_part(rows: u64, lo: i64, hi: i64, values: std::ops::Range<u64>, bloom: bool) -> P {
        let g = [rec(lo, (lo + hi) / 2, 5, KeyKind::Exact, true, 0, 0), rec((lo + hi) / 2, hi, 5, KeyKind::Exact, true, 0, 0)];
        P { rows, stats: stats_body(&g, &rec(lo, hi, 10, KeyKind::Exact, true, 0, 0)), ndv: Some(hll_body(values)), bloom, dict: false, ncodes: 0, disk: 1000 }
    }

    /// The catalog's rule as the prime would apply it (every width known here).
    fn word(c: &ColFacts) -> bool {
        zone_rule_without_catalog(c).expect("known width")
    }

    fn fold_all(parts: &[P]) -> FactsImage {
        let mut acc = FactsAcc::new();
        for p in parts {
            acc.fold_part(&word_part(p, 1)).expect("fold");
        }
        acc.into_image(9, 77, 0xfeed).unwrap()
    }

    #[test]
    fn merged_accumulators_serve_the_ordered_fold_and_zones_reassemble_in_part_order() {
        let parts = [
            two_granule_part(100, 0, 100, 0..500, true),
            two_granule_part(100, 50, 200, 250..900, true),
            two_granule_part(7, -5, 5, 0..7, false),
            two_granule_part(40, 300, 400, 900..950, true),
        ];
        let full = fold_all(&parts);
        let one = |p: &P| {
            let mut a = FactsAcc::new();
            a.fold_part(&word_part(p, 1)).unwrap();
            a.into_image(1, 1, 0xfeed).unwrap()
        };
        let imgs: Vec<FactsImage> = parts.iter().map(one).collect();
        // sets {0, 2} and {1, 3}: interleaved in part order, merged in any order
        let pair = |a: &FactsImage, b: &FactsImage| {
            let mut x = FactsAcc::from_image(a).unwrap();
            x.merge(&FactsAcc::from_image(b).unwrap()).unwrap();
            x.into_image(1, 1, 0xfeed).unwrap()
        };
        let even = pair(&imgs[0], &imgs[2]);
        let odd = pair(&imgs[1], &imgs[3]);
        for (a, b) in [(&odd, &even), (&even, &odd)] {
            let mut acc = FactsAcc::from_image(a).unwrap();
            acc.merge(&FactsAcc::from_image(b).unwrap()).unwrap();
            acc.merge(&FactsAcc::new()).unwrap();
            // part order: 0 (even 0..2), 1 (odd 0..2), 2 (even 2..4), 3 (odd 2..4)
            acc.assemble_zones(&[&even, &odd], &[(0, 0, 2), (1, 0, 2), (0, 2, 2), (1, 2, 2)]).unwrap();
            let got = acc.into_image(9, 77, 0xfeed).unwrap();
            assert_eq!(got.encode().unwrap(), full.encode().unwrap(), "byte-identical to the ordered fold here (no poisoned width)");
        }
        let mut short = FactsAcc::from_image(&even).unwrap();
        assert!(short.assemble_zones(&[&even], &[(0, 0, 2), (0, 2, 3)]).is_err(), "a slice past its source");
        // a column one side lacks: served as unknown, like the fold
        let mut other = FactsAcc::new();
        other.fold_part(&word_part(&parts[3], 2)).unwrap();
        let mut acc = FactsAcc::from_image(&imgs[0]).unwrap();
        acc.merge(&other).unwrap();
        let mut seq = FactsAcc::new();
        seq.fold_part(&word_part(&parts[0], 1)).unwrap();
        seq.fold_part(&word_part(&parts[3], 2)).unwrap();
        let (m, f) = (acc.into_image(1, 1, 1).unwrap(), seq.into_image(1, 1, 1).unwrap());
        assert_eq!(serve(&m, &word).unwrap(), serve(&f, &word).unwrap());
        let mut float = FactsAcc::new();
        let mut fp = word_part(&parts[1], 1);
        fp.cols[0].class = CLASS_F64;
        float.fold_part(&fp).unwrap();
        assert!(FactsAcc::from_image(&imgs[0]).unwrap().merge(&float).is_err(), "a class change refuses");
    }

    #[test]
    fn incremental_fold_through_the_image_equals_the_full_fold() {
        let parts = [two_granule_part(100, 0, 100, 0..500, true), two_granule_part(100, 50, 200, 250..900, true), two_granule_part(7, -5, 5, 0..7, false)];
        let full = fold_all(&parts);
        let mut acc = FactsAcc::new();
        acc.fold_part(&word_part(&parts[0], 1)).unwrap();
        let img1 = acc.into_image(1, 1, 0xfeed).unwrap();
        let mut resumed = FactsAcc::from_image(&FactsImage::decode(&img1.encode().unwrap()).unwrap()).unwrap();
        resumed.fold_part(&word_part(&parts[1], 1)).unwrap();
        let img2 = resumed.into_image(2, 2, 0xfeed).unwrap();
        let mut resumed = FactsAcc::from_image(&img2).unwrap();
        resumed.fold_part(&word_part(&parts[2], 1)).unwrap();
        assert_eq!(resumed.into_image(9, 77, 0xfeed).unwrap().encode().unwrap(), full.encode().unwrap(), "byte-identical image");
        let served = serve(&full, &word).unwrap();
        assert_eq!(served.len(), 1);
        let c = &served[0];
        assert_eq!(c.zones.as_ref().map(|z| z.decode().unwrap()).as_deref(), Some(&[(0, 50), (50, 100), (50, 125), (125, 200), (-5, 0), (0, 5)][..]), "granule ranges in part order");
        assert!(c.ndv >= 810 && c.ndv <= 990, "union estimate {} for 900 distinct", c.ndv);
        assert_eq!(full.header.rows, 207);
        assert_eq!(full.header.granules, 6);
        assert_eq!(full.header.parts, 3);
        assert_eq!(c.disk_bytes, 3000);
    }

    #[test]
    fn qual_classes_are_a_row_fraction_vote() {
        let parts = [two_granule_part(950, 0, 10, 0..10, true), two_granule_part(50, 0, 10, 0..10, false)];
        let c = &serve(&fold_all(&parts), &word).unwrap()[0];
        assert_eq!(c.qual_class & QUAL_CLASS_BLOOM, QUAL_CLASS_BLOOM, "95 % of the rows carry a bloom");
        assert_eq!(c.qual_class & QUAL_CLASS_ZONE, QUAL_CLASS_ZONE);
        let parts = [two_granule_part(500, 0, 10, 0..10, true), two_granule_part(500, 0, 10, 0..10, false)];
        assert_eq!(serve(&fold_all(&parts), &word).unwrap()[0].qual_class & QUAL_CLASS_BLOOM, 0, "half the rows is not a class");
    }

    /// P5-27's keep: the vote reads the carried share of the rows and nothing else. The share at the ruled bound
    /// carries at every scale, one row under it never does, and the part order, the part count and the values the
    /// parts hold change nothing.
    #[test]
    fn qual_class_vote_reads_only_the_row_fraction() {
        let bloom = |parts: &[P]| serve(&fold_all(parts), &word).unwrap()[0].qual_class & QUAL_CLASS_BLOOM != 0;
        for scale in [1u64, 10, 1000] {
            let at = QUAL_CLASS_VOTE_ROW_PERCENT * scale;
            let rest = (100 - QUAL_CLASS_VOTE_ROW_PERCENT) * scale;
            assert!(bloom(&[two_granule_part(at, 0, 10, 0..10, true), two_granule_part(rest, 0, 10, 0..10, false)]), "x{scale}: the bound carries");
            assert!(bloom(&[two_granule_part(rest, 5, 9, 0..3, false), two_granule_part(at, -7, 70, 0..500, true)]), "x{scale}: order and values");
            assert!(!bloom(&[two_granule_part(at - 1, 0, 10, 0..10, true), two_granule_part(rest + 1, 0, 10, 0..10, false)]), "x{scale}: under the bound");
        }
        let split: Vec<P> = (0..9).map(|_| two_granule_part(100, 0, 10, 0..10, true)).chain([two_granule_part(100, 0, 10, 0..10, false)]).collect();
        assert!(bloom(&split), "nine carrying parts of ten, equal rows");
    }

    #[test]
    fn ndv_absence_in_any_part_is_unknown_for_good_and_register_skew_refuses() {
        let mut parts = [two_granule_part(10, 0, 1, 0..100, false), two_granule_part(10, 0, 1, 0..100, false)];
        parts[1].ndv = None;
        assert_eq!(serve(&fold_all(&parts), &word).unwrap()[0].ndv, 0);
        let img = fold_all(&parts);
        assert_eq!(img.cols[0].flags & COLF_NDV_ABSENT, COLF_NDV_ABSENT);
        let mut resumed = FactsAcc::from_image(&img).unwrap();
        resumed.fold_part(&word_part(&two_granule_part(10, 0, 1, 0..100, false), 1)).unwrap();
        assert_eq!(serve(&resumed.into_image(3, 3, 3).unwrap(), &word).unwrap()[0].ndv, 0, "a later part cannot revive it");
        let mut skew = two_granule_part(10, 0, 1, 0..100, false);
        let mut short = Vec::new();
        short.extend_from_slice(&[1, 4, 0, 0, 16, 0, 0, 0]);
        short.extend_from_slice(&[0u8; 16]);
        skew.ndv = Some(short);
        let mut acc = FactsAcc::new();
        acc.fold_part(&word_part(&parts[0], 1)).unwrap();
        assert!(acc.fold_part(&word_part(&skew, 1)).is_err());
    }

    #[test]
    fn width_average_needs_the_witness_in_every_part() {
        let text = |rows: u64, sum: u64, max: u32, computed: bool| P {
            rows,
            stats: stats_body(&[rec(0, 0, rows as u32, KeyKind::Absent, computed, sum, max); 2], &rec(0, 0, rows as u32, KeyKind::Absent, computed, sum, max)),
            ndv: None,
            bloom: false,
            dict: false,
            ncodes: 0,
            disk: 10,
        };
        fn text_part(p: &P) -> PartFacts<'_> {
            let w = word_part(p, 2);
            PartFacts { cols: vec![PartColFacts { class: CLASS_VARLENA, signed: false, width: 0, ..w.cols[0] }], ..w }
        }
        let mut acc = FactsAcc::new();
        acc.fold_part(&text_part(&text(10, 100, 12, true))).unwrap();
        acc.fold_part(&text_part(&text(10, 300, 40, true))).unwrap();
        let c = &serve(&acc.clone().into_image(1, 1, 1).unwrap(), &word).unwrap()[0];
        assert_eq!(c.avg_width, 20 + 4, "(100 + 300) / 20 + VARHDRSZ");
        assert_eq!(c.max_len, 40);
        assert_eq!(c.codec, CODEC_TEXT);
        assert!(c.zones.is_none(), "a varlena column never carries zones");
        acc.fold_part(&text_part(&text(10, 0, 0, false))).unwrap();
        let c = &serve(&acc.into_image(2, 2, 2).unwrap(), &word).unwrap()[0];
        assert_eq!((c.avg_width, c.max_len), (0, 0), "the witness missing in one part poisons both");
        let mut acc = FactsAcc::new();
        acc.fold_part(&text_part(&text(10, 0, 0, true))).unwrap();
        assert_eq!(serve(&acc.into_image(1, 1, 1).unwrap(), &word).unwrap()[0].avg_width, 0, "zero sums read unknown");
    }

    #[test]
    fn zones_drop_without_the_exact_witness_and_never_exist_for_floats() {
        let mut coarse = two_granule_part(10, 0, 10, 0..10, false);
        coarse.stats = stats_body(&[rec(0, 5, 5, KeyKind::Coarse, true, 0, 0); 2], &rec(0, 10, 10, KeyKind::Coarse, true, 0, 0));
        let img = fold_all(&[two_granule_part(10, 0, 10, 0..10, false), coarse]);
        let c = &serve(&img, &word).unwrap()[0];
        assert!(c.zones.is_none());
        assert_eq!(c.qual_class & QUAL_CLASS_ZONE, 0, "10 of 20 rows carry the exact witness");
        let p = two_granule_part(10, 0, 10, 0..10, false);
        let mut acc = FactsAcc::new();
        acc.fold_part(&PartFacts { cols: vec![PartColFacts { class: CLASS_F64, signed: false, width: 8, ..word_part(&p, 3).cols[0] }], ..word_part(&p, 3) }).unwrap();
        let c = &serve(&acc.into_image(1, 1, 1).unwrap(), &word).unwrap()[0];
        assert!(c.zones.is_none());
        assert_eq!(c.codec, CODEC_WORD);
        let mut nulls = two_granule_part(10, 0, 10, 0..10, false);
        nulls.stats = stats_body(&[rec(0, 5, 5, KeyKind::Exact, true, 0, 0), rec(0, 0, 0, KeyKind::Exact, true, 0, 0)], &rec(0, 5, 5, KeyKind::Exact, true, 0, 0));
        assert_eq!(serve(&fold_all(&[nulls]), &word).unwrap()[0].zones.as_ref().map(|z| z.decode().unwrap()).as_deref(), Some(&[(0, 5), (i64::MAX, i64::MIN)][..]), "an all-null granule is the empty range");
    }

    #[test]
    fn the_dictionary_election_is_anded_and_codes_summed() {
        let mut a = two_granule_part(10, 0, 10, 0..10, false);
        a.dict = true;
        a.ncodes = 7;
        let mut b = two_granule_part(10, 0, 10, 0..10, false);
        b.dict = true;
        b.ncodes = 5;
        let c = &serve(&fold_all(&[a, b]), &word).unwrap()[0];
        assert_eq!((c.codec, c.ncodes), (CODEC_DICT, 12));
        assert_eq!(c.qual_class & QUAL_CLASS_DICT, QUAL_CLASS_DICT);
        let mut a = two_granule_part(10, 0, 10, 0..10, false);
        a.dict = true;
        a.ncodes = 7;
        let c = &serve(&fold_all(&[a, two_granule_part(10, 0, 10, 0..10, false)]), &word).unwrap()[0];
        assert_eq!((c.codec, c.ncodes), (CODEC_WORD, 0), "one plain part: the class codec, no codes");
    }

    #[test]
    fn a_walk_leaves_the_width_unknown_and_a_publisher_fills_it_in() {
        let a = two_granule_part(10, 0, 10, 0..10, false);
        let mut walked = word_part(&a, 1);
        walked.cols[0].signed = false;
        walked.cols[0].width = 0;
        let mut acc = FactsAcc::new();
        acc.fold_part(&walked).unwrap();
        let img = acc.clone().into_image(1, 1, 1).unwrap();
        assert_eq!(img.cols[0].flags & COLF_ZONES, COLF_ZONES, "zones are stored for every byval word");
        assert_eq!(zone_rule_without_catalog(&img.cols[0]), None, "an unsigned word of unknown width is ambiguous");
        let served = serve(&img, &|_| true).unwrap();
        assert!(served[0].zones.is_some(), "a catalog holder can still serve them");
        let mut published = word_part(&a, 1);
        published.cols[0].signed = false;
        published.cols[0].width = 4;
        acc.fold_part(&published).unwrap();
        let img = acc.into_image(2, 2, 2).unwrap();
        assert_eq!(img.cols[0].width, 4);
        assert_eq!(zone_rule_without_catalog(&img.cols[0]), Some(true), "oid-class: zoned");
        let mut acc = FactsAcc::new();
        acc.fold_part(&published).unwrap();
        let mut eight = published.clone();
        eight.cols[0].width = 8;
        assert!(acc.fold_part(&eight).is_err(), "two known widths disagree");
    }

    #[test]
    fn a_column_missing_from_a_part_is_unknown_and_a_class_change_refuses() {
        let a = two_granule_part(10, 0, 10, 0..10, true);
        let b = two_granule_part(10, 0, 10, 0..10, true);
        let mut acc = FactsAcc::new();
        acc.fold_part(&word_part(&a, 1)).unwrap();
        acc.fold_part(&PartFacts { cols: Vec::new(), ..word_part(&b, 1) }).unwrap();
        let c = &serve(&acc.into_image(1, 1, 1).unwrap(), &word).unwrap()[0];
        assert_eq!((c.ndv, c.avg_width, c.zones.is_none(), c.codec), (0, 0, true, CODEC_WORD));
        let mut acc = FactsAcc::new();
        acc.fold_part(&PartFacts { cols: Vec::new(), ..word_part(&a, 1) }).unwrap();
        acc.fold_part(&word_part(&b, 1)).unwrap();
        assert!(serve(&acc.into_image(1, 1, 1).unwrap(), &word).unwrap()[0].zones.is_none(), "first seen after a part without it");
        let mut acc = FactsAcc::new();
        acc.fold_part(&word_part(&a, 1)).unwrap();
        let mut other = word_part(&b, 1);
        other.cols[0].width = 4;
        assert!(acc.fold_part(&other).is_err());
    }
}
