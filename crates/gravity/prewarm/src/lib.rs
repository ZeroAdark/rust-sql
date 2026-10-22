//! The catalog-page pre-warm: after the columnar catalog is loaded, a
//! background worker per database opens every columnar relation once from a real backend
//! context — relcache entry, attributes, indexes, constraints, extended statistics,
//! `pg_statistic` rows, column types — so the catalog pages a first backend needs are in shared
//! buffers before the first connection. Metadata only. Contract: docs/gravity/hosts.md.
//!
//! The fresh-backend floor: the same worker also reads the process image
//! (the executable and its shared objects) through the page cache once, and every `pg_catalog`
//! relation, index and toast of each warmed database into shared buffers, so the first statement
//! of a fresh backend after a dropped page cache faults no code page and reads no catalog page
//! from disk (box 1: `select 1` 36 → 0.3 ms, a plain count(*) 20 → 2 ms; hosts.md).
#![deny(unsafe_code)]

use std::cell::Cell;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use pg_clock::MonoStamp;
use pgsync::Mutex;

use pgrc_am::catalog::RelSlot;
use types_core::{BlockNumber, InvalidOid, Oid, DATABASE_RELATION_ID, MAIN_FORKNUM, PG_CATALOG_NAMESPACE, RELATION_RELATION_ID};
use types_error::{PgError, PgResult, LOG, WARNING};
use types_rel::{RELKIND_INDEX, RELKIND_RELATION};
use types_storage::lock::AccessShareLock;
use types_storage::storage::ReadBufferMode;

mod raw;

const NAME: &str = "gravity columnar catalog pre-warm";
const WAIT_MAX: Duration = Duration::from_secs(30);
const ANUM_PG_CLASS_OID: i32 = 1;
const ANUM_PG_CLASS_RELNAMESPACE: i32 = 3;
const ANUM_PG_CLASS_RELTOASTRELID: i32 = 14;
const ANUM_PG_CLASS_RELKIND: i32 = 18;
/// A catalog past this many blocks (128 MB; pg_largeobject) is not a first-statement page.
const CATALOG_BLOCKS_MAX: BlockNumber = 16_384;

pgsync::process_global! {
    static DBS: Mutex<Vec<(Oid, Vec<RelSlot>)>> = Mutex::new(Vec::new());
}
static ARMED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);
static REGISTERED: AtomicBool = AtomicBool::new(false);
/// H16 (d) (lowerv4 plan §5.2 ENTRY H16, R12-L18 a): a cluster with no columnar relation still warms. The chain's first
/// worker then lists every connectable database from the shared pg_database (connected to none), warms the process image
/// and the code cache once, and the chain warms each database's catalog pages as it does for a columnar one.
static LIST: AtomicBool = AtomicBool::new(false);
/// A chained worker's argument: its database's index in `DBS` (the low bits) under this bit, so it never lists again and
/// never re-reads the image the lister read.
const LISTED: u64 = 1 << 32;
static DATABASES: AtomicUsize = AtomicUsize::new(0);
static RELATIONS: AtomicU64 = AtomicU64::new(0);
static ATTRIBUTES: AtomicU64 = AtomicU64::new(0);
static CATALOG_BLOCKS: AtomicU64 = AtomicU64::new(0);
static IMAGE_BYTES: AtomicU64 = AtomicU64::new(0);
static ERRORS: AtomicU64 = AtomicU64::new(0);
thread_local! {
    static OWED: Cell<Option<u64>> = const { Cell::new(None) };
}
static MICROS: AtomicU64 = AtomicU64::new(0);

fn worker(arg: u64) -> bgworker::BackgroundWorker {
    bgworker::BackgroundWorker {
        bgw_name: NAME.to_string(), // alloc: Cold
        bgw_type: NAME.to_string(), // alloc: Cold
        bgw_flags: bgworker::BGWORKER_SHMEM_ACCESS | bgworker::BGWORKER_BACKEND_DATABASE_CONNECTION,
        bgw_start_time: bgworker::BgWorkerStartTime::RecoveryFinished,
        bgw_restart_time: bgworker::BGW_NEVER_RESTART,
        bgw_main,
        bgw_main_arg: arg,
        bgw_extra: [0; bgworker::BGW_EXTRALEN],
        bgw_notify_pid: 0,
    }
}

/// Registers startup metadata warming under `pgrust.gravity_prewarm`, independently of predicate reuse.
pub fn register() -> PgResult<()> {
    register_if(gravity_hosts::inputs::startup_prewarm_on())
}

