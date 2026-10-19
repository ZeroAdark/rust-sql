use ::datum::Datum;
use ::types_core::Oid;
use ::types_error::{PgError, PgResult};
use ::types_fmgr::{FmgrBuiltin, FmgrInfo, FunctionCallInfoBaseData as Fcinfo, PGFunction};

fn arg_text<'a>(fcinfo: &'a Fcinfo, i: usize) -> PgResult<&'a [u8]> {
    // SAFETY: strict text arg is a non-null live varlena.
    let pv = unsafe { fcinfo.arg_varlena_packed(i) }?;
    if pv.is_short() {
        Ok(pv.data_expanded(fcinfo.result_mcx())?)
    } else {
        Ok(pv.data())
    }
}

fn fc_gin_cmp_tslexeme(_f: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let a = arg_text(fcinfo, 0)?;
    let b = arg_text(fcinfo, 1)?;
    Ok(Datum::from_i32(crate::gin_cmp_tslexeme(a, b)))
}

fn fc_gin_cmp_prefix(_f: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let a = arg_text(fcinfo, 0)?;
    let b = arg_text(fcinfo, 1)?;
    Ok(Datum::from_i32(crate::gin_cmp_prefix(a, b)))
}

// tsginidx.c: the pre-9.1 compatibility stubs guard their argument count
// before delegating, so an internal-language alias called with too few
// arguments reports C's elog instead of reaching native dispatch.
fn compat_nargs_guard(
    flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
    required: usize,
    msg: &'static str,
) -> PgResult<Datum> {
    if fcinfo.nargs() < required {
        return Err(Box::new(PgError::error(msg)));
    }
    ::types_fmgr::fc_internal_dispatch_only(flinfo, fcinfo)
}

fn fc_gin_extract_tsvector_2args(f: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    compat_nargs_guard(f, fcinfo, 3, "gin_extract_tsvector requires three arguments")
}

fn fc_gin_extract_tsquery_5args(f: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    compat_nargs_guard(f, fcinfo, 7, "gin_extract_tsquery requires seven arguments")
}

fn fc_gin_tsquery_consistent_6args(f: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    compat_nargs_guard(f, fcinfo, 8, "gin_tsquery_consistent requires eight arguments")
}

const fn b(foid: Oid, name: &'static str, nargs: i16, func: PGFunction) -> FmgrBuiltin {
    FmgrBuiltin { foid, name, nargs, strict: true, retset: false, func }
}

pub const TSGINIDX_BUILTINS: &[FmgrBuiltin] = &[
    b(2700, "gin_cmp_prefix", 4, fc_gin_cmp_prefix),
    b(3724, "gin_cmp_tslexeme", 2, fc_gin_cmp_tslexeme),
    // tsvector/tsquery GIN opclass support: native GIN dispatch; fmgr-lookup
    // parity rows.
    b(3077, "gin_extract_tsvector_2args", 2, fc_gin_extract_tsvector_2args),
    b(3087, "gin_extract_tsquery_5args", 5, fc_gin_extract_tsquery_5args),
    b(3088, "gin_tsquery_consistent_6args", 6, fc_gin_tsquery_consistent_6args),
    b(3656, "gin_extract_tsvector", 3, ::types_fmgr::fc_internal_dispatch_only),
    b(3657, "gin_extract_tsquery", 7, ::types_fmgr::fc_internal_dispatch_only),
    b(3658, "gin_tsquery_consistent", 8, ::types_fmgr::fc_internal_dispatch_only),
    b(3791, "gin_extract_tsquery_oldsig", 7, ::types_fmgr::fc_internal_dispatch_only),
    b(3792, "gin_tsquery_consistent_oldsig", 8, ::types_fmgr::fc_internal_dispatch_only),
    b(3921, "gin_tsquery_triconsistent", 7, ::types_fmgr::fc_internal_dispatch_only),
];
