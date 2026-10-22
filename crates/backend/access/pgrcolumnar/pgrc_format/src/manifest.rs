//! The part manifest + commit pointer (spec §13; rulings O-2/O-3): the
//! manifest generation REPLACES the single footer_off as the commit point.
//!
//! ## The publish ordering law (spec §13.3, WAL-first since delta v2 W0)
//!
//! 1. every new file's bytes are in WAL (`PGRC_FILE_WRITE`) before the publish;
//! 2. the `PGRC_PUBLISH` record (the manifest diff) is inserted, then the same
//!    `apply_publish` redo runs renames the parts and writes `manifest-<gen>` and
//!    `CURRENT`, all under `DELAY_CHKPT_START`;
//! 3. the checkpointer fsyncs the directory's written files;
//! 4. the commit record makes the generation effective.
//!
//! ## The clog fence (#254)
//!
//! `publisher_fxid` is epoch-qualified (64-bit) — xid recycling cannot alias
//! it. A generation is EFFECTIVE iff its publisher committed; readers and
//! recovery walk `prev_gen` past non-committed generations, so an aborted or
//! crashed-before-commit publish is structurally invisible. `CURRENT` is a
//! candidate hint + O(1) entry point, never the effectiveness authority.

use crate::rowid::{IDS_PER_BLOCK, MAX_GRANULES_PER_PART, ROWID_FIRST, ROWID_LIMIT};
use crate::wire::{crc32c, put_u32, put_u64, Cur};
use crate::{FormatError, FormatResult, FORMAT_VERSION};

/// Manifest magic: ASCII "PRCM".
pub const MANIFEST_MAGIC: u32 = u32::from_le_bytes(*b"PRCM");
/// Commit-pointer magic: ASCII "PRCC".
pub const CURRENT_MAGIC: u32 = u32::from_le_bytes(*b"PRCC");

pub const MANIFEST_HEADER_LEN: usize = 72;
/// `ManifestHeader::flags`: an extension block (`u32 len` + TLV records) follows the part records.
pub const MANIFESTF_EXT: u32 = 1;
/// `ManifestHeader::flags` (SEAL-OFFHEAD): the extension block holds an [`EXT_UNDER`] record. Set exactly when it does
/// ([`Manifest::with_ext`]), so a binary that predates it refuses the manifest ("ManifestHeader flags") instead of
/// reading another transaction's uncommitted parts as this generation's.
pub const MANIFESTF_UNDER: u32 = 2;
/// `ManifestHeader::flags` (FASTADD): the extension block holds an [`EXT_SCHEMAS`] record. Set exactly when it does
/// ([`Manifest::with_ext`]), so a binary that predates it refuses the manifest instead of opening the parts written
/// under an older schema against the header's fingerprint.
pub const MANIFESTF_SCHEMAS: u32 = 4;
/// `ManifestHeader::flags` (IX0): the extension block holds an [`EXT_RANGES`] or [`EXT_ROWID`] record, the manifest
/// names row IDs. Set exactly when it does ([`Manifest::with_ext`]), so a binary that predates IX0 refuses the
/// manifest (an unknown flag; its length still computes, so the end-of-recovery cleanup sees it intact and
/// undecodable and refuses the directory, rule (d)) instead of rewriting a table whose rows it cannot keep IDs for.
pub const MANIFESTF_ROWIDS: u32 = 8;
/// `ManifestHeader::flags` (ROWMAP2 M1): some part record names a [`PART_ROWMAP_GRANULES`] map. Set exactly when one
/// does ([`Manifest::with_ext`] derives it from the parts, the parts under an in-progress run included), so a binary
/// that predates ROWMAP2 refuses the manifest at open (an unknown flag: cleanup rule (d) then refuses the directory and
/// changes nothing) instead of serving scans and failing at the first map it loads. A table whose parts are all bulk
/// (implicit maps) or older forms keeps the bit clear and stays readable by such a binary.
pub const MANIFESTF_ROWMAP2: u32 = 16;
/// `ManifestHeader::flags` (SORTRUNS-E0): the extension block holds an [`EXT_RUNS`] record. Set exactly when it does
/// ([`Manifest::with_ext`]), so a binary that predates sorted runs refuses the manifest at open (an unknown flag:
/// cleanup rule (d) then refuses the directory and changes nothing) instead of finding no home for the IDs a run
/// homes, whose members have no [`EXT_RANGES`] entry.
pub const MANIFESTF_RUNS: u32 = 32;
pub const PART_RECORD_LEN: usize = 64;
/// `PartRecord::flags`: the part's RowIdentMap sidecar is authoritative (OC-5); set and absent is corruption.
pub const PART_FLAG_ROWMAP: u32 = 1;
/// `PartRecord::flags` bits 1..=2: the RowIdentMap sidecar's form (K1). 3 is granules (ROWMAP2 M1, `crate::rowmap`),
/// the only form. 0 sorted, 1 runs and 2 pairs were written before M1 and are retired (ROWMAP2 M2): a reader refuses
/// them, and a sidecar on an unflagged record (a seal before M1, which flagged none).
pub const PART_ROWMAP_FORM_SHIFT: u32 = 1;
pub const PART_ROWMAP_FORM_MASK: u32 = 0b11 << PART_ROWMAP_FORM_SHIFT;
pub const PART_ROWMAP_GRANULES: u32 = 3;

/// Whether `p` carries a [`PART_ROWMAP_GRANULES`] map (its flag and form).
pub fn granules_form(p: &PartRecord) -> bool {
    p.flags & PART_FLAG_ROWMAP != 0 && (p.flags & PART_ROWMAP_FORM_MASK) >> PART_ROWMAP_FORM_SHIFT == PART_ROWMAP_GRANULES
}
/// `PartRecord::flags`: a compaction output (an L1 part, K1): the policy never re-concatenates it.
pub const PART_FLAG_COMPACTED: u32 = 1 << 3;
pub const COMMIT_POINTER_LEN: usize = 32;

/// Manifest file header (72 B, spec §13.1). Wire order == declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct ManifestHeader {
    pub gen: u64,
    /// 0 = first generation (no predecessor).
    pub prev_gen: u64,
    /// FullTransactionId of the publisher — the clog fence (#254).
    pub publisher_fxid: u64,
    pub relfilenumber: u64,
    pub schema_fingerprint: u64,
    pub magic: u32,
    pub format_version: u32,
    pub spc: u32,
    pub db: u32,
    pub part_count: u32,
    /// The next part_no this table will assign (monotone; rowid §10 order).
    pub next_part_no: u32,
    pub flags: u32,
    pub reserved: u32,
}

/// Per-part manifest record (64 B, spec §13.1). `dv_gen == 0` ⇒ no delete
/// vector; the manifest is the AUTHORITATIVE reference for DVs (spec §15/§16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct PartRecord {
    pub rows: u64,
    pub file_len: u64,
    pub footer_off: u64,
    pub dv_gen: u64,
    pub dv_len: u64,
    pub part_no: u32,
    pub flags: u32,
    pub granule_count: u32,
    pub band_count: u32,
    pub dv_crc: u32,
    /// SB-10 grain echo (the A-lane manifest ripple, closed at M3-L2): the part's elected granule grain in rows. `0` =
    /// legacy default-grain record (v3-cut manifests): geometry echoes validate against the DEFAULT closed forms.
    /// Nonzero = a ladder grain: geometry echoes validate against the grain-parameterized closed forms and MATCH the
    /// part footer's `granule_rows` (the footer stays the grain truth; this field makes the manifest self-consistent
    /// instead of default-grain fiction for non-default parts).
    pub granule_rows: u32,
}

impl ManifestHeader {
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        put_u64(out, self.gen);
        put_u64(out, self.prev_gen);
        put_u64(out, self.publisher_fxid);
        put_u64(out, self.relfilenumber);
        put_u64(out, self.schema_fingerprint);
        put_u32(out, self.magic);
        put_u32(out, self.format_version);
        put_u32(out, self.spc);
        put_u32(out, self.db);
        put_u32(out, self.part_count);
        put_u32(out, self.next_part_no);
        put_u32(out, self.flags);
        put_u32(out, self.reserved);
    }

    pub fn decode_from(c: &mut Cur<'_>) -> FormatResult<ManifestHeader> {
        Ok(ManifestHeader {
            gen: c.u64("ManifestHeader")?,
            prev_gen: c.u64("ManifestHeader")?,
            publisher_fxid: c.u64("ManifestHeader")?,
            relfilenumber: c.u64("ManifestHeader")?,
            schema_fingerprint: c.u64("ManifestHeader")?,
            magic: c.u32("ManifestHeader")?,
            format_version: c.u32("ManifestHeader")?,
            spc: c.u32("ManifestHeader")?,
            db: c.u32("ManifestHeader")?,
            part_count: c.u32("ManifestHeader")?,
            next_part_no: c.u32("ManifestHeader")?,
            flags: c.u32("ManifestHeader")?,
            reserved: c.u32("ManifestHeader")?,
        })
    }
}

impl PartRecord {
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        put_u64(out, self.rows);
        put_u64(out, self.file_len);
        put_u64(out, self.footer_off);
        put_u64(out, self.dv_gen);
        put_u64(out, self.dv_len);
        put_u32(out, self.part_no);
        put_u32(out, self.flags);
        put_u32(out, self.granule_count);
        put_u32(out, self.band_count);
        put_u32(out, self.dv_crc);
        put_u32(out, self.granule_rows);
    }

    pub fn decode_from(c: &mut Cur<'_>) -> FormatResult<PartRecord> {
        Ok(PartRecord {
            rows: c.u64("PartRecord")?,
            file_len: c.u64("PartRecord")?,
            footer_off: c.u64("PartRecord")?,
            dv_gen: c.u64("PartRecord")?,
            dv_len: c.u64("PartRecord")?,
            part_no: c.u32("PartRecord")?,
            flags: c.u32("PartRecord")?,
            granule_count: c.u32("PartRecord")?,
            band_count: c.u32("PartRecord")?,
            dv_crc: c.u32("PartRecord")?,
            granule_rows: c.u32("PartRecord")?,
        })
    }
}

/// A decoded manifest generation. `ext` is the `MANIFESTF_EXT` block's TLV records (empty without the flag).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub header: ManifestHeader,
    pub parts: Vec<PartRecord>,
    pub ext: Vec<u8>,
}

/// One extension record: `u16 tag`, `u32 len`, `len` bytes.
pub fn put_ext_record(out: &mut Vec<u8>, tag: u16, bytes: &[u8]) {
    crate::wire::put_u16(out, tag);
    put_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

/// The extension block's records in order; a malformed block is `Corrupt`.
pub fn ext_records(ext: &[u8]) -> FormatResult<Vec<(u16, &[u8])>> {
    let mut c = Cur::new(ext);
    let mut out = Vec::new();
    while c.remaining() > 0 {
        let tag = c.u16("Manifest ext record")?;
        let len = c.u32("Manifest ext record")? as usize;
        out.push((tag, c.take(len, "Manifest ext record")?));
    }
    Ok(out)
}

/// Extension tag of the segment records (F1): the ring's segments, one record per slot in use.
pub const EXT_SEGMENTS: u16 = 1;
const SEGMENT_RECORD_LEN: usize = 20;
/// A ring slot's state; a free slot has no record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SegState {
    Active = 1,
    Sealing = 2,
    Sealed = 3,
}

/// One segment: its ring slot, the reserved `part_no` its rows are born under, the companion heap, its state and the
/// transaction that last changed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentRecord {
    pub slot: u16,
    pub part_no: u32,
    pub relid: u32,
    pub state: SegState,
    pub fxid: u64,
}

/// Append the `EXT_SEGMENTS` record for `recs` (sorted by slot) to an extension block.
pub fn encode_segments(recs: &[SegmentRecord], ext: &mut Vec<u8>) {
    let mut b = Vec::with_capacity(recs.len() * SEGMENT_RECORD_LEN);
    for r in recs {
        crate::wire::put_u16(&mut b, r.slot);
        crate::wire::put_u8(&mut b, r.state as u8);
        crate::wire::put_u8(&mut b, 0);
        put_u32(&mut b, r.part_no);
        put_u32(&mut b, r.relid);
        put_u64(&mut b, r.fxid);
    }
    put_ext_record(ext, EXT_SEGMENTS, &b);
}

/// The manifest's segment records, empty without the tag. Refuses duplicate or unsorted slots, an unknown state, two
/// `Active` records, and a `part_no` at or past `next_part_no`, or a live part's unless sealed into it.
pub fn segments(m: &Manifest) -> FormatResult<Vec<SegmentRecord>> {
    const AT: &str = "Manifest segments";
    let corrupt = || FormatError::Corrupt { at: AT };
    let mut out: Vec<SegmentRecord> = Vec::new();
    for (tag, bytes) in ext_records(&m.ext)? {
        if tag != EXT_SEGMENTS {
            continue;
        }
        if !out.is_empty() || bytes.len() % SEGMENT_RECORD_LEN != 0 {
            return Err(corrupt());
        }
        let mut c = Cur::new(bytes);
        while c.remaining() > 0 {
            let slot = c.u16(AT)?;
            let state = match (c.u8(AT)?, c.u8(AT)?) {
                (1, 0) => SegState::Active,
                (2, 0) => SegState::Sealing,
                (3, 0) => SegState::Sealed,
                _ => return Err(corrupt()),
            };
            let r = SegmentRecord { slot, state, part_no: c.u32(AT)?, relid: c.u32(AT)?, fxid: c.u64(AT)? };
            let live = r.state != SegState::Sealed && m.parts.iter().any(|p| p.part_no == r.part_no);
            if out.last().is_some_and(|l| l.slot >= slot) || r.part_no >= m.header.next_part_no || live {
                return Err(corrupt());
            }
            out.push(r);
        }
    }
    if out.iter().filter(|r| r.state == SegState::Active).count() > 1 {
        return Err(corrupt());
    }
    Ok(out)
}

/// Retired extension tag 2: K1's births table (`EXT_BIRTHS`), which [`EXT_RANGES`] replaced in IX0. Never given
/// another meaning: a reader skips it as an unknown tag.
pub const EXT_RETIRED_BIRTHS: u16 = 2;

