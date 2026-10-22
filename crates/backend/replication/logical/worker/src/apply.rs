use std::ffi::CString;
use std::rc::Rc;

use datum::Datum;
use elog::ereport;
use logicalproto::{
    LogicalRepTupleData, Reader, LOGICALREP_COLUMN_BINARY, LOGICALREP_COLUMN_TEXT,
    LOGICALREP_COLUMN_UNCHANGED,
};
use logicalrelation::LogicalRepRelMapEntry;
use mcx::Mcx;
use types_core::{InvalidOid, InvalidTransactionId, Oid, TransactionId};
use types_error::{
    PgError, PgResult, ERRCODE_FEATURE_NOT_SUPPORTED, ERRCODE_INVALID_BINARY_REPRESENTATION,
    ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE, ERRCODE_PROTOCOL_VIOLATION,
    ERRCODE_T_R_SERIALIZATION_FAILURE, ERRCODE_UNDEFINED_FUNCTION, ERROR, LOG,
};
use types_rel::Relation;
use types_scan::scankey::{ScanKeyData, SK_ISNULL, SK_SEARCHNULL};
use types_slot::SlotData;

use walreceiver::client::PgConn;

use crate::{
    loc, my_sub, reset_apply_error_context_info, set_apply_error_context_attnum,
    set_apply_error_context_command, set_apply_error_context_rel, set_apply_error_context_xact,
    IN_REMOTE_TRANSACTION, REMOTE_FINAL_LSN,
};

pub(crate) use backend_status_seams::BackendState;

pub(crate) fn report_activity(state: BackendState) {
    backend_status_seams::pgstat_report_activity::call(state, None);
}

const MSG_BEGIN: u8 = b'B';
const MSG_COMMIT: u8 = b'C';
const MSG_ORIGIN: u8 = b'O';
const MSG_INSERT: u8 = b'I';
const MSG_UPDATE: u8 = b'U';
const MSG_DELETE: u8 = b'D';
const MSG_TRUNCATE: u8 = b'T';
const MSG_RELATION: u8 = b'R';
const MSG_TYPE: u8 = b'Y';
const MSG_MESSAGE: u8 = b'M';
const MSG_STREAM_START: u8 = b'S';
const MSG_STREAM_STOP: u8 = b'E';
const MSG_STREAM_COMMIT: u8 = b'c';
const MSG_STREAM_ABORT: u8 = b'A';
const MSG_BEGIN_PREPARE: u8 = b'b';
const MSG_PREPARE: u8 = b'P';
const MSG_COMMIT_PREPARED: u8 = b'K';
const MSG_ROLLBACK_PREPARED: u8 = b'r';
const MSG_STREAM_PREPARE: u8 = b'p';

pub(crate) fn logicalrep_relmap_prepare() -> PgResult<()> {
    logicalrelation::logicalrep_relmap_init()
}

// This port's fmgr convention lets a function's result live in flinfo-owned
// scratch (fn_extra) — dropping the FmgrInfo frees the returned datum's storage.
pub(crate) struct InFuncs {
    per_col: Vec<Option<(fmgr::FmgrInfo, Oid)>>,
    recv_per_col: Vec<Option<(fmgr::FmgrInfo, Oid)>>,
}

impl InFuncs {
    fn new(natts: usize) -> PgResult<Self> {
        Ok(InFuncs {
            per_col: ::mcx::std_vec_from_elem("logicalworker", None, natts)?,
            recv_per_col: ::mcx::std_vec_from_elem("logicalworker", None, natts)?,
        })
    }
    fn get(&mut self, i: usize, atttypid: Oid) -> PgResult<&mut (fmgr::FmgrInfo, Oid)> {
        if self.per_col[i].is_none() {
            let (typinput, typioparam) = lsyscache::getTypeInputInfo(atttypid)?;
            self.per_col[i] = Some((fmgr_seams::fmgr_info::call(typinput)?, typioparam));
        }
        Ok(self.per_col[i].as_mut().expect("just filled"))
    }
    fn get_recv(&mut self, i: usize, atttypid: Oid) -> PgResult<&mut (fmgr::FmgrInfo, Oid)> {
        if self.recv_per_col[i].is_none() {
            let (typreceive, typioparam) = lsyscache::getTypeBinaryInputInfo(atttypid)?;
            self.recv_per_col[i] = Some((fmgr_seams::fmgr_info::call(typreceive)?, typioparam));
        }
        Ok(self.recv_per_col[i].as_mut().expect("just filled"))
    }
}

fn slot_store_datum<'mcx>(
    mcx: Mcx<'mcx>,
    infuncs: &mut InFuncs,
    i: usize,
    atttypid: Oid,
    atttypmod: i32,
    colstatus: u8,
    bytes: &[u8],
    remoteattnum: usize,
) -> PgResult<(Datum, bool)> {
    match colstatus {
        LOGICALREP_COLUMN_TEXT => {
            // This port has from_utf8_unchecked sinks that rely on the engine
            // invariant that stored strings are valid in the database
            // encoding; feeding them invalid bytes is UB.
            mbutils::pg_verifymbstr(bytes, false)?;
            let cstr = CString::new(bytes).map_err(|_| {
                Box::new(types_error::PgError::error(
                    "invalid text column data in logical replication message".to_string(),
                ))
            })?;
            let (flinfo, typioparam) = infuncs.get(i, atttypid)?;
            let ioparam = *typioparam;
            let d = fmgr::soft::input_function_call(
                flinfo,
                Some(cstr.as_c_str()),
                ioparam,
                atttypmod,
                mcx,
            )?;
            Ok((d, false))
        }
        LOGICALREP_COLUMN_BINARY => {
            let (flinfo, typioparam) = infuncs.get_recv(i, atttypid)?;
            let ioparam = *typioparam;
            let d = receive_binary_column(mcx, flinfo, ioparam, atttypmod, bytes, remoteattnum)?;
            Ok((d, false))
        }
        _ => Ok((Datum::null(), true)),
    }
}

pub(crate) fn receive_binary_column<'mcx>(
    mcx: Mcx<'mcx>,
    flinfo: &mut fmgr::FmgrInfo,
    typioparam: Oid,
    atttypmod: i32,
    bytes: &[u8],
    remoteattnum: usize,
) -> PgResult<Datum> {
    let mut buf = stringinfo::StringInfo::from_vec(mcx::slice_in(mcx, bytes)?)?;
    let d = fmgr::receive_function_call(flinfo, Some(&mut buf), typioparam, atttypmod, mcx)?;
    if buf.cursor != buf.len() {
        ereport(ERROR)
            .errcode(ERRCODE_INVALID_BINARY_REPRESENTATION)
            .errmsg(format!(
                "incorrect binary data format in logical replication column {}",
                remoteattnum + 1
            ))
            .finish(loc("slot_store_data"))?;
    }
    Ok(d)
}

// Ok(false): rereading the subscription asked the worker to exit, where C's
// apply_worker_exit proc_exit(0)s inside maybe_reread_subscription
// (worker.c:3966); the caller returns without applying and the apply loop
// exits with the transaction started here still open, so it aborts.
pub(crate) fn begin_replication_step(mcx: Mcx<'_>) -> PgResult<bool> {
    xact::SetCurrentStatementStartTimestamp();

    if !xact::IsTransactionState() {
        xact::StartTransactionCommand()?;
        crate::maybe_reread_subscription(mcx)?;
        if crate::apply_worker_exit_requested() {
            return Ok(false);
        }
    }
    let snap = snapmgr::GetTransactionSnapshot()?;
    snapmgr::PushActiveSnapshot(&snap)?;
    Ok(true)
}

pub(crate) fn end_replication_step() -> PgResult<()> {
    snapmgr::PopActiveSnapshot()?;
    xact::CommandCounterIncrement()
}

fn should_apply_changes_for_rel(entry: &LogicalRepRelMapEntry) -> PgResult<bool> {
    const SUBREL_STATE_READY: u8 = b'r';
    const SUBREL_STATE_SYNCDONE: u8 = b's';
    const SUBREL_STATE_UNKNOWN: u8 = 0;
    if crate::tablesync::AM_TABLESYNC_WORKER.with(std::cell::Cell::get) {
        let myrelid = launcher::worker_snapshot(launcher::my_worker_slot().expect("attached"))
            .map(|w| w.relid)
            .unwrap_or(types_core::InvalidOid);
        return Ok(entry.localreloid == myrelid);
    }
    if crate::parallel::am_parallel_apply_worker() {
        if entry.state != SUBREL_STATE_READY && entry.state != SUBREL_STATE_UNKNOWN {
            let name = my_sub(|s| s.name.clone());
            ereport(ERROR)
                .errcode(types_error::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE)
                .errmsg(format!(
                    "logical replication parallel apply worker for subscription \"{name}\" will stop"
                ))
                .errdetail("Cannot handle streamed replication transactions using parallel apply workers until all tables have been synchronized.")
                .finish(loc("should_apply_changes_for_rel"))?;
        }
        return Ok(entry.state == SUBREL_STATE_READY);
    }
    Ok(match entry.state {
        SUBREL_STATE_READY => true,
        SUBREL_STATE_SYNCDONE => entry.statelsn <= REMOTE_FINAL_LSN.get(),
        // The tablesync worker owns those changes; the leader skips them.
        _ => false,
    })
}

// Inside a streamed chunk every data message spools to the transaction's
// file instead of applying.
pub(crate) fn apply_dispatch(
    mcx: Mcx<'static>,
    conn: Option<&mut PgConn>,
    buf: &[u8],
) -> PgResult<()> {
    let action = Reader::new(buf).get_byte()?;
    // C restores it only on the normal exit — an error leaves the innermost
    // command for the callback to report.
    let saved_command = set_apply_error_context_command(action);
    let r = apply_dispatch_guts(mcx, conn, action, buf);
    if r.is_ok() {
        set_apply_error_context_command(saved_command);
    }
    r
}

