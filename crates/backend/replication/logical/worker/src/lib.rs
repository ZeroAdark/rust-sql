#![allow(non_snake_case)]

use std::cell::{Cell, RefCell};

use elog::ereport;
use logicalproto::LogicalRepRelation;
use mcx::{Mcx, MemoryContext};
use types_core::{InvalidTransactionId, InvalidXLogRecPtr, Oid, TimestampTz, TransactionId, XLogRecPtr};
use types_error::{
    ErrorLocation, PgError, PgResult, DEBUG1, DEBUG2, ERRCODE_ADMIN_SHUTDOWN, ERRCODE_CONNECTION_FAILURE,
    ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE, ERRCODE_PROTOCOL_VIOLATION, ERROR, FATAL, LOG, WARNING,
};

use types_storage::waiteventset::{
    WL_EXIT_ON_PM_DEATH, WL_LATCH_SET, WL_SOCKET_READABLE, WL_TIMEOUT,
};
use walreceiver::client::{CopyData, PgConn};

mod apply;
mod conflict;
mod parallel;
mod stream_apply;
mod tablesync;
#[cfg(test)]
mod tests;

pub(crate) fn request_apply_worker_exit() {
    APPLY_WORKER_EXIT.set(true);
}

pub(crate) fn apply_worker_exit_requested() -> bool {
    APPLY_WORKER_EXIT.get()
}

#[track_caller]
pub(crate) fn loc(func: &'static str) -> ErrorLocation {
    // #[track_caller] resolves to the call site, not this helper.
    let site = core::panic::Location::caller();
    ErrorLocation::new(site.file(), site.line() as i32, func)
}

pub(crate) fn get_ts() -> TimestampTz {
    timestamp_seams::get_current_timestamp::call()
}

#[derive(Clone)]
pub(crate) struct MySub {
    pub oid: Oid,
    pub name: String,
    pub conninfo: String,
    pub slotname: Option<String>,
    pub publications: Vec<String>,
    pub binary: bool,
    pub stream: u8,
    #[allow(dead_code)]
    pub twophasestate: u8,
    pub enabled: bool,
    pub origin: String,
    pub synccommit: String,
    pub skiplsn: XLogRecPtr,
    pub owner: Oid,
    pub ownersuperuser: bool,
    pub passwordrequired: bool,
    pub runasowner: bool,
    pub failover: bool,
    // disable_on_error disables the subscription instead of crash-looping the worker.
    pub disableonerr: bool,
    #[allow(dead_code)]
    pub dbid: Oid,
}

thread_local! {
    pub(crate) static MY_SUBSCRIPTION: RefCell<Option<MySub>> = const { RefCell::new(None) };
    pub(crate) static SUBSCRIPTION_CHANGED: Cell<bool> = const { Cell::new(false) };
    pub(crate) static IN_REMOTE_TRANSACTION: Cell<bool> = const { Cell::new(false) };
    pub(crate) static REMOTE_FINAL_LSN: Cell<XLogRecPtr> = const { Cell::new(InvalidXLogRecPtr) };
    static LAST_RECVPOS: Cell<XLogRecPtr> = const { Cell::new(InvalidXLogRecPtr) };
    static LAST_WRITEPOS: Cell<XLogRecPtr> = const { Cell::new(InvalidXLogRecPtr) };
    static LAST_FLUSHPOS: Cell<XLogRecPtr> = const { Cell::new(InvalidXLogRecPtr) };
    static FEEDBACK_SEND_TIME: Cell<TimestampTz> = const { Cell::new(0) };
    static LSN_MAPPING: RefCell<Vec<(XLogRecPtr, XLogRecPtr)>> = const { RefCell::new(Vec::new()) };
    pub(crate) static APPLY_WORKER_EXIT: Cell<bool> = const { Cell::new(false) };
    // Valid while the whole remote transaction whose finish LSN it holds is being skipped.
    static SKIP_XACT_FINISH_LSN: Cell<XLogRecPtr> = const { Cell::new(InvalidXLogRecPtr) };
    static APPLY_ERROR_CALLBACK_ARG: RefCell<ApplyErrorCallbackArg> =
        const { RefCell::new(ApplyErrorCallbackArg::INITIAL) };
}

pub(crate) struct ApplyErrorCallbackArg {
    pub command: u8,
    pub rel: Option<LogicalRepRelation>,
    pub remote_attnum: i32,
    pub remote_xid: TransactionId,
    pub finish_lsn: XLogRecPtr,
    pub origin_name: Option<String>,
}

impl ApplyErrorCallbackArg {
    pub const INITIAL: ApplyErrorCallbackArg = ApplyErrorCallbackArg {
        command: 0,
        rel: None,
        remote_attnum: -1,
        remote_xid: InvalidTransactionId,
        finish_lsn: InvalidXLogRecPtr,
        origin_name: None,
    };
}

pub(crate) fn apply_error_context_line(errarg: &ApplyErrorCallbackArg) -> Option<String> {
    if errarg.command == 0 {
        return None;
    }
    let origin = errarg.origin_name.as_deref().unwrap_or("(null)");
    let msgtype = logicalproto::logicalrep_message_type(errarg.command);
    let xid = errarg.remote_xid;
    let lsn = format!("{:X}/{:X}", (errarg.finish_lsn >> 32) as u32, errarg.finish_lsn as u32);
    let lsn_valid = errarg.finish_lsn != InvalidXLogRecPtr;
    Some(match &errarg.rel {
        None => {
            if xid == InvalidTransactionId {
                format!(
                    "processing remote data for replication origin \"{origin}\" during message type \"{msgtype}\""
                )
            } else if !lsn_valid {
                format!(
                    "processing remote data for replication origin \"{origin}\" during message type \"{msgtype}\" in transaction {xid}"
                )
            } else {
                format!(
                    "processing remote data for replication origin \"{origin}\" during message type \"{msgtype}\" in transaction {xid}, finished at {lsn}"
                )
            }
        }
        Some(rel) => {
            let (nsp, name) = (&rel.nspname, &rel.relname);
            if errarg.remote_attnum < 0 {
                if !lsn_valid {
                    format!(
                        "processing remote data for replication origin \"{origin}\" during message type \"{msgtype}\" for replication target relation \"{nsp}.{name}\" in transaction {xid}"
                    )
                } else {
                    format!(
                        "processing remote data for replication origin \"{origin}\" during message type \"{msgtype}\" for replication target relation \"{nsp}.{name}\" in transaction {xid}, finished at {lsn}"
                    )
                }
            } else {
                // The sites that set remote_attnum bounds-checked it against
                // the received tuple, so a miss here is impossible.
                let col = rel
                    .attnames
                    .get(errarg.remote_attnum as usize)
                    .map(String::as_str)
                    .unwrap_or("");
                if !lsn_valid {
                    format!(
                        "processing remote data for replication origin \"{origin}\" during message type \"{msgtype}\" for replication target relation \"{nsp}.{name}\" column \"{col}\" in transaction {xid}"
                    )
                } else {
                    format!(
                        "processing remote data for replication origin \"{origin}\" during message type \"{msgtype}\" for replication target relation \"{nsp}.{name}\" column \"{col}\" in transaction {xid}, finished at {lsn}"
                    )
                }
            }
        }
    })
}

