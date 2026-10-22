//! Packed columnar RowId (spec §10; ruling O-8 via the O-M3-5 confirmed
//! conservative core): the (part, granule, row) bit split, FROZEN HERE.
//!
//! `lx_vec::RowId::from_columnar` is the typed constructor hole this split
//! fills — the one-line delegation lands with M3-G's declared lx edits
//! (`lanev3-m1-chunks.md` §5 freeze table; this crate touches no lx code).
//!
//! Order-embedding: within a part, RowId order == row order; across parts,
//! part_no order (publish order, spec §13). The TID surface stays the
//! typed-refusal posture (exactness-sweep C12): a RowId is never surfaced as
//! a ctid.
//!
//! ## SB-10 grain interaction (lanev4 batch-ABI §5, AB-5.1/AB-5.2)
//!
//! The 32/19/13 split is FROZEN and grain-INDEPENDENT: 13 bits hold the
//! MAXIMUM grain (8192 rows) exactly, and SB-10's byte-bounding only ever
//! SHRINKS granules (every `geom::GRAIN_LADDER` grain ≤ 8192), so the row
//! field carries any legal grain's row ordinal with headroom. Producers pack
//! (granule, row) AT THE PART'S ELECTED GRAIN ([`pack_rowid_at`] asserts the
//! grain bound; [`pack_rowid`] keeps the capacity bound); extraction
//! ([`rowid_granule`]/[`rowid_row`] — `granule_of_rowid` in the ledger's
//! vocabulary) is pure bit arithmetic, defined for EVERY rowid regardless of
//! grain — totality is preserved by construction, and the (part, granule)
//! a rowid names exists because the producer packed within the part's
//! footer-recorded geometry. Order-embedding survives any uniform grain:
//! within a part, (granule, row) lexicographic order IS row order.

use crate::geom::{BAND_ROWS, GRANULE_ROWS};

/// Bit budget (spec §10): 32 (part) + 19 (granule) + 13 (row) = 64.
pub const ROWID_PART_BITS: u32 = 32;
pub const ROWID_GRANULE_BITS: u32 = 19;
pub const ROWID_ROW_BITS: u32 = 13;

/// Maximum granules a single part may address.
pub const MAX_GRANULES_PER_PART: u32 = 1 << ROWID_GRANULE_BITS;

// The 10B-row static asserts (the M3-A exit-slice leg, `lanev3-m3-chunks.md`
// §5): the split must address a 10-billion-row table even degenerately.
const _: () = {
    // The split spends the whole word, exactly.
    assert!(ROWID_PART_BITS + ROWID_GRANULE_BITS + ROWID_ROW_BITS == 64);
    // The row field addresses a granule exactly.
    assert!((1u64 << ROWID_ROW_BITS) == GRANULE_ROWS as u64);
    // 10B rows fit even if every part holds only ONE band (65,536 rows):
    // 2^32 parts × 65,536 rows ≥ 10^10.
    assert!((1u128 << ROWID_PART_BITS) * BAND_ROWS as u128 >= 10_000_000_000);
    // Max part capacity: 2^19 granules × 8,192 rows = 2^32 rows per part.
    assert!((1u128 << ROWID_GRANULE_BITS) * GRANULE_ROWS as u128 == 1u128 << 32);
    // DV-local rowids (granule, row) fit u32 exactly (spec §15).
    assert!(ROWID_GRANULE_BITS + ROWID_ROW_BITS == 32);
};

/// Pack (part_no, granule, row) — caller guarantees field ranges (writer/scan
/// internals; debug-asserted). `row` is bounded by the CAPACITY grain here;
/// producers that know the part's elected grain use [`pack_rowid_at`].
#[inline]
pub const fn pack_rowid(part_no: u32, granule: u32, row: u32) -> u64 {
    debug_assert!(granule < MAX_GRANULES_PER_PART);
    debug_assert!(row < GRANULE_ROWS);
    ((part_no as u64) << (ROWID_GRANULE_BITS + ROWID_ROW_BITS))
        | ((granule as u64) << ROWID_ROW_BITS)
        | row as u64
}