fn apply_dispatch_guts(
    mcx: Mcx<'static>,
    mut conn: Option<&mut PgConn>,
    action: u8,
    buf: &[u8],
) -> PgResult<()> {
    // RELATION/TYPE/MESSAGE are processed even while skipping.
    if matches!(action, MSG_INSERT | MSG_UPDATE | MSG_DELETE | MSG_TRUNCATE)
        && crate::is_skipping_changes()
    {
        return Ok(());
    }
    // In a streamed chunk every data message carries the (sub)txn xid first.
    let mut payload = &buf[1..];
    if matches!(
        action,
        MSG_RELATION | MSG_TYPE | MSG_INSERT | MSG_UPDATE | MSG_DELETE | MSG_TRUNCATE | MSG_MESSAGE
    ) {
        use crate::stream_apply::StreamedHandling;
        match crate::stream_apply::handle_streamed_transaction(mcx, action, buf)? {
            StreamedHandling::Consumed => return Ok(()),
            StreamedHandling::ContinueAt(off) => payload = &buf[off..],
            StreamedHandling::NotStreamed => {}
        }
    }
    let mut r = Reader::new(payload);
    match action {
        MSG_BEGIN => apply_handle_begin(&mut r),
        MSG_COMMIT => apply_handle_commit(mcx, conn, &mut r),
        MSG_ORIGIN => apply_handle_origin(),
        MSG_RELATION => apply_handle_relation(mcx, &mut r),
        MSG_TYPE => {
            let _ = logicalproto::logicalrep_read_typ(&mut r)?;
            Ok(())
        }
        MSG_INSERT => apply_handle_insert(mcx, &mut r),
        MSG_UPDATE => apply_handle_update(mcx, &mut r),
        MSG_DELETE => apply_handle_delete(mcx, &mut r),
        MSG_TRUNCATE => apply_handle_truncate(mcx, &mut r),
        MSG_MESSAGE => Ok(()), // transactional messages are ignored by apply
        MSG_STREAM_START => crate::stream_apply::apply_handle_stream_start(mcx, buf),
        MSG_STREAM_STOP => crate::stream_apply::apply_handle_stream_stop(mcx, buf),
        MSG_STREAM_COMMIT => {
            crate::stream_apply::apply_handle_stream_commit(mcx, conn.as_deref_mut(), buf)
        }
        MSG_STREAM_ABORT => crate::stream_apply::apply_handle_stream_abort(mcx, buf),
        MSG_STREAM_PREPARE => {
            crate::stream_apply::apply_handle_stream_prepare(mcx, conn.as_deref_mut(), buf)
        }
        MSG_BEGIN_PREPARE => apply_handle_begin_prepare(&mut r),
        MSG_PREPARE => apply_handle_prepare(mcx, conn, &mut r),
        MSG_COMMIT_PREPARED => apply_handle_commit_prepared(mcx, conn, &mut r),
        MSG_ROLLBACK_PREPARED => apply_handle_rollback_prepared(mcx, conn, &mut r),
        other => {
            ereport(ERROR)
                .errcode(ERRCODE_PROTOCOL_VIOLATION)
                .errmsg(format!("invalid logical replication message type \"??? ({other})\""))
                .finish(loc("apply_dispatch"))?;
            unreachable!();
        }
    }
}

fn apply_handle_origin() -> PgResult<()> {
    if !crate::stream_apply::in_streamed_transaction()
        && (!IN_REMOTE_TRANSACTION.get()
            || (xact::IsTransactionState()
                && !crate::tablesync::AM_TABLESYNC_WORKER.with(std::cell::Cell::get)))
    {
        ereport(ERROR)
            .errcode(ERRCODE_PROTOCOL_VIOLATION)
            .errmsg("ORIGIN message sent out of order")
            .finish(loc("apply_handle_origin"))?;
        unreachable!();
    }
    Ok(())
}

fn apply_handle_begin(r: &mut Reader<'_>) -> PgResult<()> {
    let begin = logicalproto::logicalrep_read_begin(r)?;
    set_apply_error_context_xact(begin.xid, begin.final_lsn);

    REMOTE_FINAL_LSN.set(begin.final_lsn);
    crate::maybe_start_skipping_changes(begin.final_lsn);
    IN_REMOTE_TRANSACTION.set(true);

    report_activity(BackendState::STATE_RUNNING);
    Ok(())
}

fn apply_handle_commit(
    mcx: Mcx<'static>,
    conn: Option<&mut PgConn>,
    r: &mut Reader<'_>,
) -> PgResult<()> {
    let commit = logicalproto::logicalrep_read_commit(r)?;

    if commit.commit_lsn != REMOTE_FINAL_LSN.get() {
        ereport(ERROR)
            .errcode(ERRCODE_PROTOCOL_VIOLATION)
            .errmsg(format!(
                "incorrect commit LSN {:X}/{:X} in commit message (expected {:X}/{:X})",
                (commit.commit_lsn >> 32) as u32,
                commit.commit_lsn as u32,
                (REMOTE_FINAL_LSN.get() >> 32) as u32,
                REMOTE_FINAL_LSN.get() as u32,
            ))
            .finish(loc("apply_handle_commit"))?;
    }

    apply_handle_commit_internal(mcx, &commit)?;
    crate::tablesync::process_syncing_tables(mcx, conn, commit.end_lsn)?;

    report_activity(BackendState::STATE_IDLE);
    reset_apply_error_context_info();
    Ok(())
}

pub(crate) fn apply_handle_commit_internal(
    mcx: Mcx<'static>,
    commit: &logicalproto::LogicalRepCommitData,
) -> PgResult<()> {
    if crate::is_skipping_changes() {
        crate::stop_skipping_changes();

        if !xact::IsTransactionState() {
            xact::StartTransactionCommand()?;
        }
    }

    if xact::IsTransactionState() {
        crate::clear_subscription_skip_lsn(mcx, commit.commit_lsn)?;

        // The session origin lsn/timestamp ride the local commit record
        // (origin crate seams feed xact's commit writer).
        origin::set_replorigin_session_origin_lsn(commit.end_lsn);
        origin::set_replorigin_session_origin_timestamp(commit.committime);

        xact::CommitTransactionCommand()?;

        if xact::IsTransactionBlock() {
            xact::EndTransactionBlock(false)?;
            xact::CommitTransactionCommand()?;
        }

        pgstat::pending::pgstat_report_stat(false);

        let local_end = transam_xlog_seams::xact_last_commit_end::call();
        crate::store_flush_position(commit.end_lsn, local_end);
    } else {
        inval::local::AcceptInvalidationMessages()?;
        crate::maybe_reread_subscription(mcx)?;
    }

    IN_REMOTE_TRANSACTION.set(false);
    Ok(())
}

fn apply_handle_begin_prepare(r: &mut Reader<'_>) -> PgResult<()> {
    // Tablesync should never receive prepare.
    if crate::tablesync::AM_TABLESYNC_WORKER.with(std::cell::Cell::get) {
        ereport(ERROR)
            .errcode(ERRCODE_PROTOCOL_VIOLATION)
            .errmsg("tablesync worker received a BEGIN PREPARE message")
            .finish(loc("apply_handle_begin_prepare"))?;
    }

    let begin = logicalproto::logicalrep_read_begin_prepare(r)?;
    set_apply_error_context_xact(begin.xid, begin.prepare_lsn);

    REMOTE_FINAL_LSN.set(begin.prepare_lsn);
    crate::maybe_start_skipping_changes(begin.prepare_lsn);
    IN_REMOTE_TRANSACTION.set(true);

    report_activity(BackendState::STATE_RUNNING);
    Ok(())
}

pub(crate) fn apply_handle_prepare_internal(
    prepare_data: &logicalproto::LogicalRepPreparedTxnData,
) -> PgResult<()> {
    // The publisher's own GID could deadlock with multiple subscriptions
    // from the same node.
    let gid = twophase::TwoPhaseTransactionGid(my_sub(|s| s.oid), prepare_data.xid)?;

    // BeginTransactionBlock is necessary to balance the EndTransactionBlock
    // called within the PrepareTransactionBlock below.
    if !xact::IsTransactionBlock() {
        xact::BeginTransactionBlock()?;
        xact::CommitTransactionCommand()?; // Completes the preceding Begin command.
    }

    origin::set_replorigin_session_origin_lsn(prepare_data.end_lsn);
    origin::set_replorigin_session_origin_timestamp(prepare_data.prepare_time);

    xact::PrepareTransactionBlock(&gid)?;
    Ok(())
}

fn apply_handle_prepare(
    mcx: Mcx<'static>,
    conn: Option<&mut PgConn>,
    r: &mut Reader<'_>,
) -> PgResult<()> {
    let prepare_data = logicalproto::logicalrep_read_prepare(r)?;

    if prepare_data.prepare_lsn != REMOTE_FINAL_LSN.get() {
        ereport(ERROR)
            .errcode(ERRCODE_PROTOCOL_VIOLATION)
            .errmsg(format!(
                "incorrect prepare LSN {:X}/{:X} in prepare message (expected {:X}/{:X})",
                (prepare_data.prepare_lsn >> 32) as u32,
                prepare_data.prepare_lsn as u32,
                (REMOTE_FINAL_LSN.get() >> 32) as u32,
                REMOTE_FINAL_LSN.get() as u32,
            ))
            .finish(loc("apply_handle_prepare"))?;
    }

    // Unlike commit, we always prepare the transaction even if nothing
    // changed in it: at commit prepared time we won't know whether we skipped
    // preparing.
    if !begin_replication_step(mcx)? {
        return Ok(());
    }
    apply_handle_prepare_internal(&prepare_data)?;
    end_replication_step()?;
    xact::CommitTransactionCommand()?;
    pgstat::pending::pgstat_report_stat(false);

    // The prepare record is always flushed, so acknowledging the remote_end
    // LSN with an invalid local LSN is fine.
    crate::store_flush_position(prepare_data.end_lsn, types_core::InvalidXLogRecPtr);

    IN_REMOTE_TRANSACTION.set(false);

    crate::tablesync::process_syncing_tables(mcx, conn, prepare_data.end_lsn)?;

    // Since the transaction is already prepared, a crash before clearing the
    // subskiplsn leaves it set, but the transaction won't be resent.
    crate::stop_skipping_changes();
    crate::clear_subscription_skip_lsn(mcx, prepare_data.prepare_lsn)?;

    report_activity(BackendState::STATE_IDLE);
    reset_apply_error_context_info();
    Ok(())
}