pub(crate) fn apply_error_callback(e: &mut PgError) {
    let line = APPLY_ERROR_CALLBACK_ARG.with(|a| apply_error_context_line(&a.borrow()));
    if let Some(line) = line {
        e.add_context_line(line);
    }
}

pub(crate) struct ApplyErrorContextFrame {
    callback: u64,
}

impl ApplyErrorContextFrame {
    pub(crate) fn push() -> Self {
        // A walked frame as well as an emit decorator: C's callback sits on
        // error_context_stack, so GetErrorContextStack (a PL/pgSQL EXCEPTION
        // block's PG_EXCEPTION_CONTEXT inside a replica trigger) sees the
        // "processing remote data for replication origin ..." line at the
        // catch, not only when the error leaves the apply loop.
        ApplyErrorContextFrame {
            callback: elog::push_context_frame(Box::new(apply_error_callback), true),
        }
    }

    pub(crate) fn attach<T>(&self, r: PgResult<T>) -> PgResult<T> {
        r.map_err(|mut e| {
            apply_error_callback(&mut e);
            e
        })
    }
}

impl Drop for ApplyErrorContextFrame {
    fn drop(&mut self) {
        elog::pop_emit_context_callback(self.callback);
    }
}

pub(crate) fn set_apply_error_context_xact(xid: TransactionId, lsn: XLogRecPtr) {
    APPLY_ERROR_CALLBACK_ARG.with(|a| {
        let mut a = a.borrow_mut();
        a.remote_xid = xid;
        a.finish_lsn = lsn;
    });
}

pub(crate) fn reset_apply_error_context_info() {
    APPLY_ERROR_CALLBACK_ARG.with(|a| {
        let mut a = a.borrow_mut();
        a.command = 0;
        a.rel = None;
        a.remote_attnum = -1;
        a.remote_xid = InvalidTransactionId;
        a.finish_lsn = InvalidXLogRecPtr;
    });
}

pub(crate) fn set_apply_error_context_origin(originname: &str) {
    APPLY_ERROR_CALLBACK_ARG.with(|a| a.borrow_mut().origin_name = Some(originname.to_string()));
}

pub(crate) fn set_apply_error_context_rel(rel: Option<&LogicalRepRelation>) {
    APPLY_ERROR_CALLBACK_ARG.with(|a| a.borrow_mut().rel = rel.cloned());
}

pub(crate) fn set_apply_error_context_attnum(remote_attnum: i32) {
    APPLY_ERROR_CALLBACK_ARG.with(|a| a.borrow_mut().remote_attnum = remote_attnum);
}

pub(crate) fn set_apply_error_context_command(command: u8) -> u8 {
    APPLY_ERROR_CALLBACK_ARG.with(|a| {
        let mut a = a.borrow_mut();
        let saved = a.command;
        a.command = command;
        saved
    })
}

pub(crate) fn is_skipping_changes() -> bool {
    SKIP_XACT_FINISH_LSN.get() != InvalidXLogRecPtr
}

pub(crate) fn maybe_start_skipping_changes(finish_lsn: XLogRecPtr) {
    debug_assert!(!is_skipping_changes());
    debug_assert!(!IN_REMOTE_TRANSACTION.get());
    debug_assert!(!stream_apply::in_streamed_transaction());

    let skiplsn = my_sub(|s| s.skiplsn);
    if skiplsn == InvalidXLogRecPtr || skiplsn != finish_lsn {
        return;
    }

    SKIP_XACT_FINISH_LSN.set(finish_lsn);
    let _ = elog::elog(
        LOG,
        format!(
            "logical replication starts skipping transaction at LSN {:X}/{:X}",
            (finish_lsn >> 32) as u32,
            finish_lsn as u32
        ),
    );
}

pub(crate) fn stop_skipping_changes() {
    if !is_skipping_changes() {
        return;
    }
    let lsn = SKIP_XACT_FINISH_LSN.get();
    let _ = elog::elog(
        LOG,
        format!(
            "logical replication completed skipping transaction at LSN {:X}/{:X}",
            (lsn >> 32) as u32,
            lsn as u32
        ),
    );
    SKIP_XACT_FINISH_LSN.set(InvalidXLogRecPtr);
}