/// `register` under switch value `on`: off, nothing is registered, so `arm` marks the warm done and `wait` returns at once.
fn register_if(on: bool) -> PgResult<()> {
    if !on {
        return Ok(());
    }
    REGISTERED.store(true, Ordering::Release);
    bgworker::RegisterBackgroundWorker(&worker(0))
}

pub fn arm() {
    let mut dbs: Vec<(Oid, Vec<RelSlot>)> = Vec::new(); // alloc: Cold
    for slot in pgrc_am::catalog::slots() {
        match dbs.iter_mut().find(|(db, _)| *db == slot.1) {
            Some((_, v)) => v.push(slot),
            None => dbs.push((slot.1, vec![slot])), // alloc: Cold
        }
    }
    DATABASES.store(dbs.len(), Ordering::Relaxed);
    if !REGISTERED.load(Ordering::Acquire) {
        DONE.store(true, Ordering::Release);
    } else if dbs.is_empty() {
        LIST.store(true, Ordering::Release);
    }
    *pgsync::lock(&DBS) = dbs;
    ARMED.store(true, Ordering::Release);
}

/// The postmaster: block until the chain finished (or `WAIT_MAX`), running `tick` (the
/// postmaster's own bgworker start step) between polls. False = timed out.
pub fn wait(tick: &dyn Fn()) -> bool {
    let t0 = MonoStamp::now();
    while !DONE.load(Ordering::Acquire) {
        if waited(t0) > WAIT_MAX {
            let _ = elog::elog(WARNING, format!("gravity: the catalog pre-warm did not finish within {} s; accepting connections without it", WAIT_MAX.as_secs())); // alloc: Cold
            return false;
        }
        // A chain worker is registered dynamically (the next database, or the lister's first): the postmaster's own
        // loop notices it by the bgworker-change signal, which this wait holds off, so the wait takes the signal in
        // (the ServerLoop's arm, postmaster.c) before its start step. PM_RUN: new workers are allowed.
        if pmsignal::CheckPostmasterSignal(pmsignal::PMSignalReason::PMSIGNAL_BACKGROUND_WORKER_CHANGE) {
            bgworker::BackgroundWorkerStateChange(true);
        }
        tick();
        waiter::sleep(Duration::from_millis(2));
    }
    true
}

/// Time since `t0` on the process monotonic clock (`pg_clock`: virtual time under DST, so the `WAIT_MAX` give-ups and the
/// 2 ms polls, which sleep through the waiter, run on one clock).
fn waited(t0: MonoStamp) -> Duration {
    Duration::from_nanos(t0.elapsed_ns())
}

pub struct Report {
    pub databases: usize,
    pub relations: u64,
    pub attributes: u64,
    pub catalog_blocks: u64,
    pub image_bytes: u64,
    pub errors: u64,
    pub micros: u64,
}

pub fn report() -> Option<Report> {
    DONE.load(Ordering::Acquire).then(|| Report { databases: DATABASES.load(Ordering::Relaxed), relations: RELATIONS.load(Ordering::Relaxed), attributes: ATTRIBUTES.load(Ordering::Relaxed), catalog_blocks: CATALOG_BLOCKS.load(Ordering::Relaxed), image_bytes: IMAGE_BYTES.load(Ordering::Relaxed), errors: ERRORS.load(Ordering::Relaxed), micros: MICROS.load(Ordering::Relaxed) })
}