fn apply_handle_commit_prepared(
    mcx: Mcx<'static>,
    conn: Option<&mut PgConn>,
    r: &mut Reader<'_>,
) -> PgResult<()> {
    let prepare_data = logicalproto::logicalrep_read_commit_prepared(r)?;
    set_apply_error_context_xact(prepare_data.xid, prepare_data.commit_lsn);

    let gid = twophase::TwoPhaseTransactionGid(my_sub(|s| s.oid), prepare_data.xid)?;

    // There is no transaction when COMMIT PREPARED is called.
    if !begin_replication_step(mcx)? {
        return Ok(());
    }

    origin::set_replorigin_session_origin_lsn(prepare_data.end_lsn);
    origin::set_replorigin_session_origin_timestamp(prepare_data.commit_time);

    twophase::FinishPreparedTransaction(&gid, true)?;
    end_replication_step()?;
    xact::CommitTransactionCommand()?;
    pgstat::pending::pgstat_report_stat(false);

    let local_end = transam_xlog_seams::xact_last_commit_end::call();
    crate::store_flush_position(prepare_data.end_lsn, local_end);
    IN_REMOTE_TRANSACTION.set(false);

    crate::tablesync::process_syncing_tables(mcx, conn, prepare_data.end_lsn)?;

    crate::clear_subscription_skip_lsn(mcx, prepare_data.end_lsn)?;

    report_activity(BackendState::STATE_IDLE);
    reset_apply_error_context_info();
    Ok(())
}

fn apply_handle_rollback_prepared(
    mcx: Mcx<'static>,
    conn: Option<&mut PgConn>,
    r: &mut Reader<'_>,
) -> PgResult<()> {
    let rollback_data = logicalproto::logicalrep_read_rollback_prepared(r)?;
    set_apply_error_context_xact(rollback_data.xid, rollback_data.rollback_end_lsn);

    let gid = twophase::TwoPhaseTransactionGid(my_sub(|s| s.oid), rollback_data.xid)?;

    // It is possible that we haven't received the prepare because it occurred
    // before the walsender reached a consistent point or two_phase was not
    // yet enabled; skip the rollback in that case.
    if twophase::LookupGXact(&gid, rollback_data.prepare_end_lsn, rollback_data.prepare_time)? {
        origin::set_replorigin_session_origin_lsn(rollback_data.rollback_end_lsn);
        origin::set_replorigin_session_origin_timestamp(rollback_data.rollback_time);

        // There is no transaction when ROLLBACK PREPARED is called.
        if !begin_replication_step(mcx)? {
            return Ok(());
        }
        twophase::FinishPreparedTransaction(&gid, false)?;
        end_replication_step()?;
        xact::CommitTransactionCommand()?;
        pgstat::pending::pgstat_report_stat(false);

        crate::clear_subscription_skip_lsn(mcx, rollback_data.rollback_end_lsn)?;
    }

    pgstat::pending::pgstat_report_stat(false);

    // The rollback WAL record is always flushed.
    crate::store_flush_position(
        rollback_data.rollback_end_lsn,
        types_core::InvalidXLogRecPtr,
    );
    IN_REMOTE_TRANSACTION.set(false);

    crate::tablesync::process_syncing_tables(mcx, conn, rollback_data.rollback_end_lsn)?;

    report_activity(BackendState::STATE_IDLE);
    reset_apply_error_context_info();
    Ok(())
}

fn apply_handle_relation(mcx: Mcx<'_>, r: &mut Reader<'_>) -> PgResult<()> {
    // The relmap update touches only the in-memory map; opening a transaction
    // leaves it dangling when RELATION is applied outside a remote transaction.
    let _ = mcx;
    let rel = logicalproto::logicalrep_read_rel(r)?;
    logicalrelation::logicalrep_relmap_update(&rel);
    Ok(())
}

pub(crate) fn tuple_column_check(remoteattnum: usize, ncols: usize) -> PgResult<()> {
    if remoteattnum >= ncols {
        ereport(ERROR)
            .errcode(ERRCODE_PROTOCOL_VIOLATION)
            .errmsg(format!(
                "logical replication column {} not found in tuple: only {} column(s) received",
                remoteattnum + 1,
                ncols
            ))
            .finish(loc("slot_store_data"))?;
        unreachable!();
    }
    Ok(())
}

fn slot_store_data<'mcx>(
    mcx: Mcx<'mcx>,
    slot: &mut SlotData<'mcx>,
    entry: &LogicalRepRelMapEntry,
    rel: &Relation<'mcx>,
    tup: &LogicalRepTupleData,
    infuncs: &mut InFuncs,
) -> PgResult<()> {
    let natts = rel.rd_att.natts as usize;
    exectuples::exec_clear_tuple(slot, mcx);

    for i in 0..natts {
        let att = rel.rd_att.attr(i);
        let remote = entry.attrmap.get(i).copied().unwrap_or(-1);
        let (value, isnull) = if !att.attisdropped && remote >= 0 {
            let m = remote as usize;
            tuple_column_check(m, tup.ncols)?;
            let bytes = tup.colvalues[m].as_deref().unwrap_or(&[]);
            set_apply_error_context_attnum(m as i32);
            let v = slot_store_datum(
                mcx,
                infuncs,
                i,
                att.atttypid,
                att.atttypmod,
                tup.colstatus[m],
                bytes,
                m,
            )?;
            set_apply_error_context_attnum(-1);
            v
        } else {
            (Datum::null(), true)
        };
        let base = slot.base_mut();
        base.tts_values[i] = value;
        base.tts_isnull[i] = isnull;
    }

    exectuples::exec_store_virtual_tuple(slot);
    Ok(())
}

// A result datum may live in flinfo-owned scratch, so the states must outlive
// the DML that consumes the slot.
pub(crate) fn needs_default_fill(attisdropped: bool, attgenerated: i8, remote: i16) -> bool {
    !attisdropped && attgenerated == 0 && remote < 0
}

fn slot_fill_defaults<'mcx>(
    mcx: Mcx<'mcx>,
    entry: &LogicalRepRelMapEntry,
    rel: &Relation<'mcx>,
    slot: &mut SlotData<'mcx>,
) -> PgResult<Vec<mcx::PgBox<'mcx, execexpr::ExprState<'mcx>>>> {
    let mut keep_alive = Vec::new();
    let natts = rel.rd_att.natts as usize;

    if natts == entry.remoterel.natts {
        return Ok(keep_alive);
    }

    let mut defmap = Vec::new();
    for i in 0..natts {
        let att = rel.rd_att.attr(i);
        let remote = entry.attrmap.get(i).copied().unwrap_or(-1);
        if !needs_default_fill(att.attisdropped, att.attgenerated, remote) {
            continue;
        }
        let Some(defexpr) = rewrite_handler::build_column_default(mcx, rel, i + 1)? else {
            continue;
        };
        let defexpr = clauses::eval_const_expressions(mcx, defexpr)?;
        nodes_core::fix_opfuncids(defexpr)?;
        let mut state = execexpr::exec_init_expr(mcx, Some(defexpr), execexpr::ParamBind::NONE)?
            .expect("column default expression");
        state.arm_result_mcx(mcx);
        keep_alive.push(state);
        defmap.push(i);
    }

    for (state, &i) in keep_alive.iter_mut().zip(defmap.iter()) {
        let mut slots = execexpr::EvalSlots { scan: None, inner: None, outer: None };
        let r = execexpr::exec_eval_expr(state, &mut slots)?;
        let base = slot.base_mut();
        base.tts_values[i] = r.value;
        base.tts_isnull[i] = r.isnull;
    }
    Ok(keep_alive)
}

fn slot_modify_data<'mcx>(
    mcx: Mcx<'mcx>,
    slot: &mut SlotData<'mcx>,
    srcslot: &mut SlotData<'mcx>,
    entry: &LogicalRepRelMapEntry,
    rel: &Relation<'mcx>,
    tup: &LogicalRepTupleData,
    infuncs: &mut InFuncs,
) -> PgResult<()> {
    let natts = rel.rd_att.natts as usize;
    exectuples::exec_clear_tuple(slot, mcx);

    exectuples::slot_getallattrs(srcslot);
    for i in 0..natts {
        let (v, n) = {
            let sb = srcslot.base();
            (sb.tts_values[i], sb.tts_isnull[i])
        };
        let base = slot.base_mut();
        base.tts_values[i] = v;
        base.tts_isnull[i] = n;
    }

    for i in 0..natts {
        let att = rel.rd_att.attr(i);
        let remote = entry.attrmap.get(i).copied().unwrap_or(-1);
        if remote < 0 {
            continue;
        }
        let m = remote as usize;
        tuple_column_check(m, tup.ncols)?;
        if tup.colstatus[m] == LOGICALREP_COLUMN_UNCHANGED {
            continue;
        }
        let bytes = tup.colvalues[m].as_deref().unwrap_or(&[]);
        set_apply_error_context_attnum(m as i32);
        let (value, isnull) = slot_store_datum(
            mcx,
            infuncs,
            i,
            att.atttypid,
            att.atttypmod,
            tup.colstatus[m],
            bytes,
            m,
        )?;
        set_apply_error_context_attnum(-1);
        let base = slot.base_mut();
        base.tts_values[i] = value;
        base.tts_isnull[i] = isnull;
    }

    exectuples::exec_store_virtual_tuple(slot);
    Ok(())
}

// The data half is None when track_commit_timestamp is off or the xid is
// outside the retained commit-ts range.
pub(crate) fn get_tuple_transaction_info(
    localslot: &SlotData<'_>,
) -> PgResult<(types_core::TransactionId, Option<(types_core::TimestampTz, types_core::RepOriginId)>)> {
    // A virtual slot (a pgrcolumnar target's) has no system columns (C's slot_getsysattr would raise 0A000 here).
    if matches!(localslot, SlotData::Virtual(_)) {
        return Ok((types_core::InvalidTransactionId, None));
    }
    let mut isnull = false;
    let xmin_d = exectuples::slot_getsysattr(
        localslot,
        types_tuple::MinTransactionIdAttributeNumber,
        &mut isnull,
    )?;
    debug_assert!(!isnull);
    let xmin = xmin_d.as_u32();

    if !guc_tables::vars::track_commit_timestamp.read() {
        return Ok((xmin, None));
    }
    Ok((xmin, commit_ts::TransactionIdGetCommitTsData(xmin)?))
}

