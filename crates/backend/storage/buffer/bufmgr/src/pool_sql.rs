//! SQL surface of the resizable pool: the procedure `CALL
//! pgrust_buffer_pool_resize(target_bytes bigint, timeout_ms int)`, whose
//! outcome the `pgrust_buffer_pool` view reports (active and target bytes;
//! shortfall = active - target), and `pgrust_buffer_pool_stats()` behind that
//! view.
//! Dispatch rows on the reserved-oid extra-builtin path; the SQL objects are
//! created by scripts/bufpool-views.sql (no catalog delta by default).

use datum::Datum;
use types_core::Oid;
use types_error::{
    PgError, PgResult, ERRCODE_INSUFFICIENT_PRIVILEGE, ERRCODE_INVALID_PARAMETER_VALUE,
    ERRCODE_INVALID_TRANSACTION_STATE, ERRCODE_WRONG_OBJECT_TYPE,
};
use types_fmgr::{FmgrBuiltin, FmgrInfo, FunctionCallInfoBaseData as Fcinfo};

pub const PGRUST_BUFFER_POOL_RESIZE_FOID: Oid = 9020;
pub const PGRUST_BUFFER_POOL_STATS_FOID: Oid = 9021;

fn superuser_check(func: &str) -> PgResult<()> {
    if !superuser_seams::superuser::call()? {
        return Err(Box::new(
            PgError::error(format!("must be superuser to use {func}()"))
                .with_sqlstate(ERRCODE_INSUFFICIENT_PRIVILEGE),
        ));
    }
    Ok(())
}

fn composite_result(
    fcinfo: &mut Fcinfo,
    flinfo: Option<&mut FmgrInfo>,
    what: &str,
    values: &[Datum],
    nulls: &[bool],
) -> PgResult<Datum> {
    let flinfo = flinfo.unwrap_or_else(|| panic!("{what}: resolved FmgrInfo required"));
    let expected_desc = fcinfo.rsinfo_mut().and_then(|rsi| rsi.expectedDesc);
    // SAFETY: the executor's expectedDesc remains live for this call.
    let expected = expected_desc.map(|p| unsafe { p.cast().as_ref() });
    let mcx = fcinfo.result_mcx();
    let resolved = funcapi::get_call_result_type(mcx, flinfo, expected)?;
    if resolved.class != funcapi::TypeFuncClass::Composite {
        return Err(Box::new(PgError::error("return type must be a row type")));
    }
    let tupdesc = resolved
        .result_tuple_desc
        .expect("composite result carries a tupdesc");
    if tupdesc.natts as usize != values.len() {
        return Err(Box::new(
            PgError::error(format!("{what}: expected {} result columns", values.len()))
                .with_sqlstate(types_error::ERRCODE_DATATYPE_MISMATCH),
        ));
    }
    let tup = heaptuple::heap_form_tuple(mcx, &tupdesc, values, nulls)?;
    let d = Datum::from_usize(tup.header_ptr() as usize);
    core::mem::forget(tup);
    Ok(d)
}