fn bgw_main(arg: u64) -> PgResult<()> {
    bgworker::BackgroundWorkerUnblockSignals();
    let t0 = MonoStamp::now();
    while !ARMED.load(Ordering::Acquire) {
        if waited(t0) > WAIT_MAX {
            DONE.store(true, Ordering::Release);
            return Ok(());
        }
        waiter::sleep(Duration::from_millis(2));
    }
    if arg == 0 && LIST.load(Ordering::Acquire) {
        return list_and_chain(t0);
    }
    let first = arg & LISTED == 0;
    let idx = (arg & !LISTED) as usize;
    let (db, slots) = {
        let dbs = pgsync::lock(&DBS);
        match dbs.get(idx) {
            Some(e) => e.clone(), // alloc: Cold
            None => {
                DONE.store(true, Ordering::Release);
                return Ok(());
            }
        }
    };
    OWED.with(|o| o.set(Some(arg)));
    ipc::before_shmem_exit(owed_at_exit, datum::Datum::null())?;
    if idx == 0 && first {
        warm_code_cache();
    }
    let image = if idx == 0 && first { warm_image() } else { 0 };
    IMAGE_BYTES.fetch_add(image, Ordering::Relaxed);
    let lists0 = gravity_hosts::ops::scan::pgrc::residency::LISTS.built();
    let r = warm_database(db, &slots);
    // the part lists this database's relations built (`residency::LISTS`: the chain warms one database at a time, before
    // the first connection, so the process counter's delta is this worker's)
    let lists = gravity_hosts::ops::scan::pgrc::residency::LISTS.built() - lists0;
    let (rels, attrs, cats, blocks) = match &r {
        Ok(v) => *v,
        Err(e) => {
            ERRORS.fetch_add(1, Ordering::Relaxed);
            let _ = elog::elog(WARNING, format!("gravity: catalog pre-warm of database {db}: {}", e.message())); // alloc: Cold
            (0, 0, 0, 0)
        }
    };
    RELATIONS.fetch_add(rels, Ordering::Relaxed);
    ATTRIBUTES.fetch_add(attrs, Ordering::Relaxed);
    CATALOG_BLOCKS.fetch_add(blocks, Ordering::Relaxed);
    MICROS.fetch_add(t0.elapsed_ns() / 1_000, Ordering::Relaxed);
    let _ = elog::elog(LOG, format!("gravity: catalog pages warmed for database {db}: {rels} relations, {attrs} attributes, {cats} catalogs / {blocks} blocks, {lists} part lists, image {} MB, {} us", image >> 20, t0.elapsed_ns() / 1_000)); // alloc: Cold
    pass_on();
    Ok(())
}

/// A worker that exits before passing the chain on (a FATAL connecting to an orphan database directory) still does.
fn owed_at_exit(_code: i32, _arg: datum::Datum) -> PgResult<()> {
    if OWED.with(|o| o.get()).is_some() {
        ERRORS.fetch_add(1, Ordering::Relaxed);
        pass_on();
    }
    Ok(())
}

fn pass_on() {
    let Some(arg) = OWED.with(|o| o.take()) else { return };
    if (arg & !LISTED) as usize + 1 >= DATABASES.load(Ordering::Relaxed) {
        DONE.store(true, Ordering::Release);
    } else if !matches!(bgworker::RegisterDynamicBackgroundWorker(worker(arg + 1)), Ok(Some(_))) {
        ERRORS.fetch_add(1, Ordering::Relaxed);
        let _ = elog::elog(WARNING, "gravity: catalog pre-warm: no background worker slot for the next database".to_string()); // alloc: Cold
        DONE.store(true, Ordering::Release);
    }
}

fn warm_code_cache() {
    let tc = MonoStamp::now();
    match gravity_backend::warm() {
        Ok(()) => { let _ = elog::elog(LOG, format!("gravity: code cache warmed in {} us", tc.elapsed_ns() / 1_000)); } // alloc: Cold
        Err(e) => { let _ = elog::elog(WARNING, format!("gravity: code cache warm: {}", e.message())); } // alloc: Cold
    }
}

/// H16 (d): the lister of a cluster with no columnar relation. The code cache and the process image first (what a fresh
/// backend's first statement faults on whatever the database), then every connectable database into `DBS` with no
/// columnar slot, then the chain from its first database (`LISTED`).
fn list_and_chain(t0: MonoStamp) -> PgResult<()> {
    warm_code_cache();
    let image = warm_image();
    IMAGE_BYTES.fetch_add(image, Ordering::Relaxed);
    let dbs = match list_databases() {
        Ok(d) => d,
        Err(e) => {
            ERRORS.fetch_add(1, Ordering::Relaxed);
            let _ = elog::elog(WARNING, format!("gravity: catalog pre-warm: listing the databases: {}", e.message())); // alloc: Cold
            DONE.store(true, Ordering::Release);
            return Ok(());
        }
    };
    let n = dbs.len();
    DATABASES.store(n, Ordering::Relaxed);
    *pgsync::lock(&DBS) = dbs.into_iter().map(|db| (db, Vec::new())).collect(); // alloc: Cold
    MICROS.fetch_add(t0.elapsed_ns() / 1_000, Ordering::Relaxed);
    let _ = elog::elog(LOG, format!("gravity: process image warmed ({} MB), {n} databases to warm (no columnar relation), {} us", image >> 20, t0.elapsed_ns() / 1_000)); // alloc: Cold
    if n == 0 {
        DONE.store(true, Ordering::Release);
    } else if bgworker::RegisterDynamicBackgroundWorker(worker(LISTED))?.is_none() {
        ERRORS.fetch_add(1, Ordering::Relaxed);
        let _ = elog::elog(WARNING, "gravity: catalog pre-warm: no background worker slot for the first database".to_string()); // alloc: Cold
        DONE.store(true, Ordering::Release);
    }
    Ok(())
}

