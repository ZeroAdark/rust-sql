//! jsonfuncs.c SRF slice: jsonb_object_keys (ValuePerCall), jsonb_array_elements[_text] and jsonb_each[_text] (materialized tuplestore, per-row tmp_cxt reset).

extern crate alloc;

use alloc::vec::Vec;

use crate::build::item_to_jsonb_image;

// jsonb_object_keys collects under MaxAllocSize with fallible growth.
fn admit<T>(rows: &mut Vec<T>) {
    let n = rows.len() + 1;
    if ::mcx::check_alloc_size(n.saturating_mul(core::mem::size_of::<T>().max(16))).is_err()
        || rows.try_reserve(1).is_err()
    {
        std::panic::panic_any(::mcx::oom_named("jsonb SRF", core::mem::size_of::<T>()));
    }
}

fn owned(b: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    if v.try_reserve_exact(b.len()).is_err() {
        std::panic::panic_any(::mcx::oom_named("jsonb SRF", b.len()));
    }
    v.extend_from_slice(b);
    v
}
use crate::container::*;
use crate::getfield::value_as_text;
use crate::iter::{JsonbIterator, WjbToken};
use datum::Datum;
use funcapi::{InitMaterializedSRF, MAT_SRF_BLESS, MAT_SRF_USE_EXPECTED_DESC};
use mcx::Mcx;
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};
use types_fmgr::{varlena_result, FmgrInfo, FunctionCallInfoBaseData as Fcinfo};

#[track_caller]
#[cold]
#[inline(never)]
fn invalid_param(msg: String) -> Box<PgError> {
    Box::new(PgError::error(msg).with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE))
}

// Owned cross-call rows (per-call memory resets between SRF calls).
enum SrfRows {
    Texts(Vec<Option<Vec<u8>>>),
}

fn detoast_owned(fcinfo: &Fcinfo) -> PgResult<Vec<u8>> {
    let mcx = fcinfo.result_mcx();
    let jb = super::builtins::arg_jsonb(fcinfo, 0, mcx)?;
    Ok(::mcx::std_vec_from_slice("adt", &jb.as_bytes())?)
}

fn srf_drive(
    flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
    name: &'static str,
    collect: impl FnOnce(&Fcinfo) -> PgResult<SrfRows>,
) -> PgResult<Datum> {
    let flinfo = flinfo.unwrap_or_else(|| panic!("{name}: NULL flinfo"));
    if !flinfo.has_fn_extra() {
        let rows = collect(fcinfo)?;
        let fctx = funcapi::init_MultiFuncCall(flinfo, fcinfo)?;
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
    };
    match out {
        Some(Some(d)) => Ok(funcapi::srf_return_next(flinfo, fcinfo, d)),
        Some(None) => Ok(funcapi::srf_return_next_null(flinfo, fcinfo)),
        None => Ok(funcapi::srf_return_done(flinfo, fcinfo)),
    }
}

fn require_object(payload: &[u8], name: &str) -> PgResult<()> {
    if container_is_object(payload) {
        return Ok(());
    }
    Err(invalid_param(alloc::format!(
        "cannot call {name} on a non-object"
    )))
}

pub fn fc_jsonb_object_keys(
    flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    srf_drive(flinfo, fcinfo, "jsonb_object_keys", |fcinfo| {
        let image = detoast_owned(fcinfo)?;
        let payload = &image[..];
        if container_is_scalar(payload) {
            return Err(invalid_param(
                "cannot call jsonb_object_keys on a scalar".into(),
            ));
        }
        if container_is_array(payload) {
            return Err(invalid_param(
                "cannot call jsonb_object_keys on an array".into(),
            ));
        }
        let mcx = fcinfo.result_mcx();
        let mut it = JsonbIterator::init(mcx, payload)?;
        let mut keys: Vec<Option<Vec<u8>>> = Vec::new();
        loop {
            let (tok, v) = it.next(true);
            match tok {
                WjbToken::Done => break,
                WjbToken::Key => {
                    let JsonbItem::String(s) = v else {
                        panic!("object key is not a string")
                    };
                    admit(&mut keys);
                    keys.push(Some(owned(s)));
                }
                _ => {}
            }
        }
        Ok(SrfRows::Texts(keys))
    })
}

