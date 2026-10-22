//! `contrib/spi/refint` — trigger-based referential integrity:
//! `check_primary_key` (referencing side) and `check_foreign_key`
//! (referenced side, with restrict / setnull / cascade actions).

use datum::Datum;
use mcx::PgVec;
use types_core::Oid;
use types_error::{
    PgError, PgResult, ERRCODE_TRIGGERED_ACTION_EXCEPTION, ERRCODE_UNDEFINED_COLUMN,
};
use types_fmgr::{FmgrInfo, FunctionCallInfoBaseData as Fcinfo, PGFunction};
use types_trigger::{
    TRIGGER_FIRED_AFTER, TRIGGER_FIRED_BY_DELETE, TRIGGER_FIRED_BY_INSERT,
    TRIGGER_FIRED_BY_UPDATE, TRIGGER_FIRED_FOR_ROW,
};
use types_trigger_call::trigger_data_from_fcinfo;
use types_tuple::HeapTupleData;

const LIBRARY: &str = "refint";

#[cold]
fn internal(msg: String) -> Box<PgError> {
    Box::new(PgError::error(msg))
}

#[cold]
fn no_attribute(colname: &str, relname: &str) -> Box<PgError> {
    Box::new(
        PgError::error(format!("there is no attribute \"{colname}\" in relation \"{relname}\""))
            .with_sqlstate(ERRCODE_UNDEFINED_COLUMN),
    )
}

/// quote_literal_cstr: single quotes and backslashes doubled, `E` prefix
/// when a backslash is present.
pub fn quote_literal_cstr(s: &[u8]) -> PgResult<Vec<u8>> {
    let mut out = mcx::std_vec_with_capacity("refint", 2 * s.len() + 3)?;
    if s.contains(&b'\\') {
        out.push(b'E');
    }
    out.push(b'\'');
    for &c in s {
        if c == b'\'' || c == b'\\' {
            out.push(c);
        }
        out.push(c);
    }
    out.push(b'\'');
    Ok(out)
}

fn append_where(sql: &mut Vec<u8>, keys: &[&str]) {
    for (i, key) in keys.iter().enumerate() {
        sql.extend_from_slice(format!("{} = ${} ", key, i + 1).as_bytes());
        if i + 1 < keys.len() {
            sql.extend_from_slice(b"and ");
        }
    }
}

/// `select 1 from <relname> where k1 = $1 [and k2 = $2 ...]`
pub fn primary_key_query(relname: &str, pkeys: &[&str]) -> Vec<u8> {
    let mut sql = format!("select 1 from {relname} where ").into_bytes();
    append_where(&mut sql, pkeys);
    sql
}

/// The per-reference statement check_foreign_key prepares. `newvals` is the
/// text form of the new key values (UPDATE ... cascade only).
pub fn foreign_key_query(
    action: u8,
    relname: &str,
    fkeys: &[&str],
    newvals: Option<&[Option<&[u8]>]>,
) -> PgResult<Vec<u8>> {
    let mut sql = Vec::new();
    match action {
        b'r' => sql.extend_from_slice(format!("select 1 from {relname} where ").as_bytes()),
        b'c' => match newvals {
            Some(newvals) => {
                sql.extend_from_slice(format!("update {relname} set ").as_bytes());
                for (k, key) in fkeys.iter().enumerate() {
                    sql.extend_from_slice(format!(" {key} = ").as_bytes());
                    match newvals[k] {
                        Some(nv) => sql.extend_from_slice(&quote_literal_cstr(nv)?),
                        None => sql.extend_from_slice(b"NULL"),
                    }
                    sql.push(b' ');
                    if k + 1 < fkeys.len() {
                        sql.extend_from_slice(b", ");
                    }
                }
                sql.extend_from_slice(b" where ");
            }
            None => sql.extend_from_slice(format!("delete from {relname} where ").as_bytes()),
        },
        _ => {
            sql.extend_from_slice(format!("update {relname} set ").as_bytes());
            for (i, key) in fkeys.iter().enumerate() {
                sql.extend_from_slice(format!("{key} = null").as_bytes());
                if i + 1 < fkeys.len() {
                    sql.extend_from_slice(b", ");
                }
            }
            sql.extend_from_slice(b" where ");
        }
    }
    append_where(&mut sql, fkeys);
    Ok(sql)
}

fn tuple_datum(t: core::ptr::NonNull<HeapTupleData<'_>>) -> Datum {
    Datum::from_usize(t.as_ptr() as usize)
}