/// Extension tag of the row-ID ranges table (IX0): per part, the `[lo, hi)` row-ID ranges whose rows it holds (a bulk
/// part's own aligned range; a sealed segment's extents; a compaction output's inputs' ranges). A range has one home.
/// A part with no entry holds no row ID (every part before IX0's writers).
pub const EXT_RANGES: u16 = 7;
/// Extension tag 8 is reserved (IX0 kept it for segment extents, which live outside the manifest, DV-1). Never given
/// a meaning.
pub const EXT_RESERVED_8: u16 = 8;
/// Extension tag of the table's row-ID floor (IX0): `u64 next_row_id`, at or above every ID any range of this manifest
/// names. Absent = 0, a table that has handed out no ID. The `rowid` file and `ROWID_RESERVE` are the counter; this is
/// its floor at publish, for recovery and `pgrc_rowid_stats`.
pub const EXT_ROWID: u16 = 9;
/// Extension tag of the sorted runs (SORTRUNS-E0, [`crate::sortrun`]): each run's members in key order and, for a run
/// with a reverse, the row-ID ranges it homes. Behind [`MANIFESTF_RUNS`].
pub const EXT_RUNS: u16 = 10;

/// The extension block's single record of `tag`, `None` without one; a second is `Corrupt`.
fn single_record<'a>(ext: &'a [u8], tag: u16, at: &'static str) -> FormatResult<Option<&'a [u8]>> {
    let mut out = None;
    for (t, bytes) in ext_records(ext)? {
        if t == tag && out.replace(bytes).is_some() {
            return Err(FormatError::Corrupt { at });
        }
    }
    Ok(out)
}

/// `ext` with every record of `tag` replaced by `body` (removed for `None`), every other record kept in order.
pub(crate) fn with_record(ext: &[u8], tag: u16, body: Option<&[u8]>) -> FormatResult<Vec<u8>> {
    let mut out = crate::try_vec(ext.len() + body.map_or(0, |b| b.len() + 6), "Manifest ext")?;
    for (t, bytes) in ext_records(ext)? {
        if t != tag {
            put_ext_record(&mut out, t, bytes);
        }
    }
    if let Some(b) = body {
        put_ext_record(&mut out, tag, b);
    }
    Ok(out)
}

/// The manifest's [`EXT_ROWID`] floor, 0 without the tag. Refuses a second record, a length other than 8, and a value
/// past [`crate::rowid::ROWID_LIMIT`].
pub fn next_row_id(m: &Manifest) -> FormatResult<u64> {
    const AT: &str = "Manifest next_row_id";
    match single_record(&m.ext, EXT_ROWID, AT)? {
        None => Ok(0),
        Some(b) => {
            let v = u64::from_le_bytes(b.try_into().map_err(|_| FormatError::Corrupt { at: AT })?);
            if v > ROWID_LIMIT {
                return Err(FormatError::Corrupt { at: AT });
            }
            Ok(v)
        }
    }
}

/// `ext` with its [`EXT_ROWID`] record set to `v` (removed for 0), every other record kept in order.
pub fn with_next_row_id(ext: &[u8], v: u64) -> FormatResult<Vec<u8>> {
    with_record(ext, EXT_ROWID, (v != 0).then(|| v.to_le_bytes()).as_ref().map(|b| &b[..]))
}

/// One part's row-ID ranges: sorted, disjoint, non-empty `[lo, hi)` pairs.
pub type IdRanges = Vec<(u64, u64)>;

/// Append the [`EXT_RANGES`] record for `r` (`(part_no, ranges)`, sorted by `part_no`) to an extension block:
/// `u32 count`, then per entry `u32 part_no, u32 n, n × (u64 lo, u64 hi)`.
pub fn try_encode_ranges(r: &[(u32, IdRanges)], ext: &mut Vec<u8>) -> FormatResult<()> {
    let mut out = crate::try_vec(4 + r.iter().map(|(_, l)| 8 + 16 * l.len()).sum::<usize>(), RANGES_AT)?;
    put_u32(&mut out, r.len() as u32);
    for (part_no, ranges) in r {
        put_range_entry(&mut out, *part_no, ranges);
    }
    put_ext_record(ext, EXT_RANGES, &out);
    Ok(())
}

#[cfg(test)]
pub fn encode_ranges(r: &[(u32, IdRanges)], ext: &mut Vec<u8>) {
    try_encode_ranges(r, ext).expect("encode_ranges");
}

fn put_range_entry(out: &mut Vec<u8>, part_no: u32, ranges: &[(u64, u64)]) {
    put_u32(out, part_no);
    put_u32(out, ranges.len() as u32);
    for &(lo, hi) in ranges {
        put_u64(out, lo);
        put_u64(out, hi);
    }
}

const RANGES_AT: &str = "Manifest ranges";

/// A walk over an [`EXT_RANGES`] body, entry by entry, with no list per entry: it refuses what [`range_entries`]
/// refuses, but an entry naming no part (its caller's check).
struct EntryWalk<'a> {
    c: Cur<'a>,
    left: u32,
    last: Option<u32>,
}

impl<'a> EntryWalk<'a> {
    fn new(body: &'a [u8]) -> FormatResult<EntryWalk<'a>> {
        let mut c = Cur::new(body);
        let left = c.u32(RANGES_AT)?;
        Ok(EntryWalk { c, left, last: None })
    }

    /// The byte offset of the next entry.
    fn at(&self) -> usize {
        self.c.off()
    }

    /// The next entry's part number, its ranges appended to `out`; `None` past the last.
    fn entry(&mut self, out: &mut Vec<(u64, u64)>) -> FormatResult<Option<u32>> {
        let corrupt = || FormatError::Corrupt { at: RANGES_AT };
        if self.left == 0 {
            return if self.c.remaining() == 0 { Ok(None) } else { Err(corrupt()) };
        }
        self.left -= 1;
        let part_no = self.c.u32(RANGES_AT)?;
        let k = self.c.u32(RANGES_AT)? as usize;
        if k == 0 || k > self.c.remaining() / 16 {
            return Err(corrupt());
        }
        let mut ok = self.last.is_none_or(|l| l < part_no);
        let mut prev_hi = 0;
        crate::try_reserve_more(out, k, RANGES_AT)?;
        for _ in 0..k {
            let (lo, hi) = (self.c.u64(RANGES_AT)?, self.c.u64(RANGES_AT)?);
            ok &= prev_hi <= lo && lo < hi && hi <= ROWID_LIMIT;
            prev_hi = hi;
            out.push((lo, hi));
        }
        if !ok {
            return Err(corrupt());
        }
        self.last = Some(part_no);
        Ok(Some(part_no))
    }
}

/// The [`EXT_RANGES`] entries as stored, empty without the tag. Refuses a second record, entries not sorted by
/// `part_no`, an entry naming no part of `m`, an empty list, an empty, unsorted or overlapping range, a range past
/// [`crate::rowid::ROWID_LIMIT`], and trailing bytes.
pub fn range_entries(m: &Manifest) -> FormatResult<Vec<(u32, IdRanges)>> {
    range_entries_in(&m.ext, &|no| m.parts.binary_search_by_key(&no, |p| p.part_no).is_ok())
}

/// [`range_entries`] of an extension block whose parts `known` names.
fn range_entries_in(ext: &[u8], known: &dyn Fn(u32) -> bool) -> FormatResult<Vec<(u32, IdRanges)>> {
    let Some(bytes) = single_record(ext, EXT_RANGES, RANGES_AT)? else {
        return Ok(Vec::new());
    };
    let mut walk = EntryWalk::new(bytes)?;
    let (mut out, mut ids) = (Vec::new(), Vec::new());
    while let Some(part_no) = walk.entry(&mut ids)? {
        if !known(part_no) {
            return Err(FormatError::Corrupt { at: RANGES_AT });
        }
        out.push((part_no, std::mem::take(&mut ids)));
    }
    Ok(out)
}

/// Every part's row-ID ranges in one list ([`ranges`] with no list per part): part `i` of the manifest homes
/// `ids[at[i]..at[i + 1]]`, in ID order (none without an [`EXT_RANGES`] entry).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FlatRanges {
    pub ids: Vec<(u64, u64)>,
    pub at: Vec<u32>,
}

impl FlatRanges {
    /// Part index `i`'s ranges.
    pub fn of(&self, i: usize) -> &[(u64, u64)] {
        &self.ids[self.at[i] as usize..self.at[i + 1] as usize]
    }
}

/// [`FlatRanges`] of `m`, with [`ranges`]'s checks and errors (`m.parts` ascending by `part_no`, as a decode checks).
pub fn flat_ranges(m: &Manifest) -> FormatResult<FlatRanges> {
    let mut at = crate::try_vec(m.parts.len() + 1, RANGES_AT)?;
    let mut ids = walk_flat(m, &mut |first| at.push(first))?;
    ids.shrink_to_fit();
    crate::try_resize(&mut at, m.parts.len() + 1, ids.len() as u32, RANGES_AT)?;
    Ok(FlatRanges { ids, at })
}

/// The [`EXT_RANGES`] ranges of `m` in entry order, each entry's part found by one merge walk of `m.parts`: `first`
/// gets, for each part up to the last entry's, the index of its first range.
fn walk_flat(m: &Manifest, first: &mut dyn FnMut(u32)) -> FormatResult<Vec<(u64, u64)>> {
    let Some(body) = single_record(&m.ext, EXT_RANGES, RANGES_AT)? else {
        return Ok(Vec::new());
    };
    let mut walk = EntryWalk::new(body)?;
    let mut ids = crate::try_vec(body.len() / 16, RANGES_AT)?;
    let mut j = 0;
    loop {
        let start = ids.len() as u32;
        let Some(part_no) = walk.entry(&mut ids)? else { break };
        while m.parts.get(j).is_some_and(|p| p.part_no < part_no) {
            first(start);
            j += 1;
        }
        if m.parts.get(j).is_none_or(|p| p.part_no != part_no) {
            return Err(FormatError::Corrupt { at: RANGES_AT });
        }
        first(start);
        j += 1;
    }
    Ok(ids)
}

/// Every part's row-ID ranges in manifest order: its [`EXT_RANGES`] entry, else none.
pub fn ranges(m: &Manifest) -> FormatResult<Vec<(u32, IdRanges)>> {
    let mut e = range_entries(m)?.into_iter().peekable();
    Ok(m.parts
        .iter()
        .map(|p| match e.peek() {
            Some((part_no, _)) if *part_no == p.part_no => e.next().expect("peeked"),
            _ => (p.part_no, Vec::new()),
        })
        .collect())
}

/// `ext` with its [`EXT_RANGES`] record replaced by `r` (none when `r` is empty), every other record kept in order.
pub fn with_ranges(ext: &[u8], r: &[(u32, IdRanges)]) -> FormatResult<Vec<u8>> {
    let mut b = Vec::new();
    if !r.is_empty() {
        try_encode_ranges(r, &mut b)?;
    }
    let body = ext_records(&b)?.first().map(|r| crate::try_to_vec(r.1, RANGES_AT)).transpose()?;
    with_record(ext, EXT_RANGES, body.as_deref())
}

/// The publish's edit of the row-ID records (IX0 L2): `ext` (the base's, as the plan edited it) keeps the
/// [`EXT_RANGES`] entries of the parts in `parts` (the generation's records, `part_no` ascending) that `add` does not
/// replace, and gains `add`'s non-empty entries (the parts the publish installs); [`EXT_ROWID`] rises to the highest
/// range end. A part retired drops its entry; a compaction output gets the union of its inputs' (the caller's `add`).
/// Refuses an `add` entry naming no part of `parts`.
///
/// IXCOPYPATH: one walk of the base's entries, no list per entry. When it keeps them all and every new entry's part
/// number is above the last one's (a publish that only installs: a small COPY's), the new entries are appended to the
/// stored ones as they are; otherwise the kept entries and the new ones are merged by part number. The bytes are
/// those of a full re-read, filter, sort and re-encode either way.
pub fn rebuild_ranges(ext: &[u8], parts: &[PartRecord], add: &[(u32, IdRanges)]) -> FormatResult<Vec<u8>> {
    let present = |no: u32| parts.binary_search_by_key(&no, |p| p.part_no).is_ok();
    if add.iter().any(|(no, _)| !present(*no)) {
        return Err(FormatError::EncodeContract { detail: "row-ID ranges for a part the generation does not list" });
    }
    let mut adds: Vec<&(u32, IdRanges)> = add.iter().collect();
    adds.sort_by_key(|a| a.0);
    let body = single_record(ext, EXT_RANGES, RANGES_AT)?;
    let base = base_entries(body, parts, &adds, None)?;
    let floor = single_record(ext, EXT_ROWID, "Manifest next_row_id")?.map_or(Ok(0), |b| b.try_into().map(u64::from_le_bytes).map_err(|_| FormatError::Corrupt { at: "Manifest next_row_id" }))?;
    let new: Vec<&(u32, IdRanges)> = adds.iter().copied().filter(|a| !a.1.is_empty()).collect();
    let new_len: usize = new.iter().map(|a| 8 + 16 * a.1.len()).sum();
    let new_top = new.iter().map(|a| a.1[a.1.len() - 1].1).max().unwrap_or(0);
    if base.all_kept && new.windows(2).all(|w| w[0].0 < w[1].0) && new.first().is_none_or(|a| base.last.is_none_or(|l| l < a.0)) {
        let stored = body.map_or(&[][..], |b| &b[4..]);
        let (floor, new_first) = append_floor(floor.max(base.top).max(new_top), floor);
        return emit_ranges(ext, base.count + new.len() as u32, stored.len() + new_len, floor, |out| {
            if !new_first {
                out.extend_from_slice(stored);
            }
            new.iter().for_each(|a| put_range_entry(out, a.0, &a.1));
            if new_first {
                out.extend_from_slice(stored);
            }
        });
    }
    let mut spans = Vec::new();
    let base = base_entries(body, parts, &adds, Some(&mut spans))?;
    let (body, kept_len) = (body.unwrap_or(&[]), spans.iter().map(|s| s.2 - s.1).sum::<usize>());
    emit_ranges(ext, (spans.len() + new.len()) as u32, kept_len + new_len, floor.max(base.top).max(new_top), |out| {
        let mut n = new.iter().peekable();
        for &(no, from, to) in &spans {
            while let Some(x) = n.next_if(|x| x.0 < no) {
                put_range_entry(out, x.0, &x.1);
            }
            out.extend_from_slice(&body[from..to]);
        }
        n.for_each(|x| put_range_entry(out, x.0, &x.1));
    })
}

/// One walk of a base's [`EXT_RANGES`] body for [`rebuild_ranges`].
struct BaseEntries {
    count: u32,
    /// Every entry's part is in `parts` and no `add` entry replaces it.
    all_kept: bool,
    last: Option<u32>,
    /// The highest range end of the kept entries.
    top: u64,
}

