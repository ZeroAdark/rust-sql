//! jsonfuncs.c json-half SRFs: json_object_keys (ValuePerCall, first-call
//! key collection as C), json_each[_text] and json_array_elements[_text]
//! (InitMaterializedSRF tuplestore sinks fed row by row as C, with C's
//! per-row tmp_cxt reset).

extern crate alloc;

use alloc::vec::Vec;

use crate::funcs::{invalid_param, parse_sem_or_ereport};
use crate::jsonapi::{JsonLex, JsonLexDe, JsonSem, JsonSemToken, JsonToken};
use datum::Datum;
use funcapi::{InitMaterializedSRF, MaterializedSRF, MAT_SRF_BLESS, MAT_SRF_USE_EXPECTED_DESC};
use mcx::{Mcx, MemoryContext};
use types_error::PgResult;
use types_fmgr::{byref_result, varlena_result, FmgrInfo, FunctionCallInfoBaseData as Fcinfo};

// Owned cross-call rows (per-call memory resets between SRF calls). Public for the Gravity SRF cells
// (gravity_fn srf.rs); `Tuples` is no longer built here since json_each streams into a tuplestore (pgrust main),
// but the cells still name it.
pub enum SrfRows {
    Texts(Vec<Option<Vec<u8>>>),
    Tuples(Vec<Vec<u8>>),
}

// jsonfuncs.c okeys_object_field_start: the char * array starts at 256
// slots and doubles when full, so MaxAllocSize is hit only by the repalloc
// of 2 * count pointers.
fn admit<T>(rows: &mut Vec<T>) -> PgResult<()> {
    let n = rows.len();
    if n >= 256 && n.is_power_of_two() {
        ::mcx::check_alloc_size(n.saturating_mul(2 * core::mem::size_of::<*const u8>()))?;
    }
    rows.try_reserve(1).map_err(|_| ::mcx::oom_named("json SRF", core::mem::size_of::<T>()).into())
}

fn owned(b: &[u8]) -> PgResult<Vec<u8>> {
    let mut v = Vec::new();
    v.try_reserve_exact(b.len()).map_err(|_| ::mcx::oom_named("json SRF", b.len()))?;
    v.extend_from_slice(b);
    Ok(v)
}

pub(crate) fn srf_drive(
    flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
    name: &'static str,
    collect: impl FnOnce(&Fcinfo) -> PgResult<SrfRows>,
) -> PgResult<Datum> {
    let flinfo = flinfo.unwrap_or_else(|| panic!("{name}: NULL flinfo"));
    if !flinfo.has_fn_extra() {
        let fctx = funcapi::init_MultiFuncCall(flinfo, fcinfo)?;
        let rows = collect(fcinfo)?;
        fctx.user_fctx = Some(Box::new(rows));
    }
    let fctx = funcapi::per_MultiFuncCall(flinfo);
    let idx = fctx.call_cntr as usize;
    let rows = fctx
        .user_fctx
        .as_ref()
        .expect("SRF rows set at first call")
        .downcast_ref::<SrfRows>()
        .expect("user_fctx is SrfRows");
    let mcx = fcinfo.result_mcx();
    let out: Option<Option<Datum>> = match rows {
        SrfRows::Texts(v) => v.get(idx).map(|r| match r {
            None => None,
            Some(bytes) => Some(
                varlena::cstring_to_text(mcx, bytes)
                    .map(varlena_result)
                    .expect("text result"),
            ),
        }),
        SrfRows::Tuples(v) => v
            .get(idx)
            .map(|img| Some(byref_result(mcx, img).expect("tuple result"))),
    };
    match out {
        Some(Some(d)) => Ok(funcapi::srf_return_next(flinfo, fcinfo, d)),
        Some(None) => Ok(funcapi::srf_return_next_null(flinfo, fcinfo)),
        None => Ok(funcapi::srf_return_done(flinfo, fcinfo)),
    }
}

struct OkeysState<'mcx> {
    keys: Vec<Option<Vec<u8>>>,
    _marker: core::marker::PhantomData<&'mcx ()>,
}

