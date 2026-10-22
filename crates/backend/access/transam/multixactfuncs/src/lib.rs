//! pg_get_multixact_members (multixact.c) — hosted apart from the multixact
//! crate because fmgr_core's dep tree reaches multixact (adt_scalar).
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]

use datum::Datum;
use types_error::{
    PgError, PgResult, ERRCODE_DATATYPE_MISMATCH, ERRCODE_INVALID_PARAMETER_VALUE,
};
use types_fmgr::{byref_result, FmgrBuiltin, FmgrInfo, FunctionCallInfoBaseData as Fcinfo};
use types_tuple::TupleDescData;

use multixact::{mxstatus_to_string, FirstMultiXactId, GetMultiXactIdMembers};

pub fn register_builtins() {
    fmgr_core::register_late_builtins(MULTIXACTFUNCS_BUILTINS);
}

static MULTIXACTFUNCS_BUILTINS: &[FmgrBuiltin] = &[FmgrBuiltin {
    foid: 3819,
    name: "pg_get_multixact_members",
    nargs: 1,
    strict: true,
    retset: true,
    func: fc_pg_get_multixact_members,
}];

struct MemberRows {
    members: Vec<types_storage::multixact::MultiXactMember>,
}

fn expected_desc(fcinfo: &mut Fcinfo) -> Option<&'static TupleDescData<'static>> {
    let expected_desc = fcinfo.rsinfo_mut().and_then(|rsi| rsi.expectedDesc);
    // SAFETY: expectedDesc contract — the executor armed it with the scan
    // tupdesc, live for the duration of this call.
    expected_desc.map(|p| unsafe { p.cast::<TupleDescData<'static>>().as_ref() })
}

fn result_desc<'mcx>(
    mcx: ::mcx::Mcx<'mcx>,
    flinfo: &FmgrInfo,
    expected: Option<&TupleDescData<'_>>,
) -> PgResult<TupleDescData<'mcx>> {
    let resolved = funcapi::get_call_result_type(mcx, flinfo, expected)?;
    if resolved.class != funcapi::TypeFuncClass::Composite {
        return Err(Box::new(PgError::error("return type must be a row type")));
    }
    let mut desc = resolved.result_tuple_desc.expect("composite result carries a tupdesc");
    // C BlessTupleDesc: only an anonymous record gets a typmod assigned.
    if desc.tdtypeid == types_core::catalog::RECORDOID && desc.tdtypmod < 0 {
        ::typcache_seams::assign_record_type_typmod::call(&mut desc)?;
    }
    let natts = desc.natts as usize;
    let live = desc.attrs[..natts].iter().filter(|a| !a.attisdropped).count();
    // DIVERGENCE (multixact.c:3767-3774): C's BuildTupleFromCStrings reads
    // past its two-entry values[] for a wider column definition list and
    // crashes the backend; refuse with the executor's tupledesc_match text
    // (dropped attributes are skipped there, execTuples.c:2342).
    if live > 2 {
        return Err(Box::new(
            PgError::error("function return row and query-specified return row do not match")
                .with_sqlstate(ERRCODE_DATATYPE_MISMATCH)
                .with_detail(format!(
                    "Returned row contains 2 attributes, but query expects {live}."
                )),
        ));
    }
    Ok(desc)
}

fn member_tuple(
    flinfo: &FmgrInfo,
    fcinfo: &mut Fcinfo,
    m: &types_storage::multixact::MultiXactMember,
) -> PgResult<Datum> {
    let expected = expected_desc(fcinfo);
    let mcx = fcinfo.result_mcx();
    let desc = result_desc(mcx, flinfo, expected)?;
    let natts = desc.natts as usize;
    let mut attinmeta = funcapi::AttInMetadata::new(&desc)?;
    let xid = m.xid.to_string();
    let mut live = [Some(xid.as_bytes()), Some(mxstatus_to_string(m.status).as_bytes())]
        .into_iter();
    let mut cstrings = ::mcx::std_vec_with_capacity("access", natts)?;
    for att in &desc.attrs[..natts] {
        cstrings.push(if att.attisdropped { None } else { live.next().flatten() });
    }
    let (values, isnull) = attinmeta.build(mcx, &cstrings)?;
    let tuple = heaptuple::heap_form_tuple(mcx, &desc, &values, &isnull)?;
    byref_result(mcx, &tuple.image())
}