// When finish_lsn doesn't match the cleared skiplsn, warn (e.g. the user
// specified a wrong subskiplsn).
pub(crate) fn clear_subscription_skip_lsn(mcx: Mcx<'_>, finish_lsn: XLogRecPtr) -> PgResult<()> {
    let myskiplsn = my_sub(|s| s.skiplsn);
    if myskiplsn == InvalidXLogRecPtr || parallel::am_parallel_apply_worker() {
        return Ok(());
    }

    let started_tx = if !xact::IsTransactionState() {
        xact::StartTransactionCommand()?;
        true
    } else {
        false
    };

    // Updating pg_subscription might involve TOAST table access, so ensure
    // a valid snapshot.
    snapmgr::PushActiveSnapshot(&snapmgr::GetTransactionSnapshot()?)?;

    let (suboid, subname) = my_sub(|s| (s.oid, s.name.clone()));
    let cleared = pg_subscription::ClearSubscriptionSkipLsn(mcx, suboid, &subname, myskiplsn)?;
    if cleared && myskiplsn != finish_lsn {
        let _ = ereport(WARNING)
            .errmsg(format!("skip-LSN of subscription \"{subname}\" cleared"))
            .errdetail(format!(
                "Remote transaction's finish WAL location (LSN) {:X}/{:X} did not match skip-LSN {:X}/{:X}.",
                (finish_lsn >> 32) as u32,
                finish_lsn as u32,
                (myskiplsn >> 32) as u32,
                myskiplsn as u32,
            ))
            .finish(loc("clear_subscription_skip_lsn"));
    }

    snapmgr::PopActiveSnapshot()?;

    if started_tx {
        xact::CommitTransactionCommand()?;
    }
    Ok(())
}

pub(crate) fn my_sub<R>(f: impl FnOnce(&MySub) -> R) -> R {
    MY_SUBSCRIPTION.with(|s| f(s.borrow().as_ref().expect("MySubscription loaded")))
}

fn subscription_change_cb(_arg: datum::Datum, _cacheid: i32, _hash: u32) {
    SUBSCRIPTION_CHANGED.set(true);
}

fn load_subscription(mcx: Mcx<'_>, subid: Oid) -> PgResult<Option<MySub>> {
    let Some(sub) = pg_subscription::GetSubscription(mcx, subid, true)? else {
        return Ok(None);
    };
    Ok(Some(MySub {
        oid: sub.oid,
        name: sub.name.as_str().to_string(),
        conninfo: sub.conninfo.as_str().to_string(),
        slotname: sub.slotname.as_ref().map(|s| s.as_str().to_string()),
        publications: sub.publications.iter().map(|p| p.to_string()).collect(),
        binary: sub.binary,
        stream: sub.stream,
        twophasestate: sub.twophasestate,
        enabled: sub.enabled,
        origin: sub.origin.as_str().to_string(),
        synccommit: sub.synccommit.as_str().to_string(),
        skiplsn: sub.skiplsn,
        owner: sub.owner,
        ownersuperuser: sub.ownersuperuser,
        passwordrequired: sub.passwordrequired,
        runasowner: sub.runasowner,
        failover: sub.failover,
        disableonerr: sub.disableonerr,
        dbid: sub.dbid,
    }))
}

pub(crate) fn maybe_reread_subscription(mcx: Mcx<'_>) -> PgResult<()> {
    if !SUBSCRIPTION_CHANGED.get() {
        return Ok(());
    }
    SUBSCRIPTION_CHANGED.set(false);

    // Catalog reads need a transaction, but the apply loop's idle arm
    // calls this with none.
    let started_tx = if !xact::IsTransactionState() {
        xact::StartTransactionCommand()?;
        true
    } else {
        false
    };
    let result = maybe_reread_subscription_guts(mcx);
    if started_tx {
        xact::CommitTransactionCommand()?;
    }
    result
}

