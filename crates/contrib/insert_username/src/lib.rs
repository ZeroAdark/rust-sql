//! `contrib/spi/insert_username` — BEFORE INSERT/UPDATE row trigger that
//! stores the current user name into the named text column.

use datum::Datum;
use types_core::TEXTOID;
use types_error::{PgError, PgResult, ERRCODE_TRIGGERED_ACTION_EXCEPTION};
use types_fmgr::{varlena_result, FmgrInfo, FunctionCallInfoBaseData as Fcinfo, PGFunction};
use types_trigger::{
    TRIGGER_EVENT_BEFORE, TRIGGER_EVENT_TIMINGMASK, TRIGGER_FIRED_BY_INSERT,
    TRIGGER_FIRED_BY_UPDATE, TRIGGER_FIRED_FOR_ROW,
};
use types_trigger_call::trigger_data_from_fcinfo;
use types_tuple::HeapTupleData;

const LIBRARY: &str = "insert_username";

#[cold]
fn internal(msg: String) -> Box<PgError> {
    Box::new(PgError::error(format!("insert_username{msg}")))
}

#[cold]
fn action_err(msg: String) -> Box<PgError> {
    Box::new(PgError::error(msg).with_sqlstate(ERRCODE_TRIGGERED_ACTION_EXCEPTION))
}

fn fc_insert_username(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    // SAFETY: the trigger call machinery keeps the TriggerData live for the call.
    let Some(td) = (unsafe { trigger_data_from_fcinfo(fcinfo) }) else {
        return Err(internal(": not fired by trigger manager".into()));
    };
    if !TRIGGER_FIRED_FOR_ROW(td.tg_event) {
        return Err(internal(": must be fired for row".into()));
    }
    if td.tg_event & TRIGGER_EVENT_TIMINGMASK != TRIGGER_EVENT_BEFORE {
        return Err(internal(": must be fired before event".into()));
    }
    let rettuple_nn = if TRIGGER_FIRED_BY_INSERT(td.tg_event) {
        td.tg_trigtuple.expect("INSERT row trigger has a tuple")
    } else if TRIGGER_FIRED_BY_UPDATE(td.tg_event) {
        td.tg_newtuple.expect("UPDATE row trigger has a new tuple")
    } else {
        return Err(internal(": cannot process DELETE events".into()));
    };
    // SAFETY: live tuple per the trigger call contract.
    let rettuple: &HeapTupleData<'_> = unsafe { rettuple_nn.as_ref() };

    let rel = td.tg_relation;
    let relname = String::from_utf8_lossy(rel.rd_rel.relname.name_str()).into_owned();
    let trigger = td.tg_trigger;
    if trigger.tgnargs != 1 {
        return Err(internal(format!(" ({relname}): one argument was expected")));
    }
    let colname = trigger.tgargs[0].as_str();
    let tupdesc = rel.descr();
    let attnum = spi::SPI_fnumber(tupdesc, colname);
    if attnum <= 0 {
        return Err(action_err(format!("\"{relname}\" has no attribute \"{colname}\"")));
    }
    if spi::SPI_gettypeid(tupdesc, attnum) != TEXTOID {
        return Err(action_err(format!(
            "attribute \"{colname}\" of \"{relname}\" must be type TEXT"
        )));
    }

    // SAFETY: the armed result mcx outlives this call.
    let mcx = unsafe { fcinfo.result_mcx_detached() };
    let username = miscinit::GetUserNameFromId(mcx, miscinit::GetUserId(), false)?
        .expect("noerr=false raises on a missing role");
    let newval = varlena_result(varlena::cstring_to_text(mcx, username.as_bytes())?);

    let newtuple =
        heaptuple::heap_modify_tuple_by_cols(mcx, rettuple, tupdesc, &[attnum], &[newval], &[false])?;
    let ht = newtuple.as_tuple();
    // SAFETY: the image is mcx-owned and leaked below; the descriptor copy
    // rides in the same mcx so it outlives the trigger driver's copy-out.
    let htd = unsafe {
        HeapTupleData::from_raw_parts(ht.header_ptr(), ht.t_len, ht.t_self, ht.t_tableOid)
    };
    let slot = mcx::leak_in(mcx::alloc_in(mcx, htd)?);
    core::mem::forget(newtuple);
    Ok(Datum::from_usize(core::ptr::from_mut(slot) as usize))
}

fn lookup(function: &str) -> Option<PGFunction> {
    match function {
        "insert_username" => Some(fc_insert_username as PGFunction),
        _ => None,
    }
}

pub fn init_seams() {
    dfmgr::register_builtin_library(dfmgr::BuiltinLibraryEntry {
        name: LIBRARY,
        lookup,
        pg_init: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_exports_insert_username() {
        assert!(lookup("insert_username").is_some());
        assert!(lookup("autoinc").is_none());
    }

    #[test]
    fn internal_errors_carry_function_prefix() {
        assert_eq!(
            internal(" (t): one argument was expected".into()).message(),
            "insert_username (t): one argument was expected"
        );
        assert_eq!(
            internal(": cannot process DELETE events".into()).message(),
            "insert_username: cannot process DELETE events"
        );
    }
}