impl<'mcx> JsonSem<'mcx> for OkeysState<'mcx> {
    fn object_field_start(
        &mut self,
        lex: &JsonLex<'_>,
        fname: &'mcx [u8],
        _isnull: bool,
    ) -> PgResult<bool> {
        if lex.lex_level == 1 {
            admit(&mut self.keys)?;
            self.keys.push(Some(owned(fname)?));
        }
        Ok(true)
    }

    fn array_start(&mut self, lex: &JsonLex<'_>) -> PgResult<bool> {
        if lex.lex_level == 0 {
            return Err(invalid_param(
                "cannot call json_object_keys on an array".into(),
            ));
        }
        Ok(true)
    }

    fn scalar(&mut self, lex: &JsonLex<'_>, _token: JsonSemToken<'mcx>) -> PgResult<bool> {
        if lex.lex_level == 0 {
            return Err(invalid_param(
                "cannot call json_object_keys on a scalar".into(),
            ));
        }
        Ok(true)
    }
}

/// C: json_object_keys — the first-call key collection.
pub fn object_keys_rows(mcx: Mcx<'_>, json: &[u8]) -> PgResult<SrfRows> {
    let mut lex = JsonLexDe::new(mcx, json, mbutils::GetDatabaseEncoding());
    let mut state = OkeysState {
        keys: Vec::new(),
        _marker: core::marker::PhantomData,
    };
    parse_sem_or_ereport(&mut lex, &mut state)?;
    Ok(SrfRows::Texts(state.keys))
}

// Where json_each / json_array_elements put each row: C's tuplestore sink with the per-row tmp_cxt reset (the fmgr
// path, pgrust main), or owned values collected for the Gravity SRF cells (each_pairs / elements_rows).
enum RowSink<'a, 'mcx, T> {
    Srf { sink: &'a mut MaterializedSRF<'mcx>, tmp_cxt: MemoryContext },
    Collect(Vec<T>),
}

fn owned_opt(v: Option<&[u8]>) -> PgResult<Option<Vec<u8>>> {
    v.map(owned).transpose()
}

struct EachState<'a, 'mcx> {
    input: &'a [u8],
    normalize: bool,
    next_scalar: bool,
    normalized_scalar: Option<&'mcx [u8]>,
    result_start: usize,
    out: RowSink<'a, 'mcx, (Vec<u8>, Option<Vec<u8>>)>,
}

impl<'mcx> JsonSem<'mcx> for EachState<'_, 'mcx> {
    fn object_field_start(
        &mut self,
        lex: &JsonLex<'_>,
        _fname: &'mcx [u8],
        _isnull: bool,
    ) -> PgResult<bool> {
        if lex.lex_level == 1 {
            if self.normalize && lex.token_type == JsonToken::String {
                self.next_scalar = true;
            } else {
                self.result_start = lex.token_start.expect("value token started");
            }
        }
        Ok(true)
    }

    fn object_field_end(
        &mut self,
        lex: &JsonLex<'_>,
        fname: &'mcx [u8],
        isnull: bool,
    ) -> PgResult<bool> {
        if lex.lex_level != 1 {
            return Ok(true);
        }
        let input = self.input;
        let val: Option<&[u8]> = if isnull && self.normalize {
            None
        } else if self.next_scalar {
            let s = self.normalized_scalar.expect("scalar recorded");
            self.next_scalar = false;
            Some(s)
        } else {
            Some(&input[self.result_start..lex.prev_token_terminator])
        };
        match &mut self.out {
            RowSink::Srf { sink, tmp_cxt } => {
                let tmp = tmp_cxt.mcx();
                let key = varlena_result(varlena::cstring_to_text(tmp, fname)?);
                let (val, val_null) = match val {
                    None => (Datum::null(), true),
                    Some(b) => (varlena_result(varlena::cstring_to_text(tmp, b)?), false),
                };
                sink.putvalues(&[key, val], &[false, val_null])?;
                tmp_cxt.reset();
            }
            RowSink::Collect(pairs) => {
                admit(pairs)?;
                pairs.push((owned(fname)?, owned_opt(val)?));
            }
        }
        Ok(true)
    }

