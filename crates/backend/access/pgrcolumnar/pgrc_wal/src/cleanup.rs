//! End-of-recovery cleanup: every `pgrc` directory is cleaned against clog, where only prepared transactions survive.
//! Nothing is logged: the changes are registered for the end-of-recovery checkpoint, and a crash before it reruns recovery.

use pgsync::Mutex;

use pgrc_am::{catalog, dirpath};
use pgrc_am::probe::{ClogTxnProbe, StandbyTempProbe};
use pgrc_format::dirlayout::parse_table_dir_name;
use pgrc_format::wal::{DirLoc, InstallRec, PublishRec, ReclaimRec};
use pgrc_write::cleanup::{cleanup_dir_keeping, forget_swept, note_swept, parse_temp_fxid};
use pgrc_write::walsink::{Apply, WalSink};
use pgrc_write::wvfs::{RealVfs, WriteVfs};
use pgrc_write::{WriteError, WriteResult};
use types_error::{PgError, PgResult, LOG, WARNING};
use types_storage::file::{PG_TBLSPC_DIR, TABLESPACE_VERSION_DIRECTORY};

const DEFAULTTABLESPACE_OID: u32 = 1663;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDir {
    pub loc: DirLoc,
    pub path: String,
    pub temp: bool,
}

fn listed(vfs: &mut RealVfs, dir: &str) -> PgResult<Vec<String>> {
    match vfs.list_dir(dir) {
        Ok(v) => Ok(v),
        Err(WriteError::Io { errno: libc::ENOENT, .. }) => Ok(Vec::new()),
        Err(e) => Err(pgrc_am::write_error(e)),
    }
}

fn scan_db(vfs: &mut RealVfs, spc: u32, db_path: &str, db: u32, out: &mut Vec<TableDir>) -> PgResult<()> {
    for name in listed(vfs, db_path)? {
        let Some((temp, relnumber)) = parse_table_dir_name(&name) else { continue };
        let Ok(relnumber) = u32::try_from(relnumber) else { continue };
        out.push(TableDir { loc: DirLoc { spc, db, relnumber }, path: format!("{db_path}/{name}"), temp: temp.is_some() });
    }
    Ok(())
}

