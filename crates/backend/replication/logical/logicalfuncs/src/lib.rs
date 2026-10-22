#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]

use datum::Datum;
use elog::{elog, ereport};
use funcapi::{InitMaterializedSRF, MaterializedSRF};
use logical::{
    CheckLogicalDecodingRequirements, CreateDecodingContext, LogicalConfirmReceivedLocation,
    OutputPluginContext, OutputPluginOutputType, PluginOption,
};
use logical_decode::LogicalDecodingProcessRecord;
use mcx::Mcx;
use slot::{
    CheckSlotPermissions, ReplicationSlotAcquire, ReplicationSlotMarkDirty,
    ReplicationSlotRelease, WaitForStandbyConfirmation,
};
use types_core::{Oid, TransactionId, XLogRecPtr, TEXTOID};
use types_error::{
    ErrorLocation, PgResult, ERRCODE_FEATURE_NOT_SUPPORTED, ERRCODE_NULL_VALUE_NOT_ALLOWED, ERROR,
};
use types_fmgr::{varlena_result, FmgrBuiltin, FmgrInfo, FunctionCallInfoBaseData as Fcinfo,
    PGFunction};
use xlogreader::LocalPageRead;

const InvalidXLogRecPtr: XLogRecPtr = 0;
const MaxAllocSize: usize = 0x3fffffff;
const VARHDRSZ: usize = 4;

#[track_caller]
fn loc(func: &'static str) -> ErrorLocation {
    // pgrust is Rust: report where in OUR source this was raised.
    // #[track_caller] resolves to the call site, not this helper.
    let site = core::panic::Location::caller();
    ErrorLocation::new(site.file(), site.line() as i32, func)
}

// CHECK_FOR_INTERRUPTS (miscadmin.h), the heapam pattern: cheap pending
// check inline, the ereport-raising slow path out of line.
#[cold]
#[inline(never)]
fn process_interrupts() -> PgResult<()> {
    postgres_seams::check_for_interrupts::call()
}

#[inline(always)]
fn check_for_interrupts() -> PgResult<()> {
    if init_small::globals::InterruptPending() {
        return process_interrupts();
    }
    Ok(())
}

struct DecodingOutputState {
    srf: *mut MaterializedSRF<'static>,
    mcx: Mcx<'static>,
    write_context: mcx::MemoryContext,
    // C-parity DecodingOutputState field; read when binary output-plugin options wire.
    #[allow(dead_code)]
    binary_output: bool,
    returned_rows: i64,
}

fn output_state(opc: &OutputPluginContext) -> &'static mut DecodingOutputState {
    debug_assert!(opc.output_writer_private != 0);
    // SAFETY: set by pg_logical_slot_get_changes_guts to a live
    // DecodingOutputState that outlives the decode loop.
    unsafe { &mut *(opc.output_writer_private as *mut DecodingOutputState) }
}

fn LogicalOutputPrepareWrite(
    opc: &mut OutputPluginContext,
    _lsn: XLogRecPtr,
    _xid: TransactionId,
    _last_write: bool,
) -> PgResult<()> {
    opc.out.clear();
    Ok(())
}

fn LogicalOutputWrite(
    opc: &mut OutputPluginContext,
    lsn: XLogRecPtr,
    xid: TransactionId,
    _last_write: bool,
) -> PgResult<()> {
    if opc.out.len() > MaxAllocSize - VARHDRSZ {
        return elog(ERROR, "too much output for sql interface");
    }

    let p = output_state(opc);

    p.write_row(opc.out.as_bytes(), lsn, xid)
}

impl DecodingOutputState {
    fn write_row(&mut self, output: &[u8], lsn: XLogRecPtr, xid: TransactionId) -> PgResult<()> {
        let values = [
            Datum::from_u64(lsn),
            Datum::from_transaction_id(xid),
            varlena_result(varlena::cstring_to_text(self.write_context.mcx(), output)?),
        ];
        // SAFETY: srf points to the live MaterializedSRF owned by the guts frame.
        let result = unsafe { (*self.srf).putvalues(&values, &[false; 3]) };
        self.write_context.reset();
        result?;
        self.returned_rows += 1;
        Ok(())
    }
}