/// A shrink drains for seconds (pgrust.buffer_pool_drain_rate buffers per
/// second). A caller that holds a snapshot meanwhile holds back the xmin
/// horizon for the whole drain: HOT pruning stops everywhere, and the hottest
/// tables bloat (an 18 s drain under TPC-B grew pgbench_branches from 2 to
/// 926 pages). So, like pg_wal_replay_wait() in PostgreSQL 17, the resize is a
/// procedure without output arguments: CALL runs it under a portal snapshot,
/// which this pops (the portal tolerates that), and the catalog snapshot is
/// dropped. A procedure with output arguments would not do: its portal
/// registers a hold snapshot for the result row. A snapshot still registered
/// afterwards (a REPEATABLE READ or SERIALIZABLE transaction, an open cursor,
/// an enclosing function) or an assigned transaction id would keep the
/// horizon back, so the call is refused before any work.
fn release_snapshot_for_drain(fcinfo: &Fcinfo) -> PgResult<()> {
    // SAFETY: fcinfo.context is either this CALL's CallContext node or not one.
    if unsafe { fcinfo.call_context() }.is_none() {
        return Err(Box::new(
            PgError::error("pgrust_buffer_pool_resize() must be invoked with CALL")
                .with_sqlstate(ERRCODE_WRONG_OBJECT_TYPE)
                .with_hint("CALL pgrust_buffer_pool_resize(target_bytes, timeout_ms)".to_string()),
        ));
    }
    if snapmgr_portal_seams::active_snapshot_set::call() {
        snapmgr_portal_seams::pop_active_snapshot::call()?;
    }
    snapmgr_seams::invalidate_catalog_snapshot::call();
    let holds_snapshot = snapmgr_portal_seams::have_registered_or_active_snapshot::call();
    let holds_xid = xact_seams::get_top_transaction_id_if_any::call()
        != types_core::InvalidTransactionId;
    if holds_snapshot || holds_xid {
        return Err(Box::new(
            PgError::error(if holds_xid {
                "pgrust_buffer_pool_resize() must not run in a transaction that has an xid"
            } else {
                "pgrust_buffer_pool_resize() must be called without an active or registered snapshot"
            })
            .with_sqlstate(ERRCODE_INVALID_TRANSACTION_STATE)
            .with_detail(
                "A drain holding back the xmin horizon stops HOT pruning for its whole duration."
                    .to_string(),
            )
            .with_hint(
                "CALL it outside a transaction block, or in a READ COMMITTED one that has not \
                 written, and not from a function."
                    .to_string(),
            ),
        ));
    }
    Ok(())
}

pub fn fc_pgrust_buffer_pool_resize(
    _flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    superuser_check("pgrust_buffer_pool_resize")?;
    if fcinfo.argisnull(0) || fcinfo.argisnull(1) {
        return Err(Box::new(
            PgError::error("target_bytes and timeout_ms must not be null")
                .with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE),
        ));
    }
    let target_bytes = fcinfo.arg(0).as_i64();
    let timeout_ms = fcinfo.arg(1).as_i32();
    release_snapshot_for_drain(fcinfo)?;
    crate::resize::BufferPoolResize(target_bytes, timeout_ms)?;
    fcinfo.isnull = true;
    Ok(Datum::null())
}

pub fn fc_pgrust_buffer_pool_stats(
    flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    let s = crate::resize::BufferPoolStats();
    let page = types_core::BLCKSZ as f64;
    let wss = |secs: u32| Datum::from_i64((crate::wss::estimate_pages(secs) * page) as i64);
    let values = [
        Datum::from_i64(s.max_bytes),
        Datum::from_i64(s.boot_bytes),
        Datum::from_i64(s.active_bytes),
        Datum::from_i64(s.target_bytes),
        Datum::from_i64(s.resident_bytes),
        Datum::from_i64(s.draining_extents),
        Datum::from_i64(s.retired_extents),
        Datum::from_i64(s.relocated),
        Datum::from_i64(s.written),
        Datum::from_i64(s.dropped),
        wss(60),
        wss(300),
        wss(900),
        Datum::from_f64(crate::wss::stall_seconds()),
        Datum::from_bool(s.resizing),
    ];
    composite_result(fcinfo, flinfo, "pgrust_buffer_pool_stats", &values, &[false; 15])
}

pub static BUFPOOL_BUILTINS: &[FmgrBuiltin] = &[
    FmgrBuiltin {
        foid: PGRUST_BUFFER_POOL_RESIZE_FOID,
        name: "pgrust_buffer_pool_resize",
        nargs: 2,
        strict: false,
        retset: false,
        func: fc_pgrust_buffer_pool_resize,
    },
    FmgrBuiltin {
        foid: PGRUST_BUFFER_POOL_STATS_FOID,
        name: "pgrust_buffer_pool_stats",
        nargs: 0,
        strict: false,
        retset: false,
        func: fc_pgrust_buffer_pool_stats,
    },
];
