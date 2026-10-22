//! # pgrc_am — the pgrcolumnar table-AM glue (chunk M3-H)
//!
//! DDL/AM integration for the pgrcolumnar storage system
//! (`docs/design/lanev3-m3-chunks.md` §2/§5 M3-H row; format spec
//! `docs/design/pgrc-format.md`, cited as `spec §N`): the `pgrcolumnar`
//! AM identity (O-M3-2: sibling name, one DDL-minted pg_am row during
//! coexistence, NO format-routing knob), the O-7 per-table directory
//! lifecycle, COPY routing into the frozen M3-D writer face, the scan face
//! over the frozen M3-F reader, and the O-8/O-M3-1 typed-refusal surfaces.
//!
//! ## The settled design (recorded here, cited by the lane report)
//!
//! - **Registration is the closed-AM name probe** (the old-AM precedent,
//!   `docs/design/pgrcolumnar-impl.md` §7.1): a user-minted pg_am row
//!   `CREATE ACCESS METHOD pgrcolumnar TYPE TABLE HANDLER
//!   heap_tableam_handler` is recognized by `pg_am.amname == "pgrcolumnar"`
//!   at relcache entry build and recorded in `tableam_vocab`'s sibling
//!   registry; handlers are never invoked. No new pg_proc/fmgr rows — the
//!   se-entrycost surface is untouched by construction.
//! - **O-7 directory lifecycle.** Table data lives in real files in a
//!   per-table directory, a SIBLING of the main-fork path
//!   ([`dirpath::table_dir_path`]; names frozen in
//!   `pgrc_format::dirlayout`). The main fork itself stays a normal empty
//!   smgr file (created/dropped by the ordinary catalog machinery), which is
//!   what kills the #142/#136/#152 main-fork-squatting incident classes. The
//!   DIRECTORY rides the relation's smgr storage: created (and logged, `DIR_CREATE`) with a new relfilenumber
//!   (`tableam`), removed where smgr unlinks the main fork (`catalog_storage`: commit, abort, redo, 2PC). Crash
//!   residue inside a directory is removed by `pgrc_wal`'s end-of-recovery cleanup, before any reader.
//! - **COPY + SELECT only (O-M3-1a).** `table_tuple_insert`/`multi_insert`
//!   route here from COPY and the bulk receivers (CTAS/matview refresh —
//!   they end in `table_finish_bulk_insert`, the publish point). Trickle
//!   INSERT/UPDATE/DELETE refuse TYPED at the ModifyTable gate before any
//!   AM call; UPDATE/DELETE/lock/ON CONFLICT arms in tableam refuse typed
//!   as well (belt and braces). Bitmap and sample scans refuse typed per
//!   the exactness-sweep C12 posture — clean errors, never an invalid TID
//!   (IX1 L0: TID scans and WHERE CURRENT OF fetch by row ID, `pgrc_front::ixfetch`).
//! - **Visibility is the old-AM law at manifest grain** ([`probe`]):
//!   a generation is visible iff its `publisher_fxid`'s xid is the current
//!   transaction's, or committed AND not in the scan snapshot
//!   (`XidInMVCCSnapshot`). Walking the `prev_gen` chain past invisible
//!   generations yields exactly the table state the snapshot may see
//!   (manifests are append-only states).
//! - **Publish serialization**: generational manifests require one publisher
//!   at a time per table; pgrust is thread-per-backend in ONE process, so a
//!   process-global per-table mutex ([`inval::publish_lock`]) held across
//!   `TableWriter::publish` is sufficient single-node serialization (O-3
//!   scope). Documented limit: cross-process writers (none exist) would
//!   need a heavyweight lock.
//! - **The ino-reuse hole is closed here** ([`inval`]): every part opened
//!   for a scan records its `(dev, ino, len)` cache key under the relation
//!   oid; the relcache invalidation callback (register-BEFORE-first-insert
//!   law, the part_cache precedent) drops those keys from the shared
//!   `PartRegistry`, so a DROP+recreate that reuses an inode at equal
//!   length can never serve stale bytes to a later scan.
//! - **Session state is one `thread_local!` block** ([`session`]) — the
//!   TLS census counts declaration blocks; this crate adds exactly one
//!   (ledgered in the session-surface census test).
//!
//! ## Not here (owned elsewhere)
//!
//! Codec kernels/elections (M3-C; with none wired into the writer's
//! `CandidateSource` seam yet, COPY through this crate elects
//! VERBATIM/CONST only — a REPORTED composition gap, not a silent one),
//! real meta builders (M3-E), the lx_source columnar implementor and
//! parallel claims at execution grain (M3-G), parallel ingest (M3-I),
//! trickle DML/tombstones/DV (M5).