fn elements_check(payload: &[u8]) -> PgResult<()> {
    // C: the scalar check runs only per the array check order.
    if container_is_scalar(payload) {
        return Err(invalid_param("cannot extract elements from a scalar".into()));
    }
    if !container_is_array(payload) {
        return Err(invalid_param(
            "cannot extract elements from an object".into(),
        ));
    }
    Ok(())
}

// C: the value column of one row; text mode maps json null to SQL null.
fn value_datum(tmp: Mcx<'_>, v: JsonbItem<'_>, as_text: bool) -> PgResult<(Datum, bool)> {
    if as_text {
        Ok(match value_as_text(tmp, &v)? {
            Some(t) => (varlena_result(t), false),
            None => (Datum::null(), true),
        })
    } else {
        Ok((super::builtins::image_result(item_to_jsonb_image(tmp, v)?), false))
    }
}

// C: elements_worker_jsonb.
fn elements_worker(
    flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
    name: &'static str,
    as_text: bool,
) -> PgResult<Datum> {
    let flinfo = flinfo.unwrap_or_else(|| panic!("{name}: NULL flinfo"));
    // SAFETY: executor arms the result mcx pre-call; it outlives this frame.
    let mcx = unsafe { fcinfo.result_mcx_detached() };
    let mut srf = InitMaterializedSRF(
        mcx,
        flinfo,
        fcinfo,
        MAT_SRF_USE_EXPECTED_DESC | MAT_SRF_BLESS,
    )?;
    {
        let jb = super::builtins::arg_jsonb(fcinfo, 0, mcx)?;
        let payload = jb.as_bytes();
        elements_check(payload)?;
        let mut tmp_cxt = mcx.context().new_child_wholesale("jsonb_array_elements temporary cxt");
        let mut it = JsonbIterator::init(mcx, payload)?;
        loop {
            let (tok, v) = it.next(true);
            match tok {
                WjbToken::Done => break,
                WjbToken::Elem => {
                    let (val, val_null) = value_datum(tmp_cxt.mcx(), v, as_text)?;
                    srf.putvalues(&[val], &[val_null])?;
                    tmp_cxt.reset();
                }
                _ => {}
            }
        }
    }
    Ok(srf.finish(fcinfo))
}

pub fn fc_jsonb_array_elements(
    flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    elements_worker(flinfo, fcinfo, "jsonb_array_elements", false)
}

pub fn fc_jsonb_array_elements_text(
    flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    elements_worker(flinfo, fcinfo, "jsonb_array_elements_text", true)
}

// C: each_worker_jsonb.
fn each_worker(
    flinfo: Option<&mut FmgrInfo>,
    fcinfo: &mut Fcinfo,
    name: &'static str,
    as_text: bool,
) -> PgResult<Datum> {
    let flinfo = flinfo.unwrap_or_else(|| panic!("{name}: NULL flinfo"));
    // SAFETY: executor arms the result mcx pre-call; it outlives this frame.
    let mcx = unsafe { fcinfo.result_mcx_detached() };
    let mut srf = InitMaterializedSRF(mcx, flinfo, fcinfo, MAT_SRF_BLESS)?;
    {
        let jb = super::builtins::arg_jsonb(fcinfo, 0, mcx)?;
        let payload = jb.as_bytes();
        require_object(payload, name)?;
        let mut tmp_cxt = mcx.context().new_child_wholesale("jsonb_each temporary cxt");
        let mut it = JsonbIterator::init(mcx, payload)?;
        loop {
            let (tok, k) = it.next(true);
            match tok {
                WjbToken::Done => break,
                WjbToken::Key => {
                    let JsonbItem::String(key) = k else {
                        panic!("object key is not a string")
                    };
                    let tmp = tmp_cxt.mcx();
                    let key = varlena_result(varlena::cstring_to_text(tmp, key)?);
                    let (vtok, v) = it.next(true);
                    debug_assert_eq!(vtok, WjbToken::Value);
                    let (val, val_null) = value_datum(tmp, v, as_text)?;
                    srf.putvalues(&[key, val], &[false, val_null])?;
                    tmp_cxt.reset();
                }
                _ => {}
            }
        }
    }
    Ok(srf.finish(fcinfo))
}

pub fn fc_jsonb_each(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    each_worker(flinfo, fcinfo, "jsonb_each", false)
}

pub fn fc_jsonb_each_text(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    each_worker(flinfo, fcinfo, "jsonb_each_text", true)
}