pub fn fc_pg_get_multixact_members(
    flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    let flinfo = flinfo.expect("pg_get_multixact_members: NULL flinfo");
    let mxid = fcinfo.arg(0).as_u32();
    if mxid < FirstMultiXactId {
        return Err(Box::new(
            PgError::error(format!("invalid MultiXactId: {mxid}"))
                .with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE),
        ));
    }
    if !flinfo.has_fn_extra() {
        // multixact.c:3739: SRF_FIRSTCALL_INIT precedes the member read.
        funcapi::init_MultiFuncCall(flinfo, fcinfo)?;
        let rows = (|| {
            let mut members = Vec::new();
            GetMultiXactIdMembers(mxid, false, false, &mut |m| members.extend_from_slice(m))?;
            let expected = expected_desc(fcinfo);
            result_desc(fcinfo.result_mcx(), flinfo, expected)?;
            Ok(MemberRows { members })
        })();
        let rows = match rows {
            Ok(rows) => rows,
            Err(e) => {
                funcapi::end_MultiFuncCall(flinfo);
                return Err(e);
            }
        };
        funcapi::per_MultiFuncCall(flinfo).user_fctx = Some(Box::new(rows));
    }
    let idx = funcapi::per_MultiFuncCall(flinfo).call_cntr as usize;
    // multixact.c:3762-3772: one member is converted per call.
    let member = funcapi::per_MultiFuncCall(flinfo)
        .user_fctx
        .as_ref()
        .expect("pg_get_multixact_members: rows set at first call")
        .downcast_ref::<MemberRows>()
        .expect("pg_get_multixact_members: user_fctx is MemberRows")
        .members
        .get(idx)
        .copied();
    match member {
        Some(m) => {
            let d = member_tuple(flinfo, fcinfo, &m)?;
            Ok(funcapi::srf_return_next(flinfo, fcinfo, d))
        }
        None => Ok(funcapi::srf_return_done(flinfo, fcinfo)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use types_error::ERRCODE_FEATURE_NOT_SUPPORTED;
    use types_fmgr::LocalFcinfo;

    #[test]
    fn rows_match_canonical() {
        fmgr_core::assert_rows_match_canonical(MULTIXACTFUNCS_BUILTINS);
    }

    // C: mxid < FirstMultiXactId => ERRCODE_INVALID_PARAMETER_VALUE
    // "invalid MultiXactId: %u".
    #[test]
    fn invalid_multixactid_is_22023() {
        let mut fci = LocalFcinfo::<1>::new(0);
        fci.set_arg(0, Datum::from_u32(0));
        let mut flinfo = types_fmgr::FmgrInfo::unresolved();
        let err = fc_pg_get_multixact_members(Some(&mut flinfo), &mut fci).unwrap_err();
        assert_eq!(err.sqlstate(), ERRCODE_INVALID_PARAMETER_VALUE);
    }

    // multixact.c:3739: SRF_FIRSTCALL_INIT runs before GetMultiXactIdMembers,
    // so a non-set context is 0A000 before any member (or its absence) is
    // reported, and no fn_extra is left behind.
    #[test]
    fn non_set_context_is_0a000_before_member_read() {
        let mut fci = LocalFcinfo::<1>::new(0);
        fci.set_arg(0, Datum::from_u32(5));
        let mut flinfo = types_fmgr::FmgrInfo::unresolved();
        let err = fc_pg_get_multixact_members(Some(&mut flinfo), &mut fci).unwrap_err();
        assert_eq!(err.sqlstate(), ERRCODE_FEATURE_NOT_SUPPORTED);
        assert!(!flinfo.has_fn_extra());
    }
}