fn fc_check_primary_key(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    // SAFETY: the trigger call machinery keeps the TriggerData live for the call.
    let Some(td) = (unsafe { trigger_data_from_fcinfo(fcinfo) }) else {
        return Err(internal("check_primary_key: not fired by trigger manager".into()));
    };
    if !TRIGGER_FIRED_FOR_ROW(td.tg_event) {
        return Err(internal("check_primary_key: must be fired for row".into()));
    }
    if !TRIGGER_FIRED_AFTER(td.tg_event) {
        return Err(internal("check_primary_key: must be fired by AFTER trigger".into()));
    }
    let tuple_nn = if TRIGGER_FIRED_BY_INSERT(td.tg_event) {
        td.tg_trigtuple.expect("INSERT row trigger has a tuple")
    } else if TRIGGER_FIRED_BY_DELETE(td.tg_event) {
        return Err(internal("check_primary_key: cannot process DELETE events".into()));
    } else {
        td.tg_newtuple.expect("UPDATE row trigger has a new tuple")
    };
    // SAFETY: live tuple per the trigger call contract.
    let tuple: &HeapTupleData<'_> = unsafe { tuple_nn.as_ref() };

    let trigger = td.tg_trigger;
    let nargs = trigger.tgnargs as usize;
    let args = |i: usize| trigger.tgargs[i].as_str();
    if nargs % 2 != 1 {
        return Err(internal(
            "check_primary_key: odd number of arguments should be specified".into(),
        ));
    }
    let nkeys = nargs / 2;
    let relname = args(nkeys);
    let rel = td.tg_relation;
    let tupdesc = rel.descr();
    // SAFETY: the armed result mcx outlives this call.
    let mcx = unsafe { fcinfo.result_mcx_detached() };

    spi::SPI_connect()?;

    let mut kvals: PgVec<'_, Datum> = mcx::vec_with_capacity_in(mcx, nkeys)?;
    let mut argtypes: PgVec<'_, Oid> = mcx::vec_with_capacity_in(mcx, nkeys)?;
    let mut pkeys: PgVec<'_, &str> = mcx::vec_with_capacity_in(mcx, nkeys)?;
    for i in 0..nkeys {
        pkeys.push(args(nkeys + 1 + i));
    }
    for i in 0..nkeys {
        let colname = args(i);
        let fnumber = spi::SPI_fnumber(tupdesc, colname);
        if fnumber <= 0 {
            return Err(no_attribute(
                colname,
                &String::from_utf8_lossy(rel.rd_rel.relname.name_str()),
            ));
        }
        let (val, isnull) = spi::SPI_getbinval(tuple, tupdesc, fnumber);
        if isnull {
            spi::SPI_finish()?;
            return Ok(tuple_datum(tuple_nn));
        }
        kvals.push(val);
        argtypes.push(spi::SPI_gettypeid(tupdesc, fnumber));
    }

    let sql = primary_key_query(relname, &pkeys);
    let pplan = spi::SPI_prepare(&String::from_utf8_lossy(&sql), &argtypes)?;

    let mut nulls = mcx::vec_with_capacity_in(mcx, nkeys)?;
    nulls.resize(nkeys, false);
    let ret = spi::SPI_execp(pplan, &kvals, &nulls, 1)?;
    if ret < 0 {
        return Err(internal(format!("check_primary_key: SPI_execp returned {ret}")));
    }

    if spi::SPI_processed() == 0 {
        return Err(Box::new(
            PgError::error("tuple references non-existent key")
                .with_sqlstate(ERRCODE_TRIGGERED_ACTION_EXCEPTION)
                .with_detail(format!(
                    "Trigger \"{}\" found tuple referencing non-existent key in \"{}\".",
                    trigger.tgname.as_str(),
                    relname
                )),
        ));
    }

    spi::SPI_finish()?;
    Ok(tuple_datum(tuple_nn))
}

