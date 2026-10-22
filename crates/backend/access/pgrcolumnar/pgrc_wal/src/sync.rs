//! The `pgrc` sync handler: one checkpointer request per table directory, plus the names written in it since its last sync.

use std::collections::{BTreeMap, BTreeSet};

use pgrc_format::wal::DirLoc;
use pgrc_write::wvfs::{RealVfs, WriteVfs};
use pgrc_write::{WriteError, WriteResult};
use pgsync::{lock, Mutex};
use types_core::ForkNumber;
use types_error::PgResult;
use types_storage::sync::{FileTag, FileTagOpResult, SyncRequestHandler, SyncRequestType};

/// Past this many names a directory's sync fsyncs every file in it.
pub const DIRTY_NAMES_CAP: usize = 4096;

#[derive(Debug)]
struct Dirty {
    /// D4748: the entry's bytes, off the noting session and on the part cache; back on whichever thread drops it.
    charge: memledger::Charge,
    names: BTreeSet<String>,
    overflow: bool,
    bytes: usize,
}

impl Default for Dirty {
    fn default() -> Dirty {
        Dirty { charge: memledger::Charge::NONE, names: BTreeSet::new(), overflow: false, bytes: 0 }
    }
}

/// A tree node's links, at two words.
const NODE: usize = 2 * size_of::<usize>();

fn name_bytes(capacity: usize) -> usize {
    size_of::<String>() + NODE + capacity
}

impl Dirty {
    /// The charge brought to the entry's slot and names on this thread (door 1: its names were made here).
    fn refit(&mut self) {
        const OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache));
        if self.charge.owner() != OWNER {
            self.charge = memledger::Charge::open(OWNER, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, 0);
        }
        self.charge.fit_committed((size_of::<(DirLoc, Dirty)>() + NODE + self.bytes) as u64);
    }
}

pgsync::process_global! {
    static DIRTY: Mutex<BTreeMap<DirLoc, Dirty>> = Mutex::new(BTreeMap::new());
    static DIRTY_LISTENING: pgsync::atomic::AtomicBool = pgsync::atomic::AtomicBool::new(false);
}

/// D4748: a directory the catalog removed (DROP, a rewrite's old locator, an aborted creation, DROP DATABASE) has nothing
/// left to sync, and a dropped database's request may never reach a checkpoint: its names go with it.
fn dirty_slot_event(ev: &pgrc_am::catalog::SlotEvent<'_>) {
    if let pgrc_am::catalog::SlotEvent::Removed((spc, db, relnumber), _) = *ev {
        let Ok(relnumber) = u32::try_from(relnumber) else { return };
        let gone = lock(&DIRTY).remove(&DirLoc { spc, db, relnumber });
        drop(gone);
    }
}

/// The spool's tag: no table has relfilenumber 0 in database 0, so the sentinel names `pg_pgrc_spool`.
pub const SPOOL_LOC: DirLoc = DirLoc { spc: 0, db: 0, relnumber: 0 };

fn tag_dir(loc: DirLoc) -> String {
    if loc == SPOOL_LOC {
        return pgrc_format::spool::SPOOL_DIR.to_string();
    }
    crate::dir_path(loc)
}

pub fn tag(loc: DirLoc) -> FileTag {
    FileTag::new(SyncRequestHandler::SYNC_HANDLER_PGRC, ForkNumber::MAIN_FORKNUM, crate::locator(loc), 0)
}

fn note(loc: DirLoc, names: impl IntoIterator<Item = String>) {
    if !DIRTY_LISTENING.load(pgsync::atomic::Ordering::Acquire) && !DIRTY_LISTENING.swap(true, pgsync::atomic::Ordering::AcqRel) {
        pgrc_am::catalog::on_slot_event(dirty_slot_event);
    }
    let mut g = lock(&DIRTY);
    let d = g.entry(loc).or_default();
    for n in names {
        if d.overflow {
            break;
        }
        let b = name_bytes(n.capacity());
        if d.names.insert(n) {
            d.bytes += b;
        }
        if d.names.len() > DIRTY_NAMES_CAP {
            d.names.clear();
            d.bytes = 0;
            d.overflow = true;
        }
    }
    d.refit();
}

/// The set is filled first, so a checkpoint that absorbs the request syncs the names; callers hold `DELAY_CHKPT_START`,
/// so a checkpoint whose redo pointer follows the change absorbs it.
pub fn register(loc: DirLoc, names: impl IntoIterator<Item = String>) -> PgResult<()> {
    note(loc, names);
    sync_seams::register_sync_request::call(tag(loc), SyncRequestType::SYNC_REQUEST, true).map(drop)
}