/// [`pack_rowid`] with the part's elected granule grain enforced (SB-10):
/// identical packing — the bit split never moves — but the row ordinal is
/// debug-asserted against THIS part's grain, not just the 13-bit capacity.
#[inline]
pub const fn pack_rowid_at(
    part_no: u32,
    granule: u32,
    row: u32,
    grain: crate::geom::GranuleGrain,
) -> u64 {
    debug_assert!(row < grain.rows());
    pack_rowid(part_no, granule, row)
}

#[inline]
pub const fn rowid_part(id: u64) -> u32 {
    (id >> (ROWID_GRANULE_BITS + ROWID_ROW_BITS)) as u32
}

#[inline]
pub const fn rowid_granule(id: u64) -> u32 {
    ((id >> ROWID_ROW_BITS) as u32) & (MAX_GRANULES_PER_PART - 1)
}

#[inline]
pub const fn rowid_row(id: u64) -> u32 {
    (id as u32) & (GRANULE_ROWS - 1)
}

/// DV-local rowid (spec §15): `(granule << 13) | row`, exactly u32.
#[inline]
pub const fn dv_local(granule: u32, row: u32) -> u32 {
    debug_assert!(granule < MAX_GRANULES_PER_PART);
    debug_assert!(row < GRANULE_ROWS);
    (granule << ROWID_ROW_BITS) | row
}

#[inline]
pub const fn dv_local_granule(local: u32) -> u32 {
    local >> ROWID_ROW_BITS
}

#[inline]
pub const fn dv_local_row(local: u32) -> u32 {
    local & (GRANULE_ROWS - 1)
}

/// Ring slots a table's segments can occupy (the slot a segment record names; `rotate` takes the lowest free one).
pub const SEG_SLOTS: u16 = 256;
/// `Tombstone::succ` of a plain DELETE.
pub const SUCC_NONE: u64 = 0;
/// `Tombstone::succ` of a row moved to another partition.
pub const SUCC_MOVED: u64 = u64::MAX;
/// LOCK: `Tombstone::succ` of a LOCK row, the row lock of a row nobody has deleted or updated (`SELECT ... FOR
/// <strength>`, a BEFORE ROW trigger's lock): no deletion to anyone. Like [`SUCC_MOVED`] it sits above
/// [`ROWID_LIMIT`], so it is never read as a successor's ID.
pub const SUCC_LOCK: u64 = u64::MAX - 0x1ff;

/// IX0: a pgrc row's stable ID. 40 bits, the most a heap TID can carry at 291 offsets per block
/// (291 × 2^32 ≈ 2^40.2). It is the row's ctid, its tombstone and LOCK key, and its identity in every RowMap form.
pub const ROWID_BITS: u32 = 40;
/// IDs per virtual block: `MaxHeapTuplesPerPage` (pgrc_am asserts the two equal), so every ID's TID offset passes
/// tidbitmap's and GIN's checks.
pub const IDS_PER_BLOCK: u64 = 291;
/// The first ID ever handed out. ID 0 is [`SUCC_NONE`], so virtual block 0 stays empty.
pub const ROWID_FIRST: u64 = IDS_PER_BLOCK;
/// One past the last ID.
pub const ROWID_LIMIT: u64 = 1 << ROWID_BITS;

const _: () = {
    // Every ID below the limit lands on a block below InvalidBlockNumber (0xFFFF_FFFF).
    assert!((ROWID_LIMIT - 1) / IDS_PER_BLOCK < u32::MAX as u64);
    // The sentinels sit far above the ID space, so no ID is ever read as one.
    assert!(SUCC_LOCK >= ROWID_LIMIT && SUCC_MOVED >= ROWID_LIMIT && SUCC_NONE < ROWID_FIRST);
};

/// The heap TID an ID is surfaced as: `(id / 291, id % 291 + 1)`.
#[inline]
pub const fn tid_of(id: u64) -> (u32, u16) {
    debug_assert!(id < ROWID_LIMIT);
    ((id / IDS_PER_BLOCK) as u32, (id % IDS_PER_BLOCK) as u16 + 1)
}

