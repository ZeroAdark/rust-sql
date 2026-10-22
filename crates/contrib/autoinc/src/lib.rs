//! `contrib/spi/autoinc` — BEFORE INSERT/UPDATE row trigger that fills each
//! named int4 column from its paired sequence when the column is NULL or 0.

use datum::Datum;
use types_core::INT4OID;
use types_error::{
    PgError, PgResult, ERRCODE_INVALID_NAME, ERRCODE_SYNTAX_ERROR,
    ERRCODE_TRIGGERED_ACTION_EXCEPTION,
};
use types_fmgr::{FmgrInfo, FunctionCallInfoBaseData as Fcinfo, PGFunction};
use types_trigger::{
    TRIGGER_EVENT_BEFORE, TRIGGER_EVENT_TIMINGMASK, TRIGGER_FIRED_BY_INSERT,
    TRIGGER_FIRED_BY_UPDATE, TRIGGER_FIRED_FOR_ROW,
};
use types_trigger_call::trigger_data_from_fcinfo;
use types_tuple::HeapTupleData;

const LIBRARY: &str = "autoinc";

#[cold]
fn internal(msg: String) -> Box<PgError> {
    Box::new(PgError::error(msg))
}

#[cold]
fn action_err(msg: String) -> Box<PgError> {
    Box::new(PgError::error(msg).with_sqlstate(ERRCODE_TRIGGERED_ACTION_EXCEPTION))
}

/// Trigger arguments come in (column, sequence) pairs.
pub fn valid_nargs(nargs: i32) -> bool {
    nargs > 0 && nargs % 2 == 0
}

// nextval(text): textToQualifiedNameList + makeRangeVarFromNameList +
// RangeVarGetRelid(NoLock) + nextval_internal(relid, true).
fn nextval_by_name(mcx: mcx::Mcx<'_>, seqname: &str) -> PgResult<i64> {
    let names = varlena::textToQualifiedNameList(mcx, seqname.as_bytes())?;
    let part = |i: usize| String::from_utf8_lossy(&names[i]);
    let (catalogname, schemaname, relname) = match names.len() {
        1 => (None, None, part(0)),
        2 => (None, Some(part(0)), part(1)),
        3 => (Some(part(0)), Some(part(1)), part(2)),
        0 => {
            return Err(Box::new(
                PgError::error("invalid name syntax").with_sqlstate(ERRCODE_INVALID_NAME),
            ))
        }
        n => {
            let mut joined = String::new();
            for i in 0..n {
                if i > 0 {
                    joined.push('.');
                }
                joined.push_str(&part(i));
            }
            return Err(Box::new(
                PgError::error(format!(
                    "improper relation name (too many dotted names): {joined}"
                ))
                .with_sqlstate(ERRCODE_SYNTAX_ERROR),
            ));
        }
    };
    let rv = rel_vocab::RangeVar {
        catalogname: catalogname.as_deref(),
        schemaname: schemaname.as_deref(),
        relname: &relname,
        inh: true,
        relpersistence: types_core::RELPERSISTENCE_PERMANENT,
        location: -1,
    };
    let relid = catalog_namespace::RangeVarGetRelid(&rv, types_storage::lock::NoLock, false)?;
    sequence_seams::nextval_internal::call(relid, true)
}

fn fc_autoinc(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    // SAFETY: the trigger call machinery keeps the TriggerData live for the call.
    let Some(td) = (unsafe { trigger_data_from_fcinfo(fcinfo) }) else {
        return Err(internal("not fired by trigger manager".into()));
    };
    if !TRIGGER_FIRED_FOR_ROW(td.tg_event) {
        return Err(internal("must be fired for row".into()));
    }
    if td.tg_event & TRIGGER_EVENT_TIMINGMASK != TRIGGER_EVENT_BEFORE {
        return Err(internal("must be fired before event".into()));
    }
    let rettuple_nn = if TRIGGER_FIRED_BY_INSERT(td.tg_event) {
        td.tg_trigtuple.expect("INSERT row trigger has a tuple")
    } else if TRIGGER_FIRED_BY_UPDATE(td.tg_event) {
        td.tg_newtuple.expect("UPDATE row trigger has a new tuple")
    } else {
        return Err(internal("cannot process DELETE events".into()));
    };
    // SAFETY: live tuple per the trigger call contract.
    let rettuple: &HeapTupleData<'_> = unsafe { rettuple_nn.as_ref() };

    let rel = td.tg_relation;
    let relname = String::from_utf8_lossy(rel.rd_rel.relname.name_str()).into_owned();
    let trigger = td.tg_trigger;
    let nargs = trigger.tgnargs as i32;
    if !valid_nargs(nargs) {
        return Err(internal(format!(
            "autoinc ({relname}): even number gt 0 of arguments was expected"
        )));
    }
    let args = &trigger.tgargs;
    let tupdesc = rel.descr();
    // SAFETY: the armed result mcx outlives this call.
    let mcx = unsafe { fcinfo.result_mcx_detached() };

    let mut chattrs: mcx::PgVec<'_, i32> = mcx::PgVec::new_in(mcx);
    let mut newvals: mcx::PgVec<'_, Datum> = mcx::PgVec::new_in(mcx);
    let mut i = 0usize;
    while i < nargs as usize {
        let colname = args[i].as_str();
        let attnum = spi::SPI_fnumber(tupdesc, colname);
        if attnum <= 0 {
            return Err(action_err(format!(
                "\"{relname}\" has no attribute \"{colname}\""
            )));
        }
        if spi::SPI_gettypeid(tupdesc, attnum) != INT4OID {
            return Err(action_err(format!(
                "attribute \"{colname}\" of \"{relname}\" must be type INT4"
            )));
        }
        let (val, isnull) = spi::SPI_getbinval(rettuple, tupdesc, attnum);
        if !isnull && val.as_i32() != 0 {
            i += 2;
            continue;
        }
        i += 1;
        let seqname = args[i].as_str();
        let mut newval = nextval_by_name(mcx, seqname)? as i32;
        if newval == 0 {
            newval = nextval_by_name(mcx, seqname)? as i32;
        }
        chattrs.push(attnum);
        newvals.push(Datum::from_i32(newval));
        i += 1;
    }

    if chattrs.is_empty() {
        return Ok(Datum::from_usize(rettuple_nn.as_ptr() as usize));
    }
    let mut newnulls = mcx::vec_with_capacity_in(mcx, chattrs.len())?;
    newnulls.resize(chattrs.len(), false);
    let newtuple =
        heaptuple::heap_modify_tuple_by_cols(mcx, rettuple, tupdesc, &chattrs, &newvals, &newnulls)?;
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
        "autoinc" => Some(fc_autoinc as PGFunction),
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
    fn nargs_must_be_positive_and_even() {
        assert!(!valid_nargs(0));
        assert!(!valid_nargs(1));
        assert!(valid_nargs(2));
        assert!(!valid_nargs(3));
        assert!(valid_nargs(4));
    }

    #[test]
    fn library_exports_autoinc() {
        assert!(lookup("autoinc").is_some());
        assert!(lookup("insert_username").is_none());
    }
}