/// Walk `body` (none without the record) against `parts` and `adds` (both by part number, merged): `spans` gets each
/// kept entry's `(part_no, from, to)` byte span in `body`.
fn base_entries(body: Option<&[u8]>, parts: &[PartRecord], adds: &[&(u32, IdRanges)], mut spans: Option<&mut Vec<(u32, usize, usize)>>) -> FormatResult<BaseEntries> {
    let mut out = BaseEntries { count: 0, all_kept: true, last: None, top: 0 };
    let Some(body) = body else { return Ok(out) };
    let mut walk = EntryWalk::new(body)?;
    let (mut j, mut a, mut ids) = (0, 0, Vec::new());
    loop {
        let from = walk.at();
        ids.clear();
        let Some(no) = walk.entry(&mut ids)? else { break };
        while parts.get(j).is_some_and(|p| p.part_no < no) {
            j += 1;
        }
        while adds.get(a).is_some_and(|x| x.0 < no) {
            a += 1;
        }
        out.count += 1;
        out.last = Some(no);
        if parts.get(j).is_some_and(|p| p.part_no == no) && adds.get(a).is_none_or(|x| x.0 != no) {
            out.top = out.top.max(ids[ids.len() - 1].1);
            if let Some(s) = spans.as_deref_mut() {
                s.push((no, from, walk.at()));
            }
        } else {
            out.all_kept = false;
        }
    }
    Ok(out)
}

/// `ext`'s records but [`EXT_RANGES`] and [`EXT_ROWID`], in order, then an [`EXT_RANGES`] record of `count` entries
/// whose `len` bytes `entries` writes (none for no entry), then [`EXT_ROWID`] = `floor` (none for 0): the bytes
/// `with_next_row_id(&with_ranges(ext, table)?, floor)` writes.
fn emit_ranges(ext: &[u8], count: u32, len: usize, floor: u64, entries: impl FnOnce(&mut Vec<u8>)) -> FormatResult<Vec<u8>> {
    let records = ext_records(ext)?;
    let mut out = crate::try_vec(ext.len() + len + 24, RANGES_AT)?;
    for (tag, bytes) in records {
        if tag != EXT_RANGES && tag != EXT_ROWID {
            put_ext_record(&mut out, tag, bytes);
        }
    }
    if count > 0 {
        crate::wire::put_u16(&mut out, EXT_RANGES);
        put_u32(&mut out, 4 + len as u32);
        put_u32(&mut out, count);
        let at = out.len();
        entries(&mut out);
        debug_assert_eq!(out.len() - at, len);
    }
    if floor != 0 {
        put_ext_record(&mut out, EXT_ROWID, &floor.to_le_bytes());
    }
    Ok(out)
}

/// The appending rebuild's floor (`raised`, or `base` unraised) and whether its new entries go first: never, outside
/// the tests' knob (copypath_tests: the model check must catch either).
#[cfg(not(test))]
fn append_floor(raised: u64, _base: u64) -> (u64, bool) {
    (raised, false)
}

#[cfg(test)]
thread_local! {
    static APPEND_KNOB: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
    static APPENDS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn append_floor(raised: u64, base: u64) -> (u64, bool) {
    APPENDS.with(|n| n.set(n.get() + 1));
    match APPEND_KNOB.with(std::cell::Cell::get) {
        1 => (base, false),
        2 => (raised, true),
        _ => (raised, false),
    }
}

/// Whether an extension block names row IDs: an [`EXT_RANGES`] or [`EXT_ROWID`] record (header flag
/// [`MANIFESTF_ROWIDS`]).
pub fn names_row_ids(ext: &[u8]) -> bool {
    has_tag(ext, EXT_RANGES) || has_tag(ext, EXT_ROWID)
}

/// IX0's manifest checks, run by [`Manifest::decode`]: every range of every part is aligned to a virtual block,
/// pairwise disjoint and at or below [`next_row_id`] (the one-home law for IDs). IXCOPYPATH: one flat list of the
/// ranges, sorted only when the parts' IDs are out of part order (bulk parts take theirs in order). SORTRUNS-E0: the
/// ranges a run with a reverse homes ([`EXT_RUNS`]) join the parts' (one home: a part or a run).
pub fn check_row_ids(m: &Manifest) -> FormatResult<()> {
    let next = next_row_id(m)?;
    let mut all = walk_flat(m, &mut |_| {})?;
    if has_tag(&m.ext, EXT_RUNS) {
        crate::sortrun::homed_ranges(&m.ext, &mut all)?;
    }
    if !all.is_sorted() {
        all.sort_unstable();
    }
    if all.iter().any(|&(lo, hi)| lo < ROWID_FIRST || lo % IDS_PER_BLOCK != 0 || hi > next) || all.windows(2).any(|w| w[0].1 > w[1].0) {
        return Err(FormatError::Corrupt { at: "Manifest row-ID ranges: unaligned, past the floor or an ID in two homes" });
    }
    Ok(())
}

/// Extension tag of the tombstone heaps' state (TOMB): which of the table's two tombstone heaps takes new tombstones,
/// and the retired one, if any, with the transaction that folded it. The tags in use: 1 `EXT_SEGMENTS` (F1),
/// 2 retired (K1's births), 3 `EXT_STATS` (P-1), 4 `EXT_TOMBS` (TOMB).
pub const EXT_TOMBS: u16 = 4;
const TOMBS_RECORD_LEN: usize = 16;
/// The slots `EXT_TOMBS` names (`pg_toast.pgrc_tomb_<p>` is 0, `pg_toast.pgrc_tomb_<p>_1` is 1).
pub const TOMB_SLOTS: u8 = 2;

/// The tombstone heaps' state. No record reads as the default: slot 0 current, none retired (every table before TOMB).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TombsRecord {
    pub cur: u8,
    pub ret: Option<RetiredTombs>,
}

/// A retired tombstone heap: nothing new enters it; `folded` is the full xid of the transaction whose publish folded
/// its tombstones into the parts' deletion vectors (0 = not yet folded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetiredTombs {
    pub slot: u8,
    pub folded: u64,
}

/// `ext` with its `EXT_TOMBS` record replaced by `t` (none for the default), every other record kept in order:
/// `u8 cur, u8 ret (0xff = none), 6 zero bytes, u64 folded`.
pub fn with_tombs(ext: &[u8], t: &TombsRecord) -> FormatResult<Vec<u8>> {
    let mut out = crate::try_vec(ext.len() + 6 + TOMBS_RECORD_LEN, "Manifest tombs")?;
    for (tag, bytes) in ext_records(ext)? {
        if tag != EXT_TOMBS {
            put_ext_record(&mut out, tag, bytes);
        }
    }
    if *t != TombsRecord::default() {
        let mut b = Vec::with_capacity(TOMBS_RECORD_LEN);
        crate::wire::put_u8(&mut b, t.cur);
        crate::wire::put_u8(&mut b, t.ret.map_or(0xff, |r| r.slot));
        b.extend_from_slice(&[0; 6]);
        put_u64(&mut b, t.ret.map_or(0, |r| r.folded));
        put_ext_record(&mut out, EXT_TOMBS, &b);
    }
    Ok(out)
}

/// Extension tag of the parts under a generation (SEAL-OFFHEAD, pgrc-plans/SEAL-OFFHEAD.md): a background maintenance
/// publish (rotation, seal, a freed segment) may chain on a generation whose publisher is still in progress. Every part
/// of this manifest that such a transaction added carries its creator here, and a reader that does not see the
/// creator does not read the part. An untagged part was added by this generation's publisher, or by a transaction that
/// had committed when it published: whoever sees the publisher sees it. The tags in use: 1 `EXT_SEGMENTS`,
/// 2 retired, 3 `EXT_STATS`, 4 `EXT_TOMBS`, 5 `EXT_UNDER`, 6 `EXT_SCHEMAS`, 7 `EXT_RANGES`, 8 reserved,
/// 9 `EXT_ROWID` (IX0). A reader skips a tag it does not know. Header flag [`MANIFESTF_UNDER`].
pub const EXT_UNDER: u16 = 5;
const UNDER_ENTRY_LEN: usize = 16;

/// One `EXT_UNDER` entry: `part_no` was added by the (sub)transaction `fxid`, in progress when the manifest was
/// published.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnderEntry {
    pub part_no: u32,
    pub fxid: u64,
}

/// Whether the extension block holds a record of `tag` (an undecodable block holds none).
pub fn has_tag(ext: &[u8], tag: u16) -> bool {
    ext_records(ext).is_ok_and(|r| r.iter().any(|(t, _)| *t == tag))
}

/// The manifest's `EXT_UNDER` entries by `part_no`, empty without the tag. Refuses a second record, a length that is
/// not whole entries, entries not strictly ascending by `part_no`, a part not in `m`, a creator that is `m`'s own
/// publisher, and nonzero padding. Layout per entry: `u32 part_no, u32 0, u64 fxid`.
pub fn under(m: &Manifest) -> FormatResult<Vec<UnderEntry>> {
    const AT: &str = "Manifest under";
    let corrupt = || FormatError::Corrupt { at: AT };
    let mut out: Vec<UnderEntry> = Vec::new();
    let mut seen = false;
    for (tag, bytes) in ext_records(&m.ext)? {
        if tag != EXT_UNDER {
            continue;
        }
        if std::mem::replace(&mut seen, true) || bytes.is_empty() || bytes.len() % UNDER_ENTRY_LEN != 0 {
            return Err(corrupt());
        }
        let mut c = Cur::new(bytes);
        while c.remaining() > 0 {
            let part_no = c.u32(AT)?;
            if c.u32(AT)? != 0 {
                return Err(corrupt());
            }
            let fxid = c.u64(AT)?;
            if out.last().is_some_and(|l| l.part_no >= part_no) || fxid == m.header.publisher_fxid || m.parts.binary_search_by_key(&part_no, |p| p.part_no).is_err() {
                return Err(corrupt());
            }
            out.push(UnderEntry { part_no, fxid });
        }
    }
    Ok(out)
}

/// `ext` with its `EXT_UNDER` record replaced by `entries` (none when empty), every other record kept in order.
/// `entries` are sorted by `part_no` here.
pub fn with_under(ext: &[u8], entries: &[UnderEntry]) -> FormatResult<Vec<u8>> {
    let mut out = crate::try_vec(ext.len() + 6 + UNDER_ENTRY_LEN * entries.len(), "Manifest under")?;
    for (tag, bytes) in ext_records(ext)? {
        if tag != EXT_UNDER {
            put_ext_record(&mut out, tag, bytes);
        }
    }
    if !entries.is_empty() {
        let mut e = crate::try_to_vec(entries, "Manifest under")?;
        e.sort_unstable();
        let mut b = crate::try_vec(UNDER_ENTRY_LEN * e.len(), "Manifest under")?;
        for x in &e {
            put_u32(&mut b, x.part_no);
            put_u32(&mut b, 0);
            put_u64(&mut b, x.fxid);
        }
        put_ext_record(&mut out, EXT_UNDER, &b);
    }
    Ok(out)
}

/// Extension tag of the table's older schemas (FASTADD, pgrc-plans/FASTADD.done). An ALTER TABLE that only adds or
/// drops columns rewrites nothing: the parts written before it keep their columns, and the readers fill a column a
/// part lacks from the column's `attmissingval` (NULL without one). The header's `schema_fingerprint` is the table's
/// current schema, the one new parts are written with; this record lists every other schema some part of the manifest
/// was written with: its fingerprint, its columns (the fingerprint's inputs, so a reader can tell which of its columns a
/// part has and that their storage agrees; `semantics` is not stored and decodes `Opaque`) and the `part_no` runs of
/// the parts written with it. A part in no run was written with the header's schema. Every publish keeps it
/// ([`crate::manifest::with_schemas`] from the plan: a part new to the manifest takes the publisher's schema, a carried
/// one keeps its own). Header flag [`MANIFESTF_SCHEMAS`].
///
/// Layout: `u16 count`, then per schema `u64 fingerprint, u16 ncols, u32 nruns`, `ncols` columns of 16 bytes
/// (`u32 attno, u8 class id, u8 width, u8 signed, u8 collation class, u32 fixed_len, u16 typlen, u8 typbyval,
/// u8 typalign`), `nruns` runs of 8 (`u32 first part_no, u32 last part_no`, inclusive).
pub const EXT_SCHEMAS: u16 = 6;
const SCHEMA_COL_LEN: usize = 16;

/// One older schema of an [`EXT_SCHEMAS`] record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaEntry {
    pub fingerprint: u64,
    /// Ascending by attno; `semantics` is `Opaque` (not stored).
    pub cols: Vec<crate::class::ColSchema>,
    /// Inclusive `part_no` runs of the parts written with this schema, ascending.
    pub runs: Vec<(u32, u32)>,
}

impl SchemaEntry {
    /// Whether `part_no` falls in one of the runs.
    pub fn holds(&self, part_no: u32) -> bool {
        self.runs.binary_search_by(|&(lo, hi)| if hi < part_no { std::cmp::Ordering::Less } else if lo > part_no { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Equal }).is_ok()
    }
}

fn decode_schema_record(bytes: &[u8]) -> FormatResult<Vec<SchemaEntry>> {
    use crate::class::{ColSchema, CollationClass, StorageClass};
    const AT: &str = "Manifest schemas";
    let corrupt = || FormatError::Corrupt { at: AT };
    let mut c = Cur::new(bytes);
    let count = c.u16(AT)? as usize;
    let mut out = crate::try_vec(count, AT)?;
    for _ in 0..count {
        let fingerprint = c.u64(AT)?;
        let ncols = c.u16(AT)? as usize;
        let nruns = c.u32(AT)? as usize;
        if ncols == 0 || nruns == 0 || c.remaining() < ncols * SCHEMA_COL_LEN + nruns * 8 {
            return Err(corrupt());
        }
        let mut cols: Vec<ColSchema> = crate::try_vec(ncols, AT)?;
        for _ in 0..ncols {
            let attno = c.u32(AT)?;
            let (id, width, signed, coll) = (c.u8(AT)?, c.u8(AT)?, c.u8(AT)?, c.u8(AT)?);
            let fixed_len = c.u32(AT)?;
            let typlen = c.u16(AT)? as i16;
            let (byval, align) = (c.u8(AT)?, c.u8(AT)?);
            if signed > 1 || byval > 1 || attno == 0 || cols.last().is_some_and(|l| l.attno >= attno) {
                return Err(corrupt());
            }
            let class = StorageClass::from_parts(id, width, signed == 1, fixed_len)?;
            cols.push(ColSchema::opaque(attno, class, typlen, byval == 1, align, CollationClass::from_u8(coll)?));
        }
        if crate::ident::schema_fingerprint(&cols) != fingerprint {
            return Err(FormatError::Corrupt { at: "Manifest schemas fingerprint" });
        }
        let mut runs = crate::try_vec(nruns, AT)?;
        for _ in 0..nruns {
            let (lo, hi) = (c.u32(AT)?, c.u32(AT)?);
            if lo > hi || runs.last().is_some_and(|&(_, h): &(u32, u32)| h >= lo) {
                return Err(corrupt());
            }
            runs.push((lo, hi));
        }
        out.push(SchemaEntry { fingerprint, cols, runs });
    }
    if c.remaining() != 0 {
        return Err(corrupt());
    }
    Ok(out)
}

