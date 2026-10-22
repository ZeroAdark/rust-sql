//! reinit.c: reset unlogged relations from before the last restart.
#![allow(non_snake_case)]

use elog::{elog, ereport, PCT_M};
use types_core::ForkNumber;
use types_error::{ErrorLocation, PgResult, DEBUG1, DEBUG2, ERROR, LOG};
use types_storage::{PG_TBLSPC_DIR, TABLESPACE_VERSION_DIRECTORY};

use crate::copydir::copy_file;
use crate::desc::{AllocateDir, FreeDir, ReadDir};
use crate::vfd::get_errno;
use crate::sync::fsync_fname;

pub const UNLOGGED_RELATION_CLEANUP: i32 = 1 << 0;
pub const UNLOGGED_RELATION_INIT: i32 = 1 << 1;

#[cold]
#[track_caller]
fn loc(funcname: &'static str) -> ErrorLocation {
    // pgrust is Rust: report where in OUR source this was raised.
    // #[track_caller] resolves to the call site, not this helper.
    let site = core::panic::Location::caller();
    ErrorLocation::new(site.file(), site.line() as i32, funcname)
}

/// CLEANUP removes every non-init fork of any relation that has an init
/// fork, and empties its pgrcolumnar table directory; INIT copies each init
/// fork over the main fork.
pub fn ResetUnloggedRelations(op: i32) -> PgResult<()> {
    // reinit.c:56
    elog(
        DEBUG1,
        format!(
            "resetting unlogged relations: cleanup {} init {}",
            (op & UNLOGGED_RELATION_CLEANUP != 0) as i32,
            (op & UNLOGGED_RELATION_INIT != 0) as i32
        ),
    )?;

    if startup_seams::begin_startup_progress_phase::is_installed() {
        startup_seams::begin_startup_progress_phase::call();
    }

    ResetUnloggedRelationsInTablespaceDir("base", op)?;

    let spc_dir = AllocateDir(PG_TBLSPC_DIR)?;
    while let Some(de) = ReadDir(spc_dir, PG_TBLSPC_DIR)? {
        if de.d_name == "." || de.d_name == ".." {
            continue;
        }
        let temp_path =
            format!("{PG_TBLSPC_DIR}/{}/{TABLESPACE_VERSION_DIRECTORY}", de.d_name);
        ResetUnloggedRelationsInTablespaceDir(&temp_path, op)?;
    }
    FreeDir(spc_dir)?;
    Ok(())
}

fn ResetUnloggedRelationsInTablespaceDir(tsdirname: &str, op: i32) -> PgResult<()> {
    let ts_dir = AllocateDir(tsdirname)?;
    // A DROP TABLESPACE crash can leave the symlink without the directory;
    // don't block startup on it (C logs and returns).
    if ts_dir.is_none() && get_errno() == libc::ENOENT {
        ereport(LOG)
            .with_saved_errno(get_errno())
            .errcode_for_file_access()
            .errmsg(format!("could not open directory \"{tsdirname}\": {PCT_M}"))
            .finish(loc("ResetUnloggedRelationsInTablespaceDir"))?;
        return Ok(());
    }
    while let Some(de) = ReadDir(ts_dir, tsdirname)? {
        // Only the per-database directories (all-numeric names).
        if de.d_name.is_empty() || !de.d_name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let dbspace_path = format!("{tsdirname}/{}", de.d_name);

        // reinit.c:144-149
        if op & UNLOGGED_RELATION_INIT != 0 {
            crate::sync::ereport_startup_progress(|secs, hundredths| {
                format!(
                    "resetting unlogged relations (init), elapsed time: {secs}.{hundredths:02} s, current path: {dbspace_path}"
                )
            })?;
        } else if op & UNLOGGED_RELATION_CLEANUP != 0 {
            crate::sync::ereport_startup_progress(|secs, hundredths| {
                format!(
                    "resetting unlogged relations (cleanup), elapsed time: {secs}.{hundredths:02} s, current path: {dbspace_path}"
                )
            })?;
        }

        ResetUnloggedRelationsInDbspaceDir(&dbspace_path, op, None)?;
    }
    FreeDir(ts_dir)?;
    Ok(())
}