pub use pgrc_format as format;

pub mod analyze;
pub mod answers;
pub mod catalog;
pub mod compact;
pub mod dircopy;
pub mod dirpath;
pub mod dml;
pub mod fastadd;
pub mod footer;
pub mod front_claim;
pub mod holds;
pub mod ingest;
pub mod inval;
// sortpath (trunk 3fc0e46c141 + 8d42eadfb27): the pgrc_part_layout / pgrc_part_streams debug faces
pub mod layout;
pub mod l0;
pub mod mapcache;
pub mod planner_facts;
pub mod probe;
pub mod reloptions;
pub mod rowid_alloc;
pub mod runcache;
pub mod runmerge;
pub mod rowcache;
pub mod scan;
pub mod scanstats;
pub mod schema;
pub mod session;
pub mod slot_events;
pub mod vacuum;
pub mod walgen;

#[cfg(test)]
mod tests;

/// D4372: the unit tests allocate through the server's allocator wrapper (main_main's, over the system allocator here), so
/// every block counts on its thread's memory ledger account (`memledger::door_heap`) as it does in the server.
#[cfg(test)]
#[global_allocator]
static TEST_ALLOC: init_small::oom::Rescue<std::alloc::System> = init_small::oom::Rescue(std::alloc::System);

use types_error::{PgError, PgResult};

/// The SQL-surface AM name (O-M3-2 ruling: sibling name, frozen).
pub const AM_NAME: &str = "pgrcolumnar";

/// Typed refusal, the ONE error identity for every unsupported surface
/// (exactness-sweep C12 posture: clean 0A000, never an invalid TID, never a
/// panic on a reachable path). Message shape: `pgrcolumnar does not support {what}`.
pub fn unsupported(what: &str) -> Box<PgError> {
    Box::<PgError>::new( // alloc: Cold
        PgError::error(format!("pgrcolumnar does not support {what}")) // alloc: Cold
            .with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED),
    )
}

/// D4826: a row of a part written before row IDs (IX0) has no position, so what reads one (a ctid, a row's identity for
/// DML or a lock) is refused.
pub fn no_row_ids(name: &str) -> Box<PgError> {
    Box::<PgError>::new( // alloc: Cold
        PgError::error(format!("pgrcolumnar: the row of \"{name}\" is in a part written before row IDs and has no position")) // alloc: Cold
            .with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED)
            .with_hint("Compaction or VACUUM FULL gives the part row IDs."),
    )
}

/// Typed data-corruption refusal, the ingest-boundary identity for an
/// untrusted on-tuple image whose header cannot be honored (a varlena whose
/// header claims more bytes than the materialized tuple actually backs, a
/// by-reference datum pointing outside its tuple image). Mirrors the
/// [`write_error`]/[`read_error`] `ERRCODE_DATA_CORRUPTED` mapping so a
/// crafted heap page is refused loudly, never over-read.
pub fn corrupt(what: &str) -> Box<PgError> {
    Box::<PgError>::new( // alloc: Cold
        PgError::error(format!("pgrcolumnar: {what}")) // alloc: Cold
            .with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED),
    )
}

/// D4853: a v0.3.x pgrcolumnar table (a segment file at the main fork, no table directory) is refused, never read as
/// empty; DROP TABLE still works.
pub fn refuse_v1_format(rel: &types_rel::Relation<'_>) -> PgResult<()> {
    let (locator, backend) = (rel.rd_locator.get(), rel.rd_backend);
    if dirpath::dir_exists(&dirpath::table_dir_path(locator, backend))? || !dirpath::v1_segment(locator, backend) {
        return Ok(());
    }
    Err(Box::<PgError>::new( // alloc: Cold
        PgError::error(format!("pgrcolumnar: table \"{}\" is in the v0.3.x columnar format, which this server does not read", rel.name())) // alloc: Cold
            .with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED)
            .with_hint("Dump pgrcolumnar tables with the v0.3.x binary before upgrading, and restore them after."),
    ))
}