/// The older schemas an extension block describes (no check against a manifest's parts): what a reader learns about
/// fingerprints it may meet in a part's footer. Empty without the tag; a second record is `Corrupt`.
pub fn schema_entries(ext: &[u8]) -> FormatResult<Vec<SchemaEntry>> {
    let mut out = None;
    for (tag, bytes) in ext_records(ext)? {
        if tag != EXT_SCHEMAS {
            continue;
        }
        if out.is_some() {
            return Err(FormatError::Corrupt { at: "Manifest schemas" });
        }
        out = Some(decode_schema_record(bytes)?);
    }
    Ok(out.unwrap_or_default())
}

/// The manifest's [`EXT_SCHEMAS`] entries, checked against it: no entry is the header's schema, two entries never
/// share a fingerprint, and the runs of all entries are disjoint. (A run may span part numbers the manifest does not
/// list: a reader's view that hides some parts keeps the record as it is.)
pub fn schemas(m: &Manifest) -> FormatResult<Vec<SchemaEntry>> {
    let corrupt = || FormatError::Corrupt { at: "Manifest schemas vs parts" };
    let entries = schema_entries(&m.ext)?;
    let mut all: Vec<(u32, u32)> = Vec::new();
    for (k, e) in entries.iter().enumerate() {
        if e.fingerprint == m.header.schema_fingerprint || entries[..k].iter().any(|o| o.fingerprint == e.fingerprint) {
            return Err(corrupt());
        }
        all.extend_from_slice(&e.runs);
    }
    all.sort_unstable();
    if all.windows(2).any(|w| w[0].1 >= w[1].0) {
        return Err(corrupt());
    }
    Ok(entries)
}

/// The [`EXT_SCHEMAS`] entries of a manifest whose parts are `parts` (`(part_no, schema fingerprint)`, part_no
/// ascending) under the header fingerprint `header`: the manifest-consecutive parts of one schema other than `header`
/// form a run, and `cols` gives each listed schema's columns. `Err(fingerprint)` names a schema `cols` cannot describe.
pub fn entries_of(parts: impl IntoIterator<Item = (u32, u64)>, header: u64, cols: &mut dyn FnMut(u64) -> Option<Vec<crate::class::ColSchema>>) -> Result<Vec<SchemaEntry>, u64> {
    let mut runs: Vec<(u64, u32, u32)> = Vec::new();
    let mut cur: Option<(u64, u32, u32)> = None;
    for (part_no, f) in parts {
        match &mut cur {
            Some((cf, _, hi)) if *cf == f => *hi = part_no,
            _ => {
                runs.extend(cur.take().filter(|r| r.0 != header));
                cur = Some((f, part_no, part_no));
            }
        }
    }
    runs.extend(cur.take().filter(|r| r.0 != header));
    let mut entries: Vec<SchemaEntry> = Vec::new();
    for (f, lo, hi) in runs {
        match entries.iter_mut().find(|e| e.fingerprint == f) {
            Some(e) => e.runs.push((lo, hi)),
            None => entries.push(SchemaEntry { fingerprint: f, cols: cols(f).ok_or(f)?, runs: vec![(lo, hi)] }),
        }
    }
    Ok(entries)
}

/// Each part's schema fingerprint, in manifest order: its run's, else the header's.
pub fn part_fingerprints(m: &Manifest) -> FormatResult<Vec<u64>> {
    if m.header.flags & MANIFESTF_SCHEMAS == 0 {
        return crate::try_filled(m.parts.len(), m.header.schema_fingerprint, "Manifest part fingerprints");
    }
    let entries = schemas(m)?;
    Ok(m.parts.iter().map(|p| entries.iter().find(|e| e.holds(p.part_no)).map_or(m.header.schema_fingerprint, |e| e.fingerprint)).collect())
}

/// `part_no`'s schema fingerprint in `m` (its run's, else the header's).
pub fn part_fingerprint(m: &Manifest, part_no: u32) -> FormatResult<u64> {
    if m.header.flags & MANIFESTF_SCHEMAS == 0 {
        return Ok(m.header.schema_fingerprint);
    }
    Ok(schemas(m)?.iter().find(|e| e.holds(part_no)).map_or(m.header.schema_fingerprint, |e| e.fingerprint))
}

/// `ext` with its `EXT_SCHEMAS` record replaced by `entries` (removed when none), every other record kept in order.
pub fn with_schemas(ext: &[u8], entries: &[SchemaEntry]) -> FormatResult<Vec<u8>> {
    let mut out = crate::try_vec(ext.len() + 64, "Manifest schemas")?;
    for (tag, bytes) in ext_records(ext)? {
        if tag != EXT_SCHEMAS {
            put_ext_record(&mut out, tag, bytes);
        }
    }
    if !entries.is_empty() {
        let mut b = Vec::new();
        crate::wire::put_u16(&mut b, entries.len() as u16);
        for e in entries {
            put_u64(&mut b, e.fingerprint);
            crate::wire::put_u16(&mut b, e.cols.len() as u16);
            put_u32(&mut b, e.runs.len() as u32);
            for col in &e.cols {
                put_u32(&mut b, col.attno);
                crate::wire::put_u8(&mut b, col.class.id());
                crate::wire::put_u8(&mut b, col.class.width());
                crate::wire::put_u8(&mut b, col.class.signed() as u8);
                crate::wire::put_u8(&mut b, col.collation_class.as_u8());
                put_u32(&mut b, col.class.fixed_len());
                crate::wire::put_u16(&mut b, col.typlen as u16);
                crate::wire::put_u8(&mut b, col.typbyval as u8);
                crate::wire::put_u8(&mut b, col.typalign);
            }
            for &(lo, hi) in &e.runs {
                put_u32(&mut b, lo);
                put_u32(&mut b, hi);
            }
        }
        put_ext_record(&mut out, EXT_SCHEMAS, &b);
    }
    Ok(out)
}

/// Extension tag of the stats root (P-1): one `statsrun::RunRef` naming the run that carries the generation's run
/// list. Absent = a root-less generation (written before P-1, or with no runs yet). A publish that does not change the
/// run set carries the base's record unchanged.
pub const EXT_STATS: u16 = 3;

/// The manifest's stats root reference, `None` without the tag. Two records, or one that is not one `RunRef`, is
/// `Corrupt`.
pub fn stats_ref(m: &Manifest) -> FormatResult<Option<crate::statsrun::RunRef>> {
    let corrupt = || FormatError::Corrupt { at: "Manifest stats ref" };
    let mut out = None;
    for (tag, bytes) in ext_records(&m.ext)? {
        if tag != EXT_STATS {
            continue;
        }
        if out.is_some() || bytes.len() != crate::statsrun::RUN_REF_LEN {
            return Err(corrupt());
        }
        out = Some(crate::statsrun::RunRef::decode_from(&mut Cur::new(bytes))?);
    }
    Ok(out)
}

/// The `EXT_TOMBS` state, the default without the tag. Refuses a second record, a length other than 16, a slot at or
/// past [`TOMB_SLOTS`], a retired slot equal to the current one, nonzero padding, and a fold time without a retired heap.
pub fn tombs(ext: &[u8]) -> FormatResult<TombsRecord> {
    const AT: &str = "Manifest tombstone heaps";
    let corrupt = || FormatError::Corrupt { at: AT };
    let mut out = None;
    for (tag, bytes) in ext_records(ext)? {
        if tag != EXT_TOMBS {
            continue;
        }
        if out.is_some() || bytes.len() != TOMBS_RECORD_LEN {
            return Err(corrupt());
        }
        let mut c = Cur::new(bytes);
        let (cur, ret) = (c.u8(AT)?, c.u8(AT)?);
        for _ in 0..6 {
            if c.u8(AT)? != 0 {
                return Err(corrupt());
            }
        }
        let folded = c.u64(AT)?;
        let ret = match ret {
            0xff if folded == 0 => None,
            s if s != 0xff && s < TOMB_SLOTS && s != cur => Some(RetiredTombs { slot: s, folded }),
            _ => return Err(corrupt()),
        };
        if cur >= TOMB_SLOTS {
            return Err(corrupt());
        }
        out = Some(TombsRecord { cur, ret });
    }
    Ok(out.unwrap_or_default())
}

/// `ext` with its `EXT_STATS` record replaced by `r` (removed for `None`), every other record kept in order.
pub fn with_stats_ref(ext: &[u8], r: Option<&crate::statsrun::RunRef>) -> FormatResult<Vec<u8>> {
    let mut out = crate::try_vec(ext.len() + 6 + crate::statsrun::RUN_REF_LEN, "Manifest stats ref")?;
    for (tag, bytes) in ext_records(ext)? {
        if tag != EXT_STATS {
            put_ext_record(&mut out, tag, bytes);
        }
    }
    if let Some(r) = r {
        let mut b = Vec::with_capacity(crate::statsrun::RUN_REF_LEN);
        r.encode_into(&mut b);
        put_ext_record(&mut out, EXT_STATS, &b);
    }
    Ok(out)
}

impl Manifest {
    pub fn new(header: ManifestHeader, parts: Vec<PartRecord>) -> Manifest {
        Manifest { header, parts, ext: Vec::new() }
    }

    /// Attach an extension block; the header's `MANIFESTF_EXT` bit follows `ext` being non-empty, and its
    /// `MANIFESTF_UNDER` bit `ext` holding an [`EXT_UNDER`] record.
    pub fn with_ext(mut self, ext: Vec<u8>) -> Manifest {
        if ext.is_empty() {
            self.header.flags &= !MANIFESTF_EXT;
        } else {
            self.header.flags |= MANIFESTF_EXT;
        }
        if has_tag(&ext, EXT_UNDER) {
            self.header.flags |= MANIFESTF_UNDER;
        } else {
            self.header.flags &= !MANIFESTF_UNDER;
        }
        if has_tag(&ext, EXT_SCHEMAS) {
            self.header.flags |= MANIFESTF_SCHEMAS;
        } else {
            self.header.flags &= !MANIFESTF_SCHEMAS;
        }
        if names_row_ids(&ext) {
            self.header.flags |= MANIFESTF_ROWIDS;
        } else {
            self.header.flags &= !MANIFESTF_ROWIDS;
        }
        if self.parts.iter().any(granules_form) {
            self.header.flags |= MANIFESTF_ROWMAP2;
        } else {
            self.header.flags &= !MANIFESTF_ROWMAP2;
        }
        if has_tag(&ext, EXT_RUNS) {
            self.header.flags |= MANIFESTF_RUNS;
        } else {
            self.header.flags &= !MANIFESTF_RUNS;
        }
        self.ext = ext;
        self
    }

    /// Encode the full manifest file image (trailing crc32c included).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(MANIFEST_HEADER_LEN + self.parts.len() * PART_RECORD_LEN + 8 + self.ext.len());
        self.header.encode_into(&mut out);
        debug_assert_eq!(out.len(), MANIFEST_HEADER_LEN);
        for p in &self.parts {
            p.encode_into(&mut out);
        }
        if self.header.flags & MANIFESTF_EXT != 0 {
            put_u32(&mut out, self.ext.len() as u32);
            out.extend_from_slice(&self.ext);
        }
        let crc = crc32c(&out);
        put_u32(&mut out, crc);
        out
    }

    /// Decode + validate a manifest file image: magic, version, crc, part_count consistency, strictly-increasing
    /// part_no, part_no < next_part_no, geometry echoes.
    pub fn decode(bytes: &[u8]) -> FormatResult<Manifest> {
        if bytes.len() < MANIFEST_HEADER_LEN + 4 {
            return Err(FormatError::Truncated { at: "Manifest" });
        }
        let body = &bytes[..bytes.len() - 4];
        let stored_crc = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().expect("len 4"));
        if stored_crc != crc32c(body) {
            return Err(FormatError::CrcMismatch { at: "Manifest" });
        }
        let mut c = Cur::new(body);
        let header = ManifestHeader::decode_from(&mut c)?;
        if header.magic != MANIFEST_MAGIC {
            return Err(FormatError::BadMagic {
                at: "ManifestHeader",
            });
        }
        if header.format_version != FORMAT_VERSION {
            return Err(FormatError::BadVersion {
                at: "ManifestHeader",
                got: header.format_version,
            });
        }
        if header.gen == 0 || (header.prev_gen != 0 && header.prev_gen >= header.gen) {
            return Err(FormatError::Corrupt {
                at: "ManifestHeader gen chain",
            });
        }
        if header.flags & !(MANIFESTF_EXT | MANIFESTF_UNDER | MANIFESTF_SCHEMAS | MANIFESTF_ROWIDS | MANIFESTF_ROWMAP2 | MANIFESTF_RUNS) != 0
            || (header.flags & (MANIFESTF_UNDER | MANIFESTF_SCHEMAS | MANIFESTF_ROWIDS | MANIFESTF_RUNS) != 0 && header.flags & MANIFESTF_EXT == 0)
        {
            return Err(FormatError::Corrupt { at: "ManifestHeader flags" });
        }
        let parts_len = header.part_count as usize * PART_RECORD_LEN;
        let length_ok = if header.flags & MANIFESTF_EXT == 0 {
            c.remaining() == parts_len
        } else {
            c.remaining() >= parts_len + 4
        };
        if !length_ok {
            return Err(FormatError::Corrupt {
                at: "Manifest part_count vs length",
            });
        }
        let mut parts = crate::try_vec(header.part_count as usize, "Manifest")?;
        let mut prev_part_no: Option<u32> = None;
        for _ in 0..header.part_count {
            let p = PartRecord::decode_from(&mut c)?;
            if let Some(prev) = prev_part_no {
                if p.part_no <= prev {
                    return Err(FormatError::Corrupt {
                        at: "PartRecord part_no order",
                    });
                }
            }
            if p.part_no >= header.next_part_no {
                return Err(FormatError::Corrupt {
                    at: "PartRecord part_no >= next_part_no",
                });
            }
            // SB-10 grain-aware geometry echo: legacy records (0) validate
            // at the default grain; grain-stamped records at their own.
            let grain = if p.granule_rows == 0 {
                crate::geom::GranuleGrain::DEFAULT
            } else {
                crate::geom::GranuleGrain::from_rows(p.granule_rows).map_err(|_| {
                    FormatError::Corrupt {
                        at: "PartRecord granule_rows off the ladder",
                    }
                })?
            };
            // The granule ordinal is a 19-bit rowid field (rowid §10): a part may address at most MAX_GRANULES_PER_PART
            // granules regardless of grain. Enforcing this on the read side keeps an out-of-range granule_count from
            // surviving to pack time, where pack_rowid would overflow the granule field into the neighboring part's
            // rowid space. Grain-independent: the field width never moves.
            if p.granule_count > MAX_GRANULES_PER_PART {
                return Err(FormatError::Corrupt {
                    at: "PartRecord granule_count > MAX_GRANULES_PER_PART",
                });
            }
            // Compare in u64: the closed forms are untruncated (geom §), so a hostile manifest storing the mod-2^32
            // residue of an inconsistent (rows, granule_count/band_count) can no longer alias the true value (idx 253).
            // Widening the stored u32 also rejects any true count that does not fit u32 — band_count in particular is
            // not bounded by the MAX_GRANULES_PER_PART reject above.
            if (p.granule_count as u64) != crate::geom::granule_count_at(p.rows, grain)
                || (p.band_count as u64) != crate::geom::band_count_at(p.rows, grain)
            {
                return Err(FormatError::Corrupt {
                    at: "PartRecord geometry echo",
                });
            }
            if p.footer_off
                + crate::part::FOOTER_FIXED_LEN as u64
                + crate::part::PART_TAIL_LEN as u64
                > p.file_len
            {
                return Err(FormatError::Corrupt {
                    at: "PartRecord footer_off vs file_len",
                });
            }
            prev_part_no = Some(p.part_no);
            parts.push(p);
        }
        let ext = if header.flags & MANIFESTF_EXT != 0 {
            let len = c.u32("Manifest ext")? as usize;
            if c.remaining() != len {
                return Err(FormatError::Corrupt { at: "Manifest ext length" });
            }
            let ext = crate::try_to_vec(c.take(len, "Manifest ext")?, "Manifest ext")?;
            ext_records(&ext)?;
            ext
        } else {
            Vec::new()
        };
        let m = Manifest { header, parts, ext };
        if has_tag(&m.ext, EXT_UNDER) != (header.flags & MANIFESTF_UNDER != 0) {
            return Err(FormatError::Corrupt { at: "ManifestHeader flags vs EXT_UNDER" });
        }
        under(&m)?;
        if has_tag(&m.ext, EXT_SCHEMAS) != (header.flags & MANIFESTF_SCHEMAS != 0) {
            return Err(FormatError::Corrupt { at: "ManifestHeader flags vs EXT_SCHEMAS" });
        }
        schemas(&m)?;
        if names_row_ids(&m.ext) != (header.flags & MANIFESTF_ROWIDS != 0) {
            return Err(FormatError::Corrupt { at: "ManifestHeader flags vs EXT_RANGES / EXT_ROWID" });
        }
        if m.parts.iter().any(granules_form) != (header.flags & MANIFESTF_ROWMAP2 != 0) {
            return Err(FormatError::Corrupt { at: "ManifestHeader flags vs ROWMAP2 parts" });
        }
        if has_tag(&m.ext, EXT_RUNS) != (header.flags & MANIFESTF_RUNS != 0) {
            return Err(FormatError::Corrupt { at: "ManifestHeader flags vs EXT_RUNS" });
        }
        crate::sortrun::check(&m)?;
        check_row_ids(&m)?;
        Ok(m)
    }
}