/// `only`: reset that relfilenumber alone (WALMODE's unsynced markers), else every unlogged relation.
fn ResetUnloggedRelationsInDbspaceDir(dbspacedirname: &str, op: i32, only: Option<u32>) -> PgResult<()> {
    debug_assert!(op & (UNLOGGED_RELATION_CLEANUP | UNLOGGED_RELATION_INIT) != 0);

    if op & UNLOGGED_RELATION_CLEANUP != 0 {
        // Startup-lifetime scratch set mirroring C's throwaway HTAB in a
        // temporary context (bare std collection justified: no arena exists
        // this early and the set dies with this call).
        let mut init_rels = std::collections::HashSet::new();

        let dbspace_dir = AllocateDir(dbspacedirname)?;
        while let Some(de) = ReadDir(dbspace_dir, dbspacedirname)? {
            let Some((relnumber, fork, _segno)) =
                parse_filename_for_nontemp_relation(&de.d_name)
            else {
                continue;
            };
            if fork != ForkNumber::INIT_FORKNUM || only.is_some_and(|o| o != relnumber) {
                continue;
            }
            init_rels.insert(relnumber);
        }
        FreeDir(dbspace_dir)?;

        if init_rels.is_empty() {
            return Ok(());
        }

        let dbspace_dir = AllocateDir(dbspacedirname)?;
        while let Some(de) = ReadDir(dbspace_dir, dbspacedirname)? {
            if let Some((None, n)) = pgrc_format::dirlayout::parse_table_dir_name(&de.d_name) {
                if u32::try_from(n).is_ok_and(|n| init_rels.contains(&n)) {
                    empty_table_dir(&format!("{dbspacedirname}/{}", de.d_name))?;
                }
                continue;
            }
            let Some((relnumber, fork, _segno)) =
                parse_filename_for_nontemp_relation(&de.d_name)
            else {
                continue;
            };
            if fork == ForkNumber::INIT_FORKNUM {
                continue;
            }
            if init_rels.contains(&relnumber) {
                let rm_path = format!("{dbspacedirname}/{}", de.d_name);
                if vfs::unlink(&crate::vfd::cpath(&rm_path)) != 0 {
                    return Err(ereport(ERROR)
                        .with_saved_errno(get_errno())
                        .errcode_for_file_access()
                        .errmsg(format!("could not remove file \"{rm_path}\": {PCT_M}"))
                        .finish(loc("ResetUnloggedRelationsInDbspaceDir"))
                        .unwrap_err());
                }
                // reinit.c:264
                elog(DEBUG2, format!("unlinked file \"{rm_path}\""))?;
            }
        }
        FreeDir(dbspace_dir)?;
    }

    if op & UNLOGGED_RELATION_INIT != 0 {
        let dbspace_dir = AllocateDir(dbspacedirname)?;
        while let Some(de) = ReadDir(dbspace_dir, dbspacedirname)? {
            let Some((relnumber, fork, segno)) =
                parse_filename_for_nontemp_relation(&de.d_name)
            else {
                continue;
            };
            if fork != ForkNumber::INIT_FORKNUM || only.is_some_and(|o| o != relnumber) {
                continue;
            }
            let srcpath = format!("{dbspacedirname}/{}", de.d_name);
            let dstpath = main_fork_path(dbspacedirname, relnumber, segno);
            // reinit.c:314
            elog(DEBUG2, format!("copying {srcpath} to {dstpath}"))?;
            copy_file(&srcpath, &dstpath)?;
        }
        FreeDir(dbspace_dir)?;

        // copy_file flushed data; fsync in a separate pass (no checkpoint
        // will do it during recovery), then the directory itself.
        let dbspace_dir = AllocateDir(dbspacedirname)?;
        while let Some(de) = ReadDir(dbspace_dir, dbspacedirname)? {
            let Some((relnumber, fork, segno)) =
                parse_filename_for_nontemp_relation(&de.d_name)
            else {
                continue;
            };
            if fork != ForkNumber::INIT_FORKNUM || only.is_some_and(|o| o != relnumber) {
                continue;
            }
            fsync_fname(&main_fork_path(dbspacedirname, relnumber, segno), false)?;
        }
        FreeDir(dbspace_dir)?;

        fsync_fname(dbspacedirname, true)?;
    }
    Ok(())
}

