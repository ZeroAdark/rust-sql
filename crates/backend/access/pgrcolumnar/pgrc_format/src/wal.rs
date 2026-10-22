//! The `pgrc` WAL resource manager's records (spec §14; delta v2 design §7.2): ids and codecs, no I/O.
//! File names travel as strings and must classify (`dirlayout::classify`), so redo writes nothing else.

use crate::dirlayout::classify;
use crate::manifest::{ManifestHeader, PartRecord, MANIFESTF_ROWMAP2, MANIFESTF_RUNS, MANIFESTF_UNDER, MANIFEST_HEADER_LEN, PART_RECORD_LEN};
use crate::wire::{put_u16, put_u32, put_u64, put_u8, Cur};
use crate::{FormatError, FormatResult};

pub const XLOG_PGRC_DIR_CREATE: u8 = 0x00;
pub const XLOG_PGRC_FILE_WRITE: u8 = 0x10;
pub const XLOG_PGRC_PUBLISH: u8 = 0x20;
pub const XLOG_PGRC_INSTALL: u8 = 0x30;
pub const XLOG_PGRC_RECLAIM: u8 = 0x40;
pub const XLOG_PGRC_TRUNCATE: u8 = 0x50;
/// Logical-decoding records (RL): no directory change, so redo skips them; decode emits them on the parent.
pub const XLOG_PGRC_ROW_DELETE: u8 = 0x60;
pub const XLOG_PGRC_ROW_UPDATE: u8 = 0x70;
pub const XLOG_PGRC_BULK_ROWS: u8 = 0x80;
/// A `FILE_WRITE` whose redo first truncates the file: the first write after the writer created it.
pub const XLOG_PGRC_FILE_CREATE: u8 = 0x90;
/// A durable file named by reference (WR): its bytes travel beside the WAL (stream, archive, base backup), not in it.
pub const XLOG_PGRC_FILE_REF: u8 = 0xA0;
/// IX0 (OC-2 a): a table's row IDs below `high` are reserved. Non-transactional: inserted before any ID of the batch
/// is used, so every record or file that names an ID is later in WAL than its reservation. Redo takes the max.
pub const XLOG_PGRC_ROWID_RESERVE: u8 = 0xB0;
/// IX0 DV-1: a table's live segment extent table after one change (an extent reserved, or the extents of a reused
/// slot's previous segment pruned), whole. Non-transactional like `ROWID_RESERVE`, inserted before any ID of a new
/// extent is used; redo overwrites the `segext` file with it, so a torn write of the file is repaired by replay.
pub const XLOG_PGRC_ROWID_EXTENTS: u8 = 0xC0;
/// IX0 L4 (OC-4 C, main 2026-10-05; reserved for IX0, no other lane takes it): a renumbering VACUUM FULL lowered a
/// table's counter to `high` ([`RowidReserveRec`]'s layout). Logged after the renumber's last publish commits, under
/// the command's AccessExclusiveLock, so no ID at or above `high` is in use; redo writes the `rowid` file exactly
/// (not the max). A lost record leaves the counter high, never low.
pub const XLOG_PGRC_ROWID_SET: u8 = 0xD0;
pub const XLOG_PGRC_OPMASK: u8 = 0xF0;

/// The largest `FILE_WRITE` payload; a longer write is split.
pub const FILE_WRITE_MAX_CHUNK: usize = 256 * 1024;

/// A relation's storage identity as the records carry it (`RelFileLocator` order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DirLoc {
    pub spc: u32,
    pub db: u32,
    pub relnumber: u32,
}

pub const DIR_LOC_LEN: usize = 12;

impl DirLoc {
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        put_u32(out, self.spc);
        put_u32(out, self.db);
        put_u32(out, self.relnumber);
    }
    pub fn decode_from(c: &mut Cur<'_>) -> FormatResult<DirLoc> {
        Ok(DirLoc { spc: c.u32("DirLoc")?, db: c.u32("DirLoc")?, relnumber: c.u32("DirLoc")? })
    }
}

fn put_name(out: &mut Vec<u8>, name: &str) -> FormatResult<()> {
    if classify(name).is_none() || name.len() > u16::MAX as usize {
        return Err(FormatError::EncodeContract { detail: "pgrc WAL file name does not classify" });
    }
    put_u16(out, name.len() as u16);
    out.extend_from_slice(name.as_bytes());
    Ok(())
}

fn get_name<'a>(c: &mut Cur<'a>, at: &'static str) -> FormatResult<&'a str> {
    let len = c.u16(at)? as usize;
    let s = std::str::from_utf8(c.take(len, at)?).map_err(|_| FormatError::Corrupt { at })?;
    if classify(s).is_none() {
        return Err(FormatError::Corrupt { at });
    }
    Ok(s)
}

fn get_bytes<'a>(c: &mut Cur<'a>, at: &'static str) -> FormatResult<&'a [u8]> {
    let len = c.u32(at)? as usize;
    c.take(len, at)
}

fn put_bytes32(out: &mut Vec<u8>, b: &[u8]) {
    put_u32(out, b.len() as u32);
    out.extend_from_slice(b);
}

fn finish<T>(c: &Cur<'_>, v: T, at: &'static str) -> FormatResult<T> {
    if c.remaining() != 0 {
        return Err(FormatError::Corrupt { at });
    }
    Ok(v)
}

/// `ROWID_RESERVE`: the table and its new high-water mark. `ROWID_SET` (IX0 L4) has the same layout: the table and
/// its lowered mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowidReserveRec {
    pub dir: DirLoc,
    pub high: u64,
}

impl RowidReserveRec {
    pub fn encode(&self, out: &mut Vec<u8>) {
        self.dir.encode_into(out);
        put_u64(out, self.high);
    }
    /// Refuses trailing bytes and a mark past `ROWID_LIMIT`.
    pub fn decode(b: &[u8]) -> FormatResult<RowidReserveRec> {
        let mut c = Cur::new(b);
        let dir = DirLoc::decode_from(&mut c)?;
        let high = c.u64("ROWID_RESERVE")?;
        if high > crate::rowid::ROWID_LIMIT {
            return Err(FormatError::Corrupt { at: "ROWID_RESERVE high" });
        }
        finish(&c, RowidReserveRec { dir, high }, "ROWID_RESERVE trailing bytes")
    }
}

/// `ROWID_EXTENTS`: the table and its whole extent table (`crate::rowid::encode_segext`'s image, at most
/// [`crate::rowid::SEGEXT_MAX_BYTES`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowidExtentsRec {
    pub dir: DirLoc,
    pub table: Vec<crate::rowid::SegExtent>,
}