// Reset the launcher's last-start time so the replacement is not
// throttled by wal_retrieve_retry_interval.
fn apply_worker_exit(subid: Oid) {
    let is_leader = launcher::my_worker_slot()
        .and_then(launcher::worker_snapshot)
        .is_some_and(|w| !w.is_tablesync() && !w.is_parallel_apply());
    if is_leader {
        launcher::ApplyLauncherForgetWorkerStartTime(subid);
    }
    APPLY_WORKER_EXIT.set(true);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RereadChange {
    Removed,
    Disabled,
    ParamChange,
}

pub(crate) fn reread_change_message(change: RereadChange, name: &str, parallel: bool) -> String {
    match change {
        RereadChange::Removed => format!(
            "logical replication worker for subscription \"{name}\" will stop because the subscription was removed"
        ),
        RereadChange::Disabled => format!(
            "logical replication worker for subscription \"{name}\" will stop because the subscription was disabled"
        ),
        RereadChange::ParamChange if parallel => format!(
            "logical replication parallel apply worker for subscription \"{name}\" will stop because of a parameter change"
        ),
        RereadChange::ParamChange => format!(
            "logical replication worker for subscription \"{name}\" will restart because of a parameter change"
        ),
    }
}

fn feedback_debug_line(force: bool, recvpos: u64, writepos: u64, flushpos: u64) -> String {
    let lsn = |v: u64| format!("{:X}/{:X}", (v >> 32) as u32, v as u32);
    format!(
        "sending feedback (force {}) to recv {}, write {}, flush {}",
        force as u8,
        lsn(recvpos),
        lsn(writepos),
        lsn(flushpos)
    )
}

fn maybe_reread_subscription_guts(mcx: Mcx<'_>) -> PgResult<()> {
    let subid = my_sub(|s| s.oid);
    let newsub = load_subscription(mcx, subid)?;

    let change = match &newsub {
        None => Some(RereadChange::Removed),
        Some(n) if !n.enabled => Some(RereadChange::Disabled),
        Some(n) => {
            let differs = my_sub(|old| {
                n.conninfo != old.conninfo
                    || n.name != old.name
                    || n.slotname != old.slotname
                    || n.binary != old.binary
                    || n.stream != old.stream
                    || n.publications != old.publications
                    || n.origin != old.origin
                    || n.owner != old.owner
                    // password_required governs publisher authentication: a
                    // change must reconnect and re-enforce.
                    || n.passwordrequired != old.passwordrequired
            });
            differs.then_some(RereadChange::ParamChange)
        }
    };

    if let Some(change) = change {
        let name = my_sub(|s| s.name.clone());
        let parallel = parallel::am_parallel_apply_worker();
        let _ = elog::elog(LOG, reread_change_message(change, &name, parallel));
        // A removed subscription exits every worker; parallel workers return
        // so the leader detects the change and restarts.
        if !parallel || newsub.is_none() {
            apply_worker_exit(subid);
            return Ok(());
        }
    }

    // Exit if the subscription owner's superuser privileges have been revoked
    // so it re-checks password_required and re-authenticates.
    if let Some(n) = &newsub {
        if owner_superuser_revoked(my_sub(|o| o.ownersuperuser), n.ownersuperuser) {
            let name = my_sub(|s| s.name.clone());
            if parallel::am_parallel_apply_worker() {
                let _ = elog::elog(
                    LOG,
                    format!(
                        "logical replication parallel apply worker for subscription \"{name}\" will stop because the subscription owner's superuser privileges have been revoked"
                    ),
                );
            } else {
                let _ = elog::elog(
                    LOG,
                    format!(
                        "logical replication worker for subscription \"{name}\" will restart because the subscription owner's superuser privileges have been revoked"
                    ),
                );
                apply_worker_exit(subid);
            }
            return Ok(());
        }
    }

    if let Some(n) = newsub {
        let synccommit = n.synccommit.clone();
        MY_SUBSCRIPTION.with(|s| *s.borrow_mut() = Some(n));
        set_synchronous_commit(&synccommit)?;
    }
    Ok(())
}

// (forget the launcher start time, exit now) for a worker whose subscription
// was disabled during startup (worker.c:4730 -> apply_worker_exit).
pub(crate) fn startup_disabled_action(is_tablesync: bool, is_parallel_apply: bool) -> (bool, bool) {
    if is_parallel_apply {
        return (false, false);
    }
    (!is_tablesync, true)
}

pub(crate) fn owner_superuser_revoked(old_superuser: bool, new_superuser: bool) -> bool {
    !new_superuser && old_superuser
}

fn set_synchronous_commit(synccommit: &str) -> PgResult<()> {
    guc::SetConfigOption(
        "synchronous_commit",
        Some(synccommit),
        types_guc::GucContext::PGC_BACKEND,
        types_guc::GucSource::PGC_S_OVERRIDE,
    )
}

// Parallel apply workers skip storing flush position — the leader maintains
// the mapping.
pub(crate) fn store_flush_position(remote_lsn: XLogRecPtr, local_lsn: XLogRecPtr) {
    if parallel::am_parallel_apply_worker() {
        return;
    }
    LSN_MAPPING.with(|m| m.borrow_mut().push((local_lsn, remote_lsn)));
}

fn get_flush_position() -> (XLogRecPtr, XLogRecPtr, bool) {
    get_flush_position_at(transam_xlog::GetFlushRecPtr(None))
}

// The write position is the latest locally committed remote end, not just
// the flushed one.
fn get_flush_position_at(local_flush: XLogRecPtr) -> (XLogRecPtr, XLogRecPtr, bool) {
    let mut write = InvalidXLogRecPtr;
    let mut flush = InvalidXLogRecPtr;
    let have_pending = LSN_MAPPING.with(|m| {
        let mut m = m.borrow_mut();
        let mut i = 0;
        while i < m.len() {
            let (local, remote) = m[i];
            write = remote;
            if local <= local_flush {
                flush = remote;
                i += 1;
            } else {
                write = m[m.len() - 1].1;
                break;
            }
        }
        m.drain(..i);
        !m.is_empty()
    });
    (write, flush, have_pending)
}

pub(crate) fn parse_wal_data_message(
    body: &[u8],
) -> PgResult<(XLogRecPtr, XLogRecPtr, TimestampTz, &[u8])> {
    let mut r = logicalproto::Reader::new(body);
    let start_lsn = r.get_int64()?;
    let end_lsn = r.get_int64()?;
    let send_time = r.get_int64()? as i64;
    Ok((start_lsn, end_lsn, send_time, &body[24..]))
}

pub(crate) fn parse_keepalive_message(buf: &[u8]) -> PgResult<(XLogRecPtr, TimestampTz, bool)> {
    let mut r = logicalproto::Reader::new(buf);
    let end_lsn = r.get_int64()?;
    let timestamp = r.get_int64()? as i64;
    let reply_requested = r.get_byte()? != 0;
    Ok((end_lsn, timestamp, reply_requested))
}

pub(crate) fn send_feedback(
    conn: &mut PgConn,
    recvpos_in: XLogRecPtr,
    force: bool,
    request_reply: bool,
) -> PgResult<()> {
    let interval = guc_tables::vars::wal_receiver_status_interval.read();
    if !force && interval <= 0 {
        return Ok(());
    }

    let mut recvpos = recvpos_in;
    if recvpos < LAST_RECVPOS.get() {
        recvpos = LAST_RECVPOS.get();
    }

    let (mut writepos, mut flushpos, have_pending_txes) = get_flush_position();

    // When nothing is outstanding, reporting the latest received position
    // matters for synchronous replication.
    if !have_pending_txes {
        writepos = recvpos;
        flushpos = recvpos;
    }
    if writepos < LAST_WRITEPOS.get() {
        writepos = LAST_WRITEPOS.get();
    }
    if flushpos < LAST_FLUSHPOS.get() {
        flushpos = LAST_FLUSHPOS.get();
    }

    let now = get_ts();
    if !force
        && writepos == LAST_WRITEPOS.get()
        && flushpos == LAST_FLUSHPOS.get()
        && !adt_timestamp::TimestampDifferenceExceeds(FEEDBACK_SEND_TIME.get(), now, interval * 1000)
    {
        return Ok(());
    }
    FEEDBACK_SEND_TIME.set(now);

    let mut msg = Vec::with_capacity(1 + 8 * 4 + 1);
    msg.push(b'r');
    msg.extend_from_slice(&recvpos.to_be_bytes()); // write
    msg.extend_from_slice(&flushpos.to_be_bytes()); // flush
    msg.extend_from_slice(&writepos.to_be_bytes()); // apply
    msg.extend_from_slice(&(now as i64).to_be_bytes());
    msg.push(request_reply as u8);

    let _ = elog::elog(DEBUG2, feedback_debug_line(force, recvpos, writepos, flushpos));

    if let Err(e) = conn.put_copy_data(&msg) {
        return Err(wal_stream_send_error(&e));
    }

    if recvpos > LAST_RECVPOS.get() {
        LAST_RECVPOS.set(recvpos);
    }
    if writepos > LAST_WRITEPOS.get() {
        LAST_WRITEPOS.set(writepos);
    }
    if flushpos > LAST_FLUSHPOS.get() {
        LAST_FLUSHPOS.set(flushpos);
    }
    Ok(())
}

// The idle wait must time out on this cadence so a worker against a quiet
// publisher notices subscription DDL.
const NAPTIME_PER_CYCLE: i64 = 1000;
const WAIT_EVENT_LOGICAL_APPLY_MAIN: u32 = 0x0500_0000 | 7;

fn wait_for_apply(socket: types_core::pgsocket, timeout: i64) -> PgResult<u32> {
    let rc = latch::WaitLatchOrSocket(
        init_small::globals::MyLatch(),
        WL_SOCKET_READABLE | WL_LATCH_SET | WL_TIMEOUT | WL_EXIT_ON_PM_DEATH,
        socket,
        timeout,
        WAIT_EVENT_LOGICAL_APPLY_MAIN,
    )?;
    if rc & WL_LATCH_SET != 0 {
        if let Some(latch) = init_small::globals::MyLatch() {
            latch::ResetLatch(latch);
        }
        postgres_seams::check_for_interrupts::call()?;
    }
    Ok(rc)
}

pub(crate) fn apply_loop(conn: &mut PgConn, last_received: XLogRecPtr) -> PgResult<()> {
    let frame = ApplyErrorContextFrame::push();
    let end_of_stream = frame.attach(apply_loop_guts(conn, last_received))?;
    drop(frame);
    if end_of_stream {
        walreceiver::client::end_streaming(conn)?;
    }
    Ok(())
}

pub(crate) fn wal_stream_receive_error(err: &str, sqlstate: types_error::SqlState) -> Box<PgError> {
    Box::new(
        PgError::error(format!("could not receive data from WAL stream: {}", pchomp(err)))
            .with_sqlstate(sqlstate),
    )
}

pub(crate) fn wal_stream_send_error(err: &str) -> Box<PgError> {
    Box::new(
        PgError::error(format!("could not send data to WAL stream: {}", pchomp(err)))
            .with_sqlstate(ERRCODE_CONNECTION_FAILURE),
    )
}

fn pchomp(s: &str) -> &str {
    s.trim_end_matches('\n')
}

fn apply_loop_guts(conn: &mut PgConn, mut last_received: XLogRecPtr) -> PgResult<bool> {
    let mut top = MemoryContext::new_wholesale("ApplyMessageContext");

    let mut last_recv_timestamp: TimestampTz = get_ts();
    let mut ping_sent = false;

    apply::report_activity(apply::BackendState::STATE_IDLE);

    loop {
        postgres_seams::check_for_interrupts::call()?;

        // Nothing allocated from this context outlives the iteration (the
        // streamed-chunk spool file has its own context).
        top.reset();
        // SAFETY: `top` outlives the iteration; the handle is re-derived after
        // every reset and never stored past it.
        let mcx: Mcx<'static> = unsafe { std::mem::transmute(top.mcx()) };

        let msg = match conn.get_copy_data() {
            Ok(m) => m,
            Err(e) => {
                return Err(wal_stream_receive_error(&e, ERRCODE_PROTOCOL_VIOLATION));
            }
        };

        match msg {
            CopyData::End => {
                let _ = elog::elog(LOG, "data stream from publisher has ended".to_string());
                return Ok(true);
            }
            CopyData::Block => {
                send_feedback(conn, last_received, false, false)?;

                // A chunk's spool transaction stays open until STREAM STOP.
                if !IN_REMOTE_TRANSACTION.get() && !stream_apply::in_streamed_transaction() {
                    inval::local::AcceptInvalidationMessages()?;
                    maybe_reread_subscription(mcx)?;
                    if APPLY_WORKER_EXIT.get() {
                        return Ok(false);
                    }
                    tablesync::process_syncing_tables(mcx, Some(conn), last_received)?;
                    if APPLY_WORKER_EXIT.get() {
                        return Ok(false);
                    }
                }

                // Bounded by WalWriterDelay while local commits await flush
                // (feedback urgency), else NAPTIME_PER_CYCLE.
                let wait_time = if LSN_MAPPING.with(|m| !m.borrow().is_empty()) {
                    guc_tables::vars::WalWriterDelay.read() as i64
                } else {
                    NAPTIME_PER_CYCLE
                };
                let wake_events = wait_for_apply(conn.socket(), wait_time)?;

                if interrupt::ConfigReloadPending() {
                    interrupt::SetConfigReloadPending(false);
                    guc_file::ProcessConfigFile(types_guc::GucContext::PGC_SIGHUP)?;
                }

                if wake_events & WL_TIMEOUT != 0 {
                    // Error out once publisher has been silent for
                    // wal_receiver_timeout; ping it at the halfway point.
                    let mut request_reply = false;
                    let wrt = guc_tables::vars::wal_receiver_timeout.read();
                    if wrt > 0 {
                        let now = get_ts();
                        if now >= last_recv_timestamp + wrt as i64 * 1000 {
                            ereport(ERROR)
                                .errcode(ERRCODE_CONNECTION_FAILURE)
                                .errmsg("terminating logical replication worker due to timeout")
                                .finish(loc("LogicalRepApplyLoop"))?;
                        }
                        if !ping_sent && now >= last_recv_timestamp + (wrt / 2) as i64 * 1000 {
                            request_reply = true;
                            ping_sent = true;
                        }
                    }
                    send_feedback(conn, last_received, request_reply, request_reply)?;

                    // Force reporting so long idle periods do not delay stats
                    // arbitrarily; only outside a transaction.
                    if !xact::IsTransactionState() {
                        pgstat::pending::pgstat_report_stat(true);
                    }
                }

                if !conn.consume_input() {
                    return Err(wal_stream_receive_error(
                        &conn.error_message(),
                        ERRCODE_CONNECTION_FAILURE,
                    ));
                }
            }
            CopyData::Msg(buf) => {
                // Pending SIGHUP config changes apply before each message,
                // not only after an idle wait.
                if interrupt::ConfigReloadPending() {
                    interrupt::SetConfigReloadPending(false);
                    guc_file::ProcessConfigFile(types_guc::GucContext::PGC_SIGHUP)?;
                }
                last_recv_timestamp = get_ts();
                ping_sent = false;
                let mut hdr = logicalproto::Reader::new(&buf);
                match hdr.get_byte()? {
                    b'w' => {
                        let (start_lsn, end_lsn, send_time, payload) =
                            parse_wal_data_message(&buf[1..])?;
                        if last_received < start_lsn {
                            last_received = start_lsn;
                        }
                        if last_received < end_lsn {
                            last_received = end_lsn;
                        }
                        launcher::my_worker_update_stats(last_received, send_time, false);
                        apply::apply_dispatch(mcx, Some(conn), payload)?;
                        if APPLY_WORKER_EXIT.get() {
                            return Ok(false);
                        }
                    }
                    b'k' => {
                        let (end_lsn, timestamp, reply_requested) =
                            parse_keepalive_message(&buf[1..])?;
                        if last_received < end_lsn {
                            last_received = end_lsn;
                        }
                        send_feedback(conn, last_received, reply_requested, false)?;
                        launcher::my_worker_update_stats(last_received, timestamp, true);
                    }
                    _ => {}
                }
                send_feedback(conn, last_received, false, false)?;
            }
        }
    }
}

fn start_logical_streaming(
    conn: &mut PgConn,
    startpos: XLogRecPtr,
    two_phase: bool,
) -> PgResult<()> {
    let slot = my_sub(|s| s.slotname.clone().expect("slotname checked by caller"));
    start_logical_streaming_opts(conn, &slot, startpos, two_phase)
}

pub(crate) fn start_logical_streaming_on(
    conn: &mut PgConn,
    slotname: &str,
    startpos: XLogRecPtr,
) -> PgResult<()> {
    start_logical_streaming_opts(conn, slotname, startpos, false)
}

pub(crate) fn logicalrep_proto_version(server_version: i32) -> u32 {
    if server_version >= 160000 {
        4
    } else if server_version >= 150000 {
        3
    } else if server_version >= 140000 {
        2
    } else {
        1
    }
}

pub(crate) fn logicalrep_streaming_str(
    server_version: i32,
    stream_mode: u8,
) -> Option<&'static str> {
    if server_version >= 160000 && stream_mode == pg_subscription::LOGICALREP_STREAM_PARALLEL {
        Some("parallel")
    } else if server_version >= 140000 && stream_mode != pg_subscription::LOGICALREP_STREAM_OFF {
        Some("on")
    } else {
        None
    }
}