/// A part file's I/O failure in errcode_for_file_access's class, as C's file access errors (EACCES 42501, ENOENT 58P01,
/// ENOSPC 53100, ...); an errno it has no class for (0: a synthesized failure, a short read) stays an I/O error.
fn io_sqlstate(errno: i32) -> types_error::SqlState {
    match elog::errno::sqlstate_for_file_access(errno) {
        types_error::ERRCODE_INTERNAL_ERROR => types_error::ERRCODE_IO_ERROR,
        s => s,
    }
}

/// An I/O failure's message with the errno's text, as C's %m ("...: Permission denied"); errno 0 (synthesized) has none.
#[cold]
fn io_message(e: &dyn core::fmt::Display, errno: i32) -> String {
    if errno == 0 {
        format!("{e}") // alloc: Cold
    } else {
        format!("{e}: {}", elog::errno::strerror(errno)) // alloc: Cold
    }
}

/// Map a writer error to a PgError with a class-honest sqlstate: refusals
/// are 0A000, format/manifest damage is data corruption, I/O is io error.
pub fn write_error(e: pgrc_write::WriteError) -> Box<PgError> {
    use pgrc_write::WriteError as W;
    if matches!(e, W::Contract { detail } if detail == pgrc_write::walsink::WAL_INSERT_FAILED)
        && pgrc_wal_seams::take_insert_error::is_installed()
    {
        if let Some(pe) = pgrc_wal_seams::take_insert_error::call() {
            return pe;
        }
    }
    if let W::Refused { what } = &e {
        // D4365: the load's ladder gate refused it (SPILL-DEBT LoadSealPastLine, D4370): the client's 53200 as
        // MEMORY_REFUSED's below, and (NR-3) the row's line in the server log only, unless the statement's own cancel or
        // limit refuses it (then that error, as below, and no line)
        if *what == pgrc_write::par::LOAD_PAST_LINE {
            return Box::new(mcx::governor::refusal(what).unwrap_or_else(|| {
                let _ = elog::elog(types_error::LOG_SERVER_ONLY, format!("gravity: memory: SPILL-DEBT LoadSealPastLine: {}", pgrc_write::par::LOAD_SEAL_PAST_LINE)); // alloc: Cold
                PgError::error("out of memory")
                    .with_sqlstate(types_error::ERRCODE_OUT_OF_MEMORY)
                    .with_detail(format!("The memory governor refused a {}.", pgrc_write::par::MEMORY_REFUSED)) // alloc: Cold
            }));
        }
        if *what == pgrc_write::par::MEMORY_REFUSED {
            return Box::new(mcx::governor::refusal(what).unwrap_or_else(|| {
                PgError::error("out of memory")
                    .with_sqlstate(types_error::ERRCODE_OUT_OF_MEMORY)
                    .with_detail(format!("The memory governor refused a {what}.")) // alloc: Cold
            }));
        }
    }
    if let W::Format(f @ pgrc_format::FormatError::OutOfMemory { .. }) = &e {
        return format_error(*f);
    }
    let sqlstate = match &e {
        W::Refused { .. } => types_error::ERRCODE_FEATURE_NOT_SUPPORTED,
        W::Format(_) | W::TwoWitnessSkew { .. } | W::RoundTrip { .. } | W::ManifestChain { .. } => {
            types_error::ERRCODE_DATA_CORRUPTED
        }
        W::Io { errno, .. } => io_sqlstate(*errno),
        W::Contract { .. } | W::SweptAlive { .. } => types_error::ERRCODE_INTERNAL_ERROR,
        // PUBLISH-WAIT: the caller waits it out (`ingest::publish_serialized`); surfaced only as a background deferral
        W::PublishBusy { .. } => types_error::ERRCODE_LOCK_NOT_AVAILABLE,
    };
    if let W::Io { errno, .. } = &e {
        return Box::new(PgError::error(io_message(&e, *errno)).with_sqlstate(sqlstate)); // alloc: Cold
    }
    Box::new(PgError::error(format!("{e}")).with_sqlstate(sqlstate)) // alloc: Cold
}

/// A format error as a PgError: an allocation failure is ERROR 53200, everything else is data corruption (a CRC mismatch
/// also counts as a checksum failure).
pub fn format_error(e: pgrc_format::FormatError) -> Box<PgError> {
    match e {
        pgrc_format::FormatError::OutOfMemory { bytes } => Box::new(mcx::oom_named("pgrcolumnar", bytes)),
        pgrc_format::FormatError::CrcMismatch { .. } => {
            note_crc_mismatch();
            Box::new(PgError::error(format!("{e}")).with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED)) // alloc: Cold
        }
        other => Box::new(PgError::error(format!("{other}")).with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED)), // alloc: Cold
    }
}

