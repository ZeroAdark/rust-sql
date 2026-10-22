//! hstore_subs.c execution bodies, installed on hstore_subs_seams; the
//! transform/exec plumbing lives in parse_expr::subscripts and execexpr.

use datum::{Datum, NullableDatum};
use mcx::Mcx;
use types_error::{PgError, PgResult};
use types_fmgr::{FmgrInfo, FunctionCallInfoBaseData as Fcinfo};

use crate::repr::{build_hstore, find_key, HstoreView, Pair};
use crate::{check_key_len, check_val_len};

// DatumGetTextPP payload. The execexpr subscript plumbing delivers raw
// expression results, which include 1B_E external TOAST pointers and 4B_C
// compressed-inline images, so we must detoast before slicing (parity with C's
// DatumGetTextPP). detoasted_image fetches external / decompresses compressed /
// accepts short/4B-U in place and always yields a 4B-header image.
fn text_payload<'m>(mcx: Mcx<'m>, d: Datum) -> PgResult<&'m [u8]> {
    let img = crate::gist::detoasted_image(mcx, d)?;
    Ok(&img[4..])
}

fn hstore_view<'a>(mcx: Mcx<'a>, d: Datum) -> PgResult<HstoreView<'a>> {
    let img = crate::gist::detoasted_image(mcx, d)?;
    Ok(HstoreView::from_vardata(&img[4..]))
}

pub(crate) fn fetch<'m>(mcx: Mcx<'m>, source: Datum, key: Datum) -> PgResult<NullableDatum> {
    let hs = hstore_view(mcx, source)?;
    let key = text_payload(mcx, key)?;
    match find_key(&hs, None, key) {
        Some(idx) if !hs.val_isnull(idx) => {
            let t = varlena::cstring_to_text(mcx, hs.val(idx))?;
            Ok(NullableDatum { value: types_fmgr::varlena_result(t), isnull: false })
        }
        _ => Ok(NullableDatum::null()),
    }
}

pub(crate) fn assign<'m>(
    mcx: Mcx<'m>,
    source: NullableDatum,
    key: Datum,
    replace: NullableDatum,
) -> PgResult<Datum> {
    let key = ::mcx::std_vec_from_slice("hstore", &text_payload(mcx, key)?)?;
    check_key_len(key.len())?;
    let val = if replace.isnull {
        None
    } else {
        let v = ::mcx::std_vec_from_slice("hstore", &text_payload(mcx, replace.value)?)?;
        check_val_len(v.len())?;
        Some(v)
    };
    let p = Pair { key, val, needfree: false };

    let img = if source.isnull {
        ::mcx::check_alloc_size(crate::repr::c_int32_alloc_size(null_assign_alloc_size(
            p.key.len(),
            p.val.as_ref().map_or(0, |v| v.len()),
        )))?;
        build_hstore(&[p])
    } else {
        // hstore_concat with the single new pair (s2 wins ties).
        let hs = hstore_view(mcx, source.value)?;
        ::mcx::check_alloc_size(crate::repr::c_int32_alloc_size(assign_alloc_size(&hs, &p)))?;
        let one = build_hstore(&[p]);
        let s2 = HstoreView::from_vardata(&one[4..]);
        build_hstore(&crate::concat_pairs(&hs, &s2)?)
    };
    let mut out: mcx::PgVec<'m, u8> = mcx::vec_with_capacity_in(mcx, img.len())?;
    mcx::vec_append_bytes(&mut out, &img)?;
    let d = Datum::from_usize(out.as_ptr() as usize);
    core::mem::forget(out);
    Ok(d)
}

// hstore_subs.c:183 hstorePairs(&p, 1, keylen + vallen): CALCDATASIZE(1, buflen).
pub(crate) fn null_assign_alloc_size(keylen: usize, vallen: usize) -> usize {
    8 + crate::repr::HSHRDSIZE + keylen + vallen
}

// hstore_subs.c:204: CALCDATASIZE(s1count + 1, VARSIZE(hs) + keylen + vallen).
pub(crate) fn assign_alloc_size(hs: &HstoreView<'_>, p: &Pair) -> usize {
    (hs.count() + 1) * 8 + 8 + crate::varsize(hs) + p.key.len() + p.val.as_ref().map_or(0, |v| v.len())
}

// The handler function exists only for CREATE EXTENSION's C-symbol
// validation and pg_type.typsubscript resolution; the parse/exec plumbing
// resolves it by proname, never through fmgr.
pub fn fc_hstore_subscript_handler(
    _f: Option<&mut FmgrInfo>,
    _fcinfo: &mut Fcinfo,
) -> PgResult<Datum> {
    Err(Box::new(PgError::error(
        "hstore: hstore_subscript_handler reached through a raw fmgr call — \
         subscripting dispatches by proname in parse_expr/execexpr",
    )))
}