fn detoast_datum<'mcx>(mcx: Mcx<'mcx>, d: Datum) -> PgResult<mcx::PgVec<'mcx, u8>> {
    let ptr = d.as_usize() as *const u8;
    // SAFETY: a live varlena readable through its full VARSIZE_ANY.
    let raw = unsafe { core::slice::from_raw_parts(ptr, types_tuple::varatt::varsize_any(ptr)) };
    detoast::detoast_attr(mcx, raw)
}

// C: TextDatumGetCString — the element datum is a full varlena (header
// included); open_image strips the 1B/4B header or detoasts, yielding the
// payload alone. Routing the full detoast_attr image into text_to_cstring
// (which takes a post-VARDATA_ANY payload) prepended the 4-byte header to
// every option name/value ('option "@\0\0\0include-xids" is unknown').
fn text_datum_to_string(mcx: Mcx<'_>, d: Datum) -> PgResult<String> {
    let ptr = d.as_usize() as *const u8;
    // SAFETY: a live varlena readable through its full VARSIZE_ANY.
    let raw = unsafe { core::slice::from_raw_parts(ptr, types_tuple::varatt::varsize_any(ptr)) };
    let payload = varlena::open_image(mcx, raw)?;
    Ok(String::from_utf8_lossy(payload.as_bytes()).into_owned())
}