impl RowidExtentsRec {
    /// Refuses a table `encode_segext` refuses (past the hard cap among them).
    pub fn encode(&self, out: &mut Vec<u8>) -> FormatResult<()> {
        let img = crate::rowid::encode_segext(&self.table)?;
        self.dir.encode_into(out);
        out.extend_from_slice(&img);
        Ok(())
    }
    pub fn decode(b: &[u8]) -> FormatResult<RowidExtentsRec> {
        let mut c = Cur::new(b);
        let dir = DirLoc::decode_from(&mut c)?;
        let img = c.take(c.remaining(), "ROWID_EXTENTS")?;
        let table = crate::rowid::decode_segext(img)?;
        // the file may carry a longer table's tail; the record is exactly its table
        if img.len() != crate::rowid::SEGEXT_HEADER_LEN + 16 * table.len() {
            return Err(crate::FormatError::Corrupt { at: "ROWID_EXTENTS" });
        }
        Ok(RowidExtentsRec { dir, table })
    }
}

/// `DIR_CREATE` and `TRUNCATE`: the locator alone.
pub fn encode_dir(dir: DirLoc, out: &mut Vec<u8>) {
    dir.encode_into(out);
}

pub fn decode_dir(b: &[u8]) -> FormatResult<DirLoc> {
    let mut c = Cur::new(b);
    let d = DirLoc::decode_from(&mut c)?;
    finish(&c, d, "pgrc dir record")
}

/// `FILE_WRITE` and `FILE_CREATE`: header (`encode_header`) followed by the bytes, so the inserter can register the
/// payload without a copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileWriteRec<'a> {
    pub dir: DirLoc,
    pub name: &'a str,
    pub off: u64,
    pub bytes: &'a [u8],
}

impl<'a> FileWriteRec<'a> {
    pub fn encode_header(&self, out: &mut Vec<u8>) -> FormatResult<()> {
        if self.bytes.len() > FILE_WRITE_MAX_CHUNK {
            return Err(FormatError::EncodeContract { detail: "FILE_WRITE chunk over FILE_WRITE_MAX_CHUNK" });
        }
        self.dir.encode_into(out);
        put_u64(out, self.off);
        put_name(out, self.name)
    }

    pub fn encode(&self, out: &mut Vec<u8>) -> FormatResult<()> {
        self.encode_header(out)?;
        out.extend_from_slice(self.bytes);
        Ok(())
    }

    pub fn decode(b: &'a [u8]) -> FormatResult<FileWriteRec<'a>> {
        let mut c = Cur::new(b);
        let dir = DirLoc::decode_from(&mut c)?;
        let off = c.u64("FILE_WRITE")?;
        let name = get_name(&mut c, "FILE_WRITE name")?;
        let bytes = c.take(c.remaining(), "FILE_WRITE")?;
        if bytes.len() > FILE_WRITE_MAX_CHUNK {
            return Err(FormatError::Corrupt { at: "FILE_WRITE chunk length" });
        }
        Ok(FileWriteRec { dir, name, off, bytes })
    }
}

/// The most byte ranges a `FILE_REF`'s crc may skip (a part's publish-time header and footer patches).
pub const FILE_REF_MAX_MASKS: usize = 2;

/// `(offset, length)` ranges the crc skips; a length of 0 marks an unused slot. Used slots come first, sorted by
/// offset, disjoint, and inside the file.
pub type SmallMasks = [(u64, u32); FILE_REF_MAX_MASKS];

pub const NO_MASKS: SmallMasks = [(0, 0); FILE_REF_MAX_MASKS];

/// The used slots of `masks` if they are well formed against a file of `len` bytes.
pub fn used_masks(masks: &SmallMasks, len: u64) -> Option<&[(u64, u32)]> {
    let n = masks.iter().take_while(|m| m.1 != 0).count();
    if masks[n..].iter().any(|m| *m != (0, 0)) {
        return None;
    }
    let used = &masks[..n];
    let mut end = 0u64;
    for (i, &(off, l)) in used.iter().enumerate() {
        let stop = off.checked_add(l as u64)?;
        if (i > 0 && off < end) || stop > len {
            return None;
        }
        end = stop;
    }
    Some(used)
}

/// `FILE_REF`: the complete file `dir/name`, fsynced before the record, `len` bytes whose crc32c with `masks` skipped
/// is `crc`. Redo makes the table directory hold those bytes from a source outside the WAL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRefRec<'a> {
    pub dir: DirLoc,
    pub name: &'a str,
    pub len: u64,
    pub crc: u32,
    pub masks: SmallMasks,
}

impl<'a> FileRefRec<'a> {
    pub fn encode(&self, out: &mut Vec<u8>) -> FormatResult<()> {
        let used = used_masks(&self.masks, self.len)
            .ok_or(FormatError::EncodeContract { detail: "FILE_REF masks unsorted, overlapping or past the file" })?;
        self.dir.encode_into(out);
        put_name(out, self.name)?;
        put_u64(out, self.len);
        put_u32(out, self.crc);
        put_u8(out, used.len() as u8);
        for &(off, l) in used {
            put_u64(out, off);
            put_u32(out, l);
        }
        Ok(())
    }

