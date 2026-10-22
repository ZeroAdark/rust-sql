//! skeys / svals / each (hstore_op.c): value-per-call SRFs. setup_firstcall
//! copies the hstore into the multi-call context; here the first call owns
//! the pairs (each: the formed tuples) on fn_extra and every call returns
//! one item, so no tuplestore backs a target-list call.

use datum::Datum;
use types_error::{PgError, PgResult};
use types_fmgr::{byref_result, varlena_result, FmgrInfo, FunctionCallInfoBaseData as Fcinfo};
use types_tuple::TupleDescData;

use crate::arg_hstore;

type Pairs = Vec<(Vec<u8>, Option<Vec<u8>>)>;

fn owned_pairs(fcinfo: &Fcinfo) -> PgResult<Pairs> {
    // SAFETY: catalog arg is non-null (strict fn).
    let hs = unsafe { arg_hstore(fcinfo, 0)? };
    Ok((0..hs.count())
        .map(|i| {
            (
                hs.key(i).to_vec(),
                (!hs.val_isnull(i)).then(|| hs.val(i).to_vec()),
            )
        })
        .collect())
}

fn first_call_pairs(flinfo: &mut FmgrInfo, fcinfo: &Fcinfo) -> PgResult<()> {
    if !flinfo.has_fn_extra() {
        let pairs = owned_pairs(fcinfo)?;
        ::funcapi::init_MultiFuncCall(flinfo, fcinfo)?.user_fctx = Some(Box::new(pairs));
    }
    Ok(())
}

// The pair at call_cntr: None past the end.
fn current_pair<'a>(flinfo: &'a mut FmgrInfo, name: &str) -> Option<&'a (Vec<u8>, Option<Vec<u8>>)> {
    let fctx = ::funcapi::per_MultiFuncCall(flinfo);
    let idx = fctx.call_cntr as usize;
    fctx.user_fctx
        .as_ref()
        .unwrap_or_else(|| panic!("{name}: pairs set at first call"))
        .downcast_ref::<Pairs>()
        .unwrap_or_else(|| panic!("{name}: user_fctx is the pair list"))
        .get(idx)
}

pub fn fc_hstore_skeys(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let flinfo = flinfo.expect("hstore_skeys: resolved FmgrInfo required");
    first_call_pairs(flinfo, fcinfo)?;
    let item = match current_pair(flinfo, "hstore_skeys") {
        Some((key, _)) => Some(varlena_result(varlena::cstring_to_text(fcinfo.result_mcx(), key)?)),
        None => None,
    };
    Ok(match item {
        Some(d) => ::funcapi::srf_return_next(flinfo, fcinfo, d),
        None => ::funcapi::srf_return_done(flinfo, fcinfo),
    })
}

pub fn fc_hstore_svals(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let flinfo = flinfo.expect("hstore_svals: resolved FmgrInfo required");
    first_call_pairs(flinfo, fcinfo)?;
    let item = match current_pair(flinfo, "hstore_svals") {
        Some((_, Some(val))) => {
            Some(Some(varlena_result(varlena::cstring_to_text(fcinfo.result_mcx(), val)?)))
        }
        Some((_, None)) => Some(None),
        None => None,
    };
    Ok(match item {
        Some(Some(d)) => ::funcapi::srf_return_next(flinfo, fcinfo, d),
        Some(None) => ::funcapi::srf_return_next_null(flinfo, fcinfo),
        None => ::funcapi::srf_return_done(flinfo, fcinfo),
    })
}

// setup_firstcall's get_call_result_type + BlessTupleDesc, then every
// (key, value) tuple formed up front; a call hands back one image.
fn each_rows(flinfo: &FmgrInfo, fcinfo: &mut Fcinfo, pairs: Pairs) -> PgResult<Vec<Vec<u8>>> {
    let expected_desc = fcinfo.rsinfo_mut().and_then(|rsi| rsi.expectedDesc);
    // SAFETY: expectedDesc contract — the executor armed it with the scan
    // tupdesc, live for the duration of this call.
    let expected = expected_desc.map(|p| unsafe { p.cast::<TupleDescData<'_>>().as_ref() });
    let mcx = fcinfo.result_mcx();
    let resolved = ::funcapi::get_call_result_type(mcx, flinfo, expected)?;
    if resolved.class != ::funcapi::TypeFuncClass::Composite {
        return Err(Box::new(PgError::error("return type must be a row type")));
    }
    let mut desc = resolved.result_tuple_desc.expect("composite result carries a tupdesc");
    // BlessTupleDesc (execTuples.c:2262): a named composite keeps its typmod.
    if desc.tdtypeid == types_core::RECORDOID && desc.tdtypmod < 0 {
        ::typcache_seams::assign_record_type_typmod::call(&mut desc)?;
    }

    let mut rows = ::mcx::std_vec_with_capacity("contrib", pairs.len())?;
    for (key, val) in &pairs {
        let k = varlena_result(varlena::cstring_to_text(mcx, key)?);
        let (v, vnull) = match val {
            Some(v) => (varlena_result(varlena::cstring_to_text(mcx, v)?), false),
            None => (Datum::null(), true),
        };
        let tuple = heaptuple::heap_form_tuple(mcx, &desc, &[k, v], &[false, vnull])?;
        rows.push(::mcx::std_vec_from_slice("contrib", &tuple.image())?);
    }
    Ok(rows)
}

pub fn fc_hstore_each(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let flinfo = flinfo.expect("hstore_each: resolved FmgrInfo required");
    if !flinfo.has_fn_extra() {
        let pairs = owned_pairs(fcinfo)?;
        ::funcapi::init_MultiFuncCall(flinfo, fcinfo)?;
        let rows = each_rows(flinfo, fcinfo, pairs)?;
        ::funcapi::per_MultiFuncCall(flinfo).user_fctx = Some(Box::new(rows));
    }
    let item = {
        let fctx = ::funcapi::per_MultiFuncCall(flinfo);
        let idx = fctx.call_cntr as usize;
        let rows = fctx
            .user_fctx
            .as_ref()
            .expect("hstore_each: rows set at first call")
            .downcast_ref::<Vec<Vec<u8>>>()
            .expect("hstore_each: user_fctx is the row list");
        match rows.get(idx) {
            Some(img) => Some(byref_result(fcinfo.result_mcx(), img)?),
            None => None,
        }
    };
    Ok(match item {
        Some(d) => ::funcapi::srf_return_next(flinfo, fcinfo, d),
        None => ::funcapi::srf_return_done(flinfo, fcinfo),
    })
}
