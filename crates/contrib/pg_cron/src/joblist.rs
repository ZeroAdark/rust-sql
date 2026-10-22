//! The launcher's view of cron.job (C: job_metadata.c LoadCronJobList,

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use datum::Datum;
use mcx::Mcx;
use types_core::{InvalidOid, Oid, NAMEOID};
use types_error::{PgResult, WARNING, ERRCODE_INSUFFICIENT_PRIVILEGE, LOG};
use types_storage::lock::AccessShareLock;

use crate::catalog::{self, tuple_text};
use crate::gucs;
use crate::schedule::{parse_schedule, Entry};

#[track_caller]
fn loc(func: &'static str) -> types_error::ErrorLocation {
    let site = core::panic::Location::caller();
    types_error::ErrorLocation::new(site.file(), site.line() as i32, func)
}

/// `CronJob` (job_metadata.h).
#[derive(Clone, Debug)]
pub struct CronJob {
    pub job_id: i64,
    pub schedule_text: String,
    /// Parsed schedule; an all-zero Entry (never fires) when the text failed
    pub schedule: Entry,
    pub command: String,
    pub node_name: String,
    pub node_port: i32,
    pub database: String,
    pub user_name: String,
    pub active: bool,
    pub job_name: Option<String>,
}

/// `CronJobCacheValid`.
static CACHE_VALID: AtomicBool = AtomicBool::new(false);
/// `CachedCronJobRelationId`: what the relcache callback compares against.
static CACHED_RELID: AtomicU32 = AtomicU32::new(InvalidOid);

pub fn cache_valid() -> bool {
    CACHE_VALID.load(Ordering::Relaxed)
}

pub fn set_cache_valid(v: bool) {
    CACHE_VALID.store(v, Ordering::Relaxed);
}

/// `InvalidateJobCacheCallback` (job_metadata.c:827).
fn invalidate_job_cache_callback(_arg: Datum, relid: Oid) {
    let cached = CACHED_RELID.load(Ordering::Relaxed);
    if relid == cached || relid == InvalidOid || cached == InvalidOid {
        CACHE_VALID.store(false, Ordering::Relaxed);
        CACHED_RELID.store(InvalidOid, Ordering::Relaxed);
    }
}

/// C registers this in `_PG_init`; pgrust's relcache callback list is
pub fn register_invalidation_callback() -> PgResult<()> {
    inval::invalidate::CacheRegisterRelcacheCallback(invalidate_job_cache_callback, Datum::null())
}

/// Name-typed column as a String (NameStr).
fn tuple_name(tup: &types_tuple::HeapTupleData<'_>, desc: &types_tuple::TupleDescData<'_>, attno: i32) -> Option<String> {
    let mut isnull = false;
    // SAFETY: a cron.job tuple under cron.job's descriptor.
    let d = unsafe { types_tuple::heap_getattr(tup, attno, desc, &mut isnull) };
    if isnull {
        return None;
    }
    // SAFETY: a NameData datum: NAMEDATALEN NUL-padded bytes.
    let bytes = unsafe {
        core::slice::from_raw_parts(d.as_usize() as *const u8, types_core::fmgr::NAMEDATALEN as usize)
    };
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
}

fn fixed_attr(tup: &types_tuple::HeapTupleData<'_>, desc: &types_tuple::TupleDescData<'_>, attno: i32) -> Datum {
    let mut isnull = false;
    // SAFETY: a cron.job tuple under cron.job's descriptor; the column is
    unsafe { types_tuple::heap_getattr(tup, attno, desc, &mut isnull) }
}