    pub fn decode(b: &'a [u8]) -> FormatResult<FileRefRec<'a>> {
        let mut c = Cur::new(b);
        let dir = DirLoc::decode_from(&mut c)?;
        let name = get_name(&mut c, "FILE_REF name")?;
        let len = c.u64("FILE_REF")?;
        let crc = c.u32("FILE_REF")?;
        let n = c.u8("FILE_REF masks")? as usize;
        if n > FILE_REF_MAX_MASKS {
            return Err(FormatError::Corrupt { at: "FILE_REF mask count" });
        }
        let mut masks = NO_MASKS;
        for m in masks.iter_mut().take(n) {
            *m = (c.u64("FILE_REF mask")?, c.u32("FILE_REF mask")?);
            if m.1 == 0 {
                return Err(FormatError::Corrupt { at: "FILE_REF empty mask" });
            }
        }
        if used_masks(&masks, len).is_none() {
            return Err(FormatError::Corrupt { at: "FILE_REF masks" });
        }
        finish(&c, FileRefRec { dir, name, len, crc, masks }, "FILE_REF trailing bytes")
    }
}

/// SEAL-OFFHEAD: the kind byte of a PUBLISH record whose manifest carries `EXT_UNDER` (header flag
/// `MANIFESTF_UNDER`, a stacked generation) has this bit set. A binary without EXT_UNDER refuses the record (an
/// unknown kind) instead of replaying it: its redo reads an undecodable base manifest as a missing one and would skip
/// every later publish of the table, rolling the directory back to the last generation it can decode.
pub const PUBLISH_KIND_UNDER: u8 = 0x80;
/// ROWMAP2 M1: the kind byte of a PUBLISH record whose manifest names a granules map (header flag
/// `MANIFESTF_ROWMAP2`) has this bit set, for [`PUBLISH_KIND_UNDER`]'s reason: a binary before ROWMAP2 refuses the
/// record (an unknown kind) before its redo touches a file, instead of reading the undecodable base manifest as a
/// missing one (the record's temps unlinked, every later publish of the table skipped).
pub const PUBLISH_KIND_ROWMAP2: u8 = 0x40;
/// SORTRUNS-E0: the kind byte of a PUBLISH record whose manifest carries sorted runs (header flag `MANIFESTF_RUNS`)
/// has this bit set, for [`PUBLISH_KIND_UNDER`]'s reason: a binary before sorted runs refuses the record (an unknown
/// kind) before its redo touches a file.
pub const PUBLISH_KIND_RUNS: u8 = 0x20;
/// The kind byte's flag bits; the rest is the [`PublishKind`].
pub const PUBLISH_KIND_FLAGS: u8 = PUBLISH_KIND_UNDER | PUBLISH_KIND_ROWMAP2 | PUBLISH_KIND_RUNS;

/// Why a generation was published; redo does not branch on it (desc and decoding do).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PublishKind {
    Bulk = 1,
    Rotate = 2,
    Seal = 3,
    Compile = 4,
    Compact = 5,
    /// AUTOVAC FREEZE: VACUUM's republish of the effective generation under the frozen publisher (spec §13.1
    /// "Freeze"): the base's parts unchanged. Redo keys its snapshot conflict on that publisher, not on the kind.
    Freeze = 6,
}

impl PublishKind {
    pub fn from_u8(v: u8) -> Option<PublishKind> {
        Some(match v {
            1 => PublishKind::Bulk,
            2 => PublishKind::Rotate,
            3 => PublishKind::Seal,
            4 => PublishKind::Compile,
            5 => PublishKind::Compact,
            6 => PublishKind::Freeze,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            PublishKind::Bulk => "bulk",
            PublishKind::Rotate => "rotate",
            PublishKind::Seal => "seal",
            PublishKind::Compile => "compile",
            PublishKind::Compact => "compact",
            PublishKind::Freeze => "freeze",
        }
    }
}

/// One sealed tmp file becoming `part-<part_no>`: the publish-time header and footer patches ride the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartInstall {
    pub tmp: String,
    pub part_no: u32,
    pub header: Vec<u8>,
    pub footer_off: u64,
    pub footer: Vec<u8>,
}

/// `PUBLISH`: the new manifest as a diff over `base_gen` (header, upserts by `part_no`, retirements, extension block),
/// plus the file operations `apply_publish` performs and the resulting manifest's crc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishRec {
    pub dir: DirLoc,
    pub base_gen: u64,
    pub kind: PublishKind,
    pub header: ManifestHeader,
    pub upsert: Vec<PartRecord>,
    pub retire: Vec<u32>,
    pub ext: Vec<u8>,
    pub installs: Vec<PartInstall>,
    pub renames: Vec<(String, String)>,
    pub manifest_crc: u32,
}

fn put_renames(out: &mut Vec<u8>, renames: &[(String, String)]) -> FormatResult<()> {
    put_u32(out, renames.len() as u32);
    for (from, to) in renames {
        put_name(out, from)?;
        put_name(out, to)?;
    }
    Ok(())
}

fn get_renames(c: &mut Cur<'_>) -> FormatResult<Vec<(String, String)>> {
    let n = c.u32("renames")? as usize;
    let mut v = crate::try_vec(n.min(c.remaining() / 4), "renames")?;
    for _ in 0..n {
        let from = get_name(c, "rename from")?.to_string();
        let to = get_name(c, "rename to")?.to_string();
        v.push((from, to));
    }
    Ok(v)
}

impl PublishRec {
    pub fn encode(&self, out: &mut Vec<u8>) -> FormatResult<()> {
        self.dir.encode_into(out);
        put_u64(out, self.base_gen);
        let under = if self.header.flags & MANIFESTF_UNDER != 0 { PUBLISH_KIND_UNDER } else { 0 };
        let rowmap2 = if self.header.flags & MANIFESTF_ROWMAP2 != 0 { PUBLISH_KIND_ROWMAP2 } else { 0 };
        let runs = if self.header.flags & MANIFESTF_RUNS != 0 { PUBLISH_KIND_RUNS } else { 0 };
        put_u8(out, self.kind as u8 | under | rowmap2 | runs);
        self.header.encode_into(out);
        put_u32(out, self.upsert.len() as u32);
        for p in &self.upsert {
            p.encode_into(out);
        }
        put_u32(out, self.retire.len() as u32);
        for r in &self.retire {
            put_u32(out, *r);
        }
        put_bytes32(out, &self.ext);
        put_u32(out, self.installs.len() as u32);
        for i in &self.installs {
            put_name(out, &i.tmp)?;
            put_u32(out, i.part_no);
            put_bytes32(out, &i.header);
            put_u64(out, i.footer_off);
            put_bytes32(out, &i.footer);
        }
        put_renames(out, &self.renames)?;
        put_u32(out, self.manifest_crc);
        Ok(())
    }