#[allow(clippy::too_many_arguments)]
fn report_origin_differs<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    entry: &LogicalRepRelMapEntry,
    remoteslot: &mut SlotData<'mcx>,
    localslot: &mut SlotData<'mcx>,
    newtup: Option<&LogicalRepTupleData>,
    localxmin: types_core::TransactionId,
    localorigin: types_core::RepOriginId,
    localts: types_core::TimestampTz,
) -> PgResult<()> {
    use crate::conflict::{report_apply_conflict, ConflictTupleInfo, ConflictType};
    // The new tuple's datums live in this cache's scratch until the report
    // is done.
    let mut infuncs = InFuncs::new(rel.rd_att.natts as usize)?;
    let mut newslot = match newtup {
        Some(newtup) => {
            let mut slot = tableam_real::table_slot_create(mcx, rel)?;
            slot_store_data(mcx, &mut slot, entry, rel, newtup, &mut infuncs)?;
            Some(slot)
        }
        None => None,
    };
    let ty = if newtup.is_some() {
        ConflictType::UpdateOriginDiffers
    } else {
        ConflictType::DeleteOriginDiffers
    };
    let mut conflicttuple = [ConflictTupleInfo {
        slot: Some(localslot),
        indexoid: InvalidOid,
        xmin: localxmin,
        origin: localorigin,
        ts: localts,
    }];
    report_apply_conflict(mcx, rel, LOG, ty, Some(remoteslot), newslot.as_mut(), &mut conflicttuple)
}

fn report_row_missing<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    entry: &LogicalRepRelMapEntry,
    remoteslot: &mut SlotData<'mcx>,
    localslot: &mut SlotData<'mcx>,
    newtup: Option<&LogicalRepTupleData>,
) -> PgResult<()> {
    use crate::conflict::{report_apply_conflict, ConflictTupleInfo, ConflictType};
    let mut infuncs = InFuncs::new(rel.rd_att.natts as usize)?;
    let mut conflicttuple = [ConflictTupleInfo::missing()];
    match newtup {
        Some(newtup) => {
            slot_store_data(mcx, localslot, entry, rel, newtup, &mut infuncs)?;
            report_apply_conflict(
                mcx,
                rel,
                LOG,
                ConflictType::UpdateMissing,
                Some(remoteslot),
                Some(localslot),
                &mut conflicttuple,
            )
        }
        None => report_apply_conflict(
            mcx,
            rel,
            LOG,
            ConflictType::DeleteMissing,
            Some(remoteslot),
            None,
            &mut conflicttuple,
        ),
    }
}

// Unless the subscription opted out, any user-supplied code runs as the table
// owner.
pub(crate) fn maybe_switch_to_table_owner(
    mcx: Mcx<'_>,
    relowner: Oid,
) -> PgResult<Option<types_core::UserContext>> {
    if my_sub(|s| s.runasowner) {
        return Ok(None);
    }
    let mut ucxt = types_core::UserContext::new(InvalidOid, 0, 0);
    init_small::SwitchToUntrustedUser(mcx, relowner, &mut ucxt)?;
    Ok(Some(ucxt))
}

pub(crate) fn restore_user_context(ucxt: &Option<types_core::UserContext>) -> PgResult<()> {
    if let Some(ucxt) = ucxt {
        init_small::RestoreUserContext(ucxt)?;
    }
    Ok(())
}

struct ApplyTrig<'mcx> {
    td: std::rc::Rc<types_trigger::TriggerDesc<'static>>,
    fmgr: trigger::TriggerFmgrCache,
    when: trigger::TriggerWhenCache<'mcx>,
}

fn apply_trig<'mcx>(rel: &Relation<'_>) -> PgResult<Option<ApplyTrig<'mcx>>> {
    if !rel.rd_hastriggers {
        return Ok(None);
    }
    Ok(relcache::RelationGetTriggerDesc(rel.rd_id)?.map(|td| ApplyTrig {
        td,
        fmgr: trigger::TriggerFmgrCache::default(),
        when: trigger::TriggerWhenCache::default(),
    }))
}

fn check_relation_updatable(
    mcx: Mcx<'_>,
    rel: &Relation<'_>,
    entry: &LogicalRepRelMapEntry,
) -> PgResult<()> {
    // For partitioned tables, only the target partition's updatability
    // matters.
    if rel.rd_rel.relkind == types_rel::RELKIND_PARTITIONED_TABLE {
        return Ok(());
    }
    if entry.updatable {
        return Ok(());
    }
    if logicalrelation::get_relation_identity_or_pk(mcx, rel)? != InvalidOid {
        ereport(ERROR)
            .errcode(ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE)
            .errmsg(format!(
                "publisher did not send replica identity column expected by the logical \
                 replication target relation \"{}.{}\"",
                entry.remoterel.nspname, entry.remoterel.relname
            ))
            .finish(loc("check_relation_updatable"))?;
        unreachable!();
    }
    ereport(ERROR)
        .errcode(ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE)
        .errmsg(format!(
            "logical replication target relation \"{}.{}\" has neither REPLICA IDENTITY index \
             nor PRIMARY KEY and published relation does not have REPLICA IDENTITY FULL",
            entry.remoterel.nspname, entry.remoterel.relname
        ))
        .finish(loc("check_relation_updatable"))?;
    unreachable!();
}

fn tuples_equal<'mcx>(
    mcx: Mcx<'mcx>,
    slot1: &mut SlotData<'mcx>,
    slot2: &mut SlotData<'mcx>,
    rel: &Relation<'mcx>,
) -> PgResult<bool> {
    exectuples::slot_getallattrs(slot1);
    exectuples::slot_getallattrs(slot2);

    let natts = rel.rd_att.natts as usize;
    for i in 0..natts {
        let att = rel.rd_att.attr(i);
        if att.attisdropped || att.attgenerated != 0 {
            continue;
        }
        let (n1, v1) = {
            let b = slot1.base();
            (b.tts_isnull[i], b.tts_values[i])
        };
        let (n2, v2) = {
            let b = slot2.base();
            (b.tts_isnull[i], b.tts_values[i])
        };
        if n1 != n2 {
            return Ok(false);
        }
        if n1 {
            continue;
        }
        let typentry =
            typcache::lookup_type_cache(att.atttypid, typcache::TYPECACHE_EQ_OPR_FINFO)?;
        let mut finfo = typentry.eq_opr_finfo();
        if finfo.fn_oid == InvalidOid {
            ereport(ERROR)
                .errcode(ERRCODE_UNDEFINED_FUNCTION)
                .errmsg(format!(
                    "could not identify an equality operator for type {}",
                    format_type::format_type_be(att.atttypid)?
                ))
                .finish(loc("tuples_equal"))?;
            unreachable!();
        }
        let eq = fmgr::fcinfo::function_call2_coll_in(&mut finfo, att.attcollation, mcx, v1, v2)?;
        if !eq.as_bool() {
            return Ok(false);
        }
    }
    Ok(true)
}

// Build one scan key per non-expression index key column, using the index
// opclass's own equality operator, never the type's default equality.
fn build_replindex_scan_key(
    idxrel: &Relation<'_>,
    rel: &Relation<'_>,
    searchslot: &mut SlotData<'_>,
) -> PgResult<Vec<ScanKeyData>> {
    let form = idxrel.rd_index.as_ref().expect("index form");
    let mut keys = Vec::new();
    exectuples::slot_getallattrs(searchslot);
    for (i, &table_attno) in form.indkey.iter().take(form.indnkeyatts as usize).enumerate() {
        if table_attno == 0 {
            // XXX: Currently, we don't support expressions in the scan key.
            continue;
        }
        let att = rel.rd_att.attr((table_attno - 1) as usize);
        let optype = idxrel.rd_opcintype[i];
        let opfamily = idxrel.rd_opfamily[i];
        let eq_strategy = amapi::IndexAmTranslateCompareType(
            lsyscache::COMPARE_EQ,
            idxrel.rd_rel.relam,
            opfamily,
            false,
        )?;
        let operator = lsyscache::get_opfamily_member(opfamily, optype, optype, eq_strategy as i16)?;
        if operator == InvalidOid {
            return Err(Box::new(PgError::error(format!(
                "missing operator {eq_strategy}({optype},{optype}) in opfamily {opfamily}"
            ))));
        }
        let regop = lsyscache::get_opcode(operator)?;
        let mut key = ScanKeyData::empty();
        key.sk_attno = (i + 1) as i16;
        key.sk_strategy = eq_strategy;
        key.sk_func = fmgr_seams::fmgr_info::call(regop)?;
        key.sk_collation = idxrel.rd_indcollation.get(i).copied().unwrap_or(att.attcollation);
        let (isnull, value) = {
            let b = searchslot.base();
            (
                b.tts_isnull[(table_attno - 1) as usize],
                b.tts_values[(table_attno - 1) as usize],
            )
        };
        key.sk_argument = value;
        if isnull {
            key.sk_flags |= SK_ISNULL | SK_SEARCHNULL;
        }
        keys.push(key);
    }
    debug_assert!(!keys.is_empty());
    Ok(keys)
}

