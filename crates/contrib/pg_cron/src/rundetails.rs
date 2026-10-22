//! cron.job_run_details writes from the launcher thread (C: job_metadata.c

use datum::Datum;
use mcx::Mcx;
use types_core::{Oid, SECURITY_LOCAL_USERID_CHANGE, INT4OID, INT8OID, TEXTOID, TIMESTAMPTZOID};
use types_error::{PgError, PgResult};

use crate::catalog::{self, text_datum, CRON_SCHEMA_NAME, JOB_RUN_DETAILS_TABLE_NAME};

pub type TimestampTz = i64;

/// `CronStatus` + `GetCronStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CronStatus {
    Starting,
    Running,
    Sending,
    Connecting,
    Succeeded,
    Failed,
}

impl CronStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            CronStatus::Starting => "starting",
            CronStatus::Running => "running",
            CronStatus::Sending => "sending",
            CronStatus::Connecting => "connecting",
            CronStatus::Succeeded => "succeeded",
            CronStatus::Failed => "failed",
        }
    }
}

#[cold]
#[inline(never)]
fn err(msg: impl Into<String>) -> Box<PgError> {
    Box::new(PgError::error(msg.into()))
}

/// Run `f` inside a fresh transaction with an active snapshot; commit on Ok.
fn in_own_transaction<R>(f: impl FnOnce(Mcx<'_>) -> PgResult<R>) -> PgResult<R> {
    let cx = mcx::MemoryContext::new("pg_cron run details");
    xact::SetCurrentStatementStartTimestamp();
    xact::StartTransactionCommand()?;
    let snap = snapmgr::GetTransactionSnapshot()?;
    snapmgr::PushActiveSnapshot(&snap)?;
    let r = f(cx.mcx())?;
    snapmgr::PopActiveSnapshot()?;
    xact::CommitTransactionCommand()?;
    Ok(r)
}

/// The C guard shared by every writer.
fn writable() -> PgResult<bool> {
    Ok(catalog::pg_cron_has_been_loaded()?
        && !transam_xlog::RecoveryInProgress()
        && catalog::job_run_details_table_exists()?)
}

fn spi_run(sql: &str, argtypes: &[Oid], values: &[Datum], expect: i32) -> PgResult<()> {
    if spi::SPI_connect()? != spi::SPI_OK_CONNECT {
        return Err(err("SPI_connect failed"));
    }
    let nulls = [false; 8];
    let rc = spi::SPI_execute_extended(sql, argtypes, values, &nulls[..values.len()], false, false, None)?;
    if rc != expect {
        return Err(err(format!("SPI_exec failed: {sql}")));
    }
    spi::SPI_finish()?;
    Ok(())
}

/// `NextRunId`: nextval('cron.runid_seq') as the extension owner; 0 when the
pub fn next_run_id() -> PgResult<i64> {
    in_own_transaction(|_mcx| {
        if !catalog::job_run_details_table_exists()? {
            return Ok(0);
        }
        let nsp = catalog_namespace::get_namespace_oid(CRON_SCHEMA_NAME, false)?;
        let seq = lsyscache::get_relname_relid("runid_seq", nsp)?;
        let saved = miscinit::GetUserIdAndSecContext();
        miscinit::SetUserIdAndSecContext(catalog::cron_extension_owner()?, SECURITY_LOCAL_USERID_CHANGE);
        let r = sequence_seams::nextval_internal::call(seq, true);
        miscinit::SetUserIdAndSecContext(saved.0, saved.1);
        r
    })
}

/// `InsertJobRunDetail`.
pub fn insert_job_run_detail(
    run_id: i64,
    job_id: i64,
    database: &str,
    username: &str,
    command: &str,
    status: CronStatus,
) -> PgResult<()> {
    in_own_transaction(|mcx| {
        if !writable()? {
            return Ok(());
        }
        let sql = format!(
            "insert into {CRON_SCHEMA_NAME}.{JOB_RUN_DETAILS_TABLE_NAME} \
             (jobid, runid, database, username, command, status) values ($1,$2,$3,$4,$5,$6)"
        );
        let argtypes = [INT8OID, INT8OID, TEXTOID, TEXTOID, TEXTOID, TEXTOID];
        let values = [
            Datum::from_i64(job_id),
            Datum::from_i64(run_id),
            text_datum(mcx, database)?,
            text_datum(mcx, username)?,
            text_datum(mcx, command)?,
            text_datum(mcx, status.as_str())?,
        ];
        spi_run(&sql, &argtypes, &values, spi::SPI_OK_INSERT)
    })
}

/// `UpdateJobRunDetail`: set only the provided columns on `runid = run_id`.
pub fn update_job_run_detail(
    run_id: i64,
    job_pid: Option<i32>,
    status: Option<CronStatus>,
    return_message: Option<&str>,
    start_time: Option<TimestampTz>,
    end_time: Option<TimestampTz>,
) -> PgResult<()> {
    in_own_transaction(|mcx| {
        if !writable()? {
            return Ok(());
        }
        let mut sql = format!("update {CRON_SCHEMA_NAME}.{JOB_RUN_DETAILS_TABLE_NAME} set");
        let mut argtypes: Vec<Oid> = Vec::new();
        let mut values: Vec<Datum> = Vec::new();
        let mut push = |sql: &mut String, col: &str, ty: Oid, v: Datum| {
            argtypes.push(ty);
            values.push(v);
            sql.push_str(&format!(" {col} = ${},", argtypes.len()));
        };
        if let Some(pid) = job_pid {
            push(&mut sql, "job_pid", INT4OID, Datum::from_i32(pid));
        }
        if let Some(s) = status {
            push(&mut sql, "status", TEXTOID, text_datum(mcx, s.as_str())?);
        }
        if let Some(m) = return_message {
            push(&mut sql, "return_message", TEXTOID, text_datum(mcx, m)?);
        }
        if let Some(t) = start_time {
            push(&mut sql, "start_time", TIMESTAMPTZOID, Datum::from_i64(t));
        }
        if let Some(t) = end_time {
            push(&mut sql, "end_time", TIMESTAMPTZOID, Datum::from_i64(t));
        }
        sql.pop();
        argtypes.push(INT8OID);
        values.push(Datum::from_i64(run_id));
        sql.push_str(&format!(" where runid = ${}", argtypes.len()));
        spi_run(&sql, &argtypes, &values, spi::SPI_OK_UPDATE)
    })
}

/// `MarkPendingRunsAsFailed`: runs left 'starting'/'running' by a previous
pub fn mark_pending_runs_as_failed() -> PgResult<()> {
    in_own_transaction(|_mcx| {
        if !writable()? {
            return Ok(());
        }
        let sql = format!(
            "update {CRON_SCHEMA_NAME}.{JOB_RUN_DETAILS_TABLE_NAME} set status = '{}', \
             return_message = 'server restarted' where status in ('{}','{}')",
            CronStatus::Failed.as_str(),
            CronStatus::Starting.as_str(),
            CronStatus::Running.as_str()
        );
        if spi::SPI_connect()? != spi::SPI_OK_CONNECT {
            return Err(err("SPI_connect failed"));
        }
        if spi::SPI_exec(&sql, 0)? != spi::SPI_OK_UPDATE {
            return Err(err(format!("SPI_exec failed: {sql}")));
        }
        spi::SPI_finish()?;
        Ok(())
    })
}