    pub fn decode(b: &[u8]) -> FormatResult<PublishRec> {
        let mut c = Cur::new(b);
        let dir = DirLoc::decode_from(&mut c)?;
        let base_gen = c.u64("PUBLISH")?;
        let k = c.u8("PUBLISH kind")?;
        let kind = PublishKind::from_u8(k & !PUBLISH_KIND_FLAGS).ok_or(FormatError::Corrupt { at: "PUBLISH kind" })?;
        if c.remaining() < MANIFEST_HEADER_LEN {
            return Err(FormatError::Truncated { at: "PUBLISH header" });
        }
        let header = ManifestHeader::decode_from(&mut c)?;
        if (k & PUBLISH_KIND_UNDER != 0) != (header.flags & MANIFESTF_UNDER != 0) {
            return Err(FormatError::Corrupt { at: "PUBLISH kind vs manifest flags" });
        }
        if (k & PUBLISH_KIND_ROWMAP2 != 0) != (header.flags & MANIFESTF_ROWMAP2 != 0) {
            return Err(FormatError::Corrupt { at: "PUBLISH kind vs manifest ROWMAP2 flag" });
        }
        if (k & PUBLISH_KIND_RUNS != 0) != (header.flags & MANIFESTF_RUNS != 0) {
            return Err(FormatError::Corrupt { at: "PUBLISH kind vs manifest RUNS flag" });
        }
        let n = c.u32("PUBLISH upsert")? as usize;
        if n > c.remaining() / PART_RECORD_LEN {
            return Err(FormatError::Truncated { at: "PUBLISH upsert" });
        }
        let mut upsert = crate::try_vec(n, "PUBLISH upsert")?;
        for _ in 0..n {
            upsert.push(PartRecord::decode_from(&mut c)?);
        }
        let n = c.u32("PUBLISH retire")? as usize;
        if n > c.remaining() / 4 {
            return Err(FormatError::Truncated { at: "PUBLISH retire" });
        }
        let mut retire = crate::try_vec(n, "PUBLISH retire")?;
        for _ in 0..n {
            retire.push(c.u32("PUBLISH retire")?);
        }
        let ext = crate::try_to_vec(get_bytes(&mut c, "PUBLISH ext")?, "PUBLISH ext")?;
        crate::manifest::ext_records(&ext)?;
        let n = c.u32("PUBLISH installs")? as usize;
        let mut installs = crate::try_vec(n.min(c.remaining() / 16), "PUBLISH installs")?;
        for _ in 0..n {
            let tmp = get_name(&mut c, "PUBLISH install tmp")?.to_string();
            let part_no = c.u32("PUBLISH install")?;
            let header = crate::try_to_vec(get_bytes(&mut c, "PUBLISH install header")?, "PUBLISH install")?;
            let footer_off = c.u64("PUBLISH install")?;
            let footer = crate::try_to_vec(get_bytes(&mut c, "PUBLISH install footer")?, "PUBLISH install")?;
            installs.push(PartInstall { tmp, part_no, header, footer_off, footer });
        }
        let renames = get_renames(&mut c)?;
        let manifest_crc = c.u32("PUBLISH crc")?;
        let rec = PublishRec { dir, base_gen, kind, header, upsert, retire, ext, installs, renames, manifest_crc };
        finish(&c, rec, "PUBLISH trailing bytes")
    }
}

/// `INSTALL`: renames only (a sections or sidecar file completed after its generation was published).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRec {
    pub dir: DirLoc,
    pub renames: Vec<(String, String)>,
}

impl InstallRec {
    pub fn encode(&self, out: &mut Vec<u8>) -> FormatResult<()> {
        self.dir.encode_into(out);
        put_renames(out, &self.renames)
    }
    pub fn decode(b: &[u8]) -> FormatResult<InstallRec> {
        let mut c = Cur::new(b);
        let dir = DirLoc::decode_from(&mut c)?;
        let renames = get_renames(&mut c)?;
        finish(&c, InstallRec { dir, renames }, "INSTALL trailing bytes")
    }
}

/// A `RECLAIM`'s catalog retirement: drop the generations below the value once `xid` is past the horizon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retire {
    /// Flag 1: a standby resolves the snapshot conflict first (a reclaim that frees a segment).
    Conflict(u64),
    /// Flag 2 (K1, OC-7 a): no conflict; a standby queues the retirement until its own horizon passes `xid`.
    Deferred(u64),
}

impl Retire {
    /// The oldest generation kept.
    pub fn keep_from(self) -> u64 {
        match self {
            Retire::Conflict(g) | Retire::Deferred(g) => g,
        }
    }
}

/// `RECLAIM`: unlink `names`; `retire_below` drops older generations from the catalog once `xid` is past the horizon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclaimRec {
    pub dir: DirLoc,
    pub xid: u32,
    pub retire_below: Option<Retire>,
    pub names: Vec<String>,
}

impl ReclaimRec {
    pub fn encode(&self, out: &mut Vec<u8>) -> FormatResult<()> {
        self.dir.encode_into(out);
        put_u32(out, self.xid);
        let (flag, below) = match self.retire_below {
            None => (0, 0),
            Some(Retire::Conflict(g)) => (1, g),
            Some(Retire::Deferred(g)) => (2, g),
        };
        put_u8(out, flag);
        put_u64(out, below);
        put_u32(out, self.names.len() as u32);
        for n in &self.names {
            put_name(out, n)?;
        }
        Ok(())
    }
    pub fn decode(b: &[u8]) -> FormatResult<ReclaimRec> {
        let mut c = Cur::new(b);
        let dir = DirLoc::decode_from(&mut c)?;
        let xid = c.u32("RECLAIM")?;
        let has = c.u8("RECLAIM")?;
        let below = c.u64("RECLAIM")?;
        let retire_below = match has {
            0 => None,
            1 => Some(Retire::Conflict(below)),
            2 => Some(Retire::Deferred(below)),
            _ => return Err(FormatError::Corrupt { at: "RECLAIM retire flag" }),
        };
        let n = c.u32("RECLAIM names")? as usize;
        let mut names = crate::try_vec(n.min(c.remaining() / 2), "RECLAIM names")?;
        for _ in 0..n {
            names.push(get_name(&mut c, "RECLAIM name")?.to_string());
        }
        finish(&c, ReclaimRec { dir, xid, retire_below, names }, "RECLAIM trailing bytes")
    }
}

/// A heap tuple as heap WAL carries it: `xl_heap_header` (`t_infomask2`, `t_infomask`, `t_hoff`) and the tuple from
/// `t_bits` on (`SizeofHeapTupleHeader` bytes shorter than the tuple).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TupleImage<'a> {
    pub infomask2: u16,
    pub infomask: u16,
    pub hoff: u8,
    pub data: &'a [u8],
}

/// `infomask2`, `infomask`, `hoff`: `SizeOfHeapHeader`.
pub const TUPLE_IMAGE_HEADER_LEN: usize = 5;

impl<'a> TupleImage<'a> {
    /// `len u32` (header + data), the header, the data: a `ROW_*` image, and one `BULK_ROWS` tuple.
    pub fn encode_into(&self, out: &mut Vec<u8>) -> FormatResult<()> {
        let len = TUPLE_IMAGE_HEADER_LEN + self.data.len();
        if len > u32::MAX as usize {
            return Err(FormatError::EncodeContract { detail: "pgrc tuple image over 4 GiB" });
        }
        put_u32(out, len as u32);
        put_u16(out, self.infomask2);
        put_u16(out, self.infomask);
        put_u8(out, self.hoff);
        out.extend_from_slice(self.data);
        Ok(())
    }

