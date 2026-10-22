//! Parts by reference (WR): the names of the spool that keeps a referenced file readable while its WAL is kept, the
//! archive names of referenced files, and the crc a `FILE_REF` carries (the file with its publish-time patch regions
//! skipped). No I/O.

use crate::dirlayout::classify;
use crate::wal::{used_masks, DirLoc, SmallMasks};

/// The spool directory, relative to PGDATA.
pub const SPOOL_DIR: &str = "pg_pgrc_spool";

/// WALMODE: markers of unlogged tables written with `pgrust.pgrc_unlogged_sync=off`, relative to PGDATA. Each names
/// a table directory and holds the boot id it was written under; a start on another boot resets the table.
pub const UNSYNCED_DIR: &str = "pg_pgrc_unsynced";

/// The marker of the table directory `dir` (relative to PGDATA): its path with `/` as `%`.
pub fn unsynced_name(dir: &str) -> String {
    dir.replace('/', "%")
}

/// The table directory a marker names (`None`: not a marker, e.g. a `.tmp` left by a crash).
pub fn parse_unsynced_name(n: &str) -> Option<String> {
    if !n.contains('%') || n.contains('/') || n.ends_with(".tmp") {
        return None;
    }
    Some(n.replace('%', "/"))
}

const PENDING_PREFIX: &str = "pending-";
const ARCHIVE_SUFFIX: &str = ".pgrc";

fn is_upper_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
}

/// The spool entry of the reference whose record starts at `lsn`: 16 upper-case hex digits (WAL file name style).
pub fn spool_name(lsn: u64) -> String {
    format!("{lsn:016X}")
}

pub fn parse_spool_name(n: &str) -> Option<u64> {
    if !is_upper_hex(n, 16) {
        return None;
    }
    u64::from_str_radix(n, 16).ok()
}

/// The spool link made before a reference is logged: unique per table file, so a crash leaves at most one per file.
pub fn pending_name(dir: DirLoc, file: &str) -> String {
    format!("{PENDING_PREFIX}{}-{}-{}-{file}", dir.spc, dir.db, dir.relnumber)
}

fn canonical_u32(s: &str) -> Option<u32> {
    let v: u32 = s.parse().ok()?;
    (v.to_string() == s).then_some(v)
}

pub fn parse_pending_name(n: &str) -> Option<(DirLoc, &str)> {
    let rest = n.strip_prefix(PENDING_PREFIX)?;
    let mut it = rest.splitn(4, '-');
    let spc = canonical_u32(it.next()?)?;
    let db = canonical_u32(it.next()?)?;
    let relnumber = canonical_u32(it.next()?)?;
    let file = it.next()?;
    classify(file)?;
    Some((DirLoc { spc, db, relnumber }, file))
}

/// The archive name of the file referenced at `lsn`: `<segment file name>.<offset in the segment, 8 hex>.pgrc`, so it
/// sorts beside (and is archived before) the segment that holds its reference.
pub fn archive_name(seg_file_name: &str, lsn: u64, seg_size: u64) -> String {
    format!("{seg_file_name}.{:08X}{ARCHIVE_SUFFIX}", lsn % seg_size)
}

/// `(segment file name, offset)` of an archive name.
pub fn parse_archive_name(n: &str) -> Option<(&str, u32)> {
    let stem = n.strip_suffix(ARCHIVE_SUFFIX)?;
    let (seg, off) = stem.split_once('.')?;
    if !is_upper_hex(seg, 24) || !is_upper_hex(off, 8) {
        return None;
    }
    Some((seg, u32::from_str_radix(off, 16).ok()?))
}

/// The crc32c of a file with the mask ranges skipped, fed with the file's bytes in order. The masks are fixed before
/// the first byte, so a writer can compute it while it writes.
#[derive(Debug, Clone)]
pub struct MaskedCrc {
    masks: SmallMasks,
    crc: u32,
    pos: u64,
}

impl MaskedCrc {
    pub fn new(masks: SmallMasks) -> Self {
        MaskedCrc { masks, crc: ::crc32c::CRC32C_INIT, pos: 0 }
    }