fn pg_logical_slot_get_changes_guts(
    flinfo: &mut FmgrInfo,
    fcinfo: &mut Fcinfo,
    confirm: bool,
    binary: bool,
) -> PgResult<Datum> {
    // C captures CurrentResourceOwner at entry (logicalfuncs.c old_resowner):
    // decoding replays each transaction inside an internal subtransaction
    // (reorderbuffer.c BeginInternalSubTransaction/Rollback...), whose exit
    // resets CurrentResourceOwner to the transaction owner. A caller that
    // runs us under a portal-owned resowner (CTAS, INSERT ... SELECT) then
    // unregisters its executor snapshot against the wrong owner at
    // ExecutorEnd ("snapshot reference ... is not owned by resource owner
    // TopTransaction"). decode_loop restores this value after the loop.
    let old_resowner = resowner_seams::current_resource_owner::call();

    CheckSlotPermissions()?;
    CheckLogicalDecodingRequirements()?;

    if fcinfo.argisnull(0) {
        ereport(ERROR)
            .errcode(ERRCODE_NULL_VALUE_NOT_ALLOWED)
            .errmsg("slot name must not be null")
            .finish(loc("pg_logical_slot_get_changes_guts"))?;
        unreachable!();
    }
    // SAFETY: arg 0 is a non-null name datum.
    let name_bytes = unsafe { *fcinfo.arg_name(0) };
    let name = types_tuple::NameData { data: name_bytes };
    let name = String::from_utf8_lossy(name.name_str()).into_owned();

    let upto_lsn: XLogRecPtr = if fcinfo.argisnull(1) {
        InvalidXLogRecPtr
    } else {
        fcinfo.arg_i64(1) as XLogRecPtr
    };
    let upto_nchanges: i32 = if fcinfo.argisnull(2) {
        0
    } else {
        fcinfo.arg_i32(2)
    };
    if fcinfo.argisnull(3) {
        ereport(ERROR)
            .errcode(ERRCODE_NULL_VALUE_NOT_ALLOWED)
            .errmsg("options array must not be null")
            .finish(loc("pg_logical_slot_get_changes_guts"))?;
        unreachable!();
    }

    // SAFETY: caller arms result_mcx for the query lifetime.
    let mcx: Mcx<'static> = unsafe { fcinfo.result_mcx_detached() };

    let arr_img = detoast_datum(mcx, fcinfo.arg(3))?;
    let (ndim, _, _) = arrayfuncs::read_dims_lbounds(&arr_img);
    let mut options: Vec<PluginOption> = Vec::new();
    if ndim > 1 {
        ereport(ERROR)
            .errcode(ERRCODE_FEATURE_NOT_SUPPORTED)
            .errmsg("array must be one-dimensional")
            .finish(loc("pg_logical_slot_get_changes_guts"))?;
        unreachable!();
    } else if arrayfuncs::array_contains_nulls(&arr_img)? {
        ereport(ERROR)
            .errcode(ERRCODE_FEATURE_NOT_SUPPORTED)
            .errmsg("array must not contain nulls")
            .finish(loc("pg_logical_slot_get_changes_guts"))?;
        unreachable!();
    } else if ndim == 1 {
        let (elems, _nulls) = arrayfuncs::deconstruct_array_builtin(mcx, &arr_img, TEXTOID, false)?;
        if elems.len() % 2 != 0 {
            ereport(ERROR)
                .errcode(ERRCODE_FEATURE_NOT_SUPPORTED)
                .errmsg("array must have even number of elements")
                .finish(loc("pg_logical_slot_get_changes_guts"))?;
        unreachable!();
        }
        let mut i = 0;
        while i < elems.len() {
            let optname = text_datum_to_string(mcx, elems[i])?;
            let opt = text_datum_to_string(mcx, elems[i + 1])?;
            options.push((optname, Some(opt)));
            i += 2;
        }
    }

    let mut srf = InitMaterializedSRF(mcx, flinfo, fcinfo, 0)?;

    // Compute the current end-of-wal (logicalfuncs.c): flush position on a
    // primary, replay position in recovery (logical decoding on standby).
    let end_of_wal = if transam_xlog::RecoveryInProgress() {
        xlogrecovery_seams::get_xlog_replay_rec_ptr::call().0
    } else {
        transam_xlog::write::GetFlushRecPtr(None)
    };

    ReplicationSlotAcquire(&name, true, true)?;

    let mut p = DecodingOutputState {
        srf: &mut srf as *mut MaterializedSRF<'_> as *mut MaterializedSRF<'static>,
        mcx,
        write_context: mcx::MemoryContext::new_wholesale("LogicalOutputWrite"),
        binary_output: binary,
        returned_rows: 0,
    };

    let result = decode_loop(
        flinfo,
        &mut p,
        options,
        upto_lsn,
        upto_nchanges,
        end_of_wal,
        confirm,
        binary,
        old_resowner,
    );

    match result {
        Ok(()) => {
            inval::local::InvalidateSystemCaches()?;
            Ok(srf.finish(fcinfo))
        }
        Err(e) => {
            // logicalfuncs.c:315 PG_CATCH: the slot stays acquired; only the
            // top-level error path releases it (a caught error leaves it
            // active until the next acquire or error).
            let _ = inval::local::InvalidateSystemCaches();
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_loop(
    flinfo: &mut FmgrInfo,
    p: &mut DecodingOutputState,
    options: Vec<PluginOption>,
    upto_lsn: XLogRecPtr,
    upto_nchanges: i32,
    end_of_wal: XLogRecPtr,
    confirm: bool,
    binary: bool,
    old_resowner: types_resowner::ResourceOwner,
) -> PgResult<()> {
    let mut ctx = CreateDecodingContext(
        InvalidXLogRecPtr,
        options,
        false,
        Some(LogicalOutputPrepareWrite),
        Some(LogicalOutputWrite),
        None,
    )?;

    if !binary && ctx.opc().options.output_type != OutputPluginOutputType::Textual {
        let plugin = String::from_utf8_lossy(unsafe { ctx.slot.data.get() }.plugin.name_str()).into_owned();
        let procname = adt_regproc::format_procedure(p.mcx, flinfo.fn_oid)?;
        ereport(ERROR)
            .errcode(ERRCODE_FEATURE_NOT_SUPPORTED)
            .errmsg(format!(
                "logical decoding output plugin \"{plugin}\" produces binary output, but function \"{procname}\" expects textual data"
            ))
            .finish(loc("pg_logical_slot_get_changes_guts"))?;
        unreachable!();
    }

    let wait_for_wal_lsn = if upto_lsn == InvalidXLogRecPtr {
        end_of_wal
    } else {
        upto_lsn.min(end_of_wal)
    };
    WaitForStandbyConfirmation(wait_for_wal_lsn)?;

    ctx.opc().output_writer_private = p as *mut DecodingOutputState as usize;

    let restart_lsn = unsafe { ctx.slot.data.get() }.restart_lsn;
    ctx.reader.XLogBeginRead(restart_lsn);

    inval::local::InvalidateSystemCaches()?;

    let mut routine = LocalPageRead { wait_for_wal: true };
    while ctx.reader.v.EndRecPtr < end_of_wal {
        let record = ctx.reader.XLogReadRecord(&mut routine)?;
        if record.is_none() {
            let msg = ctx.reader.errormsg().unwrap_or_default().to_string();
            return elog(
                ERROR,
                format!("could not find record for logical decoding: {msg}"),
            );
        }

        LogicalDecodingProcessRecord(&mut ctx)?;

        if upto_lsn != InvalidXLogRecPtr && upto_lsn <= ctx.reader.v.EndRecPtr {
            break;
        }
        if upto_nchanges != 0 && upto_nchanges as i64 <= p.returned_rows {
            break;
        }

        // logicalfuncs.c:275: CFI once per decoded record, so a large WAL
        // backlog stays cancellable (xlogutils' CFI only fires in the
        // caught-up wait arm, never while records are available).
        check_for_interrupts()?;
    }

    // Logical decoding could have clobbered CurrentResourceOwner during
    // transaction management, so restore the executor's value
    // (logicalfuncs.c: "This is a kluge, but it's not worth cleaning up
    // right now"). Same restore as LogicalSlotAdvanceAndCheckSnapState.
    resowner_seams::set_current_resource_owner::call(old_resowner);

    if ctx.reader.v.EndRecPtr != InvalidXLogRecPtr && confirm {
        LogicalConfirmReceivedLocation(ctx.reader.v.EndRecPtr)?;
        ReplicationSlotMarkDirty();
    }

    ctx.free()?;
    ReplicationSlotRelease()?;
    Ok(())
}

pub fn fc_pg_logical_slot_get_changes(
    flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    let flinfo = flinfo.expect("pg_logical_slot_get_changes requires flinfo");
    pg_logical_slot_get_changes_guts(flinfo, fcinfo, true, false)
}

pub fn fc_pg_logical_slot_peek_changes(
    flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    let flinfo = flinfo.expect("pg_logical_slot_peek_changes requires flinfo");
    pg_logical_slot_get_changes_guts(flinfo, fcinfo, false, false)
}

pub fn fc_pg_logical_slot_get_binary_changes(
    flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    let flinfo = flinfo.expect("pg_logical_slot_get_binary_changes requires flinfo");
    pg_logical_slot_get_changes_guts(flinfo, fcinfo, true, true)
}

pub fn fc_pg_logical_slot_peek_binary_changes(
    flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    let flinfo = flinfo.expect("pg_logical_slot_peek_binary_changes requires flinfo");
    pg_logical_slot_get_changes_guts(flinfo, fcinfo, false, true)
}

// pg_logical_emit_message_bytea/_text (logicalfuncs.c): text and bytea are
// binary compatible, so both OIDs share one body — text_to_cstring's
// strlen() truncation on an embedded NUL in prefix is reproduced by slicing
// at the first zero byte before handing prefix to LogLogicalMessage.
fn pg_logical_emit_message_guts(fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let transactional = fcinfo.arg_bool(0);
    // SAFETY: caller arms result_mcx for the query lifetime.
    let mcx: Mcx<'static> = unsafe { fcinfo.result_mcx_detached() };

    let prefix_img = detoast_datum(mcx, fcinfo.arg(1))?;
    let prefix_payload = &prefix_img[VARHDRSZ..];
    let prefix_len = prefix_payload
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(prefix_payload.len());
    let prefix = &prefix_payload[..prefix_len];

    let data_img = detoast_datum(mcx, fcinfo.arg(2))?;
    let message = &data_img[VARHDRSZ..];

    let flush = fcinfo.arg_bool(3);

    let lsn = logical_message::LogLogicalMessage(prefix, message, transactional, flush)?;
    Ok(Datum::from_u64(lsn))
}

pub fn fc_pg_logical_emit_message_bytea(
    _flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    pg_logical_emit_message_guts(fcinfo)
}

pub fn fc_pg_logical_emit_message_text(
    _flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    pg_logical_emit_message_guts(fcinfo)
}

const fn b(foid: Oid, name: &'static str, func: PGFunction) -> FmgrBuiltin {
    FmgrBuiltin {
        foid,
        name,
        nargs: 4,
        strict: false,
        retset: true,
        func,
    }
}

const fn be(foid: Oid, name: &'static str, func: PGFunction) -> FmgrBuiltin {
    FmgrBuiltin {
        foid,
        name,
        nargs: 4,
        strict: true,
        retset: false,
        func,
    }
}

pub const LOGICALFUNCS_BUILTINS: &[FmgrBuiltin] = &[
    be(3577, "pg_logical_emit_message_text", fc_pg_logical_emit_message_text),
    be(3578, "pg_logical_emit_message_bytea", fc_pg_logical_emit_message_bytea),
    b(3782, "pg_logical_slot_get_changes", fc_pg_logical_slot_get_changes),
    b(
        3783,
        "pg_logical_slot_get_binary_changes",
        fc_pg_logical_slot_get_binary_changes,
    ),
    b(3784, "pg_logical_slot_peek_changes", fc_pg_logical_slot_peek_changes),
    b(
        3785,
        "pg_logical_slot_peek_binary_changes",
        fc_pg_logical_slot_peek_binary_changes,
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_rows_release_temporary_datums() {
        let ctx = mcx::MemoryContext::new("output test");
        let mut desc = tupdesc::CreateTemplateTupleDesc(ctx.mcx(), 3).unwrap();
        for (i, oid) in [types_core::INT8OID, types_core::INT4OID, TEXTOID]
            .into_iter()
            .enumerate()
        {
            tupdesc::TupleDescInitBuiltinEntry(&mut desc, (i + 1) as i16, "v", oid, -1, 0).unwrap();
        }
        let mut rsinfo = types_fmgr::ReturnSetInfo::new(types_fmgr::SFRM_Materialize);
        rsinfo.expectedDesc = Some(core::ptr::NonNull::from(&desc).cast());
        let mut fcinfo = types_fmgr::LocalFcinfo::<0>::new(0);
        fcinfo.resultinfo = rsinfo.as_fmnode_ptr();
        let mut flinfo = FmgrInfo::unresolved();
        let mut srf = InitMaterializedSRF(
            ctx.mcx(),
            &mut flinfo,
            &mut fcinfo,
            funcapi::MAT_SRF_USE_EXPECTED_DESC,
        )
        .unwrap();
        let mut state = DecodingOutputState {
            srf: &mut srf as *mut MaterializedSRF<'_> as *mut MaterializedSRF<'static>,
            // SAFETY: state is used only while ctx is alive.
            mcx: unsafe { core::mem::transmute::<Mcx<'_>, Mcx<'static>>(ctx.mcx()) },
            write_context: mcx::MemoryContext::new_wholesale("LogicalOutputWrite"),
            binary_output: false,
            returned_rows: 0,
        };
        let baseline = ctx.stats().used;
        for _ in 0..256 {
            state.write_row(&[b'x'; 8192], 42, 7).unwrap();
        }
        assert_eq!(state.returned_rows, 256);
        assert_eq!(ctx.stats().used, baseline);
        assert_eq!(state.write_context.stats().used, 0);
        srf.finish(&mut fcinfo);
        let store = rsinfo
            .setResult
            .take()
            .unwrap()
            .downcast::<tuplestore::Tuplestore>()
            .unwrap();
        assert_eq!(store.tuple_count(), 256);
    }

    // The option-array elements arrive as full varlenas; the payload must
    // exclude the header (006_logical_decoding: 'option "@\0\0\0include-xids"
    // is unknown' when the 4B header leaked into the option name).
    #[test]
    fn text_datum_payload_excludes_varlena_header() {
        let ctx = mcx::MemoryContext::new("t");
        let mcx = ctx.mcx();
        // 4B-header form, as deconstruct_array yields for "include-xids".
        let t = varlena::cstring_to_text(mcx, b"include-xids").unwrap();
        let d = Datum::from_usize(t.as_bytes().as_ptr() as usize);
        assert_eq!(text_datum_to_string(mcx, d).unwrap(), "include-xids");
        // 1B short form for a 1-byte value like "0".
        let short = [((2usize << 1) | 1) as u8, b'0'];
        let d = Datum::from_usize(short.as_ptr() as usize);
        assert_eq!(text_datum_to_string(mcx, d).unwrap(), "0");
    }
}