fn find_repl_tuple_by_index<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    idxoid: Oid,
    searchslot: &mut SlotData<'mcx>,
    outslot: &mut SlotData<'mcx>,
) -> PgResult<bool> {
    let idxrel = indexam::index_open(mcx, idxoid, types_rel::RowExclusiveLock)?;
    // Only the primary key / replica identity index may skip the
    // per-candidate equality check.
    let is_idx_safe_to_skip_duplicates =
        logicalrelation::get_relation_identity_or_pk(mcx, rel)? == idxoid;

    // An in-progress match surfaces through dirty_xmin/dirty_xmax and is
    // waited out, then the scan is retried.
    let snap = Rc::new(types_snapshot::SnapshotData::sentinel(mcx, types_snapshot::SNAPSHOT_DIRTY));
    let keys = build_replindex_scan_key(&idxrel, rel, searchslot)?;

    let found = loop {
        let mut scan =
            indexam::index_beginscan(mcx, rel, &idxrel, snap.clone(), keys.len() as i32, 0)?;
        let mut kv = mcx::PgVec::new_in(mcx);
        for k in &keys {
            kv.push(k.clone());
        }
        indexam::index_rescan(&mut scan, Some(&kv), None)?;

        let mut found = false;
        let mut xwait = InvalidTransactionId;
        while indexam::index_getnext_slot(
            mcx,
            &mut scan,
            types_scan::sdir::ScanDirection::ForwardScanDirection,
            outslot,
        )? {
            if !is_idx_safe_to_skip_duplicates && !tuples_equal(mcx, outslot, searchslot, rel)? {
                continue;
            }
            // Own the tuple before the scan's pin goes away.
            exectuples::exec_materialize_slot(outslot, mcx)?;
            xwait = dirty_xwait(&snap);
            if xwait != InvalidTransactionId {
                break;
            }
            found = true;
            break;
        }

        if xwait != InvalidTransactionId {
            indexam::index_endscan(scan)?;
            lmgr::XactLockTableWait(xwait, None, None, types_storage::lock::XLTW_Oper::None)?;
            continue;
        }
        if found {
            // Lock the found tuple; on concurrent update/delete retry the scan.
            let tid = outslot.base().tts_tid;
            snapmgr::PushActiveSnapshot(&snapmgr::GetLatestSnapshot()?)?;
            let lockres = tableam::table_tuple_lock_for_repl(mcx, rel, &tid, outslot);
            snapmgr::PopActiveSnapshot()?;
            indexam::index_endscan(scan)?;
            match lockres? {
                LockOutcome::Ok => break true,
                LockOutcome::Retry => continue,
            }
        }
        indexam::index_endscan(scan)?;
        break false;
    };

    indexam::index_close(idxrel, types_rel::NoLock)?;
    Ok(found)
}

fn dirty_xwait(snap: &types_snapshot::SnapshotData<'_>) -> TransactionId {
    let xmin = snap.dirty_xmin.get();
    if xmin != InvalidTransactionId {
        xmin
    } else {
        snap.dirty_xmax.get()
    }
}

#[derive(Debug)]
pub(crate) enum LockOutcome {
    Ok,
    Retry,
}

pub(crate) fn should_refetch_tuple(
    res: tableam_real::TM_Result,
    moved_partitions: bool,
) -> PgResult<LockOutcome> {
    use tableam_real::TM_Result;
    match res {
        TM_Result::TM_Ok => Ok(LockOutcome::Ok),
        TM_Result::TM_Updated => {
            let msg = if moved_partitions {
                "tuple to be locked was already moved to another partition due to concurrent update, retrying"
            } else {
                "concurrent update, retrying"
            };
            ereport(LOG)
                .errcode(ERRCODE_T_R_SERIALIZATION_FAILURE)
                .errmsg(msg)
                .finish(loc("should_refetch_tuple"))?;
            Ok(LockOutcome::Retry)
        }
        TM_Result::TM_Deleted => {
            ereport(LOG)
                .errcode(ERRCODE_T_R_SERIALIZATION_FAILURE)
                .errmsg("concurrent delete, retrying")
                .finish(loc("should_refetch_tuple"))?;
            Ok(LockOutcome::Retry)
        }
        TM_Result::TM_Invisible => {
            ereport(ERROR)
                .errmsg("attempted to lock invisible tuple")
                .finish(loc("should_refetch_tuple"))?;
            unreachable!();
        }
        other => {
            ereport(ERROR)
                .errmsg(format!("unexpected table_tuple_lock status: {}", other as u32))
                .finish(loc("should_refetch_tuple"))?;
            unreachable!();
        }
    }
}

mod tableam {
    use super::*;
    use types_tuple::itemptr::ItemPointerData;

    pub(super) fn table_tuple_lock_for_repl<'mcx>(
        mcx: Mcx<'mcx>,
        rel: &Relation<'mcx>,
        tid: &ItemPointerData,
        slot: &mut SlotData<'mcx>,
    ) -> PgResult<LockOutcome> {
        use tableam_real::{LockTupleMode, LockWaitPolicy, TM_FailureData, TM_Result};
        let mut tmfd = TM_FailureData::default();
        let snap = Some(snapmgr::GetActiveSnapshot());
        let cid = xact::GetCurrentCommandId(false)?;
        let res = tableam_real::table_tuple_lock(
            mcx,
            rel,
            tid,
            &snap,
            slot,
            cid,
            LockTupleMode::LockTupleExclusive,
            LockWaitPolicy::LockWaitBlock,
            0,
            &mut tmfd,
        )?;
        // TM_SelfModified is not reachable through the apply-side lock (the
        // worker never locks its own uncommitted tuple).
        if let TM_Result::TM_SelfModified = res {
            return Ok(LockOutcome::Ok);
        }
        // tmfd.ctid is only filled for TM_Updated (ItemPointerGetOffsetNumber
        // asserts on the zeroed one).
        let moved = matches!(res, TM_Result::TM_Updated)
            && ::types_tuple::ItemPointerIndicatesMovedPartitions(&tmfd.ctid);
        super::should_refetch_tuple(res, moved)
    }
}

fn find_repl_tuple_seq<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    searchslot: &mut SlotData<'mcx>,
    outslot: &mut SlotData<'mcx>,
) -> PgResult<bool> {
    let snap = Rc::new(types_snapshot::SnapshotData::sentinel(mcx, types_snapshot::SNAPSHOT_DIRTY));
    let found = loop {
        let mut scan =
            tableam_real::table_beginscan(mcx, rel, Some(snap.clone()), 0, mcx::PgVec::new_in(mcx))?;
        let mut scanslot = tableam_real::table_slot_create(mcx, rel)?;

        let mut found = false;
        let mut xwait = InvalidTransactionId;
        while tableam_real::table_scan_getnextslot(
            mcx,
            &mut scan,
            types_scan::sdir::ScanDirection::ForwardScanDirection,
            &mut scanslot,
        )? {
            if !tuples_equal(mcx, &mut scanslot, searchslot, rel)? {
                continue;
            }
            // Materialize now, while the datums still point into scanslot's
            // pinned buffer.
            let natts = rel.rd_att.natts as usize;
            exectuples::exec_clear_tuple(outslot, mcx);
            exectuples::slot_getallattrs(&mut scanslot);
            for i in 0..natts {
                let (v, n) = {
                    let b = scanslot.base();
                    (b.tts_values[i], b.tts_isnull[i])
                };
                let base = outslot.base_mut();
                base.tts_values[i] = v;
                base.tts_isnull[i] = n;
            }
            outslot.base_mut().tts_tid = scanslot.base().tts_tid;
            exectuples::exec_store_virtual_tuple(outslot);
            exectuples::exec_materialize_slot(outslot, mcx)?;
            xwait = dirty_xwait(&snap);
            if xwait != InvalidTransactionId {
                break;
            }
            found = true;
            break;
        }
        exectuples::exec_clear_tuple(&mut scanslot, mcx);

        if xwait != InvalidTransactionId {
            tableam_real::table_endscan(scan)?;
            lmgr::XactLockTableWait(xwait, None, None, types_storage::lock::XLTW_Oper::None)?;
            continue;
        }
        if found {
            let tid = outslot.base().tts_tid;
            snapmgr::PushActiveSnapshot(&snapmgr::GetLatestSnapshot()?)?;
            let lockres = tableam::table_tuple_lock_for_repl(mcx, rel, &tid, outslot);
            snapmgr::PopActiveSnapshot()?;
            tableam_real::table_endscan(scan)?;
            match lockres? {
                LockOutcome::Ok => break true,
                LockOutcome::Retry => continue,
            }
        }
        tableam_real::table_endscan(scan)?;
        break false;
    };
    Ok(found)
}


// RLS is unsupported on the apply path, so any relation with RLS enabled is
// refused for every command alike.
fn target_privileges_check<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    mode: u64,
) -> PgResult<()> {
    let relid = rel.rd_id;
    let aclresult = aclchk::pg_class_aclcheck(relid, miscinit::GetUserId(), mode)?;
    if aclresult != aclchk::ACLCHECK_OK {
        let relname = lsyscache::get_rel_name(mcx, relid)?;
        aclchk::aclcheck_error(
            aclresult,
            tablecmds::get_relkind_objtype(rel.rd_rel.relkind),
            relname.as_ref().map(|s| s.as_str()).unwrap_or(""),
        )?;
    }

    if rls::check_enable_rls(relid, InvalidOid, false)? == rls::CheckEnableRls::RlsEnabled {
        let username = miscinit::GetUserNameFromId(mcx, miscinit::GetUserId(), true)?;
        ereport(ERROR)
            .errcode(ERRCODE_FEATURE_NOT_SUPPORTED)
            .errmsg(format!(
                "user \"{}\" cannot replicate into relation with row-level security enabled: \"{}\"",
                username.as_ref().map(|s| s.as_str()).unwrap_or(""),
                rel.name()
            ))
            .finish(loc("target_privileges_check"))?;
    }
    Ok(())
}

fn find_repl_tuple<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    entry: &LogicalRepRelMapEntry,
    searchslot: &mut SlotData<'mcx>,
    outslot: &mut SlotData<'mcx>,
) -> PgResult<bool> {
    target_privileges_check(mcx, rel, types_nodes::parsenodes::ACL_SELECT)?;
    if entry.localindexoid != InvalidOid {
        find_repl_tuple_by_index(mcx, rel, entry.localindexoid, searchslot, outslot)
    } else {
        find_repl_tuple_seq(mcx, rel, searchslot, outslot)
    }
}