    fn decode_from(c: &mut Cur<'a>, at: &'static str) -> FormatResult<TupleImage<'a>> {
        let len = c.u32(at)? as usize;
        if len < TUPLE_IMAGE_HEADER_LEN {
            return Err(FormatError::Corrupt { at });
        }
        let (infomask2, infomask, hoff) = (c.u16(at)?, c.u16(at)?, c.u8(at)?);
        Ok(TupleImage { infomask2, infomask, hoff, data: c.take(len - TUPLE_IMAGE_HEADER_LEN, at)? })
    }
}

const ROW_HAS_OLD: u8 = 1;
const ROW_HAS_NEW: u8 = 2;

/// `ROW_DELETE` / `ROW_UPDATE` (decode only; redo skips them): a row change on the parent, written once the change's
/// tombstone won. `old` is present iff the parent's replica identity is FULL; `new` is UPDATE's (and only UPDATE's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowRec<'a> {
    pub parent: DirLoc,
    pub old: Option<TupleImage<'a>>,
    pub new: Option<TupleImage<'a>>,
}

impl<'a> RowRec<'a> {
    fn shape_ok(&self, op: u8) -> bool {
        match op {
            XLOG_PGRC_ROW_DELETE => self.new.is_none(),
            XLOG_PGRC_ROW_UPDATE => self.new.is_some(),
            _ => false,
        }
    }

    /// `op`: `XLOG_PGRC_ROW_DELETE` or `XLOG_PGRC_ROW_UPDATE` (the flag bits of `info` are ignored).
    pub fn encode(&self, op: u8, out: &mut Vec<u8>) -> FormatResult<()> {
        if !self.shape_ok(op & XLOG_PGRC_OPMASK) {
            return Err(FormatError::EncodeContract { detail: "ROW_DELETE carries no new row, ROW_UPDATE one" });
        }
        self.parent.encode_into(out);
        put_u8(out, (self.old.is_some() as u8 * ROW_HAS_OLD) | (self.new.is_some() as u8 * ROW_HAS_NEW));
        for image in [&self.old, &self.new].into_iter().flatten() {
            image.encode_into(out)?;
        }
        Ok(())
    }

    pub fn decode(op: u8, b: &'a [u8]) -> FormatResult<RowRec<'a>> {
        let mut c = Cur::new(b);
        let parent = DirLoc::decode_from(&mut c)?;
        let flags = c.u8("ROW flags")?;
        if flags & !(ROW_HAS_OLD | ROW_HAS_NEW) != 0 {
            return Err(FormatError::Corrupt { at: "ROW flags" });
        }
        let old = (flags & ROW_HAS_OLD != 0).then(|| TupleImage::decode_from(&mut c, "ROW old")).transpose()?;
        let new = (flags & ROW_HAS_NEW != 0).then(|| TupleImage::decode_from(&mut c, "ROW new")).transpose()?;
        let rec = RowRec { parent, old, new };
        if !rec.shape_ok(op & XLOG_PGRC_OPMASK) {
            return Err(FormatError::Corrupt { at: "ROW shape" });
        }
        finish(&c, rec, "ROW trailing bytes")
    }
}

/// `BULK_ROWS` (decode only; redo skips it): rows a bulk-lane load wrote into parts, `ntuples` [`TupleImage`]s back to
/// back (heap2 `MULTI_INSERT`'s `datalen u16` cannot hold a flattened row past 64 KiB, which heap would have toasted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BulkRowsRec<'a> {
    pub parent: DirLoc,
    pub ntuples: u32,
    pub tuples: &'a [u8],
}

/// `BULK_ROWS`' parent and `ntuples`: the tuples start here.
pub const BULK_ROWS_HEADER_LEN: usize = DIR_LOC_LEN + 4;

impl<'a> BulkRowsRec<'a> {
    /// The header; the caller registers `tuples` after it, so a batch is logged without a copy.
    pub fn encode_header(&self, out: &mut Vec<u8>) -> FormatResult<()> {
        if self.ntuples == 0 || self.tuples.is_empty() {
            return Err(FormatError::EncodeContract { detail: "BULK_ROWS without rows" });
        }
        self.parent.encode_into(out);
        put_u32(out, self.ntuples);
        Ok(())
    }

    pub fn encode(&self, out: &mut Vec<u8>) -> FormatResult<()> {
        self.encode_header(out)?;
        out.extend_from_slice(self.tuples);
        Ok(())
    }

