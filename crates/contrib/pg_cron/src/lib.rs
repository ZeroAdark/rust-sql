//! Port of citusdata/pg_cron 1.6 (github.com/citusdata/pg_cron, not part of

pub mod catalog;
pub mod gucs;
pub mod joblist;
pub mod launcher;
pub mod rundetails;
pub mod schedule;
pub mod task;
pub mod worker;

use std::sync::atomic::{AtomicBool, Ordering};

use types_error::PgResult;
use types_fmgr::PGFunction;

const LIBRARY: &str = "pg_cron";

/// Set by `pg_init` when loaded under shared_preload_libraries /
static LAUNCHER_REGISTERED: AtomicBool = AtomicBool::new(false);

pub fn launcher_registered() -> bool {
    LAUNCHER_REGISTERED.load(Ordering::Relaxed)
}

fn lookup(function: &str) -> Option<PGFunction> {
    catalog::lookup(function)
}

pub fn init_seams() {
    gucs::install();
    dfmgr::register_builtin_library(dfmgr::BuiltinLibraryEntry {
        name: LIBRARY,
        lookup,
        pg_init: Some(pg_init),
    });
}

/// `_PG_init` (pg_cron.c:220). C errors out unless loaded via
fn pg_init() -> PgResult<()> {
    guc::MarkGUCPrefixReserved("cron");
    if !miscinit::process_shared_preload_libraries_in_progress() {
        return Ok(());
    }

    if !gucs::use_background_workers() {
        elog::elog(
            types_error::LOG,
            "pg_cron: cron.use_background_workers=off is not supported by pgrust; \
             jobs will run as background workers",
        )?;
    }

    let cap = guc_tables::vars::max_worker_processes.read() - 1;
    if gucs::max_running_jobs() > cap {
        elog::elog(
            types_error::WARNING,
            format!(
                "cron.max_running_jobs ({}) exceeds max_worker_processes - 1 ({cap}); using {cap}",
                gucs::max_running_jobs()
            ),
        )?;
        gucs::set_max_running_jobs(cap.max(0));
    }

    launcher::register()?;
    LAUNCHER_REGISTERED.store(true, Ordering::Relaxed);
    Ok(())
}