fn fc_check_foreign_key(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    // SAFETY: the trigger call machinery keeps the TriggerData live for the call.
    let Some(td) = (unsafe { trigger_data_from_fcinfo(fcinfo) }) else {
        return Err(internal("check_foreign_key: not fired by trigger manager".into()));
    };
    if !TRIGGER_FIRED_FOR_ROW(td.tg_event) {
        return Err(internal("check_foreign_key: must be fired for row".into()));
    }
    if TRIGGER_FIRED_BY_INSERT(td.tg_event) {
        return Err(internal("check_foreign_key: cannot process INSERT events".into()));
    }
    if !TRIGGER_FIRED_AFTER(td.tg_event) {
        return Err(internal("check_foreign_key: must be fired by AFTER trigger".into()));
    }

    let trigtuple_nn = td.tg_trigtuple.expect("row trigger has an old tuple");
    // SAFETY: live tuple per the trigger call contract.
    let trigtuple: &HeapTupleData<'_> = unsafe { trigtuple_nn.as_ref() };
    let (newtuple_nn, is_update) = if TRIGGER_FIRED_BY_UPDATE(td.tg_event) {
        (td.tg_newtuple, true)
    } else {
        (None, false)
    };
    let result = tuple_datum(newtuple_nn.unwrap_or(trigtuple_nn));

    let trigger = td.tg_trigger;
    let all_args = |i: usize| trigger.tgargs[i].as_str();
    let nargs = trigger.tgnargs as i32;
    if nargs < 5 {
        return Err(internal(format!(
            "check_foreign_key: too short {nargs} (< 5) list of arguments"
        )));
    }

    let nrefs = numutils::pg_strtoint32_bytes(all_args(0).as_bytes())?;
    if nrefs < 1 {
        return Err(internal(format!(
            "check_foreign_key: {nrefs} (< 1) number of references specified"
        )));
    }
    let action = all_args(1).as_bytes().first().copied().unwrap_or(0).to_ascii_lowercase();
    if action != b'r' && action != b'c' && action != b's' {
        return Err(internal(format!("check_foreign_key: invalid action {}", all_args(1))));
    }
    let nargs = nargs - 2;
    let args = |i: usize| all_args(i + 2);
    let nkeys = (nargs - nrefs) / (nrefs + 1);
    if nkeys <= 0 || nargs != nrefs + nkeys * (nrefs + 1) {
        return Err(internal(format!(
            "check_foreign_key: invalid number of arguments {} for {} references",
            nargs + 2,
            nrefs
        )));
    }
    let nkeys = nkeys as usize;
    let nrefs = nrefs as usize;

    let rel = td.tg_relation;
    let tupdesc = rel.descr();
    // SAFETY: the armed result mcx outlives this call.
    let mcx = unsafe { fcinfo.result_mcx_detached() };

    spi::SPI_connect()?;

    let mut kvals: PgVec<'_, Datum> = mcx::vec_with_capacity_in(mcx, nkeys)?;
    let mut argtypes: PgVec<'_, Oid> = mcx::vec_with_capacity_in(mcx, nkeys)?;
    let mut isequal = true;
    for i in 0..nkeys {
        let colname = args(i);
        let fnumber = spi::SPI_fnumber(tupdesc, colname);
        if fnumber <= 0 {
            return Err(no_attribute(
                colname,
                &String::from_utf8_lossy(rel.rd_rel.relname.name_str()),
            ));
        }
        let (val, isnull) = spi::SPI_getbinval(trigtuple, tupdesc, fnumber);
        if isnull {
            spi::SPI_finish()?;
            return Ok(result);
        }
        if let Some(newtuple_nn) = newtuple_nn {
            // SAFETY: live tuple per the trigger call contract.
            let newtuple = unsafe { newtuple_nn.as_ref() };
            let Some(oldval) = spi::SPI_getvalue(mcx, trigtuple, tupdesc, fnumber)? else {
                return Err(internal(format!(
                    "check_foreign_key: SPI_getvalue returned {}",
                    spi::SPI_result_code_string(spi::SPI_result())
                )));
            };
            let newval = spi::SPI_getvalue(mcx, newtuple, tupdesc, fnumber)?;
            if newval != Some(oldval) {
                isequal = false;
            }
        }
        kvals.push(val);
        argtypes.push(spi::SPI_gettypeid(tupdesc, fnumber));
    }
    let mut newvals: PgVec<'_, Option<&[u8]>> = PgVec::new_in(mcx);
    if action == b'c' && is_update {
        // SAFETY: live tuple per the trigger call contract.
        let newtuple = unsafe { newtuple_nn.expect("UPDATE has a new tuple").as_ref() };
        for i in 0..nkeys {
            let fnumber = spi::SPI_fnumber(tupdesc, args(i));
            newvals.push(spi::SPI_getvalue(mcx, newtuple, tupdesc, fnumber)?);
        }
    }

    let refrel = |r: usize| args(nkeys + r * (nkeys + 1));
    let mut plans = mcx::vec_with_capacity_in(mcx, nrefs)?;
    let mut fkeys: PgVec<'_, &str> = mcx::vec_with_capacity_in(mcx, nkeys)?;
    for r in 0..nrefs {
        fkeys.clear();
        for k in 0..nkeys {
            fkeys.push(args(nkeys + r * (nkeys + 1) + 1 + k));
        }
        let sql = foreign_key_query(
            action,
            refrel(r),
            &fkeys,
            (action == b'c' && is_update).then_some(newvals.as_slice()),
        )?;
        plans.push(spi::SPI_prepare(&String::from_utf8_lossy(&sql), &argtypes)?);
    }

    if newtuple_nn.is_some() && isequal {
        spi::SPI_finish()?;
        return Ok(result);
    }

    let mut nulls = mcx::vec_with_capacity_in(mcx, nkeys)?;
    nulls.resize(nkeys, false);
    for (r, &plan) in plans.iter().enumerate() {
        let tcount = if action == b'r' { 1 } else { 0 };
        let relname = refrel(r);
        let ret = spi::SPI_execp(plan, &kvals, &nulls, tcount)?;
        if ret < 0 {
            return Err(Box::new(
                PgError::error(format!("SPI_execp returned {ret}"))
                    .with_sqlstate(ERRCODE_TRIGGERED_ACTION_EXCEPTION),
            ));
        }
        if action == b'r' {
            if spi::SPI_processed() > 0 {
                return Err(Box::new(
                    PgError::error(format!(
                        "\"{}\": tuple is referenced in \"{}\"",
                        trigger.tgname.as_str(),
                        relname
                    ))
                    .with_sqlstate(ERRCODE_TRIGGERED_ACTION_EXCEPTION),
                ));
            }
        } else {
            let operation = match (action, is_update) {
                (b'c', true) => "updated",
                (b'c', false) => "deleted",
                _ => "set to null",
            };
            elog::elog(
                types_error::NOTICE,
                format!(
                    "{}: {} tuple(s) of {} are {}",
                    trigger.tgname.as_str(),
                    spi::SPI_processed(),
                    relname,
                    operation
                ),
            )?;
        }
    }

    spi::SPI_finish()?;
    Ok(result)
}

