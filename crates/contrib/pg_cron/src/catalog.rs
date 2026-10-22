#![allow(non_upper_case_globals)]
//! The SQL-callable surface and cron.job writes (C: job_metadata.c

use datum::Datum;
use mcx::Mcx;
use types_core::{
    InvalidOid, Oid, BOOLOID, INT4OID, INT8OID, NAMEOID, SECURITY_LOCAL_USERID_CHANGE, TEXTOID,
};
use types_error::{
    PgError, PgResult, ERRCODE_E_R_I_E_TRIGGER_PROTOCOL_VIOLATED, ERRCODE_INSUFFICIENT_PRIVILEGE,
    ERRCODE_INVALID_PARAMETER_VALUE, ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE,
};
use types_fmgr::{FmgrInfo, FunctionCallInfoBaseData as Fcinfo, PGFunction};
use types_storage::lock::{NoLock, RowExclusiveLock};

use crate::gucs;
use crate::schedule::parse_schedule;

pub const EXTENSION_NAME: &str = "pg_cron";
pub const CRON_SCHEMA_NAME: &str = "cron";
pub const JOBS_TABLE_NAME: &str = "job";
pub const JOB_ID_INDEX_NAME: &str = "job_pkey";
pub const JOB_RUN_DETAILS_TABLE_NAME: &str = "job_run_details";
pub const RUN_ID_SEQUENCE_NAME: &str = "cron.runid_seq";

/// cron.job attribute numbers (cron_job.h).
pub const Anum_cron_job_jobid: i32 = 1;
pub const Anum_cron_job_schedule: i32 = 2;
pub const Anum_cron_job_command: i32 = 3;
pub const Anum_cron_job_nodename: i32 = 4;
pub const Anum_cron_job_nodeport: i32 = 5;
pub const Anum_cron_job_database: i32 = 6;
pub const Anum_cron_job_username: i32 = 7;
pub const Anum_cron_job_active: i32 = 8;
pub const Anum_cron_job_jobname: i32 = 9;

/// pg_proc.dat int8eq (types_core::fmgr lacks it; verified against C:
const F_INT8EQ: types_core::RegProcedure = 467;

pub(crate) fn arg_text(fcinfo: &Fcinfo, i: usize) -> PgResult<String> {
    // SAFETY: a non-null text argument (callers check argisnull first).
    let v = unsafe { fcinfo.arg_varlena_packed(i)? };
    Ok(String::from_utf8_lossy(v.data()).into_owned())
}

fn arg_name_str(fcinfo: &Fcinfo, i: usize) -> String {
    // SAFETY: a non-null name argument.
    let n = unsafe { fcinfo.arg_name(i) };
    let end = n.iter().position(|&b| b == 0).unwrap_or(n.len());
    String::from_utf8_lossy(&n[..end]).into_owned()
}

/// CStringGetTextDatum for SPI parameters, allocated in `mcx`.
pub(crate) fn text_datum(mcx: Mcx<'_>, s: &str) -> PgResult<Datum> {
    Ok(types_fmgr::varlena_result(varlena::cstring_to_text(mcx, s.as_bytes())?))
}

#[cold]
#[inline(never)]
fn err(msg: impl Into<String>) -> Box<PgError> {
    Box::new(PgError::error(msg.into()))
}

#[cold]
#[inline(never)]
pub(crate) fn invalid_schedule(schedule: &str) -> Box<PgError> {
    Box::new(
        PgError::error(format!("invalid schedule: {schedule}"))
            .with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE)
            .with_hint("Use cron format (e.g. 5 4 * * *), or interval format '[1-59] seconds'"),
    )
}

#[cold]
#[inline(never)]
fn superuser_jobs_disallowed() -> Box<PgError> {
    Box::new(
        PgError::error("cannot schedule jobs as superuser")
            .with_sqlstate(ERRCODE_INSUFFICIENT_PRIVILEGE)
            .with_detail(
                "Scheduling jobs as superuser is disallowed when cron.enable_superuser_jobs is set to off.",
            ),
    )
}

fn current_user_name(mcx: Mcx<'_>) -> PgResult<String> {
    Ok(miscinit::GetUserNameFromId(mcx, miscinit::GetUserId(), false)?
        .map(|s| s.as_str().to_owned())
        .unwrap_or_default())
}