/// Every database a backend can connect to (datallowconn, not a template, not an interrupted drop), read from the shared
/// pg_database by a worker connected to no database (PostgreSQL's bgworker contract: shared catalogs only).
fn list_databases() -> PgResult<Vec<Oid>> {
    bgworker::BackgroundWorkerInitializeConnectionByOid(InvalidOid, InvalidOid, 0)?;
    let top = gravity_exec::Root::new("GravityPrewarmDatabases");
    xact::StartTransactionCommand()?;
    let r = (|| -> PgResult<Vec<Oid>> {
        let mcx = top.mcx();
        let rel = table::table_open(mcx, DATABASE_RELATION_ID, AccessShareLock)?;
        let desc = rel.descr();
        let mut scan = genam::systable_beginscan(mcx, &rel, InvalidOid, false, None, &[])?;
        let mut out: Vec<Oid> = Vec::new(); // alloc: Cold
        while let Some(tup) = genam::systable_getnext(mcx, &mut scan)? {
            let (oid, allow, template, limit) = raw::database_row(tup, desc);
            if allow && !template && limit != pg_database::DATCONNLIMIT_INVALID_DB {
                out.push(oid); // alloc: Cold
            }
        }
        genam::systable_endscan(mcx, scan)?;
        rel.close(AccessShareLock)?;
        Ok(out)
    })();
    match r {
        Ok(v) => {
            xact::CommitTransactionCommand()?;
            Ok(v)
        }
        Err(e) => {
            xact::AbortCurrentTransaction()?;
            Err(e)
        }
    }
}

/// Every file the process maps (Linux: /proc/self/maps; elsewhere the executable alone), read
/// through once. Bytes read; a file that cannot be read is skipped.
fn warm_image() -> u64 {
    let mut files: Vec<std::path::PathBuf> = Vec::new(); // alloc: Cold
    if let Ok(exe) = std::env::current_exe() {
        files.push(exe);
    }
    if let Ok(maps) = std::fs::read_to_string("/proc/self/maps") { // alloc: Cold
        for line in maps.lines() {
            let Some(path) = line.split_whitespace().nth(5) else { continue };
            if !path.starts_with('/') || line.ends_with("(deleted)") {
                continue;
            }
            let p = std::path::Path::new(path);
            if !files.iter().any(|f| f == p) && p.is_file() {
                files.push(p.to_path_buf()); // alloc: Cold
            }
        }
    }
    let mut buf = vec![0u8; gravity_exec::PREWARM_IMAGE_CHUNK]; // alloc: Cold
    let mut total = 0u64;
    for p in &files {
        let Ok(mut f) = std::fs::File::open(p) else { continue };
        while let Ok(n) = f.read(&mut buf) {
            if n == 0 {
                break;
            }
            total += n as u64;
        }
    }
    total
}

fn warm_database(db: Oid, slots: &[RelSlot]) -> PgResult<(u64, u64, u64, u64)> {
    bgworker::BackgroundWorkerInitializeConnectionByOid(db, InvalidOid, 0)?;
    let top = gravity_exec::Root::new("GravityPrewarm");
    xact::StartTransactionCommand()?;
    let mut rels = 0u64;
    let mut attrs = 0u64;
    let mut cats = 0u64;
    let mut blocks = 0u64;
    let r = (|| -> PgResult<()> {
        for slot in slots {
            let relid = relfilenumbermap::RelidByRelfilenumber(slot.0, slot.2 as Oid)?;
            if relid == InvalidOid {
                    continue;
            }
            attrs += warm_relation(top.mcx(), relid)?;
            rels += 1;
        }
        (cats, blocks) = warm_catalogs(top.mcx())?;
        Ok(())
    })();
    match r {
        Ok(()) => xact::CommitTransactionCommand()?,
        Err(e) => {
            xact::AbortCurrentTransaction()?;
            return Err(e);
        }
    }
    Ok((rels, attrs, cats, blocks))
}