    /// The bytes at `off`, which must be where the previous update ended (sequential writes only).
    pub fn update(&mut self, off: u64, bytes: &[u8]) {
        debug_assert_eq!(off, self.pos, "MaskedCrc takes sequential bytes");
        let end = off + bytes.len() as u64;
        let mut at = off;
        let masks = self.masks;
        for &(moff, mlen) in masks.iter().filter(|m| m.1 != 0) {
            let mend = moff + mlen as u64;
            if mend <= at || moff >= end {
                continue;
            }
            if moff > at {
                self.feed(&bytes[(at - off) as usize..(moff - off) as usize]);
            }
            at = mend.min(end);
        }
        if at < end {
            self.feed(&bytes[(at - off) as usize..]);
        }
        self.pos = end;
    }

    /// K1-MEM: take `masks` in place of the masks in force, which it must keep: it may only add ranges at or past the
    /// bytes fed so far, so the crc is the one `masks` from the start gives. A streamed seal learns its footer's offset
    /// at the end. The ranges in force stay sorted and disjoint ([`MaskedCrc::update`] walks them in order).
    pub fn widen_masks(&mut self, masks: SmallMasks) -> Result<(), &'static str> {
        let live = |m: &SmallMasks| {
            let mut v: Vec<(u64, u32)> = Vec::with_capacity(crate::wal::FILE_REF_MAX_MASKS);
            v.extend(m.iter().copied().filter(|r| r.1 != 0));
            v
        };
        let (old, new) = (live(&self.masks), live(&masks));
        if old.iter().any(|r| !new.contains(r)) {
            return Err("a crc mask in force was dropped");
        }
        if new.iter().any(|r| !old.contains(r) && r.0 < self.pos) {
            return Err("a crc mask added over bytes already fed");
        }
        if new.windows(2).any(|w| w[0].0 + w[0].1 as u64 > w[1].0) {
            return Err("crc masks out of order or overlapping");
        }
        self.masks = masks;
        Ok(())
    }

    fn feed(&mut self, b: &[u8]) {
        self.crc = ::crc32c::pg_comp_crc32c(self.crc, b);
    }

    /// Bytes seen so far.
    pub fn len(&self) -> u64 {
        self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.pos == 0
    }

    pub fn finish(&self) -> u32 {
        ::crc32c::fin_crc32c(self.crc)
    }
}

/// The masked crc of a whole file. The masks are assumed well formed for `bytes.len()` (`wal::used_masks`); a range
/// past the end skips nothing beyond it.
pub fn masked_crc(bytes: &[u8], masks: &SmallMasks) -> u32 {
    let mut c = MaskedCrc::new(*masks);
    c.update(0, bytes);
    c.finish()
}