// binary, two_phase and origin are version-gated — an older publisher
// rejects options it does not know.
#[allow(clippy::too_many_arguments)]
fn start_replication_command(
    server_version: i32,
    slot: &str,
    startpos: XLogRecPtr,
    publications: &[String],
    binary: bool,
    origin_opt: &str,
    streaming_str: Option<&str>,
    two_phase: bool,
) -> String {
    let proto_version = logicalrep_proto_version(server_version);
    let mut pubnames = String::new();
    for (i, p) in publications.iter().enumerate() {
        if i > 0 {
            pubnames.push(',');
        }
        pubnames.push('"');
        pubnames.push_str(&p.replace('"', "\"\""));
        pubnames.push('"');
    }
    let mut cmd = format!(
        "START_REPLICATION SLOT \"{}\" LOGICAL {:X}/{:X} (proto_version '{proto_version}'",
        slot.replace('"', "\"\""),
        (startpos >> 32) as u32,
        startpos as u32
    );
    if let Some(streaming) = streaming_str {
        cmd.push_str(&format!(", streaming '{streaming}'"));
    }
    if two_phase && server_version >= 150000 {
        cmd.push_str(", two_phase 'on'");
    }
    if server_version >= 160000 {
        cmd.push_str(&format!(", origin '{}'", origin_opt.replace('\'', "''")));
    }
    cmd.push_str(&format!(", publication_names '{}'", pubnames.replace('\'', "''")));
    if binary && server_version >= 140000 {
        cmd.push_str(", binary 'true'");
    }
    cmd.push(')');
    cmd
}