fn warm_relation(mcx: mcx::Mcx<'_>, relid: Oid) -> PgResult<u64> {
    let rel = relation::relation_open(mcx, relid, AccessShareLock)?;
    let nsp = lsyscache::relation::get_rel_namespace(relid)?;
    if let Some(name) = lsyscache::relation::get_rel_name(mcx, relid)? {
        lsyscache::relation::get_relname_relid(&name, nsp)?;
    }
    lsyscache::misc::get_namespace_name(mcx, nsp)?;
    relcache::RelationGetIndexList(mcx, relid)?;
    relcache::statextlist::RelationGetStatExtList(mcx, relid)?;
    relcache::fkeylist::RelationGetFKeyList(relid)?;
    let natts = rel.rd_att.natts as usize;
    let mut warmed = 0u64;
    for i in 0..natts {
        let a = rel.rd_att.attr(i);
        if a.attisdropped {
            continue;
        }
        lsyscache::statistics::get_attavgwidth(relid, (i + 1) as i16)?;
        lsyscache::typ::get_typlenbyval(a.atttypid)?;
        warmed += 1;
    }
    // the generation's part list (`residency::LISTS`, once per process): built here, not by the
    // first statement's prepare (~0.35 ms of a fresh server's first statement; cold-floor.md §2)
    gravity_hosts::ops::scan::pgrc::residency::resident_parts(&rel, None)?;
    table::table_close(rel, AccessShareLock).map_err(|e: Box<PgError>| e)?;
    Ok(warmed)
}

/// Every `pg_catalog` table and index (the shared catalogs included), each catalog's toast table
/// and its indexes: all main-fork blocks into shared buffers. (catalogs, blocks).
fn warm_catalogs(mcx: mcx::Mcx<'_>) -> PgResult<(u64, u64)> {
    let class = table::table_open(mcx, RELATION_RELATION_ID, AccessShareLock)?;
    let desc = class.descr();
    let mut scan = genam::systable_beginscan(mcx, &class, InvalidOid, false, None, &[])?;
    let mut targets: Vec<Oid> = Vec::new(); // alloc: Cold
    while let Some(tup) = genam::systable_getnext(mcx, &mut scan)? {
        let (nsp, kind, oid, toast) = raw::class_row(tup, desc);
        if nsp != PG_CATALOG_NAMESPACE || (kind != RELKIND_RELATION && kind != RELKIND_INDEX) {
            continue;
        }
        targets.push(oid);
        if toast != InvalidOid {
            targets.push(toast);
            for idx in relcache::RelationGetIndexList(mcx, toast)?.iter() {
                targets.push(*idx);
            }
        }
    }
    genam::systable_endscan(mcx, scan)?;
    class.close(AccessShareLock)?;
    let mut blocks = 0u64;
    for oid in &targets {
        blocks += warm_blocks(mcx, *oid)?;
    }
    Ok((targets.len() as u64, blocks))
}

fn warm_blocks(mcx: mcx::Mcx<'_>, relid: Oid) -> PgResult<u64> {
    let rel = relation::relation_open(mcx, relid, AccessShareLock)?;
    let nblocks = bufmgr::RelationGetNumberOfBlocksInFork(&rel, MAIN_FORKNUM)?;
    let mut done = 0u64;
    if nblocks <= CATALOG_BLOCKS_MAX {
        for block in 0..nblocks {
            let buf = bufmgr::ReadBufferExtended(&rel, MAIN_FORKNUM, block, ReadBufferMode::Normal, None)?;
            bufmgr::ReleaseBuffer(buf)?;
            done += 1;
        }
    }
    rel.close(AccessShareLock)?;
    Ok(done)
}

#[cfg(test)]
mod tests {
    /// A disabled startup prewarm registers no worker and completes without a start step.
    #[test]
    fn the_switch_off_at_start_skips_the_warm() {
        super::register_if(false).expect("register");
        assert!(!super::REGISTERED.load(super::Ordering::Acquire), "no warm worker registered");
        super::arm();
        assert!(super::DONE.load(super::Ordering::Acquire), "armed as done");
        let ticks = std::cell::Cell::new(0u32);
        assert!(super::wait(&|| ticks.set(ticks.get() + 1)), "the wait returns at once");
        assert_eq!(ticks.get(), 0, "no start step run");
    }
}