fn role_name(mcx: Mcx<'_>, roleid: Oid) -> PgResult<String> {
    Ok(miscinit::GetUserNameFromId(mcx, roleid, false)?
        .map(|s| s.as_str().to_owned())
        .unwrap_or_default())
}

/// `CronJobRelationId`: cron.job's oid. C caches it in a static that the
pub fn cron_job_relation_id() -> PgResult<Oid> {
    let nsp = catalog_namespace::get_namespace_oid(CRON_SCHEMA_NAME, false)?;
    lsyscache::get_relname_relid(JOBS_TABLE_NAME, nsp)
}

/// `JobTableExists`.
pub fn job_table_exists() -> PgResult<bool> {
    let nsp = catalog_namespace::get_namespace_oid(CRON_SCHEMA_NAME, false)?;
    Ok(lsyscache::get_relname_relid(JOBS_TABLE_NAME, nsp)? != InvalidOid)
}

/// `JobRunDetailsTableExists`.
pub fn job_run_details_table_exists() -> PgResult<bool> {
    let nsp = catalog_namespace::get_namespace_oid(CRON_SCHEMA_NAME, false)?;
    Ok(lsyscache::get_relname_relid(JOB_RUN_DETAILS_TABLE_NAME, nsp)? != InvalidOid)
}

/// `PgCronHasBeenLoaded`: the extension exists in this database and its
pub fn pg_cron_has_been_loaded() -> PgResult<bool> {
    let ext = extension::get_extension_oid(EXTENSION_NAME, true)?;
    if ext == InvalidOid {
        return Ok(false);
    }
    if pg_depend::creating_extension() && pg_depend::CurrentExtensionObject() == ext {
        return Ok(false);
    }
    if init_small::globals::IsBinaryUpgrade() {
        return Ok(false);
    }
    Ok(true)
}

/// `CronExtensionOwner`: pg_extension.extowner for pg_cron, via the
pub fn cron_extension_owner() -> PgResult<Oid> {
    let Some(tup) = cache_syscache::SearchSysCache1(
        cache_syscache::EXTENSIONNAME,
        cache_syscache::SysCacheKey::Str(EXTENSION_NAME),
    )?
    else {
        return Err(Box::new(
            PgError::error("pg_cron extension not loaded")
                .with_sqlstate(ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE),
        ));
    };
    let (d, isnull) =
        cache_syscache::SysCacheGetAttr(cache_syscache::EXTENSIONNAME, &tup, extension::Anum_pg_extension_extowner)?;
    debug_assert!(!isnull);
    let owner = d.as_oid();
    cache_syscache::ReleaseSysCache(tup);
    Ok(owner)
}

/// `GetRoleOidIfCanLogin`: the role must exist and have LOGIN.
pub fn role_oid_if_can_login(username: &str) -> PgResult<Oid> {
    match syscache_seams::lookup_authid_session_by_rolname::call(username)? {
        None => Err(err(format!("role \"{username}\" does not exist"))),
        Some(shape) if !shape.rolcanlogin => Err(Box::new(
            PgError::error(format!("role \"{username}\" can not log in"))
                .with_detail("Jobs may only be run by roles that have the LOGIN attribute."),
        )),
        Some(shape) => Ok(shape.roleid),
    }
}

/// `InvalidateJobCache`: a relcache inval on cron.job, which the launcher's
pub fn invalidate_job_cache() -> PgResult<()> {
    let relid = cron_job_relation_id()?;
    if let Some(tup) =
        cache_syscache::SearchSysCache1(cache_syscache::RELOID, cache_syscache::SysCacheKey::Value(Datum::from_oid(relid)))?
    {
        inval::invalidate::CacheInvalidateRelcacheByTuple(&tup.tuple())?;
        cache_syscache::ReleaseSysCache(tup);
    }
    Ok(())
}

/// Ensure `role` may CONNECT to `database` (both ScheduleCronJob and
fn ensure_connect_privilege(mcx: Mcx<'_>, role: Oid, database: &str) -> PgResult<()> {
    let dboid = dbcommands::get_database_oid(mcx, database, false)?;
    let acl = aclchk::object_aclcheck(
        types_core::DATABASE_RELATION_ID,
        dboid,
        role,
        types_nodes::parsenodes::ACL_CONNECT as u64,
    )?;
    if acl != aclchk::ACLCHECK_OK {
        return Err(err(format!(
            "User {} does not have CONNECT privilege on {database}",
            role_name(mcx, role)?
        )));
    }
    Ok(())
}