/// The ID a TID names; `None` for offset 0, an offset past 291, or a TID past the ID space.
#[inline]
pub const fn id_of(block: u32, off: u16) -> Option<u64> {
    if off == 0 || off as u64 > IDS_PER_BLOCK {
        return None;
    }
    let id = block as u64 * IDS_PER_BLOCK + off as u64 - 1;
    if id >= ROWID_LIMIT {
        return None;
    }
    Some(id)
}

/// `n` rounded up to a multiple of 291: every reservation starts and ends on a virtual block boundary.
#[inline]
pub const fn align_up(n: u64) -> u64 {
    n.div_ceil(IDS_PER_BLOCK) * IDS_PER_BLOCK
}

/// Heap blocks per segment extent (OC-1 a): a segment's IDs are reserved 1,024 blocks at a time, the first extent at
/// activation and each further one by the inserter whose block first passes the last.
pub const SEG_EXTENT_BLOCKS: u32 = 1024;
/// IDs per segment extent: 297,984.
pub const SEG_EXTENT_IDS: u64 = SEG_EXTENT_BLOCKS as u64 * IDS_PER_BLOCK;
/// A segment this many extents long is rotated before an insert (soft: an inserter that finds the maintenance lock
/// held inserts anyway, and the next one tries again).
pub const SEG_SOFT_EXTENTS: u32 = 63;
/// The extent count `pgrc_rowid_stats` reports a segment past (the soft cap missed).
pub const SEG_MAX_EXTENTS: u32 = 64;
/// A table whose extent table holds this many entries makes an inserter wait for a seal (interruptibly) first.
pub const SEGEXT_SOFT_ENTRIES: usize = 4000;
/// The hard bound on a table's extent table: the headroom over [`SEGEXT_SOFT_ENTRIES`] covers inserters that passed
/// the soft check together, and keeps the `ROWID_EXTENTS` record under [`SEGEXT_MAX_BYTES`].
pub const SEGEXT_MAX_ENTRIES: usize = 4064;
/// The largest extent table image (the `segext` file, the `ROWID_EXTENTS` record's body).
pub const SEGEXT_MAX_BYTES: usize = 65536 - 64;

/// A segment row's local position `block × 291 + off − 1` (its ID's offset from the segment's ID space); `None` for
/// offset 0, an offset past 291, or a block past `u32` positions.
#[inline]
pub const fn seg_local(block: u32, off: u16) -> Option<u32> {
    if off == 0 || off as u64 > IDS_PER_BLOCK {
        return None;
    }
    let l = block as u64 * IDS_PER_BLOCK + off as u64 - 1;
    if l > u32::MAX as u64 {
        return None;
    }
    Some(l as u32)
}

/// The ID of local position `local` in a segment whose extents start at `bases` (extent order); `None` when its
/// extent is not reserved.
#[inline]
pub fn seg_id(bases: &[u64], local: u32) -> Option<u64> {
    let i = (local as u64 / SEG_EXTENT_IDS) as usize;
    Some(*bases.get(i)? + local as u64 % SEG_EXTENT_IDS)
}

/// The local position of `id` in a segment whose extents start at `bases`; `None` when no extent holds it.
#[inline]
pub fn seg_local_of(bases: &[u64], id: u64) -> Option<u32> {
    let i = bases.iter().position(|&b| b <= id && id - b < SEG_EXTENT_IDS)?;
    u32::try_from(i as u64 * SEG_EXTENT_IDS + (id - bases[i])).ok()
}

/// One extent of a live segment (DV-1): extent `idx` of the segment activated as `part_no` starts at ID `base`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SegExtent {
    pub part_no: u32,
    pub idx: u32,
    pub base: u64,
}

/// The extent table's magic: ASCII "PRCX".
pub const SEGEXT_MAGIC: u32 = u32::from_le_bytes(*b"PRCX");
const SEGEXT_ENTRY_LEN: usize = 16;

/// The extent table's 12-byte header: `u32` magic, `u32 count`, `u32` crc32c of the magic, the count and exactly
/// `count` entries.
pub const SEGEXT_HEADER_LEN: usize = 12;