/// The fallible-allocation helpers of `pgrc_format`, with the error a PgError (ERROR 53200 on failure).
pub mod alloc {
    use super::{format_error, PgError};
    type R<T> = Result<T, Box<PgError>>;

    pub fn try_vec<T>(n: usize, at: &'static str) -> R<Vec<T>> {
        pgrc_format::try_vec(n, at).map_err(format_error)
    }
    pub fn try_filled<T: Clone>(n: usize, val: T, at: &'static str) -> R<Vec<T>> {
        pgrc_format::try_filled(n, val, at).map_err(format_error)
    }
    pub fn try_to_vec<T: Clone>(s: &[T], at: &'static str) -> R<Vec<T>> {
        pgrc_format::try_to_vec(s, at).map_err(format_error)
    }
    pub fn try_resize<T: Clone>(v: &mut Vec<T>, n: usize, val: T, at: &'static str) -> R<()> {
        pgrc_format::try_resize(v, n, val, at).map_err(format_error)
    }
    pub fn try_reserve_more<T>(v: &mut Vec<T>, extra: usize, at: &'static str) -> R<()> {
        pgrc_format::try_reserve_more(v, extra, at).map_err(format_error)
    }
}

/// CRC mismatches [`read_error`] has mapped since the process started (the load at server start reads it around each
/// directory to tell a checksum failure from other damage).
static CRC_MISMATCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The count of CRC mismatches [`read_error`] has mapped in this process.
pub fn crc_mismatches() -> u64 {
    CRC_MISMATCHES.load(std::sync::atomic::Ordering::Relaxed)
}

/// A CRC mismatch was mapped: counted for [`crc_mismatches`] and reported as a checksum failure.
fn note_crc_mismatch() {
    CRC_MISMATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    report_checksum_failure();
}

/// A pgrcolumnar CRC mismatch counts in pg_stat_database.checksum_failures, as a heap page that fails verification
/// does (bufmgr's PageIsVerified report); outside a database (the load at server start) nothing is counted here.
pub fn report_checksum_failure() {
    let db = init_small::globals::MyDatabaseId();
    if db != 0 {
        pgstat::pgstat_prepare_report_checksum_failure(db);
        pgstat::pgstat_report_checksum_failures_in_db(db, 1);
    }
}

/// Map a reader error likewise. A CRC mismatch is data corruption (XX001) and counts as a checksum failure.
pub fn read_error(e: pgrc_read::ReadError) -> Box<PgError> {
    use pgrc_read::ReadError as R;
    if let R::Format(f @ pgrc_format::FormatError::OutOfMemory { .. }) = &e {
        return format_error(*f);
    }
    if matches!(&e, R::Format(pgrc_format::FormatError::CrcMismatch { .. })) {
        note_crc_mismatch();
    }
    let sqlstate = match &e {
        R::Io { errno, .. } if *errno == libc::ENOMEM => types_error::ERRCODE_OUT_OF_MEMORY,
        R::Unsupported { .. } => types_error::ERRCODE_FEATURE_NOT_SUPPORTED,
        R::Io { errno, .. } => io_sqlstate(*errno),
        _ => types_error::ERRCODE_DATA_CORRUPTED,
    };
    if let R::Io { errno, .. } = &e {
        return Box::new(PgError::error(io_message(&e, *errno)).with_sqlstate(sqlstate)); // alloc: Cold
    }
    Box::new(PgError::error(format!("{e}")).with_sqlstate(sqlstate)) // alloc: Cold
}

/// Log (LOG) every result-neutral accelerator that was skipped for lack of memory since the last drain: the structure
/// name, how many, and the bytes. A skipped structure is "not built"; readers fall back and results do not change.
pub(crate) fn log_accel_skips() {
    for (what, count, bytes) in pgrc_format::take_accel_skips() {
        let _ = elog::elog(
            types_error::LOG,
            format!("pgrcolumnar: out of memory, skipped {count} {what} build(s) ({bytes} bytes); the data is read without it"), // alloc: Cold
        ); // alloc: Cold
    }
}

/// A best-effort build failed to allocate: count it, log it, and answer `None` (not built).
pub(crate) fn accel_skipped<T>(what: pgrc_format::Accel, bytes: usize) -> Option<T> {
    pgrc_format::note_accel_skipped(what, bytes);
    log_accel_skips();
    None
}