fn do_insert<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    root_rel: Option<&Relation<'mcx>>,
    slot: &mut SlotData<'mcx>,
) -> PgResult<()> {
    target_privileges_check(mcx, rel, types_nodes::parsenodes::ACL_INSERT)?;
    execreplication::CheckCmdReplicaIdentity(mcx, rel, types_nodes::nodes_enums::CmdType::CMD_INSERT)?;

    // The indexes are opened BEFORE the row triggers fire so a trigger
    // cannot drop an index under the insert.
    let mut index_state = execindexing::ExecOpenIndices(mcx, rel, false)?;

    let mut trig = apply_trig(rel)?;
    // A NULL return from a BEFORE ROW INSERT trigger suppresses the insert.
    if let Some(t) = trig.as_mut() {
        if t.td.trig_insert_before_row {
            let td = t.td.clone();
            let mut when =
                trigger::TriggerWhenEval { mcx, cache: &mut t.when, modified_cols: None };
            if !trigger::ExecBRInsertTriggers(mcx, rel, &td, &mut t.fmgr, &mut when, slot)? {
                execindexing::ExecCloseIndices(index_state)?;
                return Ok(());
            }
        }
    }

    let mut generated_exprs = None;
    if rel.rd_att.constr.as_deref().is_some_and(|c| c.has_generated_stored) {
        nodemodifytable::exec_compute_stored_generated(mcx, &mut generated_exprs, None, rel, slot)?;
    }
    let mut check_exprs = None;
    let mut nn_exprs = None;
    if rel.rd_att.constr.is_some() {
        nodemodifytable::exec_constraints(mcx, &mut check_exprs, &mut nn_exprs, rel, slot, root_rel, None)?;
    }
    // A row replicated straight into a partition must satisfy its partition
    // constraint.
    if rel.rd_rel.relispartition {
        let mut check_cache = None;
        if !execpartition::exec_partition_check(mcx, &mut check_cache, rel, slot)? {
            return Err(execpartition::partition_constraint_violation(mcx, rel, slot, None, root_rel));
        }
    }

    tableam_real::simple_table_tuple_insert(mcx, rel, slot)?;

    // Conflicts are checked after the insert and reported as insert_exists
    // instead of the plain unique-violation error.
    let conflict_indexes = crate::conflict::init_conflict_indexes(&index_state);
    let mut conflict = false;
    let recheck_indexes = if index_state.num_indices() > 0 {
        let eval_cx = mcx::MemoryContext::new("ApplyIndexEval");
        let r = execindexing::ExecInsertIndexTuples(
            mcx,
            eval_cx.mcx(),
            &mut index_state,
            rel,
            slot,
            None,
            !conflict_indexes.is_empty(),
            Some(&mut conflict),
            &conflict_indexes,
            false,
        )?;
        ::mcx::std_vec_from_slice("replication", &r)?
    } else {
        Vec::new()
    };
    if conflict {
        crate::conflict::check_and_report_conflict(
            mcx,
            rel,
            &mut index_state,
            crate::conflict::ConflictType::InsertExists,
            &recheck_indexes,
            &conflict_indexes,
            None,
            slot,
        )?;
    }
    execindexing::ExecCloseIndices(index_state)?;

    // After-statement triggers are not fired by replication yet.
    if let Some(t) = trig.as_mut() {
        let td = t.td.clone();
        let new_tid = slot.base().tts_tid;
        let mut when = trigger::TriggerWhenEval { mcx, cache: &mut t.when, modified_cols: None };
        trigger::ExecARInsertTriggers(
            mcx,
            rel,
            Some(&td),
            new_tid,
            &recheck_indexes,
            None,
            Some(&mut when),
            None,
        )?;
    }
    Ok(())
}

fn apply_updated_cols<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    entry: &LogicalRepRelMapEntry,
    newtup: &LogicalRepTupleData,
) -> PgResult<types_nodes::Bitmapset<'mcx>> {
    use types_tuple::htup::FirstLowInvalidHeapAttributeNumber;
    let mut cols = types_nodes::Bitmapset::empty();
    for i in 0..rel.rd_att.natts as usize {
        let att = rel.rd_att.attr(i);
        let remote = entry.attrmap.get(i).copied().unwrap_or(-1);
        if att.attisdropped || remote < 0 {
            continue;
        }
        let m = remote as usize;
        tuple_column_check(m, newtup.ncols)?;
        if newtup.colstatus[m] != LOGICALREP_COLUMN_UNCHANGED {
            cols.add_member(mcx, (i as i32 + 1) - FirstLowInvalidHeapAttributeNumber)?;
        }
    }
    Ok(cols)
}

fn do_update<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    root_rel: Option<&Relation<'mcx>>,
    searchslot: &mut SlotData<'mcx>,
    slot: &mut SlotData<'mcx>,
    modified_cols: Option<&types_nodes::Bitmapset<'mcx>>,
) -> PgResult<()> {
    use tableam_vocab::TU_UpdateIndexes;

    target_privileges_check(mcx, rel, types_nodes::parsenodes::ACL_UPDATE)?;
    execreplication::CheckCmdReplicaIdentity(mcx, rel, types_nodes::nodes_enums::CmdType::CMD_UPDATE)?;

    // As in do_insert, the indexes are open while the row triggers fire.
    let mut index_state = execindexing::ExecOpenIndices(mcx, rel, false)?;

    let mut trig = apply_trig(rel)?;
    let mut all_updated_cols = match modified_cols {
        Some(c) => c.clone_in(mcx)?,
        None => types_nodes::Bitmapset::empty(),
    };
    nodemodifytable::add_generated_extra_updated_cols(
        mcx,
        rel,
        trig.as_ref().is_some_and(|t| t.td.trig_update_before_row),
        &mut all_updated_cols,
    )?;
    let all_updated_cols = &all_updated_cols;
    // A NULL return from a BEFORE ROW UPDATE trigger suppresses the update.
    if let Some(t) = trig.as_mut() {
        if t.td.trig_update_before_row {
            let td = t.td.clone();
            let mut when = trigger::TriggerWhenEval {
                mcx,
                cache: &mut t.when,
                modified_cols: Some(all_updated_cols),
            };
            if !trigger::ExecBRUpdateTriggers(
                mcx,
                rel,
                &td,
                &mut t.fmgr,
                &mut when,
                searchslot,
                slot,
                Some(all_updated_cols),
            )? {
                execindexing::ExecCloseIndices(index_state)?;
                exectuples::exec_clear_tuple(searchslot, mcx);
                return Ok(());
            }
        }
    }

    let mut generated_exprs = None;
    if rel.rd_att.constr.as_deref().is_some_and(|c| c.has_generated_stored) {
        // The updatedCols filter the recomputed generated columns unless a
        // BEFORE ROW UPDATE trigger runs.
        let gen_filter = if trig.as_ref().is_some_and(|t| t.td.trig_update_before_row) {
            None
        } else {
            modified_cols
        };
        nodemodifytable::exec_compute_stored_generated(
            mcx,
            &mut generated_exprs,
            gen_filter,
            rel,
            slot,
        )?;
    }
    let mut check_exprs = None;
    let mut nn_exprs = None;
    if rel.rd_att.constr.is_some() {
        nodemodifytable::exec_constraints(mcx, &mut check_exprs, &mut nn_exprs, rel, slot, root_rel, None)?;
    }
    // The updated row must still satisfy the partition's constraint.
    if rel.rd_rel.relispartition {
        let mut check_cache = None;
        if !execpartition::exec_partition_check(mcx, &mut check_cache, rel, slot)? {
            return Err(execpartition::partition_constraint_violation(
                mcx,
                rel,
                slot,
                modified_cols,
                root_rel,
            ));
        }
    }

    let otid = searchslot.base().tts_tid;
    let snap = Some(snapmgr::GetActiveSnapshot());
    let mut update_indexes = TU_UpdateIndexes::TU_None;
    tableam_real::simple_table_tuple_update(mcx, rel, &otid, slot, &snap, &mut update_indexes)?;

    let mut recheck_indexes: Vec<Oid> = Vec::new();
    if !matches!(update_indexes, TU_UpdateIndexes::TU_None) {
        let only_summarizing = matches!(update_indexes, TU_UpdateIndexes::TU_Summarizing);
        let conflict_indexes = crate::conflict::init_conflict_indexes(&index_state);
        let mut conflict = false;
        if index_state.num_indices() > 0 {
            let eval_cx = mcx::MemoryContext::new("ApplyIndexEval");
            let r = execindexing::ExecInsertIndexTuples(
                mcx,
                eval_cx.mcx(),
                &mut index_state,
                rel,
                slot,
                Some(all_updated_cols),
                !conflict_indexes.is_empty(),
                Some(&mut conflict),
                &conflict_indexes,
                only_summarizing,
            )?;
            recheck_indexes = ::mcx::std_vec_from_slice("replication", &r)?;
        }
        if conflict {
            crate::conflict::check_and_report_conflict(
                mcx,
                rel,
                &mut index_state,
                crate::conflict::ConflictType::UpdateExists,
                &recheck_indexes,
                &conflict_indexes,
                Some(searchslot),
                slot,
            )?;
        }
    }
    execindexing::ExecCloseIndices(index_state)?;

    if let Some(t) = trig.as_mut() {
        let td = t.td.clone();
        let new_tid = slot.base().tts_tid;
        let mut when = trigger::TriggerWhenEval {
            mcx,
            cache: &mut t.when,
            modified_cols: Some(all_updated_cols),
        };
        trigger::ExecARUpdateTriggers(
            mcx,
            rel,
            Some(&td),
            None,
            None,
            Some(otid),
            Some(new_tid),
            &recheck_indexes,
            None,
            Some(&mut when),
            false,
            None,
            None,
            Some(all_updated_cols),
        )?;
    }
    // Release the locked tuple's buffer pin now rather than at end of
    // transaction.
    exectuples::exec_clear_tuple(searchslot, mcx);
    Ok(())
}

fn do_delete<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    searchslot: &mut SlotData<'mcx>,
) -> PgResult<()> {
    target_privileges_check(mcx, rel, types_nodes::parsenodes::ACL_DELETE)?;
    execreplication::CheckCmdReplicaIdentity(mcx, rel, types_nodes::nodes_enums::CmdType::CMD_DELETE)?;

    let index_state = execindexing::ExecOpenIndices(mcx, rel, false)?;

    let mut trig = apply_trig(rel)?;
    // A NULL return from a BEFORE ROW DELETE trigger suppresses the delete.
    if let Some(t) = trig.as_mut() {
        if t.td.trig_delete_before_row {
            let td = t.td.clone();
            let mut when =
                trigger::TriggerWhenEval { mcx, cache: &mut t.when, modified_cols: None };
            if !trigger::ExecBRDeleteTriggers(mcx, rel, &td, &mut t.fmgr, &mut when, searchslot)? {
                execindexing::ExecCloseIndices(index_state)?;
                exectuples::exec_clear_tuple(searchslot, mcx);
                return Ok(());
            }
        }
    }

    let tid = searchslot.base().tts_tid;
    let snap = Some(snapmgr::GetActiveSnapshot());
    tableam_real::simple_table_tuple_delete(mcx, rel, &tid, &snap)?;
    execindexing::ExecCloseIndices(index_state)?;

    if let Some(t) = trig.as_mut() {
        let td = t.td.clone();
        let mut when = trigger::TriggerWhenEval { mcx, cache: &mut t.when, modified_cols: None };
        trigger::ExecARDeleteTriggers(
            mcx,
            rel,
            Some(&td),
            tid,
            None,
            Some(&mut when),
            false,
            None,
        )?;
    }
    exectuples::exec_clear_tuple(searchslot, mcx);
    Ok(())
}