/// WALMODE: reset the unlogged pgrcolumnar tables written with `pgrust.pgrc_unlogged_sync=off` under another
/// boot (their files were never fsynced, so an OS crash after a clean shutdown may have torn them), at every start,
/// before anything reads them. A marker of this boot is kept; one whose directory is gone (a crash between the
/// table's drop and the marker's removal) is removed. A marker of no boot id (not Linux) never matches.
pub fn ResetUnsyncedPgrc() -> PgResult<()> {
    use pgrc_format::spool::{parse_unsynced_name, UNSYNCED_DIR};
    let names: Vec<String> = match vfs::read_dir(&crate::vfd::cpath(UNSYNCED_DIR)) {
        Ok(it) => it.filter_map(|e| e.ok()).filter_map(|p| p.as_str().map(str::to_string)).filter(|n| n != "." && n != "..").collect(),
        Err(_) => return Ok(()),
    };
    let first_line = |p: &str| read_small(p).map(|b| String::from_utf8_lossy(&b).lines().next().unwrap_or("").trim().to_string());
    let boot = first_line("/proc/sys/kernel/random/boot_id").filter(|s| !s.is_empty());
    let mut removed = false;
    for n in names {
        let marker = format!("{UNSYNCED_DIR}/{n}");
        let Some(dir) = parse_unsynced_name(&n) else {
            // a crash's half-written marker: its table was never written unsynced past it
            let _ = vfs::unlink(&crate::vfd::cpath(&marker));
            removed = true;
            continue;
        };
        let written_under = first_line(&marker);
        let mut st = vfs::FileInfo::default();
        let exists = vfs::stat(&crate::vfd::cpath(&dir), &mut st) == 0;
        if exists && boot.is_some() && written_under == boot {
            continue;
        }
        if exists {
            let (dbspace, base) = dir.rsplit_once('/').unwrap_or((".", dir.as_str()));
            if let Some((None, rel)) = pgrc_format::dirlayout::parse_table_dir_name(base) {
                ereport(LOG)
                    .errmsg(format!(
                        "pgrcolumnar: resetting unlogged table directory \"{dir}\": written without fsync under boot {}",
                        written_under.as_deref().unwrap_or("?")
                    ))
                    .finish(loc("ResetUnsyncedPgrc"))?;
                // a directory whose relation has no init fork (not unlogged any more) is left alone
                ResetUnloggedRelationsInDbspaceDir(dbspace, UNLOGGED_RELATION_CLEANUP | UNLOGGED_RELATION_INIT, u32::try_from(rel).ok())?;
            }
        }
        if vfs::unlink(&crate::vfd::cpath(&marker)) != 0 && get_errno() != libc::ENOENT {
            return Err(ereport(ERROR)
                .with_saved_errno(get_errno())
                .errcode_for_file_access()
                .errmsg(format!("could not remove file \"{marker}\": %m"))
                .finish(loc("ResetUnsyncedPgrc"))
                .unwrap_err());
        }
        removed = true;
    }
    if removed {
        fsync_fname(UNSYNCED_DIR, true)?;
    }
    Ok(())
}

/// A small file's bytes through the vfs (`None`: absent or unreadable).
fn read_small(path: &str) -> Option<Vec<u8>> {
    let fd = vfs::open(&crate::vfd::cpath(path), libc::O_RDONLY | libc::O_CLOEXEC, 0);
    if fd < 0 {
        return None;
    }
    let mut buf = vec![0u8; 4096];
    let n = vfs::pread(fd, &mut buf, 0);
    vfs::close(fd);
    (n >= 0).then(|| {
        buf.truncate(n as usize);
        buf
    })
}

/// An unlogged pgrcolumnar table directory reset: every file removed, the (logged) directory kept.
fn empty_table_dir(path: &str) -> PgResult<()> {
    let dir = AllocateDir(path)?;
    while let Some(de) = ReadDir(dir, path)? {
        if de.d_name == "." || de.d_name == ".." {
            continue;
        }
        let rm_path = format!("{path}/{}", de.d_name);
        if vfs::unlink(&crate::vfd::cpath(&rm_path)) != 0 {
            return Err(ereport(ERROR)
                .with_saved_errno(get_errno())
                .errcode_for_file_access()
                .errmsg(format!("could not remove file \"{rm_path}\": %m"))
                .finish(loc("ResetUnloggedRelationsInDbspaceDir"))
                .unwrap_err());
        }
        elog(DEBUG2, format!("unlinked file \"{rm_path}\""))?;
    }
    FreeDir(dir)?;
    fsync_fname(path, true)
}

fn main_fork_path(dbspacedirname: &str, relnumber: u32, segno: u32) -> String {
    if segno == 0 {
        format!("{dbspacedirname}/{relnumber}")
    } else {
        format!("{dbspacedirname}/{relnumber}.{segno}")
    }
}

/// parse_filename_for_nontemp_relation: `<relnumber>[_<fork>][.<segno>]`,
/// leading zeroes rejected so each value has one spelling.
pub fn parse_filename_for_nontemp_relation(name: &str) -> Option<(u32, ForkNumber, u32)> {
    let bytes = name.as_bytes();
    if !(b'1'..=b'9').contains(bytes.first()?) {
        return None;
    }
    let mut pos = 1;
    while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
        pos += 1;
    }
    let relnumber: u32 = name[..pos].parse().ok()?;

    let mut fork = ForkNumber::MAIN_FORKNUM;
    if bytes.get(pos) == Some(&b'_') {
        let rest = &name[pos + 1..];
        let (f, len) = if rest.starts_with("fsm") {
            (ForkNumber::FSM_FORKNUM, 3)
        } else if rest.starts_with("vm") {
            (ForkNumber::VISIBILITYMAP_FORKNUM, 2)
        } else if rest.starts_with("init") {
            (ForkNumber::INIT_FORKNUM, 4)
        } else {
            return None;
        };
        fork = f;
        pos += len + 1;
    }

    let mut segno: u32 = 0;
    if bytes.get(pos) == Some(&b'.') {
        if !(b'1'..=b'9').contains(bytes.get(pos + 1)?) {
            return None;
        }
        let seg_start = pos + 1;
        let mut p = seg_start;
        while bytes.get(p).is_some_and(u8::is_ascii_digit) {
            p += 1;
        }
        segno = name[seg_start..p].parse().ok()?;
        pos = p;
    }

    if pos != name.len() {
        return None;
    }
    Some((relnumber, fork, segno))
}
