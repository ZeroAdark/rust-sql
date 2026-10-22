//! Priming at server start (docs/gravity/hosts.md): every columnar relation's planner facts into
//! the process fact cache (no plan walks parts), then its parts into the residency table.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use types_error::PgResult;
use types_storage::RelFileLocator;

const DEFAULT_TABLESPACE: u32 = 1663;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Primed {
    pub relations: u32,
    pub generations: u32,
    pub parts: u32,
    pub errors: u32,
    pub complete: u32,
    pub completed: u32,
    pub section_bytes: u64,
    pub completion_micros: u64,
    pub micros: u64,
}

static KICKED: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);
static RELATIONS: AtomicU64 = AtomicU64::new(0);
static GENERATIONS: AtomicU64 = AtomicU64::new(0);
static PARTS: AtomicU64 = AtomicU64::new(0);
static ERRORS: AtomicU64 = AtomicU64::new(0);
static FACTS_LOADED: AtomicU64 = AtomicU64::new(0);
static FACTS_FOLDED: AtomicU64 = AtomicU64::new(0);
static FACTS_BYTES: AtomicU64 = AtomicU64::new(0);
static COMPLETION_MICROS: AtomicU64 = AtomicU64::new(0);
static MICROS: AtomicU64 = AtomicU64::new(0);

/// Once per process: the postmaster at hot-standby or server-ready (`launch_backend::rtpool`)
/// or, without it, the first backend to serve hands the load to `spawn` (a pool thread with
/// temp-file arming and the PGPROC gate); true the first time. `wait_done` blocks until it finished.
/// A primary's startup process may have loaded already ([`after_recovery`]), so this returns false then.
pub fn kick_off(spawn: &dyn Fn(Box<dyn FnOnce() + Send>) -> bool) -> bool {
    if KICKED.swap(true, Ordering::AcqRel) {
        return false;
    }
    crate::ops::scan::heap_window::probe_device();
    spawn(Box::new(|| load(prime_columnar()))) // alloc: Cold
}

fn load(p: PgResult<Primed>) {
    let p = p.unwrap_or_else(|_| Primed { errors: 1, ..Primed::default() });
    RELATIONS.store(p.relations as u64, Ordering::Relaxed);
    GENERATIONS.store(p.generations as u64, Ordering::Relaxed);
    PARTS.store(p.parts as u64, Ordering::Relaxed);
    ERRORS.store(p.errors as u64, Ordering::Relaxed);
    FACTS_LOADED.store(p.complete as u64, Ordering::Relaxed);
    FACTS_FOLDED.store(p.completed as u64, Ordering::Relaxed);
    FACTS_BYTES.store(p.section_bytes, Ordering::Relaxed);
    COMPLETION_MICROS.store(p.completion_micros, Ordering::Relaxed);
    MICROS.store(p.micros, Ordering::Relaxed);
    pgrc_am::catalog::log(false, format!("gravity: columnar catalog loaded: {} relations, {} generations, {} parts, sections {} complete / {} completed in {} us ({} bytes held), {} errors, {} us, part fds {} open of {}", p.relations, p.generations, p.parts, p.complete, p.completed, p.completion_micros, p.section_bytes, p.errors, p.micros, pgrc_read::fdpool::census().0, pgrc_read::fdpool::budget())); // alloc: Cold
    DONE.store(true, Ordering::Release);
}

/// The startup process after StartupXLOG (redo done, no connection yet): at a primary's start, at a standby's
/// promotion and at every in-process crash restart (startup_seams `columnar_after_recovery`, launch_backend's
/// `rtpool::gravity_after_recovery`). pgrc's WAL redo recovers every directory (pgrc_wal `rm_cleanup`) and keeps a live
/// catalog current, so a live catalog (a hot standby's, loaded at its consistency point by `kick_off`; the last cycle's
/// after an in-process crash restart) is never cleared or loaded again here: a promoted standby's sessions read it. A
/// catalog not live yet (a primary's start) is loaded now, as `kick_off` would at server ready, which then finds it
/// kicked. `_live_groups` (the settle wait for a dead leader's runtime groups before a crash restart's reload, D2876 /
/// D2883 / NEW-36) has nothing to wait for: a live catalog is never reloaded.
pub fn after_recovery(_live_groups: &dyn Fn() -> u64) -> PgResult<()> {
    if pgrc_am::catalog::is_live() || KICKED.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    crate::ops::scan::heap_window::probe_device();
    load(prime_columnar());
    Ok(())
}

/// Block until the load finished (the postmaster, before it accepts connections); false when
/// nothing was kicked. The 2 ms poll sleeps through the waiter (rides the clock provider: virtual
/// time under DST), not `thread::sleep`.
pub fn wait_done() -> bool {
    if !KICKED.load(Ordering::Acquire) {
        return false;
    }
    // bounded("other: the postmaster's wait for server start's columnar load, before it accepts a connection: no statement runs, so no statement's cancel applies")
    while !DONE.load(Ordering::Acquire) {
        waiter::sleep(std::time::Duration::from_millis(2));
    }
    true
}

