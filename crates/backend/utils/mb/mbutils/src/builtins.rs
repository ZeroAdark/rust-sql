use datum::{Datum, Varlena, VARHDRSZ};
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};
use types_fmgr::{
    byref_result, varlena_result, FmgrBuiltin, FmgrInfo, FunctionCallInfoBaseData as Fcinfo,
};
use wchar::pg_enc;

// C returns namein(DatabaseEncoding->name): a NAMEDATALEN block.
pub fn fc_getdatabaseencoding(
    _flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    let name = crate::GetDatabaseEncodingName();
    let mut buf = [0u8; 64];
    buf[..name.len()].copy_from_slice(name.as_bytes());
    byref_result(fcinfo.result_mcx(), &buf)
}

fn name_str(name: &[u8; 64]) -> &str {
    let n = name.iter().position(|&b| b == 0).unwrap_or(64);
    core::str::from_utf8(&name[..n]).unwrap_or("")
}

#[track_caller]
#[cold]
fn invalid_encoding_name(which: &str, name: &str) -> Box<PgError> {
    Box::new(
        PgError::error(format!("invalid {which} encoding name \"{name}\""))
            .with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE),
    )
}

fn memo(flinfo: Option<&mut FmgrInfo>) -> Option<&mut crate::ConvertMemo> {
    let flinfo = flinfo?;
    if !flinfo.has_fn_extra() {
        flinfo.set_fn_extra(crate::ConvertMemo::default());
    }
    flinfo.fn_extra_mut::<crate::ConvertMemo>()
}

/// pg_char_to_encoding of a `name` argument, through the site's memo slot `i` (the name a site passes is nearly
/// always the same constant).
fn name_encoding(memo: &mut Option<&mut crate::ConvertMemo>, i: usize, name: &[u8; 64]) -> pg_enc {
    if let Some(m) = memo.as_deref_mut() {
        if let Some((n, e)) = m.names[i] {
            if n == *name {
                return e;
            }
        }
        let e = crate::pg_char_to_encoding(name_str(name));
        m.names[i] = Some((*name, e));
        return e;
    }
    crate::pg_char_to_encoding(name_str(name))
}

fn convert_common(
    fcinfo: &mut Fcinfo,
    string_arg: usize,
    src_encoding: pg_enc,
    dest_encoding: pg_enc,
    memo: Option<&mut crate::ConvertMemo>,
) -> PgResult<Datum> {
    let string = unsafe { fcinfo.arg_varlena_packed(string_arg)? };
    let src = string.data();
    crate::pg_verify_mbstr(src_encoding, src, false)?;
    let mcx = fcinfo.result_mcx();
    match crate::pg_do_encoding_conversion_at(mcx, src, src_encoding, dest_encoding, memo)? {
        // mbutils.c:599 PG_RETURN_BYTEA_P(string): the detoasted image.
        None => Ok(Datum::from_usize(string.as_ptr() as usize)),
        Some(out) => {
            let mut image = ::mcx::vec_with_capacity_in(mcx, VARHDRSZ + out.len())?;
            ::mcx::vec_append_bytes(&mut image, &[0u8; VARHDRSZ])?;
            ::mcx::vec_append_bytes(&mut image, &out)?;
            Ok(varlena_result(Varlena::from_image(image)))
        }
    }
}

// BYTEA convert(BYTEA string, NAME src_encoding_name, NAME dest_encoding_name)
pub fn fc_pg_convert(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let mut memo = memo(flinfo);
    let src_name = unsafe { fcinfo.arg_name(1) };
    let src_encoding = name_encoding(&mut memo, 0, src_name);
    if src_encoding < 0 {
        return Err(invalid_encoding_name("source", name_str(src_name)));
    }
    let dest_name = unsafe { fcinfo.arg_name(2) };
    let dest_encoding = name_encoding(&mut memo, 1, dest_name);
    if dest_encoding < 0 {
        return Err(invalid_encoding_name("destination", name_str(dest_name)));
    }
    convert_common(fcinfo, 0, src_encoding, dest_encoding, memo)
}

// TEXT convert_from(BYTEA string, NAME src_encoding_name); dest = DB encoding.
pub fn fc_pg_convert_from(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let mut memo = memo(flinfo);
    let src_name = unsafe { fcinfo.arg_name(1) };
    let src_encoding = name_encoding(&mut memo, 0, src_name);
    if src_encoding < 0 {
        return Err(invalid_encoding_name("source", name_str(src_name)));
    }
    convert_common(fcinfo, 0, src_encoding, crate::GetDatabaseEncoding(), memo)
}

// BYTEA convert_to(TEXT string, NAME dest_encoding_name); src = DB encoding.
pub fn fc_pg_convert_to(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let mut memo = memo(flinfo);
    let dest_name = unsafe { fcinfo.arg_name(1) };
    let dest_encoding = name_encoding(&mut memo, 1, dest_name);
    if dest_encoding < 0 {
        return Err(invalid_encoding_name("destination", name_str(dest_name)));
    }
    convert_common(fcinfo, 0, crate::GetDatabaseEncoding(), dest_encoding, memo)
}

// INT4 length(BYTEA string, NAME src_encoding_name) — C length_in_encoding
// (mbutils.c): character count of the bytea in the named encoding, erroring
// on invalid sequences exactly like pg_verify_mbstr_len(noError = false).
pub fn fc_length_in_encoding(
    _flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    let src_name = name_str(unsafe { fcinfo.arg_name(1) });
    let src_encoding = crate::pg_char_to_encoding(src_name);
    if src_encoding < 0 {
        // C spells this one without the source/destination qualifier.
        return Err(Box::new(
            PgError::error(format!("invalid encoding name \"{src_name}\""))
                .with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE),
        ));
    }
    let string = unsafe { fcinfo.arg_varlena_packed(0)? };
    let retval = crate::pg_verify_mbstr_len(src_encoding, string.data(), false)?;
    Ok(Datum::from_i32(retval))
}

// pg_encoding_max_length_sql (mbutils.c): NULL for an invalid encoding number.
pub fn fc_pg_encoding_max_length(
    _flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    let encoding = fcinfo.arg_i32(0);
    if wchar::pg_valid_encoding(encoding) {
        Ok(Datum::from_i32(wchar::pg_encoding_max_length(encoding)))
    } else {
        Ok(fcinfo.return_null())
    }
}

// pg_proc.dat rows (all proisstrict, none retset), OID-ascending.
pub const MBUTILS_BUILTINS: &[FmgrBuiltin] = &[
    FmgrBuiltin {
        foid: 1039,
        name: "getdatabaseencoding",
        nargs: 0,
        strict: true,
        retset: false,
        func: fc_getdatabaseencoding,
    },
    FmgrBuiltin {
        foid: 1713,
        name: "length_in_encoding",
        nargs: 2,
        strict: true,
        retset: false,
        func: fc_length_in_encoding,
    },
    FmgrBuiltin {
        foid: 1714,
        name: "pg_convert_from",
        nargs: 2,
        strict: true,
        retset: false,
        func: fc_pg_convert_from,
    },
    FmgrBuiltin {
        foid: 1717,
        name: "pg_convert_to",
        nargs: 2,
        strict: true,
        retset: false,
        func: fc_pg_convert_to,
    },
    FmgrBuiltin {
        foid: 1813,
        name: "pg_convert",
        nargs: 3,
        strict: true,
        retset: false,
        func: fc_pg_convert,
    },
    FmgrBuiltin {
        foid: 2319,
        name: "pg_encoding_max_length",
        nargs: 1,
        strict: true,
        retset: false,
        func: fc_pg_encoding_max_length,
    },
];