enum RoutedOp<'a> {
    Insert,
    Update(&'a LogicalRepTupleData),
    Delete,
}

fn partition_names(mcx: Mcx<'_>, rel: &Relation<'_>) -> PgResult<(String, String)> {
    let nsp = lsyscache::get_namespace_name(mcx, rel.rd_rel.relnamespace)?
        .map(|s| s.to_string())
        .unwrap_or_default();
    let name = String::from_utf8_lossy(rel.rd_rel.relname.name_str()).into_owned();
    Ok((nsp, name))
}

// The apply estate's target ResultRelInfo (create_edata_for_relation):
// its ri_TrigDesc is fixed before any trigger of the change runs.
fn apply_result_rels(rel: &types_rel::Relation<'_>) -> PgResult<[trigger::TrigTargetDesc; 1]> {
    let td = if rel.rd_hastriggers { relcache::RelationGetTriggerDesc(rel.rd_id)? } else { None };
    Ok([(rel.rd_id, td)])
}

fn apply_handle_tuple_routing<'mcx>(
    mcx: Mcx<'mcx>,
    entry: &LogicalRepRelMapEntry,
    rel: &Relation<'mcx>,
    remoteslot: &mut SlotData<'mcx>,
    op: RoutedOp<'_>,
) -> PgResult<()> {
    let mut partition_directory: Option<execpartition::PartitionDirectory<'mcx>> = None;
    let mut proute =
        execpartition::PartitionTupleRouting::new(mcx, rel, &mut partition_directory)?;
    let eval_cx = mcx::MemoryContext::new("ApplyTupleRoutingEval");

    let idx = proute.find_partition(remoteslot, eval_cx.mcx(), &mut partition_directory, None)?;
    let partrel = proute.leaf_rel(idx).alias();
    // The partition set can change, so CREATE/ALTER SUBSCRIPTION-time
    // checks are insufficient.
    let (partnsp, partname) = partition_names(mcx, &partrel)?;
    logicalrelation::check_relkind(partrel.rd_rel.relkind as u8, &partnsp, &partname)?;

    let root_to_leaf = proute.leaf_attrmap(idx);
    let mut remoteslot_part = tableam_real::table_slot_create(mcx, &partrel)?;
    match root_to_leaf {
        Some(map) => exectuples::execute_attr_map_slot(map, remoteslot, &mut remoteslot_part, mcx),
        None => exectuples::exec_copy_slot(&mut remoteslot_part, remoteslot, mcx, mcx)?,
    }

    match op {
        RoutedOp::Insert => do_insert(mcx, &partrel, Some(rel), &mut remoteslot_part),
        RoutedOp::Delete => {
            let part_entry = logicalrelation::logicalrep_partition_open(
                mcx,
                entry,
                &partrel,
                root_to_leaf.as_deref(),
            )?;
            check_relation_updatable(mcx, &partrel, &part_entry)?;

            let mut localslot = tableam_real::table_slot_create(mcx, &partrel)?;
            let found =
                find_repl_tuple(mcx, &partrel, &part_entry, &mut remoteslot_part, &mut localslot)?;
            if found {
                let (localxmin, ctsdata) = get_tuple_transaction_info(&localslot)?;
                if let Some((localts, localorigin)) = ctsdata {
                    if localorigin != origin::replorigin_session_origin() {
                        report_origin_differs(
                            mcx,
                            &partrel,
                            &part_entry,
                            &mut remoteslot_part,
                            &mut localslot,
                            None,
                            localxmin,
                            localorigin,
                            localts,
                        )?;
                    }
                }
                do_delete(mcx, &partrel, &mut localslot)?;
            } else {
                report_row_missing(
                    mcx,
                    &partrel,
                    &part_entry,
                    &mut remoteslot_part,
                    &mut localslot,
                    None,
                )?;
            }
            Ok(())
        }
        RoutedOp::Update(newtup) => {
            let part_entry = logicalrelation::logicalrep_partition_open(
                mcx,
                entry,
                &partrel,
                root_to_leaf.as_deref(),
            )?;
            check_relation_updatable(mcx, &partrel, &part_entry)?;

            let mut localslot = tableam_real::table_slot_create(mcx, &partrel)?;
            let found =
                find_repl_tuple(mcx, &partrel, &part_entry, &mut remoteslot_part, &mut localslot)?;
            if !found {
                report_row_missing(
                    mcx,
                    &partrel,
                    &part_entry,
                    &mut remoteslot_part,
                    &mut localslot,
                    Some(newtup),
                )?;
                return Ok(());
            }
            let (localxmin, ctsdata) = get_tuple_transaction_info(&localslot)?;
            if let Some((localts, localorigin)) = ctsdata {
                if localorigin != origin::replorigin_session_origin() {
                    report_origin_differs(
                        mcx,
                        &partrel,
                        &part_entry,
                        &mut remoteslot_part,
                        &mut localslot,
                        Some(newtup),
                        localxmin,
                        localorigin,
                        localts,
                    )?;
                }
            }

            let mut modfuncs = InFuncs::new(partrel.rd_att.natts as usize)?;
            let mut newslot = tableam_real::table_slot_create(mcx, &partrel)?;
            slot_modify_data(
                mcx, &mut newslot, &mut localslot, &part_entry, &partrel, newtup, &mut modfuncs,
            )?;

            let mut check_cache = None;
            if !partrel.rd_rel.relispartition
                || execpartition::exec_partition_check(mcx, &mut check_cache, &partrel, &mut newslot)?
            {
                let updated = apply_updated_cols(mcx, &partrel, &part_entry, newtup)?;
                do_update(mcx, &partrel, Some(rel), &mut localslot, &mut newslot, Some(&updated))
            } else {
                // Moving the tuple into the new partition requires DELETE from
                // the old partition, re-route via the root, and INSERT into
                // the new one.
                let mut rootslot = tableam_real::table_slot_create(mcx, rel)?;
                match root_to_leaf.as_deref() {
                    Some(_) => {
                        let leaf_to_root =
                            tupdesc::build_attrmap_by_name(mcx, &partrel.rd_att, &rel.rd_att)?;
                        exectuples::execute_attr_map_slot(
                            &leaf_to_root, &mut newslot, &mut rootslot, mcx,
                        );
                    }
                    None => exectuples::exec_copy_slot(&mut rootslot, &mut newslot, mcx, mcx)?,
                }

                let new_idx = proute.find_partition(
                    &mut rootslot,
                    eval_cx.mcx(),
                    &mut partition_directory,
                    None,
                )?;
                let newpartrel = proute.leaf_rel(new_idx).alias();
                let (newnsp, newname) = partition_names(mcx, &newpartrel)?;
                logicalrelation::check_relkind(newpartrel.rd_rel.relkind as u8, &newnsp, &newname)?;

                do_delete(mcx, &partrel, &mut localslot)?;

                let new_map = proute.leaf_attrmap(new_idx);
                let mut newslot_part = tableam_real::table_slot_create(mcx, &newpartrel)?;
                match new_map {
                    Some(map) => exectuples::execute_attr_map_slot(
                        map, &mut rootslot, &mut newslot_part, mcx,
                    ),
                    None => {
                        exectuples::exec_copy_slot(&mut newslot_part, &mut rootslot, mcx, mcx)?
                    }
                }
                do_insert(mcx, &newpartrel, Some(rel), &mut newslot_part)
            }
        }
    }
}

fn apply_handle_insert(mcx: Mcx<'static>, r: &mut Reader<'_>) -> PgResult<()> {
    if !begin_replication_step(mcx)? {
        return Ok(());
    }

    let (relid, newtup) = logicalproto::logicalrep_read_insert(r)?;
    let subid = my_sub(|s| s.oid);
    let (entry, rel) =
        logicalrelation::logicalrep_rel_open(mcx, relid, types_rel::RowExclusiveLock, subid)?;
    if !should_apply_changes_for_rel(&entry)? {
        logicalrelation::logicalrep_rel_close(rel, types_rel::RowExclusiveLock)?;
        return end_replication_step();
    }

    let ucxt = maybe_switch_to_table_owner(mcx, rel.rd_rel.relowner)?;

    set_apply_error_context_rel(Some(&entry.remoterel));

    trigger::AfterTriggerBeginQuery();
    let result_rels = apply_result_rels(&rel)?;

    // Flinfo-owned scratch backs the by-ref datums in the slot, so the
    // input-function cache must outlive do_insert.
    let mut infuncs = InFuncs::new(rel.rd_att.natts as usize)?;
    let mut remoteslot = tableam_real::table_slot_create(mcx, &rel)?;
    slot_store_data(mcx, &mut remoteslot, &entry, &rel, &newtup, &mut infuncs)?;
    // Default datums may live in flinfo-owned scratch until do_insert
    // materializes the tuple.
    let _default_exprs = slot_fill_defaults(mcx, &entry, &rel, &mut remoteslot)?;

    if rel.rd_rel.relkind == types_rel::RELKIND_PARTITIONED_TABLE {
        apply_handle_tuple_routing(mcx, &entry, &rel, &mut remoteslot, RoutedOp::Insert)?;
    } else {
        do_insert(mcx, &rel, None, &mut remoteslot)?;
    }

    trigger::AfterTriggerEndQuery(None, &result_rels)?;

    set_apply_error_context_rel(None);

    restore_user_context(&ucxt)?;

    logicalrelation::logicalrep_rel_close(rel, types_rel::NoLock)?;
    end_replication_step()
}