fn start_logical_streaming_opts(
    conn: &mut PgConn,
    slotname: &str,
    startpos: XLogRecPtr,
    two_phase: bool,
) -> PgResult<()> {
    let slot = slotname.to_string();
    let (publications, binary, origin_opt, stream_mode) = my_sub(|s| {
        (
            s.publications.clone(),
            s.binary,
            s.origin.clone(),
            s.stream,
        )
    });

    // Against an older publisher proto_version and streaming mode must
    // fall back, or the publisher rejects START_REPLICATION.
    let server_version = conn.server_version();
    let streaming_str = logicalrep_streaming_str(server_version, stream_mode);
    launcher::my_worker_set_parallel_apply(streaming_str == Some("parallel"));
    let cmd = start_replication_command(
        server_version,
        &slot,
        startpos,
        &publications,
        binary,
        &origin_opt,
        streaming_str,
        two_phase,
    );

    let res = conn.exec(&cmd)?;
    if res.status != walreceiver::client::ExecStatus::CopyBoth {
        ereport(ERROR)
            .errcode(types_error::ERRCODE_PROTOCOL_VIOLATION)
            .errmsg(format!(
                "could not start WAL streaming: {}",
                res.err.trim_end_matches('\n')
            ))
            .finish(loc("libpqrcv_startstreaming"))?;
    }
    Ok(())
}

// Registered before_shmem_exit so the abort record of a half-applied remote
// transaction does not advance the replication origin; an advanced origin for
// an incomplete transaction loses it.
fn replorigin_reset(_code: i32, _arg: datum::Datum) -> PgResult<()> {
    origin::set_replorigin_session_origin(types_core::InvalidRepOriginId);
    origin::set_replorigin_session_origin_lsn(InvalidXLogRecPtr);
    origin::set_replorigin_session_origin_timestamp(0);
    Ok(())
}

