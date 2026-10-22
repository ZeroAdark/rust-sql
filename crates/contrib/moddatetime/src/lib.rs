//! `contrib/spi/moddatetime` — BEFORE UPDATE row trigger that stamps the
//! named timestamp/timestamptz column with the current transaction time.

use datum::Datum;
use types_core::{TIMESTAMPOID, TIMESTAMPTZOID};
use types_error::{PgError, PgResult, ERRCODE_TRIGGERED_ACTION_EXCEPTION};
use types_fmgr::{FmgrInfo, FunctionCallInfoBaseData as Fcinfo, PGFunction};
use types_trigger::{
    TRIGGER_EVENT_BEFORE, TRIGGER_EVENT_TIMINGMASK, TRIGGER_FIRED_BY_INSERT,
    TRIGGER_FIRED_BY_UPDATE, TRIGGER_FIRED_FOR_ROW,
};
use types_trigger_call::trigger_data_from_fcinfo;
use types_tuple::HeapTupleData;

const LIBRARY: &str = "moddatetime";

#[cold]
fn internal(msg: String) -> Box<PgError> {
    Box::new(PgError::error(format!("moddatetime{msg}")))
}

#[cold]
fn action_err(msg: String) -> Box<PgError> {
    Box::new(PgError::error(msg).with_sqlstate(ERRCODE_TRIGGERED_ACTION_EXCEPTION))
}

/// The "now" value of the column's type, or None for any other type.
pub fn now_for_type(atttypid: types_core::Oid) -> PgResult<Option<Datum>> {
    Ok(match atttypid {
        TIMESTAMPOID => Some(Datum::from_i64(adt_timestamp::timestamp_in("now", -1, None)?)),
        TIMESTAMPTZOID => Some(Datum::from_i64(adt_timestamp::timestamptz_in("now", -1, None)?)),
        _ => None,
    })
}

fn fc_moddatetime(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
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
        return Err(internal(": cannot process INSERT events".into()));
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
        return Err(internal(format!(" ({relname}): A single argument was expected")));
    }
    let colname = trigger.tgargs[0].as_str();
    let tupdesc = rel.descr();
    let attnum = spi::SPI_fnumber(tupdesc, colname);
    if attnum <= 0 {
        return Err(action_err(format!("\"{relname}\" has no attribute \"{colname}\"")));
    }
    let Some(newdt) = now_for_type(spi::SPI_gettypeid(tupdesc, attnum))? else {
        return Err(action_err(format!(
            "attribute \"{colname}\" of \"{relname}\" must be type TIMESTAMP or TIMESTAMPTZ"
        )));
    };

    // SAFETY: the armed result mcx outlives this call.
    let mcx = unsafe { fcinfo.result_mcx_detached() };
    let newtuple =
        heaptuple::heap_modify_tuple_by_cols(mcx, rettuple, tupdesc, &[attnum], &[newdt], &[false])?;
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
        "moddatetime" => Some(fc_moddatetime as PGFunction),
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
    fn library_exports_moddatetime() {
        assert!(lookup("moddatetime").is_some());
        assert!(lookup("autoinc").is_none());
    }

    #[test]
    fn only_timestamp_types_are_stamped() {
        assert!(now_for_type(types_core::INT4OID).unwrap().is_none());
        assert!(now_for_type(types_core::TEXTOID).unwrap().is_none());
    }

    #[test]
    fn internal_errors_carry_function_prefix() {
        assert_eq!(
            internal(" (t): A single argument was expected".into()).message(),
            "moddatetime (t): A single argument was expected"
        );
    }
}