fn apply_handle_update(mcx: Mcx<'static>, r: &mut Reader<'_>) -> PgResult<()> {
    if !begin_replication_step(mcx)? {
        return Ok(());
    }

    let upd = logicalproto::logicalrep_read_update(r)?;
    let subid = my_sub(|s| s.oid);
    let (entry, rel) =
        logicalrelation::logicalrep_rel_open(mcx, upd.relid, types_rel::RowExclusiveLock, subid)?;
    if !should_apply_changes_for_rel(&entry)? {
        logicalrelation::logicalrep_rel_close(rel, types_rel::RowExclusiveLock)?;
        return end_replication_step();
    }

    set_apply_error_context_rel(Some(&entry.remoterel));

    check_relation_updatable(mcx, &rel, &entry)?;

    let ucxt = maybe_switch_to_table_owner(mcx, rel.rd_rel.relowner)?;

    trigger::AfterTriggerBeginQuery();
    let result_rels = apply_result_rels(&rel)?;

    // updatedCols is built before the search tuple is converted.
    let updated = apply_updated_cols(mcx, &rel, &entry, &upd.newtup)?;

    let mut infuncs = InFuncs::new(rel.rd_att.natts as usize)?;
    let mut remoteslot = tableam_real::table_slot_create(mcx, &rel)?;
    let searchtup = if upd.has_oldtuple {
        upd.oldtup.as_ref().expect("oldtuple present")
    } else {
        &upd.newtup
    };
    slot_store_data(mcx, &mut remoteslot, &entry, &rel, searchtup, &mut infuncs)?;

    if rel.rd_rel.relkind == types_rel::RELKIND_PARTITIONED_TABLE {
        apply_handle_tuple_routing(
            mcx,
            &entry,
            &rel,
            &mut remoteslot,
            RoutedOp::Update(&upd.newtup),
        )?;
        trigger::AfterTriggerEndQuery(None, &result_rels)?;
        set_apply_error_context_rel(None);
        restore_user_context(&ucxt)?;
        logicalrelation::logicalrep_rel_close(rel, types_rel::NoLock)?;
        return end_replication_step();
    }

    let mut localslot = tableam_real::table_slot_create(mcx, &rel)?;
    let found = find_repl_tuple(mcx, &rel, &entry, &mut remoteslot, &mut localslot)?;

    if found {
        let (localxmin, ctsdata) = get_tuple_transaction_info(&localslot)?;
        if let Some((localts, localorigin)) = ctsdata {
            if localorigin != origin::replorigin_session_origin() {
                report_origin_differs(
                    mcx,
                    &rel,
                    &entry,
                    &mut remoteslot,
                    &mut localslot,
                    Some(&upd.newtup),
                    localxmin,
                    localorigin,
                    localts,
                )?;
            }
        }

        // slot_modify_data's writes reuse per-column scratch, which would
        // invalidate remoteslot's datums mid-use otherwise.
        let mut modfuncs = InFuncs::new(rel.rd_att.natts as usize)?;
        let mut newslot = tableam_real::table_slot_create(mcx, &rel)?;
        slot_modify_data(mcx, &mut newslot, &mut localslot, &entry, &rel, &upd.newtup, &mut modfuncs)?;
        do_update(mcx, &rel, None, &mut localslot, &mut newslot, Some(&updated))?;
    } else {
        report_row_missing(mcx, &rel, &entry, &mut remoteslot, &mut localslot, Some(&upd.newtup))?;
    }

    trigger::AfterTriggerEndQuery(None, &result_rels)?;

    set_apply_error_context_rel(None);

    restore_user_context(&ucxt)?;

    logicalrelation::logicalrep_rel_close(rel, types_rel::NoLock)?;
    end_replication_step()
}

fn apply_handle_delete(mcx: Mcx<'static>, r: &mut Reader<'_>) -> PgResult<()> {
    if !begin_replication_step(mcx)? {
        return Ok(());
    }

    let (relid, oldtup) = logicalproto::logicalrep_read_delete(r)?;
    let subid = my_sub(|s| s.oid);
    let (entry, rel) =
        logicalrelation::logicalrep_rel_open(mcx, relid, types_rel::RowExclusiveLock, subid)?;
    if !should_apply_changes_for_rel(&entry)? {
        logicalrelation::logicalrep_rel_close(rel, types_rel::RowExclusiveLock)?;
        return end_replication_step();
    }

    set_apply_error_context_rel(Some(&entry.remoterel));

    check_relation_updatable(mcx, &rel, &entry)?;

    let ucxt = maybe_switch_to_table_owner(mcx, rel.rd_rel.relowner)?;

    trigger::AfterTriggerBeginQuery();
    let result_rels = apply_result_rels(&rel)?;

    let mut infuncs = InFuncs::new(rel.rd_att.natts as usize)?;
    let mut remoteslot = tableam_real::table_slot_create(mcx, &rel)?;
    slot_store_data(mcx, &mut remoteslot, &entry, &rel, &oldtup, &mut infuncs)?;

    if rel.rd_rel.relkind == types_rel::RELKIND_PARTITIONED_TABLE {
        apply_handle_tuple_routing(mcx, &entry, &rel, &mut remoteslot, RoutedOp::Delete)?;
        trigger::AfterTriggerEndQuery(None, &result_rels)?;
        set_apply_error_context_rel(None);
        restore_user_context(&ucxt)?;
        logicalrelation::logicalrep_rel_close(rel, types_rel::NoLock)?;
        return end_replication_step();
    }

    let mut localslot = tableam_real::table_slot_create(mcx, &rel)?;
    let found = find_repl_tuple(mcx, &rel, &entry, &mut remoteslot, &mut localslot)?;

    if found {
        let (localxmin, ctsdata) = get_tuple_transaction_info(&localslot)?;
        if let Some((localts, localorigin)) = ctsdata {
            if localorigin != origin::replorigin_session_origin() {
                report_origin_differs(
                    mcx,
                    &rel,
                    &entry,
                    &mut remoteslot,
                    &mut localslot,
                    None,
                    localxmin,
                    localorigin,
                    localts,
                )?;
            }
        }

        do_delete(mcx, &rel, &mut localslot)?;
    } else {
        report_row_missing(mcx, &rel, &entry, &mut remoteslot, &mut localslot, None)?;
    }

    trigger::AfterTriggerEndQuery(None, &result_rels)?;

    set_apply_error_context_rel(None);

    restore_user_context(&ucxt)?;

    logicalrelation::logicalrep_rel_close(rel, types_rel::NoLock)?;
    end_replication_step()
}

// Even if the publisher used CASCADE, replay runs without further cascading
// (DROP_RESTRICT).
fn apply_handle_truncate(mcx: Mcx<'static>, r: &mut Reader<'_>) -> PgResult<()> {
    if !begin_replication_step(mcx)? {
        return Ok(());
    }

    let (remote_relids, _cascade, restart_seqs) = logicalproto::logicalrep_read_truncate(r)?;
    let subid = my_sub(|s| s.oid);

    let mut rels: Vec<Relation<'static>> = Vec::new();
    let mut relids: Vec<Oid> = Vec::new();
    let mut relids_logged: Vec<Oid> = Vec::new();

    for relid in remote_relids {
        let (entry, rel) = logicalrelation::logicalrep_rel_open(
            mcx,
            relid,
            types_rel::AccessExclusiveLock,
            subid,
        )?;
        if !should_apply_changes_for_rel(&entry)? {
            logicalrelation::logicalrep_rel_close(rel, types_rel::AccessExclusiveLock)?;
            continue;
        }
        target_privileges_check(mcx, &rel, types_nodes::parsenodes::ACL_TRUNCATE)?;
        if heapam::relation_is_logically_logged(&rel) {
            relids_logged.push(rel.rd_id);
        }
        relids.push(rel.rd_id);
        let is_partitioned = rel.rd_rel.relkind == types_rel::RELKIND_PARTITIONED_TABLE;
        let parentid = rel.rd_id;
        rels.push(rel);

        if is_partitioned {
            let children =
                pg_inherits::find_all_inheritors(mcx, parentid, types_rel::AccessExclusiveLock)?;
            for &childrelid in children.iter() {
                if relids.contains(&childrelid) {
                    continue;
                }
                // find_all_inheritors already got the lock.
                let childrel = table::table_open(mcx, childrelid, types_rel::NoLock)?;
                if childrel.is_other_temp() {
                    table::table_close(childrel, types_rel::AccessExclusiveLock)?;
                    continue;
                }
                target_privileges_check(mcx, &childrel, types_nodes::parsenodes::ACL_TRUNCATE)?;
                if heapam::relation_is_logically_logged(&childrel) {
                    relids_logged.push(childrelid);
                }
                relids.push(childrelid);
                rels.push(childrel);
            }
        }
    }

    if !rels.is_empty() {
        tablecmds::ExecuteTruncateGuts(
            mcx,
            &mut rels,
            &mut relids,
            &mut relids_logged,
            types_nodes::parsenodes::DropBehavior::DROP_RESTRICT,
            restart_seqs,
            // The last argument tells truncate whether to switch to the table
            // owner — exactly opposite to running as the subscription owner.
            !my_sub(|s| s.runasowner),
        )?;
    }

    for rel in rels {
        logicalrelation::logicalrep_rel_close(rel, types_rel::NoLock)?;
    }

    end_replication_step()
}

#[cfg(test)]
mod tests {
    // Under a UTF-8 database, invalid multibyte sequences must produce a
    // catchable error rather than flow through to from_utf8_unchecked sinks.
    #[test]
    fn text_column_bytes_are_encoding_verified() {
        use wchar::PG_UTF8;

        assert!(mbutils::pg_verify_mbstr(PG_UTF8, b"hello", false).unwrap());
        assert!(mbutils::pg_verify_mbstr(PG_UTF8, "ol\u{00e9}".as_bytes(), false).unwrap());

        let err = mbutils::pg_verify_mbstr(PG_UTF8, b"bad\xff", false)
            .err()
            .unwrap();
        assert_eq!(
            err.sqlstate(),
            types_error::ERRCODE_CHARACTER_NOT_IN_REPERTOIRE
        );
        let err = mbutils::pg_verify_mbstr(PG_UTF8, b"bad\xe2\x82", false)
            .err()
            .unwrap();
        assert_eq!(
            err.sqlstate(),
            types_error::ERRCODE_CHARACTER_NOT_IN_REPERTOIRE
        );
    }
}