pub(crate) fn logicalrep_worker_die() -> PgResult<()> {
    ereport(FATAL)
        .errcode(ERRCODE_ADMIN_SHUTDOWN)
        .errmsg("terminating logical replication worker due to administrator command")
        .finish(loc("ProcessInterrupts"))
}

pub fn ApplyWorkerMain(main_arg: u64) -> PgResult<()> {
    let slot = main_arg as usize;
    if launcher::worker_snapshot(slot).is_some_and(|w| !w.is_tablesync()) {
        launcher::set_initializing_apply_worker(true);
    }
    ipc::before_shmem_exit(stream_apply::stream_fileset_delete_on_exit, datum::Datum::null())?;
    launcher::logicalrep_worker_attach(slot)?;
    launcher::my_worker_init_stats_times();

    procsignal::pqsignal_thread(
        procsignal::signums::SIGHUP,
        procsignal::ThreadSignalHandler::Simple(interrupt::SignalHandlerForConfigReload),
    );
    procsignal::pqsignal_thread(
        procsignal::signums::SIGTERM,
        procsignal::ThreadSignalHandler::Fallible(logicalrep_worker_die),
    );

    let result = apply_worker_body(slot);

    // Stop reading parallel workers' error mailboxes before detach
    // SIGTERMs them.
    parallel::pa_detach_all_error_mq();
    launcher::logicalrep_worker_detach();
    parallel::pa_release_pool_dsm();
    result
}

pub(crate) fn tablesync_worker_started_message(subname: &str, relname: Option<&str>) -> String {
    format!(
        "logical replication table synchronization worker for subscription \"{subname}\", table \"{}\" has started",
        relname.unwrap_or("(null)")
    )
}

pub(crate) fn apply_worker_started_message(subname: &str) -> String {
    format!("logical replication apply worker for subscription \"{subname}\" has started")
}

pub(crate) fn initialize_logrep_worker(
    mcx: Mcx<'static>,
    w: &launcher::LogicalRepWorker,
) -> PgResult<Option<String>> {
    // Run as the replica session replication role before connecting so
    // apply-path triggers see replica.
    guc::SetConfigOption(
        "session_replication_role",
        Some("replica"),
        types_guc::GucContext::PGC_SUSET,
        types_guc::GucSource::PGC_S_OVERRIDE,
    )?;

    bgworker::BackgroundWorkerInitializeConnectionByOid(w.dbid, w.userid, 0)?;

    // Always-secure search path, so malicious users cannot redirect user code.
    guc::SetConfigOption(
        "search_path",
        Some(""),
        types_guc::GucContext::PGC_SUSET,
        types_guc::GucSource::PGC_S_OVERRIDE,
    )?;

    inval::invalidate::CacheRegisterSyscacheCallback(
        cache_syscache::cacheinfo::SUBSCRIPTIONOID,
        subscription_change_cb,
        datum::Datum::null(),
    )?;
    // Re-read subscription when owner role changes, so revoked superuser
    // or altered password requirement is noticed.
    inval::invalidate::CacheRegisterSyscacheCallback(
        cache_syscache::cacheinfo::AUTHOID,
        subscription_change_cb,
        datum::Datum::null(),
    )?;

    xact::StartTransactionCommand()?;
    // Lock subscription and re-verify existence so a worker relaunched
    // mid-DROP cannot acquire the origin and deadlock DROP SUBSCRIPTION.
    lmgr::LockSharedObject(
        pg_subscription::SubscriptionRelationId,
        w.subid,
        0,
        types_rel::AccessShareLock,
    )?;
    let Some(sub) = load_subscription(mcx, w.subid)? else {
        let _ = elog::elog(
            LOG,
            format!(
                "logical replication worker for subscription {} will not start because the subscription was removed during startup",
                w.subid
            ),
        );
        // Remove entry for worker's start time so a recreated subscription
        // isn't throttled by this incarnation's timestamp.
        if !w.is_tablesync() && !w.is_parallel_apply() {
            launcher::ApplyLauncherForgetWorkerStartTime(w.subid);
        }
        xact::CommitTransactionCommand()?;
        return Ok(None);
    };
    if !sub.enabled {
        let _ = elog::elog(
            LOG,
            format!(
                "logical replication worker for subscription \"{}\" will not start because the subscription was disabled during startup",
                sub.name
            ),
        );
        // apply_worker_exit (worker.c:3944): a parallel apply worker keeps
        // running for its leader to shut down; the leader forgets its
        // launcher start time so the next launch is not throttled.
        let (forget_start_time, exit) =
            startup_disabled_action(w.is_tablesync(), w.is_parallel_apply());
        if forget_start_time {
            launcher::ApplyLauncherForgetWorkerStartTime(w.subid);
        }
        if exit {
            xact::CommitTransactionCommand()?;
            return Ok(None);
        }
    }
    let subname = sub.name.clone();
    let synccommit = sub.synccommit.clone();
    MY_SUBSCRIPTION.with(|s| *s.borrow_mut() = Some(sub));

    set_synchronous_commit(&synccommit)?;

    // Announce start while transaction is open so tablesync can resolve
    // table name through syscache.
    if w.is_tablesync() {
        let relname = lsyscache::get_rel_name(mcx, w.relid)?;
        let _ = elog::elog(
            LOG,
            tablesync_worker_started_message(&subname, relname.as_ref().map(|s| s.as_str())),
        );
    } else {
        let _ = elog::elog(LOG, apply_worker_started_message(&subname));
    }

    xact::CommitTransactionCommand()?;

    // Register origin-state reset before any remote transaction can be
    // applied, protecting against shutdown signals before commit.
    ipc::before_shmem_exit(replorigin_reset, datum::Datum::null())?;

    Ok(Some(subname))
}