fn segext_crc(head: &[u8], entries: &[u8]) -> crate::FormatResult<u32> {
    let mut b = crate::try_vec(8 + entries.len(), "segext crc")?;
    b.extend_from_slice(&head[..8]);
    b.extend_from_slice(entries);
    Ok(crate::wire::crc32c(&b))
}

/// The extent table image (the `segext` file and the `ROWID_EXTENTS` record's body): the header (magic, entry count,
/// crc32c over the magic, the count and exactly those entries), then per entry `u32 part_no, u32 idx, u64 base`. The
/// file is rewritten in place, never truncated, so a shorter table leaves a longer one's tail behind it; the header's
/// count says where the table ends, so the decoder never guesses. Refuses a table [`check_segext`] refuses and one past
/// [`SEGEXT_MAX_ENTRIES`] (the image then stays under [`SEGEXT_MAX_BYTES`]).
pub fn encode_segext(e: &[SegExtent]) -> crate::FormatResult<Vec<u8>> {
    if e.len() > SEGEXT_MAX_ENTRIES {
        return Err(crate::FormatError::EncodeContract { detail: "extent table past SEGEXT_MAX_ENTRIES" });
    }
    if !check_segext(e) {
        return Err(crate::FormatError::EncodeContract { detail: "extent table not sorted, contiguous, aligned and disjoint" });
    }
    let mut out = crate::try_vec(SEGEXT_HEADER_LEN + SEGEXT_ENTRY_LEN * e.len(), "segext")?;
    crate::wire::put_u32(&mut out, SEGEXT_MAGIC);
    crate::wire::put_u32(&mut out, e.len() as u32);
    crate::wire::put_u32(&mut out, 0);
    for x in e {
        crate::wire::put_u32(&mut out, x.part_no);
        crate::wire::put_u32(&mut out, x.idx);
        crate::wire::put_u64(&mut out, x.base);
    }
    let crc = segext_crc(&out[..8], &out[SEGEXT_HEADER_LEN..])?;
    out[8..12].copy_from_slice(&crc.to_le_bytes());
    assert!(out.len() <= SEGEXT_MAX_BYTES, "the extent table image stays under 64 KB");
    Ok(out)
}

/// The extent table an image holds: the header's count entries, checked against the header's crc. Bytes past them
/// are a longer table's tail and are ignored. Any mismatch (short image, magic, count past [`SEGEXT_MAX_ENTRIES`],
/// crc, or a table [`check_segext`] refuses) is an error: a torn write, which redo repairs from the last
/// `ROWID_EXTENTS` record and which is an ERROR outside recovery, never a silent truncation.
pub fn decode_segext(b: &[u8]) -> crate::FormatResult<Vec<SegExtent>> {
    const AT: &str = "segment extent table";
    if b.len() < SEGEXT_HEADER_LEN || b.len() > SEGEXT_MAX_BYTES {
        return Err(crate::FormatError::Truncated { at: AT });
    }
    if u32::from_le_bytes(b[0..4].try_into().expect("4")) != SEGEXT_MAGIC {
        return Err(crate::FormatError::BadMagic { at: AT });
    }
    let n = u32::from_le_bytes(b[4..8].try_into().expect("4")) as usize;
    if n > SEGEXT_MAX_ENTRIES {
        return Err(crate::FormatError::Corrupt { at: AT });
    }
    let end = SEGEXT_HEADER_LEN + n * SEGEXT_ENTRY_LEN;
    if b.len() < end {
        return Err(crate::FormatError::Truncated { at: AT });
    }
    if u32::from_le_bytes(b[8..12].try_into().expect("4")) != segext_crc(&b[..8], &b[SEGEXT_HEADER_LEN..end])? {
        return Err(crate::FormatError::CrcMismatch { at: AT });
    }
    let mut c = crate::wire::Cur::new(&b[SEGEXT_HEADER_LEN..end]);
    let e = (0..n).map(|_| Ok(SegExtent { part_no: c.u32(AT)?, idx: c.u32(AT)?, base: c.u64(AT)? })).collect::<crate::FormatResult<Vec<_>>>()?;
    if !check_segext(&e) {
        return Err(crate::FormatError::Corrupt { at: AT });
    }
    Ok(e)
}

