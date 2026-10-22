//! `contrib/dict_int` — text search dictionary template for integers:
//! drops a leading sign when `absval`, then trims (or, with `rejectlong`,
//! rejects) tokens longer than `maxlen`.

use ::mcx::{alloc_in, vec_with_capacity_in, Mcx, PgVec};
use ::ts_locale::dict_api::{def_get_boolean, def_get_string, DictInitData, LexizeResult};
use ::ts_locale::TsLexeme;
use ::types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};
use ::types_fmgr::{FmgrInfo, FunctionCallInfoBaseData as Fcinfo, PGFunction};
use datum::Datum;

const LIBRARY: &str = "dict_int";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DictInt {
    pub maxlen: i32,
    pub rejectlong: bool,
    pub absval: bool,
}

#[cold]
fn invalid_param(msg: String) -> Box<PgError> {
    PgError::error(msg).with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE).into()
}

/// C atoi: `(int) strtol(s, NULL, 10)` — leading space, optional sign,
/// digits; stops at the first non-digit and saturates like strtol.
pub fn c_atoi(s: &[u8]) -> i32 {
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
        i += 1;
    }
    let neg = match s.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let mut acc: i64 = 0;
    let mut overflow = false;
    while i < s.len() && s[i].is_ascii_digit() {
        let d = (s[i] - b'0') as i64;
        match acc.checked_mul(10).and_then(|v| if neg { v.checked_sub(d) } else { v.checked_add(d) }) {
            Some(v) => acc = v,
            None => overflow = true,
        }
        i += 1;
    }
    if overflow {
        acc = if neg { i64::MIN } else { i64::MAX };
    }
    acc as i32
}

pub fn dintdict_init(init: &DictInitData<'_>) -> PgResult<DictInt> {
    let mut d = DictInt { maxlen: 6, rejectlong: false, absval: false };
    for (i, (name, value)) in init.dict_options.iter().enumerate() {
        match name.as_slice() {
            b"maxlen" => {
                d.maxlen = c_atoi(def_get_string(name, value)?);
                if d.maxlen < 1 {
                    return Err(invalid_param("maxlen value has to be >= 1".into()));
                }
            }
            b"rejectlong" => {
                d.rejectlong = def_get_boolean(
                    name,
                    value.as_ref().map(|v| v.as_slice()),
                    init.int_options[i],
                )?;
            }
            b"absval" => {
                d.absval = def_get_boolean(
                    name,
                    value.as_ref().map(|v| v.as_slice()),
                    init.int_options[i],
                )?;
            }
            _ => {
                return Err(invalid_param(format!(
                    "unrecognized intdict parameter: \"{}\"",
                    String::from_utf8_lossy(name)
                )))
            }
        }
    }
    Ok(d)
}

/// Some(lexeme) or None for a rejected (stopword-like) token.
pub fn dintdict_lexeme<'a>(d: &DictInt, token: &'a [u8]) -> Option<&'a [u8]> {
    let mut txt = if d.absval && matches!(token.first(), Some(b'+' | b'-')) {
        &token[1..]
    } else {
        token
    };
    // pnstrdup stops at an embedded NUL.
    if let Some(nul) = txt.iter().position(|&b| b == 0) {
        txt = &txt[..nul];
    }
    let maxlen = d.maxlen as usize;
    if txt.len() > maxlen {
        if d.rejectlong {
            return None;
        }
        txt = &txt[..maxlen];
    }
    Some(txt)
}

fn dintdict_lexize<'mcx>(mcx: Mcx<'mcx>, d: &DictInt, token: &[u8]) -> PgResult<LexizeResult<'mcx>> {
    let mut out = PgVec::new_in(mcx);
    if let Some(txt) = dintdict_lexeme(d, token) {
        let mut lexeme = vec_with_capacity_in(mcx, txt.len())?;
        lexeme.extend_from_slice(txt);
        out.push(TsLexeme { nvariant: 0, flags: 0, lexeme });
    }
    Ok(LexizeResult(out))
}

fn fc_dintdict_init(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    // SAFETY: arg0 is the DictInitData built by the ts_cache dictionary loader.
    let init = unsafe { &*(fcinfo.arg(0).as_usize() as *const DictInitData<'static>) };
    let d = dintdict_init(init)?;
    let (ptr, _) = ::mcx::PgBox::into_raw_with_allocator(alloc_in(init.mcx, d)?);
    Ok(Datum::from_usize(ptr as usize))
}

fn fc_dintdict_lexize(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    // SAFETY: arg0 came from fc_dintdict_init and outlives the cache entry.
    let d = unsafe { &*(fcinfo.arg(0).as_usize() as *const DictInt) };
    let len = fcinfo.arg(2).as_i32().max(0) as usize;
    // SAFETY: dict_api lexize convention — arg1 points at `len` live bytes.
    let token = unsafe { core::slice::from_raw_parts(fcinfo.arg(1).as_usize() as *const u8, len) };
    // SAFETY: the arming context outlives this call.
    let mcx = unsafe { fcinfo.result_mcx_detached() };
    let r = dintdict_lexize(mcx, d, token)?;
    let (ptr, _) = ::mcx::PgBox::into_raw_with_allocator(alloc_in(mcx, r)?);
    Ok(Datum::from_usize(ptr as usize))
}

fn lookup(function: &str) -> Option<PGFunction> {
    Some(match function {
        "dintdict_init" => fc_dintdict_init,
        "dintdict_lexize" => fc_dintdict_lexize,
        _ => return None,
    })
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

    const DEFAULT: DictInt = DictInt { maxlen: 6, rejectlong: false, absval: false };

    #[test]
    fn default_trims_to_six() {
        assert_eq!(dintdict_lexeme(&DEFAULT, b"12345678"), Some(&b"123456"[..]));
        assert_eq!(dintdict_lexeme(&DEFAULT, b"129"), Some(&b"129"[..]));
        assert_eq!(dintdict_lexeme(&DEFAULT, b"-40865854"), Some(&b"-40865"[..]));
    }

    #[test]
    fn absval_drops_sign_before_length_check() {
        let d = DictInt { absval: true, ..DEFAULT };
        assert_eq!(dintdict_lexeme(&d, b"-40865854"), Some(&b"408658"[..]));
        assert_eq!(dintdict_lexeme(&d, b"+40865854"), Some(&b"408658"[..]));
        assert_eq!(dintdict_lexeme(&d, b"-"), Some(&b""[..]));
    }

    #[test]
    fn rejectlong_rejects() {
        let d = DictInt { absval: true, rejectlong: true, ..DEFAULT };
        assert_eq!(dintdict_lexeme(&d, b"-40865854"), None);
        assert_eq!(dintdict_lexeme(&d, b"-4086585"), None);
        assert_eq!(dintdict_lexeme(&d, b"-408658"), Some(&b"408658"[..]));
    }

    #[test]
    fn atoi_matches_c() {
        assert_eq!(c_atoi(b"6"), 6);
        assert_eq!(c_atoi(b"6.7"), 6);
        assert_eq!(c_atoi(b"  -12x"), -12);
        assert_eq!(c_atoi(b"-214783648"), -214783648);
        assert_eq!(c_atoi(b"abc"), 0);
        assert_eq!(c_atoi(b"99999999999999999999"), -1);
    }

    #[test]
    fn library_exports_template_methods() {
        assert!(lookup("dintdict_init").is_some());
        assert!(lookup("dintdict_lexize").is_some());
        assert!(lookup("dxsyn_init").is_none());
    }
}
