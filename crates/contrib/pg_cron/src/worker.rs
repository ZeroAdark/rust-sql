//! The per-job background worker (C: pg_cron.c CronBackgroundWorker +

use std::sync::Arc;

use pgsync::Mutex;

use mcx::Mcx;
use types_error::{
    PgError, PgResult, ERRCODE_ADMIN_SHUTDOWN, ERRCODE_FEATURE_NOT_SUPPORTED,
    ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE, ERROR, LOG,
};
use types_nodes::NodeTag;
use types_portal::{CachedPlanHandle, ParamListHandle, QueryCompletion, QueryEnvHandle, FETCH_ALL};

use crate::task::{self, JobResult, JobSpec};

/// `bgw_generate_returned_message`: "SEVERITY: message" plus DETAIL / HINT /
pub fn returned_message(e: &PgError) -> String {
    let mut s = format!("{}: {}", elog::error_severity(e.level()), e.message());
    if let Some(d) = e.detail() {
        s.push_str(&format!("\nDETAIL: {d}"));
    }
    if let Some(h) = e.hint() {
        s.push_str(&format!("\nHINT: {h}"));
    }
    if let Some(c) = e.context() {
        s.push_str(&format!("\nCONTEXT: {c}"));
    }
    s
}

/// The completion tag as the wire CommandComplete carries it
fn completion_tag(qc: &QueryCompletion) -> String {
    let mut buf = [0u8; types_portal::COMPLETION_TAG_BUFSIZE];
    let n = cmdtag::BuildQueryCompletionString(&mut buf, qc, false);
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// `ExecuteSqlString` (pg_cron.c:2181): every statement of `sql` in the
fn execute_sql_string(mcx: Mcx<'_>, sql: &str) -> PgResult<Option<String>> {
    let parsetree_list = postgres::simple_query::pg_parse_query(mcx, sql)?;
    let commands_remaining = parsetree_list.len();
    let is_top_level = commands_remaining == 1;
    let mut last_tag = None;

    for parsetree in parsetree_list.iter() {
        let stmt = parsetree.stmt.expect("RawStmt has a stmt");
        if stmt.node_tag() == NodeTag::T_TransactionStmt {
            return Err(Box::new(
                PgError::error("transaction control statements are not allowed in pg_cron")
                    .with_sqlstate(ERRCODE_FEATURE_NOT_SUPPORTED),
            ));
        }
        let command_tag = utility_seams::create_command_tag::call(stmt);
        tcop_dest::BeginCommand(command_tag, types_dest::CommandDest::None);

        let mut snapshot_set = false;
        if analyze_seams::analyze_requires_snapshot::call(parsetree) {
            let snap = snapmgr::GetTransactionSnapshot()?;
            snapmgr::PushActiveSnapshot(&snap)?;
            snapshot_set = true;
        }
        let querytree_list = postgres::simple_query::pg_analyze_and_rewrite_fixedparams(
            mcx,
            parsetree,
            sql,
            &[],
            QueryEnvHandle::NULL,
        )?;
        let plantree_list =
            postgres::simple_query::pg_plan_queries(mcx, querytree_list, sql, 0, ParamListHandle::NULL)?;
        if snapshot_set {
            snapmgr::PopActiveSnapshot()?;
        }
        postgres_seams::check_for_interrupts::call()?;

        let portal = portalmem::CreatePortal("", true, true)?;
        portal.borrow_mut().visible = false;
        // SAFETY: `plantree_list` is arena-backed by `mcx` and outlives the
        let stmts = unsafe { pquery::stmt_list::register(&plantree_list) };
        portalmem::PortalDefineQuery(&portal, None, sql, command_tag, stmts, CachedPlanHandle::NULL)?;
        pquery::PortalStart(&portal, ParamListHandle::NULL, 0, None)?;
        pquery::PortalSetResultFormat(&portal, &[1])?;

        let mut receiver = tcop_dest::CreateDestReceiver(types_dest::CommandDest::None);
        let mut qc = QueryCompletion::default();
        let run = pquery::PortalRun(&portal, FETCH_ALL, is_top_level, &mut receiver, None, Some(&mut qc));
        receiver.destroy();
        run?;
        portalmem::PortalDrop(&portal, false)?;
        pquery::stmt_list::free(stmts);

        last_tag = Some(completion_tag(&qc));
    }
    xact::CommandCounterIncrement()?;
    Ok(last_tag)
}

fn run_job(spec: &JobSpec, result: &Arc<Mutex<JobResult>>) -> PgResult<()> {
    bgworker::BackgroundWorkerInitializeConnection(Some(&spec.database), Some(&spec.username), 0)?;
    result.lock().unwrap_or_else(|e| e.into_inner()).started = true;

    let _debug_query = elog::debug_query_string_scope(&spec.command);
    xact::SetCurrentStatementStartTimestamp();
    backend_status_seams::pgstat_report_activity::call(
        backend_status_seams::BackendState::STATE_RUNNING,
        Some(&spec.command),
    );

    let cx = mcx::MemoryContext::new("pg_cron parse/plan");
    xact::StartTransactionCommand()?;
    let statement_timeout = lmgr_proc::globals::StatementTimeout();
    if statement_timeout > 0 {
        timeout_seams::enable_timeout_after::call(timeout_seams::STATEMENT_TIMEOUT, statement_timeout)?;
    }
    let outcome = execute_sql_string(cx.mcx(), &spec.command);
    if timeout_seams::get_timeout_active::call(timeout_seams::STATEMENT_TIMEOUT) {
        timeout_seams::disable_timeout::call(timeout_seams::STATEMENT_TIMEOUT, false)?;
    }
    let tag = outcome?;
    xact::CommitTransactionCommand()?;

    backend_status_seams::pgstat_report_activity::call(backend_status_seams::BackendState::STATE_IDLE, None);
    result.lock().unwrap_or_else(|e| e.into_inner()).command_tag = tag;
    Ok(())
}

/// `CronBackgroundWorker` (pg_cron.c:2094): bgw_main for job workers.
pub fn cron_background_worker_main(main_arg: u64) -> PgResult<()> {
    bgworker::BackgroundWorkerUnblockSignals();

    let Some((spec, result)) = task::lookup_job(main_arg) else {
        return Err(Box::new(
            PgError::error("unable to map dynamic shared memory segment")
                .with_sqlstate(ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE),
        ));
    };

    let r = run_job(&spec, &result);
    if let Err(e) = &r {
        if e.level() > ERROR {
            if e.sqlstate() != ERRCODE_ADMIN_SHUTDOWN {
                result.lock().unwrap_or_else(|e| e.into_inner()).error = Some(returned_message(e));
            }
            result.lock().unwrap_or_else(|e| e.into_inner()).finished = true;
            return r;
        }
        elog::emit_error_report_for(e);
        if xact::IsTransactionState() {
            let _ = xact::AbortCurrentTransaction();
        }
        let _ = elog::elog(LOG, format!("cron job {} failed: {}", spec.job_id, e.message()));
        result.lock().unwrap_or_else(|e| e.into_inner()).error = Some(returned_message(e));
    }
    result.lock().unwrap_or_else(|e| e.into_inner()).finished = true;
    Ok(())
}
