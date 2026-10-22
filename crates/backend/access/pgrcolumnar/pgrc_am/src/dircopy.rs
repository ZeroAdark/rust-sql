//! Database copy and move (W0 L5): `copydir` copies a database directory's files only, so the pgrcolumnar table
//! directories inside it travel here, plainly (FILE_COPY, SET TABLESPACE and their redo) or through a logging
//! [`WriteVfs`] (`pgrc_wal::dbcopy`, STRATEGY WAL_LOG).

use pgrc_format::dirlayout::{is_temp_file_name, parse_manifest_file_name, CURRENT_FILE_NAME};
use pgrc_write::wvfs::{RealVfs, WriteVfs};
use pgrc_write::{WriteError, WriteResult};
use types_error::PgResult;

use crate::catalog::RelSlot;

const CHUNK: usize = 1 << 20;

/// The table directories of a database directory as `(relfilenumber, name)`; temp ones never travel.
pub fn table_dirs(db_path: &str) -> PgResult<Vec<(u32, String)>> {
    let names = match RealVfs.list_dir(db_path) {
        Ok(n) => n,
        Err(WriteError::Io { errno: libc::ENOENT, .. }) => return Ok(Vec::new()),
        Err(e) => return Err(crate::write_error(e)),
    };
    Ok(names.into_iter().filter_map(|n| Some((n.strip_prefix("pgrc_")?.parse().ok()?, n))).collect())
}

/// The files a copy carries: not `tmp-*` debris, not Fp128 sidecars (a cache, rebuilt on use). Manifests follow every
/// other file, oldest first, and `CURRENT` comes last: end-of-recovery cleanup takes a committed manifest as complete.
pub fn carried_files(vfs: &mut dyn WriteVfs, dir: &str) -> WriteResult<Vec<String>> {
    let mut v: Vec<String> = vfs.list_dir(dir)?.into_iter().filter(|n| !is_temp_file_name(n) && !n.contains("-fp128-") && n != CURRENT_FILE_NAME).collect();
    v.sort_by_key(|n| parse_manifest_file_name(n).map_or((0, 0), |g| (1, g)));
    if vfs.exists_path(&format!("{dir}/{CURRENT_FILE_NAME}"))? {
        v.push(CURRENT_FILE_NAME.to_string());
    }
    Ok(v)
}

/// `src/name` to `dst/to` through `vfs` in chunks; `vfs` makes it durable at close (an fsync, or its WAL).
pub fn copy_file(vfs: &mut dyn WriteVfs, src: &str, name: &str, dst: &str, to: &str) -> WriteResult<()> {
    let from = format!("{src}/{name}");
    let fd = vfs.create_rw(&format!("{dst}/{to}"))?;
    let mut off = 0u64;
    loop {
        let b = vfs.read_at(&from, off, CHUNK)?;
        if !b.is_empty() {
            vfs.pwrite_at(&fd, off, &b)?;
        }
        off += b.len() as u64;
        if b.len() < CHUNK {
            break;
        }
    }
    vfs.fsync_file(&fd)?;
    vfs.close_file(fd)
}

/// FILE_COPY, SET TABLESPACE and their redo: every table directory of `src_db` into `dst_db` (the copies fsynced,
/// then the directory), each loaded into a live catalog as `(spc, db, relfilenumber)`.
pub fn copy_table_dirs(src_db: &str, dst_db: &str, spc: u32, db: u32) -> PgResult<()> {
    for (rel, name) in table_dirs(src_db)? {
        let dst = format!("{dst_db}/{name}");
        crate::dirpath::mkdir_if_absent(&dst)?;
        copy_dir(&format!("{src_db}/{name}"), &dst)?;
        load_copy((spc, db, rel as u64), &dst);
    }
    Ok(())
}

/// One table directory's carried files into an existing `dst`, fsynced, then the directory.
pub fn copy_dir(src: &str, dst: &str) -> PgResult<()> {
    let mut vfs = RealVfs;
    (|| -> WriteResult<()> {
        for f in carried_files(&mut vfs, src)? {
            copy_file(&mut vfs, src, &f, dst, &f)?;
        }
        vfs.fsync_dir(dst)
    })()
    .map_err(crate::write_error)
}

/// A copied directory into a live catalog. A failed load drops the entry, so its readers error instead of reading
/// an empty table.
pub fn load_copy(slot: RelSlot, dir: &str) {
    if !crate::catalog::is_live() {
        return;
    }
    if let Err(e) = crate::catalog::load_dir(slot, dir, &crate::footer::fold_generation, &crate::footer::serve_generation_without_catalog) {
        crate::catalog::log(true, format!("pgrcolumnar: copied {dir} left out of the catalog: {}", e.message()));
        crate::catalog::remove_dir(dir);
    }
}