/// `masked_crc` for masks that come from outside (a record): `None` when they do not fit the file.
pub fn checked_masked_crc(bytes: &[u8], masks: &SmallMasks) -> Option<u32> {
    used_masks(masks, bytes.len() as u64)?;
    Some(masked_crc(bytes, masks))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dirlayout::temp_file_name;
    use crate::wal::NO_MASKS;

    const D: DirLoc = DirLoc { spc: 1663, db: 5, relnumber: 16384 };

    #[test]
    fn unsynced_markers_round_trip() {
        let d = "pg_tblspc/16400/PG_18_202406281/5/pgrc_16390";
        assert_eq!(parse_unsynced_name(&unsynced_name(d)).as_deref(), Some(d));
        assert_eq!(unsynced_name("base/5/pgrc_7"), "base%5%pgrc_7");
        assert_eq!(parse_unsynced_name("base%5%pgrc_7.tmp"), None);
        assert_eq!(parse_unsynced_name("README"), None);
    }

    #[test]
    fn spool_names_round_trip() {
        for lsn in [0u64, 1, 0x0000_0001_2345_6789, u64::MAX] {
            let n = spool_name(lsn);
            assert_eq!(n.len(), 16);
            assert_eq!(parse_spool_name(&n), Some(lsn));
        }
        for bad in ["", "0000000123456789a", "000000012345678g", "00000001234567890", "000000012345678a"] {
            assert_eq!(parse_spool_name(bad), None, "{bad}");
        }
        assert_eq!(parse_spool_name(&pending_name(D, "part-1.pgrc")), None);
    }

    #[test]
    fn pending_names_round_trip_and_never_collide() {
        let files = [temp_file_name(744, 0), temp_file_name(744, 12), "part-3.pgrc".to_string()];
        let dirs = [D, DirLoc { spc: 1663, db: 51, relnumber: 6384 }, DirLoc { spc: 16631, db: 5, relnumber: 16384 }, DirLoc { spc: 0, db: 0, relnumber: 0 }];
        let mut seen = std::collections::HashSet::new();
        for d in dirs {
            for f in &files {
                let n = pending_name(d, f);
                assert_eq!(parse_pending_name(&n), Some((d, f.as_str())));
                assert!(seen.insert(n));
            }
        }
        for bad in ["pending-1663-5-16384-", "pending-1663-5-16384-../x", "pending-01663-5-16384-part-1.pgrc", "pending-1663-5-part-1.pgrc", "part-1.pgrc"] {
            assert_eq!(parse_pending_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn archive_names_round_trip() {
        let seg = "000000010000000200000003";
        let n = archive_name(seg, 0x2_0300_1234, 16 << 20);
        assert_eq!(n, "000000010000000200000003.00001234.pgrc");
        assert_eq!(parse_archive_name(&n), Some((seg, 0x1234)));
        let n = archive_name(seg, 0x2_03FF_FFF8, 16 << 20);
        assert_eq!(parse_archive_name(&n), Some((seg, 0xFF_FFF8)));
        for bad in [seg, "000000010000000200000003.partial", "00000001000000020000003.00001234.pgrc", "000000010000000200000003.0001234.pgrc"] {
            assert_eq!(parse_archive_name(bad), None, "{bad}");
        }
    }

    fn image(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i.wrapping_mul(2_654_435_761) >> 7) as u8).collect()
    }

    #[test]
    fn unmasked_crc_is_the_plain_crc() {
        let b = image(10_000);
        assert_eq!(masked_crc(&b, &NO_MASKS), crate::wire::crc32c(&b));
    }

    #[test]
    fn any_write_split_gives_the_same_crc() {
        let b = image(5_000);
        let masks: SmallMasks = [(0, 64), (4_904, 96)];
        let want = masked_crc(&b, &masks);
        let mut expect = Vec::new();
        expect.extend_from_slice(&b[64..4_904]);
        assert_eq!(want, crate::wire::crc32c(&expect));
        for step in [1usize, 7, 63, 64, 65, 96, 1000, 4_904, 4_999, 5_000] {
            let mut c = MaskedCrc::new(masks);
            let mut off = 0;
            while off < b.len() {
                let end = (off + step).min(b.len());
                c.update(off as u64, &b[off..end]);
                off = end;
            }
            assert_eq!(c.len(), 5_000);
            assert_eq!(c.finish(), want, "step {step}");
        }
        // Irregular splits that start and end inside the masks.
        let cuts = [0usize, 10, 64, 70, 3_000, 4_900, 4_950, 4_999, 5_000];
        let mut c = MaskedCrc::new(masks);
        for w in cuts.windows(2) {
            c.update(w[0] as u64, &b[w[0]..w[1]]);
        }
        assert_eq!(c.finish(), want);
    }

    #[test]
    fn a_patch_inside_a_mask_keeps_the_crc_and_a_flip_outside_changes_it() {
        let b = image(5_000);
        let masks: SmallMasks = [(0, 64), (4_904, 96)];
        let want = masked_crc(&b, &masks);
        let mut patched = b.clone();
        patched[..64].fill(0xAB);
        patched[4_904..].fill(0xCD);
        assert_eq!(masked_crc(&patched, &masks), want);
        for at in [64usize, 65, 2_500, 4_903] {
            let mut flipped = b.clone();
            flipped[at] ^= 1;
            assert_ne!(masked_crc(&flipped, &masks), want, "{at}");
        }
        assert_eq!(checked_masked_crc(&b, &masks), Some(want));
        assert_eq!(checked_masked_crc(&b[..4_000], &masks), None);
    }
}
