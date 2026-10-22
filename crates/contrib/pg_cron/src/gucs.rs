//! pg_cron GUC backing storage (C: the `_PG_init` globals in pg_cron.c —

use guc_tables::GucVarAccessors;

guc_tables::session_guc_cluster!(CronGucs, CRON_GUCS:
    (log_statement_cell, bool, log_statement, set_log_statement, true),
    (log_run_cell, bool, log_run, set_log_run, true),
    (enable_superuser_jobs_cell, bool, enable_superuser_jobs, set_enable_superuser_jobs, true),
    (use_background_workers_cell, bool, use_background_workers, set_use_background_workers, true),
    (launch_active_jobs_cell, bool, launch_active_jobs, set_launch_active_jobs, true),
    (match_dom_and_dow_cell, bool, match_dom_and_dow, set_match_dom_and_dow, false),
    (max_running_jobs_cell, i32, max_running_jobs, set_max_running_jobs, 5),
    (log_min_messages_cell, i32, log_min_messages, set_log_min_messages, types_error::WARNING.0),
);

guc_tables::session_guc_string!(DATABASE_NAME, database_name_raw, set_database_name, Some("postgres"));
guc_tables::session_guc_string!(HOST, host_raw, set_host, Some("localhost"));
guc_tables::session_guc_string!(TIMEZONE, timezone_raw, set_timezone, Some("GMT"));

/// `CronTableDatabaseName`: the database holding cron.job (never NULL — the
pub fn database_name() -> String {
    database_name_raw().unwrap_or_else(|| "postgres".to_string())
}

/// `CronHost`: stored in cron.job.nodename for C compatibility; never used
pub fn host() -> String {
    host_raw().unwrap_or_else(|| "localhost".to_string())
}

/// `cron_timezone`: the zone schedules are evaluated in (boot "GMT").
pub fn timezone() -> String {
    timezone_raw().unwrap_or_else(|| "GMT".to_string())
}

pub fn install() {
    use guc_tables::vars as v;
    v::cron_log_statement.install(GucVarAccessors { get: log_statement, set: set_log_statement });
    v::cron_log_run.install(GucVarAccessors { get: log_run, set: set_log_run });
    v::cron_enable_superuser_jobs
        .install(GucVarAccessors { get: enable_superuser_jobs, set: set_enable_superuser_jobs });
    v::cron_use_background_workers
        .install(GucVarAccessors { get: use_background_workers, set: set_use_background_workers });
    v::cron_launch_active_jobs
        .install(GucVarAccessors { get: launch_active_jobs, set: set_launch_active_jobs });
    v::cron_match_dom_and_dow
        .install(GucVarAccessors { get: match_dom_and_dow, set: set_match_dom_and_dow });
    v::cron_max_running_jobs
        .install(GucVarAccessors { get: max_running_jobs, set: set_max_running_jobs });
    v::cron_log_min_messages
        .install(GucVarAccessors { get: log_min_messages, set: set_log_min_messages });
    v::cron_database_name.install(GucVarAccessors { get: database_name_raw, set: set_database_name });
    v::cron_host.install(GucVarAccessors { get: host_raw, set: set_host });
    v::cron_timezone.install(GucVarAccessors { get: timezone_raw, set: set_timezone });
    guc_tables::hooks::check_cron_timezone.install(check_cron_timezone);
}

/// `check_timezone` (pg_cron.c): the name must resolve through pg_tzset.
fn check_cron_timezone(
    newval: &mut Option<String>,
    _extra: &mut Option<guc_tables::GucHookExtra>,
    _source: types_guc::GucSource,
) -> types_error::PgResult<bool> {
    let Some(name) = newval.as_deref() else { return Ok(false) };
    match pgtz::pg_tzset(name.as_bytes())? {
        Some(_) => Ok(true),
        None => {
            guc::GUC_check_errmsg(format!("invalid cron.timezone value \"{name}\""));
            Ok(false)
        }
    }
}