fn lookup(function: &str) -> Option<PGFunction> {
    match function {
        "check_primary_key" => Some(fc_check_primary_key as PGFunction),
        "check_foreign_key" => Some(fc_check_foreign_key as PGFunction),
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

    fn s(v: Vec<u8>) -> String {
        String::from_utf8(v).unwrap()
    }

    #[test]
    fn primary_key_query_matches_c_text() {
        assert_eq!(
            s(primary_key_query("pkeys", &["pkey1", "pkey2"])),
            "select 1 from pkeys where pkey1 = $1 and pkey2 = $2 "
        );
    }

    #[test]
    fn foreign_key_queries_match_c_text() {
        assert_eq!(
            s(foreign_key_query(b'r', "fkeys", &["fkey3"], None).unwrap()),
            "select 1 from fkeys where fkey3 = $1 "
        );
        assert_eq!(
            s(foreign_key_query(b'c', "fkeys2", &["fkey21", "fkey22"], None).unwrap()),
            "delete from fkeys2 where fkey21 = $1 and fkey22 = $2 "
        );
        let nv: [Option<&[u8]>; 2] = [Some(b"7"), Some(b"7'0")];
        assert_eq!(
            s(foreign_key_query(b'c', "fkeys", &["fkey1", "fkey2"], Some(&nv)).unwrap()),
            "update fkeys set  fkey1 = '7' ,  fkey2 = '7''0'  where fkey1 = $1 and fkey2 = $2 "
        );
        let nv: [Option<&[u8]>; 1] = [None];
        assert_eq!(
            s(foreign_key_query(b'c', "f", &["a"], Some(&nv)).unwrap()),
            "update f set  a = NULL  where a = $1 "
        );
        assert_eq!(
            s(foreign_key_query(b's', "f", &["a", "b"], None).unwrap()),
            "update f set a = null, b = null where a = $1 and b = $2 "
        );
    }

    #[test]
    fn quote_literal_escapes_like_c() {
        assert_eq!(s(quote_literal_cstr(b"abc").unwrap()), "'abc'");
        assert_eq!(s(quote_literal_cstr(b"it's").unwrap()), "'it''s'");
        assert_eq!(s(quote_literal_cstr(b"a\\b").unwrap()), "E'a\\\\b'");
    }

    #[test]
    fn library_exports_both_functions() {
        assert!(lookup("check_primary_key").is_some());
        assert!(lookup("check_foreign_key").is_some());
        assert!(lookup("autoinc").is_none());
    }
}