/// An extent table's shape: sorted by `(part_no, idx)`, each segment's extents `0..k` with none missing (they are
/// reserved in order), every base aligned to a virtual block, at or past [`ROWID_FIRST`], its extent below
/// [`ROWID_LIMIT`], and no two extents sharing an ID.
pub fn check_segext(e: &[SegExtent]) -> bool {
    let shape = e.iter().enumerate().all(|(i, x)| {
        let next = match i.checked_sub(1).map(|j| e[j]) {
            Some(p) if p.part_no == x.part_no => x.idx == p.idx + 1,
            Some(p) => p.part_no < x.part_no && x.idx == 0,
            None => x.idx == 0,
        };
        next && x.base >= ROWID_FIRST && x.base % IDS_PER_BLOCK == 0 && x.base.checked_add(SEG_EXTENT_IDS).is_some_and(|h| h <= ROWID_LIMIT)
    });
    let mut bases: Vec<u64> = e.iter().map(|x| x.base).collect();
    bases.sort_unstable();
    shape && bases.windows(2).all(|w| w[0] + SEG_EXTENT_IDS <= w[1])
}

/// The `rowid` file's length: `u64 high`, `u32` magic, `u32` crc32c of the first 12 bytes.
pub const ROWID_FILE_LEN: usize = 16;
/// The `rowid` file's magic: ASCII "PRCI".
pub const ROWID_FILE_MAGIC: u32 = u32::from_le_bytes(*b"PRCI");

/// The `rowid` file's image for the high-water mark `high` (every ID below it is reserved).
pub fn encode_rowid_file(high: u64) -> [u8; ROWID_FILE_LEN] {
    let mut b = [0u8; ROWID_FILE_LEN];
    b[..8].copy_from_slice(&high.to_le_bytes());
    b[8..12].copy_from_slice(&ROWID_FILE_MAGIC.to_le_bytes());
    let crc = crate::wire::crc32c(&b[..12]);
    b[12..].copy_from_slice(&crc.to_le_bytes());
    b
}