/// Run `sql` with `args` as the extension owner (SECURITY_LOCAL_USERID_CHANGE),
fn spi_as_extension_owner<R>(
    sql: &str,
    argtypes: &[Oid],
    values: &[Datum],
    expect: i32,
    f: impl FnOnce(u64) -> PgResult<R>,
) -> PgResult<R> {
    let saved = miscinit::GetUserIdAndSecContext();
    miscinit::SetUserIdAndSecContext(cron_extension_owner()?, SECURITY_LOCAL_USERID_CHANGE);
    let r = (|| -> PgResult<R> {
        if spi::SPI_connect()? != spi::SPI_OK_CONNECT {
            return Err(err("SPI_connect failed"));
        }
        let nulls = [false; 8];
        let rc = spi::SPI_execute_extended(sql, argtypes, values, &nulls[..values.len()], false, false, None)?;
        if rc != expect {
            spi::SPI_finish()?;
            return Err(err(format!("SPI_exec failed: {sql}")));
        }
        let out = f(spi::SPI_processed());
        spi::SPI_finish()?;
        out
    })();
    miscinit::SetUserIdAndSecContext(saved.0, saved.1);
    r
}

/// `ScheduleCronJob` (job_metadata.c:188): validate, permission-check, insert
fn schedule_cron_job(
    mcx: Mcx<'_>,
    schedule: &str,
    command: &str,
    database: Option<&str>,
    username: Option<&str>,
    active: bool,
    jobname: Option<&str>,
) -> PgResult<i64> {
    if parse_schedule(schedule).is_none() {
        return Err(invalid_schedule(schedule));
    }

    let mut sql = format!(
        "insert into {} (schedule, command, nodename, nodeport, database, username, active",
        ruleutils::quote_qualified_identifier(Some(CRON_SCHEMA_NAME), JOBS_TABLE_NAME)
    );
    if jobname.is_some() {
        sql.push_str(", jobname");
    }
    sql.push_str(") values ($1, $2, $3, $4, $5, $6, $7");
    if jobname.is_some() {
        sql.push_str(
            ", $8) on conflict on constraint jobname_username_uniq do update set \
             schedule = EXCLUDED.schedule, command = EXCLUDED.command, database = EXCLUDED.database",
        );
    } else {
        sql.push(')');
    }
    sql.push_str(" returning jobid");

    let user_id = miscinit::GetUserId();
    let mut acl_user = user_id;
    let mut job_user = current_user_name(mcx)?;
    if let Some(u) = username {
        if !superuser_seams::superuser::call()? {
            return Err(err("must be superuser to create a job for another role"));
        }
        job_user = u.to_owned();
        acl_user = role_oid_if_can_login(u)?;
    }
    let database_name = database.map(str::to_owned).unwrap_or_else(gucs::database_name);

    if !gucs::enable_superuser_jobs() && superuser_seams::superuser_arg::call(acl_user)? {
        return Err(superuser_jobs_disallowed());
    }
    ensure_connect_privilege(mcx, acl_user, &database_name)?;

    let mut argtypes = vec![TEXTOID, TEXTOID, TEXTOID, INT4OID, TEXTOID, TEXTOID, BOOLOID];
    let mut values = vec![
        text_datum(mcx, schedule)?,
        text_datum(mcx, command)?,
        text_datum(mcx, &gucs::host())?,
        Datum::from_i32(guc_tables::vars::PostPortNumber.read()),
        text_datum(mcx, &database_name)?,
        text_datum(mcx, &job_user)?,
        Datum::from_bool(active),
    ];
    if let Some(n) = jobname {
        argtypes.push(TEXTOID);
        values.push(text_datum(mcx, n)?);
    }

    let job_id = spi_as_extension_owner(&sql, &argtypes, &values, spi::SPI_OK_INSERT_RETURNING, |processed| {
        if processed == 0 {
            return Err(err(format!("query did not return any rows: {sql}")));
        }
        Ok(spi::SPI_tuptable()
            .map(|h| spi::tuptable_with(h, |d| spi::SPI_getbinval(&d.vals[0], &d.tupdesc, 1).0.as_i64()))
            .unwrap_or(0))
    })?;
    invalidate_job_cache()?;
    Ok(job_id)
}