    fn array_start(&mut self, lex: &JsonLex<'_>) -> PgResult<bool> {
        if lex.lex_level == 0 {
            return Err(invalid_param(
                "cannot deconstruct an array as an object".into(),
            ));
        }
        Ok(true)
    }

    fn scalar(&mut self, lex: &JsonLex<'_>, token: JsonSemToken<'mcx>) -> PgResult<bool> {
        if lex.lex_level == 0 {
            return Err(invalid_param("cannot deconstruct a scalar".into()));
        }
        if self.next_scalar {
            let JsonSemToken::String(s) = token else {
                panic!("next_scalar set on a non-string token")
            };
            self.normalized_scalar = Some(s);
        }
        Ok(true)
    }
}

/// C: each_worker.
pub(crate) fn each_worker(
    flinfo: &mut FmgrInfo,
    fcinfo: &mut Fcinfo,
    as_text: bool,
) -> PgResult<Datum> {
    // SAFETY: executor arms the result mcx pre-call; it outlives this frame.
    let mcx = unsafe { fcinfo.result_mcx_detached() };
    let mut srf = InitMaterializedSRF(mcx, flinfo, fcinfo, MAT_SRF_BLESS)?;
    {
        // SAFETY: catalog arg 0 is a non-null json varlena (strict fn).
        let json = unsafe { fcinfo.arg_varlena_packed(0)? };
        let json = json.data();
        let mut lex = JsonLexDe::new(mcx, json, mbutils::GetDatabaseEncoding());
        let mut state = EachState {
            input: json,
            normalize: as_text,
            next_scalar: false,
            normalized_scalar: None,
            result_start: 0,
            out: RowSink::Srf {
                sink: &mut srf,
                tmp_cxt: mcx.context().new_child_wholesale("json_each temporary cxt"),
            },
        };
        parse_sem_or_ereport(&mut lex, &mut state)?;
    }
    Ok(srf.finish(fcinfo))
}

/// C: each_worker's parse — the (key, value) pairs before they are formed into rows (the
/// value bytes are the raw json text, or the normalized string / None for a null when `as_text`).
/// The Gravity json_each cell's entry (gravity_fn srf.rs).
pub fn each_pairs(mcx: Mcx<'_>, json: &[u8], as_text: bool) -> PgResult<Vec<(Vec<u8>, Option<Vec<u8>>)>> {
    let mut lex = JsonLexDe::new(mcx, json, mbutils::GetDatabaseEncoding());
    let mut state = EachState {
        input: json,
        normalize: as_text,
        next_scalar: false,
        normalized_scalar: None,
        result_start: 0,
        out: RowSink::Collect(Vec::new()),
    };
    parse_sem_or_ereport(&mut lex, &mut state)?;
    let RowSink::Collect(pairs) = state.out else { unreachable!("collect sink") };
    Ok(pairs)
}

struct ElementsState<'a, 'mcx> {
    input: &'a [u8],
    function_name: &'static str,
    normalize: bool,
    next_scalar: bool,
    normalized_scalar: Option<&'mcx [u8]>,
    result_start: usize,
    out: RowSink<'a, 'mcx, Option<Vec<u8>>>,
}

impl<'mcx> JsonSem<'mcx> for ElementsState<'_, 'mcx> {
    fn array_element_start(&mut self, lex: &JsonLex<'_>, _isnull: bool) -> PgResult<bool> {
        if lex.lex_level == 1 {
            if self.normalize && lex.token_type == JsonToken::String {
                self.next_scalar = true;
            } else {
                self.result_start = lex.token_start.expect("element token started");
            }
        }
        Ok(true)
    }

    fn array_element_end(&mut self, lex: &JsonLex<'_>, isnull: bool) -> PgResult<bool> {
        if lex.lex_level != 1 {
            return Ok(true);
        }
        let input = self.input;
        let val: Option<&[u8]> = if isnull && self.normalize {
            None
        } else if self.next_scalar {
            let s = self.normalized_scalar.expect("scalar recorded");
            self.next_scalar = false;
            Some(s)
        } else {
            Some(&input[self.result_start..lex.prev_token_terminator])
        };
        match &mut self.out {
            RowSink::Srf { sink, tmp_cxt } => {
                let tmp = tmp_cxt.mcx();
                let (val, val_null) = match val {
                    None => (Datum::null(), true),
                    Some(b) => (varlena_result(varlena::cstring_to_text(tmp, b)?), false),
                };
                sink.putvalues(&[val], &[val_null])?;
                tmp_cxt.reset();
            }
            RowSink::Collect(rows) => {
                admit(rows)?;
                rows.push(owned_opt(val)?);
            }
        }
        Ok(true)
    }