/// The high-water mark a `rowid` file holds. Refuses a wrong length, magic or crc, and a mark past [`ROWID_LIMIT`].
pub fn decode_rowid_file(b: &[u8]) -> crate::FormatResult<u64> {
    const AT: &str = "rowid file";
    if b.len() != ROWID_FILE_LEN {
        return Err(crate::FormatError::Truncated { at: AT });
    }
    if u32::from_le_bytes(b[12..].try_into().expect("4")) != crate::wire::crc32c(&b[..12]) {
        return Err(crate::FormatError::CrcMismatch { at: AT });
    }
    if u32::from_le_bytes(b[8..12].try_into().expect("4")) != ROWID_FILE_MAGIC {
        return Err(crate::FormatError::BadMagic { at: AT });
    }
    let high = u64::from_le_bytes(b[..8].try_into().expect("8"));
    if high > ROWID_LIMIT {
        return Err(crate::FormatError::Corrupt { at: AT });
    }
    Ok(high)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_ids_round_trip_through_tids_at_the_edges() {
        for id in [0, 1, 290, 291, 292, 581, 582, ROWID_FIRST, ROWID_LIMIT / 2 + 7, ROWID_LIMIT - 1] {
            let (b, o) = tid_of(id);
            assert!((1..=291).contains(&o), "id {id} off {o}");
            assert_eq!(id_of(b, o), Some(id), "id {id}");
        }
        assert_eq!(tid_of(0), (0, 1));
        assert_eq!(tid_of(290), (0, 291));
        assert_eq!(tid_of(291), (1, 1));
        assert_eq!(tid_of(ROWID_FIRST), (1, 1));
        assert_eq!(id_of(1, 1), Some(291));
        assert_eq!(id_of(0, 0), None);
        assert_eq!(id_of(7, 292), None);
        assert_eq!(id_of(7, u16::MAX), None);
        let (top, last) = tid_of(ROWID_LIMIT - 1);
        assert!(top < u32::MAX, "the top ID's block is a valid BlockNumber");
        assert_eq!(id_of(top, last + 1), None, "one past the top ID");
        assert_eq!(id_of(top + 1, 1), None);
        assert_eq!(id_of(u32::MAX, 1), None, "InvalidBlockNumber names no ID");
    }

    #[test]
    fn the_rowid_file_round_trips_and_refuses_damage() {
        for high in [0, ROWID_FIRST, 1 << 33, ROWID_LIMIT] {
            assert_eq!(decode_rowid_file(&encode_rowid_file(high)).unwrap(), high);
        }
        let img = encode_rowid_file(582);
        assert_eq!(&img[..8], &582u64.to_le_bytes());
        assert_eq!(&img[8..12], b"PRCI");
        assert!(decode_rowid_file(&img[..15]).is_err(), "short");
        let mut flipped = img;
        flipped[0] ^= 1;
        assert!(decode_rowid_file(&flipped).is_err(), "crc");
        let mut past = [0u8; 16];
        past[..8].copy_from_slice(&(ROWID_LIMIT + 291).to_le_bytes());
        past[8..12].copy_from_slice(b"PRCI");
        let crc = crate::wire::crc32c(&past[..12]);
        past[12..].copy_from_slice(&crc.to_le_bytes());
        assert!(decode_rowid_file(&past).is_err(), "past the ID space");
    }

    #[test]
    fn reservations_align_to_virtual_blocks() {
        assert_eq!(align_up(0), 0);
        assert_eq!(align_up(1), 291);
        assert_eq!(align_up(291), 291);
        assert_eq!(align_up(292), 582);
        assert_eq!(align_up(1024 * 291), 1024 * 291);
        assert_eq!(align_up(ROWID_LIMIT - 1) % IDS_PER_BLOCK, 0);
        assert!(align_up(ROWID_LIMIT - 1) >= ROWID_LIMIT - 1);
    }

    #[test]
    fn segment_local_positions_map_through_extents() {
        assert_eq!(seg_local(0, 1), Some(0));
        assert_eq!(seg_local(0, 291), Some(290));
        assert_eq!(seg_local(1, 1), Some(291));
        assert_eq!(seg_local(0, 0), None);
        assert_eq!(seg_local(0, 292), None);
        assert_eq!(seg_local(u32::MAX, 291), None, "past u32 positions");
        let bases = [ROWID_FIRST, 10 * SEG_EXTENT_IDS + 582];
        assert_eq!(seg_id(&bases, 0), Some(ROWID_FIRST));
        let last0 = SEG_EXTENT_IDS as u32 - 1;
        assert_eq!(seg_id(&bases, last0), Some(ROWID_FIRST + SEG_EXTENT_IDS - 1));
        assert_eq!(seg_id(&bases, last0 + 1), Some(bases[1]), "block 1024 starts extent 1");
        assert_eq!(seg_id(&bases, seg_local(1024, 1).unwrap()), Some(bases[1]));
        assert_eq!(seg_id(&bases, seg_local(2048, 1).unwrap()), None, "extent 2 not reserved");
        for l in [0, 7, last0, last0 + 1, last0 + 300] {
            assert_eq!(seg_local_of(&bases, seg_id(&bases, l).unwrap()), Some(l));
        }
        assert_eq!(seg_local_of(&bases, ROWID_FIRST - 1), None);
        assert_eq!(seg_local_of(&bases, bases[1] + SEG_EXTENT_IDS), None);
        let (b, o) = tid_of(seg_id(&bases, seg_local(1030, 5).unwrap()).unwrap());
        assert_eq!((b as u64, o), (bases[1] / IDS_PER_BLOCK + 6, 5), "a segment row's ctid is its heap TID shifted by its extent");
    }

    fn ext(part_no: u32, idx: u32, k: u64) -> SegExtent {
        SegExtent { part_no, idx, base: ROWID_FIRST + k * SEG_EXTENT_IDS }
    }

    #[test]
    fn the_extent_table_round_trips_and_refuses_bad_shapes() {
        let t = vec![ext(3, 0, 0), ext(3, 1, 4), ext(9, 0, 2)];
        let img = encode_segext(&t).unwrap();
        assert_eq!(img.len(), 12 + 16 * 3);
        assert_eq!(&img[..4], b"PRCX");
        assert_eq!(decode_segext(&img).unwrap(), t);
        assert_eq!(decode_segext(&encode_segext(&[]).unwrap()).unwrap(), vec![]);
        let mut flipped = img.clone();
        flipped[9] ^= 1;
        assert!(decode_segext(&flipped).is_err(), "crc");
        assert!(decode_segext(&img[..img.len() - 1]).is_err(), "short");
        for bad in [
            vec![ext(3, 1, 0)],
            vec![ext(3, 0, 0), ext(3, 2, 1)],
            vec![ext(9, 0, 0), ext(3, 0, 1)],
            vec![ext(3, 0, 0), ext(4, 0, 0)],
            vec![SegExtent { part_no: 1, idx: 0, base: ROWID_FIRST + 1 }],
            vec![SegExtent { part_no: 1, idx: 0, base: 0 }],
            vec![SegExtent { part_no: 1, idx: 0, base: ROWID_LIMIT - IDS_PER_BLOCK }],
            vec![SegExtent { part_no: 1, idx: 0, base: ROWID_FIRST }, SegExtent { part_no: 2, idx: 0, base: ROWID_FIRST + SEG_EXTENT_IDS - IDS_PER_BLOCK }],
        ] {
            assert!(!check_segext(&bad) && encode_segext(&bad).is_err(), "{bad:?}");
        }
    }

    /// The file is rewritten in place and never truncated: a shorter table over a longer one decodes exactly from the
    /// header's count, the longer one's tail ignored; a flipped byte anywhere in the live region (header or entries)
    /// is a torn write, refused, while one in the tail is not looked at.
    #[test]
    fn a_shorter_table_over_a_longer_file_decodes_exactly() {
        let long = encode_segext(&[ext(3, 0, 0), ext(3, 1, 4), ext(9, 0, 2), ext(9, 1, 7)]).unwrap();
        let short_t = vec![ext(9, 0, 2)];
        let short = encode_segext(&short_t).unwrap();
        let mut file = long.clone();
        file[..short.len()].copy_from_slice(&short);
        assert_eq!(&img_header(&file), &img_header(&short));
        assert_eq!(decode_segext(&file).unwrap(), short_t, "the longer table's tail is ignored");
        for i in 0..short.len() {
            let mut torn = file.clone();
            torn[i] ^= 0x10;
            assert!(decode_segext(&torn).is_err(), "a flipped byte {i} of the live region is detected");
        }
        let mut tail = file.clone();
        *tail.last_mut().unwrap() ^= 0x10;
        assert_eq!(decode_segext(&tail).unwrap(), short_t, "the tail is never read");
        let mut half = short.clone();
        half[4..8].copy_from_slice(&2u32.to_le_bytes());
        assert!(decode_segext(&half).is_err(), "a count past the image is torn, never guessed");
        let empty = encode_segext(&[]).unwrap();
        let mut file = long.clone();
        file[..empty.len()].copy_from_slice(&empty);
        assert_eq!(decode_segext(&file).unwrap(), vec![], "an emptied table over a full one");
    }

    fn img_header(b: &[u8]) -> [u8; SEGEXT_HEADER_LEN] {
        b[..SEGEXT_HEADER_LEN].try_into().unwrap()
    }

    #[test]
    fn the_extent_table_is_capped_under_64_kb() {
        let full: Vec<SegExtent> = (0..SEGEXT_MAX_ENTRIES as u64).map(|k| ext((k / 64) as u32, (k % 64) as u32, k)).collect();
        let img = encode_segext(&full).unwrap();
        assert!(img.len() <= SEGEXT_MAX_BYTES && img.len() <= 65536, "{} bytes at the cap", img.len());
        assert_eq!(decode_segext(&img).unwrap().len(), SEGEXT_MAX_ENTRIES);
        let mut over = full.clone();
        over.push(ext(u32::MAX, 0, SEGEXT_MAX_ENTRIES as u64));
        assert!(encode_segext(&over).is_err(), "one past the hard cap");
        assert!(SEGEXT_SOFT_ENTRIES < SEGEXT_MAX_ENTRIES && SEG_SOFT_EXTENTS < SEG_MAX_EXTENTS);
    }
}