/// `AlterJob` (job_metadata.c:1247).
fn alter_job(
    mcx: Mcx<'_>,
    job_id: i64,
    schedule: Option<&str>,
    command: Option<&str>,
    database: Option<&str>,
    username: Option<&str>,
    active: Option<bool>,
) -> PgResult<()> {
    if !pg_cron_has_been_loaded()? || transam_xlog::RecoveryInProgress() || !job_table_exists()? {
        return Ok(());
    }
    let is_superuser = superuser_seams::superuser::call()?;
    let current_user = current_user_name(mcx)?;
    let mut acl_user = miscinit::GetUserId();
    let job_user = match username {
        Some(u) => {
            if !is_superuser {
                return Err(err("must be superuser to alter username"));
            }
            acl_user = role_oid_if_can_login(u)?;
            u.to_owned()
        }
        None => current_user.clone(),
    };
    if !gucs::enable_superuser_jobs() && superuser_seams::superuser_arg::call(acl_user)? {
        return Err(superuser_jobs_disallowed());
    }

    let mut sql = format!("update {CRON_SCHEMA_NAME}.{JOBS_TABLE_NAME} set");
    let mut argtypes: Vec<Oid> = Vec::new();
    let mut values: Vec<Datum> = Vec::new();
    let mut push = |sql: &mut String, col: &str, ty: Oid, v: Datum| {
        argtypes.push(ty);
        values.push(v);
        sql.push_str(&format!(" {col} = ${},", argtypes.len()));
    };
    if let Some(db) = database {
        ensure_connect_privilege(mcx, acl_user, db)?;
        push(&mut sql, "database", TEXTOID, text_datum(mcx, db)?);
    }
    if let Some(s) = schedule {
        if parse_schedule(s).is_none() {
            return Err(invalid_schedule(s));
        }
        push(&mut sql, "schedule", TEXTOID, text_datum(mcx, s)?);
    }
    if let Some(c) = command {
        push(&mut sql, "command", TEXTOID, text_datum(mcx, c)?);
    }
    if username.is_some() {
        push(&mut sql, "username", TEXTOID, text_datum(mcx, &job_user)?);
    }
    if let Some(a) = active {
        push(&mut sql, "active", BOOLOID, Datum::from_bool(a));
    }
    sql.pop();
    argtypes.push(INT8OID);
    values.push(Datum::from_i64(job_id));
    sql.push_str(&format!(" where jobid = ${}", argtypes.len()));
    argtypes.push(TEXTOID);
    values.push(text_datum(mcx, &current_user)?);
    if !is_superuser {
        sql.push_str(&format!(" and username = ${}", argtypes.len()));
    }
    if argtypes.len() <= 2 {
        return Err(Box::new(PgError::error("no updates specified").with_hint(
            "You must specify at least one job attribute to change when calling alter_job",
        )));
    }

    let processed = spi_as_extension_owner(&sql, &argtypes, &values, spi::SPI_OK_UPDATE, Ok)?;
    if processed == 0 {
        return Err(err(format!("Job {job_id} does not exist or you don't own it")));
    }
    invalidate_job_cache()
}

/// `EnsureDeletePermission`: the row's username must be the caller, else the
fn ensure_delete_permission(mcx: Mcx<'_>, owner_name: &str) -> PgResult<()> {
    let owner = acl_seams::get_role_oid::call(owner_name, true)?;
    if owner == miscinit::GetUserId() {
        return Ok(());
    }
    let relid = cron_job_relation_id()?;
    let acl = aclchk::pg_class_aclcheck(relid, miscinit::GetUserId(), types_nodes::parsenodes::ACL_DELETE as u64)?;
    if acl != aclchk::ACLCHECK_OK {
        let name = lsyscache::get_rel_name(mcx, relid)?.map(|s| s.as_str().to_owned()).unwrap_or_default();
        aclchk::aclcheck_error(acl, types_nodes::parsenodes::ObjectType::OBJECT_TABLE, &name)?;
    }
    Ok(())
}