    fn object_start(&mut self, lex: &JsonLex<'_>) -> PgResult<bool> {
        if lex.lex_level == 0 {
            return Err(invalid_param(alloc::format!(
                "cannot call {} on a non-array",
                self.function_name
            )));
        }
        Ok(true)
    }

    fn scalar(&mut self, lex: &JsonLex<'_>, token: JsonSemToken<'mcx>) -> PgResult<bool> {
        if lex.lex_level == 0 {
            return Err(invalid_param(alloc::format!(
                "cannot call {} on a scalar",
                self.function_name
            )));
        }
        if self.next_scalar {
            let JsonSemToken::String(s) = token else {
                panic!("next_scalar set on a non-string token")
            };
            self.normalized_scalar = Some(s);
        }
        Ok(true)
    }
}

/// C: elements_worker (lexer escapes only when as_text, as C).
pub(crate) fn elements_worker(
    flinfo: &mut FmgrInfo,
    fcinfo: &mut Fcinfo,
    function_name: &'static str,
    as_text: bool,
) -> PgResult<Datum> {
    // SAFETY: executor arms the result mcx pre-call; it outlives this frame.
    let mcx = unsafe { fcinfo.result_mcx_detached() };
    let mut srf = InitMaterializedSRF(
        mcx,
        flinfo,
        fcinfo,
        MAT_SRF_USE_EXPECTED_DESC | MAT_SRF_BLESS,
    )?;
    {
        // SAFETY: catalog arg 0 is a non-null json varlena (strict fn).
        let json = unsafe { fcinfo.arg_varlena_packed(0)? };
        let json = json.data();
        let mut lex =
            JsonLexDe::with_escapes(mcx, json, mbutils::GetDatabaseEncoding(), as_text);
        let mut state = ElementsState {
            input: json,
            function_name,
            normalize: as_text,
            next_scalar: false,
            normalized_scalar: None,
            result_start: 0,
            out: RowSink::Srf {
                sink: &mut srf,
                tmp_cxt: mcx.context().new_child_wholesale("json_array_elements temporary cxt"),
            },
        };
        parse_sem_or_ereport(&mut lex, &mut state)?;
    }
    Ok(srf.finish(fcinfo))
}

/// C: elements_worker's parse (lexer escapes only when as_text, as C): the Gravity json_array_elements cell's
/// entry (gravity_fn srf.rs).
pub fn elements_rows(
    mcx: Mcx<'_>,
    json: &[u8],
    function_name: &'static str,
    as_text: bool,
) -> PgResult<SrfRows> {
    let mut lex = JsonLexDe::with_escapes(mcx, json, mbutils::GetDatabaseEncoding(), as_text);
    let mut state = ElementsState {
        input: json,
        function_name,
        normalize: as_text,
        next_scalar: false,
        normalized_scalar: None,
        result_start: 0,
        out: RowSink::Collect(Vec::new()),
    };
    parse_sem_or_ereport(&mut lex, &mut state)?;
    let RowSink::Collect(rows) = state.out else { unreachable!("collect sink") };
    Ok(SrfRows::Texts(rows))
}

#[cfg(test)]
mod admit_tests {
    // 2^26 keys fill C's array; the next key repallocs 2^27 pointers (1 GB).
    #[test]
    fn admit_follows_c_pointer_array_doubling() {
        let mut v: super::Vec<()> = super::Vec::new();
        v.resize(44_739_243, ());
        assert!(super::admit(&mut v).is_ok());
        v.resize(1 << 25, ());
        assert!(super::admit(&mut v).is_ok());
        v.resize(1 << 26, ());
        let e = super::admit(&mut v).unwrap_err();
        assert_eq!(e.message(), "invalid memory alloc request size 1073741824");
    }
}
