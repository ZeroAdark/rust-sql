//! pgrcolumnar parts by reference in the WAL archive (WR L4, docs WR.md "L4", OC-1). Before a WAL segment (or its
//! `.partial`) is archived, every spool entry of the segment's LSN range is archived through the same module as
//! `<segment>.<8-hex offset>.pgrc`, so an archive never holds a reference without its file. The entries are archived
//! whenever they exist, whatever `pgrust.pgrc_archive_parts` says now (the setting decides future references only).
//! A failure fails the segment, which is retried like any; the entries already archived for it are not sent again.

use std::cell::RefCell;

use pgrc_format::spool::archive_name;
use pgrc_spool::Spool;
use types_error::{PgResult, ERROR};

use crate::loc;

thread_local! {
    /// The segment being archived and the entry names already archived for it (a retried segment skips them: an
    /// archive_command that refuses to overwrite would fail on them forever).
    static ARCHIVED: RefCell<(String, Vec<String>)> = const { RefCell::new((String::new(), Vec::new())) };
}

pub(crate) fn segment_of(xlog: &str, seg_size: u64) -> Option<(&str, u64)> {
    let seg = xlog.strip_suffix(".partial").unwrap_or(xlog);
    if seg.len() != 24 || !seg.bytes().all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b)) {
        return None;
    }
    let log = u64::from_str_radix(&seg[8..16], 16).ok()?;
    let off = u64::from_str_radix(&seg[16..24], 16).ok()?;
    Some((seg, (log * (0x1_0000_0000 / seg_size) + off) * seg_size))
}

/// Archive the spool entries of `xlog`'s segment through `archive(file name, path)`. `Ok(false)`: one failed.
pub(crate) fn archive_segment_files_in(
    spool: &Spool,
    xlog: &str,
    seg_size: u64,
    archive: &mut dyn FnMut(&str, &str) -> PgResult<bool>,
) -> PgResult<bool> {
    let Some((seg, from)) = segment_of(xlog, seg_size) else { return Ok(true) };
    // The segment is complete: every reference in it began before this call, so after settle it is named.
    pgrc_spool::settle();
    let entries = match spool.list(from, from + seg_size) {
        Ok(v) => v,
        Err(e) => {
            elog::ereport(ERROR)
                .with_saved_errno(e.raw_os_error().unwrap_or(0))
                .errcode_for_file_access()
                .errmsg(format!("could not list \"{}\": %m", spool.dir()))
                .finish(loc("pgrc_archive_segment_files"))?;
            return Ok(false);
        }
    };
    ARCHIVED.with(|a| {
        let mut a = a.borrow_mut();
        if a.0 != seg {
            *a = (seg.to_string(), Vec::new());
        }
    });
    for (lsn, _) in entries {
        let name = archive_name(seg, lsn, seg_size);
        if ARCHIVED.with(|a| a.borrow().1.contains(&name)) {
            continue;
        }
        if !archive(&name, &spool.path(lsn))? {
            return Ok(false);
        }
        ARCHIVED.with(|a| a.borrow_mut().1.push(name));
    }
    Ok(true)
}

pub(crate) fn archive_segment_files(xlog: &str, archive: &mut dyn FnMut(&str, &str) -> PgResult<bool>) -> PgResult<bool> {
    archive_segment_files_in(&Spool::data(), xlog, transam_xlog::wal_segment_size() as u64, archive)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEG: u64 = 16 << 20;

    fn spool(tag: &str) -> (std::path::PathBuf, Spool) {
        let base = std::env::temp_dir().join(format!("pgarch-pgrc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("pg_pgrc_spool")).unwrap();
        let sp = Spool::at(base.join("pg_pgrc_spool").to_str().unwrap());
        (base, sp)
    }

    #[test]
    fn segment_names_map_to_their_lsn_range() {
        assert_eq!(segment_of("000000010000000000000003", SEG), Some(("000000010000000000000003", 3 * SEG)));
        assert_eq!(segment_of("000000020000000100000002.partial", SEG), Some(("000000020000000100000002", 0x1_0000_0000 + 2 * SEG)));
        assert_eq!(segment_of("00000002.history", SEG), None);
        assert_eq!(segment_of("000000010000000000000003.00000028.backup", SEG), None);
    }

    #[test]
    fn a_segments_entries_go_first_and_a_retry_skips_the_archived_ones() {
        let (base, sp) = spool("seg");
        for lsn in [3 * SEG - 8, 3 * SEG + 0x28, 3 * SEG + 0x9000, 4 * SEG] {
            std::fs::write(sp.path(lsn), b"part").unwrap();
        }
        let mut sent: Vec<(String, String)> = Vec::new();
        let mut fail_second = true;
        let mut archive = |f: &str, p: &str| {
            if fail_second && sent.len() == 1 {
                fail_second = false;
                return Ok(false);
            }
            sent.push((f.to_string(), p.to_string()));
            Ok(true)
        };
        let seg = "000000010000000000000003";
        assert!(!archive_segment_files_in(&sp, seg, SEG, &mut archive).unwrap(), "the second entry failed");
        assert!(archive_segment_files_in(&sp, seg, SEG, &mut archive).unwrap());
        let names: Vec<&str> = sent.iter().map(|(f, _)| f.as_str()).collect();
        assert_eq!(names, ["000000010000000000000003.00000028.pgrc", "000000010000000000000003.00009000.pgrc"]);
        assert!(sent[0].1.ends_with("/pg_pgrc_spool/0000000003000028"));
        assert!(archive_segment_files_in(&sp, "00000001.history", SEG, &mut |_, _| panic!("no entries")).unwrap());
        let _ = std::fs::remove_dir_all(base);
    }
}