/// C's ScanKeyInit leaves sk_collation InvalidOid and texteq/nameeq tolerate
fn eq_key(attno: i32, proc_oid: types_core::RegProcedure, arg: Datum) -> PgResult<types_scan::scankey::ScanKeyData> {
    let mut k = types_scan::scankey::ScanKeyData::empty();
    k.sk_attno = attno as types_core::AttrNumber;
    k.sk_strategy = types_scan::scankey::BTEqualStrategyNumber;
    k.sk_collation = match proc_oid {
        types_core::fmgr::F_TEXTEQ => types_core::DEFAULT_COLLATION_OID,
        types_core::fmgr::F_NAMEEQ => types_core::C_COLLATION_OID,
        _ => 0,
    };
    k.sk_func = fmgr_seams::fmgr_info::call(proc_oid)?;
    k.sk_argument = arg;
    Ok(k)
}

/// Text column of a heap tuple as a String (TextDatumGetCString), detoasting
pub(crate) fn tuple_text(
    mcx: Mcx<'_>,
    tup: &types_tuple::HeapTupleData<'_>,
    desc: &types_tuple::TupleDescData<'_>,
    attno: i32,
) -> PgResult<Option<String>> {
    let mut isnull = false;
    // SAFETY: a tuple read under its own relation's descriptor.
    let d = unsafe { types_tuple::heap_getattr(tup, attno, desc, &mut isnull) };
    if isnull {
        return Ok(None);
    }
    // SAFETY: a non-null text datum from heap_getattr.
    let v = unsafe { types_fmgr::datum_varlena_packed(d, mcx)? };
    Ok(Some(String::from_utf8_lossy(v.data()).into_owned()))
}

/// Shared tail of both unschedule paths: scan with `keys` (indexed or not),
fn delete_one_job(
    mcx: Mcx<'_>,
    keys: &[types_scan::scankey::ScanKeyData],
    index_oid: Oid,
    not_found: impl FnOnce() -> Box<PgError>,
) -> PgResult<()> {
    let rel = table::table_open(mcx, cron_job_relation_id()?, RowExclusiveLock)?;
    let desc = rel.descr();
    let mut scan = genam::systable_beginscan(mcx, &rel, index_oid, index_oid != InvalidOid, None, keys)?;
    let Some(tup) = genam::systable_getnext(mcx, &mut scan)? else {
        genam::systable_endscan(mcx, scan)?;
        rel.close(NoLock)?;
        return Err(not_found());
    };
    let owner = tuple_text(mcx, tup, desc, Anum_cron_job_username)?.unwrap_or_default();
    ensure_delete_permission(mcx, &owner)?;
    let tid = tup.t_self;
    heapam::simple_heap_delete(&rel, &tid)?;
    genam::systable_endscan(mcx, scan)?;
    rel.close(NoLock)?;
    xact::CommandCounterIncrement()?;
    invalidate_job_cache()
}

/// `cron_schedule(schedule text, command text) returns bigint`.
fn fc_cron_schedule(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    if fcinfo.argisnull(0) {
        return Err(err("schedule can not be NULL"));
    }
    if fcinfo.argisnull(1) {
        return Err(err("command can not be NULL"));
    }
    let mcx = fcinfo.result_mcx();
    let schedule = arg_text(fcinfo, 0)?;
    let command = arg_text(fcinfo, 1)?;
    let id = schedule_cron_job(mcx, &schedule, &command, None, None, true, None)?;
    Ok(Datum::from_i64(id))
}

/// `cron_schedule_named(job_name, schedule, command [, database, username,
fn fc_cron_schedule_named(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    if fcinfo.argisnull(0) {
        return Err(err("job_name can not be NULL"));
    }
    let jobname = if funcapi::get_fn_expr_argtype(flinfo.as_deref(), 0) == NAMEOID {
        arg_name_str(fcinfo, 0)
    } else {
        arg_text(fcinfo, 0)?
    };
    if fcinfo.argisnull(1) {
        return Err(err("schedule can not be NULL"));
    }
    if fcinfo.argisnull(2) {
        return Err(err("command can not be NULL"));
    }
    let schedule = arg_text(fcinfo, 1)?;
    let command = arg_text(fcinfo, 2)?;
    let (mut database, mut username, mut active) = (None, None, true);
    if fcinfo.nargs() > 3 {
        if !fcinfo.argisnull(3) {
            database = Some(arg_text(fcinfo, 3)?);
        }
        if !fcinfo.argisnull(4) {
            username = Some(arg_text(fcinfo, 4)?);
        }
        if !fcinfo.argisnull(5) {
            active = fcinfo.arg_bool(5);
        }
    }
    let mcx = fcinfo.result_mcx();
    let id = schedule_cron_job(
        mcx,
        &schedule,
        &command,
        database.as_deref(),
        username.as_deref(),
        active,
        Some(&jobname),
    )?;
    Ok(Datum::from_i64(id))
}