fn apply_worker_body(slot: usize) -> PgResult<()> {
    let w = launcher::worker_snapshot(slot).expect("attached worker slot");

    let top = MemoryContext::new("ApplyContext");
    // SAFETY: `top` outlives the worker body; see apply_loop.
    let mcx: Mcx<'static> = unsafe { std::mem::transmute(top.mcx()) };

    if initialize_logrep_worker(mcx, &w)?.is_none() {
        return Ok(());
    }
    launcher::set_initializing_apply_worker(false);

    inval::invalidate::CacheRegisterSyscacheCallback(
        cache_syscache::cacheinfo::SUBSCRIPTIONRELMAP,
        tablesync::invalidate_table_states_cb,
        datum::Datum::null(),
    )?;

    if w.is_tablesync() {
        apply::logicalrep_relmap_prepare()?;
        let r = tablesync::run_tablesync_worker(mcx, w.relid);
        let r = match r {
            Err(e) => finish_apply_error(mcx, e, true),
            ok => ok,
        };
        let _ = origin::replorigin_session_reset();
        return r;
    }

    if my_sub(|s| s.slotname.is_none()) {
        ereport(ERROR)
            .errcode(ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE)
            .errmsg("subscription has no replication slot set")
            .finish(loc("run_apply_worker"))?;
        unreachable!();
    }

    let originname = format!("pg_{}", my_sub(|s| s.oid));
    xact::StartTransactionCommand()?;
    let mut originid = origin::replorigin_by_name(&originname, true)?;
    if originid == types_core::InvalidRepOriginId {
        originid = origin::replorigin_create(mcx, &originname)?;
    }
    origin::replorigin_session_setup(originid, 0)?;
    origin::set_replorigin_session_origin(originid);
    let origin_startpos = origin::replorigin_session_get_progress(false)?;
    xact::CommitTransactionCommand()?;

    set_apply_error_context_origin(&originname);

    let must_use_password = my_sub(|s| s.passwordrequired && !s.ownersuperuser);
    let (conninfo, name) = my_sub(|s| (s.conninfo.clone(), s.name.clone()));
    let mut conn = match walreceiver::client::connect_extended(
        &conninfo,
        true,
        true,
        must_use_password,
        &name,
    )? {
        Ok(c) => c,
        Err(e) => {
            ereport(ERROR)
                .errcode(ERRCODE_CONNECTION_FAILURE)
                .errmsg(format!(
                    "apply worker for subscription \"{name}\" could not connect to the publisher: {e}"
                ))
                .finish(loc("run_apply_worker"))?;
            unreachable!();
        }
    };

    let _ = walreceiver::client::identify_system(&mut conn)?;

    let enable_two_phase = my_sub(|s| s.twophasestate)
        == pg_subscription::LOGICALREP_TWOPHASE_STATE_PENDING
        && tablesync::all_tablesyncs_ready(mcx)?;

    apply::logicalrep_relmap_prepare()?;

    start_logical_streaming(&mut conn, origin_startpos, enable_two_phase)?;

    if enable_two_phase {
        // Persist ENABLED so a restart does not re-request it.
        xact::StartTransactionCommand()?;
        let snap = snapmgr::GetTransactionSnapshot()?;
        snapmgr::PushActiveSnapshot(&snap)?;
        pg_subscription::UpdateTwoPhaseState(
            mcx,
            my_sub(|s| s.oid),
            pg_subscription::LOGICALREP_TWOPHASE_STATE_ENABLED,
        )?;
        snapmgr::PopActiveSnapshot()?;
        xact::CommitTransactionCommand()?;
        MY_SUBSCRIPTION.with(|s| {
            if let Some(s) = s.borrow_mut().as_mut() {
                s.twophasestate = pg_subscription::LOGICALREP_TWOPHASE_STATE_ENABLED;
            }
        });
    }

    let name = my_sub(|s| s.name.clone());
    let state = match my_sub(|s| s.twophasestate) {
        pg_subscription::LOGICALREP_TWOPHASE_STATE_DISABLED => "DISABLED",
        pg_subscription::LOGICALREP_TWOPHASE_STATE_PENDING => "PENDING",
        pg_subscription::LOGICALREP_TWOPHASE_STATE_ENABLED => "ENABLED",
        _ => "?",
    };
    let _ = elog::elog(
        DEBUG1,
        format!("logical replication apply worker for subscription \"{name}\" two_phase is {state}"),
    );

    let r = apply_loop(&mut conn, origin_startpos);
    let r = match r {
        Err(e) => finish_apply_error(mcx, e, false),
        ok => ok,
    };

    let _ = origin::replorigin_session_reset();
    r
}

// Reset origin advance state so a failed apply cannot advance origin
// progress; the publisher never resends that transaction.
fn finish_apply_error(
    mcx: Mcx<'static>,
    err: Box<types_error::PgError>,
    is_tablesync: bool,
) -> PgResult<()> {
    let _ = replorigin_reset(0, datum::Datum::null());
    let (subid, disableonerr) = my_sub(|s| (s.oid, s.disableonerr));
    if disableonerr {
        return disable_subscription_and_exit(mcx, err, subid, is_tablesync);
    }
    // Abort the current transaction so that the stats message is sent in
    // an idle state.
    xact::AbortOutOfAnyTransaction()?;
    pgstat::subscription::pgstat_report_subscription_error(subid, !is_tablesync);
    Err(err)
}

fn disable_subscription_and_exit(
    mcx: Mcx<'static>,
    err: Box<types_error::PgError>,
    subid: types_core::Oid,
    is_tablesync: bool,
) -> PgResult<()> {
    elog::emit_error_report_for(&err);
    xact::AbortOutOfAnyTransaction()?;

    pgstat::subscription::pgstat_report_subscription_error(subid, !is_tablesync);

    xact::StartTransactionCommand()?;
    let snap = snapmgr::GetTransactionSnapshot()?;
    snapmgr::PushActiveSnapshot(&snap)?;
    pg_subscription::DisableSubscription(mcx, subid)?;
    snapmgr::PopActiveSnapshot()?;
    xact::CommitTransactionCommand()?;

    if !is_tablesync {
        launcher::ApplyLauncherForgetWorkerStartTime(subid);
    }

    let name = my_sub(|s| s.name.clone());
    let _ = elog::elog(
        LOG,
        format!("subscription \"{name}\" has been disabled because of an error"),
    );
    Ok(())
}

pub fn init_seams() {
    logical_worker_seams::apply_worker_main::set(ApplyWorkerMain);
    parallel::init_seams();
}