/// The `CURRENT` commit-pointer record (32 B, spec §13.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct CommitPointer {
    pub gen: u64,
    pub manifest_len: u64,
    pub magic: u32,
    pub version: u32,
    pub manifest_crc: u32,
    pub crc: u32,
}

impl CommitPointer {
    pub fn new(gen: u64, manifest_len: u64, manifest_crc: u32) -> CommitPointer {
        CommitPointer {
            gen,
            manifest_len,
            magic: CURRENT_MAGIC,
            version: FORMAT_VERSION,
            manifest_crc,
            crc: 0,
        }
    }

    pub fn encode(&self) -> [u8; COMMIT_POINTER_LEN] {
        let mut out = Vec::with_capacity(COMMIT_POINTER_LEN);
        put_u64(&mut out, self.gen);
        put_u64(&mut out, self.manifest_len);
        put_u32(&mut out, self.magic);
        put_u32(&mut out, self.version);
        put_u32(&mut out, self.manifest_crc);
        let crc = crc32c(&out);
        put_u32(&mut out, crc);
        out.try_into().expect("commit pointer is 32 bytes")
    }

    pub fn decode(bytes: &[u8]) -> FormatResult<CommitPointer> {
        if bytes.len() < COMMIT_POINTER_LEN {
            return Err(FormatError::Truncated {
                at: "CommitPointer",
            });
        }
        let body = &bytes[..COMMIT_POINTER_LEN - 4];
        let mut c = Cur::new(bytes);
        let p = CommitPointer {
            gen: c.u64("CommitPointer")?,
            manifest_len: c.u64("CommitPointer")?,
            magic: c.u32("CommitPointer")?,
            version: c.u32("CommitPointer")?,
            manifest_crc: c.u32("CommitPointer")?,
            crc: c.u32("CommitPointer")?,
        };
        if p.magic != CURRENT_MAGIC {
            return Err(FormatError::BadMagic {
                at: "CommitPointer",
            });
        }
        if p.version != FORMAT_VERSION {
            return Err(FormatError::BadVersion {
                at: "CommitPointer",
                got: p.version,
            });
        }
        if p.crc != crc32c(body) {
            return Err(FormatError::CrcMismatch {
                at: "CommitPointer",
            });
        }
        Ok(p)
    }
}

#[cfg(test)]
mod ext_tests {
    use super::*;

    #[test]
    fn extension_block_round_trips_and_legacy_bytes_do_not_move() {
        let legacy = crate::tests::golden_sample_manifest();
        let bytes = legacy.encode();
        assert_eq!(Manifest::decode(&bytes).unwrap(), legacy);
        let mut ext = Vec::new();
        put_ext_record(&mut ext, 1, b"seg");
        put_ext_record(&mut ext, 0x7ff0, b"");
        let m = legacy.clone().with_ext(ext.clone());
        assert_eq!(m.header.flags, MANIFESTF_EXT);
        let enc = m.encode();
        assert_eq!(&enc[..MANIFEST_HEADER_LEN - 8], &bytes[..MANIFEST_HEADER_LEN - 8]);
        let back = Manifest::decode(&enc).unwrap();
        assert_eq!(back, m);
        assert_eq!(ext_records(&back.ext).unwrap(), vec![(1u16, &b"seg"[..]), (0x7ff0u16, &b""[..])]);
        assert_eq!(m.with_ext(Vec::new()), legacy);
    }

    #[test]
    fn unknown_flags_and_bad_extension_lengths_refuse() {
        let mut m = crate::tests::golden_sample_manifest();
        m.header.flags = 2;
        assert!(Manifest::decode(&m.encode()).is_err());
        let m = crate::tests::golden_sample_manifest().with_ext(vec![1, 0, 9, 0, 0, 0]);
        assert!(Manifest::decode(&m.encode()).is_err());
    }

    #[test]
    fn under_entries_round_trip_set_the_flag_and_refuse_bad_records() {
        let legacy = crate::tests::golden_sample_manifest();
        let (a, b) = (legacy.parts[0].part_no, legacy.parts[1].part_no);
        let e = [UnderEntry { part_no: b, fxid: 900 }, UnderEntry { part_no: a, fxid: 901 }];
        let m = legacy.clone().with_ext(with_under(&legacy.ext, &e).unwrap());
        assert_eq!(m.header.flags, MANIFESTF_EXT | MANIFESTF_UNDER);
        let back = Manifest::decode(&m.encode()).unwrap();
        assert_eq!(under(&back).unwrap(), vec![e[1], e[0]], "sorted by part_no");
        assert_eq!(under(&legacy).unwrap(), vec![]);
        // dropping the record drops the flag, and the manifest is byte-for-byte the one without it
        let gone = back.clone().with_ext(with_under(&back.ext, &[]).unwrap());
        assert_eq!(gone.encode(), legacy.encode());
        // a binary without SEAL-OFFHEAD sees an unknown flag: the flag without the record, or the record without the
        // flag, refuse as well
        let mut lying = m.clone();
        lying.header.flags &= !MANIFESTF_UNDER;
        assert!(Manifest::decode(&lying.encode()).is_err());
        let mut bare = legacy.clone();
        bare.header.flags |= MANIFESTF_UNDER;
        assert!(Manifest::decode(&bare.encode()).is_err());
        // a part the manifest does not hold, the publisher's own fxid
        let stray = legacy.clone().with_ext(with_under(&legacy.ext, &[UnderEntry { part_no: 999_999, fxid: 5 }]).unwrap());
        assert!(Manifest::decode(&stray.encode()).is_err());
        let own = legacy.clone().with_ext(with_under(&legacy.ext, &[UnderEntry { part_no: a, fxid: legacy.header.publisher_fxid }]).unwrap());
        assert!(Manifest::decode(&own.encode()).is_err());
    }

    /// FASTADD: the columns of a schema (int8 columns at `attnos`).
    fn cols(attnos: &[u32]) -> Vec<crate::class::ColSchema> {
        use crate::class::{ColSchema, CollationClass, StorageClass};
        attnos.iter().map(|&a| ColSchema::opaque(a, StorageClass::ByvalWord { width: 8, signed: true }, 8, true, b'd', CollationClass::C)).collect()
    }

    fn entry(attnos: &[u32], runs: &[(u32, u32)]) -> SchemaEntry {
        let cols = cols(attnos);
        SchemaEntry { fingerprint: crate::ident::schema_fingerprint(&cols), cols, runs: runs.to_vec() }
    }

    #[test]
    fn schema_records_round_trip_set_the_flag_and_name_each_part_s_schema() {
        let legacy = crate::tests::golden_sample_manifest();
        let header = legacy.header.schema_fingerprint;
        assert_eq!(part_fingerprints(&legacy).unwrap(), vec![header, header], "no record: every part has the header's");
        let old = entry(&[1, 2], &[(1, 1)]);
        let m = legacy.clone().with_ext(with_schemas(&legacy.ext, std::slice::from_ref(&old)).unwrap());
        assert_eq!(m.header.flags, MANIFESTF_EXT | MANIFESTF_SCHEMAS);
        let back = Manifest::decode(&m.encode()).unwrap();
        assert_eq!(back, m);
        assert_eq!(schemas(&back).unwrap(), vec![old.clone()]);
        assert_eq!(schema_entries(&back.ext).unwrap(), vec![old.clone()]);
        assert_eq!(part_fingerprints(&back).unwrap(), vec![old.fingerprint, header]);
        assert_eq!((part_fingerprint(&back, 1).unwrap(), part_fingerprint(&back, 3).unwrap()), (old.fingerprint, header));
        assert!(old.cols.iter().all(|c| c.semantics == crate::class::TypeSemantics::Opaque), "semantics are not stored");
        // two schemas, a run over part numbers the manifest does not list (a view that hides a part keeps it)
        let older = entry(&[1], &[(3, 3)]);
        let both = legacy.clone().with_ext(with_schemas(&legacy.ext, &[entry(&[1, 2], &[(0, 2)]), older.clone()]).unwrap());
        let back = Manifest::decode(&both.encode()).unwrap();
        assert_eq!(part_fingerprints(&back).unwrap(), vec![old.fingerprint, older.fingerprint]);
        // dropping the record drops the flag, and the manifest is byte-for-byte the one without it
        let gone = back.clone().with_ext(with_schemas(&back.ext, &[]).unwrap());
        assert_eq!(gone.encode(), legacy.encode());
        // other records are kept, in order, around it
        let mut ext = Vec::new();
        put_ext_record(&mut ext, 0x7ff0, b"other");
        let with_other = with_schemas(&ext, std::slice::from_ref(&old)).unwrap();
        let recs = ext_records(&with_other).unwrap();
        assert_eq!((recs.len(), recs[0]), (2, (0x7ff0u16, &b"other"[..])));
        assert_eq!(with_schemas(&with_other, &[]).unwrap(), ext);
    }

    #[test]
    fn schema_records_that_lie_refuse() {
        let legacy = crate::tests::golden_sample_manifest();
        let good = legacy.clone().with_ext(with_schemas(&legacy.ext, &[entry(&[1, 2], &[(1, 1)])]).unwrap());
        // a binary without FASTADD sees an unknown flag; the flag without the record, or the record without the flag
        let mut lying = good.clone();
        lying.header.flags &= !MANIFESTF_SCHEMAS;
        assert!(Manifest::decode(&lying.encode()).is_err());
        let mut bare = legacy.clone().with_ext(vec![9, 0, 0, 0, 0, 0]);
        bare.header.flags |= MANIFESTF_SCHEMAS;
        assert!(Manifest::decode(&bare.encode()).is_err());
        // columns that do not hash to the fingerprint
        let mut wrong = entry(&[1, 2], &[(1, 1)]);
        wrong.fingerprint ^= 1;
        let m = legacy.clone().with_ext(with_schemas(&legacy.ext, &[wrong]).unwrap());
        assert!(Manifest::decode(&m.encode()).is_err());
        // the header's own schema, the same schema twice, runs that overlap, a run backwards
        let mut own = entry(&[1, 2], &[(1, 1)]);
        let m = legacy.clone();
        let mut hdr = m.clone();
        hdr.header.schema_fingerprint = own.fingerprint;
        let hdr = hdr.with_ext(with_schemas(&legacy.ext, std::slice::from_ref(&own)).unwrap());
        assert!(Manifest::decode(&hdr.encode()).is_err());
        let twice = legacy.clone().with_ext(with_schemas(&legacy.ext, &[entry(&[1, 2], &[(1, 1)]), entry(&[1, 2], &[(3, 3)])]).unwrap());
        assert!(Manifest::decode(&twice.encode()).is_err());
        let overlap = legacy.clone().with_ext(with_schemas(&legacy.ext, &[entry(&[1, 2], &[(0, 2)]), entry(&[1], &[(1, 3)])]).unwrap());
        assert!(Manifest::decode(&overlap.encode()).is_err());
        own.runs = vec![(3, 1)];
        let back = legacy.clone().with_ext(with_schemas(&legacy.ext, &[own]).unwrap());
        assert!(Manifest::decode(&back.encode()).is_err());
        // columns out of order, no columns, trailing bytes, a second record
        let mut unsorted = entry(&[1, 2], &[(1, 1)]);
        unsorted.cols.reverse();
        unsorted.fingerprint = crate::ident::schema_fingerprint(&unsorted.cols);
        let m = legacy.clone().with_ext(with_schemas(&legacy.ext, &[unsorted]).unwrap());
        assert!(Manifest::decode(&m.encode()).is_err());
        let empty = SchemaEntry { fingerprint: crate::ident::schema_fingerprint(&[]), cols: Vec::new(), runs: vec![(1, 1)] };
        assert!(schema_entries(&with_schemas(&[], &[empty]).unwrap()).is_err());
        let one = with_schemas(&[], &[entry(&[1], &[(1, 1)])]).unwrap();
        let body = ext_records(&one).unwrap()[0].1.to_vec();
        let mut longer = Vec::new();
        put_ext_record(&mut longer, EXT_SCHEMAS, &[&body[..], &[0u8][..]].concat());
        assert!(schema_entries(&longer).is_err());
        let mut two = one.clone();
        put_ext_record(&mut two, EXT_SCHEMAS, &body);
        assert!(schema_entries(&two).is_err());
    }