/// `TupleToCronJob` (job_metadata.c:933). None = skipped (superuser job
fn tuple_to_cron_job(
    mcx: Mcx<'_>,
    tup: &types_tuple::HeapTupleData<'_>,
    desc: &types_tuple::TupleDescData<'_>,
) -> PgResult<Option<CronJob>> {
    let job_id = fixed_attr(tup, desc, catalog::Anum_cron_job_jobid).as_i64();
    let owner = tuple_text(mcx, tup, desc, catalog::Anum_cron_job_username)?.unwrap_or_default();
    let owner_id = acl_seams::get_role_oid::call(&owner, false)?;
    if !gucs::enable_superuser_jobs() && superuser_seams::superuser_arg::call(owner_id)? {
        let _ = elog::ereport(WARNING)
            .errcode(ERRCODE_INSUFFICIENT_PRIVILEGE)
            .errmsg(format!("skipping job {job_id} since superuser jobs are currently disallowed"))
            .finish(loc("TupleToCronJob"));
        return Ok(None);
    }

    let schedule_text = tuple_text(mcx, tup, desc, catalog::Anum_cron_job_schedule)?.unwrap_or_default();
    let command = tuple_text(mcx, tup, desc, catalog::Anum_cron_job_command)?.unwrap_or_default();
    let node_name = tuple_text(mcx, tup, desc, catalog::Anum_cron_job_nodename)?.unwrap_or_default();
    let node_port = fixed_attr(tup, desc, catalog::Anum_cron_job_nodeport).as_i32();
    let database = tuple_text(mcx, tup, desc, catalog::Anum_cron_job_database)?.unwrap_or_default();

    let active = if i32::from(tup.t_data().natts()) >= catalog::Anum_cron_job_active {
        fixed_attr(tup, desc, catalog::Anum_cron_job_active).as_bool()
    } else {
        true
    };
    let job_name = if desc.natts >= catalog::Anum_cron_job_jobname {
        if desc.attr((catalog::Anum_cron_job_jobname - 1) as usize).atttypid == NAMEOID {
            tuple_name(tup, desc, catalog::Anum_cron_job_jobname)
        } else {
            tuple_text(mcx, tup, desc, catalog::Anum_cron_job_jobname)?
        }
    } else {
        None
    };

    let schedule = match parse_schedule(&schedule_text) {
        Some(e) => e,
        None => {
            let _ = elog::elog(
                LOG,
                format!("invalid pg_cron schedule for job {job_id}: {schedule_text}"),
            );
            Entry::default()
        }
    };

    Ok(Some(CronJob {
        job_id,
        schedule_text,
        schedule,
        command,
        node_name,
        node_port,
        database,
        user_name: owner,
        active,
        job_name,
    }))
}

/// `LoadCronJobList` (job_metadata.c:859): its own transaction; empty when
pub fn load_cron_job_list() -> PgResult<Vec<CronJob>> {
    let cx = mcx::MemoryContext::new("pg_cron job list");
    let mcx = cx.mcx();
    xact::SetCurrentStatementStartTimestamp();
    xact::StartTransactionCommand()?;
    let snap = snapmgr::GetTransactionSnapshot()?;
    snapmgr::PushActiveSnapshot(&snap)?;

    let result = (|| -> PgResult<Vec<CronJob>> {
        if !catalog::pg_cron_has_been_loaded()? || transam_xlog::RecoveryInProgress() {
            return Ok(Vec::new());
        }
        let relid = catalog::cron_job_relation_id()?;
        CACHED_RELID.store(relid, Ordering::Relaxed);
        let rel = table::table_open(mcx, relid, AccessShareLock)?;
        let desc = rel.descr();
        let mut jobs = Vec::new();
        let mut scan = genam::systable_beginscan(mcx, &rel, InvalidOid, false, None, &[])?;
        while let Some(tup) = genam::systable_getnext(mcx, &mut scan)? {
            if let Some(job) = tuple_to_cron_job(mcx, tup, desc)? {
                jobs.push(job);
            }
        }
        genam::systable_endscan(mcx, scan)?;
        rel.close(AccessShareLock)?;
        Ok(jobs)
    })();

    match result {
        Ok(jobs) => {
            snapmgr::PopActiveSnapshot()?;
            xact::CommitTransactionCommand()?;
            Ok(jobs)
        }
        Err(e) => {
            Err(e)
        }
    }
}