    pub fn decode(b: &'a [u8]) -> FormatResult<BulkRowsRec<'a>> {
        let mut c = Cur::new(b);
        let parent = DirLoc::decode_from(&mut c)?;
        let ntuples = c.u32("BULK_ROWS")?;
        let tuples = c.take(c.remaining(), "BULK_ROWS")?;
        if ntuples == 0 || tuples.is_empty() {
            return Err(FormatError::Corrupt { at: "BULK_ROWS without rows" });
        }
        let rec = BulkRowsRec { parent, ntuples, tuples };
        let mut off = 0;
        for _ in 0..ntuples {
            off = rec.image_at(off)?.1;
        }
        match off == tuples.len() {
            true => Ok(rec),
            false => Err(FormatError::Corrupt { at: "BULK_ROWS trailing bytes" }),
        }
    }

    /// The tuple at byte `off` of `tuples` and the offset of the next one.
    pub fn image_at(&self, off: usize) -> FormatResult<(TupleImage<'a>, usize)> {
        let mut c = Cur::new(self.tuples.get(off..).ok_or(FormatError::Corrupt { at: "BULK_ROWS tuple" })?);
        let image = TupleImage::decode_from(&mut c, "BULK_ROWS tuple")?;
        Ok((image, off + c.off()))
    }
}

/// The record's name for `pg_waldump` / `pg_get_wal_resource_managers` identify.
pub fn identify(info: u8) -> Option<&'static str> {
    Some(match info & XLOG_PGRC_OPMASK {
        XLOG_PGRC_DIR_CREATE => "DIR_CREATE",
        XLOG_PGRC_FILE_WRITE => "FILE_WRITE",
        XLOG_PGRC_PUBLISH => "PUBLISH",
        XLOG_PGRC_INSTALL => "INSTALL",
        XLOG_PGRC_RECLAIM => "RECLAIM",
        XLOG_PGRC_TRUNCATE => "TRUNCATE",
        XLOG_PGRC_ROW_DELETE => "ROW_DELETE",
        XLOG_PGRC_ROW_UPDATE => "ROW_UPDATE",
        XLOG_PGRC_BULK_ROWS => "BULK_ROWS",
        XLOG_PGRC_FILE_CREATE => "FILE_CREATE",
        XLOG_PGRC_FILE_REF => "FILE_REF",
        XLOG_PGRC_ROWID_RESERVE => "ROWID_RESERVE",
        XLOG_PGRC_ROWID_EXTENTS => "ROWID_EXTENTS",
        XLOG_PGRC_ROWID_SET => "ROWID_SET",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dirlayout::{manifest_file_name, sections_file_name, temp_file_name};

    const D: DirLoc = DirLoc { spc: 1663, db: 5, relnumber: 16384 };

    fn header() -> ManifestHeader {
        crate::tests::golden_sample_manifest().header
    }

    #[test]
    fn file_write_round_trips_and_bounds_the_chunk() {
        let name = temp_file_name(744, 3);
        let bytes = vec![7u8; FILE_WRITE_MAX_CHUNK];
        let rec = FileWriteRec { dir: D, name: &name, off: 262_144, bytes: &bytes };
        let mut out = Vec::new();
        rec.encode(&mut out).unwrap();
        assert!(out.len() <= FILE_WRITE_MAX_CHUNK + 64);
        assert_eq!(FileWriteRec::decode(&out).unwrap(), rec);
        let big = vec![0u8; FILE_WRITE_MAX_CHUNK + 1];
        assert!(FileWriteRec { dir: D, name: &name, off: 0, bytes: &big }.encode(&mut Vec::new()).is_err());
    }

    #[test]
    fn names_that_do_not_classify_are_refused_both_ways() {
        for bad in ["../x", "a/b", "part-1.pgrc/..", "postgresql.conf"] {
            assert!(FileWriteRec { dir: D, name: bad, off: 0, bytes: &[] }.encode(&mut Vec::new()).is_err(), "{bad}");
        }
        let mut out = Vec::new();
        FileWriteRec { dir: D, name: "part-1.pgrc", off: 0, bytes: &[] }.encode(&mut out).unwrap();
        let at = DIR_LOC_LEN + 8 + 2;
        out[at..at + 6].copy_from_slice(b"../x/y");
        assert!(FileWriteRec::decode(&out).is_err());
    }

    #[test]
    fn publish_round_trips() {
        let m = crate::tests::golden_sample_manifest();
        let rec = PublishRec {
            dir: D,
            base_gen: 6,
            kind: PublishKind::Bulk,
            header: header(),
            upsert: m.parts.clone(),
            retire: vec![9, 11],
            ext: {
                let mut e = Vec::new();
                crate::manifest::put_ext_record(&mut e, 1, b"segments");
                e
            },
            installs: vec![PartInstall { tmp: temp_file_name(744, 0), part_no: 3, header: vec![1; 64], footer_off: 4096, footer: vec![2; 96] }],
            renames: vec![(temp_file_name(744, 1), sections_file_name(7))],
            manifest_crc: 0xDEAD_BEEF,
        };
        let mut out = Vec::new();
        rec.encode(&mut out).unwrap();
        assert_eq!(PublishRec::decode(&out).unwrap(), rec);
        out.push(0);
        assert!(PublishRec::decode(&out).is_err());
        assert!(PublishRec::decode(&out[..out.len() - 6]).is_err());
    }

    /// SEAL-OFFHEAD: a stacked generation's PUBLISH (header flag MANIFESTF_UNDER) sets the kind byte's top bit, which a
    /// binary without EXT_UNDER refuses as an unknown kind; this one round-trips it and refuses a bit that disagrees
    /// with the header's flag.
    #[test]
    fn a_stacked_publish_sets_the_kind_bit() {
        let at = 12 + 8; // the kind byte follows the dir and the base generation
        let mut rec = PublishRec { dir: D, base_gen: 6, kind: PublishKind::Seal, header: header(), upsert: Vec::new(), retire: Vec::new(), ext: Vec::new(), installs: Vec::new(), renames: Vec::new(), manifest_crc: 1 };
        let mut plain = Vec::new();
        rec.encode(&mut plain).unwrap();
        assert_eq!(plain[at], PublishKind::Seal as u8);
        rec.header.flags |= crate::manifest::MANIFESTF_UNDER;
        let mut out = Vec::new();
        rec.encode(&mut out).unwrap();
        assert_eq!(out[at], PublishKind::Seal as u8 | PUBLISH_KIND_UNDER);
        assert_eq!(PublishRec::decode(&out).unwrap(), rec);
        assert!(PublishKind::from_u8(out[at]).is_none(), "an older binary's decode refuses the kind");
        out[at] &= !PUBLISH_KIND_UNDER;
        assert!(PublishRec::decode(&out).is_err(), "the flag without the bit");
        plain[at] |= PUBLISH_KIND_UNDER;
        assert!(PublishRec::decode(&plain).is_err(), "the bit without the flag");
    }

    #[test]
    fn install_reclaim_dir_and_logical_records_round_trip() {
        let i = InstallRec { dir: D, renames: vec![(temp_file_name(1, 2), manifest_file_name(3))] };
        let mut out = Vec::new();
        i.encode(&mut out).unwrap();
        assert_eq!(InstallRec::decode(&out).unwrap(), i);

        for (retire_below, flag) in [(None, 0u8), (Some(Retire::Conflict(12)), 1), (Some(Retire::Deferred(12)), 2)] {
            let r = ReclaimRec { dir: D, xid: 900, retire_below, names: vec![temp_file_name(1, 2), "part-4.pgrc".into()] };
            let mut out = Vec::new();
            r.encode(&mut out).unwrap();
            assert_eq!(ReclaimRec::decode(&out).unwrap(), r);
            // The flag byte follows the dir and the xid; values 0 and 1 are the pre-K1 encoding.
            let at = out.len() - 8 - 4 - r.names.iter().map(|n| 2 + n.len()).sum::<usize>() - 1;
            assert_eq!(out[at], flag);
            if flag == 2 {
                out[at] = 3;
                assert!(ReclaimRec::decode(&out).is_err(), "an unknown retire flag");
            }
        }

        let mut out = Vec::new();
        encode_dir(D, &mut out);
        assert_eq!(decode_dir(&out).unwrap(), D);
    }

    #[test]
    fn every_record_has_a_name() {
        for info in [0x00u8, 0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xA0, 0xB0, 0xC0, 0xD0] {
            assert!(identify(info).is_some());
        }
        assert_eq!(identify(0xA0), Some("FILE_REF"));
        assert_eq!(identify(0xB0), Some("ROWID_RESERVE"));
        assert_eq!(XLOG_PGRC_ROWID_RESERVE, 0xB0, "IX0's opcode (main's ruling, 10-03)");
        assert_eq!(identify(0xC0), Some("ROWID_EXTENTS"));
        assert_eq!(XLOG_PGRC_ROWID_EXTENTS, 0xC0, "IX0 DV-1 (main 10-04: not 0xB1, the low nibble is XLR_INFO_MASK)");
        assert_eq!(XLOG_PGRC_ROWID_EXTENTS & !XLOG_PGRC_OPMASK, 0);
        assert_eq!(identify(0xD0), Some("ROWID_SET"));
        assert_eq!(XLOG_PGRC_ROWID_SET, 0xD0, "IX0 L4 (main 10-05: reserved for IX0)");
        for reserved in [0xE0u8, 0xF0] {
            assert_eq!(identify(reserved), None);
        }
    }

    #[test]
    fn rowid_reserve_round_trips_and_refuses_bad_records() {
        for high in [crate::rowid::ROWID_FIRST, 1 << 33, crate::rowid::ROWID_LIMIT] {
            let r = RowidReserveRec { dir: D, high };
            let mut out = Vec::new();
            r.encode(&mut out);
            assert_eq!(out.len(), DIR_LOC_LEN + 8, "a tiny record");
            assert_eq!(&out[DIR_LOC_LEN..], &high.to_le_bytes());
            assert_eq!(RowidReserveRec::decode(&out).unwrap(), r);
            assert!(RowidReserveRec::decode(&out[..out.len() - 1]).is_err(), "short");
            out.push(0);
            assert!(RowidReserveRec::decode(&out).is_err(), "trailing bytes");
        }
        let mut out = Vec::new();
        RowidReserveRec { dir: D, high: crate::rowid::ROWID_LIMIT + 1 }.encode(&mut out);
        assert!(RowidReserveRec::decode(&out).is_err(), "past the ID space");
    }

    #[test]
    fn rowid_extents_round_trip_and_cap() {
        use crate::rowid::{SegExtent, ROWID_FIRST, SEGEXT_MAX_ENTRIES, SEG_EXTENT_IDS};
        let e = |part_no: u32, idx: u32, k: u64| SegExtent { part_no, idx, base: ROWID_FIRST + k * SEG_EXTENT_IDS };
        for table in [vec![], vec![e(4, 0, 0), e(4, 1, 3), e(7, 0, 1)]] {
            let r = RowidExtentsRec { dir: D, table };
            let mut out = Vec::new();
            r.encode(&mut out).unwrap();
            assert_eq!(out.len(), DIR_LOC_LEN + 12 + 16 * r.table.len());
            assert_eq!(RowidExtentsRec::decode(&out).unwrap(), r);
            assert!(RowidExtentsRec::decode(&out[..out.len() - 1]).is_err(), "short");
            out.push(0);
            assert!(RowidExtentsRec::decode(&out).is_err(), "trailing bytes");
        }
        let full = RowidExtentsRec { dir: D, table: (0..SEGEXT_MAX_ENTRIES as u64).map(|k| e(k as u32, 0, k)).collect() };
        let mut out = Vec::new();
        full.encode(&mut out).unwrap();
        assert!(out.len() <= 65536, "{} bytes at the cap: under 64 KB", out.len());
        let mut over = full.clone();
        over.table.push(e(u32::MAX, 0, SEGEXT_MAX_ENTRIES as u64));
        assert!(over.encode(&mut Vec::new()).is_err(), "past the hard cap");
        assert!(RowidExtentsRec { dir: D, table: vec![e(4, 1, 0)] }.encode(&mut Vec::new()).is_err(), "a malformed table");
    }

    #[test]
    fn file_ref_round_trips_and_is_small() {
        let name = temp_file_name(744, 3);
        assert_eq!(name.len(), 15); // `tmp-744-3.pgrct`
        let long = "tmp-18446744073-4294967.pgrct";
        for (name, masks) in [
            (name.as_str(), NO_MASKS),
            (name.as_str(), [(0, 64), (0, 0)]),
            (long, [(0, 64), (17_000_000, 96)]),
        ] {
            let rec = FileRefRec { dir: D, name, len: 17_000_096, crc: 0x1234_5678, masks };
            let mut out = Vec::new();
            rec.encode(&mut out).unwrap();
            assert!(out.len() <= 96 - 24 + name.len(), "{} bytes for a {}-byte name", out.len(), name.len());
            assert_eq!(FileRefRec::decode(&out).unwrap(), rec);
            out.push(0);
            assert!(FileRefRec::decode(&out).is_err());
            assert!(FileRefRec::decode(&out[..out.len() - 2]).is_err());
        }
    }

    #[test]
    fn file_ref_masks_must_be_sorted_disjoint_and_inside() {
        let bad: [SmallMasks; 6] = [
            [(4096, 96), (0, 64)],         // out of order
            [(0, 64), (32, 64)],           // overlapping
            [(0, 64), (1000, 96)],         // past len
            [(0, 0), (0, 64)],             // a used slot after an unused one
            [(u64::MAX, 2), (0, 0)],       // overflow
            [(0, 0), (5, 0)],              // an unused slot that is not zero
        ];
        for masks in bad {
            let rec = FileRefRec { dir: D, name: "part-1.pgrc", len: 1024, crc: 0, masks };
            assert!(rec.encode(&mut Vec::new()).is_err(), "{masks:?}");
            // The decoder refuses the same masks written by hand.
            let mut out = Vec::new();
            FileRefRec { dir: D, name: "part-1.pgrc", len: 1024, crc: 0, masks: NO_MASKS }.encode(&mut out).unwrap();
            out.pop();
            let used: Vec<_> = masks.iter().filter(|m| *m != &(0, 0)).collect();
            put_u8(&mut out, used.len().max(1) as u8);
            for m in masks.iter().take(used.len().max(1)) {
                put_u64(&mut out, m.0);
                put_u32(&mut out, m.1);
            }
            assert!(FileRefRec::decode(&out).is_err(), "{masks:?}");
        }
        // Adjacent masks and a mask ending at len are fine.
        let ok = FileRefRec { dir: D, name: "part-1.pgrc", len: 1024, crc: 0, masks: [(0, 64), (64, 960)] };
        let mut out = Vec::new();
        ok.encode(&mut out).unwrap();
        assert_eq!(FileRefRec::decode(&out).unwrap(), ok);
        // More than FILE_REF_MAX_MASKS on the wire is corrupt.
        let mut out = Vec::new();
        FileRefRec { dir: D, name: "part-1.pgrc", len: 1024, crc: 0, masks: NO_MASKS }.encode(&mut out).unwrap();
        *out.last_mut().unwrap() = 3;
        for i in 0..3u64 {
            put_u64(&mut out, i * 10);
            put_u32(&mut out, 1);
        }
        assert!(FileRefRec::decode(&out).is_err());
    }

    fn img(data: &[u8]) -> TupleImage<'_> {
        TupleImage { infomask2: 3, infomask: 0x0802, hoff: 24, data }
    }

    #[test]
    fn row_records_round_trip() {
        let big = vec![0x5Au8; 1 << 20];
        let shapes = [
            (XLOG_PGRC_ROW_DELETE, None, None),
            (XLOG_PGRC_ROW_DELETE, Some(img(b"old row")), None),
            (XLOG_PGRC_ROW_DELETE, Some(img(b"")), None),
            (XLOG_PGRC_ROW_UPDATE, None, Some(img(b"new row"))),
            (XLOG_PGRC_ROW_UPDATE, Some(img(&big)), Some(img(b""))),
            (XLOG_PGRC_ROW_UPDATE, Some(img(b"o")), Some(img(&big))),
        ];
        for (op, old, new) in shapes {
            let r = RowRec { parent: D, old, new };
            let mut out = Vec::new();
            r.encode(op, &mut out).unwrap();
            assert_eq!(RowRec::decode(op, &out).unwrap(), r);
        }
        let mut tuples = Vec::new();
        for i in [img(b"abc"), img(&big), img(b"")] {
            i.encode_into(&mut tuples).unwrap();
        }
        let b = BulkRowsRec { parent: D, ntuples: 3, tuples: &tuples };
        let mut out = Vec::new();
        b.encode(&mut out).unwrap();
        let d = BulkRowsRec::decode(&out).unwrap();
        assert_eq!(d, b);
        let (t0, o) = d.image_at(0).unwrap();
        let (t1, o) = d.image_at(o).unwrap();
        let (t2, o) = d.image_at(o).unwrap();
        assert_eq!((t0, t1.data.len(), t2, o), (img(b"abc"), 1 << 20, img(b""), tuples.len()), "a row past 64 KiB is one tuple");
        let mut head = Vec::new();
        b.encode_header(&mut head).unwrap();
        assert_eq!(head, out[..BULK_ROWS_HEADER_LEN]);
    }

    #[test]
    fn malformed_row_records_are_refused() {
        let mut out = Vec::new();
        assert!(RowRec { parent: D, old: None, new: Some(img(b"n")) }.encode(XLOG_PGRC_ROW_DELETE, &mut out).is_err());
        assert!(RowRec { parent: D, old: Some(img(b"o")), new: None }.encode(XLOG_PGRC_ROW_UPDATE, &mut out).is_err());
        assert!(RowRec { parent: D, old: None, new: None }.encode(XLOG_PGRC_BULK_ROWS, &mut out).is_err());

        let mut upd = Vec::new();
        RowRec { parent: D, old: Some(img(b"old")), new: Some(img(b"new")) }.encode(XLOG_PGRC_ROW_UPDATE, &mut upd).unwrap();
        // An UPDATE read as a DELETE, and a DELETE without its flags' images.
        assert!(RowRec::decode(XLOG_PGRC_ROW_DELETE, &upd).is_err());
        let mut del = Vec::new();
        RowRec { parent: D, old: None, new: None }.encode(XLOG_PGRC_ROW_DELETE, &mut del).unwrap();
        assert!(RowRec::decode(XLOG_PGRC_ROW_UPDATE, &del).is_err());
        // Trailing bytes; a truncated image; an unknown flag bit; a set flag with a zero-length image.
        let mut t = upd.clone();
        t.push(0);
        assert!(RowRec::decode(XLOG_PGRC_ROW_UPDATE, &t).is_err());
        assert!(RowRec::decode(XLOG_PGRC_ROW_UPDATE, &upd[..upd.len() - 1]).is_err());
        let mut f = del.clone();
        f[DIR_LOC_LEN] = 4;
        assert!(RowRec::decode(XLOG_PGRC_ROW_DELETE, &f).is_err());
        let mut z = del.clone();
        z[DIR_LOC_LEN] = ROW_HAS_OLD;
        z.extend_from_slice(&0u32.to_le_bytes());
        assert!(RowRec::decode(XLOG_PGRC_ROW_DELETE, &z).is_err());
        let mut z4 = del;
        z4[DIR_LOC_LEN] = ROW_HAS_OLD;
        z4.extend_from_slice(&4u32.to_le_bytes());
        z4.extend_from_slice(&[0; 4]);
        assert!(RowRec::decode(XLOG_PGRC_ROW_DELETE, &z4).is_err());

        assert!(BulkRowsRec { parent: D, ntuples: 0, tuples: b"x" }.encode(&mut Vec::new()).is_err());
        assert!(BulkRowsRec { parent: D, ntuples: 1, tuples: b"" }.encode(&mut Vec::new()).is_err());
        let mut b = Vec::new();
        encode_dir(D, &mut b);
        b.extend_from_slice(&1u32.to_le_bytes());
        assert!(BulkRowsRec::decode(&b).is_err());
        assert!(BulkRowsRec::decode(&b[..DIR_LOC_LEN + 2]).is_err());
        // ntuples past the tuples, short of them (trailing bytes), a truncated tuple, a tuple shorter than its header.
        let mut one = Vec::new();
        img(b"row").encode_into(&mut one).unwrap();
        for (n, tuples) in [(2, one.clone()), (1, [one.clone(), one.clone()].concat()), (1, one[..one.len() - 1].to_vec()), (1, 4u32.to_le_bytes().to_vec())] {
            let mut r = Vec::new();
            BulkRowsRec { parent: D, ntuples: n, tuples: &tuples }.encode(&mut r).unwrap();
            assert!(BulkRowsRec::decode(&r).is_err(), "{n} {tuples:?}");
        }
    }
}