/// `cron_unschedule(job_id bigint) returns bool`.
fn fc_cron_unschedule(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let job_id = fcinfo.arg_i64(0);
    let mcx = fcinfo.result_mcx();
    let nsp = catalog_namespace::get_namespace_oid(CRON_SCHEMA_NAME, false)?;
    let index = lsyscache::get_relname_relid(JOB_ID_INDEX_NAME, nsp)?;
    let key = eq_key(Anum_cron_job_jobid, F_INT8EQ, Datum::from_i64(job_id))?;
    delete_one_job(mcx, &[key], index, || err(format!("could not find valid entry for job {job_id}")))?;
    Ok(Datum::from_bool(true))
}

/// `cron_unschedule_named(job_name)` — `name` on the 1.4 script, `text`
fn fc_cron_unschedule_named(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    if fcinfo.argisnull(0) {
        return Err(err("job_name can not be NULL"));
    }
    let mcx = fcinfo.result_mcx();
    let (jobname, name_key) = if funcapi::get_fn_expr_argtype(flinfo.as_deref(), 0) == NAMEOID {
        let s = arg_name_str(fcinfo, 0);
        (s, eq_key(Anum_cron_job_jobname, types_core::fmgr::F_NAMEEQ, fcinfo.arg(0))?)
    } else {
        let s = arg_text(fcinfo, 0)?;
        (s, eq_key(Anum_cron_job_jobname, types_core::fmgr::F_TEXTEQ, fcinfo.arg(0))?)
    };
    let user_key = eq_key(
        Anum_cron_job_username,
        types_core::fmgr::F_TEXTEQ,
        text_datum(mcx, &current_user_name(mcx)?)?,
    )?;
    delete_one_job(mcx, &[name_key, user_key], InvalidOid, || {
        err(format!("could not find valid entry for job '{jobname}'"))
    })?;
    Ok(Datum::from_bool(true))
}

/// `cron_alter_job(job_id, schedule, command, database, username, active)`.
fn fc_cron_alter_job(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    if fcinfo.argisnull(0) {
        return Err(err("job_id can not be NULL"));
    }
    let job_id = fcinfo.arg_i64(0);
    let opt = |i: usize| -> PgResult<Option<String>> {
        if fcinfo.argisnull(i) { Ok(None) } else { arg_text(fcinfo, i).map(Some) }
    };
    let schedule = opt(1)?;
    let command = opt(2)?;
    let database = opt(3)?;
    let username = opt(4)?;
    let active = if fcinfo.argisnull(5) { None } else { Some(fcinfo.arg_bool(5)) };
    let mcx = fcinfo.result_mcx();
    alter_job(
        mcx,
        job_id,
        schedule.as_deref(),
        command.as_deref(),
        database.as_deref(),
        username.as_deref(),
        active,
    )?;
    Ok(Datum::null())
}

/// `cron_job_cache_invalidate()`: the AFTER trigger on cron.job.
fn fc_cron_job_cache_invalidate(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    // SAFETY: the trigger call machinery keeps the TriggerData live for the call.
    if unsafe { types_trigger_call::trigger_data_from_fcinfo(fcinfo) }.is_none() {
        return Err(Box::new(
            PgError::error("must be called as trigger")
                .with_sqlstate(ERRCODE_E_R_I_E_TRIGGER_PROTOCOL_VIOLATED),
        ));
    }
    invalidate_job_cache()?;
    Ok(Datum::null())
}

pub(crate) fn lookup(function: &str) -> Option<PGFunction> {
    Some(match function {
        "cron_schedule" => fc_cron_schedule,
        "cron_schedule_named" => fc_cron_schedule_named,
        "cron_unschedule" => fc_cron_unschedule,
        "cron_unschedule_named" => fc_cron_unschedule_named,
        "cron_alter_job" => fc_cron_alter_job,
        "cron_job_cache_invalidate" => fc_cron_job_cache_invalidate,
        _ => return None,
    })
}
