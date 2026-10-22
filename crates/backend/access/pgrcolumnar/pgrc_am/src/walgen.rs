//! The generation stamp WAL carries (D3150; user ruling 2026-10-07: an error for now, never a silent empty or stale
//! table).
//!
//! A table's part files, manifests and sections are written outside WAL, so a node that rebuilds a cluster from a base
//! backup and WAL (a streaming standby, a PITR target) has only the files the backup copied. Every publish logs one
//! small record, `XLOG_SMGR_TABLE_DIR_GEN` (storage_xlog): the generation it published, its publisher, and the
//! generation it chained on with that one's publisher. Redo writes the record into the table directory as
//! [`WALGEN_FILE_NAME`] (creating the directory when the backup had none), replacing the previous stamp: publishes of a
//! table are serialized, so the newer record's base supersedes every older entry. At load the catalog judges the stamp
//! against the files: an entry whose publisher committed and whose generation is above the directory's effective one
//! names a generation this node replayed and has no files for.

/// The record's info byte under RM_SMGR_ID (storage_xlog's `XLOG_SMGR_TABLE_DIR_GEN`; pgrust-only, beside 0x70
/// CREATE_TABLE_DIR). The publisher (pgrc_am) and redo (storage_xlog) both read it from here.
pub const SMGR_INFO: u8 = 0x80;

/// The stamp's file in a table directory.
pub const WALGEN_FILE_NAME: &str = "WALGEN";
/// Its rename source.
pub const WALGEN_TMP_FILE_NAME: &str = "WALGEN.tmp";

const MAGIC: &str = "pgrc-walgen 1";

/// One stamp: the published generation and its publisher, then the base it chained on and the base's publisher
/// (`base_gen` 0 = the first generation of the table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalGen {
    pub gen: u64,
    pub publisher_fxid: u64,
    pub base_gen: u64,
    pub base_publisher_fxid: u64,
}

impl WalGen {
    /// The record's payload after the relation locator: four little-endian u64.
    pub const PAYLOAD_LEN: usize = 32;

    pub fn payload(&self) -> [u8; Self::PAYLOAD_LEN] {
        let mut b = [0u8; Self::PAYLOAD_LEN];
        b[0..8].copy_from_slice(&self.gen.to_le_bytes());
        b[8..16].copy_from_slice(&self.publisher_fxid.to_le_bytes());
        b[16..24].copy_from_slice(&self.base_gen.to_le_bytes());
        b[24..32].copy_from_slice(&self.base_publisher_fxid.to_le_bytes());
        b
    }

    pub fn from_payload(b: &[u8]) -> Option<WalGen> {
        if b.len() < Self::PAYLOAD_LEN {
            return None;
        }
        let u = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        Some(WalGen { gen: u(0), publisher_fxid: u(8), base_gen: u(16), base_publisher_fxid: u(24) })
    }

    /// The file's text: the magic line, then one `<gen> <publisher fxid>` line per entry (the base first, when any).
    pub fn encode(&self) -> String {
        let mut s = String::with_capacity(64); // alloc: Cold
        s.push_str(MAGIC);
        s.push('\n');
        if self.base_gen != 0 {
            s.push_str(&format!("{} {}\n", self.base_gen, self.base_publisher_fxid)); // alloc: Cold
        }
        s.push_str(&format!("{} {}\n", self.gen, self.publisher_fxid)); // alloc: Cold
        s
    }

    /// The `(generation, publisher fxid)` entries of a stamp file; `None` for bytes that are not one.
    pub fn decode_entries(bytes: &[u8]) -> Option<Vec<(u64, u64)>> {
        let text = std::str::from_utf8(bytes).ok()?;
        let mut lines = text.lines();
        if lines.next()? != MAGIC {
            return None;
        }
        let mut out = Vec::new(); // alloc: Cold
        for l in lines {
            let (g, x) = l.split_once(' ')?;
            out.push((g.parse().ok()?, x.parse().ok()?));
        }
        Some(out)
    }
}

/// The generation an entry list says this directory lacks: the highest `gen` among entries whose publisher
/// `committed`, when it is above `effective` (the directory's newest committed generation, 0 = none).
pub fn missing_generation(entries: &[(u64, u64)], effective: u64, committed: &dyn Fn(u64) -> bool) -> Option<u64> {
    entries.iter().filter(|(g, x)| *g > effective && committed(*x)).map(|(g, _)| *g).max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_round_trips_through_payload_and_file() {
        let w = WalGen { gen: 7, publisher_fxid: 900, base_gen: 6, base_publisher_fxid: 850 };
        assert_eq!(WalGen::from_payload(&w.payload()), Some(w));
        assert_eq!(WalGen::decode_entries(w.encode().as_bytes()), Some(vec![(6, 850), (7, 900)]));
        let first = WalGen { gen: 1, publisher_fxid: 740, base_gen: 0, base_publisher_fxid: 0 };
        assert_eq!(WalGen::decode_entries(first.encode().as_bytes()), Some(vec![(1, 740)]));
        assert_eq!(WalGen::decode_entries(b"garbage\n1 2\n"), None);
        assert_eq!(WalGen::from_payload(&[0u8; 31]), None);
    }

    #[test]
    fn a_committed_generation_above_the_files_is_missing() {
        let committed = |x: u64| x != 900; // 900 aborted
        // the base committed and is here, the newer publisher aborted: nothing missing
        assert_eq!(missing_generation(&[(6, 850), (7, 900)], 6, &committed), None);
        // the backup stopped at generation 5: the committed base 6 is missing
        assert_eq!(missing_generation(&[(6, 850), (7, 900)], 5, &committed), Some(6));
        // no directory at all (effective 0), first generation committed
        assert_eq!(missing_generation(&[(1, 740)], 0, &committed), Some(1));
        // every generation present (a crash-recovered primary)
        assert_eq!(missing_generation(&[(1, 740), (2, 741)], 2, &|_| true), None);
    }
}