/// The census once the load finished; `None` before.
pub fn report() -> Option<Primed> {
    DONE.load(Ordering::Acquire).then(|| Primed {
        relations: RELATIONS.load(Ordering::Relaxed) as u32,
        generations: GENERATIONS.load(Ordering::Relaxed) as u32,
        parts: PARTS.load(Ordering::Relaxed) as u32,
        errors: ERRORS.load(Ordering::Relaxed) as u32,
        complete: FACTS_LOADED.load(Ordering::Relaxed) as u32,
        completed: FACTS_FOLDED.load(Ordering::Relaxed) as u32,
        section_bytes: FACTS_BYTES.load(Ordering::Relaxed),
        completion_micros: COMPLETION_MICROS.load(Ordering::Relaxed),
        micros: MICROS.load(Ordering::Relaxed),
    })
}

/// `pgrc_<relfilenumber>` → the relfilenumber.
pub fn parse_table_dir(name: &str) -> Option<u32> {
    name.strip_prefix("pgrc_")?.parse().ok()
}

/// The entry names of the data-directory path `dir` through the VFS (the fd layer's syscall choke:
/// the sim namespace under DST), "." and ".." elided; none when it cannot be read.
fn dir_names(dir: &str) -> Vec<String> {
    let Ok(c) = std::ffi::CString::new(dir) else { return Vec::new() }; // alloc: Cold
    vfs::read_dir(&c).map_or_else(|_| Vec::new(), |it| it.flatten().map(|p| String::from_utf8_lossy(p.as_bytes()).into_owned()).collect()) // alloc: Cold
}

/// The columnar table directories of the cluster (relative to the data directory, the server's
/// cwd): `base/<db>/pgrc_<n>` and `pg_tblspc/<spc>/<PG_x>/<db>/pgrc_<n>`.
pub fn columnar_dirs() -> Vec<(RelFileLocator, String)> {
    let mut out = Vec::new(); // alloc: Cold
    let mut scan_db = |spc: u32, db_path: &str, db: u32| {
        // bounded("other: server start's columnar load, before the postmaster accepts a connection: no statement runs, so no statement's cancel applies")
        for name in dir_names(db_path) {
            if let Some(rel) = parse_table_dir(&name) {
                out.push((RelFileLocator { spcOid: spc, dbOid: db, relNumber: rel }, format!("{db_path}/{name}"))); // alloc: Cold
            }
        }
    };
    // bounded("other: server start's columnar load, before the postmaster accepts a connection: no statement runs, so no statement's cancel applies")
    for db in dir_names("base") {
        if let Ok(oid) = db.parse::<u32>() {
            scan_db(DEFAULT_TABLESPACE, &format!("base/{db}"), oid); // alloc: Cold
        }
    }
    // bounded("other: server start's columnar load, before the postmaster accepts a connection: no statement runs, so no statement's cancel applies")
    for spc in dir_names("pg_tblspc") {
        let Ok(spc_oid) = spc.parse::<u32>() else { continue };
        let spc_path = format!("pg_tblspc/{spc}"); // alloc: Cold
        // bounded("other: server start's columnar load, before the postmaster accepts a connection: no statement runs, so no statement's cancel applies")
        for v in dir_names(&spc_path) {
            let v_path = format!("{spc_path}/{v}"); // alloc: Cold
            // bounded("other: server start's columnar load, before the postmaster accepts a connection: no statement runs, so no statement's cancel applies")
            for db in dir_names(&v_path) {
                if let Ok(oid) = db.parse::<u32>() {
                    scan_db(spc_oid, &format!("{v_path}/{db}"), oid); // alloc: Cold
                }
            }
        }
    }
    // bounded("other: server start's columnar load, before the postmaster accepts a connection: no statement runs, so no statement's cancel applies")
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

/// Every columnar directory into the process catalog (`pgrc_am::catalog::load_dir`: read-only, the generations a
/// snapshot can reach, parts opened and pinned, facts read or folded), live first so a hot standby's redo keeps it.
pub fn prime_columnar() -> PgResult<Primed> {
    pgrc_am::catalog::set_live();
    prime_dirs(columnar_dirs())
}

/// [`prime_columnar`] over `dirs` only.
fn prime_dirs(dirs: Vec<(RelFileLocator, String)>) -> PgResult<Primed> {
    let start = Instant::now();
    let mut p = Primed::default();
    // bounded("other: server start's columnar load, before the postmaster accepts a connection: no statement runs, so no statement's cancel applies")
    for (locator, dir) in dirs {
        let slot = (locator.spcOid, locator.dbOid, locator.relNumber as u64);
        let crc_before = pgrc_am::crc_mismatches();
        match pgrc_am::catalog::load_dir(slot, &dir, &|g| pgrc_am::footer::fold_generation(g), &|g| pgrc_am::footer::serve_generation_without_catalog(g)) {
            Ok(l) => {
                p.relations += 1;
                p.generations += l.gens;
                p.parts += l.parts;
                p.complete += l.complete;
                p.completed += l.completed;
                p.section_bytes += l.section_bytes;
                p.completion_micros += l.completion_micros;
            }
            Err(e) => {
                p.errors += 1;
                pgrc_am::catalog::log(true, format!("gravity: columnar catalog: {dir}: {e}")); // alloc: Cold
                // a statement that reads the relation reports this refusal (XX001 for damage, counted as a checksum
                // failure for a CRC mismatch), not an internal "not in the catalog" error
                pgrc_am::catalog::note_load_failed(&dir, &e, pgrc_am::crc_mismatches() != crc_before);
            }
        }
    }
    p.micros = start.elapsed().as_micros() as u64;
    Ok(p)
}