    #[test]
    fn entries_of_runs_the_consecutive_parts_of_each_older_schema() {
        let (a, b) = (entry(&[1], &[]), entry(&[1, 2], &[]));
        let mut ask = |f: u64| [&a, &b].iter().find(|e| e.fingerprint == f).map(|e| e.cols.clone());
        let parts = [(0, a.fingerprint), (1, a.fingerprint), (2, 77), (4, a.fingerprint), (5, b.fingerprint), (9, b.fingerprint)];
        let got = entries_of(parts, 77, &mut ask).unwrap();
        assert_eq!(got, vec![SchemaEntry { runs: vec![(0, 1), (4, 4)], ..a.clone() }, SchemaEntry { runs: vec![(5, 9)], ..b.clone() }]);
        assert_eq!(entries_of([(0, 77), (1, 77)], 77, &mut ask).unwrap(), vec![], "every part the header's: no entry");
        assert_eq!(entries_of([(0, 5)], 77, &mut ask), Err(5), "an undescribed schema");
        assert!(!a.holds(0) && got[0].holds(4) && !got[0].holds(2) && got[1].holds(7));
    }

    fn seg(slot: u16, part_no: u32, state: SegState) -> SegmentRecord {
        SegmentRecord { slot, part_no, relid: 16400 + slot as u32, state, fxid: 700 + slot as u64 }
    }

    fn with_segments(recs: &[SegmentRecord]) -> Manifest {
        let mut m = crate::tests::golden_sample_manifest();
        m.parts.retain(|p| p.part_no != 900);
        m.header.next_part_no = 1000;
        let mut ext = Vec::new();
        put_ext_record(&mut ext, 0x7ff0, b"other");
        encode_segments(recs, &mut ext);
        m.with_ext(ext)
    }

    #[test]
    fn segment_records_round_trip_and_refuse_bad_rings() {
        assert_eq!(segments(&crate::tests::golden_sample_manifest()).unwrap(), vec![]);
        let recs = [seg(0, 900, SegState::Sealed), seg(1, 901, SegState::Sealing), seg(5, 905, SegState::Active)];
        let m = Manifest::decode(&with_segments(&recs).encode()).unwrap();
        assert_eq!(segments(&m).unwrap(), recs);
        let two_active = [seg(0, 900, SegState::Active), seg(1, 901, SegState::Active)];
        let dup = [seg(1, 900, SegState::Sealed), seg(1, 901, SegState::Active)];
        let unsorted = [seg(2, 900, SegState::Sealed), seg(1, 901, SegState::Active)];
        let past_next = [seg(0, 1000, SegState::Active)];
        for bad in [&two_active[..], &dup, &unsorted, &past_next] {
            assert!(segments(&with_segments(bad)).is_err(), "{bad:?}");
        }
        let live = crate::tests::golden_sample_manifest().parts[0].part_no;
        assert!(segments(&with_segments(&[seg(0, live, SegState::Active)])).is_err());
        assert!(segments(&with_segments(&[seg(0, live, SegState::Sealing)])).is_err());
        assert_eq!(segments(&with_segments(&[seg(0, live, SegState::Sealed)])).unwrap().len(), 1, "sealed into its part");
        let mut m = with_segments(&[seg(0, 900, SegState::Active)]);
        let n = m.ext.len();
        m.ext[n - SEGMENT_RECORD_LEN + 2] = 4;
        assert!(segments(&m).is_err(), "unknown state");
    }

    #[test]
    fn the_stats_ref_round_trips_beside_the_segments_and_refuses_duplicates() {
        use crate::statsrun::RunRef;
        let recs = [seg(0, 900, SegState::Sealed)];
        let base = with_segments(&recs);
        assert_eq!(stats_ref(&base).unwrap(), None);
        let r = RunRef { gen: 12, seq: 1, nparts: 16, len: 4096, crc: 0xdead_beef };
        let ext = with_stats_ref(&base.ext, Some(&r)).unwrap();
        let m = Manifest::decode(&base.clone().with_ext(ext.clone()).encode()).unwrap();
        assert_eq!(stats_ref(&m).unwrap(), Some(r));
        assert_eq!(segments(&m).unwrap(), recs, "the segment records are kept");
        assert_eq!(ext_records(&m.ext).unwrap()[0], (0x7ff0u16, &b"other"[..]), "other tags keep their order");
        let r2 = RunRef { gen: 13, ..r };
        let replaced = with_stats_ref(&ext, Some(&r2)).unwrap();
        assert_eq!(stats_ref(&base.clone().with_ext(replaced)).unwrap(), Some(r2));
        assert_eq!(with_stats_ref(&ext, None).unwrap(), base.ext, "removing it restores the block");
        let mut two = ext.clone();
        let mut b = Vec::new();
        r.encode_into(&mut b);
        put_ext_record(&mut two, EXT_STATS, &b);
        assert!(stats_ref(&base.clone().with_ext(two)).is_err(), "two stats records");
        let mut short = base.ext.clone();
        put_ext_record(&mut short, EXT_STATS, &b[..31]);
        assert!(stats_ref(&base.clone().with_ext(short)).is_err(), "a short stats record");
        assert!(with_stats_ref(&[1, 0, 9, 0, 0, 0], None).is_err(), "a malformed block");
    }
}

#[cfg(test)]
mod tombs_tests {
    use super::*;

    #[test]
    fn tombs_default_round_trip_keep_other_records_and_refuse_malformed() {
        let plain = crate::tests::golden_sample_manifest();
        assert_eq!(tombs(&plain.ext).unwrap(), TombsRecord::default(), "no record: slot 0 current, none retired");
        let mut ext = Vec::new();
        encode_segments(&[SegmentRecord { slot: 0, part_no: 2, relid: 16400, state: SegState::Sealed, fxid: 7 }], &mut ext);
        encode_ranges(&[(3, vec![(291, 582)])], &mut ext);
        let ext = with_next_row_id(&ext, 582).unwrap();
        let states = [
            TombsRecord { cur: 1, ret: None },
            TombsRecord { cur: 1, ret: Some(RetiredTombs { slot: 0, folded: 0 }) },
            TombsRecord { cur: 0, ret: Some(RetiredTombs { slot: 1, folded: 0x1_0000_0203 }) },
        ];
        for t in states {
            let with = with_tombs(&ext, &t).unwrap();
            let m = Manifest::decode(&plain.clone().with_ext(with.clone()).encode()).unwrap();
            assert_eq!(tombs(&m.ext).unwrap(), t);
            assert_eq!(ext_records(&m.ext).unwrap().iter().map(|r| r.0).collect::<Vec<_>>(), vec![EXT_SEGMENTS, EXT_RANGES, EXT_ROWID, EXT_TOMBS]);
            assert_eq!(range_entries(&m).unwrap(), vec![(3, vec![(291, 582)])], "ranges ride along");
            let again = with_tombs(&with, &TombsRecord { cur: 0, ret: None }).unwrap();
            assert_eq!(again, ext, "the default drops the record; the others stay in order");
            assert_eq!(with_ranges(&with, &[]).unwrap().len(), with.len() - ext_records(&ext).unwrap()[1].1.len() - 6, "a ranges edit keeps it");
            assert_eq!(tombs(&with_ranges(&with, &[]).unwrap()).unwrap(), t);
        }
        let rec = |cur: u8, ret: u8, pad: u8, folded: u64| {
            let mut b = vec![cur, ret, 0, 0, 0, pad, 0, 0];
            b.extend_from_slice(&folded.to_le_bytes());
            let mut e = Vec::new();
            put_ext_record(&mut e, EXT_TOMBS, &b);
            e
        };
        assert_eq!(tombs(&rec(1, 0, 0, 9)).unwrap(), TombsRecord { cur: 1, ret: Some(RetiredTombs { slot: 0, folded: 9 }) });
        for (why, e) in [
            ("a current slot past the two", rec(2, 0xff, 0, 0)),
            ("a retired slot past the two", rec(0, 2, 0, 0)),
            ("retired = current", rec(1, 1, 0, 0)),
            ("padding", rec(1, 0, 1, 0)),
            ("folded without a retired heap", rec(0, 0xff, 0, 5)),
        ] {
            assert!(tombs(&e).is_err(), "{why}");
        }
        let mut two = rec(1, 0, 0, 0);
        two.extend(rec(0, 1, 0, 0));
        assert!(tombs(&two).is_err(), "two records");
        let mut short = Vec::new();
        put_ext_record(&mut short, EXT_TOMBS, &[1, 0xff]);
        assert!(tombs(&short).is_err(), "short");
    }
}

#[cfg(test)]
mod rowid_tests {
    use super::*;
    use crate::rowid::{align_up, ROWID_LIMIT};

    /// The golden sample (parts 1 and 3) with the ranges table `r` (none when empty), the floor `next` (none for 0),
    /// and `extra` records.
    fn with(r: &[(u32, IdRanges)], next: u64, extra: &[u8]) -> Manifest {
        let mut m = crate::tests::golden_sample_manifest();
        m.header.next_part_no = 10;
        let mut ext = extra.to_vec();
        if !r.is_empty() {
            encode_ranges(r, &mut ext);
        }
        m.with_ext(with_next_row_id(&ext, next).unwrap())
    }

    fn decodes(m: &Manifest) -> bool {
        Manifest::decode(&m.encode()).is_ok()
    }