pub fn table_dirs() -> PgResult<Vec<TableDir>> {
    let mut vfs = RealVfs;
    let mut out = Vec::new();
    for db in listed(&mut vfs, "base")? {
        if let Ok(oid) = db.parse::<u32>() {
            scan_db(&mut vfs, DEFAULTTABLESPACE_OID, &format!("base/{db}"), oid, &mut out)?;
        }
    }
    for spc in listed(&mut vfs, PG_TBLSPC_DIR)? {
        let Ok(spc_oid) = spc.parse::<u32>() else { continue };
        let spc_dir = format!("{PG_TBLSPC_DIR}/{spc}/{TABLESPACE_VERSION_DIRECTORY}");
        for db in listed(&mut vfs, &spc_dir)? {
            if let Ok(oid) = db.parse::<u32>() {
                scan_db(&mut vfs, spc_oid, &format!("{spc_dir}/{db}"), oid, &mut out)?;
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

struct RegisterSink {
    loc: DirLoc,
}

fn not_logged<T>() -> WriteResult<T> {
    Err(WriteError::Contract { detail: "end-of-recovery cleanup logs nothing" })
}

impl WalSink for RegisterSink {
    fn logs(&self) -> bool {
        false
    }
    fn file_write(&mut self, _: &str, _: u64, _: &[u8], _: bool) -> WriteResult<()> {
        not_logged()
    }
    fn file_done(&mut self, _: &str, _: &mut dyn FnMut() -> WriteResult<()>) -> WriteResult<()> {
        not_logged()
    }
    fn publish(&mut self, _: &PublishRec, _: Apply<'_>) -> WriteResult<()> {
        not_logged()
    }
    fn install(&mut self, _: &InstallRec, _: Apply<'_>) -> WriteResult<()> {
        not_logged()
    }
    fn reclaim(&mut self, _: &ReclaimRec, _: Apply<'_>) -> WriteResult<()> {
        not_logged()
    }
    fn register(&mut self, name: &str) -> WriteResult<()> {
        crate::sync::register(self.loc, [name.to_string()]).map_err(crate::insert::stash)
    }
}

fn prepared_xids() -> PgResult<Vec<types_core::TransactionId>> {
    if !twophase_seams::prescan_prepared_transactions_xids::is_installed() {
        return Ok(Vec::new());
    }
    twophase_seams::prescan_prepared_transactions_xids::call()?;
    twophase_seams::prepared_transaction_xids_with_subxids::call()
}

fn clean(d: &TableDir, probe: &ClogTxnProbe) -> PgResult<()> {
    if d.temp {
        return dirpath::remove_table_dir(&d.path);
    }
    let mut sink = RegisterSink { loc: d.loc };
    let slot = (d.loc.spc, d.loc.db, d.loc.relnumber as u64);
    // a promoted hot standby's sessions keep reading the committed generations its catalog holds; a crash recovery
    // primes the catalog after this pass, so it keeps none
    let held = if catalog::is_live() { catalog::committed_parts(slot, probe) } else { Default::default() };
    probe.take_error()?;
    let r = cleanup_dir_keeping(&mut RealVfs, &mut sink, &d.path, probe, &held);
    probe.take_error()?;
    r.map_err(pgrc_am::write_error)?;
    if catalog::is_live() {
        catalog::drop_uncommitted(slot, probe);
        probe.take_error()?;
    }
    Ok(())
}

fn warn(what: &str, e: &PgError) {
    let _ = elog::elog(WARNING, format!("pgrcolumnar: end-of-recovery cleanup of {what} failed: {}", e.message()));
}

/// The restartpoint sweep and the end-of-recovery cleanup never run at once, since the cleanup's unlinks expect no
/// other remover; `true` once the cleanup began, after which no sweep runs.
static RECOVERY_ENDED: Mutex<bool> = Mutex::new(false);

pub fn pgrc_startup(_: mcx::Mcx<'_>) -> PgResult<()> {
    *pgsync::lock(&RECOVERY_ENDED) = false;
    forget_swept();
    Ok(())
}

/// A directory that cannot be cleaned is left as it is with a WARNING; its residue is never
/// visible (readers resolve the effective generation) and the next recovery retries.
pub fn pgrc_cleanup() {
    let mut ended = pgsync::lock(&RECOVERY_ENDED);
    *ended = true;
    forget_swept();
    crate::fetch::remove_spool_debris();
    let probe = match prepared_xids() {
        Ok(p) => ClogTxnProbe::recovery(p),
        Err(e) => return warn("the prepared transactions", &e),
    };
    let dirs = match table_dirs() {
        Ok(d) => d,
        Err(e) => return warn("the table directory walk", &e),
    };
    for d in &dirs {
        if let Err(e) = clean(d, &probe) {
            warn(&d.path, &e);
        }
    }
}

/// No WAL, and nothing registered for sync: an unlink a crash loses leaves a dead temp for the next pass. Without hot
/// standby the known-assigned xids are not complete, and nothing is removed. Redo that would later skip a noted temp
/// as missing PANICs instead, since its writer was alive.
pub fn restartpoint_temps() {
    let ended = pgsync::lock(&RECOVERY_ENDED);
    let hot = xlogrecovery_seams::hot_standby_active::is_installed() && xlogrecovery_seams::hot_standby_active::call();
    if *ended || !hot || !transam_xlog::RecoveryInProgress() {
        return;
    }
    let probe = StandbyTempProbe::take();
    let dirs = match table_dirs() {
        Ok(d) => d,
        Err(e) => return warn_sweep("the table directory walk", &e),
    };
    for d in dirs.iter().filter(|d| !d.temp) {
        let key = crate::dir_path(d.loc);
        match sweep_dir(&d.path, &key, &|fxid| probe.removable(fxid)) {
            Ok((removed, kept)) => {
                for (name, fxid) in removed {
                    let msg = format!("pgrcolumnar: restartpoint removed {key}/{name} (writer fxid {fxid})");
                    let _ = elog::elog(LOG, msg);
                }
                kept.iter().for_each(|(path, e)| warn_sweep(path, e));
            }
            Err(e) => warn_sweep(&d.path, &e),
        }
    }
}

fn warn_sweep(what: &str, e: &PgError) {
    let msg = format!("pgrcolumnar: restartpoint removal of dead temps in {what} failed: {}", e.message());
    let _ = elog::elog(WARNING, msg);
}

pub(crate) type Swept = (Vec<(String, u64)>, Vec<(String, Box<PgError>)>);

/// Each temp is noted as swept under `key` before its unlink, so no redo skip of it can slip between the two.
pub(crate) fn sweep_dir(dir: &str, key: &str, removable: &dyn Fn(u64) -> PgResult<bool>) -> PgResult<Swept> {
    let mut vfs = RealVfs;
    let (mut removed, mut kept) = (Vec::new(), Vec::new());
    for name in listed(&mut vfs, dir)? {
        let Some(fxid) = parse_temp_fxid(&name) else { continue };
        let path = format!("{dir}/{name}");
        match removable(fxid) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                kept.push((path, e));
                continue;
            }
        }
        note_swept(format!("{key}/{name}"), fxid);
        match vfs.unlink_path(&path) {
            Ok(()) => removed.push((name, fxid)),
            Err(WriteError::Io { errno: libc::ENOENT, .. }) => {}
            Err(e) => return Err(pgrc_am::write_error(e)),
        }
    }
    Ok((removed, kept))
}