fn errno_of(e: &WriteError) -> i32 {
    match e {
        WriteError::Io { errno, .. } => *errno,
        _ => libc::EIO,
    }
}

fn fsync_file(vfs: &mut RealVfs, path: &str) -> WriteResult<()> {
    let fd = vfs.open_rw(path)?;
    let r = vfs.fsync_file(&fd);
    let c = vfs.close_file(fd);
    r.and(c)
}

fn sync_dir(dir: &str, d: &Dirty) -> FileTagOpResult {
    let mut vfs = RealVfs;
    let fail = |path: &str, e: &WriteError| FileTagOpResult { result: -1, path: path.to_string(), errno: errno_of(e) };
    let names: Vec<String> = if d.overflow {
        match vfs.list_dir(dir) {
            Ok(v) => v,
            Err(e) if errno_of(&e) == libc::ENOENT => Vec::new(),
            Err(e) => return fail(dir, &e),
        }
    } else {
        d.names.iter().cloned().collect()
    };
    let paths: Vec<String> = names.iter().map(|n| format!("{dir}/{n}")).collect();
    let fsync = |path: &str| match fsync_file(&mut vfs, path) {
        Err(e) if errno_of(&e) != libc::ENOENT => Err(fail(path, &e)),
        _ => Ok(()),
    };
    let synced = pgrc_am::dirpath::flush_then_fsync(&paths, pgrc_am::dirpath::flush_path, fsync);
    if let Err(r) = synced {
        return r;
    }
    match vfs.fsync_dir(dir) {
        Err(e) if errno_of(&e) != libc::ENOENT => fail(dir, &e),
        _ => FileTagOpResult { result: 0, path: dir.to_string(), errno: 0 },
    }
}

/// A name or directory that is gone counts as done: it was renamed, reclaimed or dropped, and each of those is logged.
/// On failure the names go back.
pub fn syncfiletag(tag: &FileTag) -> PgResult<FileTagOpResult> {
    let loc = crate::dir_loc(tag.rlocator);
    Ok(sync_loc(loc, &tag_dir(loc)))
}

pub(crate) fn sync_loc(loc: DirLoc, dir: &str) -> FileTagOpResult {
    let taken = lock(&DIRTY).remove(&loc).unwrap_or_default();
    let r = sync_dir(dir, &taken);
    if r.result != 0 {
        let mut g = lock(&DIRTY);
        let d = g.entry(loc).or_default();
        if taken.overflow || d.overflow || d.names.len() + taken.names.len() > DIRTY_NAMES_CAP {
            d.names.clear();
            d.overflow = true;
        } else {
            d.names.extend(taken.names);
        }
        d.bytes = d.names.iter().map(|n| name_bytes(n.capacity())).sum();
        d.refit();
    }
    r
}

pub fn unlinkfiletag(tag: &FileTag) -> PgResult<FileTagOpResult> {
    Ok(FileTagOpResult { result: -1, path: tag_dir(crate::dir_loc(tag.rlocator)), errno: libc::EINVAL })
}

pub fn filetagmatches(tag: &FileTag, candidate: &FileTag) -> bool {
    tag.rlocator.dbOid == candidate.rlocator.dbOid
}

#[cfg(test)]
pub(crate) fn dirty_names(loc: DirLoc) -> Option<(Vec<String>, bool)> {
    lock(&DIRTY).get(&loc).map(|d| (d.names.iter().cloned().collect(), d.overflow))
}

#[cfg(test)]
mod d4748 {
    use super::*;

    #[test]
    fn a_removed_directory_takes_its_names_and_another_keeps_its_own() {
        let (a, b) = (DirLoc { spc: 1663, db: 4_000_048, relnumber: 9 }, DirLoc { spc: 1663, db: 4_000_049, relnumber: 9 });
        note(a, ["p1".to_string(), "p2".to_string()]);
        note(b, ["p1".to_string()]);
        dirty_slot_event(&pgrc_am::catalog::SlotEvent::Removed((1663, 4_000_048, 9), "/nowhere"));
        assert_eq!(dirty_names(a), None);
        assert_eq!(dirty_names(b), Some((vec!["p1".to_string()], false)));
        assert_eq!(lock(&DIRTY).get(&b).map(|d| d.bytes), Some(name_bytes("p1".to_string().capacity())));
    }
}