    /// `m`'s image with header flags `flags` and its trailing crc recomputed (an intact manifest a writer could not
    /// have produced).
    fn with_flags(m: &Manifest, flags: u32) -> Vec<u8> {
        let mut b = m.encode();
        b[64..68].copy_from_slice(&flags.to_le_bytes());
        let n = b.len();
        let crc = crc32c(&b[..n - 4]);
        b[n - 4..].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// q019g: IX0 L0 must not change the record layout an older binary reads (64 B parts, 20 B segments); IDs live in
    /// ext records only, behind a header flag older binaries refuse.
    #[test]
    fn the_ix0_tags_flag_and_unchanged_record_bytes_are_pinned() {
        assert_eq!((EXT_RANGES, EXT_RESERVED_8, EXT_ROWID), (7, 8, 9));
        assert_eq!(MANIFESTF_ROWIDS, 8);
        assert_eq!((PART_RECORD_LEN, SEGMENT_RECORD_LEN), (64, 20), "the layout an older binary reads");
        let plain = crate::tests::golden_sample_manifest();
        assert_eq!(plain.header.flags & MANIFESTF_ROWIDS, 0);
        let a = align_up(100_000);
        let m = with(&[(1, vec![(291, 291 + a)])], 1 << 20, &[]);
        assert_eq!(m.header.flags & (MANIFESTF_ROWIDS | MANIFESTF_EXT), MANIFESTF_ROWIDS | MANIFESTF_EXT, "set by with_ext");
        let enc = m.encode();
        assert_eq!(&enc[MANIFEST_HEADER_LEN..MANIFEST_HEADER_LEN + 2 * PART_RECORD_LEN], &plain.encode()[MANIFEST_HEADER_LEN..MANIFEST_HEADER_LEN + 2 * PART_RECORD_LEN], "part records unchanged");
        let back = Manifest::decode(&enc).unwrap();
        assert_eq!(back, m);
        assert_eq!(next_row_id(&back).unwrap(), 1 << 20);
        let recs = ext_records(&back.ext).unwrap();
        assert_eq!(recs.iter().map(|r| r.0).collect::<Vec<_>>(), vec![EXT_RANGES, EXT_ROWID]);
        assert_eq!(recs[1].1, &(1u64 << 20).to_le_bytes()[..], "EXT_ROWID is a bare u64");
        assert_eq!(m.with_ext(Vec::new()).header.flags & MANIFESTF_ROWIDS, 0, "cleared with the records");
        let floor_only = with(&[], 291, &[]);
        assert_ne!(floor_only.header.flags & MANIFESTF_ROWIDS, 0, "EXT_ROWID alone names IDs");
    }

    #[test]
    fn the_rowids_flag_matches_the_records() {
        let m = with(&[(3, vec![(291, 582)])], 582, &[]);
        assert!(Manifest::decode(&with_flags(&m, m.header.flags & !MANIFESTF_ROWIDS)).is_err(), "records without the flag");
        let plain = with(&[], 0, b"");
        let mut ext = Vec::new();
        put_ext_record(&mut ext, 0x7ff0, b"x");
        let other = plain.with_ext(ext);
        assert!(Manifest::decode(&other.encode()).is_ok());
        assert!(Manifest::decode(&with_flags(&other, other.header.flags | MANIFESTF_ROWIDS)).is_err(), "the flag without records");
        let bare = crate::tests::golden_sample_manifest();
        assert!(Manifest::decode(&with_flags(&bare, MANIFESTF_ROWIDS)).is_err(), "the flag without an extension block");
        assert!(Manifest::decode(&with_flags(&bare, 16)).is_err(), "an unknown flag is still refused");
    }

    #[test]
    fn next_row_id_defaults_to_zero_replaces_in_place_and_refuses_bad_records() {
        let plain = crate::tests::golden_sample_manifest();
        assert_eq!(next_row_id(&plain).unwrap(), 0);
        let mut ext = Vec::new();
        put_ext_record(&mut ext, 0x7ff0, b"kept");
        let one = with_next_row_id(&ext, 582).unwrap();
        let two = with_next_row_id(&one, 873).unwrap();
        assert_eq!(ext_records(&two).unwrap(), vec![(0x7ff0u16, &b"kept"[..]), (EXT_ROWID, &873u64.to_le_bytes()[..])]);
        assert_eq!(with_next_row_id(&two, 0).unwrap(), ext, "0 removes the record");
        let m = |ext: Vec<u8>| crate::tests::golden_sample_manifest().with_ext(ext);
        assert_eq!(next_row_id(&m(two.clone())).unwrap(), 873);
        let mut dup = two.clone();
        put_ext_record(&mut dup, EXT_ROWID, &5u64.to_le_bytes());
        assert!(next_row_id(&m(dup)).is_err(), "two records");
        let mut short = Vec::new();
        put_ext_record(&mut short, EXT_ROWID, &[1, 2, 3, 4]);
        assert!(next_row_id(&m(short)).is_err(), "a short record");
        let past = with_next_row_id(&[], ROWID_LIMIT + 1).unwrap();
        assert!(next_row_id(&m(past)).is_err(), "past the ID space");
        assert_eq!(next_row_id(&m(with_next_row_id(&[], ROWID_LIMIT).unwrap())).unwrap(), ROWID_LIMIT, "a full table");
    }

    #[test]
    fn ranges_round_trip_default_to_none_and_keep_one_home() {
        let a = align_up(100_000);
        assert!(decodes(&with(&[], 0, &[])), "no IDs at all");
        let (m20, m21) = (align_up(1 << 20), align_up(1 << 21));
        let table = vec![(1u32, vec![(291, 291 + a)]), (3, vec![(291 + a, 291 + a + 582), (m20, m20 + 291 * 3)])];
        let m = with(&table, 1 << 21, &[]);
        let back = Manifest::decode(&m.encode()).unwrap();
        assert_eq!(range_entries(&back).unwrap(), table);
        assert_eq!(ranges(&back).unwrap(), table);
        let one = with(&table[1..], 1 << 21, &[]);
        assert_eq!(ranges(&one).unwrap(), vec![(1, vec![]), table[1].clone()], "a part without an entry holds no ID");
        let cleared = with_ranges(&back.ext, &[]).unwrap();
        assert!(!has_tag(&cleared, EXT_RANGES));
        let again = with_ranges(&back.ext, &[(1, vec![(1 << 21, (1 << 21) + 291)])]).unwrap();
        assert_eq!(ext_records(&again).unwrap().iter().filter(|r| r.0 == EXT_RANGES).count(), 1, "replaced, not appended");
        let bad: [(&str, Vec<(u32, IdRanges)>, u64); 9] = [
            ("two parts own one ID", vec![(1, vec![(291, 291 + a)]), (3, vec![(582, 873)])], 1 << 21),
            ("past the floor", vec![(3, vec![(m21, m21 + 291)])], 1 << 21),
            ("without a floor", vec![(3, vec![(291, 582)])], 0),
            ("an unaligned range", vec![(3, vec![(292, 582)])], 1 << 21),
            ("below the first ID", vec![(3, vec![(0, 291)])], 1 << 21),
            ("an entry naming no part", vec![(2, vec![(291, 582)])], 1 << 21),
            ("an empty list", vec![(3, vec![])], 1 << 21),
            ("an empty range", vec![(3, vec![(582, 582)])], 1 << 21),
            ("unsorted ranges", vec![(3, vec![(m20, m20 + 291), (582, 873)])], 1 << 21),
        ];
        for (why, t, next) in bad {
            assert!(!decodes(&with(&t, next, &[])), "{why}");
        }
        assert!(!decodes(&with(&[(3, vec![(291, 582)]), (1, vec![(582, 873)])], 1 << 21, &[])), "entries out of part_no order");
        let mut two = Vec::new();
        encode_ranges(&table, &mut two);
        assert!(!decodes(&with(&table, 1 << 21, &two)), "two records");
        let mut trailing = Vec::new();
        let mut body = Vec::new();
        encode_ranges(&table, &mut body);
        put_ext_record(&mut trailing, EXT_RANGES, &[ext_records(&body).unwrap()[0].1, &[0][..]].concat());
        assert!(!decodes(&with(&[], 1 << 21, &trailing)), "trailing bytes");
    }

    /// A publish's edit (IX0 L2): a retired part drops its entry, an install gains one, a replaced entry is replaced
    /// (not duplicated), other records keep their place, and the floor only rises.
    #[test]
    fn rebuild_ranges_drops_retired_adds_installed_and_raises_the_floor() {
        let base = with(&[(1, vec![(291, 582)]), (3, vec![(582, 873)])], 873, &[]);
        let mut parts = base.parts.clone();
        let mut out = parts[0].clone();
        out.part_no = 5;
        parts.retain(|p| p.part_no != 1);
        parts.push(out);
        let ext = rebuild_ranges(&base.ext, &parts, &[(5, vec![(291, 582), (873, 1164)])]).unwrap();
        let m = base.clone().with_ext(ext);
        let m = Manifest { parts, ..m };
        let back = Manifest::decode(&m.encode()).unwrap();
        assert_eq!(range_entries(&back).unwrap(), vec![(3, vec![(582, 873)]), (5, vec![(291, 582), (873, 1164)])]);
        assert_eq!(next_row_id(&back).unwrap(), 1164, "the floor rises to the highest end");
        let kept = rebuild_ranges(&back.ext, &back.parts, &[(3, vec![(582, 873)])]).unwrap();
        assert_eq!(ext_records(&kept).unwrap().iter().filter(|r| r.0 == EXT_RANGES).count(), 1, "replaced, not appended");
        let high = with(&[(3, vec![(582, 873)])], 1 << 21, &[]);
        let same = rebuild_ranges(&high.ext, &high.parts, &[]).unwrap();
        assert_eq!(next_row_id(&high.clone().with_ext(same)).unwrap(), 1 << 21, "the floor never falls");
        assert!(rebuild_ranges(&base.ext, &base.parts, &[(2, vec![(1164, 1455)])]).is_err(), "an install the generation does not list");
        let mut extra = Vec::new();
        put_ext_record(&mut extra, 0x7ff0, b"later");
        let other = with(&[(1, vec![(291, 582)])], 582, &extra);
        let kept = rebuild_ranges(&other.ext, &other.parts, &[]).unwrap();
        assert_eq!(ext_records(&kept).unwrap()[0], (0x7ff0, &b"later"[..]), "an unknown record keeps its place");
    }

    /// Tags 2 (K1's births) and 8 (reserved) stay meaningless: no live tag takes either number, and a manifest that
    /// carries one reads it as an unknown record, skipped.
    #[test]
    fn retired_tags_stay_retired() {
        assert_eq!((EXT_RETIRED_BIRTHS, EXT_RESERVED_8), (2, 8));
        let live = [EXT_SEGMENTS, EXT_STATS, EXT_TOMBS, EXT_UNDER, EXT_SCHEMAS, EXT_RANGES, EXT_ROWID];
        assert_eq!(live, [1, 3, 4, 5, 6, 7, 9]);
        let mut extra = Vec::new();
        put_ext_record(&mut extra, EXT_RETIRED_BIRTHS, &[1, 0, 0, 0, 3, 0, 0, 0, 1, 0, 0, 0, 9, 0, 0, 0]);
        let m = with(&[(1, vec![(291, 582)])], 582, &extra);
        assert_eq!(ranges(&Manifest::decode(&m.encode()).unwrap()).unwrap(), vec![(1, vec![(291, 582)]), (3, vec![])], "a retired tag homes nothing");
    }

    #[test]
    fn a_reader_skips_tags_it_does_not_know() {
        let mut extra = Vec::new();
        put_ext_record(&mut extra, 0x7ff0, b"from a later binary");
        put_ext_record(&mut extra, EXT_RESERVED_8, b"reserved");
        assert!(decodes(&with(&[(1, vec![(291, 582)])], 582, &extra)));
    }
}

/// IXCOPYPATH: the row-ID work of a decode and of a publish's rebuild against IX0 L2+L3's code, kept here verbatim as
/// the oracle: the same results and the same errors on valid and corrupt tables, and over a long random history of
/// installs, seals, retirements, compactions and replacements.
#[cfg(test)]
mod copypath_tests {
    use super::*;
    use crate::rowid::{align_up, IDS_PER_BLOCK, ROWID_FIRST, ROWID_LIMIT};

    /// IX0 L2+L3's (20d74b8e59d) row-ID readers and rebuild, verbatim.
    mod oracle {
        use super::super::{next_row_id, single_record, with_next_row_id, with_ranges, IdRanges, Manifest, PartRecord, EXT_RANGES, EXT_ROWID};
        use crate::rowid::{IDS_PER_BLOCK, ROWID_FIRST, ROWID_LIMIT};
        use crate::wire::Cur;
        use crate::{FormatError, FormatResult};

        pub fn range_entries(m: &Manifest) -> FormatResult<Vec<(u32, IdRanges)>> {
            range_entries_in(&m.ext, &|no| m.parts.binary_search_by_key(&no, |p| p.part_no).is_ok())
        }

        pub fn range_entries_in(ext: &[u8], known: &dyn Fn(u32) -> bool) -> FormatResult<Vec<(u32, IdRanges)>> {
            const AT: &str = "Manifest ranges";
            let corrupt = || FormatError::Corrupt { at: AT };
            let Some(bytes) = single_record(ext, EXT_RANGES, AT)? else {
                return Ok(Vec::new());
            };
            let mut c = Cur::new(bytes);
            let n = c.u32(AT)? as usize;
            let mut out: Vec<(u32, IdRanges)> = Vec::with_capacity(n.min(c.remaining() / 24));
            for _ in 0..n {
                let part_no = c.u32(AT)?;
                let k = c.u32(AT)? as usize;
                if k == 0 || k > c.remaining() / 16 {
                    return Err(corrupt());
                }
                let ranges = (0..k).map(|_| Ok((c.u64(AT)?, c.u64(AT)?))).collect::<FormatResult<IdRanges>>()?;
                let shape = ranges.iter().all(|&(lo, hi)| lo < hi && hi <= ROWID_LIMIT) && ranges.windows(2).all(|w| w[0].1 <= w[1].0);
                if !shape || out.last().is_some_and(|(p, _)| *p >= part_no) || !known(part_no) {
                    return Err(corrupt());
                }
                out.push((part_no, ranges));
            }
            if c.remaining() != 0 {
                return Err(corrupt());
            }
            Ok(out)
        }

        pub fn ranges(m: &Manifest) -> FormatResult<Vec<(u32, IdRanges)>> {
            let mut e = range_entries(m)?.into_iter().peekable();
            Ok(m.parts
                .iter()
                .map(|p| match e.peek() {
                    Some((part_no, _)) if *part_no == p.part_no => e.next().expect("peeked"),
                    _ => (p.part_no, Vec::new()),
                })
                .collect())
        }

        pub fn rebuild_ranges(ext: &[u8], parts: &[PartRecord], add: &[(u32, IdRanges)]) -> FormatResult<Vec<u8>> {
            let present = |no: u32| parts.binary_search_by_key(&no, |p| p.part_no).is_ok();
            if add.iter().any(|(no, _)| !present(*no)) {
                return Err(FormatError::EncodeContract { detail: "row-ID ranges for a part the generation does not list" });
            }
            let mut table: Vec<(u32, IdRanges)> = range_entries_in(ext, &|_| true)?.into_iter().filter(|(no, _)| present(*no) && !add.iter().any(|a| a.0 == *no)).collect();
            table.extend(add.iter().filter(|(_, r)| !r.is_empty()).cloned());
            table.sort_unstable_by_key(|e| e.0);
            let floor = single_record(ext, EXT_ROWID, "Manifest next_row_id")?.map_or(Ok(0), |b| b.try_into().map(u64::from_le_bytes).map_err(|_| FormatError::Corrupt { at: "Manifest next_row_id" }))?;
            let top = table.iter().flat_map(|(_, r)| r.last()).map(|r| r.1).max().unwrap_or(0);
            with_next_row_id(&with_ranges(ext, &table)?, floor.max(top))
        }

        pub fn check_row_ids(m: &Manifest) -> FormatResult<()> {
            let next = next_row_id(m)?;
            let mut all: Vec<(u64, u64)> = Vec::new();
            for (_, r) in ranges(m)? {
                all.extend(r);
            }
            all.sort_unstable();
            if all.iter().any(|&(lo, hi)| lo < ROWID_FIRST || lo % IDS_PER_BLOCK != 0 || hi > next) || all.windows(2).any(|w| w[0].1 > w[1].0) {
                return Err(FormatError::Corrupt { at: "Manifest row-ID ranges: unaligned, past the floor or an ID in two homes" });
            }
            Ok(())
        }
    }

    /// xorshift64, seeded: no dependency, the same history on every run.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Rng {
            Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
        }
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }
        fn odds(&mut self, pct: u64) -> bool {
            self.below(100) < pct
        }
        fn shuffle<T>(&mut self, v: &mut [T]) {
            for i in (1..v.len()).rev() {
                v.swap(i, self.below(i as u64 + 1) as usize);
            }
        }
    }

    /// The golden sample's header and first part record, with `parts` (part numbers ascending) and `ext`.
    fn manifest(parts: Vec<PartRecord>, next_part_no: u32, ext: Vec<u8>) -> Manifest {
        let mut m = crate::tests::golden_sample_manifest();
        m.header.part_count = parts.len() as u32;
        m.header.next_part_no = next_part_no;
        m.parts = parts;
        m.with_ext(ext)
    }

    fn parts_of(nos: &[u32]) -> Vec<PartRecord> {
        let tmpl = crate::tests::golden_sample_manifest().parts[0];
        nos.iter().map(|&part_no| PartRecord { part_no, ..tmpl }).collect()
    }

    /// A ranges table over the parts `nos` (ascending): most parts one entry of 1-3 aligned, increasing ranges, with
    /// the IDs handed out in part order or shuffled (a compaction output homes IDs below a later bulk part's); and the
    /// floor at or above the top.
    fn random_table(r: &mut Rng, nos: &[u32]) -> (Vec<(u32, IdRanges)>, u64) {
        let mut order: Vec<usize> = (0..nos.len()).collect();
        if r.odds(50) {
            r.shuffle(&mut order);
        }
        let mut next = ROWID_FIRST;
        let mut table: Vec<(u32, IdRanges)> = Vec::new();
        for i in order {
            if r.odds(20) {
                continue;
            }
            let mut rs = Vec::new();
            for _ in 0..1 + r.below(3) {
                if r.odds(30) {
                    next += IDS_PER_BLOCK * (1 + r.below(4));
                }
                let len = align_up(1 + r.below(2000));
                rs.push((next, next + len));
                next += len;
            }
            table.push((nos[i], rs));
        }
        table.sort_by_key(|e| e.0);
        (table, if r.odds(70) { next } else { next + IDS_PER_BLOCK * r.below(10) })
    }

    /// `ext` with its `EXT_RANGES` record's body replaced by `body` (other records kept; a block that does not read is
    /// left as it is).
    fn with_body(ext: &[u8], body: &[u8]) -> Vec<u8> {
        with_record(ext, EXT_RANGES, Some(body)).unwrap_or_else(|_| ext.to_vec())
    }

    fn ranges_body(ext: &[u8]) -> Option<Vec<u8>> {
        single_record(ext, EXT_RANGES, "test").ok().flatten().map(<[u8]>::to_vec)
    }

    /// The index of a random entry of `table` with a range, if any.
    fn some_entry(r: &mut Rng, table: &[(u32, IdRanges)]) -> Option<usize> {
        let with: Vec<usize> = (0..table.len()).filter(|&i| !table[i].1.is_empty()).collect();
        (!with.is_empty()).then(|| with[r.below(with.len() as u64) as usize])
    }

    /// One random manifest of up to 24 parts, valid or broken by one or two of the ways a table can lie (in its
    /// entries, in its bytes, or in its extension block).
    fn random_case(r: &mut Rng) -> Manifest {
        let mut nos: Vec<u32> = Vec::new();
        let mut no = r.below(2) as u32;
        for _ in 0..r.below(25) {
            nos.push(no);
            no += 1 + r.below(3) as u32;
        }
        let (mut table, mut floor) = random_table(r, &nos);
        let mut ext = Vec::new();
        if r.odds(30) {
            put_ext_record(&mut ext, 0x7ff0, b"other");
        }
        let mut bytes_edits = Vec::new();
        for _ in 0..r.below(3) {
            let edit = r.below(22);
            match edit {
                1 => {
                    if let (Some(a), Some(b)) = (some_entry(r, &table), some_entry(r, &table)) {
                        let x = table[a].1[0];
                        table[b].1.push(x);
                    }
                }
                2 => {
                    if let Some(a) = some_entry(r, &table) {
                        table[a].1[0].0 += 1;
                    }
                }
                3 => floor = floor.saturating_sub(IDS_PER_BLOCK * (1 + r.below(3))),
                4 => {
                    if let Some(a) = some_entry(r, &table) {
                        table[a].1[0].0 = 0;
                    }
                }
                5 => {
                    if let Some(a) = some_entry(r, &table) {
                        table[a].0 = table[a].0 * 2 + 1;
                    }
                }
                6 => {
                    if let Some(a) = some_entry(r, &table) {
                        table[a].1.clear();
                    }
                }
                7 => {
                    if let Some(a) = some_entry(r, &table) {
                        table[a].1[0].1 = table[a].1[0].0;
                    }
                }
                8 => {
                    if let Some(a) = table.iter().position(|e| e.1.len() >= 2) {
                        table[a].1.swap(0, 1);
                    }
                }
                9 if table.len() >= 2 => {
                    let a = r.below(table.len() as u64 - 1) as usize;
                    table.swap(a, a + 1);
                }
                10 => {
                    if let Some(a) = some_entry(r, &table) {
                        let l = table[a].1.len();
                        table[a].1[l - 1].1 = ROWID_LIMIT + IDS_PER_BLOCK;
                    }
                }
                11 if !table.is_empty() => {
                    let a = r.below(table.len() as u64) as usize;
                    let e = table[a].clone();
                    table.insert(a, e);
                }
                12..=21 => bytes_edits.push(edit),
                _ => {}
            }
        }
        if !table.is_empty() || r.odds(10) {
            encode_ranges(&table, &mut ext);
        }
        let mut ext = with_next_row_id(&ext, floor).unwrap();
        for edit in bytes_edits {
            let body = ranges_body(&ext);
            match (edit, body) {
                (12, Some(b)) if !b.is_empty() => {
                    let cut = 1 + r.below(b.len() as u64) as usize;
                    ext = with_body(&ext, &b[..b.len() - cut]);
                }
                (13, Some(mut b)) => {
                    for _ in 0..1 + r.below(20) {
                        b.push(r.next() as u8);
                    }
                    ext = with_body(&ext, &b);
                }
                (14, Some(mut b)) if !b.is_empty() => {
                    let at = r.below(b.len() as u64) as usize;
                    b[at] ^= 1u8 << r.below(8);
                    ext = with_body(&ext, &b);
                }
                (15, Some(mut b)) if b.len() >= 4 => {
                    let n = u32::from_le_bytes(b[..4].try_into().unwrap());
                    let v = match r.below(3) {
                        0 => n.wrapping_add(1),
                        1 => n.wrapping_sub(1),
                        _ => r.next() as u32,
                    };
                    b[..4].copy_from_slice(&v.to_le_bytes());
                    ext = with_body(&ext, &b);
                }
                (16, Some(mut b)) if b.len() >= 12 => {
                    let v: u32 = if r.odds(50) { 0 } else { u32::MAX };
                    b[8..12].copy_from_slice(&v.to_le_bytes());
                    ext = with_body(&ext, &b);
                }
                (17, Some(b)) => put_ext_record(&mut ext, EXT_RANGES, &b),
                (18, _) => put_ext_record(&mut ext, EXT_ROWID, &floor.to_le_bytes()),
                (19, _) => ext = with_record(&ext, EXT_ROWID, Some(&[1u8, 2, 3, 4][..])).unwrap_or_else(|_| ext.clone()),
                (20, _) => ext = with_next_row_id(&ext, ROWID_LIMIT + 1).unwrap_or_else(|_| ext.clone()),
                (21, _) if !ext.is_empty() => {
                    let cut = 1 + r.below(ext.len().min(5) as u64) as usize;
                    ext.truncate(ext.len() - cut);
                }
                _ => {}
            }
        }
        let next_part_no = nos.last().map_or(1, |l| l + 1);
        manifest(parts_of(&nos), next_part_no, ext)
    }

    /// check_row_ids, ranges and range_entries give the oracle's results and errors on every case, and a decode refuses
    /// exactly the manifests the oracle's check refuses, with its error.
    #[test]
    fn row_id_checks_match_the_oracle_on_valid_and_corrupt_tables() {
        let mut r = Rng::new(0x1c0b_9a7e);
        let (mut ok, mut refused) = (0, 0);
        for case in 0..20_000 {
            let m = random_case(&mut r);
            let want = oracle::check_row_ids(&m);
            assert_eq!(check_row_ids(&m), want, "case {case}: check_row_ids");
            assert_eq!(ranges(&m), oracle::ranges(&m), "case {case}: ranges");
            assert_eq!(range_entries(&m), oracle::range_entries(&m), "case {case}: range_entries");
            match (flat_ranges(&m), oracle::ranges(&m)) {
                (Ok(f), Ok(t)) => {
                    assert_eq!(f.at.len(), m.parts.len() + 1, "case {case}");
                    assert!((0..m.parts.len()).all(|i| f.of(i) == &t[i].1[..]), "case {case}: flat_ranges");
                }
                (f, t) => assert_eq!(f.err(), t.err(), "case {case}: flat_ranges"),
            }
            assert_eq!(Manifest::decode(&m.encode()).err(), want.err(), "case {case}: decode");
            match want {
                Ok(()) => ok += 1,
                Err(_) => refused += 1,
            }
        }
        assert!(ok > 4_000 && refused > 4_000, "both kinds covered: {ok} valid, {refused} refused");
    }

    /// What the model saw: steps by kind.
    #[derive(Debug, Default)]
    struct ModelStats {
        tail_installs: u32,
        other: u32,
        /// Rebuilds that appended (the incremental path), every tail install among them.
        appended: u32,
    }

    /// Fresh aligned IDs for a part of `rows` rows: one range (a bulk grant), or two with a gap (a grant that grew).
    fn fresh(r: &mut Rng, next_id: &mut u64, rows: u64) -> IdRanges {
        let mut out = Vec::new();
        let n = if r.odds(15) { 2 } else { 1 };
        for _ in 0..n {
            *next_id += IDS_PER_BLOCK * r.below(3);
            let len = align_up(rows);
            out.push((*next_id, *next_id + len));
            *next_id += len;
        }
        out
    }

    /// The union of `inputs`' ranges, sorted, touching ranges joined (a compaction output's, ident::union_ranges).
    fn union(inputs: &[IdRanges]) -> IdRanges {
        let mut all: Vec<(u64, u64)> = inputs.iter().flatten().copied().collect();
        all.sort_unstable();
        let mut out: IdRanges = Vec::new();
        for (lo, hi) in all {
            match out.last_mut() {
                Some(l) if l.1 == lo => l.1 = hi,
                _ => out.push((lo, hi)),
            }
        }
        out
    }

    /// A random publish history of `steps` steps on one table, each step's rebuild against the oracle's (the bytes, or
    /// the error), the manifest decoded every 25 steps. `Err` names the first step that differs.
    fn model(seed: u64, steps: usize) -> Result<ModelStats, String> {
        let mut r = Rng::new(seed);
        let mut stats = ModelStats::default();
        let mut parts: Vec<PartRecord> = Vec::new();
        let mut next_part_no = 0u32;
        let mut reserved: Vec<u32> = Vec::new();
        let mut next_id = ROWID_FIRST;
        let mut ext = Vec::new();
        put_ext_record(&mut ext, 0x7ff0, b"other");
        let tmpl = crate::tests::golden_sample_manifest().parts[0];
        for step in 0..steps {
            let table = oracle::range_entries_in(&ext, &|_| true).map_err(|e| format!("step {step}: the table does not read: {e:?}"))?;
            let ids_of = |no: u32| table.iter().find(|e| e.0 == no).map_or_else(Vec::new, |e| e.1.clone());
            let mut add: Vec<(u32, IdRanges)> = Vec::new();
            let mut retire: Vec<u32> = Vec::new();
            let mut install: Vec<u32> = Vec::new();
            let full = parts.len() > 160;
            let kind = if full { 60 + r.below(25) } else { r.below(100) };
            let mut tail = false;
            match kind {
                0..=49 => {
                    tail = true;
                    for _ in 0..1 + r.below(3) {
                        let no = next_part_no;
                        next_part_no += 1;
                        if r.odds(10) {
                            reserved.push(next_part_no);
                            next_part_no += 1;
                        }
                        install.push(no);
                        let rows = 1 + r.below(5000);
                        match r.below(20) {
                            0 => {}
                            1 => add.push((no, Vec::new())),
                            _ => add.push((no, fresh(&mut r, &mut next_id, rows))),
                        }
                    }
                    if r.odds(10) {
                        let mut e = with_record(&ext, 0x7ff1, None).map_err(|e| format!("{e:?}"))?;
                        put_ext_record(&mut e, 0x7ff1, &[r.next() as u8; 3]);
                        ext = e;
                    }
                }
                50..=59 if !reserved.is_empty() => {
                    let no = reserved.remove(r.below(reserved.len() as u64) as usize);
                    install.push(no);
                    let rows = 1 + r.below(300_000);
                    add.push((no, fresh(&mut r, &mut next_id, rows)));
                }
                60..=69 if !parts.is_empty() => {
                    for _ in 0..1 + r.below(2) {
                        retire.push(parts[r.below(parts.len() as u64) as usize].part_no);
                    }
                }
                70..=84 if parts.len() >= 2 => {
                    let mut inputs: Vec<u32> = (0..2 + r.below(3)).map(|_| parts[r.below(parts.len() as u64) as usize].part_no).collect();
                    inputs.sort_unstable();
                    inputs.dedup();
                    let ids = union(&inputs.iter().map(|&no| ids_of(no)).collect::<Vec<_>>());
                    retire.extend(&inputs);
                    let no = next_part_no;
                    next_part_no += 1;
                    install.push(no);
                    if !ids.is_empty() || r.odds(50) {
                        add.push((no, ids));
                    }
                }
                85..=89 if !parts.is_empty() => {
                    let no = parts[r.below(parts.len() as u64) as usize].part_no;
                    let rows = 1 + r.below(5000);
                    let ids = match r.below(3) {
                        0 => ids_of(no),
                        1 => fresh(&mut r, &mut next_id, rows),
                        _ => Vec::new(),
                    };
                    add.push((no, ids));
                }
                _ => {}
            }
            parts.retain(|p| !retire.contains(&p.part_no));
            parts.extend(install.iter().map(|&part_no| PartRecord { part_no, ..tmpl }));
            parts.sort_by_key(|p| p.part_no);
            let appends = APPENDS.with(std::cell::Cell::get);
            let got = rebuild_ranges(&ext, &parts, &add);
            let appended = APPENDS.with(std::cell::Cell::get) > appends;
            let want = oracle::rebuild_ranges(&ext, &parts, &add);
            if got != want {
                return Err(format!("step {step} (kind {kind}, add {add:?}, retire {retire:?}): the rebuild differs from the oracle's"));
            }
            ext = got.map_err(|e| format!("step {step}: both refuse: {e:?}"))?;
            stats.appended += appended as u32;
            if tail && retire.is_empty() {
                stats.tail_installs += 1;
                if !appended {
                    return Err(format!("step {step}: a tail install (add {add:?}) did not append"));
                }
            } else {
                stats.other += 1;
            }
            if step % 25 == 0 {
                let m = manifest(parts.clone(), next_part_no.max(1), ext.clone());
                Manifest::decode(&m.encode()).map_err(|e| format!("step {step}: the generation does not decode: {e:?}"))?;
            }
        }
        Ok(stats)
    }

    /// The publish's rebuild of the row-ID records is the oracle's, byte for byte, over 2 × 12,000 random steps; every
    /// tail install appends. `IXCOPYPATH_BREAK_APPEND=1` (the floor not raised) or `=2` (the new entries first) breaks
    /// the append: this test then fails (its born-red run).
    #[test]
    fn rebuild_ranges_matches_the_oracle_over_a_random_history() {
        let knob = std::env::var("IXCOPYPATH_BREAK_APPEND").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        APPEND_KNOB.with(|k| k.set(knob));
        for seed in [0x5eed_0001, 0x5eed_0002] {
            let stats = model(seed, 12_000).unwrap_or_else(|e| panic!("seed {seed:#x}: {e}"));
            assert!(stats.tail_installs > 3_000 && stats.other > 3_000 && stats.appended >= stats.tail_installs, "{stats:?}");
        }
    }

    /// The model check has teeth: an append that does not raise the floor, or that puts the new entries before the
    /// stored ones, differs from the oracle within a few steps.
    #[test]
    fn a_broken_append_fails_the_model_check() {
        for knob in [1, 2] {
            APPEND_KNOB.with(|k| k.set(knob));
            let got = model(0x5eed_0003, 2_000);
            APPEND_KNOB.with(|k| k.set(0));
            let e = got.expect_err("the broken append is caught");
            assert!(e.contains("differs from the oracle"), "knob {knob}: {e}");
        }
    }
}
